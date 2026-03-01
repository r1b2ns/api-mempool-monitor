use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::apns::{ApnsClient, LiveActivityContentState, LiveActivityEvent};
use crate::mempool::{MempoolClient, MempoolError};
use crate::AppState;

// ── Intervalo de polling e limite máximo de tentativas ───────────────────────
const POLL_INTERVAL_SECS: u64 = 30;
/// 4 hours: (4h × 3600s) / 30s = 480 attempts
// const MAX_ATTEMPTS: u32 = 480;
const MAX_ATTEMPTS: u32 = 480;

// ── Request / Response ────────────────────────────────────────────────────────

/// Corpo do `POST /tx/watch`
#[derive(Debug, Deserialize)]
pub struct WatchRequest {
    /// ID da transação Bitcoin (hex, 64 chars)
    #[serde(rename = "txId")]
    pub tx_id: String,
    /// Device token APNs — usado para push de alerta convencional (opcional)
    #[serde(rename = "deviceToken", default)]
    pub device_token: String,
    /// Activity push token da Live Activity em execução no dispositivo.
    /// Quando presente, cada ciclo de polling envia um update à Live Activity.
    #[serde(rename = "activityToken")]
    pub activity_token: Option<String>,
}

#[derive(Serialize)]
pub struct WatchResponse {
    ok: bool,
    #[serde(rename = "txId")]
    tx_id: String,
    /// Número de confirmações no momento do registro (0 = pendente)
    pub confirmations: u32,
    /// Estado da transação: "pending" | "confirmed" | "failed"
    pub status: String,
    /// Valor total transferido em BTC (None se indisponível)
    #[serde(rename = "valueBtc", skip_serializing_if = "Option::is_none")]
    pub value_btc: Option<f64>,
    /// Taxa paga em satoshis (None se indisponível)
    #[serde(rename = "feeSats", skip_serializing_if = "Option::is_none")]
    pub fee_sats: Option<u64>,
    /// Block height where the transaction was confirmed (None if still pending)
    #[serde(rename = "blockHeight", skip_serializing_if = "Option::is_none")]
    pub block_height: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
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
    let txid          = body.tx_id.trim().to_string();
    let device_token  = body.device_token.trim().to_string();
    let activity_token = body.activity_token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    // Validação mínima
    if txid.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(WatchResponse {
                ok: false,
                tx_id: txid,
                confirmations: 0,
                status: "failed".to_string(),
                value_btc: None,
                fee_sats: None,
                block_height: None,
                message: Some("txId não pode ser vazio".to_string()),
            }),
        );
    }

    // ── Fetch inicial: retorna o estado atual da tx na resposta ───────────────
    let (confirmations, status_str, value_btc, fee_sats, initial_block_height) =
        match state.client.fetch_tx_status(&txid).await {
            Ok(s) => {
                let conf   = if s.confirmed { 1 } else { 0 };
                let status = if s.confirmed { "confirmed" } else { "pending" }.to_string();
                let (vbtc, fsats) = match state.client.fetch_tx_fee(&txid).await {
                    Ok(f) => (Some(f.value_sat as f64 / 100_000_000.0), Some(f.fee)),
                    Err(_) => (None, None),
                };
                (conf, status, vbtc, fsats, s.block_height)
            }
            // Tx not yet propagated — return pending without fee
            Err(_) => (0u32, "pending".to_string(), None, None, None),
        };

    info!(
        txid           = %txid,
        device_token   = %device_token,
        activity_token = ?activity_token,
        confirmations,
        status         = %status_str,
        poll_interval_secs = POLL_INTERVAL_SECS,
        max_attempts   = MAX_ATTEMPTS,
        "Monitoramento registrado — iniciando task em background"
    );

    // Spawna background sem bloquear o handler
    tokio::spawn(background_poll(
        state.client.clone(),
        state.apns.clone(),
        txid.clone(),
        device_token,
        activity_token,
    ));

    (
        StatusCode::OK,
        Json(WatchResponse {
            ok: true,
            tx_id: txid,
            confirmations,
            status: status_str,
            value_btc,
            fee_sats,
            block_height: initial_block_height,
            message: Some(format!(
                "Monitoramento iniciado. Push APNS será enviado ao confirmar \
                 (polling a cada {POLL_INTERVAL_SECS}s, máx. {MAX_ATTEMPTS} tentativas)."
            )),
        }),
    )
}

// ── GET /tx/:txid ─────────────────────────────────────────────────────────────

/// `GET /tx/:txid`
///
/// Consulta o estado atual de uma transação na mempool e retorna os mesmos
/// campos que `POST /tx/watch` inclui na resposta.
///
/// ### Resposta 200
/// ```json
/// { "ok": true, "txId": "abc…", "confirmations": 1, "status": "confirmed",
///   "valueBtc": 0.0725, "feeSats": 7119 }
/// ```
///
/// ### Resposta 404
/// ```json
/// { "ok": false, "txId": "abc…", "confirmations": 0, "status": "failed" }
/// ```
pub async fn get_tx(
    State(state): State<Arc<AppState>>,
    Path(txid): Path<String>,
) -> (StatusCode, Json<WatchResponse>) {
    let txid = txid.trim().to_string();
    info!(txid = %txid, "GET /tx/:txid");

    let tx_status = match state.client.fetch_tx_status(&txid).await {
        Ok(s) => s,
        Err(MempoolError::NotFound) => {
            warn!(txid = %txid, "GET /tx/:txid → 404 (transação não encontrada)");
            return (
                StatusCode::NOT_FOUND,
                Json(WatchResponse {
                    ok: false,
                    tx_id: txid,
                    confirmations: 0,
                    status: "failed".to_string(),
                    value_btc: None,
                    fee_sats: None,
                    block_height: None,
                    message: None,
                }),
            );
        }
        Err(e) => {
            warn!(txid = %txid, error = %e, "GET /tx/:txid → 502 (erro ao consultar mempool API)");
            return (
                StatusCode::BAD_GATEWAY,
                Json(WatchResponse {
                    ok: false,
                    tx_id: txid,
                    confirmations: 0,
                    status: "failed".to_string(),
                    value_btc: None,
                    fee_sats: None,
                    block_height: None,
                    message: Some(e.to_string()),
                }),
            );
        }
    };

    let (conf, status) = if tx_status.confirmed {
        // Confirmations = chain_tip - block_height + 1
        let block_height = tx_status.block_height.unwrap_or(0);
        let chain_tip    = state.client.fetch_chain_tip().await.unwrap_or(block_height);
        let confirmations = (chain_tip.saturating_sub(block_height) + 1) as u32;
        (confirmations, "confirmed".to_string())
    } else {
        (0, "pending".to_string())
    };

    let (value_btc, fee_sats) = match state.client.fetch_tx_fee(&txid).await {
        Ok(f) => (Some(f.value_sat as f64 / 100_000_000.0), Some(f.fee)),
        Err(_) => (None, None),
    };

    info!(
        txid      = %txid,
        status    = %status,
        confirmations = conf,
        value_btc = ?value_btc,
        fee_sats  = ?fee_sats,
        "GET /tx/:txid → 200"
    );

    (
        StatusCode::OK,
        Json(WatchResponse {
            ok: true,
            tx_id: txid,
            confirmations: conf,
            status,
            value_btc,
            fee_sats,
            block_height: tx_status.block_height,
            message: None,
        }),
    )
}

// ── Background task ───────────────────────────────────────────────────────────

/// Faz polling do status da transação até confirmar ou esgotar as tentativas.
///
/// A cada ciclo:
/// - Consulta status na mempool.space
/// - Envia update à Live Activity (se `activity_token` presente) com o estado atual
/// - Quando confirmada: envia evento `end` à Live Activity + push de alerta ao `device_token`
/// - Para automaticamente após confirmação ou limite de tentativas
async fn background_poll(
    client: MempoolClient,
    apns: Option<ApnsClient>,
    txid: String,
    device_token: String,
    activity_token: Option<String>,
) {
    info!(
        txid           = %txid,
        has_live_activity = activity_token.is_some(),
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

        // ── 2. Busca fee e valor da transação (mesmo ciclo, uma única chamada) ─
        let (value_btc, fee_sats) = match client.fetch_tx_fee(&txid).await {
            Ok(f) => (
                Some(f.value_sat as f64 / 100_000_000.0),
                Some(f.fee),
            ),
            Err(_) => (None, None),
        };

        // ── 3. Ainda pendente ────────────────────────────────────────────────
        if !status.confirmed {
            match fee_sats {
                Some(fee) => info!(
                    txid         = %txid,
                    attempt,
                    fee_sats     = fee,
                    value_btc    = ?value_btc,
                    "Transação pendente na mempool"
                ),
                None => info!(txid = %txid, attempt, "Transação pendente na mempool (fee indisponível)"),
            }

            let state = LiveActivityContentState {
                confirmations: 0,
                status: "pending".to_string(),
                tx_id: txid.clone(),
                value_btc,
                fee_sats,
            };
            send_live_activity(&apns, &txid, &activity_token, LiveActivityEvent::Update, &state).await;
            continue;
        }

        // ── 4. Confirmada! ───────────────────────────────────────────────────
        let block_height = status.block_height.unwrap_or(0);
        let block_hash   = status.block_hash.as_deref().unwrap_or("?");
        let block_time   = status.block_time.unwrap_or(0);

        info!(
            txid        = %txid,
            attempt,
            block_height,
            block_hash  = %block_hash,
            block_time,
            value_btc   = ?value_btc,
            fee_sats    = ?fee_sats,
            "━━━ [WATCHER] Transação CONFIRMADA ━━━"
        );

        // Envia evento "end" à Live Activity com estado "confirmed"
        let state = LiveActivityContentState {
            confirmations: 1,
            status: "confirmed".to_string(),
            tx_id: txid.clone(),
            value_btc,
            fee_sats,
        };
        send_live_activity(&apns, &txid, &activity_token, LiveActivityEvent::End, &state).await;

        // Conventional push is only sent when there is no active Live Activity.
        // With Live Activity, confirmation is already shown directly in the widget.
        if activity_token.is_none() {
            let short_txid = &txid[..txid.len().min(8)];
            let block_info = if block_height > 0 {
                format!("block #{block_height}")
            } else {
                "a block".to_string()
            };
            send_push(
                &apns,
                &txid,
                &device_token,
                "Transaction Confirmed ✓",
                &format!("Tx {short_txid}… included in {block_info}"),
            ).await;
        } else {
            info!(txid = %txid, "Conventional push suppressed — Live Activity is active");
        }

        info!(txid = %txid, attempt, "━━━ [WATCHER] Monitoramento encerrado (confirmado) ━━━");
        return;
    }

    warn!(
        txid         = %txid,
        max_attempts = MAX_ATTEMPTS,
        "━━━ [WATCHER] Monitoring timed out — max attempts reached ━━━"
    );

    // Notify the user that monitoring expired without confirmation
    let short_txid = &txid[..txid.len().min(8)];
    send_push(
        &apns,
        &txid,
        &device_token,
        "Transaction monitoring expired",
        &format!("Tx {short_txid}… was not confirmed within 4 hours."),
    ).await;

    // Close the Live Activity with "failed" status if one is active
    if activity_token.is_some() {
        let state = LiveActivityContentState {
            confirmations: 0,
            status: "failed".to_string(),
            tx_id: txid.clone(),
            value_btc: None,
            fee_sats:  None,
        };
        send_live_activity(&apns, &txid, &activity_token, LiveActivityEvent::End, &state).await;
    }

}

/// Envia atualização de Live Activity via APNS e loga o resultado.
///
/// Não faz nada se `activity_token` for `None` ou se o cliente APNS não estiver configurado.
async fn send_live_activity(
    apns: &Option<ApnsClient>,
    txid: &str,
    activity_token: &Option<String>,
    event: LiveActivityEvent,
    content_state: &LiveActivityContentState,
) {
    let Some(token) = activity_token else { return };
    let Some(apns_client) = apns else {
        warn!(txid = %txid, "APNS não configurado — Live Activity update não enviado");
        return;
    };

    info!(
        txid   = %txid,
        event  = %event.as_str(),
        status = %content_state.status,
        confirmations = content_state.confirmations,
        "Enviando Live Activity update..."
    );

    match apns_client
        .send_live_activity_update(token, event, content_state)
        .await
    {
        Ok(apns_id) => info!(
            txid    = %txid,
            event   = %event.as_str(),
            apns_id = %apns_id,
            "Live Activity update enviado com sucesso"
        ),
        Err(e) => {
            let mut detail = e.to_string();
            let mut src: Option<&dyn std::error::Error> = std::error::Error::source(&e);
            while let Some(cause) = src {
                detail.push_str(&format!(" → {cause}"));
                src = cause.source();
            }
            error!(txid = %txid, event = %event.as_str(), error = %detail, "Falha ao enviar Live Activity update");
        }
    }
}

/// Sends an APNS alert push and logs the result.
async fn send_push(
    apns: &Option<ApnsClient>,
    txid: &str,
    device_token: &str,
    title: &str,
    body: &str,
) {
    let Some(apns_client) = apns else {
        warn!(txid = %txid, "APNS not configured — push not sent");
        return;
    };

    if device_token.is_empty() {
        warn!(txid = %txid, "device_token is empty — push not sent");
        return;
    }

    info!(
        txid         = %txid,
        device_token = %device_token,
        title        = %title,
        body         = %body,
        "Sending APNS push..."
    );

    match apns_client
        .send_notification(device_token, Some(title), Some(body), None, Some("default"), None)
        .await
    {
        Ok(apns_id) => info!(
            txid    = %txid,
            apns_id = %apns_id,
            "APNS push sent successfully"
        ),
        Err(e) => {
            let mut detail = e.to_string();
            let mut src: Option<&dyn std::error::Error> = std::error::Error::source(&e);
            while let Some(cause) = src {
                detail.push_str(&format!(" → {cause}"));
                src = cause.source();
            }
            error!(txid = %txid, error = %detail, "Failed to send APNS push");
        }
    }
}
