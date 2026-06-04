//! ZIP-311 payment disclosure byte parser.
//!
//! Implements the zpay-canonical v=0x01 encoding of a
//! [ZIP-311](https://zips.z.cash/zip-0311) Payment Disclosure, which
//! is the on-wire shape the local verifier in [`super::local`]
//! consumes. ZIP-311 itself leaves the wire encoding TODO; this
//! module defines the canonical zpay encoding documented in
//! ADR-0007, on top of the normative ZIP-311 fields (digest,
//! signature scheme, structural constraints).
//!
//! The byte layout is:
//!
//! ```text
//! offset  size              field
//! ------  ----------------  ------------------------------------
//!   0     1                 version (0x01)
//!   1     32                txid (display byte order)
//!   33    CompactSize       msg_len
//!   var   msg_len           msg
//!   var   CompactSize       n_transparent
//!   loop  ...               transparent inputs
//!   var   CompactSize       n_sapling_spends
//!   loop  ...               sapling spends
//!   var   CompactSize       n_sapling_outputs
//!   loop  ...               sapling outputs
//! ```
//!
//! See ADR-0007 for the per-field layout of each loop body and for the
//! "unsigned form" the `BLAKE2b` digest is computed over.
//!
//! Parser failures are categorical: every malformed input maps to a
//! single [`Zip311ParseError`] variant. The variant strings live on
//! the operator-side log path and MUST NOT be echoed verbatim on the
//! wire (the verifier surfaces `cryptographic_verdict: "malformed"`
//! with no detail).

use serde::{Deserialize, Serialize};

/// Current zpay-canonical ZIP-311 disclosure version byte.
///
/// An unknown version surfaces as
/// [`super::CryptographicVerdict::Inconclusive`] with
/// [`super::InconclusiveReason::UnknownVersion`], not as `Malformed`.
/// That distinction matters: forward compatibility is the reason this
/// byte exists at all.
pub const ZIP311_VERSION_V1: u8 = 0x01;

/// Maximum accepted size for the disclosure `msg` field, in bytes.
///
/// Bounded to defeat a denial-of-service where an attacker submits a
/// disclosure whose `msg` claims billions of bytes via the `CompactSize`
/// prefix. 64 KiB is generous compared to the realistic ZIP-311 use
/// cases (challenge nonces, evidence hashes) and small enough that
/// re-allocation is cheap.
const MAX_MSG_LEN: u64 = 65_535;

/// Maximum entries per vector inside one disclosure. Same `DoS` bound
/// applies; 4096 is well above any realistic Sapling transaction.
const MAX_VECTOR_ENTRIES: u64 = 4096;

/// One parsed ZIP-311 Payment Disclosure.
///
/// `version` is the canonical byte; `txid` is in display byte order;
/// `message` is the verifier-supplied challenge bytes (may be empty).
/// The three vectors are in disclosure order, which is also the order
/// the verifier feeds back into the `BLAKE2b` digest reconstruction
/// step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Zip311Disclosure {
    /// Layout version byte. Always [`ZIP311_VERSION_V1`] in this
    /// commit.
    pub version: u8,
    /// 32-byte ZIP-244 transaction id in display byte order.
    pub txid: [u8; 32],
    /// Verifier-supplied challenge bytes bound into the digest. May be
    /// empty.
    pub message: Vec<u8>,
    /// Disclosed transparent inputs. Each entry pins a vin index plus
    /// the BIP-322-legacy signature over the disclosure digest.
    pub transparent_inputs: Vec<Zip311TransparentInput>,
    /// Disclosed Sapling spends. Parsed but surfaced as
    /// `Inconclusive { UnsupportedPool }` until `verify_sapling` lands.
    pub sapling_spends: Vec<Zip311SaplingSpend>,
    /// Disclosed Sapling outputs. Parsed but not consumed today.
    pub sapling_outputs: Vec<Zip311SaplingOutput>,
}

impl Zip311Disclosure {
    /// Classification of which output pool(s) the disclosure references.
    #[must_use]
    pub fn pool(&self) -> DisclosurePool {
        let has_transparent = !self.transparent_inputs.is_empty();
        let has_sapling = !self.sapling_spends.is_empty() || !self.sapling_outputs.is_empty();
        match (has_transparent, has_sapling) {
            (true, false) => DisclosurePool::Transparent,
            (false, true) => DisclosurePool::Sapling,
            (false, false) | (true, true) => DisclosurePool::Unsupported,
        }
    }
}

/// Coarse classification of the disclosed-output pool(s).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DisclosurePool {
    /// Only transparent inputs were disclosed. Verifier runs BIP-322.
    Transparent,
    /// Sapling spends/outputs were disclosed. Verifier surfaces
    /// `Inconclusive { UnsupportedPool }` until the `verify_sapling`
    /// feature gate lands.
    Sapling,
    /// Either empty, mixed-pool, or otherwise out of scope.
    Unsupported,
}

/// One disclosed transparent input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Zip311TransparentInput {
    /// Index into `tx.vin` of the input being disclosed.
    pub index: u32,
    /// BIP-322-legacy compact recoverable signature.
    ///
    /// Length-prefixed on the wire. The legacy P2PKH form is exactly
    /// 65 bytes (header byte plus r, s); the `CompactSize` prefix keeps
    /// the layout extensible for full BIP-322 witness blobs without a
    /// version bump.
    pub signature: Vec<u8>,
}

/// One disclosed Sapling spend.
///
/// The 192-byte `zkproof_spend` and the trailing 64-byte
/// `spend_auth_sig` are kept verbatim so a future
/// `verify_sapling`-feature implementation can verify them without
/// re-parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Zip311SaplingSpend {
    /// Index into `tx.shielded_spends`.
    pub index: u32,
    /// 32-byte value commitment.
    pub cv: [u8; 32],
    /// 32-byte randomized public key.
    pub rk: [u8; 32],
    /// 192-byte Groth16 spend proof.
    pub zkproof: Vec<u8>,
    /// Optional ZIP-304 address proof.
    pub address_proof: Option<Zip304AddressProof>,
    /// 64-byte `RedJubjub` spendAuthSig over the disclosure digest.
    pub spend_auth_sig: Vec<u8>,
}

/// Optional ZIP-304 address proof attached to a Sapling spend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Zip304AddressProof {
    /// 11-byte Sapling diversifier.
    pub d: [u8; 11],
    /// 32-byte diversified transmission key.
    pub pk_d: [u8; 32],
    /// 32-byte fake-note nullifier.
    pub nullifier: [u8; 32],
    /// 192-byte Sapling spend proof for the ZIP-304 fake note.
    pub zkproof: Vec<u8>,
}

/// One disclosed Sapling output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Zip311SaplingOutput {
    /// Index into `tx.shielded_outputs`.
    pub index: u32,
    /// 32-byte outgoing cipher key for OVK-based note recovery.
    pub ock: [u8; 32],
}

/// Parser error variants. Wire callers translate every variant to a
/// single `cryptographic_verdict: "malformed"` outcome with no detail
/// echoed back.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Zip311ParseError {
    /// Disclosure ran out of bytes mid-field.
    #[error("disclosure truncated at offset {offset}: needed {needed} more bytes")]
    Truncated {
        /// Offset at which the parser ran out of input.
        offset: usize,
        /// Bytes the parser still needed.
        needed: usize,
    },
    /// `CompactSize` prefix advertised a length that exceeded the
    /// configured `DoS` bound or the remaining input.
    #[error("compact size {length} exceeds bound {bound}")]
    CompactSizeOutOfRange {
        /// Length read from the wire.
        length: u64,
        /// Bound the parser enforces.
        bound: u64,
    },
    /// A length prefix that should be 1, 3, 5, or 9 bytes was
    /// otherwise malformed (non-minimal encoding).
    #[error("non-minimal compact size encoding at offset {offset}")]
    CompactSizeNonMinimal {
        /// Offset of the bad prefix.
        offset: usize,
    },
    /// Sapling spend or output sub-field length was wrong (e.g. zkproof
    /// not 192 bytes).
    #[error("sapling sub-field {field} expected {expected} bytes, got {observed}")]
    SaplingFieldLength {
        /// Field name, operator-side only.
        field: &'static str,
        /// Expected byte count.
        expected: usize,
        /// Observed byte count.
        observed: usize,
    },
    /// `address_proof_present` byte was something other than 0x00 or 0x01.
    #[error("address_proof_present byte {observed:#04x} is not 0x00 or 0x01")]
    AddressProofPresentInvalid {
        /// The observed byte.
        observed: u8,
    },
    /// Trailing bytes left over after the parser consumed every
    /// declared field.
    #[error("trailing {extra_bytes} bytes after disclosure")]
    TrailingBytes {
        /// Count of unconsumed bytes.
        extra_bytes: usize,
    },
}

/// Parse a zpay-canonical v=0x01 ZIP-311 payment disclosure.
///
/// Returns the parsed [`Zip311Disclosure`] plus the disclosure's
/// version byte. Callers (the local verifier) branch on the version
/// byte first; this function rejects truncation and bound violations
/// but does NOT itself reject an unknown version (that is the
/// caller's job, so a future v=0x02 can flow through to
/// `CryptographicVerdict::Inconclusive { UnknownVersion }` rather
/// than `Malformed`).
///
/// # Errors
///
/// Returns [`Zip311ParseError`] when bytes cannot be decoded against
/// the byte layout above.
pub fn parse(bytes: &[u8]) -> Result<Zip311Disclosure, Zip311ParseError> {
    let mut cursor = Cursor::new(bytes);
    let version = cursor.read_u8()?;
    let txid = cursor.read_array_32()?;
    let message_len = cursor.read_compact_size(MAX_MSG_LEN)?;
    let message = cursor.read_vec(usize_from(message_len))?;

    let transparent_inputs = parse_transparent_inputs(&mut cursor)?;
    let sapling_spends = parse_sapling_spends(&mut cursor)?;
    let sapling_outputs = parse_sapling_outputs(&mut cursor)?;

    if cursor.remaining() != 0 {
        return Err(Zip311ParseError::TrailingBytes {
            extra_bytes: cursor.remaining(),
        });
    }

    Ok(Zip311Disclosure {
        version,
        txid,
        message,
        transparent_inputs,
        sapling_spends,
        sapling_outputs,
    })
}

fn parse_transparent_inputs(
    cursor: &mut Cursor<'_>,
) -> Result<Vec<Zip311TransparentInput>, Zip311ParseError> {
    let count = cursor.read_compact_size(MAX_VECTOR_ENTRIES)?;
    let mut inputs = Vec::with_capacity(usize_from(count));
    for _ in 0..count {
        let index = read_index(cursor)?;
        let sig_len = cursor.read_compact_size(1024)?;
        let signature = cursor.read_vec(usize_from(sig_len))?;
        inputs.push(Zip311TransparentInput { index, signature });
    }
    Ok(inputs)
}

fn parse_sapling_spends(
    cursor: &mut Cursor<'_>,
) -> Result<Vec<Zip311SaplingSpend>, Zip311ParseError> {
    let count = cursor.read_compact_size(MAX_VECTOR_ENTRIES)?;
    let mut spends = Vec::with_capacity(usize_from(count));
    for _ in 0..count {
        let index = read_index(cursor)?;
        let cv = cursor.read_array_32()?;
        let rk = cursor.read_array_32()?;
        let zkproof = cursor.read_exact_to_vec(192, "zkproof_spend")?;
        let addr_proof_present = cursor.read_u8()?;
        let address_proof = parse_address_proof(cursor, addr_proof_present)?;
        let spend_auth_sig = cursor.read_exact_to_vec(64, "spendAuthSig")?;
        spends.push(Zip311SaplingSpend {
            index,
            cv,
            rk,
            zkproof,
            address_proof,
            spend_auth_sig,
        });
    }
    Ok(spends)
}

fn parse_address_proof(
    cursor: &mut Cursor<'_>,
    present_byte: u8,
) -> Result<Option<Zip304AddressProof>, Zip311ParseError> {
    match present_byte {
        0x00 => Ok(None),
        0x01 => {
            let d = cursor.read_array_11()?;
            let pk_d = cursor.read_array_32()?;
            let nullifier = cursor.read_array_32()?;
            let zkproof = cursor.read_exact_to_vec(192, "zkproof_addr")?;
            Ok(Some(Zip304AddressProof {
                d,
                pk_d,
                nullifier,
                zkproof,
            }))
        }
        observed => Err(Zip311ParseError::AddressProofPresentInvalid { observed }),
    }
}

fn parse_sapling_outputs(
    cursor: &mut Cursor<'_>,
) -> Result<Vec<Zip311SaplingOutput>, Zip311ParseError> {
    let count = cursor.read_compact_size(MAX_VECTOR_ENTRIES)?;
    let mut outputs = Vec::with_capacity(usize_from(count));
    for _ in 0..count {
        let index = read_index(cursor)?;
        let ock = cursor.read_array_32()?;
        outputs.push(Zip311SaplingOutput { index, ock });
    }
    Ok(outputs)
}

fn read_index(cursor: &mut Cursor<'_>) -> Result<u32, Zip311ParseError> {
    let raw = cursor.read_compact_size(u64::from(u32::MAX))?;
    u32::try_from(raw).map_err(|_| Zip311ParseError::CompactSizeOutOfRange {
        length: raw,
        bound: u64::from(u32::MAX),
    })
}

/// Serialize a disclosure back to its zpay-canonical v=0x01 bytes.
///
/// Omits the per-input signature(s) and the per-spend `spend_auth_sig`:
/// this is the "unsigned form" the `BLAKE2b` digest is computed over.
/// Symmetric with [`parse`]: re-parsing the output of [`encode_signed`]
/// reproduces the original disclosure byte-for-byte.
#[must_use]
pub fn encode_unsigned(disclosure: &Zip311Disclosure) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + disclosure.message.len());
    out.push(disclosure.version);
    out.extend_from_slice(&disclosure.txid);
    write_compact_size(&mut out, u64_from_usize(disclosure.message.len()));
    out.extend_from_slice(&disclosure.message);

    write_compact_size(
        &mut out,
        u64_from_usize(disclosure.transparent_inputs.len()),
    );
    for input in &disclosure.transparent_inputs {
        write_compact_size(&mut out, u64::from(input.index));
        // NO sig_len/sig in the unsigned form.
    }

    write_compact_size(&mut out, u64_from_usize(disclosure.sapling_spends.len()));
    for spend in &disclosure.sapling_spends {
        write_compact_size(&mut out, u64::from(spend.index));
        out.extend_from_slice(&spend.cv);
        out.extend_from_slice(&spend.rk);
        out.extend_from_slice(&spend.zkproof);
        if let Some(ref proof) = spend.address_proof {
            out.push(0x01);
            out.extend_from_slice(&proof.d);
            out.extend_from_slice(&proof.pk_d);
            out.extend_from_slice(&proof.nullifier);
            out.extend_from_slice(&proof.zkproof);
        } else {
            out.push(0x00);
        }
        // NO spend_auth_sig in the unsigned form.
    }

    write_compact_size(&mut out, u64_from_usize(disclosure.sapling_outputs.len()));
    for output in &disclosure.sapling_outputs {
        write_compact_size(&mut out, u64::from(output.index));
        out.extend_from_slice(&output.ock);
    }

    out
}

/// Serialize a disclosure back to its zpay-canonical v=0x01 bytes with
/// signatures included. Test-side symmetry helper; the verifier never
/// calls this on the hot path.
#[must_use]
pub fn encode_signed(disclosure: &Zip311Disclosure) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.push(disclosure.version);
    out.extend_from_slice(&disclosure.txid);
    write_compact_size(&mut out, u64_from_usize(disclosure.message.len()));
    out.extend_from_slice(&disclosure.message);

    write_compact_size(
        &mut out,
        u64_from_usize(disclosure.transparent_inputs.len()),
    );
    for input in &disclosure.transparent_inputs {
        write_compact_size(&mut out, u64::from(input.index));
        write_compact_size(&mut out, u64_from_usize(input.signature.len()));
        out.extend_from_slice(&input.signature);
    }

    write_compact_size(&mut out, u64_from_usize(disclosure.sapling_spends.len()));
    for spend in &disclosure.sapling_spends {
        write_compact_size(&mut out, u64::from(spend.index));
        out.extend_from_slice(&spend.cv);
        out.extend_from_slice(&spend.rk);
        out.extend_from_slice(&spend.zkproof);
        if let Some(ref proof) = spend.address_proof {
            out.push(0x01);
            out.extend_from_slice(&proof.d);
            out.extend_from_slice(&proof.pk_d);
            out.extend_from_slice(&proof.nullifier);
            out.extend_from_slice(&proof.zkproof);
        } else {
            out.push(0x00);
        }
        out.extend_from_slice(&spend.spend_auth_sig);
    }

    write_compact_size(&mut out, u64_from_usize(disclosure.sapling_outputs.len()));
    for output in &disclosure.sapling_outputs {
        write_compact_size(&mut out, u64::from(output.index));
        out.extend_from_slice(&output.ock);
    }

    out
}

/// Append a Bitcoin/Zcash `CompactSize` varint to `out`.
fn write_compact_size(out: &mut Vec<u8>, len: u64) {
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

/// Narrow a `CompactSize` length into `usize`.
///
/// The parser already bounds the input against [`MAX_VECTOR_ENTRIES`]
/// / [`MAX_MSG_LEN`], both well below `usize::MAX` on every supported
/// target; an overflow here surfaces a length of zero so the caller
/// stops cleanly rather than allocates the entire address space.
fn usize_from(len: u64) -> usize {
    usize::try_from(len).unwrap_or(0)
}

/// Widen a `usize` into `u64` for CompactSize-prefix writing.
const fn u64_from_usize(len: usize) -> u64 {
    len as u64
}

/// Minimal byte cursor used by [`parse`]. Tracks position and yields
/// typed reads with bounds checking.
struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn read_u8(&mut self) -> Result<u8, Zip311ParseError> {
        let byte = *self
            .bytes
            .get(self.offset)
            .ok_or(Zip311ParseError::Truncated {
                offset: self.offset,
                needed: 1,
            })?;
        self.offset += 1;
        Ok(byte)
    }

    fn read_array_32(&mut self) -> Result<[u8; 32], Zip311ParseError> {
        let end = self.offset + 32;
        if end > self.bytes.len() {
            return Err(Zip311ParseError::Truncated {
                offset: self.offset,
                needed: end - self.bytes.len(),
            });
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&self.bytes[self.offset..end]);
        self.offset = end;
        Ok(out)
    }

    fn read_array_11(&mut self) -> Result<[u8; 11], Zip311ParseError> {
        let end = self.offset + 11;
        if end > self.bytes.len() {
            return Err(Zip311ParseError::Truncated {
                offset: self.offset,
                needed: end - self.bytes.len(),
            });
        }
        let mut out = [0u8; 11];
        out.copy_from_slice(&self.bytes[self.offset..end]);
        self.offset = end;
        Ok(out)
    }

    fn read_vec(&mut self, length: usize) -> Result<Vec<u8>, Zip311ParseError> {
        let end = self.offset + length;
        if end > self.bytes.len() {
            return Err(Zip311ParseError::Truncated {
                offset: self.offset,
                needed: end - self.bytes.len(),
            });
        }
        let out = self.bytes[self.offset..end].to_vec();
        self.offset = end;
        Ok(out)
    }

    fn read_exact_to_vec(
        &mut self,
        length: usize,
        field: &'static str,
    ) -> Result<Vec<u8>, Zip311ParseError> {
        let remaining = self.remaining();
        if remaining < length {
            return Err(Zip311ParseError::SaplingFieldLength {
                field,
                expected: length,
                observed: remaining,
            });
        }
        let end = self.offset + length;
        let out = self.bytes[self.offset..end].to_vec();
        self.offset = end;
        Ok(out)
    }

    fn read_compact_size(&mut self, bound: u64) -> Result<u64, Zip311ParseError> {
        let prefix_offset = self.offset;
        let prefix = self.read_u8()?;
        let parsed_len = match prefix {
            0..=0xFC => u64::from(prefix),
            0xFD => {
                let lo = self.read_u8()?;
                let hi = self.read_u8()?;
                let len = u64::from(u16::from_le_bytes([lo, hi]));
                if len < 0xFD {
                    return Err(Zip311ParseError::CompactSizeNonMinimal {
                        offset: prefix_offset,
                    });
                }
                len
            }
            0xFE => {
                let b0 = self.read_u8()?;
                let b1 = self.read_u8()?;
                let b2 = self.read_u8()?;
                let b3 = self.read_u8()?;
                let len = u64::from(u32::from_le_bytes([b0, b1, b2, b3]));
                if u16::try_from(len).is_ok() {
                    return Err(Zip311ParseError::CompactSizeNonMinimal {
                        offset: prefix_offset,
                    });
                }
                len
            }
            0xFF => {
                let mut buf = [0u8; 8];
                for byte in &mut buf {
                    *byte = self.read_u8()?;
                }
                let len = u64::from_le_bytes(buf);
                if u32::try_from(len).is_ok() {
                    return Err(Zip311ParseError::CompactSizeNonMinimal {
                        offset: prefix_offset,
                    });
                }
                len
            }
        };
        if parsed_len > bound {
            return Err(Zip311ParseError::CompactSizeOutOfRange {
                length: parsed_len,
                bound,
            });
        }
        Ok(parsed_len)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DisclosurePool, ZIP311_VERSION_V1, Zip311Disclosure, Zip311ParseError,
        Zip311TransparentInput, encode_signed, encode_unsigned, parse,
    };

    fn transparent_disclosure_fixture() -> Zip311Disclosure {
        Zip311Disclosure {
            version: ZIP311_VERSION_V1,
            txid: [0x11u8; 32],
            message: b"challenge".to_vec(),
            transparent_inputs: vec![Zip311TransparentInput {
                index: 0,
                signature: vec![0xAAu8; 65],
            }],
            sapling_spends: Vec::new(),
            sapling_outputs: Vec::new(),
        }
    }

    #[test]
    fn round_trips_a_transparent_disclosure() -> Result<(), Box<dyn std::error::Error>> {
        let disclosure = transparent_disclosure_fixture();
        let encoded = encode_signed(&disclosure);
        let parsed = parse(&encoded)?;
        assert_eq!(parsed.version, ZIP311_VERSION_V1);
        assert_eq!(parsed.txid, [0x11u8; 32]);
        assert_eq!(parsed.message, b"challenge");
        assert_eq!(parsed.transparent_inputs.len(), 1);
        assert_eq!(parsed.transparent_inputs[0].index, 0);
        assert_eq!(parsed.transparent_inputs[0].signature.len(), 65);
        assert!(parsed.sapling_spends.is_empty());
        assert!(parsed.sapling_outputs.is_empty());
        assert_eq!(parsed.pool(), DisclosurePool::Transparent);
        Ok(())
    }

    #[test]
    fn unsigned_form_strips_signature_field() {
        let disclosure = transparent_disclosure_fixture();
        let signed = encode_signed(&disclosure);
        let unsigned = encode_unsigned(&disclosure);
        // The signed form carries: 1 (sig_len CompactSize) + 65 (sig bytes)
        // additional bytes per transparent input compared to the unsigned
        // form. With one input that's 66 bytes.
        assert_eq!(signed.len(), unsigned.len() + 66);
    }

    #[test]
    fn rejects_truncated_input() {
        let outcome = parse(&[ZIP311_VERSION_V1, 0x00]);
        assert!(matches!(outcome, Err(Zip311ParseError::Truncated { .. })));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = encode_signed(&transparent_disclosure_fixture());
        bytes.push(0xFF);
        let outcome = parse(&bytes);
        assert!(matches!(
            outcome,
            Err(Zip311ParseError::TrailingBytes { extra_bytes: 1 }),
        ));
    }

    #[test]
    fn rejects_non_minimal_compact_size() {
        // 0xFD prefix encoding a value < 0xFD is non-minimal. Pick
        // value 0x10 (16): one-byte representable but written with
        // the 3-byte prefix.
        let bytes = vec![
            ZIP311_VERSION_V1,
            // 32-byte txid
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            // non-minimal msg_len: 0xFD prefix encoding 0x0010 (16),
            // which fits in one byte (< 0xFD).
            0xFD,
            0x10,
            0x00,
        ];
        let outcome = parse(&bytes);
        assert!(matches!(
            outcome,
            Err(Zip311ParseError::CompactSizeNonMinimal { .. }),
        ));
    }

    #[test]
    fn parses_an_unknown_version_byte() -> Result<(), Box<dyn std::error::Error>> {
        // The parser does NOT itself reject an unknown version; that is
        // the local verifier's job, mapping to
        // `CryptographicVerdict::Inconclusive { UnknownVersion }` instead
        // of `Malformed`.
        let mut bytes = encode_signed(&transparent_disclosure_fixture());
        bytes[0] = 0x99;
        let parsed = parse(&bytes)?;
        assert_eq!(parsed.version, 0x99);
        Ok(())
    }
}
