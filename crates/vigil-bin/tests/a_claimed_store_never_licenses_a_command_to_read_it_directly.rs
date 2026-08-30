//! A store somebody is already holding is never a store this command may open
//! for itself — not even while its holder is still deciding.
//!
//! Vigil's read commands ask the process that owns the store first and read
//! the store themselves when nobody owns it. Which of those two happens is
//! decided by one gate (`crates/vigil/src/lib.rs`'s
//! `may_read_the_store_directly`): `OwnerNotRunning` and `NoStore` say the file
//! is free, `OwnerTimedOut` says the road failed and says nothing about the
//! file, and every other answer means a live owner is there.
//!
//! There is a window where a writer holds the store and has not yet said
//! whether it will serve owner requests: between claiming the store and
//! publishing its serving decision. Every writer is in it for a moment, and a
//! supervisor that starts Vigil and immediately asks about it lands there
//! routinely. What an operator must never be told in that window is that
//! nobody is holding their store — because somebody is, and a command that
//! believed it would open a store a writer is bringing up, behind that
//! writer's back.
//!
//! The substrate now answers that window truthfully: a caller that meets a
//! live claim waits for the holder's real decision inside its own declared
//! budget, and if the budget runs out it is told, in typed vocabulary, that
//! the process holding the store is not serving — never that no process is
//! holding it. This file pins what VIGIL does with that answer, which is the
//! half no substrate test can see:
//!
//! * the operator is told who holds their store — the holder sentence
//!   context-graph's `OwnerControlError::OwnerNotServing` renders, "the process
//!   holding the store at <path> …";
//! * they are never told "no process is holding the store", which would send
//!   them hunting a process that is right there;
//! * the command opens NO backend of its own and leaves NO reader breadcrumb
//!   behind, because a claimed store is not free to read;
//! * and the answer is not marked as served, because it was not.
//!
//! ## The pin next door that must keep saying the opposite
//!
//! `a_silent_owner_channel_does_not_cost_the_operator_their_answer.rs` holds a
//! journey that looks superficially similar and is its exact inverse: a live
//! but SILENT channel with no store claim behind it. There the store really is
//! free, the direct fallback must stay INTACT, and the holder sentence must be
//! ABSENT. Both files must pass together — that pair is the whole contract.
//! What separates them is not how the request failed but whether anybody holds
//! the store, and these two journeys are the two sides of exactly that.
//!
//! ## How the condition is CONSTRUCTED
//!
//! A real writer, parked between claiming the store and publishing its serving
//! decision, through the substrate's own test seam: the writer is opened on a
//! background thread with a route observer that blocks at
//! `ReadSessionEvent::PersistenceOpened`, which IS the claim — the companion is
//! held and the run a reader dials has been published, while the serving
//! decision does not exist yet. The commands under test are separate operating-
//! system processes, so nothing about this is in-process cooperation: they meet
//! the claim the way an operator's terminal meets it.
//!
//! Nothing here sleeps as synchronization and nothing asserts on elapsed time.
//! The writer signals its own claim through a condition variable, and the
//! command's wait is ended by the command's own declared budget — the product's
//! number, not a chosen interval, and no assertion is made about it.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use contextdb_core::read_contract::OwnerReadCancellation;
use contextdb_engine::read_session::{ReadSessionEvent, ReadSessionTestObserver};
use contextdb_engine::{Database, DatabaseOpenOptions, OwnerReadConfig, OwnerRequestHandler};
use vigil::OWNER_SERVED_PREFIX;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{fresh_store_copy, vigil_binary_path};

/// The variable that states where this deployment's owner plane lives. Both
/// sides read it, so it is what lets this test put the writer's channel
/// somewhere it owns.
const RUNTIME_DIR_VARIABLE: &str = "CONTEXTDB_OWNER_READ_RUNTIME_DIR";

/// The platform runtime location. Reader breadcrumbs always go here — never
/// into a stated owner-plane root — so pointing it at a directory this test
/// owns is what makes "this command opened no backend of its own" observable
/// from outside the process.
const PLATFORM_RUNTIME_VARIABLE: &str = "XDG_RUNTIME_DIR";

/// The one child every contextdb runtime file lives in. Its appearance under
/// the platform runtime location is the footprint a direct backend open
/// leaves.
const RUNTIME_CHILD: &str = "contextdb";

/// Enough seeded observations that `events` has rows to render and `why
/// --latest` has a chain to walk, so an empty answer cannot pass as a served
/// one.
const SEEDED_DETECTIONS: usize = 2;

/// The sentence an operator must be given when somebody holds their store.
const HOLDER_SENTENCE: &str = "the process holding the store at";

/// The sentence they must never be given while somebody holds it.
const ABSENT_CLAIM: &str = "no process is holding the store";

/// What the typed not-serving answer says, and the reason this journey is not
/// just "some refusal came back". Three of context-graph's refusals render the
/// holder sentence, and they do not mean the same thing to the gate that
/// decides whether this command may read the store: the TIMED-OUT one says the
/// road failed and says nothing about the file, and it DOES license the direct
/// fallback (`a_silent_owner_channel_does_not_cost_the_operator_their_answer.rs`
/// holds that journey). A claimed store must land on the not-serving one
/// instead, which is the answer that says somebody is there.
const NOT_SERVING_TAIL: &str = "does not answer owner requests";

/// The timed-out rendering, named so this journey can say it is NOT that one.
const TIMED_OUT_TAIL: &str = "did not answer inside the time this request allows";

/// Failsafe only. Nothing here asserts how long anything took.
const WATCHDOG: Duration = Duration::from_secs(60);

/// One read command as an operator meets it, with the field a served answer
/// carries — so a refusal cannot be mistaken for an answer, or the reverse.
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

/// A directory this test owns, at a short pathname: a local channel address is
/// capped by the kernel and the fixed channel basename is most of it. Owner-
/// only, which is what a runtime location's own validation requires.
struct OwnedDirectory {
    path: PathBuf,
}

impl OwnedDirectory {
    fn create(label: &str) -> Self {
        let base = PathBuf::from("/tmp");
        let mut path = base.join(format!("vg-{label}-{}", std::process::id()));
        let mut suffix = 0;
        while path.exists() {
            suffix += 1;
            path = base.join(format!("vg-{label}-{}-{suffix}", std::process::id()));
        }
        fs::create_dir(&path).expect("create a runtime location this test owns");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("a runtime location is owner-only");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Whether anything has published a contextdb runtime file here. A direct
    /// backend open creates this child to hold its reader breadcrumb; an
    /// owner-only ask that never opened a backend leaves the location as it
    /// found it.
    fn holds_a_runtime_footprint(&self) -> bool {
        self.path.join(RUNTIME_CHILD).exists()
    }
}

impl Drop for OwnedDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct EchoHandler;

impl OwnerRequestHandler for EchoHandler {
    fn handle(
        &self,
        _namespace: &str,
        request: &[u8],
        _cancellation: &OwnerReadCancellation,
    ) -> contextdb_core::Result<Vec<u8>> {
        Ok(request.to_vec())
    }
}

/// Parks a writer inside its own startup, at a chosen milestone, while it
/// holds the store.
///
/// TWO milestones matter here and they are not the same window.
///
/// `CompanionClaimTaken` is the writer's FIRST exclusive claim on the store's
/// companion. It is the moment the store becomes owned, and nothing a reader
/// can dial has been published yet — so it is the window in which a caller
/// told "nobody owns this" would go off and take a store somebody else is
/// already holding. This is the half where the lie actually lived.
///
/// `PersistenceOpened` is later: the companion is held AND the run a reader
/// dials has been published, while the serving decision still does not exist.
///
/// Both are a real writer's own startup, stopped where every writer really is
/// for a moment — not a simulation of one. Every event is recorded, because a
/// park that fired later than it claims to would find a claim already held and
/// pass while proving nothing about the window it names.
struct WriterParkedInsideItsClaim {
    park_at: ReadSessionEvent,
    seen: Mutex<Vec<ReadSessionEvent>>,
    claimed: (Mutex<bool>, Condvar),
    release: (Mutex<bool>, Condvar),
    parked: AtomicBool,
}

impl WriterParkedInsideItsClaim {
    fn parking_at(park_at: ReadSessionEvent) -> Self {
        Self {
            park_at,
            seen: Mutex::new(Vec::new()),
            claimed: (Mutex::new(false), Condvar::new()),
            release: (Mutex::new(false), Condvar::new()),
            parked: AtomicBool::new(false),
        }
    }

    /// What this writer's startup has reached so far.
    fn observed(&self) -> Vec<ReadSessionEvent> {
        self.seen
            .lock()
            .expect("observed writer milestones")
            .clone()
    }
}

impl ReadSessionTestObserver for WriterParkedInsideItsClaim {
    fn observe_event(&self, event: ReadSessionEvent) {
        self.seen
            .lock()
            .expect("observed writer milestones")
            .push(event);
        if event != self.park_at || self.parked.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let (flag, signal) = &self.claimed;
            *flag.lock().expect("claim-window state") = true;
            signal.notify_all();
        }
        let (flag, signal) = &self.release;
        let mut released = flag.lock().expect("claim-window state");
        while !*released {
            released = signal.wait(released).expect("claim-window condvar");
        }
    }
}

impl WriterParkedInsideItsClaim {
    fn wait_until_claimed(&self) {
        let (flag, signal) = &self.claimed;
        let mut claimed = flag.lock().expect("claim-window state");
        while !*claimed {
            let (next, timed_out) = signal
                .wait_timeout(claimed, WATCHDOG)
                .expect("claim-window condvar");
            claimed = next;
            assert!(
                !timed_out.timed_out() || *claimed,
                "the writer never reached the point where it holds the store, so this run never \
                 established the condition it exists to test"
            );
        }
    }

    fn let_go(&self) {
        let (flag, signal) = &self.release;
        *flag.lock().expect("claim-window state") = true;
        signal.notify_all();
    }
}

/// The parked writer, holding the store for as long as this value lives.
struct ParkedWriter {
    gate: Arc<WriterParkedInsideItsClaim>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ParkedWriter {
    /// A real writer, claiming `store_path` and publishing its channel in
    /// `runtime_dir`, stopped once it has the companion AND has published the
    /// run a reader dials, but before any serving decision exists.
    fn holding(store_path: &Path, runtime_dir: &Path) -> Self {
        Self::holding_at(store_path, runtime_dir, ReadSessionEvent::PersistenceOpened)
    }

    /// The same writer, stopped at its FIRST exclusive claim on the companion
    /// — the moment the store becomes owned, with nothing published about it
    /// yet.
    fn claiming(store_path: &Path, runtime_dir: &Path) -> Self {
        Self::holding_at(
            store_path,
            runtime_dir,
            ReadSessionEvent::CompanionClaimTaken,
        )
    }

    /// What the writer's startup had reached at the moment it parked. Read as
    /// the proof that the park is where it says it is.
    fn observed(&self) -> Vec<ReadSessionEvent> {
        self.gate.observed()
    }

    fn holding_at(store_path: &Path, runtime_dir: &Path, park_at: ReadSessionEvent) -> Self {
        let gate = Arc::new(WriterParkedInsideItsClaim::parking_at(park_at));
        let observer: Arc<dyn ReadSessionTestObserver> = gate.clone();
        let released = gate.clone();
        let path = store_path.to_path_buf();
        let runtime_dir = runtime_dir.to_path_buf();
        let thread = std::thread::spawn(move || {
            let options = DatabaseOpenOptions {
                owner_reads: OwnerReadConfig {
                    runtime_dir: Some(runtime_dir),
                    handler: Some(Arc::new(EchoHandler)),
                    enabled: true,
                    ..OwnerReadConfig::default()
                },
                test_observer: Some(observer),
                ..DatabaseOpenOptions::default()
            };
            // Once released the writer finishes its own startup and closes.
            // Nothing is asserted about what it published: this journey is
            // about the answer a caller got while it was still deciding.
            if let Ok(database) = Database::open_with_options(&path, options) {
                let _ = database.close();
            }
            let _ = &released;
        });
        gate.wait_until_claimed();
        Self {
            gate,
            thread: Some(thread),
        }
    }

    /// Let the writer finish, and hand back everything its startup reached.
    /// Called once the journey has its answers.
    fn release(mut self) -> Vec<ReadSessionEvent> {
        self.gate.let_go();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.gate.observed()
    }
}

impl Drop for ParkedWriter {
    /// Runs on the panicking path too: a failed assertion must never strand a
    /// writer parked inside its claim with nobody left to let it go.
    fn drop(&mut self) {
        self.gate.let_go();
    }
}

fn run(data_dir: &Path, owner_root: &Path, platform_root: &Path, args: &[&str]) -> Output {
    Command::new(vigil_binary_path())
        .args(args)
        .env("VIGIL_DATA_DIR", data_dir)
        .env(RUNTIME_DIR_VARIABLE, owner_root)
        .env(PLATFORM_RUNTIME_VARIABLE, platform_root)
        .env_remove("VIGIL_STORE_PATH")
        .env_remove("VIGIL_CONTROL_SOCKET")
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

#[test]
fn a_command_meeting_a_claimed_store_is_told_who_holds_it_and_reads_nothing_itself() {
    let (tmp, store_path) =
        fresh_store_copy(SEEDED_DETECTIONS).expect("seed a deterministic detection fixture");
    let data_dir = store_path
        .parent()
        .expect("the seeded store sits inside a data directory")
        .to_path_buf();
    let owner_root = OwnedDirectory::create("claim-own");
    let platform_root = OwnedDirectory::create("claim-plat");

    assert!(
        !platform_root.holds_a_runtime_footprint(),
        "sanity: the platform runtime location starts empty, or the footprint read below would \
         be about somebody else's work"
    );

    let writer = ParkedWriter::holding(&store_path, owner_root.path());

    let observed: Vec<(&Shape, Output)> = READ_SHAPES
        .iter()
        .map(|shape| {
            (
                shape,
                run(
                    &data_dir,
                    owner_root.path(),
                    platform_root.path(),
                    shape.request,
                ),
            )
        })
        .collect();

    // Read while the claim was still held, before the writer is let go, so
    // every assertion below is about the window this file exists to test.
    //
    // What this observation can still see has NARROWED, and the comment says so
    // rather than letting a weaker instrument read as the old proof: a finished
    // read session takes its own breadcrumb directories away, so an empty
    // location no longer distinguishes a command that never opened a backend
    // from one that opened and closed it. It still catches an open that is
    // HELD — the shape a command taking the store for itself would leave while
    // it worked — and the assertions above carry the rest: the typed
    // not-serving refusal, and no served field anywhere in the answer.
    let footprint = platform_root.holds_a_runtime_footprint();
    let _ = writer.release();

    for (shape, output) in &observed {
        let (out, err) = text_of(output);
        let spoken = format!("{out}{err}");
        assert!(
            spoken.contains(HOLDER_SENTENCE),
            "{}: a writer is holding this store and has not finished deciding whether it will \
             serve, so the operator must be told about the process that holds it — that sentence \
             is what tells them there is something to wait for rather than something to fix. \
             standard output was:\n{out}\nstandard error was:\n{err}",
            shape.label
        );
        assert!(
            spoken.contains(NOT_SERVING_TAIL),
            "{}: the answer must be the TYPED not-serving one. Every refusal that renders the \
             holder sentence looks alike to an operator and they are not alike to the gate that \
             decides whether this command may open the store: the timed-out refusal beside it \
             says the road failed and licenses the direct read. A claimed store has to produce \
             the answer that says somebody is holding it. standard output was:\n{out}\n\
             standard error was:\n{err}",
            shape.label
        );
        assert!(
            !spoken.contains(TIMED_OUT_TAIL),
            "{}: a writer that is holding this store and still deciding is not a transport that \
             failed — answering as though it were would hand this command the one refusal that \
             licenses it to open the store anyway. standard output was:\n{out}\nstandard error \
             was:\n{err}",
            shape.label
        );
        assert!(
            !spoken.contains(ABSENT_CLAIM),
            "{}: a writer IS holding this store, and telling the operator nobody is sends them \
             hunting a process that is right there — and licenses this command to open a store \
             behind a writer that is still bringing it up. standard output was:\n{out}\n\
             standard error was:\n{err}",
            shape.label
        );
        assert!(
            !out.starts_with(OWNER_SERVED_PREFIX),
            "{}: no owner answered this request, so the answer must never be marked as having \
             come from one. standard output was:\n{out}",
            shape.label
        );
        assert!(
            !out.contains(shape.served_field),
            "{}: a claimed store is not free to read, so this command must not come back with \
             the store's own contents — the field `{}` only appears in an answer somebody read \
             out of the store. standard output was:\n{out}",
            shape.label,
            shape.served_field
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "{}: the operator did not get what they asked for, and the exit status has to say \
             so. standard output was:\n{out}\nstandard error was:\n{err}",
            shape.label
        );
    }

    assert!(
        !footprint,
        "a claimed store is not free to read: these commands must open no backend of their own \
         while a writer holds it, and a reader breadcrumb published under {} is the footprint of \
         exactly the open that must not have happened",
        platform_root.path().display()
    );

    drop(tmp);
}

#[test]
fn a_store_claimed_but_not_yet_announced_is_never_reported_free_to_this_command() {
    // The half of the window where the lie actually lived. The identity above
    // parks once the writer has both the companion AND the run a reader dials
    // published; this one parks at the writer's FIRST exclusive claim, where
    // the store is genuinely owned and NOTHING published says so. A caller
    // told "nobody is holding this" there does not merely get a confusing
    // sentence — it is licensed to go and take a store somebody else already
    // has.
    let (tmp, store_path) =
        fresh_store_copy(SEEDED_DETECTIONS).expect("seed a deterministic detection fixture");
    let data_dir = store_path
        .parent()
        .expect("the seeded store sits inside a data directory")
        .to_path_buf();
    let owner_root = OwnedDirectory::create("early-own");
    let platform_root = OwnedDirectory::create("early-plat");

    assert!(
        !platform_root.holds_a_runtime_footprint(),
        "sanity: the platform runtime location starts empty, or the footprint read below would \
         be about somebody else's work"
    );

    let writer = ParkedWriter::claiming(&store_path, owner_root.path());

    // FIXTURE INTEGRITY, and the whole reason this identity is not a slower
    // copy of the one above. A park that fired LATER than it claims to would
    // find the claim already held and pass while proving nothing about this
    // window, so the writer's own startup is read back: at this park point it
    // must not yet have reached either milestone the later park sits at.
    let at_the_park = writer.observed();
    for later in [
        ReadSessionEvent::OpenRegistryAcquired,
        ReadSessionEvent::PersistenceOpened,
    ] {
        assert!(
            !at_the_park.contains(&later),
            "this writer parked at {later:?} or after it, which is the window the identity above \
             already covers — so this run would pass without ever putting a command in front of a \
             store that is claimed and unannounced. The writer had reached: {at_the_park:?}"
        );
    }

    let observed: Vec<(&Shape, Output)> = READ_SHAPES
        .iter()
        .map(|shape| {
            (
                shape,
                run(
                    &data_dir,
                    owner_root.path(),
                    platform_root.path(),
                    shape.request,
                ),
            )
        })
        .collect();

    let footprint = platform_root.holds_a_runtime_footprint();
    let reached = writer.release();

    // The other half of the integrity read: the writer really did go on to
    // open the store. A park inside an open that FAILED would hold no claim by
    // the end of it, and the answers above would be about a store nobody was
    // holding after all.
    assert!(
        reached.contains(&ReadSessionEvent::PersistenceOpened),
        "the parked writer never went on to open this store, so nothing here can be said to have \
         met a live claim. The writer reached: {reached:?}"
    );

    for (shape, output) in &observed {
        let (out, err) = text_of(output);
        let spoken = format!("{out}{err}");
        assert!(
            !spoken.contains(ABSENT_CLAIM),
            "{}: this store is OWNED — a writer has taken its first exclusive claim — and \
             nothing published says so yet. Telling the operator nobody is holding it is the one \
             answer that licenses this command, and anything else that believes it, to go and \
             take a store somebody else already has. standard output was:\n{out}\nstandard \
             error was:\n{err}",
            shape.label
        );
        assert!(
            spoken.contains(HOLDER_SENTENCE),
            "{}: the operator must be told about the process that holds their store, in the \
             window as much as after it — that sentence is what tells them there is something to \
             wait for rather than something to fix. standard output was:\n{out}\nstandard \
             error was:\n{err}",
            shape.label
        );
        assert!(
            spoken.contains(NOT_SERVING_TAIL),
            "{}: the answer must be the TYPED not-serving one. The timed-out refusal beside it \
             says the road failed and licenses the direct read, which is exactly what a claimed \
             store must not hand out. standard output was:\n{out}\nstandard error \
             was:\n{err}",
            shape.label
        );
        assert!(
            !spoken.contains(TIMED_OUT_TAIL),
            "{}: a writer that has just claimed this store is not a transport that failed. \
             standard output was:\n{out}\nstandard error was:\n{err}",
            shape.label
        );
        assert!(
            !out.starts_with(OWNER_SERVED_PREFIX),
            "{}: no owner answered this request, so the answer must never be marked as having \
             come from one. standard output was:\n{out}",
            shape.label
        );
        assert!(
            !out.contains(shape.served_field),
            "{}: a claimed store is not free to read, so this command must not come back with \
             the store's own contents. standard output was:\n{out}",
            shape.label
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "{}: the operator did not get what they asked for, and the exit status has to say \
             so. standard output was:\n{out}\nstandard error was:\n{err}",
            shape.label
        );
    }

    assert!(
        !footprint,
        "a store that is claimed and unannounced is not free to read: these commands must open \
         no backend of their own, and a reader breadcrumb published under {} is the footprint of \
         exactly the open that must not have happened",
        platform_root.path().display()
    );

    drop(tmp);
}

#[test]
fn a_store_nobody_holds_still_lets_the_same_command_read_it_and_leaves_the_footprint() {
    // The control for the footprint read above, and for the refusal beside it.
    // Without this the claimed journey's two negative claims would both be
    // satisfied by a command that never reads any store under any condition,
    // and by a footprint nothing ever leaves.
    let (tmp, store_path) =
        fresh_store_copy(SEEDED_DETECTIONS).expect("seed a deterministic detection fixture");
    let data_dir = store_path
        .parent()
        .expect("the seeded store sits inside a data directory")
        .to_path_buf();
    let owner_root = OwnedDirectory::create("free-own");
    let platform_root = OwnedDirectory::create("free-plat");

    let observed: Vec<(&Shape, Output)> = READ_SHAPES
        .iter()
        .map(|shape| {
            (
                shape,
                run(
                    &data_dir,
                    owner_root.path(),
                    platform_root.path(),
                    shape.request,
                ),
            )
        })
        .collect();

    for (shape, output) in &observed {
        let (out, err) = text_of(output);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}: nobody is holding this store, so the same command reads it and answers — that \
             is the promise the claimed-store refusal must not be allowed to swallow. standard \
             output was:\n{out}\nstandard error was:\n{err}",
            shape.label
        );
        assert!(
            out.contains(shape.served_field),
            "{}: the answer must be the real one read from the store — `{}` is what a served \
             answer carries. standard output was:\n{out}",
            shape.label,
            shape.served_field
        );
    }

    // What proves these commands opened the store THEMSELVES is the answers
    // above: no runtime is running, so no owner existed to serve them, and an
    // `observation_id=` or a `selection=` read out of this deployment's own
    // store could not have come from anywhere else.
    //
    // It is deliberately NOT the runtime footprint any more. A read session
    // publishes its breadcrumb while it hydrates and takes it away again when
    // it finishes — the per-store directory and the `contextdb` child it
    // created with it — so a read that COMPLETED leaves the platform runtime
    // location exactly as it found it. That is worth pinning in its own right,
    // and it is the opposite assertion to the one that used to stand here: a
    // question leaves no residue behind in a location the operator supplied.
    assert!(
        !platform_root.holds_a_runtime_footprint(),
        "these reads finished, and a finished read takes its own breadcrumb away — the per-store \
         directory and the `contextdb` child it created — so nothing of theirs may still be \
         standing under {}. A residue left in a runtime location an operator supplied is one \
         nobody is coming back to clean up",
        platform_root.path().display()
    );

    drop(tmp);
}
