//! Single-use access-token `jti` ledger (Proposal-0003 D-8).
//!
//! The wallet must sign a given access-token `jti` at most once. The guarantee
//! is reserve-BEFORE-sign: the runtime claims the `jti` (recording the intent
//! hash it is about to commit to) before it asks the wallet to build and sign
//! the transaction, then commits the signed payload once the wallet returns.
//! A crash between reserve and commit leaves a `Pending` reservation that a
//! later identical retry treats as retryable rather than re-signing blind.
//!
//! [`reserve`](LedgerStore::reserve) returns one of four outcomes:
//!
//! - [`Reservation::Fresh`]: the `jti` was unseen; the caller proceeds to sign.
//! - [`Reservation::Completed`]: an identical replay (same `jti`, same intent
//!   hash, already committed); the caller returns the cached payload without
//!   re-signing.
//! - [`Reservation::IntentConflict`]: the `jti` was already reserved or
//!   committed against a DIFFERENT intent hash; the caller rejects with
//!   [`zspend_core::ProblemKind::TokenAlreadyConsumed`].
//! - [`Reservation::Pending`]: the `jti` is reserved against the SAME intent
//!   hash but not yet committed (a prior attempt is in flight or crashed
//!   mid-sign); the caller returns a retryable 503.
//!
//! The default [`InProcessLedger`] satisfies the contract for a single
//! instance via an in-memory map. A shared backend (gated behind
//! `ZSPEND_LEDGER_URL`) would let several wallet replicas enforce single-use
//! across the fleet; until that lands the runtime runs single-instance.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::SignPaymentResponse;

/// How long a `Pending` reservation may sit uncommitted before reclaim.
///
/// After this window a later reserve treats the reservation as abandoned.
/// Sized to comfortably exceed the wallet's propose-prove-sign latency so a
/// slow-but-live sign is never stolen out from under an in-flight request.
pub(crate) const PENDING_RESERVATION_TTL: Duration = Duration::from_mins(2);

/// Outcome of reserving an access-token `jti` before signing.
#[derive(Debug)]
pub(crate) enum Reservation {
    /// The `jti` was unseen and is now reserved against `intent_hash`. The
    /// caller proceeds to sign and then [`commit`](LedgerStore::commit)s.
    Fresh,
    /// An identical replay: the same `jti` already committed the same intent.
    /// The caller returns the cached payload without re-signing.
    Completed(SignPaymentResponse),
    /// The `jti` is bound to a DIFFERENT intent hash; the caller rejects.
    IntentConflict,
    /// The `jti` is reserved against the same intent but not yet committed.
    /// A prior attempt is in flight or crashed mid-sign; retryable.
    Pending,
}

/// Single-use `jti` store: reserve before signing, commit after.
pub(crate) trait LedgerStore: Send + Sync {
    /// Reserve `jti` against `intent_hash` before signing.
    fn reserve(&self, jti: &str, intent_hash: &str) -> Reservation;

    /// Commit the signed `response` for a `jti` reserved against `intent_hash`,
    /// making a later identical replay return [`Reservation::Completed`]. The
    /// caller passes the same `intent_hash` it reserved with, so the committed
    /// record carries the verified intent rather than re-reading a prior entry.
    fn commit(&self, jti: &str, intent_hash: &str, response: SignPaymentResponse);

    /// Release a reservation (e.g. when signing failed) so a retry with the
    /// same `jti` and intent sees [`Reservation::Fresh`] rather than waiting
    /// out the pending TTL.
    fn release(&self, jti: &str);
}

/// One `jti`'s state in the in-process ledger.
enum Entry {
    /// Reserved at `since`, not yet committed.
    Pending { intent_hash: String, since: Instant },
    /// Committed: the signed payload an identical replay returns.
    Committed {
        intent_hash: String,
        response: Box<SignPaymentResponse>,
    },
}

/// In-memory [`LedgerStore`] for a single wallet instance.
#[derive(Clone)]
pub(crate) struct InProcessLedger {
    entries: Arc<Mutex<HashMap<String, Entry>>>,
}

impl InProcessLedger {
    /// Construct an empty in-process ledger.
    pub(crate) fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl LedgerStore for InProcessLedger {
    fn reserve(&self, jti: &str, intent_hash: &str) -> Reservation {
        let mut entries = self.entries.lock();
        match entries.get(jti) {
            Some(Entry::Committed {
                intent_hash: committed_intent,
                response,
            }) => {
                if committed_intent == intent_hash {
                    Reservation::Completed((**response).clone())
                } else {
                    Reservation::IntentConflict
                }
            }
            Some(Entry::Pending {
                intent_hash: pending_intent,
                since,
            }) => {
                if pending_intent != intent_hash {
                    Reservation::IntentConflict
                } else if since.elapsed() >= PENDING_RESERVATION_TTL {
                    // The prior attempt abandoned the reservation; reclaim it.
                    entries.insert(
                        jti.to_owned(),
                        Entry::Pending {
                            intent_hash: intent_hash.to_owned(),
                            since: Instant::now(),
                        },
                    );
                    Reservation::Fresh
                } else {
                    Reservation::Pending
                }
            }
            None => {
                entries.insert(
                    jti.to_owned(),
                    Entry::Pending {
                        intent_hash: intent_hash.to_owned(),
                        since: Instant::now(),
                    },
                );
                Reservation::Fresh
            }
        }
    }

    fn commit(&self, jti: &str, intent_hash: &str, response: SignPaymentResponse) {
        let mut entries = self.entries.lock();
        entries.insert(
            jti.to_owned(),
            Entry::Committed {
                intent_hash: intent_hash.to_owned(),
                response: Box::new(response),
            },
        );
    }

    fn release(&self, jti: &str) {
        // Only drop a still-pending reservation: a committed entry is the
        // single-use record and must survive a late release call.
        let mut entries = self.entries.lock();
        if matches!(entries.get(jti), Some(Entry::Pending { .. })) {
            entries.remove(jti);
        }
    }
}

// TODO(ZSPEND_LEDGER_URL): add a libsql-backed `LedgerStore` so several wallet
// replicas enforce single-use `jti` across the fleet. The trait is the seam:
// `reserve` becomes an atomic upsert (INSERT ... ON CONFLICT returning the
// stored intent_hash + committed payload) and `commit`/`release` become row
// updates. Select it in `serve()` when `ResolvedConfig::ledger_url` is set;
// the in-process default stands in for single-instance deploys until then.

#[cfg(test)]
mod tests {
    use super::{InProcessLedger, LedgerStore, Reservation};
    use crate::{AmountWire, ExpiresAtWire, SignPaymentResponse, SignedPayloadWire};

    const JTI: &str = "01ACCESSTOKENJTI0000000000";
    const INTENT: &str = "v1:sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const OTHER_INTENT: &str = "v1:sha256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    fn fixture_response() -> SignPaymentResponse {
        SignPaymentResponse {
            signed_payload: SignedPayloadWire {
                format: "raw-zcash-v5",
                bytes: "AAAA".to_owned(),
                tx_id: "deadbeef".to_owned(),
                fee: AmountWire {
                    currency: "ZEC",
                    value: "0".to_owned(),
                    unit: "base",
                },
                expires_at: ExpiresAtWire::BlockHeight(4_047_100),
                metadata: serde_json::Value::Null,
            },
        }
    }

    #[test]
    fn first_reserve_is_fresh() {
        let ledger = InProcessLedger::new();
        assert!(matches!(ledger.reserve(JTI, INTENT), Reservation::Fresh));
    }

    #[test]
    fn reserve_before_commit_is_pending_for_same_intent() {
        let ledger = InProcessLedger::new();
        assert!(matches!(ledger.reserve(JTI, INTENT), Reservation::Fresh));
        assert!(matches!(ledger.reserve(JTI, INTENT), Reservation::Pending));
    }

    #[test]
    fn reserve_with_different_intent_conflicts_before_commit() {
        let ledger = InProcessLedger::new();
        assert!(matches!(ledger.reserve(JTI, INTENT), Reservation::Fresh));
        assert!(matches!(
            ledger.reserve(JTI, OTHER_INTENT),
            Reservation::IntentConflict
        ));
    }

    #[test]
    fn identical_replay_after_commit_returns_cached_payload() {
        let ledger = InProcessLedger::new();
        assert!(matches!(ledger.reserve(JTI, INTENT), Reservation::Fresh));
        ledger.commit(JTI, INTENT, fixture_response());
        let cached_tx_id = match ledger.reserve(JTI, INTENT) {
            Reservation::Completed(cached) => cached.signed_payload.tx_id,
            Reservation::Fresh | Reservation::IntentConflict | Reservation::Pending => {
                String::new()
            }
        };
        assert_eq!(
            cached_tx_id, "deadbeef",
            "an identical replay must return the cached signed payload",
        );
    }

    #[test]
    fn different_intent_after_commit_conflicts() {
        let ledger = InProcessLedger::new();
        assert!(matches!(ledger.reserve(JTI, INTENT), Reservation::Fresh));
        ledger.commit(JTI, INTENT, fixture_response());
        assert!(matches!(
            ledger.reserve(JTI, OTHER_INTENT),
            Reservation::IntentConflict
        ));
    }

    #[test]
    fn release_lets_a_retry_reserve_fresh() {
        let ledger = InProcessLedger::new();
        assert!(matches!(ledger.reserve(JTI, INTENT), Reservation::Fresh));
        ledger.release(JTI);
        assert!(matches!(ledger.reserve(JTI, INTENT), Reservation::Fresh));
    }

    #[test]
    fn release_does_not_drop_a_committed_entry() {
        let ledger = InProcessLedger::new();
        assert!(matches!(ledger.reserve(JTI, INTENT), Reservation::Fresh));
        ledger.commit(JTI, INTENT, fixture_response());
        ledger.release(JTI);
        assert!(matches!(
            ledger.reserve(JTI, INTENT),
            Reservation::Completed(_)
        ));
    }
}
