//! The store is the source of truth; the merged configuration is not.
//!
//! Input surfaces are AUTHORS that write into the store, and the runtime
//! resolves from the store. That direction is the whole authority model: a value
//! that reached the runtime by being merged out of a file, alongside or instead
//! of the record the store holds, has no author, no surface, no scope and no
//! reason attached to it, and nothing above it — a management server's record, a
//! pin made through `vigil settings` — can outrank something that never entered
//! the ranking.
//!
//! Two legs prove that, because the detection class list alone cannot: the class
//! list is not a configuration-file key at all, so a `vigil.toml` naming it is
//! inert text and a test built on it would prove nothing. So the file leg is
//! carried by the stationary-scan interval, which the configuration file
//! genuinely does set today, and the class-list leg pins both that the store's
//! record is what resolves and that the merged configuration has no way to carry
//! that setting in the first place.
//!
//! RED: `vigil::settings_store` is skeleton-only (`todo!()` bodies) pending the
//! settings-store implementation.

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use vigil::PersistedClock;
use vigil::settings_model::{Author, ControlState, Scope, ScopeTarget, SettingValue, Surface};
use vigil::settings_store::SettingsStore;

const DETECTOR_CLASSES: &str = "detector_classes";
/// The setting the configuration file really does set, so the disagreeing
/// merged-config value in this fixture is live rather than inert text.
const STATIONARY_INTERVAL: &str = "detector_stationary_interval_secs";
const NODE: &str = "node-a";

/// Distinctive multi-digit values, so an assertion can never be satisfied by a
/// digit that happened to appear somewhere else in a rendered answer.
const STORED_INTERVAL_SECS: i64 = 47;
const CONFIG_FILE_INTERVAL_SECS: i64 = 11;

/// A deterministic, strictly advancing persisted clock: distinct ordered stamps
/// with no sleep and no dependence on machine speed.
fn advancing_clock() -> PersistedClock {
    let next = AtomicU64::new(1_700_000_000_000);
    PersistedClock::from_millis_source(move || next.fetch_add(1_000, Ordering::SeqCst))
}

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: "owner".to_string(),
        site: "home".to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

/// Unfakeable: a real configuration file naming a DIFFERENT stationary-scan
/// interval is left on disk in the deployment directory the store is opened
/// against, and the configuration parser is asked, through the production
/// dispatch, whether that key genuinely sets the field — so the disagreeing
/// value is a live merged-config value sitting exactly where a merged-config
/// reader would find it, not a line nothing reads. Any resolution that consults
/// the merged configuration returns 11 there. The class list is then pinned the
/// other way round: the production dispatch is asked whether the configuration
/// file carries that key at all, so the leg that names the class list states a
/// checked fact rather than assuming one. The stored records are read afterwards
/// too, so a value that leaked out of the file into the store without going
/// through a surface snapshot is caught even if resolution happened to answer
/// correctly.
#[test]
fn detector_class_list_resolves_from_the_store_not_the_merged_config() {
    let directory = tempfile::tempdir().expect("temporary data directory");

    let config_path = directory.path().join("vigil.toml");
    fs::write(
        &config_path,
        format!("{STATIONARY_INTERVAL} = {CONFIG_FILE_INTERVAL_SECS}\n"),
    )
    .expect("write the configuration file");
    assert!(
        vigil::config_file_recognizes_setting(
            STATIONARY_INTERVAL,
            &CONFIG_FILE_INTERVAL_SECS.to_string()
        ),
        "sanity: the configuration file really does carry this setting, so the file written above \
         is a disagreeing merged-config value rather than a line nothing reads"
    );
    assert!(
        !vigil::config_file_recognizes_setting(DETECTOR_CLASSES, "[\"cat\"]"),
        "the detection class list is not a configuration-file key at all, so the merged \
         configuration has no way to carry it: a class list can only reach the runtime by being \
         authored into the store through a surface"
    );

    let store = SettingsStore::open_with_clock(directory.path(), advancing_clock())
        .expect("open the node-side settings store");
    store
        .set_local(
            DETECTOR_CLASSES,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::list(["dog"]),
        )
        .expect("the owner names a class list through `vigil settings`");
    store
        .set_local(
            STATIONARY_INTERVAL,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::Int(STORED_INTERVAL_SECS),
        )
        .expect("the owner names a stationary-scan interval through `vigil settings`");

    // The file leg: the store and a live configuration file disagree about the
    // same setting, and the store wins.
    let interval = store
        .resolve(STATIONARY_INTERVAL, &target())
        .expect("resolve the stationary-scan interval");
    assert_eq!(
        interval.requested,
        SettingValue::Int(STORED_INTERVAL_SECS),
        "the record in the store is what runs, even with a configuration file on disk saying \
         otherwise: {interval:?}"
    );
    assert_ne!(
        interval.requested,
        SettingValue::Int(CONFIG_FILE_INTERVAL_SECS),
        "the merged configuration never becomes the effective value on its own: {interval:?}"
    );
    assert_eq!(
        interval.surface,
        Surface::VigilSettings,
        "attributed to the surface it was actually authored through, not to the file: {interval:?}"
    );
    let stored_interval_records = store
        .records(STATIONARY_INTERVAL)
        .expect("read the stored records for the stationary-scan interval");
    assert!(
        !stored_interval_records
            .iter()
            .any(|record| record.value == SettingValue::Int(CONFIG_FILE_INTERVAL_SECS)),
        "a file's contents become a record by being applied as that surface's snapshot, never by \
         being read during resolution: {stored_interval_records:?}"
    );

    // The class-list leg: the store's record is what resolves, carrying the
    // author, surface and control state the store recorded.
    let effective = store
        .resolve(DETECTOR_CLASSES, &target())
        .expect("resolve the detector class list");
    assert_eq!(
        effective.requested,
        SettingValue::list(["dog"]),
        "the record in the store is what runs: {effective:?}"
    );
    assert_eq!(
        effective.author,
        Author::LocalExplicit,
        "and the effective value carries the author the store recorded: {effective:?}"
    );
    assert_eq!(
        effective.surface,
        Surface::VigilSettings,
        "attributed to the surface it was actually authored through, not to the file: \
         {effective:?}"
    );
    assert_eq!(effective.control_state, ControlState::SetByYou);

    let stored = store
        .records(DETECTOR_CLASSES)
        .expect("read the stored records");
    assert!(
        !stored
            .iter()
            .any(|record| record.value == SettingValue::list(["cat"])),
        "nothing outside a surface snapshot may become a record: {stored:?}"
    );
}
