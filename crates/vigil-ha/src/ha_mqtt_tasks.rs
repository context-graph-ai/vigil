use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{LastWill, Packet};
use rumqttc::v5::{Client, Event, MqttOptions, RecvTimeoutError};

use vigil::{Secret, SiteControl, SubmitCorrectionError};

use crate::ha_discovery::{CommandTopicMessage, DiscoveryPayload, parse_command_topic};

// ── Published topic surface ────────────────────────────────────────────────
//
// These are the exact wire strings a Home Assistant automation or MQTT
// client binds to directly. Renaming one is a deliberate, reviewed change to
// a published identifier, not a routine refactor — see
// `crates/vigil-ha/tests/mqtt_topic_contract.rs`.

/// Owner-issued corrections (identity / wrong-class / false-alarm / enroll)
/// arrive on this topic.
pub const CORRECTION_COMMAND_TOPIC: &str = "vigil/commands/correct";
/// Enable / disable / snapshot control commands arrive on this topic.
pub const CONTROL_COMMAND_TOPIC: &str = "vigil/commands/control";
/// Home Assistant announces its own restart on this topic.
pub const HOME_ASSISTANT_STATUS_TOPIC: &str = "homeassistant/status";

// ── Public MQTT config ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MqttConfig {
    pub broker_host: String,
    pub broker_port: u16,
    pub username: Option<String>,
    pub password: Option<Secret>,
}

/// The one place a configured MQTT credential is exposed and handed to the
/// client library. Every `MqttOptions` builder in this module funnels
/// through here so that claim stays true rather than approximate.
fn apply_credentials(opts: &mut MqttOptions, config: &MqttConfig) {
    if let (Some(user), Some(pass)) = (&config.username, &config.password) {
        opts.set_credentials(user, pass.expose_secret());
    }
}

// ── Subscriber handle ──────────────────────────────────────────────────────

pub struct SubscriberHandle {
    /// Counts correction commands dropped due to channel overflow.
    pub overflow_count: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl SubscriberHandle {
    pub fn shutdown_and_join(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

// ── Internal helpers ───────────────────────────────────────────────────────

/// Generate a unique MQTT client ID for this connection.
fn next_client_id(prefix: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{seq}")
}

fn make_mqtt_options(config: &MqttConfig, id_prefix: &str) -> MqttOptions {
    let mut opts = MqttOptions::new(
        next_client_id(id_prefix),
        &config.broker_host,
        config.broker_port,
    );
    opts.set_keep_alive(Duration::from_secs(30));
    apply_credentials(&mut opts, config);
    opts
}

/// Drive the connection event loop until ConnAck or timeout.
/// Returns Ok(()) when connected, Err when connection fails or times out.
fn wait_for_connack(
    connection: &mut rumqttc::v5::Connection,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match connection.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(Event::Incoming(Packet::ConnAck(_)))) => return Ok(()),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(format!("MQTT connection error: {e}")),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err("broker disconnected before ConnAck".to_string());
            }
        }
    }
    Err("timed out waiting for broker ConnAck".to_string())
}

// ── Connect-intent predicate ───────────────────────────────────────────────

/// Returns true when a broker is configured; false otherwise.
/// Used as a gate before any MQTT I/O — avoids opening connections when
/// no broker is set (e.g. a local-only vigil deployment).
pub fn mqtt_connect_intent(config: Option<&MqttConfig>) -> bool {
    config.is_some()
}

// ── Discovery publisher ────────────────────────────────────────────────────

/// Publish all HA MQTT discovery payloads to the broker as retained messages.
///
/// Blocks until the payloads have been handed to the MQTT library.  Returns
/// after a brief drain to let QoS-0 sends flush.  Does NOT wait for broker
/// ACKs — the caller should not assume delivery is confirmed when this returns.
pub fn publish_discovery_to_broker(
    config: &MqttConfig,
    payloads: &[DiscoveryPayload],
) -> Result<(), String> {
    if payloads.is_empty() {
        return Ok(());
    }

    let opts = make_mqtt_options(config, "vd");
    let (client, mut connection) = Client::new(opts, 256);

    wait_for_connack(&mut connection, Duration::from_secs(10))?;

    for payload in payloads {
        let json = serde_json::to_string(&payload.payload)
            .map_err(|e| format!("serialize discovery payload: {e}"))?;
        client
            .publish(&payload.topic, QoS::AtMostOnce, true, json.into_bytes())
            .map_err(|e| format!("publish discovery: {e}"))?;
    }

    // Drain briefly so the event loop flushes the outbound queue before we disconnect.
    let flush_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < flush_deadline {
        match connection.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(_)) => {}
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Err(_)) => break,
        }
    }

    let _ = client.disconnect();
    Ok(())
}

// ── Production subscriber configuration ───────────────────────────────────

/// Full configuration for the long-lived production subscriber.
pub struct WiredSubscriberConfig {
    pub mqtt: MqttConfig,
    /// Raw service_id used in Vigil MQTT state topics.
    pub service_id: String,
    /// Stable MQTT client id derived from the service_id (does not reset on
    /// restart so the broker can correlate last-will with prior sessions).
    pub client_id: String,
    /// Retained availability topic — last-will publishes "offline" here on
    /// unclean disconnect so HA shows "unavailable" for all Vigil entities.
    pub availability_topic: String,
    /// Running-condition topic — republished as the live health-mapped string
    /// after broker restart and whenever health changes.
    pub condition_topic: String,
    /// Retained discovery payloads — republished after broker restart or HA
    /// restart so HA re-registers all Vigil entities without manual reload.
    pub discovery_payloads: Vec<crate::ha_discovery::DiscoveryPayload>,
    /// Live health state — polled each loop iteration to publish condition
    /// updates as retained messages when health changes.
    pub health: vigil::HealthState,
}

fn make_production_subscriber_options(
    config: &MqttConfig,
    client_id: &str,
    availability_topic: &str,
) -> MqttOptions {
    let mut opts = MqttOptions::new(client_id, &config.broker_host, config.broker_port);
    opts.set_keep_alive(Duration::from_secs(30));
    apply_credentials(&mut opts, config);
    // Last-will: broker publishes "offline" to the availability topic when
    // the TCP connection drops without a clean DISCONNECT packet.
    // In MQTT 5, LastWill::new takes an extra properties argument (None = no user properties).
    opts.set_last_will(LastWill::new(
        availability_topic,
        b"offline" as &[u8],
        QoS::AtMostOnce,
        true,
        None,
    ));
    opts
}

/// Re-publish all discovery payloads + availability + running-condition into
/// the broker using the already-connected client.  Called on every ConnAck
/// (covers broker restart) and on `homeassistant/status = "online"` (covers
/// HA restart).
///
/// `current_condition` is the live health-mapped string (e.g. "running",
/// "disk-full") — published as a retained message so HA always sees the
/// current state even after a broker restart.
fn re_announce_to_broker(
    client: &Client,
    payloads: &[crate::ha_discovery::DiscoveryPayload],
    availability_topic: &str,
    condition_topic: &str,
    current_condition: &str,
    service_id: &str,
    control: Option<&dyn SiteControl>,
) {
    for payload in payloads {
        if let Ok(json) = serde_json::to_string(&payload.payload) {
            let _ = client.publish(&payload.topic, QoS::AtMostOnce, true, json.into_bytes());
        }
    }
    let _ = client.publish(
        availability_topic,
        QoS::AtMostOnce,
        true,
        b"online" as &[u8],
    );
    // Condition is retained (true) — it's a state, not an event.
    let _ = client.publish(
        condition_topic,
        QoS::AtMostOnce,
        true,
        current_condition.as_bytes().to_vec(),
    );
    // The enable/disable reflection is a read of Vigil's own camera state, so
    // it is published only where that state is reachable. A presence-only
    // connection has no door onto it and publishes the rest regardless: the
    // entities, their availability, and the condition this node is in.
    if let Some(control) = control {
        publish_enabled_states(client, service_id, control);
    }
}

/// Published state topic for a camera's `enabled` switch/reflection.
pub fn enabled_state_topic(service_id: &str, camera_id: &str) -> String {
    format!("vigil/{service_id}/{camera_id}/enabled")
}

/// Published snapshot-image topic for a camera. No service_id prefix — the
/// control handler derives it from `camera_id` alone.
pub fn snapshot_topic(camera_id: &str) -> String {
    format!("vigil/{camera_id}/snapshot")
}

fn publish_enabled_states(client: &Client, service_id: &str, control: &dyn SiteControl) {
    for (camera_id, enabled) in control.camera_enabled_states() {
        publish_enabled_state(client, service_id, &camera_id, enabled);
    }
}

fn publish_enabled_state(client: &Client, service_id: &str, camera_id: &str, enabled: bool) {
    let payload = if enabled { "ON" } else { "OFF" };
    let _ = client.publish(
        enabled_state_topic(service_id, camera_id),
        QoS::AtMostOnce,
        true,
        payload.as_bytes().to_vec(),
    );
}

fn handle_production_control(
    payload: &[u8],
    service_id: &str,
    control: &dyn SiteControl,
    client: &Client,
) {
    #[derive(serde::Deserialize)]
    struct ControlCmd {
        service_id: Option<String>,
        camera_id: String,
        action: String,
    }

    let Ok(cmd) = serde_json::from_slice::<ControlCmd>(payload) else {
        return;
    };

    match cmd.action.as_str() {
        "disable" => {
            let state_service_id = cmd.service_id.as_deref().unwrap_or(service_id);
            if control.set_camera_enabled(&cmd.camera_id, false) {
                publish_enabled_state(client, state_service_id, &cmd.camera_id, false);
            }
        }
        "enable" => {
            let state_service_id = cmd.service_id.as_deref().unwrap_or(service_id);
            if control.set_camera_enabled(&cmd.camera_id, true) {
                publish_enabled_state(client, state_service_id, &cmd.camera_id, true);
            }
        }
        "snapshot" => {
            // Publish the most-recent detector evidence PNG to the HA image topic so
            // the HA image entity shows the latest detection frame.
            // Topic matches the image entity's image_topic (see `snapshot_topic`).
            let image_topic = snapshot_topic(&cmd.camera_id);
            if let Some(bytes) = control.latest_detection_image(&cmd.camera_id) {
                let _ = client.publish(&image_topic, QoS::AtMostOnce, false, bytes);
            } else {
                println!("snapshot_no_frame camera={}", cmd.camera_id);
            }
        }
        _ => {}
    }
}

/// Spawn the long-lived production MQTT subscriber.
///
/// Handles BOTH the correction topic (`vigil/commands/correct`) and the control
/// topic (`vigil/commands/control`).  Correction commands are forwarded via
/// `control.submit_correction` — if its queue is full, the command is dropped,
/// `overflow_count` is incremented, and a log line is emitted.  Control commands
/// (disable/enable/snapshot) are handled inline via `control`.
///
/// Vigil owns the receive end of that submission queue and is responsible for
/// draining it (its own correction-writer thread, which calls
/// `record_correction`).  Holding the receive end without draining demonstrates
/// overflow behaviour.
///
/// Features:
///   - Stable `client_id` so the broker can correlate the last-will across
///     reconnects.
///   - Last-will: broker publishes "offline" to `availability_topic` on
///     unclean TCP drop.
///   - On every ConnAck (broker restart): re-subscribes all command topics
///     and re-publishes all discovery + availability + running-condition
///     payloads so HA entities survive a Mosquitto restart without a reboot.
///   - Subscribes `homeassistant/status`; on "online" (HA restart): same
///     re-announce so HA sees all Vigil entities immediately.
///   - Routes `vigil/commands/control` to `control.set_camera_enabled` by
///     name, which is Vigil's own enable/disable action.
///   - Polls `cfg.health` each loop iteration and publishes a retained
///     condition update when health changes.
///   - Brief exponential backoff on connection errors before rumqttc retries.
pub fn spawn_production_subscriber(
    cfg: WiredSubscriberConfig,
    control: Arc<dyn SiteControl>,
    overflow_count: Arc<AtomicUsize>,
) -> SubscriberHandle {
    spawn_broker_connection(cfg, Some(control), overflow_count)
}

/// Publish this site's presence and keep it published, WITHOUT accepting owner
/// commands: the discovery payloads that make the entities exist, the retained
/// availability that makes them usable, the running condition this node is in,
/// and the last-will that turns them unavailable if this process dies unclean.
///
/// This is what a run with no store behind it brings up. Such a run cannot
/// record a correction, so it must not listen for one — but everything it CAN
/// still do (watching, detecting, alerting, and saying what condition it is in)
/// reaches Home Assistant exactly as it does on a healthy node, which is the
/// whole reason that run keeps going at all.
pub fn spawn_site_presence(cfg: WiredSubscriberConfig) -> SubscriberHandle {
    spawn_broker_connection(cfg, None, Arc::new(AtomicUsize::new(0)))
}

/// The one broker connection both surfaces are built from. With a door onto
/// Vigil's own camera state it also subscribes and serves owner commands;
/// without one it publishes and nothing more. One loop, so presence can never
/// drift from what a full connection publishes.
fn spawn_broker_connection(
    cfg: WiredSubscriberConfig,
    control: Option<Arc<dyn SiteControl>>,
    overflow_count: Arc<AtomicUsize>,
) -> SubscriberHandle {
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let overflow_clone = Arc::clone(&overflow_count);

    let thread = thread::spawn(move || {
        let opts =
            make_production_subscriber_options(&cfg.mqtt, &cfg.client_id, &cfg.availability_topic);
        let (client, mut connection) = Client::new(opts, 100);
        let mut reconnect_delay_ms: u64 = 500;

        // Track the last-published health code so we only publish on changes.
        // u16::MAX is a sentinel meaning "not yet published".
        let mut last_health_code: u16 = u16::MAX;

        loop {
            if shutdown_clone.load(Ordering::SeqCst) {
                break;
            }

            let event = match connection.recv_timeout(Duration::from_millis(100)) {
                Ok(result) => result,
                Err(RecvTimeoutError::Timeout) => {
                    // Even on timeout, poll health and push an update if it changed.
                    let (cur_health, _) = cfg.health.snapshot();
                    let cur_code = cur_health.as_u16();
                    if cur_code != last_health_code {
                        last_health_code = cur_code;
                        let condition =
                            crate::ha_discovery::map_health_to_running_condition(cur_health);
                        let _ = client.publish(
                            &cfg.condition_topic,
                            QoS::AtMostOnce,
                            true,
                            condition.as_bytes().to_vec(),
                        );
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            };

            // Poll health on every event too.
            let (cur_health, _) = cfg.health.snapshot();
            let cur_code = cur_health.as_u16();
            if cur_code != last_health_code {
                last_health_code = cur_code;
                let condition = crate::ha_discovery::map_health_to_running_condition(cur_health);
                let _ = client.publish(
                    &cfg.condition_topic,
                    QoS::AtMostOnce,
                    true,
                    condition.as_bytes().to_vec(),
                );
            }

            match event {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    reconnect_delay_ms = 500; // reset backoff on successful connect
                    // Re-subscribe on every (re)connect so subscriptions
                    // survive a broker restart. The command topics are
                    // subscribed only where the commands they carry can be
                    // served; Home Assistant's own restart announcement is
                    // subscribed either way, because re-publishing the entities
                    // after HA restarts is presence, not a command.
                    if control.is_some() {
                        let _ = client.subscribe(CORRECTION_COMMAND_TOPIC, QoS::AtLeastOnce);
                        let _ = client.subscribe(CONTROL_COMMAND_TOPIC, QoS::AtLeastOnce);
                    }
                    let _ = client.subscribe(HOME_ASSISTANT_STATUS_TOPIC, QoS::AtMostOnce);
                    // Re-publish retained discovery and availability so HA entities
                    // reappear after a broker restart without requiring a Vigil restart.
                    // Use the current live health for the condition.
                    let (conn_health, _) = cfg.health.snapshot();
                    let conn_condition =
                        crate::ha_discovery::map_health_to_running_condition(conn_health);
                    re_announce_to_broker(
                        &client,
                        &cfg.discovery_payloads,
                        &cfg.availability_topic,
                        &cfg.condition_topic,
                        conn_condition,
                        &cfg.service_id,
                        control.as_deref(),
                    );
                }
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    // In MQTT 5, Publish.topic is Bytes, not String.
                    let topic = std::str::from_utf8(&p.topic).unwrap_or("");
                    match topic {
                        HOME_ASSISTANT_STATUS_TOPIC => {
                            // HA restarted — re-publish discovery so HA re-registers all
                            // Vigil entities immediately without waiting for the next retain flush.
                            if p.payload.as_ref() == b"online" {
                                let (ha_health, _) = cfg.health.snapshot();
                                let ha_condition =
                                    crate::ha_discovery::map_health_to_running_condition(ha_health);
                                re_announce_to_broker(
                                    &client,
                                    &cfg.discovery_payloads,
                                    &cfg.availability_topic,
                                    &cfg.condition_topic,
                                    ha_condition,
                                    &cfg.service_id,
                                    control.as_deref(),
                                );
                            }
                        }
                        CORRECTION_COMMAND_TOPIC => {
                            if let Some(control) = control.as_ref()
                                && let Ok(msg) =
                                    serde_json::from_slice::<CommandTopicMessage>(&p.payload)
                                && let Ok(req) = parse_command_topic(&msg)
                            {
                                match control.submit_correction(req) {
                                    Ok(()) => {}
                                    Err(SubmitCorrectionError::QueueFull) => {
                                        let n = overflow_clone.fetch_add(1, Ordering::SeqCst) + 1;
                                        println!("mqtt_correction_command_overflow=true total={n}");
                                    }
                                    Err(SubmitCorrectionError::Disconnected) => break,
                                }
                            }
                        }
                        CONTROL_COMMAND_TOPIC => {
                            if let Some(control) = control.as_ref() {
                                handle_production_control(
                                    &p.payload,
                                    &cfg.service_id,
                                    control.as_ref(),
                                    &client,
                                );
                            }
                        }
                        _ => {}
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    // Connection error — brief backoff before rumqttc retries.
                    thread::sleep(Duration::from_millis(reconnect_delay_ms));
                    reconnect_delay_ms = (reconnect_delay_ms * 2).min(30_000);
                }
            }
        }
    });

    SubscriberHandle {
        overflow_count,
        shutdown,
        threads: vec![thread],
    }
}

// ── Detection event publisher ──────────────────────────────────────────────

/// Publish a detection event JSON payload to the given broker topic.
///
/// Connects, publishes exactly one message at QoS 0, then disconnects.
/// Returns when the message has been handed to the MQTT library.
pub fn publish_detection_event(
    config: &MqttConfig,
    event_json: &str,
    topic: &str,
) -> Result<(), String> {
    let opts = make_mqtt_options(config, "ve");
    let (client, mut connection) = Client::new(opts, 16);

    wait_for_connack(&mut connection, Duration::from_secs(10))?;

    client
        .publish(
            topic,
            QoS::AtMostOnce,
            false,
            event_json.as_bytes().to_vec(),
        )
        .map_err(|e| format!("publish detection event: {e}"))?;

    // Brief drain to let the publish flush before disconnect.
    let _ = connection.recv_timeout(Duration::from_millis(300));

    let _ = client.disconnect();
    Ok(())
}

// ── Availability publisher ─────────────────────────────────────────────────

/// Publish the "online" availability payload to the given topic as a
/// retained message.  Used during startup after a successful MQTT connect.
pub fn publish_availability_online(
    config: &MqttConfig,
    availability_topic: &str,
) -> Result<(), String> {
    let opts = make_mqtt_options(config, "va");
    let (client, mut connection) = Client::new(opts, 16);

    wait_for_connack(&mut connection, Duration::from_secs(5))?;

    client
        .publish(
            availability_topic,
            QoS::AtMostOnce,
            true,
            b"online" as &[u8],
        )
        .map_err(|e| format!("publish availability: {e}"))?;

    let _ = connection.recv_timeout(Duration::from_millis(200));
    let _ = client.disconnect();
    Ok(())
}

// ── Long-lived detection event publisher ──────────────────────────────────

const DETECTION_PUBLISH_CHANNEL_CAPACITY: usize = 32;

/// How long after the last detection a camera's motion binary_sensor stays ON.
const MOTION_ACTIVE_WINDOW: Duration = Duration::from_secs(30);

/// Messages sent to the background publisher thread.
enum PublisherMsg {
    /// Detection event payload: publish to `topic` with QoS 0, retain=false.
    Detection(String, Vec<u8>),
    /// Motion-active signal for a camera: publish "ON" retained to `active_topic`,
    /// then publish "OFF" retained after `MOTION_ACTIVE_WINDOW` of no further signals.
    MotionActive(String),
}

/// Long-lived outbound detection publisher.
///
/// The detector thread calls `try_publish` / `notify_active` (non-blocking) —
/// they never open a connection or block on ConnAck.  A dedicated background
/// thread owns a single persistent MQTT client and drains the channel.
///
/// On detection channel overflow: increments `overflow_count`, logs loudly,
/// and flips health to `KeepPaceFailed`.
/// On motion-active overflow: silently drops (the ON is best-effort retained;
/// the next detection will re-trigger it).
pub struct DetectionPublisher {
    tx: mpsc::SyncSender<PublisherMsg>,
    /// Counts detection publishes dropped due to channel overflow.
    pub overflow_count: Arc<AtomicUsize>,
    health: vigil::HealthState,
}

impl DetectionPublisher {
    /// Non-blocking send of a detection event payload.  Never blocks.
    pub fn try_publish(&self, topic: String, json: String) {
        match self
            .tx
            .try_send(PublisherMsg::Detection(topic, json.into_bytes()))
        {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                let n = self.overflow_count.fetch_add(1, Ordering::SeqCst) + 1;
                println!("mqtt_detection_publish_overflow=true total={n}");
                self.health.set(
                    vigil::HealthStatus::KeepPaceFailed,
                    "mqtt outbound channel overflow",
                );
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                println!("mqtt_detection_publisher_disconnected=true");
            }
        }
    }

    /// Non-blocking signal that a detection occurred for the camera whose
    /// `active_topic` is `vigil/{service_id}/{camera_id}/active`.
    ///
    /// The background thread publishes "ON" retained immediately and schedules
    /// an "OFF" retained after `MOTION_ACTIVE_WINDOW` of no further signals.
    /// Overflow is silently dropped — the retained "ON" from the next detection
    /// will re-arm the sensor.
    pub fn notify_active(&self, active_topic: String) {
        match self.tx.try_send(PublisherMsg::MotionActive(active_topic)) {
            Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
            Err(mpsc::TrySendError::Disconnected(_)) => {
                println!("mqtt_detection_publisher_disconnected=true");
            }
        }
    }
}

/// Handle for the background detection publisher thread.
pub struct DetectionPublisherHandle {
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl DetectionPublisherHandle {
    pub fn shutdown_and_join(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

/// Spawn a long-lived detection publisher.
///
/// Returns `(Arc<DetectionPublisher>, DetectionPublisherHandle)`.
/// The publisher is cheaply cloneable via Arc.  Drop or join the handle
/// after camera threads have exited (so all senders are gone).
pub fn spawn_detection_publisher(
    config: &MqttConfig,
    health: vigil::HealthState,
) -> (Arc<DetectionPublisher>, DetectionPublisherHandle) {
    let (tx, rx) = mpsc::sync_channel::<PublisherMsg>(DETECTION_PUBLISH_CHANNEL_CAPACITY);
    let shutdown = Arc::new(AtomicBool::new(false));

    let publisher = Arc::new(DetectionPublisher {
        tx,
        overflow_count: Arc::new(AtomicUsize::new(0)),
        health,
    });

    let config = config.clone();
    let shutdown_clone = Arc::clone(&shutdown);

    let thread = thread::spawn(move || {
        let opts = make_mqtt_options(&config, "vp");
        let (client, mut connection) = Client::new(opts, 64);
        let mut connected = false;
        let mut reconnect_delay_ms: u64 = 500;
        // topic → time of last MotionActive signal; publisher publishes OFF after
        // MOTION_ACTIVE_WINDOW of no further signals.
        let mut last_active: HashMap<String, Instant> = HashMap::new();

        loop {
            if shutdown_clone.load(Ordering::SeqCst) {
                break;
            }

            // Drive the event loop to maintain the connection.
            match connection.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(Event::Incoming(Packet::ConnAck(_)))) => {
                    connected = true;
                    reconnect_delay_ms = 500;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) => {
                    connected = false;
                    thread::sleep(Duration::from_millis(reconnect_delay_ms));
                    reconnect_delay_ms = (reconnect_delay_ms * 2).min(30_000);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            // Drain pending messages and manage motion OFF timer while connected.
            if connected {
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        PublisherMsg::Detection(topic, payload) => {
                            if client
                                .publish(&topic, QoS::AtMostOnce, false, payload)
                                .is_err()
                            {
                                // Outbound queue full; event lost but publisher stays alive.
                                println!("mqtt_detection_publisher_client_full=true");
                            }
                        }
                        PublisherMsg::MotionActive(topic) => {
                            // Publish ON retained immediately, then arm the OFF timer.
                            let _ = client.publish(
                                &topic,
                                QoS::AtMostOnce,
                                true,
                                "ON".as_bytes().to_vec(),
                            );
                            last_active.insert(topic, Instant::now());
                        }
                    }
                }
                // Publish OFF retained for any camera that has been quiet for
                // MOTION_ACTIVE_WINDOW.
                let now = Instant::now();
                last_active.retain(|topic, last_at| {
                    if now.duration_since(*last_at) > MOTION_ACTIVE_WINDOW {
                        let _ =
                            client.publish(topic, QoS::AtMostOnce, true, "OFF".as_bytes().to_vec());
                        false // remove from map
                    } else {
                        true // keep watching
                    }
                });
            }
        }

        // Drain remaining detection events before exiting (best-effort flush).
        // MotionActive signals are skipped — broker retains the last ON/OFF state.
        let flush_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < flush_deadline {
            if let Ok(PublisherMsg::Detection(topic, payload)) = rx.try_recv() {
                let _ = client.publish(&topic, QoS::AtMostOnce, false, payload);
            }
            match connection.recv_timeout(Duration::from_millis(20)) {
                Ok(Ok(_)) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
                Ok(Err(_)) => break,
            }
        }
        let _ = client.disconnect();
    });

    (
        publisher,
        DetectionPublisherHandle {
            shutdown,
            thread: Some(thread),
        },
    )
}
