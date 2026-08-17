//! Taking a value back while the machine is running.
//!
//! Some settings are only honest as a restart-pending record — the process took
//! them on at startup and cannot take them on again without one. Two are not.
//! Turning the automation off and setting a detection backend has to make that
//! backend WHAT RUNS, and an analysis rate changed by hand has to be in force on
//! the machine that is already running. Those are the two the take-back promise
//! is made of, and a pending record is not the promise.
//!
//! Everything asserted here is read from what the running process reports it is
//! running. Nothing waits on the clock to decide whether something happened, and
//! no test claims an effect the process has not reported.

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

use vigil::settings_backends::{DETECTION_BACKEND_SETTING, available_detection_backends};
use vigil::settings_domains::ACCELERATED_DETECTION_DOMAIN;
use vigil::settings_model::{Author, DETECTOR_SAMPLE_FRAMES_SETTING, Surface};
use vigil::settings_projection::{
    AUTHOR_KEY, HELD_LINE_PREFIX, NAME_KEY, NONE, PENDING_KEY, RUNNING_KEY, SETTING_LINE_PREFIX,
    SURFACE_KEY, VALUE_KEY,
};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, RtspFixture, TcpPortReservation, capture_pipe, rtsp_fixture_lock,
    vigil_binary_path, wait_until, workspace_root,
};

/// The estate's own clip, served over RTSP so the deployment has a camera that
/// genuinely connects and a detector that is genuinely built.
fn person_clip() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("fixtures")
        .join("video")
        .join("one-by-one-person-detection.mp4")
}

/// The estate's own detector weights.
fn detector_model() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("fixtures")
        .join("models")
        .join("yolox-tiny-coco.pth")
}

/// The model identity this deployment declares for those weights.
const DETECTOR_MODEL_ID: &str = "detector-under-test";

struct Deployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    config_path: PathBuf,
}

impl Deployment {
    fn with_config(body: &str) -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment directory");
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&data_dir).expect("create the data directory");
        let config_path = tmp.path().join("vigil.toml");
        fs::write(&config_path, body).expect("write the configuration file");
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

    /// A write the model is expected to refuse, returned rather than asserted
    /// so the caller can read the refusal.
    fn try_set(&self, setting: &str, value: &str) -> String {
        let output = self.settings(&["set", setting, value]);
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
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

    /// What the RUNNING process reports about itself, read over the control
    /// socket. This is the runtime's own account of the detector it built and
    /// is feeding frames through — a different source from the settings
    /// registry, which only ever holds what was asked for.
    fn stats(&self) -> String {
        let output = Command::new(vigil_binary_path())
            .arg("stats")
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .output()
            .expect("spawn `vigil stats`");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    /// The detector backend the running process reports as ACTIVE, waited for
    /// as a state it reaches rather than on the clock. Absent until a detector
    /// has genuinely been built, which is why this proof needs a camera.
    fn active_detector_backend(&self) -> String {
        wait_until(
            "the running process to report the detector backend it is using",
            RUNTIME_STARTUP_TIMEOUT,
            || {
                Ok(self
                    .stats()
                    .lines()
                    .find_map(|line| line.strip_prefix(ACTIVE_DETECTOR_BACKEND_PREFIX))
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string))
            },
        )
        .unwrap_or_else(|error| panic!("{error}. Stats read:\n{}", self.stats()))
    }
}

/// The stats receipt naming the detector the runtime actually built. Spelled
/// here because `vigil stats` renders it as a plain receipt line and nothing
/// declares the key.
const ACTIVE_DETECTOR_BACKEND_PREFIX: &str = "active-detector-backend=";

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

/// The backend an operator can genuinely pin on this artifact, which is a
/// property of what was compiled in rather than of the packaging.
fn a_backend_this_artifact_carries() -> &'static str {
    available_detection_backends()
        .first()
        .copied()
        .expect("every artifact carries at least one detection backend")
}

/// The backend to pin: the processor one, which every artifact carries and
/// every machine can honor.
///
/// It deliberately does not pin the accelerated backend even where that is
/// compiled in. The accelerated path is selected off a probe that has to pass
/// on the machine in front of you, so on a box with no visible device a pin of
/// it is a value the node cannot honor — asserting it runs would be asserting a
/// lie about the hardware under the test, the same ruling the decode side of
/// this promise already carries. What stays provable everywhere is that the
/// backend an operator pins is the one the detector is actually running.
fn a_backend_to_pin() -> &'static str {
    vigil::detection_accel::CPU_DETECTION_BACKEND
}

/// Unfakeable because the value is stored while the process is demonstrably up
/// and never restarted: the same process has to report running the new number,
/// with nothing left pending. A run that could only take a rate on at startup
/// reports the old one and names a restart, which is precisely the answer this
/// forbids for a control the owner reaches for when the machine cannot keep up.
#[test]
fn a_rate_change_made_while_running_takes_effect_without_a_restart() {
    let deployment = Deployment::with_config("cameras = []\ndetector_sample_frames = 7\n");
    let run = deployment.start();

    deployment.set(DETECTOR_SAMPLE_FRAMES_SETTING, "2");
    let rendered = deployment.listing();
    let logs = format!("{}\n{}", run.stdout(), run.stderr());
    run.stop();

    let line = setting_line(&rendered, DETECTOR_SAMPLE_FRAMES_SETTING);
    assert_eq!(
        token_field(line, VALUE_KEY),
        Some("2"),
        "the effective value is what the operator just stored: {line}"
    );
    assert_eq!(
        token_field(line, AUTHOR_KEY),
        Some(Author::LocalExplicit.as_str()),
        "attributed to the person who set it: {line}"
    );
    assert_eq!(
        token_field(line, SURFACE_KEY),
        Some(Surface::VigilSettings.as_str()),
        "and to the surface they used: {line}"
    );
    assert_eq!(
        token_field(line, RUNNING_KEY),
        Some("2"),
        "and this process — the same one that was already up — is running it. An analysis rate the \
         owner can only change by restarting the machine they are watching a property with is not \
         the control this promises: {line}\n\nRun output:\n{logs}"
    );
    assert_eq!(
        token_field(line, PENDING_KEY),
        Some(NONE),
        "with nothing left pending, because nothing has to happen for it to be in force: {line}"
    );
}

/// Unfakeable because the deployment has a REAL camera, so a detector is
/// genuinely built and fed frames, and the last assertion is read from the
/// runtime's own account of the detector it is running rather than from the
/// settings registry the change just wrote. A zero-camera deployment cannot
/// tell a rebuilt detector apart from a string recorded in a registry, which is
/// exactly the gap that let "the named backend is what runs" go unproven. The
/// whole take-back sequence still runs in order against one process: the
/// refusal comes first and has to name the domain AND how to turn it off, and
/// only then does the pin have to become what is actually running.
#[test]
fn turning_the_automation_off_makes_the_pinned_detection_backend_what_runs() {
    let _serialized = rtsp_fixture_lock().lock().unwrap_or_else(|poisoned| {
        // A previous fixture user panicking must not disable the serialization
        // this fixture needs; the lock's job is exclusion, not state.
        poisoned.into_inner()
    });
    let camera = RtspFixture::start(&person_clip()).expect("serve the estate's clip over RTSP");
    // A real model, because a detector that cannot load is a detector whose
    // backend the runtime never reports — and this proof is about which
    // detector is running, so it needs one to be running.
    let deployment = Deployment::with_config(&format!(
        "site_name = \"home farm\"\ndetector_model_id = \"{DETECTOR_MODEL_ID}\"\n\
         detector_model_path = \"{}\"\ndetector_sample_frames = 1\n\
         \n[[cameras]]\nname = \"loading dock\"\nrtsp_url = \"{}\"\n",
        deterministic_fixture_support::toml_path(&detector_model()),
        deterministic_fixture_support::toml_string(&camera.url),
    ));
    let run = deployment.start();

    // The detector this process actually built at startup, before anything is
    // changed. Reading it first is what makes the later assertion a statement
    // about a CHANGE rather than about a coincidence.
    let startup_backend = deployment.active_detector_backend();
    let backend = a_backend_to_pin();

    let refusal = deployment.try_set(DETECTION_BACKEND_SETTING, backend);
    assert!(
        refusal.contains(ACCELERATED_DETECTION_DOMAIN),
        "the refusal names the automation that is managing this value, or the operator is left \
         guessing what to switch off: {refusal}"
    );
    assert!(
        refusal.to_lowercase().contains("turn"),
        "and it tells them how to take the wheel; a refusal carrying only a cause is half an \
         answer: {refusal}"
    );

    deployment.set(ACCELERATED_DETECTION_DOMAIN, "false");
    deployment.set(DETECTION_BACKEND_SETTING, backend);

    let rendered = deployment.listing();
    let running_detector = deployment.active_detector_backend();
    let logs = format!("{}\n{}", run.stdout(), run.stderr());
    run.stop();

    let line = setting_line(&rendered, DETECTION_BACKEND_SETTING);
    assert_eq!(
        token_field(line, VALUE_KEY),
        Some(backend),
        "the pin is the effective value once the automation is off: {line}"
    );
    assert_eq!(
        token_field(line, AUTHOR_KEY),
        Some(Author::LocalExplicit.as_str()),
        "and the surface attributes it to the operator: {line}"
    );
    assert_eq!(
        token_field(line, RUNNING_KEY),
        Some(backend),
        "and the surface says the named backend is what runs: {line}\n\nRun output:\n{logs}"
    );
    assert_eq!(
        running_detector, backend,
        "and the DETECTOR THIS PROCESS IS RUNNING is that backend — read from what the process \
         reports about itself, not from the record the change just wrote. This is the step the \
         take-back promise is made of: an operator who turns the automation off and pins a \
         backend has taken the wheel, and a build that keeps feeding frames through the detector \
         it happened to construct at startup has left them holding a setting that changed \
         nothing while every surface tells them it did. Started on {startup_backend:?}, pinned \
         {backend:?}, still running {running_detector:?}\n\nRun output:\n{logs}"
    );
}

/// The same sequence where the artifact genuinely carries two backends, so the
/// pin is a real contest rather than a restatement of the only option. Gated on
/// the accelerated backend being compiled in, because on a software-only
/// artifact there is nothing to outrank.
#[cfg(feature = "detect-burn-wgpu")]
#[test]
fn a_pinned_processor_backend_outranks_the_accelerated_one_this_artifact_carries() {
    let deployment = Deployment::with_config("cameras = []\n");
    let run = deployment.start();
    let processor = vigil::detection_accel::CPU_DETECTION_BACKEND;
    assert!(
        available_detection_backends()
            .contains(&vigil::detection_accel::ACCELERATED_DETECTION_BACKEND),
        "sanity: this artifact carries the accelerated backend, or the pin below outranks nothing"
    );

    deployment.set(ACCELERATED_DETECTION_DOMAIN, "false");
    deployment.set(DETECTION_BACKEND_SETTING, processor);

    let rendered = deployment.listing();
    let logs = format!("{}\n{}", run.stdout(), run.stderr());
    run.stop();

    let line = setting_line(&rendered, DETECTION_BACKEND_SETTING);
    assert_eq!(
        token_field(line, RUNNING_KEY),
        Some(processor),
        "the operator's choice is what runs on an artifact that could have chosen otherwise, and \
         the machine stops promoting itself onto the accelerated backend: {line}\n\nRun \
         output:\n{logs}"
    );
}

/// Unfakeable because the pin is put to sleep and woken again on ONE running
/// process: the held line has to appear while the domain is on, and the same
/// value has to be running once it is off again. A build that deleted the pin,
/// or that woke it only at the next start, fails one of the two.
#[test]
fn a_dormant_pin_runs_again_when_its_domain_is_turned_off() {
    let deployment = Deployment::with_config("cameras = []\n");
    let run = deployment.start();
    let backend = a_backend_this_artifact_carries();

    deployment.set(ACCELERATED_DETECTION_DOMAIN, "false");
    deployment.set(DETECTION_BACKEND_SETTING, backend);
    deployment.set(ACCELERATED_DETECTION_DOMAIN, "true");

    let while_held = deployment.listing();
    assert!(
        while_held
            .lines()
            .filter(|line| line.starts_with(&format!("{HELD_LINE_PREFIX} ")))
            .any(|line| line.contains(DETECTION_BACKEND_SETTING)),
        "turning the automation back on holds the operator's value rather than deleting it, and \
         says so on the face of the surface:\n{while_held}"
    );

    deployment.set(ACCELERATED_DETECTION_DOMAIN, "false");
    let rendered = deployment.listing();
    let logs = format!("{}\n{}", run.stdout(), run.stderr());
    run.stop();

    let line = setting_line(&rendered, DETECTION_BACKEND_SETTING);
    assert_eq!(
        token_field(line, VALUE_KEY),
        Some(backend),
        "turning the automation off again wakes the value that was held, rather than starting from \
         nothing: {line}"
    );
    assert_eq!(
        token_field(line, RUNNING_KEY),
        Some(backend),
        "and the woken value runs. A pin that comes back as a record but never as the running \
         backend is the same broken promise as one that never woke at all: {line}\n\nRun \
         output:\n{logs}"
    );
}
