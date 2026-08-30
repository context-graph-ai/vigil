//! A runtime waiting for its store's readers to let go waits on the RELEASE,
//! not on a timer.
//!
//! The standing owner ruling of 2026-08-14 is that no product behavior in
//! Vigil is judged by wall-clock duration, and that an automatic re-attempt
//! fires on a real event and never on a periodic timer. The startup wait is
//! the one place in Vigil that still breaks it: `crates/vigil/src/store.rs`'s
//! `open_owned_waiting_for_readers` sleeps `READERS_CLEARING_INTERVAL`
//! (250 ms) and tries the open again, round and round, until the readers
//! happen to be gone at the moment it looks.
//!
//! What that costs is not tidiness. A store is free the instant the last
//! reader finishes hydrating, and a runtime that only looks four times a
//! second holds its own deployment hostage for up to a quarter of a second
//! after that — every start, on every node — while spending a wakeup every
//! 250 ms for as long as the wait lasts on a box that may be running on a
//! battery beside a camera. And the interval is a number nobody can defend:
//! it is not a declared policy, no operator can see it, and it decides how
//! promptly a deployment comes up.
//!
//! The replacement is not vigil's to invent, and it already exists one layer
//! down. contextdb ships `persistence::wait_for_reader_release(path, runtime,
//! stop) -> ReaderReleaseWait::{Released, Stopped, Unobservable}`: every
//! direct reader takes a SHARED advisory hold on the companion beside the
//! store for exactly as long as it holds the committed image, so asking for
//! that same hold EXCLUSIVELY is the caller's own question — "may I take this
//! store" — put to the only thing that can answer it. The blocking
//! acquisition sleeps in the kernel until the last holder lets go, or dies,
//! which the kernel treats as the same event. The wake IS the release,
//! nothing polls, and an explicit stop is answered as promptly as a release
//! through the cancellation token's own listener.
//!
//! Reader breadcrumbs decide nothing here, and saying otherwise would describe
//! a weaker wait than the one vigil actually gets: they are diagnosis, the
//! notes that let an unobservable wait NAME who is holding on. A reader whose
//! breadcrumb could not be published is still a reader and still holds the
//! wait, a diagnostic directory that cannot be read says nothing about whether
//! the store is free, and nothing in the store folder is opened, read, or
//! locked by the wait at all.
//!
//! It also waits on precisely the right window: a reading session holds the
//! store only while it LOADS the committed image, and that load is the only
//! window a would-be writer can collide with a reader at all (the contract is
//! written out in that suite's own header).
//!
//! Why this guard reads the SOURCE. Both halves of the behavior are already
//! pinned, behaviorally and unfakeably, by the sibling suites — a runtime that
//! waits and then comes up owning the store when the reader releases
//! (`crates/vigil-bin/tests/hydrating_reader_does_not_degrade_a_starting_runtime.rs`),
//! and a waiting runtime that still stops promptly when it is asked to, with
//! the abandoned-on-shutdown line
//! (`crates/vigil-bin/tests/a_waiting_runtime_still_stops_when_asked.rs`).
//! Those pass against the sleep loop and against a kernel wait alike, because
//! the two are externally indistinguishable in outcome: what separates them is
//! a quarter-second of latency and a wakeup rate, and asserting on either
//! would be exactly the elapsed-time judgment the ruling forbids. So the
//! behavioral legs stay where they are, as regressions the replacement must
//! keep passing, and what is pinned HERE is the shape: the wait carries no
//! sleep and no interval of its own, and it goes through the seam that owns
//! the kernel wait. Keep the shape and the timer cannot come back.
//!
//! Scope, stated exactly, because a guard whose comment misdescribes it is the
//! same defect it exists to catch: only the reader-clearing wait is read. Every
//! other sleep in vigil is a different caller with a different job and nothing
//! here constrains one.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn source(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The body of one function, from its signature to the line that closes it at
/// the same indentation.
fn function_body(text: &str, signature: &str) -> Option<String> {
    let start = text.find(signature)?;
    let rest = &text[start..];
    let indent: String = rest
        .lines()
        .next()?
        .chars()
        .take_while(|character| character.is_whitespace())
        .collect();
    let closing = format!("\n{indent}}}");
    let end = rest.find(&closing)? + closing.len();
    Some(rest[..end].to_string())
}

/// The file the startup wait lives in.
const STORE_SOURCE: &str = "crates/vigil/src/store.rs";

/// The wait itself.
const WAIT_SIGNATURE: &str = "fn open_owned_waiting_for_readers(";

/// A timer, however it is spelled. Sleeping is what makes a wait periodic, and
/// a periodic re-attempt is what the ruling of 2026-08-14 forbids.
const SLEEPS: [&str; 2] = ["thread::sleep(", "sleep("];

/// The pacing constant the loop turns on. Named separately because its
/// presence anywhere in this file means some caller still has an interval to
/// decide a wait with.
const PACING_INTERVAL: &str = "READERS_CLEARING_INTERVAL";

/// The seam that owns the kernel wait. contextdb takes the readers' own shared
/// hold on the store's companion exclusively, so the wake is the release
/// itself.
const RELEASE_WAIT_SEAM: &str = "wait_for_reader_release(";

#[test]
fn the_wait_for_a_stores_readers_carries_no_timer_of_its_own() {
    let store = source(STORE_SOURCE);
    let wait = function_body(&store, WAIT_SIGNATURE).unwrap_or_else(|| {
        panic!("the startup wait for readers must still live somewhere in {STORE_SOURCE}")
    });

    for sleeping in SLEEPS {
        assert!(
            !wait.contains(sleeping),
            "the runtime waits for its store's readers by sleeping and looking again. A store is \
             free the instant its last reader finishes hydrating, and a loop that only looks \
             every so often holds the deployment closed for the rest of that interval — every \
             start, on every node — and burns a wakeup for each turn until it does. It is also a \
             periodic re-attempt, which the standing owner ruling of 2026-08-14 forbids outright: \
             an automatic re-attempt fires on a real event. contextdb already blocks by taking \
             the readers' own shared hold on the store's companion exclusively, so the wake IS \
             the release. The body read:\n{wait}"
        );
    }

    assert!(
        !store.contains(PACING_INTERVAL),
        "a reader-clearing interval is still declared in {STORE_SOURCE}. How long a runtime \
         pauses before looking at its store again is not a number this file gets to choose: it \
         decides how promptly an operator's deployment comes up, no operator can see it, and no \
         declared policy names it. A wait that wakes on the release itself has no interval to \
         declare."
    );
}

#[test]
fn the_wait_for_a_stores_readers_goes_through_the_seam_that_owns_it() {
    let store = source(STORE_SOURCE);
    let wait = function_body(&store, WAIT_SIGNATURE).unwrap_or_else(|| {
        panic!("the startup wait for readers must still live somewhere in {STORE_SOURCE}")
    });

    assert!(
        wait.contains(RELEASE_WAIT_SEAM),
        "the wait must be contextdb's own: `persistence::wait_for_reader_release` asks \
         EXCLUSIVELY for the shared advisory hold every direct reader takes on the store's \
         companion for exactly as long as it holds the committed image, so the kernel wakes this \
         thread when the last holder lets go or dies and never before, and the caller's stop is \
         answered as promptly as a release. The readers themselves answer it — a breadcrumb is \
         diagnosis and decides nothing. Vigil re-implementing that wait — in any form — is the \
         downstream layer rebuilding an abstraction the layer below already owns, and this is \
         the one that was rebuilt as a 250 ms poll. The body read:\n{wait}"
    );
}

/// The guard's own proof that it can see what it claims to see. Without this,
/// a renamed wait would make both tests above pass by reading nothing at all,
/// which is indistinguishable from a fixed source.
#[test]
fn the_guard_recognizes_a_sleep_retry_wait_and_leaves_a_kernel_wait_alone() {
    let sleep_retry = r#"
        fn open_owned_waiting_for_readers(path: &Path) -> Result<OwnedOpen, String> {
            loop {
                match attempt() {
                    Ok(handle) => return Ok(OwnedOpen::Opened(handle)),
                    Err(CgError::StoreHeldByReaders { .. }) => {
                        std::thread::sleep(READERS_CLEARING_INTERVAL);
                    }
                    Err(error) => return Err(classify(error)),
                }
            }
        }
    "#;
    assert!(
        SLEEPS.iter().any(|sleeping| sleep_retry.contains(sleeping)),
        "positive control: the guard must recognize a sleep-retry wait, or the assertions above \
         prove nothing"
    );
    assert!(
        !sleep_retry.contains(RELEASE_WAIT_SEAM),
        "positive control: a sleep-retry wait reaches no release seam"
    );

    let kernel_wait = r#"
        fn open_owned_waiting_for_readers(path: &Path) -> Result<OwnedOpen, String> {
            loop {
                match attempt() {
                    Ok(handle) => return Ok(OwnedOpen::Opened(handle)),
                    Err(CgError::StoreHeldByReaders { .. }) => {
                        match wait_for_reader_release(path, runtime, stop) {
                            ReaderReleaseWait::Released => continue,
                            ReaderReleaseWait::Stopped => return Ok(abandoned_on_shutdown(path)),
                            ReaderReleaseWait::Unobservable(error) => return Err(named(error)),
                        }
                    }
                    Err(error) => return Err(classify(error)),
                }
            }
        }
    "#;
    assert!(
        !SLEEPS.iter().any(|sleeping| kernel_wait.contains(sleeping))
            && !kernel_wait.contains(PACING_INTERVAL)
            && kernel_wait.contains(RELEASE_WAIT_SEAM),
        "negative control: a wait that blocks on the release itself must satisfy this guard, \
         whatever the surrounding code ends up looking like — the shape is what is pinned here, \
         never a chosen arrangement"
    );
}
