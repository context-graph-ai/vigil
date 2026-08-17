//! A config file that used to name settings and now names NONE — the file
//! still exists and still parses, it just has nothing left in it — must reset
//! every record it authored, at the deployment's own scope and at every
//! camera's, exactly as removing one key does.
//!
//! Before this arc's fix, `runtime::apply_surface` returned early whenever a
//! surface's own entries were empty, with no exception for a surface that CAN
//! clear by absence. Emptying a config file down to nothing therefore left
//! every earlier pin standing forever, because the function returned before
//! ever opening the store. The fix narrows the early return to a surface that
//! both says nothing AND cannot clear by absence (the startup options only);
//! a persistent file surface always reaches the store now, present or empty
//! alike. Driven through the REAL compiled `vigil run`, across a genuine
//! restart, because the early return lives in `runtime.rs` and a direct
//! `SettingsStore::apply_surface_snapshot` call never exercises it.
//!
//! RED before this arc's fix (`7e851dd`): the second run's empty file would
//! return early before touching the store, so `motion_sensitivity`,
//! `detector_queue_capacity` and the driveway's camera-scoped
//! `motion_sensitivity` would all still read back as the FIRST run's pins,
//! attributed to the config file, rather than resetting to automatic.

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use vigil::settings_model::{
    Author, DETECTOR_QUEUE_CAPACITY_SETTING, MOTION_SENSITIVITY_SETTING, ScopeTarget,
};
use vigil::settings_projection::{AUTHOR_KEY, SETTING_LINE_PREFIX, SURFACE_KEY};

const VALUE_KEY: &str = "value";
const NAME_KEY: &str = "name";
const DRIVEWAY: &str = "driveway";

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_until,
};

struct LiveVigil {
    child: Child,
}

impl LiveVigil {
    fn spawn(config_path: &Path, data_dir: &Path) -> Self {
        let health = TcpPortReservation::reserve_loopback().expect("reserve health port");
        let review = TcpPortReservation::reserve_loopback().expect("reserve review port");
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn vigil run");
        let _stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());
        Self { child }
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

fn wait_for_control_socket(data_dir: &Path) {
    let socket_path = data_dir.join("control.sock");
    wait_until(
        &format!("the control socket at {} to appear", socket_path.display()),
        Duration::from_secs(30),
        || Ok(UnixStream::connect(&socket_path).ok().map(|_| ())),
    )
    .expect("the runtime must publish its control socket");
}

fn wait_for_control_socket_to_go_away(data_dir: &Path) {
    let socket_path = data_dir.join("control.sock");
    wait_until(
        &format!(
            "the control socket at {} to stop accepting connections",
            socket_path.display()
        ),
        Duration::from_secs(30),
        || {
            Ok(match UnixStream::connect(&socket_path) {
                Ok(_) => None,
                Err(_) => Some(()),
            })
        },
    )
    .expect("the previous runtime must be fully stopped before the restart");
}

fn token_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(&rest[..end])
}

fn setting_named<'a>(lines: &'a [String], name: &str) -> Option<&'a str> {
    lines
        .iter()
        .filter(|line| line.starts_with(&format!("{SETTING_LINE_PREFIX} ")))
        .map(String::as_str)
        .find(|line| token_field(line, NAME_KEY) == Some(name))
}

fn setting_line_at(data_dir: &Path, camera: Option<&str>, setting: &str) -> String {
    let node = vigil::node_key::scope_name(data_dir);
    let target = ScopeTarget {
        tenant: node.clone(),
        site: node.clone(),
        node,
        camera: camera.map(str::to_string),
    };
    let report = vigil::settings_projection::report_by_direct_read_at(data_dir, &target)
        .expect("direct store read must succeed");
    let lines = report.render_lines();
    setting_named(&lines, setting)
        .unwrap_or_else(|| panic!("no `{setting}` line, got:\n{}", lines.join("\n")))
        .to_string()
}

/// Unfakeable because three separate records — two at the deployment's own
/// scope and one at a camera's — are pinned by the FIRST run, and the assert
/// after the second run checks all three are gone rather than sampling one:
/// an early return that skips the store entirely leaves every one of them
/// standing, and a fix that clears only the deployment-wide half (or only the
/// camera half) still fails whichever record it left behind.
#[test]
fn an_emptied_config_file_resets_every_record_it_authored_at_node_and_camera_scope_alike() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = tmp.path().join("vigil.toml");

    fs::write(
        &config_path,
        "motion_sensitivity = 3\n\
         detector_queue_capacity = 42\n\
         [[cameras]]\n\
         name = \"driveway\"\n\
         rtsp_url = \"rtsp://camera.local:554/driveway\"\n\
         motion_sensitivity = 9\n",
    )
    .expect("write config: everything pinned");
    let run1 = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_control_socket(&data_dir);
    run1.stop();
    wait_for_control_socket_to_go_away(&data_dir);

    // Sanity: all three are pinned before the empty file is tried.
    for (camera, setting, expected) in [
        (None, MOTION_SENSITIVITY_SETTING, "3"),
        (None, DETECTOR_QUEUE_CAPACITY_SETTING, "42"),
        (Some(DRIVEWAY), MOTION_SENSITIVITY_SETTING, "9"),
    ] {
        let line = setting_line_at(&data_dir, camera, setting);
        assert_eq!(
            token_field(&line, VALUE_KEY),
            Some(expected),
            "sanity: `{setting}` (camera={camera:?}) must be pinned before the empty-file pass; \
             got: {line}"
        );
    }

    // The file still exists and still parses; it just names nothing at all —
    // no deployment-wide key and no `[[cameras]]` entry.
    fs::write(&config_path, "").expect("write config: emptied");
    let run2 = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_control_socket(&data_dir);
    run2.stop();
    wait_for_control_socket_to_go_away(&data_dir);

    for (camera, setting) in [
        (None, MOTION_SENSITIVITY_SETTING),
        (None, DETECTOR_QUEUE_CAPACITY_SETTING),
        (Some(DRIVEWAY), MOTION_SENSITIVITY_SETTING),
    ] {
        let line = setting_line_at(&data_dir, camera, setting);
        assert_ne!(
            token_field(&line, AUTHOR_KEY),
            Some(Author::LocalExplicit.as_str()),
            "an emptied-but-present config file must reset `{setting}` (camera={camera:?}) — the \
             config file no longer stands behind it; got: {line}"
        );
        assert_ne!(
            token_field(&line, SURFACE_KEY),
            Some(vigil::settings_model::Surface::ConfigFile.as_str()),
            "`{setting}` (camera={camera:?}) must no longer be attributed to the config file once \
             the file names nothing; got: {line}"
        );
    }
}
