//! Production payment-disclosure transaction fetcher backed by `WalletQuery`.

use zinder_client::{ChainIndex, RemoteChainIndex, TransactionId, TxStatus};
use zpay_core::disclosure_fetcher::{DisclosedTransaction, DisclosureFetcher, FetchError};

/// Fetches exact mined transaction context from Zinder's native wallet plane.
pub(crate) struct ZinderTransactionFetcher {
    chain: RemoteChainIndex,
}

impl ZinderTransactionFetcher {
    /// Wrap an already-configured remote chain index.
    pub(crate) const fn new(chain: RemoteChainIndex) -> Self {
        Self { chain }
    }
}

impl DisclosureFetcher for ZinderTransactionFetcher {
    async fn fetch_transaction(
        &self,
        rpc_txid: [u8; 32],
    ) -> Result<DisclosedTransaction, FetchError> {
        let expected_transaction_id = internal_transaction_id(rpc_txid);
        let status = self
            .chain
            .transaction_by_id(expected_transaction_id, None)
            .await
            .map_err(|error| FetchError::Unavailable {
                reason: error.to_string(),
            })?;
        let mined = match status {
            TxStatus::Mined(mined) => mined,
            TxStatus::NotFound | TxStatus::InMempool(_) | TxStatus::ConflictingChain => {
                return Err(FetchError::NotFound);
            }
            _ => {
                return Err(FetchError::Unavailable {
                    reason: "zinder returned an unsupported transaction status".to_owned(),
                });
            }
        };
        if mined.location.transaction_id != expected_transaction_id {
            return Err(FetchError::Unavailable {
                reason: "zinder returned a different transaction id".to_owned(),
            });
        }
        let raw_transaction_bytes =
            mined
                .raw_transaction_bytes
                .ok_or_else(|| FetchError::Unavailable {
                    reason: "zinder does not retain transaction blobs; set storage.raw_blob_policy to transactions or all before ingest"
                        .to_owned(),
                })?;
        Ok(DisclosedTransaction::new(
            rpc_txid,
            raw_transaction_bytes,
            mined.location.block_height.value(),
        ))
    }
}

fn internal_transaction_id(mut rpc_txid: [u8; 32]) -> TransactionId {
    rpc_txid.reverse();
    TransactionId::from_bytes(rpc_txid)
}

#[cfg(test)]
mod tests {
    use super::internal_transaction_id;

    #[test]
    fn rpc_transaction_id_is_reversed_for_zinder_lookup() {
        let rpc_txid = [
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31,
        ];
        let expected_internal_txid = [
            31, 30, 29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10,
            9, 8, 7, 6, 5, 4, 3, 2, 1, 0,
        ];

        assert_eq!(
            internal_transaction_id(rpc_txid).as_bytes(),
            expected_internal_txid
        );
    }
}
