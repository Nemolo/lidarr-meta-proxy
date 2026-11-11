//! Endpoint logic: artist, album, search and recent updates.

use std::{collections::HashSet, sync::Arc};

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
        let album = self.album_resource(id, &a, &owner, true).await;
        if album.releases.as_ref().map(|r| r.iter().all(|r| r.track_count == 0)).unwrap_or(true) {
            // Lidarr drops releases without tracks; an album without tracks is useless.
            tracing::warn!(%id, "synthetic album has no tracks");
        }
        Ok(album)
    }

    // ------------------------------------------------------------------ search

    /// MB artist ID for a Deezer/Spotify artist: reconciled entity, upstream links
    /// already in the response, or MB URL relation (within the lookup budget).
    async fn mb_artist_for(
        &self,
        a: &SrcArtist,
        upstream: &[ArtistResource],
        budget: &mut usize,
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
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        match self.mb.url_rels(&a.link, self.mb_missing_ttl()).await {
            Ok(rels) => Some(rels.artists.into_iter().next()),
            Err(e) => {
                tracing::debug!(error = %e, "MB url lookup failed");
                None
            }
        }
    }

    pub async fn search_artists(self: &Arc<Self>, query: &str) -> Result<Vec<ArtistResource>, ApiError> {
        let q = vec![("type".to_string(), "artist".to_string()), ("query".to_string(), query.to_string())];
        let mut results: Vec<ArtistResource> = self.upstream_list("search", &q).await?;
        for r in &mut results {
            self.mark_old_ids_artist(r).await;
        }
        let extra = self.synthetic_artist_hits(query, &results).await;
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

    async fn synthetic_artist_hits(self: &Arc<Self>, query: &str, upstream: &[ArtistResource]) -> Vec<ArtistResource> {
        let limit = self.cfg.search_limit;
        let mut hits: Vec<SrcArtist> = vec![];
        if let Some(d) = &self.deezer {
            match d.search_artists(query, limit).await {
                Ok(h) => hits.extend(h),
                Err(e) => tracing::warn!(error = %e, "deezer search failed"),
            }
        }
        if let Some(s) = &self.spotify {
            match s.search_artists(query, limit).await {
                Ok(h) => hits.extend(h),
                Err(e) => tracing::warn!(error = %e, "spotify search failed"),
            }
        }
        let mut budget = self.cfg.search_mb_lookups;
        let mut seen_mb: HashSet<String> = upstream.iter().map(|a| a.id.clone()).collect();
        let mut names: HashSet<String> = HashSet::new();
        let mut out = vec![];
        for hit in hits {
            match self.mb_artist_for(&hit, upstream, &mut budget).await {
                // On MusicBrainz: never return it as synthetic.
                Some(Some(mbid)) => {
                    if seen_mb.insert(mbid.clone())
                        && let Ok(Some(mut mb)) = self.upstream_artist(&mbid).await
                    {
                        mb.albums.clear();
                        self.mark_old_ids_artist(&mut mb).await;
                        out.push(mb);
                    }
                }
                Some(None) => {
                    // Spotify is secondary: skip if a Deezer hit with the same name exists.
                    let n = normalize_title(&hit.name);
                    if hit.source == Source::Spotify && names.contains(&n) {
                        continue;
                    }
                    names.insert(n);
                    match self.save_artist(&hit).await {
                        Ok(id) => out.push(self.artist_resource(&id, &hit, false).await),
                        Err(e) => tracing::warn!(error = %e, "failed to store artist"),
                    }
                }
                // Not verifiable within budget: omitted rather than risking a duplicate.
                None => {}
            }
        }
        out
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
        let mut results: Vec<AlbumResource> = self.upstream_list("search", &q).await?;
        for r in &mut results {
            self.album_old_ids(r).await;
        }
        let extra = self.synthetic_album_hits(query, Some(artist), &results).await;
        results.extend(extra);
        Ok(results)
    }

    async fn synthetic_album_hits(
        self: &Arc<Self>,
        query: &str,
        artist: Option<&str>,
        upstream: &[AlbumResource],
    ) -> Vec<AlbumResource> {
        let limit = self.cfg.search_limit.min(5);
        let mut hits: Vec<SrcAlbum> = vec![];
        if let Some(d) = &self.deezer {
            match d.search_albums(query, artist, limit).await {
                Ok(h) => hits.extend(h),
                Err(e) => tracing::warn!(error = %e, "deezer album search failed"),
            }
        }
        if let Some(s) = &self.spotify {
            match s.search_albums(query, artist, limit).await {
                Ok(h) => hits.extend(h),
                Err(e) => tracing::warn!(error = %e, "spotify album search failed"),
            }
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
        let mut budget = self.cfg.search_mb_lookups;
        let mut out = vec![];
        for hit in hits {
            let key = (normalize_title(hit.artist_name.as_deref().unwrap_or("")), normalize_title(&hit.title));
            if upstream_keys.contains(&key) || !seen_keys.insert(key) {
                continue;
            }
            match self.mb_album_for(&hit, &mut budget).await {
                Some(Some(rg)) => {
                    if seen_mb.insert(rg.clone())
                        && let Ok(Some(mb)) = self.upstream_album(&rg).await
                    {
                        out.push(self.finish_mb_album(mb).await);
                    }
                }
                Some(None) => {
                    let id = match self.save_album(&hit, None).await {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to store album");
                            continue;
                        }
                    };
                    // Search results always need tracks: Lidarr drops albums without releases.
                    match self.synthetic_album(&id).await {
                        Ok(a) if a.releases.as_ref().map(|r| r.iter().any(|r| r.track_count > 0)).unwrap_or(false) => {
                            out.push(a)
                        }
                        Ok(_) => {}
                        Err(e) => tracing::debug!(error = ?e, "synthetic album build failed"),
                    }
                }
                None => {}
            }
        }
        out
    }

    /// MB release group for a source album: reconciled entity, URL relation, barcode.
    async fn mb_album_for(&self, a: &SrcAlbum, budget: &mut usize) -> Option<Option<String>> {
        let sid = synthetic_id(a.source, Kind::Album, &a.id);
        if let Ok(Some(row)) = self.store.get_entity(&sid).await
            && row.mbid.is_some()
        {
            return Some(row.mbid);
        }
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let ttl = self.mb_missing_ttl();
        let mut releases = match self.mb.url_rels(&a.link, ttl).await {
            Ok(r) => r.releases,
            Err(_) => return None,
        };
        if releases.is_empty() {
            let upc = match &a.upc {
                Some(u) => Some(u.clone()),
                None => self.album_details(a.source, &a.id).await.ok().flatten().and_then(|f| f.upc),
            };
            if let Some(upc) = upc {
                if *budget == 0 {
                    return None;
                }
                *budget -= 1;
                releases = self.mb.releases_by_barcode(&upc, ttl).await.ok()?;
            }
        }
        let Some(rel) = releases.first() else { return Some(None) };
        // Release -> release group (one more lookup, cached).
        match self.mb.release(rel).await {
            Ok(Some(r)) => Some(r.release_group),
            _ => None,
        }
    }

    pub async fn search_all(self: &Arc<Self>, query: &str) -> Result<Vec<EntityResource>, ApiError> {
        let q = vec![("type".to_string(), "all".to_string()), ("query".to_string(), query.to_string())];
        let mut results: Vec<EntityResource> = self.upstream_list("search", &q).await?;
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
        for a in self.synthetic_artist_hits(query, &up_artists).await {
            if results.iter().any(|e| e.artist.as_ref().map(|x| x.id == a.id).unwrap_or(false)) {
                continue;
            }
            results.push(EntityResource { score, artist: Some(a), ..Default::default() });
        }
        for a in self.synthetic_album_hits(query, None, &up_albums).await {
            if results.iter().any(|e| e.album.as_ref().map(|x| x.id == a.id).unwrap_or(false)) {
                continue;
            }
            results.push(EntityResource { score, album: Some(a), ..Default::default() });
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

fn same_url(a: &str, b: &str) -> bool {
    match (crate::sources::parse_source_url(a), crate::sources::parse_source_url(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}
