pub mod adapter;
pub mod config;
pub mod exec_adapter;
pub mod herdr_adapter;
pub mod supervisor;

use crate::{run_live_ws_loop_with_outgoing, RelayPodClient, DEFAULT_PING_INTERVAL};
use anyhow::{anyhow, Context, Result};
use relaypod_core::{
    ActivityState, AgentConfig, Channel, UnreadItem, WsClientMessage, WsEvent, DEFAULT_AGENT_CWD,
};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::{mpsc, watch as tokio_watch};
use tracing::{error, info, warn};

use self::adapter::{ActivityUpdate, DispatchContext, WatcherAdapter};
pub use self::config::{
    append_watcher_token_to_config, ensure_default_config, ensure_default_server_config,
    remove_watcher_token_from_config, update_client_token_in_config,
    update_watcher_token_in_config, upsert_client_connection_in_config,
    upsert_watcher_token_in_config, AgentType, RunnerKind, ServerConfig, DEFAULT_SERVER_HOST,
    DEFAULT_SERVER_PORT, DEFAULT_SERVER_URL,
};
use self::config::{
    load_config_file, load_server_config_file, DEFAULT_MAX_CONSECUTIVE_AGENT_TURNS,
};
use self::exec_adapter::ExecAdapter;
pub use self::herdr_adapter::focus_herdr_pane;
use self::herdr_adapter::HerdrAdapter;
use self::supervisor::{resolved_subprocess_path, WatcherReporter};
pub use self::supervisor::{TargetPhase, WatchedTargetState, WatcherSnapshot, WatcherSupervisor};

pub fn default_config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| anyhow!("HOME environment variable is not set; pass --config <path>"))?;
    Ok(PathBuf::from(home).join(".relaypod").join("config.yaml"))
}

pub fn default_server_config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| anyhow!("HOME environment variable is not set; pass --config <path>"))?;
    Ok(PathBuf::from(home).join(".relaypod").join("server.yaml"))
}

pub fn default_config_candidates() -> Vec<PathBuf> {
    default_config_path().into_iter().collect()
}

pub(crate) fn resolve_config_path(
    explicit_path: Option<&std::path::Path>,
) -> Result<Option<PathBuf>> {
    if let Some(p) = explicit_path {
        let expanded = PathBuf::from(expand_tilde(&p.to_string_lossy()));
        if !expanded.exists() {
            return Err(anyhow!("config file not found: {}", expanded.display()));
        }
        return Ok(Some(expanded));
    }
    Ok(default_config_candidates().into_iter().find(|p| p.exists()))
}

fn resolve_server_config_path(explicit_path: Option<&std::path::Path>) -> Result<Option<PathBuf>> {
    if let Some(p) = explicit_path {
        let expanded = PathBuf::from(expand_tilde(&p.to_string_lossy()));
        if !expanded.exists() {
            return Err(anyhow!(
                "server config file not found: {}",
                expanded.display()
            ));
        }
        return Ok(Some(expanded));
    }
    Ok(default_server_config_path().ok().filter(|p| p.exists()))
}

pub fn expand_tilde(path: &str) -> String {
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub global_max_sessions: Option<usize>,
    pub max_sessions: Option<usize>,
    pub max_consecutive_agent_turns: Option<usize>,
    pub auto_ack: bool,
    pub drain_on_start: bool,
    pub cooldown_secs: u64,
    pub env: BTreeMap<String, String>,
}

/// Persistent mapping of `(identity_id, relaypod_thread_id) -> adapter_session_id`
/// stored at `~/.relaypod/sessions.json`, plus in-memory LRU counters for session eviction.
#[derive(Clone)]
pub struct SessionStore {
    path: PathBuf,
    map: Arc<Mutex<BTreeMap<String, String>>>,
    last_used: Arc<Mutex<BTreeMap<String, u64>>>,
    tick: Arc<std::sync::atomic::AtomicU64>,
}

impl SessionStore {
    fn default_path() -> PathBuf {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".relaypod").join("sessions.json")
        } else {
            std::env::temp_dir().join("relaypod-sessions.json")
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
            tick: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    pub(crate) fn load_default() -> Self {
        Self::load_from(Self::default_path())
    }

    fn next_tick(&self) -> u64 {
        self.tick.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn key(identity_id: &str, thread_id: &str) -> String {
        format!("{identity_id}:{thread_id}")
    }

    fn persist_locked(&self, map: &BTreeMap<String, String>) {
        let Ok(json) = serde_json::to_string_pretty(map) else {
            return;
        };
        if let Some(parent) = self.path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                warn!(path = %parent.display(), error = %err, "failed to create sessions directory");
                return;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        let tmp_path = self.path.with_extension("json.tmp");
        if let Err(err) = write_private_file(&tmp_path, json.as_bytes()) {
            warn!(path = %tmp_path.display(), error = %err, "failed to write temporary sessions.json");
            return;
        }
        if let Err(err) = std::fs::rename(&tmp_path, &self.path) {
            let _ = std::fs::remove_file(&tmp_path);
            warn!(path = %self.path.display(), error = %err, "failed to persist sessions.json");
        }
    }

    pub fn get(&self, identity_id: &str, thread_id: &str) -> Option<String> {
        let guard = self.map.lock().expect("session store lock poisoned");
        guard.get(&Self::key(identity_id, thread_id)).cloned()
    }

    pub fn touch(&self, identity_id: &str, thread_id: &str) {
        let mut ts = self.last_used.lock().expect("last_used lock poisoned");
        ts.insert(Self::key(identity_id, thread_id), self.next_tick());
    }

    pub fn set(&self, identity_id: &str, thread_id: &str, session_id: String) {
        let key = Self::key(identity_id, thread_id);
        {
            let mut guard = self.map.lock().expect("session store lock poisoned");
            guard.insert(key.clone(), session_id);
            self.persist_locked(&guard);
        }
        let mut ts = self.last_used.lock().expect("last_used lock poisoned");
        ts.insert(key, self.next_tick());
    }

    pub fn remove(&self, identity_id: &str, thread_id: &str) {
        let key = Self::key(identity_id, thread_id);
        {
            let mut guard = self.map.lock().expect("session store lock poisoned");
            if guard.remove(&key).is_some() {
                self.persist_locked(&guard);
            }
        }
        let mut ts = self.last_used.lock().expect("last_used lock poisoned");
        ts.remove(&key);
    }

    /// Returns `(thread_id, session_id)` for a given identity, ordered least-recently-used first.
    pub fn list_for_handle_lru(&self, identity_id: &str) -> Vec<(String, String)> {
        let prefix = format!("{identity_id}:");
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
        let mut scored: Vec<(u64, String, String)> = entries
            .into_iter()
            .map(|(k, tid, sid)| (ts.get(&k).copied().unwrap_or(0), tid, sid))
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, tid, sid)| (tid, sid)).collect()
    }

    /// Returns `(handle, thread_id, session_id)` across all handles, ordered least-recently-used first.
    pub fn list_all_lru(&self) -> Vec<(String, String, String)> {
        let entries: Vec<(String, String, String, String)> = {
            let guard = self.map.lock().expect("session store lock poisoned");
            guard
                .iter()
                .filter_map(|(k, v)| {
                    k.split_once(':')
                        .map(|(h, tid)| (k.clone(), h.to_string(), tid.to_string(), v.clone()))
                })
                .collect()
        };
        let ts = self.last_used.lock().expect("last_used lock poisoned");
        let mut scored: Vec<(u64, String, String, String)> = entries
            .into_iter()
            .map(|(k, h, tid, sid)| (ts.get(&k).copied().unwrap_or(0), h, tid, sid))
            .collect();
        scored.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        scored
            .into_iter()
            .map(|(_, h, tid, sid)| (h, tid, sid))
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

    info!("Starting RelayPod watcher for {} target(s)", targets.len());

    let (shutdown_tx, shutdown_rx) = tokio_watch::channel(false);
    let sessions = SessionStore::load_default();
    let mut join_handles = Vec::with_capacity(targets.len());
    for target in targets {
        let sessions_clone = sessions.clone();
        let rx_clone = shutdown_rx.clone();
        join_handles.push(tokio::spawn(async move {
            if let Err(err) =
                watch_single_target(target.clone(), sessions_clone, None, rx_clone).await
            {
                error!(target = %target.label, error = %err, "watcher exited with error");
            }
        }));
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down watcher...");
            let _ = shutdown_tx.send(true);
        }
        _ = async {
            for h in join_handles {
                let _ = h.await;
            }
        } => {}
    }

    Ok(())
}

pub fn load_optional_config(
    explicit_path: Option<&std::path::Path>,
) -> Result<Option<(PathBuf, config::RelayPodConfig)>> {
    match resolve_config_path(explicit_path)? {
        Some(path) => {
            let cfg = load_config_file(&path)?;
            Ok(Some((path, cfg)))
        }
        None => Ok(None),
    }
}

pub fn load_optional_server_config(
    explicit_path: Option<&std::path::Path>,
) -> Result<Option<(PathBuf, ServerConfig)>> {
    match resolve_server_config_path(explicit_path)? {
        Some(path) => {
            let cfg = load_server_config_file(&path)?;
            Ok(Some((path, cfg)))
        }
        None => Ok(None),
    }
}

fn write_private_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

pub(crate) fn resolve_targets(opts: &WatchOptions) -> Result<Vec<ResolvedTarget>> {
    let discovered_config = resolve_config_path(opts.config_path.as_deref())?;

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
                cwd: Some(expand_tilde(
                    opts.cwd_override.as_deref().unwrap_or(DEFAULT_AGENT_CWD),
                )),
                global_max_sessions: None,
                max_sessions: None,
                max_consecutive_agent_turns: Some(DEFAULT_MAX_CONSECUTIVE_AGENT_TURNS),
                auto_ack: opts.auto_ack_override.unwrap_or(false),
                drain_on_start: !opts.no_drain,
                cooldown_secs: 0,
                env: BTreeMap::new(),
            }]);
        }
    }

    let cfg_path = discovered_config.ok_or_else(|| {
        let searched = default_config_candidates()
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow!("no config file found (checked {searched}) and no --exec provided")
    })?;

    info!("Loading watch config from {}", cfg_path.display());
    let full_cfg = load_config_file(&cfg_path)?;
    let root_url = full_cfg.url;
    let cfg = full_cfg.watcher;

    let selected: Vec<(String, String)> = if let Some(ref wanted) = opts.handle_filter {
        let key = wanted.trim_start_matches('@');
        let token = cfg.agent_tokens.get(key).cloned().ok_or_else(|| {
            anyhow!(
                "token '{key}' not found under `watcher.agent_tokens` in {} (available: {})",
                cfg_path.display(),
                cfg.agent_tokens
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        vec![(key.to_string(), token)]
    } else {
        cfg.agent_tokens.into_iter().collect()
    };

    let default_instructions_file = cfg
        .instructions_file
        .as_deref()
        .map(|p| PathBuf::from(expand_tilde(p)));
    if let Some(ref p) = default_instructions_file {
        if !p.exists() {
            return Err(anyhow!(
                "watcher instructions_file not found: {}",
                p.display()
            ));
        }
    }

    let url = root_url.unwrap_or_else(|| opts.default_url.clone());
    let mut targets = Vec::new();
    for (name, token) in selected {
        let exec = opts.exec_override.clone().unwrap_or_default();
        let runner = if opts.exec_override.is_some() {
            RunnerKind::Exec
        } else {
            RunnerKind::Herdr
        };

        targets.push(ResolvedTarget {
            label: name,
            runner,
            agent_type: None,
            url: url.clone(),
            token,
            exec,
            args: None,
            prompt: None,
            instructions_file: default_instructions_file.clone(),
            cwd: Some(expand_tilde(
                opts.cwd_override.as_deref().unwrap_or(DEFAULT_AGENT_CWD),
            )),
            global_max_sessions: cfg.global_max_sessions,
            max_sessions: None,
            max_consecutive_agent_turns: cfg.max_consecutive_agent_turns,
            auto_ack: opts.auto_ack_override.unwrap_or(false),
            drain_on_start: !opts.no_drain,
            cooldown_secs: 0,
            env: BTreeMap::new(),
        });
    }

    Ok(targets)
}

fn apply_server_agent_config(
    base: &ResolvedTarget,
    agent_config: Option<&AgentConfig>,
) -> ResolvedTarget {
    let mut effective = base.clone();
    if let Some(cfg) = agent_config {
        if let Some(ref runner_str) = cfg.runner {
            if let Some(parsed) = RunnerKind::parse(runner_str) {
                effective.runner = parsed;
            }
        }
        if let Some(ref exec) = cfg.exec {
            effective.exec = exec.clone();
        }
        if let Some(ref h) = cfg.harness {
            if let Some(parsed) = AgentType::parse(h) {
                effective.agent_type = Some(parsed);
            }
        }
        if let Some(ref args) = cfg.args {
            effective.args = Some(args.clone());
        }
        if let Some(ref cwd) = cfg.cwd {
            effective.cwd = Some(expand_tilde(cwd));
        }
        if let Some(ref prompt) = cfg.prompt {
            effective.prompt = Some(prompt.clone());
        }
        if let Some(ref instr) = cfg.instructions_file {
            effective.instructions_file = Some(PathBuf::from(expand_tilde(instr)));
        }
        if let Some(max_sessions) = cfg.max_sessions {
            effective.max_sessions = Some(max_sessions);
        }
        if let Some(auto_ack) = cfg.auto_ack {
            effective.auto_ack = auto_ack;
        }
        if let Some(drain_on_start) = cfg.drain_on_start {
            effective.drain_on_start = drain_on_start;
        }
        if let Some(cooldown_secs) = cfg.cooldown_secs {
            effective.cooldown_secs = cooldown_secs;
        }
    }
    effective
}

async fn wait_for_shutdown(shutdown_rx: &mut tokio_watch::Receiver<bool>) {
    if *shutdown_rx.borrow() {
        return;
    }
    while shutdown_rx.changed().await.is_ok() {
        if *shutdown_rx.borrow() {
            return;
        }
    }
}

fn format_runner_and_harness(target: &ResolvedTarget) -> (Option<String>, Option<String>) {
    let runner = match target.runner {
        RunnerKind::Exec => "exec",
        RunnerKind::Herdr => "herdr",
    };
    let harness = target
        .agent_type
        .as_ref()
        .map(|at| at.as_herdr_kind().to_string());
    (Some(runner.to_string()), harness)
}

pub(crate) async fn watch_single_target(
    target: ResolvedTarget,
    sessions: SessionStore,
    reporter: Option<WatcherReporter>,
    mut shutdown_rx: tokio_watch::Receiver<bool>,
) -> Result<()> {
    let token_name = target.label.clone();
    let client = RelayPodClient::new(target.url.clone(), target.token.clone());
    let me = tokio::select! {
        _ = wait_for_shutdown(&mut shutdown_rx) => return Ok(()),
        res = client.me() => {
            res.with_context(|| format!("failed to authenticate {} at {}", target.label, target.url))?
        }
    };

    let base_target = target;
    let effective_target = apply_server_agent_config(&base_target, me.agent_config.as_ref());
    if let Some(ref cwd) = effective_target.cwd {
        let default_expanded = expand_tilde(DEFAULT_AGENT_CWD);
        if cwd == &default_expanded {
            if let Err(err) = std::fs::create_dir_all(cwd) {
                warn!(cwd = %cwd, error = %err, "Failed to create default agent workspace directory");
            }
        }
    }
    let initial_adapter = build_adapter(effective_target.runner);

    info!(
        handle = %me.handle,
        kind = %me.kind,
        runner = %initial_adapter.name(),
        agent_type = ?effective_target.agent_type,
        args = ?effective_target.args,
        url = %effective_target.url,
        auto_ack = effective_target.auto_ack,
        cooldown_secs = effective_target.cooldown_secs,
        "Authenticated watcher"
    );

    if let Some(ref rep) = reporter {
        let (runner_name, harness_name) = format_runner_and_harness(&effective_target);
        rep.update_target_meta(
            &token_name,
            Some(me.id.clone()),
            Some(me.handle.clone()),
            runner_name.clone(),
            harness_name.clone(),
        );
        rep.push_log(format!(
            "[@{}] authenticated ({}{})",
            me.handle,
            runner_name.as_deref().unwrap_or("herdr"),
            harness_name
                .as_deref()
                .map(|h| format!("/{h}"))
                .unwrap_or_default()
        ));
    }

    let live_handle = Arc::new(Mutex::new(me.handle.clone()));
    let live_target = Arc::new(Mutex::new(effective_target));
    let ws_live_flag = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = mpsc::unbounded_channel::<UnreadItem>();
    let (ws_out_tx, ws_out_rx) = mpsc::unbounded_channel::<WsClientMessage>();

    // Sequential worker so one handle never runs overlapping dispatches at the same time
    let worker_client = client.clone();
    let worker_target = Arc::clone(&live_target);
    let worker_identity_id = me.id.clone();
    let worker_handle = Arc::clone(&live_handle);
    let worker_sessions = sessions.clone();
    let worker_ws_tx = ws_out_tx.clone();
    let worker_reporter = reporter.clone();
    let worker_token_name = token_name.clone();
    let worker_ws_live = Arc::clone(&ws_live_flag);
    let mut worker_shutdown_rx = shutdown_rx.clone();
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            if *worker_shutdown_rx.borrow() {
                break;
            }
            let current_handle = worker_handle
                .lock()
                .expect("live_handle lock poisoned")
                .clone();
            let current_target = worker_target
                .lock()
                .expect("live_target lock poisoned")
                .clone();
            let adapter = build_adapter(current_target.runner);

            if let Some(ref rep) = worker_reporter {
                rep.set_target_phase(
                    &worker_token_name,
                    TargetPhase::Working {
                        channel: item.channel_name.clone(),
                    },
                );
                rep.push_log(format!(
                    "[@{}] dispatching #{} from @{}",
                    current_handle, item.channel_name, item.author_handle
                ));
            }

            let on_phase = |state: ActivityState| {
                if let Some(ref rep) = worker_reporter {
                    let phase = match state {
                        ActivityState::Working => TargetPhase::Working {
                            channel: item.channel_name.clone(),
                        },
                        ActivityState::Blocked => TargetPhase::Blocked {
                            channel: item.channel_name.clone(),
                        },
                    };
                    rep.set_target_phase(&worker_token_name, phase);
                    let note = match state {
                        ActivityState::Working => {
                            format!("unblocked in #{} — working", item.channel_name)
                        }
                        ActivityState::Blocked => {
                            format!("blocked in #{} (waiting for input)", item.channel_name)
                        }
                    };
                    rep.push_log(format!("[@{}] {note}", current_handle));
                }
            };

            let dispatch_env = DispatchEnv {
                adapter: adapter.as_ref(),
                client: &worker_client,
                target: &current_target,
                identity_id: &worker_identity_id,
                sessions: &worker_sessions,
                ws_out_tx: &worker_ws_tx,
                on_activity_change: &on_phase,
            };

            let outcome = tokio::select! {
                _ = wait_for_shutdown(&mut worker_shutdown_rx) => break,
                res = dispatch_unread_item(&dispatch_env, &current_handle, &item) => res,
            };

            if let Some(ref rep) = worker_reporter {
                let next_phase = if worker_ws_live.load(Ordering::Relaxed) {
                    TargetPhase::Connected
                } else {
                    TargetPhase::Reconnecting
                };
                rep.set_target_phase(&worker_token_name, next_phase);
                let status_label = match outcome {
                    DispatchStatus::Succeeded => "dispatch succeeded".to_string(),
                    DispatchStatus::Blocked => "dispatch blocked (waiting for input)".to_string(),
                    DispatchStatus::Failed(Some(detail)) => format!("dispatch failed: {detail}"),
                    DispatchStatus::Failed(None) => "dispatch failed".to_string(),
                    DispatchStatus::CircuitBreakerPaused => "paused by circuit breaker".to_string(),
                };
                rep.push_log(format!("[@{}] {status_label}", current_handle));
            }

            if current_target.cooldown_secs > 0 {
                tokio::select! {
                    _ = wait_for_shutdown(&mut worker_shutdown_rx) => break,
                    _ = tokio::time::sleep(Duration::from_secs(current_target.cooldown_secs)) => {}
                }
            }
        }
    });

    let seen_unread = Arc::new(Mutex::new(HashSet::<String>::new()));
    let ws_url = client.ws_url();
    let ws_token = client.token().to_string();

    let status_handle = Arc::clone(&live_handle);
    let status_target = Arc::clone(&live_target);
    let status_client = client.clone();
    let status_tx = tx.clone();
    let status_seen = Arc::clone(&seen_unread);
    let status_reporter = reporter.clone();
    let status_token_name = token_name.clone();
    let status_ws_live = Arc::clone(&ws_live_flag);

    let event_handle = Arc::clone(&live_handle);
    let event_target = Arc::clone(&live_target);
    let event_base_target = base_target;
    let event_me_id = me.id.clone();
    let event_tx = tx.clone();
    let event_seen = Arc::clone(&seen_unread);
    let event_reporter = reporter;
    let event_token_name = token_name;

    tokio::select! {
        _ = wait_for_shutdown(&mut shutdown_rx) => {}
        _ = run_live_ws_loop_with_outgoing(
            ws_url,
            ws_token,
            DEFAULT_PING_INTERVAL,
            ws_out_rx,
            move |live| {
                status_ws_live.store(live, Ordering::Relaxed);
                let h = status_handle
                    .lock()
                    .expect("live_handle lock poisoned")
                    .clone();
                let drain_on_start = status_target
                    .lock()
                    .expect("live_target lock poisoned")
                    .drain_on_start;
                if let Some(ref rep) = status_reporter {
                    rep.set_target_ws_live(&status_token_name, live);
                }
                if live {
                    info!(handle = %h, "WebSocket connected (live pings active) — waiting for unread events");
                    if let Some(ref rep) = status_reporter {
                        rep.push_log(format!("[@{h}] connected"));
                    }
                    if drain_on_start {
                        let c = status_client.clone();
                        let tx = status_tx.clone();
                        let seen = Arc::clone(&status_seen);
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
                    warn!(handle = %h, "WebSocket disconnected — reconnecting...");
                    if let Some(ref rep) = status_reporter {
                        rep.push_log(format!("[@{h}] disconnected — reconnecting..."));
                    }
                }
            },
            move |ev| match ev {
                WsEvent::IdentityUpdated(updated) if updated.id == event_me_id => {
                    {
                        let mut guard = event_handle.lock().expect("live_handle lock poisoned");
                        if *guard != updated.handle {
                            info!(
                                old_handle = %*guard,
                                new_handle = %updated.handle,
                                "Watcher identity renamed"
                            );
                            *guard = updated.handle.clone();
                        }
                    }
                    let next_target =
                        apply_server_agent_config(&event_base_target, updated.agent_config.as_ref());
                    let mut tguard = event_target.lock().expect("live_target lock poisoned");
                    let changed = *tguard != next_target;
                    if changed {
                        info!(
                            handle = %updated.handle,
                            runner = ?next_target.runner,
                            agent_type = ?next_target.agent_type,
                            args = ?next_target.args,
                            cooldown_secs = next_target.cooldown_secs,
                            "Reloaded agent config from server (applies to new sessions)"
                        );
                        *tguard = next_target.clone();
                    }
                    if let Some(ref rep) = event_reporter {
                        let (runner_name, harness_name) = format_runner_and_harness(&next_target);
                        rep.update_target_meta(
                            &event_token_name,
                            None,
                            Some(updated.handle.clone()),
                            runner_name,
                            harness_name,
                        );
                        if changed {
                            rep.push_log(format!("[@{}] reloaded agent config", updated.handle));
                        }
                    }
                }
                WsEvent::UnreadCreated(item) if item.identity_id == event_me_id => {
                    let is_new = event_seen
                        .lock()
                        .expect("seen_unread lock poisoned")
                        .insert(item.id.clone());
                    if is_new {
                        let h = event_handle
                            .lock()
                            .expect("live_handle lock poisoned")
                            .clone();
                        info!(
                            handle = %h,
                            unread_id = %item.id,
                            channel = %item.channel_name,
                            author = %item.author_handle,
                            "New unread item received"
                        );
                        let _ = event_tx.send(item);
                    }
                }
                _ => {}
            },
        ) => {}
    }

    Ok(())
}

const CIRCUIT_BREAKER_MARKER: &str = "Paused automatic agent collaboration";

/// Analyze a thread up to and including `current_message_id`:
/// - `consecutive_agent_turns`: count of trailing consecutive agent-authored messages
///   (resetting whenever a human posts in the thread).
/// - `last_human`: handle of the most recent human author in the thread (if any).
/// - `already_paused`: whether the latest message in the thread is already a circuit-breaker notice.
pub(crate) fn analyze_agent_turns(
    thread: &relaypod_core::ThreadView,
    current_message_id: &str,
) -> (usize, Option<String>, bool) {
    let mut msgs: Vec<&relaypod_core::Message> = Vec::with_capacity(1 + thread.replies.len());
    msgs.push(&thread.parent);
    msgs.extend(thread.replies.iter());

    let already_paused = msgs
        .last()
        .map(|m| m.body.contains(CIRCUIT_BREAKER_MARKER))
        .unwrap_or(false);

    let cutoff = msgs
        .iter()
        .position(|m| m.id == current_message_id)
        .map(|idx| idx + 1)
        .unwrap_or(msgs.len());
    let relevant = &msgs[..cutoff];

    let mut consecutive_agent_turns = 0usize;
    for msg in relevant.iter().rev() {
        if msg.author_kind == relaypod_core::IdentityKind::Agent {
            consecutive_agent_turns += 1;
        } else {
            break;
        }
    }

    let last_human = relevant
        .iter()
        .rev()
        .find(|m| m.author_kind == relaypod_core::IdentityKind::Human)
        .map(|m| m.author_handle.clone());

    (consecutive_agent_turns, last_human, already_paused)
}

struct DispatchEnv<'a> {
    adapter: &'a dyn WatcherAdapter,
    client: &'a RelayPodClient,
    target: &'a ResolvedTarget,
    identity_id: &'a str,
    sessions: &'a SessionStore,
    ws_out_tx: &'a mpsc::UnboundedSender<WsClientMessage>,
    on_activity_change: &'a (dyn Fn(ActivityState) + Send + Sync),
}

enum DispatchStatus {
    Succeeded,
    Blocked,
    Failed(Option<String>),
    CircuitBreakerPaused,
}

async fn dispatch_unread_item(
    env: &DispatchEnv<'_>,
    handle: &str,
    item: &UnreadItem,
) -> DispatchStatus {
    let adapter = env.adapter;
    let client = env.client;
    let target = env.target;
    let identity_id = env.identity_id;
    let sessions = env.sessions;
    let ws_out_tx = env.ws_out_tx;
    let thread_id = item
        .parent_id
        .clone()
        .unwrap_or_else(|| item.message_id.clone());

    if let Some(limit) = target.max_consecutive_agent_turns {
        if item.parent_id.is_some() {
            match client.get_thread(&thread_id).await {
                Ok(thread) => {
                    let (turns, last_human, already_paused) =
                        analyze_agent_turns(&thread, &item.message_id);
                    if turns >= limit {
                        warn!(
                            handle = %handle,
                            thread_id = %thread_id,
                            unread_id = %item.id,
                            consecutive_agent_turns = turns,
                            limit = limit,
                            "Circuit breaker tripped: pausing automatic agent dispatch in thread"
                        );
                        if !already_paused {
                            let notice = match last_human {
                                Some(ref human) => format!(
                                    "@{human} {CIRCUIT_BREAKER_MARKER} after {turns} consecutive agent turns in this thread (limit: {limit}). Mention an agent to continue."
                                ),
                                None => format!(
                                    "{CIRCUIT_BREAKER_MARKER} after {turns} consecutive agent turns in this thread (limit: {limit}). Mention an agent to continue."
                                ),
                            };
                            if let Err(err) = client
                                .post_message(&item.channel_id, notice, Some(thread_id.clone()))
                                .await
                            {
                                warn!(
                                    handle = %handle,
                                    thread_id = %thread_id,
                                    error = %err,
                                    "Failed to post circuit-breaker pause notice"
                                );
                            }
                        }
                        if let Err(err) = client.ack_unread(&item.id).await {
                            warn!(
                                handle = %handle,
                                unread_id = %item.id,
                                error = %err,
                                "Failed to ack unread item after circuit breaker tripped"
                            );
                        }
                        return DispatchStatus::CircuitBreakerPaused;
                    }
                }
                Err(err) => {
                    warn!(
                        handle = %handle,
                        thread_id = %thread_id,
                        error = %err,
                        "Failed to inspect thread for circuit breaker; proceeding with dispatch"
                    );
                }
            }
        }
    }

    let _ = ws_out_tx.send(WsClientMessage::Activity {
        channel_id: item.channel_id.clone(),
        thread_id: Some(thread_id.clone()),
        active: true,
        state: ActivityState::Working,
        pane_id: None,
        detail: None,
    });

    let on_activity = |update: ActivityUpdate| {
        let _ = ws_out_tx.send(WsClientMessage::Activity {
            channel_id: item.channel_id.clone(),
            thread_id: Some(thread_id.clone()),
            active: true,
            state: update.state,
            pane_id: update.pane_id,
            detail: update.detail,
        });
        (env.on_activity_change)(update.state);
    };

    let ctx = DispatchContext {
        client,
        target,
        identity_id,
        handle,
        item,
        thread_id: &thread_id,
        sessions,
        on_activity: &on_activity,
    };

    let outcome = adapter.dispatch(ctx).await;

    let _ = ws_out_tx.send(WsClientMessage::Activity {
        channel_id: item.channel_id.clone(),
        thread_id: Some(thread_id.clone()),
        active: false,
        state: ActivityState::Working,
        pane_id: None,
        detail: None,
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
        if outcome.allow_auto_ack {
            DispatchStatus::Succeeded
        } else {
            DispatchStatus::Blocked
        }
    } else {
        DispatchStatus::Failed(outcome.detail)
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
    if let Some(path) = resolved_subprocess_path() {
        cmd.env("PATH", path);
    }
    for (k, v) in common_env_pairs(target, handle, item, thread_id, existing_session) {
        cmd.env(k, v);
    }
}

pub(crate) async fn build_agent_prompt_with_context(
    ctx: &DispatchContext<'_>,
    is_resume: bool,
) -> String {
    build_agent_prompt_with_fetchers(
        ctx.target,
        ctx.handle,
        ctx.item,
        ctx.thread_id,
        is_resume,
        || ctx.client.list_channels_including_archived(),
        || ctx.client.get_thread(ctx.thread_id),
    )
    .await
}

async fn build_agent_prompt_with_fetchers<FC, FCFut, FT, FTFut>(
    target: &ResolvedTarget,
    handle: &str,
    item: &UnreadItem,
    thread_id: &str,
    is_resume: bool,
    fetch_channels: FC,
    fetch_thread: FT,
) -> String
where
    FC: FnOnce() -> FCFut,
    FCFut: std::future::Future<Output = Result<Vec<Channel>>>,
    FT: FnOnce() -> FTFut,
    FTFut: std::future::Future<Output = Result<relaypod_core::ThreadView>>,
{
    let needs_channel = (!is_resume && target.prompt.is_none())
        || target
            .prompt
            .as_deref()
            .is_some_and(|p| p.contains("{{channel_description}}"));
    let needs_prior_thread = !is_resume && target.prompt.is_none() && item.parent_id.is_some();

    let resolve_channel = |res: Result<Vec<Channel>>| match res {
        Ok(channels) => channels.into_iter().find(|ch| ch.id == item.channel_id),
        Err(err) => {
            warn!(
                handle = %handle,
                channel_id = %item.channel_id,
                error = %err,
                "Failed to fetch channel context for new session; continuing without channel description"
            );
            None
        }
    };
    let resolve_thread = |res: Result<relaypod_core::ThreadView>| match res {
        Ok(thread) => format_prior_thread_context(&thread, &item.message_id),
        Err(err) => {
            warn!(
                handle = %handle,
                thread_id = %thread_id,
                error = %err,
                "Failed to fetch prior thread context for new session; continuing with single message"
            );
            None
        }
    };

    let (channel_info, prior_thread) = match (needs_channel, needs_prior_thread) {
        (true, true) => {
            let (ch_res, th_res) = tokio::join!(fetch_channels(), fetch_thread());
            (resolve_channel(ch_res), resolve_thread(th_res))
        }
        (true, false) => (resolve_channel(fetch_channels().await), None),
        (false, true) => (None, resolve_thread(fetch_thread().await)),
        (false, false) => (None, None),
    };

    build_agent_prompt(
        target,
        handle,
        item,
        thread_id,
        is_resume,
        channel_info.as_ref(),
        prior_thread.as_deref(),
    )
}

pub(crate) fn format_channel_context(item: &UnreadItem, channel: Option<&Channel>) -> String {
    let channel_name = channel
        .map(|ch| ch.name.as_str())
        .unwrap_or(item.channel_name.as_str());
    let description = channel
        .map(|ch| ch.description.trim())
        .filter(|desc| !desc.is_empty());

    match description {
        Some(desc) => format!("Channel: #{channel_name}\nChannel description: {desc}"),
        None => format!("Channel: #{channel_name}"),
    }
}

pub(crate) fn format_prior_thread_context(
    thread: &relaypod_core::ThreadView,
    current_message_id: &str,
) -> Option<String> {
    if thread.parent.id == current_message_id {
        return None;
    }
    let mut lines = vec![format!(
        "@{} (thread root): {}",
        thread.parent.author_handle, thread.parent.body
    )];
    for reply in &thread.replies {
        if reply.id == current_message_id {
            break;
        }
        lines.push(format!("@{}: {}", reply.author_handle, reply.body));
    }
    Some(lines.join("\n\n"))
}

pub(crate) fn build_agent_prompt(
    target: &ResolvedTarget,
    handle: &str,
    item: &UnreadItem,
    thread_id: &str,
    is_resume: bool,
    channel_info: Option<&Channel>,
    prior_thread_context: Option<&str>,
) -> String {
    let channel_name = channel_info
        .map(|ch| ch.name.as_str())
        .unwrap_or(item.channel_name.as_str());
    let channel_description = channel_info.map(|ch| ch.description.trim()).unwrap_or("");

    let base = if let Some(ref custom) = target.prompt {
        render_exec_template(
            custom,
            handle,
            item,
            thread_id,
            channel_name,
            channel_description,
        )
    } else if is_resume {
        format!(
            "Follow-up reply for @{} in RelayPod channel #{} (thread {}, unread {}) from @{}:\n\n{}\n\nRespond in-thread using RelayPod MCP `post_message` (with parent_id=\"{}\") and acknowledge unread item `{}` via `ack_unread`. Do not use interactive questionnaire/user-input tools; ask any clarifying questions in-thread via `post_message` instead. Remember: other agents only wake up when explicitly @mentioned.",
            handle, channel_name, thread_id, item.id, item.author_handle, item.body, thread_id, item.id
        )
    } else {
        let channel_context = format_channel_context(item, channel_info);
        if let Some(history) = prior_thread_context {
            format!(
                "You are @{}.\n\n{}\n\nPrior messages in RelayPod channel #{} (thread {}):\n\n{}\n\n---\n\nLatest message (unread {}) from @{}:\n\n{}\n\nReply in-thread using RelayPod MCP `post_message` (with parent_id=\"{}\") and acknowledge unread item `{}` via `ack_unread`. Do not use interactive questionnaire/user-input tools; ask any clarifying questions in-thread via `post_message` instead. Note: other agents in RelayPod only wake up when explicitly @mentioned (omit agent @mentions when no further agent action is needed).",
                handle, channel_context, channel_name, thread_id, history, item.id, item.author_handle, item.body, thread_id, item.id
            )
        } else {
            format!(
                "You are @{}.\n\n{}\n\nNew RelayPod message in #{} (thread {}, unread {}) from @{}:\n\n{}\n\nIf you need prior context, call RelayPod MCP `read_thread` on `{}`. Reply in-thread using `post_message` (with parent_id=\"{}\") and acknowledge unread item `{}` via `ack_unread`. Do not use interactive questionnaire/user-input tools; ask any clarifying questions in-thread via `post_message` instead. Note: other agents in RelayPod only wake up when explicitly @mentioned (omit agent @mentions when no further agent action is needed).",
                handle, channel_context, channel_name, thread_id, item.id, item.author_handle, item.body, thread_id, thread_id, item.id
            )
        }
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

pub(crate) fn shell_escape_posix(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Renders a template for `sh -c` execution, POSIX single-quote escaping all substituted fields
/// to prevent shell command injection via message bodies or metadata.
pub(crate) fn render_shell_exec_template(
    template: &str,
    handle: &str,
    item: &UnreadItem,
    thread_id: &str,
) -> String {
    template
        .replace("{{handle}}", &shell_escape_posix(handle))
        .replace("{{unread_id}}", &shell_escape_posix(&item.id))
        .replace("{{message_id}}", &shell_escape_posix(&item.message_id))
        .replace("{{thread_id}}", &shell_escape_posix(thread_id))
        .replace("{{channel_id}}", &shell_escape_posix(&item.channel_id))
        .replace("{{channel}}", &shell_escape_posix(&item.channel_name))
        .replace("{{author}}", &shell_escape_posix(&item.author_handle))
        .replace("{{reason}}", &shell_escape_posix(&item.reason.to_string()))
        .replace("{{body}}", &shell_escape_posix(&item.body))
}

pub(crate) fn render_exec_template(
    template: &str,
    handle: &str,
    item: &UnreadItem,
    thread_id: &str,
    channel_name: &str,
    channel_description: &str,
) -> String {
    template
        .replace("{{handle}}", handle)
        .replace("{{unread_id}}", &item.id)
        .replace("{{message_id}}", &item.message_id)
        .replace("{{thread_id}}", thread_id)
        .replace("{{channel_id}}", &item.channel_id)
        .replace("{{channel}}", channel_name)
        .replace("{{channel_description}}", channel_description)
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
    fn shell_exec_template_escapes_untrusted_body() {
        let item = UnreadItem {
            id: "u1".into(),
            identity_id: "a1".into(),
            message_id: "m1".into(),
            channel_id: "c1".into(),
            channel_name: "general".into(),
            parent_id: None,
            author_handle: "alice".into(),
            body: "hi'; rm -rf / #".into(),
            reason: relaypod_core::UnreadReason::Mention,
            acked_at: None,
            created_at: 0,
        };
        let rendered = render_shell_exec_template("echo {{body}}", "codex", &item, "m1");
        assert_eq!(rendered, "echo 'hi'\\''; rm -rf / #'");
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
        store.set("codex", "t2", "herdr:w1:p2".into());
        store.set("codex", "t3", "herdr:w1:p3".into());
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

    #[test]
    fn formats_prior_thread_context_up_to_current_message() {
        let mk_msg =
            |id: &str, author: &str, body: &str, parent: Option<&str>| relaypod_core::Message {
                id: id.into(),
                channel_id: "c1".into(),
                parent_id: parent.map(ToString::to_string),
                author_id: author.into(),
                author_handle: author.into(),
                author_kind: relaypod_core::IdentityKind::Human,
                body: body.into(),
                created_at: 0,
                reply_count: 0,
            };
        let thread = relaypod_core::ThreadView {
            parent: mk_msg("m1", "alice", "Let's fix issue #4", None),
            replies: vec![
                mk_msg("m2", "codex-local", "Working on it", Some("m1")),
                mk_msg("m3", "alice", "@claude can you review?", Some("m1")),
            ],
        };

        let ctx = format_prior_thread_context(&thread, "m3").expect("should have prior history");
        assert!(ctx.contains("@alice (thread root): Let's fix issue #4"));
        assert!(ctx.contains("@codex-local: Working on it"));
        assert!(!ctx.contains("@claude can you review?"));

        // Top-level message itself has no prior context
        assert_eq!(format_prior_thread_context(&thread, "m1"), None);
    }

    #[test]
    fn analyzes_consecutive_agent_turns_and_human_resets() {
        let mk_msg = |id: &str, author: &str, kind: relaypod_core::IdentityKind, body: &str| {
            relaypod_core::Message {
                id: id.into(),
                channel_id: "c1".into(),
                parent_id: if id == "m1" { None } else { Some("m1".into()) },
                author_id: author.into(),
                author_handle: author.into(),
                author_kind: kind,
                body: body.into(),
                created_at: 0,
                reply_count: 0,
            }
        };
        let thread = relaypod_core::ThreadView {
            parent: mk_msg("m1", "alice", relaypod_core::IdentityKind::Human, "go"),
            replies: vec![
                mk_msg("m2", "agy", relaypod_core::IdentityKind::Agent, "opened PR"),
                mk_msg("m3", "codex", relaypod_core::IdentityKind::Agent, "nit 1"),
                mk_msg("m4", "agy", relaypod_core::IdentityKind::Agent, "fixed 1"),
                mk_msg(
                    "m5",
                    "alice",
                    relaypod_core::IdentityKind::Human,
                    "keep going",
                ),
                mk_msg("m6", "codex", relaypod_core::IdentityKind::Agent, "nit 2"),
                mk_msg("m7", "agy", relaypod_core::IdentityKind::Agent, "fixed 2"),
            ],
        };

        // At m4: 3 consecutive agent turns since m1
        assert_eq!(
            analyze_agent_turns(&thread, "m4"),
            (3, Some("alice".into()), false)
        );
        // At m5 (human): resets to 0
        assert_eq!(
            analyze_agent_turns(&thread, "m5"),
            (0, Some("alice".into()), false)
        );
        // At m7: 2 consecutive agent turns since m5
        assert_eq!(
            analyze_agent_turns(&thread, "m7"),
            (2, Some("alice".into()), false)
        );

        let mut paused_thread = thread;
        paused_thread.replies.push(mk_msg(
            "m8",
            "codex",
            relaypod_core::IdentityKind::Agent,
            "@alice Paused automatic agent collaboration after 6 consecutive agent turns",
        ));
        let (_, _, already_paused) = analyze_agent_turns(&paused_thread, "m7");
        assert!(already_paused);
    }

    fn sample_target(prompt: Option<&str>) -> ResolvedTarget {
        ResolvedTarget {
            label: "codex".into(),
            runner: RunnerKind::Herdr,
            agent_type: Some(AgentType::Codex),
            url: "http://127.0.0.1:8787".into(),
            token: "rp_test".into(),
            exec: String::new(),
            args: None,
            prompt: prompt.map(ToString::to_string),
            instructions_file: None,
            cwd: None,
            global_max_sessions: None,
            max_sessions: None,
            max_consecutive_agent_turns: None,
            auto_ack: false,
            drain_on_start: true,
            cooldown_secs: 0,
            env: BTreeMap::new(),
        }
    }

    fn sample_unread_item(parent_id: Option<&str>) -> UnreadItem {
        UnreadItem {
            id: "u1".into(),
            identity_id: "a1".into(),
            message_id: "m2".into(),
            channel_id: "c1".into(),
            channel_name: "backend".into(),
            parent_id: parent_id.map(ToString::to_string),
            author_handle: "alice".into(),
            body: "@codex please review the auth flow".into(),
            reason: relaypod_core::UnreadReason::Mention,
            acked_at: None,
            created_at: 0,
        }
    }

    fn sample_channel(name: &str, description: &str) -> Channel {
        Channel {
            id: "c1".into(),
            name: name.into(),
            description: description.into(),
            created_at: 0,
            created_by: Some("h1".into()),
            archived: false,
            archived_at: None,
        }
    }

    #[test]
    fn new_session_prompt_includes_channel_name_and_description() {
        let target = sample_target(None);
        let item = sample_unread_item(None);
        let channel = sample_channel("backend", "  API & auth service (repo: ~/projects/api)  ");

        let prompt = build_agent_prompt(&target, "codex", &item, "m2", false, Some(&channel), None);
        assert!(prompt.contains(
            "Channel: #backend\nChannel description: API & auth service (repo: ~/projects/api)"
        ));
        assert!(
            prompt.contains("New RelayPod message in #backend (thread m2, unread u1) from @alice:")
        );
        assert!(prompt.contains("@codex please review the auth flow"));

        let prompt_with_history = build_agent_prompt(
            &target,
            "codex",
            &item,
            "m1",
            false,
            Some(&channel),
            Some("@alice (thread root): Let's audit auth"),
        );
        assert!(prompt_with_history.contains(
            "Channel: #backend\nChannel description: API & auth service (repo: ~/projects/api)"
        ));
        assert!(prompt_with_history
            .contains("Prior messages in RelayPod channel #backend (thread m1):"));
        assert!(prompt_with_history.contains("@alice (thread root): Let's audit auth"));
        assert!(prompt_with_history.contains("Latest message (unread u1) from @alice:"));
    }

    #[test]
    fn new_session_prompt_omits_blank_channel_description_and_handles_missing_channel() {
        let target = sample_target(None);
        let item = sample_unread_item(None);
        let blank_channel = sample_channel("backend-v2", "   \n  ");

        let prompt_blank = build_agent_prompt(
            &target,
            "codex",
            &item,
            "m2",
            false,
            Some(&blank_channel),
            None,
        );
        assert!(
            prompt_blank.contains("Channel: #backend-v2\n\nNew RelayPod message in #backend-v2")
        );
        assert!(!prompt_blank.contains("Channel description:"));

        let prompt_missing = build_agent_prompt(&target, "codex", &item, "m2", false, None, None);
        assert!(prompt_missing.contains("Channel: #backend\n\nNew RelayPod message in #backend"));
        assert!(!prompt_missing.contains("Channel description:"));
    }

    #[test]
    fn resume_prompt_skips_channel_context_block_and_custom_prompt_supports_placeholder() {
        let target = sample_target(None);
        let item = sample_unread_item(Some("m1"));
        let channel = sample_channel("backend", "API & auth service");

        let resume_prompt =
            build_agent_prompt(&target, "codex", &item, "m1", true, Some(&channel), None);
        assert!(
            resume_prompt.starts_with("Follow-up reply for @codex in RelayPod channel #backend")
        );
        assert!(!resume_prompt.contains("Channel description:"));

        let custom_target =
            sample_target(Some("In #{{channel}} ({{channel_description}}): {{body}}"));
        let custom_prompt = build_agent_prompt(
            &custom_target,
            "codex",
            &item,
            "m1",
            false,
            Some(&channel),
            None,
        );
        assert_eq!(
            custom_prompt,
            "In #backend (API & auth service): @codex please review the auth flow"
        );
    }

    #[tokio::test]
    async fn build_agent_prompt_with_fetchers_injects_channel_and_thread_for_new_session() {
        let target = sample_target(None);
        let item = sample_unread_item(Some("m1"));
        let channels = vec![
            Channel {
                id: "c-other".into(),
                name: "general".into(),
                description: "General chat".into(),
                created_at: 0,
                created_by: Some("h1".into()),
                archived: false,
                archived_at: None,
            },
            sample_channel("backend", "Backend API repo at ~/projects/api"),
        ];
        let thread = relaypod_core::ThreadView {
            parent: relaypod_core::Message {
                id: "m1".into(),
                channel_id: "c1".into(),
                parent_id: None,
                author_id: "h1".into(),
                author_handle: "alice".into(),
                author_kind: relaypod_core::IdentityKind::Human,
                body: "Investigate login timeout".into(),
                created_at: 1,
                reply_count: 1,
            },
            replies: vec![relaypod_core::Message {
                id: "m2".into(),
                channel_id: "c1".into(),
                parent_id: Some("m1".into()),
                author_id: "h1".into(),
                author_handle: "alice".into(),
                author_kind: relaypod_core::IdentityKind::Human,
                body: "@codex please review the auth flow".into(),
                created_at: 2,
                reply_count: 0,
            }],
        };

        let prompt = build_agent_prompt_with_fetchers(
            &target,
            "codex",
            &item,
            "m1",
            false,
            || async { Ok(channels) },
            || async { Ok(thread) },
        )
        .await;

        assert!(prompt.contains(
            "Channel: #backend\nChannel description: Backend API repo at ~/projects/api"
        ));
        assert!(prompt.contains("@alice (thread root): Investigate login timeout"));
        assert!(prompt.contains("@codex please review the auth flow"));
        assert!(!prompt.contains("General chat"));
    }

    #[tokio::test]
    async fn build_agent_prompt_with_fetchers_skips_fetches_on_resume_and_handles_errors() {
        let target = sample_target(None);
        let item = sample_unread_item(Some("m1"));

        // On resume, neither channel nor thread fetcher should be invoked.
        let resume_prompt = build_agent_prompt_with_fetchers(
            &target,
            "codex",
            &item,
            "m1",
            true,
            || async {
                panic!("channels should not be fetched on resume");
                #[allow(unreachable_code)]
                Ok(Vec::new())
            },
            || async {
                panic!("thread should not be fetched on resume");
                #[allow(unreachable_code)]
                Err(anyhow!("unreachable"))
            },
        )
        .await;
        assert!(
            resume_prompt.starts_with("Follow-up reply for @codex in RelayPod channel #backend")
        );
        assert!(!resume_prompt.contains("Channel description:"));

        // On new session, fetch errors degrade gracefully without failing prompt construction.
        let top_level_item = sample_unread_item(None);
        let fallback_prompt = build_agent_prompt_with_fetchers(
            &target,
            "codex",
            &top_level_item,
            "m2",
            false,
            || async { Err(anyhow!("server unavailable")) },
            || async {
                panic!("thread should not be fetched for top-level message");
                #[allow(unreachable_code)]
                Err(anyhow!("unreachable"))
            },
        )
        .await;
        assert!(fallback_prompt.contains("Channel: #backend\n\nNew RelayPod message in #backend"));
        assert!(!fallback_prompt.contains("Channel description:"));
        assert!(fallback_prompt.contains("@codex please review the auth flow"));
    }
}
