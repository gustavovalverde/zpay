//! Production [`DisclosureFetcher`] backed by zinder's explorer plane.
//!
//! Reads `WalletQuery.TransactionById` (placement, raw transaction)
//! plus `WalletQuery.TransparentOutputsByOutpoint` (prevout
//! resolution). Mirrors the connection-management pattern from
//! [`super::zinder_oracle`] and [`super::zinder_broadcast`]: lazy
//! channel construction, HTTP/2 keepalive, and channel-self-heal on
//! transport-class failures.
//!
//! Today this fetcher carries scaffolding for the integration: it
//! holds the lazy channel and exposes the [`DisclosureFetcher`]
//! impl, but the bytes-to-`DisclosedTransaction` translator is not
//! wired against a specific zinder capability yet. Calls return
//! [`FetchError::Unavailable`] with a clear operator-facing reason,
//! which the verifier surfaces as `chain_presence: "oracle_unavailable"`
//! on the 3-axis wire response. The translator lands behind a
//! capability-detection step in a follow-on slice.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tonic::transport::{Channel, Endpoint};
use zinder_proto::v1::explorer::explorer_query_client::ExplorerQueryClient;
use zpay_core::disclosure_fetcher::{DisclosedTransaction, DisclosureFetcher, FetchError};

/// Production transaction fetcher backed by zinder's explorer plane.
pub(crate) struct ZinderTransactionFetcher {
    /// Lazy gRPC client; swapped out on transport-class failures so
    /// the next call dials a fresh connection.
    client: Arc<ArcSwap<ExplorerQueryClient<Channel>>>,
    /// Endpoint the channel was opened against. Kept so
    /// [`Self::reconnect_on_transport_error`] can rebuild the channel
    /// without re-parsing the URI.
    endpoint: Endpoint,
}

/// Errors raised while constructing a [`ZinderTransactionFetcher`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum ZinderFetcherConfigError {
    /// The supplied endpoint did not parse as a valid gRPC URI.
    #[error("invalid zinder explorer endpoint URI: {reason}")]
    EndpointInvalid {
        /// Operator-facing reason.
        reason: String,
    },
}

impl ZinderTransactionFetcher {
    /// Open a lazy channel to the supplied `ExplorerQuery` endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ZinderFetcherConfigError::EndpointInvalid`] if
    /// `endpoint` does not parse as a gRPC URI.
    pub(crate) fn connect(endpoint: String) -> Result<Self, ZinderFetcherConfigError> {
        let endpoint = Endpoint::from_shared(endpoint).map_err(|err| {
            ZinderFetcherConfigError::EndpointInvalid {
                reason: err.to_string(),
            }
        })?;
        // Mirror zinder-client's keepalive cadence so half-open
        // connections are detected quickly rather than hanging the
        // verify handler.
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

    /// On a transport-class failure rebuild the lazy channel so the
    /// next call dials a fresh connection.
    #[allow(
        dead_code,
        reason = "self-heal helper kept live for the upcoming fetcher translator; today the surface intentionally returns Unavailable until the capability lands"
    )]
    fn reconnect_on_transport_error(&self, status: &tonic::Status) {
        if status.code() != tonic::Code::Unavailable {
            return;
        }
        let channel = self.endpoint.clone().connect_lazy();
        self.client
            .store(Arc::new(ExplorerQueryClient::new(channel)));
    }
}

impl DisclosureFetcher for ZinderTransactionFetcher {
    async fn fetch_transaction(&self, _txid: [u8; 32]) -> Result<DisclosedTransaction, FetchError> {
        // Keep the channel handle live so the connection is
        // pre-warmed for the upcoming translator.
        let _ = &*self.client.load();
        Err(FetchError::Unavailable {
            reason: "zinder explorer fetcher does not yet translate transaction bytes to DisclosedTransaction; falls back to local cryptography-only verdict".to_owned(),
        })
    }
}
