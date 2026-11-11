//! Deterministic identifiers for synthetic entities and small text helpers.

use std::sync::OnceLock;

use uuid::Uuid;

/// Default namespace for UUID v5 generation (overridable with `UUID_NAMESPACE`).
/// Every synthetic ID stored in a Lidarr database derives from the namespace in
/// use, so it must not change once Lidarr has added synthetic entities.
pub const DEFAULT_NAMESPACE: Uuid = Uuid::from_u128(0x6c1d_a2f0_5b7e_4c3a_9f1e_2d8b_0a6c_4e51);

static NAMESPACE: OnceLock<Uuid> = OnceLock::new();

/// Namespace from configuration: a UUID is used as is, any other string is a seed
/// turned into a namespace with `uuid5(NAMESPACE_OID, seed)`.
pub fn parse_namespace(value: &str) -> Uuid {
    let v = value.trim();
    Uuid::parse_str(v).unwrap_or_else(|_| Uuid::new_v5(&Uuid::NAMESPACE_OID, v.as_bytes()))
}

/// Sets the process-wide namespace. Fails if a different one was already set.
pub fn set_namespace(ns: Uuid) -> anyhow::Result<()> {
    let current = *NAMESPACE.get_or_init(|| ns);
    anyhow::ensure!(current == ns, "UUID namespace already set to {current}, cannot switch to {ns}");
    Ok(())
}

/// Namespace in use ([`DEFAULT_NAMESPACE`] unless configured).
pub fn namespace() -> Uuid {
    *NAMESPACE.get_or_init(|| DEFAULT_NAMESPACE)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    Deezer,
    Spotify,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Deezer => "deezer",
            Source::Spotify => "spotify",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "deezer" => Some(Source::Deezer),
            "spotify" => Some(Source::Spotify),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Artist,
    Album,
    Release,
    Track,
    Recording,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Artist => "artist",
            Kind::Album => "album",
            Kind::Release => "release",
            Kind::Track => "track",
            Kind::Recording => "recording",
        }
    }
}

/// `uuid5(namespace, "<source>:<kind>:<source id>")`.
pub fn synthetic_id(source: Source, kind: Kind, source_id: &str) -> String {
    Uuid::new_v5(&namespace(), format!("{}:{}:{}", source.as_str(), kind.as_str(), source_id).as_bytes()).to_string()
}

/// Canonical lowercase hyphenated form, or `None` if `s` is not a UUID.
pub fn parse_uuid(s: &str) -> Option<String> {
    Uuid::parse_str(s.trim()).ok().map(|u| u.to_string())
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Normalized form used for name/title comparisons: ASCII-folded, lowercase,
/// edition qualifiers in brackets removed, punctuation collapsed.
pub fn normalize_title(s: &str) -> String {
    let folded = deunicode::deunicode(s).to_lowercase().replace('&', " and ");
    // Drop bracketed qualifiers such as "(Deluxe Edition)", "[Remastered 2011]".
    let mut out = String::with_capacity(folded.len());
    let mut depth = 0usize;
    for c in folded.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    // Drop trailing " - Remastered 2011" / " - Single" / " - EP" style suffixes.
    let base = match out.find(" - ") {
        Some(i) => {
            let tail = &out[i + 3..];
            const QUALIFIERS: &[&str] = &[
                "remaster",
                "deluxe",
                "single",
                "ep",
                "live",
                "expanded",
                "edition",
                "version",
                "anniversary",
                "bonus",
            ];
            if QUALIFIERS.iter().any(|q| tail.contains(q)) { out[..i].to_string() } else { out }
        }
        None => out,
    };
    let base = base.strip_prefix("the ").unwrap_or(&base).to_string();
    base.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

/// Year part of a `yyyy[-mm[-dd]]` date.
pub fn year_of(date: Option<&str>) -> Option<i32> {
    date.and_then(|d| d.get(0..4)).and_then(|y| y.parse().ok()).filter(|y| *y > 0)
}

/// Normalizes a release date to `yyyy-MM-dd` (Lidarr parses it as DateTime).
pub fn normalize_date(date: &str) -> Option<String> {
    let d = date.trim();
    if d.is_empty() || d.starts_with("0000") {
        return None;
    }
    match d.len() {
        4 => Some(format!("{d}-01-01")),
        7 => Some(format!("{d}-01")),
        _ if d.len() >= 10 => Some(d[..10].to_string()),
        _ => None,
    }
}

/// Barcodes compared without leading zeros (UPC-A vs EAN-13).
pub fn normalize_barcode(code: &str) -> Option<String> {
    let digits: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    let trimmed = digits.trim_start_matches('0');
    if trimmed.len() < 6 { None } else { Some(trimmed.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_v5() {
        let a = synthetic_id(Source::Deezer, Kind::Artist, "27");
        assert_eq!(a, synthetic_id(Source::Deezer, Kind::Artist, "27"));
        assert_ne!(a, synthetic_id(Source::Deezer, Kind::Album, "27"));
        assert_ne!(a, synthetic_id(Source::Spotify, Kind::Artist, "27"));
        assert_eq!(Uuid::parse_str(&a).unwrap().get_version_num(), 5);
    }

    #[test]
    fn namespace_parsing() {
        let u = "6c1da2f0-5b7e-4c3a-9f1e-2d8b0a6c4e51";
        assert_eq!(parse_namespace(u), DEFAULT_NAMESPACE);
        assert_eq!(parse_namespace("my-seed"), parse_namespace(" my-seed "));
        assert_ne!(parse_namespace("my-seed"), parse_namespace("other-seed"));
        assert_ne!(parse_namespace("my-seed"), DEFAULT_NAMESPACE);
    }

    #[test]
    fn titles() {
        assert_eq!(normalize_title("The Wall (Remastered 2011)"), "wall");
        assert_eq!(normalize_title("Discovery - Deluxe Edition"), "discovery");
        assert_eq!(normalize_title("Café Tacvba & Friends!"), "cafe tacvba and friends");
        assert_eq!(normalize_title("A - B"), "a b");
    }

    #[test]
    fn dates_and_barcodes() {
        assert_eq!(normalize_date("2001"), Some("2001-01-01".into()));
        assert_eq!(normalize_date("2001-03-12"), Some("2001-03-12".into()));
        assert_eq!(normalize_date("0000-00-00"), None);
        assert_eq!(year_of(Some("2001-03-12")), Some(2001));
        assert_eq!(normalize_barcode("0724384960650"), normalize_barcode("724384960650"));
    }
}
