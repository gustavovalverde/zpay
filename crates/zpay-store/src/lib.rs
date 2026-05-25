//! libSQL persistence for the prepared-tx cache and settlement ledger.
//!
//! See [ADR-0004][adr] for the choice of libSQL over alternatives.
//!
//! Schema migrations live under `migrations/` numbered from `0001` and
//! applied in order. The current schema version is exported as
//! [`SCHEMA_VERSION`].
//!
//! Implementation lands in M1; this scaffold only declares the typed
//! surface and ships migration `0001_initial.sql`.
//!
//! [adr]: https://github.com/gustavovalverde/zpay/blob/main/docs/adrs/0004-libsql-prepared-tx-cache.md

/// Current schema version produced by applying every file in `migrations/`.
pub const SCHEMA_VERSION: u32 = 1;

/// Name of the bookkeeping table that records applied migrations.
pub const MIGRATION_TABLE: &str = "zpay_schema_migrations";

/// Errors that can arise during store operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// libSQL pool exhausted or remote unreachable. Retry posture: `retryable`.
    #[error("connection failed")]
    ConnectionFailed,

    /// Operator must run `zpay-ops migrate` before the store can be used.
    /// Retry posture: `requires_operator`.
    #[error("migration pending: current={current_version}, required={required_version}")]
    MigrationPending {
        /// Schema version observed on the connected libSQL database.
        current_version: u32,
        /// Schema version the running binary expects.
        required_version: u32,
    },

    /// Application invariant violated. Retry posture: `not_retryable`.
    #[error("integrity violation: {constraint}")]
    IntegrityViolation {
        /// Name of the violated SQL constraint or invariant.
        constraint: String,
    },
}
