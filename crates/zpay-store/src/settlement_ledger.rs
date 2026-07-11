//! libSQL implementation of [`SettlementLedgerStore`].

use libsql::params;

use zpay_core::broadcast::BroadcastOutcome;
use zpay_core::status::{SettlementLedgerEntry, SettlementLedgerStore, SuccessKindRow};
use zpay_core::store::StoreError;
use zpay_core::types::{PayeeId, PaymentId, Zatoshis};

use crate::connection::StoreConnection;

/// libSQL-backed settlement ledger.
///
/// The `(broadcast_outcome_kind, transaction_id, upstream_message)`
/// triple flatten-encodes the [`BroadcastOutcome`] enum so the
/// upstream column shape stays SQL-readable for operators.
#[derive(Clone)]
pub struct LibsqlSettlementLedgerStore {
    connection: StoreConnection,
}

impl LibsqlSettlementLedgerStore {
    /// Wrap an open [`StoreConnection`].
    #[must_use]
    pub const fn new(connection: StoreConnection) -> Self {
        Self { connection }
    }
}

impl SettlementLedgerStore for LibsqlSettlementLedgerStore {
    async fn record(
        &self,
        payment_id: PaymentId,
        entry: SettlementLedgerEntry,
    ) -> Result<(), StoreError> {
        let (kind, transaction_id, upstream_message) =
            encode_broadcast_outcome(&entry.broadcast_outcome);
        let confirmation_count = entry.confirmation_count.map(i64::from);
        let mined_block_height = entry
            .mined_block_height
            .map(|height| i64::try_from(height).unwrap_or(i64::MAX));
        let reorg_count = i64::from(entry.reorg_count);
        let expiry_height = entry.expiry_height.map(i64::from);

        self.connection
            .execute(
                "INSERT INTO settlement_ledger (\
                    payment_id, broadcast_outcome_kind, transaction_id, upstream_message, \
                    settled_at_unix_seconds, confirmation_count, mined_block_height, \
                    last_confirmation_check_at_unix_seconds, reorg_count, last_reorged_at, \
                    expiry_height, payee_id, amount_zat\
                ) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?) \
                ON CONFLICT(payment_id) DO UPDATE SET \
                    broadcast_outcome_kind = excluded.broadcast_outcome_kind, \
                    transaction_id = excluded.transaction_id, \
                    upstream_message = excluded.upstream_message, \
                    settled_at_unix_seconds = excluded.settled_at_unix_seconds, \
                    confirmation_count = excluded.confirmation_count, \
                    mined_block_height = excluded.mined_block_height, \
                    last_confirmation_check_at_unix_seconds = NULL, \
                    reorg_count = excluded.reorg_count, \
                    last_reorged_at = excluded.last_reorged_at, \
                    expiry_height = excluded.expiry_height, \
                    payee_id = excluded.payee_id, \
                    amount_zat = excluded.amount_zat",
                params![
                    payment_id.0,
                    kind,
                    transaction_id,
                    upstream_message,
                    entry.settled_at_unix_seconds,
                    confirmation_count,
                    mined_block_height,
                    reorg_count,
                    entry.last_reorged_at,
                    expiry_height,
                    entry.payee_id.0.clone(),
                    i64::try_from(entry.amount_zat.0).unwrap_or(i64::MAX),
                ],
            )
            .await
            .map_err(|error| StoreConnection::to_store_error(&error))?;
        Ok(())
    }

    async fn find(
        &self,
        payment_id: &PaymentId,
    ) -> Result<Option<SettlementLedgerEntry>, StoreError> {
        let mut rows = self
            .connection
            .query(
                "SELECT broadcast_outcome_kind, transaction_id, upstream_message, \
                    settled_at_unix_seconds, confirmation_count, mined_block_height, \
                    reorg_count, last_reorged_at, expiry_height, payee_id, amount_zat \
                FROM settlement_ledger WHERE payment_id = ?",
                params![payment_id.0.clone()],
            )
            .await
            .map_err(|error| StoreConnection::to_store_error(&error))?;
        let Some(row) = rows
            .next()
            .await
            .map_err(|error| StoreConnection::to_store_error(&error))?
        else {
            return Ok(None);
        };
        Ok(Some(row_to_settlement_ledger_entry(&row, 0)?))
    }

    async fn entry_count(&self) -> Result<usize, StoreError> {
        self.connection
            .entry_count("SELECT COUNT(*) FROM settlement_ledger")
            .await
    }

    async fn success_kind_transactions(&self) -> Result<Vec<SuccessKindRow>, StoreError> {
        let mut rows = self
            .connection
            .query(
                "SELECT payment_id, transaction_id, mined_block_height \
                FROM settlement_ledger \
                WHERE broadcast_outcome_kind IN ('accepted', 'duplicate') \
                  AND transaction_id IS NOT NULL",
                params![],
            )
            .await
            .map_err(|error| StoreConnection::to_store_error(&error))?;
        let mut collected = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|error| StoreConnection::to_store_error(&error))?
        {
            let payment_id: String = row.get(0).map_err(|err| StoreError::RowMalformed {
                reason: format!("payment_id read failed: {err}"),
            })?;
            let transaction_id: String = row.get(1).map_err(|err| StoreError::RowMalformed {
                reason: format!("transaction_id read failed: {err}"),
            })?;
            let mined_block_height: Option<i64> =
                row.get(2).map_err(|err| StoreError::RowMalformed {
                    reason: format!("mined_block_height read failed: {err}"),
                })?;
            collected.push(SuccessKindRow {
                payment_id: PaymentId(payment_id),
                transaction_id,
                mined_block_height: mined_block_height.map(|raw| u64::try_from(raw).unwrap_or(0)),
            });
        }
        Ok(collected)
    }

    async fn list_recent(
        &self,
        limit: u32,
        payee_id: Option<&PayeeId>,
    ) -> Result<Vec<(PaymentId, SettlementLedgerEntry)>, StoreError> {
        const COLUMNS: &str = "payment_id, broadcast_outcome_kind, transaction_id, upstream_message, \
            settled_at_unix_seconds, confirmation_count, mined_block_height, \
            reorg_count, last_reorged_at, expiry_height, payee_id, amount_zat";
        let limit_i64 = i64::from(limit);
        let mut rows = if let Some(payee_id) = payee_id {
            self.connection
                .query(
                    &format!(
                        "SELECT {COLUMNS} FROM settlement_ledger \
                        WHERE payee_id = ? ORDER BY payment_id DESC LIMIT ?"
                    ),
                    params![payee_id.0.clone(), limit_i64],
                )
                .await
        } else {
            self.connection
                .query(
                    &format!("SELECT {COLUMNS} FROM settlement_ledger ORDER BY payment_id DESC LIMIT ?"),
                    params![limit_i64],
                )
                .await
        }
        .map_err(|error| StoreConnection::to_store_error(&error))?;

        let mut collected = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|error| StoreConnection::to_store_error(&error))?
        {
            let payment_id: String = row.get(0).map_err(|err| StoreError::RowMalformed {
                reason: format!("payment_id read failed: {err}"),
            })?;
            let entry = row_to_settlement_ledger_entry(&row, 1)?;
            collected.push((PaymentId(payment_id), entry));
        }
        Ok(collected)
    }

    async fn record_confirmation(
        &self,
        payment_id: &PaymentId,
        confirmation_count: u32,
        mined_block_height: Option<u64>,
    ) -> Result<bool, StoreError> {
        let confirmation_count_i64 = i64::from(confirmation_count);
        let mined_block_height_i64 =
            mined_block_height.map(|height| i64::try_from(height).unwrap_or(i64::MAX));
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs()),
        )
        .unwrap_or(i64::MAX);
        // When the caller does not know the mined block height (e.g.
        // the tx is in mempool) we must not overwrite a previously
        // recorded height. COALESCE(?, mined_block_height) preserves
        // the prior value on a NULL update.
        let affected = self
            .connection
            .execute(
                "UPDATE settlement_ledger SET \
                    confirmation_count = ?, \
                    mined_block_height = COALESCE(?, mined_block_height), \
                    last_confirmation_check_at_unix_seconds = ? \
                WHERE payment_id = ?",
                params![
                    confirmation_count_i64,
                    mined_block_height_i64,
                    now,
                    payment_id.0.clone(),
                ],
            )
            .await
            .map_err(|error| StoreConnection::to_store_error(&error))?;
        Ok(affected > 0)
    }

    async fn downgrade_on_reorg(
        &self,
        payment_id: &PaymentId,
        reorged_at_unix_seconds: i64,
    ) -> Result<bool, StoreError> {
        let affected = self
            .connection
            .execute(
                "UPDATE settlement_ledger SET \
                    mined_block_height = NULL, \
                    confirmation_count = 0, \
                    reorg_count = reorg_count + 1, \
                    last_reorged_at = ? \
                WHERE payment_id = ? AND mined_block_height IS NOT NULL",
                params![reorged_at_unix_seconds, payment_id.0.clone()],
            )
            .await
            .map_err(|error| StoreConnection::to_store_error(&error))?;
        Ok(affected > 0)
    }

    async fn downgrade_reorged_range(
        &self,
        reverted_start_height: u64,
        reverted_end_height: u64,
        reorged_at_unix_seconds: i64,
    ) -> Result<Vec<PaymentId>, StoreError> {
        let start = i64::try_from(reverted_start_height).unwrap_or(i64::MAX);
        let end = i64::try_from(reverted_end_height).unwrap_or(i64::MAX);
        let mut rows = self
            .connection
            .query(
                "UPDATE settlement_ledger SET \
                    mined_block_height = NULL, \
                    confirmation_count = 0, \
                    reorg_count = reorg_count + 1, \
                    last_reorged_at = ? \
                WHERE mined_block_height IS NOT NULL \
                  AND mined_block_height BETWEEN ? AND ? \
                RETURNING payment_id",
                params![reorged_at_unix_seconds, start, end],
            )
            .await
            .map_err(|error| StoreConnection::to_store_error(&error))?;
        let mut downgraded = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|error| StoreConnection::to_store_error(&error))?
        {
            let payment_id: String = row.get(0).map_err(|err| StoreError::RowMalformed {
                reason: format!("payment_id read failed: {err}"),
            })?;
            downgraded.push(PaymentId(payment_id));
        }
        Ok(downgraded)
    }
}

fn encode_broadcast_outcome(
    outcome: &BroadcastOutcome,
) -> (&'static str, Option<String>, Option<String>) {
    match outcome {
        BroadcastOutcome::Accepted { transaction_id } => {
            ("accepted", Some(transaction_id.clone()), None)
        }
        BroadcastOutcome::Duplicate { upstream_message } => {
            ("duplicate", None, Some(upstream_message.clone()))
        }
        BroadcastOutcome::InvalidEncoding { upstream_message } => {
            ("invalid_encoding", None, Some(upstream_message.clone()))
        }
        BroadcastOutcome::Rejected { upstream_message } => {
            ("rejected", None, Some(upstream_message.clone()))
        }
        BroadcastOutcome::Unknown { upstream_message } => {
            ("unknown", None, Some(upstream_message.clone()))
        }
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "BroadcastOutcome is #[non_exhaustive]; future variants persist as Unknown until they have an explicit row shape"
        )]
        _ => (
            "unknown",
            None,
            Some("unrecognised broadcast outcome variant".to_owned()),
        ),
    }
}

fn row_to_settlement_ledger_entry(
    row: &libsql::Row,
    offset: i32,
) -> Result<SettlementLedgerEntry, StoreError> {
    let kind: String = row.get(offset).map_err(|err| StoreError::RowMalformed {
        reason: format!("broadcast_outcome_kind read failed: {err}"),
    })?;
    let transaction_id: Option<String> =
        row.get(offset + 1).map_err(|err| StoreError::RowMalformed {
            reason: format!("transaction_id read failed: {err}"),
        })?;
    let upstream_message: Option<String> =
        row.get(offset + 2).map_err(|err| StoreError::RowMalformed {
            reason: format!("upstream_message read failed: {err}"),
        })?;
    let settled_at_unix_seconds: i64 =
        row.get(offset + 3).map_err(|err| StoreError::RowMalformed {
            reason: format!("settled_at_unix_seconds read failed: {err}"),
        })?;
    let confirmation_count: Option<i64> =
        row.get(offset + 4).map_err(|err| StoreError::RowMalformed {
            reason: format!("confirmation_count read failed: {err}"),
        })?;
    let mined_block_height: Option<i64> =
        row.get(offset + 5).map_err(|err| StoreError::RowMalformed {
            reason: format!("mined_block_height read failed: {err}"),
        })?;
    let reorg_count: i64 = row.get(offset + 6).map_err(|err| StoreError::RowMalformed {
        reason: format!("reorg_count read failed: {err}"),
    })?;
    let last_reorged_at: Option<i64> =
        row.get(offset + 7).map_err(|err| StoreError::RowMalformed {
            reason: format!("last_reorged_at read failed: {err}"),
        })?;
    let expiry_height: Option<i64> =
        row.get(offset + 8).map_err(|err| StoreError::RowMalformed {
            reason: format!("expiry_height read failed: {err}"),
        })?;
    let payee_id: String = row.get(offset + 9).map_err(|err| StoreError::RowMalformed {
        reason: format!("payee_id read failed: {err}"),
    })?;
    let amount_zat: i64 = row.get(offset + 10).map_err(|err| StoreError::RowMalformed {
        reason: format!("amount_zat read failed: {err}"),
    })?;

    let broadcast_outcome = match kind.as_str() {
        "accepted" => BroadcastOutcome::Accepted {
            transaction_id: transaction_id.ok_or_else(|| StoreError::RowMalformed {
                reason: "accepted row missing transaction_id".to_owned(),
            })?,
        },
        "duplicate" => BroadcastOutcome::Duplicate {
            upstream_message: upstream_message.unwrap_or_default(),
        },
        "invalid_encoding" => BroadcastOutcome::InvalidEncoding {
            upstream_message: upstream_message.unwrap_or_default(),
        },
        "rejected" => BroadcastOutcome::Rejected {
            upstream_message: upstream_message.unwrap_or_default(),
        },
        "unknown" => BroadcastOutcome::Unknown {
            upstream_message: upstream_message.unwrap_or_default(),
        },
        other => {
            return Err(StoreError::RowMalformed {
                reason: format!("unknown broadcast_outcome_kind: {other}"),
            });
        }
    };

    Ok(SettlementLedgerEntry {
        broadcast_outcome,
        settled_at_unix_seconds,
        confirmation_count: confirmation_count.map(|raw| u32::try_from(raw).unwrap_or(u32::MAX)),
        mined_block_height: mined_block_height.map(|raw| u64::try_from(raw).unwrap_or(0)),
        reorg_count: u32::try_from(reorg_count).unwrap_or(u32::MAX),
        last_reorged_at,
        expiry_height: expiry_height.map(|raw| u32::try_from(raw).unwrap_or(u32::MAX)),
        payee_id: PayeeId(payee_id),
        amount_zat: Zatoshis(u64::try_from(amount_zat).unwrap_or(0)),
    })
}
