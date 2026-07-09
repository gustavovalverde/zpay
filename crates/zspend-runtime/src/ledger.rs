//! Single-use access-token `jti` ledger (Proposal-0003 D-8).
//!
//! The wallet must sign a given access-token `jti` at most once. The guarantee
//! is reserve-BEFORE-sign: the runtime claims the `jti` (recording the intent
//! hash it is about to commit to) before it asks the wallet to build and sign
//! the transaction, then commits the signed payload once the wallet returns.
//! A crash between reserve and commit leaves a `Pending` reservation that a
//! later identical retry treats as retryable rather than re-signing blind.
//!
//! [`UsageLedger::reserve`] returns one of four outcomes:
//!
//! - [`Reservation::Fresh`]: the `jti` was unseen; the caller proceeds to sign.
//! - [`Reservation::Completed`]: an identical replay (same `jti`, same intent
//!   hash, already committed); the caller returns the cached payload without
//!   re-signing.
//! - [`Reservation::IntentConflict`]: the `jti` was already reserved or
//!   committed against a DIFFERENT intent hash; the caller rejects with
//!   [`zspend_core::ProblemKind::TokenAlreadyConsumed`].
//! - [`Reservation::Pending`]: the `jti` is reserved against the SAME intent
//!   hash but not yet committed (a prior attempt is in flight or crashed
//!   mid-sign); the caller returns a retryable 503.
//!
//! The store is libSQL-backed so the single-use guarantee survives a process
//! restart and, against a shared libSQL URL, holds across several wallet
//! replicas. The atomic claim is `INSERT ... ON CONFLICT (jti) DO NOTHING`
//! followed by a read, inside one `IMMEDIATE` transaction, so two concurrent
//! reserves of the same fresh `jti` cannot both win.
//!
//! The connection is opened through [`zpay_store::StoreConnection`] rather
//! than a direct `libsql::Builder` call: this process also links
//! `zally-storage`'s bundled `rusqlite`, and only one of the two embedded
//! `SQLite` builds may configure the C library's global threading mode.
//! `StoreConnection` is the connection path already exercised in
//! `zpay-runtime` under that same constraint.

#[cfg(test)]
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use libsql::{TransactionBehavior, params};
use tokio::sync::Mutex;
use zpay_store::StoreConnection;

use crate::SignPaymentResponse;

/// Schema version produced by applying every file under `migrations/`.
const SCHEMA_VERSION: u32 = 1;

/// libSQL statements that stand up the ledger schema, applied idempotently.
const INITIAL_SCHEMA_SQL: &str = include_str!("../migrations/0001_initial.sql");

/// How long a `Pending` reservation may sit uncommitted before reclaim.
///
/// After this window a later reserve treats the reservation as abandoned.
/// Sized to comfortably exceed the wallet's propose-prove-sign latency so a
/// slow-but-live sign is never stolen out from under an in-flight request.
pub(crate) const PENDING_RESERVATION_TTL: Duration = Duration::from_mins(2);

/// Outcome of reserving an access-token `jti` before signing.
#[derive(Debug)]
pub(crate) enum Reservation {
    /// The `jti` was unseen and is now reserved against `intent_hash`. The
    /// caller proceeds to sign and then [`UsageLedger::commit`]s.
    Fresh,
    /// An identical replay: the same `jti` already committed the same intent.
    /// The caller returns the cached payload without re-signing.
    Completed(SignPaymentResponse),
    /// The `jti` is bound to a DIFFERENT intent hash; the caller rejects.
    IntentConflict,
    /// The `jti` is reserved against the same intent but not yet committed.
    /// A prior attempt is in flight or crashed mid-sign; retryable.
    Pending,
}

/// Errors returned by the single-use ledger.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LedgerError {
    #[error("ledger backend open failed for {url:?}: {reason}")]
    Open { url: String, reason: String },
    #[error(
        "ledger backend at {url:?} is a remote libsql:// URL but no auth token was provided; set ZSPEND_LEDGER_AUTH_TOKEN"
    )]
    MissingAuthToken { url: String },
    #[error("ledger schema version {found} is newer than this binary supports")]
    SchemaTooNew { found: u32 },
    #[error("ledger statement failed: {reason}")]
    Statement { reason: String },
    #[error("ledger row malformed: {reason}")]
    RowMalformed { reason: String },
}

/// libSQL-backed single-use `jti` store: reserve before signing, commit after.
///
/// Cloning is cheap; clones share the same connection and serialization lock.
#[derive(Clone)]
pub(crate) struct UsageLedger {
    backend: UsageLedgerBackend,
    /// Serializes every ledger operation against the shared connection.
    /// `reserve_at` holds an open multi-statement `BEGIN IMMEDIATE ..
    /// COMMIT` span; a `commit` or `release` issued on the same connection
    /// while that span is open would either collide with it or get folded
    /// into it, so every operation (not just the transaction) takes this
    /// lock for its whole body.
    lock: Arc<Mutex<()>>,
}

#[derive(Clone)]
enum UsageLedgerBackend {
    Libsql(StoreConnection),
    #[cfg(test)]
    Ephemeral(Arc<Mutex<BTreeMap<String, EphemeralLedgerEntry>>>),
}

#[cfg(test)]
#[derive(Clone)]
struct EphemeralLedgerEntry {
    intent_hash: String,
    state: EphemeralLedgerState,
    response: Option<SignPaymentResponse>,
    reserved_at_ms: i64,
}

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum EphemeralLedgerState {
    Pending,
    Completed,
}

impl UsageLedger {
    /// Opens the ledger at `url` and applies the schema migrations.
    ///
    /// `url` accepts `file:<path>` or `libsql://<host>`, matching
    /// [`StoreConnection`]'s contract. `auth_token` is required for a
    /// `libsql://` URL, checked before any connection attempt, and ignored
    /// for a file path.
    pub(crate) async fn open(url: &str, auth_token: Option<&str>) -> Result<Self, LedgerError> {
        if url.starts_with("libsql://") && auth_token.is_none() {
            return Err(LedgerError::MissingAuthToken {
                url: url.to_owned(),
            });
        }
        let connection = StoreConnection::open(url, auth_token)
            .await
            .map_err(|err| LedgerError::Open {
                url: url.to_owned(),
                reason: err.to_string(),
            })?;
        run_migrations(&connection).await?;
        Ok(Self {
            backend: UsageLedgerBackend::Libsql(connection),
            lock: Arc::new(Mutex::new(())),
        })
    }

    #[cfg(test)]
    pub(crate) fn ephemeral_for_tests() -> Self {
        Self {
            backend: UsageLedgerBackend::Ephemeral(Arc::new(Mutex::new(BTreeMap::new()))),
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Reserves `jti` against `intent_hash` before signing.
    pub(crate) async fn reserve(
        &self,
        jti: &str,
        intent_hash: &str,
    ) -> Result<Reservation, LedgerError> {
        self.reserve_at(jti, intent_hash, now_ms()).await
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guard must span the whole IMMEDIATE transaction to serialize BEGIN across tasks sharing one libSQL connection"
    )]
    async fn reserve_at(
        &self,
        jti: &str,
        intent_hash: &str,
        now_ms: i64,
    ) -> Result<Reservation, LedgerError> {
        match &self.backend {
            UsageLedgerBackend::Libsql(connection) => {
                self.reserve_libsql_at(connection, jti, intent_hash, now_ms)
                    .await
            }
            #[cfg(test)]
            UsageLedgerBackend::Ephemeral(entries) => {
                reserve_ephemeral_at(entries, jti, intent_hash, now_ms).await
            }
        }
    }

    async fn reserve_libsql_at(
        &self,
        connection: &StoreConnection,
        jti: &str,
        intent_hash: &str,
        now_ms: i64,
    ) -> Result<Reservation, LedgerError> {
        let guard = self.lock.lock().await;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(statement)?;

        let inserted = transaction
            .execute(
                "INSERT INTO usage_ledger (jti, intent_hash, state, response_json, reserved_at_ms) \
                 VALUES (?1, ?2, 'pending', NULL, ?3) \
                 ON CONFLICT(jti) DO NOTHING",
                params![jti, intent_hash, now_ms],
            )
            .await
            .map_err(statement)?;
        if inserted == 1 {
            transaction.commit().await.map_err(statement)?;
            drop(guard);
            return Ok(Reservation::Fresh);
        }

        let row =
            read_entry(&transaction, jti)
                .await?
                .ok_or_else(|| LedgerError::RowMalformed {
                    reason: format!("jti {jti} conflicted on insert but no row was found"),
                })?;
        let outcome = classify_existing(&transaction, jti, intent_hash, now_ms, row).await?;
        transaction.commit().await.map_err(statement)?;
        drop(guard);
        Ok(outcome)
    }

    /// Commits the signed `response` for a `jti` reserved against `intent_hash`,
    /// making a later identical replay return [`Reservation::Completed`].
    pub(crate) async fn commit(
        &self,
        jti: &str,
        intent_hash: &str,
        response: &SignPaymentResponse,
    ) -> Result<(), LedgerError> {
        match &self.backend {
            UsageLedgerBackend::Libsql(connection) => {
                self.commit_libsql(connection, jti, intent_hash, response)
                    .await
            }
            #[cfg(test)]
            UsageLedgerBackend::Ephemeral(entries) => {
                {
                    let mut entries = entries.lock().await;
                    entries.insert(
                        jti.to_owned(),
                        EphemeralLedgerEntry {
                            intent_hash: intent_hash.to_owned(),
                            state: EphemeralLedgerState::Completed,
                            response: Some(response.clone()),
                            reserved_at_ms: now_ms(),
                        },
                    );
                }
                Ok(())
            }
        }
    }

    async fn commit_libsql(
        &self,
        connection: &StoreConnection,
        jti: &str,
        intent_hash: &str,
        response: &SignPaymentResponse,
    ) -> Result<(), LedgerError> {
        let json = serde_json::to_string(response).map_err(|err| LedgerError::Statement {
            reason: format!("signed payload did not serialize: {err}"),
        })?;
        let guard = self.lock.lock().await;
        connection
            .execute(
                "INSERT INTO usage_ledger (jti, intent_hash, state, response_json, reserved_at_ms) \
                 VALUES (?1, ?2, 'completed', ?3, ?4) \
                 ON CONFLICT(jti) DO UPDATE SET \
                    state = 'completed', \
                    intent_hash = excluded.intent_hash, \
                    response_json = excluded.response_json",
                params![jti, intent_hash, json, now_ms()],
            )
            .await
            .map_err(statement)?;
        drop(guard);
        Ok(())
    }

    /// Releases a still-pending reservation (e.g. when signing failed) so a
    /// retry with the same `jti` sees [`Reservation::Fresh`]. A committed row is
    /// the single-use record and is left untouched.
    pub(crate) async fn release(&self, jti: &str) -> Result<(), LedgerError> {
        match &self.backend {
            UsageLedgerBackend::Libsql(connection) => self.release_libsql(connection, jti).await,
            #[cfg(test)]
            UsageLedgerBackend::Ephemeral(entries) => {
                {
                    let mut entries = entries.lock().await;
                    if entries
                        .get(jti)
                        .is_some_and(|entry| entry.state == EphemeralLedgerState::Pending)
                    {
                        entries.remove(jti);
                    }
                }
                Ok(())
            }
        }
    }

    async fn release_libsql(
        &self,
        connection: &StoreConnection,
        jti: &str,
    ) -> Result<(), LedgerError> {
        let guard = self.lock.lock().await;
        connection
            .execute(
                "DELETE FROM usage_ledger WHERE jti = ?1 AND state = 'pending'",
                params![jti],
            )
            .await
            .map_err(statement)?;
        drop(guard);
        Ok(())
    }
}

#[cfg(test)]
async fn reserve_ephemeral_at(
    entries: &Mutex<BTreeMap<String, EphemeralLedgerEntry>>,
    jti: &str,
    intent_hash: &str,
    now_ms: i64,
) -> Result<Reservation, LedgerError> {
    let mut entries = entries.lock().await;
    let reservation = if let Some(entry) = entries.get_mut(jti) {
        match entry.state {
            EphemeralLedgerState::Completed => {
                if entry.intent_hash == intent_hash {
                    entry.response.clone().map_or_else(
                        || {
                            Err(LedgerError::RowMalformed {
                                reason: format!("completed jti {jti} has no stored response"),
                            })
                        },
                        |response| Ok(Reservation::Completed(response)),
                    )
                } else {
                    Ok(Reservation::IntentConflict)
                }
            }
            EphemeralLedgerState::Pending => {
                if entry.intent_hash == intent_hash {
                    let ttl_ms =
                        i64::try_from(PENDING_RESERVATION_TTL.as_millis()).unwrap_or(i64::MAX);
                    if now_ms.saturating_sub(entry.reserved_at_ms) >= ttl_ms {
                        entry.reserved_at_ms = now_ms;
                        Ok(Reservation::Fresh)
                    } else {
                        Ok(Reservation::Pending)
                    }
                } else {
                    Ok(Reservation::IntentConflict)
                }
            }
        }
    } else {
        entries.insert(
            jti.to_owned(),
            EphemeralLedgerEntry {
                intent_hash: intent_hash.to_owned(),
                state: EphemeralLedgerState::Pending,
                response: None,
                reserved_at_ms: now_ms,
            },
        );
        Ok(Reservation::Fresh)
    };
    drop(entries);
    reservation
}

/// One `usage_ledger` row read back during a reserve.
struct LedgerRow {
    intent_hash: String,
    state: String,
    response_json: Option<String>,
    reserved_at_ms: i64,
}

/// Classify an existing `jti` row against the reserving `intent_hash`, reclaiming
/// an expired pending reservation in place.
async fn classify_existing(
    transaction: &libsql::Transaction,
    jti: &str,
    intent_hash: &str,
    now_ms: i64,
    row: LedgerRow,
) -> Result<Reservation, LedgerError> {
    if row.state == "completed" {
        if row.intent_hash != intent_hash {
            return Ok(Reservation::IntentConflict);
        }
        let json = row.response_json.ok_or_else(|| LedgerError::RowMalformed {
            reason: format!("completed jti {jti} has no stored payload"),
        })?;
        let response = serde_json::from_str(&json).map_err(|err| LedgerError::RowMalformed {
            reason: format!("stored payload for jti {jti} did not parse: {err}"),
        })?;
        return Ok(Reservation::Completed(response));
    }
    if row.intent_hash != intent_hash {
        return Ok(Reservation::IntentConflict);
    }
    let ttl_ms = i64::try_from(PENDING_RESERVATION_TTL.as_millis()).unwrap_or(i64::MAX);
    if now_ms.saturating_sub(row.reserved_at_ms) >= ttl_ms {
        transaction
            .execute(
                "UPDATE usage_ledger SET reserved_at_ms = ?1 WHERE jti = ?2",
                params![now_ms, jti],
            )
            .await
            .map_err(statement)?;
        return Ok(Reservation::Fresh);
    }
    Ok(Reservation::Pending)
}

async fn read_entry(
    transaction: &libsql::Transaction,
    jti: &str,
) -> Result<Option<LedgerRow>, LedgerError> {
    let mut rows = transaction
        .query(
            "SELECT intent_hash, state, response_json, reserved_at_ms \
             FROM usage_ledger WHERE jti = ?1",
            params![jti],
        )
        .await
        .map_err(statement)?;
    let Some(row) = rows.next().await.map_err(statement)? else {
        return Ok(None);
    };
    Ok(Some(LedgerRow {
        intent_hash: row.get(0).map_err(row_malformed)?,
        state: row.get(1).map_err(row_malformed)?,
        response_json: row.get(2).map_err(row_malformed)?,
        reserved_at_ms: row.get(3).map_err(row_malformed)?,
    }))
}

async fn run_migrations(connection: &StoreConnection) -> Result<(), LedgerError> {
    connection
        .execute_transactional_batch(INITIAL_SCHEMA_SQL)
        .await
        .map_err(statement)?;
    let applied = max_applied_version(connection).await?;
    if applied > SCHEMA_VERSION {
        return Err(LedgerError::SchemaTooNew { found: applied });
    }
    Ok(())
}

async fn max_applied_version(connection: &StoreConnection) -> Result<u32, LedgerError> {
    let mut rows = connection
        .query(
            "SELECT COALESCE(MAX(version), 0) FROM zspend_schema_migrations",
            params![],
        )
        .await
        .map_err(statement)?;
    let row = rows
        .next()
        .await
        .map_err(statement)?
        .ok_or_else(|| LedgerError::RowMalformed {
            reason: "migration table missing after batch apply".to_owned(),
        })?;
    let raw: i64 = row.get(0).map_err(row_malformed)?;
    u32::try_from(raw).map_err(|_| LedgerError::RowMalformed {
        reason: "migration version overflowed u32".to_owned(),
    })
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "used as a map_err adapter, which passes the error by value"
)]
fn statement(err: libsql::Error) -> LedgerError {
    LedgerError::Statement {
        reason: err.to_string(),
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "used as a map_err adapter, which passes the error by value"
)]
fn row_malformed(err: libsql::Error) -> LedgerError {
    LedgerError::RowMalformed {
        reason: err.to_string(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::{LedgerError, PENDING_RESERVATION_TTL, Reservation, UsageLedger};
    use crate::{AmountWire, ExpiresAtWire, SignPaymentResponse, SignedPayloadWire};
    use tempfile::TempDir;

    const JTI: &str = "01ACCESSTOKENJTI0000000000";
    const INTENT: &str = "v1:sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const OTHER_INTENT: &str = "v1:sha256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    fn fixture_response(tx_id: &str) -> SignPaymentResponse {
        SignPaymentResponse {
            signed_payload: SignedPayloadWire {
                format: "pczt-v2-extractable".to_owned(),
                bytes: "AAAA".to_owned(),
                tx_id: tx_id.to_owned(),
                fee: AmountWire {
                    currency: "ZEC".to_owned(),
                    value: "0".to_owned(),
                    unit: "base".to_owned(),
                },
                expires_at: ExpiresAtWire::BlockHeight(4_047_100),
                metadata: serde_json::Value::Null,
            },
        }
    }

    async fn ledger_in(dir: &TempDir) -> Result<UsageLedger, Box<dyn std::error::Error>> {
        let url = format!("file:{}", dir.path().join("usage-ledger.db").display());
        Ok(UsageLedger::open(&url, None).await?)
    }

    #[tokio::test]
    async fn first_reserve_is_fresh() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let ledger = ledger_in(&dir).await?;
        assert!(matches!(
            ledger.reserve(JTI, INTENT).await?,
            Reservation::Fresh
        ));
        Ok(())
    }

    #[tokio::test]
    async fn reserve_before_commit_is_pending_for_same_intent()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let ledger = ledger_in(&dir).await?;
        assert!(matches!(
            ledger.reserve(JTI, INTENT).await?,
            Reservation::Fresh
        ));
        assert!(matches!(
            ledger.reserve(JTI, INTENT).await?,
            Reservation::Pending
        ));
        Ok(())
    }

    #[tokio::test]
    async fn reserve_with_different_intent_conflicts_before_commit()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let ledger = ledger_in(&dir).await?;
        assert!(matches!(
            ledger.reserve(JTI, INTENT).await?,
            Reservation::Fresh
        ));
        assert!(matches!(
            ledger.reserve(JTI, OTHER_INTENT).await?,
            Reservation::IntentConflict
        ));
        Ok(())
    }

    #[tokio::test]
    async fn identical_replay_after_commit_returns_cached_payload()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let ledger = ledger_in(&dir).await?;
        assert!(matches!(
            ledger.reserve(JTI, INTENT).await?,
            Reservation::Fresh
        ));
        ledger
            .commit(JTI, INTENT, &fixture_response("deadbeef"))
            .await?;
        let cached = match ledger.reserve(JTI, INTENT).await? {
            Reservation::Completed(cached) => cached.signed_payload.tx_id,
            Reservation::Fresh | Reservation::IntentConflict | Reservation::Pending => {
                String::new()
            }
        };
        assert_eq!(cached, "deadbeef", "replay must return the cached payload");
        Ok(())
    }

    #[tokio::test]
    async fn different_intent_after_commit_conflicts() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let ledger = ledger_in(&dir).await?;
        ledger.reserve(JTI, INTENT).await?;
        ledger
            .commit(JTI, INTENT, &fixture_response("deadbeef"))
            .await?;
        assert!(matches!(
            ledger.reserve(JTI, OTHER_INTENT).await?,
            Reservation::IntentConflict
        ));
        Ok(())
    }

    #[tokio::test]
    async fn release_lets_a_retry_reserve_fresh() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let ledger = ledger_in(&dir).await?;
        ledger.reserve(JTI, INTENT).await?;
        ledger.release(JTI).await?;
        assert!(matches!(
            ledger.reserve(JTI, INTENT).await?,
            Reservation::Fresh
        ));
        Ok(())
    }

    #[tokio::test]
    async fn release_does_not_drop_a_committed_entry() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let ledger = ledger_in(&dir).await?;
        ledger.reserve(JTI, INTENT).await?;
        ledger
            .commit(JTI, INTENT, &fixture_response("deadbeef"))
            .await?;
        ledger.release(JTI).await?;
        assert!(matches!(
            ledger.reserve(JTI, INTENT).await?,
            Reservation::Completed(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn committed_row_survives_reopen() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let url = format!("file:{}", dir.path().join("usage-ledger.db").display());
        {
            let ledger = UsageLedger::open(&url, None).await?;
            ledger.reserve(JTI, INTENT).await?;
            ledger
                .commit(JTI, INTENT, &fixture_response("cafef00d"))
                .await?;
        }
        let reopened = UsageLedger::open(&url, None).await?;
        let cached = match reopened.reserve(JTI, INTENT).await? {
            Reservation::Completed(cached) => cached.signed_payload.tx_id,
            Reservation::Fresh | Reservation::IntentConflict | Reservation::Pending => {
                String::new()
            }
        };
        assert_eq!(
            cached, "cafef00d",
            "a committed reservation must survive a process restart",
        );
        Ok(())
    }

    #[tokio::test]
    async fn pending_reservation_is_reclaimable_after_ttl() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let ledger = ledger_in(&dir).await?;
        let ttl_ms = i64::try_from(PENDING_RESERVATION_TTL.as_millis())?;
        // Reserve at a base instant, then reserve again one TTL later so the
        // pending row is past its reclaim window.
        assert!(matches!(
            ledger.reserve_at(JTI, INTENT, 1_000_000).await?,
            Reservation::Fresh
        ));
        assert!(matches!(
            ledger.reserve_at(JTI, INTENT, 1_000_000 + ttl_ms).await?,
            Reservation::Fresh
        ));
        Ok(())
    }

    #[tokio::test]
    async fn pending_reservation_is_held_before_ttl() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let ledger = ledger_in(&dir).await?;
        let ttl_ms = i64::try_from(PENDING_RESERVATION_TTL.as_millis())?;
        assert!(matches!(
            ledger.reserve_at(JTI, INTENT, 1_000_000).await?,
            Reservation::Fresh
        ));
        assert!(matches!(
            ledger
                .reserve_at(JTI, INTENT, 1_000_000 + ttl_ms - 1)
                .await?,
            Reservation::Pending
        ));
        Ok(())
    }

    #[tokio::test]
    async fn remote_url_without_auth_token_fails_closed() {
        let outcome = UsageLedger::open("libsql://example.turso.io", None).await;
        assert!(matches!(
            outcome,
            Err(LedgerError::MissingAuthToken { url }) if url == "libsql://example.turso.io"
        ));
    }

    #[tokio::test]
    async fn local_path_ignores_a_supplied_auth_token() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let url = format!("file:{}", dir.path().join("usage-ledger.db").display());
        UsageLedger::open(&url, Some("unused-token")).await?;
        Ok(())
    }

    /// Regression test for a startup panic under a real multi-thread runtime.
    ///
    /// `#[tokio::test]`'s default single-threaded flavor cannot reproduce a
    /// `sqlite3_config` misuse that only surfaces under `rt-multi-thread`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reserve_commit_replay_survives_multi_thread_runtime()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let ledger = ledger_in(&dir).await?;
        assert!(matches!(
            ledger.reserve(JTI, INTENT).await?,
            Reservation::Fresh
        ));
        ledger
            .commit(JTI, INTENT, &fixture_response("deadbeef"))
            .await?;
        let cached = match ledger.reserve(JTI, INTENT).await? {
            Reservation::Completed(cached) => cached.signed_payload.tx_id,
            Reservation::Fresh | Reservation::IntentConflict | Reservation::Pending => {
                String::new()
            }
        };
        assert_eq!(cached, "deadbeef", "replay must return the cached payload");
        Ok(())
    }
}
