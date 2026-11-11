//! Image cache: downloads artwork once, stores it under `IMAGE_DIR` named by the
//! SHA-256 of its content, and exposes it as `{PUBLIC_URL}/img/{sha256}.{ext}`.

use std::{path::PathBuf, time::Duration};

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

use crate::{
    model::ImageResource,
    store::{ImageRow, Store},
};

#[derive(Clone)]
pub struct Images {
    store: Store,
    http: reqwest::Client,
    dir: PathBuf,
    public_url: String,
}

const MAX_BYTES: usize = 25 * 1024 * 1024;
/// How long a failed lookup for an entity image is remembered.
const FAIL_TTL: i64 = 86400;

fn ext_for(t: imagesize::ImageType) -> Option<(&'static str, &'static str)> {
    use imagesize::ImageType::*;
    Some(match t {
        Jpeg => ("jpg", "image/jpeg"),
        Png => ("png", "image/png"),
        Webp => ("webp", "image/webp"),
        Gif => ("gif", "image/gif"),
        _ => return None,
    })
}

impl Images {
    pub fn new(store: Store, dir: PathBuf, public_url: String, user_agent: &str) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(Duration::from_secs(30))
            .build()
            .expect("http client");
        Self { store, http, dir, public_url }
    }

    pub fn url_for(&self, row: &ImageRow) -> String {
        let ext = row.path.rsplit('.').next().unwrap_or("jpg");
        format!("{}/img/{}.{}", self.public_url, row.hash, ext)
    }

    pub fn resource(&self, cover_type: &str, row: &ImageRow) -> ImageResource {
        ImageResource {
            cover_type: cover_type.to_string(),
            url: self.url_for(row),
            width: row.width,
            height: row.height,
            extra: Default::default(),
        }
    }

    /// Absolute path of a stored image.
    pub fn file_path(&self, row: &ImageRow) -> PathBuf {
        self.dir.join(&row.path)
    }

    /// Downloads `url` (or reuses an earlier download of the same URL).
    pub async fn fetch(&self, url: &str) -> Result<ImageRow> {
        if let Some(row) = self.store.image_by_origin(url).await?
            && self.file_path(&row).exists()
        {
            return Ok(row);
        }
        let resp = self.http.get(url).send().await?.error_for_status()?;
        if resp.content_length().map(|l| l as usize > MAX_BYTES).unwrap_or(false) {
            bail!("image too large");
        }
        let bytes = resp.bytes().await?;
        if bytes.len() > MAX_BYTES || bytes.is_empty() {
            bail!("invalid image size");
        }
        let kind = imagesize::image_type(&bytes)?;
        let Some((ext, mime)) = ext_for(kind) else { bail!("unsupported image type {kind:?}") };
        let size = imagesize::blob_size(&bytes)?;
        let hash = hex::encode(Sha256::digest(&bytes));
        let rel = format!("{}/{}.{}", &hash[..2], hash, ext);
        let path = self.dir.join(&rel);
        if !path.exists() {
            tokio::fs::create_dir_all(path.parent().unwrap()).await?;
            let tmp = path.with_extension("part");
            tokio::fs::write(&tmp, &bytes).await?;
            tokio::fs::rename(&tmp, &path).await?;
        }
        let row = ImageRow {
            hash: hash.clone(),
            origin_url: url.to_string(),
            path: rel,
            width: size.width as i64,
            height: size.height as i64,
            mime: mime.to_string(),
        };
        // Same content from another URL: the existing row (keyed by hash) is kept.
        self.store.put_image(&row).await?;
        Ok(self.store.image(&hash).await?.unwrap_or(row))
    }

    /// Image already assigned to an entity, if any.
    pub async fn cached(&self, entity_id: &str, cover_type: &str) -> Option<ImageResource> {
        let row = self.store.entity_image(entity_id, cover_type).await.ok().flatten()?;
        self.file_path(&row).exists().then(|| self.resource(cover_type, &row))
    }

    /// Whether a recent attempt to find an image for this entity failed.
    pub async fn recently_failed(&self, entity_id: &str, cover_type: &str) -> bool {
        self.store.cache_get(&format!("imgfail:{entity_id}:{cover_type}"), FAIL_TTL).await.ok().flatten().is_some()
    }

    /// Tries `candidates` in order, assigns the first that downloads to the entity.
    pub async fn assign(&self, entity_id: &str, cover_type: &str, candidates: &[String]) -> Option<ImageResource> {
        for url in candidates {
            match self.fetch(url).await {
                Ok(row) => {
                    if let Err(e) = self.store.set_entity_image(entity_id, cover_type, &row.hash).await {
                        tracing::warn!(error = %e, "failed to store entity image");
                    }
                    return Some(self.resource(cover_type, &row));
                }
                Err(e) => tracing::debug!(%url, error = %e, "image download failed"),
            }
        }
        let _ = self.store.cache_put(&format!("imgfail:{entity_id}:{cover_type}"), "1").await;
        None
    }
}
