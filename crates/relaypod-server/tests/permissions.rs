mod common;

use axum::http::StatusCode;
use common::{read_body, request};
use relaypod_core::{Channel, CreatedIdentity, Identity, Message, Store};
use relaypod_server::{build_router, AppState};
use serde_json::{json, Value};
use std::sync::Arc;

#[tokio::test]
async fn enforces_human_and_config_admin_permission_matrix() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let admin = store.bootstrap("admin").unwrap().unwrap();

    // Only `admin` is listed in `admin_handles` (defined from config.yaml, not DB).
    let state = AppState::with_admin_handles(Arc::clone(&store), ["admin"]);
    let app = build_router(state);

    // `/api/me` reflects dynamic `is_admin` from config.
    let admin_me: Identity = read_body(
        request(&app, &admin.token, "GET", "/api/me", Value::Null).await,
        StatusCode::OK,
    )
    .await;
    assert!(admin_me.is_admin);

    // Admin human can add another human handle.
    let alice: CreatedIdentity = read_body(
        request(
            &app,
            &admin.token,
            "POST",
            "/api/identities",
            json!({ "handle": "alice", "kind": "human" }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    assert!(!alice.identity.is_admin);

    // Non-admin human cannot add another human handle.
    assert_eq!(
        request(
            &app,
            &alice.token,
            "POST",
            "/api/identities",
            json!({ "handle": "bob", "kind": "human" }),
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );

    // Any human can add an agent handle.
    let alice_bot: CreatedIdentity = read_body(
        request(
            &app,
            &alice.token,
            "POST",
            "/api/identities",
            json!({ "handle": "alice-bot", "kind": "agent" }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(
        alice_bot.identity.owner_id.as_deref(),
        Some(alice.identity.id.as_str())
    );

    let admin_bot: CreatedIdentity = read_body(
        request(
            &app,
            &admin.token,
            "POST",
            "/api/identities",
            json!({ "handle": "admin-bot", "kind": "agent" }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;

    // Humans can only remove agent handles they added (even admin cannot remove someone else's agent).
    assert_eq!(
        request(
            &app,
            &alice.token,
            "DELETE",
            &format!("/api/identities/{}", admin_bot.identity.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            &admin.token,
            "DELETE",
            &format!("/api/identities/{}", alice_bot.identity.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            &alice.token,
            "DELETE",
            &format!("/api/identities/{}", alice_bot.identity.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );

    // Any human can create and update channels.
    let alice_channel: Channel = read_body(
        request(
            &app,
            &alice.token,
            "POST",
            "/api/channels",
            json!({ "name": "alice-room", "description": "Alice's room" }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(
        alice_channel.created_by.as_deref(),
        Some(alice.identity.id.as_str())
    );

    let admin_channel: Channel = read_body(
        request(
            &app,
            &admin.token,
            "POST",
            "/api/channels",
            json!({ "name": "admin-room", "description": "Admin's room" }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;

    // Non-admin human can update any channel, but can only delete channels they created.
    let updated_admin_ch: Channel = read_body(
        request(
            &app,
            &alice.token,
            "PATCH",
            &format!("/api/channels/{}", admin_channel.id),
            json!({ "description": "Updated by Alice" }),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(updated_admin_ch.description, "Updated by Alice");

    assert_eq!(
        request(
            &app,
            &alice.token,
            "DELETE",
            &format!("/api/channels/{}", admin_channel.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );

    // Admin human can modify and delete any channel (including one created by Alice).
    assert_eq!(
        request(
            &app,
            &admin.token,
            "DELETE",
            &format!("/api/channels/{}", alice_channel.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );

    // Regular human can delete a channel they created themselves.
    let alice_channel2: Channel = read_body(
        request(
            &app,
            &alice.token,
            "POST",
            "/api/channels",
            json!({ "name": "alice-temp", "description": "" }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(
        request(
            &app,
            &alice.token,
            "DELETE",
            &format!("/api/channels/{}", alice_channel2.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );

    // Non-admin human cannot remove any human handle.
    assert_eq!(
        request(
            &app,
            &alice.token,
            "DELETE",
            &format!("/api/identities/{}", admin.identity.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );

    // Admin human cannot delete themselves, but can remove another human handle.
    assert_eq!(
        request(
            &app,
            &admin.token,
            "DELETE",
            &format!("/api/identities/{}", admin.identity.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            &app,
            &admin.token,
            "DELETE",
            &format!("/api/identities/{}", alice.identity.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );

    // Deleted human's token is immediately rejected.
    assert_eq!(
        request(&app, &alice.token, "GET", "/api/me", Value::Null)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn enforces_message_delete_permissions() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let admin = store.bootstrap("admin").unwrap().unwrap();
    let state = AppState::with_admin_handles(Arc::clone(&store), ["admin"]);
    let app = build_router(state);

    let alice: CreatedIdentity = read_body(
        request(
            &app,
            &admin.token,
            "POST",
            "/api/identities",
            json!({ "handle": "alice", "kind": "human" }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    let bob: CreatedIdentity = read_body(
        request(
            &app,
            &admin.token,
            "POST",
            "/api/identities",
            json!({ "handle": "bob", "kind": "human" }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    let alice_bot: CreatedIdentity = read_body(
        request(
            &app,
            &alice.token,
            "POST",
            "/api/identities",
            json!({ "handle": "alice-bot", "kind": "agent" }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;

    // Post top-level messages from alice, alice-bot, and bob, plus a reply.
    let alice_msg: Message = read_body(
        request(
            &app,
            &alice.token,
            "POST",
            "/api/channels/general/messages",
            json!({ "body": "alice root" }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    let reply_msg: Message = read_body(
        request(
            &app,
            &alice.token,
            "POST",
            "/api/channels/general/messages",
            json!({ "body": "thread reply", "parent_id": alice_msg.id }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    let bot_msg: Message = read_body(
        request(
            &app,
            &alice_bot.token,
            "POST",
            "/api/channels/general/messages",
            json!({ "body": "bot root" }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    let bob_msg: Message = read_body(
        request(
            &app,
            &bob.token,
            "POST",
            "/api/channels/general/messages",
            json!({ "body": "bob root" }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;

    // Agent cannot delete even its own message.
    assert_eq!(
        request(
            &app,
            &alice_bot.token,
            "DELETE",
            &format!("/api/messages/{}", bot_msg.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );

    // Thread reply cannot be deleted even by admin or author.
    assert_eq!(
        request(
            &app,
            &admin.token,
            "DELETE",
            &format!("/api/messages/{}", reply_msg.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );

    // Bob cannot delete alice's message or alice-bot's message.
    assert_eq!(
        request(
            &app,
            &bob.token,
            "DELETE",
            &format!("/api/messages/{}", alice_msg.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            &bob.token,
            "DELETE",
            &format!("/api/messages/{}", bot_msg.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );

    // Alice can delete her own agent's top-level message and her own top-level message.
    assert_eq!(
        request(
            &app,
            &alice.token,
            "DELETE",
            &format!("/api/messages/{}", bot_msg.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(
            &app,
            &alice.token,
            "DELETE",
            &format!("/api/messages/{}", alice_msg.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );

    // Admin can delete bob's top-level message.
    assert_eq!(
        request(
            &app,
            &admin.token,
            "DELETE",
            &format!("/api/messages/{}", bob_msg.id),
            Value::Null,
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
}
