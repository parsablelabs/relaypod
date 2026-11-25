use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        FromRequestParts, Path, Query, State,
    },
    http::{header, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use relaypod_core::{
    CoreError, CreateChannelRequest, CreateIdentityRequest, Identity, IdentityKind,
    PostMessageRequest, Store, UpdateChannelRequest, WsClientMessage, WsEvent,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, warn};

const PRESENCE_TTL: Duration = Duration::from_secs(30);

struct PresenceEntry {
    connections: usize,
    last_ping: Instant,
}

pub struct AppState {
    pub store: Arc<Store>,
    pub events: broadcast::Sender<WsEvent>,
    presence: Mutex<HashMap<String, PresenceEntry>>,
    /// Active working/typing indicators keyed by `(identity_id, channel_id, thread_id) -> handle`.
    activity: Mutex<HashMap<(String, String, Option<String>), String>>,
    // Keep board events in the same order as their committed writes.
    board_writes: Mutex<()>,
}

impl AppState {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        let (events, _) = broadcast::channel(512);
        Arc::new(Self {
            store,
            events,
            presence: Mutex::new(HashMap::new()),
            activity: Mutex::new(HashMap::new()),
            board_writes: Mutex::new(()),
        })
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
            let entry = guard.entry(identity_id.to_string()).or_insert(PresenceEntry {
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
            let entry = guard.entry(identity_id.to_string()).or_insert(PresenceEntry {
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

    fn set_activity(
        &self,
        identity_id: &str,
        handle: &str,
        channel_id: String,
        thread_id: Option<String>,
        active: bool,
    ) {
        let changed = {
            let mut guard = self.activity.lock().expect("activity lock poisoned");
            let key = (identity_id.to_string(), channel_id.clone(), thread_id.clone());
            if active {
                guard.insert(key, handle.to_string());
                true
            } else {
                guard.remove(&key).is_some()
            }
        };

        if changed {
            self.emit(WsEvent::ActivityChanged {
                identity_id: identity_id.to_string(),
                handle: handle.to_string(),
                channel_id,
                thread_id,
                active,
            });
        }
    }

    fn clear_activity_for_identity(&self, identity_id: &str) {
        let removed: Vec<(String, Option<String>, String)> = {
            let mut guard = self.activity.lock().expect("activity lock poisoned");
            let keys: Vec<(String, String, Option<String>)> = guard
                .keys()
                .filter(|(id, _, _)| id == identity_id)
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|k| guard.remove(&k).map(|h| (k.1, k.2, h)))
                .collect()
        };

        for (channel_id, thread_id, handle) in removed {
            self.emit(WsEvent::ActivityChanged {
                identity_id: identity_id.to_string(),
                handle,
                channel_id,
                thread_id,
                active: false,
            });
        }
    }

    fn is_busy(&self, identity_id: &str) -> bool {
        let guard = self.activity.lock().expect("activity lock poisoned");
        guard.keys().any(|(id, _, _)| id == identity_id)
    }

    fn rename_activity_handle(&self, identity_id: &str, new_handle: &str) {
        let mut guard = self.activity.lock().expect("activity lock poisoned");
        for ((id, _, _), handle) in guard.iter_mut() {
            if id == identity_id {
                *handle = new_handle.to_string();
            }
        }
    }

    fn snapshot_active_events(&self) -> Vec<WsEvent> {
        let guard = self.activity.lock().expect("activity lock poisoned");
        guard
            .iter()
            .map(|((identity_id, channel_id, thread_id), handle)| WsEvent::ActivityChanged {
                identity_id: identity_id.clone(),
                handle: handle.clone(),
                channel_id: channel_id.clone(),
                thread_id: thread_id.clone(),
                active: true,
            })
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
            CoreError::Db(e) => Self::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(ErrorBody { error: self.message })).into_response()
    }
}

struct AuthIdentity(Identity);

impl FromRequestParts<Arc<AppState>> for AuthIdentity {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_bearer_or_query_token(parts).ok_or_else(|| {
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

fn extract_bearer_or_query_token(parts: &Parts) -> Option<String> {
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

    if let Some(query) = parts.uri.query() {
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == "token" && !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }

    None
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/health", get(health))
        .route("/api/me", get(get_me))
        .route("/api/identities", get(list_identities).post(create_identity))
        .route(
            "/api/identities/{id}",
            patch(update_identity).delete(delete_identity),
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
        .layer(cors)
        .with_state(state)
}

pub async fn serve(addr: SocketAddr, store: Arc<Store>) -> anyhow::Result<()> {
    let state = AppState::new(store);
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
    identity.online = state.is_online(&identity.id);
    identity.busy = state.is_busy(&identity.id);
    Json(identity)
}

async fn list_identities(
    State(state): State<Arc<AppState>>,
    AuthIdentity(_): AuthIdentity,
) -> Result<Json<Vec<Identity>>, ApiError> {
    let mut items = state.store.list_identities()?;
    for item in &mut items {
        item.online = state.is_online(&item.id);
        item.busy = state.is_busy(&item.id);
    }
    Ok(Json(items))
}

async fn create_identity(
    State(state): State<Arc<AppState>>,
    AuthIdentity(caller): AuthIdentity,
    Json(mut req): Json<CreateIdentityRequest>,
) -> Result<(StatusCode, Json<relaypod_core::CreatedIdentity>), ApiError> {
    if req.kind == IdentityKind::Human {
        require_human(&caller)?;
    } else if req.owner_id.is_none() {
        req.owner_id = Some(caller.id);
    }
    let created = state.store.create_identity(req)?;
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

    let target = state
        .store
        .get_identity(&identity_id)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("identity '{identity_id}' not found")))?;

    if target.kind != IdentityKind::Agent {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only agent identities can be renamed",
        ));
    }
    if target.owner_id.as_deref() != Some(caller.id.as_str()) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only the human who registered this agent can rename it",
        ));
    }

    let mut updated = state.store.update_identity(&target.id, request)?;
    state.rename_activity_handle(&updated.id, &updated.handle);
    updated.online = state.is_online(&updated.id);
    updated.busy = state.is_busy(&updated.id);
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

    let target = state
        .store
        .get_identity(&identity_id)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                format!("identity '{identity_id}' not found"),
            )
        })?;

    if target.kind != IdentityKind::Agent {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only agent identities can be deleted",
        ));
    }
    if target.owner_id.as_deref() != Some(caller.id.as_str()) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only the human who registered this agent can delete it",
        ));
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
    AuthIdentity(_): AuthIdentity,
    Json(req): Json<CreateChannelRequest>,
) -> Result<(StatusCode, Json<relaypod_core::Channel>), ApiError> {
    let _write = state
        .board_writes
        .lock()
        .expect("board write lock poisoned");
    let channel = state.store.create_channel(req)?;
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
    let deleted = state.store.delete_channel(&channel_id)?;
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
    let msgs = state.store.list_channel_messages(&channel_id, query.limit)?;
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
    ws.on_upgrade(move |socket| handle_ws(socket, state, identity))
}

async fn handle_ws(socket: WebSocket, state: Arc<AppState>, identity: Identity) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.events.subscribe();

    state.on_ws_connect(&identity.id);
    info!("ws connected: @{}", identity.handle);

    let initial_activity = state.snapshot_active_events();
    let send_task = tokio::spawn(async move {
        for event in initial_activity {
            if let Ok(text) = serde_json::to_string(&event) {
                if sender.send(WsMessage::Text(text.into())).await.is_err() {
                    return;
                }
            }
        }
        while let Ok(event) = rx.recv().await {
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

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            WsMessage::Text(text) => {
                if let Ok(client_msg) = serde_json::from_str::<WsClientMessage>(&text) {
                    match client_msg {
                        WsClientMessage::Ping => state.on_ws_ping(&identity.id),
                        WsClientMessage::Activity {
                            channel_id,
                            thread_id,
                            active,
                        } => {
                            state.on_ws_ping(&identity.id);
                            state.set_activity(
                                &identity.id,
                                &identity.handle,
                                channel_id,
                                thread_id,
                                active,
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

    send_task.abort();
    state.on_ws_disconnect(&identity.id);
    info!("ws disconnected: @{}", identity.handle);
}
