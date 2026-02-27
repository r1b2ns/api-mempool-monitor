use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::apns::ApnsClient;
use crate::mempool::{MempoolClient, MempoolError};
use crate::AppState;

// ── Intervalo de polling e limite máximo de tentativas ───────────────────────
const POLL_INTERVAL_SECS: u64 = 10;
/// ~24 horas: 6 polls/min × 60 min × 24 h = 8 640
const MAX_ATTEMPTS: u32 = 8_640;

// ── Request / Response ────────────────────────────────────────────────────────

/// Corpo do `POST /tx/watch`
#[derive(Debug, Deserialize)]
pub struct WatchRequest {
    /// ID da transação Bitcoin (hex, 64 chars)
    #[serde(rename = "txId")]
    pub tx_id: String,
    /// Device token iOS que receberá o push na confirmação
    #[serde(rename = "deviceToken")]
    pub device_token: String,
}

#[derive(Serialize)]
pub struct WatchResponse {
    ok: bool,
    #[serde(rename = "txId")]
    tx_id: String,
    message: String,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// `POST /tx/watch`
///
/// Registra a transação para monitoramento. Retorna **200** imediatamente e
/// mantém o polling rodando em uma `tokio::task` de background até que a
/// transação seja confirmada (ou se esgotem as tentativas).
///
/// ### Body JSON
/// ```json
/// { "txId": "abc123…", "deviceToken": "a1b2…" }
/// ```
///
/// ### Resposta 200
/// ```json
/// { "ok": true, "txId": "abc123…", "message": "Monitoramento iniciado…" }
/// ```
pub async fn watch_tx(
    State(state): State<Arc<AppState>>,
    Json(body): Json<WatchRequest>,
) -> (StatusCode, Json<WatchResponse>) {
    let txid = body.tx_id.trim().to_string();
    let device_token = body.device_token.trim().to_string();

    // Validação mínima
    if txid.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(WatchResponse {
                ok: false,
                tx_id: txid,
                message: "txId não pode ser vazio".to_string(),
            }),
        );
    }

    info!(
        txid = %txid,
        device_token = %device_token,
        poll_interval_secs = POLL_INTERVAL_SECS,
        max_attempts = MAX_ATTEMPTS,
        "Monitoramento registrado — iniciando task em background"
    );

    // Spawna background sem bloquear o handler
    tokio::spawn(background_poll(
        state.client.clone(),
        state.apns.clone(),
        txid.clone(),
        device_token,
    ));

    (
        StatusCode::OK,
        Json(WatchResponse {
            ok: true,
            tx_id: txid,
            message: format!(
                "Monitoramento iniciado. Push APNS será enviado ao confirmar \
                 (polling a cada {POLL_INTERVAL_SECS}s, máx. {MAX_ATTEMPTS} tentativas)."
            ),
        }),
    )
}

// ── Background task ───────────────────────────────────────────────────────────

/// Faz polling do status da transação até confirmar ou esgotar as tentativas.
///
/// Logs emitidos em cada ciclo:
/// - `Consultando status` — início de cada tentativa
/// - `Pendente na mempool` — tx ainda não confirmada (inclui fee e vsize)
/// - `Não encontrada` — tx não consta no mempool nem na chain
/// - `Erro ao consultar API` — falha HTTP temporária
/// - `CONFIRMADA` — tx incluída em bloco (dispara push APNS)
/// - `Push APNS enviado` / `Falha ao enviar push` — resultado do push
/// - `Encerrado por timeout` — limite de tentativas atingido
async fn background_poll(
    client: MempoolClient,
    apns: Option<ApnsClient>,
    txid: String,
    device_token: String,
) {
    info!(
        txid = %txid,
        "━━━ [WATCHER] Iniciando monitoramento ━━━"
    );

    for attempt in 1..=MAX_ATTEMPTS {
        tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;

        info!(txid = %txid, attempt, "Consultando status da transação...");

        // ── 1. Busca status ──────────────────────────────────────────────────
        let status = match client.fetch_tx_status(&txid).await {
            Ok(s) => s,
            Err(MempoolError::NotFound) => {
                warn!(txid = %txid, attempt, "Transação não encontrada na mempool (aguardando propagação)");
                continue;
            }
            Err(e) => {
                warn!(txid = %txid, attempt, error = %e, "Erro ao consultar mempool API — tentando novamente");
                continue;
            }
        };

        // ── 2. Ainda pendente ────────────────────────────────────────────────
        if !status.confirmed {
            match client.fetch_tx_fee(&txid).await {
                Ok(f) => info!(
                    txid = %txid,
                    attempt,
                    fee_sats = f.fee,
                    vsize_vbytes = f.vsize,
                    "Transação pendente na mempool"
                ),
                Err(_) => info!(
                    txid = %txid,
                    attempt,
                    "Transação pendente na mempool (fee indisponível)"
                ),
            }
            continue;
        }

        // ── 3. Confirmada! ───────────────────────────────────────────────────
        let block_height = status.block_height.unwrap_or(0);
        let block_hash   = status.block_hash.as_deref().unwrap_or("?");
        let block_time   = status.block_time.unwrap_or(0);

        info!(
            txid         = %txid,
            attempt,
            block_height,
            block_hash   = %block_hash,
            block_time,
            "━━━ [WATCHER] Transação CONFIRMADA ━━━"
        );

        // ── 4. Envia push APNS ───────────────────────────────────────────────
        send_push(&apns, &txid, &device_token, block_height).await;

        info!(txid = %txid, attempt, "━━━ [WATCHER] Monitoramento encerrado (confirmado) ━━━");
        return;
    }

    warn!(
        txid = %txid,
        max_attempts = MAX_ATTEMPTS,
        "━━━ [WATCHER] Monitoramento encerrado por timeout (limite de tentativas atingido) ━━━"
    );
}

/// Envia push APNS e loga o resultado detalhadamente.
async fn send_push(
    apns: &Option<ApnsClient>,
    txid: &str,
    device_token: &str,
    block_height: u64,
) {
    let Some(apns_client) = apns else {
        warn!(txid = %txid, "APNS não configurado — push não enviado");
        return;
    };

    if device_token.is_empty() {
        warn!(txid = %txid, "device_token vazio — push não enviado");
        return;
    }

    let title      = "Transação Confirmada ✓";
    let short_txid = &txid[..txid.len().min(8)];
    let block_info = if block_height > 0 {
        format!("bloco #{block_height}")
    } else {
        "um bloco".to_string()
    };
    let body = format!("Tx {short_txid}… incluída no {block_info}");

    info!(
        txid         = %txid,
        device_token = %device_token,
        title        = %title,
        body         = %body,
        "Enviando push APNS..."
    );

    match apns_client
        .send_notification(device_token, Some(title), Some(&body), None, Some("default"), None)
        .await
    {
        Ok(apns_id) => info!(
            txid     = %txid,
            apns_id  = %apns_id,
            "Push APNS enviado com sucesso"
        ),
        Err(e) => {
            // Expõe a cadeia completa de causas para facilitar diagnóstico
            let mut detail = e.to_string();
            let mut src: Option<&dyn std::error::Error> = std::error::Error::source(&e);
            while let Some(cause) = src {
                detail.push_str(&format!(" → {cause}"));
                src = cause.source();
            }
            error!(txid = %txid, error = %detail, "Falha ao enviar push APNS");
        }
    }
}
