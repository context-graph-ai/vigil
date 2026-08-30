//! Taking a backend change on while the node keeps watching.
//!
//! The two backend settings are the ones the take-back promise is made of: an
//! operator turns the automation off, names the detection or decode backend
//! they want, and the machine has to run it. A name written into a registry is
//! not that — the detector this process feeds frames through is chosen when the
//! detector is BUILT, and the decode path is chosen when a stream's pipeline is
//! opened. So a live change has to reach those two acts, not the record beside
//! them.
//!
//! Both halves report achieved, never configured. The detection half rebuilds
//! the detector under the newly selected backend and only then says that is
//! what runs; a build that fails leaves the detector that is genuinely running
//! in place, says so on the operator path, and leaves the running value naming
//! the backend still in use. A node watches several cameras and each has its
//! own detector, so achieved is per camera: a change only some cameras took on
//! is reported as exactly that, naming both sides, with each camera's receipt
//! going only to the camera whose detector was genuinely replaced, and the
//! node's running value naming the backend the rest are still on. The decode
//! half asks the affected pipelines to
//! reconnect and says nothing about the running backend at all — the selection
//! the new session performs is what reports it, exactly as at startup.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use std::collections::BTreeMap;

use crate::acceleration::AccelerationReceipt;
use crate::detection_transition::{
    DetectionTransitions, HandleKind, InstallOutcome, PreparedInstance,
};
#[cfg(feature = "detect-burn-wgpu")]
use crate::detection_transition::{PreparationIdentity, PreparationOutcome};
use crate::detector::{Detector, PromotableDetector};
use crate::settings_application::{bring_into_force, in_force, pinned_backend};
use crate::settings_backends::{DECODE_BACKEND_SETTING, DETECTION_BACKEND_SETTING};
use crate::settings_model::SettingValue;

/// Building a detector again under a named backend.
type DetectorBuild = dyn Fn(&str) -> Result<Box<dyn Detector>, String> + Send + Sync;

/// Every physical detector BUILD this process has run, counted where a
/// registered rebuild closure is INVOKED and nowhere else.
///
/// Not a shipped surface: no artifact builds `test-support`
/// (`artifact_never_enables_test_support.rs`).
///
/// Counted at the build and never at the install, because those are different
/// facts and only one of them costs anything: a camera returning to a backend
/// it already has an instance for is a pointer swap with no model loaded, and a
/// preparation proving a backend builds one detector nobody has entered yet. A
/// request already recorded as applied must perform ZERO builds of any kind,
/// and that is what this makes readable.
#[cfg(feature = "test-support")]
static DETECTOR_BUILDS_PERFORMED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many physical detector builds this process has run so far. A pin reads
/// it either side of a second settings read and asserts on the difference.
#[cfg(feature = "test-support")]
pub fn detector_builds_performed() -> u64 {
    DETECTOR_BUILDS_PERFORMED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Run a registered rebuild closure. THE one place a detector is physically
/// built, so what is counted above is a build and never an install.
fn run_detector_build(build: &DetectorBuild, backend: &str) -> Result<Box<dyn Detector>, String> {
    #[cfg(feature = "test-support")]
    DETECTOR_BUILDS_PERFORMED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    build(backend)
}

/// Where a selection receipt goes so the operator surfaces carry it.
type ReceiptSink = dyn Fn(&AccelerationReceipt) + Send + Sync;

/// What the request for a backend currently is: the switch that governs it, and
/// the operator's pin beneath that switch. Both move it, so both are the key —
/// a rebuild that watched only the switch would ignore a pin change, and one
/// that watched only the pin would ignore an operator taking the wheel.
type BackendRequest = (bool, Option<String>);

/// One detector this process is feeding frames through, with the way to build
/// it again under another backend and the place its receipt belongs.
struct LiveDetector {
    handle: Weak<PromotableDetector>,
    camera: String,
    model_id: String,
    build: Arc<DetectorBuild>,
    receipt: Arc<ReceiptSink>,
}

/// One registered detector, taken out of the registry so a rebuild never holds
/// the registry lock across a model load.
struct DetectorToRebuild {
    handle: Arc<PromotableDetector>,
    camera: String,
    model_id: String,
    build: Arc<DetectorBuild>,
    receipt: Arc<ReceiptSink>,
}

/// What a camera worker hands over when its detector is ready.
pub(crate) struct DetectorRebuild {
    /// The camera this detector watches, so a change only some cameras took
    /// on can say which ones did and which ones did not.
    pub(crate) camera: String,
    /// The model identity the selection probes under, so a re-selection asks
    /// the same question startup asked.
    pub(crate) model_id: String,
    /// The switch this detector was built under, which seeds what a later
    /// change is compared against.
    pub(crate) accelerated_detection: bool,
    pub(crate) build: Arc<DetectorBuild>,
    pub(crate) receipt: Arc<ReceiptSink>,
}

fn detectors() -> &'static Mutex<Vec<LiveDetector>> {
    static DETECTORS: OnceLock<Mutex<Vec<LiveDetector>>> = OnceLock::new();
    DETECTORS.get_or_init(|| Mutex::new(Vec::new()))
}

fn applied_detection_request() -> &'static Mutex<Option<BackendRequest>> {
    static APPLIED: OnceLock<Mutex<Option<BackendRequest>>> = OnceLock::new();
    APPLIED.get_or_init(|| Mutex::new(None))
}

fn applied_decode_request() -> &'static Mutex<Option<BackendRequest>> {
    static APPLIED: OnceLock<Mutex<Option<BackendRequest>>> = OnceLock::new();
    APPLIED.get_or_init(|| Mutex::new(None))
}

/// The coordinator every detection backend move goes through — automatic
/// startup acceleration and an operator naming a backend alike. It holds the
/// runtime facts (which handle runs what, what is being prepared under which
/// request version, and the last real error) so the surface can answer while
/// the physical work is still going on.
pub fn detection_transitions() -> &'static DetectionTransitions {
    static TRANSITIONS: OnceLock<DetectionTransitions> = OnceLock::new();
    TRANSITIONS.get_or_init(|| {
        let running = in_force(DETECTION_BACKEND_SETTING)
            .map(|value| value.to_string())
            .unwrap_or_else(|| crate::detection_accel::CPU_DETECTION_BACKEND.to_string());
        DetectionTransitions::with_clock(
            &running,
            Arc::new(|| crate::clock::PersistedClock::contextdb().unix_millis()),
        )
    })
}

/// The detectors this process has already loaded, by handle and backend. A
/// detector replaced by another is kept here rather than dropped, so moving
/// back onto it is a pointer swap instead of a cold model load on the
/// operator's own command.
type PreparedDetectors = BTreeMap<(String, String), Arc<dyn Detector>>;

fn prepared_detectors() -> &'static Mutex<PreparedDetectors> {
    static PREPARED: OnceLock<Mutex<PreparedDetectors>> = OnceLock::new();
    PREPARED.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Names one prepared detector for the coordinator, which carries tokens
/// rather than detectors.
static NEXT_PREPARED_INSTANCE: AtomicU64 = AtomicU64::new(1);

fn retain_prepared(handle: &str, backend: &str, detector: Arc<dyn Detector>) {
    if let Ok(mut prepared) = prepared_detectors().lock() {
        prepared.insert((handle.to_string(), backend.to_string()), detector);
    }
}

fn prepared_detector(handle: &str, backend: &str) -> Option<Arc<dyn Detector>> {
    prepared_detectors().lock().ok().and_then(|prepared| {
        prepared
            .get(&(handle.to_string(), backend.to_string()))
            .cloned()
    })
}

/// The backend this artifact runs when Vigil owns the choice and nothing is
/// pinned: the accelerated one where this artifact carries it, and the
/// processor where it does not — a backend no artifact contains is not a
/// choice.
#[cfg(feature = "detect-burn-wgpu")]
const PREFERRED_MANAGED_BACKEND: &str = crate::detection_accel::ACCELERATED_DETECTION_BACKEND;
#[cfg(not(feature = "detect-burn-wgpu"))]
const PREFERRED_MANAGED_BACKEND: &str = crate::detection_accel::CPU_DETECTION_BACKEND;

/// What is being asked for, read without touching a device: the operator's pin
/// where they set one, otherwise what Vigil chooses under the automation. No
/// probe, no model load — this runs on the command's own path.
fn requested_detection_backend(accelerated_detection: bool) -> String {
    if let Some(pin) = pinned_backend(DETECTION_BACKEND_SETTING) {
        return pin;
    }
    if accelerated_detection {
        PREFERRED_MANAGED_BACKEND.to_string()
    } else {
        crate::detection_accel::CPU_DETECTION_BACKEND.to_string()
    }
}

/// Put a detector prepared away from every control path onto its handle, and
/// move every surface with it.
///
/// This is what a background preparation calls when its work is done: the
/// instance it built and forward-tested is the instance installed, the one it
/// replaces stays loaded as the prepared way back, and the node names the new
/// backend as running only once every registered handle is on it. Work
/// belonging to a superseded request installs nothing.
///
/// The version is the one the preparation's own request took when the
/// preparation STARTED, carried here by the caller. Reading the version at
/// install time instead would ask whether anyone is standing there, not whether
/// this is still the request being waited for — so a startup preparation the
/// operator has since overruled would install itself over their choice.
///
/// Only a build carrying an accelerated detector has a backend to move onto
/// this way; without one there is nothing for a background preparation to
/// promote, so this is compiled where that detector exists.
#[cfg(feature = "detect-burn-wgpu")]
pub(crate) fn install_prepared_detector(
    camera: &str,
    backend: &str,
    version: u64,
    handle: &Arc<PromotableDetector>,
    detector: Box<dyn Detector>,
) -> Result<PreparedInstance, String> {
    take_detector_on(
        camera,
        backend,
        version,
        handle,
        ReadyDetector::FreshlyBuilt(detector),
    )
}

/// A startup move is outstanding from the moment its probe is spawned.
///
/// The probe IS the preparation: the node has work in flight from here until
/// that probe answers and whatever it proved has been taken on. Saying so
/// through the same two flags a command's preparation uses is what makes a
/// command issued during startup attach to the work in flight rather than
/// start a second physical build beside it.
#[cfg(feature = "detect-burn-wgpu")]
pub(crate) fn begin_probed_move() {
    PREPARATION_OWED.store(true, Ordering::SeqCst);
    PREPARATION_RUNNING.store(true, Ordering::SeqCst);
}

/// The probe has answered, so this pass is now carrying the move that was owed
/// when it was spawned — but ONLY if that is still the move being waited for.
///
/// `version` is the request this startup preparation belongs to. A command
/// issued while the probe ran took a newer one, and the pass owed to THAT
/// command is not a pass this one has carried out: clearing the flag on its
/// behalf would leave the operator's request recorded, versioned, and with no
/// worker anywhere in the process to act on it.
#[cfg(feature = "detect-burn-wgpu")]
pub(crate) fn carry_probed_move(version: u64) {
    if probed_move_still_stands(version) {
        PREPARATION_OWED.store(false, Ordering::SeqCst);
    }
}

/// The startup move is over — it landed, or its probe reported why it could
/// not. A request that arrived while it ran is carried now, exactly as the
/// preparation loop carries one that lands between its last turn and its
/// release.
///
/// Whether one is owed is decided by the REQUEST standing, not by a flag: a
/// command issued during the probe is a request this preparation never
/// carried, so a pass is started for it whatever the flag says.
#[cfg(feature = "detect-burn-wgpu")]
pub(crate) fn finish_probed_move(version: u64) {
    PREPARATION_RUNNING.store(false, Ordering::SeqCst);
    if PREPARATION_OWED.load(Ordering::SeqCst) || !probed_move_still_stands(version) {
        PREPARATION_OWED.store(true, Ordering::SeqCst);
        start_preparation();
    }
}

/// Take the request a startup preparation's own probe answered, so the move it
/// produced is carried by the same machinery a command's move is.
///
/// The version is taken and recorded beside its request under one lock, exactly
/// as a command's is: a later command supersedes this move, and this move
/// supersedes nothing that was asked for after it.
#[cfg(feature = "detect-burn-wgpu")]
fn take_probed_request(backend: &str) -> u64 {
    let request: BackendRequest = (true, pinned_backend(DETECTION_BACKEND_SETTING));
    let version = match request_in_flight().lock() {
        Ok(mut held) => {
            let version = detection_transitions().request_backend(backend);
            *held = Some((request, version));
            version
        }
        Err(_) => detection_transitions().request_backend(backend),
    };
    PROBED_MOVE_VERSION.store(version, Ordering::SeqCst);
    version
}

/// The version the last startup move took. A node with several cameras spawns a
/// probe per camera, and the first one to pass moves EVERY registered handle —
/// so the request standing when the second probe answers is that sibling move,
/// not a change of mind. Nothing was superseded, and reporting the second
/// camera's promotion as failed would tell an operator the accelerator could
/// not be entered on a node that is running it.
#[cfg(feature = "detect-burn-wgpu")]
static PROBED_MOVE_VERSION: AtomicU64 = AtomicU64::new(0);

/// Whether a startup move that took `version` when its probe was spawned is
/// still one this node may act on: nothing newer has been asked for, or what is
/// standing is another startup preparation's move rather than a command.
#[cfg(feature = "detect-burn-wgpu")]
fn probed_move_still_stands(version: u64) -> bool {
    let standing = detection_transitions().request_version();
    standing == version || standing == PROBED_MOVE_VERSION.load(Ordering::SeqCst)
}

/// Move every registered handle onto the backend a startup probe just proved,
/// through the one preparation machinery.
///
/// This is what automatic startup calls when its probe passes, and it is the
/// same act a command performs: the coordinator schedules one physical
/// preparation per handle, each builds through the seam that handle registered,
/// and the instance that build produced is the instance installed. Startup used
/// to load a detector for its probe, drop it, and hand its caller the job of
/// cold-loading a second one — so the accelerated compile was paid twice and
/// what ran the cameras was an instance nothing had forward-tested.
///
/// `version` is the request this preparation belongs to, taken when it started.
/// A command issued while the probe was running owns the node, and this
/// installs nothing.
#[cfg(feature = "detect-burn-wgpu")]
pub(crate) fn install_probed_detection_backend(
    backend: &str,
    version: u64,
    forward_tested: Option<Box<dyn Detector>>,
) -> Result<(), String> {
    if !probed_move_still_stands(version) {
        return Err(
            "a newer backend request holds the wheel; this preparation installs nothing"
                .to_string(),
        );
    }
    let version = take_probed_request(backend);
    let failure: Mutex<Option<String>> = Mutex::new(None);
    // The instance the preparation proved, held for the first handle prepared
    // for the backend it was proved on. The handles after it build their own,
    // exactly as they do on a command: a detector belongs to one handle at a
    // time, so there is one proved instance and no second cold load beside it.
    let proved: Mutex<Option<Box<dyn Detector>>> = Mutex::new(forward_tested);
    detection_transitions().drive_preparations(|identity: &PreparationIdentity| {
        let outcome = prepare_registered_handle(identity, version, backend, &proved);
        if let PreparationOutcome::Failed(reason) = &outcome
            && let Ok(mut held) = failure.lock()
        {
            *held = Some(reason.clone());
        }
        outcome
    });
    match failure.into_inner() {
        Ok(Some(reason)) => Err(reason),
        _ => Ok(()),
    }
}

/// Put one handle's detector on: the instance the preparation already proved
/// where this is the handle it was proved for, otherwise one built through the
/// seam that handle registered. The instance handed over here is the instance
/// installed; a handle this process registered no detector for is a real
/// error, not a silent skip.
#[cfg(feature = "detect-burn-wgpu")]
fn prepare_registered_handle(
    identity: &PreparationIdentity,
    version: u64,
    proved_backend: &str,
    proved: &Mutex<Option<Box<dyn Detector>>>,
) -> PreparationOutcome {
    let registered = detectors().lock().ok().and_then(|entries| {
        entries
            .iter()
            .find(|entry| entry.camera == identity.handle)
            .and_then(|entry| {
                entry
                    .handle
                    .upgrade()
                    .map(|handle| (handle, Arc::clone(&entry.build)))
            })
    });
    let Some((handle, build)) = registered else {
        return PreparationOutcome::Failed(format!(
            "{} runs no detector this process registered",
            identity.handle
        ));
    };
    report_preparation_progress(
        version,
        &format!("preparing the detector for {}", identity.handle),
    );
    // The proved instance was built for THIS backend, so it is only ever put
    // on a handle being moved onto it; anything else would install something
    // nothing tested.
    let proved = (identity.backend == proved_backend)
        .then(|| proved.lock().ok().and_then(|mut held| held.take()))
        .flatten();
    let ready = match proved {
        Some(detector) => Ok(detector),
        None => run_detector_build(build.as_ref(), &identity.backend),
    };
    match ready.and_then(|detector| {
        install_prepared_detector(
            &identity.handle,
            &identity.backend,
            version,
            &handle,
            detector,
        )
    }) {
        Ok(instance) => PreparationOutcome::Prepared(instance),
        Err(error) => PreparationOutcome::Failed(error),
    }
}

/// A detector ready to go on a handle: one this process built just now, or one
/// it already had loaded and is putting back.
enum ReadyDetector {
    AlreadyLoaded(Arc<dyn Detector>),
    FreshlyBuilt(Box<dyn Detector>),
}

/// The single act by which a handle changes detector, and the only place the
/// node's running claim moves.
///
/// The instance the preparation produced is the instance installed; the one it
/// replaces stays loaded under the backend it was running, which is what makes
/// a move back onto that backend a pointer swap rather than a cold load. Work
/// belonging to a request that has since been superseded installs nothing.
///
/// The detector and the node's account of itself move together here, in one
/// act: the coordinator names the backend every registered handle is genuinely
/// on, and that — never what a selection decided — is what the operator
/// surfaces answer with. A claim published when a selection made up its mind
/// describes a machine nobody has for as long as any handle is still on the
/// old backend, and a pass that then installs nothing leaves that claim behind
/// as the next pass's ground truth.
fn take_detector_on(
    camera: &str,
    backend: &str,
    version: u64,
    handle: &Arc<PromotableDetector>,
    ready: ReadyDetector,
) -> Result<PreparedInstance, String> {
    let instance = PreparedInstance(NEXT_PREPARED_INSTANCE.fetch_add(1, Ordering::SeqCst));
    let previous = detection_transitions().handle_backend(camera);
    let mut ready = Some(ready);
    let mut replaced: Option<Arc<dyn Detector>> = None;
    // The swap, the coordinator's account of it, and the value this node
    // reports running are ONE act, held together so no read can land between
    // them: a read that saw the move finished and the old value still in force
    // would be told to restart a node already running what it asked for. The
    // detector moves first, inside the version check, so nothing is ever
    // reported as moved before the frames genuinely go through it.
    let outcome = {
        let _published = crate::settings_application::hold_publication();
        let outcome = detection_transitions().install_prepared_taking_on(
            camera,
            version,
            backend,
            instance,
            &mut || {
                replaced = ready.take().map(|ready| match ready {
                    ReadyDetector::AlreadyLoaded(prepared) => handle.swap_to(prepared),
                    ReadyDetector::FreshlyBuilt(built) => handle.swap(built),
                });
            },
        );
        if matches!(outcome, InstallOutcome::Installed) {
            bring_into_force(
                DETECTION_BACKEND_SETTING,
                SettingValue::text(detection_transitions().state().running_backend),
            );
        }
        outcome
    };
    match outcome {
        InstallOutcome::Installed => {}
        InstallOutcome::RejectedStaleVersion => {
            return Err(
                "a newer backend request holds the wheel; this preparation installs nothing"
                    .to_string(),
            );
        }
        InstallOutcome::UnknownHandle => {
            return Err(format!("{camera} runs no detector this process registered"));
        }
    }
    // Kept loaded, so a move back onto either backend is a pointer swap. Held
    // outside the publication above: this registry is not one an operator
    // surface reads, and a settings read has no business waiting on it.
    retain_prepared(camera, backend, handle.snapshot());
    if let Some((previous, replaced)) = previous.zip(replaced) {
        retain_prepared(camera, &previous, replaced);
    }
    Ok(instance)
}

/// The classes a detector this node builds emits, which are part of what makes
/// one preparation the same physical build as another.
fn detection_classes_in_force() -> Vec<String> {
    in_force(crate::settings_model::DETECTOR_CLASSES_SETTING)
        .map(|value| value.to_string())
        .map(|text| {
            text.split(',')
                .map(str::trim)
                .filter(|class| !class.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a preparation pass is owed, and whether one is already carrying the
/// work. A request arriving while a preparation runs attaches to it — it marks
/// that another pass is owed rather than launching a second physical build.
static PREPARATION_OWED: AtomicBool = AtomicBool::new(false);
static PREPARATION_RUNNING: AtomicBool = AtomicBool::new(false);

/// Ask for the detection backend the store now names, and return.
///
/// This is what a settings command and startup both call. It records the
/// request, moves any handle whose detector for that backend is already
/// loaded, and hands the physical work — building and forward-testing a
/// detector — to a preparation that runs off this path entirely. No model is
/// loaded here and nothing is waited for, so the command answers with what was
/// asked for, what is still running, and that a change is under way.
pub fn request_detection_backend(accelerated_detection: bool) {
    let request: BackendRequest = (
        accelerated_detection,
        pinned_backend(DETECTION_BACKEND_SETTING),
    );
    // Already carried out: the command answers, and starts nothing.
    if request_already_carried_out(&request) {
        return;
    }
    // Asking again for what is already being prepared ATTACHES to that work.
    // Taking a new version for it would abandon a build that is doing exactly
    // what was asked for and start the cold work again from nothing.
    if !attaches_to_work_in_flight(&request) {
        take_request(accelerated_detection, request);
    }
    PREPARATION_OWED.store(true, Ordering::SeqCst);
    start_preparation();
}

/// Whether anything the request now standing asked for is still undone: a
/// preparation outstanding, or a handle still on another backend. This is the
/// coordinator answering about the machine, which is the only authority that
/// can — a record of what was applied is a record of what was asked, not of
/// what happened.
/// Whether asking for this again has already been carried out, so that asking
/// changes nothing and starts nothing.
///
/// Asked BEFORE a request version is taken, because taking one is not free: it
/// schedules every handle that is not on the backend the ask NAMES, which on a
/// node whose selection settled somewhere else is every handle — and the pass
/// that follows then sees work in flight and rebuilds, for a request that was
/// already met. So the question is put to the state the last pass left behind,
/// before anything is scheduled on top of it.
fn request_already_carried_out(requested: &BackendRequest) -> bool {
    already_applied(applied_detection_request(), requested) && !work_is_outstanding()
}

fn work_is_outstanding() -> bool {
    let state = detection_transitions().state();
    if state.pending.is_some() {
        return true;
    }
    // Measured against the backend this node is RUNNING — the one the last pass
    // settled on — and NOT against the backend that was asked for.
    //
    // The two part company whenever a selection legitimately settles somewhere
    // other than the ask: an accelerated request on a node whose forward test
    // refuses the accelerator enters the processor backend, and every handle
    // then sits on a backend that is not the one named in the request. Read
    // against the ask, those handles look like work outstanding for ever, on a
    // node that has finished and has nothing left to do. What that costs is not
    // a stale surface reading: this answer is what tells a repeat of an
    // already-applied request that there is nothing to do, so reading it
    // against the ask made every repeat re-run the whole pass — a cold model
    // load, beside a live camera, for a request already carried out.
    //
    // A handle that genuinely never moved still answers here: it is not on the
    // running backend either, which is the case this check exists for.
    !detection_transitions()
        .handles_off_backend(&state.running_backend)
        .is_empty()
}

/// The acceleration surfaces this process reports on, so a preparation's
/// progress reaches the receipt an operator reads as well as the transition
/// state. Registered by the runtime as it comes up; a run without one still
/// prepares, it simply has no receipt surface to speak to.
fn attended_acceleration_state()
-> &'static Mutex<Option<Arc<crate::acceleration::AccelerationState>>> {
    static ATTENDED: OnceLock<Mutex<Option<Arc<crate::acceleration::AccelerationState>>>> =
        OnceLock::new();
    ATTENDED.get_or_init(|| Mutex::new(None))
}

/// Tell this seam where acceleration receipts live, so what a preparation says
/// about itself reaches them.
pub(crate) fn attend_acceleration_state(state: &Arc<crate::acceleration::AccelerationState>) {
    if let Ok(mut attended) = attended_acceleration_state().lock() {
        *attended = Some(Arc::clone(state));
    }
}

/// Say what this preparation is doing, on both surfaces that report it.
///
/// Saying it does not end the preparation and does not restart it: it is the
/// one thing that lets a person watching a long cold build tell it from a
/// wedged one, which is why it comes from the work itself rather than from a
/// clock watching the work.
fn report_preparation_progress(version: u64, note: &str) {
    detection_transitions().report_progress(version, note);
    let attended = attended_acceleration_state()
        .lock()
        .ok()
        .and_then(|attended| attended.clone());
    if let Some(state) = attended {
        state.record_progress(crate::acceleration::AccelStage::Detection, note);
    }
}

/// The request a preparation is carrying right now together with the version it
/// took, held as ONE value so a repeat of it can be told from a change of mind
/// and so no pass can ever pair one request with another's version.
fn request_in_flight() -> &'static Mutex<Option<(BackendRequest, u64)>> {
    static IN_FLIGHT: OnceLock<Mutex<Option<(BackendRequest, u64)>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(|| Mutex::new(None))
}

fn attaches_to_work_in_flight(request: &BackendRequest) -> bool {
    PREPARATION_RUNNING.load(Ordering::SeqCst)
        && matches!(request_in_flight().lock(), Ok(ref held)
            if held.as_ref().map(|(standing, _)| standing) == Some(request))
}

/// The request now standing and the version it took, read together.
///
/// A pass that read the two separately could be handed one command's request
/// under the next command's version, and both version guards would then let it
/// install a backend nobody standing had asked for.
fn standing_request() -> Option<(BackendRequest, u64)> {
    request_in_flight()
        .lock()
        .ok()
        .and_then(|held| held.clone())
}

/// Whether this exact request, under this exact version, is still the one
/// standing.
fn request_still_stands(request: &BackendRequest, version: u64) -> bool {
    matches!(request_in_flight().lock(), Ok(ref held)
        if held.as_ref() == Some(&(request.clone(), version)))
}

/// Take a version for a request, which is what makes every completion under an
/// older one install nothing.
///
/// The version is taken and recorded beside its request under one lock, so the
/// pair a later pass reads is always a pair that was genuinely asked for.
fn take_request(accelerated_detection: bool, request: BackendRequest) -> (BackendRequest, u64) {
    let backend = requested_detection_backend(accelerated_detection);
    match request_in_flight().lock() {
        Ok(mut held) => {
            let version = detection_transitions().request_backend(&backend);
            *held = Some((request.clone(), version));
            (request, version)
        }
        Err(_) => (request, detection_transitions().request_backend(&backend)),
    }
}

/// Carry the owed preparation passes on a thread of their own. A pass already
/// under way takes the newer request on its next turn rather than being
/// duplicated.
fn start_preparation() {
    if PREPARATION_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        while PREPARATION_OWED.swap(false, Ordering::SeqCst) {
            // The pass carries the request that is standing, taken with its
            // version as one value; it never takes a version of its own, or a
            // command's request would be superseded by the very preparation
            // carrying it out.
            if let Some((request, version)) = standing_request() {
                prepare_for_request(request, version);
            }
        }
        PREPARATION_RUNNING.store(false, Ordering::SeqCst);
        // A request that landed in the gap between the last turn and this
        // release would otherwise wait for the next command.
        if PREPARATION_OWED.load(Ordering::SeqCst) {
            start_preparation();
        }
    });
}

/// Register a camera's detector, so a later backend change can reach it rather
/// than stopping at the record beside it.
pub(crate) fn register_live_detector(handle: &Arc<PromotableDetector>, rebuild: DetectorRebuild) {
    register_detector_handle(handle, HandleKind::Camera, rebuild);
}

/// Register the detector a node runs distributed work on.
///
/// A node watching cameras already loads a detector per camera and that work
/// runs on those, so the coordinator keeps no separate handle for it — which is
/// what stops a camera node loading a second model for work it already does.
/// A node with no camera has only this one, and it has to be reachable by a
/// backend move like any other.
///
/// Distributed work is what the fabric brings, so this is compiled where that
/// work can reach this node.
#[cfg(feature = "fabric")]
pub(crate) fn register_work_detector(
    handle: &Arc<PromotableDetector>,
    kind: HandleKind,
    rebuild: DetectorRebuild,
) {
    register_detector_handle(handle, kind, rebuild);
}

fn register_detector_handle(
    handle: &Arc<PromotableDetector>,
    kind: HandleKind,
    rebuild: DetectorRebuild,
) {
    // The handle the coordinator prepares for, and the detector it is already
    // running, which is the prepared way back from the first move onward.
    let camera = rebuild.camera.clone();
    let classes = detection_classes_in_force();
    detection_transitions().register_handle(
        &rebuild.camera,
        kind,
        &rebuild.model_id,
        &classes.iter().map(String::as_str).collect::<Vec<&str>>(),
    );
    // A handle the coordinator did not keep is one whose work another handle
    // already does; preparing for it would load a second model for nothing.
    let registered = detection_transitions().registered_handles();
    if !registered.contains(&rebuild.camera) {
        return;
    }
    if let Some(backend) = detection_transitions().handle_backend(&rebuild.camera)
        && let Ok(mut prepared) = prepared_detectors().lock()
    {
        prepared.insert((rebuild.camera.clone(), backend), handle.snapshot());
    }
    if let Ok(mut applied) = applied_detection_request().lock() {
        *applied = Some((
            rebuild.accelerated_detection,
            pinned_backend(DETECTION_BACKEND_SETTING),
        ));
    }
    if let Ok(mut live) = detectors().lock() {
        live.retain(|entry| entry.handle.strong_count() > 0);
        // A handle the coordinator has since dropped — work whose detector a
        // camera now provides — is dropped here too, so no move rebuilds it.
        live.retain(|entry| registered.contains(&entry.camera));
        live.push(LiveDetector {
            handle: Arc::downgrade(handle),
            camera: camera.clone(),
            model_id: rebuild.model_id,
            build: rebuild.build,
            receipt: rebuild.receipt,
        });
    }
    // A move asked for before this handle existed is owed to it now. Startup
    // asks for a backend while the camera threads are still building their
    // detectors, so a move can land before any handle is there to take it on —
    // and a move dropped in that window leaves the camera on the backend it
    // booted with, with nothing but a restart to retry it.
    let owed = standing_request().is_some()
        && detection_transitions()
            .handle_backend(&camera)
            .zip(detection_transitions().state().requested_backend)
            .is_some_and(|(running, requested)| running != requested);
    if owed {
        PREPARATION_OWED.store(true, Ordering::SeqCst);
        start_preparation();
    }
}

/// Run the detection backend the store now asks for.
///
/// With detectors running, the answer is re-selected and every camera's
/// detector rebuilt under it — and the node reports the new backend as running
/// only once EVERY camera is on it. A change some cameras took on and others
/// did not is reported as partly taken on, naming the cameras on each side,
/// with the running value still naming the backend the rest are on. With no
/// detector running there is nothing to rebuild, so what stands is the answer
/// that settles without a probe, which is what a node with no camera runs.
pub fn reselect_detection_backend(accelerated_detection: bool) {
    let request: BackendRequest = (
        accelerated_detection,
        pinned_backend(DETECTION_BACKEND_SETTING),
    );
    // Already carried out: nothing to select, nothing to build, nothing to say.
    if request_already_carried_out(&request) {
        return;
    }
    let (request, version) = take_request(accelerated_detection, request);
    prepare_for_request(request, version);
}

/// Carry out one preparation pass for the request that took `version`.
///
/// Everything that changes what this node runs — installing a detector,
/// publishing the running backend, ending the attempt — happens only while
/// that request is still the one standing. A pass whose request has been
/// superseded installs nothing, publishes nothing and concludes nothing: the
/// operator's last word owns all three.
///
/// The refusal comes FIRST, before any selection or build, so a request that has
/// already been replaced costs nothing at all — no cold model load, and no
/// dependence on which backend a probe would have chosen.
fn prepare_for_request(requested: BackendRequest, version: u64) {
    if !request_still_stands(&requested, version) {
        return;
    }
    let accelerated_detection = requested.0;
    let live: Vec<DetectorToRebuild> = match detectors().lock() {
        Ok(mut entries) => {
            entries.retain(|entry| entry.handle.strong_count() > 0);
            entries
                .iter()
                .filter_map(|entry| {
                    entry.handle.upgrade().map(|handle| DetectorToRebuild {
                        handle,
                        camera: entry.camera.clone(),
                        model_id: entry.model_id.clone(),
                        build: Arc::clone(&entry.build),
                        receipt: Arc::clone(&entry.receipt),
                    })
                })
                .collect()
        }
        Err(_) => Vec::new(),
    };
    if live.is_empty() {
        if let Some(backend) =
            crate::detection_accel::settled_detection_backend(accelerated_detection)
        {
            bring_into_force(DETECTION_BACKEND_SETTING, SettingValue::text(backend));
        }
        return;
    }
    // The coordinator is what says whether there is anything left to do: a
    // request already carried out is a no-op, but only when every handle is
    // genuinely on the backend it named. A record of what was applied cannot
    // answer that on its own — a handle that never moved would be skipped for
    // ever on the strength of a matching value.
    if request_already_carried_out(&requested) {
        return;
    }

    // What this process is genuinely running right now, held so a failed
    // rebuild can put it back: the selection below reports its answer the
    // moment it has one, and an answer no detector was built under is a claim
    // about a machine that is still feeding frames through the old one.
    let running_before = in_force(DETECTION_BACKEND_SETTING);
    // The backend this node can actually enter, decided by building and
    // forward-testing ONE detector — the first camera's, which is then
    // installed as it is rather than loaded again.
    let probing = &live[0];
    report_preparation_progress(
        version,
        &format!(
            "building and forward-testing the detector for {}",
            probing.camera
        ),
    );
    let prepared_pass = crate::detection_accel::prepare_detection_backend(
        accelerated_detection,
        &probing.model_id,
        crate::yolox_detector::MODEL_INPUT_SHAPE,
        &|backend: &str| run_detector_build(probing.build.as_ref(), backend),
        &|backend: &str| prepared_detector(&probing.camera, backend).is_some(),
    );
    let selection = prepared_pass.selection;
    let mut forward_tested = prepared_pass.forward_tested;
    let running_before_text = running_before
        .as_ref()
        .map(SettingValue::to_string)
        .unwrap_or_default();
    // Camera by camera, because that is the granularity the machine actually
    // changes at: one camera's detector is replaced or it is not, and a
    // camera whose rebuild failed is still feeding frames through the backend
    // it was already on.
    // The request that asked for this work is the one allowed to act on it.
    // Read here, before anything is installed or published, because a check
    // taken after the build has finished always matches whatever request is
    // newest and can reject nothing.
    if detection_transitions().request_version() != version {
        return;
    }
    // Nothing is published here. The selection has decided what this node CAN
    // enter; what it IS running changes camera by camera below, and the running
    // claim moves with the last of those acts.
    let mut took_on: Vec<&str> = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    let mut failure: Option<String> = None;
    for entry in &live {
        report_preparation_progress(
            version,
            &format!("preparing the detector for {}", entry.camera),
        );
        // A detector already loaded for this camera on this backend is put
        // straight back: the whole cost of returning to a backend already
        // prepared is a pointer swap, and no model is loaded for it.
        let ready = match (
            forward_tested.take(),
            prepared_detector(&entry.camera, &selection.backend),
        ) {
            // The instance the forward test ran on belongs to the camera the
            // preparation ran for, which is this one: installing anything else
            // would install something nothing proved.
            (Some(tested), _) => Ok(ReadyDetector::FreshlyBuilt(tested)),
            (None, Some(prepared)) => Ok(ReadyDetector::AlreadyLoaded(prepared)),
            (None, None) => run_detector_build(entry.build.as_ref(), &selection.backend)
                .map(ReadyDetector::FreshlyBuilt),
        };
        match ready.and_then(|ready| {
            take_detector_on(
                &entry.camera,
                &selection.backend,
                version,
                &entry.handle,
                ready,
            )
            .map(|_instance| ())
        }) {
            Ok(()) => {
                // The receipt goes to this camera's surface only because THIS
                // camera's detector was genuinely replaced. Handing the same
                // success receipt to a camera whose rebuild failed would have
                // that camera report a backend it is not running.
                (entry.receipt)(&selection.receipt);
                took_on.push(entry.camera.as_str());
            }
            Err(error) => {
                kept.push(entry.camera.as_str());
                println!(
                    "detection_backend_not_taken_on camera={} requested={} running={} \
                     error={error}",
                    entry.camera, selection.backend, running_before_text
                );
                failure = Some(error);
            }
        }
    }
    // A request taken while this pass was working owns the node now: this one
    // installs nothing further, publishes nothing and concludes nothing.
    if detection_transitions().request_version() != version {
        return;
    }
    // The attempt is over: it completed or a preparation reported a real
    // error, which are the only two ways one ends.
    detection_transitions().conclude_attempt(version, failure);
    if took_on.is_empty() {
        // Nothing moved, so nothing new is running — and nothing was claimed
        // on the strength of the selection either, so the backend the
        // detectors are still using is already what this process names.
        return;
    }
    if kept.is_empty() {
        println!(
            "detection_backend_taken_on backend={} detectors={}",
            selection.backend,
            took_on.len()
        );
        record_applied(applied_detection_request(), requested);
        return;
    }
    // Some cameras moved and some did not, which is neither of the two answers
    // a single running value can carry. The node reports the backend the
    // cameras that did NOT move are still on, because the change an operator
    // asked for has not been taken on by this node — leaving the requested and
    // running fields apart, which is what the surface shows a gap with. The
    // cameras on each side are named here, and each camera's own receipt
    // already says what that camera is running. The running value already says
    // so: the coordinator names a backend only once every handle is on it, so
    // a partly-taken-on move leaves it naming the one the rest are running.
    println!(
        "detection_backend_partly_taken_on backend={} running={} took_on={} kept={} \
         cameras_took_on={} cameras_kept={}",
        selection.backend,
        running_before_text,
        took_on.len(),
        kept.len(),
        took_on.join(","),
        kept.join(","),
    );
    // Deliberately NOT recorded as applied: the request has not been met on
    // this node, so the next pass tries the cameras that kept the old backend
    // again rather than reading the request as settled.
}

/// How many camera pipelines this process is running, so a decode change knows
/// whether there is anything to reconnect.
static LIVE_STREAMS: AtomicUsize = AtomicUsize::new(0);

/// The decode intent the pipelines opened under, and whether it has been set at
/// all — a run that never registered a stream must not be told this process
/// decided something it did not.
static HARDWARE_DECODING: AtomicBool = AtomicBool::new(false);
static HARDWARE_DECODING_SET: AtomicBool = AtomicBool::new(false);

/// Bumped whenever the decode answer this node should be running changes. A
/// pipeline carries the value it opened under and ends its session when the two
/// part company, which is what makes the next session select afresh.
static DECODE_SELECTION_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Register a camera pipeline this process is running, with the decode intent
/// it is opening under.
pub(crate) fn register_live_stream(hardware_decoding: bool) {
    if !HARDWARE_DECODING_SET.swap(true, Ordering::SeqCst) {
        HARDWARE_DECODING.store(hardware_decoding, Ordering::SeqCst);
    }
    if let Ok(mut applied) = applied_decode_request().lock() {
        *applied = Some((
            hardware_decoding_in_force(hardware_decoding),
            pinned_backend(DECODE_BACKEND_SETTING),
        ));
    }
    LIVE_STREAMS.fetch_add(1, Ordering::SeqCst);
}

/// The decode intent a pipeline opening now runs under: what a live change
/// asked for, or `fallback` where nothing has.
pub fn hardware_decoding_in_force(fallback: bool) -> bool {
    if HARDWARE_DECODING_SET.load(Ordering::SeqCst) {
        HARDWARE_DECODING.load(Ordering::SeqCst)
    } else {
        fallback
    }
}

/// Which decode answer this node is on. A pipeline compares the value it opened
/// under against this one.
pub fn decode_selection_epoch() -> u64 {
    DECODE_SELECTION_EPOCH.load(Ordering::SeqCst)
}

/// Run the decode backend the store now asks for.
///
/// With pipelines running, this asks each of them to reconnect under the new
/// answer and reports nothing as running: the selection the fresh session
/// performs is what says which decode path it entered, so a hardware path that
/// no probe would open never gets claimed. With no pipeline running there is
/// nothing to reconnect, so what stands is the answer that settles without a
/// probe.
pub fn reselect_decode_backend(hardware_decoding: bool) {
    let requested: BackendRequest = (hardware_decoding, pinned_backend(DECODE_BACKEND_SETTING));
    HARDWARE_DECODING.store(hardware_decoding, Ordering::SeqCst);
    HARDWARE_DECODING_SET.store(true, Ordering::SeqCst);
    if LIVE_STREAMS.load(Ordering::SeqCst) == 0 {
        // Nothing to reconnect, so nothing is under way: the answer settles on
        // this call and the value in force and the absence of a transition are
        // published together.
        let _published = crate::settings_application::hold_publication();
        if let Some(backend) = crate::decode::settled_decode_backend(hardware_decoding) {
            bring_into_force(DECODE_BACKEND_SETTING, SettingValue::text(backend));
        }
        clear_decode_transition();
        record_applied(applied_decode_request(), requested);
        return;
    }
    if already_applied(applied_decode_request(), &requested) {
        return;
    }
    record_applied(applied_decode_request(), requested);
    // From here this node owes the operator a reconnect it performs itself, and
    // the surface says so rather than naming a restart: the pipelines end their
    // sessions on this epoch and the next ones select afresh under the path
    // just named. The transition names the request it is carrying, and is
    // stamped BEFORE that request becomes visible — a completion that can see
    // the new decode answer can always see the transition belonging to it, so
    // there is no moment where an unrelated session finds a new epoch and
    // nothing to check itself against. Both happen under one publication hold,
    // so no read lands between them either.
    let epoch = {
        let _published = crate::settings_application::hold_publication();
        let epoch = DECODE_SELECTION_EPOCH.load(Ordering::SeqCst) + 1;
        if let Ok(mut transition) = decode_transition().lock() {
            *transition = Some(DecodeTransition {
                since_ms: crate::clock::PersistedClock::contextdb().unix_millis(),
                epoch,
            });
        }
        DECODE_SELECTION_EPOCH.store(epoch, Ordering::SeqCst);
        epoch
    };
    println!(
        "decode_backend_reselect_requested hardware_decoding={hardware_decoding} epoch={epoch}"
    );
}

/// A decode change this node is carrying itself, as an operator surface reports
/// it: when the reconnect was asked for, and which request asked for it.
///
/// The epoch is what makes a completion answerable: a session reports the path
/// it entered for one particular decode answer, and only the session belonging
/// to the answer this node is now on may end this transition or publish its
/// outcome.
struct DecodeTransition {
    since_ms: u64,
    epoch: u64,
}

fn decode_transition() -> &'static Mutex<Option<DecodeTransition>> {
    static TRANSITION: OnceLock<Mutex<Option<DecodeTransition>>> = OnceLock::new();
    TRANSITION.get_or_init(|| Mutex::new(None))
}

/// When the decode reconnect now outstanding was asked for, if one is.
///
/// Present from the moment a change bumps the selection this node runs on until
/// the fresh session reports what it entered. No clock ends it — a transition
/// judged over by time passing is a transition reported as finished on a node
/// that never reconnected.
pub fn decode_transition_since_ms() -> Option<u64> {
    decode_transition()
        .lock()
        .ok()
        .and_then(|transition| transition.as_ref().map(|transition| transition.since_ms))
}

/// Which decode answer the reconnect now outstanding is carrying, if one is.
fn decode_transition_epoch() -> Option<u64> {
    decode_transition()
        .lock()
        .ok()
        .and_then(|transition| transition.as_ref().map(|transition| transition.epoch))
}

fn clear_decode_transition() {
    if let Ok(mut transition) = decode_transition().lock() {
        *transition = None;
    }
}

/// What a fresh session entered, reported as the decode path this node is
/// running — and, in the same act, the end of the transition that asked for it.
///
/// One act, held together so no read can land between them: a read that saw the
/// transition over and the old path still in force would be told to restart a
/// node already decoding exactly what was asked for.
///
/// `epoch` is the decode answer the settling session opened under. A session
/// that opened before the operator's command — one still finishing an RTSP
/// handshake when it landed — reports an outcome for a request nobody is
/// waiting on any more: it installs nothing and clears nothing, so the newer
/// request keeps its transition and is taken on by the session that genuinely
/// belongs to it. Without that, an older completion overturns a newer request
/// and the operator is sent to restart for a change this node is still
/// carrying.
pub(crate) fn settle_decode_selection(epoch: u64, hardware_accelerated: bool) {
    let _published = crate::settings_application::hold_publication();
    if epoch != DECODE_SELECTION_EPOCH.load(Ordering::SeqCst) {
        return;
    }
    // And the transition outstanding must be this request's own. A transition
    // is always stamped for the epoch it was published with, so a mismatch here
    // means the same thing a stale epoch does: this completion belongs to some
    // other request. No transition at all is the ordinary startup case — a
    // session selecting with nothing outstanding reports what it entered.
    if decode_transition_epoch().is_some_and(|outstanding| outstanding != epoch) {
        return;
    }
    bring_into_force(
        DECODE_BACKEND_SETTING,
        SettingValue::text(if hardware_accelerated {
            crate::settings_backends::HARDWARE_DECODE_BACKEND
        } else {
            crate::settings_backends::SOFTWARE_DECODE_BACKEND
        }),
    );
    clear_decode_transition();
}

fn already_applied(applied: &Mutex<Option<BackendRequest>>, requested: &BackendRequest) -> bool {
    matches!(applied.lock(), Ok(ref held) if held.as_ref() == Some(requested))
}

fn record_applied(applied: &Mutex<Option<BackendRequest>>, requested: BackendRequest) {
    if let Ok(mut held) = applied.lock() {
        *held = Some(requested);
    }
}

// ── Live detection backend take-on (cold review r4 finding 9, session 8 lane
// 6) ─────────────────────────────────────────────────────────────────────
//
// `register_live_detector`/`DetectorRebuild`/`PromotableDetector::new` are all
// `pub(crate)` — a file under `tests/` compiles as a separate crate and
// cannot see them, and `runtime.rs` (the only production caller) is itself a
// private `mod`, not `pub mod`, so there is no external entry point that
// would register a detector organically either. This inline `#[cfg(test)]`
// module is the established convention this crate already uses everywhere
// else a seam is `pub(crate)`-only (clock.rs, settings.rs, decode.rs,
// camera_hub.rs, supervisor.rs, fabric.rs, site_channel.rs, secret.rs,
// media_pipeline.rs, config.rs, runtime.rs all do this) — `use super::*`
// reaches the pub(crate) surface without changing its visibility.
//
// `detectors()`, `applied_detection_request()`, and the settings
// projection's running-value registry are process-global statics, so two of
// these tests running concurrently in the same process would corrupt each
// other's registrations and running values. Rather than a shared `Mutex<()>`
// (which only serializes access — it cannot undo one test's registrations
// before the next runs, and this module has no reset hook to add without
// touching production code), each contract runs its scenario in its own
// `#[ignore]`d child test, re-executed as a fresh OS process by the visible
// test that owns it (the same self-re-exec-with-piped-stdout technique
// `tests/store_open_failure_classes.rs` already uses). Each child gets an
// empty registry and an unset running value by construction, and its real
// `println!` output — the operator-visible statement — is captured over the
// pipe instead of being swallowed by the test harness's own capture buffer.
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::mpsc;

    /// A detector whose only observable behavior is the model hash it
    /// reports.
    ///
    /// A build carrying the accelerated detector proves the backend by running
    /// a forward pass through the very detector it is about to install, so
    /// `detect_segment` IS reached on that shape — the preparation calls it.
    /// It refuses, plainly: a stub decodes nothing, and refusing is what keeps
    /// the backend these contracts run on the same on every machine, with a
    /// graphics device or without one. Panicking here instead would put an
    /// unreachable-guard panic inside the preparation's own forward test.
    struct StubDetector {
        sha: String,
    }

    impl Detector for StubDetector {
        fn model_sha256(&self) -> &str {
            &self.sha
        }

        fn detect_segment(
            &self,
            _media: &crate::media_pipeline::DecodedVideoSegment,
            _clip_sha256: String,
            _sample_frames: usize,
            _confidence_threshold: f64,
        ) -> Result<crate::yolox_detector::DetectorOutput, String> {
            Err("this stub detector decodes nothing".to_string())
        }
    }

    /// How many times one preparation pass asks its FIRST camera to build.
    ///
    /// A build carrying the accelerated detector enters that backend only on
    /// evidence from the machine in front of it: it builds the first
    /// registered camera's detector and runs a forward pass through it. The
    /// stub above refuses that pass, so the processor detector is then built
    /// for that same camera and installed — two builds for the one camera the
    /// preparation forward-tests. Where this artifact carries no accelerated
    /// detector at all, or the machine shows no device to try, there is
    /// nothing to prove and the install build is the whole of it. Every other
    /// camera builds once per pass either way.
    fn builds_per_pass_for_the_forward_tested_camera() -> usize {
        #[cfg(feature = "detect-burn-wgpu")]
        {
            1 + usize::from(crate::yolox_detector::find_hardware_adapter().is_some())
        }
        #[cfg(not(feature = "detect-burn-wgpu"))]
        {
            1
        }
    }

    /// Wait until the preparation threads — the production ones — have no work
    /// left.
    ///
    /// The wait is on the work itself being over. Not on an amount of time
    /// passing, and not on a fixed number of turns either: a preparation that
    /// legitimately does more work, as the accelerated shape's forward test
    /// does, outlives any count and is read as finished when it has not
    /// started. Nothing here reads a clock, by the same standing rule the
    /// production path follows — a bound here would judge by elapsed time the
    /// very thing these contracts say is never judged that way. A preparation
    /// that genuinely wedges is the harness's timeout to report, not this
    /// helper's.
    fn wait_for_preparations_to_finish() {
        while PREPARATION_RUNNING.load(Ordering::SeqCst) || PREPARATION_OWED.load(Ordering::SeqCst)
        {
            std::thread::yield_now();
        }
    }

    /// Registers one camera's live detector, with a build closure whose
    /// success is fixed by `succeed` and a receipt sink — both counted, so a
    /// test can observe exactly how many times each fired without parsing
    /// anything. Returns the strong handle the caller must keep alive: the
    /// registry holds only a weak reference, and a dropped handle is pruned
    /// the next time any camera registers or `reselect_detection_backend`
    /// runs.
    fn register_camera(
        camera: &str,
        model_id: &str,
        accelerated_detection: bool,
        succeed: bool,
        build_calls: Arc<AtomicUsize>,
        receipt_calls: Arc<AtomicUsize>,
    ) -> Arc<PromotableDetector> {
        let handle = Arc::new(PromotableDetector::new(Box::new(StubDetector {
            sha: format!("{camera}-initial"),
        })));
        let build_camera = camera.to_string();
        register_live_detector(
            &handle,
            DetectorRebuild {
                camera: camera.to_string(),
                model_id: model_id.to_string(),
                accelerated_detection,
                build: Arc::new(move |backend: &str| {
                    build_calls.fetch_add(1, AtomicOrdering::SeqCst);
                    if succeed {
                        Ok(Box::new(StubDetector {
                            sha: format!("{build_camera}-{backend}"),
                        }) as Box<dyn Detector>)
                    } else {
                        Err(format!("{build_camera} build refused"))
                    }
                }),
                receipt: Arc::new(move |_receipt: &AccelerationReceipt| {
                    receipt_calls.fetch_add(1, AtomicOrdering::SeqCst);
                }),
            },
        );
        handle
    }

    /// Re-executes this very test binary, selecting only the named `#[ignore]`d
    /// child test, so it runs in a fresh process with an empty detector
    /// registry and an unset running value — and so its `println!` output
    /// reaches a real pipe instead of the harness's own per-test capture
    /// buffer. Panics (surfacing the child's full captured output) if the
    /// child does not exit successfully, which is also what a failed
    /// `assert!` inside the child produces.
    fn assert_isolated_child_passed(child_test_name: &str) -> String {
        let this_binary = std::env::current_exe().expect("this test binary's own path");
        let output = Command::new(&this_binary)
            .arg("--exact")
            .arg(format!("live_backends::tests::{child_test_name}"))
            .arg("--ignored")
            .arg("--nocapture")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn the isolated child test process");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success() && combined.contains("child_assertions_passed"),
            "isolated child test {child_test_name} did not pass (status {:?}); this \
             matches on a marker line the child prints only after every assertion inside \
             it succeeds, so a filter typo that silently ran zero tests fails here too; \
             combined child output:\n{combined}",
            output.status
        );
        combined
    }

    // ── Contract 1: partial success reports partial, with per-camera truth,
    // and is not recorded applied ───────────────────────────────────────────

    #[test]
    fn partial_success_reports_partial_and_retries_only_the_failed_cameras_on_repeat() {
        let output =
            assert_isolated_child_passed("partial_success_and_repeat_retries_failed_child");
        assert!(
            output.contains(
                "detection_backend_partly_taken_on backend=burn-cpu running=seed-partial \
                 took_on=1 kept=2 cameras_took_on=front cameras_kept=back,side"
            ),
            "the partly-taken-on statement must name the camera that moved (front) and the \
             two that did not (back, side); combined child output:\n{output}"
        );
    }

    #[test]
    #[ignore = "re-executed in isolation by \
                partial_success_reports_partial_and_retries_only_the_failed_cameras_on_repeat; \
                mutates the process-global detector registry and running-value registry, so \
                it must never run concurrently with another contract in this file"]
    fn partial_success_and_repeat_retries_failed_child() {
        let front_builds = Arc::new(AtomicUsize::new(0));
        let back_builds = Arc::new(AtomicUsize::new(0));
        let side_builds = Arc::new(AtomicUsize::new(0));
        let front_receipts = Arc::new(AtomicUsize::new(0));
        let back_receipts = Arc::new(AtomicUsize::new(0));
        let side_receipts = Arc::new(AtomicUsize::new(0));

        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-partial"),
        );

        // Registered under accelerated_detection=false; the reselect call
        // below asks for true, so the request genuinely differs from what
        // registration already recorded as applied (registration itself
        // records an applied tuple for the intent the camera booted under).
        let _front = register_camera(
            "front",
            "yolox-test",
            false,
            true,
            Arc::clone(&front_builds),
            Arc::clone(&front_receipts),
        );
        let _back = register_camera(
            "back",
            "yolox-test",
            false,
            false,
            Arc::clone(&back_builds),
            Arc::clone(&back_receipts),
        );
        let _side = register_camera(
            "side",
            "yolox-test",
            false,
            false,
            Arc::clone(&side_builds),
            Arc::clone(&side_receipts),
        );

        reselect_detection_backend(true);

        assert_eq!(
            front_builds.load(AtomicOrdering::SeqCst),
            builds_per_pass_for_the_forward_tested_camera(),
            "front is the camera the preparation forward-tests, so its closure fires for \
             the backend being proved as well as for the one installed"
        );
        assert_eq!(back_builds.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(side_builds.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            Some(SettingValue::text("seed-partial")),
            "the running value must stay the pre-change backend while only one of three \
             cameras took the change on"
        );

        // An identical second call must not be read as settled: the request
        // was deliberately not recorded as applied, so this re-attempts the
        // cameras that kept the old backend (observed as their build
        // closures firing again).
        reselect_detection_backend(true);
        assert_eq!(
            back_builds.load(AtomicOrdering::SeqCst),
            2,
            "a camera that failed to rebuild must be retried by an identical repeat call"
        );
        assert_eq!(
            side_builds.load(AtomicOrdering::SeqCst),
            2,
            "a camera that failed to rebuild must be retried by an identical repeat call"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 2: receipts fire only for the rebuilt camera ───────────────

    #[test]
    fn receipts_fire_only_for_the_rebuilt_camera() {
        assert_isolated_child_passed("receipts_fire_only_for_rebuilt_camera_child");
    }

    #[test]
    #[ignore = "re-executed in isolation by receipts_fire_only_for_the_rebuilt_camera; \
                mutates the process-global detector registry and running-value registry"]
    fn receipts_fire_only_for_rebuilt_camera_child() {
        let ok_builds = Arc::new(AtomicUsize::new(0));
        let fail_builds = Arc::new(AtomicUsize::new(0));
        let ok_receipts = Arc::new(AtomicUsize::new(0));
        let fail_receipts = Arc::new(AtomicUsize::new(0));

        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-receipts"),
        );

        let _ok = register_camera(
            "rebuilt",
            "yolox-test",
            false,
            true,
            Arc::clone(&ok_builds),
            Arc::clone(&ok_receipts),
        );
        let _fail = register_camera(
            "kept",
            "yolox-test",
            false,
            false,
            Arc::clone(&fail_builds),
            Arc::clone(&fail_receipts),
        );

        reselect_detection_backend(true);

        assert_eq!(
            ok_receipts.load(AtomicOrdering::SeqCst),
            1,
            "the camera whose build succeeded must receive exactly one receipt"
        );
        assert_eq!(
            fail_receipts.load(AtomicOrdering::SeqCst),
            0,
            "a camera whose build failed must never receive the success receipt — its \
             surface would then describe a backend it is not running"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 3: all-fail restores, with no receipts and no taken-on
    // statement ──────────────────────────────────────────────────────────────

    #[test]
    fn all_fail_restores_running_prints_no_taken_on_statement_and_stays_unapplied() {
        let output = assert_isolated_child_passed("all_fail_restores_and_stays_unapplied_child");
        assert!(
            !output.contains("detection_backend_taken_on backend=")
                && !output.contains("detection_backend_partly_taken_on"),
            "an all-fail rebuild must print neither the full nor the partial taken-on \
             statement — nothing moved, so nothing was taken on; combined child \
             output:\n{output}"
        );
    }

    #[test]
    #[ignore = "re-executed in isolation by \
                all_fail_restores_running_prints_no_taken_on_statement_and_stays_unapplied; \
                mutates the process-global detector registry and running-value registry"]
    fn all_fail_restores_and_stays_unapplied_child() {
        let front_builds = Arc::new(AtomicUsize::new(0));
        let back_builds = Arc::new(AtomicUsize::new(0));
        let front_receipts = Arc::new(AtomicUsize::new(0));
        let back_receipts = Arc::new(AtomicUsize::new(0));

        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-all-fail"),
        );

        let _front = register_camera(
            "front",
            "yolox-test",
            false,
            false,
            Arc::clone(&front_builds),
            Arc::clone(&front_receipts),
        );
        let _back = register_camera(
            "back",
            "yolox-test",
            false,
            false,
            Arc::clone(&back_builds),
            Arc::clone(&back_receipts),
        );

        reselect_detection_backend(true);

        assert_eq!(
            front_builds.load(AtomicOrdering::SeqCst),
            builds_per_pass_for_the_forward_tested_camera(),
            "front is the camera the preparation forward-tests, so its closure fires for \
             the backend being proved as well as for the one installed"
        );
        assert_eq!(back_builds.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(
            front_receipts.load(AtomicOrdering::SeqCst),
            0,
            "no receipt fires when every rebuild fails"
        );
        assert_eq!(
            back_receipts.load(AtomicOrdering::SeqCst),
            0,
            "no receipt fires when every rebuild fails"
        );
        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            Some(SettingValue::text("seed-all-fail")),
            "the running value must be restored to the pre-change backend when nothing \
             moved"
        );

        // Not recorded as applied: an identical repeat call must proceed
        // again rather than reading the request as settled.
        reselect_detection_backend(true);
        assert_eq!(
            front_builds.load(AtomicOrdering::SeqCst),
            2 * builds_per_pass_for_the_forward_tested_camera(),
            "an all-fail attempt must not be recorded as applied, so a repeat call \
             re-attempts every camera"
        );
        assert_eq!(
            back_builds.load(AtomicOrdering::SeqCst),
            2,
            "an all-fail attempt must not be recorded as applied, so a repeat call \
             re-attempts every camera"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 4: full success updates running, fires every receipt, and
    // is recorded applied so an identical repeat is a no-op ─────────────────

    #[test]
    fn full_success_updates_running_fires_every_receipt_and_applies() {
        let output = assert_isolated_child_passed("full_success_updates_and_applies_child");
        assert!(
            output.contains("detection_backend_taken_on backend=burn-cpu detectors=2"),
            "the full taken-on statement must carry the selected backend and the detector \
             count; combined child output:\n{output}"
        );
    }

    #[test]
    #[ignore = "re-executed in isolation by \
                full_success_updates_running_fires_every_receipt_and_applies; mutates the \
                process-global detector registry and running-value registry"]
    fn full_success_updates_and_applies_child() {
        let front_builds = Arc::new(AtomicUsize::new(0));
        let back_builds = Arc::new(AtomicUsize::new(0));
        let front_receipts = Arc::new(AtomicUsize::new(0));
        let back_receipts = Arc::new(AtomicUsize::new(0));

        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-full-success"),
        );

        let front = register_camera(
            "front",
            "yolox-test",
            false,
            true,
            Arc::clone(&front_builds),
            Arc::clone(&front_receipts),
        );
        let back = register_camera(
            "back",
            "yolox-test",
            false,
            true,
            Arc::clone(&back_builds),
            Arc::clone(&back_receipts),
        );

        reselect_detection_backend(true);

        assert_eq!(
            front_builds.load(AtomicOrdering::SeqCst),
            builds_per_pass_for_the_forward_tested_camera(),
            "front is the camera the preparation forward-tests, so its closure fires for \
             the backend being proved as well as for the one installed"
        );
        assert_eq!(back_builds.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(
            front_receipts.load(AtomicOrdering::SeqCst),
            1,
            "every camera's receipt must fire exactly once on full success"
        );
        assert_eq!(
            back_receipts.load(AtomicOrdering::SeqCst),
            1,
            "every camera's receipt must fire exactly once on full success"
        );
        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            Some(SettingValue::text(
                crate::detection_accel::CPU_DETECTION_BACKEND
            )),
            "the running value must be the selected backend once every camera has taken \
             it on"
        );

        // Recorded as applied: an identical repeat call loads nothing. Read on
        // the cameras rather than on a build counter, because a build counter
        // cannot tell a camera being rebuilt from the preparation proving a
        // backend it has not entered — and it is the camera that must be left
        // alone. Each one is still running the very instance it was already
        // running: same pointer, so no model was loaded for either of them.
        let front_running = front.snapshot();
        let back_running = back.snapshot();
        reselect_detection_backend(true);
        assert!(
            Arc::ptr_eq(&front_running, &front.snapshot()),
            "a request already recorded as applied must not rebuild a camera's detector — \
             this camera must still be running the instance it already had"
        );
        assert!(
            Arc::ptr_eq(&back_running, &back.snapshot()),
            "a request already recorded as applied must not rebuild a camera's detector — \
             this camera must still be running the instance it already had"
        );
        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            Some(SettingValue::text(
                crate::detection_accel::CPU_DETECTION_BACKEND
            )),
            "and the running value must be untouched by the repeat"
        );

        println!("child_assertions_passed");
    }

    /// A request already recorded as applied performs ZERO physical builds.
    ///
    /// The neighbouring full-success contract reads the repeat on the CAMERAS
    /// — same instance pointer, so no model was loaded for either of them — and
    /// says in its own comment that a build counter cannot tell a camera being
    /// rebuilt from the preparation proving a backend it has not entered. That
    /// objection is answered here rather than argued with, because it holds for
    /// the FIRST pass and not for this one: on a first pass the honest count is
    /// some number that depends on whether this machine has a device to prove a
    /// backend on, which is exactly what a counter cannot interpret. On a
    /// REPEAT of a request already recorded as applied the honest count is
    /// ZERO — zero rebuilds and zero proving builds alike — and zero is the one
    /// number that needs no interpreting.
    ///
    /// What the pointer check cannot see is the whole reason to have this: a
    /// preparation that rebuilt a detector and then installed the SAME instance
    /// back onto every handle passes the pointer check while having loaded the
    /// models all over again. The cost of a repeat is a cold model load per
    /// camera, on a box beside a camera, for a request that had already been
    /// carried out — so what is pinned is the load itself, counted where a
    /// registered rebuild closure fires and never where an instance is
    /// installed.
    #[test]
    fn a_repeat_of_an_applied_request_performs_zero_detector_builds() {
        assert_isolated_child_passed("a_repeat_of_an_applied_request_performs_zero_builds_child");
    }

    #[test]
    #[ignore = "re-executed in isolation by \
                a_repeat_of_an_applied_request_performs_zero_detector_builds; mutates the \
                process-global detector registry and running-value registry"]
    fn a_repeat_of_an_applied_request_performs_zero_builds_child() {
        let front_builds = Arc::new(AtomicUsize::new(0));
        let back_builds = Arc::new(AtomicUsize::new(0));
        let front_receipts = Arc::new(AtomicUsize::new(0));
        let back_receipts = Arc::new(AtomicUsize::new(0));

        // A running backend nothing has a detector loaded for yet, so the first
        // pass has real work to do and the repeat below is a repeat of
        // something that genuinely happened.
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-zero-build-repeat"),
        );

        let front = register_camera(
            "front",
            "yolox-test",
            false,
            true,
            Arc::clone(&front_builds),
            Arc::clone(&front_receipts),
        );
        let back = register_camera(
            "back",
            "yolox-test",
            false,
            true,
            Arc::clone(&back_builds),
            Arc::clone(&back_receipts),
        );

        reselect_detection_backend(true);
        wait_for_preparations_to_finish();

        // Established before anything is concluded: the first pass really did
        // build, so a zero delta below is a repeat doing nothing rather than a
        // counter that never moves.
        assert!(
            detector_builds_performed() > 0,
            "this run never established the condition it exists to test — the first pass \
             performed no detector build at all, so a repeat performing none proves nothing"
        );
        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            Some(SettingValue::text(
                crate::detection_accel::CPU_DETECTION_BACKEND
            )),
            "sanity: the first pass must have been carried out and recorded as applied"
        );

        let builds_before_the_repeat = detector_builds_performed();
        let front_builds_before = front_builds.load(AtomicOrdering::SeqCst);
        let back_builds_before = back_builds.load(AtomicOrdering::SeqCst);
        let front_running = front.snapshot();
        let back_running = back.snapshot();

        reselect_detection_backend(true);
        wait_for_preparations_to_finish();
        let builds_after_the_repeat = detector_builds_performed();

        assert_eq!(
            builds_after_the_repeat - builds_before_the_repeat,
            0,
            "a request already recorded as applied must perform ZERO detector builds of any \
             kind — not a rebuild for a camera already running that backend, and not a build \
             to prove a backend this node has already entered. Each one is a cold model load \
             nobody asked for"
        );
        // The per-camera closures say the same thing from the other side, so
        // the process-wide count cannot be zero because it stopped counting.
        assert_eq!(
            front_builds.load(AtomicOrdering::SeqCst),
            front_builds_before,
            "the repeat fired this camera's own rebuild closure"
        );
        assert_eq!(
            back_builds.load(AtomicOrdering::SeqCst),
            back_builds_before,
            "the repeat fired this camera's own rebuild closure"
        );
        // And the cameras are untouched, which is the other half of the same
        // fact: nothing was built, so nothing was installed either.
        assert!(
            Arc::ptr_eq(&front_running, &front.snapshot()),
            "this camera must still be running the instance it already had"
        );
        assert!(
            Arc::ptr_eq(&back_running, &back.snapshot()),
            "this camera must still be running the instance it already had"
        );
        assert_eq!(
            front_receipts.load(AtomicOrdering::SeqCst),
            1,
            "and no camera reported a second selection receipt for work that did not happen"
        );
        assert_eq!(
            back_receipts.load(AtomicOrdering::SeqCst),
            1,
            "and no camera reported a second selection receipt for work that did not happen"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 5: zero registered detectors brings the settled, probe-free
    // answer into force without attempting a rebuild ────────────────────────

    #[test]
    fn zero_registered_detectors_settles_without_attempting_a_rebuild() {
        let output = assert_isolated_child_passed("zero_registered_settles_without_rebuild_child");
        assert!(
            !output.contains("detection_backend_taken_on")
                && !output.contains("detection_backend_not_taken_on"),
            "with no detector registered there is nothing to rebuild, so neither taken-on \
             statement should ever print; combined child output:\n{output}"
        );
    }

    #[test]
    #[ignore = "re-executed in isolation by \
                zero_registered_detectors_settles_without_attempting_a_rebuild; mutates the \
                process-global running-value registry"]
    fn zero_registered_settles_without_rebuild_child() {
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-unrelated-prior-value"),
        );

        // No `register_camera` call at all: the registry is empty by
        // construction in this fresh process.
        reselect_detection_backend(false);

        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            Some(SettingValue::text(
                crate::detection_accel::CPU_DETECTION_BACKEND
            )),
            "with no camera running, the settled probe-free answer must come into force"
        );

        println!("child_assertions_passed");
    }

    // ── The same contracts, on the path production actually takes ──────────
    //
    // The coordinator's own contracts are frozen against its API. These four
    // drive the entry points a settings command and startup call, because a
    // guarantee that holds only when a test calls the coordinator directly is
    // not a guarantee an operator has.

    /// Registers a camera whose build closure is under this test's control:
    /// the FIRST build blocks until released, every later build returns at
    /// once, and each returns a distinguishable instance so a test can tell
    /// which build ended up on the handle.
    fn register_camera_with_gated_build(
        camera: &str,
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        build_calls: Arc<AtomicUsize>,
    ) -> Arc<PromotableDetector> {
        let handle = Arc::new(PromotableDetector::new(Box::new(StubDetector {
            sha: format!("{camera}-initial"),
        })));
        let build_camera = camera.to_string();
        let release = Mutex::new(Some(release));
        register_live_detector(
            &handle,
            DetectorRebuild {
                camera: camera.to_string(),
                model_id: "yolox-test".to_string(),
                accelerated_detection: false,
                build: Arc::new(move |_backend: &str| {
                    let call = build_calls.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                    if call == 1 {
                        let _ = started.send(());
                        if let Some(release) = release.lock().expect("release gate").take() {
                            let _ = release.recv();
                        }
                    }
                    Ok(Box::new(StubDetector {
                        sha: format!("{build_camera}-build-{call}"),
                    }) as Box<dyn Detector>)
                }),
                receipt: Arc::new(move |_receipt: &AccelerationReceipt| {}),
            },
        );
        handle
    }

    // ── Contract 6: a superseded preparation installs nothing, on the
    // production path ───────────────────────────────────────────────────────

    #[test]
    fn a_superseded_preparation_installs_nothing_through_the_production_path() {
        assert_isolated_child_passed("superseded_preparation_installs_nothing_child");
    }

    #[test]
    #[ignore = "re-executed in isolation by \
                a_superseded_preparation_installs_nothing_through_the_production_path; mutates \
                the process-global detector registry, coordinator and running-value registry"]
    fn superseded_preparation_installs_nothing_child() {
        // A running backend nothing has a detector loaded for yet, so the
        // first move has real work to do: a handle already holding a detector
        // for the backend a move selects is put back by pointer and builds
        // nothing, which would leave these contracts observing an act that
        // never happened.
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-superseded"),
        );
        // The operator asks for one backend, the cold build runs, and while it
        // runs they change their mind. The first build then finishes. It must
        // install nothing: the detector the cameras run, the backend the node
        // reports, and the state of the request now standing all belong to the
        // second command. This is the version guard, read where production
        // reads it — a guard taken after the build has finished always matches
        // the newest request and can never reject anything.
        let builds = Arc::new(AtomicUsize::new(0));
        let (started, build_started) = mpsc::channel();
        let (release, wait_for_release) = mpsc::channel();
        let handle = register_camera_with_gated_build(
            "front",
            started,
            wait_for_release,
            Arc::clone(&builds),
        );

        let first = std::thread::spawn(|| request_detection_backend(true));
        build_started
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the first preparation must reach its build");

        // The change of mind, through the entry point a settings command uses.
        request_detection_backend(false);
        let superseding_version = detection_transitions().request_version();

        // Let the abandoned build finish and offer its result.
        let _ = release.send(());
        first.join().expect("the first request thread");
        // The preparation threads are the production ones; wait for the work
        // to be over rather than for any amount of time to pass.
        wait_for_preparations_to_finish();

        assert_eq!(
            detection_transitions().request_version(),
            superseding_version,
            "the request standing at the end is the second one"
        );
        assert_eq!(
            handle.snapshot().model_sha256(),
            "front-build-2",
            "the detector the camera runs must be the one the request now standing produced; a \
             superseded build that installs itself puts frames through a backend the operator \
             has already moved away from"
        );
        let state = detection_transitions().state();
        assert!(
            state.pending.is_none(),
            "the superseded build must not leave the current request looking unfinished"
        );
        assert!(
            state.failure_reason.is_none(),
            "and it must not publish its own outcome under the request that replaced it"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 7: a preparation under way says what it is doing, on the
    // production path ───────────────────────────────────────────────────────

    #[test]
    fn a_live_preparation_reports_its_progress() {
        assert_isolated_child_passed("live_preparation_reports_progress_child");
    }

    #[test]
    #[ignore = "re-executed in isolation by a_live_preparation_reports_its_progress; mutates the \
                process-global detector registry and coordinator"]
    fn live_preparation_reports_progress_child() {
        // A running backend nothing has a detector loaded for yet, so the
        // first move has real work to do: a handle already holding a detector
        // for the backend a move selects is put back by pointer and builds
        // nothing, which would leave these contracts observing an act that
        // never happened.
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-progress"),
        );
        // Preparing-since alone cannot tell slow from wedged: it says only that
        // something started. What the preparation is doing has to come from the
        // preparation itself, on the real path, or the surface an operator
        // watches during a long cold build is a timestamp and nothing else.
        let builds = Arc::new(AtomicUsize::new(0));
        let (started, build_started) = mpsc::channel();
        let (release, wait_for_release) = mpsc::channel();
        let _handle = register_camera_with_gated_build(
            "front",
            started,
            wait_for_release,
            Arc::clone(&builds),
        );

        let preparing = std::thread::spawn(|| request_detection_backend(true));
        build_started
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the preparation must reach its build");

        let pending = detection_transitions()
            .state()
            .pending
            .expect("a preparation is under way");
        assert!(
            pending.preparing_since_ms > 0,
            "the surface must say when this preparation began"
        );
        let progress = pending.latest_progress.unwrap_or_default();
        assert!(
            !progress.trim().is_empty(),
            "the live preparation must report what it is doing, so a person watching a long \
             cold build can tell it apart from a wedged one"
        );

        let _ = release.send(());
        preparing.join().expect("the request thread");

        println!("child_assertions_passed");
    }

    // ── Contract 8: two requests for the same thing are one build, on the
    // production path ───────────────────────────────────────────────────────

    #[test]
    fn two_requests_for_the_same_backend_are_one_build() {
        assert_isolated_child_passed("two_requests_are_one_build_child");
    }

    #[test]
    #[ignore = "re-executed in isolation by two_requests_for_the_same_backend_are_one_build; \
                mutates the process-global detector registry and coordinator"]
    fn two_requests_are_one_build_child() {
        // A running backend nothing has a detector loaded for yet, so the
        // first move has real work to do: a handle already holding a detector
        // for the backend a move selects is put back by pointer and builds
        // nothing, which would leave these contracts observing an act that
        // never happened.
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-one-build"),
        );
        // Asking twice for what is already being prepared attaches to that
        // work. A second physical build is a second cold model load nobody
        // asked for, and repeating the command is the documented action, so
        // this is the path where abandoned builds would pile up.
        let builds = Arc::new(AtomicUsize::new(0));
        let (started, build_started) = mpsc::channel();
        let (release, wait_for_release) = mpsc::channel();
        let _handle = register_camera_with_gated_build(
            "front",
            started,
            wait_for_release,
            Arc::clone(&builds),
        );

        let first = std::thread::spawn(|| request_detection_backend(true));
        build_started
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the first preparation must reach its build");
        // The same thing asked for again while the first build is still going.
        request_detection_backend(true);

        let _ = release.send(());
        first.join().expect("the first request thread");
        for _ in 0..10_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
            {
                break;
            }
            std::thread::yield_now();
        }

        assert_eq!(
            builds.load(AtomicOrdering::SeqCst),
            1,
            "one identity asked for twice is one physical build"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 9: the settings answer carries the transition the command
    // itself started ────────────────────────────────────────────────────────
    //
    // A manual backend command returns before the preparation is done, which
    // is the whole point of the non-blocking path. What the operator reads in
    // that window is this answer, and it is the only place they can read it:
    // the coordinator's pending state reaches the acceleration receipts and
    // stops there. So the answer has to name the transition, say when it
    // began, and carry the last thing the preparation said — and it must not
    // send the operator to restart a node that is already closing the gap by
    // itself.

    /// One rendered setting line out of a `vigil settings` answer.
    fn setting_line<'a>(answer: &'a str, name: &str) -> &'a str {
        let prefix = format!("{} ", crate::settings_projection::SETTING_LINE_PREFIX);
        let needle = format!(" {}={name} ", crate::settings_projection::NAME_KEY);
        answer
            .lines()
            .filter(|line| line.starts_with(&prefix))
            .find(|line| line.contains(&needle))
            .unwrap_or_else(|| panic!("the operator surface answers for {name}; got:\n{answer}"))
    }

    /// One whitespace-free field out of a rendered setting line.
    fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
        let needle = format!(" {key}=");
        let start = line.find(&needle)? + needle.len();
        let rest = &line[start..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        Some(&rest[..end])
    }

    /// The field naming when the preparation now under way began. Spelled here
    /// because the projection has no key for it yet: the coordinator's
    /// preparing-since reaches the acceleration receipt and no operator
    /// surface, and this is the surface an operator actually reads after
    /// typing the command.
    const PREPARING_SINCE_KEY: &str = "preparing-since";

    #[test]
    fn the_settings_answer_names_the_live_transition_its_own_command_started() {
        assert_isolated_child_passed("settings_answer_names_live_transition_child");
    }

    #[test]
    #[ignore = "re-executed in isolation by \
                the_settings_answer_names_the_live_transition_its_own_command_started; mutates \
                the process-global detector registry, coordinator and running-value registry"]
    fn settings_answer_names_live_transition_child() {
        // This process is the one running the cameras, so a change typed here
        // is applied here — which is what puts a transition under way while the
        // answer is being rendered.
        crate::settings_application::mark_running_process();
        let deployment = tempfile::tempdir().expect("a temporary deployment directory");
        // Naming a backend by hand means taking the wheel off the automation
        // first, which is the only way an operator reaches this command at
        // all. Written straight to the store so the transition under test is
        // the one the command below starts, and not one this setup left over.
        {
            let store = crate::settings_store::SettingsStore::open(deployment.path())
                .expect("a store for the deployment under test");
            store
                .set_local(
                    crate::settings_domains::ACCELERATED_DETECTION_DOMAIN,
                    crate::settings_model::Surface::VigilSettings,
                    crate::settings_model::Scope::node(crate::node_key::scope_name(
                        deployment.path(),
                    )),
                    SettingValue::Bool(false),
                )
                .expect("turn the automation off");
        }
        // A running backend nothing has a detector loaded for yet, so the move
        // the command asks for has real work to do.
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-transition"),
        );
        let builds = Arc::new(AtomicUsize::new(0));
        let (started, build_started) = mpsc::channel();
        let (release, wait_for_release) = mpsc::channel();
        let _handle = register_camera_with_gated_build(
            "front",
            started,
            wait_for_release,
            Arc::clone(&builds),
        );

        // The real CLI answer path, with the real store underneath it.
        let answer = crate::settings_command::answer(
            deployment.path(),
            &crate::settings_store::SettingsStore::store_path(deployment.path()),
            &format!(
                "set {DETECTION_BACKEND_SETTING} {}",
                crate::detection_accel::CPU_DETECTION_BACKEND
            ),
        );
        assert!(
            !crate::settings_command::answer_failed(&answer),
            "naming a backend this artifact carries is an ordinary change; got:\n{answer}"
        );

        // A preparation is demonstrably outstanding: this test is holding its
        // build open, so nothing about the state below is timing.
        assert!(
            detection_transitions().state().pending.is_some(),
            "the command started a transition the coordinator is carrying"
        );
        let line = setting_line(&answer, DETECTION_BACKEND_SETTING);
        assert!(
            field(line, PREPARING_SINCE_KEY)
                .and_then(|since| since.parse::<u64>().ok())
                .is_some_and(|since| since > 0),
            "the answer must say when this preparation began, or a person watching a long cold \
             build has no way to tell slow from wedged; got: {line}"
        );
        let pending = field(line, crate::settings_projection::PENDING_KEY);
        assert_ne!(
            pending,
            Some(crate::settings_model::PendingCause::Restart.as_str()),
            "the node is closing this gap by itself, so the answer must not send the operator to \
             restart it — a restart is what a person does when nothing else will work, and here \
             a preparation is already running; got: {line}"
        );
        assert_ne!(
            pending,
            Some(crate::settings_projection::NONE),
            "and it must not report nothing outstanding while a preparation is under way: the \
             backend the operator asked for is not running yet; got: {line}"
        );

        // The preparation has now reported what it is doing, on the real path.
        build_started
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the preparation must reach its build");
        let progress = detection_transitions()
            .state()
            .pending
            .and_then(|pending| pending.latest_progress)
            .expect("the preparation reported progress");
        let watching = crate::settings_command::answer(
            deployment.path(),
            &crate::settings_store::SettingsStore::store_path(deployment.path()),
            "",
        );
        assert!(
            watching.contains(&progress),
            "a read taken while the preparation runs must carry the last thing the preparation \
             said about itself ({progress:?}); the coordinator holds it and the operator surface \
             never asks for it. Got:\n{watching}"
        );

        // And the restart cause is not destroyed for the settings that
        // genuinely need one: a value only read as the process starts still
        // tells the operator to restart, because nothing here closes that gap.
        let restarting = setting_line(
            &watching,
            crate::settings_model::DETECTOR_MODEL_PATH_SETTING,
        );
        assert_eq!(
            field(restarting, crate::settings_projection::PENDING_KEY),
            Some(crate::settings_model::PendingCause::Restart.as_str()),
            "a setting this node takes on only at startup still names a restart; got: {restarting}"
        );

        let _ = release.send(());
        for _ in 0..1_000_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
            {
                break;
            }
            std::thread::yield_now();
        }

        println!("child_assertions_passed");
    }

    // ── Contract 10: the background promotion is bound by the request it
    // belongs to, not by whichever request is standing when it finishes ─────

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    fn a_startup_preparation_that_finishes_late_installs_nothing_after_a_newer_command() {
        assert_isolated_child_passed("late_startup_promotion_installs_nothing_child");
    }

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    #[ignore = "re-executed in isolation by \
                a_startup_preparation_that_finishes_late_installs_nothing_after_a_newer_command; \
                mutates the process-global detector registry, coordinator and running-value \
                registry"]
    fn late_startup_promotion_installs_nothing_child() {
        // The node boots with the automation on and begins a cold accelerator
        // preparation. While it runs, the operator names the processor: that
        // command takes its own request, installs the processor detector and
        // publishes it, and every surface says processor. Then the boot
        // preparation finally passes.
        //
        // It must install nothing. The operator has already moved away from
        // that backend, and unlike the command path there is no later pass to
        // correct it — a detector put on the handle here runs the cameras
        // until something else happens to move them.
        //
        // The version this preparation belongs to is taken when it STARTS, not
        // when it finishes; a promotion that reads the current version at
        // install time is asking whether anyone is standing there, not whether
        // it is still the one being waited for.
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-late-promotion"),
        );
        let builds = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(AtomicUsize::new(0));
        let handle = register_camera(
            "front",
            "yolox-test",
            true,
            true,
            Arc::clone(&builds),
            Arc::clone(&receipts),
        );
        // What the boot preparation belongs to.
        let boot_version = detection_transitions().request_version();

        // The operator names the processor, and the machine takes it on.
        request_detection_backend(false);
        for _ in 0..1_000_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
            {
                break;
            }
            std::thread::yield_now();
        }
        let running_after_command = in_force(DETECTION_BACKEND_SETTING);
        let detector_after_command = handle.snapshot().model_sha256().to_string();
        assert!(
            detection_transitions().request_version() > boot_version,
            "the operator's command must be a newer request than the one the boot preparation \
             belongs to"
        );

        // The boot preparation finally passes and offers its instance.
        let outcome = install_prepared_detector(
            "front",
            crate::detection_accel::ACCELERATED_DETECTION_BACKEND,
            boot_version,
            &handle,
            Box::new(StubDetector {
                sha: "front-late-accelerated".to_string(),
            }),
        );

        assert!(
            outcome.is_err(),
            "a preparation belonging to a superseded request must be refused, not installed"
        );
        assert_eq!(
            handle.snapshot().model_sha256(),
            detector_after_command,
            "the cameras must still run the detector the operator's command put there"
        );
        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            running_after_command,
            "and no surface may be moved onto the backend the operator left"
        );
        assert_ne!(
            detection_transitions().state().running_backend,
            crate::detection_accel::ACCELERATED_DETECTION_BACKEND,
            "the node must not claim the backend nothing is running"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 11: the node's running claim moves when the detectors move,
    // not when a selection decides ──────────────────────────────────────────
    //
    // The running value IS what every operator surface answers with. It is
    // published by the selection, one call before the version guard and before
    // any handle has moved, so between that publish and the last camera's swap
    // the node is naming a backend some of its cameras are not running.

    #[test]
    fn the_node_claims_no_backend_while_a_camera_still_runs_the_old_one() {
        assert_isolated_child_passed("running_claim_waits_for_every_camera_child");
    }

    #[test]
    #[ignore = "re-executed in isolation by \
                the_node_claims_no_backend_while_a_camera_still_runs_the_old_one; mutates the \
                process-global detector registry, coordinator and running-value registry"]
    fn running_claim_waits_for_every_camera_child() {
        // Two cameras, so the move is not one act: the first camera swaps and
        // the second is still doing its cold build. Nothing here is a race —
        // the second build is held open by this test.
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-multi-camera"),
        );
        let front_builds = Arc::new(AtomicUsize::new(0));
        let front_receipts = Arc::new(AtomicUsize::new(0));
        let _front = register_camera(
            "front",
            "yolox-test",
            false,
            true,
            Arc::clone(&front_builds),
            Arc::clone(&front_receipts),
        );
        let back_builds = Arc::new(AtomicUsize::new(0));
        let (started, build_started) = mpsc::channel();
        let (release, wait_for_release) = mpsc::channel();
        let back = register_camera_with_gated_build(
            "back",
            started,
            wait_for_release,
            Arc::clone(&back_builds),
        );

        let moving = std::thread::spawn(|| request_detection_backend(true));
        build_started
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the second camera must reach its build");

        // The second camera is demonstrably still on the backend it booted on:
        // its replacement has not been built yet, let alone installed.
        assert_eq!(
            back.snapshot().model_sha256(),
            "back-initial",
            "the held camera has not swapped, which is what this contract is about"
        );
        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            Some(SettingValue::text("seed-multi-camera")),
            "the node must not name a backend while a camera still feeds frames through \
             another: the running value is what every operator surface answers with, and \
             claiming the new backend here describes a machine nobody has"
        );
        assert_ne!(
            detection_transitions().state().running_backend,
            crate::detection_accel::CPU_DETECTION_BACKEND,
            "and the coordinator's own claim must agree with the surface's"
        );

        let _ = release.send(());
        moving.join().expect("the request thread");
        for _ in 0..1_000_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
            {
                break;
            }
            std::thread::yield_now();
        }
        // Once every camera has moved, the node names the new backend — the
        // claim is late, never absent.
        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            Some(SettingValue::text(
                crate::detection_accel::CPU_DETECTION_BACKEND
            )),
            "with every camera on the new backend the node says so"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 12: a superseded pass leaves the running claim on the
    // backend still in use ──────────────────────────────────────────────────

    #[test]
    fn a_superseded_pass_leaves_the_running_claim_on_the_backend_still_in_use() {
        assert_isolated_child_passed("superseded_pass_leaves_running_claim_child");
    }

    #[test]
    #[ignore = "re-executed in isolation by \
                a_superseded_pass_leaves_the_running_claim_on_the_backend_still_in_use; mutates \
                the process-global detector registry, coordinator and running-value registry"]
    fn superseded_pass_leaves_running_claim_child() {
        // A pass publishes its selection, is then superseded, and installs
        // nothing. Nothing moved, so the backend the camera is running is the
        // one it booted on — and that is what the node must still be naming.
        // A claim left behind by a pass that did nothing is not a transient:
        // the next pass reads the running value as ground truth, so a
        // nothing-moved outcome restores the lie rather than the fact.
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-superseded-claim"),
        );
        let builds = Arc::new(AtomicUsize::new(0));
        let (started, build_started) = mpsc::channel();
        let (release, wait_for_release) = mpsc::channel();
        let handle = Arc::new(PromotableDetector::new(Box::new(StubDetector {
            sha: "front-initial".to_string(),
        })));
        let release_gate = Mutex::new(Some(wait_for_release));
        let build_calls = Arc::clone(&builds);
        register_live_detector(
            &handle,
            DetectorRebuild {
                camera: "front".to_string(),
                model_id: "yolox-test".to_string(),
                accelerated_detection: false,
                build: Arc::new(move |_backend: &str| {
                    let call = build_calls.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                    if call == 1 {
                        let _ = started.send(());
                        if let Some(gate) = release_gate.lock().expect("release gate").take() {
                            let _ = gate.recv();
                        }
                        return Ok(Box::new(StubDetector {
                            sha: "front-build-1".to_string(),
                        }) as Box<dyn Detector>);
                    }
                    // Every later attempt refuses, so no pass in this scenario
                    // ever moves the camera: the running claim has only one
                    // truthful value throughout.
                    Err("front build refused".to_string())
                }),
                receipt: Arc::new(move |_receipt: &AccelerationReceipt| {}),
            },
        );

        let first = std::thread::spawn(|| request_detection_backend(true));
        build_started
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the first preparation must reach its build");
        // The change of mind, through the entry point a settings command uses.
        request_detection_backend(false);
        let _ = release.send(());
        first.join().expect("the first request thread");
        for _ in 0..1_000_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
            {
                break;
            }
            std::thread::yield_now();
        }

        assert_eq!(
            handle.snapshot().model_sha256(),
            "front-initial",
            "no pass in this scenario installed anything, which is the premise of the claim \
             assertion below"
        );
        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            Some(SettingValue::text("seed-superseded-claim")),
            "nothing moved, so the node must still name the backend its camera is genuinely \
             running; a claim published by a pass that installed nothing is a backend no frame \
             goes through"
        );
        assert_eq!(
            detection_transitions().state().running_backend,
            "seed-superseded-claim",
            "and the coordinator and the operator surface must name the same backend"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 13: a move asked for before any handle slot exists is not
    // lost ──────────────────────────────────────────────────────────────────
    //
    // Startup asks for the accelerator before the camera's promotable handle
    // has been created, and the removal of the startup deadline means the
    // preparation can now finish inside that window. The comment in the
    // runtime still asserts the window cannot be entered; it can. A move that
    // lands in it must claim nothing — and must not be silently dropped, or
    // the camera stays on the processor until somebody restarts the node.

    #[test]
    fn a_move_asked_for_before_any_handle_exists_claims_nothing_and_is_not_lost() {
        assert_isolated_child_passed("move_before_handle_slot_is_not_lost_child");
    }

    #[test]
    #[ignore = "re-executed in isolation by \
                a_move_asked_for_before_any_handle_exists_claims_nothing_and_is_not_lost; \
                mutates the process-global detector registry, coordinator and running-value \
                registry"]
    fn move_before_handle_slot_is_not_lost_child() {
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-before-slot"),
        );
        // The move arrives first, exactly as a startup promotion that finishes
        // before the camera thread has published its handle does.
        request_detection_backend(true);
        for _ in 0..1_000_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
            {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            Some(SettingValue::text("seed-before-slot")),
            "no handle took anything on, so the node claims nothing"
        );

        // The camera thread now publishes its handle.
        let builds = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(AtomicUsize::new(0));
        let (built, build_done) = mpsc::channel();
        let handle = Arc::new(PromotableDetector::new(Box::new(StubDetector {
            sha: "front-initial".to_string(),
        })));
        let build_calls = Arc::clone(&builds);
        register_live_detector(
            &handle,
            DetectorRebuild {
                camera: "front".to_string(),
                model_id: "yolox-test".to_string(),
                accelerated_detection: false,
                build: Arc::new(move |backend: &str| {
                    build_calls.fetch_add(1, AtomicOrdering::SeqCst);
                    let built = built.clone();
                    let detector = Box::new(StubDetector {
                        sha: format!("front-{backend}"),
                    }) as Box<dyn Detector>;
                    let _ = built.send(());
                    Ok(detector)
                }),
                receipt: Arc::new(move |_receipt: &AccelerationReceipt| {
                    receipts.fetch_add(1, AtomicOrdering::SeqCst);
                }),
            },
        );

        build_done
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect(
                "the move asked for before the handle existed must reach the camera once its \
                 handle is registered; a move dropped in that window leaves the camera on the \
                 processor with nothing to retry it but a restart",
            );
        for _ in 0..1_000_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
            {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(
            handle.snapshot().model_sha256(),
            format!("front-{}", crate::detection_accel::CPU_DETECTION_BACKEND),
            "the camera ends up running the backend the move asked for, without a restart"
        );
        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            Some(SettingValue::text(
                crate::detection_accel::CPU_DETECTION_BACKEND
            )),
            "and the node names it only then"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 14: automatic startup takes the same transition path a
    // command takes ─────────────────────────────────────────────────────────
    //
    // One non-blocking transition path is shared by automatic startup and
    // manual choices. Startup instead runs its own probe, throws away the
    // detector that probe proved, and hands its caller the job of cold-loading
    // a second one — so the accelerated compile is paid twice per camera and
    // what ends up running the cameras is an instance nothing forward-tested.
    // Both facts are read here off the coordinator and off the registered
    // build seam, which are production, not this test's doubles: the startup
    // entry point must move the registered handle itself, through the one
    // preparation machinery, exactly as `request_detection_backend` does.

    /// A startup preparation that finishes when this test says so and not
    /// before, so every state below is reached by an event.
    #[cfg(feature = "detect-burn-wgpu")]
    struct DrivenStartupPreparation {
        finish: mpsc::Receiver<crate::detection_accel::DetectionForwardProbeOutcome>,
    }

    #[cfg(feature = "detect-burn-wgpu")]
    impl crate::detection_accel::DetectionForwardProbe for DrivenStartupPreparation {
        fn run_forward_probe(&mut self) -> crate::detection_accel::DetectionForwardProbeOutcome {
            self.finish
                .recv()
                .unwrap_or(crate::detection_accel::DetectionForwardProbeOutcome::NoDeviceVisible)
        }
    }

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    fn automatic_startup_moves_the_registered_handle_through_the_one_preparation_path() {
        assert_isolated_child_passed("startup_uses_the_shared_transition_path_child");
    }

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    #[ignore = "re-executed in isolation by \
                automatic_startup_moves_the_registered_handle_through_the_one_preparation_path; \
                mutates the process-global detector registry, coordinator and running-value \
                registry"]
    fn startup_uses_the_shared_transition_path_child() {
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-startup"),
        );
        let builds = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(AtomicUsize::new(0));
        let handle = register_camera(
            "front",
            "yolox-test",
            true,
            true,
            Arc::clone(&builds),
            Arc::clone(&receipts),
        );

        let (finish, wait_for_finish) = mpsc::channel();
        let (ended, preparation_ended) = mpsc::channel();
        // The promotion closure is a witness only: it loads nothing and
        // installs nothing. A startup path that needs its caller to cold-load
        // and install a second detector is the duplicated load this contract
        // refuses — the instance the preparation forward-tested is the one the
        // cameras must end up running.
        let selection = crate::detection_accel::spawn_detection_probe_with_promotion(
            true,
            "yolox-test",
            crate::yolox_detector::MODEL_INPUT_SHAPE,
            DrivenStartupPreparation {
                finish: wait_for_finish,
            },
            Arc::new(crate::acceleration::AccelerationState::new()),
            || Ok(()),
            move |_receipt: &AccelerationReceipt| {
                let _ = ended.send(());
            },
        );
        assert_eq!(
            selection.backend,
            crate::detection_accel::CPU_DETECTION_BACKEND,
            "startup answers on the processor rather than waiting for an accelerator"
        );

        finish
            .send(
                crate::detection_accel::DetectionForwardProbeOutcome::Passed {
                    selected_device: "test-accelerator".to_string(),
                    evidence_fields: std::collections::BTreeMap::new(),
                },
            )
            .expect("release the startup preparation");
        preparation_ended
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the startup preparation must end on its own outcome");
        for _ in 0..1_000_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
            {
                break;
            }
            std::thread::yield_now();
        }

        assert_eq!(
            detection_transitions().physical_preparation_count(),
            1,
            "the startup move must be one physical preparation the coordinator carried, not a \
             probe outside it: the machinery that stops two builds of one identity piling up \
             cannot stop what never reaches it"
        );
        assert_eq!(
            builds.load(AtomicOrdering::SeqCst),
            1,
            "and it must build through the seam the camera registered — the same seam a command \
             uses — so the accelerated cold compile is paid once, not once for the probe and \
             again for the promotion"
        );
        // WHICH instance ends up on the handle is pinned by
        // `the_startup_preparation_installs_the_instance_it_forward_tested`,
        // whose probe stamps a per-instance identity and carries the object it
        // proved. This test cannot see that: its probe builds nothing, so the
        // strongest honest reading here is that the handle genuinely left the
        // detector it started on for one the shared preparation built.
        assert_eq!(
            handle.snapshot().model_sha256(),
            format!(
                "front-{}",
                crate::detection_accel::ACCELERATED_DETECTION_BACKEND
            ),
            "the camera must end up on a detector the shared preparation built for the \
             accelerated backend rather than still running the one it started on"
        );
        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            Some(SettingValue::text(
                crate::detection_accel::ACCELERATED_DETECTION_BACKEND
            )),
            "and the value this node reports running must be that backend: a startup move the \
             coordinator carried publishes its result the same way a command's does"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 15: the instance the startup preparation forward-tested is
    // the instance the camera runs ──────────────────────────────────────────
    //
    // The promise is one physical preparation per detector identity, and the
    // exact forward-probed instance installed — no duplicated cold load. The
    // previous contract reads the handle's backend NAME, which cannot tell an
    // installed probed instance from a second instance built for the same
    // backend, and it drives a probe that builds nothing at all. This one
    // stamps a per-instance identity on every object the registered seam
    // builds, runs the forward pass through that object, and asks the handle
    // which object it ended up running.

    /// A detector that answers a forward pass, so a preparation can prove the
    /// very object it is about to hand over instead of a canned result
    /// standing in for one.
    #[cfg(feature = "detect-burn-wgpu")]
    struct ForwardTestableDetector {
        sha: String,
    }

    #[cfg(feature = "detect-burn-wgpu")]
    impl Detector for ForwardTestableDetector {
        fn model_sha256(&self) -> &str {
            &self.sha
        }

        fn detect_segment(
            &self,
            _media: &crate::media_pipeline::DecodedVideoSegment,
            clip_sha256: String,
            _sample_frames: usize,
            _confidence_threshold: f64,
        ) -> Result<crate::yolox_detector::DetectorOutput, String> {
            Ok(crate::yolox_detector::DetectorOutput {
                detector_backend: "test".to_string(),
                detector_session_id: format!("session-{}", self.sha),
                model_sha256: self.sha.clone(),
                clip_sha256,
                model_forward_sha256: format!("forward-{}", self.sha),
                detector_nms_sha256: format!("nms-{}", self.sha),
                result_sha256: format!("result-{}", self.sha),
                detections: Vec::new(),
                forward_event_nonce: None,
                forward_event_seq: None,
            })
        }
    }

    /// The clip a probe's forward pass runs over. Empty, because what is under
    /// test is which INSTANCE the pass ran through, not what it found.
    #[cfg(feature = "detect-burn-wgpu")]
    fn probe_segment() -> crate::media_pipeline::DecodedVideoSegment {
        crate::media_pipeline::DecodedVideoSegment {
            frames: Vec::new(),
            encoded_units: Vec::new(),
            fps: 1.0,
            observed_at: None,
            codec: crate::media_pipeline::VideoCodec::H264,
        }
    }

    /// Registers a camera whose every build stamps a distinct instance
    /// identity, and hands back the SAME build seam the coordinator will use —
    /// so a probe building through it is building through the camera's own
    /// seam, and one counter counts every physical build of this identity.
    #[cfg(feature = "detect-burn-wgpu")]
    fn register_camera_with_instance_stamped_build(
        camera: &str,
        builds: Arc<AtomicUsize>,
    ) -> (Arc<PromotableDetector>, Arc<DetectorBuild>) {
        let handle = Arc::new(PromotableDetector::new(Box::new(StubDetector {
            sha: format!("{camera}-initial"),
        })));
        let build_camera = camera.to_string();
        let build: Arc<DetectorBuild> = Arc::new(move |backend: &str| {
            let instance = builds.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            Ok(Box::new(ForwardTestableDetector {
                sha: format!("{build_camera}-{backend}-instance-{instance}"),
            }) as Box<dyn Detector>)
        });
        register_live_detector(
            &handle,
            DetectorRebuild {
                camera: camera.to_string(),
                model_id: "yolox-test".to_string(),
                accelerated_detection: true,
                build: Arc::clone(&build),
                receipt: Arc::new(move |_receipt: &AccelerationReceipt| {}),
            },
        );
        (handle, build)
    }

    /// A startup preparation shaped like the production one: it builds the
    /// detector through the seam the camera registered, runs a forward pass
    /// through that object, and hands the proved instance out with its
    /// outcome.
    #[cfg(feature = "detect-burn-wgpu")]
    struct SeamBuildingStartupPreparation {
        build: Arc<DetectorBuild>,
        probed: mpsc::Sender<String>,
        tested: Option<Box<dyn Detector>>,
    }

    #[cfg(feature = "detect-burn-wgpu")]
    impl crate::detection_accel::DetectionForwardProbe for SeamBuildingStartupPreparation {
        fn run_forward_probe(&mut self) -> crate::detection_accel::DetectionForwardProbeOutcome {
            let detector = (self.build)(crate::detection_accel::ACCELERATED_DETECTION_BACKEND)
                .expect("the camera's registered seam builds the accelerated detector");
            let proved = detector
                .detect_segment(&probe_segment(), "probe-clip".to_string(), 1, 0.25)
                .expect("the forward pass runs through the instance this preparation built");
            assert_eq!(
                proved.model_sha256,
                detector.model_sha256(),
                "the forward pass must have run through THIS instance"
            );
            let _ = self.probed.send(detector.model_sha256().to_string());
            self.tested = Some(detector);
            crate::detection_accel::DetectionForwardProbeOutcome::Passed {
                selected_device: "test-accelerator".to_string(),
                evidence_fields: BTreeMap::new(),
            }
        }

        fn take_forward_tested(&mut self) -> Option<Box<dyn std::any::Any + Send>> {
            self.tested
                .take()
                .map(crate::detection_accel::forward_tested_payload)
        }
    }

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    fn the_startup_preparation_installs_the_instance_it_forward_tested() {
        assert_isolated_child_passed("startup_installs_the_instance_it_forward_tested_child");
    }

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    #[ignore = "re-executed in isolation by \
                the_startup_preparation_installs_the_instance_it_forward_tested; mutates the \
                process-global detector registry, coordinator and running-value registry"]
    fn startup_installs_the_instance_it_forward_tested_child() {
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-probed-instance"),
        );
        let builds = Arc::new(AtomicUsize::new(0));
        let (handle, build) =
            register_camera_with_instance_stamped_build("front", Arc::clone(&builds));

        let (probed, probed_identity) = mpsc::channel();
        let (ended, preparation_ended) = mpsc::channel();
        let selection = crate::detection_accel::spawn_detection_probe_with_promotion(
            true,
            "yolox-test",
            crate::yolox_detector::MODEL_INPUT_SHAPE,
            SeamBuildingStartupPreparation {
                build,
                probed,
                tested: None,
            },
            Arc::new(crate::acceleration::AccelerationState::new()),
            || Ok(()),
            move |_receipt: &AccelerationReceipt| {
                let _ = ended.send(());
            },
        );
        assert_eq!(
            selection.backend,
            crate::detection_accel::CPU_DETECTION_BACKEND,
            "startup answers on the processor rather than waiting for an accelerator"
        );

        let probed_instance = probed_identity
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the startup preparation forward-tests the instance it built");
        let _ = preparation_ended.recv_timeout(std::time::Duration::from_secs(60));
        for _ in 0..1_000_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
            {
                break;
            }
            std::thread::yield_now();
        }

        assert_eq!(
            handle.snapshot().model_sha256(),
            probed_instance,
            "the camera must end up running the very instance the preparation forward-tested; a \
             second instance built for the same backend carries the same NAME and no proof — \
             nothing ran a frame through it before it was put in front of the cameras"
        );
        assert_eq!(
            builds.load(AtomicOrdering::SeqCst),
            1,
            "and the accelerated cold compile is paid once for the whole startup move: one \
             physical build of this identity, not one for the proof and another for the install"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 16: a command issued while the startup probe runs is carried
    // when that probe answers ───────────────────────────────────────────────
    //
    // A newer request can never be overturned by an older completion, and a
    // preparation ends only by completing or by a real error. An operator who
    // names a backend during the startup probe — the longest window in the
    // system, the accelerated cold compile — records a request the startup
    // pass then refuses to install, correctly. What must not happen is that
    // their request is left standing with no worker: nothing polls for it, and
    // the operator would have to type the command again to escape.

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    fn a_command_issued_during_the_startup_probe_is_carried_when_the_probe_answers() {
        assert_isolated_child_passed("command_during_the_startup_probe_is_carried_child");
    }

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    #[ignore = "re-executed in isolation by \
                a_command_issued_during_the_startup_probe_is_carried_when_the_probe_answers; \
                mutates the process-global detector registry, coordinator and running-value \
                registry"]
    fn command_during_the_startup_probe_is_carried_child() {
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-command-during-probe"),
        );
        let builds = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(AtomicUsize::new(0));
        let handle = register_camera(
            "front",
            "yolox-test",
            true,
            true,
            Arc::clone(&builds),
            Arc::clone(&receipts),
        );

        let (finish, wait_for_finish) = mpsc::channel();
        let (ended, preparation_ended) = mpsc::channel();
        let _selection = crate::detection_accel::spawn_detection_probe_with_promotion(
            true,
            "yolox-test",
            crate::yolox_detector::MODEL_INPUT_SHAPE,
            DrivenStartupPreparation {
                finish: wait_for_finish,
            },
            Arc::new(crate::acceleration::AccelerationState::new()),
            || Ok(()),
            move |_receipt: &AccelerationReceipt| {
                let _ = ended.send(());
            },
        );

        // The operator names the processor while the probe is demonstrably
        // still running: nothing here is timing, the probe answers only when
        // this test releases it below.
        request_detection_backend(false);
        let operator_version = detection_transitions().request_version();

        finish
            .send(
                crate::detection_accel::DetectionForwardProbeOutcome::Passed {
                    selected_device: "test-accelerator".to_string(),
                    evidence_fields: BTreeMap::new(),
                },
            )
            .expect("release the startup preparation");
        let _ = preparation_ended.recv_timeout(std::time::Duration::from_secs(60));
        // Bounded, so a request nobody is carrying fails here rather than
        // hanging: the whole point is that no worker exists for it.
        for _ in 0..2_000_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
                && detection_transitions().state().pending.is_none()
                && handle.snapshot().model_sha256()
                    == format!("front-{}", crate::detection_accel::CPU_DETECTION_BACKEND)
            {
                break;
            }
            std::thread::yield_now();
        }

        assert!(
            detection_transitions().request_version() >= operator_version,
            "the operator's request is the one standing at the end"
        );
        assert_eq!(
            handle.snapshot().model_sha256(),
            format!("front-{}", crate::detection_accel::CPU_DETECTION_BACKEND),
            "the camera must end up running the backend the OPERATOR named; a request recorded \
             while the startup probe ran and then left with no worker keeps the node on the \
             backend they moved away from until they type the command again"
        );
        let state = detection_transitions().state();
        assert!(
            state.pending.is_none(),
            "and nothing may be left outstanding for a request nothing is carrying"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 17: a landing move is never read as restart-pending ────────
    //
    // Promotion is one indivisible runtime update — choice, running backend,
    // receipts, reason, pending — with no false restart-pending on an
    // already-live backend. The defect this was authored against cleared the
    // coordinator's pending in one act and published the value in force in
    // another, so a settings read landing between them saw a
    // requested/running gap with no preparation outstanding and answered
    // RESTART: it told the operator to power-cycle a node that was already
    // running exactly what they asked for. The swap, the coordinator's act and
    // the publication are now one hold (`take_detector_on`), and this contract
    // is what holds them there.
    //
    // What this test arranges deterministically is the swap's ARRIVAL: the
    // registry a swap writes its prepared detector into is held here, so the
    // preparation stops at it. That stop is AFTER the publication, not inside
    // it — the registry write is deliberately outside the hold — so the reader
    // crossing the published seam is the concurrent one below, and its
    // sampling of that seam is probabilistic rather than arranged.

    #[test]
    fn a_landing_move_is_never_read_as_restart_pending() {
        assert_isolated_child_passed("landing_move_is_never_read_as_restart_pending_child");
    }

    #[test]
    #[ignore = "re-executed in isolation by a_landing_move_is_never_read_as_restart_pending; \
                mutates the process-global detector registry, coordinator and running-value \
                registry"]
    fn landing_move_is_never_read_as_restart_pending_child() {
        crate::settings_application::mark_running_process();
        let deployment = tempfile::tempdir().expect("a temporary deployment directory");
        {
            let store = crate::settings_store::SettingsStore::open(deployment.path())
                .expect("a store for the deployment under test");
            store
                .set_local(
                    crate::settings_domains::ACCELERATED_DETECTION_DOMAIN,
                    crate::settings_model::Surface::VigilSettings,
                    crate::settings_model::Scope::node(crate::node_key::scope_name(
                        deployment.path(),
                    )),
                    SettingValue::Bool(false),
                )
                .expect("turn the automation off");
        }
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text("seed-landing"),
        );
        let builds = Arc::new(AtomicUsize::new(0));
        let (started, build_started) = mpsc::channel();
        let (release, wait_for_release) = mpsc::channel();
        let _handle = register_camera_with_gated_build(
            "front",
            started,
            wait_for_release,
            Arc::clone(&builds),
        );

        let answer = crate::settings_command::answer(
            deployment.path(),
            &crate::settings_store::SettingsStore::store_path(deployment.path()),
            &format!(
                "set {DETECTION_BACKEND_SETTING} {}",
                crate::detection_accel::CPU_DETECTION_BACKEND
            ),
        );
        assert!(
            !crate::settings_command::answer_failed(&answer),
            "naming a backend this artifact carries is an ordinary change; got:\n{answer}"
        );
        build_started
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the preparation must reach its build");

        let sampling = Arc::new(AtomicBool::new(true));
        let samples = Arc::new(AtomicUsize::new(0));
        let readings: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let reader = std::thread::spawn({
            let sampling = Arc::clone(&sampling);
            let samples = Arc::clone(&samples);
            let readings = Arc::clone(&readings);
            let deployment = deployment.path().to_path_buf();
            move || {
                while sampling.load(Ordering::SeqCst) {
                    let answer = crate::settings_command::answer(
                        &deployment,
                        &crate::settings_store::SettingsStore::store_path(&deployment),
                        "",
                    );
                    let line = setting_line(&answer, DETECTION_BACKEND_SETTING);
                    let running =
                        field(line, crate::settings_projection::RUNNING_KEY).unwrap_or_default();
                    let pending =
                        field(line, crate::settings_projection::PENDING_KEY).unwrap_or_default();
                    let requested = crate::detection_accel::CPU_DETECTION_BACKEND;
                    let complete_new =
                        running == requested && pending == crate::settings_projection::NONE;
                    let complete_old = running != requested
                        && pending == crate::settings_model::PendingCause::LiveTransition.as_str();
                    if !complete_new
                        && !complete_old
                        && let Ok(mut readings) = readings.lock()
                    {
                        readings.push(format!("running={running} pending={pending}"));
                    }
                    samples.fetch_add(1, AtomicOrdering::SeqCst);
                }
            }
        });

        // The prepared-detector registry a swap writes into, held here: the
        // preparation reaches the swap and stops there. That write is outside
        // the publication hold and after it, so this stops the preparation
        // AFTER the coordinator's act and the value in force have been
        // published together — it makes the landing arrive on this test's own
        // signal, and the reader above is what crosses the published seam.
        let held = prepared_detectors()
            .lock()
            .expect("hold the prepared-detector registry across the swap");
        let _ = release.send(());
        for _ in 0..2_000_000 {
            if detection_transitions().state().pending.is_none() {
                break;
            }
            std::thread::yield_now();
        }
        let before = samples.load(AtomicOrdering::SeqCst);
        for _ in 0..2_000_000 {
            if samples.load(AtomicOrdering::SeqCst) >= before + 3 {
                break;
            }
            std::thread::yield_now();
        }
        drop(held);

        for _ in 0..2_000_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
            {
                break;
            }
            std::thread::yield_now();
        }
        sampling.store(false, Ordering::SeqCst);
        reader.join().expect("the reader thread");

        let readings = readings.lock().expect("the reader's samples").clone();
        assert!(
            readings.is_empty(),
            "every read taken while the move landed must be either the complete old state (the \
             backend still running, with the live transition named) or the complete new state \
             (the backend asked for running, nothing outstanding). A read that lands between the \
             two tells the operator to restart a node that is already running what they asked \
             for. Samples that were neither: {readings:?}"
        );

        println!("child_assertions_passed");
    }

    // ── The same contract, in the configuration a default node is actually
    // in ─────────────────────────────────────────────────────────────────────
    //
    // The sibling above turns automatic accelerated-detection management OFF
    // and authors a backend, because that is what makes the write legal. A
    // default node is in the opposite state: the domain is ON, so a write to
    // detection_backend is REFUSED and nobody has authored one, which leaves
    // the author Automatic — and that is the only state the automatic startup
    // promotion ever moves in.
    //
    // There the choice the surface reports is not a stored value sitting still.
    // It is READ from the same running-value registry the landing move
    // publishes into, so the surface has THREE moving facts to read, not two.
    // A read that takes the choice before the move publishes and the running
    // value after it sees them disagree with no preparation outstanding, and
    // tells the operator to restart a node already running exactly what it
    // asked for.
    //
    // Nothing here is a double. The move is the production startup promotion,
    // and every sample is a production read: the deployment's own resolution of
    // the setting, the rendered `vigil settings` line, and the automatic floor
    // a never-started deployment answers from.
    //
    // The interleaving is arranged rather than waited for. Left alone it is the
    // width of one publication — microseconds against reads that take
    // milliseconds — so this holds the publication seam itself, through the
    // same function both sides take it through, to gather readers on it: held
    // while they queue, released just long enough for them to get past the
    // domain switch's own publication, then held again with the landing behind
    // it. When it is let go the landing publishes first and the readers behind
    // it are answered after — which is the interleaving the contract forbids,
    // and on a correct node is simply a complete new state.

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    fn an_automatic_landing_move_is_never_read_as_restart_pending() {
        assert_isolated_child_passed(
            "automatic_landing_move_is_never_read_as_restart_pending_child",
        );
    }

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    #[ignore = "re-executed in isolation by \
                an_automatic_landing_move_is_never_read_as_restart_pending; mutates the \
                process-global detector registry, coordinator and running-value registry"]
    fn automatic_landing_move_is_never_read_as_restart_pending_child() {
        /// How many landings the readers are held across.
        const ATTEMPTS: usize = 20;
        /// How many readers cycle over the setting. Every one of them that is
        /// waiting on the seam when the landing publishes is a reader the
        /// contract is about, and more of them makes the outcome depend less
        /// on which thread the operating system wakes first.
        const READERS: usize = 8;
        /// Long enough for the readers to gather on a held seam, and for the
        /// landing to be queued on it behind them. Nothing a test can read
        /// says "this thread is now waiting on that lock", so these are waits
        /// rather than facts; slack only ever costs a missed interleaving,
        /// never a false one, and a spin instead of a sleep here starves the
        /// very readers it is waiting for.
        const GATHER: std::time::Duration = std::time::Duration::from_millis(40);
        /// Long enough for the readers let off the seam to come round again,
        /// which is what leaves some of them past the domain switch's own
        /// publication and short of this setting's.
        const COME_ROUND: std::time::Duration = std::time::Duration::from_millis(20);

        crate::settings_application::mark_running_process();
        let deployment = tempfile::tempdir().expect("a temporary deployment directory");
        // Keep one owner alive until every operator-surface reader below has
        // finished. The production runtime owns its store for the whole time
        // it serves a command; if this fixture drops its only enduring handle,
        // the short-lived reader threads can all exit while the surface is in
        // its final open and turn this consistency proof into an owner-shutdown
        // race instead.
        let _store_owner = crate::settings_store::SettingsStore::open(deployment.path())
            .expect("a store for the deployment under test");

        // The configuration under test, established rather than assumed: with
        // the domain left at its default the operator cannot author a backend
        // at all, so the author is Automatic and the reported choice follows
        // the running value.
        let refused = crate::settings_command::answer(
            deployment.path(),
            &crate::settings_store::SettingsStore::store_path(deployment.path()),
            &format!(
                "set {DETECTION_BACKEND_SETTING} {}",
                crate::detection_accel::CPU_DETECTION_BACKEND
            ),
        );
        assert!(
            crate::settings_command::answer_failed(&refused),
            "this contract is about the state a default node runs in: automatic management on, \
             so no operator value can exist. A deployment that accepts this write is not in that \
             state; got:\n{refused}"
        );

        // Seeded first: a node that has taken no backend on has nothing running
        // to be read against, and the contract is about a move BETWEEN two live
        // values.
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text(crate::detection_accel::CPU_DETECTION_BACKEND),
        );
        let builds = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(AtomicUsize::new(0));
        let _handle = register_camera(
            "front",
            "yolox-test",
            true,
            true,
            Arc::clone(&builds),
            Arc::clone(&receipts),
        );

        let readings: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let samples = Arc::new(AtomicUsize::new(0));
        let saw_the_new_backend = Arc::new(AtomicBool::new(false));
        // Where nobody authored a value the choice IS the running backend, so
        // a complete state — the old one or the new one — reads as the two
        // agreeing with nothing outstanding. Anything else is the seam.
        let record_sample = {
            let readings = Arc::clone(&readings);
            let samples = Arc::clone(&samples);
            let saw_the_new_backend = Arc::clone(&saw_the_new_backend);
            move |surface: &str, requested: &str, running: &str, pending: &str| {
                samples.fetch_add(1, AtomicOrdering::SeqCst);
                if running == crate::detection_accel::ACCELERATED_DETECTION_BACKEND {
                    saw_the_new_backend.store(true, Ordering::SeqCst);
                }
                let complete = requested == running && pending == crate::settings_projection::NONE;
                if !complete
                    && let Ok(mut readings) = readings.lock()
                    && readings.len() < 20
                {
                    readings.push(format!(
                        "{surface}: requested={requested} running={running} pending={pending}"
                    ));
                }
            }
        };
        let target_of = |directory: &std::path::Path| crate::settings_model::ScopeTarget {
            tenant: crate::node_key::scope_name(directory),
            site: crate::node_key::scope_name(directory),
            node: crate::node_key::scope_name(directory),
            camera: None,
        };
        let sampling = Arc::new(AtomicBool::new(true));

        // The readers: each takes the deployment's own read of this one
        // setting, over and over, for as long as the landings run.
        let mut reading = Vec::new();
        for _ in 0..READERS {
            reading.push(std::thread::spawn({
                let sampling = Arc::clone(&sampling);
                let record_sample = record_sample.clone();
                let directory = deployment.path().to_path_buf();
                move || {
                    let store = crate::settings_store::SettingsStore::open(&directory)
                        .expect("a store to read the setting through");
                    let target = crate::settings_model::ScopeTarget {
                        tenant: crate::node_key::scope_name(&directory),
                        site: crate::node_key::scope_name(&directory),
                        node: crate::node_key::scope_name(&directory),
                        camera: None,
                    };
                    while sampling.load(Ordering::SeqCst) {
                        let effective = store
                            .resolve(DETECTION_BACKEND_SETTING, &target)
                            .expect("the deployment answers for the detection backend");
                        record_sample(
                            "resolved setting",
                            &effective.requested.to_string(),
                            &effective
                                .running
                                .as_ref()
                                .map(SettingValue::to_string)
                                .unwrap_or_default(),
                            &effective
                                .pending
                                .map(|cause| cause.as_str().to_string())
                                .unwrap_or_else(|| crate::settings_projection::NONE.to_string()),
                        );
                    }
                }
            }));
        }
        // And the two surfaces an operator reads whole: the rendered `vigil
        // settings` line for a deployment with a store, and the automatic floor
        // a deployment that has never been started answers from.
        let never_started = tempfile::tempdir().expect("a directory holding no store");
        let surfaces = std::thread::spawn({
            let sampling = Arc::clone(&sampling);
            let record_sample = record_sample.clone();
            let directory = deployment.path().to_path_buf();
            let floor_target = target_of(never_started.path());
            move || {
                while sampling.load(Ordering::SeqCst) {
                    let answer = crate::settings_command::answer(
                        &directory,
                        &crate::settings_store::SettingsStore::store_path(&directory),
                        "",
                    );
                    let line = setting_line(&answer, DETECTION_BACKEND_SETTING);
                    record_sample(
                        "settings answer",
                        field(line, crate::settings_projection::REQUESTED_KEY).unwrap_or_default(),
                        field(line, crate::settings_projection::RUNNING_KEY).unwrap_or_default(),
                        field(line, crate::settings_projection::PENDING_KEY).unwrap_or_default(),
                    );
                    let floor = crate::settings_store::automatic_floor_listing(&floor_target);
                    let effective = floor
                        .iter()
                        .find(|setting| setting.setting == DETECTION_BACKEND_SETTING)
                        .expect("the automatic floor answers for the detection backend");
                    record_sample(
                        "automatic floor",
                        &effective.requested.to_string(),
                        &effective
                            .running
                            .as_ref()
                            .map(SettingValue::to_string)
                            .unwrap_or_default(),
                        &effective
                            .pending
                            .map(|cause| cause.as_str().to_string())
                            .unwrap_or_else(|| crate::settings_projection::NONE.to_string()),
                    );
                }
            }
        });
        // Every reader must be up and reading before the first landing, or the
        // landing crosses an empty room.
        while samples.load(AtomicOrdering::SeqCst) < READERS {
            std::thread::yield_now();
        }

        // Each attempt lets ONE real startup promotion land while the readers
        // are gathered on the seam. The promotion is the production one — the
        // forward probe answers Passed and the move goes onto the registered
        // handle through the one preparation machinery — and the pass this
        // artifact then runs for the request still standing re-selects against
        // the device it can actually see and moves back, which is what leaves
        // the node ready for the next attempt.
        //
        // The seam is held, released for just long enough to let the readers
        // past the domain switch's own publication, and held again with the
        // landing released behind it — so when it is let go the landing
        // publishes first and the readers gathered behind it are answered
        // after. A reader that has taken the choice and is waiting for the
        // value in force is exactly the reader this contract is about, and
        // gathering several is what makes the answer this test gets a fact
        // rather than a coin toss.
        let promotions = Arc::new(AtomicUsize::new(0));
        for _ in 0..ATTEMPTS {
            let (finish, wait_for_finish) = mpsc::channel();
            let (ended, preparation_ended) = mpsc::channel();
            crate::detection_accel::spawn_detection_probe_with_promotion(
                true,
                "yolox-test",
                crate::yolox_detector::MODEL_INPUT_SHAPE,
                DrivenStartupPreparation {
                    finish: wait_for_finish,
                },
                Arc::new(crate::acceleration::AccelerationState::new()),
                {
                    // Runs only once the move has genuinely landed, so this
                    // counts installs rather than probes.
                    let promotions = Arc::clone(&promotions);
                    move || {
                        promotions.fetch_add(1, AtomicOrdering::SeqCst);
                        Ok(())
                    }
                },
                move |_receipt: &AccelerationReceipt| {
                    let _ = ended.send(());
                },
            );

            // Held while the readers gather on it: every one of them is
            // inside a read it cannot finish, because the seam is the only
            // thing standing between it and an answer.
            let seam = crate::settings_application::hold_publication();
            std::thread::sleep(GATHER);
            // Let go until they come round again, then held once more with
            // the landing released behind it.
            drop(seam);
            std::thread::sleep(COME_ROUND);
            let seam = crate::settings_application::hold_publication();
            finish
                .send(
                    crate::detection_accel::DetectionForwardProbeOutcome::Passed {
                        selected_device: "test-accelerator".to_string(),
                        evidence_fields: std::collections::BTreeMap::new(),
                    },
                )
                .expect("release the startup preparation");
            // The landing now wants the seam, from behind the readers already
            // gathered on it.
            std::thread::sleep(GATHER);
            drop(seam);

            preparation_ended
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("the startup preparation must end on its own outcome");
            for _ in 0..20_000_000 {
                if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                    && !PREPARATION_OWED.load(Ordering::SeqCst)
                {
                    break;
                }
                std::thread::yield_now();
            }
        }
        sampling.store(false, Ordering::SeqCst);
        surfaces.join().expect("the operator-surface reader");
        for reader in reading {
            reader.join().expect("a reader");
        }

        assert_eq!(
            promotions.load(AtomicOrdering::SeqCst),
            ATTEMPTS,
            "every promotion must have genuinely landed on the registered handle, or the readers \
             crossed no publication at all"
        );
        assert!(
            saw_the_new_backend.load(Ordering::SeqCst),
            "at least one read must have been answered AFTER a promotion published, or the \
             readers were never held across a landing and this contract asserts nothing: {} \
             samples taken",
            samples.load(AtomicOrdering::SeqCst)
        );
        let readings = readings.lock().expect("the readers' samples").clone();
        assert!(
            readings.is_empty(),
            "every read taken while a move landed must be a complete state — the backend this \
             node runs, reported as the choice it made, with nothing outstanding. A read that \
             takes the choice on one side of the publication and the value in force on the other \
             tells the operator to restart a node already running what it asked for. Samples \
             that were neither: {readings:?}"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 18: a node preparing the accelerator is never told to
    // restart onto the backend it is already running ────────────────────────
    //
    // Promotion carries no false restart-pending on an already-live backend.
    // A default node — automatic management on, nothing authored — runs the
    // processor detector from the moment its cameras start, while the
    // accelerated preparation runs behind it. The choice the surface reports
    // in that window IS the running value, so a node that has recorded no
    // running backend has nothing for the reported choice to agree with and
    // the surface answers RESTART: it sends the operator to power-cycle a node
    // to reach the very backend that is feeding its frames.
    //
    // The window is not short and it does not always end. It lasts the whole
    // accelerated cold compile, and on a machine with no usable accelerator
    // the probe never passes, so nothing ever installs and the answer stands
    // for as long as the node runs.
    //
    // Nothing here is timing. The preparation answers only when this test
    // releases it, and both samples are the rendered operator surface.

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    fn a_node_preparing_the_accelerator_is_never_told_to_restart_onto_the_backend_it_runs() {
        assert_isolated_child_passed("preparing_node_is_never_told_to_restart_child");
    }

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    #[ignore = "re-executed in isolation by \
                a_node_preparing_the_accelerator_is_never_told_to_restart_onto_the_backend_it_runs; \
                mutates the process-global detector registry, coordinator and running-value \
                registry"]
    fn preparing_node_is_never_told_to_restart_child() {
        crate::settings_application::mark_running_process();
        let deployment = tempfile::tempdir().expect("a temporary deployment directory");
        let store = crate::settings_store::SettingsStore::open(deployment.path())
            .expect("a store for the deployment under test");
        drop(store);

        // The configuration under test, established rather than assumed: with
        // the domain left at its default the operator cannot author a backend
        // at all, so the author is Automatic and the reported choice follows
        // the running value.
        let refused = crate::settings_command::answer(
            deployment.path(),
            &crate::settings_store::SettingsStore::store_path(deployment.path()),
            &format!(
                "set {DETECTION_BACKEND_SETTING} {}",
                crate::detection_accel::CPU_DETECTION_BACKEND
            ),
        );
        assert!(
            crate::settings_command::answer_failed(&refused),
            "this contract is about the state a default node runs in: automatic management on, \
             so no operator value can exist. A deployment that accepts this write is not in that \
             state; got:\n{refused}"
        );

        // Nothing is seeded into the running-value registry here, and that is
        // the whole point: a node that has just started has recorded whatever
        // its own startup path records, and this contract asks what that is.
        let builds = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(AtomicUsize::new(0));
        let _handle = register_camera(
            "front",
            "yolox-test",
            true,
            true,
            Arc::clone(&builds),
            Arc::clone(&receipts),
        );

        let read_the_surface = |when: &str| {
            let answer = crate::settings_command::answer(
                deployment.path(),
                &crate::settings_store::SettingsStore::store_path(deployment.path()),
                "",
            );
            let line = setting_line(&answer, DETECTION_BACKEND_SETTING);
            let running = field(line, crate::settings_projection::RUNNING_KEY).unwrap_or_default();
            let pending = field(line, crate::settings_projection::PENDING_KEY).unwrap_or_default();
            assert_ne!(
                pending,
                crate::settings_model::PendingCause::Restart.as_str(),
                "{when}: the processor detector is running the cameras, and the choice this \
                 surface reports is that same backend — telling the operator to restart to \
                 reach it names a power cycle for a backend already feeding frames. Rendered \
                 line:\n{line}"
            );
            assert_eq!(
                running,
                crate::detection_accel::CPU_DETECTION_BACKEND,
                "{when}: and the value this node reports running must be the backend actually \
                 running detection. Rendered line:\n{line}"
            );
        };

        let (finish, wait_for_finish) = mpsc::channel();
        let (ended, preparation_ended) = mpsc::channel();
        let selection = crate::detection_accel::spawn_detection_probe_with_promotion(
            true,
            "yolox-test",
            crate::yolox_detector::MODEL_INPUT_SHAPE,
            DrivenStartupPreparation {
                finish: wait_for_finish,
            },
            Arc::new(crate::acceleration::AccelerationState::new()),
            || Ok(()),
            move |_receipt: &AccelerationReceipt| {
                let _ = ended.send(());
            },
        );
        assert_eq!(
            selection.backend,
            crate::detection_accel::CPU_DETECTION_BACKEND,
            "startup answers on the processor rather than waiting for an accelerator, which is \
             the backend this contract says the surface must report"
        );

        // Sampled while the preparation is demonstrably still under way: it
        // answers only when this test releases it, below.
        read_the_surface("while the accelerated preparation is under way");

        // And the machine that never gets an accelerator: the probe reports
        // that it can see no device, nothing is ever installed, and this
        // answer is the one the node gives from here on.
        finish
            .send(crate::detection_accel::DetectionForwardProbeOutcome::NoDeviceVisible)
            .expect("release the startup preparation");
        preparation_ended
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the startup preparation must end on its own outcome");
        for _ in 0..2_000_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
            {
                break;
            }
            std::thread::yield_now();
        }
        read_the_surface("after the preparation reported that this machine has no accelerator");

        println!("child_assertions_passed");
    }

    // ── Contract 19: a return onto a detector this node still has loaded is
    // never answered with a restart ─────────────────────────────────────────
    //
    // Reverse and repeat-forward commands return immediately using retained
    // instances, and promotion is one indivisible runtime update with no false
    // restart-pending on an already-live backend. An operator moving back onto
    // a backend this node has an instance for is entitled to read what they
    // asked for, what is still running, and that Vigil is closing the gap
    // itself — never an instruction to power-cycle a node that is seconds away
    // from doing the work in this very process.
    //
    // Every other live-transition contract in this file seeds a running value
    // nothing has a detector for, so the move under test always has real work
    // to do and the retained way back is never taken. This one is the opposite
    // case and takes the retained way back deliberately: the instance it
    // returns onto is created by a genuine forward move through the production
    // command path, not by a test writing one into a registry.
    //
    // Nothing here is timing. The answer asserted on is the string the command
    // itself returned, and the preparation registry is held across the command
    // so no background pass can change the answer while it is being rendered.

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    fn a_return_onto_a_retained_detector_is_never_answered_with_restart() {
        assert_isolated_child_passed("return_onto_retained_detector_child");
    }

    #[cfg(feature = "detect-burn-wgpu")]
    #[test]
    #[ignore = "re-executed in isolation by \
                a_return_onto_a_retained_detector_is_never_answered_with_restart; mutates the \
                process-global detector registry, coordinator and running-value registry"]
    fn return_onto_retained_detector_child() {
        crate::settings_application::mark_running_process();
        let deployment = tempfile::tempdir().expect("a temporary deployment directory");
        // Naming a backend by hand means taking the wheel off the automation
        // first, which is the only way an operator reaches this command at all.
        {
            let store = crate::settings_store::SettingsStore::open(deployment.path())
                .expect("a store for the deployment under test");
            store
                .set_local(
                    crate::settings_domains::ACCELERATED_DETECTION_DOMAIN,
                    crate::settings_model::Surface::VigilSettings,
                    crate::settings_model::Scope::node(crate::node_key::scope_name(
                        deployment.path(),
                    )),
                    SettingValue::Bool(false),
                )
                .expect("turn the automation off");
        }
        // The node this contract is about is one already running the
        // accelerated backend — the state every node the reverse command
        // matters on is in. This is the starting condition, not the state
        // under test: what the command below returns onto is an instance a
        // real move retains, and the move that creates it runs through the
        // production command path immediately after.
        bring_into_force(
            DETECTION_BACKEND_SETTING,
            SettingValue::text(crate::detection_accel::ACCELERATED_DETECTION_BACKEND),
        );
        let builds = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(AtomicUsize::new(0));
        let _handle = register_camera(
            "front",
            "yolox-test",
            false,
            true,
            Arc::clone(&builds),
            Arc::clone(&receipts),
        );

        // The genuine forward move, through the real CLI answer path, driven
        // to completion: it installs the processor detector and keeps the
        // accelerated instance it stepped away from as the prepared way back.
        let forward = crate::settings_command::answer(
            deployment.path(),
            &crate::settings_store::SettingsStore::store_path(deployment.path()),
            &format!(
                "set {DETECTION_BACKEND_SETTING} {}",
                crate::detection_accel::CPU_DETECTION_BACKEND
            ),
        );
        assert!(
            !crate::settings_command::answer_failed(&forward),
            "naming a backend this artifact carries is an ordinary change; got:\n{forward}"
        );
        for _ in 0..2_000_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
            {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(
            in_force(DETECTION_BACKEND_SETTING),
            Some(SettingValue::text(
                crate::detection_accel::CPU_DETECTION_BACKEND
            )),
            "the forward move must have landed before the return is asked for, or this contract \
             is about something else"
        );
        assert!(
            prepared_detector(
                "front",
                crate::detection_accel::ACCELERATED_DETECTION_BACKEND
            )
            .is_some(),
            "and it must have kept the instance it stepped away from — the retained way back is \
             what the command below returns onto"
        );
        let builds_after_forward = builds.load(AtomicOrdering::SeqCst);

        // Held across the command so the answer asserted on cannot be
        // overtaken by a background preparation: the pass reads this registry
        // before it can install or publish anything.
        let held = prepared_detectors()
            .lock()
            .expect("hold the prepared-detector registry across the return command");

        let answer = crate::settings_command::answer(
            deployment.path(),
            &crate::settings_store::SettingsStore::store_path(deployment.path()),
            &format!(
                "set {DETECTION_BACKEND_SETTING} {}",
                crate::detection_accel::ACCELERATED_DETECTION_BACKEND
            ),
        );
        assert!(
            !crate::settings_command::answer_failed(&answer),
            "returning to a backend this artifact carries is an ordinary change; got:\n{answer}"
        );
        let line = setting_line(&answer, DETECTION_BACKEND_SETTING);
        let pending = field(line, crate::settings_projection::PENDING_KEY).unwrap_or_default();
        let running = field(line, crate::settings_projection::RUNNING_KEY).unwrap_or_default();
        assert_ne!(
            pending,
            crate::settings_model::PendingCause::Restart.as_str(),
            "this node has the detector for the backend just asked for still loaded and closes \
             the gap itself, in this process, in seconds. Answering restart tells the operator \
             to power-cycle the node that is already doing the work. Rendered line:\n{line}"
        );
        let carrying = pending == crate::settings_model::PendingCause::LiveTransition.as_str()
            && field(line, PREPARING_SINCE_KEY)
                .and_then(|since| since.parse::<u64>().ok())
                .is_some_and(|since| since > 0);
        let landed = pending == crate::settings_projection::NONE
            && running == crate::detection_accel::ACCELERATED_DETECTION_BACKEND;
        assert!(
            carrying || landed,
            "and the line has to be coherent on its own terms: either the move is under way — \
             named as a live transition, with when it began — or it is done, with the backend \
             asked for reported running. Anything else is a gap with nothing closing it. \
             Rendered line:\n{line}"
        );
        assert_eq!(
            builds.load(AtomicOrdering::SeqCst),
            builds_after_forward,
            "and no model may be loaded to buy that answer: the whole point of retaining the \
             instance is that the settings control socket never waits on a model load"
        );

        drop(held);
        for _ in 0..2_000_000 {
            if !PREPARATION_RUNNING.load(Ordering::SeqCst)
                && !PREPARATION_OWED.load(Ordering::SeqCst)
            {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(
            builds.load(AtomicOrdering::SeqCst),
            builds_after_forward,
            "the return onto a retained instance loads no model on any thread"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 20: a decode change is never answered with a restart ───────
    //
    // A manual backend command returns at once showing what was asked for,
    // what is still running, and a transition in progress — and the settings
    // surface never answers restart for a setting Vigil applies in-process.
    // Decode is such a setting: naming a decode path bumps the selection this
    // node runs on, the camera loop ends its session on it and the fresh
    // session selects afresh, all in the running process. Answering restart
    // for it contradicts the same line's own statement that it applies live.
    //
    // The live stream is load-bearing: with no pipeline running there is
    // nothing to reconnect, the change settles without one, and a test without
    // it would exercise nothing at all.

    #[cfg(feature = "decode-gstreamer")]
    #[test]
    fn a_decode_backend_change_is_never_answered_with_restart() {
        assert_isolated_child_passed("decode_change_is_never_answered_with_restart_child");
    }

    #[cfg(feature = "decode-gstreamer")]
    #[test]
    #[ignore = "re-executed in isolation by a_decode_backend_change_is_never_answered_with_restart; \
                mutates the process-global live-stream count, decode epoch and running-value \
                registry"]
    fn decode_change_is_never_answered_with_restart_child() {
        crate::settings_application::mark_running_process();
        let deployment = tempfile::tempdir().expect("a temporary deployment directory");
        // As with detection: naming a decode path by hand means taking the
        // wheel off the automation first.
        {
            let store = crate::settings_store::SettingsStore::open(deployment.path())
                .expect("a store for the deployment under test");
            store
                .set_local(
                    crate::settings_domains::HARDWARE_DECODING_DOMAIN,
                    crate::settings_model::Surface::VigilSettings,
                    crate::settings_model::Scope::node(crate::node_key::scope_name(
                        deployment.path(),
                    )),
                    SettingValue::Bool(false),
                )
                .expect("turn the automation off");
        }
        // A node that has settled on software decoding, said by the production
        // path that settles it rather than written here.
        reselect_decode_backend(false);
        assert_eq!(
            in_force(DECODE_BACKEND_SETTING),
            Some(SettingValue::text(
                crate::settings_backends::SOFTWARE_DECODE_BACKEND
            )),
            "the baseline this contract moves away from is a node genuinely decoding in software"
        );
        // A camera pipeline this process is running, so a decode change has
        // something to reconnect.
        register_live_stream(false);

        let read_the_answer = |answer: &str, requested: &str, when: &str| {
            assert!(
                !crate::settings_command::answer_failed(answer),
                "{when}: naming a decode path this artifact carries is an ordinary change; \
                 got:\n{answer}"
            );
            let line = setting_line(answer, DECODE_BACKEND_SETTING);
            assert_eq!(
                field(line, crate::settings_projection::APPLIES_KEY),
                Some(crate::settings_projection::LIVE_TOKEN),
                "{when}: this build applies a decode change on the running process, and the line \
                 says so: {line}"
            );
            let pending = field(line, crate::settings_projection::PENDING_KEY).unwrap_or_default();
            let running = field(line, crate::settings_projection::RUNNING_KEY).unwrap_or_default();
            assert_ne!(
                pending,
                crate::settings_model::PendingCause::Restart.as_str(),
                "{when}: the same line cannot say a change applies live and then send the \
                 operator to restart for it — this node closes the gap itself, by reconnecting \
                 the camera under the path just named: {line}"
            );
            let carrying = pending == crate::settings_model::PendingCause::LiveTransition.as_str()
                && field(line, PREPARING_SINCE_KEY)
                    .and_then(|since| since.parse::<u64>().ok())
                    .is_some_and(|since| since > 0);
            let landed = pending == crate::settings_projection::NONE && running == requested;
            assert!(
                carrying || landed,
                "{when}: and the line has to be coherent on its own terms — either the change is \
                 under way, named with when it began, or it is done and the path asked for is \
                 reported running: {line}"
            );
        };

        let pinned = crate::settings_command::answer(
            deployment.path(),
            &crate::settings_store::SettingsStore::store_path(deployment.path()),
            &format!(
                "set {DECODE_BACKEND_SETTING} {}",
                crate::settings_backends::HARDWARE_DECODE_BACKEND
            ),
        );
        read_the_answer(
            &pinned,
            crate::settings_backends::HARDWARE_DECODE_BACKEND,
            "pinning the hardware decode path",
        );

        // What the fresh session reports having entered, written where the
        // selection itself writes it. Until this lands the operator's surface
        // is answering about a change still under way; from here the node is
        // genuinely decoding in hardware, which is what makes the reverse
        // below a real reverse rather than a repeat.
        bring_into_force(
            DECODE_BACKEND_SETTING,
            SettingValue::text(crate::settings_backends::HARDWARE_DECODE_BACKEND),
        );

        let reversed = crate::settings_command::answer(
            deployment.path(),
            &crate::settings_store::SettingsStore::store_path(deployment.path()),
            &format!(
                "set {DECODE_BACKEND_SETTING} {}",
                crate::settings_backends::SOFTWARE_DECODE_BACKEND
            ),
        );
        read_the_answer(
            &reversed,
            crate::settings_backends::SOFTWARE_DECODE_BACKEND,
            "reversing to the software decode path",
        );

        println!("child_assertions_passed");
    }

    // ── Contract 21: a superseded decode completion installs nothing and
    // clears nothing ────────────────────────────────────────────────────────
    //
    // A decode selection belongs to the request that asked for it. A session
    // that opened under an older decode answer and settles after a newer
    // command has landed is reporting what it entered for a request nobody is
    // waiting on any more: it may not publish its outcome as this node's
    // running decode path, and it may not end the transition the newer command
    // started. Without that identity the operator is told, character for
    // character, `requested=hardware running=software pending=restart
    // applies=live` — the same line saying the change applies live and that
    // they must power-cycle the node for it, for a reconnect this node is
    // still carrying.

    #[cfg(feature = "decode-gstreamer")]
    #[test]
    fn a_superseded_decode_completion_installs_nothing_and_clears_nothing() {
        assert_isolated_child_passed("superseded_decode_completion_installs_nothing_child");
    }

    #[cfg(feature = "decode-gstreamer")]
    #[test]
    #[ignore = "re-executed in isolation by \
                a_superseded_decode_completion_installs_nothing_and_clears_nothing; mutates the \
                process-global live-stream count, decode epoch, decode transition and \
                running-value registry"]
    fn superseded_decode_completion_installs_nothing_child() {
        crate::settings_application::mark_running_process();
        let deployment = tempfile::tempdir().expect("a temporary deployment directory");
        // Naming a decode path by hand means taking the wheel off the
        // automation first, as everywhere else on this surface.
        {
            let store = crate::settings_store::SettingsStore::open(deployment.path())
                .expect("a store for the deployment under test");
            store
                .set_local(
                    crate::settings_domains::HARDWARE_DECODING_DOMAIN,
                    crate::settings_model::Surface::VigilSettings,
                    crate::settings_model::Scope::node(crate::node_key::scope_name(
                        deployment.path(),
                    )),
                    SettingValue::Bool(false),
                )
                .expect("turn the automation off");
        }
        // A node genuinely decoding in software, said by the production path
        // that settles it.
        reselect_decode_backend(false);
        assert_eq!(
            in_force(DECODE_BACKEND_SETTING),
            Some(SettingValue::text(
                crate::settings_backends::SOFTWARE_DECODE_BACKEND
            )),
            "the baseline this contract moves away from is a node genuinely decoding in software"
        );
        // A camera pipeline this process is running, so the decode change below
        // has something to reconnect. Load-bearing: with no live stream
        // `reselect_decode_backend` settles on the spot and no transition is
        // ever outstanding, so the contract would exercise nothing.
        register_live_stream(false);

        let pinned = crate::settings_command::answer(
            deployment.path(),
            &crate::settings_store::SettingsStore::store_path(deployment.path()),
            &format!(
                "set {DECODE_BACKEND_SETTING} {}",
                crate::settings_backends::HARDWARE_DECODE_BACKEND
            ),
        );
        assert!(
            !crate::settings_command::answer_failed(&pinned),
            "naming a decode path this artifact carries is an ordinary change; got:\n{pinned}"
        );
        let carrying = decode_transition_since_ms();
        assert!(
            carrying.is_some_and(|since| since > 0),
            "the command started a reconnect this node carries itself, so a transition is \
             outstanding before anything settles"
        );

        // A completion from a session that opened before that command: it
        // belongs to the previous decode answer, and the operator is no longer
        // waiting on it.
        settle_decode_selection(0, false);

        assert!(
            decode_transition_since_ms().is_some(),
            "a completion for a superseded request may not end the reconnect the newer command \
             started: the node is still carrying it, and clearing it here is what makes the \
             surface send the operator to restart for a change already under way"
        );
        assert_eq!(
            in_force(DECODE_BACKEND_SETTING),
            Some(SettingValue::text(
                crate::settings_backends::SOFTWARE_DECODE_BACKEND
            )),
            "and it installs nothing: the running decode path is what a session belonging to the \
             current request reports, never an older one's outcome"
        );
        let listing = crate::settings_command::answer(
            deployment.path(),
            &crate::settings_store::SettingsStore::store_path(deployment.path()),
            "list",
        );
        let line = setting_line(&listing, DECODE_BACKEND_SETTING);
        assert_eq!(
            field(line, crate::settings_projection::PENDING_KEY),
            Some(crate::settings_model::PendingCause::LiveTransition.as_str()),
            "so the operator surface still names the reconnect this node is carrying, rather \
             than answering restart for a setting the same line says applies live: {line}"
        );
        assert!(
            field(line, PREPARING_SINCE_KEY)
                .and_then(|since| since.parse::<u64>().ok())
                .is_some_and(|since| since > 0),
            "named with when it began, as a transition under way always is: {line}"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 21b: a transition is stamped for the request it belongs to,
    // before that request is visible ────────────────────────────────────────
    //
    // The reconnect a command starts and the decode answer that command
    // published are one thing, so the transition names which request it is
    // carrying — and it is stamped before the new answer becomes visible, so
    // there is no moment where a completion can see the new epoch and find no
    // transition to check itself against.

    #[test]
    fn a_decode_transition_names_the_request_it_belongs_to() {
        assert_isolated_child_passed("decode_transition_names_the_request_it_belongs_to_child");
    }

    #[test]
    #[ignore = "re-executed in isolation by a_decode_transition_names_the_request_it_belongs_to; \
                mutates the process-global live-stream count, decode epoch and decode transition"]
    fn decode_transition_names_the_request_it_belongs_to_child() {
        crate::settings_application::mark_running_process();
        reselect_decode_backend(false);
        register_live_stream(false);
        reselect_decode_backend(true);
        assert_eq!(
            decode_transition_epoch(),
            Some(decode_selection_epoch()),
            "the outstanding reconnect is the one this node's current decode answer asked for; a \
             transition that names no request cannot tell a completion it is waiting for from one \
             it is not"
        );
        // And again on the next command, so the stamp follows the request
        // rather than being written once and left behind.
        reselect_decode_backend(false);
        assert_eq!(
            decode_transition_epoch(),
            Some(decode_selection_epoch()),
            "every command restamps the transition for its own request"
        );

        println!("child_assertions_passed");
    }

    // ── Contract 22: a session selects against the epoch it opened under ────
    //
    // The decode answer a camera session belongs to is fixed when the session
    // opens, not re-read once its RTSP handshake is done. A selection run for
    // an older decode answer publishes nothing and ends no transition — which
    // is what stops a command that landed while a camera was opening from
    // being taken on by nobody at all.

    #[test]
    fn a_decode_selection_for_an_older_epoch_publishes_nothing() {
        assert_isolated_child_passed("decode_selection_for_an_older_epoch_publishes_nothing_child");
    }

    #[test]
    #[ignore = "re-executed in isolation by a_decode_selection_for_an_older_epoch_publishes_nothing; \
                mutates the process-global live-stream count, decode epoch, decode transition and \
                running-value registry"]
    fn decode_selection_for_an_older_epoch_publishes_nothing_child() {
        crate::settings_application::mark_running_process();
        // A node settled on software, at decode epoch 0.
        reselect_decode_backend(false);
        assert_eq!(
            in_force(DECODE_BACKEND_SETTING),
            Some(SettingValue::text(
                crate::settings_backends::SOFTWARE_DECODE_BACKEND
            )),
            "the baseline is a node genuinely decoding in software"
        );
        let opened_under = decode_selection_epoch();
        register_live_stream(false);
        // The operator names the other path while a camera is still opening a
        // session under `opened_under`.
        reselect_decode_backend(true);
        assert_ne!(
            decode_selection_epoch(),
            opened_under,
            "the command moved this node onto a new decode answer"
        );
        assert!(
            decode_transition_since_ms().is_some(),
            "and a reconnect is outstanding for it"
        );

        // That camera's session now finishes its handshake and selects — for
        // the decode answer it opened under, which is no longer this node's.
        let selection = crate::decode::select_decode_backend(
            &crate::workgraph::StreamId::new("front-yard"),
            crate::media_pipeline::VideoCodec::H264,
            1,
            opened_under,
            false,
            &[],
        )
        .expect("a disabled decode intent always yields the software path");
        assert!(
            !selection.receipt.hardware_accelerated,
            "the stale session selected the software path it opened under"
        );

        assert!(
            decode_transition_since_ms().is_some(),
            "a selection belonging to an older decode answer ends no transition: the reconnect \
             the operator asked for is still owed, and clearing it here leaves the change \
             silently unapplied behind a surface that says it landed"
        );
        assert_eq!(
            in_force(DECODE_BACKEND_SETTING),
            Some(SettingValue::text(
                crate::settings_backends::SOFTWARE_DECODE_BACKEND
            )),
            "and publishes nothing of its own"
        );

        println!("child_assertions_passed");
    }
}
