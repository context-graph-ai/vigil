//! Fabric integration (criteria C3–C8): embeds contextdb-server's shared
//! work ledger + iroh transport so this vigil node can submit its own
//! `vigil.detector` jobs onto the shared ledger AND/OR claim + execute jobs
//! submitted by other enrolled nodes — the SAME binary, no bespoke protocol
//! (placement rule; architecture Section 3 / Rule 5). Behind the
//! default-off `fabric` feature: a `vigil` build without it links no
//! sync/iroh/ledger code at all (today's behavior, byte-identical).
//!
//! This module is SKELETON ONLY — every function body is `todo!()` pending
//! the implementation pass. Signatures are fixed here so the RED tests in
//! this same commit batch (a separate author, per the test wall) can
//! compile and fail at runtime, never at the type checker.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use contextdb_engine::Database;
use contextdb_engine::work_ledger::{ExecutionInputs, JobSnapshot};
use contextdb_server::work_ledger::{ExecutionVerdict, WorkExecutor};
use contextdb_server::{FabricIdentity, SyncClient};

use crate::DecodedRgbFrame;
use crate::detector_workclass::{DetectorDetection, WireVideoCodec, WireWorkEnvelope};
use crate::offload_policy::RemoteCapability;

/// The pluggable local-detection seam a fabric worker's job executor calls
/// after resolving + decoding a `vigil.detector` job's frames.
///
/// Distinct from the process-local `crate::detector::Detector` trait (which
/// is keyed on the crate-private `DecodedVideoSegment` type) so a worker's
/// real production backend AND a test double can both implement this
/// without reaching into pipeline-internal types: the production adapter
/// (wrapping `crate::detector::PromotableDetector`) lives inside this crate;
/// a test's spy implements this trait directly from an external test crate.
pub trait FabricDetectorBackend: Send + Sync {
    /// Truthful backend tag this node advertises, e.g. `burn-cpu`/
    /// `burn-wgpu` — matching this node's OWN promotion state, never a
    /// speed claim (criterion C3/C7).
    fn backend_tag(&self) -> &str;

    /// Content hash of the loaded model artifact (provenance).
    fn model_sha256(&self) -> &str;

    /// Run detection over already-decoded RGB frames sampled from the job's
    /// clip.
    fn detect(
        &self,
        frames: &[DecodedRgbFrame],
        clip_sha256: &str,
        sample_frames: usize,
        confidence_threshold: f64,
    ) -> Result<Vec<DetectorDetection>, String>;
}

/// Registered `work_capabilities` id for one backend, e.g.
/// `vigil-detector-burn-cpu` — a distinct id per backend so a late
/// promotion (CPU → wgpu) advertises a new truthful row rather than
/// mutating an existing one (capability rows are write-once; see the
/// working doc's Task-0 verdict — `capability_id` is cosmetic to matching,
/// so this module never depends on it being read back, only on the tags).
pub fn detector_capability_id(backend_tag: &str) -> String {
    format!("vigil-detector-{backend_tag}")
}

/// The contextdb `WorkExecutor` for work class `vigil.detector` (criterion
/// C3). A claimed job's two inputs arrive (in `job.input_refs` order,
/// already resolved by the worker loop's `WorkerConfig.blob_service` before
/// this trait is invoked): a ledger-carried JSON metadata chunk (the
/// `DetectorJob` payload minus its blob reference) and the `blob_ref`
/// length-framed `encoded_units` chunk. `execute` decodes the units, runs
/// `backend.detect`, and returns the `DetectorResult` wire payload as the
/// job's output bytes.
pub struct DetectorWorkExecutor<B: FabricDetectorBackend> {
    backend: Arc<B>,
    node_id: String,
}

impl<B: FabricDetectorBackend> DetectorWorkExecutor<B> {
    pub fn new(backend: Arc<B>, node_id: String) -> Self {
        Self { backend, node_id }
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }
}

impl<B: FabricDetectorBackend> WorkExecutor for DetectorWorkExecutor<B> {
    fn execute(
        &self,
        _job: &JobSnapshot,
        _inputs: &ExecutionInputs,
        _should_abandon: &dyn Fn() -> bool,
    ) -> ExecutionVerdict {
        todo!(
            "decode the ledger-carried metadata chunk + the resolved blob_ref \
             encoded_units chunk, run backend.detect, and record a DetectorResult \
             exactly once (criterion C3)"
        )
    }
}

/// One node's fabric wiring: its ledger-backed `Database`, its `SyncClient`
/// to the (possibly self-embedded — Design decision E) hub, and its fabric
/// identity. `None` ticket + `fabric_hub == false` is today's unenrolled
/// default; the runtime still stands up so this node's OWN jobs can be
/// submitted and claimed locally once a peer joins (criterion C8
/// symmetry).
pub struct FabricRuntime {
    pub db: Arc<Database>,
    pub client: Arc<SyncClient>,
    pub identity: FabricIdentity,
    pub node_id: String,
    /// `Some` only when this node carries the hub (`fabric_hub == true`) —
    /// Design decision E: the hub is embedded in the main vigil process
    /// (`SyncServer::with_transport` enables relay internally), never a
    /// separate hub process.
    pub hub_endpoint: Option<Arc<contextdb_server::transport::iroh::IrohServer>>,
}

impl FabricRuntime {
    /// Bring up this node's fabric wiring: open the ledger database, embed
    /// or dial the hub per `fabric_hub`, and enroll via `ticket` if given.
    /// A malformed ticket returns a typed error naming the fix (criterion
    /// C6) — this function itself never panics or hangs; enrollment
    /// failure never prevents the node from standing up standalone.
    pub async fn start(
        _data_dir: &Path,
        _fabric_ticket: Option<&str>,
        _fabric_hub: bool,
    ) -> Result<Self, String> {
        todo!("fabric bring-up: identity, hub embed/dial, ticket enrollment (C6)")
    }

    /// The ready-to-use join instruction this node's own status/doctor/log
    /// surfaces print (criterion C6): when this node carries the hub
    /// (`hub_endpoint.is_some()`), the current ticket plus the exact
    /// command a second machine runs to join
    /// (`fabric-join ticket=<current> command=<...>`); when it does not,
    /// the one-line instruction naming how to grow this node into a join
    /// point (enable the hub role) — so a lone, unenrolled node's own
    /// output still teaches the join path before anyone has joined it.
    pub fn join_instruction(&self) -> String {
        todo!(
            "render `fabric-join ticket=<current> command=<...>` when hub_endpoint is Some, \
             else the one-line hub-off grow instruction (C6)"
        )
    }

    /// Validate a fabric ticket an operator supplied (config/env/CLI/HAOS
    /// options), before ever dialing it. A malformed or expired ticket
    /// returns a typed error whose text NAMES THE FIX (criterion C6 /
    /// USR-2) — never a panic, never a hang; the caller continues
    /// standalone.
    pub fn validate_fabric_ticket(_ticket: &str) -> Result<(), String> {
        todo!("validate the ticket shape; error text must name the fix (C6)")
    }

    /// Submit one segment as a `vigil.detector` job onto the shared ledger
    /// (criteria C1/C2 offload leg): ingests the length-framed
    /// `encoded_units` as a blob, submits the job with the ledger-carried
    /// metadata chunk plus the `blob_ref` input, and returns the minted job
    /// id.
    #[allow(clippy::too_many_arguments)]
    pub async fn submit_detector_job(
        &self,
        _envelope: WireWorkEnvelope,
        _codec: WireVideoCodec,
        _fps: f64,
        _sample_frames: usize,
        _confidence_threshold: f64,
        _clip_sha256: String,
        _decoded_frames_sha256: String,
        _model_id: String,
        _encoded_units: &[Vec<u8>],
        _deadline_ms: Option<i64>,
    ) -> Result<String, String> {
        todo!("submit the DetectorJob onto the shared ledger (C1/C2)")
    }

    /// Currently-known remote `vigil.detector` capabilities (criterion C2's
    /// `RemoteCapability` input, and criterion C7 provenance): every
    /// advertised `vigil-detector-*` capability row from a node other than
    /// this one.
    pub async fn remote_detector_capabilities(&self) -> Result<Vec<RemoteCapability>, String> {
        todo!(
            "read work_capabilities for class:vigil.detector rows from other \
             nodes, mapped to RemoteCapability (C2/C7)"
        )
    }

    /// Poll for and apply any `vigil.detector` results this node submitted
    /// (criterion C4): validates the result join through the existing
    /// class-map/NMS authority, applies exactly once, and discards a
    /// late/duplicate result with a receipt — never double-counted. Returns
    /// the count of results applied this pass.
    pub async fn poll_detector_results(&self) -> Result<usize, String> {
        todo!("drain + apply DetectorResult rows via validate_result_join (C4)")
    }

    /// Spawn this node's standing worker loop (criteria C3/C5/C8): advertise
    /// `vigil-detector-<backend>`, then claim + execute matching jobs
    /// (including this node's own deferred submissions, once their deadline
    /// passes) until `shutdown` flips.
    pub fn spawn_worker_loop<B: FabricDetectorBackend + 'static>(
        &self,
        _backend: Arc<B>,
        _shutdown: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        todo!(
            "run_worker_loop(WorkerConfig{{ blob_service: Some(..), \
             defer_own_submissions_until_deadline: true, .. }}, \
             DetectorWorkExecutor::new(backend, node_id)) (C3/C5/C8)"
        )
    }
}

/// Result-join authority for offloaded detector work (criterion C4): remote
/// results join back to stream/frame identity through the existing typed
/// envelope vocabulary (`crate::workgraph::validate_result_join`); class-map
/// authority and NMS ownership stay vigil-side and identical for local and
/// remote results. Tracks, per submitted `vigil.detector` job id, the
/// pending work envelope and whether a LOCAL fallback (criterion C5) has
/// already applied the same work — so a remote result racing a local
/// fallback (or a duplicate remote result) is discarded, never
/// double-counted.
pub struct PendingOffloads {
    entries: std::sync::Mutex<std::collections::HashMap<String, PendingOffload>>,
}

struct PendingOffload {
    work: crate::workgraph::WorkEnvelope,
    resolved: bool,
}

/// What happened when a remote `vigil.detector` result was offered against
/// a pending offload.
#[derive(Debug)]
pub enum RemoteResultOutcome {
    /// The result joined its work and was applied (exactly once).
    Applied,
    /// The result failed `validate_result_join` — never applied, counted on
    /// the shared `StageReceiptLog` via `count_rejected_join`, no events.
    RejectedJoin(crate::workgraph::JoinRejection),
    /// The work this result answers was already resolved (a local fallback
    /// applied first, or an earlier remote result already landed) —
    /// discarded with a `provenance=discarded-late` receipt, never
    /// double-counted.
    DiscardedLate,
}

impl Default for PendingOffloads {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingOffloads {
    pub fn new() -> Self {
        Self {
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Record that `job_id` was offloaded for `work`, pending either a
    /// remote result or a local fallback (criterion C5).
    pub fn track(&self, job_id: String, work: crate::workgraph::WorkEnvelope) {
        self.entries.lock().expect("pending offloads lock").insert(
            job_id,
            PendingOffload {
                work,
                resolved: false,
            },
        );
    }

    /// Mark `job_id` resolved because a local fallback ran the detection
    /// itself (criterion C5) — any later-arriving remote result for the
    /// same job must discard as late, never double-count.
    pub fn mark_resolved_by_fallback(&self, _job_id: &str) {
        todo!("mark the pending offload resolved so a late remote result discards (C4/C5)")
    }

    /// Offer a remote `vigil.detector` result for `job_id`. Validates the
    /// join via `crate::workgraph::validate_result_join`, applies exactly
    /// once, and discards late/duplicate results — never double-counted
    /// (criterion C4).
    pub fn apply_remote_result(
        &self,
        _job_id: &str,
        _result: &crate::workgraph::ResultEnvelope,
        _receipt: &crate::workgraph::StageReceipt,
        _log: &crate::workgraph::StageReceiptLog,
    ) -> RemoteResultOutcome {
        todo!(
            "validate_result_join against the tracked pending work, apply exactly once, \
             and discard a late/duplicate result as provenance=discarded-late (C4)"
        )
    }
}
