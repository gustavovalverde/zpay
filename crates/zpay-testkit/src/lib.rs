//! Test fixtures, live-test gates, and mocks for zpay.
//!
//! Production binaries do not link this crate. The gates here exist as a
//! defensive layer in case a T3 test accidentally lands in a non-test
//! binary.
//!
//! Every public item carries a `pub use zpay_testkit::live::*` style import
//! convention from the tests that consume it.

/// Live-test gates.
pub mod live {
    /// Marker comment for T3 tests' `#[ignore]` attribute.
    pub const IGNORE_REASON: &str = "T3 live test; opt in with ZPAY_TEST_LIVE=1";

    /// Return `true` if `ZPAY_TEST_LIVE=1` is set and the network is
    /// allowed.
    ///
    /// `ZPAY_NETWORK=mainnet` requires `ZPAY_TEST_ALLOW_MAINNET=1` in
    /// addition to `ZPAY_TEST_LIVE=1`. All other networks are allowed
    /// once `ZPAY_TEST_LIVE` is set.
    ///
    /// T3 tests call this and early-return if `false`; the calling test
    /// is then a no-op when the gate is closed.
    #[must_use]
    pub fn is_live_enabled() -> bool {
        if std::env::var("ZPAY_TEST_LIVE").as_deref() != Ok("1") {
            return false;
        }
        let network = std::env::var("ZPAY_NETWORK").unwrap_or_default();
        if network == "mainnet" && std::env::var("ZPAY_TEST_ALLOW_MAINNET").as_deref() != Ok("1") {
            return false;
        }
        true
    }
}
