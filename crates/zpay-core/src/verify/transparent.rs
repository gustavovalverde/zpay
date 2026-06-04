//! BIP-322-legacy verifier for transparent ZIP-311 disclosures.
//!
//! ZIP-311 binds transparent inputs through the BIP-322 "legacy"
//! signature format, which is the classic bitcoind `signmessage`
//! preimage:
//!
//! ```text
//! magic       = b"Bitcoin Signed Message:\n"
//! preimage    = compact_size(len(magic)) || magic || compact_size(len(msg)) || msg
//! sighash_256 = SHA256(SHA256(preimage))
//! signature   = compact-recoverable ECDSA over secp256k1 (65 bytes)
//! ```
//!
//! For a Zcash transparent input the verifier:
//!
//! 1. Reads the prevout's scriptPubKey from the fetched
//!    [`crate::transaction_fetcher::DisclosedTransaction`].
//! 2. Confirms the script is P2PKH and extracts the 20-byte
//!    `hash160`.
//! 3. Recovers the secp256k1 public key from the BIP-322-legacy
//!    signature using the disclosure's `msg` as the signed message.
//! 4. Hashes the recovered pubkey (`ripemd160(sha256(pubkey))`) and
//!    confirms it matches the hash160 from step 2.
//!
//! Any failure on any input downgrades the whole disclosure verdict
//! to [`crate::verify::CryptographicVerdict::InvalidSignature`].
//! P2SH inputs and full-BIP-322 witness blobs are not supported in
//! this commit; they surface as
//! [`crate::verify::CryptographicVerdict::Inconclusive`] with
//! [`crate::verify::InconclusiveReason::UnsupportedPool`].
//!
//! Per [ZIP-311](https://zips.z.cash/zip-0311) the transparent
//! BIP-322-legacy signature is bound to the disclosure's `msg` field,
//! NOT to the shared `BLAKE2b` digest in [`super::digest`]. The
//! digest is the Sapling primitive (it binds `spendAuthSig` and the
//! deferred Groth16 proofs); the transparent path does not consume it.
//! Both pools stay coupled to the same `msg` so a multi-pool
//! disclosure is not trivially separable.

use ripemd::{Digest as _, Ripemd160};
use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use secp256k1::{Message, Secp256k1};
use sha2::Sha256;

use crate::transaction_fetcher::DisclosedTransaction;
use crate::verify::parse_zip311::Zip311TransparentInput;

/// Length of a P2PKH scriptPubKey: `OP_DUP OP_HASH160 <push 20>
/// <20 bytes> OP_EQUALVERIFY OP_CHECKSIG`.
const P2PKH_SCRIPT_LEN: usize = 25;

/// Length of a BIP-322-legacy compact-recoverable signature.
const BIP322_LEGACY_SIG_LEN: usize = 65;

/// Bitcoin/Zcash message-signing magic prefix.
const SIGN_MESSAGE_MAGIC: &[u8] = b"Bitcoin Signed Message:\n";

/// Outcome of verifying one transparent input.
///
/// Returned per input by [`verify_inputs`]; the caller aggregates the
/// outcomes into the disclosure-level verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
    /// Signature recovered to a pubkey whose hash160 matches the
    /// prevout's P2PKH hash160.
    Valid,
    /// Signature failed to recover, hash mismatch, or other strict
    /// cryptographic failure.
    InvalidSignature,
    /// Prevout scriptPubKey is not a recognised P2PKH layout (e.g.
    /// P2SH, multisig, `op_return`). Verifier maps this to the
    /// disclosure-level `Inconclusive { UnsupportedPool }` outcome.
    UnsupportedScript,
    /// Disclosure referenced an `index` that the fetched transaction
    /// does not have, or the prevout scriptPubKey was empty (the chain
    /// plane could not resolve the outpoint).
    PrevoutUnresolved,
}

/// Verify every transparent input in a disclosure against the fetched
/// transaction, returning the per-input outcomes in disclosure order.
///
/// Callers aggregate the outcomes: a single [`InputOutcome::InvalidSignature`]
/// means the disclosure is invalid; a single
/// [`InputOutcome::UnsupportedScript`] OR
/// [`InputOutcome::PrevoutUnresolved`] means the disclosure is
/// inconclusive; otherwise valid.
#[must_use]
pub fn verify_inputs(
    transaction: &DisclosedTransaction,
    inputs: &[Zip311TransparentInput],
    message: &[u8],
) -> Vec<InputOutcome> {
    let secp = Secp256k1::verification_only();
    inputs
        .iter()
        .map(|input| verify_input(&secp, transaction, input, message))
        .collect()
}

fn verify_input(
    secp: &Secp256k1<secp256k1::VerifyOnly>,
    transaction: &DisclosedTransaction,
    input: &Zip311TransparentInput,
    message: &[u8],
) -> InputOutcome {
    let Some(vin) = transaction.transparent_inputs.get(input.index as usize) else {
        return InputOutcome::PrevoutUnresolved;
    };
    if vin.prevout_script_pub_key.is_empty() {
        return InputOutcome::PrevoutUnresolved;
    }
    let Some(expected_hash160) = extract_p2pkh_hash160(&vin.prevout_script_pub_key) else {
        return InputOutcome::UnsupportedScript;
    };
    let Some((recovery_id, sig_compact, compressed)) =
        parse_bip322_legacy_signature(&input.signature)
    else {
        return InputOutcome::InvalidSignature;
    };
    let Ok(recoverable) = RecoverableSignature::from_compact(&sig_compact, recovery_id) else {
        return InputOutcome::InvalidSignature;
    };
    let message_digest = sign_message_digest(message);
    let Ok(msg_obj) = Message::from_digest_slice(&message_digest) else {
        return InputOutcome::InvalidSignature;
    };
    let Ok(pubkey) = secp.recover_ecdsa(&msg_obj, &recoverable) else {
        return InputOutcome::InvalidSignature;
    };
    let recovered_hash160 = hash160_pubkey(&pubkey, compressed);
    if recovered_hash160 == expected_hash160 {
        InputOutcome::Valid
    } else {
        InputOutcome::InvalidSignature
    }
}

/// Pull the 20-byte hash160 out of a P2PKH scriptPubKey.
///
/// Returns `None` for any non-P2PKH script. P2SH (`a914...87`) and
/// other patterns currently surface as
/// [`InputOutcome::UnsupportedScript`].
fn extract_p2pkh_hash160(script: &[u8]) -> Option<[u8; 20]> {
    if script.len() != P2PKH_SCRIPT_LEN {
        return None;
    }
    // OP_DUP OP_HASH160 OP_PUSH(20) ... OP_EQUALVERIFY OP_CHECKSIG
    if script[0] != 0x76
        || script[1] != 0xA9
        || script[2] != 0x14
        || script[23] != 0x88
        || script[24] != 0xAC
    {
        return None;
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&script[3..23]);
    Some(out)
}

/// Parse a BIP-322-legacy compact-recoverable signature.
///
/// Returns `(recovery_id, [r, s], compressed)`. The header byte
/// encodes both the recovery id (low 2 bits after subtracting 27) and
/// whether the recovered pubkey should be serialized compressed (bit
/// 2 of the offset: header in `31..=34` means compressed).
fn parse_bip322_legacy_signature(signature: &[u8]) -> Option<(RecoveryId, [u8; 64], bool)> {
    if signature.len() != BIP322_LEGACY_SIG_LEN {
        return None;
    }
    let header = signature[0];
    if !(27..=34).contains(&header) {
        return None;
    }
    let compressed = header >= 31;
    let offset = if compressed { header - 31 } else { header - 27 };
    let recovery_id = RecoveryId::from_i32(i32::from(offset)).ok()?;
    let mut compact = [0u8; 64];
    compact.copy_from_slice(&signature[1..]);
    Some((recovery_id, compact, compressed))
}

/// Compute the classic bitcoind `signmessage` `SHA256d` digest.
fn sign_message_digest(message: &[u8]) -> [u8; 32] {
    let mut preimage = Vec::with_capacity(SIGN_MESSAGE_MAGIC.len() + message.len() + 16);
    push_compact_size(&mut preimage, u64_from_usize(SIGN_MESSAGE_MAGIC.len()));
    preimage.extend_from_slice(SIGN_MESSAGE_MAGIC);
    push_compact_size(&mut preimage, u64_from_usize(message.len()));
    preimage.extend_from_slice(message);

    let first = sha256(&preimage);
    sha256(&first)
}

const fn u64_from_usize(len: usize) -> u64 {
    len as u64
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn hash160_pubkey(pubkey: &secp256k1::PublicKey, compressed: bool) -> [u8; 20] {
    let serialized: Vec<u8> = if compressed {
        pubkey.serialize().to_vec()
    } else {
        pubkey.serialize_uncompressed().to_vec()
    };
    let sha = sha256(&serialized);
    let mut hasher = Ripemd160::new();
    hasher.update(sha);
    let digest = hasher.finalize();
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest);
    out
}

fn push_compact_size(out: &mut Vec<u8>, len: u64) {
    if len < 0xFD {
        out.push(u8::try_from(len).unwrap_or(0));
    } else if let Ok(short) = u16::try_from(len) {
        out.push(0xFD);
        out.extend_from_slice(&short.to_le_bytes());
    } else if let Ok(word) = u32::try_from(len) {
        out.push(0xFE);
        out.extend_from_slice(&word.to_le_bytes());
    } else {
        out.push(0xFF);
        out.extend_from_slice(&len.to_le_bytes());
    }
}

/// Test-side crypto helpers.
///
/// Shared between this module's unit tests and the local-verifier
/// tests in [`super::local`]. `pub(crate)` plus `#[cfg(test)]` so the
/// helpers do not leak into the production crate surface.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::{BIP322_LEGACY_SIG_LEN, hash160_pubkey, sign_message_digest};
    use secp256k1::ecdsa::RecoverableSignature;
    use secp256k1::{Message, PublicKey, Secp256k1, SecretKey};

    /// Deterministic test keypair. Returns a fixed `(secret, public)`
    /// derived from the byte string `0x01..0x20`.
    pub(crate) fn deterministic_keypair() -> Option<(SecretKey, PublicKey)> {
        let secp = Secp256k1::new();
        let sk_bytes = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C,
            0x1D, 0x1E, 0x1F, 0x20,
        ];
        let sk = SecretKey::from_slice(&sk_bytes).ok()?;
        let pk = sk.public_key(&secp);
        Some((sk, pk))
    }

    /// Hash160 of the compressed serialization of `pk`.
    pub(crate) fn hash160_pubkey_compressed(pk: &PublicKey) -> [u8; 20] {
        hash160_pubkey(pk, true)
    }

    /// Build a P2PKH scriptPubKey from a 20-byte hash160.
    pub(crate) fn p2pkh_script(h160: &[u8; 20]) -> Vec<u8> {
        let mut script = Vec::with_capacity(25);
        script.push(0x76);
        script.push(0xA9);
        script.push(0x14);
        script.extend_from_slice(h160);
        script.push(0x88);
        script.push(0xAC);
        script
    }

    /// Sign `message` with `sk` and return a 65-byte BIP-322-legacy
    /// compressed-pubkey signature plus the `SHA256d` preimage digest.
    pub(crate) fn sign_bip322_legacy(
        sk: &SecretKey,
        message: &[u8],
    ) -> Option<(Vec<u8>, [u8; 32])> {
        let secp = Secp256k1::new();
        let digest = sign_message_digest(message);
        let msg = Message::from_digest_slice(&digest).ok()?;
        let recoverable: RecoverableSignature = secp.sign_ecdsa_recoverable(&msg, sk);
        let (recovery_id, compact) = recoverable.serialize_compact();
        let header_offset = u8::try_from(recovery_id.to_i32()).ok()?;
        let header = 31u8.checked_add(header_offset)?;
        let mut signature = Vec::with_capacity(BIP322_LEGACY_SIG_LEN);
        signature.push(header);
        signature.extend_from_slice(&compact);
        Some((signature, digest))
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support;
    use super::{InputOutcome, extract_p2pkh_hash160, verify_inputs};
    use crate::transaction_fetcher::{DisclosedTransaction, DisclosedTransparentInput};
    use crate::verify::parse_zip311::Zip311TransparentInput;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn signed_input_fixture(
        message: &[u8],
    ) -> Result<(DisclosedTransaction, Zip311TransparentInput), Box<dyn std::error::Error>> {
        let (sk, pk) = tests_support::deterministic_keypair().ok_or("keypair")?;
        let h160 = tests_support::hash160_pubkey_compressed(&pk);
        let (signature, _digest) = tests_support::sign_bip322_legacy(&sk, message).ok_or("sign")?;
        let script = tests_support::p2pkh_script(&h160);

        let tx = DisclosedTransaction {
            txid: [0u8; 32],
            transparent_inputs: vec![DisclosedTransparentInput {
                prevout_txid: [0u8; 32],
                prevout_index: 0,
                prevout_script_pub_key: script,
            }],
            sapling_outputs: Vec::new(),
        };
        let input = Zip311TransparentInput {
            index: 0,
            signature,
        };
        Ok((tx, input))
    }

    #[test]
    fn extracts_p2pkh_hash160() -> TestResult {
        let mut script = vec![0x76, 0xA9, 0x14];
        script.extend_from_slice(&[0xCC; 20]);
        script.extend_from_slice(&[0x88, 0xAC]);
        let out = extract_p2pkh_hash160(&script).ok_or("p2pkh extracts")?;
        assert_eq!(out, [0xCC; 20]);
        Ok(())
    }

    #[test]
    fn rejects_non_p2pkh_script() {
        // P2SH layout: a9 14 .. 87
        let mut script = vec![0xA9, 0x14];
        script.extend_from_slice(&[0xCC; 20]);
        script.push(0x87);
        assert!(extract_p2pkh_hash160(&script).is_none());
    }

    #[test]
    fn verifies_a_signed_transparent_input_compressed() -> TestResult {
        let message = b"challenge-bytes";
        let (tx, input) = signed_input_fixture(message)?;
        let outcomes = verify_inputs(&tx, std::slice::from_ref(&input), message);
        assert_eq!(outcomes, vec![InputOutcome::Valid]);
        Ok(())
    }

    /// Signing one message and trying to verify against a different
    /// message MUST fail.
    ///
    /// This is the property that makes the challenge-response binding
    /// meaningful.
    #[test]
    fn rejects_a_signature_for_a_different_message() -> TestResult {
        let (tx, input) = signed_input_fixture(b"original-message")?;
        let outcomes = verify_inputs(&tx, std::slice::from_ref(&input), b"forged-message");
        assert_eq!(outcomes, vec![InputOutcome::InvalidSignature]);
        Ok(())
    }

    #[test]
    fn rejects_a_truncated_signature() -> TestResult {
        let (tx, mut input) = signed_input_fixture(b"msg")?;
        input.signature.truncate(64);
        let outcomes = verify_inputs(&tx, std::slice::from_ref(&input), b"msg");
        assert_eq!(outcomes, vec![InputOutcome::InvalidSignature]);
        Ok(())
    }

    #[test]
    fn surfaces_unsupported_script_for_p2sh_prevout() -> TestResult {
        let message = b"msg";
        let (mut tx, input) = signed_input_fixture(message)?;
        // Replace the P2PKH script with a P2SH one.
        let mut p2sh = vec![0xA9, 0x14];
        p2sh.extend_from_slice(&[0xCC; 20]);
        p2sh.push(0x87);
        tx.transparent_inputs[0].prevout_script_pub_key = p2sh;
        let outcomes = verify_inputs(&tx, std::slice::from_ref(&input), message);
        assert_eq!(outcomes, vec![InputOutcome::UnsupportedScript]);
        Ok(())
    }

    #[test]
    fn surfaces_unresolved_when_prevout_script_is_empty() -> TestResult {
        let message = b"msg";
        let (mut tx, input) = signed_input_fixture(message)?;
        tx.transparent_inputs[0].prevout_script_pub_key.clear();
        let outcomes = verify_inputs(&tx, std::slice::from_ref(&input), message);
        assert_eq!(outcomes, vec![InputOutcome::PrevoutUnresolved]);
        Ok(())
    }
}
