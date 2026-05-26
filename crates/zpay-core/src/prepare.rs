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

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::types::{
    EvidencePackHash, MerchantId, PaymentId, PaymentNetwork, PaymentScheme, Zatoshis,
};

/// Default validity window for a prepared transaction.
///
/// Five minutes matches the PRD-42 prepared-tx cache TTL default. Callers
/// can override per-request via [`PrepareRequest::validity_seconds`].
pub const DEFAULT_VALIDITY_SECONDS: u64 = 300;

/// Length of the protocol-defined ZIP-302 memo prefix in bytes.
///
/// Layout: protocol byte (1) + version byte (1) + challenge hash (32) +
/// resource hash (32) + evidence-pack hash (32). See PRD-42 Decision 11.
pub const PROTOCOL_MEMO_BYTE_COUNT: usize = 98;

/// Sentinel byte that prefixes every zpay-issued memo. Lets callers
/// distinguish zpay protocol memos from arbitrary user memos at a glance.
pub const PROTOCOL_MEMO_TAG: u8 = 0x5a; // 'Z' in ASCII

/// Current protocol memo layout version. Bumped when the layout changes; a
/// memo with an unknown version is rejected at settle time.
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

/// In-memory cache for prepared transactions awaiting settlement.
///
/// Storage is keyed by [`PaymentId`]. Entries carry the full prepared
/// payload plus the network, merchant, and expiry-height context that
/// settle-time validation reads. Thread-safe via [`parking_lot::Mutex`].
#[derive(Debug, Default)]
pub struct PreparedTxCache {
    entries: Mutex<HashMap<PaymentId, PreparedTxEntry>>,
}

/// Cached prepared transaction.
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
}

impl PreparedTxCache {
    /// Create a fresh empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a prepared-tx entry. Replaces any existing entry under the
    /// same `payment_id` (caller's idempotency key prevents collisions in
    /// practice; the replacement is the safe outcome on duplicate
    /// generation).
    pub fn insert(&self, entry: PreparedTxEntry) {
        let mut guard = self.entries.lock();
        guard.insert(entry.preparation.payment_id.clone(), entry);
    }

    /// Look up a prepared-tx entry by `payment_id`. Returns `None` when the
    /// entry has been removed by settle or expired by a sweep.
    #[must_use]
    pub fn find(&self, payment_id: &PaymentId) -> Option<PreparedTxEntry> {
        let guard = self.entries.lock();
        guard.get(payment_id).cloned()
    }

    /// Remove the prepared-tx entry for `payment_id`. Returns the removed
    /// entry when present; settle calls this on the success path to enforce
    /// fire-once semantics.
    pub fn remove(&self, payment_id: &PaymentId) -> Option<PreparedTxEntry> {
        let mut guard = self.entries.lock();
        guard.remove(payment_id)
    }

    /// Number of entries currently cached.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        let guard = self.entries.lock();
        guard.len()
    }

    /// Remove every entry whose `expires_at_unix_seconds` is at or before
    /// `now_unix_seconds`. Returns the number of entries dropped so a
    /// background sweeper can record useful telemetry.
    pub fn sweep_expired(&self, now_unix_seconds: u64) -> usize {
        let mut guard = self.entries.lock();
        let before = guard.len();
        guard.retain(|_, entry| entry.expires_at_unix_seconds > now_unix_seconds);
        before - guard.len()
    }
}

/// Compose a [`Preparation`] from a [`PrepareRequest`] and insert it into the
/// cache.
///
/// The returned `payment_id` is a fresh ULID; the caller pairs it with the
/// agent's DPoP thumbprint at the wire boundary. The composed `memo_bytes`
/// is the 98-byte protocol prefix (see [`compose_protocol_memo`]); the
/// agent transmits it to the user's wallet alongside the `payment_uri`.
///
/// # Errors
///
/// Returns [`PrepareError::RecipientInvalid`] when `recipient_unified_address`
/// is empty, [`PrepareError::AmountZero`] when `amount_zat` is zero, and
/// [`PrepareError::ExpiryHeightInvalid`] when `expiry_height` is zero.
pub fn propose(
    request: PrepareRequest,
    cache: &PreparedTxCache,
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

    cache.insert(PreparedTxEntry {
        preparation: preparation.clone(),
        merchant_id: request.merchant_id,
        network: request.network,
        recipient_unified_address: request.recipient_unified_address,
        amount_zat: request.amount_zat,
        expires_at_unix_seconds,
    });

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
        PrepareError, PrepareRequest, PreparedTxCache, ResourceHash, compose_protocol_memo,
        format_zec_from_zat, propose,
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

    #[test]
    fn propose_inserts_into_cache_and_returns_full_preparation() -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let preparation =
            propose(valid_request(), &cache).map_err(|_| "propose must accept valid input")?;

        assert_eq!(preparation.memo_bytes.len(), PROTOCOL_MEMO_BYTE_COUNT);
        assert_eq!(preparation.expiry_height, 3_217_900);
        assert!(
            preparation
                .payment_uri
                .starts_with("zcash:utest1exampleaddress?")
        );
        assert!(preparation.payment_uri.contains("amount=0.0005"));
        assert!(preparation.payment_uri.contains("memo="));
        assert_eq!(cache.entry_count(), 1);
        let cached = cache
            .find(&preparation.payment_id)
            .ok_or("cache must echo the prepared payment_id")?;
        assert_eq!(cached.amount_zat, Zatoshis(50_000));
        assert_eq!(cached.recipient_unified_address, "utest1exampleaddress");
        Ok(())
    }

    #[test]
    fn propose_refuses_empty_recipient() {
        let mut request = valid_request();
        request.recipient_unified_address = "   ".to_owned();
        let cache = PreparedTxCache::new();
        let outcome = propose(request, &cache);
        assert!(matches!(outcome, Err(PrepareError::RecipientInvalid)));
        assert_eq!(cache.entry_count(), 0);
    }

    #[test]
    fn propose_refuses_zero_amount() {
        let mut request = valid_request();
        request.amount_zat = Zatoshis(0);
        let cache = PreparedTxCache::new();
        let outcome = propose(request, &cache);
        assert!(matches!(outcome, Err(PrepareError::AmountZero)));
        assert_eq!(cache.entry_count(), 0);
    }

    #[test]
    fn propose_refuses_zero_expiry_height() {
        let mut request = valid_request();
        request.expiry_height = 0;
        let cache = PreparedTxCache::new();
        let outcome = propose(request, &cache);
        assert!(matches!(outcome, Err(PrepareError::ExpiryHeightInvalid)));
        assert_eq!(cache.entry_count(), 0);
    }

    #[test]
    fn cache_remove_makes_payment_id_unfindable() -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let preparation =
            propose(valid_request(), &cache).map_err(|_| "propose must accept valid input")?;
        let removed = cache.remove(&preparation.payment_id);
        assert!(removed.is_some());
        assert_eq!(cache.entry_count(), 0);
        assert!(cache.find(&preparation.payment_id).is_none());
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

    #[test]
    fn sweep_drops_expired_entries_and_keeps_active_ones() -> Result<(), &'static str> {
        let cache = PreparedTxCache::new();
        let mut short_lived = valid_request();
        short_lived.validity_seconds = Some(1);
        let mut long_lived = valid_request();
        long_lived.validity_seconds = Some(3600);

        let short = propose(short_lived, &cache).map_err(|_| "short propose must succeed")?;
        let long = propose(long_lived, &cache).map_err(|_| "long propose must succeed")?;
        assert_eq!(cache.entry_count(), 2);

        // Pick a "now" that is past the short-lived entry's expiry but not
        // the long-lived one's. The short entry uses validity_seconds = 1
        // so its expiry sits about now+1; sweep at now+5 drops it.
        let now_plus_five = super::current_unix_seconds().saturating_add(5);
        let dropped = cache.sweep_expired(now_plus_five);
        assert_eq!(dropped, 1);
        assert!(cache.find(&short.payment_id).is_none());
        assert!(cache.find(&long.payment_id).is_some());
        Ok(())
    }
}
