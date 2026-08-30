//! Every setting the operator surface lists says, on its own line, when it
//! takes effect and what closes the gap when it has not yet.
//!
//! The surface answers requested, running and pending side by side. A person
//! reading that answer is deciding one thing: do I wait, or do I restart the
//! thing watching my property? A pending field alone cannot tell them, because
//! pending says only that requested and running differ — not whether this
//! machine will close the gap by itself.
//!
//! So the timing is READ OFF THE RENDERED LINE here, never off the declaration
//! behind it. A classification no surface carries answers nobody: it would let
//! the whole roster be declared correctly and still leave every operator
//! guessing. Every assertion below observes the listing a real store and the
//! real projection produce.

use vigil::settings_backends::{DECODE_BACKEND_SETTING, DETECTION_BACKEND_SETTING};
use vigil::settings_domains::{ACCELERATED_DETECTION_DOMAIN, HARDWARE_DECODING_DOMAIN};
use vigil::settings_model::{
    DETECTOR_CONFIDENCE_THRESHOLD_SETTING, DETECTOR_MODEL_PATH_SETTING,
    DETECTOR_QUEUE_CAPACITY_SETTING, DETECTOR_SAMPLE_FRAMES_SETTING,
    DETECTOR_STATIONARY_INTERVAL_SETTING, HEALTH_PORT_SETTING, MOTION_SENSITIVITY_SETTING,
    PendingCause, RECOGNITION_SPACE_ID_SETTING, RECOGNITION_WEIGHTS_DIR_SETTING,
    REVIEW_PORT_SETTING, RTSP_URL_SETTING, ScopeTarget,
};
use vigil::settings_projection::{
    NAME_KEY, PENDING_KEY, SETTING_LINE_PREFIX, report_by_direct_read_at,
};
use vigil::settings_store::SettingsStore;

const NODE: &str = "node-a";

/// The key the rendered setting line carries the timing under, and the two
/// tokens it can hold. Spelled here as the operator reads them: a field value
/// is one whitespace-free token, and the two answers a person acts on
/// differently never share a spelling.
const APPLIES_KEY: &str = "applies";
const LIVE_TOKEN: &str = "live";
const NEXT_RESTART_TOKEN: &str = "next-restart";

/// The settings the per-setting roster classifies as taking effect on the
/// running machine: the two automatic-management switches with the backends
/// they govern, the analysis-rate group, and each camera's motion sensitivity.
/// Every other setting the surface answers for waits for the next start.
const ROSTER_LIVE_SETTINGS: &[&str] = &[
    ACCELERATED_DETECTION_DOMAIN,
    DETECTION_BACKEND_SETTING,
    HARDWARE_DECODING_DOMAIN,
    DECODE_BACKEND_SETTING,
    DETECTOR_SAMPLE_FRAMES_SETTING,
    DETECTOR_STATIONARY_INTERVAL_SETTING,
    DETECTOR_CONFIDENCE_THRESHOLD_SETTING,
    DETECTOR_QUEUE_CAPACITY_SETTING,
    MOTION_SENSITIVITY_SETTING,
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
/// anything — which carries the full declared roster, since the automatic floor
/// is present from the first start. Rendered through the one projection every
/// answer goes through, so what is asserted here is what an operator reads.
fn rendered_surface() -> (tempfile::TempDir, Vec<String>) {
    let directory = tempfile::tempdir().expect("temporary data directory");
    // Opening the store is what makes this a deployment with a surface to
    // read; the report is then built by the production direct read.
    let store = SettingsStore::open(directory.path()).expect("open the node-side settings store");
    // Taken off the handle that created it, so the report is read from the very
    // file this fixture owns.
    let store_path = store.path().to_path_buf();
    drop(store);
    let report = report_by_direct_read_at(directory.path(), &store_path, &target())
        .expect("build the operator report for this deployment");
    let lines = report.render_lines();
    (directory, lines)
}

/// The rendered line one setting is answered on.
fn setting_line<'a>(lines: &'a [String], setting: &str) -> &'a str {
    let prefix = format!("{SETTING_LINE_PREFIX} {NAME_KEY}={setting} ");
    lines
        .iter()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| {
            panic!("the operator surface answers for `{setting}`, so it renders a line for it")
        })
        .as_str()
}

/// One whitespace-free field off a rendered line.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let key = format!("{key}=");
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(key.as_str()))
}

/// Every setting the surface renders a line for.
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
fn every_setting_the_surface_answers_for_declares_when_it_takes_effect() {
    // Unfakeable as a set: the settings checked are the ones the surface
    // actually rendered, never a list retyped here, so a setting added later
    // without a timing on its line fails here rather than leaving one operator
    // in front of a value they cannot tell is in force.
    let (_directory, lines) = rendered_surface();
    let settings = answered_settings(&lines);
    assert!(
        settings.len() > 10,
        "sanity: the surface renders the declared roster rather than a handful of records; got \
         {settings:?}"
    );
    for setting in settings {
        let line = setting_line(&lines, &setting);
        let applies = field(line, APPLIES_KEY).unwrap_or_else(|| {
            panic!(
                "the line for `{setting}` does not say whether the value applies live or waits \
                 for the next restart, so an operator reading it cannot tell whether what they \
                 just set is in force: {line}"
            )
        });
        assert!(
            applies == LIVE_TOKEN || applies == NEXT_RESTART_TOKEN,
            "`{setting}` renders `{APPLIES_KEY}={applies}`, which is neither of the two answers a \
             person acts on differently: {line}"
        );
        assert!(
            field(line, PENDING_KEY).is_some(),
            "the line for `{setting}` carries no pending field, so a value waiting to take effect \
             tells the operator to wait without saying what for: {line}"
        );
    }
}

#[test]
fn the_values_the_take_back_promise_covers_are_declared_live() {
    // Unfakeable because it names the values the take-back promise is actually
    // made of: turning the automation off and pinning a backend, and changing
    // the analysis rate while the machine is under load. A surface that showed
    // any of these as waiting for the next restart would be telling the owner
    // of a machine that cannot keep up to restart the thing watching their
    // property.
    let (_directory, lines) = rendered_surface();
    for setting in [
        DETECTION_BACKEND_SETTING,
        ACCELERATED_DETECTION_DOMAIN,
        DETECTOR_SAMPLE_FRAMES_SETTING,
        DETECTOR_STATIONARY_INTERVAL_SETTING,
        DETECTOR_QUEUE_CAPACITY_SETTING,
        MOTION_SENSITIVITY_SETTING,
    ] {
        let line = setting_line(&lines, setting);
        assert_eq!(
            field(line, APPLIES_KEY),
            Some(LIVE_TOKEN),
            "`{setting}` takes effect on the running machine, and the operator's own line is \
             where that is said: {line}"
        );
    }
}

#[test]
fn a_value_a_start_can_only_take_on_at_startup_is_declared_restart_pending() {
    // Unfakeable because these are the values a start genuinely reads before
    // the store can answer, or binds once and cannot rebind. A line claiming
    // live application for one of them would report a change as running when
    // nothing changed; the conservative direction is the honest one, and the
    // operator reads it beside the remedy that closes the gap.
    let (_directory, lines) = rendered_surface();
    for setting in [
        HEALTH_PORT_SETTING,
        REVIEW_PORT_SETTING,
        RECOGNITION_WEIGHTS_DIR_SETTING,
        RECOGNITION_SPACE_ID_SETTING,
    ] {
        let line = setting_line(&lines, setting);
        assert_eq!(
            field(line, APPLIES_KEY),
            Some(NEXT_RESTART_TOKEN),
            "`{setting}` is taken on at startup, so the line says the value waits for the next \
             one: {line}"
        );
        assert_eq!(
            field(line, PENDING_KEY),
            Some(PendingCause::Restart.as_str()),
            "and the same line names a restart as what closes the gap: {line}"
        );
    }
}

#[test]
fn the_cause_a_pending_value_names_is_the_one_that_actually_closes_its_gap() {
    // Superseded contract note: this test used to require a camera reconnect as
    // the cause for the stream endpoints and a model reload for the detector
    // model, on the reading that a per-setting cause beats one blanket word.
    // The per-setting part still binds; what was wrong is which causes are
    // available. Naming a reconnect or a reload is an INSTRUCTION — it tells the
    // operator this machine closes the gap by itself and they should not restart
    // — and this build performs neither transition: both endpoints and the model
    // are read as the process starts and by nothing else, so the person told to
    // wait waits forever. A cause may only name a transition the code performs,
    // so every setting the roster classifies as taking effect at the next start
    // names that start.
    //
    // Unfakeable because it is driven from the roster the surface itself
    // answers for rather than a handful of named settings, and both fields of
    // each line are asserted together: a build that quietly reclassified a
    // startup-read setting as live to escape the cause fails the timing leg, one
    // that stops answering for a setting fails the coverage leg below, and the
    // two withdrawn causes are checked for by their own spelling across every
    // line, so restoring the wording ahead of the behavior fails wherever it is
    // put back.
    let (_directory, lines) = rendered_surface();
    let settings = answered_settings(&lines);

    for setting in &settings {
        let line = setting_line(&lines, setting);
        let applies = field(line, APPLIES_KEY)
            .unwrap_or_else(|| panic!("the line for `{setting}` must say when it takes effect"));
        let expected_applies = if ROSTER_LIVE_SETTINGS.contains(&setting.as_str()) {
            LIVE_TOKEN
        } else {
            NEXT_RESTART_TOKEN
        };
        assert_eq!(
            applies, expected_applies,
            "`{setting}` is classified {expected_applies} by the per-setting roster, and the \
             cause below is only truthful for the classification it carries: {line}"
        );
        assert_eq!(
            field(line, PENDING_KEY),
            Some(PendingCause::Restart.as_str()),
            "a restart is the only transition this build performs to close a gap, so it is what \
             `{setting}` names: {line}"
        );
    }

    // The two causes that describe transitions worth having and that no seam
    // performs yet: absent from every line, by their own rendered spelling.
    for withdrawn in [PendingCause::CameraReconnect, PendingCause::ModelReload] {
        let offenders: Vec<&String> = lines
            .iter()
            .filter(|line| field(line, PENDING_KEY) == Some(withdrawn.as_str()))
            .collect();
        assert!(
            offenders.is_empty(),
            "`{}` names a transition no seam performs, so no setting may render it until one \
             exists: {offenders:?}",
            withdrawn.as_str()
        );
    }

    // Coverage, so the loop above cannot be satisfied by answering for less: the
    // settings whose causes this test was authored about are still on the
    // surface, and still classified as waiting for the next start.
    for setting in [RTSP_URL_SETTING, DETECTOR_MODEL_PATH_SETTING] {
        assert!(
            settings.iter().any(|answered| answered == setting),
            "the surface must still answer for `{setting}`, got {settings:?}"
        );
        assert_eq!(
            field(setting_line(&lines, setting), APPLIES_KEY),
            Some(NEXT_RESTART_TOKEN),
            "`{setting}` is read once, as this process starts"
        );
    }
}

#[test]
fn a_domain_switch_and_the_backend_it_governs_take_effect_at_the_same_moment() {
    // The two domain switches and the two backends they govern are answered
    // together, because a switch that applies live while the value it governs
    // waits for a restart leaves an operator watching the automation turn off
    // and nothing else happen. Read off the surface, that is two lines that
    // must agree.
    let (_directory, lines) = rendered_surface();
    for (switch, governed) in [
        (ACCELERATED_DETECTION_DOMAIN, DETECTION_BACKEND_SETTING),
        (HARDWARE_DECODING_DOMAIN, DECODE_BACKEND_SETTING),
    ] {
        let switch_line = setting_line(&lines, switch);
        let governed_line = setting_line(&lines, governed);
        assert_eq!(
            field(switch_line, APPLIES_KEY),
            field(governed_line, APPLIES_KEY),
            "`{switch}` and `{governed}` take effect at the same moment, or turning the \
             automation off leaves the operator with a value they cannot yet set and no \
             statement of why:\n{switch_line}\n{governed_line}"
        );
        assert_eq!(
            field(switch_line, APPLIES_KEY),
            Some(LIVE_TOKEN),
            "and the moment they share is now, not the next start: {switch_line}"
        );
    }
}
