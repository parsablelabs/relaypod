use crate::auth::{
    extract_mentions, generate_token, hash_token, normalize_channel_name, normalize_handle,
};
use crate::models::{
    Channel, CreateChannelRequest, CreateIdentityRequest, CreatedIdentity, Identity, IdentityKind,
    Message, PostMessageResult, ThreadView, UnreadItem, UnreadReason,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use ulid::Ulid;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("conflict: {0}")]
    Conflict(String),
}

pub type CoreResult<T> = Result<T, CoreError>;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn new_id() -> String {
    Ulid::new().to_string()
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> CoreResult<Self> {
        let conn = Connection::open(path)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init()?;
        Ok(store)
    }

    pub fn open_in_memory() -> CoreResult<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init()?;
        Ok(store)
    }

    fn init(&self) -> CoreResult<()> {
        let conn = self.conn.lock().expect("store lock poisoned");
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS identities (
                id TEXT PRIMARY KEY,
                handle TEXT UNIQUE NOT NULL,
                display_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                owner_id TEXT NULL REFERENCES identities(id),
                token_hash TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS channels (
                id TEXT PRIMARY KEY,
                name TEXT UNIQUE NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                channel_id TEXT NOT NULL REFERENCES channels(id),
                parent_id TEXT NULL REFERENCES messages(id),
                author_id TEXT NOT NULL REFERENCES identities(id),
                body TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_messages_channel_parent_created
                ON messages(channel_id, parent_id, created_at);
            CREATE INDEX IF NOT EXISTS idx_messages_parent
                ON messages(parent_id, created_at);

            CREATE TABLE IF NOT EXISTS unread_items (
                id TEXT PRIMARY KEY,
                identity_id TEXT NOT NULL REFERENCES identities(id),
                message_id TEXT NOT NULL REFERENCES messages(id),
                reason TEXT NOT NULL,
                acked_at INTEGER NULL,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_unread_identity_acked
                ON unread_items(identity_id, acked_at, created_at);
            "#,
        )?;
        Ok(())
    }

    /// Ensures a default `#general` channel exists and, if no identities exist yet,
    /// creates an initial admin human identity and returns its plaintext token.
    pub fn bootstrap(&self, admin_handle: &str) -> CoreResult<Option<CreatedIdentity>> {
        if self.get_channel_by_name("general")?.is_none() {
            self.create_channel(CreateChannelRequest {
                name: "general".to_string(),
                description: "Shared workspace for humans and agents".to_string(),
            })?;
        }

        let count: i64 = {
            let conn = self.conn.lock().expect("store lock poisoned");
            conn.query_row("SELECT COUNT(*) FROM identities", [], |row| row.get(0))?
        };

        if count == 0 {
            let created = self.create_identity(CreateIdentityRequest {
                handle: admin_handle.to_string(),
                display_name: Some(admin_handle.to_string()),
                kind: IdentityKind::Human,
                owner_id: None,
            })?;
            Ok(Some(created))
        } else {
            Ok(None)
        }
    }

    pub fn create_identity(&self, req: CreateIdentityRequest) -> CoreResult<CreatedIdentity> {
        let handle = normalize_handle(&req.handle);
        if handle.is_empty()
            || !handle
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(CoreError::InvalidInput(
                "handle must be non-empty and contain only letters, digits, '-' or '_'".into(),
            ));
        }

        let display_name = req
            .display_name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| handle.clone());

        let id = new_id();
        let token = generate_token();
        let token_hash = hash_token(&token);
        let created_at = now_ms();

        let conn = self.conn.lock().expect("store lock poisoned");
        let res = conn.execute(
            "INSERT INTO identities (id, handle, display_name, kind, owner_id, token_hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                handle,
                display_name,
                req.kind.to_string(),
                req.owner_id,
                token_hash,
                created_at
            ],
        );

        match res {
            Ok(_) => Ok(CreatedIdentity {
                identity: Identity {
                    id,
                    handle,
                    display_name,
                    kind: req.kind,
                    owner_id: req.owner_id,
                    created_at,
                    unread_count: 0,
                    online: false,
                },
                token,
            }),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(CoreError::Conflict(format!(
                    "identity '@{handle}' already exists"
                )))
            }
            Err(e) => Err(CoreError::Db(e)),
        }
    }

    pub fn authenticate(&self, token: &str) -> CoreResult<Option<Identity>> {
        let token_hash = hash_token(token);
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT i.id, i.handle, i.display_name, i.kind, i.owner_id, i.created_at,
                    (SELECT COUNT(*) FROM unread_items u WHERE u.identity_id = i.id AND u.acked_at IS NULL)
             FROM identities i
             WHERE i.token_hash = ?1",
        )?;
        let identity = stmt
            .query_row(params![token_hash], row_to_identity)
            .optional()?;
        Ok(identity)
    }

    pub fn list_identities(&self) -> CoreResult<Vec<Identity>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT i.id, i.handle, i.display_name, i.kind, i.owner_id, i.created_at,
                    (SELECT COUNT(*) FROM unread_items u WHERE u.identity_id = i.id AND u.acked_at IS NULL)
             FROM identities i
             ORDER BY i.kind ASC, i.handle ASC",
        )?;
        let rows = stmt.query_map([], row_to_identity)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn create_channel(&self, req: CreateChannelRequest) -> CoreResult<Channel> {
        let name = normalize_channel_name(&req.name);
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(CoreError::InvalidInput(
                "channel name must be non-empty and contain only letters, digits, '-' or '_'"
                    .into(),
            ));
        }

        let id = new_id();
        let created_at = now_ms();
        let conn = self.conn.lock().expect("store lock poisoned");
        let res = conn.execute(
            "INSERT INTO channels (id, name, description, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, name, req.description, created_at],
        );

        match res {
            Ok(_) => Ok(Channel {
                id,
                name,
                description: req.description,
                created_at,
            }),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(CoreError::Conflict(format!(
                    "channel '#{name}' already exists"
                )))
            }
            Err(e) => Err(CoreError::Db(e)),
        }
    }

    pub fn list_channels(&self) -> CoreResult<Vec<Channel>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, description, created_at FROM channels ORDER BY name ASC",
        )?;
        let rows = stmt.query_map([], row_to_channel)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get_channel(&self, id_or_name: &str) -> CoreResult<Option<Channel>> {
        let normalized = normalize_channel_name(id_or_name);
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, description, created_at FROM channels WHERE id = ?1 OR name = ?2",
        )?;
        Ok(stmt
            .query_row(params![id_or_name, normalized], row_to_channel)
            .optional()?)
    }

    pub fn get_channel_by_name(&self, name: &str) -> CoreResult<Option<Channel>> {
        let normalized = normalize_channel_name(name);
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, description, created_at FROM channels WHERE name = ?1",
        )?;
        Ok(stmt
            .query_row(params![normalized], row_to_channel)
            .optional()?)
    }

    pub fn post_message(
        &self,
        channel_id_or_name: &str,
        author: &Identity,
        body: &str,
        parent_id: Option<String>,
    ) -> CoreResult<PostMessageResult> {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            return Err(CoreError::InvalidInput("message body cannot be empty".into()));
        }

        let channel = self
            .get_channel(channel_id_or_name)?
            .ok_or_else(|| CoreError::NotFound(format!("channel '{channel_id_or_name}'")))?;

        let conn = self.conn.lock().expect("store lock poisoned");

        // Validate parent message if replying in a thread
        let resolved_parent_id = if let Some(pid) = parent_id {
            let parent_row: Option<(String, Option<String>)> = conn
                .query_row(
                    "SELECT channel_id, parent_id FROM messages WHERE id = ?1",
                    params![pid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (parent_channel_id, grandparent_id) = parent_row
                .ok_or_else(|| CoreError::NotFound(format!("parent message '{pid}'")))?;
            if parent_channel_id != channel.id {
                return Err(CoreError::InvalidInput(
                    "parent message belongs to a different channel".into(),
                ));
            }
            // Flatten nested replies onto the thread root
            Some(grandparent_id.unwrap_or(pid))
        } else {
            None
        };

        let msg_id = new_id();
        let created_at = now_ms();

        conn.execute(
            "INSERT INTO messages (id, channel_id, parent_id, author_id, body, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                msg_id,
                channel.id,
                resolved_parent_id,
                author.id,
                trimmed,
                created_at
            ],
        )?;

        let mut notified_identities: HashSet<String> = HashSet::new();
        let mut created_unread: Vec<UnreadItem> = Vec::new();

        // 1. Enqueue unread items for @mentions
        for handle in extract_mentions(trimmed) {
            let target_id: Option<String> = conn
                .query_row(
                    "SELECT id FROM identities WHERE handle = ?1",
                    params![handle],
                    |r| r.get(0),
                )
                .optional()?;

            if let Some(target_id) = target_id {
                if target_id != author.id && notified_identities.insert(target_id.clone()) {
                    let unread_id = new_id();
                    conn.execute(
                        "INSERT INTO unread_items (id, identity_id, message_id, reason, acked_at, created_at)
                         VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
                        params![
                            unread_id,
                            target_id,
                            msg_id,
                            UnreadReason::Mention.to_string(),
                            created_at
                        ],
                    )?;
                    created_unread.push(UnreadItem {
                        id: unread_id,
                        identity_id: target_id,
                        message_id: msg_id.clone(),
                        channel_id: channel.id.clone(),
                        channel_name: channel.name.clone(),
                        parent_id: resolved_parent_id.clone(),
                        author_handle: author.handle.clone(),
                        body: trimmed.to_string(),
                        reason: UnreadReason::Mention,
                        acked_at: None,
                        created_at,
                    });
                }
            }
        }

        // 2. If this is a thread reply, notify prior participants in the thread
        if let Some(ref root_id) = resolved_parent_id {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT author_id FROM messages WHERE id = ?1 OR parent_id = ?1",
            )?;
            let participants = stmt.query_map(params![root_id], |r| r.get::<_, String>(0))?;
            for participant in participants {
                let target_id = participant?;
                if target_id != author.id && notified_identities.insert(target_id.clone()) {
                    let unread_id = new_id();
                    conn.execute(
                        "INSERT INTO unread_items (id, identity_id, message_id, reason, acked_at, created_at)
                         VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
                        params![
                            unread_id,
                            target_id,
                            msg_id,
                            UnreadReason::ThreadReply.to_string(),
                            created_at
                        ],
                    )?;
                    created_unread.push(UnreadItem {
                        id: unread_id,
                        identity_id: target_id,
                        message_id: msg_id.clone(),
                        channel_id: channel.id.clone(),
                        channel_name: channel.name.clone(),
                        parent_id: resolved_parent_id.clone(),
                        author_handle: author.handle.clone(),
                        body: trimmed.to_string(),
                        reason: UnreadReason::ThreadReply,
                        acked_at: None,
                        created_at,
                    });
                }
            }
        }

        let message = Message {
            id: msg_id,
            channel_id: channel.id,
            parent_id: resolved_parent_id,
            author_id: author.id.clone(),
            author_handle: author.handle.clone(),
            author_kind: author.kind,
            body: trimmed.to_string(),
            created_at,
            reply_count: 0,
        };

        Ok(PostMessageResult {
            message,
            created_unread,
        })
    }

    pub fn list_channel_messages(
        &self,
        channel_id_or_name: &str,
        limit: usize,
    ) -> CoreResult<Vec<Message>> {
        let channel = self
            .get_channel(channel_id_or_name)?
            .ok_or_else(|| CoreError::NotFound(format!("channel '{channel_id_or_name}'")))?;

        let limit = limit.clamp(1, 200) as i64;
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT m.id, m.channel_id, m.parent_id, m.author_id, i.handle, i.kind, m.body, m.created_at,
                    (SELECT COUNT(*) FROM messages r WHERE r.parent_id = m.id) AS reply_count
             FROM messages m
             JOIN identities i ON i.id = m.author_id
             WHERE m.channel_id = ?1 AND m.parent_id IS NULL
             ORDER BY m.created_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![channel.id, limit], row_to_message)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        out.reverse();
        Ok(out)
    }

    pub fn get_thread(&self, message_id: &str) -> CoreResult<ThreadView> {
        let conn = self.conn.lock().expect("store lock poisoned");

        // If message_id is a reply, resolve to its root
        let maybe_parent: Option<Option<String>> = conn
            .query_row(
                "SELECT parent_id FROM messages WHERE id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .optional()?;
        let root_id = match maybe_parent {
            None => return Err(CoreError::NotFound(format!("message '{message_id}'"))),
            Some(Some(pid)) => pid,
            Some(None) => message_id.to_string(),
        };

        let parent = conn.query_row(
            "SELECT m.id, m.channel_id, m.parent_id, m.author_id, i.handle, i.kind, m.body, m.created_at,
                    (SELECT COUNT(*) FROM messages r WHERE r.parent_id = m.id) AS reply_count
             FROM messages m
             JOIN identities i ON i.id = m.author_id
             WHERE m.id = ?1",
            params![root_id],
            row_to_message,
        )?;

        let mut stmt = conn.prepare(
            "SELECT m.id, m.channel_id, m.parent_id, m.author_id, i.handle, i.kind, m.body, m.created_at, 0
             FROM messages m
             JOIN identities i ON i.id = m.author_id
             WHERE m.parent_id = ?1
             ORDER BY m.created_at ASC",
        )?;
        let rows = stmt.query_map(params![root_id], row_to_message)?;
        let mut replies = Vec::new();
        for row in rows {
            replies.push(row?);
        }

        Ok(ThreadView { parent, replies })
    }

    pub fn list_unread(&self, identity_id: &str) -> CoreResult<Vec<UnreadItem>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT u.id, u.identity_id, u.message_id, m.channel_id, c.name, m.parent_id,
                    i.handle, m.body, u.reason, u.acked_at, u.created_at
             FROM unread_items u
             JOIN messages m ON m.id = u.message_id
             JOIN channels c ON c.id = m.channel_id
             JOIN identities i ON i.id = m.author_id
             WHERE u.identity_id = ?1 AND u.acked_at IS NULL
             ORDER BY u.created_at ASC",
        )?;
        let rows = stmt.query_map(params![identity_id], row_to_unread)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn ack_unread(&self, identity_id: &str, unread_id: &str) -> CoreResult<()> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let updated = conn.execute(
            "UPDATE unread_items SET acked_at = ?1
             WHERE id = ?2 AND identity_id = ?3 AND acked_at IS NULL",
            params![now_ms(), unread_id, identity_id],
        )?;
        if updated == 0 {
            return Err(CoreError::NotFound(format!(
                "unread item '{unread_id}' not found or already acknowledged"
            )));
        }
        Ok(())
    }
}

fn row_to_identity(row: &rusqlite::Row<'_>) -> rusqlite::Result<Identity> {
    let kind_str: String = row.get(3)?;
    let kind = kind_str.parse::<IdentityKind>().unwrap_or(IdentityKind::Agent);
    Ok(Identity {
        id: row.get(0)?,
        handle: row.get(1)?,
        display_name: row.get(2)?,
        kind,
        owner_id: row.get(4)?,
        created_at: row.get(5)?,
        unread_count: row.get(6)?,
        online: false,
    })
}

fn row_to_channel(row: &rusqlite::Row<'_>) -> rusqlite::Result<Channel> {
    Ok(Channel {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        created_at: row.get(3)?,
    })
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let kind_str: String = row.get(5)?;
    let author_kind = kind_str.parse::<IdentityKind>().unwrap_or(IdentityKind::Agent);
    Ok(Message {
        id: row.get(0)?,
        channel_id: row.get(1)?,
        parent_id: row.get(2)?,
        author_id: row.get(3)?,
        author_handle: row.get(4)?,
        author_kind,
        body: row.get(6)?,
        created_at: row.get(7)?,
        reply_count: row.get(8)?,
    })
}

fn row_to_unread(row: &rusqlite::Row<'_>) -> rusqlite::Result<UnreadItem> {
    let reason_str: String = row.get(8)?;
    let reason = reason_str
        .parse::<UnreadReason>()
        .unwrap_or(UnreadReason::Mention);
    Ok(UnreadItem {
        id: row.get(0)?,
        identity_id: row.get(1)?,
        message_id: row.get(2)?,
        channel_id: row.get(3)?,
        channel_name: row.get(4)?,
        parent_id: row.get(5)?,
        author_handle: row.get(6)?,
        body: row.get(7)?,
        reason,
        acked_at: row.get(9)?,
        created_at: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bootstrap_mentions_threads_and_unread_queue() {
        let store = Store::open_in_memory().unwrap();
        let admin = store.bootstrap("martek").unwrap().expect("admin created");
        assert_eq!(admin.identity.handle, "martek");

        // Second bootstrap is a no-op
        assert!(store.bootstrap("martek").unwrap().is_none());

        // Auth works
        let authed = store.authenticate(&admin.token).unwrap().unwrap();
        assert_eq!(authed.id, admin.identity.id);

        // Create an agent identity
        let agent = store
            .create_identity(CreateIdentityRequest {
                handle: "claude-local".into(),
                display_name: Some("Claude Local".into()),
                kind: IdentityKind::Agent,
                owner_id: Some(admin.identity.id.clone()),
            })
            .unwrap();

        // Human mentions agent in #general
        let post = store
            .post_message("general", &admin.identity, "@claude-local please check auth", None)
            .unwrap();
        assert_eq!(post.created_unread.len(), 1);
        assert_eq!(post.created_unread[0].identity_id, agent.identity.id);
        assert_eq!(post.created_unread[0].reason, UnreadReason::Mention);

        // Agent sees unread item
        let unread = store.list_unread(&agent.identity.id).unwrap();
        assert_eq!(unread.len(), 1);

        // Agent replies in thread -> notifies human via thread_reply
        let reply = store
            .post_message(
                "general",
                &agent.identity,
                "Checked! Token expiry looks good.",
                Some(post.message.id.clone()),
            )
            .unwrap();
        assert_eq!(reply.created_unread.len(), 1);
        assert_eq!(reply.created_unread[0].identity_id, admin.identity.id);
        assert_eq!(reply.created_unread[0].reason, UnreadReason::ThreadReply);

        // Agent acks its unread item
        store.ack_unread(&agent.identity.id, &unread[0].id).unwrap();
        assert!(store.list_unread(&agent.identity.id).unwrap().is_empty());

        // Thread view includes parent + reply
        let thread = store.get_thread(&post.message.id).unwrap();
        assert_eq!(thread.parent.reply_count, 1);
        assert_eq!(thread.replies.len(), 1);
    }
}
