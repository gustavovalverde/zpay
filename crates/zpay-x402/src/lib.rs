//! x402 v2 wire adapter for zpay.
//!
//! Translates between the x402 v2 HTTP wire shape (`PAYMENT-REQUIRED`,
//! `PAYMENT-SIGNATURE`, `PAYMENT-RESPONSE` headers, JSON bodies under
//! `/x402/v2/*`) and `zpay-core`'s protocol-neutral payment lifecycle.
//!
//! See [ADR-0005][adr] for the rationale behind the per-wire-adapter crate
//! boundary, and [facilitator-plane.md][plane] for the shared lifecycle.
//!
//! [adr]: https://github.com/gustavovalverde/zpay/blob/main/docs/adrs/0005-protocol-neutral-core-with-wire-adapters.md
//! [plane]: https://github.com/gustavovalverde/zpay/blob/main/docs/architecture/facilitator-plane.md

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};

/// Compose the x402 v2 router mountable under `/x402/v2`.
///
/// All routes return HTTP 501 until M1 (PRD-42 Phase 4) implements them.
pub fn router() -> Router {
    Router::new()
        .route("/accepts", get(not_yet_implemented))
        .route("/prepare", post(not_yet_implemented))
        .route("/settle", post(not_yet_implemented))
        .route("/verify", post(not_yet_implemented))
        .route("/payments/{payment_id}", get(not_yet_implemented))
}

async fn not_yet_implemented() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        [("content-type", "application/problem+json")],
        r#"{"type":"about:blank","title":"Not Implemented","status":501,"detail":"x402 v2 ships in M1 (PRD-42 Phase 4)."}"#,
    )
}
