//! The two operator-facing backend settings — which detection backend runs, and
//! which decode backend runs per stream — and the validation that refuses a
//! backend the running artifact does not carry.
//!
//! The compiled backend set is a property of the artifact, not of the packaging,
//! so both are declared loosely on the add-on surface and validated here.

use crate::settings_model::{Refusal, RefusalKind, SettingValue};

/// The operator-facing setting naming which detection backend runs.
pub const DETECTION_BACKEND_SETTING: &str = "detection_backend";

/// The operator-facing setting naming which decode backend runs per stream.
pub const DECODE_BACKEND_SETTING: &str = "decode_backend";

/// The detection backends this artifact actually carries, read from the
/// compiled feature set rather than from packaging.
pub fn available_detection_backends() -> Vec<&'static str> {
    // The processor backend first: it is what every artifact carries and what
    // a node runs until an accelerated probe proves otherwise, so the late
    // promotion is a move from this one onto the accelerated one.
    // `mut` is load-bearing only where the accelerated backend is compiled in;
    // dropping it would break the featured build, so the allow is scoped to the
    // shape that genuinely never pushes.
    #[cfg_attr(not(feature = "detect-burn-wgpu"), allow(unused_mut))]
    let mut backends = vec![crate::detection_accel::CPU_DETECTION_BACKEND];
    #[cfg(feature = "detect-burn-wgpu")]
    backends.push(crate::detection_accel::ACCELERATED_DETECTION_BACKEND);
    backends
}

/// The decode backends this artifact actually carries.
pub fn available_decode_backends() -> Vec<&'static str> {
    // As above: the hardware path exists only where the system decode stack is
    // compiled in, so the allow is scoped to the software-only shape.
    #[cfg_attr(not(feature = "decode-gstreamer"), allow(unused_mut))]
    let mut backends = vec![SOFTWARE_DECODE_BACKEND];
    #[cfg(feature = "decode-gstreamer")]
    backends.push(HARDWARE_DECODE_BACKEND);
    backends
}

/// The software decode path, which every artifact carries.
pub const SOFTWARE_DECODE_BACKEND: &str = "software";

/// The hardware decode path, which exists only where the system decode stack
/// is compiled in.
pub const HARDWARE_DECODE_BACKEND: &str = "hardware";

/// The backends one backend setting may name in this artifact.
fn available_for(setting: &str) -> Option<Vec<&'static str>> {
    match setting {
        DETECTION_BACKEND_SETTING => Some(available_detection_backends()),
        DECODE_BACKEND_SETTING => Some(available_decode_backends()),
        _ => None,
    }
}

/// Validate a proposed backend value against what this artifact carries.
/// Refuses naming both the value and what is available.
pub fn validate_backend(setting: &str, value: &str) -> Result<(), Refusal> {
    let Some(available) = available_for(setting) else {
        return Ok(());
    };
    if available.contains(&value) {
        return Ok(());
    }
    Err(Refusal {
        kind: RefusalKind::UnavailableBackend {
            setting: setting.to_string(),
        },
        cause: format!("this artifact does not carry a backend named {value}."),
        remedy: format!(
            "Name one of the backends it does carry: {}.",
            available.join(", ")
        ),
    })
}

/// Every write-time validation one setting's value has to pass, in one place
/// so the surface a value arrives through can never decide how strictly it is
/// checked.
pub fn validate_setting_value(setting: &str, value: &SettingValue) -> Result<(), Refusal> {
    if available_for(setting).is_some() {
        return validate_backend(setting, &value.to_string());
    }
    if let Some((low, high)) = declared_range(setting) {
        return match value {
            SettingValue::Int(number) if (low..=high).contains(number) => Ok(()),
            SettingValue::Int(number) => Err(out_of_range(setting, *number, low, high)),
            other => Err(invalid_value(
                setting,
                &format!("it takes a whole number, not {}", other.type_name()),
                &format!("Set a whole number between {low} and {high}."),
            )),
        };
    }
    if let Some((low, high)) = declared_float_range(setting) {
        return match value {
            SettingValue::Float(number) if (low..=high).contains(number) => Ok(()),
            SettingValue::Int(number) if (low..=high).contains(&(*number as f64)) => Ok(()),
            SettingValue::Float(number) => Err(out_of_range_float(setting, *number, low, high)),
            SettingValue::Int(number) => {
                Err(out_of_range_float(setting, *number as f64, low, high))
            }
            other => Err(invalid_value(
                setting,
                &format!("it takes a number, not {}", other.type_name()),
                &format!("Set a number between {low} and {high}."),
            )),
        };
    }
    if setting == crate::settings_model::DETECTOR_CLASSES_SETTING {
        return match value {
            SettingValue::List(classes) => validate_detection_classes(classes),
            SettingValue::Text(class) => validate_detection_classes(std::slice::from_ref(class)),
            other => Err(invalid_value(
                setting,
                &format!("a class list is a list of names, not {}", other.type_name()),
                "Name the classes as a list of strings.",
            )),
        };
    }
    Ok(())
}

/// The range a setting declares, where it declares one. A declared range never
/// refuses a value already stored and working on an install: this is consulted
/// when a value is WRITTEN and never when one is read back, which is what makes
/// a later narrowing safe for an install already running the older value.
fn declared_range(setting: &str) -> Option<(i64, i64)> {
    use crate::settings_model::{
        DETECTOR_QUEUE_CAPACITY_SETTING, DETECTOR_SAMPLE_FRAMES_SETTING,
        DETECTOR_STATIONARY_INTERVAL_SETTING, MOTION_SENSITIVITY_SETTING,
    };
    match setting {
        // The lowest runnable capacity is 1: a queue of zero holds nothing, and
        // clamping it to 1 would leave an install running a value its owner
        // never asked for while the surface reads back what they typed.
        _ if setting == DETECTOR_QUEUE_CAPACITY_SETTING => Some((1, 100_000)),
        _ if setting == MOTION_SENSITIVITY_SETTING => Some((1, 10)),
        // Matches the loader's own bound (`config.rs::validate_detector_sample_frames`)
        // exactly, so a value the store accepts can never fail to load.
        _ if setting == DETECTOR_SAMPLE_FRAMES_SETTING => Some((1, 64)),
        // Zero is a real choice here rather than a spin: it turns periodic
        // re-scanning of a motion-free scene OFF, which is what an owner whose
        // camera watches an unchanging wall asks for, and what the gate already
        // does with it. Refusing it would take a capability away with the knob
        // that used to carry it.
        _ if setting == DETECTOR_STATIONARY_INTERVAL_SETTING => Some((0, 86_400)),
        // The values arriving from the environment carry their ranges with
        // them, so a value refused at write time is refused for a reason an
        // operator can act on rather than accepted and quietly clamped later.
        _ if setting == crate::settings_model::HEALTH_PORT_SETTING
            || setting == crate::settings_model::REVIEW_PORT_SETTING =>
        {
            Some((1, 65_535))
        }
        _ if setting == crate::settings_model::FABRIC_WORKER_LEASE_MS_SETTING
            || setting == crate::settings_model::FABRIC_FALLBACK_HORIZON_MS_SETTING
            || setting == crate::settings_model::RTSP_RETRY_INITIAL_MS_SETTING
            || setting == crate::settings_model::RTSP_RETRY_MAX_MS_SETTING =>
        {
            // A wait of zero is not a faster retry, it is a spin; the ceiling
            // is a day, past which a camera that came back would not be
            // noticed for longer than anyone would call watching.
            Some((1, 86_400_000))
        }
        _ if setting == crate::settings_model::DECODE_PROBE_DEADLINE_SECS_SETTING
            || setting == crate::settings_model::HARDWARE_PROBE_DEADLINE_SECS_SETTING =>
        {
            // A probe deadline of zero fails every probe before it starts,
            // which would read as "this machine has no hardware" rather than
            // as a setting nobody meant to make.
            Some((1, 86_400))
        }
        _ => None,
    }
}

/// The range a fractional setting declares, where it declares one. Mirrors
/// `declared_range` above but for the two settings the loader already bounds
/// as floats (`config.rs::validate_confidence_threshold` /
/// `validate_recognition_threshold`) — one contract, enforced at write time
/// here exactly as it is at load time there, instead of accepted at write time
/// and only ever checked when the process next loads.
fn declared_float_range(setting: &str) -> Option<(f64, f64)> {
    match setting {
        _ if setting == crate::settings_model::DETECTOR_CONFIDENCE_THRESHOLD_SETTING
            || setting == crate::settings_model::RECOGNITION_THRESHOLD_SETTING =>
        {
            Some((0.0, 1.0))
        }
        _ => None,
    }
}

/// A value outside its declared range, refused when it is written, naming both
/// what was rejected and what the range admits — so the operator knows what to
/// type instead rather than only that they were wrong.
fn out_of_range(setting: &str, given: i64, low: i64, high: i64) -> Refusal {
    Refusal {
        kind: RefusalKind::InvalidValue {
            setting: setting.to_string(),
        },
        cause: format!(
            "{setting} was given {given}, which is outside the range it declares, {low} to {high}."
        ),
        remedy: format!("Set {setting} to a value between {low} and {high}."),
    }
}

/// A fractional bound as the operator reads it: a whole-numbered bound of a
/// fractional setting renders `0.0` and `1.0`, the decimal range every document
/// spells, rather than `0` and `1` — which reads as if only those two whole
/// numbers were admissible for a setting whose whole point is the fraction
/// between them.
fn fractional(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

/// A fractional value outside its declared range, refused when it is written.
/// Mirrors `out_of_range` above for a float-typed setting.
fn out_of_range_float(setting: &str, given: f64, low: f64, high: f64) -> Refusal {
    // The given value is rendered as the operator typed it; only the declared
    // bounds are spelled as the range.
    let low = fractional(low);
    let high = fractional(high);
    Refusal {
        kind: RefusalKind::InvalidValue {
            setting: setting.to_string(),
        },
        cause: format!(
            "{setting} was given {given}, which is outside the range it declares, {low} to {high}."
        ),
        remedy: format!("Set {setting} to a value between {low} and {high}."),
    }
}

/// The one shape an invalid value is refused in.
fn invalid_value(setting: &str, cause: &str, remedy: &str) -> Refusal {
    Refusal {
        kind: RefusalKind::InvalidValue {
            setting: setting.to_string(),
        },
        cause: format!("{setting} was given a value it cannot take: {cause}."),
        remedy: remedy.to_string(),
    }
}

/// Vigil's own product-selected value for one setting, with the derivation
/// input as its reason. This is the automatic floor: it is always present,
/// even when nobody has ever configured anything.
pub fn automatic_default(setting: &str) -> Option<(SettingValue, String)> {
    if crate::settings_domains::is_domain_switch(setting) {
        return Some((
            SettingValue::Bool(true),
            format!("{setting} manages these values together by default"),
        ));
    }
    match setting {
        DETECTION_BACKEND_SETTING => {
            let chosen = available_detection_backends();
            Some((
                SettingValue::text(chosen[0]),
                "no accelerated detection backend has proved itself on this machine yet"
                    .to_string(),
            ))
        }
        DECODE_BACKEND_SETTING => {
            let chosen = available_decode_backends();
            Some((
                SettingValue::text(chosen[0]),
                "no hardware decode path has proved itself on this machine yet".to_string(),
            ))
        }
        _ if setting == crate::settings_reflection::RESTART_ON_REFLECT_SETTING => Some((
            SettingValue::Bool(true),
            "a value that has arrived and is not yet running is the confusion this removes"
                .to_string(),
        )),
        _ if setting == crate::settings_model::DETECTOR_CLASSES_SETTING => Some((
            SettingValue::list(default_detection_classes().iter().copied()),
            "nobody has named the classes to detect, so vigil detects its default subset"
                .to_string(),
        )),
        // The rate controls and the recognition class list: Automatic means
        // vigil chose a value and may revise it, so the floor carries the value
        // it actually runs. A blank here would report Automatic while telling
        // the operator nothing about what is running.
        _ if setting == crate::settings_model::DETECTOR_SAMPLE_FRAMES_SETTING => Some((
            SettingValue::Int(5),
            "vigil's own sampling rate, until measured conditions or an operator move it"
                .to_string(),
        )),
        _ if setting == crate::settings_model::DETECTOR_STATIONARY_INTERVAL_SETTING => Some((
            SettingValue::Int(30),
            "vigil's own re-scan interval for a scene that is not moving".to_string(),
        )),
        _ if setting == crate::settings_model::DETECTOR_CONFIDENCE_THRESHOLD_SETTING => Some((
            SettingValue::Float(0.5),
            "vigil's own detection confidence threshold".to_string(),
        )),
        _ if setting == crate::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING => Some((
            SettingValue::Int(AUTOMATIC_DETECTOR_QUEUE_CAPACITY as i64),
            "vigil's own detector queue depth, sized for steady work rather than backlog"
                .to_string(),
        )),
        _ if setting == crate::settings_model::MOTION_SENSITIVITY_SETTING => Some((
            SettingValue::Int(5),
            "vigil's own motion sensitivity, mid-scale until a camera proves it needs otherwise"
                .to_string(),
        )),
        _ if setting == crate::settings_model::SITE_NAME_SETTING => Some((
            SettingValue::text(automatic::SITE_NAME),
            "vigil's own name for a deployment nobody has named".to_string(),
        )),
        _ if setting == crate::settings_model::CAMERA_NAME_SETTING => Some((
            SettingValue::text(automatic::CAMERA_NAME),
            "vigil's own name for the first camera on a deployment nobody has named".to_string(),
        )),
        _ if setting == crate::settings_model::HEALTH_PORT_SETTING => Some((
            SettingValue::Int(automatic::HEALTH_PORT),
            "vigil's own liveness port, the one the add-on's watchdog is pointed at".to_string(),
        )),
        _ if setting == crate::settings_model::REVIEW_PORT_SETTING => Some((
            SettingValue::Int(automatic::REVIEW_PORT),
            "vigil's own review port, the one the add-on maps".to_string(),
        )),
        _ if setting == crate::settings_model::DETECTOR_MODEL_ID_SETTING => Some((
            SettingValue::text(automatic::DETECTOR_MODEL_ID),
            "the detection model this artifact carries".to_string(),
        )),
        _ if setting == crate::settings_model::RECOGNITION_SPACE_ID_SETTING => Some((
            SettingValue::text(crate::recognition::RecognitionConfig::default().embedding_space_id),
            "the embedding space recognition matches within by default".to_string(),
        )),
        _ if setting == crate::settings_model::RECOGNITION_THRESHOLD_SETTING => Some((
            SettingValue::Float(crate::recognition::RecognitionConfig::default().match_threshold),
            "vigil's own recognition threshold".to_string(),
        )),
        _ if setting == crate::settings_model::FABRIC_HUB_SETTING => Some((
            SettingValue::Bool(false),
            "a node carries no hub role until someone gives it one: no silent new network \
             surface on an install"
                .to_string(),
        )),
        _ if setting == crate::settings_model::FABRIC_ALLOW_FRAME_OFFLOAD_SETTING => Some((
            SettingValue::Bool(true),
            "offload becomes automatic on joining a fabric; this is the knob that turns it back \
             off for one node"
                .to_string(),
        )),
        _ if setting == crate::settings_model::FABRIC_WORKER_LEASE_MS_SETTING => Some((
            SettingValue::Int(automatic::FABRIC_WORKER_LEASE_MS),
            "vigil's own fabric worker lease".to_string(),
        )),
        _ if setting == crate::settings_model::FABRIC_FALLBACK_HORIZON_MS_SETTING => Some((
            SettingValue::Int(automatic::FABRIC_FALLBACK_HORIZON_MS),
            "how long vigil waits for a remote result before doing the work locally".to_string(),
        )),
        _ if setting == crate::settings_model::RTSP_RETRY_INITIAL_MS_SETTING => Some((
            SettingValue::Int(automatic::RTSP_RETRY_INITIAL_MS),
            "vigil's own first reconnect wait after a camera stream drops".to_string(),
        )),
        _ if setting == crate::settings_model::RTSP_RETRY_MAX_MS_SETTING => Some((
            SettingValue::Int(automatic::RTSP_RETRY_MAX_MS),
            "the widest vigil lets that reconnect backoff get".to_string(),
        )),
        _ if setting == crate::settings_model::DECODE_PROBE_DEADLINE_SECS_SETTING => Some((
            SettingValue::Int(automatic::DECODE_PROBE_DEADLINE_SECS),
            "how long vigil gives the decode probe before using the software path".to_string(),
        )),
        _ if setting == crate::settings_model::HARDWARE_PROBE_DEADLINE_SECS_SETTING => Some((
            SettingValue::Int(automatic::HARDWARE_PROBE_DEADLINE_SECS),
            "how long vigil gives the hardware-decode probe".to_string(),
        )),
        _ if setting == crate::settings_model::RECOGNITION_COVERED_CLASSES_SETTING => Some((
            SettingValue::list(default_recognition_covered_classes().iter().copied()),
            "the classes recognition covers by default, which never widens or narrows the \
             detector's own class list"
                .to_string(),
        )),
        // A declared setting vigil has no product default for is unspecified
        // rather than unanswerable: the automatic floor is always present, even
        // when nobody has ever configured anything. A name the operator surface
        // does not answer for gets no floor at all — answering with a value and
        // a reason for something that governs nothing is how a withdrawn knob
        // goes on reading as a live control.
        _ if crate::settings_store::is_declared_setting(setting) => Some((
            SettingValue::text(String::new()),
            format!("nothing has been set for {setting}, so vigil uses its own default"),
        )),
        _ => None,
    }
}

/// The classes Vigil detects when the operator says nothing. Class selection
/// is independent of recognition: this is a sensible default subset, never a
/// list recognition widens or narrows.
pub fn default_detection_classes() -> &'static [&'static str] {
    &["person"]
}

/// The classes recognition covers when the operator has not set
/// `recognition_covered_classes`. Class selection is completely independent
/// of recognition, so this is its own default rather than a mirror of
/// `RecognitionConfig::default()`'s classes (a caller-facing engine baseline
/// wider than what a fresh install detects by default): widening
/// `detector_classes` to include a traffic class must never silently widen
/// what recognition matches to a name too. This is the deployment default
/// this add-on shipped before recognition coverage had its own settings
/// entry — person and the household pet, never a traffic class.
pub fn default_recognition_covered_classes() -> &'static [&'static str] {
    &["person", "dog"]
}

/// Vigil's own values for the behavior settings that are leaving the
/// environment. Each is the number or name the loader already defaults to, in
/// ONE place, so the automatic floor an operator reads and the value the
/// runtime actually uses cannot drift apart — the drift the queue depth's three
/// separate spellings of `1` were one edit away from.
pub mod automatic {
    /// The liveness port, matched to the add-on's declared watchdog target.
    pub const HEALTH_PORT: i64 = 8099;

    /// The review data plane's port, matched to the add-on's port mapping.
    pub const REVIEW_PORT: i64 = 8098;

    /// What a deployment calls itself until someone names it.
    pub const SITE_NAME: &str = "site-1";

    /// The first camera's name in the single-camera shape.
    pub const CAMERA_NAME: &str = "camera-1";

    /// The detection model every artifact carries.
    pub const DETECTOR_MODEL_ID: &str = "yolox-tiny-burn-cpu";

    /// The fabric worker lease.
    pub const FABRIC_WORKER_LEASE_MS: i64 = 300_000;

    /// How long a node waits for a remote result before working locally.
    pub const FABRIC_FALLBACK_HORIZON_MS: i64 = 5_000;

    /// The first reconnect wait after a camera stream drops.
    pub const RTSP_RETRY_INITIAL_MS: i64 = 2_000;

    /// The widest reconnect backoff.
    pub const RTSP_RETRY_MAX_MS: i64 = 30_000;

    /// The decode probe's deadline.
    pub const DECODE_PROBE_DEADLINE_SECS: i64 = 5;

    /// The hardware-decode probe's deadline.
    pub const HARDWARE_PROBE_DEADLINE_SECS: i64 = 10;
}

/// The whole-seconds value this process resolved for one duration setting, or
/// Vigil's own where nobody has set it.
///
/// The probe deadlines are consumed deep inside the decode path, where no
/// configuration is threaded — so they
/// are read from what the runtime RECORDED as applied when it resolved the
/// store, rather than each site resolving its own. One resolution, one value,
/// and a process that never resolved anything (a probe binary, a test calling
/// the decode path directly) still gets Vigil's own number rather than nothing.
pub fn resolved_secs(setting: &str, automatic: i64) -> u64 {
    let value = crate::settings_projection::running_value(setting)
        .and_then(|value| match value {
            SettingValue::Int(secs) => Some(secs),
            SettingValue::Float(secs) if secs.is_finite() && secs > 0.0 => Some(secs as i64),
            _ => None,
        })
        .unwrap_or(automatic);
    u64::try_from(value.max(1)).unwrap_or(automatic.max(1) as u64)
}

/// Vigil's own detector queue depth. One number, read by the automatic floor
/// the operator surface reports, by the loader's default, and by the queue the
/// runtime actually builds — so those three can never drift into disagreeing
/// about what "automatic" means here.
pub const AUTOMATIC_DETECTOR_QUEUE_CAPACITY: usize = 1;

/// Vigil's own class choice as the detector construction path consumes it —
/// the same names, already mapped to inventory indices. A run whose store is
/// not open yet, or cannot be opened at all, builds its detectors from this.
pub fn automatic_detection_class_indices() -> Vec<usize> {
    default_detection_classes()
        .iter()
        .filter_map(|name| detection_class_index(name))
        .collect()
}

/// The full declared class inventory the shipped model carries — the eighty
/// COCO classes. The breadth promise is this whole list being selectable, so it
/// is a closed enumeration a test can assert against in full, never a sample.
pub const DECLARED_DETECTION_CLASS_COUNT: usize = 80;

/// The index a class name occupies in the declared inventory, which is what the
/// detector's allowlist is expressed in.
pub fn detection_class_index(name: &str) -> Option<usize> {
    declared_detection_class_inventory()
        .iter()
        .position(|declared| *declared == name)
}

/// The detection class names the model inventory shipped with this artifact
/// declares. Validation runs against this, not against a loaded model instance,
/// so a write refuses correctly before any detector exists.
pub fn declared_detection_class_inventory() -> Vec<&'static str> {
    crate::yolox_detector::COCO_CLASSES.to_vec()
}

/// Validate a detection class list. Distinguishes omitted from explicitly empty
/// from invalid, and names the field, the offending value, and the remedy.
pub fn validate_detection_classes(classes: &[String]) -> Result<(), Refusal> {
    let setting = crate::settings_model::DETECTOR_CLASSES_SETTING;
    if classes.is_empty() {
        // Omitted, empty, invalid and unavailable stay four different things:
        // omitting the setting means automatic, while an explicitly empty list
        // asks vigil to detect nothing at all.
        return Err(Refusal {
            kind: RefusalKind::InvalidClass {
                setting: setting.to_string(),
            },
            cause: format!(
                "{setting} was given an empty list, which asks vigil to detect nothing at all."
            ),
            remedy: format!(
                "Name the classes to detect, or reset {setting} so it drops to whatever is \
                 beneath it and vigil chooses its own default subset."
            ),
        });
    }
    let inventory = declared_detection_class_inventory();
    let unknown: Vec<&str> = classes
        .iter()
        .map(String::as_str)
        .filter(|class| !inventory.contains(class))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(Refusal {
        kind: RefusalKind::InvalidClass {
            setting: setting.to_string(),
        },
        cause: format!(
            "{setting} names {} which the detection model does not carry.",
            unknown.join(", ")
        ),
        remedy: "Name classes from the model's own inventory; every class it detects is \
                 selectable."
            .to_string(),
    })
}
