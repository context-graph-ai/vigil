//! Cross-machine capability delivery failure (the S2 defect class): a worker
//! whose push never reaches the hub must emit a named, greppable failure
//! rather than losing the capability silently.
//!
//! The hub-up-late delivery contract remains in `two_process_fabric.rs`, where
//! two real Vigil processes stay separated until both startup delivery
//! attempts have expired and the shipped doctor surface proves standing
//! re-delivery. A former 6.5-second test here was removed because routing
//! could return while ContextDB's second startup push was still live, so it
//! could pass without exercising the standing re-push it claimed to prove.

#![cfg(feature = "fabric")]

use std::io::Read;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use contextdb_server::{PeerEndpoint, peer_bind_spec};

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

struct Worker {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

impl Worker {
    fn kill_and_wait(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn combined_logs(&self) -> String {
        let out = self.stdout.lock().expect("stdout lock").clone();
        let err = self.stderr.lock().expect("stderr lock").clone();
        format!("{out}{err}")
    }
}

/// Mint a syntactically valid, permanently-dead fabric ticket: bind a real
/// Iroh endpoint on a throwaway single-purpose Tokio runtime, read its
/// ticket, then drop the whole runtime. Dropping a `tokio::runtime::Runtime`
/// forcibly tears down every task it was running — including the endpoint's
/// own accept loop — so the address the ticket names stops answering before
/// this function returns. No relay/publish/lookup is enabled (this repo's
/// default-off knobs), so nothing keeps the address reachable afterward.
fn mint_unreachable_ticket() -> String {
    let dir = tempfile::tempdir().expect("throwaway hub identity dir");
    let identity_path = dir.path().join("fabric-identity.key");
    let bind_spec = peer_bind_spec(&identity_path);
    let runtime = tokio::runtime::Runtime::new().expect("throwaway runtime");
    let ticket = runtime.block_on(async {
        let endpoint = PeerEndpoint::bind(&bind_spec)
            .await
            .expect("bind a real, throwaway endpoint to mint a ticket");
        endpoint.ticket()
    });
    drop(runtime);
    ticket
}

fn spawn_worker_against_dead_ticket(
    data_dir: &std::path::Path,
    health_port: u16,
    review_port: u16,
    ticket: &str,
    model_path: &std::path::Path,
) -> Worker {
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .env("VIGIL_DATA_DIR", data_dir)
        .env("VIGIL_HEALTH_PORT", health_port.to_string())
        .env("VIGIL_REVIEW_PORT", review_port.to_string())
        .env("VIGIL_FABRIC_TICKET", ticket)
        .env("VIGIL_DETECTOR_MODEL_PATH", model_path)
        .env("VIGIL_FABRIC_WORKER_SLOT_DEADLINE_MS", 20_000.to_string())
        .env_remove("VIGIL_RTSP_URL")
        .env_remove("VIGIL_FABRIC_HUB")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn vigil binary");
    let stdout = capture_pipe(child.stdout.take());
    let stderr = capture_pipe(child.stderr.take());
    Worker {
        child,
        stdout,
        stderr,
    }
}

fn health_status(port: u16) -> Option<u16> {
    use std::io::Write;
    use std::net::{SocketAddr, TcpStream};
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

#[test]
fn failed_capability_push_emits_named_error() {
    let model_path = fixture_model_path();
    assert!(
        model_path.is_file(),
        "detector model fixture must be present to stage a loadable model: {}",
        model_path.display()
    );
    let dead_ticket = mint_unreachable_ticket();

    let data_dir = tempfile::tempdir().expect("data dir");
    let health_port = free_port();
    let review_port = free_port();
    let worker = spawn_worker_against_dead_ticket(
        data_dir.path(),
        health_port,
        review_port,
        &dead_ticket,
        &model_path,
    );

    assert!(
        wait_for_health(health_port, Duration::from_secs(15)),
        "a worker enrolled against an unreachable hub must still reach a ready \
         /health — a dead hub is not a startup failure"
    );

    // Bounded wait for the named failure line the worker's own push attempt
    // must emit once it gives up on the unreachable hub.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut logs = String::new();
    while Instant::now() < deadline {
        logs = worker.combined_logs();
        if logs.contains("fabric_capability_push_failed") {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    assert!(
        health_status(health_port) == Some(200),
        "the runtime must still be alive after the failed push: {logs}"
    );

    worker.kill_and_wait();

    assert!(
        logs.contains("fabric_capability_push_failed"),
        "a worker whose capability push never reaches an unreachable hub must \
         say so with a named, greppable line (fabric_capability_push_failed) — \
         got: {logs}"
    );
}
