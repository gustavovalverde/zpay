//! Production [`ConfirmationOracle`] backed by `zinder-client::RemoteChainIndex`.
//!
//! Resolves a hex-encoded ZIP-244 transaction id back to a typed
//! confirmation outcome from the placement and confirmation count bound to
//! the transaction lookup's chain epoch.
//!
//! The channel is opened lazily; HTTP/2 keepalive and tonic's channel
//! self-healing are inherited from `RemoteChainIndex`.

use zally_core::TxId;
use zinder_client::{ChainIndex, IndexerError, RemoteChainIndex, TransactionId, TxStatus};
use zpay_core::chain_status::ChainStatusView;
use zpay_core::oracle::{ConfirmationOracle, ConfirmationOutcome, OracleError};

/// Production confirmation oracle backed by zinder's `WalletQuery.Transaction`.
pub(crate) struct ZinderConfirmationOracle {
    chain: RemoteChainIndex,
}

impl ZinderConfirmationOracle {
    pub(crate) const fn new(chain: RemoteChainIndex) -> Self {
        Self { chain }
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
                Ok(ConfirmationOutcome::Mined {
                    block_height,
                    confirmation_count: mined.chain_context.confirmations,
                })
            }
            TxStatus::InMempool(_) => Ok(ConfirmationOutcome::InMempool),
            TxStatus::NotFound => Ok(ConfirmationOutcome::NotFound),
            #[allow(
                clippy::wildcard_enum_match_arm,
                reason = "TxStatus is non-exhaustive; future variants fail closed until this consumer defines their confirmation semantics"
            )]
            _ => Err(OracleError::ResponseMalformed {
                reason: "zinder returned an unsupported transaction status".to_owned(),
            }),
        }
    }

    async fn chain_status(&self) -> Result<ChainStatusView, OracleError> {
        let epoch = self
            .chain
            .current_epoch()
            .await
            .map_err(|err| map_indexer_error(&err))?;
        Ok(ChainStatusView {
            visible_tip_height: Some(u64::from(epoch.visible_tip_height.value())),
            settled_tip_height: Some(u64::from(epoch.settled_tip_height.value())),
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

#[cfg(test)]
mod tests {
    use super::decode_rpc_transaction_id;

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
}
