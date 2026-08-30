//! A camera thread that ends without producing a detector has ANSWERED for
//! this node, however it ended.
//!
//! One node, one worker answer. The bring-up declares how many camera detector
//! sources will really run, and the node is out of detectors only when every
//! one of them has reported. That arithmetic only completes if every source
//! that stops being one says so — and a camera thread has more ways to stop
//! than the one the runtime currently reports.
//!
//! `runtime.rs`'s camera thread reports exactly one ending: the detector model
//! would not load. It ends silently when its analysis endpoint cannot be
//! resolved into a session credential, and it ends silently when it PANICS —
//! the whole body runs inside a `catch_unwind` that latches ingest-failed,
//! logs, and returns. Either silent ending leaves the node one report short of
//! its own count forever. Nothing settles, the `worker-loop-starting`
//! placeholder stands for the life of the process, and the operator asking why
//! their node is not serving the fleet is told it is still coming up.
//!
//! The panic ending is the one that can only be pinned here. The endpoint
//! ending is reachable through the deployment's own configuration and is
//! pinned on the real binary
//! (`vigil-bin/tests/a_camera_thread_that_ends_early_is_still_accounted_for_by_its_node.rs`);
//! a panic inside a camera thread is not something an operator's configuration
//! can ask for, so it is driven directly against the node's own answer.
//!
//! What is asserted is the PROMISE — every camera thread that stops being a
//! source of a detector is accounted for, and a node whose sources have all
//! stopped says so with something real — never the mechanism. The seam these
//! legs are written against expresses that promise as a standing that is
//! RELEASED however the thread ends, which is the only shape an unwind can
//! satisfy, but nothing below asserts on how the release happens.
//!
//! Nothing here sleeps, reads a clock, spawns a process, or asserts on elapsed
//! time: every leg is a deterministic drive of the node's own answer in a
//! stated order, followed by a read of what the start was told.

#![cfg(feature = "fabric")]

use std::panic::AssertUnwindSafe;
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
    format!("camera-stopped-watching — {camera} stopped watching before it loaded a detector")
}

/// The reason a camera's failed detector load carries.
fn load_failure(camera: &str) -> String {
    format!(
        "detector-model-would-not-load — stage a loadable detection model on this node \
         (detector_model_path / VIGIL_DETECTOR_MODEL_PATH); {camera}'s model failed to load"
    )
}

#[test]
fn a_camera_thread_that_panics_before_it_answers_still_answers_for_its_node() {
    // Two cameras really run on this node.
    let node = WorkerSlotDoor::new();
    node.expect_detector_sources(2);
    let (seen, handed) = recorder();
    node.on_settled(move |answer| handed.lock().expect("record").push(answer));

    // One camera thread dies mid-flight, before it ever reaches a detector.
    // The runtime catches this unwind and keeps the process alive, which is
    // right — one camera must not take the node down — but the node is now
    // permanently one source short unless the ending is accounted for.
    let died = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _watching = node.camera_source(&stopped_watching("lower gate"));
        panic!("this camera thread died the way a real one dies: unexpectedly");
    }));
    assert!(
        died.is_err(),
        "sanity: this leg is about a camera thread that really did panic"
    );

    // Nothing has settled yet, and nothing should have: the other camera is
    // still loading, and a node with a detector on the way is not out of
    // detectors.
    assert!(
        answers(&seen).is_empty(),
        "one camera's ending is one camera's ending — the node is not out of detectors while \
         another camera is still loading one: {:?}",
        answers(&seen)
    );

    // The other camera's model will not load either. Every source this node
    // declared has now stopped being one.
    node.camera_source(&load_failure("side path"))
        .failed(&load_failure("side path"));

    let settled = answers(&seen);
    assert_eq!(
        settled.len(),
        1,
        "one node, one worker question, ONE answer: {settled:?}"
    );
    match &settled[0] {
        SettledAnswer::NoDetector { reason } => assert!(
            *reason == stopped_watching("lower gate") || *reason == load_failure("side path"),
            "the node's answer must carry a REAL reason one of its cameras reported, so the \
             operator has somewhere to go — not a placeholder and not a restatement of the \
             outcome: {reason}"
        ),
        other => panic!(
            "no camera on this node produced a detector and both have stopped trying, so the \
             node is out of detectors and must say so. A camera thread that panicked is still a \
             source that will never produce a detector: a node that keeps counting it can never \
             reach its own count, and the operator reads `worker-loop-starting` for the rest of \
             the process's life. Got: {other:?}"
        ),
    }
}

#[test]
fn a_camera_thread_that_ends_without_a_detector_answers_with_the_reason_it_ended_on() {
    // This node's one camera stops watching before it reaches a detector — the
    // ending an unresolvable analysis endpoint gives it, and the ending a
    // panic gives it, are the same ending as far as the fleet is concerned.
    let node = WorkerSlotDoor::new();
    node.expect_detector_sources(1);
    let (seen, handed) = recorder();
    node.on_settled(move |answer| handed.lock().expect("record").push(answer));

    drop(node.camera_source(&stopped_watching("lower gate")));

    assert_eq!(
        answers(&seen),
        vec![SettledAnswer::NoDetector {
            reason: stopped_watching("lower gate"),
        }],
        "the reason an operator is shown is the one the camera stopped on, carried through to \
         the node's answer — a node that settles with nothing to act on, or does not settle at \
         all, costs the operator the same thing: {:?}",
        answers(&seen)
    );
}

#[test]
fn a_camera_that_already_answered_is_not_counted_a_second_time_when_its_thread_ends() {
    // The correction must not be bought by double-counting. A camera that has
    // already reported its failure and then ends is ONE source that failed,
    // not two — and a node that counted it twice would declare itself out of
    // detectors while another camera was still loading one, which is exactly
    // the loss this whole contract exists to prevent.
    let node = WorkerSlotDoor::new();
    node.expect_detector_sources(2);
    let (seen, handed) = recorder();
    node.on_settled(move |answer| handed.lock().expect("record").push(answer));

    node.camera_source(&stopped_watching("lower gate"))
        .failed(&load_failure("lower gate"));

    assert!(
        answers(&seen).is_empty(),
        "one of two cameras has failed and the other is still loading: the node is not out of \
         detectors and must not say it is: {:?}",
        answers(&seen)
    );

    node.camera_source(&stopped_watching("side path"))
        .detector_ready("burn-cpu");

    assert_eq!(
        answers(&seen),
        vec![SettledAnswer::Ready {
            backend_tag: "burn-cpu".to_string(),
        }],
        "the node serves the fleet with the detector it has: {:?}",
        answers(&seen)
    );
}
