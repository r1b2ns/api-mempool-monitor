use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, warn};

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
    /// Soma de todos os outputs em satoshis (valor total transferido)
    pub value_sat: u64,
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
        debug!(txid = %txid, url = %url, "→ GET tx status");

        let resp = self.http.get(&url).send().await?;
        let http_status = resp.status();

        if http_status == reqwest::StatusCode::NOT_FOUND {
            warn!(txid = %txid, url = %url, "← 404 tx status (não encontrada)");
            return Err(MempoolError::NotFound);
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

    /// Busca fee e tamanho virtual da transação.
    pub async fn fetch_tx_fee(&self, txid: &str) -> Result<TxFee, MempoolError> {
        let url = format!("{MEMPOOL_API}/tx/{txid}");
        debug!(txid = %txid, url = %url, "→ GET tx details");

        let resp = self.http.get(&url).send().await?;
        let http_status = resp.status();

        if http_status == reqwest::StatusCode::NOT_FOUND {
            warn!(txid = %txid, url = %url, "← 404 tx details (não encontrada)");
            return Err(MempoolError::NotFound);
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
