//! An environment variable naming a behavior setting is reported as ignored
//! ON THE SURFACE THE OPERATOR IS LOOKING AT — not merely computed inside a
//! library nobody calls.
//!
//! `docs/configuration.md` promises that exporting `VIGIL_HARDWARE_DECODING` or
//! any other behavior variable "is reported as ignored, with the place to set it
//! instead". The report is built for every name on the roster and then filtered
//! down to the single retired variable before anything prints it, so an
//! operator who set one gets silence — and silence is the one answer this
//! model exists to refuse: they believe their deployment is wired the way they
//! wrote it, and every later diagnosis starts from that false premise.
//!
//! The existing witness calls `ignored_behavior_variables()` as a library
//! function, so it cannot see whether anything is wired to a command's output.
//! Everything here is read from the two streams of a real process instead.
//!
//! What must NOT change is the answer itself: the report is an advisory about
//! the operator's environment, never the reply to their question, so the
//! listing stays on standard output, exit status stays 0, and the variable
//! stays genuinely ignored — the value it named must not appear as the running
//! or requested value of the setting it named.

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use vigil::settings_model::DETECTOR_SAMPLE_FRAMES_SETTING;
use vigil::settings_projection::{NAME_KEY, REQUESTED_KEY, SETTING_LINE_PREFIX};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::vigil_binary_path;

/// A behavior variable an operator plausibly exports, named in the promise
/// itself.
const SWITCH_VARIABLE: &str = "VIGIL_HARDWARE_DECODING";

/// A second one, carrying a value that would be visible in the listing if it
/// were honored — so "ignored" is proven and not just claimed.
const RATE_VARIABLE: &str = "VIGIL_DETECTOR_SAMPLE_FRAMES";
const RATE_VALUE: &str = "9";

fn settings(data_dir: &Path, environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("settings")
        .env("VIGIL_DATA_DIR", data_dir)
        .env_remove("VIGIL_STORE_PATH")
        .env_remove("VIGIL_CONTROL_SOCKET")
        .env_remove(SWITCH_VARIABLE)
        .env_remove(RATE_VARIABLE)
        .stdin(Stdio::null());
    for (name, value) in environment {
        command.env(name, value);
    }
    command.output().expect("spawn `vigil settings`")
}

fn setting_line<'a>(rendered: &'a str, name: &str) -> Option<&'a str> {
    rendered
        .lines()
        .filter(|line| line.starts_with(&format!("{SETTING_LINE_PREFIX} ")))
        .find(|line| {
            let needle = format!(" {NAME_KEY}={name} ");
            line.contains(&needle) || line.ends_with(&format!(" {NAME_KEY}={name}"))
        })
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Unfakeable because the variables are set on a real child process and the
/// report is read from that process's own standard error: a library function
/// nobody calls cannot produce this, which is exactly the gap that let the
/// promise ship unwired.
#[test]
fn a_behavior_variable_in_the_environment_is_reported_as_ignored_with_where_to_set_it() {
    let tmp = tempfile::tempdir().expect("temporary deployment directory");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create the deployment directory");

    let answer = settings(
        &data_dir,
        &[(SWITCH_VARIABLE, "false"), (RATE_VARIABLE, RATE_VALUE)],
    );
    let stdout = String::from_utf8_lossy(&answer.stdout).to_string();
    let stderr = String::from_utf8_lossy(&answer.stderr).to_string();

    for variable in [SWITCH_VARIABLE, RATE_VARIABLE] {
        assert!(
            stderr.contains(variable),
            "the operator set {variable} and this deployment did nothing with it; a value that \
             quietly does nothing is the worst answer available, so the report has to reach them. \
             standard error was:\n{stderr}\nstandard output was:\n{stdout}"
        );
    }
    assert!(
        stderr.contains("settings"),
        "and it names where to set it instead — a store surface, so the operator has somewhere to \
         go: {stderr}"
    );

    assert_eq!(
        answer.status.code(),
        Some(0),
        "the report is an advisory about the environment, never a refusal of the question: \
         standard error was:\n{stderr}"
    );
    let line = setting_line(&stdout, DETECTOR_SAMPLE_FRAMES_SETTING).unwrap_or_else(|| {
        panic!("the listing is still the answer, on standard output; got:\n{stdout}")
    });
    assert_ne!(
        field(line, REQUESTED_KEY),
        Some(RATE_VALUE),
        "and the variable really is ignored — a value the environment named must not become what \
         this deployment asks for: {line}"
    );
}

/// The report describes what is present RIGHT NOW rather than reciting a
/// roster: an operator who set nothing is told nothing.
#[test]
fn a_deployment_with_no_such_variable_set_is_told_nothing() {
    let tmp = tempfile::tempdir().expect("temporary deployment directory");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create the deployment directory");

    let answer = settings(&data_dir, &[]);
    let stderr = String::from_utf8_lossy(&answer.stderr).to_string();

    assert!(
        stderr.trim().is_empty(),
        "nothing was ignored, so there is nothing to report; standard error carried:\n{stderr}"
    );
}
