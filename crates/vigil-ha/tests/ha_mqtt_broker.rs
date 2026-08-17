// MQTT broker integration tests.
//
// Tests that do NOT require the live detector run in both `pr` and `ci-full` profiles.
// Acceptance-only cg/MQTT seams are gated with `#[cfg(feature = "first-light-acceptance")]`.
// Detector inference quality belongs to first_light_loop.rs; this file uses
// deterministic cg detections so broker semantics do not depend on inference.
//
// REGRESSION GUARDs pass at scaffold.
// RED tests fail on assertions via the deliberate wrong stubs.

// The shared support module and this broker suite intentionally include
// the same shared test-helper source in separate module namespaces.
#![allow(clippy::duplicate_mod)]

use std::net::TcpListener;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use vigil::{CorrectionRequest, CorrectionType, HealthState, HealthStatus};
use vigil_ha::{
    CONTROL_COMMAND_TOPIC, CORRECTION_COMMAND_TOPIC, CameraConfig, MqttConfig, ServiceConfig,
    WiredSubscriberConfig, generate_discovery_payloads, mqtt_connect_intent,
    publish_discovery_to_broker, spawn_detection_publisher, spawn_production_subscriber,
    spawn_site_presence,
};

// Reused from the core crate rather than copied: this generic TCP/process
// test-fixture helper carries no adapter-specific content.
#[path = "../../vigil/tests/deterministic_test_support.rs"]
mod deterministic_test_support;
use deterministic_test_support::{TcpPortReservation, wait_until};

// MQTT-specific: stays local to this crate rather than in core's shared
// support (core's test builds should be as MQTT-free as its production
// builds).
#[path = "mqtt_test_probe.rs"]
mod mqtt_test_probe;
use mqtt_test_probe::MqttProbe;

// ── Mosquitto fixture ─────────────────────────────────────────────────────
// Shared cross-crate (`vigil-bin`'s own broker-backed product tests reuse it
// by path) rather than duplicated.

#[path = "mosquitto_fixture.rs"]
mod mosquitto_fixture;
use mosquitto_fixture::MosquittoFixture;

// ── Helpers ────────────────────────────────────────────────────────────────

/// Build a minimal `WiredSubscriberConfig` for tests.
/// Availability and condition topics are test-only placeholders.
fn test_subscriber_cfg(mqtt: MqttConfig) -> WiredSubscriberConfig {
    WiredSubscriberConfig {
        mqtt,
        service_id: "test".to_string(),
        client_id: "vigil-test-sub".to_string(),
        availability_topic: "vigil/test/availability".to_string(),
        condition_topic: "vigil/test/condition".to_string(),
        discovery_payloads: vec![],
        health: HealthState::new(),
    }
}

#[cfg(feature = "first-light-acceptance")]
fn subscriber_ready_probe(broker: &MosquittoFixture) -> MqttProbe {
    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &["vigil/test/availability"],
        Duration::from_secs(2),
    )
    .expect("subscriber readiness probe must receive SUBACK");
    probe
        .recv_matching(
            "production subscriber online",
            Duration::from_secs(3),
            |message| message.topic == "vigil/test/availability" && message.payload == b"online",
        )
        .expect("production subscriber must acknowledge readiness");
    probe
}

fn sample_service_config() -> ServiceConfig {
    ServiceConfig {
        service_name: "Vigil".to_string(),
        service_id: "vigil-home-farm".to_string(),
        cameras: vec![CameraConfig {
            camera_id: "cam-lower-gate".to_string(),
            camera_label: "lower gate".to_string(),
        }],
    }
}

fn correction_command_payload(detection_id: &str) -> String {
    format!(r#"{{"detection_id":"{detection_id}","correction_type":"false_alarm","label":null}}"#)
}

// ── Fake SiteControl ─────────────────────────────────────────────────────
// A trait-object test double: this is what a trait seam is for. The adapter's
// own tests exercise it against the wire (MQTT topics, discovery, command
// parsing and dispatch), never against a real store — a broker test that
// genuinely needs to prove a correction landed in cg is exercising the
// composed product and belongs with `vigil-bin`'s suites instead.
struct FakeSiteControl {
    camera_states: Mutex<std::collections::BTreeMap<String, bool>>,
    images: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
    corrections: Mutex<Vec<CorrectionRequest>>,
    correction_capacity: usize,
}

impl FakeSiteControl {
    fn new(cameras: &[(&str, bool)]) -> Self {
        Self {
            camera_states: Mutex::new(
                cameras
                    .iter()
                    .map(|(id, enabled)| (id.to_string(), *enabled))
                    .collect(),
            ),
            images: Mutex::new(std::collections::BTreeMap::new()),
            corrections: Mutex::new(Vec::new()),
            correction_capacity: usize::MAX,
        }
    }

    fn with_correction_capacity(cameras: &[(&str, bool)], capacity: usize) -> Self {
        Self {
            correction_capacity: capacity,
            ..Self::new(cameras)
        }
    }

    fn camera_state(&self, camera_id: &str) -> Option<bool> {
        self.camera_states.lock().unwrap().get(camera_id).copied()
    }

    // Only the acceptance-gated malformed-command test reads this back.
    #[cfg_attr(not(feature = "first-light-acceptance"), allow(dead_code))]
    fn submitted_corrections(&self) -> Vec<CorrectionRequest> {
        self.corrections.lock().unwrap().clone()
    }
}

impl vigil::SiteControl for FakeSiteControl {
    fn camera_enabled_states(&self) -> Vec<(String, bool)> {
        self.camera_states
            .lock()
            .unwrap()
            .iter()
            .map(|(id, enabled)| (id.clone(), *enabled))
            .collect()
    }

    fn set_camera_enabled(&self, camera_id: &str, enabled: bool) -> bool {
        let mut states = self.camera_states.lock().unwrap();
        if let Some(state) = states.get_mut(camera_id) {
            *state = enabled;
            true
        } else {
            false
        }
    }

    fn latest_detection_image(&self, camera_id: &str) -> Option<Vec<u8>> {
        self.images.lock().unwrap().get(camera_id).cloned()
    }

    fn submit_correction(
        &self,
        request: CorrectionRequest,
    ) -> Result<(), vigil::SubmitCorrectionError> {
        let mut corrections = self.corrections.lock().unwrap();
        if corrections.len() >= self.correction_capacity {
            return Err(vigil::SubmitCorrectionError::QueueFull);
        }
        corrections.push(request);
        Ok(())
    }
}

// ── REGRESSION GUARD ──────────────────────────────────────────────────────

#[test]
fn tcp_reservation_holds_port_until_release() {
    let reservation = TcpPortReservation::reserve_loopback().expect("reserve port");
    let port = reservation.port();
    assert!(TcpListener::bind(("127.0.0.1", port)).is_err());
    assert_eq!(reservation.release(), port);
}

#[test]
fn bounded_wait_reports_the_missing_state() {
    let error = wait_until("the named state", Duration::from_millis(1), || {
        Ok::<Option<()>, String>(None)
    })
    .expect_err("wait must time out");
    assert!(error.contains("the named state"), "{error}");
}

/// No detection notification → no event published to the broker.
///
/// The same production publisher first delivers a causal positive-control
/// message. Only after that ConnAck-backed receipt do we observe the detection
/// topic and require silence, so a disconnected publisher cannot false-PASS.
#[test]
fn empty_stream_publishes_no_event_to_broker() {
    let broker = MosquittoFixture::start().expect("Mosquitto must start for no-event test");
    let control_topic = "vigil/test/no-event-positive-control";
    let event_topic = "vigil/test/empty-camera/detection";
    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[control_topic, event_topic],
        Duration::from_secs(2),
    )
    .expect("no-event probe must receive SUBACKs");
    let health = HealthState::new();
    let (publisher, handle) = spawn_detection_publisher(&broker.mqtt_config(), health);

    publisher.try_publish(control_topic.to_string(), "publisher-ready".to_string());
    probe
        .recv_matching(
            "production publisher positive control",
            Duration::from_secs(3),
            |message| message.topic == control_topic && message.payload == b"publisher-ready",
        )
        .expect("the same production publisher must prove it is connected before silence counts");

    let unexpected = probe.recv_matching(
        "an event that must remain absent without a detection notification",
        Duration::from_millis(500),
        |message| message.topic == event_topic,
    );
    handle.shutdown_and_join();
    assert!(
        unexpected.is_err(),
        "production publisher emitted a detection event without a detection notification: {unexpected:?}"
    );
}

// ── RED: non-acceptance MQTT tests ────────────────────────────────────────

/// RED — wrong stub `publish_discovery_to_broker` is a no-op;
/// bounded subscribe receives nothing → assertion fails.
#[test]
fn discovery_published_to_real_broker_on_start() {
    let broker = MosquittoFixture::start().expect("Mosquitto must start for discovery test");

    let config = sample_service_config();
    let payloads = generate_discovery_payloads(&config);
    assert!(
        !payloads.is_empty(),
        "generate_discovery_payloads must return at least one payload"
    );
    assert!(
        payloads
            .iter()
            .all(|payload| payload.topic.starts_with("homeassistant/")
                && !payload.topic.starts_with("frigate/")),
        "Vigil discovery must publish only below homeassistant/# and must never contaminate frigate/#"
    );

    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &["homeassistant/#"],
        Duration::from_secs(2),
    )
    .expect("discovery probe must receive SUBACK");

    // Call the publisher (wrong stub: no-op, does not connect or publish).
    let result = publish_discovery_to_broker(&broker.mqtt_config(), &payloads);
    assert!(
        result.is_ok(),
        "publish_discovery_to_broker returned Err: {:?}",
        result.err()
    );

    probe
        .recv_matching(
            "a Home Assistant discovery publish",
            Duration::from_secs(2),
            |message| message.topic.starts_with("homeassistant/") && !message.payload.is_empty(),
        )
        .unwrap_or_else(|error| {
            panic!("expected {} discovery payload(s): {error}", payloads.len())
        });
}

/// RED — wrong stub always returns true; `mqtt_connect_intent(None)` must return false.
#[test]
fn mqtt_gated_off_when_no_broker_configured() {
    let intent_none = mqtt_connect_intent(None);
    assert!(
        !intent_none,
        "mqtt_connect_intent(None) must return false when no broker is configured; \
         wrong stub ignores the argument and always returns true"
    );

    // With a config present, intent must be true (both stub and correct impl).
    let config = MqttConfig {
        broker_host: "127.0.0.1".to_string(),
        broker_port: 1883,
        username: None,
        password: None,
    };
    assert!(
        mqtt_connect_intent(Some(&config)),
        "mqtt_connect_intent(Some(config)) must return true when a broker is configured"
    );
}

/// RED — wrong stub subscriber never connects; overflow_count stays 0
/// regardless of how many commands are published to the broker.
#[test]
fn correction_command_channel_overflow_is_loud() {
    let broker = MosquittoFixture::start().expect("Mosquitto must start for overflow test");

    let overflow_count = Arc::new(AtomicUsize::new(0));
    // Bounded capacity 1; the fake never drains, so it stays full after the
    // first submission and every subsequent one reports QueueFull.
    let control = Arc::new(FakeSiteControl::with_correction_capacity(&[], 1));

    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        control,
        Arc::clone(&overflow_count),
    );

    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &["vigil/test/availability"],
        Duration::from_secs(2),
    )
    .expect("overflow probe must receive SUBACK");
    probe
        .recv_matching(
            "production subscriber online",
            Duration::from_secs(3),
            |message| message.topic == "vigil/test/availability" && message.payload == b"online",
        )
        .expect("production subscriber must acknowledge readiness");

    // Flood the command topic well beyond the bounded capacity.
    let payload = correction_command_payload("aabbccdd-1111-2222-3333-444455556666");
    for _ in 0..20 {
        probe
            .publish_qos1("vigil/commands/correct", &payload)
            .expect("publish overflow command");
    }
    probe
        .wait_for_pubacks(20, Duration::from_secs(3))
        .expect("broker must acknowledge overflow commands");

    wait_until(
        "correction overflow counter",
        Duration::from_secs(3),
        || Ok((overflow_count.load(Ordering::SeqCst) > 0).then_some(())),
    )
    .expect("overflow must become observable");
    handle.shutdown_and_join();

    let overflow = overflow_count.load(Ordering::SeqCst);
    assert!(
        overflow > 0,
        "overflow_count must be > 0 after flooding the correction command channel \
         beyond its bound; wrong stub never connects to the broker (overflow = {overflow})"
    );
}

#[test]
fn camera_enabled_switch_state_is_retained_and_updates_on_control_commands() {
    let broker = MosquittoFixture::start().expect("Mosquitto must start for control test");

    let state_topic = "vigil/test/lower-gate/enabled".to_string();
    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[&state_topic],
        Duration::from_secs(2),
    )
    .expect("control probe must receive SUBACK");

    let overflow_count = Arc::new(AtomicUsize::new(0));
    let control = Arc::new(FakeSiteControl::new(&[
        ("lower-gate", true),
        ("driveway", false),
    ]));
    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        Arc::clone(&control) as Arc<dyn vigil::SiteControl>,
        Arc::clone(&overflow_count),
    );

    let initial = probe
        .recv_matching(
            "initial retained ON state",
            Duration::from_secs(3),
            |message| message.topic == state_topic && message.payload == b"ON",
        )
        .expect("production subscriber must publish initial ON state");
    assert_eq!(initial.payload, b"ON");
    // A live delivery may clear the RETAIN bit. A new subscription proves the
    // broker actually stored the state as retained.
    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[&state_topic],
        Duration::from_secs(2),
    )
    .expect("late control probe must receive SUBACK");
    let retained = probe
        .recv_matching(
            "broker-retained initial ON state",
            Duration::from_secs(2),
            |message| message.topic == state_topic && message.payload == b"ON",
        )
        .expect("late subscriber must receive retained ON state");
    assert!(
        retained.retain,
        "late subscriber must observe retained state"
    );
    probe
        .publish_qos1(
            "vigil/commands/control",
            r#"{"service_id":"test","camera_id":"lower-gate","action":"disable"}"#,
        )
        .expect("publish disable command");
    let disabled = probe
        .recv_matching("retained OFF state", Duration::from_secs(3), |message| {
            message.topic == state_topic && message.payload == b"OFF"
        })
        .expect("disable command must publish OFF");
    assert_eq!(disabled.payload, b"OFF");
    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[&state_topic],
        Duration::from_secs(2),
    )
    .expect("late disabled-state probe must receive SUBACK");
    let retained_disabled = probe
        .recv_matching(
            "broker-retained OFF state",
            Duration::from_secs(2),
            |message| message.topic == state_topic && message.payload == b"OFF",
        )
        .expect("late subscriber must receive retained OFF state");
    assert!(
        retained_disabled.retain,
        "late subscriber must observe retained OFF"
    );
    probe
        .publish_qos1(
            "vigil/commands/control",
            r#"{"service_id":"test","camera_id":"lower-gate","action":"enable"}"#,
        )
        .expect("publish enable command");
    let enabled = probe
        .recv_matching(
            "retained ON state after enable",
            Duration::from_secs(3),
            |message| message.topic == state_topic && message.payload == b"ON",
        )
        .expect("enable command must publish ON");
    assert_eq!(enabled.payload, b"ON");
    let mut retained_probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[&state_topic],
        Duration::from_secs(2),
    )
    .expect("late enabled-state probe must receive SUBACK");
    let retained_enabled = retained_probe
        .recv_matching(
            "broker-retained final ON state",
            Duration::from_secs(2),
            |message| message.topic == state_topic && message.payload == b"ON",
        )
        .expect("late subscriber must receive retained final ON state");
    assert!(
        retained_enabled.retain,
        "late subscriber must observe retained ON"
    );
    handle.shutdown_and_join();
    assert_eq!(
        control.camera_state("lower-gate"),
        Some(true),
        "enable command must restore the live lower-gate enabled flag"
    );
    assert_eq!(
        control.camera_state("driveway"),
        Some(false),
        "lower-gate commands must not change the driveway enabled flag"
    );
}

/// The correction authority is a local-store module. Keep network clients out of
/// that source boundary; the HA-OS TH-23 acceptance separately traces a successful
/// correction on the isolated writer after real broker ingress.
#[test]
fn correction_path_makes_no_outbound_network_beyond_broker() {
    let fingerprint_fixture = CorrectionRequest {
        detection_id: "11111111-2222-4333-8444-555555555555".to_string(),
        label: Some("th23-no-egress-fixture".to_string()),
        correction_type: CorrectionType::FalseAlarm,
    };
    assert_eq!(
        vigil::correction_execution_fingerprint(&fingerprint_fixture),
        "79cf8fef8aa139bab0ac4b2411c8e4450673aab7eddfdc026b205adf3b271c3a",
        "the production fingerprint bytes must match TH-23's shell correlation contract"
    );
    let different_owner_label = CorrectionRequest {
        label: Some("a private owner-entered name".to_string()),
        ..fingerprint_fixture
    };
    assert_eq!(
        vigil::correction_execution_fingerprint(&different_owner_label),
        "79cf8fef8aa139bab0ac4b2411c8e4450673aab7eddfdc026b205adf3b271c3a",
        "an unkeyed execution fingerprint must not make owner-entered labels dictionary-testable"
    );
}

// ── RED: acceptance tests ─────────────────────────────────────────────────

/// RED — wrong stub subscriber never delivers; malformed commands are never
/// rejected because they never arrive; subsequent well-formed commands also
/// never arrive.
#[cfg(feature = "first-light-acceptance")]
#[test]
fn malformed_correction_command_is_rejected_and_subscriber_survives() {
    let broker = MosquittoFixture::start().expect("mosquitto");

    let overflow_count = Arc::new(AtomicUsize::new(0));
    let control = Arc::new(FakeSiteControl::new(&[]));
    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        Arc::clone(&control) as Arc<dyn vigil::SiteControl>,
        Arc::clone(&overflow_count),
    );

    let mut probe = subscriber_ready_probe(&broker);

    // Publish malformed JSON first.
    probe
        .publish_qos1("vigil/commands/correct", "NOT_JSON{{{{")
        .expect("publish malformed command");

    // Then publish a well-formed command — subscriber must survive and deliver it.
    let well_formed = correction_command_payload("aabbccdd-0000-1111-2222-333344445555");
    probe
        .publish_qos1("vigil/commands/correct", &well_formed)
        .expect("publish valid command");
    probe
        .wait_for_pubacks(2, Duration::from_secs(2))
        .expect("broker must acknowledge malformed and valid commands");
    wait_until(
        "the well-formed correction submitted to the fake control",
        Duration::from_secs(3),
        || Ok((!control.submitted_corrections().is_empty()).then_some(())),
    )
    .expect("valid command must be delivered");
    handle.shutdown_and_join();

    let submitted = control.submitted_corrections();
    assert_eq!(
        submitted.len(),
        1,
        "subscriber must discard malformed input and submit only the following valid command"
    );
    assert_eq!(
        submitted[0].detection_id, "aabbccdd-0000-1111-2222-333344445555",
        "subscriber must discard malformed input and deliver the following valid command"
    );
}

// ── Fix B: live running-condition tracks health ────────────────────────────

/// RED — wrong stub publishes hardcoded "running" regardless of health;
/// subscriber must publish the mapped condition when health changes.
#[test]
fn running_condition_tracks_live_health_retained() {
    let broker = MosquittoFixture::start().expect("Mosquitto must start for reconnect test");

    let health = HealthState::new();
    health.set(HealthStatus::Ready, "test start");

    let overflow = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let condition_topic = "vigil/test-health-svc/running-condition".to_string();

    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[&condition_topic],
        Duration::from_secs(2),
    )
    .expect("condition probe must receive SUBACK");

    let cfg = WiredSubscriberConfig {
        mqtt: broker.mqtt_config(),
        service_id: "test-health-svc".to_string(),
        client_id: "vigil-test-health-sub".to_string(),
        availability_topic: "vigil/test-health-svc/availability".to_string(),
        condition_topic: condition_topic.clone(),
        discovery_payloads: vec![],
        health: health.clone(),
    };
    let control = Arc::new(FakeSiteControl::new(&[]));
    let handle = spawn_production_subscriber(cfg, control, overflow);

    probe
        .recv_matching(
            "initial running condition",
            Duration::from_secs(3),
            |message| message.topic == condition_topic && message.payload == b"running",
        )
        .expect("subscriber must publish the initial health mapping");

    // Flip health to DiskFull — subscriber must detect the change and publish "disk-full".
    health.set(HealthStatus::DiskFull, "disk full test");

    probe
        .recv_matching(
            "disk-full running condition",
            Duration::from_secs(3),
            |message| message.topic == condition_topic && message.payload == b"disk-full",
        )
        .expect("subscriber must publish changed health mapping");
    handle.shutdown_and_join();
}

// ── Fix C: long-lived detection publisher + bounded channel + loud overflow ─

/// A held non-MQTT endpoint keeps the worker behind its ConnAck barrier. This proves
/// detector-side publication uses a bounded non-blocking channel: the caller can fill
/// it, overflow is loud, and no ambient "unused port" or scheduler deadline is an oracle.
#[test]
fn outbound_detection_publish_is_nonblocking_and_overflow_is_loud() {
    let endpoint = TcpPortReservation::reserve_loopback()
        .expect("hold a real loopback endpoint that deliberately never speaks MQTT");
    let config = MqttConfig {
        broker_host: "127.0.0.1".to_string(),
        broker_port: endpoint.port(),
        username: None,
        password: None,
    };
    let health = HealthState::new();
    health.set(HealthStatus::Ready, "test");

    let (publisher, handle) = spawn_detection_publisher(&config, health.clone());

    // Fill the channel beyond capacity — all sends after the 32nd must overflow.
    for i in 0..50 {
        publisher.try_publish(
            format!("vigil/test/{i}/detection"),
            r#"{"test":true}"#.to_string(),
        );
    }

    handle.shutdown_and_join();

    let overflow = publisher.overflow_count.load(Ordering::SeqCst);
    assert!(
        overflow > 0,
        "try_publish must increment overflow_count when the channel is full; \
         wrong impl silently drops without incrementing — got overflow={overflow}"
    );

    // Health must have been flipped to KeepPaceFailed on overflow.
    let (status, _) = health.snapshot();
    assert_eq!(
        status,
        HealthStatus::KeepPaceFailed,
        "overflow must flip health to KeepPaceFailed; \
         wrong impl leaves health unchanged — got status={status:?}"
    );
}

// ── Storeless presence: a degraded start reaches Home Assistant ────────────
// (cold-review-r4 finding 1) ────────────────────────────────────────────────

fn presence_cfg(
    broker: &MosquittoFixture,
    service_id: &str,
    health: HealthState,
) -> WiredSubscriberConfig {
    let config = ServiceConfig {
        service_name: "Vigil".to_string(),
        service_id: service_id.to_string(),
        cameras: vec![CameraConfig {
            camera_id: "cam-lower-gate".to_string(),
            camera_label: "lower gate".to_string(),
        }],
    };
    WiredSubscriberConfig {
        mqtt: broker.mqtt_config(),
        service_id: service_id.to_string(),
        client_id: format!("vigil-{service_id}-presence"),
        availability_topic: format!("vigil/{service_id}/availability"),
        condition_topic: format!("vigil/{service_id}/running-condition"),
        discovery_payloads: generate_discovery_payloads(&config),
        health,
    }
}

/// Unfakeable because it drives the SAME public entry point a storeless run
/// calls instead of `spawn_production_subscriber` (`spawn_site_presence`, no
/// `SiteControl` handed in at all — structurally nothing to dispatch a
/// command onto) and observes the real broker traffic: retained discovery
/// under `homeassistant/#`, retained `online` availability, and the
/// health-mapped running condition — the three things a degraded start used
/// to leave unpublished because they lived only in `listen`, never in
/// `connect` (finding 1).
#[test]
fn storeless_presence_publishes_discovery_availability_and_running_condition() {
    let broker =
        MosquittoFixture::start().expect("Mosquitto must start for the presence publish test");
    let service_id = "presence-publish-svc";

    let health = HealthState::new();
    health.declare_unmanaged("store unreadable: presence publish test");
    health.set(HealthStatus::RunningUnmanaged, "storeless start");

    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[
            "homeassistant/#",
            &format!("vigil/{service_id}/availability"),
            &format!("vigil/{service_id}/running-condition"),
        ],
        Duration::from_secs(2),
    )
    .expect("presence probe must receive SUBACKs");

    let handle = spawn_site_presence(presence_cfg(&broker, service_id, health));

    probe
        .recv_matching(
            "a Home Assistant discovery publish from the presence connection",
            Duration::from_secs(3),
            |message| message.topic.starts_with("homeassistant/") && !message.payload.is_empty(),
        )
        .expect("a storeless start must publish discovery, exactly as a healthy start does");
    probe
        .recv_matching(
            "retained online availability from the presence connection",
            Duration::from_secs(3),
            |message| {
                message.topic == format!("vigil/{service_id}/availability")
                    && message.payload == b"online"
            },
        )
        .expect(
            "a storeless start must publish retained `online` availability, not leave every \
             entity unavailable",
        );
    probe
        .recv_matching(
            "the running-unmanaged condition from the presence connection",
            Duration::from_secs(3),
            |message| {
                message.topic == format!("vigil/{service_id}/running-condition")
                    && message.payload == b"running-unmanaged"
            },
        )
        .expect(
            "the RunningUnmanaged mapping this arc added must actually reach the condition \
             topic on a storeless start, not sit unreachable",
        );

    handle.shutdown_and_join();
}

/// Unfakeable in the same way as `camera_enabled_switch_state_is_retained_and_updates_on_control_commands`
/// above: a well-formed control command is published to the broker while
/// the presence connection is the only subscriber that could act on it, and
/// the camera's retained enabled-state topic — the one observable surface a
/// dispatched command would touch — never changes. A build that quietly wired
/// `spawn_site_presence` to the same command handling as
/// `spawn_production_subscriber` would fail this the moment a real command
/// landed.
#[test]
fn storeless_presence_never_reflects_a_control_command_it_has_no_door_to_serve() {
    let broker = MosquittoFixture::start().expect("Mosquitto must start for the no-command test");
    let service_id = "presence-no-command-svc";
    let state_topic = format!("vigil/{service_id}/cam-lower-gate/enabled");

    let health = HealthState::new();
    health.declare_unmanaged("store unreadable: no-command test");
    health.set(HealthStatus::RunningUnmanaged, "storeless start");

    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[
            format!("vigil/{service_id}/availability").as_str(),
            state_topic.as_str(),
        ],
        Duration::from_secs(2),
    )
    .expect("presence probe must receive SUBACKs");

    let handle = spawn_site_presence(presence_cfg(&broker, service_id, health));
    probe
        .recv_matching(
            "the presence connection online before sending the command",
            Duration::from_secs(3),
            |message| {
                message.topic == format!("vigil/{service_id}/availability")
                    && message.payload == b"online"
            },
        )
        .expect("presence must be connected before the command is sent");

    probe
        .publish_qos1(
            CONTROL_COMMAND_TOPIC,
            r#"{"service_id":"presence-no-command-svc","camera_id":"cam-lower-gate","action":"disable"}"#,
        )
        .expect("publish a control command the presence connection is not subscribed to serve");
    // A correction command too, on general principle: neither command topic
    // has a listener behind a presence-only connection.
    probe
        .publish_qos1(
            CORRECTION_COMMAND_TOPIC,
            r#"{"detection_id":"aabbccdd-1111-2222-3333-444455556666","correction_type":"false_alarm","label":null}"#,
        )
        .expect("publish a correction command the presence connection is not subscribed to serve");

    let reflected = probe.recv_matching(
        "an enabled-state reflection that must never arrive from a presence-only connection",
        Duration::from_millis(700),
        |message| message.topic == state_topic,
    );
    handle.shutdown_and_join();
    assert!(
        reflected.is_err(),
        "the presence connection dispatched a command it has no SiteControl door to serve: \
         {reflected:?}"
    );
}

/// Unfakeable because it observes the broker's OWN last-will publish, not a
/// client-side claim: the presence connection is torn down the same way an
/// unclean process death would (`shutdown_and_join` drops the client without
/// issuing a DISCONNECT), and only the broker firing the last-will it was
/// configured with at CONNECT time can make `offline` appear, retained, on
/// the availability topic afterward.
#[test]
fn storeless_presence_carries_a_last_will_that_marks_it_offline_on_unclean_disconnect() {
    let broker = MosquittoFixture::start().expect("Mosquitto must start for the last-will test");
    let service_id = "presence-last-will-svc";
    let availability_topic = format!("vigil/{service_id}/availability");

    let health = HealthState::new();
    health.declare_unmanaged("store unreadable: last-will test");
    health.set(HealthStatus::RunningUnmanaged, "storeless start");

    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[availability_topic.as_str()],
        Duration::from_secs(2),
    )
    .expect("last-will probe must receive SUBACK");

    let handle = spawn_site_presence(presence_cfg(&broker, service_id, health));
    probe
        .recv_matching(
            "retained online availability before the unclean disconnect",
            Duration::from_secs(3),
            |message| message.topic == availability_topic && message.payload == b"online",
        )
        .expect("presence must announce online before it is torn down");

    // No clean DISCONNECT is sent on this path — the broker sees the TCP
    // connection drop and fires the last-will it recorded at CONNECT time.
    handle.shutdown_and_join();

    probe
        .recv_matching(
            "the broker's own last-will publish of retained offline availability",
            Duration::from_secs(5),
            |message| message.topic == availability_topic && message.payload == b"offline",
        )
        .expect(
            "an unclean death of the presence connection must leave `offline` retained on the \
             availability topic, so Home Assistant marks this node's entities unavailable",
        );
}

// ── GAP 3: motion binary_sensor active feed ───────────────────────────────

/// RED — wrong stub: notify_active is a no-op (missing channel write) so nothing
/// arrives on the active topic.  The production path: detection fires →
/// notify_active → publisher publishes "ON" retained to the camera's active topic.
#[test]
fn detection_fires_active_sensor_on() {
    let broker = MosquittoFixture::start().expect("Mosquitto must start for active-state test");

    let active_topic = "vigil/home-farm/lower-gate/active";

    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[active_topic],
        Duration::from_secs(2),
    )
    .expect("active-state probe must receive SUBACK");

    let health = HealthState::new();
    health.set(HealthStatus::Ready, "test");
    let (publisher, handle) = spawn_detection_publisher(&broker.mqtt_config(), health);

    publisher.notify_active(active_topic.to_string());
    probe
        .recv_matching("active ON state", Duration::from_secs(5), |message| {
            message.topic == active_topic && message.payload == b"ON"
        })
        .expect("notify_active must publish ON");
    handle.shutdown_and_join();
}

// ── Item 6: long-lived publisher happy path + typed E2E correction ─────────

/// RED — the overflow and non-block tests only cover failure paths; the PRODUCTION
/// path (spawn_detection_publisher → try_publish → broker receipt) was exercised by
/// the old per-connection publish_detection_event, not by the new publisher.
/// Wrong stub: try_publish puts the message on the channel but the background thread
/// never calls client.publish (broken publish loop) → subscriber sees nothing.
#[cfg(feature = "first-light-acceptance")]
#[test]
fn detection_publisher_delivers_to_broker() {
    let broker = MosquittoFixture::start().expect("Mosquitto must start for queue test");

    let topic = "vigil/test/camera-1/detection";
    let payload = r#"{"class_name":"person","confidence":0.95}"#;

    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[topic],
        Duration::from_secs(2),
    )
    .expect("detection publisher probe must receive SUBACK");

    let health = HealthState::new();
    health.set(HealthStatus::Ready, "test");
    let (publisher, handle) = spawn_detection_publisher(&broker.mqtt_config(), health);

    publisher.try_publish(topic.to_string(), payload.to_string());
    probe
        .recv_matching(
            "queued detection publish",
            Duration::from_secs(5),
            |message| message.topic == topic && message.payload == payload.as_bytes(),
        )
        .expect("detection publisher must deliver queued payload");
    handle.shutdown_and_join();
}
