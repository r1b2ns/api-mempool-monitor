use serde::Serialize;

/// Event type sent to the Live Activity.
///
/// | Value    | When to use                                              |
/// |----------|----------------------------------------------------------|
/// | `Update` | Updates ContentState while keeping the activity alive   |
/// | `End`    | Updates ContentState and terminates the activity        |
#[derive(Debug, Clone, Copy)]
pub enum LiveActivityEvent {
    Update,
    End,
}

impl LiveActivityEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::End    => "end",
        }
    }
}

/// Projected confirmation block position of a pending transaction.
///
/// Determined by comparing the transaction's effective fee rate (sats/vbyte)
/// against the minimum fee rates of the mempool's projected blocks.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BlockPosition {
    /// Fee rate qualifies for the very next block
    NextBlock,
    /// Fee rate qualifies for the second upcoming block
    SecondBlock,
    /// Transaction is not projected in the first two blocks
    Other,
}

/// Dynamic state of the Live Activity — mirrors the Swift ContentState.
///
/// Serialized as `content-state` in the APNS payload.
/// Optional fields use `skip_serializing_if` to omit them from JSON
/// when unavailable (e.g. a freshly propagated tx without fee info).
#[derive(Debug, Serialize)]
pub struct LiveActivityContentState {
    /// Number of confirmations (0 = pending, ≥1 = confirmed)
    pub confirmations: u32,
    /// Status string: "pending" | "confirmed" | "failed"
    pub status: String,
    /// Transaction ID
    #[serde(rename = "txId")]
    pub tx_id: String,
    /// Total amount transferred in BTC (sum of outputs, satoshis ÷ 1e8)
    #[serde(rename = "valueBtc", skip_serializing_if = "Option::is_none")]
    pub value_btc: Option<f64>,
    /// Fee paid in satoshis
    #[serde(rename = "feeSats", skip_serializing_if = "Option::is_none")]
    pub fee_sats: Option<u64>,
    /// Projected block position — present only while the transaction is pending
    #[serde(rename = "blockPosition", skip_serializing_if = "Option::is_none")]
    pub block_position: Option<BlockPosition>,
}
