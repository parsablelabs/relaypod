use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunnerKind {
    #[default]
    Exec,
    Herdr,
}

impl RunnerKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "exec" | "direct" => Some(Self::Exec),
            "herdr" => Some(Self::Herdr),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentType {
    Codex,
    Claude,
    /// Additional agent kinds supported by interactive runners such as Herdr
    /// (e.g. `gemini`, `opencode`, `pi`, `cursor`, `agy`).
    Other(String),
}

impl AgentType {
    pub fn parse(s: &str) -> Option<Self> {
        let normalized = s.trim().to_lowercase();
        if normalized.is_empty() {
            return None;
        }
        match normalized.as_str() {
            "codex" => Some(Self::Codex),
            "claude" | "claude-code" => Some(Self::Claude),
            other if other
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') =>
            {
                Some(Self::Other(other.to_string()))
            }
            _ => None,
        }
    }

    pub fn as_herdr_kind(&self) -> &str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Other(kind) => kind.as_str(),
        }
    }
}

pub const DEFAULT_MAX_CONSECUTIVE_AGENT_TURNS: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelayPodConfig {
    pub server: ServerConfig,
    pub watcher: WatchConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerConfig {
    /// Human handles granted admin privileges by the server (defined only in config.yaml, never in DB).
    pub admin_handles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchConfig {
    pub url: Option<String>,
    pub instructions_file: Option<String>,
    pub global_max_sessions: Option<usize>,
    /// Maximum consecutive agent-authored messages in a thread before pausing
    /// automatic agent dispatch (defaults to 6; set to 0 in YAML to disable).
    pub max_consecutive_agent_turns: Option<usize>,
    pub handles: BTreeMap<String, HandleConfig>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            url: None,
            instructions_file: None,
            global_max_sessions: None,
            max_consecutive_agent_turns: Some(DEFAULT_MAX_CONSECUTIVE_AGENT_TURNS),
            handles: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandleConfig {
    pub runner: RunnerKind,
    pub agent_type: Option<AgentType>,
    pub url: Option<String>,
    pub token: Option<String>,
    pub exec: String,
    pub args: Option<String>,
    pub prompt: Option<String>,
    pub instructions_file: Option<String>,
    pub cwd: Option<String>,
    pub herdr_session: Option<String>,
    pub herdr_workspace: Option<String>,
    pub max_sessions: Option<usize>,
    pub auto_ack: bool,
    pub drain_on_start: bool,
    pub cooldown_secs: u64,
    pub env: BTreeMap<String, String>,
}

impl Default for HandleConfig {
    fn default() -> Self {
        Self {
            runner: RunnerKind::Exec,
            agent_type: None,
            url: None,
            token: None,
            exec: String::new(),
            args: None,
            prompt: None,
            instructions_file: None,
            cwd: None,
            herdr_session: None,
            herdr_workspace: None,
            max_sessions: None,
            auto_ack: false,
            drain_on_start: true,
            cooldown_secs: 0,
            env: BTreeMap::new(),
        }
    }
}

pub fn load_config_file(path: &Path) -> Result<RelayPodConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    parse_yaml_config(&raw)
        .with_context(|| format!("failed to parse config file {}", path.display()))
}

/// Lightweight, dependency-free YAML parser for `config.yaml` / `.relaypod.yaml`.
/// Supports top-level `server:` (`admin_handles:`) and `watcher:` (`url`, `instructions_file`,
/// `global_max_sessions`, `max_consecutive_agent_turns`, `handles:` map, per-handle settings,
/// nested `env:` map), single/double quoted strings, `#` comments, and `${ENV_VAR}` expansion.
pub fn parse_yaml_config(input: &str) -> Result<RelayPodConfig> {
    let mut cfg = RelayPodConfig::default();

    #[derive(Debug, PartialEq, Eq)]
    enum Section {
        Root,
        Server {
            indent: usize,
        },
        ServerAdminHandles {
            server_indent: usize,
            key_indent: usize,
        },
        Watcher {
            indent: usize,
        },
        WatcherHandles {
            watcher_indent: usize,
            handles_indent: usize,
        },
        Handle {
            watcher_indent: usize,
            handles_indent: usize,
            handle_indent: usize,
            name: String,
        },
        HandleEnv {
            watcher_indent: usize,
            handles_indent: usize,
            handle_indent: usize,
            env_indent: usize,
            name: String,
        },
    }

    let mut section = Section::Root;

    for (line_idx, raw_line) in input.lines().enumerate() {
        let line_no = line_idx + 1;
        let stripped = strip_yaml_comment(raw_line);
        if stripped.trim().is_empty() {
            continue;
        }

        let indent: usize = stripped
            .bytes()
            .take_while(|&b| b == b' ' || b == b'\t')
            .map(|b| if b == b'\t' { 4 } else { 1 })
            .sum();
        let trimmed = stripped.trim();

        // Unwind nested sections based on indentation
        loop {
            let next = match &section {
                Section::HandleEnv {
                    watcher_indent,
                    handles_indent,
                    handle_indent,
                    env_indent,
                    name,
                } if indent <= *env_indent => Some(Section::Handle {
                    watcher_indent: *watcher_indent,
                    handles_indent: *handles_indent,
                    handle_indent: *handle_indent,
                    name: name.clone(),
                }),
                Section::Handle {
                    watcher_indent,
                    handles_indent,
                    handle_indent,
                    ..
                } if indent <= *handle_indent => Some(Section::WatcherHandles {
                    watcher_indent: *watcher_indent,
                    handles_indent: *handles_indent,
                }),
                Section::WatcherHandles {
                    watcher_indent,
                    handles_indent,
                } if indent <= *handles_indent => Some(Section::Watcher {
                    indent: *watcher_indent,
                }),
                Section::Watcher { indent: w_indent } if indent <= *w_indent => Some(Section::Root),
                Section::ServerAdminHandles {
                    server_indent,
                    key_indent,
                } if !(trimmed.starts_with('-') && indent >= *key_indent)
                    && indent <= *key_indent =>
                {
                    Some(Section::Server {
                        indent: *server_indent,
                    })
                }
                Section::Server { indent: s_indent } if indent <= *s_indent => Some(Section::Root),
                _ => None,
            };
            match next {
                Some(popped) => section = popped,
                None => break,
            }
        }

        if matches!(section, Section::ServerAdminHandles { .. }) {
            let item = trimmed.strip_prefix('-').ok_or_else(|| {
                anyhow!("invalid YAML at line {line_no}: expected `- <handle>` under `admin_handles:`")
            })?;
            let handle = normalize_admin_handle(&expand_env_vars(&unquote_yaml(item)));
            if handle.is_empty() {
                return Err(anyhow!(
                    "line {line_no}: empty handle entry under `admin_handles:`"
                ));
            }
            if !cfg.server.admin_handles.contains(&handle) {
                cfg.server.admin_handles.push(handle);
            }
            continue;
        }

        let (key, raw_val) = split_key_value(trimmed)
            .ok_or_else(|| anyhow!("invalid YAML at line {line_no}: expected `key: value`"))?;
        let key = unquote_yaml(key);
        let val = raw_val.trim();

        match &section {
            Section::Root => match key.as_str() {
                "server" => {
                    if !val.is_empty() {
                        return Err(anyhow!(
                            "line {line_no}: `server:` must be followed by indented keys"
                        ));
                    }
                    section = Section::Server { indent };
                }
                "watcher" => {
                    if !val.is_empty() {
                        return Err(anyhow!(
                            "line {line_no}: `watcher:` must be followed by indented keys"
                        ));
                    }
                    section = Section::Watcher { indent };
                }
                other => {
                    return Err(anyhow!(
                        "line {line_no}: unknown top-level key `{other}` (expected `server` or `watcher`)"
                    ));
                }
            },
            Section::Server {
                indent: server_indent,
            } => {
                let server_indent = *server_indent;
                match key.as_str() {
                    "admin_handles" => {
                        if val.is_empty() {
                            section = Section::ServerAdminHandles {
                                server_indent,
                                key_indent: indent,
                            };
                        } else if let Some(inner) =
                            val.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
                        {
                            for part in inner.split(',') {
                                let part = part.trim();
                                if part.is_empty() {
                                    continue;
                                }
                                let handle =
                                    normalize_admin_handle(&expand_env_vars(&unquote_yaml(part)));
                                if !handle.is_empty() && !cfg.server.admin_handles.contains(&handle)
                                {
                                    cfg.server.admin_handles.push(handle);
                                }
                            }
                        } else {
                            return Err(anyhow!(
                                "line {line_no}: `admin_handles:` must be a YAML list (`- handle` items or `[a, b]`)"
                            ));
                        }
                    }
                    other => {
                        return Err(anyhow!(
                            "line {line_no}: unknown key `{other}` under `server:`"
                        ));
                    }
                }
            }
            Section::ServerAdminHandles { .. } => unreachable!("handled above"),
            Section::Watcher {
                indent: watcher_indent,
            } => {
                let watcher_indent = *watcher_indent;
                match key.as_str() {
                    "url" => {
                        cfg.watcher.url = Some(expand_env_vars(&unquote_yaml(val)));
                    }
                    "instructions_file" => {
                        cfg.watcher.instructions_file = Some(expand_env_vars(&unquote_yaml(val)));
                    }
                    "global_max_sessions" => {
                        let parsed = val.parse::<usize>().map_err(|_| {
                            anyhow!("line {line_no}: invalid integer `{val}` for global_max_sessions")
                        })?;
                        cfg.watcher.global_max_sessions =
                            if parsed == 0 { None } else { Some(parsed) };
                    }
                    "max_consecutive_agent_turns" => {
                        let parsed = val.parse::<usize>().map_err(|_| {
                            anyhow!(
                                "line {line_no}: invalid integer `{val}` for max_consecutive_agent_turns"
                            )
                        })?;
                        cfg.watcher.max_consecutive_agent_turns =
                            if parsed == 0 { None } else { Some(parsed) };
                    }
                    "handles" => {
                        if !val.is_empty() {
                            return Err(anyhow!(
                                "line {line_no}: `handles:` must be followed by indented handle entries"
                            ));
                        }
                        section = Section::WatcherHandles {
                            watcher_indent,
                            handles_indent: indent,
                        };
                    }
                    other => {
                        return Err(anyhow!(
                            "line {line_no}: unknown key `{other}` under `watcher:`"
                        ));
                    }
                }
            }
            Section::WatcherHandles {
                watcher_indent,
                handles_indent,
            } => {
                let watcher_indent = *watcher_indent;
                let handles_indent = *handles_indent;
                let handle_name = key.trim_start_matches('@').to_string();
                cfg.watcher.handles.entry(handle_name.clone()).or_default();
                section = Section::Handle {
                    watcher_indent,
                    handles_indent,
                    handle_indent: indent,
                    name: handle_name,
                };
            }
            Section::Handle {
                watcher_indent,
                handles_indent,
                handle_indent,
                name,
            } => {
                let watcher_indent = *watcher_indent;
                let handles_indent = *handles_indent;
                let handle_indent = *handle_indent;
                let handle_name = name.clone();
                let entry = cfg
                    .watcher
                    .handles
                    .get_mut(&handle_name)
                    .expect("handle entry exists");

                match key.as_str() {
                    "runner" => {
                        let parsed = RunnerKind::parse(&unquote_yaml(val)).ok_or_else(|| {
                            anyhow!(
                                "line {line_no}: unknown runner `{val}` (expected `exec` or `herdr`)"
                            )
                        })?;
                        entry.runner = parsed;
                    }
                    "agent_type" => {
                        let parsed = AgentType::parse(&unquote_yaml(val)).ok_or_else(|| {
                            anyhow!("line {line_no}: invalid agent_type `{val}`")
                        })?;
                        entry.agent_type = Some(parsed);
                    }
                    "url" => entry.url = Some(expand_env_vars(&unquote_yaml(val))),
                    "token" => {
                        let expanded = expand_env_vars(&unquote_yaml(val));
                        if !expanded.is_empty() {
                            entry.token = Some(expanded);
                        }
                    }
                    "exec" => entry.exec = expand_env_vars(&unquote_yaml(val)),
                    "args" => entry.args = Some(expand_env_vars(&unquote_yaml(val))),
                    "prompt" => entry.prompt = Some(expand_env_vars(&unquote_yaml(val))),
                    "instructions_file" => {
                        entry.instructions_file = Some(expand_env_vars(&unquote_yaml(val)))
                    }
                    "cwd" => entry.cwd = Some(expand_env_vars(&unquote_yaml(val))),
                    "herdr_session" => {
                        entry.herdr_session = Some(expand_env_vars(&unquote_yaml(val)))
                    }
                    "herdr_workspace" => {
                        entry.herdr_workspace = Some(expand_env_vars(&unquote_yaml(val)))
                    }
                    "max_sessions" => {
                        let parsed = val.parse::<usize>().map_err(|_| {
                            anyhow!("line {line_no}: invalid integer `{val}` for max_sessions")
                        })?;
                        entry.max_sessions = if parsed == 0 { None } else { Some(parsed) };
                    }
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
                        section = Section::HandleEnv {
                            watcher_indent,
                            handles_indent,
                            handle_indent,
                            env_indent: indent,
                            name: handle_name,
                        };
                    }
                    other => {
                        return Err(anyhow!(
                            "line {line_no}: unknown key `{other}` under handle `{handle_name}`"
                        ));
                    }
                }
            }
            Section::HandleEnv { name, .. } => {
                let entry = cfg
                    .watcher
                    .handles
                    .get_mut(name)
                    .expect("handle entry exists");
                entry
                    .env
                    .insert(key, expand_env_vars(&unquote_yaml(val)));
            }
        }
    }

    Ok(cfg)
}

fn normalize_admin_handle(raw: &str) -> String {
    raw.trim().trim_start_matches('@').to_lowercase()
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
            '#' if !in_single
                && !in_double
                && (idx == 0 || line[..idx].ends_with(' ') || line[..idx].ends_with('\t')) =>
            {
                return &line[..idx];
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

#[derive(Debug, Clone)]
pub struct NewWatcherHandleEntry {
    pub runner: RunnerKind,
    pub agent_type: Option<AgentType>,
    pub token: String,
    pub exec: Option<String>,
    pub args: Option<String>,
    pub cwd: Option<String>,
    pub herdr_session: Option<String>,
    pub herdr_workspace: Option<String>,
    pub instructions_file: Option<String>,
    pub max_sessions: Option<usize>,
    pub auto_ack: bool,
    pub drain_on_start: bool,
}

fn yaml_scalar(val: &str) -> String {
    let needs_quotes = val.is_empty()
        || val.starts_with(' ')
        || val.ends_with(' ')
        || val.contains('\n')
        || val.contains('#')
        || val.contains(':')
        || val.contains('\'')
        || val.contains('"')
        || val.starts_with('-')
        || val.starts_with('[')
        || val.starts_with('{');
    if needs_quotes {
        format!("'{}'", val.replace('\'', "''"))
    } else {
        val.to_string()
    }
}

fn render_handle_block(
    handle: &str,
    entry: &NewWatcherHandleEntry,
    handle_indent: &str,
    field_indent: &str,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("{handle_indent}{handle}:"));
    let runner_str = match entry.runner {
        RunnerKind::Herdr => "herdr",
        RunnerKind::Exec => "exec",
    };
    lines.push(format!("{field_indent}runner: {runner_str}"));
    if let Some(ref agent_type) = entry.agent_type {
        lines.push(format!(
            "{field_indent}agent_type: {}",
            agent_type.as_herdr_kind()
        ));
    }
    lines.push(format!(
        "{field_indent}token: {}",
        yaml_scalar(&entry.token)
    ));
    if let Some(ref exec) = entry.exec {
        lines.push(format!("{field_indent}exec: {}", yaml_scalar(exec)));
    }
    if let Some(ref args) = entry.args {
        lines.push(format!("{field_indent}args: {}", yaml_scalar(args)));
    }
    if let Some(ref cwd) = entry.cwd {
        lines.push(format!("{field_indent}cwd: {}", yaml_scalar(cwd)));
    }
    if let Some(ref session) = entry.herdr_session {
        lines.push(format!(
            "{field_indent}herdr_session: {}",
            yaml_scalar(session)
        ));
    }
    if let Some(ref workspace) = entry.herdr_workspace {
        lines.push(format!(
            "{field_indent}herdr_workspace: {}",
            yaml_scalar(workspace)
        ));
    }
    if let Some(ref instr) = entry.instructions_file {
        lines.push(format!(
            "{field_indent}instructions_file: {}",
            yaml_scalar(instr)
        ));
    }
    if let Some(max_sessions) = entry.max_sessions {
        lines.push(format!("{field_indent}max_sessions: {max_sessions}"));
    }
    lines.push(format!("{field_indent}auto_ack: {}", entry.auto_ack));
    lines.push(format!(
        "{field_indent}drain_on_start: {}",
        entry.drain_on_start
    ));
    lines
}

fn leading_whitespace(line: &str) -> (&str, usize) {
    let mut byte_len = 0usize;
    let mut cols = 0usize;
    for b in line.bytes() {
        match b {
            b' ' => {
                byte_len += 1;
                cols += 1;
            }
            b'\t' => {
                byte_len += 1;
                cols += 4;
            }
            _ => break,
        }
    }
    (&line[..byte_len], cols)
}

/// Inserts a new watcher handle block into an existing (or empty) `config.yaml` string,
/// preserving existing comments, indentation, and sections, and verifies the resulting YAML parses.
pub fn insert_watcher_handle_yaml(
    existing: &str,
    url: &str,
    handle: &str,
    entry: &NewWatcherHandleEntry,
) -> Result<String> {
    if !existing.trim().is_empty() {
        let parsed = parse_yaml_config(existing)?;
        if parsed.watcher.handles.contains_key(handle) {
            return Err(anyhow!(
                "watcher handle '{handle}' already exists in config.yaml"
            ));
        }
    }

    let mut lines: Vec<String> = if existing.is_empty() {
        Vec::new()
    } else {
        existing.lines().map(ToString::to_string).collect()
    };

    // Locate top-level `watcher:` section and its bounds.
    let mut watcher_start: Option<usize> = None;
    let mut watcher_end: usize = lines.len();
    for (idx, raw) in lines.iter().enumerate() {
        let stripped = strip_yaml_comment(raw);
        if stripped.trim().is_empty() {
            continue;
        }
        let (_, cols) = leading_whitespace(stripped);
        if cols == 0 {
            if let Some((key, _)) = split_key_value(stripped.trim()) {
                if watcher_start.is_none() && key == "watcher" {
                    watcher_start = Some(idx);
                } else if watcher_start.is_some() {
                    watcher_end = idx;
                    break;
                }
            }
        }
    }

    match watcher_start {
        None => {
            if !lines.is_empty() && !lines.last().is_some_and(|l| l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("watcher:".to_string());
            lines.push(format!("  url: {}", yaml_scalar(url)));
            lines.push("  handles:".to_string());
            lines.extend(render_handle_block(handle, entry, "    ", "      "));
        }
        Some(w_start) => {
            // Find `handles:` inside `watcher:` and detect child indentation.
            let mut watcher_child_indent_str: Option<String> = None;
            let mut watcher_child_cols: Option<usize> = None;
            let mut handles_idx: Option<usize> = None;
            let mut handles_indent_str = String::new();
            let mut handles_cols = 0usize;

            for (idx, raw) in lines.iter().enumerate().take(watcher_end).skip(w_start + 1) {
                let stripped = strip_yaml_comment(raw);
                if stripped.trim().is_empty() {
                    continue;
                }
                let (ws, cols) = leading_whitespace(stripped);
                if watcher_child_cols.is_none() {
                    watcher_child_cols = Some(cols);
                    watcher_child_indent_str = Some(ws.to_string());
                }
                if Some(cols) == watcher_child_cols {
                    if let Some((key, _)) = split_key_value(stripped.trim()) {
                        if key == "handles" {
                            handles_idx = Some(idx);
                            handles_indent_str = ws.to_string();
                            handles_cols = cols;
                            break;
                        }
                    }
                }
            }

            if let Some(h_idx) = handles_idx {
                let mut h_end = watcher_end;
                let mut existing_handle_indent: Option<String> = None;
                let mut existing_handle_cols: Option<usize> = None;
                let mut existing_field_indent: Option<String> = None;

                for (idx, raw) in lines.iter().enumerate().take(watcher_end).skip(h_idx + 1) {
                    let stripped = strip_yaml_comment(raw);
                    if stripped.trim().is_empty() {
                        continue;
                    }
                    let (ws, cols) = leading_whitespace(stripped);
                    if cols <= handles_cols {
                        h_end = idx;
                        break;
                    }
                    if existing_handle_cols.is_none() {
                        existing_handle_cols = Some(cols);
                        existing_handle_indent = Some(ws.to_string());
                    } else if existing_field_indent.is_none()
                        && Some(cols) > existing_handle_cols
                    {
                        existing_field_indent = Some(ws.to_string());
                    }
                }

                // Back up past trailing blank lines inside the handles block for clean insertion.
                let mut insert_at = h_end;
                while insert_at > h_idx + 1 && lines[insert_at - 1].trim().is_empty() {
                    insert_at -= 1;
                }

                let handle_indent = existing_handle_indent
                    .unwrap_or_else(|| format!("{handles_indent_str}  "));
                let field_indent =
                    existing_field_indent.unwrap_or_else(|| format!("{handle_indent}  "));
                let block = render_handle_block(handle, entry, &handle_indent, &field_indent);
                lines.splice(insert_at..insert_at, block);
            } else {
                // `watcher:` exists without an active `handles:` section.
                let mut insert_at = watcher_end;
                while insert_at > w_start + 1 && lines[insert_at - 1].trim().is_empty() {
                    insert_at -= 1;
                }
                let child_indent = watcher_child_indent_str.unwrap_or_else(|| "  ".to_string());
                let handle_indent = format!("{child_indent}  ");
                let field_indent = format!("{handle_indent}  ");
                let mut block = Vec::new();
                block.push(format!("{child_indent}handles:"));
                block.extend(render_handle_block(
                    handle,
                    entry,
                    &handle_indent,
                    &field_indent,
                ));
                lines.splice(insert_at..insert_at, block);
            }
        }
    }

    let mut out = lines.join("\n");
    out.push('\n');

    // Verify the updated YAML still parses cleanly and contains the new handle.
    let reparsed = parse_yaml_config(&out)
        .context("internal error: generated config.yaml failed validation")?;
    if !reparsed.watcher.handles.contains_key(handle) {
        return Err(anyhow!(
            "internal error: newly added handle '{handle}' missing after config update"
        ));
    }

    Ok(out)
}

fn write_config_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }

    let tmp_path = path.with_extension("yaml.tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&tmp_path)
            .with_context(|| format!("failed to open temp config {}", tmp_path.display()))?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }

    std::fs::rename(&tmp_path, path).with_context(|| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("failed to write updated config {}", path.display())
    })?;

    Ok(())
}

pub fn append_watcher_handle_to_config(
    path: &Path,
    url: &str,
    handle: &str,
    entry: &NewWatcherHandleEntry,
) -> Result<()> {
    let existing = if path.exists() {
        std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?
    } else {
        String::new()
    };

    let updated = insert_watcher_handle_yaml(&existing, url, handle, entry)?;
    write_config_atomic(path, &updated)
}

/// Updates the `token:` field for an existing handle under `watcher.handles` in a YAML string.
/// Returns `Ok(None)` if the handle is not configured in `watcher.handles`.
pub fn update_watcher_handle_token_yaml(
    existing: &str,
    handle: &str,
    new_token: &str,
) -> Result<Option<String>> {
    if existing.trim().is_empty() {
        return Ok(None);
    }
    let parsed = parse_yaml_config(existing)?;
    if !parsed.watcher.handles.contains_key(handle) {
        return Ok(None);
    }

    let mut lines: Vec<String> = existing.lines().map(ToString::to_string).collect();

    // Locate `watcher:` -> `handles:` -> `<handle>:` block.
    let mut in_watcher = false;
    let mut watcher_indent = 0usize;
    let mut in_handles = false;
    let mut handles_indent = 0usize;
    let mut handle_start: Option<usize> = None;
    let mut handle_indent_cols = 0usize;
    let mut handle_indent_str = String::new();
    let mut handle_end = lines.len();

    for (idx, raw) in lines.iter().enumerate() {
        let stripped = strip_yaml_comment(raw);
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (ws, cols) = leading_whitespace(stripped);

        if let Some(start) = handle_start {
            if cols <= handle_indent_cols && idx > start {
                handle_end = idx;
                break;
            }
            continue;
        }

        if in_handles && cols <= handles_indent {
            in_handles = false;
        }
        if in_watcher && cols <= watcher_indent {
            in_watcher = false;
        }

        if !in_watcher {
            if cols == 0 {
                if let Some((key, _)) = split_key_value(trimmed) {
                    if key == "watcher" {
                        in_watcher = true;
                        watcher_indent = cols;
                    }
                }
            }
            continue;
        }

        if !in_handles {
            if let Some((key, _)) = split_key_value(trimmed) {
                if key == "handles" {
                    in_handles = true;
                    handles_indent = cols;
                }
            }
            continue;
        }

        if let Some((key, _)) = split_key_value(trimmed) {
            if unquote_yaml(key) == handle {
                handle_start = Some(idx);
                handle_indent_cols = cols;
                handle_indent_str = ws.to_string();
            }
        }
    }

    let start = handle_start.ok_or_else(|| {
        anyhow!("internal error: handle '{handle}' found during parse but not during update")
    })?;

    let mut token_line_idx: Option<usize> = None;
    let mut field_indent: Option<String> = None;
    for (idx, raw) in lines.iter().enumerate().take(handle_end).skip(start + 1) {
        let stripped = strip_yaml_comment(raw);
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (ws, cols) = leading_whitespace(stripped);
        if cols > handle_indent_cols {
            if field_indent.is_none() {
                field_indent = Some(ws.to_string());
            }
            if let Some((key, _)) = split_key_value(trimmed) {
                if key == "token" {
                    token_line_idx = Some(idx);
                    field_indent = Some(ws.to_string());
                    break;
                }
            }
        }
    }

    let indent = field_indent.unwrap_or_else(|| {
        if handle_indent_str.contains('\t') {
            format!("{handle_indent_str}\t")
        } else {
            format!("{handle_indent_str}  ")
        }
    });
    let rendered = format!("{indent}token: {}", yaml_scalar(new_token));

    if let Some(idx) = token_line_idx {
        lines[idx] = rendered;
    } else {
        lines.insert(start + 1, rendered);
    }

    let mut out = lines.join("\n");
    out.push('\n');

    let reparsed = parse_yaml_config(&out)
        .context("internal error: updated config.yaml failed validation")?;
    let stored = reparsed
        .watcher
        .handles
        .get(handle)
        .and_then(|h| h.token.as_deref());
    if stored != Some(new_token) {
        return Err(anyhow!(
            "internal error: updated token for '{handle}' did not match after validation"
        ));
    }

    Ok(Some(out))
}

/// Updates the `token:` for an existing handle in `config.yaml` if the file and handle exist.
/// Returns `Ok(true)` if updated, `Ok(false)` if the file or handle was not present.
pub fn update_watcher_handle_token_in_config(
    path: &Path,
    handle: &str,
    new_token: &str,
) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let existing = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let Some(updated) = update_watcher_handle_token_yaml(&existing, handle, new_token)? else {
        return Ok(false);
    };
    write_config_atomic(path, &updated)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_handle_yaml_config() {
        let yaml = r#"
server:
        admin_handles:
        - admin
        - "@Alice"

watcher:
        url: http://127.0.0.1:8787
        handles:
          codex-local:
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
        assert_eq!(cfg.server.admin_handles, vec!["admin", "alice"]);
        assert_eq!(cfg.watcher.url.as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(cfg.watcher.handles.len(), 2);

        let codex = &cfg.watcher.handles["codex-local"];
        assert_eq!(codex.runner, RunnerKind::Exec);
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

        let claude = &cfg.watcher.handles["claude-local"];
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
        let yaml =
            "watcher:\n  handles:\n    bot:\n      token: ${RELAYPOD_TEST_YAML_TOK}\n      exec: echo hi\n";
        let cfg = parse_yaml_config(yaml).unwrap();
        assert_eq!(
            cfg.watcher.handles["bot"].token.as_deref(),
            Some("rp_from_env")
        );
    }

    #[test]
    fn parses_herdr_runner_and_agent_types() {
        let yaml = r#"
server:
  admin_handles: [alice, bob]
watcher:
  url: http://127.0.0.1:8787
  global_max_sessions: 4
  handles:
    codex-local:
      runner: herdr
      agent_type: codex
      token: rp_123
      args: -m gpt-5.2-codex -c model_reasoning_effort=medium
      cwd: ~/projects
      herdr_session: main
      herdr_workspace: w1
      max_sessions: 6
    claude-local:
      runner: exec
      agent_type: claude
      token: rp_456
    gemini-herdr:
      runner: herdr
      agent_type: gemini
      token: rp_789
"#;
        let cfg = parse_yaml_config(yaml).unwrap();
        assert_eq!(cfg.server.admin_handles, vec!["alice", "bob"]);
        assert_eq!(cfg.watcher.global_max_sessions, Some(4));
        assert_eq!(
            cfg.watcher.max_consecutive_agent_turns,
            Some(DEFAULT_MAX_CONSECUTIVE_AGENT_TURNS)
        );

        let codex = &cfg.watcher.handles["codex-local"];
        assert_eq!(codex.runner, RunnerKind::Herdr);
        assert_eq!(codex.agent_type, Some(AgentType::Codex));
        assert_eq!(codex.herdr_session.as_deref(), Some("main"));
        assert_eq!(codex.herdr_workspace.as_deref(), Some("w1"));
        assert_eq!(codex.max_sessions, Some(6));

        let claude = &cfg.watcher.handles["claude-local"];
        assert_eq!(claude.runner, RunnerKind::Exec);
        assert_eq!(claude.agent_type, Some(AgentType::Claude));
        assert_eq!(claude.max_sessions, None);

        let gemini = &cfg.watcher.handles["gemini-herdr"];
        assert_eq!(gemini.runner, RunnerKind::Herdr);
        assert_eq!(gemini.agent_type, Some(AgentType::Other("gemini".into())));
    }

    #[test]
    fn parses_max_consecutive_agent_turns() {
        let custom = parse_yaml_config(
            "watcher:\n  max_consecutive_agent_turns: 10\n  handles:\n    bot:\n      exec: echo hi\n",
        )
        .unwrap();
        assert_eq!(custom.watcher.max_consecutive_agent_turns, Some(10));

        let disabled = parse_yaml_config(
            "watcher:\n  max_consecutive_agent_turns: 0\n  handles:\n    bot:\n      exec: echo hi\n",
        )
        .unwrap();
        assert_eq!(disabled.watcher.max_consecutive_agent_turns, None);
    }

    #[test]
    fn parses_tab_indented_config() {
        let yaml = "server:\n\tadmin_handles:\n\t- alice\n\nwatcher:\n\turl: http://127.0.0.1:8787\n\thandles:\n\t  bot:\n\t    token: rp_123\n\t    exec: echo hi\n";
        let cfg = parse_yaml_config(yaml).expect("tab-indented config should parse");
        assert_eq!(cfg.server.admin_handles, vec!["alice"]);
        assert_eq!(cfg.watcher.url.as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(cfg.watcher.handles["bot"].token.as_deref(), Some("rp_123"));
    }

    #[test]
    fn inserts_watcher_handle_into_empty_starter_existing_and_tab_configs() {
        let entry = NewWatcherHandleEntry {
            runner: RunnerKind::Herdr,
            agent_type: Some(AgentType::Claude),
            token: "rp_secret_123".into(),
            exec: None,
            args: Some("--model sonnet".into()),
            cwd: Some("/tmp/my-repo".into()),
            herdr_session: None,
            herdr_workspace: Some("relaypod".into()),
            instructions_file: None,
            max_sessions: Some(5),
            auto_ack: false,
            drain_on_start: true,
        };

        // 1. Empty file
        let from_empty =
            insert_watcher_handle_yaml("", "http://127.0.0.1:8787", "claude-local", &entry)
                .unwrap();
        let cfg = parse_yaml_config(&from_empty).unwrap();
        assert_eq!(cfg.watcher.url.as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(
            cfg.watcher.handles["claude-local"].runner,
            RunnerKind::Herdr
        );
        assert_eq!(
            cfg.watcher.handles["claude-local"].agent_type,
            Some(AgentType::Claude)
        );
        assert_eq!(
            cfg.watcher.handles["claude-local"].token.as_deref(),
            Some("rp_secret_123")
        );

        // 2. Starter config with commented-out `# handles:`
        let starter = "server:\n  admin_handles:\n    - alice\n\nwatcher:\n  url: http://127.0.0.1:8787\n  # handles:\n  #   example:\n";
        let from_starter =
            insert_watcher_handle_yaml(starter, "http://127.0.0.1:8787", "claude-local", &entry)
                .unwrap();
        let cfg = parse_yaml_config(&from_starter).unwrap();
        assert_eq!(cfg.server.admin_handles, vec!["alice"]);
        assert!(cfg.watcher.handles.contains_key("claude-local"));

        // 3. Existing `handles:` block (with another handle already present)
        let second = NewWatcherHandleEntry {
            agent_type: Some(AgentType::Codex),
            token: "rp_second".into(),
            ..entry.clone()
        };
        let with_two =
            insert_watcher_handle_yaml(&from_starter, "http://127.0.0.1:8787", "codex-local", &second)
                .unwrap();
        let cfg = parse_yaml_config(&with_two).unwrap();
        assert_eq!(cfg.watcher.handles.len(), 2);
        assert!(cfg.watcher.handles.contains_key("claude-local"));
        assert!(cfg.watcher.handles.contains_key("codex-local"));

        // 4. Tab-indented config
        let tabbed = "server:\n\tadmin_handles:\n\t- alice\n\nwatcher:\n\turl: http://127.0.0.1:8787\n\thandles:\n\t  bot:\n\t    token: rp_123\n\t    exec: echo hi\n";
        let from_tabbed =
            insert_watcher_handle_yaml(tabbed, "http://127.0.0.1:8787", "claude-local", &entry)
                .unwrap();
        let cfg = parse_yaml_config(&from_tabbed).unwrap();
        assert_eq!(cfg.watcher.handles.len(), 2);
        assert_eq!(
            cfg.watcher.handles["claude-local"].token.as_deref(),
            Some("rp_secret_123")
        );

        // 5. Duplicate handle is rejected
        assert!(insert_watcher_handle_yaml(
            &with_two,
            "http://127.0.0.1:8787",
            "claude-local",
            &entry
        )
        .is_err());
    }

    #[test]
    fn updates_watcher_handle_token_in_yaml() {
        let yaml = r#"server:
  admin_handles:
    - alice

watcher:
  url: http://127.0.0.1:8787
  handles:
    claude-local:
      runner: herdr
      agent_type: claude
      token: rp_old_token
      cwd: /tmp/repo
    codex-local:
      runner: herdr
      agent_type: codex
      token: rp_other_token
"#;

        let updated = update_watcher_handle_token_yaml(yaml, "claude-local", "rp_new_token")
            .unwrap()
            .expect("handle exists");
        let cfg = parse_yaml_config(&updated).unwrap();
        assert_eq!(
            cfg.watcher.handles["claude-local"].token.as_deref(),
            Some("rp_new_token")
        );
        assert_eq!(
            cfg.watcher.handles["codex-local"].token.as_deref(),
            Some("rp_other_token")
        );

        // Missing handle returns Ok(None).
        assert!(update_watcher_handle_token_yaml(yaml, "unknown-bot", "rp_x")
            .unwrap()
            .is_none());
    }
}
