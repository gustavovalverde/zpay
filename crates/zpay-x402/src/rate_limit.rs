//! In-memory fixed-window rate limiter for the x402 v2 surface.
//!
//! Two dimensions share one limiter: DPoP-authenticated routes are keyed by
//! the verified `jkt` thumbprint, unauthenticated routes by client IP. Each
//! key gets a fixed 60-second window; the count resets when the window rolls
//! over. A configured limit of `0` disables that dimension.
//!
//! Memory is bounded by an opportunistic sweep: at most once per window the
//! limiter drops every entry whose window has already expired, so the live set
//! is `O(distinct keys seen within one window)`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Fixed window length. One request budget applies per key per window.
const WINDOW: Duration = Duration::from_mins(1);

/// Decision returned by a limiter check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitDecision {
    /// The request is within budget and was counted.
    Allowed,
    /// The request exceeded the budget; retry after this many seconds.
    Limited {
        /// Seconds until the current window rolls over.
        retry_after_seconds: u64,
    },
}

/// Key a request is counted against.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RateLimitKey {
    Jkt(String),
    Ip(IpAddr),
}

/// One fixed window's running count.
struct Window {
    count: u32,
    started_at: Instant,
}

/// Point-in-time view of the limiter's tracked windows.
///
/// For the operator console (see [ADR-0014](https://github.com/gustavovalverde/zpay/blob/main/docs/adrs/0014-operator-payments-console.md)).
/// Read-only: taking a snapshot never changes a budget or a window.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitSnapshot {
    /// Configured per-`jkt`-per-minute budget. `0` means the dimension is disabled.
    pub per_jkt_per_minute: u32,
    /// Configured per-IP-per-minute budget. `0` means the dimension is disabled.
    pub per_ip_per_minute: u32,
    /// Distinct `jkt` keys with a live (unexpired) window right now.
    pub tracked_jkt_count: usize,
    /// Distinct IP keys with a live (unexpired) window right now.
    pub tracked_ip_count: usize,
    /// Requests refused with [`RateLimitDecision::Limited`] since the
    /// process started, across both dimensions.
    pub limited_total_count: u64,
}

/// Shared, process-wide fixed-window limiter.
pub struct RateLimiter {
    per_jkt_per_minute: u32,
    per_ip_per_minute: u32,
    windows: Mutex<HashMap<RateLimitKey, Window>>,
    last_sweep: Mutex<Instant>,
    limited_total: AtomicU64,
}

impl RateLimiter {
    /// Build a limiter with the given per-minute budgets. A budget of `0`
    /// disables that dimension (every check returns [`RateLimitDecision::Allowed`]).
    #[must_use]
    pub fn new(per_jkt_per_minute: u32, per_ip_per_minute: u32) -> Self {
        Self {
            per_jkt_per_minute,
            per_ip_per_minute,
            windows: Mutex::new(HashMap::new()),
            last_sweep: Mutex::new(Instant::now()),
            limited_total: AtomicU64::new(0),
        }
    }

    /// Current snapshot of tracked windows and lifetime limited count, for
    /// the operator console.
    #[must_use]
    pub fn snapshot(&self) -> RateLimitSnapshot {
        let now = Instant::now();
        let windows = self.windows.lock();
        let mut tracked_jkt_count = 0;
        let mut tracked_ip_count = 0;
        for (key, window) in windows.iter() {
            if now.duration_since(window.started_at) >= WINDOW {
                continue;
            }
            match key {
                RateLimitKey::Jkt(_) => tracked_jkt_count += 1,
                RateLimitKey::Ip(_) => tracked_ip_count += 1,
            }
        }
        drop(windows);
        RateLimitSnapshot {
            per_jkt_per_minute: self.per_jkt_per_minute,
            per_ip_per_minute: self.per_ip_per_minute,
            tracked_jkt_count,
            tracked_ip_count,
            limited_total_count: self.limited_total.load(Ordering::Relaxed),
        }
    }

    /// Count a request against the DPoP `jkt` dimension.
    #[must_use]
    pub fn check_jkt(&self, jkt: &str) -> RateLimitDecision {
        self.check(RateLimitKey::Jkt(jkt.to_owned()), self.per_jkt_per_minute)
    }

    /// Count a request against the client-IP dimension.
    #[must_use]
    pub fn check_ip(&self, ip: IpAddr) -> RateLimitDecision {
        self.check(RateLimitKey::Ip(ip), self.per_ip_per_minute)
    }

    fn check(&self, key: RateLimitKey, per_minute: u32) -> RateLimitDecision {
        if per_minute == 0 {
            return RateLimitDecision::Allowed;
        }
        let now = Instant::now();
        self.sweep_if_due(now);

        let mut windows = self.windows.lock();
        let window = windows.entry(key).or_insert_with(|| Window {
            count: 0,
            started_at: now,
        });
        if now.duration_since(window.started_at) >= WINDOW {
            window.count = 0;
            window.started_at = now;
        }
        let decision = if window.count >= per_minute {
            let retry_after_seconds = WINDOW
                .saturating_sub(now.duration_since(window.started_at))
                .as_secs()
                .max(1);
            self.limited_total.fetch_add(1, Ordering::Relaxed);
            RateLimitDecision::Limited {
                retry_after_seconds,
            }
        } else {
            window.count += 1;
            RateLimitDecision::Allowed
        };
        drop(windows);
        decision
    }

    /// Drop expired windows at most once per [`WINDOW`] so the live map stays
    /// bounded by the count of distinct keys seen within a single window.
    fn sweep_if_due(&self, now: Instant) {
        let due = {
            let mut last = self.last_sweep.lock();
            if now.duration_since(*last) < WINDOW {
                false
            } else {
                *last = now;
                true
            }
        };
        if due {
            self.windows
                .lock()
                .retain(|_, window| now.duration_since(window.started_at) < WINDOW);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RateLimitDecision, RateLimitKey, RateLimiter, WINDOW, Window};
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Instant;

    fn ip(last_octet: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, last_octet))
    }

    fn rewind(by: std::time::Duration) -> Result<Instant, &'static str> {
        Instant::now().checked_sub(by).ok_or("instant underflow")
    }

    #[test]
    fn allows_up_to_the_limit_then_429s() {
        let limiter = RateLimiter::new(3, 0);
        for _ in 0..3 {
            assert_eq!(limiter.check_jkt("k"), RateLimitDecision::Allowed);
        }
        assert!(matches!(
            limiter.check_jkt("k"),
            RateLimitDecision::Limited { retry_after_seconds } if retry_after_seconds >= 1
        ));
    }

    #[test]
    fn zero_budget_disables_the_dimension() {
        let limiter = RateLimiter::new(0, 0);
        for _ in 0..1000 {
            assert_eq!(limiter.check_jkt("k"), RateLimitDecision::Allowed);
            assert_eq!(limiter.check_ip(ip(1)), RateLimitDecision::Allowed);
        }
    }

    #[test]
    fn keys_are_isolated() {
        let limiter = RateLimiter::new(1, 1);
        assert_eq!(limiter.check_jkt("a"), RateLimitDecision::Allowed);
        assert!(matches!(
            limiter.check_jkt("a"),
            RateLimitDecision::Limited { .. }
        ));
        // A different jkt has its own budget.
        assert_eq!(limiter.check_jkt("b"), RateLimitDecision::Allowed);
        // The IP dimension is independent of the jkt dimension.
        assert_eq!(limiter.check_ip(ip(1)), RateLimitDecision::Allowed);
        assert!(matches!(
            limiter.check_ip(ip(1)),
            RateLimitDecision::Limited { .. }
        ));
        assert_eq!(limiter.check_ip(ip(2)), RateLimitDecision::Allowed);
    }

    #[test]
    fn window_reset_restores_budget() -> Result<(), &'static str> {
        let limiter = RateLimiter::new(1, 0);
        assert_eq!(limiter.check_jkt("k"), RateLimitDecision::Allowed);
        assert!(matches!(
            limiter.check_jkt("k"),
            RateLimitDecision::Limited { .. }
        ));
        // Force the stored window back by more than one full window so the next
        // check rolls over and grants a fresh budget.
        let rewound = rewind(WINDOW * 2)?;
        {
            let mut windows = limiter.windows.lock();
            if let Some(window) = windows.get_mut(&RateLimitKey::Jkt("k".to_owned())) {
                window.started_at = rewound;
            }
        }
        assert_eq!(limiter.check_jkt("k"), RateLimitDecision::Allowed);
        Ok(())
    }

    #[test]
    fn sweep_evicts_expired_windows() -> Result<(), &'static str> {
        let limiter = RateLimiter::new(5, 0);
        assert_eq!(limiter.check_jkt("stale"), RateLimitDecision::Allowed);
        let stale_start = rewind(WINDOW * 3)?;
        {
            let mut windows = limiter.windows.lock();
            windows.insert(
                RateLimitKey::Jkt("stale".to_owned()),
                Window {
                    count: 1,
                    started_at: stale_start,
                },
            );
        }
        // Force the sweep clock to be due, then a check runs the sweep.
        *limiter.last_sweep.lock() = rewind(WINDOW * 3)?;
        assert_eq!(limiter.check_jkt("fresh"), RateLimitDecision::Allowed);
        assert!(
            !limiter
                .windows
                .lock()
                .contains_key(&RateLimitKey::Jkt("stale".to_owned())),
            "expired window must be evicted by the sweep",
        );
        Ok(())
    }

    #[test]
    fn snapshot_reports_configured_budgets_and_live_tracked_keys() {
        let limiter = RateLimiter::new(1, 2);
        assert_eq!(limiter.check_jkt("a"), RateLimitDecision::Allowed);
        assert_eq!(limiter.check_ip(ip(1)), RateLimitDecision::Allowed);
        assert_eq!(limiter.check_ip(ip(2)), RateLimitDecision::Allowed);

        let snapshot = limiter.snapshot();
        assert_eq!(snapshot.per_jkt_per_minute, 1);
        assert_eq!(snapshot.per_ip_per_minute, 2);
        assert_eq!(snapshot.tracked_jkt_count, 1);
        assert_eq!(snapshot.tracked_ip_count, 2);
        assert_eq!(snapshot.limited_total_count, 0);
    }

    #[test]
    fn snapshot_counts_limited_decisions_cumulatively() {
        let limiter = RateLimiter::new(1, 0);
        assert_eq!(limiter.check_jkt("k"), RateLimitDecision::Allowed);
        assert!(matches!(
            limiter.check_jkt("k"),
            RateLimitDecision::Limited { .. }
        ));
        assert!(matches!(
            limiter.check_jkt("k"),
            RateLimitDecision::Limited { .. }
        ));

        assert_eq!(limiter.snapshot().limited_total_count, 2);
    }
}
