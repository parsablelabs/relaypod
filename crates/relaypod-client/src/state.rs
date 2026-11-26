use crate::runtime::{ClientCommand, ClientEvent};
use relaypod_core::{Channel, CreatedIdentity, Identity, Message, ThreadView, UnreadItem, WsEvent};

/// Follow-up commands or UI signals produced by reducing a `ClientEvent`.
#[derive(Debug, Default)]
pub struct StateEffects {
    pub commands: Vec<ClientCommand>,
    pub channel_just_created: bool,
    pub identity_just_created: Option<CreatedIdentity>,
    pub token_just_regenerated: Option<CreatedIdentity>,
}

/// Tracks an identity (typically an AI agent) currently working on a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWorker {
    pub identity_id: String,
    pub handle: String,
    pub channel_id: String,
    pub thread_id: Option<String>,
}

/// Pure, display-framework-agnostic client workspace state.
/// Any UI (egui, iced, gpui, ratatui, etc.) renders from this struct and feeds `ClientEvent`s into `apply()`.
#[derive(Debug, Default)]
pub struct WorkspaceState {
    pub connected: bool,
    pub ws_live: bool,
    pub status_error: Option<String>,

    pub me: Option<Identity>,
    pub channels: Vec<Channel>,
    pub identities: Vec<Identity>,
    pub unread: Vec<UnreadItem>,

    pub selected_channel_id: Option<String>,
    pub messages: Vec<Message>,
    pub active_thread: Option<ThreadView>,
    pub active_workers: Vec<ActiveWorker>,
}

impl WorkspaceState {
    pub fn active_channel(&self) -> Option<&Channel> {
        let id = self.selected_channel_id.as_deref()?;
        self.channels.iter().find(|c| c.id == id)
    }

    pub fn workers_for_thread(&self, channel_id: &str, thread_id: &str) -> Vec<&ActiveWorker> {
        self.active_workers
            .iter()
            .filter(|w| w.channel_id == channel_id && w.thread_id.as_deref() == Some(thread_id))
            .collect()
    }

    fn sort_channels(&mut self) {
        self.channels.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        });
    }

    pub fn select_channel(&mut self, channel_id: String) -> ClientCommand {
        self.selected_channel_id = Some(channel_id.clone());
        self.active_thread = None;
        self.messages.clear();
        ClientCommand::LoadChannelMessages { channel_id }
    }

    pub fn apply(&mut self, ev: ClientEvent) -> StateEffects {
        let mut effects = StateEffects::default();

        match ev {
            ClientEvent::Connected {
                me,
                channels,
                identities,
                unread,
            } => {
                self.connected = true;
                self.status_error = None;
                self.me = Some(me);
                self.channels = channels;
                self.sort_channels();
                self.identities = identities;
                self.unread = unread;

                if self.selected_channel_id.is_none() {
                    if let Some(first) = self.channels.iter().find(|ch| !ch.archived) {
                        let cmd = self.select_channel(first.id.clone());
                        effects.commands.push(cmd);
                    }
                }
            }
            ClientEvent::Disconnected => {
                self.connected = false;
                self.ws_live = false;
                self.status_error = None;
                self.me = None;
                self.channels.clear();
                self.identities.clear();
                self.unread.clear();
                self.selected_channel_id = None;
                self.messages.clear();
                self.active_thread = None;
                self.active_workers.clear();
            }
            ClientEvent::ChannelMessages {
                channel_id,
                messages,
            } => {
                self.status_error = None;
                if self.selected_channel_id.as_deref() == Some(channel_id.as_str()) {
                    self.messages = messages;
                }
            }
            ClientEvent::ThreadLoaded(thread) => {
                self.status_error = None;
                if self
                    .channels
                    .iter()
                    .any(|ch| ch.id == thread.parent.channel_id)
                {
                    self.active_thread = Some(thread);
                }
            }
            ClientEvent::ChannelCreated(ch) => {
                self.status_error = None;
                if !self.channels.iter().any(|c| c.id == ch.id) {
                    self.channels.push(ch.clone());
                    self.sort_channels();
                }
                let cmd = self.select_channel(ch.id);
                effects.commands.push(cmd);
                effects.channel_just_created = true;
            }
            ClientEvent::IdentityCreated(created) => {
                self.status_error = None;
                if !self.identities.iter().any(|i| i.id == created.identity.id) {
                    self.identities.push(created.identity.clone());
                }
                effects.identity_just_created = Some(created);
            }
            ClientEvent::TokenRegenerated(regenerated) => {
                self.status_error = None;
                effects.token_just_regenerated = Some(regenerated);
            }
            ClientEvent::UnreadLoaded(items) => {
                self.unread = items
                    .into_iter()
                    .filter_map(|mut item| {
                        let channel = self.channels.iter().find(|ch| ch.id == item.channel_id)?;
                        item.channel_name.clone_from(&channel.name);
                        Some(item)
                    })
                    .collect();
            }
            ClientEvent::WsStatus(live) => {
                self.ws_live = live;
            }
            ClientEvent::Error(msg) => {
                self.status_error = Some(msg);
            }
            ClientEvent::Ws(ws_ev) => self.apply_ws(ws_ev, &mut effects),
        }

        effects
    }

    fn apply_ws(&mut self, ev: WsEvent, effects: &mut StateEffects) {
        match ev {
            WsEvent::MessageCreated(msg) => {
                // Clear matching active worker immediately when the reply lands
                self.active_workers.retain(|w| {
                    !(w.identity_id == msg.author_id
                        && w.channel_id == msg.channel_id
                        && w.thread_id == msg.parent_id)
                });
                if let Some(parent_id) = &msg.parent_id {
                    if let Some(parent) = self.messages.iter_mut().find(|m| &m.id == parent_id) {
                        parent.reply_count += 1;
                    }
                    if let Some(thread) = &mut self.active_thread {
                        if &thread.parent.id == parent_id
                            && !thread.replies.iter().any(|r| r.id == msg.id)
                        {
                            thread.parent.reply_count += 1;
                            thread.replies.push(msg);
                        }
                    }
                } else if self.selected_channel_id.as_deref() == Some(msg.channel_id.as_str())
                    && !self.messages.iter().any(|m| m.id == msg.id)
                {
                    self.messages.push(msg);
                }
            }
            WsEvent::MessageDeleted {
                message_id,
                channel_id: _,
                parent_id,
            } => {
                if let Some(parent_id) = parent_id {
                    if let Some(parent) = self.messages.iter_mut().find(|m| m.id == parent_id) {
                        parent.reply_count = (parent.reply_count - 1).max(0);
                    }
                    if let Some(thread) = &mut self.active_thread {
                        if thread.parent.id == parent_id {
                            let before = thread.replies.len();
                            thread.replies.retain(|r| r.id != message_id);
                            if thread.replies.len() < before {
                                thread.parent.reply_count = (thread.parent.reply_count - 1).max(0);
                            }
                        }
                    }
                } else {
                    self.messages.retain(|m| m.id != message_id);
                    if self
                        .active_thread
                        .as_ref()
                        .is_some_and(|t| t.parent.id == message_id)
                    {
                        self.active_thread = None;
                    }
                }
                self.unread.retain(|u| {
                    u.message_id != message_id
                        && u.parent_id.as_deref() != Some(message_id.as_str())
                });
            }
            WsEvent::UnreadCreated(item) => {
                if let Some(id) = self
                    .identities
                    .iter_mut()
                    .find(|i| i.id == item.identity_id)
                {
                    id.unread_count += 1;
                }
                if self.me.as_ref().map(|m| m.id.as_str()) == Some(item.identity_id.as_str())
                    && !self.unread.iter().any(|u| u.id == item.id)
                {
                    self.unread.push(item);
                }
            }
            WsEvent::UnreadAcked {
                unread_id,
                identity_id,
            } => {
                if let Some(id) = self.identities.iter_mut().find(|i| i.id == identity_id) {
                    id.unread_count = (id.unread_count - 1).max(0);
                }
                self.unread.retain(|u| u.id != unread_id);
            }
            WsEvent::ChannelCreated(ch) => {
                if !self.channels.iter().any(|c| c.id == ch.id) {
                    self.channels.push(ch);
                    self.sort_channels();
                }
            }
            WsEvent::ChannelUpdated(ch) => {
                for item in self
                    .unread
                    .iter_mut()
                    .filter(|item| item.channel_id == ch.id)
                {
                    item.channel_name.clone_from(&ch.name);
                }
                if let Some(current) = self.channels.iter_mut().find(|current| current.id == ch.id)
                {
                    *current = ch;
                } else {
                    self.channels.push(ch);
                }
                self.sort_channels();
            }
            WsEvent::ChannelDeleted {
                channel_id,
                unread_removed,
            } => {
                self.channels.retain(|ch| ch.id != channel_id);
                self.unread.retain(|item| item.channel_id != channel_id);
                for identity in self.identities.iter_mut() {
                    if let Some(count) = unread_removed.get(&identity.id) {
                        identity.unread_count = (identity.unread_count - count).max(0);
                    }
                }
                if self
                    .active_thread
                    .as_ref()
                    .is_some_and(|thread| thread.parent.channel_id == channel_id)
                {
                    self.active_thread = None;
                }
                if self.selected_channel_id.as_deref() == Some(channel_id.as_str()) {
                    self.selected_channel_id = None;
                    self.messages.clear();
                    if let Some(first) = self.channels.iter().find(|ch| !ch.archived) {
                        effects.commands.push(self.select_channel(first.id.clone()));
                    }
                }
            }
            WsEvent::IdentityCreated(id) => {
                if !self.identities.iter().any(|i| i.id == id.id) {
                    self.identities.push(id);
                }
            }
            WsEvent::IdentityUpdated(updated) => {
                let old_handle = self
                    .identities
                    .iter()
                    .find(|i| i.id == updated.id)
                    .map(|i| i.handle.clone());

                if let Some(existing) = self.identities.iter_mut().find(|i| i.id == updated.id) {
                    *existing = updated.clone();
                } else {
                    self.identities.push(updated.clone());
                }
                self.identities.sort_by(|a, b| {
                    a.kind
                        .to_string()
                        .cmp(&b.kind.to_string())
                        .then_with(|| a.handle.to_lowercase().cmp(&b.handle.to_lowercase()))
                        .then_with(|| a.handle.cmp(&b.handle))
                });

                if let Some(me) = &mut self.me {
                    if me.id == updated.id {
                        *me = updated.clone();
                    }
                }
                for msg in &mut self.messages {
                    if msg.author_id == updated.id {
                        msg.author_handle.clone_from(&updated.handle);
                    }
                }
                if let Some(thread) = &mut self.active_thread {
                    if thread.parent.author_id == updated.id {
                        thread.parent.author_handle.clone_from(&updated.handle);
                    }
                    for reply in &mut thread.replies {
                        if reply.author_id == updated.id {
                            reply.author_handle.clone_from(&updated.handle);
                        }
                    }
                }
                if let Some(ref prev) = old_handle {
                    for item in &mut self.unread {
                        if &item.author_handle == prev {
                            item.author_handle.clone_from(&updated.handle);
                        }
                    }
                }
                for worker in &mut self.active_workers {
                    if worker.identity_id == updated.id {
                        worker.handle.clone_from(&updated.handle);
                    }
                }
            }
            WsEvent::IdentityDeleted { identity_id } => {
                self.identities.retain(|i| i.id != identity_id);
                self.active_workers.retain(|w| w.identity_id != identity_id);
            }
            WsEvent::PresenceChanged {
                identity_id,
                online,
            } => {
                if !online {
                    self.active_workers.retain(|w| w.identity_id != identity_id);
                }
                if let Some(id) = self.identities.iter_mut().find(|i| i.id == identity_id) {
                    id.online = online;
                }
                if let Some(me) = &mut self.me {
                    if me.id == identity_id {
                        me.online = online;
                    }
                }
            }
            WsEvent::ActivityChanged {
                identity_id,
                handle,
                channel_id,
                thread_id,
                active,
            } => {
                self.active_workers.retain(|w| {
                    !(w.identity_id == identity_id
                        && w.channel_id == channel_id
                        && w.thread_id == thread_id)
                });
                if active {
                    self.active_workers.push(ActiveWorker {
                        identity_id,
                        handle,
                        channel_id,
                        thread_id,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relaypod_core::{IdentityKind, UnreadReason};

    fn sample_identity(id: &str, handle: &str, kind: IdentityKind) -> Identity {
        Identity {
            id: id.into(),
            handle: handle.into(),
            kind,
            owner_id: None,
            created_at: 0,
            unread_count: 0,
            online: false,
            busy: false,
            is_admin: false,
            agent_config: None,
        }
    }

    #[test]
    fn test_workspace_state_ws_reduction() {
        let mut state = WorkspaceState::default();
        let me = sample_identity("u1", "alice", IdentityKind::Human);
        let agent = sample_identity("a1", "codex", IdentityKind::Agent);
        let ch = Channel {
            id: "c1".into(),
            name: "general".into(),
            description: String::new(),
            archived: false,
            archived_at: None,
            created_at: 0,
            created_by: None,
        };

        let effects = state.apply(ClientEvent::Connected {
            me: me.clone(),
            channels: vec![ch],
            identities: vec![me.clone(), agent.clone()],
            unread: vec![],
        });
        assert!(state.connected);
        assert_eq!(state.selected_channel_id.as_deref(), Some("c1"));
        assert_eq!(effects.commands.len(), 1);

        // Top-level message arrives via WS
        let root = Message {
            id: "m1".into(),
            channel_id: "c1".into(),
            parent_id: None,
            author_id: "u1".into(),
            author_handle: "alice".into(),
            author_kind: IdentityKind::Human,
            body: "@codex check build".into(),
            created_at: 1,
            reply_count: 0,
        };
        state.apply(ClientEvent::Ws(WsEvent::MessageCreated(root.clone())));
        assert_eq!(state.messages.len(), 1);

        // Unread arrives for agent + reply bumps reply_count
        state.apply(ClientEvent::Ws(WsEvent::UnreadCreated(UnreadItem {
            id: "un1".into(),
            identity_id: "a1".into(),
            message_id: "m1".into(),
            channel_id: "c1".into(),
            channel_name: "general".into(),
            parent_id: None,
            author_handle: "alice".into(),
            body: "@codex check build".into(),
            reason: UnreadReason::Mention,
            acked_at: None,
            created_at: 1,
        })));
        assert_eq!(state.identities[1].unread_count, 1);

        state.apply(ClientEvent::Ws(WsEvent::MessageCreated(Message {
            id: "m2".into(),
            channel_id: "c1".into(),
            parent_id: Some("m1".into()),
            author_id: "a1".into(),
            author_handle: "codex".into(),
            author_kind: IdentityKind::Agent,
            body: "Build is green!".into(),
            created_at: 2,
            reply_count: 0,
        })));
        assert_eq!(state.messages[0].reply_count, 1);

        // Ack decrements agent unread_count
        state.apply(ClientEvent::Ws(WsEvent::UnreadAcked {
            unread_id: "un1".into(),
            identity_id: "a1".into(),
        }));
        assert_eq!(state.identities[1].unread_count, 0);

        // Presence changes toggle online status
        state.apply(ClientEvent::Ws(WsEvent::PresenceChanged {
            identity_id: "a1".into(),
            online: true,
        }));
        assert!(state.identities[1].online);

        state.apply(ClientEvent::Ws(WsEvent::PresenceChanged {
            identity_id: "a1".into(),
            online: false,
        }));
        assert!(!state.identities[1].online);

        let mut archived_channel = state.channels[0].clone();
        archived_channel.name = "builds".into();
        archived_channel.archived = true;
        archived_channel.archived_at = Some(3);
        let agent_unread = UnreadItem {
            id: "un2".into(),
            identity_id: me.id.clone(),
            message_id: root.id.clone(),
            channel_id: root.channel_id.clone(),
            channel_name: "general".into(),
            parent_id: None,
            author_handle: "codex".into(),
            body: "Build is green!".into(),
            reason: UnreadReason::Mention,
            acked_at: None,
            created_at: 2,
        };
        state.apply(ClientEvent::Ws(WsEvent::UnreadCreated(agent_unread)));
        state.apply(ClientEvent::ThreadLoaded(ThreadView {
            parent: root,
            replies: vec![],
        }));
        state.apply(ClientEvent::Ws(WsEvent::ChannelUpdated(archived_channel)));
        assert_eq!(state.unread[0].channel_name, "builds");
        assert!(state.active_channel().unwrap().archived);
        assert!(state.active_thread.is_some());
        assert_eq!(state.messages.len(), 1);

        state.apply(ClientEvent::Ws(WsEvent::ChannelCreated(Channel {
            id: "c2".into(),
            name: "general".into(),
            description: String::new(),
            created_at: 4,
            created_by: None,
            archived: false,
            archived_at: None,
        })));
        let effects = state.apply(ClientEvent::Ws(WsEvent::ChannelDeleted {
            channel_id: "c1".into(),
            unread_removed: [(me.id, 1)].into(),
        }));
        assert!(state.unread.is_empty());
        assert_eq!(state.identities[0].unread_count, 0);
        assert!(state.messages.is_empty());
        assert!(state.active_thread.is_none());
        assert_eq!(state.selected_channel_id.as_deref(), Some("c2"));
        assert!(
            matches!(&effects.commands[..], [ClientCommand::LoadChannelMessages { channel_id }] if channel_id == "c2")
        );
    }
}
