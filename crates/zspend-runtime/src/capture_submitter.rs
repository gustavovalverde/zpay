//! `CaptureSubmitter`: a Phase 4 pragmatic shortcut.
//!
//! Implements [`zally_chain::Submitter`] so the wallet's `send_payment` path
//! can be reused unchanged, but instead of forwarding the bytes to a chain
//! plane the submitter captures them into an inner buffer and returns
//! `SubmitOutcome::Accepted` with a synthesised `tx_id`. The runtime then
//! base64-encodes the captured bytes into the `/v1/payments/sign` response so
//! the caller (the agent BFF) can forward them to `zpay-runtime /settle`.
//!
//! The follow-on slice replaces this with the PCZT path once Phase 2d ships
//! `Wallet::construct_pczt` and Phase 2g ships the extractor on
//! `zpay-runtime`.

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
        let tx_id = synthesise_tx_id(raw_tx);
        *self.captured.lock() = Some(raw_tx.to_vec());
        Ok(SubmitOutcome::Accepted { tx_id })
    }
}

/// SHA-256-derived [`TxId`] for the Phase 4 capture shortcut.
///
/// The wallet's `send_payment` path uses the value the submitter returns as
/// the canonical transaction identifier in `SendOutcome::signed.tx_id`; the
/// runtime forwards that same id to the agent in the `signed_payload.tx_id`
/// field. Deterministic for byte-identical input so a replay through the
/// wallet's idempotency layer surfaces the same id.
fn synthesise_tx_id(raw_tx: &[u8]) -> TxId {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(raw_tx);
    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(&digest);
    TxId::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::CaptureSubmitter;
    use zally_chain::{SubmitOutcome, Submitter};
    use zally_core::Network;

    #[tokio::test]
    async fn captures_bytes_and_returns_accepted() -> Result<(), zally_chain::SubmitterError> {
        let submitter = CaptureSubmitter::new(Network::Testnet);
        let outcome = submitter.submit(b"raw-tx").await?;
        match outcome {
            SubmitOutcome::Accepted { tx_id } => {
                assert_eq!(submitter.network(), Network::Testnet);
                assert_eq!(
                    submitter.take_captured().as_deref(),
                    Some(b"raw-tx".as_slice())
                );
                // tx_id is deterministic for identical bytes; we cannot pattern-match
                // on internal contents because TxId hides its bytes.
                let again = CaptureSubmitter::new(Network::Testnet)
                    .submit(b"raw-tx")
                    .await?;
                if let SubmitOutcome::Accepted { tx_id: again_tx_id } = again {
                    assert_eq!(tx_id, again_tx_id);
                } else {
                    return Err(zally_chain::SubmitterError::Unavailable {
                        reason: "second submit was not Accepted".to_owned(),
                    });
                }
            }
            SubmitOutcome::Duplicate { .. }
            | SubmitOutcome::Queued { .. }
            | SubmitOutcome::Rejected { .. } => {
                return Err(zally_chain::SubmitterError::Unavailable {
                    reason: "expected Accepted, got non-accepted outcome".to_owned(),
                });
            }
            #[allow(
                clippy::wildcard_enum_match_arm,
                reason = "SubmitOutcome is non_exhaustive; future variants must fail this assertion until the test is updated to handle them"
            )]
            _ => {
                return Err(zally_chain::SubmitterError::Unavailable {
                    reason: "expected Accepted, got unknown non-exhaustive variant".to_owned(),
                });
            }
        }
        // A second take after the first returns None.
        assert!(submitter.take_captured().is_none());
        Ok(())
    }
}
