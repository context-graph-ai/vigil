//! What `vigil settings` prints as `running=` for the detector queue depth has
//! to be what this process is actually queueing at, from the first moment the
//! surface can be asked — and where the one documented environment lever
//! supplied that depth, the line has to name the lever and the operator's pin
//! it stands in front of.
//!
//! The estate already proves the projection and the rendering
//! (`crates/vigil/tests/detector_queue_capacity_environment_attribution.rs`),
//! but it makes the `record_running_from_environment` call BY HAND inside the
//! test process. Nothing proved that any shipped code path makes that call in
//! time, or at all. It does not: the lever is read and recorded inside
//! `start_rtsp_probe`, on a camera thread, after the control listener is
//! already serving — so a deployment answers `running=1` (a positively
//! asserted, false runtime fact, not a missing one) for as long as the camera
//! thread takes to get there, and on a node with no cameras it answers that
//! way forever.
//!
//! Both arms therefore drive the REAL compiled `vigil run` and ask the REAL
//! compiled `vigil settings` as a separate OS process, and the question is put
//! the instant the control socket accepts a connection — no sleep, no retry
//! loop. A test that retried would be asserting that the answer becomes true
//! eventually, which is not the promise; the promise is that it is never
//! false.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use vigil::settings_backends::AUTOMATIC_DETECTOR_QUEUE_CAPACITY;
use vigil::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING;
use vigil::settings_projection::{
    NAME_KEY, RUNNING_KEY, RUNNING_SOURCE_KEY, SETTING_LINE_PREFIX, SHADOWED_SETTING_KEY,
};
use vigil::settings_store::SettingsStore;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_for_store_owner,
};

/// The one documented environment lever over a behavior setting.
const QUEUE_CAPACITY_VARIABLE: &str = "VIGIL_DETECTOR_QUEUE_CAPACITY";
/// What the lever asks this run to queue at.
const LEVER_CAPACITY: &str = "3";
/// The operator's own pin, distinct from the lever's number so a line that
/// echoed the lever back at itself as the shadowed value cannot pass here by
/// coincidence.
const PINNED_CAPACITY: &str = "1";

// ── Process fixture ────────────────────────────────────────────────────────

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

    /// A real `vigil run` carrying the environment lever, returned only once
    /// its control socket accepts a connection — the moment the operator
    /// surface first has someone to answer for it.
    fn start_with_the_lever(&self) -> LiveVigil {
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
            .env(QUEUE_CAPACITY_VARIABLE, LEVER_CAPACITY)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Held until the moment of spawn: a released port is a race another
        // process can steal before this one binds it.
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
        wait_for_the_store_owner(&self.data_dir, &run);
        run
    }

    /// `vigil settings`, as a separate process that does NOT carry the lever —
    /// so nothing it prints can have come from its own environment. Every
    /// attribution token it renders had to come from the running process.
    fn listing(&self) -> String {
        let output: Output = Command::new(vigil_binary_path())
            .arg("settings")
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .env_remove(QUEUE_CAPACITY_VARIABLE)
            .stdin(Stdio::null())
            .output()
            .expect("spawn `vigil settings`");
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
    fn logs(&self) -> String {
        format!(
            "{}\n{}",
            self.stdout.lock().expect("stdout lock"),
            self.stderr.lock().expect("stderr lock")
        )
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

/// Readiness is the store's OWNER ROUTE answering: the runtime holds this
/// deployment's store, so a separate `vigil settings` process reaches it rather
/// than answering out of its own environment — which is the whole point of the
/// attribution this file checks. Never a sleep and never a printed line: the
/// "runtime loop ready" receipt is emitted before the runtime owns anything, so
/// a reader that trusts it races the open and silently falls through to a
/// direct store read.
fn wait_for_the_store_owner(data_dir: &Path, run: &LiveVigil) {
    wait_for_store_owner(data_dir, &SettingsStore::store_path(data_dir)).unwrap_or_else(|error| {
        panic!(
            "{error}. The runtime must own this deployment's store. Output so far:\n{}",
            run.logs()
        )
    });
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

/// The three assertions the operator's line has to satisfy, made against one
/// rendering. Shared by both arms so neither can drift into asserting less
/// than the other.
fn assert_the_lever_is_named_beside_the_pin(rendered: &str, logs: &str, arm: &str) {
    let line = setting_line(rendered, DETECTOR_QUEUE_CAPACITY_SETTING);
    assert_eq!(
        token_field(line, RUNNING_KEY),
        Some(LEVER_CAPACITY),
        "{arm}: the queue this process actually built is the lever's depth, so `running=` must \
         say so. `running={PINNED_CAPACITY}` here is not a missing answer — a missing answer \
         renders as `running=none` — it is a positively asserted, false runtime fact, and an \
         operator reading it has no way to tell it from a settled one: {line}\n\nRun \
         output:\n{logs}"
    );
    assert_eq!(
        token_field(line, RUNNING_SOURCE_KEY),
        Some(format!("environment:{QUEUE_CAPACITY_VARIABLE}").as_str()),
        "{arm}: and the line must name the lever that supplied that depth, or the operator is \
         reading a number nobody on their surface set: {line}\n\nRun output:\n{logs}"
    );
    assert_eq!(
        token_field(line, SHADOWED_SETTING_KEY),
        Some(PINNED_CAPACITY),
        "{arm}: and it must carry the operator's OWN pin the lever stands in front of, not the \
         lever's number ({LEVER_CAPACITY}) reported back at itself: {line}\n\nRun output:\n{logs}"
    );
}

// ── The two arms ───────────────────────────────────────────────────────────

/// A deployment with a camera. The lever is recorded today inside
/// `start_rtsp_probe`, on the camera thread, after detector construction — so
/// between the control listener binding and that thread arriving, the surface
/// answers with the pin and no attribution at all. Asking the instant the
/// socket accepts is what puts the question inside that window; a retry loop
/// would assert only that the answer becomes true eventually, which is a
/// different and much weaker promise than the one criterion 11 makes.
#[test]
fn the_queue_depth_a_camera_node_reports_names_its_lever_from_the_first_moment_it_can_be_asked() {
    // An unreachable-but-well-formed URL on the closed TEST-NET-1 block: the
    // camera set is a configuration fact and this proof is about what the
    // surface answers, not about live ingest. A camera that connected would
    // only make the window this test aims at narrower.
    let deployment = Deployment::with_config(&format!(
        "detector_queue_capacity = {PINNED_CAPACITY}\n\
         \n[[cameras]]\nname = \"loading dock\"\nrtsp_url = \"rtsp://192.0.2.10:554/stream\"\n"
    ));
    let run = deployment.start_with_the_lever();

    let rendered = deployment.listing();
    let logs = run.logs();
    run.stop();

    assert_the_lever_is_named_beside_the_pin(&rendered, &logs, "camera node");
}

/// The unfakeable half. A worker or discovery node with `cameras = []` is a
/// legitimate deployment, and it never runs `start_rtsp_probe` at all — so the
/// lever is never recorded, and the false `running=` is permanent rather than
/// racy. No amount of waiting fixes this arm; only publishing the lever in the
/// startup act that publishes every other running value does.
#[test]
fn a_cameraless_node_still_names_the_lever_that_supplied_its_queue_depth() {
    let deployment = Deployment::with_config(&format!(
        "cameras = []\ndetector_queue_capacity = {PINNED_CAPACITY}\n"
    ));
    let run = deployment.start_with_the_lever();

    let rendered = deployment.listing();
    let logs = run.logs();
    run.stop();

    assert_the_lever_is_named_beside_the_pin(&rendered, &logs, "cameraless node");
}

/// Most deployments never pin this setting at all, and the token is rendered
/// for them too — carrying the depth Vigil would have chosen for itself. Both
/// arms above configure a pin, so neither of them can say what an unpinned
/// node reads, and the documentation described the token as the operator's own
/// stored pin on that evidence. What the projection key actually promises is
/// the operator's pin where they set one and this artifact's own choice where
/// they did not, and this arm holds the second half.
///
/// The automatic depth and the pin the other arms use are the same number,
/// which is a property of the shipped default and not something this test
/// leans on: what makes the arm meaningful is that the deployment states no
/// depth anywhere, so whatever the token carries was authored by Vigil.
#[test]
fn an_unpinned_node_shadows_the_depth_vigil_chose_for_itself_not_an_operator_pin() {
    let deployment = Deployment::with_config("cameras = []\n");
    assert!(
        !fs::read_to_string(&deployment.config_path)
            .expect("read back the configuration under test")
            .contains(DETECTOR_QUEUE_CAPACITY_SETTING),
        "the case under test is a deployment that pins nothing, so its configuration must not \
         name the setting at all"
    );
    let run = deployment.start_with_the_lever();

    let rendered = deployment.listing();
    let logs = run.logs();
    run.stop();

    let line = setting_line(&rendered, DETECTOR_QUEUE_CAPACITY_SETTING);
    assert_eq!(
        token_field(line, RUNNING_KEY),
        Some(LEVER_CAPACITY),
        "unpinned node: the lever still supplies what this process queues at: {line}\n\nRun \
         output:\n{logs}"
    );
    assert_eq!(
        token_field(line, RUNNING_SOURCE_KEY),
        Some(format!("environment:{QUEUE_CAPACITY_VARIABLE}").as_str()),
        "unpinned node: and the line still names the lever that supplied it: {line}\n\nRun \
         output:\n{logs}"
    );
    assert_eq!(
        token_field(line, SHADOWED_SETTING_KEY),
        Some(AUTOMATIC_DETECTOR_QUEUE_CAPACITY.to_string().as_str()),
        "unpinned node: nobody pinned anything here, so the shadowed value is the depth Vigil \
         would otherwise have chosen — a reader told this token is always their own stored pin \
         reads it as a setting they never made: {line}\n\nRun output:\n{logs}"
    );
}
