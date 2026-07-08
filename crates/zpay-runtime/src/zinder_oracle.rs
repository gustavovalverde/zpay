//! Production [`ConfirmationOracle`] backed by `zinder-client::RemoteChainIndex`.
//!
//! Resolves a hex-encoded ZIP-244 transaction id back to a typed
//! confirmation outcome by combining `transaction_by_id` (placement) with
//! `chain_value_pools_at_tip` (current chain tip).
//!
//! Mirrors the [`ZinderBroadcastClient`][super::zinder_broadcast] module:
//! the channel is opened lazily and HTTP/2 keepalive plus tonic 0.14's
//! channel-self-heal pattern are inherited from `RemoteChainIndex`.

use zally_core::TxId;
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
        let transaction_id = decode_rpc_transaction_id(transaction_id_hex)?;
        let status = self
            .chain
            .transaction_by_id(transaction_id, None)
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
                let tip_height: u64 = u64::from(tip.chain_epoch.visible_tip_height.value());
                // saturating_sub guards against the rare case where the
                // reorg has retracted the tip below the tx height between
                // our two reads.
                let confirmation_count = confirmation_count(block_height, tip_height);
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

fn decode_rpc_transaction_id(transaction_id_hex: &str) -> Result<TransactionId, OracleError> {
    let tx_id =
        TxId::from_rpc_hex(transaction_id_hex).map_err(|err| OracleError::ResponseMalformed {
            reason: format!("transaction_id is not valid RPC hex: {err}"),
        })?;
    Ok(TransactionId::from_bytes(*tx_id.as_bytes()))
}

fn confirmation_count(block_height: u64, visible_tip_height: u64) -> u32 {
    u32::try_from(
        visible_tip_height
            .saturating_sub(block_height)
            .saturating_add(1),
    )
    .unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::{confirmation_count, decode_rpc_transaction_id};

    #[test]
    fn decodes_rpc_txid_before_querying_zinder() -> Result<(), Box<dyn std::error::Error>> {
        let internal_bytes = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ];
        let rpc_hex = "1f1e1d1c1b1a191817161514131211100f0e0d0c0b0a09080706050403020100";

        let decoded = decode_rpc_transaction_id(rpc_hex)?;

        assert_eq!(decoded.as_bytes(), internal_bytes);
        Ok(())
    }

    #[test]
    fn confirmation_count_uses_visible_tip_inclusive_depth() {
        assert_eq!(confirmation_count(4_152_902, 4_152_953), 52);
    }

    #[test]
    fn confirmation_count_saturates_when_tip_retracts_below_tx_height() {
        assert_eq!(confirmation_count(4_152_902, 4_152_901), 1);
    }
}
