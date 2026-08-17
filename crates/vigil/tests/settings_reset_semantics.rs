//! Reset: what replaces "delete the line".
//!
//! Removing a value from a file means "this surface no longer says anything
//! about this setting" — it clears the record THAT surface authored and nothing
//! else, and whatever is underneath becomes effective again. Two limits keep
//! that from destroying things by accident: only a persistent file surface can
//! clear by absence, and only while it is present and parses, so a missing or
//! half-written file asserts nothing and clears nothing; and startup options
//! never clear by absence, because running the binary by hand without the flags
//! the service passes means "this invocation is not asserting those settings",
//! not "reset them". Alongside absence there is an explicit reset, which drops
//! the setting to whatever is beneath it and says what that is.
//!
//! The cross-author explicit reset is the case here — a local pin reset off a
//! management server's record. The local-surface-to-local-surface reset, and
//! the single-statement shape of the reset write, are covered in
//! `settings_write_durability.rs` and are not repeated.
//!
//! RED: `vigil::settings_store` is skeleton-only (`todo!()` bodies) pending the
//! settings-store implementation.

use std::sync::atomic::{AtomicU64, Ordering};

use vigil::PersistedClock;
use vigil::settings_model::{
    Author, ControlState, Scope, ScopeLevel, ScopeTarget, SettingRecord, SettingValue, Surface,
};
use vigil::settings_store::{HandleRole, SettingsStore, SurfaceChange, SurfaceSnapshot};

const SETTING: &str = "detector_sample_frames";

/// A deterministic, strictly advancing persisted clock. Every record written
/// through the store gets a distinct, ordered stamp with no sleep anywhere and
/// no dependence on how fast the machine runs.
fn advancing_clock() -> PersistedClock {
    let next = AtomicU64::new(1_700_000_000_000);
    PersistedClock::from_millis_source(move || next.fetch_add(1_000, Ordering::SeqCst))
}

fn scope() -> Scope {
    Scope::node("node-a")
}

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: "owner".to_string(),
        site: "home".to_string(),
        node: "node-a".to_string(),
        camera: None,
    }
}

fn open(directory: &tempfile::TempDir) -> SettingsStore {
    SettingsStore::open_with_clock(directory.path(), advancing_clock())
        .expect("open the node-side settings store")
}

fn snapshot(
    surface: Surface,
    present_and_parsing: bool,
    entries: Vec<(String, SettingValue)>,
) -> SurfaceSnapshot {
    SurfaceSnapshot {
        surface,
        scope: scope(),
        present_and_parsing,
        entries,
        declared_defaults: Vec::new(),
    }
}

/// Unfakeable: the record beneath the reset pin belongs to a DIFFERENT author —
/// the management server — so an implementation that treats a reset as "go back
/// to automatic" returns Vigil's own value instead of 4321 and fails, and one
/// that deletes the local row without saying what took over cannot produce a
/// statement naming both the value and where it came from. The pushed record is
/// read back afterwards, so a reset that evicted anything on its way down is
/// caught in the same test.
#[test]
fn an_explicit_reset_drops_to_the_record_beneath_and_names_it() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let hub = SettingsStore::open_hub_role_with_clock(directory.path(), advancing_clock())
        .expect("open the hub-role settings store");
    hub.write_pushed_record(SettingRecord::pushed(
        SETTING,
        scope(),
        SettingValue::Int(4321),
        "fleet baseline for this node",
    ))
    .expect("the hub writes its record into the down-only pushed table");

    let node = hub.node_view();
    assert_eq!(
        node.role(),
        HandleRole::Node,
        "the reset below is an ordinary node-side operation, not a hub one"
    );
    node.set_local(
        SETTING,
        Surface::VigilSettings,
        scope(),
        SettingValue::Int(7),
    )
    .expect("the human pins their own value over the server's");

    let outcome = node
        .reset_local(SETTING, Surface::VigilSettings, &scope())
        .expect("the human resets their own value");

    assert_eq!(outcome.setting, SETTING);
    assert_eq!(
        outcome.dropped_to.requested,
        SettingValue::Int(4321),
        "the setting drops to the record beneath the pin, which is the server's: {outcome:?}"
    );
    assert_eq!(
        outcome.dropped_to.author,
        Author::Pushed,
        "what is beneath here is the management server, not automatic: {outcome:?}"
    );
    assert_eq!(
        outcome.dropped_to.control_state,
        ControlState::SetByManagementServer,
        "the control state the operator now sees names the server as the author: {outcome:?}"
    );
    assert!(
        outcome.statement.contains("4321"),
        "the reset names the value it dropped to: {:?}",
        outcome.statement
    );
    assert!(
        outcome
            .statement
            .to_ascii_lowercase()
            .contains("management server"),
        "the reset names WHERE the value it dropped to came from, not just the number: {:?}",
        outcome.statement
    );

    let stored = node.records(SETTING).expect("read the stored records");
    assert!(
        stored.iter().any(
            |record| record.author == Author::Pushed && record.value == SettingValue::Int(4321)
        ),
        "no local operation evicts a pushed record, including a reset: {stored:?}"
    );
    assert_eq!(
        node.resolve(SETTING, &target())
            .expect("resolve after the reset")
            .requested,
        SettingValue::Int(4321),
        "the setting is never left with a hole after a reset"
    );
}

/// Unfakeable: three records for one setting exist at the moment the config file
/// drops the key — the config file's own, the add-on options', and the server's
/// — and only one of them may disappear. An implementation that clears "the
/// setting" rather than "this surface's record about the setting" loses the
/// add-on record, the pushed record, or both, and the per-surface reads below
/// catch each case separately.
#[test]
fn removing_a_key_from_a_present_parsing_file_surface_clears_only_that_surfaces_record() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let hub = SettingsStore::open_hub_role_with_clock(directory.path(), advancing_clock())
        .expect("open the hub-role settings store");
    hub.write_pushed_record(SettingRecord::pushed(
        SETTING,
        scope(),
        SettingValue::Int(4),
        "fleet baseline for this node",
    ))
    .expect("the hub writes its record into the down-only pushed table");
    let store = hub.node_view();

    store
        .apply_surface_snapshot(&snapshot(
            Surface::ConfigFile,
            true,
            vec![(SETTING.to_string(), SettingValue::Int(3))],
        ))
        .expect("the config file names the key at first start");
    store
        .apply_surface_snapshot(&snapshot(
            Surface::AddonOptions,
            true,
            vec![(SETTING.to_string(), SettingValue::Int(9))],
        ))
        .expect("the add-on options name the key too");

    let changes = store
        .apply_surface_snapshot(&snapshot(Surface::ConfigFile, true, Vec::new()))
        .expect("apply the config file after the key was removed from it");

    assert_eq!(
        changes,
        vec![SurfaceChange::ClearedByAbsence {
            setting: SETTING.to_string(),
            surface: Surface::ConfigFile,
        }],
        "the only thing a removed key clears is the record that surface itself authored: \
         {changes:?}"
    );

    let stored = store.records(SETTING).expect("read the stored records");
    assert!(
        !stored
            .iter()
            .any(|record| record.surface == Surface::ConfigFile && !record.reset),
        "the config file's own record is gone: {stored:?}"
    );
    assert!(
        stored
            .iter()
            .any(|record| record.surface == Surface::AddonOptions
                && record.value == SettingValue::Int(9)),
        "the add-on options' record is untouched by the config file going quiet: {stored:?}"
    );
    assert!(
        stored
            .iter()
            .any(|record| record.author == Author::Pushed && record.value == SettingValue::Int(4)),
        "a node that boots with a minimal file never strips what the server set: {stored:?}"
    );
    assert_eq!(
        store
            .resolve(SETTING, &target())
            .expect("resolve after the key was removed")
            .requested,
        SettingValue::Int(9),
        "whatever is underneath becomes effective again — here the add-on options' record"
    );
}

/// Unfakeable: the surface doing the clearing is the config file, which IS
/// eligible to clear by absence, and the entry list is empty exactly as in the
/// test above — the ONLY difference is that this pass is not present and
/// parsing. An implementation that keys clearing on "the key is absent from the
/// entries" rather than on "the surface is present, parses, and no longer names
/// the key" wipes the record here and fails.
#[test]
fn a_missing_or_unparsable_file_surface_clears_nothing() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    store
        .apply_surface_snapshot(&snapshot(
            Surface::ConfigFile,
            true,
            vec![(SETTING.to_string(), SettingValue::Int(3))],
        ))
        .expect("the config file names the key at first start");

    assert!(
        Surface::ConfigFile.clears_by_absence(),
        "sanity: the config file IS a surface that can clear by absence, so only presence and \
         parsing separate this pass from the one that clears"
    );

    let changes = store
        .apply_surface_snapshot(&snapshot(Surface::ConfigFile, false, Vec::new()))
        .expect("apply a config file that is missing or half-written");

    assert!(
        changes.is_empty(),
        "a surface that is not present and parsing asserts nothing, so it authors nothing and \
         clears nothing: {changes:?}"
    );

    let stored = store.records(SETTING).expect("read the stored records");
    assert!(
        stored
            .iter()
            .any(|record| record.surface == Surface::ConfigFile
                && record.value == SettingValue::Int(3)
                && !record.reset),
        "absence of the file is not absence of the values: {stored:?}"
    );
    assert_eq!(
        store
            .resolve(SETTING, &target())
            .expect("resolve with the file unreadable")
            .requested,
        SettingValue::Int(3),
        "the setting still runs what the file said the last time it could be read"
    );
}

/// Unfakeable: this is byte-for-byte the clearing shape of the config-file test
/// — present, parsing, key absent — with only the surface changed, and the
/// config file's own eligibility is asserted alongside so a blanket "nothing
/// ever clears" implementation cannot pass both files. An operator running the
/// binary by hand without the service's flags keeps their pins.
#[test]
fn startup_options_never_clear_by_absence() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    store
        .apply_surface_snapshot(&snapshot(
            Surface::StartupOptions,
            true,
            vec![(SETTING.to_string(), SettingValue::Int(11))],
        ))
        .expect("the service's command line names the key");

    let changes = store
        .apply_surface_snapshot(&snapshot(Surface::StartupOptions, true, Vec::new()))
        .expect("apply a hand-run command line that passes no flags");

    assert!(
        !changes
            .iter()
            .any(|change| matches!(change, SurfaceChange::ClearedByAbsence { .. })),
        "a hand-run invocation says nothing about these settings; it does not reset them: \
         {changes:?}"
    );

    let stored = store.records(SETTING).expect("read the stored records");
    assert!(
        stored
            .iter()
            .any(|record| record.surface == Surface::StartupOptions
                && record.value == SettingValue::Int(11)
                && !record.reset),
        "the service's pin survives a hand-run: {stored:?}"
    );
    assert_eq!(
        store
            .resolve(SETTING, &target())
            .expect("resolve after the hand-run")
            .requested,
        SettingValue::Int(11),
        "what the service asserted is still what runs"
    );

    assert!(
        !Surface::StartupOptions.clears_by_absence(),
        "startup options are not a persistent file surface and are never eligible to clear by \
         absence"
    );
    assert!(
        Surface::ConfigFile.clears_by_absence(),
        "and a persistent file surface still is — the rule is per surface, not a blanket refusal"
    );
}

/// Unfakeable the same way as `startup_options_never_clear_by_absence` above,
/// one scope down: a hand-run invocation says nothing about ANY camera
/// either, so a camera's own startup-options record must survive it too. The
/// deliberately-kept early return in `runtime::apply_surface` skips a surface
/// only when it names nothing AND cannot clear by absence at all — never per
/// scope — so this is the same invariant, checked at the scope the early
/// return's own condition (`assertions.camera_entries.is_empty()`) names
/// explicitly.
#[test]
fn startup_options_never_clear_a_cameras_own_record_by_absence_either() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);
    let camera_scope = Scope::camera("driveway");

    store
        .apply_camera_surface_snapshot(&SurfaceSnapshot {
            surface: Surface::StartupOptions,
            scope: camera_scope.clone(),
            present_and_parsing: true,
            entries: vec![(SETTING.to_string(), SettingValue::Int(11))],
            declared_defaults: Vec::new(),
        })
        .expect("the service's command line names the camera's own key");

    let changes = store
        .apply_camera_surface_snapshot(&SurfaceSnapshot {
            surface: Surface::StartupOptions,
            scope: camera_scope,
            present_and_parsing: true,
            entries: Vec::new(),
            declared_defaults: Vec::new(),
        })
        .expect("apply a hand-run command line that passes no flags");

    assert!(
        !changes
            .iter()
            .any(|change| matches!(change, SurfaceChange::ClearedByAbsence { .. })),
        "a hand-run invocation says nothing about the driveway either; it does not reset its own \
         value: {changes:?}"
    );

    let stored = store.records(SETTING).expect("read the stored records");
    assert!(
        stored
            .iter()
            .any(|record| record.surface == Surface::StartupOptions
                && record.scope.level == ScopeLevel::Camera
                && record.value == SettingValue::Int(11)
                && !record.reset),
        "the service's pin at the camera's own scope survives a hand-run too: {stored:?}"
    );
}

/// Unfakeable in the same way as
/// `a_missing_or_unparsable_file_surface_clears_nothing` above, one scope
/// down: the entry list is empty exactly as it is there, and the ONLY
/// difference is that this pass is not present and parsing, at a CAMERA'S own
/// scope rather than the deployment's. An implementation that keys camera
/// clearing on "the camera's entries are empty" rather than on "the surface
/// is present, parses, and no longer names this camera" wipes the camera's
/// record here and fails.
#[test]
fn a_missing_or_unparsable_file_surface_clears_nothing_at_a_cameras_scope_either() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);
    let camera_scope = Scope::camera("driveway");

    store
        .apply_camera_surface_snapshot(&SurfaceSnapshot {
            surface: Surface::ConfigFile,
            scope: camera_scope.clone(),
            present_and_parsing: true,
            entries: vec![(SETTING.to_string(), SettingValue::Int(3))],
            declared_defaults: Vec::new(),
        })
        .expect("the camera's row names the key at first start");

    let changes = store
        .apply_camera_surface_snapshot(&SurfaceSnapshot {
            surface: Surface::ConfigFile,
            scope: camera_scope,
            present_and_parsing: false,
            entries: Vec::new(),
            declared_defaults: Vec::new(),
        })
        .expect("apply a config file that is missing or half-written");

    assert!(
        changes.is_empty(),
        "a surface that is not present and parsing asserts nothing about ANY camera, so it clears \
         nothing at a camera's scope either: {changes:?}"
    );

    let stored = store.records(SETTING).expect("read the stored records");
    assert!(
        stored
            .iter()
            .any(|record| record.surface == Surface::ConfigFile
                && record.scope.level == ScopeLevel::Camera
                && record.value == SettingValue::Int(3)
                && !record.reset),
        "absence of the file is not absence of the camera's own value: {stored:?}"
    );
}

/// Unfakeable because three records for one setting exist across two scopes
/// at the moment the config file goes from naming everything to naming
/// nothing — the deployment-wide record, the driveway's own, and the add-on
/// options' record at the deployment scope (untouched, so a fix that resets
/// too broadly is caught too) — and the file's own pass is applied at BOTH
/// scopes with an empty entry list, exactly what an emptied file produces.
/// This is the store-level twin of the end-to-end proof in
/// `settings_empty_surface_resets_everything_end_to_end.rs`; the early return
/// the real fix narrows lives in `runtime.rs`, not here, so this test alone
/// cannot catch that regression — it pins the STORE's own contract that an
/// empty-but-present snapshot at a camera's scope clears exactly like one at
/// the deployment's.
#[test]
fn an_empty_but_present_file_surface_clears_records_at_node_and_camera_scope_alike() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);
    let camera_scope = Scope::camera("driveway");

    store
        .apply_surface_snapshot(&snapshot(
            Surface::ConfigFile,
            true,
            vec![(SETTING.to_string(), SettingValue::Int(3))],
        ))
        .expect("the config file names the deployment-wide key at first start");
    store
        .apply_surface_snapshot(&snapshot(
            Surface::AddonOptions,
            true,
            vec![(SETTING.to_string(), SettingValue::Int(9))],
        ))
        .expect("the add-on options name the deployment-wide key too");
    store
        .apply_camera_surface_snapshot(&SurfaceSnapshot {
            surface: Surface::ConfigFile,
            scope: camera_scope.clone(),
            present_and_parsing: true,
            entries: vec![(SETTING.to_string(), SettingValue::Int(7))],
            declared_defaults: Vec::new(),
        })
        .expect("the config file names the camera's own key too");

    let node_changes = store
        .apply_surface_snapshot(&snapshot(Surface::ConfigFile, true, Vec::new()))
        .expect("apply the emptied config file at the deployment's own scope");
    let camera_changes = store
        .apply_camera_surface_snapshot(&SurfaceSnapshot {
            surface: Surface::ConfigFile,
            scope: camera_scope,
            present_and_parsing: true,
            entries: Vec::new(),
            declared_defaults: Vec::new(),
        })
        .expect("apply the emptied config file at the camera's own scope");

    assert_eq!(
        node_changes,
        vec![SurfaceChange::ClearedByAbsence {
            setting: SETTING.to_string(),
            surface: Surface::ConfigFile,
        }],
        "the deployment-wide record the config file authored must clear: {node_changes:?}"
    );
    assert_eq!(
        camera_changes,
        vec![SurfaceChange::ClearedByAbsence {
            setting: SETTING.to_string(),
            surface: Surface::ConfigFile,
        }],
        "the camera-scoped record the config file authored must clear too, the same pass one \
         scope down: {camera_changes:?}"
    );

    let stored = store.records(SETTING).expect("read the stored records");
    assert!(
        !stored
            .iter()
            .any(|record| record.surface == Surface::ConfigFile && !record.reset),
        "no live config-file record survives at either scope: {stored:?}"
    );
    assert!(
        stored
            .iter()
            .any(|record| record.surface == Surface::AddonOptions
                && record.value == SettingValue::Int(9)
                && !record.reset),
        "an emptied config file must never touch a DIFFERENT surface's record: {stored:?}"
    );
    assert_eq!(
        store
            .resolve(SETTING, &target())
            .expect("resolve after the config file emptied")
            .requested,
        SettingValue::Int(9),
        "whatever is underneath becomes effective again — here the add-on options' record"
    );
}
