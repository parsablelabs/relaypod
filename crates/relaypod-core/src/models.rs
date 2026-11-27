use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityKind {
    Human,
    Agent,
}

impl fmt::Display for IdentityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Human => write!(f, "human"),
            Self::Agent => write!(f, "agent"),
        }
    }
}

impl FromStr for IdentityKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "human" => Ok(Self::Human),
            "agent" => Ok(Self::Agent),
            other => Err(format!("invalid identity kind: {other}")),
        }
    }
}

/// Default working directory for newly registered agents when no explicit `cwd` is provided.
pub const DEFAULT_AGENT_CWD: &str = "~/.relaypod/workspace";

/// JSON-serialized configuration and routing metadata for an AI agent identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sessions: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_ack: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_on_start: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl AgentConfig {
    pub fn is_empty(&self) -> bool {
        self.runner.is_none()
            && self.harness.is_none()
            && self.args.is_none()
            && self.exec.is_none()
            && self.cwd.is_none()
            && self.prompt.is_none()
            && self.instructions_file.is_none()
            && self.max_sessions.is_none()
            && self.auto_ack.is_none()
            && self.drain_on_start.is_none()
            && self.cooldown_secs.is_none()
            && self.description.is_none()
    }

    /// Returns sensible default CLI flags for a harness when registering a new agent
    /// without explicit `args` (disabling interactive questionnaire tools that would block a TUI).
    pub fn default_args_for_harness(harness: &str) -> Option<&'static str> {
        match harness.trim().to_lowercase().as_str() {
            "claude" | "claude-code" => Some("--disallowedTools AskUserQuestion"),
            "codex" => Some("-c features.default_mode_request_user_input=false"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    pub handle: String,
    pub kind: IdentityKind,
    pub owner_id: Option<String>,
    pub created_at: i64,
    #[serde(default)]
    pub unread_count: i64,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub busy: bool,
    /// Dynamically populated by the server from `config.yaml` (`admin_handles`); never persisted in DB.
    #[serde(default)]
    pub is_admin: bool,
    /// Optional agent runner/harness/args/description configuration (stored as JSON in SQLite).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<AgentConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedIdentity {
    pub identity: Identity,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub description: String,
    pub created_at: i64,
    #[serde(default)]
    pub created_by: Option<String>,
    /// Whether the channel is archived.
    pub archived: bool,
    pub archived_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub channel_id: String,
    pub parent_id: Option<String>,
    pub author_id: String,
    pub author_handle: String,
    pub author_kind: IdentityKind,
    pub body: String,
    pub created_at: i64,
    #[serde(default)]
    pub reply_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadView {
    pub parent: Message,
    pub replies: Vec<Message>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnreadReason {
    Mention,
    ThreadReply,
}

impl fmt::Display for UnreadReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mention => write!(f, "mention"),
            Self::ThreadReply => write!(f, "thread_reply"),
        }
    }
}

impl FromStr for UnreadReason {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "mention" => Ok(Self::Mention),
            "thread_reply" => Ok(Self::ThreadReply),
            other => Err(format!("invalid unread reason: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnreadItem {
    pub id: String,
    pub identity_id: String,
    pub message_id: String,
    pub channel_id: String,
    pub channel_name: String,
    pub parent_id: Option<String>,
    pub author_handle: String,
    pub body: String,
    pub reason: UnreadReason,
    pub acked_at: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateIdentityRequest {
    pub handle: String,
    pub kind: IdentityKind,
    pub owner_id: Option<String>,
    #[serde(default)]
    pub agent_config: Option<AgentConfig>,
}

/// Fields to update on an identity. Omitted fields retain their current values.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateIdentityRequest {
    pub handle: Option<String>,
    pub agent_config: Option<AgentConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateChannelRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// Fields to update on a channel. Omitted fields retain their current values.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateChannelRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub archived: Option<bool>,
}

/// Identifies the deleted channel and the pending unread items removed with it.
#[derive(Debug)]
pub struct DeletedChannel {
    pub channel_id: String,
    pub unread_removed: HashMap<String, i64>,
}

/// Identifies a deleted identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedIdentity {
    pub identity_id: String,
}

/// Author metadata used to authorize deleting a message.
#[derive(Debug, Clone)]
pub struct MessageDeleteInfo {
    pub message_id: String,
    pub parent_id: Option<String>,
    pub author_id: String,
    pub author_kind: IdentityKind,
    pub author_owner_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostMessageRequest {
    pub body: String,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostMessageResult {
    pub message: Message,
    pub created_unread: Vec<UnreadItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletedMessage {
    pub message_id: String,
    pub channel_id: String,
    pub parent_id: Option<String>,
    pub affected_unread: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    #[default]
    Working,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WsEvent {
    #[serde(rename = "message.created")]
    MessageCreated(Message),
    #[serde(rename = "message.deleted")]
    MessageDeleted {
        message_id: String,
        channel_id: String,
        parent_id: Option<String>,
    },
    #[serde(rename = "unread.created")]
    UnreadCreated(UnreadItem),
    #[serde(rename = "unread.acked")]
    UnreadAcked {
        unread_id: String,
        identity_id: String,
    },
    #[serde(rename = "channel.created")]
    ChannelCreated(Channel),
    #[serde(rename = "channel.updated")]
    ChannelUpdated(Channel),
    #[serde(rename = "channel.deleted")]
    ChannelDeleted {
        channel_id: String,
        unread_removed: HashMap<String, i64>,
    },
    #[serde(rename = "identity.created")]
    IdentityCreated(Identity),
    #[serde(rename = "identity.updated")]
    IdentityUpdated(Identity),
    #[serde(rename = "identity.deleted")]
    IdentityDeleted { identity_id: String },
    #[serde(rename = "presence.changed")]
    PresenceChanged { identity_id: String, online: bool },
    #[serde(rename = "activity.changed")]
    ActivityChanged {
        identity_id: String,
        handle: String,
        channel_id: String,
        thread_id: Option<String>,
        active: bool,
        #[serde(default)]
        state: ActivityState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsClientMessage {
    Ping,
    Activity {
        channel_id: String,
        thread_id: Option<String>,
        active: bool,
        #[serde(default)]
        state: ActivityState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_state_defaults_to_working_when_omitted() {
        let client_msg: WsClientMessage = serde_json::from_str(
            r#"{"type":"activity","channel_id":"c1","thread_id":"t1","active":true}"#,
        )
        .unwrap();
        assert!(matches!(
            client_msg,
            WsClientMessage::Activity {
                active: true,
                state: ActivityState::Working,
                ..
            }
        ));

        let blocked_msg: WsClientMessage = serde_json::from_str(
            r#"{"type":"activity","channel_id":"c1","thread_id":"t1","active":true,"state":"blocked"}"#,
        )
        .unwrap();
        assert!(matches!(
            blocked_msg,
            WsClientMessage::Activity {
                active: true,
                state: ActivityState::Blocked,
                ..
            }
        ));

        let event: WsEvent = serde_json::from_str(
            r#"{"type":"activity.changed","data":{"identity_id":"i1","handle":"codex","channel_id":"c1","thread_id":"t1","active":true}}"#,
        )
        .unwrap();
        assert!(matches!(
            event,
            WsEvent::ActivityChanged {
                active: true,
                state: ActivityState::Working,
                ..
            }
        ));
    }
}
