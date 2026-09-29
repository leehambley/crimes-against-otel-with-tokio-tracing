//! Service plumbing shared by gateway, store and stats.

pub mod chaos;
pub mod error;
pub mod valkey;

use std::env;

use anyhow::Context as _;
use axum::{Router, routing::get};

pub use chaos::Chaos;
pub use error::AppError;
pub use valkey::Valkey;

pub fn env_or(name: &str, default: &str) -> String {
    env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

/// Serves `router` on `LISTEN_ADDR` until SIGINT/SIGTERM. `/healthz` is
/// added outside the tracing middleware so probes don't produce traces.
pub async fn serve(router: Router, default_addr: &str) -> anyhow::Result<()> {
    let addr = env_or("LISTEN_ADDR", default_addr);
    let router = router.route("/healthz", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("shut down");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {},
        _ = term.recv() => {},
    }
}
