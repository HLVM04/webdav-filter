use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::{net::TcpListener, signal};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use webdav_filter::{
    config::Config,
    filter::Classifier,
    index::Index,
    server::{AppState, router},
    store::SnapshotStore,
    upstream::Upstream,
};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[arg(short, long, default_value = "config.yml")]
    config: PathBuf,
    #[arg(long)]
    check_config: bool,
    #[arg(long)]
    healthcheck: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("webdav_filter=info")),
        )
        .json()
        .init();
    let args = Args::parse();
    if let Some(url) = args.healthcheck {
        let response = reqwest::get(&url).await.context("healthcheck request")?;
        if !response.status().is_success() {
            anyhow::bail!("healthcheck returned {}", response.status());
        }
        return Ok(());
    }
    let config = Config::load(&args.config)?;
    let classifier = Classifier::compile(&config.directories)?;
    if args.check_config {
        println!("configuration is valid");
        return Ok(());
    }

    let upstream = Upstream::new(&config.upstream, config.upstream_password()?)?;
    let store = SnapshotStore::open(&config.state.database)?;
    let index = Index::new(&config, upstream.clone(), classifier, store)?;
    let downstream_auth = match (&config.downstream_auth, config.downstream_password()?) {
        (Some(auth), Some(password)) => Some((auth.username.clone(), password)),
        (Some(_), None) => {
            anyhow::bail!("downstream_auth requires a password_env or password_file")
        }
        (None, _) => None,
    };
    if !config.listen.host.is_loopback() && downstream_auth.is_none() {
        warn!("anonymous WebDAV is listening on a non-loopback address");
    }
    if !config.listen.host.is_loopback() && config.delete.enabled {
        warn!("DELETE is enabled on a non-loopback address");
    }

    let metrics = PrometheusBuilder::new()
        .install_recorder()
        .context("install metrics recorder")?;
    let state = AppState {
        index: index.clone(),
        upstream,
        downstream_auth,
        delete_enabled: config.delete.enabled,
        metrics,
    };
    let app = router(state);
    let address = SocketAddr::new(config.listen.host, config.listen.port);
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("bind {address}"))?;
    info!(%address, "webdav-filter listening");

    let refresh_index = index.clone();
    let interval = config.interval();
    tokio::spawn(async move {
        if let Err(error) = refresh_index.refresh().await {
            error!(%error, "initial refresh failed; serving persisted snapshot if available");
        }
        loop {
            tokio::time::sleep(interval).await;
            if let Err(error) = refresh_index.refresh().await {
                error!(%error, "scheduled refresh failed; keeping previous snapshot");
            }
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
        .context("serve WebDAV")?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("install Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    info!("shutdown signal received");
}
