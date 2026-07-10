//! Pure RFC 7638 and RFC 9449 primitives shared by zpay DPoP verifiers.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use url::Url;

/// Error while canonicalizing an HTTP URL for a DPoP `htu` comparison.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CanonicalUrlError {
    /// The supplied text was not an absolute URL. Retry posture: `not_retryable`.
    #[error("url parse failed: {reason}")]
    Parse {
        /// Parser failure detail.
        reason: String,
    },
    /// The URL scheme was not HTTP or HTTPS. Retry posture: `not_retryable`.
    #[error("url scheme must be http or https, got {scheme}")]
    SchemeUnsupported {
        /// Parsed URL scheme.
        scheme: String,
    },
}

/// Compute the RFC 7638 JWK thumbprint for an EC key.
#[must_use]
pub fn compute_ec_jwk_thumbprint(crv: &str, kty: &str, x: &str, y: &str) -> String {
    let canonical = format!(r#"{{"crv":"{crv}","kty":"{kty}","x":"{x}","y":"{y}"}}"#);
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

/// Canonicalize an HTTP URL for an RFC 9449 `htu` comparison.
///
/// The result lowercases the scheme and host, resolves dot segments, strips
/// default ports, and omits query and fragment values through `url`'s parser.
pub fn canonicalize_http_url(raw: &str) -> Result<String, CanonicalUrlError> {
    let mut parsed = Url::parse(raw).map_err(|err| CanonicalUrlError::Parse {
        reason: err.to_string(),
    })?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(CanonicalUrlError::SchemeUnsupported {
            scheme: scheme.to_owned(),
        });
    }
    parsed.set_query(None);
    parsed.set_fragment(None);
    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::{CanonicalUrlError, canonicalize_http_url, compute_ec_jwk_thumbprint};

    #[test]
    fn thumbprint_matches_rfc7638_vector() {
        let computed = compute_ec_jwk_thumbprint(
            "P-256",
            "EC",
            "MKBCTNIcKUSDii11ySs3526iDZ8AiTo7Tu6KPAqv7D4",
            "4Etl6SRW2YiLUrN5vfvVHuhp7x8PxltmWWlbbM4IFyM",
        );
        assert_eq!(computed, "cn-I_WNMClehiVp51i_0VpOENW1upEerA8sEam5hn-s");
    }

    #[test]
    fn canonicalizes_http_url_for_htu_comparison() -> Result<(), CanonicalUrlError> {
        let canonical = canonicalize_http_url(
            "HTTPS://WALLET.TEST:443/v1/./payments/../payments/sign?cursor=7#section",
        )?;
        assert_eq!(canonical, "https://wallet.test/v1/payments/sign");
        Ok(())
    }

    #[test]
    fn canonicalizes_default_http_and_https_ports() -> Result<(), CanonicalUrlError> {
        assert_eq!(
            canonicalize_http_url("http://zpay.example.test:80/zpay/v1/prepare")?,
            canonicalize_http_url("http://zpay.example.test/zpay/v1/prepare")?,
        );
        assert_eq!(
            canonicalize_http_url("https://zpay.example.test:443/zpay/v1/prepare")?,
            canonicalize_http_url("https://zpay.example.test/zpay/v1/prepare")?,
        );
        Ok(())
    }

    #[test]
    fn keeps_encoded_slash_distinct_from_path_separator() -> Result<(), CanonicalUrlError> {
        let encoded = canonicalize_http_url("https://zpay.example.test/x402/v2%2fprepare")?;
        let literal = canonicalize_http_url("https://zpay.example.test/x402/v2/prepare")?;
        assert_ne!(encoded, literal);
        Ok(())
    }

    #[test]
    fn rejects_non_http_url_scheme() {
        assert!(matches!(
            canonicalize_http_url("mailto:payments@example.test"),
            Err(CanonicalUrlError::SchemeUnsupported { .. })
        ));
    }
}
