//! x402 v2 wire adapter for zpay.
//!
//! Translates between the x402 v2 HTTP wire shape and `zpay-core`'s
//! protocol-neutral payment lifecycle. The router stays generic over the
//! [`zpay_core::broadcast::BroadcastClient`] implementation so the runtime
//! can swap a production zinder-backed client for a test fake without
//! touching the adapter's public shape.
//!
//! Routes mapped to real handlers:
//!
//! - `GET /accepts` advertises the payee `accepts[]` template.
//! - `GET /tip` reports the chain plane's current tip height (diagnostic).
//! - `POST /prepare` calls [`zpay_core::prepare::propose`].
//! - `POST /settle` calls [`zpay_core::settle::submit_settlement`].
//! - `POST /verify` verifies a ZIP-311 payment disclosure (delegates to
//!   zinder's `VerifyPaymentDisclosure`).
//! - `GET /payments/{payment_id}` returns the lifecycle snapshot.
//! - `GET /payments/{payment_id}/events` streams snapshots over SSE.
//!
//! Every JSON success body is the bare inner type: no `{ data: ... }`
//! envelope. RFC 7807 `application/problem+json` documents stay
//! envelope-free per the spec.
//!
//! See [ADR-0005][adr] for the per-wire-adapter crate boundary rationale
//! and [facilitator-plane.md][plane] for the shared lifecycle.
//!
//! [adr]: https://github.com/gustavovalverde/zpay/blob/main/docs/adrs/0005-protocol-neutral-core-with-wire-adapters.md
//! [plane]: https://github.com/gustavovalverde/zpay/blob/main/docs/architecture/facilitator-plane.md

pub mod dpop;
mod events;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Json, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use zally_chain::Submitter;
use zpay_core::accepts::PayeeRegistry;
use zpay_core::disclosure_fetcher::DisclosureFetcher;
use zpay_core::prepare::{PrepareError, PrepareRequest, PreparedTxStore, propose};
use zpay_core::settle::{SettleError, SettleRequest, submit_settlement};
use zpay_core::status::{SettlementLedgerStore, lookup_payment_status};
use zpay_core::tip::{ChainTip, ChainTipOracle, TipError};
use zpay_core::types::{PayeeId, PaymentId, PaymentNetwork};
use zpay_core::verify::{PaymentDisclosureVerifier, VerifyError, VerifyRequest, verify};

pub use dpop::{
    DpopError, DpopExpectations, InMemoryReplayStore as DpopInMemoryReplayStore, ReplayOutcome,
    ReplayStore as DpopReplayStore, VerifiedDpopProof,
};
pub use events::PaymentEventHub;

/// Shared application state injected into every x402 v2 handler.
pub struct AppState<C, V, P, L, T, F> {
    /// Prepared-tx store. Reads happen on `/prepare`, `/settle`, and
    /// `/payments/{id}`; writes happen on `/prepare` and on the
    /// success-kind branch of `/settle`. The trait abstracts over the
    /// in-memory variant used by tests and the libSQL variant used in
    /// production.
    pub prepared_store: Arc<P>,
    /// Settlement ledger. Reads happen on `/payments/{id}`; writes
    /// happen on `/settle` and on every oracle confirmation tick.
    pub ledger: Arc<L>,
    /// Registered payees and their `accepts[]` templates. Read by
    /// `GET /accepts?payee_id=…` and by `/prepare`'s registry-authoritative
    /// resolution path.
    pub payees: Arc<PayeeRegistry>,
    /// Chain plane abstraction used for broadcast.
    pub chain: Arc<C>,
    /// In-process ZIP-311 payment-disclosure verifier.
    pub verifier: Arc<V>,
    /// Per-payment SSE event hub. Subscribers come from
    /// `GET /payments/{id}/events`; publishers come from the
    /// `/settle` success path and the runtime's confirmation oracle.
    pub events: Arc<PaymentEventHub>,
    /// Chain-tip oracle. Read by `/prepare` to derive `expiry_height`
    /// and by `GET /tip` for operator diagnostics.
    pub tip_oracle: Arc<T>,
    /// Chain-side transaction fetcher used by `/verify` to resolve
    /// the disclosed transaction. Separate trait from the verifier
    /// so the cryptography stays in-process while the chain-side
    /// data plugs into whichever explorer the operator runs.
    pub fetcher: Arc<F>,
    /// DPoP replay store keyed by `(jkt, jti)`. Each `POST /prepare`
    /// and `POST /settle` request records its proof here for the
    /// 5-minute replay window; a second sighting of the same
    /// `(jkt, jti)` is rejected with 401 `DpopReplay`. The trait
    /// object lets production deployments swap the bundled
    /// `InMemoryReplayStore` for a shared backend (Redis, libSQL,
    /// KMS) without touching the wire handlers.
    pub dpop_replay: Arc<dyn DpopReplayStore>,
    /// Operator-supplied DPoP expectations. `expected_scheme` is the
    /// scheme the verifier expects on every authenticated request;
    /// `expected_host` pins the host so a `Host: evil.com` header
    /// cannot redirect canonicalization. When unset the verifier
    /// falls back to the inbound `Host` header and the runtime emits
    /// a startup `WARN`.
    pub dpop_expectations: DpopExpectations,
    /// Confirmation count at which a `Mined` snapshot transitions to
    /// `Final`. Resolved from `ZPAY_FINALITY_DEPTH` at startup;
    /// `zpay_core::status::DEFAULT_FINALITY_DEPTH` (3) is the default.
    pub finality_depth: u32,
}

// Manual `Clone` impl: every field is already an `Arc` or `Copy`, so
// cloning a reference is independent of whether the inner type
// implements `Clone`.
impl<C, V, P, L, T, F> Clone for AppState<C, V, P, L, T, F> {
    fn clone(&self) -> Self {
        Self {
            prepared_store: Arc::clone(&self.prepared_store),
            ledger: Arc::clone(&self.ledger),
            payees: Arc::clone(&self.payees),
            chain: Arc::clone(&self.chain),
            verifier: Arc::clone(&self.verifier),
            events: Arc::clone(&self.events),
            tip_oracle: Arc::clone(&self.tip_oracle),
            fetcher: Arc::clone(&self.fetcher),
            dpop_replay: Arc::clone(&self.dpop_replay),
            dpop_expectations: self.dpop_expectations.clone(),
            finality_depth: self.finality_depth,
        }
    }
}

impl<C, V, P, L, T, F> AppState<C, V, P, L, T, F> {
    /// Build a fresh shared state from the supplied prepared-tx store,
    /// settlement ledger, payee registry, broadcast client, disclosure
    /// verifier, SSE event hub, chain-tip oracle, transaction
    /// fetcher, DPoP replay store, and DPoP expectations. The replay
    /// store is passed in so production deployments can swap a shared
    /// backend without touching this composition root.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "AppState is the composition root: each Arc is a distinct collaborator the runtime wires in. Bundling them into a builder would hide the wire graph without changing the dependency count."
    )]
    pub fn new(
        prepared_store: Arc<P>,
        ledger: Arc<L>,
        payees: Arc<PayeeRegistry>,
        chain: Arc<C>,
        verifier: Arc<V>,
        events: Arc<PaymentEventHub>,
        tip_oracle: Arc<T>,
        fetcher: Arc<F>,
        dpop_replay: Arc<dyn DpopReplayStore>,
        dpop_expectations: DpopExpectations,
        finality_depth: u32,
    ) -> Self {
        Self {
            prepared_store,
            ledger,
            payees,
            chain,
            verifier,
            events,
            tip_oracle,
            fetcher,
            dpop_replay,
            dpop_expectations,
            finality_depth,
        }
    }
}

/// Compose the x402 v2 router mountable under `/x402/v2`.
///
/// Returns a fully-configured `Router<()>` after binding the supplied
/// [`AppState`] via `with_state`. Callers do not see the state type at
/// the mount point.
pub fn router<C, V, P, L, T, F>(state: AppState<C, V, P, L, T, F>) -> Router
where
    C: Submitter + 'static,
    V: PaymentDisclosureVerifier + 'static,
    P: PreparedTxStore + 'static,
    L: SettlementLedgerStore + 'static,
    T: ChainTipOracle + 'static,
    F: DisclosureFetcher + 'static,
{
    Router::new()
        .route("/accepts", get(accepts_handler::<C, V, P, L, T, F>))
        .route("/tip", get(tip_handler::<C, V, P, L, T, F>))
        .route("/prepare", post(prepare_handler::<C, V, P, L, T, F>))
        .route("/settle", post(settle_handler::<C, V, P, L, T, F>))
        .route("/verify", post(verify_handler::<C, V, P, L, T, F>))
        .route(
            "/payments/{payment_id}",
            get(payment_status_handler::<C, V, P, L, T, F>),
        )
        .route(
            "/payments/{payment_id}/events",
            get(events::events_handler::<C, V, P, L, T, F>),
        )
        .with_state(state)
}

async fn prepare_handler<C, V, P, L, T, F>(
    State(state): State<AppState<C, V, P, L, T, F>>,
    axum::extract::OriginalUri(original_uri): axum::extract::OriginalUri,
    headers: HeaderMap,
    Json(mut body): Json<PrepareRequest>,
) -> Response
where
    C: Submitter + 'static,
    V: PaymentDisclosureVerifier + 'static,
    P: PreparedTxStore + 'static,
    L: SettlementLedgerStore + 'static,
    T: ChainTipOracle + 'static,
    F: DisclosureFetcher + 'static,
{
    let jkt = match verify_request_dpop(
        "POST",
        &original_uri,
        &headers,
        state.dpop_replay.as_ref(),
        &state.dpop_expectations,
    )
    .await
    {
        Ok(verified) => verified.jkt,
        Err(err) => return dpop_error_response(&err),
    };

    // Header takes precedence over body field. RFC-draft `Idempotency-Key`
    // is the conventional surface; we accept the body field as a fallback
    // for clients that cannot set custom headers (e.g. constrained MCP
    // tool runtimes).
    if let Some(header_value) = headers
        .get("idempotency-key")
        .and_then(|raw| raw.to_str().ok())
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    {
        body.idempotency_key = Some(header_value.to_owned());
    }
    match propose(
        body,
        jkt,
        state.prepared_store.as_ref(),
        state.payees.as_ref(),
        state.tip_oracle.as_ref(),
    )
    .await
    {
        Ok(preparation) => json_ok(&preparation),
        Err(err) => prepare_error_response(&err),
    }
}

async fn settle_handler<C, V, P, L, T, F>(
    State(state): State<AppState<C, V, P, L, T, F>>,
    axum::extract::OriginalUri(original_uri): axum::extract::OriginalUri,
    headers: HeaderMap,
    Json(body): Json<SettleRequest>,
) -> Response
where
    C: Submitter + 'static,
    V: PaymentDisclosureVerifier + 'static,
    P: PreparedTxStore + 'static,
    L: SettlementLedgerStore + 'static,
    T: ChainTipOracle + 'static,
    F: DisclosureFetcher + 'static,
{
    let jkt = match verify_request_dpop(
        "POST",
        &original_uri,
        &headers,
        state.dpop_replay.as_ref(),
        &state.dpop_expectations,
    )
    .await
    {
        Ok(verified) => verified.jkt,
        Err(err) => return dpop_error_response(&err),
    };

    match submit_settlement(
        body,
        &jkt,
        state.prepared_store.as_ref(),
        state.ledger.as_ref(),
        state.chain.as_ref(),
    )
    .await
    {
        Ok(outcome) => {
            // Publish a fresh snapshot to any live SSE subscriber for this
            // payment. The hub never lazy-creates an entry on publish, so
            // this is a no-op for the common case where nobody is
            // listening. The snapshot read precedes `json_ok` to keep the
            // happy-path ordering deterministic: subscribers waiting on
            // /settle receive the terminal event before the HTTP response
            // returns.
            if let Ok(snapshot) = lookup_payment_status(
                &outcome.payment_id,
                state.prepared_store.as_ref(),
                state.ledger.as_ref(),
                state.finality_depth,
            )
            .await
            {
                state.events.publish(&outcome.payment_id, snapshot);
            }
            json_ok(&outcome)
        }
        Err(err) => settle_error_response(&err),
    }
}

async fn payment_status_handler<C, V, P, L, T, F>(
    State(state): State<AppState<C, V, P, L, T, F>>,
    Path(payment_id_raw): Path<String>,
) -> Response
where
    C: Submitter + 'static,
    V: PaymentDisclosureVerifier + 'static,
    P: PreparedTxStore + 'static,
    L: SettlementLedgerStore + 'static,
    T: ChainTipOracle + 'static,
    F: DisclosureFetcher + 'static,
{
    let payment_id = match payment_id_raw.parse::<PaymentId>() {
        Ok(id) => id,
        Err(reason) => {
            return problem_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid Argument",
                422,
                &reason.to_string(),
            );
        }
    };
    lookup_payment_status(
        &payment_id,
        state.prepared_store.as_ref(),
        state.ledger.as_ref(),
        state.finality_depth,
    )
    .await
    .map_or_else(
        |_| {
            problem_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Service Unavailable",
                503,
                "payment status store is currently unavailable",
            )
        },
        |snapshot| json_ok(&snapshot),
    )
}

async fn accepts_handler<C, V, P, L, T, F>(
    State(state): State<AppState<C, V, P, L, T, F>>,
    Query(query): Query<AcceptsQuery>,
) -> Response
where
    C: Submitter + 'static,
    V: PaymentDisclosureVerifier + 'static,
    P: PreparedTxStore + 'static,
    L: SettlementLedgerStore + 'static,
    T: ChainTipOracle + 'static,
    F: DisclosureFetcher + 'static,
{
    let Some(payee_id) = query.payee_id else {
        return problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "payee_id query parameter is required",
        );
    };
    state.payees.find(&PayeeId(payee_id.clone())).map_or_else(
        || {
            problem_response(
                StatusCode::NOT_FOUND,
                "Not Found",
                404,
                &format!("payee_id {payee_id:?} is not registered with this deployment"),
            )
        },
        |entries| json_ok(&entries.to_vec()),
    )
}

async fn tip_handler<C, V, P, L, T, F>(
    State(state): State<AppState<C, V, P, L, T, F>>,
    Query(query): Query<TipQuery>,
) -> Response
where
    C: Submitter + 'static,
    V: PaymentDisclosureVerifier + 'static,
    P: PreparedTxStore + 'static,
    L: SettlementLedgerStore + 'static,
    T: ChainTipOracle + 'static,
    F: DisclosureFetcher + 'static,
{
    let Some(network_raw) = query.network else {
        return problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "network query parameter is required",
        );
    };
    let network = match parse_network(&network_raw) {
        Ok(network) => network,
        Err(reason) => {
            return problem_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid Argument",
                422,
                &reason,
            );
        }
    };
    match state.tip_oracle.current_tip(network).await {
        Ok(tip_height) => json_ok(&ChainTip {
            network,
            tip_height,
        }),
        Err(err) => tip_error_response(&err),
    }
}

fn parse_network(raw: &str) -> Result<PaymentNetwork, String> {
    match raw {
        "mainnet" => Ok(PaymentNetwork::Mainnet),
        "testnet" => Ok(PaymentNetwork::Testnet),
        "regtest" => Ok(PaymentNetwork::Regtest),
        other => Err(format!(
            "network must be one of mainnet, testnet, regtest; got {other:?}"
        )),
    }
}

#[derive(Deserialize)]
struct AcceptsQuery {
    payee_id: Option<String>,
}

#[derive(Deserialize)]
struct TipQuery {
    network: Option<String>,
}

async fn verify_handler<C, V, P, L, T, F>(
    State(state): State<AppState<C, V, P, L, T, F>>,
    Json(body): Json<VerifyRequest>,
) -> Response
where
    C: Submitter + 'static,
    V: PaymentDisclosureVerifier + 'static,
    P: PreparedTxStore + 'static,
    L: SettlementLedgerStore + 'static,
    T: ChainTipOracle + 'static,
    F: DisclosureFetcher + 'static,
{
    match verify(body, state.verifier.as_ref(), state.fetcher.as_ref()).await {
        Ok(response) => json_ok(&response),
        Err(err) => verify_error_response(&err),
    }
}

fn verify_error_response(err: &VerifyError) -> Response {
    match err {
        VerifyError::PayloadInvalid { .. } => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "disclosure_payload_hex must be valid hex",
        ),
        // VerifyError is #[non_exhaustive]; today PayloadInvalid is
        // the only variant. Any future transport-class error gets a
        // safe 500 with no operator detail echoed.
        _ => problem_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            500,
            "verify returned an unrecognised error variant",
        ),
    }
}

fn json_ok<T: Serialize>(body: &T) -> Response {
    serde_json::to_string(body).map_or_else(
        |_| {
            problem_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                500,
                "response serialization failed",
            )
        },
        |serialized| {
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                serialized,
            )
                .into_response()
        },
    )
}

fn prepare_error_response(err: &PrepareError) -> Response {
    match err {
        PrepareError::PayeeUnknown { .. } => problem_response(
            StatusCode::NOT_FOUND,
            "Not Found",
            404,
            "payee_id is not registered with this deployment",
        ),
        PrepareError::SchemeNetworkUnsupported { .. } => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "registered payee does not advertise the requested scheme on the requested network",
        ),
        PrepareError::ExpiryHeightInvalid => problem_response(
            StatusCode::BAD_GATEWAY,
            "Bad Gateway",
            502,
            "chain tip oracle returned a zero tip; the operator must point the runtime at a healthy chain plane",
        ),
        PrepareError::TipOracle(_) => problem_response(
            StatusCode::BAD_GATEWAY,
            "Bad Gateway",
            502,
            "chain tip oracle is currently unavailable",
        ),
        PrepareError::Storage(_) => problem_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Service Unavailable",
            503,
            "prepared-tx store is currently unavailable",
        ),
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "PrepareError is #[non_exhaustive]; future variants need an explicit mapping but must not break the wire surface"
        )]
        _ => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "prepare rejected the request for a reason this build does not recognise",
        ),
    }
}

fn tip_error_response(err: &TipError) -> Response {
    match err {
        TipError::Unavailable { .. } => problem_response(
            StatusCode::BAD_GATEWAY,
            "Bad Gateway",
            502,
            "chain tip oracle is currently unavailable",
        ),
        TipError::NetworkUnsupported { .. } => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "chain tip oracle does not serve the requested network",
        ),
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "TipError is #[non_exhaustive]; future variants need an explicit mapping but must not break the wire surface"
        )]
        _ => problem_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            500,
            "tip oracle returned an unrecognised error variant",
        ),
    }
}

fn settle_error_response(err: &SettleError) -> Response {
    match err {
        SettleError::PreparationNotFound { .. } => problem_response(
            StatusCode::NOT_FOUND,
            "Not Found",
            404,
            "preparation not found or already settled",
        ),
        SettleError::RawTxHexInvalid => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "raw_tx_hex must be non-empty and contain only hex characters",
        ),
        SettleError::ChainUnavailable { .. } => problem_response(
            StatusCode::BAD_GATEWAY,
            "Bad Gateway",
            502,
            "chain plane is currently unavailable",
        ),
        SettleError::TransactionMalformed { .. } => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "raw_tx_hex did not parse as a Zcash v5 transaction",
        ),
        SettleError::ExpiryHeightMismatch { .. } => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "expiry_height in the signed transaction does not match the prepared row",
        ),
        SettleError::ObsoleteMemoVersion { .. } => problem_response(
            StatusCode::CONFLICT,
            "Obsolete Memo Version",
            409,
            "cached preparation carries an obsolete protocol memo version; re-prepare against this runtime",
        ),
        SettleError::DpopMismatch => problem_response_with_code(
            StatusCode::FORBIDDEN,
            "Forbidden",
            403,
            "settle was signed with a different DPoP key than prepare",
            "dpop_mismatch",
        ),
        SettleError::Storage(_) => problem_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Service Unavailable",
            503,
            "settle store is currently unavailable",
        ),
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "SettleError is #[non_exhaustive]; future variants need an explicit mapping but must not break the wire surface"
        )]
        _ => problem_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            500,
            "settle returned an unrecognised error variant",
        ),
    }
}

/// Verify the DPoP proof on an authenticated x402 v2 request.
///
/// Reads the `DPoP` header, canonicalises the request URL from the
/// operator-pinned [`DpopExpectations`] (falling back to the inbound
/// `Host` header when `expected_host` is `None`) plus the original
/// URI path, and verifies the proof via [`dpop::verify_dpop_proof`].
async fn verify_request_dpop(
    method: &str,
    original_uri: &axum::http::Uri,
    headers: &HeaderMap,
    replay_store: &(dyn dpop::ReplayStore + '_),
    expectations: &DpopExpectations,
) -> Result<dpop::VerifiedDpopProof, dpop::DpopError> {
    let proof = headers
        .get("dpop")
        .and_then(|raw| raw.to_str().ok())
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .ok_or(dpop::DpopError::Missing)?;

    let host = expectations.expected_host.as_deref().map_or_else(
        || {
            headers
                .get("host")
                .and_then(|raw| raw.to_str().ok())
                .map(str::trim)
                .filter(|raw| !raw.is_empty())
                .unwrap_or("localhost")
                .to_owned()
        },
        str::to_owned,
    );
    let scheme = &expectations.expected_scheme;
    let path = original_uri.path();
    let url = format!("{scheme}://{host}{path}");

    dpop::verify_dpop_proof(method, &url, proof, replay_store).await
}

fn dpop_error_response(err: &dpop::DpopError) -> Response {
    match err {
        dpop::DpopError::Missing => problem_response_with_code(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            401,
            "DPoP proof header is required for this endpoint",
            "dpop_missing",
        ),
        dpop::DpopError::InvalidProof { reason } => problem_response_with_code(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            401,
            &format!("DPoP proof invalid: {reason}"),
            "dpop_invalid_proof",
        ),
        dpop::DpopError::ClockSkew { drift_seconds } => problem_response_with_code(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            401,
            &format!("DPoP proof clock drift {drift_seconds}s exceeds tolerance"),
            "dpop_clock_skew",
        ),
        dpop::DpopError::Replay => problem_response_with_code(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            401,
            "DPoP proof has already been used",
            "dpop_replay",
        ),
    }
}

fn problem_response_with_code(
    status: StatusCode,
    title: &str,
    status_code: u16,
    detail: &str,
    code: &str,
) -> Response {
    let body = serde_json::json!({
        "type": "about:blank",
        "title": title,
        "status": status_code,
        "detail": detail,
        "code": code,
    });
    (
        status,
        [("content-type", "application/problem+json")],
        body.to_string(),
    )
        .into_response()
}

pub(crate) fn problem_response(
    status: StatusCode,
    title: &str,
    status_code: u16,
    detail: &str,
) -> Response {
    let body = serde_json::json!({
        "type": "about:blank",
        "title": title,
        "status": status_code,
        "detail": detail,
    });
    (
        status,
        [("content-type", "application/problem+json")],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod host_pinning_tests {
    //! Adversarial coverage for `verify_request_dpop`'s host-pinning gate.
    //!
    //! When `DpopExpectations::expected_host` is configured, an attacker
    //! who controls the inbound `Host` header still cannot satisfy `htu`
    //! because the canonicalization uses the configured host, not the
    //! header value.

    use super::{DpopExpectations, dpop, verify_request_dpop};
    use axum::http::{HeaderMap, HeaderName, HeaderValue, Uri};
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::{EncodePrivateKey as _, LineEnding};
    use rand_core::OsRng;
    use std::time::SystemTime;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn unix_now() -> Result<i64, Box<dyn std::error::Error>> {
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_secs())?;
        Ok(i64::try_from(secs)?)
    }

    fn mint_proof_with_htu(htu: &str) -> Result<String, Box<dyn std::error::Error>> {
        let signing_key = SigningKey::random(&mut OsRng);
        let verifying = signing_key.verifying_key();
        let point = verifying.to_encoded_point(false);
        let x = URL_SAFE_NO_PAD.encode(point.x().ok_or("no x")?);
        let y = URL_SAFE_NO_PAD.encode(point.y().ok_or("no y")?);

        let jwk = serde_json::json!({"kty": "EC", "crv": "P-256", "x": x, "y": y});
        let mut header = Header::new(Algorithm::ES256);
        header.typ = Some("dpop+jwt".to_owned());
        header.jwk = Some(serde_json::from_value(jwk)?);

        let iat = unix_now()?;
        let claims = serde_json::json!({
            "htm": "POST",
            "htu": htu,
            "jti": format!("jti-{iat}"),
            "iat": iat,
        });
        let pem = signing_key.to_pkcs8_pem(LineEnding::LF)?.to_string();
        let encoding_key = EncodingKey::from_ec_pem(pem.as_bytes())?;
        Ok(encode(&header, &claims, &encoding_key)?)
    }

    fn headers_with(host: &str, proof: &str) -> Result<HeaderMap, Box<dyn std::error::Error>> {
        let mut h = HeaderMap::new();
        h.insert(
            HeaderName::from_static("host"),
            HeaderValue::from_str(host)?,
        );
        h.insert(
            HeaderName::from_static("dpop"),
            HeaderValue::from_str(proof)?,
        );
        Ok(h)
    }

    /// Attack: the inbound `Host` header lies (`attacker.com`), but
    /// the operator pinned `expected_host = zpay.example.com`. A
    /// proof bound to `attacker.com` must NOT verify against the
    /// pinned host.
    #[tokio::test]
    async fn pinned_host_rejects_attacker_signed_proof() -> TestResult {
        let expectations = DpopExpectations {
            expected_scheme: "https".to_owned(),
            expected_host: Some("zpay.example.com".to_owned()),
        };
        let replay = dpop::InMemoryReplayStore::new();
        let proof = mint_proof_with_htu("https://attacker.com/x402/v2/prepare")?;
        let uri: Uri = "/x402/v2/prepare".parse()?;
        let headers = headers_with("attacker.com", &proof)?;
        let outcome = verify_request_dpop("POST", &uri, &headers, &replay, &expectations).await;
        let Err(err) = outcome else {
            return Err("attacker proof must not verify against pinned host".into());
        };
        if !matches!(err, dpop::DpopError::InvalidProof { .. }) {
            return Err(format!("expected InvalidProof on htu mismatch, got {err:?}").into());
        }
        Ok(())
    }

    /// Positive control: a proof bound to the pinned host verifies.
    #[tokio::test]
    async fn pinned_host_accepts_correctly_bound_proof() -> TestResult {
        let expectations = DpopExpectations {
            expected_scheme: "https".to_owned(),
            expected_host: Some("zpay.example.com".to_owned()),
        };
        let replay = dpop::InMemoryReplayStore::new();
        let proof = mint_proof_with_htu("https://zpay.example.com/x402/v2/prepare")?;
        let uri: Uri = "/x402/v2/prepare".parse()?;
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("dpop"),
            HeaderValue::from_str(&proof)?,
        );
        verify_request_dpop("POST", &uri, &headers, &replay, &expectations).await?;
        Ok(())
    }
}
