//! Schema migration runner.
//!
//! Each migration script is applied at most once per `version`,
//! recorded in [`MIGRATION_TABLE`]. Re-running a migration after a
//! later one renamed columns it referenced would fail at the SQL
//! layer; the version gate prevents that.

use libsql::params;

use zpay_core::store::StoreError;

use crate::connection::StoreConnection;
use crate::{MIGRATION_TABLE, SCHEMA_VERSION};

const INITIAL_SCHEMA_SQL: &str = include_str!("../migrations/0001_initial.sql");

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
    connection
        .execute_transactional_batch(INITIAL_SCHEMA_SQL)
        .await
        .map_err(|err| StoreError::Unavailable {
            reason: format!("initial schema migration failed: {err}"),
        })?;

    let applied = max_applied_version(connection).await?;
    if applied > SCHEMA_VERSION {
        return Err(StoreError::MigrationPending {
            current_version: applied,
            required_version: SCHEMA_VERSION,
        });
    }

    tracing::info!(
        event = "zpay_store_migrations_applied",
        schema_version = SCHEMA_VERSION,
        migration_table = MIGRATION_TABLE,
        "libsql schema migrations applied",
    );
    Ok(())
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
