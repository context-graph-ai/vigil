//! Cross-machine capability DELIVERY (the S2 defect class): a worker whose
//! push to the hub misses — because the hub is merely running late, or
//! because it is never reachable at all — must not lose its capability
//! forever, and a permanent miss must be a NAMED, greppable failure rather
//! than silence.
//!
//! Diagnosed at `fabric.rs:674` (`let _ = client.push().await;`, the
//! `vigil-detector-<backend>` capability push) and the generic
//! `contextdb_server::work_ledger::run_worker_loop`'s own one-shot startup
//! push (`contextdb-server/src/work_ledger.rs:594`) it delegates into: once
//! either push exhausts its own internal retry budget
//! (`SyncClient::push` already retries a transient miss up to 5 attempts,
//! backing off 0/500/1000/1500/2000ms ~= 5s total), nothing in the standing
//! poll loop ever re-pushes the outstanding advertisement — the poll loop
//! only pushes as a side effect of claiming work, and a cameraless worker
//! with no submitted jobs never claims anything.
//!
//! `hub_up_late_still_sees_worker_capability`: the hub's own sync routing
//! comes up strictly AFTER the worker's startup push has exhausted that
//! internal retry budget — the worker must still end up advertising once
//! the hub can finally receive it. RED today.
//!
//! `failed_capability_push_emits_named_error`: the hub is never reachable
//! at all (a real, once-bound, now torn-down Iroh endpoint — a
//! syntactically valid ticket nobody will ever answer). The worker's own
//! log output must carry a NAMED, greppable line marking the capability
//! push as failed. RED today: totally silent.

#![cfg(feature = "fabric")]

use std::io::Read;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use contextdb_core::TenantId;
use contextdb_engine::Database;
use contextdb_engine::sync_types::{ConflictPolicies, ConflictPolicy};
use contextdb_server::{PeerEndpoint, SyncServer, peer_bind_spec};

use vigil::DecodedRgbFrame;
use vigil::detector_workclass::{DetectorDetection, OrderedF64};
use vigil::fabric::{FabricDetectorBackend, FabricRuntime};

/// `fabric.rs`'s own `FABRIC_TENANT` const is private and documented there
/// as a fixed, single-tenant value ("vigil-fabric") — every fabric node
/// enrolls under it, so a hand-assembled hub in this test must use the same
/// literal to land in the same tenant namespace.
const FABRIC_TENANT: &str = "vigil-fabric";

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

fn hub_capability_backend_for(db: &Database, node_id: &str) -> Option<String> {
    let result = db
        .execute(
            "SELECT node_id, capability_id FROM work_capabilities",
            &std::collections::HashMap::new(),
        )
        .ok()?;
    let node_idx = result.columns.iter().position(|c| c == "node_id")?;
    let capability_idx = result.columns.iter().position(|c| c == "capability_id")?;
    result.rows.iter().find_map(|row| {
        let contextdb_core::Value::Text(node) = &row[node_idx] else {
            return None;
        };
        if node != node_id {
            return None;
        }
        let contextdb_core::Value::Text(capability_id) = &row[capability_idx] else {
            return None;
        };
        capability_id
            .strip_prefix("vigil-detector-")
            .map(str::to_string)
    })
}

// ── T-late: the hub's sync routing arrives after the worker's own push has
// already exhausted its internal retry budget ──────────────────────────────

#[tokio::test]
async fn hub_up_late_still_sees_worker_capability() {
    let hub_dir = tempfile::tempdir().expect("hub identity dir");
    let hub_identity_path = hub_dir.path().join("fabric-identity.key");
    let bind_spec = peer_bind_spec(&hub_identity_path);
    // A REAL, reachable endpoint (the handshake succeeds) is bound right
    // now — its ticket is mintable immediately, independent of whether any
    // `SyncServer` is yet attached to route sync requests against it.
    let hub_endpoint = within(PeerEndpoint::bind(&bind_spec))
        .await
        .expect("bind the hub's real endpoint");
    let ticket = hub_endpoint.ticket();

    let worker_dir = tempfile::tempdir().expect("worker data dir");
    let worker_runtime = within(FabricRuntime::start(worker_dir.path(), Some(&ticket), false))
        .await
        .expect("worker must enroll standalone against a syntactically valid ticket");

    let backend = Arc::new(SpyBackend { tag: "burn-cpu" });
    let shutdown = Arc::new(AtomicBool::new(false));
    let _worker_task = worker_runtime.spawn_worker_loop(backend, Arc::clone(&shutdown));

    // Outlast `SyncClient::push`'s own internal retry budget (5 attempts,
    // backing off 0/500/1000/1500/2000ms ~= 5s) with slack, so the
    // swallowed-`Err` path (fabric.rs:674 and the generic worker loop's own
    // startup push) is what is under test here, not that built-in retry.
    tokio::time::sleep(Duration::from_millis(6_500)).await;

    // The hub's sync routing comes up only now.
    let hub_db = Arc::new(Database::open_memory());
    contextdb_engine::work_ledger::install_work_ledger_schema(&hub_db)
        .expect("install ledger schema on hub");
    let server = Arc::new(SyncServer::with_transport(
        hub_db.clone(),
        hub_endpoint.transport(),
        TenantId::from(FABRIC_TENANT),
        ConflictPolicies::uniform(ConflictPolicy::LatestWins),
    ));
    let server_shutdown = Arc::new(AtomicBool::new(false));
    let server_task = tokio::spawn({
        let server = server.clone();
        let server_shutdown = server_shutdown.clone();
        async move { server.run_until(server_shutdown).await }
    });

    let worker_node_id = worker_runtime.node_id.clone();
    within(async {
        loop {
            if hub_capability_backend_for(&hub_db, &worker_node_id).as_deref() == Some("burn-cpu")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    shutdown.store(true, Ordering::SeqCst);
    server_shutdown.store(true, Ordering::SeqCst);
    let _ = server_task.await;
}

// ── T-failed: the hub is never reachable — a named, greppable error, not
// silence ────────────────────────────────────────────────────────────────

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
        .env(
            "VIGIL_FABRIC_WORKER_SLOT_DEADLINE_MS",
            20_000.to_string(),
        )
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
