//! ZIP-311 payment-disclosure byte parser.
//!
//! The byte layout is fixed (per the [zinder verifier PRD][prd]
//! mirroring ZIP-311):
//!
//! ```text
//! offset  size   field
//! ------  ----   -----
//!   0      4     protocol version tag (big-endian; current = 0x00000001)
//!   4     32     transaction id (txid) of the disclosed transaction
//!  36     32     sender-chosen payment id (opaque 32-byte tag)
//!  68      8     disclosed value in zatoshis (little-endian u64)
//!  76      1     output-kind discriminator (0x00 transparent, 0x01 Sapling)
//!  77      4     output index inside the transaction (little-endian u32)
//!  81      *     proof block
//! ```
//!
//! The proof block shape depends on the output kind:
//!
//! - **Transparent** (`kind = 0x00`): 64 bytes of BIP-340 Schnorr
//!   signature over the metadata, using the recipient's transparent
//!   public key. Total disclosure length: 145 bytes.
//! - **Sapling** (`kind = 0x01`): 32 bytes ephemeral key + 11 bytes
//!   diversifier + 4 bytes ivk tag = 47 bytes. Total disclosure
//!   length: 128 bytes.
//!
//! Parser failures are deliberately coarse-grained: every malformed
//! input maps to [`ParseError::Malformed`] so the verifier's wire
//! response stays redaction-safe. Detail strings live in the error
//! variants for operator-side debugging but are not echoed across
//! the wire.
//!
//! [prd]: https://github.com/gustavovalverde/zinder/blob/main/docs/prd/zip311-payment-disclosure-verifier.md

/// Current ZIP-311 protocol version tag.
pub const PROTOCOL_VERSION_V1: u32 = 0x0000_0001;

/// Transparent-output proof block length: 64-byte BIP-340 Schnorr.
pub const TRANSPARENT_PROOF_LEN: usize = 64;

/// Sapling-output proof block length: 32 esk + 11 diversifier + 4 ivk tag.
pub const SAPLING_PROOF_LEN: usize = 47;

/// Byte offset of the version tag inside the disclosure.
const OFF_VERSION: usize = 0;
/// Byte offset of the txid inside the disclosure.
const OFF_TXID: usize = 4;
/// Byte offset of the payment id inside the disclosure.
const OFF_PAYMENT_ID: usize = 36;
/// Byte offset of the disclosed value inside the disclosure.
const OFF_DISCLOSED_VALUE: usize = 68;
/// Byte offset of the output-kind discriminator.
const OFF_OUTPUT_KIND: usize = 76;
/// Byte offset of the output index inside the disclosure.
const OFF_OUTPUT_INDEX: usize = 77;
/// Byte offset where the proof block starts.
const OFF_PROOF: usize = 81;

/// Output-kind discriminator: transparent receiver.
const OUTPUT_KIND_TRANSPARENT: u8 = 0x00;
/// Output-kind discriminator: Sapling receiver.
const OUTPUT_KIND_SAPLING: u8 = 0x01;

/// Parsed ZIP-311 disclosure payload.
///
/// The parser is exhaustive: every field of the on-the-wire layout
/// is decoded. Cryptographic verification of the proof block lives
/// in [`super::transparent`] and [`super::sapling`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ParsedDisclosure {
    /// Protocol version tag (currently always `1`).
    pub version: u32,
    /// 32-byte ZIP-244 transaction id the disclosure pins to.
    pub transaction_id: [u8; 32],
    /// 32-byte opaque payment id carried verbatim from the sender.
    pub payment_id: [u8; 32],
    /// Disclosed value the sender asserts the disclosed output paid.
    pub disclosed_value_zat: u64,
    /// Output kind plus its proof block.
    pub proof: DisclosureProof,
    /// Output index inside the transaction the disclosure pins to.
    pub output_index: u32,
}

/// Proof block by output kind.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DisclosureProof {
    /// Transparent output: 64-byte BIP-340 Schnorr signature.
    Transparent {
        /// 64-byte signature over the disclosure metadata.
        schnorr_signature: [u8; 64],
    },
    /// Sapling output: ephemeral key, diversifier, and ivk tag.
    Sapling {
        /// 32-byte sender-disclosed ephemeral secret key.
        ephemeral_key: [u8; 32],
        /// 11-byte diversifier of the recipient address.
        diversifier: [u8; 11],
        /// 4-byte recipient ivk tag.
        ivk_tag: [u8; 4],
    },
}

/// Parser error variants.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ParseError {
    /// Disclosure bytes were shorter than the minimum header.
    #[error("disclosure too short: expected at least {minimum} bytes, got {observed}")]
    TooShort {
        /// Minimum required byte count for the parsed prefix.
        minimum: usize,
        /// Observed input length.
        observed: usize,
    },
    /// Disclosure carries an unsupported protocol version tag.
    #[error("unsupported ZIP-311 protocol version: {observed}")]
    ProtocolVersionUnknown {
        /// Version tag that was observed.
        observed: u32,
    },
    /// Output-kind discriminator was outside the valid set.
    #[error("unknown output-kind discriminator: {observed:#04x}")]
    OutputKindUnknown {
        /// Discriminator byte that was observed.
        observed: u8,
    },
    /// Disclosure proof-block length did not match the kind.
    #[error(
        "proof-block length mismatch for kind {kind_label}: expected {expected}, got {observed}"
    )]
    ProofLengthMismatch {
        /// Output kind label, for the operator log only.
        kind_label: &'static str,
        /// Expected proof-block length.
        expected: usize,
        /// Observed remaining bytes after the metadata header.
        observed: usize,
    },
}

/// Parse a ZIP-311 payment-disclosure byte string.
///
/// # Errors
///
/// Returns [`ParseError`] when the bytes cannot be decoded against the
/// current ZIP-311 byte layout. Wire callers translate every parse
/// error into a single redaction-safe `Verdict::Malformed`; the
/// detail strings on these variants exist for operator-side logs
/// only and must not be echoed back to the disclosure submitter.
pub fn parse_disclosure(bytes: &[u8]) -> Result<ParsedDisclosure, ParseError> {
    if bytes.len() < OFF_PROOF {
        return Err(ParseError::TooShort {
            minimum: OFF_PROOF,
            observed: bytes.len(),
        });
    }

    let version = read_u32_be(bytes, OFF_VERSION);
    if version != PROTOCOL_VERSION_V1 {
        return Err(ParseError::ProtocolVersionUnknown { observed: version });
    }

    let transaction_id = read_32(bytes, OFF_TXID);
    let payment_id = read_32(bytes, OFF_PAYMENT_ID);
    let disclosed_value_zat = read_u64_le(bytes, OFF_DISCLOSED_VALUE);
    let output_kind = bytes[OFF_OUTPUT_KIND];
    let output_index = read_u32_le(bytes, OFF_OUTPUT_INDEX);

    let proof_remainder_len = bytes.len() - OFF_PROOF;
    let proof = match output_kind {
        OUTPUT_KIND_TRANSPARENT => {
            if proof_remainder_len != TRANSPARENT_PROOF_LEN {
                return Err(ParseError::ProofLengthMismatch {
                    kind_label: "transparent",
                    expected: TRANSPARENT_PROOF_LEN,
                    observed: proof_remainder_len,
                });
            }
            let mut signature = [0u8; TRANSPARENT_PROOF_LEN];
            signature.copy_from_slice(&bytes[OFF_PROOF..OFF_PROOF + TRANSPARENT_PROOF_LEN]);
            DisclosureProof::Transparent {
                schnorr_signature: signature,
            }
        }
        OUTPUT_KIND_SAPLING => {
            if proof_remainder_len != SAPLING_PROOF_LEN {
                return Err(ParseError::ProofLengthMismatch {
                    kind_label: "sapling",
                    expected: SAPLING_PROOF_LEN,
                    observed: proof_remainder_len,
                });
            }
            let mut ephemeral_key = [0u8; 32];
            ephemeral_key.copy_from_slice(&bytes[OFF_PROOF..OFF_PROOF + 32]);
            let mut diversifier = [0u8; 11];
            diversifier.copy_from_slice(&bytes[OFF_PROOF + 32..OFF_PROOF + 43]);
            let mut ivk_tag = [0u8; 4];
            ivk_tag.copy_from_slice(&bytes[OFF_PROOF + 43..OFF_PROOF + 47]);
            DisclosureProof::Sapling {
                ephemeral_key,
                diversifier,
                ivk_tag,
            }
        }
        observed => return Err(ParseError::OutputKindUnknown { observed }),
    };

    Ok(ParsedDisclosure {
        version,
        transaction_id,
        payment_id,
        disclosed_value_zat,
        proof,
        output_index,
    })
}

fn read_u32_be(bytes: &[u8], offset: usize) -> u32 {
    let chunk = [
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ];
    u32::from_be_bytes(chunk)
}

fn read_u32_le(bytes: &[u8], offset: usize) -> u32 {
    let chunk = [
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ];
    u32::from_le_bytes(chunk)
}

fn read_u64_le(bytes: &[u8], offset: usize) -> u64 {
    let chunk = [
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ];
    u64::from_le_bytes(chunk)
}

fn read_32(bytes: &[u8], offset: usize) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[offset..offset + 32]);
    out
}

#[cfg(test)]
mod tests {
    use super::{
        DisclosureProof, OFF_PROOF, PROTOCOL_VERSION_V1, ParseError, SAPLING_PROOF_LEN,
        TRANSPARENT_PROOF_LEN, parse_disclosure,
    };

    fn write_metadata_header(out: &mut Vec<u8>, kind: u8, output_index: u32) {
        out.extend_from_slice(&PROTOCOL_VERSION_V1.to_be_bytes());
        out.extend_from_slice(&[0x11u8; 32]); // txid
        out.extend_from_slice(&[0x22u8; 32]); // payment_id
        out.extend_from_slice(&50_000u64.to_le_bytes()); // disclosed_value_zat
        out.push(kind);
        out.extend_from_slice(&output_index.to_le_bytes());
    }

    #[test]
    fn parses_a_transparent_disclosure() -> Result<(), &'static str> {
        let mut bytes = Vec::with_capacity(OFF_PROOF + TRANSPARENT_PROOF_LEN);
        write_metadata_header(&mut bytes, 0x00, 0x0000_0003);
        bytes.extend_from_slice(&[0xAAu8; TRANSPARENT_PROOF_LEN]);

        let parsed = parse_disclosure(&bytes).map_err(|_| "parser must accept transparent")?;
        assert_eq!(parsed.version, PROTOCOL_VERSION_V1);
        assert_eq!(parsed.transaction_id, [0x11u8; 32]);
        assert_eq!(parsed.payment_id, [0x22u8; 32]);
        assert_eq!(parsed.disclosed_value_zat, 50_000);
        assert_eq!(parsed.output_index, 3);
        match parsed.proof {
            DisclosureProof::Transparent { schnorr_signature } => {
                assert_eq!(schnorr_signature, [0xAAu8; TRANSPARENT_PROOF_LEN]);
            }
            DisclosureProof::Sapling { .. } => {
                return Err("expected Transparent proof variant");
            }
        }
        Ok(())
    }

    #[test]
    fn parses_a_sapling_disclosure() -> Result<(), &'static str> {
        let mut bytes = Vec::with_capacity(OFF_PROOF + SAPLING_PROOF_LEN);
        write_metadata_header(&mut bytes, 0x01, 0x0000_0001);
        bytes.extend_from_slice(&[0xEEu8; 32]); // ephemeral_key
        bytes.extend_from_slice(&[0xCCu8; 11]); // diversifier
        bytes.extend_from_slice(&[0xDDu8; 4]); // ivk_tag

        let parsed = parse_disclosure(&bytes).map_err(|_| "parser must accept sapling")?;
        match parsed.proof {
            DisclosureProof::Sapling {
                ephemeral_key,
                diversifier,
                ivk_tag,
            } => {
                assert_eq!(ephemeral_key, [0xEEu8; 32]);
                assert_eq!(diversifier, [0xCCu8; 11]);
                assert_eq!(ivk_tag, [0xDDu8; 4]);
            }
            DisclosureProof::Transparent { .. } => {
                return Err("expected Sapling proof variant");
            }
        }
        Ok(())
    }

    #[test]
    fn rejects_short_input() {
        let bytes = [0u8; 10];
        assert!(matches!(
            parse_disclosure(&bytes),
            Err(ParseError::TooShort { .. })
        ));
    }

    #[test]
    fn rejects_unknown_version() {
        let mut bytes = Vec::with_capacity(OFF_PROOF + TRANSPARENT_PROOF_LEN);
        bytes.extend_from_slice(&0x0000_0002u32.to_be_bytes()); // unknown version
        bytes.resize(OFF_PROOF, 0);
        bytes.extend_from_slice(&[0u8; TRANSPARENT_PROOF_LEN]);
        assert!(matches!(
            parse_disclosure(&bytes),
            Err(ParseError::ProtocolVersionUnknown { observed: 2 })
        ));
    }

    #[test]
    fn rejects_unknown_output_kind() {
        let mut bytes = Vec::with_capacity(OFF_PROOF);
        write_metadata_header(&mut bytes, 0x42, 0);
        bytes.extend_from_slice(&[0u8; TRANSPARENT_PROOF_LEN]);
        assert!(matches!(
            parse_disclosure(&bytes),
            Err(ParseError::OutputKindUnknown { observed: 0x42 })
        ));
    }

    #[test]
    fn rejects_transparent_with_wrong_proof_length() {
        let mut bytes = Vec::new();
        write_metadata_header(&mut bytes, 0x00, 0);
        bytes.extend_from_slice(&[0u8; TRANSPARENT_PROOF_LEN - 1]);
        assert!(matches!(
            parse_disclosure(&bytes),
            Err(ParseError::ProofLengthMismatch {
                kind_label: "transparent",
                ..
            })
        ));
    }

    #[test]
    fn rejects_sapling_with_wrong_proof_length() {
        let mut bytes = Vec::new();
        write_metadata_header(&mut bytes, 0x01, 0);
        bytes.extend_from_slice(&[0u8; SAPLING_PROOF_LEN + 1]);
        assert!(matches!(
            parse_disclosure(&bytes),
            Err(ParseError::ProofLengthMismatch {
                kind_label: "sapling",
                ..
            })
        ));
    }
}
