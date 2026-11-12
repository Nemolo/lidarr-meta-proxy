//! HTTP routes (the `{route}` part of Lidarr's `MetadataSource`).

use std::collections::HashMap;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, RawQuery, State},
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;

use crate::{
    app::{ApiError, SharedApp},
    ids::{Kind, now},
};

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "Not found".to_string()),
            ApiError::BadRequest => (StatusCode::BAD_REQUEST, "Bad request".to_string()),
            ApiError::Unavailable(m) => {
                tracing::warn!(error = %m, "answering 503");
                (StatusCode::SERVICE_UNAVAILABLE, m.clone())
            }
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

pub fn router(app: SharedApp) -> Router {
    Router::new()
        .route("/artist/{id}", get(artist))
        .route("/album/{id}", get(album))
        .route("/search", get(search))
        .route("/search/fingerprint", get(passthrough).post(passthrough))
        .route("/recent/artist", get(recent_artist))
        .route("/recent/album", get(recent_album))
        .route("/health", get(health))
        .route("/img/{file}", get(image))
        .fallback(passthrough)
        .with_state(app)
}

async fn artist(State(app): State<SharedApp>, Path(id): Path<String>) -> Result<Response, ApiError> {
    Ok(Json(app.get_artist(&id).await?).into_response())
}

async fn album(State(app): State<SharedApp>, Path(id): Path<String>) -> Result<Response, ApiError> {
    Ok(Json(app.get_album(&id).await?).into_response())
}

async fn search(
    State(app): State<SharedApp>,
    Query(q): Query<HashMap<String, String>>,
    uri: Uri,
) -> Result<Response, ApiError> {
    let query = q.get("query").map(|s| s.trim().to_string()).unwrap_or_default();
    let kind = q.get("type").map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    if query.is_empty() {
        return Ok(Json(json!([])).into_response());
    }
    match kind.as_str() {
        "artist" => Ok(Json(app.search_artists(&query).await?).into_response()),
        "album" => {
            let artist = q.get("artist").cloned().unwrap_or_default();
            let tracks = q.get("includeTracks").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false);
            Ok(Json(app.search_albums(&query, &artist, tracks).await?).into_response())
        }
        "all" => Ok(Json(app.search_all(&query).await?).into_response()),
        _ => forward(&app, Method::GET, uri.path(), uri.query(), &HeaderMap::new(), Bytes::new()).await,
    }
}

fn since_param(q: &HashMap<String, String>) -> Result<i64, ApiError> {
    match q.get("since") {
        Some(s) => s.trim().parse().map_err(|_| ApiError::BadRequest),
        None => Ok(now() - 86400),
    }
}

async fn recent_artist(
    State(app): State<SharedApp>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    Ok(Json(app.recent(Kind::Artist, since_param(&q)?).await?).into_response())
}

async fn recent_album(
    State(app): State<SharedApp>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    Ok(Json(app.recent(Kind::Album, since_param(&q)?).await?).into_response())
}

async fn health(State(app): State<SharedApp>) -> Response {
    let db = app.store.ping().await;
    let upstream = app.upstream.reachable().await;
    let status = if db { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    let lidarr = match &app.lidarr_setup {
        Some(s) => s.state.read().await.label(),
        None => "disabled".to_string(),
    };
    (
        status,
        Json(json!({
            "status": if db && upstream { "ok" } else if db { "degraded" } else { "error" },
            "database": db,
            "upstream": upstream,
            "deezer": app.deezer.is_some(),
            "spotify": app.spotify.is_some(),
            "itunes": app.itunes.is_some(),
            "lidarrSetup": lidarr,
            "musicbrainz": app.mb.status(),
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
        .into_response()
}

async fn image(State(app): State<SharedApp>, Path(file): Path<String>) -> Response {
    let hash = file.split('.').next().unwrap_or("");
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(Some(row)) = app.store.image(&hash.to_ascii_lowercase()).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(app.images.file_path(&row)).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, row.mime.clone()),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable".to_string()),
                (header::ETAG, format!("\"{}\"", row.hash)),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn passthrough(
    State(app): State<SharedApp>,
    method: Method,
    uri: Uri,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match forward(&app, method, uri.path(), q.as_deref(), &headers, body).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn forward(
    app: &SharedApp,
    method: Method,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
    let method = reqwest::Method::from_bytes(method.as_str().as_bytes()).map_err(|_| ApiError::BadRequest)?;
    let (status, ct, body) = app.upstream.forward(method, path, query, ct, body).await?;
    let mut resp = (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), body).into_response();
    if let Some(ct) = ct.and_then(|c| c.parse().ok()) {
        resp.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    Ok(resp)
}
