use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use tracing::{error, info, warn};

// Matches a valid Bitcoin txid: exactly 64 lowercase or uppercase hex characters.
static TXID_RE: OnceLock<Regex> = OnceLock::new();

fn txid_regex() -> &'static Regex {
    TXID_RE.get_or_init(|| Regex::new(r"^[0-9a-fA-F]{64}$").expect("invalid txid regex"))
}

use crate::apns::{ApnsClient, BlockPosition, LiveActivityContentState, LiveActivityEvent};
use crate::mempool::{MempoolBlock, MempoolClient, MempoolError};
use crate::AppState;

// ── Poll settings ─────────────────────────────────────────────────────────────

const POLL_INTERVAL_SECS: u64 = 30;
/// 4 hours: (4h × 3600s) / 30s = 480 attempts
const MAX_ATTEMPTS: u32 = 480;

// ── Block position ────────────────────────────────────────────────────────────

/// Determines the projected block position of a pending transaction.
///
/// Compares the transaction's effective fee rate (sats/vbyte) against the minimum
/// fee rate of each projected mempool block (index 0 = next block, index 1 = second).
/// `fee_range[0]` from the mempool API is the lowest fee rate that made it into
/// that block — a transaction qualifies if its fee rate is at or above that value.
///
/// Returns `Other` when fee_rate is 0 (unknown vsize) or the block list is empty.
fn compute_block_position(fee_rate: f64, blocks: &[MempoolBlock]) -> BlockPosition {
    let min_fee_of = |i: usize| -> Option<f64> {
        blocks.get(i).and_then(|b| b.fee_range.first()).copied()
    };

    if fee_rate > 0.0 {
        if let Some(min) = min_fee_of(0) {
            if fee_rate >= min {
                return BlockPosition::NextBlock;
            }
        }
        if let Some(min) = min_fee_of(1) {
            if fee_rate >= min {
                return BlockPosition::SecondBlock;
            }
        }
    }

    BlockPosition::Other
}

/// Fetches projected mempool blocks and returns the block position for a given fee rate.
/// Falls back to `Other` on network errors so a failed call never breaks the poll cycle.
async fn resolve_block_position(
    client: &MempoolClient,
    txid: &str,
    fee_rate: f64,
) -> BlockPosition {
    match client.fetch_mempool_blocks().await {
        Ok(blocks) => compute_block_position(fee_rate, &blocks),
        Err(e) => {
            warn!(txid = %txid, error = %e, "Failed to fetch mempool blocks — defaulting to Other");
            BlockPosition::Other
        }
    }
}

// ── Request / Response ────────────────────────────────────────────────────────

/// Body of `POST /tx/watch`
#[derive(Debug, Deserialize)]
pub struct WatchRequest {
    /// Bitcoin transaction ID (hex, 64 chars)
    #[serde(rename = "txId")]
    pub tx_id: String,
    /// APNs device token — used for conventional alert push (optional)
    #[serde(rename = "deviceToken", default)]
    pub device_token: String,
    /// Activity push token of the Live Activity running on the device.
    /// When present, each poll cycle sends a Live Activity update.
    #[serde(rename = "activityToken")]
    pub activity_token: Option<String>,
}

#[derive(Serialize)]
pub struct WatchResponse {
    ok: bool,
    #[serde(rename = "txId")]
    tx_id: String,
    /// Number of confirmations at the time of the response (0 = pending)
    pub confirmations: u32,
    /// Transaction status: "pending" | "confirmed" | "failed"
    pub status: String,
    /// Total amount transferred in BTC (None if unavailable)
    #[serde(rename = "valueBtc", skip_serializing_if = "Option::is_none")]
    pub value_btc: Option<f64>,
    /// Fee paid in satoshis (None if unavailable)
    #[serde(rename = "feeSats", skip_serializing_if = "Option::is_none")]
    pub fee_sats: Option<u64>,
    /// Block height where the transaction was confirmed (None if still pending)
    #[serde(rename = "blockHeight", skip_serializing_if = "Option::is_none")]
    pub block_height: Option<u64>,
    /// Projected block position — present only while the transaction is pending
    #[serde(rename = "blockPosition", skip_serializing_if = "Option::is_none")]
    pub block_position: Option<BlockPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

// ── Handler: POST /tx/watch ───────────────────────────────────────────────────

/// `POST /tx/watch`
///
/// Registers a transaction for monitoring. Returns **200** immediately and keeps
/// polling in a background `tokio::task` until the transaction is confirmed
/// (or the attempt limit is reached).
///
/// ### Body JSON
/// ```json
/// { "txId": "abc123…", "deviceToken": "a1b2…", "activityToken": "c3d4…" }
/// ```
pub async fn watch_tx(
    State(state): State<Arc<AppState>>,
    Json(body): Json<WatchRequest>,
) -> (StatusCode, Json<WatchResponse>) {
    let txid           = body.tx_id.trim().to_string();
    let device_token   = body.device_token.trim().to_string();
    let activity_token = body.activity_token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    // Validate txid format: must be exactly 64 hex characters
    if !txid_regex().is_match(&txid) {
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
                block_position: None,
                message: Some("Invalid txId: must be exactly 64 hex characters".to_string()),
            }),
        );
    }

    // ── Initial fetch: return current tx state in the response ────────────────
    let (confirmations, status_str, value_btc, fee_sats, initial_block_height, block_position) =
        match state.client.fetch_tx_status(&txid).await {
            Ok(s) => {
                let conf       = if s.confirmed { 1 } else { 0 };
                let status_str = if s.confirmed { "confirmed" } else { "pending" }.to_string();

                let (vbtc, fsats, fee_rate) = match state.client.fetch_tx_fee(&txid).await {
                    Ok(f) => {
                        let rate = if f.vsize > 0 { f.fee as f64 / f.vsize as f64 } else { 0.0 };
                        (Some(f.value_sat as f64 / 100_000_000.0), Some(f.fee), rate)
                    }
                    Err(_) => (None, None, 0.0),
                };

                // Block position is only meaningful for pending transactions
                let bpos = if !s.confirmed {
                    Some(resolve_block_position(&state.client, &txid, fee_rate).await)
                } else {
                    None
                };

                (conf, status_str, vbtc, fsats, s.block_height, bpos)
            }
            // Tx not yet propagated — return pending without fee or block position
            Err(_) => (0u32, "pending".to_string(), None, None, None, None),
        };

    info!(
        txid              = %txid,
        device_token      = %device_token,
        activity_token    = ?activity_token,
        confirmations,
        status            = %status_str,
        block_position    = ?block_position,
        poll_interval_secs = POLL_INTERVAL_SECS,
        max_attempts      = MAX_ATTEMPTS,
        "Monitoring registered — spawning background task"
    );

    // Spawn background poll without blocking the handler
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
            block_position,
            message: Some(format!(
                "Monitoring started. APNS push will be sent on confirmation \
                 (polling every {POLL_INTERVAL_SECS}s, max {MAX_ATTEMPTS} attempts)."
            )),
        }),
    )
}

// ── Handler: GET /tx/:txid ────────────────────────────────────────────────────

/// `GET /tx/:txid`
///
/// Queries the current state of a transaction in the mempool and returns the
/// same fields as `POST /tx/watch`.
pub async fn get_tx(
    State(state): State<Arc<AppState>>,
    Path(txid): Path<String>,
) -> (StatusCode, Json<WatchResponse>) {
    let txid = txid.trim().to_string();
    info!(txid = %txid, "GET /tx/:txid");

    // Validate txid format: must be exactly 64 hex characters
    if !txid_regex().is_match(&txid) {
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
                block_position: None,
                message: Some("Invalid txId: must be exactly 64 hex characters".to_string()),
            }),
        );
    }

    let tx_status = match state.client.fetch_tx_status(&txid).await {
        Ok(s) => s,
        Err(MempoolError::NotFound) => {
            warn!(txid = %txid, "GET /tx/:txid → 404 (transaction not found)");
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
                    block_position: None,
                    message: None,
                }),
            );
        }
        Err(e) => {
            warn!(txid = %txid, error = %e, "GET /tx/:txid → 502 (mempool API error)");
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
                    block_position: None,
                    message: Some(e.to_string()),
                }),
            );
        }
    };

    let (conf, status) = if tx_status.confirmed {
        // Confirmations = chain_tip - block_height + 1
        let block_height  = tx_status.block_height.unwrap_or(0);
        let chain_tip     = state.client.fetch_chain_tip().await.unwrap_or(block_height);
        let confirmations = (chain_tip.saturating_sub(block_height) + 1) as u32;
        (confirmations, "confirmed".to_string())
    } else {
        (0, "pending".to_string())
    };

    // Fetch fee info; keep the full TxFee so we can compute fee_rate for block position
    let tx_fee    = state.client.fetch_tx_fee(&txid).await.ok();
    let value_btc = tx_fee.as_ref().map(|f| f.value_sat as f64 / 100_000_000.0);
    let fee_sats  = tx_fee.as_ref().map(|f| f.fee);
    let fee_rate  = tx_fee.as_ref()
        .filter(|f| f.vsize > 0)
        .map(|f| f.fee as f64 / f.vsize as f64)
        .unwrap_or(0.0);

    // Block position is only meaningful while the transaction is pending
    let block_position = if !tx_status.confirmed {
        Some(resolve_block_position(&state.client, &txid, fee_rate).await)
    } else {
        None
    };

    info!(
        txid           = %txid,
        status         = %status,
        confirmations  = conf,
        value_btc      = ?value_btc,
        fee_sats       = ?fee_sats,
        block_position = ?block_position,
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
            block_position,
            message: None,
        }),
    )
}

// ── Background task ───────────────────────────────────────────────────────────

/// Polls the transaction status until it confirms or the attempt limit is reached.
///
/// On each cycle:
/// - Fetches status and fee info from the mempool API
/// - Fetches projected mempool blocks to compute `block_position`
/// - Sends a Live Activity update (if `activity_token` is set) with the current state
/// - On confirmation: sends Live Activity `end` event + optional alert push
async fn background_poll(
    client: MempoolClient,
    apns: Option<ApnsClient>,
    txid: String,
    device_token: String,
    activity_token: Option<String>,
) {
    info!(
        txid              = %txid,
        has_live_activity = activity_token.is_some(),
        "━━━ [WATCHER] Starting monitoring ━━━"
    );

    for attempt in 1..=MAX_ATTEMPTS {
        tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;

        info!(txid = %txid, attempt, "Querying transaction status...");

        // ── 1. Fetch status ──────────────────────────────────────────────────
        let status = match client.fetch_tx_status(&txid).await {
            Ok(s) => s,
            Err(MempoolError::NotFound) => {
                warn!(txid = %txid, attempt, "Transaction not found in mempool (waiting for propagation)");
                continue;
            }
            Err(MempoolError::ClientError(code)) => {
                warn!(txid = %txid, attempt, status_code = code, "4XX from mempool API — stopping poll");
                return;
            }
            Err(e) => {
                warn!(txid = %txid, attempt, error = %e, "Mempool API error — retrying");
                continue;
            }
        };

        // ── 2. Fetch fee and value (single call per cycle) ───────────────────
        let (value_btc, fee_sats, fee_rate) = match client.fetch_tx_fee(&txid).await {
            Ok(f) => {
                let rate = if f.vsize > 0 { f.fee as f64 / f.vsize as f64 } else { 0.0 };
                (Some(f.value_sat as f64 / 100_000_000.0), Some(f.fee), rate)
            }
            Err(_) => (None, None, 0.0),
        };

        // ── 3. Still pending ─────────────────────────────────────────────────
        if !status.confirmed {
            let block_position = resolve_block_position(&client, &txid, fee_rate).await;

            match fee_sats {
                Some(fee) => info!(
                    txid           = %txid,
                    attempt,
                    fee_sats       = fee,
                    fee_rate       = format!("{fee_rate:.2}"),
                    block_position = ?block_position,
                    value_btc      = ?value_btc,
                    "Transaction pending in mempool"
                ),
                None => info!(txid = %txid, attempt, "Transaction pending (fee unavailable)"),
            }

            let content_state = LiveActivityContentState {
                confirmations: 0,
                status: "pending".to_string(),
                tx_id: txid.clone(),
                value_btc,
                fee_sats,
                block_position: Some(block_position),
            };
            send_live_activity(&apns, &txid, &activity_token, LiveActivityEvent::Update, &content_state).await;
            continue;
        }

        // ── 4. Confirmed! ────────────────────────────────────────────────────
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
            "━━━ [WATCHER] Transaction CONFIRMED ━━━"
        );

        // Send "end" event to Live Activity with "confirmed" state
        let content_state = LiveActivityContentState {
            confirmations: 1,
            status: "confirmed".to_string(),
            tx_id: txid.clone(),
            value_btc,
            fee_sats,
            block_position: None, // not relevant after confirmation
        };
        send_live_activity(&apns, &txid, &activity_token, LiveActivityEvent::End, &content_state).await;

        // Conventional push is suppressed when a Live Activity is active —
        // confirmation is already displayed directly in the widget.
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

        info!(txid = %txid, attempt, "━━━ [WATCHER] Monitoring ended (confirmed) ━━━");
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
        let content_state = LiveActivityContentState {
            confirmations: 0,
            status: "failed".to_string(),
            tx_id: txid.clone(),
            value_btc: None,
            fee_sats: None,
            block_position: None,
        };
        send_live_activity(&apns, &txid, &activity_token, LiveActivityEvent::End, &content_state).await;
    }
}

// ── APNS helpers ──────────────────────────────────────────────────────────────

/// Sends a Live Activity update via APNS and logs the result.
///
/// No-ops when `activity_token` is `None` or when the APNS client is not configured.
async fn send_live_activity(
    apns: &Option<ApnsClient>,
    txid: &str,
    activity_token: &Option<String>,
    event: LiveActivityEvent,
    content_state: &LiveActivityContentState,
) {
    let Some(token) = activity_token else { return };
    let Some(apns_client) = apns else {
        warn!(txid = %txid, "APNS not configured — Live Activity update skipped");
        return;
    };

    info!(
        txid          = %txid,
        event         = %event.as_str(),
        status        = %content_state.status,
        confirmations = content_state.confirmations,
        block_position = ?content_state.block_position,
        "Sending Live Activity update..."
    );

    match apns_client.send_live_activity_update(token, event, content_state).await {
        Ok(apns_id) => info!(
            txid    = %txid,
            event   = %event.as_str(),
            apns_id = %apns_id,
            "Live Activity update sent successfully"
        ),
        Err(e) => {
            let mut detail = e.to_string();
            let mut src: Option<&dyn std::error::Error> = std::error::Error::source(&e);
            while let Some(cause) = src {
                detail.push_str(&format!(" → {cause}"));
                src = cause.source();
            }
            error!(txid = %txid, event = %event.as_str(), error = %detail, "Failed to send Live Activity update");
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
