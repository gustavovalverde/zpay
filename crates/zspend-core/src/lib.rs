//! Service-internal vocabulary for the `zspend` wallet runtime.
//!
//! Carries the typed payment-authorization (RAR) entry, the PRC-7807 problem
//! detail envelope the runtime returns on every error path, and the
//! startup-time [`SigningPolicy`] that binds the binary to a network and a set
//! of operator-pinned invariants. Per Proposal-0003 D-1 and D-10 in
//! `docs/proposals/0003-agent-wallet-production-architecture.md`.
//!
//! This crate is strictly internal to the zpay workspace: it has no public
//! library API outside the workspace, no semver promises across the workspace
//! boundary, and no external consumers. The runtime binary
//! (`zspend-runtime`) is its only consumer.

mod error;
mod payment_authorization;
mod signing_policy;

pub use error::{ProblemDetail, ProblemKind, RemediationHint};
pub use payment_authorization::{
    Amount, AmountUnit, ChainId, ExpiresAt, IntentHashString, PAYMENT_AUTHORIZATION_TYPE_LITERAL,
    PaymentAuthorization, PaymentAuthorizationType,
};
pub use signing_policy::{SigningPolicy, SigningPolicyBuilder, SigningPolicyError};
