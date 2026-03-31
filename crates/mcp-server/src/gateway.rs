// Response structs mirror the full server schema; not every field is consumed by
// the current tool set, but they should be kept for completeness / future use.
#![allow(dead_code)]

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone)]
pub struct GatewayClient {
    client: Client,
    base_url: String,
    api_key: String,
}

// ── Request / response types ──────────────────────────────────────────────────

#[derive(Serialize)]
struct RegisterProjectRequest<'a> {
    ident: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<&'a str>,
}

#[derive(Deserialize)]
pub struct RegisterProjectResponse {
    pub ident: String,
    pub channel_name: String,
    pub room_id: String,
}

#[derive(Serialize)]
struct SendMessageRequest<'a> {
    content: &'a str,
}

#[derive(Deserialize)]
pub struct SendMessageResponse {
    pub message_id: i64,
    pub external_message_id: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct GatewayMessage {
    pub id: i64,
    pub project_ident: String,
    pub source: String,
    pub content: String,
    pub sent_at: i64,
}

#[derive(Deserialize)]
pub struct GetUnreadResponse {
    pub messages: Vec<GatewayMessage>,
    pub status: String,
}

#[derive(Deserialize)]
pub struct ConfirmResponse {
    pub confirmed: bool,
}

// ── Client implementation ─────────────────────────────────────────────────────

impl GatewayClient {
    pub fn new(base_url: String, api_key: String, timeout_ms: u64) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .context("build reqwest client")?;
        Ok(Self {
            client,
            base_url,
            api_key,
        })
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    /// Register (or re-register) a project with the gateway.
    /// `channel` selects the plugin; pass `None` to use the gateway's default.
    pub async fn register_project(
        &self,
        ident: &str,
        channel: Option<&str>,
    ) -> Result<RegisterProjectResponse> {
        let url = format!("{}/v1/projects", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth())
            .json(&RegisterProjectRequest { ident, channel })
            .send()
            .await
            .context("POST /v1/projects")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("gateway error {status}: {body}");
        }

        resp.json::<RegisterProjectResponse>()
            .await
            .context("decode register response")
    }

    /// Post an agent message to the project's channel.
    pub async fn send_message(&self, ident: &str, content: &str) -> Result<SendMessageResponse> {
        let url = format!("{}/v1/projects/{}/messages", self.base_url, ident);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth())
            .json(&SendMessageRequest { content })
            .send()
            .await
            .context("POST /v1/projects/:ident/messages")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("gateway error {status}: {body}");
        }

        resp.json::<SendMessageResponse>()
            .await
            .context("decode send message response")
    }

    /// Fetch unconfirmed messages for a project (peek — no side effects).
    pub async fn get_unread(&self, ident: &str) -> Result<GetUnreadResponse> {
        let url = format!("{}/v1/projects/{}/messages/unread", self.base_url, ident);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("GET /v1/projects/:ident/messages/unread")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("gateway error {status}: {body}");
        }

        resp.json::<GetUnreadResponse>()
            .await
            .context("decode unread response")
    }

    /// Confirm a single message as read and acted upon.
    pub async fn confirm_read(&self, ident: &str, msg_id: i64) -> Result<ConfirmResponse> {
        let url = format!(
            "{}/v1/projects/{}/messages/{}/confirm",
            self.base_url, ident, msg_id
        );
        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("POST /v1/projects/:ident/messages/:id/confirm")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("gateway error {status}: {body}");
        }

        resp.json::<ConfirmResponse>()
            .await
            .context("decode confirm response")
    }
}
