mod apns;
mod mempool;
mod watcher;

use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};
use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::apns::ApnsClient;
use crate::mempool::MempoolClient;
use crate::watcher::{get_tx, watch_tx};

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

    let network = std::env::var("MEMPOOL_NETWORK").unwrap_or_else(|_| "mainnet".to_string());
    tracing::info!(network = %network, "Mempool network selected");

    // Log last 5 chars of APNS_PRIVATE_KEY for verification (safe — never exposes the full key)
    let apns_key_tail = std::env::var("APNS_PRIVATE_KEY")
        .ok()
        .and_then(|k| {
            let trimmed = k.trim().to_string();
            let chars: Vec<char> = trimmed.chars().collect();
            if chars.len() >= 5 {
                Some(chars[chars.len() - 5..].iter().collect::<String>())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "(not set)".to_string());
    tracing::info!(apns_key_tail = %apns_key_tail, "APNS_PRIVATE_KEY tail");

    let state = Arc::new(AppState {
        client: MempoolClient::new(),
        apns,
    });

    // Rate limiting: 30 requests/minute per IP, burst of 10
    let governor_config = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(2)   // 1 token replenished every 2s = 30 req/min
            .burst_size(10)  // allow burst of up to 10 requests
            .finish()
            .expect("Failed to build rate limiter config"),
    );
    tracing::info!("Rate limiter configured: 30 req/min per IP, burst 10");

    let app = Router::new()
        // Root — responds to HEAD for uptime checks
        .route("/", get(|| async { "ok" }))
        // Consulta o estado atual de uma transação
        .route("/tx/:txid", get(get_tx))
        // Registra transação para monitoramento em background; retorna 200 imediatamente
        .route("/tx/watch", post(watch_tx))
        // Health check
        .route("/health", get(|| async { "ok" }))
        .layer(GovernorLayer { config: governor_config })
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
