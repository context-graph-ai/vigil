//! Omitted, empty, invalid and unavailable stay four distinct things, and an
//! explicitly empty class list is the one that used to be silently rewritten.
//!
//! Asking for no classes at all is not a runnable allowlist. The old behavior
//! accepted it as a valid selection of zero classes and then substituted person
//! back in when the detector was built, so an owner who explicitly asked for
//! nothing got person detection anyway with nothing telling them why. It is
//! refused when it is written, naming the field and the remedy — and the remedy
//! is the store's own reset, which drops the setting to whatever is beneath it
//! and says what that is. "Remove the option" is not the remedy here: a removed
//! key only clears the record the surface it was removed from authored, so
//! telling an owner to delete a line is telling them to perform a different
//! operation with a different outcome.
//!
//! RED: `vigil::settings_store` and `vigil::settings_backends` are skeleton-only
//! (`todo!()` bodies) pending the settings-store implementation.

use std::sync::atomic::{AtomicU64, Ordering};

use vigil::PersistedClock;
use vigil::settings_backends::validate_detection_classes;
use vigil::settings_model::{
    DETECTOR_CONFIDENCE_THRESHOLD_SETTING, DETECTOR_SAMPLE_FRAMES_SETTING,
    RECOGNITION_THRESHOLD_SETTING, RefusalKind, Scope, SettingValue, SettingsError, Surface,
};
use vigil::settings_store::SettingsStore;

const DETECTOR_CLASSES: &str = "detector_classes";
const NODE: &str = "node-a";

/// A deterministic, strictly advancing persisted clock: distinct ordered stamps
/// with no sleep and no dependence on machine speed.
fn advancing_clock() -> PersistedClock {
    let next = AtomicU64::new(1_700_000_000_000);
    PersistedClock::from_millis_source(move || next.fetch_add(1_000, Ordering::SeqCst))
}

/// Unfakeable: an empty list is the one input a re-substituting implementation
/// handles by producing a perfectly working person-only install, so no assertion
/// about the resulting detection behavior can catch it — this asserts the write
/// is refused, that nothing is stored, and that the remedy names the reset
/// operation rather than the surface edit that does something else. A refusal
/// whose remedy says "remove the option" fails, which is what stops the old
/// wording from being carried back in.
#[test]
fn explicit_empty_detector_classes_is_an_actionable_error_naming_field_and_remedy() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = SettingsStore::open_with_clock(directory.path(), advancing_clock())
        .expect("open the node-side settings store");

    let empty: Vec<String> = Vec::new();
    assert!(
        validate_detection_classes(&empty).is_err(),
        "an explicitly empty class list is not a runnable allowlist"
    );
    assert!(
        validate_detection_classes(&["person".to_string()]).is_ok(),
        "sanity: a list naming one real class is accepted, so this is not a blanket refusal"
    );

    let error = store
        .set_local(
            DETECTOR_CLASSES,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::list(Vec::<String>::new()),
        )
        .expect_err("asking for no classes at all is refused when it is written");

    let refused = match error {
        SettingsError::Refused(refused) => refused,
        other => panic!("expected a refusal carrying a cause and a remedy, got: {other:?}"),
    };
    assert_eq!(
        refused.kind,
        RefusalKind::InvalidClass {
            setting: DETECTOR_CLASSES.to_string(),
        },
        "the refusal is typed against the class list and names which setting: {refused:?}"
    );
    assert!(
        refused.cause.contains(DETECTOR_CLASSES),
        "the cause names the field: {refused:?}"
    );
    assert!(
        refused.cause.to_ascii_lowercase().contains("empty"),
        "the cause says the list is empty, which is what separates this from an invalid entry: \
         {refused:?}"
    );

    let remedy = refused.remedy.to_ascii_lowercase();
    assert!(
        remedy.contains("reset"),
        "the remedy names the store's reset operation, which drops the setting to whatever is \
         beneath it: {:?}",
        refused.remedy
    );
    for old_phrasing in [
        "remove the option",
        "remove the detector_classes option",
        "delete the line",
    ] {
        assert!(
            !remedy.contains(old_phrasing),
            "removing a key clears only the record that surface authored, which is a different \
             operation with a different outcome, and must not be offered as the remedy — the \
             refusal still says {old_phrasing:?}: {:?}",
            refused.remedy
        );
    }

    let stored = store
        .records(DETECTOR_CLASSES)
        .expect("read the stored records after the refusal");
    assert!(
        stored.is_empty(),
        "nothing is stored, so nothing downstream has an empty allowlist to re-substitute: \
         {stored:?}"
    );
}

/// Unfakeable: before cold-review-r4 finding 16's fix, `validate_setting_value`
/// range-checked integers only and fell through to `Ok(())` for every float, so
/// this exact write silently succeeded and stayed stored, ignored, forever —
/// against the documented 0.0-1.0 range and against the direction document's
/// rule that an invalid value is refused at write time, never stored and then
/// ignored. This drives the write through the real store's settings door
/// (`set_local`), not the bare validator function, so a refusal that only the
/// validator enforced but the store's write path bypassed would still be
/// caught.
#[test]
fn an_out_of_range_float_setting_is_refused_at_the_settings_door_and_nothing_is_stored() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = SettingsStore::open_with_clock(directory.path(), advancing_clock())
        .expect("open the node-side settings store");

    for setting in [
        DETECTOR_CONFIDENCE_THRESHOLD_SETTING,
        RECOGNITION_THRESHOLD_SETTING,
    ] {
        let error = store
            .set_local(
                setting,
                Surface::VigilSettings,
                Scope::node(NODE),
                SettingValue::Float(7.5),
            )
            .expect_err(&format!(
                "{setting} declares a 0.0-1.0 range; 7.5 must be refused, not silently stored"
            ));
        let refused = match error {
            SettingsError::Refused(refused) => refused,
            other => panic!("expected a refusal carrying a cause and a remedy, got: {other:?}"),
        };
        assert_eq!(
            refused.kind,
            RefusalKind::InvalidValue {
                setting: setting.to_string(),
            },
            "the refusal is typed against the setting: {refused:?}"
        );
        assert!(
            refused.cause.contains("0.0") && refused.cause.contains("1.0"),
            "the cause must state the DECIMAL bound every document spells (0.0 to 1.0), not just \
             any digit 0 and any digit 1 — a stale message declaring \"0 to 100\" would satisfy \
             the old loose check while telling the operator the wrong range: \
             {refused:?}"
        );

        let stored = store
            .records(setting)
            .expect("read the stored records after the refusal");
        assert!(
            stored.is_empty(),
            "the out-of-range float must not be stored: {stored:?}"
        );
    }
}

/// The complement of the refusal above: an in-range float, AND an in-range
/// whole number written into the same float-typed setting, are both accepted
/// — a float-typed setting is range-checked, not merely rejected for carrying
/// the "wrong" numeric variant.
#[test]
fn an_in_range_float_or_whole_number_is_accepted_for_a_float_setting() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = SettingsStore::open_with_clock(directory.path(), advancing_clock())
        .expect("open the node-side settings store");

    store
        .set_local(
            DETECTOR_CONFIDENCE_THRESHOLD_SETTING,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::Float(0.65),
        )
        .expect("an in-range float must be accepted");

    store
        .set_local(
            RECOGNITION_THRESHOLD_SETTING,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::Int(1),
        )
        .expect(
            "an in-range whole number is an admissible value for a float-typed setting, not a \
             type mismatch",
        );
}

/// Unfakeable the same way as the float case above: before the fix, the
/// settings store's own declared range for `detector_sample_frames` was 1-120
/// while the config loader that actually reads the persisted value back at
/// startup refuses anything above 64 (`config.rs::validate_detector_sample_frames`)
/// — so this exact write used to succeed at the settings door and then fail
/// to load, an accepted-then-broken value with no refusal at the point an
/// operator could act on it. The two doors must agree; 100 is now refused at
/// the door that used to accept it.
#[test]
fn detector_sample_frames_is_refused_above_the_loaders_own_ceiling() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = SettingsStore::open_with_clock(directory.path(), advancing_clock())
        .expect("open the node-side settings store");

    let error = store
        .set_local(
            DETECTOR_SAMPLE_FRAMES_SETTING,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::Int(100),
        )
        .expect_err(
            "100 exceeds the loader's own 1-64 ceiling and must be refused at the settings door, \
             matching it exactly rather than the store's former wider 1-120",
        );
    let refused = match error {
        SettingsError::Refused(refused) => refused,
        other => panic!("expected a refusal carrying a cause and a remedy, got: {other:?}"),
    };
    assert!(
        refused.cause.contains("64"),
        "the cause states the loader-matching ceiling of 64, not the former 120: {refused:?}"
    );

    let stored = store
        .records(DETECTOR_SAMPLE_FRAMES_SETTING)
        .expect("read the stored records after the refusal");
    assert!(stored.is_empty(), "100 must not be stored: {stored:?}");

    store
        .set_local(
            DETECTOR_SAMPLE_FRAMES_SETTING,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::Int(64),
        )
        .expect("64 is exactly at the loader's ceiling and must be accepted");
}

/// The store-side half of binding the two range doors together at the exact
/// boundary the loader (`config.rs::validate_confidence_threshold` /
/// `validate_recognition_threshold`) declares — 0.0 to 1.0 — for both float
/// thresholds. Paired with the loader-side half driven through the real
/// compiled binary in `crates/vigil-bin/tests/range_doors_binding.rs`
/// (`declared_range`/`declared_float_range` here are private to this crate,
/// so this file can pin only this store's own boundary; the loader-side test
/// pins the loader's identical boundary independently). Narrowing either
/// door's bound alone — this one, or the loader's — moves ONE half of a pin
/// off the shared literal `1.0`/`1.01` and fails the half that moved, which
/// is what "the two doors agree" being provable rather than merely stated in
/// a doc comment requires (cold-review-arc2-r5 finding 7).
#[test]
fn float_threshold_range_matches_the_loaders_own_ceiling_at_the_exact_boundary() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = SettingsStore::open_with_clock(directory.path(), advancing_clock())
        .expect("open the node-side settings store");

    for setting in [
        DETECTOR_CONFIDENCE_THRESHOLD_SETTING,
        RECOGNITION_THRESHOLD_SETTING,
    ] {
        store
            .set_local(
                setting,
                Surface::VigilSettings,
                Scope::node(NODE),
                SettingValue::Float(1.0),
            )
            .expect("1.0 is exactly at the loader's ceiling and must be accepted");

        let error = store
            .set_local(
                setting,
                Surface::VigilSettings,
                Scope::node(NODE),
                SettingValue::Float(1.01),
            )
            .expect_err("1.01 is one hundredth above the loader's ceiling and must be refused");
        let refused = match error {
            SettingsError::Refused(refused) => refused,
            other => panic!("expected a refusal carrying a cause and a remedy, got: {other:?}"),
        };
        assert!(
            refused.cause.contains("1.0"),
            "the cause must pin the loader-matching ceiling of 1.0: {refused:?}"
        );
    }
}
