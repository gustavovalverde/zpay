//! Local gateway for the zpay Zcash x402 demo UI.
//!
//! The gateway is intentionally a demo-only process. It owns local wallet
//! state and demo credentials, exposes browser-safe `/demo/v1/*` routes, and
//! calls the existing zpay and zspend HTTP surfaces on behalf of the UI.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::Stream;
use futures::stream;
use jsonwebtoken::{Algorithm, EncodingKey};
use p256::ecdsa::SigningKey;
use p256::pkcs8::{EncodePrivateKey as _, LineEnding};
use parking_lot::Mutex;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::time::{Instant, interval_at, timeout};
use tracing::{debug, error, warn};
use zally_chain::{ChainSource, ChainSourceError, ZinderChainSource, ZinderRemoteOptions};
#[cfg(test)]
use zally_chain::{SubmitOutcome, SubmitterError};
#[cfg(test)]
use zally_core::TxId;
use zally_core::{AccountId, BlockHeight, Memo, MemoBytes, Network, PaymentRecipient, Zatoshis};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{Sqlite, SqliteOptions};
use zally_wallet::{
    ExportPaymentDisclosurePlan, PaymentDisclosureProfile, PaymentRequest, ProposalPlan,
    SyncDriver, SyncDriverOptions, SyncHandle, Wallet,
};
use zpay_core::prepare::{PROTOCOL_MEMO_BYTE_COUNT, PROTOCOL_MEMO_BYTE_COUNT_NO_EVIDENCE};
use zpay_testkit::{
    AccessTokenError, AccessTokenGrant, DpopError, DpopKey, ResourceInfo, X402PcztPayment,
    X402SettleResponse, X402VerifyResponse, ZspendSignCall, ZspendSignError,
    build_x402_pczt_facilitator_request, mint_access_token, request_zspend_signature,
};
use zspend_core::{
    Amount, AmountUnit, ChainId, ExpiresAt, IntentHashString, PaymentAuthorization,
    PaymentAuthorizationType, recompute_intent_hash,
};

const DEFAULT_BIND_ADDR: &str = "127.0.0.1:7410";
const DEFAULT_ZPAY_URL: &str = "http://127.0.0.1:8080";
const DEFAULT_ZPAY_OPS_URL: &str = "http://127.0.0.1:9295";
const DEFAULT_ZSPEND_URL: &str = "http://127.0.0.1:8090";
const DEFAULT_ZINDER_URL: &str = "http://127.0.0.1:19101";
const DEFAULT_WALLET_DIR: &str = ".tmp/zpay-demo/wallet";
const DEFAULT_ISSUER_KEY_FILE: &str = "dev-issuer-p256.pem";
const DEFAULT_ISSUER_JWKS_FILE: &str = "dev-jwks.json";
const DEFAULT_FAUZEC_URL: &str = "https://fauzec.com";
const DEFAULT_ZEXPLORER_TX_URL: &str = "https://zexplorer.app/testnet/tx";
const DEFAULT_PAYEE_ID: &str = "aether-demo";
const DEFAULT_RESOURCE_URI: &str = "https://zpay.local/demo/reports/aether-brief";
const DEFAULT_ISSUER_KID: &str = "zpay-demo-dev";
const DEFAULT_ZSPEND_AUDIENCE: &str = "urn:zpay:zspend:local-dev";
const DEFAULT_NETWORK_LABEL: &str = "testnet";
const DEFAULT_BIRTHDAY_LOOKBACK_BLOCKS: u32 = 500;
const DEFAULT_TOKEN_TTL_SECONDS: u64 = 120;
const DEFAULT_MIN_FUNDED_ZAT: u64 = 15_000;
const DEMO_FEE_BUFFER_ZAT: u64 = 5_000;
const PROBE_TIMEOUT_SECONDS: u64 = 3;
const PAYMENT_EVENT_SECONDS: u64 = 3;
const WALLET_SYNC_POLL_INTERVAL_MS: u64 = 2_000;
const WALLET_SYNC_TIMEOUT_SECONDS: u64 = 120;

/// Runtime configuration for the local demo gateway.
#[derive(Clone, Debug)]
pub struct DemoConfig {
    bind_addr: SocketAddr,
    zpay_url: String,
    zpay_ops_url: String,
    zspend_url: String,
    zspend_public_url: String,
    zinder_url: String,
    wallet_dir: PathBuf,
    birthday_height: Option<u32>,
    network_label: String,
    network: Network,
    payee_id: String,
    resource_uri: String,
    fauzec_url: String,
    zexplorer_tx_url: String,
    issuer_key_path: PathBuf,
    issuer_jwks_path: PathBuf,
    issuer_kid: String,
    zspend_audience: String,
    token_ttl_seconds: u64,
    min_funded_zat: u64,
}

impl DemoConfig {
    /// Reads `ZPAY_DEMO_*` environment variables and applies local testnet defaults.
    ///
    /// `ZPAY_DEMO_NETWORK=mainnet` is rejected. This process is a demo gateway, not a
    /// production wallet runtime.
    pub fn from_env() -> Result<Self, DemoError> {
        let network_label = env_string("ZPAY_DEMO_NETWORK", DEFAULT_NETWORK_LABEL);
        let network = parse_demo_network(&network_label)?;
        let zspend_url = env_string("ZPAY_DEMO_ZSPEND_URL", DEFAULT_ZSPEND_URL);
        let wallet_dir = PathBuf::from(env_string("ZPAY_DEMO_WALLET_DIR", DEFAULT_WALLET_DIR));
        let issuer_key_path = env_path("ZPAY_DEMO_ISSUER_KEY_PATH")
            .unwrap_or_else(|| wallet_dir.join(DEFAULT_ISSUER_KEY_FILE));
        let issuer_jwks_path = env_path("ZPAY_DEMO_ISSUER_JWKS_PATH")
            .unwrap_or_else(|| wallet_dir.join(DEFAULT_ISSUER_JWKS_FILE));
        Ok(Self {
            bind_addr: env_socket_addr("ZPAY_DEMO_BIND_ADDR", DEFAULT_BIND_ADDR)?,
            zpay_url: env_string("ZPAY_DEMO_ZPAY_URL", DEFAULT_ZPAY_URL),
            zpay_ops_url: env_string("ZPAY_DEMO_ZPAY_OPS_URL", DEFAULT_ZPAY_OPS_URL),
            zspend_public_url: env_string("ZPAY_DEMO_ZSPEND_PUBLIC_URL", &zspend_url),
            zspend_url,
            zinder_url: env_string("ZPAY_DEMO_ZINDER_URL", DEFAULT_ZINDER_URL),
            wallet_dir,
            birthday_height: env_optional_u32("ZPAY_DEMO_BIRTHDAY_HEIGHT")?,
            network_label,
            network,
            payee_id: env_string("ZPAY_DEMO_PAYEE_ID", DEFAULT_PAYEE_ID),
            resource_uri: env_string("ZPAY_DEMO_RESOURCE_URI", DEFAULT_RESOURCE_URI),
            fauzec_url: env_string("ZPAY_DEMO_FAUZEC_URL", DEFAULT_FAUZEC_URL),
            zexplorer_tx_url: env_string("ZPAY_DEMO_ZEXPLORER_TX_URL", DEFAULT_ZEXPLORER_TX_URL),
            issuer_key_path,
            issuer_jwks_path,
            issuer_kid: env_string("ZPAY_DEMO_ISSUER_KID", DEFAULT_ISSUER_KID),
            zspend_audience: env_string("ZPAY_DEMO_ZSPEND_AUDIENCE", DEFAULT_ZSPEND_AUDIENCE),
            token_ttl_seconds: env_u64("ZPAY_DEMO_TOKEN_TTL_SECONDS", DEFAULT_TOKEN_TTL_SECONDS)?,
            min_funded_zat: env_u64("ZPAY_DEMO_MIN_FUNDED_ZAT", DEFAULT_MIN_FUNDED_ZAT)?,
        })
    }
}

/// Error type returned by the demo gateway.
#[derive(Debug, Clone)]
pub struct DemoError {
    status: StatusCode,
    title: &'static str,
    kind: &'static str,
    detail: String,
    retryable: bool,
}

impl DemoError {
    fn new(
        status: StatusCode,
        title: &'static str,
        kind: &'static str,
        detail: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            status,
            title,
            kind,
            detail: detail.into(),
            retryable,
        }
    }

    fn config(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid configuration",
            "demo_config_invalid",
            detail,
            false,
        )
    }

    fn unavailable(kind: &'static str, detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "Dependency unavailable",
            kind,
            detail,
            true,
        )
    }

    fn not_found(kind: &'static str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "Not found", kind, detail, false)
    }

    fn rejected(kind: &'static str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "Rejected", kind, detail, false)
    }
}

impl std::fmt::Display for DemoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.detail)
    }
}

impl std::error::Error for DemoError {}

impl IntoResponse for DemoError {
    fn into_response(self) -> Response {
        let status = self.status;
        let body = ProblemBody {
            title: self.title,
            kind: self.kind,
            detail: self.detail,
            retryable: self.retryable,
        };
        (status, Json(body)).into_response()
    }
}

impl From<zally_wallet::WalletError> for DemoError {
    fn from(err: zally_wallet::WalletError) -> Self {
        Self::unavailable("wallet_unavailable", err.to_string())
    }
}

impl From<ChainSourceError> for DemoError {
    fn from(err: ChainSourceError) -> Self {
        Self::unavailable("zinder_unavailable", err.to_string())
    }
}

#[derive(Serialize)]
struct ProblemBody {
    title: &'static str,
    kind: &'static str,
    detail: String,
    retryable: bool,
}

#[derive(Clone)]
struct DemoState {
    config: DemoConfig,
    client: reqwest::Client,
    wallet: Arc<Wallet>,
    account_id: AccountId,
    chain: Arc<ZinderChainSource>,
    _sync_handle: Arc<SyncHandle>,
    payments: Arc<Mutex<HashMap<String, StoredPayment>>>,
}

#[derive(Clone)]
struct StoredPayment {
    prepared: PreparedPayment,
    mode: PaymentMode,
    stage_override: Option<DemoStage>,
    settled_transaction_id: Option<String>,
    disclosure: Option<StoredDisclosure>,
}

#[derive(Clone)]
struct StoredDisclosure {
    payload_hex: String,
    message_hex: String,
}

struct SettlementArtifacts {
    transaction_id: String,
    disclosure: Option<StoredDisclosure>,
}

/// Starts the local demo gateway.
pub async fn serve(config: DemoConfig) -> Result<(), DemoError> {
    let bind_addr = config.bind_addr;
    let state = DemoState::open(config).await?;
    let listener = TcpListener::bind(bind_addr)
        .await
        .map_err(|err| DemoError::unavailable("bind_failed", err.to_string()))?;
    debug!(%bind_addr, "zpay demo gateway listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|err| DemoError::unavailable("serve_failed", err.to_string()))
}

fn router(state: DemoState) -> Router {
    Router::new()
        .route("/demo/v1/readiness", get(readiness_route))
        .route("/demo/v1/wallet", get(wallet_route))
        .route("/demo/v1/faucet-claims", post(create_faucet_claim_route))
        .route(
            "/demo/v1/faucet-claims/{request_id}",
            get(faucet_claim_route),
        )
        .route(
            "/demo/v1/payments",
            get(payments_route).post(create_payment_route),
        )
        .route("/demo/v1/payments/{payment_id}", get(payment_route))
        .route(
            "/demo/v1/payments/{payment_id}/settle",
            post(settle_payment_route),
        )
        .route(
            "/demo/v1/payments/{payment_id}/events",
            get(payment_events_route),
        )
        .route(
            "/demo/v1/payments/{payment_id}/verify",
            post(verify_payment_route),
        )
        .route("/demo/v1/console/payments", get(console_payments_route))
        .with_state(state)
}

impl DemoState {
    async fn open(config: DemoConfig) -> Result<Self, DemoError> {
        std::fs::create_dir_all(&config.wallet_dir)
            .map_err(|err| DemoError::unavailable("wallet_dir_unavailable", err.to_string()))?;
        create_issuer_key_if_missing(&config)?;
        let chain = Arc::new(ZinderChainSource::connect_remote(ZinderRemoteOptions {
            endpoint: config.zinder_url.clone(),
            network: config.network,
        })?);
        let birthday_height =
            resolve_birthday_height(chain.as_ref(), config.birthday_height).await?;
        let (wallet, account_id) = open_or_bootstrap_wallet(
            config.network,
            &config.wallet_dir,
            chain.as_ref(),
            BlockHeight::from(birthday_height),
        )
        .await?;
        let chain_for_sync: Arc<dyn ChainSource> = chain.clone();
        let sync_handle = SyncDriver::new(
            wallet.clone(),
            chain_for_sync,
            SyncDriverOptions::default()
                .with_poll_interval_ms(WALLET_SYNC_POLL_INTERVAL_MS)
                .with_sync_timeout_seconds(WALLET_SYNC_TIMEOUT_SECONDS),
        )?
        .sync_continuously();
        Ok(Self {
            config,
            client: reqwest::Client::new(),
            wallet: Arc::new(wallet),
            account_id,
            chain,
            _sync_handle: Arc::new(sync_handle),
            payments: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn stored_payment(&self, payment_id: &str) -> Result<StoredPayment, DemoError> {
        self.payments
            .lock()
            .get(payment_id)
            .cloned()
            .ok_or_else(|| DemoError::not_found("payment_unknown", "This payment is unknown"))
    }

    fn payment_ids(&self) -> Vec<String> {
        self.payments.lock().keys().cloned().collect()
    }

    fn set_stage_override(&self, payment_id: &str, stage: Option<DemoStage>) {
        if let Some(stored) = self.payments.lock().get_mut(payment_id) {
            stored.stage_override = stage;
        }
    }

    fn record_settlement(&self, payment_id: &str, artifacts: SettlementArtifacts) {
        if let Some(stored) = self.payments.lock().get_mut(payment_id) {
            stored.settled_transaction_id = Some(artifacts.transaction_id);
            stored.disclosure = artifacts.disclosure;
            stored.stage_override = None;
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            let _ = signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

async fn readiness_route(State(state): State<DemoState>) -> Result<Json<ReadinessBody>, DemoError> {
    Ok(Json(readiness_snapshot(&state).await))
}

async fn wallet_route(State(state): State<DemoState>) -> Result<Json<WalletBody>, DemoError> {
    wallet_body(&state).await.map(Json)
}

async fn create_faucet_claim_route(
    State(state): State<DemoState>,
    Json(body): Json<CreateFaucetClaimBody>,
) -> Result<Json<FaucetClaimBody>, DemoError> {
    let address = match body.address {
        Some(address) if !address.trim().is_empty() => address,
        _ => wallet_address(&state).await?,
    };
    let claim_url = format!(
        "{}/api/v1/claim",
        state.config.fauzec_url.trim_end_matches('/')
    );
    let response = state
        .client
        .post(claim_url)
        .json(&serde_json::json!({
            "network": state.config.network_label,
            "address": address,
            "memo": "zpay demo UI",
        }))
        .send()
        .await
        .map_err(|err| DemoError::unavailable("faucet_unavailable", err.to_string()))?;
    parse_faucet_response(response).await.map(Json)
}

async fn faucet_claim_route(
    State(state): State<DemoState>,
    AxumPath(request_id): AxumPath<String>,
) -> Result<Json<FaucetClaimBody>, DemoError> {
    let status_url = format!(
        "{}/api/v1/status/{}/{}",
        state.config.fauzec_url.trim_end_matches('/'),
        state.config.network_label,
        request_id
    );
    let response = state
        .client
        .get(status_url)
        .send()
        .await
        .map_err(|err| DemoError::unavailable("faucet_unavailable", err.to_string()))?;
    parse_faucet_response(response).await.map(Json)
}

async fn create_payment_route(
    State(state): State<DemoState>,
    Json(body): Json<CreatePaymentBody>,
) -> Result<Json<PaymentBody>, DemoError> {
    let wallet = wallet_body(&state).await?;
    if !wallet.is_funded {
        return Err(DemoError::rejected(
            "wallet_needs_funds",
            "The demo wallet needs testnet funds",
        ));
    }

    let dpop_key = Arc::new(DpopKey::generate().map_err(|error| dpop_key_invalid(&error))?);
    let prepared = call_prepare(&state, &dpop_key).await?;
    let payment_id = prepared.payment_id.clone();
    let stored = StoredPayment {
        prepared: prepared.clone(),
        mode: body.mode,
        stage_override: Some(DemoStage::Review),
        settled_transaction_id: None,
        disclosure: None,
    };
    state.payments.lock().insert(payment_id.clone(), stored);
    payment_snapshot(&state, &payment_id).await.map(Json)
}

async fn payments_route(
    State(state): State<DemoState>,
) -> Result<Json<Vec<PaymentBody>>, DemoError> {
    let ids = payment_ids_most_recent_first(state.payment_ids());
    let mut payments = Vec::with_capacity(ids.len());
    for payment_id in ids {
        payments.push(payment_snapshot(&state, &payment_id).await?);
    }
    Ok(Json(payments))
}

/// `payment_id` is a ULID (lexicographically time-sortable), so a plain descending
/// string sort orders payments most-recent-first without a separate timestamp column.
fn payment_ids_most_recent_first(mut ids: Vec<String>) -> Vec<String> {
    ids.sort_by(|a, b| b.cmp(a));
    ids
}

async fn payment_route(
    State(state): State<DemoState>,
    AxumPath(payment_id): AxumPath<String>,
) -> Result<Json<PaymentBody>, DemoError> {
    payment_snapshot(&state, &payment_id).await.map(Json)
}

async fn settle_payment_route(
    State(state): State<DemoState>,
    AxumPath(payment_id): AxumPath<String>,
) -> Result<Json<PaymentBody>, DemoError> {
    let stored = state.stored_payment(&payment_id)?;
    let artifacts = match stored.mode {
        PaymentMode::Checkout => settle_checkout(&state, &payment_id, &stored).await?,
        PaymentMode::Autopay => SettlementArtifacts {
            transaction_id: settle_autopay(&state, &payment_id, &stored).await?,
            disclosure: None,
        },
    };
    state.record_settlement(&payment_id, artifacts);
    payment_snapshot(&state, &payment_id).await.map(Json)
}

async fn payment_events_route(
    State(state): State<DemoState>,
    AxumPath(payment_id): AxumPath<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, DemoError> {
    let _ = state.stored_payment(&payment_id)?;
    let stream = stream::unfold(
        (
            state,
            payment_id,
            interval_at(Instant::now(), Duration::from_secs(PAYMENT_EVENT_SECONDS)),
            false,
        ),
        |(state, payment_id, mut ticker, is_done)| async move {
            if is_done {
                return None;
            }
            ticker.tick().await;
            let snapshot = match payment_snapshot(&state, &payment_id).await {
                Ok(snapshot) => snapshot,
                Err(err) => PaymentBody::error(payment_id.clone(), err.detail),
            };
            let close_after = snapshot.stage.is_terminal();
            let event = Event::default()
                .event("snapshot")
                .json_data(&snapshot)
                .unwrap_or_else(|err| {
                    error!(%payment_id, error = %err, "demo SSE serialization failed");
                    Event::default()
                        .event("serialization_failed")
                        .data(payment_id.clone())
                });
            Some((Ok(event), (state, payment_id, ticker, close_after)))
        },
    );
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn readiness_snapshot(state: &DemoState) -> ReadinessBody {
    let zpay = probe_http(
        &state.client,
        &format!("{}/readyz", state.config.zpay_ops_url),
    )
    .await;
    let zspend = probe_http(
        &state.client,
        &format!("{}/readyz", state.config.zspend_url),
    )
    .await;
    let faucet = probe_http(&state.client, &state.config.fauzec_url).await;
    let zinder = match timeout(
        Duration::from_secs(PROBE_TIMEOUT_SECONDS),
        state.chain.chain_tip(),
    )
    .await
    {
        Ok(Ok(height)) => DependencyBody::ready_with_height("ready", Some(height.as_u32())),
        Ok(Err(err)) => DependencyBody::not_ready("zinder_unavailable", err.to_string(), true),
        Err(_) => DependencyBody::not_ready("zinder_timeout", "zinder probe timed out", true),
    };
    let wallet = match wallet_body(state).await {
        Ok(wallet) => DependencyBody {
            status: if wallet.is_funded {
                "ready"
            } else {
                "needs_funds"
            }
            .to_owned(),
            kind: None,
            detail: Some(if wallet.is_funded {
                "Demo wallet has spendable funds".to_owned()
            } else {
                "The demo wallet needs testnet funds".to_owned()
            }),
            retryable: true,
            height: wallet.as_of_height,
        },
        Err(err) => DependencyBody::not_ready("wallet_unavailable", err.detail, true),
    };
    ReadinessBody {
        network: state.config.network_label.clone(),
        zpay,
        zspend,
        zinder,
        wallet,
        faucet,
    }
}

async fn probe_http(client: &reqwest::Client, url: &str) -> DependencyBody {
    match timeout(
        Duration::from_secs(PROBE_TIMEOUT_SECONDS),
        client.get(url).send(),
    )
    .await
    {
        Ok(Ok(response)) if response.status().is_success() => DependencyBody::ready("ready"),
        Ok(Ok(response)) => DependencyBody::not_ready(
            "http_not_ready",
            format!("HTTP {}", response.status().as_u16()),
            true,
        ),
        Ok(Err(err)) => DependencyBody::not_ready("http_unavailable", err.to_string(), true),
        Err(_) => DependencyBody::not_ready("http_timeout", "HTTP probe timed out", true),
    }
}

async fn wallet_body(state: &DemoState) -> Result<WalletBody, DemoError> {
    let address = wallet_address(state).await?;
    let balance = state.wallet.get_account_balance(state.account_id).await?;
    let total_zat = balance.total_zat().as_u64();
    Ok(WalletBody {
        network: state.config.network_label.clone(),
        address,
        sapling_zat: balance.sapling_zat.as_u64(),
        orchard_zat: balance.orchard_zat.as_u64(),
        ironwood_zat: balance.ironwood_zat.as_u64(),
        transparent_mature_zat: balance.transparent_mature_zat.as_u64(),
        transparent_immature_zat: balance.transparent_immature_zat.as_u64(),
        total_zat,
        is_funded: total_zat >= state.config.min_funded_zat,
        as_of_height: balance.as_of_height.map(BlockHeight::as_u32),
    })
}

async fn wallet_address(state: &DemoState) -> Result<String, DemoError> {
    let params = state.config.network.to_parameters();
    let exposed = state
        .wallet
        .list_exposed_addresses(state.account_id)
        .await?;
    if let Some(address) = exposed
        .iter()
        .find(|address| address.has_transparent_receiver)
        .or_else(|| exposed.first())
    {
        return Ok(address.unified_address.encode(&params));
    }
    let address = state
        .wallet
        .derive_next_address_with_transparent(state.account_id)
        .await?;
    Ok(address.encode(&params))
}

async fn call_prepare(state: &DemoState, dpop_key: &DpopKey) -> Result<PreparedPayment, DemoError> {
    let idempotency_key = format!("zpay-demo-{}", unix_now_ms());
    let body = PrepareRequestBody {
        payee_id: state.config.payee_id.clone(),
        network: state.config.network_label.clone(),
        scheme: "zcash".to_owned(),
        resource_uri: state.config.resource_uri.clone(),
        nonce: idempotency_key.clone(),
        evidence_pack_hash: None,
        idempotency_key: Some(idempotency_key),
    };
    let prepare_url = format!(
        "{}/zpay/v1/prepare",
        state.config.zpay_url.trim_end_matches('/')
    );
    let dpop_proof = dpop_key
        .mint_proof("POST", &prepare_url, "zpay-demo-dpop")
        .map_err(|error| dpop_proof_invalid(&error))?;
    let response = state
        .client
        .post(prepare_url)
        .header("dpop", dpop_proof)
        .json(&body)
        .send()
        .await
        .map_err(|err| DemoError::unavailable("zpay_unavailable", err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body_text = response.text().await.unwrap_or_default();
        return Err(DemoError::unavailable(
            "prepare_failed",
            format!("zpay /prepare returned {status}: {body_text}"),
        ));
    }
    response
        .json()
        .await
        .map_err(|err| DemoError::unavailable("prepare_malformed", err.to_string()))
}

async fn verify_payment_route(
    State(state): State<DemoState>,
    AxumPath(payment_id): AxumPath<String>,
) -> Result<Json<DisclosureVerifyResponseBody>, DemoError> {
    let stored = state.stored_payment(&payment_id)?;
    let transaction_id = stored.settled_transaction_id.ok_or_else(|| {
        DemoError::rejected(
            "disclosure_unavailable",
            "The payment hasn't been broadcast by this wallet",
        )
    })?;
    let disclosure = stored.disclosure.ok_or_else(|| {
        DemoError::rejected(
            "disclosure_unavailable",
            "The signing wallet didn't return a payment disclosure",
        )
    })?;
    let payment = payment_request(&stored.prepared, state.config.network)?;
    let body = DisclosureVerifyRequestBody {
        txid: transaction_id,
        expected_amount_zat: payment.amount.as_u64(),
        expected_pay_to: payment.recipient.encoded().to_owned(),
        expected_disclosure_message_hex: disclosure.message_hex,
        disclosure_payload_hex: disclosure.payload_hex,
    };
    call_verify(&state, body).await.map(Json)
}

async fn call_verify(
    state: &DemoState,
    body: DisclosureVerifyRequestBody,
) -> Result<DisclosureVerifyResponseBody, DemoError> {
    let verify_url = format!(
        "{}/zpay/v1/verify",
        state.config.zpay_url.trim_end_matches('/')
    );
    let response = state
        .client
        .post(verify_url)
        .json(&body)
        .send()
        .await
        .map_err(|err| DemoError::unavailable("zpay_unavailable", err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body_text = response.text().await.unwrap_or_default();
        return Err(DemoError::unavailable(
            "disclosure_verify_failed",
            format!("zpay /verify returned {status}: {body_text}"),
        ));
    }
    response
        .json()
        .await
        .map_err(|err| DemoError::unavailable("disclosure_verify_malformed", err.to_string()))
}

async fn console_payments_route(
    State(state): State<DemoState>,
) -> Result<Json<ConsolePaymentsBody>, DemoError> {
    call_console_payments(&state).await.map(Json)
}

async fn call_console_payments(state: &DemoState) -> Result<ConsolePaymentsBody, DemoError> {
    let url = format!(
        "{}/payments",
        state.config.zpay_ops_url.trim_end_matches('/')
    );
    let response = state
        .client
        .get(url)
        .send()
        .await
        .map_err(|err| DemoError::unavailable("zpay_unavailable", err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body_text = response.text().await.unwrap_or_default();
        return Err(DemoError::unavailable(
            "console_payments_failed",
            format!("zpay ops /payments returned {status}: {body_text}"),
        ));
    }
    response
        .json()
        .await
        .map_err(|err| DemoError::unavailable("console_payments_malformed", err.to_string()))
}

async fn settle_checkout(
    state: &DemoState,
    payment_id: &str,
    stored: &StoredPayment,
) -> Result<SettlementArtifacts, DemoError> {
    state.set_stage_override(payment_id, Some(DemoStage::Signing));
    let balance = state.wallet.get_account_balance(state.account_id).await?;
    let parsed = payment_request(&stored.prepared, state.config.network)?;
    let requested_zat = parsed.amount.as_u64();
    let spendable_zat = balance.total_zat().as_u64();
    if spendable_zat < requested_zat.saturating_add(DEMO_FEE_BUFFER_ZAT) {
        state.set_stage_override(payment_id, Some(DemoStage::NeedsFunds));
        return Err(DemoError::rejected(
            "wallet_needs_funds",
            format!(
                "The demo wallet needs testnet funds: spendable={spendable_zat}, requested={requested_zat}"
            ),
        ));
    }

    state.set_stage_override(payment_id, Some(DemoStage::Settling));
    let memo = memo_from_protocol_prefix(&stored.prepared.memo_bytes)?;
    let recipient = parsed.recipient;
    let plan = ProposalPlan::conventional(
        state.account_id,
        recipient.clone(),
        parsed.amount,
        Some(memo),
    );
    let unsigned_pczt = state
        .wallet
        .propose_pczt(plan, Some(BlockHeight::from(stored.prepared.expiry_height)))
        .await?;
    let proven_pczt = state.wallet.prove_pczt(unsigned_pczt).await?;
    let signed_pczt = state.wallet.sign_pczt(proven_pczt).await?;
    let wallet_transaction_id = state.wallet.extract_pczt(signed_pczt.clone()).await?;
    let pczt_base64 = URL_SAFE_NO_PAD.encode(signed_pczt.as_bytes());
    let transaction_id = settle_x402_pczt(state, &stored.prepared, &pczt_base64).await?;
    if transaction_id != wallet_transaction_id.to_rpc_hex() {
        return Err(DemoError::rejected(
            "settle_txid_mismatch",
            format!(
                "x402 settle returned {transaction_id} but the demo wallet extracted {}",
                wallet_transaction_id.to_rpc_hex()
            ),
        ));
    }
    let message = disclosure_message(payment_id);
    let disclosure_profile = payment_disclosure_profile(&recipient)?;
    let disclosure = state
        .wallet
        .export_payment_disclosure(ExportPaymentDisclosurePlan::new(
            wallet_transaction_id,
            recipient,
            parsed.amount,
            message.clone(),
            disclosure_profile,
        ))
        .await?;
    Ok(SettlementArtifacts {
        transaction_id,
        disclosure: Some(StoredDisclosure {
            payload_hex: hex::encode(disclosure.to_bytes()),
            message_hex: hex::encode(message),
        }),
    })
}

async fn settle_autopay(
    state: &DemoState,
    payment_id: &str,
    stored: &StoredPayment,
) -> Result<String, DemoError> {
    state.set_stage_override(payment_id, Some(DemoStage::Signing));
    let authorization = build_agent_authorization(&stored.prepared, state.config.network)?;
    let issuer_key = load_or_create_issuer_encoding_key(&state.config)?;
    let dpop_key = DpopKey::generate().map_err(|error| dpop_key_invalid(&error))?;
    let access_token = mint_access_token(&AccessTokenGrant {
        issuer_key: &issuer_key.encoding,
        issuer_algorithm: issuer_key.algorithm,
        issuer_kid: &state.config.issuer_kid,
        audience: &state.config.zspend_audience,
        dpop_jkt: dpop_key.jkt(),
        authorization: &authorization,
        token_ttl_seconds: state.config.token_ttl_seconds,
        jti_prefix: "zpay-demo-at",
    })
    .map_err(|error| access_token_invalid(&error))?;
    let sign_public_url = format!(
        "{}/v1/payments/sign",
        state.config.zspend_public_url.trim_end_matches('/')
    );
    let call_sign_url = format!(
        "{}/v1/payments/sign",
        state.config.zspend_url.trim_end_matches('/')
    );
    let dpop_proof = dpop_key
        .mint_access_bound_proof(&access_token, "POST", &sign_public_url, "zpay-demo-dpop")
        .map_err(|error| dpop_proof_invalid(&error))?;
    let signed = request_zspend_signature(
        &state.client,
        ZspendSignCall {
            call_sign_url: &call_sign_url,
            access_token: &access_token,
            dpop_proof: &dpop_proof,
            payment_uri: &stored.prepared.payment_uri,
            network_label: &state.config.network_label,
            payment_id: &stored.prepared.payment_id,
            target_expiry_height: stored.prepared.expiry_height,
        },
    )
    .await
    .map_err(zspend_sign_error)?;
    debug!(
        tx_id = %signed.tx_id,
        pczt_bytes = signed.pczt_byte_count,
        "zspend returned signed PCZT",
    );

    state.set_stage_override(payment_id, Some(DemoStage::Settling));
    let transaction_id = settle_x402_pczt(state, &stored.prepared, &signed.pczt_base64).await?;
    if transaction_id != signed.tx_id {
        return Err(DemoError::rejected(
            "settle_txid_mismatch",
            format!(
                "x402 settle returned {transaction_id} but zspend signed {}",
                signed.tx_id
            ),
        ));
    }
    Ok(transaction_id)
}

async fn payment_snapshot(state: &DemoState, payment_id: &str) -> Result<PaymentBody, DemoError> {
    let stored = state.stored_payment(payment_id)?;
    let status = fetch_payment_status(state, payment_id).await.ok();
    let ui_stage = stored
        .stage_override
        .or_else(|| status.as_ref().map(stage_from_status))
        .unwrap_or(DemoStage::Review);
    let tx_id = status
        .as_ref()
        .and_then(|snapshot| snapshot.broadcast_outcome.as_ref())
        .and_then(|outcome| outcome.transaction_id.clone())
        .or(stored.settled_transaction_id);
    let zexplorer_url = tx_id.as_ref().map(|tx_id| {
        format!(
            "{}/{}",
            state.config.zexplorer_tx_url.trim_end_matches('/'),
            tx_id
        )
    });
    Ok(PaymentBody {
        payment_id: payment_id.to_owned(),
        mode: stored.mode,
        stage: ui_stage,
        amount_zat: stored.prepared.amount_zat,
        expiry_height: stored.prepared.expiry_height,
        status: status.as_ref().map(|snapshot| snapshot.status.clone()),
        confirmation_count: status
            .as_ref()
            .and_then(|snapshot| snapshot.confirmation_count),
        mined_block_height: status
            .as_ref()
            .and_then(|snapshot| snapshot.mined_block_height),
        reorg_count: status.as_ref().map_or(0, |snapshot| snapshot.reorg_count),
        settled: is_settled(status.as_ref()),
        transaction_id: tx_id,
        zexplorer_url,
        can_settle: matches!(ui_stage, DemoStage::Review),
        message: ui_stage.message().to_owned(),
    })
}

fn is_settled(status: Option<&PaymentStatusBody>) -> bool {
    status.is_some_and(|snapshot| snapshot.settled)
}

async fn fetch_payment_status(
    state: &DemoState,
    payment_id: &str,
) -> Result<PaymentStatusBody, DemoError> {
    let url = format!(
        "{}/zpay/v1/payments/{}",
        state.config.zpay_url.trim_end_matches('/'),
        payment_id
    );
    let response = state
        .client
        .get(url)
        .send()
        .await
        .map_err(|err| DemoError::unavailable("zpay_unavailable", err.to_string()))?;
    if !response.status().is_success() {
        return Err(DemoError::unavailable(
            "payment_status_unavailable",
            format!("zpay status returned HTTP {}", response.status().as_u16()),
        ));
    }
    response
        .json()
        .await
        .map_err(|err| DemoError::unavailable("payment_status_malformed", err.to_string()))
}

fn stage_from_status(status: &PaymentStatusBody) -> DemoStage {
    match status.status.as_str() {
        "awaiting" => DemoStage::Review,
        "mined" => DemoStage::Mined,
        "final" if status.settled => DemoStage::Paid,
        "final" => DemoStage::Final,
        "expired" => DemoStage::Expired,
        "failed" | "never_issued" => DemoStage::Failed,
        _ => DemoStage::Confirming,
    }
}

fn payment_request(
    prepared: &PreparedPayment,
    network: Network,
) -> Result<ParsedDemoPayment, DemoError> {
    let parsed = PaymentRequest::from_uri(&prepared.payment_uri, network)?;
    let payment = parsed.payments().first().ok_or_else(|| {
        DemoError::rejected("payment_request_empty", "Prepared URI carries no payment")
    })?;
    Ok(ParsedDemoPayment {
        recipient: payment.recipient.clone(),
        amount: payment.amount,
    })
}

fn payment_disclosure_profile(
    recipient: &PaymentRecipient,
) -> Result<PaymentDisclosureProfile, DemoError> {
    match recipient {
        PaymentRecipient::SaplingAddress { .. } => Ok(PaymentDisclosureProfile::Zip311Draft1),
        PaymentRecipient::UnifiedAddress { .. } => Ok(PaymentDisclosureProfile::ZallyIronwood),
        PaymentRecipient::TransparentAddress { .. } | PaymentRecipient::TexAddress { .. } | _ => {
            Err(DemoError::rejected(
                "disclosure_profile_unsupported",
                "payment recipient has no supported disclosure profile",
            ))
        }
    }
}

fn memo_from_protocol_prefix(memo_bytes: &[u8]) -> Result<Memo, DemoError> {
    let len = memo_bytes.len();
    if len != PROTOCOL_MEMO_BYTE_COUNT_NO_EVIDENCE && len != PROTOCOL_MEMO_BYTE_COUNT {
        return Err(DemoError::rejected(
            "memo_length_invalid",
            format!("prepared memo length was {len}"),
        ));
    }
    let mut buf = [0u8; 512];
    buf[..len].copy_from_slice(memo_bytes);
    let memo_bytes = MemoBytes::from_bytes(&buf)
        .map_err(|err| DemoError::rejected("memo_invalid", err.to_string()))?;
    Memo::try_from(&memo_bytes).map_err(|err| DemoError::rejected("memo_invalid", err.to_string()))
}

async fn settle_x402_pczt(
    state: &DemoState,
    prepared: &PreparedPayment,
    pczt_base64: &str,
) -> Result<String, DemoError> {
    let request = build_x402_facilitator_request(state, prepared, pczt_base64)?;
    verify_x402_pczt(state, &request).await?;
    let settle_url = format!(
        "{}/x402/v2/settle",
        state.config.zpay_url.trim_end_matches('/')
    );
    let response = state
        .client
        .post(settle_url)
        .json(&request)
        .send()
        .await
        .map_err(|err| DemoError::unavailable("zpay_unavailable", err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body_text = response.text().await.unwrap_or_default();
        return Err(DemoError::unavailable(
            "settle_failed",
            format!("zpay /x402/v2/settle returned {status}: {body_text}"),
        ));
    }
    let settle: X402SettleResponse = response
        .json()
        .await
        .map_err(|err| DemoError::unavailable("settle_malformed", err.to_string()))?;
    if !settle.success {
        return Err(DemoError::rejected(
            "settle_failed",
            settle.error_reason.unwrap_or_else(|| "unknown".to_owned()),
        ));
    }
    settle.transaction.ok_or_else(|| {
        DemoError::rejected(
            "settle_malformed",
            "x402 settle succeeded without transaction id",
        )
    })
}

async fn verify_x402_pczt(
    state: &DemoState,
    request: &zpay_testkit::FacilitatorRequest,
) -> Result<(), DemoError> {
    let verify_url = format!(
        "{}/x402/v2/verify",
        state.config.zpay_url.trim_end_matches('/')
    );
    let response = state
        .client
        .post(verify_url)
        .json(request)
        .send()
        .await
        .map_err(|err| DemoError::unavailable("zpay_unavailable", err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body_text = response.text().await.unwrap_or_default();
        return Err(DemoError::unavailable(
            "verify_failed",
            format!("zpay /x402/v2/verify returned {status}: {body_text}"),
        ));
    }
    let verify: X402VerifyResponse = response
        .json()
        .await
        .map_err(|err| DemoError::unavailable("verify_malformed", err.to_string()))?;
    if !verify.is_valid {
        return Err(DemoError::rejected(
            "verify_failed",
            verify
                .invalid_reason
                .unwrap_or_else(|| "unknown".to_owned()),
        ));
    }
    Ok(())
}

fn build_x402_facilitator_request(
    state: &DemoState,
    prepared: &PreparedPayment,
    pczt_base64: &str,
) -> Result<zpay_testkit::FacilitatorRequest, DemoError> {
    let parsed = payment_request(prepared, state.config.network)?;
    let recipient = parsed.recipient.encoded();
    let resource = ResourceInfo {
        url: state.config.resource_uri.clone(),
        description: Some("zpay demo resource".to_owned()),
        mime_type: Some("application/json".to_owned()),
        service_name: Some("zpay-demo".to_owned()),
        tags: vec!["demo".to_owned(), "zcash".to_owned()],
        icon_url: None,
    };
    Ok(build_x402_pczt_facilitator_request(X402PcztPayment {
        network: state.config.network,
        recipient,
        amount_zat: parsed.amount.as_u64(),
        payment_timeout_seconds: state.config.token_ttl_seconds,
        payment_id: &prepared.payment_id,
        resource: &resource,
        pczt_base64,
    }))
}

fn dpop_key_invalid(error: &DpopError) -> DemoError {
    DemoError::rejected("dpop_key_invalid", error.to_string())
}

fn dpop_proof_invalid(error: &DpopError) -> DemoError {
    DemoError::rejected("dpop_proof_invalid", error.to_string())
}

fn access_token_invalid(error: &AccessTokenError) -> DemoError {
    DemoError::rejected("access_token_invalid", error.to_string())
}

fn zspend_sign_error(error: ZspendSignError) -> DemoError {
    match error {
        ZspendSignError::Request { reason } => DemoError::unavailable("zspend_unavailable", reason),
        ZspendSignError::Rejected { status, body } => DemoError::unavailable(
            "zspend_sign_failed",
            format!(
                "zspend /v1/payments/sign returned {}: {body}",
                status.as_u16()
            ),
        ),
        ZspendSignError::ResponseMalformed { reason } => {
            DemoError::unavailable("zspend_sign_malformed", reason)
        }
        ZspendSignError::SignedFormat { format } => DemoError::rejected(
            "signed_format_invalid",
            format!("expected pczt-v2-extractable, got {format}"),
        ),
        ZspendSignError::SignedBytes { reason } => {
            DemoError::rejected("signed_bytes_invalid", reason)
        }
        other => DemoError::unavailable("zspend_sign_malformed", other.to_string()),
    }
}

fn build_agent_authorization(
    prepared: &PreparedPayment,
    network: Network,
) -> Result<PaymentAuthorization, DemoError> {
    let parsed = payment_request(prepared, network)?;
    let reference = chain_reference(network);
    let recipient_caip10 = format!("zcash:{}:{}", reference, parsed.recipient.encoded());
    let mut authorization = PaymentAuthorization {
        authorization_type: PaymentAuthorizationType::PaymentAuthorization,
        chain: ChainId {
            namespace: "zcash".to_owned(),
            reference: reference.to_owned(),
        },
        recipient: recipient_caip10.clone(),
        amount: Amount {
            currency: "ZEC".to_owned(),
            value: parsed.amount.as_u64().to_string(),
            unit: AmountUnit::Base,
        },
        payment_id: prepared.payment_id.clone(),
        intent_hash: IntentHashString("v1:sha256:placeholder".to_owned()),
        expires_at: ExpiresAt::BlockHeight(prepared.expiry_height),
    };
    authorization.intent_hash = IntentHashString(
        recompute_intent_hash(&authorization, &recipient_caip10, parsed.amount.as_u64())
            .map_err(|err| DemoError::rejected("intent_hash_invalid", err.to_string()))?,
    );
    Ok(authorization)
}

fn load_or_create_issuer_encoding_key(config: &DemoConfig) -> Result<IssuerEncodingKey, DemoError> {
    create_issuer_key_if_missing(config)?;
    load_issuer_encoding_key(&config.issuer_key_path)
}

fn create_issuer_key_if_missing(config: &DemoConfig) -> Result<(), DemoError> {
    if !config.issuer_key_path.exists() {
        create_p256_issuer_key(config)?;
    }
    Ok(())
}

fn load_issuer_encoding_key(path: &Path) -> Result<IssuerEncodingKey, DemoError> {
    let raw = std::fs::read(path)
        .map_err(|err| DemoError::unavailable("issuer_key_unavailable", err.to_string()))?;
    if raw.starts_with(b"-----BEGIN") {
        if let Ok(encoding) = EncodingKey::from_ed_pem(&raw) {
            return Ok(IssuerEncodingKey {
                encoding,
                algorithm: Algorithm::EdDSA,
            });
        }
        if let Ok(encoding) = EncodingKey::from_ec_pem(&raw) {
            return Ok(IssuerEncodingKey {
                encoding,
                algorithm: Algorithm::ES256,
            });
        }
        Err(DemoError::rejected(
            "issuer_key_invalid",
            "issuer key must be Ed25519 or P-256 PKCS#8 PEM",
        ))
    } else {
        Ok(IssuerEncodingKey {
            encoding: EncodingKey::from_ed_der(&raw),
            algorithm: Algorithm::EdDSA,
        })
    }
}

fn create_p256_issuer_key(config: &DemoConfig) -> Result<(), DemoError> {
    if let Some(parent) = config.issuer_key_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| DemoError::unavailable("issuer_key_unavailable", err.to_string()))?;
    }
    let signing_key = SigningKey::random(&mut OsRng);
    let pem = signing_key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|err| DemoError::rejected("issuer_key_invalid", err.to_string()))?
        .to_string();
    write_secret_file(&config.issuer_key_path, pem.as_bytes())?;
    write_p256_issuer_jwks(&config.issuer_jwks_path, &config.issuer_kid, &signing_key)?;
    warn!(
        issuer_key_path = %config.issuer_key_path.display(),
        issuer_jwks_path = %config.issuer_jwks_path.display(),
        "created local demo issuer key; configure zspend with the matching JWKS for autopay",
    );
    Ok(())
}

fn write_p256_issuer_jwks(
    path: &Path,
    issuer_kid: &str,
    signing_key: &SigningKey,
) -> Result<(), DemoError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| DemoError::unavailable("issuer_jwks_unavailable", err.to_string()))?;
    }
    let point = signing_key.verifying_key().to_encoded_point(false);
    let x = URL_SAFE_NO_PAD.encode(
        point
            .x()
            .ok_or_else(|| DemoError::rejected("issuer_key_invalid", "P-256 point missing x"))?,
    );
    let y = URL_SAFE_NO_PAD.encode(
        point
            .y()
            .ok_or_else(|| DemoError::rejected("issuer_key_invalid", "P-256 point missing y"))?,
    );
    let jwks = serde_json::json!({
        "keys": [{
            "kty": "EC",
            "crv": "P-256",
            "x": x,
            "y": y,
            "kid": issuer_kid,
            "alg": "ES256",
            "use": "sig",
        }]
    });
    let bytes = serde_json::to_vec_pretty(&jwks)
        .map_err(|err| DemoError::rejected("issuer_jwks_invalid", err.to_string()))?;
    std::fs::write(path, bytes)
        .map_err(|err| DemoError::unavailable("issuer_jwks_unavailable", err.to_string()))
}

fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), DemoError> {
    std::fs::write(path, bytes)
        .map_err(|err| DemoError::unavailable("issuer_key_unavailable", err.to_string()))?;
    tighten_secret_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn tighten_secret_permissions(path: &Path) -> Result<(), DemoError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .map_err(|err| DemoError::unavailable("issuer_key_unavailable", err.to_string()))
}

#[cfg(not(unix))]
fn tighten_secret_permissions(_path: &Path) -> Result<(), DemoError> {
    Ok(())
}

async fn parse_faucet_response(response: reqwest::Response) -> Result<FaucetClaimBody, DemoError> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|err| DemoError::unavailable("faucet_unavailable", err.to_string()))?;
    let body: FaucetClaimBody = serde_json::from_slice(&bytes)
        .map_err(|err| DemoError::unavailable("faucet_malformed", err.to_string()))?;
    if !status.is_success() && body.error_code.is_none() {
        return Err(DemoError::unavailable(
            "faucet_unavailable",
            format!("fauzec returned HTTP {}", status.as_u16()),
        ));
    }
    Ok(body)
}

async fn open_or_bootstrap_wallet(
    network: Network,
    wallet_dir: &Path,
    chain: &ZinderChainSource,
    birthday: BlockHeight,
) -> Result<(Wallet, AccountId), DemoError> {
    let seed_path = wallet_dir.join("wallet.age");
    let db_path = wallet_dir.join("wallet.db");
    let bootstrap_needed = !seed_path.exists();
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(seed_path));
    let storage = Sqlite::new(SqliteOptions::for_network(network, db_path));

    if bootstrap_needed {
        let (wallet, account_id, mnemonic) = Wallet::builder(network, sealing, storage)
            .create(chain, birthday)
            .await?;
        warn!(
            mnemonic = mnemonic.as_phrase(),
            "fresh demo wallet created; back up this testnet mnemonic if you fund it"
        );
        return Ok((wallet, account_id));
    }

    Wallet::builder(network, sealing, storage)
        .open_or_create_account(chain, birthday)
        .await
        .map_err(DemoError::from)
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum PaymentMode {
    Checkout,
    Autopay,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum DemoStage {
    Ready,
    NeedsFunds,
    Review,
    Signing,
    Settling,
    Confirming,
    Mined,
    Final,
    Paid,
    Failed,
    Expired,
}

impl DemoStage {
    const fn is_terminal(self) -> bool {
        matches!(self, Self::Paid | Self::Failed | Self::Expired)
    }

    const fn message(self) -> &'static str {
        match self {
            Self::Ready => "Ready to start checkout",
            Self::NeedsFunds => "The demo wallet needs testnet funds",
            Self::Review => "Review the payment before signing",
            Self::Signing => "Signing…",
            Self::Settling => "Settling…",
            Self::Confirming => "Confirming…",
            Self::Mined => "Payment mined",
            Self::Final => "Payment final",
            Self::Paid => "Payment settled",
            Self::Failed => "Payment failed. Try again",
            Self::Expired => "This payment expired. Start a new checkout",
        }
    }
}

#[derive(Serialize)]
struct ReadinessBody {
    network: String,
    zpay: DependencyBody,
    zspend: DependencyBody,
    zinder: DependencyBody,
    wallet: DependencyBody,
    faucet: DependencyBody,
}

#[derive(Serialize)]
struct DependencyBody {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
}

impl DependencyBody {
    fn ready(status: &str) -> Self {
        Self::ready_with_height(status, None)
    }

    fn ready_with_height(status: &str, height: Option<u32>) -> Self {
        Self {
            status: status.to_owned(),
            kind: None,
            detail: None,
            retryable: false,
            height,
        }
    }

    fn not_ready(kind: &str, detail: impl Into<String>, retryable: bool) -> Self {
        Self {
            status: "not_ready".to_owned(),
            kind: Some(kind.to_owned()),
            detail: Some(detail.into()),
            retryable,
            height: None,
        }
    }
}

#[derive(Serialize)]
struct WalletBody {
    network: String,
    address: String,
    sapling_zat: u64,
    orchard_zat: u64,
    ironwood_zat: u64,
    transparent_mature_zat: u64,
    transparent_immature_zat: u64,
    total_zat: u64,
    is_funded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    as_of_height: Option<u32>,
}

#[derive(Deserialize)]
struct CreateFaucetClaimBody {
    #[serde(default)]
    address: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct FaucetClaimBody {
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    txid: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    confirmed_height: Option<u32>,
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    next_eligible_at_ms: Option<u64>,
}

#[derive(Deserialize)]
struct CreatePaymentBody {
    mode: PaymentMode,
}

#[derive(Clone, Serialize)]
struct PaymentBody {
    payment_id: String,
    mode: PaymentMode,
    stage: DemoStage,
    amount_zat: u64,
    expiry_height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confirmation_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mined_block_height: Option<u64>,
    reorg_count: u32,
    settled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    transaction_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    zexplorer_url: Option<String>,
    can_settle: bool,
    message: String,
}

impl PaymentBody {
    fn error(payment_id: String, detail: String) -> Self {
        Self {
            payment_id,
            mode: PaymentMode::Checkout,
            stage: DemoStage::Failed,
            amount_zat: 0,
            expiry_height: 0,
            status: Some("failed".to_owned()),
            confirmation_count: None,
            mined_block_height: None,
            reorg_count: 0,
            settled: false,
            transaction_id: None,
            zexplorer_url: None,
            can_settle: false,
            message: detail,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PreparedPayment {
    payment_id: String,
    payment_uri: String,
    memo_bytes: Vec<u8>,
    expiry_height: u32,
    amount_zat: u64,
}

impl Clone for PreparedPayment {
    fn clone(&self) -> Self {
        Self {
            payment_id: self.payment_id.clone(),
            payment_uri: self.payment_uri.clone(),
            memo_bytes: self.memo_bytes.clone(),
            expiry_height: self.expiry_height,
            amount_zat: self.amount_zat,
        }
    }
}

#[derive(Deserialize)]
struct PaymentStatusBody {
    status: String,
    #[serde(default)]
    broadcast_outcome: Option<BroadcastOutcomeBody>,
    #[serde(default)]
    confirmation_count: Option<u32>,
    #[serde(default)]
    mined_block_height: Option<u64>,
    #[serde(default)]
    reorg_count: u32,
    #[serde(default)]
    settled: bool,
}

#[derive(Clone, Deserialize, Serialize)]
struct BroadcastOutcomeBody {
    kind: String,
    #[serde(default)]
    transaction_id: Option<String>,
    #[serde(default)]
    upstream_message: Option<String>,
}

#[derive(Serialize)]
struct PrepareRequestBody {
    payee_id: String,
    network: String,
    scheme: String,
    resource_uri: String,
    nonce: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_pack_hash: Option<Vec<u8>>,
    idempotency_key: Option<String>,
}

#[derive(Serialize)]
struct DisclosureVerifyRequestBody {
    txid: String,
    expected_amount_zat: u64,
    expected_pay_to: String,
    expected_disclosure_message_hex: String,
    disclosure_payload_hex: String,
}

#[derive(Deserialize, Serialize)]
struct DisclosureVerifyResponseBody {
    cryptographic_verdict: String,
    #[serde(default)]
    inconclusive_reason: Option<String>,
    chain_presence: String,
    amount_reconciliation: String,
    recipient_reconciliation: String,
    message_reconciliation: String,
    #[serde(default)]
    transaction_id: Option<String>,
    #[serde(default)]
    payment_id: Option<String>,
    #[serde(default)]
    disclosed_value_zat: Option<u64>,
}

#[derive(Deserialize, Serialize)]
struct ConsolePaymentRow {
    payment_id: String,
    payee_id: String,
    amount_zat: u64,
    broadcast_outcome: BroadcastOutcomeBody,
    #[serde(default)]
    confirmation_count: Option<u32>,
    #[serde(default)]
    mined_block_height: Option<u64>,
    #[serde(default)]
    reorg_count: u32,
    settled_at_unix_seconds: i64,
}

#[derive(Deserialize, Serialize)]
struct ConsoleRateLimitsBody {
    per_jkt_per_minute: u32,
    per_ip_per_minute: u32,
    tracked_jkt_count: usize,
    tracked_ip_count: usize,
    limited_total_count: u64,
}

#[derive(Deserialize, Serialize)]
struct ConsolePaymentsBody {
    payments: Vec<ConsolePaymentRow>,
    rate_limits: ConsoleRateLimitsBody,
}

#[cfg(test)]
#[derive(Deserialize)]
struct SettlementResponseBody {
    broadcast_outcome: BroadcastOutcomeBody,
}

struct ParsedDemoPayment {
    recipient: PaymentRecipient,
    amount: Zatoshis,
}

struct IssuerEncodingKey {
    encoding: EncodingKey,
    algorithm: Algorithm,
}

#[cfg(test)]
fn zpay_outcome_to_submit_outcome(
    outcome: &SettlementResponseBody,
) -> Result<SubmitOutcome, SubmitterError> {
    let kind = outcome.broadcast_outcome.kind.as_str();
    if kind != "accepted" && kind != "duplicate" {
        return Err(SubmitterError::Unavailable {
            reason: format!("zpay broadcast outcome was non-success: {kind}"),
        });
    }
    let tx_id_hex = outcome
        .broadcast_outcome
        .transaction_id
        .clone()
        .unwrap_or_default();
    let tx_id_bytes = decode_txid(&tx_id_hex)?;
    let tx_id = TxId::from_bytes(tx_id_bytes);
    match kind {
        "accepted" => Ok(SubmitOutcome::Accepted { tx_id }),
        "duplicate" => Ok(SubmitOutcome::Duplicate { tx_id }),
        _ => unreachable!("non-success broadcast outcome rejected before txid decoding"),
    }
}

#[cfg(test)]
fn decode_txid(tx_id_hex: &str) -> Result<[u8; 32], SubmitterError> {
    let bytes = hex::decode(tx_id_hex).map_err(|err| SubmitterError::Unavailable {
        reason: format!("zpay broadcast outcome txid not hex: {err}"),
    })?;
    bytes.try_into().map_err(|_| SubmitterError::Unavailable {
        reason: "zpay broadcast outcome txid was not 32 bytes".to_owned(),
    })
}

fn parse_demo_network(raw: &str) -> Result<Network, DemoError> {
    match raw {
        "testnet" => Ok(Network::Testnet),
        "regtest" => Ok(Network::regtest()),
        "mainnet" => Err(DemoError::config("The demo gateway refuses mainnet")),
        other => Err(DemoError::config(format!("network {other} is unsupported"))),
    }
}

fn chain_reference(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "main",
        Network::Regtest(_) => "regtest",
        Network::Testnet | _ => "test",
    }
}

fn env_string(name: &str, fallback: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| !raw.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var(name)
        .ok()
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
}

fn env_socket_addr(name: &str, fallback: &str) -> Result<SocketAddr, DemoError> {
    env_string(name, fallback)
        .parse()
        .map_err(|err| DemoError::config(format!("{name} is invalid: {err}")))
}

fn env_optional_u32(name: &str) -> Result<Option<u32>, DemoError> {
    std::env::var(name)
        .ok()
        .map(|raw| {
            raw.parse()
                .map_err(|err| DemoError::config(format!("{name} is invalid: {err}")))
        })
        .transpose()
}

fn env_u64(name: &str, fallback: u64) -> Result<u64, DemoError> {
    std::env::var(name).map_or(Ok(fallback), |raw| {
        raw.parse()
            .map_err(|err| DemoError::config(format!("{name} is invalid: {err}")))
    })
}

async fn resolve_birthday_height(
    chain: &ZinderChainSource,
    configured_height: Option<u32>,
) -> Result<u32, DemoError> {
    if let Some(height) = configured_height {
        return Ok(height);
    }
    let tip = timeout(
        Duration::from_secs(PROBE_TIMEOUT_SECONDS),
        chain.chain_tip(),
    )
    .await
    .map_err(|_| DemoError::unavailable("zinder_timeout", "zinder tip probe timed out"))?
    .map_err(DemoError::from)?;
    Ok(tip
        .as_u32()
        .saturating_sub(DEFAULT_BIRTHDAY_LOOKBACK_BLOCKS))
}

fn unix_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

fn disclosure_message(payment_id: &str) -> Vec<u8> {
    format!("zpay-demo-payment:{payment_id}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::{
        BroadcastOutcomeBody, DEFAULT_FAUZEC_URL, DEFAULT_ISSUER_KID, DEFAULT_MIN_FUNDED_ZAT,
        DEFAULT_NETWORK_LABEL, DEFAULT_PAYEE_ID, DEFAULT_RESOURCE_URI, DEFAULT_TOKEN_TTL_SECONDS,
        DEFAULT_ZEXPLORER_TX_URL, DEFAULT_ZINDER_URL, DEFAULT_ZPAY_OPS_URL, DEFAULT_ZPAY_URL,
        DEFAULT_ZSPEND_AUDIENCE, DEFAULT_ZSPEND_URL, DemoConfig, DemoStage, PaymentBody,
        PaymentMode, PaymentStatusBody, SettlementResponseBody, disclosure_message, is_settled,
        load_or_create_issuer_encoding_key, parse_demo_network, payment_ids_most_recent_first,
        stage_from_status, zpay_outcome_to_submit_outcome,
    };
    use jsonwebtoken::Algorithm;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use zally_chain::SubmitOutcome;
    use zally_core::Network;

    #[test]
    fn maps_unsettled_final_to_final_stage() {
        let status = PaymentStatusBody {
            status: "final".to_owned(),
            broadcast_outcome: None,
            confirmation_count: Some(3),
            mined_block_height: Some(4100),
            reorg_count: 0,
            settled: false,
        };

        assert_eq!(stage_from_status(&status), DemoStage::Final);
    }

    #[test]
    fn maps_settled_final_to_paid_stage() {
        let status = PaymentStatusBody {
            status: "final".to_owned(),
            broadcast_outcome: Some(BroadcastOutcomeBody {
                kind: "accepted".to_owned(),
                transaction_id: Some("00".repeat(32)),
                upstream_message: None,
            }),
            confirmation_count: Some(10),
            mined_block_height: Some(4100),
            reorg_count: 0,
            settled: true,
        };

        assert_eq!(stage_from_status(&status), DemoStage::Paid);
    }

    #[test]
    fn settlement_flag_tracks_zpay_status_only() {
        let mut status = PaymentStatusBody {
            status: "final".to_owned(),
            broadcast_outcome: None,
            confirmation_count: Some(3),
            mined_block_height: Some(4100),
            reorg_count: 0,
            settled: false,
        };

        assert!(!is_settled(Some(&status)));
        status.settled = true;
        assert!(is_settled(Some(&status)));
    }

    #[test]
    fn terminal_stages_close_event_stream() {
        assert!(DemoStage::Paid.is_terminal());
        assert!(DemoStage::Failed.is_terminal());
        assert!(DemoStage::Expired.is_terminal());
        assert!(!DemoStage::Final.is_terminal());
    }

    #[test]
    fn maps_live_payment_statuses_to_ui_stages() {
        for (status, settled, expected) in [
            ("awaiting", false, DemoStage::Review),
            ("broadcast", false, DemoStage::Confirming),
            ("mined", false, DemoStage::Mined),
            ("expired", false, DemoStage::Expired),
            ("failed", false, DemoStage::Failed),
            ("never_issued", false, DemoStage::Failed),
        ] {
            let snapshot = PaymentStatusBody {
                status: status.to_owned(),
                broadcast_outcome: None,
                confirmation_count: None,
                mined_block_height: None,
                reorg_count: 0,
                settled,
            };

            assert_eq!(stage_from_status(&snapshot), expected);
        }
    }

    #[test]
    fn demo_network_refuses_mainnet() {
        assert!(matches!(
            parse_demo_network("testnet"),
            Ok(Network::Testnet)
        ));
        assert!(parse_demo_network("mainnet").is_err());
    }

    #[test]
    fn missing_issuer_key_creates_p256_dev_key() -> Result<(), Box<dyn std::error::Error>> {
        let scratch_dir =
            std::env::temp_dir().join(format!("zpay-demo-issuer-{}", super::unix_now_ms()));
        let config = DemoConfig {
            bind_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7410)),
            zpay_url: DEFAULT_ZPAY_URL.to_owned(),
            zpay_ops_url: DEFAULT_ZPAY_OPS_URL.to_owned(),
            zspend_url: DEFAULT_ZSPEND_URL.to_owned(),
            zspend_public_url: DEFAULT_ZSPEND_URL.to_owned(),
            zinder_url: DEFAULT_ZINDER_URL.to_owned(),
            wallet_dir: scratch_dir.clone(),
            birthday_height: None,
            network_label: DEFAULT_NETWORK_LABEL.to_owned(),
            network: Network::Testnet,
            payee_id: DEFAULT_PAYEE_ID.to_owned(),
            resource_uri: DEFAULT_RESOURCE_URI.to_owned(),
            fauzec_url: DEFAULT_FAUZEC_URL.to_owned(),
            zexplorer_tx_url: DEFAULT_ZEXPLORER_TX_URL.to_owned(),
            issuer_key_path: scratch_dir.join("dev-issuer-p256.pem"),
            issuer_jwks_path: scratch_dir.join("dev-jwks.json"),
            issuer_kid: DEFAULT_ISSUER_KID.to_owned(),
            zspend_audience: DEFAULT_ZSPEND_AUDIENCE.to_owned(),
            token_ttl_seconds: DEFAULT_TOKEN_TTL_SECONDS,
            min_funded_zat: DEFAULT_MIN_FUNDED_ZAT,
        };

        let issuer = load_or_create_issuer_encoding_key(&config)?;

        assert_eq!(issuer.algorithm, Algorithm::ES256);
        assert!(config.issuer_key_path.exists());
        assert!(config.issuer_jwks_path.exists());
        let jwks = std::fs::read_to_string(&config.issuer_jwks_path)?;
        assert!(jwks.contains("\"P-256\""));
        let _ = std::fs::remove_dir_all(scratch_dir);
        Ok(())
    }

    #[test]
    fn error_snapshot_is_not_settleable() {
        let body = PaymentBody::error("pay_1".to_owned(), "Payment failed".to_owned());

        assert_eq!(body.mode, PaymentMode::Checkout);
        assert_eq!(body.stage, DemoStage::Failed);
        assert!(!body.can_settle);
    }

    #[test]
    fn accepted_settle_outcome_maps_to_submit_accepted() {
        let outcome = SettlementResponseBody {
            broadcast_outcome: BroadcastOutcomeBody {
                kind: "accepted".to_owned(),
                transaction_id: Some("11".repeat(32)),
                upstream_message: None,
            },
        };

        assert!(matches!(
            zpay_outcome_to_submit_outcome(&outcome),
            Ok(SubmitOutcome::Accepted { .. })
        ));
    }

    #[test]
    fn duplicate_settle_outcome_maps_to_submit_duplicate() {
        let outcome = SettlementResponseBody {
            broadcast_outcome: BroadcastOutcomeBody {
                kind: "duplicate".to_owned(),
                transaction_id: Some("22".repeat(32)),
                upstream_message: None,
            },
        };

        assert!(matches!(
            zpay_outcome_to_submit_outcome(&outcome),
            Ok(SubmitOutcome::Duplicate { .. })
        ));
    }

    #[test]
    fn non_success_settle_outcome_is_refused() {
        let outcome = SettlementResponseBody {
            broadcast_outcome: BroadcastOutcomeBody {
                kind: "rejected".to_owned(),
                transaction_id: None,
                upstream_message: None,
            },
        };

        assert!(zpay_outcome_to_submit_outcome(&outcome).is_err());
    }

    #[test]
    fn payment_ids_sort_most_recent_first() {
        let ids = vec![
            "01JAAA0000000000000000000".to_owned(),
            "01JCCC0000000000000000000".to_owned(),
            "01JBBB0000000000000000000".to_owned(),
        ];

        assert_eq!(
            payment_ids_most_recent_first(ids),
            vec![
                "01JCCC0000000000000000000".to_owned(),
                "01JBBB0000000000000000000".to_owned(),
                "01JAAA0000000000000000000".to_owned(),
            ]
        );
    }

    #[test]
    fn disclosure_message_is_bound_to_the_payment_id() {
        assert_ne!(
            disclosure_message("01JZPAY-A"),
            disclosure_message("01JZPAY-B")
        );
        assert_eq!(
            disclosure_message("01JZPAY-A"),
            b"zpay-demo-payment:01JZPAY-A"
        );
    }
}
