use std::{env, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};

/// Runtime configuration, read from environment variables.
#[derive(Clone, Debug)]
pub struct Config {
    pub bind_addr: SocketAddr,
    /// Base URL under which Lidarr can reach this proxy (used for image URLs).
    pub public_url: String,
    pub database_path: PathBuf,
    pub image_dir: PathBuf,
    pub upstream_url: String,
    pub upstream_cache_ttl: Duration,
    /// How long data fetched from Deezer/Spotify is considered fresh.
    pub source_ttl: Duration,
    pub deezer_enabled: bool,
    pub deezer_url: String,
    pub spotify_client_id: Option<String>,
    pub spotify_client_secret: Option<String>,
    pub spotify_url: String,
    pub spotify_auth_url: String,
    pub itunes_enabled: bool,
    pub itunes_country: String,
    pub itunes_url: String,
    /// Edge of the square artwork requested from iTunes.
    pub itunes_artwork_size: u32,
    pub musicbrainz_url: String,
    pub mb_user_agent: String,
    /// Minimum interval between MusicBrainz requests (1 s per MB policy).
    pub mb_interval: Duration,
    pub reconcile_interval: Duration,
    pub prefetch_interval: Duration,
    /// Max number of Deezer/Spotify search hits checked against MusicBrainz per search.
    pub search_mb_lookups: usize,
    /// Max number of synthetic results added to a search response per source.
    pub search_limit: usize,
    pub enrich_mb_artists: bool,
}

fn var(name: &str) -> Option<String> {
    env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn var_or(name: &str, default: &str) -> String {
    var(name).unwrap_or_else(|| default.to_string())
}

fn bool_var(name: &str, default: bool) -> Result<bool> {
    match var(name) {
        None => Ok(default),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => anyhow::bail!("{name}: invalid boolean {v:?}"),
        },
    }
}

/// Parses durations such as `3600`, `90s`, `15m`, `24h`, `7d`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86400),
        _ => (s, 1),
    };
    let n: u64 = num.trim().parse().with_context(|| format!("invalid duration {s:?}"))?;
    Ok(Duration::from_secs(n * mult))
}

fn duration_var(name: &str, default: &str) -> Result<Duration> {
    parse_duration(&var_or(name, default)).with_context(|| name.to_string())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let bind_addr: SocketAddr = var_or("BIND_ADDR", "0.0.0.0:5001").parse().context("BIND_ADDR")?;
        let public_url = var("PUBLIC_URL")
            .unwrap_or_else(|| format!("http://localhost:{}", bind_addr.port()))
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            bind_addr,
            public_url,
            database_path: var_or("DATABASE_PATH", "/data/meta.db").into(),
            image_dir: var_or("IMAGE_DIR", "/data/images").into(),
            upstream_url: var_or("UPSTREAM_URL", "https://api.lidarr.audio/api/v0.4").trim_end_matches('/').to_string(),
            upstream_cache_ttl: duration_var("UPSTREAM_CACHE_TTL", "1h")?,
            source_ttl: duration_var("SOURCE_TTL", "7d")?,
            deezer_enabled: bool_var("DEEZER_ENABLED", true)?,
            deezer_url: var_or("DEEZER_URL", "https://api.deezer.com").trim_end_matches('/').into(),
            spotify_client_id: var("SPOTIFY_CLIENT_ID"),
            spotify_client_secret: var("SPOTIFY_CLIENT_SECRET"),
            spotify_url: var_or("SPOTIFY_URL", "https://api.spotify.com/v1").trim_end_matches('/').into(),
            spotify_auth_url: var_or("SPOTIFY_AUTH_URL", "https://accounts.spotify.com/api/token"),
            itunes_enabled: bool_var("ITUNES_ENABLED", true)?,
            itunes_country: var_or("ITUNES_COUNTRY", "it"),
            itunes_url: var_or("ITUNES_URL", "https://itunes.apple.com").trim_end_matches('/').into(),
            itunes_artwork_size: var_or("ITUNES_ARTWORK_SIZE", "3000").parse().context("ITUNES_ARTWORK_SIZE")?,
            musicbrainz_url: var_or("MB_URL", "https://musicbrainz.org/ws/2").trim_end_matches('/').into(),
            mb_user_agent: var_or(
                "MB_USER_AGENT",
                concat!(
                    "lidarr-meta-proxy/",
                    env!("CARGO_PKG_VERSION"),
                    " ( https://github.com/nemolo/lidarr-meta-proxy )"
                ),
            ),
            mb_interval: Duration::from_millis(var_or("MB_INTERVAL_MS", "1000").parse().context("MB_INTERVAL_MS")?),
            reconcile_interval: duration_var("RECONCILE_INTERVAL", "24h")?,
            prefetch_interval: duration_var("PREFETCH_INTERVAL", "1h")?,
            search_mb_lookups: var_or("SEARCH_MB_LOOKUPS", "5").parse().context("SEARCH_MB_LOOKUPS")?,
            search_limit: var_or("SEARCH_LIMIT", "10").parse().context("SEARCH_LIMIT")?,
            enrich_mb_artists: bool_var("ENRICH_MB_ARTISTS", true)?,
        })
    }

    pub fn spotify_enabled(&self) -> bool {
        self.spotify_client_id.is_some() && self.spotify_client_secret.is_some()
    }

    /// Configuration used by tests: everything points at `base` (a mock server).
    pub fn for_tests(base: &str, dir: &std::path::Path) -> Self {
        Self {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            public_url: "http://proxy.test".into(),
            database_path: dir.join("meta.db"),
            image_dir: dir.join("images"),
            upstream_url: format!("{base}/upstream"),
            upstream_cache_ttl: Duration::from_secs(3600),
            source_ttl: Duration::from_secs(86400),
            deezer_enabled: true,
            deezer_url: format!("{base}/deezer"),
            spotify_client_id: None,
            spotify_client_secret: None,
            spotify_url: format!("{base}/spotify"),
            spotify_auth_url: format!("{base}/spotify-token"),
            itunes_enabled: true,
            itunes_country: "it".into(),
            itunes_url: format!("{base}/itunes"),
            itunes_artwork_size: 3000,
            musicbrainz_url: format!("{base}/mb"),
            mb_user_agent: "lidarr-meta-proxy-tests/0".into(),
            mb_interval: Duration::ZERO,
            reconcile_interval: Duration::from_secs(86400),
            prefetch_interval: Duration::from_secs(3600),
            search_mb_lookups: 5,
            search_limit: 10,
            enrich_mb_artists: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86400));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604800));
        assert!(parse_duration("x").is_err());
    }
}
