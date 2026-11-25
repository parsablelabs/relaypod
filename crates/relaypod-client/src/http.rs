use anyhow::{anyhow, Context, Result};
use relaypod_core::{
    Channel, CreateChannelRequest, CreateIdentityRequest, CreatedIdentity, Identity, Message,
    PostMessageRequest, ThreadView, UnreadItem, UpdateChannelRequest, UpdateIdentityRequest,
};
use reqwest::Client;
use serde::de::DeserializeOwned;
use std::time::Duration;

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ERROR_BODY_CHARS: usize = 512;

/// Returns a ready-to-paste MCP server JSON snippet for an agent identity.
pub fn mcp_config_snippet(server_url: &str, token: &str) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": {
            "relaypod": {
                "command": "relaypod",
                "args": ["mcp"],
                "env": {
                    "RELAYPOD_URL": server_url,
                    "RELAYPOD_TOKEN": token
                }
            }
        }
    }))
    .unwrap_or_default()
}

/// UI-agnostic async HTTP client for `relaypod-server`.
#[derive(Clone)]
pub struct RelayPodClient {
    base_url: String,
    token: String,
    http: Client,
}

impl RelayPodClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let http = Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            base_url,
            token: token.into(),
            http,
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn ws_url(&self) -> String {
        let ws_base = if let Some(rest) = self.base_url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.base_url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            format!("ws://{}", self.base_url)
        };
        format!("{ws_base}/api/ws")
    }

    fn endpoint_url(&self, segments: &[&str]) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.base_url)
            .with_context(|| format!("invalid RelayPod server URL '{}'", self.base_url))?;
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| anyhow!("invalid RelayPod server URL"))?;
            path.pop_if_empty();
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url)
    }

    async fn parse_json<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        let text = resp.text().await.context("failed to read response body")?;
        if !status.is_success() {
            return Err(format_http_error(status, &text));
        }
        serde_json::from_str(&text).with_context(|| {
            format!(
                "failed to parse JSON response: {}",
                truncate_chars(&text, MAX_ERROR_BODY_CHARS)
            )
        })
    }

    async fn expect_success(resp: reqwest::Response) -> Result<()> {
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format_http_error(status, &text));
        }
        Ok(())
    }

    pub async fn me(&self) -> Result<Identity> {
        let resp = self
            .http
            .get(format!("{}/api/me", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::parse_json(resp).await
    }

    pub async fn list_identities(&self) -> Result<Vec<Identity>> {
        let resp = self
            .http
            .get(format!("{}/api/identities", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::parse_json(resp).await
    }

    pub async fn create_identity(&self, req: &CreateIdentityRequest) -> Result<CreatedIdentity> {
        let resp = self
            .http
            .post(format!("{}/api/identities", self.base_url))
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .await?;
        Self::parse_json(resp).await
    }

    pub async fn update_identity(
        &self,
        identity: &str,
        req: &UpdateIdentityRequest,
    ) -> Result<Identity> {
        let url = self.endpoint_url(&["api", "identities", identity])?;
        let resp = self
            .http
            .patch(url)
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .await?;
        Self::parse_json(resp).await
    }

    pub async fn delete_identity(&self, identity: &str) -> Result<()> {
        let url = self.endpoint_url(&["api", "identities", identity])?;
        self.delete(url).await
    }

    pub async fn list_channels(&self) -> Result<Vec<Channel>> {
        let url = self.endpoint_url(&["api", "channels"])?;
        let resp = self.http.get(url).bearer_auth(&self.token).send().await?;
        Self::parse_json(resp).await
    }

    /// Lists channels including those with read-only history.
    pub async fn list_channels_including_archived(&self) -> Result<Vec<Channel>> {
        let mut url = self.endpoint_url(&["api", "channels"])?;
        url.query_pairs_mut().append_pair("include_archived", "true");
        let resp = self.http.get(url).bearer_auth(&self.token).send().await?;
        Self::parse_json(resp).await
    }

    pub async fn create_channel(&self, req: &CreateChannelRequest) -> Result<Channel> {
        let url = self.endpoint_url(&["api", "channels"])?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .await?;
        Self::parse_json(resp).await
    }

    /// Applies a channel update using its ID or name.
    pub async fn update_channel(
        &self,
        channel: &str,
        req: &UpdateChannelRequest,
    ) -> Result<Channel> {
        let url = self.endpoint_url(&["api", "channels", channel])?;
        let resp = self
            .http
            .patch(url)
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .await?;
        Self::parse_json(resp).await
    }

    /// Permanently deletes a channel using its ID or name.
    pub async fn delete_channel(&self, channel: &str) -> Result<()> {
        let url = self.endpoint_url(&["api", "channels", channel])?;
        self.delete(url).await
    }

    pub async fn list_channel_messages(&self, channel: &str, limit: usize) -> Result<Vec<Message>> {
        let mut url = self.endpoint_url(&["api", "channels", channel, "messages"])?;
        url.query_pairs_mut()
            .append_pair("limit", &limit.to_string());
        let resp = self.http.get(url).bearer_auth(&self.token).send().await?;
        Self::parse_json(resp).await
    }

    pub async fn post_message(
        &self,
        channel: &str,
        body: String,
        parent_id: Option<String>,
    ) -> Result<Message> {
        let req = PostMessageRequest { body, parent_id };
        let url = self.endpoint_url(&["api", "channels", channel, "messages"])?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .json(&req)
            .send()
            .await?;
        Self::parse_json(resp).await
    }

    pub async fn get_thread(&self, message_id: &str) -> Result<ThreadView> {
        let url = self.endpoint_url(&["api", "messages", message_id, "thread"])?;
        let resp = self.http.get(url).bearer_auth(&self.token).send().await?;
        Self::parse_json(resp).await
    }

    pub async fn list_unread(&self) -> Result<Vec<UnreadItem>> {
        let url = self.endpoint_url(&["api", "unread"])?;
        let resp = self.http.get(url).bearer_auth(&self.token).send().await?;
        Self::parse_json(resp).await
    }

    pub async fn ack_unread(&self, unread_id: &str) -> Result<()> {
        let url = self.endpoint_url(&["api", "unread", unread_id, "ack"])?;
        let resp = self.http.post(url).bearer_auth(&self.token).send().await?;
        Self::expect_success(resp).await
    }

    pub async fn delete_message(&self, message_id: &str) -> Result<()> {
        let url = self.endpoint_url(&["api", "messages", message_id])?;
        self.delete(url).await
    }

    async fn delete(&self, url: reqwest::Url) -> Result<()> {
        let resp = self
            .http
            .delete(url)
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::expect_success(resp).await
    }
}

fn format_http_error(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(msg) = val.get("error").and_then(|v| v.as_str()) {
            return anyhow!("{msg}");
        }
    }
    let snippet = truncate_chars(body.trim(), MAX_ERROR_BODY_CHARS);
    if snippet.is_empty() {
        anyhow!("server returned {status}")
    } else {
        anyhow!("server returned {status}: {snippet}")
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_converts_scheme_without_embedding_token() {
        let http = RelayPodClient::new("http://127.0.0.1:8787/", "rp_secret");
        assert_eq!(http.ws_url(), "ws://127.0.0.1:8787/api/ws");

        let https = RelayPodClient::new("https://board.example.com", "rp_secret");
        assert_eq!(https.ws_url(), "wss://board.example.com/api/ws");
    }

    #[test]
    fn endpoint_url_percent_encodes_path_traversal_segments() {
        let client = RelayPodClient::new("http://127.0.0.1:8787", "rp_secret");
        let url = client
            .endpoint_url(&["api", "channels", "../identities?x=1", "messages"])
            .unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:8787/api/channels/..%2Fidentities%3Fx=1/messages"
        );
    }

    #[test]
    fn format_http_error_extracts_json_error_field() {
        let err = format_http_error(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"error":"only admin humans can add human identities"}"#,
        );
        assert_eq!(err.to_string(), "only admin humans can add human identities");

        let plain = format_http_error(reqwest::StatusCode::BAD_GATEWAY, "upstream unavailable");
        assert_eq!(plain.to_string(), "server returned 502 Bad Gateway: upstream unavailable");
    }
}
