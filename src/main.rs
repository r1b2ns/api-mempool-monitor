mod apns;
mod mempool;
mod push;
mod sse;

use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::apns::ApnsClient;
use crate::mempool::MempoolClient;
use crate::push::send_apns_push;
use crate::sse::watch_tx;

/// Estado compartilhado entre todos os handlers.
pub struct AppState {
    /// Cliente HTTP para consultar a API do mempool.space
    pub client: MempoolClient,
    /// Cliente APNS (disponível apenas quando as variáveis de ambiente estão configuradas)
    pub apns: Option<ApnsClient>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "api_mempool=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let apns = match ApnsClient::from_env() {
        Ok(Some(client)) => {
            tracing::info!(
                production = client.production,
                bundle_id = %client.bundle_id,
                "APNS configurado"
            );
            Some(client)
        }
        Ok(None) => {
            tracing::warn!(
                "APNS não configurado — endpoint POST /push/apns retornará 503. \
                 Defina APNS_KEY_ID, APNS_TEAM_ID, APNS_BUNDLE_ID e APNS_PRIVATE_KEY_PATH."
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
        // Monitoramento de transação Bitcoin via Server-Sent Events
        .route("/tx/:txid/watch", get(watch_tx))
        // Envio de push notification via APNS
        .route("/push/apns", post(send_apns_push))
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
