use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use tokio::process::Command;
use tracing::{error, info, warn};

use super::adapter::{ActivityUpdate, DispatchContext, DispatchOutcome, WatcherAdapter};
use super::supervisor::{resolve_program, resolved_subprocess_path};
use super::{build_agent_prompt_with_context, common_env_pairs, split_shell_words};

const SESSION_PREFIX: &str = "herdr:";
const DEFAULT_HERDR_WORKSPACE: &str = "relaypod";

/// Adapter that launches and drives interactive coding agents inside `herdr` panes.
///
/// Each RelayPod thread maps 1:1 to a live Herdr agent pane named `relaypod-<handle>-<short_thread>`
/// (tab label `relaypod-<channel>-<short_thread>`), allowing humans to inspect live terminal output,
/// see `working`/`blocked`/`idle` status in Herdr's sidebar, and intervene when needed.
pub struct HerdrAdapter;

impl WatcherAdapter for HerdrAdapter {
    fn name(&self) -> &'static str {
        "herdr"
    }

    fn dispatch<'a>(
        &'a self,
        ctx: DispatchContext<'a>,
    ) -> Pin<Box<dyn Future<Output = DispatchOutcome> + Send + 'a>> {
        Box::pin(async move {
            match dispatch_herdr(&ctx).await {
                Ok(outcome) => outcome,
                Err(err) => {
                    let msg = format!("{err:#}");
                    error!(
                        handle = %ctx.handle,
                        thread_id = %ctx.thread_id,
                        unread_id = %ctx.item.id,
                        error = %msg,
                        "Herdr adapter dispatch failed"
                    );
                    DispatchOutcome::failed_with(msg)
                }
            }
        })
    }
}

async fn dispatch_herdr(ctx: &DispatchContext<'_>) -> Result<DispatchOutcome> {
    let agent_type = ctx.target.agent_type.as_ref().ok_or_else(|| {
        anyhow!(
            "handle '{}' with `runner: herdr` requires `agent_type` (e.g. codex, claude, gemini)",
            ctx.handle
        )
    })?;
    let kind = agent_type.as_herdr_kind();

    let existing_pane = ctx
        .sessions
        .get(ctx.identity_id, ctx.thread_id)
        .and_then(|s| s.strip_prefix(SESSION_PREFIX).map(ToString::to_string));

    let (pane_id, is_resume) = if let Some(pane) = existing_pane {
        match get_herdr_agent_status(&pane).await {
            Ok(status) => {
                ctx.sessions.touch(ctx.identity_id, ctx.thread_id);
                info!(
                    handle = %ctx.handle,
                    thread_id = %ctx.thread_id,
                    pane_id = %pane,
                    status = %status,
                    "Reusing live Herdr agent pane"
                );
                if status == "working" {
                    info!(
                        handle = %ctx.handle,
                        pane_id = %pane,
                        "Waiting for busy Herdr agent to settle before sending follow-up"
                    );
                    run_herdr_json(&["agent", "wait", &pane]).await?;
                }
                (pane, true)
            }
            Err(err) => {
                warn!(
                    handle = %ctx.handle,
                    thread_id = %ctx.thread_id,
                    pane_id = %pane,
                    error = %err,
                    "Previous Herdr agent pane is no longer available; starting a fresh pane"
                );
                ctx.sessions.remove(ctx.identity_id, ctx.thread_id);
                evict_lru_panes_if_needed(ctx).await;
                let new_pane = spawn_herdr_agent_pane(ctx, kind).await?;
                ctx.sessions.set(
                    ctx.identity_id,
                    ctx.thread_id,
                    format!("{SESSION_PREFIX}{new_pane}"),
                );
                (new_pane, false)
            }
        }
    } else {
        evict_lru_panes_if_needed(ctx).await;
        let new_pane = spawn_herdr_agent_pane(ctx, kind).await?;
        ctx.sessions.set(
            ctx.identity_id,
            ctx.thread_id,
            format!("{SESSION_PREFIX}{new_pane}"),
        );
        (new_pane, false)
    };

    let prompt = if ctx.target.prompt.is_some() {
        build_agent_prompt_with_context(ctx, is_resume).await
    } else {
        build_herdr_trigger_prompt(ctx.target, ctx.handle, ctx.item, ctx.thread_id, is_resume)
    };

    // TODO: come back to this, maybe there is a better way
    // Give a freshly started TUI (e.g. `agy`, `claude`, `codex`) a brief moment to finish
    // binding its input loop before sending the very first prompt.
    if !is_resume {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    }

    info!(
        handle = %ctx.handle,
        thread_id = %ctx.thread_id,
        unread_id = %ctx.item.id,
        pane_id = %pane_id,
        resuming = is_resume,
        "Submitting prompt to Herdr agent"
    );

    let prompt_resp =
        submit_herdr_prompt_with_retry(ctx.handle, &pane_id, &prompt, !is_resume).await?;

    let mut settled_status = match extract_agent_status(&prompt_resp) {
        Some(status) => status.to_string(),
        None => {
            warn!(
                handle = %ctx.handle,
                pane_id = %pane_id,
                response = %prompt_resp,
                "Could not extract agent_status from `herdr agent prompt` response; falling back to `herdr agent get`"
            );
            get_herdr_agent_status(&pane_id)
                .await
                .unwrap_or_else(|_| "unknown".to_string())
        }
    };
    info!(
        handle = %ctx.handle,
        thread_id = %ctx.thread_id,
        pane_id = %pane_id,
        status = %settled_status,
        "Herdr agent prompt settled"
    );

    while settled_status == "blocked" {
        warn!(
            handle = %ctx.handle,
            thread_id = %ctx.thread_id,
            pane_id = %pane_id,
            "Herdr agent is blocked waiting for human input/approval in Herdr"
        );
        let peek = read_herdr_blocked_peek(&pane_id).await;
        (ctx.on_activity)(ActivityUpdate::blocked(Some(pane_id.clone()), peek));

        let next_status = match wait_for_herdr_unblock(&pane_id).await {
            Ok(status) => status,
            Err(err) => {
                warn!(
                    handle = %ctx.handle,
                    thread_id = %ctx.thread_id,
                    pane_id = %pane_id,
                    error = %err,
                    "Stopped waiting for blocked Herdr pane"
                );
                return Ok(DispatchOutcome::blocked());
            }
        };

        if next_status == "working" {
            (ctx.on_activity)(ActivityUpdate::working());
            settled_status = match wait_for_herdr_settle(&pane_id).await {
                Ok(status) => status,
                Err(err) => {
                    warn!(
                        handle = %ctx.handle,
                        thread_id = %ctx.thread_id,
                        pane_id = %pane_id,
                        error = %err,
                        "Failed waiting for unblocked Herdr pane to settle"
                    );
                    return Ok(DispatchOutcome::blocked());
                }
            };
        } else if next_status == "idle" || next_status == "done" {
            settled_status = next_status;
        } else {
            return Ok(DispatchOutcome::blocked());
        }
    }

    Ok(DispatchOutcome::ok())
}

async fn submit_herdr_prompt_with_retry(
    handle: &str,
    pane_id: &str,
    prompt: &str,
    is_fresh_pane: bool,
) -> Result<Value> {
    let max_attempts = 3;
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 1..=max_attempts {
        match run_herdr_json(&["agent", "prompt", pane_id, prompt, "--wait"]).await {
            Ok(val) => return Ok(val),
            Err(err) => {
                let msg = format!("{err:#}");
                let stalled = msg.contains("agent_prompt_stalled");
                if stalled && attempt < max_attempts {
                    warn!(
                        handle = %handle,
                        pane_id = %pane_id,
                        attempt = attempt,
                        "Herdr agent prompt stalled (TUI may still be initializing or showing a banner); retrying"
                    );
                    if is_fresh_pane && attempt == 1 {
                        // Dismiss any startup announcement banner (e.g. `esc to dismiss`) before retrying.
                        let _ = run_herdr_json(&["agent", "send-keys", pane_id, "esc"]).await;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    last_err = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("failed to submit prompt to Herdr pane {pane_id}")))
}

/// Enforces per-handle `max_sessions` and top-level `global_max_sessions` before spawning a new
/// Herdr pane by closing the least-recently-used `idle`/`done` pane(s).
/// Active (`working`) or `blocked` panes are never evicted.
async fn evict_lru_panes_if_needed(ctx: &DispatchContext<'_>) {
    if let Some(handle_cap) = ctx.target.max_sessions.filter(|&n| n > 0) {
        let handle_candidates: Vec<(String, String, String)> = ctx
            .sessions
            .list_for_handle_lru(ctx.identity_id)
            .into_iter()
            .filter_map(|(tid, sid)| {
                if tid == ctx.thread_id {
                    return None;
                }
                sid.strip_prefix(SESSION_PREFIX)
                    .map(|pane| (ctx.identity_id.to_string(), tid, pane.to_string()))
            })
            .collect();
        evict_candidates_to_cap(ctx, handle_candidates, handle_cap, "max_sessions").await;
    }

    if let Some(global_cap) = ctx.target.global_max_sessions.filter(|&n| n > 0) {
        let global_candidates: Vec<(String, String, String)> = ctx
            .sessions
            .list_all_lru()
            .into_iter()
            .filter_map(|(owner, tid, sid)| {
                if owner == ctx.identity_id && tid == ctx.thread_id {
                    return None;
                }
                sid.strip_prefix(SESSION_PREFIX)
                    .map(|pane| (owner, tid, pane.to_string()))
            })
            .collect();
        evict_candidates_to_cap(ctx, global_candidates, global_cap, "global_max_sessions").await;
    }
}

async fn evict_candidates_to_cap(
    ctx: &DispatchContext<'_>,
    candidates: Vec<(String, String, String)>,
    cap: usize,
    cap_label: &str,
) {
    // Check which panes are actually still alive in Herdr; prune stale entries immediately.
    let mut live: Vec<(String, String, String, String)> = Vec::new();
    for (owner_handle, tid, pane_id) in candidates {
        match get_herdr_agent_status(&pane_id).await {
            Ok(status) => live.push((owner_handle, tid, pane_id, status)),
            Err(_) => {
                ctx.sessions.remove(&owner_handle, &tid);
            }
        }
    }

    // Reserve 1 slot for the new pane we are about to spawn.
    let mut live_count = live.len();
    for (owner_handle, tid, pane_id, status) in live {
        if live_count < cap {
            break;
        }
        if status != "idle" && status != "done" {
            continue;
        }

        info!(
            handle = %ctx.handle,
            evicted_handle = %owner_handle,
            evicted_thread_id = %tid,
            pane_id = %pane_id,
            status = %status,
            cap_label = cap_label,
            cap = cap,
            "Evicting LRU Herdr pane to stay within session cap"
        );
        match run_herdr_json(&["pane", "close", &pane_id]).await {
            Ok(_) => {
                ctx.sessions.remove(&owner_handle, &tid);
                live_count = live_count.saturating_sub(1);
            }
            Err(err) => {
                warn!(
                    handle = %ctx.handle,
                    pane_id = %pane_id,
                    error = %err,
                    "Failed to close LRU Herdr pane"
                );
            }
        }
    }

    if live_count >= cap {
        warn!(
            handle = %ctx.handle,
            live_count = live_count,
            cap_label = cap_label,
            cap = cap,
            "All existing Herdr panes are working/blocked; spawning new pane without evicting active work"
        );
    }
}

async fn spawn_herdr_agent_pane(ctx: &DispatchContext<'_>, kind: &str) -> Result<String> {
    let label = herdr_tab_label(&ctx.item.channel_name, ctx.thread_id);
    let agent_name = herdr_agent_name(ctx.handle, ctx.thread_id);

    let env_pairs = common_env_pairs(ctx.target, ctx.handle, ctx.item, ctx.thread_id, None);
    let env_args: Vec<String> = env_pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();

    let pane_id = create_herdr_pane(ctx.target.cwd.as_deref(), &label, &env_args).await?;

    let extra_args = ctx
        .target
        .args
        .as_deref()
        .map(split_shell_words)
        .unwrap_or_default();

    let mut start_args: Vec<String> = vec![
        "agent".into(),
        "start".into(),
        agent_name.clone(),
        "--kind".into(),
        kind.into(),
        "--pane".into(),
        pane_id.clone(),
    ];
    if !extra_args.is_empty() {
        start_args.push("--".into());
        start_args.extend(extra_args);
    }

    info!(
        handle = %ctx.handle,
        agent_name = %agent_name,
        kind = %kind,
        pane_id = %pane_id,
        label = %label,
        "Starting interactive agent in Herdr pane"
    );

    // Give the newly created shell pane a moment to reach its interactive prompt,
    // and retry briefly if the shell is still initializing (~/.zshrc, prompt plugins, etc.).
    let start_refs: Vec<&str> = start_args.iter().map(String::as_str).collect();
    let max_attempts = 10;
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=max_attempts {
        if attempt == 1 {
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        match run_herdr_json(&start_refs).await {
            Ok(_) => {
                last_err = None;
                break;
            }
            Err(err) => {
                let msg = format!("{err:#}");
                let retryable = msg.contains("prompt")
                    || msg.contains("foreground")
                    || msg.contains("busy")
                    || msg.contains("initializing")
                    || msg.contains("not ready");
                if retryable && attempt < max_attempts {
                    warn!(
                        handle = %ctx.handle,
                        pane_id = %pane_id,
                        attempt = attempt,
                        error = %msg,
                        "Herdr pane shell not ready yet; retrying `herdr agent start`"
                    );
                    last_err = Some(err);
                    continue;
                }
                last_err = Some(err);
                break;
            }
        }
    }

    if let Some(err) = last_err {
        // Avoid leaking an empty shell tab if agent startup failed.
        let _ = run_herdr_json(&["pane", "close", &pane_id]).await;
        return Err(err).with_context(|| {
            format!("failed to start Herdr agent `{agent_name}` ({kind}) in pane {pane_id}")
        });
    }

    Ok(pane_id)
}

async fn create_herdr_pane(
    cwd: Option<&str>,
    tab_label: &str,
    env_args: &[String],
) -> Result<String> {
    let target_ws = DEFAULT_HERDR_WORKSPACE;

    // Look up the dedicated "relaypod" workspace by ID or label.
    let existing_ws_id = match run_herdr_json(&["workspace", "list"]).await {
        Ok(val) => find_workspace_id(&val, target_ws),
        Err(err) => {
            warn!(error = %err, "Failed to list Herdr workspaces; will attempt to create workspace");
            None
        }
    };

    if let Some(ws_id) = existing_ws_id {
        let mut tab_args: Vec<String> =
            vec!["tab".into(), "create".into(), "--workspace".into(), ws_id];
        if let Some(dir) = cwd {
            tab_args.push("--cwd".into());
            tab_args.push(dir.into());
        }
        tab_args.push("--label".into());
        tab_args.push(tab_label.into());
        for pair in env_args {
            tab_args.push("--env".into());
            tab_args.push(pair.clone());
        }
        tab_args.push("--no-focus".into());

        let tab_refs: Vec<&str> = tab_args.iter().map(String::as_str).collect();
        let val = run_herdr_json(&tab_refs).await?;
        return extract_pane_id(&val)
            .ok_or_else(|| anyhow!("missing pane_id in `herdr tab create` response: {val}"));
    }

    // Dedicated "relaypod" workspace does not exist yet — create it and reuse its initial tab/pane.
    info!(workspace = %target_ws, "Creating dedicated Herdr workspace");
    let mut ws_args: Vec<String> = vec!["workspace".into(), "create".into()];
    if let Some(dir) = cwd {
        ws_args.push("--cwd".into());
        ws_args.push(dir.into());
    }
    ws_args.push("--label".into());
    ws_args.push(target_ws.into());
    for pair in env_args {
        ws_args.push("--env".into());
        ws_args.push(pair.clone());
    }
    ws_args.push("--no-focus".into());

    let ws_refs: Vec<&str> = ws_args.iter().map(String::as_str).collect();
    let val = run_herdr_json(&ws_refs).await?;

    if let Some(tab_id) = extract_tab_id(&val) {
        let _ = run_herdr_json(&["tab", "rename", &tab_id, tab_label]).await;
    }

    extract_pane_id(&val)
        .ok_or_else(|| anyhow!("missing pane_id in `herdr workspace create` response: {val}"))
}

async fn get_herdr_agent_status(pane_id: &str) -> Result<String> {
    let val = run_herdr_json(&["agent", "get", pane_id]).await?;
    Ok(extract_agent_status(&val).unwrap_or("unknown").to_string())
}

fn redact_herdr_args(args: &[&str]) -> String {
    let mut out = Vec::with_capacity(args.len());
    let mut redact_next = false;
    for &arg in args {
        if redact_next {
            let masked = match arg.split_once('=') {
                Some((k, _)) => format!("{k}=<redacted>"),
                None => "<redacted>".to_string(),
            };
            out.push(masked);
            redact_next = false;
        } else if arg == "--env" {
            out.push(arg.to_string());
            redact_next = true;
        } else if let Some(rest) = arg.strip_prefix("--env=") {
            let key = rest.split_once('=').map(|(k, _)| k).unwrap_or("?");
            out.push(format!("--env={key}=<redacted>"));
        } else {
            out.push(arg.to_string());
        }
    }
    out.join(" ")
}

/// Waits until a blocked Herdr pane leaves `blocked` (into `working`, `idle`, `done`, or `unknown`).
async fn wait_for_herdr_unblock(pane_id: &str) -> Result<String> {
    let val = run_herdr_json(&[
        "agent", "wait", pane_id, "--until", "working", "--until", "idle", "--until", "done",
        "--until", "unknown",
    ])
    .await?;
    match extract_agent_status(&val) {
        Some(status) => Ok(status.to_string()),
        None => get_herdr_agent_status(pane_id).await,
    }
}

/// Waits until a working Herdr pane settles into `idle`, `done`, or `blocked`.
async fn wait_for_herdr_settle(pane_id: &str) -> Result<String> {
    let val = run_herdr_json(&["agent", "wait", pane_id]).await?;
    match extract_agent_status(&val) {
        Some(status) => Ok(status.to_string()),
        None => get_herdr_agent_status(pane_id).await,
    }
}

const MAX_PEEK_LINES: usize = 16;
const MAX_PEEK_CHARS: usize = 1500;

/// Focuses the given Herdr agent pane so the user can inspect or approve it directly in Herdr.
pub async fn focus_herdr_pane(pane_id: &str) -> Result<()> {
    let trimmed = pane_id.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("pane_id is empty"));
    }
    run_herdr_text(&["agent", "focus", trimmed]).await?;
    Ok(())
}

/// Reads a compact, read-only plain-text peek of the prompt/dialog currently blocking a Herdr pane.
async fn read_herdr_blocked_peek(pane_id: &str) -> Option<String> {
    let raw = match run_herdr_text(&[
        "agent",
        "read",
        pane_id,
        "--source",
        "detection",
        "--lines",
        "20",
    ])
    .await
    {
        Ok(text) if !text.trim().is_empty() => text,
        _ => run_herdr_text(&[
            "agent", "read", pane_id, "--source", "visible", "--lines", "20",
        ])
        .await
        .ok()?,
    };
    format_blocked_peek(&raw)
}

pub(crate) fn format_blocked_peek(raw: &str) -> Option<String> {
    let mut lines: Vec<&str> = Vec::new();
    let mut last_blank = false;
    for line in raw.lines() {
        let trimmed_end = line.trim_end();
        if trimmed_end.is_empty() {
            if !lines.is_empty() && !last_blank {
                lines.push("");
                last_blank = true;
            }
        } else {
            lines.push(trimmed_end);
            last_blank = false;
        }
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return None;
    }

    let start = lines.len().saturating_sub(MAX_PEEK_LINES);
    let mut joined = lines[start..].join("\n");
    if joined.len() > MAX_PEEK_CHARS {
        let mut cut = joined.len() - MAX_PEEK_CHARS;
        while !joined.is_char_boundary(cut) {
            cut += 1;
        }
        if let Some(nl) = joined[cut..].find('\n') {
            cut += nl + 1;
        }
        joined = joined[cut..].trim_start_matches('\n').to_string();
    }
    if joined.trim().is_empty() {
        None
    } else {
        Some(joined)
    }
}

async fn run_herdr_text(args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(resolve_program("herdr"));
    if let Some(path) = resolved_subprocess_path() {
        cmd.env("PATH", path);
    }
    cmd.args(args);
    cmd.kill_on_drop(true);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let redacted = redact_herdr_args(args);
    let output = cmd
        .output()
        .await
        .with_context(|| format!("failed to execute `herdr {redacted}`"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        let trimmed_out = stdout.trim();
        return Err(anyhow!(
            "`herdr {redacted}` exited with {:?}: {}",
            output.status.code(),
            if !stderr.is_empty() {
                stderr
            } else {
                trimmed_out.to_string()
            }
        ));
    }

    Ok(stdout)
}

async fn run_herdr_json(args: &[&str]) -> Result<Value> {
    let stdout = run_herdr_text(args).await?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null);
    }

    let redacted = redact_herdr_args(args);
    serde_json::from_str(trimmed)
        .with_context(|| format!("failed to parse JSON from `herdr {redacted}`: {trimmed}"))
}

/// Builds a Herdr-compliant agent name matching `[a-z][a-z0-9_-]{0,31}` with a `relaypod-` prefix.
pub(crate) fn herdr_agent_name(handle: &str, thread_id: &str) -> String {
    let clean_handle = sanitize_slug(handle);
    let clean_thread = sanitize_slug(thread_id);
    let short_thread: String = clean_thread.chars().take(8).collect();

    // Prefix is "relaypod-" (9 chars). Reserve "-" + short_thread at the end.
    let suffix = if short_thread.is_empty() {
        String::new()
    } else {
        format!("-{short_thread}")
    };
    let max_handle_len = 32usize.saturating_sub("relaypod-".len() + suffix.len());
    let truncated_handle: String = clean_handle
        .chars()
        .take(max_handle_len)
        .collect::<String>()
        .trim_end_matches(['-', '_'])
        .to_string();

    if truncated_handle.is_empty() {
        format!(
            "relaypod{}",
            if suffix.is_empty() { "-agent" } else { &suffix }
        )
    } else {
        format!("relaypod-{truncated_handle}{suffix}")
    }
}

/// Builds a human-readable Herdr tab/workspace label prefixed with `relaypod-`.
pub(crate) fn herdr_tab_label(channel: &str, thread_id: &str) -> String {
    let clean_channel = sanitize_slug(channel);
    let short_thread: String = sanitize_slug(thread_id).chars().take(8).collect();
    if short_thread.is_empty() {
        format!("relaypod-{clean_channel}")
    } else {
        format!("relaypod-{clean_channel}-{short_thread}")
    }
}

fn sanitize_slug(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() || lower == '_' {
            out.push(lower);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches(['-', '_']).to_string()
}

pub(crate) fn extract_pane_id(val: &Value) -> Option<String> {
    val.pointer("/result/root_pane/pane_id")
        .or_else(|| val.pointer("/result/pane/pane_id"))
        .or_else(|| val.pointer("/result/pane_id"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

pub(crate) fn extract_tab_id(val: &Value) -> Option<String> {
    val.pointer("/result/tab/tab_id")
        .or_else(|| val.pointer("/result/tab_id"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

pub(crate) fn find_workspace_id(val: &Value, selector: &str) -> Option<String> {
    let workspaces = val.pointer("/result/workspaces")?.as_array()?;
    for ws in workspaces {
        let id = ws.get("workspace_id").and_then(|v| v.as_str());
        let label = ws.get("label").and_then(|v| v.as_str());
        if id == Some(selector) || label.is_some_and(|l| l.eq_ignore_ascii_case(selector)) {
            return id.map(ToString::to_string);
        }
    }
    None
}

pub(crate) fn extract_agent_status(val: &Value) -> Option<&str> {
    val.pointer("/result/agent/agent_status")
        .or_else(|| val.pointer("/result/event/data/agent_status"))
        .or_else(|| val.pointer("/result/event/data/final_status"))
        .or_else(|| val.pointer("/result/pane/agent_status"))
        .or_else(|| val.pointer("/result/agent_status"))
        .or_else(|| val.pointer("/result/agent/status"))
        .or_else(|| val.pointer("/result/status"))
        .or_else(|| val.pointer("/status"))
        .and_then(|v| v.as_str())
}

/// Builds a short, single-line, `@`-free trigger prompt for interactive Herdr TUIs.
///
/// Interactive agent TUIs (Codex, Claude Code, Gemini CLI, etc.) treat `@` as a file-mention
/// autocomplete trigger and can misparse leading `/` commands or large multiline pastes.
/// By sending only metadata and instructing the agent to fetch the thread via RelayPod MCP
/// `read_thread`, the agent receives the exact unmodified message without TUI input corruption.
pub(crate) fn build_herdr_trigger_prompt(
    target: &super::ResolvedTarget,
    handle: &str,
    item: &relaypod_core::models::UnreadItem,
    thread_id: &str,
    is_resume: bool,
) -> String {
    let clean = |s: &str| {
        s.replace(['\r', '\n'], " ")
            .replace('@', "")
            .trim()
            .to_string()
    };
    let handle = clean(handle);
    let channel = clean(&item.channel_name);
    let author = clean(&item.author_handle);

    if is_resume {
        return format!(
            "Follow-up RelayPod message for {handle} in channel {channel} (thread {thread_id}, unread {unread_id}) from {author}. Call RelayPod MCP read_thread with message_id=\"{thread_id}\" to read the latest reply, respond in-thread using post_message (channel=\"{channel}\", parent_id=\"{thread_id}\"), and acknowledge unread {unread_id} via ack_unread. Use RelayPod MCP tools rather than the relaypod CLI. Do not use interactive user-input tools; ask any clarifying questions in-thread via post_message.",
            unread_id = item.id,
        );
    }

    let instructions_prefix = target
        .instructions_file
        .as_ref()
        .map(|p| clean(&p.display().to_string()))
        .filter(|p| !p.is_empty())
        .map(|p| format!("First read your instructions file at {p}. "))
        .unwrap_or_default();

    format!(
        "You are {handle}. {instructions_prefix}New RelayPod message in channel {channel} (thread {thread_id}, unread {unread_id}) from {author}. Call RelayPod MCP read_thread with message_id=\"{thread_id}\" to read the full thread and message, reply in-thread using post_message (channel=\"{channel}\", parent_id=\"{thread_id}\"), and acknowledge unread {unread_id} via ack_unread. Use RelayPod MCP tools rather than the relaypod CLI. Do not use interactive user-input tools; ask any clarifying questions in-thread via post_message.",
        unread_id = item.id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_valid_relaypod_prefixed_herdr_names() {
        let name = herdr_agent_name("codex-local", "0199a213-81c0-7800-8aa1");
        assert!(name.starts_with("relaypod-"));
        assert!(name.len() <= 32, "name too long: {name}");
        assert!(name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'));
        assert_eq!(name, "relaypod-codex-local-0199a213");

        let long_name = herdr_agent_name(
            "very-long-agent-handle-name-that-exceeds-limit",
            "abcdef1234567890",
        );
        assert!(long_name.starts_with("relaypod-"));
        assert!(long_name.len() <= 32, "name too long: {long_name}");
        assert!(long_name.ends_with("-abcdef12"));

        assert_eq!(
            herdr_tab_label("Backend Ops", "0199a213-81c0"),
            "relaypod-backend-ops-0199a213"
        );
    }

    #[test]
    fn parses_herdr_json_responses() {
        let tab_resp = json!({
            "ok": true,
            "result": {
                "tab": { "tab_id": "w1:t2" },
                "root_pane": { "pane_id": "w1:p5" }
            }
        });
        assert_eq!(extract_pane_id(&tab_resp).as_deref(), Some("w1:p5"));

        let agent_resp = json!({
            "id": "cli:agent:wait",
            "result": {
                "type": "agent_info",
                "agent": { "name": "relaypod-codex-1234", "agent_status": "idle" }
            }
        });
        assert_eq!(extract_agent_status(&agent_resp), Some("idle"));

        let prompt_resp = json!({
            "id": "cli:agent:prompt",
            "result": {
                "type": "agent_prompted",
                "agent": { "name": "relaypod-codex-1234", "agent_status": "blocked" }
            }
        });
        assert_eq!(extract_agent_status(&prompt_resp), Some("blocked"));

        let wait_event_resp = json!({
            "id": "cli:agent:wait",
            "result": {
                "type": "wait_matched",
                "event": {
                    "event": "pane_agent_status_changed",
                    "data": {
                        "type": "pane_agent_status_changed",
                        "pane_id": "w4:p7",
                        "agent_status": "working"
                    }
                }
            }
        });
        assert_eq!(extract_agent_status(&wait_event_resp), Some("working"));

        let legacy_prompt_resp = json!({
            "ok": true,
            "result": {
                "status": "blocked"
            }
        });
        assert_eq!(extract_agent_status(&legacy_prompt_resp), Some("blocked"));

        let ws_list = json!({
            "id": "cli:workspace:list",
            "result": {
                "type": "workspace_list",
                "workspaces": [
                    { "workspace_id": "w1", "label": "main" },
                    { "workspace_id": "w2", "label": "relaypod" }
                ]
            }
        });
        assert_eq!(
            find_workspace_id(&ws_list, "relaypod").as_deref(),
            Some("w2")
        );
        assert_eq!(find_workspace_id(&ws_list, "w1").as_deref(), Some("w1"));
        assert_eq!(find_workspace_id(&ws_list, "missing"), None);
    }

    #[test]
    fn redacts_env_secrets_in_herdr_args() {
        let args = [
            "tab",
            "create",
            "--env",
            "RELAYPOD_TOKEN=rp_super_secret",
            "--env=API_KEY=sk-1234",
            "--label",
            "demo",
        ];
        let redacted = redact_herdr_args(&args);
        assert!(!redacted.contains("rp_super_secret"));
        assert!(!redacted.contains("sk-1234"));
        assert!(redacted.contains("RELAYPOD_TOKEN=<redacted>"));
        assert!(redacted.contains("--env=API_KEY=<redacted>"));
    }

    #[test]
    fn formats_blocked_peek_cleanly() {
        let raw = "\n\n  Allow running command?   \n\n\n  $ cargo test  \n  1. Yes\n  2. No\n\n";
        let peek = format_blocked_peek(raw).unwrap();
        assert_eq!(
            peek,
            "  Allow running command?\n\n  $ cargo test\n  1. Yes\n  2. No"
        );

        assert_eq!(format_blocked_peek("   \n\n  "), None);

        let many: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let trimmed = format_blocked_peek(&many).unwrap();
        let kept: Vec<&str> = trimmed.lines().collect();
        assert_eq!(kept.len(), MAX_PEEK_LINES);
        assert_eq!(kept.first().copied(), Some("line 15"));
        assert_eq!(kept.last().copied(), Some("line 30"));
    }

    #[test]
    fn herdr_trigger_prompt_is_single_line_and_free_of_at_signs() {
        let target = super::super::ResolvedTarget {
            label: "codex-local".into(),
            runner: super::super::RunnerKind::Herdr,
            agent_type: Some(super::super::AgentType::Codex),
            url: "http://127.0.0.1:8787".into(),
            token: "rp_test".into(),
            exec: String::new(),
            args: None,
            prompt: None,
            instructions_file: Some(std::path::PathBuf::from("/home/user/team-norms.md")),
            cwd: None,
            global_max_sessions: None,
            max_sessions: None,
            max_consecutive_agent_turns: None,
            auto_ack: false,
            drain_on_start: true,
            cooldown_secs: 0,
            env: Default::default(),
        };
        let item = relaypod_core::models::UnreadItem {
            id: "u1".into(),
            identity_id: "id1".into(),
            message_id: "m2".into(),
            channel_id: "ch1".into(),
            channel_name: "backend".into(),
            parent_id: Some("m1".into()),
            author_handle: "alice".into(),
            body: "@codex-local check /clear\nline 2 with @file.rs".into(),
            reason: relaypod_core::models::UnreadReason::Mention,
            created_at: 0,
            acked_at: None,
        };

        let fresh = build_herdr_trigger_prompt(&target, "codex-local", &item, "m1", false);
        assert!(!fresh.contains('@'), "fresh prompt contained '@': {fresh}");
        assert!(!fresh.contains('\n'), "fresh prompt had newline: {fresh}");
        assert!(fresh.contains("First read your instructions file at /home/user/team-norms.md."));
        assert!(fresh.contains("read_thread with message_id=\"m1\""));
        assert!(fresh.contains("post_message (channel=\"backend\", parent_id=\"m1\")"));
        assert!(fresh.contains("acknowledge unread u1 via ack_unread"));
        assert!(!fresh.contains("/clear"));

        let resume = build_herdr_trigger_prompt(&target, "codex-local", &item, "m1", true);
        assert!(
            !resume.contains('@'),
            "resume prompt contained '@': {resume}"
        );
        assert!(
            !resume.contains('\n'),
            "resume prompt had newline: {resume}"
        );
        assert!(!resume.contains("instructions file"));
        assert!(resume.contains("read_thread with message_id=\"m1\""));
        assert!(resume.contains("acknowledge unread u1 via ack_unread"));
    }
}
