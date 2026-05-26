//! Merchant `accepts[]` advertisement.
//!
//! Operators register merchants in their zpay deployment's config. Each
//! merchant entry carries a list of [`AcceptsEntry`] templates describing
//! the schemes, networks, recipient addresses, and amounts the merchant
//! is willing to receive on. `GET /x402/v2/accepts?merchant_id=…` returns
//! that list verbatim.
//!
//! The registry is in-memory and read-only after construction. A future
//! change can swap in a hot-reloading loader without touching the public
//! surface.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::types::{MerchantId, PaymentNetwork, PaymentScheme, Zatoshis};

/// One `accepts[]` template entry.
///
/// A merchant typically registers more than one entry: one per
/// `(scheme, network)` pair it supports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptsEntry {
    /// Wire-protocol scheme advertised in `accepts[].scheme`.
    pub scheme: PaymentScheme,
    /// Network the payment will settle on.
    pub network: PaymentNetwork,
    /// Unified address the agent's wallet should pay.
    pub pay_to: String,
    /// Amount the merchant expects in zatoshis.
    pub amount_zat: Zatoshis,
    /// Lifetime of a preparation against this entry, in seconds.
    pub max_validity_seconds: u64,
}

/// In-memory registry of merchants and their `accepts[]` templates.
#[derive(Debug, Default, Clone)]
pub struct MerchantRegistry {
    by_id: HashMap<MerchantId, Vec<AcceptsEntry>>,
}

impl MerchantRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `entries` for `merchant_id`. Replaces any prior entries.
    pub fn register(&mut self, merchant_id: MerchantId, entries: Vec<AcceptsEntry>) {
        self.by_id.insert(merchant_id, entries);
    }

    /// Number of registered merchants.
    #[must_use]
    pub fn merchant_count(&self) -> usize {
        self.by_id.len()
    }

    /// Look up the accepts list for `merchant_id`, or `None` if the
    /// merchant is not registered.
    #[must_use]
    pub fn find(&self, merchant_id: &MerchantId) -> Option<&[AcceptsEntry]> {
        self.by_id.get(merchant_id).map(Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::{AcceptsEntry, MerchantRegistry};
    use crate::types::{MerchantId, PaymentNetwork, PaymentScheme, Zatoshis};

    fn sample_entry() -> AcceptsEntry {
        AcceptsEntry {
            scheme: PaymentScheme::Zcash,
            network: PaymentNetwork::Testnet,
            pay_to: "utest1exampleaddress".to_owned(),
            amount_zat: Zatoshis(50_000),
            max_validity_seconds: 120,
        }
    }

    #[test]
    fn empty_registry_finds_nothing() {
        let registry = MerchantRegistry::new();
        assert_eq!(registry.merchant_count(), 0);
        assert!(registry.find(&MerchantId("aether-ai".to_owned())).is_none());
    }

    #[test]
    fn registered_merchant_returns_entries_in_order() -> Result<(), &'static str> {
        let mut registry = MerchantRegistry::new();
        let mainnet = AcceptsEntry {
            network: PaymentNetwork::Mainnet,
            pay_to: "u1mainnetaddress".to_owned(),
            ..sample_entry()
        };
        let testnet = sample_entry();
        let merchant_id = MerchantId("aether-ai".to_owned());
        registry.register(merchant_id.clone(), vec![testnet, mainnet]);

        let entries = registry
            .find(&merchant_id)
            .ok_or("registry returned None for a just-registered merchant")?;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].network, PaymentNetwork::Testnet);
        assert_eq!(entries[1].network, PaymentNetwork::Mainnet);
        Ok(())
    }

    #[test]
    fn re_registering_a_merchant_replaces_prior_entries() -> Result<(), &'static str> {
        let mut registry = MerchantRegistry::new();
        let merchant_id = MerchantId("retail".to_owned());
        registry.register(merchant_id.clone(), vec![sample_entry()]);
        registry.register(merchant_id.clone(), vec![]);
        let entries = registry
            .find(&merchant_id)
            .ok_or("merchant should still be registered after re-register with empty list")?;
        assert!(entries.is_empty());
        Ok(())
    }
}
