mod apns;
mod mempool;
mod watcher;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use tower_governor::{
    governor::GovernorConfigBuilder,
    key_extractor::KeyExtractor,
    GovernorLayer,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::apns::ApnsClient;
use crate::mempool::MempoolClient;
use crate::watcher::{get_tx, watch_tx};

/// Extracts the real client IP from the X-Real-IP header (set by Nginx).
/// Falls back to the socket peer address when the header is absent (direct connections).
#[derive(Clone)]
struct RealIpKeyExtractor;

impl KeyExtractor for RealIpKeyExtractor {
    type Key = IpAddr;

    fn extract<T>(&self, req: &axum::http::Request<T>) -> Result<Self::Key, tower_governor::GovernorError> {
        // Prefer X-Real-IP set by Nginx reverse proxy
        if let Some(ip) = req.headers()
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<IpAddr>().ok())
        {
            return Ok(ip);
        }

        // Fallback: socket peer address via ConnectInfo
        req.extensions()
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip())
            .ok_or(tower_governor::GovernorError::UnableToExtractKey)
    }
}

/// Shared state across all handlers.
pub struct AppState {
    /// HTTP client for querying the mempool.space API
    pub client: MempoolClient,
    /// APNS client — used internally when confirming transactions
    pub apns: Option<ApnsClient>,
    /// Expected value of the X-API-Key header
    pub api_key: String,
}

/// Validates the X-API-Key header on every request.
/// Returns 401 Unauthorized when the header is missing or does not match.
async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let provided = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if provided == state.api_key {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Unauthorized" })),
        )
            .into_response()
    }
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

    let api_key = std::env::var("API_KEY")
        .expect("API_KEY must be set in the environment");
    let key_tail = &api_key[api_key.len().saturating_sub(4)..];
    tracing::info!(key_tail = %key_tail, "API Key loaded (****<last4>)");

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
        api_key,
    });

    // Rate limiting: 30 requests/minute per IP, burst of 10
    let governor_config = Arc::new(
        GovernorConfigBuilder::default()
            .key_extractor(RealIpKeyExtractor)
            .per_second(2)   // 1 token replenished every 2s = 30 req/min
            .burst_size(10)  // allow burst of up to 10 requests
            .finish()
            .expect("Failed to build rate limiter config"),
    );
    tracing::info!("Rate limiter configured: 30 req/min per IP, burst 10");

    let app = Router::new()
        // Root — responds to HEAD for uptime checks
        .route("/", get(|| async { "ok" }))
        // Query the current status of a transaction
        .route("/tx/:txid", get(get_tx))
        // Register a transaction for background monitoring; returns 200 immediately
        .route("/tx/watch", post(watch_tx))
        // Health check
        .route("/health", get(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .layer(GovernorLayer { config: governor_config })
        .with_state(state);

    let addr = "0.0.0.0:3000";
    tracing::info!("Listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind port 3000");

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .expect("Server error");
}
