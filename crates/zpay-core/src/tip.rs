//! Chain-tip oracle: a pull-mode abstraction over the chain plane that
//! reports the current tip height for a given network.
//!
//! `propose` uses this to derive a prepared transaction's `expiry_height`
//! from `tip + delta`, so the value reflects the network's actual head
//! rather than a value the caller supplies. The runtime composition
//! root plugs a zinder-backed implementation behind this trait; zpay-core
//! stays free of zinder types so unit tests can use an in-memory fake.

use serde::{Deserialize, Serialize};

use crate::types::PaymentNetwork;

/// Errors raised by [`ChainTipOracle`] implementations. These wrap
/// transport-level failures the chain plane could not even respond to.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TipError {
    /// The chain plane was unreachable. Retry posture: `retryable`.
    #[error("chain tip oracle unavailable: {reason}")]
    Unavailable {
        /// Operator-facing reason; the upstream error chains as the source.
        reason: String,
    },
    /// The network is not served by this oracle. Retry posture:
    /// `not_retryable`. Typically a misconfiguration: the runtime is
    /// pointed at a mainnet zinder but a testnet payee asked for prepare.
    #[error("chain tip oracle does not serve network {network:?}")]
    NetworkUnsupported {
        /// The network the caller asked about.
        network: PaymentNetwork,
    },
}

/// Snapshot returned by [`ChainTipOracle::current_tip`] and surfaced
/// verbatim via `GET /zpay/v1/tip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainTip {
    /// The network the tip belongs to.
    pub network: PaymentNetwork,
    /// Height of the chain tip.
    pub tip_height: u32,
}

/// Abstraction over the chain plane that resolves a network to the
/// current chain tip. Implementations are pinned to `Send + Sync` so
/// a single oracle can be shared across the Axum router.
pub trait ChainTipOracle: Send + Sync {
    /// Fetch the current chain tip for `network`.
    fn current_tip(
        &self,
        network: PaymentNetwork,
    ) -> impl Future<Output = Result<u32, TipError>> + Send;
}

use std::future::Future;
