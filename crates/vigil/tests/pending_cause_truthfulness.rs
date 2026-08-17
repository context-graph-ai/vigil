//! What a pending value tells an operator to wait for is something this build
//! actually does.
//!
//! The operator surface answers requested, running and pending side by side,
//! and a person reads the pending field to decide one thing: do I wait, or do I
//! restart the thing watching my property. Naming a camera reconnect or a model
//! reload is an instruction — it says this machine closes the gap by itself,
//! and the operator should not restart. Four settings name exactly that today
//! while nothing in the build ever re-reads them: the camera endpoints and the
//! detector model are taken on at startup and by no other transition, so the
//! person waiting for a reconnect that never re-reads anything waits forever.
//!
//! The correction is the honest direction, not a new adoption path: those
//! settings report a restart, which is what genuinely closes their gap. Every
//! assertion is read off the RENDERED line a real deployment produces, so a
//! cause that is right in the declaration and wrong on the surface fails here.

use vigil::settings_model::{
    DETECTOR_MODEL_ID_SETTING, DETECTOR_MODEL_PATH_SETTING, LIVE_RTSP_URL_SETTING, PendingCause,
    RTSP_URL_SETTING, ScopeTarget,
};
use vigil::settings_projection::{
    NAME_KEY, PENDING_KEY, SETTING_LINE_PREFIX, report_by_direct_read_at,
};
use vigil::settings_store::SettingsStore;

const NODE: &str = "node-a";

/// The key the rendered line carries the timing under, and the token that says
/// the value waits for the next start.
const APPLIES_KEY: &str = "applies";
const NEXT_RESTART_TOKEN: &str = "next-restart";

/// The four settings whose only reader is a start: the camera endpoints, which
/// are carried into the camera set as it is built, and the detector model,
/// which is loaded once as the detector is constructed.
const SETTINGS_ONLY_A_START_TAKES_ON: &[&str] = &[
    RTSP_URL_SETTING,
    LIVE_RTSP_URL_SETTING,
    DETECTOR_MODEL_PATH_SETTING,
    DETECTOR_MODEL_ID_SETTING,
];

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: NODE.to_string(),
        site: NODE.to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

/// The operator surface's own lines for a deployment where nobody has set
/// anything, which carries the full declared roster because the automatic floor
/// is present from the first start.
fn rendered_surface() -> (tempfile::TempDir, Vec<String>) {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = SettingsStore::open(directory.path()).expect("open the node-side settings store");
    drop(store);
    let report = report_by_direct_read_at(directory.path(), &target())
        .expect("build the operator report for this deployment");
    let lines = report.render_lines();
    (directory, lines)
}

fn setting_line<'a>(lines: &'a [String], setting: &str) -> &'a str {
    let prefix = format!("{SETTING_LINE_PREFIX} {NAME_KEY}={setting} ");
    lines
        .iter()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("the operator surface answers for `{setting}`"))
        .as_str()
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let key = format!("{key}=");
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(key.as_str()))
}

fn answered_settings(lines: &[String]) -> Vec<String> {
    let prefix = format!("{SETTING_LINE_PREFIX} {NAME_KEY}=");
    lines
        .iter()
        .filter_map(|line| line.strip_prefix(prefix.as_str()))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

#[test]
fn a_stream_or_model_change_names_the_restart_that_actually_closes_it() {
    // Unfakeable because both fields of the same rendered line are asserted for
    // each of the four: the line has to say the value waits for the next start
    // AND that a restart is what closes the gap. A build that quietly renamed
    // the cause while still claiming live application would fail the first leg,
    // and one that left the reconnect wording in place fails the second.
    let (_directory, lines) = rendered_surface();
    for setting in SETTINGS_ONLY_A_START_TAKES_ON {
        let line = setting_line(&lines, setting);
        assert_eq!(
            field(line, APPLIES_KEY),
            Some(NEXT_RESTART_TOKEN),
            "`{setting}` is read once, as this process starts, so the line says the value waits \
             for the next one: {line}"
        );
        assert_eq!(
            field(line, PENDING_KEY),
            Some(PendingCause::Restart.as_str()),
            "and a restart is what closes that gap — telling the operator to wait for a reconnect \
             or a reload names a transition this build never performs, so they wait forever: \
             {line}"
        );
    }
}

#[test]
fn no_setting_that_waits_for_a_restart_names_anything_else_as_what_closes_it() {
    // The general form of the same rule, over whatever the surface answers for
    // rather than a list retyped here, so a setting added later cannot
    // reintroduce the defect quietly. Unfakeable because the two fields come
    // off one line: a value the machine can only take on at the next start
    // cannot also be closed by something the machine does while running, and
    // any cause other than a restart claims exactly that.
    let (_directory, lines) = rendered_surface();
    let settings = answered_settings(&lines);
    assert!(
        settings.len() > 10,
        "sanity: the surface renders the declared roster, got {settings:?}"
    );

    let mut untruthful: Vec<String> = Vec::new();
    for setting in settings {
        let line = setting_line(&lines, &setting);
        if field(line, APPLIES_KEY) != Some(NEXT_RESTART_TOKEN) {
            continue;
        }
        let pending = field(line, PENDING_KEY).unwrap_or("");
        if pending != PendingCause::Restart.as_str() {
            untruthful.push(format!("{setting}: {pending}"));
        }
    }
    assert!(
        untruthful.is_empty(),
        "a value only the next start takes on is closed by that start and by nothing else, so any \
         other cause sends the operator to wait for something that never happens: {untruthful:?}"
    );
}
