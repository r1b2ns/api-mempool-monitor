use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Query, State},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use futures::stream;
use serde::Deserialize;
use tracing::{info, warn};

use crate::apns::ApnsClient;
use crate::mempool::{MempoolClient, MempoolError, TxEvent};
use crate::AppState;

/// Corpo do `POST /tx/watch`
#[derive(Debug, Deserialize)]
pub struct WatchRequest {
    /// ID da transação Bitcoin (hex, 64 caracteres)
    #[serde(rename = "txId")]
    pub tx_id: String,
    /// Device token do dispositivo iOS que receberá o push na confirmação
    #[serde(rename = "deviceToken")]
    pub device_token: String,
}

#[derive(Debug, Deserialize)]
pub struct PollParams {
    /// Intervalo de polling em segundos (padrão: 10, mínimo: 5, máximo: 60)
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    /// Para automaticamente quando a transação for confirmada (padrão: true)
    #[serde(default = "default_stop_on_confirm")]
    pub stop_on_confirmed: bool,
}

fn default_interval() -> u64 {
    10
}
fn default_stop_on_confirm() -> bool {
    true
}

/// POST /tx/watch[?interval_secs=10&stop_on_confirmed=true]
///
/// Abre um stream SSE que faz polling do status da transação Bitcoin.
/// Ao confirmar, dispara um push APNS para o `deviceToken` informado.
///
/// ### Body (JSON)
/// ```json
/// { "txId": "abc123...", "deviceToken": "a1b2c3d4..." }
/// ```
///
/// ### Eventos emitidos
/// | event        | quando                                           |
/// |--------------|--------------------------------------------------|
/// | `tx_status`  | a cada tick enquanto pendente na mempool          |
/// | `confirmed`  | quando a transação é incluída em um bloco        |
/// | `not_found`  | transação não existe na mempool nem na chain     |
/// | `error`      | falha temporária ao consultar a API              |
///
/// ### Payload dos eventos (JSON)
/// ```json
/// {
///   "txid": "abc123...",
///   "confirmed": false,
///   "block_height": null,
///   "block_hash": null,
///   "block_time": null,
///   "fee": 1500,
///   "vsize": 141
/// }
/// ```
pub async fn watch_tx(
    State(state): State<Arc<AppState>>,
    Query(params): Query<PollParams>,
    Json(body): Json<WatchRequest>,
) -> impl IntoResponse {
    let interval_secs = params.interval_secs.clamp(5, 60);
    let stop_on_confirmed = params.stop_on_confirmed;
    let txid = body.tx_id;
    let device_token = body.device_token;

    info!(
        txid = %txid,
        device_token = %device_token,
        interval_secs,
        stop_on_confirmed,
        "SSE watch started"
    );

    let mempool_client = state.client.clone();
    let apns_client = state.apns.clone();

    // `stream::unfold` carrega o estado entre cada tick do polling.
    let event_stream = stream::unfold(
        (mempool_client, apns_client, txid, device_token, false),
        move |(mempool, apns, txid, device_token, done)| async move {
            if done {
                return None; // Encerra o stream
            }

            tokio::time::sleep(Duration::from_secs(interval_secs)).await;

            let (event, is_terminal) = poll_once(
                &mempool,
                &txid,
                stop_on_confirmed,
                apns.as_ref(),
                &device_token,
            )
            .await;

            Some((
                Ok::<Event, std::convert::Infallible>(event),
                (mempool, apns, txid, device_token, is_terminal),
            ))
        },
    );

    Sse::new(event_stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(interval_secs.saturating_sub(2).max(3)))
            .text("ping"),
    )
}

/// Executa uma única consulta à API do mempool.space.
///
/// Na confirmação, envia push APNS se o cliente estiver configurado.
/// Retorna `(Event, is_terminal)` — `is_terminal = true` encerra o stream.
async fn poll_once(
    client: &MempoolClient,
    txid: &str,
    stop_on_confirmed: bool,
    apns: Option<&ApnsClient>,
    device_token: &str,
) -> (Event, bool) {
    let status = match client.fetch_tx_status(txid).await {
        Ok(s) => s,
        Err(MempoolError::NotFound) => {
            warn!(%txid, "Transaction not found");
            let payload = serde_json::json!({
                "txid": txid,
                "message": "Transaction not found in mempool or blockchain"
            });
            return (
                Event::default().event("not_found").data(payload.to_string()),
                false, // mantém polling: pode aparecer na mempool depois
            );
        }
        Err(e) => {
            warn!(%txid, error = %e, "API error");
            let payload = serde_json::json!({ "txid": txid, "message": e.to_string() });
            return (
                Event::default().event("error").data(payload.to_string()),
                false,
            );
        }
    };

    let is_confirmed = status.confirmed;

    let (fee, vsize) = match client.fetch_tx_fee(txid).await {
        Ok(f) => (Some(f.fee), Some(f.vsize)),
        Err(_) => (None, None),
    };

    let tx_event = TxEvent {
        txid: txid.to_string(),
        status,
        fee,
        vsize,
    };

    let data = serde_json::to_string(&tx_event).unwrap_or_default();

    if is_confirmed {
        info!(%txid, "Transaction confirmed — closing stream");

        // Dispara push APNS ao confirmar, se o cliente estiver configurado
        if let Some(apns_client) = apns {
            if !device_token.is_empty() {
                let title = "Transação Confirmada";
                let body = format!("Sua transação foi incluída em um bloco: {}…", &txid[..8]);
                match apns_client
                    .send_notification(device_token, Some(title), Some(&body), None, Some("default"), None)
                    .await
                {
                    Ok(apns_id) => info!(%txid, %apns_id, "Push APNS enviado com sucesso"),
                    Err(e) => warn!(%txid, error = %e, "Falha ao enviar push APNS"),
                }
            }
        }

        let event = Event::default().event("confirmed").data(data);
        let terminal = stop_on_confirmed;
        (event, terminal)
    } else {
        (Event::default().event("tx_status").data(data), false)
    }
}
