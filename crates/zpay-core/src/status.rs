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
use parking_lot::Mutex;
#[cfg(feature = "in_memory")]
use std::collections::HashMap;

/// Default finality depth used when no operator configuration is supplied.
///
/// Three confirmations matches testnet defaults; mainnet operators are
/// expected to raise this via `ZPAY_FINALITY_DEPTH=10` (or similar).
pub const DEFAULT_FINALITY_DEPTH: u32 = 3;

/// Lifecycle phase of a payment from zpay's perspective.
///
/// The wire vocabulary distinguishes pre-broadcast (`Awaiting`),
/// mempool-accepted (`Broadcast`), included in a block (`Mined`),
/// confirmed past the finality threshold (`Final`), failed settle
/// outcomes (`Failed`), never-prepared ids (`NeverIssued`), and
/// expired-but-unsettled rows (`Expired`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PaymentStatus {
    /// Prepared row exists, no settlement attempt yet. The agent has a
    /// `payment_id` but has not asked zpay to settle.
    Awaiting,
    /// Settlement ledger row exists with a success-kind broadcast
    /// outcome, but the oracle has not yet observed the tx in a block
    /// (`confirmation_count` is null or zero). The tx is in the mempool.
    Broadcast,
    /// Oracle has observed the tx in a block (`confirmation_count >= 1`
    /// and `mined_block_height` is set), but the confirmation count has
    /// not yet reached the operator's configured finality threshold.
    Mined,
    /// Oracle has observed `confirmation_count >= ZPAY_FINALITY_DEPTH`.
    /// This is the terminal success state; SSE streams close here.
    Final,
    /// Settlement ledger row exists with a failure-kind broadcast outcome
    /// (`Rejected`, `InvalidEncoding`, `Unknown`). The agent can retry
    /// settle.
    Failed,
    /// No prepared row and no settlement row exist for this id. Either it
    /// was never issued or the underlying stores were dropped.
    NeverIssued,
    /// Prepared row exists but `expires_at_unix_seconds < now`, and no
    /// settlement row was ever written. The agent must re-prepare.
    Expired,
}

impl PaymentStatus {
    /// Returns `true` when no further state changes are expected from a
    /// live subscriber's perspective.
    ///
    /// `Final`, `Failed`, `NeverIssued`, and `Expired` are terminal.
    /// `Awaiting`, `Broadcast`, and `Mined` are non-terminal: the SSE
    /// stream stays open until `Final` is reached.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Final | Self::Failed | Self::NeverIssued | Self::Expired
        )
    }
}

/// Verification posture of the merchant's intent for this payment.
///
/// Reserved for the upcoming verify oracle that proves the merchant
/// actually signed off on the prepared row. Defaults to `Unverified` on
/// every snapshot today; a future commit wires the oracle and lets the
/// snapshot transition through `VerifyInFlight` and `Verified`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IntentPosture {
    /// Default. No verify oracle wired or the oracle has not been
    /// consulted yet for this payment.
    Unverified,
    /// A verify oracle round-trip is in flight.
    VerifyInFlight,
    /// The verify oracle confirmed the merchant intent matches the
    /// prepared row.
    Verified,
    /// The verify oracle rejected the merchant intent for this payment.
    VerificationFailed,
}

/// Snapshot of a payment's lifecycle returned by [`lookup_payment_status`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentStatusSnapshot {
    /// The payment identifier the caller queried.
    pub payment_id: PaymentId,
    /// Lifecycle phase.
    pub status: PaymentStatus,
    /// Merchant-intent verification posture. Defaults to `Unverified`
    /// until the verify oracle is wired in a future commit.
    pub intent_posture: IntentPosture,
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
/// `finality_depth` is the operator-configured confirmation count at
/// which `Mined` transitions to `Final`. Pass [`DEFAULT_FINALITY_DEPTH`]
/// when no operator override is in play.
///
/// Mapping:
///
/// - Ledger row with success-kind outcome and `confirmation_count >=
///   finality_depth` -> [`PaymentStatus::Final`].
/// - Ledger row with success-kind outcome and `confirmation_count >= 1`
///   (below finality) -> [`PaymentStatus::Mined`].
/// - Ledger row with success-kind outcome and `confirmation_count` of
///   `None` or `0` -> [`PaymentStatus::Broadcast`].
/// - Ledger row with failure-kind outcome -> [`PaymentStatus::Failed`].
/// - No ledger row, prepared row with `expires_at_unix_seconds > now` ->
///   [`PaymentStatus::Awaiting`].
/// - No ledger row, prepared row with `expires_at_unix_seconds <= now`
///   -> [`PaymentStatus::Expired`].
/// - No ledger row and no prepared row -> [`PaymentStatus::NeverIssued`].
///
/// # Errors
///
/// Returns [`StoreError`] when either underlying store fails to read.
pub async fn lookup_payment_status<P, L>(
    payment_id: &PaymentId,
    prepared: &P,
    ledger: &L,
    finality_depth: u32,
) -> Result<PaymentStatusSnapshot, StoreError>
where
    P: PreparedTxStore + ?Sized,
    L: SettlementLedgerStore + ?Sized,
{
    if let Some(entry) = ledger.find(payment_id).await? {
        let status = if entry.broadcast_outcome.is_success() {
            match entry.confirmation_count {
                Some(count) if count >= finality_depth => PaymentStatus::Final,
                Some(count) if count >= 1 => PaymentStatus::Mined,
                _ => PaymentStatus::Broadcast,
            }
        } else {
            PaymentStatus::Failed
        };
        return Ok(PaymentStatusSnapshot {
            payment_id: payment_id.clone(),
            status,
            intent_posture: IntentPosture::Unverified,
            broadcast_outcome: Some(entry.broadcast_outcome),
            settled_at_unix_seconds: Some(entry.settled_at_unix_seconds),
            confirmation_count: entry.confirmation_count,
            mined_block_height: entry.mined_block_height,
        });
    }
    if let Some(entry) = prepared.find_by_payment_id(payment_id).await? {
        let now_unix_seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        let status = if entry.expires_at_unix_seconds > now_unix_seconds {
            PaymentStatus::Awaiting
        } else {
            PaymentStatus::Expired
        };
        return Ok(PaymentStatusSnapshot {
            payment_id: payment_id.clone(),
            status,
            intent_posture: IntentPosture::Unverified,
            broadcast_outcome: None,
            settled_at_unix_seconds: None,
            confirmation_count: None,
            mined_block_height: None,
        });
    }
    Ok(PaymentStatusSnapshot {
        payment_id: payment_id.clone(),
        status: PaymentStatus::NeverIssued,
        intent_posture: IntentPosture::Unverified,
        broadcast_outcome: None,
        settled_at_unix_seconds: None,
        confirmation_count: None,
        mined_block_height: None,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_FINALITY_DEPTH, IntentPosture, PaymentStatus, SettlementLedger,
        SettlementLedgerEntry, SettlementLedgerStore, lookup_payment_status,
    };
    use crate::broadcast::BroadcastOutcome;
    use crate::prepare::test_support::{
        FIXTURE_JKT, FixedTipOracle, fixture_registry, valid_request,
    };
    use crate::prepare::{PreparedTxCache, propose};
    use crate::types::PaymentId;

    #[tokio::test]
    async fn unknown_payment_returns_never_issued_status() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let snapshot = lookup_payment_status(
            &PaymentId("does-not-exist".to_owned()),
            &store,
            &ledger,
            DEFAULT_FINALITY_DEPTH,
        )
        .await
        .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::NeverIssued);
        assert_eq!(snapshot.intent_posture, IntentPosture::Unverified);
        assert!(snapshot.broadcast_outcome.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn prepared_payment_returns_awaiting_status() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let registry = fixture_registry();
        let tip = FixedTipOracle::fixture();
        let preparation = propose(
            valid_request(),
            FIXTURE_JKT.to_owned(),
            &store,
            &registry,
            &tip,
        )
        .await
        .map_err(|_| "propose must accept valid input")?;
        let snapshot = lookup_payment_status(
            &preparation.payment_id,
            &store,
            &ledger,
            DEFAULT_FINALITY_DEPTH,
        )
        .await
        .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Awaiting);
        assert_eq!(snapshot.intent_posture, IntentPosture::Unverified);
        assert!(snapshot.broadcast_outcome.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn ledger_entry_with_accepted_outcome_no_confirmations_is_broadcast()
    -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("broadcast-id".to_owned());
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
        let snapshot = lookup_payment_status(&payment_id, &store, &ledger, DEFAULT_FINALITY_DEPTH)
            .await
            .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Broadcast);
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
    async fn ledger_entry_with_one_confirmation_is_mined() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("mined-id".to_owned());
        ledger
            .record(
                payment_id.clone(),
                SettlementLedgerEntry {
                    broadcast_outcome: BroadcastOutcome::Accepted {
                        transaction_id: "abcd".to_owned(),
                    },
                    settled_at_unix_seconds: 1_700_000_000,
                    confirmation_count: Some(1),
                    mined_block_height: Some(2_000_000),
                },
            )
            .await
            .map_err(|_| "record failed")?;
        let snapshot = lookup_payment_status(&payment_id, &store, &ledger, DEFAULT_FINALITY_DEPTH)
            .await
            .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Mined);
        assert_eq!(snapshot.confirmation_count, Some(1));
        assert_eq!(snapshot.mined_block_height, Some(2_000_000));
        Ok(())
    }

    #[tokio::test]
    async fn ledger_entry_at_finality_depth_is_final() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("final-id".to_owned());
        ledger
            .record(
                payment_id.clone(),
                SettlementLedgerEntry {
                    broadcast_outcome: BroadcastOutcome::Accepted {
                        transaction_id: "abcd".to_owned(),
                    },
                    settled_at_unix_seconds: 1_700_000_000,
                    confirmation_count: Some(3),
                    mined_block_height: Some(2_000_000),
                },
            )
            .await
            .map_err(|_| "record failed")?;
        let snapshot = lookup_payment_status(&payment_id, &store, &ledger, DEFAULT_FINALITY_DEPTH)
            .await
            .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Final);
        assert!(snapshot.status.is_terminal());
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
        let snapshot = lookup_payment_status(&payment_id, &store, &ledger, DEFAULT_FINALITY_DEPTH)
            .await
            .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Failed);
        assert!(snapshot.status.is_terminal());
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
            ledger
                .entry_count()
                .await
                .map_err(|_| "entry_count failed")?,
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
    async fn record_confirmation_drives_mined_then_final_transition() -> Result<(), &'static str> {
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
                .record_confirmation(&payment_id, 1, Some(1_234_567))
                .await
                .map_err(|_| "record_confirmation failed")?
        );
        let mined = lookup_payment_status(&payment_id, &store, &ledger, DEFAULT_FINALITY_DEPTH)
            .await
            .map_err(|_| "lookup failed")?;
        assert_eq!(mined.status, PaymentStatus::Mined);

        assert!(
            ledger
                .record_confirmation(&payment_id, 3, Some(1_234_567))
                .await
                .map_err(|_| "record_confirmation failed")?
        );
        let finalized = lookup_payment_status(&payment_id, &store, &ledger, DEFAULT_FINALITY_DEPTH)
            .await
            .map_err(|_| "lookup failed")?;
        assert_eq!(finalized.status, PaymentStatus::Final);
        assert_eq!(finalized.confirmation_count, Some(3));
        assert_eq!(finalized.mined_block_height, Some(1_234_567));
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

    #[test]
    fn payment_status_serialization_round_trips_every_variant() -> Result<(), &'static str> {
        for (variant, expected) in [
            (PaymentStatus::Awaiting, "\"awaiting\""),
            (PaymentStatus::Broadcast, "\"broadcast\""),
            (PaymentStatus::Mined, "\"mined\""),
            (PaymentStatus::Final, "\"final\""),
            (PaymentStatus::Failed, "\"failed\""),
            (PaymentStatus::NeverIssued, "\"never_issued\""),
            (PaymentStatus::Expired, "\"expired\""),
        ] {
            let json = serde_json::to_string(&variant).map_err(|_| "serialize")?;
            assert_eq!(json, expected);
            let back: PaymentStatus = serde_json::from_str(&json).map_err(|_| "deserialize")?;
            assert_eq!(back, variant);
        }
        Ok(())
    }

    #[test]
    fn intent_posture_serialization_round_trips_every_variant() -> Result<(), &'static str> {
        for (variant, expected) in [
            (IntentPosture::Unverified, "\"unverified\""),
            (IntentPosture::VerifyInFlight, "\"verify_in_flight\""),
            (IntentPosture::Verified, "\"verified\""),
            (IntentPosture::VerificationFailed, "\"verification_failed\""),
        ] {
            let json = serde_json::to_string(&variant).map_err(|_| "serialize")?;
            assert_eq!(json, expected);
            let back: IntentPosture = serde_json::from_str(&json).map_err(|_| "deserialize")?;
            assert_eq!(back, variant);
        }
        Ok(())
    }

    #[test]
    fn is_terminal_matches_locked_set() {
        assert!(!PaymentStatus::Awaiting.is_terminal());
        assert!(!PaymentStatus::Broadcast.is_terminal());
        assert!(!PaymentStatus::Mined.is_terminal());
        assert!(PaymentStatus::Final.is_terminal());
        assert!(PaymentStatus::Failed.is_terminal());
        assert!(PaymentStatus::NeverIssued.is_terminal());
        assert!(PaymentStatus::Expired.is_terminal());
    }
}
