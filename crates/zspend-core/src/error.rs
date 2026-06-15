//! PRC-7807 problem-detail envelope returned by every error path in the
//! wallet runtime.
//!
//! Per Proposal-0003 D-12: error bodies carry `remediation` and `Retry-After`
//! guidance. The `kind` field is wire-stable; rename forces a versioned
//! migration on every consumer (the agent BFF, the demo-rp page, the MCP host).
//!
//! The runtime returns [`ProblemKind::NotReady`] for transient conditions
//! (startup, an in-flight single-use reservation, an unmapped wallet error)
//! and the specific verifier kinds (`dpop_proof_invalid`,
//! `access_token_invalid`, `audience_mismatch`, `intent_mismatch`,
//! `recipient_mismatch`, `token_already_consumed`) once a request fails a
//! boundary check. The revocation kinds (`token_revoked`,
//! `revocation_cache_stale`) come from the sign-time revocation check (D-6):
//! `token_revoked` when the access-token `jti` is in the revocation cache,
//! `revocation_cache_stale` when the cache cannot be proven fresh.

use serde::{Deserialize, Serialize};

/// Wire-stable discriminator for the kind of problem the runtime hit.
///
/// `#[non_exhaustive]` so future kinds land as additive variants without
/// breaking match exhaustiveness at downstream call sites that pattern-match
/// over the wire value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProblemKind {
    /// The submitted `payment_request` failed scheme-specific parsing or
    /// canonicalization. Not retryable: identical input always fails.
    PaymentRequestInvalid,
    /// The recomputed `intent_hash` did not match the RAR entry; the caller
    /// must request a new authorization that matches the parsed tuple.
    IntentMismatch,
    /// The access token's `aud` JWK thumbprint did not match the wallet's
    /// startup-time audience pin (D-5).
    AudienceMismatch,
    /// The `jti` was already consumed by a prior signed payload (D-8).
    TokenAlreadyConsumed,
    /// The wallet has insufficient funds to construct the proposal.
    InsufficientFunds,
    /// The sealed seed is unavailable; the runtime cannot sign until the
    /// operator restores it.
    SeedUnavailable,
    /// The chain plane is unreachable; the wallet could not consult the tip
    /// or fetch UTXOs.
    ChainUnreachable,
    /// The runtime is starting up or a precondition is not yet satisfied;
    /// retry once readiness clears.
    NotReady,
    /// The DPoP proof on the inbound request failed verification.
    DpopProofInvalid,
    /// The access token failed JWKS or claim verification.
    AccessTokenInvalid,
    /// The caller-supplied `target_expiry_height` is at or below the wallet's
    /// observed chain tip. The caller must pick a fresher height (typically
    /// from a re-issued `/prepare`) and retry. Not retryable with identical
    /// inputs; surfaced with HTTP 409.
    TargetExpiryStale,
    /// The signed transaction's `expiry_height` did not match the
    /// caller-supplied `target_expiry_height`. Indicates a bug in the
    /// wallet's PCZT proposal path; not actionable by the caller. Surfaced
    /// with HTTP 500.
    TargetExpiryMismatchInternal,
    /// The access token's `jti` appears in the wallet's revocation cache; the
    /// grant was revoked after mint (D-6). Distinct from `AccessTokenInvalid`
    /// for telemetry. Remediation: request a new authorization.
    TokenRevoked,
    /// The recipient parsed from the `payment_request` does not equal the
    /// `recipient` the issuer signed into the RAR (D-4/D-2). Distinct from
    /// `IntentMismatch` so telemetry separates a recipient swap from a
    /// general hash mismatch.
    RecipientMismatch,
    /// The amount parsed from the `payment_request` exceeds the wallet's
    /// optional local backstop cap (defense-in-depth; the issuer is the
    /// authoritative policy gate per D-2).
    AmountExceeded,
    /// The RAR `expires_at` has passed; the authorization is no longer
    /// spendable. Surfaced with HTTP 410.
    AuthorizationExpired,
    /// `authorization_details` did not carry exactly one entry; v1 accepts a
    /// single `payment_authorization` (D-14).
    RarTooManyEntries,
    /// The revocation cache is older than the fail-closed staleness bound; the
    /// wallet refuses to sign until it refreshes (D-6). Retryable.
    RevocationCacheStale,
}

/// Operator-facing remediation hint attached to a [`ProblemDetail`].
///
/// Mirrors the zentity OAuth error vocabulary so a 401 from `/v1/payments/sign`
/// can suggest the right next call: refresh DPoP, re-auth via CIBA, or request
/// a new RAR. The fields are all optional so the runtime emits only the hints
/// that apply.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemediationHint {
    /// Recommended next action label, e.g. `"refresh_dpop"`, `"reauth_ciba"`,
    /// `"request_new_authorization"`.
    pub action: String,
    /// URL pointing at the human-readable docs for this remediation step.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub docs_url: Option<String>,
    /// CIBA endpoint to call when `action` is `"reauth_ciba"`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ciba_endpoint: Option<String>,
    /// Authorization endpoint to call when `action` is
    /// `"request_new_authorization"`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub authorize_endpoint: Option<String>,
}

/// PRC-7807 problem-detail envelope.
///
/// The wallet returns this body with a `Content-Type: application/problem+json`
/// header on every non-2xx response. The `kind` field replaces RFC 7807's
/// `type` URL so the wire stays self-contained; consumers dispatch on `kind`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProblemDetail {
    /// Discriminator for the kind of problem.
    pub kind: ProblemKind,
    /// Short human-readable title.
    pub title: String,
    /// Long human-readable explanation suitable for an operator log.
    pub detail: String,
    /// Whether retrying the identical call could succeed.
    pub retryable: bool,
    /// Optional remediation hint.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remediation: Option<RemediationHint>,
}

impl ProblemDetail {
    /// Constructs a not-retryable problem detail with the given kind and
    /// title/detail strings.
    #[must_use]
    pub fn not_retryable(
        kind: ProblemKind,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            detail: detail.into(),
            retryable: false,
            remediation: None,
        }
    }

    /// Constructs a retryable problem detail with the given kind and
    /// title/detail strings.
    #[must_use]
    pub fn retryable(
        kind: ProblemKind,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            detail: detail.into(),
            retryable: true,
            remediation: None,
        }
    }

    /// Attaches a remediation hint and returns the modified problem detail.
    #[must_use]
    pub fn with_remediation(mut self, remediation: RemediationHint) -> Self {
        self.remediation = Some(remediation);
        self
    }
}

impl core::fmt::Display for ProblemDetail {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "{} ({:?}): {}",
            self.title, self.kind, self.detail
        )
    }
}

impl core::error::Error for ProblemDetail {}

#[cfg(test)]
mod tests {
    use super::{ProblemDetail, ProblemKind, RemediationHint};

    #[test]
    fn serializes_kind_as_snake_case() -> Result<(), serde_json::Error> {
        let problem = ProblemDetail::not_retryable(
            ProblemKind::IntentMismatch,
            "intent mismatch",
            "recomputed v1:sha256: did not match RAR entry",
        );
        let wire = serde_json::to_value(&problem)?;
        assert_eq!(wire["kind"].as_str(), Some("intent_mismatch"));
        assert_eq!(wire["retryable"].as_bool(), Some(false));
        assert!(wire.get("remediation").is_none());
        Ok(())
    }

    #[test]
    fn serializes_remediation_when_present() -> Result<(), serde_json::Error> {
        let problem = ProblemDetail::retryable(
            ProblemKind::NotReady,
            "not ready",
            "verifier stubs not yet wired",
        )
        .with_remediation(RemediationHint {
            action: "request_new_authorization".to_owned(),
            docs_url: Some("https://errors.zentity.xyz/wallet/not_ready".to_owned()),
            ciba_endpoint: None,
            authorize_endpoint: None,
        });
        let wire = serde_json::to_value(&problem)?;
        assert_eq!(wire["kind"].as_str(), Some("not_ready"));
        assert_eq!(
            wire["remediation"]["action"].as_str(),
            Some("request_new_authorization"),
        );
        assert!(wire["remediation"].get("ciba_endpoint").is_none());
        Ok(())
    }
}
