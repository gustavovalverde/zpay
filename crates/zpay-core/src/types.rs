//! Cross-cutting domain types for the payment lifecycle.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Maximum accepted length for a `PaymentId` string. ULIDs are 26
/// chars; the ceiling is generous so legacy ids and operator-provided
/// trace ids stay representable without inflating the hashmap key
/// space.
pub const PAYMENT_ID_MAX_LEN: usize = 64;

/// Wire-protocol scheme advertised in `accepts[]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PaymentScheme {
    /// ZEC settled on the Zcash network.
    Zcash,
}

/// Network identifier carried by every payment-bearing type.
///
/// Constructors that take a network value fail closed on mismatch with an
/// address, key, or transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PaymentNetwork {
    /// Mainnet. Real funds.
    Mainnet,
    /// Public testnet.
    Testnet,
    /// Local-only regtest.
    Regtest,
}

/// Server-issued payment identifier.
///
/// Generated at `prepare` time, opaque to clients, durable on the
/// settlement ledger. ULID-shaped under the hood; clients treat as opaque.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaymentId(pub String);

impl PaymentId {
    /// Generate a fresh `PaymentId`.
    #[must_use]
    pub fn new() -> Self {
        Self(ulid::Ulid::new().to_string())
    }
}

impl Default for PaymentId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for PaymentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reason a string failed to parse as a `PaymentId`. Surfaced as the
/// Axum `Path<PaymentId>` extractor's rejection reason so the wire
/// returns a clean 422.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum PaymentIdParseError {
    /// Empty or all-whitespace input.
    #[error("payment_id must not be empty or whitespace")]
    Empty,
    /// Input exceeded [`PAYMENT_ID_MAX_LEN`] after trim.
    #[error("payment_id exceeds {limit} characters", limit = PAYMENT_ID_MAX_LEN)]
    TooLong,
}

impl FromStr for PaymentId {
    type Err = PaymentIdParseError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(PaymentIdParseError::Empty);
        }
        if trimmed.len() > PAYMENT_ID_MAX_LEN {
            return Err(PaymentIdParseError::TooLong);
        }
        Ok(Self(trimmed.to_owned()))
    }
}

/// Integer zatoshis. 1 ZEC = `100_000_000` zatoshis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Zatoshis(pub u64);

/// Operator-assigned payee identifier; lowercase kebab-case.
///
/// Names the registry row whose `accepts[]` template a prepare request
/// resolves against. "Payee" is deliberately broader than "merchant":
/// the same vocabulary covers commerce, donations, P2P, and bill pay.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PayeeId(pub String);

/// Identifier returned by the confirmation oracle for a per-txid
/// subscription.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WatchId(pub String);

/// 32-byte SHA-256 hash binding an on-chain payment to an evidence
/// pack produced by the relying party. Stored in the trailing
/// 32 bytes of the ZIP-302 protocol memo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EvidencePackHash(pub [u8; 32]);
