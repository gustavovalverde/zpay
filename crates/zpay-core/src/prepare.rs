//! Prepare a payment: compose the recipient URI, the protocol memo, and the
//! `payment_id` that the agent attaches to its payee request.
//!
//! Two surfaces live here:
//!
//! - The [`PrepareRequest`] / [`Preparation`] types and the [`propose`]
//!   function that combine a server-issued `payment_id` with the protocol
//!   memo and write the result to a [`PreparedTxCache`]. Memo composition
//!   itself lives in [`crate::binding::compose_binding_memo`]; the prepare
//!   path is its only sanctioned caller.
//! - The store-shaped persistence trait [`PreparedTxStore`] plus the
//!   in-memory implementation gated behind the `in_memory` Cargo feature.
//!
//! `propose` is registry-authoritative: the wire request names a payee,
//! scheme, and network; the [`crate::accepts::PayeeRegistry`] supplies
//! the recipient address, the expected amount, and the validity window.
//! Callers cannot override those values from the wire.
//!
//! Memo composition is server-side: the wire carries the resource URI
//! the agent advertised plus a caller-supplied nonce, and the binding
//! module derives the challenge and resource hashes via domain-separated
//! SHA-256. Clients no longer pre-hash anything. When an evidence pack
//! is bound to the payment, its 32-byte hash rides as the trailing slot
//! of the memo prefix; absent that, the memo is 66 bytes and stops at the
//! resource hash.
//!
//! Expiry math is oracle-driven: `propose` calls a [`ChainTipOracle`] and
//! adds [`DEFAULT_EXPIRY_DELTA_BLOCKS`] (or the matched entry's per-payee
//! override) to derive the prepared row's `expiry_height`. This eliminates
//! the prior contract where callers had to pre-compute and hand in a height
//! that had to match the wallet's signed transaction.

use std::future::Future;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::accepts::PayeeRegistry;
use crate::binding::compose_binding_memo;
use crate::store::StoreError;
use crate::tip::{ChainTipOracle, TipError};
use crate::types::{EvidencePackHash, PayeeId, PaymentId, PaymentNetwork, PaymentScheme, Zatoshis};

#[cfg(feature = "in_memory")]
use parking_lot::Mutex;
#[cfg(feature = "in_memory")]
use std::collections::HashMap;

/// Default validity window for a prepared transaction.
///
/// Five minutes balances retry tolerance against the cost of stale
/// prepared rows pinned in libSQL. Per-payee overrides apply via the
/// registry's `max_validity_seconds` field.
pub const DEFAULT_VALIDITY_SECONDS: u64 = 300;

/// Default delta (in blocks) added to the chain-tip oracle's report
/// when deriving a prepared row's `expiry_height`.
///
/// Mirrors `zcash_client_backend::wallet::DEFAULT_TX_EXPIRY_DELTA`
/// (40 blocks): wallets target `tip + 40` for signed transactions, so
/// the prepared row uses the same delta to keep settle's exact-match
/// gate from rejecting a wallet that synced against a slightly earlier
/// tip than zpay did. Per-payee overrides apply via [`AcceptsEntry::expiry_delta_blocks`].
///
/// [`AcceptsEntry::expiry_delta_blocks`]: crate::accepts::AcceptsEntry::expiry_delta_blocks
pub const DEFAULT_EXPIRY_DELTA_BLOCKS: u32 = 40;

/// Length of the protocol-defined memo prefix in bytes when an evidence
/// pack is bound to the payment.
///
/// Layout: protocol byte (1) + version byte (1) + challenge hash (32) +
/// resource hash (32) + evidence-pack hash (32).
///
/// On chain this 98-byte prefix occupies the leading region of a 512-byte
/// ZIP-302 [`Arbitrary`] memo: the wallet writes the prefix, zero-pads to
/// 511 bytes, and the protocol tag itself becomes byte 0 of the 512-byte
/// container.
///
/// [`Arbitrary`]: https://zips.z.cash/zip-0302#arbitrary
pub const PROTOCOL_MEMO_BYTE_COUNT: usize = 98;

/// Length of the protocol-defined memo prefix in bytes when no evidence
/// pack is bound to the payment.
///
/// Same layout as [`PROTOCOL_MEMO_BYTE_COUNT`], minus the trailing
/// 32-byte evidence-pack slot. The memo stops after the resource hash.
pub const PROTOCOL_MEMO_BYTE_COUNT_NO_EVIDENCE: usize = 66;

/// Leading byte of the zpay protocol memo.
///
/// ZIP-302 (`zips/zip-0302.rst:58-61`) reserves `0xFF` for arbitrary
/// application-defined payloads, with the remaining 511 bytes
/// unconstrained. Any value in `0x00..=0xF4` would force the rest of the
/// memo to parse as valid UTF-8, which our hash material cannot satisfy.
pub const PROTOCOL_MEMO_TAG: u8 = 0xff;

/// Current protocol memo layout version. Bumped when the layout changes;
/// the settle path rejects any other version with
/// [`crate::settle::SettleError::ObsoleteMemoVersion`].
///
/// `0x02` introduced server-side memo composition: the wire stopped
/// carrying pre-hashed `challenge_hash` / `resource_hash` arrays and
/// started carrying the resource URI and the caller nonce that
/// [`crate::binding::compose_binding_memo`] hashes into the prefix.
pub const PROTOCOL_MEMO_VERSION: u8 = 0x02;

/// Input to [`propose`].
///
/// Composed by a wire adapter from a protocol-specific request shape.
/// Registry-authoritative: only the keys (payee, scheme, network) plus
/// the human-meaningful binding inputs ride on the wire; the registry
/// supplies the recipient, amount, and validity, and the binding
/// module hashes the URI + nonce into the protocol memo prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareRequest {
    /// Payee whose `accepts[]` template the registry resolves against.
    pub payee_id: PayeeId,
    /// Network the payment will settle on.
    pub network: PaymentNetwork,
    /// Payment scheme advertised in the payee's `accepts[]` entry.
    pub scheme: PaymentScheme,
    /// Resource the agent advertised to the payer (the URL the payer
    /// is paying for). Stored verbatim on the wire and folded into
    /// both the challenge and resource hashes server-side; clients
    /// never pre-hash it.
    pub resource_uri: String,
    /// Caller-supplied nonce that uniquifies the challenge. Typically a
    /// UUID or hash; stored verbatim in the SHA-256 pre-image.
    pub nonce: String,
    /// Optional evidence-pack hash binding this payment to a zentity
    /// proof set. When present, the protocol memo grows from 66 to 98
    /// bytes and the supplied hash occupies the trailing slot.
    #[serde(default)]
    pub evidence_pack_hash: Option<EvidencePackHash>,
    /// Caller-supplied idempotency key. When set, a second `propose`
    /// call from the same agent (same DPoP `jkt`) with the same
    /// `idempotency_key` returns the original preparation instead of
    /// allocating a new one. The composite uniqueness is enforced at
    /// the store layer as `(agent_dpop_jkt, idempotency_key)`; the
    /// wire surface does not carry the jkt because the DPoP proof
    /// header does.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// Output of [`propose`]. The agent passes `payment_uri` and `memo_bytes`
/// to the user's wallet for signing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preparation {
    /// Server-issued opaque identifier; pair with the agent's DPoP JKT.
    pub payment_id: PaymentId,
    /// ZIP-321 payment URI for the user's wallet to consume.
    pub payment_uri: String,
    /// Protocol memo content: 66 bytes when no evidence pack is bound,
    /// 98 bytes when one is. Layout is the
    /// [`crate::binding::compose_binding_memo`] return value verbatim.
    pub memo_bytes: Vec<u8>,
    /// Block height after which this preparation cannot be settled.
    pub expiry_height: u32,
    /// Amount the payee expects in zatoshis. Resolved from the registry
    /// at propose time; agents read this back from the response so the
    /// receipt and the wallet both reflect the same authoritative value.
    pub amount_zat: Zatoshis,
}

/// Errors that can arise during preparation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PrepareError {
    /// Caller named a payee the registry does not have. Retry posture:
    /// `not_retryable`. The operator must register the payee or the
    /// agent must use a payee id that already exists.
    #[error("payee unknown: {payee_id:?}")]
    PayeeUnknown {
        /// The payee identifier that failed to resolve.
        payee_id: PayeeId,
    },
    /// The named payee is registered but has no `accepts[]` entry for
    /// the requested `(scheme, network)` pair. Retry posture:
    /// `not_retryable`.
    #[error("payee {payee_id:?} does not advertise scheme={scheme:?} on network={network:?}")]
    SchemeNetworkUnsupported {
        /// The payee identifier the caller asked about.
        payee_id: PayeeId,
        /// The scheme that was not advertised.
        scheme: PaymentScheme,
        /// The network that was not advertised.
        network: PaymentNetwork,
    },
    /// The chain-tip oracle returned zero. Retry posture: `requires_operator`.
    /// Either the chain plane is misreporting or the static fallback
    /// height was misconfigured.
    #[error("expiry_height derived from chain tip is zero")]
    ExpiryHeightInvalid,
    /// The chain-tip oracle itself surfaced an error. Retry posture:
    /// inherits from the wrapped [`TipError`] variant.
    #[error("chain tip oracle failure: {0}")]
    TipOracle(#[from] TipError),
    /// The prepared-tx store could not complete the read or write.
    /// Retry posture: inherits from the surfaced [`StoreError`] variant.
    #[error("prepared-tx store failure: {0}")]
    Storage(#[from] StoreError),
}

/// Cached prepared transaction.
///
/// Values flow through every [`PreparedTxStore`] implementation: an
/// in-memory store, a libSQL store, or any future backend. The store
/// trait stays free of backend-specific types so the same row shape
/// reads and writes against any of them.
#[derive(Debug, Clone)]
pub struct PreparedTxEntry {
    /// The full preparation returned to the agent.
    pub preparation: Preparation,
    /// Payee the prepare request targeted.
    pub payee_id: PayeeId,
    /// Network the payment is bound to.
    pub network: PaymentNetwork,
    /// Recipient address the user's wallet must pay.
    pub recipient_unified_address: String,
    /// Amount the user's wallet must pay.
    pub amount_zat: Zatoshis,
    /// Wall-clock deadline after which the sweeper removes the entry.
    pub expires_at_unix_seconds: u64,
    /// Caller-supplied idempotency key, scoped to `agent_dpop_jkt`.
    /// None when the prepare request did not carry one.
    pub idempotency_key: Option<String>,
    /// RFC 7638 JWK thumbprint of the agent's DPoP signing key. The
    /// wire layer extracts this from the verified `DPoP` proof on
    /// `POST /prepare` and threads it down to [`propose`]. Settle
    /// compares the cached value against the proof presented on
    /// `POST /settle` and refuses any mismatch.
    pub agent_dpop_jkt: String,
}

/// Storage trait for prepared transactions awaiting settlement.
///
/// Implementations live behind the trait so the wire layer, the TTL
/// sweeper, and the settle path do not see whether they are talking to
/// an in-memory `HashMap` or a libSQL row.
pub trait PreparedTxStore: Send + Sync {
    /// Persist a prepared-tx entry. Replaces any existing entry under
    /// the same `payment_id`. When `idempotency_key` is set, the
    /// implementation also indexes the entry under the
    /// `(agent_dpop_jkt, idempotency_key)` pair so a retried prepare
    /// from the same agent returns the original entry instead of
    /// allocating a new one.
    fn insert(&self, entry: PreparedTxEntry)
    -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Look up a prepared-tx entry by `payment_id`.
    fn find_by_payment_id(
        &self,
        payment_id: &PaymentId,
    ) -> impl Future<Output = Result<Option<PreparedTxEntry>, StoreError>> + Send;

    /// Resolve a `(jkt, idempotency_key)` to the prepared entry the
    /// first call produced. Returns `None` when no entry exists or the
    /// prior entry has already expired. The `jkt` is the RFC 7638 JWK
    /// thumbprint of the calling agent's DPoP signing key.
    fn find_by_idempotency(
        &self,
        jkt: &str,
        idempotency_key: &str,
    ) -> impl Future<Output = Result<Option<PreparedTxEntry>, StoreError>> + Send;

    /// Remove the prepared-tx entry for `payment_id`. The success-kind
    /// settle path calls this to enforce fire-once semantics. Returns
    /// the removed entry when present.
    fn remove(
        &self,
        payment_id: &PaymentId,
    ) -> impl Future<Output = Result<Option<PreparedTxEntry>, StoreError>> + Send;

    /// Remove every entry whose `expires_at_unix_seconds` is at or
    /// before `now_unix_seconds`. Returns the count of entries dropped
    /// so the sweeper can record telemetry.
    fn sweep_expired(
        &self,
        now_unix_seconds: u64,
    ) -> impl Future<Output = Result<usize, StoreError>> + Send;

    /// Number of prepared-tx entries currently in the store. Useful for
    /// tests and operator-side metrics; not required on a hot path.
    fn entry_count(&self) -> impl Future<Output = Result<usize, StoreError>> + Send;
}

/// In-memory implementation of [`PreparedTxStore`].
///
/// Storage is keyed by [`PaymentId`] with a secondary `(payee_id,
/// idempotency_key)` index so a retried prepare returns the same
/// preparation. Both maps live behind a single mutex to make insert
/// and sweep transitions atomic.
///
/// Suitable for unit tests and ad-hoc local development; not durable
/// across process restarts. Production runtimes compose the libSQL
/// implementation from `zpay-store`.
#[cfg(feature = "in_memory")]
#[derive(Debug, Default)]
pub struct PreparedTxCache {
    inner: Mutex<CacheInner>,
}

#[cfg(feature = "in_memory")]
#[derive(Debug, Default)]
struct CacheInner {
    by_payment_id: HashMap<PaymentId, PreparedTxEntry>,
    by_idempotency: HashMap<(String, String), PaymentId>,
}

#[cfg(feature = "in_memory")]
impl PreparedTxCache {
    /// Create a fresh empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(feature = "in_memory")]
impl PreparedTxStore for PreparedTxCache {
    async fn insert(&self, entry: PreparedTxEntry) -> Result<(), StoreError> {
        let mut guard = self.inner.lock();
        if let Some(key) = entry.idempotency_key.clone() {
            guard.by_idempotency.insert(
                (entry.agent_dpop_jkt.clone(), key),
                entry.preparation.payment_id.clone(),
            );
        }
        guard
            .by_payment_id
            .insert(entry.preparation.payment_id.clone(), entry);
        drop(guard);
        Ok(())
    }

    async fn find_by_payment_id(
        &self,
        payment_id: &PaymentId,
    ) -> Result<Option<PreparedTxEntry>, StoreError> {
        let guard = self.inner.lock();
        Ok(guard.by_payment_id.get(payment_id).cloned())
    }

    async fn find_by_idempotency(
        &self,
        jkt: &str,
        idempotency_key: &str,
    ) -> Result<Option<PreparedTxEntry>, StoreError> {
        let guard = self.inner.lock();
        let Some(payment_id) = guard
            .by_idempotency
            .get(&(jkt.to_owned(), idempotency_key.to_owned()))
        else {
            return Ok(None);
        };
        Ok(guard.by_payment_id.get(payment_id).cloned())
    }

    async fn remove(&self, payment_id: &PaymentId) -> Result<Option<PreparedTxEntry>, StoreError> {
        let mut guard = self.inner.lock();
        let removed = guard.by_payment_id.remove(payment_id);
        if let Some(ref entry) = removed
            && let Some(ref key) = entry.idempotency_key
        {
            guard
                .by_idempotency
                .remove(&(entry.agent_dpop_jkt.clone(), key.clone()));
        }
        drop(guard);
        Ok(removed)
    }

    async fn sweep_expired(&self, now_unix_seconds: u64) -> Result<usize, StoreError> {
        let mut guard = self.inner.lock();
        let before = guard.by_payment_id.len();
        guard
            .by_payment_id
            .retain(|_, entry| entry.expires_at_unix_seconds > now_unix_seconds);
        let live_ids: std::collections::HashSet<PaymentId> =
            guard.by_payment_id.keys().cloned().collect();
        guard.by_idempotency.retain(|_, pid| live_ids.contains(pid));
        Ok(before - guard.by_payment_id.len())
    }

    async fn entry_count(&self) -> Result<usize, StoreError> {
        let guard = self.inner.lock();
        Ok(guard.by_payment_id.len())
    }
}

/// Compose a [`Preparation`] from a [`PrepareRequest`] and insert it into
/// the store.
///
/// The returned `payment_id` is a fresh ULID; the caller pairs it with the
/// agent's DPoP thumbprint at the wire boundary. The composed `memo_bytes`
/// is the protocol prefix returned by
/// [`crate::binding::compose_binding_memo`]: 66 bytes when the request
/// omits `evidence_pack_hash`, 98 bytes when it carries one. The agent
/// transmits the bytes to the user's wallet alongside the `payment_uri`.
///
/// Registry-authoritative resolution:
/// `(payee_id, scheme, network)` looks up an [`crate::accepts::AcceptsEntry`]
/// that supplies the recipient address, the expected amount, the validity
/// window, and the optional per-entry expiry delta. The chain-tip oracle
/// supplies the current tip; expiry is `tip + delta`.
///
/// # Errors
///
/// - [`PrepareError::PayeeUnknown`] when the registry has no entries for
///   `payee_id`.
/// - [`PrepareError::SchemeNetworkUnsupported`] when the payee is
///   registered but does not advertise the requested `(scheme, network)`.
/// - [`PrepareError::ExpiryHeightInvalid`] when the oracle reports a zero
///   tip (a misconfigured static fallback, typically).
/// - [`PrepareError::TipOracle`] when the chain-tip oracle itself fails.
/// - [`PrepareError::Storage`] when the underlying [`PreparedTxStore`]
///   surfaces a [`crate::store::StoreError`].
pub async fn propose<S, T>(
    request: PrepareRequest,
    jkt: String,
    store: &S,
    registry: &PayeeRegistry,
    tip_oracle: &T,
) -> Result<Preparation, PrepareError>
where
    S: PreparedTxStore + ?Sized,
    T: ChainTipOracle + ?Sized,
{
    // Idempotency replay: a prior prepare with the same (jkt,
    // idempotency_key) returns the original preparation verbatim. Done
    // before any registry / oracle work so a retried prepare never
    // pays the resolution cost twice. A different agent presenting the
    // same idempotency_key (and therefore a different jkt) gets a
    // fresh payment_id.
    if let Some(key) = request
        .idempotency_key
        .as_ref()
        .filter(|raw| !raw.trim().is_empty())
        && let Some(prior) = store
            .find_by_idempotency(&jkt, key)
            .await
            .map_err(PrepareError::Storage)?
    {
        return Ok(prior.preparation);
    }

    // Registry-authoritative resolution. The wire request cannot
    // override the recipient, amount, or validity; the operator's
    // accepts.toml is the only source of truth for those values.
    let Some(entries) = registry.find(&request.payee_id) else {
        return Err(PrepareError::PayeeUnknown {
            payee_id: request.payee_id,
        });
    };
    let Some(entry) = entries.iter().find(|candidate| {
        candidate.scheme == request.scheme && candidate.network == request.network
    }) else {
        return Err(PrepareError::SchemeNetworkUnsupported {
            payee_id: request.payee_id,
            scheme: request.scheme,
            network: request.network,
        });
    };

    // Chain-tip-driven expiry. The wallet builds its signed tx against
    // (tip + DEFAULT_TX_EXPIRY_DELTA); the prepared row uses the same
    // math so the settle-time exact-match gate accepts it.
    let tip_height = tip_oracle.current_tip(request.network).await?;
    let delta = entry
        .expiry_delta_blocks
        .unwrap_or(DEFAULT_EXPIRY_DELTA_BLOCKS);
    let expiry_height = tip_height.saturating_add(delta);
    if expiry_height == 0 {
        return Err(PrepareError::ExpiryHeightInvalid);
    }

    let memo = compose_binding_memo(
        &request.payee_id,
        request.scheme,
        request.network,
        &request.resource_uri,
        &request.nonce,
        request.evidence_pack_hash.as_ref(),
    );
    let payment_uri = compose_payment_uri(&entry.pay_to, entry.amount_zat, &memo);

    let preparation = Preparation {
        payment_id: PaymentId::new(),
        payment_uri,
        memo_bytes: memo,
        expiry_height,
        amount_zat: entry.amount_zat,
    };

    let validity_seconds = if entry.max_validity_seconds > 0 {
        entry.max_validity_seconds
    } else {
        DEFAULT_VALIDITY_SECONDS
    };
    let expires_at_unix_seconds = current_unix_seconds().saturating_add(validity_seconds);

    store
        .insert(PreparedTxEntry {
            preparation: preparation.clone(),
            payee_id: request.payee_id,
            network: request.network,
            recipient_unified_address: entry.pay_to.clone(),
            amount_zat: entry.amount_zat,
            expires_at_unix_seconds,
            idempotency_key: request.idempotency_key.filter(|raw| !raw.trim().is_empty()),
            agent_dpop_jkt: jkt,
        })
        .await
        .map_err(PrepareError::Storage)?;

    Ok(preparation)
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Compose a minimal ZIP-321 URI for the recipient and amount.
///
/// Hand-rolled implementation for M1; once zally's `PaymentRequest::to_uri`
/// is wired into the workspace, this delegates to that surface so the URI
/// vocabulary lives in zally. The memo is base64url-encoded inline per
/// ZIP-321.
///
/// `pub` so the libSQL store adapter can re-derive the URI from the
/// persisted components (recipient + amount + memo) when answering
/// `find_by_idempotency`. The URI is intentionally not persisted as
/// a column because every byte of it is recoverable from the typed
/// fields the schema already carries.
#[must_use]
pub fn compose_payment_uri(
    recipient_unified_address: &str,
    amount_zat: Zatoshis,
    memo: &[u8],
) -> String {
    use base64::Engine;
    let memo_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(memo);
    let amount_zec = format_zec_from_zat(amount_zat);
    format!("zcash:{recipient_unified_address}?amount={amount_zec}&memo={memo_b64}")
}

fn format_zec_from_zat(amount_zat: Zatoshis) -> String {
    let zat = amount_zat.0;
    let whole = zat / 100_000_000;
    let fraction = zat % 100_000_000;
    if fraction == 0 {
        format!("{whole}")
    } else {
        let fraction_str = format!("{fraction:08}");
        let trimmed = fraction_str.trim_end_matches('0');
        format!("{whole}.{trimmed}")
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared fixtures for the prepare / settle / status / events tests.
    //!
    //! `propose` now takes a `PayeeRegistry` and a `ChainTipOracle`.
    //! Keeping the fakes here avoids re-declaring them in every test
    //! module.

    use parking_lot::Mutex;

    use super::PrepareRequest;
    use crate::accepts::{AcceptsEntry, PayeeRegistry};
    use crate::tip::{ChainTipOracle, TipError};
    use crate::types::{PayeeId, PaymentNetwork, PaymentScheme, Zatoshis};

    /// Default payee id used by every shared fixture.
    pub(crate) const FIXTURE_PAYEE_ID: &str = "aether-ai";

    /// Recipient address baked into the registry fixture. Tests assert
    /// the URI / persisted row carry this value verbatim.
    pub(crate) const FIXTURE_RECIPIENT: &str = "utest1exampleaddress";

    /// Amount baked into the registry fixture (`50_000` zat).
    pub(crate) const FIXTURE_AMOUNT_ZAT: u64 = 50_000;

    /// Tip height returned by [`FixedTipOracle`] in shared fixtures.
    pub(crate) const FIXTURE_TIP_HEIGHT: u32 = 3_217_900;

    /// JWK thumbprint stand-in for tests that do not exercise the
    /// DPoP-bound idempotency composite. Distinct from
    /// [`ALTERNATE_FIXTURE_JKT`] so a test can flip the agent
    /// identity in-place.
    pub(crate) const FIXTURE_JKT: &str = "test-jkt-aether-agent";

    /// Second JWK thumbprint stand-in used by the
    /// idempotency-scoped-by-jkt regression test.
    pub(crate) const ALTERNATE_FIXTURE_JKT: &str = "test-jkt-rival-agent";

    /// Build a [`PrepareRequest`] that resolves cleanly against the
    /// shared registry + tip oracle fixtures.
    pub(crate) fn valid_request() -> PrepareRequest {
        PrepareRequest {
            payee_id: PayeeId(FIXTURE_PAYEE_ID.to_owned()),
            network: PaymentNetwork::Testnet,
            scheme: PaymentScheme::Zcash,
            resource_uri: "https://example.test/resources/fixture".to_owned(),
            nonce: "00000000-0000-0000-0000-000000000000".to_owned(),
            evidence_pack_hash: None,
            idempotency_key: None,
        }
    }

    /// Registry with a single zcash/testnet accepts entry for
    /// [`FIXTURE_PAYEE_ID`]. Tests that need a different payee can call
    /// [`registry_with`] directly.
    pub(crate) fn fixture_registry() -> PayeeRegistry {
        registry_with(FIXTURE_PAYEE_ID)
    }

    /// Registry seeded with one zcash/testnet entry for `payee_id`.
    pub(crate) fn registry_with(payee_id: &str) -> PayeeRegistry {
        let mut registry = PayeeRegistry::new();
        registry.register(
            PayeeId(payee_id.to_owned()),
            vec![AcceptsEntry {
                scheme: PaymentScheme::Zcash,
                network: PaymentNetwork::Testnet,
                pay_to: FIXTURE_RECIPIENT.to_owned(),
                amount_zat: Zatoshis(FIXTURE_AMOUNT_ZAT),
                max_validity_seconds: 600,
                expiry_delta_blocks: None,
                merchant_requires_verify: false,
            }],
        );
        registry
    }

    /// In-memory [`ChainTipOracle`] that always returns the supplied tip.
    pub(crate) struct FixedTipOracle {
        tip: Mutex<u32>,
    }

    impl FixedTipOracle {
        pub(crate) fn new(tip: u32) -> Self {
            Self {
                tip: Mutex::new(tip),
            }
        }

        pub(crate) fn fixture() -> Self {
            Self::new(FIXTURE_TIP_HEIGHT)
        }
    }

    impl ChainTipOracle for FixedTipOracle {
        async fn current_tip(&self, _network: PaymentNetwork) -> Result<u32, TipError> {
            Ok(*self.tip.lock())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{
        ALTERNATE_FIXTURE_JKT, FIXTURE_JKT, FIXTURE_PAYEE_ID, FIXTURE_TIP_HEIGHT, FixedTipOracle,
        fixture_registry, registry_with, valid_request,
    };
    use super::{
        DEFAULT_EXPIRY_DELTA_BLOCKS, PROTOCOL_MEMO_BYTE_COUNT_NO_EVIDENCE, PrepareError,
        PreparedTxCache, PreparedTxStore, format_zec_from_zat, propose,
    };
    use crate::tip::{ChainTipOracle, TipError};
    use crate::types::{PayeeId, PaymentNetwork, Zatoshis};

    #[tokio::test]
    async fn propose_inserts_into_store_and_returns_full_preparation() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let registry = fixture_registry();
        let oracle = FixedTipOracle::fixture();
        let preparation = propose(
            valid_request(),
            FIXTURE_JKT.to_owned(),
            &store,
            &registry,
            &oracle,
        )
        .await
        .map_err(|_| "propose must accept valid input")?;

        assert_eq!(
            preparation.memo_bytes.len(),
            PROTOCOL_MEMO_BYTE_COUNT_NO_EVIDENCE,
        );
        let expected_expiry = FIXTURE_TIP_HEIGHT.saturating_add(DEFAULT_EXPIRY_DELTA_BLOCKS);
        assert_eq!(preparation.expiry_height, expected_expiry);
        assert_eq!(preparation.amount_zat, Zatoshis(50_000));
        assert!(
            preparation
                .payment_uri
                .starts_with("zcash:utest1exampleaddress?")
        );
        assert!(preparation.payment_uri.contains("amount=0.0005"));
        assert!(preparation.payment_uri.contains("memo="));
        assert_eq!(
            store
                .entry_count()
                .await
                .map_err(|_| "entry_count failed")?,
            1
        );
        let cached = store
            .find_by_payment_id(&preparation.payment_id)
            .await
            .map_err(|_| "find failed")?
            .ok_or("store must echo the prepared payment_id")?;
        assert_eq!(cached.amount_zat, Zatoshis(50_000));
        assert_eq!(cached.recipient_unified_address, "utest1exampleaddress");
        assert_eq!(cached.agent_dpop_jkt, FIXTURE_JKT);
        Ok(())
    }

    #[tokio::test]
    async fn propose_refuses_unknown_payee() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let registry = registry_with("known-payee");
        let oracle = FixedTipOracle::fixture();
        let mut request = valid_request();
        request.payee_id = PayeeId("missing".to_owned());
        let outcome = propose(request, FIXTURE_JKT.to_owned(), &store, &registry, &oracle).await;
        assert!(matches!(outcome, Err(PrepareError::PayeeUnknown { .. })));
        assert_eq!(
            store
                .entry_count()
                .await
                .map_err(|_| "entry_count failed")?,
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn propose_refuses_scheme_network_mismatch() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let registry = fixture_registry();
        let oracle = FixedTipOracle::fixture();
        let mut request = valid_request();
        request.network = PaymentNetwork::Mainnet;
        let outcome = propose(request, FIXTURE_JKT.to_owned(), &store, &registry, &oracle).await;
        assert!(matches!(
            outcome,
            Err(PrepareError::SchemeNetworkUnsupported { .. })
        ));
        assert_eq!(
            store
                .entry_count()
                .await
                .map_err(|_| "entry_count failed")?,
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn propose_refuses_zero_tip() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let registry = fixture_registry();
        let oracle = FixedTipOracle::new(0);
        let mut request = valid_request();
        // Override the registry entry's delta to zero so tip+delta == 0.
        // (Cannot reach via the wire; we have to mutate the registry.)
        let mut zero_registry = registry;
        if let Some(entries) = zero_registry
            .find(&PayeeId(FIXTURE_PAYEE_ID.to_owned()))
            .map(<[crate::accepts::AcceptsEntry]>::to_vec)
        {
            let mut adjusted = entries;
            for entry in &mut adjusted {
                entry.expiry_delta_blocks = Some(0);
            }
            zero_registry.register(PayeeId(FIXTURE_PAYEE_ID.to_owned()), adjusted);
        }
        request.idempotency_key = None;
        let outcome = propose(
            request,
            FIXTURE_JKT.to_owned(),
            &store,
            &zero_registry,
            &oracle,
        )
        .await;
        assert!(matches!(outcome, Err(PrepareError::ExpiryHeightInvalid)));
        Ok(())
    }

    #[tokio::test]
    async fn propose_surfaces_tip_oracle_unavailable() -> Result<(), &'static str> {
        struct FailingOracle;

        impl ChainTipOracle for FailingOracle {
            async fn current_tip(&self, _network: PaymentNetwork) -> Result<u32, TipError> {
                Err(TipError::Unavailable {
                    reason: "no chain plane".to_owned(),
                })
            }
        }

        let store = PreparedTxCache::new();
        let registry = fixture_registry();
        let outcome = propose(
            valid_request(),
            FIXTURE_JKT.to_owned(),
            &store,
            &registry,
            &FailingOracle,
        )
        .await;
        assert!(matches!(outcome, Err(PrepareError::TipOracle(_))));
        Ok(())
    }

    #[tokio::test]
    async fn store_remove_makes_payment_id_unfindable() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let registry = fixture_registry();
        let oracle = FixedTipOracle::fixture();
        let preparation = propose(
            valid_request(),
            FIXTURE_JKT.to_owned(),
            &store,
            &registry,
            &oracle,
        )
        .await
        .map_err(|_| "propose must accept valid input")?;
        let removed = store
            .remove(&preparation.payment_id)
            .await
            .map_err(|_| "remove failed")?;
        assert!(removed.is_some());
        assert_eq!(
            store
                .entry_count()
                .await
                .map_err(|_| "entry_count failed")?,
            0
        );
        assert!(
            store
                .find_by_payment_id(&preparation.payment_id)
                .await
                .map_err(|_| "find failed")?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn format_zec_handles_whole_and_fractional_amounts() {
        assert_eq!(format_zec_from_zat(Zatoshis(100_000_000)), "1");
        assert_eq!(format_zec_from_zat(Zatoshis(50_000)), "0.0005");
        assert_eq!(format_zec_from_zat(Zatoshis(1)), "0.00000001");
        assert_eq!(format_zec_from_zat(Zatoshis(0)), "0");
        assert_eq!(format_zec_from_zat(Zatoshis(150_000_000)), "1.5");
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "fixture-heavy regression test; splitting would obscure the temporal ordering this test asserts"
    )]
    async fn sweep_drops_expired_entries_and_keeps_active_ones() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let oracle = FixedTipOracle::fixture();
        // Two distinct registries with different max_validity_seconds.
        let short_registry = {
            let mut registry = crate::accepts::PayeeRegistry::new();
            registry.register(
                PayeeId(FIXTURE_PAYEE_ID.to_owned()),
                vec![crate::accepts::AcceptsEntry {
                    scheme: crate::types::PaymentScheme::Zcash,
                    network: PaymentNetwork::Testnet,
                    pay_to: "utest1exampleaddress".to_owned(),
                    amount_zat: Zatoshis(50_000),
                    max_validity_seconds: 1,
                    expiry_delta_blocks: None,
                    merchant_requires_verify: false,
                }],
            );
            registry
        };
        let long_registry = {
            let mut registry = crate::accepts::PayeeRegistry::new();
            registry.register(
                PayeeId(FIXTURE_PAYEE_ID.to_owned()),
                vec![crate::accepts::AcceptsEntry {
                    scheme: crate::types::PaymentScheme::Zcash,
                    network: PaymentNetwork::Testnet,
                    pay_to: "utest1exampleaddress".to_owned(),
                    amount_zat: Zatoshis(50_000),
                    max_validity_seconds: 3600,
                    expiry_delta_blocks: None,
                    merchant_requires_verify: false,
                }],
            );
            registry
        };

        let mut short_lived = valid_request();
        short_lived.idempotency_key = Some("short".to_owned());
        let mut long_lived = valid_request();
        long_lived.idempotency_key = Some("long".to_owned());

        let short = propose(
            short_lived,
            FIXTURE_JKT.to_owned(),
            &store,
            &short_registry,
            &oracle,
        )
        .await
        .map_err(|_| "short propose must succeed")?;
        let long = propose(
            long_lived,
            FIXTURE_JKT.to_owned(),
            &store,
            &long_registry,
            &oracle,
        )
        .await
        .map_err(|_| "long propose must succeed")?;
        assert_eq!(
            store
                .entry_count()
                .await
                .map_err(|_| "entry_count failed")?,
            2
        );

        let now_plus_five = super::current_unix_seconds().saturating_add(5);
        let dropped = store
            .sweep_expired(now_plus_five)
            .await
            .map_err(|_| "sweep failed")?;
        assert_eq!(dropped, 1);
        assert!(
            store
                .find_by_payment_id(&short.payment_id)
                .await
                .map_err(|_| "find failed")?
                .is_none()
        );
        assert!(
            store
                .find_by_payment_id(&long.payment_id)
                .await
                .map_err(|_| "find failed")?
                .is_some()
        );
        Ok(())
    }

    #[tokio::test]
    async fn idempotent_propose_returns_same_payment_id() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let registry = fixture_registry();
        let oracle = FixedTipOracle::fixture();
        let mut first = valid_request();
        first.idempotency_key = Some("order-abc-001".to_owned());
        let mut second = valid_request();
        second.idempotency_key = Some("order-abc-001".to_owned());

        let initial = propose(first, FIXTURE_JKT.to_owned(), &store, &registry, &oracle)
            .await
            .map_err(|_| "first propose must succeed")?;
        let replay = propose(second, FIXTURE_JKT.to_owned(), &store, &registry, &oracle)
            .await
            .map_err(|_| "replay propose must succeed")?;
        assert_eq!(initial.payment_id, replay.payment_id);
        assert_eq!(initial.payment_uri, replay.payment_uri);
        assert_eq!(
            store
                .entry_count()
                .await
                .map_err(|_| "entry_count failed")?,
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn idempotency_key_is_jkt_scoped() -> Result<(), &'static str> {
        // Same payee and same idempotency_key, but different DPoP keys
        // (different jkt) must allocate distinct payment_ids. This is
        // the regression gate for the Commit E (jkt, idempotency_key)
        // composite: a rival agent cannot replay another agent's
        // prepared row by guessing its idempotency_key.
        let store = PreparedTxCache::new();
        let registry = fixture_registry();
        let oracle = FixedTipOracle::fixture();
        let mut first = valid_request();
        first.idempotency_key = Some("order-001".to_owned());
        let mut second = valid_request();
        second.idempotency_key = Some("order-001".to_owned());

        let one = propose(first, FIXTURE_JKT.to_owned(), &store, &registry, &oracle)
            .await
            .map_err(|_| "first propose must succeed")?;
        let two = propose(
            second,
            ALTERNATE_FIXTURE_JKT.to_owned(),
            &store,
            &registry,
            &oracle,
        )
        .await
        .map_err(|_| "second propose must succeed")?;
        assert_ne!(one.payment_id, two.payment_id);
        assert_eq!(
            store
                .entry_count()
                .await
                .map_err(|_| "entry_count failed")?,
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn empty_idempotency_key_is_treated_as_absent() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let registry = fixture_registry();
        let oracle = FixedTipOracle::fixture();
        let mut first = valid_request();
        first.idempotency_key = Some("   ".to_owned());
        let mut second = valid_request();
        second.idempotency_key = Some(String::new());

        let a = propose(first, FIXTURE_JKT.to_owned(), &store, &registry, &oracle)
            .await
            .map_err(|_| "first propose must succeed")?;
        let b = propose(second, FIXTURE_JKT.to_owned(), &store, &registry, &oracle)
            .await
            .map_err(|_| "second propose must succeed")?;
        assert_ne!(a.payment_id, b.payment_id);
        assert_eq!(
            store
                .entry_count()
                .await
                .map_err(|_| "entry_count failed")?,
            2
        );
        Ok(())
    }
}
