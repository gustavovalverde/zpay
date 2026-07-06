//! Applies the reorg-aware migration on top of a hand-built v1 database
//! and asserts the added columns land with their defaults while the
//! pre-existing row survives.

use std::error::Error;

use libsql::params;
use tempfile::TempDir;

use zpay_core::status::SettlementLedgerStore;
use zpay_core::types::PaymentId;

use zpay_store::{LibsqlSettlementLedgerStore, SCHEMA_VERSION, StoreConnection, run_migrations};

type TestResult = Result<(), Box<dyn Error>>;

/// The v1 `settlement_ledger` shape (0001), before the reorg columns exist.
const V1_SCHEMA_SQL: &str = "\
CREATE TABLE zpay_schema_migrations (\
    version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL, description TEXT NOT NULL);\
CREATE TABLE settlement_ledger (\
    payment_id TEXT PRIMARY KEY, \
    broadcast_outcome_kind TEXT NOT NULL CHECK (broadcast_outcome_kind IN \
        ('accepted', 'duplicate', 'invalid_encoding', 'rejected', 'unknown')), \
    transaction_id TEXT, upstream_message TEXT, settled_at_unix_seconds INTEGER NOT NULL, \
    confirmation_count INTEGER, mined_block_height INTEGER, \
    last_confirmation_check_at_unix_seconds INTEGER);\
INSERT INTO zpay_schema_migrations (version, applied_at_ms, description) \
    VALUES (1, 0, 'initial');";

#[tokio::test]
async fn migration_upgrades_existing_v1_database() -> TestResult {
    let temp = TempDir::new()?;
    let path = temp.path().join("zpay.libsql");
    let url = format!("file:{}", path.display());

    let connection = StoreConnection::open(&url, None).await?;
    connection
        .execute_transactional_batch(V1_SCHEMA_SQL)
        .await?;
    connection
        .execute(
            "INSERT INTO settlement_ledger (\
                payment_id, broadcast_outcome_kind, transaction_id, settled_at_unix_seconds, \
                confirmation_count, mined_block_height) \
            VALUES ('legacy', 'accepted', 'deadbeef', 1700000000, 2, 500000)",
            params![],
        )
        .await?;

    // Upgrade the live v1 database in place.
    run_migrations(&connection).await?;

    // The version ledger advanced to the current schema version.
    let mut rows = connection
        .query(
            "SELECT COALESCE(MAX(version), 0) FROM zpay_schema_migrations",
            params![],
        )
        .await?;
    let version: i64 = rows.next().await?.ok_or("version row")?.get(0)?;
    assert_eq!(u32::try_from(version)?, SCHEMA_VERSION);

    // The pre-existing row now reads back through the typed store with the
    // new columns defaulted.
    let ledger = LibsqlSettlementLedgerStore::new(connection.clone());
    let entry = ledger
        .find(&PaymentId("legacy".to_owned()))
        .await?
        .ok_or("legacy row missing after migration")?;
    assert_eq!(entry.confirmation_count, Some(2));
    assert_eq!(entry.mined_block_height, Some(500_000));
    assert_eq!(entry.reorg_count, 0);
    assert_eq!(entry.last_reorged_at, None);
    assert_eq!(entry.expiry_height, None);

    // The reorg-aware path works against the upgraded row.
    assert!(
        ledger
            .downgrade_on_reorg(&PaymentId("legacy".to_owned()), 1_700_001_000)
            .await?
    );
    let after = ledger
        .find(&PaymentId("legacy".to_owned()))
        .await?
        .ok_or("legacy row missing after downgrade")?;
    assert_eq!(after.reorg_count, 1);
    assert_eq!(after.mined_block_height, None);

    // Re-running the migration on the already-upgraded database is a no-op.
    run_migrations(&connection).await?;
    Ok(())
}
