pub mod adapter;
pub mod config;
pub mod exec_adapter;
pub mod herdr_adapter;

use anyhow::{anyhow, Context, Result};
use relaypod_client::{
    run_live_ws_loop_with_outgoing, RelayPodClient, DEFAULT_PING_INTERVAL,
};
use relaypod_core::{UnreadItem, WsClientMessage, WsEvent};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use self::adapter::{DispatchContext, WatcherAdapter};
use self::config::{load_config_file, AgentType, HandleConfig, RunnerKind};
use self::exec_adapter::ExecAdapter;
use self::herdr_adapter::HerdrAdapter;

pub fn default_config_candidates() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("config/.relaypod.yaml"),
        PathBuf::from(".relaypod.yaml"),
    ];
    if let Ok(home) = std::env::var("HOME") {
        paths.push(PathBuf::from(home).join(".relaypod").join("config.yaml"));
    }
    paths
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    } else if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return home;
        }
    }
    path.to_string()
}

#[derive(Debug, Clone)]
pub struct WatchOptions {
    pub default_url: String,
    pub default_token: Option<String>,
    pub config_path: Option<PathBuf>,
    pub handle_filter: Option<String>,
    pub exec_override: Option<String>,
    pub cwd_override: Option<String>,
    pub auto_ack_override: Option<bool>,
    pub no_drain: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub label: String,
    pub runner: RunnerKind,
    pub agent_type: Option<AgentType>,
    pub url: String,
    pub token: String,
    pub exec: String,
    pub args: Option<String>,
    pub prompt: Option<String>,
    pub instructions_file: Option<PathBuf>,
    pub cwd: Option<String>,
    pub herdr_session: Option<String>,
    pub herdr_workspace: Option<String>,
    pub max_sessions: Option<usize>,
    pub auto_ack: bool,
    pub drain_on_start: bool,
    pub cooldown_secs: u64,
    pub env: BTreeMap<String, String>,
}

/// Persistent mapping of `(handle, relaypod_thread_id) -> adapter_session_id`
/// stored at `~/.relaypod/sessions.json`, plus in-memory LRU timestamps for session eviction.
#[derive(Clone)]
pub struct SessionStore {
    path: PathBuf,
    map: Arc<Mutex<BTreeMap<String, String>>>,
    last_used: Arc<Mutex<BTreeMap<String, u128>>>,
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

impl SessionStore {
    fn default_path() -> PathBuf {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".relaypod").join("sessions.json")
        } else {
            PathBuf::from(".relaypod-sessions.json")
        }
    }

    pub fn load_from(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<BTreeMap<String, String>>(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            map: Arc::new(Mutex::new(map)),
            last_used: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn load_default() -> Self {
        Self::load_from(Self::default_path())
    }

    fn key(handle: &str, thread_id: &str) -> String {
        format!("{handle}:{thread_id}")
    }

    fn persist_locked(&self, map: &BTreeMap<String, String>) {
        if let Ok(json) = serde_json::to_string_pretty(map) {
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(err) = std::fs::write(&self.path, json) {
                warn!(path = %self.path.display(), error = %err, "failed to persist sessions.json");
            }
        }
    }

    pub fn get(&self, handle: &str, thread_id: &str) -> Option<String> {
        let guard = self.map.lock().expect("session store lock poisoned");
        guard.get(&Self::key(handle, thread_id)).cloned()
    }

    pub fn touch(&self, handle: &str, thread_id: &str) {
        let mut ts = self.last_used.lock().expect("last_used lock poisoned");
        ts.insert(Self::key(handle, thread_id), now_millis());
    }

    pub fn set(&self, handle: &str, thread_id: &str, session_id: String) {
        let key = Self::key(handle, thread_id);
        {
            let mut guard = self.map.lock().expect("session store lock poisoned");
            guard.insert(key.clone(), session_id);
            self.persist_locked(&guard);
        }
        let mut ts = self.last_used.lock().expect("last_used lock poisoned");
        ts.insert(key, now_millis());
    }

    pub fn remove(&self, handle: &str, thread_id: &str) {
        let key = Self::key(handle, thread_id);
        {
            let mut guard = self.map.lock().expect("session store lock poisoned");
            if guard.remove(&key).is_some() {
                self.persist_locked(&guard);
            }
        }
        let mut ts = self.last_used.lock().expect("last_used lock poisoned");
        ts.remove(&key);
    }

    /// Returns `(thread_id, session_id)` for a given handle, ordered least-recently-used first.
    pub fn list_for_handle_lru(&self, handle: &str) -> Vec<(String, String)> {
        let prefix = format!("{handle}:");
        let entries: Vec<(String, String, String)> = {
            let guard = self.map.lock().expect("session store lock poisoned");
            guard
                .iter()
                .filter_map(|(k, v)| {
                    k.strip_prefix(&prefix)
                        .map(|tid| (k.clone(), tid.to_string(), v.clone()))
                })
                .collect()
        };
        let ts = self.last_used.lock().expect("last_used lock poisoned");
        let mut scored: Vec<(u128, String, String)> = entries
            .into_iter()
            .map(|(k, tid, sid)| (ts.get(&k).copied().unwrap_or(0), tid, sid))
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        scored
            .into_iter()
            .map(|(_, tid, sid)| (tid, sid))
            .collect()
    }
}

fn build_adapter(runner: RunnerKind) -> Arc<dyn WatcherAdapter> {
    match runner {
        RunnerKind::Exec => Arc::new(ExecAdapter),
        RunnerKind::Herdr => Arc::new(HerdrAdapter),
    }
}

pub async fn run_watch(opts: WatchOptions) -> Result<()> {
    let targets = resolve_targets(&opts)?;
    if targets.is_empty() {
        return Err(anyhow!("no handles configured to watch"));
    }

    info!(
        "Starting RelayPod watcher for {} target(s)",
        targets.len()
    );

    let sessions = SessionStore::load_default();
    let mut join_handles = Vec::with_capacity(targets.len());
    for target in targets {
        let sessions_clone = sessions.clone();
        join_handles.push(tokio::spawn(async move {
            if let Err(err) = watch_single_target(target.clone(), sessions_clone).await {
                error!(target = %target.label, error = %err, "watcher exited with error");
            }
        }));
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down watcher...");
        }
        _ = async {
            for h in join_handles {
                let _ = h.await;
            }
        } => {}
    }

    Ok(())
}

fn resolve_targets(opts: &WatchOptions) -> Result<Vec<ResolvedTarget>> {
    let candidates = default_config_candidates();

    let discovered_config = if let Some(ref p) = opts.config_path {
        let expanded = PathBuf::from(expand_tilde(&p.to_string_lossy()));
        if !expanded.exists() {
            return Err(anyhow!("config file not found: {}", expanded.display()));
        }
        Some(expanded)
    } else {
        candidates.iter().find(|p| p.exists()).cloned()
    };

    // Direct CLI mode: `--exec` provided and no `--handle` selected from config
    if let Some(ref exec_cmd) = opts.exec_override {
        if opts.handle_filter.is_none() && opts.config_path.is_none() {
            let token = opts
                .default_token
                .clone()
                .ok_or_else(|| anyhow!("RELAYPOD_TOKEN (or --token) is required with --exec"))?;
            return Ok(vec![ResolvedTarget {
                label: "cli".to_string(),
                runner: RunnerKind::Exec,
                agent_type: None,
                url: opts.default_url.clone(),
                token,
                exec: exec_cmd.clone(),
                args: None,
                prompt: None,
                instructions_file: None,
                cwd: opts.cwd_override.as_deref().map(expand_tilde),
                herdr_session: None,
                herdr_workspace: None,
                max_sessions: None,
                auto_ack: opts.auto_ack_override.unwrap_or(false),
                drain_on_start: !opts.no_drain,
                cooldown_secs: 0,
                env: BTreeMap::new(),
            }]);
        }
    }

    let cfg_path = discovered_config.ok_or_else(|| {
        let searched = candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow!("no config file found (checked {searched}) and no --exec provided")
    })?;

    info!("Loading watch config from {}", cfg_path.display());
    let cfg = load_config_file(&cfg_path)?;

    let selected: Vec<(String, HandleConfig)> = if let Some(ref wanted) = opts.handle_filter {
        let key = wanted.trim_start_matches('@');
        let h = cfg.handles.get(key).cloned().ok_or_else(|| {
            anyhow!(
                "handle '@{key}' not found in {} (available: {})",
                cfg_path.display(),
                cfg.handles
                    .keys()
                    .map(|k| format!("@{k}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        vec![(key.to_string(), h)]
    } else {
        cfg.handles.into_iter().collect()
    };

    let mut targets = Vec::new();
    for (name, h) in selected {
        let url = h
            .url
            .or_else(|| cfg.url.clone())
            .unwrap_or_else(|| opts.default_url.clone());

        let token = h
            .token
            .or_else(|| opts.default_token.clone())
            .ok_or_else(|| {
                anyhow!(
                    "handle '@{name}' has no token configured and RELAYPOD_TOKEN is not set"
                )
            })?;

        let exec = opts.exec_override.clone().unwrap_or(h.exec);
        let (runner, agent_type) = if opts.exec_override.is_some() {
            (RunnerKind::Exec, None)
        } else {
            (h.runner, h.agent_type)
        };

        match runner {
            RunnerKind::Exec => {
                if agent_type.is_none() && exec.trim().is_empty() {
                    return Err(anyhow!(
                        "handle '@{name}' must set either `agent_type` (codex/claude) or `exec`"
                    ));
                }
                if let Some(AgentType::Other(ref other)) = agent_type {
                    return Err(anyhow!(
                        "handle '@{name}' uses `agent_type: {other}` which is not supported by `runner: exec` (use `runner: herdr` or `codex`/`claude`)"
                    ));
                }
            }
            RunnerKind::Herdr => {
                if agent_type.is_none() {
                    return Err(anyhow!(
                        "handle '@{name}' with `runner: herdr` requires `agent_type` (e.g. codex, claude, gemini)"
                    ));
                }
            }
        }

        let instructions_file = h
            .instructions_file
            .or_else(|| cfg.instructions_file.clone())
            .map(|p| PathBuf::from(expand_tilde(&p)));
        if let Some(ref p) = instructions_file {
            if !p.exists() {
                return Err(anyhow!(
                    "handle '@{name}' instructions_file not found: {}",
                    p.display()
                ));
            }
        }

        targets.push(ResolvedTarget {
            label: format!("@{name}"),
            runner,
            agent_type,
            url,
            token,
            exec,
            args: h.args,
            prompt: h.prompt,
            instructions_file,
            cwd: opts
                .cwd_override
                .clone()
                .or(h.cwd)
                .as_deref()
                .map(expand_tilde),
            herdr_session: h.herdr_session,
            herdr_workspace: h.herdr_workspace,
            max_sessions: h.max_sessions.or(cfg.max_sessions),
            auto_ack: opts.auto_ack_override.unwrap_or(h.auto_ack),
            drain_on_start: if opts.no_drain {
                false
            } else {
                h.drain_on_start
            },
            cooldown_secs: h.cooldown_secs,
            env: h.env,
        });
    }

    Ok(targets)
}

async fn watch_single_target(target: ResolvedTarget, sessions: SessionStore) -> Result<()> {
    let client = RelayPodClient::new(target.url.clone(), target.token.clone());
    let me = client
        .me()
        .await
        .with_context(|| format!("failed to authenticate {} at {}", target.label, target.url))?;

    let adapter = build_adapter(target.runner);

    info!(
        handle = %me.handle,
        kind = %me.kind,
        runner = %adapter.name(),
        agent_type = ?target.agent_type,
        url = %target.url,
        auto_ack = target.auto_ack,
        "Authenticated watcher"
    );

    let (tx, mut rx) = mpsc::unbounded_channel::<UnreadItem>();
    let (ws_out_tx, ws_out_rx) = mpsc::unbounded_channel::<WsClientMessage>();

    // Sequential worker so one handle never runs overlapping dispatches at the same time
    let worker_client = client.clone();
    let worker_target = target.clone();
    let worker_handle = me.handle.clone();
    let worker_sessions = sessions.clone();
    let worker_adapter = Arc::clone(&adapter);
    let worker_ws_tx = ws_out_tx.clone();
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            dispatch_unread_item(
                worker_adapter.as_ref(),
                &worker_client,
                &worker_target,
                &worker_handle,
                &item,
                &worker_sessions,
                &worker_ws_tx,
            )
            .await;
            if worker_target.cooldown_secs > 0 {
                tokio::time::sleep(Duration::from_secs(worker_target.cooldown_secs)).await;
            }
        }
    });

    let seen_unread = Arc::new(Mutex::new(HashSet::<String>::new()));
    let ws_url = client.ws_url();

    let status_handle = me.handle.clone();
    let status_client = client.clone();
    let status_tx = tx.clone();
    let status_seen = Arc::clone(&seen_unread);
    let drain_on_start = target.drain_on_start;

    let event_handle = me.handle.clone();
    let event_me_id = me.id.clone();
    let event_tx = tx.clone();
    let event_seen = Arc::clone(&seen_unread);

    run_live_ws_loop_with_outgoing(
        ws_url,
        DEFAULT_PING_INTERVAL,
        ws_out_rx,
        move |live| {
            if live {
                info!(handle = %status_handle, "WebSocket connected (live pings active) — waiting for unread events");
                if drain_on_start {
                    let c = status_client.clone();
                    let tx = status_tx.clone();
                    let seen = Arc::clone(&status_seen);
                    let h = status_handle.clone();
                    tokio::spawn(async move {
                        match c.list_unread().await {
                            Ok(items) => {
                                let mut guard = seen.lock().expect("seen_unread lock poisoned");
                                for item in items {
                                    if guard.insert(item.id.clone()) {
                                        let _ = tx.send(item);
                                    }
                                }
                            }
                            Err(err) => {
                                warn!(handle = %h, error = %err, "failed to drain unread queue");
                            }
                        }
                    });
                }
            } else {
                warn!(handle = %status_handle, "WebSocket disconnected — reconnecting...");
            }
        },
        move |ev| {
            if let WsEvent::UnreadCreated(item) = ev {
                if item.identity_id == event_me_id {
                    let is_new = event_seen
                        .lock()
                        .expect("seen_unread lock poisoned")
                        .insert(item.id.clone());
                    if is_new {
                        info!(
                            handle = %event_handle,
                            unread_id = %item.id,
                            channel = %item.channel_name,
                            author = %item.author_handle,
                            "New unread item received"
                        );
                        let _ = event_tx.send(item);
                    }
                }
            }
        },
    )
    .await;

    Ok(())
}

async fn dispatch_unread_item(
    adapter: &dyn WatcherAdapter,
    client: &RelayPodClient,
    target: &ResolvedTarget,
    handle: &str,
    item: &UnreadItem,
    sessions: &SessionStore,
    ws_out_tx: &mpsc::UnboundedSender<WsClientMessage>,
) {
    let thread_id = item
        .parent_id
        .clone()
        .unwrap_or_else(|| item.message_id.clone());

    let _ = ws_out_tx.send(WsClientMessage::Activity {
        channel_id: item.channel_id.clone(),
        thread_id: Some(thread_id.clone()),
        active: true,
    });

    let ctx = DispatchContext {
        target,
        handle,
        item,
        thread_id: &thread_id,
        sessions,
    };

    let outcome = adapter.dispatch(ctx).await;

    let _ = ws_out_tx.send(WsClientMessage::Activity {
        channel_id: item.channel_id.clone(),
        thread_id: Some(thread_id.clone()),
        active: false,
    });

    if outcome.succeeded {
        info!(
            handle = %handle,
            adapter = %adapter.name(),
            unread_id = %item.id,
            "Watcher dispatch succeeded"
        );
        if target.auto_ack && outcome.allow_auto_ack {
            match client.ack_unread(&item.id).await {
                Ok(()) => {
                    info!(handle = %handle, unread_id = %item.id, "Auto-acked unread item")
                }
                Err(err) => warn!(
                    handle = %handle,
                    unread_id = %item.id,
                    error = %err,
                    "Auto-ack failed (item may already be acked by the agent)"
                ),
            }
        }
    }
}

pub(crate) fn common_env_pairs(
    target: &ResolvedTarget,
    handle: &str,
    item: &UnreadItem,
    thread_id: &str,
    existing_session: Option<&str>,
) -> Vec<(String, String)> {
    let mut pairs = vec![
        ("RELAYPOD_URL".into(), target.url.clone()),
        ("RELAYPOD_TOKEN".into(), target.token.clone()),
        ("RELAYPOD_HANDLE".into(), handle.to_string()),
        ("RELAYPOD_UNREAD_ID".into(), item.id.clone()),
        ("RELAYPOD_MESSAGE_ID".into(), item.message_id.clone()),
        ("RELAYPOD_THREAD_ID".into(), thread_id.to_string()),
        ("RELAYPOD_CHANNEL_ID".into(), item.channel_id.clone()),
        ("RELAYPOD_CHANNEL".into(), item.channel_name.clone()),
        ("RELAYPOD_AUTHOR".into(), item.author_handle.clone()),
        ("RELAYPOD_REASON".into(), item.reason.to_string()),
        ("RELAYPOD_BODY".into(), item.body.clone()),
    ];
    if let Some(sid) = existing_session {
        pairs.push(("RELAYPOD_SESSION_ID".into(), sid.to_string()));
    }
    for (k, v) in &target.env {
        pairs.push((k.clone(), v.clone()));
    }
    pairs
}

pub(crate) fn apply_common_env(
    cmd: &mut Command,
    target: &ResolvedTarget,
    handle: &str,
    item: &UnreadItem,
    thread_id: &str,
    existing_session: Option<&str>,
) {
    if let Some(ref cwd) = target.cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in common_env_pairs(target, handle, item, thread_id, existing_session) {
        cmd.env(k, v);
    }
}

pub(crate) fn build_agent_prompt(
    target: &ResolvedTarget,
    handle: &str,
    item: &UnreadItem,
    thread_id: &str,
    is_resume: bool,
) -> String {
    let base = if let Some(ref custom) = target.prompt {
        render_exec_template(custom, handle, item, thread_id)
    } else if is_resume {
        format!(
            "Follow-up reply in RelayPod channel #{} (thread {}, unread {}) from @{}:\n\n{}\n\nRespond in-thread using RelayPod MCP `post_message` (with parent_id=\"{}\") and acknowledge unread item `{}` via `ack_unread`.",
            item.channel_name, thread_id, item.id, item.author_handle, item.body, thread_id, item.id
        )
    } else {
        format!(
            "New RelayPod message in #{} (thread {}, unread {}) from @{}:\n\n{}\n\nIf you need prior context, call RelayPod MCP `read_thread` on `{}`. Reply in-thread using `post_message` (with parent_id=\"{}\") and acknowledge unread item `{}` via `ack_unread`.",
            item.channel_name, thread_id, item.id, item.author_handle, item.body, thread_id, thread_id, item.id
        )
    };

    if !is_resume {
        if let Some(ref path) = target.instructions_file {
            match std::fs::read_to_string(path) {
                Ok(instructions) if !instructions.trim().is_empty() => {
                    return format!("{}\n\n---\n\n{}", instructions.trim(), base);
                }
                Ok(_) => {}
                Err(err) => {
                    warn!(path = %path.display(), error = %err, "failed to read instructions_file");
                }
            }
        }
    }

    base
}

pub(crate) fn split_shell_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '\\' if in_double => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            other => cur.push(other),
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

pub(crate) fn render_exec_template(
    template: &str,
    handle: &str,
    item: &UnreadItem,
    thread_id: &str,
) -> String {
    template
        .replace("{{handle}}", handle)
        .replace("{{unread_id}}", &item.id)
        .replace("{{message_id}}", &item.message_id)
        .replace("{{thread_id}}", thread_id)
        .replace("{{channel_id}}", &item.channel_id)
        .replace("{{channel}}", &item.channel_name)
        .replace("{{author}}", &item.author_handle)
        .replace("{{reason}}", &item.reason.to_string())
        .replace("{{body}}", &item.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_shell_words_with_quotes() {
        assert_eq!(
            split_shell_words(r#"--full-auto -m "gpt-5" --skip-git-repo-check"#),
            vec!["--full-auto", "-m", "gpt-5", "--skip-git-repo-check"]
        );
    }

    #[test]
    fn session_store_orders_by_lru_and_removes() {
        let tmp = std::env::temp_dir().join(format!(
            "relaypod-sessions-lru-test-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp);
        let store = SessionStore::load_from(tmp.clone());

        store.set("codex", "t1", "herdr:w1:p1".into());
        std::thread::sleep(Duration::from_millis(5));
        store.set("codex", "t2", "herdr:w1:p2".into());
        std::thread::sleep(Duration::from_millis(5));
        store.set("codex", "t3", "herdr:w1:p3".into());
        std::thread::sleep(Duration::from_millis(5));
        // Touch t1 so t2 becomes the oldest (LRU)
        store.touch("codex", "t1");

        let order: Vec<String> = store
            .list_for_handle_lru("codex")
            .into_iter()
            .map(|(tid, _)| tid)
            .collect();
        assert_eq!(order, vec!["t2", "t3", "t1"]);

        store.remove("codex", "t2");
        assert!(store.get("codex", "t2").is_none());
        let _ = std::fs::remove_file(&tmp);
    }
}
