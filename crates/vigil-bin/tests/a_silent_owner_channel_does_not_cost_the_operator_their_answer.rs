//! A read command that could not get an answer over the owner channel must
//! still answer from the store, when the store is free to read.
//!
//! Vigil's read commands ask the process that owns the store first and read
//! the store themselves when nobody owns it. That dispatch is the promise:
//! the same command, the same answer, whether or not Vigil is running.
//!
//! At this branch's base (`crates/vigil/src/lib.rs`'s `print_control_or_direct`
//! at `dad9b1c`) EVERY failed owner request fell through to the direct read —
//! `Err(socket_error) => match direct_read_local(command, request)`. The
//! current tree gates that fall-through on
//! `may_read_the_store_directly(&owner_error)`, which is
//! `OwnerControlError::owner_absent()` and therefore true for exactly two
//! variants: `OwnerNotRunning` and `NoStore`. Every other refusal now exits 2
//! with the transport's own words and never looks at the store.
//!
//! For the refusals that mean a live owner really is holding the store, that
//! tightening is right and this file does not weaken it: opening the store
//! behind a live owner would either be refused by the lock or put a second
//! writer behind the first one's back, and answering from there is a different
//! deployment's answer.
//!
//! It is wrong for the transport classes where NO process holds the store. A
//! channel that accepts a connection and never answers is the plain case: the
//! store file is free, `vigil events` could read it in full, and instead the
//! operator is told "the process holding the store at <path> did not answer
//! inside the time this request allows" — about a store no process is holding.
//! They lose their review history to a road, not to their data, and the
//! sentence they are given to debug names a process that does not exist. That
//! is an unratified behavior change against the base cited above.
//!
//! What is pinned: the answer arrives, exit status says it was served, and the
//! ROUTE is reported honestly — a direct read carries no
//! `OWNER_SERVED_PREFIX`, so an operator (and every one of these pins) can
//! tell which road the answer came home on.
//!
//! How the condition is CONSTRUCTED, deterministically and with no product
//! seam: the owner channel is addressed by the store's path under the runtime
//! directory this deployment states (`CONTEXTDB_OWNER_READ_RUNTIME_DIR`, the
//! one variable both sides of the owner plane read). This test binds a real
//! socket at exactly that address and accepts connections without ever
//! answering, from a process that stays alive throughout — so the channel is
//! genuinely live, genuinely silent, and genuinely not a store owner. Nothing
//! here asserts on elapsed time: the assertions read the answer and the exit
//! status, never how long the transport took to give up.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;

use contextdb_engine::local_transport::{derive_channel_address, opaque_channel_basename};
use vigil::OWNER_SERVED_PREFIX;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{fresh_store_copy, vigil_binary_path};

/// The variable that states where this deployment's owner plane lives. Both
/// sides read it, which is what lets this test put the channel somewhere it
/// can bind a socket of its own.
const RUNTIME_DIR_VARIABLE: &str = "CONTEXTDB_OWNER_READ_RUNTIME_DIR";

/// Enough seeded observations that `events` has rows to render and `why
/// --latest` has a chain to walk — so an empty answer cannot be mistaken for a
/// served one.
const SEEDED_DETECTIONS: usize = 2;

/// The claim a silent channel must never produce: no process is holding this
/// store, and telling an operator one is sends them hunting a process that
/// does not exist.
const HOLDER_CLAIM: &str = "the process holding the store at";

/// A runtime root this test owns, at a short pathname: a local channel address
/// is capped by the kernel and the fixed channel basename is 69 bytes of it,
/// so the root has to be shallow. Owner-only, which is what the runtime
/// directory's own validation requires of it.
struct RuntimeRoot {
    path: PathBuf,
}

impl RuntimeRoot {
    fn create() -> Self {
        let base = PathBuf::from("/tmp");
        let mut path = base.join(format!("vg-own-{}", std::process::id()));
        let mut suffix = 0;
        while path.exists() {
            suffix += 1;
            path = base.join(format!("vg-own-{}-{suffix}", std::process::id()));
        }
        fs::create_dir(&path).expect("create the runtime root this deployment states");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("the runtime root is owner-only");
        Self { path }
    }
}

impl Drop for RuntimeRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A channel that accepts and never answers, at the exact address this store's
/// owner plane is addressed by.
///
/// Nothing here simulates a hang: the socket is real, the accept is real, and
/// the connection is held open by a live process for as long as this holder
/// exists. What it is NOT is a store owner — the store file is untouched and
/// free for any reader.
struct SilentChannel {
    _root: RuntimeRoot,
    root_path: PathBuf,
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
    accepting: Option<JoinHandle<()>>,
}

impl SilentChannel {
    fn bind(store_path: &Path) -> Self {
        let root = RuntimeRoot::create();
        let address =
            derive_channel_address(store_path).expect("derive this store's own channel address");
        let socket_path = root.path.join(opaque_channel_basename(address));
        let listener = UnixListener::bind(&socket_path).expect("bind the owner channel address");
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
            .expect("the channel is owner-only, as a real one is");
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicUsize::new(0));
        let accepting = {
            let stop = Arc::clone(&stop);
            let connections = Arc::clone(&connections);
            std::thread::spawn(move || {
                let mut held = Vec::new();
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            connections.fetch_add(1, Ordering::SeqCst);
                            // Held, never answered: the caller is connected to
                            // something alive that says nothing.
                            held.push(stream);
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        let root_path = root.path.clone();
        Self {
            _root: root,
            root_path,
            socket_path,
            stop,
            connections,
            accepting: Some(accepting),
        }
    }

    fn root(&self) -> &Path {
        &self.root_path
    }

    /// How many callers have reached this channel. Read as the proof the
    /// condition was really established — a command that never connected met
    /// no silent channel and proves nothing.
    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Take the channel down. The accept is woken by one connection of this
    /// test's own — the socket is still bound at that point, so the wake is
    /// certain — and the pathname is removed only once the accepting thread
    /// has left.
    fn release(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&self.socket_path);
        if let Some(handle) = self.accepting.take() {
            let _ = handle.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

impl Drop for SilentChannel {
    /// Runs on the panicking path too, so a failed assertion cannot strand a
    /// bound channel. The accepting thread is woken and left to exit on its
    /// own rather than joined, because a drop must never block.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&self.socket_path);
        let _ = fs::remove_file(&self.socket_path);
    }
}

fn run(data_dir: &Path, runtime_root: &Path, args: &[&str]) -> Output {
    Command::new(vigil_binary_path())
        .args(args)
        .env("VIGIL_DATA_DIR", data_dir)
        .env(RUNTIME_DIR_VARIABLE, runtime_root)
        .env_remove("VIGIL_STORE_PATH")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("spawn `vigil {}`: {error}", args.join(" ")))
}

fn text_of(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// One read command as an operator meets it, with the field its served answer
/// must carry so an empty or refused answer cannot pass as a served one.
struct Shape {
    label: &'static str,
    request: &'static [&'static str],
    served_field: &'static str,
}

const READ_SHAPES: [Shape; 2] = [
    Shape {
        label: "vigil events",
        request: &["events"],
        served_field: "observation_id=",
    },
    Shape {
        label: "vigil why --latest",
        request: &["why", "--latest"],
        served_field: "selection=",
    },
];

#[test]
fn a_silent_owner_channel_falls_back_to_the_store_and_answers() {
    let (tmp, store_path) =
        fresh_store_copy(SEEDED_DETECTIONS).expect("seed a deterministic detection fixture");
    let data_dir = store_path
        .parent()
        .expect("the seeded store sits inside a data directory")
        .to_path_buf();
    let channel = SilentChannel::bind(&store_path);

    let observed: Vec<(&Shape, Output)> = READ_SHAPES
        .iter()
        .map(|shape| (shape, run(&data_dir, channel.root(), shape.request)))
        .collect();

    // Established before anything is concluded: the commands really did meet
    // the silent channel. Read after they have all returned, so this is a
    // count of what happened rather than a wait on anything.
    let connections = channel.connections();
    channel.release();
    assert!(
        connections > 0,
        "this run never established the condition it exists to test: no command reached the \
         channel bound at this store's own address, so each one met a plain absent owner"
    );

    for (shape, output) in &observed {
        let (out, err) = text_of(output);
        assert!(
            !out.starts_with(OWNER_SERVED_PREFIX),
            "{}: nothing is serving this store, so an answer must never be marked as having \
             come from its owner. standard output was:\n{out}",
            shape.label
        );
        assert!(
            !err.contains(HOLDER_CLAIM),
            "{}: no process is holding this store — the channel is a socket with nobody behind \
             it — and the operator is told to go and look at the process that holds it. standard \
             error was:\n{err}",
            shape.label
        );
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}: the store is free to read and carries the answer, so a channel that said \
             nothing must not cost the operator their answer. standard output was:\n{out}\n\
             standard error was:\n{err}",
            shape.label
        );
        assert!(
            out.contains(shape.served_field),
            "{}: the answer must be the real one read from the store — the field \
             `{}` is what a served answer carries. standard output was:\n{out}\nstandard error \
             was:\n{err}",
            shape.label,
            shape.served_field
        );
    }

    drop(tmp);
}
