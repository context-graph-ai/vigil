//! A camera an operator switches on while the deployment is up is one of this
//! node's detector sources, and the node's worker answer counts it.
//!
//! The promise is that a node serves the fleet if ANY camera that really runs
//! loads a detector, and is out of detectors only when EVERY camera that
//! really runs has failed. "Really runs" includes a camera an operator enables
//! at RUNTIME: the operator opens the settings surface, switches the camera
//! back on, and that camera's thread — parked on its enable flag since startup
//! — wakes up and goes looking for a detector like any other
//! (`site_channel.rs`'s `set_camera_enabled` flips the flag the parked thread
//! polls; `runtime.rs`'s camera thread takes its standing as a detector source
//! the moment it is past the park).
//!
//! The count the node measures "every" against is not that set. It is fixed
//! ONCE, before any camera thread exists, from the cameras that were switched
//! on at startup (`runtime.rs`'s `eligible_detector_sources`, read off the
//! startup disable markers). A camera enabled afterwards takes a standing the
//! node never counted, and the arithmetic that decides "every source is out"
//! is wrong in the operator's face:
//!
//! - its failure is one report against a smaller count, so it can complete a
//!   count the cameras that were counted have not answered yet — the node says
//!   it has no detector while a counted camera is still loading one, and the
//!   detector that then arrives is discarded against the standing answer;
//! - and once the node has said it has no detector, the camera the operator
//!   just switched on cannot take that back, however well it loads.
//!
//! Either way the operator did the one thing the surface offered them — switch
//! the camera on — and the node is worse off for it, permanently, with no
//! surface saying why.
//!
//! These are ORDERING contracts about one node's single answer, which is why
//! they are driven directly against that answer rather than from a process
//! surface: a camera thread's failure and another camera thread's cold model
//! load cannot be made to land in a chosen order from outside the process. The
//! sibling file
//! (`a_detector_from_a_camera_enabled_later_still_reaches_the_fleet.rs`) holds
//! the half where the late camera's detector must supersede an answer the node
//! has already given.
//!
//! Nothing here sleeps, reads a clock, spawns a process, or asserts on elapsed
//! time: every leg is a deterministic drive of the node's own answer in a
//! stated order, followed by a read of what the start was told.

#![cfg(feature = "fabric")]

use std::sync::{Arc, Mutex};

use vigil::fabric::worker_slot_door::{SettledAnswer, WorkerSlotDoor};

/// What the start the fabric bring-up installs was told, recorded so a leg
/// reads an EVENT rather than waiting for anything.
type Recorded = Arc<Mutex<Vec<SettledAnswer>>>;

fn recorder() -> (Recorded, Recorded) {
    let seen: Recorded = Arc::new(Mutex::new(Vec::new()));
    let handed = Arc::clone(&seen);
    (seen, handed)
}

fn answers(seen: &Recorded) -> Vec<SettledAnswer> {
    seen.lock().expect("recorded worker answers").clone()
}

/// What an operator is shown when a camera thread stops watching without ever
/// producing a detector: the thing that is wrong, named per camera, never a
/// restatement of the outcome.
fn stopped_watching(camera: &str) -> String {
    format!(
        "camera-stopped-watching — {camera} stopped watching before it loaded a detector; check \
         that camera's analysis endpoint and its credentials"
    )
}

/// The reason a camera's failed detector load carries: the real error an
/// operator has to act on, named per camera so the surface can print one.
fn load_failure(camera: &str) -> String {
    format!(
        "detector-model-would-not-load — stage a loadable detection model on this node \
         (detector_model_path / VIGIL_DETECTOR_MODEL_PATH); {camera}'s model failed to load"
    )
}

#[test]
fn a_camera_switched_on_later_that_fails_does_not_cost_the_node_a_detector_still_loading() {
    // One camera was switched on when this deployment came up, so the node
    // counted one detector source. Its thread is running and its model is a
    // cold load: it has not answered yet.
    let node = WorkerSlotDoor::new();
    node.expect_detector_sources(1);
    let (seen, handed) = recorder();
    node.on_settled(move |answer| handed.lock().expect("record").push(answer));
    let counted = node.camera_source(&stopped_watching("lower gate"));

    // ARMOR, the first milestone: the node is open with its counted camera
    // outstanding. A leg that reached the assertion below with the question
    // already answered would be asserting nothing about the late camera.
    assert!(
        answers(&seen).is_empty(),
        "the node's one counted camera is still loading a detector, so its worker question is \
         open: nothing has been settled and nothing may be"
    );

    // The operator switches a second camera on. Its thread wakes from the
    // enable-flag park and takes up its standing as a detector source — one
    // the bring-up's count never included — and then its endpoint turns out to
    // be unreachable, so it stops watching without a detector.
    let switched_on_later = node.camera_source(&stopped_watching("orchard"));
    switched_on_later.failed(&load_failure("orchard"));

    assert!(
        answers(&seen).is_empty(),
        "a camera the operator switched on while the deployment was up is an EXTRA source of a \
         detector for this node, never a substitute for the camera that was already running: its \
         failure cannot answer for a counted camera that is still loading a detector. Answering \
         here declares the node out of detectors while one is being loaded — and the operator \
         caused it by doing the only thing the settings surface offered them"
    );

    // The counted camera finishes its load, which is the answer the node was
    // waiting for all along.
    counted.detector_ready("burn-cpu");

    assert_eq!(
        answers(&seen),
        vec![SettledAnswer::Ready {
            backend_tag: "burn-cpu".to_string()
        }],
        "the camera that was running all along loaded a detector, so this node serves the fleet: \
         a node that had already declared itself out of detectors discards this and serves \
         nothing for the rest of the process's life"
    );
}

#[test]
fn a_camera_switched_on_later_is_a_source_the_node_waits_for_before_it_is_out() {
    // ARMOR for the leg above, and the count contract in its own right: the
    // late camera's report is not merely "not the whole answer", it is a real
    // report this node's arithmetic needs. If the standing it takes were
    // silently discarded — a source that never counted and never reported —
    // this leg's final answer would never arrive at all, and the leg above
    // would be passing on a fixture that established nothing.
    let node = WorkerSlotDoor::new();
    node.expect_detector_sources(1);
    let (seen, handed) = recorder();
    node.on_settled(move |answer| handed.lock().expect("record").push(answer));

    let counted = node.camera_source(&stopped_watching("lower gate"));
    let switched_on_later = node.camera_source(&stopped_watching("orchard"));

    switched_on_later.failed(&load_failure("orchard"));
    assert!(
        answers(&seen).is_empty(),
        "one of this node's two running cameras is out; the other is still loading and the node \
         may yet serve"
    );

    counted.failed(&load_failure("lower gate"));

    let settled = answers(&seen);
    assert_eq!(
        settled.len(),
        1,
        "one node, one worker question, ONE answer — never one per camera: {settled:?}"
    );
    match &settled[0] {
        SettledAnswer::NoDetector { reason } => {
            // The reason carried is the FIRST failure reported, which here is
            // the camera the operator switched on later. That it is that
            // camera's real error is what proves its report reached this
            // node's arithmetic rather than being dropped on the floor.
            assert!(
                reason.contains("orchard"),
                "every camera that runs on this node has now failed, and the reason an operator \
                 reads back is the first real failure reported — the camera they switched on. A \
                 reason naming only the counted camera means the late camera's ending was never \
                 counted at all: got {reason:?}"
            );
            assert!(
                !reason.contains("worker-loop-starting"),
                "the settled reason must never be the placeholder the bring-up seeds before the \
                 question is answered: got {reason:?}"
            );
        }
        other => panic!(
            "both cameras that run on this node have stopped without a detector, so the node is \
             out of detectors and must say so with a reason: got {other:?}"
        ),
    }
}

#[test]
fn a_node_whose_cameras_are_exactly_the_ones_it_counted_still_answers_when_they_are_out() {
    // The regression this must not buy. Making the node wait for sources it
    // never counted is one over-correction away from making it wait forever:
    // a node whose running cameras are exactly the ones the bring-up counted
    // answers as soon as the last of them is out, with a real reason, and no
    // extra source is invented to keep the question open.
    let node = WorkerSlotDoor::new();
    node.expect_detector_sources(2);
    let (seen, handed) = recorder();
    node.on_settled(move |answer| handed.lock().expect("record").push(answer));

    let lower_gate = node.camera_source(&stopped_watching("lower gate"));
    let side_path = node.camera_source(&stopped_watching("side path"));

    lower_gate.failed(&load_failure("lower gate"));
    assert!(
        answers(&seen).is_empty(),
        "one of this node's two cameras is out and the other is still loading: the node has not \
         answered yet"
    );

    side_path.failed(&load_failure("side path"));

    let settled = answers(&seen);
    assert_eq!(
        settled.len(),
        1,
        "one node, one worker question, ONE answer: {settled:?}"
    );
    match &settled[0] {
        SettledAnswer::NoDetector { reason } => assert!(
            reason.contains("lower gate"),
            "the reason an operator reads back is the first real failure reported: got {reason:?}"
        ),
        other => panic!(
            "every camera this node counted has failed, so it is out of detectors and must say \
             so rather than keeping the operator on `worker-loop-starting` for the life of the \
             process: got {other:?}"
        ),
    }
}
