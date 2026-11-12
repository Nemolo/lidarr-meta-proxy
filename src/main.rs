use std::sync::Arc;

use anyhow::Result;
use lidarr_meta_proxy::{app::App, config::Config, reconcile::spawn_jobs, routes::router};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    // `lidarr-meta-proxy healthcheck` probes /health (used by Docker HEALTHCHECK,
    // the runtime image has no shell or curl).
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck().await;
    }

    let cfg = Config::from_env()?;
    tracing::info!(
        bind = %cfg.bind_addr,
        public_url = %cfg.public_url,
        upstream = %cfg.upstream_url,
        deezer = cfg.deezer_enabled,
        spotify = cfg.spotify_enabled(),
        itunes = cfg.itunes_enabled,
        lidarr_setup = cfg.lidarr_url.as_deref().unwrap_or("disabled"),
        "starting lidarr-meta-proxy"
    );
    let bind = cfg.bind_addr;
    let app = Arc::new(App::new(cfg).await?);
    spawn_jobs(app.clone());

    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn healthcheck() -> Result<()> {
    let bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:5001".into());
    let port = bind.rsplit(':').next().unwrap_or("5001");
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/health"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?;
    if resp.status().is_success() { Ok(()) } else { anyhow::bail!("unhealthy: {}", resp.status()) }
}
