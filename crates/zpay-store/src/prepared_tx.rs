//! libSQL implementation of [`PreparedTxStore`].

use libsql::params;

use zpay_core::prepare::{Preparation, PreparedTxEntry, PreparedTxStore};
use zpay_core::store::StoreError;
use zpay_core::types::{MerchantId, PaymentId, PaymentNetwork, Zatoshis};

use crate::connection::StoreConnection;

/// libSQL-backed prepared-tx store.
///
/// Persists every prepared payment into the `prepared_tx` table per
/// the 0001 migration. The schema mirrors [`PreparedTxEntry`] exactly;
/// columns the entry does not carry (`agent_dpop_jkt`,
/// `evidence_pack_hash`) land with their own migrations when the
/// PRD-42 phases that need them go in.
#[derive(Clone)]
pub struct LibsqlPreparedTxStore {
    connection: StoreConnection,
}

impl LibsqlPreparedTxStore {
    /// Wrap an open [`StoreConnection`].
    #[must_use]
    pub const fn new(connection: StoreConnection) -> Self {
        Self { connection }
    }
}

impl PreparedTxStore for LibsqlPreparedTxStore {
    async fn insert(&self, entry: PreparedTxEntry) -> Result<(), StoreError> {
        // The `payment_id` is the primary key; on retry of the same
        // `(merchant_id, idempotency_key)` pair, the caller resolves
        // via `find_by_idempotency` first and never gets here.
        // `ON CONFLICT (payment_id) DO UPDATE` mirrors the in-memory
        // impl's `HashMap::insert` replace-on-collision behaviour.
        let network = network_to_sql(entry.network);
        let memo_bytes = entry.preparation.memo_bytes.clone();
        let amount_zat = i64::try_from(entry.amount_zat.0).map_err(|_| {
            StoreError::IntegrityViolation {
                constraint: "amount_zat exceeds i64".to_owned(),
            }
        })?;
        let expiry_height = i64::from(entry.preparation.expiry_height);
        let expires_at = i64::try_from(entry.expires_at_unix_seconds).map_err(|_| {
            StoreError::IntegrityViolation {
                constraint: "expires_at_unix_seconds exceeds i64".to_owned(),
            }
        })?;

        self.connection
            .execute(
                "INSERT INTO prepared_tx (\
                    payment_id, merchant_id, network, recipient_unified_address, \
                    amount_zat, memo_bytes, expiry_height, idempotency_key, \
                    expires_at_unix_seconds\
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
                ON CONFLICT(payment_id) DO UPDATE SET \
                    merchant_id = excluded.merchant_id, \
                    network = excluded.network, \
                    recipient_unified_address = excluded.recipient_unified_address, \
                    amount_zat = excluded.amount_zat, \
                    memo_bytes = excluded.memo_bytes, \
                    expiry_height = excluded.expiry_height, \
                    idempotency_key = excluded.idempotency_key, \
                    expires_at_unix_seconds = excluded.expires_at_unix_seconds",
                params![
                    entry.preparation.payment_id.0.clone(),
                    entry.merchant_id.0.clone(),
                    network,
                    entry.recipient_unified_address.clone(),
                    amount_zat,
                    memo_bytes,
                    expiry_height,
                    entry.idempotency_key.clone(),
                    expires_at,
                ],
            )
            .await
            .map_err(|err| libsql_to_store_error(&err))?;
        Ok(())
    }

    async fn find_by_payment_id(
        &self,
        payment_id: &PaymentId,
    ) -> Result<Option<PreparedTxEntry>, StoreError> {
        let mut rows = self
            .connection
            .query(
                "SELECT payment_id, merchant_id, network, recipient_unified_address, \
                    amount_zat, memo_bytes, expiry_height, idempotency_key, \
                    expires_at_unix_seconds \
                FROM prepared_tx WHERE payment_id = ?",
                params![payment_id.0.clone()],
            )
            .await
            .map_err(|err| libsql_to_store_error(&err))?;
        let Some(row) = rows.next().await.map_err(|err| libsql_to_store_error(&err))? else {
            return Ok(None);
        };
        Ok(Some(row_to_prepared_tx_entry(&row)?))
    }

    async fn find_by_idempotency(
        &self,
        merchant_id: &MerchantId,
        idempotency_key: &str,
    ) -> Result<Option<PreparedTxEntry>, StoreError> {
        let mut rows = self
            .connection
            .query(
                "SELECT payment_id, merchant_id, network, recipient_unified_address, \
                    amount_zat, memo_bytes, expiry_height, idempotency_key, \
                    expires_at_unix_seconds \
                FROM prepared_tx \
                WHERE merchant_id = ? AND idempotency_key = ?",
                params![merchant_id.0.clone(), idempotency_key.to_owned()],
            )
            .await
            .map_err(|err| libsql_to_store_error(&err))?;
        let Some(row) = rows.next().await.map_err(|err| libsql_to_store_error(&err))? else {
            return Ok(None);
        };
        Ok(Some(row_to_prepared_tx_entry(&row)?))
    }

    async fn remove(
        &self,
        payment_id: &PaymentId,
    ) -> Result<Option<PreparedTxEntry>, StoreError> {
        let existing = self.find_by_payment_id(payment_id).await?;
        if existing.is_some() {
            self.connection
                .execute(
                    "DELETE FROM prepared_tx WHERE payment_id = ?",
                    params![payment_id.0.clone()],
                )
                .await
                .map_err(|err| libsql_to_store_error(&err))?;
        }
        Ok(existing)
    }

    async fn sweep_expired(&self, now_unix_seconds: u64) -> Result<usize, StoreError> {
        let now = i64::try_from(now_unix_seconds).map_err(|_| {
            StoreError::IntegrityViolation {
                constraint: "now_unix_seconds exceeds i64".to_owned(),
            }
        })?;
        let dropped = self
            .connection
            .execute(
                "DELETE FROM prepared_tx WHERE expires_at_unix_seconds <= ?",
                params![now],
            )
            .await
            .map_err(|err| libsql_to_store_error(&err))?;
        usize::try_from(dropped).map_err(|_| StoreError::RowMalformed {
            reason: "delete count overflowed usize".to_owned(),
        })
    }

    async fn entry_count(&self) -> Result<usize, StoreError> {
        let mut rows = self
            .connection
            .query("SELECT COUNT(*) FROM prepared_tx", params![])
            .await
            .map_err(|err| libsql_to_store_error(&err))?;
        let row = rows
            .next()
            .await
            .map_err(|err| libsql_to_store_error(&err))?
            .ok_or_else(|| StoreError::Unavailable {
                reason: "count query returned no row".to_owned(),
            })?;
        let raw: i64 = row.get(0).map_err(|err| StoreError::RowMalformed {
            reason: format!("count column non-integer: {err}"),
        })?;
        usize::try_from(raw).map_err(|_| StoreError::RowMalformed {
            reason: "count overflowed usize".to_owned(),
        })
    }
}

fn row_to_prepared_tx_entry(row: &libsql::Row) -> Result<PreparedTxEntry, StoreError> {
    let payment_id: String = row.get(0).map_err(|err| StoreError::RowMalformed {
        reason: format!("payment_id read failed: {err}"),
    })?;
    let merchant_id: String = row.get(1).map_err(|err| StoreError::RowMalformed {
        reason: format!("merchant_id read failed: {err}"),
    })?;
    let network: String = row.get(2).map_err(|err| StoreError::RowMalformed {
        reason: format!("network read failed: {err}"),
    })?;
    let recipient_unified_address: String =
        row.get(3).map_err(|err| StoreError::RowMalformed {
            reason: format!("recipient_unified_address read failed: {err}"),
        })?;
    let amount_zat_raw: i64 = row.get(4).map_err(|err| StoreError::RowMalformed {
        reason: format!("amount_zat read failed: {err}"),
    })?;
    let memo_bytes_raw: libsql::Value = row.get(5).map_err(|err| StoreError::RowMalformed {
        reason: format!("memo_bytes read failed: {err}"),
    })?;
    let memo_bytes = match memo_bytes_raw {
        libsql::Value::Blob(bytes) => bytes,
        #[allow(
            clippy::wildcard_in_or_patterns,
            reason = "every other Value variant means a schema drift; surfacing the variant in the error keeps operator triage cheap"
        )]
        other @ (libsql::Value::Null
        | libsql::Value::Integer(_)
        | libsql::Value::Real(_)
        | libsql::Value::Text(_)) => {
            return Err(StoreError::RowMalformed {
                reason: format!("memo_bytes wrong sql type: {other:?}"),
            });
        }
    };
    let expiry_height_raw: i64 = row.get(6).map_err(|err| StoreError::RowMalformed {
        reason: format!("expiry_height read failed: {err}"),
    })?;
    let idempotency_key: Option<String> =
        row.get(7).map_err(|err| StoreError::RowMalformed {
            reason: format!("idempotency_key read failed: {err}"),
        })?;
    let expires_at_raw: i64 = row.get(8).map_err(|err| StoreError::RowMalformed {
        reason: format!("expires_at_unix_seconds read failed: {err}"),
    })?;

    Ok(PreparedTxEntry {
        preparation: Preparation {
            payment_id: PaymentId(payment_id),
            payment_uri: String::new(),
            memo_bytes,
            expiry_height: u32::try_from(expiry_height_raw).map_err(|_| {
                StoreError::RowMalformed {
                    reason: "expiry_height does not fit u32".to_owned(),
                }
            })?,
        },
        merchant_id: MerchantId(merchant_id),
        network: sql_to_network(&network)?,
        recipient_unified_address,
        amount_zat: Zatoshis(u64::try_from(amount_zat_raw).map_err(|_| {
            StoreError::RowMalformed {
                reason: "amount_zat does not fit u64".to_owned(),
            }
        })?),
        expires_at_unix_seconds: u64::try_from(expires_at_raw).map_err(|_| {
            StoreError::RowMalformed {
                reason: "expires_at_unix_seconds does not fit u64".to_owned(),
            }
        })?,
        idempotency_key,
    })
}

fn network_to_sql(network: PaymentNetwork) -> &'static str {
    match network {
        PaymentNetwork::Mainnet => "mainnet",
        PaymentNetwork::Testnet => "testnet",
        PaymentNetwork::Regtest => "regtest",
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "PaymentNetwork is #[non_exhaustive]; the schema CHECK guards unknown values"
        )]
        _ => "unknown",
    }
}

fn sql_to_network(raw: &str) -> Result<PaymentNetwork, StoreError> {
    match raw {
        "mainnet" => Ok(PaymentNetwork::Mainnet),
        "testnet" => Ok(PaymentNetwork::Testnet),
        "regtest" => Ok(PaymentNetwork::Regtest),
        other => Err(StoreError::RowMalformed {
            reason: format!("unknown network: {other}"),
        }),
    }
}

fn libsql_to_store_error(err: &libsql::Error) -> StoreError {
    let message = err.to_string();
    if message.contains("UNIQUE") {
        return StoreError::IntegrityViolation { constraint: message };
    }
    StoreError::Unavailable { reason: message }
}
