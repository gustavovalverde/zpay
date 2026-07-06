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
mod rate_limit;

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, FromRequestParts, Json, Path, Query, State};
use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tower_http::cors::{AllowOrigin, CorsLayer};
use zally_chain::Submitter;
use zpay_core::accepts::PayeeRegistry;
use zpay_core::chain_status::ChainStatusCache;
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
pub use rate_limit::{RateLimitDecision, RateLimiter};

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
    /// Latest chain view (visible tip, settled tip) the background chain
    /// tasks refresh. Read on `/payments/{id}` and the SSE path to derive
    /// the `settled` flag and the unmined expiry-lapse without a chain
    /// round-trip per request.
    pub chain_status: Arc<ChainStatusCache>,
    /// Per-key fixed-window rate limiter. DPoP-authenticated routes count
    /// against the verified `jkt`; unauthenticated routes count against the
    /// client IP. Shared across handlers behind an `Arc`.
    pub rate_limiter: Arc<RateLimiter>,
    /// Whether the client-IP rate-limit dimension may trust
    /// `X-Forwarded-For`/`X-Real-IP`. Off by default: a direct caller
    /// controls those headers, so honoring them unconditionally lets an
    /// attacker rotate the leftmost hop per request and bypass the limiter.
    /// Only enable this when a trusted reverse proxy terminates every
    /// inbound connection and sets the header itself.
    pub trust_forwarded_headers: bool,
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
            chain_status: Arc::clone(&self.chain_status),
            rate_limiter: Arc::clone(&self.rate_limiter),
            trust_forwarded_headers: self.trust_forwarded_headers,
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
        chain_status: Arc<ChainStatusCache>,
        rate_limiter: Arc<RateLimiter>,
        trust_forwarded_headers: bool,
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
            chain_status,
            rate_limiter,
            trust_forwarded_headers,
        }
    }
}

/// Compose the x402 v2 router mountable under `/x402/v2`.
///
/// Returns a fully-configured `Router<()>` after binding the supplied
/// [`AppState`] via `with_state`. Callers do not see the state type at
/// the mount point.
///
/// `cors_allowlist` is a set of exact browser origins. An empty slice
/// attaches no CORS layer, so cross-origin browser requests stay blocked;
/// a non-empty slice permits those exact origins with no wildcard support.
pub fn router<C, V, P, L, T, F>(
    state: AppState<C, V, P, L, T, F>,
    cors_allowlist: &[String],
) -> Router
where
    C: Submitter + 'static,
    V: PaymentDisclosureVerifier + 'static,
    P: PreparedTxStore + 'static,
    L: SettlementLedgerStore + 'static,
    T: ChainTipOracle + 'static,
    F: DisclosureFetcher + 'static,
{
    let router = Router::new()
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
        .with_state(state);
    match build_cors_layer(cors_allowlist) {
        Some(layer) => router.layer(layer),
        None => router,
    }
}

/// Build a CORS layer permitting the exact origins in `allowlist`.
///
/// Returns `None` when the allowlist is empty (no CORS headers emitted).
/// Origins that do not parse as header values are dropped; an allowlist that
/// parses to zero valid origins also yields `None`.
fn build_cors_layer(allowlist: &[String]) -> Option<CorsLayer> {
    if allowlist.is_empty() {
        return None;
    }
    let origins: Vec<HeaderValue> = allowlist
        .iter()
        .filter_map(|origin| origin.parse::<HeaderValue>().ok())
        .collect();
    if origins.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([Method::GET, Method::POST])
            .allow_headers([
                CONTENT_TYPE,
                axum::http::header::AUTHORIZATION,
                HeaderName::from_static("dpop"),
                HeaderName::from_static("idempotency-key"),
            ]),
    )
}

/// Record a per-route request outcome counter for the instrumented routes.
fn record_request_metric(route: &'static str, response: &Response) {
    metrics::counter!(
        "zpay_requests_total",
        "route" => route,
        "outcome" => outcome_label(response.status()),
    )
    .increment(1);
}

/// Bounded outcome label derived from the response status class.
fn outcome_label(status: StatusCode) -> &'static str {
    if status.is_success() {
        "success"
    } else if status.is_client_error() {
        "client_error"
    } else if status.is_server_error() {
        "server_error"
    } else {
        "other"
    }
}

/// Bounded `kind` label for a broadcast outcome.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "BroadcastOutcome is #[non_exhaustive]; its Unknown variant and any future variant report `unknown`"
)]
fn broadcast_kind_label(outcome: &zpay_core::broadcast::BroadcastOutcome) -> &'static str {
    use zpay_core::broadcast::BroadcastOutcome;
    match outcome {
        BroadcastOutcome::Accepted { .. } => "accepted",
        BroadcastOutcome::Duplicate { .. } => "duplicate",
        BroadcastOutcome::InvalidEncoding { .. } => "invalid_encoding",
        BroadcastOutcome::Rejected { .. } => "rejected",
        _ => "unknown",
    }
}

/// Optional peer socket address, populated only when the router is served
/// with connection info. Extraction always succeeds, so handlers stay usable
/// under `oneshot` tests that carry no connect info.
pub(crate) struct PeerAddr(pub(crate) Option<SocketAddr>);

impl<S> FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|connect_info| connect_info.0),
        ))
    }
}

/// Enforce the client-IP rate-limit dimension, returning a 429 response when
/// the caller is over budget.
pub(crate) fn ip_rate_limit(
    limiter: &RateLimiter,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trust_forwarded_headers: bool,
) -> Option<Response> {
    match limiter.check_ip(client_ip(headers, peer, trust_forwarded_headers)) {
        RateLimitDecision::Limited {
            retry_after_seconds,
        } => Some(rate_limited_response(retry_after_seconds)),
        RateLimitDecision::Allowed => None,
    }
}

/// Best-effort client IP for the unauthenticated rate-limit dimension.
///
/// Consults the forwarding headers a reverse proxy sets only when
/// `trust_forwarded_headers` is `true`; otherwise those headers are
/// caller-controlled input and are ignored in favor of the socket peer
/// address. When neither the trusted header nor the peer is available,
/// every caller shares the unspecified-address bucket.
pub(crate) fn client_ip(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trust_forwarded_headers: bool,
) -> IpAddr {
    trust_forwarded_headers
        .then(|| forwarded_client_ip(headers))
        .flatten()
        .unwrap_or_else(|| peer.map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |addr| addr.ip()))
}

/// Parse a client IP from `X-Forwarded-For` (leftmost hop) or `X-Real-IP`.
fn forwarded_client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    if let Some(forwarded) = headers
        .get("x-forwarded-for")
        .and_then(|header| header.to_str().ok())
        && let Some(first) = forwarded.split(',').next()
        && let Ok(ip) = first.trim().parse::<IpAddr>()
    {
        return Some(ip);
    }
    headers
        .get("x-real-ip")
        .and_then(|header| header.to_str().ok())
        .and_then(|raw| raw.trim().parse::<IpAddr>().ok())
}

/// 429 problem-detail response carrying a `Retry-After` hint. Mirrors the
/// crate's other problem-detail bodies.
pub(crate) fn rate_limited_response(retry_after_seconds: u64) -> Response {
    let body = serde_json::json!({
        "title": "Too Many Requests",
        "kind": "rate_limited",
        "detail": "per-key request rate limit exceeded; retry after the window resets",
        "retryable": true,
    })
    .to_string();
    let mut response = (StatusCode::TOO_MANY_REQUESTS, body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    if let Ok(retry_after) = HeaderValue::from_str(&retry_after_seconds.to_string()) {
        headers.insert(RETRY_AFTER, retry_after);
    }
    response
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
    let response = 'response: {
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
            Err(err) => break 'response dpop_error_response(&err),
        };

        if let RateLimitDecision::Limited {
            retry_after_seconds,
        } = state.rate_limiter.check_jkt(&jkt)
        {
            break 'response rate_limited_response(retry_after_seconds);
        }

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
    };
    record_request_metric("prepare", &response);
    response
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
    let response = 'response: {
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
            Err(err) => break 'response dpop_error_response(&err),
        };

        if let RateLimitDecision::Limited {
            retry_after_seconds,
        } = state.rate_limiter.check_jkt(&jkt)
        {
            break 'response rate_limited_response(retry_after_seconds);
        }

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
                metrics::counter!(
                    "zpay_broadcast_outcomes_total",
                    "kind" => broadcast_kind_label(&outcome.broadcast_outcome),
                )
                .increment(1);
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
                    state.chain_status.load(),
                )
                .await
                {
                    state.events.publish(&outcome.payment_id, snapshot);
                }
                json_ok(&outcome)
            }
            Err(err) => settle_error_response(&err),
        }
    };
    record_request_metric("settle", &response);
    response
}

async fn payment_status_handler<C, V, P, L, T, F>(
    State(state): State<AppState<C, V, P, L, T, F>>,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
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
    if let Some(limited) = ip_rate_limit(
        &state.rate_limiter,
        &headers,
        peer,
        state.trust_forwarded_headers,
    ) {
        return limited;
    }
    let payment_id = match payment_id_raw.parse::<PaymentId>() {
        Ok(id) => id,
        Err(reason) => {
            return problem_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid Argument",
                "payment_id_invalid",
                &reason.to_string(),
                false,
            );
        }
    };
    lookup_payment_status(
        &payment_id,
        state.prepared_store.as_ref(),
        state.ledger.as_ref(),
        state.finality_depth,
        state.chain_status.load(),
    )
    .await
    .map_or_else(
        |_| {
            problem_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Service Unavailable",
                "status_store_unavailable",
                "payment status store is currently unavailable",
                true,
            )
        },
        |snapshot| json_ok(&snapshot),
    )
}

async fn accepts_handler<C, V, P, L, T, F>(
    State(state): State<AppState<C, V, P, L, T, F>>,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
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
    if let Some(limited) = ip_rate_limit(
        &state.rate_limiter,
        &headers,
        peer,
        state.trust_forwarded_headers,
    ) {
        return limited;
    }
    let Some(payee_id) = query.payee_id else {
        return problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            "payee_id_required",
            "payee_id query parameter is required",
            false,
        );
    };
    state.payees.find(&PayeeId(payee_id.clone())).map_or_else(
        || {
            problem_response(
                StatusCode::NOT_FOUND,
                "Not Found",
                "payee_unknown",
                &format!("payee_id {payee_id:?} is not registered with this deployment"),
                false,
            )
        },
        |entries| json_ok(&entries.to_vec()),
    )
}

async fn tip_handler<C, V, P, L, T, F>(
    State(state): State<AppState<C, V, P, L, T, F>>,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
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
    if let Some(limited) = ip_rate_limit(
        &state.rate_limiter,
        &headers,
        peer,
        state.trust_forwarded_headers,
    ) {
        return limited;
    }
    let Some(network_raw) = query.network else {
        return problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            "network_required",
            "network query parameter is required",
            false,
        );
    };
    let network = match parse_network(&network_raw) {
        Ok(network) => network,
        Err(reason) => {
            return problem_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid Argument",
                "network_invalid",
                &reason,
                false,
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
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
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
    let response = 'response: {
        if let Some(limited) = ip_rate_limit(
            &state.rate_limiter,
            &headers,
            peer,
            state.trust_forwarded_headers,
        ) {
            break 'response limited;
        }
        match verify(body, state.verifier.as_ref(), state.fetcher.as_ref()).await {
            Ok(response) => json_ok(&response),
            Err(err) => verify_error_response(&err),
        }
    };
    record_request_metric("verify", &response);
    response
}

fn verify_error_response(err: &VerifyError) -> Response {
    match err {
        VerifyError::PayloadInvalid { .. } => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            "disclosure_payload_invalid",
            "disclosure_payload_hex must be valid hex",
            false,
        ),
        // VerifyError is #[non_exhaustive]; today PayloadInvalid is
        // the only variant. Any future transport-class error gets a
        // safe 500 with no operator detail echoed.
        _ => problem_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            "verify_internal",
            "verify returned an unrecognised error variant",
            false,
        ),
    }
}

fn json_ok<T: Serialize>(body: &T) -> Response {
    serde_json::to_string(body).map_or_else(
        |_| {
            problem_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal",
                "serialization_failed",
                "response serialization failed",
                false,
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
            "payee_unknown",
            "payee_id is not registered with this deployment",
            false,
        ),
        PrepareError::SchemeNetworkUnsupported { .. } => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            "scheme_network_unsupported",
            "registered payee does not advertise the requested scheme on the requested network",
            false,
        ),
        PrepareError::ExpiryHeightInvalid => problem_response(
            StatusCode::BAD_GATEWAY,
            "Bad Gateway",
            "tip_oracle_zero_tip",
            "chain tip oracle returned a zero tip; the operator must point the runtime at a healthy chain plane",
            true,
        ),
        PrepareError::TipOracle(_) => problem_response(
            StatusCode::BAD_GATEWAY,
            "Bad Gateway",
            "tip_oracle_unavailable",
            "chain tip oracle is currently unavailable",
            true,
        ),
        PrepareError::Storage(_) => problem_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Service Unavailable",
            "prepared_store_unavailable",
            "prepared-tx store is currently unavailable",
            true,
        ),
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "PrepareError is #[non_exhaustive]; future variants need an explicit mapping but must not break the wire surface"
        )]
        _ => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            "prepare_invalid",
            "prepare rejected the request for a reason this build does not recognise",
            false,
        ),
    }
}

fn tip_error_response(err: &TipError) -> Response {
    match err {
        TipError::Unavailable { .. } => problem_response(
            StatusCode::BAD_GATEWAY,
            "Bad Gateway",
            "tip_oracle_unavailable",
            "chain tip oracle is currently unavailable",
            true,
        ),
        TipError::NetworkUnsupported { .. } => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            "network_unsupported",
            "chain tip oracle does not serve the requested network",
            false,
        ),
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "TipError is #[non_exhaustive]; future variants need an explicit mapping but must not break the wire surface"
        )]
        _ => problem_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            "tip_internal",
            "tip oracle returned an unrecognised error variant",
            false,
        ),
    }
}

fn settle_error_response(err: &SettleError) -> Response {
    match err {
        SettleError::PreparationNotFound { .. } => problem_response(
            StatusCode::NOT_FOUND,
            "Not Found",
            "preparation_not_found",
            "preparation not found or already settled",
            false,
        ),
        SettleError::RawTxHexInvalid => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            "raw_tx_hex_invalid",
            "raw_tx_hex must be non-empty and contain only hex characters",
            false,
        ),
        SettleError::ChainUnavailable { .. } => problem_response(
            StatusCode::BAD_GATEWAY,
            "Bad Gateway",
            "chain_unavailable",
            "chain plane is currently unavailable",
            true,
        ),
        SettleError::TransactionMalformed { reason } => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            "transaction_malformed",
            &format!("raw_tx_hex did not parse as a Zcash v5 transaction: {reason}"),
            false,
        ),
        SettleError::ExpiryHeightMismatch { .. } => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            "expiry_height_mismatch",
            "expiry_height in the signed transaction does not match the prepared row",
            false,
        ),
        SettleError::ObsoleteMemoVersion { .. } => problem_response(
            StatusCode::CONFLICT,
            "Obsolete Memo Version",
            "obsolete_memo_version",
            "cached preparation carries an obsolete protocol memo version; re-prepare against this runtime",
            false,
        ),
        SettleError::DpopMismatch => problem_response(
            StatusCode::FORBIDDEN,
            "Forbidden",
            "dpop_mismatch",
            "settle was signed with a different DPoP key than prepare",
            false,
        ),
        SettleError::Storage(_) => problem_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Service Unavailable",
            "settle_store_unavailable",
            "settle store is currently unavailable",
            true,
        ),
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "SettleError is #[non_exhaustive]; future variants need an explicit mapping but must not break the wire surface"
        )]
        _ => problem_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal",
            "settle_internal",
            "settle returned an unrecognised error variant",
            false,
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
        dpop::DpopError::Missing => problem_response(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "dpop_missing",
            "DPoP proof header is required for this endpoint",
            false,
        ),
        dpop::DpopError::InvalidProof { reason } => problem_response(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "dpop_invalid_proof",
            &format!("DPoP proof invalid: {reason}"),
            false,
        ),
        dpop::DpopError::ClockSkew { drift_seconds } => problem_response(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "dpop_clock_skew",
            &format!("DPoP proof clock drift {drift_seconds}s exceeds tolerance"),
            true,
        ),
        dpop::DpopError::Replay => problem_response(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "dpop_replay",
            "DPoP proof has already been used",
            false,
        ),
    }
}

pub(crate) fn problem_response(
    status: StatusCode,
    title: &str,
    kind: &str,
    detail: &str,
    retryable: bool,
) -> Response {
    let body = serde_json::json!({
        "title": title,
        "kind": kind,
        "detail": detail,
        "retryable": retryable,
    });
    (
        status,
        [("content-type", "application/problem+json")],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod ops_tests {
    //! Coverage for the CORS layer gating, the 429 envelope, and client-IP
    //! resolution used by the rate limiter.

    use super::{build_cors_layer, client_ip, rate_limited_response};
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[test]
    fn cors_layer_absent_when_allowlist_empty() {
        assert!(build_cors_layer(&[]).is_none());
    }

    #[test]
    fn cors_layer_present_when_allowlist_set() {
        let allowlist = vec!["https://app.example.com".to_owned()];
        assert!(build_cors_layer(&allowlist).is_some());
    }

    #[test]
    fn cors_layer_absent_when_no_origin_parses() {
        // A control byte is not a valid header value, so it drops out and the
        // allowlist parses to zero origins: no layer.
        let allowlist = vec!["http://bad\u{0}origin".to_owned()];
        assert!(build_cors_layer(&allowlist).is_none());
    }

    #[test]
    fn rate_limited_response_is_429_problem_json_with_retry_after() {
        let response = rate_limited_response(42);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response.headers().get("content-type"),
            Some(&HeaderValue::from_static("application/problem+json")),
        );
        assert_eq!(
            response.headers().get("retry-after"),
            Some(&HeaderValue::from_static("42")),
        );
    }

    #[test]
    fn client_ip_prefers_forwarded_header_when_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.7, 10.0.0.1"),
        );
        let peer = SocketAddr::from(([127, 0, 0, 1], 8080));
        let ip = client_ip(&headers, Some(peer), true);
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
    }

    #[test]
    fn client_ip_ignores_spoofed_forwarded_header_when_untrusted() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.7, 10.0.0.1"),
        );
        let peer = SocketAddr::from(([127, 0, 0, 1], 8080));
        let ip = client_ip(&headers, Some(peer), false);
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn client_ip_falls_back_to_peer_then_unspecified() {
        let headers = HeaderMap::new();
        let peer = SocketAddr::from(([198, 51, 100, 9], 8080));
        assert_eq!(
            client_ip(&headers, Some(peer), true),
            IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)),
        );
        assert_eq!(
            client_ip(&headers, None, true),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        );
    }
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
