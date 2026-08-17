//! Acceleration intent configuration: `hardware_decoding` and
//! `accelerated_detection` default to true, are understood by every config
//! entry point, and absence means true. The booleans are INTENT — true means
//! "probe and use only if a real probe succeeds", never "report active".
//!
//! The two variables that once set these booleans (VIGIL_HARDWARE_DECODING /
//! VIGIL_ACCELERATED_DETECTION) no longer steer anything — the environment is
//! not a settings surface — and one test here sets them to prove exactly that.
//! Every test still clears them, because `cargo test`'s default parallelism
//! runs these as threads sharing one process and a variable left set by one
//! test would make the next one prove nothing. Serialized by `ENV_LOCK` (the same
//! pattern `tests/fabric_worker_lease_knob.rs` already uses) so this binary's
//! own tests cannot race each other under plain `cargo test`; the repo gate
//! also runs under nextest, one process per test, where this race cannot fire
//! at all — the lock exists for developer-facing `cargo test` runs.

use std::ffi::OsString;
use std::fs;
use std::sync::Mutex;

use vigil::acceleration_intent_from_args;

static ENV_LOCK: Mutex<()> = Mutex::new(());

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
    let _env_lock = ENV_LOCK.lock().expect("acceleration environment lock");
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
    let _env_lock = ENV_LOCK.lock().expect("acceleration environment lock");
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
    let _env_lock = ENV_LOCK.lock().expect("acceleration environment lock");
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

/// The environment is not a settings surface: an operator who exports one of
/// these variables has said nothing about what this node does, and the two
/// acceleration switches keep the value their real surfaces gave them. The
/// variables are still recognized by NAME — that is how a run tells the
/// operator their export did nothing and where to set it instead — which is a
/// different contract, proven by its own test rather than duplicated here.
#[test]
fn environment_variables_no_longer_steer_acceleration() {
    let _env_lock = ENV_LOCK.lock().expect("acceleration environment lock");
    clear_acceleration_env();
    let tmp = tempfile::tempdir().expect("tempdir");

    // A MIXED pair, both ways round, so neither field can pass by being
    // hardwired to the answer this test wants: if either variable still
    // reached the loader, one half of one pair would come back false.
    for (hardware, detection) in [("false", "true"), ("true", "false")] {
        unsafe {
            std::env::set_var("VIGIL_HARDWARE_DECODING", hardware);
            std::env::set_var("VIGIL_ACCELERATED_DETECTION", detection);
        }
        let intent =
            acceleration_intent_from_args(args(&["--data-dir", tmp.path().to_str().unwrap()]))
                .expect("the configuration loads with the variables set");
        clear_acceleration_env();
        assert!(
            intent.hardware_decoding,
            "VIGIL_HARDWARE_DECODING={hardware} must change nothing: hardware decoding stays at \
             the value its own surfaces gave it, which with none of them speaking is true"
        );
        assert!(
            intent.accelerated_detection,
            "VIGIL_ACCELERATED_DETECTION={detection} must change nothing either — a variable that \
             still steered one of the two would be a behavior surface nobody can see on the \
             operator surface"
        );
    }
}

#[test]
fn invalid_boolean_fails_loud() {
    let _env_lock = ENV_LOCK.lock().expect("acceleration environment lock");
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
