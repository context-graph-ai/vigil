//! Owner ruling: retrieving the fabric enrollment ticket must become a
//! deliberate local act — `vigil fabric ticket` — rather than something
//! served to any reader of an HTTP endpoint (see the companion omission
//! pin, `fabric_ticket_health_exposure.rs`).
//!
//! This drives the real compiled `vigil` binary end to end, mirroring
//! `crates/vigil-bin/tests/two_process_fabric.rs`'s reliance on the SAME
//! persisted identity + sticky port across two separate process
//! invocations: a hub-role node prints its own `fabric-join
//! ticket=<ticket> command=...` line on the sanctioned startup log (kept by
//! this same ruling), then exits; a later, separate `vigil fabric ticket`
//! invocation against the identical data directory must print that exact
//! same ticket, proving the new command reads the SAME on-disk identity the
//! running node advertised rather than fabricating a decorative value.
//!
//! Command name `vigil fabric ticket` is provisional pending the owner's
//! confirmation; every place the literal string appears is named in this
//! pass's report.

#![cfg(feature = "fabric")]

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_until,
};

/// Redact any `ticket=<value>` field down to its byte length before a raw
/// captured-log blob is ever formatted into a panic message, so a timing
/// failure here (the join line not yet observed) cannot leak the real
/// credential into CI output via a race against a line that arrived just
/// after the last poll.
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

fn run_fabric_ticket(data_dir: &std::path::Path) -> (bool, String, usize) {
    let output = Command::new(vigil_binary_path())
        .arg("fabric")
        .arg("ticket")
        .arg("--data-dir")
        .arg(data_dir)
        .stdin(Stdio::null())
        .output()
        .expect("spawn vigil fabric ticket");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let length = combined.len();
    (output.status.success(), combined, length)
}

#[test]
fn fabric_ticket_command_prints_the_same_ticket_the_running_hub_advertised() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let health_reservation = TcpPortReservation::reserve_loopback().expect("reserve health port");
    let review_reservation = TcpPortReservation::reserve_loopback().expect("reserve review port");
    let health_port = health_reservation.port();
    let review_port = review_reservation.port();

    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .env("VIGIL_DATA_DIR", data_dir.path())
        .env("VIGIL_HEALTH_PORT", health_port.to_string())
        .env("VIGIL_REVIEW_PORT", review_port.to_string())
        .env("VIGIL_FABRIC_HUB", "true")
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

    let join_line_result = wait_until(
        "the hub node to print its startup fabric-join ticket line",
        Duration::from_secs(20),
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
            let _ = child.kill();
            let _ = child.wait();
            panic!("{error}; logs so far:\n{logs}");
        }
    };
    let advertised_ticket = extract_ticket(&join_line)
        .unwrap_or_else(|| panic!("the printed startup join line must carry a ticket= field"));

    let _ = child.kill();
    let _ = child.wait();

    // The standalone ticket command must be able to bind against the SAME
    // persisted identity once the running node has released its port —
    // exactly the restart precondition `two_process_fabric.rs`'s
    // `..._hub_up_late` arm already depends on.
    let port_released = wait_until(
        "the hub's health port to release after shutdown",
        Duration::from_secs(10),
        || {
            Ok(std::net::TcpStream::connect(("127.0.0.1", health_port))
                .is_err()
                .then_some(()))
        },
    );
    if let Err(error) = port_released {
        panic!("{error}");
    }

    let (success, combined_output, output_length) = run_fabric_ticket(data_dir.path());
    assert!(
        success,
        "vigil fabric ticket must succeed for a data dir with an existing fabric identity \
         (output length {output_length})"
    );
    assert!(
        combined_output.contains(&advertised_ticket),
        "vigil fabric ticket must print the SAME ticket the running hub node's startup log \
         advertised for this node's identity; got an output that did not contain the expected \
         ticket (output length {output_length}, expected ticket length {})",
        advertised_ticket.len()
    );
}

/// The only situation an operator is ever actually in: Vigil already
/// running, needing its ticket to hand to a second machine. Unlike
/// `fabric_ticket_command_prints_the_same_ticket_the_running_hub_advertised`
/// above (which kills the node BEFORE invoking the command, so it never
/// proves retrieval works while Vigil is up), this keeps the hub node alive
/// for the whole `vigil fabric ticket` invocation.
#[test]
fn fabric_ticket_command_prints_the_same_ticket_while_the_hub_node_is_still_running() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let health_reservation = TcpPortReservation::reserve_loopback().expect("reserve health port");
    let review_reservation = TcpPortReservation::reserve_loopback().expect("reserve review port");
    let health_port = health_reservation.port();
    let review_port = review_reservation.port();

    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .env("VIGIL_DATA_DIR", data_dir.path())
        .env("VIGIL_HEALTH_PORT", health_port.to_string())
        .env("VIGIL_REVIEW_PORT", review_port.to_string())
        .env("VIGIL_FABRIC_HUB", "true")
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

    let join_line_result = wait_until(
        "the hub node to print its startup fabric-join ticket line",
        Duration::from_secs(20),
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
            let _ = child.kill();
            let _ = child.wait();
            panic!("{error}; logs so far:\n{logs}");
        }
    };
    let advertised_ticket = extract_ticket(&join_line)
        .unwrap_or_else(|| panic!("the printed startup join line must carry a ticket= field"));

    // Deliberately NOT killed: this is the load-bearing difference from the
    // sibling test above. The `vigil fabric ticket` invocation below runs
    // against a data dir whose fabric identity is bound and held by this
    // still-running process.
    let (success, combined_output, output_length) = run_fabric_ticket(data_dir.path());

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        success,
        "vigil fabric ticket must succeed against a data dir whose fabric identity is currently \
         held by a running vigil process — this is the only situation an operator is ever \
         actually in (output length {output_length})"
    );
    assert!(
        combined_output.contains(&advertised_ticket),
        "vigil fabric ticket, run WHILE the hub node is still up, must print the SAME ticket the \
         running node's own startup log advertised; got an output that did not contain the \
         expected ticket (output length {output_length}, expected ticket length {})",
        advertised_ticket.len()
    );
}

/// The cache fallback exists for exactly ONE failure class — a running
/// node holding this identity's fabric ledger lock. A different failure
/// (here: a corrupt identity file, which fails at `FabricIdentity::
/// load_or_generate` before the ledger is ever opened) must surface
/// honestly rather than being masked by whatever happens to be sitting in
/// the cache file, even when a cache file exists and is well-formed.
#[test]
fn fabric_ticket_command_surfaces_a_non_lock_failure_instead_of_masking_it_with_a_stale_cache() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let fabric_dir = data_dir.path().join("fabric");
    std::fs::create_dir_all(&fabric_dir).expect("create fabric dir");

    // `FabricIdentity::load_or_generate` requires exactly 32 secret-seed
    // bytes; this is corrupt on its face and unrelated to any lock
    // contention.
    std::fs::write(fabric_dir.join("fabric-identity.key"), b"not-32-bytes")
        .expect("write corrupt identity file");

    // Plant a cache file carrying an obviously-synthetic value this binary
    // never generated, so a masked failure is directly observable: if the
    // command wrongly fell back to it, this exact string would appear in
    // its output.
    let planted = "synthetic-non-live-ticket-marker-should-never-be-printed";
    std::fs::write(fabric_dir.join("own-ticket"), planted).expect("plant cache file");

    let (success, combined_output, output_length) = run_fabric_ticket(data_dir.path());

    assert!(
        !success,
        "vigil fabric ticket must FAIL loudly on a corrupt identity file — a failure class \
         unrelated to a running node holding the ledger lock — rather than silently returning \
         the planted cache value (output length {output_length})"
    );
    assert!(
        !combined_output.contains(planted),
        "a non-lock-conflict failure must never be masked by falling back to the cached \
         ticket, even when a cache file exists; output length {output_length}"
    );
}

/// The cache fallback must trigger on the FAILURE'S TYPE (a genuine
/// `contextdb_core::Error::DatabaseLocked`), never on searching the
/// flattened error TEXT for a marker substring. A text-matching classifier
/// is fooled the moment an unrelated failure's message happens to contain
/// that substring — and every failure message here interpolates the data
/// dir / identity path, so a data dir NAMED with the marker text is exactly
/// that accident. This drives the real corrupt-identity failure from
/// `fabric_ticket_command_surfaces_a_non_lock_failure_instead_of_masking_it_with_a_stale_cache`
/// above, but from a data dir whose path literally contains the lock
/// marker's text, with a REAL, validly-cached prior ticket sitting there
/// (unlike that test's synthetic marker string, which fails ticket syntax
/// validation regardless of classification and so cannot distinguish a
/// text-matching bug from a typed check).
#[test]
fn fabric_ticket_command_never_mistakes_a_non_lock_failure_for_a_lock_conflict_from_path_text_alone()
 {
    let base = tempfile::tempdir().expect("base dir");
    // A single path component that happens to contain the exact substring
    // the old classifier searched for
    // (`"database is locked: another process"`) — every failure message
    // this command can produce interpolates the data dir or identity path,
    // so this text lands in ANY failure's flattened string, lock-related or
    // not.
    let data_dir = base
        .path()
        .join("data-database is locked: another process (pid 1) holds x");
    std::fs::create_dir_all(&data_dir).expect("create crafted data dir");

    // First, a genuine successful run: generates a real identity, binds
    // once, and caches a real, syntactically valid ticket — exactly what a
    // later `DatabaseLocked` fallback is meant to trust.
    let (first_success, first_output, first_length) = run_fabric_ticket(&data_dir);
    assert!(
        first_success,
        "the first run against a fresh crafted data dir must succeed and cache a real ticket \
         (output length {first_length})"
    );
    let cached_ticket = extract_ticket(first_output.trim())
        .unwrap_or_else(|| panic!("the first run must print a `ticket=` field"));

    // Now corrupt the identity file — a real, non-lock failure, unrelated
    // to any running process holding the ledger. Its flattened error
    // message will read something like "fabric: load or generate identity
    // at <data_dir>/fabric-identity.key: fabric identity file ... is
    // corrupt ..." — and `<data_dir>` is the crafted path above, so this
    // message DOES contain the lock marker's text despite being a
    // completely different failure class.
    let fabric_dir = data_dir.join("fabric");
    std::fs::write(fabric_dir.join("fabric-identity.key"), b"not-32-bytes")
        .expect("corrupt identity file");

    let (second_success, second_output, second_length) = run_fabric_ticket(&data_dir);

    assert!(
        !second_success,
        "a corrupt-identity failure must surface honestly even when the data dir's own path \
         text happens to contain the lock-conflict marker string — a type-based classifier must \
         not be fooled by path content the way a substring search would be (output length \
         {second_length})"
    );
    assert!(
        !second_output.contains(&cached_ticket),
        "the corrupt-identity failure must never be masked by falling back to the real cached \
         ticket from the earlier successful run, even though that cached ticket is valid and a \
         text-matching classifier would have been fooled into returning it (output length \
         {second_length})"
    );
    assert!(
        second_output.to_ascii_lowercase().contains("corrupt")
            || second_output.to_ascii_lowercase().contains("identity"),
        "the surfaced failure must name the real problem (a corrupt identity file), not a \
         generic error; output length {second_length}"
    );
}

#[test]
fn fabric_ticket_command_generates_a_fresh_identity_on_an_empty_data_dir() {
    let data_dir = tempfile::tempdir().expect("data dir");

    let (success, combined_output, output_length) = run_fabric_ticket(data_dir.path());
    assert!(
        success,
        "vigil fabric ticket must succeed on a first-run data dir with no prior fabric \
         identity, generating one on demand (output length {output_length})"
    );
    assert!(
        combined_output.contains("ticket="),
        "vigil fabric ticket must print a `ticket=` field on a fresh data dir, output length \
         {output_length}"
    );
}
