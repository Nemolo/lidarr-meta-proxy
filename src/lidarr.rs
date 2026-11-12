//! Optional self-configuration of Lidarr: sets its hidden `MetadataSource` setting
//! (`PUT /api/v1/config/metadataprovider`) to this proxy once at startup.
//!
//! Enabled by `LIDARR_URL` plus an API key (`LIDARR_API_KEY`, or `LIDARR_CONFIG_XML`
//! pointing at Lidarr's `config.xml`). Runs in background with retries, because
//! Lidarr usually starts after the proxy, and changes the value only if it differs.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::sync::RwLock;

#[derive(Clone, Debug)]
pub enum KeySource {
    Value(String),
    ConfigXml(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub enum SetupState {
    Pending,
    /// Value already correct, nothing changed.
    Unchanged,
    /// `MetadataSource` updated (previous value).
    Updated(String),
    Failed(String),
}

impl SetupState {
    pub fn label(&self) -> String {
        match self {
            SetupState::Pending => "pending".into(),
            SetupState::Unchanged => "configured".into(),
            SetupState::Updated(_) => "configured".into(),
            SetupState::Failed(e) => format!("failed: {e}"),
        }
    }
}

pub struct LidarrSetup {
    url: String,
    key: KeySource,
    target: String,
    http: reqwest::Client,
    pub state: RwLock<SetupState>,
}

/// Extracts `<ApiKey>` from Lidarr's `config.xml`.
pub fn api_key_from_xml(xml: &str) -> Option<String> {
    let start = xml.find("<ApiKey>")? + "<ApiKey>".len();
    let end = start + xml[start..].find("</ApiKey>")?;
    let key = xml[start..end].trim();
    (!key.is_empty()).then(|| key.to_string())
}

fn same_source(a: &str, b: &str) -> bool {
    a.trim().trim_end_matches('/').eq_ignore_ascii_case(b.trim().trim_end_matches('/'))
}

impl LidarrSetup {
    pub fn new(url: String, key: KeySource, target: String) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().expect("http client");
        Self { url: url.trim_end_matches('/').to_string(), key, target, http, state: RwLock::new(SetupState::Pending) }
    }

    async fn api_key(&self) -> Result<String> {
        match &self.key {
            KeySource::Value(k) => Ok(k.clone()),
            // Read on every attempt: on a fresh install the file appears only after Lidarr's first start.
            KeySource::ConfigXml(path) => {
                let xml =
                    tokio::fs::read_to_string(path).await.with_context(|| format!("reading {}", path.display()))?;
                api_key_from_xml(&xml).ok_or_else(|| anyhow!("no <ApiKey> in {}", path.display()))
            }
        }
    }

    /// One attempt: reads the setting and writes it back if different.
    pub async fn apply_once(&self) -> Result<SetupState> {
        let key = self.api_key().await?;
        let endpoint = format!("{}/api/v1/config/metadataprovider", self.url);
        let resp = self.http.get(&endpoint).header("X-Api-Key", &key).send().await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            bail!("Lidarr rejected the API key (401)");
        }
        let mut cfg: Value = resp.error_for_status()?.json().await?;
        let current = cfg.get("metadataSource").and_then(Value::as_str).unwrap_or("").to_string();
        if same_source(&current, &self.target) {
            return Ok(SetupState::Unchanged);
        }
        let obj = cfg.as_object_mut().ok_or_else(|| anyhow!("unexpected metadataprovider response"))?;
        obj.insert("metadataSource".into(), Value::String(self.target.clone()));
        let resp = self.http.put(&endpoint).header("X-Api-Key", &key).json(&cfg).send().await?;
        let saved: Value = resp.error_for_status()?.json().await?;
        let now = saved.get("metadataSource").and_then(Value::as_str).unwrap_or("");
        if !same_source(now, &self.target) {
            bail!("Lidarr did not store the new MetadataSource (got {now:?})");
        }
        Ok(SetupState::Updated(current))
    }

    /// Retries every `interval` up to `attempts` times; records the outcome.
    pub async fn run(self: Arc<Self>, interval: Duration, attempts: u32) {
        for attempt in 1..=attempts {
            match self.apply_once().await {
                Ok(state) => {
                    match &state {
                        SetupState::Updated(prev) => tracing::info!(
                            lidarr = %self.url, previous = %prev, metadata_source = %self.target,
                            "Lidarr MetadataSource set"
                        ),
                        _ => {
                            tracing::info!(lidarr = %self.url, metadata_source = %self.target, "Lidarr MetadataSource already set")
                        }
                    }
                    *self.state.write().await = state;
                    return;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::debug!(attempt, error = %msg, "Lidarr not ready for configuration");
                    if attempt == attempts {
                        tracing::warn!(lidarr = %self.url, error = %msg, "could not configure Lidarr MetadataSource");
                        *self.state.write().await = SetupState::Failed(msg);
                        return;
                    }
                }
            }
            tokio::time::sleep(interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_api_key() {
        let xml = "<Config>\n  <Port>8686</Port>\n  <ApiKey>abc123def</ApiKey>\n  <AuthenticationMethod>Forms</AuthenticationMethod>\n</Config>";
        assert_eq!(api_key_from_xml(xml).as_deref(), Some("abc123def"));
        assert_eq!(api_key_from_xml("<Config><ApiKey></ApiKey></Config>"), None);
        assert_eq!(api_key_from_xml("<Config/>"), None);
    }

    #[test]
    fn compares_sources() {
        assert!(same_source("http://lidarr-meta:5001/", "http://lidarr-meta:5001"));
        assert!(!same_source("", "http://lidarr-meta:5001"));
    }
}
