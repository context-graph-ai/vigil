//! Records coexist per surface; they never overwrite each other.
//!
//! The local-explicit rank has four surfaces — the add-on options, the config
//! file, startup options, and a `vigil settings` change made against this
//! deployment — and each keeps its own record. If they shared one, a config
//! file would silently revert a `vigil settings` change at every restart,
//! because the file would look like the author changing their mind on every
//! boot. Between local surfaces the winner is the surface that most recently
//! authored — the same human changing their own mind, the one place recency is
//! the right arbiter — and when two author in the same first start the
//! deterministic order is startup options, then the `vigil settings` change,
//! then the file surface. A surface that says what it said last time authors
//! nothing and re-wins nothing.
//!
//! RED: `vigil::settings_store` is skeleton-only (`todo!()` bodies) pending the
//! settings-store implementation.

use vigil::settings_model::{
    Author, HeldReason, Scope, ScopeTarget, SettingRecord, SettingValue, Surface,
};
use vigil::settings_store::{SettingsStore, SurfaceChange, SurfaceSnapshot};

const SETTING: &str = "detector_stationary_interval_secs";

/// One instant, shared by every record in the first-start tie: the tie is
/// broken by surface order, never by an ordering the clock happened to give.
const TIE_MS: i64 = 1_700_000_000_000;

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

fn local_record(surface: Surface, value: i64, written_at_ms: i64) -> SettingRecord {
    SettingRecord {
        setting: SETTING.to_string(),
        author: Author::LocalExplicit,
        surface,
        scope: scope(),
        value: SettingValue::Int(value),
        reason: format!("a human authored this through {surface:?}"),
        written_at_ms,
        domain_generation: 0,
        reset: false,
    }
}

fn config_file_snapshot(value: i64) -> SurfaceSnapshot {
    SurfaceSnapshot {
        surface: Surface::ConfigFile,
        scope: scope(),
        present_and_parsing: true,
        entries: vec![(SETTING.to_string(), SettingValue::Int(value))],
        declared_defaults: Vec::new(),
    }
}

fn open(directory: &tempfile::TempDir) -> SettingsStore {
    SettingsStore::open(directory.path()).expect("open the node-side settings store")
}

/// Unfakeable: all four surfaces write the SAME setting at the SAME scope with
/// four different values, so a store keyed on (setting, scope) alone — or on
/// (setting, author, scope) — physically cannot hold four rows and fails this
/// count. Reading the values back per surface proves none was overwritten by a
/// later one.
#[test]
fn the_four_local_surfaces_keep_their_own_records() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    let authored = [
        (Surface::StartupOptions, 3),
        (Surface::VigilSettings, 5),
        (Surface::ConfigFile, 7),
        (Surface::AddonOptions, 11),
    ];
    for (index, (surface, value)) in authored.iter().enumerate() {
        store
            .write_record(local_record(*surface, *value, TIE_MS + index as i64))
            .unwrap_or_else(|error| panic!("write the {surface:?} record: {error:?}"));
    }

    let stored = store.records(SETTING).expect("read every stored record");
    assert_eq!(
        stored.len(),
        authored.len(),
        "each local surface keeps its own record; none replaces another: {stored:?}"
    );
    for (surface, value) in authored {
        let matching: Vec<&SettingRecord> = stored
            .iter()
            .filter(|record| record.surface == surface)
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "exactly one record per surface for this setting and scope: {matching:?}"
        );
        assert_eq!(
            matching[0].value,
            SettingValue::Int(value),
            "the {surface:?} record still carries what that surface said"
        );
    }
}

/// Unfakeable: the config-file snapshot re-applied after the `vigil settings`
/// change asserts the OLD value, which is exactly the shape of a restart on a
/// machine whose file was never edited. A shared-record implementation reverts
/// to 3 here; a per-surface one recognizes the file as saying what it already
/// said, authors nothing, and leaves the newer `vigil settings` record
/// effective.
#[test]
fn a_config_file_value_never_reverts_a_vigil_settings_change_at_restart() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    let first_start = store
        .apply_surface_snapshot(&config_file_snapshot(3))
        .expect("apply the config file at first start");
    assert!(
        first_start
            .iter()
            .any(|change| matches!(change, SurfaceChange::Authored(record) if record.surface == Surface::ConfigFile)),
        "the config file authors its own record on first start: {first_start:?}"
    );

    store
        .set_local(
            SETTING,
            Surface::VigilSettings,
            scope(),
            SettingValue::Int(5),
        )
        .expect("the human changes their mind through `vigil settings`");

    let restart = store
        .apply_surface_snapshot(&config_file_snapshot(3))
        .expect("apply the unchanged config file at restart");
    assert!(
        restart.iter().any(|change| matches!(
            change,
            SurfaceChange::Unchanged { setting } if setting == SETTING
        )),
        "the file says what it last said, so it authors nothing at restart: {restart:?}"
    );

    let effective = store.resolve(SETTING, &target()).expect("resolve");
    assert_eq!(
        effective.requested,
        SettingValue::Int(5),
        "the `vigil settings` change is still what runs after the restart: {effective:?}"
    );
    assert_eq!(effective.surface, Surface::VigilSettings);
}

/// Unfakeable: the assertion compares the STORED records before and after the
/// second application, including their written timestamps, so a re-authored
/// record with identical content — which would re-win the surface contest at
/// every boot — is caught even though the effective value never changes.
#[test]
fn an_unchanged_surface_authors_nothing_and_re_wins_nothing_at_restart() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    store
        .apply_surface_snapshot(&config_file_snapshot(7))
        .expect("apply the config file at first start");
    let after_first_start = store.records(SETTING).expect("read the stored records");
    assert_eq!(
        after_first_start.len(),
        1,
        "the file authored exactly one record: {after_first_start:?}"
    );

    let restart = store
        .apply_surface_snapshot(&config_file_snapshot(7))
        .expect("apply the unchanged config file at restart");
    assert_eq!(
        restart,
        vec![SurfaceChange::Unchanged {
            setting: SETTING.to_string()
        }],
        "an unchanged surface is silent: it re-asserts nothing"
    );

    let after_restart = store.records(SETTING).expect("read the stored records");
    assert_eq!(
        after_restart, after_first_start,
        "no new record was stored and no existing record was re-stamped: {after_restart:?}"
    );
}

/// Unfakeable: the `vigil settings` change is the NEWER record, and the
/// first-start tie order would have preferred startup options. An
/// implementation that applies the deterministic surface order unconditionally
/// — rather than only when two surfaces authored at the same first start —
/// returns 3 here.
#[test]
fn between_local_surfaces_the_most_recent_author_wins() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    store
        .write_record(local_record(Surface::StartupOptions, 3, TIE_MS))
        .expect("the service's command line said 3");
    store
        .write_record(local_record(Surface::VigilSettings, 5, TIE_MS + 60_000))
        .expect("the same human said 5 later through `vigil settings`");

    let effective = store.resolve(SETTING, &target()).expect("resolve");
    assert_eq!(effective.requested, SettingValue::Int(5));
    assert_eq!(
        effective.surface,
        Surface::VigilSettings,
        "the surface that most recently authored is what runs: {effective:?}"
    );
}

/// Unfakeable: every record carries the IDENTICAL timestamp, so no clock
/// comparison can separate them — the order has to come from the declared
/// surface precedence. The winner is written first in one store and last in
/// another, so an implementation that falls back to insertion order fails, and
/// each rung is checked with the rung above it removed so passing requires the
/// whole order rather than one lucky arm.
#[test]
fn a_first_start_tie_resolves_startup_options_then_vigil_settings_then_the_file_surface() {
    let all_three = tempfile::tempdir().expect("temporary data directory");
    let store = open(&all_three);
    // Winner written FIRST here.
    store
        .write_record(local_record(Surface::StartupOptions, 3, TIE_MS))
        .expect("startup options authored at first start");
    store
        .write_record(local_record(Surface::VigilSettings, 5, TIE_MS))
        .expect("`vigil settings` authored at the same first start");
    store
        .write_record(local_record(Surface::ConfigFile, 7, TIE_MS))
        .expect("the config file authored at the same first start");
    let effective = store.resolve(SETTING, &target()).expect("resolve");
    assert_eq!(
        effective.surface,
        Surface::StartupOptions,
        "startup options resolve first in a first-start tie: {effective:?}"
    );
    assert_eq!(effective.requested, SettingValue::Int(3));

    let without_startup_options = tempfile::tempdir().expect("temporary data directory");
    let store = open(&without_startup_options);
    // Winner written LAST here.
    store
        .write_record(local_record(Surface::ConfigFile, 7, TIE_MS))
        .expect("the config file authored at first start");
    store
        .write_record(local_record(Surface::VigilSettings, 5, TIE_MS))
        .expect("`vigil settings` authored at the same first start");
    let effective = store.resolve(SETTING, &target()).expect("resolve");
    assert_eq!(
        effective.surface,
        Surface::VigilSettings,
        "with no startup option present, the `vigil settings` change resolves next: {effective:?}"
    );
    assert_eq!(effective.requested, SettingValue::Int(5));

    let file_surface_only = tempfile::tempdir().expect("temporary data directory");
    let store = open(&file_surface_only);
    store
        .write_record(local_record(Surface::AddonOptions, 11, TIE_MS))
        .expect("the add-on options authored at first start");
    let effective = store.resolve(SETTING, &target()).expect("resolve");
    assert_eq!(
        effective.surface,
        Surface::AddonOptions,
        "the file surface resolves last, and is what runs when it is alone: {effective:?}"
    );
    assert_eq!(effective.requested, SettingValue::Int(11));
}

/// Unfakeable: the outranked record must appear on the FACE of the operator
/// surface with a statement naming both values — "your service's command line
/// says 3 frames; you set 5 later with `vigil settings`; 5 is running." A
/// disagreement between two things the same person wrote is a thing to show,
/// so an implementation that silently drops the loser, or files it only in
/// history, fails here even though the effective value is right.
#[test]
fn one_local_surface_being_outranked_by_another_is_stated_on_the_operator_surface() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    store
        .write_record(local_record(Surface::StartupOptions, 3, TIE_MS))
        .expect("the service's command line said 3");
    store
        .write_record(local_record(Surface::VigilSettings, 5, TIE_MS + 60_000))
        .expect("the same human said 5 later through `vigil settings`");

    let effective = store.resolve(SETTING, &target()).expect("resolve");
    let outranked = effective
        .held
        .iter()
        .find(|held| held.record.surface == Surface::StartupOptions)
        .unwrap_or_else(|| {
            panic!("the outranked startup-options record stays stored and reported: {effective:?}")
        });

    assert_eq!(
        outranked.reason,
        HeldReason::OutrankedByLocalSurface {
            by_surface: Surface::VigilSettings,
        },
        "the held reason names the local surface that outranked it"
    );
    assert!(
        outranked.statement.contains('3') && outranked.statement.contains('5'),
        "the statement names what each surface said and which one is running: {:?}",
        outranked.statement
    );
}

#[test]
fn a_held_statement_never_claims_a_stored_value_is_running() {
    // Unfakeable because the store is the only thing this path consults and
    // nothing here ever started a process: any sentence asserting that a value
    // IS running is asserting a runtime fact from a store read. The shipped
    // wording ended "and 5 is running" for a deployment running nothing at
    // all, and an operator read it beside a running field naming a different
    // backend.
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    store
        .write_record(local_record(Surface::StartupOptions, 3, TIE_MS))
        .expect("the service's command line said 3");
    store
        .write_record(local_record(Surface::VigilSettings, 5, TIE_MS + 60_000))
        .expect("the same human said 5 later through `vigil settings`");

    let effective = store.resolve(SETTING, &target()).expect("resolve");
    let outranked = effective
        .held
        .iter()
        .find(|held| held.record.surface == Surface::StartupOptions)
        .expect("the outranked startup-options record stays stored and reported");

    assert!(
        !outranked.statement.contains("is running"),
        "the store knows what it holds, never what a process is executing: {:?}",
        outranked.statement
    );
    assert!(
        outranked.statement.contains('3') && outranked.statement.contains('5'),
        "both values stay on the statement — this narrows the claim, not the content: {:?}",
        outranked.statement
    );
}
