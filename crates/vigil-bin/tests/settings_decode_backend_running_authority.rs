//! The decode backend an operator pins reaches the running machine.
//!
//! The decode backend is one of exactly two values the automatic-management
//! gate governs, and the promise made for both is the same: turn the automation
//! off and the value you set is effective, not merely recorded. A value that is
//! writable, resolvable and displayable while the running process reports
//! nothing for it leaves an operator who turned hardware decoding off with
//! nothing to set — which is the take-back promise going hollow at its last
//! step.
//!
//! Gated on the system decode stack being compiled in, because that is what
//! makes the decode backend a choice at all: a software-only artifact carries
//! one decode path, so a pin there restates the only option and proves nothing
//! about authority.
//!
//! What this proves and what it does not: the pin REACHES the running decode
//! selection and is attributed to the operator rather than to Vigil's own
//! choice. It deliberately does not assert a hardware backend running, because
//! hardware is selected only off a probe that passes on the machine in front of
//! you — a value this node cannot honor stays requested with the difference
//! reported, and manufacturing a pass here would be asserting a lie about the
//! hardware under the test.

#![cfg(feature = "decode-gstreamer")]

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

use vigil::settings_backends::{DECODE_BACKEND_SETTING, SOFTWARE_DECODE_BACKEND};
use vigil::settings_domains::HARDWARE_DECODING_DOMAIN;
use vigil::settings_model::{Author, Surface};
use vigil::settings_projection::{
    AUTHOR_KEY, NAME_KEY, NONE, RUNNING_KEY, SETTING_LINE_PREFIX, SURFACE_KEY, VALUE_KEY,
};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, TcpPortReservation, capture_pipe, vigil_binary_path, wait_until,
};

struct Deployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    config_path: PathBuf,
}

impl Deployment {
    fn prepare() -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment directory");
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&data_dir).expect("create the data directory");
        let config_path = tmp.path().join("vigil.toml");
        fs::write(&config_path, "cameras = []\n").expect("write the configuration file");
        Self {
            _tmp: tmp,
            data_dir,
            config_path,
        }
    }

    fn start(&self) -> LiveVigil {
        let health = TcpPortReservation::reserve_loopback().expect("reserve a liveness port");
        let review = TcpPortReservation::reserve_loopback().expect("reserve a review port");
        let mut command = Command::new(vigil_binary_path());
        command
            .arg("run")
            .arg("--config")
            .arg(&self.config_path)
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--health-port")
            .arg(health.port().to_string())
            .arg("--review-port")
            .arg(review.port().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn `vigil run`");
        let stdout = capture_pipe(child.stdout.take());
        let stderr = capture_pipe(child.stderr.take());
        let run = LiveVigil {
            child,
            stdout,
            stderr,
        };
        let socket_path = self.data_dir.join("control.sock");
        wait_until(
            &format!("the control socket at {} to appear", socket_path.display()),
            RUNTIME_STARTUP_TIMEOUT,
            || Ok(UnixStream::connect(&socket_path).ok().map(|_| ())),
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}. The runtime must come up and publish its control socket. Output so \
                 far:\n{}\n{}",
                run.stdout(),
                run.stderr()
            )
        });
        run
    }

    fn settings(&self, args: &[&str]) -> Output {
        Command::new(vigil_binary_path())
            .arg("settings")
            .args(args)
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .output()
            .expect("spawn `vigil settings`")
    }

    fn set(&self, setting: &str, value: &str) {
        let output = self.settings(&["set", setting, value]);
        assert!(
            output.status.success(),
            "`vigil settings set {setting} {value}` must be accepted; stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn listing(&self) -> String {
        let output = self.settings(&[]);
        assert!(
            output.status.success(),
            "`vigil settings` must answer; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }
}

struct LiveVigil {
    child: Child,
    stdout: std::sync::Arc<std::sync::Mutex<String>>,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
}

impl LiveVigil {
    fn stdout(&self) -> String {
        self.stdout.lock().expect("stdout lock").clone()
    }

    fn stderr(&self) -> String {
        self.stderr.lock().expect("stderr lock").clone()
    }

    /// The validated stop: `Child::kill` on this test's own child handle, so
    /// nothing here formats a direct or negative process-signal target.
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

fn token_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(&rest[..end])
}

fn setting_line<'a>(rendered: &'a str, name: &str) -> &'a str {
    rendered
        .lines()
        .filter(|line| line.starts_with(&format!("{SETTING_LINE_PREFIX} ")))
        .find(|line| token_field(line, NAME_KEY) == Some(name))
        .unwrap_or_else(|| panic!("no {name} line in the operator answer:\n{rendered}"))
}

/// Unfakeable in the direction that matters: the refusal has to come first and
/// name the automation, and afterwards the running field has to carry the
/// operator's value ATTRIBUTED TO THEM. A build that reported the same backend
/// as Vigil's own automatic choice fails the author assertion, and a build that
/// recorded the pin without ever bringing it into force reports nothing running
/// at all — which is the state this artifact ships in today.
#[test]
fn turning_hardware_decoding_off_makes_the_pinned_decode_backend_what_runs() {
    let deployment = Deployment::prepare();
    let run = deployment.start();

    let refusal = {
        let output = deployment.settings(&["set", DECODE_BACKEND_SETTING, SOFTWARE_DECODE_BACKEND]);
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };
    assert!(
        refusal.contains(HARDWARE_DECODING_DOMAIN),
        "the refusal names the automation managing this value, so the operator knows what to turn \
         off rather than guessing: {refusal}"
    );

    deployment.set(HARDWARE_DECODING_DOMAIN, "false");
    deployment.set(DECODE_BACKEND_SETTING, SOFTWARE_DECODE_BACKEND);

    let rendered = deployment.listing();
    let logs = format!("{}\n{}", run.stdout(), run.stderr());
    run.stop();

    let line = setting_line(&rendered, DECODE_BACKEND_SETTING);
    assert_eq!(
        token_field(line, VALUE_KEY),
        Some(SOFTWARE_DECODE_BACKEND),
        "the pin is the effective value once the automation is off: {line}"
    );
    assert_eq!(
        token_field(line, AUTHOR_KEY),
        Some(Author::LocalExplicit.as_str()),
        "and it is the OPERATOR's value, not Vigil's own choice that happens to agree: {line}"
    );
    assert_eq!(
        token_field(line, SURFACE_KEY),
        Some(Surface::VigilSettings.as_str()),
        "attributed to the surface they used: {line}"
    );
    assert_ne!(
        token_field(line, RUNNING_KEY),
        Some(NONE),
        "and the process reports which decode path it is running. Reporting nothing means the pin \
         reached no consumer, so an operator who turned hardware decoding off to take this value \
         back has taken nothing back: {line}\n\nRun output:\n{logs}"
    );
    assert_eq!(
        token_field(line, RUNNING_KEY),
        Some(SOFTWARE_DECODE_BACKEND),
        "and what runs is what they set: {line}\n\nRun output:\n{logs}"
    );
}
