//! Broadcast a user-signed Zcash transaction through a chain plane.
//!
//! [`BroadcastClient`] is the abstraction zpay-core uses to hand a raw
//! transaction off to the network. Production deployments wire it to
//! `zinder-client::broadcast_transaction`; tests wire it to an in-memory
//! mock. The trait stays free of zinder types so zpay-core builds without
//! a chain dependency.
//!
//! The categorical outcomes mirror zinder's `BroadcastTransactionResponse`
//! without lossy translation: an `Accepted` response carries the
//! transaction identifier the upstream reports, the other variants carry
//! the upstream-supplied human-readable message.

use serde::{Deserialize, Serialize};

/// Outcome of a broadcast attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BroadcastOutcome {
    /// Transaction accepted into the mempool. Carries the upstream-reported
    /// transaction id.
    Accepted {
        /// Hex-encoded ZIP-244 transaction id reported by the upstream.
        transaction_id: String,
    },
    /// Transaction already known to the network. Caller treats this as
    /// idempotent success.
    Duplicate {
        /// Upstream-supplied human-readable message; safe to log.
        upstream_message: String,
    },
    /// Transaction bytes did not parse.
    InvalidEncoding {
        /// Upstream-supplied human-readable message; safe to log.
        upstream_message: String,
    },
    /// Transaction parsed but failed consensus or policy checks.
    Rejected {
        /// Upstream-supplied human-readable message; safe to log.
        upstream_message: String,
    },
    /// Outcome could not be determined within the broadcast deadline.
    Unknown {
        /// Upstream-supplied human-readable message; safe to log.
        upstream_message: String,
    },
}

impl BroadcastOutcome {
    /// Returns the transaction id reported by an `Accepted` outcome, or
    /// `None` for every other variant.
    #[must_use]
    pub fn transaction_id(&self) -> Option<&str> {
        match self {
            Self::Accepted { transaction_id } => Some(transaction_id.as_str()),
            Self::Duplicate { .. }
            | Self::InvalidEncoding { .. }
            | Self::Rejected { .. }
            | Self::Unknown { .. } => None,
        }
    }

    /// Returns `true` for outcomes where the transaction is on-chain or in a
    /// mempool from zpay's perspective (`Accepted`, `Duplicate`).
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Accepted { .. } | Self::Duplicate { .. })
    }
}

/// Errors raised by [`BroadcastClient`] implementations. These wrap
/// transport-level failures the upstream could not even respond to.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BroadcastError {
    /// The upstream chain plane was unreachable. Retry posture: `retryable`.
    #[error("chain plane unavailable: {reason}")]
    Unavailable {
        /// Operator-facing reason; never includes raw transaction bytes.
        reason: String,
    },
    /// The upstream responded but the response could not be interpreted.
    /// Retry posture: `requires_operator` (almost certainly a wire-protocol
    /// mismatch worth investigating).
    #[error("chain plane response malformed: {reason}")]
    ResponseMalformed {
        /// Operator-facing reason; never includes raw transaction bytes.
        reason: String,
    },
}

/// Abstraction over the chain plane that accepts a hex-encoded raw
/// transaction and reports a categorical outcome.
///
/// Implementors are pinned to `Send + Sync` so a single client can be
/// shared across the Axum router. The `'a` lifetime on the return future
/// is inferred; callers do not need to spell it out.
pub trait BroadcastClient: Send + Sync {
    /// Broadcast the given hex-encoded transaction.
    ///
    /// Implementations must not log `raw_tx_hex` outside trace spans the
    /// caller has explicitly opted in to. The hex string carries the
    /// user's signed transaction and may contain shielded ciphertexts that
    /// the operator never approved for logging.
    fn broadcast(
        &self,
        raw_tx_hex: &str,
    ) -> impl Future<Output = Result<BroadcastOutcome, BroadcastError>> + Send;
}

use std::future::Future;

#[cfg(test)]
mod tests {
    use super::{BroadcastError, BroadcastOutcome};

    #[test]
    fn accepted_outcome_exposes_transaction_id() {
        let outcome = BroadcastOutcome::Accepted {
            transaction_id: "abcd".to_owned(),
        };
        assert_eq!(outcome.transaction_id(), Some("abcd"));
        assert!(outcome.is_success());
    }

    #[test]
    fn duplicate_outcome_is_success_but_has_no_txid() {
        let outcome = BroadcastOutcome::Duplicate {
            upstream_message: "already in mempool".to_owned(),
        };
        assert_eq!(outcome.transaction_id(), None);
        assert!(outcome.is_success());
    }

    #[test]
    fn rejected_outcome_is_not_success_kind() {
        let outcome = BroadcastOutcome::Rejected {
            upstream_message: "consensus failure".to_owned(),
        };
        assert!(!outcome.is_success());
        assert_eq!(outcome.transaction_id(), None);
    }

    #[test]
    fn broadcast_error_unavailable_displays_reason() {
        let err = BroadcastError::Unavailable {
            reason: "dial timeout".to_owned(),
        };
        assert!(format!("{err}").contains("dial timeout"));
    }
}
