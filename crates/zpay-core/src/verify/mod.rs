//! Verify a ZIP-311 payment disclosure against its mined transaction.

use std::sync::Arc;

use sapling::circuit::PreparedSpendVerifyingKey;
use serde::{Deserialize, Serialize};
use zcash_keys::address::Address as ParsedZcashAddress;
use zcash_payment_disclosure::{
    PaymentDisclosure, PaymentDisclosureCodecError, PaymentDisclosureProfile,
    PaymentDisclosureVerificationError, verify_disclosure as verify_zip311_disclosure,
};
use zcash_protocol::consensus::BlockHeight;

use crate::disclosure_fetcher::{DisclosedTransaction, DisclosureFetcher, FetchError};
use crate::types::Zatoshis;

/// Input to [`verify`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyRequest {
    /// Transaction identifier whose receipt is being verified.
    pub txid: String,
    /// Amount the merchant expected to receive.
    pub expected_amount_zat: Zatoshis,
    /// Unified Address the merchant expected the payment to target.
    pub expected_pay_to: String,
    /// ZIP-311 message the merchant expected, hex-encoded.
    pub expected_disclosure_message_hex: String,
    /// ZIP-311 payment-disclosure bytes, hex-encoded.
    pub disclosure_payload_hex: String,
}

/// Wire response shape of `POST /zpay/v1/verify`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResponse {
    /// Cryptographic verdict over the disclosure bytes.
    pub cryptographic_verdict: CryptographicVerdict,
    /// Reason a cryptographic verdict was inconclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inconclusive_reason: Option<InconclusiveReason>,
    /// On-chain presence verdict.
    pub chain_presence: ChainPresence,
    /// Amount-match verdict.
    pub amount_reconciliation: AmountReconciliation,
    /// Recipient-match verdict for the authenticated disclosed outputs.
    pub recipient_reconciliation: RecipientReconciliation,
    /// Message-match verdict after cryptographic verification.
    pub message_reconciliation: MessageReconciliation,
    /// Hex-encoded ZIP-244 transaction id when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    /// Payment id when the disclosure matched a prepared row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payment_id: Option<String>,
    /// Sum of the authenticated disclosed Sapling outputs, in zatoshis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosed_value_zat: Option<u64>,
}

/// Cryptographic-axis verdict over the disclosure bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CryptographicVerdict {
    /// Every selected spend and output disclosure verified.
    Valid,
    /// A spend-authority proof or output recovery failed.
    InvalidSignature,
    /// Disclosure or transaction bytes were structurally invalid.
    Malformed,
    /// The verifier could not answer definitively.
    Inconclusive,
}

/// Sub-classification when [`CryptographicVerdict::Inconclusive`] is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InconclusiveReason {
    /// Disclosure references a pool this Draft1 implementation does not support.
    UnsupportedPool,
    /// Disclosure carries a profile byte this build does not recognize.
    UnknownVersion,
    /// Retained for wire compatibility with the earlier transparent verifier.
    PrevoutUnresolved,
    /// The mined transaction bytes required for verification were unavailable.
    TransactionUnavailable,
}

/// Chain-presence axis verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChainPresence {
    /// Chain plane returned the mined transaction and its bytes.
    Mined,
    /// Chain plane has no mined record of the transaction.
    NotFound,
    /// Chain plane could not serve the transaction.
    OracleUnavailable,
}

/// Amount-reconciliation axis verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AmountReconciliation {
    /// Authenticated disclosed outputs sum to the expected amount.
    Match,
    /// Authenticated disclosed outputs do not sum to the expected amount.
    Mismatch,
    /// No Sapling output was disclosed, so reconciliation was impossible.
    NotChecked,
}

/// Recipient-reconciliation axis verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RecipientReconciliation {
    /// Every authenticated disclosed output targets `expected_pay_to`.
    Match,
    /// At least one authenticated disclosed output targets another recipient.
    Mismatch,
    /// No Sapling output was disclosed, so reconciliation was impossible.
    NotChecked,
}

/// Disclosure-message reconciliation axis verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MessageReconciliation {
    /// Authenticated disclosure message equals the expected message.
    Match,
    /// Authenticated disclosure message differs from the expected message.
    Mismatch,
    /// Cryptographic verification did not authenticate the message.
    NotChecked,
}

/// Errors raised before an in-band verification verdict can be produced.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// Disclosure envelope was not hex. Retry posture: `not_retryable`.
    #[error("disclosure_payload_hex must be valid hex: {reason}")]
    PayloadInvalid {
        /// Decoder reason.
        reason: String,
    },
    /// Expected payee lacks the profile's receiver on the configured network. Retry posture: `not_retryable`.
    #[error("expected_pay_to lacks the disclosure profile's receiver on the configured network")]
    ExpectedPayToInvalid,
    /// Expected disclosure message was not hex. Retry posture: `not_retryable`.
    #[error("expected_disclosure_message_hex must be valid hex: {reason}")]
    ExpectedDisclosureMessageInvalid {
        /// Decoder reason.
        reason: String,
    },
}

/// Authenticated facts returned by a disclosure cryptography implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPaymentDisclosure {
    sapling_outputs: Vec<VerifiedSaplingOutput>,
    ironwood_outputs: Vec<VerifiedIronwoodOutput>,
}

impl VerifiedPaymentDisclosure {
    /// Constructs authenticated disclosure facts.
    #[must_use]
    pub fn new(sapling_outputs: Vec<VerifiedSaplingOutput>) -> Self {
        Self {
            sapling_outputs,
            ironwood_outputs: Vec::new(),
        }
    }

    /// Returns authenticated Sapling output facts.
    #[must_use]
    pub fn sapling_outputs(&self) -> &[VerifiedSaplingOutput] {
        &self.sapling_outputs
    }

    /// Adds authenticated Ironwood output facts.
    #[must_use]
    pub fn with_ironwood_outputs(mut self, ironwood_outputs: Vec<VerifiedIronwoodOutput>) -> Self {
        self.ironwood_outputs = ironwood_outputs;
        self
    }

    /// Returns authenticated Ironwood output facts.
    #[must_use]
    pub fn ironwood_outputs(&self) -> &[VerifiedIronwoodOutput] {
        &self.ironwood_outputs
    }
}

/// Authenticated facts recovered from one disclosed Sapling output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedSaplingOutput {
    index: u32,
    recipient_bytes: [u8; 43],
    amount_zat: u64,
}

impl VerifiedSaplingOutput {
    /// Constructs authenticated Sapling output facts.
    #[must_use]
    pub const fn new(index: u32, recipient_bytes: [u8; 43], amount_zat: u64) -> Self {
        Self {
            index,
            recipient_bytes,
            amount_zat,
        }
    }

    /// Returns the output index in the mined Sapling bundle.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// Returns the canonical 43-byte Sapling payment address.
    #[must_use]
    pub const fn recipient_bytes(self) -> [u8; 43] {
        self.recipient_bytes
    }

    /// Returns the authenticated output amount in zatoshis.
    #[must_use]
    pub const fn amount_zat(self) -> u64 {
        self.amount_zat
    }
}

/// Authenticated facts recovered from one disclosed Ironwood output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedIronwoodOutput {
    index: u32,
    recipient_bytes: [u8; 43],
    amount_zat: u64,
}

impl VerifiedIronwoodOutput {
    /// Constructs authenticated Ironwood output facts.
    #[must_use]
    pub const fn new(index: u32, recipient_bytes: [u8; 43], amount_zat: u64) -> Self {
        Self {
            index,
            recipient_bytes,
            amount_zat,
        }
    }

    /// Returns the action index in the mined Ironwood bundle.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// Returns the canonical 43-byte Ironwood receiver.
    #[must_use]
    pub const fn recipient_bytes(self) -> [u8; 43] {
        self.recipient_bytes
    }

    /// Returns the authenticated output amount in zatoshis.
    #[must_use]
    pub const fn amount_zat(self) -> u64 {
        self.amount_zat
    }
}

/// Failure returned by a disclosure cryptography implementation.
#[derive(Debug, thiserror::Error)]
#[error("ZIP-311 disclosure verification failed: {source}")]
pub struct DisclosureVerificationError {
    #[source]
    source: PaymentDisclosureVerificationError,
}

impl DisclosureVerificationError {
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "the upstream error is non-exhaustive; future verification failures fail closed as malformed until zpay classifies them explicitly"
    )]
    fn verdict(&self) -> CryptographicVerdict {
        match self.source {
            PaymentDisclosureVerificationError::IronwoodSpendAuthorityInvalid { .. }
            | PaymentDisclosureVerificationError::IronwoodOutputRecoveryFailed { .. }
            | PaymentDisclosureVerificationError::SpendAuthorityInvalid { .. }
            | PaymentDisclosureVerificationError::OutputRecoveryFailed { .. } => {
                CryptographicVerdict::InvalidSignature
            }
            PaymentDisclosureVerificationError::TransactionMalformed { .. }
            | PaymentDisclosureVerificationError::TransactionTrailingBytes { .. }
            | PaymentDisclosureVerificationError::TransactionIdMismatch { .. }
            | PaymentDisclosureVerificationError::IronwoodBundleMissing
            | PaymentDisclosureVerificationError::IronwoodActionIndexOutOfBounds { .. }
            | PaymentDisclosureVerificationError::SaplingBundleMissing
            | PaymentDisclosureVerificationError::SpendIndexOutOfBounds { .. }
            | PaymentDisclosureVerificationError::OutputIndexOutOfBounds { .. }
            | PaymentDisclosureVerificationError::SpendCommitmentMalformed { .. }
            | PaymentDisclosureVerificationError::RandomizedVerificationKeyMalformed { .. }
            | PaymentDisclosureVerificationError::SpendProofMalformed { .. }
            | _ => CryptographicVerdict::Malformed,
        }
    }
}

struct PaymentReconciliation {
    amount: AmountReconciliation,
    recipient: RecipientReconciliation,
    disclosed_value_zat: Option<u64>,
}

#[derive(Clone, Copy)]
enum ExpectedShieldedRecipient {
    Sapling([u8; 43]),
    Ironwood([u8; 43]),
}

/// Cryptographic boundary used by the verification driver.
pub trait PaymentDisclosureVerifier: Send + Sync {
    /// Verify a parsed disclosure against its exact mined transaction.
    fn verify_disclosure(
        &self,
        disclosure: &PaymentDisclosure,
        transaction: &DisclosedTransaction,
    ) -> Result<VerifiedPaymentDisclosure, DisclosureVerificationError>;
}

/// In-process verifier backed by `zcash-payment-disclosure`.
pub struct LocalPaymentDisclosureVerifier {
    params: zally_core::NetworkParameters,
    spend_verifying_key: Arc<PreparedSpendVerifyingKey>,
}

impl LocalPaymentDisclosureVerifier {
    /// Builds a verifier for one network and prepared Sapling Spend verifying key.
    #[must_use]
    pub fn new(
        network: zally_core::Network,
        spend_verifying_key: PreparedSpendVerifyingKey,
    ) -> Self {
        Self {
            params: network.to_parameters(),
            spend_verifying_key: Arc::new(spend_verifying_key),
        }
    }
}

impl PaymentDisclosureVerifier for LocalPaymentDisclosureVerifier {
    fn verify_disclosure(
        &self,
        disclosure: &PaymentDisclosure,
        transaction: &DisclosedTransaction,
    ) -> Result<VerifiedPaymentDisclosure, DisclosureVerificationError> {
        let evidence = verify_zip311_disclosure(
            disclosure,
            &transaction.raw_transaction_bytes,
            BlockHeight::from_u32(transaction.mined_height),
            &self.params,
            &self.spend_verifying_key,
        )
        .map_err(|source| DisclosureVerificationError { source })?;
        let sapling_outputs = evidence
            .sapling_outputs()
            .iter()
            .map(|output| {
                VerifiedSaplingOutput::new(
                    output.index(),
                    output.recipient().to_bytes(),
                    output.amount_zat(),
                )
            })
            .collect();
        let ironwood_outputs = evidence
            .ironwood_outputs()
            .iter()
            .map(|output| {
                VerifiedIronwoodOutput::new(
                    output.index(),
                    output.recipient().to_raw_address_bytes(),
                    output.amount_zat(),
                )
            })
            .collect();
        Ok(VerifiedPaymentDisclosure::new(sapling_outputs).with_ironwood_outputs(ironwood_outputs))
    }
}

/// Drive a verify request through parsing, chain lookup, cryptography, and reconciliation.
///
/// # Errors
///
/// Returns [`VerifyError::PayloadInvalid`] when the disclosure hex envelope is
/// invalid, [`VerifyError::ExpectedDisclosureMessageInvalid`] when the
/// expected message hex is invalid, or [`VerifyError::ExpectedPayToInvalid`]
/// when the expected recipient cannot be reconciled on `network`.
pub async fn verify<V, F>(
    request: VerifyRequest,
    network: zally_core::Network,
    verifier: &V,
    fetcher: &F,
) -> Result<VerifyResponse, VerifyError>
where
    V: PaymentDisclosureVerifier,
    F: DisclosureFetcher,
{
    let expected_disclosure_message = hex::decode(&request.expected_disclosure_message_hex)
        .map_err(|error| VerifyError::ExpectedDisclosureMessageInvalid {
            reason: error.to_string(),
        })?;
    let disclosure_bytes = hex::decode(request.disclosure_payload_hex).map_err(|error| {
        VerifyError::PayloadInvalid {
            reason: error.to_string(),
        }
    })?;
    let disclosure = match PaymentDisclosure::from_bytes(&disclosure_bytes) {
        Ok(disclosure) => disclosure,
        Err(PaymentDisclosureCodecError::ProfileUnsupported { .. }) => {
            return Ok(short_circuit_inconclusive(
                InconclusiveReason::UnknownVersion,
                None,
                ChainPresence::OracleUnavailable,
            ));
        }
        Err(_) => return Ok(short_circuit_malformed(None)),
    };
    let expected_recipient =
        expected_shielded_recipient(network, &request.expected_pay_to, disclosure.profile())?;
    let rpc_txid = rpc_transaction_id_bytes(disclosure.transaction_id());
    let rpc_txid_hex = hex::encode(rpc_txid);
    if !request_txid_matches(&request.txid, &rpc_txid) {
        return Ok(short_circuit_malformed(Some(rpc_txid_hex)));
    }

    let transaction = match fetcher.fetch_transaction(rpc_txid).await {
        Ok(transaction) => transaction,
        Err(error) => {
            return Ok(short_circuit_inconclusive(
                InconclusiveReason::TransactionUnavailable,
                Some(rpc_txid_hex),
                chain_presence_for(&error),
            ));
        }
    };
    if transaction.txid != rpc_txid {
        return Ok(short_circuit_malformed(Some(rpc_txid_hex)));
    }

    let authenticated_disclosure = match verifier.verify_disclosure(&disclosure, &transaction) {
        Ok(authenticated_disclosure) => authenticated_disclosure,
        Err(error) => {
            return Ok(verification_failure_response(&error, rpc_txid_hex));
        }
    };
    let Some(reconciliation) = compute_payment_reconciliation(
        &authenticated_disclosure,
        request.expected_amount_zat.0,
        expected_recipient,
    ) else {
        return Ok(short_circuit_malformed(Some(rpc_txid_hex)));
    };

    Ok(VerifyResponse {
        cryptographic_verdict: CryptographicVerdict::Valid,
        inconclusive_reason: None,
        chain_presence: ChainPresence::Mined,
        amount_reconciliation: reconciliation.amount,
        recipient_reconciliation: reconciliation.recipient,
        message_reconciliation: if disclosure.message() == expected_disclosure_message {
            MessageReconciliation::Match
        } else {
            MessageReconciliation::Mismatch
        },
        transaction_id: Some(rpc_txid_hex),
        payment_id: None,
        disclosed_value_zat: reconciliation.disclosed_value_zat,
    })
}

fn verification_failure_response(
    error: &DisclosureVerificationError,
    rpc_txid_hex: String,
) -> VerifyResponse {
    VerifyResponse {
        cryptographic_verdict: error.verdict(),
        inconclusive_reason: None,
        chain_presence: ChainPresence::Mined,
        amount_reconciliation: AmountReconciliation::NotChecked,
        recipient_reconciliation: RecipientReconciliation::NotChecked,
        message_reconciliation: MessageReconciliation::NotChecked,
        transaction_id: Some(rpc_txid_hex),
        payment_id: None,
        disclosed_value_zat: None,
    }
}

fn compute_payment_reconciliation(
    authenticated_disclosure: &VerifiedPaymentDisclosure,
    expected_amount_zat: u64,
    expected_recipient: ExpectedShieldedRecipient,
) -> Option<PaymentReconciliation> {
    let (disclosed_value_zat, recipient_matches) = match expected_recipient {
        ExpectedShieldedRecipient::Sapling(expected_recipient_bytes) => {
            let outputs = authenticated_disclosure.sapling_outputs();
            if outputs.is_empty() {
                return Some(unchecked_payment_reconciliation());
            }
            (
                outputs
                    .iter()
                    .try_fold(0_u64, |sum, output| sum.checked_add(output.amount_zat()))?,
                outputs
                    .iter()
                    .all(|output| output.recipient_bytes() == expected_recipient_bytes),
            )
        }
        ExpectedShieldedRecipient::Ironwood(expected_recipient_bytes) => {
            let outputs = authenticated_disclosure.ironwood_outputs();
            if outputs.is_empty() {
                return Some(unchecked_payment_reconciliation());
            }
            (
                outputs
                    .iter()
                    .try_fold(0_u64, |sum, output| sum.checked_add(output.amount_zat()))?,
                outputs
                    .iter()
                    .all(|output| output.recipient_bytes() == expected_recipient_bytes),
            )
        }
    };
    let amount = if disclosed_value_zat == expected_amount_zat {
        AmountReconciliation::Match
    } else {
        AmountReconciliation::Mismatch
    };
    let recipient = if recipient_matches {
        RecipientReconciliation::Match
    } else {
        RecipientReconciliation::Mismatch
    };
    Some(PaymentReconciliation {
        amount,
        recipient,
        disclosed_value_zat: Some(disclosed_value_zat),
    })
}

fn unchecked_payment_reconciliation() -> PaymentReconciliation {
    PaymentReconciliation {
        amount: AmountReconciliation::NotChecked,
        recipient: RecipientReconciliation::NotChecked,
        disclosed_value_zat: None,
    }
}

fn expected_shielded_recipient(
    network: zally_core::Network,
    expected_pay_to: &str,
    profile: PaymentDisclosureProfile,
) -> Result<ExpectedShieldedRecipient, VerifyError> {
    let params = network.to_parameters();
    let Some(ParsedZcashAddress::Unified(unified_address)) =
        ParsedZcashAddress::decode(&params, expected_pay_to)
    else {
        return Err(VerifyError::ExpectedPayToInvalid);
    };
    match profile {
        PaymentDisclosureProfile::Zip311Draft1 => unified_address
            .sapling()
            .map(|recipient| ExpectedShieldedRecipient::Sapling(recipient.to_bytes()))
            .ok_or(VerifyError::ExpectedPayToInvalid),
        PaymentDisclosureProfile::ZallyIronwood => unified_address
            .orchard()
            .map(|recipient| ExpectedShieldedRecipient::Ironwood(recipient.to_raw_address_bytes()))
            .ok_or(VerifyError::ExpectedPayToInvalid),
        _ => Err(VerifyError::ExpectedPayToInvalid),
    }
}

fn rpc_transaction_id_bytes(transaction_id: zcash_protocol::TxId) -> [u8; 32] {
    let mut bytes = *transaction_id.as_ref();
    bytes.reverse();
    bytes
}

fn request_txid_matches(request_txid_hex: &str, parsed_txid: &[u8; 32]) -> bool {
    let trimmed = request_txid_hex
        .strip_prefix("0x")
        .or_else(|| request_txid_hex.strip_prefix("0X"))
        .unwrap_or(request_txid_hex);
    let Ok(bytes) = hex::decode(trimmed) else {
        return false;
    };
    bytes.as_slice() == parsed_txid.as_slice()
}

/// Translate a chain-fetch error into the chain-presence axis.
#[must_use]
pub const fn chain_presence_for(error: &FetchError) -> ChainPresence {
    if matches!(error, FetchError::NotFound) {
        ChainPresence::NotFound
    } else {
        ChainPresence::OracleUnavailable
    }
}

fn short_circuit_malformed(transaction_id: Option<String>) -> VerifyResponse {
    VerifyResponse {
        cryptographic_verdict: CryptographicVerdict::Malformed,
        inconclusive_reason: None,
        chain_presence: ChainPresence::OracleUnavailable,
        amount_reconciliation: AmountReconciliation::NotChecked,
        recipient_reconciliation: RecipientReconciliation::NotChecked,
        message_reconciliation: MessageReconciliation::NotChecked,
        transaction_id,
        payment_id: None,
        disclosed_value_zat: None,
    }
}

fn short_circuit_inconclusive(
    reason: InconclusiveReason,
    transaction_id: Option<String>,
    chain_presence: ChainPresence,
) -> VerifyResponse {
    VerifyResponse {
        cryptographic_verdict: CryptographicVerdict::Inconclusive,
        inconclusive_reason: Some(reason),
        chain_presence,
        amount_reconciliation: AmountReconciliation::NotChecked,
        recipient_reconciliation: RecipientReconciliation::NotChecked,
        message_reconciliation: MessageReconciliation::NotChecked,
        transaction_id,
        payment_id: None,
        disclosed_value_zat: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AmountReconciliation, ChainPresence, CryptographicVerdict, DisclosureVerificationError,
        MessageReconciliation, PaymentDisclosureVerifier, RecipientReconciliation,
        VerifiedPaymentDisclosure, VerifiedSaplingOutput, VerifyRequest, verify,
    };
    use crate::disclosure_fetcher::{DisclosedTransaction, DisclosureFetcher, FetchError};
    use crate::types::Zatoshis;
    use zcash_keys::address::{Address as ParsedZcashAddress, UnifiedAddress};
    use zcash_payment_disclosure::{
        PaymentDisclosure, PaymentDisclosureProfile, SaplingSpendDisclosure,
    };
    use zcash_protocol::TxId;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    struct VerifiedDisclosure {
        amount_zat: u64,
        recipient: sapling::PaymentAddress,
    }

    impl PaymentDisclosureVerifier for VerifiedDisclosure {
        fn verify_disclosure(
            &self,
            _disclosure: &PaymentDisclosure,
            _transaction: &DisclosedTransaction,
        ) -> Result<VerifiedPaymentDisclosure, DisclosureVerificationError> {
            Ok(VerifiedPaymentDisclosure::new(vec![
                VerifiedSaplingOutput::new(0, self.recipient.to_bytes(), self.amount_zat),
            ]))
        }
    }

    struct MinedTransaction;

    impl DisclosureFetcher for MinedTransaction {
        async fn fetch_transaction(
            &self,
            txid: [u8; 32],
        ) -> Result<DisclosedTransaction, FetchError> {
            Ok(DisclosedTransaction {
                txid,
                raw_transaction_bytes: vec![1, 2, 3],
                mined_height: 42,
            })
        }
    }

    fn request(
        expected_amount_zat: u64,
        expected_recipient: sapling::PaymentAddress,
        expected_disclosure_message: &[u8],
    ) -> Result<VerifyRequest, Box<dyn std::error::Error>> {
        let internal_txid = [0x11; 32];
        let disclosure = PaymentDisclosure::new(
            PaymentDisclosureProfile::Zip311Draft1,
            TxId::from_bytes(internal_txid),
            b"merchant-challenge".to_vec(),
            vec![SaplingSpendDisclosure::new(
                0, [1; 32], [2; 32], [3; 192], [4; 64],
            )],
            Vec::new(),
        )?;
        let mut rpc_txid = internal_txid;
        rpc_txid.reverse();
        Ok(VerifyRequest {
            txid: hex::encode(rpc_txid),
            expected_amount_zat: Zatoshis(expected_amount_zat),
            expected_pay_to: unified_address(expected_recipient)?,
            expected_disclosure_message_hex: hex::encode(expected_disclosure_message),
            disclosure_payload_hex: hex::encode(disclosure.to_bytes()),
        })
    }

    fn sapling_recipient(seed: u8) -> sapling::PaymentAddress {
        let spending_key =
            zcash_keys::keys::sapling::spending_key(&[seed; 32], 1, zip32::AccountId::ZERO);
        spending_key
            .to_diversifiable_full_viewing_key()
            .default_address()
            .1
    }

    fn unified_address(recipient: sapling::PaymentAddress) -> Result<String, &'static str> {
        let unified = UnifiedAddress::from_receivers(None, Some(recipient), None)
            .ok_or("could not construct test Unified Address")?;
        Ok(ParsedZcashAddress::Unified(unified)
            .encode(&zally_core::Network::Testnet.to_parameters()))
    }

    #[test]
    fn verify_request_requires_an_expected_disclosure_message() {
        let request_json = serde_json::json!({
            "txid": "11",
            "expected_amount_zat": 50_000,
            "expected_pay_to": "utest1recipient",
            "disclosure_payload_hex": "01"
        });

        assert!(serde_json::from_value::<VerifyRequest>(request_json).is_err());
    }

    #[tokio::test]
    async fn verified_output_reconciles_the_expected_amount() -> TestResult {
        let expected_recipient = sapling_recipient(7);
        let response = verify(
            request(50_000, expected_recipient, b"merchant-challenge")?,
            zally_core::Network::Testnet,
            &VerifiedDisclosure {
                amount_zat: 50_000,
                recipient: expected_recipient,
            },
            &MinedTransaction,
        )
        .await?;

        assert_eq!(response.cryptographic_verdict, CryptographicVerdict::Valid);
        assert_eq!(response.chain_presence, ChainPresence::Mined);
        assert_eq!(response.amount_reconciliation, AmountReconciliation::Match);
        assert_eq!(
            response.recipient_reconciliation,
            RecipientReconciliation::Match
        );
        assert_eq!(
            response.message_reconciliation,
            MessageReconciliation::Match
        );
        assert_eq!(response.disclosed_value_zat, Some(50_000));
        Ok(())
    }

    #[tokio::test]
    async fn same_amount_to_another_recipient_does_not_match() -> TestResult {
        let expected_recipient = sapling_recipient(7);
        let disclosed_recipient = sapling_recipient(8);
        let response = verify(
            request(50_000, expected_recipient, b"merchant-challenge")?,
            zally_core::Network::Testnet,
            &VerifiedDisclosure {
                amount_zat: 50_000,
                recipient: disclosed_recipient,
            },
            &MinedTransaction,
        )
        .await?;

        assert_eq!(response.cryptographic_verdict, CryptographicVerdict::Valid);
        assert_eq!(response.amount_reconciliation, AmountReconciliation::Match);
        assert_eq!(
            response.recipient_reconciliation,
            RecipientReconciliation::Mismatch
        );
        Ok(())
    }

    #[tokio::test]
    async fn another_disclosure_message_does_not_match() -> TestResult {
        let expected_recipient = sapling_recipient(7);
        let response = verify(
            request(50_000, expected_recipient, b"another-challenge")?,
            zally_core::Network::Testnet,
            &VerifiedDisclosure {
                amount_zat: 50_000,
                recipient: expected_recipient,
            },
            &MinedTransaction,
        )
        .await?;

        assert_eq!(response.cryptographic_verdict, CryptographicVerdict::Valid);
        assert_eq!(response.amount_reconciliation, AmountReconciliation::Match);
        assert_eq!(
            response.recipient_reconciliation,
            RecipientReconciliation::Match
        );
        assert_eq!(
            response.message_reconciliation,
            MessageReconciliation::Mismatch
        );
        Ok(())
    }
}
