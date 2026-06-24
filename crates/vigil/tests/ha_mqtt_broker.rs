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
    CameraConfig, CorrectionRequest, CorrectionType, MqttConfig, ServiceConfig,
    generate_discovery_payloads, mqtt_connect_intent, publish_discovery_to_broker,
    record_correction, spawn_correction_subscriber,
};

// ── Acceptance-only imports ────────────────────────────────────────────────
#[cfg(feature = "first-light-acceptance")]
use vigil::{
    CorrectionError, publish_detection_event, review_why, spawn_wired_correction_subscriber,
};

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
    // Bounded channel with capacity 1; a correct subscriber fills it and increments overflow.
    let (tx, _rx) = mpsc::sync_channel::<CorrectionRequest>(1);

    let handle = spawn_correction_subscriber(broker.mqtt_config(), tx, Arc::clone(&overflow_count));

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
    let (tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for correction_and_event_under_disk_full_fail_loudly");
    let data_dir = tmp.path().join("data");

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

    // Open the store, then restrict the directory to read-only (simulates disk-full).
    let store =
        ha_test_support::open_store_at(&store_path).expect("open store for disk-constrained write");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o444));
    }

    let req = CorrectionRequest {
        detection_id: detection_id.clone(),
        label: None,
        correction_type: CorrectionType::FalseAlarm,
    };
    let result = record_correction(&store, req);

    // Restore permissions before tmp drop (so TempDir cleanup succeeds).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o755));
    }

    // Must return Err(WriteFailed) specifically — a real id exists so NoAnchor is wrong.
    // Wrong stub always returns Ok(receipt) → assertion FAILS.
    assert!(
        matches!(result, Err(CorrectionError::WriteFailed(_))),
        "record_correction must return Err(WriteFailed) when the cg write fails against \
         a constrained store with a real detection id; wrong stub always returns Ok — \
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

/// RED — wrong stub `spawn_wired_correction_subscriber` never connects to the broker;
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
    // Production path: subscriber wires broker → record_correction internally.
    // The test observes via cg/review, not by draining a command channel directly.
    let handle = spawn_wired_correction_subscriber(
        broker.mqtt_config(),
        Arc::clone(&store),
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

/// RED — wrong stub `spawn_wired_correction_subscriber` never connects; redelivered
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
    // Production path: subscriber calls record_correction internally — no cmd_rx drain here.
    let handle = spawn_wired_correction_subscriber(
        broker.mqtt_config(),
        Arc::clone(&store),
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

/// RED — wrong stub `spawn_wired_correction_subscriber` never connects; two distinct
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
    // Production path: subscriber calls record_correction for each command internally.
    let handle = spawn_wired_correction_subscriber(
        broker.mqtt_config(),
        Arc::clone(&store),
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
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<CorrectionRequest>(100);
    let handle =
        spawn_correction_subscriber(broker.mqtt_config(), cmd_tx, Arc::clone(&overflow_count));

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

/// RED — three sub-checks for operator actions delivered through the production
/// wired subscriber. All three fail via wrong stub (never connects → nothing routed).
///
/// (a) disable-camera: the wired subscriber receives the command and writes a disable
///     marker at {data_dir}/camera-disabled/{camera_id}; wrong stub → never connects →
///     no marker written → assert FAILS.
/// (b) snapshot: the wired subscriber receives the command and writes a snapshot file
///     under {data_dir}/snapshots/{camera_id}/; wrong stub → never connects → no file
///     written → assert FAILS.
/// (c) ack (Identity correction) via subscriber → cg; wrong stub → nothing in cg →
///     review_why shows no corrections → assert_eq FAILS.
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
    // Production path: all operator commands go through the wired subscriber.
    // No cmd_rx in test body.
    let handle = spawn_wired_correction_subscriber(
        broker.mqtt_config(),
        Arc::clone(&store_arc),
        Arc::clone(&overflow_count),
    );
    thread::sleep(Duration::from_millis(300));

    // ── Sub-check (a): disable-camera routes to the named camera ─────────────
    // The correct wired subscriber connects to the control topic and routes the disable
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

    // ── Sub-check (b): snapshot command produces a file for the named camera ──
    // The correct wired subscriber receives the snapshot command for 'lower-gate' and
    // writes a snapshot file under {data_dir}/snapshots/lower-gate/.  Wrong stub: never
    // connects → no file written → assert FAILS.
    let snap_dir = data_dir.join("snapshots").join("lower-gate");
    mosquitto_pub_n(
        broker.port,
        "vigil/commands/control",
        r#"{"camera_id":"lower-gate","action":"snapshot"}"#,
        1,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let has_file =
            snap_dir.is_dir() && fs::read_dir(&snap_dir).map_or(false, |mut d| d.next().is_some());
        if has_file || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        snap_dir.is_dir() && fs::read_dir(&snap_dir).map_or(false, |mut d| d.next().is_some()),
        "snapshot command must produce a file under {}/; \
         wrong stub never connects to the broker so no snapshot file was written",
        snap_dir.display()
    );

    // ── Sub-check (c): ack (Identity) via subscriber → cg ─────────────────────
    // Wrong stub: subscriber never calls record_correction → corrections empty → FAILS.
    let ack_payload = format!(
        r#"{{"detection_id":"{detection_id}","correction_type":"identity","label":"confirmed: Roshan"}}"#
    );
    mosquitto_pub_n(broker.port, "vigil/commands/correct", &ack_payload, 1);
    thread::sleep(Duration::from_secs(2));
    handle.shutdown_and_join();

    let why = review_why(&store_arc, &detection_id).expect("review_why");
    assert_eq!(
        why.corrections.len(),
        1,
        "operator ack (Identity correction with label) must land in cg via subscriber; \
         wrong stub never connects (corrections = {})",
        why.corrections.len()
    );
}

/// RED — wrong stub `spawn_wired_correction_subscriber` never calls record_correction;
/// the ack must survive two successive store reopens (two daemon restarts).
///
/// Drive ack via the production wired subscriber (no direct record_correction call
/// in test body). Drop ALL in-process state after delivery attempt. Reopen ONLY the cg
/// Store (fresh handle each time). Assert via list_observations (no review_why) so there
/// is no in-process mirror that could answer for a shadowed write.
#[cfg(feature = "first-light-acceptance")]
#[test]
fn acknowledge_survives_reopen_and_restart_via_cg_authority() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for acknowledge_survives_reopen_and_restart_via_cg_authority");
    let broker = MosquittoFixture::start().expect("mosquitto must start");

    let detection_id = {
        let store = ha_test_support::open_store_at(&store_path).expect("open store");
        let obs = store.list_observations(None).expect("list");
        assert!(!obs.is_empty(), "need at least one detection");
        obs[0].id.to_string()
    };

    // Drive ack via the production wired subscriber — no record_correction in test body.
    {
        let store =
            Arc::new(ha_test_support::open_store_at(&store_path).expect("open store for ack"));
        let overflow_count = Arc::new(AtomicUsize::new(0));
        let handle = spawn_wired_correction_subscriber(
            broker.mqtt_config(),
            Arc::clone(&store),
            Arc::clone(&overflow_count),
        );
        thread::sleep(Duration::from_millis(300));
        let ack_payload = format!(
            r#"{{"detection_id":"{detection_id}","correction_type":"identity","label":"acknowledged"}}"#
        );
        mosquitto_pub_n(broker.port, "vigil/commands/correct", &ack_payload, 1);
        thread::sleep(Duration::from_secs(2));
        handle.shutdown_and_join();
        // Arc<Store> dropped here — all in-process state released.
    }

    // First reopen — simulates daemon restart.
    // Read via list_observations ONLY (no review_why).
    {
        let store_r1 =
            ha_test_support::open_store_at(&store_path).expect("open store after first reopen");
        let all_obs = store_r1
            .list_observations(None)
            .expect("list after first reopen");
        let correction_in_cg = all_obs.iter().any(|o| {
            o.observed_properties
                .get("anchored_detection_id")
                .or_else(|| o.properties.get("anchored_detection_id"))
                .and_then(|v| v.as_str())
                == Some(detection_id.as_str())
        });
        assert!(
            correction_in_cg,
            "acknowledged correction must persist in cg after first store reopen; \
             wrong stub subscriber never calls record_correction → nothing written \
             → anchored_detection_id '{detection_id}' absent from list_observations"
        );
    }

    // Second reopen — simulates a second restart (e.g. add-on update).
    {
        let store_r2 =
            ha_test_support::open_store_at(&store_path).expect("open store after second reopen");
        let all_obs_2 = store_r2
            .list_observations(None)
            .expect("list after second reopen");
        let still_in_cg = all_obs_2.iter().any(|o| {
            o.observed_properties
                .get("anchored_detection_id")
                .or_else(|| o.properties.get("anchored_detection_id"))
                .and_then(|v| v.as_str())
                == Some(detection_id.as_str())
        });
        assert!(
            still_in_cg,
            "acknowledged correction must survive a second store reopen; \
             wrong stub never wrote to cg so both reopens show no correction \
             (detection_id = {detection_id})"
        );
    }
}
