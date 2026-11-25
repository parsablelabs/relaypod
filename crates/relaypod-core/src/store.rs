use crate::auth::{
    extract_mentions, generate_token, hash_token, normalize_channel_name, normalize_handle,
};
use crate::models::{
    Channel, CreateChannelRequest, CreateIdentityRequest, CreatedIdentity, DeletedChannel,
    DeletedIdentity, DeletedMessage, Identity, IdentityKind, Message, PostMessageResult,
    ThreadView, UnreadItem, UnreadReason, UpdateChannelRequest, UpdateIdentityRequest,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::collections::{HashMap, HashSet};
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
        let mut conn = self.conn.lock().expect("store lock poisoned");
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS identities (
                id TEXT PRIMARY KEY,
                handle TEXT UNIQUE NOT NULL,
                kind TEXT NOT NULL,
                owner_id TEXT NULL REFERENCES identities(id),
                token_hash TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                deleted_at INTEGER NULL
            );

            CREATE TABLE IF NOT EXISTS channels (
                id TEXT PRIMARY KEY,
                name TEXT UNIQUE NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL,
                archived_at INTEGER
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

        // Lock the schema check and alteration together in case another process opens the database.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_archived_at: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('channels') WHERE name = 'archived_at')",
            [],
            |row| row.get(0),
        )?;
        if !has_archived_at {
            tx.execute(
                "ALTER TABLE channels ADD COLUMN archived_at INTEGER NULL",
                [],
            )?;
        }
        let has_display_name: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('identities') WHERE name = 'display_name')",
            [],
            |row| row.get(0),
        )?;
        if has_display_name {
            tx.execute("ALTER TABLE identities DROP COLUMN display_name", [])?;
        }
        let has_deleted_at: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('identities') WHERE name = 'deleted_at')",
            [],
            |row| row.get(0),
        )?;
        if !has_deleted_at {
            tx.execute(
                "ALTER TABLE identities ADD COLUMN deleted_at INTEGER NULL",
                [],
            )?;
        }
        tx.commit()?;

        Ok(())
    }

    /// Creates the default channel and initial human identity when starting a new board.
    /// Returns the new identity and its plaintext token.
    pub fn bootstrap(&self, admin_handle: &str) -> CoreResult<Option<CreatedIdentity>> {
        let count: i64 = {
            let conn = self.conn.lock().expect("store lock poisoned");
            conn.query_row("SELECT COUNT(*) FROM identities", [], |row| row.get(0))?
        };

        if count == 0 {
            if self.get_channel_by_name("general")?.is_none() {
                self.create_channel(CreateChannelRequest {
                    name: "general".to_string(),
                    description: "Shared workspace for humans and agents".to_string(),
                })?;
            }
            let created = self.create_identity(CreateIdentityRequest {
                handle: admin_handle.to_string(),
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

        let id = new_id();
        let token = generate_token();
        let token_hash = hash_token(&token);
        let created_at = now_ms();

        let conn = self.conn.lock().expect("store lock poisoned");
        let res = conn.execute(
            "INSERT INTO identities (id, handle, kind, owner_id, token_hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                handle,
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
                    kind: req.kind,
                    owner_id: req.owner_id,
                    created_at,
                    unread_count: 0,
                    online: false,
                    busy: false,
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
            "SELECT i.id, i.handle, i.kind, i.owner_id, i.created_at,
                    (SELECT COUNT(*) FROM unread_items u WHERE u.identity_id = i.id AND u.acked_at IS NULL)
             FROM identities i
             WHERE i.token_hash = ?1 AND i.deleted_at IS NULL",
        )?;
        let identity = stmt
            .query_row(params![token_hash], row_to_identity)
            .optional()?;
        Ok(identity)
    }

    pub fn list_identities(&self) -> CoreResult<Vec<Identity>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT i.id, i.handle, i.kind, i.owner_id, i.created_at,
                    (SELECT COUNT(*) FROM unread_items u WHERE u.identity_id = i.id AND u.acked_at IS NULL)
             FROM identities i
             WHERE i.deleted_at IS NULL
             ORDER BY i.kind ASC, i.handle ASC",
        )?;
        let rows = stmt.query_map([], row_to_identity)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get_identity(&self, id_or_handle: &str) -> CoreResult<Option<Identity>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        find_identity(&conn, id_or_handle)
    }

    pub fn update_identity(
        &self,
        id_or_handle: &str,
        request: UpdateIdentityRequest,
    ) -> CoreResult<Identity> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut identity = find_identity(&tx, id_or_handle)?
            .ok_or_else(|| CoreError::NotFound(format!("identity '{id_or_handle}'")))?;

        if let Some(raw_handle) = request.handle {
            let handle = normalize_handle(&raw_handle);
            if handle.is_empty()
                || !handle
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(CoreError::InvalidInput(
                    "handle must be non-empty and contain only letters, digits, '-' or '_'".into(),
                ));
            }
            identity.handle = handle;
        }

        let res = tx.execute(
            "UPDATE identities SET handle = ?2 WHERE id = ?1",
            params![identity.id, identity.handle],
        );
        match res {
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                    || err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(CoreError::Conflict(format!(
                    "identity '@{}' already exists",
                    identity.handle
                )));
            }
            result => {
                result?;
            }
        }
        tx.commit()?;
        Ok(identity)
    }

    /// Deletes an identity:
    /// - Hard-deletes if it has no authored messages and no existing messages mentioning its handle.
    /// - Soft-deletes (revokes token, clears unread queue, hides from active listings) otherwise,
    ///   preserving message history and mention references.
    pub fn delete_identity(&self, id_or_handle: &str) -> CoreResult<DeletedIdentity> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let identity = find_identity(&tx, id_or_handle)?
            .ok_or_else(|| CoreError::NotFound(format!("identity '{id_or_handle}'")))?;

        let has_authored: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE author_id = ?1)",
            params![&identity.id],
            |r| r.get(0),
        )?;

        let has_mentions = if has_authored {
            true
        } else {
            let mut stmt = tx.prepare("SELECT body FROM messages")?;
            let bodies = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut mentioned = false;
            for body in bodies {
                let body = body?;
                if extract_mentions(&body).iter().any(|h| h == &identity.handle) {
                    mentioned = true;
                    break;
                }
            }
            mentioned
        };

        tx.execute(
            "DELETE FROM unread_items WHERE identity_id = ?1",
            params![&identity.id],
        )?;

        let hard_deleted = if !has_authored && !has_mentions {
            tx.execute("DELETE FROM identities WHERE id = ?1", params![&identity.id])?;
            true
        } else {
            let deleted_at = now_ms();
            let revoked_hash = format!("revoked:{}:{deleted_at}", identity.id);
            tx.execute(
                "UPDATE identities SET deleted_at = ?2, token_hash = ?3 WHERE id = ?1",
                params![&identity.id, deleted_at, revoked_hash],
            )?;
            false
        };

        tx.commit()?;
        Ok(DeletedIdentity {
            identity_id: identity.id,
            hard_deleted,
        })
    }

    pub fn create_channel(&self, req: CreateChannelRequest) -> CoreResult<Channel> {
        let name = validate_channel_name(&req.name)?;

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
                archived: false,
                archived_at: None,
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

    /// Lists channels by name, including archived channels only when requested.
    ///
    /// # Errors
    ///
    /// Returns a database error if the channel list cannot be read.
    pub fn list_channels(&self, include_archived: bool) -> CoreResult<Vec<Channel>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, description, created_at, archived_at FROM channels
             WHERE ?1 OR archived_at IS NULL
             ORDER BY LOWER(name) ASC, name ASC",
        )?;
        let rows = stmt.query_map([include_archived], row_to_channel)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn get_channel(&self, id_or_name: &str) -> CoreResult<Option<Channel>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        find_channel(&conn, id_or_name)
    }

    /// Updates the supplied fields together and preserves the original archive time on retries.
    pub fn update_channel(
        &self,
        id_or_name: &str,
        request: UpdateChannelRequest,
    ) -> CoreResult<Channel> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut channel = require_channel(&tx, id_or_name)?;

        if let Some(name) = request.name {
            channel.name = validate_channel_name(&name)?;
        }
        if let Some(description) = request.description {
            channel.description = description;
        }
        match request.archived {
            Some(true) => {
                channel.archived_at.get_or_insert_with(now_ms);
            }
            Some(false) => channel.archived_at = None,
            None => {}
        }
        channel.archived = channel.archived_at.is_some();

        let result = tx.execute(
            "UPDATE channels SET name = ?2, description = ?3, archived_at = ?4 WHERE id = ?1",
            params![
                channel.id,
                channel.name,
                channel.description,
                channel.archived_at
            ],
        );
        match result {
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                return Err(CoreError::Conflict(format!(
                    "channel '#{}' already exists",
                    channel.name
                )));
            }
            result => {
                result?;
            }
        }
        tx.commit()?;
        Ok(channel)
    }

    /// Updates archive status while preserving channel details and messages.
    ///
    /// # Errors
    ///
    /// Returns an error if the channel does not exist or the database update fails.
    pub fn set_channel_archived(&self, id_or_name: &str, archived: bool) -> CoreResult<Channel> {
        self.update_channel(
            id_or_name,
            UpdateChannelRequest {
                archived: Some(archived),
                ..Default::default()
            },
        )
    }

    /// Permanently removes a channel and its messages and unread items in one transaction.
    pub fn delete_channel(&self, id_or_name: &str) -> CoreResult<DeletedChannel> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let channel = require_channel(&tx, id_or_name)?;
        let unread_removed = {
            let mut stmt = tx.prepare(
                "SELECT u.identity_id, COUNT(*) FROM unread_items u
                 JOIN messages m ON m.id = u.message_id
                 WHERE m.channel_id = ?1 AND u.acked_at IS NULL GROUP BY u.identity_id",
            )?;
            let rows = stmt.query_map([&channel.id], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<Result<HashMap<_, _>, _>>()?
        };
        tx.execute(
            "DELETE FROM unread_items WHERE message_id IN (SELECT id FROM messages WHERE channel_id = ?1)",
            [&channel.id],
        )?;
        tx.execute("DELETE FROM messages WHERE channel_id = ?1", [&channel.id])?;
        tx.execute("DELETE FROM channels WHERE id = ?1", [&channel.id])?;
        tx.commit()?;
        Ok(DeletedChannel {
            channel_id: channel.id,
            unread_removed,
        })
    }

    pub fn get_channel_by_name(&self, name: &str) -> CoreResult<Option<Channel>> {
        let normalized = normalize_channel_name(name);
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, description, created_at, archived_at FROM channels WHERE name = ?1",
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

        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let channel = require_channel(&tx, channel_id_or_name)?;
        ensure_channel_writable(&channel)?;

        // Validate parent message if replying in a thread
        let resolved_parent_id = if let Some(pid) = parent_id {
            let parent_row: Option<(String, Option<String>)> = tx
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

        tx.execute(
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
            let target_id: Option<String> = tx
                .query_row(
                    "SELECT id FROM identities WHERE handle = ?1 AND deleted_at IS NULL",
                    params![handle],
                    |r| r.get(0),
                )
                .optional()?;

            if let Some(target_id) = target_id {
                if target_id != author.id && notified_identities.insert(target_id.clone()) {
                    let unread_id = new_id();
                    tx.execute(
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

        // 2. If this is a thread reply, notify prior human participants in the thread
        // (agents must be explicitly @mentioned to avoid waking every agent in multi-agent threads)
        if let Some(ref root_id) = resolved_parent_id {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT m.author_id
                 FROM messages m
                 JOIN identities i ON i.id = m.author_id
                 WHERE (m.id = ?1 OR m.parent_id = ?1) AND i.kind = 'human'",
            )?;
            let participants = stmt.query_map(params![root_id], |r| r.get::<_, String>(0))?;
            for participant in participants {
                let target_id = participant?;
                if target_id != author.id && notified_identities.insert(target_id.clone()) {
                    let unread_id = new_id();
                    tx.execute(
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

        tx.commit()?;

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

    /// Deletes a message (and any child thread replies + associated unread items).
    pub fn delete_message(&self, message_id: &str) -> CoreResult<DeletedMessage> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let meta: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT channel_id, parent_id FROM messages WHERE id = ?1",
                params![message_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        let (channel_id, parent_id) =
            meta.ok_or_else(|| CoreError::NotFound(format!("message '{message_id}' not found")))?;

        ensure_channel_writable(&require_channel(&tx, &channel_id)?)?;

        if parent_id.is_some() {
            return Err(CoreError::InvalidInput(
                "cannot delete an individual thread reply; delete the top-level message instead"
                    .into(),
            ));
        }

        let mut stmt = tx.prepare(
            "SELECT id, identity_id FROM unread_items
             WHERE acked_at IS NULL
               AND (message_id = ?1 OR message_id IN (SELECT id FROM messages WHERE parent_id = ?1))",
        )?;
        let affected_unread: Vec<(String, String)> = stmt
            .query_map(params![message_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);

        tx.execute(
            "DELETE FROM unread_items
             WHERE message_id = ?1 OR message_id IN (SELECT id FROM messages WHERE parent_id = ?1)",
            params![message_id],
        )?;
        tx.execute(
            "DELETE FROM messages WHERE parent_id = ?1",
            params![message_id],
        )?;
        tx.execute("DELETE FROM messages WHERE id = ?1", params![message_id])?;

        tx.commit()?;

        Ok(DeletedMessage {
            message_id: message_id.to_string(),
            channel_id,
            parent_id,
            affected_unread,
        })
    }
}

fn validate_channel_name(raw: &str) -> CoreResult<String> {
    let name = normalize_channel_name(raw);
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(CoreError::InvalidInput(
            "channel name must be non-empty and contain only letters, digits, '-' or '_'".into(),
        ));
    }

    Ok(name)
}

fn find_identity(conn: &Connection, id_or_handle: &str) -> CoreResult<Option<Identity>> {
    let normalized = normalize_handle(id_or_handle);
    Ok(conn
        .query_row(
            "SELECT i.id, i.handle, i.kind, i.owner_id, i.created_at,
                    (SELECT COUNT(*) FROM unread_items u WHERE u.identity_id = i.id AND u.acked_at IS NULL)
             FROM identities i
             WHERE (i.id = ?1 OR i.handle = ?2) AND i.deleted_at IS NULL
             ORDER BY (i.id = ?1) DESC
             LIMIT 1",
            params![id_or_handle, normalized],
            row_to_identity,
        )
        .optional()?)
}

fn find_channel(conn: &Connection, id_or_name: &str) -> CoreResult<Option<Channel>> {
    let normalized = normalize_channel_name(id_or_name);
    Ok(conn
        .query_row(
            "SELECT id, name, description, created_at, archived_at FROM channels
             WHERE id = ?1 OR name = ?2 ORDER BY (id = ?1) DESC LIMIT 1",
            params![id_or_name, normalized],
            row_to_channel,
        )
        .optional()?)
}

fn require_channel(conn: &Connection, id_or_name: &str) -> CoreResult<Channel> {
    find_channel(conn, id_or_name)?
        .ok_or_else(|| CoreError::NotFound(format!("channel '{id_or_name}'")))
}

fn ensure_channel_writable(channel: &Channel) -> CoreResult<()> {
    if channel.archived {
        return Err(CoreError::Conflict(format!(
            "channel '#{}' is archived",
            channel.name
        )));
    }
    Ok(())
}

fn row_to_identity(row: &rusqlite::Row<'_>) -> rusqlite::Result<Identity> {
    let kind_str: String = row.get(2)?;
    let kind = kind_str.parse::<IdentityKind>().unwrap_or(IdentityKind::Agent);
    Ok(Identity {
        id: row.get(0)?,
        handle: row.get(1)?,
        kind,
        owner_id: row.get(3)?,
        created_at: row.get(4)?,
        unread_count: row.get(5)?,
        online: false,
        busy: false,
    })
}

fn row_to_channel(row: &rusqlite::Row<'_>) -> rusqlite::Result<Channel> {
    let archived_at: Option<i64> = row.get(4)?;
    Ok(Channel {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        created_at: row.get(3)?,
        archived: archived_at.is_some(),
        archived_at,
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
    fn migrates_legacy_channels_and_preserves_archive_state_on_reinit() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE channels (
                id TEXT PRIMARY KEY,
                name TEXT UNIQUE NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL
            );
            INSERT INTO channels VALUES ('legacy', 'builds', 'Build results', 123);",
        )
        .unwrap();
        let store = Store {
            conn: Mutex::new(conn),
        };

        store.init().unwrap();
        let channels = store.list_channels(false).unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].description, "Build results");
        assert_eq!(channels[0].created_at, 123);
        assert!(!channels[0].archived);
        assert_eq!(channels[0].archived_at, None);

        let archived = store.set_channel_archived("legacy", true).unwrap();
        store.init().unwrap();
        assert!(store.list_channels(false).unwrap().is_empty());
        let channels = store.list_channels(true).unwrap();
        assert_eq!(channels.len(), 1);
        assert!(channels[0].archived);
        assert_eq!(channels[0].archived_at, archived.archived_at);
    }

    #[test]
    fn updates_channel_details_and_archive_state() {
        let store = Store::open_in_memory().unwrap();
        let admin = store.bootstrap("admin").unwrap().unwrap();
        let channel = store.get_channel("general").unwrap().unwrap();
        let message = store
            .post_message(&channel.id, &admin.identity, "Build passed", None)
            .unwrap()
            .message;

        let updated = store
            .update_channel(
                &channel.id,
                UpdateChannelRequest {
                    name: Some(" #CI ".into()),
                    description: Some("Build results".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.id, channel.id);
        assert_eq!(updated.name, "ci");
        assert_eq!(updated.description, "Build results");
        assert!(store.get_channel("general").unwrap().is_none());

        let archived = store.set_channel_archived("ci", true).unwrap();
        assert!(archived.archived);
        let archive_time = archived.archived_at.expect("archive time was recorded");
        let retried = store.set_channel_archived("ci", true).unwrap();
        assert_eq!(retried.archived_at, Some(archive_time));
        assert_eq!(store.get_thread(&message.id).unwrap().parent.id, message.id);

        let restored = store.set_channel_archived("ci", false).unwrap();
        assert!(!restored.archived);
        assert_eq!(restored.archived_at, None);
        let active = store.list_channels(false).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, channel.id);
        assert_eq!(active[0].name, "ci");
        assert_eq!(active[0].description, "Build results");
    }

    #[test]
    fn deletes_channel_threads_and_all_unread_items() {
        let store = Store::open_in_memory().unwrap();
        let admin = store.bootstrap("admin").unwrap().unwrap();
        let bot = store
            .create_identity(CreateIdentityRequest {
                handle: "bot".into(),
                kind: IdentityKind::Agent,
                owner_id: Some(admin.identity.id.clone()),
            })
            .unwrap();
        let channel = store.get_channel("general").unwrap().unwrap();
        let root = store
            .post_message(&channel.id, &admin.identity, "@bot check CI", None)
            .unwrap();
        store
            .ack_unread(&bot.identity.id, &root.created_unread[0].id)
            .unwrap();
        store
            .post_message(&channel.id, &bot.identity, "Passed", Some(root.message.id))
            .unwrap();
        store.set_channel_archived(&channel.id, true).unwrap();

        let deleted = store.delete_channel(&channel.id).unwrap();
        assert_eq!(deleted.channel_id, channel.id);
        assert_eq!(deleted.unread_removed, [(admin.identity.id, 1)].into());
        let conn = store.conn.lock().unwrap();
        for table in ["channels", "messages", "unread_items"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} must be empty");
        }
    }

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

        // Human follow-up WITHOUT @mention should NOT enqueue the agent again
        let human_followup = store
            .post_message(
                "general",
                &admin.identity,
                "Thanks, discussing this with another human first.",
                Some(post.message.id.clone()),
            )
            .unwrap();
        assert!(human_followup.created_unread.is_empty());
        assert!(store.list_unread(&agent.identity.id).unwrap().is_empty());

        // Human follow-up WITH explicit @claude-local SHOULD enqueue the agent
        let human_ping = store
            .post_message(
                "general",
                &admin.identity,
                "@claude-local please open a PR for this",
                Some(post.message.id.clone()),
            )
            .unwrap();
        assert_eq!(human_ping.created_unread.len(), 1);
        assert_eq!(human_ping.created_unread[0].identity_id, agent.identity.id);
        assert_eq!(human_ping.created_unread[0].reason, UnreadReason::Mention);

        // Thread view includes parent + replies
        let thread = store.get_thread(&post.message.id).unwrap();
        assert_eq!(thread.parent.reply_count, 3);
        assert_eq!(thread.replies.len(), 3);

        // Deleting an individual thread reply is rejected
        assert!(store.delete_message(&reply.message.id).is_err());

        // Deleting parent message cascades to replies and unacked unread items
        let deleted = store.delete_message(&post.message.id).unwrap();
        assert_eq!(deleted.message_id, post.message.id);
        assert_eq!(deleted.affected_unread.len(), 2);
        assert!(store.list_channel_messages("general", 20).unwrap().is_empty());
        assert!(store.list_unread(&admin.identity.id).unwrap().is_empty());
    }
}
