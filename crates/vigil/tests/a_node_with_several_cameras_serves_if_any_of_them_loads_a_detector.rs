//! One node, several cameras, ONE worker answer — and the answer belongs to
//! the node, not to whichever camera spoke first.
//!
//! A node serves the fleet from a single detector: the worker-detector slot
//! holds this node's one answer, and whatever produces a detector settles it.
//! With one camera that is the same thing as the camera's own answer. With
//! several it is not, and today the difference costs the operator a node that
//! serves nothing.
//!
//! What happens today: the node-wide slot is cloned into every camera producer
//! (`runtime.rs`), and the first non-cancellation answer settles it
//! (`fabric.rs`'s `answer`). A camera whose model will not load reports that
//! failure the moment it happens — earliest, because a failed load is fast and
//! a successful one is a cold model load — so the node's answer becomes "no
//! detector is coming" while a second camera is still loading one. When that
//! camera's detector arrives it is discarded: the slot is already settled, the
//! worker loop never starts, and `fabric-worker-serving=false` stands with the
//! FIRST camera's load error beside it for the rest of the process's life. The
//! operator has a node with a working detector, sitting idle, telling them it
//! has none.
//!
//! What the node owes them instead: it serves the fleet if ANY camera that
//! really runs loads a detector, whatever order the answers arrive in, and it
//! is out of detectors only when EVERY camera that really runs has failed.
//! "Really runs" is the other half — a camera an operator switched off at
//! startup parks on its enable flag and never reaches a detector load at all
//! (`runtime.rs`'s startup disable marker), so it is not a source of an answer:
//! counting one would leave the node waiting forever on a camera that will
//! never speak, and the node would keep the `worker-loop-starting` placeholder
//! for the rest of its life.
//!
//! These are ORDERING contracts, which is why they are pinned here rather than
//! from the process surface. A camera thread's failure and another camera
//! thread's cold model load cannot be made to land in a chosen order from
//! outside the process, and a fixture that tried would be asserting on which
//! of two threads the scheduler ran first. The operator-facing halves that CAN
//! be reached from outside are pinned on the real binary
//! (`vigil-bin/tests/a_multi_camera_node_answers_its_worker_question_once.rs`).
//!
//! Nothing here sleeps, reads a clock, or asserts on elapsed time: every leg
//! is a deterministic drive of the node's own answer in a stated order,
//! followed by a read of what the start was told.

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

/// The reason a camera's failed detector load carries: the real error an
/// operator has to act on, named per camera so the surface can print one.
fn load_failure(camera: &str) -> String {
    format!(
        "detector-model-would-not-load — stage a loadable detection model on this node \
         (detector_model_path / VIGIL_DETECTOR_MODEL_PATH); {camera}'s model failed to load"
    )
}

#[test]
fn one_cameras_failed_load_does_not_discard_the_detector_another_camera_is_still_loading() {
    // Two cameras really run on this node.
    let node = WorkerSlotDoor::new();
    node.expect_detector_sources(2);
    let (seen, handed) = recorder();
    node.on_settled(move |answer| handed.lock().expect("record").push(answer));

    // The failure lands first, which is the ordinary case rather than the
    // unlucky one: a model that will not load fails fast, and the camera that
    // succeeds is doing a cold model load.
    node.detector_source_failed(&load_failure("lower gate"));
    assert!(
        answers(&seen).is_empty(),
        "one camera's failed load is ONE CAMERA's answer, not the node's: another camera is \
         still loading a detector and the node does not yet know whether it can serve"
    );

    node.detector_ready("burn-cpu");
    assert_eq!(
        answers(&seen),
        vec![SettledAnswer::Ready {
            backend_tag: "burn-cpu".to_string()
        }],
        "the node has a detector, so it SERVES the fleet — a camera whose model would not load \
         must not cost the operator a node that is holding a perfectly good detector and \
         reporting it has none"
    );
}

#[test]
fn a_camera_that_fails_after_the_node_is_already_serving_does_not_stop_it_serving() {
    // The same window from the other side, and the one that already worked:
    // the detector arrives first and the failure follows it.
    let node = WorkerSlotDoor::new();
    node.expect_detector_sources(2);
    let (seen, handed) = recorder();
    node.on_settled(move |answer| handed.lock().expect("record").push(answer));

    node.detector_ready("burn-cpu");
    node.detector_source_failed(&load_failure("side path"));

    assert_eq!(
        answers(&seen),
        vec![SettledAnswer::Ready {
            backend_tag: "burn-cpu".to_string()
        }],
        "a node serving the fleet from one camera's detector keeps serving when a second \
         camera's model will not load: the fleet is served by the node's ONE detector, and the \
         camera that failed is honestly ingest-failed on its own surface"
    );
}

#[test]
fn a_node_is_out_of_detectors_only_when_every_camera_that_runs_has_failed() {
    let node = WorkerSlotDoor::new();
    node.expect_detector_sources(3);
    let (seen, handed) = recorder();
    node.on_settled(move |answer| handed.lock().expect("record").push(answer));

    node.detector_source_failed(&load_failure("lower gate"));
    node.detector_source_failed(&load_failure("side path"));
    assert!(
        answers(&seen).is_empty(),
        "two of this node's three running cameras are out; the third is still loading and the \
         node may yet serve"
    );

    node.detector_source_failed(&load_failure("orchard"));

    let settled = answers(&seen);
    assert_eq!(
        settled.len(),
        1,
        "one node, one worker question, ONE answer — never one per camera: {settled:?}"
    );
    match &settled[0] {
        SettledAnswer::NoDetector { reason } => {
            // ONE reason is chosen, because the operator surface prints a
            // single `reason=`. Which camera's it is is not the contract; that
            // it is a REAL camera's load error, and not a restatement of the
            // outcome or the starting placeholder, is.
            assert!(
                ["lower gate", "side path", "orchard"]
                    .iter()
                    .any(|camera| reason.contains(camera)),
                "the settled reason must be a real camera's load error, which is what the \
                 operator acts on: got {reason:?}"
            );
            assert!(
                !reason.contains("worker-loop-starting"),
                "the settled reason must never be the placeholder the bring-up seeds before the \
                 question is answered: got {reason:?}"
            );
        }
        other => panic!(
            "every camera that runs on this node has failed to load a detector, so the node is \
             out of detectors and must say so with a reason: got {other:?}"
        ),
    }
}

#[test]
fn an_operator_stop_still_wins_over_a_detector_that_landed_during_teardown() {
    // Teardown's real order on a multi-camera node, which no process-level
    // fixture can construct: one camera's detector answers the slot, the
    // operator stops the node, and only THEN does the bring-up install the
    // start. The answer sitting there has not run, so the stop is still the
    // node's last word — otherwise a node on its way down brings a worker loop
    // up and starts claiming fleet jobs it will abandon.
    let node = WorkerSlotDoor::new();
    node.expect_detector_sources(2);
    let (seen, handed) = recorder();

    node.detector_ready("burn-cpu");
    node.cancel();
    node.on_settled(move |answer| handed.lock().expect("record").push(answer));

    assert_eq!(
        answers(&seen),
        vec![SettledAnswer::Cancelled],
        "the operator's stop arrived before anything ran on the detector's answer, so the start \
         must be told the node is going down — not handed a detector to serve the fleet with"
    );
}

#[test]
fn a_camera_switched_off_at_startup_is_not_a_detector_source_this_node_waits_on() {
    // Three cameras are configured and one of them is switched off at
    // startup. Its thread parks on the enable flag and never reaches a
    // detector load, so it is not a source of an answer and this node runs
    // TWO. Counting it would leave the node waiting on a camera that will
    // never speak.
    let node = WorkerSlotDoor::new();
    node.expect_detector_sources(2);
    let (seen, handed) = recorder();
    node.on_settled(move |answer| handed.lock().expect("record").push(answer));

    node.detector_source_failed(&load_failure("lower gate"));
    node.detector_source_failed(&load_failure("side path"));

    let settled = answers(&seen);
    assert_eq!(
        settled.len(),
        1,
        "every camera that really runs on this node has failed, so the node's question is \
         ANSWERED: a switched-off camera must not hold it open — an operator whose node will \
         never serve is otherwise told on every surface they can reach that it is still coming \
         up, for the rest of the process's life. Got: {settled:?}"
    );
    assert!(
        matches!(settled[0], SettledAnswer::NoDetector { .. }),
        "the answer is that no detector is coming, with the reason a running camera failed \
         with: {settled:?}"
    );

    // And the other direction: the switched-off camera does not SETTLE the
    // question either. A node whose one running camera is still loading has
    // not answered anything yet.
    let still_loading = WorkerSlotDoor::new();
    still_loading.expect_detector_sources(1);
    let (unanswered, handed) = recorder();
    still_loading.on_settled(move |answer| handed.lock().expect("record").push(answer));
    assert!(
        answers(&unanswered).is_empty(),
        "a camera an operator switched off is absent from the question, not an answer to it: \
         nothing about it may settle a node whose running camera is still loading a detector"
    );
}
