//! A local operator shadows a pushed value; they never delete it.
//!
//! Your pin sits above the server's record and runs, the server's record stays
//! stored and visible underneath, and resetting your pin later brings the
//! server's value back rather than leaving a hole. The alternative — letting a
//! node evict a push — buys nothing a shadow does not already buy and costs a
//! node that has quietly opted out of the fleet while the hub keeps re-pushing
//! into the gap. Because a shadowed push is the single most likely source of
//! confusion in this model, it is reported on the FACE of the operator listing,
//! not in a history someone has to go looking for.
//!
//! The pushed row is written through the hub-role handle, which is the harness
//! stand-in for hub sync and ships in no artifact; that the ordinary node-side
//! handle cannot write that table at all is proven in
//! `settings_pushed_table_boundary.rs`. This file is about what a pushed record
//! does once it is legitimately there.
//!
//! RED: `vigil::settings_store` is skeleton-only (`todo!()` bodies) pending the
//! settings-store implementation.

use std::sync::atomic::{AtomicU64, Ordering};

use vigil::PersistedClock;
use vigil::settings_model::{
    Author, ControlState, HeldReason, Scope, ScopeTarget, SettingRecord, SettingValue,
    SettingsError, Surface,
};
use vigil::settings_store::{SettingsStore, SurfaceSnapshot};

const SETTING: &str = "detector_sample_frames";

/// Distinctive multi-digit values: the shadowed statement is checked by
/// substring, and a single digit can be satisfied by an unrelated number that
/// happens to appear in the rendered sentence. Neither of these is a substring
/// of the other, nor of any timestamp or count this file renders.
const PUSHED_VALUE: i64 = 37;
const PINNED_VALUE: i64 = 53;

/// The push is stamped NEWER than anything the local clock below produces, so a
/// resolution that arbitrates by recency returns the pushed value in every test
/// here rather than the pin that outranks it.
const PUSH_MS: i64 = 9_000_000_000_000;

const NODE: &str = "node-a";

/// A deterministic, strictly advancing persisted clock: distinct ordered stamps
/// with no sleep and no dependence on machine speed.
fn advancing_clock() -> PersistedClock {
    let next = AtomicU64::new(1_700_000_000_000);
    PersistedClock::from_millis_source(move || next.fetch_add(1_000, Ordering::SeqCst))
}

fn scope() -> Scope {
    Scope::node(NODE)
}

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: "owner".to_string(),
        site: "home".to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

/// A deployment carrying one pushed record, handed back as a genuinely
/// edge-labeled node handle: the hub-role handle that wrote the record is
/// closed before the node-role handle is opened, so the returned store's own
/// engine-level scope label is the node's — not the hub's — and any later
/// attempt through it to write the pushed table is refused on that handle's
/// own authority rather than a role field flipped on a still-hub-scoped
/// handle. Both opens share one clock so timestamps keep advancing across the
/// reopen with no sleep and no collision.
fn node_with_pushed_record(directory: &tempfile::TempDir) -> SettingsStore {
    let clock = advancing_clock();
    {
        let hub = SettingsStore::open_hub_role_with_clock(directory.path(), clock.clone())
            .expect("open the hub-role settings store");
        hub.write_pushed_record(SettingRecord {
            setting: SETTING.to_string(),
            author: Author::Pushed,
            surface: Surface::ManagementServer,
            scope: scope(),
            value: SettingValue::Int(PUSHED_VALUE),
            reason: "fleet baseline for this node".to_string(),
            written_at_ms: PUSH_MS,
            domain_generation: 0,
            reset: false,
        })
        .expect("the hub writes its record into the down-only pushed table");
        // `hub` drops here, closing the hub-labeled handle before the node
        // reopen below, so no clone of it keeps that scope label alive.
    }
    SettingsStore::open_with_clock(directory.path(), clock)
        .expect("reopen the same deployment directory as the genuinely edge-labeled node handle")
}

fn pushed_record_still_stored(store: &SettingsStore, after: &str) {
    let stored = store.records(SETTING).expect("read the stored records");
    let pushed: Vec<&SettingRecord> = stored
        .iter()
        .filter(|record| record.author == Author::Pushed)
        .collect();
    assert_eq!(
        pushed.len(),
        1,
        "the pushed record is still stored, exactly once, after {after}: {stored:?}"
    );
    assert_eq!(
        pushed[0].value,
        SettingValue::Int(PUSHED_VALUE),
        "and it still carries what the server set, unmodified, after {after}: {pushed:?}"
    );
    assert!(
        !pushed[0].reset,
        "and it was not turned into a reset record by a local operation after {after}: {pushed:?}"
    );
}

fn snapshot_without_the_key(surface: Surface) -> SurfaceSnapshot {
    SurfaceSnapshot {
        surface,
        scope: scope(),
        present_and_parsing: true,
        entries: Vec::new(),
        declared_defaults: Vec::new(),
    }
}

/// Unfakeable: the pushed record is read back through the store after the pin
/// lands, so an implementation that made the pin effective by replacing the
/// server's row — which produces an identical effective value — fails on the
/// record read. The pushed record is also proven effective BEFORE the pin, so
/// this cannot pass by the push never having been applied in the first place.
#[test]
fn a_local_pin_over_a_pushed_record_is_effective_with_the_pushed_record_still_stored() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = node_with_pushed_record(&directory);

    let before = store
        .resolve(SETTING, &target())
        .expect("resolve before the pin");
    assert_eq!(
        before.requested,
        SettingValue::Int(PUSHED_VALUE),
        "with nothing local, the server's value is what runs: {before:?}"
    );
    assert_eq!(before.author, Author::Pushed);
    assert_eq!(before.control_state, ControlState::SetByManagementServer);

    store
        .set_local(
            SETTING,
            Surface::VigilSettings,
            scope(),
            SettingValue::Int(PINNED_VALUE),
        )
        .expect("the operator pins their own value over the server's");

    let after = store
        .resolve(SETTING, &target())
        .expect("resolve after the pin");
    assert_eq!(
        after.requested,
        SettingValue::Int(PINNED_VALUE),
        "the pin is effective: {after:?}"
    );
    assert_eq!(after.author, Author::LocalExplicit);
    assert_eq!(after.control_state, ControlState::SetByYou);

    pushed_record_still_stored(&store, "a local pin was made over it");
}

/// Unfakeable: the shadowed record is asserted on the resolved answer AND on the
/// listing the operator actually reads, and the rendered statement must name both
/// numbers — what the server set and what is running — so a listing that merely
/// keeps the row somewhere retrievable, or that renders a held record without
/// saying why it is held, fails. It is also asserted NOT to be reported as
/// dormant, so the two held reasons cannot be collapsed into one bucket.
#[test]
fn the_shadowed_pushed_record_is_reported_on_the_face_of_the_listing_not_in_history() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = node_with_pushed_record(&directory);

    store
        .set_local(
            SETTING,
            Surface::VigilSettings,
            scope(),
            SettingValue::Int(PINNED_VALUE),
        )
        .expect("the operator pins their own value over the server's");

    let effective = store
        .resolve(SETTING, &target())
        .expect("resolve with the push shadowed");
    let shadowed = effective
        .held
        .iter()
        .find(|held| held.record.author == Author::Pushed)
        .unwrap_or_else(|| {
            panic!("the pushed record is reported alongside the effective value: {effective:?}")
        });
    assert_eq!(
        shadowed.reason,
        HeldReason::Shadowed {
            by_author: Author::LocalExplicit,
            by_surface: Surface::VigilSettings,
        },
        "the held reason names who is shadowing it and through which surface: {shadowed:?}"
    );
    assert!(
        shadowed.statement.contains(&PUSHED_VALUE.to_string())
            && shadowed.statement.contains(&PINNED_VALUE.to_string()),
        "the statement names what the server set and what is actually running, so nobody believes \
         the push was applied: {:?}",
        shadowed.statement
    );
    assert_eq!(
        effective.shadowed().len(),
        1,
        "and it is reported as shadowed: {effective:?}"
    );
    assert!(
        effective.dormant().is_empty(),
        "shadowed by a higher-ranked record is a different thing from held dormant by a domain, \
         and the surface keeps them apart: {effective:?}"
    );

    let listing = store
        .listing(&target())
        .expect("read the operator listing for this deployment");
    let listed = listing
        .iter()
        .find(|entry| entry.setting == SETTING)
        .unwrap_or_else(|| panic!("the setting appears on the listing: {listing:?}"));
    assert!(
        listed
            .held
            .iter()
            .any(|held| held.record.author == Author::Pushed
                && matches!(held.reason, HeldReason::Shadowed { .. })),
        "the shadowed push is on the FACE of the listing, not somewhere a reader has to go \
         looking for it: {listed:?}"
    );
}

/// Unfakeable: the pushed value is asserted through the reset outcome AND
/// through a fresh resolution afterwards, so a reset that reported the right
/// value while leaving the pin effective fails the second read. The held list is
/// checked too: once the pin is reset, the push is no longer being shadowed by
/// anything, so a surface still labelling it shadowed is telling the operator
/// their pin is still in force.
#[test]
fn resetting_the_local_pin_restores_the_pushed_value() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = node_with_pushed_record(&directory);

    store
        .set_local(
            SETTING,
            Surface::VigilSettings,
            scope(),
            SettingValue::Int(PINNED_VALUE),
        )
        .expect("the operator pins their own value over the server's");

    let outcome = store
        .reset_local(SETTING, Surface::VigilSettings, &scope())
        .expect("the operator resets their own pin");
    assert_eq!(
        outcome.dropped_to.requested,
        SettingValue::Int(PUSHED_VALUE),
        "resetting the pin brings the server's value back rather than leaving a hole: {outcome:?}"
    );
    assert_eq!(outcome.dropped_to.author, Author::Pushed);

    let effective = store
        .resolve(SETTING, &target())
        .expect("resolve after the reset");
    assert_eq!(
        effective.requested,
        SettingValue::Int(PUSHED_VALUE),
        "and the server's value is what runs now: {effective:?}"
    );
    assert_eq!(effective.control_state, ControlState::SetByManagementServer);
    assert!(
        !effective
            .held
            .iter()
            .any(|held| held.record.author == Author::Pushed
                && matches!(held.reason, HeldReason::Shadowed { .. })),
        "nothing is shadowing the push any more, and the surface stops saying so: {effective:?}"
    );

    pushed_record_still_stored(&store, "the local pin was reset");
}

/// Unfakeable: this walks EVERY local operation the surface offers — pinning,
/// resetting, and each file or startup surface going quiet about the key — and
/// re-reads the pushed record after each one, so no single operation can be the
/// one that quietly evicts it. The node-side attempt to write the pushed table
/// with a different value is included, with the stored value re-checked after
/// the refusal: a node that could author at the pushed rank would be exactly the
/// second control path this model forbids.
#[test]
fn no_local_operation_deletes_a_pushed_record() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = node_with_pushed_record(&directory);

    store
        .set_local(
            SETTING,
            Surface::VigilSettings,
            scope(),
            SettingValue::Int(PINNED_VALUE),
        )
        .expect("the operator pins their own value over the server's");
    pushed_record_still_stored(&store, "a local pin");

    store
        .reset_local(SETTING, Surface::VigilSettings, &scope())
        .expect("the operator resets their own pin");
    pushed_record_still_stored(&store, "a local reset");

    for surface in [
        Surface::ConfigFile,
        Surface::AddonOptions,
        Surface::StartupOptions,
    ] {
        store
            .apply_surface_snapshot(&snapshot_without_the_key(surface))
            .unwrap_or_else(|error| {
                panic!("apply a {surface:?} snapshot without the key: {error:?}")
            });
        pushed_record_still_stored(
            &store,
            "a local surface stopped naming the key — absence is never how a server's record goes \
             away",
        );
    }

    let usurped = store.write_pushed_record(SettingRecord {
        setting: SETTING.to_string(),
        author: Author::Pushed,
        surface: Surface::ManagementServer,
        scope: scope(),
        value: SettingValue::Int(999),
        reason: "a node manufacturing a fleet instruction for itself".to_string(),
        written_at_ms: PUSH_MS + 1,
        domain_generation: 0,
        reset: false,
    });
    match usurped
        .expect_err("the ordinary node-side handle cannot author at the pushed rank at all")
    {
        SettingsError::ScopeLabelViolation { requested, allowed } => {
            assert!(
                !requested.is_empty() && !allowed.is_empty(),
                "the refusal names the scope label the write needed and the one this handle \
                 carries: requested={requested:?} allowed={allowed:?}"
            );
            assert_ne!(
                requested, allowed,
                "the refusal exists because the two differ: requested={requested:?} \
                 allowed={allowed:?}"
            );
        }
        other => panic!(
            "the boundary must hold as the engine's typed scope-label violation — a refusal for \
             any other reason (a validation quibble, a generic store error, a Vigil-side check) \
             would leave a node able to author at the pushed rank the day that reason stops \
             applying: {other:?}"
        ),
    }
    pushed_record_still_stored(&store, "a node-side attempt to overwrite the pushed table");
}
