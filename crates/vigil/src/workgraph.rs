//! The typed reflexive work graph.
//!
//! Every stage of the video runtime consumes a [`WorkEnvelope`] plus a typed
//! payload, returns a [`ResultEnvelope`] plus a typed result, and records a
//! [`StageReceipt`] for the backend attempt that actually ran. Work carries
//! enough identity, ordering, timing, priority, and provenance that a stage
//! can reject, retry, deduplicate, and join work without process-local state.
//!
//! Stage identifiers are an open vocabulary: adding a new stage requires no
//! change to this module. Envelopes may gain fields; the named fields here are
//! a stable contract.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Envelope/payload compatibility version.
pub const WORK_ENVELOPE_SCHEMA_VERSION: u32 = 1;

/// A stage in the reflexive work graph. Open vocabulary: any consumer may
/// introduce a new stage without touching this module.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StageId(String);

impl StageId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for StageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Registered stage names for the stages that exist today. These are
/// constants, not an enum: future stages join the vocabulary by string.
pub const STAGE_DECODED_MEDIA: &str = "decoded_media";
pub const STAGE_MOTION: &str = "motion";
pub const STAGE_DETECTION: &str = "detection";
pub const STAGE_RECOGNITION: &str = "recognition";
pub const STAGE_CROP_EMBED: &str = "crop_embed";
pub const STAGE_CLIP_EVIDENCE: &str = "clip_evidence";
pub const STAGE_EVENT_FACT: &str = "event_fact";

/// Globally unique work identity, stable across retry of the same work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkId(Uuid);

impl WorkId {
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for WorkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Receipt identity, unique within a node. Future distributed use qualifies
/// it by node identity; this type already carries global uniqueness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiptId(Uuid);

impl ReceiptId {
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for ReceiptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Configured stream identity, independent of RTSP URL shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamId(String);

impl StreamId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stable identity for the media item under work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaItemId {
    Frame {
        segment_sequence: u64,
        frame_index: u64,
    },
    Segment {
        segment_sequence: u64,
    },
}

/// Stream-local ordering: the stream sequence plus the media sequence within
/// the stream epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkOrdering {
    pub stream_epoch: u64,
    pub stream_sequence: u64,
}

/// Normalized priority inputs for cross-stream scheduling. Higher is sooner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkPriority(pub u8);

impl WorkPriority {
    pub const BACKGROUND: WorkPriority = WorkPriority(10);
    pub const MOTION: WorkPriority = WorkPriority(100);
    pub const USER_REQUESTED: WorkPriority = WorkPriority(255);
}

/// Timeout / cancellation / late-result handling for one work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkDeadline {
    None,
    /// The result is useless after this wall-clock instant.
    Discard(DateTime<Utc>),
}

/// The stage-neutral work envelope.
#[derive(Debug, Clone)]
pub struct WorkEnvelope {
    pub work_id: WorkId,
    /// Primary provenance link when work derives from earlier work.
    pub parent_work_id: Option<WorkId>,
    /// Additional parents for aggregation stages (a track joins many
    /// detections; an episode joins many tracks). Additive; empty for 1:1
    /// stages.
    pub contributing_work_ids: Vec<WorkId>,
    pub stage: StageId,
    pub stream_id: StreamId,
    pub media_item: Option<MediaItemId>,
    pub ordering: WorkOrdering,
    /// Camera/media time when known.
    pub observed_at: Option<DateTime<Utc>>,
    /// Ingress time on this node.
    pub received_at: DateTime<Utc>,
    pub priority: WorkPriority,
    pub deadline: WorkDeadline,
    pub schema_version: u32,
}

impl WorkEnvelope {
    /// Derive one stage's work from its parent: the ONE derivation rule.
    /// Identity (stream, media item, ordering, observed time, priority,
    /// deadline, schema) is inherited; the parent becomes the provenance
    /// link; contributing parents are added via [`StageAttempt::contributing`].
    pub fn derive(&self, stage: &str) -> WorkEnvelope {
        WorkEnvelope {
            work_id: WorkId::generate(),
            parent_work_id: Some(self.work_id),
            contributing_work_ids: Vec::new(),
            stage: StageId::new(stage),
            stream_id: self.stream_id.clone(),
            media_item: self.media_item,
            ordering: self.ordering,
            observed_at: self.observed_at,
            received_at: Utc::now(),
            priority: self.priority,
            deadline: self.deadline,
            schema_version: self.schema_version,
        }
    }
}

/// The result envelope mirrors the work identity so results join back
/// without process-local state.
#[derive(Debug, Clone)]
pub struct ResultEnvelope {
    pub work_id: WorkId,
    pub parent_work_id: Option<WorkId>,
    pub contributing_work_ids: Vec<WorkId>,
    pub stage: StageId,
    pub stream_id: StreamId,
    pub media_item: Option<MediaItemId>,
    pub ordering: WorkOrdering,
    pub observed_at: Option<DateTime<Utc>>,
    pub result_schema_version: u32,
    pub receipt_id: ReceiptId,
}

/// What actually happened to one unit of work at one stage attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkDisposition {
    Completed,
    Dropped,
    Coalesced,
    Retried,
    Rejected,
}

/// Runtime/operator record of one stage attempt. Not a product-domain event.
#[derive(Debug, Clone)]
pub struct StageReceipt {
    pub receipt_id: ReceiptId,
    pub work_id: WorkId,
    pub parent_work_id: Option<WorkId>,
    pub stage: StageId,
    pub stream_id: StreamId,
    pub configured_backend: Option<String>,
    pub attempted_backend: Option<String>,
    pub active_backend: Option<String>,
    pub fallback_backend: Option<String>,
    pub selected_device: Option<String>,
    pub probe_result: Option<String>,
    pub fallback_reason: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    /// A stage may emit zero or many results for one work item.
    pub output_count: u64,
    pub disposition: WorkDisposition,
}

/// Why a result failed to join back to its work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinRejection {
    WorkIdMismatch,
    StageMismatch,
    StreamMismatch,
    MediaItemMismatch,
    OrderingMismatch,
    ObservedAtMismatch,
    ParentMismatch,
    SchemaIncompatible {
        result_schema_version: u32,
    },
    ReceiptMissing,
    /// The referenced receipt was recorded for different work.
    ReceiptMismatch,
}

/// A result joins only to the exact stream, media item, ordering, observed
/// time, and parent work of the envelope it answers — and must reference the
/// exact backend attempt via its receipt. Anything else is rejected, never
/// guessed.
pub fn validate_result_join(
    work: &WorkEnvelope,
    result: &ResultEnvelope,
    receipt: Option<&StageReceipt>,
) -> Result<(), JoinRejection> {
    if result.result_schema_version != work.schema_version {
        return Err(JoinRejection::SchemaIncompatible {
            result_schema_version: result.result_schema_version,
        });
    }
    if result.work_id != work.work_id {
        return Err(JoinRejection::WorkIdMismatch);
    }
    if result.stage != work.stage {
        return Err(JoinRejection::StageMismatch);
    }
    if result.stream_id != work.stream_id {
        return Err(JoinRejection::StreamMismatch);
    }
    if result.media_item != work.media_item {
        return Err(JoinRejection::MediaItemMismatch);
    }
    if result.ordering != work.ordering {
        return Err(JoinRejection::OrderingMismatch);
    }
    if result.observed_at != work.observed_at {
        return Err(JoinRejection::ObservedAtMismatch);
    }
    if result.parent_work_id != work.parent_work_id
        || result.contributing_work_ids != work.contributing_work_ids
    {
        return Err(JoinRejection::ParentMismatch);
    }
    let Some(receipt) = receipt else {
        return Err(JoinRejection::ReceiptMissing);
    };
    if receipt.receipt_id != result.receipt_id
        || receipt.work_id != work.work_id
        || receipt.stage != work.stage
        || receipt.stream_id != work.stream_id
    {
        return Err(JoinRejection::ReceiptMismatch);
    }
    Ok(())
}

/// Counters for one bounded stage queue. Visible in stats; never silent.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct QueueCounters {
    pub queued_total: u64,
    pub replaced_dropped_total: u64,
    pub coalesced_total: u64,
    pub rejected_closed_total: u64,
    pub current_depth: u64,
}

/// Outcome of a bounded-queue receive.
pub enum QueueRecv<T> {
    Item(T),
    Timeout,
    Closed,
}

/// A bounded queue between reflexive stages with an explicit capacity, a
/// keep-newest replacement policy, and visible counters.
pub struct BoundedStageQueue<T> {
    state: Mutex<QueueState<T>>,
    signal: Condvar,
    capacity: usize,
    counters: QueueCountersState,
}

struct QueueState<T> {
    pending: VecDeque<T>,
    closed: bool,
}

#[derive(Default)]
struct QueueCountersState {
    queued_total: AtomicU64,
    replaced_dropped_total: AtomicU64,
    coalesced_total: AtomicU64,
    rejected_closed_total: AtomicU64,
}

impl<T> BoundedStageQueue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(QueueState {
                pending: VecDeque::new(),
                closed: false,
            }),
            signal: Condvar::new(),
            capacity: capacity.max(1),
            counters: QueueCountersState::default(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Push, keeping the NEWEST work when full: the oldest pending item is
    /// returned to the caller as the replaced casualty (counted), so the
    /// caller can release its resources visibly. `Err(item)` when closed.
    pub fn push_latest(&self, item: T) -> Result<Option<T>, T> {
        let mut state = self.state.lock().expect("stage queue lock");
        if state.closed {
            self.counters
                .rejected_closed_total
                .fetch_add(1, Ordering::SeqCst);
            return Err(item);
        }
        let replaced = if state.pending.len() >= self.capacity {
            self.counters
                .replaced_dropped_total
                .fetch_add(1, Ordering::SeqCst);
            state.pending.pop_front()
        } else {
            None
        };
        state.pending.push_back(item);
        self.counters.queued_total.fetch_add(1, Ordering::SeqCst);
        drop(state);
        self.signal.notify_one();
        Ok(replaced)
    }

    pub fn recv_timeout(&self, timeout: Duration) -> QueueRecv<T> {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.state.lock().expect("stage queue lock");
        loop {
            if let Some(item) = state.pending.pop_front() {
                return QueueRecv::Item(item);
            }
            if state.closed {
                return QueueRecv::Closed;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return QueueRecv::Timeout;
            }
            let (next, wait) = self
                .signal
                .wait_timeout(state, deadline - now)
                .expect("stage queue lock");
            state = next;
            if wait.timed_out() && state.pending.is_empty() && !state.closed {
                return QueueRecv::Timeout;
            }
        }
    }

    pub fn counters(&self) -> QueueCounters {
        let state = self.state.lock().expect("stage queue lock");
        QueueCounters {
            queued_total: self.counters.queued_total.load(Ordering::SeqCst),
            replaced_dropped_total: self.counters.replaced_dropped_total.load(Ordering::SeqCst),
            coalesced_total: self.counters.coalesced_total.load(Ordering::SeqCst),
            rejected_closed_total: self.counters.rejected_closed_total.load(Ordering::SeqCst),
            current_depth: state.pending.len() as u64,
        }
    }

    pub fn close(&self) {
        let mut state = self.state.lock().expect("stage queue lock");
        state.closed = true;
        drop(state);
        self.signal.notify_all();
    }

    pub fn drain(&self) -> Vec<T> {
        let mut state = self.state.lock().expect("stage queue lock");
        state.pending.drain(..).collect()
    }
}

/// Bounded, thread-safe log of the most recent stage receipts. The runtime
/// records every stage attempt here; stats and operator surfaces render
/// FROM these recorded receipts, so a printed id always references a real
/// backend attempt.
pub struct StageReceiptLog {
    inner: Mutex<VecDeque<StageReceipt>>,
    capacity: usize,
    rejected_joins: AtomicU64,
}

impl StageReceiptLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
            rejected_joins: AtomicU64::new(0),
        }
    }

    pub fn record(&self, receipt: StageReceipt) {
        let mut inner = self.inner.lock().expect("stage receipt log lock");
        if inner.len() >= self.capacity {
            inner.pop_front();
        }
        inner.push_back(receipt);
    }

    pub fn snapshot(&self) -> Vec<StageReceipt> {
        self.inner
            .lock()
            .expect("stage receipt log lock")
            .iter()
            .cloned()
            .collect()
    }

    /// Count a result that failed `validate_result_join` — rejected work is
    /// visible, never silent.
    pub fn count_rejected_join(&self) {
        self.rejected_joins.fetch_add(1, Ordering::SeqCst);
    }

    pub fn rejected_joins(&self) -> u64 {
        self.rejected_joins.load(Ordering::SeqCst)
    }
}

/// One stage attempt, begun before the work runs and finished exactly once.
/// Owns the identity discipline end to end — derivation, attempt timing,
/// result mirroring, join validation, receipt recording, and the rendered
/// operator line — so every stage speaks identity the same way instead of
/// hand-rolling it per call site.
pub struct StageAttempt {
    work: WorkEnvelope,
    backend: Option<String>,
    started_at: DateTime<Utc>,
}

/// What `finish` records: the receipt id actually recorded, and whether the
/// mirrored result JOINED its work (a failed join is recorded Rejected and
/// must gate any downstream apply).
#[derive(Debug, Clone, Copy)]
pub struct FinishedAttempt {
    pub receipt_id: ReceiptId,
    pub joined: bool,
}

impl StageAttempt {
    /// Begin an attempt for an existing work envelope (a root item from
    /// ingress, or derived work carried across a queue).
    pub fn begin(work: WorkEnvelope) -> Self {
        Self {
            work,
            backend: None,
            started_at: Utc::now(),
        }
    }

    /// Begin an attempt for work derived from a parent stage's work.
    pub fn derive(parent: &WorkEnvelope, stage: &str) -> Self {
        Self::begin(parent.derive(stage))
    }

    /// Name an additional contributing parent (aggregation provenance).
    pub fn contributing(mut self, id: WorkId) -> Self {
        self.work.contributing_work_ids.push(id);
        self
    }

    /// Attribute the attempt to a backend, when one applies.
    pub fn backend(mut self, backend: Option<String>) -> Self {
        self.backend = backend;
        self
    }

    /// Override the attempt start (e.g. media-received time for decode).
    pub fn started_at(mut self, at: DateTime<Utc>) -> Self {
        self.started_at = at;
        self
    }

    pub fn work(&self) -> &WorkEnvelope {
        &self.work
    }

    /// Finish the attempt: mirror the result envelope from the work,
    /// validate the join, record the receipt (Rejected when the join
    /// fails — counted, never silent), and emit ONE operator line rendered
    /// from the RECORDED receipt. Returns the recorded receipt id and the
    /// join verdict; callers applying results downstream must gate on it.
    pub fn finish(
        self,
        log: &StageReceiptLog,
        disposition: WorkDisposition,
        output_count: u64,
        detail: &str,
        emit_line: &mut dyn FnMut(String),
    ) -> FinishedAttempt {
        let work = &self.work;
        let receipt = StageReceipt {
            receipt_id: ReceiptId::generate(),
            work_id: work.work_id,
            parent_work_id: work.parent_work_id,
            stage: work.stage.clone(),
            stream_id: work.stream_id.clone(),
            configured_backend: None,
            attempted_backend: self.backend.clone(),
            active_backend: self.backend.clone(),
            fallback_backend: None,
            selected_device: None,
            probe_result: None,
            fallback_reason: None,
            started_at: self.started_at,
            ended_at: Utc::now(),
            output_count,
            disposition,
        };
        let result = ResultEnvelope {
            work_id: work.work_id,
            parent_work_id: work.parent_work_id,
            contributing_work_ids: work.contributing_work_ids.clone(),
            stage: work.stage.clone(),
            stream_id: work.stream_id.clone(),
            media_item: work.media_item,
            ordering: work.ordering,
            observed_at: work.observed_at,
            result_schema_version: work.schema_version,
            receipt_id: receipt.receipt_id,
        };
        let (receipt, joined, rendered_detail) =
            match validate_result_join(work, &result, Some(&receipt)) {
                Ok(()) => (receipt, true, detail.to_string()),
                Err(rejection) => {
                    log.count_rejected_join();
                    let mut rejected = receipt;
                    rejected.disposition = WorkDisposition::Rejected;
                    (rejected, false, format!("rejected={rejection:?} {detail}"))
                }
            };
        let line = format!(
            "stage={} work_id={} parent_work_id={} stream={} ordering={}:{} outputs={} disposition={:?} receipt_id={}{}{}",
            receipt.stage,
            receipt.work_id,
            receipt
                .parent_work_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_string()),
            receipt.stream_id.as_str(),
            work.ordering.stream_epoch,
            work.ordering.stream_sequence,
            receipt.output_count,
            receipt.disposition,
            receipt.receipt_id,
            if rendered_detail.is_empty() { "" } else { " " },
            rendered_detail
        );
        let receipt_id = receipt.receipt_id;
        log.record(receipt);
        emit_line(line);
        FinishedAttempt { receipt_id, joined }
    }
}
