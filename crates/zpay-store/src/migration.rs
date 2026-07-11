//! Schema migration runner.
//!
//! Each migration script is applied at most once per `version`,
//! recorded in [`MIGRATION_TABLE`]. The version gate ensures a
//! non-idempotent statement (an `ALTER TABLE ADD COLUMN`) runs exactly
//! once even when a later start replays the runner.

use libsql::params;

use zpay_core::store::StoreError;

use crate::connection::StoreConnection;
use crate::{MIGRATION_TABLE, SCHEMA_VERSION};

/// Every migration script paired with the schema version it produces.
///
/// Ascending order. Version `1` creates the bookkeeping table and is
/// fully idempotent (`CREATE TABLE IF NOT EXISTS`, `INSERT OR IGNORE`);
/// later versions carry non-idempotent statements gated by the persisted
/// version.
const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../migrations/0001_initial.sql")),
    (
        2,
        include_str!("../migrations/0002_reorg_aware_settlement.sql"),
    ),
    (
        3,
        include_str!("../migrations/0003_settlement_ledger_payee_and_amount.sql"),
    ),
];

/// Apply every migration whose version is greater than the
/// already-applied set, in order. Idempotent across reruns.
///
/// # Errors
///
/// Returns [`StoreError::Unavailable`] when the underlying libSQL
/// connection rejects a statement, and [`StoreError::MigrationPending`]
/// when the persisted schema version is newer than what this binary
/// understands (rollbacks are not supported).
pub async fn run_migrations(connection: &StoreConnection) -> Result<(), StoreError> {
    // Version 1 creates the bookkeeping table every later step reads, and
    // is idempotent, so it runs unconditionally.
    apply_migration(connection, MIGRATIONS[0].1).await?;

    let applied = max_applied_version(connection).await?;
    if applied > SCHEMA_VERSION {
        return Err(StoreError::MigrationPending {
            current_version: applied,
            required_version: SCHEMA_VERSION,
        });
    }

    for (version, sql) in &MIGRATIONS[1..] {
        if *version > applied {
            apply_migration(connection, sql).await?;
        }
    }

    tracing::info!(
        event = "zpay_store_migrations_applied",
        schema_version = SCHEMA_VERSION,
        migration_table = MIGRATION_TABLE,
        "libsql schema migrations applied",
    );
    Ok(())
}

async fn apply_migration(connection: &StoreConnection, sql: &str) -> Result<(), StoreError> {
    connection
        .execute_transactional_batch(sql)
        .await
        .map_err(|err| StoreError::Unavailable {
            reason: format!("schema migration failed: {err}"),
        })
}

async fn max_applied_version(connection: &StoreConnection) -> Result<u32, StoreError> {
    let mut rows = connection
        .query(
            "SELECT COALESCE(MAX(version), 0) FROM zpay_schema_migrations",
            params![],
        )
        .await
        .map_err(|err| StoreError::Unavailable {
            reason: format!("read of migration version failed: {err}"),
        })?;
    let row = rows
        .next()
        .await
        .map_err(|err| StoreError::Unavailable {
            reason: format!("read of migration version row failed: {err}"),
        })?
        .ok_or_else(|| StoreError::Unavailable {
            reason: "migration table missing after batch apply".to_owned(),
        })?;
    let raw: i64 = row.get(0).map_err(|err| StoreError::RowMalformed {
        reason: format!("migration version column non-integer: {err}"),
    })?;
    u32::try_from(raw).map_err(|_| StoreError::RowMalformed {
        reason: "migration version overflowed u32".to_owned(),
    })
}
