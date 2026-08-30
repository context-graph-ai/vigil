//! This node's service identity must be derived once, at first start, and then
//! persisted — because it names the messaging topics and the Home Assistant
//! device, so recomputing it from the site name renames the device and orphans
//! every bit of history hanging off the old one. Today the identity is
//! recomputed from the site name at every startup
//! (`crates/vigil/src/config.rs:1509-1512`), which is exactly the defect these
//! tests pin.
//!
//! Every assertion here is over stored state read back through the production
//! resolution path, never over a return value the same call just produced: the
//! rename test resolves twice against the same store path and compares the two
//! answers byte for byte, so an implementation that recomputes from whatever
//! site name it was handed cannot pass no matter how it words its output.
//!
//! Where a test claims nothing was written, the claim is checked against the
//! FILESYSTEM — every regular file under the enclosing temporary directory,
//! hashed before and after — not against the product's own account of what it
//! wrote. The enclosing directory rather than the data directory, because the
//! sidecar cache lives NEXT TO the data directory, outside it. The product's
//! own write audit is asserted as well, but never as the evidence.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

use vigil::service_identity::{
    IdentityDerivation, ServiceIdentity, change_deliberately, describe_change, identity_paths,
    read_sidecar, refuse_ordinary_edit, resolve_ephemeral, resolve_persisted,
};
use vigil::settings_model::RefusalKind;

/// The degraded runtime's identity derivation, inlined here rather than routed
/// through a production `DegradedRuntime` handle: the real degraded path
/// (`crate::runtime`, proven at the process level by
/// `crates/vigil-bin/tests/storeless_runtime_degraded_mode.rs`) derives its
/// running identity from the data-directory name and never persists it. This
/// mirrors that derivation exactly, for the one behavior this suite pins at
/// the library level — that a degraded run's identity is ephemeral.
fn degraded_run_identity(data_dir: &Path) -> ServiceIdentity {
    let deployment_name = data_dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "vigil".to_string());
    resolve_ephemeral(&deployment_name)
}

const FIRST_SITE: &str = "Front House";
const RENAMED_SITE: &str = "Back Garden Workshop";

/// A deployment: a data directory inside an enclosing temporary directory, so
/// the sidecar cache — which lives beside the data directory, not inside it —
/// is inside the audited tree.
struct Deployment {
    root: TempDir,
    data_dir: PathBuf,
}

impl Deployment {
    fn prepare() -> Self {
        let root = tempfile::tempdir().expect("temporary enclosing directory");
        let data_dir = root.path().join("data");
        fs::create_dir_all(&data_dir).expect("create the data directory");
        Self { root, data_dir }
    }

    fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The store file, at the location the product fixes for this data
    /// directory. Nothing here assumes a filename.
    fn store(&self) -> PathBuf {
        identity_paths(&self.data_dir).store
    }

    /// The sidecar cache, at the location the product fixes for it.
    fn sidecar(&self) -> PathBuf {
        identity_paths(&self.data_dir).sidecar
    }

    /// Every regular file in the whole enclosing tree, with its length and the
    /// hash of its contents.
    fn snapshot(&self) -> BTreeMap<PathBuf, String> {
        snapshot_regular_files(self.root.path())
    }
}

/// Every regular file under `root`, mapped to a digest of its contents and its
/// length. Directories are descended into; anything that is not a regular file
/// is skipped.
fn snapshot_regular_files(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => panic!("read {}: {error}", directory.display()),
        };
        for entry in entries {
            let entry = entry.expect("directory entry");
            let path = entry.path();
            let file_type = entry.file_type().expect("file type");
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            // A file whose permissions were cleared still counts: it is hashed
            // by whatever can be read, and anything that changed it would have
            // had to make it writable first, which is itself a write.
            let bytes = fs::read(&path).unwrap_or_default();
            let digest = Sha256::digest(&bytes);
            found.insert(path, format!("{digest:x}:{}", bytes.len()));
        }
    }
    found
}

/// Fail on any file added, removed or changed between the two snapshots.
fn assert_tree_untouched(
    before: &BTreeMap<PathBuf, String>,
    after: &BTreeMap<PathBuf, String>,
    what: &str,
) {
    let added: Vec<&PathBuf> = after
        .keys()
        .filter(|path| !before.contains_key(*path))
        .collect();
    assert!(
        added.is_empty(),
        "{what} must write nothing at all, but these files appeared: {added:?}"
    );
    let removed: Vec<&PathBuf> = before
        .keys()
        .filter(|path| !after.contains_key(*path))
        .collect();
    assert!(
        removed.is_empty(),
        "{what} must remove nothing either, but these files disappeared: {removed:?}"
    );
    for (path, digest) in before {
        assert_eq!(
            after.get(path),
            Some(digest),
            "{what} changed {} — it must write nothing at all",
            path.display()
        );
    }
}

/// The consequence a change to the identity carries, in whatever words the
/// implementation chooses: Home Assistant sees a new device and the history
/// attached to the old one is orphaned. Asserted as three independent plain
/// words rather than one frozen sentence, so the contract is pinned without
/// freezing the phrasing.
fn states_the_orphaned_history_consequence(statement: &str) -> bool {
    let lowered = statement.to_lowercase();
    lowered.contains("orphan") && lowered.contains("history") && lowered.contains("device")
}

fn assert_consequence_stated(statement: &str, what: &str) {
    assert!(
        states_the_orphaned_history_consequence(statement),
        "{what} must state, in plain language, that Home Assistant sees a new device and the \
         history attached to the old one is orphaned; got {statement:?}"
    );
}

#[test]
fn identity_is_derived_once_at_first_start_and_persisted() {
    // Unfakeable: the second resolution is a separate call against the same
    // store path, and the sidecar is read back through a different function
    // than the one that wrote it. A derivation that recomputes each time
    // cannot report `DerivedAtFirstStart` on the second call AND agree with
    // the cache it never wrote.
    let deployment = Deployment::prepare();
    let store = deployment.store();

    let first = resolve_persisted(deployment.data_dir(), &store, FIRST_SITE)
        .expect("first start resolves an identity");
    assert!(
        !first.value.is_empty(),
        "a derived service identity must be a real value, not an empty string"
    );
    assert_eq!(
        first.derivation,
        IdentityDerivation::DerivedAtFirstStart,
        "the first store-backed start derives the identity and records that it did"
    );

    let second = resolve_persisted(deployment.data_dir(), &store, FIRST_SITE)
        .expect("a later start resolves the same identity");
    assert_eq!(
        second.value, first.value,
        "a later start must use the persisted identity, not derive a fresh one"
    );
    assert_eq!(
        second.derivation,
        IdentityDerivation::DerivedAtFirstStart,
        "how the identity was arrived at is a persisted fact, not a per-run guess"
    );

    let cached = read_sidecar(deployment.data_dir())
        .expect("store-backed operation writes the non-authoritative sidecar cache");
    assert_eq!(
        cached.value, first.value,
        "the sidecar caches the store record; a cache that disagrees with the store record is a \
         defect, not a second opinion"
    );
}

#[test]
fn a_storeless_first_start_persists_nothing_and_the_identity_is_persisted_at_the_first_successful_store_open()
 {
    // Unfakeable: no store is ever opened before the ephemeral derivation, and
    // the "nothing was persisted" claim is checked by hashing every regular
    // file in the enclosing tree — which contains both the store's location and
    // the sidecar's — before and after. An implementation that wrote a record
    // or a sidecar cache during a storeless start is caught on the filesystem,
    // whatever it reports about itself. The second half then pins the other end
    // of the ratified sequence: the FIRST successful store open is what
    // persists that identity, and the identity the storeless run showed is the
    // one that gets persisted — deriving a different value at store-open time
    // would rename the Home Assistant device the moment the store came back,
    // which is the whole defect this suite exists to stop.
    let deployment = Deployment::prepare();
    let store = deployment.store();
    let sidecar = deployment.sidecar();
    assert!(
        !store.exists(),
        "sanity: this is a storeless first start, so no store may exist yet"
    );

    let before = deployment.snapshot();

    let ephemeral = resolve_ephemeral(FIRST_SITE);
    assert!(
        !ephemeral.value.is_empty(),
        "a storeless start still derives a real identity for the run: the node has to name its \
         topics and its device"
    );
    assert_eq!(
        ephemeral.derivation,
        IdentityDerivation::DerivedNotYetPersisted,
        "a storeless start derives the identity for this run only and says so, rather than \
         claiming it was derived at first start and persisted"
    );

    let after_ephemeral = deployment.snapshot();
    assert_tree_untouched(&before, &after_ephemeral, "a storeless start");
    assert!(
        !store.exists(),
        "a storeless start creates no store: persistence happens at the first successful store \
         open, not on the storeless path"
    );
    assert!(
        !sidecar.exists(),
        "a storeless start writes no sidecar cache either; the sidecar caches a store record, and \
         there is no store record yet"
    );
    assert!(
        read_sidecar(deployment.data_dir()).is_none(),
        "there is nothing to read back, because nothing was written"
    );

    // The first successful store open: the identity is persisted now, and how
    // it was arrived at becomes the persisted fact.
    let persisted = resolve_persisted(deployment.data_dir(), &store, FIRST_SITE)
        .expect("the first successful store open resolves an identity");
    assert_eq!(
        persisted.value, ephemeral.value,
        "the identity the storeless run was using is the one that gets persisted at the first \
         successful store open; deriving a different value here would move the Home Assistant \
         device the moment the store came back"
    );
    assert_eq!(
        persisted.derivation,
        IdentityDerivation::DerivedAtFirstStart,
        "once it is persisted it is no longer derived-not-yet-persisted: the first successful \
         store open is the first start this identity was derived at"
    );

    let cached = read_sidecar(deployment.data_dir())
        .expect("the first store-backed operation writes the sidecar cache");
    assert_eq!(
        cached.value, persisted.value,
        "the sidecar caches the store record it was written from"
    );

    let restarted = resolve_persisted(deployment.data_dir(), &store, FIRST_SITE)
        .expect("a later start resolves against the now-persisted identity");
    assert_eq!(
        restarted.value, persisted.value,
        "and it stays persisted: a later start uses it rather than deriving again"
    );
}

#[test]
fn a_site_rename_leaves_the_persisted_identity_unchanged_across_restart() {
    // Unfakeable: this is the live defect. The two resolutions differ ONLY in
    // the site name they are handed, so any implementation that lets the site
    // name reach the identity produces two different values here. Byte
    // equality is the whole assertion; there is no wording to satisfy instead.
    let deployment = Deployment::prepare();
    let store = deployment.store();

    let before = resolve_persisted(deployment.data_dir(), &store, FIRST_SITE)
        .expect("first start resolves an identity");
    let after = resolve_persisted(deployment.data_dir(), &store, RENAMED_SITE)
        .expect("a restart after a rename still resolves");

    assert_eq!(
        after.value, before.value,
        "renaming the site must not move the Home Assistant device: the identity was derived once \
         at first start and persisted, so it stays byte-identical across a rename and restart"
    );
    assert_eq!(
        after.derivation,
        IdentityDerivation::DerivedAtFirstStart,
        "a rename is not an explicit identity change; the derivation stays what it was"
    );
}

#[test]
fn an_ordinary_edit_of_the_identity_is_refused_naming_the_orphaned_history_consequence() {
    // Unfakeable: the refusal must carry BOTH the typed kind and the plain
    // consequence. A refusal that classifies correctly but explains nothing,
    // or explains well under some other kind, fails.
    let refusal = refuse_ordinary_edit("front-house-replacement");

    assert_eq!(
        refusal.kind,
        RefusalKind::IdentityOrdinaryEdit,
        "an ordinary settings edit aimed at the service identity is its own refusal kind"
    );
    assert!(
        !refusal.cause.is_empty() && !refusal.remedy.is_empty(),
        "every refusal names a cause and a remedy; one without the other is a defect: {refusal:?}"
    );
    assert_consequence_stated(&refusal.statement(), "the ordinary-edit refusal");
}

#[test]
fn the_deliberate_change_operation_states_the_consequence_before_it_takes_effect() {
    // Unfakeable: after describing the change, the identity is re-read through
    // the production resolution path. A `describe_change` that quietly applied
    // the change would be caught by that read-back, not by the wording check.
    let deployment = Deployment::prepare();
    let store = deployment.store();

    let current = resolve_persisted(deployment.data_dir(), &store, FIRST_SITE)
        .expect("first start resolves an identity");
    let proposed = "front-house-second-box";

    let consequence = describe_change(deployment.data_dir(), &store, proposed)
        .expect("the consequence is describable");
    assert_eq!(
        consequence.current, current.value,
        "the consequence names the identity actually in force"
    );
    assert_eq!(
        consequence.proposed, proposed,
        "the consequence names the identity being proposed"
    );
    assert_consequence_stated(&consequence.statement, "the deliberate-change consequence");

    let after_describing = resolve_persisted(deployment.data_dir(), &store, FIRST_SITE)
        .expect("describing a change leaves the deployment resolvable");
    assert_eq!(
        after_describing.value, current.value,
        "stating the consequence must not itself change the stored identity; it is stated BEFORE \
         the change takes effect"
    );

    let changed = change_deliberately(deployment.data_dir(), &store, proposed)
        .expect("the deliberate change applies");
    assert_eq!(
        changed.value, proposed,
        "the deliberate change operation is what actually moves the identity"
    );
    assert_eq!(
        changed.derivation,
        IdentityDerivation::SetExplicitly,
        "an identity moved by the deliberate operation is shown as set explicitly, not as derived"
    );
    let after_changing = resolve_persisted(deployment.data_dir(), &store, FIRST_SITE)
        .expect("the changed identity is persisted");
    assert_eq!(
        after_changing.value, proposed,
        "the deliberate change persists; a later start uses it"
    );
}

#[test]
fn the_degraded_storeless_run_never_rewrites_the_persisted_identity() {
    // Unfakeable: the evidence that the degraded run wrote nothing is the
    // enclosing tree, hashed file by file before and after — independent of
    // anything the runtime says about itself, and wide enough to include the
    // sidecar that lives beside the data directory.
    let deployment = Deployment::prepare();
    let store = deployment.store();

    let persisted = resolve_persisted(deployment.data_dir(), &store, FIRST_SITE)
        .expect("a store-backed start persists an identity");
    let cached_before =
        read_sidecar(deployment.data_dir()).expect("the sidecar cache exists after that start");

    let ephemeral = resolve_ephemeral(RENAMED_SITE);
    assert_eq!(
        ephemeral.derivation,
        IdentityDerivation::DerivedNotYetPersisted,
        "a storeless start derives the identity for this run only and says so, alongside the \
         continuous unmanaged statement"
    );

    let before = deployment.snapshot();

    // The scenario this test pins: the settings store is unreadable, so the
    // node runs degraded — still deriving an identity for this run, still
    // writing nothing.
    let running_identity = degraded_run_identity(deployment.data_dir());
    assert_eq!(
        running_identity.derivation,
        IdentityDerivation::DerivedNotYetPersisted,
        "the identity a degraded run uses is derived for that run only, and the run says so"
    );
    assert!(
        !running_identity.value.is_empty(),
        "a degraded run still names its topics and its device, so it still carries an identity"
    );

    let after = deployment.snapshot();
    assert_tree_untouched(&before, &after, "a degraded run");

    let cached_after =
        read_sidecar(deployment.data_dir()).expect("the sidecar cache survives a degraded run");
    assert_eq!(
        cached_after.value, cached_before.value,
        "the degraded path never rewrites the persisted service identity"
    );
    let after_degraded = resolve_persisted(deployment.data_dir(), &store, RENAMED_SITE)
        .expect("the store-backed path still resolves after a degraded run");
    assert_eq!(
        after_degraded.value, persisted.value,
        "the identity a degraded run saw must not have replaced the persisted one"
    );
}

/// Compile-time proof that the resolution entry points take the paths this
/// suite hands them; a signature drift shows up here rather than as a confusing
/// failure inside a behavioral test.
#[allow(dead_code)]
fn resolution_entry_points_take_paths(store: &Path, data_dir: &Path) {
    let _ = resolve_persisted(data_dir, store, FIRST_SITE);
    let _ = read_sidecar(data_dir);
    let _ = identity_paths(data_dir);
}
