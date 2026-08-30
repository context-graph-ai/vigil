//! Reading Vigil's store while Vigil is starting must not cost the operator
//! their whole deployment.
//!
//! A read command that finds no owner opens the store itself, and while it is
//! reading that file the runtime's own writable open is refused — the store
//! says so in as many words, naming the reader that holds it and saying the
//! condition clears on its own: `the store at <path> is busy: 1 direct reader
//! is still reading it, so a writer cannot take it yet; it clears when they
//! finish. Verified process <pid> (<name>)`. The runtime must wait for exactly
//! that. It does not today: one refused open and the process declares the store
//! unreadable, drops into unmanaged, registers no owner route, and stays that
//! way for the rest of its life. So an operator who types `vigil stats` while
//! Vigil is coming up loses recording, review history, corrections and settings
//! changes until they notice and restart — and nothing they did was wrong.
//!
//! The reader is held for the length of the refusal and released the moment the
//! runtime has been seen to refuse, so the retry it must then perform is a
//! retry and not a first attempt that happened to succeed.
//! Every wait is a bounded poll of the process's own output; nothing sleeps and
//! nothing asserts on elapsed time.
//!
//! Where the hazard was seen: a runtime start polled with `vigil stats` was
//! refused for exactly this reason and went unmanaged for the rest of its life.
//! (The refusal was worded differently then; the needles below deliberately
//! match only the parts of the sentence that carry the meaning, so a rewording
//! cannot fail this pin and a different refusal cannot satisfy it.)
//!
//! The wait has NO outer limit, and that is the ruled contract (owner ruling
//! of 2026-08-25, folded into `contextdb-read-intent.md`): a runtime starting
//! while someone briefly reads its store says it is waiting, stays in startup,
//! and starts normally when the reader finishes — it retries for exactly as
//! long as the typed reader-held condition persists. There is no timeout, no
//! configuration knob, no default and no expiry journey; a configurable
//! startup timeout is a separate future proposal and not this contract.
//!
//! So the pins here assert exactly that and nothing more: the condition holds,
//! the runtime says it is waiting rather than declaring the store unreadable,
//! and it comes up owning the store WHEN THE READER RELEASES — released by
//! this test, deterministically, through the seam below rather than by a clock
//! running out. No pin drives a reader past a deadline, because under this
//! contract there is no deadline to drive it past; the bounded waits in the
//! harness are the test's own patience, never a product promise.
//!
//! How the condition is CONSTRUCTED, since it cannot be raced into place: a
//! read session that has finished opening does not hold the store against a
//! writer — the runtime opens straight through one, measured — so the reader
//! has to be stopped in the middle of hydration, which is the state that does
//! hold it. contextdb's own checkpoint does that and nothing else does:
//! `arm_read_image_hydration_pause_for_test` makes the next read-image
//! hydration announce the breadcrumb it just took and then block until it is
//! told to carry on. The reader here really is in that state, holding the real
//! lock, in a separate process, for exactly as long as this test leaves it
//! there — so a runtime that reported the refusal without one, or reported it
//! and then never retried, is caught on what actually happened rather than on
//! a condition the test asserted into existence.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use contextdb_engine::ReadSession;
use vigil::OWNER_SERVED_PREFIX;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, TcpPortReservation, capture_pipe, open_store_at, vigil_binary_path,
    wait_until,
};

/// The store the parked reader is told to read.
const HOLDER_STORE_VARIABLE: &str = "VIGIL_TEST_HYDRATION_HOLDER_STORE";

/// The name of the child helper below, as the test harness filters on it.
const HOLDER_TEST_NAME: &str =
    "a_direct_reader_parks_inside_hydration_until_its_parent_releases_it";

/// The line contextdb's own scaffold prints from inside hydration, carrying
/// the breadcrumb it has just taken. Read here only as the signal that the
/// condition is established; what the runtime makes of that breadcrumb is the
/// assertion, not this.
const PARKED_MARKER: &str = "READ_IMAGE_HYDRATION_HELD=";

/// The word the scaffold waits to read before it finishes hydrating.
const RELEASE_COMMAND: &str = "release";

/// What the scaffold prints instead of a breadcrumb when the reader it parked
/// took none. A reader with no breadcrumb is a different condition from the one
/// under test — there is nothing for the store to name, so a refusal that named
/// nobody would be correct — so this test refuses to run on it rather than
/// reporting a defect that is not there.
const UNVERIFIED_HOLDER: &str = "unverified";

/// The reader that holds the store MID-HYDRATION, in a process of its own.
///
/// This is the whole difficulty of the hazard. A read session that has finished
/// opening does not hold the store against a writer — the runtime opens
/// straight through one — so a test that merely kept a session alive proved
/// nothing and said so. The state that does hold it is the middle of hydration,
/// and the only way to stop a reader there is contextdb's own checkpoint:
/// `arm_read_image_hydration_pause_for_test` makes the next read-image
/// hydration announce the breadcrumb it took and then block on stdin until it
/// is told to carry on. Nothing here simulates that state — the reader really
/// is inside it, holding the real lock, for exactly as long as the parent
/// leaves it there.
///
/// It runs as a SEPARATE process, this test binary re-executed, for the same
/// reason the store-lock fixture does: the process the refusal names must be
/// one this test can check independently and could not have produced by
/// reporting itself.
#[test]
#[ignore = "re-executed as the hydration-holding child process; not an ordinary test"]
// The parent hands this child its store path through the environment because it
// re-executes the test binary (allow: test IPC channel).
#[allow(clippy::disallowed_methods)]
fn a_direct_reader_parks_inside_hydration_until_its_parent_releases_it() {
    let Ok(store_path) = std::env::var(HOLDER_STORE_VARIABLE) else {
        return;
    };
    contextdb_engine::persistence::read_persistence_test_scaffold::
        arm_read_image_hydration_pause_for_test();
    // Parks inside this call: the scaffold prints its marker from the middle of
    // hydration and blocks until the parent writes the release word. The
    // session is dropped immediately afterwards, so nothing is held once the
    // parent has what it needs.
    let session = ReadSession::open_with_options(
        Path::new(&store_path),
        context_graph::owner_control::context_graph_read_session_options(),
    )
    .expect("a direct reader must be able to open the store nobody owns");
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
    fn park(store_path: &Path) -> Self {
        // This very test binary, re-executed: a resolved path to an
        // already-running executable, so it can never name cargo or a container
        // runtime. `--nocapture` is required — the scaffold's marker is printed
        // on stdout and the harness would otherwise swallow it.
        let parked_reader_binary = std::env::current_exe().expect("this test binary's own path");
        let mut child = Command::new(&parked_reader_binary)
            .arg("--exact")
            .arg(HOLDER_TEST_NAME)
            .arg("--ignored")
            .arg("--nocapture")
            .env(HOLDER_STORE_VARIABLE, store_path)
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

    /// The process id the refusal must name. Taken from the standard library's
    /// own handle, never parsed out of the child's output, so the two are
    /// genuinely independent.
    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Block until the reader is genuinely INSIDE hydration, proven by the
    /// marker the scaffold prints only from that point, and return that
    /// marker's payload. An exit before the marker is a failure and not a wait:
    /// there would be no reader left for the runtime to be refused by.
    fn wait_until_parked(&mut self) -> String {
        let outcome = {
            let child = &mut self.child;
            let stdout = self.stdout.clone();
            wait_until(
                "the direct reader to park inside hydration",
                RUNTIME_STARTUP_TIMEOUT,
                move || {
                    if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                        return Err(format!(
                            "the reader exited ({status}) instead of parking inside hydration, so \
                             the runtime below would have had nothing holding its store"
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
        let output = self.output();
        output
            .lines()
            .find_map(|line| line.split_once(PARKED_MARKER))
            .map(|(_, payload)| payload.trim().to_string())
            .unwrap_or_else(|| panic!("the parked marker vanished from:\n{output}"))
    }

    /// The process id the parked reader published in its own breadcrumb, when
    /// it published one. Read off the scaffold's marker, and checked against
    /// the standard library's handle by the caller, so a store that named a
    /// holder can be held to naming the RIGHT one.
    fn published_pid(marker: &str) -> Option<u64> {
        if marker == UNVERIFIED_HOLDER {
            return None;
        }
        marker.split(':').nth(1).and_then(|pid| pid.parse().ok())
    }

    /// Let the reader finish hydrating and leave. The store is free from here.
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

/// What the store says when readers hold it, as an operator reads it:
///
/// > the store at &lt;path&gt; is busy: 1 direct reader is still reading it, so a
/// > writer cannot take it yet; it clears when they finish. Verified process
/// > &lt;pid&gt; (&lt;name&gt;)
///
/// Retyped here on purpose rather than reached for as a constant: this is
/// contextdb's operator-facing wording arriving through two crates, and a test
/// that shared the constant would stop noticing if the runtime began reporting
/// some other refusal entirely.
///
/// The needle is deliberately a FRAGMENT and not the sentence. One reader and
/// several readers are worded differently (`1 direct reader is` against
/// `2 direct readers are`), the path and the count are per-run, and none of
/// that is the contract — what this test needs to know is only that the runtime
/// was refused because the store is busy with readers. `is busy:` carries that
/// and survives both numbers.
const HELD_BY_READERS: &str = "is busy:";

/// The second fragment of the same sentence, naming WHAT the store is busy
/// with. Both are required together: `is busy:` alone is short enough to turn
/// up in some unrelated line one day and quietly satisfy the wait, and a
/// runtime refused for any other reason must not be mistaken for this one.
/// `direct reader` is the longest run that survives both `1 direct reader is`
/// and `2 direct readers are`.
const READERS_HOLD_IT: &str = "direct reader";

/// The other half of the same sentence: the part that names who is holding it.
/// Kept separate because it is a different promise — the refusal must be
/// ACTIONABLE, not merely correct — and because pinning it apart from the
/// count means a build that dropped the holder while keeping the wording is
/// caught rather than absorbed.
const NAMES_THE_HOLDER: &str = "Verified process";

/// What the runtime prints when it has given up on the store.
const GAVE_UP: &str = "store_unreadable=true";

/// What it prints when it is up.
const CAME_UP: &str = "boot_phase=pipeline-up";

struct Deployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    store_path: PathBuf,
    config_path: PathBuf,
}

impl Deployment {
    /// A deployment whose store exists and holds nothing. It is deliberately
    /// empty: what is under test is who may open the file and when, and an
    /// empty store is the one shape every reader and every writer agrees is
    /// readable, so nothing in the file's contents can be mistaken for the
    /// refusal this test is about.
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment directory");
        let data_dir = tmp.path().join("data");
        let store_path = data_dir.join("store.contextgraph");
        drop(open_store_at(&store_path).expect("create the deployment's store"));
        let config_path = tmp.path().join("vigil.toml");
        fs::write(&config_path, "cameras = []\n").expect("write the configuration file");
        Self {
            _tmp: tmp,
            data_dir,
            store_path,
            config_path,
        }
    }

    fn start(&self) -> StartingVigil {
        let health = TcpPortReservation::reserve_loopback().expect("reserve a liveness port");
        let review = TcpPortReservation::reserve_loopback().expect("reserve a review port");
        let mut command = Command::new(vigil_binary_path());
        command
            .arg("run")
            .arg("--config")
            .arg(&self.config_path)
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--health-port")
            .arg(health.port().to_string())
            .arg("--review-port")
            .arg(review.port().to_string())
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn `vigil run`");
        let stdout = capture_pipe(child.stdout.take());
        let stderr = capture_pipe(child.stderr.take());
        StartingVigil {
            child,
            stdout,
            stderr,
        }
    }
}

struct StartingVigil {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

impl StartingVigil {
    fn logs(&self) -> String {
        let read =
            |pipe: &Arc<Mutex<String>>| pipe.lock().map(|text| text.clone()).unwrap_or_default();
        format!("{}{}", read(&self.stdout), read(&self.stderr))
    }

    /// Bounded wait for something the process itself printed. Every fragment
    /// must be present: a sentence is pinned by the parts of it that are
    /// stable, never by matching the whole of it, because the count and the
    /// path in it are per-run.
    fn wait_for(&self, what: &str, needles: &[&str]) -> Result<String, String> {
        wait_until(what, RUNTIME_STARTUP_TIMEOUT, || {
            let logs = self.logs();
            Ok(needles
                .iter()
                .all(|needle| logs.contains(needle))
                .then_some(logs))
        })
    }

    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for StartingVigil {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn vigil_command(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(vigil_binary_path())
        .args(args)
        .env("VIGIL_DATA_DIR", data_dir)
        .env_remove("VIGIL_STORE_PATH")
        .env_remove("VIGIL_CONTROL_SOCKET")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("spawn `vigil {}`: {error}", args.join(" ")))
}

/// The pin: a momentary refusal is momentary, and the runtime treats it that
/// way. Unfakeable because the reader is genuinely holding the store when the
/// runtime tries to open it — the runtime says so itself before the reader is
/// released — and because the store is only released once, so a runtime that
/// never retried can never end up owning it.
#[test]
fn a_hydrating_reader_does_not_permanently_degrade_a_starting_runtime() {
    let deployment = Deployment::new();
    let mut reader = ParkedReader::park(&deployment.store_path);
    let _parked = reader.wait_until_parked();

    let vigil = deployment.start();
    let refusal = vigil
        .wait_for(
            "the starting runtime to report the store held by readers",
            &[HELD_BY_READERS, READERS_HOLD_IT],
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}. This run never established the condition it exists to test: a direct \
                 reader was holding the store and the runtime never reported being refused for \
                 that reason. Output:\n{}",
                vigil.logs()
            )
        });
    assert!(
        !refusal.contains(GAVE_UP),
        "the runtime declared the store unreadable on a refusal that says the condition clears \
         when the readers finish, so a deployment loses recording, review history, corrections \
         and settings changes because somebody read it at the wrong second; got:\n{refusal}"
    );

    // The reader finishes. Nothing else ever holds this store, so from here the
    // only thing that can open it is a retry.
    reader.release();

    let up = vigil
        .wait_for(
            "the runtime to retry and come up owning the store",
            &[CAME_UP],
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}. The reader let go, so the store was there to be taken and the runtime \
                 had to take it. Output:\n{}",
                vigil.logs()
            )
        });
    assert!(
        !up.contains(GAVE_UP),
        "the runtime came up still calling the store unreadable, so it never retried the open \
         after the reader let go; got:\n{up}"
    );

    let events = vigil_command(&deployment.data_dir, &["events"]);
    vigil.stop();
    assert!(
        events.status.success()
            && String::from_utf8_lossy(&events.stdout).starts_with(OWNER_SERVED_PREFIX),
        "after retrying, the runtime must own the store and answer over the owner route; exited \
         {:?}, stdout:\n{}\nstderr:\n{}",
        events.status.code(),
        String::from_utf8_lossy(&events.stdout),
        String::from_utf8_lossy(&events.stderr)
    );
}

/// The refusal has to be actionable. An operator reading only that their store
/// is busy with one direct reader cannot tell WHICH of their own commands is
/// holding it; the store knows, and what it knows has to reach the log line.
#[test]
fn a_startup_open_refused_by_readers_names_the_readers_holding_the_store() {
    let deployment = Deployment::new();
    let mut reader = ParkedReader::park(&deployment.store_path);
    let marker = reader.wait_until_parked();
    let reader_pid = reader.pid();
    assert_ne!(
        reader_pid,
        std::process::id(),
        "sanity: the reader must be a different process from the one asserting, or the pid check \
         below proves nothing"
    );
    // If the parked reader published no breadcrumb there is nothing for the
    // store to name, and a refusal that named nobody would be correct rather
    // than defective. That is a different condition, and this test says so
    // instead of reporting a defect that is not there.
    let published = ParkedReader::published_pid(&marker).unwrap_or_else(|| {
        panic!(
            "this run never established the condition it exists to test: the parked reader \
             published no breadcrumb ({marker:?}), so the store has no holder to name and the \
             assertion below would be checking nothing"
        )
    });
    assert_eq!(
        published,
        u64::from(reader_pid),
        "sanity: the breadcrumb the reader parked with must be the reader's own, or the refusal \
         below is being checked against the wrong process"
    );

    let vigil = deployment.start();
    let refusal = vigil
        .wait_for(
            "the starting runtime to report the store held by readers",
            &[HELD_BY_READERS, READERS_HOLD_IT],
        )
        .unwrap_or_else(|error| panic!("{error}. Output:\n{}", vigil.logs()));
    reader.release();
    vigil.stop();

    let holder = format!("{NAMES_THE_HOLDER} {reader_pid}");
    assert!(
        refusal.contains(&holder),
        "the refusal named no reader, so an operator is told their store is held and given \
         nothing to act on; it must name the holding process as {holder:?} — the reader this \
         test spawned is the only one there is, and its pid came off the standard library's own \
         handle rather than out of the text being checked; got:\n{refusal}"
    );
}
