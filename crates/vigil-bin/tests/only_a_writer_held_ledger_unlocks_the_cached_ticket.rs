//! A cached enrollment ticket is served for exactly ONE reason, and no other.
//!
//! `vigil fabric ticket` exists to answer while Vigil is up. The running node
//! owns its own fabric ledger, so the command's own open is refused as
//! writer-held — and that ONE condition is what lets it fall back to the
//! ticket it cached the last time it really did bind. The two existing pins
//! (`fabric_ticket_command.rs`, `fabric_ticket_single_retrieval_path.rs`)
//! prove that journey works.
//!
//! This file is the other half: what must NEVER unlock that cache. A cached
//! ticket served under any other condition is a stale credential presented as
//! a live answer — the operator pastes it into a second machine and enrolls
//! against an endpoint that may no longer exist, with nothing in the output
//! saying the answer came out of a file.
//!
//! Two conditions, both driven for real rather than asserted into existence:
//!
//!  * The ledger held by READERS, not by a writer. A crowd of readers is not
//!    the running node's write ownership, and the store says so in its own
//!    typed vocabulary. The reader here is a separate process genuinely parked
//!    INSIDE hydration through contextdb's own checkpoint — the only state
//!    that holds a store against a writer — so the command really does meet
//!    the reader-held condition rather than a simulation of it.
//!  * A cache stamped by a DIFFERENT identity, met while the running node
//!    genuinely holds the ledger. The condition that unlocks the cache is
//!    present and the cache is syntactically perfect; only the stamp says this
//!    ticket belongs to an identity that is no longer in play. That is a
//!    leftover from a rotated identity, and serving it hands the operator a
//!    ticket for a node that no longer exists.
//!
//! Both are negative controls, so both need the POSITIVE half to be real
//! first: in each, a real bind puts a real ticket in the cache — the seeding
//! run in the reader-held pin, the running hub itself in the stamp pin — and
//! each asserts that exact ticket is what never comes out. A refusal that
//! happened because there was nothing to serve would prove nothing.
//!
//! Never formats the real ticket into a panic or assertion message — only its
//! byte length — so a failing run cannot leak the credential into CI logs.

#![cfg(feature = "fabric")]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use contextdb_engine::ReadSession;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, TcpPortReservation, capture_pipe, vigil_binary_path, wait_until,
};

/// The ledger the parked reader is told to read.
const HOLDER_LEDGER_VARIABLE: &str = "VIGIL_TEST_FABRIC_LEDGER_HOLDER";

/// The name of the child helper below, as the test harness filters on it.
const HOLDER_TEST_NAME: &str = "a_direct_reader_parks_inside_hydration_of_the_fabric_ledger";

/// The line contextdb's own scaffold prints from inside hydration. Read here
/// only as the signal that the condition is established.
const PARKED_MARKER: &str = "READ_IMAGE_HYDRATION_HELD=";

/// The word the scaffold waits to read before it finishes hydrating.
const RELEASE_COMMAND: &str = "release";

/// Where the ledger lives under a data directory.
fn fabric_ledger_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("fabric").join("fabric-ledger.db")
}

/// Where the ticket cache itself lives — the credential a wrongly-unlocked
/// fallback would print. Read only to know what must NOT appear in an answer;
/// never formatted into a message.
fn cached_ticket_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("fabric").join("own-ticket")
}

/// Where the cached ticket's identity stamp lives. Written by a successful
/// bind beside the cache itself; holds only the public node id, never the
/// credential.
fn ticket_identity_stamp_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("fabric").join("own-ticket.node-id")
}

/// The reader that holds the fabric ledger MID-HYDRATION, in a process of its
/// own.
///
/// A read session that has finished opening does not hold a store against a
/// writer; the state that does is the middle of hydration, and the only way to
/// stop a reader there is contextdb's own checkpoint. Nothing here simulates
/// that state — the reader really is inside it, holding the real lock, for
/// exactly as long as this test leaves it there.
///
/// It runs as a SEPARATE process (this test binary re-executed) because the
/// condition under test is one process reading while another tries to take the
/// ledger; a same-process hold would be a different typed condition entirely.
#[test]
#[ignore = "re-executed as the hydration-holding child process; not an ordinary test"]
// The parent hands this child its ledger path through the environment because
// it re-executes the test binary (allow: test IPC channel).
#[allow(clippy::disallowed_methods)]
fn a_direct_reader_parks_inside_hydration_of_the_fabric_ledger() {
    let Ok(ledger_path) = std::env::var(HOLDER_LEDGER_VARIABLE) else {
        return;
    };
    contextdb_engine::persistence::read_persistence_test_scaffold::
        arm_read_image_hydration_pause_for_test();
    // Parks inside this call: the scaffold prints its marker from the middle
    // of hydration and blocks until the parent writes the release word. The
    // session is dropped immediately afterwards, so nothing is held once the
    // parent has what it needs.
    let session = ReadSession::open(Path::new(&ledger_path))
        .expect("a direct reader must be able to open the fabric ledger nobody owns");
    drop(session);
}

/// The parent's handle on that child. Kills and reaps through the standard
/// library's own `Child`, so nothing here formats a pid or a signal target,
/// and it runs on the panicking path too so a failed assertion cannot strand a
/// reader inside hydration.
struct ParkedReader {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

impl ParkedReader {
    fn park(ledger_path: &Path) -> Self {
        // This very test binary, re-executed: a resolved path to an
        // already-running executable, so it can never name cargo or a
        // container runtime. `--nocapture` is required — the scaffold's marker
        // is printed on stdout and the harness would otherwise swallow it.
        let parked_reader_binary = std::env::current_exe().expect("this test binary's own path");
        let mut child = Command::new(&parked_reader_binary)
            .arg("--exact")
            .arg(HOLDER_TEST_NAME)
            .arg("--ignored")
            .arg("--nocapture")
            .env(HOLDER_LEDGER_VARIABLE, ledger_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the hydration-holding reader");
        let stdout = capture_pipe(child.stdout.take());
        let stderr = capture_pipe(child.stderr.take());
        Self {
            child,
            stdout,
            stderr,
        }
    }

    fn output(&self) -> String {
        let read =
            |pipe: &Arc<Mutex<String>>| pipe.lock().map(|text| text.clone()).unwrap_or_default();
        format!("{}{}", read(&self.stdout), read(&self.stderr))
    }

    /// Block until the reader is genuinely INSIDE hydration, proven by the
    /// marker the scaffold prints only from that point. An exit before the
    /// marker is a failure and not a wait: there would be no reader left to
    /// refuse the command.
    fn wait_until_parked(&mut self) {
        let outcome = {
            let child = &mut self.child;
            let stdout = self.stdout.clone();
            wait_until(
                "the direct reader to park inside hydration of the fabric ledger",
                RUNTIME_STARTUP_TIMEOUT,
                move || {
                    if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                        return Err(format!(
                            "the reader exited ({status}) instead of parking inside hydration, so \
                             the command below would have met no reader at all"
                        ));
                    }
                    let logs = stdout.lock().map(|text| text.clone()).unwrap_or_default();
                    Ok(logs.contains(PARKED_MARKER).then_some(()))
                },
            )
        };
        if let Err(error) = outcome {
            panic!("{error}. The reader's output was:\n{}", self.output());
        }
    }

    /// Let the reader finish hydrating and leave. The ledger is free from here.
    fn release(mut self) {
        if let Some(mut stdin) = self.child.stdin.take() {
            use std::io::Write as _;
            let _ = writeln!(stdin, "{RELEASE_COMMAND}");
            let _ = stdin.flush();
        }
        let _ = self.child.wait();
    }
}

impl Drop for ParkedReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One `vigil fabric ticket` invocation: whether it succeeded, its combined
/// output, and that output's length (the only thing ever formatted into a
/// message, so the credential cannot leak through a failure).
fn run_fabric_ticket(data_dir: &Path) -> (bool, String, usize) {
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

/// Pull the ticket value out of a printed `ticket=<ticket>` line without ever
/// formatting the whole line (which carries the credential) into a message.
fn extract_ticket(output: &str) -> Option<String> {
    let after_marker = output.split_once("ticket=")?.1;
    let ticket = after_marker
        .split_once(char::is_whitespace)
        .map(|(ticket, _)| ticket)
        .unwrap_or(after_marker);
    let trimmed = ticket.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Bind once for real against a fresh data directory, which generates the
/// identity, creates the ledger, and caches this node's own ticket with the
/// identity stamp beside it. Returns the ticket that is now in the cache — the
/// exact string a wrongly-unlocked cache would print.
fn seed_a_real_cached_ticket(data_dir: &Path) -> String {
    let (success, output, length) = run_fabric_ticket(data_dir);
    assert!(
        success,
        "the seeding run must bind for real and cache this node's ticket, or the refusals below \
         would be refusing to serve a cache that was never there (output length {length})"
    );
    let ticket = extract_ticket(&output)
        .unwrap_or_else(|| panic!("the seeding run must print a ticket= field"));
    assert!(
        fabric_ledger_path(data_dir).is_file(),
        "the seeding run must leave a real fabric ledger for a reader to hold: {}",
        fabric_ledger_path(data_dir).display()
    );
    ticket
}

/// A running hub node, holding its own fabric ledger for as long as it lives —
/// the ordinary writer-held condition an operator's `vigil fabric ticket` meets.
struct RunningHub {
    child: Child,
    stdout: Arc<Mutex<String>>,
}

impl RunningHub {
    fn start(data_dir: &Path) -> Self {
        let health = TcpPortReservation::reserve_loopback().expect("reserve a liveness port");
        let review = TcpPortReservation::reserve_loopback().expect("reserve a review port");
        let mut command = Command::new(vigil_binary_path());
        command
            .arg("run")
            .arg("--health-port")
            .arg(health.port().to_string())
            .arg("--review-port")
            .arg(review.port().to_string())
            .arg("--fabric-hub")
            .arg("true")
            .env("VIGIL_DATA_DIR", data_dir)
            .env_remove("VIGIL_RTSP_URL")
            .env_remove("VIGIL_FABRIC_TICKET")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn vigil binary");
        let stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());
        Self { child, stdout }
    }

    /// Block until this node has its fabric up and therefore genuinely owns
    /// the ledger file — the condition the command below has to meet.
    fn wait_until_it_owns_its_ledger(&self) {
        let stdout = self.stdout.clone();
        wait_until(
            "the hub node to bring its fabric up and take its ledger",
            RUNTIME_STARTUP_TIMEOUT,
            move || {
                let logs = stdout.lock().map(|text| text.clone()).unwrap_or_default();
                Ok(logs.contains("fabric_ready=true").then_some(()))
            },
        )
        .expect("the hub node must reach fabric_ready before the command can meet a live writer");
    }
}

impl Drop for RunningHub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Readers holding the ledger is a different condition from the running node
/// owning it, and only the second one may answer from the cache. A reader is
/// transient and says so; the command's job is to refuse and let the operator
/// ask again, not to reach for a file.
#[test]
fn a_ledger_held_by_readers_is_refused_instead_of_answered_from_the_cache() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let cached_ticket = seed_a_real_cached_ticket(data_dir.path());

    let mut reader = ParkedReader::park(&fabric_ledger_path(data_dir.path()));
    reader.wait_until_parked();

    let (success, output, length) = run_fabric_ticket(data_dir.path());
    reader.release();

    assert!(
        !success,
        "a ledger held by a direct reader is not the running node's write ownership, so the \
         command must refuse rather than answer at all (output length {length})"
    );
    assert!(
        !output.contains(&cached_ticket),
        "the command served its cached ticket while the ledger was merely being READ — a stale \
         credential presented as a live answer, and the operator has nothing in the output \
         telling them it came out of a file (output length {length}, cached ticket length {})",
        cached_ticket.len()
    );
}

/// The identity stamp is what tells a cached ticket that still belongs to this
/// node from one left behind by an identity that has since been replaced. With
/// the running node genuinely holding the ledger — the one condition that
/// unlocks the cache — a stamp naming another identity must still refuse.
///
/// The positive half of this exact journey already has its own pin (`vigil
/// fabric ticket` prints the live ticket while the hub node is still running,
/// in `fabric_ticket_command.rs`), so this file does not repeat it: what is
/// established here is that the SAME condition, with only the stamp changed,
/// stops producing an answer.
#[test]
fn a_cache_stamped_by_another_identity_is_refused_even_while_a_writer_holds_the_ledger() {
    let data_dir = tempfile::tempdir().expect("data dir");

    // The hub binds for real, which is what caches this node's ticket and
    // stamps the identity that produced it — and it goes on holding the ledger
    // for the whole of the command below, so the writer-held condition is
    // genuinely in force rather than arranged.
    let hub = RunningHub::start(data_dir.path());
    hub.wait_until_it_owns_its_ledger();

    let cached_ticket = std::fs::read_to_string(cached_ticket_path(data_dir.path()))
        .expect("the running hub must have cached its own ticket")
        .trim()
        .to_string();
    assert!(
        !cached_ticket.is_empty(),
        "the cache must hold a real ticket, or the refusal below would be refusing to serve \
         nothing and would prove nothing"
    );
    let stamp_path = ticket_identity_stamp_path(data_dir.path());
    assert!(
        stamp_path.is_file(),
        "the running hub must have stamped the identity that produced that cache: {}",
        stamp_path.display()
    );

    // Restamp it with a node id this identity file will never produce — what a
    // rotated identity leaves behind — while changing nothing else: same cache,
    // same ticket, same identity file, same live writer.
    std::fs::write(
        &stamp_path,
        "0000000000000000000000000000000000000000000000000000000000000000",
    )
    .expect("restamp the cached ticket with a since-rotated identity");

    let (success, output, length) = run_fabric_ticket(data_dir.path());
    drop(hub);

    assert!(
        !success,
        "the cached ticket belongs to an identity that is no longer in play, so even the \
         writer-held condition that legitimately unlocks the cache must not produce an answer \
         (output length {length})"
    );
    assert!(
        !output.contains(&cached_ticket),
        "a cache left behind by a since-rotated identity was served as this node's live ticket; \
         an operator pasting it enrolls a second machine against a node that no longer exists \
         (output length {length}, cached ticket length {})",
        cached_ticket.len()
    );
}
