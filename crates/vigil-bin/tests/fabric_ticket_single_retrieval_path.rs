//! Owner ruling, extended: the fabric enrollment ticket is a credential.
//! `fabric_ticket_health_exposure.rs` pinned it off `/health`; independent
//! review then found it also reaches several AMBIENT surfaces that ruling
//! never knew about — `vigil stats`, `vigil doctor`, and the persisted
//! `runtime-stats.json` file (exactly what an operator pastes into a bug
//! report, and a file that outlives the process). This is the SAME
//! principle applied to those newly found instances, not a new decision.
//!
//! This is the "one answer" guarantee, extended to a two-tier contract: one
//! test drives a real, running hub node and asserts the ticket string
//! appears on no DISPLAYED/SERVED surface at all (logs, `vigil stats`,
//! `vigil doctor`, `/health`) except the two sanctioned retrieval paths —
//! the startup log and `vigil fabric ticket` — and on no PRIVATE PERSISTED
//! store except the two sanctioned ones — `fabric/own-ticket` and the
//! fabric ledger (`fabric/fabric-ledger.db`, whose upstream `peer_directory`
//! table is the fleet's own `node_id -> ticket` coordination directory).
//! Both sanctioned retrieval paths are exercised as non-vacuous positive
//! controls (the ticket really is live and retrievable this run), so this
//! cannot pass merely because fabric never configured or the ticket never
//! existed. The byte-level sweep below keeps full reach over every
//! persisted file — the sanctioned stores are allowlisted by exact path
//! after being swept, never excluded by extension or directory — so a
//! FUTURE unsanctioned file is still caught by construction.
//!
//! Never formats the real ticket value into a panic or assertion message —
//! only its byte length — so a failing run cannot leak the credential into
//! CI logs.

#![cfg(feature = "fabric")]

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, get, vigil_binary_path, wait_until,
};

/// Redact every occurrence of the fabric enrollment ticket from a raw
/// captured-log blob before it is ever formatted into a panic message.
///
/// `FabricRuntime::join_instruction` prints the ticket TWICE on one line —
/// `fabric-join ticket={ticket} command=vigil run --fabric-ticket
/// {ticket}` — the second copy under a DIFFERENT syntax (`--fabric-ticket
/// <value>`, space-separated, no `=`) that a `ticket=`-only, first-match
/// scan never even recognizes, let alone redacts. The robust primary
/// mechanism is redacting the KNOWN ticket VALUE wherever it appears in the
/// blob, syntax notwithstanding (`str::replace` is exhaustive over every
/// occurrence, not just the first) — that alone closes the doubled-line
/// leak. `known_ticket` is `None` at call sites that redact before the
/// ticket has been observed/extracted yet; the syntactic backstop pass
/// (`ticket=` and `--fabric-ticket `, each redacted for every occurrence on
/// every line, not just the first) still applies in that case.
fn redact_ticket(logs: &str, known_ticket: Option<&str>) -> String {
    let mut out = String::new();
    for line in logs.lines() {
        let mut redacted_line = line.to_string();
        if let Some(ticket) = known_ticket.filter(|ticket| !ticket.is_empty())
            && redacted_line.contains(ticket)
        {
            redacted_line = redacted_line.replace(ticket, &format!("<redacted:{}b>", ticket.len()));
        }
        redacted_line = redact_marker_occurrences(&redacted_line, "ticket=");
        redacted_line = redact_marker_occurrences(&redacted_line, "--fabric-ticket ");
        out.push_str(&redacted_line);
        out.push('\n');
    }
    out
}

/// Redact the space-delimited value following EVERY occurrence of `marker`
/// on `line` (not just the first), down to its byte length. A value that is
/// already a `<redacted:...>` placeholder (from an earlier pass) is left
/// untouched rather than redacted again, so this backstop pass never
/// double-wraps a value the known-ticket pass already handled.
fn redact_marker_occurrences(line: &str, marker: &str) -> String {
    let mut result = String::new();
    let mut rest = line.to_string();
    while let Some(idx) = rest.find(marker) {
        let after_marker = &rest[idx + marker.len()..];
        let (value, suffix) = match after_marker.split_once(' ') {
            Some((value, suffix)) => (value.to_string(), format!(" {suffix}")),
            None => (after_marker.to_string(), String::new()),
        };
        result.push_str(&rest[..idx]);
        result.push_str(marker);
        if value.starts_with("<redacted:") {
            result.push_str(&value);
        } else {
            result.push_str(&format!("<redacted:{}b>", value.len()));
        }
        rest = suffix;
    }
    result.push_str(&rest);
    result
}

/// The live defect this fix closes: `join_instruction` prints the ticket
/// twice on one line under two different syntaxes. Proves the doubled line
/// comes back with ZERO copies of a (synthetic, non-real) ticket value —
/// never quoting a real ticket, matching this file's own no-leak
/// discipline for its self-tests.
#[test]
fn redact_ticket_removes_every_occurrence_on_a_doubled_instruction_line() {
    let ticket = "synthetic-ticket-value-for-redaction-self-test";
    let blob = format!(
        "boot_phase=pipeline-up\nfabric-join ticket={ticket} command=vigil run --fabric-ticket {ticket}\n"
    );
    assert_eq!(
        blob.matches(ticket).count(),
        2,
        "test setup sanity: the doubled instruction line must carry exactly two copies of the \
         ticket, or this test proves nothing about exhaustive redaction"
    );

    let redacted = redact_ticket(&blob, Some(ticket));

    assert!(
        !redacted.contains(ticket),
        "every occurrence of the ticket on a doubled instruction line must be redacted"
    );
    assert_eq!(
        redacted.matches("<redacted:").count(),
        2,
        "both the `ticket=` copy and the `--fabric-ticket` copy must each be redacted once, \
         leaving exactly two placeholders"
    );

    // Also prove the backstop (syntax-only, no known value) independently
    // closes the same doubled line, since production code takes this path
    // whenever the ticket has not been observed/extracted yet.
    let backstop_redacted = redact_ticket(&blob, None);
    assert!(
        !backstop_redacted.contains(ticket),
        "the prefix-based backstop pass alone must also remove every occurrence, since \
         `--fabric-ticket <value>` is a distinct syntax from `ticket=<value>` and both must be \
         recognized"
    );
}

/// A single line can carry the SAME marker twice with two DIFFERENT values
/// (unlike the doubled-instruction line above, which pairs two DIFFERENT
/// markers on one line). This is the one shape `redact_marker_occurrences`'s
/// while-loop must walk more than once: after redacting the first `ticket=`
/// occurrence it must keep scanning the remainder of the line and find the
/// second. Uses two distinct synthetic values (never a real ticket) so a
/// failure here cannot leak a credential, and so the assertions pin each
/// value independently rather than merely counting placeholders.
#[test]
fn redact_marker_occurrences_removes_every_occurrence_of_a_repeated_marker_on_one_line() {
    let first_ticket = "synthetic-ticket-value-alpha";
    let second_ticket = "synthetic-ticket-value-bravo-longer";
    let line = format!("fabric-relay ticket={first_ticket} rejoin ticket={second_ticket}");

    let redacted = redact_marker_occurrences(&line, "ticket=");

    assert!(
        !redacted.contains(first_ticket),
        "the first occurrence of a twice-repeated marker on one line must be redacted"
    );
    assert!(
        !redacted.contains(second_ticket),
        "the second occurrence of a twice-repeated marker on one line must be redacted"
    );
    assert_eq!(
        redacted.matches("<redacted:").count(),
        2,
        "both occurrences of the repeated `ticket=` marker must be redacted independently, \
         leaving exactly two placeholders"
    );
}

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

/// Defect #4 fix: the displayed-surface half of the two-tier contract says
/// the ticket appears on NO displayed surface except the single sanctioned
/// startup line — but until this fix, stderr was captured and never
/// inspected, and stdout was only ever searched for boot markers. This
/// scans every line of `text` (one of the two real process streams) and
/// fails if the ticket appears on any line other than `sanctioned_line`,
/// matched by EXACT equality — never a prefix/substring match such as
/// "contains fabric-join", which would exclude every future line that
/// happens to mention that word and re-open the hole this closes. Never
/// formats the offending line into the panic message (only its length),
/// so a failure here cannot leak the credential.
fn assert_ticket_confined_to_the_sanctioned_line(
    stream_name: &str,
    text: &str,
    ticket: &str,
    sanctioned_line: &str,
) {
    for line in text.lines() {
        if line.contains(ticket) && line != sanctioned_line {
            panic!(
                "{stream_name} must never carry the fabric enrollment ticket outside the single \
                 sanctioned startup fabric-join line; offending line length {}, sanctioned line \
                 length {}",
                line.len(),
                sanctioned_line.len()
            );
        }
    }
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

fn run_fabric_ticket(data_dir: &std::path::Path) -> (bool, String) {
    let output = Command::new(vigil_binary_path())
        .arg("fabric")
        .arg("ticket")
        .arg("--data-dir")
        .arg(data_dir)
        .stdin(Stdio::null())
        .output()
        .expect("spawn vigil fabric ticket");
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn run_public_stats(data_dir: &std::path::Path) -> String {
    let output = Command::new(vigil_binary_path())
        .arg("stats")
        .env("VIGIL_DATA_DIR", data_dir)
        .output()
        .expect("run vigil stats");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Every regular file persisted anywhere under `dir`, walked recursively.
/// Used instead of a hand-listed set of "the persisted surfaces we know
/// about" so a FUTURE file this node persists (a new cache, a new snapshot)
/// is swept into the containment check below by construction — the exact
/// gap that let `fabric/own-ticket` go unchecked when it was added.
/// Walks recursively and FAILS LOUDLY (returns `Err`) on any traversal
/// error, rather than treating an unreadable directory as empty: the sweep
/// this feeds exists to prove the ticket appears nowhere unsanctioned, and a
/// directory this walk silently skipped would be a blind spot indistinguishable
/// from "checked and clean."
fn list_files_recursive(dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>, String> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|err| format!("read_dir {} failed: {err}", dir.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|err| format!("read_dir entry under {}: {err}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| format!("file_type for {}: {err}", path.display()))?;
        if file_type.is_dir() {
            out.extend(list_files_recursive(&path)?);
        } else if file_type.is_file() {
            out.push(path);
        }
    }
    Ok(out)
}

/// Read `path` as raw BYTES (never `read_to_string`, which would turn a
/// non-UTF-8/binary file into an `Err` a caller could silently swallow) and
/// report whether `ticket` appears anywhere in it. A read failure is a hard
/// panic naming only the path and byte length — never the file's contents,
/// so a failure here cannot leak the credential.
fn file_contains_ticket_bytes(path: &std::path::Path, ticket: &str) -> bool {
    let contents =
        std::fs::read(path).unwrap_or_else(|err| panic!("read {} failed: {err}", path.display()));
    let ticket_bytes = ticket.as_bytes();
    contents
        .windows(ticket_bytes.len())
        .any(|window| window == ticket_bytes)
}

/// Self-contained proof the sweep mechanism itself catches a credential
/// embedded in a non-UTF-8 file: `read_to_string(...).unwrap_or_default()`
/// (the prior implementation) would have returned `""` for this file —
/// silently vacuous — because the file is not valid UTF-8 at all. The
/// byte-level replacement must still find the ticket. No live `vigil`
/// process involved; this exercises exactly the two functions the real
/// sweep above calls.
#[test]
fn sweep_detects_a_ticket_embedded_in_a_binary_non_utf8_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let ticket = "synthetic-ticket-marker-for-sweep-self-test";

    // Sanity control: a CLEAN binary file (no ticket bytes) must not trip
    // the check — otherwise this test would prove nothing about the ticket
    // specifically.
    let clean_binary: Vec<u8> = vec![0xff, 0x00, 0xfe, 0x80, 0x01, 0xff, 0xfe];
    let clean_path = dir.path().join("clean.blob");
    std::fs::write(&clean_path, &clean_binary).expect("write clean binary file");
    assert!(
        !file_contains_ticket_bytes(&clean_path, ticket),
        "sanity: a binary file with no embedded ticket must not be flagged"
    );

    // The planted case: invalid-UTF-8 bytes surrounding a copy of the
    // ticket. `String::from_utf8` on this whole buffer fails outright (the
    // lone 0xff/0xfe bytes are not valid UTF-8 continuations), so
    // `read_to_string` would never even see the ticket bytes it contains.
    let mut binary_with_ticket: Vec<u8> = vec![0xff, 0x00, 0xfe];
    binary_with_ticket.extend_from_slice(ticket.as_bytes());
    binary_with_ticket.extend_from_slice(&[0x80, 0x01, 0xff]);
    assert!(
        std::str::from_utf8(&binary_with_ticket).is_err(),
        "test setup sanity: the planted buffer must not be valid UTF-8, or this proves nothing \
         about the binary-file blind spot"
    );
    let planted_path = dir.path().join("planted.blob");
    std::fs::write(&planted_path, &binary_with_ticket).expect("write planted binary file");

    assert!(
        file_contains_ticket_bytes(&planted_path, ticket),
        "the byte-level sweep must detect a ticket embedded in a non-UTF-8 binary file — a \
         `read_to_string`-based check would have silently returned empty here and missed it"
    );

    // Also prove the directory walk itself surfaces this file (not just the
    // byte-scan in isolation), matching exactly what the real sweep does.
    let walked = list_files_recursive(dir.path()).expect("walk temp dir");
    assert!(
        walked.contains(&planted_path),
        "list_files_recursive must surface the binary file for the sweep to check"
    );
}

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
fn fabric_enrollment_ticket_appears_on_no_surface_except_the_two_sanctioned_retrieval_paths() {
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
    let stderr: Arc<Mutex<String>> = capture_pipe(child.stderr.take());
    let node = Node { child };

    let boot = wait_until(
        "the hub node to report boot_phase=pipeline-up",
        Duration::from_secs(20),
        || {
            Ok(stdout
                .lock()
                .expect("stdout lock")
                .contains("boot_phase=pipeline-up")
                .then_some(()))
        },
    );
    if let Err(error) = boot {
        let logs = redact_ticket(&stdout.lock().expect("stdout lock"), None);
        node.kill_and_wait();
        panic!("{error}; logs so far:\n{logs}");
    }

    // Sanctioned path #1 (positive control): the startup log's own
    // fabric-join line, proving the ticket really is live this run.
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
            let logs = redact_ticket(&stdout.lock().expect("stdout lock"), None);
            node.kill_and_wait();
            panic!("{error}; logs so far:\n{logs}");
        }
    };
    let advertised_ticket = match extract_ticket(&join_line) {
        Some(ticket) => ticket,
        None => {
            node.kill_and_wait();
            panic!("the printed startup join line must carry a ticket= field");
        }
    };

    // Sanctioned path #2 (positive control, run WHILE the node is up): the
    // deliberate local command must independently produce the identical
    // ticket.
    let (ticket_command_success, ticket_command_output) = run_fabric_ticket(data_dir.path());
    if !ticket_command_success || !ticket_command_output.contains(&advertised_ticket) {
        let output_length = ticket_command_output.len();
        node.kill_and_wait();
        panic!(
            "sanity: `vigil fabric ticket` must succeed and reproduce the advertised ticket \
             while the hub node is running, before this test can prove the ambient surfaces omit \
             it (success={ticket_command_success}, output length {output_length}, expected \
             ticket length {})",
            advertised_ticket.len()
        );
    }

    // The ambient surfaces this pass closes. Each must be genuinely
    // exercised (non-empty / a real HTTP 2xx) before its absence proves
    // anything, mirroring `fabric_ticket_health_exposure.rs`'s existing
    // non-vacuous-control discipline.
    let stats_output = run_public_stats(data_dir.path());
    let doctor_output = run_doctor_acceleration(data_dir.path());
    let health_body = get(health_port, "/health").body_text();
    let persisted_stats_path = data_dir.path().join("runtime-stats.json");
    let persisted_stats = std::fs::read_to_string(&persisted_stats_path).unwrap_or_default();

    let raw_stdout = stdout.lock().expect("stdout lock").clone();
    let raw_stderr = stderr.lock().expect("stderr lock").clone();
    let boot_logs = redact_ticket(&raw_stdout, Some(&advertised_ticket));
    node.kill_and_wait();

    assert!(
        !persisted_stats.is_empty(),
        "sanity: runtime-stats.json must exist and be non-empty before this test can prove it \
         omits the ticket; logs:\n{boot_logs}"
    );
    assert!(
        doctor_output.contains("fabric-status="),
        "sanity: vigil doctor must genuinely report a live fabric-status line (proving fabric \
         really enrolled this run) before this test can prove it omits the ticket; doctor output \
         length {}; logs:\n{boot_logs}",
        doctor_output.len()
    );
    assert!(
        stats_output.contains("fabric-status="),
        "sanity: vigil stats must genuinely report a live fabric-status line before this test \
         can prove it omits the ticket; stats output length {}; logs:\n{boot_logs}",
        stats_output.len()
    );

    assert!(
        !stats_output.contains(&advertised_ticket),
        "`vigil stats` must never carry the fabric enrollment ticket; output length {}",
        stats_output.len()
    );
    assert!(
        !doctor_output.contains(&advertised_ticket),
        "`vigil doctor` must never carry the fabric enrollment ticket; output length {}",
        doctor_output.len()
    );
    assert!(
        !health_body.contains(&advertised_ticket),
        "the `/health` body must never carry the fabric enrollment ticket; body length {}",
        health_body.len()
    );
    assert!(
        !persisted_stats.contains(&advertised_ticket),
        "the persisted runtime-stats.json file must never carry the fabric enrollment ticket \
         (it outlives the process); file length {}",
        persisted_stats.len()
    );

    // The two-tier contract's DISPLAYED-surface half, checked directly
    // against both process streams rather than only the boot markers this
    // test already scanned stdout for: the ticket must appear on no line of
    // stdout or stderr except the single sanctioned startup fabric-join
    // line, matched EXACTLY (not merely a line containing "fabric-join",
    // which would re-open the hole an over-broad exclusion creates).
    assert_ticket_confined_to_the_sanctioned_line(
        "stdout",
        &raw_stdout,
        &advertised_ticket,
        &join_line,
    );
    assert_ticket_confined_to_the_sanctioned_line(
        "stderr",
        &raw_stderr,
        &advertised_ticket,
        &join_line,
    );

    // `fabric/own-ticket` (the cache `run_ticket_command` falls back to) is
    // a SANCTIONED persisted surface — unlike the two checks above, its
    // property is not absence but permissions + containment: it is expected
    // to carry the ticket (that is its whole purpose), so it must be
    // readable by no one but the owner, and it must be one of exactly two
    // filesystem surfaces (of everything this node persisted this run) that
    // carry the ticket at all.
    let own_ticket_path = data_dir.path().join("fabric").join("own-ticket");
    let own_ticket_contents = std::fs::read_to_string(&own_ticket_path).unwrap_or_default();
    assert!(
        own_ticket_contents.trim() == advertised_ticket,
        "sanity: fabric/own-ticket must exist and cache exactly the advertised ticket before \
         this test can prove its permissions and containment; found length {}",
        own_ticket_contents.len()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(&own_ticket_path).expect("stat fabric/own-ticket");
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "fabric/own-ticket carries a credential and must be owner-read/write only \
             (0600), got {mode:o}"
        );
    }

    // `fabric/fabric-ledger.db` is the SECOND sanctioned persisted surface.
    // It is the fleet's coordination mechanism (upstream's `peer_directory`
    // table is a `node_id -> ticket` directory that peers legitimately read
    // to address each other), and reaching it already requires filesystem
    // access to the data dir — at which point `own-ticket` is readable
    // anyway. It is sanctioned on the SAME terms as `own-ticket`: not
    // excluded from the byte sweep below (a `.db` exclusion would restore
    // the exact blind spot this sweep was built to close), but held to the
    // same owner-only permission bar, tightened vigil-side after
    // `Database::open` since contextdb creates it at a wider default mode.
    let fabric_ledger_path = data_dir.path().join("fabric").join("fabric-ledger.db");
    assert!(
        fabric_ledger_path.is_file(),
        "sanity: fabric/fabric-ledger.db must exist before this test can prove its permissions \
         and containment"
    );
    // Defect #3 fix: the ledger's ticket occurrence was never POSITIVELY
    // proved — this test used to assert only that the file exists and is
    // 0600, then skip its bytes entirely. If contextdb ever stopped storing
    // the ticket in `peer_directory`, this allowlist entry would silently
    // become dead weight (an unconditional exemption) while the test kept
    // passing, proving strictly less than it claims. Assert the ledger
    // genuinely DOES contain the ticket bytes before it is allowlisted out
    // of the sweep below, turning the allowlist into a checked statement
    // about observed behavior.
    assert!(
        file_contains_ticket_bytes(&fabric_ledger_path, &advertised_ticket),
        "sanity: fabric/fabric-ledger.db must genuinely contain the fabric enrollment ticket \
         (via contextdb's peer_directory table) before this test can allowlist it out of the \
         sweep below — otherwise the allowlist proves nothing"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata =
            std::fs::metadata(&fabric_ledger_path).expect("stat fabric/fabric-ledger.db");
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "fabric/fabric-ledger.db carries this node's own enrollment ticket via the peer \
             directory table and must be owner-read/write only (0600), got {mode:o}"
        );
    }

    // Sweep EVERY file this node persisted under its data dir, derived from
    // the filesystem rather than hand-listed, and require the ticket to
    // appear on NONE of them except the two sanctioned files above — so a
    // future persisted surface is caught by this test without anyone having
    // to remember to name it here. The ledger stays IN this walk (it is not
    // excluded by extension or path) so any *other* unsanctioned file still
    // fails the test.
    let sanctioned_persisted_paths = [own_ticket_path, fabric_ledger_path];
    let swept_files =
        list_files_recursive(data_dir.path()).expect("walk the node data directory recursively");
    for path in swept_files {
        if sanctioned_persisted_paths.contains(&path) {
            continue;
        }
        // `file_contains_ticket_bytes` reads raw BYTES rather than
        // `read_to_string`: a non-UTF-8 or binary file (an embedded
        // database, a compressed artefact) would make `read_to_string`
        // return an `Err` that `unwrap_or_default` silently turned into an
        // empty, vacuously-passing string — the exact blind spot this
        // sweep exists to close (proven in isolation by
        // `sweep_detects_a_ticket_embedded_in_a_binary_non_utf8_file`
        // above). A read failure of any kind is now a hard test failure,
        // never a silent pass; the file's contents are never formatted
        // into the panic message (only its byte length), so a failure
        // here cannot leak the credential.
        assert!(
            !file_contains_ticket_bytes(&path, &advertised_ticket),
            "persisted file {} must never carry the fabric enrollment ticket (only \
             fabric/own-ticket is a sanctioned store for it)",
            path.display()
        );
    }
}
