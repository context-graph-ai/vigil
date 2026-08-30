//! A store somebody else is reading is BUSY, not broken — and what an
//! operator is told has to say so.
//!
//! contextdb's read intent rules this condition typed and TEMPORARY: the
//! refusal names the verified readers holding the file and says the condition
//! clears when they finish, direct readers coexist, and a reader holds the
//! file lock only while it hydrates. context-graph carries it through with its
//! own type — the observed reader count, the verified readers, and the store's
//! path. Vigil is where it stops: the read commands keep only the writer-held
//! shape, so a store that is momentarily busy with a reader arrives at the
//! operator as the STORELESS journey — `vigil why` and `vigil events` refuse
//! with the degraded review-history-unavailable answer, and a read-only `vigil
//! settings` renders the unmanaged/unreadable listing.
//!
//! What that costs the operator is the whole point: their store is healthy,
//! nothing is wrong with it, and they are told it cannot be read and pointed at
//! repairing their working storage. The honest answer is one sentence — someone
//! is reading it, here is who, ask again in a moment — and it is already sitting
//! in the typed error two layers down.
//!
//! The families have since parted company, and the reason is which door each of
//! them opens. Every command that only ASKS — `vigil settings` and its
//! read-only shapes, and now `vigil why` and `vigil events` on context-graph's
//! typed graph view — goes through the read-only consumer reader, and direct
//! readers COEXIST: a reader hydrating this store does not stop another read
//! from being served, so those shapes never meet the busy condition at all and
//! simply answer. That is better than the busy answer, and it is what they pin
//! here now — an operator asking what their node is set to, or what it saw,
//! while something reads the store gets the answer rather than an explanation.
//!
//! `vigil enroll` and `vigil forget` are what is left, and they are what this
//! file's original defect now belongs to. They CHANGE this deployment's
//! recognition, so they take the writable existing-only door, and a reader
//! hydrating the store genuinely is in their way. They are the shapes that
//! still owe the busy answer: somebody is reading it, here is who, ask again in
//! a moment — never that the store is unreadable or this node unmanaged.
//!
//! Every pin here is on the operator-facing delivery: the status the command
//! exits with, the stream the answer arrives on, what a busy answer must carry
//! (the reader count, the reader's own process id, the exact store path), and
//! what it must NOT claim (that the store is unreadable, that this node is
//! unmanaged, or any invented settings/domain/identity line). The WORDING is
//! not pinned — that is the implementer's to write.
//!
//! How the condition is CONSTRUCTED, since it cannot be raced into place: a
//! read session that has finished opening holds nothing, so the reader has to
//! be stopped in the middle of hydration, which is the state that does hold the
//! file. contextdb's own checkpoint does that and nothing else does:
//! `arm_read_image_hydration_pause_for_test` makes the next read-image
//! hydration announce the breadcrumb it just took and then block until it is
//! told to carry on. The reader here really is in that state, in a separate
//! process, holding the real lock, for exactly as long as this test leaves it
//! there — and it is released deterministically by this test, never by a clock
//! running out.
//!
//! Nothing here asserts on elapsed time, and nothing asserts a deadline: the
//! ruled startup wait (owner ruling of 2026-08-25) is a different contract and
//! is untouched by every pin below.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use contextdb_engine::ReadSession;
use vigil::settings_projection::{
    DOMAIN_LINE_PREFIX, IDENTITY_LINE_PREFIX, SETTING_LINE_PREFIX, UNMANAGED_LINE_PREFIX,
};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, capture_pipe, open_store_at, vigil_binary_path, wait_until,
};

/// The store the parked reader is told to read.
const HOLDER_STORE_VARIABLE: &str = "VIGIL_TEST_BUSY_STORE_HOLDER";

/// The name of the child helper below, as the test harness filters on it.
const HOLDER_TEST_NAME: &str = "a_direct_reader_parks_inside_hydration_of_the_deployments_store";

/// The line contextdb's own scaffold prints from inside hydration, carrying the
/// breadcrumb it has just taken. Read here only as the signal that the
/// condition is established.
const PARKED_MARKER: &str = "READ_IMAGE_HYDRATION_HELD=";

/// The word the scaffold waits to read before it finishes hydrating.
const RELEASE_COMMAND: &str = "release";

/// What the scaffold prints instead of a breadcrumb when the reader it parked
/// took none. Nothing for the store to name is a different condition from the
/// one under test, so a run that lands there says so rather than reporting a
/// defect that is not there.
const UNVERIFIED_HOLDER: &str = "unverified";

/// The claim the busy answer must never make. A store held by a reader is
/// healthy; telling an operator it is unreadable sends them to repair working
/// storage.
const UNREADABLE_CLAIM: &str = "is unreadable";

/// The note vigil staples onto an answer it did not recognize as a refusal. It
/// reports on the OWNER route — a road the operator never asked about — and on
/// a busy answer it directly contradicts the sentence in front of it.
const OWNER_ROUTE_TRAILER: &str = "runtime owner unavailable";

/// The claim inside that note, spelled out separately because it is the part
/// that is FALSE here: a verified reader is holding this store, and saying
/// nobody is holding it is the contradiction an operator has to resolve on
/// their own.
const NO_HOLDER_CLAIM: &str = "no process is holding the store";

/// The status a read command exits with when it refuses. Today's contract for
/// a refusal that did not serve the request (`lib.rs`'s refusal path, and the
/// ruled `identity` shape), kept exactly as it is: a momentarily busy store
/// changes what the operator is TOLD, never whether their request was served.
const REFUSED: i32 = 2;

/// The reader that holds the store MID-HYDRATION, in a process of its own.
///
/// A read session that has finished opening does not hold the store — the
/// state that does is the middle of hydration, and the only way to stop a
/// reader there is contextdb's own checkpoint. Nothing here simulates that
/// state; the reader really is inside it, holding the real lock.
///
/// It runs as a SEPARATE process, this test binary re-executed, because the
/// answer under test must name a process this test can check independently and
/// could not have produced by reporting itself.
#[test]
#[ignore = "re-executed as the hydration-holding child process; not an ordinary test"]
// The parent hands this child its store path through the environment because it
// re-executes the test binary (allow: test IPC channel).
#[allow(clippy::disallowed_methods)]
fn a_direct_reader_parks_inside_hydration_of_the_deployments_store() {
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
/// library's own `Child`, so nothing here formats a pid or a signal target, and
/// it runs on the panicking path too so a failed assertion cannot strand a
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

    /// The process id the answer must name. Taken from the standard library's
    /// own handle, never parsed out of the child's output, so the two are
    /// genuinely independent.
    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Block until the reader is genuinely INSIDE hydration, proven by the
    /// marker the scaffold prints only from that point, and return that
    /// marker's payload. An exit before the marker is a failure and not a wait:
    /// there would be no reader left for the commands to be refused by.
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
                             the commands below would have met a perfectly free store"
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
    /// it published one. Read off the scaffold's marker and checked against the
    /// standard library's handle by the caller, so an answer that named a
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

/// A deployment with a real store and nothing running: every command below
/// reads the store directly, which is exactly the journey an operator takes
/// when they ask a question while their node happens to be reading it.
struct Deployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    store_path: PathBuf,
}

impl Deployment {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment directory");
        let data_dir = tmp.path().join("data");
        let store_path = data_dir.join("store.contextgraph");
        drop(open_store_at(&store_path).expect("create the deployment's store"));
        // A deployment has a configuration file; the read commands below take
        // the data directory and find the store themselves, exactly as an
        // operator's own invocation does.
        fs::write(tmp.path().join("vigil.toml"), "cameras = []\n")
            .expect("write the configuration file");
        Self {
            _tmp: tmp,
            data_dir,
            store_path,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(vigil_binary_path())
            .args(args)
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("spawn `vigil {}`: {error}", args.join(" ")))
    }

    /// The store exactly as it stands, so a command that wrote to it while
    /// refusing can be caught.
    fn store_fingerprint(&self) -> (std::time::SystemTime, Vec<u8>) {
        let modified = fs::metadata(&self.store_path)
            .and_then(|meta| meta.modified())
            .expect("the store's modification time");
        let bytes = fs::read(&self.store_path).expect("the store's bytes");
        (modified, bytes)
    }
}

/// One command shape as an operator meets it.
struct Shape {
    label: &'static str,
    request: &'static [&'static str],
    /// Whether this shape's answer belongs on standard error.
    on_standard_error: bool,
    /// The status this shape exits with when its request could not be served.
    /// Only the shapes whose delivery is already ruled are pinned here; the
    /// listing shapes carry `None` because what status a MOMENTARILY BUSY
    /// listing returns is a product decision nobody has taken, and this test
    /// refuses to invent it.
    refused_status: Option<i32>,
}

/// Review history and the decision trail. Both read through the door that
/// coexists with other readers, so a reader holding the store costs them
/// nothing: `vigil events` answers with the events, and `vigil why --latest`
/// gives whatever answer this deployment's own history earns it — on a store
/// with no events that is its own honest refusal, which is not what this file
/// is about and is not pinned here.
const REVIEW_SHAPES: [Shape; 2] = [
    Shape {
        label: "vigil why",
        request: &["why", "--latest"],
        on_standard_error: true,
        refused_status: None,
    },
    Shape {
        label: "vigil events",
        request: &["events"],
        on_standard_error: false,
        refused_status: None,
    },
];

/// A well-formed detection id no event carries, and a name nothing is enrolled
/// under: the two edits an operator can aim at a stopped deployment.
const UNKNOWN_ID: &str = "11111111-2222-3333-4444-555555555555";
const UNKNOWN_NAME: &str = "nobody-by-that-name";

/// The shapes that still MEET the busy condition, and the only ones left that
/// can: an enrollment and a forget change this deployment's recognition, so
/// they take the writable existing-only door and a reader hydrating the store
/// really is in their way. Both are refusals when the store cannot be taken —
/// stderr, status 2 — and a busy store changes what the refusal SAYS, never
/// whether the request was served.
const EDIT_SHAPES: [Shape; 2] = [
    Shape {
        label: "vigil enroll",
        request: &["enroll", UNKNOWN_ID, "somebody"],
        on_standard_error: true,
        refused_status: Some(REFUSED),
    },
    Shape {
        label: "vigil forget",
        request: &["forget", UNKNOWN_NAME],
        on_standard_error: true,
        refused_status: Some(REFUSED),
    },
];

/// Every read-only settings shape. All four are SERVED while a reader holds the
/// store — the reading door coexists with other readers — so none of them
/// carries a refused status any more: `identity`'s ruled failed-request
/// delivery applies when no answer exists to give, and here one does.
const SETTINGS_SHAPES: [Shape; 4] = [
    Shape {
        label: "the bare settings listing",
        request: &["settings"],
        on_standard_error: false,
        refused_status: None,
    },
    Shape {
        label: "an explicit settings list",
        request: &["settings", "list"],
        on_standard_error: false,
        refused_status: None,
    },
    Shape {
        label: "a searched settings read",
        request: &["settings", "find", "detection"],
        on_standard_error: false,
        refused_status: None,
    },
    Shape {
        label: "a settings identity read",
        request: &["settings", "identity"],
        on_standard_error: false,
        refused_status: None,
    },
];

fn text_of(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// The answer this shape delivers, and the stream that must stay quiet. The
/// stream is read BEFORE the text is: a script, a supervisor and an add-on log
/// all read the delivery first, and an answer split across both streams is
/// half-lost to whichever one the operator redirected away.
fn carried_and_quiet<'a>(shape: &Shape, out: &'a str, err: &'a str) -> (&'a str, &'a str) {
    if shape.on_standard_error {
        (err, out)
    } else {
        (out, err)
    }
}

/// What a busy answer must carry, whatever words it is written in: how many
/// readers hold the store, which process they are, and the store itself.
fn assert_names_the_readers(label: &str, answer: &str, reader_pid: u32, store_path: &Path) {
    assert!(
        answer.contains(&reader_pid.to_string()),
        "{label}: the operator is told their store is busy and given nobody to look at. The \
         verified reader's process id ({reader_pid}) is in the typed condition two layers down \
         and has to reach the answer, or nothing here is actionable; the answer was:\n{answer}"
    );
    assert!(
        answer.contains(&store_path.display().to_string()),
        "{label}: the answer must name the exact store it is talking about, or an operator with \
         more than one deployment cannot tell which; the answer was:\n{answer}"
    );
    let counted = answer
        .lines()
        .filter(|line| line.to_ascii_lowercase().contains("reader"))
        .any(|line| line.contains('1'));
    assert!(
        counted,
        "{label}: the answer must say HOW MANY readers hold the store — one reader clearing in a \
         moment and a crowd of them are different situations for the operator, and the observed \
         count is carried in the typed condition already; the answer was:\n{answer}"
    );
}

/// What a busy answer must never claim. Each of these is the storeless journey
/// arriving on a healthy store: the store is fine, nothing needs repairing, and
/// this node is not unmanaged.
fn assert_claims_nothing_broken(label: &str, out: &str, err: &str) {
    let whole = format!("{out}{err}");
    assert!(
        !whole.contains(UNREADABLE_CLAIM),
        "{label}: the store is healthy and briefly busy, and the operator was told it is \
         unreadable — which sends them to repair working storage; the answer was:\n{whole}"
    );
    for prefix in [
        UNMANAGED_LINE_PREFIX,
        SETTING_LINE_PREFIX,
        DOMAIN_LINE_PREFIX,
        IDENTITY_LINE_PREFIX,
    ] {
        assert!(
            !whole
                .lines()
                .any(|line| line.starts_with(&format!("{prefix} "))),
            "{label}: a command that could not read the store must not render a `{prefix}` line \
             — the unmanaged condition, the settings, the domain roster and this node's identity \
             are all things it would be inventing, and an operator reading them cannot tell; the \
             answer was:\n{whole}"
        );
    }
}

/// Every entry under the deployment directory, with its size and modification
/// time — the whole tree, not just the store file, because a diagnosis that
/// re-opens the store it is diagnosing does its filesystem work on the way in
/// (a deployment directory created, a runtime artifact laid down) before it
/// ever reaches the refusal it is trying to classify.
fn deployment_manifest(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            if meta.is_dir() {
                out.push(format!("{relative}/"));
                walk(&path, root, out);
            } else {
                // The modification time is compared, never measured: it is
                // rendered as the opaque value the operating system reports and
                // matched against itself. Nothing here reads a clock or asks
                // how long anything took.
                let modified = meta.modified().ok();
                out.push(format!("{relative} {} {modified:?}", meta.len()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// A command that could not read the store must not TAKE it on the way to
/// saying so.
///
/// The answer to a busy store is a classification of the refusal the command
/// already holds. A second open — and the one on this path is a WRITABLE node
/// handle, the same door the runtime itself comes through — is a diagnosis
/// that contends with the thing it is diagnosing: it creates the deployment
/// directory if it is missing, it publishes itself against a store somebody
/// else is reading, and if the holder let go in between it answers about a
/// different world than the one that refused the command. Nothing a refused
/// read command does may touch this deployment's storage at all.
///
/// Unfakeable because the whole directory is fingerprinted, not the store file
/// alone: an open that failed still did its filesystem work on the way in, and
/// that work is what shows up here.
#[test]
fn a_refused_command_leaves_the_deployment_directory_exactly_as_it_found_it() {
    let deployment = Deployment::new();
    let mut reader = ParkedReader::park(&deployment.store_path);
    reader.wait_until_parked();

    let before = deployment_manifest(&deployment.data_dir);
    assert!(
        !before.is_empty(),
        "sanity: the deployment must have a directory to leave alone"
    );
    let answers: Vec<String> = REVIEW_SHAPES
        .iter()
        .chain(SETTINGS_SHAPES.iter())
        .chain(EDIT_SHAPES.iter())
        .map(|shape| {
            let output = deployment.run(shape.request);
            let (out, err) = text_of(&output);
            format!("{}: {out}{err}", shape.label)
        })
        .collect();
    let after = deployment_manifest(&deployment.data_dir);
    reader.release();

    assert_eq!(
        after,
        before,
        "a command that could not read the store touched this deployment's storage on its way to \
         saying so — the entries that changed are the difference between these two listings, and \
         the answers given were:\n{}",
        answers.join("\n")
    );
}

/// The pin for the shapes that still meet the condition: an edit against a
/// store somebody is reading is told who is reading it, and told nothing is
/// broken.
///
/// `vigil enroll` and `vigil forget` change this deployment's recognition, so
/// they take the writable existing-only door and a reader hydrating the store
/// genuinely holds them off. What they owe the operator is one sentence —
/// somebody is reading it, here is who, ask again in a moment — and never the
/// storeless journey: this store is healthy, and telling an operator it cannot
/// be read sends them to repair working storage.
///
/// The answer is a classification of the refusal the command's OWN open
/// returned, and there is no second look anywhere on this path to disagree with
/// it. That property is held structurally rather than raced: each direct-path
/// command now makes exactly one open and classifies what it got, which
/// `crates/vigil/tests/a_failed_open_is_classified_from_the_error_it_returned.rs`
/// reads off the source, because the window between two opens is one no test
/// can time reliably.
///
/// Unfakeable because the reader genuinely holds the store for the whole of
/// both commands, in a process of its own, and is released only by this test.
#[test]
fn an_edit_answers_that_the_store_is_busy_rather_than_unreadable() {
    let deployment = Deployment::new();
    let mut reader = ParkedReader::park(&deployment.store_path);
    let marker = reader.wait_until_parked();
    let reader_pid = reader.pid();
    ParkedReader::published_pid(&marker).unwrap_or_else(|| {
        panic!(
            "this run never established the condition it exists to test: the parked reader \
             published no breadcrumb ({marker:?})"
        )
    });

    let before = deployment.store_fingerprint();
    let observed: Vec<(&Shape, Output)> = EDIT_SHAPES
        .iter()
        .map(|shape| (shape, deployment.run(shape.request)))
        .collect();
    let after = deployment.store_fingerprint();
    reader.release();

    for (shape, output) in &observed {
        let (out, err) = text_of(output);
        assert_eq!(
            output.status.code(),
            shape.refused_status,
            "{}: a change that was not made is a refusal however the store was held, and a busy \
             store changes what is said rather than whether it was served. standard output \
             was:\n{out}\nstandard error was:\n{err}",
            shape.label
        );
        let (carried, quiet) = carried_and_quiet(shape, &out, &err);
        assert!(
            !carried.trim().is_empty() && quiet.trim().is_empty(),
            "{}: the answer must arrive on one stream alone — an operator who redirected the \
             other away must not be reading half of it. carried:\n{carried}\nquiet:\n{quiet}",
            shape.label
        );
        assert_names_the_readers(shape.label, carried, reader_pid, &deployment.store_path);
        assert_claims_nothing_broken(shape.label, &out, &err);
    }

    assert_eq!(
        after, before,
        "and an edit that was refused wrote nothing: the store is byte-for-byte as the reader \
         left it"
    );
}

/// The pin for the review surfaces: a momentary reader costs an operator their
/// review history not at all, because both commands are SERVED straight through
/// it.
///
/// This is the stronger end of the promise this file was written for. `vigil
/// why` and `vigil events` read through context-graph's typed graph view over
/// the read-only consumer reader, which coexists with the reader holding the
/// store, so there is no busy answer left to give them and no explanation to
/// lose. What is pinned is that they answer on their own stream, name nobody as
/// holding anything, never call the store unreadable or this node unmanaged,
/// and leave the store byte-identical.
///
/// Unfakeable because the reader genuinely holds the store for the whole of
/// both commands, in a process of its own, and is released only by this test.
#[test]
fn why_and_events_are_served_while_a_reader_holds_the_store() {
    let deployment = Deployment::new();
    let mut reader = ParkedReader::park(&deployment.store_path);
    let marker = reader.wait_until_parked();
    let reader_pid = reader.pid();
    let published = ParkedReader::published_pid(&marker).unwrap_or_else(|| {
        panic!(
            "this run never established the condition it exists to test: the parked reader \
             published no breadcrumb ({marker:?}), so the store has no verified holder to name"
        )
    });
    assert_eq!(
        published,
        u64::from(reader_pid),
        "sanity: the breadcrumb the reader parked with must be the reader's own, or the answers \
         below are being checked against the wrong process"
    );

    let before = deployment.store_fingerprint();
    let observed: Vec<(&Shape, Output)> = REVIEW_SHAPES
        .iter()
        .map(|shape| (shape, deployment.run(shape.request)))
        .collect();
    let after = deployment.store_fingerprint();
    reader.release();

    for (shape, output) in &observed {
        let (out, err) = text_of(output);
        let whole = format!("{out}{err}");
        assert!(
            !whole.contains(&reader_pid.to_string()),
            "{}: nothing was in this read's way — the reading door coexists with the reader — so \
             nobody may be named as holding the store. An operator sent to look at a process that \
             never delayed them is sent nowhere. The answer was:\n{whole}",
            shape.label
        );
        assert_claims_nothing_broken(shape.label, &out, &err);
    }

    assert_eq!(
        after.0, before.0,
        "a review read writes nothing, so the store's modification time cannot have moved"
    );
    assert_eq!(
        after.1, before.1,
        "and the store's bytes must be exactly as the reader left them"
    );
}

/// A busy answer must not contradict itself in its own last sentence.
///
/// The busy statement says the store is healthy and somebody is reading it.
/// Vigil then appends a note about the OWNER route the operator never asked
/// about — "runtime owner unavailable: no process is holding the store at
/// <path>" — and the two sentences flatly disagree: one says a named process
/// is holding the store right now, the other says no process is holding it.
/// An operator reading that cannot tell which half to believe, and the half
/// that is actionable (who is reading it, ask again in a moment) is the half
/// the contradiction discredits.
///
/// It is appended because the busy answer is not recognized as a refusal:
/// `settings_degraded::is_refusal` matches the capability and settings-error
/// markers and nothing else, so the busy marker falls through to the arm that
/// exists for a genuine store fault and gets the transport note bolted on. The
/// refusal arm directly above it already states the rule this answer should
/// have met: a refusal names its own cause and its own remedy, and a note about
/// a transport the operator never asked about only tells them about a road, not
/// about their store.
///
/// The sibling pins in this file hold what the answer must CARRY and what it
/// must not CLAIM about the store; this one holds what must not be stapled to
/// the end of it. Nothing here pins the wording of either sentence — only that
/// the owner-route note is not one of them.
#[test]
fn a_busy_answer_carries_no_owner_route_trailer_contradicting_it() {
    let deployment = Deployment::new();
    let mut reader = ParkedReader::park(&deployment.store_path);
    let marker = reader.wait_until_parked();
    let reader_pid = reader.pid();
    let published = ParkedReader::published_pid(&marker).unwrap_or_else(|| {
        panic!(
            "this run never established the condition it exists to test: the parked reader \
             published no breadcrumb ({marker:?}), so the store has no verified holder to name"
        )
    });
    assert_eq!(
        published,
        u64::from(reader_pid),
        "sanity: the breadcrumb the reader parked with must be the reader's own, or the answers \
         below are being checked against the wrong process"
    );

    let observed: Vec<(&Shape, Output)> = EDIT_SHAPES
        .iter()
        .map(|shape| (shape, deployment.run(shape.request)))
        .collect();
    reader.release();

    for (shape, output) in &observed {
        let (out, err) = text_of(output);
        let whole = format!("{out}{err}");
        // Established first: this really is the busy answer, so an absent
        // trailer below cannot be an absent answer.
        assert_names_the_readers(shape.label, &whole, reader_pid, &deployment.store_path);
        assert!(
            !whole.contains(OWNER_ROUTE_TRAILER),
            "{}: the operator is told a named process is reading their store and, in the same \
             breath, that no process is holding it. A refusal names its own cause and its own \
             remedy; the owner route is a road they never asked about and its note here only \
             contradicts the answer. The answer was:\n{whole}",
            shape.label
        );
        assert!(
            !whole.contains(NO_HOLDER_CLAIM),
            "{}: the answer must never claim nobody is holding this store — a verified reader \
             is holding it, which is the whole condition being reported. The answer was:\n{whole}",
            shape.label
        );
    }
}

/// The pin for the settings surfaces: a reader holding the store costs an
/// operator nothing at all, because every read-only settings shape is SERVED
/// through the reading door while that reader is still in there.
///
/// This is the stronger end of the same promise the review pins hold. Their
/// answer is "somebody is reading it, here is who, ask again in a moment";
/// these shapes never have to say it, because context-graph's read-only
/// consumer reader coexists with the reader and hands them the answer. So what
/// is pinned is that they answer — the real listing, this node's real identity,
/// the narrowed search — on their own stream, with no busy line, no unmanaged
/// line, nothing about the store being unreadable, and nobody named as holding
/// anything. And the store is byte-identical afterwards: a question is not a
/// change, however many are asked while it is being read.
#[test]
fn every_read_only_settings_shape_is_served_while_a_reader_holds_the_store() {
    let deployment = Deployment::new();
    let mut reader = ParkedReader::park(&deployment.store_path);
    let marker = reader.wait_until_parked();
    let reader_pid = reader.pid();
    ParkedReader::published_pid(&marker).unwrap_or_else(|| {
        panic!(
            "this run never established the condition it exists to test: the parked reader \
             published no breadcrumb ({marker:?}), so the store has no verified holder to name"
        )
    });

    let before = deployment.store_fingerprint();
    let observed: Vec<(&Shape, Output)> = SETTINGS_SHAPES
        .iter()
        .map(|shape| (shape, deployment.run(shape.request)))
        .collect();
    let after = deployment.store_fingerprint();
    reader.release();

    for (shape, output) in &observed {
        let (out, err) = text_of(output);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}: a reader hydrating this store does not stop a read from being served — the \
             reading door coexists with it — so this shape answers. standard output \
             was:\n{out}\nstandard error was:\n{err}",
            shape.label
        );
        let (carried, quiet) = carried_and_quiet(shape, &out, &err);
        assert!(
            !carried.trim().is_empty() && quiet.trim().is_empty(),
            "{}: the answer must arrive on one stream alone. carried:\n{carried}\nquiet:\n{quiet}",
            shape.label
        );
        assert!(
            !carried.contains(&reader_pid.to_string()),
            "{}: nothing was in this read's way, so nobody may be named as holding the store — an \
             operator sent to look at a process that never delayed them is sent nowhere. The \
             answer was:\n{carried}",
            shape.label
        );
        let whole = format!("{out}{err}");
        assert!(
            !whole.contains(UNREADABLE_CLAIM)
                && !whole
                    .lines()
                    .any(|line| line.starts_with(&format!("{UNMANAGED_LINE_PREFIX} "))),
            "{}: the store is healthy, this read was served, and neither the unreadable claim nor \
             the unmanaged statement may appear on the answer:\n{whole}",
            shape.label
        );
    }

    assert_eq!(
        after.0, before.0,
        "and a settings read writes nothing, so the store's modification time cannot have moved"
    );
    assert_eq!(
        after.1, before.1,
        "and the store's bytes must be exactly as the reader left them"
    );
}

/// The positive control, and the proof the answers above are about the READER
/// and not about this deployment: every one of those same commands, run before
/// the reader ever existed and again after it let go, answers the way it always
/// does — same status, same stream, and never the busy answer.
///
/// It carries the split between the two families in one place. The EDIT shapes
/// must MEET the busy condition while the reader is parked, or the pins above
/// prove nothing. Every asking shape must not: `vigil settings` and its
/// read-only shapes, `vigil why` and `vigil events` are all served through the
/// reading door throughout, so all three moments answer alike and none of them
/// names the reader.
#[test]
fn the_same_commands_answer_normally_once_the_reader_has_finished() {
    let deployment = Deployment::new();
    let shapes: Vec<&Shape> = REVIEW_SHAPES
        .iter()
        .chain(SETTINGS_SHAPES.iter())
        .chain(EDIT_SHAPES.iter())
        .collect();

    let baseline: Vec<Output> = shapes
        .iter()
        .map(|shape| deployment.run(shape.request))
        .collect();

    let mut reader = ParkedReader::park(&deployment.store_path);
    reader.wait_until_parked();
    let reader_pid = reader.pid();
    let busy: Vec<Output> = shapes
        .iter()
        .map(|shape| deployment.run(shape.request))
        .collect();
    reader.release();

    let recovered: Vec<Output> = shapes
        .iter()
        .map(|shape| deployment.run(shape.request))
        .collect();

    for ((shape, baseline), (busy, recovered)) in shapes
        .iter()
        .zip(baseline.iter())
        .zip(busy.iter().zip(recovered.iter()))
    {
        let (baseline_out, baseline_err) = text_of(baseline);
        let (busy_out, busy_err) = text_of(busy);
        let (recovered_out, recovered_err) = text_of(recovered);

        assert!(
            !format!("{baseline_out}{baseline_err}").contains(&reader_pid.to_string()),
            "{}: sanity — the answer given with no reader anywhere near the store must not name \
             the reader, or the busy pins above prove nothing:\n{baseline_out}{baseline_err}",
            shape.label
        );
        // Only the EDIT shapes meet the busy condition: they take the writable
        // existing-only door, so the reader is genuinely in their way and has
        // to be named. Every asking shape reads through the door that coexists
        // with it and is served throughout — for those the proof is the
        // opposite one, that the reader never showed up in the answer at all,
        // which the pin below makes for all three moments together.
        if EDIT_SHAPES.iter().any(|edit| edit.label == shape.label) {
            assert!(
                format!("{busy_out}{busy_err}").contains(&reader_pid.to_string()),
                "{}: sanity — this run has to have MET the busy condition for the recovery below \
                 to mean anything:\n{busy_out}{busy_err}",
                shape.label
            );
        } else {
            assert!(
                !format!("{busy_out}{busy_err}").contains(&reader_pid.to_string()),
                "{}: a reader holding the store must cost a question nothing — it is served \
                 through the door that coexists with readers, so nobody may be named as being in \
                 its way:\n{busy_out}{busy_err}",
                shape.label
            );
            assert_eq!(
                busy.status.code(),
                baseline.status.code(),
                "{}: and it must answer exactly as it does with no reader anywhere near the \
                 store. before:\n{baseline_out}{baseline_err}\nwith a reader:\n{busy_out}{busy_err}",
                shape.label
            );
        }
        assert_eq!(
            recovered.status.code(),
            baseline.status.code(),
            "{}: the reader finished, so this command must answer exactly as it did before \
             anyone was reading. before:\n{baseline_out}{baseline_err}\nafter:\n\
             {recovered_out}{recovered_err}",
            shape.label
        );
        assert!(
            !format!("{recovered_out}{recovered_err}").contains(&reader_pid.to_string()),
            "{}: the reader is gone, so nothing may still be reported as holding the store:\n\
             {recovered_out}{recovered_err}",
            shape.label
        );
    }
}
