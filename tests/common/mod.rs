//! Test harness: one wiremock server stands in for every remote service
//! (`/upstream`, `/deezer`, `/mb`, `/itunes`, `/cdn` for images).

#![allow(dead_code)]

use std::sync::Arc;

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use lidarr_meta_proxy::{app::App, config::Config, routes::router};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

pub const MB_ARTIST: &str = "11111111-1111-4111-8111-111111111111";
pub const MB_RG_FIRST: &str = "22222222-2222-4222-8222-222222222222";
pub const MB_RELEASE_FIRST: &str = "33333333-3333-4333-8333-333333333333";

pub struct Harness {
    pub server: MockServer,
    pub app: Arc<App>,
    pub router: Router,
    _dir: tempfile::TempDir,
}

/// Minimal PNG header (imagesize only needs the IHDR chunk).
pub fn png(w: u32, h: u32) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
    v.extend(w.to_be_bytes());
    v.extend(h.to_be_bytes());
    v.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    v.extend(b"rest-of-image-data");
    v
}

impl Harness {
    pub async fn new() -> Self {
        Self::with(|_| {}).await
    }

    pub async fn with(tweak: impl FnOnce(&mut Config)) -> Self {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_tests(&server.uri(), dir.path());
        tweak(&mut cfg);
        let app = Arc::new(App::new(cfg).await.unwrap());
        let router = router(app.clone());
        Self { server, app, router, _dir: dir }
    }

    pub fn uri(&self) -> String {
        self.server.uri()
    }

    pub async fn get(&self, uri: &str) -> (u16, Value) {
        let resp = self.router.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    pub async fn get_raw(&self, uri: &str) -> (u16, Vec<u8>, Option<String>) {
        let resp = self.router.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status().as_u16();
        let ct = resp.headers().get("content-type").map(|v| v.to_str().unwrap().to_string());
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec(), ct)
    }

    pub async fn mock_json(&self, p: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    pub async fn mock_png(&self, p: &str, w: u32, h: u32) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(png(w, h)).insert_header("content-type", "image/png"),
            )
            .mount(&self.server)
            .await;
    }

    pub async fn mock_mb_url(&self, resource: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path("/mb/url"))
            .and(query_param("resource", resource))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    /// Upstream `search` returning `[]` for every query.
    pub async fn mock_empty_upstream_search(&self) {
        self.mock_json("/upstream/search", json!([])).await;
    }

    // ---------------------------------------------------------------- deezer

    /// Deezer artist 27 ("Indie Person", absent from MusicBrainz) with two albums,
    /// one of which is duplicated as a regional variant.
    pub async fn mock_deezer_indie(&self) {
        let u = self.uri();
        self.mock_json(
            "/deezer/artist/27",
            json!({"id": 27, "name": "Indie Person", "link": "https://www.deezer.com/artist/27",
                   "picture_xl": format!("{u}/cdn/artist27.png"), "nb_album": 2, "type": "artist"}),
        )
        .await;
        self.mock_json(
            "/deezer/artist/27/albums",
            json!({"data": [
                {"id": 1001, "title": "Debut", "record_type": "album", "release_date": "2021-05-01",
                 "cover_xl": format!("{u}/cdn/c1001.png"), "link": "https://www.deezer.com/album/1001",
                 "artist": {"id": 27, "name": "Indie Person"}},
                {"id": 1009, "title": "Debut", "record_type": "album", "release_date": "2021-05-01",
                 "cover_xl": format!("{u}/cdn/c1001.png"), "artist": {"id": 27, "name": "Indie Person"}},
                {"id": 1003, "title": "Small Things", "record_type": "ep", "release_date": "2022-01-10",
                 "cover_xl": format!("{u}/cdn/c1003.png"), "artist": {"id": 27, "name": "Indie Person"}},
                {"id": 7777, "title": "Someone Else's Compilation", "record_type": "compile",
                 "artist": {"id": 999, "name": "Various Artists"}}
            ], "total": 4}),
        )
        .await;
        self.mock_json(
            "/deezer/album/1001",
            json!({"id": 1001, "title": "Debut", "upc": "0123456789012", "label": "Tiny Label",
                   "record_type": "album", "release_date": "2021-05-01", "link": "https://www.deezer.com/album/1001",
                   "cover_xl": format!("{u}/cdn/c1001.png"), "genres": {"data": [{"id": 1, "name": "Pop"}]},
                   "artist": {"id": 27, "name": "Indie Person"}, "explicit_lyrics": false,
                   "tracks": {"data": []}}),
        )
        .await;
        self.mock_json(
            "/deezer/album/1001/tracks",
            json!({"data": [
                {"id": 50001, "title": "Opening", "duration": 201, "track_position": 1, "disk_number": 1,
                 "isrc": "QZAAA2100001", "explicit_lyrics": false, "artist": {"id": 27}},
                {"id": 50002, "title": "Closing", "duration": 305, "track_position": 2, "disk_number": 1,
                 "isrc": "QZAAA2100002", "explicit_lyrics": true, "artist": {"id": 27}},
                {"id": 50003, "title": "Bonus", "duration": 99, "track_position": 1, "disk_number": 2,
                 "isrc": "QZAAA2100003", "explicit_lyrics": false, "artist": {"id": 27}}
            ], "total": 3}),
        )
        .await;
        self.mock_png("/cdn/artist27.png", 1000, 1000).await;
        self.mock_png("/cdn/c1001.png", 1000, 1000).await;
        self.mock_png("/cdn/c1003.png", 1000, 1000).await;
        // iTunes finds the album by UPC and serves a larger cover.
        Mock::given(method("GET"))
            .and(path("/itunes/lookup"))
            .and(query_param("upc", "0123456789012"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"resultCount": 1, "results": [
                {"wrapperType": "collection", "collectionName": "Debut", "artistName": "Indie Person",
                 "artworkUrl100": format!("{u}/cdn/itunes/100x100bb.jpg")}]})))
            .mount(&self.server)
            .await;
        self.mock_png("/cdn/itunes/3000x3000bb.jpg", 3000, 3000).await;
    }

    // ------------------------------------------------------------ upstream MB

    /// MB artist with one release group; links to Deezer artist 500 which has an extra album.
    pub async fn mock_mb_artist_with_deezer(&self) {
        let u = self.uri();
        // Upstream answers with the inconsistent casing of the real metadata server.
        self.mock_json(
            &format!("/upstream/artist/{MB_ARTIST}"),
            json!({
                "id": MB_ARTIST, "artistname": "Known Band", "sortname": "Known Band", "artistaliases": [],
                "disambiguation": "", "overview": "A band.", "type": "Group", "status": "active", "genres": ["rock"],
                "images": [], "links": [{"target": "https://www.deezer.com/artist/500", "type": "deezer"}],
                "oldids": [], "rating": {"Count": 3, "Value": 8.2},
                "Albums": [{"Id": MB_RG_FIRST, "OldIds": [], "Title": "First Album", "Type": "Album",
                            "SecondaryTypes": [], "ReleaseStatuses": ["Official"], "ReleaseDate": "2010-03-01"}]
            }),
        )
        .await;
        self.mock_json(
            "/deezer/artist/500",
            json!({"id": 500, "name": "Known Band", "link": "https://www.deezer.com/artist/500",
                   "picture_xl": format!("{u}/cdn/artist500.png")}),
        )
        .await;
        self.mock_png("/cdn/artist500.png", 1000, 1000).await;
        self.mock_json(
            "/deezer/artist/500/albums",
            json!({"data": [
                {"id": 2001, "title": "First Album (Remastered 2020)", "record_type": "album", "release_date": "2020-01-01",
                 "cover_xl": format!("{u}/cdn/c2001.png"), "artist": {"id": 500, "name": "Known Band"}},
                {"id": 2002, "title": "Missing Album", "record_type": "album", "release_date": "2015-06-01",
                 "cover_xl": format!("{u}/cdn/c2002.png"), "artist": {"id": 500, "name": "Known Band"}},
                {"id": 2003, "title": "Barcode Match", "record_type": "single", "release_date": "2012-01-01",
                 "artist": {"id": 500, "name": "Known Band"}}
            ]}),
        )
        .await;
        self.mock_png("/cdn/c2001.png", 1000, 1000).await;
        self.mock_png("/cdn/c2002.png", 1000, 1000).await;
        self.mock_json(
            "/deezer/album/2002",
            json!({"id": 2002, "title": "Missing Album", "upc": "5000000000002", "record_type": "album",
                   "release_date": "2015-06-01", "cover_xl": format!("{u}/cdn/c2002.png"),
                   "artist": {"id": 500, "name": "Known Band"}}),
        )
        .await;
        self.mock_json(
            "/deezer/album/2002/tracks",
            json!({"data": [{"id": 60001, "title": "Only Song", "duration": 180, "track_position": 1, "disk_number": 1,
                              "isrc": "QZBBB1500001"}]}),
        )
        .await;
        self.mock_json(
            "/deezer/album/2003",
            json!({"id": 2003, "title": "Barcode Match", "upc": "5000000000003", "record_type": "single",
                   "artist": {"id": 500, "name": "Known Band"}}),
        )
        .await;
        self.mock_json("/deezer/album/2003/tracks", json!({"data": []})).await;
        // MB browse: the single exists on MB under another release group (matched by barcode).
        Mock::given(method("GET"))
            .and(path("/mb/release"))
            .and(query_param("artist", MB_ARTIST))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"release-count": 2, "releases": [
                {"id": MB_RELEASE_FIRST, "title": "First Album", "barcode": "111", "release-group": {"id": MB_RG_FIRST}},
                {"id": "44444444-4444-4444-8444-444444444444", "title": "Barcode Match (single)", "barcode": "5000000000003",
                 "release-group": {"id": "55555555-5555-4555-8555-555555555555"}}
            ]})))
            .mount(&self.server)
            .await;
    }
}
