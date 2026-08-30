//! The operator surface names an environment lever when one supplied the
//! running value for a setting, beside the operator's own pin it stands in
//! front of — `runtime.rs`'s only production call site is
//! `VIGIL_DETECTOR_QUEUE_CAPACITY` overriding `detector_queue_capacity`
//! (cold-review-r4 finding 14; `docs/configuration.md:425-435` now states
//! the exception). Nothing in the estate had proven the two rendered tokens
//! an operator actually reads — `running-source=` and `shadowed-setting=`
//! — ever appear when the lever is set, are absent when it is not, or that
//! `shadowed-setting` carries the operator's PIN rather than the lever's own
//! number (cold-review-arc2-r5 finding 10). This drives the real production
//! projection seam (`settings_projection::record_running_from_environment`,
//! the exact function `runtime.rs` calls) and reads the RENDERED line, not
//! the registry it populates, so a rendering regression is caught the same
//! way a registry regression would be.

use vigil::settings_model::{DETECTOR_QUEUE_CAPACITY_SETTING, ScopeTarget, SettingValue, Surface};
use vigil::settings_projection::{
    NAME_KEY, RUNNING_SOURCE_KEY, SETTING_LINE_PREFIX, SHADOWED_SETTING_KEY,
    record_running_from_environment, report_by_direct_read_at,
};
use vigil::settings_store::SettingsStore;
use vigil::{PersistedClock, settings_model::Scope};

const NODE: &str = "node-attribution";
const ENV_VARIABLE: &str = "VIGIL_DETECTOR_QUEUE_CAPACITY";
/// The operator's own pin — distinct from the lever's value below so a test
/// that accidentally read the lever's number back as the shadowed value
/// cannot pass by coincidence.
const PINNED_CAPACITY: i64 = 5_000;
/// The lever's value — the running_capacity a run driven by the environment
/// variable actually queues at, standing in front of the pin above.
const LEVER_CAPACITY: i64 = 750;

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: NODE.to_string(),
        site: NODE.to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

fn setting_line<'a>(lines: &'a [String], setting: &str) -> &'a str {
    let prefix = format!("{SETTING_LINE_PREFIX} {NAME_KEY}={setting} ");
    lines
        .iter()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("the operator surface answers for `{setting}`: {lines:#?}"))
        .as_str()
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let key = format!("{key}=");
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(key.as_str()))
}

/// Unfakeable in three directions on one rendered line, driven end to end
/// through a real store: before the lever is recorded, the line carries
/// neither token; the operator's own pin is written through `set_local`, so
/// it is a genuine record and not a bare literal threaded past the store;
/// once the lever is recorded exactly as `runtime.rs` records it, the SAME
/// line gains `running-source=environment:VIGIL_DETECTOR_QUEUE_CAPACITY`
/// and `shadowed-setting=` carrying the PIN's value, never the lever's own
/// number.
#[test]
fn the_environment_lever_names_itself_beside_the_pin_it_stands_in_front_of() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let clock = PersistedClock::from_millis_source(|| 1_700_000_000_000);
    let store = SettingsStore::open_with_clock(directory.path(), clock)
        .expect("open the node-side settings store");
    store
        .set_local(
            DETECTOR_QUEUE_CAPACITY_SETTING,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::Int(PINNED_CAPACITY),
        )
        .expect("write the operator's own pin");

    let report = report_by_direct_read_at(directory.path(), store.path(), &target())
        .expect("build the operator report for this deployment");

    let before = setting_line(&report.render_lines(), DETECTOR_QUEUE_CAPACITY_SETTING).to_string();
    assert_eq!(
        field(&before, RUNNING_SOURCE_KEY),
        None,
        "no environment lever has been recorded yet, so the line must carry no running-source: \
         {before}"
    );
    assert_eq!(
        field(&before, SHADOWED_SETTING_KEY),
        None,
        "and no shadowed-setting either, since nothing is shadowing the pin yet: {before}"
    );

    // The exact call `runtime.rs` makes when `VIGIL_DETECTOR_QUEUE_CAPACITY`
    // overrides the resolved pin for this run.
    record_running_from_environment(
        DETECTOR_QUEUE_CAPACITY_SETTING,
        ENV_VARIABLE,
        SettingValue::Int(LEVER_CAPACITY),
        Some(SettingValue::Int(PINNED_CAPACITY)),
    );

    let after = setting_line(&report.render_lines(), DETECTOR_QUEUE_CAPACITY_SETTING).to_string();
    assert_eq!(
        field(&after, RUNNING_SOURCE_KEY),
        Some(format!("environment:{ENV_VARIABLE}").as_str()),
        "once the lever is recorded, the line must name it and the variable that supplied it: \
         {after}"
    );
    assert_eq!(
        field(&after, SHADOWED_SETTING_KEY),
        Some(PINNED_CAPACITY.to_string().as_str()),
        "shadowed-setting must carry the OPERATOR'S PIN the lever stands in front of, not the \
         lever's own running value ({LEVER_CAPACITY}) reported back at itself: {after}"
    );
    assert_ne!(
        field(&after, SHADOWED_SETTING_KEY),
        Some(LEVER_CAPACITY.to_string().as_str()),
        "sanity: the pin and the lever are distinct literals in this file specifically so this \
         mistake cannot pass by coincidence: {after}"
    );
}
