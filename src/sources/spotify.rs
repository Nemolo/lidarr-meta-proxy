//! Spotify Web API with the client-credentials flow (secondary source).
//!
//! Apps in development mode have a reduced API surface; every call degrades to
//! "no data" on 403 so the proxy keeps working with Deezer only.

use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;
use tokio::sync::Mutex;

use super::{SrcAlbum, SrcArtist, SrcImage, SrcTrack};
use crate::{
    http::{FetchError, SourceClient},
    ids::{Source, normalize_date},
};

pub struct Spotify {
    client: SourceClient,
    base: String,
    auth_url: String,
    client_id: String,
    client_secret: String,
    token: Mutex<Option<(String, Instant)>>,
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn images(v: &Value) -> Vec<SrcImage> {
    let mut list: Vec<SrcImage> = v
        .get("images")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|i| {
                    Some(SrcImage {
                        url: s(i, "url")?,
                        width: i.get("width").and_then(Value::as_i64).unwrap_or(0),
                        height: i.get("height").and_then(Value::as_i64).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    list.sort_by_key(|i| -(i.width * i.height));
    list
}

pub fn artist_from(v: &Value) -> Option<SrcArtist> {
    let id = s(v, "id")?;
    Some(SrcArtist {
        source: Source::Spotify,
        name: s(v, "name")?,
        link: format!("https://open.spotify.com/artist/{id}"),
        genres: v
            .get("genres")
            .and_then(Value::as_array)
            .map(|g| g.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
        images: images(v),
        id,
    })
}

pub fn album_from(v: &Value) -> Option<SrcAlbum> {
    let id = s(v, "id")?;
    let artist = v.get("artists").and_then(Value::as_array).and_then(|a| a.first());
    Some(SrcAlbum {
        source: Source::Spotify,
        title: s(v, "name")?,
        record_type: s(v, "album_type").unwrap_or_else(|| "album".into()),
        release_date: s(v, "release_date").and_then(|d| normalize_date(&d)),
        upc: v.pointer("/external_ids/upc").and_then(Value::as_str).map(str::to_string),
        label: s(v, "label"),
        genres: vec![],
        explicit: false,
        link: format!("https://open.spotify.com/album/{id}"),
        artist_id: artist.and_then(|a| s(a, "id")),
        artist_name: artist.and_then(|a| s(a, "name")),
        images: images(v),
        tracks: None,
        id,
    })
}

fn track_from(v: &Value) -> Option<SrcTrack> {
    Some(SrcTrack {
        id: s(v, "id")?,
        title: s(v, "name")?,
        disc: v.get("disc_number").and_then(Value::as_i64).unwrap_or(1).max(1),
        position: v.get("track_number").and_then(Value::as_i64).unwrap_or(1),
        duration_ms: v.get("duration_ms").and_then(Value::as_i64).unwrap_or(0),
        isrc: v.pointer("/external_ids/isrc").and_then(Value::as_str).map(str::to_string),
        explicit: v.get("explicit").and_then(Value::as_bool).unwrap_or(false),
        artist_id: v.get("artists").and_then(Value::as_array).and_then(|a| a.first()).and_then(|a| s(a, "id")),
    })
}

impl Spotify {
    pub fn new(client: SourceClient, base: String, auth_url: String, client_id: String, client_secret: String) -> Self {
        Self { client, base, auth_url, client_id, client_secret, token: Mutex::new(None) }
    }

    async fn token(&self) -> Result<String> {
        let mut guard = self.token.lock().await;
        if let Some((t, exp)) = guard.as_ref()
            && Instant::now() < *exp
        {
            return Ok(t.clone());
        }
        let resp = self
            .client
            .http
            .post(&self.auth_url)
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await?
            .error_for_status()?;
        let v: Value = resp.json().await?;
        let token = s(&v, "access_token").ok_or_else(|| anyhow::anyhow!("spotify: no access_token"))?;
        let ttl = v.get("expires_in").and_then(Value::as_u64).unwrap_or(3600);
        *guard = Some((token.clone(), Instant::now() + Duration::from_secs(ttl.saturating_sub(60))));
        Ok(token)
    }

    async fn get(&self, path_or_url: &str, query: &[(&str, String)]) -> Result<Option<Value>> {
        let url = if path_or_url.starts_with("http") {
            path_or_url.to_string()
        } else {
            format!("{}/{}", self.base, path_or_url.trim_start_matches('/'))
        };
        let token = self.token().await?;
        let q = query.to_vec();
        match self.client.json(|c| c.get(&url).bearer_auth(&token).query(&q)).await {
            Err(e) if matches!(e.downcast_ref::<FetchError>(), Some(FetchError::Forbidden(..))) => {
                // Development-mode restriction or revoked credentials: behave as "no data".
                tracing::warn!(%url, error = %e, "spotify request forbidden");
                *self.token.lock().await = None;
                Ok(None)
            }
            other => other,
        }
    }

    pub async fn artist(&self, id: &str) -> Result<Option<SrcArtist>> {
        Ok(self.get(&format!("artists/{id}"), &[]).await?.as_ref().and_then(artist_from))
    }

    pub async fn artist_albums(&self, id: &str) -> Result<Vec<SrcAlbum>> {
        let mut out = vec![];
        let mut next = Some(format!("{}/artists/{id}/albums", self.base));
        let mut first = true;
        while let Some(url) = next.take() {
            let q: Vec<(&str, String)> = if first {
                vec![("include_groups", "album,single,compilation".into()), ("limit", "10".into())]
            } else {
                vec![]
            };
            first = false;
            let Some(page) = self.get(&url, &q).await? else { break };
            if let Some(items) = page.get("items").and_then(Value::as_array) {
                out.extend(items.iter().filter_map(album_from));
            }
            next = s(&page, "next");
            if out.len() > 1000 {
                break;
            }
        }
        Ok(out)
    }

    /// Full album: UPC, label and all tracks (ISRC only when the API provides it).
    pub async fn album(&self, id: &str) -> Result<Option<SrcAlbum>> {
        let Some(v) = self.get(&format!("albums/{id}"), &[]).await? else { return Ok(None) };
        let Some(mut album) = album_from(&v) else { return Ok(None) };
        let mut tracks: Vec<SrcTrack> = v
            .pointer("/tracks/items")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(track_from).collect())
            .unwrap_or_default();
        let mut next = v.pointer("/tracks/next").and_then(Value::as_str).map(str::to_string);
        while let Some(url) = next.take() {
            let Some(page) = self.get(&url, &[]).await? else { break };
            if let Some(items) = page.get("items").and_then(Value::as_array) {
                tracks.extend(items.iter().filter_map(track_from));
            }
            next = s(&page, "next");
        }
        tracks.sort_by_key(|t| (t.disc, t.position));
        if album.record_type == "single" && tracks.len() >= 4 {
            album.record_type = "ep".into();
        }
        album.tracks = Some(tracks);
        Ok(Some(album))
    }

    /// ISRC of a single track (simplified album tracks do not carry it).
    pub async fn track_isrc(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .get(&format!("tracks/{id}"), &[])
            .await?
            .and_then(|v| v.pointer("/external_ids/isrc").and_then(Value::as_str).map(str::to_string)))
    }

    /// Cover URLs (largest first) of the album with this UPC.
    pub async fn album_images_by_upc(&self, upc: &str) -> Result<Vec<String>> {
        let v =
            self.get("search", &[("q", format!("upc:{upc}")), ("type", "album".into()), ("limit", "1".into())]).await?;
        Ok(items(&v, "/albums/items")
            .first()
            .map(|a| images(a).into_iter().map(|i| i.url).collect())
            .unwrap_or_default())
    }

    pub async fn search_artists(&self, q: &str, limit: usize) -> Result<Vec<SrcArtist>> {
        let v = self
            .get("search", &[("q", q.into()), ("type", "artist".into()), ("limit", limit.min(10).to_string())])
            .await?;
        Ok(items(&v, "/artists/items").iter().filter_map(artist_from).collect())
    }

    pub async fn search_albums(&self, q: &str, artist: Option<&str>, limit: usize) -> Result<Vec<SrcAlbum>> {
        let query = match artist.filter(|a| !a.trim().is_empty()) {
            Some(a) => format!("album:{q} artist:{a}"),
            None => q.to_string(),
        };
        let v =
            self.get("search", &[("q", query), ("type", "album".into()), ("limit", limit.min(10).to_string())]).await?;
        Ok(items(&v, "/albums/items").iter().filter_map(album_from).collect())
    }
}

fn items(v: &Option<Value>, ptr: &str) -> Vec<Value> {
    v.as_ref().and_then(|v| v.pointer(ptr)).and_then(Value::as_array).cloned().unwrap_or_default()
}
