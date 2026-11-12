//! Client for the official metadata server (api.lidarr.audio), cached in SQLite.

use std::time::Duration;

use bytes::Bytes;
use reqwest::{StatusCode, header};
use serde_json::Value;

use crate::{ids::now, store::Store};

#[derive(Clone)]
pub struct Upstream {
    http: reqwest::Client,
    base: String,
    ttl: i64,
    store: Store,
    /// Last reachability probe (time, result), reused for 5 minutes.
    probe: std::sync::Arc<tokio::sync::Mutex<Option<(std::time::Instant, bool)>>>,
    /// Rate limit for requests that actually reach the upstream server (cache hits are free).
    limiter: Option<std::sync::Arc<governor::DefaultDirectRateLimiter>>,
}

#[derive(Debug, Clone)]
pub struct UpstreamResponse {
    pub status: u16,
    pub body: Bytes,
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("upstream unavailable: {0}")]
    Unavailable(String),
}

impl UpstreamResponse {
    pub fn json(&self) -> Option<Value> {
        if self.status == 200 { serde_json::from_slice(&self.body).ok() } else { None }
    }
}

/// Cache key: path plus query parameters sorted (Lidarr already lowercases values).
pub fn route_key(path: &str, query: &[(String, String)]) -> String {
    let mut q: Vec<_> = query.to_vec();
    q.sort();
    if q.is_empty() {
        path.trim_start_matches('/').to_string()
    } else {
        let qs = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
        format!("{}?{qs}", path.trim_start_matches('/'))
    }
}

impl Upstream {
    /// `rate`: max requests per second to the upstream server (0 = unlimited).
    pub fn new(base: String, ttl: Duration, store: Store, user_agent: &str, rate: u32) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .gzip(true)
            .build()
            .expect("http client");
        let limiter = std::num::NonZeroU32::new(rate)
            .map(|r| std::sync::Arc::new(governor::RateLimiter::direct(governor::Quota::per_second(r))));
        Self { http, base, ttl: ttl.as_secs() as i64, store, probe: Default::default(), limiter }
    }

    async fn throttle(&self) {
        if let Some(l) = &self.limiter {
            l.until_ready().await;
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    /// GET with cache. Fresh cache wins; otherwise the upstream is asked (with
    /// `If-None-Match` when an ETag is known). On upstream failure a stale cached
    /// answer is served; with nothing cached the error is returned (→ 503).
    pub async fn get(&self, path: &str, query: &[(String, String)]) -> Result<UpstreamResponse, UpstreamError> {
        let key = route_key(path, query);
        // `/recent/*` is keyed by a moving timestamp: caching it would only grow the table.
        let cacheable = !path.trim_start_matches('/').starts_with("recent/");
        let cached = if cacheable { self.store.upstream_get(&key).await.ok().flatten() } else { None };
        if let Some(c) = &cached
            && now() - c.fetched_at < self.ttl
        {
            return Ok(UpstreamResponse { status: c.status, body: Bytes::from(c.body.clone()) });
        }

        let stale = |reason: String| match &cached {
            Some(c) => {
                tracing::warn!(route = %key, %reason, "upstream failed, serving stale cache");
                Ok(UpstreamResponse { status: c.status, body: Bytes::from(c.body.clone()) })
            }
            None => {
                tracing::warn!(route = %key, %reason, "upstream failed, nothing cached");
                Err(UpstreamError::Unavailable(reason))
            }
        };
        // One retry on 429, honouring Retry-After (capped).
        let mut attempt = 0;
        let resp = loop {
            let mut req = self.http.get(self.url(path)).query(query);
            if let Some(etag) = cached.as_ref().and_then(|c| c.etag.as_deref()) {
                req = req.header(header::IF_NONE_MATCH, etag);
            }
            self.throttle().await;
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => return stale(e.to_string()),
            };
            if resp.status() == StatusCode::TOO_MANY_REQUESTS && attempt == 0 {
                attempt += 1;
                let wait = resp
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(2)
                    .clamp(1, 10);
                tracing::info!(route = %key, wait_secs = wait, "upstream rate limited (429), retrying");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            break resp;
        };
        let status = resp.status();
        if status == StatusCode::NOT_MODIFIED
            && let Some(c) = &cached
        {
            let _ = self.store.upstream_touch(&key).await;
            return Ok(UpstreamResponse { status: c.status, body: Bytes::from(c.body.clone()) });
        }
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            return stale(format!("HTTP {status}"));
        }
        let etag = resp.headers().get(header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return stale(e.to_string()),
        };
        // Only definitive answers are cached.
        if status == StatusCode::OK && serde_json::from_slice::<Value>(&body).is_err() {
            return stale("invalid JSON from upstream".into());
        }
        if cacheable
            && (status == StatusCode::OK || status == StatusCode::NOT_FOUND)
            && let Err(e) = self.store.upstream_put(&key, &body, status.as_u16(), etag.as_deref()).await
        {
            tracing::warn!(error = %e, "failed to cache upstream response");
        }
        Ok(UpstreamResponse { status: status.as_u16(), body })
    }

    /// Uncached passthrough of an arbitrary request (e.g. `POST /search/fingerprint`).
    pub async fn forward(
        &self,
        method: reqwest::Method,
        path: &str,
        raw_query: Option<&str>,
        content_type: Option<&str>,
        body: Bytes,
    ) -> Result<(u16, Option<String>, Bytes), UpstreamError> {
        let mut url = self.url(path);
        if let Some(q) = raw_query.filter(|q| !q.is_empty()) {
            url = format!("{url}?{q}");
        }
        let mut req = self.http.request(method, url);
        if let Some(ct) = content_type {
            req = req.header(header::CONTENT_TYPE, ct);
        }
        if !body.is_empty() {
            req = req.body(body);
        }
        self.throttle().await;
        let resp = req.send().await.map_err(|e| UpstreamError::Unavailable(e.to_string()))?;
        let status = resp.status().as_u16();
        if resp.status().is_server_error() {
            return Err(UpstreamError::Unavailable(format!("HTTP {status}")));
        }
        let ct = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = resp.bytes().await.map_err(|e| UpstreamError::Unavailable(e.to_string()))?;
        Ok((status, ct, body))
    }

    /// Lightweight reachability probe for `/health`, cached for 5 minutes so the
    /// Docker healthcheck does not hit the public server every 30 seconds.
    pub async fn reachable(&self) -> bool {
        let mut last = self.probe.lock().await;
        if let Some((at, ok)) = *last
            && at.elapsed() < Duration::from_secs(300)
        {
            return ok;
        }
        let ok = match self
            .http
            .get(self.url("recent/artist"))
            .query(&[("since", now().to_string())])
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(r) => !r.status().is_server_error(),
            Err(_) => false,
        };
        *last = Some((std::time::Instant::now(), ok));
        ok
    }
}
