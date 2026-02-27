mod apns;
mod mempool;
mod sse;

use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::apns::ApnsClient;
use crate::mempool::MempoolClient;
use crate::sse::watch_tx;

/// Estado compartilhado entre todos os handlers.
pub struct AppState {
    /// Cliente HTTP para consultar a API do mempool.space
    pub client: MempoolClient,
    /// Cliente APNS — usado internamente ao confirmar transações
    pub apns: Option<ApnsClient>,
}

#[tokio::main]
async fn main() {
    // Carrega .env.development ou .env.production conforme APP_ENV (padrão: development)
    let env = std::env::var("APP_ENV").unwrap_or_else(|_| "development".to_string());
    let env_file = format!(".env.{env}");
    match dotenvy::from_filename(&env_file) {
        Ok(path) => eprintln!("Loaded env from {}", path.display()),
        Err(e) => eprintln!("Warning: could not load {env_file}: {e}"),
    }

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "api_mempool=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let apns = match ApnsClient::from_env() {
        Ok(Some(client)) => {
            tracing::info!(
                production = client.production,
                bundle_id = %client.bundle_id,
                "APNS configurado — push será enviado ao confirmar transações"
            );
            Some(client)
        }
        Ok(None) => {
            tracing::warn!(
                "APNS não configurado — push notifications desabilitados. \
                 Defina APNS_KEY_ID, APNS_TEAM_ID, APNS_BUNDLE_ID e APNS_PRIVATE_KEY."
            );
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "Falha ao inicializar cliente APNS");
            None
        }
    };

    let state = Arc::new(AppState {
        client: MempoolClient::new(),
        apns,
    });

    let app = Router::new()
        // Monitora transação Bitcoin via SSE e envia push APNS ao confirmar
        .route("/tx/watch", post(watch_tx))
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
