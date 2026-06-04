//! Per-payment SSE event bus for `GET /x402/v2/payments/{payment_id}/events`.
//!
//! ## Topology
//!
//! One `tokio::sync::broadcast::Sender<PaymentStatusSnapshot>` per
//! `payment_id`, lazy-created on the first subscribe. `publish` never
//! creates an entry: a zero-subscriber publish is a no-op. This caps the
//! registry at the set of payments that actually had a live subscriber
//! at some point in the process lifetime, which is the smallest leak
//! surface compatible with "the bridge can subscribe before /settle and
//! then drop without leaving zombie state".
//!
//! ## Publishers
//!
//! - `settle_handler` after a successful broadcast outcome. The first
//!   snapshot published is `status: "broadcast"`; the SSE stream stays
//!   open and surfaces `"mined"` once the oracle records the first
//!   confirmation, then closes after `"final"`.
//! - The confirmation oracle after each `record_confirmation` returning
//!   `Ok(true)`. The stream stays open through `"mined"` and only closes
//!   once `confirmation_count >= ZPAY_FINALITY_DEPTH` flips the snapshot
//!   to `"final"`.
//!
//! ## Wire schema
//!
//! Each snapshot is emitted as a named `snapshot` event whose `data:`
//! field is a `PaymentStatusSnapshot` serialized as JSON in `snake_case`.
//! The SSE payload deliberately does NOT wrap the snapshot in
//! `{ "data": ... }`: the REST endpoint at
//! `GET /x402/v2/payments/{payment_id}` uses that envelope because HTTP
//! responses sometimes carry sibling fields, while every SSE event has
//! a single canonical shape per event name. Bridge code reads
//! `event.data` directly as a snapshot.
//!
//! Event names emitted:
//!
//! - `event: snapshot`: snapshot payloads. The bridge wires
//!   `addEventListener("snapshot", ...)`.
//! - `event: lag`: emitted with `data: {"reason":"resync"}` when the
//!   broadcast receiver lags past the channel buffer. The bridge
//!   refetches `GET /payments/{id}` on receipt.
//! - `event: serialization_failed`: emitted when a snapshot fails to
//!   serialize. Body: `{"payment_id":"..."}`. The bridge should
//!   refetch `GET /payments/{id}` (the SSE stream cannot recover this
//!   snapshot, but the canonical state is still in the store).
//! - `event: resync_failed`: emitted when the lag-recovery re-read
//!   itself fails. Body: `{"payment_id":"..."}`. The stream closes
//!   immediately afterward; the bridge should retry the subscription.
//!
//! The stream closes after delivering the first snapshot whose status is
//! terminal (`final`, `failed`, `never_issued`, or `expired`). `awaiting`,
//! `broadcast`, and `mined` are explicitly non-terminal: the stream stays
//! open while confirmations accumulate. The browser `EventSource` will
//! auto-reconnect, but the new connection's initial snapshot will be
//! terminal again and the stream will close immediately, which is the
//! documented contract.
//!
//! ## Cleanup
//!
//! Hub entries are never reaped before process restart. Bound: one
//! `broadcast::Sender` per `payment_id` that ever had a subscriber.
//! Subscriber-gated insert is what keeps this from being an
//! attacker-controlled leak: every subscribe runs through
//! `events_handler`, which rejects ids that are not present in either
//! the prepared-tx store or the settlement ledger.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::stream::Stream;
use parking_lot::Mutex;
use pin_project_lite::pin_project;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use zpay_core::broadcast::BroadcastClient;
use zpay_core::prepare::PreparedTxStore;
use zpay_core::status::{
    PaymentStatus, PaymentStatusSnapshot, SettlementLedgerStore, lookup_payment_status,
};
use zpay_core::store::StoreError;
use zpay_core::transaction_fetcher::TransactionFetcher;
use zpay_core::types::PaymentId;
use zpay_core::verify::PaymentDisclosureVerifier;

use crate::{AppState, problem_response};

/// Resync hook used by [`EventStream`] when the broadcast receiver lags
/// past the channel buffer.
///
/// When the buffer overflows we no longer know which snapshots the
/// receiver missed, so the only safe recovery is to re-read the
/// authoritative state from the backing stores. We hide that read behind
/// a trait object so the events module does not depend directly on
/// concrete store types; the handler wires in a tiny adapter over the
/// configured prepared-tx store and settlement ledger.
pub(crate) trait ResyncSource: Send + Sync {
    /// Re-read the canonical snapshot for `payment_id` from backing
    /// storage. Used after a `Lagged` event to recover the latest known
    /// state.
    fn re_read<'a>(
        &'a self,
        payment_id: &'a PaymentId,
    ) -> Pin<Box<dyn Future<Output = Result<PaymentStatusSnapshot, StoreError>> + Send + 'a>>;
}

/// Adapter that wires a `(PreparedTxStore, SettlementLedgerStore)` pair
/// into the [`ResyncSource`] trait without leaking those generic types
/// into the events module's stream type.
struct StoreResyncSource<P, L> {
    prepared_store: Arc<P>,
    ledger: Arc<L>,
    finality_depth: u32,
}

impl<P, L> ResyncSource for StoreResyncSource<P, L>
where
    P: PreparedTxStore + Send + Sync + 'static,
    L: SettlementLedgerStore + Send + Sync + 'static,
{
    fn re_read<'a>(
        &'a self,
        payment_id: &'a PaymentId,
    ) -> Pin<Box<dyn Future<Output = Result<PaymentStatusSnapshot, StoreError>> + Send + 'a>> {
        Box::pin(async move {
            lookup_payment_status(
                payment_id,
                self.prepared_store.as_ref(),
                self.ledger.as_ref(),
                self.finality_depth,
            )
            .await
        })
    }
}

/// Channel buffer per payment.
///
/// One settle publish plus a small burst of oracle confirmation ticks
/// fits in 16 slots. The lag-then-resync path covers a slow consumer
/// without losing the terminal event.
const CHANNEL_CAPACITY: usize = 16;

/// SSE keepalive cadence.
///
/// 15 seconds defeats most proxy idle timeouts and is short enough that
/// a dropped TCP connection is detected within a typical demo step.
const KEEPALIVE_INTERVAL_SECONDS: u64 = 15;

/// Per-payment broadcast hub.
///
/// Lives behind an `Arc` on `AppState`. Lock holds are pure compute:
/// `HashMap` entry plus `broadcast::Sender::send` (non-blocking,
/// synchronous). `parking_lot::Mutex` is the correct primitive because
/// no `.await` ever crosses the guard and any future Drop-based
/// bookkeeping can run synchronously without the async-mutex Drop
/// hazard.
#[derive(Debug, Default)]
pub struct PaymentEventHub {
    channels: Mutex<HashMap<PaymentId, broadcast::Sender<PaymentStatusSnapshot>>>,
}

impl PaymentEventHub {
    /// Fresh, empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lazy-create a sender and return a fresh receiver.
    ///
    /// Called from `events_handler` AFTER the handler has confirmed the
    /// `payment_id` is well-formed and present in a backing store. The
    /// existence check is enforced at the handler boundary, not here, so
    /// publishers may safely call into the hub without that round-trip.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the parking_lot guard is held for two synchronous calls (entry + subscribe); tightening to a single expression would force a double-lookup and worsen the lock-hold profile this lint is meant to improve."
    )]
    pub fn subscribe(&self, payment_id: &PaymentId) -> broadcast::Receiver<PaymentStatusSnapshot> {
        let mut channels = self.channels.lock();
        let sender = channels
            .entry(payment_id.clone())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0);
        sender.subscribe()
    }

    /// Publish a snapshot iff a sender already exists for this id.
    ///
    /// A `SendError` with zero subscribers is expected (the last
    /// subscriber may have disconnected between the receiver lookup and
    /// the send) and is silently ignored. The non-insert behaviour caps
    /// the hub at the set of payments that ever had a live subscriber:
    /// oracle ticks for payments nobody is watching are a no-op.
    ///
    /// ## Monotonicity is the caller's responsibility
    ///
    /// `publish` does not serialize publishers. The `/settle` handler and
    /// the confirmation oracle can both call into the hub concurrently,
    /// and a `Settled(conf=None)` publish may interleave with a
    /// `Settled(conf=Some(n))` publish in either order. That interleave
    /// is safe in practice because the SSE stream closes inclusively on
    /// the first terminal snapshot it observes: regardless of which
    /// terminal lands first, the subscriber sees exactly one terminal
    /// event and the stream ends. Callers that need stricter ordering
    /// (none today) would have to serialize at the call site, not here.
    pub fn publish(&self, payment_id: &PaymentId, snapshot: PaymentStatusSnapshot) {
        // Clone the sender out so `broadcast::Sender::send` never runs
        // while the registry mutex is held. `broadcast::Sender` is an Arc
        // internally; the clone is cheap.
        let sender = {
            let channels = self.channels.lock();
            channels.get(payment_id).cloned()
        };
        if let Some(sender) = sender {
            // `broadcast::Sender::send` is non-blocking and returns
            // `SendError(0)` when no receivers are live. We discard that
            // because zero subscribers is the documented quiet path.
            let _ = sender.send(snapshot);
        }
    }

    /// Number of registered senders. Test surface only.
    #[cfg(test)]
    pub(crate) fn channel_count(&self) -> usize {
        self.channels.lock().len()
    }
}

/// `GET /x402/v2/payments/{payment_id}/events` handler.
///
/// Flow:
///
/// 1. Validate the `payment_id` shape. Reject 422 on failure.
/// 2. Probe `lookup_payment_status`. Reject 503 on store failure;
///    reject 404 on `Unknown`. The probe runs BEFORE `subscribe` so
///    probing clients cannot inflate the hub registry against
///    payment ids the store has never seen.
/// 3. Subscribe to the hub. Only well-formed, store-known ids reach
///    this step, so the registry stays bounded by real payments.
/// 4. Re-read `lookup_payment_status` to capture the canonical
///    connect-time snapshot. The re-read closes the race window
///    between the probe and the subscribe: a publish that lands in
///    that window is reflected in the store, so the canonical
///    snapshot reports terminal even if the broadcast buffer is
///    empty. A publish that lands AFTER subscribe sits in the
///    broadcast tail and is delivered after the initial event.
/// 5. Prepend the initial snapshot ahead of a `BroadcastStream` of
///    subsequent publishes. The stream closes after the first
///    snapshot whose status is terminal (which may be the initial
///    one).
///
/// The probe-subscribe-reread sequence pairs the registry-bounding
/// existence check with race-window correctness for the publish that
/// lands while the handler is still building its response. The
/// inclusive terminal cutoff in [`EventStream`] absorbs the duplicate
/// when both the canonical snapshot and the buffered tail carry the
/// same terminal status.
///
/// The handler sets `Cache-Control: no-cache, no-transform` and
/// `X-Accel-Buffering: no` so reverse proxies (Railway router,
/// Cloudflare, Next.js streaming) do not buffer the response.
pub(crate) async fn events_handler<C, V, P, L, T, F>(
    State(state): State<AppState<C, V, P, L, T, F>>,
    Path(payment_id_raw): Path<String>,
) -> Response
where
    C: BroadcastClient + 'static,
    V: PaymentDisclosureVerifier + 'static,
    P: PreparedTxStore + Send + Sync + 'static,
    L: SettlementLedgerStore + Send + Sync + 'static,
    T: zpay_core::tip::ChainTipOracle + 'static,
    F: TransactionFetcher + 'static,
{
    let payment_id = match payment_id_raw.parse::<PaymentId>() {
        Ok(id) => id,
        Err(reason) => {
            return problem_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid Argument",
                422,
                &reason.to_string(),
            );
        }
    };

    // Probe the store first so that `NeverIssued` ids are rejected with 404
    // BEFORE `subscribe` creates a hub entry. Without this gate, a probing
    // client could inflate the registry by opening then dropping SSE
    // connections for arbitrary ids. `Expired` rows still pass the gate
    // because the bridge needs to render "this payment window has expired"
    // copy from a real snapshot.
    let Ok(probe) = lookup_payment_status(
        &payment_id,
        state.prepared_store.as_ref(),
        state.ledger.as_ref(),
        state.finality_depth,
    )
    .await
    else {
        return problem_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Service Unavailable",
            503,
            "payment status store is currently unavailable",
        );
    };

    if matches!(probe.status, PaymentStatus::NeverIssued) {
        return problem_response(
            StatusCode::NOT_FOUND,
            "Not Found",
            404,
            "payment_id is not registered with this deployment",
        );
    }

    let rx = state.events.subscribe(&payment_id);

    // Re-read after subscribe to close the race window between the probe
    // and the subscribe call. A publish landing in that window updates
    // the store but reaches a hub that has no subscribers yet, so the
    // canonical post-subscribe read is the only deterministic capture.
    // The publish itself is consumed by the no-op publish path; the
    // initial event below carries the updated terminal status.
    let Ok(initial_snapshot) = lookup_payment_status(
        &payment_id,
        state.prepared_store.as_ref(),
        state.ledger.as_ref(),
        state.finality_depth,
    )
    .await
    else {
        return problem_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Service Unavailable",
            503,
            "payment status store is currently unavailable",
        );
    };

    let resync: Arc<dyn ResyncSource> = Arc::new(StoreResyncSource {
        prepared_store: Arc::clone(&state.prepared_store),
        ledger: Arc::clone(&state.ledger),
        finality_depth: state.finality_depth,
    });

    let body_stream = EventStream::new(payment_id, initial_snapshot, rx, resync);

    let mut response = Sse::new(body_stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECONDS)))
        .into_response();

    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    // Defeat nginx-derived reverse proxy buffering. Cloud edges
    // honour `X-Accel-Buffering: no` even when they ignore other
    // streaming hints.
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// In-flight resync future used by [`EventStream`] when the broadcast
/// receiver lags.
///
/// Boxed because the concrete future returned by
/// [`ResyncSource::re_read`] is opaque; the stream needs a single named
/// type to park between polls.
type ResyncFuture = Pin<Box<dyn Future<Output = Result<PaymentStatusSnapshot, StoreError>> + Send>>;

pin_project! {
    /// SSE body stream: initial snapshot (head), then broadcast tail,
    /// closing inclusively after the first terminal snapshot.
    ///
    /// ## Terminal-inclusive cutoff
    ///
    /// The stream emits exactly one event per terminal snapshot it
    /// observes (initial, broadcast tail, or lag-recovery), then closes
    /// on the next poll. That contract is what makes concurrent
    /// publishers safe: even if `/settle` and the oracle race to publish
    /// terminal snapshots in arbitrary order, the subscriber sees one
    /// terminal event and the stream ends. See
    /// [`PaymentEventHub::publish`] for the monotonicity contract.
    ///
    /// ## Lag recovery
    ///
    /// `BroadcastStream` reports `Lagged(n)` when the receiver fell
    /// `n` slots behind the channel's bounded buffer. Without a
    /// resync we would silently lose state, and if the lagged window
    /// contained the only terminal snapshot the stream would stay open
    /// forever. On `Lagged` the stream:
    ///
    /// 1. Emits a `lag` event so the bridge knows to resync.
    /// 2. Calls [`ResyncSource::re_read`] to recover the latest
    ///    canonical state from the backing stores.
    /// 3. Emits the resync result as a `snapshot` event.
    /// 4. Closes if the recovered snapshot is terminal.
    ///
    /// Hand-rolled rather than composed via `futures::stream` adapters
    /// because the terminal-inclusive cutoff plus the lag-recovery
    /// state machine are easier to read as one explicit `poll_next`.
    struct EventStream {
        payment_id: PaymentId,
        head: Option<PaymentStatusSnapshot>,
        #[pin]
        tail: BroadcastStream<PaymentStatusSnapshot>,
        resync_source: Arc<dyn ResyncSource>,
        resync_in_flight: Option<ResyncFuture>,
        closed: bool,
    }
}

impl EventStream {
    fn new(
        payment_id: PaymentId,
        initial: PaymentStatusSnapshot,
        rx: broadcast::Receiver<PaymentStatusSnapshot>,
        resync_source: Arc<dyn ResyncSource>,
    ) -> Self {
        Self {
            payment_id,
            head: Some(initial),
            tail: BroadcastStream::new(rx),
            resync_source,
            resync_in_flight: None,
            closed: false,
        }
    }
}

impl Stream for EventStream {
    type Item = Result<Event, axum::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        if *this.closed {
            return Poll::Ready(None);
        }

        if let Some(initial) = this.head.take() {
            let terminal = initial.status.is_terminal();
            let event = snapshot_to_event(&initial, this.payment_id);
            if terminal {
                *this.closed = true;
            }
            return Poll::Ready(Some(Ok(event)));
        }

        // Drain a pending resync first: while a re-read is in flight, we
        // must not advance the broadcast tail or we would interleave a
        // newer tail event ahead of the resync snapshot the subscriber
        // is owed.
        if let Some(future) = this.resync_in_flight.as_mut() {
            match future.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(snapshot)) => {
                    *this.resync_in_flight = None;
                    let terminal = snapshot.status.is_terminal();
                    let event = snapshot_to_event(&snapshot, this.payment_id);
                    if terminal {
                        *this.closed = true;
                    }
                    return Poll::Ready(Some(Ok(event)));
                }
                Poll::Ready(Err(err)) => {
                    *this.resync_in_flight = None;
                    // Store failure during resync is unrecoverable for
                    // this connection: we can't trust the in-buffer tail
                    // (it may have been clobbered by the lag we were
                    // recovering from). Surface a serialization_failed
                    // event so the bridge can refetch via REST and close
                    // the stream.
                    tracing::error!(
                        payment_id = %this.payment_id.0,
                        error = %err,
                        "SSE resync re-read failed; closing stream",
                    );
                    *this.closed = true;
                    return Poll::Ready(Some(Ok(resync_failed_event(this.payment_id))));
                }
            }
        }

        match this.tail.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(snapshot))) => {
                let terminal = snapshot.status.is_terminal();
                let event = snapshot_to_event(&snapshot, this.payment_id);
                if terminal {
                    *this.closed = true;
                }
                Poll::Ready(Some(Ok(event)))
            }
            Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_)))) => {
                // Kick off the resync re-read and emit the lag event now;
                // the next poll will drive the in-flight future and emit
                // the recovered snapshot.
                let resync_source = Arc::clone(this.resync_source);
                let payment_id = this.payment_id.clone();
                *this.resync_in_flight =
                    Some(Box::pin(
                        async move { resync_source.re_read(&payment_id).await },
                    ));
                Poll::Ready(Some(Ok(lag_event())))
            }
            Poll::Ready(None) => {
                *this.closed = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn snapshot_to_event(snapshot: &PaymentStatusSnapshot, payment_id: &PaymentId) -> Event {
    // `Event::json_data` only fails when `serde_json::to_string` itself
    // fails. `PaymentStatusSnapshot` serialization is total in every
    // shape we control, but we still log and emit an explicit failure
    // event rather than masquerading as a normal snapshot: a corrupt
    // wire frame is operator-visible noise we would never want a
    // subscriber to silently consume as if it were valid state.
    match Event::default().event("snapshot").json_data(snapshot) {
        Ok(event) => event,
        Err(err) => {
            tracing::error!(
                payment_id = %payment_id.0,
                error = %err,
                "SSE snapshot serialization failed; emitting serialization_failed event",
            );
            serialization_failed_event(payment_id)
        }
    }
}

fn lag_event() -> Event {
    Event::default().event("lag").data(r#"{"reason":"resync"}"#)
}

fn serialization_failed_event(payment_id: &PaymentId) -> Event {
    let body = serde_json::json!({ "payment_id": payment_id.0 }).to_string();
    Event::default().event("serialization_failed").data(body)
}

fn resync_failed_event(payment_id: &PaymentId) -> Event {
    let body = serde_json::json!({ "payment_id": payment_id.0 }).to_string();
    Event::default().event("resync_failed").data(body)
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Duration;

    use futures::StreamExt as _;
    use tokio::sync::broadcast;
    use tokio::time::timeout;
    use tokio_stream::wrappers::BroadcastStream;
    use zpay_core::accepts::{AcceptsEntry, PayeeRegistry};
    use zpay_core::broadcast::{BroadcastClient, BroadcastError, BroadcastOutcome};
    use zpay_core::prepare::{PrepareRequest, PreparedTxCache, propose};
    use zpay_core::status::{
        DEFAULT_FINALITY_DEPTH, IntentPosture, PaymentStatus, PaymentStatusSnapshot,
        SettlementLedger, SettlementLedgerEntry, SettlementLedgerStore, lookup_payment_status,
    };
    use zpay_core::store::StoreError;
    use zpay_core::tip::{ChainTipOracle, TipError};
    use zpay_core::transaction_fetcher::{DisclosedTransaction, FetchError, TransactionFetcher};
    use zpay_core::types::{PayeeId, PaymentId, PaymentNetwork, PaymentScheme, Zatoshis};
    use zpay_core::verify::{
        AmountReconciliation, ChainPresence, CryptographicVerdict, PaymentDisclosureVerifier,
        VerifyError, VerifyResponse,
    };

    use super::{EventStream, PaymentEventHub, ResyncSource, StoreResyncSource};
    use crate::AppState;

    fn store_resync(
        cache: &Arc<PreparedTxCache>,
        ledger: &Arc<SettlementLedger>,
    ) -> Arc<dyn ResyncSource> {
        Arc::new(StoreResyncSource {
            prepared_store: Arc::clone(cache),
            ledger: Arc::clone(ledger),
            finality_depth: DEFAULT_FINALITY_DEPTH,
        })
    }

    /// Resync source that always returns the snapshot it was constructed
    /// with. Used by tests that do not exercise the lag path.
    struct StaticResync {
        snapshot: PaymentStatusSnapshot,
    }

    impl ResyncSource for StaticResync {
        fn re_read<'a>(
            &'a self,
            _payment_id: &'a PaymentId,
        ) -> Pin<Box<dyn Future<Output = Result<PaymentStatusSnapshot, StoreError>> + Send + 'a>>
        {
            let snapshot = self.snapshot.clone();
            Box::pin(async move { Ok(snapshot) })
        }
    }

    struct UnusedChain;

    impl BroadcastClient for UnusedChain {
        async fn broadcast(&self, _raw_tx_hex: &str) -> Result<BroadcastOutcome, BroadcastError> {
            Err(BroadcastError::Unavailable {
                reason: "test fixture: broadcast must not be called".to_owned(),
            })
        }
    }

    struct UnusedVerifier;

    impl PaymentDisclosureVerifier for UnusedVerifier {
        async fn verify_disclosure<Fetcher>(
            &self,
            _disclosure_bytes: &[u8],
            _fetcher: &Fetcher,
        ) -> Result<VerifyResponse, VerifyError>
        where
            Fetcher: TransactionFetcher + ?Sized,
        {
            Ok(VerifyResponse {
                cryptographic_verdict: CryptographicVerdict::Inconclusive,
                inconclusive_reason: None,
                chain_presence: ChainPresence::OracleUnavailable,
                amount_reconciliation: AmountReconciliation::NotChecked,
                transaction_id: None,
                payment_id: None,
                disclosed_value_zat: None,
            })
        }
    }

    struct UnusedFetcher;

    impl TransactionFetcher for UnusedFetcher {
        async fn fetch_transaction(
            &self,
            _txid: [u8; 32],
        ) -> Result<DisclosedTransaction, FetchError> {
            Err(FetchError::Unavailable {
                reason: "test fixture: fetcher must not be called".to_owned(),
            })
        }
    }

    struct FixedTipOracle;

    impl ChainTipOracle for FixedTipOracle {
        async fn current_tip(&self, _network: PaymentNetwork) -> Result<u32, TipError> {
            Ok(3_217_900)
        }
    }

    fn valid_prepare_request() -> PrepareRequest {
        PrepareRequest {
            payee_id: PayeeId("aether-ai".to_owned()),
            network: PaymentNetwork::Testnet,
            scheme: PaymentScheme::Zcash,
            resource_uri: "https://example.test/resources/events".to_owned(),
            nonce: "00000000-0000-0000-0000-0000000000aa".to_owned(),
            evidence_pack_hash: None,
            idempotency_key: None,
        }
    }

    fn fixture_registry() -> PayeeRegistry {
        let mut registry = PayeeRegistry::new();
        registry.register(
            PayeeId("aether-ai".to_owned()),
            vec![AcceptsEntry {
                scheme: PaymentScheme::Zcash,
                network: PaymentNetwork::Testnet,
                pay_to: "utest1exampleaddress".to_owned(),
                amount_zat: Zatoshis(50_000),
                max_validity_seconds: 600,
                expiry_delta_blocks: None,
                merchant_requires_verify: false,
            }],
        );
        registry
    }

    fn build_state(
        cache: Arc<PreparedTxCache>,
        ledger: Arc<SettlementLedger>,
        events: Arc<PaymentEventHub>,
    ) -> AppState<
        UnusedChain,
        UnusedVerifier,
        PreparedTxCache,
        SettlementLedger,
        FixedTipOracle,
        UnusedFetcher,
    > {
        AppState::new(
            cache,
            ledger,
            Arc::new(PayeeRegistry::new()),
            Arc::new(UnusedChain),
            Arc::new(UnusedVerifier),
            events,
            Arc::new(FixedTipOracle),
            Arc::new(UnusedFetcher),
            Arc::new(crate::dpop::InMemoryReplayStore::new()),
            crate::dpop::DpopExpectations::unbound("http"),
            DEFAULT_FINALITY_DEPTH,
        )
    }

    #[test]
    fn payment_id_parse_rejects_empty_and_oversized_ids() -> Result<(), &'static str> {
        assert!("".parse::<PaymentId>().is_err());
        assert!("   ".parse::<PaymentId>().is_err());
        let oversized = "x".repeat(65);
        assert!(oversized.parse::<PaymentId>().is_err());
        let ok = "01JABCXYZ"
            .parse::<PaymentId>()
            .map_err(|_| "ulid-shaped id must parse")?;
        assert_eq!(ok.0, "01JABCXYZ");
        Ok(())
    }

    #[test]
    fn publish_without_subscribers_does_not_create_an_entry() {
        let hub = PaymentEventHub::new();
        let payment_id = PaymentId("never-watched".to_owned());
        hub.publish(
            &payment_id,
            PaymentStatusSnapshot {
                payment_id: payment_id.clone(),
                status: PaymentStatus::Awaiting,
                intent_posture: IntentPosture::Unverified,
                broadcast_outcome: None,
                settled_at_unix_seconds: None,
                confirmation_count: None,
                mined_block_height: None,
            },
        );
        assert_eq!(hub.channel_count(), 0);
    }

    #[test]
    fn subscribe_creates_exactly_one_entry_per_id() {
        let hub = PaymentEventHub::new();
        let payment_id = PaymentId("once-watched".to_owned());
        let _rx_a = hub.subscribe(&payment_id);
        let _rx_b = hub.subscribe(&payment_id);
        assert_eq!(hub.channel_count(), 1);
    }

    #[tokio::test]
    async fn subscribe_then_publish_delivers_snapshot_and_stream_closes_on_terminal()
    -> Result<(), Box<dyn std::error::Error>> {
        let cache = Arc::new(PreparedTxCache::new());
        let ledger = Arc::new(SettlementLedger::new());
        let hub = Arc::new(PaymentEventHub::new());

        // Use a real prepare so the connect-time snapshot reports
        // `prepared` and the route accepts the id (existence check
        // passes via the prepared_store branch of lookup_payment_status).
        let registry = fixture_registry();
        let oracle = FixedTipOracle;
        let preparation = propose(
            valid_prepare_request(),
            "test-jkt-events".to_owned(),
            cache.as_ref(),
            &registry,
            &oracle,
        )
        .await
        .map_err(|_| "propose failed")?;
        let payment_id = preparation.payment_id.clone();

        // The route opens by reading the snapshot and then subscribing.
        // The end-to-end exercise mirrors that ordering so we go through
        // the hub the same way the SSE handler does.
        let initial = lookup_payment_status(
            &payment_id,
            cache.as_ref(),
            ledger.as_ref(),
            DEFAULT_FINALITY_DEPTH,
        )
        .await?;
        assert_eq!(initial.status, PaymentStatus::Awaiting);
        let rx = hub.subscribe(&payment_id);

        // Simulate the confirmation oracle publishing a terminal Final
        // snapshot once `confirmation_count >= ZPAY_FINALITY_DEPTH`.
        let terminal_snapshot = PaymentStatusSnapshot {
            payment_id: payment_id.clone(),
            status: PaymentStatus::Final,
            intent_posture: IntentPosture::Unverified,
            broadcast_outcome: Some(BroadcastOutcome::Accepted {
                transaction_id: "deadbeef".to_owned(),
            }),
            settled_at_unix_seconds: Some(1_730_000_000),
            confirmation_count: Some(3),
            mined_block_height: Some(2_000_000),
        };
        hub.publish(&payment_id, terminal_snapshot.clone());

        let resync = store_resync(&cache, &ledger);
        let mut stream = std::pin::pin!(EventStream::new(payment_id.clone(), initial, rx, resync));

        // First event: the initial awaiting snapshot.
        let first = timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or("expected initial event")??;
        let payload = format!("{first:?}");
        assert!(
            payload.contains("\\\"status\\\":\\\"awaiting\\\""),
            "first event should report awaiting status, got {payload}",
        );

        // Second event: the terminal final snapshot delivered via the
        // broadcast tail.
        let second = timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or("expected broadcast event")??;
        let payload = format!("{second:?}");
        assert!(
            payload.contains("\\\"status\\\":\\\"final\\\""),
            "second event should report final status, got {payload}",
        );

        // Third poll: the stream MUST close after delivering the
        // terminal event inclusive. Bound with a timeout so a regression
        // that left the stream open fails fast instead of hanging.
        let closed = timeout(Duration::from_secs(2), stream.next()).await?;
        assert!(closed.is_none(), "stream must close after terminal event");

        // The AppState wiring is exercised indirectly via build_state;
        // we touch it here to keep the import live and catch breakage
        // if AppState ever changes shape.
        let _state = build_state(cache, ledger, hub);
        Ok(())
    }

    #[tokio::test]
    async fn terminal_initial_snapshot_emits_once_then_closes()
    -> Result<(), Box<dyn std::error::Error>> {
        let ledger = SettlementLedger::new();
        let payment_id = PaymentId("already-final".to_owned());
        ledger
            .record(
                payment_id.clone(),
                SettlementLedgerEntry {
                    broadcast_outcome: BroadcastOutcome::Accepted {
                        transaction_id: "abcd".to_owned(),
                    },
                    settled_at_unix_seconds: 1_730_000_000,
                    confirmation_count: Some(DEFAULT_FINALITY_DEPTH),
                    mined_block_height: Some(2_000_000),
                },
            )
            .await?;

        let cache = PreparedTxCache::new();
        let initial =
            lookup_payment_status(&payment_id, &cache, &ledger, DEFAULT_FINALITY_DEPTH).await?;
        assert_eq!(initial.status, PaymentStatus::Final);

        let hub = PaymentEventHub::new();
        let rx = hub.subscribe(&payment_id);
        let cache_arc = Arc::new(cache);
        let ledger_arc = Arc::new(ledger);
        let resync = store_resync(&cache_arc, &ledger_arc);
        let mut stream = std::pin::pin!(EventStream::new(payment_id.clone(), initial, rx, resync));

        let first = timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or("expected initial event")??;
        let payload = format!("{first:?}");
        assert!(payload.contains("\\\"status\\\":\\\"final\\\""));

        let closed = timeout(Duration::from_secs(2), stream.next()).await?;
        assert!(
            closed.is_none(),
            "terminal initial snapshot must close the stream after one event",
        );
        Ok(())
    }

    /// Race regression for the probe-subscribe-reread ordering.
    ///
    /// The handler probes the store, subscribes, then re-reads to
    /// capture the canonical initial snapshot. A `/settle` publish
    /// landing between subscribe and the re-read is the kind that
    /// would have been lost under the original snapshot-first order;
    /// the re-read closes the race because the store always reflects
    /// the terminal state by the time `publish` runs.
    ///
    /// This test exercises the stream-level invariant the handler
    /// relies on: even when a publish was deposited into a channel
    /// that no subscriber read from yet, the buffered tail still
    /// reaches the new subscriber after the initial event.
    ///
    /// 1. Subscribe to attach a receiver to the channel.
    /// 2. `publish` the terminal snapshot while no one is reading.
    /// 3. Build the stream with the pre-publish snapshot as initial.
    /// 4. Drive the stream and assert the terminal event arrives via
    ///    the buffered broadcast tail.
    #[tokio::test]
    async fn subscribe_before_snapshot_captures_publish_in_race_window()
    -> Result<(), Box<dyn std::error::Error>> {
        let cache = Arc::new(PreparedTxCache::new());
        let ledger = Arc::new(SettlementLedger::new());
        let hub = Arc::new(PaymentEventHub::new());

        let registry = fixture_registry();
        let oracle = FixedTipOracle;
        let preparation = propose(
            valid_prepare_request(),
            "test-jkt-events".to_owned(),
            cache.as_ref(),
            &registry,
            &oracle,
        )
        .await
        .map_err(|_| "propose failed")?;
        let payment_id = preparation.payment_id.clone();

        // Subscribe FIRST so the broadcast channel exists and any publish
        // is buffered for the receiver. This is the new handler order.
        let rx = hub.subscribe(&payment_id);

        // A publish lands during the race window (after subscribe, before
        // the snapshot read). Under the old order this would have been a
        // no-op because no subscriber was attached yet AND the snapshot
        // read had already returned `Awaiting`. Under the new order it
        // sits in the broadcast buffer waiting to be consumed.
        let terminal_snapshot = PaymentStatusSnapshot {
            payment_id: payment_id.clone(),
            status: PaymentStatus::Final,
            intent_posture: IntentPosture::Unverified,
            broadcast_outcome: Some(BroadcastOutcome::Accepted {
                transaction_id: "racewin".to_owned(),
            }),
            settled_at_unix_seconds: Some(1_730_000_000),
            confirmation_count: Some(DEFAULT_FINALITY_DEPTH),
            mined_block_height: Some(2_000_000),
        };
        hub.publish(&payment_id, terminal_snapshot.clone());

        // Snapshot read happens AFTER the publish. The ledger has not
        // recorded anything (the publish is just a broadcast hint), so
        // the snapshot still reports `Awaiting`. The terminal event lives
        // exclusively in the broadcast buffer at this point.
        let initial = lookup_payment_status(
            &payment_id,
            cache.as_ref(),
            ledger.as_ref(),
            DEFAULT_FINALITY_DEPTH,
        )
        .await?;
        assert_eq!(initial.status, PaymentStatus::Awaiting);

        let resync = store_resync(&cache, &ledger);
        let mut stream = std::pin::pin!(EventStream::new(payment_id.clone(), initial, rx, resync,));

        // First event: the initial awaiting snapshot (head of stream).
        let first = timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or("expected initial event")??;
        let payload = format!("{first:?}");
        assert!(
            payload.contains("\\\"status\\\":\\\"awaiting\\\""),
            "first event should report awaiting, got {payload}",
        );

        // Second event: the buffered terminal snapshot. The new ordering
        // is what makes this reachable; under the old ordering the
        // publish would have been dropped.
        let second = timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or("expected buffered terminal event")??;
        let payload = format!("{second:?}");
        assert!(
            payload.contains("\\\"status\\\":\\\"final\\\""),
            "second event should report final, got {payload}",
        );

        // Stream closes after the terminal event.
        let closed = timeout(Duration::from_secs(2), stream.next()).await?;
        assert!(closed.is_none(), "stream must close after terminal event");
        Ok(())
    }

    /// Lag resync regression for terminal recovery.
    ///
    /// When the broadcast receiver lags past the channel buffer and the
    /// lagged window contained the only terminal snapshot, the stream
    /// must resync from the backing stores rather than stay open
    /// forever. The test forces `Lagged` by publishing more snapshots
    /// than the channel capacity holds without polling the receiver,
    /// then drives the stream and asserts the sequence: `lag` event,
    /// recovered `snapshot` event from the resync source, stream close.
    #[tokio::test]
    async fn lag_triggers_resync_and_closes_on_terminal_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let payment_id = PaymentId("lag-victim".to_owned());

        // Capacity 1 makes `Lagged` cheap to trigger: any second publish
        // that lands before the receiver consumes the first pushes the
        // receiver into a lagged state.
        let (tx, rx) = broadcast::channel::<PaymentStatusSnapshot>(1);

        let non_terminal = PaymentStatusSnapshot {
            payment_id: payment_id.clone(),
            status: PaymentStatus::Awaiting,
            intent_posture: IntentPosture::Unverified,
            broadcast_outcome: None,
            settled_at_unix_seconds: None,
            confirmation_count: None,
            mined_block_height: None,
        };
        let terminal = PaymentStatusSnapshot {
            payment_id: payment_id.clone(),
            status: PaymentStatus::Final,
            intent_posture: IntentPosture::Unverified,
            broadcast_outcome: Some(BroadcastOutcome::Accepted {
                transaction_id: "lostinlag".to_owned(),
            }),
            settled_at_unix_seconds: Some(1_730_000_000),
            confirmation_count: Some(DEFAULT_FINALITY_DEPTH),
            mined_block_height: Some(2_000_000),
        };

        // Three non-terminal publishes plus one terminal publish into a
        // capacity-1 channel, all without consuming. The receiver sees
        // `Lagged` on its first poll and the terminal snapshot is past
        // the buffer horizon.
        let _ = tx.send(non_terminal.clone());
        let _ = tx.send(non_terminal.clone());
        let _ = tx.send(non_terminal.clone());
        let _ = tx.send(terminal.clone());

        // The resync source is the canonical state that `lookup_payment_status`
        // would return in production. Here we point it at the terminal
        // snapshot directly, simulating the case where the ledger has
        // already recorded the settle outcome by the time the receiver
        // resyncs.
        let resync: Arc<dyn ResyncSource> = Arc::new(StaticResync {
            snapshot: terminal.clone(),
        });

        // Drive the broadcast tail directly (no head snapshot) so the
        // first poll lands on the lag arm without consuming the initial
        // event slot.
        let mut stream = std::pin::pin!(EventStream {
            payment_id: payment_id.clone(),
            head: None,
            tail: BroadcastStream::new(rx),
            resync_source: resync,
            resync_in_flight: None,
            closed: false,
        });

        // First emission: the lag event.
        let lag = timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or("expected lag event")??;
        let lag_payload = format!("{lag:?}");
        assert!(
            lag_payload.contains("lag") && lag_payload.contains("resync"),
            "first event after overflow must be the lag/resync hint, got {lag_payload}",
        );

        // Second emission: the recovered terminal snapshot from the
        // resync source.
        let recovered = timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or("expected resync snapshot")??;
        let recovered_payload = format!("{recovered:?}");
        assert!(
            recovered_payload.contains("\\\"status\\\":\\\"final\\\""),
            "resync snapshot must report the terminal state, got {recovered_payload}",
        );

        // Stream closes after the terminal recovery: this is the bug
        // we're guarding against. Without resync the stream would have
        // stayed open forever after the lag because the terminal
        // snapshot was lost in the buffer overflow.
        let closed = timeout(Duration::from_secs(2), stream.next()).await?;
        assert!(
            closed.is_none(),
            "stream must close after a terminal resync snapshot",
        );
        Ok(())
    }
}
