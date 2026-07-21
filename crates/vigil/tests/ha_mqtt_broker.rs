// MQTT broker integration tests.
//
// Tests that do NOT require the live detector run in both `pr` and `ci-full` profiles.
// Acceptance-only cg/MQTT seams are gated with `#[cfg(feature = "first-light-acceptance")]`.
// Detector inference quality belongs to first_light_loop.rs; this file uses
// deterministic cg detections so broker semantics do not depend on inference.
//
// REGRESSION GUARDs pass at scaffold.
// RED tests fail on assertions via the deliberate wrong stubs.

// The acceptance support module and this broker suite intentionally include
// the same shared test-helper source in separate module namespaces.
#![cfg_attr(feature = "first-light-acceptance", allow(clippy::duplicate_mod))]

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::Duration;

use context_graph::{EmbedderConfig, Store, StoreConfig};
use vigil::{
    CameraConfig, CorrectionRequest, CorrectionType, HealthState, HealthStatus, MqttConfig,
    ServiceConfig, WiredSubscriberConfig, generate_discovery_payloads, mqtt_connect_intent,
    publish_discovery_to_broker, spawn_detection_publisher, spawn_production_subscriber,
};

#[path = "deterministic_test_support.rs"]
mod deterministic_test_support;
use deterministic_test_support::{MqttProbe, TcpPortReservation, required_tool, wait_until};

// ── Acceptance-only imports ────────────────────────────────────────────────
#[cfg(feature = "first-light-acceptance")]
use vigil::{
    CorrectionError, DetectionInput, map_detection_to_event_payload, publish_detection_event,
    record_correction, review_events, review_why,
};

// ── Acceptance test support (deterministic cg fixture) ────────────────────

#[cfg(feature = "first-light-acceptance")]
#[path = "ha_test_support.rs"]
mod ha_test_support;

// ── Mosquitto fixture ─────────────────────────────────────────────────────

struct MosquittoFixture {
    pub host: String,
    pub port: u16,
    pub child: Child,
    _stdout: Arc<Mutex<String>>,
    _stderr: Arc<Mutex<String>>,
    _config: File,
    _exclusive: MutexGuard<'static, ()>,
}

static MOSQUITTO_FIXTURE_LOCK: Mutex<()> = Mutex::new(());
const MOSQUITTO_FIXTURE_PORT: u16 = 41_883;

impl MosquittoFixture {
    fn start() -> Result<Self, String> {
        let exclusive = MOSQUITTO_FIXTURE_LOCK
            .lock()
            .map_err(|_| "Mosquitto fixture exclusivity lock was poisoned".to_string())?;
        let mosquitto_bin = required_tool(
            "VIGIL_MOSQUITTO_BIN",
            "mosquitto",
            &[Path::new("/usr/sbin/mosquitto")],
        )?;
        let host = process_unique_loopback()?.to_string();
        let port = MOSQUITTO_FIXTURE_PORT;
        let config = mosquitto_memfd_config(&host, port)?;
        let config_path = format!("/proc/self/fd/{}", config.as_raw_fd());
        let mut child = Command::new(&mosquitto_bin)
            // A memfd is a real file for Mosquitto but never traverses an
            // AppArmor-sensitive temporary path. With CLOEXEC deliberately
            // absent, the broker inherits this exact immutable config file.
            .arg("-c")
            .arg(config_path)
            .arg("-v")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn mosquitto: {e}"))?;
        let stdout = capture_pipe(child.stdout.take());
        let stderr = capture_pipe(child.stderr.take());
        let started = wait_until("Mosquitto TCP listener", Duration::from_secs(5), || {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| format!("inspect Mosquitto: {e}"))?
            {
                return Err(format!("Mosquitto exited early with {status}"));
            }
            Ok(TcpStream::connect((host.as_str(), port)).ok().map(|_| ()))
        });
        let ready = started
            .and_then(|()| MqttProbe::connect(&host, port, Duration::from_secs(2)).map(|_| ()));
        if ready.is_ok() && child.try_wait().map_err(|e| e.to_string())?.is_none() {
            return Ok(Self {
                host,
                port,
                child,
                _stdout: stdout,
                _stderr: stderr,
                _config: config,
                _exclusive: exclusive,
            });
        }
        let _ = child.kill();
        let status = child.wait().ok();
        let _ = wait_until(
            "Mosquitto stdout/stderr capture to drain",
            Duration::from_millis(250),
            || {
                let has_output = stdout.lock().is_ok_and(|value| !value.is_empty())
                    || stderr.lock().is_ok_and(|value| !value.is_empty());
                Ok(has_output.then_some(()))
            },
        );
        let out = stdout.lock().map(|v| v.clone()).unwrap_or_default();
        let err = stderr.lock().map(|v| v.clone()).unwrap_or_default();
        let detail = ready
            .err()
            .unwrap_or_else(|| format!("exited after readiness with {status:?}"));
        Err(format!(
            "single causally exclusive Mosquitto startup failed at {host}:{port}: {detail}; stdout={out:?}; stderr={err:?}"
        ))
    }

    fn mqtt_config(&self) -> MqttConfig {
        MqttConfig {
            broker_host: self.host.clone(),
            broker_port: self.port,
            username: None,
            password: None,
        }
    }
}

fn process_unique_loopback() -> Result<Ipv4Addr, String> {
    let pid = std::process::id();
    let host_id = pid
        .checked_add(0x40_0000)
        .filter(|value| *value <= 0xff_ffff)
        .ok_or_else(|| format!("process id {pid} cannot map injectively into 127/8"))?;
    Ok(Ipv4Addr::new(
        127,
        ((host_id >> 16) & 0xff) as u8,
        ((host_id >> 8) & 0xff) as u8,
        (host_id & 0xff) as u8,
    ))
}

fn mosquitto_memfd_config(host: &str, port: u16) -> Result<File, String> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: the C string points to immutable storage for the duration of
        // the call. Flags 0 intentionally leaves CLOEXEC off so Mosquitto can
        // open /proc/self/fd/<fd> after exec.
        let fd = unsafe { libc::memfd_create(c"vigil-mosquitto-config".as_ptr(), 0) };
        if fd < 0 {
            return Err(format!(
                "create Mosquitto memfd config: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: memfd_create returned a new owned descriptor on success.
        let mut file = unsafe { File::from_raw_fd(fd) };
        write!(
            file,
            "listener {port} {host}\nallow_anonymous true\npersistence false\n"
        )
        .map_err(|error| format!("write Mosquitto memfd config: {error}"))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| format!("rewind Mosquitto memfd config: {error}"))?;
        Ok(file)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (host, port);
        Err("real-broker Mosquitto fixture requires Linux memfd_create".to_string())
    }
}

impl Drop for MosquittoFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn open_store_at(path: &Path) -> Result<Store, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create store parent {}: {e}", parent.display()))?;
    }
    Store::open(StoreConfig {
        db_path: path.to_path_buf(),
        default_text_embedder: Some(EmbedderConfig::disabled()),
        ..StoreConfig::default()
    })
    .map_err(|e| format!("open store {}: {e}", path.display()))
}

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

fn capture_pipe<T: Read + Send + 'static>(pipe: Option<T>) -> Arc<Mutex<String>> {
    let buf = Arc::new(Mutex::new(String::new()));
    if let Some(mut pipe) = pipe {
        let captured = Arc::clone(&buf);
        thread::spawn(move || {
            let mut tmp = [0u8; 4096];
            loop {
                match pipe.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut g) = captured.lock() {
                            g.push_str(&String::from_utf8_lossy(&tmp[..n]));
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    buf
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
    // Bounded channel with capacity 1; the subscriber fills it and increments overflow.
    // The _rx is held but never drained so the channel stays full after the first message.
    let (tx, _rx) = mpsc::sync_channel::<CorrectionRequest>(1);

    let tmp = tempfile::tempdir().unwrap();
    let store_path = tmp.path().join("data").join("store.contextgraph");
    let store = Arc::new(open_store_at(&store_path).expect("store"));

    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        store,
        std::collections::BTreeMap::new(),
        tx,
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

    let tmp = tempfile::tempdir().unwrap();
    let store_path = tmp.path().join("data").join("store.contextgraph");
    let store = Arc::new(open_store_at(&store_path).expect("store"));

    let lower_gate_enabled = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let driveway_enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut camera_flags = std::collections::BTreeMap::new();
    camera_flags.insert("lower-gate".to_string(), Arc::clone(&lower_gate_enabled));
    camera_flags.insert("driveway".to_string(), Arc::clone(&driveway_enabled));

    let state_topic = "vigil/test/lower-gate/enabled".to_string();
    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[&state_topic],
        Duration::from_secs(2),
    )
    .expect("control probe must receive SUBACK");

    let overflow_count = Arc::new(AtomicUsize::new(0));
    let (tx, _rx) = mpsc::sync_channel::<CorrectionRequest>(8);
    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        store,
        camera_flags,
        tx,
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
    assert!(
        lower_gate_enabled.load(std::sync::atomic::Ordering::SeqCst),
        "enable command must restore the live lower-gate enabled flag"
    );
    assert!(
        !driveway_enabled.load(std::sync::atomic::Ordering::SeqCst),
        "lower-gate commands must not change the driveway enabled flag"
    );
}

/// RED — wrong stub `record_correction` always returns Ok;
/// a disk-constrained write against a REAL detection must return
/// Err(CorrectionError::WriteFailed).
///
/// Use a real cg detection record so a real ObservationId
/// exists, then constrain the data dir (read-only), then assert Err(WriteFailed) specifically
/// — the empty-store+fake-id path only tests NoAnchor, never the disk-full path.
#[cfg(feature = "first-light-acceptance")]
#[test]
fn correction_and_event_under_disk_full_fail_loudly() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for correction_and_event_under_disk_full_fail_loudly");

    let detection_id = {
        let s =
            ha_test_support::open_store_at(&store_path).expect("open store for detection_id read");
        let obs = s.list_observations(None).expect("list_observations");
        assert!(
            !obs.is_empty(),
            "at least one cg detection must be in the store for disk-full path"
        );
        obs[0].id.to_string()
    };

    // Open the store and cap the database at its current size so the next INSERT fails.
    // contextdb's DiskBudgetExceeded error propagates up through record_observation →
    // record_correction as WriteFailed — this is the real cg write path, not a probe.
    let store =
        ha_test_support::open_store_at(&store_path).expect("open store for disk-constrained write");

    let db = store.sync_database();
    let current_size = db
        .disk_file_size()
        .expect("store must be file-backed for disk-limit test");
    db.set_disk_limit(Some(current_size))
        .expect("set disk limit to current file size");

    let req = CorrectionRequest {
        detection_id: detection_id.clone(),
        label: None,
        correction_type: CorrectionType::FalseAlarm,
    };
    let result = record_correction(&store, req);

    // Must return Err(WriteFailed) specifically — a real id exists so NoAnchor is wrong.
    // Wrong stub always returns Ok(receipt) → assertion FAILS.
    assert!(
        matches!(result, Err(CorrectionError::WriteFailed(_))),
        "record_correction must return Err(WriteFailed) when the cg write fails against \
         a disk-limit-capped store with a real detection id; wrong stub always returns Ok — \
         got: {result:?}"
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

/// RED — wrong stub `publish_detection_event` is a no-op;
/// the detection event built from REAL cg data never arrives on the broker topic.
///
/// Resolve one deterministic cg detection through `review_why`, map that exact
/// authority record with the production event mapper, publish it through a real
/// broker, and compare the received id, class, and confidence back to the same
/// `review_why` record. Separate fixtures cannot satisfy the causal join.
#[cfg(feature = "first-light-acceptance")]
#[test]
fn detection_publishes_event_to_real_broker() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for detection_publishes_event_to_real_broker");
    let broker = MosquittoFixture::start().expect("mosquitto must start for acceptance test");

    // Read detection data from cg authority.
    let store = ha_test_support::open_store_at(&store_path).expect("open store for detection read");
    let observations = store.list_observations(None).expect("list_observations");
    assert!(
        !observations.is_empty(),
        "at least one detection must be in the store"
    );
    let detection_obs = &observations[0];
    let real_detection_id = detection_obs.id.to_string();
    let why_before = review_why(&store, &real_detection_id)
        .expect("the authoritative cg detection must resolve through review_why");
    let event_payload = map_detection_to_event_payload(&DetectionInput {
        observation_id: why_before.observation_id.clone(),
        camera_name: why_before.camera_name.clone(),
        object_class: why_before.class_name.clone(),
        confidence: why_before.confidence,
        timestamp_ms: detection_obs.observed_at.timestamp_millis(),
        evidence_ref: why_before.clip_ref.clone(),
        snapshot_ref: why_before.detector_image_ref.clone(),
        entity_name: why_before
            .recognition
            .as_ref()
            .map(|recognition| recognition.name.clone()),
        match_score: why_before
            .recognition
            .as_ref()
            .map(|recognition| recognition.score),
    });
    let event_json =
        serde_json::to_string(&event_payload).expect("the production event payload must serialize");
    let topic = format!("vigil/test/{}/detection", why_before.camera_id);

    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[&topic],
        Duration::from_secs(2),
    )
    .expect("detection probe must receive SUBACK");

    let result = publish_detection_event(&broker.mqtt_config(), &event_json, &topic);
    assert!(
        result.is_ok(),
        "publish_detection_event returned Err: {:?}",
        result.err()
    );

    let received = probe
        .recv_matching(
            "published detection event",
            Duration::from_secs(2),
            |message| message.topic == topic && message.payload == event_json.as_bytes(),
        )
        .expect("publish_detection_event must deliver the value-equal event");
    let received: serde_json::Value =
        serde_json::from_slice(&received.payload).expect("detection event must be JSON");
    let why_after = review_why(&store, &real_detection_id)
        .expect("the broker event detection id must still resolve through review_why");
    assert_eq!(
        received
            .get("detection_id")
            .and_then(serde_json::Value::as_str),
        Some(why_after.observation_id.as_str()),
        "broker detection_id must identify the exact cg /why record"
    );
    assert_eq!(
        received
            .get("object_class")
            .and_then(serde_json::Value::as_str),
        Some(why_after.class_name.as_str()),
        "broker object_class must value-equal the same cg /why record"
    );
    let broker_confidence = received
        .get("confidence")
        .and_then(serde_json::Value::as_f64)
        .expect("broker event must carry numeric confidence");
    assert!(
        (broker_confidence - why_after.confidence).abs() < f64::EPSILON,
        "broker confidence {broker_confidence} must value-equal the same cg /why confidence {}",
        why_after.confidence
    );
}

/// RED — wrong stub the wired subscriber never connects to the broker;
/// the correction command is published but never internally delivered to `record_correction`,
/// so nothing lands in cg.
#[cfg(feature = "first-light-acceptance")]
#[test]
fn correction_command_on_broker_lands_in_cg() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for correction_command_on_broker_lands_in_cg");
    let broker = MosquittoFixture::start().expect("mosquitto must start");

    let store = Arc::new(ha_test_support::open_store_at(&store_path).expect("open store"));
    let obs = store.list_observations(None).expect("list_observations");
    assert!(!obs.is_empty(), "at least one detection required");
    let detection_id = obs[0].id.to_string();

    let overflow_count = Arc::new(AtomicUsize::new(0));
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<CorrectionRequest>(64);
    let (committed_tx, committed_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    // Worker drains the correction channel and writes to cg so review_why can verify.
    let worker = {
        let store_w = Arc::clone(&store);
        thread::spawn(move || {
            let result = cmd_rx
                .recv()
                .map_err(|error| format!("correction worker channel closed: {error}"))
                .and_then(|request| {
                    record_correction(&store_w, request)
                        .map(|_| ())
                        .map_err(|error| format!("record correction: {error}"))
                });
            let _ = committed_tx.send(result);
        })
    };
    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &["vigil/test/availability"],
        Duration::from_secs(2),
    )
    .expect("correction probe must receive SUBACK");
    // Production subscriber forwards correct commands to the channel.
    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        Arc::clone(&store),
        std::collections::BTreeMap::new(),
        cmd_tx,
        Arc::clone(&overflow_count),
    );

    probe
        .recv_matching(
            "production subscriber online",
            Duration::from_secs(3),
            |message| message.topic == "vigil/test/availability" && message.payload == b"online",
        )
        .expect("production subscriber must acknowledge readiness");
    let cmd_payload = format!(
        r#"{{"detection_id":"{detection_id}","correction_type":"identity","label":"that's Arjun"}}"#
    );
    probe
        .publish_qos1("vigil/commands/correct", &cmd_payload)
        .expect("publish correction command");
    probe
        .wait_for_pubacks(1, Duration::from_secs(2))
        .expect("broker must acknowledge correction command");
    committed_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("correction worker must acknowledge commit")
        .expect("correction commit must succeed");
    handle.shutdown_and_join();
    worker.join().expect("correction worker must join");

    // Wrong stub: subscriber never connects → record_correction never called → corrections empty.
    let why = review_why(&store, &detection_id).expect("review_why must not error");
    assert_eq!(
        why.corrections.len(),
        1,
        "subscriber must internally deliver the correction command to record_correction in cg; \
         wrong stub never connects to the broker — corrections is empty (count = {})",
        why.corrections.len()
    );

    // label + correction_type value-equality via review_why.
    let correction = &why.corrections[0];
    assert_eq!(
        correction.label.as_deref(),
        Some("that's Arjun"),
        "correction label must value-equal the broker command label 'that\\'s Arjun'; \
         wrong stub writes nothing so label is absent"
    );
    assert_eq!(
        correction.correction_type,
        CorrectionType::Identity,
        "correction_type must be Identity from the broker command; \
         wrong stub writes nothing so correction_type is absent"
    );
}

/// RED — wrong stub the wired subscriber never connects; redelivered
/// commands can't be idempotent if nothing arrives, so nothing lands in cg.
#[cfg(feature = "first-light-acceptance")]
#[test]
fn redelivered_correction_command_is_idempotent() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for redelivered_correction_command_is_idempotent");
    let broker = MosquittoFixture::start().expect("mosquitto");

    let store = Arc::new(ha_test_support::open_store_at(&store_path).expect("open store"));
    let obs = store.list_observations(None).expect("list");
    assert!(!obs.is_empty(), "need at least one detection");
    let detection_id = obs[0].id.to_string();

    let overflow_count = Arc::new(AtomicUsize::new(0));
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<CorrectionRequest>(64);
    let (recorded_tx, recorded_rx) = mpsc::sync_channel(2);
    let worker = {
        let store_w = Arc::clone(&store);
        thread::spawn(move || {
            while let Ok(req) = cmd_rx.recv() {
                let result = record_correction(&store_w, req)
                    .map(|_| ())
                    .map_err(|error| format!("record correction: {error}"));
                let _ = recorded_tx.send(result);
            }
        })
    };
    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        Arc::clone(&store),
        std::collections::BTreeMap::new(),
        cmd_tx,
        Arc::clone(&overflow_count),
    );

    let mut probe = subscriber_ready_probe(&broker);
    // Deliver the same command twice (broker redelivery simulation).
    let payload = correction_command_payload(&detection_id);
    probe
        .publish_qos1("vigil/commands/correct", &payload)
        .expect("publish first delivery");
    probe
        .publish_qos1("vigil/commands/correct", &payload)
        .expect("publish redelivery");
    probe
        .wait_for_pubacks(2, Duration::from_secs(2))
        .expect("broker must acknowledge both deliveries");
    for _ in 0..2 {
        recorded_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("worker must process delivery")
            .expect("record delivery");
    }
    handle.shutdown_and_join();
    worker.join().expect("correction worker must join");

    // Wrong stub: subscriber never connects → nothing in cg → corrections.len() == 0, not 1.
    let why = review_why(&store, &detection_id).expect("review_why");
    assert_eq!(
        why.corrections.len(),
        1,
        "redelivered correction commands must result in exactly one correction in cg \
         (idempotent); wrong stub never connects so nothing was written (count = {})",
        why.corrections.len()
    );
}

/// RED — wrong stub the wired subscriber never connects; two distinct
/// corrections on the same detection both require delivery and recording in cg.
#[cfg(feature = "first-light-acceptance")]
#[test]
fn two_distinct_corrections_on_same_detection_both_land() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for two_distinct_corrections_on_same_detection_both_land");
    let broker = MosquittoFixture::start().expect("mosquitto");

    let store = Arc::new(ha_test_support::open_store_at(&store_path).expect("open store"));
    let obs = store.list_observations(None).expect("list");
    assert!(!obs.is_empty(), "need at least one detection");
    let detection_id = obs[0].id.to_string();

    let overflow_count = Arc::new(AtomicUsize::new(0));
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<CorrectionRequest>(64);
    let (recorded_tx, recorded_rx) = mpsc::sync_channel(2);
    let worker = {
        let store_w = Arc::clone(&store);
        thread::spawn(move || {
            while let Ok(req) = cmd_rx.recv() {
                let result = record_correction(&store_w, req)
                    .map(|_| ())
                    .map_err(|error| format!("record correction: {error}"));
                let _ = recorded_tx.send(result);
            }
        })
    };
    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        Arc::clone(&store),
        std::collections::BTreeMap::new(),
        cmd_tx,
        Arc::clone(&overflow_count),
    );

    let mut probe = subscriber_ready_probe(&broker);

    let identity_payload = format!(
        r#"{{"detection_id":"{detection_id}","correction_type":"identity","label":"that's Arjun"}}"#
    );
    let wrong_class_payload = format!(
        r#"{{"detection_id":"{detection_id}","correction_type":"wrong_class","label":"actually the neighbour"}}"#
    );
    probe
        .publish_qos1("vigil/commands/correct", &identity_payload)
        .expect("publish identity correction");
    probe
        .publish_qos1("vigil/commands/correct", &wrong_class_payload)
        .expect("publish wrong-class correction");
    probe
        .wait_for_pubacks(2, Duration::from_secs(2))
        .expect("broker must acknowledge both corrections");
    for _ in 0..2 {
        recorded_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("worker must process correction")
            .expect("record correction");
    }
    handle.shutdown_and_join();
    worker.join().expect("correction worker must join");

    // Wrong stub: subscriber never connects → nothing in cg → corrections.len() == 0, not 2.
    let why = review_why(&store, &detection_id).expect("review_why");
    assert_eq!(
        why.corrections.len(),
        2,
        "both distinct corrections (identity + wrong_class) must land in cg; \
         wrong stub never connects so nothing was written (count = {})",
        why.corrections.len()
    );

    // label value-equality for both corrections.
    let labels: Vec<Option<&str>> = why.corrections.iter().map(|c| c.label.as_deref()).collect();
    assert!(
        labels.contains(&Some("that's Arjun")),
        "identity correction label 'that\\'s Arjun' must be present; \
         wrong stub writes nothing so no labels exist — got {labels:?}"
    );
    assert!(
        labels.contains(&Some("actually the neighbour")),
        "wrong-class correction label 'actually the neighbour' must be present; \
         wrong stub writes nothing so no labels exist — got {labels:?}"
    );
}

/// RED — wrong stub subscriber never delivers; malformed commands are never
/// rejected because they never arrive; subsequent well-formed commands also
/// never arrive.
#[cfg(feature = "first-light-acceptance")]
#[test]
fn malformed_correction_command_is_rejected_and_subscriber_survives() {
    let broker = MosquittoFixture::start().expect("mosquitto");

    let overflow_count = Arc::new(AtomicUsize::new(0));
    // Capacity large enough so no overflow occurs; test only checks subscriber survival.
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<CorrectionRequest>(100);

    let tmp = tempfile::tempdir().unwrap();
    let store_path = tmp.path().join("data").join("store.contextgraph");
    let store = Arc::new(open_store_at(&store_path).expect("store"));

    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        store,
        std::collections::BTreeMap::new(),
        cmd_tx,
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
    let received = cmd_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("valid command must be delivered");
    handle.shutdown_and_join();

    assert_eq!(
        received.detection_id, "aabbccdd-0000-1111-2222-333344445555",
        "subscriber must discard malformed input and deliver the following valid command"
    );
    assert!(
        cmd_rx.try_recv().is_err(),
        "malformed command must not enter the correction channel"
    );
}

/// RED — wrong stub `record_correction` writes nothing; after broker drops the
/// correction must be durable in cg (cg is the authority).
///
/// Durability assertion reads via `list_observations` on a fresh store handle
/// (not `review_why`) so the no-shadow requirement is satisfied — an in-process mirror
/// could answer review_why, but only a fresh-opened store proves cg durability.
#[cfg(feature = "first-light-acceptance")]
#[test]
fn broker_drop_does_not_affect_durable_cg_record() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for broker_drop_does_not_affect_durable_cg_record");
    let mut broker = MosquittoFixture::start().expect("mosquitto");

    let store = ha_test_support::open_store_at(&store_path).expect("open store");
    let obs = store.list_observations(None).expect("list");
    assert!(!obs.is_empty(), "need at least one detection");
    let detection_id = obs[0].id.to_string();

    let req = CorrectionRequest {
        detection_id: detection_id.clone(),
        label: None,
        correction_type: CorrectionType::FalseAlarm,
    };
    let _ = record_correction(&store, req);

    // Kill the broker — correction must persist in cg regardless of broker state.
    let _ = broker.child.kill();
    let _ = broker.child.wait();

    // drop all in-process state; reopen ONLY the cg Store.
    drop(store);
    let store2 = ha_test_support::open_store_at(&store_path).expect("reopen store");
    let all_obs = store2
        .list_observations(None)
        .expect("list_observations after broker drop");

    // Find the correction observation anchored to this detection.
    // Wrong stub: record_correction writes nothing → anchored_detection_id absent → FAIL.
    let correction_in_cg = all_obs.iter().any(|o| {
        o.observed_properties
            .get("anchored_detection_id")
            .or_else(|| o.properties.get("anchored_detection_id"))
            .and_then(|v| v.as_str())
            == Some(detection_id.as_str())
    });
    assert!(
        correction_in_cg,
        "correction must persist in cg after broker drop (cg is the authority); \
         wrong stub wrote nothing — anchored_detection_id '{detection_id}' is absent \
         from list_observations on fresh store handle"
    );
}

/// RED — three sub-checks for operator actions delivered through the production subscriber.
/// All three fail via wrong stub (never connects → nothing routed).
///
/// (a) disable-camera: the subscriber receives the command and writes a disable marker at
///     {data_dir}/camera-disabled/{camera_id}; wrong stub → never connects → no marker
///     written → assert FAILS.
/// (b) ack (Identity correction) via subscriber → cg; wrong stub → nothing in cg →
///     review_why shows no corrections → assert_eq FAILS.
/// (c) snapshot: the subscriber publishes the latest detector evidence PNG to
///     vigil/{camera_id}/snapshot; wrong stub → never connects → no bytes published →
///     assert FAILS (non-empty bytes on snapshot topic required).
#[cfg(feature = "first-light-acceptance")]
#[test]
fn operator_action_command_effects_action() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for operator_action_command_effects_action");
    let broker = MosquittoFixture::start().expect("mosquitto");

    let store_arc = Arc::new(ha_test_support::open_store_at(&store_path).expect("open store"));
    let obs = store_arc.list_observations(None).expect("list");
    assert!(!obs.is_empty(), "need at least one detection");
    let detection_id = obs[0].id.to_string();

    let overflow_count = Arc::new(AtomicUsize::new(0));
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<CorrectionRequest>(64);
    let (recorded_tx, recorded_rx) = mpsc::sync_channel(1);
    let worker = {
        let store_w = Arc::clone(&store_arc);
        thread::spawn(move || {
            while let Ok(req) = cmd_rx.recv() {
                let result = record_correction(&store_w, req)
                    .map(|_| ())
                    .map_err(|error| format!("record correction: {error}"));
                let _ = recorded_tx.send(result);
            }
        })
    };
    // Two live per-camera enabled flags. The production control handler flips
    // these on disable/enable; disabling A must flip ONLY A's flag — the
    // differential "camera A stops while B keeps detecting" contract.
    let flag_a = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag_b = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut camera_flags = std::collections::BTreeMap::new();
    camera_flags.insert("lower-gate".to_string(), Arc::clone(&flag_a));
    camera_flags.insert("driveway".to_string(), Arc::clone(&flag_b));
    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        Arc::clone(&store_arc),
        camera_flags,
        cmd_tx,
        Arc::clone(&overflow_count),
    );
    let mut probe = MqttProbe::connect_and_subscribe_with_max_packet(
        &broker.host,
        broker.port,
        &["vigil/test/availability", "vigil/lower-gate/snapshot"],
        Duration::from_secs(2),
        Some(5 * 1024 * 1024),
    )
    .expect("operator probe must receive SUBACKs");
    probe
        .recv_matching(
            "production subscriber online",
            Duration::from_secs(3),
            |message| message.topic == "vigil/test/availability" && message.payload == b"online",
        )
        .expect("production subscriber must acknowledge readiness");

    // ── Sub-check (a): disable-camera routes to the named camera ─────────────
    // The correct subscriber connects to the control topic and routes the disable
    // command to camera 'lower-gate', writing a disable marker file at
    // {data_dir}/camera-disabled/lower-gate.  Wrong stub: never connects → no marker
    // written → assert FAILS.
    let data_dir = store_path
        .parent()
        .expect("store_path must have a parent data directory");
    let disable_marker = data_dir.join("camera-disabled").join("lower-gate");
    probe
        .publish_qos1(
            "vigil/commands/control",
            r#"{"camera_id":"lower-gate","action":"disable"}"#,
        )
        .expect("publish disable command");
    probe
        .wait_for_pubacks(1, Duration::from_secs(2))
        .expect("broker must acknowledge disable command");
    wait_until("lower-gate disable marker", Duration::from_secs(5), || {
        Ok(disable_marker.exists().then_some(()))
    })
    .expect("disable command must create its causal marker");
    // The load-bearing differential effect: the production handler flipped ONLY
    // camera A's live enabled flag. A regression that writes the marker but skips
    // the flag flip (camera A would keep detecting) fails here.
    assert!(
        !flag_a.load(std::sync::atomic::Ordering::SeqCst),
        "disabling 'lower-gate' must flip its live enabled flag to false (camera A stops detecting)"
    );
    assert!(
        flag_b.load(std::sync::atomic::Ordering::SeqCst),
        "disabling 'lower-gate' must NOT touch 'driveway' (camera B keeps detecting)"
    );

    // ── Sub-check (b): ack (Identity) via subscriber → cg ─────────────────────
    // Wrong stub: subscriber never calls record_correction → corrections empty → FAILS.
    let ack_payload = format!(
        r#"{{"detection_id":"{detection_id}","correction_type":"identity","label":"confirmed: Arjun"}}"#
    );
    probe
        .publish_qos1("vigil/commands/correct", &ack_payload)
        .expect("publish operator acknowledgement");
    probe
        .wait_for_pubacks(1, Duration::from_secs(2))
        .expect("broker must acknowledge operator acknowledgement");
    recorded_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("operator acknowledgement must be processed")
        .expect("operator acknowledgement must commit");

    // ── Sub-check (c): snapshot → referenced evidence PNG published to image topic ──
    // The subscriber must read the latest referenced evidence PNG from disk and publish it
    // to "vigil/{camera_id}/snapshot" so the HA image entity shows the detection frame.
    // camera_id for "lower-gate" matches the seeded store's camera slug.
    // Wrong stub: subscriber never connects → nothing published → assert FAILS.
    //
    probe
        .publish_qos1(
            "vigil/commands/control",
            r#"{"camera_id":"lower-gate","action":"snapshot"}"#,
        )
        .expect("publish snapshot command");
    let snapshot = probe
        .recv_matching(
            "non-empty detector snapshot",
            Duration::from_secs(5),
            |message| message.topic == "vigil/lower-gate/snapshot" && !message.payload.is_empty(),
        )
        .expect("snapshot command must publish evidence bytes");

    handle.shutdown_and_join();
    worker.join().expect("operator correction worker must join");

    let why = review_why(&store_arc, &detection_id).expect("review_why");
    assert_eq!(
        why.corrections.len(),
        1,
        "operator ack (Identity correction with label) must land in cg via subscriber; \
         wrong stub never connects (corrections = {})",
        why.corrections.len()
    );

    assert!(
        !snapshot.payload.is_empty(),
        "snapshot command must publish referenced evidence PNG bytes to \
         vigil/lower-gate/snapshot; wrong stub never connects so nothing was received"
    );
}

/// RED — `review_events` must return only detection observations; corrections and
/// other cg-internal observation types must not surface.  `review_why("--latest")`
/// must pick the newest detection even when a more-recent correction exists.
///
/// Wrong stub: no filter applied → corrections appear in `review_events` rows; and
/// `--latest` might pick the correction instead of the detection.
#[cfg(feature = "first-light-acceptance")]
#[test]
fn detection_only_events_excludes_corrections() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for detection_only_events_excludes_corrections");
    let store = ha_test_support::open_store_at(&store_path).expect("open store");

    // The fixture contains the exact detection count requested by the test.
    // Use the NEWEST detection as the anchor so we can verify that after writing a correction
    // (which gets an even newer id), `--latest` still picks the newest DETECTION, not the correction.
    let obs = store.list_observations(None).expect("list_observations");
    let detection = obs
        .iter()
        .filter(|o| o.observation_type == "detection")
        .max_by_key(|o| o.observed_at)
        .expect("at least one detection required in seeded store");
    let detection_id = detection.id.to_string();

    // Write a correction — its v7 UUID timestamp is newer than any existing detection.
    // It must NOT appear in review_events or be selected by --latest.
    let req = CorrectionRequest {
        detection_id: detection_id.clone(),
        label: Some("acknowledged".to_string()),
        correction_type: CorrectionType::Identity,
    };
    record_correction(&store, req).expect("record_correction must not error");

    // review_events must return ONLY detection rows — the correction must be absent.
    let events = review_events(&store, 100).expect("review_events must not error");
    for row in &events.rows {
        // Find the raw observation to verify its type.
        let raw = store
            .list_observations(None)
            .unwrap_or_default()
            .into_iter()
            .find(|o| o.id.to_string() == row.observation_id);
        if let Some(raw_obs) = raw {
            assert_eq!(
                raw_obs.observation_type, "detection",
                "review_events returned a row for observation {} which has type '{}', not 'detection'; \
                 only detection observations must surface on the events display path",
                row.observation_id, raw_obs.observation_type
            );
        }
    }

    // Verify the detection IS present (sanity: filter must not over-strip).
    let detection_present = events.rows.iter().any(|r| r.observation_id == detection_id);
    assert!(
        detection_present,
        "review_events must include the detection row for {detection_id}; \
         over-filtering dropped it"
    );

    // `vigil why --latest` (handle_why_read with "--latest") must select the detection,
    // not the newer correction.  Wrong stub: no filter → picks the newer correction →
    // provenance walk fails (no decision/intention).
    let why = review_why(&store, "--latest")
        .expect("review_why('--latest') must resolve to the detection, not the newer correction");
    assert_eq!(
        why.observation_id, detection_id,
        "review_why('--latest') must select the newest DETECTION ({}), not the newer \
         correction; wrong stub picks the correction and the provenance walk fails",
        detection_id
    );
}

/// RED — `record_correction` must return `Err(NoAnchor)` when the anchor observation
/// exists in cg but has `observation_type != "detection"`.  Zero new observations must
/// be written (the type guard fires before any write).
///
/// Wrong stub: no type check → returns Ok even for a non-detection anchor → assertions FAIL.
#[cfg(feature = "first-light-acceptance")]
#[test]
fn correction_rejected_for_non_detection_anchor() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for correction_rejected_for_non_detection_anchor");
    let store = ha_test_support::open_store_at(&store_path).expect("open store");

    // Obtain the detection id and then write a correction anchored to it.
    let obs = store.list_observations(None).expect("list_observations");
    let detection = obs
        .iter()
        .find(|o| o.observation_type == "detection")
        .expect("at least one detection required");
    let detection_id = detection.id.to_string();

    let first_correction_req = CorrectionRequest {
        detection_id: detection_id.clone(),
        label: None,
        correction_type: CorrectionType::FalseAlarm,
    };
    let receipt = record_correction(&store, first_correction_req)
        .expect("initial correction on detection must succeed");

    // The correction observation now exists in cg with observation_type = "correction".
    // Attempt to anchor a second correction to the CORRECTION observation (not the detection).
    let obs_after = store
        .list_observations(None)
        .expect("list after first correction");
    let count_before_second = obs_after.len();

    let bad_req = CorrectionRequest {
        detection_id: receipt.correction_id.clone(), // ← anchoring to the correction, not the detection
        label: Some("bad anchor".to_string()),
        correction_type: CorrectionType::Identity,
    };
    let result = record_correction(&store, bad_req);

    // Must return Err(NoAnchor) — the anchor is valid (exists, well-formed UUID) but
    // has the wrong type.  Wrong stub: no type check → Ok(receipt) → FAILS.
    assert!(
        matches!(result, Err(CorrectionError::NoAnchor(_))),
        "record_correction anchored to a correction observation (type='correction') must \
         return Err(NoAnchor); wrong stub has no type check and returns Ok — got: {result:?}"
    );

    // Zero new observations must have been written (the guard fires before the write).
    let obs_final = store
        .list_observations(None)
        .expect("list after rejected correction");
    assert_eq!(
        obs_final.len(),
        count_before_second,
        "rejected non-detection anchor must write zero new observations; \
         wrong stub writes a spurious correction before checking the type (count delta = {})",
        obs_final.len() as i64 - count_before_second as i64
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

    let tmp = tempfile::tempdir().unwrap();
    let store_path = tmp.path().join("data").join("store.contextgraph");
    let store = Arc::new(open_store_at(&store_path).expect("store"));

    let overflow = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (cmd_tx, _cmd_rx) = mpsc::sync_channel::<CorrectionRequest>(64);
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
    let handle = spawn_production_subscriber(
        cfg,
        store,
        std::collections::BTreeMap::new(),
        cmd_tx,
        overflow,
    );

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

/// RED — wrong stub: parse_command_topic returns None for correction payloads
/// so record_correction is never called and nothing lands in cg.
/// Drive the production path end-to-end: MQTT correct command →
/// parse_command_topic → record_correction → cg read-back.
/// Covers both false_alarm (no label) and wrong_class + label="cat".
#[cfg(feature = "first-light-acceptance")]
#[test]
fn typed_correction_via_mqtt_lands_in_cg() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(2)
        .expect("seeded store for typed_correction_via_mqtt_lands_in_cg");
    let store = Arc::new(ha_test_support::open_store_at(&store_path).expect("open store"));

    let obs = store.list_observations(None).expect("list observations");
    let mut detections: Vec<_> = obs
        .iter()
        .filter(|o| o.observation_type == "detection")
        .collect();
    assert!(
        detections.len() >= 2,
        "need at least 2 detections in the seeded store; got {}",
        detections.len()
    );
    detections.sort_by_key(|o| o.id.to_string());
    let detection_a = detections[0].id.to_string();
    let detection_b = detections[1].id.to_string();

    let broker = MosquittoFixture::start().expect("Mosquitto must start for semantics test");

    let overflow = Arc::new(AtomicUsize::new(0));
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<CorrectionRequest>(64);
    let (recorded_tx, recorded_rx) = mpsc::sync_channel(2);
    let worker = {
        let store_w = Arc::clone(&store);
        thread::spawn(move || {
            while let Ok(req) = cmd_rx.recv() {
                let result = record_correction(&store_w, req)
                    .map(|_| ())
                    .map_err(|error| format!("record correction: {error}"));
                let _ = recorded_tx.send(result);
            }
        })
    };
    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        Arc::clone(&store),
        std::collections::BTreeMap::new(),
        cmd_tx,
        Arc::clone(&overflow),
    );

    let mut probe = subscriber_ready_probe(&broker);

    // false_alarm on detection A (no label).
    let fa_payload =
        format!(r#"{{"detection_id":"{detection_a}","correction_type":"false_alarm"}}"#);
    probe
        .publish_qos1("vigil/commands/correct", &fa_payload)
        .expect("publish false-alarm correction");

    // wrong_class + label="cat" on detection B.
    let wc_payload = format!(
        r#"{{"detection_id":"{detection_b}","correction_type":"wrong_class","label":"cat"}}"#
    );
    probe
        .publish_qos1("vigil/commands/correct", &wc_payload)
        .expect("publish wrong-class correction");
    probe
        .wait_for_pubacks(2, Duration::from_secs(2))
        .expect("broker must acknowledge typed corrections");

    for _ in 0..2 {
        recorded_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("typed correction must be processed")
            .expect("typed correction must commit");
    }
    handle.shutdown_and_join();
    worker.join().expect("typed correction worker must join");

    // Read back FalseAlarm on A via review_why.
    let why_a = review_why(&store, &detection_a)
        .expect("review_why(A) must not error after false_alarm correction");
    assert_eq!(
        why_a.corrections.len(),
        1,
        "false_alarm MQTT command must record a FalseAlarm correction in cg; \
         wrong stub parse_command_topic returns None → record_correction never called → 0 corrections"
    );
    assert_eq!(
        why_a.corrections[0].correction_type,
        CorrectionType::FalseAlarm,
        "correction type must be FalseAlarm; got {:?}",
        why_a.corrections[0].correction_type
    );

    // Read back WrongClass + label="cat" on B via review_why.
    let why_b = review_why(&store, &detection_b)
        .expect("review_why(B) must not error after wrong_class correction");
    assert_eq!(
        why_b.corrections.len(),
        1,
        "wrong_class MQTT command must record a WrongClass correction in cg; \
         wrong stub returns None → 0 corrections"
    );
    assert_eq!(
        why_b.corrections[0].correction_type,
        CorrectionType::WrongClass,
        "correction type must be WrongClass; got {:?}",
        why_b.corrections[0].correction_type
    );
    assert_eq!(
        why_b.corrections[0].label.as_deref(),
        Some("cat"),
        "wrong_class correction label must read back as 'cat'; \
         wrong stub drops the label — got {:?}",
        why_b.corrections[0].label
    );
}
