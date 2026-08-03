//! Companion to `cameraless_health_statement.rs`: proves the `/health`
//! body's `cameras_by_kind` structure reports GENUINE per-kind counts and
//! per-camera entries for a node that actually has cameras configured, not
//! a structure that is always zero regardless of configuration. Without
//! this file, the cameraless test's all-zero assertion could be satisfied
//! by a body that never counts anything — this file is what proves the
//! zeros in the cameraless case are a real fact about that deployment, not
//! a hardcoded shape.
//!
//! Only the `rtsp` source kind can actually load and run in this artifact
//! today (`config::artifact_supports_source_kind` — usb/csi/mjpeg entries
//! fail the whole config load as `unsupported_by_this_artifact`), so this
//! drives two distinctly named RTSP cameras through the real, composed
//! `vigil` binary end to end — its own boot path and its own raw `/health`
//! HTTP responder, never a helper reimplementing that logic — and never
//! waits on an actual RTSP connection succeeding: the count/list is a
//! configuration fact, not a live-ingest fact, so the assertions below do
//! not depend on, and must not depend on, whether ingest ever connects.

use std::fs;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, get, vigil_binary_path, wait_until,
};

const FIRST_CAMERA_NAME: &str = "loading dock";
const SECOND_CAMERA_NAME: &str = "north fence";

struct Node {
    child: Child,
    stdout: Arc<Mutex<String>>,
}

impl Node {
    fn kill_and_wait(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn stdout(&self) -> String {
        self.stdout.lock().expect("stdout lock").clone()
    }
}

fn spawn_two_camera_node(
    data_dir: &std::path::Path,
    health_reservation: TcpPortReservation,
    review_reservation: TcpPortReservation,
) -> Node {
    let health_port = health_reservation.port();
    let review_port = review_reservation.port();
    let config_path = data_dir.join("vigil.toml");
    // Unreachable-but-well-formed RTSP URLs on the closed TEST-NET-1 block:
    // the loader must accept and count them without ever needing a real
    // stream to connect (the by-kind count is a configuration fact, not a
    // live-ingest fact).
    fs::write(
        &config_path,
        format!(
            r#"
            [[cameras]]
            name = "{FIRST_CAMERA_NAME}"
            rtsp_url = "rtsp://192.0.2.10:554/stream"

            [[cameras]]
            name = "{SECOND_CAMERA_NAME}"
            rtsp_url = "rtsp://192.0.2.11:554/stream"
            "#
        ),
    )
    .expect("write two-camera config");

    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .arg("--config")
        .arg(&config_path)
        .env("VIGIL_DATA_DIR", data_dir)
        .env("VIGIL_HEALTH_PORT", health_port.to_string())
        .env("VIGIL_REVIEW_PORT", review_port.to_string())
        .env_remove("VIGIL_RTSP_URL")
        .env_remove("VIGIL_FABRIC_TICKET")
        .env_remove("VIGIL_FABRIC_HUB")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Held open until the moment of spawn, not released early — a released
    // port is a race another process can steal before this one binds it.
    health_reservation.release();
    review_reservation.release();
    let mut child = command.spawn().expect("spawn vigil binary");
    let stdout = capture_pipe(child.stdout.take());
    let _stderr = capture_pipe(child.stderr.take());
    Node { child, stdout }
}

/// The `/health` body is JSON on its first line only; when acceleration or
/// fabric receipts exist, additional plain-text receipt-block lines follow
/// (see `crates/vigil/src/health.rs`). A node with real camera ingest
/// activity can grow those lines at any point after boot, so — unlike the
/// cameraless fixture, which never decodes anything and therefore never
/// grows a second line — this test must parse only the first line as JSON
/// rather than the whole body, or it would be racing the ingest thread's
/// own receipts irrespective of what this feature is about.
fn first_line_json(body_text: &str) -> Value {
    let first_line = body_text.lines().next().unwrap_or_default();
    serde_json::from_str(first_line).unwrap_or_else(|error| {
        panic!("first line of /health body must be JSON: {error}; got body: {body_text}")
    })
}

#[test]
fn configured_rtsp_cameras_are_counted_and_listed_by_kind_on_health() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let health_reservation = TcpPortReservation::reserve_loopback().expect("reserve health port");
    let review_reservation = TcpPortReservation::reserve_loopback().expect("reserve review port");
    let health_port = health_reservation.port();
    let node = spawn_two_camera_node(data_dir.path(), health_reservation, review_reservation);

    let boot_reached_pipeline_up = wait_until(
        "the two-camera node to report boot_phase=pipeline-up",
        Duration::from_secs(20),
        || {
            if node.stdout().contains("boot_phase=pipeline-up") {
                Ok(Some(()))
            } else {
                Ok(None)
            }
        },
    );
    if let Err(error) = boot_reached_pipeline_up {
        let logs = node.stdout();
        node.kill_and_wait();
        panic!("{error}; logs so far:\n{logs}");
    }

    let response = get(health_port, "/health");
    let whole_body = response.body_text();
    let body = first_line_json(&whole_body);
    let boot_logs = node.stdout();
    node.kill_and_wait();

    let cameras_by_kind = body
        .get("cameras_by_kind")
        .and_then(Value::as_object)
        .unwrap_or_else(|| {
            panic!(
                "a node with configured cameras must carry a \"cameras_by_kind\" object; got \
                 body: {whole_body}; boot logs:\n{boot_logs}"
            )
        });

    let rtsp_entry = cameras_by_kind.get("rtsp").unwrap_or_else(|| {
        panic!(
            "cameras_by_kind must carry an entry for source kind \"rtsp\"; got body: {whole_body}"
        )
    });
    assert_eq!(
        rtsp_entry.get("count").and_then(Value::as_u64),
        Some(2),
        "two configured [[cameras]] entries, both rtsp, must report \
         cameras_by_kind.rtsp.count == 2 — not a fixed/hardcoded value — got body: {whole_body}"
    );
    let rtsp_cameras = rtsp_entry
        .get("cameras")
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            panic!("cameras_by_kind.rtsp.cameras must be a list; got body: {whole_body}")
        });
    assert_eq!(
        rtsp_cameras.len(),
        2,
        "cameras_by_kind.rtsp.cameras must list exactly the two configured cameras; got body: {whole_body}"
    );
    let listed_names: std::collections::BTreeSet<String> = rtsp_cameras
        .iter()
        .map(|entry| {
            entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("each cameras_by_kind.rtsp.cameras entry must carry a \"name\"; got body: {whole_body}"))
                .to_string()
        })
        .collect();
    let expected_names: std::collections::BTreeSet<String> = [
        FIRST_CAMERA_NAME.to_string(),
        SECOND_CAMERA_NAME.to_string(),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        listed_names, expected_names,
        "cameras_by_kind.rtsp.cameras must name exactly the two configured cameras \
         (\"{FIRST_CAMERA_NAME}\" and \"{SECOND_CAMERA_NAME}\"), never a substitute or partial \
         set; got body: {whole_body}"
    );

    let top_level_status = body
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    for camera_entry in rtsp_cameras {
        let camera_status = camera_entry
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("each cameras_by_kind.rtsp.cameras entry must carry a \"status\"; got body: {whole_body}"));
        assert_eq!(
            camera_status, top_level_status,
            "a per-camera \"status\" must agree with the same snapshot's top-level \"status\" \
             field (the runtime tracks one shared liveness state today; a per-camera entry must \
             never invent a diverging value) — got body: {whole_body}"
        );
    }

    // Kinds this artifact cannot yet carry must still appear in the
    // vocabulary, reporting zero — proving the by-kind structure is
    // complete and not merely present for whichever kind happens to be
    // configured.
    for empty_kind in ["usb", "csi", "mjpeg"] {
        let entry = cameras_by_kind.get(empty_kind).unwrap_or_else(|| {
            panic!("cameras_by_kind must carry an entry for source kind \"{empty_kind}\"; got body: {whole_body}")
        });
        assert_eq!(
            entry.get("count").and_then(Value::as_u64),
            Some(0),
            "no {empty_kind} cameras are configured, so cameras_by_kind.{empty_kind}.count must \
             be 0; got body: {whole_body}"
        );
        assert!(
            entry
                .get("cameras")
                .and_then(Value::as_array)
                .is_some_and(|list| list.is_empty()),
            "no {empty_kind} cameras are configured, so cameras_by_kind.{empty_kind}.cameras \
             must be empty; got body: {whole_body}"
        );
    }
}
