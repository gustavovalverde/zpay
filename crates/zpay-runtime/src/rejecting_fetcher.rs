//! Placeholder [`DisclosureFetcher`] for deployments without an
//! explorer plane.
//!
//! Every call returns [`FetchError::Unavailable`], which the local
//! verifier surfaces as
//! `cryptographic_verdict: "inconclusive" / inconclusive_reason: "prevout_unresolved"`
//! with `chain_presence: "oracle_unavailable"` on the wire. Pinned to
//! the same trait as [`super::zinder_fetcher::ZinderTransactionFetcher`]
//! so the runtime can swap between them by capability check at startup
//! without changing [`zpay_x402::AppState`].
//!
//! See [ADR-0007](https://github.com/gustavovalverde/zpay/blob/main/docs/adrs/0007-local-zip311-verifier.md).

use zpay_core::disclosure_fetcher::{DisclosedTransaction, DisclosureFetcher, FetchError};

/// Placeholder fetcher used when no explorer endpoint is configured.
///
/// Carries no state; selecting this variant at startup means every
/// `/x402/v2/verify` call returns
/// `chain_presence: "oracle_unavailable"` until an explorer plane is
/// wired.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct RejectingTransactionFetcher;

impl RejectingTransactionFetcher {
    /// Operator-facing reason carried on every [`FetchError::Unavailable`].
    pub(crate) const REASON: &'static str = "transaction fetcher not configured; set ZPAY_EXPLORER_URL to surface mined chain_presence on /verify";

    /// Construct the fetcher. Stateless; `new` is here for symmetry
    /// with the other variants.
    pub(crate) const fn new() -> Self {
        Self
    }
}

impl DisclosureFetcher for RejectingTransactionFetcher {
    async fn fetch_transaction(&self, _txid: [u8; 32]) -> Result<DisclosedTransaction, FetchError> {
        Err(FetchError::Unavailable {
            reason: Self::REASON.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::RejectingTransactionFetcher;
    use zpay_core::disclosure_fetcher::{DisclosureFetcher, FetchError};

    #[tokio::test]
    async fn fetch_transaction_returns_unavailable() {
        let fetcher = RejectingTransactionFetcher::new();
        let outcome = fetcher.fetch_transaction([0u8; 32]).await;
        assert!(matches!(
            outcome,
            Err(FetchError::Unavailable { ref reason }) if reason == RejectingTransactionFetcher::REASON,
        ));
    }
}
