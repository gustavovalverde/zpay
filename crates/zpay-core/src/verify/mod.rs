//! Verify a ZIP-311 payment disclosure against on-chain state.
//!
//! ## 3-axis verdict model
//!
//! The wire response (`POST /x402/v2/verify`) splits the verdict into
//! three orthogonal axes:
//!
//! - [`CryptographicVerdict`] answers "are the disclosure bytes
//!   well-formed and is every signature valid?". Runs entirely
//!   in-process via [`local::LocalPaymentDisclosureVerifier`], which
//!   composes [`mod@parse_zip311`], [`digest`], and [`transparent`].
//! - [`ChainPresence`] answers "is the disclosed transaction visible
//!   on the chain plane?". Driven by the
//!   [`crate::disclosure_fetcher::DisclosureFetcher`] outcome.
//! - [`AmountReconciliation`] answers "does the disclosed value
//!   match what the merchant expected?". Reserved for a follow-on
//!   slice; today every response carries
//!   `AmountReconciliation::NotChecked`.
//!
//! Callers that previously checked `verdict == "valid"` migrate to
//! `cryptographic_verdict == "valid" && chain_presence == "mined" &&
//! amount_reconciliation == "match"`. There is no backwards-compat
//! shim: the fused single-verdict shape is gone.
//!
//! ## Trait split
//!
//! [`PaymentDisclosureVerifier`] runs only the cryptography; it
//! never reaches the chain plane directly. The chain-side data lives
//! behind [`crate::disclosure_fetcher::DisclosureFetcher`], a
//! top-level sibling trait that mirrors the existing
//! `BroadcastClient` / `ChainTipOracle` / `ConfirmationOracle`
//! convention.
//!
//! See [ADR-0007](https://github.com/gustavovalverde/zpay/blob/main/docs/adrs/0007-local-zip311-verifier.md)
//! for the design rationale.

pub mod digest;
pub mod local;
pub mod parse_zip311;
pub mod transparent;

use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::disclosure_fetcher::{FetchError, DisclosureFetcher};
use crate::types::Zatoshis;
use crate::verify::parse_zip311::parse as parse_zip311;

pub use local::LocalPaymentDisclosureVerifier;

/// Input to [`verify`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyRequest {
    /// Transaction identifier whose receipt is being verified.
    pub txid: String,
    /// Amount the merchant expected to receive.
    pub expected_amount_zat: Zatoshis,
    /// ZIP-311 payment-disclosure payload bytes, hex-encoded.
    pub disclosure_payload_hex: String,
}

/// Wire response shape of `POST /x402/v2/verify`.
///
/// All three axes are carried as separate fields so callers can
/// reason about cryptography, chain-presence, and amount-match
/// independently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResponse {
    /// Cryptographic verdict over the disclosure bytes.
    pub cryptographic_verdict: CryptographicVerdict,
    /// Reason a cryptographic verdict was inconclusive. Present only
    /// when [`Self::cryptographic_verdict`] is
    /// [`CryptographicVerdict::Inconclusive`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inconclusive_reason: Option<InconclusiveReason>,
    /// On-chain presence verdict.
    pub chain_presence: ChainPresence,
    /// Amount-match verdict.
    pub amount_reconciliation: AmountReconciliation,
    /// Hex-encoded ZIP-244 transaction id when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    /// Payment id when the disclosure matched a prepared row.
    /// Reserved for the follow-on reconciliation slice; always
    /// `None` today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payment_id: Option<String>,
    /// Disclosed value, in zatoshis, when known. Reserved for the
    /// reconciliation slice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosed_value_zat: Option<u64>,
}

/// Cryptographic-axis verdict over the disclosure bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CryptographicVerdict {
    /// Every signature in the disclosure verified.
    Valid,
    /// At least one signature failed to verify under the configured
    /// network's digest. Includes message-binding mismatches.
    InvalidSignature,
    /// Disclosure bytes could not be decoded against the ZIP-311
    /// canonical layout.
    Malformed,
    /// Disclosure decoded but the verifier cannot answer
    /// definitively. See [`InconclusiveReason`].
    Inconclusive,
}

/// Sub-classification when [`CryptographicVerdict::Inconclusive`] is
/// the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InconclusiveReason {
    /// Disclosure referenced a pool whose verifier is not enabled
    /// (e.g. Sapling without the `verify_sapling` feature, or a P2SH
    /// prevout).
    UnsupportedPool,
    /// Disclosure carries a version byte this build does not
    /// recognise. Forward-compat path: a future ZIP-311 revision can
    /// land without older verifiers reporting `Malformed`.
    UnknownVersion,
    /// Chain plane did not supply the prevout scriptPubKey the
    /// BIP-322-legacy verifier needs to compare the recovered pubkey
    /// hash against. Distinct from [`Self::UnsupportedPool`]: the
    /// verifier had no data to check, not the wrong kind of data.
    PrevoutUnresolved,
}

/// Chain-presence axis verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChainPresence {
    /// Chain plane has the disclosed transaction in a mined block.
    Mined,
    /// Chain plane has no record of the disclosed transaction.
    NotFound,
    /// Chain plane could not be reached. Operator should investigate
    /// the configured `ZPAY_NODE__EXPLORER_GRPC_ADDR`.
    OracleUnavailable,
}

/// Amount-reconciliation axis verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AmountReconciliation {
    /// Disclosed value matches the merchant's expected amount.
    Match,
    /// Disclosed value does not match the merchant's expected amount.
    Mismatch,
    /// Reconciliation was not performed. Today every response is
    /// `NotChecked`; the follow-on slice will populate `Match` /
    /// `Mismatch` once the verifier reads the disclosed Sapling
    /// outputs.
    NotChecked,
}

/// Errors raised by [`PaymentDisclosureVerifier`] implementations.
///
/// Reserved for transport-level failures that prevent the verifier
/// from producing any verdict at all; in-band outcomes (malformed,
/// invalid signature, inconclusive) flow through the
/// [`VerifyResponse`] shape instead.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// Disclosure payload was not hex.
    #[error("disclosure_payload_hex must be valid hex: {reason}")]
    PayloadInvalid {
        /// Operator-facing reason.
        reason: String,
    },
}

/// Abstraction over the in-process ZIP-311 disclosure verifier.
///
/// Implementations consume the raw disclosure bytes plus a
/// [`DisclosureFetcher`] for the chain-side data and return the
/// 3-axis verdict directly. The trait gains a fetcher parameter
/// rather than a separate "fetch first, verify second" composition so
/// the implementation owns the parse/verify/fetch ordering and can
/// short-circuit fetch when the parse already gave a definitive
/// answer (e.g. malformed bytes do not need a chain round-trip).
pub trait PaymentDisclosureVerifier: Send + Sync {
    /// Verify the supplied disclosure against the chain plane, using
    /// the supplied fetcher to resolve the on-chain transaction.
    fn verify_disclosure<F>(
        &self,
        disclosure_bytes: &[u8],
        fetcher: &F,
    ) -> impl Future<Output = Result<VerifyResponse, VerifyError>> + Send
    where
        F: DisclosureFetcher + ?Sized;
}

/// Drive a verify request end-to-end.
///
/// Decodes the disclosure payload, asserts that `request.txid` matches
/// the txid carried inside the parsed disclosure (chain-plane integrity
/// gate), then delegates to the verifier. The cross-check prevents a
/// caller from claiming a disclosure for transaction A is "for"
/// transaction B; a mismatch short-circuits to
/// [`CryptographicVerdict::Malformed`] without touching the fetcher.
///
/// # Errors
///
/// Returns [`VerifyError::PayloadInvalid`] when the hex envelope is
/// not valid hex. All other failures (malformed bytes, signature
/// failures, chain plane unavailable, txid mismatches) surface in-band
/// via the response axes.
pub async fn verify<V, F>(
    request: VerifyRequest,
    verifier: &V,
    fetcher: &F,
) -> Result<VerifyResponse, VerifyError>
where
    V: PaymentDisclosureVerifier,
    F: DisclosureFetcher,
{
    let disclosure_bytes =
        hex::decode(request.disclosure_payload_hex).map_err(|err| VerifyError::PayloadInvalid {
            reason: err.to_string(),
        })?;
    let _ = &request.expected_amount_zat;

    // Cross-check the request-supplied txid against the parsed
    // disclosure's txid before any fetcher call. A mismatch means the
    // caller is presenting evidence for one transaction while claiming
    // it is for another. Treat as Malformed and skip the chain
    // round-trip.
    if let Ok(parsed) = parse_zip311(&disclosure_bytes)
        && !request_txid_matches(&request.txid, &parsed.txid)
    {
        return Ok(VerifyResponse {
            cryptographic_verdict: CryptographicVerdict::Malformed,
            inconclusive_reason: None,
            chain_presence: ChainPresence::OracleUnavailable,
            amount_reconciliation: AmountReconciliation::NotChecked,
            transaction_id: Some(hex::encode(parsed.txid)),
            payment_id: None,
            disclosed_value_zat: None,
        });
    }

    verifier.verify_disclosure(&disclosure_bytes, fetcher).await
}

/// Constant-time-ish hex compare. Strips an optional `0x` prefix and
/// compares byte-by-byte after lowercasing. Returns `false` on any
/// length mismatch or non-hex character.
fn request_txid_matches(request_txid_hex: &str, parsed_txid: &[u8; 32]) -> bool {
    let trimmed = request_txid_hex
        .strip_prefix("0x")
        .or_else(|| request_txid_hex.strip_prefix("0X"))
        .unwrap_or(request_txid_hex);
    let Ok(bytes) = hex::decode(trimmed) else {
        return false;
    };
    bytes.as_slice() == parsed_txid.as_slice()
}

/// Translate a [`FetchError`] into the corresponding [`ChainPresence`]
/// axis verdict. Lives at the module level so both the local verifier
/// and any future verifier reuse one mapping.
#[must_use]
pub const fn chain_presence_for(error: &FetchError) -> ChainPresence {
    // FetchError is #[non_exhaustive]; the safe default for any
    // future transport-class variant is OracleUnavailable so the wire
    // response stays predictable.
    if matches!(error, FetchError::NotFound) {
        ChainPresence::NotFound
    } else {
        ChainPresence::OracleUnavailable
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CryptographicVerdict, LocalPaymentDisclosureVerifier, VerifyError, VerifyRequest,
        VerifyResponse, verify,
    };
    use crate::disclosure_fetcher::{DisclosedTransaction, FetchError, DisclosureFetcher};
    use crate::types::{PaymentNetwork, Zatoshis};
    use crate::verify::parse_zip311::{
        ZIP311_VERSION_V1, Zip311Disclosure, Zip311TransparentInput, encode_signed,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Fetcher that records whether it was consulted; tests assert
    /// `consulted == false` for the short-circuit paths.
    struct BannedFetcher {
        consulted: parking_lot::Mutex<bool>,
    }

    impl BannedFetcher {
        fn new() -> Self {
            Self {
                consulted: parking_lot::Mutex::new(false),
            }
        }

        fn was_consulted(&self) -> bool {
            *self.consulted.lock()
        }
    }

    impl DisclosureFetcher for BannedFetcher {
        async fn fetch_transaction(
            &self,
            _txid: [u8; 32],
        ) -> Result<DisclosedTransaction, FetchError> {
            *self.consulted.lock() = true;
            Err(FetchError::Unavailable {
                reason: "banned fetcher consulted".to_owned(),
            })
        }
    }

    fn minimal_signed_disclosure(parsed_txid: [u8; 32]) -> Zip311Disclosure {
        Zip311Disclosure {
            version: ZIP311_VERSION_V1,
            txid: parsed_txid,
            message: b"msg".to_vec(),
            transparent_inputs: vec![Zip311TransparentInput {
                index: 0,
                signature: vec![0u8; 65],
            }],
            sapling_spends: Vec::new(),
            sapling_outputs: Vec::new(),
        }
    }

    /// When `request.txid` does not match `parsed.txid` the verify
    /// driver returns `Malformed` without consulting the fetcher.
    #[tokio::test]
    async fn cross_check_short_circuits_on_request_txid_mismatch() -> TestResult {
        let parsed_txid = [0x11u8; 32];
        let disclosure = minimal_signed_disclosure(parsed_txid);
        let payload_hex = hex::encode(encode_signed(&disclosure));
        let request = VerifyRequest {
            txid: hex::encode([0x22u8; 32]),
            expected_amount_zat: Zatoshis(0),
            disclosure_payload_hex: payload_hex,
        };
        let verifier = LocalPaymentDisclosureVerifier::new(PaymentNetwork::Mainnet);
        let fetcher = BannedFetcher::new();

        let response: VerifyResponse = verify(request, &verifier, &fetcher).await?;
        assert_eq!(
            response.cryptographic_verdict,
            CryptographicVerdict::Malformed
        );
        assert!(
            !fetcher.was_consulted(),
            "fetcher must not be consulted on txid mismatch",
        );
        assert_eq!(response.transaction_id, Some(hex::encode(parsed_txid)));
        Ok(())
    }

    /// A `0x`-prefixed request txid is accepted when the bytes match.
    #[tokio::test]
    async fn cross_check_accepts_request_txid_with_0x_prefix() -> TestResult {
        let parsed_txid = [0x33u8; 32];
        let disclosure = minimal_signed_disclosure(parsed_txid);
        let payload_hex = hex::encode(encode_signed(&disclosure));
        let request = VerifyRequest {
            txid: format!("0x{}", hex::encode(parsed_txid)),
            expected_amount_zat: Zatoshis(0),
            disclosure_payload_hex: payload_hex,
        };
        let verifier = LocalPaymentDisclosureVerifier::new(PaymentNetwork::Mainnet);
        let fetcher = BannedFetcher::new();

        let response = verify(request, &verifier, &fetcher).await?;
        // The fetcher is consulted (and returns Unavailable), so the
        // verdict surfaces as Inconclusive, not Malformed.
        assert_ne!(
            response.cryptographic_verdict,
            CryptographicVerdict::Malformed,
            "matching 0x-prefixed txid must not collapse to Malformed",
        );
        assert!(fetcher.was_consulted());
        Ok(())
    }

    /// Non-hex `request.txid` is treated as a mismatch.
    #[tokio::test]
    async fn cross_check_rejects_non_hex_request_txid() -> TestResult {
        let parsed_txid = [0x44u8; 32];
        let disclosure = minimal_signed_disclosure(parsed_txid);
        let payload_hex = hex::encode(encode_signed(&disclosure));
        let request = VerifyRequest {
            txid: "not-hex".to_owned(),
            expected_amount_zat: Zatoshis(0),
            disclosure_payload_hex: payload_hex,
        };
        let verifier = LocalPaymentDisclosureVerifier::new(PaymentNetwork::Mainnet);
        let fetcher = BannedFetcher::new();

        let response = verify(request, &verifier, &fetcher).await?;
        assert_eq!(
            response.cryptographic_verdict,
            CryptographicVerdict::Malformed
        );
        assert!(!fetcher.was_consulted());
        Ok(())
    }

    /// A non-hex disclosure payload surfaces as
    /// [`VerifyError::PayloadInvalid`] from the driver: the only
    /// transport-class error the wire can raise.
    #[tokio::test]
    async fn payload_invalid_when_disclosure_hex_is_garbage() -> TestResult {
        let request = VerifyRequest {
            txid: hex::encode([0u8; 32]),
            expected_amount_zat: Zatoshis(0),
            disclosure_payload_hex: "not-hex".to_owned(),
        };
        let verifier = LocalPaymentDisclosureVerifier::new(PaymentNetwork::Mainnet);
        let fetcher = BannedFetcher::new();

        let outcome = verify(request, &verifier, &fetcher).await;
        assert!(matches!(outcome, Err(VerifyError::PayloadInvalid { .. })));
        assert!(!fetcher.was_consulted());
        Ok(())
    }
}
