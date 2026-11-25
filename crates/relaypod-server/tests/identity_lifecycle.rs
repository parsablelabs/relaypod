mod common;

use axum::http::StatusCode;
use common::{read_body, request};
use relaypod_core::{CreateIdentityRequest, Identity, IdentityKind, Store, WsEvent};
use relaypod_server::{build_router, AppState};
use serde_json::{json, Value};
use std::sync::Arc;

#[tokio::test]
async fn only_owning_human_can_rename_agent_and_history_persists() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let owner = store.bootstrap("bob").unwrap().unwrap();
    let other_human = store
        .create_identity(CreateIdentityRequest {
            handle: "alice".into(),
            kind: IdentityKind::Human,
            owner_id: None,
        })
        .unwrap();
    let agent = store
        .create_identity(CreateIdentityRequest {
            handle: "codex-local".into(),
            kind: IdentityKind::Agent,
            owner_id: Some(owner.identity.id.clone()),
        })
        .unwrap();
    let general = store.get_channel("general").unwrap().unwrap();
    store
        .post_message(&general.id, &agent.identity, "Initial PR opened", None)
        .unwrap();

    let state = AppState::new(Arc::clone(&store));
    let mut events = state.events.subscribe();
    let app = build_router(state);
    let agent_path = format!("/api/identities/{}", agent.identity.id);

    // Agent itself cannot rename
    assert_eq!(
        request(&app, &agent.token, "PATCH", &agent_path, json!({"handle": "codex"}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );

    // Different human cannot rename another human's agent
    assert_eq!(
        request(
            &app,
            &other_human.token,
            "PATCH",
            &agent_path,
            json!({"handle": "codex"})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );

    // Human identity itself cannot be renamed via this endpoint
    assert_eq!(
        request(
            &app,
            &owner.token,
            "PATCH",
            &format!("/api/identities/{}", owner.identity.id),
            json!({"handle": "bob2"})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );

    // Duplicate handle is rejected with 409
    assert_eq!(
        request(&app, &owner.token, "PATCH", &agent_path, json!({"handle": "@alice"}))
            .await
            .status(),
        StatusCode::CONFLICT
    );

    // Owning human renames their agent
    let renamed: Identity = read_body(
        request(
            &app,
            &owner.token,
            "PATCH",
            &agent_path,
            json!({"handle": "@codex"}),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(renamed.id, agent.identity.id);
    assert_eq!(renamed.handle, "codex");

    // WebSocket broadcast emitted
    assert!(matches!(
        events.try_recv().unwrap(),
        WsEvent::IdentityUpdated(id) if id.id == agent.identity.id && id.handle == "codex"
    ));

    // Existing bearer token still authenticates as the renamed identity
    let me: Identity = read_body(
        request(&app, &agent.token, "GET", "/api/me", Value::Null).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(me.id, agent.identity.id);
    assert_eq!(me.handle, "codex");

    // Past messages reflect the updated handle automatically
    let msgs = store.list_channel_messages(&general.id, 10).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].author_handle, "codex");
}

#[tokio::test]
async fn deletes_agent_hard_without_mentions_and_soft_with_messages_or_mentions() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let owner = store.bootstrap("bob").unwrap().unwrap();
    let other_human = store
        .create_identity(CreateIdentityRequest {
            handle: "alice".into(),
            kind: IdentityKind::Human,
            owner_id: None,
        })
        .unwrap();
    let unused_agent = store
        .create_identity(CreateIdentityRequest {
            handle: "temp-bot".into(),
            kind: IdentityKind::Agent,
            owner_id: Some(owner.identity.id.clone()),
        })
        .unwrap();
    let mentioned_agent = store
        .create_identity(CreateIdentityRequest {
            handle: "mentioned-bot".into(),
            kind: IdentityKind::Agent,
            owner_id: Some(owner.identity.id.clone()),
        })
        .unwrap();
    let general = store.get_channel("general").unwrap().unwrap();
    store
        .post_message(&general.id, &owner.identity, "Hey @mentioned-bot check this", None)
        .unwrap();

    let state = AppState::new(Arc::clone(&store));
    let mut events = state.events.subscribe();
    let app = build_router(state);

    let unused_path = format!("/api/identities/{}", unused_agent.identity.id);
    let mentioned_path = format!("/api/identities/{}", mentioned_agent.identity.id);

    // Non-owner, agent, and human target are forbidden
    assert_eq!(
        request(&app, &other_human.token, "DELETE", &unused_path, Value::Null)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&app, &unused_agent.token, "DELETE", &unused_path, Value::Null)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            &owner.token,
            "DELETE",
            &format!("/api/identities/{}", owner.identity.id),
            Value::Null
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );

    // 1. Unused agent (no messages, no mentions) -> hard deleted
    assert_eq!(
        request(&app, &owner.token, "DELETE", &unused_path, Value::Null)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert!(matches!(
        events.try_recv().unwrap(),
        WsEvent::IdentityDeleted { identity_id } if identity_id == unused_agent.identity.id
    ));
    // Handle is immediately reusable because it was hard-deleted
    let recreated = store
        .create_identity(CreateIdentityRequest {
            handle: "temp-bot".into(),
            kind: IdentityKind::Agent,
            owner_id: Some(owner.identity.id.clone()),
        })
        .unwrap();
    assert_ne!(recreated.identity.id, unused_agent.identity.id);

    // 2. Mentioned agent -> soft deleted (token revoked, hidden from list, messages intact)
    assert_eq!(
        request(&app, &owner.token, "DELETE", &mentioned_path, Value::Null)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert!(matches!(
        events.try_recv().unwrap(),
        WsEvent::IdentityDeleted { identity_id } if identity_id == mentioned_agent.identity.id
    ));
    assert_eq!(
        request(&app, &mentioned_agent.token, "GET", "/api/me", Value::Null)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let active_ids = store.list_identities().unwrap();
    assert!(!active_ids.iter().any(|i| i.id == mentioned_agent.identity.id));
    // Re-creating the same handle conflicts because the soft-deleted identity preserves mention history
    assert!(store
        .create_identity(CreateIdentityRequest {
            handle: "mentioned-bot".into(),
            kind: IdentityKind::Agent,
            owner_id: Some(owner.identity.id.clone()),
        })
        .is_err());
}
