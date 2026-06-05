//! Settlement-side projection of a chain-plane broadcast outcome.
//!
//! [`BroadcastOutcome`] is the persisted shape the settlement ledger holds
//! for every settled payment; the categorical variants align with the
//! upstream `zinder` `BroadcastTransactionResponse` and survive across
//! restarts via libSQL.
//!
//! As of Phase 2f of Proposal-0003, the broadcast TRAIT lives in zally:
//! `zally_chain::Submitter` is the canonical contract any chain plane must
//! satisfy, and zpay's settle path consumes it directly. The mapping from
//! `zally_chain::SubmitOutcome` to this projection lives in
//! [`BroadcastOutcome::from_submit_outcome`] so the persistence shape stays
//! stable even as the trait surface evolves upstream.

use serde::{Deserialize, Serialize};

/// Outcome of a broadcast attempt as persisted in the settlement ledger.
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

    /// Maps a [`zally_chain::SubmitOutcome`] into the settlement-side
    /// projection. zally's `Queued` variant (broadcast accepted but not yet
    /// in the chain's mempool snapshot) folds into `Accepted` because both
    /// represent a successful handoff from zpay's perspective; the
    /// confirmation oracle is the authoritative source for "actually on
    /// chain" via [`Self::is_success`] and the subsequent watch.
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "zally_chain::SubmitOutcome is non_exhaustive; unknown variants fall through to Unknown so a future zally variant does not silently coerce to a success."
    )]
    #[must_use]
    pub fn from_submit_outcome(outcome: zally_chain::SubmitOutcome) -> Self {
        match outcome {
            zally_chain::SubmitOutcome::Accepted { tx_id }
            | zally_chain::SubmitOutcome::Queued { tx_id } => Self::Accepted {
                transaction_id: tx_id.to_rpc_hex(),
            },
            zally_chain::SubmitOutcome::Duplicate { tx_id } => Self::Duplicate {
                upstream_message: format!("already in mempool: {}", tx_id.to_rpc_hex()),
            },
            zally_chain::SubmitOutcome::Rejected { reason, detail } => Self::Rejected {
                upstream_message: format!("{reason:?}: {detail}"),
            },
            _ => Self::Unknown {
                upstream_message: "submitter returned an unrecognised outcome variant".to_owned(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BroadcastOutcome;
    use zally_chain::SubmitOutcome;
    use zally_core::TxId;

    fn fixture_txid() -> TxId {
        TxId::from_bytes([0xab; 32])
    }

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
    fn submit_outcome_accepted_maps_to_accepted_with_rpc_hex_txid() {
        let mapped = BroadcastOutcome::from_submit_outcome(SubmitOutcome::Accepted {
            tx_id: fixture_txid(),
        });
        assert!(mapped.is_success());
        assert_eq!(
            mapped.transaction_id(),
            Some(fixture_txid().to_rpc_hex().as_str())
        );
    }

    #[test]
    fn submit_outcome_queued_maps_to_accepted_for_persistence() {
        let mapped = BroadcastOutcome::from_submit_outcome(SubmitOutcome::Queued {
            tx_id: fixture_txid(),
        });
        assert!(mapped.is_success());
    }

    #[test]
    fn submit_outcome_rejected_maps_to_rejected_with_reason_and_detail() {
        let mapped = BroadcastOutcome::from_submit_outcome(SubmitOutcome::Rejected {
            reason: zally_chain::RejectionReason::Unknown,
            detail: "consensus failure".to_owned(),
        });
        assert!(matches!(mapped, BroadcastOutcome::Rejected { .. }));
        assert!(!mapped.is_success());
    }
}
