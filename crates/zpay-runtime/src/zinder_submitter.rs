//! Production `Submitter` backed by `zinder-client::RemoteChainIndex`.
//!
//! Replaces the prior `ZinderBroadcastClient` shape (Phase 2f of
//! Proposal-0003): the broadcast trait now lives in zally as
//! `zally_chain::Submitter`, and zpay-runtime ships the production
//! implementation against zinder's gRPC `WalletQuery.BroadcastTransaction`.

use std::sync::Arc;

use async_trait::async_trait;
use zally_chain::{RejectionReason, SubmitOutcome, Submitter, SubmitterError};
use zally_core::{Network, TxId};
use zinder_client::{
    EndpointBackedIndex, IndexerError, RawTransactionBytes, RemoteChainIndex,
    TransactionBroadcastResult,
};

/// Production submitter backed by zinder's `WalletQuery.BroadcastTransaction`.
pub(crate) struct ZinderSubmitter {
    chain: Arc<RemoteChainIndex>,
    network: Network,
}

impl ZinderSubmitter {
    pub(crate) fn new(chain: RemoteChainIndex, network: Network) -> Self {
        Self {
            chain: Arc::new(chain),
            network,
        }
    }
}

#[async_trait]
impl Submitter for ZinderSubmitter {
    fn network(&self) -> Network {
        self.network
    }

    async fn submit(&self, raw_tx: &[u8]) -> Result<SubmitOutcome, SubmitterError> {
        let raw = RawTransactionBytes::new(raw_tx.to_vec());
        let response = self
            .chain
            .broadcast_transaction(raw)
            .await
            .map_err(|err| map_indexer_error(&err))?;
        Ok(map_broadcast_result(response))
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "TransactionBroadcastResult is non_exhaustive; unknown variants fall through to Rejected so a future zinder variant does not silently coerce to success."
)]
fn map_broadcast_result(response: TransactionBroadcastResult) -> SubmitOutcome {
    match response {
        TransactionBroadcastResult::Accepted(detail) => SubmitOutcome::Accepted {
            tx_id: tx_id_from_transaction_id(detail.transaction_id),
        },
        TransactionBroadcastResult::Duplicate(_) => SubmitOutcome::Duplicate {
            // zinder's Duplicate carries an upstream message, not a tx_id; we
            // synthesize a zero-bytes TxId so the SubmitOutcome variant stays
            // typed. The persisted BroadcastOutcome::Duplicate carries the
            // upstream_message reason on the wire so the operator does not
            // lose context.
            tx_id: TxId::from_bytes([0_u8; 32]),
        },
        TransactionBroadcastResult::Queued(_) => SubmitOutcome::Queued {
            // Same handling as Duplicate: Queued's identifier is the upstream
            // queue state, not a chain tx_id. The Phase 2f settlement-side
            // BroadcastOutcome mapping treats Queued as Accepted regardless.
            tx_id: TxId::from_bytes([0_u8; 32]),
        },
        TransactionBroadcastResult::InvalidEncoding(detail) => SubmitOutcome::Rejected {
            reason: RejectionReason::Unknown,
            detail: detail.message,
        },
        TransactionBroadcastResult::Rejected(detail) => SubmitOutcome::Rejected {
            reason: RejectionReason::Unknown,
            detail: detail.message,
        },
        TransactionBroadcastResult::Unknown(detail) => SubmitOutcome::Rejected {
            reason: RejectionReason::Unknown,
            detail: detail.message,
        },
        _ => SubmitOutcome::Rejected {
            reason: RejectionReason::Unknown,
            detail: "zinder returned an unrecognised broadcast variant".to_owned(),
        },
    }
}

fn tx_id_from_transaction_id(tx_id: zinder_client::TransactionId) -> TxId {
    TxId::from_bytes(tx_id.as_bytes())
}

fn map_indexer_error(err: &IndexerError) -> SubmitterError {
    SubmitterError::Unavailable {
        reason: err.to_string(),
    }
}
