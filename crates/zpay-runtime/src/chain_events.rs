//! Live chain-event subscription that keeps the settlement ledger
//! reorg-aware in near-real-time.
//!
//! On startup, and after a cursor-expiry, the task runs one full
//! reconciliation sweep (re-polling every unsettled success-kind row
//! through the confirmation oracle) before resuming from the live tail.
//! In steady state it applies each `ChainReorged` envelope immediately by
//! downgrading the ledger rows whose mined height falls in the reverted
//! range, and refreshes the shared chain view from every envelope so the
//! wire handlers read a current visible/settled tip.
//!
//! The subscription loop is generic over an [`EventSource`] and a
//! [`LoopSink`] so the control flow (drain, cursor-expiry resume, backoff)
//! is unit-testable with a scripted stream. Production wires the source to
//! `zinder-client`'s `RemoteChainIndex` and the sink to the shared stores.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use zinder_client::{
    ChainEvent, ChainEventCursor, ChainEventEnvelope, ChainEventStream, EndpointBackedIndex,
    EventStreamStart, IndexerError, RemoteChainIndex,
};
use zpay_core::chain_status::ChainStatusCache;
use zpay_core::status::SettlementLedgerStore;

use crate::zinder_oracle::ZinderConfirmationOracle;
use crate::{
    AnyPreparedTxStore, AnySettlementLedgerStore, PaymentEventHub, now_unix_seconds,
    poll_oracle_once, publish_snapshot,
};

/// Initial reconnect backoff after a transient stream break.
const RECONNECT_BACKOFF_INITIAL_SECONDS: u64 = 1;
/// Ceiling for the exponential reconnect backoff.
const RECONNECT_BACKOFF_MAX_SECONDS: u64 = 30;

/// Collaborators the chain-event task shares with the confirmation poll.
pub(crate) struct ChainEventsDeps {
    pub chain: RemoteChainIndex,
    pub oracle: Arc<ZinderConfirmationOracle>,
    pub ledger: Arc<AnySettlementLedgerStore>,
    pub prepared_store: Arc<AnyPreparedTxStore>,
    pub events: Arc<PaymentEventHub>,
    pub chain_status: Arc<ChainStatusCache>,
    pub finality_depth: u32,
}

/// Spawn the background chain-event task.
pub(crate) fn spawn(deps: ChainEventsDeps) {
    tokio::spawn(run(deps));
    tracing::info!("chain-event subscription wired");
}

async fn run(deps: ChainEventsDeps) {
    let ChainEventsDeps {
        chain,
        oracle,
        ledger,
        prepared_store,
        events,
        chain_status,
        finality_depth,
    } = deps;
    let source = RemoteEventSource { chain };
    let sink = RuntimeSink {
        oracle,
        ledger,
        prepared_store,
        events,
        chain_status,
        finality_depth,
    };
    run_loop(&source, &sink).await;
}

/// Position a subscription resumes from. Mirrors the subset of
/// `EventStreamStart` the loop uses, generic over the cursor token so the
/// loop stays testable without the opaque zinder cursor.
#[derive(Clone)]
enum Resume<C> {
    LiveTail,
    After(C),
}

/// A chain-event delivery reduced to what the loop acts on.
struct Delivery<C> {
    visible_tip: u64,
    settled_tip: u64,
    cursor: C,
    /// `Some((start_height, end_height))` when the envelope is a reorg.
    reverted: Option<(u64, u64)>,
}

/// One item drained from an open subscription.
enum StreamItem<C> {
    /// A delivered envelope.
    Delivery(Delivery<C>),
    /// The resume cursor expired mid-stream; a full sweep is required.
    CursorExpired,
    /// A transient error or clean end; reconnect from the last cursor.
    Disconnect,
}

/// Result of opening a subscription.
enum SubscribeOutcome<S> {
    /// The subscription opened.
    Open(S),
    /// The resume cursor was rejected; a full sweep is required.
    CursorExpired,
    /// A transient error; back off and retry from the same position.
    Reconnect,
    /// No more work; ends the loop.
    #[allow(
        dead_code,
        reason = "constructed only by the test EventSource to terminate the otherwise-infinite production loop"
    )]
    Stop,
}

/// Why a drained subscription stopped.
enum DrainStop {
    CursorExpired,
    Reconnect,
}

/// Source of resumable chain-event subscriptions.
trait EventSource {
    type Cursor: Clone + Send;
    type Stream: EventStream<Cursor = Self::Cursor> + Send;
    fn subscribe(
        &self,
        resume: Resume<Self::Cursor>,
    ) -> impl Future<Output = SubscribeOutcome<Self::Stream>> + Send;
}

/// An open subscription yielding reduced deliveries.
trait EventStream {
    type Cursor: Clone + Send;
    fn next_item(&mut self) -> impl Future<Output = Option<StreamItem<Self::Cursor>>> + Send;
}

/// Side effects the loop performs on the shared stores.
trait LoopSink {
    fn reconcile(&self) -> impl Future<Output = ()> + Send;
    fn store_chain_view(&self, visible_tip: u64, settled_tip: u64);
    fn handle_reorg(
        &self,
        reverted_start_height: u64,
        reverted_end_height: u64,
    ) -> impl Future<Output = ()> + Send;
    fn backoff_sleep(&self, backoff: Duration) -> impl Future<Output = ()> + Send;
}

/// Drive the subscription loop: one startup sweep, then subscribe, drain,
/// and resume forever, running a full sweep on every cursor expiry.
async fn run_loop<S, K>(source: &S, sink: &K)
where
    S: EventSource + Sync,
    K: LoopSink + Sync,
{
    sink.reconcile().await;

    let mut resume = Resume::LiveTail;
    let mut backoff = Duration::from_secs(RECONNECT_BACKOFF_INITIAL_SECONDS);

    loop {
        match source.subscribe(resume.clone()).await {
            SubscribeOutcome::Open(mut stream) => {
                let (stop, progressed) = drain(&mut stream, sink, &mut resume).await;
                if progressed {
                    backoff = Duration::from_secs(RECONNECT_BACKOFF_INITIAL_SECONDS);
                }
                match stop {
                    DrainStop::CursorExpired => {
                        tracing::warn!(
                            "chain-event cursor expired mid-stream; reconciling then resuming from live tail",
                        );
                        sink.reconcile().await;
                        resume = Resume::LiveTail;
                    }
                    DrainStop::Reconnect => {
                        sink.backoff_sleep(backoff).await;
                        backoff = grow(backoff);
                    }
                }
            }
            SubscribeOutcome::CursorExpired => {
                tracing::warn!("chain-event cursor expired; running reconciliation sweep");
                sink.reconcile().await;
                resume = Resume::LiveTail;
            }
            SubscribeOutcome::Reconnect => {
                sink.backoff_sleep(backoff).await;
                backoff = grow(backoff);
            }
            SubscribeOutcome::Stop => break,
        }
    }
}

/// Drain one live subscription until it errors or ends.
///
/// Advances `resume` past every applied envelope. Returns why it stopped and
/// whether at least one envelope was applied (so the caller resets backoff).
async fn drain<St, K>(
    stream: &mut St,
    sink: &K,
    resume: &mut Resume<St::Cursor>,
) -> (DrainStop, bool)
where
    St: EventStream + Send,
    K: LoopSink + Sync,
{
    let mut progressed = false;
    while let Some(next) = stream.next_item().await {
        match next {
            StreamItem::Delivery(delivery) => {
                progressed = true;
                sink.store_chain_view(delivery.visible_tip, delivery.settled_tip);
                *resume = Resume::After(delivery.cursor);
                if let Some((start_height, end_height)) = delivery.reverted {
                    sink.handle_reorg(start_height, end_height).await;
                }
            }
            StreamItem::CursorExpired => return (DrainStop::CursorExpired, progressed),
            StreamItem::Disconnect => {
                tracing::warn!("chain-event stream error; reconnecting");
                return (DrainStop::Reconnect, progressed);
            }
        }
    }
    (DrainStop::Reconnect, progressed)
}

/// Production event source backed by `zinder-client`'s `RemoteChainIndex`.
struct RemoteEventSource {
    chain: RemoteChainIndex,
}

impl EventSource for RemoteEventSource {
    type Cursor = ChainEventCursor;
    type Stream = RemoteEventStream;

    async fn subscribe(
        &self,
        resume: Resume<ChainEventCursor>,
    ) -> SubscribeOutcome<RemoteEventStream> {
        let start = match resume {
            Resume::LiveTail => EventStreamStart::LiveTail,
            Resume::After(cursor) => EventStreamStart::AfterCursor(cursor),
        };
        match self.chain.chain_events(start).await {
            Ok(stream) => SubscribeOutcome::Open(RemoteEventStream { stream }),
            Err(err) if is_cursor_error(&err) => SubscribeOutcome::CursorExpired,
            Err(err) => {
                tracing::warn!(error = %err, "chain-event subscribe failed; backing off");
                SubscribeOutcome::Reconnect
            }
        }
    }
}

/// Production open subscription over a `zinder-client` `ChainEventStream`.
struct RemoteEventStream {
    stream: ChainEventStream,
}

impl EventStream for RemoteEventStream {
    type Cursor = ChainEventCursor;

    async fn next_item(&mut self) -> Option<StreamItem<ChainEventCursor>> {
        match self.stream.next().await {
            Some(Ok(envelope)) => Some(StreamItem::Delivery(delivery_from_envelope(envelope))),
            Some(Err(err)) if is_cursor_error(&err) => Some(StreamItem::CursorExpired),
            Some(Err(err)) => {
                tracing::warn!(error = %err, "chain-event stream error");
                Some(StreamItem::Disconnect)
            }
            None => None,
        }
    }
}

/// Reduce a `zinder-client` envelope to the loop's [`Delivery`].
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "ChainEvent is #[non_exhaustive]; only ChainReorged carries a reverted range, and every other variant reduces to a plain view refresh"
)]
fn delivery_from_envelope(envelope: ChainEventEnvelope) -> Delivery<ChainEventCursor> {
    let reverted = match envelope.event {
        ChainEvent::ChainReorged { reverted, .. } => Some((
            u64::from(reverted.block_range.start.value()),
            u64::from(reverted.block_range.end.value()),
        )),
        _ => None,
    };
    Delivery {
        visible_tip: u64::from(envelope.chain_epoch.visible_tip_height.value()),
        settled_tip: u64::from(envelope.chain_epoch.settled_tip_height.value()),
        cursor: envelope.cursor,
        reverted,
    }
}

/// Production sink over the shared confirmation oracle and stores.
struct RuntimeSink {
    oracle: Arc<ZinderConfirmationOracle>,
    ledger: Arc<AnySettlementLedgerStore>,
    prepared_store: Arc<AnyPreparedTxStore>,
    events: Arc<PaymentEventHub>,
    chain_status: Arc<ChainStatusCache>,
    finality_depth: u32,
}

impl LoopSink for RuntimeSink {
    async fn reconcile(&self) {
        poll_oracle_once(
            self.oracle.as_ref(),
            self.ledger.as_ref(),
            self.prepared_store.as_ref(),
            self.events.as_ref(),
            self.chain_status.as_ref(),
            self.finality_depth,
        )
        .await;
    }

    fn store_chain_view(&self, visible_tip: u64, settled_tip: u64) {
        self.chain_status.store(visible_tip, settled_tip);
    }

    async fn handle_reorg(&self, reverted_start_height: u64, reverted_end_height: u64) {
        metrics::counter!("zpay_chain_reorgs_observed_total").increment(1);
        tracing::debug!(
            reverted_start_height,
            reverted_end_height,
            "chain-event reorg observed",
        );
        let downgraded = match self
            .ledger
            .downgrade_reorged_range(
                reverted_start_height,
                reverted_end_height,
                now_unix_seconds(),
            )
            .await
        {
            Ok(downgraded) => downgraded,
            Err(err) => {
                tracing::warn!(error = %err, "downgrade_reorged_range failed");
                return;
            }
        };
        if downgraded.is_empty() {
            return;
        }
        metrics::counter!("zpay_reorg_downgrades_total", "source" => "chain_event")
            .increment(u64::try_from(downgraded.len()).unwrap_or(u64::MAX));
        tracing::info!(
            reverted_start_height,
            reverted_end_height,
            downgraded_count = downgraded.len(),
            "reorg downgrade: returned mined payments to broadcast",
        );
        let chain_view = self.chain_status.load();
        for payment_id in downgraded {
            if self.events.has_subscribers(&payment_id) {
                publish_snapshot(
                    self.prepared_store.as_ref(),
                    self.ledger.as_ref(),
                    self.events.as_ref(),
                    chain_view,
                    self.finality_depth,
                    &payment_id,
                )
                .await;
            }
        }
    }

    async fn backoff_sleep(&self, backoff: Duration) {
        tokio::time::sleep(backoff).await;
    }
}

/// Classify an error as a resume-cursor expiry or invalidation.
///
/// zinder maps an expired chain-event cursor to `FailedPrecondition` and
/// an undecodable cursor to `InvalidRequest`; either means the resume
/// point is gone and a full sweep must re-anchor the subscription.
fn is_cursor_error(err: &IndexerError) -> bool {
    matches!(err, IndexerError::FailedPrecondition { .. })
        || matches!(err, IndexerError::InvalidRequest { reason } if reason.contains("cursor"))
}

fn grow(backoff: Duration) -> Duration {
    (backoff * 2).min(Duration::from_secs(RECONNECT_BACKOFF_MAX_SECONDS))
}

#[cfg(test)]
mod tests {
    use super::{
        Delivery, EventSource, EventStream, LoopSink, Resume, RuntimeSink, StreamItem,
        SubscribeOutcome, run_loop,
    };
    use crate::zinder_oracle::ZinderConfirmationOracle;
    use crate::{AnyPreparedTxStore, AnySettlementLedgerStore, PaymentEventHub};
    use parking_lot::Mutex;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::time::Duration;
    use zpay_core::chain_status::ChainStatusCache;
    use zpay_core::prepare::PreparedTxCache;
    use zpay_core::status::SettlementLedger;

    /// One scripted subscription attempt handed to the loop in order.
    enum Attempt {
        /// Open a stream that yields these items, then ends.
        Open(Vec<StreamItem<u64>>),
        /// Reject the subscribe as a cursor expiry.
        CursorExpired,
        /// Reject the subscribe as a transient error.
        Reconnect,
    }

    /// Resume position observed at each subscribe, for assertions.
    #[derive(Debug, PartialEq, Eq)]
    enum ResumeKind {
        LiveTail,
        After(u64),
    }

    struct ScriptStream {
        items: VecDeque<StreamItem<u64>>,
    }

    impl EventStream for ScriptStream {
        type Cursor = u64;
        async fn next_item(&mut self) -> Option<StreamItem<u64>> {
            self.items.pop_front()
        }
    }

    struct ScriptSource {
        attempts: Mutex<VecDeque<Attempt>>,
        resume_log: Mutex<Vec<ResumeKind>>,
    }

    impl ScriptSource {
        fn new(attempts: Vec<Attempt>) -> Self {
            Self {
                attempts: Mutex::new(attempts.into()),
                resume_log: Mutex::new(Vec::new()),
            }
        }
    }

    impl EventSource for ScriptSource {
        type Cursor = u64;
        type Stream = ScriptStream;
        async fn subscribe(&self, resume: Resume<u64>) -> SubscribeOutcome<ScriptStream> {
            self.resume_log.lock().push(match resume {
                Resume::LiveTail => ResumeKind::LiveTail,
                Resume::After(cursor) => ResumeKind::After(cursor),
            });
            let attempt = self.attempts.lock().pop_front();
            match attempt {
                Some(Attempt::Open(items)) => SubscribeOutcome::Open(ScriptStream {
                    items: items.into(),
                }),
                Some(Attempt::CursorExpired) => SubscribeOutcome::CursorExpired,
                Some(Attempt::Reconnect) => SubscribeOutcome::Reconnect,
                None => SubscribeOutcome::Stop,
            }
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        reconciles: Mutex<u32>,
        reorgs: Mutex<Vec<(u64, u64)>>,
        views: Mutex<Vec<(u64, u64)>>,
        sleeps: Mutex<u32>,
    }

    impl LoopSink for RecordingSink {
        async fn reconcile(&self) {
            *self.reconciles.lock() += 1;
        }
        fn store_chain_view(&self, visible_tip: u64, settled_tip: u64) {
            self.views.lock().push((visible_tip, settled_tip));
        }
        async fn handle_reorg(&self, reverted_start_height: u64, reverted_end_height: u64) {
            self.reorgs
                .lock()
                .push((reverted_start_height, reverted_end_height));
        }
        async fn backoff_sleep(&self, _backoff: Duration) {
            *self.sleeps.lock() += 1;
        }
    }

    fn delivery(cursor: u64, reverted: Option<(u64, u64)>) -> StreamItem<u64> {
        StreamItem::Delivery(Delivery {
            visible_tip: 1000 + cursor,
            settled_tip: 900 + cursor,
            cursor,
            reverted,
        })
    }

    #[tokio::test]
    async fn reorg_delivery_triggers_downgrade_and_refreshes_view() {
        let source = ScriptSource::new(vec![Attempt::Open(vec![
            delivery(1, None),
            delivery(2, Some((900, 950))),
        ])]);
        let sink = RecordingSink::default();

        run_loop(&source, &sink).await;

        assert_eq!(*sink.reorgs.lock(), vec![(900, 950)]);
        // The reverted range downgrade fired from the chain-event path.
        assert_eq!(sink.views.lock().len(), 2, "view refreshed per delivery");
        // Only the startup sweep ran; a reorg is not a cursor expiry.
        assert_eq!(*sink.reconciles.lock(), 1);
    }

    /// `handle_reorg` observes a reorg before it ever asks the ledger to
    /// downgrade anything.
    ///
    /// An empty ledger (zero rows to downgrade) must still leave the reorg
    /// visible to an operator via `zpay_chain_reorgs_observed_total` and a
    /// debug log, instead of returning silently. The global metrics recorder
    /// makes the counter itself awkward to assert per-test, so this drives
    /// the production sink directly through the `LoopSink` trait seam (the
    /// same seam `RecordingSink` exercises above) and asserts the
    /// zero-downgrade path still completes cleanly past that observability
    /// hook.
    #[tokio::test]
    async fn handle_reorg_completes_through_the_observed_then_zero_downgrade_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let oracle = ZinderConfirmationOracle::connect(
            "http://127.0.0.1:1".to_owned(),
            zinder_client::Network::ZcashRegtest,
        )?;
        let sink = RuntimeSink {
            oracle: Arc::new(oracle),
            ledger: Arc::new(AnySettlementLedgerStore::Memory(SettlementLedger::new())),
            prepared_store: Arc::new(AnyPreparedTxStore::Memory(PreparedTxCache::new())),
            events: Arc::new(PaymentEventHub::new()),
            chain_status: Arc::new(ChainStatusCache::new()),
            finality_depth: 3,
        };

        sink.handle_reorg(900, 950).await;
        Ok(())
    }

    #[tokio::test]
    async fn cursor_expired_triggers_full_sweep_and_live_tail_resume() {
        let source = ScriptSource::new(vec![Attempt::Open(vec![
            delivery(5, None),
            StreamItem::CursorExpired,
        ])]);
        let sink = RecordingSink::default();

        run_loop(&source, &sink).await;

        // Startup sweep plus the cursor-expiry sweep.
        assert_eq!(*sink.reconciles.lock(), 2);
        // Both subscribes resume from the live tail: the first at startup, the
        // second after the cursor expiry re-anchored the stream.
        assert_eq!(
            *source.resume_log.lock(),
            vec![ResumeKind::LiveTail, ResumeKind::LiveTail],
        );
    }

    #[tokio::test]
    async fn subscribe_cursor_error_sweeps_then_resumes_live_tail() {
        let source = ScriptSource::new(vec![
            Attempt::Open(vec![delivery(9, None)]),
            Attempt::CursorExpired,
        ]);
        let sink = RecordingSink::default();

        run_loop(&source, &sink).await;

        // Startup sweep, then a sweep on the subscribe-level cursor rejection.
        assert_eq!(*sink.reconciles.lock(), 2);
        // Third subscribe resumes from live tail after the cursor rejection.
        assert_eq!(
            *source.resume_log.lock(),
            vec![
                ResumeKind::LiveTail,
                ResumeKind::After(9),
                ResumeKind::LiveTail,
            ],
        );
    }

    #[tokio::test]
    async fn subscribe_transient_error_backs_off_then_retries() {
        let source = ScriptSource::new(vec![
            Attempt::Reconnect,
            Attempt::Open(vec![delivery(3, None)]),
        ]);
        let sink = RecordingSink::default();

        run_loop(&source, &sink).await;

        // A transient subscribe failure is not a cursor expiry: only the
        // startup sweep ran.
        assert_eq!(*sink.reconciles.lock(), 1);
        // One sleep for the rejected subscribe, one for the drained stream's
        // clean end.
        assert_eq!(*sink.sleeps.lock(), 2);
        // The retry after a transient rejection resumes from the same live
        // tail; only the applied delivery advances the cursor.
        assert_eq!(
            *source.resume_log.lock(),
            vec![
                ResumeKind::LiveTail,
                ResumeKind::LiveTail,
                ResumeKind::After(3),
            ],
        );
    }

    #[tokio::test]
    async fn plain_disconnect_resumes_from_after_cursor() {
        let source = ScriptSource::new(vec![Attempt::Open(vec![
            delivery(7, None),
            StreamItem::Disconnect,
        ])]);
        let sink = RecordingSink::default();

        run_loop(&source, &sink).await;

        // A disconnect is not a cursor expiry, so only the startup sweep ran.
        assert_eq!(*sink.reconciles.lock(), 1);
        // One backoff sleep on the reconnect path.
        assert_eq!(*sink.sleeps.lock(), 1);
        // The reconnect resumes strictly after the last applied cursor.
        assert_eq!(
            *source.resume_log.lock(),
            vec![ResumeKind::LiveTail, ResumeKind::After(7)],
        );
    }
}
