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
use crate::chain_status::ChainStatusView;
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
    ///
    /// Not immutable: a reorg that drops the containing block returns the
    /// payment to `Broadcast`.
    Mined,
    /// Oracle has observed `confirmation_count >= ZPAY_FINALITY_DEPTH`.
    ///
    /// A depth-based UX milestone, not immutability. A reorg deeper than
    /// the finality threshold still returns the payment to `Broadcast`;
    /// the `settled` flag, not this status, marks the point past which no
    /// reorg can move the payment.
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
    /// Returns `true` when the status alone is terminal, independent of
    /// chain depth.
    ///
    /// `Failed`, `NeverIssued`, and `Expired` are terminal by status.
    /// `Final` is not: it can regress to `Broadcast` on a reorg. A mined
    /// payment reaches immutability through the `settled` flag on the
    /// snapshot, not through this status. See
    /// [`PaymentStatusSnapshot::stream_closed`].
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Failed | Self::NeverIssued | Self::Expired)
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
    /// Number of times a reorg has returned this payment from a mined
    /// status to `Broadcast`. Zero for a payment that never regressed.
    pub reorg_count: u32,
    /// `true` once the containing block is at or below the chain's settled
    /// tip, past which no reorg can move the payment. Immutable success.
    pub settled: bool,
}

impl PaymentStatusSnapshot {
    /// Returns `true` when a live SSE stream should close after emitting
    /// this snapshot.
    ///
    /// Closes on immutable success (`settled`) or on a status-terminal
    /// outcome (`Failed`, `NeverIssued`, `Expired`). A `Final` snapshot
    /// that is not yet settled keeps the stream open so a later reorg
    /// downgrade still reaches the subscriber.
    #[must_use]
    pub const fn stream_closed(&self) -> bool {
        self.settled || self.status.is_terminal()
    }
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
    /// Number of reorg downgrades this row has taken.
    pub reorg_count: u32,
    /// Unix-seconds timestamp of the most recent reorg downgrade, or
    /// `None` if the row has never regressed.
    pub last_reorged_at: Option<i64>,
    /// Prepared `expiry_height` carried onto the ledger at settle time.
    /// The success path removes the prepared row, so the ledger is the
    /// only place the status projection can read the expiry height for an
    /// unmined row's expiry-lapse check. `None` on rows written before the
    /// column existed.
    pub expiry_height: Option<u32>,
}

/// A success-kind ledger row the confirmation poll iterates each tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuccessKindRow {
    /// Payment the row belongs to.
    pub payment_id: PaymentId,
    /// Hex-encoded ZIP-244 transaction id the broadcast outcome reported.
    pub transaction_id: String,
    /// Containing block height, or `None` while the tx is unmined.
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

    /// Collect a [`SuccessKindRow`] for every entry whose broadcast
    /// outcome was a success kind. The background confirmation oracle
    /// iterates this list each tick.
    fn success_kind_transactions(
        &self,
    ) -> impl Future<Output = Result<Vec<SuccessKindRow>, StoreError>> + Send;

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

    /// Return a single currently-mined row to `Broadcast` after the chain
    /// plane stopped reporting it mined.
    ///
    /// Clears `mined_block_height`, resets `confirmation_count` to zero,
    /// increments `reorg_count`, and stamps `last_reorged_at`. Returns
    /// `true` when a mined row was downgraded, `false` when the row was
    /// absent or already unmined (so the call is idempotent under repeat
    /// reorg signals).
    fn downgrade_on_reorg(
        &self,
        payment_id: &PaymentId,
        reorged_at_unix_seconds: i64,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// Return every currently-mined success-kind row whose
    /// `mined_block_height` lies within the inclusive reverted range to
    /// `Broadcast`, applying the same field changes as
    /// [`Self::downgrade_on_reorg`]. Returns the downgraded payment ids so
    /// the caller can emit corrective snapshots.
    fn downgrade_reorged_range(
        &self,
        reverted_start_height: u64,
        reverted_end_height: u64,
        reorged_at_unix_seconds: i64,
    ) -> impl Future<Output = Result<Vec<PaymentId>, StoreError>> + Send;
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

    async fn success_kind_transactions(&self) -> Result<Vec<SuccessKindRow>, StoreError> {
        let guard = self.entries.lock();
        let rows: Vec<SuccessKindRow> = guard
            .iter()
            .filter_map(|(payment_id, entry)| {
                entry
                    .broadcast_outcome
                    .transaction_id()
                    .map(|txid| SuccessKindRow {
                        payment_id: payment_id.clone(),
                        transaction_id: txid.to_owned(),
                        mined_block_height: entry.mined_block_height,
                    })
            })
            .collect();
        drop(guard);
        Ok(rows)
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

    async fn downgrade_on_reorg(
        &self,
        payment_id: &PaymentId,
        reorged_at_unix_seconds: i64,
    ) -> Result<bool, StoreError> {
        let mut guard = self.entries.lock();
        Ok(guard.get_mut(payment_id).is_some_and(|entry| {
            if entry.mined_block_height.is_none() {
                return false;
            }
            entry.mined_block_height = None;
            entry.confirmation_count = Some(0);
            entry.reorg_count = entry.reorg_count.saturating_add(1);
            entry.last_reorged_at = Some(reorged_at_unix_seconds);
            true
        }))
    }

    async fn downgrade_reorged_range(
        &self,
        reverted_start_height: u64,
        reverted_end_height: u64,
        reorged_at_unix_seconds: i64,
    ) -> Result<Vec<PaymentId>, StoreError> {
        let mut guard = self.entries.lock();
        let mut downgraded = Vec::new();
        for (payment_id, entry) in guard.iter_mut() {
            let Some(height) = entry.mined_block_height else {
                continue;
            };
            if height < reverted_start_height || height > reverted_end_height {
                continue;
            }
            entry.mined_block_height = None;
            entry.confirmation_count = Some(0);
            entry.reorg_count = entry.reorg_count.saturating_add(1);
            entry.last_reorged_at = Some(reorged_at_unix_seconds);
            downgraded.push(payment_id.clone());
        }
        drop(guard);
        Ok(downgraded)
    }
}

/// Compute the current lifecycle snapshot for a `payment_id`.
///
/// `finality_depth` is the operator-configured confirmation count at
/// which `Mined` becomes `Final`. Pass [`DEFAULT_FINALITY_DEPTH`] when no
/// operator override is in play. `chain_view` carries the visible and
/// settled tips; pass [`ChainStatusView::default`] where no chain read is
/// available (the projection then reports no row settled and never lapses
/// an unmined row).
///
/// Ledger-row mapping (success-kind outcome):
///
/// - `mined_block_height` known and `confirmation_count >= finality_depth`
///   -> [`PaymentStatus::Final`].
/// - `mined_block_height` known and `confirmation_count >= 1` (below
///   finality) -> [`PaymentStatus::Mined`].
/// - unmined and the visible tip has passed the row's `expiry_height` ->
///   [`PaymentStatus::Expired`].
/// - unmined otherwise -> [`PaymentStatus::Broadcast`].
///
/// A success-kind row is reported `settled` once its
/// `mined_block_height` is at or below the settled tip. A failure-kind
/// outcome maps to [`PaymentStatus::Failed`].
///
/// No-ledger-row mapping:
///
/// - prepared row with `expires_at_unix_seconds > now` ->
///   [`PaymentStatus::Awaiting`], otherwise [`PaymentStatus::Expired`].
/// - no prepared row -> [`PaymentStatus::NeverIssued`].
///
/// # Errors
///
/// Returns [`StoreError`] when either underlying store fails to read.
pub async fn lookup_payment_status<P, L>(
    payment_id: &PaymentId,
    prepared: &P,
    ledger: &L,
    finality_depth: u32,
    chain_view: ChainStatusView,
) -> Result<PaymentStatusSnapshot, StoreError>
where
    P: PreparedTxStore + ?Sized,
    L: SettlementLedgerStore + ?Sized,
{
    if let Some(entry) = ledger.find(payment_id).await? {
        let (status, settled) = if entry.broadcast_outcome.is_success() {
            classify_success_row(&entry, finality_depth, chain_view)
        } else {
            (PaymentStatus::Failed, false)
        };
        return Ok(PaymentStatusSnapshot {
            payment_id: payment_id.clone(),
            status,
            intent_posture: IntentPosture::Unverified,
            broadcast_outcome: Some(entry.broadcast_outcome),
            settled_at_unix_seconds: Some(entry.settled_at_unix_seconds),
            confirmation_count: entry.confirmation_count,
            mined_block_height: entry.mined_block_height,
            reorg_count: entry.reorg_count,
            settled,
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
            reorg_count: 0,
            settled: false,
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
        reorg_count: 0,
        settled: false,
    })
}

/// Map a success-kind ledger row to its `(status, settled)` pair.
///
/// A mined row is `Final` at or above finality depth and `Mined` below
/// it, and is `settled` once its height is at or below the settled tip.
/// An unmined row is `Expired` when the visible tip has passed its
/// `expiry_height` (the reorged-out or never-mined window has lapsed) and
/// `Broadcast` otherwise.
fn classify_success_row(
    entry: &SettlementLedgerEntry,
    finality_depth: u32,
    chain_view: ChainStatusView,
) -> (PaymentStatus, bool) {
    if let Some(height) = entry.mined_block_height
        && matches!(entry.confirmation_count, Some(count) if count >= 1)
    {
        let confirmation_count = entry.confirmation_count.unwrap_or(0);
        let status = if confirmation_count >= finality_depth {
            PaymentStatus::Final
        } else {
            PaymentStatus::Mined
        };
        return (status, chain_view.is_settled_at(height));
    }
    let lapsed = entry
        .expiry_height
        .is_some_and(|expiry| chain_view.is_lapsed_at(u64::from(expiry)));
    let status = if lapsed {
        PaymentStatus::Expired
    } else {
        PaymentStatus::Broadcast
    };
    (status, false)
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_FINALITY_DEPTH, IntentPosture, PaymentStatus, SettlementLedger,
        SettlementLedgerEntry, SettlementLedgerStore, lookup_payment_status,
    };
    use crate::broadcast::BroadcastOutcome;
    use crate::chain_status::ChainStatusView;
    use crate::prepare::test_support::{
        FIXTURE_JKT, FixedTipOracle, fixture_registry, valid_request,
    };
    use crate::prepare::{PreparedTxCache, propose};
    use crate::types::PaymentId;

    const UNKNOWN_CHAIN: ChainStatusView = ChainStatusView {
        visible_tip_height: None,
        settled_tip_height: None,
    };

    fn accepted_entry(
        transaction_id: &str,
        confirmation_count: Option<u32>,
        mined_block_height: Option<u64>,
    ) -> SettlementLedgerEntry {
        SettlementLedgerEntry {
            broadcast_outcome: BroadcastOutcome::Accepted {
                transaction_id: transaction_id.to_owned(),
            },
            settled_at_unix_seconds: 1_700_000_000,
            confirmation_count,
            mined_block_height,
            reorg_count: 0,
            last_reorged_at: None,
            expiry_height: None,
        }
    }

    fn rejected_entry(upstream_message: &str) -> SettlementLedgerEntry {
        SettlementLedgerEntry {
            broadcast_outcome: BroadcastOutcome::Rejected {
                upstream_message: upstream_message.to_owned(),
            },
            settled_at_unix_seconds: 1_700_000_001,
            confirmation_count: None,
            mined_block_height: None,
            reorg_count: 0,
            last_reorged_at: None,
            expiry_height: None,
        }
    }

    #[tokio::test]
    async fn unknown_payment_returns_never_issued_status() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let snapshot = lookup_payment_status(
            &PaymentId("does-not-exist".to_owned()),
            &store,
            &ledger,
            DEFAULT_FINALITY_DEPTH,
            UNKNOWN_CHAIN,
        )
        .await
        .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::NeverIssued);
        assert_eq!(snapshot.intent_posture, IntentPosture::Unverified);
        assert!(snapshot.broadcast_outcome.is_none());
        assert_eq!(snapshot.reorg_count, 0);
        assert!(!snapshot.settled);
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
            UNKNOWN_CHAIN,
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
            .record(payment_id.clone(), accepted_entry("deadbeef", None, None))
            .await
            .map_err(|_| "record failed")?;
        let snapshot = lookup_payment_status(
            &payment_id,
            &store,
            &ledger,
            DEFAULT_FINALITY_DEPTH,
            UNKNOWN_CHAIN,
        )
        .await
        .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Broadcast);
        assert!(!snapshot.settled);
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
                accepted_entry("abcd", Some(1), Some(2_000_000)),
            )
            .await
            .map_err(|_| "record failed")?;
        let snapshot = lookup_payment_status(
            &payment_id,
            &store,
            &ledger,
            DEFAULT_FINALITY_DEPTH,
            UNKNOWN_CHAIN,
        )
        .await
        .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Mined);
        assert_eq!(snapshot.confirmation_count, Some(1));
        assert_eq!(snapshot.mined_block_height, Some(2_000_000));
        assert!(!snapshot.settled);
        Ok(())
    }

    #[tokio::test]
    async fn ledger_entry_at_finality_depth_is_final_but_not_terminal() -> Result<(), &'static str>
    {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("final-id".to_owned());
        ledger
            .record(
                payment_id.clone(),
                accepted_entry("abcd", Some(3), Some(2_000_000)),
            )
            .await
            .map_err(|_| "record failed")?;
        let snapshot = lookup_payment_status(
            &payment_id,
            &store,
            &ledger,
            DEFAULT_FINALITY_DEPTH,
            UNKNOWN_CHAIN,
        )
        .await
        .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Final);
        assert!(!snapshot.status.is_terminal());
        assert!(!snapshot.stream_closed());
        Ok(())
    }

    #[tokio::test]
    async fn mined_row_is_settled_once_at_or_below_settled_tip() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("settled-id".to_owned());
        ledger
            .record(
                payment_id.clone(),
                accepted_entry("abcd", Some(120), Some(2_000_000)),
            )
            .await
            .map_err(|_| "record failed")?;
        let below = ChainStatusView {
            visible_tip_height: Some(2_000_050),
            settled_tip_height: Some(1_999_999),
        };
        let snapshot =
            lookup_payment_status(&payment_id, &store, &ledger, DEFAULT_FINALITY_DEPTH, below)
                .await
                .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Final);
        assert!(!snapshot.settled, "mined above settled tip is not settled");
        assert!(!snapshot.stream_closed());

        let past = ChainStatusView {
            visible_tip_height: Some(2_000_150),
            settled_tip_height: Some(2_000_050),
        };
        let snapshot =
            lookup_payment_status(&payment_id, &store, &ledger, DEFAULT_FINALITY_DEPTH, past)
                .await
                .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Final);
        assert!(snapshot.settled, "mined at or below settled tip is settled");
        assert!(
            snapshot.stream_closed(),
            "settled snapshot closes the stream"
        );
        Ok(())
    }

    #[tokio::test]
    async fn unmined_row_lapses_to_expired_once_tip_passes_expiry_height()
    -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("lapse-id".to_owned());
        let mut entry = accepted_entry("abcd", None, None);
        entry.expiry_height = Some(2_000_000);
        ledger
            .record(payment_id.clone(), entry)
            .await
            .map_err(|_| "record failed")?;

        let within = ChainStatusView {
            visible_tip_height: Some(2_000_000),
            settled_tip_height: Some(1_999_900),
        };
        let snapshot =
            lookup_payment_status(&payment_id, &store, &ledger, DEFAULT_FINALITY_DEPTH, within)
                .await
                .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Broadcast);
        assert!(!snapshot.stream_closed());

        let past = ChainStatusView {
            visible_tip_height: Some(2_000_001),
            settled_tip_height: Some(1_999_901),
        };
        let snapshot =
            lookup_payment_status(&payment_id, &store, &ledger, DEFAULT_FINALITY_DEPTH, past)
                .await
                .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Expired);
        assert!(
            snapshot.stream_closed(),
            "expired snapshot closes the stream"
        );
        Ok(())
    }

    #[tokio::test]
    async fn mined_downgrades_to_broadcast_on_reorg() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("reorg-mined".to_owned());
        ledger
            .record(
                payment_id.clone(),
                accepted_entry("abcd", Some(1), Some(2_000_000)),
            )
            .await
            .map_err(|_| "record failed")?;
        assert!(
            ledger
                .downgrade_on_reorg(&payment_id, 1_700_000_500)
                .await
                .map_err(|_| "downgrade failed")?
        );
        let snapshot = lookup_payment_status(
            &payment_id,
            &store,
            &ledger,
            DEFAULT_FINALITY_DEPTH,
            UNKNOWN_CHAIN,
        )
        .await
        .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Broadcast);
        assert_eq!(snapshot.confirmation_count, Some(0));
        assert_eq!(snapshot.mined_block_height, None);
        assert_eq!(snapshot.reorg_count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn final_downgrades_to_broadcast_on_reorg() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("reorg-final".to_owned());
        ledger
            .record(
                payment_id.clone(),
                accepted_entry("abcd", Some(6), Some(2_000_000)),
            )
            .await
            .map_err(|_| "record failed")?;
        assert!(
            ledger
                .downgrade_on_reorg(&payment_id, 1_700_000_500)
                .await
                .map_err(|_| "downgrade failed")?
        );
        let snapshot = lookup_payment_status(
            &payment_id,
            &store,
            &ledger,
            DEFAULT_FINALITY_DEPTH,
            UNKNOWN_CHAIN,
        )
        .await
        .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Broadcast);
        assert_eq!(snapshot.reorg_count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn downgraded_row_lapses_to_expired_when_tip_passes_expiry() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("reorg-then-lapse".to_owned());
        let mut entry = accepted_entry("abcd", Some(2), Some(2_000_000));
        entry.expiry_height = Some(2_000_010);
        ledger
            .record(payment_id.clone(), entry)
            .await
            .map_err(|_| "record failed")?;
        assert!(
            ledger
                .downgrade_on_reorg(&payment_id, 1_700_000_500)
                .await
                .map_err(|_| "downgrade failed")?
        );
        let past = ChainStatusView {
            visible_tip_height: Some(2_000_011),
            settled_tip_height: Some(1_999_900),
        };
        let snapshot =
            lookup_payment_status(&payment_id, &store, &ledger, DEFAULT_FINALITY_DEPTH, past)
                .await
                .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.status, PaymentStatus::Expired);
        assert_eq!(snapshot.reorg_count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn downgrade_on_reorg_is_a_noop_for_unmined_rows() -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("unmined".to_owned());
        ledger
            .record(payment_id.clone(), accepted_entry("abcd", None, None))
            .await
            .map_err(|_| "record failed")?;
        assert!(
            !ledger
                .downgrade_on_reorg(&payment_id, 1_700_000_500)
                .await
                .map_err(|_| "downgrade failed")?
        );
        let entry = ledger
            .find(&payment_id)
            .await
            .map_err(|_| "find failed")?
            .ok_or("row missing")?;
        assert_eq!(entry.reorg_count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn downgrade_reorged_range_selects_only_rows_in_range() -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        for (id, height) in [("low", 100), ("mid", 200), ("high", 300)] {
            ledger
                .record(
                    PaymentId(id.to_owned()),
                    accepted_entry(id, Some(1), Some(height)),
                )
                .await
                .map_err(|_| "record failed")?;
        }
        let downgraded = ledger
            .downgrade_reorged_range(150, 250, 1_700_000_600)
            .await
            .map_err(|_| "range downgrade failed")?;
        assert_eq!(downgraded, vec![PaymentId("mid".to_owned())]);

        let mid = ledger
            .find(&PaymentId("mid".to_owned()))
            .await
            .map_err(|_| "find mid failed")?
            .ok_or("mid missing")?;
        assert_eq!(mid.mined_block_height, None);
        assert_eq!(mid.reorg_count, 1);
        assert_eq!(mid.last_reorged_at, Some(1_700_000_600));

        for id in ["low", "high"] {
            let row = ledger
                .find(&PaymentId(id.to_owned()))
                .await
                .map_err(|_| "find failed")?
                .ok_or("row missing")?;
            assert_eq!(row.reorg_count, 0, "{id} outside range must be untouched");
            assert!(row.mined_block_height.is_some());
        }
        Ok(())
    }

    #[tokio::test]
    async fn ledger_entry_with_rejected_outcome_is_failed() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("failed-id".to_owned());
        ledger
            .record(payment_id.clone(), rejected_entry("policy: dust output"))
            .await
            .map_err(|_| "record failed")?;
        let snapshot = lookup_payment_status(
            &payment_id,
            &store,
            &ledger,
            DEFAULT_FINALITY_DEPTH,
            UNKNOWN_CHAIN,
        )
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
            .record(payment_id.clone(), rejected_entry("first attempt"))
            .await
            .map_err(|_| "first record failed")?;
        ledger
            .record(payment_id.clone(), accepted_entry("feedface", None, None))
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
            .record(payment_id.clone(), accepted_entry("feedface", None, None))
            .await
            .map_err(|_| "record failed")?;

        let store = PreparedTxCache::new();
        assert!(
            ledger
                .record_confirmation(&payment_id, 1, Some(1_234_567))
                .await
                .map_err(|_| "record_confirmation failed")?
        );
        let mined = lookup_payment_status(
            &payment_id,
            &store,
            &ledger,
            DEFAULT_FINALITY_DEPTH,
            UNKNOWN_CHAIN,
        )
        .await
        .map_err(|_| "lookup failed")?;
        assert_eq!(mined.status, PaymentStatus::Mined);

        assert!(
            ledger
                .record_confirmation(&payment_id, 3, Some(1_234_567))
                .await
                .map_err(|_| "record_confirmation failed")?
        );
        let finalized = lookup_payment_status(
            &payment_id,
            &store,
            &ledger,
            DEFAULT_FINALITY_DEPTH,
            UNKNOWN_CHAIN,
        )
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
    async fn success_kind_transactions_carries_mined_height_and_skips_failures()
    -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        ledger
            .record(
                PaymentId("ok".to_owned()),
                accepted_entry("abcd", Some(2), Some(2_000_000)),
            )
            .await
            .map_err(|_| "record failed")?;
        ledger
            .record(PaymentId("fail".to_owned()), rejected_entry("no"))
            .await
            .map_err(|_| "record failed")?;
        let rows = ledger
            .success_kind_transactions()
            .await
            .map_err(|_| "success_kind_transactions failed")?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].payment_id, PaymentId("ok".to_owned()));
        assert_eq!(rows[0].transaction_id, "abcd");
        assert_eq!(rows[0].mined_block_height, Some(2_000_000));
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
    fn is_terminal_excludes_final() {
        assert!(!PaymentStatus::Awaiting.is_terminal());
        assert!(!PaymentStatus::Broadcast.is_terminal());
        assert!(!PaymentStatus::Mined.is_terminal());
        assert!(!PaymentStatus::Final.is_terminal());
        assert!(PaymentStatus::Failed.is_terminal());
        assert!(PaymentStatus::NeverIssued.is_terminal());
        assert!(PaymentStatus::Expired.is_terminal());
    }
}
