//! MPP (Machine Payments Protocol) wire adapter for zpay.
//!
//! Stubbed in M0. PRD-42 Phase 5 fills in the real wire shape once MPP's
//! spec stabilises. Until then, every route returns HTTP 501.
//!
//! The crate exists at scaffold time so that the dual-adapter shape is
//! exercised end-to-end (PRD-42 M4 exit criterion: adding `zpay-mpp` does
//! not modify `zpay-core`).
//!
//! See [ADR-0005][adr] for the per-wire-adapter crate boundary rationale.
//!
//! [adr]: https://github.com/gustavovalverde/zpay/blob/main/docs/adrs/0005-protocol-neutral-core-with-wire-adapters.md

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};

/// Compose the MPP v1 router mountable under `/mpp/v1`.
///
/// All routes return HTTP 501 until M4 (PRD-42 Phase 5) implements them.
/// The runtime mounts this only when the `mpp` Cargo feature is enabled
/// on `zpay-runtime`; the feature is off by default.
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
        r#"{"type":"about:blank","title":"Not Implemented","status":501,"detail":"MPP ships in M4 (PRD-42 Phase 5)."}"#,
    )
}
