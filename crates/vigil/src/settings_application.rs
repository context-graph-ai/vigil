//! When each setting takes effect: live, or at the next restart — and the one
//! place a value that IS in force on this process is held.
//!
//! The operator surface answers requested, running and pending side by side,
//! and the pending field names what closes the gap. That answer is only
//! honest if every setting DECLARES which of the two it is, next to the
//! setting itself, rather than each consumer deciding for itself and the
//! surface guessing afterwards.
//!
//! The conservative direction is the safe one: a setting is restart-pending
//! unless Vigil genuinely brings it into force on a running process. Claiming
//! live application for a value the process only reads at startup would report
//! a change as running when nothing changed.
//!
//! Bringing a value into force and reporting it as running are ONE act here.
//! [`bring_into_force`] writes the value where the consumer reads it, and
//! [`in_force`] is that same read — so what the surface reports as running is
//! by construction the value the machine is using, never a copy of it recorded
//! beside a dead assignment.

use crate::settings_backends::{DECODE_BACKEND_SETTING, DETECTION_BACKEND_SETTING};
use crate::settings_domains::{ACCELERATED_DETECTION_DOMAIN, HARDWARE_DECODING_DOMAIN};
use crate::settings_model::{PendingCause, SettingValue};

/// When a setting takes effect once it has been stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationTiming {
    /// The running process brings the new value into force without a restart,
    /// so requested and running agree as soon as the write lands.
    Live,
    /// The process takes this value on at startup, so a change waits for the
    /// next one and the surface carries it as pending until then.
    NextRestart,
}

/// The values this process brings into force without a restart.
///
/// Every one of them is read from [`in_force`] by the seam that uses it on each
/// pass — the detector's own loop, the motion gate, the backend selection — so
/// a value landing here is a value the next pass runs at. Nothing else may be
/// listed: declaring live application for a value a consumer only reads at
/// startup would report a change as running while the old value kept running.
const LIVE_SETTINGS: &[&str] = &[
    // The two automatic-management switches and the backends they govern, as
    // one group: a switch that took effect live while the value it governs
    // waited for a restart would leave an operator watching the automation turn
    // off and nothing else happen.
    ACCELERATED_DETECTION_DOMAIN,
    DETECTION_BACKEND_SETTING,
    HARDWARE_DECODING_DOMAIN,
    DECODE_BACKEND_SETTING,
    // The analysis-rate group, which is what an owner whose machine cannot keep
    // up reaches for. All four are read per segment by the detector loop.
    crate::settings_model::DETECTOR_SAMPLE_FRAMES_SETTING,
    crate::settings_model::DETECTOR_STATIONARY_INTERVAL_SETTING,
    crate::settings_model::DETECTOR_CONFIDENCE_THRESHOLD_SETTING,
    crate::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING,
    // Read by the motion gate on every decoded segment.
    crate::settings_model::MOTION_SENSITIVITY_SETTING,
];

/// The live values a consumer reads AFRESH on each pass, so the value standing
/// in the running registry is what the next pass runs at.
///
/// Named once because two passes act on exactly this group: a change brings the
/// authored value into force, and a withdrawal brings whatever the withdrawal
/// left in force. A second spelling of the group would let one of the two move
/// without the other.
const READ_AFRESH_EACH_PASS: &[&str] = &[
    crate::settings_model::DETECTOR_SAMPLE_FRAMES_SETTING,
    crate::settings_model::DETECTOR_STATIONARY_INTERVAL_SETTING,
    crate::settings_model::DETECTOR_CONFIDENCE_THRESHOLD_SETTING,
    crate::settings_model::MOTION_SENSITIVITY_SETTING,
];

/// When `setting` takes effect. Every setting the operator surface answers for
/// has an entry: a setting with no declared timing is a setting whose pending
/// field cannot be rendered honestly.
pub fn application_timing(setting: &str) -> Option<ApplicationTiming> {
    if !declares_timing(setting) {
        return None;
    }
    if LIVE_SETTINGS.contains(&setting) {
        return Some(ApplicationTiming::Live);
    }
    Some(ApplicationTiming::NextRestart)
}

/// What closes the gap for `setting` when requested and running differ — the
/// cause the operator surface renders.
///
/// A person reads this field to decide one thing: do I wait, or do I restart
/// the thing watching my property. Naming a camera reconnect or a model reload
/// is an instruction — it says this machine closes the gap by itself and the
/// operator should not restart. Where nothing performs such a transition, a
/// person told to wait waits forever, so a restart is what this answers.
///
/// This is the answer for a setting with no transition of its own behind it —
/// the camera endpoints and the detector model, read as the process starts and
/// by nothing else afterwards. A setting that IS carried by a seam on the
/// running node — the detection backend's preparation, the decode path's
/// reconnect — is answered by [`live_transition`] before this is reached, and
/// that seam names its own cause rather than having one spelled here.
///
/// A live setting has a cause too: it is what an operator would be waiting for
/// in the one case a live application could not land — the value having been
/// taken on by a process that is no longer the one running.
///
/// The two remaining causes stay in the vocabulary and are deliberately
/// unreachable from here. The honest way to reach one is to build the seam that
/// performs it and then let that seam speak — never to restore the wording
/// ahead of the behavior.
pub fn pending_cause(setting: &str) -> Option<PendingCause> {
    if !declares_timing(setting) {
        return None;
    }
    Some(PendingCause::Restart)
}

/// A transition this node started and is carrying itself, as an operator
/// surface reports it: when the preparation began, and the last thing it said
/// about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTransition {
    pub preparing_since_ms: u64,
    pub latest_progress: Option<String>,
}

/// The transition outstanding for `setting` on this process right now, if there
/// is one.
///
/// A manual backend change answers before the preparation is done — that is the
/// whole point of the non-blocking path — so between the command and the swap
/// the requested and running values genuinely differ. Reading that gap as a
/// restart would send the operator to restart a node that is already closing it,
/// and preparing-since with the preparation's own progress note is the only way
/// a person watching a long cold build can tell slow from wedged.
///
/// Two settings have such work behind them, and they are the two this build
/// genuinely carries out on a running node. The detection backend has a
/// coordinator preparing a detector for it. The decode path has the camera
/// pipelines: naming one bumps the selection this node runs on, each pipeline
/// ends its session on it, and the fresh session selects afresh under the path
/// just named — all in this process, with nothing for an operator to restart.
/// Every other setting answers from the declaration, which is why this returns
/// nothing for them rather than inventing a transition no seam performs.
pub fn live_transition(setting: &str) -> Option<LiveTransition> {
    match setting {
        crate::settings_backends::DETECTION_BACKEND_SETTING => {
            let pending = crate::live_backends::detection_transitions()
                .state()
                .pending?;
            Some(LiveTransition {
                preparing_since_ms: pending.preparing_since_ms,
                latest_progress: pending.latest_progress,
            })
        }
        // The switch and the path it governs move together, as everywhere else:
        // a reconnect asked for by turning the automation on or off is the same
        // reconnect, and answering restart for the switch while the path it
        // governs says a change is under way tells one operator two things.
        DECODE_BACKEND_SETTING | HARDWARE_DECODING_DOMAIN => {
            // No progress note: a reconnect reports itself by reconnecting, and
            // the pipelines say nothing on the way. Preparing-since is what
            // this transition genuinely has to say.
            Some(LiveTransition {
                preparing_since_ms: crate::live_backends::decode_transition_since_ms()?,
                latest_progress: None,
            })
        }
        _ => None,
    }
}

/// Whether this setting is one the operator surface answers for at all. The
/// roster is the store's own declared roster, so a setting added there declares
/// its timing here or fails loudly rather than rendering a blank pending field
/// on a real install.
fn declares_timing(setting: &str) -> bool {
    crate::settings_store::is_declared_setting(setting)
}

/// Put `value` where the consumer of `setting` reads it on its next pass, and
/// report it as what this process is running.
///
/// One act, one home: the running registry IS the live value, so a consumer
/// reading [`in_force`] and an operator reading the surface can never be told
/// two different things.
pub fn bring_into_force(setting: &str, value: SettingValue) {
    crate::settings_projection::record_running(setting, value);
}

/// The value in force on this process for `setting`, if it has taken one on.
pub fn in_force(setting: &str) -> Option<SettingValue> {
    crate::settings_projection::running_value(setting)
}

/// Hold the published facts about a live change together: the value in force,
/// whether a preparation is still outstanding for it, and — where nobody
/// authored a value, so the choice reported IS the running one — the choice
/// itself.
///
/// A landing change writes those in separate registries. A reader that took
/// them separately could land between the writes and see the OLD value in
/// force with NOTHING outstanding, or the pre-move choice against the
/// post-move value in force — either way a gap with nothing closing it, which
/// the surface reads as a restart, sending an operator to power-cycle a node
/// that is already running exactly what they asked for. Both sides take this
/// first, so a reader sees the complete old state or the complete new one and
/// never the seam between them.
///
/// Taken BEFORE the detection coordinator's own lock on both sides, so the
/// two can never be acquired in opposite orders.
pub fn hold_publication() -> std::sync::MutexGuard<'static, ()> {
    static PUBLICATION: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    PUBLICATION
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the store asks this process to run, for the live values a STRUCTURE has
/// to take on rather than a parameter a caller reads afresh.
///
/// The analysis rate is read on each pass, so a stored change is what the next
/// pass runs at the moment it lands. The detector queue is different: it was
/// SIZED when the runtime started and is holding work under that bound, so a
/// new depth is what it takes on when it next accepts work. Both are live —
/// neither waits for a restart — and separating what is asked for from what has
/// been taken on is what lets the surface say so honestly instead of reporting
/// a depth as running the instant it is typed.
static REQUESTED_LIVE_VALUES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<String, SettingValue>>,
> = std::sync::OnceLock::new();

fn requested_registry()
-> &'static std::sync::Mutex<std::collections::BTreeMap<String, SettingValue>> {
    REQUESTED_LIVE_VALUES.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}

/// Ask this process to run `setting` at `value` as soon as the structure that
/// holds it can take it on.
pub fn request_live(setting: &str, value: SettingValue) {
    if let Ok(mut requested) = requested_registry().lock() {
        requested.insert(setting.to_string(), value);
    }
}

/// What has been asked for, for a consumer about to take a new value on.
pub fn requested_live(setting: &str) -> Option<SettingValue> {
    requested_registry()
        .lock()
        .ok()
        .and_then(|requested| requested.get(setting).cloned())
}

/// Bring every live setting's stored value into force on this running process.
///
/// Called after a change lands, by the process running the cameras — which is
/// the process that answers `vigil settings` while it is up, so an operator's
/// change is applied by the same act that records it. A `vigil settings`
/// invocation on a stopped deployment does nothing here: it has no cameras to
/// apply anything to, and reporting its own write as running would claim a
/// machine is doing something no machine is doing.
///
/// A setting nobody has authored is left alone rather than dropped to the
/// automatic floor: what stands in that case is the value this run started on,
/// which is what the floor is beneath.
pub fn apply_live_change<S: crate::settings_store::ResolvesSettings>(
    store: &S,
    target: &crate::settings_model::ScopeTarget,
) {
    if !is_running_process() {
        return;
    }
    let authored = |setting: &str| -> Option<SettingValue> {
        match store.resolve(setting, target) {
            Ok(effective) if effective.author == crate::settings_model::Author::Automatic => None,
            Ok(effective) => Some(effective.requested),
            Err(_) => None,
        }
    };
    // Read afresh on each pass by the seam that uses them, so the pass that
    // runs next runs at the new value: they are in force the moment they land.
    for setting in READ_AFRESH_EACH_PASS.iter().copied() {
        if let Some(value) = authored(setting) {
            bring_into_force(setting, value);
        }
    }
    // The queue was sized when this run started and is holding work under that
    // bound; it takes the new depth on when it next accepts work, and until
    // then what it is running at is the bound it holds.
    if let Some(value) = authored(crate::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING) {
        request_live(
            crate::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING,
            value,
        );
    }
    // The two automatic-management switches and the backends they govern move
    // together: a switch turned off with the value it governs still waiting
    // would leave an operator watching the automation stop and nothing else
    // happen.
    //
    // Every pin is published BEFORE either re-selection runs, because the
    // selection seams read the pins from that registry — a re-selection reading
    // a pin the change had not published yet would decide against the previous
    // answer.
    let mut switched_on = [false; 2];
    for (index, (switch, setting)) in [
        (ACCELERATED_DETECTION_DOMAIN, DETECTION_BACKEND_SETTING),
        (HARDWARE_DECODING_DOMAIN, DECODE_BACKEND_SETTING),
    ]
    .into_iter()
    .enumerate()
    {
        let on = !matches!(store.resolve(switch, target), Ok(ref effective) if effective.requested == SettingValue::Bool(false));
        switched_on[index] = on;
        bring_into_force(switch, SettingValue::Bool(on));
        publish_pinned_backend(
            setting,
            match authored(setting) {
                Some(SettingValue::Text(backend)) if !backend.is_empty() => Some(backend),
                _ => None,
            },
        );
    }
    // What asks for the detector this process feeds frames through to be
    // replaced, and what asks this node's camera pipelines to reconnect under
    // the decode path they were just asked for. The detection half records the
    // request and returns: building and forward-testing a detector happens off
    // this path, so a change lands on the operator's surface at once and the
    // machine reports the new backend as running only once detectors are
    // genuinely on it.
    crate::live_backends::request_detection_backend(switched_on[0]);
    crate::live_backends::reselect_decode_backend(switched_on[1]);
    // The camera-scoped half of the same pass: a value naming one camera is
    // what that camera's gate runs at, and every other camera keeps running
    // what it inherits.
    apply_camera_scoped_change(store, target);
}

/// Bring the value a WITHDRAWAL leaves in force onto this running process,
/// then run the ordinary pass for everything else.
///
/// [`apply_live_change`] deliberately leaves a setting nobody has authored
/// alone: what stands for it is the value this run started on, which the
/// automatic floor sits beneath, and dropping a running process to that floor
/// because no record exists would change a value the operator never touched.
/// A reset is the one move where that reasoning does not hold, and reusing it
/// there is what left a withdrawn value running. What a reset restores is not
/// the value the run started on — it is whatever the withdrawn record was
/// standing on top of, and until this brings it into force the process keeps
/// running the value the operator just took back, until somebody restarts the
/// node.
///
/// Only the withdrawn setting is treated this way, because only it changed
/// author. The backends and the switches that govern them are left to the
/// ordinary pass below, which answers a withdrawal by unpinning the choice and
/// asking for a re-selection — bringing a resolved backend into force here
/// instead would report a detector as running before one had been built for it.
pub fn apply_live_withdrawal<S: crate::settings_store::ResolvesSettings>(
    store: &S,
    target: &crate::settings_model::ScopeTarget,
    withdrawn: &str,
) {
    if is_running_process()
        && let Ok(effective) = store.resolve(withdrawn, target)
    {
        if READ_AFRESH_EACH_PASS.contains(&withdrawn) {
            bring_into_force(withdrawn, effective.requested);
        } else if withdrawn == crate::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING {
            // Sized when this run started and holding work under that bound: it
            // takes the withdrawn-to depth on when it next accepts work, which
            // is the same road a set of it takes.
            request_live(withdrawn, effective.requested);
        }
    }
    apply_live_change(store, target);
}

/// Whether this process is the one running the cameras.
///
/// A `vigil settings` invocation on a stopped deployment brings nothing into
/// force: it writes the store and exits, and reporting its own write as running
/// would claim a machine is doing something no machine is doing.
static RUNTIME_PROCESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Declare this process the running runtime. Called once, by the runtime, as it
/// comes up.
pub fn mark_running_process() {
    RUNTIME_PROCESS.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Whether this process runs the cameras.
pub fn is_running_process() -> bool {
    RUNTIME_PROCESS.load(std::sync::atomic::Ordering::SeqCst)
}

/// The whole number in force for `setting`, or `fallback` where this process
/// has taken nothing on. The typed readers below are what the consuming seams
/// call, so no consumer re-spells the conversion.
fn whole_in_force(setting: &str, fallback: i64) -> i64 {
    match in_force(setting) {
        Some(SettingValue::Int(number)) => number,
        _ => fallback,
    }
}

/// How many frames the detector samples on its next pass.
pub fn detector_sample_frames_in_force(fallback: usize) -> usize {
    usize::try_from(whole_in_force(
        crate::settings_model::DETECTOR_SAMPLE_FRAMES_SETTING,
        fallback as i64,
    ))
    .unwrap_or(fallback)
    .max(1)
}

/// The confidence threshold the detector applies on its next pass.
pub fn detector_confidence_threshold_in_force(fallback: f64) -> f64 {
    match in_force(crate::settings_model::DETECTOR_CONFIDENCE_THRESHOLD_SETTING) {
        Some(SettingValue::Float(threshold)) => threshold,
        Some(SettingValue::Int(threshold)) => threshold as f64,
        _ => fallback,
    }
}

/// How long between stationary scans on the next pass.
pub fn detector_stationary_interval_in_force(fallback: u64) -> u64 {
    u64::try_from(whole_in_force(
        crate::settings_model::DETECTOR_STATIONARY_INTERVAL_SETTING,
        fallback as i64,
    ))
    .unwrap_or(fallback)
}

/// How deep the detector queue holds work on its next push.
pub fn detector_queue_capacity_in_force(fallback: usize) -> usize {
    usize::try_from(whole_in_force(
        crate::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING,
        fallback as i64,
    ))
    .unwrap_or(fallback)
    .max(1)
}

/// The depth the detector queue is asked to hold work at, for the queue to take
/// on when it next accepts work.
pub fn detector_queue_capacity_requested(fallback: usize) -> usize {
    match requested_live(crate::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING) {
        Some(SettingValue::Int(depth)) => usize::try_from(depth).unwrap_or(fallback).max(1),
        _ => fallback,
    }
}

/// Vigil's own motion sensitivity, mid-scale, when nothing is in force yet.
pub const AUTOMATIC_MOTION_SENSITIVITY: i64 = 5;

/// How sensitive the motion gate is on the next decoded segment, on the
/// declared 1..=10 scale.
pub fn motion_sensitivity_in_force() -> i64 {
    whole_in_force(
        crate::settings_model::MOTION_SENSITIVITY_SETTING,
        AUTOMATIC_MOTION_SENSITIVITY,
    )
    .clamp(1, 10)
}

/// What each camera's gate runs at, kept apart per camera because that is the
/// whole point: an owner who turns the driveway down because a tree moves in
/// the wind must not thereby turn the hallway down too.
static CAMERA_MOTION_SENSITIVITY: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<String, i64>>,
> = std::sync::OnceLock::new();

fn camera_motion_registry() -> &'static std::sync::Mutex<std::collections::BTreeMap<String, i64>> {
    CAMERA_MOTION_SENSITIVITY
        .get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}

/// How sensitive the motion gate is on `camera`'s next decoded segment.
///
/// A camera that has its own value in force runs it; a camera that has none
/// runs what the whole deployment is running, which is what inheriting means.
pub fn motion_sensitivity_in_force_for(camera: &str) -> i64 {
    let own = camera_motion_registry()
        .lock()
        .ok()
        .and_then(|sensitivities| sensitivities.get(camera).copied());
    match own {
        Some(sensitivity) => sensitivity.clamp(1, 10),
        None => motion_sensitivity_in_force(),
    }
}

/// The cameras this run watches, so a later change made through `vigil
/// settings` reaches the gate of the camera it names without the command
/// surface having to know the camera set.
static WATCHED_CAMERAS: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> =
    std::sync::OnceLock::new();

fn watched_camera_registry() -> &'static std::sync::Mutex<Vec<String>> {
    WATCHED_CAMERAS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Name the cameras this run watches, so a later change made through `vigil
/// settings` — which knows a data directory and not a camera set — still
/// reaches the gate of the camera it names.
pub fn adopt_camera_set(cameras: &[String]) {
    if let Ok(mut watched) = watched_camera_registry().lock() {
        *watched = cameras.to_vec();
    }
}

/// Put each watched camera's own motion sensitivity where that camera's gate
/// reads it, as this run comes up. The live path does the same on every later
/// change; this is the first one, before any change has been made.
pub fn adopt_camera_scoped_settings(
    store: &crate::settings_store::SettingsStore,
    target: &crate::settings_model::ScopeTarget,
    cameras: &[String],
) {
    adopt_camera_set(cameras);
    apply_camera_scoped_change(store, target);
}

/// Resolve every watched camera's own motion sensitivity and put it where that
/// camera's gate reads it.
fn apply_camera_scoped_change<S: crate::settings_store::ResolvesSettings>(
    store: &S,
    target: &crate::settings_model::ScopeTarget,
) {
    let cameras = match watched_camera_registry().lock() {
        Ok(watched) => watched.clone(),
        Err(_) => return,
    };
    let Ok(mut sensitivities) = camera_motion_registry().lock() else {
        return;
    };
    for camera in &cameras {
        let scoped = crate::settings_model::ScopeTarget {
            tenant: target.tenant.clone(),
            site: target.site.clone(),
            node: target.node.clone(),
            camera: Some(camera.clone()),
        };
        let resolved = match store
            .resolve(crate::settings_model::MOTION_SENSITIVITY_SETTING, &scoped)
        {
            // Automatic means nobody has chosen for this camera or above it, so
            // what stands is what this run is already running.
            Ok(effective) if effective.author == crate::settings_model::Author::Automatic => None,
            Ok(effective) => match effective.requested {
                SettingValue::Int(sensitivity) => Some(sensitivity),
                _ => None,
            },
            Err(_) => None,
        };
        match resolved {
            Some(sensitivity) => sensitivities.insert(camera.clone(), sensitivity),
            None => sensitivities.remove(camera),
        };
    }
}

/// The backend an operator pinned for `setting`, as resolved from the store.
///
/// Distinct from [`in_force`] on purpose: a pin is what the store says, and
/// what runs is what the selection makes of it. A pin naming a path that can
/// only be entered off a passing probe does not become the running backend
/// merely by being stored.
static PINNED_BACKENDS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<String, String>>,
> = std::sync::OnceLock::new();

fn pinned_registry() -> &'static std::sync::Mutex<std::collections::BTreeMap<String, String>> {
    PINNED_BACKENDS.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}

/// Publish what the store resolved for one backend setting, for the selection
/// seams that run deep in the decode and detection paths where no configuration
/// is threaded.
pub fn publish_pinned_backend(setting: &str, pin: Option<String>) {
    if let Ok(mut pins) = pinned_registry().lock() {
        match pin {
            Some(pin) => pins.insert(setting.to_string(), pin),
            None => pins.remove(setting),
        };
    }
}

/// The operator's pin for one backend setting, as the selection seam reads it.
pub fn pinned_backend(setting: &str) -> Option<String> {
    pinned_registry()
        .lock()
        .ok()
        .and_then(|pins| pins.get(setting).cloned())
}
