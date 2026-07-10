//! DPoP proof-of-possession verification for `POST /v1/payments/sign`
//! (RFC 9449).
//!
//! The wallet binds the inbound DPoP proof to both the request and the access
//! token. Beyond the standard `htm`/`htu`/`iat` checks it requires two extra
//! bindings the facilitator's DPoP check does not (the facilitator has no
//! access token to bind to):
//!
//! 1. The proof key's RFC 7638 thumbprint must equal the access token's
//!    `cnf.jkt` (proof of possession of the bound key).
//! 2. The proof's `ath` claim must equal the base64url SHA-256 of the presented
//!    access token, so a stolen proof cannot be paired with a different token.
//!
//! Anti-replay of the proof `jti` is the runtime's responsibility: a
//! short-window store distinct from the single-use access-token `jti` ledger.
//! This module returns the verified `jti` for the caller to record.
//!
//! RFC 7638 thumbprint derivation and RFC 9449 URL canonicalization come from
//! `zpay-dpop`, so the wallet and facilitator use one deterministic `jkt` and
//! `htu` implementation.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zpay_dpop::canonicalize_http_url;

pub use zpay_dpop::compute_ec_jwk_thumbprint as ec_jwk_thumbprint;

use crate::error::{ProblemDetail, ProblemKind};

/// Default clock-skew tolerance for the DPoP `iat` claim, in seconds.
pub const DPOP_CLOCK_SKEW_SECONDS: u64 = 60;

/// Claims body of a DPoP proof per RFC 9449. Only the fields the wallet checks
/// are deserialized.
#[derive(Debug, Deserialize)]
struct DpopClaims {
    htm: String,
    htu: String,
    jti: String,
    iat: i64,
    ath: String,
}

/// What the wallet pins an inbound DPoP proof against.
///
/// Bundles the request and access-token bindings so the verifier signature
/// stays within the project's argument-count budget and reads as one contract.
#[derive(Debug, Clone, Copy)]
pub struct DpopBinding<'a> {
    /// Expected HTTP method, compared byte-exact against the proof `htm`.
    pub method: &'a str,
    /// Wallet endpoint URL, compared against the proof `htu` (canonicalized).
    pub request_url: &'a str,
    /// The `at+jwt` the proof accompanies; its SHA-256 must equal the proof `ath`.
    pub access_token: &'a str,
    /// The access token's `cnf.jkt`; the proof key thumbprint must equal it.
    pub bound_jkt: &'a str,
}

/// Successful DPoP verification result.
#[derive(Debug, Clone)]
pub struct VerifiedDpop {
    /// The proof's `jti`. The runtime records it in a short-window store to
    /// reject a replayed proof within the `iat` skew window.
    pub jti: String,
    /// RFC 7638 thumbprint of the proof key. Equal to the access token's
    /// `cnf.jkt` (verified here), and threaded onward for telemetry.
    pub jkt: String,
}

/// Verify a DPoP proof bound to an access token.
///
/// `now_unix` is the verifier's wall clock in epoch seconds; `skew_seconds` is
/// the tolerated `iat` drift. The request and token bindings travel in
/// `binding`.
///
/// # Errors
///
/// Returns a [`ProblemDetail`] with [`ProblemKind::DpopProofInvalid`] for any
/// structural failure, a thumbprint that does not match `binding.bound_jkt`, a
/// bad signature, an `htm`/`htu`/`ath` mismatch, or an `iat` beyond
/// `skew_seconds`.
pub fn verify_dpop_proof(
    proof_jwt: &str,
    binding: &DpopBinding<'_>,
    now_unix: i64,
    skew_seconds: u64,
) -> Result<VerifiedDpop, ProblemDetail> {
    let header = decode_header(proof_jwt)
        .map_err(|err| dpop_invalid(format!("header decode failed: {err}")))?;
    if header.typ.as_deref() != Some("dpop+jwt") {
        return Err(dpop_invalid("typ must be dpop+jwt".to_owned()));
    }
    if header.alg != Algorithm::ES256 {
        return Err(dpop_invalid(format!(
            "alg must be ES256, got {:?}",
            header.alg
        )));
    }

    let jwk = header
        .jwk
        .ok_or_else(|| dpop_invalid("proof header carries no jwk".to_owned()))?;
    let AlgorithmParameters::EllipticCurve(ec) = &jwk.algorithm else {
        return Err(dpop_invalid("proof jwk must be an EC key".to_owned()));
    };
    if ec.curve != EllipticCurve::P256 {
        return Err(dpop_invalid("proof jwk curve must be P-256".to_owned()));
    }

    // Proof of possession: the proof key must be the key the access token was
    // bound to. Compared before signature work so a key swap is rejected early.
    let jkt = ec_jwk_thumbprint("P-256", "EC", &ec.x, &ec.y);
    if jkt != binding.bound_jkt {
        return Err(dpop_invalid(
            "proof key thumbprint does not match the access token cnf.jkt".to_owned(),
        ));
    }

    let decoding_key = DecodingKey::from_ec_components(&ec.x, &ec.y)
        .map_err(|err| dpop_invalid(format!("jwk components rejected: {err}")))?;
    let mut validation = Validation::new(Algorithm::ES256);
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation.validate_aud = false;
    let claims = decode::<DpopClaims>(proof_jwt, &decoding_key, &validation)
        .map_err(|err| dpop_invalid(format!("signature verification failed: {err}")))?
        .claims;

    if claims.htm != binding.method {
        return Err(dpop_invalid(format!(
            "htm mismatch: expected {}, got {}",
            binding.method, claims.htm
        )));
    }
    if canonicalize_url(&claims.htu)? != canonicalize_url(binding.request_url)? {
        return Err(dpop_invalid("htu mismatch".to_owned()));
    }

    let drift = abs_drift(claims.iat, now_unix);
    if drift > skew_seconds {
        return Err(dpop_invalid(format!(
            "iat skew {drift}s exceeds {skew_seconds}s tolerance"
        )));
    }

    // ath binds this proof to this exact access token.
    let expected_ath = URL_SAFE_NO_PAD.encode(Sha256::digest(binding.access_token.as_bytes()));
    if claims.ath != expected_ath {
        return Err(dpop_invalid(
            "ath does not match the presented access token".to_owned(),
        ));
    }

    Ok(VerifiedDpop {
        jti: claims.jti,
        jkt,
    })
}

fn dpop_invalid(detail: String) -> ProblemDetail {
    ProblemDetail::not_retryable(ProblemKind::DpopProofInvalid, "dpop_proof_invalid", detail)
}

fn abs_drift(iat_claim: i64, now_secs: i64) -> u64 {
    iat_claim
        .saturating_sub(now_secs)
        .unsigned_abs()
        .max(now_secs.saturating_sub(iat_claim).unsigned_abs())
}

/// Canonicalize a URL for `htu` comparison: lower-case scheme/host, strip
/// default ports, resolve dot segments, drop query and fragment.
fn canonicalize_url(raw: &str) -> Result<String, ProblemDetail> {
    canonicalize_http_url(raw).map_err(|err| dpop_invalid(err.to_string()))
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::{EncodePrivateKey as _, LineEnding};
    use rand_core::OsRng;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    use super::{
        DPOP_CLOCK_SKEW_SECONDS, DpopBinding, ProblemKind, VerifiedDpop, ec_jwk_thumbprint,
        verify_dpop_proof,
    };

    type TestResult = Result<(), Box<dyn core::error::Error>>;

    const METHOD: &str = "POST";
    const URL: &str = "https://wallet.test/v1/payments/sign";
    const ACCESS_TOKEN: &str = "header.payload.signature";
    const JTI: &str = "01DPOPJTI0000000000000000";
    const NOW: i64 = 1_900_000_000;

    struct Proof {
        jwt: String,
        jkt: String,
    }

    fn mint(
        method: &str,
        htu: &str,
        access_token: &str,
        iat: i64,
    ) -> Result<Proof, Box<dyn core::error::Error>> {
        let signing_key = SigningKey::random(&mut OsRng);
        let point = signing_key.verifying_key().to_encoded_point(false);
        let x = URL_SAFE_NO_PAD.encode(point.x().ok_or("no x coordinate")?);
        let y = URL_SAFE_NO_PAD.encode(point.y().ok_or("no y coordinate")?);

        let mut header = Header::new(Algorithm::ES256);
        header.typ = Some("dpop+jwt".to_owned());
        header.jwk = Some(serde_json::from_value(json!({
            "kty": "EC", "crv": "P-256", "x": x, "y": y,
        }))?);

        let ath = URL_SAFE_NO_PAD.encode(Sha256::digest(access_token.as_bytes()));
        let claims = json!({
            "htm": method,
            "htu": htu,
            "jti": JTI,
            "iat": iat,
            "ath": ath,
        });
        let pem = signing_key.to_pkcs8_pem(LineEnding::LF)?.to_string();
        let encoding = EncodingKey::from_ec_pem(pem.as_bytes())?;
        let jwt = encode(&header, &claims, &encoding)?;
        let jkt = ec_jwk_thumbprint("P-256", "EC", &x, &y);
        Ok(Proof { jwt, jkt })
    }

    /// Verify `proof` against the canonical `POST`/wallet-URL binding, varying
    /// only the access token and the bound thumbprint per test.
    fn verify(
        proof: &Proof,
        access_token: &str,
        bound_jkt: &str,
    ) -> Result<VerifiedDpop, super::ProblemDetail> {
        verify_dpop_proof(
            &proof.jwt,
            &DpopBinding {
                method: METHOD,
                request_url: URL,
                access_token,
                bound_jkt,
            },
            NOW,
            DPOP_CLOCK_SKEW_SECONDS,
        )
    }

    #[test]
    fn accepts_a_proof_bound_to_the_token() -> TestResult {
        let proof = mint(METHOD, URL, ACCESS_TOKEN, NOW)?;
        let verified = verify(&proof, ACCESS_TOKEN, &proof.jkt)?;
        assert_eq!(verified.jkt, proof.jkt);
        assert_eq!(verified.jti, JTI);
        Ok(())
    }

    #[test]
    fn rejects_thumbprint_not_matching_cnf_jkt() -> TestResult {
        let proof = mint(METHOD, URL, ACCESS_TOKEN, NOW)?;
        assert!(matches!(
            verify(&proof, ACCESS_TOKEN, "a-different-bound-jkt"),
            Err(err) if err.kind == ProblemKind::DpopProofInvalid
        ));
        Ok(())
    }

    #[test]
    fn rejects_ath_for_a_different_access_token() -> TestResult {
        let proof = mint(METHOD, URL, ACCESS_TOKEN, NOW)?;
        assert!(matches!(
            verify(&proof, "a.different.token", &proof.jkt),
            Err(err) if err.kind == ProblemKind::DpopProofInvalid
        ));
        Ok(())
    }

    #[test]
    fn rejects_htm_mismatch() -> TestResult {
        let proof = mint("GET", URL, ACCESS_TOKEN, NOW)?;
        assert!(matches!(
            verify(&proof, ACCESS_TOKEN, &proof.jkt),
            Err(err) if err.kind == ProblemKind::DpopProofInvalid
        ));
        Ok(())
    }

    #[test]
    fn rejects_htu_mismatch() -> TestResult {
        let proof = mint(METHOD, "https://wallet.test/v1/holds", ACCESS_TOKEN, NOW)?;
        assert!(matches!(
            verify(&proof, ACCESS_TOKEN, &proof.jkt),
            Err(err) if err.kind == ProblemKind::DpopProofInvalid
        ));
        Ok(())
    }

    #[test]
    fn rejects_iat_beyond_skew() -> TestResult {
        let stale = NOW - i64::try_from(DPOP_CLOCK_SKEW_SECONDS + 60).unwrap_or(0);
        let proof = mint(METHOD, URL, ACCESS_TOKEN, stale)?;
        assert!(matches!(
            verify(&proof, ACCESS_TOKEN, &proof.jkt),
            Err(err) if err.kind == ProblemKind::DpopProofInvalid
        ));
        Ok(())
    }

    #[test]
    fn accepts_default_port_difference_in_htu() -> TestResult {
        let proof = mint(
            METHOD,
            "https://wallet.test:443/v1/payments/sign",
            ACCESS_TOKEN,
            NOW,
        )?;
        let verified = verify(&proof, ACCESS_TOKEN, &proof.jkt)?;
        assert_eq!(verified.jkt, proof.jkt);
        Ok(())
    }
}
