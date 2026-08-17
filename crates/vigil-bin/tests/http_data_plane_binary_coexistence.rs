//! `crates/vigil/tests/http_data_plane.rs` proves the review/health HTTP
//! surfaces in-process, against the library entry points directly. This one
//! test is the exception that needs the real compiled binary (health and
//! review must both answer from a single running `vigil run` process), so
//! it lives with the composition root instead of core.

use std::fs;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use vigil::{EVENTS_ROUTE, HEALTH_PATH};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{TcpPortReservation, fresh_store_copy, get, json_body};

struct ChildGuard {
    child: Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn health_liveness_still_serves_alongside_data_plane_in_single_binary() {
    let (_tmp, store_path) = fresh_store_copy(1).expect("seeded store for binary coexistence");
    let data_dir = store_path.parent().expect("data dir").to_path_buf();
    let config_tmp = tempfile::TempDir::new().expect("config tempdir");
    let config_path = config_tmp.path().join("vigil-review-coexistence.toml");
    // Held open until the moment of spawn, not released early — a released
    // port is a race another process can steal before this one binds it.
    let health_reservation = TcpPortReservation::reserve_loopback().expect("reserve health port");
    let review_reservation = TcpPortReservation::reserve_loopback().expect("reserve review port");
    let health_port = health_reservation.port();
    let review_port = review_reservation.port();
    fs::write(
        &config_path,
        format!(
            "data_dir = \"{}\"\nstore_path = \"{}\"\nhealth_port = {}\nreview_port = {}\ncameras = []\n",
            deterministic_fixture_support::toml_path(&data_dir),
            deterministic_fixture_support::toml_path(&store_path),
            health_port,
            review_port,
        ),
    )
    .expect("write config");

    let mut command = Command::new(deterministic_fixture_support::vigil_binary_path());
    command
        .arg("run")
        .arg("--config")
        .arg(&config_path)
        .arg("--health-port")
        .arg(health_port.to_string())
        .arg("--review-port")
        .arg(review_port.to_string())
        .env("VIGIL_DATA_DIR", &data_dir)
        .env("VIGIL_STORE_PATH", &store_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    health_reservation.release();
    review_reservation.release();
    let child = command.spawn().expect("spawn vigil run");
    let _guard = ChildGuard { child };

    deterministic_fixture_support::wait_for_tcp_port(health_port, Duration::from_secs(10))
        .expect("health port must open");
    deterministic_fixture_support::wait_for_tcp_port(review_port, Duration::from_secs(10))
        .expect("review port must open");

    let health = get(health_port, HEALTH_PATH);
    assert!(
        matches!(health.status, 200 | 503),
        "health endpoint must return liveness status, got {}",
        health.status
    );
    let health_json = json_body(&health);
    assert!(
        health_json.get("status").is_some(),
        "/health must return its status JSON shape"
    );

    let events = get(review_port, EVENTS_ROUTE);
    assert_eq!(events.status, 200, "review data plane must serve /events");
    let events_json = json_body(&events);
    assert!(
        events_json.as_array().is_some(),
        "/events must be distinguishable from /health by returning an event row list"
    );
    assert!(
        events_json.get("status").is_none(),
        "/events must not hijack the /health status JSON shape"
    );
}
