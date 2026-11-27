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
            other
                if other.chars().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'
                }) =>
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
pub const DEFAULT_SERVER_HOST: &str = "127.0.0.1";
pub const DEFAULT_SERVER_PORT: u16 = 8787;
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8787";

/// Client configuration stored at `~/.config/relaypod/config.yaml`.
/// Shared by the desktop UI (`relaypod-ui`), CLI (`relaypod`), and watcher (`relaypod watch`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelayPodConfig {
    /// RelayPod server URL (e.g. `http://127.0.0.1:8787`).
    pub url: Option<String>,
    /// Optional human bearer token (for UI auto-login and CLI commands; `RELAYPOD_TOKEN` overrides).
    pub token: Option<String>,
    /// Local agent watcher settings and agent tokens.
    pub watcher: WatchConfig,
}

/// Server configuration stored at `~/.config/relaypod/server.yaml` (used only by `relaypod serve`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Interface/IP to bind to (defaults to `127.0.0.1`).
    pub host: String,
    /// TCP port to listen on (defaults to `8787`).
    pub port: u16,
    /// Optional SQLite database path override.
    pub db: Option<String>,
    /// Human handles granted admin privileges by the server (defined only in server.yaml, never in DB).
    pub admin_handles: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_SERVER_HOST.to_string(),
            port: DEFAULT_SERVER_PORT,
            db: None,
            admin_handles: Vec::new(),
        }
    }
}

impl ServerConfig {
    pub fn bind_addr(&self) -> Result<std::net::SocketAddr> {
        let ip: std::net::IpAddr = self
            .host
            .parse()
            .with_context(|| format!("invalid server host IP `{}` in server.yaml", self.host))?;
        Ok(std::net::SocketAddr::new(ip, self.port))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchConfig {
    pub instructions_file: Option<String>,
    pub global_max_sessions: Option<usize>,
    /// Maximum consecutive agent-authored messages in a thread before pausing
    /// automatic agent dispatch (defaults to 6; set to 0 in YAML to disable).
    pub max_consecutive_agent_turns: Option<usize>,
    /// Flat map of unique local names to agent bearer tokens (e.g. `mbp-codex: rp_...`).
    pub agent_tokens: BTreeMap<String, String>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            instructions_file: None,
            global_max_sessions: None,
            max_consecutive_agent_turns: Some(DEFAULT_MAX_CONSECUTIVE_AGENT_TURNS),
            agent_tokens: BTreeMap::new(),
        }
    }
}

pub fn load_config_file(path: &Path) -> Result<RelayPodConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    parse_yaml_config(&raw)
        .with_context(|| format!("failed to parse config file {}", path.display()))
}

pub fn load_server_config_file(path: &Path) -> Result<ServerConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read server config file {}", path.display()))?;
    parse_server_config_yaml(&raw)
        .with_context(|| format!("failed to parse server config file {}", path.display()))
}

/// Lightweight, dependency-free YAML parser for `~/.config/relaypod/server.yaml`.
/// Supports top-level `host`, `port`, `db`, and `admin_handles` (`- handle` list or `[a, b]`),
/// single/double quoted strings, `#` comments, and `${ENV_VAR}` expansion.
pub fn parse_server_config_yaml(input: &str) -> Result<ServerConfig> {
    let mut cfg = ServerConfig::default();

    #[derive(Debug, PartialEq, Eq)]
    enum Section {
        Root,
        AdminHandles { key_indent: usize },
    }

    let mut section = Section::Root;

    for (line_idx, raw_line) in input.lines().enumerate() {
        let line_no = line_idx + 1;
        let stripped = strip_yaml_comment(raw_line);
        if stripped.trim().is_empty() {
            continue;
        }

        let (_, indent) = leading_whitespace(stripped);
        let trimmed = stripped.trim();

        if let Section::AdminHandles { key_indent } = section {
            if !(trimmed.starts_with('-') && indent >= key_indent) && indent <= key_indent {
                section = Section::Root;
            }
        }

        if matches!(section, Section::AdminHandles { .. }) {
            let item = trimmed.strip_prefix('-').ok_or_else(|| {
                anyhow!(
                    "invalid YAML at line {line_no}: expected `- <handle>` under `admin_handles:`"
                )
            })?;
            let handle = normalize_admin_handle(&expand_env_vars(&unquote_yaml(item)));
            if handle.is_empty() {
                return Err(anyhow!(
                    "line {line_no}: empty handle entry under `admin_handles:`"
                ));
            }
            if !cfg.admin_handles.contains(&handle) {
                cfg.admin_handles.push(handle);
            }
            continue;
        }

        let (key, raw_val) = split_key_value(trimmed)
            .ok_or_else(|| anyhow!("invalid YAML at line {line_no}: expected `key: value`"))?;
        let key = unquote_yaml(key);
        let val = raw_val.trim();

        match key.as_str() {
            "host" => {
                let host = expand_env_vars(&unquote_yaml(val));
                if host.is_empty() {
                    return Err(anyhow!("line {line_no}: `host` cannot be empty"));
                }
                cfg.host = host;
            }
            "port" => {
                let raw_port = expand_env_vars(&unquote_yaml(val));
                let parsed = raw_port.parse::<u16>().map_err(|_| {
                    anyhow!("line {line_no}: invalid port `{raw_port}` (expected 1-65535)")
                })?;
                if parsed == 0 {
                    return Err(anyhow!("line {line_no}: port must be between 1 and 65535"));
                }
                cfg.port = parsed;
            }
            "db" => {
                let db = expand_env_vars(&unquote_yaml(val));
                if !db.is_empty() {
                    cfg.db = Some(db);
                }
            }
            "admin_handles" => {
                if val.is_empty() {
                    section = Section::AdminHandles { key_indent: indent };
                } else if let Some(inner) = val.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
                {
                    for part in inner.split(',') {
                        let part = part.trim();
                        if part.is_empty() {
                            continue;
                        }
                        let handle = normalize_admin_handle(&expand_env_vars(&unquote_yaml(part)));
                        if !handle.is_empty() && !cfg.admin_handles.contains(&handle) {
                            cfg.admin_handles.push(handle);
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
                    "line {line_no}: unknown top-level key `{other}` in server.yaml (expected `host`, `port`, `db`, or `admin_handles`)"
                ));
            }
        }
    }

    Ok(cfg)
}

/// Lightweight, dependency-free YAML parser for `~/.config/relaypod/config.yaml` (client / UI / watcher).
/// Supports top-level `url`, `token`, and `watcher:` (`instructions_file`, `global_max_sessions`,
/// `max_consecutive_agent_turns`, `agent_tokens:` flat `name: token` map), single/double quoted
/// strings, `#` comments, and `${ENV_VAR}` expansion.
pub fn parse_yaml_config(input: &str) -> Result<RelayPodConfig> {
    let mut cfg = RelayPodConfig::default();

    #[derive(Debug, PartialEq, Eq)]
    enum Section {
        Root,
        Watcher {
            indent: usize,
        },
        WatcherAgentTokens {
            watcher_indent: usize,
            tokens_indent: usize,
        },
    }

    let mut section = Section::Root;

    for (line_idx, raw_line) in input.lines().enumerate() {
        let line_no = line_idx + 1;
        let stripped = strip_yaml_comment(raw_line);
        if stripped.trim().is_empty() {
            continue;
        }

        let (_, indent) = leading_whitespace(stripped);
        let trimmed = stripped.trim();

        // Unwind nested sections based on indentation
        loop {
            let next = match &section {
                Section::WatcherAgentTokens {
                    watcher_indent,
                    tokens_indent,
                } if indent <= *tokens_indent => Some(Section::Watcher {
                    indent: *watcher_indent,
                }),
                Section::Watcher { indent: w_indent } if indent <= *w_indent => Some(Section::Root),
                _ => None,
            };
            match next {
                Some(popped) => section = popped,
                None => break,
            }
        }

        let (key, raw_val) = split_key_value(trimmed)
            .ok_or_else(|| anyhow!("invalid YAML at line {line_no}: expected `key: value`"))?;
        let key = unquote_yaml(key);
        let val = raw_val.trim();

        match &section {
            Section::Root => match key.as_str() {
                "url" => {
                    let resolved = expand_env_vars(&unquote_yaml(val));
                    if !resolved.is_empty() {
                        cfg.url = Some(resolved);
                    }
                }
                "token" => {
                    let resolved = expand_env_vars(&unquote_yaml(val));
                    if !resolved.is_empty() {
                        cfg.token = Some(resolved);
                    }
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
                        "line {line_no}: unknown top-level key `{other}` in config.yaml (expected `url`, `token`, or `watcher`; server settings belong in server.yaml)"
                    ));
                }
            },
            Section::Watcher {
                indent: watcher_indent,
            } => {
                let watcher_indent = *watcher_indent;
                match key.as_str() {
                    "instructions_file" => {
                        cfg.watcher.instructions_file = Some(expand_env_vars(&unquote_yaml(val)));
                    }
                    "global_max_sessions" => {
                        let parsed = val.parse::<usize>().map_err(|_| {
                            anyhow!(
                                "line {line_no}: invalid integer `{val}` for global_max_sessions"
                            )
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
                    "agent_tokens" => {
                        if !val.is_empty() {
                            return Err(anyhow!(
                                "line {line_no}: `agent_tokens:` must be followed by indented `name: token` entries"
                            ));
                        }
                        section = Section::WatcherAgentTokens {
                            watcher_indent,
                            tokens_indent: indent,
                        };
                    }
                    other => {
                        return Err(anyhow!(
                            "line {line_no}: unknown key `{other}` under `watcher:`"
                        ));
                    }
                }
            }
            Section::WatcherAgentTokens { .. } => {
                let name = key.trim_start_matches('@').to_string();
                if name.is_empty() {
                    return Err(anyhow!(
                        "line {line_no}: empty key under `watcher.agent_tokens:`"
                    ));
                }
                let token = expand_env_vars(&unquote_yaml(val));
                if token.is_empty() {
                    return Err(anyhow!(
                        "line {line_no}: empty token for `{name}` under `watcher.agent_tokens:`"
                    ));
                }
                if cfg.watcher.agent_tokens.contains_key(&name) {
                    return Err(anyhow!(
                        "line {line_no}: duplicate token name `{name}` under `watcher.agent_tokens:`"
                    ));
                }
                cfg.watcher.agent_tokens.insert(name, token);
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

/// Inserts a `name: token` entry into `watcher.agent_tokens` in an existing (or empty)
/// `config.yaml` string, preserving comments, indentation, and sections.
pub fn insert_watcher_token_yaml(
    existing: &str,
    url: &str,
    name: &str,
    token: &str,
) -> Result<String> {
    if !existing.trim().is_empty() {
        let parsed = parse_yaml_config(existing)?;
        if parsed.watcher.agent_tokens.contains_key(name) {
            return Err(anyhow!(
                "watcher token '{name}' already exists in config.yaml"
            ));
        }
    }

    let mut lines: Vec<String> = if existing.is_empty() {
        Vec::new()
    } else {
        existing.lines().map(ToString::to_string).collect()
    };

    // Locate top-level `watcher:` section and check whether top-level `url:` is already set.
    let mut has_root_url = false;
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
                if unquote_yaml(key) == "url" {
                    has_root_url = true;
                }
                if watcher_start.is_some() {
                    watcher_end = idx;
                    break;
                }
                if key == "watcher" {
                    watcher_start = Some(idx);
                }
            }
        }
    }

    match watcher_start {
        None => {
            if !has_root_url && !url.trim().is_empty() {
                if !lines.is_empty() && !lines.last().is_some_and(|l| l.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push(format!("url: {}", yaml_scalar(url)));
            }
            if !lines.is_empty() && !lines.last().is_some_and(|l| l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("watcher:".to_string());
            lines.push("  agent_tokens:".to_string());
            lines.push(format!("    {name}: {}", yaml_scalar(token)));
        }
        Some(w_start) => {
            let mut tokens_start: Option<usize> = None;
            let mut tokens_indent_cols: usize = 0;
            let mut tokens_indent_str = String::from("  ");
            let mut watcher_child_indent: Option<String> = None;

            for (idx, raw) in lines.iter().enumerate().take(watcher_end).skip(w_start + 1) {
                let stripped = strip_yaml_comment(raw);
                let trimmed = stripped.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let (ws, cols) = leading_whitespace(stripped);
                if watcher_child_indent.is_none() && cols > 0 {
                    watcher_child_indent = Some(ws.to_string());
                }
                if let Some((key, _)) = split_key_value(trimmed) {
                    if key == "agent_tokens" {
                        tokens_start = Some(idx);
                        tokens_indent_cols = cols;
                        tokens_indent_str = ws.to_string();
                        break;
                    }
                }
            }

            let step = watcher_child_indent.as_deref().unwrap_or("  ");
            let indent_unit = if step.contains('\t') { "\t" } else { "  " };

            if let Some(t_start) = tokens_start {
                let mut tokens_end = watcher_end;
                let mut entry_indent: Option<String> = None;
                for (idx, raw) in lines.iter().enumerate().take(watcher_end).skip(t_start + 1) {
                    let stripped = strip_yaml_comment(raw);
                    let trimmed = stripped.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let (ws, cols) = leading_whitespace(stripped);
                    if cols <= tokens_indent_cols {
                        tokens_end = idx;
                        break;
                    }
                    if entry_indent.is_none() {
                        entry_indent = Some(ws.to_string());
                    }
                }
                let entry_indent =
                    entry_indent.unwrap_or_else(|| format!("{tokens_indent_str}{indent_unit}"));

                let mut insert_at = tokens_end;
                while insert_at > t_start + 1 && lines[insert_at - 1].trim().is_empty() {
                    insert_at -= 1;
                }
                lines.insert(
                    insert_at,
                    format!("{entry_indent}{name}: {}", yaml_scalar(token)),
                );
            } else {
                let mut insert_at = watcher_end;
                while insert_at > w_start + 1 && lines[insert_at - 1].trim().is_empty() {
                    insert_at -= 1;
                }
                let key_indent = step.to_string();
                let entry_indent = format!("{key_indent}{indent_unit}");
                let block = vec![
                    String::new(),
                    format!("{key_indent}agent_tokens:"),
                    format!("{entry_indent}{name}: {}", yaml_scalar(token)),
                ];
                lines.splice(insert_at..insert_at, block);
            }
        }
    }

    let mut out = lines.join("\n");
    out.push('\n');

    let reparsed = parse_yaml_config(&out)
        .context("internal error: generated config.yaml failed validation")?;
    if reparsed.watcher.agent_tokens.get(name).map(String::as_str) != Some(token) {
        return Err(anyhow!(
            "internal error: newly added token '{name}' missing after config update"
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

pub fn append_watcher_token_to_config(
    path: &Path,
    url: &str,
    name: &str,
    token: &str,
) -> Result<()> {
    let existing = if path.exists() {
        std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?
    } else {
        String::new()
    };

    let updated = insert_watcher_token_yaml(&existing, url, name, token)?;
    write_config_atomic(path, &updated)
}

/// Updates an existing `name: token` entry under `watcher.agent_tokens` in a YAML string.
/// Returns `Ok(None)` if `name` is not configured under `watcher.agent_tokens`.
pub fn update_watcher_token_yaml(
    existing: &str,
    name: &str,
    new_token: &str,
) -> Result<Option<String>> {
    if existing.trim().is_empty() {
        return Ok(None);
    }
    let parsed = parse_yaml_config(existing)?;
    if !parsed.watcher.agent_tokens.contains_key(name) {
        return Ok(None);
    }

    let mut lines: Vec<String> = existing.lines().map(ToString::to_string).collect();

    let mut in_watcher = false;
    let mut watcher_indent = 0usize;
    let mut in_tokens = false;
    let mut tokens_indent = 0usize;
    let mut updated_line = false;

    for raw in &mut lines {
        let stripped = strip_yaml_comment(raw);
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (ws, cols) = leading_whitespace(stripped);

        if in_tokens && cols <= tokens_indent {
            in_tokens = false;
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

        if !in_tokens {
            if let Some((key, _)) = split_key_value(trimmed) {
                if key == "agent_tokens" {
                    in_tokens = true;
                    tokens_indent = cols;
                }
            }
            continue;
        }

        if let Some((key, _)) = split_key_value(trimmed) {
            if unquote_yaml(key).trim_start_matches('@') == name {
                *raw = format!("{ws}{name}: {}", yaml_scalar(new_token));
                updated_line = true;
                break;
            }
        }
    }

    if !updated_line {
        return Err(anyhow!(
            "internal error: token '{name}' found during parse but not during update"
        ));
    }

    let mut out = lines.join("\n");
    out.push('\n');

    let reparsed =
        parse_yaml_config(&out).context("internal error: updated config.yaml failed validation")?;
    if reparsed.watcher.agent_tokens.get(name).map(String::as_str) != Some(new_token) {
        return Err(anyhow!(
            "internal error: updated token for '{name}' did not match after validation"
        ));
    }

    Ok(Some(out))
}

/// Updates an existing `name: token` entry under `watcher.agent_tokens` in `config.yaml` if present.
/// Returns `Ok(true)` if updated, `Ok(false)` if the file or key was not present.
pub fn update_watcher_token_in_config(path: &Path, name: &str, new_token: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let existing = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let Some(updated) = update_watcher_token_yaml(&existing, name, new_token)? else {
        return Ok(false);
    };
    write_config_atomic(path, &updated)?;
    Ok(true)
}

/// Inserts or updates a `name: token` entry under `watcher.agent_tokens` in `config.yaml`.
pub fn upsert_watcher_token_in_config(
    path: &Path,
    url: &str,
    name: &str,
    token: &str,
) -> Result<()> {
    let existing = if path.exists() {
        std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?
    } else {
        String::new()
    };

    let updated = match update_watcher_token_yaml(&existing, name, token)? {
        Some(updated) => updated,
        None => insert_watcher_token_yaml(&existing, url, name, token)?,
    };
    write_config_atomic(path, &updated)
}

/// Removes an existing `name: token` entry under `watcher.agent_tokens` in a YAML string.
/// Returns `Ok(None)` if `name` is not configured under `watcher.agent_tokens`.
pub fn remove_watcher_token_yaml(existing: &str, name: &str) -> Result<Option<String>> {
    if existing.trim().is_empty() {
        return Ok(None);
    }
    let parsed = parse_yaml_config(existing)?;
    if !parsed.watcher.agent_tokens.contains_key(name) {
        return Ok(None);
    }

    let mut lines: Vec<String> = existing.lines().map(ToString::to_string).collect();
    let mut in_watcher = false;
    let mut watcher_indent = 0usize;
    let mut in_tokens = false;
    let mut tokens_indent = 0usize;
    let mut remove_idx: Option<usize> = None;

    for (idx, raw) in lines.iter().enumerate() {
        let stripped = strip_yaml_comment(raw);
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (_, cols) = leading_whitespace(stripped);

        if in_tokens && cols <= tokens_indent {
            in_tokens = false;
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

        if !in_tokens {
            if let Some((key, _)) = split_key_value(trimmed) {
                if key == "agent_tokens" {
                    in_tokens = true;
                    tokens_indent = cols;
                }
            }
            continue;
        }

        if let Some((key, _)) = split_key_value(trimmed) {
            if unquote_yaml(key).trim_start_matches('@') == name {
                remove_idx = Some(idx);
                break;
            }
        }
    }

    let Some(idx) = remove_idx else {
        return Err(anyhow!(
            "internal error: token '{name}' found during parse but not during removal"
        ));
    };
    lines.remove(idx);

    let mut out = lines.join("\n");
    out.push('\n');

    let reparsed =
        parse_yaml_config(&out).context("internal error: updated config.yaml failed validation")?;
    if reparsed.watcher.agent_tokens.contains_key(name) {
        return Err(anyhow!(
            "internal error: token '{name}' still present after removal"
        ));
    }

    Ok(Some(out))
}

/// Removes a `name: token` entry under `watcher.agent_tokens` in `config.yaml` if present.
/// Returns `Ok(true)` if removed, `Ok(false)` if the file or key was not present.
pub fn remove_watcher_token_from_config(path: &Path, name: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let existing = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let Some(updated) = remove_watcher_token_yaml(&existing, name)? else {
        return Ok(false);
    };
    write_config_atomic(path, &updated)?;
    Ok(true)
}

/// Updates or inserts root-level `url:` and (optionally) `token:` in a client `config.yaml` string,
/// preserving comments and the `watcher:` section.
pub fn upsert_client_connection_yaml(
    existing: &str,
    url: &str,
    token: Option<&str>,
) -> Result<String> {
    if !existing.trim().is_empty() {
        let _ = parse_yaml_config(existing)?;
    }

    let mut lines: Vec<String> = if existing.is_empty() {
        Vec::new()
    } else {
        existing.lines().map(ToString::to_string).collect()
    };

    let mut url_idx: Option<usize> = None;
    let mut token_idx: Option<usize> = None;
    let mut watcher_idx: Option<usize> = None;

    for (idx, raw) in lines.iter().enumerate() {
        let stripped = strip_yaml_comment(raw);
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (_, cols) = leading_whitespace(stripped);
        if cols == 0 {
            if let Some((key, _)) = split_key_value(trimmed) {
                match unquote_yaml(key).as_str() {
                    "url" => url_idx = Some(idx),
                    "token" => token_idx = Some(idx),
                    "watcher" if watcher_idx.is_none() => watcher_idx = Some(idx),
                    _ => {}
                }
            }
        }
    }

    let url_line = format!("url: {}", yaml_scalar(url.trim()));
    if let Some(idx) = url_idx {
        lines[idx] = url_line;
    } else {
        let insert_at = watcher_idx.unwrap_or(lines.len());
        lines.insert(insert_at, url_line);
        url_idx = Some(insert_at);
        if let Some(ref mut t_idx) = token_idx {
            if *t_idx >= insert_at {
                *t_idx += 1;
            }
        }
        if let Some(ref mut w_idx) = watcher_idx {
            if *w_idx >= insert_at {
                *w_idx += 1;
            }
        }
    }

    if let Some(tok) = token.map(str::trim).filter(|s| !s.is_empty()) {
        let token_line = format!("token: {}", yaml_scalar(tok));
        if let Some(idx) = token_idx {
            lines[idx] = token_line;
        } else {
            let insert_at = url_idx
                .map(|i| i + 1)
                .or(watcher_idx)
                .unwrap_or(lines.len());
            lines.insert(insert_at, token_line);
        }
    }

    let mut out = lines.join("\n");
    out.push('\n');

    let reparsed =
        parse_yaml_config(&out).context("internal error: updated config.yaml failed validation")?;
    if reparsed.url.as_deref() != Some(url.trim()) {
        return Err(anyhow!(
            "internal error: updated url did not match after validation"
        ));
    }
    if let Some(tok) = token.map(str::trim).filter(|s| !s.is_empty()) {
        if reparsed.token.as_deref() != Some(tok) {
            return Err(anyhow!(
                "internal error: updated token did not match after validation"
            ));
        }
    }

    Ok(out)
}

/// Saves the client's server `url` and optional human `token` to `~/.config/relaypod/config.yaml`.
pub fn upsert_client_connection_in_config(
    path: &Path,
    url: &str,
    token: Option<&str>,
) -> Result<()> {
    let existing = if path.exists() {
        std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?
    } else {
        render_starter_config_yaml()
    };

    let updated = upsert_client_connection_yaml(&existing, url, token)?;
    write_config_atomic(path, &updated)
}

/// Updates root `token:` in `config.yaml` ONLY if a root `token:` is already configured.
/// Returns `Ok(true)` if updated, `Ok(false)` otherwise.
pub fn update_client_token_in_config(path: &Path, new_token: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let existing = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let parsed = parse_yaml_config(&existing)?;
    if parsed.token.is_none() {
        return Ok(false);
    }
    let url = parsed.url.as_deref().unwrap_or(DEFAULT_SERVER_URL);
    let updated = upsert_client_connection_yaml(&existing, url, Some(new_token))?;
    write_config_atomic(path, &updated)?;
    Ok(true)
}

/// Renders the starter client `~/.config/relaypod/config.yaml` for first-time initialization.
pub fn render_starter_config_yaml() -> String {
    format!(
        "# RelayPod client configuration\n\
         # Used by: RelayPod desktop UI, `relaypod` CLI, and `relaypod watch`\n\
         \n\
         url: {DEFAULT_SERVER_URL}\n\
         # Human bearer token (env RELAYPOD_TOKEN takes precedence)\n\
         # token: rp_...\n\
         \n\
         watcher:\n\
         \x20\x20global_max_sessions: 10\n\
         \x20\x20max_consecutive_agent_turns: 6\n\
         \x20\x20# instructions_file: ~/.config/relaypod/agent-instructions.md\n\
         \n\
         \x20\x20# Agent tokens (added automatically when creating an agent in the UI or CLI).\n\
         \x20\x20# All other agent settings (runner, harness, args, cwd, cooldown_secs, etc.)\n\
         \x20\x20# are stored in the server DB and editable live in the RelayPod UI.\n\
         \x20\x20# agent_tokens:\n\
         \x20\x20#   mbp-codex: ${{RELAYPOD_CODEX_TOKEN}}\n"
    )
}

/// Renders the starter server `~/.config/relaypod/server.yaml` for first-time `relaypod serve` initialization.
pub fn render_starter_server_config_yaml(admin_handle: &str) -> String {
    let normalized = normalize_admin_handle(admin_handle);
    let admin = if normalized.is_empty() {
        "admin".to_string()
    } else {
        normalized
    };
    format!(
        "# RelayPod server configuration\n\
         # Used by: relaypod serve\n\
         \n\
         host: {DEFAULT_SERVER_HOST}\n\
         port: {DEFAULT_SERVER_PORT}\n\
         # db: ~/.config/relaypod/relaypod.db\n\
         \n\
         # Human handles granted admin privileges (config-only, never stored in SQLite)\n\
         admin_handles:\n\
         \x20\x20- {}\n",
        yaml_scalar(&admin)
    )
}

/// Ensures `base_dir` (0700), `base_dir/workspace`, and a starter client `base_dir/config.yaml` (0600)
/// exist. Existing `config.yaml` files are never overwritten.
/// Returns `(config_path, created_config)`.
pub fn ensure_relaypod_home_in(base_dir: &Path) -> Result<(std::path::PathBuf, bool)> {
    std::fs::create_dir_all(base_dir)
        .with_context(|| format!("failed to create RelayPod dir {}", base_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(base_dir, std::fs::Permissions::from_mode(0o700));
    }

    let workspace_dir = base_dir.join("workspace");
    std::fs::create_dir_all(&workspace_dir).with_context(|| {
        format!(
            "failed to create RelayPod workspace dir {}",
            workspace_dir.display()
        )
    })?;

    let config_path = base_dir.join("config.yaml");
    if config_path.exists() {
        return Ok((config_path, false));
    }

    let starter = render_starter_config_yaml();
    write_config_atomic(&config_path, &starter)?;
    Ok((config_path, true))
}

/// Ensures `~/.config/relaypod/`, `~/.config/relaypod/workspace/`, and a starter client `~/.config/relaypod/config.yaml`
/// exist on disk without overwriting an existing config.
pub fn ensure_default_config() -> Result<(std::path::PathBuf, bool)> {
    let base_dir = super::default_config_dir()?;
    ensure_relaypod_home_in(&base_dir)
}

/// Ensures `base_dir` (0700) and a starter `base_dir/server.yaml` (0600) exist without overwriting
/// an existing `server.yaml`. Returns `(server_config_path, created)`.
pub fn ensure_server_config_in(
    base_dir: &Path,
    admin_handle: Option<&str>,
) -> Result<(std::path::PathBuf, bool)> {
    std::fs::create_dir_all(base_dir)
        .with_context(|| format!("failed to create RelayPod dir {}", base_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(base_dir, std::fs::Permissions::from_mode(0o700));
    }

    let server_path = base_dir.join("server.yaml");
    if server_path.exists() {
        return Ok((server_path, false));
    }

    let env_user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty());
    let admin = admin_handle
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(env_user.as_deref())
        .unwrap_or("admin");

    let starter = render_starter_server_config_yaml(admin);
    write_config_atomic(&server_path, &starter)?;
    Ok((server_path, true))
}

/// Ensures `~/.config/relaypod/` and a starter `~/.config/relaypod/server.yaml` exist on disk without overwriting
/// an existing `server.yaml`.
pub fn ensure_default_server_config(
    admin_handle: Option<&str>,
) -> Result<(std::path::PathBuf, bool)> {
    let base_dir = super::default_config_dir()?;
    ensure_server_config_in(&base_dir, admin_handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_client_yaml_config() {
        let yaml = r#"
url: http://127.0.0.1:8787
token: rp_human_secret

watcher:
  global_max_sessions: 4
  instructions_file: ~/.config/relaypod/team-norms.md
  agent_tokens:
    mbp-codex: rp_123
    mbp-claude: rp_456
"#;
        let cfg = parse_yaml_config(yaml).unwrap();
        assert_eq!(cfg.url.as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(cfg.token.as_deref(), Some("rp_human_secret"));
        assert_eq!(cfg.watcher.global_max_sessions, Some(4));
        assert_eq!(
            cfg.watcher.instructions_file.as_deref(),
            Some("~/.config/relaypod/team-norms.md")
        );
        assert_eq!(
            cfg.watcher.max_consecutive_agent_turns,
            Some(DEFAULT_MAX_CONSECUTIVE_AGENT_TURNS)
        );
        assert_eq!(cfg.watcher.agent_tokens.len(), 2);
        assert_eq!(
            cfg.watcher
                .agent_tokens
                .get("mbp-codex")
                .map(String::as_str),
            Some("rp_123")
        );
        assert_eq!(
            cfg.watcher
                .agent_tokens
                .get("mbp-claude")
                .map(String::as_str),
            Some("rp_456")
        );
    }

    #[test]
    fn parses_server_yaml_config_with_port_host_db_and_admins() {
        let yaml = r#"
host: 0.0.0.0
port: 9090
db: ~/.config/relaypod/relaypod.db
admin_handles:
  - admin
  - "@Alice"
"#;
        let cfg = parse_server_config_yaml(yaml).unwrap();
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.port, 9090);
        assert_eq!(cfg.db.as_deref(), Some("~/.config/relaypod/relaypod.db"));
        assert_eq!(cfg.admin_handles, vec!["admin", "alice"]);
        assert_eq!(cfg.bind_addr().unwrap().to_string(), "0.0.0.0:9090");

        // Defaults when omitted
        let defaults = parse_server_config_yaml("admin_handles: [bob]\n").unwrap();
        assert_eq!(defaults.host, "127.0.0.1");
        assert_eq!(defaults.port, 8787);
        assert_eq!(defaults.admin_handles, vec!["bob"]);

        // Port 0 rejected
        assert!(parse_server_config_yaml("port: 0\n").is_err());
    }

    #[test]
    fn expands_env_vars_in_values() {
        std::env::set_var("RELAYPOD_TEST_TOKEN_VAR", "rp_from_env");
        let yaml = r#"
url: http://127.0.0.1:8787
watcher:
  agent_tokens:
    bot: ${RELAYPOD_TEST_TOKEN_VAR}
"#;
        let cfg = parse_yaml_config(yaml).unwrap();
        assert_eq!(
            cfg.watcher.agent_tokens.get("bot").map(String::as_str),
            Some("rp_from_env")
        );
    }

    #[test]
    fn parses_max_consecutive_agent_turns() {
        let custom = parse_yaml_config(
            "watcher:\n  max_consecutive_agent_turns: 10\n  agent_tokens:\n    bot: rp_1\n",
        )
        .unwrap();
        assert_eq!(custom.watcher.max_consecutive_agent_turns, Some(10));

        let disabled = parse_yaml_config(
            "watcher:\n  max_consecutive_agent_turns: 0\n  agent_tokens:\n    bot: rp_1\n",
        )
        .unwrap();
        assert_eq!(disabled.watcher.max_consecutive_agent_turns, None);
    }

    #[test]
    fn parses_tab_indented_config() {
        let yaml = "url: http://127.0.0.1:8787\n\nwatcher:\n\tagent_tokens:\n\t  bot: rp_123\n";
        let cfg = parse_yaml_config(yaml).expect("tab-indented config should parse");
        assert_eq!(cfg.url.as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(
            cfg.watcher.agent_tokens.get("bot").map(String::as_str),
            Some("rp_123")
        );
    }

    #[test]
    fn inserts_watcher_token_into_empty_starter_existing_and_tab_configs() {
        // 1. Empty file
        let from_empty =
            insert_watcher_token_yaml("", "http://127.0.0.1:8787", "claude-local", "rp_secret_123")
                .unwrap();
        let cfg = parse_yaml_config(&from_empty).unwrap();
        assert_eq!(cfg.url.as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(
            cfg.watcher
                .agent_tokens
                .get("claude-local")
                .map(String::as_str),
            Some("rp_secret_123")
        );

        // 2. Starter config with commented-out `# agent_tokens:`
        let starter = render_starter_config_yaml();
        let from_starter = insert_watcher_token_yaml(
            &starter,
            "http://127.0.0.1:8787",
            "claude-local",
            "rp_secret_123",
        )
        .unwrap();
        let cfg = parse_yaml_config(&from_starter).unwrap();
        assert_eq!(cfg.url.as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(
            cfg.watcher
                .agent_tokens
                .get("claude-local")
                .map(String::as_str),
            Some("rp_secret_123")
        );

        // 3. Existing `agent_tokens:` block
        let with_two = insert_watcher_token_yaml(
            &from_starter,
            "http://127.0.0.1:8787",
            "codex-local",
            "rp_second",
        )
        .unwrap();
        let cfg = parse_yaml_config(&with_two).unwrap();
        assert_eq!(cfg.watcher.agent_tokens.len(), 2);
        assert_eq!(
            cfg.watcher
                .agent_tokens
                .get("codex-local")
                .map(String::as_str),
            Some("rp_second")
        );

        // 4. Tab-indented config
        let tabbed = "url: http://127.0.0.1:8787\n\nwatcher:\n\tagent_tokens:\n\t  bot: rp_123\n";
        let from_tabbed = insert_watcher_token_yaml(
            tabbed,
            "http://127.0.0.1:8787",
            "claude-local",
            "rp_secret_123",
        )
        .unwrap();
        let cfg = parse_yaml_config(&from_tabbed).unwrap();
        assert_eq!(cfg.watcher.agent_tokens.len(), 2);
        assert_eq!(
            cfg.watcher
                .agent_tokens
                .get("claude-local")
                .map(String::as_str),
            Some("rp_secret_123")
        );

        // 5. Duplicate key is rejected
        assert!(insert_watcher_token_yaml(
            &with_two,
            "http://127.0.0.1:8787",
            "claude-local",
            "rp_dup"
        )
        .is_err());
    }

    #[test]
    fn upserts_client_url_and_token_preserving_watcher() {
        let starter = render_starter_config_yaml();
        let saved =
            upsert_client_connection_yaml(&starter, "http://127.0.0.1:9000", Some("rp_human_1"))
                .unwrap();
        let cfg = parse_yaml_config(&saved).unwrap();
        assert_eq!(cfg.url.as_deref(), Some("http://127.0.0.1:9000"));
        assert_eq!(cfg.token.as_deref(), Some("rp_human_1"));

        let rotated =
            upsert_client_connection_yaml(&saved, "http://127.0.0.1:9000", Some("rp_human_2"))
                .unwrap();
        let cfg2 = parse_yaml_config(&rotated).unwrap();
        assert_eq!(cfg2.token.as_deref(), Some("rp_human_2"));
    }

    #[test]
    fn updates_watcher_token_in_yaml() {
        let yaml = r#"url: http://127.0.0.1:8787

watcher:
  agent_tokens:
    claude-local: rp_old_token
    codex-local: rp_other_token
"#;

        let updated = update_watcher_token_yaml(yaml, "claude-local", "rp_new_token")
            .unwrap()
            .expect("key exists");
        let cfg = parse_yaml_config(&updated).unwrap();
        assert_eq!(
            cfg.watcher
                .agent_tokens
                .get("claude-local")
                .map(String::as_str),
            Some("rp_new_token")
        );
        assert_eq!(
            cfg.watcher
                .agent_tokens
                .get("codex-local")
                .map(String::as_str),
            Some("rp_other_token")
        );

        // Missing key returns Ok(None).
        assert!(update_watcher_token_yaml(yaml, "unknown-bot", "rp_x")
            .unwrap()
            .is_none());
    }

    #[test]
    fn removes_watcher_token_from_yaml() {
        let yaml = r#"url: http://127.0.0.1:8787

watcher:
  agent_tokens:
    claude-local: rp_old_token
    codex-local: rp_other_token
"#;

        let removed = remove_watcher_token_yaml(yaml, "claude-local")
            .unwrap()
            .expect("key exists");
        let cfg = parse_yaml_config(&removed).unwrap();
        assert!(!cfg.watcher.agent_tokens.contains_key("claude-local"));
        assert_eq!(
            cfg.watcher
                .agent_tokens
                .get("codex-local")
                .map(String::as_str),
            Some("rp_other_token")
        );

        let removed_last = remove_watcher_token_yaml(&removed, "codex-local")
            .unwrap()
            .expect("second key exists");
        let cfg_empty = parse_yaml_config(&removed_last).unwrap();
        assert!(cfg_empty.watcher.agent_tokens.is_empty());

        assert!(remove_watcher_token_yaml(yaml, "missing-bot")
            .unwrap()
            .is_none());
    }

    #[test]
    fn initializes_client_and_server_configs_idempotently() {
        let base =
            std::env::temp_dir().join(format!("relaypod-home-init-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        // Client config.yaml
        let (cfg_path, created) = ensure_relaypod_home_in(&base).unwrap();
        assert!(created);
        assert!(cfg_path.is_file());
        assert!(base.join("workspace").is_dir());

        let raw = std::fs::read_to_string(&cfg_path).unwrap();
        let parsed = parse_yaml_config(&raw).unwrap();
        assert_eq!(parsed.url.as_deref(), Some("http://127.0.0.1:8787"));
        assert!(parsed.token.is_none());
        assert!(parsed.watcher.agent_tokens.is_empty());

        let with_tok =
            insert_watcher_token_yaml(&raw, "http://127.0.0.1:8787", "claude", "rp_abc").unwrap();
        std::fs::write(&cfg_path, &with_tok).unwrap();
        let (_, created_again) = ensure_relaypod_home_in(&base).unwrap();
        assert!(!created_again);
        let after = parse_yaml_config(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        assert_eq!(
            after.watcher.agent_tokens.get("claude").map(String::as_str),
            Some("rp_abc")
        );

        // Server server.yaml
        let (srv_path, srv_created) = ensure_server_config_in(&base, Some("alice")).unwrap();
        assert!(srv_created);
        assert!(srv_path.is_file());
        let srv = load_server_config_file(&srv_path).unwrap();
        assert_eq!(srv.host, "127.0.0.1");
        assert_eq!(srv.port, 8787);
        assert_eq!(srv.admin_handles, vec!["alice"]);

        let (_, srv_created_again) = ensure_server_config_in(&base, Some("bob")).unwrap();
        assert!(!srv_created_again);
        let srv_after = load_server_config_file(&srv_path).unwrap();
        assert_eq!(srv_after.admin_handles, vec!["alice"]);

        let _ = std::fs::remove_dir_all(&base);
    }
}
