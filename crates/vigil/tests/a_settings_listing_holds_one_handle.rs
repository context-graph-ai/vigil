//! A settings listing opens this deployment's store ONCE, however many secrets
//! it reports.
//!
//! The listing an operator asks for holds its own read-only handle on the store
//! for the whole of the answer. Reading the secrets' source lines used to be
//! the exception: `settings_environment::secret_source_lines` was handed the
//! store PATH rather than the handle already in hand, and opened the store
//! again once per entry in the declared secret roster — so a listing that
//! reports three secrets took four opens of one file, three of them behind the
//! back of a handle the same call already held.
//!
//! What that cost the operator is the point, and it is not tidiness. Each extra
//! open is a fresh chance to be refused: a reader that begins hydrating this
//! store part-way through a listing turns an answer that was already succeeding
//! into `StoreHeldByReaders`, and the `?` on that call threw away the roster
//! the listing had already built. The operator asked one question, the store
//! was healthy throughout, their node had already read most of the answer, and
//! they got a refusal — for a condition that arrived after the work was done.
//! There is nothing in the answer that could tell them so, either: how many
//! times a listing opened the store is invisible from what it prints, which is
//! exactly why this is proven on a counter and not on a rendering.
//!
//! So the promise these pins hold is: ONE open per listing, and a roster that
//! names every secret this deployment declares. A listing that costs one open
//! has no mid-listing window at all — the answer is decided by the single
//! observation its own open made, and a reader arriving after it cannot reach
//! in and change it. The sibling behavioral pins in
//! `crates/vigil-bin/tests/a_store_busy_with_readers_is_not_an_unreadable_store.rs`
//! hold what an operator is told when that ONE open is the refused one.
//!
//! The count is of opens through EITHER door, which is what makes it a cost
//! rather than a spelling: the listing's one open is now the read-only consumer
//! reader instead of a writable handle, and a counter that only saw writable
//! opens would report this listing as costing nothing while it went on opening
//! the store four times.
//!
//! `settings_store::settings_store_opens()` is a `test-support` seam and is in
//! no shipped artifact (`artifact_never_enables_test_support.rs` proves no
//! artifact build ever names the feature). It is read either side of a real
//! listing and the DIFFERENCE is asserted, so nothing here depends on what any
//! other test in the binary did first.
//!
//! Nothing here sleeps, reads a clock, spawns a process, or asserts on elapsed
//! time: every assertion is a counter read or a field read after one
//! deterministic call.

use vigil::settings_environment::SecretSourceLine;
use vigil::settings_model::ScopeTarget;
use vigil::settings_projection::report_by_direct_read_at;
use vigil::settings_store::{SettingsStore, settings_store_opens};

/// What one listing may cost this deployment's store. The listing holds its own
/// handle, so the answer is built through that one open and no other.
const OPENS_PER_LISTING: u64 = 1;

/// The secrets a deployment declares. Held as a roster rather than a count
/// because the defect scaled with THIS list: every entry added here used to add
/// another open to every listing, so a pin on the number alone would stop
/// meaning anything the moment a secret was added.
const DECLARED_SECRETS: [&str; 3] = ["rtsp_username", "rtsp_password", "fabric_ticket"];

fn target() -> ScopeTarget {
    let name = "one-handle-listing".to_string();
    ScopeTarget {
        tenant: name.clone(),
        site: name.clone(),
        node: name,
        camera: None,
    }
}

/// A deployment with a real store and nothing running, which is the journey an
/// operator takes when they ask their node what it is set to.
fn deployment() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("temporary data directory");
    // Opening the store is what makes this a deployment with a surface to read.
    // The path is taken off the handle that created it, so the listing below
    // reads the very file this fixture owns rather than one rebuilt from a
    // parent directory.
    let store = SettingsStore::open(directory.path()).expect("open the node-side settings store");
    let store_path = store.path().to_path_buf();
    drop(store);
    (directory, store_path)
}

fn secret_names(secrets: &[SecretSourceLine]) -> Vec<String> {
    let mut names: Vec<String> = secrets.iter().map(|line| line.secret.clone()).collect();
    names.sort();
    names
}

/// ONE test, deliberately.
///
/// `settings_store_opens()` counts opens for the whole PROCESS, so a delta
/// across one listing is only readable while nothing else in this binary is
/// opening a settings store. A second test here would open one to build its own
/// deployment and the two deltas would race — which is exactly what happened
/// when this file was first written as two. So both facts are asserted about
/// the SAME listing: it is one question an operator asked, and what it cost and
/// what it returned are two things about that one answer.
#[test]
fn a_settings_listing_opens_this_deployments_store_once_and_reports_every_secret() {
    let (directory, store_path) = deployment();

    let before = settings_store_opens();
    let report = report_by_direct_read_at(directory.path(), &store_path, &target())
        .expect("a deployment with a healthy store must be able to answer what it is set to");
    let opens = settings_store_opens() - before;

    // Established first: this listing really did report the whole roster, so the
    // open count below cannot be the cost of a roster that was dropped on the
    // way. The `?` that used to discard the built roster is the other half of
    // the same defect, and this is what catches it.
    let mut declared: Vec<String> = DECLARED_SECRETS
        .iter()
        .map(|name| name.to_string())
        .collect();
    declared.sort();
    assert_eq!(
        secret_names(&report.secrets),
        declared,
        "the listing must name every secret this deployment declares — an operator reading a \
         short roster cannot tell a secret that is unset from one the answer gave up on"
    );
    for line in &report.secrets {
        assert!(
            !line.statement.trim().is_empty(),
            "every secret in the roster must carry the line an operator reads, naming where the \
             value is coming from; `{}` carried nothing",
            line.secret
        );
    }
    assert!(
        !report.settings.is_empty(),
        "sanity: the listing must have actually answered — an empty report would make the count \
         below the cost of doing nothing"
    );

    assert_eq!(
        opens,
        OPENS_PER_LISTING,
        "the listing reported {} secrets and cost {opens} opens of one store. The \
         listing already holds its own handle for the whole of the answer; every open beyond that \
         one is work done behind the back of a handle in hand, and a fresh chance for a reader \
         arriving mid-answer to turn a listing that was already succeeding into a refusal. What a \
         listing costs its own store must also not scale with the roster it reports, or every \
         secret this product declares makes every listing more likely to be refused part-way \
         through, on a store that is perfectly healthy.",
        report.secrets.len()
    );
}
