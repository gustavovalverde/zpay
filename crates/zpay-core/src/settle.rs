//! Settle a prepared payment: validate the PoH token, verify the
//! user-signed transaction matches the prepared payload, broadcast
//! through zinder, subscribe the confirmation oracle.
//!
//! Implementation lands in M1.

use serde::{Deserialize, Serialize};

use crate::types::{PaymentId, WatchId};

/// Input to `settle`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettleRequest {
    /// The `payment_id` returned from `prepare`.
    pub payment_id: PaymentId,
    /// Hex-encoded, user-signed, unbroadcast v5 Zcash transaction.
    pub raw_tx_hex: String,
}

/// Output of `settle`. Mirrors zinder's `BroadcastTransactionResponse`
/// without lossy translation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementOutcome {
    /// Transaction identifier of the broadcast transaction.
    pub txid: String,
    /// Categorical outcome of the broadcast attempt.
    pub broadcast_outcome: BroadcastOutcome,
    /// Watch handle for the confirmation oracle.
    pub watch_id: WatchId,
}

/// Categorical broadcast outcomes; mirrors zinder's surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BroadcastOutcome {
    /// Transaction accepted into the mempool.
    Accepted,
    /// Transaction already known to the network; treat as success.
    Duplicate,
    /// Transaction bytes did not parse.
    InvalidEncoding,
    /// Transaction parsed but failed consensus or policy checks.
    Rejected,
    /// Outcome could not be determined within the broadcast deadline.
    Unknown,
}
