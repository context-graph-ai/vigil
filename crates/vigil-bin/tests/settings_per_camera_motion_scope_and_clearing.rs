//! A camera row's own `motion_sensitivity` authors at THAT camera's scope,
//! not the deployment's, and clears the same way a deployment-wide value
//! does — one scope down.
//!
//! Before this arc's fix, `CameraEntryPartial` had no `motion_sensitivity`
//! field at all, so a value typed into a camera's row on the add-on options
//! page (or the config file) was silently dropped: no record at the camera's
//! scope, and no way for that camera's gate to ever diverge from the
//! deployment-wide value. This drives the REAL compiled `vigil run` against a
//! real configuration file naming two cameras, and reads the result back with
//! a DIRECT store read at each camera's own scope
//! (`vigil::settings_projection::report_by_direct_read_at`) — the same seam
//! `addon_config_surface.rs` uses, and the only way to ask the store what a
//! camera's own record says, since `vigil settings` answers for the
//! deployment only.
//!
//! RED before this arc's fix (`7e851dd`): `CameraEntryPartial` had no
//! `motion_sensitivity` field, so the driveway's row set nothing and both
//! cameras resolved to the deployment-wide value; `apply_camera_surface_snapshot`
//! and `camera_scopes_authored_through` did not exist on `SettingsStore` at
//! all, so the clearing test below fails to compile against that revision.

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use vigil::settings_model::{Author, MOTION_SENSITIVITY_SETTING, ScopeTarget, Surface};
use vigil::settings_projection::{AUTHOR_KEY, SCOPE_KEY, SETTING_LINE_PREFIX, SURFACE_KEY};

const VALUE_KEY: &str = "value";
const NAME_KEY: &str = "name";

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_until,
};

const DRIVEWAY: &str = "driveway";
const HALLWAY: &str = "hallway";
/// The deployment-wide value every camera without its own runs at.
const DEPLOYMENT_SENSITIVITY: i64 = 3;
/// The driveway's own first value, distinct from the deployment value and
/// from what it moves to below, so no answer can be right by coincidence.
const DRIVEWAY_SENSITIVITY: i64 = 9;

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

fn cameras_fragment(driveway_motion_sensitivity: Option<i64>) -> String {
    let driveway_motion_line = driveway_motion_sensitivity
        .map(|value| format!("motion_sensitivity = {value}\n"))
        .unwrap_or_default();
    format!(
        "[[cameras]]\n\
         name = \"{DRIVEWAY}\"\n\
         rtsp_url = \"rtsp://camera.local:554/driveway\"\n\
         {driveway_motion_line}\n\
         [[cameras]]\n\
         name = \"{HALLWAY}\"\n\
         rtsp_url = \"rtsp://camera.local:554/hallway\"\n"
    )
}

fn config_with_both_cameras(driveway_motion_sensitivity: Option<i64>) -> String {
    format!(
        "motion_sensitivity = {DEPLOYMENT_SENSITIVITY}\n{}",
        cameras_fragment(driveway_motion_sensitivity)
    )
}

/// The scope name this deployment persisted at `data_dir`, read back the same
/// way the store itself resolves it — so a direct read asks about the exact
/// scope the running process wrote to.
fn node_scope_target(data_dir: &Path, camera: Option<&str>) -> ScopeTarget {
    let node = vigil::node_key::scope_name(data_dir);
    ScopeTarget {
        tenant: node.clone(),
        site: node.clone(),
        node,
        camera: camera.map(str::to_string),
    }
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

fn motion_sensitivity_line_at(data_dir: &Path, camera: Option<&str>) -> String {
    let target = node_scope_target(data_dir, camera);
    let report = vigil::settings_projection::report_by_direct_read_at(data_dir, &target)
        .expect("direct store read must succeed");
    let lines = report.render_lines();
    setting_named(&lines, MOTION_SENSITIVITY_SETTING)
        .unwrap_or_else(|| {
            panic!(
                "no `{MOTION_SENSITIVITY_SETTING}` line for target {target:?}, got:\n{}",
                lines.join("\n")
            )
        })
        .to_string()
}

/// Unfakeable because both cameras are read from the SAME store after the
/// SAME process wrote it, and the two answers can only differ if the process
/// kept the driveway's record apart from the deployment's: an implementation
/// that authors the camera row at NODE scope (the pre-fix silent-drop shape,
/// or a naive fix that forgets the scope) makes both cameras resolve to 9,
/// and one that never authors the camera row at all makes both resolve to 3.
/// Only "driveway=9 at camera scope, hallway=3 inherited" is a correct
/// answer.
#[test]
fn a_cameras_own_motion_sensitivity_authors_at_that_cameras_scope_and_an_unlisted_camera_inherits()
{
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = tmp.path().join("vigil.toml");
    fs::write(
        &config_path,
        config_with_both_cameras(Some(DRIVEWAY_SENSITIVITY)),
    )
    .expect("write config");

    let live = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_control_socket(&data_dir);
    live.stop();
    wait_for_control_socket_to_go_away(&data_dir);

    let driveway_line = motion_sensitivity_line_at(&data_dir, Some(DRIVEWAY));
    assert_eq!(
        token_field(&driveway_line, VALUE_KEY),
        Some(DRIVEWAY_SENSITIVITY.to_string()).as_deref(),
        "the driveway's own row must win at the driveway's own scope; got: {driveway_line}"
    );
    assert_eq!(
        token_field(&driveway_line, SCOPE_KEY),
        Some(format!("camera:{DRIVEWAY}")).as_deref(),
        "the driveway's record must be authored AT THE CAMERA'S OWN SCOPE, not the deployment's; \
         got: {driveway_line}"
    );
    assert_eq!(
        token_field(&driveway_line, AUTHOR_KEY),
        Some(Author::LocalExplicit.as_str()),
        "got: {driveway_line}"
    );
    assert_eq!(
        token_field(&driveway_line, SURFACE_KEY),
        Some(Surface::ConfigFile.as_str()),
        "got: {driveway_line}"
    );

    let hallway_line = motion_sensitivity_line_at(&data_dir, Some(HALLWAY));
    assert_eq!(
        token_field(&hallway_line, VALUE_KEY),
        Some(DEPLOYMENT_SENSITIVITY.to_string()).as_deref(),
        "a camera the config file lists but does not give its own value must inherit the \
         deployment-wide value, never the OTHER camera's value and never the automatic floor; \
         got: {hallway_line}"
    );
    assert_eq!(
        token_field(&hallway_line, SCOPE_KEY),
        Some("node:".to_string() + &vigil::node_key::scope_name(&data_dir)).as_deref(),
        "the inherited value is the deployment's own record, read through inheritance rather \
         than copied onto the camera; got: {hallway_line}"
    );
}

/// The evidence file's own three-step verification, reproduced end to end:
/// set the driveway's own value, remove just that key, then remove the whole
/// row. Unfakeable because three genuinely separate `vigil run` processes
/// share one data directory and each step is checked against BOTH cameras and
/// the deployment-wide record, so a fix that clears too much (the deployment
/// value, or the hallway) or too little (leaves the driveway pinned) fails
/// whichever half it gets wrong.
#[test]
fn removing_a_cameras_key_clears_that_camera_alone_and_removing_its_row_clears_what_the_row_authored()
 {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = tmp.path().join("vigil.toml");

    // Step 1: the driveway carries its own value.
    fs::write(
        &config_path,
        config_with_both_cameras(Some(DRIVEWAY_SENSITIVITY)),
    )
    .expect("write config: driveway pinned");
    let run1 = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_control_socket(&data_dir);
    run1.stop();
    wait_for_control_socket_to_go_away(&data_dir);
    assert_eq!(
        token_field(
            &motion_sensitivity_line_at(&data_dir, Some(DRIVEWAY)),
            VALUE_KEY
        ),
        Some(DRIVEWAY_SENSITIVITY.to_string()).as_deref(),
        "sanity: the driveway must be pinned before either removal is checked"
    );

    // Step 2: the key is removed from the driveway's row, but the row stays.
    fs::write(&config_path, config_with_both_cameras(None)).expect("write config: key removed");
    let run2 = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_control_socket(&data_dir);
    run2.stop();
    wait_for_control_socket_to_go_away(&data_dir);

    let driveway_after_key_removed = motion_sensitivity_line_at(&data_dir, Some(DRIVEWAY));
    assert_eq!(
        token_field(&driveway_after_key_removed, VALUE_KEY),
        Some(DEPLOYMENT_SENSITIVITY.to_string()).as_deref(),
        "removing the key from the driveway's row must clear THAT camera's record and let it \
         inherit the deployment value again; got: {driveway_after_key_removed}"
    );
    let hallway_after_key_removed = motion_sensitivity_line_at(&data_dir, Some(HALLWAY));
    assert_eq!(
        token_field(&hallway_after_key_removed, VALUE_KEY),
        Some(DEPLOYMENT_SENSITIVITY.to_string()).as_deref(),
        "a camera nobody touched must be exactly where it was; got: {hallway_after_key_removed}"
    );
    let deployment_after_key_removed = motion_sensitivity_line_at(&data_dir, None);
    assert_eq!(
        token_field(&deployment_after_key_removed, VALUE_KEY),
        Some(DEPLOYMENT_SENSITIVITY.to_string()).as_deref(),
        "clearing one camera's record must never touch the deployment-wide record; \
         got: {deployment_after_key_removed}"
    );
    assert_eq!(
        token_field(&deployment_after_key_removed, AUTHOR_KEY),
        Some(Author::LocalExplicit.as_str()),
        "the deployment-wide value is still a human's own pin, not reset to automatic; \
         got: {deployment_after_key_removed}"
    );

    // Step 3: pin the driveway again, then remove its WHOLE row.
    fs::write(
        &config_path,
        config_with_both_cameras(Some(DRIVEWAY_SENSITIVITY)),
    )
    .expect("write config: driveway re-pinned");
    let run3 = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_control_socket(&data_dir);
    run3.stop();
    wait_for_control_socket_to_go_away(&data_dir);
    assert_eq!(
        token_field(
            &motion_sensitivity_line_at(&data_dir, Some(DRIVEWAY)),
            VALUE_KEY
        ),
        Some(DRIVEWAY_SENSITIVITY.to_string()).as_deref(),
        "sanity: the driveway must be pinned again before its row is dropped"
    );

    let hallway_only = format!(
        "motion_sensitivity = {DEPLOYMENT_SENSITIVITY}\n\
         [[cameras]]\n\
         name = \"{HALLWAY}\"\n\
         rtsp_url = \"rtsp://camera.local:554/hallway\"\n"
    );
    fs::write(&config_path, hallway_only).expect("write config: driveway row dropped");
    let run4 = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_control_socket(&data_dir);
    run4.stop();
    wait_for_control_socket_to_go_away(&data_dir);

    // The driveway is no longer a camera this deployment watches, but the
    // record its row authored must be gone — asked about directly, since a
    // dropped camera is still a scope the store can be asked about.
    let driveway_after_row_dropped = motion_sensitivity_line_at(&data_dir, Some(DRIVEWAY));
    assert_eq!(
        token_field(&driveway_after_row_dropped, VALUE_KEY),
        Some(DEPLOYMENT_SENSITIVITY.to_string()).as_deref(),
        "dropping the driveway's whole row must clear the record that row authored, exactly as \
         dropping the one value inside it does; got: {driveway_after_row_dropped}"
    );
    assert_ne!(
        token_field(&driveway_after_row_dropped, SCOPE_KEY),
        Some(format!("camera:{DRIVEWAY}")).as_deref(),
        "the answer must come from inheritance, not a surviving camera-scoped record; \
         got: {driveway_after_row_dropped}"
    );
}
