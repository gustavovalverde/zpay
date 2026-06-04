//! Settle a prepared payment.
//!
//! Parses the user-signed transaction, gates it against the cached
//! preparation's well-formedness contract, broadcasts through the chain
//! plane, and mints a watch handle the confirmation oracle subscribes
//! to.
//!
//! # Trust boundary
//!
//! Settle is a relay with a well-formedness gate. It does not, and
//! cannot, verify the recipient address, the disclosed amount, or the
//! plaintext memo content of a shielded transaction: those fields live
//! inside the AEAD-encrypted output ciphertext keyed by the recipient's
//! incoming viewing key (ZIP-244 §T.3b/T.4b). Settle therefore checks
//! only the properties that are observable from the unencrypted v5 tx
//! header: the bytes parse as a Zcash transaction, and the parsed
//! `expiry_height` equals the `expiry_height` zpay returned at
//! `/prepare`.
//!
//! The cached preparation also carries the protocol memo prefix zpay
//! composed at prepare time. Settle re-reads that prefix and refuses to
//! relay any preparation whose version byte does not match the current
//! [`crate::prepare::PROTOCOL_MEMO_VERSION`]; the byte is observable on
//! the transparent path and is the only zpay-defined cleartext gate the
//! relay can apply without breaking the shielded-memo trust boundary.
//!
//! Cryptographic recipient/amount/memo binding is the job of the
//! [`crate::verify`] surface, which consumes a ZIP-311 payment
//! disclosure that only the sender can construct. See ADR-0006.
//!
//! The settle path is fire-once: a successful broadcast removes the
//! cached preparation so a second call returns
//! [`SettleError::PreparationNotFound`]. Failed broadcasts leave the
//! preparation in cache so the agent can retry the wallet step without
//! re-preparing.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::BranchId;

use crate::broadcast::{BroadcastClient, BroadcastError, BroadcastOutcome};
use crate::prepare::{PROTOCOL_MEMO_VERSION, PreparedTxStore};
use crate::status::{SettlementLedgerEntry, SettlementLedgerStore};
use crate::store::StoreError;
use crate::types::{PaymentId, WatchId};

/// Input to [`submit_settlement`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettleRequest {
    /// The `payment_id` returned from `prepare`.
    pub payment_id: PaymentId,
    /// Hex-encoded, user-signed, unbroadcast v5 Zcash transaction.
    pub raw_tx_hex: String,
}

/// Output of [`submit_settlement`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementOutcome {
    /// Payment identifier the caller settled.
    pub payment_id: PaymentId,
    /// Categorical outcome of the broadcast attempt.
    pub broadcast_outcome: BroadcastOutcome,
    /// Watch handle the agent passes to the confirmation oracle. Present
    /// only on success-kind outcomes (`Accepted`, `Duplicate`).
    pub watch_id: Option<WatchId>,
}

/// Errors that arise during settlement.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SettleError {
    /// Caller's `payment_id` does not exist in the prepared-tx cache.
    /// Retry posture: `not_retryable`. Either the payment was already
    /// settled or the preparation expired.
    #[error("preparation not found: {payment_id}")]
    PreparationNotFound {
        /// The unknown `payment_id`.
        payment_id: PaymentId,
    },
    /// `raw_tx_hex` is empty or violates the hex alphabet. Retry posture:
    /// `not_retryable`. Wire adapters should catch most of this at the
    /// codec layer; this is a defensive backstop.
    #[error("raw_tx_hex must be non-empty and contain only hex characters")]
    RawTxHexInvalid,
    /// The chain plane could not be reached. Retry posture: `retryable`.
    /// Cache entry is preserved so the agent can retry.
    #[error("chain plane unavailable: {reason}")]
    ChainUnavailable {
        /// Operator-facing reason; the original `BroadcastError` lives in
        /// the error source chain.
        reason: String,
    },
    /// `raw_tx_hex` is hex-shaped but not a valid Zcash transaction.
    /// Retry posture: `not_retryable`. The wallet must rebuild the tx.
    /// The cache entry is preserved so the agent can submit a corrected
    /// payload.
    #[error("raw_tx_hex did not parse as a Zcash transaction: {reason}")]
    TransactionMalformed {
        /// Operator-facing parse error.
        reason: String,
    },
    /// The parsed transaction's `expiry_height` does not equal the
    /// `expiry_height` zpay returned from `/prepare`. Indicates the
    /// wallet built a transaction for a different prepared row than the
    /// one named by `payment_id`. Retry posture: `not_retryable`.
    #[error(
        "expiry_height mismatch: prepared={prepared_expiry_height}, signed={signed_expiry_height}"
    )]
    ExpiryHeightMismatch {
        /// `expiry_height` zpay returned from `/prepare`.
        prepared_expiry_height: u32,
        /// `expiry_height` observed in the user-signed transaction.
        signed_expiry_height: u32,
    },
    /// The cached preparation's protocol memo version does not match the
    /// version this build of zpay knows how to broadcast. The agent must
    /// re-prepare against a runtime whose
    /// [`crate::prepare::PROTOCOL_MEMO_VERSION`] matches. Retry posture:
    /// `not_retryable`.
    #[error(
        "protocol memo version mismatch: expected {expected:#04x}, observed {observed:#04x}",
        expected = PROTOCOL_MEMO_VERSION,
    )]
    ObsoleteMemoVersion {
        /// Version byte observed in the cached preparation's memo prefix.
        observed: u8,
    },
    /// The DPoP `jkt` presented on settle does not match the jkt that
    /// prepared the row. Retry posture: `not_retryable`; a different
    /// agent attempted to settle a preparation it did not own.
    #[error("DPoP key mismatch: prepared by a different agent")]
    DpopMismatch,
    /// One of the underlying stores (prepared-tx cache or settlement
    /// ledger) surfaced a [`StoreError`]. Retry posture: inherits.
    #[error("settle store failure: {0}")]
    Storage(#[from] StoreError),
}

/// Submit a prepared payment to the chain plane.
///
/// On a success-kind outcome (`Accepted` or `Duplicate`) the preparation
/// is removed from the cache to enforce fire-once semantics. On failure
/// outcomes (`InvalidEncoding`, `Rejected`, `Unknown`) the preparation
/// stays so the agent can retry with a corrected wallet payload.
///
/// # Errors
///
/// - [`SettleError::PreparationNotFound`] when the cached preparation has
///   already been removed (a successful settle, an expiry sweep, or a
///   bogus `payment_id`).
/// - [`SettleError::RawTxHexInvalid`] when the supplied hex string fails
///   the basic alphabet check.
/// - [`SettleError::ChainUnavailable`] when [`BroadcastClient::broadcast`]
///   itself errors. The cache entry is preserved on this path.
pub async fn submit_settlement<C, P, L>(
    request: SettleRequest,
    jkt: &str,
    prepared_store: &P,
    ledger: &L,
    chain: &C,
) -> Result<SettlementOutcome, SettleError>
where
    C: BroadcastClient + ?Sized,
    P: PreparedTxStore + ?Sized,
    L: SettlementLedgerStore + ?Sized,
{
    validate_raw_tx_hex(&request.raw_tx_hex)?;

    let Some(prepared) = prepared_store.find_by_payment_id(&request.payment_id).await? else {
        return Err(SettleError::PreparationNotFound {
            payment_id: request.payment_id,
        });
    };

    // The DPoP key that signed /settle must equal the one that prepared
    // the row. A rival agent with a fresh keypair cannot settle another
    // agent's prepared payment even if it learned the payment_id; the
    // wire layer surfaces this as 403 dpop_mismatch.
    if prepared.agent_dpop_jkt != jkt {
        return Err(SettleError::DpopMismatch);
    }

    if let Some(observed) = prepared.preparation.memo_bytes.get(1).copied()
        && observed != PROTOCOL_MEMO_VERSION
    {
        return Err(SettleError::ObsoleteMemoVersion { observed });
    }

    let signed_expiry_height = parse_signed_expiry_height(&request.raw_tx_hex)?;
    if signed_expiry_height != prepared.preparation.expiry_height {
        return Err(SettleError::ExpiryHeightMismatch {
            prepared_expiry_height: prepared.preparation.expiry_height,
            signed_expiry_height,
        });
    }

    let outcome = chain
        .broadcast(&request.raw_tx_hex)
        .await
        .map_err(|err| match err {
            BroadcastError::Unavailable { reason }
            | BroadcastError::ResponseMalformed { reason } => {
                SettleError::ChainUnavailable { reason }
            }
        })?;

    ledger
        .record(
            request.payment_id.clone(),
            SettlementLedgerEntry {
                broadcast_outcome: outcome.clone(),
                settled_at_unix_seconds: current_unix_seconds(),
                confirmation_count: None,
                mined_block_height: None,
            },
        )
        .await?;

    let watch_id = if outcome.is_success() {
        prepared_store.remove(&request.payment_id).await?;
        Some(WatchId(format!("watch_{}", request.payment_id)))
    } else {
        None
    };

    Ok(SettlementOutcome {
        payment_id: request.payment_id,
        broadcast_outcome: outcome,
        watch_id,
    })
}

fn current_unix_seconds() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(now.as_secs()).unwrap_or(i64::MAX)
}

fn validate_raw_tx_hex(raw_tx_hex: &str) -> Result<(), SettleError> {
    if raw_tx_hex.is_empty()
        || !raw_tx_hex.len().is_multiple_of(2)
        || !raw_tx_hex.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(SettleError::RawTxHexInvalid);
    }
    Ok(())
}

/// Parse `raw_tx_hex` as a v5 Zcash transaction and return its
/// `expiry_height`.
///
/// `Transaction::read` requires a `BranchId` but uses it only for v3/v4
/// txid computation; v5 carries the consensus branch id in its own
/// header. zpay is post-NU5 only, so we pass [`BranchId::Nu5`] as a
/// placeholder and trust the v5 path.
fn parse_signed_expiry_height(raw_tx_hex: &str) -> Result<u32, SettleError> {
    let raw_bytes = hex::decode(raw_tx_hex).map_err(|_| SettleError::RawTxHexInvalid)?;
    let tx = Transaction::read(raw_bytes.as_slice(), BranchId::Nu5).map_err(|err| {
        SettleError::TransactionMalformed {
            reason: err.to_string(),
        }
    })?;
    Ok(u32::from(tx.expiry_height()))
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::{SettleError, SettleRequest, submit_settlement};
    use crate::broadcast::{BroadcastClient, BroadcastError, BroadcastOutcome};
    use crate::prepare::test_support::{
        ALTERNATE_FIXTURE_JKT, FIXTURE_JKT, FixedTipOracle, fixture_registry, valid_request,
    };
    use crate::prepare::{PreparedTxCache, PreparedTxStore, propose};
    use crate::status::SettlementLedger;

    struct FakeChain {
        outcome: Mutex<BroadcastOutcome>,
    }

    impl FakeChain {
        fn new(outcome: BroadcastOutcome) -> Self {
            Self {
                outcome: Mutex::new(outcome),
            }
        }
    }

    impl BroadcastClient for FakeChain {
        async fn broadcast(&self, _raw_tx_hex: &str) -> Result<BroadcastOutcome, BroadcastError> {
            Ok(self.outcome.lock().clone())
        }
    }

    struct UnavailableChain;

    impl BroadcastClient for UnavailableChain {
        async fn broadcast(&self, _raw_tx_hex: &str) -> Result<BroadcastOutcome, BroadcastError> {
            Err(BroadcastError::Unavailable {
                reason: "dial timeout".to_owned(),
            })
        }
    }

    #[tokio::test]
    async fn accepted_broadcast_returns_watch_id_and_removes_preparation()
    -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let registry = fixture_registry();
        let tip = FixedTipOracle::fixture();
        let preparation = propose(valid_request(), FIXTURE_JKT.to_owned(), &cache, &registry, &tip)
            .await
            .map_err(|_| "propose must accept valid input")?;
        let chain = FakeChain::new(BroadcastOutcome::Accepted {
            transaction_id: "deadbeef".to_owned(),
        });

        let outcome = submit_settlement(
SettleRequest {
                payment_id: preparation.payment_id.clone(),
                raw_tx_hex: minimal_v5_tx_hex(preparation.expiry_height),
            },
            FIXTURE_JKT,
            &cache,
            &ledger,
            &chain,
        )
        .await
        .map_err(|_| "settle must accept the prepared payment")?;

        assert_eq!(outcome.payment_id, preparation.payment_id);
        assert_eq!(outcome.broadcast_outcome.transaction_id(), Some("deadbeef"));
        assert!(outcome.watch_id.is_some());
        assert_eq!(
            cache.entry_count().await.map_err(|_| "entry_count failed")?,
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn rejected_broadcast_keeps_preparation_for_retry() -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let registry = fixture_registry();
        let tip = FixedTipOracle::fixture();
        let preparation = propose(valid_request(), FIXTURE_JKT.to_owned(), &cache, &registry, &tip)
            .await
            .map_err(|_| "propose must accept valid input")?;
        let chain = FakeChain::new(BroadcastOutcome::Rejected {
            upstream_message: "policy: dust output".to_owned(),
        });

        let outcome = submit_settlement(
SettleRequest {
                payment_id: preparation.payment_id.clone(),
                raw_tx_hex: minimal_v5_tx_hex(preparation.expiry_height),
            },
            FIXTURE_JKT,
            &cache,
            &ledger,
            &chain,
        )
        .await
        .map_err(|_| "settle must return Ok with a Rejected variant")?;

        assert!(matches!(
            outcome.broadcast_outcome,
            BroadcastOutcome::Rejected { .. }
        ));
        assert!(outcome.watch_id.is_none());
        assert_eq!(
            cache.entry_count().await.map_err(|_| "entry_count failed")?,
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn unknown_payment_id_returns_preparation_not_found() {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let chain = FakeChain::new(BroadcastOutcome::Accepted {
            transaction_id: "deadbeef".to_owned(),
        });
        let request = SettleRequest {
            payment_id: crate::types::PaymentId("unknown".to_owned()),
            raw_tx_hex: "0500".to_owned(),
        };

        let outcome = submit_settlement(request, FIXTURE_JKT, &cache, &ledger, &chain).await;
        assert!(matches!(
            outcome,
            Err(SettleError::PreparationNotFound { .. })
        ));
    }

    #[tokio::test]
    async fn empty_raw_tx_hex_returns_raw_tx_hex_invalid() -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let registry = fixture_registry();
        let tip = FixedTipOracle::fixture();
        let preparation = propose(valid_request(), FIXTURE_JKT.to_owned(), &cache, &registry, &tip)
            .await
            .map_err(|_| "propose must accept valid input")?;
        let chain = FakeChain::new(BroadcastOutcome::Accepted {
            transaction_id: "deadbeef".to_owned(),
        });

        let outcome = submit_settlement(
SettleRequest {
                payment_id: preparation.payment_id,
                raw_tx_hex: String::new(),
            },
            FIXTURE_JKT,
            &cache,
            &ledger,
            &chain,
        )
        .await;

        assert!(matches!(outcome, Err(SettleError::RawTxHexInvalid)));
        Ok(())
    }

    #[tokio::test]
    async fn odd_length_raw_tx_hex_returns_raw_tx_hex_invalid() -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let registry = fixture_registry();
        let tip = FixedTipOracle::fixture();
        let preparation = propose(valid_request(), FIXTURE_JKT.to_owned(), &cache, &registry, &tip)
            .await
            .map_err(|_| "propose must accept valid input")?;
        let chain = FakeChain::new(BroadcastOutcome::Accepted {
            transaction_id: "deadbeef".to_owned(),
        });

        let outcome = submit_settlement(
SettleRequest {
                payment_id: preparation.payment_id,
                raw_tx_hex: "abc".to_owned(),
            },
            FIXTURE_JKT,
            &cache,
            &ledger,
            &chain,
        )
        .await;

        assert!(matches!(outcome, Err(SettleError::RawTxHexInvalid)));
        Ok(())
    }

    #[tokio::test]
    async fn chain_unavailable_returns_chain_unavailable_error() -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let registry = fixture_registry();
        let tip = FixedTipOracle::fixture();
        let preparation = propose(valid_request(), FIXTURE_JKT.to_owned(), &cache, &registry, &tip)
            .await
            .map_err(|_| "propose must accept valid input")?;
        let chain = UnavailableChain;

        let outcome = submit_settlement(
SettleRequest {
                payment_id: preparation.payment_id,
                raw_tx_hex: minimal_v5_tx_hex(preparation.expiry_height),
            },
            FIXTURE_JKT,
            &cache,
            &ledger,
            &chain,
        )
        .await;

        assert!(matches!(outcome, Err(SettleError::ChainUnavailable { .. })));
        assert_eq!(
            cache.entry_count().await.map_err(|_| "entry_count failed")?,
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn signed_tx_with_wrong_expiry_returns_expiry_mismatch() -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let registry = fixture_registry();
        let tip = FixedTipOracle::fixture();
        let preparation = propose(valid_request(), FIXTURE_JKT.to_owned(), &cache, &registry, &tip)
            .await
            .map_err(|_| "propose must accept valid input")?;
        let chain = FakeChain::new(BroadcastOutcome::Accepted {
            transaction_id: "deadbeef".to_owned(),
        });

        // Wallet signed a tx whose expiry_height is not the one zpay
        // returned at /prepare. Settle must reject before broadcasting.
        let wrong_expiry = preparation.expiry_height.wrapping_add(1);
        let outcome = submit_settlement(
SettleRequest {
                payment_id: preparation.payment_id.clone(),
                raw_tx_hex: minimal_v5_tx_hex(wrong_expiry),
            },
            FIXTURE_JKT,
            &cache,
            &ledger,
            &chain,
        )
        .await;

        assert!(matches!(
            outcome,
            Err(SettleError::ExpiryHeightMismatch { .. })
        ));
        assert_eq!(
            cache.entry_count().await.map_err(|_| "entry_count failed")?,
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn raw_tx_hex_that_is_not_a_zcash_tx_returns_malformed() -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let registry = fixture_registry();
        let tip = FixedTipOracle::fixture();
        let preparation = propose(valid_request(), FIXTURE_JKT.to_owned(), &cache, &registry, &tip)
            .await
            .map_err(|_| "propose must accept valid input")?;
        let chain = FakeChain::new(BroadcastOutcome::Accepted {
            transaction_id: "deadbeef".to_owned(),
        });

        // Hex-shaped but garbage bytes.
        let outcome = submit_settlement(
SettleRequest {
                payment_id: preparation.payment_id.clone(),
                raw_tx_hex: "deadbeefdeadbeefdeadbeefdeadbeef".to_owned(),
            },
            FIXTURE_JKT,
            &cache,
            &ledger,
            &chain,
        )
        .await;

        assert!(matches!(
            outcome,
            Err(SettleError::TransactionMalformed { .. })
        ));
        assert_eq!(
            cache.entry_count().await.map_err(|_| "entry_count failed")?,
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn obsolete_memo_version_is_rejected_before_broadcast() -> Result<(), &'static str> {
        use crate::prepare::{
            PROTOCOL_MEMO_TAG, Preparation, PreparedTxEntry,
        };
        use crate::types::{PayeeId, PaymentId, PaymentNetwork, Zatoshis};

        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        // Stuff a preparation whose memo version is the retired 0x01
        // straight into the cache so we can exercise the gate without
        // forging a stale runtime.
        let stale_memo = {
            let mut bytes = vec![PROTOCOL_MEMO_TAG, 0x01];
            bytes.extend_from_slice(&[0u8; 64]);
            bytes
        };
        let entry = PreparedTxEntry {
            preparation: Preparation {
                payment_id: PaymentId("obsolete-memo".to_owned()),
                payment_uri: "zcash:utest1stub?amount=0.0005".to_owned(),
                memo_bytes: stale_memo,
                expiry_height: 1_234,
                amount_zat: Zatoshis(50_000),
            },
            payee_id: PayeeId("aether-ai".to_owned()),
            network: PaymentNetwork::Testnet,
            recipient_unified_address: "utest1stub".to_owned(),
            amount_zat: Zatoshis(50_000),
            expires_at_unix_seconds: u64::MAX,
            idempotency_key: None,
            agent_dpop_jkt: FIXTURE_JKT.to_owned(),
        };
        cache.insert(entry).await.map_err(|_| "insert failed")?;
        let chain = FakeChain::new(BroadcastOutcome::Accepted {
            transaction_id: "deadbeef".to_owned(),
        });

        let outcome = submit_settlement(
SettleRequest {
                payment_id: PaymentId("obsolete-memo".to_owned()),
                raw_tx_hex: minimal_v5_tx_hex(1_234),
            },
            FIXTURE_JKT,
            &cache,
            &ledger,
            &chain,
        )
        .await;

        assert!(matches!(
            outcome,
            Err(SettleError::ObsoleteMemoVersion { observed: 0x01 })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn settle_rejects_jkt_mismatch_before_broadcast() -> Result<(), &'static str> {
        // A different agent attempting to settle a prepared row gets
        // SettleError::DpopMismatch. The wire layer surfaces this as
        // 403 application/problem+json with code=dpop_mismatch.
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let registry = fixture_registry();
        let tip = FixedTipOracle::fixture();
        let preparation = propose(valid_request(), FIXTURE_JKT.to_owned(), &cache, &registry, &tip)
            .await
            .map_err(|_| "propose must accept valid input")?;
        let chain = FakeChain::new(BroadcastOutcome::Accepted {
            transaction_id: "deadbeef".to_owned(),
        });

        let outcome = submit_settlement(
            SettleRequest {
                payment_id: preparation.payment_id.clone(),
                raw_tx_hex: minimal_v5_tx_hex(preparation.expiry_height),
            },
            ALTERNATE_FIXTURE_JKT,
            &cache,
            &ledger,
            &chain,
        )
        .await;

        assert!(matches!(outcome, Err(SettleError::DpopMismatch)));
        assert_eq!(
            cache.entry_count().await.map_err(|_| "entry_count failed")?,
            1
        );
        Ok(())
    }

    /// Build the smallest v5 transaction the `zcash_primitives` reader
    /// will accept: V5 header, V5 version group id, Nu5 consensus
    /// branch id, supplied expiry, zero transparent/sapling/orchard
    /// items.
    fn minimal_v5_tx_hex(expiry_height: u32) -> String {
        use std::fmt::Write as _;
        let mut bytes = Vec::with_capacity(25);
        bytes.extend_from_slice(&0x8000_0005u32.to_le_bytes()); // version + overwintered
        bytes.extend_from_slice(&0x26A7_270Au32.to_le_bytes()); // V5_VERSION_GROUP_ID
        bytes.extend_from_slice(&0xC2D6_D0B4u32.to_le_bytes()); // BranchId::Nu5
        bytes.extend_from_slice(&0u32.to_le_bytes()); // lock_time
        bytes.extend_from_slice(&expiry_height.to_le_bytes());
        bytes.push(0x00); // transparent tx_in count
        bytes.push(0x00); // transparent tx_out count
        bytes.push(0x00); // sapling spends count
        bytes.push(0x00); // sapling outputs count
        bytes.push(0x00); // orchard actions count
        let mut hex_out = String::with_capacity(bytes.len() * 2);
        for byte in &bytes {
            let _ = write!(&mut hex_out, "{byte:02x}");
        }
        hex_out
    }
}
