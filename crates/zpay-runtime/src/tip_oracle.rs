//! Chain-tip oracle implementations wired by the runtime.
//!
//! Two concrete strategies:
//!
//! - [`ZinderTipOracle`] reads the visible tip from zinder's current
//!   chain epoch. Production deployments wire this.
//! - [`StaticTipOracle`] returns a configurable fixed height and logs a
//!   WARN on every call. Demo / dev deployments without a chain plane
//!   wire this so `/prepare` still produces a deterministic
//!   `expiry_height`.
//!
//! The composition root selects between them in `build_tip_oracle`.

use zinder_client::{ChainIndex, IndexerError, RemoteChainIndex};
use zpay_core::tip::{ChainTipOracle, TipError};
use zpay_core::types::PaymentNetwork;

/// Production chain-tip oracle backed by zinder's current chain epoch.
pub(crate) struct ZinderTipOracle {
    chain: RemoteChainIndex,
}

impl ZinderTipOracle {
    pub(crate) const fn new(chain: RemoteChainIndex) -> Self {
        Self { chain }
    }
}

impl ChainTipOracle for ZinderTipOracle {
    async fn current_tip(&self, _network: PaymentNetwork) -> Result<u32, TipError> {
        let epoch = self
            .chain
            .current_epoch()
            .await
            .map_err(|err| map_indexer_error(&err))?;
        Ok(epoch.visible_tip_height.value())
    }
}

fn map_indexer_error(err: &IndexerError) -> TipError {
    TipError::Unavailable {
        reason: err.to_string(),
    }
}

/// Static chain-tip oracle.
///
/// Returns a fixed height on every call. Used by demo / dev runs that
/// have no zinder endpoint configured; the WARN log makes it obvious in
/// operator output that the prepared row's `expiry_height` is not
/// tracking real chain state.
pub(crate) struct StaticTipOracle {
    fallback_tip: u32,
}

impl StaticTipOracle {
    pub(crate) const fn new(fallback_tip: u32) -> Self {
        Self { fallback_tip }
    }
}

impl ChainTipOracle for StaticTipOracle {
    async fn current_tip(&self, network: PaymentNetwork) -> Result<u32, TipError> {
        tracing::warn!(
            network = ?network,
            fallback_tip = self.fallback_tip,
            "chain tip oracle is static fallback; configure ZPAY_CHAIN_SOURCE_URL to track real chain state",
        );
        Ok(self.fallback_tip)
    }
}

/// Runtime-time discriminator over the configured chain-tip oracle.
///
/// Using an enum (rather than `Arc<dyn ChainTipOracle>`) keeps the
/// `impl Future + Send` return type from [`ChainTipOracle::current_tip`]
/// statically resolvable without an `async-trait` allocation per call.
pub(crate) enum AnyTipOracle {
    /// Static fallback when no chain plane is configured.
    Static(StaticTipOracle),
    /// Production oracle backed by zinder's `WalletQuery`.
    ///
    /// Boxed because `RemoteChainIndex` carries a tonic `Endpoint` of
    /// several hundred bytes while the `Static` variant is small;
    /// clippy's `large_enum_variant` rule prefers indirection.
    Zinder(Box<ZinderTipOracle>),
}

impl ChainTipOracle for AnyTipOracle {
    async fn current_tip(&self, network: PaymentNetwork) -> Result<u32, TipError> {
        match self {
            Self::Static(inner) => inner.current_tip(network).await,
            Self::Zinder(inner) => inner.current_tip(network).await,
        }
    }
}
