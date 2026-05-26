//! Prepare a payment: compose the recipient URI, the protocol memo, and the
//! `payment_id` that the agent attaches to its merchant request.
//!
//! Two surfaces live here:
//!
//! - The pure [`compose_protocol_memo`] function that lays out the 98-byte
//!   ZIP-302 memo prefix (protocol byte + version + three 32-byte hashes).
//!   Used by every prepare path; safe to call without any cache.
//! - The [`PrepareRequest`] / [`Preparation`] types and the [`propose`]
//!   function that combine the memo with a server-issued `payment_id` and
//!   write the result to a [`PreparedTxCache`].
//!
//! In-memory cache only: prepared transactions live for the TTL window and
//! are dropped on process restart. A future change can swap the cache for a
//! libSQL-backed implementation without touching the [`PreparedTxCache`]
//! trait surface.

use std::future::Future;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::store::StoreError;
use crate::types::{
    EvidencePackHash, MerchantId, PaymentId, PaymentNetwork, PaymentScheme, Zatoshis,
};

#[cfg(feature = "in_memory")]
use std::collections::HashMap;
#[cfg(feature = "in_memory")]
use parking_lot::Mutex;

/// Default validity window for a prepared transaction.
///
/// Five minutes matches the PRD-42 prepared-tx cache TTL default. Callers
/// can override per-request via [`PrepareRequest::validity_seconds`].
pub const DEFAULT_VALIDITY_SECONDS: u64 = 300;

/// Length of the protocol-defined memo prefix in bytes.
///
/// Layout: protocol byte (1) + version byte (1) + challenge hash (32) +
/// resource hash (32) + evidence-pack hash (32). See PRD-42 Decision 11.
///
/// On chain this 98-byte prefix occupies the leading region of a 512-byte
/// ZIP-302 [`Arbitrary`] memo: the wallet writes the prefix, zero-pads to
/// 511 bytes, and the protocol tag itself becomes byte 0 of the 512-byte
/// container.
///
/// [`Arbitrary`]: https://zips.z.cash/zip-0302#arbitrary
pub const PROTOCOL_MEMO_BYTE_COUNT: usize = 98;

/// Leading byte of the zpay protocol memo.
///
/// ZIP-302 (`zips/zip-0302.rst:58-61`) reserves `0xFF` for arbitrary
/// application-defined payloads, with the remaining 511 bytes
/// unconstrained. Any value in `0x00..=0xF4` would force the rest of the
/// memo to parse as valid UTF-8, which our 96 bytes of hash material
/// cannot satisfy.
pub const PROTOCOL_MEMO_TAG: u8 = 0xff;

/// Current protocol memo layout version. Bumped when the layout changes; a
/// memo with an unknown version is rejected by the disclosure verifier.
pub const PROTOCOL_MEMO_VERSION: u8 = 0x01;

/// 32-byte hash binding the payment to a specific request the agent
/// presented to the merchant (typically a SHA-256 over merchant id +
/// resource URI + nonce).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChallengeHash(pub [u8; 32]);

/// 32-byte hash of the merchant resource URI the agent is paying for.
/// Treated as an opaque tag; the verifier only checks equality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResourceHash(pub [u8; 32]);

/// Input to [`propose`]. Composed by a wire adapter from a protocol-specific
/// request shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareRequest {
    /// Merchant whose `accepts[]` template applies.
    pub merchant_id: MerchantId,
    /// Network the payment will settle on.
    pub network: PaymentNetwork,
    /// Payment scheme advertised in the merchant's `accepts[]` entry.
    pub scheme: PaymentScheme,
    /// Unified address the merchant expects to receive payment at. Format
    /// validation is the wire adapter's responsibility; zpay-core stores
    /// the encoded string verbatim.
    pub recipient_unified_address: String,
    /// Amount the merchant expects in zatoshis.
    pub amount_zat: Zatoshis,
    /// Hash binding this payment to a specific merchant challenge.
    pub challenge_hash: ChallengeHash,
    /// Hash of the merchant resource the agent is paying for.
    pub resource_hash: ResourceHash,
    /// Evidence-pack hash binding this payment to a zentity proof set.
    pub evidence_pack_hash: EvidencePackHash,
    /// Block height after which the prepared transaction cannot settle.
    pub expiry_height: u32,
    /// Wall-clock validity window in seconds. Defaults to
    /// [`DEFAULT_VALIDITY_SECONDS`] when omitted.
    #[serde(default)]
    pub validity_seconds: Option<u64>,
    /// Caller-supplied idempotency key. When set, a second `propose`
    /// call with the same `(merchant_id, idempotency_key)` pair returns
    /// the original preparation instead of allocating a new one.
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
    /// 98-byte protocol memo content (protocol byte + version + three
    /// 32-byte hashes).
    pub memo_bytes: Vec<u8>,
    /// Block height after which this preparation cannot be settled.
    pub expiry_height: u32,
}

/// Errors that can arise during preparation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PrepareError {
    /// Recipient address was empty or otherwise not acceptable for the
    /// configured scheme. Retry posture: `not_retryable`.
    #[error("recipient_unified_address must be a non-empty ZIP-316 unified address")]
    RecipientInvalid,
    /// Caller asked for a payment with zero zatoshis. Retry posture:
    /// `not_retryable`.
    #[error("amount_zat must be greater than zero")]
    AmountZero,
    /// Caller's expiry height is at or below the lower bound. Retry posture:
    /// `not_retryable`.
    #[error("expiry_height must be greater than zero")]
    ExpiryHeightInvalid,
    /// The prepared-tx store could not complete the read or write.
    /// Retry posture: inherits from the surfaced [`StoreError`] variant.
    #[error("prepared-tx store failure: {0}")]
    Storage(#[from] StoreError),
}

/// Compose the 98-byte ZIP-302 memo prefix that binds the payment to the
/// challenge, resource, and evidence pack.
///
/// The layout is fixed: byte 0 is [`PROTOCOL_MEMO_TAG`], byte 1 is
/// [`PROTOCOL_MEMO_VERSION`], bytes 2..34 are `challenge_hash`, bytes
/// 34..66 are `resource_hash`, bytes 66..98 are `evidence_pack_hash`.
#[must_use]
pub fn compose_protocol_memo(
    challenge_hash: ChallengeHash,
    resource_hash: ResourceHash,
    evidence_pack_hash: EvidencePackHash,
) -> [u8; PROTOCOL_MEMO_BYTE_COUNT] {
    let mut bytes = [0u8; PROTOCOL_MEMO_BYTE_COUNT];
    bytes[0] = PROTOCOL_MEMO_TAG;
    bytes[1] = PROTOCOL_MEMO_VERSION;
    bytes[2..34].copy_from_slice(&challenge_hash.0);
    bytes[34..66].copy_from_slice(&resource_hash.0);
    bytes[66..98].copy_from_slice(&evidence_pack_hash.0);
    bytes
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
    /// Merchant the prepare request targeted.
    pub merchant_id: MerchantId,
    /// Network the payment is bound to.
    pub network: PaymentNetwork,
    /// Recipient address the user's wallet must pay.
    pub recipient_unified_address: String,
    /// Amount the user's wallet must pay.
    pub amount_zat: Zatoshis,
    /// Wall-clock deadline after which the sweeper removes the entry.
    pub expires_at_unix_seconds: u64,
    /// Caller-supplied idempotency key, scoped to `merchant_id`. None
    /// when the prepare request did not carry one.
    pub idempotency_key: Option<String>,
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
    /// `(merchant_id, idempotency_key)` pair so a retried prepare
    /// returns the original entry instead of allocating a new one.
    fn insert(
        &self,
        entry: PreparedTxEntry,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Look up a prepared-tx entry by `payment_id`.
    fn find_by_payment_id(
        &self,
        payment_id: &PaymentId,
    ) -> impl Future<Output = Result<Option<PreparedTxEntry>, StoreError>> + Send;

    /// Resolve a `(merchant_id, idempotency_key)` to the prepared entry
    /// the first call produced. Returns `None` when no entry exists or
    /// the prior entry has already expired.
    fn find_by_idempotency(
        &self,
        merchant_id: &MerchantId,
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
/// Storage is keyed by [`PaymentId`] with a secondary `(merchant_id,
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
    by_idempotency: HashMap<(MerchantId, String), PaymentId>,
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
                (entry.merchant_id.clone(), key),
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
        merchant_id: &MerchantId,
        idempotency_key: &str,
    ) -> Result<Option<PreparedTxEntry>, StoreError> {
        let guard = self.inner.lock();
        let Some(payment_id) = guard
            .by_idempotency
            .get(&(merchant_id.clone(), idempotency_key.to_owned()))
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
                .remove(&(entry.merchant_id.clone(), key.clone()));
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
/// is the 98-byte protocol prefix (see [`compose_protocol_memo`]); the
/// agent transmits it to the user's wallet alongside the `payment_uri`.
///
/// # Errors
///
/// - [`PrepareError::RecipientInvalid`] when `recipient_unified_address`
///   is empty.
/// - [`PrepareError::AmountZero`] when `amount_zat` is zero.
/// - [`PrepareError::ExpiryHeightInvalid`] when `expiry_height` is zero.
/// - [`PrepareError::Storage`] when the underlying
///   [`PreparedTxStore`] surfaces a [`crate::store::StoreError`].
pub async fn propose<S: PreparedTxStore + ?Sized>(
    request: PrepareRequest,
    store: &S,
) -> Result<Preparation, PrepareError> {
    if request.recipient_unified_address.trim().is_empty() {
        return Err(PrepareError::RecipientInvalid);
    }
    if request.amount_zat.0 == 0 {
        return Err(PrepareError::AmountZero);
    }
    if request.expiry_height == 0 {
        return Err(PrepareError::ExpiryHeightInvalid);
    }

    // Idempotency replay: if a prior prepare with the same (merchant,
    // idempotency_key) is still in the store, hand the agent the same
    // preparation. Expiry sweep already drops stale entries so a hit here
    // is always within the validity window.
    if let Some(key) = request
        .idempotency_key
        .as_ref()
        .filter(|raw| !raw.trim().is_empty())
        && let Some(prior) = store
            .find_by_idempotency(&request.merchant_id, key)
            .await
            .map_err(PrepareError::Storage)?
    {
        return Ok(prior.preparation);
    }

    let memo = compose_protocol_memo(
        request.challenge_hash,
        request.resource_hash,
        request.evidence_pack_hash,
    );
    let payment_uri = compose_payment_uri(
        &request.recipient_unified_address,
        request.amount_zat,
        &memo,
    );

    let preparation = Preparation {
        payment_id: PaymentId::new(),
        payment_uri,
        memo_bytes: memo.to_vec(),
        expiry_height: request.expiry_height,
    };

    let validity_seconds = request
        .validity_seconds
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_VALIDITY_SECONDS);
    let expires_at_unix_seconds = current_unix_seconds().saturating_add(validity_seconds);

    store
        .insert(PreparedTxEntry {
            preparation: preparation.clone(),
            merchant_id: request.merchant_id,
            network: request.network,
            recipient_unified_address: request.recipient_unified_address,
            amount_zat: request.amount_zat,
            expires_at_unix_seconds,
            idempotency_key: request
                .idempotency_key
                .filter(|raw| !raw.trim().is_empty()),
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
fn compose_payment_uri(
    recipient_unified_address: &str,
    amount_zat: Zatoshis,
    memo: &[u8; PROTOCOL_MEMO_BYTE_COUNT],
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
mod tests {
    use super::{
        ChallengeHash, PROTOCOL_MEMO_BYTE_COUNT, PROTOCOL_MEMO_TAG, PROTOCOL_MEMO_VERSION,
        PrepareError, PrepareRequest, PreparedTxCache, PreparedTxStore, ResourceHash,
        compose_protocol_memo, format_zec_from_zat, propose,
    };
    use crate::types::{EvidencePackHash, MerchantId, PaymentNetwork, PaymentScheme, Zatoshis};

    fn valid_request() -> PrepareRequest {
        PrepareRequest {
            merchant_id: MerchantId("aether-ai".to_owned()),
            network: PaymentNetwork::Testnet,
            scheme: PaymentScheme::Zcash,
            recipient_unified_address: "utest1exampleaddress".to_owned(),
            amount_zat: Zatoshis(50_000),
            challenge_hash: ChallengeHash([0x11; 32]),
            resource_hash: ResourceHash([0x22; 32]),
            evidence_pack_hash: EvidencePackHash([0x33; 32]),
            expiry_height: 3_217_900,
            validity_seconds: None,
            idempotency_key: None,
        }
    }

    #[test]
    fn protocol_memo_layout_is_98_bytes() {
        let memo = compose_protocol_memo(
            ChallengeHash([1u8; 32]),
            ResourceHash([2u8; 32]),
            EvidencePackHash([3u8; 32]),
        );
        assert_eq!(memo.len(), PROTOCOL_MEMO_BYTE_COUNT);
        assert_eq!(memo[0], PROTOCOL_MEMO_TAG);
        assert_eq!(memo[1], PROTOCOL_MEMO_VERSION);
        assert_eq!(&memo[2..34], &[1u8; 32]);
        assert_eq!(&memo[34..66], &[2u8; 32]);
        assert_eq!(&memo[66..98], &[3u8; 32]);
    }

    #[tokio::test]
    async fn propose_inserts_into_store_and_returns_full_preparation()
    -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let preparation = propose(valid_request(), &store)
            .await
            .map_err(|_| "propose must accept valid input")?;

        assert_eq!(preparation.memo_bytes.len(), PROTOCOL_MEMO_BYTE_COUNT);
        assert_eq!(preparation.expiry_height, 3_217_900);
        assert!(
            preparation
                .payment_uri
                .starts_with("zcash:utest1exampleaddress?")
        );
        assert!(preparation.payment_uri.contains("amount=0.0005"));
        assert!(preparation.payment_uri.contains("memo="));
        assert_eq!(
            store.entry_count().await.map_err(|_| "entry_count failed")?,
            1
        );
        let cached = store
            .find_by_payment_id(&preparation.payment_id)
            .await
            .map_err(|_| "find failed")?
            .ok_or("store must echo the prepared payment_id")?;
        assert_eq!(cached.amount_zat, Zatoshis(50_000));
        assert_eq!(cached.recipient_unified_address, "utest1exampleaddress");
        Ok(())
    }

    #[tokio::test]
    async fn propose_refuses_empty_recipient() -> Result<(), &'static str> {
        let mut request = valid_request();
        request.recipient_unified_address = "   ".to_owned();
        let store = PreparedTxCache::new();
        let outcome = propose(request, &store).await;
        assert!(matches!(outcome, Err(PrepareError::RecipientInvalid)));
        assert_eq!(
            store.entry_count().await.map_err(|_| "entry_count failed")?,
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn propose_refuses_zero_amount() -> Result<(), &'static str> {
        let mut request = valid_request();
        request.amount_zat = Zatoshis(0);
        let store = PreparedTxCache::new();
        let outcome = propose(request, &store).await;
        assert!(matches!(outcome, Err(PrepareError::AmountZero)));
        assert_eq!(
            store.entry_count().await.map_err(|_| "entry_count failed")?,
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn propose_refuses_zero_expiry_height() -> Result<(), &'static str> {
        let mut request = valid_request();
        request.expiry_height = 0;
        let store = PreparedTxCache::new();
        let outcome = propose(request, &store).await;
        assert!(matches!(outcome, Err(PrepareError::ExpiryHeightInvalid)));
        assert_eq!(
            store.entry_count().await.map_err(|_| "entry_count failed")?,
            0
        );
        Ok(())
    }

    #[tokio::test]
    async fn store_remove_makes_payment_id_unfindable() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let preparation = propose(valid_request(), &store)
            .await
            .map_err(|_| "propose must accept valid input")?;
        let removed = store
            .remove(&preparation.payment_id)
            .await
            .map_err(|_| "remove failed")?;
        assert!(removed.is_some());
        assert_eq!(
            store.entry_count().await.map_err(|_| "entry_count failed")?,
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
    async fn sweep_drops_expired_entries_and_keeps_active_ones() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let mut short_lived = valid_request();
        short_lived.validity_seconds = Some(1);
        let mut long_lived = valid_request();
        long_lived.validity_seconds = Some(3600);

        let short = propose(short_lived, &store)
            .await
            .map_err(|_| "short propose must succeed")?;
        let long = propose(long_lived, &store)
            .await
            .map_err(|_| "long propose must succeed")?;
        assert_eq!(
            store.entry_count().await.map_err(|_| "entry_count failed")?,
            2
        );

        // Pick a "now" that is past the short-lived entry's expiry but not
        // the long-lived one's. The short entry uses validity_seconds = 1
        // so its expiry sits about now+1; sweep at now+5 drops it.
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
        let mut first = valid_request();
        first.idempotency_key = Some("order-abc-001".to_owned());
        let mut second = valid_request();
        second.idempotency_key = Some("order-abc-001".to_owned());

        let initial = propose(first, &store)
            .await
            .map_err(|_| "first propose must succeed")?;
        let replay = propose(second, &store)
            .await
            .map_err(|_| "replay propose must succeed")?;
        assert_eq!(initial.payment_id, replay.payment_id);
        assert_eq!(initial.payment_uri, replay.payment_uri);
        assert_eq!(
            store.entry_count().await.map_err(|_| "entry_count failed")?,
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn idempotency_key_is_merchant_scoped() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let mut a = valid_request();
        a.merchant_id = MerchantId("aether-ai".to_owned());
        a.idempotency_key = Some("order-001".to_owned());
        let mut b = valid_request();
        b.merchant_id = MerchantId("demo-rp".to_owned());
        b.idempotency_key = Some("order-001".to_owned());

        let first = propose(a, &store)
            .await
            .map_err(|_| "first propose must succeed")?;
        let second = propose(b, &store)
            .await
            .map_err(|_| "second propose must succeed")?;
        assert_ne!(first.payment_id, second.payment_id);
        assert_eq!(
            store.entry_count().await.map_err(|_| "entry_count failed")?,
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn empty_idempotency_key_is_treated_as_absent() -> Result<(), &'static str> {
        let store = PreparedTxCache::new();
        let mut first = valid_request();
        first.idempotency_key = Some("   ".to_owned());
        let mut second = valid_request();
        second.idempotency_key = Some(String::new());

        let a = propose(first, &store)
            .await
            .map_err(|_| "first propose must succeed")?;
        let b = propose(second, &store)
            .await
            .map_err(|_| "second propose must succeed")?;
        assert_ne!(a.payment_id, b.payment_id);
        assert_eq!(
            store.entry_count().await.map_err(|_| "entry_count failed")?,
            2
        );
        Ok(())
    }
}
