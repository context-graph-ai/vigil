//! Cross-PROCESS remote-detector visibility (the missing test class that let
//! the S2 smoke fail TWICE): the full advertise → push → hub-apply → cache →
//! render chain, exercised between two REAL `vigil` binaries talking over real
//! loopback iroh — never in-process. The in-process fixtures
//! (`symmetry.rs`/`detector_worker.rs`/`cameraless_worker.rs::T2`) all share
//! one `Database` or drive `advertise_capability`/`poll_and_execute_once`
//! directly, so they cannot witness a push that is dropped on the wire; the
//! owner smoke did, twice, and shipped both times because CI's hub was always
//! up before the worker pushed.
//!
//! The assertion is the operator-visible contract: the HUB binary's own
//! `vigil doctor acceleration` output must render a `fabric-status=… remote-
//! detectors=<worker-node>:<backend> …` line that names the joined worker's
//! node id and truthful backend (`render_fabric_status_receipt`,
//! offload_policy.rs:288).
//!
//! Two arms:
//!
//! * `..._hub_first` — the hub is up before the worker enrolls. The worker's
//!   one-shot startup push lands immediately. This is the STANDING GUARD: it
//!   is expected to pass at HEAD and stays as the regression fence that the
//!   render chain keeps working end to end across two processes.
//!
//! * `..._hub_up_late` — the worker enrolls against a valid ticket while the
//!   hub's process is DOWN, so its one-shot startup push
//!   (`fabric.rs:674 let _ = client.push().await`, delegating into
//!   `work_ledger::run_worker_loop`'s own one-shot startup push) misses and is
//!   swallowed; the hub then restarts on the SAME data dir (identity + sticky
//!   port persisted beside it → the issued ticket stays valid and reachable).
//!   Nothing on the worker's standing poll loop ever re-pushes the outstanding
//!   advertisement, so the hub never learns the capability. RED today: the
//!   hub's rendered `remote-detectors=` stays `-` forever.

#![cfg(feature = "fabric")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn vigil_binary_path() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_vigil") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_vigil") {
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

/// A NON-DEFAULT free TCP port. The default review port 8098 is known to be
/// squatted on the smoke box by a stray `vigil run`; every port this test
/// binds is OS-allocated, so it never collides with a default-port process.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("allocate local TCP port");
    listener.local_addr().expect("read local TCP port").port()
}

/// The repo's real detector model fixture — staging it makes the worker's
/// detector actually LOADABLE, which is what earns the capability
/// advertisement (advertisement requires executable capability — PO ruling
/// 2026-07-13). Both the hub and the worker stage it (a cameraless node only
/// serves when it has an executable detector).
fn fixture_model_path() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("fixtures")
        .join("models")
        .join("yolox-tiny-coco.pth")
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

struct Node {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

impl Node {
    fn kill_and_wait(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn logs(&self) -> String {
        let out = self.stdout.lock().expect("stdout lock").clone();
        let err = self.stderr.lock().expect("stderr lock").clone();
        format!("{out}{err}")
    }
}

/// Spawn a real `vigil run` process: cameraless (no `VIGIL_RTSP_URL`), a real
/// loadable detector model staged, non-default OS-allocated health/review
/// ports. When `hub` is true it carries the embedded hub; when `ticket` is set
/// it enrolls as an edge that dials that ticket.
fn spawn_node(
    data_dir: &std::path::Path,
    health_port: u16,
    review_port: u16,
    hub: bool,
    ticket: Option<&str>,
) -> Node {
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .env("VIGIL_DATA_DIR", data_dir)
        .env("VIGIL_HEALTH_PORT", health_port.to_string())
        .env("VIGIL_REVIEW_PORT", review_port.to_string())
        .env("VIGIL_DETECTOR_MODEL_PATH", fixture_model_path())
        // Generous worker-slot deadline: a real checkpoint load in a debug
        // build needs headroom over the production wait before the cameraless
        // worker loop is declared started.
        .env("VIGIL_FABRIC_WORKER_SLOT_DEADLINE_MS", "20000")
        .env_remove("VIGIL_RTSP_URL")
        .env_remove("VIGIL_FABRIC_HUB")
        .env_remove("VIGIL_FABRIC_TICKET");
    if hub {
        command.env("VIGIL_FABRIC_HUB", "true");
    }
    if let Some(ticket) = ticket {
        command.env("VIGIL_FABRIC_TICKET", ticket);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn vigil binary");
    let stdout = capture_pipe(child.stdout.take());
    let stderr = capture_pipe(child.stderr.take());
    Node {
        child,
        stdout,
        stderr,
    }
}

fn health_status(port: u16) -> Option<u16> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(200)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    let _ = stream.write_all(b"GET /health HTTP/1.1\r\nhost: localhost\r\n\r\n");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
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

/// The hub prints its ready-to-paste join line at startup
/// (`fabric-join ticket=<ticket> command=…`, fabric.rs:1021). Read the ticket
/// off its stdout so the worker can enroll with the exact value an operator
/// would paste.
fn wait_for_ticket(node: &Node, timeout: Duration) -> Option<String> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let logs = node.stdout.lock().expect("stdout lock").clone();
        if let Some(ticket) = logs.lines().find_map(|line| {
            line.split("ticket=")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .map(str::to_string)
        }) {
            return Some(ticket);
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Wait until a given marker line appears on this node's stdout.
fn wait_for_line(node: &Node, needle: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if node.stdout.lock().expect("stdout lock").contains(needle) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Read this node's own id off its `fabric_ready=true node_id=<id>` line.
fn wait_for_node_id(node: &Node, timeout: Duration) -> Option<String> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let logs = node.stdout.lock().expect("stdout lock").clone();
        if let Some(node_id) = logs
            .lines()
            .find_map(|line| line.strip_prefix("fabric_ready=true node_id="))
            .map(str::to_string)
        {
            return Some(node_id);
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Run `vigil doctor acceleration` against a running node's data dir and
/// return its combined stdout/stderr — the surface that renders the shared
/// `fabric-status=… remote-detectors=…` line from the persisted stats
/// snapshot.
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

/// Whether the hub's rendered `remote-detectors=` field names this worker's
/// node id with the given backend (`render_fabric_status_receipt` joins each
/// remote as `<node_id>:<backend>`).
fn hub_renders_remote_worker(doctor_output: &str, worker_node_id: &str, backend: &str) -> bool {
    doctor_output.contains(&format!("{worker_node_id}:{backend}"))
}

/// Poll the hub's `vigil doctor acceleration` for a bounded window until its
/// rendered `remote-detectors=` names the worker, returning the last output
/// seen (for the failure message).
fn poll_hub_for_remote_worker(
    hub_data_dir: &std::path::Path,
    worker_node_id: &str,
    backend: &str,
    timeout: Duration,
) -> (bool, String) {
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < timeout {
        last = run_doctor_acceleration(hub_data_dir);
        if hub_renders_remote_worker(&last, worker_node_id, backend) {
            return (true, last);
        }
        thread::sleep(Duration::from_millis(500));
    }
    (false, last)
}

// ── Arm A: hub up first (standing guard — expected GREEN at HEAD) ──────────

#[test]
fn two_real_processes_render_remote_detectors_line_hub_first() {
    assert!(
        fixture_model_path().is_file(),
        "detector model fixture must be present: {}",
        fixture_model_path().display()
    );

    let hub_dir = tempfile::tempdir().expect("hub data dir");
    let hub_health = free_port();
    let hub = spawn_node(hub_dir.path(), hub_health, free_port(), true, None);
    assert!(
        wait_for_health(hub_health, Duration::from_secs(20)),
        "hub node must come up"
    );
    let ticket = wait_for_ticket(&hub, Duration::from_secs(20))
        .expect("hub must print its fabric-join ticket");

    // Worker enrolls with the hub already up: its one-shot startup push lands
    // immediately.
    let worker_dir = tempfile::tempdir().expect("worker data dir");
    let worker_health = free_port();
    let worker = spawn_node(
        worker_dir.path(),
        worker_health,
        free_port(),
        false,
        Some(&ticket),
    );
    assert!(
        wait_for_health(worker_health, Duration::from_secs(20)),
        "worker node must reach a ready /health"
    );
    let worker_node_id =
        wait_for_node_id(&worker, Duration::from_secs(20)).expect("worker must print its node id");

    let (seen, doctor_output) = poll_hub_for_remote_worker(
        hub_dir.path(),
        &worker_node_id,
        "burn-cpu",
        Duration::from_secs(45),
    );

    worker.kill_and_wait();
    hub.kill_and_wait();

    assert!(
        seen,
        "with the hub up first, its own doctor acceleration must render \
         remote-detectors={worker_node_id}:burn-cpu once the worker joins; \
         last doctor output:\n{doctor_output}"
    );
}

// ── Arm B: hub up LATE (RED today — the one-shot push is swallowed) ────────

#[test]
fn two_real_processes_render_remote_detectors_line_hub_up_late() {
    assert!(
        fixture_model_path().is_file(),
        "detector model fixture must be present: {}",
        fixture_model_path().display()
    );

    // Bring the hub up ONCE to persist its identity + sticky port and mint a
    // real, reachable ticket, then take it down. The identity key and the
    // remembered port live beside the data dir, so a later restart on the SAME
    // data dir rebinds the SAME address and the issued ticket stays valid.
    let hub_dir = tempfile::tempdir().expect("hub data dir");
    let hub_health = free_port();
    let hub_review = free_port();
    let hub_first = spawn_node(hub_dir.path(), hub_health, hub_review, true, None);
    assert!(
        wait_for_health(hub_health, Duration::from_secs(20)),
        "hub node must come up to mint its ticket"
    );
    let ticket = wait_for_ticket(&hub_first, Duration::from_secs(20))
        .expect("hub must print its fabric-join ticket");
    hub_first.kill_and_wait();
    // Let the OS release the hub's UDP socket before the worker starts dialing
    // a now-dead address.
    thread::sleep(Duration::from_millis(500));

    // Worker enrolls against the valid-but-currently-unreachable ticket. It
    // stands up standalone-serving (validated ticket ⇒ serving role), loads
    // its model, advertises locally, and fires its ONE startup push — which
    // misses, because nothing answers the hub's address yet, and is swallowed.
    let worker_dir = tempfile::tempdir().expect("worker data dir");
    let worker_health = free_port();
    let worker = spawn_node(
        worker_dir.path(),
        worker_health,
        free_port(),
        false,
        Some(&ticket),
    );
    assert!(
        wait_for_health(worker_health, Duration::from_secs(20)),
        "a worker enrolled against a temporarily-unreachable hub must still \
         reach a ready /health (a down hub is not a startup failure)"
    );
    let worker_node_id =
        wait_for_node_id(&worker, Duration::from_secs(20)).expect("worker must print its node id");

    // Wait until the worker's loop has actually STARTED — that is the point at
    // which it advertises locally and fires its one-shot startup push
    // (fabric.rs:674) against the DOWN hub.
    assert!(
        wait_for_line(&worker, "fabric_worker_loop_started=true", Duration::from_secs(20)),
        "the cameraless worker must start its worker loop (and fire its \
         one-shot push) before the hub returns: {}",
        worker.logs()
    );

    // Hold the hub down long enough for that ONE push to DEFINITIVELY give up.
    // The push is not a fast fail: `SyncClient::push` → `ensure_connected`
    // dials the ticket, and iroh keeps trying to reach the node (CONNECT_TIMEOUT
    // 5s per attempt over the retry budget) — so a hub that returns while the
    // dial is still in flight would let the SAME push call heal, which is NOT
    // the defect under test. Measured on this box: a 25s window still heals, a
    // 40s window is permanently lost. 55s puts the hub's return comfortably
    // AFTER the one-shot push has failed and its watermark stayed put, so the
    // ONLY thing that could still deliver the capability is a re-push on the
    // standing poll cadence — which is exactly what does not exist today.
    thread::sleep(Duration::from_secs(55));

    // The hub's routing finally comes up, on the SAME data dir → same sticky
    // port → the worker's ticket is now reachable.
    let hub_second = spawn_node(hub_dir.path(), free_port(), free_port(), true, None);
    assert!(
        wait_for_health_any(&hub_second, Duration::from_secs(20)),
        "the restarted hub must come up on its remembered sticky port: {}",
        hub_second.logs()
    );

    let (seen, doctor_output) = poll_hub_for_remote_worker(
        hub_dir.path(),
        &worker_node_id,
        "burn-cpu",
        Duration::from_secs(45),
    );

    let worker_logs = worker.logs();
    worker.kill_and_wait();
    hub_second.kill_and_wait();

    assert!(
        seen,
        "a worker whose ONE startup push missed while the hub was down must be \
         re-advertised on the standing poll cadence once the hub returns — the \
         restarted hub's doctor acceleration must eventually render \
         remote-detectors={worker_node_id}:burn-cpu, not `-` forever.\n\
         last hub doctor output:\n{doctor_output}\n\nworker logs:\n{worker_logs}"
    );
}

/// The restarted hub rebinds a fresh OS-allocated health port each run, so
/// wait on its own `fabric_ready`/join line (proof it bound and is serving)
/// rather than a fixed health port.
fn wait_for_health_any(node: &Node, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let logs = node.stdout.lock().expect("stdout lock").clone();
        if logs.contains("fabric_ready=true") {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}
