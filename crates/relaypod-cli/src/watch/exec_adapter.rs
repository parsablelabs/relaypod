use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{error, info, warn};

use super::adapter::{DispatchContext, DispatchOutcome, WatcherAdapter};
use super::config::AgentType;
use super::{
    apply_common_env, build_agent_prompt_with_context, render_shell_exec_template,
    split_shell_words,
};

/// Adapter that executes headless CLI commands directly:
/// - Managed `codex exec` / `claude -p` with automatic JSON session ID capture & resume
/// - Custom shell `exec:` commands when `agent_type` is not set
pub struct ExecAdapter;

impl WatcherAdapter for ExecAdapter {
    fn name(&self) -> &'static str {
        "exec"
    }

    fn dispatch<'a>(
        &'a self,
        ctx: DispatchContext<'a>,
    ) -> Pin<Box<dyn Future<Output = DispatchOutcome> + Send + 'a>> {
        Box::pin(async move {
            let existing_session = ctx
                .sessions
                .get(ctx.identity_id, ctx.thread_id)
                // Ignore session entries written by other adapters (e.g. `herdr:w1:p2`)
                // or malformed values that could be parsed as CLI flags.
                .filter(|s| !s.starts_with("herdr:") && is_safe_session_id(s));

            let succeeded = match &ctx.target.agent_type {
                Some(agent_type) => {
                    run_managed_agent(agent_type, &ctx, existing_session.as_deref()).await
                }
                None => run_custom_shell(&ctx, existing_session.as_deref()).await,
            };

            if succeeded {
                DispatchOutcome::ok()
            } else {
                DispatchOutcome::failed()
            }
        })
    }
}

async fn run_custom_shell(ctx: &DispatchContext<'_>, existing_session: Option<&str>) -> bool {
    let rendered_cmd =
        render_shell_exec_template(&ctx.target.exec, ctx.handle, ctx.item, ctx.thread_id);
    info!(
        handle = %ctx.handle,
        unread_id = %ctx.item.id,
        channel = %ctx.item.channel_name,
        "Executing watcher shell command"
    );

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&rendered_cmd);
    apply_common_env(
        &mut cmd,
        ctx.target,
        ctx.handle,
        ctx.item,
        ctx.thread_id,
        existing_session,
    );

    match cmd.status().await {
        Ok(status) if status.success() => true,
        Ok(status) => {
            warn!(
                handle = %ctx.handle,
                unread_id = %ctx.item.id,
                code = ?status.code(),
                "Watcher command exited non-zero"
            );
            false
        }
        Err(err) => {
            error!(
                handle = %ctx.handle,
                unread_id = %ctx.item.id,
                error = %err,
                "Failed to spawn watcher command"
            );
            false
        }
    }
}

async fn run_managed_agent(
    agent_type: &AgentType,
    ctx: &DispatchContext<'_>,
    existing_session: Option<&str>,
) -> bool {
    let is_resume = existing_session.is_some();
    let prompt = build_agent_prompt_with_context(ctx, is_resume).await;
    let extra_args = ctx
        .target
        .args
        .as_deref()
        .map(split_shell_words)
        .unwrap_or_default();

    let mut cmd = match agent_type {
        AgentType::Codex => {
            let mut c = Command::new("codex");
            c.arg("exec");
            if let Some(sid) = existing_session {
                c.arg("resume").arg(sid);
            }
            c.arg("--json");
            c.args(&extra_args);
            c.arg(&prompt);
            c
        }
        AgentType::Claude => {
            let mut c = Command::new("claude");
            c.arg("-p").arg("--output-format").arg("json");
            if let Some(sid) = existing_session {
                c.arg("--resume").arg(sid);
            }
            c.args(&extra_args);
            c.arg(&prompt);
            c
        }
        AgentType::Other(other) => {
            error!(
                handle = %ctx.handle,
                agent_type = %other,
                "ExecAdapter only supports `codex` and `claude` (use `runner: herdr` for `{other}`)"
            );
            return false;
        }
    };

    apply_common_env(
        &mut cmd,
        ctx.target,
        ctx.handle,
        ctx.item,
        ctx.thread_id,
        existing_session,
    );
    cmd.stdout(Stdio::piped()).stderr(Stdio::inherit());

    info!(
        handle = %ctx.handle,
        agent_type = ?agent_type,
        thread_id = %ctx.thread_id,
        unread_id = %ctx.item.id,
        resuming = is_resume,
        session_id = ?existing_session,
        "Launching managed agent via exec adapter"
    );

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            error!(
                handle = %ctx.handle,
                agent_type = ?agent_type,
                error = %err,
                "Failed to spawn managed agent"
            );
            return false;
        }
    };

    let mut captured_session: Option<String> = None;
    if let Some(stdout) = child.stdout.take() {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if captured_session.is_none() {
                if let Some(sid) = extract_session_id_from_json_line(agent_type, &line) {
                    info!(
                        handle = %ctx.handle,
                        thread_id = %ctx.thread_id,
                        session_id = %sid,
                        "Captured agent session ID for thread"
                    );
                    ctx.sessions.set(ctx.identity_id, ctx.thread_id, sid.clone());
                    captured_session = Some(sid);
                }
            }
            println!("{line}");
        }
    }

    match child.wait().await {
        Ok(status) if status.success() => true,
        Ok(status) => {
            warn!(
                handle = %ctx.handle,
                agent_type = ?agent_type,
                unread_id = %ctx.item.id,
                code = ?status.code(),
                "Managed agent exited non-zero"
            );
            false
        }
        Err(err) => {
            error!(
                handle = %ctx.handle,
                agent_type = ?agent_type,
                error = %err,
                "Failed waiting on managed agent"
            );
            false
        }
    }
}

pub(crate) fn is_safe_session_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
}

pub(crate) fn extract_session_id_from_json_line(
    agent_type: &AgentType,
    line: &str,
) -> Option<String> {
    let val: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let raw = match agent_type {
        AgentType::Codex => val
            .get("thread_id")
            .or_else(|| val.get("session_id"))
            .and_then(|v| v.as_str()),
        AgentType::Claude => val.get("session_id").and_then(|v| v.as_str()),
        AgentType::Other(_) => None,
    }?;
    is_safe_session_id(raw).then(|| raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_codex_and_claude_session_ids() {
        let codex_line =
            r#"{"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a53"}"#;
        assert_eq!(
            extract_session_id_from_json_line(&AgentType::Codex, codex_line).as_deref(),
            Some("0199a213-81c0-7800-8aa1-bbab2a035a53")
        );

        let claude_line = r#"{"type":"result","session_id":"sess_abc123","result":"done"}"#;
        assert_eq!(
            extract_session_id_from_json_line(&AgentType::Claude, claude_line).as_deref(),
            Some("sess_abc123")
        );

        let flag_smuggle = r#"{"type":"result","session_id":"--dangerously-skip-permissions"}"#;
        assert_eq!(
            extract_session_id_from_json_line(&AgentType::Claude, flag_smuggle),
            None
        );
    }
}
