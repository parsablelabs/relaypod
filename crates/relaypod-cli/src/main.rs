mod mcp;
mod watch;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use relaypod_client::{mcp_config_snippet, RelayPodClient};
use relaypod_core::{
    CreateChannelRequest, CreateIdentityRequest, IdentityKind, Store, UpdateChannelRequest,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "relaypod", version, about = "Shared message board for humans and AI agents")]
struct Cli {
    /// RelayPod server URL
    #[arg(long, env = "RELAYPOD_URL", default_value = "http://127.0.0.1:8787")]
    url: String,

    /// Bearer token for authenticating with RelayPod
    #[arg(long, env = "RELAYPOD_TOKEN")]
    token: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the RelayPod HTTP + WebSocket server
    Serve {
        /// Address to bind to
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
        /// Path to the SQLite database file
        #[arg(long, default_value = "relaypod.db")]
        db: PathBuf,
        /// Handle for the initial admin identity if the database is empty
        #[arg(long, default_value = "admin")]
        admin: String,
    },
    /// Run as a stdio MCP server for agent harnesses (Claude, Codex, Antigravity, etc.)
    Mcp,
    /// Watch for unread mentions/replies over WebSocket and execute configured commands
    Watch {
        /// Path to YAML config file (defaults to ./config/.relaypod.yaml, ./.relaypod.yaml, or ~/.relaypod/config.yaml)
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
        #[arg(long)]
        ack: Option<String>,
    },
}

#[derive(Subcommand)]
enum IdentityAction {
    List,
    Create {
        handle: String,
        #[arg(long, default_value = "agent")]
        kind: String,
    },
}

#[derive(Subcommand)]
enum ChannelAction {
    List {
        #[arg(long)]
        include_archived: bool,
    },
    Create {
        name: String,
        #[arg(long, default_value = "")]
        description: String,
    },
    Update {
        channel: String,
        #[arg(long, required_unless_present = "description")]
        name: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    Archive {
        channel: String,
    },
    Unarchive {
        channel: String,
    },
    Delete {
        channel: String,
        /// Confirm permanent deletion of the channel and all its messages.
        #[arg(long)]
        yes: bool,
    },
}

fn require_client(cli: &Cli) -> Result<RelayPodClient> {
    let token = cli.token.clone().ok_or_else(|| {
        anyhow!("RELAYPOD_TOKEN (or --token) is required for this command")
    })?;
    Ok(RelayPodClient::new(cli.url.clone(), token))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Serve { bind, db, admin } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "relaypod_server=info,relaypod_cli=info".into()),
                )
                .init();

            let store = Arc::new(Store::open(db)?);
            if let Some(created) = store.bootstrap(admin)? {
                println!("============================================================");
                println!("Initialized RelayPod workspace with default #general channel");
                println!("Admin handle : @{}", created.identity.handle);
                println!("Admin token  : {}", created.token);
                println!("Export this token to use the CLI or UI:");
                println!("  export RELAYPOD_URL=http://{bind}");
                println!("  export RELAYPOD_TOKEN={}", created.token);
                println!("============================================================");
            }

            relaypod_server::serve(*bind, store).await?;
        }
        Commands::Mcp => {
            let client = require_client(&cli)?;
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
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "relaypod_cli=info,relaypod=info".into()),
                )
                .init();

            watch::run_watch(watch::WatchOptions {
                default_url: cli.url.clone(),
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
                        let badge = match id.kind {
                            IdentityKind::Human => "human",
                            IdentityKind::Agent => "agent",
                        };
                        println!("@{} [{}] unread={}", id.handle, badge, id.unread_count);
                    }
                }
                IdentityAction::Create { handle, kind } => {
                    let parsed_kind = kind
                        .parse::<IdentityKind>()
                        .map_err(|e| anyhow!(e))?;
                    let created = client
                        .create_identity(&CreateIdentityRequest {
                            handle: handle.clone(),
                            kind: parsed_kind,
                            owner_id: None,
                        })
                        .await?;
                    println!("Created @{} ({})", created.identity.handle, created.identity.kind);
                    println!("Token: {}", created.token);
                    if created.identity.kind == IdentityKind::Agent {
                        println!("\nMCP config snippet:");
                        println!("{}", mcp_config_snippet(&cli.url, &created.token));
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
                println!("[{}] @{}: {}{}", msg.id, msg.author_handle, msg.body, replies);
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
        Commands::Unread { ack } => {
            let client = require_client(&cli)?;
            if let Some(unread_id) = ack {
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
