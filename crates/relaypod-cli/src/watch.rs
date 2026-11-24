use anyhow::{anyhow, Context, Result};
use relaypod_client::{run_live_ws_loop, RelayPodClient, DEFAULT_PING_INTERVAL};
use relaypod_core::{UnreadItem, WsEvent};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchConfig {
    pub url: Option<String>,
    pub handles: BTreeMap<String, HandleConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandleConfig {
    pub url: Option<String>,
    pub token: Option<String>,
    pub exec: String,
    pub cwd: Option<String>,
    pub auto_ack: bool,
    pub drain_on_start: bool,
    pub cooldown_secs: u64,
    pub env: BTreeMap<String, String>,
}

impl Default for HandleConfig {
    fn default() -> Self {
        Self {
            url: None,
            token: None,
            exec: String::new(),
            cwd: None,
            auto_ack: false,
            drain_on_start: true,
            cooldown_secs: 0,
            env: BTreeMap::new(),
        }
    }
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
struct ResolvedTarget {
    label: String,
    url: String,
    token: String,
    exec: String,
    cwd: Option<String>,
    auto_ack: bool,
    drain_on_start: bool,
    cooldown_secs: u64,
    env: BTreeMap<String, String>,
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

    let mut join_handles = Vec::with_capacity(targets.len());
    for target in targets {
        join_handles.push(tokio::spawn(async move {
            if let Err(err) = watch_single_target(target.clone()).await {
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

    // Locate config file if explicitly provided or present in default locations
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
                url: opts.default_url.clone(),
                token,
                exec: exec_cmd.clone(),
                cwd: opts.cwd_override.as_deref().map(expand_tilde),
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

        let exec = opts
            .exec_override
            .clone()
            .unwrap_or(h.exec);
        if exec.trim().is_empty() {
            return Err(anyhow!("handle '@{name}' has an empty `exec` command"));
        }

        targets.push(ResolvedTarget {
            label: format!("@{name}"),
            url,
            token,
            exec,
            cwd: opts.cwd_override.clone().or(h.cwd).as_deref().map(expand_tilde),
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

async fn watch_single_target(target: ResolvedTarget) -> Result<()> {
    let client = RelayPodClient::new(target.url.clone(), target.token.clone());
    let me = client
        .me()
        .await
        .with_context(|| format!("failed to authenticate {} at {}", target.label, target.url))?;

    info!(
        handle = %me.handle,
        kind = %me.kind,
        url = %target.url,
        auto_ack = target.auto_ack,
        "Authenticated watcher"
    );

    let (tx, mut rx) = mpsc::unbounded_channel::<UnreadItem>();

    // Sequential worker so one handle never runs overlapping execs at the same time
    let worker_client = client.clone();
    let worker_target = target.clone();
    let worker_handle = me.handle.clone();
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            run_exec_for_item(&worker_client, &worker_target, &worker_handle, &item).await;
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

    run_live_ws_loop(
        ws_url,
        DEFAULT_PING_INTERVAL,
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

async fn run_exec_for_item(
    client: &RelayPodClient,
    target: &ResolvedTarget,
    handle: &str,
    item: &UnreadItem,
) {
    let thread_id = item
        .parent_id
        .clone()
        .unwrap_or_else(|| item.message_id.clone());
    let rendered_cmd = render_exec_template(&target.exec, handle, item, &thread_id);

    info!(
        handle = %handle,
        unread_id = %item.id,
        channel = %item.channel_name,
        "Executing watcher command"
    );

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&rendered_cmd);
    if let Some(ref cwd) = target.cwd {
        cmd.current_dir(cwd);
    }

    cmd.env("RELAYPOD_URL", &target.url)
        .env("RELAYPOD_TOKEN", &target.token)
        .env("RELAYPOD_HANDLE", handle)
        .env("RELAYPOD_UNREAD_ID", &item.id)
        .env("RELAYPOD_MESSAGE_ID", &item.message_id)
        .env("RELAYPOD_THREAD_ID", &thread_id)
        .env("RELAYPOD_CHANNEL_ID", &item.channel_id)
        .env("RELAYPOD_CHANNEL", &item.channel_name)
        .env("RELAYPOD_AUTHOR", &item.author_handle)
        .env("RELAYPOD_REASON", item.reason.to_string())
        .env("RELAYPOD_BODY", &item.body);

    for (k, v) in &target.env {
        cmd.env(k, v);
    }

    match cmd.status().await {
        Ok(status) if status.success() => {
            info!(handle = %handle, unread_id = %item.id, "Watcher command succeeded");
            if target.auto_ack {
                match client.ack_unread(&item.id).await {
                    Ok(()) => info!(handle = %handle, unread_id = %item.id, "Auto-acked unread item"),
                    Err(err) => warn!(
                        handle = %handle,
                        unread_id = %item.id,
                        error = %err,
                        "Auto-ack failed (item may already be acked by the agent)"
                    ),
                }
            }
        }
        Ok(status) => {
            warn!(
                handle = %handle,
                unread_id = %item.id,
                code = ?status.code(),
                "Watcher command exited non-zero"
            );
        }
        Err(err) => {
            error!(
                handle = %handle,
                unread_id = %item.id,
                error = %err,
                "Failed to spawn watcher command"
            );
        }
    }
}

fn render_exec_template(template: &str, handle: &str, item: &UnreadItem, thread_id: &str) -> String {
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

pub fn load_config_file(path: &Path) -> Result<WatchConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    parse_yaml_config(&raw)
}

/// Lightweight, dependency-free YAML parser for `.relaypod.yaml`.
/// Supports top-level `url`, `handles:` map, per-handle settings, nested `env:` map,
/// single/double quoted strings, `#` comments, and `${ENV_VAR}` expansion.
pub fn parse_yaml_config(input: &str) -> Result<WatchConfig> {
    let mut cfg = WatchConfig::default();

    #[derive(Debug, PartialEq, Eq)]
    enum Section {
        Root,
        Handles,
        Handle(String, usize),
        HandleEnv(String, usize),
    }

    let mut section = Section::Root;

    for (line_idx, raw_line) in input.lines().enumerate() {
        let line_no = line_idx + 1;
        let stripped = strip_yaml_comment(raw_line);
        if stripped.trim().is_empty() {
            continue;
        }

        let indent = stripped.bytes().take_while(|&b| b == b' ').count();
        let trimmed = stripped.trim();

        // Unwind sections based on indentation
        match &section {
            Section::HandleEnv(handle, env_indent) if indent <= *env_indent => {
                if indent == 0 {
                    section = Section::Root;
                } else {
                    section = Section::Handle(handle.clone(), env_indent.saturating_sub(2));
                }
            }
            _ => {}
        }
        match &section {
            Section::Handle(_, handle_indent) if indent <= *handle_indent => {
                if indent == 0 {
                    section = Section::Root;
                } else {
                    section = Section::Handles;
                }
            }
            Section::Handles if indent == 0 => {
                section = Section::Root;
            }
            _ => {}
        }

        let (key, raw_val) = split_key_value(trimmed)
            .ok_or_else(|| anyhow!("invalid YAML at line {line_no}: expected `key: value`"))?;
        let key = unquote_yaml(key);
        let val = raw_val.trim();

        match &section {
            Section::Root => match key.as_str() {
                "url" => {
                    cfg.url = Some(expand_env_vars(&unquote_yaml(val)));
                }
                "handles" => {
                    if !val.is_empty() {
                        return Err(anyhow!(
                            "line {line_no}: `handles:` must be followed by indented handle entries"
                        ));
                    }
                    section = Section::Handles;
                }
                other => {
                    return Err(anyhow!("line {line_no}: unknown top-level key `{other}`"));
                }
            },
            Section::Handles => {
                let handle_name = key.trim_start_matches('@').to_string();
                cfg.handles.entry(handle_name.clone()).or_default();
                section = Section::Handle(handle_name, indent);
            }
            Section::Handle(handle_name, _) => {
                let handle_name = handle_name.clone();
                let entry = cfg
                    .handles
                    .get_mut(&handle_name)
                    .expect("handle entry exists");

                match key.as_str() {
                    "url" => entry.url = Some(expand_env_vars(&unquote_yaml(val))),
                    "token" => {
                        let expanded = expand_env_vars(&unquote_yaml(val));
                        if !expanded.is_empty() {
                            entry.token = Some(expanded);
                        }
                    }
                    "exec" => entry.exec = expand_env_vars(&unquote_yaml(val)),
                    "cwd" => entry.cwd = Some(expand_env_vars(&unquote_yaml(val))),
                    "auto_ack" => {
                        entry.auto_ack = parse_bool(val)
                            .ok_or_else(|| anyhow!("line {line_no}: invalid boolean `{val}`"))?;
                    }
                    "drain_on_start" => {
                        entry.drain_on_start = parse_bool(val)
                            .ok_or_else(|| anyhow!("line {line_no}: invalid boolean `{val}`"))?;
                    }
                    "cooldown_secs" => {
                        entry.cooldown_secs = val.parse::<u64>().map_err(|_| {
                            anyhow!("line {line_no}: invalid integer `{val}` for cooldown_secs")
                        })?;
                    }
                    "env" => {
                        if !val.is_empty() {
                            return Err(anyhow!(
                                "line {line_no}: `env:` must be followed by indented KEY: VALUE lines"
                            ));
                        }
                        section = Section::HandleEnv(handle_name, indent);
                    }
                    other => {
                        return Err(anyhow!(
                            "line {line_no}: unknown key `{other}` under handle `{handle_name}`"
                        ));
                    }
                }
            }
            Section::HandleEnv(handle_name, _) => {
                let entry = cfg
                    .handles
                    .get_mut(handle_name)
                    .expect("handle entry exists");
                entry
                    .env
                    .insert(key, expand_env_vars(&unquote_yaml(val)));
            }
        }
    }

    Ok(cfg)
}

fn strip_yaml_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => {
                if idx == 0 || line[..idx].ends_with(' ') || line[..idx].ends_with('\t') {
                    return &line[..idx];
                }
            }
            _ => {}
        }
    }
    line
}

fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let mut in_single = false;
    let mut in_double = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ':' if !in_single && !in_double => {
                let rest = &line[idx + 1..];
                if rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t') {
                    return Some((line[..idx].trim(), rest));
                }
            }
            _ => {}
        }
    }
    None
}

fn unquote_yaml(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return t[1..t.len() - 1].replace("''", "'");
    }
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        let inner = &t[1..t.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(ch);
            }
        }
        return out;
    }
    t.to_string()
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_lowercase().as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(end) = after.find('}') {
            let var_name = &after[..end];
            if let Ok(val) = std::env::var(var_name) {
                out.push_str(&val);
            }
            rest = &after[end + 1..];
        } else {
            out.push_str("${");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_handle_yaml_config() {
        let yaml = r#"
# Top-level server URL
url: http://127.0.0.1:8787

handles:
  marcin-codex:
    token: rp_test_123
    exec: 'codex exec "Check #{{channel}} from @{{author}}: $RELAYPOD_BODY"'
    cwd: /tmp/repo
    auto_ack: true
    drain_on_start: false
    cooldown_secs: 5
    env:
      RUST_LOG: debug
      CUSTOM_URL: "http://example.com:8080/path # not a comment"

  claude-local:
    token: rp_test_456
    exec: "claude -p \"Handle unread $RELAYPOD_UNREAD_ID\""
"#;

        let cfg = parse_yaml_config(yaml).expect("valid yaml");
        assert_eq!(cfg.url.as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(cfg.handles.len(), 2);

        let codex = &cfg.handles["marcin-codex"];
        assert_eq!(codex.token.as_deref(), Some("rp_test_123"));
        assert_eq!(
            codex.exec,
            r#"codex exec "Check #{{channel}} from @{{author}}: $RELAYPOD_BODY""#
        );
        assert_eq!(codex.cwd.as_deref(), Some("/tmp/repo"));
        assert!(codex.auto_ack);
        assert!(!codex.drain_on_start);
        assert_eq!(codex.cooldown_secs, 5);
        assert_eq!(codex.env.get("RUST_LOG").map(String::as_str), Some("debug"));
        assert_eq!(
            codex.env.get("CUSTOM_URL").map(String::as_str),
            Some("http://example.com:8080/path # not a comment")
        );

        let claude = &cfg.handles["claude-local"];
        assert_eq!(claude.token.as_deref(), Some("rp_test_456"));
        assert!(!claude.auto_ack);
        assert!(claude.drain_on_start);
        assert_eq!(
            claude.exec,
            r#"claude -p "Handle unread $RELAYPOD_UNREAD_ID""#
        );
    }

    #[test]
    fn expands_env_vars_in_values() {
        std::env::set_var("RELAYPOD_TEST_YAML_TOK", "rp_from_env");
        let yaml = "handles:\n  bot:\n    token: ${RELAYPOD_TEST_YAML_TOK}\n    exec: echo hi\n";
        let cfg = parse_yaml_config(yaml).unwrap();
        assert_eq!(cfg.handles["bot"].token.as_deref(), Some("rp_from_env"));
    }
}
