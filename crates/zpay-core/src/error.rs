//! Typed errors for the payment lifecycle.
//!
//! Every variant documents its retry posture: `retryable`,
//! `not_retryable`, or `requires_operator`. See
//! [error-vocabulary.md](https://github.com/gustavovalverde/zpay/blob/main/docs/reference/error-vocabulary.md)
//! for the canonical registry.
//!
//! The active `PrepareError` lives in [`crate::prepare`] so it can name
//! the prepare-specific collaborators (registry, tip oracle, store).
//! This module carries the settle / oracle / verify / compliance
//! vocabularies that the wire adapter maps onto RFC 7807 problem
//! documents.

use crate::types::PaymentId;

/// Errors that can arise during settlement.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SettleError {
    /// Caller's `payment_id` does not exist or expired. Retry posture: `not_retryable`.
    #[error("preparation not found: {payment_id}")]
    PreparationNotFound {
        /// The unknown `payment_id`.
        payment_id: PaymentId,
    },

    /// Prepared transaction expired. Retry posture: `not_retryable`; agent must re-prepare.
    #[error("preparation expired: {payment_id}")]
    PreparationExpired {
        /// The expired `payment_id`.
        payment_id: PaymentId,
    },

    /// zinder unreachable; check `/readyz`. Retry posture: `requires_operator`.
    #[error("indexer unavailable")]
    IndexerUnavailable,

    /// libSQL unreachable. Retry posture: `requires_operator`.
    #[error("store unavailable")]
    StoreUnavailable,
}

/// Errors that can arise during confirmation oracle lookups.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OracleError {
    /// Caller's `payment_id` never reached settle. Retry posture: `not_retryable`.
    #[error("payment not found: {payment_id}")]
    PaymentNotFound {
        /// The unknown `payment_id`.
        payment_id: PaymentId,
    },

    /// Fallback zexplorer watch endpoint unreachable. Retry posture: `requires_operator`.
    #[error("watch endpoint unavailable")]
    WatchEndpointUnavailable,
}

/// Errors that can arise during disclosure verification.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// ZIP-311 disclosure payload did not parse. Retry posture: `not_retryable`.
    #[error("disclosure invalid")]
    DisclosureInvalid,

    /// zinder's ZIP-311 verifier capability is off; enable upstream.
    /// Retry posture: `requires_operator`.
    #[error("verifier capability disabled upstream")]
    VerifierCapabilityDisabled,
}

/// Errors that can arise during PoH-token compliance checks.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ComplianceError {
    /// JWKS fetch failed transiently. Retry posture: `retryable`.
    #[error("jwks fetch failed")]
    JwksFetchFailed,

    /// PoH `iss` claim is not in `ZPAY_COMPLIANCE__ACCEPTED_ISSUERS`.
    /// Retry posture: `not_retryable`.
    #[error("issuer not trusted: {iss}")]
    IssuerNotTrusted {
        /// The rejected issuer claim.
        iss: String,
    },
}
