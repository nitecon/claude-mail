use anyhow::{bail, Context, Result};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    pub size: i64,
    pub checksum: String,
    pub uploaded_at: i64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct UploadResponse {
    pub name: String,
    pub size: i64,
    pub checksum: String,
}

pub struct SkillsClient {
    base_url: String,
    api_key: String,
    client: Client,
}

impl SkillsClient {
    pub fn new(base_url: String, api_key: String, timeout_ms: u64) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .context("build HTTP client")?;
        Ok(Self {
            base_url,
            api_key,
            client,
        })
    }

    pub async fn upload(&self, name: &str, zip: Vec<u8>) -> Result<UploadResponse> {
        let url = format!("{}/v1/skills/{}", self.base_url, name);
        let resp = self
            .client
            .put(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/zip")
            .body(zip)
            .send()
            .await
            .context("upload skill")?;
        if !resp.status().is_success() {
            bail!("upload failed: HTTP {}", resp.status());
        }
        resp.json::<UploadResponse>()
            .await
            .context("parse upload response")
    }

    pub async fn download(&self, name: &str) -> Result<Vec<u8>> {
        let url = format!("{}/v1/skills/{}", self.base_url, name);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .context("download skill")?;
        if resp.status() == StatusCode::NOT_FOUND {
            bail!("skill '{}' not found on gateway", name);
        }
        if !resp.status().is_success() {
            bail!("download failed: HTTP {}", resp.status());
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .context("read response bytes")
    }

    pub async fn list(&self) -> Result<Vec<SkillMeta>> {
        let url = format!("{}/v1/skills", self.base_url);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .context("list skills")?;
        if !resp.status().is_success() {
            bail!("list failed: HTTP {}", resp.status());
        }
        resp.json::<Vec<SkillMeta>>()
            .await
            .context("parse list response")
    }

    pub async fn delete(&self, name: &str) -> Result<()> {
        let url = format!("{}/v1/skills/{}", self.base_url, name);
        let resp = self
            .client
            .delete(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .context("delete skill")?;
        if resp.status() == StatusCode::NOT_FOUND {
            bail!("skill '{}' not found on gateway", name);
        }
        if !resp.status().is_success() {
            bail!("delete failed: HTTP {}", resp.status());
        }
        Ok(())
    }
}
