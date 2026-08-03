//! An artifact with zero cameras configured is a legitimate deployment (a
//! worker or discovery node), but its own `/health` endpoint must say so
//! STRUCTURALLY rather than presenting as an ordinary, camera-serving,
//! unqualified "ready" box — on a camera product, a box that reports it is
//! fine while watching nothing is the worst failure mode there is.
//!
//! Superseded statement (ruling, recorded so the change is never mistaken
//! for a quiet drop): this test previously asserted the bare body substring
//! `"no cameras configured"`. The `/health` body now carries a
//! `cameras_by_kind` structure — one entry per camera source kind (rtsp,
//! usb, csi, mjpeg), each with a `count` and a `cameras` list — rich enough
//! for a future UI to consume directly instead of parsing prose. A
//! cameraless node shows zero counts and empty lists across EVERY kind, and
//! that structural fact *is* the cameraless statement now; the property
//! (never presenting as an ordinary camera-serving box) is unchanged, only
//! how it is expressed. See `crates/vigil-bin/tests/camera_health_by_kind.rs`
//! for the companion proof that a populated node reports genuine non-zero
//! counts, so this file's zeros cannot be a hardcoded constant.
//!
//! This drives the real, composed `vigil` binary end to end: a config file
//! declaring `cameras = []` (an explicit, deliberate empty list, never an
//! omitted field), through `vigil run`'s own boot path and its own raw
//! `/health` HTTP responder — not a helper reimplementing that logic.

use std::fs;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, get, json_body, vigil_binary_path, wait_until,
};

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

fn spawn_cameraless_node(
    data_dir: &std::path::Path,
    health_reservation: TcpPortReservation,
    review_reservation: TcpPortReservation,
) -> Node {
    let health_port = health_reservation.port();
    let review_port = review_reservation.port();
    let config_path = data_dir.join("vigil.toml");
    fs::write(&config_path, "cameras = []\n").expect("write cameraless config");

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

#[test]
fn a_cameraless_node_states_no_cameras_configured_on_health_and_never_bare_ready() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let health_reservation = TcpPortReservation::reserve_loopback().expect("reserve health port");
    let review_reservation = TcpPortReservation::reserve_loopback().expect("reserve review port");
    let health_port = health_reservation.port();
    let node = spawn_cameraless_node(data_dir.path(), health_reservation, review_reservation);

    let boot_reached_pipeline_up = wait_until(
        "the cameraless node to report boot_phase=pipeline-up",
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
    let body = json_body(&response);
    let boot_logs = node.stdout();
    node.kill_and_wait();

    assert_eq!(
        response.status, 200,
        "an explicitly cameraless node (a legitimate worker/discovery deployment) is alive and \
         must still answer the liveness probe with 2xx; logs:\n{boot_logs}"
    );

    let whole_body = response.body_text();

    // The cameraless statement is now structural: a `cameras_by_kind` object
    // keyed by every camera source kind this artifact knows about, each
    // reporting zero configured cameras and an empty per-camera list. This
    // is asserted against the FIXED kind vocabulary (`CameraSourceKind`'s
    // own labels), never against anything this fixture's config chose, so
    // it cannot be satisfied by fixture-vocabulary coincidence.
    let cameras_by_kind = body
        .get("cameras_by_kind")
        .and_then(Value::as_object)
        .unwrap_or_else(|| {
            panic!(
                "the /health body for an explicitly cameraless node must carry a \"cameras_by_kind\" \
                 object; got body: {whole_body}"
            )
        });
    for kind in ["rtsp", "usb", "csi", "mjpeg"] {
        let entry = cameras_by_kind.get(kind).unwrap_or_else(|| {
            panic!(
                "cameras_by_kind must carry an entry for source kind \"{kind}\"; got body: {whole_body}"
            )
        });
        let count = entry.get("count").and_then(Value::as_u64);
        assert_eq!(
            count,
            Some(0),
            "an explicitly cameraless node must report cameras_by_kind.{kind}.count == 0 — \
             zero counts across EVERY kind is the structural cameraless statement; got body: {whole_body}"
        );
        let cameras = entry
            .get("cameras")
            .and_then(Value::as_array)
            .unwrap_or_else(|| {
                panic!("cameras_by_kind.{kind}.cameras must be a list; got body: {whole_body}")
            });
        assert!(
            cameras.is_empty(),
            "an explicitly cameraless node must report an empty cameras_by_kind.{kind}.cameras \
             list; got body: {whole_body}"
        );
    }

    let status_field = body
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_ne!(
        status_field, "ready",
        "a cameraless node's /health \"status\" field must never be the bare, unqualified \
         \"ready\" label an ordinary camera-serving box reports — a box that watches nothing \
         must never present as ordinarily healthy; got body: {whole_body}"
    );
}
