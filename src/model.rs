//! JSON contract with Lidarr, mirroring `NzbDrone.Core/MetadataSource/SkyHook/Resource/*.cs`.
//!
//! Every struct keeps unknown fields in `extra` so upstream responses survive a
//! deserialize/serialize round trip unchanged. List fields default to `[]` and are
//! always serialized: Lidarr calls `.Select()` on several of them without null checks.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImageResource {
    pub cover_type: String,
    pub url: String,
    #[serde(default)]
    pub height: i64,
    #[serde(default)]
    pub width: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LinkResource {
    #[serde(default)]
    pub target: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RatingResource {
    #[serde(default)]
    pub count: i64,
    #[serde(default)]
    pub value: f64,
}

impl Default for RatingResource {
    fn default() -> Self {
        Self { count: 0, value: 0.0 }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ArtistResource {
    pub id: String,
    #[serde(default)]
    pub old_ids: Vec<String>,
    #[serde(default)]
    pub artist_name: String,
    #[serde(default)]
    pub artist_aliases: Vec<String>,
    #[serde(default)]
    pub disambiguation: String,
    #[serde(default)]
    pub overview: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub images: Vec<ImageResource>,
    #[serde(default)]
    pub links: Vec<LinkResource>,
    #[serde(default)]
    pub rating: RatingResource,
    #[serde(default)]
    pub albums: Vec<AlbumResource>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AlbumResource {
    pub id: String,
    #[serde(default)]
    pub old_ids: Vec<String>,
    #[serde(default)]
    pub artist_id: String,
    #[serde(default)]
    pub artists: Vec<ArtistResource>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub disambiguation: String,
    #[serde(default)]
    pub overview: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub secondary_types: Vec<String>,
    #[serde(default)]
    pub release_statuses: Vec<String>,
    /// ISO-8601 date (`yyyy-MM-dd`), deserialized by Lidarr as `DateTime?`.
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub images: Vec<ImageResource>,
    #[serde(default)]
    pub links: Vec<LinkResource>,
    #[serde(default)]
    pub rating: RatingResource,
    /// `None` keeps the field absent (artist album lists); Lidarr treats null as "no releases".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub releases: Option<Vec<ReleaseResource>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseResource {
    pub id: String,
    #[serde(default)]
    pub old_ids: Vec<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub country: Vec<String>,
    #[serde(default)]
    pub label: Vec<String>,
    #[serde(default)]
    pub media: Vec<MediumResource>,
    #[serde(default)]
    pub track_count: i64,
    #[serde(default)]
    pub tracks: Vec<TrackResource>,
    #[serde(default)]
    pub disambiguation: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MediumResource {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub position: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TrackResource {
    pub id: String,
    #[serde(default)]
    pub old_ids: Vec<String>,
    #[serde(default)]
    pub recording_id: String,
    #[serde(default)]
    pub old_recording_ids: Vec<String>,
    #[serde(default)]
    pub artist_id: String,
    #[serde(default)]
    pub track_name: String,
    #[serde(default)]
    pub track_number: String,
    #[serde(default)]
    pub track_position: i64,
    #[serde(default)]
    pub medium_number: i64,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default)]
    pub explicit: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EntityResource {
    #[serde(default)]
    pub score: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<ArtistResource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<AlbumResource>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RecentUpdatesResource {
    #[serde(default)]
    pub count: i64,
    #[serde(default)]
    pub limited: bool,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub items: Vec<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Album types accepted by Lidarr metadata profiles.
pub const PRIMARY_TYPES: &[&str] = &["Album", "EP", "Single", "Broadcast", "Other"];

impl ArtistResource {
    /// Copy without the album list, as embedded in `AlbumResource.artists`.
    pub fn without_albums(&self) -> Self {
        Self { albums: vec![], ..self.clone() }
    }

    pub fn has_image(&self, cover_type: &str) -> bool {
        self.images.iter().any(|i| i.cover_type.eq_ignore_ascii_case(cover_type))
    }
}

impl AlbumResource {
    pub fn has_image(&self, cover_type: &str) -> bool {
        self.images.iter().any(|i| i.cover_type.eq_ignore_ascii_case(cover_type))
    }
}

/// Canonical (camelCase) names of every field of the resources above.
const CANONICAL_KEYS: &[&str] = &[
    "id",
    "oldIds",
    "artistName",
    "artistAliases",
    "disambiguation",
    "overview",
    "type",
    "status",
    "genres",
    "images",
    "links",
    "rating",
    "albums",
    "artistId",
    "artists",
    "title",
    "secondaryTypes",
    "releaseStatuses",
    "releaseDate",
    "releases",
    "country",
    "label",
    "media",
    "trackCount",
    "tracks",
    "name",
    "format",
    "position",
    "recordingId",
    "oldRecordingIds",
    "trackName",
    "trackNumber",
    "trackPosition",
    "mediumNumber",
    "durationMs",
    "explicit",
    "coverType",
    "url",
    "height",
    "width",
    "target",
    "count",
    "value",
    "score",
    "artist",
    "album",
    "limited",
    "since",
    "items",
];

/// The upstream metadata server answers with inconsistent key casing (`artistname`,
/// `Albums`, `CoverType`, ...) and Lidarr matches keys case-insensitively. Rewrites
/// known keys to their canonical camelCase spelling, recursively, so typed
/// deserialization sees every field and no key is emitted twice.
pub fn normalize_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                let key =
                    CANONICAL_KEYS.iter().find(|c| c.eq_ignore_ascii_case(&k)).map(|c| c.to_string()).unwrap_or(k);
                let v = normalize_keys(v);
                // Keep the first non-null value if two spellings collide.
                match out.get(&key) {
                    Some(existing) if !existing.is_null() => {}
                    _ => {
                        out.insert(key, v);
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(normalize_keys).collect()),
        other => other,
    }
}

/// Deserializes an upstream JSON body after key normalization. Null scalars are
/// treated as absent so `#[serde(default)]` applies.
pub fn from_upstream<T: serde::de::DeserializeOwned>(value: Value) -> serde_json::Result<T> {
    serde_json::from_value(strip_nulls(normalize_keys(value)))
}

fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, strip_nulls(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

pub fn push_unique(list: &mut Vec<String>, value: &str) {
    if !value.is_empty() && !list.iter().any(|v| v.eq_ignore_ascii_case(value)) {
        list.push(value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_unknown_fields() {
        let raw = serde_json::json!({
            "id": "a", "artistName": "X", "albums": [], "images": [],
            "someFutureField": {"k": 1}, "AristUrl": "u"
        });
        let a: ArtistResource = serde_json::from_value(raw.clone()).unwrap();
        let back = serde_json::to_value(&a).unwrap();
        assert_eq!(back["someFutureField"], raw["someFutureField"]);
        assert_eq!(back["AristUrl"], "u");
        assert_eq!(back["oldIds"], serde_json::json!([]));
    }

    #[test]
    fn normalizes_upstream_casing() {
        let raw = serde_json::json!({
            "id": "a", "artistname": "X", "Albums": [{"Id": "b", "Title": "T", "ReleaseStatuses": ["Official"], "SecondaryTypes": null}],
            "images": [{"CoverType": "Poster", "Url": "http://x"}], "oldids": ["z"], "Rating": {"Count": 2, "Value": 7.5},
            "sortname": "x"
        });
        let a: ArtistResource = from_upstream(raw).unwrap();
        assert_eq!(a.artist_name, "X");
        assert_eq!(a.albums[0].title, "T");
        assert_eq!(a.albums[0].release_statuses, vec!["Official"]);
        assert!(a.albums[0].secondary_types.is_empty());
        assert!(a.has_image("poster"));
        assert_eq!(a.old_ids, vec!["z"]);
        assert_eq!(a.rating.count, 2);
        assert_eq!(serde_json::to_value(&a).unwrap()["sortname"], "x");
    }

    #[test]
    fn album_without_releases_omits_field() {
        let a = AlbumResource { id: "x".into(), ..Default::default() };
        let v = serde_json::to_value(&a).unwrap();
        assert!(v.get("releases").is_none());
        assert_eq!(v["secondaryTypes"], serde_json::json!([]));
    }
}
