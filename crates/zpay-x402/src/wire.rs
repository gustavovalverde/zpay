//! Official x402 v2 wire types and HTTP header codecs.
//!
//! This module is the only place in zpay that preserves standards-owned
//! x402 field names such as `paymentPayload` and `payload`. The facilitator
//! and demo gateway translate these shapes into zpay domain names at the
//! boundary.

use std::collections::BTreeMap;

use axum::http::HeaderValue;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Official x402 protocol version implemented by this adapter.
pub const X402_VERSION: u32 = 2;

/// HTTP response header carrying a base64-encoded [`PaymentRequired`] value.
pub const PAYMENT_REQUIRED_HEADER: &str = "PAYMENT-REQUIRED";

/// HTTP request header carrying a base64-encoded [`PaymentPayload`] value.
pub const PAYMENT_SIGNATURE_HEADER: &str = "PAYMENT-SIGNATURE";

/// HTTP response header carrying a base64-encoded [`SettleResponse`] value.
pub const PAYMENT_RESPONSE_HEADER: &str = "PAYMENT-RESPONSE";

/// x402 v2 payment requirements advertised by a protected resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequirements {
    /// Payment scheme, for example `exact`.
    pub scheme: String,
    /// CAIP-2 network identifier.
    pub network: String,
    /// Decimal string amount in the asset's base unit.
    pub amount: String,
    /// Asset identifier understood by the scheme and network binding.
    pub asset: String,
    /// Recipient account or address.
    pub pay_to: String,
    /// Maximum payment validity window in seconds.
    pub max_timeout_seconds: u64,
    /// Scheme-specific requirements.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Protected resource metadata carried in x402 payment messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceInfo {
    /// URL of the protected resource.
    pub url: String,
    /// Optional resource description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional response MIME type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Optional service name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    /// Optional discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Optional service icon URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}

/// x402 v2 402 response body encoded into `PAYMENT-REQUIRED`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentRequired {
    /// x402 protocol version.
    pub x402_version: u32,
    /// Protected resource metadata.
    pub resource: ResourceInfo,
    /// Payment options accepted by the resource.
    pub accepts: Vec<PaymentRequirements>,
    /// Optional protocol extensions advertised by the resource.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// x402 v2 payment authorization encoded into `PAYMENT-SIGNATURE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentPayload {
    /// x402 protocol version.
    pub x402_version: u32,
    /// Protected resource metadata the payment authorizes.
    pub resource: ResourceInfo,
    /// The payment requirements selected by the payer.
    pub accepted: PaymentRequirements,
    /// Scheme-specific authorization material.
    pub payload: serde_json::Value,
    /// Optional extension values echoed by the payer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Request body accepted by official x402 facilitator `verify` and `settle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FacilitatorRequest {
    /// x402 protocol version.
    pub x402_version: u32,
    /// Payer authorization material.
    pub payment_payload: PaymentPayload,
    /// Resource requirements the facilitator must verify against.
    pub payment_requirements: PaymentRequirements,
}

/// One scheme and network pair supported by this facilitator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportedKind {
    /// x402 protocol version.
    pub x402_version: u32,
    /// Payment scheme, for example `exact`.
    pub scheme: String,
    /// CAIP-2 network identifier.
    pub network: String,
    /// Scheme-specific support metadata.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// One extension advertised by this facilitator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportedExtension {
    /// Extension name.
    pub name: String,
    /// Extension version.
    pub version: String,
}

/// Official x402 facilitator `GET /supported` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportedResponse {
    /// Supported scheme and network pairs.
    pub kinds: Vec<SupportedKind>,
    /// Supported extension descriptors.
    #[serde(default)]
    pub extensions: Vec<SupportedExtension>,
    /// Optional signer hints, keyed by scheme or network binding.
    #[serde(default)]
    pub signers: BTreeMap<String, Vec<String>>,
}

impl SupportedResponse {
    /// Return an official response for a facilitator with no advertised payment
    /// kind.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            kinds: Vec::new(),
            extensions: Vec::new(),
            signers: BTreeMap::new(),
        }
    }
}

/// Official x402 facilitator `POST /verify` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResponse {
    /// Whether the payment authorization satisfies the requirements.
    pub is_valid: bool,
    /// Machine-readable invalid reason when `is_valid` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_reason: Option<String>,
    /// Payer account when the scheme can identify it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
    /// Scheme-specific verification details.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Official x402 facilitator `POST /settle` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettleResponse {
    /// Whether settlement completed.
    pub success: bool,
    /// Machine-readable failure reason when `success` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
    /// Payer account when the scheme can identify it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
    /// Chain transaction identifier when settlement succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction: Option<String>,
    /// CAIP-2 network identifier.
    pub network: String,
    /// Settled amount as a decimal string in the asset's base unit.
    pub amount: String,
    /// Optional extension values produced by settlement.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Errors raised while encoding or decoding x402 header values.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WireHeaderError {
    /// JSON serialization failed. Retry posture: `not_retryable`.
    #[error("x402 header JSON serialization failed: {reason}")]
    Serialization {
        /// Serialization failure reason.
        reason: String,
    },
    /// The encoded header was not a valid HTTP header value. Retry posture:
    /// `not_retryable`.
    #[error("x402 header value invalid: {reason}")]
    HeaderValue {
        /// Header encoding failure reason.
        reason: String,
    },
    /// The raw header was not valid visible text. Retry posture:
    /// `not_retryable`.
    #[error("x402 header text invalid: {reason}")]
    HeaderText {
        /// Header text failure reason.
        reason: String,
    },
    /// Base64 decoding failed. Retry posture: `not_retryable`.
    #[error("x402 header base64 decode failed: {reason}")]
    Base64 {
        /// Base64 decoder failure reason.
        reason: String,
    },
    /// JSON decoding failed. Retry posture: `not_retryable`.
    #[error("x402 header JSON decode failed: {reason}")]
    Json {
        /// JSON decoder failure reason.
        reason: String,
    },
}

/// Encode a [`PaymentRequired`] value for the `PAYMENT-REQUIRED` header.
pub fn encode_payment_required_header(
    payment_required: &PaymentRequired,
) -> Result<HeaderValue, WireHeaderError> {
    encode_json_header(payment_required)
}

/// Decode a [`PaymentPayload`] value from the `PAYMENT-SIGNATURE` header.
pub fn decode_payment_signature_header(
    header: &HeaderValue,
) -> Result<PaymentPayload, WireHeaderError> {
    decode_json_header(header)
}

/// Encode a [`SettleResponse`] value for the `PAYMENT-RESPONSE` header.
pub fn encode_payment_response_header(
    settle_response: &SettleResponse,
) -> Result<HeaderValue, WireHeaderError> {
    encode_json_header(settle_response)
}

fn encode_json_header<T>(body: &T) -> Result<HeaderValue, WireHeaderError>
where
    T: Serialize,
{
    let json = serde_json::to_vec(body).map_err(|err| WireHeaderError::Serialization {
        reason: err.to_string(),
    })?;
    HeaderValue::from_str(&BASE64_STANDARD.encode(json)).map_err(|err| {
        WireHeaderError::HeaderValue {
            reason: err.to_string(),
        }
    })
}

fn decode_json_header<T>(header: &HeaderValue) -> Result<T, WireHeaderError>
where
    T: DeserializeOwned,
{
    let encoded = header.to_str().map_err(|err| WireHeaderError::HeaderText {
        reason: err.to_string(),
    })?;
    let json = BASE64_STANDARD
        .decode(encoded)
        .map_err(|err| WireHeaderError::Base64 {
            reason: err.to_string(),
        })?;
    serde_json::from_slice(&json).map_err(|err| WireHeaderError::Json {
        reason: err.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        PAYMENT_REQUIRED_HEADER, PAYMENT_RESPONSE_HEADER, PAYMENT_SIGNATURE_HEADER, PaymentPayload,
        PaymentRequired, PaymentRequirements, ResourceInfo, SettleResponse,
        decode_payment_signature_header, encode_payment_required_header,
        encode_payment_response_header,
    };
    use axum::http::HeaderValue;
    use base64::Engine as _;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn requirements() -> PaymentRequirements {
        PaymentRequirements {
            scheme: "exact".to_owned(),
            network: "eip155:8453".to_owned(),
            amount: "10000".to_owned(),
            asset: "0x0000000000000000000000000000000000000000".to_owned(),
            pay_to: "0x1111111111111111111111111111111111111111".to_owned(),
            max_timeout_seconds: 60,
            extra: BTreeMap::new(),
        }
    }

    fn resource() -> ResourceInfo {
        ResourceInfo {
            url: "https://resource.example/report".to_owned(),
            description: Some("Premium report".to_owned()),
            mime_type: Some("application/json".to_owned()),
            service_name: None,
            tags: Vec::new(),
            icon_url: None,
        }
    }

    #[test]
    fn header_names_match_official_transport() {
        assert_eq!(PAYMENT_REQUIRED_HEADER, "PAYMENT-REQUIRED");
        assert_eq!(PAYMENT_SIGNATURE_HEADER, "PAYMENT-SIGNATURE");
        assert_eq!(PAYMENT_RESPONSE_HEADER, "PAYMENT-RESPONSE");
    }

    #[test]
    fn payment_required_serializes_with_official_field_names() -> Result<(), serde_json::Error> {
        let body = PaymentRequired {
            x402_version: 2,
            resource: resource(),
            accepts: vec![requirements()],
            extensions: BTreeMap::new(),
        };
        let encoded = serde_json::to_value(body)?;
        assert_eq!(encoded["x402Version"], 2);
        assert_eq!(encoded["resource"]["url"], resource().url);
        assert_eq!(encoded["accepts"][0]["payTo"], requirements().pay_to);
        assert_eq!(
            encoded["accepts"][0]["maxTimeoutSeconds"],
            requirements().max_timeout_seconds,
        );
        Ok(())
    }

    #[test]
    fn payment_signature_header_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let body = PaymentPayload {
            x402_version: 2,
            resource: resource(),
            accepted: requirements(),
            payload: json!({ "authorization": "0xabc" }),
            extensions: BTreeMap::new(),
        };
        let header = HeaderValue::from_str(
            &base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&body)?),
        )?;
        let decoded = decode_payment_signature_header(&header)?;
        assert_eq!(decoded, body);
        Ok(())
    }

    #[test]
    fn payment_response_header_is_base64_json() -> Result<(), Box<dyn std::error::Error>> {
        let body = SettleResponse {
            success: true,
            error_reason: None,
            payer: Some("0x2222222222222222222222222222222222222222".to_owned()),
            transaction: Some("0xabc".to_owned()),
            network: "eip155:8453".to_owned(),
            amount: "10000".to_owned(),
            extensions: BTreeMap::new(),
        };
        let header = encode_payment_response_header(&body)?;
        let decoded: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD.decode(header.to_str()?)?,
        )?;
        assert_eq!(decoded["success"], true);
        assert_eq!(decoded["errorReason"], serde_json::Value::Null);
        Ok(())
    }

    #[test]
    fn payment_required_header_is_base64_json() -> Result<(), Box<dyn std::error::Error>> {
        let body = PaymentRequired {
            x402_version: 2,
            resource: resource(),
            accepts: vec![requirements()],
            extensions: BTreeMap::new(),
        };
        let header = encode_payment_required_header(&body)?;
        let decoded: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD.decode(header.to_str()?)?,
        )?;
        assert_eq!(decoded["x402Version"], 2);
        Ok(())
    }
}
