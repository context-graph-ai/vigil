//! A camera node whose detector cannot load must say so, once, and stop
//! looking like a node that is still starting.
//!
//! Vigil answers the question "is this node serving the fleet?" from ONE
//! place: the worker-detector slot. Whatever produces the detector settles it
//! — a detector arrives, no detector is coming, or the operator stopped the
//! node — and the settled answer is what starts the worker loop or prints the
//! honest `fabric_worker_loop_not_started=true reason=…` line an operator
//! reads back through `vigil doctor acceleration`.
//!
//! A CAMERALESS worker box already does that: its own bootstrap answers
//! `no_detector()` when the model will not load, the not-started line is
//! printed once, and the doctor renders `fabric-worker-serving=false
//! reason=no-model` with the staging fix named
//! (`cameraless_worker.rs`'s no-model arm).
//!
//! A node that owns a CAMERA does not. `runtime.rs`'s per-camera detector
//! thread turns a failed load into `None` and returns; nothing settles the
//! slot, so the node keeps the placeholder reason `worker-loop-starting` for
//! the rest of its life. What that costs the operator is the whole point:
//! their node is not serving the fleet, will never serve the fleet, and every
//! surface they can reach says it is still coming up — so there is nothing to
//! act on and no error to search for. The load error is printed once by the
//! camera thread and never reaches the not-serving reason at all.
//!
//! There is a second consequence in the same place, invisible from here and
//! stated so a reader of this file knows it is not being pinned: the settle
//! closure the bring-up installs captures the bundle, and the bundle holds the
//! slot, so a slot that is never settled never releases either — the closure
//! and the bundle keep each other alive for the process's lifetime.
//!
//! The pins below are the operator-facing half. Both drive the REAL `vigil`
//! binary, because the defect lived in process bring-up and no in-process
//! entry point reaches it. Both were authored RED against the defect described
//! above — the not-started line was never printed and the reason never settled.
//!
//! What they hold now the camera path settles its own slot: the answer arrives,
//! it arrives ONCE, it is not the seeded starting text, and the run does not
//! end because of it. That last one is why liveness is read from the operating
//! system's handle on the process and not from `/health`: a camera whose
//! detector will not load is honestly ingest-failed, and its liveness surface
//! is entitled to say so. What must never happen is the process going away —
//! an operator whose node has exited has nothing left to ask.
//!
//! Nothing here waits on a clock or asserts on elapsed time. Each wait is for
//! a STATE the node publishes on its own output or through its own doctor
//! rendering; the bounds are the fixture's liveness guards, and the standing
//! owner ruling of 2026-08-14 forbids judging any product outcome by how long
//! it took.
//!
//! How the condition is CONSTRUCTED, deterministically and without a network:
//! the camera is pointed at a syntactically valid RTSP endpoint that is never
//! contacted (the probe that would connect happens AFTER the detector loads,
//! so nothing here needs a stream, and no port is allocated for it), and
//! `--detector-model-path` names a file that exists and is not a model. The
//! production load path reads it and fails with a real error — the same
//! terminal answer a mis-staged model gives an operator in the field. The one
//! port this fixture does need, the liveness port, is held as a reservation
//! until the instant the node is spawned.

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

/// The analysis endpoint this camera is pointed at.
///
/// Never contacted, and it does not need to be: the per-camera thread loads its
/// detector BEFORE it opens the stream, so the load failure is what this node
/// meets first and the endpoint only has to parse. Port 1 is privileged, so no
/// unprivileged process on this machine can make it answer by accident — which
/// is exactly the property a never-contacted endpoint wants, and why no port is
/// allocated for it.
const UNREACHED_RTSP_ENDPOINT: &str = "rtsp://127.0.0.1:1/lower-gate";

/// The line the cameraless journey already prints when no detector is coming.
/// A camera node's failed load is the same answer and must arrive the same
/// way.
const NOT_STARTED_LINE: &str = "fabric_worker_loop_not_started=true";

/// What the camera thread prints when the model will not load. Read here as
/// the proof that the condition under test was really established — a run that
/// never reached the load would prove nothing.
const LOAD_FAILED_LINE: &str = "detector model load failed";

/// The placeholder the bring-up seeds before the worker question is answered.
/// An answer that never arrives leaves this standing forever, which is the
/// defect.
const STARTING_PLACEHOLDER: &str = "worker-loop-starting";

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

    /// Whether this node is still running. Read from the operating system's own
    /// handle on the process rather than from any status it reports about
    /// itself: what is being asked is whether a detector that will not load
    /// ENDED the run, and a process that has exited cannot answer that about
    /// itself.
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
/// load path opens it, reads it, and fails — which is the terminal answer this
/// node has to act on.
fn unloadable_model(inside: &std::path::Path) -> std::path::PathBuf {
    let path = inside.join("not-a-detection-model.pth");
    fs::write(&path, b"this file is not a detector checkpoint").expect("stage an unloadable model");
    path
}

/// A fabric-serving node that owns one camera and cannot load a detector.
///
/// The RTSP endpoint is never reached: the per-camera thread loads its
/// detector before it opens the stream, so the load failure is what this node
/// meets first and the endpoint only has to parse.
fn spawn_camera_node_with_an_unloadable_detector(
    data_dir: &std::path::Path,
    model_path: &std::path::Path,
) -> Node {
    // Held until the moment of spawn, then released: the child binds this port
    // itself and cannot inherit the listener, so holding it up to here is what
    // stops another fixture in the same run from taking it in between.
    let health = TcpPortReservation::reserve_loopback().expect("reserve a liveness port");
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .arg("--health-port")
        .arg(health.port().to_string())
        .arg("--fabric-hub")
        .arg("true")
        .arg("--camera-name")
        .arg("lower gate")
        .arg("--rtsp-url")
        .arg(UNREACHED_RTSP_ENDPOINT)
        // Acceleration off: the subject is the settled answer to a failed
        // load, not which backend was selected to attempt it.
        .arg("--accelerated-detection")
        .arg("false")
        .arg("--detector-model-path")
        .arg(model_path)
        .env("VIGIL_DATA_DIR", data_dir)
        .env_remove("VIGIL_RTSP_URL")
        .env_remove("VIGIL_DETECTOR_MODEL_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    health.release();
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

/// Wait until this node has printed the load failure, so every assertion below
/// is made about a run that really reached the condition.
fn wait_for_the_load_to_fail(node: &Node) -> String {
    wait_until("this node's detector load to fail", VERDICT_WAIT, || {
        let logs = node.logs();
        Ok(logs.contains(LOAD_FAILED_LINE).then_some(logs))
    })
    .unwrap_or_else(|error| {
        panic!(
            "{error}: this run never established the condition it exists to test — the camera \
             thread never reached its detector load. The node said:\n{}",
            node.logs()
        )
    })
}

#[test]
fn a_camera_node_whose_detector_will_not_load_settles_its_worker_answer_with_the_real_error() {
    let tmp = tempfile::tempdir().expect("data dir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create the deployment directory");
    let model_path = unloadable_model(tmp.path());
    let mut node = spawn_camera_node_with_an_unloadable_detector(&data_dir, &model_path);

    // Established first, so every assertion below is made about a run that
    // really reached the condition. Nothing here requires the worker answer to
    // arrive AFTER this observation: the settle happens on the load failure
    // itself, so the two can land in the same read of the node's output, and a
    // fixture that insisted on seeing them separately would be asserting on
    // the order two lines were polled in.
    wait_for_the_load_to_fail(&node);

    // A model that cannot be loaded is a SETTLED answer — no detector is
    // coming from this camera, and this node owns no other source of one — so
    // the not-started line arrives on that failure. The bound is the fixture's
    // liveness guard and nothing here judges the outcome by how long it took.
    let logs = wait_until(
        "the worker loop to say whether it started",
        VERDICT_WAIT,
        || {
            let logs = node.logs();
            let spoken =
                logs.contains(NOT_STARTED_LINE) || logs.contains("fabric_worker_loop_started=true");
            Ok(spoken.then_some(logs))
        },
    )
    .unwrap_or_else(|error| {
        panic!(
            "{error}: a camera node whose only detector failed to load must ANSWER the worker \
             question rather than wait forever on a detector that is never coming. The node \
             said:\n{}",
            node.logs()
        )
    });

    assert!(
        !logs.contains("fabric_worker_loop_started=true"),
        "a node with no loadable detector must never start a worker loop it cannot serve \
         from:\n{logs}"
    );
    assert_eq!(
        logs.matches(NOT_STARTED_LINE).count(),
        1,
        "the settled no-detector answer is given ONCE, not repeated by anything still \
         watching:\n{logs}"
    );

    let reason = logs
        .lines()
        .find(|line| line.contains(NOT_STARTED_LINE))
        .and_then(|line| line.split_once("reason="))
        .map(|(_, reason)| reason.trim().to_string())
        .unwrap_or_else(|| panic!("the not-started line must carry a reason:\n{logs}"));
    assert!(
        !reason.is_empty() && reason != STARTING_PLACEHOLDER,
        "the reason must be the settled answer, never the `{STARTING_PLACEHOLDER}` placeholder \
         the bring-up seeds before the question is answered; got reason={reason:?}"
    );

    assert!(
        node.still_running(),
        "the runtime must still be RUNNING once the worker question has been answered — a \
         detector that will not load is a loud not-serving state this node reports and keeps \
         reporting, never a reason to end the run. The node's camera is honestly ingest-failed \
         and its liveness surface says so; what must not happen is the process going away, \
         because an operator whose node has exited has nothing left to ask:\n{logs}"
    );
}

#[test]
fn the_doctor_tells_a_camera_operator_their_node_is_not_serving_and_why() {
    let tmp = tempfile::tempdir().expect("data dir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create the deployment directory");
    let model_path = unloadable_model(tmp.path());
    let node = spawn_camera_node_with_an_unloadable_detector(&data_dir, &model_path);

    wait_for_the_load_to_fail(&node);

    // The status task writes the settled snapshot on its own refresh, so the
    // fixture waits for the STATE to appear rather than waiting a chosen
    // interval and hoping. A verdict is settled when it is no longer the
    // in-flight placeholder.
    let rendered = wait_until(
        "the doctor rendering to carry a settled fabric-worker-serving verdict",
        VERDICT_WAIT,
        || {
            let rendered = doctor(&data_dir);
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
            doctor(&data_dir),
            node.logs()
        )
    });

    assert!(
        rendered.contains("fabric-worker-serving=false"),
        "a node whose detector will not load is not serving the fleet and must say so:\n{rendered}"
    );
    assert!(
        !rendered.contains(&format!("reason={STARTING_PLACEHOLDER}")),
        "the not-serving reason must be the settled one, never the placeholder seeded before the \
         worker question was answered:\n{rendered}"
    );
    assert!(
        rendered.contains("detector_model_path")
            || rendered.contains("VIGIL_DETECTOR_MODEL_PATH")
            || rendered.to_ascii_lowercase().contains("model"),
        "the not-serving line must name what is wrong — the detector model this node could not \
         load — so the operator has somewhere to go:\n{rendered}"
    );
}
