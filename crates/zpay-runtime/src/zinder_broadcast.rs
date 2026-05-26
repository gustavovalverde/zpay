//! Production [`BroadcastClient`] backed by `zinder-client::RemoteChainIndex`.
//!
//! Constructed on startup when `ZPAY_NODE__INDEXER_GRPC_ADDR` is set in the
//! environment. The endpoint is treated as a `WalletQuery` gRPC URI; the
//! underlying channel handles HTTP/2 keepalive, lazy reconnect, and the
//! tonic 0.14 channel-self-heal pattern transparently.
//!
//! Failures map onto the typed [`BroadcastError`] vocabulary so the settle
//! handler returns the right RFC 7807 problem document without exposing
//! internal gRPC status codes.

use zinder_client::{
    ChainIndex, IndexerError, Network as ZinderNetwork, RawTransactionBytes, RemoteChainIndex,
    RemoteOpenOptions, TransactionBroadcastResult,
};
use zpay_core::broadcast::{BroadcastClient, BroadcastError, BroadcastOutcome};

/// Production broadcast client backed by zinder's `WalletQuery.BroadcastTransaction`.
pub(crate) struct ZinderBroadcastClient {
    chain: RemoteChainIndex,
}

/// Errors raised while constructing a [`ZinderBroadcastClient`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum ZinderBroadcastConfigError {
    /// The supplied endpoint did not parse as a valid gRPC URI.
    #[error("invalid zinder endpoint URI: {reason}")]
    EndpointInvalid {
        /// Operator-facing reason.
        reason: String,
    },
}

impl ZinderBroadcastClient {
    /// Connect to the supplied `WalletQuery` endpoint.
    ///
    /// The channel is built lazily; only URI parsing happens here. Transport
    /// errors surface on the first `broadcast` call.
    ///
    /// # Errors
    ///
    /// Returns [`ZinderBroadcastConfigError::EndpointInvalid`] if `endpoint`
    /// does not parse as a gRPC URI.
    pub(crate) fn connect(
        endpoint: String,
        network: ZinderNetwork,
    ) -> Result<Self, ZinderBroadcastConfigError> {
        let chain =
            RemoteChainIndex::connect(RemoteOpenOptions { endpoint, network }).map_err(|err| {
                ZinderBroadcastConfigError::EndpointInvalid {
                    reason: err.to_string(),
                }
            })?;
        Ok(Self { chain })
    }
}

impl BroadcastClient for ZinderBroadcastClient {
    async fn broadcast(&self, raw_tx_hex: &str) -> Result<BroadcastOutcome, BroadcastError> {
        let raw_bytes = hex::decode(raw_tx_hex).map_err(|err| BroadcastError::Unavailable {
            reason: format!("raw_tx_hex did not decode: {err}"),
        })?;
        let bytes = RawTransactionBytes::new(raw_bytes);
        let broadcast_result = self
            .chain
            .broadcast_transaction(bytes)
            .await
            .map_err(|err| map_indexer_error(&err))?;
        Ok(map_broadcast_result(&broadcast_result))
    }
}

fn map_indexer_error(err: &IndexerError) -> BroadcastError {
    BroadcastError::Unavailable {
        reason: err.to_string(),
    }
}

fn map_broadcast_result(broadcast_result: &TransactionBroadcastResult) -> BroadcastOutcome {
    match broadcast_result {
        TransactionBroadcastResult::Accepted(accepted) => BroadcastOutcome::Accepted {
            transaction_id: hex::encode(accepted.transaction_id.as_bytes()),
        },
        TransactionBroadcastResult::Duplicate(duplicate) => BroadcastOutcome::Duplicate {
            upstream_message: duplicate.message.clone(),
        },
        // Surfaced as Duplicate (a success kind) per zinder's
        // `BroadcastQueued` contract: the upstream already has the
        // bytes in its download or verification queue and will mine
        // the locally-computed tx_id eventually. The agent retries
        // settle as a no-op because the cache entry is already gone.
        TransactionBroadcastResult::Queued(queued) => BroadcastOutcome::Duplicate {
            upstream_message: queued.message.clone(),
        },
        TransactionBroadcastResult::InvalidEncoding(invalid) => BroadcastOutcome::InvalidEncoding {
            upstream_message: invalid.message.clone(),
        },
        TransactionBroadcastResult::Rejected(rejected) => BroadcastOutcome::Rejected {
            upstream_message: rejected.message.clone(),
        },
        TransactionBroadcastResult::Unknown(unknown) => BroadcastOutcome::Unknown {
            upstream_message: unknown.message.clone(),
        },
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "TransactionBroadcastResult is #[non_exhaustive]; future variants surface as Unknown until they have an explicit mapping"
        )]
        _ => BroadcastOutcome::Unknown {
            upstream_message: "unrecognised broadcast outcome from upstream".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::map_broadcast_result;
    use zinder_client::{
        BroadcastAccepted, BroadcastDuplicate, BroadcastRejected, TransactionBroadcastResult,
        TransactionId,
    };
    use zpay_core::broadcast::BroadcastOutcome;

    #[test]
    fn accepted_maps_with_lowercase_hex_txid() -> Result<(), &'static str> {
        let raw_bytes = [0xabu8; 32];
        let broadcast_result = TransactionBroadcastResult::Accepted(BroadcastAccepted {
            transaction_id: TransactionId::from_bytes(raw_bytes),
        });
        let outcome = map_broadcast_result(&broadcast_result);
        let BroadcastOutcome::Accepted { transaction_id } = outcome else {
            return Err("expected Accepted variant");
        };
        assert_eq!(transaction_id, "ab".repeat(32));
        Ok(())
    }

    #[test]
    fn duplicate_carries_upstream_message() {
        let broadcast_result = TransactionBroadcastResult::Duplicate(BroadcastDuplicate {
            error_code: None,
            message: "already in mempool".to_owned(),
        });
        let outcome = map_broadcast_result(&broadcast_result);
        assert!(matches!(
            outcome,
            BroadcastOutcome::Duplicate { ref upstream_message } if upstream_message == "already in mempool"
        ));
    }

    #[test]
    fn rejected_is_not_a_success_kind() {
        let broadcast_result = TransactionBroadcastResult::Rejected(BroadcastRejected {
            kind: zinder_client::BroadcastRejectionReason::Unknown,
            error_code: Some(-26),
            message: "policy: dust output".to_owned(),
        });
        let outcome = map_broadcast_result(&broadcast_result);
        assert!(!outcome.is_success_kind());
    }
}
