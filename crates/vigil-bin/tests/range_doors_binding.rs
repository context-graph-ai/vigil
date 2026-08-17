//! Binds the settings store's declared write-time range for a value to the
//! config loader's own accepted range for the SAME setting, at the exact
//! boundary — driven through the real compiled `vigil` binary's `doctor
//! acceleration` subcommand, which runs `config::load` and exits before any
//! camera or store setup, so this is a fast, cheap way to observe the
//! loader's own refusal (`config.rs::validate_detector_sample_frames` /
//! `validate_confidence_threshold` / `validate_recognition_threshold`)
//! without spinning up a runtime.
//!
//! The settings-store half of the SAME boundary is pinned independently in
//! `crates/vigil/tests/detector_class_emptiness_and_rate_range_gaps.rs`
//! (`declared_range`/`declared_float_range` in `settings_backends.rs` are
//! private to that crate, so this file cannot call them directly — it drives
//! the loader from the outside instead). Together the two files prove "the
//! two doors agree" rather than merely stating it in a doc comment
//! (cold-review-arc2-r5 finding 7): each pins the SAME literal boundary
//! value independently, so narrowing either door's bound alone moves one
//! half of the pin off that literal and fails — the store-side test if the
//! store narrows, this file if the loader narrows.

use std::path::Path;
use std::process::Command;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::vigil_binary_path;

/// Runs `vigil doctor acceleration` with the given extra arguments against a
/// fresh, per-call data directory (so no state or prior run interferes), and
/// returns `(exit_success, stdout+stderr combined)`. `doctor acceleration`
/// calls `config::load` and returns its error (never panicking or trying to
/// open a camera) before doing anything else, so a refused config value
/// surfaces here exactly as it would on `vigil run`, without the wait for a
/// real runtime boot.
fn doctor_acceleration(data_dir: &Path, extra_args: &[&str]) -> (bool, String) {
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("doctor")
        .arg("acceleration")
        .arg("--data-dir")
        .arg(data_dir);
    for arg in extra_args {
        command.arg(arg);
    }
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("spawn `vigil doctor acceleration`: {error}"));
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), combined)
}

/// The loader's own ceiling for `detector_sample_frames`
/// (`config.rs::validate_detector_sample_frames`) is exactly 64 — matching
/// the settings store's `declared_range(DETECTOR_SAMPLE_FRAMES_SETTING)` —
/// pinned at both the accepted boundary and one past it. Narrow the
/// loader's `1..=64` to, say, `1..=32` without touching the store, and 64
/// stops being accepted here while the store-side test (which still writes
/// 64 successfully) keeps passing — this is what makes that drift visible
/// with every test green otherwise.
#[test]
fn detector_sample_frames_loader_ceiling_matches_the_declared_boundary() {
    let tmp = tempfile::tempdir().expect("temporary data directory");

    let (accepted, _) = doctor_acceleration(
        &tmp.path().join("at-ceiling"),
        &["--detector-sample-frames", "64"],
    );
    assert!(
        accepted,
        "64 is exactly the loader's declared ceiling and must be accepted"
    );

    let (refused, output) = doctor_acceleration(
        &tmp.path().join("past-ceiling"),
        &["--detector-sample-frames", "65"],
    );
    assert!(
        !refused,
        "65 is one past the loader's declared ceiling and must be refused; got: {output}"
    );
    assert!(
        output.contains("64"),
        "the loader's refusal must name its own ceiling of 64, matching the settings store's \
         declared_range(DETECTOR_SAMPLE_FRAMES_SETTING): {output}"
    );
}

/// The loader's own ceiling for `detector_confidence_threshold`
/// (`config.rs::validate_confidence_threshold`) is exactly `1.0` — matching
/// the settings store's `declared_float_range` for the same setting.
#[test]
fn detector_confidence_threshold_loader_ceiling_matches_the_declared_boundary() {
    let tmp = tempfile::tempdir().expect("temporary data directory");

    let (accepted, _) = doctor_acceleration(
        &tmp.path().join("at-ceiling"),
        &["--detector-confidence-threshold", "1.0"],
    );
    assert!(
        accepted,
        "1.0 is exactly the loader's declared ceiling and must be accepted"
    );

    let (refused, output) = doctor_acceleration(
        &tmp.path().join("past-ceiling"),
        &["--detector-confidence-threshold", "1.01"],
    );
    assert!(
        !refused,
        "1.01 is one hundredth past the loader's declared ceiling and must be refused; got: \
         {output}"
    );
    assert!(
        output.contains("1.0"),
        "the loader's refusal must name its own decimal ceiling of 1.0, matching the settings \
         store's declared_float_range: {output}"
    );
}

/// `recognition_threshold` has no CLI flag (only `detector_confidence_threshold`
/// does); it is reached the way an operator's `--config` file reaches it, via
/// `config::load`'s TOML branch. Same boundary, same binding, same reason.
#[test]
fn recognition_threshold_loader_ceiling_matches_the_declared_boundary() {
    let tmp = tempfile::tempdir().expect("temporary data directory");

    let at_ceiling_config = tmp.path().join("at-ceiling.toml");
    std::fs::write(&at_ceiling_config, "recognition_threshold = 1.0\n")
        .expect("write at-ceiling config");
    let (accepted, _) = doctor_acceleration(
        &tmp.path().join("at-ceiling-data"),
        &["--config", at_ceiling_config.to_str().expect("utf-8 path")],
    );
    assert!(
        accepted,
        "1.0 is exactly the loader's declared ceiling and must be accepted"
    );

    let past_ceiling_config = tmp.path().join("past-ceiling.toml");
    std::fs::write(&past_ceiling_config, "recognition_threshold = 1.01\n")
        .expect("write past-ceiling config");
    let (refused, output) = doctor_acceleration(
        &tmp.path().join("past-ceiling-data"),
        &[
            "--config",
            past_ceiling_config.to_str().expect("utf-8 path"),
        ],
    );
    assert!(
        !refused,
        "1.01 is one hundredth past the loader's declared ceiling and must be refused; got: \
         {output}"
    );
    assert!(
        output.contains("1.0"),
        "the loader's refusal must name its own decimal ceiling of 1.0, matching the settings \
         store's declared_float_range: {output}"
    );
}
