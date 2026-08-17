//! Three different store-open failures get three different answers, and this is
//! the pair that must not collapse into "the store did not open":
//!
//! - **Absent** is a first start on a new machine. Vigil creates the store; it
//!   is the normal case, not an error.
//! - **Locked by another runtime** is a refusal. Another Vigil runtime owns this
//!   data directory, and Vigil says which process holds it.
//!
//! The holder is carried as a process id in the typed variant on BOTH sides —
//! the classification and the refusal the second open returns — never buried in
//! a message string, because an operator surface that has to parse prose to name
//! the holder cannot render it reliably.
//!
//! The lock in the locked test is held by a SEPARATE PROCESS — this test binary
//! re-executed against the same deployment directory — because a holder pid
//! taken from the classifying process itself is a number the classifier already
//! has: an implementation that reported its own pid, having identified no holder
//! at all, would pass. A pid that belongs to a child this test spawned can only
//! have come from real lock ownership.
//!
//! Both call sites hand `SettingsStore::open` the DEPLOYMENT DIRECTORY. Where a
//! test needs the store FILE — to check that classification created nothing —
//! it asks `SettingsStore::store_path`, so no filename is assumed here.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vigil::settings_degraded::{StoreOpenClass, classify_store_open};
use vigil::settings_model::SettingsError;
use vigil::settings_store::SettingsStore;

#[path = "deterministic_test_support.rs"]
mod deterministic_test_support;
use deterministic_test_support::{capture_pipe, wait_until};

/// The deployment directory the re-executed lock holder opens.
const HOLDER_DIRECTORY_VARIABLE: &str = "VIGIL_TEST_LOCK_HOLDER_DIRECTORY";
/// Where the lock holder writes its own pid once the store is genuinely open.
const HOLDER_READY_VARIABLE: &str = "VIGIL_TEST_LOCK_HOLDER_READY_FILE";
/// The name of the helper below, as the test harness filters on it.
const HOLDER_TEST_NAME: &str =
    "a_separate_process_holds_the_store_lock_while_the_parent_classifies";

/// The lock holder. Never part of an ordinary run: it is `#[ignore]`d, and it
/// does nothing at all unless the parent handed it a deployment directory
/// through the environment. Re-executed by
/// `a_locked_store_still_refuses_a_second_runtime_naming_the_holder`, it opens
/// the store, publishes its own pid, and then holds the open handle until its
/// parent kills it.
#[test]
#[ignore = "re-executed as the lock-holder child process; not an ordinary test"]
// The parent hands this child its directory and ready-file paths through the
// environment because it re-executes the test binary (allow: test IPC channel).
#[allow(clippy::disallowed_methods)]
fn a_separate_process_holds_the_store_lock_while_the_parent_classifies() {
    let Ok(directory) = std::env::var(HOLDER_DIRECTORY_VARIABLE) else {
        return;
    };
    let ready = std::env::var(HOLDER_READY_VARIABLE).expect("the parent names the ready file");

    let held = SettingsStore::open(Path::new(&directory)).expect("the holder opens the store");
    std::fs::write(&ready, std::process::id().to_string()).expect("publish the holder pid");

    // Held open until the parent kills this process. The parent's assertions all
    // run inside this window; nothing here waits on the parent, so there is no
    // handshake to time out in the other direction.
    std::thread::sleep(Duration::from_secs(120));
    drop(held);
}

/// Kills and reaps through the child handle the standard library owns, so
/// nothing here formats a pid or a negative process-group signal target. Runs on
/// the panicking path too, so a failed assertion cannot strand the holder.
struct HolderProcess {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

impl HolderProcess {
    fn spawn(data_dir: &Path, ready_file: &Path) -> Self {
        // This very test binary, re-executed. The program expression is a
        // resolved path to an already-running executable — it can never name
        // cargo or a container runtime — and it is the only way to get a
        // SEPARATE process holding the lock without dragging the whole product
        // binary and its configuration surfaces into a store-open test.
        let lock_holder_binary = std::env::current_exe().expect("this test binary's own path");
        let mut child = Command::new(&lock_holder_binary)
            .arg("--exact")
            .arg(HOLDER_TEST_NAME)
            .arg("--ignored")
            .arg("--nocapture")
            .env(HOLDER_DIRECTORY_VARIABLE, data_dir)
            .env(HOLDER_READY_VARIABLE, ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the lock-holder process");
        // Drained on background threads, so the holder can never block on a
        // full pipe and a failure report never blocks reading a live child.
        let stdout = capture_pipe(child.stdout.take());
        let stderr = capture_pipe(child.stderr.take());
        Self {
            child,
            stdout,
            stderr,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn output(&self) -> String {
        let out = self.stdout.lock().expect("holder stdout").clone();
        let err = self.stderr.lock().expect("holder stderr").clone();
        format!("{out}{err}")
    }

    /// Block until the holder has the store genuinely open, proven by the pid it
    /// writes only after the open returns. An exit before that is a failure,
    /// never a wait: there would be no holder left for the classification to
    /// name.
    fn wait_until_holding(&mut self, ready_file: &Path) {
        let outcome = {
            let child = &mut self.child;
            wait_until(
                "the lock-holder process to report holding the store open",
                Duration::from_secs(30),
                || {
                    if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                        return Err(format!(
                            "the lock-holder process exited ({status}) instead of holding the \
                             store open, so the classification below would have had no holder to \
                             name"
                        ));
                    }
                    Ok(match std::fs::read(ready_file) {
                        Ok(bytes) if !bytes.is_empty() => Some(()),
                        _ => None,
                    })
                },
            )
        };
        if let Err(error) = outcome {
            panic!("{error}. The holder's output was:\n{}", self.output());
        }
    }
}

impl Drop for HolderProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_locked_store_still_refuses_a_second_runtime_naming_the_holder() {
    // Unfakeable: the lock is held for real, for the whole test, by a process
    // that is NOT this one — so the holder the classification reports is
    // checkable against a pid this process knows independently and could not
    // have produced by echoing itself. A classifier that returned a plausible
    // "locked" answer without identifying the holder fails on the pid; one that
    // reported its own pid fails on the inequality; and one that identified the
    // holder without refusing the second open fails on the open. The refusal is
    // held to the same bar as the classification: it must carry the holder typed
    // and it must be the ownership refusal specifically, so an implementation
    // that collapses "someone else has this directory" into a generic store-open
    // failure is caught here rather than passing on a well-worded message. No
    // timing is involved in the assertions: the lock is a state, and the wait
    // below is on the holder publishing its pid, never on a duration.
    let dir = tempfile::tempdir().expect("temporary data directory");
    let signalling = tempfile::tempdir().expect("temporary directory for the readiness file");
    let ready_file = signalling.path().join("holder.pid");

    let mut holder = HolderProcess::spawn(dir.path(), &ready_file);
    let holder_pid = holder.pid();
    holder.wait_until_holding(&ready_file);

    let published = std::fs::read_to_string(&ready_file).expect("read the published holder pid");
    assert_eq!(
        published.trim().parse::<u32>().ok(),
        Some(holder_pid),
        "sanity: the process this test spawned is the process that opened the store, so a holder \
         pid equal to it means real lock ownership rather than a coincidence of numbering"
    );
    assert_ne!(
        holder_pid,
        std::process::id(),
        "sanity: the holder must be a different process from the one classifying, or the pid \
         assertion below proves nothing"
    );

    let class = classify_store_open(dir.path())
        .expect("a store held by another runtime is a classifiable open failure");
    assert_eq!(
        class,
        StoreOpenClass::LockedByAnotherRuntime {
            holder_pid: holder.pid()
        },
        "the classification must name the holder as a process id in the typed variant, so an \
         operator surface can render it without parsing prose"
    );
    match &class {
        StoreOpenClass::LockedByAnotherRuntime { holder_pid } => assert_ne!(
            *holder_pid,
            std::process::id(),
            "the holder pid must come from whoever actually owns the lock; reporting the \
             classifying process's own pid names nobody"
        ),
        other => panic!("a held store classifies as locked, not as {other:?}"),
    }

    let second = SettingsStore::open(dir.path());
    match second {
        Err(SettingsError::LockedByAnotherRuntime { holder_pid }) => {
            assert_eq!(
                holder_pid,
                holder.pid(),
                "the refusal names the runtime that actually owns the directory, carried typed on \
                 the error itself so a caller renders the holder without a second lookup and \
                 without parsing prose"
            );
            assert_ne!(
                holder_pid,
                std::process::id(),
                "a refusal naming the refused process itself names nobody: the holder pid must \
                 come from whoever owns the lock"
            );
        }
        Err(SettingsError::Store(detail)) => panic!(
            "a store held by another runtime is its own typed refusal, not an undifferentiated \
             store-open failure an operator has to read a message to understand; got \
             Store({detail:?})"
        ),
        Err(SettingsError::Refused(refusal)) => panic!(
            "a second runtime on a held store is refused by ownership of the data directory, not \
             by the settings gate; got {refusal:?}"
        ),
        Err(SettingsError::ScopeLabelViolation { requested, allowed }) => panic!(
            "a second runtime on a held store has nothing to do with which rank a handle may \
             write at; got a scope-label violation requesting {requested:?} against {allowed:?}"
        ),
        Ok(_) => panic!(
            "two runtimes on one store is not a configuration problem: the second runtime is \
             refused, never quietly admitted alongside the first"
        ),
    }
}

#[test]
fn an_absent_store_is_created_rather_than_treated_as_a_failure() {
    // Unfakeable: the store file is checked on disk both before and after the
    // classification, and then the store is actually opened. An implementation
    // that reported `Absent` by creating the store during classification is
    // caught by the "classification creates nothing" check; one that reported
    // `Absent` and then refused to open is caught by the open. The store file's
    // location comes from the product, not from a filename this test guessed.
    let dir = tempfile::tempdir().expect("temporary data directory");
    let store_file = SettingsStore::store_path(dir.path());
    assert!(
        !store_file.exists(),
        "the fixture must start with no store at all, or this proves nothing"
    );

    let class = classify_store_open(dir.path())
        .expect("a first start on a new machine is classifiable, not an error to propagate");
    assert_eq!(
        class,
        StoreOpenClass::Absent,
        "a store that does not exist yet is Absent — the normal first start — and must never be \
         classified as unreadable or as held by another runtime"
    );
    assert!(
        !store_file.exists(),
        "classifying an open failure must not itself create the store; creation is the runtime's \
         decision, taken after the classification says Absent"
    );

    let created = SettingsStore::open(dir.path());
    assert!(
        created.is_ok(),
        "an absent store is created rather than treated as a failure: {:?}",
        created.err()
    );
    assert!(
        store_file.exists(),
        "and opening an absent store is what creates it, at the location the product decides"
    );
}
