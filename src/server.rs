use axum::{routing::get, Json, Router};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::info;

use crate::stats::Stats;

pub async fn serve(stats: Arc<Stats>, port: u16) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/stats", get(stats_handler))
        .with_state(stats);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;
    info!("status server listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn root() -> &'static str {
    "rpow-miner OK\n"
}

async fn health() -> &'static str {
    "ok"
}

async fn stats_handler(
    axum::extract::State(stats): axum::extract::State<Arc<Stats>>,
) -> Json<crate::stats::StatsSnapshot> {
    Json(stats.snapshot())
}
