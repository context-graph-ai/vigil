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
use std::sync::atomic::AtomicU64;
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
    SchemaIncompatible { result_schema_version: u32 },
    ReceiptMissing,
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
    let _ = (work, result, receipt);
    unimplemented!("scaffold: result-join validation is not implemented yet")
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

// Scaffold: fields are read once the queue implementation lands.
#[allow(dead_code)]
struct QueueState<T> {
    pending: VecDeque<T>,
    closed: bool,
}

// Scaffold: counters are read once the queue implementation lands.
#[allow(dead_code)]
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
    /// caller can release its resources visibly.
    pub fn push_latest(&self, item: T) -> Result<Option<T>, T> {
        let _ = item;
        unimplemented!("scaffold: bounded stage queue push is not implemented yet")
    }

    pub fn recv_timeout(&self, timeout: Duration) -> QueueRecv<T> {
        let _ = timeout;
        let _ = (&self.state, &self.signal);
        unimplemented!("scaffold: bounded stage queue recv is not implemented yet")
    }

    pub fn counters(&self) -> QueueCounters {
        let _ = &self.counters;
        unimplemented!("scaffold: bounded stage queue counters are not implemented yet")
    }

    pub fn close(&self) {
        unimplemented!("scaffold: bounded stage queue close is not implemented yet")
    }

    pub fn drain(&self) -> Vec<T> {
        unimplemented!("scaffold: bounded stage queue drain is not implemented yet")
    }
}
