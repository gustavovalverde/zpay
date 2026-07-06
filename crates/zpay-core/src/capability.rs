//! Capability string registry.
//!
//! Capability strings appear on every wire response's `capabilities[]`
//! array, in `/healthz`, and as the `x-capability` extension on each
//! OpenAPI operation. Operators turn capabilities on or off in
//! configuration, not in code.
//!
//! See [public-interfaces.md §Capability strings][1] for the naming
//! discipline.
//!
//! [1]: https://github.com/gustavovalverde/zpay/blob/main/docs/architecture/public-interfaces.md#capability-strings

/// x402 v2 `accepts[]` advertisement.
pub const X402_V2_ACCEPTS: &str = "x402.v2.accepts";
/// x402 v2 `prepare` endpoint.
pub const X402_V2_PREPARE: &str = "x402.v2.prepare";
/// x402 v2 `settle` endpoint.
pub const X402_V2_SETTLE: &str = "x402.v2.settle";
/// x402 v2 `verify` endpoint.
pub const X402_V2_VERIFY: &str = "x402.v2.verify";
/// x402 v2 `payments/{payment_id}` lookup endpoint.
pub const X402_V2_PAYMENTS: &str = "x402.v2.payments";

/// MPP v1 `accepts[]` advertisement.
pub const MPP_V1_ACCEPTS: &str = "mpp.v1.accepts";
/// MPP v1 `prepare` endpoint.
pub const MPP_V1_PREPARE: &str = "mpp.v1.prepare";
/// MPP v1 `settle` endpoint.
pub const MPP_V1_SETTLE: &str = "mpp.v1.settle";
/// MPP v1 `verify` endpoint.
pub const MPP_V1_VERIFY: &str = "mpp.v1.verify";
/// MPP v1 `payments/{payment_id}` lookup endpoint.
pub const MPP_V1_PAYMENTS: &str = "mpp.v1.payments";

/// Broadcast transaction via zinder.
pub const BROADCAST_TRANSACTION_V1: &str = "broadcast.transaction.v1";
/// Confirmation oracle from `ChainEvents`.
pub const BROADCAST_ORACLE_CONFIRM_V1: &str = "broadcast.oracle.confirm_v1";

/// Idempotent prepare via the prepared-tx cache.
pub const CACHE_PREPARE_IDEMPOTENT: &str = "cache.prepare.idempotent";
/// TTL discipline for the prepared-tx cache.
pub const CACHE_PREPARE_TTL: &str = "cache.prepare.ttl";
/// Append-only settlement ledger.
pub const CACHE_SETTLEMENT_LEDGER: &str = "cache.settlement.ledger";
