//! Detection acceleration selection seam.
//!
//! This is the detection-side analogue of the decode selection seam
//! (`decode::select_decode_backend`): the selector is public, takes operator
//! intent directly (no host-facts probing object), and returns the selected
//! backend plus the receipt the operator surfaces render. Every receipt
//! field is derived from an observed probe or the compiled-backend set,
//! never echoed from the `accelerated_detection` intent boolean.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
#[cfg(feature = "detect-burn-wgpu")]
use std::sync::{Arc, Mutex, mpsc};
#[cfg(feature = "detect-burn-wgpu")]
use std::thread;
#[cfg(feature = "detect-burn-wgpu")]
use std::time::Duration;

#[cfg(feature = "detect-burn-wgpu")]
use crate::acceleration::AccelerationState;
use crate::acceleration::{
    AccelStage, AccelerationReceipt, ActionKind, EvidenceKind, FailureCode, ProbeStatus,
};

pub const CPU_DETECTION_BACKEND: &str = "burn-cpu";
pub const ACCELERATED_DETECTION_BACKEND: &str = "burn-wgpu";

/// Places the accelerated detector's GPU caches under the add-on's PERSISTENT
/// data root and exports BOTH cache-location levers into vigil's OWN process
/// before wgpu initialises, so the cold-start cost is kept across restarts
/// instead of being re-paid every boot. A container has no durable HOME, so
/// without these the caches land in a container-local path that a restart
/// wipes. Two env vars are set:
///
/// - `MESA_SHADER_CACHE_DIR` → `<data>/shader-cache`: Mesa's Vulkan/SPIR-V
///   shader cache.
/// - `XDG_CACHE_HOME` → `<data>/xdg-cache`: the load-bearing lever — it holds
///   the cubecl AUTOTUNE cache (`$XDG_CACHE_HOME/cubecl`, the real ~44s → ~21s
///   warm speedup measured on the 680M; the Mesa cache alone gave none), plus
///   the gstreamer registry and the Mesa fallback path.
///
/// Called at startup (before the camera-thread probe) and from
/// `vigil doctor acceleration`, so a compile/autotune completed by either warms
/// the other and every later boot. Both directories are created if absent so
/// the detector never races an uncreated path. Returns the Mesa shader-cache
/// directory.
pub fn configure_persistent_shader_cache(data_dir: &Path) -> std::io::Result<PathBuf> {
    let shader_cache_dir = data_dir.join("shader-cache");
    let xdg_cache_dir = data_dir.join("xdg-cache");
    std::fs::create_dir_all(&shader_cache_dir)?;
    std::fs::create_dir_all(&xdg_cache_dir)?;
    // SAFETY: called once at process start (runtime startup / doctor entry),
    // before any wgpu/Vulkan initialisation or camera thread, so no concurrent
    // env access races these writes.
    unsafe {
        std::env::set_var("MESA_SHADER_CACHE_DIR", &shader_cache_dir);
        std::env::set_var("XDG_CACHE_HOME", &xdg_cache_dir);
    }
    Ok(shader_cache_dir)
}

/// The startup probe is a one-time, once-per-process attempt; a wall-clock
/// deadline bounds it so a wedged forward pass never blocks camera startup
/// forever. The default is sized to admit a real GPU's cold first-shader
/// compile: on the reference AMD 680M (RADV) an in-container cold Vulkan/SPIR-V
/// compile plus a tiny-model forward pass measured ~44.5 s, so a shorter
/// deadline would misclassify a capable-but-cold GPU as a failed probe and
/// never reach the accelerated backend. 60 s gives that cold start margin. A
/// GPU-less box never pays this wait — it classifies `no_device_visible`
/// before any compile begins (see `YoloxForwardProbe::run_forward_probe`, which
/// returns early on `find_hardware_adapter() == None`); only a box that HAS a
/// device but whose compute hangs waits out the full deadline, which is
/// bounded and acceptable for a once-per-process probe.
#[cfg(feature = "detect-burn-wgpu")]
const PROBE_DEADLINE: Duration = Duration::from_secs(60);

/// The effective forward-probe deadline. The default admits a real GPU's cold
/// start (first-time shader compile, model load); a box that needs even longer,
/// or a test that wants a short deterministic timeout, sets
/// `VIGIL_DETECTION_PROBE_DEADLINE_SECS` to override it. An unparseable value
/// falls back to the default; the timeout evidence records the effective
/// deadline either way.
#[cfg(feature = "detect-burn-wgpu")]
fn detection_probe_deadline() -> Duration {
    std::env::var("VIGIL_DETECTION_PROBE_DEADLINE_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs > 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(PROBE_DEADLINE)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectionForwardProbeOutcome {
    Passed {
        selected_device: String,
        evidence_fields: BTreeMap<String, String>,
    },
    NoDeviceVisible,
    Failed {
        reason: String,
    },
}

pub trait DetectionForwardProbe: Send + 'static {
    fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome;
}

pub struct NoopDetectionForwardProbe;

impl DetectionForwardProbe for NoopDetectionForwardProbe {
    fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
        DetectionForwardProbeOutcome::NoDeviceVisible
    }
}

#[derive(Debug, Clone)]
pub struct DetectionAccelerationSelection {
    pub backend: String,
    pub receipt: AccelerationReceipt,
}

/// The operator-configured detector model path, registered once at process
/// startup (`register_detector_model_path`, called from `runtime::run_inner`
/// before any camera thread starts). The production probe below takes no
/// path parameter — it mirrors `select_decode_backend`'s host-facts-free
/// shape — so it reads the same checkpoint the live per-camera detector
/// loads through this slot instead.
static DETECTOR_MODEL_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Registers the operator-configured detector model path once per process.
/// A later call is a no-op: the first registration (at startup, before any
/// camera thread) wins.
pub(crate) fn register_detector_model_path(path: Option<PathBuf>) {
    let _ = DETECTOR_MODEL_PATH.set(path);
}

#[cfg(feature = "detect-burn-wgpu")]
fn registered_detector_model_path() -> Option<PathBuf> {
    DETECTOR_MODEL_PATH.get().cloned().flatten()
}

/// The honest fallback action when the accelerated detector is NOT compiled
/// into this build: decode stays hardware-accelerated as configured,
/// accelerated detection is not part of this build, and CPU detection is the
/// supported path. Only ever used on the software-only artifact; a build that
/// carries the accelerated backend uses the probe-aware actions below.
#[cfg(not(feature = "detect-burn-wgpu"))]
fn cpu_detection_fallback_action() -> String {
    "accelerated detection is not in this build; hardware decode stays as configured, \
     and CPU detection is the supported path."
        .to_string()
}

/// The honest fallback action when the accelerated detector IS compiled in but
/// the startup forward probe failed (wedged, panicked, or ran past the
/// deadline). The build carries the backend, so the action names the real
/// next steps the operator can take — never claims the backend is missing.
#[cfg(feature = "detect-burn-wgpu")]
fn probe_failed_fallback_action() -> String {
    "accelerated detection is compiled in but the GPU forward probe failed; \
     raise VIGIL_DETECTION_PROBE_DEADLINE_SECS and restart to allow a slow GPU cold \
     start (first shader compile, model load), and verify the GPU is usable by this \
     container; CPU detection is the supported fallback until the probe passes."
        .to_string()
}

/// The honest fallback action when the accelerated detector IS compiled in but
/// no usable GPU was found (no device visible, or only a software Vulkan
/// adapter). The build carries the backend, so the action points at the GPU,
/// never at a missing build.
#[cfg(feature = "detect-burn-wgpu")]
fn no_usable_gpu_fallback_action() -> String {
    "accelerated detection is compiled in but no usable GPU was found; verify the GPU \
     is passed through to this container (for example /dev/dri mapped with render-group \
     access) and usable; CPU detection is the supported fallback until a usable GPU is \
     present."
        .to_string()
}

fn base_detection_receipt(
    model_id: &str,
    input_shape: &str,
    configured: bool,
) -> AccelerationReceipt {
    AccelerationReceipt {
        stage: AccelStage::Detection,
        work_id: None,
        parent_work_id: None,
        stream_id: None,
        media_item: None,
        configured,
        attempted_backend: ACCELERATED_DETECTION_BACKEND.to_string(),
        active_backend: CPU_DETECTION_BACKEND.to_string(),
        hardware_accelerated: false,
        selected_device: None,
        codec: None,
        model_id: Some(model_id.to_string()),
        model_version: None,
        input_shape: Some(input_shape.to_string()),
        probe_status: ProbeStatus::Fallback,
        failure_code: FailureCode::None,
        evidence_kind: None,
        evidence_fields: BTreeMap::new(),
        action_kind: ActionKind::NoAction,
        action_payload: None,
    }
}

#[cfg(feature = "detect-burn-wgpu")]
pub fn select_detection_acceleration(
    accelerated_detection: bool,
    model_id: &str,
    input_shape: &str,
) -> DetectionAccelerationSelection {
    select_detection_acceleration_with_probe(
        accelerated_detection,
        model_id,
        input_shape,
        YoloxForwardProbe::new(model_id, input_shape),
    )
}

#[cfg(not(feature = "detect-burn-wgpu"))]
pub fn select_detection_acceleration(
    accelerated_detection: bool,
    model_id: &str,
    input_shape: &str,
) -> DetectionAccelerationSelection {
    select_detection_acceleration_with_probe(
        accelerated_detection,
        model_id,
        input_shape,
        NoopDetectionForwardProbe,
    )
}

pub fn select_detection_acceleration_with_probe<P: DetectionForwardProbe>(
    accelerated_detection: bool,
    model_id: &str,
    input_shape: &str,
    probe: P,
) -> DetectionAccelerationSelection {
    if !accelerated_detection {
        // Intent says CPU-only: return before the probe is even constructed
        // on the calling side would touch anything — no forward probe is
        // scheduled, not even in the background.
        let mut receipt = base_detection_receipt(model_id, input_shape, false);
        receipt.probe_status = ProbeStatus::Disabled;
        return DetectionAccelerationSelection {
            backend: CPU_DETECTION_BACKEND.to_string(),
            receipt,
        };
    }

    #[cfg(feature = "detect-burn-wgpu")]
    {
        run_probe_with_deadline(probe, model_id, input_shape)
    }

    #[cfg(not(feature = "detect-burn-wgpu"))]
    {
        let _ = probe;
        // This artifact ships no accelerated detector backend: honest
        // fallback derived from the observed compiled-backend set, never
        // from the intent boolean.
        let mut receipt = base_detection_receipt(model_id, input_shape, true);
        receipt.failure_code = FailureCode::BackendNotCompiled;
        receipt.evidence_kind = Some(EvidenceKind::SelectedBackend);
        receipt.evidence_fields.insert(
            "compiled_backends".to_string(),
            CPU_DETECTION_BACKEND.to_string(),
        );
        receipt.action_kind = ActionKind::ManualActionRequired;
        receipt.action_payload = Some(cpu_detection_fallback_action());
        DetectionAccelerationSelection {
            backend: CPU_DETECTION_BACKEND.to_string(),
            receipt,
        }
    }
}

/// Runs the forward probe on a spawned helper thread bounded by a
/// result-channel `recv_timeout` — never inline on the caller's thread. A
/// wedged or panicking probe is abandoned/caught and classified
/// `probe_failed`, never left to hang or to kill the caller.
#[cfg(feature = "detect-burn-wgpu")]
fn run_probe_with_deadline<P: DetectionForwardProbe>(
    mut probe: P,
    model_id: &str,
    input_shape: &str,
) -> DetectionAccelerationSelection {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe.run_forward_probe()));
        // If the receiver already gave up on the deadline, the send just
        // finds nobody listening; the abandoned thread's result is dropped.
        let _ = tx.send(outcome);
    });

    let mut receipt = base_detection_receipt(model_id, input_shape, true);
    let probe_deadline = detection_probe_deadline();
    let classified = match rx.recv_timeout(probe_deadline) {
        Ok(Ok(DetectionForwardProbeOutcome::Passed {
            selected_device,
            evidence_fields,
        })) => {
            receipt.active_backend = ACCELERATED_DETECTION_BACKEND.to_string();
            receipt.hardware_accelerated = true;
            receipt.selected_device = Some(selected_device);
            receipt.probe_status = ProbeStatus::Active;
            receipt.failure_code = FailureCode::None;
            receipt.evidence_kind = Some(EvidenceKind::BackendProbe);
            receipt.evidence_fields = evidence_fields;
            // The seam stamps its own record of the observed outcome — a
            // passing probe always validates, regardless of whether the
            // probe implementation itself carried this field through.
            receipt
                .evidence_fields
                .insert("forward_probe".to_string(), "passed".to_string());
            if detection_hardware_claim_is_valid(&receipt) {
                ACCELERATED_DETECTION_BACKEND
            } else {
                // A probe can pass on a software Vulkan adapter (llvmpipe /
                // lavapipe and kin) that some driver stacks report as a
                // usable device. Running a rasterizer on the CPU is not
                // hardware acceleration: claiming it would be the exact lie
                // the receipt system exists to prevent, so the claim is
                // downgraded to the honest no-usable-device fallback with
                // the adapter named in evidence.
                let software_adapter = receipt.selected_device.take();
                receipt.active_backend = CPU_DETECTION_BACKEND.to_string();
                receipt.hardware_accelerated = false;
                receipt.probe_status = ProbeStatus::Fallback;
                receipt.failure_code = FailureCode::NoDeviceVisible;
                receipt.evidence_kind = Some(EvidenceKind::SelectedBackend);
                receipt.evidence_fields.remove("forward_probe");
                if let Some(adapter) = software_adapter {
                    receipt
                        .evidence_fields
                        .insert("software_adapter".to_string(), adapter);
                }
                receipt.action_kind = ActionKind::RunHaosPrecheck;
                receipt.action_payload = Some(no_usable_gpu_fallback_action());
                CPU_DETECTION_BACKEND
            }
        }
        Ok(Ok(DetectionForwardProbeOutcome::NoDeviceVisible)) => {
            receipt.failure_code = FailureCode::NoDeviceVisible;
            receipt.action_kind = ActionKind::RunHaosPrecheck;
            receipt.action_payload = Some(no_usable_gpu_fallback_action());
            CPU_DETECTION_BACKEND
        }
        Ok(Ok(DetectionForwardProbeOutcome::Failed { reason })) => {
            receipt.failure_code = FailureCode::ProbeFailed;
            receipt.evidence_kind = Some(EvidenceKind::UpstreamError);
            receipt
                .evidence_fields
                .insert("probe_error".to_string(), reason);
            receipt.action_kind = ActionKind::ManualActionRequired;
            receipt.action_payload = Some(probe_failed_fallback_action());
            CPU_DETECTION_BACKEND
        }
        Ok(Err(_panic)) => {
            receipt.failure_code = FailureCode::ProbeFailed;
            receipt.evidence_kind = Some(EvidenceKind::UpstreamError);
            receipt.evidence_fields.insert(
                "probe_error".to_string(),
                "forward probe panicked".to_string(),
            );
            receipt.action_kind = ActionKind::ManualActionRequired;
            receipt.action_payload = Some(probe_failed_fallback_action());
            CPU_DETECTION_BACKEND
        }
        Err(_deadline_or_disconnect) => {
            // Deadline exceeded (or the helper thread vanished without
            // sending): abandon the wedged thread — detached, harmless for a
            // once-per-process probe that never reruns. A timeout is its own
            // named evidence class: "no answer inside the probe window" must
            // never masquerade as "this hardware cannot accelerate", and the
            // operator lever for a slow cold start is named right here.
            receipt.failure_code = FailureCode::ProbeFailed;
            receipt.evidence_kind = Some(EvidenceKind::UpstreamError);
            receipt.evidence_fields.insert(
                "probe_error".to_string(),
                format!(
                    "timed out after {:.1}s; a slow GPU cold start (first shader compile, model load) can exceed the probe window - raise VIGIL_DETECTION_PROBE_DEADLINE_SECS and restart to retry",
                    probe_deadline.as_secs_f64()
                ),
            );
            receipt.action_kind = ActionKind::ManualActionRequired;
            receipt.action_payload = Some(probe_failed_fallback_action());
            CPU_DETECTION_BACKEND
        }
    };
    DetectionAccelerationSelection {
        backend: classified.to_string(),
        receipt,
    }
}

/// Runs the forward probe under a deadline, but NEVER abandons a slow probe,
/// and promotes the running detector live when the probe passes after the
/// startup deadline. When the deadline expires it returns the honest
/// fallback-now selection immediately (so camera startup is never blocked on a
/// cold shader compile); the probe thread runs on to its REAL outcome, and on a
/// valid late PASS the late recorder calls `promote` (the runtime's live
/// detector swap) and, only if that swap succeeds, produces an Active receipt
/// naming the accelerated backend the workers now run. A promotion that fails,
/// or a late FAIL/timeout, leaves CPU standing with its honest classification —
/// `promote` is never called on a non-PASS, and no Active receipt is ever
/// produced without the swap having succeeded. The FINAL late receipt is
/// written to EVERY receipt surface: `accel` (/health, doctor) via `record`,
/// and `on_late_receipt` (the runtime's RuntimeStats sink — `vigil stats` and
/// the worker provenance read) — so no surface can disagree after a late
/// outcome. Doctor stays on the bounded variant.
#[cfg(feature = "detect-burn-wgpu")]
pub fn spawn_detection_probe_with_promotion<P, F, R>(
    accelerated_detection: bool,
    model_id: &str,
    input_shape: &str,
    probe: P,
    accel: Arc<AccelerationState>,
    promote: F,
    on_late_receipt: R,
) -> DetectionAccelerationSelection
where
    P: DetectionForwardProbe,
    F: FnOnce() -> Result<(), String> + Send + 'static,
    R: FnOnce(&AccelerationReceipt) + Send + 'static,
{
    if !accelerated_detection {
        let mut receipt = base_detection_receipt(model_id, input_shape, false);
        receipt.probe_status = ProbeStatus::Disabled;
        return DetectionAccelerationSelection {
            backend: CPU_DETECTION_BACKEND.to_string(),
            receipt,
        };
    }

    let (tx, rx) = mpsc::channel();
    let mut probe = probe;
    thread::spawn(move || {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe.run_forward_probe()));
        let _ = tx.send(outcome);
    });

    let probe_deadline = detection_probe_deadline();
    match rx.recv_timeout(probe_deadline) {
        // In time: a valid PASS is Active NOW; the runtime loads the accelerated
        // detector directly from this selection, so no live promotion is needed
        // and `promote` is dropped unused.
        Ok(outcome) => classify_forward_probe_selection(outcome, model_id, input_shape),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The probe is still running its cold compile. Return the honest
            // fallback-now so startup proceeds, and hand the live channel to a
            // late recorder that awaits the probe's true outcome, promotes on a
            // valid PASS, and records the moved-together receipt.
            let accel_for_late = Arc::clone(&accel);
            let model_id_owned = model_id.to_string();
            let input_shape_owned = input_shape.to_string();
            thread::spawn(move || {
                if let Ok(outcome) = rx.recv() {
                    let receipt = classify_late_outcome_and_promote(
                        outcome,
                        &model_id_owned,
                        &input_shape_owned,
                        promote,
                    );
                    // The SAME final receipt reaches every surface: the stats
                    // sink first (by reference), then the acceleration state
                    // (which moves it) — /health, `vigil stats`, and the worker
                    // provenance all agree on this late outcome.
                    on_late_receipt(&receipt);
                    accel_for_late.record(receipt);
                }
            });
            timeout_fallback_selection(model_id, input_shape, probe_deadline)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // The probe thread vanished without sending (should not happen —
            // the panic is caught and still sent). No late outcome is coming;
            // `promote` is dropped without being called.
            timeout_fallback_selection(model_id, input_shape, probe_deadline)
        }
    }
}

/// Classifies a late probe outcome and, only for a valid-HW PASS, performs the
/// live detector promotion. On a successful swap the Active receipt is stamped
/// with the live-promotion action + evidence; a failed swap records the honest
/// CPU-standing fallback (never an accelerated claim the workers do not run);
/// every non-PASS outcome records its real classification and never promotes.
#[cfg(feature = "detect-burn-wgpu")]
fn classify_late_outcome_and_promote<F>(
    outcome: std::thread::Result<DetectionForwardProbeOutcome>,
    model_id: &str,
    input_shape: &str,
    promote: F,
) -> AccelerationReceipt
where
    F: FnOnce() -> Result<(), String>,
{
    let mut receipt = classify_forward_probe_selection(outcome, model_id, input_shape).receipt;
    // Only a valid-HW PASS (Active + hardware-accelerated) is promotable.
    if receipt.probe_status == ProbeStatus::Active && receipt.hardware_accelerated {
        match promote() {
            Ok(()) => {
                // The live swap succeeded: the workers now run the accelerated
                // detector, so the Active receipt is honest — stamp the
                // late-promotion action + evidence.
                receipt.action_kind = ActionKind::NoAction;
                receipt.action_payload = Some(promoted_pass_action());
                receipt
                    .evidence_fields
                    .insert("promotion".to_string(), "late".to_string());
                receipt
            }
            Err(error) => {
                // The probe passed but the live swap failed: the workers still
                // run CPU, so the receipt must NOT claim the accelerated
                // backend. Record the honest CPU-standing fallback.
                promotion_failed_receipt(model_id, input_shape, error)
            }
        }
    } else {
        receipt
    }
}

/// The immediate "fallback for now" selection returned when the deadline
/// expires while the probe keeps running: an honest probe timeout that
/// classifies fallback while the live probe continues toward a possible
/// promotion.
#[cfg(feature = "detect-burn-wgpu")]
fn timeout_fallback_selection(
    model_id: &str,
    input_shape: &str,
    probe_deadline: Duration,
) -> DetectionAccelerationSelection {
    let mut receipt = base_detection_receipt(model_id, input_shape, true);
    receipt.failure_code = FailureCode::ProbeFailed;
    receipt.evidence_kind = Some(EvidenceKind::UpstreamError);
    receipt.evidence_fields.insert(
        "probe_error".to_string(),
        format!(
            "timed out after {:.1}s; a slow GPU cold start (first shader compile, model load) can exceed the probe window - the probe keeps running and detection is promoted live if it passes",
            probe_deadline.as_secs_f64()
        ),
    );
    receipt.action_kind = ActionKind::ManualActionRequired;
    receipt.action_payload = Some(probe_failed_fallback_action());
    DetectionAccelerationSelection {
        backend: CPU_DETECTION_BACKEND.to_string(),
        receipt,
    }
}

/// The action for a probe that PASSED after the startup deadline and was
/// PROMOTED live: detection was promoted after the deadline and is now active
/// with no restart and no knob to raise. Never the pre-promotion wording.
#[cfg(feature = "detect-burn-wgpu")]
fn promoted_pass_action() -> String {
    "detection was promoted to the accelerated backend after the startup deadline; accelerated \
     detection is now active. No action required."
        .to_string()
}

/// The honest CPU-standing receipt when the probe PASSED but the live detector
/// swap failed: the workers still run CPU, so the receipt classifies fallback
/// and names the swap error — never an accelerated claim the workers do not run.
#[cfg(feature = "detect-burn-wgpu")]
fn promotion_failed_receipt(
    model_id: &str,
    input_shape: &str,
    error: String,
) -> AccelerationReceipt {
    let mut receipt = base_detection_receipt(model_id, input_shape, true);
    receipt.failure_code = FailureCode::ProbeFailed;
    receipt.evidence_kind = Some(EvidenceKind::UpstreamError);
    receipt
        .evidence_fields
        .insert("promotion_error".to_string(), error);
    receipt.action_kind = ActionKind::ManualActionRequired;
    receipt.action_payload = Some(
        "the accelerated detector passed its probe but could not be loaded for live promotion; \
         CPU detection continues. Restart to retry accelerated detection."
            .to_string(),
    );
    receipt
}

/// Classifies a completed forward-probe outcome into a detection selection,
/// shared by the in-time and the late-promotion paths. A valid-HW PASS yields
/// an Active receipt with no action (the caller stamps the promotion action on
/// the late path); every fallback outcome carries its honest classification.
#[cfg(feature = "detect-burn-wgpu")]
fn classify_forward_probe_selection(
    outcome: std::thread::Result<DetectionForwardProbeOutcome>,
    model_id: &str,
    input_shape: &str,
) -> DetectionAccelerationSelection {
    let mut receipt = base_detection_receipt(model_id, input_shape, true);
    let classified = match outcome {
        Ok(DetectionForwardProbeOutcome::Passed {
            selected_device,
            evidence_fields,
        }) => {
            receipt.active_backend = ACCELERATED_DETECTION_BACKEND.to_string();
            receipt.hardware_accelerated = true;
            receipt.selected_device = Some(selected_device);
            receipt.probe_status = ProbeStatus::Active;
            receipt.failure_code = FailureCode::None;
            receipt.evidence_kind = Some(EvidenceKind::BackendProbe);
            receipt.evidence_fields = evidence_fields;
            receipt
                .evidence_fields
                .insert("forward_probe".to_string(), "passed".to_string());
            if detection_hardware_claim_is_valid(&receipt) {
                ACCELERATED_DETECTION_BACKEND
            } else {
                // A software Vulkan adapter (llvmpipe/lavapipe): not hardware
                // acceleration, downgraded to the honest no-usable-GPU fallback.
                let software_adapter = receipt.selected_device.take();
                receipt.active_backend = CPU_DETECTION_BACKEND.to_string();
                receipt.hardware_accelerated = false;
                receipt.probe_status = ProbeStatus::Fallback;
                receipt.failure_code = FailureCode::NoDeviceVisible;
                receipt.evidence_kind = Some(EvidenceKind::SelectedBackend);
                receipt.evidence_fields.remove("forward_probe");
                if let Some(adapter) = software_adapter {
                    receipt
                        .evidence_fields
                        .insert("software_adapter".to_string(), adapter);
                }
                receipt.action_kind = ActionKind::RunHaosPrecheck;
                receipt.action_payload = Some(no_usable_gpu_fallback_action());
                CPU_DETECTION_BACKEND
            }
        }
        Ok(DetectionForwardProbeOutcome::NoDeviceVisible) => {
            receipt.failure_code = FailureCode::NoDeviceVisible;
            receipt.action_kind = ActionKind::RunHaosPrecheck;
            receipt.action_payload = Some(no_usable_gpu_fallback_action());
            CPU_DETECTION_BACKEND
        }
        Ok(DetectionForwardProbeOutcome::Failed { reason }) => {
            receipt.failure_code = FailureCode::ProbeFailed;
            receipt.evidence_kind = Some(EvidenceKind::UpstreamError);
            receipt
                .evidence_fields
                .insert("probe_error".to_string(), reason);
            receipt.action_kind = ActionKind::ManualActionRequired;
            receipt.action_payload = Some(probe_failed_fallback_action());
            CPU_DETECTION_BACKEND
        }
        Err(_panic) => {
            receipt.failure_code = FailureCode::ProbeFailed;
            receipt.evidence_kind = Some(EvidenceKind::UpstreamError);
            receipt.evidence_fields.insert(
                "probe_error".to_string(),
                "forward probe panicked".to_string(),
            );
            receipt.action_kind = ActionKind::ManualActionRequired;
            receipt.action_payload = Some(probe_failed_fallback_action());
            CPU_DETECTION_BACKEND
        }
    };
    DetectionAccelerationSelection {
        backend: classified.to_string(),
        receipt,
    }
}

/// Rejects a `hardware_accelerated=true` claim unless it carries a recorded
/// passing forward-probe: `Active` status, no failure, `BackendProbe`
/// evidence with an exact `forward_probe = "passed"` field, an accelerated
/// (non-CPU-fallback) `active_backend`, and a non-empty `selected_device`
/// whose lowercase form names no known software adapter. A `false` claim is
/// never second-guessed by this predicate — it asserts nothing.
pub fn detection_hardware_claim_is_valid(receipt: &AccelerationReceipt) -> bool {
    if !receipt.hardware_accelerated {
        return true;
    }
    if receipt.probe_status != ProbeStatus::Active {
        return false;
    }
    if receipt.failure_code != FailureCode::None {
        return false;
    }
    if receipt.evidence_kind != Some(EvidenceKind::BackendProbe) {
        return false;
    }
    if receipt.active_backend == CPU_DETECTION_BACKEND {
        return false;
    }
    match receipt.evidence_fields.get("forward_probe") {
        Some(value) if value == "passed" => {}
        _ => return false,
    }
    match receipt.selected_device.as_deref() {
        Some(device) if !device.trim().is_empty() => {
            let lower = device.to_ascii_lowercase();
            !["cpu", "llvmpipe", "lavapipe", "software", "swrast"]
                .iter()
                .any(|needle| lower.contains(needle))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------
// The real YOLOX forward probe (feature-gated). Backend selection and the
// forward probe are one act here: a passing outcome names the actual
// selected accelerator; a failure or absent device is classified, never
// silently swallowed.
// ---------------------------------------------------------------------

#[cfg(feature = "detect-burn-wgpu")]
type ProbeCacheKey = (String, String);

/// Process-scoped cache of the last real probe outcome, keyed by
/// (model_id, input_shape). Genuinely once-per-process: the lock is held
/// across the compute itself (never released and reacquired around it), so
/// concurrent callers for the SAME identity block on the one in-flight
/// computation instead of each racing their own real probe — the equivalent
/// of a keyed `OnceLock::get_or_init`. A different identity (only ever seen
/// in tests, e.g. an intentionally-impossible input shape) still computes
/// fresh once that lock is free.
#[cfg(feature = "detect-burn-wgpu")]
static PROCESS_PROBE_CACHE: OnceLock<Mutex<Option<(ProbeCacheKey, DetectionForwardProbeOutcome)>>> =
    OnceLock::new();

/// Counts real (non-cached) probe computations — a deterministic, observable
/// proof that concurrent callers shared one computation rather than each
/// racing their own, independent of what the computation happened to return.
#[cfg(feature = "detect-burn-wgpu")]
static PROCESS_PROBE_COMPUTATIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Test-only observability accessor: production code never needs to read
/// this counter, only prove it stayed at one shared computation.
#[cfg(all(test, feature = "detect-burn-wgpu"))]
pub(crate) fn process_probe_computation_count() -> usize {
    PROCESS_PROBE_COMPUTATIONS.load(std::sync::atomic::Ordering::SeqCst)
}

/// Returns the cached outcome for `key` if already computed; otherwise runs
/// `compute` ONCE while still holding the lock, caches it, and returns it.
/// Holding the lock across `compute` (rather than releasing it between a
/// check and a later write) is what makes this genuinely once-per-process:
/// a second caller for the same identity blocks here until the first
/// caller's real computation has both finished AND been stored.
#[cfg(feature = "detect-burn-wgpu")]
fn probe_outcome_once(
    key: ProbeCacheKey,
    compute: impl FnOnce() -> DetectionForwardProbeOutcome,
) -> DetectionForwardProbeOutcome {
    let cache = PROCESS_PROBE_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((cached_key, outcome)) = guard.as_ref()
        && *cached_key == key
    {
        return outcome.clone();
    }
    PROCESS_PROBE_COMPUTATIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let outcome = compute();
    *guard = Some((key, outcome.clone()));
    outcome
}

#[cfg(feature = "detect-burn-wgpu")]
fn classify_no_usable_device() -> DetectionForwardProbeOutcome {
    DetectionForwardProbeOutcome::NoDeviceVisible
}

#[cfg(feature = "detect-burn-wgpu")]
fn classify_forward_failure(reason: String) -> DetectionForwardProbeOutcome {
    DetectionForwardProbeOutcome::Failed { reason }
}

#[cfg(feature = "detect-burn-wgpu")]
pub(crate) struct YoloxForwardProbe {
    model_id: String,
    input_shape: String,
}

#[cfg(feature = "detect-burn-wgpu")]
impl YoloxForwardProbe {
    pub(crate) fn new(model_id: &str, input_shape: &str) -> Self {
        Self {
            model_id: model_id.to_string(),
            input_shape: input_shape.to_string(),
        }
    }
}

#[cfg(feature = "detect-burn-wgpu")]
impl DetectionForwardProbe for YoloxForwardProbe {
    fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
        let key = (self.model_id.clone(), self.input_shape.clone());
        let model_id = self.model_id.clone();
        let input_shape = self.input_shape.clone();
        probe_outcome_once(key, move || {
            if input_shape != crate::yolox_detector::MODEL_INPUT_SHAPE {
                return classify_forward_failure(format!(
                    "unsupported detector input shape {input_shape} for model {model_id}"
                ));
            }
            match crate::yolox_detector::find_hardware_adapter() {
                None => classify_no_usable_device(),
                Some(adapter) => {
                    let model_path = registered_detector_model_path();
                    let loaded: Result<
                        crate::yolox_detector::YoloxDetector<crate::yolox_detector::AccelBackend>,
                        String,
                    > = crate::yolox_detector::load_detector(
                        model_path.as_deref(),
                        "burn-yolox-tiny-wgpu",
                    );
                    match loaded {
                        Err(error) => classify_forward_failure(error),
                        Ok(detector) => match crate::yolox_detector::synthetic_probe_clip() {
                            Err(error) => classify_forward_failure(error),
                            Ok(clip) => match crate::yolox_detector::detect_frame(
                                &detector,
                                clip.path(),
                                1,
                                0.25,
                            ) {
                                Err(error) => classify_forward_failure(error),
                                Ok(output) => {
                                    let mut evidence_fields = BTreeMap::new();
                                    evidence_fields.insert(
                                        "model_forward_sha256".to_string(),
                                        output.model_forward_sha256.clone(),
                                    );
                                    evidence_fields.insert(
                                        "detector_session_id".to_string(),
                                        output.detector_session_id.clone(),
                                    );
                                    DetectionForwardProbeOutcome::Passed {
                                        selected_device: adapter.name,
                                        evidence_fields,
                                    }
                                }
                            },
                        },
                    }
                }
            }
        })
    }
}
