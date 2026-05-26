//! zpay runtime binary: starts the HTTP listener, the ops listener, and the
//! signal handler.
//!
//! Configuration today is minimal: bind addresses and network read from
//! `ZPAY_*` env vars. The full layered config (TOML + env + CLI) lands in
//! M1; this scaffold only carries the env-var entry points needed to run
//! `/healthz` and the x402 stub routes.

mod zinder_broadcast;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use clap::Parser;
use zinder_broadcast::ZinderBroadcastClient;
use zinder_client::Network as ZinderNetwork;
use zpay_core::accepts::{AcceptsEntry, MerchantRegistry};
use zpay_core::broadcast::{BroadcastClient, BroadcastError, BroadcastOutcome};
use zpay_core::prepare::PreparedTxCache;
use zpay_core::status::SettlementLedger;
use zpay_core::types::MerchantId;
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
}

fn build_app_router(config: &ResolvedConfig) -> Result<AppPlane, StartupError> {
    let chain = build_broadcast_client(config)?;
    let merchants = load_merchant_registry(config)?;
    let cache = Arc::new(PreparedTxCache::new());
    let state = AppState::new(
        Arc::clone(&cache),
        Arc::new(SettlementLedger::new()),
        Arc::new(merchants),
        Arc::new(chain),
    );
    let router = Router::new().nest("/x402/v2", zpay_x402::router(state));

    #[cfg(feature = "mpp")]
    let router = router.nest("/mpp/v1", zpay_mpp::router());

    Ok(AppPlane {
        router: router.layer(tower_http::trace::TraceLayer::new_for_http()),
        cache,
    })
}

/// Interval at which the prepared-tx sweeper drops expired entries.
const PREPARED_TX_SWEEP_INTERVAL_SECONDS: u64 = 30;

fn spawn_prepared_tx_sweeper(cache: Arc<PreparedTxCache>) {
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
            let dropped = cache.sweep_expired(now_unix_seconds);
            if dropped > 0 {
                tracing::info!(
                    dropped_count = dropped,
                    "prepared-tx sweeper dropped expired entries",
                );
            }
        }
    });
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
