use std::time::Duration;

use anyhow::Context;
use niscord_server::Config;
use tracing_subscriber::EnvFilter;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn env_list(name: &str, default: &str) -> Vec<String> {
    env(name)
        .unwrap_or_else(|| default.to_owned())
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let bind = env("NISCORD_BIND").unwrap_or_else(|| "0.0.0.0:8080".into());
    let turn_ttl = match env("NISCORD_TURN_TTL_SECONDS") {
        Some(v) => v.parse().context("NISCORD_TURN_TTL_SECONDS must be a number")?,
        None => 12 * 60 * 60,
    };
    let config = Config {
        password: env("NISCORD_PASSWORD").unwrap_or_default(),
        stun_urls: env_list("NISCORD_STUN_URLS", "stun:stun.l.google.com:19302"),
        turn_urls: env_list("NISCORD_TURN_URLS", ""),
        turn_secret: env("NISCORD_TURN_SECRET").unwrap_or_default(),
        turn_ttl: Duration::from_secs(turn_ttl),
    };

    let listener = tokio::net::TcpListener::bind(&bind).await.with_context(|| format!("binding {bind}"))?;
    tracing::info!("listening on ws://{}", listener.local_addr()?);
    if config.password.is_empty() {
        tracing::warn!("NISCORD_PASSWORD is not set: anyone who can reach this server can join");
    }
    if config.turn_urls.is_empty() || config.turn_secret.is_empty() {
        tracing::warn!("TURN is not configured: peers behind strict NATs may fail to connect");
    } else {
        tracing::info!(urls = ?config.turn_urls, "TURN enabled");
    }

    tokio::select! {
        res = niscord_server::run(listener, config) => res,
        _ = shutdown_signal() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
}

/// Ctrl+C, or SIGTERM from systemd/Docker on Unix.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            },
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
