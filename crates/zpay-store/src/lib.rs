//! libSQL persistence for the prepared-tx cache and settlement ledger.
//!
//! See [ADR-0004][adr] for the persistence decision.
//!
//! The crate exposes two implementations of zpay-core's storage
//! traits: [`LibsqlPreparedTxStore`] for [`zpay_core::prepare::PreparedTxStore`]
//! and [`LibsqlSettlementLedgerStore`] for
//! [`zpay_core::status::SettlementLedgerStore`]. Both share a single
//! [`StoreConnection`] that auto-reconnects on Turso Hrana stream
//! expiry.
//!
//! Schema migrations live under `migrations/`, numbered from `0001`
//! and applied in order via [`run_migrations`]. The current schema
//! version is exported as [`SCHEMA_VERSION`].
//!
//! [adr]: https://github.com/gustavovalverde/zpay/blob/main/docs/adrs/0004-libsql-prepared-tx-cache.md

pub mod connection;
pub mod migration;
pub mod prepared_tx;
pub mod settlement_ledger;

pub use connection::StoreConnection;
pub use migration::run_migrations;
pub use prepared_tx::LibsqlPreparedTxStore;
pub use settlement_ledger::LibsqlSettlementLedgerStore;
pub use zpay_core::store::StoreError;

/// Schema version produced by applying every file in `migrations/`.
pub const SCHEMA_VERSION: u32 = 2;

/// Name of the bookkeeping table that records applied migrations.
pub const MIGRATION_TABLE: &str = "zpay_schema_migrations";

/// Convenience: open a [`StoreConnection`] and run the migrations the
/// current binary requires. Returns the live connection on success.
///
/// # Errors
///
/// Forwards every failure from [`StoreConnection::open`] and
/// [`run_migrations`].
pub async fn open_and_migrate(
    store_url: &str,
    auth_token: Option<&str>,
) -> Result<StoreConnection, StoreError> {
    let connection = StoreConnection::open(store_url, auth_token).await?;
    run_migrations(&connection).await?;
    Ok(connection)
}
