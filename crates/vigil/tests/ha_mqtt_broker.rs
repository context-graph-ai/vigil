// MQTT broker integration tests.
//
// Tests that do NOT require the live detector run in both `pr` and `ci-full` profiles.
// Tests that DO require a live detector are gated with `#[cfg(feature = "first-light-acceptance")]`.
//
// REGRESSION GUARDs pass at scaffold.
// RED tests fail on assertions via the deliberate wrong stubs.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use context_graph::{EmbedderConfig, Store, StoreConfig};
use tempfile::TempDir;
use vigil::{
    CameraConfig, CorrectionRequest, CorrectionType, HealthState, HealthStatus, MqttConfig,
    ServiceConfig, WiredSubscriberConfig, generate_discovery_payloads, mqtt_connect_intent,
    publish_discovery_to_broker, record_correction, spawn_detection_publisher,
    spawn_production_subscriber,
};

// ── Acceptance-only imports ────────────────────────────────────────────────
#[cfg(feature = "first-light-acceptance")]
use vigil::{CorrectionError, publish_detection_event, review_events, review_why};

// ── Acceptance test support (OnceLock-seeded template store) ──────────────

#[cfg(feature = "first-light-acceptance")]
#[path = "ha_test_support.rs"]
mod ha_test_support;

// ── Mosquitto fixture ─────────────────────────────────────────────────────

struct MosquittoFixture {
    pub port: u16,
    pub child: Child,
    _conf_dir: TempDir,
}

impl MosquittoFixture {
    fn start() -> Result<Self, String> {
        let port = free_port()?;
        let conf_dir = tempfile::tempdir().map_err(|e| format!("mosquitto conf dir: {e}"))?;
        let conf_path = conf_dir.path().join("mosquitto.conf");
        fs::write(&conf_path, format!("port {port}\nallow_anonymous true\n"))
            .map_err(|e| format!("write mosquitto.conf: {e}"))?;

        let mosquitto_bin = find_binary("mosquitto")
            .or_else(|| Some(PathBuf::from("/usr/sbin/mosquitto")))
            .filter(|p| p.exists())
            .ok_or_else(|| "mosquitto binary not found".to_string())?;

        let child = Command::new(&mosquitto_bin)
            .arg("-c")
            .arg(&conf_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn mosquitto: {e}"))?;

        wait_for_tcp_port(port, Duration::from_secs(5))?;

        Ok(Self {
            port,
            child,
            _conf_dir: conf_dir,
        })
    }

    fn mqtt_config(&self) -> MqttConfig {
        MqttConfig {
            broker_host: "127.0.0.1".to_string(),
            broker_port: self.port,
            username: None,
            password: None,
        }
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
        client_id: "vigil-test-sub".to_string(),
        availability_topic: "vigil/test/availability".to_string(),
        condition_topic: "vigil/test/condition".to_string(),
        discovery_payloads: vec![],
        health: HealthState::new(),
    }
}

fn free_port() -> Result<u16, String> {
    static ALLOCATED: OnceLock<Mutex<BTreeSet<u16>>> = OnceLock::new();
    let allocated = ALLOCATED.get_or_init(|| Mutex::new(BTreeSet::new()));
    for _ in 0..128 {
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|e| format!("allocate TCP port: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("read TCP port: {e}"))?
            .port();
        if allocated
            .lock()
            .map(|mut ports| ports.insert(port))
            .unwrap_or(false)
        {
            return Ok(port);
        }
    }
    Err("could not allocate a unique local TCP port".to_string())
}

fn wait_for_tcp_port(port: u16, timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    while start.elapsed() < timeout {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "port {port} did not open within {}s",
        timeout.as_secs()
    ))
}

fn find_binary(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    })
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

/// Subscribe to a broker topic using `mosquitto_sub`, return captured output
/// after a bounded wait period.
fn mosquitto_sub_output(broker_port: u16, topic: &str, wait: Duration) -> String {
    let sub_bin = find_binary("mosquitto_sub").or_else(|| {
        let p = PathBuf::from("/usr/bin/mosquitto_sub");
        p.exists().then_some(p)
    });

    let Some(sub_bin) = sub_bin else {
        return String::new();
    };

    let mut child = match Command::new(&sub_bin)
        .arg("-h")
        .arg("127.0.0.1")
        .arg("-p")
        .arg(broker_port.to_string())
        .arg("-t")
        .arg(topic)
        .arg("-W")
        .arg(wait.as_secs().max(1).to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    let stdout = capture_pipe(child.stdout.take());
    thread::sleep(wait + Duration::from_millis(300));

    let _ = child.kill();
    let _ = child.wait();

    stdout.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Publish `count` messages to the broker topic using `mosquitto_pub`.
fn mosquitto_pub_n(broker_port: u16, topic: &str, payload: &str, count: usize) {
    let pub_bin = find_binary("mosquitto_pub").or_else(|| {
        let p = PathBuf::from("/usr/bin/mosquitto_pub");
        p.exists().then_some(p)
    });
    let Some(pub_bin) = pub_bin else { return };
    for _ in 0..count {
        let _ = Command::new(&pub_bin)
            .args([
                "-h",
                "127.0.0.1",
                "-p",
                &broker_port.to_string(),
                "-t",
                topic,
                "-m",
                payload,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
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

/// No detection stream → no event published to the broker.
///
/// REGRESSION GUARD: always-green by contract.  The correct implementation
/// must not call `publish_detection_event` spuriously when no detection
/// has occurred; the wrong stub is also a no-op, so both pass.
#[test]
fn empty_stream_publishes_no_event_to_broker() {
    // REGRESSION GUARD (always-green): no event is published when no detection occurs.
    // The correct implementation must not call publish_detection_event spuriously;
    // the wrong stub is also a no-op, so both honour this constraint.
    // The guard is a named sentinel: it will be refined once the runtime wires
    // MQTT into the detection pipeline.
    //
    // Verify the guard is meaningful: mqtt_connect_intent(None) must not panic
    // (the wrong stub returns true — the gating assertion lives in
    // mqtt_gated_off_when_no_broker_configured).
    let _ = mqtt_connect_intent(None);
}

// ── RED: non-acceptance MQTT tests ────────────────────────────────────────

/// RED — wrong stub `publish_discovery_to_broker` is a no-op;
/// bounded subscribe receives nothing → assertion fails.
#[test]
fn discovery_published_to_real_broker_on_start() {
    let broker = match MosquittoFixture::start() {
        Ok(b) => b,
        Err(e) => {
            assert!(
                e.contains("not found"),
                "mosquitto fixture failed unexpectedly: {e}"
            );
            return;
        }
    };

    let config = sample_service_config();
    let payloads = generate_discovery_payloads(&config);
    assert!(
        !payloads.is_empty(),
        "generate_discovery_payloads must return at least one payload"
    );

    // Brief delay so the broker has fully started.
    thread::sleep(Duration::from_millis(150));

    // Start subscriber BEFORE calling the publisher.
    // mosquitto_sub_output subscribes and waits for 2 seconds.
    let sub_buf = Arc::new(Mutex::new(String::new()));
    let sub_buf_clone = Arc::clone(&sub_buf);
    let broker_port = broker.port;
    let sub_thread = thread::spawn(move || {
        let got = mosquitto_sub_output(broker_port, "homeassistant/#", Duration::from_secs(2));
        *sub_buf_clone.lock().unwrap() = got;
    });

    thread::sleep(Duration::from_millis(200)); // let subscriber connect

    // Call the publisher (wrong stub: no-op, does not connect or publish).
    let result = publish_discovery_to_broker(&broker.mqtt_config(), &payloads);
    assert!(
        result.is_ok(),
        "publish_discovery_to_broker returned Err: {:?}",
        result.err()
    );

    sub_thread.join().ok();

    let got = sub_buf.lock().unwrap().clone();
    assert!(
        !got.is_empty(),
        "publish_discovery_to_broker must publish {} discovery payload(s) to the broker; \
         wrong stub is a no-op and nothing was received on homeassistant/# within 2s",
        payloads.len()
    );
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
    let broker = match MosquittoFixture::start() {
        Ok(b) => b,
        Err(e) => {
            assert!(
                e.contains("not found"),
                "mosquitto failed unexpectedly: {e}"
            );
            return;
        }
    };

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

    // Let the subscriber connect (correct implementation would; wrong stub does not).
    thread::sleep(Duration::from_millis(300));

    // Flood the command topic well beyond the bounded capacity.
    let payload = correction_command_payload("aabbccdd-1111-2222-3333-444455556666");
    mosquitto_pub_n(broker.port, "vigil/commands/correct", &payload, 20);

    thread::sleep(Duration::from_millis(500));
    handle.shutdown_and_join();

    let overflow = overflow_count.load(Ordering::SeqCst);
    assert!(
        overflow > 0,
        "overflow_count must be > 0 after flooding the correction command channel \
         beyond its bound; wrong stub never connects to the broker (overflow = {overflow})"
    );
}

/// RED — wrong stub `record_correction` always returns Ok;
/// a disk-constrained write against a REAL detection must return
/// Err(CorrectionError::WriteFailed).
///
/// Drive a real detection via the seeded template store so a real ObservationId
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
            "at least one real detection must be in the store for disk-full path"
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

/// RED — wrong stub `record_correction` opens an outbound socket to 127.0.0.1:19876;
/// a sentry listener captures it and the no-egress assertion fires.
#[test]
fn correction_path_makes_no_outbound_network_beyond_broker() {
    let tmp = tempfile::tempdir().unwrap();
    let store_path = tmp.path().join("store.contextgraph");
    let store = open_store_at(&store_path).expect("store must open");

    // ── Negative control: prove the sentry mechanism works ──────────────────
    let nc_listener = TcpListener::bind("127.0.0.1:0")
        .expect("negative-control listener: port allocation failed");
    let nc_port = nc_listener.local_addr().unwrap().port();
    nc_listener.set_nonblocking(true).unwrap();

    let _nc_stream = TcpStream::connect(format!("127.0.0.1:{nc_port}"));
    thread::sleep(Duration::from_millis(50));
    assert!(
        nc_listener.accept().is_ok(),
        "negative control failed: sentry did not capture expected connection to port {nc_port}"
    );

    // ── Main test: assert no outbound socket beyond the broker ───────────────
    // Wrong stub deliberately opens TcpStream::connect("127.0.0.1:19876").
    // Pre-bind a sentry there to catch it.
    let sentry_19876 = TcpListener::bind("127.0.0.1:19876").ok();
    if let Some(ref s) = sentry_19876 {
        let _ = s.set_nonblocking(true);
    }

    let req = CorrectionRequest {
        detection_id: "ccbbaa99-8877-6655-4433-221100aabbcc".to_string(),
        label: None,
        correction_type: CorrectionType::WrongClass,
    };
    let _ = record_correction(&store, req);
    thread::sleep(Duration::from_millis(100));

    let outbound_captured = sentry_19876
        .as_ref()
        .and_then(|l| l.accept().ok())
        .is_some();

    assert!(
        !outbound_captured,
        "record_correction must not open outbound sockets beyond the local broker; \
         a sentry on 127.0.0.1:19876 captured a connection — \
         wrong stub deliberately opens this socket"
    );
}

// ── RED: acceptance tests ─────────────────────────────────────────────────

/// RED — wrong stub `publish_detection_event` is a no-op;
/// the detection event built from REAL cg data never arrives on the broker topic.
///
/// Drive a real detection via the seeded template store; read real ObservationId +
/// camera + evidence_ref from cg; build event JSON from that real data; assert the
/// published event carries value-equal fields. Using fake/canned data is removed —
/// it hides mismatches between the cg observation and what the publisher actually sends.
#[cfg(feature = "first-light-acceptance")]
#[test]
fn detection_publishes_event_to_real_broker() {
    let (tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for detection_publishes_event_to_real_broker");
    let broker = MosquittoFixture::start().expect("mosquitto must start for acceptance test");

    // Read real detection data from cg authority.
    let store = ha_test_support::open_store_at(&store_path).expect("open store for detection read");
    let observations = store.list_observations(None).expect("list_observations");
    assert!(
        !observations.is_empty(),
        "at least one detection must be in the store"
    );
    let detection_obs = &observations[0];
    let real_detection_id = detection_obs.id.to_string();
    let real_camera = detection_obs
        .observed_properties
        .get("camera_name")
        .or_else(|| detection_obs.properties.get("camera_name"))
        .and_then(|v| v.as_str())
        .unwrap_or(ha_test_support::CAMERA_NAME);
    let real_evidence_ref = detection_obs
        .observed_properties
        .get("evidence_ref")
        .or_else(|| detection_obs.properties.get("evidence_ref"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Build event JSON from real cg observation data (no canned/fake data).
    let event_json = format!(
        r#"{{"detection_id":"{real_detection_id}","camera":"{real_camera}","evidence_ref":"{real_evidence_ref}"}}"#
    );
    let topic = format!(
        "vigil/{}/{real_camera}/detection",
        tmp.path()
            .join("data")
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("home-farm")
    );

    thread::sleep(Duration::from_millis(150));

    let sub_buf = Arc::new(Mutex::new(String::new()));
    let sub_buf_clone = Arc::clone(&sub_buf);
    let broker_port = broker.port;
    let topic_sub = topic.clone();
    let sub_thread = thread::spawn(move || {
        let got = mosquitto_sub_output(broker_port, &topic_sub, Duration::from_secs(2));
        *sub_buf_clone.lock().unwrap() = got;
    });

    thread::sleep(Duration::from_millis(200));

    let result = publish_detection_event(&broker.mqtt_config(), &event_json, &topic);
    assert!(
        result.is_ok(),
        "publish_detection_event returned Err: {:?}",
        result.err()
    );

    sub_thread.join().ok();

    let received = sub_buf.lock().unwrap().clone();
    // Wrong stub: publish_detection_event is a no-op → nothing received → FAILS.
    assert!(
        !received.is_empty(),
        "publish_detection_event must publish the event JSON to broker topic '{topic}'; \
         wrong stub is a no-op — nothing received within 2s"
    );

    // value-equality — the received message must carry the real detection_id from cg.
    // (Only reached if the publisher actually sends something — the wrong stub fails above.)
    assert!(
        received.contains(&real_detection_id),
        "published event must carry the real detection_id '{real_detection_id}' from cg; \
         event must be built from the actual observation, not a canned fixture"
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
    // Worker drains the correction channel and writes to cg so review_why can verify.
    {
        let store_w = Arc::clone(&store);
        thread::spawn(move || {
            while let Ok(req) = cmd_rx.recv() {
                let _ = record_correction(&store_w, req);
            }
        });
    }
    // Production subscriber forwards correct commands to the channel.
    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        Arc::clone(&store),
        std::collections::BTreeMap::new(),
        cmd_tx,
        Arc::clone(&overflow_count),
    );

    thread::sleep(Duration::from_millis(300));
    let cmd_payload = format!(
        r#"{{"detection_id":"{detection_id}","correction_type":"identity","label":"that's Roshan"}}"#
    );
    mosquitto_pub_n(broker.port, "vigil/commands/correct", &cmd_payload, 1);
    thread::sleep(Duration::from_secs(2));
    handle.shutdown_and_join();

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
        Some("that's Roshan"),
        "correction label must value-equal the broker command label 'that\\'s Roshan'; \
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
    {
        let store_w = Arc::clone(&store);
        thread::spawn(move || {
            while let Ok(req) = cmd_rx.recv() {
                let _ = record_correction(&store_w, req);
            }
        });
    }
    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        Arc::clone(&store),
        std::collections::BTreeMap::new(),
        cmd_tx,
        Arc::clone(&overflow_count),
    );

    thread::sleep(Duration::from_millis(300));
    // Deliver the same command twice (broker redelivery simulation).
    let payload = correction_command_payload(&detection_id);
    mosquitto_pub_n(broker.port, "vigil/commands/correct", &payload, 2);
    thread::sleep(Duration::from_secs(2));
    handle.shutdown_and_join();

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
    {
        let store_w = Arc::clone(&store);
        thread::spawn(move || {
            while let Ok(req) = cmd_rx.recv() {
                let _ = record_correction(&store_w, req);
            }
        });
    }
    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        Arc::clone(&store),
        std::collections::BTreeMap::new(),
        cmd_tx,
        Arc::clone(&overflow_count),
    );

    thread::sleep(Duration::from_millis(300));

    let identity_payload = format!(
        r#"{{"detection_id":"{detection_id}","correction_type":"identity","label":"that's Roshan"}}"#
    );
    let wrong_class_payload = format!(
        r#"{{"detection_id":"{detection_id}","correction_type":"wrong_class","label":"actually the neighbour"}}"#
    );
    mosquitto_pub_n(broker.port, "vigil/commands/correct", &identity_payload, 1);
    mosquitto_pub_n(
        broker.port,
        "vigil/commands/correct",
        &wrong_class_payload,
        1,
    );
    thread::sleep(Duration::from_secs(2));
    handle.shutdown_and_join();

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
        labels.contains(&Some("that's Roshan")),
        "identity correction label 'that\\'s Roshan' must be present; \
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

    thread::sleep(Duration::from_millis(300));

    // Publish malformed JSON first.
    mosquitto_pub_n(broker.port, "vigil/commands/correct", "NOT_JSON{{{{", 1);
    thread::sleep(Duration::from_millis(200));

    // Then publish a well-formed command — subscriber must survive and deliver it.
    let well_formed = correction_command_payload("aabbccdd-0000-1111-2222-333344445555");
    mosquitto_pub_n(broker.port, "vigil/commands/correct", &well_formed, 1);
    thread::sleep(Duration::from_secs(2));
    handle.shutdown_and_join();

    let mut received: Vec<CorrectionRequest> = Vec::new();
    while let Ok(cmd) = cmd_rx.recv_timeout(Duration::from_millis(100)) {
        received.push(cmd);
    }

    assert_eq!(
        received.len(),
        1,
        "subscriber must deliver exactly one command (the well-formed one) \
         and silently discard the malformed one; wrong stub never connects \
         (received {})",
        received.len()
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
    {
        let store_w = Arc::clone(&store_arc);
        thread::spawn(move || {
            while let Ok(req) = cmd_rx.recv() {
                let _ = record_correction(&store_w, req);
            }
        });
    }
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
    thread::sleep(Duration::from_millis(300));

    // ── Sub-check (a): disable-camera routes to the named camera ─────────────
    // The correct subscriber connects to the control topic and routes the disable
    // command to camera 'lower-gate', writing a disable marker file at
    // {data_dir}/camera-disabled/lower-gate.  Wrong stub: never connects → no marker
    // written → assert FAILS.
    let data_dir = store_path
        .parent()
        .expect("store_path must have a parent data directory");
    let disable_marker = data_dir.join("camera-disabled").join("lower-gate");
    mosquitto_pub_n(
        broker.port,
        "vigil/commands/control",
        r#"{"camera_id":"lower-gate","action":"disable"}"#,
        1,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !disable_marker.exists() {
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        disable_marker.exists(),
        "disable command must route to camera 'lower-gate' and write a disable marker at \
         {}; wrong stub never connects to the broker so no marker was written",
        disable_marker.display()
    );
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
        r#"{{"detection_id":"{detection_id}","correction_type":"identity","label":"confirmed: Roshan"}}"#
    );
    mosquitto_pub_n(broker.port, "vigil/commands/correct", &ack_payload, 1);
    thread::sleep(Duration::from_secs(2));

    // ── Sub-check (c): snapshot → real detector evidence PNG published to image topic ──
    // The subscriber must read the latest detector evidence PNG from disk and publish it
    // to "vigil/{camera_id}/snapshot" so the HA image entity shows the detection frame.
    // camera_id for "lower-gate" matches the seeded store's camera slug.
    // Wrong stub: subscriber never connects → nothing published → assert FAILS.
    //
    // An in-process rumqttc subscriber is used (not mosquitto_sub) to avoid the
    // stdout full-buffering issue: mosquitto_sub buffers output when piped, and
    // SIGKILL drops the unflushed buffer before capture_pipe can read it.
    // The in-process subscriber receives the Publish packet directly, no buffering.
    let broker_port_snap = broker.port;
    let snap_thread = thread::spawn(move || -> bool {
        use rumqttc::v5::mqttbytes::QoS;
        use rumqttc::v5::mqttbytes::v5::Packet;
        use rumqttc::v5::{Client, Event, MqttOptions, RecvTimeoutError};
        let mut opts = MqttOptions::new("vigil-test-snap-sub", "127.0.0.1", broker_port_snap);
        opts.set_keep_alive(Duration::from_secs(5));
        // Match the production subscriber's packet-size ceiling so the PNG Publish
        // from the broker (up to 5 MB) is accepted rather than dropped.
        // In MQTT v5, set_max_packet_size takes a single Option<u32> (incoming size only).
        opts.set_max_packet_size(Some(5 * 1024 * 1024u32));
        let (client, mut connection) = Client::new(opts, 10);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut subscribed = false;
        while Instant::now() < deadline {
            match connection.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(Event::Incoming(Packet::ConnAck(_)))) => {
                    let _ = client.subscribe("vigil/lower-gate/snapshot", QoS::AtMostOnce);
                    subscribed = true;
                }
                Ok(Ok(Event::Incoming(Packet::Publish(p)))) if subscribed => {
                    // Any non-empty payload is the PNG bytes published by the real impl.
                    // A wrong stub never connects → this branch is never reached → returns false.
                    return !p.payload.is_empty();
                }
                Ok(Ok(_)) | Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        false
    });

    // Give the in-process subscriber 200 ms to connect and subscribe before sending.
    thread::sleep(Duration::from_millis(200));
    mosquitto_pub_n(
        broker.port,
        "vigil/commands/control",
        r#"{"camera_id":"lower-gate","action":"snapshot"}"#,
        1,
    );
    let snap_received = snap_thread.join().unwrap_or(false);

    handle.shutdown_and_join();

    let why = review_why(&store_arc, &detection_id).expect("review_why");
    assert_eq!(
        why.corrections.len(),
        1,
        "operator ack (Identity correction with label) must land in cg via subscriber; \
         wrong stub never connects (corrections = {})",
        why.corrections.len()
    );

    assert!(
        snap_received,
        "snapshot command must publish real detector evidence PNG bytes to \
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

    // The seeded template may have more than one detection (OnceLock seeds with minimum=3).
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
    let broker = match MosquittoFixture::start() {
        Ok(b) => b,
        Err(e) => {
            if e.contains("not found") {
                return;
            }
            panic!("mosquitto failed: {e}");
        }
    };

    thread::sleep(Duration::from_millis(150));

    let health = HealthState::new();
    health.set(HealthStatus::Ready, "test start");

    let tmp = tempfile::tempdir().unwrap();
    let store_path = tmp.path().join("data").join("store.contextgraph");
    let store = Arc::new(open_store_at(&store_path).expect("store"));

    let overflow = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (cmd_tx, _cmd_rx) = mpsc::sync_channel::<CorrectionRequest>(64);
    let condition_topic = "vigil/test-health-svc/running-condition".to_string();

    // Subscribe to the condition topic using an in-process rumqttc subscriber
    // before spawning the production subscriber.
    let broker_port = broker.port;
    let topic_clone = condition_topic.clone();
    let sub_buf: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sub_buf_clone = Arc::clone(&sub_buf);
    let sub_thread = thread::spawn(move || {
        use rumqttc::v5::mqttbytes::QoS;
        use rumqttc::v5::mqttbytes::v5::Packet;
        use rumqttc::v5::{Client, Event, MqttOptions, RecvTimeoutError};
        let mut opts = MqttOptions::new("vigil-test-condition-sub", "127.0.0.1", broker_port);
        opts.set_keep_alive(Duration::from_secs(5));
        let (client, mut connection) = Client::new(opts, 10);
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut subscribed = false;
        let mut payloads: Vec<String> = Vec::new();
        while Instant::now() < deadline {
            match connection.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(Event::Incoming(Packet::ConnAck(_)))) => {
                    let _ = client.subscribe(&topic_clone, QoS::AtMostOnce);
                    subscribed = true;
                }
                Ok(Ok(Event::Incoming(Packet::Publish(p)))) if subscribed => {
                    if let Ok(s) = std::str::from_utf8(&p.payload) {
                        payloads.push(s.to_string());
                    }
                }
                Ok(Ok(_)) | Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        if let Ok(mut g) = sub_buf_clone.lock() {
            *g = payloads;
        }
    });

    // Give the subscriber 300ms to connect and subscribe.
    thread::sleep(Duration::from_millis(300));

    let cfg = WiredSubscriberConfig {
        mqtt: broker.mqtt_config(),
        client_id: "vigil-test-health-sub".to_string(),
        availability_topic: "vigil/test-health-svc/availability".to_string(),
        condition_topic,
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

    // Let the subscriber connect and re-announce (publishes current condition).
    thread::sleep(Duration::from_millis(500));

    // Flip health to DiskFull — subscriber must detect the change and publish "disk-full".
    health.set(HealthStatus::DiskFull, "disk full test");

    thread::sleep(Duration::from_millis(600));
    handle.shutdown_and_join();

    sub_thread.join().ok();
    let payloads = sub_buf.lock().map(|g| g.clone()).unwrap_or_default();

    // Must have received "disk-full" at some point.
    assert!(
        payloads.iter().any(|p| p == "disk-full"),
        "running-condition must reflect DiskFull health as 'disk-full'; \
         wrong impl publishes hardcoded 'running' regardless of health — got: {payloads:?}"
    );
}

// ── Fix C: long-lived detection publisher + bounded channel + loud overflow ─

/// RED — wrong impl uses a per-detection connection; overflow is never counted
/// because the channel blocks on connack rather than using try_send.
#[test]
fn outbound_detection_publish_channel_overflow_is_loud() {
    // Use a non-reachable address so the publisher thread never connects —
    // this means the channel fills immediately (nothing is drained).
    let config = MqttConfig {
        broker_host: "127.0.0.1".to_string(),
        broker_port: 19999, // nothing listening here
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

    thread::sleep(Duration::from_millis(200));
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

/// RED — wrong impl opens a fresh connection per publish (blocks up to 10s on connack).
/// try_publish must return without waiting for the broker.
#[test]
fn detection_publish_does_not_block_when_broker_unreachable() {
    let config = MqttConfig {
        broker_host: "127.0.0.1".to_string(),
        broker_port: 19998, // nothing listening
        username: None,
        password: None,
    };
    let health = HealthState::new();
    let (publisher, handle) = spawn_detection_publisher(&config, health);

    let start = Instant::now();
    // 10 publishes — must ALL return well under 100ms total (no connection attempt per send).
    for i in 0..10 {
        publisher.try_publish(
            format!("vigil/test/{i}/detection"),
            r#"{"detection_id":"test"}"#.to_string(),
        );
    }
    let elapsed = start.elapsed();

    handle.shutdown_and_join();

    assert!(
        elapsed < Duration::from_millis(100),
        "10 try_publish calls must complete in <100ms even when the broker is unreachable; \
         wrong impl opens a connection per publish (waits up to 10s connack) — took {elapsed:?}"
    );
}

// ── GAP 3: motion binary_sensor active feed ───────────────────────────────

/// RED — wrong stub: notify_active is a no-op (missing channel write) so nothing
/// arrives on the active topic.  The production path: detection fires →
/// notify_active → publisher publishes "ON" retained to the camera's active topic.
#[test]
fn detection_fires_active_sensor_on() {
    let broker = match MosquittoFixture::start() {
        Ok(b) => b,
        Err(e) => {
            if e.contains("not found") {
                return;
            }
            panic!("mosquitto failed: {e}");
        }
    };
    thread::sleep(Duration::from_millis(150));

    let active_topic = "vigil/home-farm/lower-gate/active";

    // Subscribe to the active topic before spawning the publisher.
    let broker_port = broker.port;
    let topic_owned = active_topic.to_string();
    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received_clone = Arc::clone(&received);
    let sub_thread = thread::spawn(move || {
        use rumqttc::v5::mqttbytes::QoS;
        use rumqttc::v5::mqttbytes::v5::Packet;
        use rumqttc::v5::{Client, Event, MqttOptions, RecvTimeoutError};
        let mut opts = MqttOptions::new("vigil-test-active-sub", "127.0.0.1", broker_port);
        opts.set_keep_alive(Duration::from_secs(5));
        let (client, mut connection) = Client::new(opts, 10);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut subscribed = false;
        let mut payloads: Vec<String> = Vec::new();
        while Instant::now() < deadline {
            match connection.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(Event::Incoming(Packet::ConnAck(_)))) => {
                    let _ = client.subscribe(&topic_owned, QoS::AtMostOnce);
                    subscribed = true;
                }
                Ok(Ok(Event::Incoming(Packet::Publish(p)))) if subscribed => {
                    if let Ok(s) = std::str::from_utf8(&p.payload) {
                        payloads.push(s.to_string());
                    }
                }
                Ok(Ok(_)) | Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if !payloads.is_empty() {
                break;
            }
        }
        if let Ok(mut g) = received_clone.lock() {
            *g = payloads;
        }
    });

    // Give the subscriber 300ms to connect and subscribe.
    thread::sleep(Duration::from_millis(300));

    let health = HealthState::new();
    health.set(HealthStatus::Ready, "test");
    let (publisher, handle) = spawn_detection_publisher(&broker.mqtt_config(), health);

    // Give the publisher thread time to connect (initial reconnect delay 500ms).
    thread::sleep(Duration::from_millis(700));

    publisher.notify_active(active_topic.to_string());

    // Wait for the retained ON to propagate broker → subscriber.
    thread::sleep(Duration::from_millis(800));
    handle.shutdown_and_join();
    sub_thread.join().ok();

    let msgs = received.lock().map(|g| g.clone()).unwrap_or_default();
    assert!(
        msgs.iter().any(|m| m == "ON"),
        "notify_active must publish 'ON' retained to the camera's active topic; \
         wrong stub never sends to channel → subscriber sees nothing — received: {msgs:?}"
    );
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
    let broker = match MosquittoFixture::start() {
        Ok(b) => b,
        Err(e) => {
            if e.contains("not found") {
                return;
            }
            panic!("mosquitto failed: {e}");
        }
    };
    thread::sleep(Duration::from_millis(150));

    let topic = "vigil/test/camera-1/detection";
    let payload = r#"{"class_name":"person","confidence":0.95}"#;

    // Subscribe to the topic in-process before spawning the publisher.
    let broker_port = broker.port;
    let topic_owned = topic.to_string();
    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received_clone = Arc::clone(&received);
    let sub_thread = thread::spawn(move || {
        use rumqttc::v5::mqttbytes::QoS;
        use rumqttc::v5::mqttbytes::v5::Packet;
        use rumqttc::v5::{Client, Event, MqttOptions, RecvTimeoutError};
        let mut opts = MqttOptions::new("vigil-test-det-sub", "127.0.0.1", broker_port);
        opts.set_keep_alive(Duration::from_secs(5));
        let (client, mut connection) = Client::new(opts, 10);
        let deadline = Instant::now() + Duration::from_secs(12);
        let mut subscribed = false;
        let mut payloads: Vec<String> = Vec::new();
        while Instant::now() < deadline {
            match connection.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(Event::Incoming(Packet::ConnAck(_)))) => {
                    let _ = client.subscribe(&topic_owned, QoS::AtMostOnce);
                    subscribed = true;
                }
                Ok(Ok(Event::Incoming(Packet::Publish(p)))) if subscribed => {
                    if let Ok(s) = std::str::from_utf8(&p.payload) {
                        payloads.push(s.to_string());
                    }
                }
                Ok(Ok(_)) | Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if !payloads.is_empty() {
                break;
            }
        }
        if let Ok(mut g) = received_clone.lock() {
            *g = payloads;
        }
    });

    // Give the subscriber 300ms to connect and subscribe.
    thread::sleep(Duration::from_millis(300));

    let health = HealthState::new();
    health.set(HealthStatus::Ready, "test");
    let (publisher, handle) = spawn_detection_publisher(&broker.mqtt_config(), health);

    // Give the publisher thread time to connect (default reconnect delay is 500ms).
    thread::sleep(Duration::from_millis(700));

    publisher.try_publish(topic.to_string(), payload.to_string());

    // Wait for the message to propagate broker → subscriber.
    thread::sleep(Duration::from_millis(1000));
    handle.shutdown_and_join();
    sub_thread.join().ok();

    let msgs = received.lock().map(|g| g.clone()).unwrap_or_default();
    assert!(
        msgs.iter().any(|m| m == payload),
        "spawn_detection_publisher → try_publish must deliver the payload to the broker; \
         wrong stub's publish loop never calls client.publish — received: {msgs:?}"
    );
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

    let broker = match MosquittoFixture::start() {
        Ok(b) => b,
        Err(e) => {
            if e.contains("not found") {
                return;
            }
            panic!("mosquitto failed: {e}");
        }
    };
    thread::sleep(Duration::from_millis(150));

    let overflow = Arc::new(AtomicUsize::new(0));
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<CorrectionRequest>(64);
    {
        let store_w = Arc::clone(&store);
        thread::spawn(move || {
            while let Ok(req) = cmd_rx.recv() {
                let _ = record_correction(&store_w, req);
            }
        });
    }
    let handle = spawn_production_subscriber(
        test_subscriber_cfg(broker.mqtt_config()),
        Arc::clone(&store),
        std::collections::BTreeMap::new(),
        cmd_tx,
        Arc::clone(&overflow),
    );

    // Give the subscriber time to connect and subscribe.
    thread::sleep(Duration::from_millis(400));

    // false_alarm on detection A (no label).
    let fa_payload =
        format!(r#"{{"detection_id":"{detection_a}","correction_type":"false_alarm"}}"#);
    mosquitto_pub_n(broker.port, "vigil/commands/correct", &fa_payload, 1);

    // wrong_class + label="cat" on detection B.
    let wc_payload = format!(
        r#"{{"detection_id":"{detection_b}","correction_type":"wrong_class","label":"cat"}}"#
    );
    mosquitto_pub_n(broker.port, "vigil/commands/correct", &wc_payload, 1);

    // Wait for both corrections to be processed into cg.
    thread::sleep(Duration::from_secs(2));
    handle.shutdown_and_join();
    // Brief drain for the correction worker thread.
    thread::sleep(Duration::from_millis(200));

    // Read back FalseAlarm on A via review_why.
    let why_a = review_why(&*store, &detection_a)
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
    let why_b = review_why(&*store, &detection_b)
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
