//! BLAKE2b-256 digest construction for ZIP-311 payment disclosures.
//!
//! Per [ZIP-311](https://zips.z.cash/zip-0311), the disclosure digest is:
//!
//! ```text
//! digest = BLAKE2b-256(
//!     personalization = b"ZIP311Signed" || coinType_LE32,
//!     input           = unsignedPaymentDisclosure,
//! )
//! ```
//!
//! The personalization is exactly 16 bytes: the 12-byte ASCII string
//! `"ZIP311Signed"` followed by the 4-byte little-endian encoding of
//! the SLIP-44 coin-type index. Mainnet Zcash is `133`; testnet is `1`.
//! The verifier MUST use the configured network rather than something
//! parsed from the disclosure itself: that prevents a maliciously
//! crafted disclosure from pivoting the verifier into an
//! attacker-controlled network.
//!
//! The digest binds every Sapling spend's `spendAuthSig`. The
//! transparent inputs use a separate BIP-322-legacy preimage (see
//! [`super::transparent`]); the shared `BLAKE2b` digest is NOT what the
//! transparent signature signs. Both keep their own preimage rules and
//! both are bound to the same disclosure `msg`, which is what keeps a
//! multi-pool disclosure from being trivially separable.

use blake2b_simd::Params;

use crate::types::PaymentNetwork;
use crate::verify::parse_zip311::{Zip311Disclosure, encode_unsigned};

/// Canonical personalization tag prefix from the ZIP-311 spec.
const PERSONALIZATION_TAG: &[u8; 12] = b"ZIP311Signed";

/// SLIP-44 coin type for Zcash mainnet (index form, not hardened).
const COIN_TYPE_MAINNET: u32 = 133;

/// SLIP-44 coin type for Zcash testnet (index form, not hardened).
///
/// Per BIP-44 the testnet index is `1`, shared across most chains'
/// testnets. ZIP-311 verifiers running on regtest must pin
/// [`PaymentNetwork::Testnet`]; regtest carries no distinct SLIP-44
/// number and the digest is computed against the testnet tag.
const COIN_TYPE_TESTNET: u32 = 1;

/// Length of the BLAKE2b-256 digest output, in bytes.
pub const DIGEST_LEN: usize = 32;

/// Compute the ZIP-311 BLAKE2b-256 digest for a disclosure under a
/// configured network.
///
/// The digest binds:
///
/// - The network-specific personalization tag (so a disclosure
///   produced for mainnet does NOT verify under testnet).
/// - The full unsigned form of the disclosure (every field except
///   per-input signatures and per-spend spend-auth sigs).
///
/// The result is the input every Sapling `spendAuthSig` MUST sign and
/// the input the local verifier reconstructs to compare.
#[must_use]
pub fn compute(network: PaymentNetwork, disclosure: &Zip311Disclosure) -> [u8; DIGEST_LEN] {
    let coin_type = coin_type_for(network);
    let mut personalization = [0u8; 16];
    personalization[..12].copy_from_slice(PERSONALIZATION_TAG);
    personalization[12..].copy_from_slice(&coin_type.to_le_bytes());

    let unsigned = encode_unsigned(disclosure);
    let hash = Params::new()
        .hash_length(DIGEST_LEN)
        .personal(&personalization)
        .hash(&unsigned);

    let mut out = [0u8; DIGEST_LEN];
    out.copy_from_slice(hash.as_bytes());
    out
}

/// Resolve the SLIP-44 coin type bytes for a payment network.
///
/// Per ZIP-311: "Let coinType be the 4-byte little-endian encoding of
/// the coin type in its index form, not its hardened form (i.e. 133
/// for mainnet Zcash)".
#[must_use]
const fn coin_type_for(network: PaymentNetwork) -> u32 {
    // Regtest has no distinct SLIP-44 number; falls under the testnet
    // tag. Future PaymentNetwork variants pin to testnet too, so the
    // verifier fails closed: a digest that does not match the
    // on-chain signature bubbles up as
    // `CryptographicVerdict::InvalidSignature` rather than silently
    // accepting under the wrong personalization.
    if matches!(network, PaymentNetwork::Mainnet) {
        COIN_TYPE_MAINNET
    } else {
        COIN_TYPE_TESTNET
    }
}

#[cfg(test)]
mod tests {
    use super::{COIN_TYPE_MAINNET, COIN_TYPE_TESTNET, compute};
    use crate::types::PaymentNetwork;
    use crate::verify::parse_zip311::{ZIP311_VERSION_V1, Zip311Disclosure, Zip311TransparentInput};

    fn fixture() -> Zip311Disclosure {
        Zip311Disclosure {
            version: ZIP311_VERSION_V1,
            txid: [0x42u8; 32],
            message: b"hello".to_vec(),
            transparent_inputs: vec![Zip311TransparentInput {
                index: 0,
                signature: vec![0xAAu8; 65],
            }],
            sapling_spends: Vec::new(),
            sapling_outputs: Vec::new(),
        }
    }

    #[test]
    fn digest_is_stable_for_mainnet() {
        let d = fixture();
        let a = compute(PaymentNetwork::Mainnet, &d);
        let b = compute(PaymentNetwork::Mainnet, &d);
        assert_eq!(a, b);
    }

    #[test]
    fn digest_is_stable_for_testnet() {
        let d = fixture();
        let a = compute(PaymentNetwork::Testnet, &d);
        let b = compute(PaymentNetwork::Testnet, &d);
        assert_eq!(a, b);
    }

    /// Mainnet vs. testnet must produce DIFFERENT digests.
    ///
    /// This is the property that stops a mainnet disclosure from
    /// accidentally verifying under testnet (which would otherwise be
    /// a silent network-confusion attack).
    #[test]
    fn mainnet_and_testnet_diverge() {
        let d = fixture();
        let mainnet = compute(PaymentNetwork::Mainnet, &d);
        let testnet = compute(PaymentNetwork::Testnet, &d);
        assert_ne!(mainnet, testnet);
    }

    #[test]
    fn coin_type_constants_match_slip44() {
        assert_eq!(COIN_TYPE_MAINNET, 133);
        assert_eq!(COIN_TYPE_TESTNET, 1);
    }

    /// Two disclosures that differ only in the `msg` field must produce
    /// different digests: that is the property that makes the verifier
    /// challenge-response binding meaningful.
    #[test]
    fn different_messages_produce_different_digests() {
        let mut a = fixture();
        a.message = b"challenge-a".to_vec();
        let mut b = fixture();
        b.message = b"challenge-b".to_vec();
        let digest_a = compute(PaymentNetwork::Mainnet, &a);
        let digest_b = compute(PaymentNetwork::Mainnet, &b);
        assert_ne!(digest_a, digest_b);
    }

    /// The digest must NOT change if the per-input signature changes.
    ///
    /// The unsigned form strips it before hashing, so two disclosures
    /// that differ only by their signature bytes must produce the
    /// same digest. This is what makes the digest a valid sign-target.
    #[test]
    fn digest_is_invariant_under_signature_changes() {
        let mut a = fixture();
        a.transparent_inputs[0].signature = vec![0x01u8; 65];
        let mut b = fixture();
        b.transparent_inputs[0].signature = vec![0x02u8; 65];
        let digest_a = compute(PaymentNetwork::Mainnet, &a);
        let digest_b = compute(PaymentNetwork::Mainnet, &b);
        assert_eq!(digest_a, digest_b);
    }
}
