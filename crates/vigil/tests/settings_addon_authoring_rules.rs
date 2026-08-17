//! What the Home Assistant add-on options surface is allowed to author.
//!
//! The Supervisor cannot tell an untouched declared default from a value typed
//! equal to it, and the web interface posts the entire rendered form on save.
//! So the rules below are the only thing standing between a fresh install and a
//! screenful of spurious pins. Every assertion is on the change the store
//! produced and on the resolved state afterwards — no timing, no sleeping.

use vigil::settings_model::{Author, ControlState, Scope, ScopeTarget, SettingValue, Surface};
use vigil::settings_store::{SettingsStore, SurfaceChange, SurfaceSnapshot};

/// The integer behavior key these tests drive.
const INT_SETTING: &str = "detector_stationary_interval_secs";

/// A schema-optional key with no declared default: absent from the file
/// entirely when nobody set it.
const UNSET_OPTIONAL_KEY: &str = "detector_model_path";

fn node_target() -> ScopeTarget {
    ScopeTarget {
        tenant: "tenant-a".to_string(),
        site: "site-a".to_string(),
        node: "node-a".to_string(),
        camera: None,
    }
}

fn open_store(directory: &tempfile::TempDir) -> SettingsStore {
    SettingsStore::open(directory.path()).expect("node-side settings store")
}

fn addon_snapshot(
    entries: Vec<(String, SettingValue)>,
    declared_defaults: Vec<(String, SettingValue)>,
) -> SurfaceSnapshot {
    SurfaceSnapshot {
        surface: Surface::AddonOptions,
        scope: Scope::node("node-a"),
        present_and_parsing: true,
        entries,
        declared_defaults,
    }
}

fn change_for<'a>(changes: &'a [SurfaceChange], setting: &str) -> Option<&'a SurfaceChange> {
    changes.iter().find(|change| match change {
        SurfaceChange::Authored(record) => record.setting == setting,
        SurfaceChange::ClearedByAbsence { setting: name, .. } => name == setting,
        SurfaceChange::Unchanged { setting: name } => name == setting,
        SurfaceChange::AuthorsNothing { setting: name, .. } => name == setting,
        SurfaceChange::Refused { setting: name, .. } => name == setting,
    })
}

#[test]
fn an_add_on_value_equal_to_the_declared_default_authors_nothing() {
    // Unfakeable because the identical posted value is checked against two
    // different declared defaults in the same test: equal to the default it
    // authors nothing, and with the declared default moved away from it the
    // very same posted value authors a record. A rule that just ignored the
    // add-on surface would pass the first half and fail the second.
    let directory = tempfile::tempdir().expect("temp directory");
    let store = open_store(&directory);

    let changes = store
        .apply_surface_snapshot(&addon_snapshot(
            vec![(INT_SETTING.to_string(), SettingValue::Int(30))],
            vec![(INT_SETTING.to_string(), SettingValue::Int(30))],
        ))
        .expect("applying the add-on surface must not error");

    match change_for(&changes, INT_SETTING) {
        Some(SurfaceChange::AuthorsNothing { why, .. }) => {
            assert!(
                why.to_lowercase().contains("default"),
                "the reason must name the declared default, got {why}"
            );
        }
        other => panic!("a value equal to the declared default authors nothing, got {other:?}"),
    }
    assert!(
        store
            .records(INT_SETTING)
            .expect("stored records")
            .iter()
            .all(|record| record.surface != Surface::AddonOptions),
        "nothing may be stored for the add-on surface"
    );

    // Same posted value, declared default moved: now it is a human saying
    // something.
    let elsewhere = tempfile::tempdir().expect("temp directory");
    let store = open_store(&elsewhere);
    let changes = store
        .apply_surface_snapshot(&addon_snapshot(
            vec![(INT_SETTING.to_string(), SettingValue::Int(30))],
            vec![(INT_SETTING.to_string(), SettingValue::Int(45))],
        ))
        .expect("applying the add-on surface must not error");
    assert!(
        matches!(
            change_for(&changes, INT_SETTING),
            Some(SurfaceChange::Authored(_))
        ),
        "the same value against a different declared default must author, got {changes:?}"
    );
}

#[test]
fn a_posted_key_whose_value_is_null_or_empty_authors_nothing() {
    // Unfakeable because it covers both empty shapes the rendered form
    // produces — an empty text field and an empty list — for keys that have NO
    // declared default, where the declared-default rule cannot save them. It
    // also checks a genuinely nonempty value on the same key authors, so a
    // blanket "add-on authors nothing" implementation fails.
    let directory = tempfile::tempdir().expect("temp directory");
    let store = open_store(&directory);

    assert!(
        SettingValue::text("").is_null_or_empty(),
        "an empty text field is null-or-empty"
    );
    assert!(
        SettingValue::List(Vec::new()).is_null_or_empty(),
        "an empty list field is null-or-empty"
    );
    assert!(
        !SettingValue::text("/share/vigil/detector.mpk").is_null_or_empty(),
        "a filled field is not null-or-empty"
    );

    let changes = store
        .apply_surface_snapshot(&addon_snapshot(
            vec![
                (UNSET_OPTIONAL_KEY.to_string(), SettingValue::text("")),
                (
                    "recognition_covered_classes".to_string(),
                    SettingValue::List(Vec::new()),
                ),
            ],
            Vec::new(),
        ))
        .expect("applying the add-on surface must not error");

    for setting in [UNSET_OPTIONAL_KEY, "recognition_covered_classes"] {
        match change_for(&changes, setting) {
            Some(SurfaceChange::AuthorsNothing { why, .. }) => {
                let why = why.to_lowercase();
                assert!(
                    why.contains("empty") || why.contains("null"),
                    "the reason must say the posted key was empty, got {why}"
                );
            }
            other => panic!("`{setting}` posted empty must author nothing, got {other:?}"),
        }
    }
    assert!(
        store
            .records(UNSET_OPTIONAL_KEY)
            .expect("stored records")
            .is_empty(),
        "an empty form field pins nothing"
    );

    let elsewhere = tempfile::tempdir().expect("temp directory");
    let store = open_store(&elsewhere);
    let changes = store
        .apply_surface_snapshot(&addon_snapshot(
            vec![(
                UNSET_OPTIONAL_KEY.to_string(),
                SettingValue::text("/share/vigil/detector.mpk"),
            )],
            Vec::new(),
        ))
        .expect("applying the add-on surface must not error");
    assert!(
        matches!(
            change_for(&changes, UNSET_OPTIONAL_KEY),
            Some(SurfaceChange::Authored(_))
        ),
        "a filled field on the same key must author, got {changes:?}"
    );
}

#[test]
fn an_unset_schema_optional_key_is_absent_from_the_options_file_and_resolves_automatic() {
    // Unfakeable because absence is asserted as absence: the snapshot the
    // add-on surface presents does not name the key at all — not null, not an
    // empty string — and the resolution afterwards has to come back Automatic
    // with no add-on record stored. An implementation that materialized a
    // default for every schema key would resolve to Set-by-you and fail.
    let directory = tempfile::tempdir().expect("temp directory");
    let store = open_store(&directory);

    let snapshot = addon_snapshot(
        vec![(INT_SETTING.to_string(), SettingValue::Int(45))],
        Vec::new(),
    );
    assert!(
        !snapshot
            .entries
            .iter()
            .any(|(key, _)| key == UNSET_OPTIONAL_KEY),
        "the unset schema-optional key must be absent from the file entirely"
    );

    let changes = store
        .apply_surface_snapshot(&snapshot)
        .expect("applying the add-on surface must not error");
    assert!(
        change_for(&changes, UNSET_OPTIONAL_KEY).is_none(),
        "an absent key produces no change of any kind, got {changes:?}"
    );
    assert!(
        store
            .records(UNSET_OPTIONAL_KEY)
            .expect("stored records")
            .is_empty(),
        "an absent key stores nothing"
    );

    let effective = store
        .resolve(UNSET_OPTIONAL_KEY, &node_target())
        .expect("an untouched setting still resolves");
    assert_eq!(effective.control_state, ControlState::Automatic);
    assert_eq!(effective.author, Author::Automatic);
    assert_eq!(effective.surface, Surface::Automatic);
}

#[test]
fn a_value_diverging_from_the_declared_default_authors_a_local_explicit_record() {
    // Unfakeable because the authored record is inspected field by field —
    // author, surface, scope, and value — and then read back through the
    // resolution the operator surface renders. Authoring a record that does not
    // attribute itself to the add-on options surface, or that does not become
    // effective, fails.
    let directory = tempfile::tempdir().expect("temp directory");
    let store = open_store(&directory);

    let changes = store
        .apply_surface_snapshot(&addon_snapshot(
            vec![(INT_SETTING.to_string(), SettingValue::Int(90))],
            vec![(INT_SETTING.to_string(), SettingValue::Int(30))],
        ))
        .expect("applying the add-on surface must not error");

    let authored = match change_for(&changes, INT_SETTING) {
        Some(SurfaceChange::Authored(record)) => record,
        other => panic!("a diverging value must author a record, got {other:?}"),
    };
    assert_eq!(authored.setting, INT_SETTING);
    assert_eq!(authored.author, Author::LocalExplicit);
    assert_eq!(authored.surface, Surface::AddonOptions);
    assert_eq!(authored.scope, Scope::node("node-a"));
    assert_eq!(authored.value, SettingValue::Int(90));
    assert!(
        !authored.reason.trim().is_empty(),
        "a record with no reason is a defect, not a blank field"
    );

    let effective = store
        .resolve(INT_SETTING, &node_target())
        .expect("the authored value resolves");
    assert_eq!(effective.requested, SettingValue::Int(90));
    assert_eq!(effective.control_state, ControlState::SetByYou);
    assert_eq!(effective.surface, Surface::AddonOptions);
}
