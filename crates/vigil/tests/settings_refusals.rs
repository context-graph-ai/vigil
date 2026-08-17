//! The two configurability commitments, both of which were violated in living
//! memory.
//!
//! An invalid value on a local surface is refused at the moment it is written,
//! with an actionable reason — never stored and then quietly ignored, which
//! leaves an operator reading their own value off the surface while the machine
//! runs something else. And a declared validation range never refuses a value
//! that is already stored and working on an install: once a node runs with a
//! value, a later Vigil version that narrows the range does not get to reject it
//! on restart, strand it, or silently move it. A range that would reject an
//! existing value is a defect in the range, not in the install — the
//! queue-capacity ceiling that refused a working `=2000` install is the exact
//! shape this forbids.
//!
//! RED: `vigil::settings_store` is skeleton-only (`todo!()` bodies) pending the
//! settings-store implementation.

use std::sync::atomic::{AtomicU64, Ordering};

use vigil::PersistedClock;
use vigil::settings_model::{
    Author, Refusal, RefusalKind, Scope, ScopeTarget, SettingRecord, SettingValue, SettingsError,
    Surface,
};
use vigil::settings_store::SettingsStore;

const QUEUE_CAPACITY: &str = "detector_queue_capacity";
const MOTION_SENSITIVITY: &str = "motion_sensitivity";

/// A deterministic, strictly advancing persisted clock: distinct ordered stamps
/// with no sleep and no dependence on machine speed.
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

fn refusal(error: SettingsError) -> Refusal {
    match error {
        SettingsError::Refused(refusal) => refusal,
        other => panic!("expected a refusal carrying a cause and a remedy, got: {other:?}"),
    }
}

/// Unfakeable on the half that actually bites: any implementation returns an
/// error somewhere for a zero queue capacity, so the error alone proves nothing.
/// The stored-record list is captured BEFORE the write and compared afterwards,
/// so a value that is written and then ignored at resolution — the exact defect
/// this commitment names — fails here even though its refusal message would look
/// identical.
#[test]
fn an_invalid_value_is_refused_at_write_time_and_nothing_is_stored() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    let before = store
        .records(QUEUE_CAPACITY)
        .expect("read the stored records before the invalid write");

    let error = store
        .set_local(
            QUEUE_CAPACITY,
            Surface::VigilSettings,
            scope(),
            SettingValue::Int(0),
        )
        .expect_err("a zero detection queue capacity is not a runnable value");

    let refused = refusal(error);
    assert_eq!(
        refused.kind,
        RefusalKind::InvalidValue {
            setting: QUEUE_CAPACITY.to_string(),
        },
        "the refusal is typed as an invalid value and names which setting: {refused:?}"
    );
    assert!(
        !refused.cause.is_empty() && !refused.remedy.is_empty(),
        "every refusal carries both what went wrong and what the operator does about it: \
         {refused:?}"
    );
    let statement = refused.statement();
    assert!(
        statement.contains(QUEUE_CAPACITY),
        "the rendered refusal names the field: {statement:?}"
    );
    assert!(
        statement.contains('0'),
        "the rendered refusal names the value it rejected: {statement:?}"
    );

    let after = store
        .records(QUEUE_CAPACITY)
        .expect("read the stored records after the refusal");
    assert_eq!(
        after, before,
        "a refused write stores NOTHING — not the rejected value, and not a corrected one: \
         {after:?}"
    );
}

/// Unfakeable: the same value, 11, is refused by `set_local` and honored by
/// resolution in one test. That pair can only pass if validation runs at write
/// time over the proposed value and never again over what is already stored;
/// an implementation that re-validates on read (the upgrade behavior this
/// commitment forbids) fails the resolution half, and one that never validates
/// at all fails the refusal half. Neither half can be satisfied by loosening
/// the range, because loosening it breaks the refusal.
#[test]
fn a_narrowed_range_never_refuses_a_value_already_stored_and_working() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    // What the install is already running: a record written when the declared
    // range still admitted it, exactly as it would come back from the hub after
    // a wipe or be read out of the store after an add-on update.
    store
        .write_record(SettingRecord {
            setting: MOTION_SENSITIVITY.to_string(),
            author: Author::LocalExplicit,
            surface: Surface::ConfigFile,
            scope: scope(),
            value: SettingValue::Int(11),
            reason: "set on this install before a later version narrowed the range".to_string(),
            written_at_ms: 1_600_000_000_000,
            domain_generation: 0,
            reset: false,
        })
        .expect("a value already stored on this install is readable back, not rejected");

    // The range as this version declares it is genuinely narrower: writing the
    // same value fresh is refused right now.
    let error = store
        .set_local(
            MOTION_SENSITIVITY,
            Surface::VigilSettings,
            scope(),
            SettingValue::Int(11),
        )
        .expect_err("the declared range no longer admits this value for a NEW write");
    assert_eq!(
        refusal(error).kind,
        RefusalKind::InvalidValue {
            setting: MOTION_SENSITIVITY.to_string(),
        },
        "sanity: the narrowing is real, so this test is not asserting against a range that still \
         admits the stored value"
    );

    let effective = store
        .resolve(MOTION_SENSITIVITY, &target())
        .expect("the install still resolves its own setting after the range narrowed");
    assert_eq!(
        effective.requested,
        SettingValue::Int(11),
        "the stored value is neither rejected nor silently moved: {effective:?}"
    );
    assert_eq!(
        effective.author,
        Author::LocalExplicit,
        "and it is still attributed to whoever set it: {effective:?}"
    );

    let listed = store
        .listing(&target())
        .expect("the operator surface still renders this deployment");
    let rendered = listed
        .iter()
        .find(|entry| entry.setting == MOTION_SENSITIVITY)
        .unwrap_or_else(|| {
            panic!("a setting this install runs is never dropped from the listing: {listed:?}")
        });
    assert_eq!(
        rendered.requested,
        SettingValue::Int(11),
        "the value stays effective on the operator surface, not stranded off it: {rendered:?}"
    );

    let stored = store
        .records(MOTION_SENSITIVITY)
        .expect("read the stored records");
    assert!(
        stored
            .iter()
            .any(|record| record.value == SettingValue::Int(11) && !record.reset),
        "and it is still in the store, unmoved: {stored:?}"
    );
}
