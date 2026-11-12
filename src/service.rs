//! Endpoint logic: artist, album, search and recent updates.

use std::{collections::HashSet, sync::Arc, time::Instant};

use serde_json::Value;

use crate::{
    app::{ApiError, App},
    ids::{Kind, Source, normalize_title, parse_uuid, synthetic_id},
    model::*,
    sources::{SrcAlbum, SrcArtist, map_record_type},
};

impl App {
    // ------------------------------------------------------------------ artist

    pub async fn get_artist(self: &Arc<Self>, raw_id: &str) -> Result<ArtistResource, ApiError> {
        let Some(id) = parse_uuid(raw_id) else { return Err(ApiError::BadRequest) };
        if let Some(row) = self.store.get_entity(&id).await? {
            if row.kind != "artist" {
                return Err(ApiError::NotFound);
            }
            if let Some(mbid) = &row.mbid {
                match self.upstream_artist(mbid).await {
                    Ok(Some(mut mb)) => {
                        push_unique(&mut mb.old_ids, &id);
                        return Ok(self.finish_mb_artist(mb).await);
                    }
                    Ok(None) => tracing::warn!(%id, %mbid, "reconciled MBID unknown upstream, serving synthetic"),
                    Err(e) => tracing::warn!(%id, %mbid, error = %e, "upstream failed, serving synthetic"),
                }
            }
            return self.synthetic_artist(&row.id).await;
        }
        match self.upstream_artist(&id).await? {
            Some(mb) => Ok(self.finish_mb_artist(mb).await),
            None => Err(ApiError::NotFound),
        }
    }

    pub async fn synthetic_artist(self: &Arc<Self>, id: &str) -> Result<ArtistResource, ApiError> {
        let row = self.store.get_entity(id).await?.ok_or(ApiError::NotFound)?;
        let sa = self.artist_payload(&row).await?;
        let mut artist = self.artist_resource(id, &sa, true).await;

        let albums = match self.discography(sa.source, &sa.id).await {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(%id, error = %e, "discography fetch failed, using stored albums");
                let rows = self.store.children(id, Kind::Album).await?;
                if rows.is_empty() {
                    return Err(ApiError::Unavailable(e.to_string()));
                }
                rows.iter().filter_map(|r| r.payload::<SrcAlbum>().ok()).collect()
            }
        };
        for (album_id, a) in self.save_discography(id, albums).await? {
            artist.albums.push(self.album_summary(&album_id, id, &a, false).await);
        }
        Ok(artist)
    }

    /// Stores a synthetic artist's discography, dropping exact duplicates
    /// (same title and type: regional variants) deterministically.
    async fn save_discography(
        &self,
        artist_id: &str,
        mut albums: Vec<SrcAlbum>,
    ) -> anyhow::Result<Vec<(String, SrcAlbum)>> {
        albums.sort_by(|a, b| match (a.id.parse::<u64>(), b.id.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => a.id.cmp(&b.id),
        });
        let mut seen = HashSet::new();
        let mut out = vec![];
        for a in albums {
            let (kind, _) = map_record_type(&a.record_type, None);
            if !seen.insert((a.title.to_lowercase(), kind)) {
                continue;
            }
            let album_id = self.save_album(&a, Some(artist_id)).await?;
            out.push((album_id, a));
        }
        Ok(out)
    }

    // ------------------------------------------------------------------- album

    pub async fn get_album(self: &Arc<Self>, raw_id: &str) -> Result<AlbumResource, ApiError> {
        let Some(id) = parse_uuid(raw_id) else { return Err(ApiError::BadRequest) };
        if let Some(row) = self.store.get_entity(&id).await? {
            if row.kind != "album" {
                return Err(ApiError::NotFound);
            }
            if let Some(mbid) = &row.mbid {
                match self.upstream_album(mbid).await {
                    Ok(Some(mut mb)) => {
                        push_unique(&mut mb.old_ids, &id);
                        return Ok(self.finish_mb_album(mb).await);
                    }
                    Ok(None) => tracing::warn!(%id, %mbid, "reconciled MBID unknown upstream, serving synthetic"),
                    Err(e) => tracing::warn!(%id, %mbid, error = %e, "upstream failed, serving synthetic"),
                }
            }
            return self.synthetic_album(&id).await;
        }
        match self.upstream_album(&id).await? {
            Some(mb) => Ok(self.finish_mb_album(mb).await),
            None => Err(ApiError::NotFound),
        }
    }

    pub async fn synthetic_album(self: &Arc<Self>, id: &str) -> Result<AlbumResource, ApiError> {
        self.build_synthetic_album(id, true).await
    }

    /// `wait_images`: download a missing cover before answering (album page) or not (search).
    async fn build_synthetic_album(self: &Arc<Self>, id: &str, wait_images: bool) -> Result<AlbumResource, ApiError> {
        let row = self.store.get_entity(id).await?.ok_or(ApiError::NotFound)?;
        let a = self.album_payload(&row).await?;
        let row = self.store.get_entity(id).await?.unwrap_or(row);
        let owner = self.album_owner(&row, &a).await?;
        if row.parent_id.is_none() {
            // Remember the owner chosen for albums first seen in a search.
            sqlx::query("UPDATE entity SET parent_id = ? WHERE id = ? AND parent_id IS NULL")
                .bind(&owner.artist.id)
                .bind(id)
                .execute(&self.store.pool)
                .await
                .map_err(|e| ApiError::Unavailable(e.to_string()))?;
        }
        let album = self.album_resource(id, &a, &owner, wait_images).await;
        if album.releases.as_ref().map(|r| r.iter().all(|r| r.track_count == 0)).unwrap_or(true) {
            // Lidarr drops releases without tracks; an album without tracks is useless.
            tracing::warn!(%id, "synthetic album has no tracks");
        }
        Ok(album)
    }

    // ------------------------------------------------------------------ search
    //
    // Latency: upstream and Deezer/Spotify are queried in parallel; MusicBrainz checks
    // (1 req/s) share one budget and a deadline per request. Hits not verified in time
    // are left out of this answer and verified in background, so the cached results
    // make the next identical search complete.

    fn search_budget(&self) -> SearchBudget {
        SearchBudget {
            deadline: Instant::now() + self.cfg.search_timeout,
            lookups: self.cfg.search_mb_lookups,
            skipped: false,
        }
    }

    /// MB artist ID for a Deezer/Spotify artist: reconciled entity, upstream links
    /// already in the response, or MB URL relation (within the budget).
    /// `None` = not verifiable now.
    async fn mb_artist_for(
        &self,
        a: &SrcArtist,
        upstream: &[ArtistResource],
        budget: &mut SearchBudget,
    ) -> Option<Option<String>> {
        let sid = synthetic_id(a.source, Kind::Artist, &a.id);
        if let Ok(Some(row)) = self.store.get_entity(&sid).await
            && row.mbid.is_some()
        {
            return Some(row.mbid);
        }
        for u in upstream {
            if u.links.iter().any(|l| same_url(&l.target, &a.link)) {
                return Some(Some(u.id.clone()));
            }
        }
        if !budget.take() {
            return None;
        }
        match budget.within(self.mb.url_rels(&a.link, self.mb_missing_ttl())).await? {
            Ok(rels) => Some(rels.artists.into_iter().next()),
            Err(e) => {
                tracing::debug!(error = %e, "MB url lookup failed");
                None
            }
        }
    }

    /// MB release group for a source album: reconciled entity, URL relation, barcode.
    async fn mb_album_for(&self, a: &SrcAlbum, budget: &mut SearchBudget) -> Option<Option<String>> {
        let sid = synthetic_id(a.source, Kind::Album, &a.id);
        if let Ok(Some(row)) = self.store.get_entity(&sid).await
            && row.mbid.is_some()
        {
            return Some(row.mbid);
        }
        if !budget.take() {
            return None;
        }
        let ttl = self.mb_missing_ttl();
        let mut releases = match budget.within(self.mb.url_rels(&a.link, ttl)).await? {
            Ok(r) => r.releases,
            Err(_) => return None,
        };
        if releases.is_empty() {
            let upc = match &a.upc {
                Some(u) => Some(u.clone()),
                None => self.album_details(a.source, &a.id).await.ok().flatten().and_then(|f| f.upc),
            };
            if let Some(upc) = upc {
                if !budget.take() {
                    return None;
                }
                releases = budget.within(self.mb.releases_by_barcode(&upc, ttl)).await?.ok()?;
            }
        }
        let Some(rel) = releases.first() else { return Some(None) };
        // Release -> release group.
        if !budget.take() {
            return None;
        }
        match budget.within(self.mb.release(rel)).await? {
            Ok(Some(r)) => Some(r.release_group),
            _ => None,
        }
    }

    async fn source_artist_hits(&self, query: &str) -> Vec<SrcArtist> {
        let limit = self.cfg.search_limit;
        let deezer = async {
            match &self.deezer {
                Some(d) => d.search_artists(query, limit).await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "deezer search failed");
                    vec![]
                }),
                None => vec![],
            }
        };
        let spotify = async {
            match &self.spotify {
                Some(s) => s.search_artists(query, limit).await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "spotify search failed");
                    vec![]
                }),
                None => vec![],
            }
        };
        let (mut a, b) = tokio::join!(deezer, spotify);
        a.extend(b);
        a
    }

    async fn source_album_hits(&self, query: &str, artist: Option<&str>) -> Vec<SrcAlbum> {
        let limit = self.cfg.search_limit.min(5);
        let deezer = async {
            match &self.deezer {
                Some(d) => d.search_albums(query, artist, limit).await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "deezer album search failed");
                    vec![]
                }),
                None => vec![],
            }
        };
        let spotify = async {
            match &self.spotify {
                Some(s) => s.search_albums(query, artist, limit).await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "spotify album search failed");
                    vec![]
                }),
                None => vec![],
            }
        };
        let (mut a, b) = tokio::join!(deezer, spotify);
        a.extend(b);
        a
    }

    pub async fn search_artists(self: &Arc<Self>, query: &str) -> Result<Vec<ArtistResource>, ApiError> {
        let q = vec![("type".to_string(), "artist".to_string()), ("query".to_string(), query.to_string())];
        let mut budget = self.search_budget();
        let (upstream, hits) =
            tokio::join!(self.upstream_list::<ArtistResource>("search", &q), self.source_artist_hits(query));
        let mut results = upstream?;
        for r in &mut results {
            self.mark_old_ids_artist(r).await;
        }
        let extra = self.synthetic_artist_hits(hits, &results, &mut budget).await;
        results.extend(extra);
        Ok(results)
    }

    async fn mark_old_ids_artist(&self, a: &mut ArtistResource) {
        if let Ok(ids) = self.store.ids_for_mbid(&a.id, Kind::Artist).await {
            for id in ids {
                push_unique(&mut a.old_ids, &id);
            }
        }
    }

    async fn synthetic_artist_hits(
        self: &Arc<Self>,
        hits: Vec<SrcArtist>,
        upstream: &[ArtistResource],
        budget: &mut SearchBudget,
    ) -> Vec<ArtistResource> {
        enum Hit {
            Mb(String),
            Synthetic(SrcArtist),
        }
        let mut seen_mb: HashSet<String> = upstream.iter().map(|a| a.id.clone()).collect();
        let mut names: HashSet<String> = HashSet::new();
        let mut verified = vec![];
        let mut pending = vec![];
        for hit in hits {
            match self.mb_artist_for(&hit, upstream, budget).await {
                // On MusicBrainz: never return it as synthetic.
                Some(Some(mbid)) => {
                    if seen_mb.insert(mbid.clone()) {
                        verified.push(Hit::Mb(mbid));
                    }
                }
                Some(None) => {
                    // Spotify is secondary: skip if a Deezer hit with the same name exists.
                    let n = normalize_title(&hit.name);
                    if hit.source == Source::Spotify && names.contains(&n) {
                        continue;
                    }
                    names.insert(n);
                    verified.push(Hit::Synthetic(hit));
                }
                // Not verifiable now: omitted rather than risking a duplicate.
                None => pending.push(hit),
            }
        }
        if !pending.is_empty() {
            tracing::debug!(pending = pending.len(), "artist hits left for background verification");
            let app = self.clone();
            tokio::spawn(async move {
                let mut b = SearchBudget::background();
                for hit in pending {
                    app.mb_artist_for(&hit, &[], &mut b).await;
                }
            });
        }
        // Build the answers concurrently (upstream fetches, DB writes).
        let built = futures::future::join_all(verified.into_iter().map(|h| async move {
            match h {
                Hit::Mb(mbid) => match self.upstream_artist(&mbid).await {
                    Ok(Some(mut mb)) => {
                        mb.albums.clear();
                        self.mark_old_ids_artist(&mut mb).await;
                        Some(mb)
                    }
                    _ => None,
                },
                Hit::Synthetic(hit) => match self.save_artist(&hit).await {
                    Ok(id) => Some(self.artist_resource(&id, &hit, false).await),
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to store artist");
                        None
                    }
                },
            }
        }))
        .await;
        built.into_iter().flatten().collect()
    }

    pub async fn search_albums(
        self: &Arc<Self>,
        query: &str,
        artist: &str,
        include_tracks: bool,
    ) -> Result<Vec<AlbumResource>, ApiError> {
        let mut q = vec![("type".to_string(), "album".to_string()), ("query".to_string(), query.to_string())];
        q.push(("artist".to_string(), artist.to_string()));
        if include_tracks {
            q.push(("includeTracks".to_string(), "1".to_string()));
        }
        let mut budget = self.search_budget();
        let (upstream, hits) = tokio::join!(
            self.upstream_list::<AlbumResource>("search", &q),
            self.source_album_hits(query, Some(artist))
        );
        let mut results = upstream?;
        for r in &mut results {
            self.album_old_ids(r).await;
        }
        let extra = self.synthetic_album_hits(hits, &results, &mut budget).await;
        results.extend(extra);
        Ok(results)
    }

    async fn synthetic_album_hits(
        self: &Arc<Self>,
        hits: Vec<SrcAlbum>,
        upstream: &[AlbumResource],
        budget: &mut SearchBudget,
    ) -> Vec<AlbumResource> {
        enum Hit {
            Mb(String),
            Synthetic(Box<SrcAlbum>),
        }
        // Upstream albums by (artist name, title): a source hit matching one is a duplicate.
        let upstream_keys: HashSet<(String, String)> = upstream
            .iter()
            .flat_map(|a| {
                let t = normalize_title(&a.title);
                a.artists.iter().map(move |ar| (normalize_title(&ar.artist_name), t.clone()))
            })
            .collect();
        let mut seen_mb: HashSet<String> = upstream.iter().map(|a| a.id.clone()).collect();
        let mut seen_keys: HashSet<(String, String)> = HashSet::new();
        let mut verified = vec![];
        let mut pending = vec![];
        for hit in hits {
            let key = (normalize_title(hit.artist_name.as_deref().unwrap_or("")), normalize_title(&hit.title));
            if upstream_keys.contains(&key) || !seen_keys.insert(key) {
                continue;
            }
            match self.mb_album_for(&hit, budget).await {
                Some(Some(rg)) => {
                    if seen_mb.insert(rg.clone()) {
                        verified.push(Hit::Mb(rg));
                    }
                }
                Some(None) => verified.push(Hit::Synthetic(Box::new(hit))),
                None => pending.push(hit),
            }
        }
        if !pending.is_empty() {
            tracing::debug!(pending = pending.len(), "album hits left for background verification");
            let app = self.clone();
            tokio::spawn(async move {
                let mut b = SearchBudget::background();
                for hit in pending {
                    app.mb_album_for(&hit, &mut b).await;
                }
            });
        }
        let built = futures::future::join_all(verified.into_iter().map(|h| async move {
            match h {
                Hit::Mb(rg) => match self.upstream_album(&rg).await {
                    Ok(Some(mb)) => Some(self.finish_mb_album(mb).await),
                    _ => None,
                },
                Hit::Synthetic(hit) => {
                    let id = match self.save_album(&hit, None).await {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to store album");
                            return None;
                        }
                    };
                    // Search results always need tracks: Lidarr drops albums without releases.
                    // Covers are not awaited here (cached ones are used, others load in background).
                    match self.build_synthetic_album(&id, false).await {
                        Ok(a) if a.releases.as_ref().map(|r| r.iter().any(|r| r.track_count > 0)).unwrap_or(false) => {
                            Some(a)
                        }
                        Ok(_) => None,
                        Err(e) => {
                            tracing::debug!(error = ?e, "synthetic album build failed");
                            None
                        }
                    }
                }
            }
        }))
        .await;
        built.into_iter().flatten().collect()
    }

    pub async fn search_all(self: &Arc<Self>, query: &str) -> Result<Vec<EntityResource>, ApiError> {
        let q = vec![("type".to_string(), "all".to_string()), ("query".to_string(), query.to_string())];
        // One budget for artists and albums: MusicBrainz is the bottleneck (1 req/s).
        let mut budget = self.search_budget();
        let (upstream, artist_hits, album_hits) = tokio::join!(
            self.upstream_list::<EntityResource>("search", &q),
            self.source_artist_hits(query),
            self.source_album_hits(query, None)
        );
        let mut results = upstream?;
        let up_artists: Vec<ArtistResource> = results.iter().filter_map(|e| e.artist.clone()).collect();
        let up_albums: Vec<AlbumResource> = results.iter().filter_map(|e| e.album.clone()).collect();
        for e in &mut results {
            if let Some(a) = &mut e.artist {
                self.mark_old_ids_artist(a).await;
            }
            if let Some(a) = &mut e.album {
                self.album_old_ids(a).await;
            }
        }
        let min_score = results.iter().map(|e| e.score).min().unwrap_or(100);
        let score = (min_score - 1).max(0);
        for a in self.synthetic_artist_hits(artist_hits, &up_artists, &mut budget).await {
            if results.iter().any(|e| e.artist.as_ref().map(|x| x.id == a.id).unwrap_or(false)) {
                continue;
            }
            results.push(EntityResource { score, artist: Some(a), ..Default::default() });
        }
        for a in self.synthetic_album_hits(album_hits, &up_albums, &mut budget).await {
            if results.iter().any(|e| e.album.as_ref().map(|x| x.id == a.id).unwrap_or(false)) {
                continue;
            }
            results.push(EntityResource { score, album: Some(a), ..Default::default() });
        }
        if budget.skipped {
            tracing::info!(query, "search answered before all MusicBrainz checks; remaining ones run in background");
        }
        Ok(results)
    }

    /// Upstream list endpoint; 404 is treated as "no results".
    async fn upstream_list<T: serde::de::DeserializeOwned>(
        &self,
        route: &str,
        q: &[(String, String)],
    ) -> Result<Vec<T>, ApiError> {
        let r = self.upstream.get(route, q).await?;
        match r.status {
            200 => {
                let v: Value = r.json().ok_or_else(|| ApiError::Unavailable("invalid upstream JSON".into()))?;
                from_upstream(v).map_err(|e| ApiError::Unavailable(e.to_string()))
            }
            404 => Ok(vec![]),
            400 => Err(ApiError::BadRequest),
            s => Err(ApiError::Unavailable(format!("upstream HTTP {s}"))),
        }
    }

    // ------------------------------------------------------------------ recent

    pub async fn recent(&self, kind: Kind, since: i64) -> Result<RecentUpdatesResource, ApiError> {
        let route = format!("recent/{}", kind.as_str());
        let q = vec![("since".to_string(), since.to_string())];
        let mut res: RecentUpdatesResource = match self.upstream.get(&route, &q).await {
            Ok(r) if r.status == 200 => r
                .json()
                .and_then(|v| from_upstream(v).ok())
                .unwrap_or(RecentUpdatesResource { limited: true, ..Default::default() }),
            // Upstream unknown: ask Lidarr to refresh everything rather than miss changes.
            _ => RecentUpdatesResource { limited: true, ..Default::default() },
        };
        let mut local = self.store.recent(kind, since).await?;
        if kind == Kind::Artist {
            local.extend(self.store.recent_enriched_artists(since).await?);
        }
        for id in local {
            push_unique(&mut res.items, &id);
        }
        res.count = res.items.len() as i64;
        Ok(res)
    }
}

/// MusicBrainz lookups allowed while answering one search.
struct SearchBudget {
    deadline: Instant,
    lookups: usize,
    /// Some check was skipped for lack of budget/time.
    skipped: bool,
}

impl SearchBudget {
    /// Budget for verifications continued after the answer was sent.
    fn background() -> Self {
        Self { deadline: Instant::now() + std::time::Duration::from_secs(300), lookups: 50, skipped: false }
    }

    /// Runs a lookup, giving up (`None`) when the deadline passes.
    async fn within<F: std::future::Future>(&mut self, f: F) -> Option<F::Output> {
        match tokio::time::timeout_at(self.deadline.into(), f).await {
            Ok(v) => Some(v),
            Err(_) => {
                self.skipped = true;
                None
            }
        }
    }

    fn take(&mut self) -> bool {
        if self.lookups == 0 || Instant::now() >= self.deadline {
            self.skipped = true;
            return false;
        }
        self.lookups -= 1;
        true
    }
}

fn same_url(a: &str, b: &str) -> bool {
    match (crate::sources::parse_source_url(a), crate::sources::parse_source_url(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}
