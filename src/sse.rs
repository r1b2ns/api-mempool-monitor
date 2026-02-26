use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path, Query, State},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
};
use futures::stream;
use serde::Deserialize;
use tracing::{info, warn};

use crate::mempool::{MempoolClient, MempoolError, TxEvent};

pub struct AppState {
    pub client: MempoolClient,
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

/// GET /tx/:txid/watch[?interval_secs=10&stop_on_confirmed=true]
///
/// Abre um stream SSE que faz polling do status da transação Bitcoin.
///
/// ### Eventos emitidos
/// | event        | quando                                          |
/// |------------- |------------------------------------------------ |
/// | `tx_status`  | a cada tick enquanto pendente na mempool         |
/// | `confirmed`  | quando a transação é incluída em um bloco        |
/// | `not_found`  | transação não existe na mempool nem na chain     |
/// | `error`      | falha temporária ao consultar a API              |
///
/// ### Payload (JSON)
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
    Path(txid): Path<String>,
    Query(params): Query<PollParams>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let interval_secs = params.interval_secs.clamp(5, 60);
    let stop_on_confirmed = params.stop_on_confirmed;

    info!(txid = %txid, interval_secs, stop_on_confirmed, "SSE watch started");

    let client = state.client.clone();

    // Gera um stream infinito de futures, cada uma disparada após o intervalo.
    // `stream::unfold` carrega estado (done: bool) para encerrar após confirmação.
    let event_stream = stream::unfold(
        (client, txid.clone(), false),
        move |(client, txid, done)| async move {
            if done {
                return None; // Encerra o stream
            }

            tokio::time::sleep(Duration::from_secs(interval_secs)).await;

            let (event, is_terminal) = poll_once(&client, &txid, stop_on_confirmed).await;

            Some((
                Ok::<Event, std::convert::Infallible>(event),
                (client, txid, is_terminal),
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
/// Retorna `(Event, is_terminal)`.
/// `is_terminal = true` indica que o stream deve ser encerrado após este evento.
async fn poll_once(client: &MempoolClient, txid: &str, stop_on_confirmed: bool) -> (Event, bool) {
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
        let event = Event::default().event("confirmed").data(data);
        let terminal = stop_on_confirmed;
        (event, terminal)
    } else {
        (Event::default().event("tx_status").data(data), false)
    }
}
