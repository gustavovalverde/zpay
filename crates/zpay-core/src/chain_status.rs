//! Shared chain-tip view used to make the settlement lifecycle
//! reorg-aware.
//!
//! [`ChainStatusView`] carries the two heights the status projection
//! needs: the visible tip (for expiry-lapse) and the settled tip (the
//! reorg-window scan ceiling, below which a mined payment is immutable).
//! [`ChainStatusCache`] is the process-wide holder the background chain
//! tasks refresh and the wire handlers read without a chain round-trip.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Two-height chain snapshot consumed by [`crate::status::lookup_payment_status`].
///
/// Both heights are `None` until a chain read has populated the cache;
/// the status projection fails open in that state (no expiry-lapse, no
/// row reported settled).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChainStatusView {
    /// Best visible tip height, or `None` before the first chain read.
    pub visible_tip_height: Option<u64>,
    /// Reorg-window scan ceiling: heights at or below this are immutable.
    /// `None` before the first chain read.
    pub settled_tip_height: Option<u64>,
}

impl ChainStatusView {
    /// Returns `true` when `mined_height` sits at or below the settled tip.
    ///
    /// A `None` settled tip yields `false`: an unknown chain view never
    /// reports a payment settled.
    #[must_use]
    pub const fn is_settled_at(&self, mined_height: u64) -> bool {
        match self.settled_tip_height {
            Some(settled) => mined_height <= settled,
            None => false,
        }
    }

    /// Returns `true` when `expiry_height` is strictly below the visible
    /// tip, meaning an unmined payment's settle window has lapsed.
    ///
    /// A `None` visible tip yields `false`: an unknown chain view never
    /// expires a payment.
    #[must_use]
    pub const fn is_lapsed_at(&self, expiry_height: u64) -> bool {
        match self.visible_tip_height {
            Some(tip) => tip > expiry_height,
            None => false,
        }
    }
}

/// Process-wide cache of the latest [`ChainStatusView`].
///
/// The chain-event subscription refreshes it from every envelope and the
/// confirmation-oracle poll refreshes it every tick, so the wire handlers
/// read a current view with a pair of relaxed atomic loads.
#[derive(Debug, Default)]
pub struct ChainStatusCache {
    visible_tip_height: AtomicU64,
    settled_tip_height: AtomicU64,
    known: AtomicBool,
    /// Wall-clock second of the last [`Self::store`], or `0` before the first
    /// refresh. The readiness probe reads this so a dead poll loop surfaces as
    /// a growing cache age even while a live chain probe still succeeds.
    refreshed_at_unix_seconds: AtomicU64,
}

impl ChainStatusCache {
    /// Fresh cache with no chain read applied yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Overwrite the cached view with a fresh chain read.
    pub fn store(&self, visible_tip_height: u64, settled_tip_height: u64) {
        self.visible_tip_height
            .store(visible_tip_height, Ordering::Relaxed);
        self.settled_tip_height
            .store(settled_tip_height, Ordering::Relaxed);
        self.refreshed_at_unix_seconds
            .store(now_unix_seconds(), Ordering::Relaxed);
        self.known.store(true, Ordering::Relaxed);
    }

    /// Wall-clock second of the last [`Self::store`], or `None` before the
    /// first refresh. Used by the readiness probe and the metrics sampler to
    /// derive the cache age.
    #[must_use]
    pub fn last_refresh_unix_seconds(&self) -> Option<u64> {
        if !self.known.load(Ordering::Relaxed) {
            return None;
        }
        Some(self.refreshed_at_unix_seconds.load(Ordering::Relaxed))
    }

    /// Read the cached view. Returns the default (both `None`) until the
    /// first [`Self::store`].
    #[must_use]
    pub fn load(&self) -> ChainStatusView {
        if !self.known.load(Ordering::Relaxed) {
            return ChainStatusView::default();
        }
        ChainStatusView {
            visible_tip_height: Some(self.visible_tip_height.load(Ordering::Relaxed)),
            settled_tip_height: Some(self.settled_tip_height.load(Ordering::Relaxed)),
        }
    }
}

/// Current wall-clock time in unix seconds, saturating to `0` on a
/// pre-epoch clock.
fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::{ChainStatusCache, ChainStatusView};

    #[test]
    fn unknown_view_never_settles_or_lapses() {
        let view = ChainStatusView::default();
        assert!(!view.is_settled_at(0));
        assert!(!view.is_lapsed_at(0));
    }

    #[test]
    fn settled_and_lapsed_thresholds_are_inclusive_and_strict() {
        let view = ChainStatusView {
            visible_tip_height: Some(200),
            settled_tip_height: Some(100),
        };
        assert!(view.is_settled_at(100));
        assert!(view.is_settled_at(99));
        assert!(!view.is_settled_at(101));
        assert!(view.is_lapsed_at(199));
        assert!(!view.is_lapsed_at(200));
    }

    #[test]
    fn last_refresh_is_none_until_first_store() {
        let cache = ChainStatusCache::new();
        assert_eq!(cache.last_refresh_unix_seconds(), None);
        cache.store(10, 5);
        assert!(cache.last_refresh_unix_seconds().is_some());
    }

    #[test]
    fn cache_starts_unknown_then_reflects_last_store() {
        let cache = ChainStatusCache::new();
        assert_eq!(cache.load(), ChainStatusView::default());
        cache.store(500, 400);
        assert_eq!(
            cache.load(),
            ChainStatusView {
                visible_tip_height: Some(500),
                settled_tip_height: Some(400),
            }
        );
    }
}
