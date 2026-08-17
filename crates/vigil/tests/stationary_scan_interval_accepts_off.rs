//! Turning periodic re-scanning off is a value the settings surface accepts.
//!
//! A camera pointed at an unchanging wall re-scans on a timer for no reason,
//! and the owner's answer is to turn that timer off. Zero is what carries it:
//! not a spin, not a clamp, but the value that says "do not re-scan a
//! motion-free scene at all". A surface that refuses it takes a capability away
//! along with the knob that used to carry it.
//!
//! This is a standing regression guard rather than a proof of new behavior: the
//! range already admits zero and the gate already reads it that way. What was
//! missing is anything that fails if the lower bound creeps back up to one, at
//! which point an owner who had re-scanning off finds their setting refused on
//! the next write and no test says a word about it.
//!
//! Unfakeable because it pins the boundary from both sides in one test: zero is
//! accepted AND reads back as zero, and a value genuinely outside the range is
//! still refused. A build that widened the range by dropping validation
//! altogether passes the first half and fails the second.

use std::sync::atomic::{AtomicU64, Ordering};

use vigil::PersistedClock;
use vigil::settings_model::{DETECTOR_STATIONARY_INTERVAL_SETTING, Scope, SettingValue, Surface};
use vigil::settings_store::SettingsStore;

const SITE: &str = "home";

/// The value that turns periodic re-scanning off.
const RESCANNING_OFF: i64 = 0;

/// A value past the top of the declared range, which stays refused. One second
/// beyond a day, so the refusal is about the bound rather than about the shape
/// of the number.
const BEYOND_THE_RANGE: i64 = 86_401;

/// A deterministic, strictly advancing persisted clock: ordered stamps with no
/// sleep and no dependence on machine speed.
fn advancing_clock() -> PersistedClock {
    let next = AtomicU64::new(1_700_000_000_000);
    PersistedClock::from_millis_source(move || next.fetch_add(1_000, Ordering::SeqCst))
}

#[test]
fn the_settings_store_accepts_zero_as_periodic_rescanning_turned_off() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = SettingsStore::open_with_clock(directory.path(), advancing_clock())
        .expect("open the settings store");

    let written = store.set_local(
        DETECTOR_STATIONARY_INTERVAL_SETTING,
        Surface::VigilSettings,
        Scope::site(SITE),
        SettingValue::Int(RESCANNING_OFF),
    );
    assert!(
        written.is_ok(),
        "turning periodic re-scanning off has to be a value the surface accepts: an owner whose \
         camera watches an unchanging wall is asking for exactly this, and refusing it removes a \
         capability along with the knob that used to carry it. Got: {written:?}"
    );

    let records = store
        .records(DETECTOR_STATIONARY_INTERVAL_SETTING)
        .expect("read the records back");
    let stored = records
        .iter()
        .find(|record| !record.reset)
        .unwrap_or_else(|| panic!("the accepted value must leave a record; got: {records:?}"));
    assert_eq!(
        stored.value,
        SettingValue::Int(RESCANNING_OFF),
        "and it is stored as the value that was typed, not quietly moved to the lowest spinning \
         interval — a surface that reads back something the owner did not ask for is the failure \
         accepting zero exists to avoid"
    );

    let refused = store.set_local(
        DETECTOR_STATIONARY_INTERVAL_SETTING,
        Surface::VigilSettings,
        Scope::site(SITE),
        SettingValue::Int(BEYOND_THE_RANGE),
    );
    assert!(
        refused.is_err(),
        "and the range is still a range: a value past the top of it is refused at write time, so \
         accepting zero is a deliberate lower bound rather than validation having been dropped. \
         Got: {refused:?}"
    );
}
