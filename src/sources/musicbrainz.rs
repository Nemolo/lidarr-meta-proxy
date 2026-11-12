//! MusicBrainz WS2, used for reconciliation and for Deezer/Spotify URL relations.
//! Requires a descriptive User-Agent and at most 1 request per second.

use anyhow::Result;
use serde_json::Value;

use crate::{http::SourceClient, ids::normalize_barcode};

#[derive(Clone)]
pub struct MusicBrainz {
    client: SourceClient,
    base: String,
}

const TTL_FOUND: i64 = 7 * 86400;

/// Entities attached to a URL in MusicBrainz.
#[derive(Debug, Default, Clone)]
pub struct UrlRels {
    pub artists: Vec<String>,
    pub releases: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MbRelease {
    pub id: String,
    pub title: String,
    pub barcode: Option<String>,
    pub date: Option<String>,
    pub release_group: Option<String>,
    /// Artist credit MBIDs, in order.
    pub artists: Vec<String>,
    pub media: Vec<MbMedium>,
}

#[derive(Debug, Clone)]
pub struct MbMedium {
    pub position: i64,
    pub tracks: Vec<MbTrack>,
}

#[derive(Debug, Clone)]
pub struct MbTrack {
    pub id: String,
    pub position: i64,
    pub recording: String,
    pub isrcs: Vec<String>,
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn release_from(v: &Value) -> Option<MbRelease> {
    Some(MbRelease {
        id: s(v, "id")?,
        title: s(v, "title").unwrap_or_default(),
        barcode: s(v, "barcode"),
        date: s(v, "date"),
        release_group: v.get("release-group").and_then(|g| s(g, "id")),
        artists: v
            .get("artist-credit")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|c| c.get("artist").and_then(|x| s(x, "id"))).collect())
            .unwrap_or_default(),
        media: v
            .get("media")
            .and_then(Value::as_array)
            .map(|m| {
                m.iter()
                    .map(|m| MbMedium {
                        position: m.get("position").and_then(Value::as_i64).unwrap_or(1),
                        tracks: m
                            .get("tracks")
                            .and_then(Value::as_array)
                            .map(|t| {
                                t.iter()
                                    .filter_map(|t| {
                                        let rec = t.get("recording")?;
                                        Some(MbTrack {
                                            id: s(t, "id")?,
                                            position: t.get("position").and_then(Value::as_i64).unwrap_or(0),
                                            recording: s(rec, "id")?,
                                            isrcs: rec
                                                .get("isrcs")
                                                .and_then(Value::as_array)
                                                .map(|i| {
                                                    i.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()
                                                })
                                                .unwrap_or_default(),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

impl MusicBrainz {
    pub fn new(client: SourceClient, base: String) -> Self {
        Self { client, base }
    }

    pub fn is_paused(&self) -> bool {
        self.client.is_paused()
    }

    /// Reachability as seen by the circuit breaker (`ok`, `paused (…)`, ...).
    pub fn status(&self) -> String {
        self.client.status().unwrap_or_else(|| "ok".into())
    }

    async fn get(&self, key: &str, path: &str, query: Vec<(&'static str, String)>, ttl: i64) -> Result<Option<Value>> {
        let url = format!("{}/{}", self.base, path);
        let mut q = query;
        q.push(("fmt", "json".into()));
        self.client.cached_json(key, ttl, |c| c.get(&url).query(&q)).await
    }

    /// `/url?resource=<url>&inc=artist-rels+release-rels`.
    pub async fn url_rels(&self, resource: &str, ttl_missing: i64) -> Result<UrlRels> {
        let key = format!("url:{resource}");
        // Not-found results are kept for a shorter time so new MB edits are picked up.
        let v = self
            .get(
                &key,
                "url",
                vec![("resource", resource.to_string()), ("inc", "artist-rels+release-rels".into())],
                ttl_missing,
            )
            .await?;
        let mut out = UrlRels::default();
        if let Some(rels) = v.as_ref().and_then(|v| v.get("relations")).and_then(Value::as_array) {
            for r in rels {
                if let Some(id) = r.get("artist").and_then(|a| s(a, "id")) {
                    out.artists.push(id);
                }
                if let Some(id) = r.get("release").and_then(|a| s(a, "id")) {
                    out.releases.push(id);
                }
            }
        }
        Ok(out)
    }

    /// URL relations of an artist (Deezer, Spotify, ... links).
    pub async fn artist_urls(&self, mbid: &str) -> Result<Vec<String>> {
        let v = self
            .get(&format!("artist-urls:{mbid}"), &format!("artist/{mbid}"), vec![("inc", "url-rels".into())], TTL_FOUND)
            .await?;
        Ok(v.as_ref()
            .and_then(|v| v.get("relations"))
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(|r| r.get("url").and_then(|u| s(u, "resource"))).collect())
            .unwrap_or_default())
    }

    /// All releases of an artist (barcode + release group), browsed 100 at a time.
    pub async fn artist_releases(&self, mbid: &str) -> Result<Vec<MbRelease>> {
        let mut out = vec![];
        let mut offset = 0usize;
        loop {
            let v = self
                .get(
                    &format!("artist-releases:{mbid}:{offset}"),
                    "release",
                    vec![
                        ("artist", mbid.to_string()),
                        ("inc", "release-groups".into()),
                        ("limit", "100".into()),
                        ("offset", offset.to_string()),
                    ],
                    TTL_FOUND,
                )
                .await?;
            let Some(v) = v else { break };
            let list = v.get("releases").and_then(Value::as_array).cloned().unwrap_or_default();
            let total = v.get("release-count").and_then(Value::as_u64).unwrap_or(0) as usize;
            out.extend(list.iter().filter_map(release_from));
            offset += list.len();
            if list.is_empty() || offset >= total || offset >= 2000 {
                break;
            }
        }
        Ok(out)
    }

    /// Releases of a release group (for barcodes).
    pub async fn release_group_releases(&self, rgid: &str) -> Result<Vec<MbRelease>> {
        let v = self
            .get(
                &format!("rg-releases:{rgid}"),
                "release",
                vec![("release-group", rgid.to_string()), ("limit", "100".into())],
                TTL_FOUND,
            )
            .await?;
        Ok(v.as_ref()
            .and_then(|v| v.get("releases"))
            .and_then(Value::as_array)
            .map(|l| l.iter().filter_map(release_from).collect())
            .unwrap_or_default())
    }

    /// Release with release group, artist credits, tracks and ISRCs.
    pub async fn release(&self, id: &str) -> Result<Option<MbRelease>> {
        let v = self
            .get(
                &format!("release:{id}"),
                &format!("release/{id}"),
                vec![("inc", "release-groups+artist-credits+recordings+isrcs".into())],
                TTL_FOUND,
            )
            .await?;
        Ok(v.as_ref().and_then(release_from))
    }

    pub async fn releases_by_barcode(&self, barcode: &str, ttl_missing: i64) -> Result<Vec<String>> {
        let Some(norm) = normalize_barcode(barcode) else { return Ok(vec![]) };
        let v = self
            .get(
                &format!("barcode:{norm}"),
                "release",
                vec![("query", format!("barcode:{barcode}")), ("limit", "10".into())],
                ttl_missing,
            )
            .await?;
        Ok(v.as_ref()
            .and_then(|v| v.get("releases"))
            .and_then(Value::as_array)
            .map(|l| {
                l.iter()
                    .filter(|r| {
                        r.get("barcode").and_then(Value::as_str).and_then(normalize_barcode).as_deref()
                            == Some(norm.as_str())
                    })
                    .filter_map(|r| s(r, "id"))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Release IDs containing a recording with this ISRC.
    pub async fn releases_by_isrc(&self, isrc: &str, ttl_missing: i64) -> Result<Vec<String>> {
        let v = self
            .get(&format!("isrc:{isrc}"), &format!("isrc/{isrc}"), vec![("inc", "releases".into())], ttl_missing)
            .await?;
        let mut out = vec![];
        if let Some(recs) = v.as_ref().and_then(|v| v.get("recordings")).and_then(Value::as_array) {
            for r in recs {
                if let Some(rels) = r.get("releases").and_then(Value::as_array) {
                    out.extend(rels.iter().filter_map(|x| s(x, "id")));
                }
            }
        }
        out.dedup();
        Ok(out)
    }
}
