//! Integration tests that open a real libSQL file in a tempdir and
//! round-trip every store trait method, mirroring the in-memory
//! impl's behaviour.

use std::error::Error;

use tempfile::TempDir;

use zpay_core::broadcast::BroadcastOutcome;
use zpay_core::prepare::{Preparation, PreparedTxEntry, PreparedTxStore};
use zpay_core::status::{SettlementLedgerEntry, SettlementLedgerStore};
use zpay_core::types::{PayeeId, PaymentId, PaymentNetwork, Zatoshis};

use zpay_store::{LibsqlPreparedTxStore, LibsqlSettlementLedgerStore, open_and_migrate};

type TestResult = Result<(), Box<dyn Error>>;

const FIXTURE_JKT: &str = "test-jkt-primary";
const ALTERNATE_FIXTURE_JKT: &str = "test-jkt-rival";

async fn fresh_stores()
-> Result<(TempDir, LibsqlPreparedTxStore, LibsqlSettlementLedgerStore), Box<dyn Error>> {
    let temp = TempDir::new()?;
    let path = temp.path().join("zpay.libsql");
    let url = format!("file:{}", path.display());
    let connection = open_and_migrate(&url, None).await?;
    let prepared = LibsqlPreparedTxStore::new(connection.clone());
    let ledger = LibsqlSettlementLedgerStore::new(connection);
    Ok((temp, prepared, ledger))
}

fn sample_entry(payment_id: &str, idempotency_key: Option<&str>) -> PreparedTxEntry {
    sample_entry_with_jkt(payment_id, idempotency_key, FIXTURE_JKT)
}

fn sample_entry_with_jkt(
    payment_id: &str,
    idempotency_key: Option<&str>,
    jkt: &str,
) -> PreparedTxEntry {
    PreparedTxEntry {
        preparation: Preparation {
            payment_id: PaymentId(payment_id.to_owned()),
            payment_uri: String::new(),
            memo_bytes: vec![0xFF, 0x01, 0x42, 0x43, 0x44],
            expiry_height: 1_234,
            amount_zat: Zatoshis(50_000),
        },
        payee_id: PayeeId("aether-ai".to_owned()),
        network: PaymentNetwork::Testnet,
        recipient_unified_address: "utest1example".to_owned(),
        amount_zat: Zatoshis(50_000),
        expires_at_unix_seconds: 1_700_000_000,
        idempotency_key: idempotency_key.map(str::to_owned),
        agent_dpop_jkt: jkt.to_owned(),
    }
}

#[tokio::test]
async fn prepared_tx_round_trip() -> TestResult {
    let (_temp, store, _) = fresh_stores().await?;
    let entry = sample_entry("pid-1", Some("order-001"));
    store.insert(entry).await?;

    let by_id = store
        .find_by_payment_id(&PaymentId("pid-1".to_owned()))
        .await?
        .ok_or("by_payment_id miss")?;
    assert_eq!(by_id.amount_zat, Zatoshis(50_000));
    assert_eq!(by_id.preparation.amount_zat, Zatoshis(50_000));

    let by_key = store
        .find_by_idempotency(FIXTURE_JKT, "order-001")
        .await?
        .ok_or("by_idempotency miss")?;
    assert_eq!(by_key.preparation.payment_id.0, "pid-1");
    assert_eq!(by_key.preparation.amount_zat, Zatoshis(50_000));
    assert_eq!(by_key.agent_dpop_jkt, FIXTURE_JKT);

    assert_eq!(store.entry_count().await?, 1);

    let removed = store.remove(&PaymentId("pid-1".to_owned())).await?;
    assert!(removed.is_some());
    assert_eq!(store.entry_count().await?, 0);
    Ok(())
}

#[tokio::test]
async fn idempotency_is_scoped_by_jkt() -> TestResult {
    // Same idempotency_key, different DPoP keys, distinct rows. The
    // partial-UNIQUE index on `(agent_dpop_jkt, idempotency_key)` is
    // the regression gate; without it a rival agent could collide
    // onto another agent's prepared row.
    let (_temp, store, _) = fresh_stores().await?;
    store
        .insert(sample_entry_with_jkt(
            "pid-primary",
            Some("order-001"),
            FIXTURE_JKT,
        ))
        .await?;
    store
        .insert(sample_entry_with_jkt(
            "pid-rival",
            Some("order-001"),
            ALTERNATE_FIXTURE_JKT,
        ))
        .await?;
    assert_eq!(store.entry_count().await?, 2);

    let primary = store
        .find_by_idempotency(FIXTURE_JKT, "order-001")
        .await?
        .ok_or("primary jkt miss")?;
    assert_eq!(primary.preparation.payment_id.0, "pid-primary");

    let rival = store
        .find_by_idempotency(ALTERNATE_FIXTURE_JKT, "order-001")
        .await?
        .ok_or("rival jkt miss")?;
    assert_eq!(rival.preparation.payment_id.0, "pid-rival");

    let none = store
        .find_by_idempotency("test-jkt-stranger", "order-001")
        .await?;
    assert!(none.is_none());
    Ok(())
}

#[tokio::test]
async fn idempotent_find_rebuilds_payment_uri() -> TestResult {
    let (_temp, store, _) = fresh_stores().await?;

    // The wire path stores an entry whose preparation carries the
    // ZIP-321 URI it just derived. The libSQL schema does NOT persist
    // payment_uri, so it must be recomputed on read.
    let mut entry = sample_entry("pid-1", Some("order-001"));
    entry.preparation.payment_uri = "zcash:utest1example?amount=0.0005&memo=_wEBAQ".to_owned();
    let expected_uri =
        zpay_core::prepare::compose_payment_uri(
            &entry.recipient_unified_address,
            entry.amount_zat,
            &entry.preparation.memo_bytes,
        );

    store.insert(entry.clone()).await?;

    let by_id = store
        .find_by_payment_id(&PaymentId("pid-1".to_owned()))
        .await?
        .ok_or("by_payment_id miss")?;
    assert!(
        !by_id.preparation.payment_uri.is_empty(),
        "payment_uri must be non-empty on read",
    );
    assert!(
        by_id.preparation.payment_uri.starts_with("zcash:"),
        "payment_uri must be a ZIP-321 URI, got: {}",
        by_id.preparation.payment_uri,
    );
    assert_eq!(
        by_id.preparation.payment_uri, expected_uri,
        "find_by_payment_id must re-derive the same URI compose_payment_uri produces",
    );

    let by_key = store
        .find_by_idempotency(FIXTURE_JKT, "order-001")
        .await?
        .ok_or("by_idempotency miss")?;
    assert_eq!(
        by_key.preparation.payment_uri, expected_uri,
        "find_by_idempotency must rebuild the SAME payment_uri as find_by_payment_id; clients that retry /prepare depend on this",
    );

    Ok(())
}

#[tokio::test]
async fn sweep_drops_expired_only() -> TestResult {
    let (_temp, store, _) = fresh_stores().await?;
    let mut short = sample_entry("pid-short", None);
    short.expires_at_unix_seconds = 1_700_000_000;
    let mut long = sample_entry("pid-long", None);
    long.expires_at_unix_seconds = 9_999_999_999;
    store.insert(short).await?;
    store.insert(long).await?;

    let dropped = store.sweep_expired(1_700_000_005).await?;
    assert_eq!(dropped, 1);
    assert!(
        store
            .find_by_payment_id(&PaymentId("pid-short".to_owned()))
            .await?
            .is_none()
    );
    assert!(
        store
            .find_by_payment_id(&PaymentId("pid-long".to_owned()))
            .await?
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn idempotency_clash_returns_integrity_violation() -> TestResult {
    let (_temp, store, _) = fresh_stores().await?;
    let entry = sample_entry("pid-1", Some("order-001"));
    store.insert(entry.clone()).await?;

    // Driver-side: the prepare path resolves via find_by_idempotency
    // first and never gets here. We assert the SQL constraint still
    // fires if a buggy caller bypasses the lookup.
    let mut clash = entry;
    clash.preparation.payment_id = PaymentId("pid-2".to_owned());
    let err = store.insert(clash).await.err().ok_or("insert must clash")?;
    let lower = format!("{err}").to_lowercase();
    assert!(
        lower.contains("integrity") || lower.contains("unique"),
        "expected integrity error, got: {err}",
    );
    Ok(())
}

#[tokio::test]
async fn settlement_ledger_round_trip() -> TestResult {
    let (_temp, _, ledger) = fresh_stores().await?;
    let payment_id = PaymentId("pid-1".to_owned());
    ledger
        .record(
            payment_id.clone(),
            SettlementLedgerEntry {
                broadcast_outcome: BroadcastOutcome::Accepted {
                    transaction_id: "deadbeef".to_owned(),
                },
                settled_at_unix_seconds: 1_700_000_000,
                confirmation_count: None,
                mined_block_height: None,
            },
        )
        .await?;

    let found = ledger
        .find(&payment_id)
        .await?
        .ok_or("ledger find miss")?;
    let BroadcastOutcome::Accepted { transaction_id } = found.broadcast_outcome else {
        return Err("expected Accepted outcome".into());
    };
    assert_eq!(transaction_id, "deadbeef");
    assert_eq!(found.confirmation_count, None);

    let updated = ledger
        .record_confirmation(&payment_id, 3, Some(123_456))
        .await?;
    assert!(updated);

    let after = ledger
        .find(&payment_id)
        .await?
        .ok_or("ledger find miss after confirmation")?;
    assert_eq!(after.confirmation_count, Some(3));
    assert_eq!(after.mined_block_height, Some(123_456));

    let pairs = ledger.success_kind_transactions().await?;
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].1, "deadbeef");
    Ok(())
}

#[tokio::test]
async fn record_confirmation_misses_on_unknown_payment_id() -> TestResult {
    let (_temp, _, ledger) = fresh_stores().await?;
    let missing = ledger
        .record_confirmation(&PaymentId("never-recorded".to_owned()), 1, None)
        .await?;
    assert!(!missing);
    Ok(())
}

#[tokio::test]
async fn success_kind_transactions_skips_failure_outcomes() -> TestResult {
    let (_temp, _, ledger) = fresh_stores().await?;
    ledger
        .record(
            PaymentId("ok".to_owned()),
            SettlementLedgerEntry {
                broadcast_outcome: BroadcastOutcome::Accepted {
                    transaction_id: "abcd".to_owned(),
                },
                settled_at_unix_seconds: 1,
                confirmation_count: None,
                mined_block_height: None,
            },
        )
        .await?;
    ledger
        .record(
            PaymentId("fail".to_owned()),
            SettlementLedgerEntry {
                broadcast_outcome: BroadcastOutcome::Rejected {
                    upstream_message: "no".to_owned(),
                },
                settled_at_unix_seconds: 2,
                confirmation_count: None,
                mined_block_height: None,
            },
        )
        .await?;

    let pairs = ledger.success_kind_transactions().await?;
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].0, PaymentId("ok".to_owned()));
    Ok(())
}
