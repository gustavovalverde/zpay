//! `at+jwt` access-token verification (Proposal-0003 D-1, D-5, D-14).
//!
//! The wallet accepts a spend only when the caller presents a DPoP-bound
//! `at+jwt` whose `authorization_details[0]` is the signed `payment_authorization`
//! grant. This module verifies the token's signature and claims against the
//! issuer's JWKS; the DPoP proof-of-possession check (RFC 9449) and the
//! intent-hash recompute (D-4) are separate steps the runtime runs after this
//! one.
//!
//! ## Algorithm pinning
//!
//! The verification algorithm is taken from the resolved JWKS key (kid -> key ->
//! alg), never from the attacker-influenced token header. A token whose header
//! `alg` disagrees with its key's type is rejected. This closes the classic
//! alg-confusion / key-confusion seam (RS256<->ES256, `alg: none`). v1 issues
//! access tokens as EdDSA; the wallet also accepts ES256 keys, and rejects every
//! other key type.

use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

use crate::error::{ProblemDetail, ProblemKind};
use crate::payment_authorization::PaymentAuthorization;

/// DPoP confirmation claim (RFC 9449 `cnf`).
///
/// Binds the access token to the holder's DPoP key; the wallet later verifies
/// the inbound DPoP proof is signed by the key whose thumbprint equals `jkt`.
#[derive(Clone, Debug, Deserialize)]
pub struct Confirmation {
    /// SHA-256 JWK thumbprint of the bound DPoP key.
    pub jkt: String,
}

/// Verified claims of a `payment_authorization` access token.
#[derive(Clone, Debug, Deserialize)]
pub struct AccessTokenClaims {
    /// Audience: the wallet instance's JWK thumbprint (D-5). The issuer mints a
    /// single-string `aud`, never a URL.
    pub aud: String,
    /// Single-use spend identifier (D-8).
    pub jti: String,
    /// Expiry in epoch seconds. Validated against the clock with leeway.
    pub exp: i64,
    /// DPoP key binding.
    pub cnf: Confirmation,
    /// Rich Authorization Request entries. v1 carries exactly one (D-14).
    pub authorization_details: Vec<PaymentAuthorization>,
}

impl AccessTokenClaims {
    /// The single RAR entry. Rejects any token that does not carry exactly one
    /// `payment_authorization` (D-14).
    ///
    /// # Errors
    ///
    /// Returns [`ProblemKind::RarTooManyEntries`] when the count is not one.
    pub fn payment_authorization(&self) -> Result<&PaymentAuthorization, ProblemDetail> {
        match self.authorization_details.as_slice() {
            [single] => Ok(single),
            entries => Err(ProblemDetail::not_retryable(
                ProblemKind::RarTooManyEntries,
                "rar_too_many_entries",
                format!(
                    "v1 accepts exactly one authorization_details entry; got {}",
                    entries.len()
                ),
            )),
        }
    }
}

/// Verify a DPoP-bound `at+jwt` against the issuer's JWKS.
///
/// Pins the algorithm to the resolved key's type, then checks signature, `exp`
/// (with `leeway_seconds` tolerance), and `aud == expected_audience`.
///
/// # Errors
///
/// Returns a [`ProblemDetail`] with [`ProblemKind::AccessTokenInvalid`] for a
/// missing/unknown key, malformed token, alg mismatch, or bad signature;
/// [`ProblemKind::AudienceMismatch`] when `aud` does not match; and
/// [`ProblemKind::AuthorizationExpired`] when the token has expired.
pub fn verify_access_token(
    token: &str,
    jwks: &JwkSet,
    expected_audience: &str,
    leeway_seconds: u64,
) -> Result<AccessTokenClaims, ProblemDetail> {
    let header =
        decode_header(token).map_err(|err| access_invalid("malformed token header", &err))?;
    let kid = header
        .kid
        .ok_or_else(|| access_invalid_msg("access token header carries no kid"))?;
    let jwk = jwks
        .find(&kid)
        .ok_or_else(|| access_invalid_msg("no JWKS key matches the token kid"))?;

    let key_alg = jwk_algorithm(jwk)?;
    if header.alg != key_alg {
        return Err(access_invalid_msg(
            "token header alg does not match the JWKS key type (alg-confusion guard)",
        ));
    }

    let decoding_key =
        DecodingKey::from_jwk(jwk).map_err(|err| access_invalid("unusable JWKS key", &err))?;
    let mut validation = Validation::new(key_alg);
    validation.leeway = leeway_seconds;
    validation.set_audience(&[expected_audience]);
    validation.set_required_spec_claims(&["exp", "aud"]);

    let decoded = decode::<AccessTokenClaims>(token, &decoding_key, &validation)
        .map_err(|err| map_decode_error(&err))?;
    Ok(decoded.claims)
}

/// Resolve the verification algorithm from the key material, not the token.
///
/// v1 issues EdDSA access tokens; ES256 is accepted for forward compatibility.
/// Every other key type is rejected fail-closed.
fn jwk_algorithm(jwk: &Jwk) -> Result<Algorithm, ProblemDetail> {
    let unsupported = || {
        access_invalid_msg(
            "JWKS key type not supported for at+jwt verification (expected OKP/Ed25519 or EC/P-256)",
        )
    };
    match &jwk.algorithm {
        AlgorithmParameters::OctetKeyPair(_) => Ok(Algorithm::EdDSA),
        AlgorithmParameters::EllipticCurve(ec) if ec.curve == EllipticCurve::P256 => {
            Ok(Algorithm::ES256)
        }
        AlgorithmParameters::EllipticCurve(_)
        | AlgorithmParameters::RSA(_)
        | AlgorithmParameters::OctetKey(_) => Err(unsupported()),
    }
}

fn map_decode_error(err: &jsonwebtoken::errors::Error) -> ProblemDetail {
    use jsonwebtoken::errors::ErrorKind;
    if matches!(err.kind(), ErrorKind::ExpiredSignature) {
        ProblemDetail::not_retryable(
            ProblemKind::AuthorizationExpired,
            "authorization_expired",
            "the access token has expired; request a new authorization",
        )
    } else if matches!(err.kind(), ErrorKind::InvalidAudience) {
        ProblemDetail::not_retryable(
            ProblemKind::AudienceMismatch,
            "audience_mismatch",
            "the access token aud does not match this wallet's thumbprint",
        )
    } else {
        access_invalid("access token failed verification", err)
    }
}

fn access_invalid(context: &str, err: &jsonwebtoken::errors::Error) -> ProblemDetail {
    ProblemDetail::not_retryable(
        ProblemKind::AccessTokenInvalid,
        "access_token_invalid",
        format!("{context}: {err}"),
    )
}

fn access_invalid_msg(detail: &str) -> ProblemDetail {
    ProblemDetail::not_retryable(
        ProblemKind::AccessTokenInvalid,
        "access_token_invalid",
        detail.to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::jwk::{Jwk, JwkSet};
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use serde_json::json;

    use super::{ProblemKind, verify_access_token};

    type TestResult = Result<(), Box<dyn core::error::Error>>;

    const KID: &str = "test-issuer-key";
    const WALLET_AUD: &str = "wallet-jkt-thumbprint";
    const DPOP_JKT: &str = "dpop-key-thumbprint";
    const RECIPIENT: &str = "zcash:test:utest1qq";
    const LEEWAY: u64 = 60;
    const FAR_FUTURE: i64 = 4_102_444_800; // 2100-01-01
    const ALREADY_EXPIRED: i64 = 1_000_000_000; // 2001

    struct Issuer {
        encoding: EncodingKey,
        jwks: JwkSet,
    }

    fn issuer() -> Result<Issuer, Box<dyn core::error::Error>> {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)?;
        let keypair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())?;
        let public_x = URL_SAFE_NO_PAD.encode(keypair.public_key().as_ref());
        let encoding = EncodingKey::from_ed_der(pkcs8.as_ref());
        let jwk: Jwk = serde_json::from_value(json!({
            "kty": "OKP",
            "crv": "Ed25519",
            "x": public_x,
            "kid": KID,
            "alg": "EdDSA",
            "use": "sig",
        }))?;
        Ok(Issuer {
            encoding,
            jwks: JwkSet { keys: vec![jwk] },
        })
    }

    fn rar() -> serde_json::Value {
        json!({
            "type": "payment_authorization",
            "chain": { "namespace": "zcash", "reference": "test" },
            "recipient": RECIPIENT,
            "amount": { "currency": "ZEC", "value": "50000000", "unit": "base" },
            "payment_id": "01KT9A0V431VGD5YH7R7G635HC",
            "intent_hash": "v1:sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "expires_at": { "kind": "block_height", "value": 4_047_100 }
        })
    }

    fn token_claims(aud: &str, exp: i64, details: serde_json::Value) -> serde_json::Value {
        let mut claims = json!({
            "aud": aud,
            "jti": "01JTI000000000000000000000",
            "exp": exp,
            "cnf": { "jkt": DPOP_JKT },
        });
        claims["authorization_details"] = details;
        claims
    }

    fn mint(
        issuer: &Issuer,
        claims: &serde_json::Value,
    ) -> Result<String, jsonwebtoken::errors::Error> {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(KID.to_owned());
        encode(&header, claims, &issuer.encoding)
    }

    #[test]
    fn accepts_a_conformant_token() -> TestResult {
        let issuer = issuer()?;
        let token = mint(
            &issuer,
            &token_claims(WALLET_AUD, FAR_FUTURE, json!([rar()])),
        )?;
        let verified = verify_access_token(&token, &issuer.jwks, WALLET_AUD, LEEWAY)?;
        assert_eq!(verified.aud, WALLET_AUD);
        assert_eq!(verified.cnf.jkt, DPOP_JKT);
        let auth = verified.payment_authorization()?;
        assert_eq!(auth.recipient, RECIPIENT);
        Ok(())
    }

    #[test]
    fn rejects_tampered_signature() -> TestResult {
        let issuer = issuer()?;
        let token = mint(
            &issuer,
            &token_claims(WALLET_AUD, FAR_FUTURE, json!([rar()])),
        )?;
        let mut bytes = token.into_bytes();
        if let Some(last) = bytes.last_mut() {
            *last = if *last == b'A' { b'B' } else { b'A' };
        }
        let tampered = String::from_utf8(bytes)?;
        assert!(matches!(
            verify_access_token(&tampered, &issuer.jwks, WALLET_AUD, LEEWAY),
            Err(err) if err.kind == ProblemKind::AccessTokenInvalid
        ));
        Ok(())
    }

    #[test]
    fn rejects_wrong_audience() -> TestResult {
        let issuer = issuer()?;
        let token = mint(
            &issuer,
            &token_claims("some-other-wallet", FAR_FUTURE, json!([rar()])),
        )?;
        assert!(matches!(
            verify_access_token(&token, &issuer.jwks, WALLET_AUD, LEEWAY),
            Err(err) if err.kind == ProblemKind::AudienceMismatch
        ));
        Ok(())
    }

    #[test]
    fn rejects_expired_token() -> TestResult {
        let issuer = issuer()?;
        let token = mint(
            &issuer,
            &token_claims(WALLET_AUD, ALREADY_EXPIRED, json!([rar()])),
        )?;
        assert!(matches!(
            verify_access_token(&token, &issuer.jwks, WALLET_AUD, LEEWAY),
            Err(err) if err.kind == ProblemKind::AuthorizationExpired
        ));
        Ok(())
    }

    #[test]
    fn rejects_unknown_kid() -> TestResult {
        let signer = issuer()?;
        let token = mint(
            &signer,
            &token_claims(WALLET_AUD, FAR_FUTURE, json!([rar()])),
        )?;
        // A JWKS that does not contain the signing kid.
        let foreign = JwkSet {
            keys: vec![serde_json::from_value(json!({
                "kty": "OKP",
                "crv": "Ed25519",
                "x": URL_SAFE_NO_PAD.encode([7_u8; 32]),
                "kid": "different-kid",
                "alg": "EdDSA",
                "use": "sig",
            }))?],
        };
        assert!(matches!(
            verify_access_token(&token, &foreign, WALLET_AUD, LEEWAY),
            Err(err) if err.kind == ProblemKind::AccessTokenInvalid
        ));
        Ok(())
    }

    #[test]
    fn rejects_multiple_rar_entries() -> TestResult {
        let issuer = issuer()?;
        let token = mint(
            &issuer,
            &token_claims(WALLET_AUD, FAR_FUTURE, json!([rar(), rar()])),
        )?;
        let verified = verify_access_token(&token, &issuer.jwks, WALLET_AUD, LEEWAY)?;
        assert!(matches!(
            verified.payment_authorization(),
            Err(err) if err.kind == ProblemKind::RarTooManyEntries
        ));
        Ok(())
    }
}
