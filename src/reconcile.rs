//! Periodic jobs: reconciliation of synthetic entities with MusicBrainz, and
//! image prefetch.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use serde::Deserialize;

use crate::{
    app::App,
    ids::{Kind, synthetic_id},
    sources::{SrcAlbum, SrcArtist, musicbrainz::MbRelease},
    store::EntityRow,
};

#[derive(Debug, Default)]
pub struct ReconcileStats {
    pub artists: usize,
    pub albums: usize,
    pub tracks: usize,
}

#[derive(Deserialize)]
struct TrackPayload {
    disc: i64,
    position: i64,
    isrc: Option<String>,
}

impl App {
    pub async fn reconcile_once(self: &Arc<Self>) -> Result<ReconcileStats> {
        let mut stats = ReconcileStats::default();
        let ttl = self.mb_missing_ttl();

        // 1. Artists by Deezer/Spotify URL relation.
        for row in self.store.unreconciled(Kind::Artist, 1000).await? {
            let Ok(sa) = row.payload::<SrcArtist>() else { continue };
            if sa.link.is_empty() {
                continue;
            }
            let rels = match self.mb.url_rels(&sa.link, ttl).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "MB lookup failed, stopping this run");
                    return Ok(stats);
                }
            };
            if let [mbid] = rels.artists.as_slice()
                && self.upstream_artist(mbid).await.ok().flatten().is_some()
            {
                self.store.set_mbid(&row.id, Kind::Artist, mbid).await?;
                tracing::info!(id = %row.id, %mbid, name = %sa.name, "artist reconciled");
                stats.artists += 1;
            }
        }

        // 2. Albums by URL relation, then barcode, then ISRC.
        for row in self.store.unreconciled(Kind::Album, 2000).await? {
            match self.reconcile_album(&row).await {
                Ok(Some(n)) => {
                    stats.albums += 1;
                    stats.tracks += n;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(id = %row.id, error = %e, "album reconciliation failed, stopping this run");
                    break;
                }
            }
        }
        Ok(stats)
    }

    /// Owner artist MBID of a synthetic album: `Ok(Some)` if known, `Ok(None)` if the
    /// owner is a synthetic artist not yet reconciled.
    async fn owner_mbid(&self, row: &EntityRow) -> Result<(Option<String>, Option<EntityRow>)> {
        let Some(parent) = &row.parent_id else { return Ok((None, None)) };
        match self.store.get_entity(parent).await? {
            Some(p) if p.kind == "artist" => Ok((p.mbid.clone(), Some(p))),
            Some(_) => Ok((None, None)),
            // Parent is not a synthetic entity: it is the MBID of an enriched artist.
            None => Ok((Some(parent.clone()), None)),
        }
    }

    async fn find_release(&self, a: &SrcAlbum) -> Result<Option<MbRelease>> {
        let ttl = self.mb_missing_ttl();
        let track_count = a.tracks.as_ref().map(Vec::len).unwrap_or(0);
        let mut candidates = self.mb.url_rels(&a.link, ttl).await?.releases;
        if candidates.is_empty()
            && let Some(upc) = &a.upc
        {
            candidates = self.mb.releases_by_barcode(upc, ttl).await?;
        }
        let by_isrc = candidates.is_empty();
        if by_isrc && let Some(isrc) = a.tracks.iter().flatten().find_map(|t| t.isrc.clone()) {
            candidates = self.mb.releases_by_isrc(&isrc, ttl).await?;
            candidates.truncate(5);
        }
        for id in candidates {
            let Some(rel) = self.mb.release(&id).await? else { continue };
            let n: usize = rel.media.iter().map(|m| m.tracks.len()).sum();
            // An ISRC is shared by every release of a recording: require the same tracklist size.
            if by_isrc && n != track_count {
                continue;
            }
            return Ok(Some(rel));
        }
        Ok(None)
    }

    /// Returns the number of tracks mapped when the album was reconciled.
    async fn reconcile_album(self: &Arc<Self>, row: &EntityRow) -> Result<Option<usize>> {
        let summary: SrcAlbum = row.payload()?;
        let (mut owner, owner_row) = self.owner_mbid(row).await?;
        if owner.is_none() && owner_row.is_none() {
            // Album only seen in a search result: nothing in Lidarr depends on it yet.
            return Ok(None);
        }
        let a = if summary.tracks.is_some() && summary.upc.is_some() {
            summary
        } else {
            match self.album_details(summary.source, &summary.id).await? {
                Some(full) => {
                    self.save_album(&full, row.parent_id.as_deref()).await?;
                    self.save_album_children(&row.id, &full).await?;
                    full
                }
                None => summary,
            }
        };
        let Some(rel) = self.find_release(&a).await? else { return Ok(None) };
        let Some(rg) = rel.release_group.clone() else { return Ok(None) };

        match (&owner, &owner_row) {
            (Some(mbid), _) => {
                if !rel.artists.iter().any(|x| x == mbid) {
                    tracing::debug!(id = %row.id, "MB release credited to another artist, skipping");
                    return Ok(None);
                }
            }
            (None, Some(artist_row)) => {
                // Owner not reconciled by URL: adopt the release's sole credited artist.
                let [credit] = rel.artists.as_slice() else { return Ok(None) };
                if self.upstream_artist(credit).await.ok().flatten().is_none() {
                    return Ok(None);
                }
                self.store.set_mbid(&artist_row.id, Kind::Artist, credit).await?;
                tracing::info!(id = %artist_row.id, mbid = %credit, "artist reconciled through album credit");
                owner = Some(credit.clone());
            }
            (None, None) => return Ok(None),
        }

        // The release group must be served by the upstream metadata server.
        if self.upstream_album(&rg).await.ok().flatten().is_none() {
            tracing::debug!(id = %row.id, %rg, "release group not yet on upstream");
            return Ok(None);
        }

        let release_id = synthetic_id(a.source, Kind::Release, &a.id);
        self.store.set_mbid(&release_id, Kind::Release, &rel.id).await?;
        let mapped = self.map_tracks(&release_id, &rel).await?;
        self.store.set_mbid(&row.id, Kind::Album, &rg).await?;
        tracing::info!(id = %row.id, %rg, release = %rel.id, tracks = mapped, owner = ?owner, "album reconciled");
        Ok(Some(mapped))
    }

    /// Maps synthetic tracks/recordings to MB tracks/recordings by (disc, position),
    /// falling back to ISRC.
    async fn map_tracks(&self, release_id: &str, rel: &MbRelease) -> Result<usize> {
        let tracks = self.store.children(release_id, Kind::Track).await?;
        let mut mapped = 0;
        for t in &tracks {
            let Ok(p) = t.payload::<TrackPayload>() else { continue };
            let by_pos = rel
                .media
                .iter()
                .find(|m| m.position == p.disc)
                .and_then(|m| m.tracks.iter().find(|x| x.position == p.position));
            let by_isrc = || {
                let isrc = p.isrc.as_ref()?;
                rel.media
                    .iter()
                    .flat_map(|m| m.tracks.iter())
                    .find(|x| x.isrcs.iter().any(|i| i.eq_ignore_ascii_case(isrc)))
            };
            // ISRC agreement wins over position when both are available.
            let m = match (by_pos, by_isrc()) {
                (_, Some(x)) => Some(x),
                (Some(x), None) => Some(x),
                _ => None,
            };
            let Some(m) = m else { continue };
            self.store.set_mbid(&t.id, Kind::Track, &m.id).await?;
            if let Some(source) = t.source() {
                let key = p.isrc.clone().unwrap_or_else(|| format!("track-{}", t.source_id));
                let rid = synthetic_id(source, Kind::Recording, &key);
                self.store.set_mbid(&rid, Kind::Recording, &m.recording).await?;
            }
            mapped += 1;
        }
        Ok(mapped)
    }

    /// Downloads missing images for synthetic entities.
    pub async fn prefetch_images(self: &Arc<Self>, limit: usize) -> Result<usize> {
        let mut done = 0;
        for row in self.store.all_of_kind(Kind::Artist).await? {
            if done >= limit {
                return Ok(done);
            }
            if self.images.cached(&row.id, "poster").await.is_some()
                || self.images.recently_failed(&row.id, "poster").await
            {
                continue;
            }
            let Ok(a) = row.payload::<SrcArtist>() else { continue };
            let c: Vec<String> = a.images.iter().map(|i| i.url.clone()).collect();
            if !c.is_empty() && self.images.assign(&row.id, "poster", &c).await.is_some() {
                done += 1;
            }
        }
        for row in self.store.all_of_kind(Kind::Album).await? {
            if done >= limit {
                break;
            }
            if row.mbid.is_some()
                || self.images.cached(&row.id, "cover").await.is_some()
                || self.images.recently_failed(&row.id, "cover").await
            {
                continue;
            }
            let Ok(a) = row.payload::<SrcAlbum>() else { continue };
            let c = self.album_cover_candidates(&a).await;
            if !c.is_empty() && self.images.assign(&row.id, "cover", &c).await.is_some() {
                done += 1;
            }
        }
        Ok(done)
    }
}

/// Spawns the periodic reconciliation and prefetch loops.
pub fn spawn_jobs(app: Arc<App>) {
    let a = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(300)).await;
        loop {
            match a.reconcile_once().await {
                Ok(s) => tracing::info!(
                    artists = s.artists,
                    albums = s.albums,
                    tracks = s.tracks,
                    "reconciliation run finished"
                ),
                Err(e) => tracing::warn!(error = %e, "reconciliation run failed"),
            }
            tokio::time::sleep(a.cfg.reconcile_interval).await;
        }
    });
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            match app.prefetch_images(500).await {
                Ok(n) if n > 0 => tracing::info!(images = n, "image prefetch finished"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "image prefetch failed"),
            }
            tokio::time::sleep(app.cfg.prefetch_interval).await;
        }
    });
}
