//! Rate-limited JSON client shared by the third-party sources.

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use governor::{
    Quota, RateLimiter,
    clock::DefaultClock,
    state::{InMemoryState, NotKeyed},
};
use reqwest::{RequestBuilder, StatusCode};
use serde_json::Value;

use crate::store::Store;

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

#[derive(Clone)]
pub struct SourceClient {
    pub name: &'static str,
    pub http: reqwest::Client,
    limiter: Option<Arc<Limiter>>,
    store: Store,
}

/// Errors the caller may want to distinguish from "not found".
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("{0}: forbidden ({1})")]
    Forbidden(&'static str, String),
    #[error("{0}: {1}")]
    Other(&'static str, String),
}

pub fn build_http(user_agent: &str, timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(10))
        .gzip(true)
        .build()
        .expect("http client")
}

impl SourceClient {
    /// `per_period` requests every `period`, bursting up to `per_period`.
    /// `None` disables limiting.
    pub fn new(name: &'static str, http: reqwest::Client, store: Store, rate: Option<(u32, Duration)>) -> Self {
        let limiter = rate.and_then(|(n, period)| {
            let n = NonZeroU32::new(n)?;
            let quota = Quota::with_period(period / n.get())?.allow_burst(n);
            Some(Arc::new(RateLimiter::direct(quota)))
        });
        Self { name, http, limiter, store }
    }

    pub async fn throttle(&self) {
        if let Some(l) = &self.limiter {
            l.until_ready().await;
        }
    }

    /// Sends a request built by `build` (rebuilt on retry), returning parsed JSON,
    /// `Ok(None)` on 404, and retrying on 429/5xx with backoff.
    pub async fn json<F>(&self, build: F) -> Result<Option<Value>>
    where
        F: Fn(&reqwest::Client) -> RequestBuilder,
    {
        let mut delay = Duration::from_millis(500);
        for attempt in 0..4 {
            self.throttle().await;
            let resp = match build(&self.http).send().await {
                Ok(r) => r,
                Err(e) if attempt < 3 && (e.is_timeout() || e.is_connect()) => {
                    tracing::debug!(source = self.name, error = %e, "retrying");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                    continue;
                }
                Err(e) => return Err(FetchError::Other(self.name, e.to_string()).into()),
            };
            let status = resp.status();
            if status == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                if attempt < 3 {
                    let wait = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(|s| Duration::from_secs(s.min(30)))
                        .unwrap_or(delay);
                    tracing::debug!(source = self.name, %status, ?wait, "backing off");
                    tokio::time::sleep(wait).await;
                    delay *= 2;
                    continue;
                }
                return Err(FetchError::Other(self.name, format!("HTTP {status}")).into());
            }
            if status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED {
                let body = resp.text().await.unwrap_or_default();
                return Err(FetchError::Forbidden(self.name, body.chars().take(200).collect()).into());
            }
            if !status.is_success() {
                return Err(FetchError::Other(self.name, format!("HTTP {status}")).into());
            }
            let v: Value = resp.json().await.map_err(|e| FetchError::Other(self.name, e.to_string()))?;
            return Ok(Some(v));
        }
        Err(anyhow!("{}: retries exhausted", self.name))
    }

    /// Like [`json`](Self::json) but memoized in `source_cache` for `ttl` seconds
    /// (including "not found", stored as JSON null).
    pub async fn cached_json<F>(&self, key: &str, ttl: i64, build: F) -> Result<Option<Value>>
    where
        F: Fn(&reqwest::Client) -> RequestBuilder,
    {
        let key = format!("{}:{}", self.name, key);
        if let Some(body) = self.store.cache_get(&key, ttl).await? {
            let v: Value = serde_json::from_str(&body)?;
            return Ok(if v.is_null() { None } else { Some(v) });
        }
        let v = self.json(build).await?;
        self.store.cache_put(&key, &serde_json::to_string(&v.clone().unwrap_or(Value::Null))?).await?;
        Ok(v)
    }
}
