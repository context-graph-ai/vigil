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

/// The two acceleration stages this receipt model covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccelStage {
    Decode,
    Detection,
}

impl AccelStage {
    pub fn as_str(self) -> &'static str {
        match self {
            AccelStage::Decode => "decode",
            AccelStage::Detection => "detection",
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

/// Whether the probed path is live, fell back, or was skipped by intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStatus {
    Active,
    Fallback,
    Disabled,
}

impl ProbeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeStatus::Active => "active",
            ProbeStatus::Fallback => "fallback",
            ProbeStatus::Disabled => "disabled",
        }
    }
}

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
    let _ = receipt;
    unimplemented!("scaffold: receipt rendering is not implemented yet")
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

// Scaffold: fields are read once the state implementation lands.
#[allow(dead_code)]
#[derive(Default)]
struct AccelerationStateInner {
    receipts: Vec<AccelerationReceipt>,
    log_budget: BTreeMap<String, u64>,
}

impl AccelerationState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the latest receipt for its (stream, stage) slot.
    pub fn record(&self, receipt: AccelerationReceipt) {
        let _ = receipt;
        let _ = &self.inner;
        unimplemented!("scaffold: acceleration state recording is not implemented yet")
    }

    /// The active decoder backend for one stream, from observed receipts.
    pub fn active_decoder(&self, stream_id: &StreamId) -> Option<String> {
        let _ = stream_id;
        unimplemented!("scaffold: active decoder lookup is not implemented yet")
    }

    /// The active detector backend, from observed receipts.
    pub fn active_detector_backend(&self) -> Option<String> {
        unimplemented!("scaffold: active detector backend lookup is not implemented yet")
    }

    /// Degradations for the health surface: configured-but-fallback stages.
    pub fn health_degradations(&self) -> Vec<Degradation> {
        unimplemented!("scaffold: health degradations are not implemented yet")
    }

    /// Bounded logging decision: one startup/backend-selection line per
    /// stream/codec plus bounded fallback lines per distinct reason — never
    /// per-frame spam.
    pub fn should_log(&self, receipt: &AccelerationReceipt) -> bool {
        let _ = receipt;
        unimplemented!("scaffold: bounded log budgeting is not implemented yet")
    }

    /// Latest receipts snapshot (doctor and stats rendering input).
    pub fn snapshot(&self) -> Vec<AccelerationReceipt> {
        unimplemented!("scaffold: acceleration snapshot is not implemented yet")
    }
}
