//! Self-configuration of Lidarr's MetadataSource against a mock Lidarr API.

use std::{sync::Arc, time::Duration};

use lidarr_meta_proxy::lidarr::{KeySource, LidarrSetup, SetupState};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, header, method, path},
};

const EP: &str = "/api/v1/config/metadataprovider";
const TARGET: &str = "http://lidarr-meta:5001";

async fn mock_get(server: &MockServer, current: &str) {
    Mock::given(method("GET"))
        .and(path(EP))
        .and(header("X-Api-Key", "k3y"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 1, "metadataSource": current, "writeAudioTags": "no", "scrubAudioTags": false
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn sets_metadata_source_keeping_other_fields() {
    let server = MockServer::start().await;
    mock_get(&server, "").await;
    Mock::given(method("PUT"))
        .and(path(EP))
        .and(header("X-Api-Key", "k3y"))
        // The whole resource is sent back, with only metadataSource changed.
        .and(body_partial_json(
            json!({"id": 1, "metadataSource": TARGET, "writeAudioTags": "no", "scrubAudioTags": false}),
        ))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({"id": 1, "metadataSource": TARGET})))
        .expect(1)
        .mount(&server)
        .await;
    let setup = LidarrSetup::new(server.uri(), KeySource::Value("k3y".into()), TARGET.into());
    assert_eq!(setup.apply_once().await.unwrap(), SetupState::Updated(String::new()));
}

#[tokio::test]
async fn leaves_correct_value_untouched() {
    let server = MockServer::start().await;
    mock_get(&server, "http://lidarr-meta:5001/").await;
    Mock::given(method("PUT")).respond_with(ResponseTemplate::new(500)).expect(0).mount(&server).await;
    let setup = LidarrSetup::new(server.uri(), KeySource::Value("k3y".into()), TARGET.into());
    assert_eq!(setup.apply_once().await.unwrap(), SetupState::Unchanged);
}

#[tokio::test]
async fn reads_key_from_config_xml_and_retries_until_lidarr_is_up() {
    let server = MockServer::start().await;
    // Lidarr still starting: first answers 503.
    Mock::given(method("GET"))
        .and(path(EP))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    mock_get(&server, "").await;
    Mock::given(method("PUT"))
        .and(path(EP))
        .and(header("X-Api-Key", "k3y"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({"metadataSource": TARGET})))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let xml = dir.path().join("config.xml");
    std::fs::write(&xml, "<Config>\n  <Port>8686</Port>\n  <ApiKey>k3y</ApiKey>\n</Config>\n").unwrap();

    let setup = Arc::new(LidarrSetup::new(server.uri(), KeySource::ConfigXml(xml), TARGET.into()));
    setup.clone().run(Duration::from_millis(10), 10).await;
    assert!(matches!(*setup.state.read().await, SetupState::Updated(_)));
}

#[tokio::test]
async fn reports_failure_after_retries() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path(EP)).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    let setup = Arc::new(LidarrSetup::new(server.uri(), KeySource::Value("wrong".into()), TARGET.into()));
    setup.clone().run(Duration::from_millis(1), 3).await;
    let state = setup.state.read().await.clone();
    assert!(state.label().contains("401"), "{state:?}");
}

#[test]
fn config_requires_complete_settings() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = lidarr_meta_proxy::config::Config::for_tests("http://x", dir.path());
    assert!(cfg.lidarr_setup().unwrap().is_none());
    cfg.lidarr_url = Some("http://lidarr:8686".into());
    assert!(cfg.lidarr_setup().is_err(), "URL without key");
    cfg.lidarr_config_xml = Some("/lidarr/config.xml".into());
    let (_, _, target) = cfg.lidarr_setup().unwrap().unwrap();
    assert_eq!(target, "http://proxy.test", "defaults to PUBLIC_URL");
    cfg.lidarr_url = None;
    assert!(cfg.lidarr_setup().is_err(), "key without URL");
}
