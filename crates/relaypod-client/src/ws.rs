use futures_util::{SinkExt, StreamExt};
use relaypod_core::{WsClientMessage, WsEvent};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{HeaderValue, AUTHORIZATION};
use tokio_tungstenite::tungstenite::Message as TungsteniteMsg;
use tracing::warn;

/// Default interval between presence keepalive pings sent over WebSocket.
pub const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(10);

/// Shared reconnecting WebSocket loop with periodic presence pings.
/// Used by both the desktop UI runtime and `relaypod watch` so both report
/// live presence identically.
pub async fn run_live_ws_loop<OnStatus, OnEvent>(
    ws_url: String,
    token: String,
    ping_interval: Duration,
    on_status: OnStatus,
    on_event: OnEvent,
) where
    OnStatus: FnMut(bool),
    OnEvent: FnMut(WsEvent),
{
    let (_tx, rx) = mpsc::unbounded_channel();
    run_live_ws_loop_with_outgoing(ws_url, token, ping_interval, rx, on_status, on_event).await;
}

/// Variant of [`run_live_ws_loop`] that also forwards outgoing [`WsClientMessage`]s
/// (such as agent working/activity indicators) over the active WebSocket connection.
pub async fn run_live_ws_loop_with_outgoing<OnStatus, OnEvent>(
    ws_url: String,
    token: String,
    ping_interval: Duration,
    mut outgoing_rx: mpsc::UnboundedReceiver<WsClientMessage>,
    mut on_status: OnStatus,
    mut on_event: OnEvent,
) where
    OnStatus: FnMut(bool),
    OnEvent: FnMut(WsEvent),
{
    let ping_payload = serde_json::to_string(&WsClientMessage::Ping)
        .unwrap_or_else(|_| r#"{"type":"ping"}"#.into());

    loop {
        let request = match build_ws_request(&ws_url, &token) {
            Ok(req) => req,
            Err(e) => {
                warn!("invalid WebSocket request for {ws_url}: {e}");
                on_status(false);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        match connect_async(request).await {
            Ok((ws_stream, _)) => {
                on_status(true);
                let (mut write, mut read) = ws_stream.split();
                let mut ticker = tokio::time::interval(ping_interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            if write.send(TungsteniteMsg::Text(ping_payload.clone().into())).await.is_err() {
                                break;
                            }
                        }
                        Some(msg) = outgoing_rx.recv() => {
                            if let Ok(text) = serde_json::to_string(&msg) {
                                if write.send(TungsteniteMsg::Text(text.into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                        frame = read.next() => {
                            match frame {
                                Some(Ok(TungsteniteMsg::Text(text))) => {
                                    if let Ok(ev) = serde_json::from_str::<WsEvent>(&text) {
                                        on_event(ev);
                                    }
                                }
                                Some(Ok(TungsteniteMsg::Close(_))) | Some(Err(_)) | None => {
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                on_status(false);
            }
            Err(e) => {
                warn!("WebSocket connection to {ws_url} failed: {e}");
                on_status(false);
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn build_ws_request(
    ws_url: &str,
    token: &str,
) -> anyhow::Result<tokio_tungstenite::tungstenite::http::Request<()>> {
    let mut req = ws_url.into_client_request()?;
    let auth_value = HeaderValue::from_str(&format!("Bearer {token}"))?;
    req.headers_mut().insert(AUTHORIZATION, auth_value);
    Ok(req)
}
