-- Synthetic entities built from Deezer / Spotify (artists, albums, releases, tracks, recordings).
CREATE TABLE entity (
    id          TEXT PRIMARY KEY,           -- UUID v5 exposed to Lidarr
    kind        TEXT NOT NULL,              -- artist | album | release | track | recording
    source      TEXT NOT NULL,              -- deezer | spotify
    source_id   TEXT NOT NULL,
    parent_id   TEXT,                       -- album -> owning artist id (synthetic UUID or MBID)
    mbid        TEXT,                       -- set once reconciled with MusicBrainz
    payload     TEXT NOT NULL,              -- normalized source JSON
    fetched_at  INTEGER NOT NULL,           -- last time payload was refreshed from the source
    updated_at  INTEGER NOT NULL,           -- last change visible to Lidarr (drives /recent)
    UNIQUE (source, kind, source_id)
);
CREATE INDEX entity_mbid ON entity (mbid) WHERE mbid IS NOT NULL;
CREATE INDEX entity_updated ON entity (kind, updated_at);
CREATE INDEX entity_parent ON entity (parent_id);

-- Extra external identifiers per entity (deezer, spotify, upc, isrc).
CREATE TABLE source_link (
    entity_id   TEXT NOT NULL REFERENCES entity (id) ON DELETE CASCADE,
    source      TEXT NOT NULL,
    source_id   TEXT NOT NULL,
    PRIMARY KEY (entity_id, source, source_id)
);
CREATE INDEX source_link_lookup ON source_link (source, source_id);

-- Cache of api.lidarr.audio responses.
CREATE TABLE upstream_cache (
    route       TEXT PRIMARY KEY,
    body        BLOB NOT NULL,
    status      INTEGER NOT NULL,
    fetched_at  INTEGER NOT NULL,
    etag        TEXT
);

-- Generic cache for third-party JSON (MusicBrainz WS2, Deezer, Spotify, iTunes).
CREATE TABLE source_cache (
    key         TEXT PRIMARY KEY,
    body        TEXT NOT NULL,
    fetched_at  INTEGER NOT NULL
);

-- Images downloaded and served by the proxy, deduplicated by content hash.
CREATE TABLE image (
    hash        TEXT PRIMARY KEY,
    origin_url  TEXT NOT NULL,
    path        TEXT NOT NULL,
    width       INTEGER NOT NULL,
    height      INTEGER NOT NULL,
    mime        TEXT NOT NULL,
    fetched_at  INTEGER NOT NULL
);
CREATE INDEX image_origin ON image (origin_url);

-- Image chosen for an entity (synthetic UUID or MBID) and cover type.
CREATE TABLE entity_image (
    entity_id   TEXT NOT NULL,
    cover_type  TEXT NOT NULL,
    hash        TEXT NOT NULL REFERENCES image (hash),
    PRIMARY KEY (entity_id, cover_type)
);

-- History of synthetic -> MusicBrainz reconciliations.
CREATE TABLE reconcile_log (
    entity_id     TEXT NOT NULL,
    mbid          TEXT NOT NULL,
    kind          TEXT NOT NULL,
    reconciled_at INTEGER NOT NULL
);

-- State of MB artist discography enrichment (drives /recent/artist for MB artists).
CREATE TABLE enrichment (
    artist_mbid  TEXT PRIMARY KEY,
    albums_hash  TEXT NOT NULL,
    checked_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
