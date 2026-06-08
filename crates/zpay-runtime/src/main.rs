//! zpay runtime binary: starts the HTTP listener, the ops listener, and the
//! signal handler.
//!
//! Configuration today is minimal: bind addresses and network read from
//! `ZPAY_*` env vars. The full layered config (TOML + env + CLI) lands in
//! M1; this scaffold only carries the env-var entry points needed to run
//! `/healthz` and the x402 stub routes.

mod rejecting_fetcher;
mod tip_oracle;
mod zinder_fetcher;
mod zinder_oracle;
mod zinder_submitter;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use clap::Parser;
use rejecting_fetcher::RejectingTransactionFetcher;
use tip_oracle::{AnyTipOracle, StaticTipOracle, ZinderTipOracle};
use zinder_client::Network as ZinderNetwork;
use zinder_fetcher::ZinderTransactionFetcher;
use zinder_oracle::ZinderConfirmationOracle;
use zinder_submitter::ZinderSubmitter;
use zpay_core::accepts::{AcceptsEntry, PayeeRegistry};
use zpay_core::disclosure_fetcher::{DisclosedTransaction, DisclosureFetcher, FetchError};
use zpay_core::oracle::{ConfirmationOracle, ConfirmationOutcome};
use zpay_core::prepare::{PreparedTxCache, PreparedTxEntry, PreparedTxStore};
use zpay_core::status::{
    DEFAULT_FINALITY_DEPTH, SettlementLedger, SettlementLedgerEntry, SettlementLedgerStore,
    lookup_payment_status,
};
use zpay_core::store::StoreError;
use zpay_core::types::{PayeeId, PaymentId, PaymentNetwork};
use zpay_core::verify::LocalPaymentDisclosureVerifier;
use zpay_store::{LibsqlPreparedTxStore, LibsqlSettlementLedgerStore, open_and_migrate};
use zpay_x402::{AppState, DpopExpectations, DpopInMemoryReplayStore, PaymentEventHub};

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

    let app_plane = build_app_router(&config).await?;
    let app_router = app_plane.router;
    spawn_prepared_tx_sweeper(Arc::clone(&app_plane.prepared_store));
    spawn_confirmation_oracle(
        &config,
        Arc::clone(&app_plane.ledger),
        Arc::clone(&app_plane.prepared_store),
        Arc::clone(&app_plane.events),
        config.finality_depth,
    )?;
    let ops_router = build_ops_router();

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
    let app_serve = axum::serve(app_listener, app_router).with_graceful_shutdown(shutdown_signal());
    let ops_serve = axum::serve(ops_listener, ops_router).with_graceful_shutdown(shutdown);

    tokio::try_join!(app_serve, ops_serve).map_err(|source| StartupError::Serve { source })?;
    Ok(())
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

    let expectations = build_dpop_expectations(config);
    let replay_store: Arc<dyn zpay_x402::DpopReplayStore> =
        Arc::new(DpopInMemoryReplayStore::new());
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
    );
    // `/healthz` lives on the app listener too so platform health probes
    // (Railway, Kubernetes, AWS ALB) that only reach the public port get a
    // 200 from a healthy process. The ops listener also serves it so
    // intra-cluster sidecars keep the existing path.
    let router = Router::new()
        .route("/healthz", get(healthz))
        .nest("/x402/v2", zpay_x402::router(state));

    Ok(AppPlane {
        router: router.layer(tower_http::trace::TraceLayer::new_for_http()),
        prepared_store,
        ledger,
        events,
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

    async fn success_kind_transactions(&self) -> Result<Vec<(PaymentId, String)>, StoreError> {
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

fn spawn_confirmation_oracle(
    config: &ResolvedConfig,
    ledger: Arc<AnySettlementLedgerStore>,
    prepared_store: Arc<AnyPreparedTxStore>,
    events: Arc<PaymentEventHub>,
    finality_depth: u32,
) -> Result<(), StartupError> {
    let Some(endpoint) = config.indexer_grpc_addr.clone() else {
        tracing::warn!(
            "ZPAY_CHAIN_SOURCE_URL unset; confirmation oracle disabled until a chain plane is configured",
        );
        return Ok(());
    };
    let oracle = ZinderConfirmationOracle::connect(
        endpoint.clone(),
        zinder_network_from_str(&config.network)?,
    )
    .map_err(|source| StartupError::Submitter {
        endpoint,
        source: Box::new(source),
    })?;
    let oracle = Arc::new(oracle);
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
                finality_depth,
            )
            .await;
        }
    });
    tracing::info!(finality_depth, "confirmation oracle wired");
    Ok(())
}

async fn poll_oracle_once<O, L, P>(
    oracle: &O,
    ledger: &L,
    prepared_store: &P,
    events: &PaymentEventHub,
    finality_depth: u32,
) where
    O: ConfirmationOracle,
    L: SettlementLedgerStore + ?Sized,
    P: zpay_core::prepare::PreparedTxStore + ?Sized,
{
    let entries = match ledger.success_kind_transactions().await {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(error = %err, "ledger lookup for oracle tick failed");
            return;
        }
    };
    if entries.is_empty() {
        return;
    }
    for (payment_id, transaction_id) in entries {
        let changed = match oracle.fetch_confirmations(&transaction_id).await {
            Ok(ConfirmationOutcome::Mined {
                block_height,
                confirmation_count,
            }) => match ledger
                .record_confirmation(&payment_id, confirmation_count, Some(block_height))
                .await
            {
                Ok(updated) => updated,
                Err(err) => {
                    tracing::warn!(error = %err, "record_confirmation failed");
                    false
                }
            },
            Ok(ConfirmationOutcome::InMempool) => {
                match ledger.record_confirmation(&payment_id, 0, None).await {
                    Ok(updated) => updated,
                    Err(err) => {
                        tracing::warn!(error = %err, "record_confirmation failed");
                        false
                    }
                }
            }
            Ok(ConfirmationOutcome::NotFound | ConfirmationOutcome::ConflictingChain) => {
                // Leave confirmation_count untouched; an Accepted broadcast
                // that vanishes from the chain plane is operator-visible
                // through the unchanged ledger state and the warning below.
                tracing::warn!(
                    payment_id = %payment_id,
                    transaction_id = %transaction_id,
                    "oracle reports tx no longer visible on chain plane",
                );
                false
            }
            #[allow(
                clippy::wildcard_enum_match_arm,
                reason = "ConfirmationOutcome is #[non_exhaustive]; future variants stay a no-op until they have explicit handling"
            )]
            Ok(_) => false,
            Err(err) => {
                tracing::warn!(
                    payment_id = %payment_id,
                    transaction_id = %transaction_id,
                    error = %err,
                    "confirmation oracle poll failed",
                );
                false
            }
        };

        if changed {
            // The hub never inserts on publish, so this is a no-op for
            // payments without a live SSE subscriber. We only re-read
            // the snapshot when the ledger actually changed, so an idle
            // oracle tick costs one `find` per success-kind row and
            // nothing more.
            match lookup_payment_status(&payment_id, prepared_store, ledger, finality_depth).await {
                Ok(snapshot) => events.publish(&payment_id, snapshot),
                Err(err) => {
                    tracing::warn!(
                        payment_id = %payment_id,
                        error = %err,
                        "snapshot read for oracle publish failed",
                    );
                }
            }
        }
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
                reason:
                    "submitter not configured; set ZPAY_CHAIN_SOURCE_URL to enable settle"
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

fn build_ops_router() -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
}

async fn healthz() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        r#"{"status":"alive"}"#,
    )
}

async fn readyz() -> impl IntoResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [("content-type", "application/json")],
        r#"{"status":"starting","reason":"dependency probes not implemented yet"}"#,
    )
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
        })
    }
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
    use zpay_core::prepare::PreparedTxCache;
    use zpay_core::status::{
        DEFAULT_FINALITY_DEPTH, SettlementLedger, SettlementLedgerEntry, SettlementLedgerStore,
        lookup_payment_status,
    };
    use zpay_x402::PaymentEventHub;

    struct ScriptedOracle {
        outcomes: Mutex<std::collections::HashMap<String, ConfirmationOutcome>>,
    }

    impl ScriptedOracle {
        fn new(pairs: &[(&'static str, ConfirmationOutcome)]) -> Self {
            let mut outcomes = std::collections::HashMap::new();
            for (txid, outcome) in pairs {
                outcomes.insert((*txid).to_owned(), outcome.clone());
            }
            Self {
                outcomes: Mutex::new(outcomes),
            }
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
    }

    #[tokio::test]
    async fn poll_oracle_records_confirmations_for_mined_txs() -> Result<(), &'static str> {
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("p1".to_owned());
        ledger
            .record(
                payment_id.clone(),
                SettlementLedgerEntry {
                    broadcast_outcome: BroadcastOutcome::Accepted {
                        transaction_id: "abcd".to_owned(),
                    },
                    settled_at_unix_seconds: 1_700_000_000,
                    confirmation_count: None,
                    mined_block_height: None,
                },
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
        poll_oracle_once(&oracle, &ledger, &cache, &events, DEFAULT_FINALITY_DEPTH).await;

        let snapshot = lookup_payment_status(&payment_id, &cache, &ledger, DEFAULT_FINALITY_DEPTH)
            .await
            .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.confirmation_count, Some(5));
        assert_eq!(snapshot.mined_block_height, Some(1_234_567));
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
        poll_oracle_once(&oracle, &ledger, &cache, &events, DEFAULT_FINALITY_DEPTH).await;
        Ok(())
    }
}
