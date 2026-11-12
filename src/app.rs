//! Application state and construction of synthetic Lidarr resources.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use serde_json::Value;

use crate::{
    config::Config,
    http::{SourceClient, build_http},
    ids::{Kind, Source, now, synthetic_id},
    images::Images,
    model::*,
    sources::{
        SrcAlbum, SrcArtist, SrcTrack, deezer::Deezer, itunes::Itunes, map_record_type, musicbrainz::MusicBrainz,
        spotify::Spotify,
    },
    store::{EntityRow, Store},
    upstream::{Upstream, UpstreamError},
};

pub struct App {
    pub cfg: Config,
    pub store: Store,
    pub upstream: Upstream,
    pub deezer: Option<Deezer>,
    pub spotify: Option<Spotify>,
    pub itunes: Option<Itunes>,
    pub mb: MusicBrainz,
    pub images: Images,
    pub lidarr_setup: Option<Arc<crate::lidarr::LidarrSetup>>,
    /// Recent interactive searches (sequence, normalized query), newest last, for debouncing.
    pub recent_searches: std::sync::Mutex<std::collections::VecDeque<(u64, String)>>,
}

pub type SharedApp = Arc<App>;

/// Errors mapped to the status codes Lidarr understands.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("not found")]
    NotFound,
    #[error("bad request")]
    BadRequest,
    #[error("unavailable: {0}")]
    Unavailable(String),
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Unavailable(format!("{e:#}"))
    }
}

impl From<UpstreamError> for ApiError {
    fn from(e: UpstreamError) -> Self {
        ApiError::Unavailable(e.to_string())
    }
}

/// Owner of a synthetic album as seen by Lidarr.
#[derive(Clone, Debug)]
pub struct Owner {
    pub artist: ArtistResource,
}

impl App {
    pub async fn new(cfg: Config) -> Result<Self> {
        let store = Store::open(&cfg.database_path).await?;
        store.check_namespace(&cfg.id_namespace.to_string()).await?;
        crate::ids::set_namespace(cfg.id_namespace)?;
        std::fs::create_dir_all(&cfg.image_dir)?;
        let ua = cfg.mb_user_agent.clone();
        let http = build_http(&ua, Duration::from_secs(30));
        let deezer = cfg.deezer_enabled.then(|| {
            // 50 requests / 5 s documented; keep a margin.
            let c = SourceClient::new("deezer", http.clone(), store.clone(), Some((40, Duration::from_secs(5))));
            Deezer::new(c, cfg.deezer_url.clone())
        });
        let spotify = match (&cfg.spotify_client_id, &cfg.spotify_client_secret) {
            (Some(id), Some(secret)) => {
                let c = SourceClient::new("spotify", http.clone(), store.clone(), Some((5, Duration::from_secs(1))));
                Some(Spotify::new(c, cfg.spotify_url.clone(), cfg.spotify_auth_url.clone(), id.clone(), secret.clone()))
            }
            _ => None,
        };
        let itunes = cfg.itunes_enabled.then(|| {
            // iTunes Search allows roughly 20 requests per minute.
            let c = SourceClient::new("itunes", http.clone(), store.clone(), Some((20, Duration::from_secs(60))));
            Itunes::new(c, cfg.itunes_url.clone(), cfg.itunes_country.clone(), cfg.itunes_artwork_size)
        });
        let mb_rate = if cfg.mb_interval.is_zero() { None } else { Some((1, cfg.mb_interval)) };
        let mb = MusicBrainz::new(
            SourceClient::new("musicbrainz", http.clone(), store.clone(), mb_rate),
            cfg.musicbrainz_url.clone(),
        );
        let upstream =
            Upstream::new(cfg.upstream_url.clone(), cfg.upstream_cache_ttl, store.clone(), &ua, cfg.upstream_rate);
        let images = Images::new(store.clone(), cfg.image_dir.clone(), cfg.public_url.clone(), &ua);
        let lidarr_setup =
            cfg.lidarr_setup()?.map(|(url, key, target)| Arc::new(crate::lidarr::LidarrSetup::new(url, key, target)));
        Ok(Self {
            cfg,
            store,
            upstream,
            deezer,
            spotify,
            itunes,
            mb,
            images,
            lidarr_setup,
            recent_searches: Default::default(),
        })
    }

    pub fn source_ttl(&self) -> i64 {
        self.cfg.source_ttl.as_secs() as i64
    }

    /// TTL for negative MusicBrainz lookups: shorter than the reconcile interval.
    pub fn mb_missing_ttl(&self) -> i64 {
        (self.cfg.reconcile_interval.as_secs() as i64 - 3600).max(3600)
    }

    fn is_stale(&self, row: &EntityRow) -> bool {
        now() - row.fetched_at > self.source_ttl()
    }

    // ------------------------------------------------------------- upstream

    /// Upstream JSON for `artist/{id}` or `album/{id}`: `Ok(None)` on 404.
    pub async fn upstream_entity(&self, route: &str) -> Result<Option<Value>, ApiError> {
        let r = self.upstream.get(route, &[]).await?;
        match r.status {
            200 => r.json().map(Some).ok_or_else(|| ApiError::Unavailable("invalid upstream JSON".into())),
            404 => Ok(None),
            400 => Err(ApiError::BadRequest),
            s => Err(ApiError::Unavailable(format!("upstream HTTP {s}"))),
        }
    }

    pub async fn upstream_artist(&self, mbid: &str) -> Result<Option<ArtistResource>, ApiError> {
        match self.upstream_entity(&format!("artist/{mbid}")).await? {
            Some(v) => Ok(Some(from_upstream(v).map_err(|e| ApiError::Unavailable(e.to_string()))?)),
            None => Ok(None),
        }
    }

    pub async fn upstream_album(&self, mbid: &str) -> Result<Option<AlbumResource>, ApiError> {
        match self.upstream_entity(&format!("album/{mbid}")).await? {
            Some(v) => Ok(Some(from_upstream(v).map_err(|e| ApiError::Unavailable(e.to_string()))?)),
            None => Ok(None),
        }
    }

    // --------------------------------------------------------- source data

    pub async fn fetch_artist(&self, source: Source, id: &str) -> Result<Option<SrcArtist>> {
        match source {
            Source::Deezer => match &self.deezer {
                Some(d) => d.artist(id).await,
                None => Ok(None),
            },
            Source::Spotify => match &self.spotify {
                Some(s) => s.artist(id).await,
                None => Ok(None),
            },
        }
    }

    pub async fn fetch_album(&self, source: Source, id: &str) -> Result<Option<SrcAlbum>> {
        match source {
            Source::Deezer => match &self.deezer {
                Some(d) => d.album(id).await,
                None => Ok(None),
            },
            Source::Spotify => match &self.spotify {
                Some(s) => s.album(id).await,
                None => Ok(None),
            },
        }
    }

    /// Discography summary, cached in `source_cache` for `SOURCE_TTL`.
    pub async fn discography(&self, source: Source, artist_id: &str) -> Result<Vec<SrcAlbum>> {
        let key = format!("discography:{}:{artist_id}", source.as_str());
        if let Some(body) = self.store.cache_get(&key, self.source_ttl()).await?
            && let Ok(list) = serde_json::from_str(&body)
        {
            return Ok(list);
        }
        let list = match source {
            Source::Deezer => match &self.deezer {
                Some(d) => d.artist_albums(artist_id).await?,
                None => vec![],
            },
            Source::Spotify => match &self.spotify {
                Some(s) => s.artist_albums(artist_id).await?,
                None => vec![],
            },
        };
        // Keep only releases where the artist is the main artist (drop "appears on").
        let list: Vec<SrcAlbum> =
            list.into_iter().filter(|a| a.artist_id.as_deref().map(|x| x == artist_id).unwrap_or(true)).collect();
        self.store.cache_put(&key, &serde_json::to_string(&list)?).await?;
        Ok(list)
    }

    /// Full album details (tracks, UPC) cached in `source_cache`.
    pub async fn album_details(&self, source: Source, id: &str) -> Result<Option<SrcAlbum>> {
        let key = format!("album:{}:{id}", source.as_str());
        if let Some(body) = self.store.cache_get(&key, self.source_ttl()).await?
            && let Ok(a) = serde_json::from_str::<Option<SrcAlbum>>(&body)
        {
            return Ok(a);
        }
        let a = self.fetch_album(source, id).await?;
        self.store.cache_put(&key, &serde_json::to_string(&a)?).await?;
        Ok(a)
    }

    // ------------------------------------------------------------ entities

    /// Stores a synthetic artist and its UUID.
    pub async fn save_artist(&self, a: &SrcArtist) -> Result<String> {
        let id = synthetic_id(a.source, Kind::Artist, &a.id);
        self.store.upsert_entity(&id, Kind::Artist, a.source, &a.id, None, a).await?;
        Ok(id)
    }

    /// Stores a synthetic album, merging with details already known (tracks, UPC).
    pub async fn save_album(&self, a: &SrcAlbum, parent: Option<&str>) -> Result<String> {
        let id = synthetic_id(a.source, Kind::Album, &a.id);
        let mut merged = a.clone();
        if let Some(existing) = self.store.get_entity(&id).await?
            && let Ok(old) = existing.payload::<SrcAlbum>()
        {
            merged.tracks = merged.tracks.or(old.tracks);
            merged.upc = merged.upc.or(old.upc);
            merged.label = merged.label.or(old.label);
            if merged.genres.is_empty() {
                merged.genres = old.genres;
            }
            if merged.images.is_empty() {
                merged.images = old.images;
            }
        }
        self.store.upsert_entity(&id, Kind::Album, a.source, &a.id, parent, &merged).await?;
        if let Some(upc) = merged.upc.as_deref().and_then(crate::ids::normalize_barcode) {
            self.store.add_link(&id, "upc", &upc).await?;
        }
        Ok(id)
    }

    /// Stores release/track/recording entities of a full album (needed to map
    /// MusicBrainz IDs onto them at reconciliation time).
    pub async fn save_album_children(&self, album_id: &str, a: &SrcAlbum) -> Result<()> {
        let release_id = synthetic_id(a.source, Kind::Release, &a.id);
        let rel_payload = serde_json::json!({ "album": a.id });
        self.store.upsert_entity(&release_id, Kind::Release, a.source, &a.id, Some(album_id), &rel_payload).await?;
        for t in a.tracks.iter().flatten() {
            let tid = synthetic_id(a.source, Kind::Track, &t.id);
            let payload = serde_json::json!({ "disc": t.disc, "position": t.position, "isrc": t.isrc });
            self.store.upsert_entity(&tid, Kind::Track, a.source, &t.id, Some(&release_id), &payload).await?;
            let rec_key = recording_key(t);
            let rid = synthetic_id(a.source, Kind::Recording, &rec_key);
            self.store.upsert_entity(&rid, Kind::Recording, a.source, &rec_key, Some(&release_id), &payload).await?;
            if let Some(isrc) = &t.isrc {
                self.store.add_link(&tid, "isrc", isrc).await?;
                self.store.add_link(&rid, "isrc", isrc).await?;
            }
        }
        Ok(())
    }

    /// Payload of an artist entity, refreshed from its source when stale.
    pub async fn artist_payload(&self, row: &EntityRow) -> Result<SrcArtist> {
        let current: SrcArtist = row.payload()?;
        if self.is_stale(row) {
            match self.fetch_artist(current.source, &current.id).await {
                Ok(Some(fresh)) => {
                    self.store.upsert_entity(&row.id, Kind::Artist, fresh.source, &fresh.id, None, &fresh).await?;
                    return Ok(fresh);
                }
                Ok(None) => tracing::info!(id = %row.id, "artist vanished from source, keeping last known data"),
                Err(e) => tracing::warn!(id = %row.id, error = %e, "artist refresh failed, using stored data"),
            }
        }
        Ok(current)
    }

    /// Payload of an album entity with tracks, fetched/refreshed when needed.
    pub async fn album_payload(&self, row: &EntityRow) -> Result<SrcAlbum, ApiError> {
        let current: SrcAlbum = row.payload()?;
        if current.tracks.is_none() || self.is_stale(row) {
            match self.fetch_album(current.source, &current.id).await {
                Ok(Some(fresh)) => {
                    self.save_album(&fresh, row.parent_id.as_deref()).await?;
                    self.save_album_children(&row.id, &fresh).await?;
                    return Ok(fresh);
                }
                Ok(None) if current.tracks.is_some() => {}
                Ok(None) => return Err(ApiError::NotFound),
                Err(e) if current.tracks.is_some() => {
                    tracing::warn!(error = %e, "album refresh failed, using stored data")
                }
                Err(e) => return Err(ApiError::Unavailable(e.to_string())),
            }
        }
        Ok(current)
    }

    // ------------------------------------------------------------- images

    /// Cached image for an entity; when missing, downloads it now (`wait`) or in background.
    pub async fn image_for(
        self: &Arc<Self>,
        entity_id: &str,
        cover_type: &str,
        candidates: Vec<String>,
        wait: bool,
    ) -> Option<ImageResource> {
        if let Some(img) = self.images.cached(entity_id, cover_type).await {
            return Some(img);
        }
        if candidates.is_empty() || self.images.recently_failed(entity_id, cover_type).await {
            return None;
        }
        if wait {
            self.images.assign(entity_id, cover_type, &candidates).await
        } else {
            let app = self.clone();
            let (id, ct) = (entity_id.to_string(), cover_type.to_string());
            tokio::spawn(async move {
                app.images.assign(&id, &ct, &candidates).await;
            });
            None
        }
    }

    /// Artwork candidates for an album, best first: iTunes (UPC, then exact artist+title),
    /// the source's own cover, then Spotify (same UPC).
    pub async fn album_cover_candidates(&self, a: &SrcAlbum) -> Vec<String> {
        let mut out = vec![];
        // Discography summaries carry no UPC: get it from the (cached) album details.
        let upc = match (&self.itunes, &a.upc) {
            (Some(_), None) => self.album_details(a.source, &a.id).await.ok().flatten().and_then(|f| f.upc),
            (_, upc) => upc.clone(),
        };
        if let Some(it) = &self.itunes {
            let found = match &upc {
                Some(upc) => it.artwork_by_upc(upc).await,
                None => Ok(None),
            };
            let found = match (found, &a.artist_name) {
                // No UPC hit: exact (normalized) artist + title match.
                (Ok(None), Some(artist)) => it.artwork_by_name(artist, &a.title).await,
                (r, _) => r,
            };
            match found {
                Ok(Some(u)) => out.push(u),
                Ok(None) => {}
                Err(e) => tracing::debug!(error = %e, "itunes lookup failed"),
            }
        }
        out.extend(a.images.iter().map(|i| i.url.clone()));
        // Spotify fallback for Deezer albums, matched by UPC only.
        if let (Source::Deezer, Some(sp), Some(upc)) = (a.source, &self.spotify, &upc) {
            match sp.album_images_by_upc(upc).await {
                Ok(urls) => out.extend(urls),
                Err(e) => tracing::debug!(error = %e, "spotify upc lookup failed"),
            }
        }
        out
    }

    // ---------------------------------------------------------- resources

    pub fn artist_links(a: &SrcArtist) -> Vec<LinkResource> {
        vec![LinkResource { target: a.link.clone(), kind: a.source.as_str().into(), extra: Default::default() }]
    }

    /// Synthetic artist resource without albums.
    pub async fn artist_resource(self: &Arc<Self>, id: &str, a: &SrcArtist, wait_images: bool) -> ArtistResource {
        let candidates = a.images.iter().map(|i| i.url.clone()).collect();
        let images = self.image_for(id, "poster", candidates, wait_images).await.into_iter().collect();
        ArtistResource {
            id: id.to_string(),
            artist_name: a.name.clone(),
            genres: a.genres.clone(),
            images,
            links: Self::artist_links(a),
            status: "active".into(),
            ..Default::default()
        }
    }

    /// Album summary (as listed in `ArtistResource.albums`), no releases.
    pub async fn album_summary(
        self: &Arc<Self>,
        id: &str,
        artist_id: &str,
        a: &SrcAlbum,
        wait_images: bool,
    ) -> AlbumResource {
        let (kind, secondary) = map_record_type(&a.record_type, a.tracks.as_ref().map(Vec::len));
        let images = if wait_images {
            let c = self.album_cover_candidates(a).await;
            self.image_for(id, "cover", c, true).await
        } else {
            // Listing: only already cached covers, download the rest in background.
            match self.images.cached(id, "cover").await {
                Some(i) => Some(i),
                None => {
                    if !self.images.recently_failed(id, "cover").await {
                        let app = self.clone();
                        let (id, a) = (id.to_string(), a.clone());
                        tokio::spawn(async move {
                            let c = app.album_cover_candidates(&a).await;
                            app.images.assign(&id, "cover", &c).await;
                        });
                    }
                    None
                }
            }
        };
        AlbumResource {
            id: id.to_string(),
            artist_id: artist_id.to_string(),
            title: a.title.clone(),
            kind: kind.to_string(),
            secondary_types: secondary,
            release_statuses: vec!["Official".into()],
            release_date: a.release_date.clone(),
            genres: a.genres.clone(),
            images: images.into_iter().collect(),
            links: vec![LinkResource {
                target: a.link.clone(),
                kind: a.source.as_str().into(),
                extra: Default::default(),
            }],
            ..Default::default()
        }
    }

    /// Full album resource with its single release and tracks.
    pub async fn album_resource(
        self: &Arc<Self>,
        id: &str,
        a: &SrcAlbum,
        owner: &Owner,
        wait_images: bool,
    ) -> AlbumResource {
        let artist_id = owner.artist.id.clone();
        let mut album = self.album_summary(id, &artist_id, a, wait_images).await;
        album.artists = vec![owner.artist.without_albums()];
        album.releases = Some(vec![release_resource(a, &artist_id)]);
        album
    }

    /// Resolves who owns a synthetic album (synthetic artist, or MB artist when enriched).
    pub async fn album_owner(self: &Arc<Self>, row: &EntityRow, a: &SrcAlbum) -> Result<Owner, ApiError> {
        if let Some(parent) = &row.parent_id {
            if let Some(prow) = self.store.get_entity(parent).await? {
                if prow.kind == "artist" {
                    // A reconciled synthetic artist is presented under its MBID.
                    if let Some(mbid) = &prow.mbid
                        && let Some(mb) = self.upstream_artist(mbid).await.ok().flatten()
                    {
                        return Ok(Owner { artist: mb.without_albums() });
                    }
                    let sa = self.artist_payload(&prow).await?;
                    return Ok(Owner { artist: self.artist_resource(&prow.id, &sa, false).await });
                }
            } else if let Some(mb) = self.upstream_artist(parent).await? {
                return Ok(Owner { artist: mb.without_albums() });
            }
        }
        // Album known only from search: attach it to the source's main artist.
        let (Some(aid), Some(name)) = (&a.artist_id, &a.artist_name) else {
            return Err(ApiError::Unavailable("album without artist".into()));
        };
        let artist = match self.fetch_artist(a.source, aid).await? {
            Some(sa) => sa,
            None => SrcArtist {
                source: a.source,
                id: aid.clone(),
                name: name.clone(),
                link: String::new(),
                genres: vec![],
                images: vec![],
            },
        };
        let artist_id = self.save_artist(&artist).await?;
        Ok(Owner { artist: self.artist_resource(&artist_id, &artist, false).await })
    }
}

/// Key of a recording: ISRC when known (shared across releases), else the track ID.
pub fn recording_key(t: &SrcTrack) -> String {
    t.isrc.clone().unwrap_or_else(|| format!("track-{}", t.id))
}

/// One release per synthetic album; media derived from disc numbers.
pub fn release_resource(a: &SrcAlbum, artist_id: &str) -> ReleaseResource {
    let tracks = a.tracks.clone().unwrap_or_default();
    let mut discs: Vec<i64> = tracks.iter().map(|t| t.disc).collect();
    discs.sort_unstable();
    discs.dedup();
    let media = discs
        .iter()
        .map(|d| MediumResource {
            name: String::new(),
            format: "Digital Media".into(),
            position: *d,
            extra: Default::default(),
        })
        .collect();
    let track_res: Vec<TrackResource> = tracks
        .iter()
        .enumerate()
        .map(|(n, t)| TrackResource {
            id: synthetic_id(a.source, Kind::Track, &t.id),
            recording_id: synthetic_id(a.source, Kind::Recording, &recording_key(t)),
            artist_id: artist_id.to_string(),
            track_name: t.title.clone(),
            track_number: t.position.to_string(),
            track_position: n as i64 + 1,
            medium_number: t.disc,
            duration_ms: t.duration_ms,
            explicit: t.explicit,
            ..Default::default()
        })
        .collect();
    ReleaseResource {
        id: synthetic_id(a.source, Kind::Release, &a.id),
        title: a.title.clone(),
        status: "Official".into(),
        release_date: a.release_date.clone(),
        country: vec![],
        label: a.label.clone().into_iter().collect(),
        media,
        track_count: track_res.len() as i64,
        tracks: track_res,
        ..Default::default()
    }
}
