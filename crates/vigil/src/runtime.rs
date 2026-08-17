use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use context_graph::{
    AuditFilter, AuditTarget, CreateContext, CreateDecision, CreateEntity, CreateIntention,
    EntityPatch, EntityType, EvidenceKind, EvidenceProducer, EvidenceRef, IntentionOrigin,
    IntentionStatus, ListEntityFilter, ObservationId, RecordObservation, RetentionStatus, Store,
    start_control_listener,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config;
#[cfg(test)]
use crate::detection_accel::select_detection_acceleration;
use crate::detector::Detector;
use crate::ha_camera_registration::{generic_camera_url, register_generic_camera};
use crate::health::{CameraCondition, HealthServer, HealthState, HealthStatus};
use crate::live_read;
use crate::media_pipeline;
use crate::privilege;
use crate::runtime_stats::RuntimeStatsState;
use crate::shutdown;
use crate::store;
use crate::yolox_detector;

const DETECTOR_INPUT_WIDTH: u32 = 640;
const DETECTOR_INPUT_HEIGHT: u32 = 640;
static CAPTURE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(crate) fn run(
    args: Vec<OsString>,
    site_channel: &dyn crate::site_channel::SiteChannelFactory,
) -> ExitCode {
    match run_inner(args, site_channel) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

/// Write what this deployment's input surfaces assert into the settings store,
/// at its own scope: the file (or add-on options) surface and the startup
/// options, each authoring its OWN record. Keeping them apart is what stops a
/// config file from silently reverting a `vigil settings` change at every
/// restart, and what lets the operator surface name which surface a value came
/// from.
///
/// A failure here is reported and never fatal: the cameras are the point, and a
/// settings write that could not land is a thing to say out loud rather than a
/// reason to stop watching.
fn apply_input_surfaces(config: &crate::config::RuntimeConfig) {
    apply_surface(config, config.file_surface.as_ref());
    apply_surface(config, config.startup_surface.as_ref());
}

fn apply_surface(
    config: &crate::config::RuntimeConfig,
    assertions: Option<&crate::config::SurfaceAssertions>,
) {
    let Some(assertions) = assertions else {
        return;
    };
    // A present surface that names no setting is not the same thing as no
    // surface at all. Deleting the last key out of a config file or the add-on
    // options leaves a file that still speaks and now says nothing — which is
    // exactly what clears that surface's records, and deleting the last key has
    // to behave like deleting any other one. Only a surface that cannot clear
    // by absence (the startup options) has nothing to do when it names nothing.
    if assertions.entries.is_empty()
        && assertions.camera_entries.is_empty()
        && !assertions.surface.clears_by_absence()
    {
        return;
    }
    let store = match crate::settings_store::SettingsStore::open(&config.data_dir) {
        Ok(store) => store,
        Err(error) => {
            println!(
                "settings-surface-error surface={:?} {error}",
                assertions.surface
            );
            return;
        }
    };
    let snapshot = crate::settings_store::SurfaceSnapshot {
        surface: assertions.surface,
        scope: crate::settings_model::Scope::node(deployment_scope_name(&config.data_dir)),
        present_and_parsing: true,
        entries: assertions.entries.clone(),
        declared_defaults: Vec::new(),
    };
    report_surface_outcome(assertions.surface, store.apply_surface_snapshot(&snapshot));
    // What the same surface says about each camera it lists, authored at that
    // camera's own scope. A camera that names nothing still gets its pass, so a
    // value deleted out of one camera's row clears that camera's record and
    // leaves every other camera — and the deployment-wide value it now inherits
    // — exactly where they were.
    // A camera the surface has stopped listing altogether gets the same pass
    // with nothing to say, so removing a camera's whole row clears what that
    // row authored rather than leaving a value behind for a camera of that name
    // to inherit if it ever comes back.
    let mut camera_passes = assertions.camera_entries.clone();
    if assertions.surface.clears_by_absence() {
        match store.camera_scopes_authored_through(assertions.surface) {
            Ok(cameras) => {
                for camera in cameras {
                    if !camera_passes.iter().any(|(named, _)| named == &camera) {
                        camera_passes.push((camera, Vec::new()));
                    }
                }
            }
            Err(error) => println!(
                "settings-surface-error surface={:?} {error}",
                assertions.surface
            ),
        }
    }
    for (camera, entries) in &camera_passes {
        let camera_snapshot = crate::settings_store::SurfaceSnapshot {
            surface: assertions.surface,
            scope: crate::settings_model::Scope::camera(camera.clone()),
            present_and_parsing: true,
            entries: entries.clone(),
            declared_defaults: Vec::new(),
        };
        report_surface_outcome(
            assertions.surface,
            store.apply_camera_surface_snapshot(&camera_snapshot),
        );
    }
}

/// Say out loud what applying one surface snapshot did to the store. A value
/// the surface names and Vigil cannot take is reported per setting, never
/// silently dropped, and a store that could not be written is reported once.
fn report_surface_outcome(
    surface: crate::settings_model::Surface,
    outcome: Result<
        Vec<crate::settings_store::SurfaceChange>,
        crate::settings_model::SettingsError,
    >,
) {
    match outcome {
        Ok(changes) => {
            for change in changes {
                if let crate::settings_store::SurfaceChange::Refused { setting, error } = change {
                    println!("settings-refused setting={setting} {error}");
                }
            }
        }
        Err(error) => println!("settings-surface-error surface={surface:?} {error}"),
    }
}

/// Resolve every setting the store governs and put each where the runtime
/// reads it. Called once at startup, after the input surfaces have authored and
/// before any camera thread or detector exists, so ONE resolved value reaches
/// every consumer rather than each deriving its own.
///
/// A value is taken from the store when a real author stands behind it — a
/// person at this deployment, or a management server. Where nothing has been
/// authored, the setting reads Automatic and the loader's own default stays,
/// which is the same value Vigil's automatic floor would give: the store is the
/// authority for what someone CHOSE, and Vigil's own choice is the floor
/// beneath it either way.
///
/// A failure to read the store is reported and never fatal. The cameras are the
/// point, and a settings read that could not land is a thing to say out loud
/// rather than a reason to stop watching.
fn apply_resolved_settings(config: &mut crate::config::RuntimeConfig) {
    use crate::settings_model::{Author, SettingValue};

    let store = match crate::settings_store::SettingsStore::open(&config.data_dir) {
        Ok(store) => store,
        Err(error) => {
            println!("settings-unresolved {error}");
            return;
        }
    };
    let name = deployment_scope_name(&config.data_dir);
    let target = crate::settings_model::ScopeTarget {
        tenant: name.clone(),
        site: name.clone(),
        node: name,
        camera: None,
    };

    // The class list is resolved through the construction seam rather than the
    // raw value: the detector is built from indices, and binding the indices
    // here is what makes "what the detector emits" and "what the operator
    // chose" the same fact.
    match store.detector_class_allowlist(&target) {
        // An empty resolution is not an instruction to detect nothing: an
        // explicitly empty list is refused when it is written, so nothing valid
        // can produce one here, and Vigil's own choice keeps running.
        Ok(indices) if !indices.is_empty() => config.detector_class_indices = indices,
        Ok(_) => {}
        Err(error) => println!("settings-unresolved setting=detector_classes {error}"),
    }

    let authored = |setting: &str| -> Option<SettingValue> {
        match store.resolve(setting, &target) {
            // Automatic means nobody has chosen: the loader's default stands,
            // and overwriting it here would let the floor outrank a value that
            // reached this process through a surface that has not authored yet.
            Ok(effective) if effective.author == Author::Automatic => None,
            Ok(effective) => Some(effective.requested),
            Err(error) => {
                println!("settings-unresolved setting={setting} {error}");
                None
            }
        }
    };

    if let Some(SettingValue::Int(frames)) =
        authored(crate::settings_model::DETECTOR_SAMPLE_FRAMES_SETTING)
        && let Ok(frames) = usize::try_from(frames)
    {
        config.detector_sample_frames = frames;
    }
    if let Some(SettingValue::Int(interval)) =
        authored(crate::settings_model::DETECTOR_STATIONARY_INTERVAL_SETTING)
        && let Ok(interval) = u64::try_from(interval)
    {
        config.detector_stationary_interval_secs = interval;
    }
    if let Some(SettingValue::Float(threshold)) =
        authored(crate::settings_model::DETECTOR_CONFIDENCE_THRESHOLD_SETTING)
    {
        config.detector_confidence_threshold = threshold;
    }
    if let Some(SettingValue::Int(capacity)) =
        authored(crate::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING)
        && let Ok(capacity) = usize::try_from(capacity)
    {
        config.detector_queue_capacity = capacity;
    }
    if let Some(SettingValue::Int(initial)) =
        authored(crate::settings_model::RTSP_RETRY_INITIAL_MS_SETTING)
        && let Ok(initial) = u64::try_from(initial)
    {
        config.rtsp_retry_initial_ms = initial;
    }
    if let Some(SettingValue::Int(widest)) =
        authored(crate::settings_model::RTSP_RETRY_MAX_MS_SETTING)
        && let Ok(widest) = u64::try_from(widest)
    {
        config.rtsp_retry_max_ms = widest;
    }
    if let Some(SettingValue::Float(threshold)) =
        authored(crate::settings_model::RECOGNITION_THRESHOLD_SETTING)
    {
        config.recognition.match_threshold = threshold;
    }
    if let Some(SettingValue::Bool(allowed)) =
        authored(crate::settings_model::FABRIC_ALLOW_FRAME_OFFLOAD_SETTING)
    {
        config.fabric_allow_frame_offload = allowed;
    }
    // The three fabric values. Bring-up is spawned after this resolution
    // precisely so they can be resolved here: fabric attaches off the
    // readiness-critical path, so its configuration is taken once the store has
    // answered and an operator has nothing to wait for.
    if let Some(SettingValue::Bool(hub)) = authored(crate::settings_model::FABRIC_HUB_SETTING) {
        config.fabric_hub = hub;
    }
    if let Some(SettingValue::Int(lease)) =
        authored(crate::settings_model::FABRIC_WORKER_LEASE_MS_SETTING)
        && let Ok(lease) = u64::try_from(lease)
    {
        config.fabric_worker_lease_ms = lease;
    }
    if let Some(SettingValue::Int(horizon)) =
        authored(crate::settings_model::FABRIC_FALLBACK_HORIZON_MS_SETTING)
        && let Ok(horizon) = u64::try_from(horizon)
    {
        config.fabric_fallback_horizon_ms = horizon;
    }
    if let Some(SettingValue::Text(model)) =
        authored(crate::settings_model::DETECTOR_MODEL_ID_SETTING)
    {
        config.detector_model_id = model;
    }
    // The two domain switches are ordinary settings, resolved like any other.
    if let Some(SettingValue::Bool(on)) =
        authored(crate::settings_domains::HARDWARE_DECODING_DOMAIN)
    {
        config.hardware_decoding = on;
    }
    if let Some(SettingValue::Bool(on)) =
        authored(crate::settings_domains::ACCELERATED_DETECTION_DOMAIN)
    {
        config.accelerated_detection = on;
    }

    // The deployment's own names. Both reach real consumers already — every
    // event this node reports carries them — but they reached them through the
    // loader, which merges the command line, the environment and the options
    // file BEFORE this store is open. Resolving them here is what makes a name
    // an operator sets the name the machine uses.
    if let Some(SettingValue::Text(name)) = authored(crate::settings_model::SITE_NAME_SETTING)
        && !name.is_empty()
    {
        config.site_name = name;
    }
    let stored_camera_name = match authored(crate::settings_model::CAMERA_NAME_SETTING) {
        Some(SettingValue::Text(name)) if !name.is_empty() => Some(name),
        _ => None,
    };
    if let Some(name) = stored_camera_name.as_ref() {
        config.camera_name = name.clone();
    }
    // The two camera endpoints. Which camera this node watches, and which
    // stream a person is shown, are values an operator sets — so the store
    // outranks the file for them exactly as it does for everything else. They
    // are carried to the camera set below rather than assigned here, because
    // the camera entries are what a camera thread is actually built from.
    let stored_camera = StoredCameraIdentity {
        name: stored_camera_name,
        analysis: match authored(crate::settings_model::RTSP_URL_SETTING) {
            Some(SettingValue::Text(endpoint)) if !endpoint.is_empty() => Some(endpoint),
            _ => None,
        },
        live: match authored(crate::settings_model::LIVE_RTSP_URL_SETTING) {
            Some(SettingValue::Text(endpoint)) if !endpoint.is_empty() => Some(endpoint),
            _ => None,
        },
    };
    if let Some(SettingValue::Text(path)) =
        authored(crate::settings_model::DETECTOR_MODEL_PATH_SETTING)
        && !path.is_empty()
    {
        config.detector_model_path = Some(std::path::PathBuf::from(path));
    }
    // Which classes recognition covers. It never widens or narrows the
    // detector's own class list; it decides which of the classes vigil already
    // detects get a name put to them.
    match authored(crate::settings_model::RECOGNITION_COVERED_CLASSES_SETTING) {
        Some(SettingValue::List(classes)) if !classes.is_empty() => {
            config.recognition.covered_classes = classes;
        }
        Some(SettingValue::Text(class)) if !class.is_empty() => {
            config.recognition.covered_classes = vec![class];
        }
        _ => {}
    }
    // The motion sensitivity the gate runs at. It is read from what this
    // process has in force on every decoded segment, so it is brought into
    // force here rather than copied onto a configuration nothing reads.
    let motion_sensitivity = match authored(crate::settings_model::MOTION_SENSITIVITY_SETTING) {
        Some(SettingValue::Int(sensitivity)) => sensitivity,
        _ => crate::settings_application::AUTOMATIC_MOTION_SENSITIVITY,
    };
    crate::settings_application::bring_into_force(
        crate::settings_model::MOTION_SENSITIVITY_SETTING,
        SettingValue::Int(motion_sensitivity),
    );
    // The operator's backend pins, published for the selection seams that run
    // deep in the decode and detection paths where no configuration is
    // threaded. A pin is what the store says; what runs is what the selection
    // makes of it, and the selection is what reports the running backend.
    for setting in [
        crate::settings_backends::DETECTION_BACKEND_SETTING,
        crate::settings_backends::DECODE_BACKEND_SETTING,
    ] {
        let pin = match authored(setting) {
            Some(SettingValue::Text(backend)) if !backend.is_empty() => Some(backend),
            _ => None,
        };
        crate::settings_application::publish_pinned_backend(setting, pin);
    }
    // Where the answer no longer depends on a probe — the automation is off and
    // nobody pinned a path a probe has to open — what this node runs is settled
    // the moment the store has answered, and a node that has settled it says so
    // rather than leaving the operator's surface blank until a camera happens
    // to connect. The same rule the selection applies, asked once here.
    if let Some(backend) =
        crate::detection_accel::settled_detection_backend(config.accelerated_detection)
    {
        crate::settings_application::bring_into_force(
            crate::settings_backends::DETECTION_BACKEND_SETTING,
            SettingValue::text(backend),
        );
    }
    if let Some(backend) = crate::decode::settled_decode_backend(config.hardware_decoding) {
        crate::settings_application::bring_into_force(
            crate::settings_backends::DECODE_BACKEND_SETTING,
            SettingValue::text(backend),
        );
    }

    // What this process actually applied, recorded as it applies it, so the
    // operator surface can answer requested and running side by side without
    // either standing in for the other.
    crate::settings_projection::record_running(
        crate::settings_model::DETECTOR_SAMPLE_FRAMES_SETTING,
        SettingValue::Int(config.detector_sample_frames as i64),
    );
    crate::settings_projection::record_running(
        crate::settings_model::DETECTOR_STATIONARY_INTERVAL_SETTING,
        SettingValue::Int(config.detector_stationary_interval_secs as i64),
    );
    crate::settings_projection::record_running(
        crate::settings_model::DETECTOR_CONFIDENCE_THRESHOLD_SETTING,
        SettingValue::Float(config.detector_confidence_threshold),
    );
    crate::settings_projection::record_running(
        crate::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING,
        SettingValue::Int(config.detector_queue_capacity as i64),
    );
    // The one environment lever over a behavior setting is published in the
    // same startup act as every other running value, before the control
    // listener binds. It used to be recorded where the queue is built — on a
    // camera thread, after detector construction — so the operator surface
    // could answer `running=` with the pin and no attribution at all for as
    // long as that thread took to arrive, and forever on a node with no
    // cameras. What runs is a property of the process, not of any camera.
    if let Some(levered_capacity) = detector_queue_capacity_lever() {
        let pinned = crate::settings_application::detector_queue_capacity_in_force(
            config.detector_queue_capacity,
        );
        crate::settings_projection::record_running_from_environment(
            crate::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING,
            DETECTOR_QUEUE_CAPACITY_VARIABLE,
            SettingValue::Int(levered_capacity as i64),
            (pinned != levered_capacity).then_some(SettingValue::Int(pinned as i64)),
        );
        if pinned != levered_capacity {
            println!(
                "detector_queue_capacity_environment_override=true \
                 variable={DETECTOR_QUEUE_CAPACITY_VARIABLE} running={levered_capacity} \
                 shadowed_setting={pinned}"
            );
        }
    }
    crate::settings_projection::record_running(
        crate::settings_model::RTSP_RETRY_INITIAL_MS_SETTING,
        SettingValue::Int(config.rtsp_retry_initial_ms as i64),
    );
    crate::settings_projection::record_running(
        crate::settings_model::RTSP_RETRY_MAX_MS_SETTING,
        SettingValue::Int(config.rtsp_retry_max_ms as i64),
    );
    crate::settings_projection::record_running(
        crate::settings_model::RECOGNITION_THRESHOLD_SETTING,
        SettingValue::Float(config.recognition.match_threshold),
    );
    crate::settings_projection::record_running(
        crate::settings_model::FABRIC_ALLOW_FRAME_OFFLOAD_SETTING,
        SettingValue::Bool(config.fabric_allow_frame_offload),
    );
    crate::settings_projection::record_running(
        crate::settings_model::FABRIC_HUB_SETTING,
        SettingValue::Bool(config.fabric_hub),
    );
    crate::settings_projection::record_running(
        crate::settings_model::FABRIC_WORKER_LEASE_MS_SETTING,
        SettingValue::Int(config.fabric_worker_lease_ms as i64),
    );
    crate::settings_projection::record_running(
        crate::settings_model::FABRIC_FALLBACK_HORIZON_MS_SETTING,
        SettingValue::Int(config.fabric_fallback_horizon_ms as i64),
    );
    crate::settings_projection::record_running(
        crate::settings_model::DETECTOR_MODEL_ID_SETTING,
        SettingValue::text(config.detector_model_id.clone()),
    );
    // The decode deadlines are consumed deep in the decode path, where no
    // configuration is threaded — they are read back from what this process
    // recorded here, so one resolution reaches every site rather than each site
    // resolving its own.
    for (setting, automatic) in [
        (
            crate::settings_model::DECODE_PROBE_DEADLINE_SECS_SETTING,
            crate::settings_backends::automatic::DECODE_PROBE_DEADLINE_SECS,
        ),
        (
            crate::settings_model::HARDWARE_PROBE_DEADLINE_SECS_SETTING,
            crate::settings_backends::automatic::HARDWARE_PROBE_DEADLINE_SECS,
        ),
    ] {
        let seconds = match authored(setting) {
            Some(SettingValue::Int(secs)) => secs,
            Some(SettingValue::Float(secs)) if secs.is_finite() && secs > 0.0 => secs as i64,
            _ => automatic,
        };
        crate::settings_projection::record_running(setting, SettingValue::Int(seconds.max(1)));
    }
    crate::settings_projection::record_running(
        crate::settings_domains::HARDWARE_DECODING_DOMAIN,
        SettingValue::Bool(config.hardware_decoding),
    );
    crate::settings_projection::record_running(
        crate::settings_domains::ACCELERATED_DETECTION_DOMAIN,
        SettingValue::Bool(config.accelerated_detection),
    );
    // The camera set this run watches, derived from the values just resolved,
    // and the detection model every detector this run builds loads from.
    adopt_resolved_camera_set(config, &stored_camera);
    // Each of those cameras' own motion sensitivity, put where that camera's
    // gate reads it. A camera with no value of its own runs what it inherits.
    let watched: Vec<String> = config
        .cameras
        .iter()
        .map(|camera| camera.name.clone())
        .collect();
    crate::settings_application::adopt_camera_scoped_settings(&store, &target, &watched);
    adopt_resolved_detector_model(config);
    // The classes recognition puts a name to, where recognition reads them.
    crate::settings_application::bring_into_force(
        crate::settings_model::RECOGNITION_COVERED_CLASSES_SETTING,
        SettingValue::list(config.recognition.covered_classes.clone()),
    );
    crate::settings_application::bring_into_force(
        crate::settings_model::SITE_NAME_SETTING,
        SettingValue::text(config.site_name.clone()),
    );
    // The values the NEXT start has to read before it can open this store, put
    // where that start can read them. The store stays the truth; this is what
    // it last resolved.
    crate::settings_cache::refresh(&store, &config.data_dir, &target);
}

/// What the store authored for the single-camera shape, carried from resolution
/// to the camera set that is built out of it. Each field is present only when a
/// real author stands behind it, so a deployment that named its cameras in a
/// list keeps the names it gave them.
struct StoredCameraIdentity {
    name: Option<String>,
    analysis: Option<String>,
    live: Option<String>,
}

/// Put the resolved camera identity onto the camera this run brings up, and
/// report what it is watching.
///
/// The camera name and the two endpoints are the single-camera shape — "the
/// first camera's name", "the first camera's stream" — so a stored value lands
/// on the first camera entry, which is what a camera thread is built from. The
/// endpoints are answered for as a SOURCE and never as a value: their spelling
/// carries the camera's credentials.
fn adopt_resolved_camera_set(
    config: &mut crate::config::RuntimeConfig,
    stored: &StoredCameraIdentity,
) {
    use crate::settings_model::SettingValue;

    if let Some(endpoint) = stored.analysis.as_ref() {
        config.rtsp_url = Some(endpoint.clone());
    }
    if let Some(first) = config.cameras.first_mut() {
        if let Some(name) = stored.name.as_ref() {
            first.name = name.clone();
        }
        // Only onto a camera that is reached over a stream: naming an endpoint
        // on a USB or CSI camera would give it two sources, which is refused
        // when a camera is loaded and would be no better here.
        if first.rtsp_url.is_some() {
            if let Some(endpoint) = stored.analysis.as_ref() {
                first.rtsp_url = Some(endpoint.clone());
            }
            if let Some(endpoint) = stored.live.as_ref() {
                first.live_rtsp_url = Some(endpoint.clone());
            }
        }
    }
    let live_endpoint = config
        .cameras
        .first()
        .and_then(|camera| camera.live_rtsp_url.clone())
        .or_else(|| stored.live.clone());
    crate::settings_application::bring_into_force(
        crate::settings_model::CAMERA_NAME_SETTING,
        SettingValue::text(config.camera_name.clone()),
    );
    crate::settings_application::bring_into_force(
        crate::settings_model::RTSP_URL_SETTING,
        SettingValue::text(endpoint_source(
            stored.analysis.is_some(),
            config.rtsp_url.is_some(),
        )),
    );
    crate::settings_application::bring_into_force(
        crate::settings_model::LIVE_RTSP_URL_SETTING,
        SettingValue::text(endpoint_source(
            stored.live.is_some(),
            live_endpoint.is_some(),
        )),
    );
}

/// Where the endpoint this run took on came from — a stored record somebody
/// authored, this run's own configuration, or nowhere at all.
fn endpoint_source(from_store: bool, present: bool) -> &'static str {
    if from_store {
        crate::settings_projection::SOURCE_STORED
    } else if present {
        crate::settings_projection::SOURCE_CONFIGURED
    } else {
        crate::settings_projection::ABSENT
    }
}

/// Register the resolved detection model where every detector this run builds
/// loads it from, replacing the registration made before the store was open.
fn adopt_resolved_detector_model(config: &crate::config::RuntimeConfig) {
    crate::detection_accel::register_detector_model_path(config.detector_model_path.clone());
    crate::detection_accel::register_detector_class_indices(&config.detector_class_indices);
    crate::settings_application::bring_into_force(
        crate::settings_model::DETECTOR_MODEL_PATH_SETTING,
        crate::settings_model::SettingValue::text(
            config
                .detector_model_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        ),
    );
}

/// Build a detector under a named backend, from the model file and class list
/// this camera resolved.
///
/// One builder, two callers: the late acceleration probe promoting a running
/// detector, and an operator naming a backend while the node runs. Both load
/// through the same `yolox_detector` path initial construction uses, so a
/// replaced detector can never be built from a different model or a different
/// class list than the one it replaced.
fn detector_build_for(
    model_path: Option<std::path::PathBuf>,
    class_indices: Vec<usize>,
) -> impl Fn(&str) -> Result<Box<dyn crate::detector::Detector>, String> + Send + Sync + 'static {
    move |backend: &str| {
        if backend == crate::detection_accel::ACCELERATED_DETECTION_BACKEND {
            #[cfg(feature = "detect-burn-wgpu")]
            {
                return yolox_detector::load_accelerated_detector_with_classes(
                    model_path.as_deref(),
                    &class_indices,
                )
                .map(|detector| Box::new(detector) as Box<dyn crate::detector::Detector>);
            }
            #[cfg(not(feature = "detect-burn-wgpu"))]
            {
                // Said as what it is rather than quietly loaded on the
                // processor: an operator who named a backend this artifact does
                // not carry is owed the refusal, not a different detector.
                return Err(format!("this artifact carries no {backend} detector"));
            }
        }
        yolox_detector::load_cpu_detector_with_classes(model_path.as_deref(), &class_indices)
            .map(|detector| Box::new(detector) as Box<dyn crate::detector::Detector>)
    }
}

/// Where a detection selection receipt lands so every operator surface reads
/// the same account of which detector is running.
///
/// One sink, two callers, for the same reason the builder above has two: a
/// backend that moved after startup — late probe or operator pin — has to reach
/// `vigil stats` and the worker provenance, or those surfaces keep naming the
/// detector this run happened to construct first.
fn detection_receipt_sink(
    stats: RuntimeStatsState,
) -> impl Fn(&crate::acceleration::AccelerationReceipt) + Send + Sync + 'static {
    move |receipt: &crate::acceleration::AccelerationReceipt| {
        stats.update(|stats| {
            stats.active_detector_backend = receipt.active_backend.clone();
            stats.detection_acceleration = format!(
                "{}{}",
                receipt.probe_status.as_str(),
                match receipt.failure_code {
                    crate::acceleration::FailureCode::None => String::new(),
                    code => format!(":{}", code.as_str()),
                }
            );
            stats.detection_receipt_block = crate::acceleration::render_receipt_block(receipt);
        });
    }
}

/// What the review surface is actually answering on, recorded once it is
/// serving. A port this process never bound has nothing to report as running.
fn record_running_review_port(port: u16) {
    crate::settings_projection::record_running(
        crate::settings_model::REVIEW_PORT_SETTING,
        crate::settings_model::SettingValue::Int(i64::from(port)),
    );
}

/// Start fabric bring-up, on its own thread, with the configuration this run
/// resolved.
///
/// Fabric attaches ASYNCHRONOUSLY, off the readiness-critical path. Opening the
/// fabric ledger (Database::open) can take minutes on a large ledger, and a node
/// with a camera must serve NVR duty (store + camera + detector + /health ready)
/// without waiting for it — otherwise the Supervisor watchdog kills a slow boot
/// and reloops. So the NVR pipeline comes up first and /health reports ready
/// before fabric is done; the fabric wiring (offload seam, worker loop,
/// result-consumer, status task) lands in the SAME state the old synchronous
/// path produced, just later, published through the shared slot. Every
/// downstream consumer already tolerates absent fabric (fabric is optional
/// config), so reads of an unattached slot behave exactly like an unenrolled
/// node.
///
/// Being off that path is also what lets the three fabric values be ordinary
/// store settings: nothing here runs until the store has answered, so the
/// snapshot this bring-up carries is the resolved one.
#[cfg(feature = "fabric")]
fn spawn_fabric_bring_up(
    config: &crate::config::RuntimeConfig,
    stats: &RuntimeStatsState,
    accel: &Arc<crate::acceleration::AccelerationState>,
    fabric_slot: &Arc<std::sync::OnceLock<Arc<FabricBundle>>>,
    worker_detector_candidate: &Arc<
        std::sync::OnceLock<(Arc<crate::detector::PromotableDetector>, String)>,
    >,
) {
    if config.fabric_ticket.is_none() && !config.fabric_hub {
        return;
    }
    // Honest transient status until the async attach completes: every
    // operator surface (stats/doctor/health) reads this persisted line, so
    // it must say "starting" rather than look like an unenrolled node or a
    // healthy enrolled one. The async status task overwrites it with the
    // real enrolled/hub line once attached.
    stats.update(|stats| {
        stats.fabric_status = "fabric-status=starting fabric=starting \
             reason=fabric-bringup-in-progress remote-detectors=- in-use=false \
             fabric-worker-serving=false"
            .to_string();
    });
    println!("boot_phase=fabric-bringup-start");
    let bringup_config = config.clone();
    let bringup_stats = stats.clone();
    let bringup_accel = accel.clone();
    let bringup_slot = fabric_slot.clone();
    let bringup_candidate = worker_detector_candidate.clone();
    thread::spawn(move || {
        let started = Instant::now();
        match fabric_bring_up(
            &bringup_config,
            &bringup_stats,
            &bringup_accel,
            bringup_candidate,
        ) {
            Some(bundle) => {
                let _ = bringup_slot.set(bundle);
                println!(
                    "boot_phase=fabric-bringup-done elapsed_ms={} attached=true",
                    started.elapsed().as_millis()
                );
            }
            None => {
                println!(
                    "boot_phase=fabric-bringup-done elapsed_ms={} attached=false",
                    started.elapsed().as_millis()
                );
            }
        }
    });
}

/// The scope name this deployment records at. The operator surface resolves at
/// the same name, so a value written here is a value `vigil settings` reads
/// back rather than one resolved at a subtly different node.
fn deployment_scope_name(data_dir: &std::path::Path) -> String {
    crate::node_key::scope_name(data_dir)
}

pub(crate) fn run_detector_probe(args: Vec<OsString>) -> ExitCode {
    match run_detector_probe_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

fn run_detector_probe_inner(args: Vec<OsString>) -> Result<(), String> {
    yolox_detector::run_detector_probe(args)
}

fn run_inner(
    args: Vec<OsString>,
    site_channel: &dyn crate::site_channel::SiteChannelFactory,
) -> Result<(), String> {
    let boot_started = Instant::now();
    // Mutable for exactly one reason: once the store opens, the detection class
    // list is resolved from it and replaces the automatic default the loader
    // carried in. The store is the source of truth for that setting, and the
    // configuration is where every detector construction site reads it.
    let mut config = config::load(args)?;
    // What the store last resolved for the values consumed before it opens.
    // Anything a surface named on this start already stands; this only fills
    // the gap where nothing was said, and the store replaces it below.
    crate::settings_cache::apply_cached_startup_values(&mut config);
    validate_compiled_capability_requests(config.fabric_ticket.is_some(), config.fabric_hub)?;
    privilege::prepare_runtime_user(&config.store_path)?;
    // This process runs the cameras, so a value it resolves is a value in force
    // and a change it is handed applies to a live machine.
    crate::settings_application::mark_running_process();
    let mut shutdown = shutdown::install()?;
    let shutdown_flag = shutdown.flag();
    let health = HealthState::new();
    let accel = Arc::new(crate::acceleration::AccelerationState::new());
    // Where a preparation's progress lands beside the moment it began, so the
    // receipt an operator reads carries both while a cold build is going on.
    crate::live_backends::attend_acceleration_state(&accel);
    let stage_receipts = Arc::new(crate::workgraph::StageReceiptLog::new(64));
    // Registered once, before any camera thread starts: the production
    // detection probe (constructed with no path parameter, mirroring the
    // decode seam's host-facts-free shape) reads the same checkpoint the
    // live per-camera detector loads through this slot.
    crate::detection_accel::register_detector_model_path(config.detector_model_path.clone());
    crate::detection_accel::register_detector_class_indices(&config.detector_class_indices);
    // Created before the health server binds so `/health` can render the
    // same fabric-status/fabric-join lines `vigil stats`/`vigil doctor` do
    // (criterion C7).
    let stats = RuntimeStatsState::new(&config.data_dir);
    stats.update(|stats| {
        stats.health = "ready".to_string();
        stats.processing_lag_bound_ms = 1.0;
    });
    let camera_health_entries: Vec<config::CameraHealthEntry> = config
        .cameras
        .iter()
        .map(config::CameraEntry::health_entry)
        .collect();
    let server = HealthServer::bind(
        config.health_port,
        health.clone(),
        shutdown_flag.clone(),
        Some(accel.clone()),
        Some(crate::runtime_stats::HealthFabricStatus::from_state(&stats)),
        camera_health_entries,
    )?;
    // What this process is actually serving the liveness surface on, which is
    // what the operator surface reports as running beside the value the store
    // holds — a value stored while this run is up is pending, never effective.
    crate::settings_projection::record_running(
        crate::settings_model::HEALTH_PORT_SETTING,
        crate::settings_model::SettingValue::Int(i64::from(server.port())),
    );
    // `worker_detector_candidate` is created HERE, before both the cameras and
    // the fabric bring-up, and shared with both: a camera's detector may load
    // before fabric finishes attaching, so it publishes its detector into this
    // slot and the worker loop picks it up the instant the bundle appears —
    // decoupling "first camera detector wins the worker slot" from bring-up
    // timing.
    #[cfg(feature = "fabric")]
    let fabric_slot: Arc<std::sync::OnceLock<Arc<FabricBundle>>> =
        Arc::new(std::sync::OnceLock::new());
    #[cfg(feature = "fabric")]
    let worker_detector_candidate: Arc<
        std::sync::OnceLock<(Arc<crate::detector::PromotableDetector>, String)>,
    > = Arc::new(std::sync::OnceLock::new());

    log_startup(&config);

    let mut control = None;
    let mut camera_handles: Vec<JoinHandle<()>> = Vec::new();
    let mut command_listener: Option<Box<dyn crate::site_channel::CommandListener>> = None;
    let mut detection_publisher: Option<Arc<dyn crate::site_channel::DetectionChannel>> = None;
    let mut site_presence: Option<Box<dyn crate::site_channel::SitePresence>> = None;
    let mut review_server = None;
    // Set when the run has no store behind it and keeps watching anyway,
    // carrying WHY: an unreadable store and a component that failed to load
    // are different faults with different repairs, and the surfaces say which.
    let mut unmanaged_start: Option<(crate::settings_degraded::DegradedCause, String)> = None;
    println!(
        "boot_phase=store-open-start path={}",
        config.store_path.display()
    );
    let store_open_started = Instant::now();
    let opened = match store::open_with_recognition(&config.store_path, &config.recognition) {
        Ok((store, embedder)) => {
            println!(
                "boot_phase=store-open-done elapsed_ms={}",
                store_open_started.elapsed().as_millis()
            );
            if embedder.is_some() {
                println!("{}", recognition_enabled_startup_line(&config.recognition));
                // The store is open WITH this embedder, so these two are in
                // force for as long as this run lasts: what the operator
                // surface reports as running, beside whatever the store holds.
                if let Some(weights_dir) = config.recognition.weights_dir.as_ref() {
                    crate::settings_projection::record_running(
                        crate::settings_model::RECOGNITION_WEIGHTS_DIR_SETTING,
                        crate::settings_model::SettingValue::text(
                            weights_dir.display().to_string(),
                        ),
                    );
                }
                crate::settings_projection::record_running(
                    crate::settings_model::RECOGNITION_SPACE_ID_SETTING,
                    crate::settings_model::SettingValue::text(
                        config.recognition.embedding_space_id.clone(),
                    ),
                );
            }
            Some((store, embedder))
        }
        Err(failure) => {
            let detail = failure.detail();
            println!(
                "store open error path={} error={detail}",
                config.store_path.display(),
            );
            match failure {
                // The store was never reached: a component the runtime loads
                // first is what failed. The property is still watched, and the
                // operator is sent to the thing that actually broke rather than
                // to a store that is perfectly fine.
                store::StartupOpenFailure::Component { component, detail } => {
                    println!("component_load_failed=true component={component} error={detail}");
                    unmanaged_start = Some((
                        crate::settings_degraded::DegradedCause::ComponentUnavailable {
                            component: component.to_string(),
                        },
                        detail,
                    ));
                }
                store::StartupOpenFailure::Store(detail)
                    if crate::is_database_locked_error(&detail) =>
                {
                    // Another runtime owns this data directory. Two runtimes on
                    // one store is not a configuration problem, and degrading
                    // over it would put a second writer behind the first one's
                    // back.
                    health.set(HealthStatus::StoreOpenFailed, "store open failed");
                }
                store::StartupOpenFailure::Store(detail) => {
                    // The store exists and cannot be read. A camera system going
                    // blind because a settings database is unreadable is the worse
                    // outcome, so this run keeps watching and says so.
                    unmanaged_start = Some((
                        crate::settings_degraded::DegradedCause::StoreUnreadable,
                        detail,
                    ));
                }
            }
            None
        }
    };
    let recognition_embedder = opened.as_ref().and_then(|(_, embedder)| embedder.clone());
    let store = match opened {
        Some((store, _)) => {
            let state = if store.created {
                "store created"
            } else {
                "existing store"
            };
            println!("{state} path={}", store.path.display());
            println!("store opened path={}", store.path.display());
            println!("{}", store.trace);
            // This run has a store behind it, so its counters are worth
            // recording and the data directory is Vigil's to write in. Both
            // stay untouched until here: a run that turns out to have no store
            // must never leave a trace that makes it look recorded.
            stats.begin_persisting();
            // A crash between staging write and cleanup strands files; staging
            // is ephemeral by definition, so sweep it every boot.
            let staging_dir = config.data_dir.join("staging");
            if staging_dir.exists()
                && let Err(error) = fs::remove_dir_all(&staging_dir)
            {
                println!(
                    "staging_sweep_failed=true path={} error={error}",
                    staging_dir.display()
                );
            }
            // Persist the wgpu/Vulkan shader cache under the data root once,
            // before any camera-thread detection probe compiles shaders, so a
            // first boot's cold compile survives restarts instead of being
            // re-paid every start.
            #[cfg(feature = "detect-burn-wgpu")]
            if let Err(error) =
                crate::detection_accel::configure_persistent_shader_cache(&config.data_dir)
            {
                println!(
                    "shader_cache_setup_failed=true path={} error={error}",
                    config.data_dir.display()
                );
            }
            // Step three of the startup path: the surfaces write what they
            // assert into the store, as records naming the surface they came
            // from, and the runtime then resolves from the store. A surface
            // authors only when what it says now differs from what it last
            // said, so an unchanged file re-asserts nothing and re-wins
            // nothing at every boot.
            apply_input_surfaces(&config);
            // Step four: resolve every setting the store governs — what the
            // cameras look for, how hard they look, and the two automatic
            // management switches — before any detector or camera thread
            // exists, so what runs is what the store says rather than what a
            // merge produced.
            apply_resolved_settings(&mut config);
            // Step four and a half: this node's identity, resolved from its
            // persisted record before anything announces it — the site
            // announcement below, the outbound channel, and the operator
            // surface all name the same value.
            resolve_service_identity(&mut config, true);
            log_resolved_startup(&config);
            // The store may have named a fabric role this artifact does not
            // carry, and the check that refuses one ran before the store could
            // answer. It runs again on the resolved value, and it stays loud:
            // coming up as if a role had been taken would be the quiet failure
            // the check exists to prevent.
            validate_compiled_capability_requests(
                config.fabric_ticket.is_some(),
                config.fabric_hub,
            )?;
            // Step five: fabric bring-up, which reads the values just resolved.
            #[cfg(feature = "fabric")]
            spawn_fabric_bring_up(
                &config,
                &stats,
                &accel,
                &fabric_slot,
                &worker_detector_candidate,
            );
            println!("runtime loop ready");
            if config.cameras.is_empty() {
                // A legitimate worker/discovery deployment with zero cameras
                // must never present as an ordinary, camera-serving,
                // unqualified "ready" box (the worst failure mode on a
                // camera product is a box that reports it is fine while
                // watching nothing) — still a live, healthy 2xx answer.
                health.set(
                    HealthStatus::NoCamerasConfigured,
                    "no cameras configured: this deployment is a worker/discovery node with zero [[cameras]] entries",
                );
            } else {
                health.set(HealthStatus::Ready, "store open and runtime loop ready");
            }
            let owner_store = store.handle.clone();
            let owner_stats = stats.clone();
            // The deployment directory travels with the handler because the
            // settings answer is about the deployment, not about the memory
            // graph: opening a second store inside the handler would re-derive
            // what this process already holds.
            let owner_data_dir = config.data_dir.clone();
            let read_handler: context_graph::ControlHandler = Arc::new(move |request: String| {
                let stats = owner_stats.snapshot();
                live_read::handle_owner_request(&owner_store, &stats, &owner_data_dir, &request)
            });
            let control_socket_path = crate::control_socket::control_socket_path(&config.data_dir);
            control =
                start_control_listener(&control_socket_path, shutdown_flag.clone(), read_handler);
            match crate::http_data_plane::spawn_review_data_plane(
                store.handle.clone(),
                config.data_dir.clone(),
                config.review_port,
            ) {
                Ok(handle) => {
                    println!("review_data_plane_started=true port={}", config.review_port);
                    record_running_review_port(config.review_port);
                    review_server = Some(handle);
                }
                Err(error) => {
                    println!(
                        "review_data_plane_start_failed=true port={} error={error}",
                        config.review_port
                    );
                }
            }

            // ── Outbound detection channel — connected before camera threads ──
            // An attached channel owns one persistent connection for all
            // detection facts.  It must be connected before camera threads
            // start so they capture a live Arc rather than None.
            if let Some(ref endpoint) = config.mqtt {
                detection_publisher =
                    site_channel.connect(endpoint, &config.service_id, health.clone());
            }

            // ── Multi-camera fan-out ───────────────────────────────────────
            // Build per-camera enabled flags (checked against startup disable markers).
            let mut camera_flags: std::collections::BTreeMap<String, Arc<AtomicBool>> =
                std::collections::BTreeMap::new();

            for camera in &config.cameras {
                let cam_id = camera_slug(&camera.name);
                let disable_marker = config.data_dir.join("camera-disabled").join(&cam_id);
                let is_disabled = disable_marker.exists();
                let enabled = Arc::new(AtomicBool::new(!is_disabled));
                camera_flags.insert(cam_id.clone(), Arc::clone(&enabled));

                if let Some(url) = &camera.rtsp_url {
                    let memory_url = media_pipeline::redact_rtsp_url_for_persistence(url);
                    // Clone config and patch per-camera fields so existing sub-functions
                    // (maintain_runtime_memory, record_detected_events) see the right camera.
                    let mut cam_config = config.clone();
                    cam_config.camera_name = camera.name.clone();
                    cam_config.rtsp_url = Some(url.clone());
                    cam_config.rtsp_username = camera.username.clone();
                    cam_config.rtsp_password = camera.password.clone();

                    match maintain_runtime_memory(&store.handle, &cam_config, &memory_url) {
                        Ok(_) => println!("runtime_memory_ready camera={cam_id}"),
                        Err(error) => {
                            println!("runtime_memory_setup_failed camera={cam_id} error={error}")
                        }
                    }

                    if is_disabled {
                        println!("camera_disabled_at_startup camera={cam_id}");
                    }

                    let handle = start_rtsp_probe(
                        url.clone(),
                        cam_config,
                        Some(store.handle.clone()),
                        stats.clone(),
                        health.clone(),
                        shutdown_flag.clone(),
                        Arc::clone(&enabled),
                        detection_publisher.clone(),
                        recognition_embedder.clone(),
                        accel.clone(),
                        stage_receipts.clone(),
                        #[cfg(feature = "fabric")]
                        fabric_slot.clone(),
                        #[cfg(feature = "fabric")]
                        worker_detector_candidate.clone(),
                    );
                    camera_handles.push(handle);
                }

                let generic_camera_url = generic_camera_url(camera);
                if let Some(generic_camera_url) = generic_camera_url {
                    // Create a Generic Camera config entry in HA for live view. When
                    // live_rtsp_url is set, keep it separate from the detection ingest URL.
                    register_generic_camera(&cam_id, generic_camera_url, &config.data_dir);
                }
            }

            // Announce the site and start listening for owner commands (needs
            // camera_flags which is now fully built). Gate on the same
            // predicate used above so the listener and the outbound channel
            // are always either both present or both absent.
            if let Some(ref endpoint) = config.mqtt {
                let site = crate::site_channel::SiteAnnouncement {
                    service_name: config.site_name.clone(),
                    service_id: config.service_id.clone(),
                    cameras: config
                        .cameras
                        .iter()
                        .map(|c| crate::site_channel::CameraAnnouncement {
                            id: camera_slug(&c.name),
                            label: c.name.clone(),
                        })
                        .collect(),
                };
                // Bounded correction channel: the listener forwards correction
                // commands here; the worker thread drains and writes to cg.
                let (correction_tx, correction_rx) =
                    std::sync::mpsc::sync_channel::<crate::correction::CorrectionRequest>(64);
                let store_worker = Arc::new(store.handle.clone());
                thread::Builder::new()
                    // This name is also the physical HA-OS audit boundary: TH-23
                    // attaches syscall tracing to this dedicated writer before it
                    // publishes a correction. Keep the write isolated here so
                    // camera, MQTT, and fabric traffic cannot create false egress
                    // findings for the local correction path.
                    .name("vigil-correct".to_string())
                    .spawn(move || {
                        for req in correction_rx {
                            let fingerprint =
                                crate::correction::correction_execution_fingerprint(&req);
                            match crate::correction::record_correction(&store_worker, req) {
                                Ok(_) => println!(
                                    "correction_writer_receipt={fingerprint} status=landed"
                                ),
                                Err(_) => println!(
                                    "correction_writer_receipt={fingerprint} status=failed"
                                ),
                            }
                        }
                    })
                    .map_err(|error| format!("could not start correction writer: {error}"))?;
                // Matches the pre-seam derivation exactly (`store.db_path()`'s
                // parent, not `config.data_dir`): an operator-overridden
                // `--store-path` need not live under `data_dir`, and the
                // disable-marker/snapshot-read location must not silently
                // move when it doesn't.
                let control_data_dir = store
                    .handle
                    .db_path()
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| config.data_dir.clone());
                let control = crate::site_channel::store_backed_site_control(
                    store.handle.clone(),
                    control_data_dir,
                    camera_flags,
                    correction_tx,
                );
                command_listener = site_channel.listen(endpoint, site, health.clone(), control);
            }
            println!(
                "boot_phase=pipeline-up elapsed_ms={}",
                boot_started.elapsed().as_millis()
            );
            Some(store.handle)
        }
        // This run has no store behind it. Everything that needs one is
        // unavailable and says so; everything that does not need one keeps
        // running, because someone watching a property still has to see what is
        // happening and still has to be told about it. Nothing here writes —
        // not a stats snapshot, not a Home Assistant camera registration, not
        // the persisted service identity — so an unmanaged run can never later
        // be mistaken for a recorded one.
        None => {
            if let Some((cause, error)) = unmanaged_start.clone() {
                // Declared before anything states it, so every surface names
                // the fault that actually happened rather than the one the
                // storeless seam was first written for.
                crate::settings_degraded::declare_cause(cause.clone());
                let statement = crate::settings_degraded::unmanaged_statement();
                println!(
                    "{} {statement}",
                    crate::settings_projection::UNMANAGED_LINE_PREFIX
                );
                let health_detail = match &cause {
                    crate::settings_degraded::DegradedCause::StoreUnreadable => {
                        println!(
                            "store_unreadable=true path={} error={error}",
                            config.store_path.display()
                        );
                        "store unreadable: live view, detection and alerting are running unmanaged"
                    }
                    crate::settings_degraded::DegradedCause::ComponentUnavailable { .. } => {
                        println!(
                            "store_unreadable=false component_unavailable=true path={} \
                             error={error}",
                            config.store_path.display()
                        );
                        "a component failed to load, so no store was opened: live view, \
                         detection and alerting are running unmanaged"
                    }
                };
                // Declared before the status is set, so the first `/health`
                // answer that reports this run already carries the statement.
                health.declare_unmanaged(statement);
                health.set(HealthStatus::RunningUnmanaged, health_detail);
                // A run with no store behind it still says what it came up on:
                // there is nothing to resolve from, so these are the values its
                // own surfaces carried in — including an identity derived for
                // this run only and written nowhere.
                resolve_service_identity(&mut config, false);
                log_resolved_startup(&config);

                // Fabric brings up on the values this run resolved from its
                // surfaces: there is no store to resolve them from, and a node
                // that can still reach its fabric is one more capability the
                // degraded path keeps rather than loses.
                #[cfg(feature = "fabric")]
                spawn_fabric_bring_up(
                    &config,
                    &stats,
                    &accel,
                    &fabric_slot,
                    &worker_detector_candidate,
                );

                // The operator surface answers over the same control socket it
                // always does — that is where the unmanaged statement is read,
                // so it is the last thing a degraded run may lose.
                let handler_stats = stats.clone();
                let handler_data_dir = degraded_control_data_dir(&config);
                let read_handler: context_graph::ControlHandler =
                    Arc::new(move |request: String| {
                        let stats = handler_stats.snapshot();
                        live_read::handle_degraded_request(&stats, &handler_data_dir, &request)
                    });
                let control_socket_path =
                    crate::control_socket::control_socket_path(&config.data_dir);
                control = start_control_listener(
                    &control_socket_path,
                    shutdown_flag.clone(),
                    read_handler,
                );

                // The review surfaces answer too, and what they answer is why
                // they cannot serve what was asked for. A port refusing
                // connections would leave an operator with a transport error
                // instead of the cause.
                match crate::http_data_plane::spawn_degraded_review_plane(config.review_port) {
                    Ok(handle) => {
                        println!(
                            "review_data_plane_started=true port={} unmanaged=true",
                            config.review_port
                        );
                        record_running_review_port(config.review_port);
                        review_server = Some(handle);
                    }
                    Err(error) => {
                        println!(
                            "review_data_plane_start_failed=true port={} error={error}",
                            config.review_port
                        );
                    }
                }

                // Alerting: the outbound detection channel is connected before
                // any camera thread starts, exactly as it is on the
                // store-backed path, so a detection made in the first seconds
                // still reaches Home Assistant.
                if let Some(ref endpoint) = config.mqtt {
                    detection_publisher =
                        site_channel.connect(endpoint, &config.service_id, health.clone());
                }
                // No owner-command listener: the commands it carries are
                // corrections, and corrections are exactly what this run
                // cannot record.

                for camera in &config.cameras {
                    let cam_id = camera_slug(&camera.name);
                    let disable_marker = config.data_dir.join("camera-disabled").join(&cam_id);
                    let is_disabled = disable_marker.exists();
                    let enabled = Arc::new(AtomicBool::new(!is_disabled));
                    if is_disabled {
                        println!("camera_disabled_at_startup camera={cam_id}");
                    }
                    if let Some(url) = &camera.rtsp_url {
                        let mut cam_config = config.clone();
                        cam_config.camera_name = camera.name.clone();
                        cam_config.rtsp_url = Some(url.clone());
                        cam_config.rtsp_username = camera.username.clone();
                        cam_config.rtsp_password = camera.password.clone();
                        let handle = start_rtsp_probe(
                            url.clone(),
                            cam_config,
                            // No store: detections are published best-effort
                            // and nothing is recorded.
                            None,
                            stats.clone(),
                            health.clone(),
                            shutdown_flag.clone(),
                            Arc::clone(&enabled),
                            detection_publisher.clone(),
                            // Recognition matches against the site library,
                            // which lives in the store.
                            None,
                            accel.clone(),
                            stage_receipts.clone(),
                            #[cfg(feature = "fabric")]
                            fabric_slot.clone(),
                            #[cfg(feature = "fabric")]
                            worker_detector_candidate.clone(),
                        );
                        camera_handles.push(handle);
                    }
                    // No Home Assistant generic-camera registration here: it
                    // writes a file that outlives the run, and what this run
                    // leaves behind is exactly nothing. This run does put
                    // files under the data directory while it lives — its
                    // control socket, and capture segments it writes and
                    // deletes as it goes — but it never adds to, or creates,
                    // the store data a healthy run would have persisted.
                }

                // Presence: the same entities, the same availability, the same
                // running condition and the same last will a healthy run
                // announces — announced here too, because the person watching
                // the property is the reason this run kept going, and an
                // integration that never heard of this node shows them nothing.
                // What is NOT brought up is the owner-command listener: the
                // commands it carries are corrections, and corrections are
                // exactly what this run cannot record.
                if let Some(ref endpoint) = config.mqtt {
                    let site = crate::site_channel::SiteAnnouncement {
                        service_name: config.site_name.clone(),
                        service_id: config.service_id.clone(),
                        cameras: config
                            .cameras
                            .iter()
                            .map(|camera| crate::site_channel::CameraAnnouncement {
                                id: camera_slug(&camera.name),
                                label: camera.name.clone(),
                            })
                            .collect(),
                    };
                    site_presence = site_channel.announce(endpoint, site, health.clone());
                }
                println!(
                    "boot_phase=pipeline-up elapsed_ms={} unmanaged=true",
                    boot_started.elapsed().as_millis()
                );
            }
            None
        }
    };

    shutdown.wait();

    // Teardown order — store handle drops LAST:
    //  1. Command listener (holds Arc<Store>; signals shutdown, blocks until thread exits)
    //  2. Detection channel (camera threads must exit first so all senders are gone)
    //  3. Camera probe threads (each holds a Store clone via Arc)
    //  4. Control listener (read_handler captures a Store clone)
    //  5. Review data plane (holds a Store clone)
    //  6. Health server (no Store reference — safe to join before or after store)
    //  7. drop(store) — all other Store holders are now joined and their clones dropped
    if let Some(listener) = command_listener {
        listener.shutdown_and_join();
        println!("mqtt_subscriber_stopped=true");
    }
    // The presence connection stands where the listener does on a store-backed
    // run, and comes down in the same place for the same reason.
    if let Some(presence) = site_presence {
        presence.shutdown_and_join();
        println!("mqtt_presence_stopped=true");
    }
    // Camera handles exit first so their detection-channel senders are all dropped.
    for handle in camera_handles {
        let _ = handle.join();
    }
    if let Some(channel) = detection_publisher {
        channel.shutdown_and_join();
        println!("mqtt_detection_publisher_stopped=true");
    }
    if let Some(handle) = control.take() {
        let _ = handle.join();
    }
    if let Some(handle) = review_server {
        handle.shutdown();
    }
    server.join();
    drop(store);
    Ok(())
}

fn validate_compiled_capability_requests(
    fabric_ticket_configured: bool,
    fabric_hub_requested: bool,
) -> Result<(), String> {
    #[cfg(not(feature = "fabric"))]
    if fabric_ticket_configured || fabric_hub_requested {
        return Err(
            "fabric was configured, but this Vigil binary was built without the fabric capability; use an amd64/aarch64 normal release artifact or rebuild with --features fabric"
                .to_string(),
        );
    }
    let _ = (fabric_ticket_configured, fabric_hub_requested);
    Ok(())
}

fn log_startup(config: &config::RuntimeConfig) {
    println!(
        "vigil version={} startup_epoch={}",
        env!("CARGO_PKG_VERSION"),
        startup_epoch()
    );
    println!("data_dir={}", display(&config.data_dir));
    println!("store_path={}", display(&config.store_path));
    println!("health_port={}", config.health_port);
    println!("review_port={}", config.review_port);
    println!("detector_model_id={}", config.detector_model_id);
    println!(
        "detector_confidence_threshold={}",
        config.detector_confidence_threshold
    );
    println!("detector_sample_frames={}", config.detector_sample_frames);
    println!(
        "detector_stationary_interval_secs={}",
        config.detector_stationary_interval_secs
    );
    if let Some(mqtt) = config.mqtt.as_ref() {
        println!("mqtt_host={}", mqtt.host);
        println!("mqtt_port={}", mqtt.port);
        println!(
            "mqtt_username={}",
            mqtt.username.as_deref().unwrap_or("<none>")
        );
    } else {
        println!("mqtt=disabled");
    }
}

/// Put this node's persisted service identity into the configuration every
/// consumer reads it from, before anything announces it.
///
/// The identity names the Home Assistant device and the messaging topics, so
/// it is derived ONCE — at the first start that has a store to persist it into
/// — and read back from that record on every later start. A site rename after
/// that changes a display name and nothing else. Where the deployment supplied
/// an identifier itself, that value seeds the first start; it does not move an
/// identity that has already named a device, because moving one is its own
/// deliberate operation and this is not it.
///
/// A store that cannot answer leaves the run with an identity derived for this
/// run only, which is what the storeless path uses and says so.
fn resolve_service_identity(config: &mut config::RuntimeConfig, store_backed: bool) {
    let configured = config.service_id.clone();
    let resolved = if !store_backed {
        Err(None)
    } else if configured.is_empty() {
        crate::service_identity::resolve_persisted(&config.store_path, &config.site_name)
            .map_err(|error| Some(error.to_string()))
    } else {
        crate::service_identity::resolve_persisted_configured(&config.store_path, &configured)
            .map_err(|error| Some(error.to_string()))
    };
    let identity = match resolved {
        Ok(identity) => identity,
        Err(error) => {
            if let Some(error) = error {
                println!("service_identity_unresolved={error}");
            }
            resolve_ephemeral_identity(config)
        }
    };
    if !configured.is_empty() && identity.value != configured {
        // Said out loud rather than applied silently: the deployment asked for
        // one identifier and this node already answers to another one it
        // persisted earlier, which only the deliberate change operation moves.
        println!(
            "service_identity_configured_ignored={configured} in_force={}",
            identity.value
        );
    }
    // Two lines, because each carries one whole value: an operator grepping
    // for the identifier this node announces must read it without having to
    // cut a longer line apart.
    println!("service_id={}", identity.value);
    println!(
        "service_identity_derivation={}",
        crate::settings_projection::derivation_token(&identity.derivation)
    );
    // One resolution, read by every later surface in this process: the operator
    // surface answers with the identity this run is announcing rather than
    // working the question out again from the deployment directory's name.
    crate::service_identity::record_in_force(&identity);
    config.service_id = identity.value;
}

/// The deployment directory a run with no store behind it answers from.
///
/// Derived from the configured store path's parent, exactly as the
/// store-backed path derives it from the opened store's own location: an
/// operator-overridden `--store-path` need not live under `data_dir`, and a
/// degraded run that answered from `data_dir` instead would resolve a settings
/// change against a place no store lives — creating a fresh, healthy store
/// there and writing into it, which is precisely the write an unmanaged run
/// must never make.
fn degraded_control_data_dir(config: &config::RuntimeConfig) -> PathBuf {
    config
        .store_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config.data_dir.clone())
}

/// The identity a run with no store behind it uses: derived for this run only
/// and written nowhere, so an unmanaged run can never be mistaken for a
/// recorded one.
fn resolve_ephemeral_identity(
    config: &config::RuntimeConfig,
) -> crate::service_identity::ServiceIdentity {
    if config.service_id.is_empty() {
        crate::service_identity::resolve_ephemeral(&config.site_name)
    } else {
        crate::service_identity::ServiceIdentity {
            value: config.service_id.clone(),
            // The deployment named this node. Nothing persisted it, because
            // there is no store to persist it into — but it was stated, not
            // worked out, and the surface says which of the two happened.
            derivation: crate::service_identity::IdentityDerivation::ConfiguredNotYetPersisted,
        }
    }
}

/// Which camera streams this run came up on, announced once the store has
/// answered.
///
/// They are announced HERE rather than beside the ports because they are not
/// settled until the store has been resolved: a line printed before that says
/// what the configuration file asked for, which is exactly the value a stored
/// record outranks. Printing it early would have this run name a camera it does
/// not bring up.
///
/// The endpoints and the model file are what this names, and the deployment's
/// names are not: an operator reads a name off `vigil settings`, with who set
/// it and what is running beside it, while the two endpoints are answered for
/// there as a SOURCE — their spelling carries the camera's credentials — so the
/// redacted line here is the only place a person can see which stream this run
/// actually opened.
fn log_resolved_startup(config: &config::RuntimeConfig) {
    if let Some(rtsp_url) = config.rtsp_url.as_ref() {
        println!("rtsp_url={}", media_pipeline::redact_rtsp_url(rtsp_url));
    }
    if let Some(camera) = config.cameras.first()
        && let Some(live_rtsp_url) = camera.live_rtsp_url.as_ref()
    {
        println!(
            "live_rtsp_url={}",
            media_pipeline::redact_rtsp_url(live_rtsp_url)
        );
    }
    if let Some(model_path) = config.detector_model_path.as_ref() {
        println!("detector_model_path={}", display(model_path));
    }
}

pub(crate) fn recognition_enabled_startup_line(
    recognition: &crate::recognition::RecognitionConfig,
) -> String {
    let mut line = format!(
        "recognition_enabled=true space={} threshold={}",
        recognition.embedding_space_id, recognition.match_threshold
    );
    if recognition.match_threshold < 0.90 {
        line.push_str(" accuracy_warning=below_recommended_default_0.9");
    }
    line
}

fn startup_epoch() -> u64 {
    crate::clock::unix_seconds()
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

/// Bootstrap ONE worker detector for a cameraless fabric worker box (owner
/// steer 2026-07-13): a node with a serving role but no camera source still
/// serves the fleet, so it needs a detector without any per-camera thread
/// building one. Uses the SAME `detection_accel` selection the per-camera path
/// uses (so the receipt and the live detector can never disagree) to pick the
/// truthful backend tag, records the receipt onto stats/accel (so `vigil
/// doctor`/`vigil stats` report it), and loads the detector through the SAME
/// `yolox_detector` path.
///
/// `Some` with the loaded promotable detector + truthful backend tag ONLY when
/// a model artifact is actually present and loadable — advertisement requires
/// EXECUTABLE capability (PO ruling 2026-07-13): a node that cannot run
/// detection must not tell the fleet it can. A missing/unloadable model is
/// logged loudly and returns `None`; the caller then renders a named
/// `fabric-worker-serving=false reason=no-model` line with the staging fix,
/// never a silent no-op and never a panic.
/// What the detector a node runs distributed work on is called, where a
/// camera's handle is called by the camera's own name.
#[cfg(feature = "fabric")]
const WORKER_DETECTOR_HANDLE: &str = "distributed-work";

#[cfg(feature = "fabric")]
pub(crate) fn bootstrap_worker_detector(
    config: &config::RuntimeConfig,
    accel: &Arc<crate::acceleration::AccelerationState>,
    stats: &RuntimeStatsState,
) -> Option<(Arc<crate::detector::PromotableDetector>, String)> {
    // The fabric bring-up took its snapshot of the configuration before the
    // store opened, so this path resolves the class list itself rather than
    // building a worker detector that looks for something narrower than the
    // cameras on the same node do.
    let mut resolved = config.clone();
    apply_resolved_settings(&mut resolved);
    let config = &resolved;
    let selection = crate::detection_accel::select_detection_acceleration(
        config.accelerated_detection,
        &config.detector_model_id,
        yolox_detector::MODEL_INPUT_SHAPE,
    );
    let receipt = selection.receipt;
    let active_backend_tag = receipt.active_backend.clone();
    let accelerated_selected =
        selection.backend == crate::detection_accel::ACCELERATED_DETECTION_BACKEND;
    // One input decides which classes this detector emits: the resolved class
    // setting, carried on the configuration. Recognition is not consulted here
    // and has no branch — that coupling is what made a widened class list
    // detect nothing.
    let detector_load: Result<Box<dyn crate::detector::Detector>, String> = if accelerated_selected
    {
        #[cfg(feature = "detect-burn-wgpu")]
        {
            yolox_detector::load_accelerated_detector_with_classes(
                config.detector_model_path.as_deref(),
                &config.detector_class_indices,
            )
            .map(|detector| Box::new(detector) as Box<dyn crate::detector::Detector>)
        }
        #[cfg(not(feature = "detect-burn-wgpu"))]
        {
            yolox_detector::load_cpu_detector_with_classes(
                config.detector_model_path.as_deref(),
                &config.detector_class_indices,
            )
            .map(|detector| Box::new(detector) as Box<dyn crate::detector::Detector>)
        }
    } else {
        yolox_detector::load_cpu_detector_with_classes(
            config.detector_model_path.as_deref(),
            &config.detector_class_indices,
        )
        .map(|detector| Box::new(detector) as Box<dyn crate::detector::Detector>)
    };

    match detector_load {
        Ok(detector) => {
            println!(
                "fabric_worker_detector_loaded id={} sha256={} backend={active_backend_tag}",
                config.detector_model_id,
                detector.model_sha256()
            );
            stats.update(|stats| {
                stats.active_detector_backend = receipt.active_backend.clone();
                stats.detection_acceleration = format!(
                    "{}{}",
                    receipt.probe_status.as_str(),
                    match receipt.failure_code {
                        crate::acceleration::FailureCode::None => String::new(),
                        code => format!(":{}", code.as_str()),
                    }
                );
                stats.detection_receipt_block = crate::acceleration::render_receipt_block(&receipt);
            });
            accel.record(receipt);
            let handle = Arc::new(crate::detector::PromotableDetector::new(detector));
            // The detector this node runs distributed work on is registered
            // like any other, so a backend move reaches it. On a node that
            // watches cameras this work runs on the camera detectors, and the
            // coordinator keeps no second handle for it — which is what stops
            // a camera node preparing a second model for work already done.
            crate::live_backends::register_work_detector(
                &handle,
                crate::detection_transition::HandleKind::DistributedWork,
                crate::live_backends::DetectorRebuild {
                    camera: WORKER_DETECTOR_HANDLE.to_string(),
                    model_id: config.detector_model_id.clone(),
                    accelerated_detection: config.accelerated_detection,
                    build: Arc::new(detector_build_for(
                        config.detector_model_path.clone(),
                        config.detector_class_indices.clone(),
                    )),
                    receipt: Arc::new(detection_receipt_sink(stats.clone())),
                },
            );
            Some((handle, active_backend_tag))
        }
        Err(error) => {
            // Loud, named, non-fatal: no executable capability, so this node
            // will NOT advertise a detector row — the caller renders a
            // named not-serving reason. Never a panic.
            println!(
                "fabric_worker_detector_load_failed=true backend={active_backend_tag} \
                 error={error} action=stage-the-detection-model-on-this-worker"
            );
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(feature = "fabric"), allow(unused_variables))]
pub fn start_rtsp_probe(
    rtsp_url: String,
    config: config::RuntimeConfig,
    // `None` when this run has no store behind it: the camera is watched and
    // its detections are published best-effort, and nothing is recorded.
    store: Option<Store>,
    stats: RuntimeStatsState,
    health: HealthState,
    shutdown: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    detection_publisher: Option<Arc<dyn crate::site_channel::DetectionChannel>>,
    recognition_embedder: Option<Arc<dyn context_graph::Embedder>>,
    accel: Arc<crate::acceleration::AccelerationState>,
    receipts: Arc<crate::workgraph::StageReceiptLog>,
    // Fabric attaches asynchronously (see `run_inner`), so the probe reads the
    // bundle from a shared slot per-segment rather than capturing it at spawn:
    // it may still be empty when the camera starts and fill in later. The
    // detector, when it loads, publishes itself into `worker_detector_candidate`
    // (independent of fabric timing) for the worker loop to claim.
    #[cfg(feature = "fabric")] fabric_slot: Arc<std::sync::OnceLock<Arc<FabricBundle>>>,
    #[cfg(feature = "fabric")] worker_detector_candidate: Arc<
        std::sync::OnceLock<(Arc<crate::detector::PromotableDetector>, String)>,
    >,
) -> JoinHandle<()> {
    thread::spawn(move || {
        // A panic on this thread must not kill a camera silently while
        // /health keeps saying Ready: catch it, latch ingest-failed, log.
        let panic_health = health.clone();
        let panic_stats = stats.clone();
        let panic_camera = config.camera_name.clone();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            // Wait until enabled (respects startup disable marker).
            while !shutdown.load(Ordering::SeqCst) && !enabled.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(5));
            }
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            let rtsp_source = match media_pipeline::prepare_rtsp_source(
                &rtsp_url,
                config.rtsp_username.as_deref(),
                config.rtsp_password.as_ref(),
            ) {
                Ok(source) => source,
                Err(error) => {
                    println!(
                        "rtsp probe failed url={} error={error}",
                        media_pipeline::redact_rtsp_url(&rtsp_url)
                    );
                    stats.update(|stats| {
                        stats.ingest_signal = "decode-error".to_string();
                        mark_health_condition(&mut stats.health, "ingest_failed");
                    });
                    health.set(HealthStatus::IngestFailed, "RTSP ingest failed");
                    health.set_camera(&config.camera_name, CameraCondition::IngestFailed);
                    return;
                }
            };
            // `session_url()` has its userinfo stripped (`strip_url_credentials`)
            // but NOT its query/fragment — a token-carrying query
            // (`?token=...`) survives unchanged there. Route through the
            // hardened redactor so a token never reaches the probe/open/play
            // log lines below.
            let rtsp_log_url = media_pipeline::redact_rtsp_url(rtsp_source.session_url());
            println!("rtsp probe starting url={rtsp_log_url}");
            // The detector sits behind the engine-neutral trait: the pipeline sees
            // `dyn Detector`, the engine lives in the implementation.
            // Person-only by default (baseline NVR, first-light contract), and
            // it stays that way until an operator says otherwise: what the
            // detector looks for comes from the detection class setting alone.
            // Turning recognition on, or widening what it covers, neither
            // widens nor narrows detection — the two are separate choices a
            // person makes, and one field serving both would mean widening
            // either silently widened the other.
            // Capture must not report Ready while no detector is running:
            // load failure and detector-thread panic clear this flag, and
            // the per-segment health refresh consults it.
            let detector_alive = Arc::new(AtomicBool::new(false));
            // Select ONCE, before construction: the concrete backend built
            // below is picked from this SAME selection, so the receipt and
            // the live detector can never disagree — an accelerated claim
            // always means an accelerated detector actually runs, and any
            // fallback selection always means the CPU detector runs, even
            // with the accel feature compiled in.
            // Live promotion: the startup preparation moves every registered
            // detector itself, through the one transition machinery an operator
            // command goes through — one physical build per camera, through the
            // seam that camera registered, and the instance the preparation
            // forward-tested is the instance installed. This closure loads
            // nothing and installs nothing: it is this camera's account of a
            // move that has already landed. The paired stats sink writes the
            // same late receipt to RuntimeStats so `vigil stats` and the worker
            // provenance agree with /health on every late outcome.
            #[cfg(feature = "detect-burn-wgpu")]
            let selection = {
                let promote = move || -> Result<(), String> {
                    println!(
                        "detection_promoted_live backend={}",
                        crate::detection_accel::ACCELERATED_DETECTION_BACKEND
                    );
                    Ok(())
                };
                // The all-surfaces late-receipt sink: write the final late
                // receipt into RuntimeStats with the SAME shape as the startup
                // write below, so `vigil stats` and the per-detection worker
                // provenance read stop claiming burn-cpu after a real promotion
                // (and stay honest CPU on a failed one).
                let on_late_receipt = detection_receipt_sink(stats.clone());
                crate::detection_accel::spawn_detection_probe_with_promotion(
                    config.accelerated_detection,
                    &config.detector_model_id,
                    yolox_detector::MODEL_INPUT_SHAPE,
                    crate::detection_accel::YoloxForwardProbe::new(
                        &config.detector_model_id,
                        yolox_detector::MODEL_INPUT_SHAPE,
                    ),
                    Arc::clone(&accel),
                    promote,
                    on_late_receipt,
                )
            };
            #[cfg(not(feature = "detect-burn-wgpu"))]
            let selection = crate::detection_accel::select_detection_acceleration(
                config.accelerated_detection,
                &config.detector_model_id,
                yolox_detector::MODEL_INPUT_SHAPE,
            );
            let receipt = selection.receipt;
            let accelerated_selected =
                selection.backend == crate::detection_accel::ACCELERATED_DETECTION_BACKEND;
            // The class allowlist comes from the resolved setting on the
            // configuration, whichever backend runs. Recognition has no branch
            // here: which classes Vigil looks for is one setting an operator
            // owns, and a detector built from recognition's covered classes is
            // the coupling that made a widened class list detect nothing.
            let detector_load: Result<Box<dyn crate::detector::Detector>, String> =
                if accelerated_selected {
                    #[cfg(feature = "detect-burn-wgpu")]
                    {
                        yolox_detector::load_accelerated_detector_with_classes(
                            config.detector_model_path.as_deref(),
                            &config.detector_class_indices,
                        )
                        .map(|detector| Box::new(detector) as Box<dyn crate::detector::Detector>)
                    }
                    #[cfg(not(feature = "detect-burn-wgpu"))]
                    {
                        // The selection cannot report accelerated without the
                        // feature compiled in; unreachable in practice, but
                        // stays on the honest CPU path if it somehow did.
                        yolox_detector::load_cpu_detector_with_classes(
                            config.detector_model_path.as_deref(),
                            &config.detector_class_indices,
                        )
                        .map(|detector| Box::new(detector) as Box<dyn crate::detector::Detector>)
                    }
                } else {
                    yolox_detector::load_cpu_detector_with_classes(
                        config.detector_model_path.as_deref(),
                        &config.detector_class_indices,
                    )
                    .map(|detector| Box::new(detector) as Box<dyn crate::detector::Detector>)
                };
            #[cfg(feature = "fabric")]
            let active_backend_tag = receipt.active_backend.clone();
            let detector: Option<Box<dyn crate::detector::Detector>> = match detector_load {
                Ok(detector) => {
                    println!(
                        "detector model loaded id={} sha256={}",
                        config.detector_model_id,
                        detector.model_sha256()
                    );
                    if accel.should_log(&receipt) {
                        println!(
                            "detector_backend_selected active={} status={} failure={}",
                            receipt.active_backend,
                            receipt.probe_status.as_str(),
                            receipt.failure_code.as_str()
                        );
                    }
                    stats.update(|stats| {
                        stats.active_detector_backend = receipt.active_backend.clone();
                        // Same compact form as decode-acceleration (the
                        // failure code renders only when there is one); kept
                        // inline because the crate's source contract pins
                        // this assignment shape.
                        stats.detection_acceleration = format!(
                            "{}{}",
                            receipt.probe_status.as_str(),
                            match receipt.failure_code {
                                crate::acceleration::FailureCode::None => String::new(),
                                code => format!(":{}", code.as_str()),
                            }
                        );
                        stats.detection_receipt_block =
                            crate::acceleration::render_receipt_block(&receipt);
                    });
                    accel.record(receipt);
                    detector_alive.store(true, Ordering::SeqCst);
                    Some(detector)
                }
                Err(error) => {
                    println!("detector model load failed error={error}");
                    stats.update(|stats| {
                        stats.ingest_signal = "detector-load-error".to_string();
                        mark_health_condition(&mut stats.health, "ingest_failed");
                    });
                    health.set(HealthStatus::IngestFailed, "detector model load failed");
                    // The detector is this camera's: without one, this camera
                    // is watching nothing, whatever the others are doing.
                    health.set_camera(&config.camera_name, CameraCondition::IngestFailed);
                    None
                }
            };
            // Wrap the worker's detector in the swappable handle so a late
            // acceleration probe can promote it live. Registration is the only
            // way that promotion reaches this camera, and it is what closes the
            // window a deferred slot left open: a preparation that finishes
            // before this line runs finds no handle to move, and the
            // registration below is what asks for that move again — so nothing
            // depends on the probe finishing after this point.
            let detector: Option<Arc<crate::detector::PromotableDetector>> =
                detector.map(|detector| {
                    let handle = Arc::new(crate::detector::PromotableDetector::new(detector));
                    // The same handle, registered where a live backend change
                    // can reach it. An operator who takes the wheel gets the
                    // detector REPLACED under the backend they named, through
                    // the same load path this construction used — the late
                    // acceleration probe above is the other caller of the very
                    // same swap.
                    crate::live_backends::register_live_detector(
                        &handle,
                        crate::live_backends::DetectorRebuild {
                            camera: config.camera_name.clone(),
                            model_id: config.detector_model_id.clone(),
                            accelerated_detection: config.accelerated_detection,
                            build: Arc::new(detector_build_for(
                                config.detector_model_path.clone(),
                                config.detector_class_indices.clone(),
                            )),
                            receipt: Arc::new(detection_receipt_sink(stats.clone())),
                        },
                    );
                    // First camera detector ready wins the fabric worker
                    // loop's backend (the worker claims/executes ANY node's
                    // job, not one per camera — see `FabricBundle`'s doc).
                    // Published into the shared candidate slot independently of
                    // fabric attach timing: the bundle's `worker_detector_slot`
                    // IS this same `Arc<OnceLock>`, so a detector that loads
                    // before fabric finishes attaching is still claimed the
                    // instant the worker loop starts.
                    #[cfg(feature = "fabric")]
                    let _ = worker_detector_candidate
                        .set((Arc::clone(&handle), active_backend_tag.clone()));
                    handle
                });
            // Two legs, one spelling. The operator's queue depth is the
            // resolved setting on the configuration; the environment variable
            // is the deterministic pressure lever the owner smoke uses to make
            // an overflow reproducible without waiting on live scene traffic,
            // and it overrides for that run only.
            let queue_capacity_lever = detector_queue_capacity_lever();
            let detector_work_delay =
                Duration::from_millis(env_u64("VIGIL_DETECTOR_WORK_DELAY_MS").unwrap_or_default());
            let detector_queue: Arc<LatestSegmentQueue<CapturedSegment>> =
                Arc::new(match queue_capacity_lever {
                    // The lever's depth was already published at startup, with
                    // the value it stands in front of named beside it. The
                    // queue built here does not read that publication back: it
                    // re-derives the same depth from the same source, the
                    // process environment, which no longer changes once the
                    // process is up. What matters is that the surface no
                    // longer waits on this line to learn the depth.
                    Some(capacity) => LatestSegmentQueue::new(capacity),
                    None => LatestSegmentQueue::following_the_setting(
                        crate::settings_application::detector_queue_capacity_in_force(
                            config.detector_queue_capacity,
                        ),
                    ),
                });
            let active_stream_generation = Arc::new(AtomicU64::new(0));
            let detector_handle = detector.map(|detector| {
                let config = config.clone();
                let store = store.clone();
                let stats = stats.clone();
                let health = health.clone();
                let shutdown = shutdown.clone();
                let detector_queue = detector_queue.clone();
                let active_stream_generation = active_stream_generation.clone();
                let detection_publisher = detection_publisher.clone();
                let recognition_embedder = recognition_embedder.clone();
                let receipts = receipts.clone();
                let panic_alive = detector_alive.clone();
                let panic_queue = detector_queue.clone();
                #[cfg(feature = "fabric")]
                let fabric_for_detector = fabric_slot.clone();
                // Created lazily on the first segment that actually sees an
                // attached fabric: bring-up may still be in flight when this
                // detector thread starts, and the seam's config comes from the
                // bundle. Until then it stays `None` and no offload is
                // considered — byte-identical to an unenrolled node.
                #[cfg(feature = "fabric")]
                let mut offload_seam: Option<crate::offload_policy::RuntimeOffloadSeam> = None;
                let panic_camera = config.camera_name.clone();
                thread::spawn(move || {
                    let panic_health = health.clone();
                    let panic_stats = stats.clone();
                    let unwind =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                            let mut detector_total = 0_u64;
                            // Test-only deterministic pressure lever, resolved
                            // ONCE at loop start (not re-read per decision):
                            // absent env → ZERO, no hot-path cost.
                            let decision_delay = decision_delay_from_env(env_u64(
                                "VIGIL_DETECTOR_DECISION_DELAY_MS",
                            ));
                            while !shutdown.load(Ordering::SeqCst) {
                                let segment =
                                    match detector_queue.recv_timeout(Duration::from_millis(100)) {
                                        LatestSegmentRecv::Item(segment) => segment,
                                        LatestSegmentRecv::Timeout => continue,
                                        LatestSegmentRecv::Closed => break,
                                    };
                                if segment.stream_generation
                                    < active_stream_generation.load(Ordering::SeqCst)
                                {
                                    println!(
                                        "stale_stream_segment_suppressed=true sequence={}",
                                        segment.sequence
                                    );
                                    crate::workgraph::StageAttempt::begin(
                                        segment
                                            .detection_work
                                            .clone()
                                            .unwrap_or_else(|| segment.envelope.clone()),
                                    )
                                    .finish(
                                        &receipts,
                                        crate::workgraph::WorkDisposition::Dropped,
                                        0,
                                        "stale_stream=true",
                                        &mut receipt_line_sink(&stats),
                                    );
                                    let _ = fs::remove_file(&segment.path);
                                    continue;
                                }
                                detector_total = detector_total.saturating_add(1);
                                stats.update(|stats| {
                                    stats.detector_invocations =
                                        stats.detector_invocations.saturating_add(1);
                                });
                                println!("detector_invocations={detector_total}");
                                sleep_shutdown_aware(&shutdown, detector_work_delay);
                                let detector_started = Instant::now();
                                let detection_started_at = crate::clock::now_utc();
                                // The SAME detection work identity created at enqueue.
                                let detection_work =
                                    segment.detection_work.clone().unwrap_or_else(|| {
                                        segment
                                            .motion_work
                                            .as_ref()
                                            .unwrap_or(&segment.envelope)
                                            .derive(crate::workgraph::STAGE_DETECTION)
                                    });

                                // Offload decision (criterion C2): only
                                // consulted when fabric is configured AND
                                // this source has not opted out of frame
                                // movement (`fabric_allow_frame_offload`,
                                // default on — the addon privacy knob); with
                                // either absent this block never runs, so
                                // behavior stays byte-identical to today
                                // (always local).
                                #[cfg(feature = "fabric")]
                                if let Some(fabric_state) = fabric_for_detector
                                    .get()
                                    .filter(|_| config.fabric_allow_frame_offload)
                                {
                                    let counters = detector_queue.counters();
                                    let lifetime = crate::offload_policy::LifetimeQueueSnapshot {
                                        depth: counters.current_depth,
                                        capacity: detector_queue.capacity() as u64,
                                        queued_total: counters.queued_total,
                                        dropped_total: counters.replaced_dropped_total,
                                        coalesced_total: counters.coalesced_total,
                                        degraded: counters.replaced_dropped_total > 0,
                                        dropped_motion_positive_frames_total: stats
                                            .snapshot()
                                            .dropped_motion_positive_frames,
                                    };
                                    let remotes = fabric_state
                                        .remote_capabilities
                                        .lock()
                                        .expect("remote capabilities lock")
                                        .clone();
                                    let decision = offload_seam
                                        .get_or_insert_with(|| {
                                            crate::offload_policy::RuntimeOffloadSeam::new(
                                                fabric_state.offload_policy_config,
                                            )
                                        })
                                        .decide_for_segment(lifetime, &remotes);
                                    let decision_line =
                                        crate::offload_policy::render_offload_decision_receipt(&decision);
                                    stats.update(|stats| {
                                        crate::runtime_stats::push_recent_receipt(
                                            stats,
                                            decision_line.clone(),
                                        );
                                    });
                                    if let crate::offload_policy::Decision::Offload { remote, .. } =
                                        &decision
                                    {
                                        match try_offload_segment(
                                            fabric_state,
                                            &config,
                                            &segment,
                                            &detection_work,
                                        ) {
                                            Ok(job_id) => {
                                                println!(
                                                    "offload_submitted=true job_id={job_id} remote={} backend={}",
                                                    remote.node_id, remote.backend
                                                );
                                                fabric_state
                                                    .pending
                                                    .track(job_id.clone(), detection_work.clone());
                                                fabric_state
                                                    .pending_segments
                                                    .lock()
                                                    .expect("pending segments lock")
                                                    .insert(
                                                        job_id,
                                                        PendingOffloadSegment {
                                                            segment,
                                                            detection_work: detection_work.clone(),
                                                            detection_started_at,
                                                            config: config.clone(),
                                                            store: store.clone(),
                                                            stats: stats.clone(),
                                                            health: health.clone(),
                                                            detection_publisher: detection_publisher
                                                                .clone(),
                                                            recognition_embedder: recognition_embedder
                                                                .clone(),
                                                            receipts: receipts.clone(),
                                                        },
                                                    );
                                                continue;
                                            }
                                            Err(error) => {
                                                println!(
                                                    "offload_submit_failed=true error={error} falling_back=local"
                                                );
                                                // Fall through: `segment` was
                                                // only borrowed above, so the
                                                // local path below still owns
                                                // it.
                                            }
                                        }
                                    }
                                }

                                // Apply the once-resolved test-only pressure
                                // delay (see `decision_delay` above) on the
                                // LOCAL-inference branch only: it simulates a
                                // slow local detector, so it must never gate
                                // the offload decision above or an offloaded
                                // segment's dispatch — a real slow node
                                // decides quickly and ships work fast; only
                                // its own inference is slow. (Round-5 S3
                                // lesson: sleeping before the decision
                                // throttled the policy to one decision per
                                // delay period and starved offload.) Zero
                                // when the env is unset.
                                if !decision_delay.is_zero() {
                                    sleep_shutdown_aware(&shutdown, decision_delay);
                                }
                                // The analysis rate this pass runs at is the
                                // one in force NOW, not the one that was in
                                // force when this thread started: an owner
                                // whose machine cannot keep up reaches for it
                                // precisely because they cannot afford to
                                // restart the thing watching their property.
                                let output = detector.detect_segment(
                                    &segment.media,
                                    segment.clip_sha256.clone(),
                                    crate::settings_application::detector_sample_frames_in_force(
                                        config.detector_sample_frames,
                                    ),
                                    crate::settings_application::detector_confidence_threshold_in_force(
                                        config.detector_confidence_threshold,
                                    ),
                                );
                                let latency_ms = detector_started.elapsed().as_secs_f64() * 1000.0;
                                stats.update(|stats| {
                                    stats.detector_latency_p50_ms = latency_ms;
                                    stats.detector_latency_p95_ms = latency_ms;
                                    stats.detector_latency_max_ms =
                                        stats.detector_latency_max_ms.max(latency_ms);
                                });
                                match output {
                                    Ok(output) => {
                                        println!("detector_detections={}", output.detections.len());
                                        // The detection result joins back to its exact
                                        // work + recorded backend attempt, or it is
                                        // rejected — never guessed.
                                        let detector_backend =
                                            stats.snapshot().active_detector_backend;
                                        let finished = crate::workgraph::StageAttempt::begin(
                                            detection_work.clone(),
                                        )
                                        .backend(
                                            (!detector_backend.is_empty())
                                                .then_some(detector_backend),
                                        )
                                        .started_at(detection_started_at)
                                        .finish(
                                            &receipts,
                                            crate::workgraph::WorkDisposition::Completed,
                                            output.detections.len() as u64,
                                            &format!(
                                                "detections={} decode_receipt_id={}",
                                                output.detections.len(),
                                                segment
                                                    .decode_receipt_id
                                                    .map(|id| id.to_string())
                                                    .unwrap_or_else(|| "-".to_string())
                                            ),
                                            &mut receipt_line_sink(&stats),
                                        );
                                        // A result that failed its join never becomes
                                        // events — receipted Rejected above, gated here.
                                        if finished.joined {
                                            if let Err(error) = record_or_publish_detected_events(
                                                store.as_ref(),
                                                &segment,
                                                &output,
                                                &detection_work,
                                                &DetectionRecordingContext {
                                                    config: &config,
                                                    stats: &stats,
                                                    health: &health,
                                                    detection_publisher: detection_publisher
                                                        .as_deref(),
                                                    recognition_embedder: recognition_embedder
                                                        .as_deref(),
                                                    receipts: &receipts,
                                                },
                                            ) {
                                                println!("record_detection_failed error={error}");
                                            }
                                        } else {
                                            println!("detection_result_rejected=true");
                                            let _ = fs::remove_file(&segment.path);
                                        }
                                    }
                                    Err(error) => {
                                        println!("detector invocation failed error={error}");
                                        crate::workgraph::StageAttempt::begin(
                                            detection_work.clone(),
                                        )
                                        .started_at(detection_started_at)
                                        .finish(
                                            &receipts,
                                            crate::workgraph::WorkDisposition::Rejected,
                                            0,
                                            &format!("error={error}"),
                                            &mut receipt_line_sink(&stats),
                                        );
                                    }
                                }
                            }
                            for segment in detector_queue.drain() {
                                // Minted detection work never vanishes receipt-less,
                                // even in the shutdown window.
                                crate::workgraph::StageAttempt::begin(
                                    segment
                                        .detection_work
                                        .clone()
                                        .unwrap_or_else(|| segment.envelope.clone()),
                                )
                                .finish(
                                    &receipts,
                                    crate::workgraph::WorkDisposition::Dropped,
                                    0,
                                    "shutdown=true",
                                    &mut receipt_line_sink(&stats),
                                );
                                let _ = fs::remove_file(&segment.path);
                            }
                        }));
                    if unwind.is_err() {
                        println!("detector_thread_panicked=true");
                        panic_alive.store(false, Ordering::SeqCst);
                        panic_queue.close();
                        panic_stats.update(|stats| {
                            stats.ingest_signal = "panic".to_string();
                            mark_health_condition(&mut stats.health, "ingest_failed");
                        });
                        panic_health.set(HealthStatus::IngestFailed, "detector thread panicked");
                        panic_health
                            .set_camera(&panic_camera, CameraCondition::IngestFailed);
                    }
                })
            });
            let mut decoded_total = 0_u64;
            // The active decode backend for this stream, as observed from the
            // latest selection/fallback receipt. Receipt attribution reads it.
            let current_decode_backend: Arc<std::sync::Mutex<Option<String>>> =
                Arc::new(std::sync::Mutex::new(None));
            let mut reconnect_pending = false;
            let mut stream_generation = active_stream_generation.load(Ordering::SeqCst);
            let mut last_stationary_detector_scan: Option<Instant> = None;
            let stationary_detector_interval =
                Duration::from_secs(config.detector_stationary_interval_secs);
            // Resolved from the store with the rest of this camera's settings:
            // how long vigil waits before trying a dropped stream again is a
            // thing an operator sets, not a thing the environment whispers.
            let retry_initial_ms = config.rtsp_retry_initial_ms;
            let retry_max_ms = config.rtsp_retry_max_ms.max(retry_initial_ms);
            let mut retry_delay_ms = retry_initial_ms;
            // This pipeline, registered where a live decode change can reach
            // it: which decode path a stream runs is decided when its session
            // opens, so a change reaches it by having it open a new one.
            crate::live_backends::register_live_stream(config.hardware_decoding);
            while !shutdown.load(Ordering::SeqCst) {
                // Per-camera disable: gate on the enabled flag without exiting the
                // thread so that a subsequent enable resumes ingest immediately.
                if !enabled.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_secs(5));
                    continue;
                }
                let capture_frames = env_u64("VIGIL_CAPTURE_FRAMES").unwrap_or(48).max(1) as usize;
                let receipt_stats = stats.clone();
                let receipt_accel = accel.clone();
                let receipt_backend = current_decode_backend.clone();
                // The decode answer this session opens under, held so the end
                // of the session can be told apart from a stream fault: a
                // pipeline that stopped because the operator changed the
                // backend reconnects at once, with no fault recorded against
                // the camera and no retry delay to sit through.
                let decode_epoch_at_open = crate::live_backends::decode_selection_epoch();
                let capture_result = media_pipeline::capture_rtsp_segments(
                    &rtsp_source,
                    capture_frames,
                    shutdown.clone(),
                    media_pipeline::CaptureDecodeOptions {
                        stream_id: crate::workgraph::StreamId::new(config.camera_name.clone()),
                        stream_epoch: stream_generation,
                        decode_epoch: decode_epoch_at_open,
                        hardware_decoding: crate::live_backends::hardware_decoding_in_force(
                            config.hardware_decoding,
                        ),
                    },
                    move |receipt| {
                        if receipt_accel.should_log(&receipt) {
                            println!(
                                "decode_backend_selected stream={} attempted={} active={} hardware={} status={} failure={}",
                                receipt
                                    .stream_id
                                    .as_ref()
                                    .map(crate::workgraph::StreamId::as_str)
                                    .unwrap_or(""),
                                receipt.attempted_backend,
                                receipt.active_backend,
                                receipt.hardware_accelerated,
                                receipt.probe_status.as_str(),
                                receipt.failure_code.as_str()
                            );
                        }
                        if let Ok(mut backend) = receipt_backend.lock() {
                            *backend = Some(receipt.active_backend.clone());
                        }
                        receipt_stats.update(|stats| {
                            if let Some(stream) = receipt.stream_id.as_ref() {
                                stats.active_decoder.insert(
                                    stream.as_str().to_string(),
                                    receipt.active_backend.clone(),
                                );
                                // Same full fixed-format block doctor and
                                // /health render — stats stays at receipt
                                // parity with the other operator surfaces.
                                stats.decode_receipt_blocks.insert(
                                    stream.as_str().to_string(),
                                    crate::acceleration::render_receipt_block(&receipt),
                                );
                            }
                            stats.decode_acceleration = compact_acceleration_summary(&receipt);
                        });
                        receipt_accel.record(receipt);
                    },
                    || {
                        println!("rtsp opened url={rtsp_log_url}");
                        println!("rtsp play observed url={rtsp_log_url}");
                        Ok(())
                    },
                    |media| {
                        let mut segment = build_captured_segment(
                            media,
                            &config.data_dir,
                            &config.camera_name,
                            stream_generation,
                        )?;
                        let frames = segment.frames;
                        let fps = segment.fps;
                        let motion_positive = segment.motion_positive_frames;
                        decoded_total = decoded_total.saturating_add(frames);
                        stats.update(|stats| {
                            stats.frames_received = stats.frames_received.saturating_add(frames);
                            stats.motion_positive_frames =
                                stats.motion_positive_frames.saturating_add(motion_positive);
                            stats.stream_fps = fps;
                            stats.processing_lag_bound_ms = 1000.0_f64 / fps.max(1.0);
                            stats.ingest_signal = "ok".to_string();
                            if reconnect_pending {
                                stats.stream_reconnects = stats.stream_reconnects.saturating_add(1);
                            }
                        });
                        if detector_alive.load(Ordering::SeqCst) {
                            set_ready_unless_latched_fault(&health, "RTSP ingest active");
                            health.set_camera(&config.camera_name, CameraCondition::Watching);
                            // The stats surface follows the live health state
                            // through recovery: a condition latched into the
                            // stats string during an outage must not outlive
                            // the outage, or /health and stats tell two
                            // different stories about the same process.
                            if matches!(health.snapshot().0, HealthStatus::Ready) {
                                stats.update(|stats| stats.health = "ready".to_string());
                            }
                        } else {
                            // Decode alone is not a working pipeline: a dead or
                            // never-loaded detector keeps health failed instead of
                            // being masked by capture activity.
                            health.set(
                                HealthStatus::IngestFailed,
                                "detector unavailable (capture active)",
                            );
                            health.set_camera(&config.camera_name, CameraCondition::IngestFailed);
                        }
                        reconnect_pending = false;
                        retry_delay_ms = retry_initial_ms;
                        // Record the REAL decode stage attempt; the segment
                        // carries its recorded receipt id into the graph.
                        let decode_backend = current_decode_backend
                            .lock()
                            .ok()
                            .and_then(|backend| backend.clone());
                        let decode_finished =
                            crate::workgraph::StageAttempt::begin(segment.envelope.clone())
                                .backend(decode_backend)
                                .started_at(segment.envelope.received_at)
                                .finish(
                                    &receipts,
                                    crate::workgraph::WorkDisposition::Completed,
                                    frames,
                                    "",
                                    &mut receipt_line_sink(&stats),
                                );
                        segment.decode_receipt_id = Some(decode_finished.receipt_id);

                        // The stationary-scan interval in force now, for the
                        // same reason the sample rate is read per segment.
                        let decision = detector_segment_decision(
                            motion_positive,
                            Duration::from_secs(
                                crate::settings_application::detector_stationary_interval_in_force(
                                    stationary_detector_interval.as_secs(),
                                ),
                            ),
                            last_stationary_detector_scan.map(|last| last.elapsed()),
                        );
                        let motion_work = segment.envelope.derive(crate::workgraph::STAGE_MOTION);
                        if decision == DetectorSegmentDecision::SuppressMotionGate {
                            println!("motion_gate_suppressed_segment=true");
                            crate::workgraph::StageAttempt::begin(motion_work)
                                .started_at(segment.observed_at)
                                .finish(
                                    &receipts,
                                    crate::workgraph::WorkDisposition::Coalesced,
                                    0,
                                    "suppressed=true",
                                    &mut receipt_line_sink(&stats),
                                );
                            let _ = fs::remove_file(&segment.path);
                        } else if detector_handle.is_some() {
                            // The motion stage emits ONE result: the gated
                            // segment, which detection consumes as its parent.
                            crate::workgraph::StageAttempt::begin(motion_work.clone())
                                .started_at(segment.observed_at)
                                .finish(
                                    &receipts,
                                    crate::workgraph::WorkDisposition::Completed,
                                    1,
                                    &format!("motion_positive_frames={motion_positive}"),
                                    &mut receipt_line_sink(&stats),
                                );
                            // Detection work is minted HERE, once, and rides the
                            // queue: every later receipt for it — completed,
                            // rejected, failed, stale, replaced — shares this
                            // identity. The decoded media is a contributing
                            // parent (multi-parent provenance).
                            segment.detection_work = Some(motion_work.derive_with_contributor(
                                crate::workgraph::STAGE_DETECTION,
                                segment.envelope.work_id,
                            ));
                            segment.motion_work = Some(motion_work);
                            if let DetectorSegmentDecision::Enqueue {
                                stationary_scan: true,
                            } = decision
                            {
                                last_stationary_detector_scan = Some(Instant::now());
                                println!("stationary_detector_scan=true");
                            }
                            match detector_queue.push_latest(segment) {
                                Ok(Some(dropped_segment)) => {
                                    // The replaced segment's detection never
                                    // runs: receipt it as Dropped, visibly.
                                    crate::workgraph::StageAttempt::begin(
                                        dropped_segment
                                            .detection_work
                                            .clone()
                                            .unwrap_or_else(|| dropped_segment.envelope.clone()),
                                    )
                                    .started_at(dropped_segment.observed_at)
                                    .finish(
                                        &receipts,
                                        crate::workgraph::WorkDisposition::Dropped,
                                        0,
                                        "replaced_by_newer=true",
                                        &mut receipt_line_sink(&stats),
                                    );
                                    let dropped = dropped_segment.motion_positive_frames.max(1);
                                    stats.update(|stats| {
                                        stats.dropped_motion_positive_frames = stats
                                            .dropped_motion_positive_frames
                                            .saturating_add(dropped);
                                        stats.processing_lag_ms = stats
                                            .processing_lag_ms
                                            .max(stats.processing_lag_bound_ms + 1.0);
                                        mark_health_condition(
                                            &mut stats.health,
                                            "keep-pace-failed",
                                        );
                                    });
                                    health.set(
                                        HealthStatus::KeepPaceFailed,
                                        "detector queue fell behind",
                                    );
                                    health.set_camera(
                                        &config.camera_name,
                                        CameraCondition::KeepPaceFailed,
                                    );
                                    println!(
                                        "detector_queue_replaced_pending_segment=true dropped_sequence={}",
                                        dropped_segment.sequence
                                    );
                                    let _ = fs::remove_file(&dropped_segment.path);
                                }
                                Ok(None) => {}
                                Err(segment) => {
                                    println!("detector queue disconnected");
                                    crate::workgraph::StageAttempt::begin(
                                        segment
                                            .detection_work
                                            .clone()
                                            .unwrap_or_else(|| segment.envelope.clone()),
                                    )
                                    .finish(
                                        &receipts,
                                        crate::workgraph::WorkDisposition::Dropped,
                                        0,
                                        "queue_closed=true",
                                        &mut receipt_line_sink(&stats),
                                    );
                                    let _ = fs::remove_file(&segment.path);
                                }
                            }
                        } else {
                            // The motion gate DID run, and the detection this
                            // segment deserved never will: both receipted, never
                            // silent (latched ingest-failed health already marks
                            // the pipeline degraded).
                            println!("detector_unavailable_dropped_segment=true");
                            let never_run_detection = motion_work.derive_with_contributor(
                                crate::workgraph::STAGE_DETECTION,
                                segment.envelope.work_id,
                            );
                            crate::workgraph::StageAttempt::begin(motion_work)
                                .started_at(segment.observed_at)
                                .finish(
                                    &receipts,
                                    crate::workgraph::WorkDisposition::Completed,
                                    1,
                                    &format!("motion_positive_frames={motion_positive}"),
                                    &mut receipt_line_sink(&stats),
                                );
                            crate::workgraph::StageAttempt::begin(never_run_detection).finish(
                                &receipts,
                                crate::workgraph::WorkDisposition::Dropped,
                                0,
                                "detector_unavailable=true",
                                &mut receipt_line_sink(&stats),
                            );
                            let _ = fs::remove_file(&segment.path);
                        }
                        // Rendered AFTER the enqueue/replace outcome so the
                        // stats line reflects THIS segment's queue effect.
                        stats.update(|stats| {
                        let counters = detector_queue.counters();
                        stats.detector_queue = format!(
                            "depth={} capacity={} queued={} dropped={} coalesced={} degraded={}",
                            counters.current_depth,
                            detector_queue.capacity(),
                            counters.queued_total,
                            counters.replaced_dropped_total,
                            counters.coalesced_total,
                            counters.replaced_dropped_total > 0
                        );
                    });
                        println!("decoded_frames={decoded_total}");
                        Ok(())
                    },
                );
                let session_end = classify_capture_end(
                    capture_result.is_err(),
                    shutdown.load(Ordering::SeqCst),
                    decode_epoch_at_open,
                    crate::live_backends::decode_selection_epoch(),
                );
                match session_end {
                    CaptureSessionEnd::ShuttingDown => break,
                    CaptureSessionEnd::PlannedDecodeReselect => {
                        // Not a fault: the session ended because the operator
                        // named a different decode path, and the pipeline is
                        // reopened immediately so the selection runs afresh
                        // under it. A new stream epoch, because the units that
                        // follow are decoded by a different backend than the
                        // ones before them.
                        println!("decode_backend_reconnect camera={}", config.camera_name);
                        reconnect_pending = true;
                        stream_generation =
                            active_stream_generation.fetch_add(1, Ordering::SeqCst) + 1;
                    }
                    CaptureSessionEnd::Quiescent => {}
                    CaptureSessionEnd::Fault => {
                        let error = capture_result.err().unwrap_or_default();
                        println!("rtsp probe failed url={rtsp_log_url} error={error}");
                        println!("decoded_frames={decoded_total}");
                        stats.update(|stats| {
                            stats.stream_drops = stats.stream_drops.saturating_add(1);
                            stats.ingest_signal = "decode-error".to_string();
                            mark_health_condition(&mut stats.health, "ingest_failed");
                        });
                        reconnect_pending = true;
                        stream_generation =
                            active_stream_generation.fetch_add(1, Ordering::SeqCst) + 1;
                        health.set(HealthStatus::IngestFailed, "RTSP ingest failed");
                        health.set_camera(&config.camera_name, CameraCondition::IngestFailed);
                        println!("rtsp_retry_after_ms={retry_delay_ms}");
                        sleep_shutdown_aware(&shutdown, Duration::from_millis(retry_delay_ms));
                        retry_delay_ms = retry_delay_ms.saturating_mul(2).min(retry_max_ms);
                    }
                }
            }
            detector_queue.close();
            if let Some(handle) = detector_handle {
                let _ = handle.join();
            }
        }));
        if unwind.is_err() {
            println!("camera_thread_panicked=true");
            panic_stats.update(|stats| {
                stats.ingest_signal = "panic".to_string();
                mark_health_condition(&mut stats.health, "ingest_failed");
            });
            panic_health.set(HealthStatus::IngestFailed, "camera thread panicked");
            panic_health.set_camera(&panic_camera, CameraCondition::IngestFailed);
        }
    })
}

/// The one adapter between the work graph's operator lines and the stats
/// surface: every finished stage attempt lands in the bounded
/// recent-work-receipts window through here.
fn receipt_line_sink(stats: &RuntimeStatsState) -> impl FnMut(String) + '_ {
    move |line: String| {
        stats.update(|stats| {
            crate::runtime_stats::push_recent_receipt(stats, line.clone());
        });
    }
}

fn sleep_shutdown_aware(shutdown: &AtomicBool, duration: Duration) {
    let started = Instant::now();
    while !shutdown.load(Ordering::SeqCst) && started.elapsed() < duration {
        let remaining = duration.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

fn build_captured_segment(
    media: media_pipeline::DecodedVideoSegment,
    data_dir: &Path,
    camera_name: &str,
    stream_generation: u64,
) -> Result<CapturedSegment, String> {
    let staging_dir = data_dir.join("staging");
    // Describing a segment names where its clip WOULD be kept; it does not
    // prepare the place. A run that is not allowed to record must leave no
    // trace of a recording, and a clip directory standing empty under the data
    // root is exactly that trace — so the directory is created where a clip is
    // actually finalized, by the run that is allowed to write one.
    let clip_dir = data_dir.join("clips");
    // The sensitivity in force for THIS camera right now: an operator who turns
    // one camera down is answered on that camera's next segment, not at the
    // next restart, and every other camera keeps running what it inherits.
    let motion_positive_frames = media_pipeline::motion_gate(
        &media,
        crate::settings_application::motion_sensitivity_in_force_for(camera_name),
    )
    .motion_positive_frames
    .min(media.frame_count());
    let observed_at = media.observed_at.unwrap_or_else(crate::clock::now_utc);
    let stamp = startup_epoch();
    let sequence = CAPTURE_SEQUENCE.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or_default();
    let camera_slug = camera_slug(camera_name);
    let file_name = format!("{camera_slug}-event-{stamp}-{sequence}-{nanos}.mp4");
    let staging_path = staging_dir.join(&file_name);
    let final_path = clip_dir.join(&file_name);
    let clip_sha256 = encoded_clip_sha256(&media);
    let decoded_frames_sha256 = decoded_frames_sha256(&media);
    let frames = media.frame_count();
    if frames == 0 {
        let _ = fs::remove_file(&staging_path);
        return Err(format!(
            "recorded RTSP segment {} has no decodable frames",
            staging_path.display()
        ));
    }
    let envelope = crate::workgraph::WorkEnvelope {
        work_id: crate::workgraph::WorkId::generate(),
        parent_work_id: None,
        contributing_work_ids: Vec::new(),
        stage: crate::workgraph::StageId::new(crate::workgraph::STAGE_DECODED_MEDIA),
        stream_id: crate::workgraph::StreamId::new(camera_name.to_string()),
        media_item: Some(crate::workgraph::MediaItemId::Segment {
            segment_sequence: sequence,
        }),
        ordering: crate::workgraph::WorkOrdering {
            stream_epoch: stream_generation,
            stream_sequence: sequence,
        },
        observed_at: Some(observed_at),
        received_at: crate::clock::now_utc(),
        priority: if motion_positive_frames > 0 {
            crate::workgraph::WorkPriority::MOTION
        } else {
            crate::workgraph::WorkPriority::BACKGROUND
        },
        deadline: crate::workgraph::WorkDeadline::None,
        schema_version: crate::workgraph::WORK_ENVELOPE_SCHEMA_VERSION,
    };
    Ok(CapturedSegment {
        envelope,
        decode_receipt_id: None,
        motion_work: None,
        detection_work: None,
        path: staging_path,
        final_path,
        source_ref: format!("vigil-edge:clip/{file_name}"),
        sequence,
        stream_generation,
        frames,
        fps: media.fps,
        mime_type: "video/mp4".to_string(),
        clip_sha256,
        decoded_frames_sha256,
        motion_positive_frames,
        observed_at,
        media,
    })
}

fn camera_slug(camera_name: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in camera_name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "camera".to_string()
    } else {
        slug
    }
}

fn finalize_clip(
    segment: &CapturedSegment,
    stats: &RuntimeStatsState,
    health: &HealthState,
) -> Result<(), String> {
    // The stream this segment came off is the camera whose clip could not be
    // written, so a failure here reports against that camera and no other.
    let camera = segment.envelope.stream_id.as_str();
    if let Err(error) =
        media_pipeline::write_browser_playable_mp4_clip(&segment.media, &segment.path)
    {
        return Err(clip_write_failure(
            stats,
            health,
            camera,
            Some(&segment.path),
            format!("write staging clip {}: {error}", segment.path.display()),
        ));
    }
    if let Some(parent) = segment.final_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            clip_write_failure(
                stats,
                health,
                camera,
                None,
                format!("create clip dir {}: {error}", parent.display()),
            )
        })?;
    }
    if let Err(error) = fs::copy(&segment.path, &segment.final_path) {
        return Err(clip_write_failure(
            stats,
            health,
            camera,
            Some(&segment.final_path),
            format!(
                "write durable clip {}: {error}",
                segment.final_path.display()
            ),
        ));
    }
    let file = match fs::File::open(&segment.final_path) {
        Ok(file) => file,
        Err(error) => {
            return Err(clip_write_failure(
                stats,
                health,
                camera,
                Some(&segment.final_path),
                format!(
                    "open durable clip {}: {error}",
                    segment.final_path.display()
                ),
            ));
        }
    };
    if let Err(error) = file.sync_all() {
        return Err(clip_write_failure(
            stats,
            health,
            camera,
            Some(&segment.final_path),
            format!(
                "sync durable clip {}: {error}",
                segment.final_path.display()
            ),
        ));
    }
    if let Some(parent) = segment.final_path.parent() {
        let directory = match fs::File::open(parent) {
            Ok(directory) => directory,
            Err(error) => {
                return Err(clip_write_failure(
                    stats,
                    health,
                    camera,
                    Some(&segment.final_path),
                    format!("open clip directory {}: {error}", parent.display()),
                ));
            }
        };
        if let Err(error) = directory.sync_all() {
            return Err(clip_write_failure(
                stats,
                health,
                camera,
                Some(&segment.final_path),
                format!("sync clip directory {}: {error}", parent.display()),
            ));
        }
    }
    Ok(())
}

fn clip_write_failure(
    stats: &RuntimeStatsState,
    health: &HealthState,
    camera: &str,
    partial_final_path: Option<&Path>,
    message: impl Into<String>,
) -> String {
    stats.update(|stats| {
        stats.clip_write_failures = stats.clip_write_failures.saturating_add(1);
        mark_health_condition(&mut stats.health, "disk-full");
    });
    health.set(HealthStatus::DiskFull, "clip write failed");
    health.set_camera(camera, CameraCondition::ClipWriteFailed);
    if let Some(path) = partial_final_path {
        let _ = fs::remove_file(path);
    }
    message.into()
}

// This wrapper's own callers are enumerated, reviewed test-only pressure
// levers (`environment_read_surface.baseline.txt`), not an ad-hoc read.
#[allow(clippy::disallowed_methods)]
fn env_is(key: &str, expected: &str) -> bool {
    std::env::var(key).is_ok_and(|value| value == expected)
}

/// The compact operator stats line: an operator reads `active:none` as
/// "nothing is active", so a receipt with no failure renders just its
/// status; the failure code appears only when there is one to explain.
fn compact_acceleration_summary(receipt: &crate::acceleration::AccelerationReceipt) -> String {
    if receipt.failure_code == crate::acceleration::FailureCode::None {
        receipt.probe_status.as_str().to_string()
    } else {
        format!(
            "{}:{}",
            receipt.probe_status.as_str(),
            receipt.failure_code.as_str()
        )
    }
}

fn mark_health_condition(current: &mut String, condition: &str) {
    if current.is_empty() || current == "ready" {
        *current = condition.to_string();
        return;
    }
    if !current.split(',').any(|part| part == condition) {
        current.push(',');
        current.push_str(condition);
    }
}

/// Record that the pipeline is doing its work, without overwriting a fault
/// that is still standing and without promoting a run to a state it never
/// earned.
///
/// What "working" means is the run's OWN baseline: an ordinary run is ready, a
/// run with no store behind it is running unmanaged. Decoding a frame proves
/// the cameras are working; it proves nothing about a store that failed to
/// open, so it must not answer for one. A later independent fault still moves
/// the status to whatever that fault is, and when the fault clears this brings
/// the run back to its own baseline rather than to a readiness it never had.
fn set_ready_unless_latched_fault(health: &HealthState, detail: &'static str) {
    let (status, _) = health.snapshot();
    if matches!(
        status,
        HealthStatus::DiskFull | HealthStatus::KeepPaceFailed
    ) {
        return;
    }
    health.set(health.healthy_baseline(), detail);
}

fn decoded_frames_sha256(media: &media_pipeline::DecodedVideoSegment) -> String {
    let mut hasher = Sha256::new();
    for frame in &media.frames {
        hasher.update(frame.width.to_le_bytes());
        hasher.update(frame.height.to_le_bytes());
        hasher.update(&frame.rgb);
    }
    format!("{:x}", hasher.finalize())
}

fn encoded_clip_sha256(media: &media_pipeline::DecodedVideoSegment) -> String {
    let mut hasher = Sha256::new();
    for unit in &media.encoded_units {
        hasher.update(unit);
    }
    format!("{:x}", hasher.finalize())
}

// This wrapper's own callers are enumerated, reviewed test-only pressure
// levers (`environment_read_surface.baseline.txt`), not an ad-hoc read.
#[allow(clippy::disallowed_methods)]
fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.parse().ok()
}

/// The one documented environment lever over a behavior setting: the
/// deterministic queue-pressure lever the owner smoke uses to make an overflow
/// reproducible without waiting on live scene traffic. It overrides for that
/// run only.
pub(crate) const DETECTOR_QUEUE_CAPACITY_VARIABLE: &str = "VIGIL_DETECTOR_QUEUE_CAPACITY";

/// The queue depth this process runs at when the lever supplied one. Read from
/// one place so the depth the surface reports and the depth the queue is built
/// at cannot diverge.
fn detector_queue_capacity_lever() -> Option<usize> {
    env_u64(DETECTOR_QUEUE_CAPACITY_VARIABLE).map(|capacity| capacity.max(1) as usize)
}

/// Map a raw `VIGIL_DETECTOR_DECISION_DELAY_MS` value to a per-detection-
/// decision delay. This is a TEST-ONLY deterministic pressure lever, fenced
/// exactly like `VIGIL_DETECTOR_QUEUE_CAPACITY`: env-only, no options-schema
/// entry, no CLI flag — it exists so the owner smoke's S1/S3 pressure windows
/// are reproducible without depending on live scene traffic, never as an
/// operator control. `None` (env unset) → `Duration::ZERO`, so there is no
/// added latency on the hot path in normal operation.
fn decision_delay_from_env(raw: Option<u64>) -> Duration {
    raw.map(Duration::from_millis).unwrap_or(Duration::ZERO)
}

fn maybe_crash_after_startup_node(node: &str) {
    if env_is("VIGIL_FAULT_CRASH_AFTER_STARTUP_NODE", node) {
        std::process::exit(3);
    }
}

/// The detector stage queue: the work-graph bounded queue (keep-newest,
/// visible counters) behind the runtime's historical name and API.
struct LatestSegmentQueue<T> {
    inner: crate::workgraph::BoundedStageQueue<T>,
    /// Whether this queue's depth follows the operator's setting as it changes.
    follows_setting: bool,
}

enum LatestSegmentRecv<T> {
    Item(T),
    Timeout,
    Closed,
}

impl<T> LatestSegmentQueue<T> {
    fn new(capacity: usize) -> Self {
        Self {
            inner: crate::workgraph::BoundedStageQueue::new(capacity),
            follows_setting: false,
        }
    }

    /// A queue whose depth is the operator's setting, followed as it changes.
    /// The deterministic pressure lever builds the fixed-depth shape instead:
    /// it overrides the setting for one run, so a queue that kept re-reading
    /// the setting would undo the lever on the first push.
    fn following_the_setting(capacity: usize) -> Self {
        Self {
            inner: crate::workgraph::BoundedStageQueue::new(capacity),
            follows_setting: true,
        }
    }

    fn push_latest(&self, segment: T) -> Result<Option<T>, T> {
        if self.follows_setting {
            // Accepting work is where a new depth is taken on, and taking it on
            // is what makes it the depth this queue is running at. Until then
            // the queue is holding work under the bound it was given, and the
            // surface says so rather than reporting a typed number as running.
            let requested = crate::settings_application::detector_queue_capacity_requested(
                self.inner.capacity(),
            );
            if requested != self.inner.capacity() {
                self.inner.set_capacity(requested);
                crate::settings_application::bring_into_force(
                    crate::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING,
                    crate::settings_model::SettingValue::Int(requested as i64),
                );
            }
        }
        self.inner.push_latest(segment)
    }

    fn recv_timeout(&self, timeout: Duration) -> LatestSegmentRecv<T> {
        match self.inner.recv_timeout(timeout) {
            crate::workgraph::QueueRecv::Item(segment) => LatestSegmentRecv::Item(segment),
            crate::workgraph::QueueRecv::Timeout => LatestSegmentRecv::Timeout,
            crate::workgraph::QueueRecv::Closed => LatestSegmentRecv::Closed,
        }
    }

    fn counters(&self) -> crate::workgraph::QueueCounters {
        self.inner.counters()
    }

    fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    fn close(&self) {
        self.inner.close();
    }

    fn drain(&self) -> Vec<T> {
        self.inner.drain()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectorSegmentDecision {
    Enqueue { stationary_scan: bool },
    SuppressMotionGate,
}

fn detector_segment_decision(
    motion_positive_frames: u64,
    stationary_interval: Duration,
    elapsed_since_last_stationary_scan: Option<Duration>,
) -> DetectorSegmentDecision {
    if motion_positive_frames > 0 {
        return DetectorSegmentDecision::Enqueue {
            stationary_scan: false,
        };
    }
    if stationary_interval.is_zero() {
        return DetectorSegmentDecision::SuppressMotionGate;
    }
    match elapsed_since_last_stationary_scan {
        None => DetectorSegmentDecision::Enqueue {
            stationary_scan: true,
        },
        Some(elapsed) if elapsed >= stationary_interval => DetectorSegmentDecision::Enqueue {
            stationary_scan: true,
        },
        Some(_) => DetectorSegmentDecision::SuppressMotionGate,
    }
}

pub(crate) struct CapturedSegment {
    /// The decoded_media work envelope this segment IS. Downstream stages
    /// derive their work from it; receipts join back to it.
    envelope: crate::workgraph::WorkEnvelope,
    /// Receipt id of the RECORDED decode stage attempt (set when the decode
    /// receipt is recorded, before the segment enters the graph).
    decode_receipt_id: Option<crate::workgraph::ReceiptId>,
    /// The motion stage work this segment passed through before detection:
    /// detection derives FROM motion, which derives from decoded media.
    motion_work: Option<crate::workgraph::WorkEnvelope>,
    /// The detection work item this segment IS once enqueued — created
    /// BEFORE enqueue so every detection receipt (completed, rejected,
    /// failed, stale, replaced) carries the SAME work identity.
    detection_work: Option<crate::workgraph::WorkEnvelope>,
    pub(crate) path: PathBuf,
    final_path: PathBuf,
    source_ref: String,
    sequence: u64,
    stream_generation: u64,
    frames: u64,
    pub(crate) fps: f64,
    mime_type: String,
    pub(crate) clip_sha256: String,
    pub(crate) decoded_frames_sha256: String,
    motion_positive_frames: u64,
    observed_at: chrono::DateTime<chrono::Utc>,
    pub(crate) media: media_pipeline::DecodedVideoSegment,
}

struct MemoryNodes {
    context_id: context_graph::ContextId,
    camera_id: context_graph::EntityId,
    decision_id: context_graph::DecisionId,
    intention_id: context_graph::IntentionId,
}

fn maintain_runtime_memory(
    store: &Store,
    config: &config::RuntimeConfig,
    rtsp_url: &str,
) -> Result<MemoryNodes, String> {
    let context = get_or_create_context(store, &config.site_name)?;
    let camera = get_or_create_camera(store, context.id, &config.camera_name, rtsp_url)?;
    let intention = get_or_create_intention(store, context.id, &config.camera_name)?;
    let decision = get_or_create_decision(store, config, context.id, camera.id, intention.id)?;
    maybe_crash_after_startup_node("detector-decision");
    Ok(MemoryNodes {
        context_id: context.id,
        camera_id: camera.id,
        decision_id: decision.id,
        intention_id: intention.id,
    })
}

/// Everything a detection needs recording or publishing THROUGH, as opposed to
/// the detection itself: this camera's configuration, where counters and health
/// go, the outbound channel, the recognizer, and the receipt log. Grouped
/// because they travel together for the life of a camera thread and change per
/// camera, never per detection — and because a ten-argument call is a place for
/// two arguments to swap silently.
pub(crate) struct DetectionRecordingContext<'a> {
    pub(crate) config: &'a config::RuntimeConfig,
    pub(crate) stats: &'a RuntimeStatsState,
    pub(crate) health: &'a HealthState,
    pub(crate) detection_publisher: Option<&'a dyn crate::site_channel::DetectionChannel>,
    pub(crate) recognition_embedder: Option<&'a dyn context_graph::Embedder>,
    pub(crate) receipts: &'a crate::workgraph::StageReceiptLog,
}

/// What happens to a detection, whichever kind of run made it.
///
/// With a store behind it, the detection becomes recorded memory with a clip
/// beside it and a published fact naming that observation. With no store, the
/// detection is still published — someone watching a property has to be told
/// what is happening — and nothing is written: no clip is finalized, no
/// observation exists, and the fact says as much rather than naming a record
/// nobody could look up.
pub(crate) fn record_or_publish_detected_events(
    store: Option<&Store>,
    segment: &CapturedSegment,
    output: &yolox_detector::DetectorOutput,
    detection_work: &crate::workgraph::WorkEnvelope,
    context: &DetectionRecordingContext<'_>,
) -> Result<(), String> {
    match store {
        Some(store) => record_detected_events(store, segment, output, detection_work, context),
        None => {
            publish_unmanaged_detected_events(
                context.config,
                segment,
                output,
                context.stats,
                context.detection_publisher,
            );
            Ok(())
        }
    }
}

/// Publish the detections of one segment on a run with no store behind it.
///
/// Recording is one of the four capabilities the degraded contract gives up,
/// so the captured segment is dropped rather than finalized into a clip, and
/// the published fact carries no observation identifier: there is no
/// observation, and inventing one would hand Home Assistant a reference that
/// resolves to nothing the moment anyone follows it.
fn publish_unmanaged_detected_events(
    config: &config::RuntimeConfig,
    segment: &CapturedSegment,
    output: &yolox_detector::DetectorOutput,
    stats: &RuntimeStatsState,
    detection_publisher: Option<&dyn crate::site_channel::DetectionChannel>,
) {
    let _ = fs::remove_file(&segment.path);
    if output.detections.is_empty() {
        return;
    }
    stats.update(|stats| {
        stats.detections_emitted = stats.detections_emitted.saturating_add(1);
    });
    let Some(channel) = detection_publisher else {
        return;
    };
    for detection in output.detections.iter().take(1) {
        channel.publish_detection(crate::site_channel::DetectionFact {
            observation_id: String::new(),
            camera_id: camera_slug(&config.camera_name),
            camera_name: config.camera_name.clone(),
            object_class: detection.class_name.clone(),
            confidence: detection.confidence,
            timestamp_ms: segment.observed_at.timestamp_millis(),
            evidence_ref: segment.source_ref.clone(),
            snapshot_ref: String::new(),
            entity_name: None,
            match_score: None,
        });
    }
}

pub(crate) fn record_detected_events(
    store: &Store,
    segment: &CapturedSegment,
    output: &yolox_detector::DetectorOutput,
    detection_work: &crate::workgraph::WorkEnvelope,
    context: &DetectionRecordingContext<'_>,
) -> Result<(), String> {
    // Destructured rather than threaded field by field: every one of these is a
    // shared reference, so this is the same set of bindings the body used when
    // they arrived as arguments.
    let DetectionRecordingContext {
        config,
        stats,
        health,
        detection_publisher,
        recognition_embedder,
        receipts,
    } = *context;
    if output.detections.is_empty() {
        let _ = fs::remove_file(&segment.path);
        return Ok(());
    }
    let rtsp_url = config
        .rtsp_url
        .as_deref()
        .map(media_pipeline::redact_rtsp_url_for_persistence)
        .unwrap_or_default();
    let nodes = match maintain_runtime_memory(store, config, &rtsp_url) {
        Ok(nodes) => nodes,
        Err(error) => {
            let _ = fs::remove_file(&segment.path);
            return Err(error);
        }
    };
    if duplicate_detection_seen(store, segment, nodes.decision_id, nodes.context_id) {
        println!(
            "duplicate_segment_suppressed=true sequence={}",
            segment.sequence
        );
        let _ = fs::remove_file(&segment.path);
        return Ok(());
    }
    stats.update(|stats| {
        stats.detections_emitted = stats.detections_emitted.saturating_add(1);
    });
    if let Err(error) = finalize_clip(segment, stats, health) {
        crate::workgraph::StageAttempt::derive(
            &segment.envelope,
            crate::workgraph::STAGE_CLIP_EVIDENCE,
        )
        .started_at(segment.envelope.received_at)
        .finish(
            receipts,
            crate::workgraph::WorkDisposition::Rejected,
            0,
            &format!("error={error}"),
            &mut receipt_line_sink(stats),
        );
        let _ = fs::remove_file(&segment.path);
        return Err(error);
    }
    crate::workgraph::StageAttempt::derive(
        &segment.envelope,
        crate::workgraph::STAGE_CLIP_EVIDENCE,
    )
    .started_at(segment.observed_at)
    .finish(
        receipts,
        crate::workgraph::WorkDisposition::Completed,
        1,
        "",
        &mut receipt_line_sink(stats),
    );
    let _ = fs::remove_file(&segment.path);
    for detection in output.detections.iter().take(1) {
        match record_one_event(store, &nodes, detection, output, segment, config) {
            Ok((observation_id, snapshot_ref)) => {
                stats.update(|stats| {
                    stats.observations_written = stats.observations_written.saturating_add(1);
                });
                set_ready_unless_latched_fault(health, "event recorded");
                // As above: the stats health string follows the live health
                // state through recovery rather than latching a past outage.
                if matches!(health.snapshot().0, HealthStatus::Ready) {
                    stats.update(|stats| stats.health = "ready".to_string());
                }
                println!("observation_written=true");
                // Recognition: crop the sighting, embed it, match it against
                // the site library, and record it into site memory. A failure
                // here is loud but never drops the detection event.
                let recognition_started_at = crate::clock::now_utc();
                let recognition_attempt = recognition_embedder.map(|embedder| {
                    recognize_detection(
                        store,
                        embedder,
                        config,
                        &nodes,
                        detection,
                        segment,
                        &observation_id.to_string(),
                        stats,
                    )
                });
                // Receipt truth per attempt class: a real failure is
                // Rejected with the failing stage; an uncovered class was
                // never an attempt and gets no receipt.
                match &recognition_attempt {
                    Some(RecognitionAttempt::Done(outcome)) => {
                        let matched = outcome.entity_id.is_some();
                        crate::workgraph::StageAttempt::derive(
                            detection_work,
                            crate::workgraph::STAGE_RECOGNITION,
                        )
                        .started_at(recognition_started_at)
                        .finish(
                            receipts,
                            crate::workgraph::WorkDisposition::Completed,
                            u64::from(matched),
                            &format!("matched={matched}"),
                            &mut receipt_line_sink(stats),
                        );
                    }
                    Some(RecognitionAttempt::Failed { stage, error }) => {
                        crate::workgraph::StageAttempt::derive(
                            detection_work,
                            crate::workgraph::STAGE_RECOGNITION,
                        )
                        .started_at(recognition_started_at)
                        .finish(
                            receipts,
                            crate::workgraph::WorkDisposition::Rejected,
                            0,
                            &format!("failed_stage={stage} error={error}"),
                            &mut receipt_line_sink(stats),
                        );
                    }
                    Some(RecognitionAttempt::NotCovered) | None => {}
                }
                let (entity_name, match_score) = match recognition_attempt {
                    Some(RecognitionAttempt::Done(outcome)) => (outcome.name, Some(outcome.score)),
                    _ => (None, None),
                };
                // Publish the detection fact via the long-lived channel when configured.
                if let Some(channel) = detection_publisher {
                    let fact = crate::site_channel::DetectionFact {
                        observation_id: observation_id.to_string(),
                        camera_id: camera_slug(&config.camera_name),
                        camera_name: config.camera_name.clone(),
                        object_class: detection.class_name.clone(),
                        confidence: detection.confidence,
                        timestamp_ms: segment.observed_at.timestamp_millis(),
                        evidence_ref: segment.source_ref.clone(),
                        snapshot_ref,
                        entity_name,
                        match_score,
                    };
                    channel.publish_detection(fact);
                }
            }
            Err(error) => {
                stats.update(|stats| {
                    stats.observation_write_failures =
                        stats.observation_write_failures.saturating_add(1);
                });
                // The clip was already finalized into the durable clips dir;
                // with no observation referencing it, it would leak forever
                // (nothing sweeps clips). Remove it with the failure, and
                // receipt the evidence work as Rejected — the earlier
                // Completed clip receipt described finalization, not the
                // full evidence outcome.
                let _ = fs::remove_file(&segment.final_path);
                crate::workgraph::StageAttempt::derive(
                    &segment.envelope,
                    crate::workgraph::STAGE_CLIP_EVIDENCE,
                )
                .finish(
                    receipts,
                    crate::workgraph::WorkDisposition::Rejected,
                    0,
                    &format!("evidence_discarded=true error={error}"),
                    &mut receipt_line_sink(stats),
                );
                return Err(error);
            }
        }
    }
    Ok(())
}

fn duplicate_detection_seen(
    store: &Store,
    segment: &CapturedSegment,
    decision_id: context_graph::DecisionId,
    // Vigil is one-context-per-site; scope to the known context to avoid a full-store scan.
    context_id: context_graph::ContextId,
) -> bool {
    let decision_id = decision_id.to_string();
    store
        .list_observations(Some(context_id))
        .map(|observations| {
            observations.iter().any(|observation| {
                let same_decision = observation
                    .observed_properties
                    .get("detector_decision_id")
                    .and_then(Value::as_str)
                    == Some(decision_id.as_str());
                let same_clip = observation
                    .properties
                    .get("clip_sha256")
                    .and_then(Value::as_str)
                    == Some(segment.clip_sha256.as_str())
                    || observation
                        .properties
                        .get("decoded_frames_sha256")
                        .and_then(Value::as_str)
                        == Some(segment.decoded_frames_sha256.as_str());
                let same_stream_generation = observation
                    .properties
                    .get("stream_generation")
                    .and_then(Value::as_u64)
                    == Some(segment.stream_generation);
                same_decision && same_clip && same_stream_generation
            })
        })
        .unwrap_or(false)
}

/// The recognition step for one covered detection: crop the subject from the
/// native-resolution frame, embed it, match it open-set against the site's
/// enrolled references, and record the sighting into site memory anchored to
/// its detection. Errors are loud (counted and printed) but never fail the
/// detection write.
#[allow(clippy::too_many_arguments)]
/// What actually happened to one recognition attempt — a real failure is
/// distinct from "class not covered" and from "attempted, no match".
enum RecognitionAttempt {
    NotCovered,
    Failed { stage: String, error: String },
    Done(crate::recognition::MatchOutcome),
}

#[allow(clippy::too_many_arguments)]
fn recognize_detection(
    store: &Store,
    embedder: &dyn context_graph::Embedder,
    config: &config::RuntimeConfig,
    nodes: &MemoryNodes,
    detection: &yolox_detector::Detection,
    segment: &CapturedSegment,
    anchored_detection_id: &str,
    stats: &RuntimeStatsState,
) -> RecognitionAttempt {
    let recognition = &config.recognition;
    if !crate::recognition::class_is_covered(recognition, &detection.class_name) {
        return RecognitionAttempt::NotCovered;
    }
    let fail = |stage: &str, error: String| {
        println!("recognition_failed=true stage={stage} error={error}");
        stats.update(|stats| {
            stats.recognition_failures = stats.recognition_failures.saturating_add(1);
        });
        RecognitionAttempt::Failed {
            stage: stage.to_string(),
            error,
        }
    };
    let Some(frame) = segment.media.frames.get(detection.frame_index as usize) else {
        return fail(
            "frame",
            format!("frame index {} not in segment", detection.frame_index),
        );
    };
    let Some(rect) = crate::recognition::map_bbox_to_frame(
        &detection.bbox,
        DETECTOR_INPUT_WIDTH,
        DETECTOR_INPUT_HEIGHT,
        frame.width,
        frame.height,
    ) else {
        return fail("bbox", format!("bbox {} does not map", detection.bbox));
    };
    let crop = match crate::recognition::crop_png(&frame.rgb, frame.width, frame.height, rect) {
        Ok(crop) => crop,
        Err(error) => return fail("crop", error),
    };
    let embed_started = Instant::now();
    let probe = match embedder.embed(context_graph::EmbeddingInput::ImageBytes(crop)) {
        Ok(output) => output.vector,
        Err(error) => return fail("embed", format!("{error}")),
    };
    let embed_ms = embed_started.elapsed().as_secs_f64() * 1000.0;
    let outcome = match crate::recognition::match_vector_for_class(
        store,
        &recognition.embedding_space_id,
        nodes.context_id,
        &probe,
        recognition.match_threshold,
        &detection.class_name,
    ) {
        Ok(outcome) => outcome,
        Err(error) => return fail("match", error),
    };
    if let Err(error) = crate::recognition::record_match_observation(
        store,
        nodes.camera_id,
        nodes.context_id,
        anchored_detection_id,
        &outcome,
        &probe,
        &detection.class_name,
        &segment.source_ref,
    ) {
        return fail("record", error);
    }
    stats.update(|stats| {
        stats.crops_embedded = stats.crops_embedded.saturating_add(1);
        stats.embed_latency_max_ms = stats.embed_latency_max_ms.max(embed_ms);
        if outcome.entity_id.is_some() {
            stats.recognition_matches = stats.recognition_matches.saturating_add(1);
        } else {
            stats.recognition_unknowns = stats.recognition_unknowns.saturating_add(1);
        }
    });
    match &outcome.name {
        Some(name) => println!(
            "recognition_match=true name={name} score={:.4}",
            outcome.score
        ),
        None => println!(
            "recognition_match=false class={} score={:.4}",
            detection.class_name, outcome.score
        ),
    }
    RecognitionAttempt::Done(outcome)
}

/// Record a single detection event into cg and return the new observation id plus the
/// snapshot (detector evidence image) source_ref so the caller can publish the MQTT
/// detection event without re-querying the store.
fn record_one_event(
    store: &Store,
    nodes: &MemoryNodes,
    detection: &yolox_detector::Detection,
    output: &yolox_detector::DetectorOutput,
    segment: &CapturedSegment,
    config: &config::RuntimeConfig,
) -> Result<(ObservationId, String), String> {
    let observed_at = segment.observed_at;
    let video_evidence = EvidenceRef {
        id: context_graph::EvidenceId::new_v7(),
        kind: EvidenceKind::VideoSegment,
        source_ref: segment.source_ref.clone(),
        mime_type: Some(segment.mime_type.clone()),
        captured_at: Some(observed_at),
        producer: EvidenceProducer {
            system: "vigil".to_string(),
            model_name: config.detector_model_id.clone(),
            model_version: "0.1".to_string(),
            pipeline_version: "first-run".to_string(),
        },
        retention_status: RetentionStatus::RetainedExternal,
        ..EvidenceRef::default()
    };
    let detector_image = write_detector_evidence_image(segment, detection, config)?;
    let image_evidence = EvidenceRef {
        id: context_graph::EvidenceId::new_v7(),
        kind: EvidenceKind::ImageFrame,
        source_ref: detector_image.source_ref.clone(),
        mime_type: Some("image/png".to_string()),
        content_hash: Some(format!("sha256:{}", detector_image.sha256)),
        captured_at: Some(observed_at),
        frame_index: Some(detection.frame_index),
        region: Some(json!({
            "bbox": detection.bbox,
            "coordinate_space": "detector_input",
            "width": DETECTOR_INPUT_WIDTH,
            "height": DETECTOR_INPUT_HEIGHT
        })),
        producer: EvidenceProducer {
            system: "vigil".to_string(),
            model_name: config.detector_model_id.clone(),
            model_version: "0.1".to_string(),
            pipeline_version: "first-run".to_string(),
        },
        retention_status: RetentionStatus::RetainedExternal,
        ..EvidenceRef::default()
    };
    let mut observed_properties = BTreeMap::new();
    observed_properties.insert(
        "class".to_string(),
        Value::String(detection.class_name.clone()),
    );
    observed_properties.insert("confidence".to_string(), json!(detection.confidence));
    observed_properties.insert("bbox".to_string(), Value::String(detection.bbox.clone()));
    observed_properties.insert("frame_index".to_string(), json!(detection.frame_index));
    observed_properties.insert(
        "detector_decision_id".to_string(),
        Value::String(nodes.decision_id.to_string()),
    );
    let mut properties = BTreeMap::new();
    properties.insert("clip_frame_count".to_string(), json!(segment.frames));
    properties.insert(
        "baseline_intention_id".to_string(),
        Value::String(nodes.intention_id.to_string()),
    );
    // Backend identity stays OUT of product-domain observations (receipts/
    // stats/logs/doctor carry it); execution proof rides the session id +
    // model/forward digests below.
    properties.insert(
        "detector_session_id".to_string(),
        Value::String(output.detector_session_id.clone()),
    );
    properties.insert(
        "detector_model_sha256".to_string(),
        Value::String(output.model_sha256.clone()),
    );
    properties.insert(
        detector_digest_property_key("model_forward"),
        Value::String(output.model_forward_digest().to_string()),
    );
    properties.insert(
        detector_digest_property_key("nms"),
        Value::String(output.nms_digest().to_string()),
    );
    properties.insert(
        detector_digest_property_key("result"),
        Value::String(output.result_digest().to_string()),
    );
    properties.insert(
        "clip_sha256".to_string(),
        Value::String(output.clip_digest().to_string()),
    );
    properties.insert(
        "decoded_frames_sha256".to_string(),
        Value::String(segment.decoded_frames_sha256.clone()),
    );
    properties.insert(
        "stream_generation".to_string(),
        Value::Number(segment.stream_generation.into()),
    );
    properties.insert("capture_sequence".to_string(), json!(segment.sequence));
    properties.insert(
        "detector_evidence_ref".to_string(),
        Value::String(detector_image.source_ref.clone()),
    );
    properties.insert(
        "detector_evidence_sha256".to_string(),
        Value::String(detector_image.sha256.clone()),
    );
    properties.insert(
        "detector_input_width".to_string(),
        json!(DETECTOR_INPUT_WIDTH),
    );
    properties.insert(
        "detector_input_height".to_string(),
        json!(DETECTOR_INPUT_HEIGHT),
    );
    let observation_id = ObservationId::new_v7();
    if let Err(error) = store.record_observation(RecordObservation {
        id: observation_id,
        entity_id: nodes.camera_id,
        context_id: nodes.context_id,
        observation_type: "detection".to_string(),
        source: "vigil".to_string(),
        observed_at,
        evidence: vec![video_evidence, image_evidence],
        observed_properties,
        state_delta: BTreeMap::new(),
        properties,
        embeddings: Vec::new(),
    }) {
        let _ = fs::remove_file(&detector_image.path);
        return Err(format!("record observation: {error}"));
    }
    Ok((observation_id, detector_image.source_ref))
}

struct DetectorEvidenceImage {
    source_ref: String,
    sha256: String,
    path: PathBuf,
}

fn write_detector_evidence_image(
    segment: &CapturedSegment,
    detection: &yolox_detector::Detection,
    config: &config::RuntimeConfig,
) -> Result<DetectorEvidenceImage, String> {
    let parent = segment
        .final_path
        .parent()
        .ok_or_else(|| format!("clip path {} has no parent", segment.final_path.display()))?;
    let stem = segment
        .final_path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            format!(
                "clip path {} has no UTF-8 stem",
                segment.final_path.display()
            )
        })?;
    let class_name = detection
        .class_name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    let file_name = format!(
        "{stem}-detector-frame-{}-{class_name}.png",
        detection.frame_index
    );
    let path = parent.join(&file_name);
    media_pipeline::write_detector_evidence_png(
        &segment.media,
        detection.frame_index,
        &detection.bbox,
        &path,
        DETECTOR_INPUT_WIDTH,
        DETECTOR_INPUT_HEIGHT,
    )
    .map_err(|error| {
        format!(
            "write detector evidence for {}: {error}",
            config.camera_name
        )
    })?;
    // A hash failure must not strand the just-written PNG on disk.
    let sha256 = match media_pipeline::sha256_path(&path) {
        Ok(sha256) => sha256,
        Err(error) => {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
    };
    Ok(DetectorEvidenceImage {
        source_ref: format!("vigil-edge:clip/{file_name}"),
        sha256,
        path,
    })
}

fn detector_digest_property_key(kind: &str) -> String {
    ["detector", kind, "sha256"].join("_")
}

fn get_or_create_context(store: &Store, name: &str) -> Result<context_graph::Context, String> {
    if let Some(context) = store
        .list_contexts()
        .map_err(|error| format!("list contexts: {error}"))?
        .into_iter()
        .find(|context| context.name == name)
    {
        return Ok(context);
    }
    store
        .create_context(CreateContext {
            name: name.to_string(),
            labels: vec!["site".to_string()],
            properties: BTreeMap::new(),
        })
        .map_err(|error| format!("create context: {error}"))
}

fn get_or_create_camera(
    store: &Store,
    context_id: context_graph::ContextId,
    name: &str,
    rtsp_url: &str,
) -> Result<context_graph::Entity, String> {
    if let Some(camera) = store
        .list_entities(ListEntityFilter {
            entity_type: Some(EntityType::Device),
            context_id: Some(context_id),
            ..ListEntityFilter::default()
        })
        .map_err(|error| format!("list cameras: {error}"))?
        .into_iter()
        .find(|camera| camera.name == name)
    {
        if camera.properties.get("rtsp_url").and_then(Value::as_str) == Some(rtsp_url) {
            return Ok(camera);
        }
        let mut properties = camera.properties.clone();
        properties.insert("rtsp_url".to_string(), Value::String(rtsp_url.to_string()));
        return store
            .update_entity(
                camera.id,
                EntityPatch {
                    properties: Some(properties),
                    ..EntityPatch::default()
                },
            )
            .map_err(|error| format!("update camera rtsp_url: {error}"));
    }
    let mut properties = BTreeMap::new();
    properties.insert("rtsp_url".to_string(), Value::String(rtsp_url.to_string()));
    store
        .create_entity(CreateEntity {
            entity_type: EntityType::Device,
            name: name.to_string(),
            properties,
            tags: vec!["camera".to_string()],
            context_id,
        })
        .map_err(|error| format!("create camera: {error}"))
}

fn get_or_create_intention(
    store: &Store,
    context_id: context_graph::ContextId,
    camera_name: &str,
) -> Result<context_graph::Intention, String> {
    let description = camera_intention_description(camera_name);
    for entry in store
        .audit_query(AuditFilter::default())
        .map_err(|error| format!("query audit: {error}"))?
    {
        let AuditTarget::Intention(id) = entry.target else {
            continue;
        };
        if let Some(intention) = store
            .get_intention(id)
            .map_err(|error| format!("get intention: {error}"))?
            && intention.context_id == context_id
            && intention.description == description
        {
            return Ok(intention);
        }
    }
    store
        .create_intention(CreateIntention {
            id: None,
            description,
            status: IntentionStatus::Active,
            origin: IntentionOrigin::Agent,
            context_id,
            properties: BTreeMap::new(),
            tags: vec!["camera".to_string()],
            blueprint_catalog_id: None,
        })
        .map_err(|error| format!("create intention: {error}"))
}

fn camera_intention_description(camera_name: &str) -> String {
    format!("watch {}", camera_name.trim())
}

fn get_or_create_decision(
    store: &Store,
    config: &config::RuntimeConfig,
    context_id: context_graph::ContextId,
    camera_id: context_graph::EntityId,
    intention_id: context_graph::IntentionId,
) -> Result<context_graph::Decision, String> {
    for entry in store
        .audit_query(AuditFilter::default())
        .map_err(|error| format!("query audit: {error}"))?
    {
        let AuditTarget::Decision(id) = entry.target else {
            continue;
        };
        if let Some(decision) = store
            .get_decision(id)
            .map_err(|error| format!("get decision: {error}"))?
            && decision.context_id == context_id
            && decision.properties.get("model_id").and_then(Value::as_str)
                == Some(config.detector_model_id.as_str())
            && decision
                .properties
                .get("threshold")
                .and_then(Value::as_f64)
                .map(|value| (value - config.detector_confidence_threshold).abs() < f64::EPSILON)
                .unwrap_or(false)
        {
            return Ok(decision);
        }
    }
    let mut properties = BTreeMap::new();
    properties.insert(
        "model_id".to_string(),
        Value::String(config.detector_model_id.clone()),
    );
    properties.insert(
        "threshold".to_string(),
        json!(config.detector_confidence_threshold),
    );
    store
        .create_decision(CreateDecision {
            decision_type: "detector_config".to_string(),
            description: format!("Run local detector for {}", config.camera_name.trim()),
            reasoning: Vec::new(),
            confidence: Some(0.5),
            intention_ids: vec![intention_id],
            based_on_entity_ids: vec![camera_id],
            basis_fields: None,
            based_on_snapshots: Vec::new(),
            tags: vec!["detector".to_string()],
            properties,
            context_id,
            precedent_ids: Vec::new(),
            agent_id: None,
        })
        .map_err(|error| format!("create decision: {error}"))
}

// ── Fabric integration (criteria C1-C10): the production wiring that turns
// the already-shipped fabric/offload machinery into a live node now lives
// in `crate::fabric` — the background-task spawns, the wire-result →
// `DetectorOutput` bridge, and the node bring-up are OWNED there so this
// detector-source file stays free of task-spawning and forward-proof
// machinery. Absent fabric config (`fabric_ticket`/`fabric_hub` both unset)
// the subsystem never starts (`crate::fabric::fabric_bring_up` returns
// `None`) and the detection path is BYTE IDENTICAL to today. This file keeps
// only the feature-gated calls into `crate::fabric`.
#[cfg(feature = "fabric")]
use crate::fabric::{FabricBundle, PendingOffloadSegment, fabric_bring_up, try_offload_segment};

/// How one capture session ended, which is what decides whether the camera
/// records a fault against itself.
///
/// A session ends four ways and only one of them is the camera's fault. The
/// operator naming a different decode path ends the session on purpose: the
/// pipeline reopens at once under the new path, and counting that against the
/// camera would show an operator who asked for a change a stream that dropped,
/// a health surface reporting failed ingest, and a retry delay they never asked
/// to sit through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureSessionEnd {
    /// The process is going down; nothing is reopened.
    ShuttingDown,
    /// The operator named a different decode path, so this session was ended to
    /// let the next one select afresh under it.
    PlannedDecodeReselect,
    /// The session ended with nothing wrong and nothing to say.
    Quiescent,
    /// A genuine stream fault: the camera is not delivering.
    Fault,
}

/// Which of the four a session end was.
///
/// The decode answer this node runs is the discriminator, and it is read
/// against the answer the session opened under — never against how the capture
/// happened to report itself. Tearing a live session down to reopen it under a
/// different decoder surfaces as an ordinary stream error just as often as it
/// surfaces as a quiet exit, so a classification that only recognises the quiet
/// shape describes the rare case and books the ordinary one as a fault.
fn classify_capture_end(
    capture_failed: bool,
    shutting_down: bool,
    decode_epoch_at_open: u64,
    decode_epoch_now: u64,
) -> CaptureSessionEnd {
    if capture_failed && shutting_down {
        return CaptureSessionEnd::ShuttingDown;
    }
    // The decode answer is read FIRST, before how the capture reported itself,
    // because that is the only fact that says whose doing this end was.
    if !shutting_down && decode_epoch_now != decode_epoch_at_open {
        return CaptureSessionEnd::PlannedDecodeReselect;
    }
    if capture_failed {
        CaptureSessionEnd::Fault
    } else {
        CaptureSessionEnd::Quiescent
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CaptureSessionEnd, DetectorSegmentDecision, LatestSegmentQueue, LatestSegmentRecv,
        classify_capture_end, decision_delay_from_env, detector_segment_decision,
    };
    use crate::config;
    use crate::ha_camera_registration::generic_camera_url;
    use std::time::Duration;

    #[test]
    fn detector_decision_delay_is_zero_without_env_and_honors_short_values() {
        // Absent env → zero added latency on the hot path.
        assert_eq!(
            decision_delay_from_env(None),
            Duration::ZERO,
            "no VIGIL_DETECTOR_DECISION_DELAY_MS means no per-decision delay"
        );
        // An explicit 0 is also zero (no sleep triggered).
        assert_eq!(decision_delay_from_env(Some(0)), Duration::ZERO);
        // A short value delays each decision by exactly that many ms.
        assert_eq!(
            decision_delay_from_env(Some(15)),
            Duration::from_millis(15),
            "the lever delays each detection decision by the configured ms"
        );
        assert_eq!(decision_delay_from_env(Some(3)), Duration::from_millis(3));
    }

    #[test]
    fn latest_segment_queue_keeps_newest_pending_work_when_full() {
        let queue = LatestSegmentQueue::new(1);

        assert_eq!(queue.push_latest(1), Ok(None));
        assert_eq!(queue.push_latest(2), Ok(Some(1)));
        assert_eq!(queue.push_latest(3), Ok(Some(2)));

        match queue.recv_timeout(Duration::from_millis(0)) {
            LatestSegmentRecv::Item(value) => assert_eq!(value, 3),
            LatestSegmentRecv::Timeout => panic!("expected newest pending segment"),
            LatestSegmentRecv::Closed => panic!("queue closed unexpectedly"),
        }
        match queue.recv_timeout(Duration::from_millis(0)) {
            LatestSegmentRecv::Timeout => {}
            LatestSegmentRecv::Item(value) => panic!("unexpected pending segment {value}"),
            LatestSegmentRecv::Closed => panic!("queue closed unexpectedly"),
        }
    }

    #[test]
    fn latest_segment_queue_wakes_receiver_when_closed() {
        let queue = LatestSegmentQueue::<u64>::new(1);
        queue.close();

        match queue.recv_timeout(Duration::from_secs(5)) {
            LatestSegmentRecv::Closed => {}
            LatestSegmentRecv::Timeout => panic!("closed queue should not wait until timeout"),
            LatestSegmentRecv::Item(value) => panic!("unexpected pending segment {value}"),
        }
        assert_eq!(queue.push_latest(1), Err(1));
    }

    #[test]
    fn runtime_detection_selection_seam_is_honest_for_this_artifact() {
        assert!(
            super::validate_compiled_capability_requests(false, false).is_ok(),
            "an unconfigured optional capability must remain inert"
        );
        #[cfg(not(feature = "fabric"))]
        for (ticket, hub) in [(true, false), (false, true), (true, true)] {
            let error = super::validate_compiled_capability_requests(ticket, hub)
                .expect_err("configured fabric must fail loudly in a non-fabric binary");
            assert!(
                error.contains("built without the fabric capability"),
                "the operator error must name the missing compiled capability: {error}"
            );
        }
        #[cfg(feature = "fabric")]
        assert!(
            super::validate_compiled_capability_requests(true, true).is_ok(),
            "a feature-complete shipping binary must accept configured fabric intent"
        );

        // Guard for the single detection-selection seam runtime now calls
        // (post-RED guard, not a criteria-coverage test): a future artifact
        // that compiles an accelerated detector backend without keeping this
        // seam as the one source would regress the fold. Under this CPU-only
        // artifact, accelerated_detection=true is an honest
        // backend_not_compiled FALLBACK; false is DISABLED and never touches
        // the probe (the invocation-count contract itself is covered by the
        // frozen detection_accel_backend suite via the injected probe).
        let configured = super::select_detection_acceleration(
            true,
            "model-under-test",
            crate::yolox_detector::MODEL_INPUT_SHAPE,
        );
        let receipt = configured.receipt;
        assert!(receipt.configured);
        assert_eq!(
            receipt.model_id.as_deref(),
            Some("model-under-test"),
            "the receipt names the model identity"
        );

        let disabled = crate::detection_accel::select_detection_acceleration_with_probe(
            false,
            "model-under-test",
            crate::yolox_detector::MODEL_INPUT_SHAPE,
            crate::detection_accel::NoopDetectionForwardProbe,
        );
        assert_eq!(
            disabled.receipt.probe_status,
            crate::acceleration::ProbeStatus::Disabled
        );
        assert_eq!(
            disabled.receipt.failure_code,
            crate::acceleration::FailureCode::None
        );

        #[cfg(not(feature = "detect-burn-wgpu"))]
        {
            assert_eq!(
                receipt.probe_status,
                crate::acceleration::ProbeStatus::Fallback
            );
            assert_eq!(
                receipt.failure_code,
                crate::acceleration::FailureCode::BackendNotCompiled
            );
            assert_eq!(
                receipt.active_backend,
                crate::detection_accel::CPU_DETECTION_BACKEND
            );
            assert!(!receipt.hardware_accelerated);
        }

        // Deterministic once-per-process proof (feature-gated: only the real
        // probe path is deduplicated). A barrier maximizes simultaneity so
        // concurrent callers actually race into the seam together; every
        // caller must observe the identical outcome AND the underlying real
        // computation must have run exactly once — this fails immediately
        // if per-camera selection ever regresses into N independent probes
        // instead of sharing one.
        #[cfg(feature = "detect-burn-wgpu")]
        {
            let before = crate::detection_accel::process_probe_computation_count();
            const CONCURRENT_CALLERS: usize = 8;
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(CONCURRENT_CALLERS));
            let handles: Vec<_> = (0..CONCURRENT_CALLERS)
                .map(|_| {
                    let barrier = std::sync::Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        crate::detection_accel::select_detection_acceleration(
                            true,
                            "model-under-test-concurrency-probe",
                            crate::yolox_detector::MODEL_INPUT_SHAPE,
                        )
                    })
                })
                .collect();
            let selections: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().expect("probe thread must not panic"))
                .collect();
            let first = &selections[0].receipt;
            for selection in &selections[1..] {
                assert_eq!(
                    selection.receipt.failure_code, first.failure_code,
                    "every concurrent caller for the same identity must observe the identical selection outcome"
                );
                assert_eq!(selection.receipt.selected_device, first.selected_device);
                assert_eq!(
                    selection.receipt.hardware_accelerated,
                    first.hardware_accelerated
                );
            }
            let after = crate::detection_accel::process_probe_computation_count();
            assert_eq!(
                after - before,
                1,
                "concurrent callers for the same identity must share exactly one real computation, not race N separate probes"
            );
        }
    }

    #[test]
    fn generic_camera_url_prefers_live_rtsp_url_over_detection_rtsp_url() {
        let camera = config::CameraEntry {
            name: "front-gate-cam".to_string(),
            rtsp_url: Some("rtsp://camera/detect".to_string()),
            live_rtsp_url: Some("rtsp://camera/live".to_string()),
            username: None,
            password: None,
            usb_device: None,
            csi_module: None,
            mjpeg_url: None,
            source_kind: Some(config::CameraSourceKind::Rtsp),
        };

        assert_eq!(generic_camera_url(&camera), Some("rtsp://camera/live"));
    }

    #[test]
    fn generic_camera_url_falls_back_to_detection_rtsp_url_for_single_stream_cameras() {
        let camera = config::CameraEntry {
            name: "single-stream-cam".to_string(),
            rtsp_url: Some("rtsp://camera/main".to_string()),
            live_rtsp_url: None,
            username: None,
            password: None,
            usb_device: None,
            csi_module: None,
            mjpeg_url: None,
            source_kind: Some(config::CameraSourceKind::Rtsp),
        };

        assert_eq!(generic_camera_url(&camera), Some("rtsp://camera/main"));
    }

    #[test]
    fn detector_gate_still_enqueues_motion_positive_segments() {
        assert_eq!(
            detector_segment_decision(3, Duration::from_secs(30), Some(Duration::ZERO)),
            DetectorSegmentDecision::Enqueue {
                stationary_scan: false
            }
        );
    }

    #[test]
    fn detector_gate_suppresses_motion_free_segments_when_stationary_scan_is_disabled() {
        assert_eq!(
            detector_segment_decision(0, Duration::ZERO, None),
            DetectorSegmentDecision::SuppressMotionGate
        );
    }

    #[test]
    fn detector_gate_periodically_enqueues_motion_free_segments_for_stationary_scan() {
        assert_eq!(
            detector_segment_decision(0, Duration::from_secs(30), None),
            DetectorSegmentDecision::Enqueue {
                stationary_scan: true
            }
        );
        assert_eq!(
            detector_segment_decision(0, Duration::from_secs(30), Some(Duration::from_secs(29))),
            DetectorSegmentDecision::SuppressMotionGate
        );
        assert_eq!(
            detector_segment_decision(0, Duration::from_secs(30), Some(Duration::from_secs(30))),
            DetectorSegmentDecision::Enqueue {
                stationary_scan: true
            }
        );
    }

    #[test]
    fn a_planned_decode_reselect_is_not_counted_as_a_stream_fault() {
        // An operator who names a different decode path asked for exactly one
        // thing: the camera comes back on the new path. What they must not get
        // is their camera booking faults against itself for doing what they
        // asked — a stream drop, failed-ingest health, and an exponential retry
        // sleep that is a real outage nobody requested.
        //
        // The discriminating case is the shape a live change actually
        // produces. Tearing down a running RTSP session to reopen it under
        // another decoder ends the session through one of the pipeline's error
        // returns — the stream ending, the demux failing, the video-unit
        // watchdog — far more often than through the quiet loop-condition exit.
        // A classification that recognises the planned move only on the quiet
        // shape is right about the rare case and wrong about the ordinary one.
        assert_eq!(
            classify_capture_end(true, false, 4, 5),
            CaptureSessionEnd::PlannedDecodeReselect,
            "a session that ended with an error BECAUSE the operator changed the decode path is \
             the change they asked for, not a fault the camera should be marked down for"
        );

        // The quiet exit through the loop condition is the same event and is
        // classified the same way.
        assert_eq!(
            classify_capture_end(false, false, 4, 5),
            CaptureSessionEnd::PlannedDecodeReselect,
            "the quiescent end of a session whose decode answer moved is the same planned move"
        );

        // The sibling that stops the fix being a blanket suppression: with the
        // decode answer unchanged, an error is exactly what it has always been.
        assert_eq!(
            classify_capture_end(true, false, 5, 5),
            CaptureSessionEnd::Fault,
            "a genuine stream fault must keep every consequence it has today — the drop counted, \
             ingest marked failed, the retry backed off"
        );
        assert_eq!(
            classify_capture_end(false, false, 5, 5),
            CaptureSessionEnd::Quiescent,
            "and a session that simply ended with nothing wrong says nothing"
        );

        // A process going down is not a camera fault either way.
        assert_eq!(
            classify_capture_end(true, true, 5, 5),
            CaptureSessionEnd::ShuttingDown
        );
        assert_eq!(
            classify_capture_end(true, true, 4, 5),
            CaptureSessionEnd::ShuttingDown
        );
    }
}
