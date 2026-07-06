//! zpay runtime binary: starts the HTTP listener, the ops listener, and the
//! signal handler.
//!
//! Configuration today is minimal: bind addresses and network read from
//! `ZPAY_*` env vars. The full layered config (TOML + env + CLI) lands in
//! M1; this scaffold only carries the env-var entry points needed to run
//! `/healthz` and the x402 stub routes.

mod chain_events;
mod rejecting_fetcher;
mod tip_oracle;
mod zinder_fetcher;
mod zinder_oracle;
mod zinder_submitter;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use clap::Parser;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use rejecting_fetcher::RejectingTransactionFetcher;
use tip_oracle::{AnyTipOracle, StaticTipOracle, ZinderTipOracle};
use zinder_client::{Network as ZinderNetwork, RemoteChainIndex, RemoteOpenOptions};
use zinder_fetcher::ZinderTransactionFetcher;
use zinder_oracle::ZinderConfirmationOracle;
use zinder_submitter::ZinderSubmitter;
use zpay_core::accepts::{AcceptsEntry, PayeeRegistry};
use zpay_core::chain_status::{ChainStatusCache, ChainStatusView};
use zpay_core::disclosure_fetcher::{DisclosedTransaction, DisclosureFetcher, FetchError};
use zpay_core::oracle::{ConfirmationOracle, ConfirmationOutcome};
use zpay_core::prepare::{PreparedTxCache, PreparedTxEntry, PreparedTxStore};
use zpay_core::status::{
    DEFAULT_FINALITY_DEPTH, SettlementLedger, SettlementLedgerEntry, SettlementLedgerStore,
    SuccessKindRow, lookup_payment_status,
};
use zpay_core::store::StoreError;
use zpay_core::types::{PayeeId, PaymentId, PaymentNetwork};
use zpay_core::verify::LocalPaymentDisclosureVerifier;
use zpay_store::{LibsqlPreparedTxStore, LibsqlSettlementLedgerStore, open_and_migrate};
use zpay_x402::{
    AppState, DpopExpectations, DpopInMemoryReplayStore, PaymentEventHub, RateLimiter,
};

/// zpay facilitator runtime.
#[derive(Debug, Parser)]
#[command(name = "zpay-runtime", version, about)]
struct Cli {
    /// Print the resolved configuration with secrets redacted, then exit.
    #[arg(long)]
    print_config: bool,
}

#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error("invalid bind address: {field}={provided:?}: {source}")]
    BindAddress {
        field: &'static str,
        provided: String,
        #[source]
        source: std::net::AddrParseError,
    },
    #[error("listener bind failed on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("listener serve failed: {source}")]
    Serve {
        #[source]
        source: std::io::Error,
    },
    #[error("tracing subscriber install failed: {source}")]
    Tracing {
        #[source]
        source: tracing_subscriber::util::TryInitError,
    },
    #[error("metrics recorder install failed: {reason}")]
    Metrics { reason: String },
    #[error("{field} has invalid u32 value {provided:?}: {source}")]
    EnvU32Invalid {
        field: &'static str,
        provided: String,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("zinder broadcast client construction failed for endpoint {endpoint}: {source}")]
    Submitter {
        endpoint: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("payee registry config read failed: {path}: {reason}")]
    PayeesConfig { path: String, reason: String },
    #[error(
        "ZPAY_NETWORK has unknown value {provided:?}; expected one of mainnet, testnet, regtest"
    )]
    NetworkInvalid { provided: String },
    #[error(
        "ZPAY_VERIFY__NETWORK has unknown value {provided:?}; expected one of mainnet, testnet"
    )]
    VerifyNetworkInvalid { provided: String },
    #[error(
        "ZPAY_VERIFY__NETWORK is required; set it to mainnet or testnet (regtest pins to testnet)"
    )]
    VerifyNetworkMissing,
    #[error("ZPAY_STATIC_TIP_FALLBACK has invalid u32 value {provided:?}: {source}")]
    StaticTipInvalid {
        provided: String,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("ZPAY_FINALITY_DEPTH has invalid u32 value {provided:?}: {source}")]
    FinalityDepthInvalid {
        provided: String,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("store backend {backend:?} initialisation failed: {source}")]
    StoreBackend {
        backend: String,
        #[source]
        source: StoreError,
    },
    #[error("invalid store backend {backend:?}: expected 'memory' or 'libsql'")]
    StoreBackendInvalid { backend: String },
    #[error(
        "payee {payee_id:?} advertises the baked-in demo placeholder pay_to; refusing to start. \
         Override the payees config (ZPAY_PAYEES__CONFIG_PATH) with a real receiver, or set \
         ZPAY_ALLOW_DEMO_PAYEE=1 to bypass for dev (never set this in production)."
    )]
    PlaceholderPayee { payee_id: String },
}

#[tokio::main]
async fn main() -> Result<(), StartupError> {
    install_tracing()?;

    let cli = Cli::parse();
    let config = ResolvedConfig::from_env()?;

    if cli.print_config {
        emit_config(&config);
        return Ok(());
    }

    let metrics = install_metrics_recorder()?;
    let app_plane = build_app_router(&config).await?;
    spawn_prepared_tx_sweeper(Arc::clone(&app_plane.prepared_store));
    let chain_probe = spawn_settlement_reconciliation(&config, &app_plane)?;

    let ops_state = OpsState {
        chain_probe,
        store_probe: Arc::clone(&app_plane.prepared_store) as Arc<dyn ReadinessStoreProbe>,
        chain_status: Arc::clone(&app_plane.chain_status),
        metrics,
        app_bind_addr: config.app_bind_addr,
        ops_bind_addr: config.ops_bind_addr,
    };

    let app_router = app_plane.router;
    let ops_router = build_ops_router(ops_state);

    let app_listener = tokio::net::TcpListener::bind(config.app_bind_addr)
        .await
        .map_err(|source| StartupError::Bind {
            addr: config.app_bind_addr,
            source,
        })?;
    let ops_listener = tokio::net::TcpListener::bind(config.ops_bind_addr)
        .await
        .map_err(|source| StartupError::Bind {
            addr: config.ops_bind_addr,
            source,
        })?;

    tracing::info!(
        app = %config.app_bind_addr,
        ops = %config.ops_bind_addr,
        network = %config.network,
        "zpay-runtime ready",
    );

    let shutdown = shutdown_signal();
    // The app listener is served with connection info so the rate limiter can
    // fall back to the peer socket when no forwarding header is present.
    let app_serve = axum::serve(
        app_listener,
        app_router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal());
    let ops_serve = axum::serve(ops_listener, ops_router).with_graceful_shutdown(shutdown);

    tokio::try_join!(app_serve, ops_serve).map_err(|source| StartupError::Serve { source })?;
    Ok(())
}

/// Install the Prometheus recorder as the process-global metrics sink and
/// return the render handle the `/metrics` route serves.
fn install_metrics_recorder() -> Result<PrometheusHandle, StartupError> {
    PrometheusBuilder::new()
        .install_recorder()
        .map_err(|source| StartupError::Metrics {
            reason: source.to_string(),
        })
}

fn install_tracing() -> Result<(), StartupError> {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("zpay=info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json())
        .try_init()
        .map_err(|source| StartupError::Tracing { source })
}

struct AppPlane {
    router: Router,
    prepared_store: Arc<AnyPreparedTxStore>,
    ledger: Arc<AnySettlementLedgerStore>,
    events: Arc<PaymentEventHub>,
    chain_status: Arc<ChainStatusCache>,
}

async fn build_app_router(config: &ResolvedConfig) -> Result<AppPlane, StartupError> {
    let chain = build_broadcast_client(config)?;
    let verifier = LocalPaymentDisclosureVerifier::new(config.verify_network);
    let fetcher = build_transaction_fetcher(config)?;
    let payees = load_payee_registry(config)?;
    validate_payees(&payees, config.allow_demo_payee)?;
    let tip_oracle = build_tip_oracle(config)?;
    let (prepared_store, ledger) = build_stores(config).await?;
    let events = Arc::new(PaymentEventHub::new());
    let chain_status = Arc::new(ChainStatusCache::new());

    let expectations = build_dpop_expectations(config);
    let replay_store: Arc<dyn zpay_x402::DpopReplayStore> =
        Arc::new(DpopInMemoryReplayStore::new());
    let rate_limiter = Arc::new(RateLimiter::new(
        config.rate_limit_per_jkt_per_minute,
        config.rate_limit_per_ip_per_minute,
    ));
    let state = AppState::new(
        Arc::clone(&prepared_store),
        Arc::clone(&ledger),
        Arc::new(payees),
        Arc::new(chain),
        Arc::new(verifier),
        Arc::clone(&events),
        Arc::new(tip_oracle),
        Arc::new(fetcher),
        replay_store,
        expectations,
        config.finality_depth,
        Arc::clone(&chain_status),
        rate_limiter,
        config.rate_limit_trust_forwarded_headers,
    );
    // `/healthz` lives on the app listener too so platform health probes
    // (Railway, Kubernetes, AWS ALB) that only reach the public port get a
    // 200 from a healthy process. The ops listener also serves it so
    // intra-cluster sidecars keep the existing path.
    let router = Router::new()
        .route("/healthz", get(healthz))
        .nest("/x402/v2", zpay_x402::router(state, &config.cors_allowlist));

    Ok(AppPlane {
        router: router.layer(tower_http::trace::TraceLayer::new_for_http()),
        prepared_store,
        ledger,
        events,
        chain_status,
    })
}

async fn build_stores(
    config: &ResolvedConfig,
) -> Result<(Arc<AnyPreparedTxStore>, Arc<AnySettlementLedgerStore>), StartupError> {
    match config.store_backend.as_str() {
        "memory" => {
            tracing::info!(backend = "memory", "store backend wired (no persistence)");
            Ok((
                Arc::new(AnyPreparedTxStore::Memory(PreparedTxCache::new())),
                Arc::new(AnySettlementLedgerStore::Memory(SettlementLedger::new())),
            ))
        }
        "libsql" => {
            let connection =
                open_and_migrate(&config.store_url, config.store_auth_token.as_deref())
                    .await
                    .map_err(|source| StartupError::StoreBackend {
                        backend: "libsql".to_owned(),
                        source,
                    })?;
            tracing::info!(
                backend = "libsql",
                store_url = %config.store_url,
                "store backend wired (libsql migrations applied)",
            );
            Ok((
                Arc::new(AnyPreparedTxStore::Libsql(LibsqlPreparedTxStore::new(
                    connection.clone(),
                ))),
                Arc::new(AnySettlementLedgerStore::Libsql(
                    LibsqlSettlementLedgerStore::new(connection),
                )),
            ))
        }
        other => Err(StartupError::StoreBackendInvalid {
            backend: other.to_owned(),
        }),
    }
}

/// Runtime-time discriminator over the configured prepared-tx store.
enum AnyPreparedTxStore {
    Memory(PreparedTxCache),
    Libsql(LibsqlPreparedTxStore),
}

impl PreparedTxStore for AnyPreparedTxStore {
    async fn insert(&self, entry: PreparedTxEntry) -> Result<(), StoreError> {
        match self {
            Self::Memory(inner) => inner.insert(entry).await,
            Self::Libsql(inner) => inner.insert(entry).await,
        }
    }

    async fn find_by_payment_id(
        &self,
        payment_id: &PaymentId,
    ) -> Result<Option<PreparedTxEntry>, StoreError> {
        match self {
            Self::Memory(inner) => inner.find_by_payment_id(payment_id).await,
            Self::Libsql(inner) => inner.find_by_payment_id(payment_id).await,
        }
    }

    async fn find_by_idempotency(
        &self,
        jkt: &str,
        idempotency_key: &str,
    ) -> Result<Option<PreparedTxEntry>, StoreError> {
        match self {
            Self::Memory(inner) => inner.find_by_idempotency(jkt, idempotency_key).await,
            Self::Libsql(inner) => inner.find_by_idempotency(jkt, idempotency_key).await,
        }
    }

    async fn remove(&self, payment_id: &PaymentId) -> Result<Option<PreparedTxEntry>, StoreError> {
        match self {
            Self::Memory(inner) => inner.remove(payment_id).await,
            Self::Libsql(inner) => inner.remove(payment_id).await,
        }
    }

    async fn sweep_expired(&self, now_unix_seconds: u64) -> Result<usize, StoreError> {
        match self {
            Self::Memory(inner) => inner.sweep_expired(now_unix_seconds).await,
            Self::Libsql(inner) => inner.sweep_expired(now_unix_seconds).await,
        }
    }

    async fn entry_count(&self) -> Result<usize, StoreError> {
        match self {
            Self::Memory(inner) => inner.entry_count().await,
            Self::Libsql(inner) => inner.entry_count().await,
        }
    }
}

/// Runtime-time discriminator over the configured settlement ledger.
enum AnySettlementLedgerStore {
    Memory(SettlementLedger),
    Libsql(LibsqlSettlementLedgerStore),
}

impl SettlementLedgerStore for AnySettlementLedgerStore {
    async fn record(
        &self,
        payment_id: PaymentId,
        entry: SettlementLedgerEntry,
    ) -> Result<(), StoreError> {
        match self {
            Self::Memory(inner) => inner.record(payment_id, entry).await,
            Self::Libsql(inner) => inner.record(payment_id, entry).await,
        }
    }

    async fn find(
        &self,
        payment_id: &PaymentId,
    ) -> Result<Option<SettlementLedgerEntry>, StoreError> {
        match self {
            Self::Memory(inner) => inner.find(payment_id).await,
            Self::Libsql(inner) => inner.find(payment_id).await,
        }
    }

    async fn entry_count(&self) -> Result<usize, StoreError> {
        match self {
            Self::Memory(inner) => inner.entry_count().await,
            Self::Libsql(inner) => inner.entry_count().await,
        }
    }

    async fn success_kind_transactions(&self) -> Result<Vec<SuccessKindRow>, StoreError> {
        match self {
            Self::Memory(inner) => inner.success_kind_transactions().await,
            Self::Libsql(inner) => inner.success_kind_transactions().await,
        }
    }

    async fn record_confirmation(
        &self,
        payment_id: &PaymentId,
        confirmation_count: u32,
        mined_block_height: Option<u64>,
    ) -> Result<bool, StoreError> {
        match self {
            Self::Memory(inner) => {
                inner
                    .record_confirmation(payment_id, confirmation_count, mined_block_height)
                    .await
            }
            Self::Libsql(inner) => {
                inner
                    .record_confirmation(payment_id, confirmation_count, mined_block_height)
                    .await
            }
        }
    }

    async fn downgrade_on_reorg(
        &self,
        payment_id: &PaymentId,
        reorged_at_unix_seconds: i64,
    ) -> Result<bool, StoreError> {
        match self {
            Self::Memory(inner) => {
                inner
                    .downgrade_on_reorg(payment_id, reorged_at_unix_seconds)
                    .await
            }
            Self::Libsql(inner) => {
                inner
                    .downgrade_on_reorg(payment_id, reorged_at_unix_seconds)
                    .await
            }
        }
    }

    async fn downgrade_reorged_range(
        &self,
        reverted_start_height: u64,
        reverted_end_height: u64,
        reorged_at_unix_seconds: i64,
    ) -> Result<Vec<PaymentId>, StoreError> {
        match self {
            Self::Memory(inner) => {
                inner
                    .downgrade_reorged_range(
                        reverted_start_height,
                        reverted_end_height,
                        reorged_at_unix_seconds,
                    )
                    .await
            }
            Self::Libsql(inner) => {
                inner
                    .downgrade_reorged_range(
                        reverted_start_height,
                        reverted_end_height,
                        reorged_at_unix_seconds,
                    )
                    .await
            }
        }
    }
}

/// Interval at which the prepared-tx sweeper drops expired entries.
const PREPARED_TX_SWEEP_INTERVAL_SECONDS: u64 = 30;

/// Interval at which the confirmation oracle re-polls zinder.
///
/// Short enough that a freshly mined block is visible to agents within
/// a minute; long enough not to thrash the chain plane.
const CONFIRMATION_ORACLE_POLL_SECONDS: u64 = 60;

/// Default static tip height for the chain-tip fallback.
///
/// Used when neither a chain plane nor an explicit
/// `ZPAY_STATIC_TIP_FALLBACK` is configured. Chosen to match the
/// long-standing demo expiry the host probes used (`4_000_000`) so
/// cold-start scenarios stay deterministic across releases.
const DEFAULT_STATIC_TIP_FALLBACK: u32 = 4_000_000;

fn spawn_prepared_tx_sweeper(store: Arc<AnyPreparedTxStore>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
            PREPARED_TX_SWEEP_INTERVAL_SECONDS,
        ));
        // Tick once at start, then on every interval boundary.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let now_unix_seconds = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs());
            match store.sweep_expired(now_unix_seconds).await {
                Ok(dropped) if dropped > 0 => {
                    tracing::info!(
                        dropped_count = dropped,
                        "prepared-tx sweeper dropped expired entries",
                    );
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(error = %err, "prepared-tx sweep failed");
                }
            }
        }
    });
}

/// Wire the reorg-aware settlement reconciliation: the periodic
/// confirmation poll and the live chain-event subscription.
///
/// Both share one confirmation oracle so the chain-event task can run a
/// full reconciliation sweep on startup and after a cursor-expiry. When
/// no chain plane is configured, both stay disabled.
fn spawn_settlement_reconciliation(
    config: &ResolvedConfig,
    app_plane: &AppPlane,
) -> Result<Option<Arc<dyn ReadinessChainProbe>>, StartupError> {
    let Some(endpoint) = config.indexer_grpc_addr.clone() else {
        tracing::warn!(
            "ZPAY_CHAIN_SOURCE_URL unset; settlement reconciliation disabled until a chain plane is configured",
        );
        return Ok(None);
    };
    let zinder_network = zinder_network_from_str(&config.network)?;
    let oracle =
        ZinderConfirmationOracle::connect(endpoint.clone(), zinder_network).map_err(|source| {
            StartupError::Submitter {
                endpoint: endpoint.clone(),
                source: Box::new(source),
            }
        })?;
    let oracle = Arc::new(oracle);
    let chain = RemoteChainIndex::connect(RemoteOpenOptions {
        endpoint: endpoint.clone(),
        network: zinder_network,
    })
    .map_err(|source| StartupError::Submitter {
        endpoint,
        source: Box::new(source),
    })?;

    spawn_confirmation_poll_loop(
        Arc::clone(&oracle),
        Arc::clone(&app_plane.ledger),
        Arc::clone(&app_plane.prepared_store),
        Arc::clone(&app_plane.events),
        Arc::clone(&app_plane.chain_status),
        config.finality_depth,
    );
    chain_events::spawn(chain_events::ChainEventsDeps {
        chain,
        oracle: Arc::clone(&oracle),
        ledger: Arc::clone(&app_plane.ledger),
        prepared_store: Arc::clone(&app_plane.prepared_store),
        events: Arc::clone(&app_plane.events),
        chain_status: Arc::clone(&app_plane.chain_status),
        finality_depth: config.finality_depth,
    });
    spawn_chain_status_metrics(Arc::clone(&app_plane.chain_status));
    tracing::info!(
        finality_depth = config.finality_depth,
        "settlement reconciliation wired",
    );
    let probe: Arc<dyn ReadinessChainProbe> = oracle;
    Ok(Some(probe))
}

/// Interval at which the chain-status gauges are resampled.
///
/// Independent of the confirmation poll: the sampler holds no chain
/// connection, so it keeps reporting a growing cache age when the poll loop
/// dies, which is the signal an operator alerts on.
const CHAIN_STATUS_METRICS_SAMPLE_SECONDS: u64 = 15;

/// Sample the shared chain view into Prometheus gauges on a fixed cadence.
fn spawn_chain_status_metrics(chain_status: Arc<ChainStatusCache>) {
    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(Duration::from_secs(CHAIN_STATUS_METRICS_SAMPLE_SECONDS));
        loop {
            ticker.tick().await;
            let view = chain_status.load();
            if let Some(visible) = view.visible_tip_height {
                metrics::gauge!("zpay_chain_visible_tip_height").set(gauge_value(visible));
            }
            if let Some(settled) = view.settled_tip_height {
                metrics::gauge!("zpay_chain_settled_tip_height").set(gauge_value(settled));
            }
            if let Some(refreshed) = chain_status.last_refresh_unix_seconds() {
                let now = u64::try_from(now_unix_seconds()).unwrap_or(0);
                metrics::gauge!("zpay_chain_status_cache_age_seconds")
                    .set(gauge_value(now.saturating_sub(refreshed)));
            }
        }
    });
}

/// Cast a `u64` gauge sample to the `f64` the metrics facade expects.
#[allow(
    clippy::cast_precision_loss,
    reason = "gauge samples are chain heights and small cache ages, well inside f64's exact-integer range"
)]
fn gauge_value(sample: u64) -> f64 {
    sample as f64
}

#[allow(
    clippy::too_many_arguments,
    reason = "the poll loop threads the same collaborator set the reconciliation sweep uses; a bundle struct would only rename the arguments"
)]
fn spawn_confirmation_poll_loop(
    oracle: Arc<ZinderConfirmationOracle>,
    ledger: Arc<AnySettlementLedgerStore>,
    prepared_store: Arc<AnyPreparedTxStore>,
    events: Arc<PaymentEventHub>,
    chain_status: Arc<ChainStatusCache>,
    finality_depth: u32,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
            CONFIRMATION_ORACLE_POLL_SECONDS,
        ));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            poll_oracle_once(
                oracle.as_ref(),
                ledger.as_ref(),
                prepared_store.as_ref(),
                events.as_ref(),
                chain_status.as_ref(),
                finality_depth,
            )
            .await;
        }
    });
}

/// Current wall-clock time in unix seconds, saturating on overflow.
fn now_unix_seconds() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs()),
    )
    .unwrap_or(i64::MAX)
}

/// Reconcile every unsettled success-kind row against the chain plane.
///
/// Refreshes the shared chain view, records confirmations for mined rows,
/// downgrades a mined row the chain plane no longer reports, and skips the
/// chain call for rows already at or below the settled tip. A row with a
/// live SSE subscriber gets a fresh snapshot published so a downgrade,
/// expiry lapse, or settlement reaches the open stream.
#[allow(
    clippy::too_many_arguments,
    reason = "each argument is a distinct collaborator the reconciliation reads or writes; a bundle struct would only rename them"
)]
pub(crate) async fn poll_oracle_once<O, L, P>(
    oracle: &O,
    ledger: &L,
    prepared_store: &P,
    events: &PaymentEventHub,
    chain_status: &ChainStatusCache,
    finality_depth: u32,
) where
    O: ConfirmationOracle,
    L: SettlementLedgerStore + ?Sized,
    P: PreparedTxStore + ?Sized,
{
    let chain_view = match oracle.chain_status().await {
        Ok(view) => {
            if let (Some(visible), Some(settled)) =
                (view.visible_tip_height, view.settled_tip_height)
            {
                chain_status.store(visible, settled);
            }
            view
        }
        Err(err) => {
            tracing::warn!(error = %err, "chain status read for reconciliation tick failed");
            chain_status.load()
        }
    };

    let rows = match ledger.success_kind_transactions().await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(error = %err, "ledger lookup for reconciliation tick failed");
            return;
        }
    };

    for row in rows {
        let settled = row
            .mined_block_height
            .is_some_and(|height| chain_view.is_settled_at(height));
        if !settled {
            reconcile_unsettled_row(oracle, ledger, &row).await;
        }
        if events.has_subscribers(&row.payment_id) {
            publish_snapshot(
                prepared_store,
                ledger,
                events,
                chain_view,
                finality_depth,
                &row.payment_id,
            )
            .await;
        }
    }
}

/// Poll one unsettled row against the chain plane and apply the result.
async fn reconcile_unsettled_row<O, L>(oracle: &O, ledger: &L, row: &SuccessKindRow)
where
    O: ConfirmationOracle,
    L: SettlementLedgerStore + ?Sized,
{
    let outcome = match oracle.fetch_confirmations(&row.transaction_id).await {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!(
                payment_id = %row.payment_id,
                transaction_id = %row.transaction_id,
                error = %err,
                "confirmation oracle poll failed",
            );
            return;
        }
    };
    metrics::counter!(
        "zpay_confirmation_updates_total",
        "outcome" => confirmation_outcome_label(&outcome),
    )
    .increment(1);
    match outcome {
        ConfirmationOutcome::Mined {
            block_height,
            confirmation_count,
        } => {
            if let Err(err) = ledger
                .record_confirmation(&row.payment_id, confirmation_count, Some(block_height))
                .await
            {
                tracing::warn!(error = %err, "record_confirmation failed");
            }
        }
        ConfirmationOutcome::InMempool => {
            if row.mined_block_height.is_some() {
                // A tx that was mined and is now back in the mempool had
                // its block reorged out: downgrade rather than keep the
                // stale mined height alongside a zeroed count.
                downgrade_reorged_row(ledger, row).await;
            } else if let Err(err) = ledger.record_confirmation(&row.payment_id, 0, None).await {
                tracing::warn!(error = %err, "record_confirmation failed");
            }
        }
        #[allow(
            clippy::collapsible_match,
            reason = "folding the guard into the arm would let the #[non_exhaustive] wildcard match NotFound/ConflictingChain and weaken the exhaustiveness check"
        )]
        ConfirmationOutcome::NotFound | ConfirmationOutcome::ConflictingChain => {
            if row.mined_block_height.is_some() {
                downgrade_reorged_row(ledger, row).await;
            }
        }
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "ConfirmationOutcome is #[non_exhaustive]; future variants stay a no-op until they have explicit handling"
        )]
        _ => {}
    }
}

/// Bounded `outcome` label for a confirmation-oracle result.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "ConfirmationOutcome is #[non_exhaustive]; a future variant reports `other` until it has an explicit label"
)]
fn confirmation_outcome_label(outcome: &ConfirmationOutcome) -> &'static str {
    match outcome {
        ConfirmationOutcome::Mined { .. } => "mined",
        ConfirmationOutcome::InMempool => "in_mempool",
        ConfirmationOutcome::NotFound => "not_found",
        ConfirmationOutcome::ConflictingChain => "conflicting_chain",
        _ => "other",
    }
}

/// Downgrade a mined row the chain plane stopped reporting mined.
async fn downgrade_reorged_row<L>(ledger: &L, row: &SuccessKindRow)
where
    L: SettlementLedgerStore + ?Sized,
{
    match ledger
        .downgrade_on_reorg(&row.payment_id, now_unix_seconds())
        .await
    {
        Ok(true) => {
            metrics::counter!("zpay_reorg_downgrades_total", "source" => "poll").increment(1);
            tracing::info!(
                payment_id = %row.payment_id,
                transaction_id = %row.transaction_id,
                "reorg downgrade: mined tx no longer on chain plane",
            );
        }
        Ok(false) => {}
        Err(err) => tracing::warn!(error = %err, "downgrade_on_reorg failed"),
    }
}

/// Re-read and publish a payment's snapshot to any live SSE subscriber.
#[allow(
    clippy::too_many_arguments,
    reason = "the publish path needs both stores, the hub, the chain view, the finality depth, and the target id; none is redundant"
)]
pub(crate) async fn publish_snapshot<P, L>(
    prepared_store: &P,
    ledger: &L,
    events: &PaymentEventHub,
    chain_view: zpay_core::chain_status::ChainStatusView,
    finality_depth: u32,
    payment_id: &PaymentId,
) where
    P: PreparedTxStore + ?Sized,
    L: SettlementLedgerStore + ?Sized,
{
    match lookup_payment_status(
        payment_id,
        prepared_store,
        ledger,
        finality_depth,
        chain_view,
    )
    .await
    {
        Ok(snapshot) => events.publish(payment_id, snapshot),
        Err(err) => tracing::warn!(
            payment_id = %payment_id,
            error = %err,
            "snapshot read for publish failed",
        ),
    }
}

/// Build the operator-supplied DPoP expectations bundle.
///
/// When `ZPAY_EXPECTED_HOST` is unset we emit a single startup `WARN`
/// so an operator who ships to production without pinning the host
/// sees the gap in the logs; the verifier then falls back to the
/// inbound `Host` header.
fn build_dpop_expectations(config: &ResolvedConfig) -> DpopExpectations {
    config.expected_host.clone().map_or_else(
        || {
            tracing::warn!(
                scheme = %config.expected_scheme,
                "ZPAY_EXPECTED_HOST unset; DPoP htu canonicalization uses inbound Host header. Set this in production.",
            );
            DpopExpectations::unbound(config.expected_scheme.clone())
        },
        |host| {
            tracing::info!(
                scheme = %config.expected_scheme,
                host = %host,
                "DPoP host pinning enabled",
            );
            DpopExpectations::pinned(config.expected_scheme.clone(), host)
        },
    )
}

fn build_broadcast_client(config: &ResolvedConfig) -> Result<AnySubmitter, StartupError> {
    let zally_network = zally_network_from_config_str(&config.network)?;
    let Some(endpoint) = config.indexer_grpc_addr.clone() else {
        tracing::warn!(
            "ZPAY_CHAIN_SOURCE_URL unset; /x402/v2/settle will return 502 until a chain plane is configured",
        );
        return Ok(AnySubmitter::Rejecting(zally_network));
    };
    let zinder_network = zinder_network_from_str(&config.network)?;
    let client = ZinderSubmitter::connect(endpoint.clone(), zinder_network, zally_network)
        .map_err(|source| StartupError::Submitter {
            endpoint,
            source: Box::new(source),
        })?;
    tracing::info!(
        network = %config.network,
        "zinder submitter wired",
    );
    Ok(AnySubmitter::Zinder(Box::new(client)))
}

fn zally_network_from_config_str(raw: &str) -> Result<zally_core::Network, StartupError> {
    match raw {
        "mainnet" => Ok(zally_core::Network::Mainnet),
        "testnet" | "regtest" => Ok(zally_core::Network::Testnet),
        other => Err(StartupError::NetworkInvalid {
            provided: other.to_owned(),
        }),
    }
}

fn build_tip_oracle(config: &ResolvedConfig) -> Result<AnyTipOracle, StartupError> {
    let Some(endpoint) = config.indexer_grpc_addr.clone() else {
        tracing::warn!(
            fallback_tip = config.static_tip_fallback,
            "ZPAY_CHAIN_SOURCE_URL unset; using static fallback tip for /prepare and /tip",
        );
        return Ok(AnyTipOracle::Static(StaticTipOracle::new(
            config.static_tip_fallback,
        )));
    };
    let zinder_network = zinder_network_from_str(&config.network)?;
    let oracle = ZinderTipOracle::connect(endpoint.clone(), zinder_network).map_err(|source| {
        StartupError::Submitter {
            endpoint,
            source: Box::new(source),
        }
    })?;
    tracing::info!(
        network = %config.network,
        "zinder chain tip oracle wired",
    );
    Ok(AnyTipOracle::Zinder(Box::new(oracle)))
}

fn load_payee_registry(config: &ResolvedConfig) -> Result<PayeeRegistry, StartupError> {
    let Some(path) = config.payees_config_path.clone() else {
        return Ok(PayeeRegistry::new());
    };
    let raw = std::fs::read_to_string(&path).map_err(|source| StartupError::PayeesConfig {
        path: path.clone(),
        reason: source.to_string(),
    })?;
    let parsed: PayeesConfigFile =
        toml::from_str(&raw).map_err(|source| StartupError::PayeesConfig {
            path: path.clone(),
            reason: source.to_string(),
        })?;
    let mut registry = PayeeRegistry::new();
    for (payee_id, payee) in parsed.payees {
        registry.register(PayeeId(payee_id), payee.accepts);
    }
    tracing::info!(
        payee_count = registry.payee_count(),
        path = %path,
        "payee registry loaded",
    );
    Ok(registry)
}

/// Walk every registered payee and refuse to start when the baked-in
/// demo placeholder `pay_to` is still present.
///
/// The placeholder shipped in `etc/aether-demo.toml` is a long string
/// of the form `utest1qqq…qqq` (Zcash testnet UA prefix followed by a
/// run of `q` padding characters). The gate matches that *shape*, not
/// the exact baked-in literal, so a slightly different placeholder
/// (extra char, off-by-one length) still trips the gate. Any real UA
/// has non-`q` characters in the data portion and passes through.
///
/// When `allow_demo_payee` is true the gate emits a `WARN` per
/// offending payee and proceeds; this is the dev / docker-compose
/// escape hatch only and must never be set in production.
fn validate_payees(registry: &PayeeRegistry, allow_demo: bool) -> Result<(), StartupError> {
    for (payee_id, entries) in registry.iter() {
        for entry in entries {
            if !is_placeholder_pay_to(&entry.pay_to) {
                continue;
            }
            if allow_demo {
                tracing::warn!(
                    payee_id = %payee_id.0,
                    "ZPAY_ALLOW_DEMO_PAYEE=1; running with placeholder payee; do not use in production",
                );
            } else {
                return Err(StartupError::PlaceholderPayee {
                    payee_id: payee_id.0.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Demo-placeholder UA shape: the Zcash testnet UA HRP followed by a
/// run of `q` padding characters.
const PLACEHOLDER_PAY_TO_PREFIX: &str = "utest1q";
/// Minimum total length the shape check requires.
///
/// The baked-in `etc/aether-demo.toml` placeholder is 133 chars; we
/// leave a small floor below that so a typo (off by a few chars)
/// still trips the gate without false-positive matches on short
/// hand-typed addresses.
const PLACEHOLDER_PAY_TO_MIN_LEN: usize = 100;

/// Match the demo placeholder by *shape*, not by full-string compare.
///
/// Rule: starts with `utest1q`, is at least 100 chars long, and every
/// character after the `utest1` HRP is `q`. Real testnet UAs encode
/// keys and metadata that yield a base32 alphabet over the data
/// portion, so they always contain non-`q` characters past position 6.
fn is_placeholder_pay_to(pay_to: &str) -> bool {
    /// Length of the testnet UA HRP `utest1` (Zcash UA human-readable
    /// prefix).
    const HRP_LEN: usize = 6;
    if !pay_to.starts_with(PLACEHOLDER_PAY_TO_PREFIX) || pay_to.len() < PLACEHOLDER_PAY_TO_MIN_LEN {
        return false;
    }
    pay_to[HRP_LEN..].bytes().all(|b| b == b'q')
}

#[derive(Debug, serde::Deserialize)]
struct PayeesConfigFile {
    #[serde(default)]
    payees: std::collections::HashMap<String, PayeesConfigEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct PayeesConfigEntry {
    #[serde(default)]
    accepts: Vec<AcceptsEntry>,
}

fn zinder_network_from_str(raw: &str) -> Result<ZinderNetwork, StartupError> {
    match raw {
        "mainnet" => Ok(ZinderNetwork::ZcashMainnet),
        "testnet" => Ok(ZinderNetwork::ZcashTestnet),
        "regtest" => Ok(ZinderNetwork::ZcashRegtest),
        other => Err(StartupError::NetworkInvalid {
            provided: other.to_owned(),
        }),
    }
}

/// Concrete submitter variant chosen at startup.
///
/// Using an enum (rather than `Arc<dyn Submitter>`) keeps the dispatch
/// statically resolvable. The unit-sized `Rejecting` variant carries the
/// network it is bound to so the `Submitter::network()` method is
/// answerable without inspecting an absent inner.
enum AnySubmitter {
    /// Placeholder when no chain plane is configured. Every `submit` call
    /// returns `SubmitterError::Unavailable`, which the settle handler maps
    /// to `SettleError::ChainUnavailable` and the wire layer surfaces as 502.
    Rejecting(zally_core::Network),
    /// Production submitter backed by zinder's
    /// `WalletQuery.BroadcastTransaction`. Boxed because the underlying
    /// `RemoteChainIndex` carries a tonic `Endpoint` of several hundred
    /// bytes, while the `Rejecting` variant is unit-sized; clippy's
    /// `large_enum_variant` rule prefers indirection.
    Zinder(Box<ZinderSubmitter>),
}

#[async_trait::async_trait]
impl zally_chain::Submitter for AnySubmitter {
    fn network(&self) -> zally_core::Network {
        match self {
            Self::Rejecting(network) => *network,
            Self::Zinder(submitter) => submitter.network(),
        }
    }

    async fn submit(
        &self,
        raw_tx: &[u8],
    ) -> Result<zally_chain::SubmitOutcome, zally_chain::SubmitterError> {
        match self {
            Self::Rejecting(_) => Err(zally_chain::SubmitterError::Unavailable {
                reason: "submitter not configured; set ZPAY_CHAIN_SOURCE_URL to enable settle"
                    .to_owned(),
            }),
            Self::Zinder(submitter) => submitter.submit(raw_tx).await,
        }
    }
}

/// Concrete transaction-fetcher variant chosen at startup.
///
/// The runtime composes the local ZIP-311 verifier with whichever
/// concrete fetcher the operator's environment supports. See ADR-0007.
enum AnyTransactionFetcher {
    /// Placeholder when no explorer endpoint is configured. Every
    /// call returns `FetchError::Unavailable`, which the local verifier
    /// surfaces as `chain_presence: "oracle_unavailable"` on the wire.
    Rejecting(RejectingTransactionFetcher),
    /// Production fetcher backed by zinder's explorer plane.
    ///
    /// Boxed because the underlying tonic `Endpoint` is several hundred
    /// bytes, while the `Rejecting` variant is unit-sized; clippy's
    /// `large_enum_variant` rule prefers indirection.
    Zinder(Box<ZinderTransactionFetcher>),
}

impl DisclosureFetcher for AnyTransactionFetcher {
    async fn fetch_transaction(&self, txid: [u8; 32]) -> Result<DisclosedTransaction, FetchError> {
        match self {
            Self::Rejecting(inner) => inner.fetch_transaction(txid).await,
            Self::Zinder(fetcher) => fetcher.fetch_transaction(txid).await,
        }
    }
}

fn build_transaction_fetcher(
    config: &ResolvedConfig,
) -> Result<AnyTransactionFetcher, StartupError> {
    let Some(endpoint) = config.explorer_grpc_addr.clone() else {
        tracing::warn!(
            "ZPAY_EXPLORER_URL unset; /x402/v2/verify reports chain_presence=oracle_unavailable until an explorer plane is configured",
        );
        return Ok(AnyTransactionFetcher::Rejecting(
            RejectingTransactionFetcher::new(),
        ));
    };
    let fetcher = ZinderTransactionFetcher::connect(endpoint.clone()).map_err(|source| {
        StartupError::Submitter {
            endpoint,
            source: Box::new(source),
        }
    })?;
    tracing::info!("zinder transaction fetcher wired");
    Ok(AnyTransactionFetcher::Zinder(Box::new(fetcher)))
}

/// Deadline for the live chain probe on `/readyz`. Short so a hung chain
/// plane does not stall the readiness check.
const CHAIN_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Ceiling on the [`ChainStatusCache`] age before the chain dependency reads
/// not-ready, even while a live probe succeeds. Three poll intervals leaves
/// room for one missed tick without flapping.
const CHAIN_STATUS_FRESHNESS_CEILING_SECONDS: u64 = 3 * CONFIRMATION_ORACLE_POLL_SECONDS;

/// A cheap live chain read used by the readiness probe.
#[async_trait::async_trait]
trait ReadinessChainProbe: Send + Sync {
    async fn live_tip(&self) -> Result<ChainStatusView, String>;
}

#[async_trait::async_trait]
impl ReadinessChainProbe for ZinderConfirmationOracle {
    async fn live_tip(&self) -> Result<ChainStatusView, String> {
        self.chain_status().await.map_err(|err| err.to_string())
    }
}

/// A trivial store liveness read used by the readiness probe.
#[async_trait::async_trait]
trait ReadinessStoreProbe: Send + Sync {
    async fn liveness(&self) -> Result<(), String>;
}

#[async_trait::async_trait]
impl ReadinessStoreProbe for AnyPreparedTxStore {
    async fn liveness(&self) -> Result<(), String> {
        self.entry_count()
            .await
            .map(|_| ())
            .map_err(|err| err.to_string())
    }
}

/// State shared by the ops-listener routes.
#[derive(Clone)]
struct OpsState {
    /// Live chain probe, or `None` when no chain plane is configured.
    chain_probe: Option<Arc<dyn ReadinessChainProbe>>,
    /// Store liveness probe.
    store_probe: Arc<dyn ReadinessStoreProbe>,
    /// Shared chain view used to report tip heights and cache freshness.
    chain_status: Arc<ChainStatusCache>,
    /// Prometheus render handle for `/metrics`.
    metrics: PrometheusHandle,
    app_bind_addr: SocketAddr,
    ops_bind_addr: SocketAddr,
}

fn build_ops_router(state: OpsState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_handler))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        r#"{"status":"alive"}"#,
    )
}

async fn metrics_handler(State(state): State<OpsState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        state.metrics.render(),
    )
}

async fn readyz(State(state): State<OpsState>) -> Response {
    let (status, body) = evaluate_readiness(
        state.chain_probe.as_ref(),
        &state.store_probe,
        &state.chain_status,
        state.app_bind_addr,
        state.ops_bind_addr,
    )
    .await;
    (
        status,
        [("content-type", "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// Evaluate the readiness of the chain plane and the store, returning the HTTP
/// status and the structured JSON body.
///
/// `not_ready` (503) when the store probe fails or, with a chain plane
/// configured, when the live chain probe is unreachable or the shared chain
/// view is staler than [`CHAIN_STATUS_FRESHNESS_CEILING_SECONDS`]. With no
/// chain plane configured the chain dependency reports `not_configured` and
/// does not gate readiness.
async fn evaluate_readiness(
    chain_probe: Option<&Arc<dyn ReadinessChainProbe>>,
    store_probe: &Arc<dyn ReadinessStoreProbe>,
    chain_status: &ChainStatusCache,
    app_bind_addr: SocketAddr,
    ops_bind_addr: SocketAddr,
) -> (StatusCode, serde_json::Value) {
    let store_result = store_probe.liveness().await;
    let store_json = match &store_result {
        Ok(()) => serde_json::json!({ "status": "ready", "probe": "ok" }),
        Err(reason) => serde_json::json!({ "status": "not_ready", "probe": reason }),
    };
    let store_ready = store_result.is_ok();

    let view = chain_status.load();
    let now = u64::try_from(now_unix_seconds()).unwrap_or(0);
    let cache_age_seconds = chain_status
        .last_refresh_unix_seconds()
        .map(|refreshed| now.saturating_sub(refreshed));

    let (chain_ready, chain_json) = match chain_probe {
        None => (
            true,
            serde_json::json!({
                "status": "not_configured",
                "live_probe": "not_configured",
                "visible_tip_height": view.visible_tip_height,
                "settled_tip_height": view.settled_tip_height,
                "cache_age_seconds": cache_age_seconds,
            }),
        ),
        Some(probe) => {
            let live = tokio::time::timeout(CHAIN_PROBE_TIMEOUT, probe.live_tip()).await;
            let live_ok = matches!(live, Ok(Ok(_)));
            let live_probe = match live {
                Ok(Ok(_)) => "ok",
                Ok(Err(_)) => "unreachable",
                Err(_) => "timeout",
            };
            let fresh =
                cache_age_seconds.is_some_and(|age| age <= CHAIN_STATUS_FRESHNESS_CEILING_SECONDS);
            let ready = live_ok && fresh;
            (
                ready,
                serde_json::json!({
                    "status": if ready { "ready" } else { "not_ready" },
                    "live_probe": live_probe,
                    "visible_tip_height": view.visible_tip_height,
                    "settled_tip_height": view.settled_tip_height,
                    "cache_age_seconds": cache_age_seconds,
                }),
            )
        }
    };

    let ready = store_ready && chain_ready;
    let body = serde_json::json!({
        "status": if ready { "ready" } else { "not_ready" },
        "dependencies": { "chain": chain_json, "store": store_json },
        "listeners": {
            "app": app_bind_addr.to_string(),
            "ops": ops_bind_addr.to_string(),
        },
    });
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, body)
}

#[derive(Debug, Clone)]
struct ResolvedConfig {
    app_bind_addr: SocketAddr,
    ops_bind_addr: SocketAddr,
    network: String,
    /// Network the ZIP-311 `BLAKE2b` digest personalization binds to.
    /// Read from `ZPAY_VERIFY__NETWORK` and constrained to mainnet
    /// or testnet (per ADR-0007: regtest carries no distinct SLIP-44
    /// number).
    verify_network: PaymentNetwork,
    indexer_grpc_addr: Option<String>,
    explorer_grpc_addr: Option<String>,
    payees_config_path: Option<String>,
    store_backend: String,
    store_url: String,
    store_auth_token: Option<String>,
    static_tip_fallback: u32,
    finality_depth: u32,
    /// Pinned host the DPoP verifier expects on every inbound
    /// request. Read from `ZPAY_EXPECTED_HOST`. When `None`, the
    /// verifier falls back to the inbound `Host` header and the
    /// runtime emits a startup `WARN`.
    expected_host: Option<String>,
    /// Scheme the DPoP verifier expects on every inbound request.
    /// Read from `ZPAY_EXPECTED_SCHEME`. Defaults to `"https"` when
    /// `ZPAY_EXPECTED_HOST` is set (operator-pinned production
    /// deployment) and `"http"` otherwise (dev fallback).
    expected_scheme: String,
    /// Operator-toggled escape hatch for the placeholder-payee gate.
    /// Read from `ZPAY_ALLOW_DEMO_PAYEE`. Off by default; set to a
    /// truthy value (`1`, `true`, `yes`, case-insensitive) only in
    /// dev/compose stacks that intentionally ship the baked-in
    /// `aether-demo` placeholder.
    allow_demo_payee: bool,
    /// Per-`jkt` request budget per minute on the DPoP-authenticated routes.
    /// Read from `ZPAY_RATE_LIMIT__PER_JKT_PER_MINUTE`; `0` disables the
    /// dimension.
    rate_limit_per_jkt_per_minute: u32,
    /// Per-IP request budget per minute on the unauthenticated routes. Read
    /// from `ZPAY_RATE_LIMIT__PER_IP_PER_MINUTE`; `0` disables the dimension.
    rate_limit_per_ip_per_minute: u32,
    /// Whether the per-IP rate-limit dimension may trust
    /// `X-Forwarded-For`/`X-Real-IP`. Read from
    /// `ZPAY_RATE_LIMIT__TRUST_FORWARDED_HEADERS`. Off by default: a direct
    /// caller controls those headers, so trusting them unconditionally lets
    /// an attacker rotate the leftmost hop per request and bypass the
    /// limiter. Enable only behind a reverse proxy that terminates every
    /// inbound connection and sets the header itself.
    rate_limit_trust_forwarded_headers: bool,
    /// Exact browser origins permitted by CORS. Read from
    /// `ZPAY_SERVER__CORS__ALLOWLIST` (comma-separated). Empty emits no CORS
    /// headers, so cross-origin browser calls stay blocked.
    cors_allowlist: Vec<String>,
}

impl ResolvedConfig {
    #[allow(
        clippy::too_many_lines,
        reason = "from_env is the env-var dispatch surface; each block reads, trims, and converts a single ZPAY_* variable, and splitting it into helper functions per variable would scatter the env-var vocabulary across the file without changing the read order"
    )]
    fn from_env() -> Result<Self, StartupError> {
        let app_bind_raw = std::env::var("ZPAY_SERVER__BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string());
        let ops_bind_raw =
            std::env::var("ZPAY_OPS__BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:9295".to_string());
        let network = std::env::var("ZPAY_NETWORK").unwrap_or_else(|_| "regtest".to_string());
        let indexer_grpc_addr = std::env::var("ZPAY_CHAIN_SOURCE_URL")
            .ok()
            .filter(|raw| !raw.trim().is_empty());
        let explorer_grpc_addr = std::env::var("ZPAY_EXPLORER_URL")
            .ok()
            .filter(|raw| !raw.trim().is_empty());
        let payees_config_path = std::env::var("ZPAY_PAYEES__CONFIG_PATH")
            .ok()
            .filter(|raw| !raw.trim().is_empty());

        // Persistence: `libsql` is the default to match ADR-0004. Set
        // `ZPAY_STORE__BACKEND=memory` for ephemeral runs (no
        // persistence across restarts; useful for unit-style smoke
        // tests). `ZPAY_STORE__URL` accepts `file:<path>` for local
        // SQLite and `libsql://<host>` for Turso; the auth token only
        // applies to the remote shape.
        let store_backend =
            std::env::var("ZPAY_STORE__BACKEND").unwrap_or_else(|_| "libsql".to_string());
        let store_url =
            std::env::var("ZPAY_STORE__URL").unwrap_or_else(|_| "file:./zpay.libsql".to_string());
        let store_auth_token = std::env::var("ZPAY_STORE__AUTH_TOKEN")
            .ok()
            .filter(|raw| !raw.trim().is_empty());

        let static_tip_fallback = match std::env::var("ZPAY_STATIC_TIP_FALLBACK") {
            Ok(raw) => {
                let trimmed = raw.trim().to_owned();
                if trimmed.is_empty() {
                    DEFAULT_STATIC_TIP_FALLBACK
                } else {
                    trimmed
                        .parse::<u32>()
                        .map_err(|source| StartupError::StaticTipInvalid {
                            provided: trimmed,
                            source,
                        })?
                }
            }
            Err(_) => DEFAULT_STATIC_TIP_FALLBACK,
        };

        let finality_depth = match std::env::var("ZPAY_FINALITY_DEPTH") {
            Ok(raw) => {
                let trimmed = raw.trim().to_owned();
                if trimmed.is_empty() {
                    DEFAULT_FINALITY_DEPTH
                } else {
                    trimmed
                        .parse::<u32>()
                        .map_err(|source| StartupError::FinalityDepthInvalid {
                            provided: trimmed,
                            source,
                        })?
                }
            }
            Err(_) => DEFAULT_FINALITY_DEPTH,
        };

        let app_bind_addr = app_bind_raw
            .parse()
            .map_err(|source| StartupError::BindAddress {
                field: "ZPAY_SERVER__BIND_ADDR",
                provided: app_bind_raw,
                source,
            })?;
        let ops_bind_addr = ops_bind_raw
            .parse()
            .map_err(|source| StartupError::BindAddress {
                field: "ZPAY_OPS__BIND_ADDR",
                provided: ops_bind_raw,
                source,
            })?;

        // DPoP expectations: ZPAY_EXPECTED_HOST pins the host the DPoP
        // verifier canonicalizes against; ZPAY_EXPECTED_SCHEME pins the
        // scheme. The scheme defaults to "https" when a host is pinned
        // (production) and "http" otherwise (dev fallback). Either env
        // var may be set independently; an operator running TLS
        // termination at the edge can pin scheme=http if the runtime
        // sits behind the terminator.
        let expected_host = std::env::var("ZPAY_EXPECTED_HOST")
            .ok()
            .map(|raw| raw.trim().to_owned())
            .filter(|raw| !raw.is_empty());
        let expected_scheme = std::env::var("ZPAY_EXPECTED_SCHEME").map_or_else(
            |_| default_expected_scheme(expected_host.as_deref()),
            |raw| {
                let trimmed = raw.trim().to_owned();
                if trimmed.is_empty() {
                    default_expected_scheme(expected_host.as_deref())
                } else {
                    trimmed
                }
            },
        );

        // Verify-network: the digest personalization binds the
        // ZIP-311 BLAKE2b output to the network. Pinned via
        // ZPAY_VERIFY__NETWORK. The config has no default: a mismatch
        // between operator intent and the SLIP-44 coin type the
        // verifier uses would silently produce wrong verdicts (a
        // mainnet disclosure would never verify under testnet
        // personalization). Force the operator to choose.
        let verify_network_raw = std::env::var("ZPAY_VERIFY__NETWORK").ok();
        let verify_network = resolve_verify_network(verify_network_raw.as_deref())?;

        // Placeholder-payee gate: off by default. Truthy values are
        // `1`, `true`, `yes` (case-insensitive); anything else is
        // treated as off so a typo does not silently disable the gate.
        let allow_demo_payee =
            std::env::var("ZPAY_ALLOW_DEMO_PAYEE").is_ok_and(|raw| parse_truthy(&raw));

        let rate_limit_per_jkt_per_minute = parse_u32_env(
            "ZPAY_RATE_LIMIT__PER_JKT_PER_MINUTE",
            DEFAULT_RATE_LIMIT_PER_JKT_PER_MINUTE,
        )?;
        let rate_limit_per_ip_per_minute = parse_u32_env(
            "ZPAY_RATE_LIMIT__PER_IP_PER_MINUTE",
            DEFAULT_RATE_LIMIT_PER_IP_PER_MINUTE,
        )?;
        let rate_limit_trust_forwarded_headers =
            std::env::var("ZPAY_RATE_LIMIT__TRUST_FORWARDED_HEADERS")
                .is_ok_and(|raw| parse_truthy(&raw));
        let cors_allowlist = std::env::var("ZPAY_SERVER__CORS__ALLOWLIST")
            .ok()
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|origin| !origin.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(Self {
            app_bind_addr,
            ops_bind_addr,
            network,
            verify_network,
            indexer_grpc_addr,
            explorer_grpc_addr,
            payees_config_path,
            store_backend,
            store_url,
            store_auth_token,
            static_tip_fallback,
            finality_depth,
            expected_host,
            expected_scheme,
            allow_demo_payee,
            rate_limit_per_jkt_per_minute,
            rate_limit_per_ip_per_minute,
            rate_limit_trust_forwarded_headers,
            cors_allowlist,
        })
    }
}

/// Default per-`jkt` request budget per minute.
const DEFAULT_RATE_LIMIT_PER_JKT_PER_MINUTE: u32 = 120;
/// Default per-IP request budget per minute.
const DEFAULT_RATE_LIMIT_PER_IP_PER_MINUTE: u32 = 600;

/// Read a `u32` env var, returning `default` when unset or blank and erroring
/// on a present-but-unparseable value so a typo cannot silently disable a
/// limit.
fn parse_u32_env(field: &'static str, default: u32) -> Result<u32, StartupError> {
    let raw = std::env::var(field).unwrap_or_default();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    trimmed
        .parse::<u32>()
        .map_err(|source| StartupError::EnvU32Invalid {
            field,
            provided: trimmed.to_owned(),
            source,
        })
}

/// Parse a string into a truthy/falsy boolean.
///
/// Uses the same vocabulary most container runtimes accept: `1`,
/// `true`, `yes` (case-insensitive) are truthy; everything else
/// (including unset, empty, `0`, `false`, `no`, and any typo) is
/// falsy. Used for opt-in operator switches where a typo should not
/// silently flip behaviour into the dangerous direction.
fn parse_truthy(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

/// Parse `ZPAY_VERIFY__NETWORK` into a [`PaymentNetwork`]. Constrained
/// to mainnet or testnet per ADR-0007.
fn parse_verify_network(raw: &str) -> Result<PaymentNetwork, StartupError> {
    match raw {
        "mainnet" => Ok(PaymentNetwork::Mainnet),
        "testnet" => Ok(PaymentNetwork::Testnet),
        other => Err(StartupError::VerifyNetworkInvalid {
            provided: other.to_owned(),
        }),
    }
}

/// Resolve the raw `ZPAY_VERIFY__NETWORK` env value into a pinned
/// [`PaymentNetwork`].
///
/// Returns [`StartupError::VerifyNetworkMissing`] when the var is
/// absent or empty after trimming, and
/// [`StartupError::VerifyNetworkInvalid`] for any other string.
fn resolve_verify_network(raw: Option<&str>) -> Result<PaymentNetwork, StartupError> {
    let trimmed = raw.map_or("", str::trim);
    if trimmed.is_empty() {
        return Err(StartupError::VerifyNetworkMissing);
    }
    parse_verify_network(trimmed)
}

fn default_expected_scheme(expected_host: Option<&str>) -> String {
    if expected_host.is_some() {
        "https".to_owned()
    } else {
        "http".to_owned()
    }
}

fn emit_config(config: &ResolvedConfig) {
    tracing::info!(
        app = %config.app_bind_addr,
        ops = %config.ops_bind_addr,
        network = %config.network,
        wallet_age_identity = "[REDACTED]",
        store_auth_token = "[REDACTED]",
        "resolved configuration",
    );
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut stream) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            stream.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }

    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::{
        ConfirmationOracle, ConfirmationOutcome, StartupError, is_placeholder_pay_to, parse_truthy,
        poll_oracle_once, resolve_verify_network, validate_payees,
    };
    use parking_lot::Mutex;
    use zpay_core::accepts::{AcceptsEntry, PayeeRegistry};
    use zpay_core::broadcast::BroadcastOutcome;
    use zpay_core::oracle::OracleError;
    use zpay_core::types::{PayeeId, PaymentId, PaymentNetwork, PaymentScheme, Zatoshis};

    #[test]
    fn verify_network_required_when_env_missing() {
        let outcome = resolve_verify_network(None);
        assert!(matches!(outcome, Err(StartupError::VerifyNetworkMissing)));
    }

    #[test]
    fn verify_network_required_when_env_empty() {
        let outcome = resolve_verify_network(Some("   "));
        assert!(matches!(outcome, Err(StartupError::VerifyNetworkMissing)));
    }

    #[test]
    fn verify_network_invalid_for_unrecognised_value() {
        let outcome = resolve_verify_network(Some("regtest"));
        assert!(matches!(
            outcome,
            Err(StartupError::VerifyNetworkInvalid { ref provided }) if provided == "regtest",
        ));
    }

    #[test]
    fn verify_network_accepts_mainnet_and_testnet() {
        assert!(matches!(
            resolve_verify_network(Some("mainnet")),
            Ok(PaymentNetwork::Mainnet),
        ));
        assert!(matches!(
            resolve_verify_network(Some("testnet")),
            Ok(PaymentNetwork::Testnet),
        ));
    }
    use zpay_core::chain_status::{ChainStatusCache, ChainStatusView};
    use zpay_core::prepare::PreparedTxCache;
    use zpay_core::status::{
        DEFAULT_FINALITY_DEPTH, SettlementLedger, SettlementLedgerEntry, SettlementLedgerStore,
        lookup_payment_status,
    };
    use zpay_x402::PaymentEventHub;

    struct ScriptedOracle {
        outcomes: Mutex<std::collections::HashMap<String, ConfirmationOutcome>>,
        chain_view: ChainStatusView,
    }

    impl ScriptedOracle {
        fn new(pairs: &[(&'static str, ConfirmationOutcome)]) -> Self {
            let mut outcomes = std::collections::HashMap::new();
            for (txid, outcome) in pairs {
                outcomes.insert((*txid).to_owned(), outcome.clone());
            }
            Self {
                outcomes: Mutex::new(outcomes),
                chain_view: ChainStatusView::default(),
            }
        }

        fn with_chain_view(mut self, chain_view: ChainStatusView) -> Self {
            self.chain_view = chain_view;
            self
        }
    }

    impl ConfirmationOracle for ScriptedOracle {
        async fn fetch_confirmations(
            &self,
            transaction_id: &str,
        ) -> Result<ConfirmationOutcome, OracleError> {
            let guard = self.outcomes.lock();
            guard
                .get(transaction_id)
                .cloned()
                .ok_or_else(|| OracleError::Unavailable {
                    reason: format!("no scripted outcome for txid={transaction_id}"),
                })
        }

        async fn chain_status(&self) -> Result<ChainStatusView, OracleError> {
            Ok(self.chain_view)
        }
    }

    fn accepted_ledger_entry(
        transaction_id: &str,
        confirmation_count: Option<u32>,
        mined_block_height: Option<u64>,
    ) -> SettlementLedgerEntry {
        SettlementLedgerEntry {
            broadcast_outcome: BroadcastOutcome::Accepted {
                transaction_id: transaction_id.to_owned(),
            },
            settled_at_unix_seconds: 1_700_000_000,
            confirmation_count,
            mined_block_height,
            reorg_count: 0,
            last_reorged_at: None,
            expiry_height: Some(2_000_000),
        }
    }

    #[tokio::test]
    async fn poll_oracle_records_confirmations_for_mined_txs() -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("p1".to_owned());
        ledger
            .record(
                payment_id.clone(),
                accepted_ledger_entry("abcd", None, None),
            )
            .await
            .map_err(|_| "ledger record failed")?;
        let oracle = ScriptedOracle::new(&[(
            "abcd",
            ConfirmationOutcome::Mined {
                block_height: 1_234_567,
                confirmation_count: 5,
            },
        )]);

        let cache = PreparedTxCache::new();
        let events = PaymentEventHub::new();
        let chain_status = ChainStatusCache::new();
        poll_oracle_once(
            &oracle,
            &ledger,
            &cache,
            &events,
            &chain_status,
            DEFAULT_FINALITY_DEPTH,
        )
        .await;

        let snapshot = lookup_payment_status(
            &payment_id,
            &cache,
            &ledger,
            DEFAULT_FINALITY_DEPTH,
            chain_status.load(),
        )
        .await
        .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.confirmation_count, Some(5));
        assert_eq!(snapshot.mined_block_height, Some(1_234_567));
        Ok(())
    }

    #[tokio::test]
    async fn poll_downgrades_mined_row_on_not_found() -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("reorged".to_owned());
        ledger
            .record(
                payment_id.clone(),
                accepted_ledger_entry("abcd", Some(2), Some(1_900_000)),
            )
            .await
            .map_err(|_| "ledger record failed")?;
        // NotFound with the settled tip below the mined height: the poll
        // must treat the vanished mined row as a reorg and downgrade it.
        let oracle = ScriptedOracle::new(&[("abcd", ConfirmationOutcome::NotFound)])
            .with_chain_view(ChainStatusView {
                visible_tip_height: Some(1_900_050),
                settled_tip_height: Some(1_899_000),
            });
        let cache = PreparedTxCache::new();
        let events = PaymentEventHub::new();
        let chain_status = ChainStatusCache::new();
        poll_oracle_once(
            &oracle,
            &ledger,
            &cache,
            &events,
            &chain_status,
            DEFAULT_FINALITY_DEPTH,
        )
        .await;

        let entry = ledger
            .find(&payment_id)
            .await
            .map_err(|_| "find failed")?
            .ok_or("row missing")?;
        assert_eq!(entry.mined_block_height, None);
        assert_eq!(entry.confirmation_count, Some(0));
        assert_eq!(entry.reorg_count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn poll_downgrades_mined_row_on_conflicting_chain() -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("conflicted".to_owned());
        ledger
            .record(
                payment_id.clone(),
                accepted_ledger_entry("abcd", Some(1), Some(1_900_000)),
            )
            .await
            .map_err(|_| "ledger record failed")?;
        let oracle = ScriptedOracle::new(&[("abcd", ConfirmationOutcome::ConflictingChain)])
            .with_chain_view(ChainStatusView {
                visible_tip_height: Some(1_900_050),
                settled_tip_height: Some(1_899_000),
            });
        let cache = PreparedTxCache::new();
        let events = PaymentEventHub::new();
        let chain_status = ChainStatusCache::new();
        poll_oracle_once(
            &oracle,
            &ledger,
            &cache,
            &events,
            &chain_status,
            DEFAULT_FINALITY_DEPTH,
        )
        .await;

        let entry = ledger
            .find(&payment_id)
            .await
            .map_err(|_| "find failed")?
            .ok_or("row missing")?;
        assert_eq!(entry.reorg_count, 1);
        assert_eq!(entry.mined_block_height, None);
        Ok(())
    }

    #[tokio::test]
    async fn poll_downgrades_mined_row_returned_to_mempool() -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("back-to-mempool".to_owned());
        ledger
            .record(
                payment_id.clone(),
                accepted_ledger_entry("abcd", Some(2), Some(1_900_000)),
            )
            .await
            .map_err(|_| "ledger record failed")?;
        // A previously mined tx now reports InMempool: its block was
        // reorged out, so the row must downgrade, not keep the stale
        // mined height.
        let oracle = ScriptedOracle::new(&[("abcd", ConfirmationOutcome::InMempool)])
            .with_chain_view(ChainStatusView {
                visible_tip_height: Some(1_900_050),
                settled_tip_height: Some(1_899_000),
            });
        let cache = PreparedTxCache::new();
        let events = PaymentEventHub::new();
        let chain_status = ChainStatusCache::new();
        poll_oracle_once(
            &oracle,
            &ledger,
            &cache,
            &events,
            &chain_status,
            DEFAULT_FINALITY_DEPTH,
        )
        .await;

        let entry = ledger
            .find(&payment_id)
            .await
            .map_err(|_| "find failed")?
            .ok_or("row missing")?;
        assert_eq!(entry.mined_block_height, None);
        assert_eq!(entry.reorg_count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn poll_skips_settled_rows() -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("settled".to_owned());
        ledger
            .record(
                payment_id.clone(),
                accepted_ledger_entry("abcd", Some(150), Some(1_900_000)),
            )
            .await
            .map_err(|_| "ledger record failed")?;
        // The row is at or below the settled tip. Even though the oracle
        // would report NotFound, the poll must not consult it and must not
        // downgrade an immutable row.
        let oracle = ScriptedOracle::new(&[("abcd", ConfirmationOutcome::NotFound)])
            .with_chain_view(ChainStatusView {
                visible_tip_height: Some(1_900_200),
                settled_tip_height: Some(1_900_100),
            });
        let cache = PreparedTxCache::new();
        let events = PaymentEventHub::new();
        let chain_status = ChainStatusCache::new();
        poll_oracle_once(
            &oracle,
            &ledger,
            &cache,
            &events,
            &chain_status,
            DEFAULT_FINALITY_DEPTH,
        )
        .await;

        let entry = ledger
            .find(&payment_id)
            .await
            .map_err(|_| "find failed")?
            .ok_or("row missing")?;
        assert_eq!(entry.reorg_count, 0, "settled row must not be downgraded");
        assert_eq!(entry.mined_block_height, Some(1_900_000));

        let snapshot = lookup_payment_status(
            &payment_id,
            &cache,
            &ledger,
            DEFAULT_FINALITY_DEPTH,
            chain_status.load(),
        )
        .await
        .map_err(|_| "lookup failed")?;
        assert!(snapshot.settled);
        Ok(())
    }

    /// The literal `pay_to` baked into `etc/aether-demo.toml`.
    /// Used by the placeholder-shape tests below so a future tweak to
    /// the baked file does not silently invalidate the gate.
    const DEMO_PLACEHOLDER_PAY_TO: &str = "utest1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq";

    fn placeholder_entry() -> AcceptsEntry {
        AcceptsEntry {
            scheme: PaymentScheme::Zcash,
            network: PaymentNetwork::Testnet,
            pay_to: DEMO_PLACEHOLDER_PAY_TO.to_owned(),
            amount_zat: Zatoshis(1),
            max_validity_seconds: 1800,
            expiry_delta_blocks: None,
            merchant_requires_verify: false,
        }
    }

    fn real_entry() -> AcceptsEntry {
        AcceptsEntry {
            scheme: PaymentScheme::Zcash,
            network: PaymentNetwork::Testnet,
            // Real-shape UA (not all `q` past the HRP).
            pay_to: "utest1abcdefghijklmnopqrstuvwxyz0987654321abcdefghijklmnopqrstuvwxyz0987654321abcdefghijklmnopqrstuvwxyz0987654321abcdefghijklmnopqrs".to_owned(),
            amount_zat: Zatoshis(50_000),
            max_validity_seconds: 120,
            expiry_delta_blocks: None,
            merchant_requires_verify: false,
        }
    }

    #[test]
    fn placeholder_shape_matches_baked_file() {
        assert!(is_placeholder_pay_to(DEMO_PLACEHOLDER_PAY_TO));
    }

    #[test]
    fn placeholder_shape_matches_off_by_one_padding() {
        // Same shape (all-`q` past HRP) but one char shorter; still
        // load-bearing for the gate so a typo in the override file
        // does not slip through.
        let near = format!("utest1{}", "q".repeat(120));
        assert!(is_placeholder_pay_to(&near));
    }

    #[test]
    fn placeholder_shape_rejects_real_ua() {
        let entry = real_entry();
        assert!(!is_placeholder_pay_to(&entry.pay_to));
    }

    #[test]
    fn placeholder_shape_rejects_short_strings() {
        assert!(!is_placeholder_pay_to("utest1qq"));
        assert!(!is_placeholder_pay_to(""));
    }

    #[test]
    fn placeholder_shape_rejects_mainnet_prefix() {
        let mainnet_shaped = format!("u1{}", "q".repeat(128));
        assert!(!is_placeholder_pay_to(&mainnet_shaped));
    }

    #[test]
    fn validate_payees_rejects_placeholder_by_default() {
        let mut registry = PayeeRegistry::new();
        registry.register(PayeeId("aether-demo".to_owned()), vec![placeholder_entry()]);
        let outcome = validate_payees(&registry, false);
        assert!(matches!(
            outcome,
            Err(StartupError::PlaceholderPayee { ref payee_id }) if payee_id == "aether-demo",
        ));
    }

    #[test]
    fn validate_payees_accepts_placeholder_when_allow_demo_payee_set() {
        let mut registry = PayeeRegistry::new();
        registry.register(PayeeId("aether-demo".to_owned()), vec![placeholder_entry()]);
        let outcome = validate_payees(&registry, true);
        assert!(outcome.is_ok());
    }

    #[test]
    fn validate_payees_accepts_real_payees() {
        let mut registry = PayeeRegistry::new();
        registry.register(PayeeId("acme".to_owned()), vec![real_entry()]);
        let outcome = validate_payees(&registry, false);
        assert!(outcome.is_ok());
    }

    #[test]
    fn validate_payees_accepts_empty_registry() {
        let registry = PayeeRegistry::new();
        assert!(validate_payees(&registry, false).is_ok());
    }

    #[test]
    fn parse_truthy_matches_documented_vocabulary() {
        for truthy in ["1", "true", "TRUE", "True", "yes", "YES", " yes ", "  1  "] {
            assert!(parse_truthy(truthy), "expected {truthy:?} to be truthy");
        }
        for falsy in ["", " ", "0", "false", "no", "off", "enabled", "2", "y"] {
            assert!(!parse_truthy(falsy), "expected {falsy:?} to be falsy");
        }
    }

    #[tokio::test]
    async fn poll_oracle_leaves_failed_outcomes_alone() -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        ledger
            .record(
                PaymentId("rejected".to_owned()),
                SettlementLedgerEntry {
                    broadcast_outcome: BroadcastOutcome::Rejected {
                        upstream_message: "policy".to_owned(),
                    },
                    settled_at_unix_seconds: 1_700_000_000,
                    confirmation_count: None,
                    mined_block_height: None,
                    reorg_count: 0,
                    last_reorged_at: None,
                    expiry_height: None,
                },
            )
            .await
            .map_err(|_| "ledger record failed")?;
        // No outcomes scripted; the oracle returns Unavailable for any
        // txid. The Rejected entry should be skipped before the oracle is
        // even consulted because it carries no transaction_id.
        let oracle = ScriptedOracle::new(&[]);
        let cache = PreparedTxCache::new();
        let events = PaymentEventHub::new();
        let chain_status = ChainStatusCache::new();
        poll_oracle_once(
            &oracle,
            &ledger,
            &cache,
            &events,
            &chain_status,
            DEFAULT_FINALITY_DEPTH,
        )
        .await;
        Ok(())
    }
}

#[cfg(test)]
mod readiness_tests {
    use super::{ReadinessChainProbe, ReadinessStoreProbe, evaluate_readiness};
    use async_trait::async_trait;
    use axum::http::StatusCode;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use zpay_core::chain_status::{ChainStatusCache, ChainStatusView};

    struct FakeChainProbe(Result<ChainStatusView, String>);

    #[async_trait]
    impl ReadinessChainProbe for FakeChainProbe {
        async fn live_tip(&self) -> Result<ChainStatusView, String> {
            self.0.clone()
        }
    }

    struct FakeStoreProbe(bool);

    #[async_trait]
    impl ReadinessStoreProbe for FakeStoreProbe {
        async fn liveness(&self) -> Result<(), String> {
            if self.0 {
                Ok(())
            } else {
                Err("store down".to_owned())
            }
        }
    }

    fn addrs() -> (SocketAddr, SocketAddr) {
        (
            SocketAddr::from(([127, 0, 0, 1], 8080)),
            SocketAddr::from(([127, 0, 0, 1], 9295)),
        )
    }

    fn chain_probe(result: Result<ChainStatusView, String>) -> Arc<dyn ReadinessChainProbe> {
        Arc::new(FakeChainProbe(result))
    }

    fn store_probe(ok: bool) -> Arc<dyn ReadinessStoreProbe> {
        Arc::new(FakeStoreProbe(ok))
    }

    #[tokio::test]
    async fn ready_when_store_ok_chain_reachable_and_cache_fresh() {
        let chain = chain_probe(Ok(ChainStatusView {
            visible_tip_height: Some(100),
            settled_tip_height: Some(90),
        }));
        let store = store_probe(true);
        let cache = ChainStatusCache::new();
        cache.store(100, 90);
        let (app, ops) = addrs();
        let (status, body) = evaluate_readiness(Some(&chain), &store, &cache, app, ops).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ready");
        assert_eq!(body["dependencies"]["chain"]["live_probe"], "ok");
        assert_eq!(body["dependencies"]["chain"]["visible_tip_height"], 100);
        assert_eq!(body["listeners"]["app"], "127.0.0.1:8080");
    }

    #[tokio::test]
    async fn not_ready_when_store_probe_fails() {
        let chain = chain_probe(Ok(ChainStatusView::default()));
        let store = store_probe(false);
        let cache = ChainStatusCache::new();
        cache.store(1, 1);
        let (app, ops) = addrs();
        let (status, body) = evaluate_readiness(Some(&chain), &store, &cache, app, ops).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["dependencies"]["store"]["status"], "not_ready");
    }

    #[tokio::test]
    async fn not_ready_when_chain_unreachable() {
        let chain = chain_probe(Err("dial timeout".to_owned()));
        let store = store_probe(true);
        let cache = ChainStatusCache::new();
        cache.store(1, 1);
        let (app, ops) = addrs();
        let (status, body) = evaluate_readiness(Some(&chain), &store, &cache, app, ops).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["dependencies"]["chain"]["live_probe"], "unreachable");
    }

    #[tokio::test]
    async fn not_ready_when_cache_stale_even_if_probe_ok() {
        // A reachable probe but a never-refreshed cache (age unknown) is the
        // dead-poll-loop signal: the chain dependency must read not-ready.
        let chain = chain_probe(Ok(ChainStatusView::default()));
        let store = store_probe(true);
        let cache = ChainStatusCache::new();
        let (app, ops) = addrs();
        let (status, body) = evaluate_readiness(Some(&chain), &store, &cache, app, ops).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["dependencies"]["chain"]["status"], "not_ready");
        assert_eq!(body["dependencies"]["chain"]["live_probe"], "ok");
    }

    #[tokio::test]
    async fn ready_when_chain_not_configured() {
        let store = store_probe(true);
        let cache = ChainStatusCache::new();
        let (app, ops) = addrs();
        let (status, body) = evaluate_readiness(None, &store, &cache, app, ops).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["dependencies"]["chain"]["status"], "not_configured");
    }
}
