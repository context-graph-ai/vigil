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
//!   hub's process is DOWN, so both startup delivery attempts expire; the hub
//!   then restarts on the SAME data dir (identity + sticky port persisted
//!   beside it, so the issued ticket stays valid and reachable). The standing
//!   worker loop must re-push the outstanding advertisement, and the restarted
//!   hub's shipped doctor surface must render the capability.

#![cfg(feature = "fabric")]

use contextdb_engine::Database;
use contextdb_engine::work_ledger::advertise_capability;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use vigil::fabric::detector_capability_id;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{capture_pipe, free_port, vigil_binary_path, workspace_root};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis() as i64
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
        .arg("--health-port")
        .arg(health_port.to_string())
        .arg("--review-port")
        .arg(review_port.to_string())
        .arg("--detector-model-path")
        .arg(fixture_model_path())
        .env("VIGIL_DATA_DIR", data_dir)
        // No --fabric-worker-slot-deadline-ms CLI flag exists yet; left as
        // env per the settings-authority census (still no flag/config seam).
        //
        // Generous worker-slot deadline: a real checkpoint load in a debug
        // build needs headroom over the production wait before the cameraless
        // worker loop is declared started.
        .env("VIGIL_FABRIC_WORKER_SLOT_DEADLINE_MS", "20000")
        .env_remove("VIGIL_RTSP_URL")
        .env_remove("VIGIL_FABRIC_TICKET");
    if hub {
        command.arg("--fabric-hub").arg("true");
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
    let hub_health = free_port().expect("reserve a free port");
    let hub = spawn_node(
        hub_dir.path(),
        hub_health,
        free_port().expect("reserve a free port"),
        true,
        None,
    );
    assert!(
        wait_for_health(hub_health, Duration::from_secs(20)),
        "hub node must come up"
    );
    let ticket = wait_for_ticket(&hub, Duration::from_secs(20))
        .expect("hub must print its fabric-join ticket");

    // Worker enrolls with the hub already up: its one-shot startup push lands
    // immediately.
    let worker_dir = tempfile::tempdir().expect("worker data dir");
    let worker_health = free_port().expect("reserve a free port");
    let worker = spawn_node(
        worker_dir.path(),
        worker_health,
        free_port().expect("reserve a free port"),
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

    // Wait for the HUB's own worker role to come up and attempt its first
    // capability delivery (worker-slot deadline + one push attempt), so the
    // no-noise assertion below is exercised against a hub whose worker loop
    // genuinely ran — not vacuously green against a hub still in bring-up.
    let hub_loop_deadline = Instant::now() + Duration::from_secs(40);
    while Instant::now() < hub_loop_deadline && !hub.logs().contains("fabric_worker_loop_started") {
        thread::sleep(Duration::from_millis(500));
    }
    thread::sleep(Duration::from_secs(25));

    let hub_logs = hub.logs();
    worker.kill_and_wait();
    hub.kill_and_wait();

    assert!(
        seen,
        "with the hub up first, its own doctor acceleration must render \
         remote-detectors={worker_node_id}:burn-cpu once the worker joins; \
         last doctor output:\n{doctor_output}"
    );
    assert!(
        hub_logs.contains("fabric_worker_loop_started"),
        "the hub's own worker role must have started before the no-noise \
         assertion means anything; hub log:\n{hub_logs}"
    );
    // Operator honesty (fix cycle 8, option C): a HEALTHY hub must never
    // spend its life printing capability-push failures — the hub's writes
    // are already canonical in the shared ledger db, so its worker role has
    // nothing to push and must not try (a self-push is a category error,
    // and a permanently-failing loud line trains operators to ignore the
    // exact signal that matters on a real edge).
    assert!(
        !hub_logs.contains("fabric_capability_push_failed"),
        "a healthy hub's worker role must not emit capability-push failure \
         noise — its ledger writes are already canonical; hub log:\n{hub_logs}"
    );
}

// ── Arm B: hub up LATE (standing re-delivery guard) ───────────────────────

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
    let hub_health = free_port().expect("reserve a free port");
    let hub_review = free_port().expect("reserve a free port");
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
    let worker_health = free_port().expect("reserve a free port");
    let worker = spawn_node(
        worker_dir.path(),
        worker_health,
        free_port().expect("reserve a free port"),
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
        wait_for_line(
            &worker,
            "fabric_worker_loop_started=true",
            Duration::from_secs(20)
        ),
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
    // ONLY thing that can now deliver the capability is a re-push on the
    // standing poll cadence — the exact recovery contract under test.
    thread::sleep(Duration::from_secs(55));

    // The hub's routing finally comes up, on the SAME data dir → same sticky
    // port → the worker's ticket is now reachable.
    let hub_second = spawn_node(
        hub_dir.path(),
        free_port().expect("reserve a free port"),
        free_port().expect("reserve a free port"),
        true,
        None,
    );
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

/// The fabric ledger db path under a node's data dir — `<data_dir>/fabric/
/// fabric-ledger.db` (see `runtime.rs`'s `config.data_dir.join("fabric")` and
/// `FabricRuntime::start`'s `data_dir.join("fabric-ledger.db")`).
fn fabric_ledger_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("fabric").join("fabric-ledger.db")
}

/// One read of a node's `work_capabilities` capability ids straight from a
/// ledger db file. `Err` distinguishes "could not open/read the db" (a
/// transient just after the owner process exits — the redb lock/file settles)
/// from "opened cleanly, row simply absent". The db is single-writer locked
/// cross-process (redb), so the caller must read only when the owning process
/// is DOWN (e.g. after `hub.kill_and_wait()`).
fn read_ledger_capability_ids(
    data_dir: &std::path::Path,
    node_id: &str,
) -> Result<Vec<String>, String> {
    let db = Database::open(fabric_ledger_path(data_dir)).map_err(|err| format!("open: {err}"))?;
    let ids = db
        .execute(
            "SELECT node_id, capability_id FROM work_capabilities",
            &std::collections::HashMap::new(),
        )
        .map_err(|err| format!("query: {err}"))
        .map(|result| {
            let node_idx = result.columns.iter().position(|c| c == "node_id");
            let cap_idx = result.columns.iter().position(|c| c == "capability_id");
            let mut ids = Vec::new();
            if let (Some(node_idx), Some(cap_idx)) = (node_idx, cap_idx) {
                for row in &result.rows {
                    if let (contextdb_core::Value::Text(node), contextdb_core::Value::Text(cap)) =
                        (&row[node_idx], &row[cap_idx])
                        && node == node_id
                    {
                        ids.push(cap.clone());
                    }
                }
            }
            ids
        });
    let _ = db.close();
    ids
}

/// Poll a node's ledger `work_capabilities` (opening the DOWN process's db
/// fresh each attempt) until `capability_id` is physically present, returning
/// the last capability-id list seen (or the last open/read error) for the
/// failure message. Retries ride out both the transient post-exit db-open
/// window and any persistence lag; a genuinely-never-arriving row still fails
/// after the timeout.
fn poll_ledger_for_capability(
    data_dir: &std::path::Path,
    node_id: &str,
    capability_id: &str,
    timeout: Duration,
) -> (bool, String) {
    let start = Instant::now();
    let mut last = "no read attempted".to_string();
    while start.elapsed() < timeout {
        match read_ledger_capability_ids(data_dir, node_id) {
            Ok(ids) => {
                if ids.iter().any(|id| id == capability_id) {
                    return (true, format!("{ids:?}"));
                }
                last = format!("opened ok; node capability ids = {ids:?}");
            }
            Err(err) => last = err,
        }
        thread::sleep(Duration::from_millis(300));
    }
    (false, last)
}

/// Poll the hub's `vigil doctor acceleration` until its rendered
/// `remote-detectors=` NO LONGER names the worker with any backend, returning
/// the last output seen.
fn poll_hub_until_worker_absent(
    hub_data_dir: &std::path::Path,
    worker_node_id: &str,
    timeout: Duration,
) -> (bool, String) {
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < timeout {
        last = run_doctor_acceleration(hub_data_dir);
        if !last.contains(worker_node_id) {
            return (true, last);
        }
        thread::sleep(Duration::from_millis(500));
    }
    (false, last)
}

// ── Arm C: restart onto a different backend — hub renders ONLY current ─────

/// A hub that holds BOTH a node's stale prior-era backend row and its current
/// one must render the node as ONLY its current backend — the stale sibling
/// must never appear alongside it (the S4 defect: a `burn-wgpu` era row left
/// behind after the worker restarted acceleration-off onto `burn-cpu`). This
/// proves the currency collapse over two REAL processes and the real render
/// chain, not just in-process.
///
/// A CPU test box cannot TRUTHFULLY produce a `burn-wgpu` backend — the
/// advertised tag must reflect REAL capability (criterion C3/C7), never a
/// forced label — so the stale GPU-era row is injected directly into the hub's
/// ledger (at an OLD `advertised_at`, with the exact tags production's
/// `spawn_worker_loop` writes) while the hub is briefly DOWN; its redb ledger
/// is single-writer locked cross-process while it runs. Injecting at the hub —
/// rather than at a restarted worker whose `changes_since` only replays its own
/// session's writes and so would never re-push a prior-session row — makes the
/// stale row's presence at the hub DETERMINISTIC: the restarted hub loads and
/// keeps it, and the still-running worker's fresh `burn-cpu` re-advertisement
/// gives the hub both rows. The post-kill ledger read then proves the stale row
/// was physically present, so the collapse assertion is never vacuous.
#[test]
fn two_real_processes_render_only_the_restarted_workers_current_backend() {
    assert!(
        fixture_model_path().is_file(),
        "detector model fixture must be present: {}",
        fixture_model_path().display()
    );

    let hub_dir = tempfile::tempdir().expect("hub data dir");
    let hub_health = free_port().expect("reserve a free port");
    let hub = spawn_node(
        hub_dir.path(),
        hub_health,
        free_port().expect("reserve a free port"),
        true,
        None,
    );
    assert!(
        wait_for_health(hub_health, Duration::from_secs(20)),
        "hub node must come up"
    );
    let ticket = wait_for_ticket(&hub, Duration::from_secs(20))
        .expect("hub must print its fabric-join ticket");

    let worker_dir = tempfile::tempdir().expect("worker data dir");
    let worker_health = free_port().expect("reserve a free port");
    let worker = spawn_node(
        worker_dir.path(),
        worker_health,
        free_port().expect("reserve a free port"),
        false,
        Some(&ticket),
    );
    assert!(
        wait_for_health(worker_health, Duration::from_secs(20)),
        "worker node must reach a ready /health"
    );
    let worker_node_id =
        wait_for_node_id(&worker, Duration::from_secs(20)).expect("worker must print its node id");

    // Control: the live worker's real backend renders end to end.
    let (seen_cpu, out_first) = poll_hub_for_remote_worker(
        hub_dir.path(),
        &worker_node_id,
        "burn-cpu",
        Duration::from_secs(45),
    );
    assert!(
        seen_cpu,
        "the hub must first render the worker's real burn-cpu; last doctor output:\n{out_first}"
    );

    // Take the HUB down and inject the stale GPU-era row a prior era would have
    // left in its ledger for this node (OLD advertised_at, production tags).
    hub.kill_and_wait();
    thread::sleep(Duration::from_millis(500));
    {
        let db = Database::open(fabric_ledger_path(hub_dir.path()))
            .expect("open the down hub's ledger to inject the stale era row");
        advertise_capability(
            &db,
            &worker_node_id,
            &detector_capability_id("burn-wgpu"),
            &[
                "class:vigil.detector".to_string(),
                "backend:burn-wgpu".to_string(),
            ],
            now_ms() - 3_600_000,
        )
        .expect("inject the stale burn-wgpu era capability row at the hub");
        db.close().expect("flush and close the injected hub ledger");
    }

    // Restart the hub on the SAME data dir (sticky port ⇒ the worker's ticket
    // stays reachable and it reconnects), now carrying the stale burn-wgpu row.
    let hub = spawn_node(
        hub_dir.path(),
        free_port().expect("reserve a free port"),
        free_port().expect("reserve a free port"),
        true,
        None,
    );
    assert!(
        wait_for_health_any(&hub, Duration::from_secs(20)),
        "the restarted hub must come back up: {}",
        hub.logs()
    );

    // The still-running worker reconnects and re-advertises its fresh burn-cpu;
    // the hub now holds BOTH the old burn-wgpu and the fresh burn-cpu and must
    // render ONLY the current backend (currency collapse over two processes).
    let (seen_cpu2, out_second) = poll_hub_for_remote_worker(
        hub_dir.path(),
        &worker_node_id,
        "burn-cpu",
        Duration::from_secs(45),
    );
    let worker_logs = worker.logs();
    worker.kill_and_wait();
    hub.kill_and_wait();

    // NON-VACUITY ANCHOR: with the hub down (ledger lock released), read its
    // work_capabilities directly and require the stale burn-wgpu row to be
    // PHYSICALLY present — so the collapse below is asserted against a hub that
    // genuinely HELD both the stale and the current row, not a timing window.
    // Deterministic here: it was injected straight into the hub and persisted
    // across the restart.
    let (wgpu_present, wgpu_diag) = poll_ledger_for_capability(
        hub_dir.path(),
        &worker_node_id,
        &detector_capability_id("burn-wgpu"),
        Duration::from_secs(15),
    );
    assert!(
        wgpu_present,
        "the injected stale burn-wgpu row must be physically present in the \
         hub's work_capabilities — otherwise the collapse assertion is vacuous; \
         last hub-ledger read: {wgpu_diag}"
    );

    assert!(
        seen_cpu2,
        "the hub must render the worker's current burn-cpu; last doctor output:\n{out_second}\n\nworker logs:\n{worker_logs}"
    );
    assert!(
        !hub_renders_remote_worker(&out_second, &worker_node_id, "burn-wgpu"),
        "the stale burn-wgpu era row must NOT render alongside the current \
         burn-cpu — the hub collapses a node's rows to its latest advertised \
         backend; hub doctor output:\n{out_second}"
    );
    assert_eq!(
        out_second
            .matches(&format!("{worker_node_id}:burn-cpu"))
            .count(),
        1,
        "the node must render EXACTLY ONCE, as its current burn-cpu — not \
         duplicated and not once per era; hub doctor output:\n{out_second}"
    );
}

// ── Arm D: dead worker ages out of the hub's rendered remote-detectors ─────

/// A worker that joins and then stops contacting the hub must AGE OUT of the
/// hub's rendered `remote-detectors=` line within the liveness TTL — its
/// capability row lingers, but the hub stops offering a node it has not heard
/// from. Proven over two REAL processes: the hub's per-node last-contact clock
/// (advanced on every served exchange) freezes when the worker dies, and past
/// the TTL the shared live/current set drops it.
#[test]
fn two_real_processes_age_out_a_dead_worker_from_remote_detectors() {
    assert!(
        fixture_model_path().is_file(),
        "detector model fixture must be present: {}",
        fixture_model_path().display()
    );

    let hub_dir = tempfile::tempdir().expect("hub data dir");
    let hub_health = free_port().expect("reserve a free port");
    let hub = spawn_node(
        hub_dir.path(),
        hub_health,
        free_port().expect("reserve a free port"),
        true,
        None,
    );
    assert!(
        wait_for_health(hub_health, Duration::from_secs(20)),
        "hub node must come up"
    );
    let ticket = wait_for_ticket(&hub, Duration::from_secs(20))
        .expect("hub must print its fabric-join ticket");

    let worker_dir = tempfile::tempdir().expect("worker data dir");
    let worker_health = free_port().expect("reserve a free port");
    let worker = spawn_node(
        worker_dir.path(),
        worker_health,
        free_port().expect("reserve a free port"),
        false,
        Some(&ticket),
    );
    assert!(
        wait_for_health(worker_health, Duration::from_secs(20)),
        "worker node must reach a ready /health"
    );
    let worker_node_id =
        wait_for_node_id(&worker, Duration::from_secs(20)).expect("worker must print its node id");

    // The worker is rendered while alive (its pushes keep its last-contact
    // fresh).
    let (seen, out_alive) = poll_hub_for_remote_worker(
        hub_dir.path(),
        &worker_node_id,
        "burn-cpu",
        Duration::from_secs(45),
    );
    assert!(
        seen,
        "the live worker must first render on the hub; last doctor output:\n{out_alive}"
    );

    // Kill the worker: its last-contact freezes and its poll-cadence re-push
    // stops. Wait past the default liveness TTL
    // (`DEFAULT_CAPABILITY_LIVENESS_TTL_MS` = 10s) with margin.
    worker.kill_and_wait();
    thread::sleep(Duration::from_secs(13));

    let (absent, out_dead) =
        poll_hub_until_worker_absent(hub_dir.path(), &worker_node_id, Duration::from_secs(20));
    hub.kill_and_wait();

    assert!(
        absent,
        "a worker that stopped contacting the hub must age out of the rendered \
         remote-detectors within the TTL, not linger forever; last doctor \
         output still naming {worker_node_id}:\n{out_dead}"
    );
}
