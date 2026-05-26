//! Production [`DisclosureVerifier`] backed by zinder's
//! `ExplorerQuery.VerifyPaymentDisclosure` RPC.
//!
//! The verifier talks to the explorer-plane gRPC service rather than the
//! wallet-plane one used for broadcast and confirmations. Operators wire
//! it through `ZPAY_NODE__EXPLORER_GRPC_ADDR`; when that env var is
//! unset the `/verify` endpoint surfaces 503 `capability_unavailable`
//! until a chain plane is configured.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tonic::transport::{Channel, Endpoint};
use zinder_proto::v1::explorer::{
    PaymentDisclosureVerdict, VerifyPaymentDisclosureRequest,
    explorer_query_client::ExplorerQueryClient,
};
use zpay_core::verify::{DisclosureVerdict, DisclosureVerifier, Verdict, VerifyError};

/// Production verifier backed by zinder's explorer plane.
pub(crate) struct ZinderDisclosureVerifier {
    client: Arc<ArcSwap<ExplorerQueryClient<Channel>>>,
    endpoint: Endpoint,
}

/// Errors raised while constructing a [`ZinderDisclosureVerifier`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum ZinderVerifierConfigError {
    /// The supplied endpoint did not parse as a valid gRPC URI.
    #[error("invalid zinder explorer endpoint URI: {reason}")]
    EndpointInvalid {
        /// Operator-facing reason.
        reason: String,
    },
}

impl ZinderDisclosureVerifier {
    /// Open a lazy channel to the supplied `ExplorerQuery` endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ZinderVerifierConfigError::EndpointInvalid`] if `endpoint`
    /// does not parse as a gRPC URI.
    pub(crate) fn connect(endpoint: String) -> Result<Self, ZinderVerifierConfigError> {
        let endpoint = Endpoint::from_shared(endpoint).map_err(|err| {
            ZinderVerifierConfigError::EndpointInvalid {
                reason: err.to_string(),
            }
        })?;
        // Mirror zinder-client's keepalive cadence so half-open connections
        // are detected quickly rather than hanging the verify handler.
        let endpoint = endpoint
            .http2_keep_alive_interval(Duration::from_secs(20))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true);
        let channel = endpoint.connect_lazy();
        let client = ExplorerQueryClient::new(channel);
        Ok(Self {
            client: Arc::new(ArcSwap::from_pointee(client)),
            endpoint,
        })
    }
}

impl DisclosureVerifier for ZinderDisclosureVerifier {
    async fn verify_disclosure(
        &self,
        disclosure_bytes: &[u8],
    ) -> Result<DisclosureVerdict, VerifyError> {
        let mut client = (**self.client.load()).clone();
        let request = tonic::Request::new(VerifyPaymentDisclosureRequest {
            payment_disclosure_bytes: disclosure_bytes.to_vec(),
        });
        let response_result = client.verify_payment_disclosure(request).await;
        match response_result {
            Ok(response) => Ok(map_response(response.into_inner())),
            Err(status) if status.code() == tonic::Code::Unimplemented => {
                // Chain plane is reachable but the verify_v1 capability is
                // off. Surface as a typed verdict rather than a transport
                // error so the caller knows to fall back to local
                // verification.
                Ok(DisclosureVerdict {
                    verdict: Verdict::CapabilityUnavailable,
                    transaction_id: None,
                    payment_id: None,
                    disclosed_value_zat: None,
                })
            }
            Err(status) => {
                self.reconnect_on_transport_error(&status);
                Err(VerifyError::Unavailable {
                    reason: status.message().to_owned(),
                })
            }
        }
    }
}

impl ZinderDisclosureVerifier {
    /// On a transport-class failure rebuild the lazy channel so the next
    /// call dials a fresh connection. Mirrors the channel-self-heal
    /// pattern used by `zinder-client::RemoteChainIndex`.
    fn reconnect_on_transport_error(&self, status: &tonic::Status) {
        if status.code() != tonic::Code::Unavailable {
            return;
        }
        let channel = self.endpoint.clone().connect_lazy();
        self.client
            .store(Arc::new(ExplorerQueryClient::new(channel)));
    }
}

fn map_response(
    response: zinder_proto::v1::explorer::VerifyPaymentDisclosureResponse,
) -> DisclosureVerdict {
    let public_facts = response.public_facts;
    let (transaction_id, payment_id, disclosed_value_zat) = public_facts.map_or(
        (None, None, None),
        |facts| {
            (
                Some(hex::encode(facts.transaction_id)),
                Some(hex::encode(facts.payment_id)),
                Some(facts.disclosed_value_zat),
            )
        },
    );
    let verdict = match PaymentDisclosureVerdict::try_from(response.verdict)
        .unwrap_or(PaymentDisclosureVerdict::Unspecified)
    {
        PaymentDisclosureVerdict::Valid => Verdict::Valid,
        PaymentDisclosureVerdict::InvalidSignature => Verdict::InvalidSignature,
        PaymentDisclosureVerdict::TransactionNotFound => Verdict::TransactionNotFound,
        PaymentDisclosureVerdict::Malformed => Verdict::Malformed,
        PaymentDisclosureVerdict::Unspecified => Verdict::CapabilityUnavailable,
    };
    DisclosureVerdict {
        verdict,
        transaction_id,
        payment_id,
        disclosed_value_zat,
    }
}
