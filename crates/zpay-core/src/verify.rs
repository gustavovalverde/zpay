//! Verify a ZIP-311 payment disclosure against on-chain state.
//!
//! zpay delegates the actual ZIP-311 cryptography to zinder's
//! `VerifyPaymentDisclosure` RPC. This module composes the typed request,
//! interprets the typed response, and applies zpay's additional checks
//! (recipient match, amount match, `evidence_pack_hash` equality).
//!
//! Implementation lands in M2.

use serde::{Deserialize, Serialize};

use crate::types::Zatoshis;

/// Input to `verify`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyRequest {
    /// Transaction identifier whose receipt is being verified.
    pub txid: String,
    /// Amount the merchant expected to receive.
    pub expected_amount_zat: Zatoshis,
    /// ZIP-311 payment-disclosure payload bytes.
    pub disclosure_payload_hex: String,
}

/// Output of `verify`. The merchant gates its product delivery on a
/// `Verdict::Valid` outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisclosureVerdict {
    /// Categorical verdict.
    pub verdict: Verdict,
}

/// Categorical disclosure-verification outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Verdict {
    /// Disclosure verified and matches expectations.
    Valid,
    /// Disclosure verified but the amount does not match.
    MismatchAmount,
    /// Disclosure verified but the recipient does not match.
    MismatchRecipient,
    /// Disclosure signature did not verify.
    InvalidSignature,
    /// Disclosed transaction not on chain.
    TransactionNotFound,
    /// Upstream capability is disabled; operator must enable.
    CapabilityUnavailable,
}
