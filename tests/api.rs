mod common;

use common::*;
use lidarr_meta_proxy::ids::{Kind, Source, synthetic_id};
use serde_json::{Value, json};
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path},
};

// ------------------------------------------------------------------ contract

/// Fields of the C# classes in `NzbDrone.Core/MetadataSource/SkyHook/Resource`,
/// with the JSON type Lidarr expects. Lists must never be null.
fn check_fields(v: &Value, fields: &[(&str, &str)], what: &str) {
    let obj = v.as_object().unwrap_or_else(|| panic!("{what}: not an object: {v}"));
    for (name, ty) in fields {
        let f = obj.get(*name).unwrap_or_else(|| panic!("{what}: missing {name} in {v}"));
        let ok = match *ty {
            "string" => f.is_string(),
            "string?" => f.is_string() || f.is_null(),
            "int" => f.is_i64() || f.is_u64(),
            "number" => f.is_number(),
            "bool" => f.is_boolean(),
            "list" => f.is_array(),
            "object" => f.is_object(),
            _ => unreachable!(),
        };
        assert!(ok, "{what}.{name}: expected {ty}, got {f}");
    }
}

fn check_image(v: &Value) {
    check_fields(v, &[("coverType", "string"), ("url", "string"), ("width", "int"), ("height", "int")], "Image");
    assert!(v["width"].as_i64().unwrap() > 0 && v["height"].as_i64().unwrap() > 0, "image without size: {v}");
}

fn check_artist(v: &Value) {
    check_fields(
        v,
        &[
            ("id", "string"),
            ("oldIds", "list"),
            ("artistName", "string"),
            ("artistAliases", "list"),
            ("disambiguation", "string"),
            ("overview", "string"),
            ("type", "string"),
            ("status", "string"),
            ("genres", "list"),
            ("images", "list"),
            ("links", "list"),
            ("rating", "object"),
            ("albums", "list"),
        ],
        "Artist",
    );
    uuid::Uuid::parse_str(v["id"].as_str().unwrap()).expect("artist id must be a UUID");
    v["images"].as_array().unwrap().iter().for_each(check_image);
    for a in v["albums"].as_array().unwrap() {
        check_album_summary(a);
    }
}

fn check_album_summary(v: &Value) {
    check_fields(
        v,
        &[
            ("id", "string"),
            ("oldIds", "list"),
            ("artistId", "string"),
            ("title", "string"),
            ("type", "string"),
            ("secondaryTypes", "list"),
            ("releaseStatuses", "list"),
            ("releaseDate", "string?"),
            ("genres", "list"),
            ("images", "list"),
            ("links", "list"),
            ("rating", "object"),
        ],
        "Album",
    );
    uuid::Uuid::parse_str(v["id"].as_str().unwrap()).expect("album id must be a UUID");
    let t = v["type"].as_str().unwrap();
    assert!(lidarr_meta_proxy::model::PRIMARY_TYPES.contains(&t), "invalid album type {t}");
    v["images"].as_array().unwrap().iter().for_each(check_image);
}

fn check_album(v: &Value) {
    check_album_summary(v);
    check_fields(v, &[("artists", "list"), ("releases", "list")], "Album");
    let artist_id = v["artistId"].as_str().unwrap();
    let artist_ids: Vec<&str> = v["artists"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert!(artist_ids.contains(&artist_id), "artistId must be in artists (Lidarr indexes artistDict by it)");
    for r in v["releases"].as_array().unwrap() {
        check_fields(
            r,
            &[
                ("id", "string"),
                ("oldIds", "list"),
                ("title", "string"),
                ("status", "string"),
                ("country", "list"),
                ("label", "list"),
                ("media", "list"),
                ("trackCount", "int"),
                ("tracks", "list"),
                ("disambiguation", "string"),
            ],
            "Release",
        );
        let tracks = r["tracks"].as_array().unwrap();
        assert_eq!(r["trackCount"].as_i64().unwrap() as usize, tracks.len());
        assert!(!tracks.is_empty(), "releases with trackCount 0 are dropped by Lidarr");
        let media: Vec<i64> = r["media"].as_array().unwrap().iter().map(|m| m["position"].as_i64().unwrap()).collect();
        for m in r["media"].as_array().unwrap() {
            check_fields(m, &[("name", "string"), ("format", "string"), ("position", "int")], "Medium");
        }
        for (n, t) in tracks.iter().enumerate() {
            check_fields(
                t,
                &[
                    ("id", "string"),
                    ("oldIds", "list"),
                    ("recordingId", "string"),
                    ("oldRecordingIds", "list"),
                    ("artistId", "string"),
                    ("trackName", "string"),
                    ("trackNumber", "string"),
                    ("trackPosition", "int"),
                    ("mediumNumber", "int"),
                    ("durationMs", "int"),
                    ("explicit", "bool"),
                ],
                "Track",
            );
            assert!(artist_ids.contains(&t["artistId"].as_str().unwrap()));
            assert_eq!(t["trackPosition"].as_i64().unwrap(), n as i64 + 1);
            assert!(media.contains(&t["mediumNumber"].as_i64().unwrap()), "track on a medium not listed");
        }
    }
}

// --------------------------------------------------------------------- tests

#[tokio::test]
async fn passthrough_is_cached_and_normalized() {
    let h = Harness::new().await;
    Mock::given(method("GET"))
        .and(path(format!("/upstream/artist/{MB_ARTIST}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": MB_ARTIST, "artistname": "Known Band", "Albums": [], "images": [{"CoverType": "Poster", "Url": "http://img/x.jpg"}],
            "links": [], "sortname": "Known Band"
        })))
        .expect(1)
        .mount(&h.server)
        .await;
    for _ in 0..2 {
        // The cache is written in background: give it a moment.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let (status, v) = h.get(&format!("/artist/{MB_ARTIST}")).await;
        assert_eq!(status, 200);
        assert_eq!(v["artistName"], "Known Band");
        assert_eq!(v["sortname"], "Known Band", "unknown upstream fields are preserved");
        assert!(v.get("artistname").is_none(), "keys are not duplicated in two casings");
        assert_eq!(v["images"][0]["url"], "http://img/x.jpg");
    }
}

#[tokio::test]
async fn upstream_errors_are_503_never_404() {
    let h = Harness::new().await;
    Mock::given(method("GET"))
        .and(path("/upstream/album/99999999-9999-4999-8999-999999999999"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;
    let (status, _) = h.get("/album/99999999-9999-4999-8999-999999999999").await;
    assert_eq!(status, 503);

    Mock::given(method("GET"))
        .and(path("/upstream/album/88888888-8888-4888-8888-888888888888"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.server)
        .await;
    let (status, _) = h.get("/album/88888888-8888-4888-8888-888888888888").await;
    assert_eq!(status, 404);

    let (status, _) = h.get("/artist/not-a-uuid").await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn stale_cache_served_when_upstream_down() {
    let h = Harness::with(|c| c.upstream_cache_ttl = std::time::Duration::ZERO).await;
    let id = "77777777-7777-4777-8777-777777777777";
    Mock::given(method("GET"))
        .and(path(format!("/upstream/artist/{id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": id, "artistname": "Cached"})))
        .up_to_n_times(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/upstream/artist/{id}")))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    assert_eq!(h.get(&format!("/artist/{id}")).await.0, 200);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await; // background cache write
    let (status, v) = h.get(&format!("/artist/{id}")).await;
    assert_eq!(status, 200);
    assert_eq!(v["artistName"], "Cached");
}

#[tokio::test]
async fn synthetic_artist_from_search_then_lookup() {
    let h = Harness::new().await;
    h.mock_empty_upstream_search().await;
    h.mock_deezer_indie().await;
    h.mock_json(
        "/deezer/search/artist",
        json!({"data": [{"id": 27, "name": "Indie Person",
        "link": "https://www.deezer.com/artist/27", "picture_xl": format!("{}/cdn/artist27.png", h.uri())}]}),
    )
    .await;
    // No MB URL relation for the Deezer artist (wiremock answers 404).

    let (status, v) = h.get("/search?type=artist&query=indie%20person").await;
    assert_eq!(status, 200);
    let list = v.as_array().unwrap();
    assert_eq!(list.len(), 1);
    let id = synthetic_id(Source::Deezer, Kind::Artist, "27");
    assert_eq!(list[0]["id"], id.as_str());

    let (status, v) = h.get(&format!("/artist/{id}")).await;
    assert_eq!(status, 200, "{v}");
    check_artist(&v);
    assert_eq!(v["artistName"], "Indie Person");
    // Poster downloaded and served by the proxy, with measured size.
    let poster = &v["images"][0];
    assert_eq!(poster["coverType"], "poster");
    assert!(poster["url"].as_str().unwrap().starts_with("http://proxy.test/img/"));
    assert_eq!(poster["width"], 1000);
    // Regional duplicate (1009) and the other artist's compilation are dropped.
    let titles: Vec<&str> = v["albums"].as_array().unwrap().iter().map(|a| a["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["Debut", "Small Things"]);
    let ep = &v["albums"][1];
    assert_eq!(ep["type"], "EP");
    assert_eq!(ep["releaseStatuses"], json!(["Official"]));
    assert_eq!(ep["artistId"], id.as_str());

    // The image is served from the local cache.
    let file = poster["url"].as_str().unwrap().trim_start_matches("http://proxy.test");
    let (status, body, ct) = h.get_raw(file).await;
    assert_eq!(status, 200);
    assert_eq!(ct.as_deref(), Some("image/png"));
    assert_eq!(body, png(1000, 1000));
}

#[tokio::test]
async fn synthetic_album_has_tracks_media_and_itunes_cover() {
    let h = Harness::new().await;
    h.mock_deezer_indie().await;
    let artist_id = synthetic_id(Source::Deezer, Kind::Artist, "27");
    // The artist must be known first (as after a search).
    h.app.save_artist(&h.app.deezer.as_ref().unwrap().artist("27").await.unwrap().unwrap()).await.unwrap();
    assert_eq!(h.get(&format!("/artist/{artist_id}")).await.0, 200);

    let album_id = synthetic_id(Source::Deezer, Kind::Album, "1001");
    let (status, v) = h.get(&format!("/album/{album_id}")).await;
    assert_eq!(status, 200, "{v}");
    check_album(&v);
    assert_eq!(v["artistId"], artist_id.as_str());
    assert_eq!(v["genres"], json!(["Pop"]));
    let rel = &v["releases"][0];
    assert_eq!(rel["id"], synthetic_id(Source::Deezer, Kind::Release, "1001").as_str());
    assert_ne!(rel["id"], v["id"], "album and release IDs differ");
    assert_eq!(rel["trackCount"], 3);
    assert_eq!(rel["label"], json!(["Tiny Label"]));
    assert_eq!(rel["media"].as_array().unwrap().len(), 2);
    let t = &rel["tracks"][1];
    assert_eq!(t["durationMs"], 305000);
    assert_eq!(t["explicit"], true);
    assert_eq!(t["recordingId"], synthetic_id(Source::Deezer, Kind::Recording, "QZAAA2100002").as_str());
    let bonus = &rel["tracks"][2];
    assert_eq!(
        (bonus["mediumNumber"].as_i64(), bonus["trackNumber"].as_str(), bonus["trackPosition"].as_i64()),
        (Some(2), Some("1"), Some(3))
    );
    // iTunes artwork wins over Deezer's cover.
    assert_eq!(v["images"][0]["coverType"], "cover");
    assert_eq!(v["images"][0]["width"], 3000);
}

#[tokio::test]
async fn mb_artist_enriched_with_missing_deezer_albums() {
    let h = Harness::new().await;
    h.mock_mb_artist_with_deezer().await;
    let since = lidarr_meta_proxy::ids::now() - 10;

    let (status, v) = h.get(&format!("/artist/{MB_ARTIST}")).await;
    assert_eq!(status, 200, "{v}");
    check_artist(&v);
    let albums = v["albums"].as_array().unwrap();
    // "First Album (Remastered 2020)" matches by title, "Barcode Match" by barcode.
    let titles: Vec<&str> = albums.iter().map(|a| a["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["First Album", "Missing Album"]);
    let added = &albums[1];
    assert_eq!(added["id"], synthetic_id(Source::Deezer, Kind::Album, "2002").as_str());
    assert_eq!(added["artistId"], MB_ARTIST, "synthetic album hangs under the MB artist");
    // MB artist had no poster: integrated from the URL-linked Deezer artist.
    assert_eq!(v["images"][0]["coverType"], "poster");

    // The enriched MB artist shows up in /recent/artist so Lidarr refreshes it.
    let (_, recent) = h.get(&format!("/recent/artist?since={since}")).await;
    assert!(recent["items"].as_array().unwrap().iter().any(|i| i == MB_ARTIST), "{recent}");

    // The added album resolves with the MB artist as owner.
    let (status, a) = h.get(&format!("/album/{}", added["id"].as_str().unwrap())).await;
    assert_eq!(status, 200, "{a}");
    check_album(&a);
    assert_eq!(a["artistId"], MB_ARTIST);
    assert_eq!(a["artists"][0]["artistName"], "Known Band");
}

#[tokio::test]
async fn search_replaces_mb_linked_hits_with_mb_entity() {
    let h = Harness::new().await;
    h.mock_mb_artist_with_deezer().await;
    h.mock_deezer_indie().await;
    h.mock_empty_upstream_search().await;
    h.mock_json(
        "/deezer/search/artist",
        json!({"data": [
            {"id": 500, "name": "Known Band", "link": "https://www.deezer.com/artist/500"},
            {"id": 27, "name": "Indie Person", "link": "https://www.deezer.com/artist/27"}
        ]}),
    )
    .await;
    h.mock_mb_url("https://www.deezer.com/artist/500", json!({"relations": [{"artist": {"id": MB_ARTIST}}]})).await;

    let (status, v) = h.get("/search?type=artist&query=band").await;
    assert_eq!(status, 200);
    let ids: Vec<&str> = v.as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![MB_ARTIST, synthetic_id(Source::Deezer, Kind::Artist, "27").as_str()]);
    assert!(!ids.contains(&synthetic_id(Source::Deezer, Kind::Artist, "500").as_str()));

    // type=all wraps results in EntityResource.
    let (status, v) = h.get("/search?type=all&query=band").await;
    assert_eq!(status, 200);
    assert!(v.as_array().unwrap().iter().all(|e| e.get("score").is_some()));
}

#[tokio::test]
async fn album_search_returns_releases_with_tracks() {
    let h = Harness::new().await;
    h.mock_deezer_indie().await;
    h.mock_empty_upstream_search().await;
    h.mock_json(
        "/deezer/search/album",
        json!({"data": [
            {"id": 1001, "title": "Debut", "record_type": "album", "link": "https://www.deezer.com/album/1001",
             "artist": {"id": 27, "name": "Indie Person"}}
        ]}),
    )
    .await;
    let (status, v) = h.get("/search?type=album&query=debut&artist=indie%20person&includeTracks=1").await;
    assert_eq!(status, 200, "{v}");
    let list = v.as_array().unwrap();
    assert_eq!(list.len(), 1);
    check_album(&list[0]);
    assert_eq!(list[0]["artistId"], synthetic_id(Source::Deezer, Kind::Artist, "27").as_str());
}

#[tokio::test]
async fn reconciliation_merges_into_mb_with_old_ids() {
    let h = Harness::new().await;
    h.mock_deezer_indie().await;
    let artist_id = synthetic_id(Source::Deezer, Kind::Artist, "27");
    let album_id = synthetic_id(Source::Deezer, Kind::Album, "1001");
    h.app.save_artist(&h.app.deezer.as_ref().unwrap().artist("27").await.unwrap().unwrap()).await.unwrap();
    assert_eq!(h.get(&format!("/artist/{artist_id}")).await.0, 200);
    assert_eq!(h.get(&format!("/album/{album_id}")).await.0, 200);

    // The artist and the album now exist on MusicBrainz (stubbed).
    let mb_artist = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let mb_rg = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let mb_rel = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    h.mock_mb_url("https://www.deezer.com/artist/27", json!({"relations": [{"artist": {"id": mb_artist}}]})).await;
    h.mock_mb_url("https://www.deezer.com/album/1001", json!({"relations": [{"release": {"id": mb_rel}}]})).await;
    h.mock_json(
        &format!("/mb/release/{mb_rel}"),
        json!({"id": mb_rel, "title": "Debut", "barcode": "0123456789012", "release-group": {"id": mb_rg},
               "artist-credit": [{"artist": {"id": mb_artist}}],
               "media": [
                 {"position": 1, "tracks": [
                    {"id": "d0000000-0000-4000-8000-000000000001", "position": 1, "recording": {"id": "e0000000-0000-4000-8000-000000000001", "isrcs": ["QZAAA2100001"]}},
                    {"id": "d0000000-0000-4000-8000-000000000002", "position": 2, "recording": {"id": "e0000000-0000-4000-8000-000000000002", "isrcs": []}}]},
                 {"position": 2, "tracks": [
                    {"id": "d0000000-0000-4000-8000-000000000003", "position": 1, "recording": {"id": "e0000000-0000-4000-8000-000000000003"}}]}
               ]}),
    )
    .await;
    h.mock_json(
        &format!("/upstream/artist/{mb_artist}"),
        json!({"id": mb_artist, "artistname": "Indie Person", "links": [], "images": [],
               "Albums": [{"Id": mb_rg, "Title": "Debut", "Type": "Album", "SecondaryTypes": [], "ReleaseStatuses": ["Official"]}]}),
    )
    .await;
    h.mock_json(
        &format!("/upstream/album/{mb_rg}"),
        json!({"id": mb_rg, "artistid": mb_artist, "title": "Debut", "type": "Album", "secondarytypes": [], "releasestatuses": ["Official"],
               "images": [{"CoverType": "Cover", "Url": "http://caa/x.jpg"}],
               "artists": [{"id": mb_artist, "artistname": "Indie Person"}],
               "releases": [{"id": mb_rel, "title": "Debut", "status": "Official", "media": [{"Format": "CD", "Name": "", "Position": 1}],
                             "trackcount": 1, "tracks": [{"id": "d0000000-0000-4000-8000-000000000001",
                             "recordingid": "e0000000-0000-4000-8000-000000000001", "artistid": mb_artist,
                             "trackname": "Opening", "tracknumber": "1", "trackposition": 1, "mediumnumber": 1, "durationms": 201000}]}]}),
    )
    .await;

    let since = lidarr_meta_proxy::ids::now() - 1;
    let stats = h.app.reconcile_once().await.unwrap();
    assert_eq!((stats.artists, stats.albums, stats.tracks), (1, 1, 3));

    // Recent lists both synthetic IDs so Lidarr refreshes them.
    let (_, recent) = h.get(&format!("/recent/album?since={since}")).await;
    assert!(recent["items"].as_array().unwrap().iter().any(|i| i == album_id.as_str()), "{recent}");
    let (_, recent) = h.get(&format!("/recent/artist?since={since}")).await;
    assert!(recent["items"].as_array().unwrap().iter().any(|i| i == artist_id.as_str()));

    // Requesting the synthetic artist now yields the MB entity carrying the old ID.
    let (status, v) = h.get(&format!("/artist/{artist_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(v["id"], mb_artist);
    assert!(v["oldIds"].as_array().unwrap().iter().any(|i| i == artist_id.as_str()));
    let album = v["albums"].as_array().unwrap().iter().find(|a| a["id"] == mb_rg).unwrap();
    assert!(album["oldIds"].as_array().unwrap().iter().any(|i| i == album_id.as_str()));
    // Albums still missing from MB stay attached, with their synthetic IDs, under the MB artist.
    let ep = v["albums"].as_array().unwrap().iter().find(|a| a["title"] == "Small Things").expect("EP kept");
    assert_eq!(ep["id"], synthetic_id(Source::Deezer, Kind::Album, "1003").as_str());
    assert_eq!(ep["artistId"], mb_artist);
    // The reconciled album is not added a second time as synthetic.
    assert!(!v["albums"].as_array().unwrap().iter().any(|a| a["id"] == album_id.as_str()));

    let (status, v) = h.get(&format!("/album/{album_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(v["id"], mb_rg);
    assert!(v["oldIds"].as_array().unwrap().iter().any(|i| i == album_id.as_str()));
    let rel = &v["releases"][0];
    assert!(
        rel["oldIds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i == synthetic_id(Source::Deezer, Kind::Release, "1001").as_str())
    );
    let t = &rel["tracks"][0];
    assert!(
        t["oldIds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i == synthetic_id(Source::Deezer, Kind::Track, "50001").as_str())
    );
    assert!(
        t["oldRecordingIds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i == synthetic_id(Source::Deezer, Kind::Recording, "QZAAA2100001").as_str())
    );
    assert!(v["artists"][0]["oldIds"].as_array().unwrap().iter().any(|i| i == artist_id.as_str()));
}

#[tokio::test]
async fn fingerprint_is_passed_through() {
    let h = Harness::new().await;
    Mock::given(method("POST"))
        .and(path("/upstream/search/fingerprint"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": "x"}])))
        .mount(&h.server)
        .await;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let req = Request::post("/search/fingerprint")
        .header("content-type", "application/json")
        .body(Body::from("[\"abc\"]"))
        .unwrap();
    let resp = h.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!([{"id": "x"}]));
}

#[tokio::test]
async fn recent_merges_upstream_and_marks_limited_on_failure() {
    let h = Harness::new().await;
    h.mock_json(
        "/upstream/recent/album",
        json!({"count": 1, "limited": false, "since": "2026-01-01T00:00:00Z", "items": ["up-1"]}),
    )
    .await;
    let (status, v) = h.get("/recent/album?since=0").await;
    assert_eq!(status, 200);
    assert_eq!(v["items"], json!(["up-1"]));
    assert_eq!(v["limited"], false);
    // No upstream mock for artists → 404 → treated as unknown → limited.
    let (_, v) = h.get("/recent/artist?since=0").await;
    assert_eq!(v["limited"], true);
}

#[tokio::test]
async fn health_reports_status() {
    let h = Harness::new().await;
    let (status, v) = h.get("/health").await;
    assert_eq!(status, 200);
    assert_eq!(v["database"], true);
}

#[tokio::test]
async fn namespace_is_pinned_in_the_database() {
    use lidarr_meta_proxy::{ids::parse_namespace, store::Store};
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("meta.db");
    let ns = parse_namespace("my-seed").to_string();
    Store::open(&db).await.unwrap().check_namespace(&ns).await.unwrap();
    // Same value: fine. Different value: refused, IDs would all change.
    let store = Store::open(&db).await.unwrap();
    store.check_namespace(&ns).await.unwrap();
    let err = store.check_namespace(&parse_namespace("other").to_string()).await.unwrap_err();
    assert!(err.to_string().contains("UUID_NAMESPACE mismatch"), "{err}");
}

#[tokio::test]
async fn enrichment_adds_nothing_when_musicbrainz_is_unavailable() {
    let h = Harness::new().await;
    // MB refuses the release browse (as when throttled): mounted first, so it wins.
    Mock::given(method("GET"))
        .and(path("/mb/release"))
        .respond_with(ResponseTemplate::new(403).set_body_string("throttled"))
        .mount(&h.server)
        .await;
    h.mock_mb_artist_with_deezer().await;

    let (status, v) = h.get(&format!("/artist/{MB_ARTIST}")).await;
    assert_eq!(status, 200, "{v}");
    let titles: Vec<&str> = v["albums"].as_array().unwrap().iter().map(|a| a["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["First Album"], "without the barcode check no album is added");
    // Nothing cached: a later request retries the enrichment.
    let (_, recent) = h.get("/recent/artist?since=0").await;
    assert!(!recent["items"].as_array().unwrap().iter().any(|i| i == MB_ARTIST));
}

#[tokio::test]
async fn slow_musicbrainz_does_not_block_search() {
    // MusicBrainz answers after 1.5 s; the search may wait 300 ms.
    let h = Harness::with(|c| c.search_timeout = std::time::Duration::from_millis(300)).await;
    h.mock_empty_upstream_search().await;
    h.mock_deezer_indie().await;
    h.mock_json(
        "/deezer/search/artist",
        json!({"data": [{"id": 27, "name": "Indie Person", "link": "https://www.deezer.com/artist/27"}]}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/mb/url"))
        .respond_with(ResponseTemplate::new(404).set_delay(std::time::Duration::from_millis(1500)))
        .mount(&h.server)
        .await;

    let start = std::time::Instant::now();
    let (status, v) = h.get("/search?type=artist&query=indie").await;
    assert_eq!(status, 200);
    assert!(start.elapsed() < std::time::Duration::from_millis(1200), "search waited for MB: {:?}", start.elapsed());
    assert_eq!(v, json!([]), "unverified hit is not returned as synthetic");

    // Verification continues in background and is cached: the same search now has the hit.
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
    let (_, v) = h.get("/search?type=artist&query=indie").await;
    assert_eq!(v[0]["id"], synthetic_id(Source::Deezer, Kind::Artist, "27").as_str());
}

#[tokio::test]
async fn typing_supersedes_older_related_searches() {
    use wiremock::matchers::query_param;
    let h = Harness::with(|c| c.search_debounce = std::time::Duration::from_millis(300)).await;
    // "que" must never reach upstream: "queen" (typed 50 ms later) supersedes it.
    Mock::given(method("GET"))
        .and(path("/upstream/search"))
        .and(query_param("query", "que"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(0)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/upstream/search"))
        .and(query_param("query", "queen"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"score": 100, "artist": {"id": MB_ARTIST, "artistname": "Queen"}}])),
        )
        .expect(1)
        .mount(&h.server)
        .await;
    // An unrelated search running at the same time (another user) is not affected.
    Mock::given(method("GET"))
        .and(path("/upstream/search"))
        .and(query_param("query", "abba"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&h.server)
        .await;

    let first = h.get("/search?type=all&query=que");
    let second = async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        h.get("/search?type=all&query=queen").await
    };
    let third = async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        h.get("/search?type=all&query=abba").await
    };
    let ((s1, v1), (s2, v2), (s3, _)) = tokio::join!(first, second, third);
    assert_eq!((s1, s2, s3), (200, 200, 200));
    assert_eq!(v1, json!([]));
    assert_eq!(v2[0]["artist"]["artistName"], "Queen");
}

#[tokio::test]
async fn bare_mbid_search_finds_artist_or_release_group() {
    let h = Harness::new().await;
    h.mock_mb_artist_with_deezer().await;
    h.mock_json(
        &format!("/upstream/album/{MB_RG_FIRST}"),
        json!({"id": MB_RG_FIRST, "artistid": MB_ARTIST, "title": "First Album", "type": "Album",
               "artists": [{"id": MB_ARTIST, "artistname": "Known Band"}], "releases": []}),
    )
    .await;
    // Artist ID.
    let (status, v) = h.get(&format!("/search?type=all&query={MB_ARTIST}")).await;
    assert_eq!(status, 200);
    assert_eq!(v[0]["artist"]["id"], MB_ARTIST);
    // Release-group ID: /artist/{id} is 404 upstream, /album/{id} answers.
    let (status, v) = h.get(&format!("/search?type=all&query={MB_RG_FIRST}")).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v[0]["album"]["id"], MB_RG_FIRST);
    // Unknown ID: empty, not an error.
    let (status, v) = h.get("/search?type=all&query=99999999-9999-4999-8999-999999999999").await;
    assert_eq!((status, v), (200, json!([])));
}

#[tokio::test]
async fn upstream_429_is_retried_after_retry_after() {
    let h = Harness::new().await;
    let id = "66666666-6666-4666-8666-666666666666";
    Mock::given(method("GET"))
        .and(path(format!("/upstream/artist/{id}")))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/upstream/artist/{id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": id, "artistname": "After Retry"})))
        .mount(&h.server)
        .await;
    let (status, v) = h.get(&format!("/artist/{id}")).await;
    assert_eq!(status, 200);
    assert_eq!(v["artistName"], "After Retry");
}

#[tokio::test]
async fn upstream_cache_is_compressed_and_pruned() {
    use lidarr_meta_proxy::store::Store;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("meta.db")).await.unwrap();
    let body = serde_json::to_vec(&json!({"albums": vec!["x".repeat(100); 1000]})).unwrap();
    store.upstream_put("artist/x", &body, 200, Some("\"e\"")).await.unwrap();
    store.upstream_put("search?query=old&type=all", b"[]", 200, None).await.unwrap();
    let got = store.upstream_get("artist/x").await.unwrap().unwrap();
    assert_eq!(got.body, body);
    assert_eq!(got.etag.as_deref(), Some("\"e\""));
    let stored: (i64,) = sqlx::query_as("SELECT length(body) FROM upstream_cache WHERE route = 'artist/x'")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert!((stored.0 as usize) < body.len() / 10, "stored {} of {} bytes", stored.0, body.len());
    // Old search answers are dropped, recent ones kept.
    sqlx::query("UPDATE upstream_cache SET fetched_at = fetched_at - 8 * 86400 WHERE route LIKE 'search%'")
        .execute(&store.pool)
        .await
        .unwrap();
    assert_eq!(store.maintenance().await.unwrap(), 1);
    assert!(store.upstream_get("search?query=old&type=all").await.unwrap().is_none());
    assert!(store.upstream_get("artist/x").await.unwrap().is_some());
}
