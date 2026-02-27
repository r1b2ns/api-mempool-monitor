use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::AppState;

/// Corpo da requisição `POST /push/apns`
#[derive(Debug, Deserialize)]
pub struct PushRequest {
    /// Device token do dispositivo iOS (hex, 64 caracteres)
    pub device_token: String,
    /// Título da notificação (opcional; se ausente, envia alerta simples)
    pub title: Option<String>,
    /// Corpo da notificação (opcional)
    pub body: Option<String>,
    /// Valor do badge no ícone do app (opcional)
    pub badge: Option<u32>,
    /// Nome do som a reproduzir; use `"default"` para o som padrão (opcional)
    pub sound: Option<String>,
    /// Dados personalizados adicionados ao campo `"data"` do payload (opcional)
    pub data: Option<serde_json::Value>,
}

/// Resposta de `POST /push/apns`
#[derive(Debug, Serialize)]
pub struct PushResponse {
    pub success: bool,
    /// Identificador único atribuído pela Apple (presente em caso de sucesso)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apns_id: Option<String>,
    /// Mensagem de erro (presente em caso de falha)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `POST /push/apns`
///
/// Envia uma notificação push para um dispositivo iOS via APNS (token-based auth).
///
/// ### Exemplo de corpo
/// ```json
/// {
///   "device_token": "a1b2c3d4...64hexchars",
///   "title": "Nova transação confirmada",
///   "body": "Sua transação foi incluída no bloco 840000",
///   "badge": 1,
///   "sound": "default",
///   "data": { "txid": "abc123..." }
/// }
/// ```
///
/// ### Variáveis de ambiente necessárias
/// | Variável                 | Descrição                              |
/// |--------------------------|----------------------------------------|
/// | `APNS_KEY_ID`            | Key ID (ex.: `ABCD123456`)             |
/// | `APNS_TEAM_ID`           | Team ID (ex.: `TEAM000001`)            |
/// | `APNS_BUNDLE_ID`         | Bundle ID (ex.: `com.example.myapp`)   |
/// | `APNS_PRIVATE_KEY_PATH`  | Caminho para o arquivo `.p8`           |
/// | `APNS_PRODUCTION`        | `true` = produção, padrão: sandbox     |
pub async fn send_apns_push(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PushRequest>,
) -> (StatusCode, Json<PushResponse>) {
    let client = match &state.apns {
        Some(c) => c,
        None => {
            warn!("Push solicitado mas APNS não está configurado");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(PushResponse {
                    success: false,
                    apns_id: None,
                    error: Some(
                        "APNS não configurado. Defina APNS_KEY_ID, APNS_TEAM_ID, \
                         APNS_BUNDLE_ID e APNS_PRIVATE_KEY_PATH."
                            .to_string(),
                    ),
                }),
            );
        }
    };

    info!(
        device_token = %req.device_token,
        title = ?req.title,
        "Enviando push via APNS"
    );

    match client
        .send_notification(
            &req.device_token,
            req.title.as_deref(),
            req.body.as_deref(),
            req.badge,
            req.sound.as_deref(),
            req.data.as_ref(),
        )
        .await
    {
        Ok(apns_id) => {
            info!(apns_id = %apns_id, "Push enviado com sucesso");
            (
                StatusCode::OK,
                Json(PushResponse {
                    success: true,
                    apns_id: Some(apns_id),
                    error: None,
                }),
            )
        }
        Err(e) => {
            warn!(error = %e, "Falha ao enviar push");
            (
                StatusCode::BAD_GATEWAY,
                Json(PushResponse {
                    success: false,
                    apns_id: None,
                    error: Some(e.to_string()),
                }),
            )
        }
    }
}
