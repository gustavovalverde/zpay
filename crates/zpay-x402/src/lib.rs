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
//! - `POST /prepare` calls [`zpay_core::prepare::propose`].
//! - `POST /settle` calls [`zpay_core::settle::submit_settlement`].
//!
//! Still 501 (M2/M3 PRD-42 work):
//!
//! - `GET /accepts` advertises the merchant `accepts[]` template.
//! - `POST /verify` verifies a ZIP-311 payment disclosure (delegates to
//!   zinder's `VerifyPaymentDisclosure`).
//! - `GET /payments/{payment_id}` returns the lifecycle snapshot.
//!
//! See [ADR-0005][adr] for the per-wire-adapter crate boundary rationale
//! and [facilitator-plane.md][plane] for the shared lifecycle.
//!
//! [adr]: https://github.com/gustavovalverde/zpay/blob/main/docs/adrs/0005-protocol-neutral-core-with-wire-adapters.md
//! [plane]: https://github.com/gustavovalverde/zpay/blob/main/docs/architecture/facilitator-plane.md

use std::sync::Arc;

use axum::Router;
use axum::extract::{Json, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use zpay_core::accepts::{AcceptsEntry, MerchantRegistry};
use zpay_core::broadcast::BroadcastClient;
use zpay_core::prepare::{Preparation, PrepareError, PrepareRequest, PreparedTxCache, propose};
use zpay_core::settle::{SettleError, SettleRequest, SettlementOutcome, submit_settlement};
use zpay_core::status::{PaymentStatusSnapshot, SettlementLedger, lookup_payment_status};
use zpay_core::types::{MerchantId, PaymentId};
use zpay_core::verify::{DisclosureVerdict, DisclosureVerifier, VerifyError, VerifyRequest, verify};

/// Shared application state injected into every x402 v2 handler.
pub struct AppState<C, V> {
    /// Cache holding prepared transactions awaiting settlement.
    pub cache: Arc<PreparedTxCache>,
    /// Settlement ledger holding the last broadcast outcome per
    /// `payment_id`. Read by `GET /payments/{id}` and written by `settle`.
    pub ledger: Arc<SettlementLedger>,
    /// Registered merchants and their `accepts[]` templates. Read by
    /// `GET /accepts?merchant_id=…`.
    pub merchants: Arc<MerchantRegistry>,
    /// Chain plane abstraction used for broadcast. Wrapped in `Arc` so the
    /// state stays `Clone`; the underlying client is `Send + Sync` per the
    /// [`BroadcastClient`] contract.
    pub chain: Arc<C>,
    /// ZIP-311 disclosure verifier. Wrapped in `Arc` for the same reason
    /// as `chain`.
    pub verifier: Arc<V>,
}

// Manual `Clone` impl: `Arc<C>` and `Arc<V>` clone the reference count
// regardless of `C: Clone` or `V: Clone`, so the derive's bounds would
// be incorrect.
impl<C, V> Clone for AppState<C, V> {
    fn clone(&self) -> Self {
        Self {
            cache: Arc::clone(&self.cache),
            ledger: Arc::clone(&self.ledger),
            merchants: Arc::clone(&self.merchants),
            chain: Arc::clone(&self.chain),
            verifier: Arc::clone(&self.verifier),
        }
    }
}

impl<C, V> AppState<C, V> {
    /// Build a fresh shared state from the supplied cache, settlement
    /// ledger, merchant registry, broadcast client, and disclosure
    /// verifier.
    #[must_use]
    pub fn new(
        cache: Arc<PreparedTxCache>,
        ledger: Arc<SettlementLedger>,
        merchants: Arc<MerchantRegistry>,
        chain: Arc<C>,
        verifier: Arc<V>,
    ) -> Self {
        Self {
            cache,
            ledger,
            merchants,
            chain,
            verifier,
        }
    }
}

/// Compose the x402 v2 router mountable under `/x402/v2`.
///
/// Returns a fully-configured `Router<()>` after binding the supplied
/// [`AppState`] via `with_state`. Callers do not see the state type at
/// the mount point.
pub fn router<C: BroadcastClient + 'static, V: DisclosureVerifier + 'static>(
    state: AppState<C, V>,
) -> Router {
    Router::new()
        .route("/accepts", get(accepts_handler::<C, V>))
        .route("/prepare", post(prepare_handler::<C, V>))
        .route("/settle", post(settle_handler::<C, V>))
        .route("/verify", post(verify_handler::<C, V>))
        .route("/payments/{payment_id}", get(payment_status_handler::<C, V>))
        .with_state(state)
}

async fn prepare_handler<C: BroadcastClient + 'static, V: DisclosureVerifier + 'static>(
    State(state): State<AppState<C, V>>,
    headers: HeaderMap,
    Json(mut body): Json<PrepareRequest>,
) -> Response {
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
    match propose(body, &state.cache) {
        Ok(preparation) => json_ok(&PrepareResponseBody { data: preparation }),
        Err(err) => prepare_error_response(&err),
    }
}

async fn settle_handler<C: BroadcastClient + 'static, V: DisclosureVerifier + 'static>(
    State(state): State<AppState<C, V>>,
    Json(body): Json<SettleRequest>,
) -> Response {
    match submit_settlement(body, &state.cache, &state.ledger, state.chain.as_ref()).await {
        Ok(outcome) => json_ok(&SettleResponseBody { data: outcome }),
        Err(err) => settle_error_response(&err),
    }
}

async fn payment_status_handler<C: BroadcastClient + 'static, V: DisclosureVerifier + 'static>(
    State(state): State<AppState<C, V>>,
    Path(payment_id): Path<String>,
) -> Response {
    let snapshot = lookup_payment_status(&PaymentId(payment_id), &state.cache, &state.ledger);
    json_ok(&PaymentStatusResponseBody { data: snapshot })
}

async fn accepts_handler<C: BroadcastClient + 'static, V: DisclosureVerifier + 'static>(
    State(state): State<AppState<C, V>>,
    Query(query): Query<AcceptsQuery>,
) -> Response {
    let Some(merchant_id) = query.merchant_id else {
        return problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "merchant_id query parameter is required",
        );
    };
    state
        .merchants
        .find(&MerchantId(merchant_id.clone()))
        .map_or_else(
            || {
                problem_response(
                    StatusCode::NOT_FOUND,
                    "Not Found",
                    404,
                    &format!("merchant_id {merchant_id:?} is not registered with this deployment"),
                )
            },
            |entries| {
                json_ok(&AcceptsResponseBody {
                    accepts: entries.to_vec(),
                })
            },
        )
}

#[derive(Deserialize)]
struct AcceptsQuery {
    merchant_id: Option<String>,
}

#[derive(Serialize)]
struct PrepareResponseBody {
    data: Preparation,
}

#[derive(Serialize)]
struct SettleResponseBody {
    data: SettlementOutcome,
}

#[derive(Serialize)]
struct PaymentStatusResponseBody {
    data: PaymentStatusSnapshot,
}

#[derive(Serialize)]
struct AcceptsResponseBody {
    accepts: Vec<AcceptsEntry>,
}

#[derive(Serialize)]
struct VerifyResponseBody {
    data: DisclosureVerdict,
}

async fn verify_handler<C: BroadcastClient + 'static, V: DisclosureVerifier + 'static>(
    State(state): State<AppState<C, V>>,
    Json(body): Json<VerifyRequest>,
) -> Response {
    match verify(body, state.verifier.as_ref()).await {
        Ok(verdict) => json_ok(&VerifyResponseBody { data: verdict }),
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
        VerifyError::Unavailable { .. } => problem_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Service Unavailable",
            503,
            "disclosure verifier is currently unavailable",
        ),
        VerifyError::ResponseMalformed { .. } => problem_response(
            StatusCode::BAD_GATEWAY,
            "Bad Gateway",
            502,
            "disclosure verifier response could not be interpreted",
        ),
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "VerifyError is #[non_exhaustive]; future variants need an explicit mapping but must not break the wire surface"
        )]
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
        PrepareError::RecipientInvalid => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "recipient_unified_address must be a non-empty ZIP-316 unified address",
        ),
        PrepareError::AmountZero => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "amount_zat must be greater than zero",
        ),
        PrepareError::ExpiryHeightInvalid => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "expiry_height must be greater than zero",
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
        SettleError::MemoMismatch => problem_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Argument",
            422,
            "raw_tx_hex does not contain the prepared protocol memo; the wallet may have signed a different transaction",
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

fn problem_response(status: StatusCode, title: &str, status_code: u16, detail: &str) -> Response {
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

