use serde::{Deserialize, Serialize};
use thiserror::Error;

const MEMPOOL_API: &str = "https://mempool.space/api";

#[derive(Debug, Error)]
pub enum MempoolError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Transaction not found")]
    NotFound,
}

/// Resposta do endpoint GET /tx/{txid}/status
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TxStatus {
    pub confirmed: bool,
    pub block_height: Option<u64>,
    pub block_hash: Option<String>,
    pub block_time: Option<u64>,
}

/// Subset de campos do endpoint GET /tx/{txid}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TxFee {
    pub fee: u64,
    pub weight: u64,
    pub vsize: u64,
}

/// Payload enviado ao cliente via SSE
#[derive(Debug, Clone, Serialize)]
pub struct TxEvent {
    pub txid: String,
    #[serde(flatten)]
    pub status: TxStatus,
    pub fee: Option<u64>,
    pub vsize: Option<u64>,
}

#[derive(Clone)]
pub struct MempoolClient {
    http: reqwest::Client,
}

impl MempoolClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent("api-mempool-monitor/0.1")
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
        }
    }

    /// Busca o status da transação.
    /// Retorna `Err(MempoolError::NotFound)` quando a API responde 404.
    pub async fn fetch_tx_status(&self, txid: &str) -> Result<TxStatus, MempoolError> {
        let url = format!("{MEMPOOL_API}/tx/{txid}/status");
        let resp = self.http.get(&url).send().await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(MempoolError::NotFound);
        }

        let status = resp.error_for_status()?.json::<TxStatus>().await?;
        Ok(status)
    }

    /// Busca fee e tamanho virtual da transação.
    pub async fn fetch_tx_fee(&self, txid: &str) -> Result<TxFee, MempoolError> {
        let url = format!("{MEMPOOL_API}/tx/{txid}");
        let resp = self.http.get(&url).send().await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(MempoolError::NotFound);
        }

        let tx: serde_json::Value = resp.error_for_status()?.json().await?;

        Ok(TxFee {
            fee: tx["fee"].as_u64().unwrap_or(0),
            weight: tx["weight"].as_u64().unwrap_or(0),
            vsize: tx["vsize"].as_u64().unwrap_or(0),
        })
    }
}
