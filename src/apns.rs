use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client as HttpClient;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::debug;

#[derive(Debug, Error)]
pub enum ApnsError {
    #[error("Erro HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Erro ao gerar JWT: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("APNS rejeitou a notificação (HTTP {code}): {reason}")]
    Apns { code: u16, reason: String },
}

// ── Live Activity ─────────────────────────────────────────────────────────────

/// Tipo do evento enviado à Live Activity.
///
/// | Valor    | Quando usar                                             |
/// |----------|---------------------------------------------------------|
/// | `Update` | Atualiza o `ContentState` mantendo a activity ativa     |
/// | `End`    | Atualiza o `ContentState` e encerra a activity          |
#[derive(Debug, Clone, Copy)]
pub enum LiveActivityEvent {
    Update,
    End,
}

impl LiveActivityEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::End    => "end",
        }
    }
}

/// Estado dinâmico da Live Activity — espelha o `ContentState` do Swift.
///
/// Serializado como `content-state` no payload APNS.
/// Campos opcionais usam `skip_serializing_if` para não aparecerem
/// no JSON quando não disponíveis (ex.: tx recém-propagada sem fee).
#[derive(Debug, Serialize)]
pub struct LiveActivityContentState {
    /// Número de confirmações (0 = pendente, ≥1 = confirmada)
    pub confirmations: u32,
    /// Status textual: "pending" | "confirmed" | "failed"
    pub status: String,
    /// TXID da transação
    #[serde(rename = "txId")]
    pub tx_id: String,
    /// Valor total transferido em BTC (soma dos outputs, satoshis ÷ 1e8)
    #[serde(rename = "valueBtc", skip_serializing_if = "Option::is_none")]
    pub value_btc: Option<f64>,
    /// Taxa paga em satoshis
    #[serde(rename = "feeSats", skip_serializing_if = "Option::is_none")]
    pub fee_sats: Option<u64>,
}

// ── JWT cache ─────────────────────────────────────────────────────────────────

/// Token JWT em cache com o instante em que foi emitido (Unix timestamp, segundos).
struct CachedToken {
    token:     String,
    issued_at: u64,
}

/// Tempo de vida do token reutilizável.
///
/// A Apple aceita JWTs com até 60 min de idade; renovamos aos 55 min para
/// manter margem de segurança e nunca ultrapassar o limite de 2 tokens / 20 min.
const TOKEN_TTL_SECS: u64 = 55 * 60; // 55 minutos

// ── Claims JWT ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ApnsClaims {
    iss: String,
    iat: u64,
}

// ── Cliente ───────────────────────────────────────────────────────────────────

/// Cliente APNS implementado diretamente sobre HTTP/2 + JWT.
///
/// ### Cache de JWT
/// A Apple limita a **2 tokens novos por janela de 20 minutos** por Team ID.
/// Gerar um JWT a cada request dispara `TooManyProviderTokenUpdates` (429).
/// Para evitar isso, o cliente armazena o JWT em memória e o reutiliza por
/// até 55 minutos, renovando-o automaticamente só quando necessário.
#[derive(Clone)]
pub struct ApnsClient {
    http: Arc<HttpClient>,
    key_id: String,
    team_id: String,
    /// Bundle ID do app iOS (ex.: "com.example.myapp")
    pub bundle_id: String,
    /// `true` = endpoint de produção; `false` = sandbox
    pub production: bool,
    /// Conteúdo PEM da chave privada (.p8)
    pem: Arc<String>,
    /// JWT em cache — compartilhado entre todos os clones via Arc<Mutex>
    token_cache: Arc<Mutex<Option<CachedToken>>>,
}

impl ApnsClient {
    /// Tenta construir um `ApnsClient` a partir de variáveis de ambiente.
    ///
    /// ### Variáveis obrigatórias
    /// | Variável           | Descrição                                        |
    /// |--------------------|--------------------------------------------------|
    /// | `APNS_KEY_ID`      | Key ID do portal Apple Developer                 |
    /// | `APNS_TEAM_ID`     | Team/Organization ID                             |
    /// | `APNS_BUNDLE_ID`   | Bundle ID do app (ex.: com.example.myapp)        |
    /// | `APNS_PRIVATE_KEY` | Conteúdo PEM da chave `.p8` (texto, não caminho) |
    ///
    /// ### Variável opcional
    /// | Variável           | Padrão  | Descrição                      |
    /// |--------------------|---------|--------------------------------|
    /// | `APNS_PRODUCTION`  | `false` | `true` para usar produção      |
    ///
    /// Retorna `Ok(None)` se alguma variável obrigatória não estiver definida.
    pub fn from_env() -> Result<Option<Self>, ApnsError> {
        let (key_id, team_id, bundle_id) = match (
            std::env::var("APNS_KEY_ID").ok(),
            std::env::var("APNS_TEAM_ID").ok(),
            std::env::var("APNS_BUNDLE_ID").ok(),
        ) {
            (Some(k), Some(t), Some(b)) => (k, t, b),
            _ => return Ok(None),
        };

        let pem = match std::env::var("APNS_PRIVATE_KEY") {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };

        let production = std::env::var("APNS_PRODUCTION")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        // Força HTTP/2 via ALPN usando rustls — obrigatório para APNS
        let http = HttpClient::builder()
            .user_agent("api-mempool-monitor/0.1")
            .timeout(Duration::from_secs(10))
            .http2_prior_knowledge()
            .build()?;

        Ok(Some(Self {
            http: Arc::new(http),
            key_id,
            team_id,
            bundle_id,
            production,
            pem: Arc::new(pem),
            token_cache: Arc::new(Mutex::new(None)),
        }))
    }

    // ── JWT com cache ─────────────────────────────────────────────────────────

    /// Retorna o JWT vigente, gerando um novo somente quando o cache expirar.
    ///
    /// **Regra Apple:** máx. 2 tokens novos por janela de 20 min por Team ID.
    /// Reutilizamos o mesmo JWT por `TOKEN_TTL_SECS` (55 min) para nunca
    /// ultrapassar esse limite mesmo com muitos polls simultâneos.
    async fn jwt_token(&self) -> Result<String, ApnsError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut cache = self.token_cache.lock().await;

        // Devolve o token em cache se ainda estiver dentro do TTL
        if let Some(ref cached) = *cache {
            if now.saturating_sub(cached.issued_at) < TOKEN_TTL_SECS {
                return Ok(cached.token.clone());
            }
            debug!("JWT APNS expirado — renovando (age={}s)", now - cached.issued_at);
        } else {
            debug!("JWT APNS ausente — gerando pela primeira vez");
        }

        // Gera novo JWT
        let claims = ApnsClaims { iss: self.team_id.clone(), iat: now };
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(self.key_id.clone());
        let key = jsonwebtoken::EncodingKey::from_ec_pem(self.pem.as_bytes())?;
        let token = jsonwebtoken::encode(&header, &claims, &key)?;

        *cache = Some(CachedToken { token: token.clone(), issued_at: now });
        Ok(token)
    }

    // ── Push de alerta convencional ───────────────────────────────────────────

    /// Envia uma notificação de alerta via APNS HTTP/2 API.
    ///
    /// Retorna o `apns-id` atribuído pela Apple em caso de sucesso.
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

        // Monta o objeto alert
        let mut alert = serde_json::Map::new();
        if let Some(t) = title { alert.insert("title".into(), t.into()); }
        if let Some(b) = body  { alert.insert("body".into(),  b.into()); }

        // Monta o objeto aps
        let mut aps = serde_json::json!({ "alert": alert });
        if let Some(n) = badge { aps["badge"] = serde_json::json!(n); }
        if let Some(s) = sound { aps["sound"] = serde_json::Value::String(s.into()); }

        // Payload raiz
        let mut payload = serde_json::json!({ "aps": aps });
        if let Some(data) = custom_data { payload["data"] = data.clone(); }

        self.post(&url, &token, &self.bundle_id, "alert", &payload).await
    }

    // ── Live Activity ─────────────────────────────────────────────────────────

    /// Envia uma atualização de Live Activity via APNS HTTP/2.
    ///
    /// Usa o **activity push token** (diferente do device token APNs regular),
    /// que é obtido pela Live Activity em execução no dispositivo.
    ///
    /// ### Payload gerado
    /// ```json
    /// {
    ///   "aps": {
    ///     "event": "update",
    ///     "content-state": { "confirmations": 0, "status": "pending" },
    ///     "timestamp": 1712345678
    ///   }
    /// }
    /// ```
    ///
    /// ### Headers APNS obrigatórios para Live Activity
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

        // Live Activities exigem topic no formato <bundle_id>.push-type.liveactivity
        let topic = format!("{}.push-type.liveactivity", self.bundle_id);

        self.post(&url, &jwt, &topic, "liveactivity", &payload).await
    }

    // ── Helper HTTP ───────────────────────────────────────────────────────────

    /// Executa o POST para o endpoint APNS e interpreta a resposta.
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
            .header("apns-topic",    topic)
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
