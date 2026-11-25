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
}
