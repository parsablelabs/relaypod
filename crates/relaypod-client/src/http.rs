use anyhow::{anyhow, Context, Result};
use relaypod_core::{
    Channel, CreateChannelRequest, CreateIdentityRequest, CreatedIdentity, Identity, Message,
    PostMessageRequest, ThreadView, UnreadItem, UpdateChannelRequest,
};
use reqwest::Client;
use serde::de::DeserializeOwned;

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
        Self {
            base_url,
            token: token.into(),
            http: Client::new(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
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
        format!("{ws_base}/api/ws?token={}", self.token)
    }

    async fn parse_json<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        let text = resp.text().await.context("failed to read response body")?;
        if !status.is_success() {
            return Err(anyhow!("server returned {status}: {text}"));
        }
        serde_json::from_str(&text).with_context(|| format!("failed to parse JSON: {text}"))
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

    pub async fn list_channels(&self) -> Result<Vec<Channel>> {
        self.fetch_channels("/api/channels").await
    }

    /// Lists channels including those with read-only history.
    pub async fn list_channels_including_archived(&self) -> Result<Vec<Channel>> {
        self.fetch_channels("/api/channels?include_archived=true")
            .await
    }

    async fn fetch_channels(&self, path: &str) -> Result<Vec<Channel>> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::parse_json(resp).await
    }

    pub async fn create_channel(&self, req: &CreateChannelRequest) -> Result<Channel> {
        let resp = self
            .http
            .post(format!("{}/api/channels", self.base_url))
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
        let url = self.channel_url(channel)?;
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
        self.delete(self.channel_url(channel)?).await
    }

    fn channel_url(&self, channel: &str) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&format!("{}/api/channels", self.base_url))?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("invalid RelayPod server URL"))?
            .push(channel);
        Ok(url)
    }

    pub async fn list_channel_messages(&self, channel: &str, limit: usize) -> Result<Vec<Message>> {
        let resp = self
            .http
            .get(format!(
                "{}/api/channels/{channel}/messages?limit={limit}",
                self.base_url
            ))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::parse_json(resp).await
    }

    pub async fn post_message(
        &self,
        channel: &str,
        body: String,
        parent_id: Option<String>,
    ) -> Result<Message> {
        let req = PostMessageRequest { body, parent_id };
        let resp = self
            .http
            .post(format!("{}/api/channels/{channel}/messages", self.base_url))
            .bearer_auth(&self.token)
            .json(&req)
            .send()
            .await?;
        Self::parse_json(resp).await
    }

    pub async fn get_thread(&self, message_id: &str) -> Result<ThreadView> {
        let resp = self
            .http
            .get(format!("{}/api/messages/{message_id}/thread", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::parse_json(resp).await
    }

    pub async fn list_unread(&self) -> Result<Vec<UnreadItem>> {
        let resp = self
            .http
            .get(format!("{}/api/unread", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::parse_json(resp).await
    }

    pub async fn ack_unread(&self, unread_id: &str) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/api/unread/{unread_id}/ack", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("server returned {status}: {text}"));
        }
        Ok(())
    }

    pub async fn delete_message(&self, message_id: &str) -> Result<()> {
        self.delete(format!("{}/api/messages/{message_id}", self.base_url))
            .await
    }

    async fn delete(&self, url: impl reqwest::IntoUrl) -> Result<()> {
        let resp = self
            .http
            .delete(url)
            .bearer_auth(&self.token)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("server returned {status}: {text}"));
        }
        Ok(())
    }
}
