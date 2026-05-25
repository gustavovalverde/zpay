//! Cross-cutting domain types for the payment lifecycle.

use serde::{Deserialize, Serialize};

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

impl PaymentNetwork {
    /// CAIP-style network identifier used in `accepts[].network`.
    #[must_use]
    pub const fn caip_id(self) -> &'static str {
        match self {
            Self::Mainnet => "zcash:mainnet",
            Self::Testnet => "zcash:testnet",
            Self::Regtest => "zcash:regtest",
        }
    }
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

/// Integer zatoshis. 1 ZEC = `100_000_000` zatoshis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Zatoshis(pub u64);

/// Operator-assigned merchant identifier; lowercase kebab-case.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MerchantId(pub String);

/// Identifier returned by the confirmation oracle for a per-txid
/// subscription.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WatchId(pub String);

/// 32-byte SHA-256 hash binding an on-chain payment to a zentity
/// evidence pack.
///
/// Stored in bytes 66-97 of the ZIP-302 memo per
/// [PRD-42 Decision 11](https://github.com/gustavovalverde/zentity/blob/main/docs/plans/prd-42-zcash-agentic-payments-cross-stack.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EvidencePackHash(pub [u8; 32]);
