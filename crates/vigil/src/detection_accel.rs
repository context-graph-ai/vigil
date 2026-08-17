//! Detection acceleration selection seam.
//!
//! This is the detection-side analogue of the decode selection seam
//! (`decode::select_decode_backend`): the selector is public, takes operator
//! intent directly (no host-facts probing object), and returns the selected
//! backend plus the receipt the operator surfaces render. Every receipt
//! field is derived from an observed probe or the compiled-backend set,
//! never echoed from the `accelerated_detection` intent boolean.

#[cfg(feature = "detect-burn-wgpu")]
use crate::acceleration::AccelerationState;
use crate::acceleration::{
    AccelStage, AccelerationReceipt, ActionKind, EvidenceKind, FailureCode, ProbeStatus,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(feature = "detect-burn-wgpu")]
use std::sync::OnceLock;
#[cfg(feature = "detect-burn-wgpu")]
use std::sync::{Arc, Mutex, mpsc};
#[cfg(feature = "detect-burn-wgpu")]
use std::thread;

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

    /// The exact detector instance this probe's forward pass ran on, handed
    /// over so the preparation installs THAT object rather than cold-loading a
    /// second one behind it.
    ///
    /// The instance travels as an opaque box because the detector seam is
    /// crate-private while this trait is the public startup door;
    /// [`forward_tested_detector`] is the one place it is recovered. A probe
    /// that proves a device without building the node's own detector carries
    /// nothing, and the preparation then builds through the seam each handle
    /// registered, exactly as it always has.
    fn take_forward_tested(&mut self) -> Option<Box<dyn std::any::Any + Send>> {
        None
    }
}

/// Carry a forward-tested detector out of a probe.
#[cfg(feature = "detect-burn-wgpu")]
pub(crate) fn forward_tested_payload(
    detector: Box<dyn crate::detector::Detector>,
) -> Box<dyn std::any::Any + Send> {
    Box::new(detector)
}

/// Recover a forward-tested detector a probe carried out. Anything else a
/// caller put in the box is not a detector this process can install, so it is
/// dropped rather than guessed at.
#[cfg(feature = "detect-burn-wgpu")]
pub(crate) fn forward_tested_detector(
    payload: Box<dyn std::any::Any + Send>,
) -> Option<Box<dyn crate::detector::Detector>> {
    payload
        .downcast::<Box<dyn crate::detector::Detector>>()
        .ok()
        .map(|boxed| *boxed)
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
static DETECTOR_MODEL_PATH: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Registers the model path every detector this process builds loads from.
///
/// Called twice on a store-backed start, and the SECOND call is the one that
/// matters: the first carries what the loader merged before the store could be
/// read, and the second carries what the store resolved. Both run before any
/// camera thread exists, so nothing is loading a model while this changes, and
/// the store is the authority for which model this node runs.
pub(crate) fn register_detector_model_path(path: Option<PathBuf>) {
    if let Ok(mut registered) = DETECTOR_MODEL_PATH.lock() {
        *registered = path;
    }
}

#[cfg(feature = "detect-burn-wgpu")]
fn registered_detector_model_path() -> Option<PathBuf> {
    DETECTOR_MODEL_PATH
        .lock()
        .ok()
        .and_then(|path| path.clone())
}

/// The classes every detector this process builds looks for, registered beside
/// the model path and for the same reason: the probe's detector is the one the
/// cameras run, so it has to be built from the same class list. A probe that
/// proved a person-only detector and handed it to a node whose operator
/// widened the classes would leave those cameras detecting less than they were
/// told they would.
static DETECTOR_CLASS_INDICES: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// Registers the class list every detector this process builds looks for.
/// Called wherever the model path is, on the same schedule and for the same
/// reason.
pub(crate) fn register_detector_class_indices(class_indices: &[usize]) {
    if let Ok(mut registered) = DETECTOR_CLASS_INDICES.lock() {
        *registered = class_indices.to_vec();
    }
}

#[cfg(feature = "detect-burn-wgpu")]
fn registered_detector_class_indices() -> Vec<usize> {
    DETECTOR_CLASS_INDICES
        .lock()
        .map(|classes| classes.clone())
        .unwrap_or_default()
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
/// the preparation reported an error.
///
/// The build carries the backend, so the action never claims it is missing —
/// and the step it names has to be one that can change the result. Nothing was
/// decided by a wait here: the preparation ended on an error it observed, so
/// offering to lengthen a wait would send an operator to do something that
/// cannot help. What can help is checking that the graphics device is usable
/// by this container, and then asking for the backend again.
#[cfg(feature = "detect-burn-wgpu")]
fn probe_failed_fallback_action() -> String {
    "accelerated detection is compiled in but the preparation reported an error; verify the GPU \
     is usable by this container (for example /dev/dri mapped with render-group access), then \
     ask for the accelerated backend again; CPU detection keeps running meanwhile."
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

/// The detection backend this node runs when no probe is involved at all — the
/// automation is off and nobody has pinned the accelerated path — or nothing
/// while the answer still depends on a probe.
///
/// With the automation off the operator holds the wheel, so their pin decides
/// which path is attempted. A pin naming the accelerated backend still goes
/// through the probe: hardware is entered off a probe that passes on the
/// machine in front of you, never off a stored name.
pub fn settled_detection_backend(accelerated_detection: bool) -> Option<&'static str> {
    let pinned_accelerated = crate::settings_application::pinned_backend(
        crate::settings_backends::DETECTION_BACKEND_SETTING,
    )
    .is_some_and(|pin| pin == ACCELERATED_DETECTION_BACKEND);
    (!accelerated_detection && !pinned_accelerated).then_some(CPU_DETECTION_BACKEND)
}

pub fn select_detection_acceleration_with_probe<P: DetectionForwardProbe>(
    accelerated_detection: bool,
    model_id: &str,
    input_shape: &str,
    probe: P,
) -> DetectionAccelerationSelection {
    let selection = select_detection_backend(accelerated_detection, model_id, input_shape, probe);
    // This is where the detection backend is decided, so this is where the node
    // reports which one it is running. A name in the store is a request; what
    // came out of the selection is the answer.
    crate::settings_application::bring_into_force(
        crate::settings_backends::DETECTION_BACKEND_SETTING,
        crate::settings_model::SettingValue::text(selection.backend.clone()),
    );
    selection
}

/// Building one detector under a named backend, which is the physical work a
/// preparation does.
pub(crate) type DetectorBuild<'a> =
    &'a dyn Fn(&str) -> Result<Box<dyn crate::detector::Detector>, String>;

/// What a preparation running off every control path produced: the backend
/// this node can actually enter, its receipt, and — when the accelerated
/// backend was entered — the exact detector instance that was forward-tested.
pub(crate) struct PreparedDetectionBackend {
    pub(crate) selection: DetectionAccelerationSelection,
    /// The instance the forward test ran on. It is installed as it is; nothing
    /// loads the model a second time to put something else in its place.
    pub(crate) forward_tested: Option<Box<dyn crate::detector::Detector>>,
}

/// Prepare the detection backend this node can enter, building the detector
/// ONCE.
///
/// No deadline anywhere: nothing is waiting on this, so nothing may cut it
/// short. A slow first shader compile on hardware that can genuinely do the
/// work finishes and is entered, instead of being read as a failure because it
/// took a while. Only a real outcome decides anything here.
///
/// The accelerated path is entered on evidence from THIS machine, and the
/// evidence is a forward pass through the very detector that will run the
/// cameras — not a second one built to be thrown away. When that detector
/// cannot be built, or cannot complete a forward pass, the processor detector
/// is prepared instead and the receipt says why; that is the one case where a
/// second load happens, and it is the case where the first instance is unusable.
#[cfg(feature = "detect-burn-wgpu")]
pub(crate) fn prepare_detection_backend(
    accelerated_detection: bool,
    model_id: &str,
    input_shape: &str,
    build: DetectorBuild<'_>,
    already_prepared: &dyn Fn(&str) -> bool,
) -> PreparedDetectionBackend {
    // A preparation publishes nothing. What a selection decided is not what this
    // node is running: the detectors are still on the backend they booted with
    // until a handle genuinely takes the new one on, and the node's running
    // claim moves with that act — never one call ahead of it.
    match settled_detection_backend(accelerated_detection) {
        Some(_) => {
            let mut receipt = base_detection_receipt(model_id, input_shape, false);
            receipt.probe_status = ProbeStatus::Disabled;
            PreparedDetectionBackend {
                selection: DetectionAccelerationSelection {
                    backend: CPU_DETECTION_BACKEND.to_string(),
                    receipt,
                },
                forward_tested: None,
            }
        }
        None => enter_accelerated_or_say_why(model_id, input_shape, build, already_prepared),
    }
}

/// Without an accelerated detector in this artifact there is no device to
/// enter and nothing to forward-test: the processor is the answer, and the
/// ordinary selection is what says so.
#[cfg(not(feature = "detect-burn-wgpu"))]
pub(crate) fn prepare_detection_backend(
    accelerated_detection: bool,
    model_id: &str,
    input_shape: &str,
    build: DetectorBuild<'_>,
    already_prepared: &dyn Fn(&str) -> bool,
) -> PreparedDetectionBackend {
    let _ = (build, already_prepared);
    PreparedDetectionBackend {
        // The selection without its publish: a preparation says what backend
        // this node CAN enter, and the node names it as running only once a
        // handle has genuinely taken it on.
        selection: select_detection_backend(
            accelerated_detection,
            model_id,
            input_shape,
            NoopDetectionForwardProbe,
        ),
        forward_tested: None,
    }
}

/// Build the accelerated detector and prove it on this machine, or say what
/// stopped it.
#[cfg(feature = "detect-burn-wgpu")]
fn enter_accelerated_or_say_why(
    model_id: &str,
    input_shape: &str,
    build: DetectorBuild<'_>,
    already_prepared: &dyn Fn(&str) -> bool,
) -> PreparedDetectionBackend {
    let outcome = match crate::yolox_detector::find_hardware_adapter() {
        None => Err(DetectionForwardProbeOutcome::NoDeviceVisible),
        // This node has entered this backend before and the detector it proved
        // is still loaded, so coming back to it is a pointer swap: there is
        // nothing to build and nothing left to find out.
        Some(adapter) if already_prepared(ACCELERATED_DETECTION_BACKEND) => {
            let mut evidence_fields = BTreeMap::new();
            evidence_fields.insert("prepared_instance".to_string(), "retained".to_string());
            return PreparedDetectionBackend {
                selection: classify_forward_probe_selection(
                    Ok(DetectionForwardProbeOutcome::Passed {
                        selected_device: adapter.name,
                        evidence_fields,
                    }),
                    model_id,
                    input_shape,
                ),
                forward_tested: None,
            };
        }
        Some(adapter) => match build(ACCELERATED_DETECTION_BACKEND) {
            Err(error) => Err(DetectionForwardProbeOutcome::Failed { reason: error }),
            Ok(detector) => match forward_test(detector.as_ref()) {
                Err(error) => Err(DetectionForwardProbeOutcome::Failed { reason: error }),
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
                    Ok((
                        detector,
                        DetectionForwardProbeOutcome::Passed {
                            selected_device: adapter.name,
                            evidence_fields,
                        },
                    ))
                }
            },
        },
    };
    match outcome {
        Ok((detector, passed)) => {
            let selection = classify_forward_probe_selection(Ok(passed), model_id, input_shape);
            // A software rasterizer reported as a device is not acceleration,
            // so the classification downgrades it — and the instance built for
            // it is not what this node runs.
            let entered = selection.backend == ACCELERATED_DETECTION_BACKEND;
            PreparedDetectionBackend {
                selection,
                forward_tested: entered.then_some(detector),
            }
        }
        Err(refused) => PreparedDetectionBackend {
            selection: classify_forward_probe_selection(Ok(refused), model_id, input_shape),
            forward_tested: None,
        },
    }
}

/// Run one forward pass through a built detector, which is what turns "this
/// machine has a device" into "this detector runs on it".
#[cfg(feature = "detect-burn-wgpu")]
fn forward_test(
    detector: &dyn crate::detector::Detector,
) -> Result<crate::yolox_detector::DetectorOutput, String> {
    let clip = crate::yolox_detector::synthetic_probe_clip()?;
    let clip_sha256 = crate::media_pipeline::sha256_path(clip.path())?;
    let segment = crate::media_pipeline::decode_video_file(clip.path())?;
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        detector.detect_segment(&segment, clip_sha256, 1, 0.25)
    }))
    .unwrap_or_else(|_| Err("the forward pass stopped unexpectedly".to_string()))
}

fn select_detection_backend<P: DetectionForwardProbe>(
    accelerated_detection: bool,
    model_id: &str,
    input_shape: &str,
    probe: P,
) -> DetectionAccelerationSelection {
    if settled_detection_backend(accelerated_detection).is_some() {
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
        run_probe_off_the_caller(probe, model_id, input_shape)
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

/// Runs the forward probe on a spawned helper thread — never inline on the
/// caller's thread — and reports what the probe itself says. The probe ends one
/// of two ways: it reports an outcome, or it panics and is caught and
/// classified `probe_failed`. Nothing else ends it, so a slow cold start on
/// capable hardware is never written off as hardware that cannot do the job.
#[cfg(feature = "detect-burn-wgpu")]
fn run_probe_off_the_caller<P: DetectionForwardProbe>(
    mut probe: P,
    model_id: &str,
    input_shape: &str,
) -> DetectionAccelerationSelection {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe.run_forward_probe()));
        // A caller that has already gone away leaves nobody listening; the
        // send then finds no receiver and the result is dropped.
        let _ = tx.send(outcome);
    });

    let mut receipt = base_detection_receipt(model_id, input_shape, true);
    let classified = match rx.recv() {
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
        Err(_disconnected) => {
            // The helper thread went away without reporting anything, so
            // there is no outcome to report and none is invented. This is
            // the probe ending on its own account, not a wait ending it.
            receipt.failure_code = FailureCode::ProbeFailed;
            receipt.evidence_kind = Some(EvidenceKind::UpstreamError);
            receipt.evidence_fields.insert(
                "probe_error".to_string(),
                "the forward probe ended without reporting an outcome".to_string(),
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

/// Runs the forward probe away from startup and promotes the running detector
/// live when the probe passes. Startup gets its answer at once — the processor
/// is running detection and a preparation is under way — so camera startup is
/// never held up by a cold shader compile; the probe thread runs on to its REAL
/// outcome, and on a valid PASS the recorder calls `promote` (the runtime's
/// live detector swap) and, only if that swap succeeds, produces an Active
/// receipt naming the accelerated backend the workers now run. A promotion that
/// fails, or a probe that reports an error, leaves CPU standing with its honest
/// classification — `promote` is never called on a non-PASS, and no Active
/// receipt is ever produced without the swap having succeeded. Every receipt is
/// written to EVERY receipt surface: `accel` (/health, doctor) via `record`,
/// and `on_late_receipt` (the runtime's RuntimeStats sink — `vigil stats` and
/// the worker provenance read) — so no surface can disagree once the
/// preparation ends. Doctor asks for the outcome on its own thread and reports
/// what comes back.
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
    R: Fn(&AccelerationReceipt) + Send + Sync + 'static,
{
    if !accelerated_detection {
        let mut receipt = base_detection_receipt(model_id, input_shape, false);
        receipt.probe_status = ProbeStatus::Disabled;
        return DetectionAccelerationSelection {
            backend: CPU_DETECTION_BACKEND.to_string(),
            receipt,
        };
    }

    // The processor detector runs the cameras for as long as this preparation
    // does, so this node reports it as the backend it is running — recorded
    // HERE, before the probe can answer, because the surface has no other
    // source for it. Without it a node that has taken no backend on reads its
    // automatic choice against nothing and answers restart: for the whole
    // accelerated cold compile, and for ever on a machine whose probe never
    // passes, an operator is told to power-cycle a node to reach the very
    // backend already feeding its frames. A probe that passes publishes the
    // accelerated value through the one preparation path, exactly as a
    // command's move does. This is the same act the path without an
    // accelerated backend performs in `select_detection_acceleration_with_probe`.
    //
    // Only where nothing has been recorded yet, because this says what a node
    // with no answer runs and nothing more: a node with several cameras spawns
    // a probe per camera, and one that has already been promoted onto the
    // accelerated backend is running it — writing the processor over that
    // would report a backend no frame goes through.
    if crate::settings_application::in_force(crate::settings_backends::DETECTION_BACKEND_SETTING)
        .is_none()
    {
        crate::settings_application::bring_into_force(
            crate::settings_backends::DETECTION_BACKEND_SETTING,
            crate::settings_model::SettingValue::text(CPU_DETECTION_BACKEND),
        );
    }
    // The request this startup preparation belongs to, taken HERE, where the
    // preparation starts. A command issued while the probe runs is a newer
    // request and owns the node; this preparation then installs nothing. Read
    // on completion instead, it would ask whether anyone is standing there
    // rather than whether this is still the move being waited for.
    let startup_version = crate::live_backends::detection_transitions().request_version();
    // From here the node has a move in flight: the probe is the preparation,
    // and it is outstanding until it answers and what it proved has been taken
    // on. Said through the same machinery a command's preparation uses, so a
    // command issued during startup attaches to this work instead of starting a
    // second physical build beside it.
    crate::live_backends::begin_probed_move();
    let (tx, rx) = mpsc::channel();
    let mut probe = probe;
    thread::spawn(move || {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe.run_forward_probe()));
        // The instance the probe proved travels with its outcome, so what the
        // cameras end up running is the object the forward pass went through
        // rather than a second one loaded behind it.
        let forward_tested =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe.take_forward_tested()))
                .unwrap_or(None);
        let _ = tx.send((outcome, forward_tested));
    });

    // ONE receipt authority for every probe timing: this recorder is the single
    // constructor-side write path. Every receipt this selection produces —
    // in-time, boot fallback, and every late terminal — flows through the SAME
    // sink (the runtime's stats surface) and the SAME acceleration state
    // (/health, doctor) with the writer path named in the log, so the surfaces
    // can never diverge and a silent outcome is structurally impossible.
    let recorder: DetectionReceiptRecorder = {
        let accel = Arc::clone(&accel);
        Arc::new(move |receipt: AccelerationReceipt, path: &str| {
            println!(
                "detection_receipt_recorded path={path} status={} backend={} failure={}",
                receipt.probe_status.as_str(),
                receipt.active_backend,
                receipt.failure_code.as_str()
            );
            on_late_receipt(&receipt);
            accel.record(receipt);
        })
    };

    // The preparation is under way; nothing waits for it. Startup gets its
    // answer now — the processor is running detection — and the preparation
    // ends the only two ways it may: it completes, or it reports an error
    // about itself. A waiter carries it to whichever of those happens and
    // records it on every surface, so an outcome is never silent, and no
    // amount of time passing produces one.
    let recorder_for_outcome = Arc::clone(&recorder);
    let model_id_owned = model_id.to_string();
    let input_shape_owned = input_shape.to_string();
    thread::spawn(move || match rx.recv() {
        Ok((outcome, forward_tested)) => {
            // The probe has answered, so this pass is the one carrying the move
            // that was owed when it was spawned.
            crate::live_backends::carry_probed_move(startup_version);
            let classified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                classify_late_outcome_and_promote(
                    outcome,
                    &model_id_owned,
                    &input_shape_owned,
                    startup_version,
                    forward_tested,
                    promote,
                )
            }));
            crate::live_backends::finish_probed_move(startup_version);
            match classified {
                Ok(receipt) => {
                    let path = if receipt.probe_status == ProbeStatus::Active {
                        "completed"
                    } else {
                        "reported_error"
                    };
                    recorder_for_outcome(receipt, path);
                }
                Err(_) => recorder_for_outcome(
                    preparation_error_receipt(
                        &model_id_owned,
                        &input_shape_owned,
                        "classifying the preparation's result stopped unexpectedly; CPU \
                         detection keeps running",
                    ),
                    "classification_stopped",
                ),
            }
        }
        Err(mpsc::RecvError) => {
            // The move this probe was carrying is over too — it stopped without
            // an answer, which is one of the two ways a preparation ends and is
            // no reason to leave the node saying work is in flight for ever.
            crate::live_backends::carry_probed_move(startup_version);
            crate::live_backends::finish_probed_move(startup_version);
            recorder_for_outcome(
                preparation_error_receipt(
                    &model_id_owned,
                    &input_shape_owned,
                    "the preparation stopped without reporting a result; CPU detection keeps \
                     running",
                ),
                "stopped_without_result",
            );
        }
    });

    let selection = preparing_selection(model_id, input_shape, &accel);
    recorder(selection.receipt.clone(), "preparing");
    selection
}

/// The answer while a preparation is under way: the processor is running
/// detection, the accelerator is being prepared, and nothing has failed. The
/// clock is read exactly here, once, so a surface can say when this began —
/// it decides nothing, and it never moves again.
#[cfg(feature = "detect-burn-wgpu")]
fn preparing_selection(
    model_id: &str,
    input_shape: &str,
    accel: &AccelerationState,
) -> DetectionAccelerationSelection {
    let _ = accel;
    let mut receipt = base_detection_receipt(model_id, input_shape, true);
    receipt.probe_status = ProbeStatus::Preparing;
    receipt.failure_code = FailureCode::None;
    receipt.evidence_kind = Some(EvidenceKind::BackendProbe);
    receipt.evidence_fields.insert(
        crate::acceleration::PREPARING_SINCE_FIELD.to_string(),
        crate::clock::PersistedClock::contextdb()
            .unix_millis()
            .to_string(),
    );
    receipt.action_kind = ActionKind::NoAction;
    receipt.action_payload = Some(
        "the accelerated detector is being prepared in the background; the processor is running \
         detection meanwhile. No action required."
            .to_string(),
    );
    DetectionAccelerationSelection {
        backend: CPU_DETECTION_BACKEND.to_string(),
        receipt,
    }
}

/// The single constructor-side write path for detection receipts: sink, then
/// acceleration state, with the writer path named in the log.
#[cfg(feature = "detect-burn-wgpu")]
type DetectionReceiptRecorder = Arc<dyn Fn(AccelerationReceipt, &str) + Send + Sync>;

/// The CPU-standing receipt for a preparation that ended without reporting an
/// outcome of its own — it stopped, or classifying its result did. Honest
/// fallback with a concrete operator action, never an accelerated claim.
#[cfg(feature = "detect-burn-wgpu")]
fn preparation_error_receipt(
    model_id: &str,
    input_shape: &str,
    action: &str,
) -> AccelerationReceipt {
    let mut receipt = base_detection_receipt(model_id, input_shape, true);
    receipt.probe_status = ProbeStatus::Fallback;
    receipt.attempted_backend = ACCELERATED_DETECTION_BACKEND.to_string();
    receipt.active_backend = CPU_DETECTION_BACKEND.to_string();
    receipt.hardware_accelerated = false;
    receipt.failure_code = FailureCode::ProbeFailed;
    receipt.action_kind = ActionKind::ManualActionRequired;
    receipt.action_payload = Some(action.to_string());
    receipt
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
    startup_version: u64,
    forward_tested: Option<Box<dyn std::any::Any + Send>>,
    promote: F,
) -> AccelerationReceipt
where
    F: FnOnce() -> Result<(), String>,
{
    let mut receipt = classify_forward_probe_selection(outcome, model_id, input_shape).receipt;
    // Only a valid-HW PASS (Active + hardware-accelerated) is promotable.
    if receipt.probe_status == ProbeStatus::Active && receipt.hardware_accelerated {
        // The move itself goes through the one preparation machinery every
        // command goes through: each registered handle builds through the seam
        // it registered, and the instance that build produced is the instance
        // installed. `promote` is the caller's own account of the move, run
        // once that move has genuinely landed.
        match crate::live_backends::install_probed_detection_backend(
            ACCELERATED_DETECTION_BACKEND,
            startup_version,
            forward_tested.and_then(forward_tested_detector),
        )
        .and_then(|()| promote())
        {
            Ok(()) => {
                // The live swap succeeded: the workers now run the accelerated
                // detector, so the Active receipt is honest — stamp the
                // late-promotion action + evidence.
                receipt.action_kind = ActionKind::NoAction;
                receipt.action_payload = Some(promoted_pass_action());
                receipt
                    .evidence_fields
                    .insert("promotion".to_string(), "live".to_string());
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

/// The action for a preparation that COMPLETED and was PROMOTED live:
/// detection moved onto the accelerated backend and is now active, with no
/// restart and nothing to turn. Never the pre-promotion wording.
#[cfg(feature = "detect-burn-wgpu")]
fn promoted_pass_action() -> String {
    "detection was promoted to the accelerated backend when its preparation completed; \
     accelerated detection is now active. No action required."
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
    /// The instance the forward pass ran through, kept so the preparation
    /// installs THAT detector instead of loading the model a second time.
    forward_tested: Arc<Mutex<Option<Box<dyn crate::detector::Detector>>>>,
}

#[cfg(feature = "detect-burn-wgpu")]
impl YoloxForwardProbe {
    pub(crate) fn new(model_id: &str, input_shape: &str) -> Self {
        Self {
            model_id: model_id.to_string(),
            input_shape: input_shape.to_string(),
            forward_tested: Arc::new(Mutex::new(None)),
        }
    }
}

#[cfg(feature = "detect-burn-wgpu")]
impl DetectionForwardProbe for YoloxForwardProbe {
    fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
        let key = (self.model_id.clone(), self.input_shape.clone());
        let model_id = self.model_id.clone();
        let input_shape = self.input_shape.clone();
        // A repeat probe for the same identity answers from the cached
        // outcome, so it proves nothing new and carries no instance; the
        // preparation then builds through each handle's own seam, as it does
        // for every other backend.
        let proved = Arc::clone(&self.forward_tested);
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
                    // The same model and the same class list every camera on
                    // this node builds its detector from, because this
                    // instance IS the one they end up running.
                    let class_indices = registered_detector_class_indices();
                    let loaded: Result<
                        crate::yolox_detector::YoloxDetector<crate::yolox_detector::AccelBackend>,
                        String,
                    > = crate::yolox_detector::load_detector(
                        model_path.as_deref(),
                        "burn-yolox-tiny-wgpu",
                        &class_indices,
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
                                    // Kept, so the detector this pass proved
                                    // is the detector the cameras run.
                                    if let Ok(mut proved) = proved.lock() {
                                        *proved = Some(Box::new(detector));
                                    }
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

    fn take_forward_tested(&mut self) -> Option<Box<dyn std::any::Any + Send>> {
        self.forward_tested
            .lock()
            .ok()
            .and_then(|mut proved| proved.take())
            .map(forward_tested_payload)
    }
}
