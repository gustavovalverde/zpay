//! Synchronous access-token revocation check (Proposal-0003 D-6).
//!
//! A grant can be revoked at the issuer AFTER its `at+jwt` was minted (CIBA
//! cancellation, operator kill-switch, fraud signal). The single-use `jti`
//! ledger only catches a *replay* of an already-signed token; it cannot catch a
//! first-use of a token whose grant the issuer pulled. This module closes that
//! gap: before the runtime reserves the `jti` in the ledger, it consults a
//! revocation cache so a revoked-then-replayed token returns `token_revoked`
//! rather than the cached signed payload (PRD-43 D-D ordering).
//!
//! The cache is a delta-synced set of revoked `jti`s. It pulls from the issuer
//! endpoint
//! `GET {issuer}/api/auth/oauth2/revoked?since=<unix-ms>&limit=<n>`
//! (Bearer-gated when the issuer sets `INTERNAL_SERVICE_TOKEN`), advancing a
//! `since` cursor and draining `truncated` pages each refresh. The set only
//! grows; the cursor only advances on a fully successful drain.
//!
//! [`RevocationStore::check`] is the sign-time gate. It refreshes lazily when
//! the cache is older than the configured staleness bound, then returns:
//!
//! - [`RevocationOutcome::Revoked`]: the `jti` is in the revoked set.
//! - [`RevocationOutcome::CacheStale`]: the last successful refresh is older
//!   than the staleness bound (the issuer is unreachable or slow and we cannot
//!   prove the token is still live). Fail closed: refuse to sign.
//! - [`RevocationOutcome::Live`]: the `jti` is absent and the cache is fresh.
//!
//! When no issuer URL is configured the store is disabled: `check` always
//! returns `Live` and `refresh` is a no-op. This keeps local dev and the first
//! happy-path E2E runnable without the issuer wired; `serve` logs a startup
//! warning that revocation is not enforced.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Deserialize;

/// Default page size requested from the issuer delta endpoint.
const REVOCATION_PAGE_LIMIT: u32 = 500;

/// Maximum number of truncated delta pages a single refresh will drain.
///
/// Bounds the drain loop so a buggy or hostile issuer that returns
/// `truncated: true` forever cannot spin the sign handler indefinitely. At the
/// page limit above this caps a single refresh at 5M revoked rows, far beyond
/// any plausible delta; hitting it signals a misbehaving issuer, so the refresh
/// errors and the cache ages toward `CacheStale` (fail-closed) rather than
/// looping.
const MAX_DELTA_PAGES: u32 = 10_000;

/// Outcome of consulting the revocation cache for one access-token `jti`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RevocationOutcome {
    /// The `jti` is absent from the revoked set and the cache is fresh; the
    /// caller proceeds to reserve and sign.
    Live,
    /// The `jti` appears in the revoked set; the grant was pulled after mint.
    Revoked,
    /// The cache could not be proven fresh within the staleness bound; the
    /// caller fails closed with a retryable 503.
    CacheStale,
}

/// Error refreshing the revocation cache from the issuer delta endpoint.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RevocationRefreshError {
    /// The HTTP request did not complete (DNS, connect, TLS, timeout).
    #[error("revocation delta request failed: {0}")]
    Request(String),
    /// The issuer returned a non-success status.
    #[error("revocation delta endpoint returned status {0}")]
    Status(u16),
    /// The response body did not deserialize into the expected delta shape.
    #[error("revocation delta response decode failed: {0}")]
    Decode(String),
    /// The issuer kept signalling `truncated: true` without advancing its
    /// `next_since` cursor (or past the page cap), so the drain cannot make
    /// progress. Treated as a refresh failure so the cache ages toward
    /// `CacheStale` and the wallet fails closed rather than looping.
    #[error("revocation delta drain did not make progress: {0}")]
    NonAdvancing(&'static str),
}

/// One revoked-token row in the issuer delta response.
#[derive(Debug, Deserialize)]
struct RevocationRow {
    jti: String,
}

/// The issuer delta-endpoint response page.
#[derive(Debug, Deserialize)]
struct RevocationPage {
    next_since: i64,
    revocations: Vec<RevocationRow>,
    truncated: bool,
}

/// Mutable cache state guarded by a single lock.
struct CacheState {
    /// The set of revoked `jti`s observed so far. Grows monotonically.
    revoked: HashSet<String>,
    /// The `since` cursor (unix ms) to pass on the next delta request. Advances
    /// only after a fully successful drain.
    cursor: i64,
    /// When the cache was last refreshed successfully. `None` until the first
    /// successful refresh, which reads as stale.
    last_refresh: Option<Instant>,
}

/// In-memory revocation cache plus its issuer delta client.
///
/// Cloneable: the cache state is shared behind an [`Arc`] so every request
/// handler observes the same set and cursor.
#[derive(Clone)]
pub(crate) struct RevocationStore {
    inner: Arc<Inner>,
}

/// Shared, non-clonable interior of a [`RevocationStore`].
struct Inner {
    /// Issuer base URL. `None` disables revocation enforcement.
    issuer_url: Option<String>,
    /// Bearer presented to the issuer delta endpoint. `None` sends no header
    /// (local dev where the issuer has no `INTERNAL_SERVICE_TOKEN`).
    bearer: Option<String>,
    /// Maximum age a successful refresh may reach before `check` fails closed
    /// and a lazy refresh is forced.
    max_staleness: Duration,
    client: reqwest::Client,
    state: Mutex<CacheState>,
}

impl RevocationStore {
    /// Construct a store enabled against `issuer_url` with the given Bearer and
    /// staleness bound.
    pub(crate) fn new(
        issuer_url: Option<String>,
        bearer: Option<String>,
        max_staleness: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                issuer_url,
                bearer,
                max_staleness,
                client: reqwest::Client::new(),
                state: Mutex::new(CacheState {
                    revoked: HashSet::new(),
                    cursor: 0,
                    last_refresh: None,
                }),
            }),
        }
    }

    /// Construct a disabled store: `check` always returns
    /// [`RevocationOutcome::Live`] and `refresh` is a no-op. Used when no
    /// issuer URL is configured.
    pub(crate) fn disabled() -> Self {
        Self::new(None, None, Duration::from_secs(0))
    }

    /// Whether revocation enforcement is enabled (an issuer URL is configured).
    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.issuer_url.is_some()
    }

    /// Readiness label for `/readyz`: `disabled` when no issuer is wired,
    /// `fresh` when the last successful refresh is within the staleness bound,
    /// else `stale`.
    pub(crate) fn readiness(&self) -> &'static str {
        if !self.is_enabled() {
            return "disabled";
        }
        if self.is_fresh() { "fresh" } else { "stale" }
    }

    /// Whether the last successful refresh is within the staleness bound.
    fn is_fresh(&self) -> bool {
        self.inner
            .state
            .lock()
            .last_refresh
            .is_some_and(|at| at.elapsed() < self.inner.max_staleness)
    }

    /// Consult the cache for `jti`, refreshing lazily when stale.
    ///
    /// A disabled store short-circuits to [`RevocationOutcome::Live`]. Otherwise
    /// it refreshes when the cache is older than the staleness bound, then
    /// returns `Revoked` if the `jti` is in the set, `CacheStale` if the cache
    /// still cannot be proven fresh (the refresh failed or has never landed),
    /// else `Live`.
    pub(crate) async fn check(&self, jti: &str) -> RevocationOutcome {
        if self.inner.issuer_url.is_none() {
            return RevocationOutcome::Live;
        }

        if !self.is_fresh()
            && let Err(err) = self.refresh().await
        {
            tracing::warn!(error = %err, "revocation cache refresh failed; serving last-known set");
        }

        let (revoked, fresh) = {
            let state = self.inner.state.lock();
            let fresh = state
                .last_refresh
                .is_some_and(|at| at.elapsed() < self.inner.max_staleness);
            (state.revoked.contains(jti), fresh)
        };
        if revoked {
            RevocationOutcome::Revoked
        } else if fresh {
            RevocationOutcome::Live
        } else {
            RevocationOutcome::CacheStale
        }
    }

    /// Construct an enabled store with a pre-seeded revoked set and refresh
    /// timestamp, bypassing the network. Test-only: lets the runtime
    /// integration tests drive `check` through the HTTP handler against a known
    /// cache state. `last_refresh` of `Some(now)` reads fresh; `None` or a past
    /// instant reads stale.
    #[cfg(test)]
    pub(crate) fn seeded_for_test(
        issuer_url: &str,
        revoked: &[&str],
        last_refresh: Option<Instant>,
        max_staleness: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                issuer_url: Some(issuer_url.to_owned()),
                bearer: None,
                max_staleness,
                client: reqwest::Client::new(),
                state: Mutex::new(CacheState {
                    revoked: revoked.iter().map(|jti| (*jti).to_owned()).collect(),
                    cursor: 0,
                    last_refresh,
                }),
            }),
        }
    }

    /// Drain the issuer delta endpoint from the current cursor, union new `jti`s
    /// into the set, advance the cursor, and stamp the refresh time.
    ///
    /// On any HTTP, network, or decode error the call returns the error WITHOUT
    /// advancing the cursor or stamping the refresh time, so the cache keeps its
    /// last-known state and its staleness age keeps growing.
    pub(crate) async fn refresh(&self) -> Result<(), RevocationRefreshError> {
        let Some(issuer_url) = self.inner.issuer_url.as_deref() else {
            return Ok(());
        };

        let mut cursor = self.inner.state.lock().cursor;
        let mut discovered: Vec<String> = Vec::new();
        let mut pages = 0_u32;
        loop {
            let page = self.fetch_page(issuer_url, cursor).await?;
            discovered.extend(page.revocations.into_iter().map(|row| row.jti));
            if !page.truncated {
                cursor = page.next_since;
                break;
            }
            // Truncated: the issuer promises more rows past `next_since`. Refuse
            // to follow a cursor that does not strictly advance, or to drain more
            // than the page cap; either is a misbehaving issuer and looping would
            // hang the sign handler. Erroring here keeps the cache unrefreshed so
            // it ages toward CacheStale (fail-closed).
            if page.next_since <= cursor {
                return Err(RevocationRefreshError::NonAdvancing(
                    "issuer returned truncated:true with a non-advancing next_since cursor",
                ));
            }
            cursor = page.next_since;
            pages += 1;
            if pages >= MAX_DELTA_PAGES {
                return Err(RevocationRefreshError::NonAdvancing(
                    "issuer exceeded the maximum truncated delta page count in one refresh",
                ));
            }
        }

        {
            let mut state = self.inner.state.lock();
            state.revoked.extend(discovered);
            state.cursor = cursor;
            state.last_refresh = Some(Instant::now());
        }
        Ok(())
    }

    /// Fetch one page of the delta endpoint at `since`.
    async fn fetch_page(
        &self,
        issuer_url: &str,
        since: i64,
    ) -> Result<RevocationPage, RevocationRefreshError> {
        let url = format!(
            "{}/api/auth/oauth2/revoked?since={since}&limit={REVOCATION_PAGE_LIMIT}",
            issuer_url.trim_end_matches('/'),
        );
        let mut request = self.inner.client.get(&url);
        if let Some(bearer) = self.inner.bearer.as_deref() {
            request = request.bearer_auth(bearer);
        }
        let response = request
            .send()
            .await
            .map_err(|err| RevocationRefreshError::Request(err.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(RevocationRefreshError::Status(status.as_u16()));
        }
        response
            .json::<RevocationPage>()
            .await
            .map_err(|err| RevocationRefreshError::Decode(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use axum::Router;
    use axum::extract::Query;
    use axum::routing::get;
    use serde::Deserialize;

    use super::{RevocationOutcome, RevocationStore};

    const REVOKED_JTI: &str = "01REVOKEDACCESSTOKENJTI0000";
    const LIVE_JTI: &str = "01LIVEACCESSTOKENJTI0000000";
    const UNREACHABLE_ISSUER: &str = "http://127.0.0.1:1";

    #[tokio::test]
    async fn disabled_store_admits_every_jti() {
        let store = RevocationStore::disabled();
        assert!(!store.is_enabled());
        assert_eq!(store.check(REVOKED_JTI).await, RevocationOutcome::Live);
        assert_eq!(store.readiness(), "disabled");
    }

    #[tokio::test]
    async fn revoked_jti_in_a_fresh_cache_is_revoked() {
        let store = RevocationStore::seeded_for_test(
            UNREACHABLE_ISSUER,
            &[REVOKED_JTI],
            Some(Instant::now()),
            Duration::from_secs(30),
        );
        assert_eq!(store.check(REVOKED_JTI).await, RevocationOutcome::Revoked);
        assert_eq!(store.readiness(), "fresh");
    }

    #[tokio::test]
    async fn live_jti_in_a_fresh_cache_proceeds() {
        let store = RevocationStore::seeded_for_test(
            UNREACHABLE_ISSUER,
            &[REVOKED_JTI],
            Some(Instant::now()),
            Duration::from_secs(30),
        );
        assert_eq!(store.check(LIVE_JTI).await, RevocationOutcome::Live);
    }

    #[tokio::test]
    async fn stale_cache_with_failing_refresh_fails_closed() {
        // Enabled store pointed at an unreachable port with a zero staleness
        // bound, so the prior refresh stamp is immediately past the bound. The
        // lazy refresh tries the dead endpoint, fails without advancing, and the
        // cache stays stale -> CacheStale.
        let store = RevocationStore::seeded_for_test(
            UNREACHABLE_ISSUER,
            &[],
            Some(Instant::now()),
            Duration::ZERO,
        );
        assert_eq!(store.check(LIVE_JTI).await, RevocationOutcome::CacheStale);
        assert_eq!(store.readiness(), "stale");
    }

    #[tokio::test]
    async fn never_refreshed_enabled_store_with_failing_issuer_is_stale() {
        // No prior refresh (last_refresh = None) and an unreachable issuer: the
        // lazy refresh fails and the cache has never landed -> CacheStale.
        let store = RevocationStore::seeded_for_test(
            UNREACHABLE_ISSUER,
            &[],
            None,
            Duration::from_secs(30),
        );
        assert_eq!(store.check(LIVE_JTI).await, RevocationOutcome::CacheStale);
    }

    /// `since` cursor parsed off the stub issuer's delta request.
    #[derive(Deserialize)]
    struct SinceQuery {
        since: i64,
    }

    /// Serve one delta page: `since=0` returns `REVOKED_JTI` and points to
    /// `since=1` with `truncated:true`; any later cursor returns an empty,
    /// non-truncated page so the drain terminates.
    async fn delta_page(Query(query): Query<SinceQuery>) -> axum::Json<serde_json::Value> {
        if query.since == 0 {
            axum::Json(serde_json::json!({
                "since": 0,
                "next_since": 1,
                "revocations": [
                    { "jti": REVOKED_JTI, "revoked_at": "2026-01-01T00:00:00Z", "reason": null }
                ],
                "truncated": true,
            }))
        } else {
            axum::Json(serde_json::json!({
                "since": query.since,
                "next_since": query.since,
                "revocations": [],
                "truncated": false,
            }))
        }
    }

    /// Spawn a stub issuer on an ephemeral port. Returns the base URL plus a
    /// task handle the caller aborts at end of test.
    async fn spawn_stub_issuer()
    -> Result<(String, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
        let app = Router::new().route("/api/auth/oauth2/revoked", get(delta_page));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok((format!("http://{addr}"), handle))
    }

    #[tokio::test]
    async fn refresh_drains_truncated_pages_and_marks_revoked()
    -> Result<(), Box<dyn std::error::Error>> {
        let (issuer_url, handle) = spawn_stub_issuer().await?;
        let store =
            RevocationStore::seeded_for_test(&issuer_url, &[], None, Duration::from_secs(30));

        // The lazy refresh drains both pages, lands the revoked jti, and stamps
        // the cache fresh. The revoked jti is then Revoked; an unrelated jti is
        // Live against the same fresh cache.
        assert_eq!(store.check(REVOKED_JTI).await, RevocationOutcome::Revoked);
        assert_eq!(store.readiness(), "fresh");
        assert_eq!(store.check(LIVE_JTI).await, RevocationOutcome::Live);

        handle.abort();
        Ok(())
    }

    /// Serve a page that is always `truncated: true` with a flat `next_since`
    /// cursor: a buggy/hostile issuer that never lets the drain finish.
    async fn flat_truncated_page(Query(query): Query<SinceQuery>) -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({
            "since": query.since,
            "next_since": query.since,
            "revocations": [],
            "truncated": true,
        }))
    }

    async fn spawn_flat_truncated_issuer()
    -> Result<(String, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
        let app = Router::new().route("/api/auth/oauth2/revoked", get(flat_truncated_page));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok((format!("http://{addr}"), handle))
    }

    #[tokio::test]
    async fn non_advancing_truncated_cursor_fails_refresh_and_stays_stale()
    -> Result<(), Box<dyn std::error::Error>> {
        let (issuer_url, handle) = spawn_flat_truncated_issuer().await?;
        // No prior refresh and a fresh-looking staleness bound: the drain must
        // bail on the non-advancing cursor rather than loop forever, leaving the
        // cache unrefreshed.
        let store =
            RevocationStore::seeded_for_test(&issuer_url, &[], None, Duration::from_secs(30));

        let outcome = store.refresh().await;
        assert!(
            matches!(outcome, Err(super::RevocationRefreshError::NonAdvancing(_))),
            "a non-advancing truncated cursor must fail the refresh; got {outcome:?}",
        );

        // The failed refresh left the cache unstamped, so check() fails closed.
        assert_eq!(store.check(LIVE_JTI).await, RevocationOutcome::CacheStale);
        assert_eq!(store.readiness(), "stale");

        handle.abort();
        Ok(())
    }
}
