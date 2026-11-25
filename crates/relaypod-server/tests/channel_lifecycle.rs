use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::Router;
use relaypod_core::{Channel, CreateIdentityRequest, IdentityKind, Store, WsEvent};
use relaypod_server::{build_router, AppState};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

async fn request(app: &Router, token: &str, method: &str, path: &str, body: Value) -> Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("valid test request"),
        )
        .await
        .expect("router handles test request")
}

async fn read_body<T: DeserializeOwned>(response: Response, status: StatusCode) -> T {
    assert_eq!(response.status(), status);
    let body = to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("read test response");
    serde_json::from_slice(&body).expect("valid JSON response")
}

#[tokio::test]
async fn humans_manage_channels_and_deletion_cascades() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let human = store.bootstrap("admin").unwrap().unwrap();
    let agent = store
        .create_identity(CreateIdentityRequest {
            handle: "bot".into(),
            display_name: None,
            kind: IdentityKind::Agent,
            owner_id: Some(human.identity.id.clone()),
        })
        .unwrap();
    let channel = store
        .create_channel(relaypod_core::CreateChannelRequest {
            name: "builds".into(),
            description: "CI activity".into(),
        })
        .unwrap();
    let post = store
        .post_message(&channel.id, &human.identity, "@bot check CI", None)
        .unwrap();
    let root_id = post.message.id;
    let reply = store
        .post_message(
            &channel.id,
            &agent.identity,
            "Passed",
            Some(root_id.clone()),
        )
        .unwrap();
    store
        .ack_unread(&human.identity.id, &reply.created_unread[0].id)
        .unwrap();
    store
        .post_message("general", &human.identity, "@bot keep this", None)
        .unwrap();

    let state = AppState::new(Arc::clone(&store));
    let mut events = state.events.subscribe();
    let app = build_router(state);
    let path = format!("/api/channels/{}", channel.id);
    let messages = format!("{path}/messages");

    for (method, body) in [
        ("PATCH", json!({"archived": true})),
        ("PATCH", json!({"archived": false})),
        ("PATCH", json!({"name": "renamed"})),
        ("DELETE", Value::Null),
    ] {
        assert_eq!(
            request(&app, &agent.token, method, &path, body)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        request(
            &app,
            &agent.token,
            "POST",
            "/api/identities",
            json!({"handle": "fake-admin", "kind": "human"})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            &agent.token,
            "DELETE",
            &format!("/api/messages/{root_id}"),
            Value::Null
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );

    let duplicate = request(
        &app,
        &human.token,
        "PATCH",
        &path,
        json!({"name": "#GENERAL", "archived": true}),
    )
    .await;
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    assert!(store
        .get_channel(&channel.id)
        .unwrap()
        .unwrap()
        .archived_at
        .is_none());
    assert!(events.try_recv().is_err());

    let archived: Channel = read_body(
        request(
            &app,
            &human.token,
            "PATCH",
            &path,
            json!({"name": "#CI", "description": "", "archived": true}),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(archived.name, "ci");
    assert_eq!(archived.description, "");
    assert!(archived.archived);
    assert!(archived.archived_at.is_some());
    assert!(
        matches!(events.try_recv().unwrap(), WsEvent::ChannelUpdated(ch) if ch.id == channel.id && ch.name == "ci" && ch.description.is_empty() && ch.archived && ch.archived_at == archived.archived_at)
    );
    assert!(store
        .list_unread(&agent.identity.id)
        .unwrap()
        .iter()
        .any(|item| item.channel_name == "ci"));

    let default_list: Vec<Channel> = read_body(
        request(&app, &agent.token, "GET", "/api/channels", Value::Null).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(default_list.len(), 1);
    let active_list: Vec<Channel> = read_body(
        request(
            &app,
            &agent.token,
            "GET",
            "/api/channels?include_archived=false",
            Value::Null,
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(active_list.len(), 1);
    assert_eq!(active_list[0].id, default_list[0].id);
    let full_list: Vec<Channel> = read_body(
        request(
            &app,
            &agent.token,
            "GET",
            "/api/channels?include_archived=true",
            Value::Null,
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(full_list.len(), 2);
    assert_eq!(
        request(&app, &agent.token, "GET", &messages, Value::Null)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(store.get_thread(&root_id).unwrap().replies.len(), 1);

    for parent in [None, Some(&root_id)] {
        assert_eq!(
            request(
                &app,
                &human.token,
                "POST",
                &messages,
                json!({"body": "hello", "parent_id": parent})
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
    }
    let unarchived: Channel = read_body(
        request(
            &app,
            &human.token,
            "PATCH",
            &path,
            json!({"archived": false}),
        )
        .await,
        StatusCode::OK,
    )
    .await;
    assert!(!unarchived.archived);
    assert!(unarchived.archived_at.is_none());
    assert!(
        matches!(events.try_recv().unwrap(), WsEvent::ChannelUpdated(ch) if !ch.archived && ch.archived_at.is_none())
    );
    assert_eq!(
        request(
            &app,
            &agent.token,
            "POST",
            &messages,
            json!({"body": "back online", "parent_id": root_id})
        )
        .await
        .status(),
        StatusCode::CREATED
    );
    assert!(matches!(
        events.try_recv().unwrap(),
        WsEvent::MessageCreated(_)
    ));
    assert!(matches!(
        events.try_recv().unwrap(),
        WsEvent::UnreadCreated(_)
    ));

    assert_eq!(
        request(&app, &human.token, "DELETE", &path, Value::Null)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert!(
        matches!(events.try_recv().unwrap(), WsEvent::ChannelDeleted { channel_id, unread_removed } if channel_id == channel.id && unread_removed[&agent.identity.id] == 1 && unread_removed[&human.identity.id] == 1)
    );
    assert!(store.get_thread(&root_id).is_err());
    assert!(store.get_channel(&channel.id).unwrap().is_none());
    assert!(store.list_unread(&human.identity.id).unwrap().is_empty());
    let remaining = store.list_unread(&agent.identity.id).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].channel_name, "general");
}
