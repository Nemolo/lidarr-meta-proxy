//! Alternative metadata server for Lidarr: MusicBrainz passthrough with cache,
//! synthetic artists/albums from Deezer and Spotify, artwork, and automatic
//! reconciliation once entities appear on MusicBrainz.

pub mod app;
pub mod config;
pub mod enrich;
pub mod http;
pub mod ids;
pub mod images;
pub mod lidarr;
pub mod model;
pub mod reconcile;
pub mod routes;
pub mod service;
pub mod sources;
pub mod store;
pub mod upstream;
