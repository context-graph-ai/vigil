//! `vigil settings set service_identity <x>` and `vigil settings reset
//! service_identity` are both ordinary edits aimed at this node's identity,
//! and Home Assistant sees an identity change as an entirely new device —
//! orphaning the entity history attached to the old one.
//! `crates/vigil/src/settings_store.rs`'s
//! `refuse_setting_no_ordinary_verb_may_touch` refuses both verbs through one
//! shared gate, sourced from `crate::service_identity::refuse_ordinary_edit`;
//! this file drives that SAME refusal through the real CLI surface end to
//! end, spawning the compiled `vigil` binary against a fresh, never-started
//! deployment. No runtime is up for either spawn, so the answer comes from
//! `settings_command::answer`'s direct-store path (`ask_runtime_owner`
//! fails to reach a control socket that was never opened), exactly the path
//! an operator hits running `vigil settings ...` against a stopped
//! deployment.

use std::process::{Command, Output};

use vigil::settings_model::SERVICE_IDENTITY_SETTING;
use vigil::settings_store::SettingsStore;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::vigil_binary_path;

fn run_settings(data_dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(vigil_binary_path())
        .env("VIGIL_DATA_DIR", data_dir)
        .arg("settings")
        .args(args)
        .output()
        .expect("spawn vigil settings")
}

/// The two facts every identity refusal owes, checked against one process's
/// stderr regardless of which ordinary verb produced it.
fn assert_refuses_with_the_identity_consequence(output: &Output) {
    assert!(
        !output.status.success(),
        "an ordinary edit of the identity must exit non-zero: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr
            .lines()
            .any(|line| line.trim_start().starts_with("settings-error")),
        "the refusal answers with the settings-error prefix: {stderr:?}"
    );
    assert!(
        stderr.contains("orphaned"),
        "the refusal states the orphaned-history consequence: {stderr:?}"
    );
    assert!(
        stderr.contains("deliberate identity-change operation"),
        "the refusal names the deliberate identity operation an operator would need to run \
         instead: {stderr:?}"
    );
}

/// The store this refusal ran against holds no `service_identity` record at
/// all — not the rejected value, and for a reset, not a `reset = true`
/// record dropping the identity back to its floor either.
fn assert_no_identity_record_was_written(data_dir: &std::path::Path) {
    let store = SettingsStore::open(data_dir).expect("open the store the refusal ran against");
    let records = store
        .records(SERVICE_IDENTITY_SETTING)
        .expect("read back any service_identity records");
    assert!(
        records.is_empty(),
        "a refused set or reset must leave the stored identity untouched — no record at all: \
         {records:?}"
    );
}

#[test]
fn setting_the_identity_through_the_cli_is_refused_with_the_consequence_and_changes_nothing() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let output = run_settings(
        deployment.path(),
        &["set", "service_identity", "front-door-camera"],
    );
    assert_refuses_with_the_identity_consequence(&output);
    assert_no_identity_record_was_written(deployment.path());
}

#[test]
fn resetting_the_identity_through_the_cli_is_refused_with_the_consequence_and_changes_nothing() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let output = run_settings(deployment.path(), &["reset", "service_identity"]);
    assert_refuses_with_the_identity_consequence(&output);
    assert_no_identity_record_was_written(deployment.path());
}
