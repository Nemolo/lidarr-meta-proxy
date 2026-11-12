//! SQLite persistence (WAL mode, migrations embedded at build time).

use std::{path::Path, str::FromStr, time::Duration};

use anyhow::Result;
use serde::{Serialize, de::DeserializeOwned};
use sqlx::{
    ConnectOptions, Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous},
};

use crate::ids::{Kind, Source, now};

#[derive(Clone)]
pub struct Store {
    pub pool: SqlitePool,
}

#[derive(Clone, Debug)]
pub struct EntityRow {
    pub id: String,
    pub kind: String,
    pub source: String,
    pub source_id: String,
    pub parent_id: Option<String>,
    pub mbid: Option<String>,
    pub payload: String,
    pub fetched_at: i64,
    pub updated_at: i64,
}

impl EntityRow {
    fn from_row(r: &SqliteRow) -> Self {
        Self {
            id: r.get("id"),
            kind: r.get("kind"),
            source: r.get("source"),
            source_id: r.get("source_id"),
            parent_id: r.get("parent_id"),
            mbid: r.get("mbid"),
            payload: r.get("payload"),
            fetched_at: r.get("fetched_at"),
            updated_at: r.get("updated_at"),
        }
    }

    pub fn payload<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_str(&self.payload)?)
    }

    pub fn source(&self) -> Option<Source> {
        Source::parse(&self.source)
    }
}

#[derive(Clone, Debug)]
pub struct ImageRow {
    pub hash: String,
    pub origin_url: String,
    pub path: String,
    pub width: i64,
    pub height: i64,
    pub mime: String,
}

impl ImageRow {
    fn from_row(r: &SqliteRow) -> Self {
        Self {
            hash: r.get("hash"),
            origin_url: r.get("origin_url"),
            path: r.get("path"),
            width: r.get("width"),
            height: r.get("height"),
            mime: r.get("mime"),
        }
    }
}

pub struct CachedResponse {
    pub body: Vec<u8>,
    pub status: u16,
    pub fetched_at: i64,
    pub etag: Option<String>,
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)?;
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10))
            // Writes can wait behind a WAL checkpoint on slow disks: only flag really slow ones.
            .log_slow_statements(log::LevelFilter::Warn, Duration::from_secs(5));
        let pool = SqlitePoolOptions::new().max_connections(8).connect_with(opts).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    /// Records the UUID namespace on first use and refuses to start with a different
    /// one: every synthetic ID in the database (and in Lidarr) derives from it.
    pub async fn check_namespace(&self, ns: &str) -> Result<()> {
        let stored: Option<(String,)> =
            sqlx::query_as("SELECT value FROM meta WHERE key = 'uuid_namespace'").fetch_optional(&self.pool).await?;
        match stored {
            Some((v,)) if v == ns => Ok(()),
            Some((v,)) => anyhow::bail!(
                "UUID_NAMESPACE mismatch: the database was created with namespace {v}, configured {ns}. \
                 Changing it would change every synthetic ID known to Lidarr; restore the previous value \
                 or start from an empty database."
            ),
            None => {
                // Databases created before this check used the default namespace.
                let (entities,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM entity").fetch_one(&self.pool).await?;
                let default = crate::ids::DEFAULT_NAMESPACE.to_string();
                if entities > 0 && ns != default {
                    anyhow::bail!(
                        "UUID_NAMESPACE mismatch: the database already holds entities created with the default \
                         namespace {default}, configured {ns}"
                    );
                }
                let value = if entities > 0 { default } else { ns.to_string() };
                sqlx::query("INSERT INTO meta (key, value) VALUES ('uuid_namespace', ?)")
                    .bind(value)
                    .execute(&self.pool)
                    .await?;
                Ok(())
            }
        }
    }

    pub async fn ping(&self) -> bool {
        sqlx::query("SELECT 1").fetch_one(&self.pool).await.is_ok()
    }

    // ---------------------------------------------------------------- entities

    /// Inserts or refreshes an entity. `updated_at` only moves when the payload
    /// actually changes, so `/recent/*` reports real changes. Returns true if changed.
    pub async fn upsert_entity<T: Serialize>(
        &self,
        id: &str,
        kind: Kind,
        source: Source,
        source_id: &str,
        parent_id: Option<&str>,
        payload: &T,
    ) -> Result<bool> {
        let payload = serde_json::to_string(payload)?;
        let ts = now();
        let existing: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT payload, parent_id FROM entity WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        match existing {
            None => {
                sqlx::query(
                    "INSERT INTO entity (id, kind, source, source_id, parent_id, payload, fetched_at, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT (id) DO NOTHING",
                )
                .bind(id)
                .bind(kind.as_str())
                .bind(source.as_str())
                .bind(source_id)
                .bind(parent_id)
                .bind(&payload)
                .bind(ts)
                .bind(ts)
                .execute(&self.pool)
                .await?;
                self.add_link(id, source.as_str(), source_id).await?;
                Ok(true)
            }
            Some((old, old_parent)) => {
                // Keep the first owner: an album first seen under an artist stays there.
                let parent = old_parent.as_deref().or(parent_id);
                if old == payload {
                    sqlx::query("UPDATE entity SET fetched_at = ?, parent_id = ? WHERE id = ?")
                        .bind(ts)
                        .bind(parent)
                        .bind(id)
                        .execute(&self.pool)
                        .await?;
                    Ok(false)
                } else {
                    sqlx::query(
                        "UPDATE entity SET payload = ?, fetched_at = ?, updated_at = ?, parent_id = ? WHERE id = ?",
                    )
                    .bind(&payload)
                    .bind(ts)
                    .bind(ts)
                    .bind(parent)
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
                    Ok(true)
                }
            }
        }
    }

    pub async fn get_entity(&self, id: &str) -> Result<Option<EntityRow>> {
        let row = sqlx::query("SELECT id, kind, source, source_id, parent_id, mbid, payload, fetched_at, updated_at FROM entity WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(EntityRow::from_row))
    }

    pub async fn touch(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE entity SET updated_at = ? WHERE id = ?").bind(now()).bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn add_link(&self, entity_id: &str, source: &str, source_id: &str) -> Result<()> {
        if source_id.is_empty() {
            return Ok(());
        }
        sqlx::query("INSERT OR IGNORE INTO source_link (entity_id, source, source_id) VALUES (?, ?, ?)")
            .bind(entity_id)
            .bind(source)
            .bind(source_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Entities of `kind` carrying the external identifier `source:source_id`.
    pub async fn find_by_link(&self, source: &str, source_id: &str, kind: Kind) -> Result<Vec<EntityRow>> {
        let rows = sqlx::query(
            "SELECT e.id, e.kind, e.source, e.source_id, e.parent_id, e.mbid, e.payload, e.fetched_at, e.updated_at
             FROM source_link l JOIN entity e ON e.id = l.entity_id
             WHERE l.source = ? AND l.source_id = ? AND e.kind = ?
             ORDER BY CASE e.source WHEN 'deezer' THEN 0 ELSE 1 END, e.id",
        )
        .bind(source)
        .bind(source_id)
        .bind(kind.as_str())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(EntityRow::from_row).collect())
    }

    pub async fn links_of(&self, entity_id: &str) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as("SELECT source, source_id FROM source_link WHERE entity_id = ?")
            .bind(entity_id)
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn children(&self, parent_id: &str, kind: Kind) -> Result<Vec<EntityRow>> {
        let rows = sqlx::query(
            "SELECT id, kind, source, source_id, parent_id, mbid, payload, fetched_at, updated_at
             FROM entity WHERE parent_id = ? AND kind = ? ORDER BY id",
        )
        .bind(parent_id)
        .bind(kind.as_str())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(EntityRow::from_row).collect())
    }

    /// Synthetic IDs reconciled to `mbid` (used to fill `oldIds`).
    pub async fn ids_for_mbid(&self, mbid: &str, kind: Kind) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT id FROM entity WHERE mbid = ? AND kind = ? ORDER BY id")
            .bind(mbid)
            .bind(kind.as_str())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// Marks an entity as reconciled. Bumps `updated_at` so Lidarr refreshes it.
    pub async fn set_mbid(&self, id: &str, kind: Kind, mbid: &str) -> Result<()> {
        let ts = now();
        let res =
            sqlx::query("UPDATE entity SET mbid = ?, updated_at = ? WHERE id = ? AND (mbid IS NULL OR mbid <> ?)")
                .bind(mbid)
                .bind(ts)
                .bind(id)
                .bind(mbid)
                .execute(&self.pool)
                .await?;
        if res.rows_affected() > 0 {
            sqlx::query("INSERT INTO reconcile_log (entity_id, mbid, kind, reconciled_at) VALUES (?, ?, ?, ?)")
                .bind(id)
                .bind(mbid)
                .bind(kind.as_str())
                .bind(ts)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn unreconciled(&self, kind: Kind, limit: i64) -> Result<Vec<EntityRow>> {
        let rows = sqlx::query(
            "SELECT id, kind, source, source_id, parent_id, mbid, payload, fetched_at, updated_at
             FROM entity WHERE kind = ? AND mbid IS NULL ORDER BY fetched_at LIMIT ?",
        )
        .bind(kind.as_str())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(EntityRow::from_row).collect())
    }

    pub async fn all_of_kind(&self, kind: Kind) -> Result<Vec<EntityRow>> {
        let rows = sqlx::query(
            "SELECT id, kind, source, source_id, parent_id, mbid, payload, fetched_at, updated_at FROM entity WHERE kind = ?",
        )
        .bind(kind.as_str())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(EntityRow::from_row).collect())
    }

    /// Synthetic IDs of `kind` changed after `since`.
    pub async fn recent(&self, kind: Kind, since: i64) -> Result<Vec<String>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT id FROM entity WHERE kind = ? AND updated_at > ? ORDER BY updated_at")
                .bind(kind.as_str())
                .bind(since)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    // ------------------------------------------------------------- enrichment

    /// Records the set of synthetic albums added to an MB artist; returns true if it changed.
    pub async fn record_enrichment(&self, artist_mbid: &str, albums_hash: &str) -> Result<bool> {
        let ts = now();
        let old: Option<(String,)> = sqlx::query_as("SELECT albums_hash FROM enrichment WHERE artist_mbid = ?")
            .bind(artist_mbid)
            .fetch_optional(&self.pool)
            .await?;
        let changed = old.as_ref().map(|o| o.0 != albums_hash).unwrap_or(true);
        sqlx::query(
            "INSERT INTO enrichment (artist_mbid, albums_hash, checked_at, updated_at) VALUES (?, ?, ?, ?)
             ON CONFLICT (artist_mbid) DO UPDATE SET albums_hash = excluded.albums_hash, checked_at = excluded.checked_at,
               updated_at = CASE WHEN enrichment.albums_hash = excluded.albums_hash THEN enrichment.updated_at ELSE excluded.updated_at END",
        )
        .bind(artist_mbid)
        .bind(albums_hash)
        .bind(ts)
        .bind(ts)
        .execute(&self.pool)
        .await?;
        // A first enrichment with no synthetic album is not a change Lidarr must see.
        Ok(changed && !(old.is_none() && albums_hash.is_empty()))
    }

    pub async fn recent_enriched_artists(&self, since: i64) -> Result<Vec<String>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT artist_mbid FROM enrichment WHERE updated_at > ? AND albums_hash <> ''")
                .bind(since)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    // ------------------------------------------------------------------ caches

    pub async fn upstream_get(&self, route: &str) -> Result<Option<CachedResponse>> {
        let row = sqlx::query("SELECT body, status, fetched_at, etag, encoding FROM upstream_cache WHERE route = ?")
            .bind(route)
            .fetch_optional(&self.pool)
            .await?;
        let Some(r) = row else { return Ok(None) };
        let raw: Vec<u8> = r.get("body");
        let body = match r.get::<Option<String>, _>("encoding").as_deref() {
            Some("gzip") => gunzip(&raw)?,
            _ => raw,
        };
        Ok(Some(CachedResponse {
            body,
            status: r.get::<i64, _>("status") as u16,
            fetched_at: r.get("fetched_at"),
            etag: r.get("etag"),
        }))
    }

    pub async fn upstream_put(&self, route: &str, body: &[u8], status: u16, etag: Option<&str>) -> Result<()> {
        sqlx::query(
            "INSERT INTO upstream_cache (route, body, status, fetched_at, etag, encoding) VALUES (?, ?, ?, ?, ?, 'gzip')
             ON CONFLICT (route) DO UPDATE SET body = excluded.body, status = excluded.status,
               fetched_at = excluded.fetched_at, etag = excluded.etag, encoding = excluded.encoding",
        )
        .bind(route)
        .bind(gzip(body)?)
        .bind(status as i64)
        .bind(now())
        .bind(etag)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Housekeeping: drops cache rows nobody will use again and compacts the WAL.
    /// Search answers are kept 7 days, other upstream answers (stale fallback when
    /// the upstream is down) and third-party JSON 90 days. Returns rows deleted.
    pub async fn maintenance(&self) -> Result<u64> {
        let ts = now();
        let mut deleted = 0;
        deleted += sqlx::query("DELETE FROM upstream_cache WHERE route LIKE 'search%' AND fetched_at < ?")
            .bind(ts - 7 * 86400)
            .execute(&self.pool)
            .await?
            .rows_affected();
        deleted += sqlx::query("DELETE FROM upstream_cache WHERE fetched_at < ?")
            .bind(ts - 90 * 86400)
            .execute(&self.pool)
            .await?
            .rows_affected();
        deleted += sqlx::query("DELETE FROM source_cache WHERE fetched_at < ?")
            .bind(ts - 90 * 86400)
            .execute(&self.pool)
            .await?
            .rows_affected();
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&self.pool).await?;
        sqlx::query("PRAGMA optimize").execute(&self.pool).await?;
        Ok(deleted)
    }

    pub async fn upstream_touch(&self, route: &str) -> Result<()> {
        sqlx::query("UPDATE upstream_cache SET fetched_at = ? WHERE route = ?")
            .bind(now())
            .bind(route)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Cached third-party JSON if younger than `ttl` seconds.
    pub async fn cache_get(&self, key: &str, ttl: i64) -> Result<Option<String>> {
        let row: Option<(String, i64)> = sqlx::query_as("SELECT body, fetched_at FROM source_cache WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.filter(|(_, at)| now() - at < ttl).map(|(b, _)| b))
    }

    pub async fn cache_put(&self, key: &str, body: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO source_cache (key, body, fetched_at) VALUES (?, ?, ?)
             ON CONFLICT (key) DO UPDATE SET body = excluded.body, fetched_at = excluded.fetched_at",
        )
        .bind(key)
        .bind(body)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ------------------------------------------------------------------ images

    pub async fn image(&self, hash: &str) -> Result<Option<ImageRow>> {
        let row = sqlx::query("SELECT hash, origin_url, path, width, height, mime FROM image WHERE hash = ?")
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(ImageRow::from_row))
    }

    pub async fn image_by_origin(&self, origin_url: &str) -> Result<Option<ImageRow>> {
        let row =
            sqlx::query("SELECT hash, origin_url, path, width, height, mime FROM image WHERE origin_url = ? LIMIT 1")
                .bind(origin_url)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.as_ref().map(ImageRow::from_row))
    }

    pub async fn put_image(&self, img: &ImageRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO image (hash, origin_url, path, width, height, mime, fetched_at) VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (hash) DO NOTHING",
        )
        .bind(&img.hash)
        .bind(&img.origin_url)
        .bind(&img.path)
        .bind(img.width)
        .bind(img.height)
        .bind(&img.mime)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn entity_image(&self, entity_id: &str, cover_type: &str) -> Result<Option<ImageRow>> {
        let row = sqlx::query(
            "SELECT i.hash, i.origin_url, i.path, i.width, i.height, i.mime
             FROM entity_image ei JOIN image i ON i.hash = ei.hash
             WHERE ei.entity_id = ? AND ei.cover_type = ?",
        )
        .bind(entity_id)
        .bind(cover_type)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(ImageRow::from_row))
    }

    pub async fn set_entity_image(&self, entity_id: &str, cover_type: &str, hash: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO entity_image (entity_id, cover_type, hash) VALUES (?, ?, ?)
             ON CONFLICT (entity_id, cover_type) DO UPDATE SET hash = excluded.hash",
        )
        .bind(entity_id)
        .bind(cover_type)
        .bind(hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(data.len() / 4), flate2::Compression::fast());
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::with_capacity(data.len() * 4);
    flate2::read::GzDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}
