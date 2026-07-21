//! `CaptureSubmitter`: records the signed transaction bytes instead of
//! broadcasting them.
//!
//! Implements [`zally_chain::Submitter`] so the wallet's `send_payment` path
//! can be reused unchanged, but instead of forwarding the bytes to a chain
//! plane the submitter captures them into an inner buffer. The runtime then
//! base64-encodes the captured bytes into the `/v1/payments/sign` response so
//! the caller (the agent BFF) can forward them to `zpay-runtime /settle`.
//!
//! The submit call returns [`SubmitOutcome::Queued`] with the canonical
//! transaction ID derived from the captured bytes, allowing the wallet to
//! validate the success response against its own extracted transaction.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use zally_chain::{
    RejectionReason, SubmitOutcome, Submitter, SubmitterError, parse_transaction_id,
};
use zally_core::Network;

/// Submitter that records the raw transaction bytes instead of broadcasting.
pub(crate) struct CaptureSubmitter {
    network: Network,
    captured: Arc<Mutex<Option<Vec<u8>>>>,
}

impl CaptureSubmitter {
    pub(crate) fn new(network: Network) -> Self {
        Self {
            network,
            captured: Arc::new(Mutex::new(None)),
        }
    }

    /// Removes and returns the captured transaction bytes, if any were
    /// recorded. After this call the submitter's inner buffer is empty.
    pub(crate) fn take_captured(&self) -> Option<Vec<u8>> {
        self.captured.lock().take()
    }
}

#[async_trait]
impl Submitter for CaptureSubmitter {
    fn network(&self) -> Network {
        self.network
    }

    async fn submit(&self, raw_tx: &[u8]) -> Result<SubmitOutcome, SubmitterError> {
        let tx_id = match parse_transaction_id(raw_tx) {
            Ok(tx_id) => tx_id,
            Err(error) => {
                return Ok(SubmitOutcome::Rejected {
                    reason: RejectionReason::InvalidEncoding,
                    detail: error.to_string(),
                });
            }
        };
        *self.captured.lock() = Some(raw_tx.to_vec());
        Ok(SubmitOutcome::Queued { tx_id })
    }
}

#[cfg(test)]
mod tests {
    use super::CaptureSubmitter;
    use zally_chain::{SubmitOutcome, Submitter};
    use zally_core::{BranchId, Network};
    use zcash_primitives::transaction::{TransactionData, TxVersion};
    use zcash_protocol::consensus::BlockHeight;

    fn valid_transaction_bytes() -> Result<Vec<u8>, zally_chain::SubmitterError> {
        let transaction = TransactionData::from_parts(
            TxVersion::V5,
            BranchId::Nu5,
            0,
            BlockHeight::from_u32(123_456),
            None,
            None,
            None,
            None,
        )
        .freeze()
        .map_err(|error| zally_chain::SubmitterError::Unavailable {
            reason: format!("minimal transaction did not freeze: {error}"),
        })?;
        let mut bytes = Vec::new();
        transaction.write(&mut bytes).map_err(|error| {
            zally_chain::SubmitterError::Unavailable {
                reason: format!("minimal transaction did not serialize: {error}"),
            }
        })?;
        Ok(bytes)
    }

    #[tokio::test]
    async fn captures_bytes_and_queues_for_wallet_txid() -> Result<(), zally_chain::SubmitterError>
    {
        let submitter = CaptureSubmitter::new(Network::Testnet);
        let raw_tx = valid_transaction_bytes()?;
        let expected_tx_id = zally_chain::parse_transaction_id(&raw_tx).map_err(|error| {
            zally_chain::SubmitterError::Unavailable {
                reason: error.to_string(),
            }
        })?;
        let outcome = submitter.submit(&raw_tx).await?;
        assert_eq!(
            outcome,
            SubmitOutcome::Queued {
                tx_id: expected_tx_id,
            }
        );
        assert_eq!(submitter.network(), Network::Testnet);
        assert_eq!(
            submitter.take_captured().as_deref(),
            Some(raw_tx.as_slice()),
        );
        assert!(submitter.take_captured().is_none());
        Ok(())
    }

    #[tokio::test]
    async fn malformed_bytes_are_rejected_without_capture()
    -> Result<(), zally_chain::SubmitterError> {
        let submitter = CaptureSubmitter::new(Network::Testnet);
        let outcome = submitter.submit(b"not-a-transaction").await?;

        assert!(matches!(
            outcome,
            SubmitOutcome::Rejected {
                reason: zally_chain::RejectionReason::InvalidEncoding,
                ..
            }
        ));
        assert!(submitter.take_captured().is_none());
        Ok(())
    }
}
