//! Rank is a rank of AUTHORS, never a race of clocks.
//!
//! Three authors exist — Vigil itself, your management server, you at this
//! deployment — and between two different authors time is irrelevant: a server
//! push from an hour ago does not lose to an automatic adjustment from a
//! minute ago, and a local pin from last year does not lose to a push that
//! arrived this morning. Within one author, the most specific scope naming the
//! target wins, and that happens BEFORE authors are compared.
//!
//! Every record below is written with a deliberately INVERTED `written_at_ms`:
//! the lower-ranked record is always the newer one. An implementation that
//! resolves by recency — the keep-latest arbiter this model exists to replace —
//! fails every test in this file, and cannot be made to pass by adjusting a
//! tolerance or a margin.
//!
//! RED: `vigil::settings_store` is skeleton-only (`todo!()` bodies) pending the
//! settings-store implementation.

use vigil::settings_model::{
    Author, ControlState, HeldReason, Scope, ScopeLevel, ScopeTarget, SettingRecord, SettingValue,
    Surface,
};
use vigil::settings_store::SettingsStore;

const SETTING: &str = "detector_stationary_interval_secs";

/// Deliberately inverted against the ranking: every lower-ranked record below
/// is stamped with this NEWER time, every higher-ranked one with `OLD_MS`.
const NEW_MS: i64 = 9_000_000_000;
const OLD_MS: i64 = 1_000;

fn record(
    author: Author,
    surface: Surface,
    scope: Scope,
    value: SettingValue,
    written_at_ms: i64,
) -> SettingRecord {
    SettingRecord {
        setting: SETTING.to_string(),
        author,
        surface,
        scope,
        value,
        reason: format!("test record authored through {surface:?}"),
        written_at_ms,
        domain_generation: 0,
        reset: false,
    }
}

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: "owner".to_string(),
        site: "home".to_string(),
        node: "node-a".to_string(),
        camera: Some("driveway".to_string()),
    }
}

/// The hub-role handle is the harness stand-in for a hub push: it writes the
/// down-only pushed table in exactly the shape hub sync would apply it. That
/// the ORDINARY node-side handle cannot do this is proven separately in
/// `settings_pushed_table_boundary.rs`; these tests are about what a pushed
/// record does once it is legitimately there.
fn hub_store(directory: &tempfile::TempDir) -> SettingsStore {
    SettingsStore::open_hub_role(directory.path()).expect("open the hub-role settings store")
}

/// Unfakeable: the local pin is stamped nine million seconds OLDER than the
/// push, so any recency-based resolution returns the pushed value. Only a
/// resolution that compares authors first can return the local record here.
#[test]
fn an_older_local_pin_outranks_a_newer_pushed_record() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = hub_store(&directory);
    let scope = Scope::node("node-a");

    store
        .write_record(record(
            Author::LocalExplicit,
            Surface::VigilSettings,
            scope.clone(),
            SettingValue::Int(3),
            OLD_MS,
        ))
        .expect("write the local pin");
    store
        .write_pushed_record(record(
            Author::Pushed,
            Surface::ManagementServer,
            scope.clone(),
            SettingValue::Int(5),
            NEW_MS,
        ))
        .expect("write the pushed record through the hub-role handle");

    let effective = store.resolve(SETTING, &target()).expect("resolve");

    assert_eq!(effective.author, Author::LocalExplicit);
    assert_eq!(effective.surface, Surface::VigilSettings);
    assert_eq!(effective.requested, SettingValue::Int(3));
    assert_eq!(effective.control_state, ControlState::SetByYou);
    assert!(
        effective.held.iter().any(|held| {
            held.record.author == Author::Pushed
                && matches!(held.reason, HeldReason::Shadowed { .. })
        }),
        "the pushed record stays stored and is reported as shadowed, never evicted: {:?}",
        effective.held
    );
}

/// Unfakeable: the automatic adjustment is the NEWEST record present, and
/// automatic is exactly the writer whose routine tuning pass a keep-latest
/// table would let erase a managed value. A resolution that lets Vigil's own
/// tuning displace a hub's value fails here.
#[test]
fn an_older_pushed_record_outranks_a_newer_automatic_adjustment() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = hub_store(&directory);
    let scope = Scope::node("node-a");

    store
        .write_pushed_record(record(
            Author::Pushed,
            Surface::ManagementServer,
            scope.clone(),
            SettingValue::Int(5),
            OLD_MS,
        ))
        .expect("write the pushed record through the hub-role handle");
    store
        .write_record(record(
            Author::Automatic,
            Surface::Automatic,
            scope.clone(),
            SettingValue::Int(30),
            NEW_MS,
        ))
        .expect("write the automatic adjustment");

    let effective = store.resolve(SETTING, &target()).expect("resolve");

    assert_eq!(effective.author, Author::Pushed);
    assert_eq!(effective.surface, Surface::ManagementServer);
    assert_eq!(effective.requested, SettingValue::Int(5));
    assert_eq!(effective.control_state, ControlState::SetByManagementServer);
    assert!(
        effective.held.iter().any(|held| {
            held.record.author == Author::Automatic
                && matches!(held.reason, HeldReason::Shadowed { .. })
        }),
        "Vigil's own value stays stored underneath and is reported as shadowed: {:?}",
        effective.held
    );
}

/// Unfakeable on two counts at once. The local pin is both OLDER and LESS
/// SPECIFIC than the push — site-wide against per-camera — so an
/// implementation that resolves by recency fails, and so does one that lets
/// scope specificity beat author rank. Specificity is decided WITHIN an author;
/// it never promotes a lower author over a higher one. The shadowed push must
/// also be visible on the face of the operator surface, because a shadowed
/// value is the single most likely source of confusion in this model.
#[test]
fn an_older_site_wide_pin_shadows_a_newer_per_camera_push_visibly() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = hub_store(&directory);

    store
        .write_record(record(
            Author::LocalExplicit,
            Surface::VigilSettings,
            Scope::site("home"),
            SettingValue::Int(3),
            OLD_MS,
        ))
        .expect("write the site-wide local pin");
    store
        .write_pushed_record(record(
            Author::Pushed,
            Surface::ManagementServer,
            Scope::camera("driveway"),
            SettingValue::Int(5),
            NEW_MS,
        ))
        .expect("write the per-camera push through the hub-role handle");

    let effective = store.resolve(SETTING, &target()).expect("resolve");

    assert_eq!(effective.author, Author::LocalExplicit);
    assert_eq!(effective.requested, SettingValue::Int(3));
    assert_eq!(effective.scope.level, ScopeLevel::Site);
    assert!(
        effective.inherited,
        "the driveway camera is running a site-wide value rather than its own override, and the \
         operator surface says which: {effective:?}"
    );

    let shadowed = effective
        .held
        .iter()
        .find(|held| held.record.author == Author::Pushed)
        .unwrap_or_else(|| {
            panic!("the pushed record must remain stored and reported: {effective:?}")
        });
    assert_eq!(
        shadowed.reason,
        HeldReason::Shadowed {
            by_author: Author::LocalExplicit,
            by_surface: Surface::VigilSettings,
        },
        "the held reason names who is shadowing it and through which surface"
    );
    assert!(
        shadowed.statement.contains('5') && shadowed.statement.contains('3'),
        "the statement rendered on the operator surface names what the server set and what is \
         actually running: {:?}",
        shadowed.statement
    );
}

/// Unfakeable: within the local-explicit author the camera-scoped record is the
/// OLDER one, so recency would pick the node-scoped value; and a pushed record
/// sits at the same camera scope, so an implementation that picks the most
/// specific record across ALL authors before ranking them would return the
/// push. Only "most specific within the winning author, author rank first"
/// returns the local camera-scoped value.
#[test]
fn the_most_specific_scope_wins_within_one_author_before_authors_are_compared() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = hub_store(&directory);

    store
        .write_record(record(
            Author::LocalExplicit,
            Surface::VigilSettings,
            Scope::node("node-a"),
            SettingValue::Int(30),
            NEW_MS,
        ))
        .expect("write the node-scoped local pin");
    store
        .write_record(record(
            Author::LocalExplicit,
            Surface::VigilSettings,
            Scope::camera("driveway"),
            SettingValue::Int(3),
            OLD_MS,
        ))
        .expect("write the camera-scoped local pin");
    store
        .write_pushed_record(record(
            Author::Pushed,
            Surface::ManagementServer,
            Scope::camera("driveway"),
            SettingValue::Int(5),
            NEW_MS,
        ))
        .expect("write the per-camera push through the hub-role handle");

    let effective = store.resolve(SETTING, &target()).expect("resolve");

    assert_eq!(effective.author, Author::LocalExplicit);
    assert_eq!(effective.requested, SettingValue::Int(3));
    assert_eq!(effective.scope, Scope::camera("driveway"));
    assert!(
        !effective.inherited,
        "this camera is running its own override, not inheriting the node's: {effective:?}"
    );
    assert!(
        effective
            .held
            .iter()
            .any(|held| held.record.scope == Scope::node("node-a")),
        "the wider local record stays stored and reported rather than being overwritten: {:?}",
        effective.held
    );
}
