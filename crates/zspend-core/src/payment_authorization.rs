//! [`PaymentAuthorization`]: the Rust mirror of the zentity Zod schema at
//! `@zentity/sdk/protocol` (packages/sdk/src/protocol/payment-authorization.ts).
//!
//! Per Proposal-0003 D-1 (the RAR entry IS the spend grant), D-4 (parsed-tuple
//! `intent_hash`), D-10 (CAIP-typed identifiers; decimal-string amounts), and
//! D-14 (exactly one entry per token in v1). The wallet rejects any access
//! token whose `authorization_details` does not deserialize into exactly one
//! [`PaymentAuthorization`] entry.

use serde::{Deserialize, Serialize};

/// The chain-neutral `amount`/`unit`/`expires_at` vocabulary is owned by
/// `zally-core` (the canonical wallet-library wire types) and re-exported here
/// so the RAR entry, the signed-payload envelope, and the `IntentHash` input
/// speak one vocabulary instead of three hand-kept mirrors. The JSON shapes are
/// identical to the zentity Zod schema: `amount` is `{ currency, value, unit }`
/// with a lowercase `unit`, and `expires_at` is a `{ kind, value }` tagged union.
pub use zally_core::{Amount, AmountUnit, ExpiresAt};

/// Wire-stable RAR `type` discriminator. Renaming requires a versioned
/// migration on every consumer (issuer, wallet, integrators).
pub const PAYMENT_AUTHORIZATION_TYPE_LITERAL: &str = "payment_authorization";

/// Tagged literal for the RAR `type` field.
///
/// Serializes as the string `"payment_authorization"`; rejects any other
/// value. Lifts the wire contract into the type system so a typo in a future
/// edit fails at deserialize-time rather than letting an unrecognised type
/// silently pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentAuthorizationType {
    /// The only v1 variant: the single payment-authorization RAR entry.
    PaymentAuthorization,
}

/// CAIP-2 chain identifier: `{ namespace, reference }`.
///
/// Wire example: `{ "namespace": "zcash", "reference": "test" }`. The wallet
/// does no regex validation on these fields at the type boundary; the runtime
/// gates them against the operator-pinned [`super::SigningPolicy::network`]
/// at request time, and any well-formed CAIP-2 string passes the type. The
/// zentity issuer is the canonical regex validator (D-2).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChainId {
    /// CAIP-2 namespace, e.g. `"zcash"`, `"eip155"`, `"solana"`.
    pub namespace: String,
    /// CAIP-2 chain reference, e.g. `"main"`, `"test"`, `"1"`.
    pub reference: String,
}

/// Versioned intent-hash wire string: `"v1:sha256:<base64url-no-pad>"`.
///
/// Wrapped in a newtype so the runtime cannot accidentally compare a recipient
/// string to an intent hash; the type also documents the wire prefix at every
/// use site. The zentity issuer mints this value; the wallet recomputes and
/// compares (D-4).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IntentHashString(pub String);

/// The `payment_authorization` RAR entry as it appears in
/// `authorization_details[0]` of the OAuth `at+jwt`.
///
/// Mirrors the zentity Zod schema at
/// `@zentity/sdk/protocol` (packages/sdk/src/protocol/payment-authorization.ts). The wallet recomputes
/// `intent_hash` over the parsed payment-request tuple and compares against
/// this field; mismatch returns [`super::ProblemKind::IntentMismatch`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaymentAuthorization {
    /// Always [`PaymentAuthorizationType::PaymentAuthorization`] on the wire.
    #[serde(rename = "type")]
    pub authorization_type: PaymentAuthorizationType,
    /// CAIP-2 chain identifier.
    pub chain: ChainId,
    /// CAIP-10 recipient account id.
    pub recipient: String,
    /// Bounded spend amount.
    pub amount: Amount,
    /// Issuer-assigned payment identifier (lifecycle key on zpay).
    pub payment_id: String,
    /// Parsed-tuple intent hash; wallet recomputes and compares.
    pub intent_hash: IntentHashString,
    /// Chain-tagged expiry.
    pub expires_at: ExpiresAt,
}

#[cfg(test)]
mod tests {
    use super::{
        Amount, AmountUnit, ChainId, ExpiresAt, IntentHashString, PaymentAuthorization,
        PaymentAuthorizationType,
    };
    use serde_json::json;

    fn fixture() -> PaymentAuthorization {
        PaymentAuthorization {
            authorization_type: PaymentAuthorizationType::PaymentAuthorization,
            chain: ChainId {
                namespace: "zcash".to_owned(),
                reference: "test".to_owned(),
            },
            recipient:
                "zcash:test:utest1abcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabc"
                    .to_owned(),
            amount: Amount {
                currency: "ZEC".to_owned(),
                value: "50000000".to_owned(),
                unit: AmountUnit::Base,
            },
            payment_id: "01HQ6N9YK4ZN5Z3M8WJX5T1F7Q".to_owned(),
            intent_hash: IntentHashString(
                "v1:sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
            ),
            expires_at: ExpiresAt::BlockHeight(4_047_100),
        }
    }

    #[test]
    fn round_trips_through_json() -> Result<(), serde_json::Error> {
        let parsed = fixture();
        let wire = serde_json::to_value(&parsed)?;
        assert_eq!(wire["type"].as_str(), Some("payment_authorization"));
        assert_eq!(wire["chain"]["namespace"].as_str(), Some("zcash"));
        assert_eq!(wire["amount"]["unit"].as_str(), Some("base"));
        assert_eq!(wire["expires_at"]["kind"].as_str(), Some("block_height"));
        assert_eq!(wire["expires_at"]["value"].as_u64(), Some(4_047_100));
        let back: PaymentAuthorization = serde_json::from_value(wire)?;
        assert_eq!(parsed, back);
        Ok(())
    }

    #[test]
    fn rejects_unknown_type_discriminator() {
        let bad = json!({
            "type": "subscription_authorization",
            "chain": { "namespace": "zcash", "reference": "test" },
            "recipient": "zcash:test:utest1q",
            "amount": { "currency": "ZEC", "value": "1", "unit": "base" },
            "payment_id": "p",
            "intent_hash": "v1:sha256:abc",
            "expires_at": { "kind": "block_height", "value": 1 },
        });
        let outcome: Result<PaymentAuthorization, _> = serde_json::from_value(bad);
        assert!(outcome.is_err());
    }
}
