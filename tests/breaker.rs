//! Circuit breaker of third-party sources (used for MusicBrainz).

use std::time::{Duration, Instant};

use lidarr_meta_proxy::{
    http::{Breaker, FetchError, SourceClient, build_http},
    store::Store,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn client(dir: &tempfile::TempDir) -> SourceClient {
    let store = Store::open(&dir.path().join("meta.db")).await.unwrap();
    SourceClient::new("musicbrainz", build_http("test/0", Duration::from_secs(5)), store, None)
        .with_breaker(Breaker::new(3, Duration::from_secs(60), Duration::from_secs(120)))
        .with_retry_base(Duration::from_millis(1))
}

fn kind(e: &anyhow::Error) -> &'static str {
    match e.downcast_ref::<FetchError>() {
        Some(FetchError::Unavailable(..)) => "unavailable",
        Some(FetchError::Paused(..)) => "paused",
        _ => "other",
    }
}

#[tokio::test]
async fn pauses_after_repeated_connection_failures() {
    let dir = tempfile::tempdir().unwrap();
    let c = client(&dir).await;
    // A port nobody listens on: connection refused, like a blocked IP.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}/ws/2/url");
    assert_eq!(c.status().unwrap(), "unknown");
    for _ in 0..3 {
        let e = c.json(|h| h.get(&url)).await.unwrap_err();
        assert_eq!(kind(&e), "unavailable", "{e:#}");
    }
    assert!(c.status().unwrap().starts_with("paused"), "{:?}", c.status());
    // While paused: immediate error, no network attempt.
    let start = Instant::now();
    let e = c.json(|h| h.get(&url)).await.unwrap_err();
    assert_eq!(kind(&e), "paused");
    assert!(start.elapsed() < Duration::from_millis(50));
}

#[tokio::test]
async fn server_errors_count_and_success_resets() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/down")).respond_with(ResponseTemplate::new(503)).mount(&server).await;
    Mock::given(method("GET")).and(path("/missing")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/up"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let c = client(&dir).await;
    let down = format!("{}/down", server.uri());
    for _ in 0..2 {
        assert_eq!(kind(&c.json(|h| h.get(&down)).await.unwrap_err()), "unavailable");
    }
    assert!(c.status().unwrap().starts_with("failing (2)"));
    // "Not found" is a valid answer, not a failure: it resets the counter.
    assert!(c.json(|h| h.get(format!("{}/missing", server.uri()))).await.unwrap().is_none());
    assert_eq!(c.status().unwrap(), "ok");
    assert!(c.json(|h| h.get(format!("{}/up", server.uri()))).await.unwrap().is_some());
    assert_eq!(c.status().unwrap(), "ok");
}

#[tokio::test]
async fn in_flight_failures_do_not_extend_the_pause() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow-down"))
        .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_millis(100)))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let c = client(&dir).await;
    let url = format!("{}/slow-down", server.uri());
    // Eight concurrent requests all failing: the breaker opens once, with the base pause.
    let calls = (0..8).map(|_| c.json(|h| h.get(&url)));
    futures::future::join_all(calls).await;
    let status = c.status().unwrap();
    assert!(status.starts_with("paused"), "{status}");
    let secs: u64 = status["paused (".len()..].split('s').next().unwrap().parse().unwrap();
    assert!(secs <= 60, "pause escalated by in-flight failures: {status}");
}
