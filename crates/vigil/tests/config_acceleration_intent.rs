//! Acceleration intent configuration: `hardware_decoding` and
//! `accelerated_detection` default to true, are understood by every config
//! entry point, and absence means true. The booleans are INTENT — true means
//! "probe and use only if a real probe succeeds", never "report active".
//!
//! Note on process hygiene: these tests mutate process environment variables
//! and rely on the repo gate running them under nextest (one process per
//! test).

use std::ffi::OsString;
use std::fs;

use vigil::acceleration_intent_from_args;

fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

fn clear_acceleration_env() {
    unsafe {
        std::env::remove_var("VIGIL_HARDWARE_DECODING");
        std::env::remove_var("VIGIL_ACCELERATED_DETECTION");
    }
}

#[test]
fn acceleration_booleans_default_true_everywhere() {
    clear_acceleration_env();
    let tmp = tempfile::tempdir().expect("tempdir");

    // No config at all: missing means true.
    let intent = acceleration_intent_from_args(args(&["--data-dir", tmp.path().to_str().unwrap()]))
        .expect("defaults load");
    assert!(
        intent.hardware_decoding,
        "hardware_decoding must default to true"
    );
    assert!(
        intent.accelerated_detection,
        "accelerated_detection must default to true"
    );

    // A config file that does not mention the booleans: still true.
    let config_path = tmp.path().join("vigil.toml");
    fs::write(&config_path, "site_name = \"site-a\"\n").expect("write config");
    let intent = acceleration_intent_from_args(args(&["--config", config_path.to_str().unwrap()]))
        .expect("config without the booleans loads");
    assert!(intent.hardware_decoding, "absent in TOML means true");
    assert!(intent.accelerated_detection, "absent in TOML means true");
}

#[test]
fn toml_config_can_disable_each_boolean_independently() {
    clear_acceleration_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    let config_path = tmp.path().join("vigil.toml");
    fs::write(
        &config_path,
        "hardware_decoding = false\naccelerated_detection = true\n",
    )
    .expect("write config");

    let intent = acceleration_intent_from_args(args(&["--config", config_path.to_str().unwrap()]))
        .expect("config loads");
    assert!(!intent.hardware_decoding, "explicit false is honored");
    assert!(intent.accelerated_detection, "explicit true is honored");

    fs::write(
        &config_path,
        "hardware_decoding = true\naccelerated_detection = false\n",
    )
    .expect("write config");
    let intent = acceleration_intent_from_args(args(&["--config", config_path.to_str().unwrap()]))
        .expect("config loads");
    assert!(intent.hardware_decoding);
    assert!(!intent.accelerated_detection);
}

#[test]
fn cli_flags_override_config_file() {
    clear_acceleration_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    let config_path = tmp.path().join("vigil.toml");
    fs::write(
        &config_path,
        "hardware_decoding = true\naccelerated_detection = true\n",
    )
    .expect("write config");

    let intent = acceleration_intent_from_args(args(&[
        "--config",
        config_path.to_str().unwrap(),
        "--hardware-decoding",
        "false",
        "--accelerated-detection",
        "false",
    ]))
    .expect("CLI overrides load");
    assert!(!intent.hardware_decoding, "CLI false overrides config true");
    assert!(
        !intent.accelerated_detection,
        "CLI false overrides config true"
    );
}

#[test]
fn env_vars_are_understood() {
    clear_acceleration_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    // A MIXED pair: one false, one explicitly true. This can only pass when
    // the env vars are genuinely parsed — a hardwired default in either
    // direction fails one half.
    unsafe {
        std::env::set_var("VIGIL_HARDWARE_DECODING", "false");
        std::env::set_var("VIGIL_ACCELERATED_DETECTION", "true");
    }
    let intent = acceleration_intent_from_args(args(&["--data-dir", tmp.path().to_str().unwrap()]))
        .expect("env overrides load");
    clear_acceleration_env();
    assert!(!intent.hardware_decoding, "VIGIL_HARDWARE_DECODING=false");
    assert!(
        intent.accelerated_detection,
        "VIGIL_ACCELERATED_DETECTION=true is honored, not defaulted"
    );

    // And the swapped pair, so neither field can be hardwired.
    unsafe {
        std::env::set_var("VIGIL_HARDWARE_DECODING", "true");
        std::env::set_var("VIGIL_ACCELERATED_DETECTION", "false");
    }
    let intent = acceleration_intent_from_args(args(&["--data-dir", tmp.path().to_str().unwrap()]))
        .expect("env overrides load");
    clear_acceleration_env();
    assert!(intent.hardware_decoding, "VIGIL_HARDWARE_DECODING=true");
    assert!(
        !intent.accelerated_detection,
        "VIGIL_ACCELERATED_DETECTION=false"
    );
}

#[test]
fn invalid_boolean_fails_loud() {
    clear_acceleration_env();
    let tmp = tempfile::tempdir().expect("tempdir");
    let config_path = tmp.path().join("vigil.toml");
    fs::write(&config_path, "hardware_decoding = \"maybe\"\n").expect("write config");

    let error = acceleration_intent_from_args(args(&["--config", config_path.to_str().unwrap()]))
        .expect_err("a non-boolean intent value must fail loud, never default silently");
    assert!(
        error.contains("hardware_decoding"),
        "the error names the offending option: {error}"
    );
}
