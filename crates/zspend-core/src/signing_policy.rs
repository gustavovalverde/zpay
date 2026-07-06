//! Startup-time invariants the wallet runtime binds at boot.
//!
//! Per Proposal-0003 D-13: the runtime reports its posture; it does not assume
//! it. The [`SigningPolicy`] is the in-memory record of what the binary was
//! configured to allow, and every request-time check ([`Self::network`],
//! [`Self::max_amount_zat`], optional recipient allowlist, audience URI) is
//! answered against this struct rather than re-reading env vars.

use url::Url;
use zally_core::Network;

/// Operator-pinned invariants the wallet enforces on every spend.
#[derive(Clone, Debug)]
pub struct SigningPolicy {
    network: Network,
    max_amount_zat: u64,
    recipient_allowlist: Option<Vec<String>>,
    audience: String,
}

impl SigningPolicy {
    /// Starts a builder for a [`SigningPolicy`].
    #[must_use]
    pub fn builder() -> SigningPolicyBuilder {
        SigningPolicyBuilder::default()
    }

    /// Network the runtime is bound to.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// Hard upper bound the runtime accepts on any single spend, in zatoshis.
    #[must_use]
    pub const fn max_amount_zat(&self) -> u64 {
        self.max_amount_zat
    }

    /// Optional CAIP-10 recipient allowlist. When `None`, every recipient
    /// that passes the canonicalizer is accepted. When `Some`, the spend
    /// fails closed if the canonicalized recipient is not in the list.
    #[must_use]
    pub fn recipient_allowlist(&self) -> Option<&[String]> {
        self.recipient_allowlist.as_deref()
    }

    /// Absolute-URI wallet identity the access-token verifier pins `aud`
    /// against (e.g. `urn:zentity:wallet:<jkt>`). Validated as an absolute URI
    /// at startup; never read from request headers.
    #[must_use]
    pub fn audience(&self) -> &str {
        &self.audience
    }
}

/// Builder for [`SigningPolicy`]. Required fields panic at build-time via the
/// typed [`SigningPolicyError`] rather than at runtime.
#[derive(Clone, Debug, Default)]
pub struct SigningPolicyBuilder {
    network: Option<Network>,
    max_amount_zat: Option<u64>,
    recipient_allowlist: Option<Vec<String>>,
    audience: Option<String>,
}

impl SigningPolicyBuilder {
    /// Sets the network the runtime is bound to.
    #[must_use]
    pub fn network(mut self, network: Network) -> Self {
        self.network = Some(network);
        self
    }

    /// Sets the hard upper bound on any single spend.
    #[must_use]
    pub fn max_amount_zat(mut self, max_amount_zat: u64) -> Self {
        self.max_amount_zat = Some(max_amount_zat);
        self
    }

    /// Sets the optional CAIP-10 recipient allowlist. Passing an empty `Vec`
    /// is treated as "no recipients allowed"; pass `None` (the default) to
    /// accept any well-formed recipient.
    #[must_use]
    pub fn recipient_allowlist(mut self, allowlist: Option<Vec<String>>) -> Self {
        self.recipient_allowlist = allowlist;
        self
    }

    /// Sets the absolute-URI wallet identity the access-token verifier pins
    /// `aud` against (e.g. `urn:zentity:wallet:<jkt>`).
    #[must_use]
    pub fn audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = Some(audience.into());
        self
    }

    /// Consumes the builder and produces a [`SigningPolicy`].
    ///
    /// Returns [`SigningPolicyError::MissingField`] when a required field has
    /// not been provided.
    pub fn build(self) -> Result<SigningPolicy, SigningPolicyError> {
        let network = self
            .network
            .ok_or(SigningPolicyError::MissingField { field: "network" })?;
        let max_amount_zat = self
            .max_amount_zat
            .ok_or(SigningPolicyError::MissingField {
                field: "max_amount_zat",
            })?;
        let audience = self
            .audience
            .ok_or(SigningPolicyError::MissingField { field: "audience" })?;
        // The issuer mints `aud` through an RFC 8707 resource indicator, which
        // is an absolute URI; pin the same shape here so a bare-string misconfig
        // fails fast at boot rather than silently mismatching every token.
        if Url::parse(&audience).is_err() {
            return Err(SigningPolicyError::InvalidAudience { audience });
        }
        Ok(SigningPolicy {
            network,
            max_amount_zat,
            recipient_allowlist: self.recipient_allowlist,
            audience,
        })
    }
}

/// Construction-time errors for [`SigningPolicy`].
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SigningPolicyError {
    /// A required builder field was not set.
    #[error("signing policy is missing required field: {field}")]
    MissingField {
        /// Name of the missing field.
        field: &'static str,
    },
    /// The audience is not an absolute URI (RFC 8707 §2). The issuer reaches
    /// `aud` only through a resource indicator, which must be a URI, so a
    /// bare-string pin could never match a real token.
    #[error("signing policy audience must be an absolute URI (RFC 8707 §2): {audience}")]
    InvalidAudience {
        /// The rejected audience.
        audience: String,
    },
}

#[cfg(test)]
mod tests {
    use super::{SigningPolicy, SigningPolicyError};
    use zally_core::Network;

    #[test]
    fn builds_with_required_fields() -> Result<(), SigningPolicyError> {
        let policy = SigningPolicy::builder()
            .network(Network::Testnet)
            .max_amount_zat(100_000_000)
            .audience("urn:zentity:wallet:fixture")
            .build()?;
        assert_eq!(policy.network(), Network::Testnet);
        assert_eq!(policy.max_amount_zat(), 100_000_000);
        assert_eq!(policy.audience(), "urn:zentity:wallet:fixture");
        assert!(policy.recipient_allowlist().is_none());
        Ok(())
    }

    #[test]
    fn rejects_missing_network() {
        let outcome = SigningPolicy::builder()
            .max_amount_zat(1)
            .audience("urn:zentity:wallet:test")
            .build();
        assert!(matches!(
            outcome,
            Err(SigningPolicyError::MissingField { field }) if field == "network",
        ));
    }

    #[test]
    fn rejects_missing_audience() {
        let outcome = SigningPolicy::builder()
            .network(Network::Mainnet)
            .max_amount_zat(1)
            .build();
        assert!(matches!(
            outcome,
            Err(SigningPolicyError::MissingField { field }) if field == "audience",
        ));
    }

    #[test]
    fn rejects_non_uri_audience() {
        let outcome = SigningPolicy::builder()
            .network(Network::Mainnet)
            .max_amount_zat(1)
            .audience("not-a-uri")
            .build();
        assert!(matches!(
            outcome,
            Err(SigningPolicyError::InvalidAudience { audience }) if audience == "not-a-uri",
        ));
    }

    #[test]
    fn allowlist_passes_through() -> Result<(), SigningPolicyError> {
        let policy = SigningPolicy::builder()
            .network(Network::Mainnet)
            .max_amount_zat(1)
            .audience("urn:zentity:wallet:test")
            .recipient_allowlist(Some(vec!["zcash:main:u1abc".to_owned()]))
            .build()?;
        assert_eq!(policy.recipient_allowlist().map(<[_]>::len), Some(1));
        Ok(())
    }
}
