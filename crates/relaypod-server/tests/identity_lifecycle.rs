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
            agent_config: None,
        })
        .unwrap();
    let agent = store
        .create_identity(CreateIdentityRequest {
            handle: "codex-local".into(),
            kind: IdentityKind::Agent,
            owner_id: Some(owner.identity.id.clone()),
            agent_config: None,
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
            agent_config: None,
        })
        .unwrap();
    let unused_agent = store
        .create_identity(CreateIdentityRequest {
            handle: "temp-bot".into(),
            kind: IdentityKind::Agent,
            owner_id: Some(owner.identity.id.clone()),
            agent_config: None,
        })
        .unwrap();
    let mentioned_agent = store
        .create_identity(CreateIdentityRequest {
            handle: "mentioned-bot".into(),
            kind: IdentityKind::Agent,
            owner_id: Some(owner.identity.id.clone()),
            agent_config: None,
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
            agent_config: None,
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
            agent_config: None,
        })
        .is_err());
}

#[tokio::test]
async fn updates_agent_config_json_and_broadcasts_ws_event() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let owner = store.bootstrap("bob").unwrap().unwrap();
    let state = AppState::new(Arc::clone(&store));
    let mut events = state.events.subscribe();
    let app = build_router(state);

    let created: relaypod_core::CreatedIdentity = read_body(
        request(
            &app,
            &owner.token,
            "POST",
            "/api/identities",
            json!({
                "handle": "claude-dev",
                "kind": "agent",
                "agent_config": {
                    "harness": "claude",
                    "args": "--model claude-sonnet-4-6",
                    "description": "General coding"
                }
            }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    let _ = events.try_recv();
    let cfg = created.identity.agent_config.as_ref().expect("agent_config");
    assert_eq!(cfg.harness.as_deref(), Some("claude"));
    assert_eq!(cfg.args.as_deref(), Some("--model claude-sonnet-4-6"));

    // Owner updates agent config via PATCH; WS broadcasts the updated config for watcher hot-reload.
    let updated: Identity = read_body(
        request(
            &app,
            &owner.token,
            "PATCH",
            &format!("/api/identities/{}", created.identity.id),
            json!({
                "agent_config": {
                    "harness": "claude",
                    "args": "--model claude-opus-4-6 --effort high",
                    "cooldown_secs": 3,
                    "description": "Deep debugging"
                }
            }),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    let updated_cfg = updated.agent_config.as_ref().expect("updated config");
    assert_eq!(updated_cfg.cooldown_secs, Some(3));
    assert_eq!(
        updated_cfg.args.as_deref(),
        Some("--model claude-opus-4-6 --effort high")
    );

    assert!(matches!(
        events.try_recv().unwrap(),
        WsEvent::IdentityUpdated(id)
            if id.id == created.identity.id
                && id.agent_config.as_ref().and_then(|c| c.cooldown_secs) == Some(3)
    ));

    // Other agents/humans can inspect agent_config via GET /api/identities for routing.
    let list: Vec<Identity> = read_body(
        request(&app, &created.token, "GET", "/api/identities", Value::Null).await,
        StatusCode::OK,
    )
    .await;
    let found = list
        .iter()
        .find(|i| i.id == created.identity.id)
        .expect("agent in list");
    assert_eq!(
        found.agent_config.as_ref().and_then(|c| c.description.as_deref()),
        Some("Deep debugging")
    );

    // Creating a claude or codex agent without explicit `args` populates default non-interactive args,
    // which the owner can later clear or modify via PATCH.
    let claude_default: relaypod_core::CreatedIdentity = read_body(
        request(
            &app,
            &owner.token,
            "POST",
            "/api/identities",
            json!({
                "handle": "claude-auto",
                "kind": "agent",
                "agent_config": { "harness": "claude" }
            }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(
        claude_default
            .identity
            .agent_config
            .as_ref()
            .and_then(|c| c.args.as_deref()),
        Some("--disallowedTools AskUserQuestion")
    );

    let codex_default: relaypod_core::CreatedIdentity = read_body(
        request(
            &app,
            &owner.token,
            "POST",
            "/api/identities",
            json!({
                "handle": "codex-auto",
                "kind": "agent",
                "agent_config": { "harness": "codex" }
            }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(
        codex_default
            .identity
            .agent_config
            .as_ref()
            .and_then(|c| c.args.as_deref()),
        Some("-c features.default_mode_request_user_input=false")
    );

    // Explicitly passing empty `args: ""` opts out of default args.
    let claude_opt_out: relaypod_core::CreatedIdentity = read_body(
        request(
            &app,
            &owner.token,
            "POST",
            "/api/identities",
            json!({
                "handle": "claude-raw",
                "kind": "agent",
                "agent_config": { "harness": "claude", "args": "" }
            }),
        )
        .await,
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(
        claude_opt_out
            .identity
            .agent_config
            .as_ref()
            .and_then(|c| c.args.as_deref()),
        None
    );
}
