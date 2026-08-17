//! The shared acceleration probe + receipt model.
//!
//! One receipt shape serves the runtime, `vigil doctor acceleration`, health,
//! stats, and logs. Receipts are runtime/operator records, never
//! product-domain events: backend and device names live here and must not
//! become MQTT/Home Assistant semantic event fields, context-graph
//! observations, correction records, or evidence semantics.
//!
//! Every level in a receipt derives from an observed probe or attempt —
//! achieved, never configured.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::workgraph::{StreamId, WorkId};

/// The three acceleration stages this receipt model covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccelStage {
    Decode,
    Detection,
    /// The shared camera-encoder seam (`crate::encode`): USB/CSI/MJPEG
    /// producers' one H.264 encode. Kept distinct from `Decode` so an
    /// encoder's receipt is never misclassified as a decode outcome.
    Encode,
}

impl AccelStage {
    pub fn as_str(self) -> &'static str {
        match self {
            AccelStage::Decode => "decode",
            AccelStage::Detection => "detection",
            AccelStage::Encode => "encode",
        }
    }
}

/// Fixed failure vocabulary. A failure outside this set is a defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCode {
    None,
    NoDeviceVisible,
    DeviceNotMapped,
    PermissionDenied,
    MissingRuntimeDependency,
    BackendNotCompiled,
    ProbeFailed,
    UnsupportedByThisArtifact,
    UnclassifiedSelectedDecoder,
}

impl FailureCode {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureCode::None => "none",
            FailureCode::NoDeviceVisible => "no_device_visible",
            FailureCode::DeviceNotMapped => "device_not_mapped",
            FailureCode::PermissionDenied => "permission_denied",
            FailureCode::MissingRuntimeDependency => "missing_runtime_dependency",
            FailureCode::BackendNotCompiled => "backend_not_compiled",
            FailureCode::ProbeFailed => "probe_failed",
            FailureCode::UnsupportedByThisArtifact => "unsupported_by_this_artifact",
            FailureCode::UnclassifiedSelectedDecoder => "unclassified_selected_decoder",
        }
    }
}

/// What kind of evidence backs a failed check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    DevicePath,
    ProcessCredentials,
    RuntimeDependency,
    BackendProbe,
    SelectedBackend,
    UpstreamError,
}

impl EvidenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceKind::DevicePath => "device_path",
            EvidenceKind::ProcessCredentials => "process_credentials",
            EvidenceKind::RuntimeDependency => "runtime_dependency",
            EvidenceKind::BackendProbe => "backend_probe",
            EvidenceKind::SelectedBackend => "selected_backend",
            EvidenceKind::UpstreamError => "upstream_error",
        }
    }
}

/// Exactly one operator action per failed check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    NoAction,
    RunCommand,
    ServiceSnippet,
    InstallPackageProfile,
    RunHaosPrecheck,
    InstallSupportedArtifact,
    ManualActionRequired,
}

impl ActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActionKind::NoAction => "no_action",
            ActionKind::RunCommand => "run_command",
            ActionKind::ServiceSnippet => "service_snippet",
            ActionKind::InstallPackageProfile => "install_package_profile",
            ActionKind::RunHaosPrecheck => "run_haos_precheck",
            ActionKind::InstallSupportedArtifact => "install_supported_artifact",
            ActionKind::ManualActionRequired => "manual_action_required",
        }
    }
}

/// Whether the probed path is live, still being prepared, fell back, or was
/// skipped by intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStatus {
    Active,
    /// Work that has neither completed nor reported an error is still under
    /// way. It is not a fallback the machine settled for and not a failure:
    /// the processor is running detection meanwhile, and only the
    /// preparation's own outcome moves it off this.
    Preparing,
    Fallback,
    Disabled,
}

impl ProbeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeStatus::Active => "active",
            ProbeStatus::Preparing => "preparing",
            ProbeStatus::Fallback => "fallback",
            ProbeStatus::Disabled => "disabled",
        }
    }
}

/// The field naming when a preparation began, so a surface can say how long it
/// has been going. Read once, at the start; never compared against anything.
pub const PREPARING_SINCE_FIELD: &str = "preparing_since_ms";

/// The field carrying the last thing a preparation said about itself, which is
/// what tells a person waiting apart from a person stuck.
pub const LATEST_PROGRESS_FIELD: &str = "latest_progress";

/// The field naming which selection this receipt belongs to: the stream epoch
/// the session opened under. A camera that reconnects because the operator
/// named a different decode path opens a new session under a new epoch, and
/// that session's selection is its own event — the proof line for it must not
/// be swallowed as a repeat of an outcome an earlier session already reported.
pub const SELECTION_EPOCH_FIELD: &str = "selection_epoch";

/// One decode or detection acceleration attempt, fully described.
#[derive(Debug, Clone)]
pub struct AccelerationReceipt {
    pub stage: AccelStage,
    pub work_id: Option<WorkId>,
    pub parent_work_id: Option<WorkId>,
    pub stream_id: Option<StreamId>,
    /// `frame_id`/`segment_id` of the media item under work, when media-backed.
    pub media_item: Option<String>,
    pub configured: bool,
    pub attempted_backend: String,
    pub active_backend: String,
    pub hardware_accelerated: bool,
    pub selected_device: Option<String>,
    pub codec: Option<String>,
    /// Detector model identity; `None` for decode receipts.
    pub model_id: Option<String>,
    pub model_version: Option<String>,
    /// Detector/model input shape; `None` for decode receipts.
    pub input_shape: Option<String>,
    pub probe_status: ProbeStatus,
    pub failure_code: FailureCode,
    pub evidence_kind: Option<EvidenceKind>,
    pub evidence_fields: BTreeMap<String, String>,
    pub action_kind: ActionKind,
    pub action_payload: Option<String>,
}

/// Render one receipt as the stable operator text block used by doctor and
/// logs, e.g.:
///
/// ```text
/// [decode.hardware]
/// configured: true
/// status: fallback
/// attempted_backend: ...
/// active_backend: ...
/// failure_code: permission_denied
/// evidence_kind: device_path
/// evidence_fields:
///   path: ...
/// action_kind: run_command
/// action_payload:
///   ...
/// ```
pub fn render_receipt_block(receipt: &AccelerationReceipt) -> String {
    let mut block = String::new();
    let header = match receipt.stage {
        AccelStage::Decode => "[decode.hardware]",
        AccelStage::Detection => "[detect.acceleration]",
        AccelStage::Encode => "[encode.hardware]",
    };
    block.push_str(header);
    block.push('\n');
    block.push_str(&format!("configured: {}\n", receipt.configured));
    block.push_str(&format!("status: {}\n", receipt.probe_status.as_str()));
    block.push_str(&format!(
        "attempted_backend: {}\n",
        receipt.attempted_backend
    ));
    block.push_str(&format!("active_backend: {}\n", receipt.active_backend));
    block.push_str(&format!(
        "hardware_accelerated: {}\n",
        receipt.hardware_accelerated
    ));
    if let Some(device) = &receipt.selected_device {
        block.push_str(&format!("selected_device: {device}\n"));
    }
    if let Some(codec) = &receipt.codec {
        block.push_str(&format!("codec: {codec}\n"));
    }
    if let Some(model_id) = &receipt.model_id {
        block.push_str(&format!("model_id: {model_id}\n"));
    }
    if let Some(model_version) = &receipt.model_version {
        block.push_str(&format!("model_version: {model_version}\n"));
    }
    if let Some(input_shape) = &receipt.input_shape {
        block.push_str(&format!("input_shape: {input_shape}\n"));
    }
    block.push_str(&format!(
        "failure_code: {}\n",
        receipt.failure_code.as_str()
    ));
    if let Some(evidence_kind) = receipt.evidence_kind {
        block.push_str(&format!("evidence_kind: {}\n", evidence_kind.as_str()));
    }
    if !receipt.evidence_fields.is_empty() {
        block.push_str("evidence_fields:\n");
        for (key, value) in &receipt.evidence_fields {
            block.push_str(&format!("  {key}: {value}\n"));
        }
    }
    block.push_str(&format!("action_kind: {}\n", receipt.action_kind.as_str()));
    if let Some(action_payload) = &receipt.action_payload {
        block.push_str("action_payload:\n");
        for line in action_payload.lines() {
            block.push_str(&format!("  {line}\n"));
        }
    }
    block
}

/// Health degradation derived from the latest receipts: configured-true but
/// software/CPU active is degraded acceleration, never total failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Degradation {
    pub stage: AccelStage,
    pub reason: String,
}

/// Thread-safe registry of the latest acceleration receipt per (stream,
/// stage). The runtime records; health, stats, doctor, and logs read.
#[derive(Default)]
pub struct AccelerationState {
    inner: Mutex<AccelerationStateInner>,
}

#[derive(Default)]
struct AccelerationStateInner {
    receipts: Vec<AccelerationReceipt>,
    log_budget: BTreeMap<String, u64>,
    /// The last thing a preparation said about itself, per stage. Held here
    /// rather than only on the receipt because a preparation can report
    /// progress before the receipt saying it is under way has been recorded,
    /// and a note that arrived first must not be lost.
    progress: BTreeMap<&'static str, String>,
}

impl AccelerationState {
    pub fn new() -> Self {
        Self::default()
    }

    /// One slot per (stage, stream): the latest receipt wins.
    fn slot_key(receipt: &AccelerationReceipt) -> String {
        format!(
            "{}::{}",
            receipt.stage.as_str(),
            receipt
                .stream_id
                .as_ref()
                .map(StreamId::as_str)
                .unwrap_or("")
        )
    }

    /// What a log line for this receipt would deduplicate on: the slot, the
    /// selection this receipt belongs to, and the observed outcome (status +
    /// failure + active backend).
    ///
    /// The selection is part of the key because a session is its own event. A
    /// camera reopened under a decode path the operator just named selects
    /// afresh, and that selection may land on an outcome an earlier session
    /// already reported — leaving the operator unable to tell the change they
    /// asked for from nothing happening at all. Repeats WITHIN one selection
    /// share the key and stay suppressed, which is the per-frame spam this
    /// budget exists to prevent.
    fn log_key(receipt: &AccelerationReceipt) -> String {
        format!(
            "{}::{}::{}::{}::{}::{}",
            Self::slot_key(receipt),
            receipt
                .evidence_fields
                .get(SELECTION_EPOCH_FIELD)
                .map(String::as_str)
                .unwrap_or(""),
            receipt.codec.as_deref().unwrap_or(""),
            receipt.probe_status.as_str(),
            receipt.failure_code.as_str(),
            receipt.active_backend
        )
    }

    /// Record what a preparation last said about itself.
    ///
    /// Saying something does not end a preparation and does not restart it: the
    /// note lands beside the moment it began, so a person reading the surface
    /// can tell a slow first build from a wedged one without the machine
    /// deciding on their behalf.
    pub fn record_progress(&self, stage: AccelStage, note: &str) {
        let mut inner = self.inner.lock().expect("acceleration state lock");
        inner.progress.insert(stage.as_str(), note.to_string());
        for receipt in inner.receipts.iter_mut().filter(|receipt| {
            receipt.stage == stage && receipt.probe_status == ProbeStatus::Preparing
        }) {
            receipt
                .evidence_fields
                .insert(LATEST_PROGRESS_FIELD.to_string(), note.to_string());
        }
    }

    /// Record the latest receipt for its (stream, stage) slot.
    pub fn record(&self, mut receipt: AccelerationReceipt) {
        let mut inner = self.inner.lock().expect("acceleration state lock");
        if receipt.probe_status == ProbeStatus::Preparing {
            // A note the preparation reported before this receipt existed
            // belongs on it: the order the two arrive in is a race, and losing
            // the note would leave the surface silent about work that has
            // spoken.
            if let Some(note) = inner.progress.get(receipt.stage.as_str()) {
                receipt
                    .evidence_fields
                    .insert(LATEST_PROGRESS_FIELD.to_string(), note.clone());
            }
        } else {
            // The preparation ended, so what it said on the way is no longer
            // what is going on.
            inner.progress.remove(receipt.stage.as_str());
        }
        let key = Self::slot_key(&receipt);
        let log_key = Self::log_key(&receipt);
        *inner.log_budget.entry(log_key).or_insert(0) += 1;
        if let Some(existing) = inner
            .receipts
            .iter_mut()
            .find(|existing| Self::slot_key(existing) == key)
        {
            *existing = receipt;
        } else {
            inner.receipts.push(receipt);
        }
    }

    /// The active decoder backend for one stream, from observed receipts.
    pub fn active_decoder(&self, stream_id: &StreamId) -> Option<String> {
        let inner = self.inner.lock().expect("acceleration state lock");
        inner
            .receipts
            .iter()
            .find(|receipt| {
                receipt.stage == AccelStage::Decode && receipt.stream_id.as_ref() == Some(stream_id)
            })
            .map(|receipt| receipt.active_backend.clone())
    }

    /// The active detector backend, from observed receipts.
    pub fn active_detector_backend(&self) -> Option<String> {
        let inner = self.inner.lock().expect("acceleration state lock");
        inner
            .receipts
            .iter()
            .find(|receipt| receipt.stage == AccelStage::Detection)
            .map(|receipt| receipt.active_backend.clone())
    }

    /// Degradations for the health surface: configured-but-fallback stages.
    pub fn health_degradations(&self) -> Vec<Degradation> {
        let inner = self.inner.lock().expect("acceleration state lock");
        inner
            .receipts
            .iter()
            .filter(|receipt| receipt.configured && receipt.probe_status == ProbeStatus::Fallback)
            .map(|receipt| Degradation {
                stage: receipt.stage,
                reason: format!(
                    "{} configured but {} active ({})",
                    receipt.attempted_backend,
                    receipt.active_backend,
                    receipt.failure_code.as_str()
                ),
            })
            .collect()
    }

    /// Bounded logging decision: one startup/backend-selection line per
    /// stream/codec plus bounded fallback lines per distinct reason — never
    /// per-frame spam. Logs only when this exact outcome has not been
    /// recorded for its slot before.
    pub fn should_log(&self, receipt: &AccelerationReceipt) -> bool {
        let inner = self.inner.lock().expect("acceleration state lock");
        !inner.log_budget.contains_key(&Self::log_key(receipt))
    }

    /// Latest receipts snapshot (doctor and stats rendering input).
    pub fn snapshot(&self) -> Vec<AccelerationReceipt> {
        let inner = self.inner.lock().expect("acceleration state lock");
        inner.receipts.clone()
    }
}
