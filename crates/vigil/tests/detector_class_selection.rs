//! Which classes the detector looks for is one setting, resolved from the store,
//! and recognition has nothing to do with it.
//!
//! Configured nothing, Vigil looks for people and says so — the value is its own
//! and it may revise it. Named a list, that list is what reaches the detector,
//! exactly, with nothing added and nothing dropped, whether or not recognition
//! is running. A class name the model inventory does not carry is refused when it
//! is written, naming the field and the offending entry, because the inventory
//! lives with the model rather than being frozen into the packaging.
//!
//! The old behavior this replaces derived the detector's allowlist from the
//! recognition configuration at the moment a detector was constructed: with
//! recognition off it fell back to person-only, and with recognition on it
//! widened to recognition's covered classes. That coupling is what made a widened
//! class list detect nothing.
//!
//! RED: `vigil::settings_store` and `vigil::settings_backends` are skeleton-only
//! (`todo!()` bodies) pending the settings-store implementation.

use std::sync::atomic::{AtomicU64, Ordering};

use vigil::PersistedClock;
use vigil::settings_backends::{
    DECLARED_DETECTION_CLASS_COUNT, declared_detection_class_inventory, detection_class_index,
    validate_detection_classes,
};
use vigil::settings_model::{
    Author, ControlState, DETECTOR_CLASSES_SETTING, RECOGNITION_COVERED_CLASSES_SETTING,
    RefusalKind, Scope, ScopeTarget, SettingValue, SettingsError, Surface,
};
use vigil::settings_store::SettingsStore;

const DETECTOR_CLASSES: &str = DETECTOR_CLASSES_SETTING;

/// The recognition-side setting the old construction path read to decide the
/// detector's allowlist. Written here purely so the tests can prove it no longer
/// reaches that decision.
const RECOGNITION_COVERED_CLASSES: &str = RECOGNITION_COVERED_CLASSES_SETTING;

const NODE: &str = "node-a";

/// A deterministic, strictly advancing persisted clock: distinct ordered stamps
/// with no sleep and no dependence on machine speed.
fn advancing_clock() -> PersistedClock {
    let next = AtomicU64::new(1_700_000_000_000);
    PersistedClock::from_millis_source(move || next.fetch_add(1_000, Ordering::SeqCst))
}

fn open(directory: &tempfile::TempDir) -> SettingsStore {
    SettingsStore::open_with_clock(directory.path(), advancing_clock())
        .expect("open the node-side settings store")
}

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: "owner".to_string(),
        site: "home".to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

/// The resolved class list as the detector construction path consumes it: read
/// from the store, and from nowhere else.
fn resolved_classes(store: &SettingsStore) -> SettingValue {
    store
        .resolve(DETECTOR_CLASSES, &target())
        .expect("resolve the detector class list")
        .requested
}

/// The allowlist the detector is ACTUALLY constructed with: the resolved class
/// names already mapped to their inventory indices. This is the seam the old
/// recognition coupling lived on (`crates/vigil/src/yolox_detector.rs:77`), so a
/// test that binds this binds detector construction rather than the resolved
/// value one step upstream.
fn construction_allowlist(store: &SettingsStore) -> Vec<usize> {
    store
        .detector_class_allowlist(&target())
        .expect("the detector construction path resolves its class allowlist")
}

/// The indices those names occupy, read through the inventory rather than
/// assumed as ordinals.
fn indices_of(names: &[&str]) -> Vec<usize> {
    names
        .iter()
        .map(|name| {
            detection_class_index(name)
                .unwrap_or_else(|| panic!("{name} is a declared detection class"))
        })
        .collect()
}

fn turn_recognition_on(store: &SettingsStore, covered: &[&str]) {
    store
        .set_local(
            RECOGNITION_COVERED_CLASSES,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::list(covered.iter().copied()),
        )
        .expect("recognition's covered classes are an ordinary setting");
}

/// Unfakeable: the store is empty, so the value can only come from Vigil's own
/// automatic layer, and both the author and the control state are asserted
/// alongside the value — a hardcoded person-only default returned without a
/// record behind it has no reason to carry and no author to report, and the
/// reason assertion catches that.
#[test]
fn omitted_classes_keep_person_only_and_stay_automatic() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    let effective = store
        .resolve(DETECTOR_CLASSES, &target())
        .expect("a fresh install still answers what it is looking for");

    assert_eq!(
        effective.requested,
        SettingValue::list(["person"]),
        "configured nothing, Vigil looks for people: {effective:?}"
    );
    assert_eq!(
        effective.author,
        Author::Automatic,
        "nobody pinned this; it is Vigil's own choice: {effective:?}"
    );
    assert_eq!(
        effective.control_state,
        ControlState::Automatic,
        "and Vigil may revise it: {effective:?}"
    );
    assert!(
        !effective.reason.is_empty(),
        "an automatic value owes the reason it was chosen; a blank field is a defect: {effective:?}"
    );

    let stored = store
        .records(DETECTOR_CLASSES)
        .expect("read the stored records");
    assert!(
        !stored
            .iter()
            .any(|record| record.author == Author::LocalExplicit),
        "reading a fresh install pins nothing on the owner's behalf: {stored:?}"
    );
}

/// Unfakeable: the explicit list deliberately omits `person`, which is the value
/// every silent re-substitution in the old path reached for. An implementation
/// that adds person back, sorts a canonical set in, or drops an entry it does
/// not recognize returns something other than exactly `["dog", "car"]` here.
#[test]
fn explicit_class_list_reaches_the_detector_allowlist_exactly() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    store
        .set_local(
            DETECTOR_CLASSES,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::list(["dog", "car"]),
        )
        .expect("an explicit class list is an ordinary local write");

    let effective = store
        .resolve(DETECTOR_CLASSES, &target())
        .expect("resolve the detector class list");
    assert_eq!(
        effective.requested,
        SettingValue::list(["dog", "car"]),
        "what the owner named is exactly what the detector looks for — person is NOT added back: \
         {effective:?}"
    );
    assert_eq!(
        effective.control_state,
        ControlState::SetByYou,
        "and the surface attributes it to them: {effective:?}"
    );
    assert_eq!(effective.author, Author::LocalExplicit);
}

/// Unfakeable: recognition is moved twice in one test — once NARROWER than the
/// explicit list and once WIDER than it — against the same stored class list. An
/// implementation that intersects fails the narrow pass, one that unions fails
/// the wide pass, and one that errors on a mismatch fails both. Only a class list
/// that ignores recognition entirely returns the same three answers.
#[test]
fn recognition_never_widens_or_narrows_an_explicit_class_list() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    store
        .set_local(
            DETECTOR_CLASSES,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::list(["dog", "car"]),
        )
        .expect("an explicit class list is an ordinary local write");

    let with_recognition_off = resolved_classes(&store);
    assert_eq!(with_recognition_off, SettingValue::list(["dog", "car"]));

    turn_recognition_on(&store, &["dog"]);
    assert_eq!(
        resolved_classes(&store),
        with_recognition_off,
        "recognition covering LESS than the explicit list narrows nothing"
    );

    turn_recognition_on(&store, &["dog", "car", "cat", "person"]);
    assert_eq!(
        resolved_classes(&store),
        with_recognition_off,
        "recognition covering MORE than the explicit list widens nothing"
    );

    let effective = store
        .resolve(DETECTOR_CLASSES, &target())
        .expect("resolve the detector class list with recognition running");
    assert_eq!(
        effective.control_state,
        ControlState::SetByYou,
        "and the class list is still the owner's, not something recognition co-authored: \
         {effective:?}"
    );
}

/// Unfakeable: the refusal is asserted at the store write AND at the validator
/// the write runs, and the declared inventory is read in the same test — so an
/// implementation that refuses everything, or that validates against a list
/// hardcoded next to the test rather than the model's own inventory, fails on
/// `dog` being present and `spaceship` being absent. Nothing being stored is
/// asserted separately, because a value that is written and then dropped at
/// construction is the failure this refusal exists to prevent.
#[test]
fn unknown_class_name_is_refused_naming_the_field_and_the_value() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    let inventory = declared_detection_class_inventory();
    assert!(
        inventory.contains(&"dog"),
        "sanity: the model inventory really carries `dog`, so the refusal below is about \
         `spaceship` alone: {inventory:?}"
    );
    assert!(
        !inventory.contains(&"spaceship"),
        "sanity: the model inventory does not carry `spaceship`: {inventory:?}"
    );

    let error = store
        .set_local(
            DETECTOR_CLASSES,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::list(["dog", "spaceship"]),
        )
        .expect_err("a class name the model does not carry is refused when it is written");

    let refused = match error {
        SettingsError::Refused(refused) => refused,
        other => panic!("expected a refusal carrying a cause and a remedy, got: {other:?}"),
    };
    assert_eq!(
        refused.kind,
        RefusalKind::InvalidClass {
            setting: DETECTOR_CLASSES.to_string(),
        },
        "the refusal is typed as an invalid class and names which setting: {refused:?}"
    );
    let statement = refused.statement();
    assert!(
        statement.contains(DETECTOR_CLASSES),
        "the refusal names the field: {statement:?}"
    );
    assert!(
        statement.contains("spaceship"),
        "the refusal names the offending entry, not just that something was wrong: {statement:?}"
    );

    assert!(
        validate_detection_classes(&["dog".to_string(), "spaceship".to_string()]).is_err(),
        "the same judgement is reachable at the validator, so it is validation against the \
         model's inventory rather than a store-local list"
    );
    assert!(
        validate_detection_classes(&["dog".to_string(), "car".to_string()]).is_ok(),
        "and a list the inventory does carry is accepted — this is not a blanket refusal"
    );

    let stored = store
        .records(DETECTOR_CLASSES)
        .expect("read the stored records after the refusal");
    assert!(
        stored.is_empty(),
        "a refused class list stores nothing, not even the entries that were valid: {stored:?}"
    );
}

/// Unfakeable: this binds the DETECTOR-CONSTRUCTION seam — the indices the
/// detector is actually built with — rather than the resolved setting value,
/// which recognition config structurally cannot influence and which therefore
/// could not have caught the old coupling at all. Both cases the coupling had a
/// branch for are driven: the OMITTED case (recognition on used to WIDEN the
/// constructed allowlist to recognition's covered classes, recognition off used
/// to fall back to person-only) and the EXPLICIT case. In each, the allowlist
/// with recognition ON is asserted identical to the allowlist with recognition
/// OFF. An implementation that kept the widening branch at any construction site
/// returns three indices for the omitted case and fails.
#[test]
fn runtime_never_derives_detector_construction_classes_from_recognition_config() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    let omitted_with_recognition_off = construction_allowlist(&store);
    assert_eq!(
        omitted_with_recognition_off,
        indices_of(&["person"]),
        "sanity: with nothing configured, the detector is constructed to look for people"
    );

    turn_recognition_on(&store, &["person", "dog", "cat"]);
    assert_eq!(
        construction_allowlist(&store),
        omitted_with_recognition_off,
        "OMITTED case: turning recognition on does not widen the allowlist the detector is built \
         with; the class list is the only input to that decision"
    );

    store
        .reset_local(
            RECOGNITION_COVERED_CLASSES,
            Surface::VigilSettings,
            &Scope::node(NODE),
        )
        .expect("turn recognition's covered classes back off");
    assert_eq!(
        construction_allowlist(&store),
        omitted_with_recognition_off,
        "OMITTED case: and turning it back off does not narrow it back to a person-only fallback \
         derived from recognition"
    );

    store
        .set_local(
            DETECTOR_CLASSES,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::list(["dog", "car"]),
        )
        .expect("the owner names a class list");
    let explicit_with_recognition_off = construction_allowlist(&store);
    assert_eq!(
        explicit_with_recognition_off,
        indices_of(&["dog", "car"]),
        "EXPLICIT case: the construction path consumes exactly what the owner named"
    );

    turn_recognition_on(&store, &["person", "dog", "cat"]);
    assert_eq!(
        construction_allowlist(&store),
        explicit_with_recognition_off,
        "EXPLICIT case: with recognition running the detector is built with the same allowlist, \
         neither widened by recognition's covered classes nor intersected with them"
    );

    store
        .reset_local(
            RECOGNITION_COVERED_CLASSES,
            Surface::VigilSettings,
            &Scope::node(NODE),
        )
        .expect("turn recognition's covered classes back off");
    assert_eq!(
        construction_allowlist(&store),
        explicit_with_recognition_off,
        "EXPLICIT case: and identical again with recognition off — the same allowlist either way"
    );
}

/// Unfakeable: the promise is that the WHOLE declared inventory is selectable,
/// so the inventory is asserted as a closed enumeration — exactly
/// `DECLARED_DETECTION_CLASS_COUNT` entries, no duplicates — and then a spread
/// reaching across the whole of it, INCLUDING the final entry, is written and
/// read back off the construction seam. An implementation carrying a truncated
/// inventory, a padded one, or one whose tail is unreachable fails here, which a
/// `contains("dog")` sample cannot catch. Indices come from
/// `detection_class_index` rather than assumed ordinals, so this binds the
/// name-to-index mapping the detector is built from rather than a guess at it.
#[test]
fn every_declared_detection_class_is_selectable_across_the_whole_inventory() {
    let inventory = declared_detection_class_inventory();
    assert_eq!(
        inventory.len(),
        DECLARED_DETECTION_CLASS_COUNT,
        "the declared inventory is the whole shipped class list, not a sample of it: {inventory:?}"
    );
    let mut unique = inventory.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        DECLARED_DETECTION_CLASS_COUNT,
        "a duplicated name would make one index unreachable by name: {inventory:?}"
    );

    for (position, name) in inventory.iter().enumerate() {
        assert_eq!(
            detection_class_index(name),
            Some(position),
            "every declared name maps to its own index, so any class in the inventory can be \
             named and reach the detector: {name}"
        );
    }

    // A spread across the whole inventory: its first entry, entries drawn from
    // the middle, and — the case a sample always misses — its LAST entry.
    let spread: Vec<&str> = vec![
        inventory[0],
        inventory[DECLARED_DETECTION_CLASS_COUNT / 4],
        inventory[DECLARED_DETECTION_CLASS_COUNT / 2],
        inventory[(DECLARED_DETECTION_CLASS_COUNT * 3) / 4],
        inventory[DECLARED_DETECTION_CLASS_COUNT - 1],
    ];

    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);
    store
        .set_local(
            DETECTOR_CLASSES,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::list(spread.iter().copied()),
        )
        .expect("a class list spanning the declared inventory is an ordinary local write");

    assert_eq!(
        construction_allowlist(&store),
        indices_of(&spread),
        "the whole inventory is selectable: every named class reaches the detector, in the order \
         named, with nothing added and nothing dropped — including the inventory's last entry: \
         {spread:?}"
    );
}
