//! Criterion 10's sibling fault: recognition switched on with a weights
//! directory that has nothing loadable in it. `crates/vigil/src/store.rs`'s
//! `open_with_recognition` never reaches the store at all in this case — the
//! embedder fails to load FIRST — so `crates/vigil/src/settings_degraded.rs`
//! classifies this as `DegradedCause::ComponentUnavailable`, a different fact
//! from `StoreUnreadable`: the store here is perfectly fine, only the
//! recognition component failed, so a settings change still lands and this
//! run still takes it on. Every operator-facing statement must say which of
//! the two happened; the store-unreadable path is proven beside this file in
//! `storeless_runtime_degraded_mode.rs`.
//!
//! The weights directory here is real but empty: `cg-vision-embedder`'s own
//! `SiglipVisionEmbedder::load` fails cleanly (`missing weights file
//! .../model.safetensors`) rather than panicking, so no corrupt binary needs
//! staging to reach this fault.

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use vigil::settings_degraded::{NOT_IN_FORCE_LINE_PREFIX, RECOGNITION_EMBEDDER_COMPONENT};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, get, toml_path, vigil_binary_path, wait_until, workspace_root,
};

const CAMERA_NAME: &str = "loading dock";

fn fixture_detector_model_path() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("fixtures")
        .join("models")
        .join("yolox-tiny-coco.pth")
}

struct ComponentFailureDeployment {
    _tmp: tempfile::TempDir,
    _socket_tmp: tempfile::TempDir,
    data_dir: PathBuf,
    config_path: PathBuf,
    socket_path: PathBuf,
}

impl ComponentFailureDeployment {
    /// Recognition switched on (`recognition_weights_dir` set) against a real,
    /// empty directory — never a corrupt or absent one, so the fault is
    /// unambiguously "nothing loadable here" rather than a filesystem error a
    /// reader could mistake for something else.
    fn prepare() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket_tmp = tempfile::tempdir().expect("tempdir for the control socket");
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&data_dir).expect("data dir");
        let weights_dir = tmp.path().join("weights");
        fs::create_dir_all(&weights_dir).expect("empty weights dir");

        let config_path = tmp.path().join("vigil.toml");
        let config = format!(
            "site_name = \"home farm\"\nrecognition_weights_dir = \"{}\"\n\n[[cameras]]\n\
             name = \"{CAMERA_NAME}\"\nrtsp_url = \"rtsp://192.0.2.10:554/stream\"\n",
            toml_path(&weights_dir)
        );
        fs::write(&config_path, config).expect("write the camera config");

        let socket_path = socket_tmp.path().join("control.sock");
        Self {
            _tmp: tmp,
            _socket_tmp: socket_tmp,
            data_dir,
            config_path,
            socket_path,
        }
    }

    fn start(&self) -> ComponentFailureRun {
        let health = TcpPortReservation::reserve_loopback().expect("reserve health port");
        let review = TcpPortReservation::reserve_loopback().expect("reserve review port");
        let health_port = health.port();
        let review_port = review.port();

        let model_path = fixture_detector_model_path();
        assert!(
            model_path.is_file(),
            "the repo's real detector-model fixture must be present: {}",
            model_path.display()
        );

        let mut command = Command::new(vigil_binary_path());
        command
            .arg("run")
            .arg("--config")
            .arg(&self.config_path)
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--health-port")
            .arg(health_port.to_string())
            .arg("--review-port")
            .arg(review_port.to_string())
            .arg("--detector-model-path")
            .arg(&model_path)
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env("VIGIL_CONTROL_SOCKET", &self.socket_path)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_FABRIC_TICKET")
            .env_remove("VIGIL_FABRIC_HUB")
            .env_remove("VIGIL_RECOGNITION_WEIGHTS_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn vigil run");
        let stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());

        let run = ComponentFailureRun {
            child,
            health_port,
            stdout,
        };

        wait_until(
            &format!(
                "the component-failure runtime's control socket at {} to appear",
                self.socket_path.display()
            ),
            Duration::from_secs(30),
            || Ok(UnixStream::connect(&self.socket_path).ok().map(|_| ())),
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}. A failed recognition component must not stop Vigil starting — the \
                 store was never even opened. Process output so far:\n{}",
                run.stdout()
            )
        });

        run
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(vigil_binary_path())
            .args(args)
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env("VIGIL_CONTROL_SOCKET", &self.socket_path)
            .env_remove("VIGIL_STORE_PATH")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("spawn vigil {args:?}: {error}"))
    }
}

struct ComponentFailureRun {
    child: Child,
    health_port: u16,
    stdout: std::sync::Arc<std::sync::Mutex<String>>,
}

impl ComponentFailureRun {
    fn stdout(&self) -> String {
        self.stdout.lock().expect("stdout lock").clone()
    }

    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ComponentFailureRun {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn settings_surface(deployment: &ComponentFailureDeployment) -> String {
    let output = deployment.cli(&["settings"]);
    assert!(
        output.status.success(),
        "the operator surface must answer while a component failed to load; `vigil settings` \
         exited {:?}, output:\n{}",
        output.status.code(),
        combined(&output)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// The store is intact here — only the recognition component failed to load
/// before the store was ever reached — so the operator surface names
/// `recognition-embedder`, the health/telemetry line commits to
/// `store_unreadable=false`, and a settings change still lands with a
/// `not-in-force` line naming the component rather than being refused
/// outright.
#[test]
fn recognition_component_failure_names_itself_and_still_lands_settings_changes() {
    let deployment = ComponentFailureDeployment::prepare();
    let run = deployment.start();

    let rendered = settings_surface(&deployment);
    let health = get(run.health_port, "/health").body_text();
    let logs = run.stdout();

    assert!(
        rendered.contains(RECOGNITION_EMBEDDER_COMPONENT),
        "the operator surface must name the component that actually failed \
         ({RECOGNITION_EMBEDDER_COMPONENT}); got:\n{rendered}"
    );
    assert!(
        !rendered.contains("store_unreadable=true") && !health.contains("store_unreadable=true"),
        "the store is intact here; neither surface may claim it is unreadable — \
         settings:\n{rendered}\nhealth:\n{health}"
    );
    assert!(
        !logs.contains("store_unreadable=true"),
        "the startup telemetry line must never claim the store is unreadable when only the \
         recognition component failed to load; logs:\n{logs}"
    );
    assert!(
        logs.contains("store_unreadable=false") && logs.contains("component_unavailable=true"),
        "the startup telemetry line must commit to the store being intact and name the \
         component as what actually failed; logs:\n{logs}"
    );

    let settings_change = deployment.cli(&["settings", "set", "detector_sample_frames", "7"]);
    run.stop();

    assert!(
        settings_change.status.success(),
        "a component failure must let a settings change land — the store that would hold it is \
         perfectly readable; `vigil settings set` exited {:?}, output:\n{}",
        settings_change.status.code(),
        combined(&settings_change)
    );
    let change_text = combined(&settings_change);
    assert!(
        change_text
            .lines()
            .any(|line| line.starts_with(NOT_IN_FORCE_LINE_PREFIX)),
        "a landed change under a component failure must still carry the `{NOT_IN_FORCE_LINE_PREFIX}` \
         line, naming what the change cannot bring into force; got:\n{change_text}"
    );
    assert!(
        change_text.contains(&format!("component={RECOGNITION_EMBEDDER_COMPONENT}")),
        "the not-in-force line must name the component by the same spelling the startup line \
         uses; got:\n{change_text}"
    );
}
