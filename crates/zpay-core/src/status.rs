//! Settlement ledger and payment-status lookup.
//!
//! After a settle call broadcasts, the resulting outcome is inserted into
//! the [`SettlementLedgerStore`] so a later `GET /x402/v2/payments/{payment_id}`
//! call can report what happened. The ledger holds every settle attempt
//! (success or failure); a retried settle overwrites the previous entry,
//! so callers see the latest known outcome rather than a stale one.
//!
//! [`lookup_payment_status`] combines a [`PreparedTxStore`] read with a
//! [`SettlementLedgerStore`] read into a single [`PaymentStatusSnapshot`]
//! callers can serialize to JSON.

use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::broadcast::BroadcastOutcome;
use crate::prepare::PreparedTxStore;
use crate::store::StoreError;
use crate::types::PaymentId;

#[cfg(feature = "in_memory")]
use std::collections::HashMap;
#[cfg(feature = "in_memory")]
use parking_lot::Mutex;

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
    /// Confirmation count reported by the oracle on its last poll.
    /// `None` until the oracle has observed the tx; a freshly mined tx
    /// is `Some(1)`, a tx still in mempool stays `Some(0)`.
    pub confirmation_count: Option<u32>,
    /// Block height that includes the tx, when known.
    pub mined_block_height: Option<u64>,
}

/// Stored snapshot of a single settle attempt.
#[derive(Debug, Clone)]
pub struct SettlementLedgerEntry {
    /// Outcome reported by the chain plane on this attempt.
    pub broadcast_outcome: BroadcastOutcome,
    /// Wall-clock timestamp of the settle attempt.
    pub settled_at_unix_seconds: i64,
    /// Latest confirmation count observed by the oracle. `None` until
    /// the first oracle poll completes.
    pub confirmation_count: Option<u32>,
    /// Block height that includes the tx, when the oracle has seen it
    /// mined.
    pub mined_block_height: Option<u64>,
}

/// Storage trait for the settle outcome ledger.
///
/// Append-mostly: a retried settle overwrites the prior row for the
/// same `payment_id`. The ledger never grows beyond the active
/// payment set unless retried payments stack up.
pub trait SettlementLedgerStore: Send + Sync {
    /// Record a settle outcome for `payment_id`. Overwrites any prior
    /// entry under the same identifier.
    fn record(
        &self,
        payment_id: PaymentId,
        entry: SettlementLedgerEntry,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Look up the recorded outcome for `payment_id`, or `None` if no
    /// settle attempt has been recorded.
    fn find(
        &self,
        payment_id: &PaymentId,
    ) -> impl Future<Output = Result<Option<SettlementLedgerEntry>, StoreError>> + Send;

    /// Number of recorded settle outcomes. Useful for tests and
    /// operator-side metrics.
    fn entry_count(&self) -> impl Future<Output = Result<usize, StoreError>> + Send;

    /// Collect `(payment_id, transaction_id)` pairs for every entry
    /// whose broadcast outcome was a success kind. The background
    /// confirmation oracle iterates this list each tick.
    fn success_kind_transactions(
        &self,
    ) -> impl Future<Output = Result<Vec<(PaymentId, String)>, StoreError>> + Send;

    /// Update the confirmation count and mined block height for an
    /// existing ledger entry. Returns `true` when an entry was found
    /// and updated, `false` when the `payment_id` was not in the
    /// ledger.
    fn record_confirmation(
        &self,
        payment_id: &PaymentId,
        confirmation_count: u32,
        mined_block_height: Option<u64>,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;
}

/// In-memory implementation of [`SettlementLedgerStore`].
///
/// Suitable for unit tests and ad-hoc local development. Production
/// runtimes compose the libSQL implementation from `zpay-store`.
#[cfg(feature = "in_memory")]
#[derive(Debug, Default)]
pub struct SettlementLedger {
    entries: Mutex<HashMap<PaymentId, SettlementLedgerEntry>>,
}

#[cfg(feature = "in_memory")]
impl SettlementLedger {
    /// Create a fresh, empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(feature = "in_memory")]
impl SettlementLedgerStore for SettlementLedger {
    async fn record(
        &self,
        payment_id: PaymentId,
        entry: SettlementLedgerEntry,
    ) -> Result<(), StoreError> {
        let mut guard = self.entries.lock();
        guard.insert(payment_id, entry);
        drop(guard);
        Ok(())
    }

    async fn find(
        &self,
        payment_id: &PaymentId,
    ) -> Result<Option<SettlementLedgerEntry>, StoreError> {
        let guard = self.entries.lock();
        Ok(guard.get(payment_id).cloned())
    }

    async fn entry_count(&self) -> Result<usize, StoreError> {
        let guard = self.entries.lock();
        Ok(guard.len())
    }

    async fn success_kind_transactions(&self) -> Result<Vec<(PaymentId, String)>, StoreError> {
        let guard = self.entries.lock();
        let pairs: Vec<(PaymentId, String)> = guard
            .iter()
            .filter_map(|(payment_id, entry)| {
                entry
                    .broadcast_outcome
                    .transaction_id()
                    .map(|txid| (payment_id.clone(), txid.to_owned()))
            })
            .collect();
        drop(guard);
        Ok(pairs)
    }

    async fn record_confirmation(
        &self,
        payment_id: &PaymentId,
        confirmation_count: u32,
        mined_block_height: Option<u64>,
    ) -> Result<bool, StoreError> {
        let mut guard = self.entries.lock();
        Ok(guard.get_mut(payment_id).is_some_and(|entry| {
            entry.confirmation_count = Some(confirmation_count);
            if mined_block_height.is_some() {
                entry.mined_block_height = mined_block_height;
            }
            true
        }))
    }
}

/// Compute the current lifecycle snapshot for a `payment_id`.
///
/// Reads the settlement ledger first (settle outcome takes precedence
/// over a still-cached preparation in the rare case both exist; settle
/// removes the prepared entry on success-kind outcomes but failure-kind
/// outcomes leave both populated until the agent retries or the
/// preparation expires). Falls back to the prepared-tx store, then to
/// `Unknown`.
///
/// # Errors
///
/// Returns [`StoreError`] when either underlying store fails to read.
pub async fn lookup_payment_status<P, L>(
    payment_id: &PaymentId,
    prepared: &P,
    ledger: &L,
) -> Result<PaymentStatusSnapshot, StoreError>
where
    P: PreparedTxStore + ?Sized,
    L: SettlementLedgerStore + ?Sized,
{
    if let Some(entry) = ledger.find(payment_id).await? {
        let status = if entry.broadcast_outcome.is_success_kind() {
            PaymentStatus::Settled
        } else {
            PaymentStatus::Failed
        };
        return Ok(PaymentStatusSnapshot {
            payment_id: payment_id.clone(),
            status,
            broadcast_outcome: Some(entry.broadcast_outcome),
            settled_at_unix_seconds: Some(entry.settled_at_unix_seconds),
            confirmation_count: entry.confirmation_count,
            mined_block_height: entry.mined_block_height,
        });
    }
    if let Some(entry) = prepared.find_by_payment_id(payment_id).await? {
        // An expired-but-not-yet-swept entry must look the same as an
        // unknown one to callers. Otherwise an agent would build a tx
        // against a stale preparation that settle will refuse.
        let now_unix_seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        if entry.expires_at_unix_seconds > now_unix_seconds {
            return Ok(PaymentStatusSnapshot {
                payment_id: payment_id.clone(),
                status: PaymentStatus::Prepared,
                broadcast_outcome: None,
                settled_at_unix_seconds: None,
                confirmation_count: None,
                mined_block_height: None,
            });
        }
    }
    Ok(PaymentStatusSnapshot {
        payment_id: payment_id.clone(),
        status: PaymentStatus::Unknown,
        broadcast_outcome: None,
        settled_at_unix_seconds: None,
        confirmation_count: None,
        mined_block_height: None,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        PaymentStatus, SettlementLedger, SettlementLedgerEntry, SettlementLedgerStore,
        lookup_payment_status,
    };
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
            validity_seconds: None,
            idempotency_key: None,
        }
    }

    #[tokio::test]
    async fn unknown_payment_returns_unknown_status() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let snapshot =
            lookup_payment_status(&PaymentId("does-not-exist".to_owned()), &store, &ledger)
                .await
                .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Unknown);
        assert!(snapshot.broadcast_outcome.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn prepared_payment_returns_prepared_status() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let preparation = propose(valid_prepare_request(), &store)
            .await
            .map_err(|_| "propose must accept valid input")?;
        let snapshot = lookup_payment_status(&preparation.payment_id, &store, &ledger)
            .await
            .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Prepared);
        assert!(snapshot.broadcast_outcome.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn ledger_entry_with_accepted_outcome_is_settled() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("settled-id".to_owned());
        ledger
            .record(
                payment_id.clone(),
                SettlementLedgerEntry {
                    broadcast_outcome: BroadcastOutcome::Accepted {
                        transaction_id: "deadbeef".to_owned(),
                    },
                    settled_at_unix_seconds: 1_700_000_000,
                    confirmation_count: None,
                    mined_block_height: None,
                },
            )
            .await
            .map_err(|_| "record failed")?;
        let snapshot = lookup_payment_status(&payment_id, &store, &ledger)
            .await
            .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Settled);
        match snapshot.broadcast_outcome {
            Some(BroadcastOutcome::Accepted { transaction_id }) => {
                assert_eq!(transaction_id, "deadbeef");
            }
            _ => return Err("ledger entry was Accepted; lookup must surface it"),
        }
        assert_eq!(snapshot.settled_at_unix_seconds, Some(1_700_000_000));
        Ok(())
    }

    #[tokio::test]
    async fn ledger_entry_with_rejected_outcome_is_failed() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("failed-id".to_owned());
        ledger
            .record(
                payment_id.clone(),
                SettlementLedgerEntry {
                    broadcast_outcome: BroadcastOutcome::Rejected {
                        upstream_message: "policy: dust output".to_owned(),
                    },
                    settled_at_unix_seconds: 1_700_000_001,
                    confirmation_count: None,
                    mined_block_height: None,
                },
            )
            .await
            .map_err(|_| "record failed")?;
        let snapshot = lookup_payment_status(&payment_id, &store, &ledger)
            .await
            .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Failed);
        Ok(())
    }

    #[tokio::test]
    async fn ledger_record_overwrites_prior_entry() -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("retried-id".to_owned());
        ledger
            .record(
                payment_id.clone(),
                SettlementLedgerEntry {
                    broadcast_outcome: BroadcastOutcome::Rejected {
                        upstream_message: "first attempt".to_owned(),
                    },
                    settled_at_unix_seconds: 1_700_000_000,
                    confirmation_count: None,
                    mined_block_height: None,
                },
            )
            .await
            .map_err(|_| "first record failed")?;
        ledger
            .record(
                payment_id.clone(),
                SettlementLedgerEntry {
                    broadcast_outcome: BroadcastOutcome::Accepted {
                        transaction_id: "feedface".to_owned(),
                    },
                    settled_at_unix_seconds: 1_700_000_100,
                    confirmation_count: None,
                    mined_block_height: None,
                },
            )
            .await
            .map_err(|_| "second record failed")?;
        assert_eq!(
            ledger.entry_count().await.map_err(|_| "entry_count failed")?,
            1
        );
        let entry = ledger.find(&payment_id).await.map_err(|_| "find failed")?;
        assert!(matches!(
            entry,
            Some(SettlementLedgerEntry {
                broadcast_outcome: BroadcastOutcome::Accepted { .. },
                ..
            })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn record_confirmation_updates_existing_entry() -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("watch-me".to_owned());
        ledger
            .record(
                payment_id.clone(),
                SettlementLedgerEntry {
                    broadcast_outcome: BroadcastOutcome::Accepted {
                        transaction_id: "feedface".to_owned(),
                    },
                    settled_at_unix_seconds: 1_700_000_000,
                    confirmation_count: None,
                    mined_block_height: None,
                },
            )
            .await
            .map_err(|_| "record failed")?;

        let store = PreparedTxCache::new();
        assert!(
            ledger
                .record_confirmation(&payment_id, 3, Some(1_234_567))
                .await
                .map_err(|_| "record_confirmation failed")?
        );
        let snapshot = lookup_payment_status(&payment_id, &store, &ledger)
            .await
            .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.confirmation_count, Some(3));
        assert_eq!(snapshot.mined_block_height, Some(1_234_567));
        Ok(())
    }

    #[tokio::test]
    async fn record_confirmation_returns_false_for_missing_entry() -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("not-recorded".to_owned());
        assert!(
            !ledger
                .record_confirmation(&payment_id, 1, None)
                .await
                .map_err(|_| "record_confirmation failed")?
        );
        Ok(())
    }

    #[tokio::test]
    async fn success_kind_transactions_skips_failure_outcomes() -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        ledger
            .record(
                PaymentId("ok".to_owned()),
                SettlementLedgerEntry {
                    broadcast_outcome: BroadcastOutcome::Accepted {
                        transaction_id: "abcd".to_owned(),
                    },
                    settled_at_unix_seconds: 1_700_000_000,
                    confirmation_count: None,
                    mined_block_height: None,
                },
            )
            .await
            .map_err(|_| "record failed")?;
        ledger
            .record(
                PaymentId("fail".to_owned()),
                SettlementLedgerEntry {
                    broadcast_outcome: BroadcastOutcome::Rejected {
                        upstream_message: "no".to_owned(),
                    },
                    settled_at_unix_seconds: 1_700_000_001,
                    confirmation_count: None,
                    mined_block_height: None,
                },
            )
            .await
            .map_err(|_| "record failed")?;
        let pairs = ledger
            .success_kind_transactions()
            .await
            .map_err(|_| "success_kind_transactions failed")?;
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, PaymentId("ok".to_owned()));
        assert_eq!(pairs[0].1, "abcd");
        Ok(())
    }
}
