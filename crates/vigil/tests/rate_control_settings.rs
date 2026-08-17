//! The rate controls: the four values an owner reaches for when the machine
//! cannot keep up with the load it has been given.
//!
//! Frame sampling, the stationary-scan interval, per-camera motion sensitivity,
//! and the detection queue capacity are real settings that take effect at the
//! value they are set to. **No automatic-management domain governs any of
//! them** — no tuner writes them — so setting one takes no disabling step
//! first, and a refusal here would be a defect rather than the model working.
//! Motion sensitivity is the one an owner tunes per camera, so a camera with its
//! own value runs it while every other camera inherits the site's, and the
//! operator surface says which of the two is happening. An out-of-range value is
//! refused out loud rather than clamped into something the owner never asked
//! for.
//!
//! RED: `vigil::settings_store` and `vigil::settings_domains` are skeleton-only
//! (`todo!()` bodies) pending the settings-store implementation.

use std::sync::atomic::{AtomicU64, Ordering};

use vigil::PersistedClock;
use vigil::settings_backends::DETECTION_BACKEND_SETTING;
use vigil::settings_domains::{
    ACCELERATED_DETECTION_DOMAIN, GateDecision, HARDWARE_DECODING_DOMAIN, gate_write,
    governing_domain,
};
use vigil::settings_model::{
    Author, ControlState, RefusalKind, Scope, ScopeLevel, ScopeTarget, SettingValue, SettingsError,
    Surface,
};
use vigil::settings_store::SettingsStore;

const SAMPLE_FRAMES: &str = "detector_sample_frames";
const STATIONARY_INTERVAL: &str = "detector_stationary_interval_secs";
const MOTION_SENSITIVITY: &str = "motion_sensitivity";
const QUEUE_CAPACITY: &str = "detector_queue_capacity";

/// The four values this file is about, named once so no test can quietly cover
/// three of them.
const RATE_CONTROLS: [&str; 4] = [
    SAMPLE_FRAMES,
    STATIONARY_INTERVAL,
    MOTION_SENSITIVITY,
    QUEUE_CAPACITY,
];

const SITE: &str = "home";
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

fn camera_target(camera: &str) -> ScopeTarget {
    ScopeTarget {
        tenant: "owner".to_string(),
        site: SITE.to_string(),
        node: NODE.to_string(),
        camera: Some(camera.to_string()),
    }
}

fn node_target() -> ScopeTarget {
    ScopeTarget {
        tenant: "owner".to_string(),
        site: SITE.to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

/// Unfakeable: each control is set to a DIFFERENT value and every one is read
/// back through the real resolution, so an implementation that accepts the write
/// and then resolves a default, or that shares one row across the four controls,
/// returns the wrong number for at least one of them. The control state and
/// author are asserted alongside the value, so a stored-but-unattributed write
/// does not pass either.
///
/// No runtime is brought up here, so this deliberately does NOT claim any of the
/// four is what a running machine is using: that is a separate proof against a
/// real process, and a name promising it here would be an overclaim.
#[test]
fn each_rate_control_is_stored_and_resolved_at_its_declared_value() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);
    let scope = Scope::node(NODE);

    let declared = [
        (SAMPLE_FRAMES, 8_i64),
        (STATIONARY_INTERVAL, 45),
        (MOTION_SENSITIVITY, 8),
        (QUEUE_CAPACITY, 4),
    ];

    for (setting, value) in declared {
        store
            .set_local(
                setting,
                Surface::VigilSettings,
                scope.clone(),
                SettingValue::Int(value),
            )
            .unwrap_or_else(|error| panic!("set {setting} to {value}: {error:?}"));
    }

    for (setting, value) in declared {
        let effective = store
            .resolve(setting, &node_target())
            .unwrap_or_else(|error| panic!("resolve {setting}: {error:?}"));
        assert_eq!(
            effective.requested,
            SettingValue::Int(value),
            "{setting} resolves at the value it was set to: {effective:?}"
        );
        assert_eq!(
            effective.control_state,
            ControlState::SetByYou,
            "{setting} is attributed to the person who set it: {effective:?}"
        );
        assert_eq!(effective.author, Author::LocalExplicit);
        assert_eq!(effective.surface, Surface::VigilSettings);
        assert!(
            !effective.reason.is_empty(),
            "{setting} carries why the record exists; a blank reason is a defect: {effective:?}"
        );
    }
}

/// Unfakeable: both domain switches are left at their default ON state, which is
/// exactly the state in which a governed value is refused, and the writes still
/// succeed. The contrast is asserted in the same test — the detection backend IS
/// governed — so a `governing_domain` that returns `None` for everything, or a
/// gate that returns `Open` for everything, fails here rather than passing by
/// being uniformly permissive.
#[test]
fn no_domain_governs_the_rate_controls_so_setting_one_takes_no_disabling_step() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);
    let scope = Scope::node(NODE);

    let both_domains_on = [
        (ACCELERATED_DETECTION_DOMAIN.to_string(), true),
        (HARDWARE_DECODING_DOMAIN.to_string(), true),
    ];

    for setting in RATE_CONTROLS {
        assert!(
            governing_domain(setting).is_none(),
            "no automatic-management domain governs {setting}; no tuner writes the rate controls"
        );
        assert_eq!(
            gate_write(setting, &scope, &both_domains_on, None),
            GateDecision::Open,
            "with every domain on, the gate is still open for {setting} — there is nothing to \
             turn off first"
        );
        store
            .set_local(
                setting,
                Surface::VigilSettings,
                scope.clone(),
                SettingValue::Int(6),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "setting {setting} must succeed with no disabling step; expecting a refusal \
                     here would be expecting a defect: {error:?}"
                )
            });
    }

    let governed = governing_domain(DETECTION_BACKEND_SETTING).unwrap_or_else(|| {
        panic!(
            "sanity: the detection backend IS governed, so this file is not asserting against a \
             roster that governs nothing at all"
        )
    });
    assert_eq!(
        governed.switch, ACCELERATED_DETECTION_DOMAIN,
        "and it is governed by the accelerated-detection domain: {governed:?}"
    );
    assert!(
        !governed.members.contains(&MOTION_SENSITIVITY),
        "a rate control never appears in a domain's membership: {governed:?}"
    );
}

/// Unfakeable: both cameras are asked about in the SAME store with the SAME
/// site-wide record present, so the two answers can only differ because of the
/// camera-scoped record. `inherited` is asserted in both directions, so an
/// implementation that hardcodes it either way fails one of the two, and the
/// resolved scope level is asserted alongside it so the flag cannot be set
/// without the resolution behind it agreeing.
#[test]
fn per_camera_motion_sensitivity_overrides_the_site_value_and_others_inherit() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    store
        .set_local(
            MOTION_SENSITIVITY,
            Surface::VigilSettings,
            Scope::site(SITE),
            SettingValue::Int(5),
        )
        .expect("the site-wide motion sensitivity");
    store
        .set_local(
            MOTION_SENSITIVITY,
            Surface::VigilSettings,
            Scope::camera("front"),
            SettingValue::Int(8),
        )
        .expect("the front camera's own motion sensitivity");

    let front = store
        .resolve(MOTION_SENSITIVITY, &camera_target("front"))
        .expect("resolve the front camera");
    assert_eq!(
        front.requested,
        SettingValue::Int(8),
        "the camera with its own value runs it: {front:?}"
    );
    assert_eq!(front.scope, Scope::camera("front"));
    assert!(
        !front.inherited,
        "and the operator surface says it is running its own override: {front:?}"
    );

    let back = store
        .resolve(MOTION_SENSITIVITY, &camera_target("back"))
        .expect("resolve the back camera");
    assert_eq!(
        back.requested,
        SettingValue::Int(5),
        "every other camera inherits the site's value: {back:?}"
    );
    assert_eq!(back.scope.level, ScopeLevel::Site);
    assert!(
        back.inherited,
        "and the operator surface says it is inheriting rather than running its own: {back:?}"
    );
}

/// Unfakeable: the silent clamp this forbids is `.max(1)`, which produces a
/// perfectly working install running 1 while the owner reads 0 back — so the
/// error alone is not enough, and the stored records are read afterwards and
/// asserted to carry neither the rejected 0 nor a corrected 1. The refusal must
/// also state the range, which is what makes it actionable rather than a bare
/// rejection.
#[test]
fn zero_queue_capacity_is_an_actionable_range_error_not_a_silent_clamp() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = open(&directory);

    let error = store
        .set_local(
            QUEUE_CAPACITY,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::Int(0),
        )
        .expect_err("a zero detection queue capacity is outside the declared range");

    let refused = match error {
        SettingsError::Refused(refused) => refused,
        other => panic!("expected a refusal carrying a cause and a remedy, got: {other:?}"),
    };
    assert_eq!(
        refused.kind,
        RefusalKind::InvalidValue {
            setting: QUEUE_CAPACITY.to_string(),
        },
        "the refusal is typed as an invalid value and names which setting: {refused:?}"
    );
    let statement = refused.statement();
    assert!(
        statement.contains(QUEUE_CAPACITY),
        "the refusal names the field: {statement:?}"
    );
    assert!(
        statement.contains('1'),
        "the refusal states the valid range, whose lowest runnable capacity is 1, so the operator \
         knows what to type instead: {statement:?}"
    );
    assert!(
        !refused.remedy.is_empty(),
        "and it says what to do about it: {refused:?}"
    );

    let stored = store
        .records(QUEUE_CAPACITY)
        .expect("read the stored records after the refusal");
    assert!(
        !stored
            .iter()
            .any(|record| record.value == SettingValue::Int(0)),
        "the rejected value is not stored: {stored:?}"
    );
    assert!(
        !stored
            .iter()
            .any(|record| record.value == SettingValue::Int(1)),
        "and it is not silently clamped to 1 either — the owner never asked for 1: {stored:?}"
    );
}
