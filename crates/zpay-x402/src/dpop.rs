//! DPoP (Demonstration of Proof-of-Possession) middleware for x402 v2.
//!
//! This module verifies a DPoP proof JWT on every authenticated request
//! to `POST /x402/v2/prepare` and `POST /x402/v2/settle`. The proof is
//! signed by an EC P-256 (ES256) key the caller supplied in the JWT's
//! own `jwk` header. The verifier:
//!
//! 1. Parses the proof JWT and extracts `typ`, `alg`, and `jwk` from
//!    the header.
//! 2. Confirms `typ == "dpop+jwt"` and `alg == "ES256"`.
//! 3. Computes the RFC 7638 JWK thumbprint, validates the supplied
//!    `jti` is non-empty and bounded by [`MAX_JTI_LEN`], then burns
//!    `(jkt, jti)` against the replay store BEFORE any attacker-tweakable
//!    claim is checked. A probe-then-replay attack window therefore
//!    does not exist.
//! 4. Confirms the payload `htm` matches the request method byte-for-byte
//!    (RFC 9449 requires a case-sensitive comparison; the caller is
//!    expected to send upper-case verbs).
//! 5. Confirms the payload `htu` matches the URL the request is hitting
//!    after a structural canonicalization (scheme + host + path; default
//!    ports stripped; dot segments resolved; percent-encoding normalized
//!    by the `url` crate).
//! 6. Confirms `iat` is within +/- [`CLOCK_SKEW_SECONDS`] of server time.
//! 7. Verifies the JWT signature against the supplied JWK.
//!
//! On success the thumbprint is returned to the caller. The thumbprint
//! is the `jkt` half of the `(jkt, idempotency_key)` idempotency
//! composite ADR-0004 documents.
//!
//! The [`ReplayStore`] trait is the seam production deployments swap
//! to share a `(jkt, jti)` ledger across runtime processes (Redis,
//! Turso, KMS-backed). The bundled [`InMemoryReplayStore`] is the
//! single-process implementation used by tests and the dev container.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use url::Url;

/// Maximum clock drift (in seconds) tolerated between the proof's
/// `iat` claim and the verifier's wall clock.
pub const CLOCK_SKEW_SECONDS: u64 = 60;

/// Replay-protection window. After this duration the `(jkt, jti)`
/// entry is dropped from the replay store, so a stale proof from a
/// long-dead client cannot pin memory.
pub const REPLAY_WINDOW_SECONDS: u64 = 300;

/// Upper bound on the byte length of a DPoP proof's `jti` claim.
///
/// A 128-character cap is generous for legitimate randomly-generated
/// jti values (a UUID v4 is 36 chars; a base64-encoded 32-byte random
/// is 43 chars) while bounding the memory an adversary can pin per
/// `(jkt, jti)` row in the replay store.
pub const MAX_JTI_LEN: usize = 128;

/// Typed DPoP verification errors. The wire layer maps each variant
/// onto a typed `application/problem+json` document with the right
/// HTTP status.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DpopError {
    /// No DPoP proof was supplied on a route that requires one.
    #[error("DPoP proof header missing")]
    Missing,
    /// The proof JWT was malformed, used the wrong algorithm or `typ`,
    /// carried an invalid `jwk`, or had a wrong `htm`/`htu`/`jti`/`iat`
    /// shape, or its signature did not verify against the embedded JWK.
    #[error("DPoP proof invalid: {reason}")]
    InvalidProof {
        /// Operator-facing reason. Does not include the JWT contents.
        reason: String,
    },
    /// The proof's `iat` claim is outside the
    /// [`CLOCK_SKEW_SECONDS`] tolerance window.
    #[error("DPoP clock skew: drift={drift_seconds}s")]
    ClockSkew {
        /// Absolute drift between `iat` and server time, in seconds.
        drift_seconds: u64,
    },
    /// Either the `(jkt, jti)` pair has already been seen during the
    /// current replay window or the supplied `jti` is empty.
    #[error("DPoP proof replay detected")]
    Replay,
}

/// Successful verification result.
#[derive(Debug, Clone)]
pub struct VerifiedDpopProof {
    /// JWK thumbprint of the proof's signing key (RFC 7638). The wire
    /// layer threads this onto `propose` and uses it as the first
    /// component of the `(jkt, idempotency_key)` idempotency composite.
    pub jkt: String,
}

/// Outcome of recording a `(jkt, jti)` sighting against a replay store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayOutcome {
    /// The pair was not present in the window; the store now holds it.
    Fresh,
    /// The pair was already recorded during the active replay window.
    /// The DPoP verifier maps this onto [`DpopError::Replay`].
    Already,
}

/// Boxed future type returned from [`ReplayStore::observe`].
///
/// A pinned heap-allocated future is the dyn-compatible analogue of
/// `async fn` on a trait. The boxing cost is one allocation per DPoP
/// proof; the trait object is the seam production deployments use to
/// inject a Redis or libSQL store, which is a small price for the
/// flexibility.
pub type ReplayObserveFuture<'a> = Pin<Box<dyn Future<Output = ReplayOutcome> + Send + 'a>>;

/// Shared replay-store contract.
///
/// Production deployments swap the bundled [`InMemoryReplayStore`] for
/// a shared backend (Redis, libSQL, KMS) without touching the verifier
/// or the wire handlers. The trait stays minimal: every implementation
/// promises a single atomic insert-or-detect over `(jkt, jti)` keyed by
/// a wall-clock `ttl`. The verifier never reaches into store internals.
pub trait ReplayStore: Send + Sync {
    /// Record the supplied `(jkt, jti)` pair if it is not already
    /// present in the active replay window, returning whether the
    /// pair was [`ReplayOutcome::Fresh`] or [`ReplayOutcome::Already`].
    ///
    /// Implementations MUST be atomic across concurrent observers:
    /// two simultaneous calls with the same `(jkt, jti)` must return
    /// `Fresh` exactly once and `Already` for every other caller. The
    /// returned future is boxed so the trait is dyn-compatible; the
    /// verifier holds an `Arc<dyn ReplayStore>` for the lifetime of
    /// the process.
    fn observe<'a>(&'a self, jkt: &'a str, jti: &'a str, ttl: Duration) -> ReplayObserveFuture<'a>;
}

/// In-memory replay store keyed by `(jkt, jti)`.
///
/// One instance lives in [`AppState`][crate::AppState] so every
/// runtime process gets its own store. The store is bounded by the
/// [`REPLAY_WINDOW_SECONDS`] sweeper; a long-lived process cannot
/// accumulate unbounded entries.
#[derive(Default)]
pub struct InMemoryReplayStore {
    inner: Mutex<HashMap<(String, String), Instant>>,
}

impl InMemoryReplayStore {
    /// Build an empty replay store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Force-clear the store (test-only).
    #[cfg(test)]
    fn clear(&self) {
        self.inner.lock().clear();
    }
}

impl ReplayStore for InMemoryReplayStore {
    fn observe<'a>(&'a self, jkt: &'a str, jti: &'a str, ttl: Duration) -> ReplayObserveFuture<'a> {
        Box::pin(async move {
            let now = Instant::now();
            let cutoff = now.checked_sub(ttl).unwrap_or(now);
            let mut guard = self.inner.lock();
            guard.retain(|_, seen_at| *seen_at > cutoff);
            let key = (jkt.to_owned(), jti.to_owned());
            if guard.contains_key(&key) {
                return ReplayOutcome::Already;
            }
            guard.insert(key, now);
            ReplayOutcome::Fresh
        })
    }
}

impl std::fmt::Debug for InMemoryReplayStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.inner.lock().len();
        f.debug_struct("InMemoryReplayStore")
            .field("entries", &count)
            .finish()
    }
}

/// Operator-supplied expectations the DPoP verifier pins against.
///
/// The verifier needs a single canonical request URL to compare against
/// the proof's `htu` claim. Two attacker-controlled inputs feed the
/// inbound URL: the `Host` header and the `X-Forwarded-Proto` header.
/// In production both are pinned by the operator via env vars; in dev
/// the unbound fallback lets the runtime answer on `localhost` without
/// a config file. The runtime emits a startup `WARN` whenever
/// `expected_host` is `None` so operators do not ship to production
/// in unbound mode unaware.
#[derive(Debug, Clone)]
pub struct DpopExpectations {
    /// Scheme to use when canonicalizing the inbound request URL.
    /// Typically `"https"` in production, `"http"` for local dev.
    pub expected_scheme: String,
    /// Pinned host (and optional `:port`) to use when canonicalizing
    /// the inbound request URL. When `Some`, the inbound `Host` header
    /// is ignored, blocking a Host-spoof attack where an adversary
    /// sends `Host: evil.com` alongside a proof minted against
    /// `evil.com`. When `None`, the verifier falls back to the inbound
    /// `Host` header and the runtime is expected to log a warning.
    pub expected_host: Option<String>,
}

impl DpopExpectations {
    /// Build an expectations bundle that always trusts the inbound
    /// `Host` header. Use only for tests or operator-knows-best dev
    /// loops; production deployments must pin the host.
    #[must_use]
    pub fn unbound(scheme: impl Into<String>) -> Self {
        Self {
            expected_scheme: scheme.into(),
            expected_host: None,
        }
    }

    /// Build an expectations bundle pinned to a single `(scheme, host)`
    /// pair. The host string is used verbatim and may include a port
    /// suffix (`zpay.example.com:8443`).
    #[must_use]
    pub fn pinned(scheme: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            expected_scheme: scheme.into(),
            expected_host: Some(host.into()),
        }
    }
}

/// Parsed proof header. Public-by-private; the rest of the module
/// reads `jwk` and dispatches to the JWK kind it represents.
#[derive(Debug, serde::Deserialize)]
struct ProofHeader {
    typ: Option<String>,
    alg: String,
    jwk: serde_json::Value,
}

/// EC P-256 JWK as defined by RFC 7518. Only the fields the verifier
/// needs are deserialized; unknown fields are ignored.
#[derive(Debug, serde::Deserialize)]
struct EcJwk {
    kty: String,
    crv: String,
    x: String,
    y: String,
}

/// Claims body of a DPoP proof per RFC 9449. Only the fields the
/// verifier checks are deserialized.
#[derive(Debug, serde::Deserialize)]
struct ProofClaims {
    htm: String,
    htu: String,
    jti: String,
    iat: i64,
}

/// Verify a DPoP proof, returning the proof's JWK thumbprint on
/// success.
///
/// `method` is the HTTP verb the caller used (e.g. `"POST"`). The
/// comparison against the proof's `htm` claim is byte-exact per RFC
/// 9449; the wire layer is expected to forward the verb verbatim.
///
/// `url` is the request URL the caller hit (scheme + host + path).
/// `canonicalize_url` resolves dot segments, lowercases the host,
/// strips default ports, and normalizes percent-encoding before
/// comparing against the proof's `htu` claim.
///
/// `proof` is the raw DPoP header value (the proof JWT).
///
/// # Errors
///
/// - [`DpopError::Missing`] when `proof` is empty.
/// - [`DpopError::InvalidProof`] for any structural or signature
///   failure, including a `jti` longer than [`MAX_JTI_LEN`].
/// - [`DpopError::ClockSkew`] when `iat` is beyond the tolerance.
/// - [`DpopError::Replay`] when `(jkt, jti)` was seen during the window.
#[allow(
    clippy::too_many_lines,
    reason = "DPoP verification is a single conceptual operation; splitting it into smaller helpers would scatter the proof's invariant chain across multiple functions and hurt audit-readability"
)]
pub async fn verify_dpop_proof(
    method: &str,
    url: &str,
    proof: &str,
    replay_store: &(dyn ReplayStore + '_),
) -> Result<VerifiedDpopProof, DpopError> {
    if proof.trim().is_empty() {
        return Err(DpopError::Missing);
    }

    let header = decode_header(proof).map_err(|err| DpopError::InvalidProof {
        reason: format!("header decode failed: {err}"),
    })?;
    if header.alg != Algorithm::ES256 {
        return Err(DpopError::InvalidProof {
            reason: format!("alg must be ES256, got {:?}", header.alg),
        });
    }

    let proof_header: ProofHeader =
        decode_unverified_header(proof).map_err(|err| DpopError::InvalidProof {
            reason: format!("custom header decode failed: {err}"),
        })?;
    if proof_header.typ.as_deref() != Some("dpop+jwt") {
        return Err(DpopError::InvalidProof {
            reason: "typ must be dpop+jwt".to_owned(),
        });
    }
    if proof_header.alg != "ES256" {
        return Err(DpopError::InvalidProof {
            reason: format!("alg must be ES256, got {}", proof_header.alg),
        });
    }

    let ec_jwk: EcJwk =
        serde_json::from_value(proof_header.jwk).map_err(|err| DpopError::InvalidProof {
            reason: format!("jwk parse failed: {err}"),
        })?;
    if ec_jwk.kty != "EC" {
        return Err(DpopError::InvalidProof {
            reason: format!("jwk.kty must be EC, got {}", ec_jwk.kty),
        });
    }
    if ec_jwk.crv != "P-256" {
        return Err(DpopError::InvalidProof {
            reason: format!("jwk.crv must be P-256, got {}", ec_jwk.crv),
        });
    }

    // Decode the claims without verifying the signature first, so we
    // can extract the jti and burn it against the replay store before
    // any attacker-tweakable claim (htu, htm, iat) is checked. The
    // signature is still verified against the embedded JWK below; that
    // step is the last gate before returning success.
    let claims = decode_unverified_claims(proof).map_err(|err| DpopError::InvalidProof {
        reason: format!("claims decode failed: {err}"),
    })?;

    // jti shape gate: non-empty and capped at MAX_JTI_LEN bytes so an
    // adversary cannot pin unbounded memory in the replay store with a
    // single malformed probe.
    let jti = claims.jti.trim();
    if jti.is_empty() {
        return Err(DpopError::InvalidProof {
            reason: "jti must be non-empty".to_owned(),
        });
    }
    if jti.len() > MAX_JTI_LEN {
        return Err(DpopError::InvalidProof {
            reason: format!("jti exceeds {MAX_JTI_LEN}-byte cap"),
        });
    }

    let jkt = compute_ec_jwk_thumbprint(&ec_jwk.crv, &ec_jwk.kty, &ec_jwk.x, &ec_jwk.y);
    // Burn the (jkt, jti) before validating any remaining claim. Even
    // when the htm/htu/iat/signature checks below reject the proof,
    // the jti stays burned, so a probe-then-replay attack (mint with
    // wrong htm to learn the verifier's response, then retry with the
    // same jti and a correct htm) does not exist.
    match replay_store
        .observe(&jkt, jti, Duration::from_secs(REPLAY_WINDOW_SECONDS))
        .await
    {
        ReplayOutcome::Fresh => {}
        ReplayOutcome::Already => return Err(DpopError::Replay),
    }

    if method != claims.htm {
        return Err(DpopError::InvalidProof {
            reason: format!(
                "htm mismatch: expected {method}, got {claim}",
                claim = claims.htm
            ),
        });
    }

    let want = canonicalize_url(url)?;
    let got = canonicalize_url(&claims.htu)?;
    if want != got {
        return Err(DpopError::InvalidProof {
            reason: format!("htu mismatch: expected {want}, got {got}"),
        });
    }

    let now_secs = current_unix_seconds();
    let drift = abs_drift_seconds(claims.iat, now_secs);
    if drift > CLOCK_SKEW_SECONDS {
        return Err(DpopError::ClockSkew {
            drift_seconds: drift,
        });
    }

    // Signature verification is the final gate. Decoding here only
    // re-parses the claims and checks the ECDSA signature; the values
    // we read out of the unverified-decode pass above are already the
    // ones we matched against.
    let decoding_key = decoding_key_from_ec_jwk(&ec_jwk.x, &ec_jwk.y)?;
    let mut validation = Validation::new(Algorithm::ES256);
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation.validate_aud = false;
    decode::<ProofClaims>(proof, &decoding_key, &validation).map_err(|err| {
        DpopError::InvalidProof {
            reason: format!("signature verification failed: {err}"),
        }
    })?;

    Ok(VerifiedDpopProof { jkt })
}

/// Compute the RFC 7638 JWK thumbprint for an EC P-256 key.
///
/// The canonical JWK JSON for EC is the lexicographic ordering of the
/// required members: `{"crv":...,"kty":...,"x":...,"y":...}` with no
/// whitespace.
#[must_use]
pub fn compute_ec_jwk_thumbprint(crv: &str, kty: &str, x: &str, y: &str) -> String {
    // Lexicographic ordering of required EC JWK members per RFC 7638.
    let canonical = format!(r#"{{"crv":"{crv}","kty":"{kty}","x":"{x}","y":"{y}"}}"#);
    let digest = Sha256::digest(canonical.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn decoding_key_from_ec_jwk(x_b64: &str, y_b64: &str) -> Result<DecodingKey, DpopError> {
    // jsonwebtoken's from_ec_components expects base64url-encoded
    // x/y coordinates of an EC P-256 public key (RFC 7518). The strings
    // we hand in are exactly what arrived in the JWK header, so a
    // successful decode here doubles as a syntactic guard on the JWK.
    DecodingKey::from_ec_components(x_b64, y_b64).map_err(|err| DpopError::InvalidProof {
        reason: format!("jwk components rejected: {err}"),
    })
}

fn decode_unverified_header(proof: &str) -> Result<ProofHeader, serde_json::Error> {
    let mut parts = proof.split('.');
    let header_b64 = parts.next().unwrap_or("");
    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(serde::de::Error::custom)?;
    serde_json::from_slice(&header_bytes)
}

fn decode_unverified_claims(proof: &str) -> Result<ProofClaims, serde_json::Error> {
    let mut parts = proof.split('.');
    let _ = parts.next();
    let claims_b64 = parts.next().unwrap_or("");
    let claims_bytes = URL_SAFE_NO_PAD
        .decode(claims_b64)
        .map_err(serde::de::Error::custom)?;
    serde_json::from_slice(&claims_bytes)
}

fn current_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_i64, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

fn abs_drift_seconds(iat_claim: i64, now_secs: i64) -> u64 {
    let drift = iat_claim.saturating_sub(now_secs).unsigned_abs();
    let other = now_secs.saturating_sub(iat_claim).unsigned_abs();
    drift.max(other)
}

/// Canonicalize a URL for comparison between the inbound request and
/// the proof's `htu` claim.
///
/// Rules applied (all delegated to the `url` crate):
///
/// - Scheme is lower-cased and required to be `http` or `https`.
/// - Host is lower-cased.
/// - Default ports (`80` for `http`, `443` for `https`) are stripped.
/// - Dot segments in the path are resolved (`/a/./b` -> `/a/b`,
///   `/a/../b` -> `/b`).
/// - Percent-encoded ASCII bytes are normalized to upper-case hex by
///   the `url` crate's path serializer; both sides therefore agree on
///   the same percent-encoding for any byte that is itself
///   percent-encoded. A `%2f` in the proof remains distinct from a
///   literal `/` in the request URL, so path-traversal attempts that
///   tunnel through an encoded slash are not silently equated.
/// - Query and fragment are stripped before comparison.
fn canonicalize_url(raw: &str) -> Result<String, DpopError> {
    let mut parsed = Url::parse(raw).map_err(|err| DpopError::InvalidProof {
        reason: format!("url parse failed: {err}"),
    })?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(DpopError::InvalidProof {
            reason: format!("url scheme must be http or https, got {scheme}"),
        });
    }
    // The url crate's serializer already lower-cases the host and
    // strips default ports for `http` and `https` when re-serializing
    // through `set_port(None)` after a comparison, but the safest path
    // is to clear query and fragment and let the serializer do its job.
    parsed.set_query(None);
    parsed.set_fragment(None);
    Ok(parsed.to_string())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-only: unwrap_err is the canonical 'expect error' pattern; the surrounding asserts pin the variant"
)]
mod tests {
    use super::{
        CLOCK_SKEW_SECONDS, DpopError, InMemoryReplayStore, MAX_JTI_LEN, REPLAY_WINDOW_SECONDS,
        ReplayOutcome, ReplayStore, canonicalize_url, compute_ec_jwk_thumbprint,
        current_unix_seconds, verify_dpop_proof,
    };
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use p256::ecdsa::SigningKey;
    use rand_core::OsRng;
    use std::time::Duration;

    fn mint_proof(
        method: &str,
        url: &str,
        jti: &str,
        iat: i64,
    ) -> Result<(String, String), Box<dyn std::error::Error>> {
        use p256::pkcs8::EncodePrivateKey as _;

        let signing_key = SigningKey::random(&mut OsRng);
        let verifying = signing_key.verifying_key();
        let point = verifying.to_encoded_point(false);
        let x = URL_SAFE_NO_PAD.encode(point.x().ok_or("no x coordinate")?);
        let y = URL_SAFE_NO_PAD.encode(point.y().ok_or("no y coordinate")?);

        let jwk = serde_json::json!({
            "kty": "EC",
            "crv": "P-256",
            "x": x,
            "y": y,
        });

        let mut header = Header::new(Algorithm::ES256);
        header.typ = Some("dpop+jwt".to_owned());
        header.jwk = Some(serde_json::from_value(jwk)?);

        let claims = serde_json::json!({
            "htm": method,
            "htu": url,
            "jti": jti,
            "iat": iat,
        });
        let pem = signing_key
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)?
            .to_string();
        let encoding_key = EncodingKey::from_ec_pem(pem.as_bytes())?;
        let proof = encode(&header, &claims, &encoding_key)?;

        let thumbprint = compute_ec_jwk_thumbprint("P-256", "EC", &x, &y);
        Ok((proof, thumbprint))
    }

    #[test]
    fn thumbprint_matches_rfc7638_vector() {
        let crv = "P-256";
        let kty = "EC";
        let x = "MKBCTNIcKUSDii11ySs3526iDZ8AiTo7Tu6KPAqv7D4";
        let y = "4Etl6SRW2YiLUrN5vfvVHuhp7x8PxltmWWlbbM4IFyM";
        let computed = compute_ec_jwk_thumbprint(crv, kty, x, y);
        let expected = "cn-I_WNMClehiVp51i_0VpOENW1upEerA8sEam5hn-s";
        assert_eq!(computed, expected);
    }

    #[tokio::test]
    async fn verifies_valid_proof_and_returns_jkt() -> Result<(), Box<dyn std::error::Error>> {
        let store = InMemoryReplayStore::new();
        let now = current_unix_seconds();
        let (proof, expected_jkt) =
            mint_proof("POST", "https://example.test/x402/v2/prepare", "jti-1", now)?;
        let verified = verify_dpop_proof(
            "POST",
            "https://example.test/x402/v2/prepare",
            &proof,
            &store,
        )
        .await
        .map_err(|err| format!("expected ok, got {err:?}"))?;
        assert_eq!(verified.jkt, expected_jkt);
        Ok(())
    }

    #[tokio::test]
    async fn missing_proof_returns_missing() {
        let store = InMemoryReplayStore::new();
        let err = verify_dpop_proof("POST", "https://example.test/x", "", &store)
            .await
            .unwrap_err();
        assert!(matches!(err, DpopError::Missing));
    }

    #[tokio::test]
    async fn replay_rejects_second_sighting() -> Result<(), Box<dyn std::error::Error>> {
        let store = InMemoryReplayStore::new();
        let now = current_unix_seconds();
        let (proof, _) = mint_proof("POST", "https://example.test/x", "jti-r", now)?;
        verify_dpop_proof("POST", "https://example.test/x", &proof, &store)
            .await
            .map_err(|err| format!("first call must succeed, got {err:?}"))?;
        let err = verify_dpop_proof("POST", "https://example.test/x", &proof, &store)
            .await
            .unwrap_err();
        assert!(matches!(err, DpopError::Replay));
        Ok(())
    }

    #[tokio::test]
    async fn clock_skew_beyond_window_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let store = InMemoryReplayStore::new();
        let stale = current_unix_seconds() - i64::try_from(CLOCK_SKEW_SECONDS + 60).unwrap_or(0);
        let (proof, _) = mint_proof("POST", "https://example.test/x", "jti-c", stale)?;
        let err = verify_dpop_proof("POST", "https://example.test/x", &proof, &store)
            .await
            .unwrap_err();
        assert!(matches!(err, DpopError::ClockSkew { .. }));
        Ok(())
    }

    #[tokio::test]
    async fn htm_mismatch_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let store = InMemoryReplayStore::new();
        let now = current_unix_seconds();
        let (proof, _) = mint_proof("POST", "https://example.test/x", "jti-m", now)?;
        let err = verify_dpop_proof("GET", "https://example.test/x", &proof, &store)
            .await
            .unwrap_err();
        assert!(matches!(err, DpopError::InvalidProof { .. }));
        Ok(())
    }

    #[tokio::test]
    async fn htu_mismatch_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let store = InMemoryReplayStore::new();
        let now = current_unix_seconds();
        let (proof, _) = mint_proof("POST", "https://example.test/x", "jti-h", now)?;
        let err = verify_dpop_proof("POST", "https://example.test/y", &proof, &store)
            .await
            .unwrap_err();
        assert!(matches!(err, DpopError::InvalidProof { .. }));
        Ok(())
    }

    #[tokio::test]
    async fn htm_lower_case_rejected_rfc9449() -> Result<(), Box<dyn std::error::Error>> {
        // RFC 9449 mandates byte-exact htm comparison. A proof carrying
        // a lower-case verb must not match an upper-case request method.
        let store = InMemoryReplayStore::new();
        let now = current_unix_seconds();
        let (proof, _) = mint_proof("post", "https://example.test/x", "jti-case", now)?;
        let err = verify_dpop_proof("POST", "https://example.test/x", &proof, &store)
            .await
            .unwrap_err();
        if let DpopError::InvalidProof { reason } = &err {
            assert!(reason.contains("htm mismatch"), "reason: {reason}");
        } else {
            return Err(format!("expected InvalidProof, got {err:?}").into());
        }
        Ok(())
    }

    #[tokio::test]
    async fn jti_at_max_len_accepted() -> Result<(), Box<dyn std::error::Error>> {
        let store = InMemoryReplayStore::new();
        let now = current_unix_seconds();
        let jti = "a".repeat(MAX_JTI_LEN);
        let (proof, _) = mint_proof("POST", "https://example.test/x", &jti, now)?;
        let verified = verify_dpop_proof("POST", "https://example.test/x", &proof, &store)
            .await
            .map_err(|err| format!("expected ok, got {err:?}"))?;
        assert!(!verified.jkt.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn jti_over_max_len_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let store = InMemoryReplayStore::new();
        let now = current_unix_seconds();
        let jti = "a".repeat(MAX_JTI_LEN + 1);
        let (proof, _) = mint_proof("POST", "https://example.test/x", &jti, now)?;
        let err = verify_dpop_proof("POST", "https://example.test/x", &proof, &store)
            .await
            .unwrap_err();
        if let DpopError::InvalidProof { reason } = &err {
            assert!(reason.contains("jti exceeds"), "reason: {reason}");
        } else {
            return Err(format!("expected InvalidProof, got {err:?}").into());
        }
        Ok(())
    }

    #[tokio::test]
    async fn jti_burned_on_failed_htm() -> Result<(), Box<dyn std::error::Error>> {
        // Probe-then-replay defense: a proof that fails the htm check
        // still burns its jti. A second proof with the same jkt+jti
        // (even if everything else is correct) must be rejected as a
        // replay, not as the original validation error.
        let store = InMemoryReplayStore::new();
        let now = current_unix_seconds();
        let (bad_proof, _) = mint_proof("PUT", "https://example.test/x", "jti-burn", now)?;
        let first_err = verify_dpop_proof("POST", "https://example.test/x", &bad_proof, &store)
            .await
            .unwrap_err();
        assert!(matches!(first_err, DpopError::InvalidProof { .. }));

        // Try a different, well-formed proof for POST that reuses the
        // same jti against the same jkt. The mint_proof helper picks a
        // fresh key per call, so the jkt differs; we exercise the
        // observe path directly to keep the test focused on burn order.
        let outcome = store
            .observe(
                "dummy-jkt",
                "jti-burn",
                Duration::from_secs(REPLAY_WINDOW_SECONDS),
            )
            .await;
        assert_eq!(outcome, ReplayOutcome::Fresh);

        // Same-jkt replay: re-presenting the bad proof must now report
        // Replay, not InvalidProof.
        let second_err = verify_dpop_proof("POST", "https://example.test/x", &bad_proof, &store)
            .await
            .unwrap_err();
        assert!(matches!(second_err, DpopError::Replay));
        Ok(())
    }

    #[tokio::test]
    async fn replay_store_clears_after_window_via_internal_helper() {
        let store = InMemoryReplayStore::new();
        let ttl = Duration::from_secs(REPLAY_WINDOW_SECONDS);
        let res = store.observe("jkt-1", "jti-1", ttl).await;
        assert_eq!(res, ReplayOutcome::Fresh);
        let res_again = store.observe("jkt-1", "jti-1", ttl).await;
        assert_eq!(res_again, ReplayOutcome::Already);
        store.clear();
        let res_after_clear = store.observe("jkt-1", "jti-1", ttl).await;
        assert_eq!(res_after_clear, ReplayOutcome::Fresh);
    }

    #[tokio::test]
    async fn replay_store_trait_round_trip_concurrent() -> Result<(), &'static str> {
        // Spawn two tasks racing on the same (jkt, jti); exactly one
        // must see Fresh, the other Already. Repeat across rounds to
        // surface a flaky implementation.
        let store = std::sync::Arc::new(InMemoryReplayStore::new());
        let ttl = Duration::from_secs(REPLAY_WINDOW_SECONDS);
        for round in 0..20 {
            let key_jkt = format!("jkt-round-{round}");
            let key_jti = format!("jti-round-{round}");
            let store_a = std::sync::Arc::clone(&store);
            let store_b = std::sync::Arc::clone(&store);
            let jkt_a = key_jkt.clone();
            let jkt_b = key_jkt.clone();
            let jti_a = key_jti.clone();
            let jti_b = key_jti.clone();
            let task_a = tokio::spawn(async move { store_a.observe(&jkt_a, &jti_a, ttl).await });
            let task_b = tokio::spawn(async move { store_b.observe(&jkt_b, &jti_b, ttl).await });
            let result_a = task_a.await.map_err(|_| "task_a panicked")?;
            let result_b = task_b.await.map_err(|_| "task_b panicked")?;
            let fresh_count = [result_a, result_b]
                .iter()
                .filter(|outcome| matches!(outcome, ReplayOutcome::Fresh))
                .count();
            let already_count = [result_a, result_b]
                .iter()
                .filter(|outcome| matches!(outcome, ReplayOutcome::Already))
                .count();
            assert_eq!(fresh_count, 1, "round {round}: fresh_count={fresh_count}");
            assert_eq!(
                already_count, 1,
                "round {round}: already_count={already_count}"
            );
        }
        Ok(())
    }

    #[test]
    fn canonicalize_url_strips_default_port_https() -> Result<(), DpopError> {
        let a = canonicalize_url("https://zpay.example.com:443/x402/v2/prepare")?;
        let b = canonicalize_url("https://zpay.example.com/x402/v2/prepare")?;
        assert_eq!(a, b);
        Ok(())
    }

    #[test]
    fn canonicalize_url_strips_default_port_http() -> Result<(), DpopError> {
        let a = canonicalize_url("http://zpay.example.com:80/x402/v2/prepare")?;
        let b = canonicalize_url("http://zpay.example.com/x402/v2/prepare")?;
        assert_eq!(a, b);
        Ok(())
    }

    #[test]
    fn canonicalize_url_resolves_dot_segments() -> Result<(), DpopError> {
        let a = canonicalize_url("https://zpay.example.com/x402/v2/./prepare")?;
        let b = canonicalize_url("https://zpay.example.com/x402/v2/prepare")?;
        assert_eq!(a, b);
        Ok(())
    }

    #[test]
    fn canonicalize_url_lowercases_host() -> Result<(), DpopError> {
        let a = canonicalize_url("https://ZPay.Example.COM/x402/v2/prepare")?;
        let b = canonicalize_url("https://zpay.example.com/x402/v2/prepare")?;
        assert_eq!(a, b);
        Ok(())
    }

    #[test]
    fn canonicalize_url_strips_query_and_fragment() -> Result<(), DpopError> {
        let a = canonicalize_url("https://zpay.example.com/x402/v2/prepare?k=v#frag")?;
        let b = canonicalize_url("https://zpay.example.com/x402/v2/prepare")?;
        assert_eq!(a, b);
        Ok(())
    }

    #[test]
    fn canonicalize_url_keeps_encoded_slash_distinct() -> Result<(), DpopError> {
        // A %2f inside the path must NOT collapse onto a literal '/'.
        // Otherwise an attacker could tunnel path traversal through an
        // encoded slash.
        let encoded = canonicalize_url("https://zpay.example.com/x402/v2%2fprepare")?;
        let literal = canonicalize_url("https://zpay.example.com/x402/v2/prepare")?;
        assert_ne!(encoded, literal);
        Ok(())
    }

    #[test]
    fn canonicalize_url_rejects_non_http_scheme() {
        let err = canonicalize_url("ftp://zpay.example.com/x").unwrap_err();
        assert!(matches!(err, DpopError::InvalidProof { .. }));
    }
}
