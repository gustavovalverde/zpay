//! Local, in-process ZIP-311 payment-disclosure verifier.
//!
//! [`LocalPaymentDisclosureVerifier`] composes:
//!
//! - [`super::parse_zip311::parse`] to decode the disclosure bytes,
//! - [`super::digest::compute`] to reconstruct the ZIP-311 digest,
//! - [`super::transparent::verify_inputs`] to run the BIP-322-legacy
//!   transparent verification path,
//! - and the supplied [`TransactionFetcher`] to resolve the on-chain
//!   transaction.
//!
//! Sapling and any other non-transparent pool surface as
//! [`super::CryptographicVerdict::Inconclusive`] with
//! [`super::InconclusiveReason::UnsupportedPool`] today; the
//! `verify_sapling` feature gate gates the Groth16 implementation that
//! lifts the inconclusive verdict for Sapling. Unknown disclosure
//! version bytes surface as
//! [`super::InconclusiveReason::UnknownVersion`] rather than
//! `Malformed` so a future ZIP-311 revision can land without breaking
//! older verifiers.

use crate::transaction_fetcher::TransactionFetcher;
use crate::types::PaymentNetwork;
use crate::verify::parse_zip311::{
    DisclosurePool, ZIP311_VERSION_V1, Zip311Disclosure, parse as parse_zip311,
};
use crate::verify::transparent::{InputOutcome, verify_inputs};
use crate::verify::{
    AmountReconciliation, ChainPresence, CryptographicVerdict, InconclusiveReason,
    PaymentDisclosureVerifier, VerifyError, VerifyResponse, chain_presence_for,
};

/// In-process ZIP-311 verifier pinned to a single [`PaymentNetwork`].
///
/// Pinning the network at construction time is deliberate: the
/// `BLAKE2b` digest is personalized with the SLIP-44 coin type, so a
/// disclosure produced for mainnet would never verify under testnet.
/// Auto-detecting the network from disclosure bytes would invite a
/// network-confusion attack where a malicious sender pivots the
/// verifier into a network of their choosing.
#[derive(Debug, Clone, Copy)]
pub struct LocalPaymentDisclosureVerifier {
    /// Network the digest personalization binds to.
    pub network: PaymentNetwork,
}

impl LocalPaymentDisclosureVerifier {
    /// Build a verifier pinned to a network.
    #[must_use]
    pub const fn new(network: PaymentNetwork) -> Self {
        Self { network }
    }
}

impl PaymentDisclosureVerifier for LocalPaymentDisclosureVerifier {
    async fn verify_disclosure<F>(
        &self,
        disclosure_bytes: &[u8],
        fetcher: &F,
    ) -> Result<VerifyResponse, VerifyError>
    where
        F: TransactionFetcher + ?Sized,
    {
        let Ok(parsed) = parse_zip311(disclosure_bytes) else {
            return Ok(short_circuit_malformed(None));
        };

        if parsed.version != ZIP311_VERSION_V1 {
            return Ok(short_circuit_inconclusive(
                InconclusiveReason::UnknownVersion,
                Some(hex::encode(parsed.txid)),
            ));
        }

        // An empty disclosure asserts nothing: no transparent inputs,
        // no Sapling spends, no Sapling outputs. There is nothing to
        // verify, so the verdict is Malformed rather than Inconclusive.
        if parsed.transparent_inputs.is_empty()
            && parsed.sapling_spends.is_empty()
            && parsed.sapling_outputs.is_empty()
        {
            return Ok(short_circuit_malformed(Some(hex::encode(parsed.txid))));
        }

        if parsed.pool() == DisclosurePool::Transparent {
            self.run_transparent(parsed, fetcher).await
        } else {
            // Sapling and any future non-exhaustive pool variant
            // collapse to the same Inconclusive-with-pool outcome:
            // today the verifier only runs the transparent path.
            Ok(short_circuit_inconclusive(
                InconclusiveReason::UnsupportedPool,
                Some(hex::encode(parsed.txid)),
            ))
        }
    }
}

impl LocalPaymentDisclosureVerifier {
    /// Run the transparent verification path.
    ///
    /// Per [ZIP-311](https://zips.z.cash/zip-0311), the transparent
    /// signature is bound to `parsed.message` via the
    /// BIP-322-legacy `signmessage` preimage, NOT to the `BLAKE2b`
    /// disclosure digest. The digest is the Sapling primitive (it
    /// binds `spendAuthSig` and the deferred Groth16 proofs); the
    /// transparent path does not consume it. See
    /// [`super::digest`] for the digest construction the
    /// `verify_sapling` feature gate will consume.
    async fn run_transparent<F>(
        &self,
        parsed: Zip311Disclosure,
        fetcher: &F,
    ) -> Result<VerifyResponse, VerifyError>
    where
        F: TransactionFetcher + ?Sized,
    {
        let txid_hex = hex::encode(parsed.txid);

        // BIP-322 transparent verification needs the prevout
        // scriptPubKey from the chain plane. Without it the verifier
        // has nothing to check the recovered pubkey hash against, so a
        // fetcher failure is Inconclusive { PrevoutUnresolved }, not
        // Valid. chain_presence still reflects the fetcher outcome
        // (NotFound vs OracleUnavailable) so callers can distinguish
        // "transaction missing" from "explorer plane down".
        let transaction = match fetcher.fetch_transaction(parsed.txid).await {
            Ok(transaction) => transaction,
            Err(err) => {
                return Ok(VerifyResponse {
                    cryptographic_verdict: CryptographicVerdict::Inconclusive,
                    inconclusive_reason: Some(InconclusiveReason::PrevoutUnresolved),
                    chain_presence: chain_presence_for(&err),
                    amount_reconciliation: AmountReconciliation::NotChecked,
                    transaction_id: Some(txid_hex),
                    payment_id: None,
                    disclosed_value_zat: None,
                });
            }
        };

        // Chain-plane integrity: the fetcher MUST return the
        // transaction the disclosure points at. A mismatch means the
        // explorer plane returned the wrong row (operator bug or
        // upstream malice). Treat as Malformed so the caller does not
        // act on a cross-transaction signature.
        if transaction.txid != parsed.txid {
            return Ok(short_circuit_malformed(Some(txid_hex)));
        }

        let outcomes = verify_inputs(&transaction, &parsed.transparent_inputs, &parsed.message);
        let cryptographic_verdict = aggregate_transparent_outcomes(&outcomes);
        let inconclusive_reason = inconclusive_reason_for(cryptographic_verdict, &outcomes);

        Ok(VerifyResponse {
            cryptographic_verdict,
            inconclusive_reason,
            chain_presence: ChainPresence::Mined,
            amount_reconciliation: AmountReconciliation::NotChecked,
            transaction_id: Some(txid_hex),
            payment_id: None,
            disclosed_value_zat: None,
        })
    }
}

/// Pick the most-specific [`InconclusiveReason`] when the per-input
/// outcomes aggregated to [`CryptographicVerdict::Inconclusive`].
///
/// Priority: `PrevoutUnresolved` beats `UnsupportedPool`. The
/// unresolved-prevout signal is the more useful operator hint (the
/// chain plane returned the row but did not resolve the prevout) and
/// distinguishes "no data" from "wrong kind of data". Any non-
/// Inconclusive verdict returns `None`: the `inconclusive_reason`
/// field belongs to [`CryptographicVerdict::Inconclusive`] alone.
fn inconclusive_reason_for(
    verdict: CryptographicVerdict,
    outcomes: &[InputOutcome],
) -> Option<InconclusiveReason> {
    if !matches!(verdict, CryptographicVerdict::Inconclusive) {
        return None;
    }
    if outcomes
        .iter()
        .any(|outcome| matches!(outcome, InputOutcome::PrevoutUnresolved))
    {
        return Some(InconclusiveReason::PrevoutUnresolved);
    }
    Some(InconclusiveReason::UnsupportedPool)
}

/// Aggregate the per-input outcomes into a single cryptographic
/// verdict. The priority order is:
///
/// 1. `InvalidSignature` if any input failed cryptographically.
/// 2. `Inconclusive` if any input had an unsupported script or
///    unresolved prevout.
/// 3. `Valid` only when every input is `Valid` AND the input set is
///    non-empty.
///
/// An empty input set surfaces as `Malformed`: a transparent
/// disclosure that asserts nothing about its inputs is not something
/// the verifier can call inconclusive in good faith. The same case is
/// guarded one layer up so the chain round-trip is skipped, but this
/// stays as a defensive lower bound.
fn aggregate_transparent_outcomes(outcomes: &[InputOutcome]) -> CryptographicVerdict {
    if outcomes.is_empty() {
        return CryptographicVerdict::Malformed;
    }
    let mut any_inconclusive = false;
    for outcome in outcomes {
        match outcome {
            InputOutcome::InvalidSignature => {
                return CryptographicVerdict::InvalidSignature;
            }
            InputOutcome::UnsupportedScript | InputOutcome::PrevoutUnresolved => {
                any_inconclusive = true;
            }
            InputOutcome::Valid => {}
        }
    }
    if any_inconclusive {
        CryptographicVerdict::Inconclusive
    } else {
        CryptographicVerdict::Valid
    }
}

fn short_circuit_malformed(txid_hex: Option<String>) -> VerifyResponse {
    VerifyResponse {
        cryptographic_verdict: CryptographicVerdict::Malformed,
        inconclusive_reason: None,
        chain_presence: ChainPresence::OracleUnavailable,
        amount_reconciliation: AmountReconciliation::NotChecked,
        transaction_id: txid_hex,
        payment_id: None,
        disclosed_value_zat: None,
    }
}

fn short_circuit_inconclusive(
    reason: InconclusiveReason,
    txid_hex: Option<String>,
) -> VerifyResponse {
    VerifyResponse {
        cryptographic_verdict: CryptographicVerdict::Inconclusive,
        inconclusive_reason: Some(reason),
        chain_presence: ChainPresence::OracleUnavailable,
        amount_reconciliation: AmountReconciliation::NotChecked,
        transaction_id: txid_hex,
        payment_id: None,
        disclosed_value_zat: None,
    }
}

#[cfg(test)]
mod tests {
    use super::LocalPaymentDisclosureVerifier;
    use crate::transaction_fetcher::{
        DisclosedTransaction, DisclosedTransparentInput, FetchError, TransactionFetcher,
    };
    use crate::types::PaymentNetwork;
    use crate::verify::parse_zip311::{
        ZIP311_VERSION_V1, Zip311Disclosure, Zip311SaplingSpend, Zip311TransparentInput,
        encode_signed,
    };
    use crate::verify::transparent::tests_support;
    use crate::verify::{
        AmountReconciliation, ChainPresence, CryptographicVerdict, InconclusiveReason,
        PaymentDisclosureVerifier,
    };
    use parking_lot::Mutex;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Single-shot fetcher fixture used by the test cases.
    struct ScriptedFetcher {
        outcome: Mutex<Option<Result<DisclosedTransaction, FetchError>>>,
    }

    impl ScriptedFetcher {
        fn new(outcome: Result<DisclosedTransaction, FetchError>) -> Self {
            Self {
                outcome: Mutex::new(Some(outcome)),
            }
        }
    }

    impl TransactionFetcher for ScriptedFetcher {
        async fn fetch_transaction(
            &self,
            _txid: [u8; 32],
        ) -> Result<DisclosedTransaction, FetchError> {
            let mut guard = self.outcome.lock();
            guard.take().unwrap_or_else(|| {
                Err(FetchError::Unavailable {
                    reason: "scripted fetcher exhausted".to_owned(),
                })
            })
        }
    }

    fn fixture_with_valid_signature(
        network: PaymentNetwork,
    ) -> Result<(Zip311Disclosure, DisclosedTransaction), Box<dyn std::error::Error>> {
        let _ = network;
        let message: &[u8] = b"challenge-bytes";
        let (sk, pk) = tests_support::deterministic_keypair().ok_or("keypair")?;
        let h160 = tests_support::hash160_pubkey_compressed(&pk);
        let (sig, _digest) = tests_support::sign_bip322_legacy(&sk, message).ok_or("sign")?;
        let script = tests_support::p2pkh_script(&h160);

        let disclosure = Zip311Disclosure {
            version: ZIP311_VERSION_V1,
            txid: [0x33u8; 32],
            message: message.to_vec(),
            transparent_inputs: vec![Zip311TransparentInput {
                index: 0,
                signature: sig,
            }],
            sapling_spends: Vec::new(),
            sapling_outputs: Vec::new(),
        };
        let transaction = DisclosedTransaction {
            txid: disclosure.txid,
            transparent_inputs: vec![DisclosedTransparentInput {
                prevout_txid: [0u8; 32],
                prevout_index: 0,
                prevout_script_pub_key: script,
            }],
            sapling_outputs: Vec::new(),
        };
        Ok((disclosure, transaction))
    }

    #[tokio::test]
    async fn returns_valid_for_a_well_signed_transparent_disclosure() -> TestResult {
        let network = PaymentNetwork::Mainnet;
        let (disclosure, transaction) = fixture_with_valid_signature(network)?;
        let bytes = encode_signed(&disclosure);
        let verifier = LocalPaymentDisclosureVerifier::new(network);
        let fetcher = ScriptedFetcher::new(Ok(transaction));
        let response = verifier.verify_disclosure(&bytes, &fetcher).await?;
        assert_eq!(response.cryptographic_verdict, CryptographicVerdict::Valid);
        assert_eq!(response.chain_presence, ChainPresence::Mined);
        assert_eq!(
            response.amount_reconciliation,
            AmountReconciliation::NotChecked
        );
        assert_eq!(response.transaction_id, Some(hex::encode([0x33u8; 32])));
        Ok(())
    }

    #[tokio::test]
    async fn returns_invalid_signature_when_sig_bytes_flipped() -> TestResult {
        let network = PaymentNetwork::Mainnet;
        let (mut disclosure, transaction) = fixture_with_valid_signature(network)?;
        // Flip a single byte of the signature; recovery yields a
        // different pubkey whose hash160 does not match.
        let sig = &mut disclosure.transparent_inputs[0].signature;
        let last = sig.len() - 1;
        sig[last] ^= 0xFF;
        let bytes = encode_signed(&disclosure);
        let verifier = LocalPaymentDisclosureVerifier::new(network);
        let fetcher = ScriptedFetcher::new(Ok(transaction));
        let response = verifier.verify_disclosure(&bytes, &fetcher).await?;
        assert_eq!(
            response.cryptographic_verdict,
            CryptographicVerdict::InvalidSignature
        );
        // Even on invalid sig, chain presence still reports Mined: the
        // 3 axes are orthogonal.
        assert_eq!(response.chain_presence, ChainPresence::Mined);
        Ok(())
    }

    #[tokio::test]
    async fn returns_inconclusive_unsupported_pool_for_sapling() -> TestResult {
        let network = PaymentNetwork::Mainnet;
        let disclosure = Zip311Disclosure {
            version: ZIP311_VERSION_V1,
            txid: [0x44u8; 32],
            message: b"msg".to_vec(),
            transparent_inputs: Vec::new(),
            sapling_spends: vec![Zip311SaplingSpend {
                index: 0,
                cv: [0u8; 32],
                rk: [0u8; 32],
                zkproof: vec![0u8; 192],
                address_proof: None,
                spend_auth_sig: vec![0u8; 64],
            }],
            sapling_outputs: Vec::new(),
        };
        let bytes = encode_signed(&disclosure);
        let verifier = LocalPaymentDisclosureVerifier::new(network);
        let fetcher = ScriptedFetcher::new(Err(FetchError::Unavailable {
            reason: "not consulted".to_owned(),
        }));
        let response = verifier.verify_disclosure(&bytes, &fetcher).await?;
        assert_eq!(
            response.cryptographic_verdict,
            CryptographicVerdict::Inconclusive
        );
        assert_eq!(
            response.inconclusive_reason,
            Some(InconclusiveReason::UnsupportedPool)
        );
        Ok(())
    }

    /// A disclosure with zero inputs of any kind asserts nothing; the
    /// verdict is Malformed and the fetcher is never consulted.
    #[tokio::test]
    async fn returns_malformed_for_empty_disclosure() -> TestResult {
        let network = PaymentNetwork::Mainnet;
        let disclosure = Zip311Disclosure {
            version: ZIP311_VERSION_V1,
            txid: [0x55u8; 32],
            message: b"msg".to_vec(),
            transparent_inputs: Vec::new(),
            sapling_spends: Vec::new(),
            sapling_outputs: Vec::new(),
        };
        let bytes = encode_signed(&disclosure);
        let verifier = LocalPaymentDisclosureVerifier::new(network);
        let fetcher = ScriptedFetcher::new(Err(FetchError::Unavailable {
            reason: "must not be called".to_owned(),
        }));
        let response = verifier.verify_disclosure(&bytes, &fetcher).await?;
        assert_eq!(
            response.cryptographic_verdict,
            CryptographicVerdict::Malformed
        );
        assert_eq!(response.transaction_id, Some(hex::encode([0x55u8; 32])));
        Ok(())
    }

    #[tokio::test]
    async fn returns_inconclusive_unknown_version_for_unknown_version_byte() -> TestResult {
        let network = PaymentNetwork::Mainnet;
        let (mut disclosure, _transaction) = fixture_with_valid_signature(network)?;
        disclosure.version = 0x99;
        let bytes = encode_signed(&disclosure);
        let verifier = LocalPaymentDisclosureVerifier::new(network);
        let fetcher = ScriptedFetcher::new(Err(FetchError::NotFound));
        let response = verifier.verify_disclosure(&bytes, &fetcher).await?;
        assert_eq!(
            response.cryptographic_verdict,
            CryptographicVerdict::Inconclusive
        );
        assert_eq!(
            response.inconclusive_reason,
            Some(InconclusiveReason::UnknownVersion)
        );
        Ok(())
    }

    #[tokio::test]
    async fn returns_inconclusive_prevout_unresolved_when_fetcher_reports_not_found() -> TestResult
    {
        let network = PaymentNetwork::Mainnet;
        let (disclosure, _transaction) = fixture_with_valid_signature(network)?;
        let bytes = encode_signed(&disclosure);
        let verifier = LocalPaymentDisclosureVerifier::new(network);
        let fetcher = ScriptedFetcher::new(Err(FetchError::NotFound));
        let response = verifier.verify_disclosure(&bytes, &fetcher).await?;
        assert_eq!(
            response.cryptographic_verdict,
            CryptographicVerdict::Inconclusive
        );
        assert_eq!(
            response.inconclusive_reason,
            Some(InconclusiveReason::PrevoutUnresolved)
        );
        assert_eq!(response.chain_presence, ChainPresence::NotFound);
        Ok(())
    }

    #[tokio::test]
    async fn returns_inconclusive_prevout_unresolved_when_fetcher_unreachable() -> TestResult {
        let network = PaymentNetwork::Mainnet;
        let (disclosure, _transaction) = fixture_with_valid_signature(network)?;
        let bytes = encode_signed(&disclosure);
        let verifier = LocalPaymentDisclosureVerifier::new(network);
        let fetcher = ScriptedFetcher::new(Err(FetchError::Unavailable {
            reason: "dial timeout".to_owned(),
        }));
        let response = verifier.verify_disclosure(&bytes, &fetcher).await?;
        assert_eq!(
            response.cryptographic_verdict,
            CryptographicVerdict::Inconclusive
        );
        assert_eq!(
            response.inconclusive_reason,
            Some(InconclusiveReason::PrevoutUnresolved)
        );
        assert_eq!(response.chain_presence, ChainPresence::OracleUnavailable);
        Ok(())
    }

    /// Chain-plane integrity check.
    ///
    /// The fetcher must return the transaction the disclosure pins
    /// to. A mismatch means the explorer plane returned the wrong
    /// row, so the verdict short-circuits to `Malformed` without
    /// verifying any signature.
    #[tokio::test]
    async fn returns_malformed_when_fetched_txid_does_not_match_parsed_txid() -> TestResult {
        let network = PaymentNetwork::Mainnet;
        let (disclosure, mut transaction) = fixture_with_valid_signature(network)?;
        // Fetcher returns a transaction whose txid does not match
        // parsed.txid; the explorer plane returned the wrong row.
        transaction.txid = [0x99u8; 32];
        let bytes = encode_signed(&disclosure);
        let verifier = LocalPaymentDisclosureVerifier::new(network);
        let fetcher = ScriptedFetcher::new(Ok(transaction));
        let response = verifier.verify_disclosure(&bytes, &fetcher).await?;
        assert_eq!(
            response.cryptographic_verdict,
            CryptographicVerdict::Malformed
        );
        // transaction_id echoes the parsed disclosure's txid so the
        // caller can see which row was requested.
        assert_eq!(response.transaction_id, Some(hex::encode(disclosure.txid)));
        Ok(())
    }

    #[tokio::test]
    async fn returns_malformed_for_garbage_bytes() -> TestResult {
        let verifier = LocalPaymentDisclosureVerifier::new(PaymentNetwork::Mainnet);
        let fetcher = ScriptedFetcher::new(Err(FetchError::NotFound));
        let response = verifier.verify_disclosure(&[0x00, 0x01], &fetcher).await?;
        assert_eq!(
            response.cryptographic_verdict,
            CryptographicVerdict::Malformed
        );
        Ok(())
    }
}
