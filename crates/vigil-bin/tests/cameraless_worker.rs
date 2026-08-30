//! Cameraless worker bootstrap (diagnosis: fabric.rs:967-988's bounded wait
//! over `worker_detector_slot`, populated only at runtime.rs:824-827 inside
//! the PER-CAMERA detector-load path). A node started with fabric enrolled
//! and detection enabled but ZERO cameras never runs any per-camera thread,
//! so `worker_detector_slot` is never set, the worker loop never starts, and
//! this node can never serve the fleet — even though the owner steer is
//! explicit: a remote node started with detector/decode enabled must USE
//! that regardless of whether it owns cameras.
//!
//! T1 drives the real `vigil` binary (the bug lives in runtime.rs process
//! bring-up, not in any function reachable from an integration test without
//! it) with zero cameras and asserts the POST-FIX state: the worker loop
//! reports started (not the `fabric_worker_loop_not_started=true` line), a
//! `vigil-detector-<backend>` capability row exists in this node's own
//! ledger, and the process stays alive throughout. RED today: the slot is
//! never populated, so the bounded wait always ends in
//! `fabric_worker_loop_not_started=true` and no capability row is ever
//! written.
//!
//! T2 proves the same intent from the HUB's observation point, using an
//! in-process hub + worker pair (the existing fabric test fixture pattern
//! from `symmetry.rs`/`fabric_enrollment.rs`: two real `FabricRuntime`
//! instances, a real ticket, no subprocess): once a cameraless worker is
//! serving, the hub's own `remote_detector_capabilities()` must include the
//! worker's node id and truthful backend. RED today for a SEPARATE, newly
//! found reason worth flagging on its own: `FabricRuntime::spawn_worker_loop`
//! (fabric.rs:591) never advertises the documented `vigil-detector-<backend>`
//! capability id — it delegates entirely to contextdb's
//! `work_ledger::run_worker_loop`, which advertises the fixed literal
//! `"worker-loop"` (contextdb-server/src/work_ledger.rs:590). No existing
//! test exercises `spawn_worker_loop` end-to-end (`detector_worker.rs` and
//! `symmetry.rs` both bypass it, calling `advertise_capability`/
//! `poll_and_execute_once` directly with the correct id) so this gap is
//! real, present in the current shipped code, and independent of the
//! cameraless bug: `remote_detector_capabilities()`'s
//! `capability_id.strip_prefix("vigil-detector-")` filter (fabric.rs:535)
//! can never match a row `spawn_worker_loop` itself wrote.

#![cfg(feature = "fabric")]

use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use contextdb_engine::Database;

use vigil::DecodedRgbFrame;
use vigil::detector_workclass::{DetectorDetection, OrderedF64};
use vigil::fabric::{FabricDetectorBackend, FabricRuntime};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    capture_pipe, free_port, vigil_binary_path, wait_until, workspace_root,
};

async fn within<F: std::future::Future>(fut: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .expect("bounded fabric operation exceeded 30s")
}

struct SpyBackend {
    tag: &'static str,
}

impl FabricDetectorBackend for SpyBackend {
    fn backend_tag(&self) -> &str {
        self.tag
    }

    fn model_sha256(&self) -> &str {
        "spy-model-sha"
    }

    fn detect(
        &self,
        _frames: &[DecodedRgbFrame],
        _clip_sha256: &str,
        _sample_frames: usize,
        _confidence_threshold: f64,
    ) -> Result<Vec<DetectorDetection>, String> {
        Ok(vec![DetectorDetection {
            class_name: "person".to_string(),
            confidence: OrderedF64(0.9),
            bbox: "1,1,2,2".to_string(),
            frame_index: 0,
        }])
    }
}

// ── T1: subprocess (the bug lives in runtime.rs process bring-up) ─────────

fn health_status(port: u16) -> Option<u16> {
    use std::io::Read;
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(200)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    let _ = stream.write_health_probe();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
}

trait WriteHealthProbe {
    fn write_health_probe(&mut self) -> std::io::Result<()>;
}

impl WriteHealthProbe for TcpStream {
    fn write_health_probe(&mut self) -> std::io::Result<()> {
        use std::io::Write;
        self.write_all(b"GET /health HTTP/1.1\r\nhost: localhost\r\n\r\n")
    }
}

fn wait_for_health(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if health_status(port) == Some(200) {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

struct Worker {
    child: Child,
    stdout: Arc<Mutex<String>>,
}

impl Worker {
    fn kill_and_wait(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The repo's real detector model fixture (a raw yolox-tiny checkpoint the
/// production `yolox_detector` load path reads directly — the same artifact
/// `deterministic_fixture_support` points `VIGIL_DETECTOR_MODEL_PATH` at). Staging it makes
/// the worker's detector actually LOADABLE, which is what earns the capability
/// advertisement (advertisement requires executable capability — PO ruling
/// 2026-07-13).
fn fixture_model_path() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("fixtures")
        .join("models")
        .join("yolox-tiny-coco.pth")
}

/// Run `vigil doctor acceleration` against an already-running node's data dir
/// and return its combined stdout/stderr (the surface that renders the shared
/// `fabric-status` / `fabric-worker-serving` line).
fn run_doctor_acceleration(data_dir: &std::path::Path) -> String {
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

/// How long the fixture is willing to keep waiting for the worker loop's own
/// verdict. A liveness guard on the test, never a bound on the load: the
/// standing owner ruling of 2026-08-14 forbids judging any product outcome by
/// elapsed time, and a viable slow cold start must still end in a started
/// worker however long it takes.
const WORKER_LOOP_VERDICT_WAIT: Duration = Duration::from_secs(120);

/// How long the fixture is willing to keep looking for the settled state. A
/// liveness guard on the test itself, never a bound the product is asked to
/// meet: the assertions below read the rendered state, and no outcome here is
/// decided by how long it took to appear.
const FABRIC_STATUS_SETTLE_WAIT: Duration = Duration::from_secs(60);

/// The doctor rendering, read once this node's own fabric status has SETTLED —
/// a serving verdict whose reason is no longer the in-flight `worker-loop-
/// starting` placeholder. The status task writes that snapshot on its own
/// refresh, so the fixture waits for the STATE to appear instead of waiting a
/// chosen interval and hoping (a flat wait passes on a loaded box and fails on
/// a quiet one, and judging a product outcome by elapsed time is banned
/// outright by the owner ruling of 2026-08-14).
fn doctor_once_fabric_status_settles(data_dir: &std::path::Path) -> String {
    wait_until(
        "the doctor rendering to carry a settled fabric-worker-serving verdict",
        FABRIC_STATUS_SETTLE_WAIT,
        || {
            let rendered = run_doctor_acceleration(data_dir);
            let settled = rendered.contains("fabric-worker-serving=true")
                || (rendered.contains("fabric-worker-serving=false reason=")
                    && !rendered.contains("reason=worker-loop-starting"));
            Ok(settled.then_some(rendered))
        },
    )
    .expect("this node's own fabric status must reach the surface an operator reads")
}

fn spawn_cameraless_fabric_worker(
    data_dir: &std::path::Path,
    health_port: u16,
    model_path: Option<&std::path::Path>,
) -> Worker {
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .arg("--health-port")
        .arg(health_port.to_string())
        .arg("--fabric-hub")
        .arg("true")
        // Acceleration off: the subject here is the cameraless worker slot
        // itself, so the node loads the CPU detector it advertises rather than
        // preparing an accelerated one first. Nothing below waits on elapsed
        // time — each arm waits for the worker loop's own terminal line.
        .arg("--accelerated-detection")
        .arg("false")
        .env("VIGIL_DATA_DIR", data_dir)
        .env_remove("VIGIL_RTSP_URL");
    match model_path {
        Some(path) => {
            command.arg("--detector-model-path").arg(path);
        }
        None => {
            // No CLI-flag equivalent of "explicitly absent" exists for
            // --detector-model-path (an omitted flag already means absent),
            // so the no-model arm needs no action here — env_remove is now
            // a no-op left over from the prior env-driven invocation and is
            // dropped rather than kept as dead code.
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn vigil binary");
    let stdout = capture_pipe(child.stdout.take());
    let _stderr = capture_pipe(child.stderr.take());
    Worker { child, stdout }
}

#[test]
fn detector_job_registers_as_vigil_detector_class_slot_populates_without_a_camera() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let health_port = free_port().expect("reserve a free port");
    let model_path = fixture_model_path();
    assert!(
        model_path.is_file(),
        "detector model fixture must be present to stage a loadable model: {}",
        model_path.display()
    );
    // A cameraless worker with a REAL, loadable model staged: it has executable
    // capability, so it must advertise and serve however long the load takes.
    let worker = spawn_cameraless_fabric_worker(data_dir.path(), health_port, Some(&model_path));

    assert!(
        wait_for_health(health_port, Duration::from_secs(15)),
        "a cameraless fabric-enrolled node must still reach a ready /health \
         (zero cameras is not a startup failure)"
    );

    // The start is installed while the model is still loading, so the
    // preparation completes only AFTER something is already waiting for it —
    // the ordinary cold start, and the one the retired deadline used to give
    // up on. Waits for the worker loop's own terminal line (started or
    // not-started); the bound is the fixture's liveness guard, and nothing
    // here judges the outcome by how long the load took.
    let logs = wait_until(
        "the worker loop to say whether it started",
        WORKER_LOOP_VERDICT_WAIT,
        || {
            let logs = worker.stdout.lock().expect("stdout lock").clone();
            let spoken = logs.contains("fabric_worker_loop_started=true")
                || logs.contains("fabric_worker_loop_not_started=true");
            Ok(spoken.then_some(logs))
        },
    )
    .expect("a cameraless node with a loadable model must reach a worker-loop verdict");

    assert!(
        health_status(health_port) == Some(200),
        "the runtime must still be alive (ready) once the worker loop has spoken"
    );

    assert!(
        logs.contains("fabric_worker_loop_started=true"),
        "a cameraless node with detection enabled must start the fabric worker \
         loop (serve the fleet) instead of giving up — got: {logs}"
    );
    assert!(
        !logs.contains("fabric_worker_loop_not_started=true"),
        "must not hit the not-started line: {logs}"
    );
    assert_eq!(
        logs.matches("fabric_worker_loop_started=true").count(),
        1,
        "exactly ONE worker loop serves the fleet for one node — a late-arriving \
         preparation must not start a second: {logs}"
    );

    let node_id = logs
        .lines()
        .find_map(|line| line.strip_prefix("fabric_ready=true node_id="))
        .map(str::to_string)
        .expect("fabric_ready line must carry this node's id");

    worker.kill_and_wait();

    let ledger_path = data_dir.path().join("fabric").join("fabric-ledger.db");
    let db = Database::open(&ledger_path).expect("open this node's own fabric ledger");
    let result = db
        .execute(
            "SELECT node_id, capability_id FROM work_capabilities",
            &std::collections::HashMap::new(),
        )
        .expect("scan work_capabilities");
    let node_idx = result
        .columns
        .iter()
        .position(|c| c == "node_id")
        .expect("node_id column");
    let capability_idx = result
        .columns
        .iter()
        .position(|c| c == "capability_id")
        .expect("capability_id column");
    let has_detector_capability = result.rows.iter().any(|row| {
        let contextdb_core::Value::Text(node) = &row[node_idx] else {
            return false;
        };
        let contextdb_core::Value::Text(capability_id) = &row[capability_idx] else {
            return false;
        };
        node == &node_id && capability_id.starts_with("vigil-detector-")
    });
    assert!(
        has_detector_capability,
        "a vigil-detector-<backend> capability row for this node must exist \
         in its own ledger once the worker loop starts serving; rows found: {:?}",
        result.rows
    );
}

// ── T1 truthfulness arm: advertisement requires executable capability ─────

#[test]
fn cameraless_worker_without_a_loadable_model_does_not_advertise_and_reports_no_model() {
    // The mirror of the arm above: the SAME cameraless serving-role node, but
    // with NO detection model staged, has no executable capability. Advertising
    // a detector it cannot run would lie to the fleet (PO ruling 2026-07-13:
    // advertisement requires executable capability). So it must write NO
    // vigil-detector-<backend> capability row, and its own doctor/status
    // rendering must say fabric-worker-serving=false with reason=no-model and
    // the named staging fix — never an enrolled-looking node that silently
    // serves nothing.
    let data_dir = tempfile::tempdir().expect("data dir");
    let health_port = free_port().expect("reserve a free port");
    // No model staged: the terminal load failure itself resolves the
    // not-started path — nothing waits on a deadline for it.
    let worker = spawn_cameraless_fabric_worker(data_dir.path(), health_port, None);

    assert!(
        wait_for_health(health_port, Duration::from_secs(15)),
        "a cameraless node with no staged model must still reach a ready /health \
         (a missing model is a loud not-serving state, not a startup failure)"
    );

    // A model that cannot be loaded is a SETTLED answer — no detector is
    // coming — so the not-started line arrives on that failure, not on any
    // expiry. The bound is the fixture's liveness guard.
    let logs = wait_until(
        "the worker loop to report that it did not start",
        WORKER_LOOP_VERDICT_WAIT,
        || {
            let logs = worker.stdout.lock().expect("stdout lock").clone();
            Ok(logs
                .contains("fabric_worker_loop_not_started=true")
                .then_some(logs))
        },
    )
    .expect("a node whose model cannot be loaded must say so rather than wait forever");
    assert!(
        health_status(health_port) == Some(200),
        "the runtime must still be alive (ready) with no staged model: {logs}"
    );
    assert!(
        !logs.contains("fabric_worker_loop_started=true"),
        "a node with no loadable model must never start a worker loop: {logs}"
    );
    assert_eq!(
        logs.matches("fabric_worker_loop_not_started=true").count(),
        1,
        "the settled no-detector answer is given ONCE, not repeated by anything \
         still watching: {logs}"
    );

    let doctor_output = doctor_once_fabric_status_settles(data_dir.path());

    let node_id = logs
        .lines()
        .find_map(|line| line.strip_prefix("fabric_ready=true node_id="))
        .map(str::to_string)
        .expect("fabric_ready line must carry this node's id");

    worker.kill_and_wait();

    assert!(
        doctor_output.contains("fabric-worker-serving=false"),
        "a worker with no loadable model must report it is not serving: {doctor_output}"
    );
    assert!(
        doctor_output.contains("reason=no-model"),
        "the not-serving reason must name the missing model (reason=no-model): {doctor_output}"
    );
    assert!(
        doctor_output.contains("detector_model_path")
            || doctor_output.contains("VIGIL_DETECTOR_MODEL_PATH"),
        "the not-serving line must name the concrete staging fix: {doctor_output}"
    );

    // And it must NOT have advertised a detector capability it cannot execute.
    let ledger_path = data_dir.path().join("fabric").join("fabric-ledger.db");
    let db = Database::open(&ledger_path).expect("open this node's own fabric ledger");
    let result = db
        .execute(
            "SELECT node_id, capability_id FROM work_capabilities",
            &std::collections::HashMap::new(),
        )
        .expect("scan work_capabilities");
    let node_idx = result
        .columns
        .iter()
        .position(|c| c == "node_id")
        .expect("node_id column");
    let capability_idx = result
        .columns
        .iter()
        .position(|c| c == "capability_id")
        .expect("capability_id column");
    let advertised_detector = result.rows.iter().any(|row| {
        let contextdb_core::Value::Text(node) = &row[node_idx] else {
            return false;
        };
        let contextdb_core::Value::Text(capability_id) = &row[capability_idx] else {
            return false;
        };
        node == &node_id && capability_id.starts_with("vigil-detector-")
    });
    assert!(
        !advertised_detector,
        "a worker with no loadable model must NOT advertise a vigil-detector-<backend> \
         capability row (advertisement requires executable capability); rows found: {:?}",
        result.rows
    );
}

// ── T2: in-process hub + cameraless-enrolled worker pair ──────────────────

#[tokio::test]
async fn hub_sees_cameraless_worker_detector_capability_after_join() {
    // Hub node: carries the embedded hub (Design decision E), no cameras of
    // its own — irrelevant to what's under test.
    let hub_dir = tempfile::tempdir().expect("hub data dir");
    let hub_runtime = within(FabricRuntime::start(hub_dir.path(), None, true))
        .await
        .expect("hub-role node must stand up");
    let ticket = hub_runtime
        .join_instruction()
        .split("ticket=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("hub join instruction must carry a ticket")
        .to_string();

    // Worker node: a cameraless node enrolling with the hub's ticket, then
    // running the REAL production `spawn_worker_loop` entry point (the exact
    // call runtime.rs's fabric bring-up makes once the worker_detector_slot
    // is populated) with a truthful burn-cpu backend double.
    let worker_dir = tempfile::tempdir().expect("worker data dir");
    let worker_runtime = within(FabricRuntime::start(
        worker_dir.path(),
        Some(&ticket),
        false,
    ))
    .await
    .expect("cameraless worker must enroll via the hub's ticket");

    let backend = Arc::new(SpyBackend { tag: "burn-cpu" });
    let shutdown = Arc::new(AtomicBool::new(false));
    let _worker_task = worker_runtime.spawn_worker_loop(backend, Arc::clone(&shutdown));

    // Give the worker loop's initial advertise+push, and the hub's own
    // periodic pull, a bounded window to converge.
    let worker_node_id = worker_runtime.node_id.clone();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut remotes = Vec::new();
    while Instant::now() < deadline {
        let _ = within(hub_runtime.client.pull_default()).await;
        remotes = within(hub_runtime.remote_detector_capabilities())
            .await
            .expect("hub must be able to scan remote detector capabilities");
        if remotes
            .iter()
            .any(|remote| remote.node_id == worker_node_id && remote.backend == "burn-cpu")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    shutdown.store(true, Ordering::SeqCst);

    assert!(
        remotes
            .iter()
            .any(|remote| remote.node_id == worker_node_id && remote.backend == "burn-cpu"),
        "the hub's own remote_detector_capabilities() must include the \
         cameraless worker's node id + truthful backend once it is serving; \
         saw: {remotes:?}"
    );
}
