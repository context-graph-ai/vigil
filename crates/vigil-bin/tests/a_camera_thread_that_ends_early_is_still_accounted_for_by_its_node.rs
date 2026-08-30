//! Every camera thread that ENDS is accounted for in the node's one worker
//! answer — including the ones that end before they ever reach a detector.
//!
//! A node answers the fleet's question "are you serving?" once, from the
//! worker-detector slot, and the bring-up declares up front how many camera
//! detector sources will really run (`runtime.rs`'s
//! `eligible_detector_sources`). That declaration is what makes "no detector
//! is coming from anywhere on this node" knowable: the answer settles when a
//! detector arrives, or when EVERY declared source has reported that it is
//! terminally out.
//!
//! The promise an operator holds is the one the counting exists to keep — a
//! node that will never serve says so, with something to act on. Today only
//! ONE of a camera thread's terminal endings reports: the detector load
//! failure. A camera thread that ends BEFORE the load — its analysis endpoint
//! carries a password with no username to attach it to, so the endpoint cannot
//! be resolved at all — simply returns. It is still counted as a source, and
//! it never answers as one. Put that camera beside a camera whose detector
//! model will not load and the arithmetic never completes: one failure
//! reported against two sources declared. Nothing settles. The node keeps the
//! `worker-loop-starting` placeholder for the rest of its life, and the
//! operator who asks why their node is not serving the fleet is told, on every
//! surface they can reach, that it is still coming up — while the real error,
//! a detection model that will not load, is sitting in the log with nothing
//! carrying it to the answer.
//!
//! That is a REGRESSION, and it is worth naming as one: a node with a single
//! camera in this condition used to answer immediately, because a single
//! source's failure was the node's answer
//! (`a_camera_nodes_failed_detector_load_settles_its_worker_slot.rs`, still
//! green). Multi-camera counting bought the ordered legs
//! (`a_node_with_several_cameras_serves_if_any_of_them_loads_a_detector.rs`)
//! and lost the guarantee that the count can ever be reached.
//!
//! So what is asserted here is the PROMISE — the node settles, and settles on
//! something real — never the mechanism by which an early ending reports
//! itself. Any implementation that accounts for every terminal camera-thread
//! exit satisfies both identities below.
//!
//! How the conditions are CONSTRUCTED, deterministically and without a
//! network: both cameras are pointed at syntactically valid RTSP endpoints
//! that are never contacted (the connection happens well after the detector
//! load, so nothing here needs a stream, and port 1 is privileged so no
//! unprivileged process on this machine can answer by accident). One camera
//! carries a `password` and no `username`, which the deployment accepts at
//! load — it is a real thing an operator types — and which its own thread then
//! cannot resolve into a session credential, so that thread ends before it
//! reaches a detector. The other camera's endpoint resolves cleanly, so its
//! thread reaches the detector load and meets whatever model this deployment
//! staged.
//!
//! Nothing here waits on a clock or asserts on elapsed time. Each wait is for
//! a STATE the node publishes on its own output or through its own doctor
//! rendering; the bounds are the fixture's liveness guards, and the standing
//! owner ruling of 2026-08-14 forbids judging any product outcome by how long
//! it took.

#![cfg(feature = "fabric")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_until, workspace_root,
};

/// How long the fixture is willing to keep looking for the node's own verdict.
/// A liveness guard on the test, never a bound the product is asked to meet —
/// but it is what makes a node that NEVER answers read as a failure rather
/// than as a hang.
const VERDICT_WAIT: Duration = Duration::from_secs(120);

/// The camera whose analysis endpoint this deployment cannot resolve into a
/// session credential: a password with no username to attach it to. Its thread
/// ends before it reaches a detector.
const UNRESOLVABLE: (&str, &str) = ("lower gate", "rtsp://127.0.0.1:1/lower-gate");

/// The camera whose endpoint resolves cleanly, so its thread reaches the
/// detector load — the only ending this node accounts for today.
const RESOLVABLE: (&str, &str) = ("side path", "rtsp://127.0.0.1:1/side-path");

/// The line the node prints when its worker question is answered and the
/// answer is "no detector".
const NOT_STARTED_LINE: &str = "fabric_worker_loop_not_started=true";

/// The line the node prints when the answer is a detector.
const STARTED_LINE: &str = "fabric_worker_loop_started=true";

/// What a camera thread prints on the ending under test: its endpoint could
/// not be resolved into a session credential, so it stops before any detector
/// work. Read as proof the condition really was established, and keyed on the
/// credential failure itself rather than on the shared `rtsp probe failed`
/// prefix — the OTHER camera reaches its stream and fails to connect to it,
/// which prints that prefix too and is a different ending entirely, after the
/// detector load rather than before it.
const EARLY_ENDING_LINE: &str = "error=rtsp_password requires rtsp_username";

/// What a camera thread prints when the model will not load.
const LOAD_FAILED_LINE: &str = "detector model load failed";

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
fn unloadable_model(inside: &Path) -> PathBuf {
    let path = inside.join("not-a-detection-model.pth");
    fs::write(&path, b"this file is not a detector checkpoint").expect("stage an unloadable model");
    path
}

/// The repository's real detector checkpoint, which the production load path
/// reads and accepts. Staged when the leg needs a camera whose detector really
/// does arrive.
fn loadable_model() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("fixtures")
        .join("models")
        .join("yolox-tiny-coco.pth")
}

/// A fabric-serving node with both cameras above configured, sharing one
/// detector model. The first camera carries a password and no username: the
/// deployment accepts the entry, and that camera's own thread cannot turn it
/// into a session credential.
fn spawn_node(data_dir: &Path, model_path: &Path) -> Node {
    let config_path = data_dir.join("vigil.toml");
    fs::write(
        &config_path,
        format!(
            "site_name = \"home farm\"\n\n\
             [[cameras]]\nname = \"{}\"\nrtsp_url = \"{}\"\npassword = \"a-password-with-nobody-to-attach-it-to\"\n\n\
             [[cameras]]\nname = \"{}\"\nrtsp_url = \"{}\"\n",
            UNRESOLVABLE.0, UNRESOLVABLE.1, RESOLVABLE.0, RESOLVABLE.1
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
fn doctor(data_dir: &Path) -> String {
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
            "{error}: the node never reached the condition this leg exists to test. It \
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

#[test]
fn a_node_settles_when_one_camera_ended_early_and_the_other_could_not_load_a_detector() {
    let tmp = tempfile::tempdir().expect("data dir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create the deployment directory");
    let model_path = unloadable_model(tmp.path());
    let mut node = spawn_node(&data_dir, &model_path);

    // Established first, so nothing below is asserted about a different run:
    // the fabric bring-up really installed a start on the slot, one camera
    // thread really ended before any detector work, and the other really
    // reached the load and failed there.
    wait_for_line(
        &node,
        BRINGUP_DONE,
        "this node's fabric bring-up to finish attaching",
    );
    wait_for_line(
        &node,
        EARLY_ENDING_LINE,
        "the camera whose endpoint cannot be resolved to end before it reaches a detector",
    );
    wait_for_line(
        &node,
        LOAD_FAILED_LINE,
        "the other camera's detector load to fail",
    );

    // Neither camera on this node is going to produce a detector, and both
    // have finished trying. The node is waiting on nothing, so it owes the
    // fleet an answer.
    let logs = wait_for_the_worker_answer(
        &node,
        "every camera on this node has finished trying and none of them produced a detector, so \
         the node must ANSWER the fleet's question rather than sit on the starting placeholder \
         for the rest of its life. A camera thread that ends before its detector load is still a \
         source that will never produce one, and a node that counts it and never hears from it \
         can never reach its own count — the operator is told on every surface they can reach \
         that the node is still coming up, while the real error sits unreported in the log.",
    );

    assert!(
        !logs.contains(STARTED_LINE),
        "no camera on this node loaded a detector, so no worker loop may have started:\n{logs}"
    );
    assert_eq!(
        logs.matches(NOT_STARTED_LINE).count(),
        1,
        "one node, one worker question, ONE answer — not one per camera:\n{logs}"
    );

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
        "a node on which no camera produced a detector is not serving the fleet and must say \
         so:\n{rendered}"
    );
    assert_eq!(
        rendered.matches("fabric-worker-serving=").count(),
        1,
        "the operator is shown one verdict for the node, not one per camera:\n{rendered}"
    );
    assert!(
        rendered.contains("detector_model_path")
            || rendered.contains("VIGIL_DETECTOR_MODEL_PATH")
            || rendered.to_ascii_lowercase().contains("model"),
        "the reason must be a REAL one an operator can act on — the detection model this node \
         could not load — never a restatement of the outcome and never silence:\n{rendered}"
    );

    assert!(
        node.still_running(),
        "the runtime must still be RUNNING once the worker question has been answered: a camera \
         that ended early is a state this node reports and keeps reporting, never a reason to end \
         the run — an operator whose node has exited has nothing left to ask:\n{logs}"
    );
}

#[test]
fn a_camera_that_ended_early_does_not_cost_the_node_the_detector_its_neighbour_loaded() {
    let tmp = tempfile::tempdir().expect("data dir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create the deployment directory");
    // The same two cameras, but this deployment staged a real detection model,
    // so the camera whose endpoint resolves reaches the load and the load
    // succeeds.
    let mut node = spawn_node(&data_dir, &loadable_model());

    wait_for_line(
        &node,
        BRINGUP_DONE,
        "this node's fabric bring-up to finish attaching",
    );
    wait_for_line(
        &node,
        EARLY_ENDING_LINE,
        "the camera whose endpoint cannot be resolved to end before it reaches a detector",
    );

    // This is the other half of the same contract, and it is what an
    // over-correction breaks: accounting for a camera that ended early must
    // not turn one camera's ending into the NODE's answer. One detector is
    // what a node serves the fleet with, and this node has one.
    let logs = wait_for_the_worker_answer(
        &node,
        "one camera ended before its detector load and the other loaded a detector, so this node \
         IS serving the fleet — a node that reported itself out of detectors while holding a \
         perfectly good one would be telling the operator to go fix a model that loaded.",
    );

    assert!(
        logs.contains(STARTED_LINE),
        "a node holding a loaded detector serves the fleet with it, whatever happened to the \
         other camera:\n{logs}"
    );
    assert!(
        !logs.contains(NOT_STARTED_LINE),
        "a camera that ended early is one camera's ending, never the node's answer — this node \
         has a detector:\n{logs}"
    );

    assert!(
        node.still_running(),
        "the runtime must still be RUNNING once the worker question has been answered:\n{logs}"
    );
}
