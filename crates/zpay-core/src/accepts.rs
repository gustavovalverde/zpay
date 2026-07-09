//! Payee `accepts[]` advertisement.
//!
//! Operators register payees in their zpay deployment's config. Each
//! payee entry carries a list of [`AcceptsEntry`] templates describing
//! the schemes, networks, recipient addresses, and amounts the payee
//! is willing to receive on. `GET /zpay/v1/accepts?payee_id=…` returns
//! that list verbatim.
//!
//! Beyond advertisement, the registry is also the authoritative source
//! for prepare resolution. `propose` calls [`PayeeRegistry::resolve`]
//! with a `(payee_id, scheme, network)` tuple to look up the recipient
//! address, the expected amount, the validity window, and the optional
//! expiry-delta override that the prepared row uses.
//!
//! The registry is in-memory and read-only after construction. A future
//! change can swap in a hot-reloading loader without touching the public
//! surface.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::types::{PayeeId, PaymentNetwork, PaymentScheme, Zatoshis};

/// One `accepts[]` template entry.
///
/// A payee typically registers more than one entry: one per
/// `(scheme, network)` pair it supports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptsEntry {
    /// Wire-protocol scheme advertised in `accepts[].scheme`.
    pub scheme: PaymentScheme,
    /// Network the payment will settle on.
    pub network: PaymentNetwork,
    /// Unified address the agent's wallet should pay.
    pub pay_to: String,
    /// Amount the payee expects in zatoshis.
    pub amount_zat: Zatoshis,
    /// Lifetime of a preparation against this entry, in seconds.
    pub max_validity_seconds: u64,
    /// Optional per-entry override for the `tip + delta` expiry math.
    /// When `None`, [`crate::prepare::DEFAULT_EXPIRY_DELTA_BLOCKS`] applies.
    #[serde(default)]
    pub expiry_delta_blocks: Option<u32>,
    /// Whether the merchant expects a verify-oracle pass on prepared
    /// payments before the agent broadcasts. Defaults to `false`; flip
    /// to `true` in `payees.toml` when the merchant participates in
    /// intent verification. Consumers downstream of `/accepts` read this
    /// to drive UI affordances (e.g., a "verify required" badge in the
    /// bridge).
    #[serde(default)]
    pub merchant_requires_verify: bool,
}

/// In-memory registry of payees and their `accepts[]` templates.
#[derive(Debug, Default, Clone)]
pub struct PayeeRegistry {
    by_id: HashMap<PayeeId, Vec<AcceptsEntry>>,
}

impl PayeeRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `entries` for `payee_id`. Replaces any prior entries.
    pub fn register(&mut self, payee_id: PayeeId, entries: Vec<AcceptsEntry>) {
        self.by_id.insert(payee_id, entries);
    }

    /// Number of registered payees.
    #[must_use]
    pub fn payee_count(&self) -> usize {
        self.by_id.len()
    }

    /// Iterate every registered `(payee_id, accepts[])` pair.
    ///
    /// Used by the runtime's startup-time gates (e.g., the production
    /// placeholder-receiver check) that need to inspect every advertised
    /// `pay_to` before binding the listener.
    pub fn iter(&self) -> impl Iterator<Item = (&PayeeId, &[AcceptsEntry])> {
        self.by_id
            .iter()
            .map(|(payee_id, entries)| (payee_id, entries.as_slice()))
    }

    /// Look up the accepts list for `payee_id`, or `None` if the
    /// payee is not registered.
    #[must_use]
    pub fn find(&self, payee_id: &PayeeId) -> Option<&[AcceptsEntry]> {
        self.by_id.get(payee_id).map(Vec::as_slice)
    }

    /// Find the single `(payee_id, scheme, network)` template that
    /// applies to a prepare request. Returns `None` when the payee is
    /// not registered or none of its entries match the scheme + network
    /// pair.
    #[must_use]
    pub fn resolve(
        &self,
        payee_id: &PayeeId,
        scheme: PaymentScheme,
        network: PaymentNetwork,
    ) -> Option<&AcceptsEntry> {
        self.by_id
            .get(payee_id)?
            .iter()
            .find(|entry| entry.scheme == scheme && entry.network == network)
    }
}

#[cfg(test)]
mod tests {
    use super::{AcceptsEntry, PayeeRegistry};
    use crate::types::{PayeeId, PaymentNetwork, PaymentScheme, Zatoshis};

    fn sample_entry() -> AcceptsEntry {
        AcceptsEntry {
            scheme: PaymentScheme::Zcash,
            network: PaymentNetwork::Testnet,
            pay_to: "utest1exampleaddress".to_owned(),
            amount_zat: Zatoshis(50_000),
            max_validity_seconds: 120,
            expiry_delta_blocks: None,
            merchant_requires_verify: false,
        }
    }

    #[test]
    fn merchant_requires_verify_defaults_to_false_in_toml() -> Result<(), &'static str> {
        let toml_src = r#"
            scheme = "zcash"
            network = "testnet"
            pay_to = "utest1example"
            amount_zat = 50000
            max_validity_seconds = 120
        "#;
        let parsed: AcceptsEntry =
            toml::from_str(toml_src).map_err(|_| "TOML must parse without the new field")?;
        assert!(!parsed.merchant_requires_verify);
        Ok(())
    }

    #[test]
    fn merchant_requires_verify_round_trips_from_toml() -> Result<(), &'static str> {
        let toml_src = r#"
            scheme = "zcash"
            network = "testnet"
            pay_to = "utest1example"
            amount_zat = 50000
            max_validity_seconds = 120
            merchant_requires_verify = true
        "#;
        let parsed: AcceptsEntry = toml::from_str(toml_src)
            .map_err(|_| "TOML with explicit merchant_requires_verify must parse")?;
        assert!(parsed.merchant_requires_verify);
        Ok(())
    }

    #[test]
    fn empty_registry_finds_nothing() {
        let registry = PayeeRegistry::new();
        assert_eq!(registry.payee_count(), 0);
        assert!(registry.find(&PayeeId("aether-ai".to_owned())).is_none());
    }

    #[test]
    fn registered_payee_returns_entries_in_order() -> Result<(), &'static str> {
        let mut registry = PayeeRegistry::new();
        let mainnet = AcceptsEntry {
            network: PaymentNetwork::Mainnet,
            pay_to: "u1mainnetaddress".to_owned(),
            ..sample_entry()
        };
        let testnet = sample_entry();
        let payee_id = PayeeId("aether-ai".to_owned());
        registry.register(payee_id.clone(), vec![testnet, mainnet]);

        let entries = registry
            .find(&payee_id)
            .ok_or("registry returned None for a just-registered payee")?;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].network, PaymentNetwork::Testnet);
        assert_eq!(entries[1].network, PaymentNetwork::Mainnet);
        Ok(())
    }

    #[test]
    fn re_registering_a_payee_replaces_prior_entries() -> Result<(), &'static str> {
        let mut registry = PayeeRegistry::new();
        let payee_id = PayeeId("retail".to_owned());
        registry.register(payee_id.clone(), vec![sample_entry()]);
        registry.register(payee_id.clone(), vec![]);
        let entries = registry
            .find(&payee_id)
            .ok_or("payee should still be registered after re-register with empty list")?;
        assert!(entries.is_empty());
        Ok(())
    }

    #[test]
    fn resolve_matches_on_payee_scheme_and_network() -> Result<(), &'static str> {
        let mut registry = PayeeRegistry::new();
        let testnet = sample_entry();
        let mainnet = AcceptsEntry {
            network: PaymentNetwork::Mainnet,
            pay_to: "u1mainnetaddress".to_owned(),
            ..sample_entry()
        };
        let payee_id = PayeeId("aether-ai".to_owned());
        registry.register(payee_id.clone(), vec![testnet, mainnet]);

        let resolved = registry
            .resolve(&payee_id, PaymentScheme::Zcash, PaymentNetwork::Mainnet)
            .ok_or("expected mainnet entry to resolve")?;
        assert_eq!(resolved.network, PaymentNetwork::Mainnet);
        assert_eq!(resolved.pay_to, "u1mainnetaddress");

        assert!(
            registry
                .resolve(&payee_id, PaymentScheme::Zcash, PaymentNetwork::Regtest)
                .is_none()
        );
        assert!(
            registry
                .resolve(
                    &PayeeId("unknown".to_owned()),
                    PaymentScheme::Zcash,
                    PaymentNetwork::Testnet,
                )
                .is_none()
        );
        Ok(())
    }
}
