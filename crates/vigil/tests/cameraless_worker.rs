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

use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
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

fn vigil_binary_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_vigil") {
        return PathBuf::from(path);
    }
    if let Some(path) = option_env!("CARGO_BIN_EXE_vigil") {
        return PathBuf::from(path);
    }
    workspace_root()
        .join("target")
        .join("debug")
        .join(if cfg!(windows) { "vigil.exe" } else { "vigil" })
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("allocate local TCP port");
    listener.local_addr().expect("read local TCP port").port()
}

fn health_status(port: u16) -> Option<u16> {
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

fn capture_pipe<T: Read + Send + 'static>(pipe: Option<T>) -> Arc<Mutex<String>> {
    let logs = Arc::new(Mutex::new(String::new()));
    if let Some(mut pipe) = pipe {
        let captured = Arc::clone(&logs);
        thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if let Ok(mut logs) = captured.lock() {
                            logs.push_str(&String::from_utf8_lossy(&buffer[..read]));
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    logs
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
/// `ha_test_support` points `VIGIL_DETECTOR_MODEL_PATH` at). Staging it makes
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

fn spawn_cameraless_fabric_worker(
    data_dir: &std::path::Path,
    health_port: u16,
    model_path: Option<&std::path::Path>,
    worker_slot_deadline_ms: u64,
) -> Worker {
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .env("VIGIL_DATA_DIR", data_dir)
        .env("VIGIL_HEALTH_PORT", health_port.to_string())
        .env("VIGIL_FABRIC_HUB", "true")
        // Test-only deadline override (scaffold, fabric.rs): a REAL model load
        // needs headroom over the production wait in a debug build (~2-3s to
        // deserialize the fixture checkpoint), so the serving arm passes a
        // generous bound; the no-model arm passes a short one so its
        // not-started path resolves quickly.
        .env(
            "VIGIL_FABRIC_WORKER_SLOT_DEADLINE_MS",
            worker_slot_deadline_ms.to_string(),
        )
        .env_remove("VIGIL_RTSP_URL");
    match model_path {
        Some(path) => {
            command.env("VIGIL_DETECTOR_MODEL_PATH", path);
        }
        None => {
            command.env_remove("VIGIL_DETECTOR_MODEL_PATH");
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
    let health_port = free_port();
    let model_path = fixture_model_path();
    assert!(
        model_path.is_file(),
        "detector model fixture must be present to stage a loadable model: {}",
        model_path.display()
    );
    // A cameraless worker with a REAL, loadable model staged: it has executable
    // capability, so it must advertise and serve. Generous slot deadline so the
    // debug-build checkpoint load (~2-3s) completes before the wait ends.
    let worker =
        spawn_cameraless_fabric_worker(data_dir.path(), health_port, Some(&model_path), 20_000);

    assert!(
        wait_for_health(health_port, Duration::from_secs(15)),
        "a cameraless fabric-enrolled node must still reach a ready /health \
         (zero cameras is not a startup failure)"
    );

    // Bounded wait for the worker loop's own terminal line (started or
    // not-started); the shortened deadline above keeps this well under the
    // real 60s.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut logs = String::new();
    while Instant::now() < deadline {
        logs = worker.stdout.lock().expect("stdout lock").clone();
        if logs.contains("fabric_worker_loop_started=true")
            || logs.contains("fabric_worker_loop_not_started=true")
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        health_status(health_port) == Some(200),
        "the runtime must still be alive (ready) after the worker-slot deadline"
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
    let health_port = free_port();
    // No model staged; short slot deadline so the not-started path resolves fast.
    let worker = spawn_cameraless_fabric_worker(data_dir.path(), health_port, None, 1500);

    assert!(
        wait_for_health(health_port, Duration::from_secs(15)),
        "a cameraless node with no staged model must still reach a ready /health \
         (a missing model is a loud not-serving state, not a startup failure)"
    );

    // Bounded wait past the shortened worker-slot deadline so the not-started
    // path has resolved and the persisted stats snapshot has settled.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut logs = String::new();
    while Instant::now() < deadline {
        logs = worker.stdout.lock().expect("stdout lock").clone();
        if logs.contains("fabric_worker_loop_not_started=true") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(Duration::from_millis(500));

    assert!(
        health_status(health_port) == Some(200),
        "the runtime must still be alive (ready) with no staged model: {logs}"
    );

    let doctor_output = run_doctor_acceleration(data_dir.path());

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
