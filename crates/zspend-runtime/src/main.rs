//! Wallet runtime binary for the zspend service.
//!
//! Proposal-0003 (see
//! `docs/proposals/0003-agent-wallet-production-architecture.md`). The binary
//! opens a zally-backed wallet against a sealed seed, exposes
//! `/v1/payments/sign` for the agent flow, and reports liveness and readiness
//! on the standard probes.
//!
//! Every `/v1/payments/sign` call clears the landed trust boundary before any
//! signing work: it verifies the DPoP-bound `at+jwt` against the issuer JWKS
//! with the audience pinned to this wallet (D-1, D-5), re-derives the
//! `intent_hash` from the parsed payment request and compares it to the signed
//! RAR (D-4), consults the revocation cache on the access-token `jti` so a
//! grant pulled after mint is rejected ahead of any cached payload (D-6), then
//! reserves the `jti` in the single-use ledger before signing so an identical
//! replay returns the cached payload and a conflicting reuse is refused (D-8).
//! Revocation enforcement is disabled (always admits) when no
//! `ZSPEND_ISSUER_URL` is configured; the readiness probe then reports its
//! cache as `disabled`.

mod bootstrap;
mod capture_submitter;
mod init;
mod ledger;
mod revocation;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD;
use bootstrap::{BootstrapError, BootstrapInputs, ChainSourceFactory};
use capture_submitter::CaptureSubmitter;
use clap::{Parser, Subcommand};
use jsonwebtoken::jwk::JwkSet;
use ledger::{Reservation, UsageLedger};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use parking_lot::Mutex;
use revocation::{RevocationOutcome, RevocationStore};
use serde::{Deserialize, Serialize};
use zally_chain::ChainSource;
use zally_core::{AccountId, BlockHeight, Network, PaymentRecipient};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions, SealingPosture, SeedSealing as _};
use zally_pczt::PcztBytes;
use zally_wallet::{
    PaymentRequest, ProposalPlan, SendOutcome, SyncDriver, SyncDriverOptions, SyncDriverPhase,
    SyncHandle, SyncSnapshot, SyncStatus, Wallet, WalletError,
};
use zspend_core::{
    AccessTokenClaims, DPOP_CLOCK_SKEW_SECONDS, DpopBinding, PaymentAuthorization, ProblemDetail,
    ProblemKind, SigningPolicy, SigningPolicyError, intent_matches, recompute_intent_hash,
    verify_access_token, verify_dpop_proof,
};

/// Wire format identifier returned on `/v1/payments/sign`.
///
/// zspend returns a signed, proven, extractor-ready PCZT that can be placed in
/// the Zcash exact x402 authorization object.
const SIGNED_PAYLOAD_FORMAT_WIRE_LITERAL: &str = "pczt-v2-extractable";

/// `zspend-runtime` command-line entry point.
///
/// `serve` (the default when no subcommand is given) preserves the Phase 4
/// axum behavior: open the wallet against the configured sealed seed and
/// expose `/v1/payments/sign` plus the probes. `init` provisions a fresh
/// sealed seed at `$ZSPEND_SEALED_SEED_PATH` so a freshly mounted volume
/// can boot the runtime without an operator stepping in.
#[derive(Debug, Parser)]
#[command(name = "zspend-runtime", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the wallet runtime HTTP listener (default).
    Serve,
    /// Provision the wallet seed sealed at `$ZSPEND_SEALED_SEED_PATH`.
    ///
    /// Generates a fresh BIP-39 mnemonic and reveals it once, or with
    /// `--restore` seals a seed derived from a mnemonic read on stdin.
    Init {
        /// Overwrite an existing sealed seed instead of refusing.
        #[arg(long)]
        force: bool,
        /// Seal a seed derived from a BIP-39 mnemonic read on stdin instead of
        /// generating a fresh one.
        #[arg(long)]
        restore: bool,
        /// Suppress the one-time mnemonic reveal for the docker auto-provision
        /// path. The seed is sealed but never printed; a log line records that
        /// an unbacked throwaway dev seed was provisioned. Never pass this when
        /// provisioning a wallet an operator must back up.
        #[arg(long)]
        auto_provision: bool,
        /// Override `$ZSPEND_SEALED_SEED_PATH` for this invocation.
        #[arg(long, env = "ZSPEND_SEALED_SEED_PATH")]
        sealed_seed_path: PathBuf,
    },
}

#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error("invalid bind address {provided:?}: {source}")]
    BindAddress {
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
    #[error("ZSPEND_NETWORK has unknown value {provided:?}; expected mainnet, testnet, or regtest")]
    NetworkInvalid { provided: String },
    #[error("required env var {name} is missing")]
    EnvMissing { name: &'static str },
    #[error("issuer JWKS load failed for {path:?}: {reason}")]
    Jwks { path: PathBuf, reason: String },
    #[error("wallet bootstrap failed: {source}")]
    Bootstrap {
        #[source]
        source: BootstrapError,
    },
    #[error("wallet sync driver failed to start: {source}")]
    SyncDriver {
        #[source]
        source: WalletError,
    },
    #[error("signing policy build failed: {source}")]
    SigningPolicy {
        #[source]
        source: SigningPolicyError,
    },
    #[error("init subcommand failed: {source}")]
    Init {
        #[source]
        source: init::InitError,
    },
    #[error(
        "production posture requires {missing}; refusing to start on mainnet without it (D-13)"
    )]
    ProductionPostureUnmet { missing: &'static str },
    #[error(
        "seed is sealed with a dev-posture implementation (age file); refusing to serve. Front the seed with a KMS/HSM sealing, or set ZSPEND_ALLOW_DEV_SEED=1 to accept a dev-posture seed (D-13)"
    )]
    DevSeedPostureRefused,
    #[error("single-use ledger open failed: {source}")]
    Ledger {
        #[source]
        source: ledger::LedgerError,
    },
}

/// Window (seconds) after which a recorded DPoP proof `jti` is evicted from the
/// per-process anti-replay set.
const DPOP_REPLAY_WINDOW_SECONDS: u64 = 300;

/// Default revocation-cache staleness ceiling, in seconds.
///
/// Used when `ZSPEND_REVOCATION_MAX_STALENESS_SECONDS` is unset, and reused as
/// the `Retry-After` hint on a `revocation_cache_stale` response.
const DEFAULT_REVOCATION_MAX_STALENESS_SECONDS: u64 = 30;

/// Default wallet sync polling cadence when no chain event arrives.
const DEFAULT_WALLET_SYNC_POLL_INTERVAL_MS: u64 = 5_000;

/// Default maximum number of wallet sync iterations per driver wakeup.
const DEFAULT_WALLET_SYNC_MAX_ITERATIONS_PER_WAKE_COUNT: u32 = 1_000;

/// Default timeout for one wallet sync iteration.
const DEFAULT_WALLET_SYNC_TIMEOUT_SECONDS: u64 = 120;

/// Default maximum lag tolerated before the wallet refuses signing.
const DEFAULT_WALLET_SYNC_MAX_LAG_BLOCKS: u32 = 3;

/// Default maximum age for the most recent sync snapshot.
const DEFAULT_WALLET_SYNC_STALE_AFTER_SECONDS: u64 = 30;

/// Retry hint for a wallet that is catching up, recovering, or parked.
const DEFAULT_WALLET_SYNC_RETRY_SECONDS: u64 = 5;

#[derive(Clone)]
struct AppState {
    wallet: Wallet,
    account_id: AccountId,
    policy: Arc<SigningPolicy>,
    /// Issuer JWKS used to verify inbound `at+jwt` access tokens (D-1). Empty
    /// when no `ZSPEND_JWKS_FILE` is configured, which fails every spend closed.
    jwks: Arc<JwkSet>,
    /// The wallet's externally-reachable `/v1/payments/sign` URL, compared
    /// against the DPoP proof `htu` (D-5, RFC 9449).
    public_sign_url: Arc<str>,
    /// Clock-skew leeway (seconds) for access-token `exp` and DPoP `iat`.
    leeway_seconds: u64,
    /// Operational posture reported on `/readyz` (e.g. `"dev"`,
    /// `"production"`), pinned at startup from config.
    posture: &'static str,
    /// At-rest seal posture of the sealing implementation (D-13). Reported on
    /// `/readyz`; a `Dev` posture requires `ZSPEND_ALLOW_DEV_SEED` to serve.
    seal_posture: SealingPosture,
    /// Short-window DPoP proof `jti` anti-replay set (per-process).
    dpop_proof_jti_seen: Arc<Mutex<HashMap<String, Instant>>>,
    /// Single-use access-token `jti` ledger (reserve-before-sign, D-8),
    /// libSQL-backed so the guarantee survives a restart. A reserved `jti` is
    /// committed with its signed payload after signing so an identical replay
    /// returns the cached payload instead of re-signing.
    spend_idempotency_cache: UsageLedger,
    /// Delta-synced access-token revocation cache (D-6). Consulted on
    /// `claims.jti` before the single-use reserve so a revoked-then-replayed
    /// token is rejected ahead of the cached payload. Disabled (always admits)
    /// when no `ZSPEND_ISSUER_URL` is configured.
    revocation: RevocationStore,
    /// Latest long-lived zally sync driver snapshot.
    wallet_sync: WalletSyncState,
    /// Maximum wallet lag accepted by `/readyz` and `/v1/payments/sign`.
    wallet_sync_max_lag_blocks: u32,
    /// Maximum accepted age for the latest sync snapshot.
    wallet_sync_stale_after_seconds: u64,
    /// Prometheus render handle served on `/metrics`.
    metrics: PrometheusHandle,
}

#[derive(Clone)]
struct WalletSyncState {
    snapshot: Arc<Mutex<WalletSyncSnapshot>>,
}

impl WalletSyncState {
    fn new(snapshot: WalletSyncSnapshot) -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(snapshot)),
        }
    }

    fn snapshot(&self) -> WalletSyncSnapshot {
        self.snapshot.lock().clone()
    }

    fn publish(&self, snapshot: WalletSyncSnapshot) {
        *self.snapshot.lock() = snapshot;
    }
}

#[derive(Clone, Debug)]
struct WalletSyncSnapshot {
    network: Network,
    phase: WalletSyncPhase,
    sync_status: WalletSyncScanStatus,
    scanned_height: Option<BlockHeight>,
    safe_chain_tip_height: Option<BlockHeight>,
    lag_blocks: Option<u32>,
    last_fault: Option<WalletSyncFault>,
    published_at_ms: u64,
}

impl From<SyncSnapshot> for WalletSyncSnapshot {
    fn from(snapshot: SyncSnapshot) -> Self {
        Self {
            network: snapshot.network,
            phase: WalletSyncPhase::from(&snapshot.phase),
            sync_status: WalletSyncScanStatus::from(&snapshot.sync_status),
            scanned_height: snapshot.scanned_height,
            safe_chain_tip_height: snapshot.safe_chain_tip_height,
            lag_blocks: snapshot.lag_blocks,
            last_fault: snapshot.last_fault.map(WalletSyncFault::from),
            published_at_ms: snapshot.published_at_ms,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalletSyncPhase {
    Starting,
    Syncing,
    Waiting,
    Recovering,
    Parked,
    Closing,
    Closed,
    Unknown,
}

impl From<&SyncDriverPhase> for WalletSyncPhase {
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "SyncDriverPhase is non_exhaustive; future phases fail readiness as unknown"
    )]
    fn from(phase: &SyncDriverPhase) -> Self {
        match phase {
            SyncDriverPhase::Starting => Self::Starting,
            SyncDriverPhase::Syncing => Self::Syncing,
            SyncDriverPhase::Waiting => Self::Waiting,
            SyncDriverPhase::Recovering { .. } => Self::Recovering,
            SyncDriverPhase::Parked { .. } => Self::Parked,
            SyncDriverPhase::Closing => Self::Closing,
            SyncDriverPhase::Closed => Self::Closed,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalletSyncScanStatus {
    NotStarted,
    WaitingForTip,
    Starting,
    CatchingUp,
    AtTip,
    TipRegressed,
    Unknown,
}

impl From<&SyncStatus> for WalletSyncScanStatus {
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "SyncStatus is non_exhaustive; future states report unknown"
    )]
    fn from(status: &SyncStatus) -> Self {
        match status {
            SyncStatus::NotStarted => Self::NotStarted,
            SyncStatus::WaitingForTip { .. } => Self::WaitingForTip,
            SyncStatus::Starting { .. } => Self::Starting,
            SyncStatus::CatchingUp { .. } => Self::CatchingUp,
            SyncStatus::AtTip { .. } => Self::AtTip,
            SyncStatus::TipRegressed { .. } => Self::TipRegressed,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
struct WalletSyncFault {
    reason: String,
    repair: String,
    occurred_at_ms: u64,
    consecutive_faults: u32,
}

impl From<zally_wallet::SyncFault> for WalletSyncFault {
    fn from(fault: zally_wallet::SyncFault) -> Self {
        Self {
            reason: fault.reason,
            repair: fault.repair.label().to_owned(),
            occurred_at_ms: fault.occurred_at_ms,
            consecutive_faults: fault.consecutive_faults,
        }
    }
}

impl AppState {
    /// Returns the canonical Unified Address an operator can fund.
    ///
    /// Idempotent: surfaces the first previously-exposed UA when one is on
    /// record, otherwise derives a fresh UA with a transparent receiver so a
    /// vanilla testnet wallet can credit it. The result is encoded against the
    /// policy network so the wire string carries the `utest1…` (or
    /// `u1…`/`uregtest1…`) prefix consumers grep for.
    pub(crate) async fn current_unified_address(&self) -> Result<String, WalletError> {
        let network = self.policy.network();
        let params = network.to_parameters();
        let exposed = self.wallet.list_exposed_addresses(self.account_id).await?;
        if let Some(row) = exposed.into_iter().next() {
            return Ok(row.unified_address.encode(&params));
        }
        let ua = self
            .wallet
            .derive_next_address_with_transparent(self.account_id)
            .await?;
        Ok(ua.encode(&params))
    }
}

#[tokio::main]
async fn main() -> Result<(), StartupError> {
    install_tracing()?;
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::Init {
            force,
            restore,
            auto_provision,
            sealed_seed_path,
        } => init::run(sealed_seed_path, force, restore, auto_provision)
            .await
            .map_err(|source| StartupError::Init { source }),
    }
}

async fn serve() -> Result<(), StartupError> {
    let config = ResolvedConfig::from_env()?;
    let metrics = install_metrics_recorder()?;
    let revocation = build_revocation_store(&config);
    let seal_posture = age_seal_posture(&config.sealed_seed_path);
    metrics::gauge!("zspend_seal_posture_info", "posture" => seal_posture_str(seal_posture))
        .set(1.0);

    let spend_idempotency_cache = open_spend_idempotency_cache(&config).await?;
    let (wallet, account_id, chain) = bootstrap::bootstrap(BootstrapInputs {
        network: config.network,
        sealed_seed_path: config.sealed_seed_path.clone(),
        storage_path: config.storage_path.clone(),
        indexer_grpc_addr: config.indexer_grpc_addr.clone(),
        birthday_override: config.birthday_override,
        chain_source_factory: ChainSourceFactory::Live,
    })
    .await
    .map_err(|source| StartupError::Bootstrap { source })?;

    let wallet_sync = start_wallet_sync(&wallet, chain, &config)?;
    let policy = build_signing_policy(&config)?;
    let jwks = load_jwks(config.jwks_file.as_ref())?;
    if jwks.keys.is_empty() {
        tracing::warn!(
            "no ZSPEND_JWKS_FILE configured: every /v1/payments/sign call fails closed until the issuer JWKS is wired",
        );
    }

    // D-13: refuse to serve a dev-posture seed without an explicit override, and
    // refuse a mainnet (production) deploy that lacks the issuer wiring that makes
    // the trust boundary real. Without a JWKS every spend fails closed (no harm),
    // but without an issuer URL revocation is silently disabled, so a grant the
    // issuer pulls after mint would still sign.
    enforce_production_posture(
        config.posture,
        config.issuer_url.is_some(),
        jwks.keys.is_empty(),
        seal_posture,
        config.allow_dev_seed,
    )?;

    let router = build_router(build_app_state(
        &config,
        RuntimeParts {
            wallet,
            account_id,
            policy,
            jwks,
            spend_idempotency_cache,
            revocation,
            wallet_sync,
            metrics,
            seal_posture,
        },
    ));

    serve_router(config.bind_addr, router).await
}

async fn open_spend_idempotency_cache(
    config: &ResolvedConfig,
) -> Result<UsageLedger, StartupError> {
    let cache = UsageLedger::open(&config.ledger_url, config.ledger_auth_token.as_deref())
        .await
        .map_err(|source| StartupError::Ledger { source })?;
    tracing::info!(
        ledger_url = %config.ledger_url,
        "single-use jti ledger ready",
    );
    Ok(cache)
}

fn start_wallet_sync(
    wallet: &Wallet,
    chain: Arc<dyn ChainSource>,
    config: &ResolvedConfig,
) -> Result<WalletSyncState, StartupError> {
    let sync_options = SyncDriverOptions::default()
        .with_poll_interval_ms(config.wallet_sync_poll_interval_ms)
        .with_max_sync_iterations_per_wake_count(config.wallet_sync_max_iterations_per_wake_count)
        .with_sync_timeout_seconds(config.wallet_sync_timeout_seconds);
    let sync_handle = SyncDriver::new(wallet.clone(), chain, sync_options)
        .map_err(|source| StartupError::SyncDriver { source })?
        .sync_continuously();
    Ok(observe_wallet_sync(sync_handle))
}

fn build_signing_policy(config: &ResolvedConfig) -> Result<SigningPolicy, StartupError> {
    SigningPolicy::builder()
        .network(config.network)
        .max_amount_zat(config.max_amount_zat)
        .audience(config.audience.clone())
        .build()
        .map_err(|source| StartupError::SigningPolicy { source })
}

struct RuntimeParts {
    wallet: Wallet,
    account_id: AccountId,
    policy: SigningPolicy,
    jwks: JwkSet,
    spend_idempotency_cache: UsageLedger,
    revocation: RevocationStore,
    wallet_sync: WalletSyncState,
    metrics: PrometheusHandle,
    seal_posture: SealingPosture,
}

fn build_app_state(config: &ResolvedConfig, parts: RuntimeParts) -> AppState {
    AppState {
        wallet: parts.wallet,
        account_id: parts.account_id,
        policy: Arc::new(parts.policy),
        jwks: Arc::new(parts.jwks),
        public_sign_url: Arc::from(config.public_sign_url.as_str()),
        leeway_seconds: config.leeway_seconds,
        posture: config.posture,
        seal_posture: parts.seal_posture,
        dpop_proof_jti_seen: Arc::new(Mutex::new(HashMap::new())),
        spend_idempotency_cache: parts.spend_idempotency_cache,
        revocation: parts.revocation,
        wallet_sync: parts.wallet_sync,
        wallet_sync_max_lag_blocks: config.wallet_sync_max_lag_blocks,
        wallet_sync_stale_after_seconds: config.wallet_sync_stale_after_seconds,
        metrics: parts.metrics,
    }
}

async fn serve_router(bind_addr: SocketAddr, router: Router) -> Result<(), StartupError> {
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .map_err(|source| StartupError::Bind {
            addr: bind_addr,
            source,
        })?;

    tracing::info!(
        bind = %bind_addr,
        "zspend-runtime ready",
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|source| StartupError::Serve { source })?;
    Ok(())
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_handler))
        .route("/v1/capabilities", get(get_capabilities))
        .route(
            "/.well-known/wallet-configuration",
            get(get_wallet_configuration),
        )
        .route("/v1/payments/sign", post(sign_payment))
        .route("/v1/wallet/address", get(get_wallet_address))
        .with_state(state)
}

fn observe_wallet_sync(sync_handle: SyncHandle) -> WalletSyncState {
    let wallet_sync = WalletSyncState::new(sync_handle.status_snapshot().into());
    let wallet_sync_writer = wallet_sync.clone();
    let mut snapshots = sync_handle.observe_status();
    tokio::spawn(async move {
        let _sync_handle = sync_handle;
        while let Some(snapshot) = snapshots.next().await {
            wallet_sync_writer.publish(snapshot.into());
        }
    });
    wallet_sync
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

/// `GET /metrics`: the Prometheus exposition. No auth: an operator must keep
/// this listener off the public internet or front it with network policy.
async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        state.metrics.render(),
    )
}

fn install_tracing() -> Result<(), StartupError> {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("zspend=info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json())
        .try_init()
        .map_err(|source| StartupError::Tracing { source })
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

async fn healthz() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        r#"{"status":"alive"}"#,
    )
}

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    let (status, body) = readyz_state(&state);
    (
        status,
        [("content-type", "application/json")],
        body.to_string(),
    )
}

/// Compute the readiness body and HTTP status from live [`AppState`].
///
/// Returns 503 when a precondition that would fail every spend is unmet: an
/// empty issuer JWKS rejects every access token, and (when revocation is wired)
/// a stale revocation cache fails every spend closed. The revocation cache is
/// reported `disabled` when no `ZSPEND_ISSUER_URL` is configured, `fresh` when
/// the last successful refresh is within the staleness bound, else `stale`.
fn readyz_state(state: &AppState) -> (StatusCode, serde_json::Value) {
    let jwks_cache = if state.jwks.keys.is_empty() {
        "empty"
    } else {
        "loaded"
    };
    let revocation_cache = state.revocation.readiness();
    // A stale cache fails closed everywhere. A `disabled` cache is acceptable in
    // dev (no issuer wired) but not in production: a mainnet wallet that cannot
    // enforce revocation is not ready to take spends (D-13).
    let revocation_ready = match revocation_cache {
        "stale" => false,
        "disabled" => state.posture != "production",
        _ => true,
    };
    let (wallet_sync, wallet_sync_ready) = wallet_sync_readiness(state);
    let ready = jwks_cache == "loaded" && revocation_ready && wallet_sync_ready;
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = serde_json::json!({
        "network": network_label(state.policy.network()),
        "sealed_seed": seal_posture_str(state.seal_posture),
        "posture": state.posture,
        "jwks_cache": jwks_cache,
        "revocation_cache": revocation_cache,
        "wallet_sync": wallet_sync,
    });
    (status, body)
}

fn wallet_sync_readiness(state: &AppState) -> (serde_json::Value, bool) {
    let snapshot = state.wallet_sync.snapshot();
    let snapshot_age_seconds = unix_now_ms()
        .saturating_sub(snapshot.published_at_ms)
        .saturating_div(1_000);
    let is_fresh = is_wallet_sync_fresh(
        &snapshot,
        state.policy.network(),
        state.wallet_sync_max_lag_blocks,
        state.wallet_sync_stale_after_seconds,
        snapshot_age_seconds,
    );
    record_wallet_sync_metrics(&snapshot, snapshot_age_seconds, is_fresh);

    let body = serde_json::json!({
        "network": network_label(snapshot.network),
        "phase": sync_phase_label(snapshot.phase),
        "sync_status": sync_status_label(snapshot.sync_status),
        "scanned_height": snapshot.scanned_height.map(zally_core::BlockHeight::as_u32),
        "safe_chain_tip_height": snapshot.safe_chain_tip_height.map(zally_core::BlockHeight::as_u32),
        "lag_blocks": snapshot.lag_blocks,
        "snapshot_age_seconds": snapshot_age_seconds,
        "freshness": if is_fresh { "fresh" } else { "stale" },
        "is_fresh": is_fresh,
        "last_fault": snapshot.last_fault.as_ref().map(last_fault_body),
    });
    (body, is_fresh)
}

fn is_wallet_sync_fresh(
    snapshot: &WalletSyncSnapshot,
    expected_network: Network,
    max_lag_blocks: u32,
    stale_after_seconds: u64,
    snapshot_age_seconds: u64,
) -> bool {
    if snapshot.network != expected_network || snapshot_age_seconds > stale_after_seconds {
        return false;
    }
    if !matches!(
        snapshot.phase,
        WalletSyncPhase::Syncing | WalletSyncPhase::Waiting
    ) {
        return false;
    }
    let Some(lag_blocks) = snapshot.lag_blocks else {
        return false;
    };
    snapshot.scanned_height.is_some()
        && snapshot.safe_chain_tip_height.is_some()
        && lag_blocks <= max_lag_blocks
}

fn record_wallet_sync_metrics(
    snapshot: &WalletSyncSnapshot,
    snapshot_age_seconds: u64,
    is_fresh: bool,
) {
    let snapshot_age = std::time::Duration::from_secs(snapshot_age_seconds).as_secs_f64();
    metrics::gauge!("zspend_wallet_sync_snapshot_age_seconds").set(snapshot_age);
    metrics::gauge!("zspend_wallet_sync_fresh").set(if is_fresh { 1.0 } else { 0.0 });
    if let Some(lag_blocks) = snapshot.lag_blocks {
        metrics::gauge!("zspend_wallet_sync_lag_blocks").set(f64::from(lag_blocks));
    }
    if let Some(height) = snapshot.scanned_height {
        metrics::gauge!("zspend_wallet_sync_scanned_height").set(f64::from(height.as_u32()));
    }
    if let Some(height) = snapshot.safe_chain_tip_height {
        metrics::gauge!("zspend_wallet_sync_safe_chain_tip_height").set(f64::from(height.as_u32()));
    }
}

fn last_fault_body(fault: &WalletSyncFault) -> serde_json::Value {
    serde_json::json!({
        "reason": fault.reason.as_str(),
        "repair": fault.repair.as_str(),
        "occurred_at_ms": fault.occurred_at_ms,
        "consecutive_faults": fault.consecutive_faults,
    })
}

fn sync_phase_label(phase: WalletSyncPhase) -> &'static str {
    match phase {
        WalletSyncPhase::Starting => "starting",
        WalletSyncPhase::Syncing => "syncing",
        WalletSyncPhase::Waiting => "waiting",
        WalletSyncPhase::Recovering => "recovering",
        WalletSyncPhase::Parked => "parked",
        WalletSyncPhase::Closing => "closing",
        WalletSyncPhase::Closed => "closed",
        WalletSyncPhase::Unknown => "unknown",
    }
}

fn sync_status_label(status: WalletSyncScanStatus) -> &'static str {
    match status {
        WalletSyncScanStatus::NotStarted => "not_started",
        WalletSyncScanStatus::WaitingForTip => "waiting_for_tip",
        WalletSyncScanStatus::Starting => "starting",
        WalletSyncScanStatus::CatchingUp => "catching_up",
        WalletSyncScanStatus::AtTip => "at_tip",
        WalletSyncScanStatus::TipRegressed => "tip_regressed",
        WalletSyncScanStatus::Unknown => "unknown",
    }
}

fn network_label(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "mainnet",
        Network::Regtest(_) => "regtest",
        Network::Testnet | _ => "testnet",
    }
}

/// Read the at-rest seal posture of the sealing implementation the runtime
/// uses (age-file sealing), via [`zally_keys::SeedSealing::posture`].
fn age_seal_posture(sealed_seed_path: &std::path::Path) -> SealingPosture {
    AgeFileSealing::new(AgeFileSealingOptions::at_path(
        sealed_seed_path.to_path_buf(),
    ))
    .posture()
}

/// Map a [`SealingPosture`] to its wire string for `/readyz`.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "SealingPosture is non_exhaustive; an unrecognised future posture reports `unknown` rather than failing the probe"
)]
fn seal_posture_str(posture: SealingPosture) -> &'static str {
    match posture {
        SealingPosture::Dev => "dev",
        SealingPosture::Hsm => "hsm",
        SealingPosture::Kms => "kms",
        _ => "unknown",
    }
}

/// Returns the RAR projection of the active access token (D-12).
///
/// The grant travels per-request in the DPoP-bound `at+jwt`, not in
/// server-side session state, so this discovery endpoint has no standing grant
/// to project and returns an empty array. Consumers read the active
/// authorization from the `authorization_details` of the token they present.
async fn get_capabilities() -> impl IntoResponse {
    Json(serde_json::json!({ "capabilities": [] }))
}

/// Returns the funded Unified Address for the bootstrapped account.
///
/// Lets an operator credit the wallet from any local testnet wallet without
/// inspecting the storage backend. Idempotent across calls: the first call may
/// derive a fresh UA, subsequent calls surface the same address.
async fn get_wallet_address(
    State(state): State<AppState>,
) -> Result<Json<WalletAddressResponse>, ProblemResponse> {
    let ua = state.current_unified_address().await.map_err(|err| {
        tracing::warn!(error = %err, "wallet address lookup failed");
        ProblemResponse::server_error(ProblemDetail::not_retryable(
            ProblemKind::NotReady,
            "wallet not ready",
            err.to_string(),
        ))
    })?;
    let network = WireNetworkOut::from(WireNetwork::from_zally(state.policy.network()));
    Ok(Json(WalletAddressResponse { ua, network }))
}

/// `.well-known` discovery per Proposal-0003 D-12.
async fn get_wallet_configuration(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "supported_formats": [SIGNED_PAYLOAD_FORMAT_WIRE_LITERAL],
        "supported_schemes": ["zip321"],
        "intent_hash_algorithm": "v1:sha256",
        "audience": state.policy.audience(),
    }))
}

/// Inputs to `POST /v1/payments/sign`.
///
/// `target_expiry_height` is required: the caller passes the height it pinned at
/// `/prepare` time so the wallet builds the PCZT with that value on
/// `Global::expiry_height`. The IO Finalizer step inside zally's storage signs
/// every dummy orchard action against the shielded sighash derived from that
/// global, so the value cannot be mutated later without producing
/// `SighashMismatch` at extraction time.
#[derive(Debug, Deserialize)]
struct SignPaymentRequest {
    payment_request: WirePaymentRequest,
    network: WireNetwork,
    payment_id: String,
    target_expiry_height: u32,
}

/// Scheme-tagged payment request body per D-11.
#[derive(Debug, Deserialize)]
struct WirePaymentRequest {
    scheme: String,
    value: String,
}

/// Network discriminator on the inbound wire.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum WireNetwork {
    Mainnet,
    Testnet,
    Regtest,
}

impl WireNetwork {
    fn to_zally(self) -> Network {
        match self {
            Self::Mainnet => Network::Mainnet,
            Self::Testnet => Network::Testnet,
            Self::Regtest => Network::regtest(),
        }
    }

    fn from_zally(network: Network) -> Self {
        match network {
            Network::Mainnet => Self::Mainnet,
            Network::Regtest(_) => Self::Regtest,
            Network::Testnet | _ => Self::Testnet,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum WireNetworkOut {
    Mainnet,
    Testnet,
    Regtest,
}

impl From<WireNetwork> for WireNetworkOut {
    fn from(net: WireNetwork) -> Self {
        match net {
            WireNetwork::Mainnet => Self::Mainnet,
            WireNetwork::Testnet => Self::Testnet,
            WireNetwork::Regtest => Self::Regtest,
        }
    }
}

#[derive(Debug, Serialize)]
struct WalletAddressResponse {
    ua: String,
    network: WireNetworkOut,
}

/// `signed_payload` envelope returned by `/v1/payments/sign`.
///
/// Field shape mirrors `zally_core::SignedPayload`, while `format` names the
/// Zcash exact x402 binding accepted by zpay.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SignedPayloadWire {
    format: String,
    bytes: String,
    tx_id: String,
    fee: AmountWire,
    expires_at: ExpiresAtWire,
    #[serde(skip_serializing_if = "serde_json::Value::is_null", default)]
    metadata: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AmountWire {
    currency: String,
    value: String,
    unit: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum ExpiresAtWire {
    BlockHeight(u32),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SignPaymentResponse {
    signed_payload: SignedPayloadWire,
}

/// Renders the wallet's [`SendOutcome`] and signed PCZT bytes into the
/// `/v1/payments/sign` wire envelope.
///
/// `tx_id` carries the wallet-computed ZIP-244 identifier in canonical RPC byte
/// order, the form every block explorer and downstream lookup expects.
fn signed_payload_response(outcome: &SendOutcome, signed_pczt: &PcztBytes) -> SignPaymentResponse {
    SignPaymentResponse {
        signed_payload: SignedPayloadWire {
            format: SIGNED_PAYLOAD_FORMAT_WIRE_LITERAL.to_owned(),
            bytes: BASE64_URL_SAFE_NO_PAD.encode(signed_pczt.as_bytes()),
            tx_id: outcome.signed.tx_id.to_rpc_hex(),
            fee: AmountWire {
                currency: "ZEC".to_owned(),
                value: outcome.signed.fee_zat.as_u64().to_string(),
                unit: "base".to_owned(),
            },
            expires_at: ExpiresAtWire::BlockHeight(outcome.signed.tx_expiry_height.as_u32()),
            metadata: serde_json::Value::Null,
        },
    }
}

/// Handler for `POST /v1/payments/sign`.
///
/// Runs the landed trust boundary, then signs: verify the DPoP-bound `at+jwt`
/// and recompute the `intent_hash` from the parsed request, reserve the
/// access-token `jti` in the single-use ledger BEFORE signing, parse the
/// ZIP-321 URI, build a [`ProposalPlan`], run the PCZT prove and sign roles,
/// extract the transaction through [`CaptureSubmitter`] so the wallet records
/// the spend, then commit the signed PCZT to the ledger so an identical replay
/// returns the cached envelope.
async fn sign_payment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SignPaymentRequest>,
) -> Result<Json<SignPaymentResponse>, ProblemResponse> {
    let signed = sign_payment_inner(&state, &headers, body).await;
    let outcome = match &signed {
        Ok(_) => "signed",
        Err(problem) => problem_kind_metric_label(problem.body.kind),
    };
    metrics::counter!("zspend_sign_requests_total", "outcome" => outcome).increment(1);
    signed
}

/// Bounded `outcome` label for the sign-request counter: `signed` on success,
/// else the refusal's [`ProblemKind`] rendered as its wire slug.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "ProblemKind is #[non_exhaustive]; a future kind reports `other` until it has an explicit label"
)]
fn problem_kind_metric_label(kind: ProblemKind) -> &'static str {
    match kind {
        ProblemKind::PaymentRequestInvalid => "payment_request_invalid",
        ProblemKind::IntentMismatch => "intent_mismatch",
        ProblemKind::AudienceMismatch => "audience_mismatch",
        ProblemKind::TokenAlreadyConsumed => "token_already_consumed",
        ProblemKind::InsufficientFunds => "insufficient_funds",
        ProblemKind::SeedUnavailable => "seed_unavailable",
        ProblemKind::ChainUnreachable => "chain_unreachable",
        ProblemKind::NotReady => "not_ready",
        ProblemKind::WalletUnavailable => "wallet_unavailable",
        ProblemKind::DpopProofInvalid => "dpop_proof_invalid",
        ProblemKind::AccessTokenInvalid => "access_token_invalid",
        ProblemKind::TargetExpiryStale => "target_expiry_stale",
        ProblemKind::TargetExpiryMismatchInternal => "target_expiry_mismatch_internal",
        ProblemKind::TokenRevoked => "token_revoked",
        ProblemKind::RecipientMismatch => "recipient_mismatch",
        ProblemKind::AmountExceeded => "amount_exceeded",
        ProblemKind::AuthorizationExpired => "authorization_expired",
        ProblemKind::RarTooManyEntries => "rar_too_many_entries",
        ProblemKind::RevocationCacheStale => "revocation_cache_stale",
        _ => "other",
    }
}

/// Bounded `result` label for the revocation-check counter.
fn revocation_result_label(outcome: RevocationOutcome) -> &'static str {
    match outcome {
        RevocationOutcome::Live => "live",
        RevocationOutcome::Revoked => "revoked",
        RevocationOutcome::CacheStale => "cache_stale",
    }
}

/// Bounded `outcome` label for the single-use reserve counter.
fn reservation_label(reservation: &Result<Reservation, ledger::LedgerError>) -> &'static str {
    match reservation {
        Ok(Reservation::Fresh) => "fresh",
        Ok(Reservation::Completed(_)) => "completed",
        Ok(Reservation::IntentConflict) => "intent_conflict",
        Ok(Reservation::Pending) => "pending",
        Err(_) => "error",
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "single linear request handler: verify -> recompute -> reserve -> sign -> capture -> commit; splitting would scatter the flow across helpers"
)]
async fn sign_payment_inner(
    state: &AppState,
    headers: &HeaderMap,
    body: SignPaymentRequest,
) -> Result<Json<SignPaymentResponse>, ProblemResponse> {
    // Trust boundary (Proposal-0003 Slice 1): verify the DPoP-bound access
    // token, then re-derive every bound field from the signed RAR before any
    // signing work. A request that does not carry a conformant grant is
    // rejected here, not signed.
    let claims = verify_spend_authorization(state, headers)?;
    let auth = claims.payment_authorization().map_err(problem_response)?;
    cross_check_request(auth, &body)?;

    if body.payment_request.scheme != "zip321" {
        return Err(ProblemResponse::bad_request(ProblemDetail::not_retryable(
            ProblemKind::PaymentRequestInvalid,
            "payment_request scheme not supported",
            format!(
                "scheme={:?}; only zip321 is supported in Phase 4",
                body.payment_request.scheme
            ),
        )));
    }
    let requested_network = body.network.to_zally();
    if requested_network != state.policy.network() {
        return Err(ProblemResponse::bad_request(ProblemDetail::not_retryable(
            ProblemKind::PaymentRequestInvalid,
            "network mismatch",
            format!(
                "request network={requested_network:?} does not match policy network={:?}",
                state.policy.network(),
            ),
        )));
    }

    let parsed = PaymentRequest::from_uri(&body.payment_request.value, requested_network).map_err(
        |err| {
            ProblemResponse::bad_request(ProblemDetail::not_retryable(
                ProblemKind::PaymentRequestInvalid,
                "ZIP-321 parse failed",
                err.to_string(),
            ))
        },
    )?;

    let payment = parsed.payments().first().ok_or_else(|| {
        ProblemResponse::bad_request(ProblemDetail::not_retryable(
            ProblemKind::PaymentRequestInvalid,
            "payment_request carried no payments",
            "ZIP-321 URI parsed but the payment list was empty",
        ))
    })?;

    // D-4 binding: re-derive the intent_hash from the parsed recipient and
    // amount, combined with the signed chain/payment_id/expiry, and reject a
    // request whose tuple does not reproduce the RAR's intent_hash.
    let recipient_caip10 = format!(
        "zcash:{}:{}",
        auth.chain.reference,
        payment.recipient.encoded()
    );
    let intent_ok =
        intent_matches(auth, &recipient_caip10, payment.amount.as_u64()).map_err(|err| {
            problem_response(ProblemDetail::not_retryable(
                ProblemKind::IntentMismatch,
                "intent_mismatch",
                err.to_string(),
            ))
        })?;
    if !intent_ok {
        // The full tuple diverged. Isolate whether the recipient ALONE is the
        // diverging field: recompute with the RAR's own signed recipient but
        // the request's parsed amount. If that reproduces the signed
        // intent_hash (while the full recompute did not), amount/expiry/
        // payment_id all matched and only the payee changed; surface
        // RecipientMismatch so telemetry separates a payee swap from generic
        // drift.
        return Err(classify_intent_divergence(auth, payment.amount.as_u64()));
    }

    // Local backstop cap (defense in depth; the issuer is the authoritative
    // policy gate per D-2).
    if payment.amount.as_u64() > state.policy.max_amount_zat() {
        return Err(problem_response(ProblemDetail::not_retryable(
            ProblemKind::AmountExceeded,
            "amount exceeds wallet backstop cap",
            format!(
                "requested {} > max {}",
                payment.amount.as_u64(),
                state.policy.max_amount_zat(),
            ),
        )));
    }

    let recipient_for_plan = clone_recipient(&payment.recipient);
    let amount_for_plan = payment.amount;

    // Revocation (D-6): consult the revocation cache on the access-token jti
    // BEFORE the single-use reserve, so a grant the issuer pulled after mint is
    // rejected ahead of any cached payload an identical replay would otherwise
    // return (PRD-43 D-D ordering). A disabled store (no ZSPEND_ISSUER_URL)
    // always admits; a stale-and-unrefreshable cache fails closed (retryable).
    let revocation_outcome = state.revocation.check(&claims.jti).await;
    metrics::counter!(
        "zspend_revocation_checks_total",
        "result" => revocation_result_label(revocation_outcome),
    )
    .increment(1);
    match revocation_outcome {
        RevocationOutcome::Live => {}
        RevocationOutcome::Revoked => {
            return Err(problem_response(ProblemDetail::not_retryable(
                ProblemKind::TokenRevoked,
                "token_revoked",
                "this access-token jti was revoked at the issuer after mint",
            )));
        }
        RevocationOutcome::CacheStale => {
            return Err(problem_response(ProblemDetail::retryable(
                ProblemKind::RevocationCacheStale,
                "revocation_cache_stale",
                "the revocation cache is older than the staleness bound and the issuer is unreachable; retry shortly",
            )));
        }
    }

    require_wallet_sync_fresh(state)?;

    // Single-use jti (D-8): reserve the access-token jti against the verified
    // intent BEFORE signing, so a crash between reserve and commit cannot let a
    // retry sign blind. An identical replay returns the cached payload; a reuse
    // against a different intent is refused; an in-flight reservation is
    // retryable. The reservation is durable (libSQL) so the guarantee holds
    // across a restart, and across replicas when they share one ledger URL.
    let reservation = state
        .spend_idempotency_cache
        .reserve(&claims.jti, &auth.intent_hash.0)
        .await;
    metrics::counter!(
        "zspend_ledger_claims_total",
        "outcome" => reservation_label(&reservation),
    )
    .increment(1);
    match reservation {
        Ok(Reservation::Fresh) => {}
        Ok(Reservation::Completed(cached)) => return Ok(Json(cached)),
        Ok(Reservation::IntentConflict) => {
            return Err(problem_response(ProblemDetail::not_retryable(
                ProblemKind::TokenAlreadyConsumed,
                "token_already_consumed",
                "this access-token jti was already used to sign a different intent",
            )));
        }
        Ok(Reservation::Pending) => {
            return Err(problem_response(ProblemDetail::retryable(
                ProblemKind::NotReady,
                "not_ready",
                "this access-token jti is being signed by an in-flight request; retry shortly",
            )));
        }
        Err(err) => return Err(ledger_unavailable(&err)),
    }

    let target_expiry_height = BlockHeight::from(body.target_expiry_height);
    let plan = ProposalPlan::conventional(
        state.account_id,
        recipient_for_plan,
        amount_for_plan,
        payment.memo.clone(),
    );

    let unsigned_pczt = match state
        .wallet
        .propose_pczt(plan, Some(target_expiry_height))
        .await
    {
        Ok(pczt) => pczt,
        Err(err) => {
            release_reservation(state, &claims.jti).await;
            return Err(map_wallet_err(&err));
        }
    };

    let proven_pczt = match state.wallet.prove_pczt(unsigned_pczt).await {
        Ok(pczt) => pczt,
        Err(err) => {
            release_reservation(state, &claims.jti).await;
            return Err(map_wallet_err(&err));
        }
    };

    let signed_pczt = match state.wallet.sign_pczt(proven_pczt).await {
        Ok(pczt) => pczt,
        Err(err) => {
            release_reservation(state, &claims.jti).await;
            return Err(map_wallet_err(&err));
        }
    };

    let capture = CaptureSubmitter::new(requested_network);
    let send_outcome = match state
        .wallet
        .extract_and_submit_pczt(signed_pczt.clone(), &capture)
        .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            if matches!(err, WalletError::ChainSource(_)) {
                metrics::counter!("zspend_wallet_chain_source_errors_total").increment(1);
            }
            release_reservation(state, &claims.jti).await;
            return Err(map_wallet_err(&err));
        }
    };

    let Some(captured) = capture.take_captured() else {
        release_reservation(state, &claims.jti).await;
        return Err(ProblemResponse::server_error(ProblemDetail::not_retryable(
            ProblemKind::NotReady,
            "submitter captured no bytes",
            "internal: CaptureSubmitter::submit was not invoked by PCZT extraction",
        )));
    };
    drop(captured);

    let response = signed_payload_response(&send_outcome, &signed_pczt);
    if let Err(err) = state
        .spend_idempotency_cache
        .commit(&claims.jti, &auth.intent_hash.0, &response)
        .await
    {
        // The signature succeeded; a failed commit only loses this runtime's
        // replay memory. The pending ledger reservation still blocks an
        // immediate second signing attempt for the same jti.
        tracing::error!(error = %err, "single-use ledger commit failed after signing");
    }
    Ok(Json(response))
}

fn require_wallet_sync_fresh(state: &AppState) -> Result<(), ProblemResponse> {
    let snapshot = state.wallet_sync.snapshot();
    let snapshot_age_seconds = unix_now_ms()
        .saturating_sub(snapshot.published_at_ms)
        .saturating_div(1_000);
    let is_fresh = is_wallet_sync_fresh(
        &snapshot,
        state.policy.network(),
        state.wallet_sync_max_lag_blocks,
        state.wallet_sync_stale_after_seconds,
        snapshot_age_seconds,
    );
    record_wallet_sync_metrics(&snapshot, snapshot_age_seconds, is_fresh);
    if is_fresh {
        return Ok(());
    }
    Err(problem_response(ProblemDetail::retryable(
        ProblemKind::WalletUnavailable,
        "wallet_unavailable",
        format!(
            "wallet sync phase={} lag_blocks={:?} snapshot_age_seconds={snapshot_age_seconds}; retry after sync freshness recovers",
            sync_phase_label(snapshot.phase),
            snapshot.lag_blocks,
        ),
    )))
}

/// Classify a failed full-tuple intent recompute as a recipient swap or a
/// generic intent mismatch (D-4/D-2).
///
/// Recomputes the expected `intent_hash` with the RAR's own signed recipient
/// but the request's parsed `amount`. When that reproduces the signed hash the
/// `amount`/`expiry`/`payment_id` all matched, so the recipient is the only
/// diverging field and the wallet surfaces [`ProblemKind::RecipientMismatch`];
/// otherwise some other field drifted and it surfaces
/// [`ProblemKind::IntentMismatch`].
fn classify_intent_divergence(auth: &PaymentAuthorization, parsed_amount: u64) -> ProblemResponse {
    let recipient_alone_diverged = recompute_intent_hash(auth, &auth.recipient, parsed_amount)
        .is_ok_and(|recomputed| recomputed == auth.intent_hash.0);
    if recipient_alone_diverged {
        problem_response(ProblemDetail::not_retryable(
            ProblemKind::RecipientMismatch,
            "recipient_mismatch",
            "parsed recipient does not match the recipient the issuer signed into the authorization",
        ))
    } else {
        problem_response(ProblemDetail::not_retryable(
            ProblemKind::IntentMismatch,
            "intent_mismatch",
            "recomputed intent_hash does not match the signed authorization",
        ))
    }
}

fn clone_recipient(recipient: &PaymentRecipient) -> PaymentRecipient {
    recipient.clone()
}

/// Release a still-pending single-use reservation after a signing failure,
/// logging (rather than propagating) a ledger error since the reservation
/// clears on its own after the pending TTL.
async fn release_reservation(state: &AppState, jti: &str) {
    if let Err(err) = state.spend_idempotency_cache.release(jti).await {
        tracing::warn!(error = %err, "failed to release single-use reservation after signing error");
    }
}

/// Map a single-use ledger backend failure to a retryable 503 so the caller
/// retries once the store recovers rather than treating a transient outage as
/// a signing rejection.
fn ledger_unavailable(err: &ledger::LedgerError) -> ProblemResponse {
    problem_response(ProblemDetail::retryable(
        ProblemKind::NotReady,
        "not_ready",
        format!("single-use ledger is unavailable: {err}"),
    ))
}

/// Verify the DPoP-bound `at+jwt` on `POST /v1/payments/sign` (Slice 1).
///
/// Runs the boundary in order: extract `Authorization: DPoP <at+jwt>` plus the
/// `DPoP` proof, verify the access token against the issuer JWKS with the
/// audience pinned to this wallet's identity URI, verify the DPoP proof binds
/// to the token's `cnf.jkt` and this request, then reject a replayed proof
/// `jti`.
fn verify_spend_authorization(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AccessTokenClaims, ProblemResponse> {
    let (access_token, proof) = extract_dpop_bearer(headers)?;
    let claims = verify_access_token(
        &access_token,
        &state.jwks,
        state.policy.audience(),
        state.leeway_seconds,
    )
    .map_err(problem_response)?;

    let binding = DpopBinding {
        method: "POST",
        request_url: &state.public_sign_url,
        access_token: &access_token,
        bound_jkt: &claims.cnf.jkt,
    };
    let verified = verify_dpop_proof(&proof, &binding, unix_now(), DPOP_CLOCK_SKEW_SECONDS)
        .map_err(problem_response)?;
    record_dpop_jti(state, verified.jti)?;
    Ok(claims)
}

/// Record a verified DPoP proof `jti` in the short-window anti-replay set,
/// rejecting a `jti` already seen in the window. Scoped to its own function so
/// the lock guard drops before the caller returns.
fn record_dpop_jti(state: &AppState, jti: String) -> Result<(), ProblemResponse> {
    let now = Instant::now();
    let mut seen = state.dpop_proof_jti_seen.lock();
    seen.retain(|_, recorded| {
        now.duration_since(*recorded) < std::time::Duration::from_secs(DPOP_REPLAY_WINDOW_SECONDS)
    });
    if seen.contains_key(&jti) {
        return Err(problem_response(ProblemDetail::not_retryable(
            ProblemKind::DpopProofInvalid,
            "dpop_proof_invalid",
            "DPoP proof jti was already presented in the replay window",
        )));
    }
    seen.insert(jti, now);
    drop(seen);
    Ok(())
}

/// Pull the `at+jwt` and the DPoP proof off the request headers.
fn extract_dpop_bearer(headers: &HeaderMap) -> Result<(String, String), ProblemResponse> {
    let access_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("DPoP "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            problem_response(ProblemDetail::not_retryable(
                ProblemKind::AccessTokenInvalid,
                "access_token_invalid",
                "missing or malformed Authorization: DPoP <at+jwt> header",
            ))
        })?
        .to_owned();
    let proof = headers
        .get("dpop")
        .and_then(|header| header.to_str().ok())
        .map(str::trim)
        .filter(|proof| !proof.is_empty())
        .ok_or_else(|| {
            problem_response(ProblemDetail::not_retryable(
                ProblemKind::DpopProofInvalid,
                "dpop_proof_invalid",
                "missing DPoP proof header",
            ))
        })?
        .to_owned();
    Ok((access_token, proof))
}

/// Cross-check the plaintext request body against the signed RAR so the body's
/// `network`/`payment_id`/`target_expiry_height` cannot diverge from the grant.
fn cross_check_request(
    auth: &PaymentAuthorization,
    body: &SignPaymentRequest,
) -> Result<(), ProblemResponse> {
    if body.payment_id != auth.payment_id {
        return Err(reject_intent(
            "payment_id does not match the signed authorization",
        ));
    }
    let expiry = auth_expiry_height(&auth.expires_at).map_err(problem_response)?;
    if body.target_expiry_height != expiry {
        return Err(reject_intent(
            "target_expiry_height does not match the signed authorization expires_at",
        ));
    }
    let body_reference = match body.network {
        WireNetwork::Mainnet => "main",
        WireNetwork::Testnet => "test",
        WireNetwork::Regtest => "regtest",
    };
    if auth.chain.namespace != "zcash" || auth.chain.reference != body_reference {
        return Err(reject_intent(
            "network does not match the signed authorization chain",
        ));
    }
    Ok(())
}

fn reject_intent(detail: &str) -> ProblemResponse {
    problem_response(ProblemDetail::not_retryable(
        ProblemKind::IntentMismatch,
        "intent_mismatch",
        detail.to_owned(),
    ))
}

/// Project the chain-tagged RAR expiry onto the Zcash block height the wallet
/// commits to. Non-block-height kinds are rejected fail-closed.
fn auth_expiry_height(expires_at: &zspend_core::ExpiresAt) -> Result<u32, ProblemDetail> {
    match *expires_at {
        zspend_core::ExpiresAt::BlockHeight(height) => Ok(height),
        zspend_core::ExpiresAt::Slot(_)
        | zspend_core::ExpiresAt::BlockNumber(_)
        | zspend_core::ExpiresAt::TimestampSeconds(_) => Err(ProblemDetail::not_retryable(
            ProblemKind::PaymentRequestInvalid,
            "unsupported expiry kind",
            "zcash authorizations must carry a block_height expires_at",
        )),
        _ => Err(ProblemDetail::not_retryable(
            ProblemKind::PaymentRequestInvalid,
            "unsupported expiry kind",
            "unknown expires_at kind",
        )),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Wrap a [`ProblemDetail`] in an HTTP response with the status the §4 error
/// table assigns to its `kind`.
fn problem_response(body: ProblemDetail) -> ProblemResponse {
    ProblemResponse {
        status: status_for_kind(body.kind),
        body,
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "the explicit TargetExpiryMismatchInternal arm documents its 500 mapping; the `_` arm exists only to cover the non_exhaustive hidden variant and shares the same status"
)]
fn status_for_kind(kind: ProblemKind) -> StatusCode {
    match kind {
        ProblemKind::PaymentRequestInvalid => StatusCode::BAD_REQUEST,
        ProblemKind::DpopProofInvalid
        | ProblemKind::AccessTokenInvalid
        | ProblemKind::TokenRevoked => StatusCode::UNAUTHORIZED,
        ProblemKind::IntentMismatch
        | ProblemKind::RecipientMismatch
        | ProblemKind::AmountExceeded
        | ProblemKind::AudienceMismatch => StatusCode::FORBIDDEN,
        ProblemKind::TokenAlreadyConsumed | ProblemKind::TargetExpiryStale => StatusCode::CONFLICT,
        ProblemKind::AuthorizationExpired => StatusCode::GONE,
        ProblemKind::InsufficientFunds | ProblemKind::RarTooManyEntries => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        ProblemKind::SeedUnavailable
        | ProblemKind::ChainUnreachable
        | ProblemKind::WalletUnavailable
        | ProblemKind::RevocationCacheStale
        | ProblemKind::NotReady => StatusCode::SERVICE_UNAVAILABLE,
        ProblemKind::TargetExpiryMismatchInternal => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Build the revocation store from config (D-6).
///
/// When `ZSPEND_ISSUER_URL` is set the store enforces revocation against the
/// issuer delta endpoint. When unset it is disabled (always admits) and a
/// startup warning records that a grant revoked after mint will still sign
/// until the issuer is wired.
fn build_revocation_store(config: &ResolvedConfig) -> RevocationStore {
    let Some(issuer_url) = config.issuer_url.as_deref() else {
        tracing::warn!(
            "no ZSPEND_ISSUER_URL configured: token revocation is NOT enforced; a grant revoked after mint will still sign until the issuer is wired",
        );
        return RevocationStore::disabled();
    };
    tracing::info!(
        issuer_url,
        max_staleness_secs = config.revocation_max_staleness.as_secs(),
        "revocation enforcement enabled: /v1/payments/sign consults the issuer delta endpoint before signing",
    );
    RevocationStore::new(
        config.issuer_url.clone(),
        config.revocation_token.clone(),
        config.revocation_max_staleness,
    )
}

/// Load the issuer JWKS from `ZSPEND_JWKS_FILE`. An absent path yields an empty
/// key set, which fails every spend closed until the operator wires the issuer.
fn load_jwks(path: Option<&PathBuf>) -> Result<JwkSet, StartupError> {
    let Some(path) = path else {
        return Ok(JwkSet { keys: Vec::new() });
    };
    let raw = std::fs::read_to_string(path).map_err(|err| StartupError::Jwks {
        path: path.clone(),
        reason: err.to_string(),
    })?;
    serde_json::from_str(&raw).map_err(|err| StartupError::Jwks {
        path: path.clone(),
        reason: err.to_string(),
    })
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "the three caller-actionable wallet errors are matched explicitly; the remaining internal variants all map to a generic 500, and enumerating them would only trip match_same_arms"
)]
fn map_wallet_err(err: &zally_wallet::WalletError) -> ProblemResponse {
    match err {
        zally_wallet::WalletError::TargetExpiryStale { target, chain_tip } => ProblemResponse {
            status: StatusCode::CONFLICT,
            body: ProblemDetail::not_retryable(
                ProblemKind::TargetExpiryStale,
                "target_expiry_height is past the chain tip",
                format!(
                    "caller-supplied target_expiry_height={} is at or below the wallet's \
                     observed tip={}; request a fresh /prepare and retry",
                    u32::from(*target),
                    u32::from(*chain_tip),
                ),
            ),
        },
        zally_wallet::WalletError::TargetExpiryMismatch { target, signed } => {
            ProblemResponse::server_error(ProblemDetail::not_retryable(
                ProblemKind::TargetExpiryMismatchInternal,
                "signed expiry_height did not match target_expiry_height",
                format!(
                    "wallet signed bytes with expiry_height={} but caller asked to commit to {}; \
                     indicates a bug in the wallet's PCZT proposal path",
                    u32::from(*signed),
                    u32::from(*target),
                ),
            ))
        }
        zally_wallet::WalletError::InsufficientBalance {
            requested_zat,
            spendable_zat,
        } => ProblemResponse::server_error(ProblemDetail::not_retryable(
            ProblemKind::InsufficientFunds,
            "insufficient spendable balance",
            format!(
                "requested {} zat, spendable {} zat",
                requested_zat.as_u64(),
                spendable_zat.as_u64(),
            ),
        )),
        other => ProblemResponse::server_error(ProblemDetail::not_retryable(
            ProblemKind::NotReady,
            "wallet signing failed",
            other.to_string(),
        )),
    }
}

/// PRC-7807 problem-detail response wrapper.
struct ProblemResponse {
    status: StatusCode,
    body: ProblemDetail,
}

impl ProblemResponse {
    fn bad_request(body: ProblemDetail) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            body,
        }
    }

    fn server_error(body: ProblemDetail) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body,
        }
    }
}

impl IntoResponse for ProblemResponse {
    fn into_response(self) -> axum::response::Response {
        let json = serde_json::to_string(&self.body)
            .unwrap_or_else(|_| r#"{"kind":"not_ready","title":"encode error","detail":"failed to serialize problem detail","retryable":false}"#.to_owned());
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/problem+json"),
        );
        if let Some(seconds) = retry_after_seconds(self.body.kind)
            && let Ok(retry_after) = axum::http::HeaderValue::from_str(&seconds.to_string())
        {
            headers.insert(axum::http::header::RETRY_AFTER, retry_after);
        }
        (self.status, headers, json).into_response()
    }
}

/// Numeric `Retry-After` (seconds) for the retryable problem kinds.
///
/// Per the §4 error table (error.rs): a `RevocationCacheStale` clears once the
/// cache refreshes, so it hints the default staleness bound; a `NotReady` from
/// an in-flight single-use reservation clears after the pending TTL. Every
/// other kind is not retryable and carries no header.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "only the two retryable kinds carry a Retry-After; every other current and future kind correctly yields None"
)]
fn retry_after_seconds(kind: ProblemKind) -> Option<u64> {
    match kind {
        ProblemKind::RevocationCacheStale => Some(DEFAULT_REVOCATION_MAX_STALENESS_SECONDS),
        ProblemKind::NotReady => Some(ledger::PENDING_RESERVATION_TTL.as_secs()),
        ProblemKind::WalletUnavailable => Some(DEFAULT_WALLET_SYNC_RETRY_SECONDS),
        _ => None,
    }
}

#[derive(Debug)]
struct ResolvedConfig {
    bind_addr: SocketAddr,
    network: Network,
    sealed_seed_path: PathBuf,
    storage_path: PathBuf,
    max_amount_zat: u64,
    audience: String,
    jwks_file: Option<PathBuf>,
    public_sign_url: String,
    leeway_seconds: u64,
    posture: &'static str,
    /// Single-use `jti` ledger backend (`ZSPEND_LEDGER_URL`): a `libsql://` URL
    /// or a filesystem path. Defaults to a `usage-ledger.db` file next to
    /// `ZSPEND_STORAGE_PATH` so single-use enforcement is durable out of the box.
    ledger_url: String,
    /// Auth token for a remote (`libsql://`) ledger backend
    /// (`ZSPEND_LEDGER_AUTH_TOKEN`). Ignored for a file-backed ledger.
    /// Required for a `libsql://` URL; startup fails closed without it
    /// rather than attempting an unauthenticated connection.
    ledger_auth_token: Option<String>,
    /// Whether a dev-posture seal may serve (`ZSPEND_ALLOW_DEV_SEED=1`, D-13).
    allow_dev_seed: bool,
    /// Issuer base URL for the revocation delta endpoint (`ZSPEND_ISSUER_URL`).
    /// Unset disables revocation enforcement (local dev / first happy-path E2E).
    issuer_url: Option<String>,
    /// Bearer presented to the issuer revocation endpoint
    /// (`ZSPEND_REVOCATION_TOKEN`). Unset sends no header (dev issuer with no
    /// `INTERNAL_SERVICE_TOKEN`).
    revocation_token: Option<String>,
    /// Maximum age a successful revocation refresh may reach before `check`
    /// fails closed (`ZSPEND_REVOCATION_MAX_STALENESS_SECONDS`).
    revocation_max_staleness: std::time::Duration,
    indexer_grpc_addr: Option<String>,
    birthday_override: Option<u32>,
    /// Polling cadence for the long-lived wallet sync driver.
    wallet_sync_poll_interval_ms: u64,
    /// Maximum sync iterations per driver wakeup.
    wallet_sync_max_iterations_per_wake_count: u32,
    /// Timeout for one wallet sync iteration.
    wallet_sync_timeout_seconds: u64,
    /// Maximum wallet lag accepted by readiness and signing.
    wallet_sync_max_lag_blocks: u32,
    /// Maximum accepted age for the latest wallet sync snapshot.
    wallet_sync_stale_after_seconds: u64,
}

struct ListenerConfig {
    bind_addr: SocketAddr,
    network: Network,
}

struct WalletPaths {
    sealed_seed_path: PathBuf,
    storage_path: PathBuf,
}

struct SigningConfig {
    max_amount_zat: u64,
    audience: String,
    jwks_file: Option<PathBuf>,
    public_sign_url: String,
    leeway_seconds: u64,
}

struct WalletSyncConfig {
    poll_interval_ms: u64,
    max_iterations_per_wake_count: u32,
    timeout_seconds: u64,
    max_lag_blocks: u32,
    stale_after_seconds: u64,
}

impl ResolvedConfig {
    fn from_env() -> Result<Self, StartupError> {
        let listener = read_listener_config()?;
        let wallet_paths = read_wallet_paths()?;
        let signing = read_signing_config(listener.bind_addr)?;
        let wallet_sync = read_wallet_sync_config();
        let posture = posture_for_network(listener.network);
        let ledger_url = optional_env("ZSPEND_LEDGER_URL")
            .unwrap_or_else(|| default_ledger_url(&wallet_paths.storage_path));
        let ledger_auth_token = optional_env("ZSPEND_LEDGER_AUTH_TOKEN");
        let allow_dev_seed = env_flag("ZSPEND_ALLOW_DEV_SEED");
        let issuer_url = optional_env("ZSPEND_ISSUER_URL");
        let revocation_token = optional_env("ZSPEND_REVOCATION_TOKEN");
        let revocation_max_staleness = std::time::Duration::from_secs(
            std::env::var("ZSPEND_REVOCATION_MAX_STALENESS_SECONDS")
                .ok()
                .and_then(|raw| raw.trim().parse::<u64>().ok())
                .unwrap_or(DEFAULT_REVOCATION_MAX_STALENESS_SECONDS),
        );

        let indexer_grpc_addr = optional_env("ZSPEND_CHAIN_SOURCE_URL");
        let birthday_override = std::env::var("ZSPEND_BIRTHDAY_HEIGHT")
            .ok()
            .and_then(|raw| raw.trim().parse::<u32>().ok());

        Ok(Self {
            bind_addr: listener.bind_addr,
            network: listener.network,
            sealed_seed_path: wallet_paths.sealed_seed_path,
            storage_path: wallet_paths.storage_path,
            max_amount_zat: signing.max_amount_zat,
            audience: signing.audience,
            jwks_file: signing.jwks_file,
            public_sign_url: signing.public_sign_url,
            leeway_seconds: signing.leeway_seconds,
            posture,
            ledger_url,
            ledger_auth_token,
            allow_dev_seed,
            issuer_url,
            revocation_token,
            revocation_max_staleness,
            indexer_grpc_addr,
            birthday_override,
            wallet_sync_poll_interval_ms: wallet_sync.poll_interval_ms,
            wallet_sync_max_iterations_per_wake_count: wallet_sync.max_iterations_per_wake_count,
            wallet_sync_timeout_seconds: wallet_sync.timeout_seconds,
            wallet_sync_max_lag_blocks: wallet_sync.max_lag_blocks,
            wallet_sync_stale_after_seconds: wallet_sync.stale_after_seconds,
        })
    }
}

fn read_listener_config() -> Result<ListenerConfig, StartupError> {
    let bind_raw =
        std::env::var("ZSPEND_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8090".to_owned());
    let bind_addr = bind_raw
        .parse()
        .map_err(|source| StartupError::BindAddress {
            provided: bind_raw,
            source,
        })?;
    let network_raw = std::env::var("ZSPEND_NETWORK").unwrap_or_else(|_| "testnet".to_owned());
    let network = parse_network(&network_raw)?;
    Ok(ListenerConfig { bind_addr, network })
}

fn read_wallet_paths() -> Result<WalletPaths, StartupError> {
    let sealed_seed_path = required_path("ZSPEND_SEALED_SEED_PATH")?;
    if let Ok(custom_identity) = std::env::var("ZSPEND_AGE_IDENTITY_PATH") {
        tracing::info!(
            custom_identity_path = %custom_identity,
            "ZSPEND_AGE_IDENTITY_PATH is informational: sealing derives identity path from sealed seed path",
        );
    }
    let storage_path = required_path("ZSPEND_STORAGE_PATH")?;
    Ok(WalletPaths {
        sealed_seed_path,
        storage_path,
    })
}

fn read_signing_config(bind_addr: SocketAddr) -> Result<SigningConfig, StartupError> {
    let max_amount_zat = std::env::var("ZSPEND_MAX_AMOUNT_ZAT")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(1_000_000_000);
    let audience = optional_env("ZSPEND_AUDIENCE").ok_or(StartupError::EnvMissing {
        name: "ZSPEND_AUDIENCE",
    })?;
    let jwks_file = optional_env("ZSPEND_JWKS_FILE").map(PathBuf::from);
    let public_base =
        optional_env("ZSPEND_PUBLIC_URL").unwrap_or_else(|| format!("http://{bind_addr}"));
    let public_sign_url = format!("{}/v1/payments/sign", public_base.trim_end_matches('/'));
    let leeway_seconds = env_u64_or("ZSPEND_LEEWAY_SECONDS", 60);
    Ok(SigningConfig {
        max_amount_zat,
        audience,
        jwks_file,
        public_sign_url,
        leeway_seconds,
    })
}

fn read_wallet_sync_config() -> WalletSyncConfig {
    WalletSyncConfig {
        poll_interval_ms: env_u64_or(
            "ZSPEND_WALLET_SYNC_POLL_INTERVAL_MS",
            DEFAULT_WALLET_SYNC_POLL_INTERVAL_MS,
        ),
        max_iterations_per_wake_count: env_u32_or(
            "ZSPEND_WALLET_SYNC_MAX_ITERATIONS_PER_WAKE_COUNT",
            DEFAULT_WALLET_SYNC_MAX_ITERATIONS_PER_WAKE_COUNT,
        ),
        timeout_seconds: env_u64_or(
            "ZSPEND_WALLET_SYNC_TIMEOUT_SECONDS",
            DEFAULT_WALLET_SYNC_TIMEOUT_SECONDS,
        ),
        max_lag_blocks: env_u32_or(
            "ZSPEND_WALLET_SYNC_MAX_LAG_BLOCKS",
            DEFAULT_WALLET_SYNC_MAX_LAG_BLOCKS,
        ),
        stale_after_seconds: env_u64_or(
            "ZSPEND_WALLET_SYNC_STALE_AFTER_SECONDS",
            DEFAULT_WALLET_SYNC_STALE_AFTER_SECONDS,
        ),
    }
}

/// Map the bound network to the readiness `posture` string. Mainnet reports
/// `"production"`; every other network reports `"dev"`.
const fn posture_for_network(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "production",
        Network::Regtest(_) | Network::Testnet | _ => "dev",
    }
}

/// Fail-closed startup gate for the seal and production postures (D-13).
///
/// A dev-posture seal (age-file sealing) is refused unless `allow_dev_seed` is
/// set, so a production deploy consciously acknowledges it is not fronting the
/// seed with a KMS/HSM. A `"production"` posture (mainnet) must additionally
/// boot with the issuer wiring that makes the trust boundary real: a configured
/// issuer URL (so revocation is enforced, not silently disabled) and a non-empty
/// JWKS (so access tokens can verify at all). Dev and testnet postures stay
/// permissive so the happy-path E2E runs without the issuer wired.
fn enforce_production_posture(
    posture: &str,
    issuer_present: bool,
    jwks_empty: bool,
    seal_posture: SealingPosture,
    allow_dev_seed: bool,
) -> Result<(), StartupError> {
    if matches!(seal_posture, SealingPosture::Dev) && !allow_dev_seed {
        return Err(StartupError::DevSeedPostureRefused);
    }
    if posture != "production" {
        return Ok(());
    }
    if !issuer_present {
        return Err(StartupError::ProductionPostureUnmet {
            missing: "ZSPEND_ISSUER_URL (revocation enforcement)",
        });
    }
    if jwks_empty {
        return Err(StartupError::ProductionPostureUnmet {
            missing: "a non-empty issuer JWKS (ZSPEND_JWKS_FILE)",
        });
    }
    Ok(())
}

/// Read an optional, trimmed, non-empty environment variable. An unset or
/// blank value yields `None`.
fn optional_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| !raw.is_empty())
}

/// Whether `name` is set to the literal `1`.
fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|raw| raw.trim() == "1")
}

fn env_u64_or(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(fallback)
}

fn env_u32_or(name: &str, fallback: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .unwrap_or(fallback)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Default single-use ledger URL: a `usage-ledger.db` file next to the wallet
/// storage database so the ledger persists on the same volume.
fn default_ledger_url(storage_path: &std::path::Path) -> String {
    let ledger_path = storage_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("usage-ledger.db");
    format!("file:{}", ledger_path.display())
}

fn required_path(name: &'static str) -> Result<PathBuf, StartupError> {
    let raw = std::env::var(name).map_err(|_| StartupError::EnvMissing { name })?;
    if raw.trim().is_empty() {
        return Err(StartupError::EnvMissing { name });
    }
    Ok(PathBuf::from(raw))
}

fn parse_network(raw: &str) -> Result<Network, StartupError> {
    match raw {
        "mainnet" => Ok(Network::Mainnet),
        "testnet" => Ok(Network::Testnet),
        "regtest" => Ok(Network::regtest()),
        other => Err(StartupError::NetworkInvalid {
            provided: other.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AmountWire, AppState, ExpiresAtWire, PaymentRequest, ProblemResponse, RevocationStore,
        SIGNED_PAYLOAD_FORMAT_WIRE_LITERAL, SignPaymentResponse, SignedPayloadWire,
        WalletSyncPhase, WalletSyncScanStatus, WalletSyncSnapshot, WalletSyncState, WireNetwork,
        WireNetworkOut, build_router, parse_network, readyz_state, recompute_intent_hash,
    };
    use crate::bootstrap::{BootstrapInputs, ChainSourceFactory, bootstrap};
    use crate::init;
    use crate::ledger::{Reservation, UsageLedger};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use http_body_util::BodyExt as _;
    use jsonwebtoken::jwk::{Jwk, JwkSet};
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::{EncodePrivateKey as _, LineEnding};
    use rand_core::OsRng;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use serde_json::json;
    use sha2::{Digest as _, Sha256};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::tempdir;
    use tower::ServiceExt as _;
    use zally_core::{BlockHeight, Network};
    use zally_keys::SealingPosture;
    use zally_testkit::MockChainSource;
    use zspend_core::{ProblemDetail, ProblemKind, SigningPolicy};

    #[test]
    fn parse_network_accepts_documented_vocabulary() -> Result<(), super::StartupError> {
        assert!(matches!(parse_network("mainnet")?, Network::Mainnet));
        assert!(matches!(parse_network("testnet")?, Network::Testnet));
        assert!(matches!(parse_network("regtest")?, Network::Regtest(_)));
        Ok(())
    }

    #[test]
    fn parse_network_rejects_unknown_value() {
        assert!(parse_network("devnet").is_err());
    }

    #[test]
    fn dev_seed_posture_requires_allow_override() {
        // A dev-posture seal is refused without the override, on any network.
        assert!(matches!(
            super::enforce_production_posture("dev", true, false, SealingPosture::Dev, false),
            Err(super::StartupError::DevSeedPostureRefused)
        ));
        // The override admits a dev-posture seal.
        assert!(
            super::enforce_production_posture("dev", true, false, SealingPosture::Dev, true)
                .is_ok()
        );
        // A non-dev posture never needs the override.
        assert!(
            super::enforce_production_posture("dev", true, false, SealingPosture::Kms, false)
                .is_ok()
        );
    }

    #[test]
    fn production_posture_requires_issuer_and_jwks() {
        // Non-dev seal so the dev-seed gate does not mask the issuer/JWKS checks.
        let seal = SealingPosture::Kms;
        // Dev network posture is permissive even with nothing wired.
        assert!(super::enforce_production_posture("dev", false, true, seal, false).is_ok());
        // Production with both wired starts.
        assert!(super::enforce_production_posture("production", true, false, seal, false).is_ok());
        // Production missing the issuer URL is refused.
        assert!(matches!(
            super::enforce_production_posture("production", false, false, seal, false),
            Err(super::StartupError::ProductionPostureUnmet { .. })
        ));
        // Production missing the JWKS is refused.
        assert!(matches!(
            super::enforce_production_posture("production", true, true, seal, false),
            Err(super::StartupError::ProductionPostureUnmet { .. })
        ));
    }

    #[tokio::test]
    async fn readyz_production_with_disabled_revocation_is_not_ready()
    -> Result<(), Box<dyn std::error::Error>> {
        // A production wallet whose revocation is disabled cannot enforce a
        // post-mint revocation and must report 503 even with a loaded JWKS.
        let issuer = fixture_issuer()?;
        let base = state_with_issuer(&issuer).await?;
        let state = AppState {
            posture: "production",
            ..base
        };
        let (status, body) = readyz_state(&state);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["jwks_cache"], "loaded");
        assert_eq!(body["revocation_cache"], "disabled");
        assert_eq!(body["posture"], "production");
        Ok(())
    }

    #[tokio::test]
    async fn readyz_dev_with_disabled_revocation_is_ready() -> Result<(), Box<dyn std::error::Error>>
    {
        // The same disabled revocation is acceptable in dev once the JWKS loads.
        let issuer = fixture_issuer()?;
        let state = state_with_issuer(&issuer).await?;
        let (status, body) = readyz_state(&state);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["revocation_cache"], "disabled");
        assert_eq!(body["posture"], "dev");
        Ok(())
    }

    #[test]
    fn signed_payload_serialises_with_expected_fields() -> Result<(), serde_json::Error> {
        let envelope = SignPaymentResponse {
            signed_payload: SignedPayloadWire {
                format: SIGNED_PAYLOAD_FORMAT_WIRE_LITERAL.to_owned(),
                bytes: "AAAA".to_owned(),
                tx_id: "deadbeef".to_owned(),
                fee: AmountWire {
                    currency: "ZEC".to_owned(),
                    value: "0".to_owned(),
                    unit: "base".to_owned(),
                },
                expires_at: ExpiresAtWire::BlockHeight(4_047_100),
                metadata: serde_json::Value::Null,
            },
        };
        let wire = serde_json::to_value(&envelope)?;
        assert_eq!(
            wire["signed_payload"]["format"].as_str(),
            Some(SIGNED_PAYLOAD_FORMAT_WIRE_LITERAL),
        );
        assert_eq!(
            wire["signed_payload"]["fee"]["currency"].as_str(),
            Some("ZEC")
        );
        assert_eq!(
            wire["signed_payload"]["expires_at"]["kind"].as_str(),
            Some("block_height"),
        );
        assert!(wire["signed_payload"].get("metadata").is_none());
        Ok(())
    }

    #[test]
    fn wire_tx_id_is_the_send_outcome_txid_in_rpc_byte_order() {
        use zally_core::{BlockHeight, Network, TxId, Zatoshis};
        use zally_pczt::PcztBytes;
        use zally_wallet::{BroadcastOutcome, SendOutcome, SignedPczt};

        // The two forms are byte-reversed views of one real testnet txid: the
        // wallet holds internal consensus bytes; every text boundary renders
        // the RPC-order hex that zcash-cli and block explorers display.
        const INTERNAL_BYTES: [u8; 32] = [
            0x36, 0x94, 0x55, 0xb7, 0x8a, 0xfc, 0xa3, 0xdc, 0xb5, 0x2b, 0xec, 0xfd, 0x38, 0x72,
            0xba, 0xf5, 0xd0, 0x51, 0xb3, 0x2e, 0x81, 0x65, 0xbc, 0x2c, 0x79, 0x61, 0x06, 0x9e,
            0xe6, 0x0c, 0xca, 0xc3,
        ];
        const RPC_HEX: &str = "c3ca0ce69e0661792cbc65812eb351d0f5ba7238fdec2bb5dca3fc8ab7559436";
        // What `hex::encode` over the raw internal bytes would emit: the
        // byte-reversed value that must never reach the wire.
        const INTERNAL_ORDER_HEX: &str =
            "369455b78afca3dcb52becfd3872baf5d051b32e8165bc2c7961069ee60ccac3";

        let tx_id = TxId::from_bytes(INTERNAL_BYTES);
        let outcome = SendOutcome {
            signed: SignedPczt {
                tx_id,
                fee_zat: Zatoshis::zero(),
                tx_expiry_height: BlockHeight::from(TARGET_EXPIRY),
            },
            broadcast: BroadcastOutcome {
                tx_id,
                broadcast_at_height: BlockHeight::from(0),
            },
        };

        let signed_pczt = PcztBytes::from_serialized(b"signed-pczt".to_vec(), Network::Testnet);
        let wire = super::signed_payload_response(&outcome, &signed_pczt);

        assert_eq!(
            wire.signed_payload.tx_id,
            outcome.signed.tx_id.to_rpc_hex(),
            "wire tx_id must be the send outcome's own txid",
        );
        assert_eq!(
            wire.signed_payload.tx_id, RPC_HEX,
            "wire tx_id must render in canonical RPC byte order",
        );
        assert_ne!(
            wire.signed_payload.tx_id, INTERNAL_ORDER_HEX,
            "internal consensus-order bytes must not reach the wire",
        );
    }

    #[tokio::test]
    async fn readyz_reports_empty_jwks_and_503() -> Result<(), Box<dyn std::error::Error>> {
        // Default build_state_for_network leaves the JWKS empty.
        let state = build_state_for_network(Network::Testnet, 4_047_000).await?;
        let (status, body) = readyz_state(&state);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["jwks_cache"], "empty");
        assert_eq!(body["sealed_seed"], "dev");
        assert_eq!(body["posture"], "dev");
        assert_eq!(body["revocation_cache"], "disabled");
        Ok(())
    }

    #[tokio::test]
    async fn readyz_reports_loaded_jwks_and_200() -> Result<(), Box<dyn std::error::Error>> {
        let issuer = fixture_issuer()?;
        let state = state_with_issuer(&issuer).await?;
        let (status, body) = readyz_state(&state);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["jwks_cache"], "loaded");
        assert_eq!(body["revocation_cache"], "disabled");
        assert_eq!(body["wallet_sync"]["freshness"], "fresh");
        assert_eq!(body["wallet_sync"]["lag_blocks"].as_u64(), Some(0));
        Ok(())
    }

    #[tokio::test]
    async fn readyz_reports_stale_wallet_sync_and_503() -> Result<(), Box<dyn std::error::Error>> {
        let issuer = fixture_issuer()?;
        let base = state_with_issuer(&issuer).await?;
        let state = AppState {
            wallet_sync: stale_wallet_sync(Network::Testnet, 4_047_000),
            ..base
        };
        let (status, body) = readyz_state(&state);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["wallet_sync"]["phase"], "parked");
        assert_eq!(body["wallet_sync"]["freshness"], "stale");
        assert_eq!(body["wallet_sync"]["is_fresh"].as_bool(), Some(false));
        Ok(())
    }

    #[tokio::test]
    async fn metrics_route_renders_exposition() -> Result<(), Box<dyn std::error::Error>> {
        let state = build_state_for_network(Network::Testnet, 4_047_000).await?;
        let request = Request::builder()
            .method("GET")
            .uri("/metrics")
            .body(Body::empty())?;
        let response = build_router(state).oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_type.starts_with("text/plain"),
            "metrics content-type must be text/plain, got {content_type}",
        );
        Ok(())
    }

    #[test]
    fn problem_response_emits_problem_json_content_type() {
        let response = ProblemResponse::bad_request(ProblemDetail::not_retryable(
            ProblemKind::PaymentRequestInvalid,
            "bad scheme",
            "only zip321 supported",
        ))
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(ct, "application/problem+json");
    }

    #[test]
    fn wire_network_round_trips_through_zally_network() {
        for net in [Network::Mainnet, Network::Testnet, Network::regtest()] {
            let wire = WireNetwork::from_zally(net);
            let back = wire.to_zally();
            assert_eq!(
                std::mem::discriminant(&net),
                std::mem::discriminant(&back),
                "round-trip must preserve the network variant"
            );
        }
    }

    async fn build_state_for_network(
        network: Network,
        birthday: u32,
    ) -> Result<AppState, Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let sealed_seed = dir.path().join("wallet.age");
        let storage = dir.path().join("wallet.db");
        init::run(sealed_seed.clone(), false, false, false).await?;

        let mock = Arc::new(MockChainSource::new(network));
        let (wallet, account_id, _chain) = bootstrap(BootstrapInputs {
            network,
            sealed_seed_path: sealed_seed,
            storage_path: storage,
            indexer_grpc_addr: None,
            birthday_override: Some(birthday),
            chain_source_factory: ChainSourceFactory::Custom(mock),
        })
        .await?;

        let policy = SigningPolicy::builder()
            .network(network)
            .max_amount_zat(1_000_000_000)
            .audience("urn:zentity:wallet:test")
            .build()?;

        let spend_idempotency_cache = UsageLedger::ephemeral_for_tests();

        // Hold `dir` for the wallet's lifetime by leaking it: the temp dir
        // lives as long as the test process so the sqlite handles stay valid.
        std::mem::forget(dir);

        Ok(AppState {
            wallet,
            account_id,
            policy: Arc::new(policy),
            jwks: Arc::new(jsonwebtoken::jwk::JwkSet { keys: Vec::new() }),
            public_sign_url: Arc::from("http://127.0.0.1:8090/v1/payments/sign"),
            leeway_seconds: 60,
            posture: "dev",
            seal_posture: SealingPosture::Dev,
            dpop_proof_jti_seen: Arc::new(
                parking_lot::Mutex::new(std::collections::HashMap::new()),
            ),
            spend_idempotency_cache,
            revocation: RevocationStore::disabled(),
            wallet_sync: fresh_wallet_sync(network, birthday),
            wallet_sync_max_lag_blocks: 3,
            wallet_sync_stale_after_seconds: 30,
            metrics: metrics_exporter_prometheus::PrometheusBuilder::new()
                .build_recorder()
                .handle(),
        })
    }

    fn fresh_wallet_sync(network: Network, height: u32) -> WalletSyncState {
        let height = BlockHeight::from(height);
        WalletSyncState::new(WalletSyncSnapshot {
            network,
            phase: WalletSyncPhase::Waiting,
            sync_status: WalletSyncScanStatus::AtTip,
            scanned_height: Some(height),
            safe_chain_tip_height: Some(height),
            lag_blocks: Some(0),
            last_fault: None,
            published_at_ms: super::unix_now_ms(),
        })
    }

    fn stale_wallet_sync(network: Network, height: u32) -> WalletSyncState {
        let scanned_height = BlockHeight::from(height);
        let safe_chain_tip_height = BlockHeight::from(height.saturating_add(10));
        WalletSyncState::new(WalletSyncSnapshot {
            network,
            phase: WalletSyncPhase::Parked,
            sync_status: WalletSyncScanStatus::CatchingUp,
            scanned_height: Some(scanned_height),
            safe_chain_tip_height: Some(safe_chain_tip_height),
            lag_blocks: Some(10),
            last_fault: None,
            published_at_ms: super::unix_now_ms(),
        })
    }

    #[tokio::test]
    async fn current_unified_address_returns_testnet_ua() -> Result<(), Box<dyn std::error::Error>>
    {
        let state = build_state_for_network(Network::Testnet, 4_047_000).await?;

        let ua = state.current_unified_address().await?;

        assert!(
            ua.starts_with("utest1"),
            "testnet UA must carry the utest1 HRP; got {ua}"
        );
        assert!(
            (80..=256).contains(&ua.len()),
            "UA length should fall in the expected range; got {} ({ua})",
            ua.len(),
        );

        let again = state.current_unified_address().await?;
        assert_eq!(
            ua, again,
            "second call must surface the same address (idempotent allocator)"
        );
        Ok(())
    }

    #[tokio::test]
    async fn wallet_address_response_serialises_with_network_label()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = build_state_for_network(Network::Testnet, 4_047_000).await?;
        let body = super::WalletAddressResponse {
            ua: state.current_unified_address().await?,
            network: WireNetworkOut::from(WireNetwork::from_zally(state.policy.network())),
        };

        let wire = serde_json::to_value(&body)?;
        assert_eq!(wire["network"].as_str(), Some("testnet"));
        let ua_field = wire["ua"].as_str().unwrap_or_default();
        assert!(ua_field.starts_with("utest1"), "ua must be a utest1 UA");
        Ok(())
    }

    // ----- Slice 1 trust-boundary HTTP integration tests -----
    //
    // These drive the real Axum handler over HTTP (tower oneshot) to prove the
    // verifier chain is wired into `/v1/payments/sign` and rejects an
    // unauthenticated or non-conformant request before any signing work. The
    // per-verifier logic is covered by the zspend-core unit tests; these tests
    // prove the integration.

    const ISSUER_KID: &str = "test-issuer";
    const WALLET_AUDIENCE: &str = "urn:zentity:wallet:test";
    const FAR_FUTURE: i64 = 4_102_444_800;

    struct FixtureIssuer {
        encoding: EncodingKey,
        jwks: JwkSet,
    }

    fn fixture_issuer() -> Result<FixtureIssuer, Box<dyn std::error::Error>> {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())?;
        let keypair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())?;
        let public_x = URL_SAFE_NO_PAD.encode(keypair.public_key().as_ref());
        let encoding = EncodingKey::from_ed_der(pkcs8.as_ref());
        let jwk: Jwk = serde_json::from_value(json!({
            "kty": "OKP",
            "crv": "Ed25519",
            "x": public_x,
            "kid": ISSUER_KID,
            "alg": "EdDSA",
            "use": "sig",
        }))?;
        Ok(FixtureIssuer {
            encoding,
            jwks: JwkSet { keys: vec![jwk] },
        })
    }

    fn rar() -> serde_json::Value {
        json!({
            "type": "payment_authorization",
            "chain": { "namespace": "zcash", "reference": "test" },
            "recipient": "zcash:test:utest1qq",
            "amount": { "currency": "ZEC", "value": "50000000", "unit": "base" },
            "payment_id": "01KT9A0V431VGD5YH7R7G635HC",
            "intent_hash": "v1:sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "expires_at": { "kind": "block_height", "value": 4_047_100 }
        })
    }

    fn mint_at_jwt(
        issuer: &FixtureIssuer,
        aud: &str,
        exp: i64,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(ISSUER_KID.to_owned());
        let claims = json!({
            "aud": aud,
            "jti": "01ACCESSTOKENJTI0000000000",
            "exp": exp,
            "cnf": { "jkt": "dpop-key-thumbprint" },
            "authorization_details": [rar()],
        });
        Ok(encode(&header, &claims, &issuer.encoding)?)
    }

    fn sign_body() -> String {
        json!({
            "payment_request": { "scheme": "zip321", "value": "zcash:utest1qq?amount=0.5" },
            "network": "testnet",
            "payment_id": "01KT9A0V431VGD5YH7R7G635HC",
            "target_expiry_height": 4_047_100,
        })
        .to_string()
    }

    async fn state_with_issuer(
        issuer: &FixtureIssuer,
    ) -> Result<AppState, Box<dyn std::error::Error>> {
        let base = build_state_for_network(Network::Testnet, 4_047_000).await?;
        Ok(AppState {
            jwks: Arc::new(issuer.jwks.clone()),
            ..base
        })
    }

    /// POST to `/v1/payments/sign` over a real oneshot request and return the
    /// HTTP status plus the problem-detail `kind`.
    async fn post_sign(
        state: AppState,
        authorization: Option<&str>,
        dpop: Option<&str>,
    ) -> Result<(StatusCode, ProblemKind), Box<dyn std::error::Error>> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/payments/sign")
            .header("content-type", "application/json");
        if let Some(header_value) = authorization {
            builder = builder.header("authorization", header_value);
        }
        if let Some(header_value) = dpop {
            builder = builder.header("dpop", header_value);
        }
        let request = builder.body(Body::from(sign_body()))?;
        let response = build_router(state).oneshot(request).await?;
        let status = response.status();
        let bytes = response.into_body().collect().await?.to_bytes();
        let problem: ProblemDetail = serde_json::from_slice(&bytes)?;
        Ok((status, problem.kind))
    }

    #[tokio::test]
    async fn sign_without_authorization_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let state = build_state_for_network(Network::Testnet, 4_047_000).await?;
        let (status, kind) = post_sign(state, None, None).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(kind, ProblemKind::AccessTokenInvalid);
        Ok(())
    }

    #[tokio::test]
    async fn sign_with_unverifiable_token_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let state = build_state_for_network(Network::Testnet, 4_047_000).await?;
        let (status, kind) = post_sign(state, Some("DPoP not-a-jwt"), Some("not-a-proof")).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(kind, ProblemKind::AccessTokenInvalid);
        Ok(())
    }

    #[tokio::test]
    async fn sign_with_wrong_audience_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let issuer = fixture_issuer()?;
        let state = state_with_issuer(&issuer).await?;
        let token = mint_at_jwt(&issuer, "some-other-wallet", FAR_FUTURE)?;
        let (status, kind) =
            post_sign(state, Some(&format!("DPoP {token}")), Some("any-proof")).await?;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(kind, ProblemKind::AudienceMismatch);
        Ok(())
    }

    #[tokio::test]
    async fn sign_with_valid_token_but_bad_dpop_is_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let issuer = fixture_issuer()?;
        let state = state_with_issuer(&issuer).await?;
        let token = mint_at_jwt(&issuer, WALLET_AUDIENCE, FAR_FUTURE)?;
        let (status, kind) = post_sign(
            state,
            Some(&format!("DPoP {token}")),
            Some("not-a-valid-dpop-proof"),
        )
        .await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(kind, ProblemKind::DpopProofInvalid);
        Ok(())
    }

    // ----- Positive and ledger-path integration tests -----
    //
    // These mint a real EdDSA at+jwt plus a matching ES256 DPoP proof so the
    // request clears the whole boundary (token, DPoP, intent recompute, ledger
    // reserve). A funded wallet / live chain is not available in-process, so a
    // request that passes the boundary reaches `wallet.send_payment` and fails
    // with a wallet-level error (no spendable funds), which is the signal that
    // the verifier+intent+ledger reserve all passed. The signed-payload wire
    // shape and the cached-replay return are covered by the ledger unit tests
    // and `signed_payload_serialises_with_expected_fields`.

    const SIGN_URL: &str = "http://127.0.0.1:8090/v1/payments/sign";
    const ACCESS_TOKEN_JTI: &str = "01ACCESSTOKENJTI0000000000";
    const PAYMENT_ID: &str = "01KT9A0V431VGD5YH7R7G635HC";
    const TARGET_EXPIRY: u32 = 4_047_100;

    fn unix_now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
    }

    /// An ES256 DPoP keypair and its RFC 7638 thumbprint.
    struct DpopKey {
        encoding: EncodingKey,
        x: String,
        y: String,
        jkt: String,
    }

    fn dpop_key() -> Result<DpopKey, Box<dyn std::error::Error>> {
        let signing_key = SigningKey::random(&mut OsRng);
        let point = signing_key.verifying_key().to_encoded_point(false);
        let x = URL_SAFE_NO_PAD.encode(point.x().ok_or("no x coordinate")?);
        let y = URL_SAFE_NO_PAD.encode(point.y().ok_or("no y coordinate")?);
        let pem = signing_key.to_pkcs8_pem(LineEnding::LF)?.to_string();
        let encoding = EncodingKey::from_ec_pem(pem.as_bytes())?;
        let jkt = zspend_core::ec_jwk_thumbprint("P-256", "EC", &x, &y);
        Ok(DpopKey {
            encoding,
            x,
            y,
            jkt,
        })
    }

    /// Mint an ES256 DPoP proof bound to `access_token` for `POST SIGN_URL`.
    fn mint_dpop(key: &DpopKey, access_token: &str) -> Result<String, Box<dyn std::error::Error>> {
        let mut header = Header::new(Algorithm::ES256);
        header.typ = Some("dpop+jwt".to_owned());
        header.jwk = Some(serde_json::from_value(json!({
            "kty": "EC", "crv": "P-256", "x": key.x, "y": key.y,
        }))?);
        let ath = URL_SAFE_NO_PAD.encode(Sha256::digest(access_token.as_bytes()));
        let claims = json!({
            "htm": "POST",
            "htu": SIGN_URL,
            "jti": "01DPOPPROOFJTI000000000000",
            "iat": unix_now_secs(),
            "ath": ath,
        });
        Ok(encode(&header, &claims, &key.encoding)?)
    }

    /// Mint an EdDSA at+jwt carrying a single `payment_authorization` RAR with
    /// the given `intent_hash`, bound to `dpop_jkt`.
    fn mint_token_with_intent(
        issuer: &FixtureIssuer,
        dpop_jkt: &str,
        recipient_rar: &str,
        intent_hash: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(ISSUER_KID.to_owned());
        let claims = json!({
            "aud": WALLET_AUDIENCE,
            "jti": ACCESS_TOKEN_JTI,
            "exp": FAR_FUTURE,
            "cnf": { "jkt": dpop_jkt },
            "authorization_details": [{
                "type": "payment_authorization",
                "chain": { "namespace": "zcash", "reference": "test" },
                "recipient": recipient_rar,
                "amount": { "currency": "ZEC", "value": "50000000", "unit": "base" },
                "payment_id": PAYMENT_ID,
                "intent_hash": intent_hash,
                "expires_at": { "kind": "block_height", "value": TARGET_EXPIRY },
            }],
        });
        Ok(encode(&header, &claims, &issuer.encoding)?)
    }

    /// Build a `payment_authorization` for the intent recompute that pins the
    /// signed `chain`/`amount`/`payment_id`/`expiry`; `intent_hash` is
    /// irrelevant to the recompute itself and carries a placeholder.
    fn recompute_auth(recipient_rar: &str) -> zspend_core::PaymentAuthorization {
        use zspend_core::{
            Amount, AmountUnit, ChainId, ExpiresAt, IntentHashString, PaymentAuthorizationType,
        };
        zspend_core::PaymentAuthorization {
            authorization_type: PaymentAuthorizationType::PaymentAuthorization,
            chain: ChainId {
                namespace: "zcash".to_owned(),
                reference: "test".to_owned(),
            },
            recipient: recipient_rar.to_owned(),
            amount: Amount {
                currency: "ZEC".to_owned(),
                value: "50000000".to_owned(),
                unit: AmountUnit::Base,
            },
            payment_id: PAYMENT_ID.to_owned(),
            intent_hash: IntentHashString("v1:sha256:placeholder".to_owned()),
            expires_at: ExpiresAt::BlockHeight(TARGET_EXPIRY),
        }
    }

    /// Derive the wallet's UA, build a paying URI, and parse it the way the
    /// handler does. Returns the URI plus the CAIP-10 recipient string and
    /// the amount the handler will hash.
    async fn paying_request(
        state: &AppState,
    ) -> Result<(String, String, u64), Box<dyn std::error::Error>> {
        let ua = state.current_unified_address().await?;
        let uri = format!("zcash:{ua}?amount=0.5");
        let parsed = PaymentRequest::from_uri(&uri, Network::Testnet)?;
        let payment = parsed.payments().first().ok_or("no payment parsed")?;
        let recipient_caip10 = format!("zcash:test:{}", payment.recipient.encoded());
        let amount = payment.amount.as_u64();
        Ok((uri, recipient_caip10, amount))
    }

    fn sign_body_for(uri: &str) -> String {
        json!({
            "payment_request": { "scheme": "zip321", "value": uri },
            "network": "testnet",
            "payment_id": PAYMENT_ID,
            "target_expiry_height": TARGET_EXPIRY,
        })
        .to_string()
    }

    /// POST a custom body with a real DPoP-bound token and return status + kind.
    async fn post_sign_body(
        state: AppState,
        token: &str,
        proof: &str,
        body: String,
    ) -> Result<(StatusCode, ProblemKind), Box<dyn std::error::Error>> {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/payments/sign")
            .header("content-type", "application/json")
            .header("authorization", format!("DPoP {token}"))
            .header("dpop", proof)
            .body(Body::from(body))?;
        let response = build_router(state).oneshot(request).await?;
        let status = response.status();
        let bytes = response.into_body().collect().await?.to_bytes();
        let problem: ProblemDetail = serde_json::from_slice(&bytes)?;
        Ok((status, problem.kind))
    }

    #[tokio::test]
    async fn conformant_request_clears_boundary_and_reaches_signing()
    -> Result<(), Box<dyn std::error::Error>> {
        let issuer = fixture_issuer()?;
        let state = state_with_issuer(&issuer).await?;
        let (uri, recipient_caip10, amount) = paying_request(&state).await?;
        let intent_hash = recompute_intent_hash(
            &recompute_auth(&recipient_caip10),
            &recipient_caip10,
            amount,
        )?;
        let key = dpop_key()?;
        let token = mint_token_with_intent(&issuer, &key.jkt, &recipient_caip10, &intent_hash)?;
        let proof = mint_dpop(&key, &token)?;

        let (status, kind) = post_sign_body(state, &token, &proof, sign_body_for(&uri)).await?;

        // The wallet has no spendable funds in-process, so a request that
        // passed the whole boundary fails inside `wallet.send_payment` with a
        // wallet-layer error (no funds / proposal rejected), NOT a boundary
        // rejection. That a wallet-layer kind is returned proves the token,
        // DPoP, intent recompute, and ledger reserve all admitted the request
        // up to signing.
        assert!(
            matches!(
                kind,
                ProblemKind::InsufficientFunds
                    | ProblemKind::NotReady
                    | ProblemKind::ChainUnreachable
            ),
            "expected a wallet-layer outcome after clearing the boundary, got {kind:?} (status {status})",
        );
        assert!(
            !matches!(
                kind,
                ProblemKind::AccessTokenInvalid
                    | ProblemKind::AudienceMismatch
                    | ProblemKind::DpopProofInvalid
                    | ProblemKind::IntentMismatch
                    | ProblemKind::RecipientMismatch
                    | ProblemKind::TokenAlreadyConsumed
                    | ProblemKind::AuthorizationExpired
            ),
            "boundary must have admitted the request, but it was rejected with {kind:?}",
        );
        Ok(())
    }

    #[tokio::test]
    async fn stale_wallet_sync_refuses_signing_before_reserving_jti()
    -> Result<(), Box<dyn std::error::Error>> {
        let issuer = fixture_issuer()?;
        let base = state_with_issuer(&issuer).await?;
        let ledger = base.spend_idempotency_cache.clone();
        let state = AppState {
            wallet_sync: stale_wallet_sync(Network::Testnet, 4_047_000),
            ..base
        };

        let (status, kind) = post_conformant_sign(state, &issuer).await?;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(kind, ProblemKind::WalletUnavailable);
        assert!(
            matches!(
                ledger.reserve(ACCESS_TOKEN_JTI, "v1:sha256:any").await?,
                Reservation::Fresh
            ),
            "stale wallet sync must not consume the access-token jti",
        );
        Ok(())
    }

    #[tokio::test]
    async fn identical_replay_returns_the_cached_signed_payload()
    -> Result<(), Box<dyn std::error::Error>> {
        let issuer = fixture_issuer()?;
        let state = state_with_issuer(&issuer).await?;
        let (uri, recipient_caip10, amount) = paying_request(&state).await?;
        let intent_hash = recompute_intent_hash(
            &recompute_auth(&recipient_caip10),
            &recipient_caip10,
            amount,
        )?;
        let key = dpop_key()?;
        let token = mint_token_with_intent(&issuer, &key.jkt, &recipient_caip10, &intent_hash)?;
        let proof = mint_dpop(&key, &token)?;

        // Seed a committed payload for this jti+intent, as a prior signed call
        // would have. The replay must return it verbatim without re-signing.
        let cached = SignPaymentResponse {
            signed_payload: SignedPayloadWire {
                format: SIGNED_PAYLOAD_FORMAT_WIRE_LITERAL.to_owned(),
                bytes: "Y2FjaGVk".to_owned(),
                tx_id: "cafef00d".to_owned(),
                fee: AmountWire {
                    currency: "ZEC".to_owned(),
                    value: "1000".to_owned(),
                    unit: "base".to_owned(),
                },
                expires_at: ExpiresAtWire::BlockHeight(TARGET_EXPIRY),
                metadata: serde_json::Value::Null,
            },
        };
        state
            .spend_idempotency_cache
            .reserve(ACCESS_TOKEN_JTI, &intent_hash)
            .await?;
        state
            .spend_idempotency_cache
            .commit(ACCESS_TOKEN_JTI, &intent_hash, &cached)
            .await?;

        let request = Request::builder()
            .method("POST")
            .uri("/v1/payments/sign")
            .header("content-type", "application/json")
            .header("authorization", format!("DPoP {token}"))
            .header("dpop", proof)
            .body(Body::from(sign_body_for(&uri)))?;
        let response = build_router(state).oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await?.to_bytes();
        let wire: serde_json::Value = serde_json::from_slice(&bytes)?;

        assert_eq!(
            wire["signed_payload"]["format"].as_str(),
            Some(SIGNED_PAYLOAD_FORMAT_WIRE_LITERAL),
        );
        assert_eq!(wire["signed_payload"]["tx_id"].as_str(), Some("cafef00d"));
        assert_eq!(wire["signed_payload"]["bytes"].as_str(), Some("Y2FjaGVk"));
        assert_eq!(
            wire["signed_payload"]["fee"]["currency"].as_str(),
            Some("ZEC"),
        );
        assert_eq!(
            wire["signed_payload"]["expires_at"]["kind"].as_str(),
            Some("block_height"),
        );
        Ok(())
    }

    #[tokio::test]
    async fn same_jti_with_different_intent_is_token_already_consumed()
    -> Result<(), Box<dyn std::error::Error>> {
        let issuer = fixture_issuer()?;
        let state = state_with_issuer(&issuer).await?;
        let (uri, recipient_caip10, amount) = paying_request(&state).await?;
        let intent_hash = recompute_intent_hash(
            &recompute_auth(&recipient_caip10),
            &recipient_caip10,
            amount,
        )?;
        let key = dpop_key()?;

        // First call reserves the jti (then fails on funds, releasing it). To
        // hold a committed/conflicting reservation we instead drive the ledger
        // directly so the second call observes a same-jti / different-intent
        // reservation regardless of the wallet outcome.
        state
            .spend_idempotency_cache
            .reserve(ACCESS_TOKEN_JTI, "v1:sha256:a-different-intent-hash")
            .await?;

        let token = mint_token_with_intent(&issuer, &key.jkt, &recipient_caip10, &intent_hash)?;
        let proof = mint_dpop(&key, &token)?;
        let (status, kind) = post_sign_body(state, &token, &proof, sign_body_for(&uri)).await?;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(kind, ProblemKind::TokenAlreadyConsumed);
        Ok(())
    }

    #[tokio::test]
    async fn recipient_swap_is_recipient_mismatch_not_intent_mismatch()
    -> Result<(), Box<dyn std::error::Error>> {
        let issuer = fixture_issuer()?;
        let state = state_with_issuer(&issuer).await?;
        let (uri, _recipient_caip10, amount) = paying_request(&state).await?;
        // The intent hash is computed over a DIFFERENT recipient than the one
        // the request parses, with the same amount/expiry/payment_id, so only
        // the payee diverges.
        let other_recipient = "zcash:test:utest1someotherrecipientaddressxyz";
        let intent_hash =
            recompute_intent_hash(&recompute_auth(other_recipient), other_recipient, amount)?;
        let key = dpop_key()?;
        let token = mint_token_with_intent(&issuer, &key.jkt, other_recipient, &intent_hash)?;
        let proof = mint_dpop(&key, &token)?;

        let (status, kind) = post_sign_body(state, &token, &proof, sign_body_for(&uri)).await?;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(kind, ProblemKind::RecipientMismatch);
        Ok(())
    }

    // ----- Revocation-path integration tests (D-6) -----
    //
    // The revocation check runs after the trust boundary admits the request and
    // BEFORE the single-use reserve, so a revoked-then-replayed token returns
    // `token_revoked` rather than a cached payload. These drive a conformant
    // DPoP-bound request through the handler with an enabled revocation store
    // seeded directly (no live issuer) to isolate the revocation outcome.

    /// Build a conformant, fully-boundary-clearing request (token + DPoP +
    /// intent) against `state` and post it. Returns status + problem kind.
    async fn post_conformant_sign(
        state: AppState,
        issuer: &FixtureIssuer,
    ) -> Result<(StatusCode, ProblemKind), Box<dyn std::error::Error>> {
        let (uri, recipient_caip10, amount) = paying_request(&state).await?;
        let intent_hash = recompute_intent_hash(
            &recompute_auth(&recipient_caip10),
            &recipient_caip10,
            amount,
        )?;
        let key = dpop_key()?;
        let token = mint_token_with_intent(issuer, &key.jkt, &recipient_caip10, &intent_hash)?;
        let proof = mint_dpop(&key, &token)?;
        post_sign_body(state, &token, &proof, sign_body_for(&uri)).await
    }

    #[tokio::test]
    async fn revoked_jti_is_rejected_before_signing() -> Result<(), Box<dyn std::error::Error>> {
        let issuer = fixture_issuer()?;
        let base = state_with_issuer(&issuer).await?;
        // Enabled store with the access-token jti already in the revoked set and
        // a fresh refresh stamp, so `check` returns Revoked without any network.
        let state = AppState {
            revocation: RevocationStore::seeded_for_test(
                "http://issuer.invalid",
                &[ACCESS_TOKEN_JTI],
                Some(std::time::Instant::now()),
                std::time::Duration::from_secs(30),
            ),
            ..base
        };

        let (status, kind) = post_conformant_sign(state, &issuer).await?;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(kind, ProblemKind::TokenRevoked);
        Ok(())
    }

    #[tokio::test]
    async fn stale_revocation_cache_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
        let issuer = fixture_issuer()?;
        let base = state_with_issuer(&issuer).await?;
        // Enabled store pointed at an unreachable port with a zero staleness
        // bound, so the prior refresh stamp is immediately past the bound: the
        // sign-time refresh fails, the cache stays stale, and the handler fails
        // closed with a retryable 503.
        let state = AppState {
            revocation: RevocationStore::seeded_for_test(
                "http://127.0.0.1:1",
                &[],
                Some(std::time::Instant::now()),
                std::time::Duration::ZERO,
            ),
            ..base
        };

        let (status, kind) = post_conformant_sign(state, &issuer).await?;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(kind, ProblemKind::RevocationCacheStale);
        Ok(())
    }

    #[test]
    fn cache_stale_response_carries_numeric_retry_after() {
        let response = super::problem_response(ProblemDetail::retryable(
            ProblemKind::RevocationCacheStale,
            "revocation_cache_stale",
            "cache too old",
        ))
        .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let retry_after = response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        assert_eq!(
            retry_after,
            Some(super::DEFAULT_REVOCATION_MAX_STALENESS_SECONDS),
            "a revocation_cache_stale response must hint a numeric Retry-After",
        );
    }

    #[test]
    fn not_ready_response_carries_numeric_retry_after() {
        let response = super::problem_response(ProblemDetail::retryable(
            ProblemKind::NotReady,
            "not_ready",
            "in-flight reservation",
        ))
        .into_response();
        let retry_after = response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        assert!(
            retry_after.is_some_and(|seconds| seconds > 0),
            "a not_ready response must hint a positive numeric Retry-After",
        );
    }

    #[test]
    fn wallet_unavailable_response_carries_numeric_retry_after() {
        let response = super::problem_response(ProblemDetail::retryable(
            ProblemKind::WalletUnavailable,
            "wallet_unavailable",
            "wallet sync stale",
        ))
        .into_response();
        let retry_after = response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        assert_eq!(
            retry_after,
            Some(super::DEFAULT_WALLET_SYNC_RETRY_SECONDS),
            "a wallet_unavailable response must hint a numeric Retry-After",
        );
    }

    #[test]
    fn not_retryable_response_omits_retry_after() {
        let response = super::problem_response(ProblemDetail::not_retryable(
            ProblemKind::IntentMismatch,
            "intent_mismatch",
            "recomputed hash diverged",
        ))
        .into_response();
        assert!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .is_none(),
            "a non-retryable problem must not carry a Retry-After header",
        );
    }

    #[tokio::test]
    async fn disabled_revocation_store_admits_request_to_signing()
    -> Result<(), Box<dyn std::error::Error>> {
        let issuer = fixture_issuer()?;
        // state_with_issuer leaves the revocation store disabled, matching local
        // dev with no ZSPEND_ISSUER_URL: the check returns Live and the request
        // proceeds past revocation to the wallet (which then fails on funds).
        let state = state_with_issuer(&issuer).await?;

        let (status, kind) = post_conformant_sign(state, &issuer).await?;

        assert!(
            !matches!(
                kind,
                ProblemKind::TokenRevoked | ProblemKind::RevocationCacheStale
            ),
            "a disabled revocation store must not reject the request; got {kind:?} (status {status})",
        );
        Ok(())
    }
}
