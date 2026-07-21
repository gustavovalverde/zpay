//! Verifies the schema-v4 settlement outcome invariant against hand-built
//! schema-v3 databases.

use std::error::Error;

use libsql::params;
use tempfile::TempDir;

use zpay_core::store::StoreError;

use zpay_store::{SCHEMA_VERSION, StoreConnection, run_migrations};

type TestResult = Result<(), Box<dyn Error>>;

const V3_SCHEMA_SQL: &str = "\
CREATE TABLE zpay_schema_migrations (\
    version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL, description TEXT NOT NULL);\
CREATE TABLE settlement_ledger (\
    payment_id TEXT PRIMARY KEY, \
    broadcast_outcome_kind TEXT NOT NULL CHECK (broadcast_outcome_kind IN \
        ('accepted', 'duplicate', 'invalid_encoding', 'rejected', 'unknown')), \
    transaction_id TEXT, upstream_message TEXT, settled_at_unix_seconds INTEGER NOT NULL, \
    confirmation_count INTEGER, mined_block_height INTEGER, \
    last_confirmation_check_at_unix_seconds INTEGER, \
    reorg_count INTEGER NOT NULL DEFAULT 0, last_reorged_at INTEGER, expiry_height INTEGER, \
    payee_id TEXT NOT NULL DEFAULT '', amount_zat INTEGER NOT NULL DEFAULT 0);\
INSERT INTO zpay_schema_migrations (version, applied_at_ms, description) VALUES \
    (1, 0, 'initial'), (2, 0, 'reorg-aware settlement'), (3, 0, 'payee attribution');";

async fn v3_connection(temp: &TempDir) -> Result<StoreConnection, StoreError> {
    let path = temp.path().join("zpay.libsql");
    let connection = StoreConnection::open(&format!("file:{}", path.display()), None).await?;
    connection
        .execute_transactional_batch(V3_SCHEMA_SQL)
        .await
        .map_err(|error| StoreError::Unavailable {
            reason: error.to_string(),
        })?;
    Ok(connection)
}

async fn applied_version(connection: &StoreConnection) -> Result<u32, Box<dyn Error>> {
    let mut rows = connection
        .query(
            "SELECT COALESCE(MAX(version), 0) FROM zpay_schema_migrations",
            params![],
        )
        .await?;
    let raw: i64 = rows.next().await?.ok_or("version row missing")?.get(0)?;
    Ok(u32::try_from(raw)?)
}

#[tokio::test]
async fn migration_rejects_legacy_success_without_transaction_id() -> TestResult {
    let temp = TempDir::new()?;
    let connection = v3_connection(&temp).await?;
    connection
        .execute(
            "INSERT INTO settlement_ledger (\
                payment_id, broadcast_outcome_kind, upstream_message, settled_at_unix_seconds) \
             VALUES ('legacy-duplicate', 'duplicate', 'already known', 1700000000)",
            params![],
        )
        .await?;

    let error = match run_migrations(&connection).await {
        Ok(()) => return Err("schema-v3 success row without a transaction id was accepted".into()),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        StoreError::SchemaInvariantViolation {
            invariant: "settlement_outcome_columns_v4",
            violating_rows: 1,
        }
    ));
    assert_eq!(applied_version(&connection).await?, 3);

    let mut rows = connection
        .query(
            "SELECT transaction_id, upstream_message FROM settlement_ledger \
             WHERE payment_id = 'legacy-duplicate'",
            params![],
        )
        .await?;
    let row = rows.next().await?.ok_or("legacy row missing")?;
    let transaction_id: Option<String> = row.get(0)?;
    let upstream_message: Option<String> = row.get(1)?;
    assert_eq!(transaction_id, None);
    assert_eq!(upstream_message.as_deref(), Some("already known"));
    Ok(())
}

#[tokio::test]
async fn migration_preserves_valid_rows_and_is_idempotent() -> TestResult {
    let temp = TempDir::new()?;
    let connection = v3_connection(&temp).await?;
    connection
        .execute(
            "INSERT INTO settlement_ledger (\
                payment_id, broadcast_outcome_kind, transaction_id, settled_at_unix_seconds) \
             VALUES ('accepted', 'accepted', 'abc123', 1700000000)",
            params![],
        )
        .await?;
    connection
        .execute(
            "INSERT INTO settlement_ledger (\
                payment_id, broadcast_outcome_kind, upstream_message, settled_at_unix_seconds) \
             VALUES ('rejected', 'rejected', 'policy rejection', 1700000001)",
            params![],
        )
        .await?;

    run_migrations(&connection).await?;
    assert_eq!(applied_version(&connection).await?, SCHEMA_VERSION);

    let mut rows = connection
        .query(
            "SELECT payment_id, transaction_id, upstream_message FROM settlement_ledger \
             ORDER BY payment_id",
            params![],
        )
        .await?;
    let accepted = rows.next().await?.ok_or("accepted row missing")?;
    assert_eq!(accepted.get::<String>(0)?, "accepted");
    assert_eq!(
        accepted.get::<Option<String>>(1)?.as_deref(),
        Some("abc123")
    );
    assert_eq!(accepted.get::<Option<String>>(2)?, None);
    let rejected = rows.next().await?.ok_or("rejected row missing")?;
    assert_eq!(rejected.get::<String>(0)?, "rejected");
    assert_eq!(rejected.get::<Option<String>>(1)?, None);
    assert_eq!(
        rejected.get::<Option<String>>(2)?.as_deref(),
        Some("policy rejection")
    );

    run_migrations(&connection).await?;
    Ok(())
}

#[tokio::test]
async fn schema_v4_constraint_rejects_invalid_inserts_and_updates() -> TestResult {
    let temp = TempDir::new()?;
    let connection = v3_connection(&temp).await?;
    connection
        .execute(
            "INSERT INTO settlement_ledger (\
                payment_id, broadcast_outcome_kind, transaction_id, settled_at_unix_seconds) \
             VALUES ('accepted', 'accepted', 'abc123', 1700000000)",
            params![],
        )
        .await?;
    run_migrations(&connection).await?;

    let invalid_insert = connection
        .execute(
            "INSERT INTO settlement_ledger (\
                payment_id, broadcast_outcome_kind, settled_at_unix_seconds) \
             VALUES ('invalid-success', 'accepted', 1700000002)",
            params![],
        )
        .await;
    let Err(error) = invalid_insert else {
        return Err("schema v4 accepted a success row without a transaction id".into());
    };
    assert!(error.to_string().contains("settlement_outcome_columns_v4"));

    let invalid_failure = connection
        .execute(
            "INSERT INTO settlement_ledger (\
                payment_id, broadcast_outcome_kind, settled_at_unix_seconds) \
             VALUES ('invalid-failure', 'rejected', 1700000003)",
            params![],
        )
        .await;
    let Err(error) = invalid_failure else {
        return Err("schema v4 accepted a failure row without a message".into());
    };
    assert!(error.to_string().contains("settlement_outcome_columns_v4"));

    let invalid_update = connection
        .execute(
            "UPDATE settlement_ledger SET transaction_id = NULL WHERE payment_id = 'accepted'",
            params![],
        )
        .await;
    let Err(error) = invalid_update else {
        return Err("schema v4 allowed a valid success row to lose its transaction id".into());
    };
    assert!(error.to_string().contains("settlement_outcome_columns_v4"));
    Ok(())
}
