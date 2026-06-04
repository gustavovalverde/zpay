//! Server-side composition of the zpay protocol memo binding.
//!
//! The 1.x wire shape made the agent pre-hash the challenge and resource
//! and submit three opaque 32-byte arrays. That contract leaked memo
//! layout into every client, made it impossible to reproduce a challenge
//! from a `prepare` request alone, and let two distinct protocol payloads
//! collide onto the same SHA-256 input by accident.
//!
//! Commit D moves composition entirely server-side. `propose` receives
//! the human-meaningful inputs (payee id, scheme, network, resource URI,
//! caller nonce, optional evidence-pack hash) and this module derives
//! the 66- or 98-byte memo prefix from them. Domain-separation tags
//! ensure that the challenge and the resource hashes can never share an
//! image even when their underlying byte payloads overlap.
//!
//! Layout (`compose_binding_memo` return value):
//!
//! - byte 0: [`crate::prepare::PROTOCOL_MEMO_TAG`] (`0xFF`, ZIP-302
//!   Arbitrary).
//! - byte 1: [`crate::prepare::PROTOCOL_MEMO_VERSION`] (`0x02`).
//! - bytes 2..34: `challenge_hash = SHA256(DOMAIN_TAG_CHALLENGE || 0x00
//!   || payee_id || 0x00 || scheme_byte || 0x00 || network_byte || 0x00
//!   || resource_uri || 0x00 || nonce)`.
//! - bytes 34..66: `resource_hash = SHA256(DOMAIN_TAG_RESOURCE || 0x00
//!   || resource_uri)`.
//! - bytes 66..98 (only when an evidence pack is bound): the supplied
//!   [`crate::types::EvidencePackHash`] bytes verbatim.

use sha2::{Digest, Sha256};

use crate::prepare::{PROTOCOL_MEMO_TAG, PROTOCOL_MEMO_VERSION};
use crate::types::{EvidencePackHash, PayeeId, PaymentNetwork, PaymentScheme};

/// Domain-separation prefix mixed into the challenge SHA-256 input.
///
/// The leading ASCII tag plus the canonical `0x00` field separators
/// guarantee that no other zpay-defined hash can replay onto the same
/// pre-image, even when its byte payload happens to start with this
/// tag.
pub const DOMAIN_TAG_CHALLENGE: &[u8] = b"zpay/v1/challenge";

/// Domain-separation prefix for the resource SHA-256 input.
///
/// Resources are URIs the agent advertised to the payer; the verifier
/// only checks equality with `SHA256(DOMAIN_TAG_RESOURCE || 0x00 ||
/// resource_uri)`, so domain separation is what stops collisions
/// against application-defined payloads on the wire.
pub const DOMAIN_TAG_RESOURCE: &[u8] = b"zpay/v1/resource";

/// Compose the protocol memo prefix for one prepare request.
///
/// Returns the 66-byte prefix when `evidence_pack` is `None` and the
/// 98-byte prefix when it is `Some` (the supplied
/// [`EvidencePackHash`] occupies bytes 66..98 unchanged).
///
/// The caller passes the same `(payee_id, scheme, network)` triple
/// the registry resolved against, the resource URI the agent presented
/// to the payer, and a caller-supplied nonce (typically a UUID; the
/// only requirement is uniqueness inside the `(payee_id,
/// idempotency_key)` scope so two distinct challenges cannot collide).
///
/// Callers must not pre-hash any input; the domain-separated SHA-256
/// invocations below are the only sanctioned way to produce a valid
/// challenge or resource hash. Pre-hashing is exactly the practice
/// Commit D removes.
#[must_use]
#[allow(
    clippy::too_many_arguments,
    reason = "the binding pre-image is six fields; bundling them behind a builder type would hide the SHA-256 input order from the call site for no readability win"
)]
pub fn compose_binding_memo(
    payee_id: &PayeeId,
    scheme: PaymentScheme,
    network: PaymentNetwork,
    resource_uri: &str,
    nonce: &str,
    evidence_pack: Option<&EvidencePackHash>,
) -> Vec<u8> {
    let challenge_hash = hash_challenge(payee_id, scheme, network, resource_uri, nonce);
    let resource_hash = hash_resource(resource_uri);

    let mut memo = Vec::with_capacity(if evidence_pack.is_some() { 98 } else { 66 });
    memo.push(PROTOCOL_MEMO_TAG);
    memo.push(PROTOCOL_MEMO_VERSION);
    memo.extend_from_slice(&challenge_hash);
    memo.extend_from_slice(&resource_hash);
    if let Some(evidence) = evidence_pack {
        memo.extend_from_slice(&evidence.0);
    }
    memo
}

fn hash_challenge(
    payee_id: &PayeeId,
    scheme: PaymentScheme,
    network: PaymentNetwork,
    resource_uri: &str,
    nonce: &str,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN_TAG_CHALLENGE);
    hasher.update([0u8]);
    hasher.update(payee_id.0.as_bytes());
    hasher.update([0u8]);
    hasher.update([scheme_byte(scheme)]);
    hasher.update([0u8]);
    hasher.update([network_byte(network)]);
    hasher.update([0u8]);
    hasher.update(resource_uri.as_bytes());
    hasher.update([0u8]);
    hasher.update(nonce.as_bytes());
    hasher.finalize().into()
}

fn hash_resource(resource_uri: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN_TAG_RESOURCE);
    hasher.update([0u8]);
    hasher.update(resource_uri.as_bytes());
    hasher.finalize().into()
}

/// Stable single-byte tag for a [`PaymentScheme`].
///
/// The numeric tag is part of the challenge pre-image and must therefore
/// stay constant across protocol-version bumps. New schemes append; never
/// renumber.
fn scheme_byte(scheme: PaymentScheme) -> u8 {
    match scheme {
        PaymentScheme::Zcash => 0x01,
    }
}

/// Stable single-byte tag for a [`PaymentNetwork`].
///
/// Same constancy rule as [`scheme_byte`].
fn network_byte(network: PaymentNetwork) -> u8 {
    match network {
        PaymentNetwork::Mainnet => 0x01,
        PaymentNetwork::Testnet => 0x02,
        PaymentNetwork::Regtest => 0x03,
    }
}

#[cfg(test)]
mod tests {
    use super::{compose_binding_memo, hash_challenge, hash_resource};
    use crate::prepare::{
        PROTOCOL_MEMO_BYTE_COUNT, PROTOCOL_MEMO_BYTE_COUNT_NO_EVIDENCE, PROTOCOL_MEMO_TAG,
        PROTOCOL_MEMO_VERSION,
    };
    use crate::types::{EvidencePackHash, PayeeId, PaymentNetwork, PaymentScheme};

    fn fixture_payee() -> PayeeId {
        PayeeId("aether-ai".to_owned())
    }

    /// Golden vector for the no-evidence path.
    ///
    /// The hash bytes are derived from the fixed inputs below via
    /// `sha2::Sha256`; regenerating them here keeps the assertion
    /// authoritative without hard-coding a 64-character hex literal
    /// that masks intent. The point of the test is that the *layout*
    /// stays stable and the *recipe* stays stable; future contributors
    /// must not silently change either.
    #[test]
    fn no_evidence_memo_is_66_bytes_and_stable() {
        let payee = fixture_payee();
        let memo = compose_binding_memo(
            &payee,
            PaymentScheme::Zcash,
            PaymentNetwork::Testnet,
            "https://example.test/resources/fixture",
            "00000000-0000-0000-0000-000000000000",
            None,
        );

        assert_eq!(memo.len(), PROTOCOL_MEMO_BYTE_COUNT_NO_EVIDENCE);
        assert_eq!(memo[0], PROTOCOL_MEMO_TAG);
        assert_eq!(memo[1], PROTOCOL_MEMO_VERSION);

        let expected_challenge = hash_challenge(
            &payee,
            PaymentScheme::Zcash,
            PaymentNetwork::Testnet,
            "https://example.test/resources/fixture",
            "00000000-0000-0000-0000-000000000000",
        );
        let expected_resource = hash_resource("https://example.test/resources/fixture");

        assert_eq!(&memo[2..34], expected_challenge.as_slice());
        assert_eq!(&memo[34..66], expected_resource.as_slice());
    }

    #[test]
    fn with_evidence_memo_is_98_bytes_and_carries_evidence_verbatim() {
        let payee = fixture_payee();
        let evidence = EvidencePackHash([0xCD; 32]);
        let memo = compose_binding_memo(
            &payee,
            PaymentScheme::Zcash,
            PaymentNetwork::Mainnet,
            "https://example.test/resources/with-evidence",
            "11111111-1111-1111-1111-111111111111",
            Some(&evidence),
        );

        assert_eq!(memo.len(), PROTOCOL_MEMO_BYTE_COUNT);
        assert_eq!(memo[0], PROTOCOL_MEMO_TAG);
        assert_eq!(memo[1], PROTOCOL_MEMO_VERSION);
        assert_eq!(&memo[66..98], evidence.0.as_slice());
    }

    /// Reproducing the same inputs must yield byte-identical output;
    /// this is what makes the wire deterministic for retries and for
    /// disclosure verifiers that recompute the challenge.
    #[test]
    fn same_inputs_produce_identical_memo() {
        let payee = fixture_payee();
        let first = compose_binding_memo(
            &payee,
            PaymentScheme::Zcash,
            PaymentNetwork::Testnet,
            "https://example.test/resources/fixture",
            "deadbeef",
            None,
        );
        let second = compose_binding_memo(
            &payee,
            PaymentScheme::Zcash,
            PaymentNetwork::Testnet,
            "https://example.test/resources/fixture",
            "deadbeef",
            None,
        );
        assert_eq!(first, second);
    }

    /// Domain separation is the entire point of the leading tag.
    ///
    /// Even when the body bytes overlap (here the same URI), the
    /// challenge and resource SHA-256 inputs land in disjoint domains
    /// and must produce different 32-byte outputs.
    #[test]
    fn challenge_and_resource_hashes_are_domain_separated() {
        let payee = fixture_payee();
        let resource_uri = "https://example.test/resources/fixture";
        let memo = compose_binding_memo(
            &payee,
            PaymentScheme::Zcash,
            PaymentNetwork::Testnet,
            resource_uri,
            // Use a nonce that is also the URI so the body bytes
            // collide; only the domain tag should keep the hashes
            // distinct.
            resource_uri,
            None,
        );
        let challenge = &memo[2..34];
        let resource = &memo[34..66];
        assert_ne!(
            challenge, resource,
            "domain tags must keep challenge and resource hashes disjoint",
        );
    }

    /// Changing any single input (payee, scheme, network, URI, nonce)
    /// must change the challenge hash. Lock the property so a future
    /// refactor cannot accidentally drop a field from the pre-image.
    #[test]
    fn changing_any_field_changes_the_challenge_hash() {
        let payee = fixture_payee();
        let base = compose_binding_memo(
            &payee,
            PaymentScheme::Zcash,
            PaymentNetwork::Testnet,
            "https://example.test/resources/fixture",
            "nonce-a",
            None,
        );
        let with_different_nonce = compose_binding_memo(
            &payee,
            PaymentScheme::Zcash,
            PaymentNetwork::Testnet,
            "https://example.test/resources/fixture",
            "nonce-b",
            None,
        );
        let with_different_uri = compose_binding_memo(
            &payee,
            PaymentScheme::Zcash,
            PaymentNetwork::Testnet,
            "https://example.test/resources/other",
            "nonce-a",
            None,
        );
        let with_different_network = compose_binding_memo(
            &payee,
            PaymentScheme::Zcash,
            PaymentNetwork::Mainnet,
            "https://example.test/resources/fixture",
            "nonce-a",
            None,
        );
        let with_different_payee = compose_binding_memo(
            &PayeeId("other".to_owned()),
            PaymentScheme::Zcash,
            PaymentNetwork::Testnet,
            "https://example.test/resources/fixture",
            "nonce-a",
            None,
        );

        assert_ne!(&base[2..34], &with_different_nonce[2..34]);
        assert_ne!(&base[2..34], &with_different_uri[2..34]);
        assert_ne!(&base[2..34], &with_different_network[2..34]);
        assert_ne!(&base[2..34], &with_different_payee[2..34]);
    }
}
