use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    pub handle: String,
    pub display_name: String,
    pub kind: IdentityKind,
    pub owner_id: Option<String>,
    pub created_at: i64,
    #[serde(default)]
    pub unread_count: i64,
    #[serde(default)]
    pub online: bool,
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
    pub display_name: Option<String>,
    pub kind: IdentityKind,
    pub owner_id: Option<String>,
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
    #[serde(rename = "presence.changed")]
    PresenceChanged { identity_id: String, online: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsClientMessage {
    Ping,
}
