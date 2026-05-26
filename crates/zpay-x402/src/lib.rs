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
use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use zpay_core::broadcast::BroadcastClient;
use zpay_core::prepare::{Preparation, PrepareError, PrepareRequest, PreparedTxCache, propose};
use zpay_core::settle::{SettleError, SettleRequest, SettlementOutcome, submit_settlement};

/// Shared application state injected into every x402 v2 handler.
pub struct AppState<C> {
    /// Cache holding prepared transactions awaiting settlement.
    pub cache: Arc<PreparedTxCache>,
    /// Chain plane abstraction used for broadcast. Wrapped in `Arc` so the
    /// state stays `Clone`; the underlying client is `Send + Sync` per the
    /// [`BroadcastClient`] contract.
    pub chain: Arc<C>,
}

// Manual `Clone` impl: `Arc<C>` clones the reference count regardless of
// `C: Clone`, so the derive's `C: Clone` bound is incorrect.
impl<C> Clone for AppState<C> {
    fn clone(&self) -> Self {
        Self {
            cache: Arc::clone(&self.cache),
            chain: Arc::clone(&self.chain),
        }
    }
}

impl<C> AppState<C> {
    /// Build a fresh shared state from the supplied cache and broadcast
    /// client.
    #[must_use]
    pub fn new(cache: Arc<PreparedTxCache>, chain: Arc<C>) -> Self {
        Self { cache, chain }
    }
}

/// Compose the x402 v2 router mountable under `/x402/v2`.
///
/// Returns a fully-configured `Router<()>` after binding the supplied
/// [`AppState`] via `with_state`. Callers do not see the state type at
/// the mount point.
pub fn router<C: BroadcastClient + 'static>(state: AppState<C>) -> Router {
    Router::new()
        .route("/accepts", get(not_yet_implemented))
        .route("/prepare", post(prepare_handler::<C>))
        .route("/settle", post(settle_handler::<C>))
        .route("/verify", post(not_yet_implemented))
        .route("/payments/{payment_id}", get(not_yet_implemented))
        .with_state(state)
}

async fn prepare_handler<C: BroadcastClient + 'static>(
    State(state): State<AppState<C>>,
    Json(body): Json<PrepareRequest>,
) -> Response {
    match propose(body, &state.cache) {
        Ok(preparation) => json_ok(&PrepareResponseBody { data: preparation }),
        Err(err) => prepare_error_response(&err),
    }
}

async fn settle_handler<C: BroadcastClient + 'static>(
    State(state): State<AppState<C>>,
    Json(body): Json<SettleRequest>,
) -> Response {
    match submit_settlement(body, &state.cache, state.chain.as_ref()).await {
        Ok(outcome) => json_ok(&SettleResponseBody { data: outcome }),
        Err(err) => settle_error_response(&err),
    }
}

#[derive(Serialize)]
struct PrepareResponseBody {
    data: Preparation,
}

#[derive(Serialize)]
struct SettleResponseBody {
    data: SettlementOutcome,
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

async fn not_yet_implemented() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        [("content-type", "application/problem+json")],
        r#"{"type":"about:blank","title":"Not Implemented","status":501,"detail":"This x402 v2 surface ships in a later PRD-42 phase."}"#,
    )
}
