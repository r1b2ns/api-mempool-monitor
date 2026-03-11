use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client as HttpClient;
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::debug;

use super::error::ApnsError;
use super::types::{LiveActivityContentState, LiveActivityEvent};

// ── JWT cache ─────────────────────────────────────────────────────────────────

/// Cached JWT token with the Unix timestamp (seconds) at which it was issued.
struct CachedToken {
    token:     String,
    issued_at: u64,
}

/// JWT lifetime before renewal.
///
/// Apple accepts JWTs up to 60 min old; we renew at 55 min to maintain a safety
/// margin and never exceed the 2 new tokens / 20 min per Team ID limit.
const TOKEN_TTL_SECS: u64 = 55 * 60;

// ── JWT claims ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ApnsClaims {
    iss: String,
    iat: u64,
}

// ── Client ────────────────────────────────────────────────────────────────────

/// APNS client built directly on HTTP/2 + JWT bearer authentication.
///
/// ### JWT cache
/// Apple limits new tokens to 2 per 20-minute window per Team ID.
/// Generating a JWT on every request triggers TooManyProviderTokenUpdates (429).
/// To avoid this, the client caches the JWT in memory and reuses it for up to
/// 55 minutes, renewing automatically only when necessary.
#[derive(Clone)]
pub struct ApnsClient {
    http: Arc<HttpClient>,
    key_id: String,
    team_id: String,
    /// Bundle ID of the iOS app (e.g. "com.example.myapp")
    pub bundle_id: String,
    /// `true` = production endpoint; `false` = sandbox
    pub production: bool,
    /// PEM content of the private key (.p8)
    pem: Arc<String>,
    /// Cached JWT — shared between all clones via Arc<Mutex>
    token_cache: Arc<Mutex<Option<CachedToken>>>,
}

impl ApnsClient {
    /// Creates a new `ApnsClient`. Intended to be called only by `ApnsBuilder`.
    pub(crate) fn new(
        key_id: String,
        team_id: String,
        bundle_id: String,
        production: bool,
        pem: String,
    ) -> Result<Self, ApnsError> {
        // Force HTTP/2 via ALPN using rustls — required by APNS
        let http = HttpClient::builder()
            .user_agent("api-mempool-monitor/0.1")
            .timeout(Duration::from_secs(10))
            .http2_prior_knowledge()
            .build()?;

        Ok(Self {
            http: Arc::new(http),
            key_id,
            team_id,
            bundle_id,
            production,
            pem: Arc::new(pem),
            token_cache: Arc::new(Mutex::new(None)),
        })
    }

    // ── JWT with cache ─────────────────────────────────────────────────────────

    /// Returns the current JWT, generating a new one only when the cache expires.
    ///
    /// **Apple rule:** max 2 new tokens per 20-min window per Team ID.
    /// We reuse the same JWT for TOKEN_TTL_SECS (55 min) to never exceed this
    /// limit even under heavy concurrent polling.
    async fn jwt_token(&self) -> Result<String, ApnsError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut cache = self.token_cache.lock().await;

        // Return cached token if still within TTL
        if let Some(ref cached) = *cache {
            if now.saturating_sub(cached.issued_at) < TOKEN_TTL_SECS {
                return Ok(cached.token.clone());
            }
            debug!("APNS JWT expired — renewing (age={}s)", now - cached.issued_at);
        } else {
            debug!("APNS JWT absent — generating for the first time");
        }

        // Generate a new JWT
        let claims = ApnsClaims { iss: self.team_id.clone(), iat: now };
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(self.key_id.clone());
        let key = jsonwebtoken::EncodingKey::from_ec_pem(self.pem.as_bytes())?;
        let token = jsonwebtoken::encode(&header, &claims, &key)?;

        *cache = Some(CachedToken { token: token.clone(), issued_at: now });
        Ok(token)
    }

    // ── Conventional alert push ────────────────────────────────────────────────

    /// Sends an alert notification via the APNS HTTP/2 API.
    ///
    /// Returns the `apns-id` assigned by Apple on success.
    pub async fn send_notification(
        &self,
        device_token: &str,
        title: Option<&str>,
        body: Option<&str>,
        badge: Option<u32>,
        sound: Option<&str>,
        custom_data: Option<&serde_json::Value>,
    ) -> Result<String, ApnsError> {
        let host = if self.production {
            "https://api.push.apple.com"
        } else {
            "https://api.sandbox.push.apple.com"
        };
        let url = format!("{host}/3/device/{device_token}");

        let token = self.jwt_token().await?;

        // Build the alert object
        let mut alert = serde_json::Map::new();
        if let Some(t) = title { alert.insert("title".into(), t.into()); }
        if let Some(b) = body  { alert.insert("body".into(),  b.into()); }

        // Build the aps object
        let mut aps = serde_json::json!({ "alert": alert });
        if let Some(n) = badge { aps["badge"] = serde_json::json!(n); }
        if let Some(s) = sound { aps["sound"] = serde_json::Value::String(s.into()); }

        // Build the root payload
        let mut payload = serde_json::json!({ "aps": aps });
        if let Some(data) = custom_data { payload["data"] = data.clone(); }

        self.post(&url, &token, &self.bundle_id, "alert", &payload).await
    }

    // ── Live Activity ──────────────────────────────────────────────────────────

    /// Sends a Live Activity update via the APNS HTTP/2 API.
    ///
    /// Uses the **activity push token** (different from the regular APNs device token),
    /// obtained from the Live Activity running on the device.
    ///
    /// ### Required APNS headers for Live Activity
    /// - `apns-push-type: liveactivity`
    /// - `apns-topic: <bundle_id>.push-type.liveactivity`
    pub async fn send_live_activity_update(
        &self,
        activity_token: &str,
        event: LiveActivityEvent,
        content_state: &LiveActivityContentState,
    ) -> Result<String, ApnsError> {
        let host = if self.production {
            "https://api.push.apple.com"
        } else {
            "https://api.sandbox.push.apple.com"
        };
        let url = format!("{host}/3/device/{activity_token}");

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let payload = serde_json::json!({
            "aps": {
                "event":         event.as_str(),
                "content-state": content_state,
                "timestamp":     timestamp
            }
        });

        let jwt = self.jwt_token().await?;

        // Live Activities require the topic in the format <bundle_id>.push-type.liveactivity
        let topic = format!("{}.push-type.liveactivity", self.bundle_id);

        self.post(&url, &jwt, &topic, "liveactivity", &payload).await
    }

    // ── HTTP helper ────────────────────────────────────────────────────────────

    /// POSTs to the APNS endpoint and interprets the response.
    async fn post(
        &self,
        url: &str,
        jwt: &str,
        topic: &str,
        push_type: &str,
        payload: &serde_json::Value,
    ) -> Result<String, ApnsError> {
        let response = self
            .http
            .post(url)
            .header("authorization", format!("bearer {jwt}"))
            .header("apns-topic",     topic)
            .header("apns-push-type", push_type)
            .json(payload)
            .send()
            .await?;

        let status = response.status();

        if status.is_success() {
            let apns_id = response
                .headers()
                .get("apns-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            Ok(apns_id)
        } else {
            let code = status.as_u16();
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            let reason = body["reason"].as_str().unwrap_or("Unknown").to_string();
            Err(ApnsError::Apns { code, reason })
        }
    }
}
