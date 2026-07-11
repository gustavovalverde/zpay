//! Fetch the exact mined transaction a payment disclosure references.

use std::future::Future;

use serde::{Deserialize, Serialize};

/// Canonical mined transaction context required by ZIP-311 verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DisclosedTransaction {
    /// ZIP-244 transaction id in RPC/display byte order.
    pub txid: [u8; 32],
    /// Canonical serialized consensus transaction bytes.
    pub raw_transaction_bytes: Vec<u8>,
    /// Height of the canonical block containing the transaction.
    pub mined_height: u32,
}

impl DisclosedTransaction {
    /// Constructs canonical mined transaction context.
    #[must_use]
    pub fn new(txid: [u8; 32], raw_transaction_bytes: Vec<u8>, mined_height: u32) -> Self {
        Self {
            txid,
            raw_transaction_bytes,
            mined_height,
        }
    }
}

/// Errors raised by [`DisclosureFetcher`] implementations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FetchError {
    /// Chain plane has no mined record of the transaction. Retry posture: `retryable`.
    #[error("transaction not found on chain plane")]
    NotFound,
    /// Chain plane could not serve the transaction. Retry posture: `retryable`.
    #[error("transaction fetcher unavailable: {reason}")]
    Unavailable {
        /// Operator-facing reason.
        reason: String,
    },
}

/// Resolve an RPC-order transaction id to its canonical mined transaction context.
pub trait DisclosureFetcher: Send + Sync {
    /// Fetch the exact mined transaction `txid` references.
    fn fetch_transaction(
        &self,
        txid: [u8; 32],
    ) -> impl Future<Output = Result<DisclosedTransaction, FetchError>> + Send;
}
