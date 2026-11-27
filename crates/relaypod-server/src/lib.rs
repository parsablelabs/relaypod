use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, FromRequestParts, Path, Query, State,
    },
    http::{header, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use relaypod_core::{
    auth::normalize_handle, ActivityState, CoreError, CreateChannelRequest, CreateIdentityRequest,
    Identity, IdentityKind, PostMessageRequest, Store, UpdateChannelRequest, WsClientMessage,
    WsEvent,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Notify};
use tracing::{error, info, warn};

const PRESENCE_TTL: Duration = Duration::from_secs(30);
const MAX_HTTP_BODY_BYTES: usize = 256 * 1024;
const MAX_WS_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_ACTIVITY_PER_IDENTITY: usize = 16;
const MAX_ACTIVITY_ID_LEN: usize = 128;
const MAX_ACTIVITY_DETAIL_LEN: usize = 2048;

type ActivityKey = (String, String, Option<String>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActivityEntry {
    handle: String,
    state: ActivityState,
    pane_id: Option<String>,
    detail: Option<String>,
}

struct PresenceEntry {
    connections: usize,
    last_ping: Instant,
}

pub struct AppState {
    pub store: Arc<Store>,
    pub events: broadcast::Sender<WsEvent>,
    /// Internal signal carrying an `identity_id` whose token was rotated, so active WS sessions disconnect.
    token_revoked: broadcast::Sender<String>,
    /// Lowercased human handles granted admin privileges via `config.yaml` (`admin_handles`).
    admin_handles: HashSet<String>,
    presence: Mutex<HashMap<String, PresenceEntry>>,
    /// Active working/blocked indicators keyed by `(identity_id, channel_id, thread_id)`.
    activity: Mutex<HashMap<ActivityKey, ActivityEntry>>,
    // Keep board events in the same order as their committed writes.
    board_writes: Mutex<()>,
}

impl AppState {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        Self::with_admin_handles(store, Vec::<String>::new())
    }

    pub fn with_admin_handles<I, S>(store: Arc<Store>, admin_handles: I) -> Arc<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let (events, _) = broadcast::channel(512);
        let (token_revoked, _) = broadcast::channel(256);
        let admin_handles = admin_handles
            .into_iter()
            .map(|h| normalize_handle(&h.into()))
            .filter(|h| !h.is_empty())
            .collect();
        Arc::new(Self {
            store,
            events,
            token_revoked,
            admin_handles,
            presence: Mutex::new(HashMap::new()),
            activity: Mutex::new(HashMap::new()),
            board_writes: Mutex::new(()),
        })
    }

    pub fn is_admin(&self, identity: &Identity) -> bool {
        identity.kind == IdentityKind::Human
            && self.admin_handles.contains(&identity.handle.to_lowercase())
    }

    fn hydrate_identity(&self, identity: &mut Identity) {
        identity.online = self.is_online(&identity.id);
        identity.busy = self.is_busy(&identity.id);
        identity.is_admin = self.is_admin(identity);
    }

    pub fn emit(&self, event: WsEvent) {
        let _ = self.events.send(event);
    }

    pub fn is_online(&self, identity_id: &str) -> bool {
        let guard = self.presence.lock().expect("presence lock poisoned");
        guard
            .get(identity_id)
            .is_some_and(|e| e.connections > 0 && e.last_ping.elapsed() <= PRESENCE_TTL)
    }

    fn on_ws_connect(&self, identity_id: &str) {
        let became_online = {
            let mut guard = self.presence.lock().expect("presence lock poisoned");
            let now = Instant::now();
            let was_online = guard
                .get(identity_id)
                .is_some_and(|e| e.connections > 0 && e.last_ping.elapsed() <= PRESENCE_TTL);
            let entry = guard
                .entry(identity_id.to_string())
                .or_insert(PresenceEntry {
                    connections: 0,
                    last_ping: now,
                });
            entry.connections += 1;
            entry.last_ping = now;
            !was_online
        };

        if became_online {
            self.emit(WsEvent::PresenceChanged {
                identity_id: identity_id.to_string(),
                online: true,
            });
        }
    }

    fn on_ws_ping(&self, identity_id: &str) {
        let became_online = {
            let mut guard = self.presence.lock().expect("presence lock poisoned");
            let now = Instant::now();
            let was_online = guard
                .get(identity_id)
                .is_some_and(|e| e.connections > 0 && e.last_ping.elapsed() <= PRESENCE_TTL);
            let entry = guard
                .entry(identity_id.to_string())
                .or_insert(PresenceEntry {
                    connections: 1,
                    last_ping: now,
                });
            if entry.connections == 0 {
                entry.connections = 1;
            }
            entry.last_ping = now;
            !was_online
        };

        if became_online {
            self.emit(WsEvent::PresenceChanged {
                identity_id: identity_id.to_string(),
                online: true,
            });
        }
    }

    fn on_ws_disconnect(&self, identity_id: &str) {
        let became_offline = {
            let mut guard = self.presence.lock().expect("presence lock poisoned");
            if let Some(entry) = guard.get_mut(identity_id) {
                entry.connections = entry.connections.saturating_sub(1);
                if entry.connections == 0 {
                    guard.remove(identity_id);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        if became_offline {
            self.clear_activity_for_identity(identity_id);
            self.emit(WsEvent::PresenceChanged {
                identity_id: identity_id.to_string(),
                online: false,
            });
        }
    }

    fn sweep_stale_presence(&self) {
        let expired: Vec<String> = {
            let mut guard = self.presence.lock().expect("presence lock poisoned");
            let stale: Vec<String> = guard
                .iter()
                .filter(|(_, e)| e.last_ping.elapsed() > PRESENCE_TTL)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &stale {
                guard.remove(id);
            }
            stale
        };

        for identity_id in expired {
            self.clear_activity_for_identity(&identity_id);
            self.emit(WsEvent::PresenceChanged {
                identity_id,
                online: false,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn set_activity(
        &self,
        identity_id: &str,
        handle: &str,
        channel_id: String,
        thread_id: Option<String>,
        active: bool,
        state: ActivityState,
        pane_id: Option<String>,
        detail: Option<String>,
    ) {
        if channel_id.is_empty()
            || channel_id.len() > MAX_ACTIVITY_ID_LEN
            || thread_id
                .as_deref()
                .is_some_and(|t| t.is_empty() || t.len() > MAX_ACTIVITY_ID_LEN)
        {
            return;
        }
        let pane_id = pane_id
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty() && p.len() <= MAX_ACTIVITY_ID_LEN);
        let detail = detail.and_then(|d| {
            let trimmed = d.trim();
            if trimmed.is_empty() {
                return None;
            }
            if trimmed.len() <= MAX_ACTIVITY_DETAIL_LEN {
                Some(trimmed.to_string())
            } else {
                let mut end = MAX_ACTIVITY_DETAIL_LEN;
                while !trimmed.is_char_boundary(end) {
                    end -= 1;
                }
                Some(trimmed[..end].to_string())
            }
        });

        let emitted = {
            let mut guard = self.activity.lock().expect("activity lock poisoned");
            let key = (
                identity_id.to_string(),
                channel_id.clone(),
                thread_id.clone(),
            );
            if active {
                if !guard.contains_key(&key) {
                    let count = guard.keys().filter(|(id, _, _)| id == identity_id).count();
                    if count >= MAX_ACTIVITY_PER_IDENTITY {
                        return;
                    }
                }
                let next = ActivityEntry {
                    handle: handle.to_string(),
                    state,
                    pane_id,
                    detail,
                };
                if guard.get(&key) == Some(&next) {
                    None
                } else {
                    let out = (next.state, next.pane_id.clone(), next.detail.clone());
                    guard.insert(key, next);
                    Some(out)
                }
            } else {
                guard.remove(&key).map(|removed| (removed.state, None, None))
            }
        };

        if let Some((emitted_state, emitted_pane_id, emitted_detail)) = emitted {
            self.emit(WsEvent::ActivityChanged {
                identity_id: identity_id.to_string(),
                handle: handle.to_string(),
                channel_id,
                thread_id,
                active,
                state: emitted_state,
                pane_id: emitted_pane_id,
                detail: emitted_detail,
            });
        }
    }

    fn clear_activity_for_identity(&self, identity_id: &str) {
        let removed: Vec<(String, Option<String>, ActivityEntry)> = {
            let mut guard = self.activity.lock().expect("activity lock poisoned");
            let keys: Vec<(String, String, Option<String>)> = guard
                .keys()
                .filter(|(id, _, _)| id == identity_id)
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|k| guard.remove(&k).map(|entry| (k.1, k.2, entry)))
                .collect()
        };

        for (channel_id, thread_id, entry) in removed {
            self.emit(WsEvent::ActivityChanged {
                identity_id: identity_id.to_string(),
                handle: entry.handle,
                channel_id,
                thread_id,
                active: false,
                state: entry.state,
                pane_id: None,
                detail: None,
            });
        }
    }

    fn is_busy(&self, identity_id: &str) -> bool {
        let guard = self.activity.lock().expect("activity lock poisoned");
        guard.keys().any(|(id, _, _)| id == identity_id)
    }

    fn rename_activity_handle(&self, identity_id: &str, new_handle: &str) {
        let mut guard = self.activity.lock().expect("activity lock poisoned");
        for ((id, _, _), entry) in guard.iter_mut() {
            if id == identity_id {
                entry.handle = new_handle.to_string();
            }
        }
    }

    fn snapshot_active_events(&self) -> Vec<WsEvent> {
        let guard = self.activity.lock().expect("activity lock poisoned");
        guard
            .iter()
            .map(
                |((identity_id, channel_id, thread_id), entry)| WsEvent::ActivityChanged {
                    identity_id: identity_id.clone(),
                    handle: entry.handle.clone(),
                    channel_id: channel_id.clone(),
                    thread_id: thread_id.clone(),
                    active: true,
                    state: entry.state,
                    pane_id: entry.pane_id.clone(),
                    detail: entry.detail.clone(),
                },
            )
            .collect()
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl From<CoreError> for ApiError {
    fn from(err: CoreError) -> Self {
        match err {
            CoreError::NotFound(m) => Self::new(StatusCode::NOT_FOUND, m),
            CoreError::InvalidInput(m) => Self::new(StatusCode::BAD_REQUEST, m),
            CoreError::Conflict(m) => Self::new(StatusCode::CONFLICT, m),
            CoreError::Db(e) => {
                error!("database error: {e}");
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

struct AuthIdentity(Identity);

impl FromRequestParts<Arc<AppState>> for AuthIdentity {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_bearer_token(parts).ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "missing Authorization: Bearer <token>",
            )
        })?;

        let identity = state
            .store
            .authenticate(&token)
            .map_err(ApiError::from)?
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "invalid token"))?;

        Ok(AuthIdentity(identity))
    }
}

fn require_human(identity: &Identity) -> Result<(), ApiError> {
    if identity.kind != IdentityKind::Human {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "human identity required",
        ));
    }
    Ok(())
}

fn extract_bearer_token(parts: &Parts) -> Option<String> {
    if let Some(val) = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(token) = val.strip_prefix("Bearer ") {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    None
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/me", get(get_me))
        .route(
            "/api/identities",
            get(list_identities).post(create_identity),
        )
        .route(
            "/api/identities/{id}",
            patch(update_identity).delete(delete_identity),
        )
        .route(
            "/api/identities/{id}/token",
            post(regenerate_identity_token),
        )
        .route("/api/channels", get(list_channels).post(create_channel))
        .route(
            "/api/channels/{id}",
            patch(update_channel).delete(delete_channel),
        )
        .route(
            "/api/channels/{id}/messages",
            get(list_channel_messages).post(post_message),
        )
        .route("/api/messages/{id}", delete(delete_message))
        .route("/api/messages/{id}/thread", get(get_thread))
        .route("/api/unread", get(list_unread))
        .route("/api/unread/{id}/ack", post(ack_unread))
        .route("/api/ws", get(ws_handler))
        .layer(DefaultBodyLimit::max(MAX_HTTP_BODY_BYTES))
        .with_state(state)
}

pub async fn serve(
    addr: SocketAddr,
    store: Arc<Store>,
    admin_handles: Vec<String>,
) -> anyhow::Result<()> {
    if !addr.ip().is_loopback() {
        warn!(
            "RelayPod is binding to non-loopback address {addr} over plaintext HTTP/WS; terminate TLS with a reverse proxy"
        );
    }
    let state = AppState::with_admin_handles(store, admin_handles);
    let sweeper_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            sweeper_state.sweep_stale_presence();
        }
    });

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("RelayPod server listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok", "service": "relaypod" }))
}

async fn get_me(
    State(state): State<Arc<AppState>>,
    AuthIdentity(mut identity): AuthIdentity,
) -> Json<Identity> {
    state.hydrate_identity(&mut identity);
    Json(identity)
}

async fn list_identities(
    State(state): State<Arc<AppState>>,
    AuthIdentity(_): AuthIdentity,
) -> Result<Json<Vec<Identity>>, ApiError> {
    let mut items = state.store.list_identities()?;
    for item in &mut items {
        state.hydrate_identity(item);
    }
    Ok(Json(items))
}

async fn create_identity(
    State(state): State<Arc<AppState>>,
    AuthIdentity(caller): AuthIdentity,
    Json(mut req): Json<CreateIdentityRequest>,
) -> Result<(StatusCode, Json<relaypod_core::CreatedIdentity>), ApiError> {
    require_human(&caller)?;
    let _write = state
        .board_writes
        .lock()
        .expect("board write lock poisoned");
    if req.kind == IdentityKind::Human {
        if !state.is_admin(&caller) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "only admin humans can add human identities",
            ));
        }
        req.owner_id = None;
        req.agent_config = None;
    } else {
        req.owner_id = Some(caller.id);
        let cfg = req
            .agent_config
            .get_or_insert_with(relaypod_core::AgentConfig::default);
        if cfg.args.is_none() {
            if let Some(ref harness) = cfg.harness {
                if let Some(default_args) =
                    relaypod_core::AgentConfig::default_args_for_harness(harness)
                {
                    cfg.args = Some(default_args.to_string());
                }
            }
        }
        if cfg.cwd.is_none() {
            cfg.cwd = Some(relaypod_core::DEFAULT_AGENT_CWD.to_string());
        }
    }
    let mut created = state.store.create_identity(req)?;
    state.hydrate_identity(&mut created.identity);
    state.emit(WsEvent::IdentityCreated(created.identity.clone()));
    Ok((StatusCode::CREATED, Json(created)))
}

async fn update_identity(
    State(state): State<Arc<AppState>>,
    AuthIdentity(caller): AuthIdentity,
    Path(identity_id): Path<String>,
    Json(request): Json<relaypod_core::UpdateIdentityRequest>,
) -> Result<Json<Identity>, ApiError> {
    require_human(&caller)?;
    let _write = state
        .board_writes
        .lock()
        .expect("board write lock poisoned");

    let target = state.store.get_identity(&identity_id)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            format!("identity '{identity_id}' not found"),
        )
    })?;

    if target.kind != IdentityKind::Agent {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only agent identities can be updated",
        ));
    }
    if target.owner_id.as_deref() != Some(caller.id.as_str()) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only the human who registered this agent can update it",
        ));
    }

    let mut updated = state.store.update_identity(&target.id, request)?;
    state.rename_activity_handle(&updated.id, &updated.handle);
    state.hydrate_identity(&mut updated);
    state.emit(WsEvent::IdentityUpdated(updated.clone()));
    Ok(Json(updated))
}

async fn delete_identity(
    State(state): State<Arc<AppState>>,
    AuthIdentity(caller): AuthIdentity,
    Path(identity_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_human(&caller)?;
    let _write = state
        .board_writes
        .lock()
        .expect("board write lock poisoned");

    let target = state.store.get_identity(&identity_id)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            format!("identity '{identity_id}' not found"),
        )
    })?;

    match target.kind {
        IdentityKind::Human => {
            if !state.is_admin(&caller) {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "only admin humans can remove human identities",
                ));
            }
            if target.id == caller.id {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "cannot delete your own identity",
                ));
            }
        }
        IdentityKind::Agent => {
            if target.owner_id.as_deref() != Some(caller.id.as_str()) {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "only the human who registered this agent can delete it",
                ));
            }
        }
    }

    let deleted = state.store.delete_identity(&target.id)?;
    state.clear_activity_for_identity(&deleted.identity_id);
    {
        let mut guard = state.presence.lock().expect("presence lock poisoned");
        guard.remove(&deleted.identity_id);
    }
    state.emit(WsEvent::IdentityDeleted {
        identity_id: deleted.identity_id,
    });
    Ok(StatusCode::NO_CONTENT)
}

async fn regenerate_identity_token(
    State(state): State<Arc<AppState>>,
    AuthIdentity(caller): AuthIdentity,
    Path(identity_id): Path<String>,
) -> Result<Json<relaypod_core::CreatedIdentity>, ApiError> {
    require_human(&caller)?;
    let _write = state
        .board_writes
        .lock()
        .expect("board write lock poisoned");

    let target = state.store.get_identity(&identity_id)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            format!("identity '{identity_id}' not found"),
        )
    })?;

    match target.kind {
        IdentityKind::Human => {
            if !state.is_admin(&caller) {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "only admin humans can regenerate tokens for human identities",
                ));
            }
        }
        IdentityKind::Agent => {
            if !state.is_admin(&caller) && target.owner_id.as_deref() != Some(caller.id.as_str()) {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "only the human who registered this agent or an admin can regenerate its token",
                ));
            }
        }
    }

    let mut regenerated = state.store.regenerate_identity_token(&target.id)?;
    state.hydrate_identity(&mut regenerated.identity);
    let _ = state.token_revoked.send(target.id);
    Ok(Json(regenerated))
}

#[derive(Debug, Deserialize)]
struct ChannelListQuery {
    #[serde(default)]
    include_archived: bool,
}

async fn list_channels(
    State(state): State<Arc<AppState>>,
    AuthIdentity(_): AuthIdentity,
    Query(query): Query<ChannelListQuery>,
) -> Result<Json<Vec<relaypod_core::Channel>>, ApiError> {
    Ok(Json(state.store.list_channels(query.include_archived)?))
}

async fn create_channel(
    State(state): State<Arc<AppState>>,
    AuthIdentity(caller): AuthIdentity,
    Json(req): Json<CreateChannelRequest>,
) -> Result<(StatusCode, Json<relaypod_core::Channel>), ApiError> {
    require_human(&caller)?;
    let _write = state
        .board_writes
        .lock()
        .expect("board write lock poisoned");
    let channel = state.store.create_channel(req, Some(caller.id))?;
    state.emit(WsEvent::ChannelCreated(channel.clone()));
    Ok((StatusCode::CREATED, Json(channel)))
}

async fn update_channel(
    State(state): State<Arc<AppState>>,
    AuthIdentity(caller): AuthIdentity,
    Path(channel_id): Path<String>,
    Json(request): Json<UpdateChannelRequest>,
) -> Result<Json<relaypod_core::Channel>, ApiError> {
    require_human(&caller)?;
    let _write = state
        .board_writes
        .lock()
        .expect("board write lock poisoned");
    let channel = state.store.update_channel(&channel_id, request)?;
    state.emit(WsEvent::ChannelUpdated(channel.clone()));
    Ok(Json(channel))
}

async fn delete_channel(
    State(state): State<Arc<AppState>>,
    AuthIdentity(caller): AuthIdentity,
    Path(channel_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_human(&caller)?;
    let _write = state
        .board_writes
        .lock()
        .expect("board write lock poisoned");
    let channel = state.store.get_channel(&channel_id)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            format!("channel '{channel_id}' not found"),
        )
    })?;
    if !state.is_admin(&caller) && channel.created_by.as_deref() != Some(caller.id.as_str()) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only the human who created this channel or an admin can delete it",
        ));
    }
    let deleted = state.store.delete_channel(&channel.id)?;
    state.emit(WsEvent::ChannelDeleted {
        channel_id: deleted.channel_id,
        unread_removed: deleted.unread_removed,
    });
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct MessagesQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    50
}

async fn list_channel_messages(
    State(state): State<Arc<AppState>>,
    AuthIdentity(_): AuthIdentity,
    Path(channel_id): Path<String>,
    Query(query): Query<MessagesQuery>,
) -> Result<Json<Vec<relaypod_core::Message>>, ApiError> {
    let msgs = state
        .store
        .list_channel_messages(&channel_id, query.limit)?;
    Ok(Json(msgs))
}

async fn post_message(
    State(state): State<Arc<AppState>>,
    AuthIdentity(author): AuthIdentity,
    Path(channel_id): Path<String>,
    Json(req): Json<PostMessageRequest>,
) -> Result<(StatusCode, Json<relaypod_core::Message>), ApiError> {
    let _write = state
        .board_writes
        .lock()
        .expect("board write lock poisoned");
    let res = state
        .store
        .post_message(&channel_id, &author, &req.body, req.parent_id)?;

    state.set_activity(
        &author.id,
        &author.handle,
        res.message.channel_id.clone(),
        res.message.parent_id.clone(),
        false,
        ActivityState::Working,
        None,
        None,
    );
    state.emit(WsEvent::MessageCreated(res.message.clone()));
    for unread in res.created_unread {
        state.emit(WsEvent::UnreadCreated(unread));
    }

    Ok((StatusCode::CREATED, Json(res.message)))
}

async fn delete_message(
    State(state): State<Arc<AppState>>,
    AuthIdentity(caller): AuthIdentity,
    Path(message_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_human(&caller)?;
    let _write = state
        .board_writes
        .lock()
        .expect("board write lock poisoned");
    let info = state
        .store
        .get_message_delete_info(&message_id)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                format!("message '{message_id}' not found"),
            )
        })?;
    if info.parent_id.is_some() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "cannot delete an individual thread reply; delete the top-level message instead",
        ));
    }
    let allowed = state.is_admin(&caller)
        || info.author_id == caller.id
        || (info.author_kind == IdentityKind::Agent
            && info.author_owner_id.as_deref() == Some(caller.id.as_str()));
    if !allowed {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only an admin, the message author, or the owner of the authoring agent can delete this message",
        ));
    }
    let deleted = state.store.delete_message(&message_id)?;
    for (unread_id, identity_id) in deleted.affected_unread {
        state.emit(WsEvent::UnreadAcked {
            unread_id,
            identity_id,
        });
    }
    state.emit(WsEvent::MessageDeleted {
        message_id: deleted.message_id,
        channel_id: deleted.channel_id,
        parent_id: deleted.parent_id,
    });
    Ok(StatusCode::NO_CONTENT)
}

async fn get_thread(
    State(state): State<Arc<AppState>>,
    AuthIdentity(_): AuthIdentity,
    Path(message_id): Path<String>,
) -> Result<Json<relaypod_core::ThreadView>, ApiError> {
    let thread = state.store.get_thread(&message_id)?;
    Ok(Json(thread))
}

async fn list_unread(
    State(state): State<Arc<AppState>>,
    AuthIdentity(identity): AuthIdentity,
) -> Result<Json<Vec<relaypod_core::UnreadItem>>, ApiError> {
    let items = state.store.list_unread(&identity.id)?;
    Ok(Json(items))
}

async fn ack_unread(
    State(state): State<Arc<AppState>>,
    AuthIdentity(identity): AuthIdentity,
    Path(unread_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let _write = state
        .board_writes
        .lock()
        .expect("board write lock poisoned");
    state.store.ack_unread(&identity.id, &unread_id)?;
    state.emit(WsEvent::UnreadAcked {
        unread_id,
        identity_id: identity.id,
    });
    Ok(StatusCode::NO_CONTENT)
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    AuthIdentity(identity): AuthIdentity,
) -> impl IntoResponse {
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_ws(socket, state, identity))
}

async fn handle_ws(socket: WebSocket, state: Arc<AppState>, identity: Identity) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.events.subscribe();
    let mut token_revoked_rx = state.token_revoked.subscribe();

    state.on_ws_connect(&identity.id);
    info!("ws connected: @{}", identity.handle);

    let initial_activity = state.snapshot_active_events();
    let my_id = identity.id.clone();
    let my_kind = identity.kind;
    let revoked = Arc::new(Notify::new());
    let revoked_tx = Arc::clone(&revoked);
    let send_task = tokio::spawn(async move {
        for event in initial_activity {
            if let Ok(text) = serde_json::to_string(&event) {
                if sender.send(WsMessage::Text(text.into())).await.is_err() {
                    return;
                }
            }
        }
        loop {
            let event = match rx.recv().await {
                Ok(ev) => ev,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!("ws client {my_id} lagged by {skipped} events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };

            if let WsEvent::IdentityDeleted { ref identity_id } = event {
                if identity_id == &my_id {
                    revoked_tx.notify_one();
                    break;
                }
            }

            // Agents only receive their own unread queue events; humans see board-wide badges.
            if my_kind == IdentityKind::Agent {
                match &event {
                    WsEvent::UnreadCreated(item) if item.identity_id != my_id => continue,
                    WsEvent::UnreadAcked { identity_id, .. } if identity_id != &my_id => continue,
                    _ => {}
                }
            }

            match serde_json::to_string(&event) {
                Ok(text) => {
                    if sender.send(WsMessage::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("failed to serialize ws event: {e}"),
            }
        }
    });

    loop {
        tokio::select! {
            _ = revoked.notified() => break,
            revoked_id = token_revoked_rx.recv() => {
                match revoked_id {
                    Ok(id) if id == identity.id => break,
                    Err(broadcast::error::RecvError::Closed) => break,
                    _ => {}
                }
            }
            next = receiver.next() => {
                let Some(Ok(msg)) = next else { break; };
                match msg {
                    WsMessage::Text(text) => {
                        if let Ok(client_msg) = serde_json::from_str::<WsClientMessage>(&text) {
                            match client_msg {
                                WsClientMessage::Ping => state.on_ws_ping(&identity.id),
                                WsClientMessage::Activity {
                                    channel_id,
                                    thread_id,
                                    active,
                                    state: activity_state,
                                    pane_id,
                                    detail,
                                } => {
                                    state.on_ws_ping(&identity.id);
                                    let current_handle = state
                                        .store
                                        .get_identity(&identity.id)
                                        .ok()
                                        .flatten()
                                        .map(|i| i.handle)
                                        .unwrap_or_else(|| identity.handle.clone());
                                    state.set_activity(
                                        &identity.id,
                                        &current_handle,
                                        channel_id,
                                        thread_id,
                                        active,
                                        activity_state,
                                        pane_id,
                                        detail,
                                    );
                                }
                            }
                        }
                    }
                    WsMessage::Ping(_) => {
                        state.on_ws_ping(&identity.id);
                    }
                    WsMessage::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    send_task.abort();
    state.on_ws_disconnect(&identity.id);
    info!("ws disconnected: @{}", identity.handle);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_state_transitions_emit_and_snapshot_correctly() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let app = AppState::new(store);
        let mut rx = app.events.subscribe();

        app.set_activity(
            "a1",
            "codex",
            "c1".into(),
            Some("t1".into()),
            true,
            ActivityState::Working,
            None,
            None,
        );
        let ev1 = rx.try_recv().unwrap();
        assert!(matches!(
            ev1,
            WsEvent::ActivityChanged {
                active: true,
                state: ActivityState::Working,
                pane_id: None,
                detail: None,
                ..
            }
        ));

        // Duplicate identical state should not re-emit
        app.set_activity(
            "a1",
            "codex",
            "c1".into(),
            Some("t1".into()),
            true,
            ActivityState::Working,
            None,
            None,
        );
        assert!(rx.try_recv().is_err());

        // Transition Working -> Blocked emits pane_id & detail and persists in snapshot
        app.set_activity(
            "a1",
            "codex",
            "c1".into(),
            Some("t1".into()),
            true,
            ActivityState::Blocked,
            Some("w4:p7".into()),
            Some("Allow running `rm -rf build`?".into()),
        );
        let ev2 = rx.try_recv().unwrap();
        assert!(matches!(
            ev2,
            WsEvent::ActivityChanged {
                active: true,
                state: ActivityState::Blocked,
                pane_id: Some(ref p),
                detail: Some(ref d),
                ..
            } if p == "w4:p7" && d == "Allow running `rm -rf build`?"
        ));

        let snap = app.snapshot_active_events();
        assert_eq!(snap.len(), 1);
        assert!(matches!(
            snap[0],
            WsEvent::ActivityChanged {
                active: true,
                state: ActivityState::Blocked,
                pane_id: Some(ref p),
                detail: Some(ref d),
                ..
            } if p == "w4:p7" && d == "Allow running `rm -rf build`?"
        ));

        // Clearing emits active: false
        app.set_activity(
            "a1",
            "codex",
            "c1".into(),
            Some("t1".into()),
            false,
            ActivityState::Working,
            None,
            None,
        );
        let ev3 = rx.try_recv().unwrap();
        assert!(matches!(
            ev3,
            WsEvent::ActivityChanged { active: false, .. }
        ));
        assert!(app.snapshot_active_events().is_empty());
    }
}
