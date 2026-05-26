//! Auto-reconnecting libSQL connection wrapper.
//!
//! Turso closes idle Hrana streams server-side after their TTL. The
//! libSQL [`Connection`] keeps a reference to the dead stream; the
//! next query returns `stream not found: <stream-id>` and every
//! subsequent operation reuses the same dead handle. The failure mode
//! is silent at the application layer because the runtime is otherwise
//! healthy.
//!
//! [`StoreConnection`] wraps an underlying [`Connection`] with the
//! recovery contract this calls for: on a `stream not found` error it
//! rebuilds the libSQL client from the cached URL and auth token, then
//! retries the failed operation exactly once. A second `stream not
//! found` propagates so the caller does not loop forever on an
//! unrelated upstream outage.

use std::sync::Arc;

use libsql::{Builder, Connection, params::IntoParams};
use tokio::sync::RwLock;

use zpay_core::store::StoreError;

/// libSQL connection that survives Turso Hrana stream expiry.
///
/// Cloning a `StoreConnection` is cheap; clones share the same inner
/// [`Connection`] and the same reconnect serialization, so a recovery
/// triggered by one repository is observed by every other repository
/// that shares the handle.
#[derive(Clone)]
pub struct StoreConnection {
    inner: Arc<StoreConnectionInner>,
}

struct StoreConnectionInner {
    store_url: String,
    auth_token: Option<String>,
    connection: RwLock<Connection>,
}

impl StoreConnection {
    /// Open a connection from the production store URL shape.
    ///
    /// Accepts `file:<path>` for local `SQLite` and `libsql://<host>`
    /// for Turso.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Unavailable`] when the libSQL builder
    /// rejects the URL or fails to connect.
    pub async fn open(store_url: &str, auth_token: Option<&str>) -> Result<Self, StoreError> {
        let connection = build_connection(store_url, auth_token).await?;
        Ok(Self {
            inner: Arc::new(StoreConnectionInner {
                store_url: store_url.to_owned(),
                auth_token: auth_token.map(str::to_owned),
                connection: RwLock::new(connection),
            }),
        })
    }

    /// Execute a SELECT against the connection. On a `stream not
    /// found` error the wrapper rebuilds the libSQL client and retries
    /// the query once.
    pub async fn query(
        &self,
        sql: &str,
        params: impl IntoParams,
    ) -> Result<libsql::Rows, libsql::Error> {
        let params = params.into_params()?;
        let first_outcome = self
            .inner
            .connection
            .read()
            .await
            .query(sql, params.clone())
            .await;
        match first_outcome {
            Ok(rows) => Ok(rows),
            Err(err) if is_stream_not_found(&err) => {
                self.report_and_reconnect(&err).await;
                self.inner.connection.read().await.query(sql, params).await
            }
            Err(err) => Err(err),
        }
    }

    /// Execute an INSERT/UPDATE/DELETE statement. Same retry contract
    /// as [`Self::query`].
    pub async fn execute(
        &self,
        sql: &str,
        params: impl IntoParams,
    ) -> Result<u64, libsql::Error> {
        let params = params.into_params()?;
        let first_outcome = self
            .inner
            .connection
            .read()
            .await
            .execute(sql, params.clone())
            .await;
        match first_outcome {
            Ok(affected) => Ok(affected),
            Err(err) if is_stream_not_found(&err) => {
                self.report_and_reconnect(&err).await;
                self.inner
                    .connection
                    .read()
                    .await
                    .execute(sql, params)
                    .await
            }
            Err(err) => Err(err),
        }
    }

    /// Execute a transactional batch script (multiple statements in
    /// one transaction). Same retry contract as [`Self::query`].
    pub async fn execute_transactional_batch(&self, sql: &str) -> Result<(), libsql::Error> {
        let first_outcome = self
            .inner
            .connection
            .read()
            .await
            .execute_transactional_batch(sql)
            .await;
        match first_outcome {
            Ok(_) => Ok(()),
            Err(err) if is_stream_not_found(&err) => {
                self.report_and_reconnect(&err).await;
                self.inner
                    .connection
                    .read()
                    .await
                    .execute_transactional_batch(sql)
                    .await
                    .map(|_| ())
            }
            Err(err) => Err(err),
        }
    }

    async fn report_and_reconnect(&self, original_error: &libsql::Error) {
        tracing::warn!(
            event = "zpay_store_hrana_stream_lost",
            original_error = %original_error,
            "rebuilding libSQL client after Turso closed the Hrana stream",
        );
        if let Err(reconnect_failure) = self.reconnect().await {
            tracing::error!(
                event = "zpay_store_hrana_reconnect_failed",
                reconnect_failure = %reconnect_failure,
                "could not rebuild libSQL client; the next operation will surface the original error",
            );
        }
    }

    async fn reconnect(&self) -> Result<(), StoreError> {
        let fresh =
            build_connection(&self.inner.store_url, self.inner.auth_token.as_deref()).await?;
        *self.inner.connection.write().await = fresh;
        Ok(())
    }
}

async fn build_connection(
    store_url: &str,
    auth_token: Option<&str>,
) -> Result<Connection, StoreError> {
    let database = if let Some(file_path) = store_url.strip_prefix("file:") {
        Builder::new_local(file_path)
            .build()
            .await
            .map_err(|err| StoreError::Unavailable {
                reason: format!("libsql local builder failed for {file_path}: {err}"),
            })?
    } else if store_url.starts_with("libsql://") {
        let token = auth_token.ok_or_else(|| StoreError::Unavailable {
            reason: "remote libsql URL requires an auth token (set ZPAY_STORE__AUTH_TOKEN)"
                .to_owned(),
        })?;
        Builder::new_remote(store_url.to_owned(), token.to_owned())
            .build()
            .await
            .map_err(|err| StoreError::Unavailable {
                reason: format!("libsql remote builder failed for {store_url}: {err}"),
            })?
    } else {
        return Err(StoreError::Unavailable {
            reason: format!(
                "unsupported store URL: {store_url} (expected file:<path> or libsql://<host>)",
            ),
        });
    };

    database.connect().map_err(|err| StoreError::Unavailable {
        reason: format!("libsql connect failed: {err}"),
    })
}

/// Detect Hrana's "stream not found" error.
///
/// The Turso edge returns a 404 with a stream-id body; the libsql
/// client surfaces it via [`libsql::Error::Hrana`] whose [`Display`]
/// impl includes the literal text "stream not found". Matching on the
/// message avoids coupling to a specific `hrana_client_proto` error
/// variant.
fn is_stream_not_found(err: &libsql::Error) -> bool {
    err.to_string().contains("stream not found")
}
