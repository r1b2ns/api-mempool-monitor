use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client as HttpClient;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApnsError {
    #[error("Erro HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Erro ao gerar JWT: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("APNS rejeitou a notificação (HTTP {code}): {reason}")]
    Apns { code: u16, reason: String },
}

/// Claims do JWT de autenticação APNS (token-based auth)
#[derive(Serialize)]
struct ApnsClaims {
    /// Team ID (issuer)
    iss: String,
    /// Issued-at timestamp (segundos desde Unix epoch)
    iat: u64,
}

/// Cliente APNS implementado diretamente sobre HTTP/2 + JWT.
///
/// Usa `reqwest` (já presente no projeto) para evitar conflitos de versão
/// com outras dependências que também utilizam `hyper`.
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
            .http2_prior_knowledge()  // garante que apenas HTTP/2 seja usado
            .build()?;

        Ok(Some(Self {
            http: Arc::new(http),
            key_id,
            team_id,
            bundle_id,
            production,
            pem: Arc::new(pem),
        }))
    }

    /// Gera um JWT assinado com ES256 para autenticar no APNS.
    ///
    /// O token é válido por 1 hora. Como o overhead de geração é mínimo
    /// (microssegundos), geramos um novo token a cada chamada.
    fn jwt_token(&self) -> Result<String, ApnsError> {
        let iat = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let claims = ApnsClaims {
            iss: self.team_id.clone(),
            iat,
        };

        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(self.key_id.clone());

        let key = jsonwebtoken::EncodingKey::from_ec_pem(self.pem.as_bytes())?;
        Ok(jsonwebtoken::encode(&header, &claims, &key)?)
    }

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

        let token = self.jwt_token()?;

        // Monta o objeto alert
        let mut alert = serde_json::Map::new();
        if let Some(t) = title {
            alert.insert("title".into(), t.into());
        }
        if let Some(b) = body {
            alert.insert("body".into(), b.into());
        }

        // Monta o objeto aps
        let mut aps = serde_json::json!({ "alert": alert });
        if let Some(n) = badge {
            aps["badge"] = serde_json::json!(n);
        }
        if let Some(s) = sound {
            aps["sound"] = serde_json::Value::String(s.into());
        }

        // Payload raiz
        let mut payload = serde_json::json!({ "aps": aps });
        if let Some(data) = custom_data {
            payload["data"] = data.clone();
        }

        let response = self
            .http
            .post(&url)
            .header("authorization", format!("bearer {token}"))
            .header("apns-topic", &self.bundle_id)
            .header("apns-push-type", "alert")
            .json(&payload)
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
            let resp_body: serde_json::Value = response.json().await.unwrap_or_default();
            let reason = resp_body["reason"]
                .as_str()
                .unwrap_or("Unknown")
                .to_string();
            Err(ApnsError::Apns { code, reason })
        }
    }
}
