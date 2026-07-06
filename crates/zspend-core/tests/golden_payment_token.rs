//! Cross-language golden `at+jwt` conformance (PRD-43 Slice 1, D-C).
//!
//! `tests/fixtures/golden_payment_token.json` is a byte-for-byte copy of the
//! SDK's `packages/sdk/src/testing/__fixtures__/golden-payment-token.json`,
//! which is the source of truth: it is produced by the `@zentity/sdk/testing`
//! minter (`mintPaymentAuthorizationToken`) and asserted byte-identical by
//! `payment-token.golden.test.ts` on the TS side. This test loads the SAME
//! JSON and runs the wallet's real `verify_access_token` over the SAME token
//! string, proving one minter serves both repos.
//!
//! The token's `exp` is `4_102_444_800` (year 2100), so it verifies regardless
//! of wall clock under the verifier's default leeway: jsonwebtoken's
//! `validate_exp` only rejects an expired token, never a far-future one.

use jsonwebtoken::jwk::JwkSet;
use serde::Deserialize;
use zspend_core::verify_access_token;

type TestResult = Result<(), Box<dyn core::error::Error>>;

/// Leeway matching `access_token.rs`'s test default. Irrelevant to a
/// far-future `exp`, but pinned so the conformance run mirrors production.
const LEEWAY: u64 = 60;

/// The committed cross-language fixture. `expected_*` fields document the
/// values both repos pin so a drift surfaces as a field assertion, not a raw
/// verify failure.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoldenFixture {
    token: String,
    jwks: JwkSet,
    audience: String,
    expected_recipient: String,
    expected_intent_hash: String,
    cnf_jkt: String,
}

fn load_fixture() -> Result<GoldenFixture, serde_json::Error> {
    let raw = include_str!("fixtures/golden_payment_token.json");
    serde_json::from_str(raw)
}

#[test]
fn verifies_the_sdk_minted_golden_token() -> TestResult {
    let fixture = load_fixture()?;

    let claims = verify_access_token(&fixture.token, &fixture.jwks, &fixture.audience, LEEWAY)?;

    assert_eq!(
        claims.aud, fixture.audience,
        "aud must match the wallet pin"
    );
    assert_eq!(
        claims.cnf.jkt, fixture.cnf_jkt,
        "cnf.jkt must round-trip from the minter",
    );

    let auth = claims.payment_authorization()?;

    assert_eq!(
        auth.authorization_type,
        zspend_core::PaymentAuthorizationType::PaymentAuthorization,
    );
    assert_eq!(auth.chain.namespace, "zcash");
    assert_eq!(auth.chain.reference, "test");
    assert_eq!(auth.recipient, fixture.expected_recipient);
    assert_eq!(auth.amount.currency, "ZEC");
    assert_eq!(auth.amount.value, "50000000");
    assert_eq!(auth.amount.unit, zspend_core::AmountUnit::Base);
    assert_eq!(auth.intent_hash.0, fixture.expected_intent_hash);
    assert!(
        matches!(
            auth.expires_at,
            zspend_core::ExpiresAt::BlockHeight(4_047_100)
        ),
        "expires_at must parse as the signed block_height",
    );
    Ok(())
}

#[test]
fn rejects_the_golden_token_under_a_wrong_audience() -> TestResult {
    let fixture = load_fixture()?;
    let outcome = verify_access_token(
        &fixture.token,
        &fixture.jwks,
        "urn:zentity:wallet:other",
        LEEWAY,
    );
    assert!(
        outcome.is_err(),
        "a token minted for one wallet must not verify against another's audience pin",
    );
    Ok(())
}
