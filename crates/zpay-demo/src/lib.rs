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
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use p256::ecdsa::SigningKey;
use p256::pkcs8::{EncodePrivateKey as _, LineEnding};
use parking_lot::Mutex;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
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
    PaymentRequest, ProposalPlan, SyncDriver, SyncDriverOptions, SyncHandle, Wallet,
};
use zpay_core::prepare::{PROTOCOL_MEMO_BYTE_COUNT, PROTOCOL_MEMO_BYTE_COUNT_NO_EVIDENCE};
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
        .route("/demo/v1/payments", post(create_payment_route))
        .route("/demo/v1/payments/{payment_id}", get(payment_route))
        .route(
            "/demo/v1/payments/{payment_id}/settle",
            post(settle_payment_route),
        )
        .route(
            "/demo/v1/payments/{payment_id}/events",
            get(payment_events_route),
        )
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

    fn set_stage_override(&self, payment_id: &str, stage: Option<DemoStage>) {
        if let Some(stored) = self.payments.lock().get_mut(payment_id) {
            stored.stage_override = stage;
        }
    }

    fn record_settlement(&self, payment_id: &str, transaction_id: String) {
        if let Some(stored) = self.payments.lock().get_mut(payment_id) {
            stored.settled_transaction_id = Some(transaction_id);
            stored.stage_override = Some(DemoStage::Paid);
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

    let dpop_key = Arc::new(DpopKey::generate()?);
    let prepared = call_prepare(&state, &dpop_key).await?;
    let payment_id = prepared.payment_id.clone();
    let stored = StoredPayment {
        prepared: prepared.clone(),
        mode: body.mode,
        stage_override: Some(DemoStage::Review),
        settled_transaction_id: None,
    };
    state.payments.lock().insert(payment_id.clone(), stored);
    payment_snapshot(&state, &payment_id).await.map(Json)
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
    let transaction_id = match stored.mode {
        PaymentMode::Checkout => settle_checkout(&state, &payment_id, &stored).await?,
        PaymentMode::Autopay => settle_autopay(&state, &payment_id, &stored).await?,
    };
    state.record_settlement(&payment_id, transaction_id);
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
    let dpop_proof = dpop_key.mint_proof("POST", &prepare_url)?;
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

async fn settle_checkout(
    state: &DemoState,
    payment_id: &str,
    stored: &StoredPayment,
) -> Result<String, DemoError> {
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
    let plan = ProposalPlan::conventional(
        state.account_id,
        parsed.recipient,
        parsed.amount,
        Some(memo),
    );
    let unsigned_pczt = state
        .wallet
        .propose_pczt(plan, Some(BlockHeight::from(stored.prepared.expiry_height)))
        .await?;
    let proven_pczt = state.wallet.prove_pczt(unsigned_pczt).await?;
    let signed_pczt = state.wallet.sign_pczt(proven_pczt).await?;
    let pczt_base64 = URL_SAFE_NO_PAD.encode(signed_pczt.as_bytes());
    settle_x402_pczt(state, &stored.prepared, &pczt_base64).await
}

async fn settle_autopay(
    state: &DemoState,
    payment_id: &str,
    stored: &StoredPayment,
) -> Result<String, DemoError> {
    state.set_stage_override(payment_id, Some(DemoStage::Signing));
    let authorization = build_agent_authorization(&stored.prepared, state.config.network)?;
    let issuer_key = load_or_create_issuer_encoding_key(&state.config)?;
    let dpop_key = DpopKey::generate()?;
    let access_token = mint_access_token(&AccessTokenGrant {
        issuer_key: &issuer_key.encoding,
        issuer_algorithm: issuer_key.algorithm,
        issuer_kid: &state.config.issuer_kid,
        audience: &state.config.zspend_audience,
        dpop_jkt: &dpop_key.jkt,
        authorization: &authorization,
        token_ttl_seconds: state.config.token_ttl_seconds,
    })?;
    let sign_public_url = format!(
        "{}/v1/payments/sign",
        state.config.zspend_public_url.trim_end_matches('/')
    );
    let call_sign_url = format!(
        "{}/v1/payments/sign",
        state.config.zspend_url.trim_end_matches('/')
    );
    let dpop_proof = dpop_key.mint_access_bound_proof(&access_token, "POST", &sign_public_url)?;
    let signed = request_zspend_signature(
        state,
        SignPaymentCall {
            call_sign_url,
            access_token,
            dpop_proof,
            prepared: stored.prepared.clone(),
        },
    )
    .await?;

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
        settled: status.as_ref().is_some_and(|snapshot| snapshot.settled)
            || matches!(ui_stage, DemoStage::Paid),
        transaction_id: tx_id,
        zexplorer_url,
        can_settle: matches!(ui_stage, DemoStage::Review),
        message: ui_stage.message().to_owned(),
    })
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

async fn request_zspend_signature(
    state: &DemoState,
    call: SignPaymentCall,
) -> Result<AgentSignedPczt, DemoError> {
    let body = SignPaymentRequestBody {
        payment_request: WirePaymentRequestBody {
            scheme: "zip321".to_owned(),
            request_uri: call.prepared.payment_uri,
        },
        network: state.config.network_label.clone(),
        payment_id: call.prepared.payment_id,
        target_expiry_height: call.prepared.expiry_height,
    };
    let response = state
        .client
        .post(call.call_sign_url)
        .header("authorization", format!("DPoP {}", call.access_token))
        .header("dpop", call.dpop_proof)
        .json(&body)
        .send()
        .await
        .map_err(|err| DemoError::unavailable("zspend_unavailable", err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body_text = response.text().await.unwrap_or_default();
        return Err(DemoError::unavailable(
            "zspend_sign_failed",
            format!("zspend /v1/payments/sign returned {status}: {body_text}"),
        ));
    }
    let signed: SignResponseBody = response
        .json()
        .await
        .map_err(|err| DemoError::unavailable("zspend_sign_malformed", err.to_string()))?;
    if signed.signed.format != "pczt-v2-extractable" {
        return Err(DemoError::rejected(
            "signed_format_invalid",
            format!("expected pczt-v2-extractable, got {}", signed.signed.format),
        ));
    }
    let pczt_bytes = URL_SAFE_NO_PAD
        .decode(signed.signed.bytes.as_bytes())
        .map_err(|err| DemoError::rejected("signed_bytes_invalid", err.to_string()))?;
    debug!(
        tx_id = %signed.signed.tx_id,
        pczt_bytes = pczt_bytes.len(),
        "zspend returned signed PCZT",
    );
    Ok(AgentSignedPczt {
        tx_id: signed.signed.tx_id,
        pczt_base64: signed.signed.bytes,
    })
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
    let settle: X402SettleResponseBody = response
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

async fn verify_x402_pczt(state: &DemoState, request: &serde_json::Value) -> Result<(), DemoError> {
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
    let verify: X402VerifyResponseBody = response
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
) -> Result<serde_json::Value, DemoError> {
    let parsed = payment_request(prepared, state.config.network)?;
    let network_id = x402_network_id(state.config.network);
    let requirements = serde_json::json!({
        "scheme": "exact",
        "network": network_id,
        "amount": parsed.amount.as_u64().to_string(),
        "asset": "ZEC",
        "payTo": parsed.recipient.encoded(),
        "maxTimeoutSeconds": state.config.token_ttl_seconds,
        "extra": {
            "binding": "x402-zcash-exact-v1",
            "amountUnit": "zat",
            "authorizationFormat": "pczt-v2-extractable",
            "zpayPaymentId": prepared.payment_id.as_str()
        }
    });
    let resource = serde_json::json!({
        "url": state.config.resource_uri.clone(),
        "description": "zpay demo resource",
        "mimeType": "application/json",
        "serviceName": "zpay-demo",
        "tags": ["demo", "zcash"],
    });
    Ok(serde_json::json!({
        "x402Version": 2,
        "paymentPayload": {
            "x402Version": 2,
            "resource": resource,
            "accepted": requirements,
            "payload": {
                "format": "pczt-v2-extractable",
                "pczt": pczt_base64,
            },
        },
        "paymentRequirements": requirements,
    }))
}

fn x402_network_id(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "zcash:mainnet",
        Network::Regtest(_) => "zcash:regtest",
        Network::Testnet | _ => "zcash:testnet",
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

fn mint_access_token(grant: &AccessTokenGrant<'_>) -> Result<String, DemoError> {
    let mut header = Header::new(grant.issuer_algorithm);
    header.kid = Some(grant.issuer_kid.to_owned());
    let claims = serde_json::json!({
        "aud": grant.audience,
        "jti": format!("zpay-demo-at-{}", unix_now_ms()),
        "exp": unix_now_seconds().saturating_add(grant.token_ttl_seconds),
        "cnf": { "jkt": grant.dpop_jkt },
        "authorization_details": [grant.authorization],
    });
    encode(&header, &claims, grant.issuer_key)
        .map_err(|err| DemoError::rejected("access_token_invalid", err.to_string()))
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

#[derive(Clone, Deserialize)]
struct BroadcastOutcomeBody {
    #[allow(
        dead_code,
        reason = "status JSON mirrors zpay broadcast outcome; runtime reads transaction_id and tests read kind"
    )]
    kind: String,
    #[serde(default)]
    transaction_id: Option<String>,
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

#[cfg(test)]
#[derive(Deserialize)]
struct SettlementResponseBody {
    broadcast_outcome: BroadcastOutcomeBody,
}

#[derive(Debug, Serialize)]
struct SignPaymentRequestBody {
    payment_request: WirePaymentRequestBody,
    network: String,
    payment_id: String,
    target_expiry_height: u32,
}

#[derive(Debug, Serialize)]
struct WirePaymentRequestBody {
    scheme: String,
    #[serde(rename = "value")]
    request_uri: String,
}

#[derive(Debug, Deserialize)]
struct SignResponseBody {
    #[serde(rename = "signed_payload")]
    signed: SignedSpendWire,
}

#[derive(Debug, Deserialize)]
struct SignedSpendWire {
    format: String,
    bytes: String,
    tx_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct X402VerifyResponseBody {
    is_valid: bool,
    invalid_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct X402SettleResponseBody {
    success: bool,
    error_reason: Option<String>,
    transaction: Option<String>,
}

struct ParsedDemoPayment {
    recipient: PaymentRecipient,
    amount: Zatoshis,
}

struct AgentSignedPczt {
    tx_id: String,
    pczt_base64: String,
}

struct SignPaymentCall {
    call_sign_url: String,
    access_token: String,
    dpop_proof: String,
    prepared: PreparedPayment,
}

struct AccessTokenGrant<'a> {
    issuer_key: &'a EncodingKey,
    issuer_algorithm: Algorithm,
    issuer_kid: &'a str,
    audience: &'a str,
    dpop_jkt: &'a str,
    authorization: &'a PaymentAuthorization,
    token_ttl_seconds: u64,
}

struct IssuerEncodingKey {
    encoding: EncodingKey,
    algorithm: Algorithm,
}

struct DpopKey {
    encoding: EncodingKey,
    x: String,
    y: String,
    jkt: String,
}

impl DpopKey {
    fn generate() -> Result<Self, DemoError> {
        let signing_key = SigningKey::random(&mut OsRng);
        let point = signing_key.verifying_key().to_encoded_point(false);
        let x = URL_SAFE_NO_PAD.encode(
            point
                .x()
                .ok_or_else(|| DemoError::rejected("dpop_key_invalid", "P-256 point missing x"))?,
        );
        let y = URL_SAFE_NO_PAD.encode(
            point
                .y()
                .ok_or_else(|| DemoError::rejected("dpop_key_invalid", "P-256 point missing y"))?,
        );
        let pem = signing_key
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|err| DemoError::rejected("dpop_key_invalid", err.to_string()))?
            .to_string();
        let encoding = EncodingKey::from_ec_pem(pem.as_bytes())
            .map_err(|err| DemoError::rejected("dpop_key_invalid", err.to_string()))?;
        let jkt = zspend_core::ec_jwk_thumbprint("P-256", "EC", &x, &y);
        Ok(Self {
            encoding,
            x,
            y,
            jkt,
        })
    }

    fn mint_proof(&self, method: &str, proof_url: &str) -> Result<String, DemoError> {
        self.mint_proof_inner(method, proof_url, None)
    }

    fn mint_access_bound_proof(
        &self,
        access_token: &str,
        method: &str,
        proof_url: &str,
    ) -> Result<String, DemoError> {
        let ath = URL_SAFE_NO_PAD.encode(Sha256::digest(access_token.as_bytes()));
        self.mint_proof_inner(method, proof_url, Some(ath))
    }

    fn mint_proof_inner(
        &self,
        method: &str,
        proof_url: &str,
        ath: Option<String>,
    ) -> Result<String, DemoError> {
        let mut header = Header::new(Algorithm::ES256);
        header.typ = Some("dpop+jwt".to_owned());
        header.jwk = Some(
            serde_json::from_value(serde_json::json!({
                "kty": "EC",
                "crv": "P-256",
                "x": self.x,
                "y": self.y,
            }))
            .map_err(|err| DemoError::rejected("dpop_proof_invalid", err.to_string()))?,
        );
        let mut claims = serde_json::json!({
            "htm": method,
            "htu": proof_url,
            "jti": format!("zpay-demo-dpop-{}", unix_now_ms()),
            "iat": unix_now_seconds(),
        });
        if let Some(ath) = ath {
            claims["ath"] = serde_json::Value::String(ath);
        }
        encode(&header, &claims, &self.encoding)
            .map_err(|err| DemoError::rejected("dpop_proof_invalid", err.to_string()))
    }
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

fn unix_now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn unix_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

#[cfg(test)]
mod tests {
    use super::{
        BroadcastOutcomeBody, DEFAULT_FAUZEC_URL, DEFAULT_ISSUER_KID, DEFAULT_MIN_FUNDED_ZAT,
        DEFAULT_NETWORK_LABEL, DEFAULT_PAYEE_ID, DEFAULT_RESOURCE_URI, DEFAULT_TOKEN_TTL_SECONDS,
        DEFAULT_ZEXPLORER_TX_URL, DEFAULT_ZINDER_URL, DEFAULT_ZPAY_OPS_URL, DEFAULT_ZPAY_URL,
        DEFAULT_ZSPEND_AUDIENCE, DEFAULT_ZSPEND_URL, DemoConfig, DemoStage, PaymentBody,
        PaymentMode, PaymentStatusBody, SettlementResponseBody, load_or_create_issuer_encoding_key,
        parse_demo_network, stage_from_status, zpay_outcome_to_submit_outcome,
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
            }),
            confirmation_count: Some(10),
            mined_block_height: Some(4100),
            reorg_count: 0,
            settled: true,
        };

        assert_eq!(stage_from_status(&status), DemoStage::Paid);
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
            },
        };

        assert!(zpay_outcome_to_submit_outcome(&outcome).is_err());
    }
}
