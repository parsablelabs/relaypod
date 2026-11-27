mod mcp;

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand};
use relaypod_client::{mcp_config_snippet, watch, RelayPodClient};
use relaypod_core::{
    AgentConfig, CreateChannelRequest, CreateIdentityRequest, IdentityKind, Store,
    UpdateChannelRequest, UpdateIdentityRequest,
};
use std::io::{self, IsTerminal, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "relaypod",
    version,
    about = "Shared message board for humans and AI agents"
)]
struct Cli {
    /// RelayPod server URL (overrides ~/.config/relaypod/config.yaml `url`)
    #[arg(long, env = "RELAYPOD_URL")]
    url: Option<String>,

    /// Bearer token for authenticating with RelayPod (overrides ~/.config/relaypod/config.yaml `token`)
    #[arg(long, env = "RELAYPOD_TOKEN")]
    token: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the RelayPod HTTP + WebSocket server
    Serve {
        /// Full socket address to bind to (overrides host/port in ~/.config/relaypod/server.yaml)
        #[arg(long, conflicts_with = "port")]
        bind: Option<SocketAddr>,
        /// TCP port to listen on (overrides `port` in ~/.config/relaypod/server.yaml; default: 8787)
        #[arg(long)]
        port: Option<u16>,
        /// Path to the SQLite database file (overrides `db` in ~/.config/relaypod/server.yaml; default: relaypod.db)
        #[arg(long)]
        db: Option<PathBuf>,
        /// Handle for the initial human identity if the database is empty
        #[arg(long, default_value = "admin")]
        admin: String,
        /// Path to server YAML config file (defaults to ~/.config/relaypod/server.yaml)
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Run as a stdio MCP server for agent harnesses (Claude, Codex, Antigravity, etc.)
    Mcp,
    /// Watch for unread mentions/replies over WebSocket and execute configured commands
    Watch {
        /// Path to YAML config file (defaults to ~/.config/relaypod/config.yaml)
        #[arg(long)]
        config: Option<PathBuf>,
        /// Only watch a specific handle from the config file
        #[arg(long)]
        handle: Option<String>,
        /// Shell command to execute on each unread event (overrides config `exec` or runs standalone without a config file)
        #[arg(long)]
        exec: Option<String>,
        /// Working directory for the executed command
        #[arg(long)]
        cwd: Option<String>,
        /// Automatically ack the unread item when the command exits 0
        #[arg(long)]
        auto_ack: bool,
        /// Do not drain existing unacked items on startup (only react to new events)
        #[arg(long)]
        no_drain: bool,
    },
    /// Show the currently authenticated identity
    Me,
    /// Manage human and agent identities
    Identity {
        #[command(subcommand)]
        action: IdentityAction,
    },
    /// Manage channels
    Channel {
        #[command(subcommand)]
        action: ChannelAction,
    },
    /// Post a message to a channel or thread
    Post {
        /// Channel name or ID
        channel: String,
        /// Message text
        body: String,
        /// Optional parent message ID to reply in a thread
        #[arg(long)]
        thread: Option<String>,
    },
    /// Read recent top-level messages from a channel
    Read {
        /// Channel name or ID
        channel: String,
        /// Max messages to fetch
        #[arg(long, default_value_t = 25)]
        limit: usize,
    },
    /// Read a message thread
    Thread {
        /// Parent (or reply) message ID
        message_id: String,
    },
    /// Delete a message (and its thread replies if it is a parent message)
    Delete {
        /// Message ID to delete
        message_id: String,
    },
    /// Inspect or acknowledge items in your unread queue
    Unread {
        /// Acknowledge the given unread item ID
        #[arg(long, conflicts_with = "ack_all")]
        ack: Option<String>,
        /// Acknowledge all pending unread items
        #[arg(long)]
        ack_all: bool,
    },
}

#[derive(Args)]
struct IdentityCreateArgs {
    /// Handle for the new identity (without leading '@')
    handle: String,
    /// Identity kind: 'agent' or 'human'
    #[arg(long, default_value = "agent")]
    kind: String,
    /// Do not add a watcher handle entry to ~/.config/relaypod/config.yaml
    #[arg(long)]
    omit_config: bool,
    /// Path to YAML config file to update (defaults to ~/.config/relaypod/config.yaml)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Watcher runner for the agent: 'herdr' or 'exec'
    #[arg(long, default_value = "herdr")]
    runner: String,
    /// Agent harness type (e.g. 'claude', 'codex', 'gemini', 'agy', 'opencode', 'cursor', 'pi', or 'custom')
    #[arg(long, alias = "harness")]
    agent_type: Option<String>,
    /// Short agent role/capability description (stored in DB for routing/display)
    #[arg(long)]
    description: Option<String>,
    /// Custom shell command when using --runner exec without --agent-type
    #[arg(long)]
    exec: Option<String>,
    /// Working directory for the agent (defaults to ~/.config/relaypod/workspace)
    #[arg(long)]
    cwd: Option<String>,
    /// Extra CLI flags forwarded to the agent (stored in DB and applied on new sessions)
    #[arg(long)]
    args: Option<String>,
    /// Optional per-handle custom prompt template
    #[arg(long)]
    prompt: Option<String>,
    /// Optional per-handle instructions file path
    #[arg(long)]
    instructions_file: Option<String>,
    /// Optional per-handle max concurrent sessions
    #[arg(long)]
    max_sessions: Option<usize>,
    /// Automatically ack unread items on exit 0 (default: false)
    #[arg(long)]
    auto_ack: bool,
    /// Do not drain existing unacked items on startup/reconnect
    #[arg(long)]
    no_drain: bool,
    /// Optional pause in seconds after each dispatch before handling the next unread item
    #[arg(long)]
    cooldown_secs: Option<u64>,
}

#[derive(Args)]
struct IdentityUpdateArgs {
    /// Current agent handle or ID
    identity: String,
    /// New handle for the agent
    #[arg(long)]
    handle: Option<String>,
    /// Runner: 'herdr' or 'exec'; pass "" to clear
    #[arg(long)]
    runner: Option<String>,
    /// Harness type (e.g. 'claude', 'codex', 'gemini', 'agy', 'pi'); pass "" to clear
    #[arg(long, alias = "agent-type")]
    harness: Option<String>,
    /// Extra CLI flags forwarded to the harness on new sessions; pass "" to clear
    #[arg(long)]
    args: Option<String>,
    /// Custom shell command for --runner exec; pass "" to clear
    #[arg(long)]
    exec: Option<String>,
    /// Working directory for the agent; pass "" to clear
    #[arg(long)]
    cwd: Option<String>,
    /// Custom prompt template; pass "" to clear
    #[arg(long)]
    prompt: Option<String>,
    /// Instructions file path; pass "" to clear
    #[arg(long)]
    instructions_file: Option<String>,
    /// Max concurrent sessions (0 to clear)
    #[arg(long)]
    max_sessions: Option<usize>,
    /// Automatically ack unread items on exit 0
    #[arg(long)]
    auto_ack: Option<bool>,
    /// Drain existing unacked items on startup/reconnect
    #[arg(long)]
    drain_on_start: Option<bool>,
    /// Pause in seconds after each dispatch (0 for none)
    #[arg(long)]
    cooldown_secs: Option<u64>,
    /// Short role/capability description; pass "" to clear
    #[arg(long)]
    description: Option<String>,
}

#[derive(Subcommand)]
enum IdentityAction {
    /// List all active human and agent identities
    List,
    /// Register a new human (admin only) or agent identity.
    /// For agents, stores agent config in the server DB and writes only the token to ~/.config/relaypod/config.yaml unless --omit-config is passed.
    Create(Box<IdentityCreateArgs>),
    /// Rename an agent identity you registered
    Rename {
        /// Current agent handle or ID
        identity: String,
        /// New handle for the agent
        handle: String,
    },
    /// Update an agent identity's handle and/or DB-backed config
    Update(Box<IdentityUpdateArgs>),
    /// Delete an agent identity you registered (or a human identity, if admin)
    Delete {
        /// Identity handle or ID to delete
        identity: String,
    },
    /// Regenerate the bearer token for an identity (immediately invalidates the old token)
    #[command(alias = "rotate-token")]
    RegenerateToken {
        /// Identity handle or ID
        identity: String,
        /// Do not update the token in ~/.config/relaypod/config.yaml if present
        #[arg(long)]
        omit_config: bool,
        /// Path to YAML config file to update (defaults to ~/.config/relaypod/config.yaml)
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ChannelAction {
    /// List channels on the board
    List {
        /// Include archived read-only channels
        #[arg(long)]
        include_archived: bool,
    },
    /// Create a new channel
    Create {
        /// Channel name (without leading '#')
        name: String,
        /// Optional channel description
        #[arg(long, default_value = "")]
        description: String,
    },
    /// Update a channel's name and/or description
    Update {
        /// Channel name or ID
        channel: String,
        /// New channel name
        #[arg(long, required_unless_present = "description")]
        name: Option<String>,
        /// New channel description
        #[arg(long)]
        description: Option<String>,
    },
    /// Archive a channel (makes it read-only)
    Archive {
        /// Channel name or ID
        channel: String,
    },
    /// Unarchive a previously archived channel
    Unarchive {
        /// Channel name or ID
        channel: String,
    },
    /// Permanently delete a channel and all its messages
    Delete {
        /// Channel name or ID
        channel: String,
        /// Confirm permanent deletion of the channel and all its messages.
        #[arg(long)]
        yes: bool,
    },
}

fn resolve_client_url(cli: &Cli) -> String {
    if let Some(ref url) = cli.url {
        return url.clone();
    }
    if let Ok(Some((_, cfg))) = watch::load_optional_config(None) {
        if let Some(url) = cfg.url {
            return url;
        }
    }
    watch::DEFAULT_SERVER_URL.to_string()
}

fn require_client(cli: &Cli) -> Result<RelayPodClient> {
    let url = resolve_client_url(cli);
    let token = match cli.token.clone() {
        Some(tok) => tok,
        None => watch::load_optional_config(None)?
            .and_then(|(_, cfg)| cfg.token)
            .ok_or_else(|| {
                anyhow!(
                    "RELAYPOD_TOKEN (or --token, or `token:` in ~/.config/relaypod/config.yaml) is required for this command"
                )
            })?,
    };
    Ok(RelayPodClient::new(url, token))
}

/// MCP servers run on behalf of an agent harness and must receive an explicit token via
/// `RELAYPOD_TOKEN` or `--token` so they never silently inherit the human's `config.yaml` token.
fn require_mcp_client(cli: &Cli) -> Result<RelayPodClient> {
    let token = cli
        .token
        .clone()
        .ok_or_else(|| anyhow!("RELAYPOD_TOKEN (or --token) is required for `relaypod mcp`"))?;
    Ok(RelayPodClient::new(resolve_client_url(cli), token))
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

fn default_exec_for_harness(harness: &str) -> String {
    let prompt_msg = "New RelayPod message in #$RELAYPOD_CHANNEL from @$RELAYPOD_AUTHOR (thread $RELAYPOD_THREAD_ID, unread $RELAYPOD_UNREAD_ID). Reply via RelayPod MCP and ack $RELAYPOD_UNREAD_ID.";
    match harness {
        "codex" => format!(r#"codex exec "{prompt_msg}""#),
        "claude" | "claude-code" => format!(r#"claude -p "{prompt_msg}""#),
        "gemini" => format!(r#"gemini -p "{prompt_msg}""#),
        "agy" => format!(r#"agy -p "{prompt_msg}""#),
        "pi" => format!(r#"pi -p "{prompt_msg}""#),
        other => format!(r#"{other} "{prompt_msg}""#),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Serve {
            bind,
            port,
            db,
            admin,
            config,
        } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "relaypod_server=info,relaypod_cli=info".into()),
                )
                .init();

            if config.is_none() {
                if let Ok((path, true)) = watch::ensure_default_server_config(Some(admin)) {
                    tracing::info!(
                        path = %path.display(),
                        admin = %admin,
                        "Initialized default RelayPod server config"
                    );
                }
            }

            let server_cfg = match watch::load_optional_server_config(config.as_deref())? {
                Some((path, cfg)) => {
                    tracing::info!(
                        path = %path.display(),
                        host = %cfg.host,
                        port = cfg.port,
                        admins = ?cfg.admin_handles,
                        "Loaded server config"
                    );
                    cfg
                }
                None => watch::ServerConfig::default(),
            };

            let effective_bind: SocketAddr = match bind {
                Some(addr) => *addr,
                None => {
                    let mut cfg_for_bind = server_cfg.clone();
                    if let Some(p) = port {
                        if *p == 0 {
                            return Err(anyhow!("--port must be between 1 and 65535"));
                        }
                        cfg_for_bind.port = *p;
                    }
                    cfg_for_bind.bind_addr()?
                }
            };

            let effective_db: PathBuf = match db {
                Some(path) => path.clone(),
                None => match server_cfg.db.as_deref() {
                    Some(cfg_db) => PathBuf::from(watch::expand_tilde(cfg_db)),
                    None => PathBuf::from("relaypod.db"),
                },
            };

            let admin_handles = server_cfg.admin_handles;
            if admin_handles.is_empty() {
                tracing::warn!(
                    "No admin_handles configured in ~/.config/relaypod/server.yaml; no human handles will have admin privileges"
                );
            }

            let store = Arc::new(Store::open(&effective_db)?);
            if let Some(created) = store.bootstrap(admin)? {
                println!("============================================================");
                println!("Initialized RelayPod workspace with default #general channel");
                println!("Initial handle : @{}", created.identity.handle);
                println!("Initial token  : {}", created.token);
                println!("Connect via the RelayPod desktop UI or export for CLI:");
                println!("  export RELAYPOD_URL=http://{effective_bind}");
                println!("  export RELAYPOD_TOKEN={}", created.token);
                println!("============================================================");
            }

            relaypod_server::serve(effective_bind, store, admin_handles).await?;
        }
        Commands::Mcp => {
            let client = require_mcp_client(&cli)?;
            mcp::run_mcp_stdio(client).await?;
        }
        Commands::Watch {
            config,
            handle,
            exec,
            cwd,
            auto_ack,
            no_drain,
        } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                        "relaypod_client=info,relaypod_cli=info,relaypod=info".into()
                    }),
                )
                .init();

            watch::run_watch(watch::WatchOptions {
                default_url: resolve_client_url(&cli),
                default_token: cli.token.clone(),
                config_path: config.clone(),
                handle_filter: handle.clone(),
                exec_override: exec.clone(),
                cwd_override: cwd.clone(),
                auto_ack_override: if *auto_ack { Some(true) } else { None },
                no_drain: *no_drain,
            })
            .await?;
        }
        Commands::Me => {
            let client = require_client(&cli)?;
            let me = client.me().await?;
            println!("{}", serde_json::to_string_pretty(&me)?);
        }
        Commands::Identity { action } => {
            let client = require_client(&cli)?;
            match action {
                IdentityAction::List => {
                    let list = client.list_identities().await?;
                    for id in list {
                        let admin_tag = if id.is_admin { " [admin]" } else { "" };
                        let status = if id.busy {
                            "busy"
                        } else if id.online {
                            "online"
                        } else {
                            "offline"
                        };
                        let mut details = Vec::new();
                        if let Some(ref cfg) = id.agent_config {
                            if let Some(ref r) = cfg.runner {
                                details.push(format!("runner={r}"));
                            }
                            if let Some(ref h) = cfg.harness {
                                details.push(format!("harness={h}"));
                            }
                            if let Some(ref a) = cfg.args {
                                details.push(format!("args={a}"));
                            }
                        }
                        let detail_suffix = if details.is_empty() {
                            String::new()
                        } else {
                            format!(" {{{}}}", details.join(", "))
                        };
                        println!(
                            "@{} [{}]{} ({}) unread={}{}",
                            id.handle, id.kind, admin_tag, status, id.unread_count, detail_suffix
                        );
                    }
                }
                IdentityAction::Create(create_args) => {
                    let IdentityCreateArgs {
                        handle,
                        kind,
                        omit_config,
                        config,
                        runner,
                        agent_type,
                        description,
                        exec,
                        cwd,
                        args,
                        prompt,
                        instructions_file,
                        max_sessions,
                        auto_ack,
                        no_drain,
                        cooldown_secs,
                    } = create_args.as_ref();
                    let parsed_kind = kind.parse::<IdentityKind>().map_err(|e| anyhow!(e))?;

                    // Validate/prompt watcher config inputs before creating the remote identity so
                    // we never leave an orphaned agent if flags are missing/invalid.
                    let watcher_plan = if parsed_kind == IdentityKind::Agent && !*omit_config {
                        let parsed_runner = watch::RunnerKind::parse(runner).ok_or_else(|| {
                            anyhow!("invalid --runner '{runner}' (expected 'herdr' or 'exec')")
                        })?;

                        let mut raw_harness = agent_type.clone();
                        let mut resolved_exec = exec.clone();

                        // If harness/exec weren't passed on the command line, prompt interactively
                        // when attached to a TTY.
                        if raw_harness.is_none()
                            && resolved_exec.is_none()
                            && io::stdin().is_terminal()
                        {
                            let input = prompt_line(
                                "Harness type (claude, codex, gemini, agy, opencode, cursor, pi, custom): ",
                            )?;
                            if !input.is_empty() {
                                raw_harness = Some(input);
                            }
                        }

                        let (parsed_agent_type, final_exec) = match raw_harness
                            .as_deref()
                            .map(|s| s.trim().to_lowercase())
                            .as_deref()
                        {
                            Some("custom") => {
                                if parsed_runner == watch::RunnerKind::Herdr {
                                    return Err(anyhow!(
                                        "'custom' harness requires --runner exec (and --exec <cmd>)"
                                    ));
                                }
                                if resolved_exec.is_none() && io::stdin().is_terminal() {
                                    let cmd = prompt_line("Exec command: ")?;
                                    if !cmd.is_empty() {
                                        resolved_exec = Some(cmd);
                                    }
                                }
                                if resolved_exec.is_none() {
                                    return Err(anyhow!(
                                        "--exec <cmd> is required for custom harness"
                                    ));
                                }
                                (None, resolved_exec)
                            }
                            Some(h) => {
                                let at = watch::AgentType::parse(h)
                                    .ok_or_else(|| anyhow!("invalid --agent-type '{h}'"))?;
                                match parsed_runner {
                                    watch::RunnerKind::Herdr => (Some(at), resolved_exec),
                                    watch::RunnerKind::Exec => match at {
                                        // ExecAdapter natively manages codex, claude, gemini, agy, and pi.
                                        watch::AgentType::Codex | watch::AgentType::Claude => {
                                            (Some(at), resolved_exec)
                                        }
                                        watch::AgentType::Other(ref other)
                                            if matches!(
                                                other.as_str(),
                                                "gemini" | "agy" | "pi"
                                            ) =>
                                        {
                                            (Some(at), resolved_exec)
                                        }
                                        // Other harnesses run via custom shell `exec:` with a sensible default.
                                        watch::AgentType::Other(ref other) => {
                                            let default_cmd = default_exec_for_harness(other);
                                            if resolved_exec.is_none() {
                                                if io::stdin().is_terminal() {
                                                    let input = prompt_line(&format!(
                                                        "Exec command [{default_cmd}]: "
                                                    ))?;
                                                    resolved_exec = Some(if input.is_empty() {
                                                        default_cmd
                                                    } else {
                                                        input
                                                    });
                                                } else {
                                                    resolved_exec = Some(default_cmd);
                                                }
                                            }
                                            (None, resolved_exec)
                                        }
                                    },
                                }
                            }
                            None => match parsed_runner {
                                watch::RunnerKind::Herdr => {
                                    return Err(anyhow!(
                                        "--agent-type (or --harness) is required for --runner herdr (e.g. claude, codex, gemini, agy), or pass --omit-config"
                                    ));
                                }
                                watch::RunnerKind::Exec => {
                                    if resolved_exec.is_none() {
                                        return Err(anyhow!(
                                            "--agent-type (claude|codex|...) or --exec is required for --runner exec, or pass --omit-config"
                                        ));
                                    }
                                    (None, resolved_exec)
                                }
                            },
                        };

                        let resolved_cwd = Some(
                            cwd.clone()
                                .unwrap_or_else(|| relaypod_core::DEFAULT_AGENT_CWD.to_string()),
                        );
                        let cfg_path = match config {
                            Some(p) => p.clone(),
                            None => watch::default_config_path()?,
                        };
                        Some((
                            cfg_path,
                            parsed_runner,
                            parsed_agent_type,
                            final_exec,
                            resolved_cwd,
                        ))
                    } else {
                        None
                    };

                    let (db_runner, db_harness, db_exec, db_cwd) = if let Some((
                        _,
                        parsed_runner,
                        ref parsed_at,
                        ref final_exec,
                        ref resolved_cwd,
                    )) = watcher_plan
                    {
                        let r = match parsed_runner {
                            watch::RunnerKind::Herdr => "herdr",
                            watch::RunnerKind::Exec => "exec",
                        };
                        (
                            Some(r.to_string()),
                            parsed_at.as_ref().map(|t| t.as_herdr_kind().to_string()),
                            final_exec.clone(),
                            resolved_cwd.clone(),
                        )
                    } else {
                        (
                            Some(runner.clone()),
                            agent_type.clone(),
                            exec.clone(),
                            cwd.clone(),
                        )
                    };
                    let agent_config = if parsed_kind == IdentityKind::Agent {
                        let cfg = AgentConfig {
                            runner: db_runner,
                            harness: db_harness,
                            args: args.clone(),
                            exec: db_exec,
                            cwd: db_cwd,
                            prompt: prompt.clone(),
                            instructions_file: instructions_file.clone(),
                            max_sessions: *max_sessions,
                            auto_ack: Some(*auto_ack),
                            drain_on_start: Some(!*no_drain),
                            cooldown_secs: *cooldown_secs,
                            description: description.clone(),
                        };
                        if cfg.is_empty() {
                            None
                        } else {
                            Some(cfg)
                        }
                    } else {
                        None
                    };

                    let created = client
                        .create_identity(&CreateIdentityRequest {
                            handle: handle.clone(),
                            kind: parsed_kind,
                            owner_id: None,
                            agent_config,
                        })
                        .await?;
                    println!(
                        "Created @{} ({})",
                        created.identity.handle, created.identity.kind
                    );
                    println!("Token: {}", created.token);
                    if let Some(ref cfg) = created.identity.agent_config {
                        if let Some(ref saved_args) = cfg.args {
                            println!("Args: {saved_args}");
                        }
                    }

                    let effective_url = resolve_client_url(&cli);
                    if let Some((cfg_path, _, _, _, _)) = watcher_plan {
                        watch::append_watcher_token_to_config(
                            &cfg_path,
                            &effective_url,
                            &created.identity.handle,
                            &created.token,
                        )?;
                        println!(
                            "Added token '{}' to watcher.agent_tokens in {}",
                            created.identity.handle,
                            cfg_path.display()
                        );
                    }

                    if created.identity.kind == IdentityKind::Agent {
                        println!("\nMCP config snippet:");
                        println!("{}", mcp_config_snippet(&effective_url, &created.token));
                    }
                }
                IdentityAction::Rename { identity, handle } => {
                    let updated = client
                        .update_identity(
                            identity,
                            &UpdateIdentityRequest {
                                handle: Some(handle.clone()),
                                agent_config: None,
                            },
                        )
                        .await?;
                    println!("Renamed identity {} to @{}", updated.id, updated.handle);
                }
                IdentityAction::Update(update_args) => {
                    let IdentityUpdateArgs {
                        identity,
                        handle,
                        runner,
                        harness,
                        args,
                        exec,
                        cwd,
                        prompt,
                        instructions_file,
                        max_sessions,
                        auto_ack,
                        drain_on_start,
                        cooldown_secs,
                        description,
                    } = update_args.as_ref();
                    let has_config_change = runner.is_some()
                        || harness.is_some()
                        || args.is_some()
                        || exec.is_some()
                        || cwd.is_some()
                        || prompt.is_some()
                        || instructions_file.is_some()
                        || max_sessions.is_some()
                        || auto_ack.is_some()
                        || drain_on_start.is_some()
                        || cooldown_secs.is_some()
                        || description.is_some();
                    if handle.is_none() && !has_config_change {
                        return Err(anyhow!(
                            "specify at least one field to update (e.g. --handle, --runner, --harness, --args, --cooldown-secs, --description)"
                        ));
                    }

                    let agent_config = if has_config_change {
                        let all = client.list_identities().await?;
                        let normalized = identity.trim().trim_start_matches('@').to_lowercase();
                        let existing = all
                            .into_iter()
                            .find(|i| i.id == *identity || i.handle.to_lowercase() == normalized)
                            .and_then(|i| i.agent_config)
                            .unwrap_or_default();
                        Some(AgentConfig {
                            runner: runner.clone().or(existing.runner),
                            harness: harness.clone().or(existing.harness),
                            args: args.clone().or(existing.args),
                            exec: exec.clone().or(existing.exec),
                            cwd: cwd.clone().or(existing.cwd),
                            prompt: prompt.clone().or(existing.prompt),
                            instructions_file: instructions_file
                                .clone()
                                .or(existing.instructions_file),
                            max_sessions: match max_sessions {
                                Some(0) => None,
                                Some(n) => Some(*n),
                                None => existing.max_sessions,
                            },
                            auto_ack: auto_ack.or(existing.auto_ack),
                            drain_on_start: drain_on_start.or(existing.drain_on_start),
                            cooldown_secs: cooldown_secs.or(existing.cooldown_secs),
                            description: description.clone().or(existing.description),
                        })
                    } else {
                        None
                    };

                    let updated = client
                        .update_identity(
                            identity,
                            &UpdateIdentityRequest {
                                handle: handle.clone(),
                                agent_config,
                            },
                        )
                        .await?;
                    println!("Updated @{} ({})", updated.handle, updated.id);
                    if let Some(ref cfg) = updated.agent_config {
                        println!("{}", serde_json::to_string_pretty(cfg)?);
                    }
                }
                IdentityAction::Delete { identity } => {
                    client.delete_identity(identity).await?;
                    println!("Deleted identity {identity}");
                }
                IdentityAction::RegenerateToken {
                    identity,
                    omit_config,
                    config,
                } => {
                    let regenerated = client.regenerate_identity_token(identity).await?;
                    println!(
                        "Regenerated token for @{} ({})",
                        regenerated.identity.handle, regenerated.identity.kind
                    );
                    println!("New token: {}", regenerated.token);
                    println!("Previous token is now invalid.");

                    if regenerated.identity.kind == IdentityKind::Agent && !*omit_config {
                        let cfg_path = match config {
                            Some(p) => Some(p.clone()),
                            None => watch::default_config_path().ok(),
                        };
                        if let Some(cfg_path) = cfg_path {
                            if watch::update_watcher_token_in_config(
                                &cfg_path,
                                &regenerated.identity.handle,
                                &regenerated.token,
                            )? {
                                println!(
                                    "Updated token '{}' in {} (restart 'relaypod watch' to pick it up)",
                                    regenerated.identity.handle,
                                    cfg_path.display()
                                );
                            }
                        }
                    }

                    if regenerated.identity.kind == IdentityKind::Agent {
                        let effective_url = resolve_client_url(&cli);
                        println!("\nMCP config snippet:");
                        println!("{}", mcp_config_snippet(&effective_url, &regenerated.token));
                    }
                }
            }
        }
        Commands::Channel { action } => {
            let client = require_client(&cli)?;
            match action {
                ChannelAction::List { include_archived } => {
                    let channels = if *include_archived {
                        client.list_channels_including_archived().await?
                    } else {
                        client.list_channels().await?
                    };
                    for ch in channels {
                        let status = if ch.archived { " [archived]" } else { "" };
                        println!("#{} ({}){status} — {}", ch.name, ch.id, ch.description);
                    }
                }
                ChannelAction::Create { name, description } => {
                    let ch = client
                        .create_channel(&CreateChannelRequest {
                            name: name.clone(),
                            description: description.clone(),
                        })
                        .await?;
                    println!("Created #{} ({})", ch.name, ch.id);
                }
                ChannelAction::Update {
                    channel,
                    name,
                    description,
                } => {
                    let ch = client
                        .update_channel(
                            channel,
                            &UpdateChannelRequest {
                                name: name.clone(),
                                description: description.clone(),
                                ..Default::default()
                            },
                        )
                        .await?;
                    println!("Updated #{} ({})", ch.name, ch.id);
                }
                ChannelAction::Archive { channel } | ChannelAction::Unarchive { channel } => {
                    let archived = matches!(action, ChannelAction::Archive { .. });
                    let ch = client
                        .update_channel(
                            channel,
                            &UpdateChannelRequest {
                                archived: Some(archived),
                                ..Default::default()
                            },
                        )
                        .await?;
                    let verb = if archived { "Archived" } else { "Unarchived" };
                    println!("{verb} #{} ({})", ch.name, ch.id);
                }
                ChannelAction::Delete { channel, yes } => {
                    anyhow::ensure!(*yes, "pass --yes to confirm permanent channel deletion");
                    client.delete_channel(channel).await?;
                    println!("Deleted channel {channel}");
                }
            }
        }
        Commands::Post {
            channel,
            body,
            thread,
        } => {
            let client = require_client(&cli)?;
            let msg = client
                .post_message(channel, body.clone(), thread.clone())
                .await?;
            println!("Posted {} by @{}", msg.id, msg.author_handle);
        }
        Commands::Read { channel, limit } => {
            let client = require_client(&cli)?;
            for msg in client.list_channel_messages(channel, *limit).await? {
                let replies = if msg.reply_count > 0 {
                    format!(" [{} replies]", msg.reply_count)
                } else {
                    String::new()
                };
                println!(
                    "[{}] @{}: {}{}",
                    msg.id, msg.author_handle, msg.body, replies
                );
            }
        }
        Commands::Thread { message_id } => {
            let client = require_client(&cli)?;
            let thread = client.get_thread(message_id).await?;
            println!(
                "[{}] @{}: {}",
                thread.parent.id, thread.parent.author_handle, thread.parent.body
            );
            for r in thread.replies {
                println!("  └ [{}] @{}: {}", r.id, r.author_handle, r.body);
            }
        }
        Commands::Delete { message_id } => {
            let client = require_client(&cli)?;
            client.delete_message(message_id).await?;
            println!("Deleted message {message_id}");
        }
        Commands::Unread { ack, ack_all } => {
            let client = require_client(&cli)?;
            if *ack_all {
                let items = client.list_unread().await?;
                if items.is_empty() {
                    println!("No unread items.");
                } else {
                    for item in &items {
                        client.ack_unread(&item.id).await?;
                    }
                    println!("Acknowledged {} unread item(s).", items.len());
                }
            } else if let Some(unread_id) = ack {
                client.ack_unread(unread_id).await?;
                println!("Acknowledged {unread_id}");
            } else {
                let items = client.list_unread().await?;
                if items.is_empty() {
                    println!("No unread items.");
                } else {
                    for item in items {
                        println!(
                            "[unread:{}] #{} ({}) @{}: {} (msg:{})",
                            item.id,
                            item.channel_name,
                            item.reason,
                            item.author_handle,
                            item.body,
                            item.message_id
                        );
                    }
                }
            }
        }
    }

    Ok(())
}
