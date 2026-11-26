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
    /// Flat map of unique local names to agent bearer tokens (e.g. `mbp-codex: rp_...`).
    pub agent_tokens: BTreeMap<String, String>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            url: None,
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

/// Lightweight, dependency-free YAML parser for `~/.relaypod/config.yaml`.
/// Supports top-level `server:` (`admin_handles:`) and `watcher:` (`url`, `instructions_file`,
/// `global_max_sessions`, `max_consecutive_agent_turns`, `agent_tokens:` flat `name: token` map),
/// single/double quoted strings, `#` comments, and `${ENV_VAR}` expansion.
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

        let indent: usize = stripped
            .bytes()
            .take_while(|&b| b == b' ' || b == b'\t')
            .map(|b| if b == b'\t' { 4 } else { 1 })
            .sum();
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
            if !lines.is_empty() && !lines.last().is_some_and(|l| l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("watcher:".to_string());
            lines.push(format!("  url: {}", yaml_scalar(url)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_token_yaml_config() {
        let yaml = r#"
server:
  admin_handles:
    - admin
    - "@Alice"

watcher:
  url: http://127.0.0.1:8787
  global_max_sessions: 4
  instructions_file: ~/.relaypod/team-norms.md
  agent_tokens:
    mbp-codex: rp_123
    mbp-claude: rp_456
"#;
        let cfg = parse_yaml_config(yaml).unwrap();
        assert_eq!(cfg.server.admin_handles, vec!["admin", "alice"]);
        assert_eq!(cfg.watcher.url.as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(cfg.watcher.global_max_sessions, Some(4));
        assert_eq!(
            cfg.watcher.instructions_file.as_deref(),
            Some("~/.relaypod/team-norms.md")
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
    fn expands_env_vars_in_values() {
        std::env::set_var("RELAYPOD_TEST_TOKEN_VAR", "rp_from_env");
        let yaml = r#"
watcher:
  url: http://127.0.0.1:8787
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
        let yaml = "server:\n\tadmin_handles:\n\t- alice\n\nwatcher:\n\turl: http://127.0.0.1:8787\n\tagent_tokens:\n\t  bot: rp_123\n";
        let cfg = parse_yaml_config(yaml).expect("tab-indented config should parse");
        assert_eq!(cfg.server.admin_handles, vec!["alice"]);
        assert_eq!(cfg.watcher.url.as_deref(), Some("http://127.0.0.1:8787"));
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
        assert_eq!(cfg.watcher.url.as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(
            cfg.watcher
                .agent_tokens
                .get("claude-local")
                .map(String::as_str),
            Some("rp_secret_123")
        );

        // 2. Starter config with commented-out `# agent_tokens:`
        let starter = "server:\n  admin_handles:\n    - alice\n\nwatcher:\n  url: http://127.0.0.1:8787\n  # agent_tokens:\n  #   example: rp_...\n";
        let from_starter = insert_watcher_token_yaml(
            starter,
            "http://127.0.0.1:8787",
            "claude-local",
            "rp_secret_123",
        )
        .unwrap();
        let cfg = parse_yaml_config(&from_starter).unwrap();
        assert_eq!(cfg.server.admin_handles, vec!["alice"]);
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
        let tabbed = "server:\n\tadmin_handles:\n\t- alice\n\nwatcher:\n\turl: http://127.0.0.1:8787\n\tagent_tokens:\n\t  bot: rp_123\n";
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
    fn updates_watcher_token_in_yaml() {
        let yaml = r#"server:
  admin_handles:
    - alice

watcher:
  url: http://127.0.0.1:8787
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
        let yaml = r#"server:
  admin_handles:
    - alice

watcher:
  url: http://127.0.0.1:8787
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
}
