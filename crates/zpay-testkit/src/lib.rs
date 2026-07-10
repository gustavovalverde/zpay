//! Dev and test fixtures for the DPoP-bound agent payment flow.
//!
//! The testkit centralizes agent-side protocol construction shared by
//! `zpay-demo` and `zpay-e2e`. Each caller owns its own transport-facing error
//! words and maps the typed failures from this crate at that seam.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use p256::ecdsa::SigningKey;
use p256::pkcs8::{EncodePrivateKey as _, LineEnding};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use zally_core::Network;
use zpay_x402::{PaymentPayload, PaymentRequirements, X402_VERSION};
use zspend_core::PaymentAuthorization;

pub use zpay_x402::{FacilitatorRequest, ResourceInfo, X402SettleResponse, X402VerifyResponse};

const PCZT_FORMAT: &str = "pczt-v2-extractable";
const X402_ZCASH_EXACT_BINDING: &str = "x402-zcash-exact-v1";

/// Ephemeral P-256 key material used to create DPoP proofs.
pub struct DpopKey {
    encoding_key: EncodingKey,
    x_coordinate: String,
    y_coordinate: String,
    jkt: String,
}

impl DpopKey {
    /// Generate an ephemeral P-256 DPoP key. Failures are not retryable
    /// without changing the local cryptographic environment.
    pub fn generate() -> Result<Self, DpopError> {
        let signing_key = SigningKey::random(&mut OsRng);
        let point = signing_key.verifying_key().to_encoded_point(false);
        let x_coordinate =
            URL_SAFE_NO_PAD.encode(point.x().ok_or_else(|| DpopError::Encoding {
                reason: "P-256 point missing x coordinate".to_owned(),
            })?);
        let y_coordinate =
            URL_SAFE_NO_PAD.encode(point.y().ok_or_else(|| DpopError::Encoding {
                reason: "P-256 point missing y coordinate".to_owned(),
            })?);
        let pem = signing_key
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|err| DpopError::Encoding {
                reason: err.to_string(),
            })?
            .to_string();
        let encoding_key =
            EncodingKey::from_ec_pem(pem.as_bytes()).map_err(|err| DpopError::Encoding {
                reason: err.to_string(),
            })?;
        let jkt = zspend_core::ec_jwk_thumbprint("P-256", "EC", &x_coordinate, &y_coordinate);
        Ok(Self {
            encoding_key,
            x_coordinate,
            y_coordinate,
            jkt,
        })
    }

    /// Return the JWK thumbprint bound into the access token.
    #[must_use]
    pub fn jkt(&self) -> &str {
        &self.jkt
    }

    /// Mint a DPoP proof for a request without an access-token hash. Failures
    /// are not retryable without changing the local proof inputs.
    pub fn mint_proof(
        &self,
        method: &str,
        proof_url: &str,
        dpop_jti_prefix: &str,
    ) -> Result<String, DpopError> {
        self.mint_proof_inner(method, proof_url, dpop_jti_prefix, None)
    }

    /// Mint a DPoP proof whose `ath` claim binds the supplied access token.
    /// Failures are not retryable without changing the local proof inputs.
    pub fn mint_access_bound_proof(
        &self,
        access_token: &str,
        method: &str,
        proof_url: &str,
        dpop_jti_prefix: &str,
    ) -> Result<String, DpopError> {
        let access_token_hash = URL_SAFE_NO_PAD.encode(Sha256::digest(access_token.as_bytes()));
        self.mint_proof_inner(method, proof_url, dpop_jti_prefix, Some(access_token_hash))
    }

    fn mint_proof_inner(
        &self,
        method: &str,
        proof_url: &str,
        dpop_jti_prefix: &str,
        access_token_hash: Option<String>,
    ) -> Result<String, DpopError> {
        let mut header = Header::new(Algorithm::ES256);
        header.typ = Some("dpop+jwt".to_owned());
        header.jwk = Some(
            serde_json::from_value(serde_json::json!({
                "kty": "EC",
                "crv": "P-256",
                "x": self.x_coordinate,
                "y": self.y_coordinate,
            }))
            .map_err(|err| DpopError::Encoding {
                reason: err.to_string(),
            })?,
        );
        let mut claims = serde_json::json!({
            "htm": method,
            "htu": proof_url,
            "jti": format!("{dpop_jti_prefix}-{}", unix_now_ms()),
            "iat": unix_now_seconds(),
        });
        if let Some(access_token_hash) = access_token_hash {
            claims["ath"] = serde_json::Value::String(access_token_hash);
        }
        encode(&header, &claims, &self.encoding_key).map_err(|err| DpopError::Encoding {
            reason: err.to_string(),
        })
    }
}

/// Failure while creating a DPoP key or proof.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DpopError {
    /// Local DPoP key or proof encoding failed. Retry posture: `not_retryable`.
    #[error("{reason}")]
    Encoding {
        /// Detailed encoding failure reason.
        reason: String,
    },
}

/// Inputs used to mint a DPoP-bound access token.
pub struct AccessTokenGrant<'a> {
    /// Issuer signing key.
    pub issuer_key: &'a EncodingKey,
    /// JWT signing algorithm selected for the issuer key.
    pub issuer_algorithm: Algorithm,
    /// JWT key identifier advertised by the issuer.
    pub issuer_kid: &'a str,
    /// zspend audience claim.
    pub audience: &'a str,
    /// DPoP JWK thumbprint bound into the confirmation claim.
    pub dpop_jkt: &'a str,
    /// Payment authorization included in `authorization_details`.
    pub authorization: &'a PaymentAuthorization,
    /// Maximum token validity in seconds.
    pub token_ttl_seconds: u64,
    /// Prefix for the minted token's single-use JWT identifier.
    pub jti_prefix: &'a str,
}

/// Mint a DPoP-bound access token. Failures are not retryable without changing
/// the issuer key or token claims.
pub fn mint_access_token(grant: &AccessTokenGrant<'_>) -> Result<String, AccessTokenError> {
    let mut header = Header::new(grant.issuer_algorithm);
    header.kid = Some(grant.issuer_kid.to_owned());
    let claims = serde_json::json!({
        "aud": grant.audience,
        "jti": format!("{}-{}", grant.jti_prefix, unix_now_ms()),
        "exp": unix_now_seconds().saturating_add(grant.token_ttl_seconds),
        "cnf": { "jkt": grant.dpop_jkt },
        "authorization_details": [grant.authorization],
    });
    encode(&header, &claims, grant.issuer_key).map_err(|err| AccessTokenError::Encoding {
        reason: err.to_string(),
    })
}

/// Failure while minting an access token.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AccessTokenError {
    /// Local token encoding failed. Retry posture: `not_retryable`.
    #[error("{reason}")]
    Encoding {
        /// Detailed encoding failure reason.
        reason: String,
    },
}

/// Inputs for the zspend signing request.
pub struct ZspendSignCall<'a> {
    /// zspend URL used for the HTTP request.
    pub call_sign_url: &'a str,
    /// DPoP-bound bearer token.
    pub access_token: &'a str,
    /// DPoP proof for the public zspend request URL.
    pub dpop_proof: &'a str,
    /// ZIP-321 payment URI issued by zpay.
    pub payment_uri: &'a str,
    /// Network label accepted by the zspend wire surface.
    pub network_label: &'a str,
    /// zpay payment identifier.
    pub payment_id: &'a str,
    /// Prepared payment expiry height accepted by zspend.
    pub target_expiry_height: u32,
}

/// Signed PCZT returned by zspend.
#[derive(Debug)]
pub struct AgentSignedPczt {
    /// Hex-encoded ZIP-244 transaction identifier.
    pub tx_id: String,
    /// URL-safe base64 PCZT bytes.
    pub pczt_base64: String,
    /// Decoded PCZT byte count.
    pub pczt_byte_count: usize,
}

/// Request a signed PCZT from zspend. Transport failures are retryable;
/// response-shape and signed-PCZT failures are not retryable without changing
/// the remote response.
pub async fn request_zspend_signature(
    http_client: &reqwest::Client,
    call: ZspendSignCall<'_>,
) -> Result<AgentSignedPczt, ZspendSignError> {
    let request = SignPaymentRequestBody {
        payment_request: WirePaymentRequestBody {
            scheme: "zip321".to_owned(),
            request_uri: call.payment_uri.to_owned(),
        },
        network: call.network_label.to_owned(),
        payment_id: call.payment_id.to_owned(),
        target_expiry_height: call.target_expiry_height,
    };
    let response = http_client
        .post(call.call_sign_url)
        .header("authorization", format!("DPoP {}", call.access_token))
        .header("dpop", call.dpop_proof)
        .json(&request)
        .send()
        .await
        .map_err(|err| ZspendSignError::Request {
            reason: err.to_string(),
        })?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(ZspendSignError::Rejected { status, body });
    }
    let signed: SignResponseBody =
        response
            .json()
            .await
            .map_err(|err| ZspendSignError::ResponseMalformed {
                reason: err.to_string(),
            })?;
    if signed.signed.format != PCZT_FORMAT {
        return Err(ZspendSignError::SignedFormat {
            format: signed.signed.format,
        });
    }
    let pczt_byte_count = URL_SAFE_NO_PAD
        .decode(signed.signed.bytes.as_bytes())
        .map_err(|err| ZspendSignError::SignedBytes {
            reason: err.to_string(),
        })?
        .len();
    Ok(AgentSignedPczt {
        tx_id: signed.signed.tx_id,
        pczt_base64: signed.signed.bytes,
        pczt_byte_count,
    })
}

/// Failure while requesting or decoding a signed PCZT from zspend.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ZspendSignError {
    /// The zspend HTTP request failed. Retry posture: `retryable`.
    #[error("{reason}")]
    Request {
        /// Detailed transport failure reason.
        reason: String,
    },
    /// zspend rejected the signing request. Retry posture: `not_retryable`.
    #[error("zspend /v1/payments/sign returned {status}: {body}")]
    Rejected {
        /// HTTP status returned by zspend.
        status: reqwest::StatusCode,
        /// Response body returned by zspend.
        body: String,
    },
    /// zspend returned a successful response with an invalid body. Retry
    /// posture: `not_retryable`.
    #[error("{reason}")]
    ResponseMalformed {
        /// Detailed response decoding failure reason.
        reason: String,
    },
    /// zspend returned a signed payload with an unsupported format. Retry
    /// posture: `not_retryable`.
    #[error("expected pczt-v2-extractable, got {format}")]
    SignedFormat {
        /// Signed payload format returned by zspend.
        format: String,
    },
    /// zspend returned malformed signed PCZT bytes. Retry posture:
    /// `not_retryable`.
    #[error("{reason}")]
    SignedBytes {
        /// Detailed base64 decoding failure reason.
        reason: String,
    },
}

/// Inputs used to build an x402 Zcash exact request for a signed PCZT.
#[derive(Clone, Copy)]
pub struct X402PcztPayment<'a> {
    /// Network carried by the prepared payment.
    pub network: Network,
    /// ZIP-316 recipient selected from the prepared ZIP-321 payment URI.
    pub recipient: &'a str,
    /// Payment amount in zatoshis.
    pub amount_zat: u64,
    /// Maximum payment validity in seconds.
    pub payment_timeout_seconds: u64,
    /// zpay payment identifier bound into the x402 requirements.
    pub payment_id: &'a str,
    /// Resource metadata protected by the payment.
    pub resource: &'a ResourceInfo,
    /// URL-safe base64 signed PCZT bytes.
    pub pczt_base64: &'a str,
}

/// Build the official x402 Zcash exact request for a signed PCZT.
#[must_use]
pub fn build_x402_pczt_facilitator_request(payment: X402PcztPayment<'_>) -> FacilitatorRequest {
    let requirements = PaymentRequirements {
        scheme: "exact".to_owned(),
        network: x402_network_id(payment.network).to_owned(),
        amount: payment.amount_zat.to_string(),
        asset: "ZEC".to_owned(),
        pay_to: payment.recipient.to_owned(),
        max_timeout_seconds: payment.payment_timeout_seconds,
        extra: BTreeMap::from([
            (
                "binding".to_owned(),
                serde_json::Value::String(X402_ZCASH_EXACT_BINDING.to_owned()),
            ),
            (
                "amountUnit".to_owned(),
                serde_json::Value::String("zat".to_owned()),
            ),
            (
                "authorizationFormat".to_owned(),
                serde_json::Value::String(PCZT_FORMAT.to_owned()),
            ),
            (
                "zpayPaymentId".to_owned(),
                serde_json::Value::String(payment.payment_id.to_owned()),
            ),
        ]),
    };
    FacilitatorRequest {
        x402_version: X402_VERSION,
        payment_payload: PaymentPayload {
            x402_version: X402_VERSION,
            resource: payment.resource.clone(),
            accepted: requirements.clone(),
            payload: serde_json::json!({
                "format": PCZT_FORMAT,
                "pczt": payment.pczt_base64,
            }),
            extensions: BTreeMap::new(),
        },
        payment_requirements: requirements,
    }
}

#[derive(Debug, Serialize)]
struct SignPaymentRequestBody {
    payment_request: WirePaymentRequestBody,
    network: String,
    payment_id: String,
    target_expiry_height: u32,
}

#[derive(Debug, Serialize)]
struct WirePaymentRequestBody {
    scheme: String,
    #[serde(rename = "value")]
    request_uri: String,
}

#[derive(Debug, Deserialize)]
struct SignResponseBody {
    #[serde(rename = "signed_payload")]
    signed: SignedSpendWire,
}

#[derive(Debug, Deserialize)]
struct SignedSpendWire {
    format: String,
    bytes: String,
    tx_id: String,
}

const fn x402_network_id(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "zcash:mainnet",
        Network::Regtest(_) => "zcash:regtest",
        Network::Testnet | _ => "zcash:testnet",
    }
}

fn unix_now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn unix_now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

#[cfg(test)]
mod tests {
    use super::{
        DpopKey, ResourceInfo, SignPaymentRequestBody, WirePaymentRequestBody, X402PcztPayment,
        build_x402_pczt_facilitator_request,
    };
    use zally_core::Network;

    #[test]
    fn builds_the_official_x402_zcash_exact_wire_shape() {
        let resource = ResourceInfo {
            url: "https://zpay.local/payments/01JTEST".to_owned(),
            description: Some("test payment".to_owned()),
            mime_type: Some("application/json".to_owned()),
            service_name: Some("zpay-testkit".to_owned()),
            tags: vec!["test".to_owned(), "zcash".to_owned()],
            icon_url: None,
        };
        let request = build_x402_pczt_facilitator_request(X402PcztPayment {
            network: Network::Testnet,
            recipient: "utest1recipient",
            amount_zat: 50_000,
            payment_timeout_seconds: 120,
            payment_id: "01JTEST",
            resource: &resource,
            pczt_base64: "cGN6dA",
        });

        assert_eq!(request.x402_version, 2);
        assert_eq!(request.payment_requirements.network, "zcash:testnet");
        assert_eq!(request.payment_requirements.amount, "50000");
        assert_eq!(
            request.payment_requirements.extra.get("zpayPaymentId"),
            Some(&serde_json::Value::String("01JTEST".to_owned()))
        );
        assert_eq!(
            request.payment_payload.payload,
            serde_json::json!({ "format": "pczt-v2-extractable", "pczt": "cGN6dA" })
        );
    }

    #[test]
    fn encodes_the_zspend_sign_wire_shape() -> Result<(), serde_json::Error> {
        let request = SignPaymentRequestBody {
            payment_request: WirePaymentRequestBody {
                scheme: "zip321".to_owned(),
                request_uri: "zcash:utest1recipient?amount=0.0005".to_owned(),
            },
            network: "testnet".to_owned(),
            payment_id: "01JTEST".to_owned(),
            target_expiry_height: 4_152_900,
        };

        assert_eq!(
            serde_json::to_value(request)?,
            serde_json::json!({
                "payment_request": {
                    "scheme": "zip321",
                    "value": "zcash:utest1recipient?amount=0.0005",
                },
                "network": "testnet",
                "payment_id": "01JTEST",
                "target_expiry_height": 4_152_900,
            })
        );
        Ok(())
    }

    #[test]
    fn mints_a_dpop_proof() -> Result<(), super::DpopError> {
        let dpop_key = DpopKey::generate()?;
        let proof = dpop_key.mint_proof(
            "POST",
            "https://zspend.local/v1/payments/sign",
            "zpay-testkit-dpop",
        )?;

        assert_eq!(proof.split('.').count(), 3);
        assert!(!dpop_key.jkt().is_empty());
        Ok(())
    }
}
