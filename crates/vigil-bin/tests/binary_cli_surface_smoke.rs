//! Proves the compiled `vigil` binary's CLI surface — `--version`,
//! `--help`, and its exit code on missing arguments — is intact for the
//! composition root's own build. This is CLI-argument-parsing coverage
//! only: none of these three invocations configure MQTT, so none of them
//! reach `site_channel.connect`/`listen` or the real `vigil_ha::
//! HomeAssistantMqtt` adapter. That composed-wiring path (a real `vigil
//! run` process actually driving the adapter's discovery/command handling
//! against a live broker) is proven today at the function level by
//! `vigil-ha`'s `ha_mqtt_broker.rs`, not yet via a spawned `vigil-bin`
//! process; closing that gap is separate follow-on work, not this file's
//! job.
//!
//! This is also the reason `vigil-bin` carries an integration test at all:
//! only a package with an integration test gets its `[[bin]]` target linked
//! to `CARGO_BIN_EXE_vigil` (and copied to the friendly `target/<profile>/
//! vigil` path other suites' binary-path fallback resolves). A package with
//! only an inline unit test does not get this treatment. Every other test
//! suite across the workspace that spawns the `vigil` binary depends on
//! this file existing.

use std::path::PathBuf;
use std::process::Command;

fn vigil_binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_vigil"))
}

#[test]
fn version_flag_reports_core_crate_version() {
    let output = Command::new(vigil_binary_path())
        .arg("--version")
        .output()
        .expect("spawn vigil --version");
    assert!(output.status.success(), "vigil --version must exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim() == format!("vigil {}", env!("CARGO_PKG_VERSION")),
        "unexpected --version output: {stdout:?}"
    );
}

#[test]
fn help_flag_names_the_run_command() {
    let output = Command::new(vigil_binary_path())
        .arg("--help")
        .output()
        .expect("spawn vigil --help");
    assert!(output.status.success(), "vigil --help must exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("run"),
        "vigil --help must document the `run` command, got: {stdout:?}"
    );
}

#[test]
fn no_arguments_prints_help_and_exits_nonzero() {
    let output = Command::new(vigil_binary_path())
        .output()
        .expect("spawn vigil with no arguments");
    assert!(
        !output.status.success(),
        "vigil with no command must exit non-zero"
    );
}
