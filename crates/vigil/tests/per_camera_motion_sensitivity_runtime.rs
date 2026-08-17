//! A camera's own motion sensitivity is what that camera's gate runs at.
//!
//! The store resolves a camera-scoped override correctly today and the running
//! process throws that away: it resolves once with no camera named, publishes
//! one value, and every camera's gate reads it. So an owner who turns the
//! driveway down because a tree moves in the wind turns every camera down, and
//! the operator surface says the driveway is running its own override while
//! nothing of the sort is happening.
//!
//! This is a process proof rather than a resolution proof: both cameras are
//! asked about in one running process, against one store, so the two answers
//! can only differ because the process kept them apart.
//!
//! Both halves are driven through the entry points production itself calls —
//! the runtime's own camera adoption as it starts the cameras, and the live
//! path a `vigil settings` change takes on the running machine. A test that
//! drove a convenience wrapper the runtime never calls would prove the wrapper
//! works and leave the shipped path unproven.

use std::sync::atomic::{AtomicU64, Ordering};

use vigil::PersistedClock;
use vigil::settings_application::{
    adopt_camera_scoped_settings, apply_live_change, mark_running_process,
    motion_sensitivity_in_force_for,
};
use vigil::settings_model::{
    MOTION_SENSITIVITY_SETTING, Scope, ScopeTarget, SettingValue, Surface,
};
use vigil::settings_store::SettingsStore;

const SITE: &str = "home";
const NODE: &str = "node-a";
const OVERRIDDEN_CAMERA: &str = "driveway";
const INHERITING_CAMERA: &str = "hallway";

/// The site-wide value every camera without its own runs at.
const SITE_SENSITIVITY: i64 = 3;
/// The overridden camera's first value, and the one it is moved to live. Both
/// differ from the site value and from each other, so no answer can be right by
/// coincidence.
const CAMERA_SENSITIVITY: i64 = 9;
const CAMERA_SENSITIVITY_AFTER_CHANGE: i64 = 6;

/// A deterministic, strictly advancing persisted clock: distinct ordered stamps
/// with no sleep and no dependence on machine speed.
fn advancing_clock() -> PersistedClock {
    let next = AtomicU64::new(1_700_000_000_000);
    PersistedClock::from_millis_source(move || next.fetch_add(1_000, Ordering::SeqCst))
}

fn node_target() -> ScopeTarget {
    ScopeTarget {
        tenant: "owner".to_string(),
        site: SITE.to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

#[test]
fn each_cameras_gate_runs_at_that_cameras_own_motion_sensitivity() {
    // Unfakeable because the two cameras are asked in the SAME process against
    // the SAME store, and every value is distinct: an implementation that
    // publishes one process-wide number gives both cameras the same answer and
    // fails whichever half it does not match, and one that hardcodes the
    // override direction fails the inheriting camera. The live half then moves
    // only one camera's record and asserts the other did not follow, so a
    // re-application that flattens the two back into one number fails there
    // even if the first half were somehow satisfied.
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = SettingsStore::open_with_clock(directory.path(), advancing_clock())
        .expect("open the node-side settings store");

    store
        .set_local(
            MOTION_SENSITIVITY_SETTING,
            Surface::VigilSettings,
            Scope::site(SITE),
            SettingValue::Int(SITE_SENSITIVITY),
        )
        .expect("the site-wide motion sensitivity");
    store
        .set_local(
            MOTION_SENSITIVITY_SETTING,
            Surface::VigilSettings,
            Scope::camera(OVERRIDDEN_CAMERA),
            SettingValue::Int(CAMERA_SENSITIVITY),
        )
        .expect("the overridden camera's own motion sensitivity");

    // This process is the one running the cameras, which is what makes a stored
    // value something a gate takes on rather than a row somebody wrote.
    mark_running_process();
    let cameras = vec![OVERRIDDEN_CAMERA.to_string(), INHERITING_CAMERA.to_string()];
    // The startup adoption, through the entry point the runtime itself calls as
    // it brings the cameras up.
    adopt_camera_scoped_settings(&store, &node_target(), &cameras);

    assert_eq!(
        motion_sensitivity_in_force_for(OVERRIDDEN_CAMERA),
        CAMERA_SENSITIVITY,
        "the camera with its own value runs it"
    );
    assert_eq!(
        motion_sensitivity_in_force_for(INHERITING_CAMERA),
        SITE_SENSITIVITY,
        "and every other camera keeps running the value it inherits, or turning one camera down \
         turns the whole site down"
    );

    // The same change made while the machine runs: one camera's record moves.
    store
        .set_local(
            MOTION_SENSITIVITY_SETTING,
            Surface::VigilSettings,
            Scope::camera(OVERRIDDEN_CAMERA),
            SettingValue::Int(CAMERA_SENSITIVITY_AFTER_CHANGE),
        )
        .expect("the overridden camera's new motion sensitivity");
    // And the live change, through the entry point `vigil settings` calls when
    // an operator changes a value on the running machine. It is given no camera
    // set: a settings command knows a data directory, so the run's own cameras
    // are what it has to reach, and that is exactly the step a wrapper taking
    // the camera list as an argument was hiding.
    apply_live_change(&store, &node_target());

    assert_eq!(
        motion_sensitivity_in_force_for(OVERRIDDEN_CAMERA),
        CAMERA_SENSITIVITY_AFTER_CHANGE,
        "the camera whose value changed runs the new one on its next segment"
    );
    assert_eq!(
        motion_sensitivity_in_force_for(INHERITING_CAMERA),
        SITE_SENSITIVITY,
        "and the camera nobody touched is exactly where it was"
    );
}
