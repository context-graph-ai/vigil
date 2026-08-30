//! Loud non-serving worker intent (owner steer 2026-07-13: "a remote node
//! started with detector/decode enabled must USE that ... worker service
//! must not depend on the node owning cameras"; and: "silent-standalone
//! continuation may also have swallowed a real enrollment error — C6
//! requires NAMED errors").
//!
//! A node with worker intent (a ticket configured, detection enabled) that
//! ends up NOT actually serving the fleet must say so, loudly, on its OWN
//! `vigil doctor`/`vigil stats` rendering — not just in a once-at-startup
//! stdout line nobody is tailing 60s later. Today, `fabric-status=` (the
//! ONE rendered vocabulary line every surface prints — offload_policy.rs:286)
//! hardcodes `enrolled=true` the instant `FabricRuntime::start` succeeds
//! (fabric.rs:1054), regardless of whether this node ever actually joined a
//! hub or ever started its worker loop — so a node with a malformed ticket,
//! or one stuck in the cameraless not-started state, reports EXACTLY the
//! same `fabric-status=enrolled=true role=edge remote-detectors=- in-use=false`
//! as a node that is fine and simply idle. `enrolled=true` alone cannot be
//! read as "this node is serving" — this test pins that a distinct,
//! greppable not-serving signal must exist on the same rendered surface.
//!
//! Drives the real compiled `vigil` binary, so it lives alongside the
//! composition root that produces it. The empty-ticket-normalization arm of
//! this same diagnosis needs no process at all (it calls
//! `vigil::fabric_intent_from_args` directly) and stays in
//! `crates/vigil/tests/worker_intent_loudness.rs`.

#![cfg(feature = "fabric")]

use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{capture_pipe, free_port, vigil_binary_path, wait_until};

fn health_status(port: u16) -> Option<u16> {
    use std::io::{Read, Write};
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

/// Run `vigil doctor acceleration` against an already-running node's data
/// dir (the exact surface `haos-test-environment-smoke-instructions.md`
/// polls) and return its combined stdout.
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

#[test]
fn worker_intent_present_but_not_serving_gets_a_named_greppable_line_on_its_own_doctor_rendering() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let health_port = free_port().expect("reserve a free port");

    // Worker intent present (a ticket is configured) + detection enabled +
    // zero cameras: today this node never starts its fabric worker loop
    // (the cameraless bootstrap gap), so it never actually serves — yet
    // its enrollment intent is real and it must say why serving didn't
    // happen, not just look identically healthy to a fine, idle node.
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .arg("--health-port")
        .arg(health_port.to_string())
        .arg("--fabric-hub")
        .arg("false")
        .env("VIGIL_DATA_DIR", data_dir.path())
        .env("VIGIL_FABRIC_TICKET", "not-a-real-ticket")
        .env_remove("VIGIL_RTSP_URL")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn vigil binary");
    let stdout = capture_pipe(child.stdout.take());
    let stderr = capture_pipe(child.stderr.take());

    assert!(
        wait_for_health(health_port, Duration::from_secs(15)),
        "the node must still come up standalone despite the malformed ticket (criterion C6)"
    );

    // Bounded wait for the not-started line this node emits once its own
    // enrollment fails, so the persisted stats snapshot doctor reads has
    // settled. The bound is the test's liveness guard, not the outcome.
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let logs = stdout.lock().expect("stdout lock").clone();
        if logs.contains("fabric_worker_loop_not_started=true") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let doctor_output = doctor_once_fabric_status_settles(data_dir.path());

    let _ = child.kill();
    let _ = child.wait();
    let stderr_logs = stderr.lock().expect("stderr lock").clone();
    let _ = stderr_logs; // diagnostic only, not asserted on

    assert!(
        doctor_output.contains("enrolled=true"),
        "this node's own fabric-status line reports enrolled=true (a ticket \
         was configured) — the healthy-looking half of the gap this test \
         pins: doctor output was: {doctor_output}"
    );
    assert!(
        doctor_output.contains("fabric-worker-serving=false"),
        "enrolled=true alone must not be read as \"this node is serving\" — \
         its own doctor/status rendering must ALSO carry a distinct, \
         greppable not-serving line (e.g. fabric-worker-serving=false \
         reason=...) whenever this node has worker intent but never \
         actually started its fabric worker loop; doctor output was: {doctor_output}"
    );
}
