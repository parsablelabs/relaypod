use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use tokio::process::Command;
use tracing::{error, info, warn};

use super::adapter::{DispatchContext, DispatchOutcome, WatcherAdapter};
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
                    error!(
                        handle = %ctx.handle,
                        thread_id = %ctx.thread_id,
                        unread_id = %ctx.item.id,
                        error = %format!("{err:#}"),
                        "Herdr adapter dispatch failed"
                    );
                    DispatchOutcome::failed()
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
    let herdr_session = ctx.target.herdr_session.as_deref();

    let existing_pane = ctx
        .sessions
        .get(ctx.handle, ctx.thread_id)
        .and_then(|s| s.strip_prefix(SESSION_PREFIX).map(ToString::to_string));

    let (pane_id, is_resume) = if let Some(pane) = existing_pane {
        match get_herdr_agent_status(herdr_session, &pane).await {
            Ok(status) => {
                ctx.sessions.touch(ctx.handle, ctx.thread_id);
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
                    run_herdr_json(herdr_session, &["agent", "wait", &pane]).await?;
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
                ctx.sessions.remove(ctx.handle, ctx.thread_id);
                evict_lru_panes_if_needed(ctx, herdr_session).await;
                let new_pane = spawn_herdr_agent_pane(ctx, herdr_session, kind).await?;
                ctx.sessions.set(
                    ctx.handle,
                    ctx.thread_id,
                    format!("{SESSION_PREFIX}{new_pane}"),
                );
                (new_pane, false)
            }
        }
    } else {
        evict_lru_panes_if_needed(ctx, herdr_session).await;
        let new_pane = spawn_herdr_agent_pane(ctx, herdr_session, kind).await?;
        ctx.sessions.set(
            ctx.handle,
            ctx.thread_id,
            format!("{SESSION_PREFIX}{new_pane}"),
        );
        (new_pane, false)
    };

    let prompt = build_agent_prompt_with_context(ctx, is_resume).await;

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

    let prompt_resp = submit_herdr_prompt_with_retry(
        herdr_session,
        ctx.handle,
        &pane_id,
        &prompt,
        !is_resume,
    )
    .await?;

    let settled_status = extract_agent_status(&prompt_resp).unwrap_or("unknown");
    info!(
        handle = %ctx.handle,
        thread_id = %ctx.thread_id,
        pane_id = %pane_id,
        status = %settled_status,
        "Herdr agent prompt settled"
    );

    if settled_status == "blocked" {
        warn!(
            handle = %ctx.handle,
            thread_id = %ctx.thread_id,
            pane_id = %pane_id,
            "Herdr agent is blocked waiting for human input/approval in Herdr"
        );
        Ok(DispatchOutcome::blocked())
    } else {
        Ok(DispatchOutcome::ok())
    }
}

async fn submit_herdr_prompt_with_retry(
    herdr_session: Option<&str>,
    handle: &str,
    pane_id: &str,
    prompt: &str,
    is_fresh_pane: bool,
) -> Result<Value> {
    let max_attempts = 3;
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 1..=max_attempts {
        match run_herdr_json(
            herdr_session,
            &["agent", "prompt", pane_id, prompt, "--wait"],
        )
        .await
        {
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
                        let _ =
                            run_herdr_json(herdr_session, &["agent", "send-keys", pane_id, "esc"])
                                .await;
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
async fn evict_lru_panes_if_needed(ctx: &DispatchContext<'_>, herdr_session: Option<&str>) {
    if let Some(handle_cap) = ctx.target.max_sessions.filter(|&n| n > 0) {
        let handle_candidates: Vec<(String, String, String)> = ctx
            .sessions
            .list_for_handle_lru(ctx.handle)
            .into_iter()
            .filter_map(|(tid, sid)| {
                if tid == ctx.thread_id {
                    return None;
                }
                sid.strip_prefix(SESSION_PREFIX)
                    .map(|pane| (ctx.handle.to_string(), tid, pane.to_string()))
            })
            .collect();
        evict_candidates_to_cap(
            ctx,
            herdr_session,
            handle_candidates,
            handle_cap,
            "max_sessions",
        )
        .await;
    }

    if let Some(global_cap) = ctx.target.global_max_sessions.filter(|&n| n > 0) {
        let global_candidates: Vec<(String, String, String)> = ctx
            .sessions
            .list_all_lru()
            .into_iter()
            .filter_map(|(h, tid, sid)| {
                if h == ctx.handle && tid == ctx.thread_id {
                    return None;
                }
                sid.strip_prefix(SESSION_PREFIX)
                    .map(|pane| (h, tid, pane.to_string()))
            })
            .collect();
        evict_candidates_to_cap(
            ctx,
            herdr_session,
            global_candidates,
            global_cap,
            "global_max_sessions",
        )
        .await;
    }
}

async fn evict_candidates_to_cap(
    ctx: &DispatchContext<'_>,
    herdr_session: Option<&str>,
    candidates: Vec<(String, String, String)>,
    cap: usize,
    cap_label: &str,
) {
    // Check which panes are actually still alive in Herdr; prune stale entries immediately.
    let mut live: Vec<(String, String, String, String)> = Vec::new();
    for (owner_handle, tid, pane_id) in candidates {
        match get_herdr_agent_status(herdr_session, &pane_id).await {
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
        match run_herdr_json(herdr_session, &["pane", "close", &pane_id]).await {
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

async fn spawn_herdr_agent_pane(
    ctx: &DispatchContext<'_>,
    herdr_session: Option<&str>,
    kind: &str,
) -> Result<String> {
    let label = herdr_tab_label(&ctx.item.channel_name, ctx.thread_id);
    let agent_name = herdr_agent_name(ctx.handle, ctx.thread_id);

    let env_pairs = common_env_pairs(ctx.target, ctx.handle, ctx.item, ctx.thread_id, None);
    let env_args: Vec<String> = env_pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();

    let pane_id = create_herdr_pane(
        herdr_session,
        ctx.target.herdr_workspace.as_deref(),
        ctx.target.cwd.as_deref(),
        &label,
        &env_args,
    )
    .await?;

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

        match run_herdr_json(herdr_session, &start_refs).await {
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
        let _ = run_herdr_json(herdr_session, &["pane", "close", &pane_id]).await;
        return Err(err).with_context(|| {
            format!("failed to start Herdr agent `{agent_name}` ({kind}) in pane {pane_id}")
        });
    }

    Ok(pane_id)
}

async fn create_herdr_pane(
    herdr_session: Option<&str>,
    workspace_selector: Option<&str>,
    cwd: Option<&str>,
    tab_label: &str,
    env_args: &[String],
) -> Result<String> {
    let target_ws = workspace_selector
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_HERDR_WORKSPACE);

    // Look up an existing workspace matching by ID or label (default: "relaypod").
    let existing_ws_id = match run_herdr_json(herdr_session, &["workspace", "list"]).await {
        Ok(val) => find_workspace_id(&val, target_ws),
        Err(err) => {
            warn!(error = %err, "Failed to list Herdr workspaces; will attempt to create workspace");
            None
        }
    };

    if let Some(ws_id) = existing_ws_id {
        let mut tab_args: Vec<String> = vec![
            "tab".into(),
            "create".into(),
            "--workspace".into(),
            ws_id,
        ];
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
        let val = run_herdr_json(herdr_session, &tab_refs).await?;
        return extract_pane_id(&val)
            .ok_or_else(|| anyhow!("missing pane_id in `herdr tab create` response: {val}"));
    }

    // Dedicated workspace does not exist yet — create it labeled `target_ws` (e.g. "relaypod")
    // and reuse its initial tab/pane.
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
    let val = run_herdr_json(herdr_session, &ws_refs).await?;

    if let Some(tab_id) = extract_tab_id(&val) {
        let _ = run_herdr_json(herdr_session, &["tab", "rename", &tab_id, tab_label]).await;
    }

    extract_pane_id(&val)
        .ok_or_else(|| anyhow!("missing pane_id in `herdr workspace create` response: {val}"))
}

async fn get_herdr_agent_status(herdr_session: Option<&str>, pane_id: &str) -> Result<String> {
    let val = run_herdr_json(herdr_session, &["agent", "get", pane_id]).await?;
    Ok(extract_agent_status(&val).unwrap_or("unknown").to_string())
}

async fn run_herdr_json(herdr_session: Option<&str>, args: &[&str]) -> Result<Value> {
    let mut cmd = Command::new("herdr");
    if let Some(session) = herdr_session {
        cmd.arg("--session").arg(session);
    }
    cmd.args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd
        .output()
        .await
        .with_context(|| format!("failed to execute `herdr {}`", args.join(" ")))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        return Err(anyhow!(
            "`herdr {}` exited with {:?}: {}",
            args.join(" "),
            output.status.code(),
            if !stderr.is_empty() { stderr } else { stdout }
        ));
    }

    if stdout.is_empty() {
        return Ok(Value::Null);
    }

    serde_json::from_str(&stdout).with_context(|| {
        format!(
            "failed to parse JSON from `herdr {}`: {stdout}",
            args.join(" ")
        )
    })
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
        format!("relaypod{}", if suffix.is_empty() { "-agent" } else { &suffix })
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
    val.pointer("/result/agent/status")
        .or_else(|| val.pointer("/result/status"))
        .or_else(|| val.pointer("/status"))
        .and_then(|v| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_valid_relaypod_prefixed_herdr_names() {
        let name = herdr_agent_name("marcin-codex", "0199a213-81c0-7800-8aa1");
        assert!(name.starts_with("relaypod-"));
        assert!(name.len() <= 32, "name too long: {name}");
        assert!(name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'));
        assert_eq!(name, "relaypod-marcin-codex-0199a213");

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
            "ok": true,
            "result": {
                "agent": { "name": "relaypod-codex-1234", "status": "idle" }
            }
        });
        assert_eq!(extract_agent_status(&agent_resp), Some("idle"));

        let prompt_resp = json!({
            "ok": true,
            "result": {
                "status": "blocked"
            }
        });
        assert_eq!(extract_agent_status(&prompt_resp), Some("blocked"));

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
        assert_eq!(find_workspace_id(&ws_list, "relaypod").as_deref(), Some("w2"));
        assert_eq!(find_workspace_id(&ws_list, "w1").as_deref(), Some("w1"));
        assert_eq!(find_workspace_id(&ws_list, "missing"), None);
    }
}
