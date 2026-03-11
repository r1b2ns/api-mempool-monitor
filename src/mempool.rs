use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, warn};

const MEMPOOL_API_MAINNET: &str = "https://mempool.space/api";
const MEMPOOL_API_SIGNET:  &str = "https://mempool.space/signet/api";

#[derive(Debug, Error)]
pub enum MempoolError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Transaction not found")]
    NotFound,
    #[error("Client error from mempool API: HTTP {0}")]
    ClientError(u16),
}

/// Resposta do endpoint GET /tx/{txid}/status
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TxStatus {
    pub confirmed: bool,
    pub block_height: Option<u64>,
    pub block_hash: Option<String>,
    pub block_time: Option<u64>,
}

/// Subset of fields from GET /tx/{txid}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TxFee {
    pub fee: u64,
    pub weight: u64,
    pub vsize: u64,
    /// Sum of all outputs in satoshis (total transferred value)
    pub value_sat: u64,
}

/// A single projected block from GET /api/v1/fees/mempool-blocks.
///
/// The mempool.space API sorts all pending transactions by fee rate (desc) and
/// simulates filling blocks of up to 4 million weight units. Index 0 = next block.
#[derive(Debug, Clone, Deserialize)]
pub struct MempoolBlock {
    /// Virtual size consumed by all transactions in this projected block
    #[serde(rename = "blockVSize")]
    pub block_vsize: f64,
    /// Number of transactions in this projected block
    #[serde(rename = "nTx")]
    pub n_tx: u32,
    /// Median fee rate of transactions in this block (sats/vbyte)
    #[serde(rename = "medianFee")]
    pub median_fee: f64,
    /// Histogram of fee rates [min, …, max] (sats/vbyte).
    /// `fee_range[0]` is the minimum fee rate required to be included in this block.
    #[serde(rename = "feeRange")]
    pub fee_range: Vec<f64>,
}


#[derive(Clone)]
pub struct MempoolClient {
    http: reqwest::Client,
    /// Base URL of the mempool.space API (mainnet or signet)
    base_url: String,
}

impl MempoolClient {
    /// Creates a new client. Reads `MEMPOOL_NETWORK` from the environment:
    /// - `"signet"` → uses the signet API endpoint
    /// - anything else (or unset) → uses mainnet
    pub fn new() -> Self {
        let base_url = match std::env::var("MEMPOOL_NETWORK").as_deref() {
            Ok("signet") => MEMPOOL_API_SIGNET,
            _            => MEMPOOL_API_MAINNET,
        }.to_string();

        Self {
            http: reqwest::Client::builder()
                .user_agent("api-mempool-monitor/0.1")
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
            base_url,
        }
    }

    /// Busca o status da transação.
    /// Retorna `Err(MempoolError::NotFound)` quando a API responde 404.
    pub async fn fetch_tx_status(&self, txid: &str) -> Result<TxStatus, MempoolError> {
        let url = format!("{}/tx/{txid}/status", self.base_url);
        debug!(txid = %txid, url = %url, "→ GET tx status");

        let resp = self.http.get(&url).send().await?;
        let http_status = resp.status();

        if http_status == reqwest::StatusCode::NOT_FOUND {
            warn!(txid = %txid, url = %url, "← 404 tx status (não encontrada)");
            return Err(MempoolError::NotFound);
        }

        if http_status.is_client_error() {
            warn!(txid = %txid, url = %url, status = %http_status, "← 4XX tx status");
            return Err(MempoolError::ClientError(http_status.as_u16()));
        }

        let status = resp.error_for_status()?.json::<TxStatus>().await?;
        debug!(
            txid       = %txid,
            http       = %http_status,
            confirmed  = status.confirmed,
            block_height = ?status.block_height,
            "← tx status OK"
        );
        Ok(status)
    }

    /// Fetches the current best block height (chain tip).
    ///
    /// Calls `GET /api/blocks/tip/height`, which returns a plain-text integer.
    pub async fn fetch_chain_tip(&self) -> Result<u64, MempoolError> {
        let url = format!("{}/blocks/tip/height", self.base_url);
        debug!(url = %url, "→ GET chain tip height");

        let resp = self.http.get(&url).send().await?;
        let http_status = resp.status();
        let text = resp.error_for_status()?.text().await?;
        let height: u64 = text.trim().parse().unwrap_or(0);

        debug!(http = %http_status, height, "← chain tip height OK");
        Ok(height)
    }

    /// Fetches the projected next mempool blocks.
    ///
    /// Calls `GET /api/v1/fees/mempool-blocks`, which returns an array of projected
    /// blocks ordered by priority (index 0 = next block, index 1 = second block, …).
    /// Each entry includes the fee-rate range required to be included in that block.
    pub async fn fetch_mempool_blocks(&self) -> Result<Vec<MempoolBlock>, MempoolError> {
        let url = format!("{}/v1/fees/mempool-blocks", self.base_url);
        debug!(url = %url, "→ GET mempool blocks");

        let resp   = self.http.get(&url).send().await?;
        let status = resp.status();
        let blocks = resp.error_for_status()?.json::<Vec<MempoolBlock>>().await?;

        debug!(http = %status, count = blocks.len(), "← mempool blocks OK");
        Ok(blocks)
    }

    /// Fetches fee and virtual size of a transaction.
    pub async fn fetch_tx_fee(&self, txid: &str) -> Result<TxFee, MempoolError> {
        let url = format!("{}/tx/{txid}", self.base_url);
        debug!(txid = %txid, url = %url, "→ GET tx details");

        let resp = self.http.get(&url).send().await?;
        let http_status = resp.status();

        if http_status == reqwest::StatusCode::NOT_FOUND {
            warn!(txid = %txid, url = %url, "← 404 tx details (não encontrada)");
            return Err(MempoolError::NotFound);
        }

        if http_status.is_client_error() {
            warn!(txid = %txid, url = %url, status = %http_status, "← 4XX tx details");
            return Err(MempoolError::ClientError(http_status.as_u16()));
        }

        let tx: serde_json::Value = resp.error_for_status()?.json().await?;

        // Soma todos os outputs para obter o valor total transferido
        let value_sat = tx["vout"]
            .as_array()
            .map(|outputs| outputs.iter().map(|o| o["value"].as_u64().unwrap_or(0)).sum())
            .unwrap_or(0);

        let result = TxFee {
            fee: tx["fee"].as_u64().unwrap_or(0),
            weight: tx["weight"].as_u64().unwrap_or(0),
            vsize: tx["vsize"].as_u64().unwrap_or(0),
            value_sat,
        };
        debug!(
            txid      = %txid,
            http      = %http_status,
            fee       = result.fee,
            vsize     = result.vsize,
            value_sat = result.value_sat,
            "← tx details OK"
        );
        Ok(result)
    }
}
