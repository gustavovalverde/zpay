//! Production [`ConfirmationOracle`] backed by `zinder-client::RemoteChainIndex`.
//!
//! Resolves a hex-encoded ZIP-244 transaction id back to a typed
//! confirmation outcome by combining `transaction_by_id` (placement) with
//! `chain_value_pools_at_tip` (current chain tip).
//!
//! Mirrors the [`ZinderBroadcastClient`][super::zinder_broadcast] module:
//! the channel is opened lazily and HTTP/2 keepalive plus tonic 0.14's
//! channel-self-heal pattern are inherited from `RemoteChainIndex`.

use zinder_client::{
    ChainIndex, EndpointBackedIndex, IndexerError, Network as ZinderNetwork, RemoteChainIndex,
    RemoteOpenOptions, TransactionId, TxStatus,
};
use zpay_core::chain_status::ChainStatusView;
use zpay_core::oracle::{ConfirmationOracle, ConfirmationOutcome, OracleError};

/// Production confirmation oracle backed by zinder's `WalletQuery.TransactionById`.
pub(crate) struct ZinderConfirmationOracle {
    chain: RemoteChainIndex,
}

/// Errors raised while constructing a [`ZinderConfirmationOracle`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum ZinderOracleConfigError {
    /// The supplied endpoint did not parse as a valid gRPC URI.
    #[error("invalid zinder endpoint URI: {reason}")]
    EndpointInvalid {
        /// Operator-facing reason.
        reason: String,
    },
}

impl ZinderConfirmationOracle {
    /// Open a connection to the supplied `WalletQuery` endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ZinderOracleConfigError::EndpointInvalid`] if `endpoint`
    /// does not parse as a gRPC URI.
    pub(crate) fn connect(
        endpoint: String,
        network: ZinderNetwork,
    ) -> Result<Self, ZinderOracleConfigError> {
        let chain =
            RemoteChainIndex::connect(RemoteOpenOptions { endpoint, network }).map_err(|err| {
                ZinderOracleConfigError::EndpointInvalid {
                    reason: err.to_string(),
                }
            })?;
        Ok(Self { chain })
    }
}

impl ConfirmationOracle for ZinderConfirmationOracle {
    async fn fetch_confirmations(
        &self,
        transaction_id_hex: &str,
    ) -> Result<ConfirmationOutcome, OracleError> {
        let txid_bytes =
            hex::decode(transaction_id_hex).map_err(|err| OracleError::ResponseMalformed {
                reason: format!("transaction_id is not valid hex: {err}"),
            })?;
        let txid_array: [u8; 32] =
            txid_bytes
                .try_into()
                .map_err(|_| OracleError::ResponseMalformed {
                    reason: "transaction_id must be exactly 32 bytes".to_owned(),
                })?;
        let status = self
            .chain
            .transaction_by_id(TransactionId::from_bytes(txid_array), None)
            .await
            .map_err(|err| map_indexer_error(&err))?;
        match status {
            TxStatus::Mined(mined) => {
                let block_height: u64 = u64::from(mined.location.block_height.value());
                let tip = self
                    .chain
                    .chain_value_pools_at_tip()
                    .await
                    .map_err(|err| map_indexer_error(&err))?;
                let tip_height: u64 = u64::from(tip.tip_height.value());
                // saturating_sub guards against the rare case where the
                // reorg has retracted the tip below the tx height between
                // our two reads.
                let confirmation_count =
                    u32::try_from(tip_height.saturating_sub(block_height).saturating_add(1))
                        .unwrap_or(u32::MAX);
                Ok(ConfirmationOutcome::Mined {
                    block_height,
                    confirmation_count,
                })
            }
            TxStatus::InMempool(_) => Ok(ConfirmationOutcome::InMempool),
            TxStatus::ConflictingChain => Ok(ConfirmationOutcome::ConflictingChain),
            #[allow(
                clippy::wildcard_enum_match_arm,
                reason = "TxStatus is #[non_exhaustive]; NotFound is the safe default for the explicit NotFound variant and any future additions"
            )]
            TxStatus::NotFound | _ => Ok(ConfirmationOutcome::NotFound),
        }
    }

    async fn chain_status(&self) -> Result<ChainStatusView, OracleError> {
        let tip = self
            .chain
            .chain_value_pools_at_tip()
            .await
            .map_err(|err| map_indexer_error(&err))?;
        Ok(ChainStatusView {
            visible_tip_height: Some(u64::from(tip.chain_epoch.visible_tip_height.value())),
            settled_tip_height: Some(u64::from(tip.chain_epoch.settled_tip_height.value())),
        })
    }
}

fn map_indexer_error(err: &IndexerError) -> OracleError {
    OracleError::Unavailable {
        reason: err.to_string(),
    }
}
