//! Confirmation oracle: a pull-mode abstraction over the chain plane
//! that maps a broadcast transaction id back to its current
//! confirmation status.
//!
//! The runtime composition root plugs a zinder-backed implementation
//! behind this trait; zpay-core stays free of zinder types so it can
//! be unit-tested with an in-memory fake.

use serde::{Deserialize, Serialize};

/// Outcome returned by [`ConfirmationOracle::fetch_confirmations`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConfirmationOutcome {
    /// Transaction is in a mined block. Carries the block height the
    /// chain placed it in and the current confirmation depth.
    Mined {
        /// Height of the containing block.
        block_height: u64,
        /// Number of blocks from the containing block to the chain tip,
        /// inclusive (a freshly mined tx has `confirmation_count: 1`).
        confirmation_count: u32,
    },
    /// Transaction is visible in the mempool but not yet mined.
    InMempool,
    /// Chain plane has no record of the transaction.
    NotFound,
    /// Transaction conflicts with the visible canonical chain.
    ConflictingChain,
}

/// Errors raised by [`ConfirmationOracle`] implementations. These wrap
/// transport-level failures the chain plane could not even respond to.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OracleError {
    /// The chain plane was unreachable. Retry posture: `retryable`.
    #[error("oracle unavailable: {reason}")]
    Unavailable {
        /// Operator-facing reason; the upstream error chains as the source.
        reason: String,
    },
    /// The chain plane responded but the response could not be interpreted.
    /// Retry posture: `requires_operator`.
    #[error("oracle response malformed: {reason}")]
    ResponseMalformed {
        /// Operator-facing reason.
        reason: String,
    },
}

/// Abstraction over the chain plane that resolves a transaction id to a
/// confirmation outcome. Implementations are pinned to `Send + Sync` so
/// a single oracle can be shared across background tasks.
pub trait ConfirmationOracle: Send + Sync {
    /// Fetch the current confirmation status for `transaction_id`.
    ///
    /// `transaction_id` is the hex-encoded ZIP-244 txid the broadcast
    /// outcome reported. Implementations decode it; an unparseable id
    /// surfaces as [`OracleError::ResponseMalformed`].
    fn fetch_confirmations(
        &self,
        transaction_id: &str,
    ) -> impl Future<Output = Result<ConfirmationOutcome, OracleError>> + Send;
}

use std::future::Future;
