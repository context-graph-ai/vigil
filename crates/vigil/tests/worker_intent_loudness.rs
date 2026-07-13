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
//! Also pins a second, related honesty gap named in the diagnosis: the
//! add-on options→env mapping can hand `FabricRuntime::start` an EMPTY
//! ticket string (`Some("")`, from a blank options.json field) rather than
//! `None`. `validate_fabric_ticket("")` correctly calls that malformed (an
//! operator-typed empty string), but a field the operator never touched at
//! all must never be treated as if they typed something and got it wrong —
//! `vigil::fabric_intent_from_args` (the exact resolution `vigil run` does)
//! must normalize an empty ticket to no ticket configured.

#![cfg(feature = "fabric")]

use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
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

fn health_status(port: u16) -> Option<u16> {
    use std::io::Write;
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

#[test]
fn worker_intent_present_but_not_serving_gets_a_named_greppable_line_on_its_own_doctor_rendering() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let health_port = free_port();

    // Worker intent present (a ticket is configured) + detection enabled +
    // zero cameras: today this node never starts its fabric worker loop
    // (the cameraless bootstrap gap), so it never actually serves — yet
    // its enrollment intent is real and it must say why serving didn't
    // happen, not just look identically healthy to a fine, idle node.
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .env("VIGIL_DATA_DIR", data_dir.path())
        .env("VIGIL_HEALTH_PORT", health_port.to_string())
        .env("VIGIL_FABRIC_HUB", "false")
        .env("VIGIL_FABRIC_TICKET", "not-a-real-ticket")
        .env("VIGIL_FABRIC_WORKER_SLOT_DEADLINE_MS", "1500")
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

    // Bounded wait past the shortened worker-slot deadline so the persisted
    // stats snapshot doctor reads has settled.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let logs = stdout.lock().expect("stdout lock").clone();
        if logs.contains("fabric_worker_loop_not_started=true") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(Duration::from_millis(500));

    let doctor_output = run_doctor_acceleration(data_dir.path());

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

#[test]
fn empty_string_ticket_from_the_addon_options_mapping_is_treated_as_no_ticket_configured() {
    // Serializes on process env — this crate's other env-driven config
    // tests carry the same caveat; run this file with --test-threads=1 if
    // run alongside anything else that touches VIGIL_FABRIC_TICKET/
    // VIGIL_FABRIC_HUB/VIGIL_DATA_DIR.
    let data_dir = tempfile::tempdir().expect("data dir");
    unsafe {
        std::env::set_var("VIGIL_DATA_DIR", data_dir.path());
        std::env::set_var("VIGIL_FABRIC_TICKET", "");
        std::env::remove_var("VIGIL_FABRIC_HUB");
    }

    let intent =
        vigil::fabric_intent_from_args(vec![]).expect("fabric intent must resolve from env");

    unsafe {
        std::env::remove_var("VIGIL_DATA_DIR");
        std::env::remove_var("VIGIL_FABRIC_TICKET");
    }

    assert_eq!(
        intent.fabric_ticket, None,
        "an empty-string ticket (a blank HAOS options.json field, mapped to \
         VIGIL_FABRIC_TICKET=\"\") must resolve to no ticket configured, the \
         same as the operator never having touched the field — never a \
         'ticket rejected'/'ticket is empty' enrollment error, which is for \
         an operator who actually typed a malformed value; got: {:?}",
        intent.fabric_ticket
    );
}
