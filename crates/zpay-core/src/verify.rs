//! Verify a ZIP-311 payment disclosure against on-chain state.
//!
//! zpay delegates the cryptography to zinder's `VerifyPaymentDisclosure`
//! RPC. This module composes the typed request, interprets the typed
//! response, and exposes a [`DisclosureVerifier`] abstraction so the
//! runtime can plug a real zinder-backed verifier behind it while
//! tests use an in-memory fake.

use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::types::Zatoshis;

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

/// Output of [`verify`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisclosureVerdict {
    /// Categorical verdict.
    pub verdict: Verdict,
    /// Disclosed transaction id when the upstream returned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    /// Disclosed payment id when the upstream returned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payment_id: Option<String>,
    /// Disclosed value the upstream asserted the payment moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosed_value_zat: Option<u64>,
}

/// Categorical disclosure-verification outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Verdict {
    /// Disclosure verified and the disclosed amount matches.
    Valid,
    /// Disclosure verified but the disclosed amount does not match the
    /// expected amount.
    MismatchAmount,
    /// Disclosure signature did not verify.
    InvalidSignature,
    /// Upstream could not find the disclosed transaction on chain.
    TransactionNotFound,
    /// Disclosure bytes are malformed or out of spec.
    Malformed,
    /// Upstream verifier capability is not enabled on this chain plane.
    CapabilityUnavailable,
}

/// Errors raised by [`DisclosureVerifier`] implementations. Transport-
/// level failures that prevented the upstream from answering.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// Upstream payload was not hex.
    #[error("disclosure_payload_hex must be valid hex: {reason}")]
    PayloadInvalid {
        /// Operator-facing reason.
        reason: String,
    },
    /// Upstream chain plane could not be reached.
    #[error("verifier unavailable: {reason}")]
    Unavailable {
        /// Operator-facing reason.
        reason: String,
    },
    /// Upstream responded but the response could not be interpreted.
    #[error("verifier response malformed: {reason}")]
    ResponseMalformed {
        /// Operator-facing reason.
        reason: String,
    },
}

/// Abstraction over the chain plane's ZIP-311 disclosure verifier.
pub trait DisclosureVerifier: Send + Sync {
    /// Verify the supplied disclosure. Returns a typed verdict; only
    /// transport failures surface as [`VerifyError`].
    fn verify_disclosure(
        &self,
        disclosure_bytes: &[u8],
    ) -> impl Future<Output = Result<DisclosureVerdict, VerifyError>> + Send;
}

/// Verify a payment disclosure by delegating to the chain plane and
/// reconciling the upstream-disclosed amount with the merchant's
/// expectation.
///
/// # Errors
///
/// Returns [`VerifyError`] when the disclosure bytes are not hex or the
/// chain plane could not be reached.
pub async fn verify<V: DisclosureVerifier>(
    request: VerifyRequest,
    verifier: &V,
) -> Result<DisclosureVerdict, VerifyError> {
    let disclosure_bytes =
        hex::decode(request.disclosure_payload_hex).map_err(|err| VerifyError::PayloadInvalid {
            reason: err.to_string(),
        })?;

    let mut verdict = verifier.verify_disclosure(&disclosure_bytes).await?;

    if verdict.verdict == Verdict::Valid {
        // Upstream verified the cryptography; we still need to make sure
        // the disclosed value matches what the merchant expected.
        if let Some(disclosed) = verdict.disclosed_value_zat
            && disclosed != request.expected_amount_zat.0
        {
            verdict.verdict = Verdict::MismatchAmount;
        }
    }

    Ok(verdict)
}
