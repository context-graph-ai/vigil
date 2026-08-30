//! What the settings surface says after automatic management moves the
//! detector.
//!
//! Automatic management chooses the detection backend, and when its background
//! check proves a faster one the running detector moves onto it live. From
//! that moment the operator surface has one job: agree with the detector the
//! frames actually go through. A surface that still names the old backend as
//! the automatic choice, or shows the backend already running as something
//! waiting for a restart, is describing a machine that does not exist.
//!
//! Both halves are read out of the ordinary operator report, because that is
//! the thing an operator reads.

use vigil::settings_backends::DETECTION_BACKEND_SETTING;
use vigil::settings_model::SettingValue;
use vigil::settings_projection::{record_running, report_by_direct_read};
use vigil::settings_store::SettingsStore;

/// The backend the background check proved and moved the detector onto.
const PROVED_BACKEND: &str = "burn-wgpu";

fn a_started_deployment() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let _store = SettingsStore::open(directory.path()).expect("open the settings store");
    directory
}

/// The store this fixture's deployment actually owns — the file
/// `a_started_deployment` just created, named rather than left to be derived,
/// so the report below is read from that deployment and no other.
fn store_of(directory: &tempfile::TempDir) -> std::path::PathBuf {
    SettingsStore::store_path(directory.path())
}

#[test]
fn the_automatic_choice_names_the_backend_the_detector_is_running() {
    // Unfakeable because the running fact is recorded exactly the way the
    // runtime records it, and the answer is read from the operator report
    // rather than from the registry the test wrote: a report that rereads a
    // stored resolution and ignores the live fact fails here with the old
    // name, which is precisely what a person was shown.
    let directory = a_started_deployment();
    record_running(
        DETECTION_BACKEND_SETTING,
        SettingValue::text(PROVED_BACKEND),
    );

    let report = report_by_direct_read(directory.path(), &store_of(&directory))
        .expect("read the operator report");
    let setting = report
        .settings
        .iter()
        .find(|setting| setting.setting == DETECTION_BACKEND_SETTING)
        .expect("the report must carry the detection backend");

    assert_eq!(
        setting.running.as_ref(),
        Some(&SettingValue::text(PROVED_BACKEND)),
        "the report must name the backend the detector is running"
    );
    assert_eq!(
        setting.requested,
        SettingValue::text(PROVED_BACKEND),
        "once automatic management has moved the detector, the choice it reports IS the \
         backend now running — reporting the pre-move choice describes a machine nobody has"
    );
    assert_eq!(
        setting.pending, None,
        "a backend already running is not waiting for a restart"
    );
}

#[test]
fn the_domain_view_agrees_with_the_running_detector() {
    // The same fact reached a second way. The domain view is a separate read
    // path over the same authority, and the smoke failure was exactly this:
    // one path updated, the other still answering from the old resolution, so
    // the operator got two different answers on one screen.
    let directory = a_started_deployment();
    record_running(
        DETECTION_BACKEND_SETTING,
        SettingValue::text(PROVED_BACKEND),
    );

    let report = report_by_direct_read(directory.path(), &store_of(&directory))
        .expect("read the operator report");
    let choice = report
        .domains
        .iter()
        .flat_map(|domain| domain.members.iter())
        .find(|member| member.setting == DETECTION_BACKEND_SETTING)
        .expect("the detection domain must carry the backend it governs");

    assert_eq!(
        choice.current_choice,
        SettingValue::text(PROVED_BACKEND),
        "the domain view and the setting line are one answer; a stale choice here is the \
         same lie told twice"
    );
}

#[test]
fn the_reason_moves_onto_the_promoted_backend_with_the_choice() {
    // Unfakeable because the value and the reason are read out of the same
    // report line and checked against each other: the choice names the
    // promoted backend, so a reason still saying no accelerated backend has
    // proved itself is the surface contradicting itself inside one line. A
    // reason held as a constant beside a value read from the running registry
    // fails here, which is exactly what an operator was shown after a real
    // promotion.
    let directory = a_started_deployment();
    record_running(
        DETECTION_BACKEND_SETTING,
        SettingValue::text(PROVED_BACKEND),
    );

    let report = report_by_direct_read(directory.path(), &store_of(&directory))
        .expect("read the operator report");
    let setting = report
        .settings
        .iter()
        .find(|setting| setting.setting == DETECTION_BACKEND_SETTING)
        .expect("the report must carry the detection backend");

    assert_eq!(
        setting.requested,
        SettingValue::text(PROVED_BACKEND),
        "the precondition of this contract: the choice has moved onto the promoted backend"
    );
    assert!(
        !setting
            .reason
            .contains("no accelerated detection backend has proved itself"),
        "a backend that has been promoted and is running has proved itself, and the reason \
         beside it may not say otherwise: {:?}",
        setting.reason
    );
    assert!(
        setting.reason.contains(PROVED_BACKEND),
        "the reason has to name the backend the choice moved onto: {:?}",
        setting.reason
    );
}
