//! Rate-limited JSON client shared by the third-party sources.

use std::{
    num::NonZeroU32,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Result;
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
    breaker: Option<Arc<Breaker>>,
    /// First retry delay (doubles on each retry).
    retry_base: Duration,
}

/// Circuit breaker: after `threshold` consecutive failures (network errors,
/// 5xx/429 after retries) the source is not contacted for a pause that doubles
/// up to `max`. Keeps a blocked or unreachable service from being hammered,
/// which would only prolong an IP block, and keeps requests fast meanwhile.
pub struct Breaker {
    threshold: u32,
    base: Duration,
    max: Duration,
    state: Mutex<BreakerState>,
}

#[derive(Default)]
struct BreakerState {
    failures: u32,
    open_until: Option<Instant>,
    pause: Duration,
    ever_ok: bool,
    last_error: Option<String>,
}

impl Breaker {
    pub fn new(threshold: u32, base: Duration, max: Duration) -> Self {
        Self { threshold, base, max, state: Mutex::new(BreakerState { pause: base, ..Default::default() }) }
    }

    /// Remaining pause, if the breaker is open.
    pub fn paused_for(&self) -> Option<Duration> {
        let s = self.state.lock().unwrap();
        s.open_until.and_then(|t| t.checked_duration_since(Instant::now())).filter(|d| !d.is_zero())
    }

    fn success(&self) {
        let mut s = self.state.lock().unwrap();
        s.failures = 0;
        s.open_until = None;
        s.pause = self.base;
        s.ever_ok = true;
        s.last_error = None;
    }

    fn failure(&self, name: &str, error: &str) {
        let mut s = self.state.lock().unwrap();
        s.failures += 1;
        s.last_error = Some(error.chars().take(200).collect());
        if s.failures >= self.threshold {
            let pause = s.pause;
            s.open_until = Some(Instant::now() + pause);
            s.pause = (pause * 2).min(self.max);
            // After the pause a single failed probe re-opens it.
            s.failures = self.threshold.saturating_sub(1);
            tracing::warn!(source = name, pause_secs = pause.as_secs(), error, "source unreachable, pausing requests");
        }
    }

    /// `ok`, `unknown` (no request yet), `failing (n)`, `paused (Ns): error`.
    pub fn status(&self) -> String {
        let paused = self.paused_for();
        let s = self.state.lock().unwrap();
        let err = s.last_error.clone().unwrap_or_default();
        match paused {
            Some(d) => format!("paused ({}s): {err}", d.as_secs()),
            None if s.failures > 0 => format!("failing ({}): {err}", s.failures),
            None if s.ever_ok => "ok".into(),
            None => "unknown".into(),
        }
    }
}

/// Errors the caller may want to distinguish from "not found".
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("{0}: forbidden ({1})")]
    Forbidden(&'static str, String),
    /// Network error or 5xx/429 after retries (counts for the circuit breaker).
    #[error("{0}: unavailable ({1})")]
    Unavailable(&'static str, String),
    #[error("{0}: paused for {1}s after repeated failures")]
    Paused(&'static str, u64),
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
        Self { name, http, limiter, store, breaker: None, retry_base: Duration::from_millis(500) }
    }

    pub fn with_breaker(mut self, breaker: Breaker) -> Self {
        self.breaker = Some(Arc::new(breaker));
        self
    }

    pub fn with_retry_base(mut self, base: Duration) -> Self {
        self.retry_base = base;
        self
    }

    /// Circuit breaker status (`None` if the source has none).
    pub fn status(&self) -> Option<String> {
        self.breaker.as_ref().map(|b| b.status())
    }

    pub async fn throttle(&self) {
        if let Some(l) = &self.limiter {
            l.until_ready().await;
        }
    }

    /// Sends a request built by `build` (rebuilt on retry), returning parsed JSON,
    /// `Ok(None)` on 404, and retrying on 429/5xx with backoff. Fails fast while
    /// the circuit breaker is open.
    pub async fn json<F>(&self, build: F) -> Result<Option<Value>>
    where
        F: Fn(&reqwest::Client) -> RequestBuilder,
    {
        if let Some(b) = &self.breaker
            && let Some(d) = b.paused_for()
        {
            return Err(FetchError::Paused(self.name, d.as_secs()).into());
        }
        let result = self.json_once(build).await;
        if let Some(b) = &self.breaker {
            match &result {
                Ok(_) => b.success(),
                Err(e) => {
                    if let Some(FetchError::Unavailable(_, msg)) = e.downcast_ref::<FetchError>() {
                        b.failure(self.name, msg);
                    }
                }
            }
        }
        result
    }

    async fn json_once<F>(&self, build: F) -> Result<Option<Value>>
    where
        F: Fn(&reqwest::Client) -> RequestBuilder,
    {
        let mut delay = self.retry_base;
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
                Err(e) => {
                    let unavailable = e.is_timeout() || e.is_connect() || e.is_request();
                    let msg = format!("{e:#}");
                    return Err(if unavailable {
                        FetchError::Unavailable(self.name, msg)
                    } else {
                        FetchError::Other(self.name, msg)
                    }
                    .into());
                }
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
                return Err(FetchError::Unavailable(self.name, format!("HTTP {status}")).into());
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
        Err(FetchError::Unavailable(self.name, "retries exhausted".into()).into())
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
