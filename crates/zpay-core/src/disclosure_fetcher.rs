//! Fetch the on-chain transaction a ZIP-311 payment disclosure references.
//!
//! [`DisclosureFetcher`] is the chain-plane abstraction that the local
//! payment-disclosure verifier in [`crate::verify::local`] composes with to
//! reach the on-chain bytes a disclosure pins to. It mirrors the sibling
//! pattern used by [`crate::broadcast::BroadcastClient`],
//! [`crate::tip::ChainTipOracle`], and [`crate::oracle::ConfirmationOracle`]:
//! a single-method `Send + Sync` async trait that returns either a typed
//! minimal subset of the transaction or a transport-class error.
//!
//! The fetcher is intentionally separate from the verifier so the
//! ZIP-311 cryptography stays in-process while the chain-data side
//! plugs into whichever explorer plane the operator runs. Two runtime
//! implementations live in `zpay-runtime`:
//!
//! - A zinder-backed fetcher that reads `WalletQuery.TransactionById`
//!   plus the prevout resolution path.
//! - A rejecting fetcher used when no explorer endpoint is configured;
//!   every call returns [`FetchError::Unavailable`] so the verifier's
//!   wire response surfaces `chain_presence: "oracle_unavailable"`.
//!
//! [`DisclosedTransaction`] is the MINIMAL view the verifier needs: the
//! transparent inputs' prevout scriptPubKey bytes (used for BIP-322
//! legacy verification) and the shielded-output fields the deferred
//! Sapling slice will consume once `verify_sapling` lands. It is not a
//! re-encoding of the full `zcash_primitives::Transaction`.

use std::future::Future;

use serde::{Deserialize, Serialize};

/// Minimal subset of a Zcash transaction the local payment-disclosure
/// verifier reads. Carries only the fields the cryptography needs:
///
/// - Transparent prevout scriptPubKeys per vin index, so the BIP-322
///   legacy verifier can recover and compare the spending pubkey hash.
/// - Sapling output cipher data the deferred Sapling slice will
///   consume once `verify_sapling` lands.
///
/// Other transaction-level facts (lock time, expiry, fee, branch id)
/// are out of scope: the verifier asserts the disclosure pins to a
/// transaction the chain plane knows about, not that the transaction
/// itself is well-formed by Zcash consensus rules. Consensus is the
/// chain plane's responsibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DisclosedTransaction {
    /// Canonical ZIP-244 transaction id in RPC byte order. Echoed
    /// verbatim into the verify response when present.
    pub txid: [u8; 32],
    /// Transparent inputs, in vin order. The verifier indexes into this
    /// vector by the disclosure's `index` field, so position MUST match
    /// the on-chain vin order.
    pub transparent_inputs: Vec<DisclosedTransparentInput>,
    /// Sapling outputs, in shielded-output order. Reserved for the
    /// Sapling slice; not consumed in this commit.
    pub sapling_outputs: Vec<DisclosedSaplingOutput>,
}

/// Transparent input projection the verifier reads.
///
/// `prevout_script_pub_key` is the resolved scriptPubKey of the
/// previously-known output this input spends, in raw bytes. The
/// verifier extracts the P2PKH hash160 from it and compares against
/// the pubkey hash recovered from the BIP-322 signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DisclosedTransparentInput {
    /// Outpoint transaction id this input spends from.
    pub prevout_txid: [u8; 32],
    /// Output index inside the prevout transaction.
    pub prevout_index: u32,
    /// Raw scriptPubKey of the resolved prevout. Empty when the chain
    /// plane could not resolve the outpoint; the verifier surfaces
    /// that as `CryptographicVerdict::Inconclusive { UnsupportedPool }`.
    pub prevout_script_pub_key: Vec<u8>,
}

/// Sapling output projection the verifier reads.
///
/// Carries only the fields the deferred `verify_sapling` slice
/// consumes: `cv`, `cmu`, `ephemeral_key`, `enc_ciphertext`, and the
/// outgoing cipher key (`ock`). Not consumed in this commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DisclosedSaplingOutput {
    /// Value commitment to the note.
    pub cv: [u8; 32],
    /// Note commitment.
    pub cmu: [u8; 32],
    /// Ephemeral key the recipient derives the shared secret from.
    pub ephemeral_key: [u8; 32],
}

/// Errors raised by [`DisclosureFetcher`] implementations.
///
/// Mirrors the transport-vs-categorical split used elsewhere in this
/// crate: `Unavailable` is a retryable transport-class failure; a
/// transaction the chain plane simply does not know about surfaces as
/// the typed [`Self::NotFound`] outcome (verifier maps it to
/// `chain_presence: "not_found"`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FetchError {
    /// Chain plane has no record of the transaction. Verifier maps to
    /// `chain_presence: "not_found"` on the wire.
    #[error("transaction not found on chain plane")]
    NotFound,
    /// Chain plane could not be reached. Verifier maps to
    /// `chain_presence: "oracle_unavailable"` on the wire.
    #[error("transaction fetcher unavailable: {reason}")]
    Unavailable {
        /// Operator-facing reason.
        reason: String,
    },
}

/// Abstraction over the chain plane that resolves a ZIP-244 transaction
/// id to the minimal subset the local ZIP-311 verifier reads.
///
/// Implementors are pinned to `Send + Sync` so a single fetcher can be
/// shared across the Axum router.
pub trait DisclosureFetcher: Send + Sync {
    /// Fetch the transaction `txid` references.
    ///
    /// `txid` is in RPC byte order (the form users see in block
    /// explorers). Implementations decode and translate to whichever
    /// shape the underlying chain plane consumes.
    fn fetch_transaction(
        &self,
        txid: [u8; 32],
    ) -> impl Future<Output = Result<DisclosedTransaction, FetchError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::{
        DisclosedSaplingOutput, DisclosedTransaction, DisclosedTransparentInput, FetchError,
    };

    #[test]
    fn fetch_error_variants_display_their_reason() {
        let err = FetchError::Unavailable {
            reason: "dial timeout".to_owned(),
        };
        assert!(format!("{err}").contains("dial timeout"));
        let err = FetchError::NotFound;
        assert!(format!("{err}").contains("not found"));
    }

    #[test]
    fn disclosed_transaction_serializes() -> Result<(), Box<dyn std::error::Error>> {
        let tx = DisclosedTransaction {
            txid: [0u8; 32],
            transparent_inputs: vec![DisclosedTransparentInput {
                prevout_txid: [1u8; 32],
                prevout_index: 0,
                prevout_script_pub_key: vec![0x76, 0xA9, 0x14],
            }],
            sapling_outputs: vec![DisclosedSaplingOutput {
                cv: [2u8; 32],
                cmu: [3u8; 32],
                ephemeral_key: [4u8; 32],
            }],
        };
        let json = serde_json::to_string(&tx)?;
        let parsed: DisclosedTransaction = serde_json::from_str(&json)?;
        assert_eq!(parsed.txid, [0u8; 32]);
        assert_eq!(parsed.transparent_inputs.len(), 1);
        assert_eq!(parsed.sapling_outputs.len(), 1);
        Ok(())
    }
}
