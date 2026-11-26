use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::watch as tokio_watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use super::{
    default_config_candidates, resolve_config_path, resolve_targets, watch_single_target,
    SessionStore, WatchOptions,
};

const MAX_RECENT_LOGS: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetPhase {
    Connecting,
    Connected,
    Working { channel: String },
    Reconnecting,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedTargetState {
    pub token_name: String,
    pub identity_id: Option<String>,
    pub handle: Option<String>,
    pub runner: Option<String>,
    pub harness: Option<String>,
    pub phase: TargetPhase,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatcherSnapshot {
    pub running: bool,
    pub config_path: Option<PathBuf>,
    pub targets: Vec<WatchedTargetState>,
    pub last_error: Option<String>,
    pub recent_logs: Vec<String>,
}

impl WatcherSnapshot {
    pub fn connected_count(&self) -> usize {
        self.targets
            .iter()
            .filter(|t| {
                matches!(
                    t.phase,
                    TargetPhase::Connected | TargetPhase::Working { .. }
                )
            })
            .count()
    }

    pub fn working_count(&self) -> usize {
        self.targets
            .iter()
            .filter(|t| matches!(t.phase, TargetPhase::Working { .. }))
            .count()
    }

    pub fn error_count(&self) -> usize {
        self.targets
            .iter()
            .filter(|t| matches!(t.phase, TargetPhase::Error(_)))
            .count()
    }

    pub fn token_name_for_identity(&self, identity_id: &str) -> Option<&str> {
        self.targets
            .iter()
            .find(|t| t.identity_id.as_deref() == Some(identity_id))
            .map(|t| t.token_name.as_str())
    }
}

struct SharedState {
    snapshot: WatcherSnapshot,
    logs: VecDeque<String>,
}

type SnapshotCallback = Arc<dyn Fn(WatcherSnapshot) + Send + Sync>;

#[derive(Clone)]
pub(crate) struct WatcherReporter {
    state: Arc<Mutex<SharedState>>,
    on_update: SnapshotCallback,
}

impl WatcherReporter {
    fn new(
        config_path: Option<PathBuf>,
        token_names: Vec<String>,
        on_update: SnapshotCallback,
    ) -> Self {
        let targets = token_names
            .into_iter()
            .map(|token_name| WatchedTargetState {
                token_name,
                identity_id: None,
                handle: None,
                runner: None,
                harness: None,
                phase: TargetPhase::Connecting,
            })
            .collect();
        let snapshot = WatcherSnapshot {
            running: true,
            config_path,
            targets,
            last_error: None,
            recent_logs: Vec::new(),
        };
        Self {
            state: Arc::new(Mutex::new(SharedState {
                snapshot,
                logs: VecDeque::with_capacity(MAX_RECENT_LOGS),
            })),
            on_update,
        }
    }

    fn emit(&self) {
        let snap = {
            let mut guard = self.state.lock().expect("watcher state lock poisoned");
            guard.snapshot.recent_logs = guard.logs.iter().cloned().collect();
            guard.snapshot.clone()
        };
        (self.on_update)(snap);
    }

    pub(crate) fn push_log(&self, line: String) {
        {
            let mut guard = self.state.lock().expect("watcher state lock poisoned");
            if guard.logs.len() >= MAX_RECENT_LOGS {
                guard.logs.pop_front();
            }
            guard.logs.push_back(line);
        }
        self.emit();
    }

    pub(crate) fn update_target_meta(
        &self,
        token_name: &str,
        identity_id: Option<String>,
        handle: Option<String>,
        runner: Option<String>,
        harness: Option<String>,
    ) {
        {
            let mut guard = self.state.lock().expect("watcher state lock poisoned");
            if let Some(t) = guard
                .snapshot
                .targets
                .iter_mut()
                .find(|t| t.token_name == token_name)
            {
                if identity_id.is_some() {
                    t.identity_id = identity_id;
                }
                if handle.is_some() {
                    t.handle = handle;
                }
                t.runner = runner;
                t.harness = harness;
            }
        }
        self.emit();
    }

    pub(crate) fn set_target_phase(&self, token_name: &str, phase: TargetPhase) {
        {
            let mut guard = self.state.lock().expect("watcher state lock poisoned");
            if let Some(t) = guard
                .snapshot
                .targets
                .iter_mut()
                .find(|t| t.token_name == token_name)
            {
                t.phase = phase;
            }
        }
        self.emit();
    }

    pub(crate) fn set_target_ws_live(&self, token_name: &str, live: bool) {
        {
            let mut guard = self.state.lock().expect("watcher state lock poisoned");
            if let Some(t) = guard
                .snapshot
                .targets
                .iter_mut()
                .find(|t| t.token_name == token_name)
            {
                if live {
                    if !matches!(t.phase, TargetPhase::Working { .. }) {
                        t.phase = TargetPhase::Connected;
                    }
                } else {
                    t.phase = TargetPhase::Reconnecting;
                }
            }
        }
        self.emit();
    }

    pub(crate) fn snapshot(&self) -> WatcherSnapshot {
        let mut guard = self.state.lock().expect("watcher state lock poisoned");
        guard.snapshot.recent_logs = guard.logs.iter().cloned().collect();
        guard.snapshot.clone()
    }
}

/// Manages the lifecycle of embedded watcher tasks inside a Tokio runtime (e.g. `relaypod-ui`).
pub struct WatcherSupervisor {
    on_update: SnapshotCallback,
    shutdown_tx: Option<tokio_watch::Sender<bool>>,
    tasks: Vec<JoinHandle<()>>,
    reporter: Option<WatcherReporter>,
    last_snapshot: WatcherSnapshot,
}

impl WatcherSupervisor {
    pub fn new<F>(on_update: F) -> Self
    where
        F: Fn(WatcherSnapshot) + Send + Sync + 'static,
    {
        Self {
            on_update: Arc::new(on_update),
            shutdown_tx: None,
            tasks: Vec::new(),
            reporter: None,
            last_snapshot: WatcherSnapshot::default(),
        }
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        for task in self.tasks.drain(..) {
            task.abort();
        }
        let mut snap = self
            .reporter
            .take()
            .map(|r| r.snapshot())
            .unwrap_or_else(|| self.last_snapshot.clone());
        snap.running = false;
        snap.targets.clear();
        snap.last_error = None;
        if !snap.recent_logs.is_empty() {
            if snap.recent_logs.len() >= MAX_RECENT_LOGS {
                snap.recent_logs.remove(0);
            }
            snap.recent_logs.push("Watcher stopped".to_string());
        }
        self.last_snapshot = snap.clone();
        (self.on_update)(snap);
    }

    /// Starts (or cleanly reloads) watching all `watcher.agent_tokens` configured in `config.yaml`.
    /// Missing config files or empty `agent_tokens` maps leave the watcher idle without error.
    pub fn start_or_reload(&mut self, default_url: String, config_path: Option<PathBuf>) {
        // Stop any existing run silently before starting fresh.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        for task in self.tasks.drain(..) {
            task.abort();
        }
        let prior_logs = self
            .reporter
            .take()
            .map(|r| r.snapshot().recent_logs)
            .unwrap_or_else(|| self.last_snapshot.recent_logs.clone());

        let resolved_path = match resolve_config_path(config_path.as_deref()) {
            Ok(Some(path)) => Some(path),
            Ok(None) => {
                let default_path = default_config_candidates().into_iter().next();
                let snap = WatcherSnapshot {
                    running: false,
                    config_path: default_path,
                    targets: Vec::new(),
                    last_error: None,
                    recent_logs: prior_logs,
                };
                self.last_snapshot = snap.clone();
                (self.on_update)(snap);
                return;
            }
            Err(err) => {
                let mut logs = prior_logs;
                push_capped(&mut logs, format!("Config error: {err}"));
                let snap = WatcherSnapshot {
                    running: false,
                    config_path,
                    targets: Vec::new(),
                    last_error: Some(err.to_string()),
                    recent_logs: logs,
                };
                self.last_snapshot = snap.clone();
                (self.on_update)(snap);
                return;
            }
        };

        let opts = WatchOptions {
            default_url,
            default_token: None,
            config_path: resolved_path.clone(),
            handle_filter: None,
            exec_override: None,
            cwd_override: None,
            auto_ack_override: None,
            no_drain: false,
        };

        let targets = match resolve_targets(&opts) {
            Ok(targets) => targets,
            Err(err) => {
                let mut logs = prior_logs;
                push_capped(&mut logs, format!("Failed to load watcher config: {err}"));
                let snap = WatcherSnapshot {
                    running: false,
                    config_path: resolved_path,
                    targets: Vec::new(),
                    last_error: Some(err.to_string()),
                    recent_logs: logs,
                };
                self.last_snapshot = snap.clone();
                (self.on_update)(snap);
                return;
            }
        };

        if targets.is_empty() {
            let snap = WatcherSnapshot {
                running: false,
                config_path: resolved_path,
                targets: Vec::new(),
                last_error: None,
                recent_logs: prior_logs,
            };
            self.last_snapshot = snap.clone();
            (self.on_update)(snap);
            return;
        }

        let token_names: Vec<String> = targets.iter().map(|t| t.label.clone()).collect();
        let reporter =
            WatcherReporter::new(resolved_path, token_names, Arc::clone(&self.on_update));
        {
            let mut guard = reporter.state.lock().expect("watcher state lock poisoned");
            for line in prior_logs {
                if guard.logs.len() >= MAX_RECENT_LOGS {
                    guard.logs.pop_front();
                }
                guard.logs.push_back(line);
            }
        }
        reporter.push_log(format!(
            "Starting local watcher for {} agent(s)",
            targets.len()
        ));

        let (shutdown_tx, shutdown_rx) = tokio_watch::channel(false);
        let sessions = SessionStore::load_default();

        info!(
            "Starting embedded RelayPod watcher for {} target(s)",
            targets.len()
        );

        let mut tasks = Vec::with_capacity(targets.len());
        for target in targets {
            let sessions_clone = sessions.clone();
            let rep_clone = reporter.clone();
            let rx_clone = shutdown_rx.clone();
            let label = target.label.clone();
            tasks.push(tokio::spawn(async move {
                if let Err(err) =
                    watch_single_target(target, sessions_clone, Some(rep_clone.clone()), rx_clone)
                        .await
                {
                    let msg = format!("{err:#}");
                    error!(target = %label, error = %msg, "embedded watcher exited with error");
                    rep_clone.set_target_phase(&label, TargetPhase::Error(msg.clone()));
                    rep_clone.push_log(format!("[{label}] error: {msg}"));
                }
            }));
        }

        self.shutdown_tx = Some(shutdown_tx);
        self.tasks = tasks;
        self.last_snapshot = reporter.snapshot();
        self.reporter = Some(reporter);
    }
}

impl Drop for WatcherSupervisor {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

fn push_capped(logs: &mut Vec<String>, line: String) {
    if logs.len() >= MAX_RECENT_LOGS {
        let excess = logs.len() + 1 - MAX_RECENT_LOGS;
        logs.drain(..excess);
    }
    logs.push(line);
}

/// Returns an enriched `PATH` suitable for spawning CLI tools (`herdr`, `claude`, `codex`, etc.)
/// even when RelayPod is launched as a GUI app bundle (macOS Finder/Spotlight) with a minimal `PATH`.
pub(crate) fn resolved_subprocess_path() -> Option<&'static str> {
    static PATH_CACHE: OnceLock<Option<String>> = OnceLock::new();
    PATH_CACHE.get_or_init(compute_subprocess_path).as_deref()
}

/// Resolves a bare executable name (e.g. `"herdr"`, `"claude"`) against [`resolved_subprocess_path`]
/// so `Command::new(...)` finds user-installed tools even when the parent process's `PATH` is minimal.
pub(crate) fn resolve_program(program: &str) -> PathBuf {
    let candidate = PathBuf::from(program);
    if candidate.is_absolute() || program.contains(std::path::MAIN_SEPARATOR) {
        return candidate;
    }

    if let Some(search_path) = resolved_subprocess_path() {
        for dir in std::env::split_paths(search_path) {
            let full = dir.join(program);
            if is_executable_file(&full) {
                return full;
            }
        }
    }

    candidate
}

#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
    path.is_file()
}

fn compute_subprocess_path() -> Option<String> {
    let current = std::env::var("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&current)
        .filter(|p| !p.as_os_str().is_empty())
        .collect();

    // When launched outside a terminal (e.g. macOS `.app` bundle), probe the user's login shell
    // once so version managers (nvm, volta, mise, pyenv, cargo, homebrew) are on PATH.
    #[cfg(unix)]
    if std::env::var_os("TERM").is_none() {
        if let Some(shell_path) = probe_login_shell_path() {
            for dir in std::env::split_paths(&shell_path) {
                if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
                    dirs.push(dir);
                }
            }
        }
    }

    // Ensure common user/package-manager bin directories and the directory next to our own binary
    // are always searchable when they exist on disk.
    let mut fallbacks: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            fallbacks.push(parent.to_path_buf());
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let h = PathBuf::from(home);
        fallbacks.push(h.join(".local").join("bin"));
        fallbacks.push(h.join(".cargo").join("bin"));
        fallbacks.push(h.join(".bun").join("bin"));
        fallbacks.push(h.join(".volta").join("bin"));
        fallbacks.push(h.join("bin"));
    }
    #[cfg(unix)]
    {
        fallbacks.push(PathBuf::from("/opt/homebrew/bin"));
        fallbacks.push(PathBuf::from("/opt/homebrew/sbin"));
        fallbacks.push(PathBuf::from("/usr/local/bin"));
    }

    for dir in fallbacks {
        if dir.is_dir() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }

    std::env::join_paths(dirs)
        .ok()
        .and_then(|os| os.into_string().ok())
}

#[cfg(unix)]
fn probe_login_shell_path() -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_string());

    let mut child = match Command::new(&shell)
        .args(["-lc", r#"printf '%s' "$PATH""#])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(shell = %shell, error = %err, "failed to spawn login shell for PATH discovery");
            return None;
        }
    };

    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut buf = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.read_to_string(&mut buf);
                }
                let trimmed = buf.trim();
                return (!trimmed.is_empty()).then(|| trimmed.to_string());
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    warn!(shell = %shell, "timed out probing login shell PATH");
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }
}
