//! Third-party sources and the normalized model stored as entity payloads.

pub mod deezer;
pub mod itunes;
pub mod musicbrainz;
pub mod spotify;

use serde::{Deserialize, Serialize};

use crate::ids::Source;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SrcImage {
    pub url: String,
    /// Dimensions declared by the source (the real ones are measured on download).
    pub width: i64,
    pub height: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SrcArtist {
    pub source: Source,
    pub id: String,
    pub name: String,
    pub link: String,
    #[serde(default)]
    pub genres: Vec<String>,
    /// Best image first.
    #[serde(default)]
    pub images: Vec<SrcImage>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SrcAlbum {
    pub source: Source,
    pub id: String,
    pub title: String,
    /// Deezer `record_type` (album/ep/single/compile) or Spotify `album_type`.
    pub record_type: String,
    pub release_date: Option<String>,
    pub upc: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub explicit: bool,
    pub link: String,
    /// Main artist as known by the source.
    pub artist_id: Option<String>,
    pub artist_name: Option<String>,
    #[serde(default)]
    pub images: Vec<SrcImage>,
    /// `None` when only the summary (discography listing) is known.
    #[serde(default)]
    pub tracks: Option<Vec<SrcTrack>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SrcTrack {
    pub id: String,
    pub title: String,
    pub disc: i64,
    pub position: i64,
    pub duration_ms: i64,
    pub isrc: Option<String>,
    #[serde(default)]
    pub explicit: bool,
    #[serde(default)]
    pub artist_id: Option<String>,
}

impl Serialize for Source {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Source::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("unknown source {s}")))
    }
}

/// Lidarr primary type + secondary types for a source record type.
pub fn map_record_type(record_type: &str, track_count: Option<usize>) -> (&'static str, Vec<String>) {
    match record_type.to_ascii_lowercase().as_str() {
        "ep" => ("EP", vec![]),
        "single" => match track_count {
            // Spotify reports EPs as singles; MusicBrainz guidelines treat 4+ tracks as EP.
            Some(n) if n >= 4 => ("EP", vec![]),
            _ => ("Single", vec![]),
        },
        "compile" | "compilation" => ("Album", vec!["Compilation".to_string()]),
        _ => ("Album", vec![]),
    }
}

/// Extracts `(source, kind, id)` from a Deezer/Spotify web URL.
pub fn parse_source_url(url: &str) -> Option<(Source, &'static str, String)> {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    let (source, rest) = if let Some(i) = lower.find("deezer.com/") {
        (Source::Deezer, &u[i + "deezer.com/".len()..])
    } else {
        let i = lower.find("open.spotify.com/")?;
        (Source::Spotify, &u[i + "open.spotify.com/".len()..])
    };
    let parts: Vec<&str> = rest.split(['/', '?', '#']).filter(|p| !p.is_empty()).collect();
    // Skip locale segments such as "en" or "intl-it".
    let start = parts.iter().position(|p| matches!(*p, "artist" | "album" | "track"))?;
    let kind = match parts[start] {
        "artist" => "artist",
        "album" => "album",
        _ => "track",
    };
    let id = parts.get(start + 1)?.to_string();
    Some((source, kind, id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(parse_source_url("https://www.deezer.com/artist/27"), Some((Source::Deezer, "artist", "27".into())));
        assert_eq!(
            parse_source_url("https://www.deezer.com/en/album/302127?x=1"),
            Some((Source::Deezer, "album", "302127".into()))
        );
        assert_eq!(
            parse_source_url("https://open.spotify.com/intl-it/artist/4tZwfgrHOc3mvqYlEYSvVi"),
            Some((Source::Spotify, "artist", "4tZwfgrHOc3mvqYlEYSvVi".into()))
        );
        assert_eq!(parse_source_url("https://example.com/artist/1"), None);
    }

    #[test]
    fn record_types() {
        assert_eq!(map_record_type("compile", None), ("Album", vec!["Compilation".to_string()]));
        assert_eq!(map_record_type("single", Some(5)).0, "EP");
        assert_eq!(map_record_type("single", Some(2)).0, "Single");
        assert_eq!(map_record_type("ep", None).0, "EP");
    }
}
