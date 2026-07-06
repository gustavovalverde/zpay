//! `CaptureSubmitter`: records the signed transaction bytes instead of
//! broadcasting them.
//!
//! Implements [`zally_chain::Submitter`] so the wallet's `send_payment` path
//! can be reused unchanged, but instead of forwarding the bytes to a chain
//! plane the submitter captures them into an inner buffer. The runtime then
//! base64-encodes the captured bytes into the `/v1/payments/sign` response so
//! the caller (the agent BFF) can forward them to `zpay-runtime /settle`.
//!
//! The submit call returns [`SubmitOutcome::Queued`]: the wallet's
//! `resolve_send_outcome` treats `Queued` as success-equivalent and surfaces
//! its own ZIP-244 `tx_id` (computed at prepare time) rather than any id the
//! submitter returns, so the `/v1/payments/sign` response carries the real
//! transaction id.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use zally_chain::{SubmitOutcome, Submitter, SubmitterError};
use zally_core::{Network, TxId};

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
        *self.captured.lock() = Some(raw_tx.to_vec());
        // `Queued` makes the wallet's `resolve_send_outcome` discard this id and
        // return the ZIP-244 txid it computed at prepare time; the zeroed value
        // is a placeholder that never reaches the caller.
        Ok(SubmitOutcome::Queued {
            tx_id: TxId::from_bytes([0_u8; 32]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::CaptureSubmitter;
    use zally_chain::{SubmitOutcome, Submitter};
    use zally_core::Network;

    #[tokio::test]
    async fn captures_bytes_and_queues_for_wallet_txid() -> Result<(), zally_chain::SubmitterError>
    {
        let submitter = CaptureSubmitter::new(Network::Testnet);
        let outcome = submitter.submit(b"raw-tx").await?;
        assert!(
            matches!(outcome, SubmitOutcome::Queued { .. }),
            "capture submitter must queue so the wallet returns its own ZIP-244 txid, not a byte-derived id",
        );
        assert_eq!(submitter.network(), Network::Testnet);
        assert_eq!(
            submitter.take_captured().as_deref(),
            Some(b"raw-tx".as_slice()),
        );
        assert!(submitter.take_captured().is_none());
        Ok(())
    }

    #[tokio::test]
    async fn queued_tx_id_is_independent_of_the_bytes() -> Result<(), zally_chain::SubmitterError> {
        // Regression guard: the submitter must not derive the tx id from the
        // bytes. Two different payloads produce the same placeholder id because
        // the wallet, not the submitter, owns the real identifier.
        let first = capture_queued_tx_id(b"one").await?;
        let second = capture_queued_tx_id(b"two").await?;
        assert_eq!(
            first, second,
            "queued placeholder id must not depend on input bytes"
        );
        Ok(())
    }

    async fn capture_queued_tx_id(bytes: &[u8]) -> Result<[u8; 32], zally_chain::SubmitterError> {
        let outcome = CaptureSubmitter::new(Network::Testnet)
            .submit(bytes)
            .await?;
        match outcome {
            SubmitOutcome::Queued { tx_id } => Ok(*tx_id.as_bytes()),
            SubmitOutcome::Accepted { .. }
            | SubmitOutcome::Duplicate { .. }
            | SubmitOutcome::Rejected { .. } => Err(zally_chain::SubmitterError::Unavailable {
                reason: "expected Queued".to_owned(),
            }),
            #[allow(
                clippy::wildcard_enum_match_arm,
                reason = "SubmitOutcome is non_exhaustive; future variants must fail this assertion until the test handles them"
            )]
            _ => Err(zally_chain::SubmitterError::Unavailable {
                reason: "expected Queued, got unknown non-exhaustive variant".to_owned(),
            }),
        }
    }
}
