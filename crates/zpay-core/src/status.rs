//! Settlement ledger and payment-status lookup.
//!
//! After a settle call broadcasts, the resulting outcome is inserted into
//! [`SettlementLedger`] so a later `GET /x402/v2/payments/{payment_id}`
//! call can report what happened. The ledger holds every settle attempt
//! (success or failure); a retried settle overwrites the previous entry,
//! so callers see the latest known outcome rather than a stale one.
//!
//! [`lookup_payment_status`] combines a [`PreparedTxCache`] read with a
//! [`SettlementLedger`] read into a single [`PaymentStatusSnapshot`]
//! callers can serialize to JSON.

use std::collections::HashMap;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::broadcast::BroadcastOutcome;
use crate::prepare::PreparedTxCache;
use crate::types::PaymentId;

/// Lifecycle phase of a payment from zpay's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PaymentStatus {
    /// In the prepared-tx cache. The agent has a `payment_id` but has not
    /// yet asked zpay to settle.
    Prepared,
    /// Settled with a success-kind broadcast outcome (Accepted or
    /// Duplicate). The transaction is in the mempool or on chain.
    Settled,
    /// Settled with a failure-kind broadcast outcome (`Rejected`,
    /// `InvalidEncoding`, `Unknown`). The agent can retry settle.
    Failed,
    /// The `payment_id` is not in any zpay state store. Either it
    /// expired, never existed, or the cache and ledger were dropped on
    /// process restart.
    Unknown,
}

/// Snapshot of a payment's lifecycle returned by [`lookup_payment_status`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentStatusSnapshot {
    /// The payment identifier the caller queried.
    pub payment_id: PaymentId,
    /// Lifecycle phase.
    pub status: PaymentStatus,
    /// Last broadcast outcome, if any settle attempt has been made.
    pub broadcast_outcome: Option<BroadcastOutcome>,
    /// Unix-seconds timestamp of the last settle attempt.
    pub settled_at_unix_seconds: Option<i64>,
}

/// Append-mostly ledger of settle outcomes keyed by [`PaymentId`].
///
/// "Append-mostly" because a retried settle overwrites the prior entry
/// for the same `payment_id`; the ledger never grows beyond the active
/// payment set unless retried payments stack up. The implementation is
/// in-memory; a future swap to libSQL will follow the same trait shape
/// (see [`PreparedTxCache`] for the precedent).
#[derive(Debug, Default)]
pub struct SettlementLedger {
    entries: Mutex<HashMap<PaymentId, SettlementLedgerEntry>>,
}

/// Stored snapshot of a single settle attempt.
#[derive(Debug, Clone)]
pub struct SettlementLedgerEntry {
    /// Outcome reported by the chain plane on this attempt.
    pub broadcast_outcome: BroadcastOutcome,
    /// Wall-clock timestamp of the settle attempt.
    pub settled_at_unix_seconds: i64,
}

impl SettlementLedger {
    /// Create a fresh, empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a settle outcome for `payment_id`. Overwrites any prior
    /// entry under the same identifier.
    pub fn record(&self, payment_id: PaymentId, entry: SettlementLedgerEntry) {
        let mut guard = self.entries.lock();
        guard.insert(payment_id, entry);
    }

    /// Look up the recorded outcome for `payment_id`, or `None` if no
    /// settle attempt has been recorded.
    #[must_use]
    pub fn find(&self, payment_id: &PaymentId) -> Option<SettlementLedgerEntry> {
        let guard = self.entries.lock();
        guard.get(payment_id).cloned()
    }

    /// Number of recorded settle outcomes.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        let guard = self.entries.lock();
        guard.len()
    }
}

/// Compute the current lifecycle snapshot for a `payment_id`.
///
/// Reads the settlement ledger first (settle outcome takes precedence
/// over a still-cached preparation in the rare case both exist; settle
/// removes the prepared entry on success-kind outcomes but failure-kind
/// outcomes leave both populated until the agent retries or the
/// preparation expires). Falls back to the prepared-tx cache, then to
/// `Unknown`.
#[must_use]
pub fn lookup_payment_status(
    payment_id: &PaymentId,
    prepared: &PreparedTxCache,
    ledger: &SettlementLedger,
) -> PaymentStatusSnapshot {
    if let Some(entry) = ledger.find(payment_id) {
        let status = if entry.broadcast_outcome.is_success_kind() {
            PaymentStatus::Settled
        } else {
            PaymentStatus::Failed
        };
        return PaymentStatusSnapshot {
            payment_id: payment_id.clone(),
            status,
            broadcast_outcome: Some(entry.broadcast_outcome),
            settled_at_unix_seconds: Some(entry.settled_at_unix_seconds),
        };
    }
    if prepared.find(payment_id).is_some() {
        return PaymentStatusSnapshot {
            payment_id: payment_id.clone(),
            status: PaymentStatus::Prepared,
            broadcast_outcome: None,
            settled_at_unix_seconds: None,
        };
    }
    PaymentStatusSnapshot {
        payment_id: payment_id.clone(),
        status: PaymentStatus::Unknown,
        broadcast_outcome: None,
        settled_at_unix_seconds: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{PaymentStatus, SettlementLedger, SettlementLedgerEntry, lookup_payment_status};
    use crate::broadcast::BroadcastOutcome;
    use crate::prepare::{ChallengeHash, PrepareRequest, PreparedTxCache, ResourceHash, propose};
    use crate::types::{
        EvidencePackHash, MerchantId, PaymentId, PaymentNetwork, PaymentScheme, Zatoshis,
    };

    fn valid_prepare_request() -> PrepareRequest {
        PrepareRequest {
            merchant_id: MerchantId("aether-ai".to_owned()),
            network: PaymentNetwork::Testnet,
            scheme: PaymentScheme::Zcash,
            recipient_unified_address: "utest1exampleaddress".to_owned(),
            amount_zat: Zatoshis(50_000),
            challenge_hash: ChallengeHash([0x11; 32]),
            resource_hash: ResourceHash([0x22; 32]),
            evidence_pack_hash: EvidencePackHash([0x33; 32]),
            expiry_height: 3_217_900,
        }
    }

    #[test]
    fn unknown_payment_returns_unknown_status() {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let snapshot =
            lookup_payment_status(&PaymentId("does-not-exist".to_owned()), &cache, &ledger);
        assert_eq!(snapshot.status, PaymentStatus::Unknown);
        assert!(snapshot.broadcast_outcome.is_none());
    }

    #[test]
    fn prepared_payment_returns_prepared_status() -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let preparation = propose(valid_prepare_request(), &cache)
            .map_err(|_| "propose must accept valid input")?;
        let snapshot = lookup_payment_status(&preparation.payment_id, &cache, &ledger);
        assert_eq!(snapshot.status, PaymentStatus::Prepared);
        assert!(snapshot.broadcast_outcome.is_none());
        Ok(())
    }

    #[test]
    fn ledger_entry_with_accepted_outcome_is_settled() {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("settled-id".to_owned());
        ledger.record(
            payment_id.clone(),
            SettlementLedgerEntry {
                broadcast_outcome: BroadcastOutcome::Accepted {
                    transaction_id: "deadbeef".to_owned(),
                },
                settled_at_unix_seconds: 1_700_000_000,
            },
        );
        let snapshot = lookup_payment_status(&payment_id, &cache, &ledger);
        assert_eq!(snapshot.status, PaymentStatus::Settled);
        match snapshot.broadcast_outcome {
            Some(BroadcastOutcome::Accepted { transaction_id }) => {
                assert_eq!(transaction_id, "deadbeef");
            }
            _ => unreachable!("ledger entry was Accepted; lookup must surface it"),
        }
        assert_eq!(snapshot.settled_at_unix_seconds, Some(1_700_000_000));
    }

    #[test]
    fn ledger_entry_with_rejected_outcome_is_failed() {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("failed-id".to_owned());
        ledger.record(
            payment_id.clone(),
            SettlementLedgerEntry {
                broadcast_outcome: BroadcastOutcome::Rejected {
                    upstream_message: "policy: dust output".to_owned(),
                },
                settled_at_unix_seconds: 1_700_000_001,
            },
        );
        let snapshot = lookup_payment_status(&payment_id, &cache, &ledger);
        assert_eq!(snapshot.status, PaymentStatus::Failed);
    }

    #[test]
    fn ledger_record_overwrites_prior_entry() {
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("retried-id".to_owned());
        ledger.record(
            payment_id.clone(),
            SettlementLedgerEntry {
                broadcast_outcome: BroadcastOutcome::Rejected {
                    upstream_message: "first attempt".to_owned(),
                },
                settled_at_unix_seconds: 1_700_000_000,
            },
        );
        ledger.record(
            payment_id.clone(),
            SettlementLedgerEntry {
                broadcast_outcome: BroadcastOutcome::Accepted {
                    transaction_id: "feedface".to_owned(),
                },
                settled_at_unix_seconds: 1_700_000_100,
            },
        );
        assert_eq!(ledger.entry_count(), 1);
        let entry = ledger.find(&payment_id);
        assert!(matches!(
            entry,
            Some(SettlementLedgerEntry {
                broadcast_outcome: BroadcastOutcome::Accepted { .. },
                ..
            })
        ));
    }
}
