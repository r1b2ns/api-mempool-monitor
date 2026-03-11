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
use governor::middleware::NoOpMiddleware;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::apns::{ApnsBuilder, ApnsClient};
use crate::mempool::MempoolClient;
use crate::watcher::{get_tx, watch_tx};

// ── Rate limiter ──────────────────────────────────────────────────────────────

/// Extracts the real client IP from the X-Real-IP header (set by Nginx).
/// Falls back to the socket peer address when the header is absent (direct connections).
#[derive(Clone)]
struct RealIpKeyExtractor;

impl KeyExtractor for RealIpKeyExtractor {
    type Key = IpAddr;

    fn extract<T>(&self, req: &axum::http::Request<T>) -> Result<Self::Key, tower_governor::GovernorError> {
        // Prefer X-Real-IP set by the Nginx reverse proxy
        if let Some(ip) = req.headers()
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<IpAddr>().ok())
        {
            return Ok(ip);
        }

        // Fallback: socket peer address via ConnectInfo (used for direct connections)
        req.extensions()
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip())
            .ok_or(tower_governor::GovernorError::UnableToExtractKey)
    }
}

/// Builds the rate limiter layer: 30 req/min per IP with a burst of 10.
fn build_rate_limiter() -> GovernorLayer<RealIpKeyExtractor, NoOpMiddleware> {
    let config = Arc::new(
        GovernorConfigBuilder::default()
            .key_extractor(RealIpKeyExtractor)
            .per_second(2)   // 1 token replenished every 2s = 30 req/min
            .burst_size(10)  // allow an initial burst of up to 10 requests
            .finish()
            .expect("Failed to build rate limiter config"),
    );
    tracing::info!("Rate limiter configured: 30 req/min per IP, burst 10");
    GovernorLayer { config }
}

// ── App state ─────────────────────────────────────────────────────────────────

/// Shared state across all handlers.
pub struct AppState {
    /// HTTP client for querying the mempool.space API
    pub client: MempoolClient,
    /// APNS client — used internally when confirming transactions
    pub apns: Option<ApnsClient>,
    /// Expected value of the X-API-Key header
    pub api_key: String,
}

// ── Auth middleware ───────────────────────────────────────────────────────────

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

// ── Environment setup ─────────────────────────────────────────────────────────

/// Loads the .env file for the current APP_ENV and initializes the tracing subscriber.
///
/// The active environment is resolved in priority order:
///   1. `APP_ENV` environment variable (explicit override)
///   2. Cargo build profile: debug build → "development", release build → "production"
///
/// This ensures `cargo run` uses dev config and `cargo build --release` uses prod config
/// without requiring APP_ENV to be set manually. The systemd service files may still
/// set APP_ENV explicitly to override this behaviour.
fn setup_env() {
    // cfg!(debug_assertions) is true for debug builds and false for release builds
    let default_env = if cfg!(debug_assertions) { "development" } else { "production" };
    let env = std::env::var("APP_ENV").unwrap_or_else(|_| default_env.to_string());
    let env_file = format!(".env.{env}");
    match dotenvy::from_filename(&env_file) {
        Ok(path) => eprintln!("Loaded env from {}", path.display()),
        Err(e)   => eprintln!("Warning: could not load {env_file}: {e}"),
    }

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "api_mempool=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    setup_env();

    let apns = match ApnsBuilder::build() {
        Ok(Some(client)) => {
            tracing::info!(
                production = client.production,
                bundle_id  = %client.bundle_id,
                "APNS configured — push will be sent on transaction confirmation"
            );
            Some(client)
        }
        Ok(None) => {
            tracing::warn!(
                "APNS not configured — push notifications disabled. \
                 Set APNS_KEY_ID, APNS_TEAM_ID, APNS_BUNDLE_ID, and APNS_PRIVATE_KEY."
            );
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to initialize APNS client");
            None
        }
    };

    let network = std::env::var("MEMPOOL_NETWORK").unwrap_or_else(|_| "mainnet".to_string());
    tracing::info!(network = %network, "Mempool network selected");

    let api_key  = std::env::var("API_KEY").expect("API_KEY must be set in the environment");
    let key_tail = &api_key[api_key.len().saturating_sub(4)..];
    tracing::info!(key_tail = %key_tail, "API Key loaded (****<last4>)");

    let state = Arc::new(AppState {
        client: MempoolClient::new(),
        apns,
        api_key,
    });

    // Public routes — no API key required (used by uptime monitors)
    let public = Router::new()
        .route("/", get(|| async { "ok" }));

    // Protected routes — require a valid X-API-Key header
    let protected = Router::new()
        .route("/tx/:txid", get(get_tx))
        .route("/tx/watch", post(watch_tx))
        .route("/health", get(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    let app = Router::new()
        .merge(public)
        .merge(protected)
        .layer(build_rate_limiter())
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
