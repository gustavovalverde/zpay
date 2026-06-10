//! Parsed-tuple intent-hash recompute (Proposal-0003 D-4).
//!
//! The wallet independently re-derives the `intent_hash` from the recipient and
//! amount it PARSED out of the `payment_request`, combined with the chain,
//! `payment_id`, and expiry carried in the signed RAR. A mismatch against
//! [`PaymentAuthorization::intent_hash`] means a caller substituted a different
//! payment URI than the one the user approved; the spend is refused before any
//! signing work.
//!
//! The numeric hash uses `zally_core::IntentHash`, the same hasher the zentity
//! issuer mirrors in `@zentity/sdk/protocol`. The conformance test below pins
//! the wallet's RAR-to-`IntentInput` mapping to the cross-language vector so a
//! byte-layout drift between the two implementations fails in CI rather than on
//! a live spend.

use zally_core::{IntentHash, IntentHashError, IntentInput};

use crate::payment_authorization::{ExpiresAt, PaymentAuthorization};

/// Recompute the wire-form `intent_hash` for `auth` against the recipient and
/// amount the wallet parsed from the `payment_request`.
///
/// The chain, `payment_id`, and expiry come from the signed RAR; the
/// `recipient_caip10` and `amount_value` come from the caller's parsed URI (the
/// bytes the wallet will actually sign). Returns the `"v1:sha256:<base64url>"`
/// wire string.
///
/// # Errors
///
/// Returns [`IntentHashError`] when a hashed field exceeds the wire length cap.
pub fn recompute_intent_hash(
    auth: &PaymentAuthorization,
    recipient_caip10: &str,
    amount_value: u64,
) -> Result<String, IntentHashError> {
    let input = IntentInput {
        chain_namespace: &auth.chain.namespace,
        chain_reference: &auth.chain.reference,
        recipient_caip10,
        amount_value,
        amount_unit: auth.amount.unit,
        payment_id: &auth.payment_id,
        expiry_height: expiry_height(&auth.expires_at),
    };
    Ok(IntentHash::compute(&input)?.to_wire_string())
}

/// Whether the parsed `(recipient, amount)` tuple reproduces the RAR's
/// `intent_hash`. This equality is the D-4 binding check the wallet runs before
/// signing.
///
/// # Errors
///
/// Returns [`IntentHashError`] when the recompute fails.
pub fn intent_matches(
    auth: &PaymentAuthorization,
    recipient_caip10: &str,
    amount_value: u64,
) -> Result<bool, IntentHashError> {
    Ok(recompute_intent_hash(auth, recipient_caip10, amount_value)? == auth.intent_hash.0)
}

/// Project a chain-tagged expiry onto the scalar height the hasher binds.
///
/// All known variants are enumerated so an upstream addition forces a decision
/// here; the `_` arm covers only the `#[non_exhaustive]` hidden tail and maps
/// to `0`, which fails the binding check (fail closed) for an expiry kind this
/// wallet does not understand.
fn expiry_height(expires_at: &ExpiresAt) -> u64 {
    match *expires_at {
        ExpiresAt::BlockHeight(height) => u64::from(height),
        ExpiresAt::Slot(scalar)
        | ExpiresAt::BlockNumber(scalar)
        | ExpiresAt::TimestampSeconds(scalar) => scalar,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use zally_core::{IntentHash, IntentHashError};

    use super::{intent_matches, recompute_intent_hash};
    use crate::payment_authorization::{
        Amount, AmountUnit, ChainId, ExpiresAt, IntentHashString, PaymentAuthorization,
        PaymentAuthorizationType,
    };

    type TestResult = Result<(), Box<dyn core::error::Error>>;

    /// The shared cross-language conformance vector.
    ///
    /// Identical inputs and digest appear in `zally-core::intent_hash::tests`
    /// and the SDK's `ZCASH_TESTNET_MINIMAL_VECTOR`
    /// (packages/sdk/src/protocol/intent-hash.ts).
    const VECTOR_RECIPIENT: &str = "zcash:test:utest1qq...";
    const VECTOR_AMOUNT: u64 = 50_000_000;
    const VECTOR_DIGEST_HEX: &str =
        "b47e481896e757a3714a5f679b06c573c1160c5eab7d871780e7c71669888d44";

    fn vector_auth(intent_hash: &str) -> PaymentAuthorization {
        PaymentAuthorization {
            authorization_type: PaymentAuthorizationType::PaymentAuthorization,
            chain: ChainId {
                namespace: "zcash".to_owned(),
                reference: "test".to_owned(),
            },
            recipient: VECTOR_RECIPIENT.to_owned(),
            amount: Amount {
                currency: "ZEC".to_owned(),
                value: VECTOR_AMOUNT.to_string(),
                unit: AmountUnit::Base,
            },
            payment_id: "01KT9A0V431VGD5YH7R7G635HC".to_owned(),
            intent_hash: IntentHashString(intent_hash.to_owned()),
            expires_at: ExpiresAt::BlockHeight(4_047_100),
        }
    }

    fn digest_hex(wire: &str) -> Result<String, IntentHashError> {
        Ok(hex::encode(IntentHash::from_wire_string(wire)?.as_bytes()))
    }

    #[test]
    fn recompute_matches_cross_language_conformance_vector() -> TestResult {
        let auth = vector_auth("v1:sha256:placeholder");
        let wire = recompute_intent_hash(&auth, VECTOR_RECIPIENT, VECTOR_AMOUNT)?;
        assert!(wire.starts_with("v1:sha256:"));
        assert_eq!(digest_hex(&wire)?, VECTOR_DIGEST_HEX);
        Ok(())
    }

    #[test]
    fn intent_matches_true_when_parsed_tuple_reproduces_rar() -> TestResult {
        let wire = recompute_intent_hash(
            &vector_auth("v1:sha256:placeholder"),
            VECTOR_RECIPIENT,
            VECTOR_AMOUNT,
        )?;
        let auth = vector_auth(&wire);
        assert!(intent_matches(&auth, VECTOR_RECIPIENT, VECTOR_AMOUNT)?);
        Ok(())
    }

    #[test]
    fn intent_matches_false_when_recipient_substituted() -> TestResult {
        let wire = recompute_intent_hash(
            &vector_auth("v1:sha256:placeholder"),
            VECTOR_RECIPIENT,
            VECTOR_AMOUNT,
        )?;
        let auth = vector_auth(&wire);
        assert!(!intent_matches(
            &auth,
            "zcash:test:utest1qattacker",
            VECTOR_AMOUNT
        )?);
        Ok(())
    }

    #[test]
    fn intent_matches_false_when_amount_substituted() -> TestResult {
        let wire = recompute_intent_hash(
            &vector_auth("v1:sha256:placeholder"),
            VECTOR_RECIPIENT,
            VECTOR_AMOUNT,
        )?;
        let auth = vector_auth(&wire);
        assert!(!intent_matches(&auth, VECTOR_RECIPIENT, VECTOR_AMOUNT + 1)?);
        Ok(())
    }
}
