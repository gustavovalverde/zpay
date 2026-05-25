//! Prepare a payment: compose the recipient URI, the protocol memo, and the
//! `payment_id` that the agent attaches to its merchant request.
//!
//! The actual preparation logic lands in M1. This module currently only
//! declares the typed contract.

use serde::{Deserialize, Serialize};

use crate::types::{EvidencePackHash, MerchantId, PaymentId, PaymentNetwork, Zatoshis};

/// Input to `prepare`. Composed by a wire adapter from a protocol-specific
/// request shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareRequest {
    /// Merchant whose `accepts[]` template applies.
    pub merchant_id: MerchantId,
    /// Network the payment will settle on.
    pub network: PaymentNetwork,
    /// Amount the merchant expects in zatoshis.
    pub amount_zat: Zatoshis,
    /// Evidence-pack hash binding this payment to a zentity proof set.
    pub evidence_pack_hash: EvidencePackHash,
}

/// Output of `prepare`. The agent passes `payment_uri` and `memo_bytes`
/// to the user's wallet for signing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preparation {
    /// Server-issued opaque identifier; pair with the agent's DPoP JKT.
    pub payment_id: PaymentId,
    /// ZIP-321 payment URI for the user's wallet to consume.
    pub payment_uri: String,
    /// 98-byte protocol memo content (protocol byte + version + three
    /// 32-byte hashes).
    pub memo_bytes: Vec<u8>,
    /// Block height after which this preparation cannot be settled.
    pub expiry_height: u32,
}
