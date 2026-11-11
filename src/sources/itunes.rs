//! iTunes Search API, used only to find high resolution album artwork.

use anyhow::Result;
use serde_json::Value;

use crate::{http::SourceClient, ids::normalize_title};

#[derive(Clone)]
pub struct Itunes {
    client: SourceClient,
    base: String,
    country: String,
    size: u32,
}

const TTL: i64 = 30 * 86400;

impl Itunes {
    pub fn new(client: SourceClient, base: String, country: String, size: u32) -> Self {
        Self { client, base, country, size }
    }

    /// `artworkUrl100` ends with `/100x100bb.jpg`; the CDN renders any requested size
    /// (capped at the master's resolution).
    pub fn resize(&self, url: &str) -> String {
        match url.rfind('/') {
            Some(i) if url[i..].contains("100x100") => {
                format!("{}/{s}x{s}bb.jpg", &url[..i], s = self.size)
            }
            _ => url.to_string(),
        }
    }

    fn artwork(&self, v: &Value) -> Option<String> {
        v.get("artworkUrl100").and_then(Value::as_str).map(|u| self.resize(u))
    }

    pub async fn artwork_by_upc(&self, upc: &str) -> Result<Option<String>> {
        let url = format!("{}/lookup", self.base);
        let q = [("upc", upc.to_string()), ("entity", "album".into()), ("country", self.country.clone())];
        let v = self.client.cached_json(&format!("upc:{}:{upc}", self.country), TTL, |c| c.get(&url).query(&q)).await?;
        Ok(v.as_ref()
            .and_then(|v| v.get("results"))
            .and_then(Value::as_array)
            .and_then(|r| {
                r.iter().find(|x| x.get("wrapperType").and_then(Value::as_str) == Some("collection")).or(r.first())
            })
            .and_then(|r| self.artwork(r)))
    }

    /// Exact (normalized) artist + title match only.
    pub async fn artwork_by_name(&self, artist: &str, title: &str) -> Result<Option<String>> {
        let url = format!("{}/search", self.base);
        let q = [
            ("term", format!("{artist} {title}")),
            ("entity", "album".into()),
            ("country", self.country.clone()),
            ("limit", "10".into()),
        ];
        let key = format!("search:{}:{}|{}", self.country, normalize_title(artist), normalize_title(title));
        let v = self.client.cached_json(&key, TTL, |c| c.get(&url).query(&q)).await?;
        let (na, nt) = (normalize_title(artist), normalize_title(title));
        Ok(v.as_ref()
            .and_then(|v| v.get("results"))
            .and_then(Value::as_array)
            .and_then(|r| {
                r.iter().find(|x| {
                    let a = x.get("artistName").and_then(Value::as_str).map(normalize_title);
                    let t = x.get("collectionName").and_then(Value::as_str).map(normalize_title);
                    a.as_deref() == Some(na.as_str()) && t.as_deref() == Some(nt.as_str())
                })
            })
            .and_then(|r| self.artwork(r)))
    }
}
