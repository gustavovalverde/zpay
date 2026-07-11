use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD;
use serde_json::json;
use zpay_core::settle::PcztSettlementRequest;
use zpay_core::types::{PaymentId, PaymentNetwork, Zatoshis};

use crate::wire::{
    FacilitatorRequest, PaymentRequirements, SupportedExtension, SupportedKind, SupportedResponse,
    X402_VERSION,
};

pub(crate) const ZCASH_EXACT_BINDING_VERSION: &str = "x402-zcash-exact-v1";

pub(crate) const ZCASH_EXACT_SCHEME: &str = "exact";
pub(crate) const ZCASH_EXACT_ASSET: &str = "ZEC";
pub(crate) const ZCASH_EXACT_AMOUNT_UNIT: &str = "zat";
pub(crate) const ZCASH_EXACT_AUTHORIZATION_FORMAT: &str = "pczt-v2-extractable";
pub(crate) const ZPAY_PAYMENT_ID_EXTENSION: &str = "zpayPaymentId";
const ZCASH_EXACT_MAX_ZAT: u64 = 21_000_000 * 100_000_000;
const ZCASH_MAINNET_NETWORK: &str = "zcash:mainnet";
const ZCASH_TESTNET_NETWORK: &str = "zcash:testnet";
const ZCASH_REGTEST_NETWORK: &str = "zcash:regtest";

pub(crate) fn supported_response(network: zally_core::Network) -> SupportedResponse {
    let Some(network_id) = network_id(network) else {
        return SupportedResponse::empty();
    };
    SupportedResponse {
        kinds: vec![SupportedKind {
            x402_version: X402_VERSION,
            scheme: ZCASH_EXACT_SCHEME.to_owned(),
            network: network_id.to_owned(),
            extra: binding_extensions(),
        }],
        extensions: vec![SupportedExtension {
            name: ZCASH_EXACT_BINDING_VERSION.to_owned(),
            version: "1".to_owned(),
        }],
        signers: BTreeMap::from([(
            ZCASH_EXACT_AUTHORIZATION_FORMAT.to_owned(),
            vec!["zspend".to_owned(), "zally-wallet".to_owned()],
        )]),
    }
}

pub(crate) fn request_invalid_reason(request: &FacilitatorRequest) -> Option<&'static str> {
    if !is_zcash_exact_request(request) {
        return None;
    }
    requirements_invalid_reason(&request.payment_requirements)
        .or_else(|| authorization_invalid_reason(&request.payment_payload.payload))
}

pub(crate) fn is_zcash_exact_request(request: &FacilitatorRequest) -> bool {
    has_zcash_exact_network(&request.payment_requirements)
}

pub(crate) fn response_extensions(
    request: &FacilitatorRequest,
) -> BTreeMap<String, serde_json::Value> {
    if !has_zcash_exact_network(&request.payment_requirements) {
        return BTreeMap::new();
    }

    binding_extensions()
}

pub(crate) fn settlement_request(
    request: &FacilitatorRequest,
) -> Result<PcztSettlementRequest, &'static str> {
    if !is_zcash_exact_request(request) {
        return Err("scheme_network_not_supported");
    }
    if let Some(reason) = request_invalid_reason(request) {
        return Err(reason);
    }
    let network = payment_network(&request.payment_requirements.network)
        .ok_or("zcash_exact_network_invalid")?;
    let amount_zat = request
        .payment_requirements
        .amount
        .parse::<u64>()
        .map_err(|_| "zcash_exact_amount_invalid")?;
    let authorization = request
        .payment_payload
        .payload
        .as_object()
        .ok_or("zcash_exact_authorization_malformed")?;
    let pczt = authorization
        .get("pczt")
        .and_then(serde_json::Value::as_str)
        .ok_or("zcash_exact_authorization_malformed")?;
    let pczt_bytes = BASE64_URL_SAFE_NO_PAD
        .decode(pczt)
        .map_err(|_| "zcash_exact_authorization_malformed")?;
    Ok(PcztSettlementRequest {
        network,
        amount_zat: Zatoshis(amount_zat),
        pay_to: request.payment_requirements.pay_to.clone(),
        pczt_bytes,
    })
}

pub(crate) fn zpay_payment_id(
    requirements: &PaymentRequirements,
) -> Result<Option<PaymentId>, &'static str> {
    let Some(raw_payment_id) = requirements
        .extra
        .get(ZPAY_PAYMENT_ID_EXTENSION)
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    raw_payment_id
        .parse::<PaymentId>()
        .map(Some)
        .map_err(|_| "zpay_payment_id_invalid")
}

pub(crate) fn network_id(network: zally_core::Network) -> Option<&'static str> {
    match network {
        zally_core::Network::Mainnet => Some(ZCASH_MAINNET_NETWORK),
        zally_core::Network::Testnet => Some(ZCASH_TESTNET_NETWORK),
        zally_core::Network::Regtest(_) => Some(ZCASH_REGTEST_NETWORK),
        #[allow(
            clippy::wildcard_enum_match_arm,
            reason = "zally_core::Network is non_exhaustive; future networks must not be advertised as Zcash exact without a deliberate binding update"
        )]
        _ => None,
    }
}

fn binding_extensions() -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([
        ("binding".to_owned(), json!(ZCASH_EXACT_BINDING_VERSION)),
        ("bindingStatus".to_owned(), json!("implemented")),
        ("amountUnit".to_owned(), json!(ZCASH_EXACT_AMOUNT_UNIT)),
        ("asset".to_owned(), json!(ZCASH_EXACT_ASSET)),
        (
            "authorizationFormat".to_owned(),
            json!(ZCASH_EXACT_AUTHORIZATION_FORMAT),
        ),
        (
            "settlementPosture".to_owned(),
            json!("extracts_and_broadcasts_pczt"),
        ),
    ])
}

fn requirements_invalid_reason(requirements: &PaymentRequirements) -> Option<&'static str> {
    if requirements.scheme != ZCASH_EXACT_SCHEME {
        return Some("zcash_exact_scheme_invalid");
    }
    if !is_zcash_network(&requirements.network) {
        return Some("zcash_exact_network_invalid");
    }
    if requirements.asset != ZCASH_EXACT_ASSET {
        return Some("zcash_exact_asset_unsupported");
    }
    if !is_valid_zat_amount(&requirements.amount) {
        return Some("zcash_exact_amount_invalid");
    }
    if !has_valid_recipient_prefix(&requirements.network, &requirements.pay_to) {
        return Some("zcash_exact_pay_to_invalid");
    }
    None
}

fn authorization_invalid_reason(authorization: &serde_json::Value) -> Option<&'static str> {
    let Some(authorization_fields) = authorization.as_object() else {
        return Some("zcash_exact_authorization_malformed");
    };
    let Some(format) = authorization_fields
        .get("format")
        .and_then(serde_json::Value::as_str)
    else {
        return Some("zcash_exact_authorization_malformed");
    };
    if format != ZCASH_EXACT_AUTHORIZATION_FORMAT {
        return Some("zcash_exact_authorization_format_unsupported");
    }
    if authorization_fields
        .get("pczt")
        .and_then(serde_json::Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Some("zcash_exact_authorization_malformed");
    }
    if let Some(pczt) = authorization_fields
        .get("pczt")
        .and_then(serde_json::Value::as_str)
        && BASE64_URL_SAFE_NO_PAD.decode(pczt).is_err()
    {
        return Some("zcash_exact_authorization_malformed");
    }
    None
}

fn has_zcash_exact_network(requirements: &PaymentRequirements) -> bool {
    requirements.scheme == ZCASH_EXACT_SCHEME && is_zcash_network(&requirements.network)
}

fn is_zcash_network(network: &str) -> bool {
    matches!(
        network,
        ZCASH_MAINNET_NETWORK | ZCASH_TESTNET_NETWORK | ZCASH_REGTEST_NETWORK
    )
}

fn payment_network(network: &str) -> Option<PaymentNetwork> {
    match network {
        ZCASH_MAINNET_NETWORK => Some(PaymentNetwork::Mainnet),
        ZCASH_TESTNET_NETWORK => Some(PaymentNetwork::Testnet),
        ZCASH_REGTEST_NETWORK => Some(PaymentNetwork::Regtest),
        _ => None,
    }
}

fn is_valid_zat_amount(amount_zat: &str) -> bool {
    amount_zat
        .parse::<u64>()
        .is_ok_and(|parsed_zat| parsed_zat > 0 && parsed_zat <= ZCASH_EXACT_MAX_ZAT)
}

fn has_valid_recipient_prefix(network: &str, pay_to: &str) -> bool {
    match network {
        ZCASH_MAINNET_NETWORK => {
            pay_to.starts_with("u1")
                || pay_to.starts_with("zu1")
                || pay_to.starts_with("zs1")
        }
        ZCASH_TESTNET_NETWORK => {
            pay_to.starts_with("utest1")
                || pay_to.starts_with("zutest1")
                || pay_to.starts_with("ztestsapling1")
        }
        ZCASH_REGTEST_NETWORK => {
            pay_to.starts_with("uregtest1")
                || pay_to.starts_with("zuregtest1")
                || pay_to.starts_with("zregtestsapling1")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ZCASH_EXACT_AUTHORIZATION_FORMAT, ZCASH_EXACT_BINDING_VERSION, request_invalid_reason,
        response_extensions,
    };
    use crate::wire::{FacilitatorRequest, PaymentPayload, PaymentRequirements, ResourceInfo};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn requirements() -> PaymentRequirements {
        PaymentRequirements {
            scheme: "exact".to_owned(),
            network: "zcash:testnet".to_owned(),
            amount: "10000".to_owned(),
            asset: "ZEC".to_owned(),
            pay_to: "utest1recipientaddress".to_owned(),
            max_timeout_seconds: 60,
            extra: BTreeMap::new(),
        }
    }

    fn request_with(
        requirements: PaymentRequirements,
        authorization: serde_json::Value,
    ) -> FacilitatorRequest {
        FacilitatorRequest {
            x402_version: 2,
            payment_payload: PaymentPayload {
                x402_version: 2,
                resource: ResourceInfo {
                    url: "https://merchant.example/resource".to_owned(),
                    description: None,
                    mime_type: None,
                    service_name: None,
                    tags: Vec::new(),
                    icon_url: None,
                },
                accepted: requirements.clone(),
                payload: authorization,
                extensions: BTreeMap::new(),
            },
            payment_requirements: requirements,
        }
    }

    #[test]
    fn accepts_well_formed_request_for_settlement_verification() {
        let request = request_with(
            requirements(),
            json!({
                "format": ZCASH_EXACT_AUTHORIZATION_FORMAT,
                "pczt": "UENaVAIAAAAA",
            }),
        );

        assert_eq!(request_invalid_reason(&request), None);
        assert_eq!(
            response_extensions(&request)
                .get("binding")
                .and_then(serde_json::Value::as_str),
            Some(ZCASH_EXACT_BINDING_VERSION),
        );
        assert_eq!(
            response_extensions(&request)
                .get("bindingStatus")
                .and_then(serde_json::Value::as_str),
            Some("implemented"),
        );
    }

    #[test]
    fn rejects_decimal_or_zero_atomic_amounts() {
        let mut decimal = requirements();
        decimal.amount = "0.1".to_owned();
        let request = request_with(decimal, json!({}));
        assert_eq!(
            request_invalid_reason(&request),
            Some("zcash_exact_amount_invalid"),
        );

        let mut zero = requirements();
        zero.amount = "0".to_owned();
        let request = request_with(zero, json!({}));
        assert_eq!(
            request_invalid_reason(&request),
            Some("zcash_exact_amount_invalid"),
        );
    }

    #[test]
    fn rejects_network_mismatched_unified_address() {
        let mut invalid = requirements();
        invalid.network = "zcash:mainnet".to_owned();
        invalid.pay_to = "utest1recipientaddress".to_owned();
        let request = request_with(invalid, json!({}));

        assert_eq!(
            request_invalid_reason(&request),
            Some("zcash_exact_pay_to_invalid"),
        );
    }

    #[test]
    fn accepts_network_matched_testnet_sapling_address() {
        let mut sapling = requirements();
        sapling.pay_to = "ztestsapling12p79hg7sffq7j2ukmpur208cyy7cxdr4mkwnn8eh09w3hgnysv6dmtwuwy8z7e6lvgmngrxeh6g".to_owned();
        let request = request_with(
            sapling,
            json!({
                "format": ZCASH_EXACT_AUTHORIZATION_FORMAT,
                "pczt": "UENaVAIAAAAA",
            }),
        );

        assert_eq!(request_invalid_reason(&request), None);
    }

    #[test]
    fn rejects_wrong_authorization_format() {
        let request = request_with(
            requirements(),
            json!({
                "format": "raw-zcash-transaction-v5",
                "rawTxHex": "00",
            }),
        );

        assert_eq!(
            request_invalid_reason(&request),
            Some("zcash_exact_authorization_format_unsupported"),
        );
    }
}
