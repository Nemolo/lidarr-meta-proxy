//! Deezer public API (no key). Limit is ~50 requests / 5 s per IP; we stay below it.

use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use super::{SrcAlbum, SrcArtist, SrcImage, SrcTrack};
use crate::{
    http::SourceClient,
    ids::{Source, normalize_date},
};

#[derive(Clone)]
pub struct Deezer {
    client: SourceClient,
    base: String,
}

fn s(v: &Value, k: &str) -> Option<String> {
    match v.get(k)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn i(v: &Value, k: &str) -> Option<i64> {
    match v.get(k)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Deezer answers errors with HTTP 200 and `{"error": {...}}`; code 800 = no data.
fn check(v: Option<Value>) -> Result<Option<Value>> {
    match v {
        Some(v) => match v.get("error") {
            Some(e) => {
                let code = e.get("code").and_then(Value::as_i64).unwrap_or(0);
                if code == 800 {
                    Ok(None)
                } else {
                    anyhow::bail!("deezer error {code}: {}", e.get("message").and_then(Value::as_str).unwrap_or(""))
                }
            }
            None => Ok(Some(v)),
        },
        None => Ok(None),
    }
}

fn picture(v: &Value, prefix: &str) -> Vec<SrcImage> {
    // `*_xl` is 1000x1000; skip Deezer's generic placeholder (empty md5).
    let mut out = vec![];
    if let Some(url) = s(v, &format!("{prefix}_xl"))
        && !url.contains("/images/artist//")
        && !url.contains("/images/cover//")
    {
        out.push(SrcImage { url, width: 1000, height: 1000 });
    }
    out
}

pub fn artist_from(v: &Value) -> Option<SrcArtist> {
    let id = s(v, "id")?;
    Some(SrcArtist {
        source: Source::Deezer,
        name: s(v, "name")?,
        link: s(v, "link").unwrap_or_else(|| format!("https://www.deezer.com/artist/{id}")),
        genres: vec![],
        images: picture(v, "picture"),
        id,
    })
}

pub fn album_from(v: &Value) -> Option<SrcAlbum> {
    let id = s(v, "id")?;
    let artist = v.get("artist");
    let genres = v
        .pointer("/genres/data")
        .and_then(Value::as_array)
        .map(|g| g.iter().filter_map(|x| s(x, "name")).collect())
        .unwrap_or_default();
    Some(SrcAlbum {
        source: Source::Deezer,
        title: s(v, "title")?,
        record_type: s(v, "record_type").unwrap_or_else(|| "album".into()),
        release_date: s(v, "release_date").and_then(|d| normalize_date(&d)),
        upc: s(v, "upc"),
        label: s(v, "label"),
        genres,
        explicit: v.get("explicit_lyrics").and_then(Value::as_bool).unwrap_or(false),
        link: s(v, "link").unwrap_or_else(|| format!("https://www.deezer.com/album/{id}")),
        artist_id: artist.and_then(|a| s(a, "id")),
        artist_name: artist.and_then(|a| s(a, "name")),
        images: picture(v, "cover"),
        tracks: None,
        id,
    })
}

pub fn track_from(v: &Value, fallback_pos: i64) -> Option<SrcTrack> {
    Some(SrcTrack {
        id: s(v, "id")?,
        title: s(v, "title")?,
        disc: i(v, "disk_number").filter(|d| *d > 0).unwrap_or(1),
        position: i(v, "track_position").filter(|p| *p > 0).unwrap_or(fallback_pos),
        // Deezer only exposes whole seconds.
        duration_ms: i(v, "duration").unwrap_or(0) * 1000,
        isrc: s(v, "isrc"),
        explicit: v.get("explicit_lyrics").and_then(Value::as_bool).unwrap_or(false),
        artist_id: v.get("artist").and_then(|a| s(a, "id")),
    })
}

impl Deezer {
    pub fn new(client: SourceClient, base: String) -> Self {
        Self { client, base }
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Option<Value>> {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let q = query.to_vec();
        loop {
            let v = self.client.json(|c| c.get(&url).query(&q)).await?;
            // Code 4 = quota exceeded: wait and retry.
            if v.as_ref().and_then(|v| v.pointer("/error/code")).and_then(Value::as_i64) == Some(4) {
                tracing::warn!("deezer quota exceeded, waiting");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            return check(v);
        }
    }

    /// Follows `next` links of a paginated list.
    async fn paged(&self, path: &str, max: usize) -> Result<Vec<Value>> {
        let mut out = vec![];
        let mut index = 0usize;
        loop {
            let page = self.get(path, &[("limit", "100".into()), ("index", index.to_string())]).await?;
            let Some(page) = page else { break };
            let data = page.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
            let n = data.len();
            out.extend(data);
            index += n;
            if n == 0 || page.get("next").is_none() || out.len() >= max {
                break;
            }
        }
        Ok(out)
    }

    pub async fn artist(&self, id: &str) -> Result<Option<SrcArtist>> {
        Ok(self.get(&format!("artist/{id}"), &[]).await?.as_ref().and_then(artist_from))
    }

    /// Discography summary (no tracks, no UPC).
    pub async fn artist_albums(&self, id: &str) -> Result<Vec<SrcAlbum>> {
        let items = self.paged(&format!("artist/{id}/albums"), 2000).await?;
        Ok(items.iter().filter_map(album_from).collect())
    }

    /// Full album with tracks (disc/position/ISRC come from `/album/{id}/tracks`).
    pub async fn album(&self, id: &str) -> Result<Option<SrcAlbum>> {
        let Some(v) = self.get(&format!("album/{id}"), &[]).await? else { return Ok(None) };
        self.complete_album(v).await
    }

    pub async fn album_by_upc(&self, upc: &str) -> Result<Option<SrcAlbum>> {
        let Some(v) = self.get(&format!("album/upc:{upc}"), &[]).await? else { return Ok(None) };
        self.complete_album(v).await
    }

    async fn complete_album(&self, v: Value) -> Result<Option<SrcAlbum>> {
        let Some(mut album) = album_from(&v) else { return Ok(None) };
        let tracks = self.paged(&format!("album/{}/tracks", album.id), 1000).await?;
        let tracks = if tracks.is_empty() {
            v.pointer("/tracks/data").and_then(Value::as_array).cloned().unwrap_or_default()
        } else {
            tracks
        };
        let mut list: Vec<SrcTrack> =
            tracks.iter().enumerate().filter_map(|(n, t)| track_from(t, n as i64 + 1)).collect();
        list.sort_by_key(|t| (t.disc, t.position));
        album.tracks = Some(list);
        Ok(Some(album))
    }

    pub async fn search_artists(&self, q: &str, limit: usize) -> Result<Vec<SrcArtist>> {
        let v = self.get("search/artist", &[("q", q.to_string()), ("limit", limit.to_string())]).await?;
        Ok(data(&v).iter().filter_map(artist_from).collect())
    }

    pub async fn search_albums(&self, q: &str, artist: Option<&str>, limit: usize) -> Result<Vec<SrcAlbum>> {
        let query = match artist.filter(|a| !a.trim().is_empty()) {
            Some(a) => format!("artist:\"{}\" album:\"{}\"", a.replace('"', ""), q.replace('"', "")),
            None => q.to_string(),
        };
        let v = self.get("search/album", &[("q", query), ("limit", limit.to_string())]).await?;
        Ok(data(&v).iter().filter_map(album_from).collect())
    }
}

fn data(v: &Option<Value>) -> Vec<Value> {
    v.as_ref().and_then(|v| v.get("data")).and_then(Value::as_array).cloned().unwrap_or_default()
}
