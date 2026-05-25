//! Confirmation oracle: subscribe to zinder `ChainEvents` for live
//! processes; expose pull-mode lookup via the settlement ledger.
//!
//! Implementation lands in M2.

use serde::{Deserialize, Serialize};

/// Confirmation status returned by the oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConfirmationStatus {
    /// In `prepared_tx`, not yet settled.
    Prepared,
    /// In `settlement_ledger`, broadcast succeeded, `confirmations: 0`.
    Settled,
    /// Settlement has reached at least one confirmation.
    Confirmed,
    /// Prepared transaction expired without settling.
    Expired,
    /// Settlement failed permanently.
    Failed,
}

/// Snapshot of a payment's confirmation state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmationSnapshot {
    /// Where the payment is in its lifecycle.
    pub status: ConfirmationStatus,
    /// Number of confirmations observed.
    pub confirmations_count: u32,
    /// Block height at which the transaction was first observed.
    pub mined_block_height: Option<u32>,
}
