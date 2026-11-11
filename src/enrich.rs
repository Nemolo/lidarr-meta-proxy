//! MusicBrainz-backed responses: `oldIds` from reconciled synthetic entities,
//! missing images, and discography completion from Deezer/Spotify.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    app::App,
    ids::{Kind, Source, normalize_barcode, normalize_title, synthetic_id, year_of},
    model::*,
    sources::{SrcAlbum, map_record_type, parse_source_url},
};

/// Result of matching an MB artist's discography against Deezer/Spotify.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Enrichment {
    /// Synthetic album entity IDs to append.
    pub albums: Vec<String>,
    /// MB release-group id -> cover candidates from matched source albums.
    pub covers: HashMap<String, Vec<String>>,
    /// Artist poster candidates from linked source artists.
    pub posters: Vec<String>,
}

/// How long the enrichment request waits before answering without it.
const ENRICH_WAIT: Duration = Duration::from_secs(20);

impl App {
    /// Appends synthetic IDs reconciled to `mbid` to `old_ids`.
    async fn add_old_ids(&self, mbid: &str, kind: Kind, old_ids: &mut Vec<String>) {
        if let Ok(ids) = self.store.ids_for_mbid(mbid, kind).await {
            for id in ids {
                if id != mbid {
                    push_unique(old_ids, &id);
                }
            }
        }
    }

    /// Fills `oldIds` of an MB album (album, releases, tracks, recordings, artists).
    pub async fn album_old_ids(&self, album: &mut AlbumResource) {
        self.add_old_ids(&album.id.clone(), Kind::Album, &mut album.old_ids).await;
        for artist in &mut album.artists {
            self.add_old_ids(&artist.id.clone(), Kind::Artist, &mut artist.old_ids).await;
        }
        for release in album.releases.iter_mut().flatten() {
            self.add_old_ids(&release.id.clone(), Kind::Release, &mut release.old_ids).await;
            for t in &mut release.tracks {
                self.add_old_ids(&t.id.clone(), Kind::Track, &mut t.old_ids).await;
                self.add_old_ids(&t.recording_id.clone(), Kind::Recording, &mut t.old_recording_ids).await;
            }
        }
    }

    /// Post-processing of an upstream `/album/{mbid}` answer.
    pub async fn finish_mb_album(self: &Arc<Self>, mut album: AlbumResource) -> AlbumResource {
        self.album_old_ids(&mut album).await;
        if !album.has_image("cover")
            && let Some(img) = self.mb_album_cover(&album.id, &[]).await
        {
            album.images.push(img);
        }
        album
    }

    /// Cover for an MB release group, via the barcodes of its releases (iTunes, then Deezer).
    async fn mb_album_cover(self: &Arc<Self>, rgid: &str, extra: &[String]) -> Option<ImageResource> {
        if let Some(img) = self.images.cached(rgid, "cover").await {
            return Some(img);
        }
        if self.images.recently_failed(rgid, "cover").await {
            return None;
        }
        let mut candidates: Vec<String> = vec![];
        let releases = self.mb.release_group_releases(rgid).await.unwrap_or_default();
        let mut barcodes: Vec<String> = releases.iter().filter_map(|r| r.barcode.clone()).collect();
        barcodes.dedup();
        for code in barcodes.iter().take(3) {
            if let Some(it) = &self.itunes
                && let Ok(Some(u)) = it.artwork_by_upc(code).await
            {
                candidates.push(u);
                break;
            }
        }
        if candidates.is_empty()
            && let Some(d) = &self.deezer
        {
            for code in barcodes.iter().take(3) {
                if let Ok(Some(a)) = d.album_by_upc(code).await {
                    candidates.extend(a.images.iter().map(|i| i.url.clone()));
                    break;
                }
            }
        }
        candidates.extend(extra.iter().cloned());
        self.images.assign(rgid, "cover", &candidates).await
    }

    /// Post-processing of an upstream `/artist/{mbid}` answer.
    pub async fn finish_mb_artist(self: &Arc<Self>, mut artist: ArtistResource) -> ArtistResource {
        let mbid = artist.id.clone();
        self.add_old_ids(&mbid, Kind::Artist, &mut artist.old_ids).await;
        for album in &mut artist.albums {
            self.add_old_ids(&album.id.clone(), Kind::Album, &mut album.old_ids).await;
        }
        if !self.cfg.enrich_mb_artists {
            return artist;
        }

        // Compute (or reuse) the enrichment, waiting a bounded time for it.
        let enrichment = match self.cached_enrichment(&mbid).await {
            Some(e) => e,
            None => {
                let app = self.clone();
                let a = artist.clone();
                let handle = tokio::spawn(async move { app.compute_enrichment(&a).await });
                match tokio::time::timeout(ENRICH_WAIT, handle).await {
                    Ok(Ok(Ok(e))) => e,
                    Ok(Ok(Err(e))) => {
                        tracing::warn!(%mbid, error = %e, "enrichment failed");
                        Enrichment::default()
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(%mbid, error = %e, "enrichment task failed");
                        Enrichment::default()
                    }
                    Err(_) => {
                        tracing::info!(%mbid, "enrichment still running, answering without it");
                        Enrichment::default()
                    }
                }
            }
        };

        if !artist.has_image("poster")
            && !enrichment.posters.is_empty()
            && let Some(img) = self.image_for(&mbid, "poster", enrichment.posters.clone(), true).await
        {
            artist.images.push(img);
        }
        for album in &mut artist.albums {
            if !album.has_image("cover")
                && let Some(c) = enrichment.covers.get(&album.id)
                && let Some(img) = self.image_for(&album.id, "cover", c.clone(), false).await
            {
                album.images.push(img);
            }
        }
        let existing: HashSet<String> = artist.albums.iter().map(|a| a.id.clone()).collect();
        for id in &enrichment.albums {
            if existing.contains(id) {
                continue;
            }
            let Ok(Some(row)) = self.store.get_entity(id).await else { continue };
            // Reconciled meanwhile: the MB album is (or will be) in the upstream list.
            if row.mbid.is_some() {
                continue;
            }
            let Ok(a) = row.payload::<SrcAlbum>() else { continue };
            artist.albums.push(self.album_summary(id, &mbid, &a, false).await);
        }
        artist
    }

    async fn cached_enrichment(&self, mbid: &str) -> Option<Enrichment> {
        let body = self.store.cache_get(&format!("enrich:{mbid}"), self.source_ttl()).await.ok()??;
        serde_json::from_str(&body).ok()
    }

    /// Source artists linked to an MB artist: URL relations (upstream links, then
    /// WS2) and synthetic artists reconciled to it.
    async fn linked_source_artists(&self, artist: &ArtistResource) -> Vec<(Source, String)> {
        let mut out: Vec<(Source, String)> = vec![];
        let mut add = |s: Source, id: String| {
            if !out.contains(&(s, id.clone())) {
                out.push((s, id));
            }
        };
        let mut from_links = false;
        for l in &artist.links {
            if let Some((s, "artist", id)) = parse_source_url(&l.target) {
                add(s, id);
                from_links = true;
            }
        }
        if !from_links {
            match self.mb.artist_urls(&artist.id).await {
                Ok(urls) => {
                    for u in urls {
                        if let Some((s, "artist", id)) = parse_source_url(&u) {
                            add(s, id);
                        }
                    }
                }
                Err(e) => tracing::debug!(error = %e, "MB url-rels lookup failed"),
            }
        }
        if let Ok(ids) = self.store.ids_for_mbid(&artist.id, Kind::Artist).await {
            for id in ids {
                if let Ok(Some(row)) = self.store.get_entity(&id).await
                    && let Some(s) = row.source()
                {
                    add(s, row.source_id.clone());
                }
            }
        }
        out.retain(|(s, _)| match s {
            Source::Deezer => self.deezer.is_some(),
            Source::Spotify => self.spotify.is_some(),
        });
        // Deezer first: it has priority when the same album exists on both.
        out.sort_by_key(|(s, _)| matches!(s, Source::Spotify));
        out
    }

    /// Matches the discographies of the linked Deezer/Spotify artists against the
    /// MB release groups; unmatched albums become synthetic albums of the MB artist.
    pub async fn compute_enrichment(self: &Arc<Self>, artist: &ArtistResource) -> Result<Enrichment> {
        let mbid = artist.id.clone();
        let linked = self.linked_source_artists(artist).await;
        let mut result = Enrichment::default();
        if linked.is_empty() {
            self.store.cache_put(&format!("enrich:{mbid}"), &serde_json::to_string(&result)?).await?;
            self.store.record_enrichment(&mbid, "").await?;
            return Ok(result);
        }

        // MB side: release groups by normalized title (+year), barcodes per release group.
        let mb_albums: HashSet<String> = artist.albums.iter().map(|a| a.id.clone()).collect();
        let mut by_title: HashMap<String, Vec<(String, Option<i32>)>> = HashMap::new();
        for a in &artist.albums {
            by_title
                .entry(normalize_title(&a.title))
                .or_default()
                .push((a.id.clone(), year_of(a.release_date.as_deref())));
        }
        let mut by_barcode: HashMap<String, String> = HashMap::new();
        match self.mb.artist_releases(&mbid).await {
            Ok(releases) => {
                for r in releases {
                    if let (Some(code), Some(rg)) = (r.barcode.as_deref().and_then(normalize_barcode), r.release_group)
                    {
                        by_barcode.insert(code, rg);
                    }
                }
            }
            Err(e) => tracing::warn!(%mbid, error = %e, "MB release browse failed; matching by title only"),
        }

        let match_title = |a: &SrcAlbum| -> Option<String> {
            let cands = by_title.get(&normalize_title(&a.title))?;
            let year = year_of(a.release_date.as_deref());
            if let Some((id, _)) = cands.iter().find(|(_, y)| y.is_some() && *y == year) {
                return Some(id.clone());
            }
            // Same title, different year: almost always a reissue of the same release group.
            if cands.len() == 1 { Some(cands[0].0.clone()) } else { None }
        };

        // Albums kept as synthetic so far (for Deezer/Spotify cross-dedupe).
        let mut kept_titles: HashMap<String, Option<i32>> = HashMap::new();
        let mut kept_upcs: HashSet<String> = HashSet::new();

        for (source, source_artist) in &linked {
            if let Ok(Some(sa)) = self.fetch_artist(*source, source_artist).await {
                result.posters.extend(sa.images.iter().map(|i| i.url.clone()));
            }
            let albums = match self.discography(*source, source_artist).await {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(%mbid, source = source.as_str(), error = %e, "discography fetch failed");
                    // Do not drop albums already published because of a transient error.
                    return Err(e);
                }
            };
            let mut seen_exact: HashSet<(String, String)> = HashSet::new();
            let mut albums = albums;
            albums.sort_by(|a, b| numeric_id_cmp(&a.id, &b.id));
            for summary in albums {
                let sid = synthetic_id(summary.source, Kind::Album, &summary.id);
                // Already reconciled to an album of this artist.
                if let Ok(Some(row)) = self.store.get_entity(&sid).await
                    && row.mbid.as_ref().map(|m| mb_albums.contains(m)).unwrap_or(false)
                {
                    continue;
                }
                if let Some(rg) = match_title(&summary) {
                    result.covers.entry(rg).or_default().extend(summary.images.iter().map(|i| i.url.clone()));
                    continue;
                }
                // Exact duplicates within the source (regional variants): keep the lowest ID.
                let (kind, _) = map_record_type(&summary.record_type, None);
                if !seen_exact.insert((summary.title.to_lowercase(), kind.to_string())) {
                    continue;
                }
                // Barcode needs the full album.
                let full = match self.album_details(summary.source, &summary.id).await {
                    Ok(Some(f)) => f,
                    Ok(None) => summary.clone(),
                    Err(e) => {
                        tracing::debug!(error = %e, "album details failed");
                        summary.clone()
                    }
                };
                let upc = full.upc.as_deref().and_then(normalize_barcode);
                if let Some(rg) = upc.as_ref().and_then(|u| by_barcode.get(u)) {
                    result.covers.entry(rg.clone()).or_default().extend(full.images.iter().map(|i| i.url.clone()));
                    continue;
                }
                // Cross-source dedupe: Deezer wins over Spotify.
                if let Some(u) = &upc
                    && kept_upcs.contains(u)
                {
                    continue;
                }
                let nt = normalize_title(&full.title);
                if *source == Source::Spotify
                    && let Some(y) = kept_titles.get(&nt)
                    && (y.is_none() || *y == year_of(full.release_date.as_deref()))
                {
                    continue;
                }
                if let Some(u) = upc {
                    kept_upcs.insert(u);
                }
                kept_titles.insert(nt, year_of(full.release_date.as_deref()));
                let id = self.save_album(&full, Some(&mbid)).await?;
                if full.tracks.is_some() {
                    self.save_album_children(&id, &full).await?;
                }
                result.albums.push(id);
            }
        }

        result.albums.sort();
        result.albums.dedup();
        let hash = if result.albums.is_empty() {
            String::new()
        } else {
            hex::encode(Sha256::digest(result.albums.join(",").as_bytes()))
        };
        if self.store.record_enrichment(&mbid, &hash).await? {
            tracing::info!(%mbid, added = result.albums.len(), "discography enrichment changed");
        }
        self.store.cache_put(&format!("enrich:{mbid}"), &serde_json::to_string(&result)?).await?;
        Ok(result)
    }
}

/// Orders numeric IDs numerically (stable choice among duplicates), others lexically.
fn numeric_id_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}
