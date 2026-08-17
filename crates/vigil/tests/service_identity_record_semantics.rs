//! How the identity record is read back is a property of the record, not of
//! its prose.
//!
//! The identity carries how it was arrived at, and the operator surface shows
//! that beside the value: an identity someone deliberately moved reads
//! differently from one derived at first start. Two things therefore have to be
//! true of the stored record. Its classification has to survive an editor
//! touching the human-readable reason, because the reason is prose written for
//! a person and prose gets reworded. And a deliberate change has to beat the
//! record it replaces whichever order the two are read back in, because "which
//! of these is in force" cannot be left to how rows happen to come off storage.
//!
//! Both are asserted through the public identity API against a real store, so
//! neither can be satisfied by a reader that happens to work on today's row
//! order or today's wording.

use std::path::PathBuf;

use vigil::service_identity::{
    IdentityDerivation, change_deliberately, describe_change, identity_paths, resolve_persisted,
};
use vigil::settings_model::SERVICE_IDENTITY_SETTING;
use vigil::settings_store::SettingsStore;

const SITE_NAME: &str = "home farm";

/// The identity an operator deliberately moves to.
const CHOSEN_IDENTITY: &str = "north_gate_node";

/// The same meaning as the reason a deliberate change records, written
/// differently — a copy edit, which is all it takes to break a reader that
/// recovers the classification by matching the prose.
const REWORDED_DELIBERATE_REASON: &str =
    "set on purpose through the deliberate identity-change operation";

struct Deployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
}

impl Deployment {
    fn prepare() -> Self {
        let tmp = tempfile::tempdir().expect("temporary data directory");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("create the data directory");
        Self {
            _tmp: tmp,
            data_dir,
        }
    }

    fn store_path(&self) -> PathBuf {
        identity_paths(&self.data_dir).store
    }

    fn store(&self) -> SettingsStore {
        SettingsStore::open(&self.data_dir).expect("open the settings store")
    }
}

/// Unfakeable because the record is written by the real deliberate-change
/// operation and only its human-readable reason is then reworded — every typed
/// field is left exactly as production wrote it. A reader that recovers the
/// classification from a typed property is untouched by the edit; one that
/// recovers it by comparing prose silently reports the operator's deliberate
/// choice as a value Vigil derived for them, which is the opposite of what
/// happened and is shown to them as such.
#[test]
fn the_identity_derivation_survives_a_rewording_of_the_reason_prose() {
    let deployment = Deployment::prepare();
    let store_path = deployment.store_path();

    // A first start, then a deliberate move: the ordinary way a node arrives at
    // an explicitly-set identity.
    resolve_persisted(&store_path, SITE_NAME).expect("the first start resolves an identity");
    describe_change(&store_path, CHOSEN_IDENTITY).expect("the consequence is stated first");
    let changed =
        change_deliberately(&store_path, CHOSEN_IDENTITY).expect("the deliberate change applies");
    assert_eq!(
        changed.derivation,
        IdentityDerivation::SetExplicitly,
        "the operation itself reports the change it just made as a deliberate one"
    );

    // An editor rewords the reason. Nothing else about the record moves.
    let store = deployment.store();
    let records = store
        .records(SERVICE_IDENTITY_SETTING)
        .expect("read the identity records back");
    let mut identity_record = records
        .into_iter()
        .find(|record| record.value.to_string() == CHOSEN_IDENTITY)
        .expect("the deliberate change left a record carrying the chosen identity");
    identity_record.reason = REWORDED_DELIBERATE_REASON.to_string();
    store
        .write_record(identity_record)
        .expect("store the reworded record");

    let resolved =
        resolve_persisted(&store_path, SITE_NAME).expect("a later start resolves the identity");
    assert_eq!(
        resolved.value, CHOSEN_IDENTITY,
        "the identity in force is still the one the operator chose"
    );
    assert_eq!(
        resolved.derivation,
        IdentityDerivation::SetExplicitly,
        "and it is still reported as deliberately set. How an identity was arrived at is a fact \
         about the record, and recovering it by matching the reason text means any rewording of \
         that sentence re-reads every stored identity as one Vigil derived — telling an operator \
         their deliberate choice was automatic, on the read-only line they are meant to trust"
    );
}

/// Unfakeable because it asserts the outcome across a genuine restart-shaped
/// read: the change is written through the real operation and then resolved
/// afresh, the way a later start does. A resolution that picks between the
/// first-start record and the deliberate one by anything that can tie — equal
/// timestamps broken by whichever row is read first — is not reliably wrong,
/// which is exactly why it must be pinned rather than left to chance.
#[test]
fn a_deliberate_change_outranks_the_first_start_record_on_a_later_start() {
    let deployment = Deployment::prepare();
    let store_path = deployment.store_path();

    let first = resolve_persisted(&store_path, SITE_NAME).expect("the first start persists one");
    assert_eq!(
        first.derivation,
        IdentityDerivation::DerivedAtFirstStart,
        "a first start derives and persists the identity"
    );
    let derived_value = first.value.clone();
    assert_ne!(
        derived_value, CHOSEN_IDENTITY,
        "the chosen identity has to differ from the derived one, or this proves nothing"
    );

    change_deliberately(&store_path, CHOSEN_IDENTITY).expect("the deliberate change applies");

    let resolved =
        resolve_persisted(&store_path, SITE_NAME).expect("a later start resolves the identity");
    assert_eq!(
        resolved.value, CHOSEN_IDENTITY,
        "the later start runs under the identity the operator deliberately chose. Handing back \
         the superseded first-start value moves the Home Assistant device back to a name the \
         operator already moved away from, and orphans the history that accumulated under the \
         one they chose. Resolved {resolved:?} against a derived {derived_value:?}"
    );
    assert_eq!(
        resolved.derivation,
        IdentityDerivation::SetExplicitly,
        "and it still reads as deliberately set"
    );
}
