//! A node with several cameras answers the fleet's question ONCE, and the
//! cameras that are switched off do not get a vote.
//!
//! Vigil answers "is this node serving the fleet?" from one place — the
//! worker-detector slot — and whatever produces a detector settles it: a
//! detector arrives, no detector is coming, or the operator stopped the node.
//! The settled answer is what starts the worker loop or prints the honest
//! `fabric_worker_loop_not_started=true reason=…` line an operator reads back
//! through `vigil doctor acceleration`.
//!
//! On a node with ONE camera that already holds
//! (`a_camera_nodes_failed_detector_load_settles_its_worker_slot.rs`). This
//! file is about the node an operator actually deploys: several cameras, some
//! of them switched off, one shared worker answer between them.
//!
//! Two things go wrong there today, and both cost the operator the same thing
//! — a node that will never serve the fleet, on every surface saying it is
//! still coming up, with nothing to act on:
//!
//! 1. A camera switched off at startup still counts as this node's detector
//!    source. `fabric.rs`'s bring-up asks only whether a camera is
//!    CONFIGURED with an analysis endpoint, so a node whose every camera is
//!    disabled skips the cameraless worker bootstrap that would have loaded a
//!    detector — while each disabled camera's own thread parks on its enable
//!    flag and never reaches a detector load. Nothing settles the slot. The
//!    node keeps the `worker-loop-starting` placeholder for the rest of its
//!    life.
//! 2. The first camera to answer settles the question for all of them. A load
//!    failure on one camera is one camera's answer, not the node's, and the
//!    node is only out of detectors when EVERY camera that will really run has
//!    failed. The second test below is the half of that a process-level
//!    fixture can reach: a camera that is switched off must neither settle the
//!    answer nor hold it up, because it is not a source of one. The ordered
//!    legs — a failure and a success racing each other in a chosen order —
//!    cannot be constructed from out here at all and are pinned directly on
//!    the slot.
//!
//! Nothing here waits on a clock or asserts on elapsed time. Each wait is for
//! a STATE the node publishes on its own output or through its own doctor
//! rendering; the bounds are the fixture's liveness guards, and the standing
//! owner ruling of 2026-08-14 forbids judging any product outcome by how long
//! it took.
//!
//! How the conditions are CONSTRUCTED, deterministically and without a
//! network: every camera is pointed at a syntactically valid RTSP endpoint
//! that is never contacted (the probe that would connect happens after the
//! detector loads, so nothing here needs a stream, and no port is allocated
//! for one), and `--detector-model-path` names a file that exists and is not a
//! model, so the production load path reads it and fails with a real error.
//! A camera is switched off the way an operator switches one off: the disable
//! marker this deployment writes under its own data directory.

#![cfg(feature = "fabric")]

use std::fs;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_until,
};

/// How long the fixture is willing to keep looking for the node's own verdict.
/// A liveness guard on the test, never a bound the product is asked to meet.
const VERDICT_WAIT: Duration = Duration::from_secs(120);

/// The two cameras this node is configured with, and the analysis endpoints
/// they are pointed at. Never contacted, and they do not need to be: each
/// camera thread loads its detector before it opens its stream, so the load is
/// what this node meets first and the endpoints only have to parse and differ
/// from one another. Port 1 is privileged, so no unprivileged process on this
/// machine can make either answer by accident.
const LOWER_GATE: (&str, &str) = ("lower gate", "rtsp://127.0.0.1:1/lower-gate");
const SIDE_PATH: (&str, &str) = ("side path", "rtsp://127.0.0.1:1/side-path");

/// The line the node prints when its worker question is answered and the
/// answer is "no detector".
const NOT_STARTED_LINE: &str = "fabric_worker_loop_not_started=true";

/// The line the node prints when the answer is a detector.
const STARTED_LINE: &str = "fabric_worker_loop_started=true";

/// What a camera thread prints when the model will not load. Read as proof the
/// condition under test was really established.
const LOAD_FAILED_LINE: &str = "detector model load failed";

/// What the node prints for each camera an operator has switched off. Read as
/// proof this run really is the disabled-camera journey.
const DISABLED_LINE: &str = "camera_disabled_at_startup camera=";

/// The placeholder the bring-up seeds before the worker question is answered.
/// An answer that never arrives leaves this standing forever, which is the
/// defect.
const STARTING_PLACEHOLDER: &str = "worker-loop-starting";

/// Where the fabric bring-up finishes attaching. Until this lands there is no
/// start installed on the slot at all, so an assertion made before it would be
/// about a node that had not yet asked the question.
const BRINGUP_DONE: &str = "boot_phase=fabric-bringup-done";

struct Node {
    child: Child,
    stdout: Arc<Mutex<String>>,
}

impl Node {
    fn logs(&self) -> String {
        self.stdout
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    /// Whether this node is still running, read from the operating system's
    /// own handle on the process: what is being asked is whether an
    /// unanswerable worker question ENDED the run, and a process that has
    /// exited cannot answer that about itself.
    fn still_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A file that exists at a model's pathname and is not a model. The production
/// load path opens it, reads it, and fails — the terminal answer a mis-staged
/// model gives an operator in the field.
fn unloadable_model(inside: &std::path::Path) -> std::path::PathBuf {
    let path = inside.join("not-a-detection-model.pth");
    fs::write(&path, b"this file is not a detector checkpoint").expect("stage an unloadable model");
    path
}

/// The slug this deployment names a camera by, which is what its disable
/// marker is named after.
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
    slug
}

/// Switch a camera off the way an operator does: the disable marker this
/// deployment writes under its own data directory and reads back at startup.
fn switch_off(data_dir: &std::path::Path, camera_name: &str) {
    let markers = data_dir.join("camera-disabled");
    fs::create_dir_all(&markers).expect("create this deployment's disable-marker directory");
    fs::write(markers.join(camera_slug(camera_name)), b"")
        .expect("switch this camera off at startup");
}

/// A fabric-serving node with the two cameras above configured, sharing one
/// detector model that will not load.
fn spawn_two_camera_node(data_dir: &std::path::Path, model_path: &std::path::Path) -> Node {
    let config_path = data_dir.join("vigil.toml");
    fs::write(
        &config_path,
        format!(
            "site_name = \"home farm\"\n\n\
             [[cameras]]\nname = \"{}\"\nrtsp_url = \"{}\"\n\n\
             [[cameras]]\nname = \"{}\"\nrtsp_url = \"{}\"\n",
            LOWER_GATE.0, LOWER_GATE.1, SIDE_PATH.0, SIDE_PATH.1
        ),
    )
    .expect("write the two-camera configuration");

    // Held until the moment of spawn, then released: the child binds these
    // ports itself and cannot inherit the listeners, so holding them up to
    // here is what stops another fixture in the same run from taking them in
    // between.
    let health = TcpPortReservation::reserve_loopback().expect("reserve a liveness port");
    let review = TcpPortReservation::reserve_loopback().expect("reserve a review port");
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .arg("--config")
        .arg(&config_path)
        .arg("--health-port")
        .arg(health.port().to_string())
        .arg("--review-port")
        .arg(review.port().to_string())
        .arg("--fabric-hub")
        .arg("true")
        // Acceleration off: the subject is the settled answer, not which
        // backend was selected to attempt a load.
        .arg("--accelerated-detection")
        .arg("false")
        .arg("--detector-model-path")
        .arg(model_path)
        .env("VIGIL_DATA_DIR", data_dir)
        .env_remove("VIGIL_RTSP_URL")
        .env_remove("VIGIL_DETECTOR_MODEL_PATH")
        .env_remove("VIGIL_FABRIC_TICKET")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    health.release();
    review.release();
    let mut child = command.spawn().expect("spawn vigil binary");
    let stdout = capture_pipe(child.stdout.take());
    let _stderr = capture_pipe(child.stderr.take());
    Node { child, stdout }
}

/// Run `vigil doctor acceleration` against this deployment and return what an
/// operator reads.
fn doctor(data_dir: &std::path::Path) -> String {
    let output = Command::new(vigil_binary_path())
        .arg("doctor")
        .arg("acceleration")
        .env("VIGIL_DATA_DIR", data_dir)
        .stdin(Stdio::null())
        .output()
        .expect("run vigil doctor acceleration");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Wait until this node has printed `marker`, so every assertion afterwards is
/// made about a run that really reached the condition being tested.
fn wait_for_line(node: &Node, marker: &str, what: &str) -> String {
    wait_until(what, VERDICT_WAIT, || {
        let logs = node.logs();
        Ok(logs.contains(marker).then_some(logs))
    })
    .unwrap_or_else(|error| {
        panic!(
            "{error}: this run never established the condition it exists to test. The node \
             said:\n{}",
            node.logs()
        )
    })
}

/// Wait for the node's own worker verdict, whichever way it went.
fn wait_for_the_worker_answer(node: &Node, when_it_never_comes: &str) -> String {
    wait_until(
        "the worker loop to say whether it started",
        VERDICT_WAIT,
        || {
            let logs = node.logs();
            let spoken = logs.contains(NOT_STARTED_LINE) || logs.contains(STARTED_LINE);
            Ok(spoken.then_some(logs))
        },
    )
    .unwrap_or_else(|error| {
        panic!(
            "{error}: {when_it_never_comes} The node said:\n{}",
            node.logs()
        )
    })
}

/// The settled `fabric-worker-serving` verdict an operator reads back, waited
/// for as a STATE rather than for a chosen interval: a verdict is settled when
/// it is no longer the in-flight placeholder.
fn wait_for_the_doctors_settled_verdict(node: &Node, data_dir: &std::path::Path) -> String {
    wait_until(
        "the doctor rendering to carry a settled fabric-worker-serving verdict",
        VERDICT_WAIT,
        || {
            let rendered = doctor(data_dir);
            let settled = rendered.contains("fabric-worker-serving=true")
                || (rendered.contains("fabric-worker-serving=false reason=")
                    && !rendered.contains(&format!("reason={STARTING_PLACEHOLDER}")));
            Ok(settled.then_some(rendered))
        },
    )
    .unwrap_or_else(|error| {
        panic!(
            "{error}: an operator asking their node why it is not serving the fleet must be told \
             something they can act on, not that it is still starting. The doctor \
             rendered:\n{}\nand the node said:\n{}",
            doctor(data_dir),
            node.logs()
        )
    })
}

#[test]
fn a_node_whose_every_camera_is_switched_off_still_answers_whether_it_serves_the_fleet() {
    let tmp = tempfile::tempdir().expect("data dir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create the deployment directory");
    let model_path = unloadable_model(tmp.path());
    switch_off(&data_dir, LOWER_GATE.0);
    switch_off(&data_dir, SIDE_PATH.0);
    let mut node = spawn_two_camera_node(&data_dir, &model_path);

    // Established first: this really is the every-camera-switched-off journey,
    // and the fabric bring-up really did install a start on the slot. An
    // assertion made before either would be about a different run.
    wait_for_line(
        &node,
        DISABLED_LINE,
        "a camera this deployment was told to switch off to be switched off",
    );
    wait_for_line(
        &node,
        BRINGUP_DONE,
        "this node's fabric bring-up to finish attaching",
    );

    // Not one of this node's cameras will produce a detector — every one of
    // them is parked on its enable flag and none will ever reach a load — so
    // the node is not waiting on anything. It has to say what it is: either it
    // found a detector some other way and serves, or it did not and names the
    // reason. What it must never do is keep the placeholder.
    let logs = wait_for_the_worker_answer(
        &node,
        "a node whose every camera is switched off has no camera that will ever produce a \
         detector, so it must ANSWER the fleet's question rather than sit on the starting \
         placeholder for the rest of its life — an operator whose node will never serve is told \
         on every surface they can reach that it is still coming up, with nothing to act on.",
    );

    if logs.contains(NOT_STARTED_LINE) {
        assert_eq!(
            logs.matches(NOT_STARTED_LINE).count(),
            1,
            "one node, one worker question, ONE answer — not one per camera:\n{logs}"
        );
    }

    let rendered = wait_for_the_doctors_settled_verdict(&node, &data_dir);
    assert!(
        !rendered.contains(&format!("reason={STARTING_PLACEHOLDER}")),
        "the verdict an operator reads back must be the settled one, never the placeholder the \
         bring-up seeds before the question is answered:\n{rendered}"
    );
    assert_eq!(
        rendered.matches("fabric-worker-serving=").count(),
        1,
        "the operator is shown one verdict for the node, not one per camera:\n{rendered}"
    );

    assert!(
        node.still_running(),
        "the runtime must still be RUNNING once the worker question has been answered: cameras \
         an operator switched off are a state this node reports and keeps reporting, never a \
         reason to end the run — an operator whose node has exited has nothing left to \
         ask:\n{logs}"
    );
}

#[test]
fn a_switched_off_camera_neither_settles_the_worker_answer_nor_holds_it_up() {
    let tmp = tempfile::tempdir().expect("data dir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create the deployment directory");
    let model_path = unloadable_model(tmp.path());
    // One camera switched off, one running. The running one is this node's
    // only real detector source, and its failure is therefore the node's
    // answer — reached without waiting on the camera that will never speak.
    switch_off(&data_dir, SIDE_PATH.0);
    let mut node = spawn_two_camera_node(&data_dir, &model_path);

    wait_for_line(
        &node,
        DISABLED_LINE,
        "the camera this deployment was told to switch off to be switched off",
    );
    wait_for_line(
        &node,
        LOAD_FAILED_LINE,
        "the running camera's detector load to fail",
    );

    let logs = wait_for_the_worker_answer(
        &node,
        "a camera an operator switched off is not a source of a detector, so it must not hold up \
         the answer the running camera already settled — a node left waiting on a camera that \
         will never speak keeps the starting placeholder for the rest of its life.",
    );

    assert!(
        !logs.contains(STARTED_LINE),
        "no camera on this node loaded a detector, so no worker loop may have started:\n{logs}"
    );
    assert_eq!(
        logs.matches(NOT_STARTED_LINE).count(),
        1,
        "the settled no-detector answer is given ONCE for the node, not once per camera and not \
         repeated by anything still watching:\n{logs}"
    );

    let rendered = wait_for_the_doctors_settled_verdict(&node, &data_dir);
    assert!(
        rendered.contains("fabric-worker-serving=false"),
        "a node whose only running camera could not load a detector is not serving the fleet and \
         must say so:\n{rendered}"
    );
    assert!(
        rendered.contains("detector_model_path")
            || rendered.contains("VIGIL_DETECTOR_MODEL_PATH")
            || rendered.to_ascii_lowercase().contains("model"),
        "the not-serving reason must be the REAL one the running camera failed with — the \
         detector model this node could not load — so the operator has somewhere to go, rather \
         than an answer about the camera they switched off themselves:\n{rendered}"
    );

    assert!(
        node.still_running(),
        "the runtime must still be RUNNING once the worker question has been answered:\n{logs}"
    );
}
