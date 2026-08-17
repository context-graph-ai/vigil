//! Changing the detection backend while the node keeps watching.
//!
//! An operator names a backend and gets an answer back straight away: what
//! they asked for, what is still running, and that a change is under way. The
//! machine then does the expensive part — building and forward-testing a
//! detector for every registered handle — away from the command, away from
//! camera processing, and away from every read of the settings surface. Only
//! when every registered handle is running the new backend does the node say
//! it is running that backend.
//!
//! A preparation under way ends exactly two ways: it completes, or the
//! preparation itself reports a real error. Nothing else ends it. A slow cold
//! build on capable hardware is not a failure and is never turned into one, so
//! while it runs the surface says when it started and what it last reported —
//! visible without being judged.
//!
//! Everything below drives that work by hand rather than waiting for it: the
//! preparation step is executed by an explicit call, and progress arrives as
//! events the test emits. No assertion here reads a stopwatch, nothing sleeps,
//! and no elapsed time decides any outcome.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vigil::detection_accel::{ACCELERATED_DETECTION_BACKEND, CPU_DETECTION_BACKEND};
use vigil::detection_transition::{
    DetectionTransitions, HandleKind, InstallOutcome, PreparationIdentity, PreparationOutcome,
    PreparedInstance,
};

const MODEL: &str = "yolox-tiny";
const CLASSES: [&str; 1] = ["person"];

/// The moment every coordinator in this file is built at. Fixed, and never
/// moved: time is here so a surface can say when a preparation started, not so
/// a test can make something happen by moving it.
const FIXED_MILLIS: u64 = 1_700_000_000_000;

fn fixed_clock() -> Arc<dyn Fn() -> u64 + Send + Sync> {
    Arc::new(|| FIXED_MILLIS)
}

/// A stand-in for the physical work of building and forward-testing one
/// detector: it mints a distinct instance per call and records every identity
/// it was asked for, so a duplicated build is visible as a second call rather
/// than inferred.
#[derive(Clone)]
struct PreparationRecorder {
    calls: Arc<Mutex<Vec<PreparationIdentity>>>,
    next_instance: Arc<AtomicU64>,
}

impl PreparationRecorder {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            next_instance: Arc::new(AtomicU64::new(1)),
        }
    }

    fn succeed(&self) -> impl Fn(&PreparationIdentity) -> PreparationOutcome + '_ {
        move |identity: &PreparationIdentity| {
            self.calls.lock().expect("calls").push(identity.clone());
            PreparationOutcome::Prepared(PreparedInstance(
                self.next_instance.fetch_add(1, Ordering::SeqCst),
            ))
        }
    }

    fn succeed_only(
        &self,
        handle: &'static str,
    ) -> impl Fn(&PreparationIdentity) -> PreparationOutcome + '_ {
        move |identity: &PreparationIdentity| {
            self.calls.lock().expect("calls").push(identity.clone());
            if identity.handle == handle {
                PreparationOutcome::Prepared(PreparedInstance(
                    self.next_instance.fetch_add(1, Ordering::SeqCst),
                ))
            } else {
                PreparationOutcome::Failed(format!("{} cannot build this backend", identity.handle))
            }
        }
    }

    /// A real error reported by the preparation itself — the only thing other
    /// than completion that ends an attempt.
    fn fail(&self) -> impl Fn(&PreparationIdentity) -> PreparationOutcome + '_ {
        move |identity: &PreparationIdentity| {
            self.calls.lock().expect("calls").push(identity.clone());
            PreparationOutcome::Failed("no usable graphics device".to_string())
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().expect("calls").len()
    }

    fn last_instance(&self) -> PreparedInstance {
        PreparedInstance(self.next_instance.load(Ordering::SeqCst) - 1)
    }
}

fn coordinator_with_one_camera() -> DetectionTransitions {
    let transitions = DetectionTransitions::with_clock(CPU_DETECTION_BACKEND, fixed_clock());
    transitions.register_handle("front", HandleKind::Camera, MODEL, &CLASSES);
    transitions
}

// ── The command comes back before the work does ────────────────────────────

#[test]
fn a_backend_command_answers_with_requested_running_and_a_transition_under_way() {
    // Unfakeable without doing the work off the command's path: the physical
    // preparation is deliberately not executed here, so a coordinator that
    // performed it inline could not reach these assertions with the work still
    // outstanding. The three facts are read as one answer, which is what the
    // operator is shown.
    let transitions = coordinator_with_one_camera();

    transitions.request_backend(ACCELERATED_DETECTION_BACKEND);

    let state = transitions.state();
    assert_eq!(
        state.requested_backend.as_deref(),
        Some(ACCELERATED_DETECTION_BACKEND),
        "the answer must name the backend the operator asked for"
    );
    assert_eq!(
        state.running_backend, CPU_DETECTION_BACKEND,
        "the answer must name the backend still running the cameras"
    );
    let pending = state
        .pending
        .as_ref()
        .expect("the answer must say a change is under way, not that a restart is owed");
    assert_eq!(
        pending.target_backend, ACCELERATED_DETECTION_BACKEND,
        "the transition under way must name what it is preparing"
    );
    assert_eq!(
        transitions.scheduled_preparations().len(),
        1,
        "the work must be outstanding, not already done on the command's own path"
    );
}

// ── A preparation under way is visible, and never judged ───────────────────

#[test]
fn a_preparation_under_way_says_when_it_started_and_what_it_last_reported() {
    // A slow cold build on capable hardware must be watchable without being
    // failed. So the surface carries the moment preparation began and the last
    // thing the preparation said about itself — the two facts that tell a
    // person waiting apart from a person stuck.
    let transitions = coordinator_with_one_camera();
    let version = transitions.request_backend(ACCELERATED_DETECTION_BACKEND);

    let pending = transitions
        .state()
        .pending
        .expect("a scheduled preparation is pending");
    assert_eq!(
        pending.preparing_since_ms, FIXED_MILLIS,
        "the surface must carry the moment preparation began, stamped from the deployment's \
         one clock"
    );
    assert_eq!(
        pending.latest_progress, None,
        "a preparation that has said nothing yet must not have words put in its mouth"
    );

    transitions.report_progress(version, "compiling shaders");
    transitions.report_progress(version, "loading model weights");

    let pending = transitions
        .state()
        .pending
        .expect("reporting progress does not end a preparation");
    assert_eq!(
        pending.latest_progress.as_deref(),
        Some("loading model weights"),
        "the surface must carry the latest thing the preparation reported"
    );
    assert_eq!(
        pending.preparing_since_ms, FIXED_MILLIS,
        "progress does not restart the attempt"
    );
}

#[test]
fn moving_the_clock_changes_nothing_a_surface_reports() {
    // The one mechanical statement of the rule the rest of this file is
    // written under: a clock is something a surface READS so a person can tell
    // slow from wedged, never something that decides an outcome. This is the
    // only test here holding a clock it can move, and it moves it precisely to
    // prove that moving it does nothing — a coordinator that failed, expired,
    // re-explained, or re-attempted anything on elapsed time fails here.
    let now = Arc::new(AtomicU64::new(FIXED_MILLIS));
    let readable = {
        let now = Arc::clone(&now);
        Arc::new(move || now.load(Ordering::SeqCst)) as Arc<dyn Fn() -> u64 + Send + Sync>
    };
    let transitions = DetectionTransitions::with_clock(CPU_DETECTION_BACKEND, readable);
    transitions.register_handle("front", HandleKind::Camera, MODEL, &CLASSES);
    let version = transitions.request_backend(ACCELERATED_DETECTION_BACKEND);
    transitions.report_progress(version, "compiling shaders");

    let before = transitions.state();
    // A week, which is longer than any preparation anyone would defend, and
    // still not an event.
    now.fetch_add(7 * 24 * 60 * 60 * 1_000, Ordering::SeqCst);
    let after = transitions.state();

    assert_eq!(
        after.running_backend, before.running_backend,
        "no amount of time moves the running backend"
    );
    assert_eq!(
        after.requested_backend, before.requested_backend,
        "no amount of time changes what the operator asked for"
    );
    assert_eq!(
        after.failure_reason, before.failure_reason,
        "no amount of time invents a failure"
    );
    let (before_pending, after_pending) = (
        before.pending.expect("a preparation was under way"),
        after.pending.expect("and it still is"),
    );
    assert_eq!(
        after_pending.preparing_since_ms, before_pending.preparing_since_ms,
        "the moment preparation began is a fact about the past and does not drift"
    );
    assert_eq!(
        after_pending.latest_progress, before_pending.latest_progress,
        "the last thing the preparation said changes when it says something, not when time \
         passes"
    );
    assert_eq!(
        after_pending.target_backend, before_pending.target_backend,
        "and what is being prepared is unchanged"
    );
    assert_eq!(
        transitions.scheduled_preparations().len(),
        1,
        "the outstanding work is neither abandoned nor duplicated by the passage of time"
    );
}

#[test]
fn a_preparation_that_has_neither_completed_nor_errored_stays_pending() {
    // Nothing but completion or a real error may end an attempt. Reading the
    // surface, repeatedly, is not an event: a coordinator that gave up on a
    // preparation because it was asked about it often enough would fail its
    // slowest and most important case — the first cold build on a machine
    // that can genuinely do the work.
    let transitions = coordinator_with_one_camera();
    let version = transitions.request_backend(ACCELERATED_DETECTION_BACKEND);
    transitions.report_progress(version, "still compiling");

    for read in 0..5 {
        let state = transitions.state();
        assert!(
            state.pending.is_some(),
            "read {read}: an unfinished preparation is still under way"
        );
        assert!(
            state.failure_reason.is_none(),
            "read {read}: nothing may turn a running preparation into a failure"
        );
        assert_eq!(
            state.running_backend, CPU_DETECTION_BACKEND,
            "read {read}: the cameras keep running what they are running"
        );
    }

    assert_eq!(
        transitions.scheduled_preparations().len(),
        1,
        "the work is still outstanding and still owned by this attempt"
    );
}

// ── One physical preparation per detector identity ─────────────────────────

#[test]
fn each_detector_identity_is_prepared_once_however_many_times_it_is_asked_for() {
    // A handle, the model artifact it loads, the classes it emits, and the
    // backend it targets are one identity. Two requests naming the same
    // identity are the same physical build, so asking twice must not build
    // twice — repeated attempts piling up abandoned builds is what this
    // forbids.
    let transitions = coordinator_with_one_camera();
    let recorder = PreparationRecorder::new();

    let first = transitions.request_backend(ACCELERATED_DETECTION_BACKEND);
    let second = transitions.request_backend(ACCELERATED_DETECTION_BACKEND);
    assert!(
        second > first,
        "a repeated request is its own request version, so a later completion can be told \
         from an earlier one"
    );
    assert_eq!(
        transitions.scheduled_preparations().len(),
        1,
        "a repeat of work already under way must attach to it rather than schedule a second \
         physical build"
    );

    transitions.drive_preparations(recorder.succeed());
    assert_eq!(
        recorder.call_count(),
        1,
        "one identity, one physical preparation"
    );
    assert_eq!(
        transitions.physical_preparation_count(),
        1,
        "the coordinator's own count of physical builds must agree"
    );
}

#[test]
fn the_instance_that_was_forward_tested_is_the_instance_that_runs() {
    // The cold work is a build AND a forward test of one detector. Loading a
    // second detector to install after testing the first pays the cold cost
    // twice and installs something that was never tested, so the installed
    // instance is asserted to be the exact one the preparation returned.
    let transitions = coordinator_with_one_camera();
    let recorder = PreparationRecorder::new();

    transitions.request_backend(ACCELERATED_DETECTION_BACKEND);
    transitions.drive_preparations(recorder.succeed());

    assert_eq!(
        transitions.installed_instance("front"),
        Some(recorder.last_instance()),
        "the installed detector must be the instance the preparation forward-tested"
    );
    assert_eq!(
        recorder.call_count(),
        1,
        "installing must not trigger a second build of the same identity"
    );
}

#[test]
fn completing_changes_every_fact_together() {
    // The move is one indivisible flip, so no read can catch the machine
    // half-changed: the node names the new backend, nothing is pending any
    // more, and no failure is claimed.
    let transitions = coordinator_with_one_camera();
    let recorder = PreparationRecorder::new();

    transitions.request_backend(ACCELERATED_DETECTION_BACKEND);
    transitions.drive_preparations(recorder.succeed());

    let state = transitions.state();
    assert_eq!(state.running_backend, ACCELERATED_DETECTION_BACKEND);
    assert_eq!(
        state.requested_backend.as_deref(),
        Some(ACCELERATED_DETECTION_BACKEND)
    );
    assert!(
        state.pending.is_none(),
        "a completed move leaves nothing under way"
    );
    assert!(
        state.failure_reason.is_none(),
        "a completed move claims no failure"
    );
    assert_eq!(state.moved_handles, vec!["front".to_string()]);
    assert!(state.unmoved_handles.is_empty());
}

// ── Request versions ───────────────────────────────────────────────────────

#[test]
fn an_older_requests_completion_can_never_replace_the_backend_now_asked_for() {
    // The operator's last word wins. An earlier request finishing its cold
    // build after the operator changed their mind must install nothing, or the
    // machine ends up running the backend nobody currently wants.
    let transitions = coordinator_with_one_camera();

    let older = transitions.request_backend(ACCELERATED_DETECTION_BACKEND);
    let newer = transitions.request_backend(CPU_DETECTION_BACKEND);
    assert!(newer > older);

    let outcome = transitions.complete_preparation("front", older, PreparedInstance(41));
    assert_eq!(
        outcome,
        InstallOutcome::RejectedStaleVersion,
        "a completion from a superseded request must be refused"
    );
    assert_eq!(
        transitions.installed_instance("front"),
        None,
        "nothing from a superseded request may be installed"
    );
    assert_eq!(
        transitions.state().requested_backend.as_deref(),
        Some(CPU_DETECTION_BACKEND),
        "the request the operator now holds must stand"
    );
}

#[test]
fn repeating_the_same_backend_after_a_real_error_is_a_fresh_attempt() {
    // The documented action after a failed move is to ask for it again. An
    // unchanged-value no-op would make that action do nothing at all, leaving
    // the operator repeating a command that cannot succeed.
    let transitions = coordinator_with_one_camera();
    let recorder = PreparationRecorder::new();

    let first = transitions.request_backend(ACCELERATED_DETECTION_BACKEND);
    transitions.drive_preparations(recorder.fail());
    let failed = transitions.state();
    assert!(
        failed.failure_reason.is_some(),
        "an error reported by the preparation must leave a reason an operator can read"
    );
    assert!(
        failed.pending.is_none(),
        "an attempt that ended in a real error is no longer under way"
    );
    assert_eq!(
        failed.running_backend, CPU_DETECTION_BACKEND,
        "a failed preparation leaves the detectors on the backend they are running"
    );
    assert_eq!(
        failed.requested_backend.as_deref(),
        Some(ACCELERATED_DETECTION_BACKEND),
        "a failure must not delete what the operator asked for"
    );

    let retry = transitions.request_backend(ACCELERATED_DETECTION_BACKEND);
    assert!(
        retry > first,
        "the same value asked for again after a real error is a new request version"
    );
    assert_eq!(
        transitions.scheduled_preparations().len(),
        1,
        "a fresh attempt must schedule real work, never be discarded as an unchanged value"
    );

    // The abandoned work of the attempt that errored must stay abandoned: a
    // straggler completing under the old version installs nothing.
    assert_eq!(
        transitions.complete_preparation("front", first, PreparedInstance(99)),
        InstallOutcome::RejectedStaleVersion,
        "work belonging to a finished attempt can never install itself"
    );

    transitions.drive_preparations(recorder.succeed());
    assert_eq!(
        recorder.call_count(),
        2,
        "the fresh attempt must have performed a second, genuinely new physical preparation"
    );
    assert_eq!(
        transitions.state().running_backend,
        ACCELERATED_DETECTION_BACKEND,
        "an attempt that succeeds moves the node"
    );
}

// ── The node claims a backend only when every handle runs it ───────────────

#[test]
fn the_node_claims_the_new_backend_only_after_every_handle_has_moved() {
    let transitions = DetectionTransitions::with_clock(CPU_DETECTION_BACKEND, fixed_clock());
    transitions.register_handle("front", HandleKind::Camera, MODEL, &CLASSES);
    transitions.register_handle("back", HandleKind::Camera, MODEL, &CLASSES);
    let recorder = PreparationRecorder::new();

    transitions.request_backend(ACCELERATED_DETECTION_BACKEND);
    transitions.drive_preparations(recorder.succeed_only("front"));

    let state = transitions.state();
    assert_eq!(
        state.running_backend, CPU_DETECTION_BACKEND,
        "with one handle still on the old backend the node has not moved"
    );
    assert_eq!(
        state.moved_handles,
        vec!["front".to_string()],
        "a partial move names the handles that moved"
    );
    assert_eq!(
        state.unmoved_handles,
        vec!["back".to_string()],
        "a partial move names the handles that did not"
    );
}

// ── The prepared instance already loaded is the one a reverse uses ─────────

#[test]
fn moving_back_installs_the_retained_instance_without_preparing_again() {
    // Going forward keeps the detector it replaced. Coming back is then a
    // pointer swap onto something already loaded, which is what keeps a
    // reverse command off the model-loading path entirely — the absence of a
    // build is the assertion, never how long the command took.
    let transitions = coordinator_with_one_camera();
    let recorder = PreparationRecorder::new();

    transitions.request_backend(ACCELERATED_DETECTION_BACKEND);
    transitions.drive_preparations(recorder.succeed());
    let builds_after_forward = recorder.call_count();
    assert_eq!(
        transitions.state().running_backend,
        ACCELERATED_DETECTION_BACKEND
    );
    assert!(
        transitions
            .retained_instance("front", CPU_DETECTION_BACKEND)
            .is_some(),
        "the replaced detector must be retained as the prepared way back"
    );

    transitions.request_backend(CPU_DETECTION_BACKEND);

    // The command records the move; it does not perform it. This coordinator
    // holds no detector, so a move it recorded on the command itself would say
    // the handle is on the new backend while the frames still go through the
    // old one — and would leave nothing outstanding while the value an
    // operator surface reads as running has not moved, which reads as a gap
    // with nothing closing it and sends them to restart a node that is already
    // closing it.
    assert_eq!(
        transitions.state().running_backend,
        ACCELERATED_DETECTION_BACKEND,
        "until the swap happens the node names the backend its handle is genuinely on"
    );
    assert!(
        transitions.state().pending.is_some(),
        "and the move onto the loaded instance is outstanding, so the surface has a transition \
         to name rather than a gap it must explain some other way"
    );
    assert_eq!(
        transitions.scheduled_preparations().len(),
        1,
        "a reverse onto a retained instance is scheduled like any other move — what makes it \
         cheap is that the pass putting it back loads nothing"
    );
    assert_eq!(
        recorder.call_count(),
        builds_after_forward,
        "a reverse onto a retained instance must load no model"
    );
    assert!(
        transitions
            .retained_instance("front", ACCELERATED_DETECTION_BACKEND)
            .is_some(),
        "the instance just stepped away from is retained as the other prepared choice"
    );

    // And the move lands: the handle takes the instance it already had back,
    // the node names the backend it is now on, and both instances stay loaded
    // so the next move either way is a pointer swap again.
    transitions.drive_preparations(recorder.succeed());
    assert_eq!(
        transitions.state().running_backend,
        CPU_DETECTION_BACKEND,
        "once the move is taken on the node names the backend it is running"
    );
    assert!(transitions.state().pending.is_none());
    assert!(
        transitions
            .retained_instance("front", ACCELERATED_DETECTION_BACKEND)
            .is_some(),
        "and the accelerated instance stays the prepared way back"
    );
}

// ── Distributed work reuses a camera's detector ────────────────────────────

#[test]
fn distributed_work_reuses_a_camera_detector_and_only_a_camera_less_node_owns_its_own() {
    let with_camera = DetectionTransitions::with_clock(CPU_DETECTION_BACKEND, fixed_clock());
    with_camera.register_handle("front", HandleKind::Camera, MODEL, &CLASSES);
    with_camera.register_handle("worker", HandleKind::DistributedWork, MODEL, &CLASSES);
    assert_eq!(
        with_camera.registered_handles().len(),
        1,
        "distributed work on a node that already watches a camera must reuse that camera's \
         detector rather than owning a second one"
    );

    let camera_less = DetectionTransitions::with_clock(CPU_DETECTION_BACKEND, fixed_clock());
    camera_less.register_handle("worker", HandleKind::DistributedWork, MODEL, &CLASSES);
    assert_eq!(
        camera_less.registered_handles().len(),
        1,
        "a node with no camera owns exactly one dedicated detector for distributed work"
    );

    let recorder = PreparationRecorder::new();
    with_camera.request_backend(ACCELERATED_DETECTION_BACKEND);
    with_camera.drive_preparations(recorder.succeed());
    assert_eq!(
        recorder.call_count(),
        1,
        "a reused detector is prepared once, not once per consumer"
    );
}

// ── The coordinator owns runtime facts, never the operator's pin ───────────

#[test]
fn the_coordinator_never_authors_the_operators_stored_choice() {
    // The store owns what the operator asked for; the coordinator owns what
    // is actually running. A coordinator that wrote the pin would make its own
    // runtime outcome look like an operator decision, and the operator could
    // never take it back.
    let transitions = coordinator_with_one_camera();
    let recorder = PreparationRecorder::new();

    vigil::settings_application::publish_pinned_backend(
        vigil::settings_backends::DETECTION_BACKEND_SETTING,
        Some(ACCELERATED_DETECTION_BACKEND.to_string()),
    );

    transitions.request_backend(ACCELERATED_DETECTION_BACKEND);
    transitions.drive_preparations(recorder.fail());

    assert_eq!(
        vigil::settings_application::pinned_backend(
            vigil::settings_backends::DETECTION_BACKEND_SETTING
        )
        .as_deref(),
        Some(ACCELERATED_DETECTION_BACKEND),
        "a failed preparation must leave the operator's stored choice exactly as they left it"
    );
    assert_eq!(
        transitions.state().running_backend,
        CPU_DETECTION_BACKEND,
        "and the running backend stays what the detectors are actually running"
    );
}
