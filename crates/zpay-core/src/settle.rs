//! Settle a prepared payment: validate the user-signed transaction
//! against the cached preparation, broadcast through the chain plane, and
//! mint a watch handle the confirmation oracle subscribes to.
//!
//! The settle path is fire-once: a successful broadcast removes the
//! cached preparation so a second call returns
//! [`SettleError::PreparationNotFound`]. Failed broadcasts leave the
//! preparation in cache so the agent can retry the wallet step without
//! re-preparing.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::broadcast::{BroadcastClient, BroadcastError, BroadcastOutcome};
use crate::prepare::PreparedTxCache;
use crate::status::{SettlementLedger, SettlementLedgerEntry};
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
pub async fn submit_settlement<C: BroadcastClient>(
    request: SettleRequest,
    cache: &PreparedTxCache,
    ledger: &SettlementLedger,
    chain: &C,
) -> Result<SettlementOutcome, SettleError> {
    validate_raw_tx_hex(&request.raw_tx_hex)?;

    if cache.find(&request.payment_id).is_none() {
        return Err(SettleError::PreparationNotFound {
            payment_id: request.payment_id,
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

    ledger.record(
        request.payment_id.clone(),
        SettlementLedgerEntry {
            broadcast_outcome: outcome.clone(),
            settled_at_unix_seconds: current_unix_seconds(),
        },
    );

    let watch_id = if outcome.is_success_kind() {
        cache.remove(&request.payment_id);
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

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::{SettleError, SettleRequest, submit_settlement};
    use crate::broadcast::{BroadcastClient, BroadcastError, BroadcastOutcome};
    use crate::prepare::{ChallengeHash, PrepareRequest, PreparedTxCache, ResourceHash, propose};
    use crate::status::SettlementLedger;
    use crate::types::{EvidencePackHash, MerchantId, PaymentNetwork, PaymentScheme, Zatoshis};

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
    async fn accepted_broadcast_returns_watch_id_and_removes_preparation()
    -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let preparation = propose(valid_prepare_request(), &cache)
            .map_err(|_| "propose must accept valid input")?;
        let chain = FakeChain::new(BroadcastOutcome::Accepted {
            transaction_id: "deadbeef".to_owned(),
        });

        let outcome = submit_settlement(
            SettleRequest {
                payment_id: preparation.payment_id.clone(),
                raw_tx_hex: "0500000080".to_owned(),
            },
            &cache,
            &ledger,
            &chain,
        )
        .await
        .map_err(|_| "settle must accept the prepared payment")?;

        assert_eq!(outcome.payment_id, preparation.payment_id);
        assert_eq!(outcome.broadcast_outcome.transaction_id(), Some("deadbeef"));
        assert!(outcome.watch_id.is_some());
        assert_eq!(cache.entry_count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn rejected_broadcast_keeps_preparation_for_retry() -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let preparation = propose(valid_prepare_request(), &cache)
            .map_err(|_| "propose must accept valid input")?;
        let chain = FakeChain::new(BroadcastOutcome::Rejected {
            upstream_message: "policy: dust output".to_owned(),
        });

        let outcome = submit_settlement(
            SettleRequest {
                payment_id: preparation.payment_id.clone(),
                raw_tx_hex: "0500000080".to_owned(),
            },
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
        assert_eq!(cache.entry_count(), 1);
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
            raw_tx_hex: "0500000080".to_owned(),
        };

        let outcome = submit_settlement(request, &cache, &ledger, &chain).await;
        assert!(matches!(
            outcome,
            Err(SettleError::PreparationNotFound { .. })
        ));
    }

    #[tokio::test]
    async fn empty_raw_tx_hex_returns_raw_tx_hex_invalid() -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let ledger = SettlementLedger::new();
        let preparation = propose(valid_prepare_request(), &cache)
            .map_err(|_| "propose must accept valid input")?;
        let chain = FakeChain::new(BroadcastOutcome::Accepted {
            transaction_id: "deadbeef".to_owned(),
        });

        let outcome = submit_settlement(
            SettleRequest {
                payment_id: preparation.payment_id,
                raw_tx_hex: String::new(),
            },
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
        let preparation = propose(valid_prepare_request(), &cache)
            .map_err(|_| "propose must accept valid input")?;
        let chain = FakeChain::new(BroadcastOutcome::Accepted {
            transaction_id: "deadbeef".to_owned(),
        });

        let outcome = submit_settlement(
            SettleRequest {
                payment_id: preparation.payment_id,
                raw_tx_hex: "abc".to_owned(),
            },
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
        let preparation = propose(valid_prepare_request(), &cache)
            .map_err(|_| "propose must accept valid input")?;
        let chain = UnavailableChain;

        let outcome = submit_settlement(
            SettleRequest {
                payment_id: preparation.payment_id,
                raw_tx_hex: "0500000080".to_owned(),
            },
            &cache,
            &ledger,
            &chain,
        )
        .await;

        assert!(matches!(outcome, Err(SettleError::ChainUnavailable { .. })));
        assert_eq!(cache.entry_count(), 1);
        Ok(())
    }
}
