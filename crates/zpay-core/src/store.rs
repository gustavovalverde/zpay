//! Storage-trait abstractions for zpay's stateful tables.
//!
//! [`PreparedTxStore`][crate::prepare::PreparedTxStore] and
//! [`SettlementLedgerStore`][crate::status::SettlementLedgerStore] sit
//! between the protocol-neutral [`prepare`][crate::prepare] /
//! [`settle`][crate::settle] / [`status`][crate::status] paths and
//! whatever persistence layer the runtime composes. Two implementations exist:
//!
//! - An in-memory variant (in [`crate::prepare::PreparedTxCache`] and
//!   [`crate::status::SettlementLedger`]) that lives in this crate
//!   behind the `in_memory` feature, used by unit tests and by ad-hoc
//!   local development.
//! - A libSQL variant in the `zpay-store` crate, used by production
//!   deployments and the live e2e harness.
//!
//! Both implementations share the same trait surface so the wire
//! adapters, the TTL sweeper, and the confirmation oracle do not see
//! which backend they are talking to. See [ADR-0004][adr] for the
//! persistence decision and the schema discipline.
//!
//! [adr]: https://github.com/gustavovalverde/zpay/blob/main/docs/adrs/0004-libsql-prepared-tx-cache.md

/// Errors that storage implementations surface to callers.
///
/// The trait callers (wire adapters, sweeper, oracle) treat these as
/// retry hints. A typed enum (rather than a `Box<dyn Error>`) lets the
/// wire layer map specific failure modes onto specific HTTP responses
/// without string sniffing.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// libSQL connection pool exhausted or remote unreachable.
    /// Retry posture: `retryable`.
    #[error("store unavailable: {reason}")]
    Unavailable {
        /// Operator-facing reason. Does not include row contents.
        reason: String,
    },
    /// Schema migration was not applied before the store accepted a
    /// query. The operator must run the migration runner.
    /// Retry posture: `requires_operator`.
    #[error("migration pending: current={current_version}, required={required_version}")]
    MigrationPending {
        /// Schema version observed on the connected database.
        current_version: u32,
        /// Schema version this binary expects.
        required_version: u32,
    },
    /// Application invariant violated (foreign key, uniqueness, check
    /// constraint). Retry posture: `not_retryable`.
    #[error("integrity violation: {constraint}")]
    IntegrityViolation {
        /// Name of the violated SQL constraint or in-process invariant.
        constraint: String,
    },
    /// A stored row did not deserialize to the typed value the caller
    /// expects. Indicates either a schema drift or a corrupted row.
    /// Retry posture: `requires_operator`.
    #[error("stored row malformed: {reason}")]
    RowMalformed {
        /// Operator-facing reason. Does not include row contents.
        reason: String,
    },
}
