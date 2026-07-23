//! MQTT-command-to-cg product tests that need a real `SiteControl` backed by
//! a real store. `store_backed_site_control` is `pub(crate)` to core — only
//! `runtime.rs` constructs one — so proving what an MQTT command does once it
//! reaches cg means driving the real, compiled `vigil` binary end to end
//! (CLI → `runtime::run` → `site_channel.listen` → the real `vigil_ha`
//! adapter → a real broker), not constructing the seam directly from a test.
//! These moved here from `vigil-ha`'s test suite for exactly that reason.

#![cfg(feature = "first-light-acceptance")]
// This file and `mosquitto_fixture.rs` (included below) both declare the
// same shared support modules by path, in separate namespaces — the same
// pattern `ha_mqtt_broker.rs` uses.
#![allow(clippy::duplicate_mod)]

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use vigil_ha::{
    CONTROL_COMMAND_TOPIC, CORRECTION_COMMAND_TOPIC, service_availability_topic, snapshot_topic,
};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, get, json_body, vigil_binary_path, wait_for_tcp_port, wait_until,
};

#[path = "../../vigil-ha/tests/mqtt_test_probe.rs"]
mod mqtt_test_probe;
use mqtt_test_probe::MqttProbe;

#[path = "../../vigil-ha/tests/mosquitto_fixture.rs"]
mod mosquitto_fixture;
use mosquitto_fixture::MosquittoFixture;

struct Node {
    child: Child,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn correction_payload(detection_id: &str, correction_type: &str, label: Option<&str>) -> String {
    match label {
        Some(label) => format!(
            r#"{{"detection_id":"{detection_id}","correction_type":"{correction_type}","label":"{label}"}}"#
        ),
        None => format!(
            r#"{{"detection_id":"{detection_id}","correction_type":"{correction_type}","label":null}}"#
        ),
    }
}

/// Spawn the real compiled `vigil` binary wired to `broker`, pointed at a
/// pre-seeded store, cameraless (no RTSP configured). Returns the node
/// (kill-on-drop) plus its health and review ports.
fn spawn_vigil_with_mqtt(
    data_dir: &std::path::Path,
    store_path: &std::path::Path,
    service_id: &str,
    broker: &MosquittoFixture,
) -> (Node, u16, u16) {
    // Held open until the moment of spawn, not released early — a released
    // port is a race another process can steal before this one binds it.
    let health_reservation = TcpPortReservation::reserve_loopback().expect("reserve health port");
    let review_reservation = TcpPortReservation::reserve_loopback().expect("reserve review port");
    let health_port = health_reservation.port();
    let review_port = review_reservation.port();
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .env("VIGIL_DATA_DIR", data_dir)
        .env("VIGIL_STORE_PATH", store_path)
        .env("VIGIL_SITE_NAME", deterministic_fixture_support::SITE_NAME)
        .env("VIGIL_SERVICE_ID", service_id)
        .env("VIGIL_HEALTH_PORT", health_port.to_string())
        .env("VIGIL_REVIEW_PORT", review_port.to_string())
        .env("MQTT_HOST", &broker.host)
        .env("MQTT_PORT", broker.port.to_string())
        .env_remove("VIGIL_RTSP_URL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    health_reservation.release();
    review_reservation.release();
    let child = command.spawn().expect("spawn vigil binary");
    let node = Node { child };

    wait_for_tcp_port(health_port, Duration::from_secs(15)).expect("health port must open");
    wait_for_tcp_port(review_port, Duration::from_secs(15)).expect("review port must open");

    (node, health_port, review_port)
}

/// Wait for the real command subscriber to be ready to receive commands.
///
/// The adapter announces "online" TWICE: once from a one-shot connect
/// (publish "online")/disconnect helper that runs BEFORE the real,
/// long-lived command subscriber even connects, and again from that
/// subscriber's own connection, immediately after IT has subscribed to the
/// command topics (on the same connection, so TCP ordering places the
/// subscribe ahead of it). The first occurrence is not a safe readiness
/// signal — a command published right after it can arrive before the
/// subscriber exists. Wait for the second.
fn wait_for_subscriber_ready(probe: &mut MqttProbe, availability_topic: &str) {
    for attempt in 1..=2 {
        probe
            .recv_matching(
                &format!("the composed binary's adapter online (occurrence {attempt}/2)"),
                Duration::from_secs(10),
                |message| message.topic == availability_topic && message.payload == b"online",
            )
            .unwrap_or_else(|error| {
                panic!(
                    "the real binary's command subscriber must announce availability \
                     (occurrence {attempt}/2) before commands are sent: {error}"
                )
            });
    }
}

/// Drives a correction command through MQTT, the real compiled binary, the
/// real adapter, and into cg — then proves: it lands (was
/// `correction_command_on_broker_lands_in_cg`); redelivery of the identical
/// command is idempotent (was `redelivered_correction_command_is_idempotent`);
/// a second, distinct correction on the same detection also lands alongside
/// the first (was `two_distinct_corrections_on_same_detection_both_land`);
/// and a typed correction with a label on a second detection reads back with
/// its type and label intact (was `typed_correction_via_mqtt_lands_in_cg`).
/// Consolidated into one real-binary run rather than four, since each spin
/// of the composed binary against a real broker is materially more expensive
/// than the in-process form these replaced.
#[test]
fn correction_commands_via_mqtt_land_in_cg_through_the_composed_binary() {
    let (_tmp, store_path) =
        deterministic_fixture_support::fresh_store_copy(2).expect("seeded store with 2 detections");
    let data_dir = store_path.parent().expect("data dir").to_path_buf();
    let broker = MosquittoFixture::start().expect("mosquitto must start");

    let (detection_a, detection_b) = {
        let store = deterministic_fixture_support::open_store_at(&store_path)
            .expect("open store to read ids");
        let mut detections: Vec<_> = store
            .list_observations(None)
            .expect("list_observations")
            .into_iter()
            .filter(|o| o.observation_type == "detection")
            .collect();
        assert!(
            detections.len() >= 2,
            "need at least 2 detections; got {}",
            detections.len()
        );
        detections.sort_by_key(|o| o.id.to_string());
        (detections[0].id.to_string(), detections[1].id.to_string())
        // store handle drops here, before the spawned binary opens the same file.
    };

    let service_id = "mqtt-composed-product-test";
    let (_node, _health_port, review_port) =
        spawn_vigil_with_mqtt(&data_dir, &store_path, service_id, &broker);

    let availability_topic = service_availability_topic(service_id);
    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[&availability_topic],
        Duration::from_secs(5),
    )
    .expect("probe must receive SUBACK");
    wait_for_subscriber_ready(&mut probe, &availability_topic);

    // Initial correction on detection A.
    let identity_payload = correction_payload(&detection_a, "identity", Some("that's Arjun"));
    probe
        .publish_qos1(CORRECTION_COMMAND_TOPIC, &identity_payload)
        .expect("publish identity correction");
    // Redelivery of the identical command — must not double-record.
    probe
        .publish_qos1(CORRECTION_COMMAND_TOPIC, &identity_payload)
        .expect("publish redelivery");
    // A second, distinct correction on the SAME detection.
    let wrong_class_payload =
        correction_payload(&detection_a, "wrong_class", Some("actually the neighbour"));
    probe
        .publish_qos1(CORRECTION_COMMAND_TOPIC, &wrong_class_payload)
        .expect("publish wrong-class correction");
    // A typed correction with a label on a SECOND detection.
    let typed_payload = correction_payload(&detection_b, "wrong_class", Some("cat"));
    probe
        .publish_qos1(CORRECTION_COMMAND_TOPIC, &typed_payload)
        .expect("publish typed correction on detection B");
    probe
        .wait_for_pubacks(4, Duration::from_secs(5))
        .expect("broker must acknowledge all four commands");

    // Poll the composed binary's OWN review HTTP surface while it stays
    // running — a PUBACK proves the broker received the command, not that
    // this process's subscriber has finished writing it to cg. Reopening
    // the store file from a second process after killing this one would
    // also race the writer's own commit; the running binary's `/why/<id>`
    // is the correct, lock-free way to observe its own durable state.
    let why_a_path = format!("/why/{detection_a}");
    let why_a_json = wait_until(
        "detection A to carry both corrections",
        Duration::from_secs(15),
        || {
            let response = get(review_port, &why_a_path);
            if response.status != 200 {
                return Ok(None);
            }
            let body = json_body(&response);
            let corrections = body
                .get("corrections")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            Ok((corrections.len() >= 2).then_some(body))
        },
    )
    .expect("corrections must land in cg through the composed binary");

    let corrections_a = why_a_json
        .get("corrections")
        .and_then(|value| value.as_array())
        .expect("corrections array");
    assert_eq!(
        corrections_a.len(),
        2,
        "detection A must carry exactly the identity correction (redelivery deduped) plus \
         the distinct wrong_class correction; got {corrections_a:?}"
    );
    let labels: Vec<Option<&str>> = corrections_a
        .iter()
        .map(|c| c.get("label").and_then(|v| v.as_str()))
        .collect();
    assert!(
        labels.contains(&Some("that's Arjun")),
        "identity correction label must be present on detection A; got {labels:?}"
    );
    assert!(
        labels.contains(&Some("actually the neighbour")),
        "wrong_class correction label must be present on detection A; got {labels:?}"
    );

    let why_b_path = format!("/why/{detection_b}");
    let why_b_json = wait_until(
        "detection B to carry its typed correction",
        Duration::from_secs(15),
        || {
            let response = get(review_port, &why_b_path);
            if response.status != 200 {
                return Ok(None);
            }
            let body = json_body(&response);
            let has_correction = body
                .get("corrections")
                .and_then(|value| value.as_array())
                .is_some_and(|corrections| !corrections.is_empty());
            Ok(has_correction.then_some(body))
        },
    )
    .expect("detection B's typed correction must land in cg through the composed binary");
    let corrections_b = why_b_json
        .get("corrections")
        .and_then(|value| value.as_array())
        .expect("corrections array");
    assert_eq!(
        corrections_b.len(),
        1,
        "detection B must carry exactly the one typed correction; got {corrections_b:?}"
    );
    assert_eq!(
        corrections_b[0]
            .get("correction_type")
            .and_then(|v| v.as_str()),
        Some("WrongClass"),
        "detection B's correction type must be WrongClass"
    );
    assert_eq!(
        corrections_b[0].get("label").and_then(|v| v.as_str()),
        Some("cat"),
        "detection B's correction label must read back as 'cat'"
    );
}

/// An MQTT `snapshot` control command must publish the latest referenced
/// detector evidence PNG to `vigil/{camera_id}/snapshot`, driven through the
/// real compiled binary and a real broker (was the snapshot sub-check of
/// `operator_action_command_effects_action`; its disable-marker sub-check
/// is covered by `site_channel::tests` and its ack-correction sub-check by
/// `correction_commands_via_mqtt_land_in_cg_through_the_composed_binary`
/// above — this test is what is left).
#[test]
fn snapshot_command_publishes_the_latest_detector_evidence_through_the_composed_binary() {
    let (_tmp, store_path) =
        deterministic_fixture_support::fresh_store_copy(1).expect("seeded store with a detection");
    let data_dir = store_path.parent().expect("data dir").to_path_buf();
    let broker = MosquittoFixture::start().expect("mosquitto must start");

    // The seeded fixture's one camera, slugified — matches
    // `deterministic_fixture_support::CAMERA_NAME` ("lower gate").
    let camera_id = "lower-gate";
    let service_id = "mqtt-snapshot-product-test";
    let (_node, _health_port, _review_port) =
        spawn_vigil_with_mqtt(&data_dir, &store_path, service_id, &broker);

    let availability_topic = service_availability_topic(service_id);
    let expected_snapshot_topic = snapshot_topic(camera_id);
    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[&availability_topic, &expected_snapshot_topic],
        Duration::from_secs(5),
    )
    .expect("probe must receive SUBACKs");
    wait_for_subscriber_ready(&mut probe, &availability_topic);

    probe
        .publish_qos1(
            CONTROL_COMMAND_TOPIC,
            format!(r#"{{"camera_id":"{camera_id}","action":"snapshot"}}"#),
        )
        .expect("publish snapshot command");

    let snapshot = probe
        .recv_matching(
            "non-empty detector snapshot on the composed binary's snapshot topic",
            Duration::from_secs(10),
            |message| message.topic == expected_snapshot_topic && !message.payload.is_empty(),
        )
        .expect(
            "the composed binary must publish the latest referenced evidence PNG bytes \
             in response to a real snapshot command",
        );

    assert!(
        !snapshot.payload.is_empty(),
        "snapshot command must publish non-empty evidence bytes to {expected_snapshot_topic}"
    );
}
