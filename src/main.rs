mod mempool;
mod sse;

use std::sync::Arc;

use axum::{
    routing::get,
    Router,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::mempool::MempoolClient;
use crate::sse::{watch_tx, AppState};

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "api_mempool=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let state = Arc::new(AppState {
        client: MempoolClient::new(),
    });

    let app = Router::new()
        // Endpoint principal de monitoramento via SSE
        .route("/tx/:txid/watch", get(watch_tx))
        // Health check
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    let addr = "0.0.0.0:3000";
    tracing::info!("Listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind port 3000");

    axum::serve(listener, app)
        .await
        .expect("Server error");
}
