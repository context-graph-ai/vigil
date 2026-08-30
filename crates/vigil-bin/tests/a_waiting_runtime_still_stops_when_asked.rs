//! A node waiting for a reader to finish must still stop when its operator
//! stops it.
//!
//! The ruled startup contract (owner ruling of 2026-08-25, folded into
//! `contextdb-read-intent.md`) is that a runtime starting while someone reads
//! its store waits for exactly as long as the typed reader-held condition
//! lasts — no timeout, no deadline, no unmanaged start. That wait is right,
//! and this test does not weaken it. What it pins is the OTHER promise the
//! same runtime carries: the installable substrate promises an orderly exit
//! inside its stop grace, and a `systemctl stop`, a container stop, or a Home
//! Assistant add-on restart all arrive as SIGTERM.
//!
//! Those two promises meet inside the retry loop, and a wait with no way to be
//! cancelled queues the signal behind a wait that ends only when the reader
//! lets go: an operator's stop hangs until the grace expires and the supervisor
//! escalates to SIGKILL — exactly the ungraceful exit the substrate promised
//! not to have. Cancellation is NOT a timeout: nothing here asks the runtime to
//! give up on the store, only to stop waiting when it has been told to stop.
//!
//! Being able to notice the stop is only half of it, and the second half is
//! what an operator actually sees go wrong. A loop that looks at the store
//! before it looks at the stop takes the store whenever the reader happens to
//! let go first — so the stop is noticed, and the node comes up anyway. That is
//! a node the operator did not ask for, holding a store their supervisor now
//! has to stop it out of a second time. Both proofs live here because they are
//! the same promise from the same window.
//!
//! Unfakeable: the reader really holds the store, parked inside hydration
//! through contextdb's own checkpoint in a separate process, so the runtime is
//! genuinely mid-wait and says so on its own output before the signal is sent;
//! the signal is a real SIGTERM to a real operating-system process; the exit
//! status is read from that process rather than inferred; and the store is
//! checked afterwards for having been neither taken nor written, so a runtime
//! that "stopped" by barging past the reader fails just as surely as one that
//! hung.

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use contextdb_engine::ReadSession;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, TcpPortReservation, capture_pipe, open_store_at, vigil_binary_path,
    wait_until,
};

/// The stop grace the installable substrate promises an orderly exit inside.
/// Read as the window the exit must fit in, never as a deadline the product is
/// asked to enforce on its own wait.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// What the runtime prints while it is waiting for readers — the proof the
/// condition is established before anything is asked to stop.
const WAITING_MARKER: &str = "store_open_waiting_for_readers=true";

/// What it must NOT print: giving up on the store is a different outcome from
/// being asked to stop, and only one of them is being pinned here.
const GAVE_UP: &str = "store_unreadable=true";

/// What a runtime prints on the way out when the stop reached it before it ever
/// owned the store: nothing was taken and nothing was written.
const STOPPED_BEFORE_STORE_OPEN: &str = "boot_phase=stopped-before-store-open";

/// What it prints when it TOOK the store after waiting, and what it prints when
/// that open finished. Either one on the output of a run that was asked to stop
/// says the stop lost: the store moved to a process on its way out of existence.
const READERS_CLEARED: &str = "store_open_readers_cleared=true";
const STORE_OPEN_DONE: &str = "boot_phase=store-open-done";

const HOLDER_STORE_VARIABLE: &str = "VIGIL_TEST_STOPPING_HOLDER_STORE";
const HOLDER_TEST_NAME: &str =
    "a_direct_reader_parks_inside_hydration_while_the_runtime_is_stopped";
const PARKED_MARKER: &str = "READ_IMAGE_HYDRATION_HELD=";
const RELEASE_COMMAND: &str = "release";

/// The reader that holds the store mid-hydration, in a process of its own.
/// Same protocol as `hydrating_reader_does_not_degrade_a_starting_runtime.rs`:
/// a finished read session does not hold the store against a writer, so the
/// reader has to be stopped INSIDE hydration, which contextdb's own checkpoint
/// is the only way to do.
#[test]
#[ignore = "re-executed as the hydration-holding child process; not an ordinary test"]
// The parent hands this child its store path through the environment because it
// re-executes the test binary (allow: test IPC channel).
#[allow(clippy::disallowed_methods)]
fn a_direct_reader_parks_inside_hydration_while_the_runtime_is_stopped() {
    let Ok(store_path) = std::env::var(HOLDER_STORE_VARIABLE) else {
        return;
    };
    contextdb_engine::persistence::read_persistence_test_scaffold::
        arm_read_image_hydration_pause_for_test();
    let session = ReadSession::open_with_options(
        Path::new(&store_path),
        context_graph::owner_control::context_graph_read_session_options(),
    )
    .expect("a direct reader must be able to open the store nobody owns");
    drop(session);
}

struct ParkedReader {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

impl ParkedReader {
    fn park(store_path: &Path) -> Self {
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
        let read = |pipe: &Arc<Mutex<String>>| pipe.lock().map(|t| t.clone()).unwrap_or_default();
        format!("{}{}", read(&self.stdout), read(&self.stderr))
    }

    fn wait_until_parked(&mut self) {
        let outcome = {
            let child = &mut self.child;
            let stdout = self.stdout.clone();
            wait_until(
                "the direct reader to park inside hydration",
                RUNTIME_STARTUP_TIMEOUT,
                move || {
                    if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                        return Err(format!(
                            "the reader exited ({status}) instead of parking inside hydration, so \
                             the runtime below would never have been mid-wait"
                        ));
                    }
                    let logs = stdout.lock().map(|t| t.clone()).unwrap_or_default();
                    Ok(logs.contains(PARKED_MARKER).then_some(()))
                },
            )
        };
        if let Err(error) = outcome {
            panic!("{error}. The reader's output was:\n{}", self.output());
        }
    }

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

struct Deployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    store_path: PathBuf,
    config_path: PathBuf,
    owner_runtime: PathBuf,
}

impl Deployment {
    fn prepare() -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment directory");
        let data_dir = tmp.path().join("data");
        let store_path = data_dir.join("store.contextgraph");
        drop(open_store_at(&store_path).expect("create the deployment's store"));
        let config_path = tmp.path().join("vigil.toml");
        fs::write(&config_path, "cameras = []\n").expect("write the configuration file");
        let owner_runtime = tmp.path().join("owner-plane");
        fs::create_dir(&owner_runtime).expect("create the task-scoped owner runtime directory");
        fs::set_permissions(&owner_runtime, fs::Permissions::from_mode(0o700))
            .expect("make the task-scoped owner runtime directory owner-only");
        Self {
            _tmp: tmp,
            data_dir,
            store_path,
            config_path,
            owner_runtime,
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
            .env("CONTEXTDB_OWNER_READ_RUNTIME_DIR", &self.owner_runtime)
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

    /// Ask the one command that can safely probe the owner before its store is
    /// ready: the stopped fallback reads only the deployment's counters file,
    /// while the live answer proves the custom handler and its store are both up.
    fn owner_stats(&self) -> Output {
        Command::new(vigil_binary_path())
            .arg("stats")
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env("VIGIL_STORE_PATH", &self.store_path)
            .env("CONTEXTDB_OWNER_READ_RUNTIME_DIR", &self.owner_runtime)
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .output()
            .expect("ask the running owner for stats")
    }

    /// Start a settings listing and retain the process while its owner request
    /// is deliberately held inside the runtime.
    fn start_owner_settings(&self) -> StartingVigil {
        let mut child = Command::new(vigil_binary_path())
            .arg("settings")
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env("VIGIL_STORE_PATH", &self.store_path)
            .env("CONTEXTDB_OWNER_READ_RUNTIME_DIR", &self.owner_runtime)
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("ask the running owner for its settings");
        let stdout = capture_pipe(child.stdout.take());
        let stderr = capture_pipe(child.stderr.take());
        StartingVigil {
            child,
            stdout,
            stderr,
        }
    }

    /// Replace this disposable deployment's node-key file with a FIFO and
    /// return the bytes a held reader will eventually receive.
    ///
    /// A settings listing reads this file only after the owner handler has
    /// upgraded its weak store slot. Holding that read therefore fixes the
    /// exact lifetime ordering the orderly-stop regression needs without a
    /// clock, a production setting, or a test-only branch in the runtime.
    fn park_settings_on_node_key(&self) -> (PathBuf, Vec<u8>) {
        let node_key = self.data_dir.join("node-key");
        let original = fs::read(&node_key).expect("read the runtime's recorded node key");
        fs::rename(
            &node_key,
            self.data_dir.join("node-key.before-orderly-stop-proof"),
        )
        .expect("move the disposable node key aside before installing the FIFO");
        let encoded = CString::new(node_key.as_os_str().as_bytes())
            .expect("a temporary path contains no interior NUL");
        // SAFETY: `encoded` is the exact task-scoped path inside this test's
        // TempDir, is NUL-terminated by CString, and did not exist after the
        // rename above. No wildcard or caller-owned path is involved.
        let created = unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) };
        assert_eq!(
            created,
            0,
            "create the task-scoped node-key FIFO: {}",
            std::io::Error::last_os_error()
        );
        (node_key, original)
    }

    fn owner_sockets(&self) -> Vec<PathBuf> {
        let mut sockets: Vec<_> = fs::read_dir(&self.owner_runtime)
            .expect("read the task-scoped owner runtime directory")
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(FileTypeExt::is_socket)
                    .map(|_| entry.path())
            })
            .collect();
        sockets.sort();
        sockets
    }
}

struct StartingVigil {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

impl StartingVigil {
    fn logs(&self) -> String {
        let read = |pipe: &Arc<Mutex<String>>| pipe.lock().map(|t| t.clone()).unwrap_or_default();
        format!("{}{}", read(&self.stdout), read(&self.stderr))
    }

    fn wait_for(&self, what: &str, needle: &str) -> Result<String, String> {
        wait_until(what, RUNTIME_STARTUP_TIMEOUT, || {
            let logs = self.logs();
            Ok(logs.contains(needle).then_some(logs))
        })
    }

    /// Send one signal to this process, the way its supervisor does.
    ///
    /// The one raw signal site in this fixture. Always to the positive pid of a
    /// child this test spawned and owns, taken from the standard library's own
    /// handle — never a formatted target, never a process group, never a
    /// wildcard. The standard library offers only `kill`, which is SIGKILL, and
    /// a SIGKILL here would prove the opposite of what is being asked: that the
    /// process died, not that it stopped in an orderly way when told to.
    fn signal(&self, signal: i32, what: &str) {
        let pid = i32::try_from(self.child.id()).expect("a child pid fits in a pid_t");
        // SAFETY: `pid` is this test's own direct child, taken from the handle
        // that spawned it and still alive (unreaped), so it names that process
        // and nothing else. `kill` with a positive pid has no other effect.
        let sent = unsafe { libc::kill(pid, signal) };
        assert_eq!(
            sent,
            0,
            "{what} must reach the runtime, or nothing below is being tested: {}",
            std::io::Error::last_os_error()
        );
    }

    /// Ask this process to stop, the way `systemctl stop`, a container stop and
    /// a Home Assistant add-on restart all do.
    fn ask_to_stop(&self) {
        self.signal(libc::SIGTERM, "SIGTERM");
    }

    /// Freeze this process where it stands, so what happens next happens in an
    /// order this test chose rather than in one the scheduler chose.
    ///
    /// This is the whole reason the race below is a regression and not a
    /// coin toss: while the runtime is held still, the stop can be delivered
    /// AND the reader can let go, both provably before the runtime's next look
    /// at the store. SIGSTOP cannot be caught, so it changes nothing about what
    /// the runtime does — only when it does it.
    fn hold_still(&self) {
        self.signal(libc::SIGSTOP, "SIGSTOP");
    }

    /// Let it run again, with the stop already queued for it.
    fn let_it_continue(&self) {
        self.signal(libc::SIGCONT, "SIGCONT");
    }

    /// Wait until the operating system reports this process as stopped, so the
    /// ordering above is a fact read off the process rather than an assumption
    /// about how promptly a signal lands.
    fn wait_until_held_still(&self) {
        let pid = self.child.id();
        let outcome = wait_until(
            "the runtime to be held still mid-wait",
            RUNTIME_STARTUP_TIMEOUT,
            || {
                let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
                    .map_err(|error| error.to_string())?;
                // The command name sits in parentheses and may itself contain
                // spaces, so the state letter is read after the LAST one.
                let state = stat
                    .rsplit(')')
                    .next()
                    .and_then(|rest| rest.split_whitespace().next())
                    .map(str::to_string);
                Ok((state.as_deref() == Some("T")).then_some(()))
            },
        );
        if let Err(error) = outcome {
            panic!("{error}. Output so far:\n{}", self.logs());
        }
    }

    /// Wait for the process to exit, up to the substrate's stop grace.
    ///
    /// A bounded state poll through the estate's own helper, never a hand-rolled
    /// clock: what is being read is the process's exit status, and the grace is
    /// the window that status has to land inside — not a duration anything here
    /// measures or asserts on.
    fn wait_for_exit(&mut self, grace: Duration) -> Option<std::process::ExitStatus> {
        let child = &mut self.child;
        wait_until(
            "the runtime to exit after being asked to stop",
            grace,
            || child.try_wait().map_err(|error| error.to_string()),
        )
        .ok()
    }
}

impl Drop for StartingVigil {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An ordinary owner-ready run must begin owner shutdown even while one
/// request is inside its handler, then take down the exact local channel it
/// created before its clean SIGTERM exit becomes observable.
///
/// This catches a lifetime cycle that every route-parity test misses: a dead
/// process releases the kernel listener, so a later command can recover the
/// stale pathname and still print the right answer. The pathname itself is the
/// contract here. The test gives the owner a private task-scoped runtime root,
/// proves a real `stats` request is served through it, parks a real settings
/// request after its handler has borrowed the store, sends the supervisor's
/// real SIGTERM, observes the owner refusing new work while that request is
/// still held, releases it, waits for that request to finish without prescribing
/// whether cancellation or its answer wins, reads the runtime's exit zero, and
/// then inspects the root.
#[test]
fn an_orderly_stop_removes_the_owner_socket_created_by_that_run() {
    let deployment = Deployment::prepare();
    let mut vigil = deployment.start();
    wait_until(
        "the ordinary runtime to answer through its owner socket",
        RUNTIME_STARTUP_TIMEOUT,
        || {
            let answer = deployment.owner_stats();
            let stdout = String::from_utf8_lossy(&answer.stdout);
            Ok((answer.status.success()
                && stdout.starts_with(vigil::OWNER_SERVED_PREFIX)
                && !stdout.contains("owner-error"))
            .then_some(()))
        },
    )
    .unwrap_or_else(|error| panic!("{error}. Output so far:\n{}", vigil.logs()));

    let live_sockets = deployment.owner_sockets();
    assert_eq!(
        live_sockets.len(),
        1,
        "one owner-ready run must publish exactly one task-scoped socket: {live_sockets:?}"
    );

    let (node_key_fifo, node_key_bytes) = deployment.park_settings_on_node_key();
    let mut held_request = deployment.start_owner_settings();
    let mut fifo_writer = wait_until(
        "the owner settings handler to open the node-key FIFO for reading",
        RUNTIME_STARTUP_TIMEOUT,
        || match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&node_key_fifo)
        {
            Ok(writer) => Ok(Some(writer)),
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => Ok(None),
            Err(error) => Err(format!(
                "open the task-scoped node-key FIFO writer: {error}"
            )),
        },
    )
    .unwrap_or_else(|error| {
        panic!(
            "{error}. Runtime output:\n{}\nHeld request output:\n{}",
            vigil.logs(),
            held_request.logs()
        )
    });
    assert!(
        held_request
            .child
            .try_wait()
            .expect("inspect the held settings request")
            .is_none(),
        "the settings request must still be inside the owner handler before SIGTERM; output:\n{}",
        held_request.logs()
    );

    vigil.ask_to_stop();
    wait_until(
        "the owner to stop admitting new requests while the first one remains in flight",
        STOP_GRACE,
        || {
            if let Some(status) = vigil.child.try_wait().map_err(|error| error.to_string())? {
                return Err(format!(
                    "the runtime exited {status} before beginning ContextDB owner shutdown"
                ));
            }
            let answer = deployment.owner_stats();
            let rendered = format!(
                "{}{}",
                String::from_utf8_lossy(&answer.stdout),
                String::from_utf8_lossy(&answer.stderr)
            );
            Ok(
                (!answer.status.success() && rendered.contains("does not answer owner requests"))
                    .then_some(()),
            )
        },
    )
    .unwrap_or_else(|error| {
        panic!(
            "{error}. Runtime output:\n{}\nHeld request output:\n{}",
            vigil.logs(),
            held_request.logs()
        )
    });
    assert_eq!(
        deployment.owner_sockets().len(),
        1,
        "a draining owner keeps its exact channel until admitted work leaves"
    );

    fs::remove_file(&node_key_fifo).expect("unlink the node-key FIFO after proving the hold");
    fs::rename(
        deployment
            .data_dir
            .join("node-key.before-orderly-stop-proof"),
        &node_key_fifo,
    )
    .expect("restore the regular node-key file before later settings reads");
    fifo_writer
        .write_all(&node_key_bytes)
        .expect("release the held settings read with the original node key");
    drop(fifo_writer);
    held_request.wait_for_exit(STOP_GRACE).unwrap_or_else(|| {
        panic!(
            "the admitted settings request did not finish after its FIFO was released. Output:\n{}",
            held_request.logs()
        )
    });

    let status = vigil.wait_for_exit(STOP_GRACE).unwrap_or_else(|| {
        panic!(
            "the owner-ready runtime was still running {STOP_GRACE:?} after SIGTERM. Output:\n{}",
            vigil.logs()
        )
    });
    assert_eq!(
        status.code(),
        Some(0),
        "an orderly owner-ready stop must exit zero. Output:\n{}",
        vigil.logs()
    );
    assert!(
        deployment.owner_sockets().is_empty(),
        "the process exited cleanly but left its owner socket behind; a stopped command would have \
         to recover a stale owner instead of seeing an idle store. Remaining sockets: {:?}\nOutput:\n{}",
        deployment.owner_sockets(),
        vigil.logs()
    );
}

/// Unfakeable because the runtime states on its own output that it is waiting
/// before anything is asked of it, so the stop lands squarely inside the wait;
/// because the exit status comes from the real process rather than from a
/// timeout the test chose; and because the store is checked afterwards, so a
/// runtime that escaped the wait by taking the store from under the reader
/// fails exactly as hard as one that hung.
#[test]
fn a_runtime_waiting_for_a_reader_stops_when_it_is_asked_to() {
    let deployment = Deployment::prepare();
    let store_before = fs::metadata(&deployment.store_path)
        .and_then(|meta| meta.modified())
        .expect("the seeded store's modification time");
    let store_bytes_before = fs::read(&deployment.store_path).expect("read the seeded store");

    let mut reader = ParkedReader::park(&deployment.store_path);
    reader.wait_until_parked();

    let mut vigil = deployment.start();
    vigil
        .wait_for(
            "the runtime to report that it is waiting for readers",
            WAITING_MARKER,
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}. This run never established the condition it exists to test: the runtime \
                 has to be MID-WAIT when it is asked to stop. Output:\n{}",
                vigil.logs()
            )
        });

    vigil.ask_to_stop();
    let outcome = vigil.wait_for_exit(STOP_GRACE);
    let logs = vigil.logs();
    reader.release();

    let status = outcome.unwrap_or_else(|| {
        panic!(
            "the runtime was still running {STOP_GRACE:?} after it was asked to stop, so a \
             supervisor would now be escalating to SIGKILL — the ungraceful exit the installable \
             substrate promises not to have. The wait for the reader is right and is not what is \
             being questioned; a wait with no way to be cancelled is. Output:\n{logs}"
        )
    });
    assert_eq!(
        status.code(),
        Some(0),
        "and it must stop CLEANLY: an operator who typed stop got a failure status instead, \
         which reads as a fault on a node that did nothing wrong. Output:\n{logs}"
    );
    assert!(
        !logs.contains(GAVE_UP),
        "stopping is not giving up on the store: the runtime must not declare it unreadable on \
         its way out, or a restart inherits a verdict nothing established. Output:\n{logs}"
    );

    // The reader still held the store for the whole of that, so the runtime
    // cannot have taken it — and it must not have written to it either.
    let store_after = fs::metadata(&deployment.store_path)
        .and_then(|meta| meta.modified())
        .expect("the store's modification time after the stop");
    assert_eq!(
        store_after, store_before,
        "a runtime that stopped while waiting for a reader never took the store, so it cannot \
         have written to it; the store's modification time moved. Output:\n{logs}"
    );
    assert_eq!(
        fs::read(&deployment.store_path).expect("read the store after the stop"),
        store_bytes_before,
        "and its bytes must be exactly as the reader left them. Output:\n{logs}"
    );
}

/// The stop must win even when the reader lets go in the same instant.
///
/// A stop is not advice. Once an operator has stopped a node, that node must
/// not go on to take its store and come up — a supervisor that asked for a
/// stop and got a started runtime has a node it did not ask for, holding a
/// store it will have to be stopped out of again, and on a restart-on-stop
/// supervisor the two race each other for the store.
///
/// The window is real and narrow: the runtime pauses between attempts, so a
/// stop that arrives during that pause and a reader that finishes during the
/// same pause both land before the runtime looks again. Whichever it notices
/// first decides the outcome, and noticing the store first is the wrong answer.
///
/// Deterministic rather than hopeful: the runtime is FROZEN mid-wait — read
/// back off the operating system, not assumed — and only then is the stop
/// delivered and the reader released and reaped. Both are facts before the
/// runtime takes another breath, so this ordering is the one every run gets.
#[test]
fn a_stop_delivered_before_the_reader_lets_go_beats_the_store_open() {
    let deployment = Deployment::prepare();
    let store_before = fs::metadata(&deployment.store_path)
        .and_then(|meta| meta.modified())
        .expect("the seeded store's modification time");
    let store_bytes_before = fs::read(&deployment.store_path).expect("read the seeded store");

    let mut reader = ParkedReader::park(&deployment.store_path);
    reader.wait_until_parked();

    let mut vigil = deployment.start();
    vigil
        .wait_for(
            "the runtime to report that it is waiting for readers",
            WAITING_MARKER,
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}. This run never established the condition it exists to test: the \
                 runtime has to be MID-WAIT before the race is set up. Output:\n{}",
                vigil.logs()
            )
        });

    // The race, arranged so it is not a race. Frozen first, so the two events
    // below cannot be observed in the other order by the process they are
    // aimed at.
    vigil.hold_still();
    vigil.wait_until_held_still();
    vigil.ask_to_stop();
    // Released AND reaped while the runtime is still frozen: the store is
    // genuinely free before the runtime's next attempt, which is exactly the
    // ordering that makes a store-first loop take it.
    reader.release();
    vigil.let_it_continue();

    let outcome = vigil.wait_for_exit(STOP_GRACE);
    let logs = vigil.logs();

    let status = outcome.unwrap_or_else(|| {
        panic!(
            "the runtime was still running {STOP_GRACE:?} after it was asked to stop. Output:\n\
             {logs}"
        )
    });
    assert_eq!(
        status.code(),
        Some(0),
        "a stop that arrived before the store did is still an ordinary stop, and an operator \
         who typed it must not read a failure status. Output:\n{logs}"
    );
    assert!(
        !logs.contains(READERS_CLEARED) && !logs.contains(STORE_OPEN_DONE),
        "this runtime was told to stop and then TOOK ITS STORE anyway: the reader let go first \
         and the loop looked at the store before it looked at the stop. The operator's stop is \
         now a node that came up owning a store, which their supervisor will have to stop \
         again — and on a restart-on-stop supervisor, race for. Output:\n{logs}"
    );
    assert!(
        logs.contains(STOPPED_BEFORE_STORE_OPEN),
        "and it must say WHICH ending this was: stopped before it ever owned its store, so an \
         operator reading the journal after a stop that raced a release is not left guessing \
         whether this node touched their store. Output:\n{logs}"
    );
    assert!(
        !logs.contains(GAVE_UP),
        "stopping is not giving up on the store: a restart must not inherit a verdict nothing \
         established. Output:\n{logs}"
    );

    // Nothing was taken, so nothing can have been written — including by a run
    // that opened the store and only then noticed it had been stopped.
    let store_after = fs::metadata(&deployment.store_path)
        .and_then(|meta| meta.modified())
        .expect("the store's modification time after the stop");
    assert_eq!(
        store_after, store_before,
        "a runtime that stopped before it owned the store cannot have written to it; the \
         store's modification time moved. Output:\n{logs}"
    );
    assert_eq!(
        fs::read(&deployment.store_path).expect("read the store after the stop"),
        store_bytes_before,
        "and its bytes must be exactly as the reader left them. Output:\n{logs}"
    );
}
