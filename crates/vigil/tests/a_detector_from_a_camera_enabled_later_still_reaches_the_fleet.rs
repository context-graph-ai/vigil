//! "This node has no detector" is what is true right now, not a sentence the
//! node is held to for the rest of its life.
//!
//! A node answers its worker question once every camera that runs has failed
//! to produce a detector, and the operator reads that back as
//! `fabric-worker-serving=false` with the real reason beside it. Then they act
//! on it — they stage a model, fix an endpoint, and switch a camera on through
//! the settings surface (`site_channel.rs`'s `set_camera_enabled`). That
//! camera's thread wakes from its enable-flag park, loads a detector, and
//! hands it to the node.
//!
//! The node must serve the fleet with it. Today it does not: the answer
//! already standing is kept and the detector is discarded (`fabric.rs`'s
//! `answer` returns the moment an answer has been given, and the start that
//! brings the worker loop up has already run on the old one). The deployment
//! holds a working detector, the operator did exactly what they were told to
//! do, and every surface they can reach still says the node is out of
//! detectors — for the rest of the process's life, with a restart as the only
//! way out.
//!
//! The other half of the same promise — a camera switched on later must not
//! COST the node a detector another camera is still loading — is pinned in
//! `a_camera_enabled_after_bring_up_is_a_detector_source_this_node_counts.rs`.
//!
//! The operator's stop is the one answer nothing supersedes, and the second
//! leg holds that line: a node on its way down must not bring a worker loop up
//! and start claiming fleet jobs it will abandon, however well a camera's
//! detector loaded during teardown.
//!
//! Nothing here sleeps, reads a clock, spawns a process, or asserts on elapsed
//! time: every leg is a deterministic drive of the node's own answer in a
//! stated order, followed by a read of what the start was told.

#![cfg(feature = "fabric")]

use std::sync::{Arc, Mutex};

use vigil::fabric::worker_slot_door::{SettledAnswer, WorkerSlotDoor};

/// Every answer the start the fabric bring-up installs was handed, in order,
/// recorded so a leg reads EVENTS rather than waiting for anything.
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
/// producing a detector.
fn stopped_watching(camera: &str) -> String {
    format!(
        "camera-stopped-watching — {camera} stopped watching before it loaded a detector; check \
         that camera's analysis endpoint and its credentials"
    )
}

/// The reason a camera's failed detector load carries: the real error an
/// operator has to act on.
fn load_failure(camera: &str) -> String {
    format!(
        "detector-model-would-not-load — stage a loadable detection model on this node \
         (detector_model_path / VIGIL_DETECTOR_MODEL_PATH); {camera}'s model failed to load"
    )
}

#[test]
fn a_detector_from_a_camera_switched_on_later_serves_a_node_that_reported_none() {
    // One camera ran when this deployment came up and its model would not
    // load, so the node is out of detectors and says so.
    let node = WorkerSlotDoor::new();
    node.expect_detector_sources(1);
    let (seen, handed) = recorder();
    node.on_answer(move |answer| handed.lock().expect("record").push(answer));

    let counted = node.camera_source(&stopped_watching("lower gate"));
    counted.failed(&load_failure("lower gate"));

    // ARMOR, the milestone that makes the assertion below mean something: the
    // node really did tell the operator it has no detector. Without this the
    // final assertion is satisfied by a node that simply never answered the
    // first time, which is the opposite defect and leaves the operator on
    // `worker-loop-starting` forever.
    assert_eq!(
        answers(&seen),
        vec![SettledAnswer::NoDetector {
            reason: load_failure("lower gate")
        }],
        "the one camera that ran has failed, so the node is out of detectors and must say so \
         with the real error the operator has to act on"
    );

    // The operator stages a model and switches a second camera on. Its thread
    // wakes from the enable-flag park, takes up its standing as a detector
    // source, and this time the model loads.
    let switched_on_later = node.camera_source(&stopped_watching("orchard"));
    switched_on_later.detector_ready("burn-cpu");

    assert_eq!(
        answers(&seen),
        vec![
            SettledAnswer::NoDetector {
                reason: load_failure("lower gate")
            },
            SettledAnswer::Ready {
                backend_tag: "burn-cpu".to_string()
            },
        ],
        "the node has a detector now, so it serves the fleet now: \"no detector is coming\" was \
         true when every camera that ran had failed, and a camera the operator switched on \
         afterwards is a camera that runs. A node that keeps the old answer holds a working \
         detector idle and tells the operator — who did exactly what the reason told them to do \
         — that it has none, until somebody restarts the deployment"
    );
}

#[test]
fn an_operator_stop_is_still_the_last_word_after_a_node_reported_no_detector() {
    // The over-correction this must not buy: letting a later answer replace a
    // standing one is only right for a DETECTOR. The operator's stop is the
    // node's last word, and a detector that lands during teardown must not
    // bring a worker loop up on a node that is going down — it would claim
    // fleet jobs it is about to abandon.
    let node = WorkerSlotDoor::new();
    node.expect_detector_sources(1);
    let (seen, handed) = recorder();
    node.on_answer(move |answer| handed.lock().expect("record").push(answer));

    let counted = node.camera_source(&stopped_watching("lower gate"));
    counted.failed(&load_failure("lower gate"));
    node.cancel();

    let switched_on_later = node.camera_source(&stopped_watching("orchard"));
    switched_on_later.detector_ready("burn-cpu");

    assert_eq!(
        answers(&seen),
        vec![
            SettledAnswer::NoDetector {
                reason: load_failure("lower gate")
            },
            SettledAnswer::Cancelled,
        ],
        "the operator stopped this node, so nothing after that starts a worker loop on it: a \
         detector that loaded during teardown is not a reason to claim fleet work the node is \
         about to walk away from"
    );
}
