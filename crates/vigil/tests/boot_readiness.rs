//! Boot READINESS + boot OBSERVABILITY (distributed-compute fix cycle 5).
//!
//! The live incident: `run_inner` opened the fabric ledger (FabricRuntime
//! bring-up → `Database::open`) SYNCHRONOUSLY on the boot path, before the
//! health state left `Starting` and with no output. A node with a camera could
//! not serve NVR duty until the fabric ledger open completed, and an operator
//! watching logs saw nothing but s6 noise. On a large ledger that open takes
//! minutes, so the Supervisor watchdog (2-probe cadence) killed the boot and
//! relooped — each unclean kill adding recovery work.
//!
//! Two contracts, exercised against a REAL `vigil run` process:
//!
//!  * `health_becomes_ready_before_fabric_attaches` — with fabric bring-up made
//!    deliberately slow (`VIGIL_FABRIC_BRINGUP_DELAY_MS`, a test-only lever that
//!    is inert in production), `/health` must answer 200 BEFORE the
//!    `fabric_ready=true` line appears. RED at HEAD: readiness waits for the
//!    synchronous bring-up, so health only turns 200 after fabric is already up.
//!
//!  * `boot_prints_phase_markers` — boot prints one-line `boot_phase=` markers
//!    (store open start/done, pipeline up, fabric bring-up start/done, with
//!    elapsed_ms on the done lines) so a wedged boot names its phase. RED at
//!    HEAD: no such markers exist.

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

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("allocate local TCP port");
    listener.local_addr().expect("read local TCP port").port()
}

/// The repo's real detector model fixture — staging it makes the node's
/// detector actually LOADABLE (advertisement requires an executable
/// capability), so the fabric worker loop can reach its serving state.
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

    fn stdout(&self) -> String {
        self.stdout.lock().expect("stdout lock").clone()
    }

    fn logs(&self) -> String {
        let out = self.stdout.lock().expect("stdout lock").clone();
        let err = self.stderr.lock().expect("stderr lock").clone();
        format!("{out}{err}")
    }
}

/// Spawn a real `vigil run` process: cameraless, a real loadable detector model
/// staged, the embedded hub role on, non-default OS-allocated ports.
/// `bringup_delay_ms`, when non-zero, makes fabric bring-up sleep that long
/// (test-only lever) so a slow ledger open is simulated deterministically.
fn spawn_hub(data_dir: &std::path::Path, health_port: u16, bringup_delay_ms: u64) -> Node {
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .env("VIGIL_DATA_DIR", data_dir)
        .env("VIGIL_HEALTH_PORT", health_port.to_string())
        .env("VIGIL_REVIEW_PORT", free_port().to_string())
        .env("VIGIL_DETECTOR_MODEL_PATH", fixture_model_path())
        .env("VIGIL_FABRIC_HUB", "true")
        .env("VIGIL_FABRIC_WORKER_SLOT_DEADLINE_MS", "20000")
        .env_remove("VIGIL_RTSP_URL")
        .env_remove("VIGIL_FABRIC_TICKET");
    if bringup_delay_ms > 0 {
        command.env(
            "VIGIL_FABRIC_BRINGUP_DELAY_MS",
            bringup_delay_ms.to_string(),
        );
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

fn wait_for_line(node: &Node, needle: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if node.stdout().contains(needle) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn health_becomes_ready_before_fabric_attaches() {
    assert!(
        fixture_model_path().is_file(),
        "detector model fixture missing at {}",
        fixture_model_path().display()
    );
    let data_dir = tempfile::tempdir().expect("tempdir");
    let health_port = free_port();
    // 4s bring-up delay: on the fixed (async-attach) path health is ready in
    // ~1s while fabric_ready appears ~4s later — a clean, non-flaky gap. On the
    // broken (synchronous) path readiness cannot appear until after bring-up, so
    // fabric_ready is already printed by the time health turns 200.
    let node = spawn_hub(data_dir.path(), health_port, 4000);

    // Poll frequently so we catch the instant /health first turns 200.
    let start = Instant::now();
    let mut health_ready = false;
    while start.elapsed() < Duration::from_secs(30) {
        if health_status(health_port) == Some(200) {
            health_ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    let logs_at_health = node.stdout();
    let fabric_ready_present = logs_at_health.contains("fabric_ready=true");

    // Confirm fabric actually attaches eventually, so the ordering assertion is
    // not vacuously satisfied by fabric never coming up.
    let fabric_eventually = wait_for_line(&node, "fabric_ready=true", Duration::from_secs(30));
    let final_logs = node.logs();
    node.kill_and_wait();

    assert!(
        health_ready,
        "/health never became ready within 30s. logs:\n{final_logs}"
    );
    assert!(
        fabric_eventually,
        "fabric never attached (no fabric_ready=true) — ordering test would be vacuous. logs:\n{final_logs}"
    );
    assert!(
        !fabric_ready_present,
        "/health turned 200 only AFTER fabric had already attached (fabric_ready=true was already \
         printed) — fabric bring-up is still on the readiness-critical path. logs at health-ready:\n{logs_at_health}"
    );
}

#[test]
fn boot_prints_phase_markers() {
    assert!(
        fixture_model_path().is_file(),
        "detector model fixture missing at {}",
        fixture_model_path().display()
    );
    let data_dir = tempfile::tempdir().expect("tempdir");
    let health_port = free_port();
    let node = spawn_hub(data_dir.path(), health_port, 0);

    let required = [
        "boot_phase=store-open-start",
        "boot_phase=store-open-done elapsed_ms=",
        "boot_phase=pipeline-up elapsed_ms=",
        "boot_phase=fabric-bringup-start",
        "boot_phase=fabric-bringup-done elapsed_ms=",
    ];
    // The store-open/pipeline markers land on the main boot thread while the
    // bring-up markers land on the detached fabric thread; poll until every
    // phase has been printed (or a generous timeout) rather than reading at a
    // single instant that could precede a slower main-thread phase.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(40) {
        let logs = node.stdout();
        if required.iter().all(|marker| logs.contains(marker)) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let logs = node.logs();
    node.kill_and_wait();

    for marker in required {
        assert!(
            logs.contains(marker),
            "boot must print the phase marker `{marker}` so a wedged boot names its phase; \
             it was absent. logs:\n{logs}"
        );
    }
}
