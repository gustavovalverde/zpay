//! zpay runtime binary: starts the HTTP listener, the ops listener, and the
//! signal handler.
//!
//! Configuration today is minimal: bind addresses and network read from
//! `ZPAY_*` env vars. The full layered config (TOML + env + CLI) lands in
//! M1; this scaffold only carries the env-var entry points needed to run
//! `/healthz` and the x402 stub routes.

mod zinder_broadcast;
mod zinder_oracle;
mod zinder_verify;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use clap::Parser;
use zinder_broadcast::ZinderBroadcastClient;
use zinder_client::Network as ZinderNetwork;
use zinder_oracle::ZinderConfirmationOracle;
use zinder_verify::ZinderDisclosureVerifier;
use zpay_core::accepts::{AcceptsEntry, MerchantRegistry};
use zpay_core::broadcast::{BroadcastClient, BroadcastError, BroadcastOutcome};
use zpay_core::oracle::{ConfirmationOracle, ConfirmationOutcome};
use zpay_core::prepare::{PreparedTxCache, PreparedTxStore};
use zpay_core::status::{SettlementLedger, SettlementLedgerStore};
use zpay_core::types::MerchantId;
use zpay_core::verify::{DisclosureVerdict, DisclosureVerifier, Verdict, VerifyError};
use zpay_x402::AppState;

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
    BroadcastClient {
        endpoint: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("merchant registry config read failed: {path}: {reason}")]
    MerchantsConfig { path: String, reason: String },
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

    let app_plane = build_app_router(&config)?;
    let app_router = app_plane.router;
    spawn_prepared_tx_sweeper(Arc::clone(&app_plane.cache));
    spawn_confirmation_oracle(&config, Arc::clone(&app_plane.ledger))?;
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
    cache: Arc<PreparedTxCache>,
    ledger: Arc<SettlementLedger>,
}

fn build_app_router(config: &ResolvedConfig) -> Result<AppPlane, StartupError> {
    let chain = build_broadcast_client(config)?;
    let verifier = build_disclosure_verifier(config)?;
    let merchants = load_merchant_registry(config)?;
    let cache = Arc::new(PreparedTxCache::new());
    let ledger = Arc::new(SettlementLedger::new());
    let state = AppState::new(
        Arc::clone(&cache),
        Arc::clone(&ledger),
        Arc::new(merchants),
        Arc::new(chain),
        Arc::new(verifier),
    );
    let router = Router::new().nest("/x402/v2", zpay_x402::router(state));

    #[cfg(feature = "mpp")]
    let router = router.nest("/mpp/v1", zpay_mpp::router());

    Ok(AppPlane {
        router: router.layer(tower_http::trace::TraceLayer::new_for_http()),
        cache,
        ledger,
    })
}

/// Interval at which the prepared-tx sweeper drops expired entries.
const PREPARED_TX_SWEEP_INTERVAL_SECONDS: u64 = 30;

/// Interval at which the confirmation oracle re-polls zinder.
///
/// Short enough that a freshly mined block is visible to agents within
/// a minute; long enough not to thrash the chain plane.
const CONFIRMATION_ORACLE_POLL_SECONDS: u64 = 60;

fn spawn_prepared_tx_sweeper(store: Arc<PreparedTxCache>) {
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
    ledger: Arc<SettlementLedger>,
) -> Result<(), StartupError> {
    let Some(endpoint) = config.indexer_grpc_addr.clone() else {
        tracing::warn!(
            "ZPAY_NODE__INDEXER_GRPC_ADDR unset; confirmation oracle disabled until a chain plane is configured",
        );
        return Ok(());
    };
    let oracle =
        ZinderConfirmationOracle::connect(endpoint.clone(), zinder_network_from_str(&config.network))
            .map_err(|source| StartupError::BroadcastClient {
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
            poll_oracle_once(oracle.as_ref(), ledger.as_ref()).await;
        }
    });
    tracing::info!("confirmation oracle wired");
    Ok(())
}

async fn poll_oracle_once<O, L>(oracle: &O, ledger: &L)
where
    O: ConfirmationOracle,
    L: SettlementLedgerStore + ?Sized,
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
        match oracle.fetch_confirmations(&transaction_id).await {
            Ok(ConfirmationOutcome::Mined {
                block_height,
                confirmation_count,
            }) => {
                if let Err(err) = ledger
                    .record_confirmation(&payment_id, confirmation_count, Some(block_height))
                    .await
                {
                    tracing::warn!(error = %err, "record_confirmation failed");
                }
            }
            Ok(ConfirmationOutcome::InMempool) => {
                if let Err(err) = ledger.record_confirmation(&payment_id, 0, None).await {
                    tracing::warn!(error = %err, "record_confirmation failed");
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
            }
            #[allow(
                clippy::wildcard_enum_match_arm,
                reason = "ConfirmationOutcome is #[non_exhaustive]; future variants stay a no-op until they have explicit handling"
            )]
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(
                    payment_id = %payment_id,
                    transaction_id = %transaction_id,
                    error = %err,
                    "confirmation oracle poll failed",
                );
            }
        }
    }
}

fn build_broadcast_client(config: &ResolvedConfig) -> Result<AnyBroadcastClient, StartupError> {
    let Some(endpoint) = config.indexer_grpc_addr.clone() else {
        tracing::warn!(
            "ZPAY_NODE__INDEXER_GRPC_ADDR unset; /x402/v2/settle will return 502 until a chain plane is configured",
        );
        return Ok(AnyBroadcastClient::Rejecting);
    };
    let zinder_network = zinder_network_from_str(&config.network);
    let client =
        ZinderBroadcastClient::connect(endpoint.clone(), zinder_network).map_err(|source| {
            StartupError::BroadcastClient {
                endpoint,
                source: Box::new(source),
            }
        })?;
    tracing::info!(
        network = %config.network,
        "zinder broadcast client wired",
    );
    Ok(AnyBroadcastClient::Zinder(Box::new(client)))
}

fn load_merchant_registry(config: &ResolvedConfig) -> Result<MerchantRegistry, StartupError> {
    let Some(path) = config.merchants_config_path.clone() else {
        return Ok(MerchantRegistry::new());
    };
    let raw = std::fs::read_to_string(&path).map_err(|source| StartupError::MerchantsConfig {
        path: path.clone(),
        reason: source.to_string(),
    })?;
    let parsed: MerchantsConfigFile =
        toml::from_str(&raw).map_err(|source| StartupError::MerchantsConfig {
            path: path.clone(),
            reason: source.to_string(),
        })?;
    let mut registry = MerchantRegistry::new();
    for (merchant_id, merchant) in parsed.merchants {
        registry.register(MerchantId(merchant_id), merchant.accepts);
    }
    tracing::info!(
        merchant_count = registry.merchant_count(),
        path = %path,
        "merchant registry loaded",
    );
    Ok(registry)
}

#[derive(Debug, serde::Deserialize)]
struct MerchantsConfigFile {
    #[serde(default)]
    merchants: std::collections::HashMap<String, MerchantsConfigEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct MerchantsConfigEntry {
    #[serde(default)]
    accepts: Vec<AcceptsEntry>,
}

const fn zinder_network_from_str(raw: &str) -> ZinderNetwork {
    match raw.as_bytes() {
        b"mainnet" => ZinderNetwork::ZcashMainnet,
        b"testnet" => ZinderNetwork::ZcashTestnet,
        _ => ZinderNetwork::ZcashRegtest,
    }
}

/// Concrete broadcast client variant chosen at startup.
///
/// Using an enum (rather than `Arc<dyn BroadcastClient>`) keeps the
/// `impl Future + Send` return type from [`BroadcastClient::broadcast`]
/// statically resolvable without an `async-trait` allocation per call.
enum AnyBroadcastClient {
    /// Placeholder when no chain plane is configured. Every call returns
    /// `BroadcastError::Unavailable` so the settle handler returns 502.
    Rejecting,
    /// Production client backed by zinder's `WalletQuery.BroadcastTransaction`.
    ///
    /// Boxed because the underlying `RemoteChainIndex` carries a tonic
    /// `Endpoint` of several hundred bytes, while the `Rejecting` variant
    /// is unit-sized; clippy's `large_enum_variant` rule prefers
    /// indirection.
    Zinder(Box<ZinderBroadcastClient>),
}

impl BroadcastClient for AnyBroadcastClient {
    async fn broadcast(&self, raw_tx_hex: &str) -> Result<BroadcastOutcome, BroadcastError> {
        match self {
            Self::Rejecting => Err(BroadcastError::Unavailable {
                reason: "broadcast client not configured; set ZPAY_NODE__INDEXER_GRPC_ADDR to enable settle"
                    .to_owned(),
            }),
            Self::Zinder(client) => client.broadcast(raw_tx_hex).await,
        }
    }
}

/// Concrete disclosure-verifier variant chosen at startup.
enum AnyDisclosureVerifier {
    /// Placeholder when no explorer endpoint is configured. Returns
    /// `Verdict::CapabilityUnavailable` so the `/verify` handler surfaces
    /// 503 `capability_unavailable`.
    CapabilityUnavailable,
    /// Production verifier backed by zinder's
    /// `ExplorerQuery.VerifyPaymentDisclosure`.
    Zinder(Box<ZinderDisclosureVerifier>),
}

impl DisclosureVerifier for AnyDisclosureVerifier {
    async fn verify_disclosure(
        &self,
        disclosure_bytes: &[u8],
    ) -> Result<DisclosureVerdict, VerifyError> {
        match self {
            Self::CapabilityUnavailable => Ok(DisclosureVerdict {
                verdict: Verdict::CapabilityUnavailable,
                transaction_id: None,
                payment_id: None,
                disclosed_value_zat: None,
            }),
            Self::Zinder(verifier) => verifier.verify_disclosure(disclosure_bytes).await,
        }
    }
}

fn build_disclosure_verifier(
    config: &ResolvedConfig,
) -> Result<AnyDisclosureVerifier, StartupError> {
    let Some(endpoint) = config.explorer_grpc_addr.clone() else {
        tracing::warn!(
            "ZPAY_NODE__EXPLORER_GRPC_ADDR unset; /x402/v2/verify returns capability_unavailable until an explorer plane is configured",
        );
        return Ok(AnyDisclosureVerifier::CapabilityUnavailable);
    };
    let verifier = ZinderDisclosureVerifier::connect(endpoint.clone()).map_err(|source| {
        StartupError::BroadcastClient {
            endpoint,
            source: Box::new(source),
        }
    })?;
    tracing::info!("zinder disclosure verifier wired");
    Ok(AnyDisclosureVerifier::Zinder(Box::new(verifier)))
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
        r#"{"status":"starting","reason":"dependency probes not yet implemented (M1)"}"#,
    )
}

#[derive(Debug, Clone)]
struct ResolvedConfig {
    app_bind_addr: SocketAddr,
    ops_bind_addr: SocketAddr,
    network: String,
    indexer_grpc_addr: Option<String>,
    explorer_grpc_addr: Option<String>,
    merchants_config_path: Option<String>,
}

impl ResolvedConfig {
    fn from_env() -> Result<Self, StartupError> {
        let app_bind_raw = std::env::var("ZPAY_SERVER__BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string());
        let ops_bind_raw =
            std::env::var("ZPAY_OPS__BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:9295".to_string());
        let network = std::env::var("ZPAY_NETWORK").unwrap_or_else(|_| "regtest".to_string());
        let indexer_grpc_addr = std::env::var("ZPAY_NODE__INDEXER_GRPC_ADDR")
            .ok()
            .filter(|raw| !raw.trim().is_empty());
        let explorer_grpc_addr = std::env::var("ZPAY_NODE__EXPLORER_GRPC_ADDR")
            .ok()
            .filter(|raw| !raw.trim().is_empty());
        let merchants_config_path = std::env::var("ZPAY_MERCHANTS__CONFIG_PATH")
            .ok()
            .filter(|raw| !raw.trim().is_empty());

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
        Ok(Self {
            app_bind_addr,
            ops_bind_addr,
            network,
            indexer_grpc_addr,
            explorer_grpc_addr,
            merchants_config_path,
        })
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
    use super::{ConfirmationOracle, ConfirmationOutcome, poll_oracle_once};
    use parking_lot::Mutex;
    use zpay_core::broadcast::BroadcastOutcome;
    use zpay_core::oracle::OracleError;
    use zpay_core::status::{
        SettlementLedger, SettlementLedgerEntry, SettlementLedgerStore, lookup_payment_status,
    };
    use zpay_core::prepare::PreparedTxCache;
    use zpay_core::types::PaymentId;

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

        poll_oracle_once(&oracle, &ledger).await;

        let cache = PreparedTxCache::new();
        let snapshot = lookup_payment_status(&payment_id, &cache, &ledger)
            .await
            .map_err(|_| "lookup failed")?;
        assert_eq!(snapshot.confirmation_count, Some(5));
        assert_eq!(snapshot.mined_block_height, Some(1_234_567));
        Ok(())
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
        poll_oracle_once(&oracle, &ledger).await;
        Ok(())
    }
}
