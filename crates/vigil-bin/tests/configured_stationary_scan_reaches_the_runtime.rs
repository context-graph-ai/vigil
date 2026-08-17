//! A stationary-scan interval steered through the configuration file is the
//! one the runtime comes up on.
//!
//! Tests that used to steer this value through the environment now write it
//! into the configuration file instead. That migration is only worth anything
//! if the new steering path is load-bearing: a helper that wrote the file and
//! never reached the process would leave every one of those tests passing on
//! the default, proving nothing while looking green.
//!
//! This is the negative control for that. It configures a value that is NOT the
//! default and asserts the running process announces that value — so a steering
//! path that silently no-ops fails here, loudly, in one cheap test.
//!
//! Unfakeable because the configured value is compared against BOTH the value
//! that was asked for and the default it has to differ from. A runtime that
//! ignores the file announces the default and fails; a test fixture that
//! "passes" by asking for the default is rejected by the second assertion
//! before it can prove nothing quietly.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, TcpPortReservation, capture_pipe, vigil_binary_path, wait_until,
};

use vigil::settings_model::DETECTOR_STATIONARY_INTERVAL_SETTING;

/// The startup receipt naming the interval the process came up on.
fn announced_prefix() -> String {
    format!("{DETECTOR_STATIONARY_INTERVAL_SETTING}=")
}

/// The value this deployment asks for: periodic re-scanning off. Chosen because
/// it is both the ratified off value and unmistakably not the default.
const CONFIGURED_INTERVAL: u64 = 0;

/// The interval a deployment that was never steered comes up on. Spelled here
/// so that a runtime ignoring the configuration file is caught by name rather
/// than by a value that might coincide with what was asked for.
const DEFAULT_INTERVAL: u64 = 30;

struct Deployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    config_path: PathBuf,
}

impl Deployment {
    /// A deployment whose configuration file carries the interval, written the
    /// same way the migrated helpers write it.
    fn with_stationary_interval(secs: u64) -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment directory");
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&data_dir).expect("create the data directory");
        let config_path = tmp.path().join("vigil.toml");
        fs::write(
            &config_path,
            format!(
                "cameras = []\nsite_name = \"home farm\"\n\
                 {DETECTOR_STATIONARY_INTERVAL_SETTING} = {secs}\n"
            ),
        )
        .expect("write the configuration file");
        Self {
            _tmp: tmp,
            data_dir,
            config_path,
        }
    }

    fn start(&self) -> LiveVigil {
        LiveVigil::spawn(&self.config_path, &self.data_dir)
    }
}

struct LiveVigil {
    child: Child,
    stdout: Arc<Mutex<String>>,
}

impl LiveVigil {
    fn spawn(config_path: &Path, data_dir: &Path) -> Self {
        let health = TcpPortReservation::reserve_loopback().expect("reserve a liveness port");
        let review = TcpPortReservation::reserve_loopback().expect("reserve a review port");
        let mut command = Command::new(vigil_binary_path());
        command
            .arg("run")
            .arg("--config")
            .arg(config_path)
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--health-port")
            .arg(health.port().to_string())
            .arg("--review-port")
            .arg(review.port().to_string())
            // The environment read this value used to arrive through is cleared,
            // so the configuration file is demonstrably the path under test
            // rather than one of two possible sources.
            .env_remove("VIGIL_DETECTOR_STATIONARY_INTERVAL_SECS")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn vigil run");
        let stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());
        Self { child, stdout }
    }

    /// The interval the process announces it came up on.
    fn announced_interval(&self) -> u64 {
        let prefix = announced_prefix();
        wait_until(
            "the runtime to state the stationary-scan interval it came up on",
            RUNTIME_STARTUP_TIMEOUT,
            || {
                let captured = self.stdout.lock().expect("stdout buffer").clone();
                Ok(captured
                    .lines()
                    .find_map(|line| line.strip_prefix(prefix.as_str()))
                    .and_then(|value| value.trim().parse::<u64>().ok()))
            },
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}. Output so far:\n{}",
                self.stdout.lock().expect("stdout buffer")
            )
        })
    }

    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LiveVigil {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_stationary_scan_interval_written_to_the_configuration_file_is_what_the_runtime_runs() {
    assert_ne!(
        CONFIGURED_INTERVAL, DEFAULT_INTERVAL,
        "this control only means something if the configured value differs from the default; \
         asking for the default would let a runtime that ignores the file pass"
    );

    let deployment = Deployment::with_stationary_interval(CONFIGURED_INTERVAL);
    let run = deployment.start();
    let announced = run.announced_interval();
    run.stop();

    assert_eq!(
        announced, CONFIGURED_INTERVAL,
        "the interval written to the configuration file is the one the process came up on. If \
         steering through the file does not reach the runtime, every test that moved off the \
         environment onto it is asserting against a default nobody set — passing while proving \
         nothing about the behavior it names. Announced {announced}, configured \
         {CONFIGURED_INTERVAL}"
    );
    assert_ne!(
        announced, DEFAULT_INTERVAL,
        "and it is demonstrably not the default: that is the whole point of the control"
    );
}
