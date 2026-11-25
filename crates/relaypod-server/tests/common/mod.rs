use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::Router;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tower::ServiceExt;

pub async fn request(app: &Router, token: &str, method: &str, path: &str, body: Value) -> Response {
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

pub async fn read_body<T: DeserializeOwned>(response: Response, status: StatusCode) -> T {
    assert_eq!(response.status(), status);
    let body = to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("read test response");
    serde_json::from_slice(&body).expect("valid JSON response")
}
