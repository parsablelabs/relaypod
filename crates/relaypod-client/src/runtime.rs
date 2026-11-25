use crate::http::RelayPodClient;
use crate::ws::{run_live_ws_loop, DEFAULT_PING_INTERVAL};
use relaypod_core::{
    Channel, CreateChannelRequest, CreateIdentityRequest, CreatedIdentity, Identity, IdentityKind,
    Message, ThreadView, UnreadItem, UpdateChannelRequest, UpdateIdentityRequest, WsEvent,
};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use tokio::sync::mpsc as tokio_mpsc;

#[derive(Debug)]
pub enum ClientCommand {
    Connect {
        base_url: String,
        token: String,
    },
    Disconnect,
    LoadChannelMessages {
        channel_id: String,
    },
    LoadThread {
        message_id: String,
    },
    PostMessage {
        channel_id: String,
        body: String,
        parent_id: Option<String>,
    },
    DeleteMessage {
        message_id: String,
    },
    CreateChannel {
        name: String,
        description: String,
    },
    UpdateChannel {
        channel_id: String,
        request: UpdateChannelRequest,
    },
    DeleteChannel {
        channel_id: String,
    },
    CreateIdentity {
        handle: String,
        kind: IdentityKind,
    },
    UpdateIdentity {
        identity_id: String,
        request: UpdateIdentityRequest,
    },
    RefreshUnread,
    AckUnread {
        unread_id: String,
    },
}

#[derive(Debug)]
pub enum ClientEvent {
    Connected {
        me: Identity,
        channels: Vec<Channel>,
        identities: Vec<Identity>,
        unread: Vec<UnreadItem>,
    },
    Disconnected,
    ChannelMessages {
        channel_id: String,
        messages: Vec<Message>,
    },
    ThreadLoaded(ThreadView),
    ChannelCreated(Channel),
    IdentityCreated(CreatedIdentity),
    UnreadLoaded(Vec<UnreadItem>),
    Ws(WsEvent),
    WsStatus(bool),
    Error(String),
}

/// UI-agnostic handle to the background async HTTP + WebSocket worker.
pub struct ClientRuntime {
    cmd_tx: tokio_mpsc::UnboundedSender<ClientCommand>,
    event_rx: Receiver<ClientEvent>,
}

impl ClientRuntime {
    pub fn send(&self, cmd: ClientCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn try_recv(&self) -> Option<ClientEvent> {
        self.event_rx.try_recv().ok()
    }
}

/// Spawns a background Tokio runtime driving HTTP + WebSocket operations.
/// `on_event` is invoked whenever a new `ClientEvent` is queued (e.g. to trigger a UI repaint).
pub fn spawn_runtime<F>(on_event: F) -> ClientRuntime
where
    F: Fn() + Send + Sync + 'static,
{
    let (cmd_tx, mut cmd_rx) = tokio_mpsc::unbounded_channel::<ClientCommand>();
    let (event_tx, event_rx) = mpsc::channel::<ClientEvent>();
    let notify = Arc::new(on_event);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");
        rt.block_on(async move {
            let mut client: Option<RelayPodClient> = None;
            let mut ws_task: Option<tokio::task::JoinHandle<()>> = None;

            let emit = |tx: &Sender<ClientEvent>, notify: &Arc<F>, ev: ClientEvent| {
                let _ = tx.send(ev);
                (notify)();
            };

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    ClientCommand::Connect { base_url, token } => {
                        if let Some(handle) = ws_task.take() {
                            handle.abort();
                        }

                        let new_client = RelayPodClient::new(base_url, token.trim());
                        match fetch_initial(&new_client).await {
                            Ok((me, channels, identities, unread)) => {
                                let ws_url = new_client.ws_url();
                                client = Some(new_client);
                                emit(
                                    &event_tx,
                                    &notify,
                                    ClientEvent::Connected {
                                        me,
                                        channels,
                                        identities,
                                        unread,
                                    },
                                );
                                let tx_ws = event_tx.clone();
                                let notify_ws = Arc::clone(&notify);
                                ws_task = Some(tokio::spawn(async move {
                                    run_ws_loop(ws_url, tx_ws, notify_ws).await;
                                }));
                            }
                            Err(e) => emit(&event_tx, &notify, ClientEvent::Error(e.to_string())),
                        }
                    }
                    ClientCommand::Disconnect => {
                        if let Some(handle) = ws_task.take() {
                            handle.abort();
                        }
                        client = None;
                        emit(&event_tx, &notify, ClientEvent::Disconnected);
                    }
                    ClientCommand::LoadChannelMessages { channel_id } => {
                        if let Some(c) = &client {
                            match c.list_channel_messages(&channel_id, 100).await {
                                Ok(messages) => emit(
                                    &event_tx,
                                    &notify,
                                    ClientEvent::ChannelMessages {
                                        channel_id,
                                        messages,
                                    },
                                ),
                                Err(e) => {
                                    emit(&event_tx, &notify, ClientEvent::Error(e.to_string()))
                                }
                            }
                        }
                    }
                    ClientCommand::LoadThread { message_id } => {
                        if let Some(c) = &client {
                            match c.get_thread(&message_id).await {
                                Ok(thread) => {
                                    emit(&event_tx, &notify, ClientEvent::ThreadLoaded(thread))
                                }
                                Err(e) => {
                                    emit(&event_tx, &notify, ClientEvent::Error(e.to_string()))
                                }
                            }
                        }
                    }
                    ClientCommand::PostMessage {
                        channel_id,
                        body,
                        parent_id,
                    } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.post_message(&channel_id, body, parent_id).await {
                                emit(&event_tx, &notify, ClientEvent::Error(e.to_string()));
                            }
                        }
                    }
                    ClientCommand::DeleteMessage { message_id } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.delete_message(&message_id).await {
                                emit(&event_tx, &notify, ClientEvent::Error(e.to_string()));
                            }
                        }
                    }
                    ClientCommand::CreateChannel { name, description } => {
                        if let Some(c) = &client {
                            let req = CreateChannelRequest { name, description };
                            match c.create_channel(&req).await {
                                Ok(ch) => {
                                    emit(&event_tx, &notify, ClientEvent::ChannelCreated(ch))
                                }
                                Err(e) => {
                                    emit(&event_tx, &notify, ClientEvent::Error(e.to_string()))
                                }
                            }
                        }
                    }
                    ClientCommand::UpdateChannel {
                        channel_id,
                        request,
                    } => {
                        if let Some(c) = &client {
                            match c.update_channel(&channel_id, &request).await {
                                Ok(ch) => emit(
                                    &event_tx,
                                    &notify,
                                    ClientEvent::Ws(WsEvent::ChannelUpdated(ch)),
                                ),
                                Err(e) => {
                                    emit(&event_tx, &notify, ClientEvent::Error(e.to_string()));
                                }
                            }
                        }
                    }
                    ClientCommand::DeleteChannel { channel_id } => {
                        if let Some(c) = &client {
                            if let Err(e) = c.delete_channel(&channel_id).await {
                                emit(&event_tx, &notify, ClientEvent::Error(e.to_string()));
                            }
                        }
                    }
                    ClientCommand::CreateIdentity { handle, kind } => {
                        if let Some(c) = &client {
                            let req = CreateIdentityRequest {
                                handle,
                                kind,
                                owner_id: None,
                            };
                            match c.create_identity(&req).await {
                                Ok(created) => {
                                    emit(&event_tx, &notify, ClientEvent::IdentityCreated(created))
                                }
                                Err(e) => {
                                    emit(&event_tx, &notify, ClientEvent::Error(e.to_string()))
                                }
                            }
                        }
                    }
                    ClientCommand::UpdateIdentity {
                        identity_id,
                        request,
                    } => {
                        if let Some(c) = &client {
                            match c.update_identity(&identity_id, &request).await {
                                Ok(updated) => emit(
                                    &event_tx,
                                    &notify,
                                    ClientEvent::Ws(WsEvent::IdentityUpdated(updated)),
                                ),
                                Err(e) => {
                                    emit(&event_tx, &notify, ClientEvent::Error(e.to_string()));
                                }
                            }
                        }
                    }
                    ClientCommand::RefreshUnread => {
                        if let Some(c) = &client {
                            match c.list_unread().await {
                                Ok(items) => {
                                    emit(&event_tx, &notify, ClientEvent::UnreadLoaded(items))
                                }
                                Err(e) => {
                                    emit(&event_tx, &notify, ClientEvent::Error(e.to_string()))
                                }
                            }
                        }
                    }
                    ClientCommand::AckUnread { unread_id } => {
                        if let Some(c) = &client {
                            match c.ack_unread(&unread_id).await {
                                Ok(()) => {
                                    if let Ok(items) = c.list_unread().await {
                                        emit(&event_tx, &notify, ClientEvent::UnreadLoaded(items));
                                    }
                                }
                                Err(e) => {
                                    emit(&event_tx, &notify, ClientEvent::Error(e.to_string()))
                                }
                            }
                        }
                    }
                }
            }
        });
    });

    ClientRuntime { cmd_tx, event_rx }
}

async fn fetch_initial(
    client: &RelayPodClient,
) -> anyhow::Result<(Identity, Vec<Channel>, Vec<Identity>, Vec<UnreadItem>)> {
    let me = client.me().await?;
    let channels = client.list_channels_including_archived().await?;
    let identities = client.list_identities().await?;
    let unread = client.list_unread().await?;
    Ok((me, channels, identities, unread))
}

async fn run_ws_loop<F>(ws_url: String, tx: Sender<ClientEvent>, notify: Arc<F>)
where
    F: Fn() + Send + Sync + 'static,
{
    let tx_status = tx.clone();
    let notify_status = Arc::clone(&notify);
    run_live_ws_loop(
        ws_url,
        DEFAULT_PING_INTERVAL,
        move |live| {
            let _ = tx_status.send(ClientEvent::WsStatus(live));
            (notify_status)();
        },
        move |ev| {
            let _ = tx.send(ClientEvent::Ws(ev));
            (notify)();
        },
    )
    .await;
}
