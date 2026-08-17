//! The listing a never-started or unmanaged deployment answers with reports a
//! pending gap the same way every other listing does: by comparing what is
//! running against what is effective.
//!
//! This listing is the one an operator reads when the store cannot be opened —
//! exactly the moment they are trying to work out what this box is doing. A
//! pending field declared empty there says "nothing is waiting" about a machine
//! that is running a value nobody chose, while the same setting read through
//! the store would name the remedy. One of the two answers is wrong, and it is
//! the one that cannot be wrong for a reason.

use vigil::settings_application::bring_into_force;
use vigil::settings_model::{
    DETECTOR_SAMPLE_FRAMES_SETTING, MOTION_SENSITIVITY_SETTING, ScopeTarget, SettingValue,
};
use vigil::settings_store::automatic_floor_listing;

const NODE: &str = "node-a";

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: NODE.to_string(),
        site: NODE.to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

fn entry(
    listing: &[vigil::settings_model::EffectiveSetting],
    setting: &str,
) -> vigil::settings_model::EffectiveSetting {
    listing
        .iter()
        .find(|entry| entry.setting == setting)
        .unwrap_or_else(|| panic!("the floor listing answers for `{setting}`"))
        .clone()
}

#[test]
fn a_floor_listing_reports_the_gap_it_can_see_and_claims_none_where_there_is_none() {
    // Unfakeable in both directions from one listing: this process is genuinely
    // running one value that differs from the floor and one that matches it, so
    // a pending field that is declared rather than derived fails on one half
    // whichever constant it is declared as. The two settings are read from the
    // same listing, so no ordering or fixture difference separates them.
    let floor_sensitivity = entry(
        &automatic_floor_listing(&target()),
        MOTION_SENSITIVITY_SETTING,
    )
    .requested;
    let floor_sample_frames = entry(
        &automatic_floor_listing(&target()),
        DETECTOR_SAMPLE_FRAMES_SETTING,
    )
    .requested;

    let running_sensitivity = match &floor_sensitivity {
        SettingValue::Int(number) => SettingValue::Int(number + 1),
        other => panic!("the motion sensitivity floor is a whole number; got {other:?}"),
    };
    // This process takes on one value that differs from the floor, and one that
    // is exactly the floor.
    bring_into_force(MOTION_SENSITIVITY_SETTING, running_sensitivity.clone());
    bring_into_force(DETECTOR_SAMPLE_FRAMES_SETTING, floor_sample_frames.clone());

    let listing = automatic_floor_listing(&target());

    let diverged = entry(&listing, MOTION_SENSITIVITY_SETTING);
    assert_eq!(
        diverged.running.as_ref(),
        Some(&running_sensitivity),
        "the listing reports what this process is actually running"
    );
    assert!(
        diverged.pending.is_some(),
        "this process is running {running_sensitivity} where the listing's own effective value is \
         {floor_sensitivity}, so the operator is told what closes that gap rather than that there \
         is nothing to wait for; got {:?}",
        diverged.pending
    );

    let agreed = entry(&listing, DETECTOR_SAMPLE_FRAMES_SETTING);
    assert_eq!(
        agreed.running.as_ref(),
        Some(&floor_sample_frames),
        "and the second setting is genuinely running at the listing's own value"
    );
    assert_eq!(
        agreed.pending, None,
        "where running and effective agree there is nothing pending, and saying otherwise sends \
         an operator to restart a machine that is already doing what they asked; got {:?}",
        agreed.pending
    );
}
