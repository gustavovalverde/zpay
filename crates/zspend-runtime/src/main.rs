//! Wallet runtime binary for the zspend service.
//!
//! Phase 4 of Proposal-0003 (see
//! `docs/proposals/0003-agent-wallet-production-architecture.md`). The
//! binary opens a zally-backed wallet against a sealed seed, exposes
//! `/v1/payments/sign` for the agent flow, and reports liveness and
//! readiness on the standard probes. The full DPoP, JWKS, RAR, revocation,
//! and usage-ledger verifier stack lands in a follow-on slice (D-1, D-5,
//! D-6, D-8); the routes in this binary stub those checks with a TODO and
//! the [`ProblemKind::NotReady`] response so the operational shape is
//! visible end-to-end while the auth wiring lands incrementally.

mod bootstrap;
mod capture_submitter;
mod init;

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
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bootstrap::{BootstrapError, BootstrapInputs, ChainSourceFactory};
use capture_submitter::CaptureSubmitter;
use clap::{Parser, Subcommand};
use jsonwebtoken::jwk::JwkSet;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use zally_core::{AccountId, BlockHeight, IdempotencyKey, Network, PaymentRecipient};
use zally_wallet::{PaymentRequest, SendPaymentPlan, Wallet, WalletError};
use zspend_core::{
    AccessTokenClaims, DPOP_CLOCK_SKEW_SECONDS, DpopBinding, PaymentAuthorization, ProblemDetail,
    ProblemKind, SigningPolicy, SigningPolicyError, intent_matches, verify_access_token,
    verify_dpop_proof,
};

/// Wire format identifier returned on `/v1/payments/sign`.
///
/// Phase 4 returns raw consensus-encoded Zcash v5 transaction bytes because
/// the PCZT methods on `zally::Wallet` (Phase 2d) and the matching `/settle`
/// extractor on `zpay-runtime` (Phase 2g) have not yet landed. The follow-on
/// slice flips this to `"pczt-v1"` (matching
/// `zally_core::SignedPayloadFormat::PcztV1`) once both ends ship the PCZT
/// path. Until then the wire schema diverges from the locked envelope in
/// `zally_core::SignedPayload` on this single field only; the surrounding
/// shape (`bytes`, `tx_id`, `fee`, `expires_at`, `metadata`) is identical.
const SIGNED_PAYLOAD_FORMAT_WIRE_LITERAL: &str = "raw-zcash-v5";

/// Liveness/readiness/probe contracts the runtime emits, kept here so the
/// strings can be compared in tests without re-allocating.
const READYZ_BODY: &str = r#"{"sealed_seed":"available","posture":"dev","jwks_cache":"unused","revocation_cache":"unused"}"#;

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
    /// Generate a fresh wallet seed and seal it at `$ZSPEND_SEALED_SEED_PATH`.
    Init {
        /// Overwrite an existing sealed seed instead of refusing.
        #[arg(long)]
        force: bool,
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
}

/// Window (seconds) after which a recorded DPoP proof `jti` is evicted from the
/// per-process anti-replay set.
const DPOP_REPLAY_WINDOW_SECONDS: u64 = 300;

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
    /// Short-window DPoP proof `jti` anti-replay set (per-process).
    dpop_seen: Arc<Mutex<HashMap<String, Instant>>>,
    /// Single-use access-token `jti` ledger (write-then-sign, D-8). Maps a
    /// consumed `jti` to the signed payload returned on an identical replay.
    spend_ledger: Arc<Mutex<HashMap<String, SignPaymentResponse>>>,
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
            sealed_seed_path,
        } => init::run(sealed_seed_path, force)
            .await
            .map_err(|source| StartupError::Init { source }),
    }
}

async fn serve() -> Result<(), StartupError> {
    let config = ResolvedConfig::from_env()?;

    let (wallet, account_id) = bootstrap::bootstrap(BootstrapInputs {
        network: config.network,
        sealed_seed_path: config.sealed_seed_path,
        storage_path: config.storage_path,
        indexer_grpc_addr: config.indexer_grpc_addr,
        birthday_override: config.birthday_override,
        chain_source_factory: ChainSourceFactory::Live,
    })
    .await
    .map_err(|source| StartupError::Bootstrap { source })?;

    let policy = SigningPolicy::builder()
        .network(config.network)
        .max_amount_zat(config.max_amount_zat)
        .audience_thumbprint(config.audience_thumbprint)
        .build()
        .map_err(|source| StartupError::SigningPolicy { source })?;

    let jwks = load_jwks(config.jwks_file.as_ref())?;
    if jwks.keys.is_empty() {
        tracing::warn!(
            "no ZSPEND_JWKS_FILE configured: every /v1/payments/sign call fails closed until the issuer JWKS is wired",
        );
    }

    let state = AppState {
        wallet,
        account_id,
        policy: Arc::new(policy),
        jwks: Arc::new(jwks),
        public_sign_url: Arc::from(config.public_sign_url.as_str()),
        leeway_seconds: config.leeway_seconds,
        dpop_seen: Arc::new(Mutex::new(HashMap::new())),
        spend_ledger: Arc::new(Mutex::new(HashMap::new())),
    };

    let router = build_router(state);

    let listener = tokio::net::TcpListener::bind(config.bind_addr)
        .await
        .map_err(|source| StartupError::Bind {
            addr: config.bind_addr,
            source,
        })?;

    tracing::info!(
        bind = %config.bind_addr,
        network = ?config.network,
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
        .route("/v1/capabilities", get(get_capabilities))
        .route(
            "/.well-known/wallet-configuration",
            get(get_wallet_configuration),
        )
        .route("/v1/payments/sign", post(sign_payment))
        .route("/v1/wallet/address", get(get_wallet_address))
        .with_state(state)
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

async fn readyz() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        READYZ_BODY,
    )
}

/// Returns the RAR projection of the active access token (D-12).
///
/// TODO(phase-4-followup): wire the [`AccessTokenVerifier`] stub. Until then
/// the endpoint returns an empty array so consumers can rely on the shape but
/// learn nothing about the current grant. Documented in Proposal-0003 D-12.
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
        "audience_thumbprint": state.policy.audience_thumbprint(),
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
/// Field shape mirrors `zally_core::SignedPayload` for forward compatibility;
/// the runtime serializes the struct directly rather than going through that
/// type so the `format` field can carry `"raw-zcash-v5"` until the PCZT path
/// (`pczt-v1`) lands on both ends. See [`SIGNED_PAYLOAD_FORMAT_WIRE_LITERAL`].
#[derive(Clone, Debug, Serialize)]
struct SignedPayloadWire {
    format: &'static str,
    bytes: String,
    tx_id: String,
    fee: AmountWire,
    expires_at: ExpiresAtWire,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    metadata: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
struct AmountWire {
    currency: &'static str,
    value: String,
    unit: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum ExpiresAtWire {
    BlockHeight(u32),
}

#[derive(Clone, Debug, Serialize)]
struct SignPaymentResponse {
    signed_payload: SignedPayloadWire,
}

/// Handler for `POST /v1/payments/sign`.
///
/// TODO(phase-4-followup): the inbound DPoP proof, the `at+jwt` access token,
/// the audience thumbprint check, the RAR projection, and the single-use
/// `jti` claim all land in the follow-on slice. Phase 4 ships the "sign on
/// demand" core: parse the ZIP-321 URI, build a [`SendPaymentPlan`], and call
/// [`Wallet::send_payment`] with the [`CaptureSubmitter`] so the broadcaster
/// receives the raw bytes without actually broadcasting. The envelope shape
/// the agent sees is identical between Phase 4 and the follow-on slice.
#[allow(
    clippy::too_many_lines,
    reason = "single linear request handler: parse -> validate -> sign -> capture -> envelope; splitting would scatter the flow across helpers"
)]
async fn sign_payment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SignPaymentRequest>,
) -> Result<Json<SignPaymentResponse>, ProblemResponse> {
    // Trust boundary (Proposal-0003 Slice 1): verify the DPoP-bound access
    // token, then re-derive every bound field from the signed RAR before any
    // signing work. A request that does not carry a conformant grant is
    // rejected here, not signed.
    let claims = verify_spend_authorization(&state, &headers)?;
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
        return Err(problem_response(ProblemDetail::not_retryable(
            ProblemKind::IntentMismatch,
            "intent_mismatch",
            "recomputed intent_hash does not match the signed authorization",
        )));
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

    // Phase 4 uses the `payment_id` as the wallet's idempotency key. The
    // follow-on slice replaces this with the access token's `jti` claim so
    // the wallet enforces D-8 (single-use, write-then-sign) against the
    // shared usage ledger; until then the per-call key keeps replays
    // idempotent against the wallet's storage layer.
    // Single-use jti (D-8): an identical replay returns the cached signed
    // payload; the wallet never signs the same access-token jti twice. v1 uses
    // an in-process ledger (single instance); the shared-backend upgrade is a
    // deploy-time change documented in PRD-43.
    let cached = state.spend_ledger.lock().get(&claims.jti).cloned();
    if let Some(cached) = cached {
        return Ok(Json(cached));
    }

    let idempotency = IdempotencyKey::try_from(body.payment_id.as_str()).map_err(|err| {
        ProblemResponse::bad_request(ProblemDetail::not_retryable(
            ProblemKind::PaymentRequestInvalid,
            "invalid payment_id for idempotency",
            err.to_string(),
        ))
    })?;

    let capture = CaptureSubmitter::new(requested_network);
    let target_expiry_height = BlockHeight::from(body.target_expiry_height);
    let plan = SendPaymentPlan::conventional(
        state.account_id,
        idempotency,
        recipient_for_plan,
        amount_for_plan,
        &capture,
    )
    .with_target_expiry_height(target_expiry_height);

    let send_outcome = state
        .wallet
        .send_payment(plan)
        .await
        .map_err(|err| map_wallet_err(&err))?;

    let captured = capture.take_captured().ok_or_else(|| {
        ProblemResponse::server_error(ProblemDetail::not_retryable(
            ProblemKind::NotReady,
            "submitter captured no bytes",
            "internal: CaptureSubmitter::submit was not invoked by the wallet path",
        ))
    })?;

    let expiry_value = send_outcome.signed.tx_expiry_height.as_u32();

    let response = SignPaymentResponse {
        signed_payload: SignedPayloadWire {
            format: SIGNED_PAYLOAD_FORMAT_WIRE_LITERAL,
            bytes: BASE64_STANDARD.encode(&captured),
            tx_id: hex::encode(send_outcome.signed.tx_id.as_bytes()),
            fee: AmountWire {
                currency: "ZEC",
                value: send_outcome.signed.fee_zat.as_u64().to_string(),
                unit: "base",
            },
            expires_at: ExpiresAtWire::BlockHeight(expiry_value),
            metadata: serde_json::Value::Null,
        },
    };
    state
        .spend_ledger
        .lock()
        .insert(claims.jti.clone(), response.clone());
    Ok(Json(response))
}

fn clone_recipient(recipient: &PaymentRecipient) -> PaymentRecipient {
    recipient.clone()
}

/// Verify the DPoP-bound `at+jwt` on `POST /v1/payments/sign` (Slice 1).
///
/// Runs the boundary in order: extract `Authorization: DPoP <at+jwt>` plus the
/// `DPoP` proof, verify the access token against the issuer JWKS with the
/// audience pinned to this wallet's thumbprint, verify the DPoP proof binds to
/// the token's `cnf.jkt` and this request, then reject a replayed proof `jti`.
fn verify_spend_authorization(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AccessTokenClaims, ProblemResponse> {
    let (access_token, proof) = extract_dpop_bearer(headers)?;
    let claims = verify_access_token(
        &access_token,
        &state.jwks,
        state.policy.audience_thumbprint(),
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
    let mut seen = state.dpop_seen.lock();
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
        | ProblemKind::RevocationCacheStale
        | ProblemKind::NotReady => StatusCode::SERVICE_UNAVAILABLE,
        ProblemKind::TargetExpiryMismatchInternal => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
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
            "wallet send_payment failed",
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
        (
            self.status,
            [("content-type", "application/problem+json")],
            json,
        )
            .into_response()
    }
}

#[derive(Debug)]
struct ResolvedConfig {
    bind_addr: SocketAddr,
    network: Network,
    sealed_seed_path: PathBuf,
    storage_path: PathBuf,
    max_amount_zat: u64,
    audience_thumbprint: String,
    jwks_file: Option<PathBuf>,
    public_sign_url: String,
    leeway_seconds: u64,
    indexer_grpc_addr: Option<String>,
    birthday_override: Option<u32>,
}

impl ResolvedConfig {
    fn from_env() -> Result<Self, StartupError> {
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

        let sealed_seed_path = required_path("ZSPEND_SEALED_SEED_PATH")?;
        // The `ZSPEND_AGE_IDENTITY_PATH` env var is consumed by
        // `AgeFileSealing`, which stores the identity sidecar at
        // `<sealed_seed_path>.age-identity`. We read the env var (and log it
        // when set) so operators can pin a non-default identity location for
        // diagnostics, even though the sealing implementation derives the
        // path from the sealed seed location.
        if let Ok(custom_identity) = std::env::var("ZSPEND_AGE_IDENTITY_PATH") {
            tracing::info!(
                custom_identity_path = %custom_identity,
                "ZSPEND_AGE_IDENTITY_PATH is informational: sealing derives identity path from sealed seed path",
            );
        }
        let storage_path = required_path("ZSPEND_STORAGE_PATH")?;

        // Phase 4 pragmatic defaults: the cap and audience thumbprint can be
        // overridden by env so deployments can pin them, but a sensible
        // default keeps the binary runnable in dev without extra wiring.
        let max_amount_zat = std::env::var("ZSPEND_MAX_AMOUNT_ZAT")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(1_000_000_000);
        let audience_thumbprint = std::env::var("ZSPEND_AUDIENCE_THUMBPRINT")
            .unwrap_or_else(|_| "phase4-stub-thumbprint".to_owned());

        let jwks_file = std::env::var("ZSPEND_JWKS_FILE")
            .ok()
            .map(|raw| raw.trim().to_owned())
            .filter(|raw| !raw.is_empty())
            .map(PathBuf::from);
        let public_base = std::env::var("ZSPEND_PUBLIC_URL")
            .ok()
            .map(|raw| raw.trim().to_owned())
            .filter(|raw| !raw.is_empty())
            .unwrap_or_else(|| format!("http://{bind_addr}"));
        let public_sign_url = format!("{}/v1/payments/sign", public_base.trim_end_matches('/'));
        let leeway_seconds = std::env::var("ZSPEND_LEEWAY_SECONDS")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(60);

        let indexer_grpc_addr = std::env::var("ZSPEND_CHAIN_SOURCE_URL")
            .ok()
            .map(|raw| raw.trim().to_owned())
            .filter(|raw| !raw.is_empty());
        let birthday_override = std::env::var("ZSPEND_BIRTHDAY_HEIGHT")
            .ok()
            .and_then(|raw| raw.trim().parse::<u32>().ok());

        Ok(Self {
            bind_addr,
            network,
            sealed_seed_path,
            storage_path,
            max_amount_zat,
            audience_thumbprint,
            jwks_file,
            public_sign_url,
            leeway_seconds,
            indexer_grpc_addr,
            birthday_override,
        })
    }
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
        AmountWire, AppState, ExpiresAtWire, ProblemResponse, READYZ_BODY,
        SIGNED_PAYLOAD_FORMAT_WIRE_LITERAL, SignPaymentResponse, SignedPayloadWire, WireNetwork,
        WireNetworkOut, build_router, parse_network,
    };
    use crate::bootstrap::{BootstrapInputs, ChainSourceFactory, bootstrap};
    use crate::init;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use http_body_util::BodyExt as _;
    use jsonwebtoken::jwk::{Jwk, JwkSet};
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use serde_json::json;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tower::ServiceExt as _;
    use zally_core::Network;
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
    fn signed_payload_serialises_with_expected_fields() -> Result<(), serde_json::Error> {
        let envelope = SignPaymentResponse {
            signed_payload: SignedPayloadWire {
                format: SIGNED_PAYLOAD_FORMAT_WIRE_LITERAL,
                bytes: "AAAA".to_owned(),
                tx_id: "deadbeef".to_owned(),
                fee: AmountWire {
                    currency: "ZEC",
                    value: "0".to_owned(),
                    unit: "base",
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
    fn readyz_body_advertises_phase4_posture() -> Result<(), serde_json::Error> {
        let parsed: serde_json::Value = serde_json::from_str(READYZ_BODY)?;
        assert_eq!(parsed["sealed_seed"], "available");
        assert_eq!(parsed["posture"], "dev");
        assert_eq!(parsed["jwks_cache"], "unused");
        assert_eq!(parsed["revocation_cache"], "unused");
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
        init::run(sealed_seed.clone(), false).await?;

        let mock = Arc::new(MockChainSource::new(network));
        let (wallet, account_id) = bootstrap(BootstrapInputs {
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
            .audience_thumbprint("test-thumbprint")
            .build()?;

        // Hold `dir` for the wallet's lifetime by leaking it: the temp dir
        // lives as long as the test process so the sqlite handle stays valid.
        std::mem::forget(dir);

        Ok(AppState {
            wallet,
            account_id,
            policy: Arc::new(policy),
            jwks: Arc::new(jsonwebtoken::jwk::JwkSet { keys: Vec::new() }),
            public_sign_url: Arc::from("http://127.0.0.1:8090/v1/payments/sign"),
            leeway_seconds: 60,
            dpop_seen: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            spend_ledger: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
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
    const WALLET_AUDIENCE: &str = "test-thumbprint";
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
}
