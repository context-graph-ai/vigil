//! Owner ruling: the fabric enrollment ticket is a credential. Nothing
//! legitimate consuming `/health` needs it — the Home Assistant Supervisor
//! watchdog checks HTTP status only — so a credential on a served surface is
//! the defect this pins closed. Retrieval is confined to two DELIBERATE
//! local acts — the startup log instruction and `vigil fabric ticket` (see
//! `fabric_ticket_command.rs`) — never `vigil doctor`, `vigil stats`, the
//! persisted `runtime-stats.json` file, or this served `/health` body (see
//! `fabric_ticket_single_retrieval_path.rs` for the comprehensive
//! "appears nowhere else" pin across all of those surfaces at once).
//!
//! This is a BEHAVIORAL pin, not a structural one: it drives the real
//! composed `vigil` binary end to end — a hub-role fabric node's own raw
//! `/health` HTTP responder, never a helper reimplementing that logic — and
//! asserts the served body excludes the ticket marker. A structural
//! guarantee (a type incapable of carrying the ticket threaded into the
//! `/health` responder, the same shape `crates/vigil/src/secret.rs`'s
//! `Secret` uses elsewhere in this codebase) is the stronger fix and is left
//! to the implementation pass; test-authoring here does not restructure
//! `crate::health`'s production wiring.
//!
//! Never formats the real ticket value into any assertion or panic message
//! (only its byte length), so a failing run cannot leak the credential into
//! CI logs.

#![cfg(feature = "fabric")]

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, TcpPortReservation, capture_pipe, get, vigil_binary_path, wait_until,
};

/// Redact any `ticket=<value>` field down to its byte length before a raw
/// captured-log blob is ever formatted into a panic message. This test's
/// whole point is proving the credential is ABSENT from `/health` — its own
/// failure output must not become a second place the credential leaks (e.g.
/// into CI logs) via an unrelated assertion (boot timeout, non-2xx status)
/// that happens to dump the hub's full captured stdout, which legitimately
/// carries the real ticket on its sanctioned startup log line.
fn redact_ticket(logs: &str) -> String {
    let mut out = String::new();
    for line in logs.lines() {
        match line.find("ticket=") {
            Some(idx) => {
                let prefix = &line[..idx];
                let rest = &line[idx + "ticket=".len()..];
                let (value, suffix) = match rest.split_once(' ') {
                    Some((value, suffix)) => (value, format!(" {suffix}")),
                    None => (rest, String::new()),
                };
                out.push_str(prefix);
                out.push_str(&format!("ticket=<redacted:{}b>", value.len()));
                out.push_str(&suffix);
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

struct Node {
    child: Child,
}

impl Node {
    fn kill_and_wait(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Pull the ticket value out of a printed `fabric-join ticket=<ticket>
/// command=...` line, without ever formatting the whole line (which
/// contains the credential) into a panic message.
fn extract_ticket(join_line: &str) -> Option<String> {
    let after_marker = join_line.split_once("ticket=")?.1;
    let ticket = after_marker
        .split_once(" command=")
        .map(|(ticket, _)| ticket)
        .unwrap_or(after_marker);
    let trimmed = ticket.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[test]
fn health_body_never_carries_the_fabric_enrollment_ticket() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let health_reservation = TcpPortReservation::reserve_loopback().expect("reserve health port");
    let review_reservation = TcpPortReservation::reserve_loopback().expect("reserve review port");
    let health_port = health_reservation.port();
    let review_port = review_reservation.port();

    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .arg("--health-port")
        .arg(health_port.to_string())
        .arg("--review-port")
        .arg(review_port.to_string())
        .arg("--fabric-hub")
        .arg("true")
        .env("VIGIL_DATA_DIR", data_dir.path())
        .env_remove("VIGIL_RTSP_URL")
        .env_remove("VIGIL_FABRIC_TICKET")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    health_reservation.release();
    review_reservation.release();
    let mut child = command.spawn().expect("spawn vigil binary");
    let stdout: Arc<Mutex<String>> = capture_pipe(child.stdout.take());
    let _stderr = capture_pipe(child.stderr.take());
    let node = Node { child };

    let boot = wait_until(
        "the hub node to report boot_phase=pipeline-up",
        RUNTIME_STARTUP_TIMEOUT,
        || {
            Ok(stdout
                .lock()
                .expect("stdout lock")
                .contains("boot_phase=pipeline-up")
                .then_some(()))
        },
    );
    if let Err(error) = boot {
        let logs = redact_ticket(&stdout.lock().expect("stdout lock"));
        node.kill_and_wait();
        panic!("{error}; logs so far:\n{logs}");
    }

    // Non-vacuous positive control: this node's OWN startup log — one of
    // the two surfaces still sanctioned to carry the ticket — must
    // genuinely have printed it, proving fabric really enrolled and a real
    // ticket exists this run, never that fabric simply never got
    // configured.
    let join_line_result = wait_until(
        "the hub node to print its startup fabric-join ticket line",
        RUNTIME_STARTUP_TIMEOUT,
        || {
            let logs = stdout.lock().expect("stdout lock").clone();
            Ok(logs
                .lines()
                .find(|line| line.starts_with("fabric-join "))
                .map(str::to_string))
        },
    );
    let join_line = match join_line_result {
        Ok(line) => line,
        Err(error) => {
            let logs = redact_ticket(&stdout.lock().expect("stdout lock"));
            node.kill_and_wait();
            panic!("{error}; logs so far:\n{logs}");
        }
    };
    let advertised_ticket = match extract_ticket(&join_line) {
        Some(ticket) => ticket,
        None => {
            node.kill_and_wait();
            panic!(
                "sanity: the sanctioned startup log must genuinely carry a ticket= field before \
                 this test can prove /health omits it"
            );
        }
    };

    let response = get(health_port, "/health");
    let body = response.body_text();
    let body_length = body.len();
    let boot_logs = redact_ticket(&stdout.lock().expect("stdout lock"));
    node.kill_and_wait();

    assert_eq!(
        response.status, 200,
        "a hub-role fabric node is alive and must still answer the liveness probe with 2xx; \
         logs:\n{boot_logs}"
    );
    assert!(
        !body.contains(&advertised_ticket),
        "the /health body must never carry the fabric enrollment ticket — a credential on a \
         served HTTP surface — even though the sanctioned startup log above genuinely carried it \
         this run; got a /health body of length {body_length}"
    );
    assert!(
        !body.contains("ticket="),
        "the /health body must never carry a `ticket=` marker of any kind; got a /health body of \
         length {body_length}"
    );
    assert!(
        !body.contains("fabric-join"),
        "the /health body must not carry the fabric join instruction line at all (the line \
         that names the ticket and the exact command to use it), got a /health body of length \
         {body_length}"
    );
    assert!(
        body.contains("fabric-status="),
        "removing the ticket must not remove the non-secret fabric-status summary line from \
         /health — only the credential-bearing join line — got a /health body of length \
         {body_length}"
    );
}
