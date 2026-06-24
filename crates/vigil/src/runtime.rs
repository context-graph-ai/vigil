use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use context_graph::{
    AuditFilter, AuditTarget, CreateContext, CreateDecision, CreateEntity, CreateIntention,
    EntityPatch, EntityType, EvidenceKind, EvidenceProducer, EvidenceRef, IntentionOrigin,
    IntentionStatus, ListEntityFilter, ObservationId, RecordObservation, RetentionStatus, Store,
    start_control_listener,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config;
use crate::health::{HealthServer, HealthState, HealthStatus};
use crate::live_read;
use crate::media_pipeline;
use crate::privilege;
use crate::runtime_stats::RuntimeStatsState;
use crate::shutdown;
use crate::store;
use crate::yolox_detector;

const DETECTOR_QUEUE_CAPACITY: usize = 4;
const DETECTOR_INPUT_WIDTH: u32 = 640;
const DETECTOR_INPUT_HEIGHT: u32 = 640;
static CAPTURE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(crate) fn run(args: Vec<OsString>) -> ExitCode {
    match run_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

pub(crate) fn run_detector_probe(args: Vec<OsString>) -> ExitCode {
    match run_detector_probe_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

fn run_detector_probe_inner(args: Vec<OsString>) -> Result<(), String> {
    yolox_detector::run_detector_probe(args)
}

fn run_inner(args: Vec<OsString>) -> Result<(), String> {
    let config = config::load(args)?;
    // Install the bundled Lovelace card into HA's config/www BEFORE dropping to the
    // runtime uid: /homeassistant is root-owned, so the file copy must run as root.
    install_lovelace_card();
    privilege::prepare_runtime_user(&config.store_path)?;
    let mut shutdown = shutdown::install()?;
    let shutdown_flag = shutdown.flag();
    let health = HealthState::new();
    let server = HealthServer::listen(config.health_port, health.clone(), shutdown_flag.clone())?;
    let stats = RuntimeStatsState::new(&config.data_dir);
    stats.update(|stats| {
        stats.health = "ready".to_string();
        stats.processing_lag_bound_ms = 1.0;
    });

    log_startup(&config);

    let mut control = None;
    let mut camera_handles: Vec<JoinHandle<()>> = Vec::new();
    let mut mqtt_subscriber: Option<crate::ha_mqtt_tasks::SubscriberHandle> = None;
    let mut detection_publisher: Option<Arc<crate::ha_mqtt_tasks::DetectionPublisher>> = None;
    let mut detection_publisher_handle: Option<crate::ha_mqtt_tasks::DetectionPublisherHandle> =
        None;
    let store = match store::open(&config.store_path) {
        Ok(store) => {
            let state = if store.created {
                "store created"
            } else {
                "existing store"
            };
            println!("{state} path={}", store.path.display());
            println!("store opened path={}", store.path.display());
            println!("{}", store.trace);
            println!("runtime loop ready");
            health.set(HealthStatus::Ready, "store open and runtime loop ready");
            let owner_store = store.handle.clone();
            let owner_stats = stats.clone();
            let read_handler: context_graph::ControlHandler = Arc::new(move |request: String| {
                let stats = owner_stats.snapshot();
                live_read::handle_owner_request(&owner_store, &stats, &request)
            });
            let control_socket_path = crate::control_socket::control_socket_path(&config.data_dir);
            control =
                start_control_listener(&control_socket_path, shutdown_flag.clone(), read_handler);

            // ── MQTT detection publisher — spawned before camera threads ──────
            // The publisher owns one persistent MQTT connection for all detection
            // events.  It must be created before camera threads start so they capture
            // a live Arc rather than None.
            if crate::ha_mqtt_tasks::mqtt_connect_intent(config.mqtt.as_ref())
                && let Some(ref mqtt_cfg) = config.mqtt
            {
                let (pub_arc, pub_handle) =
                    crate::ha_mqtt_tasks::spawn_detection_publisher(mqtt_cfg, health.clone());
                detection_publisher = Some(pub_arc);
                detection_publisher_handle = Some(pub_handle);
                println!("mqtt_detection_publisher_started=true");
            }

            // ── Multi-camera fan-out ───────────────────────────────────────
            // Build per-camera enabled flags (checked against startup disable markers).
            let mut camera_flags: std::collections::BTreeMap<String, Arc<AtomicBool>> =
                std::collections::BTreeMap::new();

            for camera in &config.cameras {
                let cam_id = camera_slug(&camera.name);
                let disable_marker = config.data_dir.join("camera-disabled").join(&cam_id);
                let is_disabled = disable_marker.exists();
                let enabled = Arc::new(AtomicBool::new(!is_disabled));
                camera_flags.insert(cam_id.clone(), Arc::clone(&enabled));

                if let Some(url) = &camera.rtsp_url {
                    let memory_url = media_pipeline::redact_rtsp_url(url);
                    // Clone config and patch per-camera fields so existing sub-functions
                    // (maintain_runtime_memory, record_detected_events) see the right camera.
                    let mut cam_config = config.clone();
                    cam_config.camera_name = camera.name.clone();
                    cam_config.rtsp_url = Some(url.clone());
                    cam_config.rtsp_username = camera.username.clone();
                    cam_config.rtsp_password = camera.password.clone();

                    match maintain_runtime_memory(&store.handle, &cam_config, &memory_url) {
                        Ok(_) => println!("runtime_memory_ready camera={cam_id}"),
                        Err(error) => {
                            println!("runtime_memory_setup_failed camera={cam_id} error={error}")
                        }
                    }

                    // Create a Generic Camera config entry in HA pointing at the
                    // camera's RTSP directly. HA Core serves it over WebRTC via its
                    // built-in go2rtc (2024.11+) with no separate stream registration.
                    // The MQTT camera platform is image-only, so this is the live entity.
                    register_generic_camera(&cam_id, url, &config.data_dir);

                    if is_disabled {
                        println!("camera_disabled_at_startup camera={cam_id}");
                    }

                    let handle = start_rtsp_probe(
                        url.clone(),
                        cam_config,
                        store.handle.clone(),
                        stats.clone(),
                        health.clone(),
                        shutdown_flag.clone(),
                        Arc::clone(&enabled),
                        detection_publisher.clone(),
                    );
                    camera_handles.push(handle);
                }
            }

            // Wire the MQTT subscriber (needs camera_flags which is now fully built).
            // Gate on the same predicate used above so the subscriber and publisher
            // are always either both present or both absent.
            if crate::ha_mqtt_tasks::mqtt_connect_intent(config.mqtt.as_ref())
                && let Some(ref mqtt_cfg) = config.mqtt
            {
                let svc = service_config_from_runtime(&config);
                let payloads = crate::ha_discovery::generate_discovery_payloads(&svc);
                match crate::ha_mqtt_tasks::publish_discovery_to_broker(mqtt_cfg, &payloads) {
                    Ok(()) => println!("mqtt_discovery_published=true"),
                    Err(e) => println!("mqtt_discovery_error={e}"),
                }
                let avail_topic = format!("vigil/{}/availability", config.service_id);
                match crate::ha_mqtt_tasks::publish_availability_online(mqtt_cfg, &avail_topic) {
                    Ok(()) => println!("mqtt_availability_online=true"),
                    Err(e) => println!("mqtt_availability_error={e}"),
                }
                let condition_topic =
                    crate::ha_discovery::running_condition_topic(&config.service_id);
                let overflow = Arc::new(AtomicUsize::new(0));
                // Derive a stable client id from the service_id so the broker
                // can correlate last-will across restarts.
                let client_id = format!("vigil-{}-sub", slug_for_id(&config.service_id));
                let sub_cfg = crate::ha_mqtt_tasks::WiredSubscriberConfig {
                    mqtt: mqtt_cfg.clone(),
                    client_id,
                    availability_topic: avail_topic,
                    condition_topic,
                    discovery_payloads: payloads,
                    // Pass live health so the subscriber can publish condition updates.
                    health: health.clone(),
                };
                // Bounded correction channel: the MQTT subscriber thread forwards
                // correction commands here; the worker thread drains and writes to cg.
                let (correction_tx, correction_rx) =
                    std::sync::mpsc::sync_channel::<crate::correction::CorrectionRequest>(64);
                let store_worker = Arc::new(store.handle.clone());
                thread::spawn(move || {
                    for req in correction_rx {
                        let _ = crate::correction::record_correction(&store_worker, req);
                    }
                });
                let subscriber = crate::ha_mqtt_tasks::spawn_production_subscriber(
                    sub_cfg,
                    Arc::new(store.handle.clone()),
                    camera_flags,
                    correction_tx,
                    overflow,
                );
                mqtt_subscriber = Some(subscriber);
                println!("mqtt_subscriber_started=true");
            }
            Some(store.handle)
        }
        Err(error) => {
            println!(
                "store open error path={} error={}",
                config.store_path.display(),
                error
            );
            health.set(HealthStatus::StoreOpenFailed, "store open failed");
            None
        }
    };

    shutdown.wait();

    // Teardown order — store handle drops LAST:
    //  1. MQTT subscriber (holds Arc<Store>; signals shutdown, blocks until thread exits)
    //  2. Detection publisher (camera threads must exit first so all senders are gone)
    //  3. Camera probe threads (each holds a Store clone via Arc)
    //  4. Control listener (read_handler captures a Store clone)
    //  5. Health server (no Store reference — safe to join before or after store)
    //  6. drop(store) — all other Store holders are now joined and their clones dropped
    if let Some(sub) = mqtt_subscriber {
        sub.shutdown_and_join();
        println!("mqtt_subscriber_stopped=true");
    }
    // Camera handles exit first so their DetectionPublisher senders are all dropped.
    for handle in camera_handles {
        let _ = handle.join();
    }
    if let Some(pub_handle) = detection_publisher_handle {
        pub_handle.shutdown_and_join();
        println!("mqtt_detection_publisher_stopped=true");
    }
    if let Some(handle) = control.take() {
        let _ = handle.join();
    }
    server.join();
    drop(store);
    Ok(())
}

/// Derive a stable lowercase slug for use as a stable MQTT client id.
fn slug_for_id(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Register each camera as an HA Generic Camera config entry via the Core config-flow API.
///
/// The MQTT camera platform is image-only (no `stream_source` key), so live video requires
/// a real streaming camera entity.  This creates one Generic Camera per camera, pointed at
/// the camera's own RTSP URL; HA Core reaches the camera directly and serves the entity over
/// WebRTC via its built-in go2rtc (2024.11+) with no separate stream registration.
///
/// Flow:
/// 1. Sentinel guard — if `data_dir/generic_camera_<slug>.registered` exists, skip.
/// 2. POST /core/api/config/config_entries/flow `{"handler":"generic"}` → flow_id.
/// 3. POST .../flow/<flow_id> with `stream_source` + the required `advanced` object
///    (`framerate` / `verify_ssl` / `rtsp_transport:"tcp"`).
/// 4. If HA returns an intermediate "form" step (confirm/preview), submit `{}`.
/// 5. On `"type":"create_entry"`, write the sentinel and log success.
///
/// HA version floor: 2024.11 (built-in go2rtc + WebRTC).  Below that, the entity falls back
/// to HLS (laggier but functional).  H.265 sources register but don't negotiate WebRTC.
///
/// Failures are logged and never panic; the add-on continues without live view.
fn register_generic_camera(cam_slug: &str, rtsp_url: &str, data_dir: &std::path::Path) {
    // ── Sentinel-based idempotency ─────────────────────────────────────────
    // The Generic Camera config-flow API is NOT inherently idempotent — calling it
    // twice creates duplicate camera entities.  Write a marker the first time we
    // succeed so subsequent add-on restarts skip the API call entirely.
    let sentinel = data_dir.join(format!("generic_camera_{cam_slug}.registered"));
    if sentinel.exists() {
        println!("generic_camera_already_registered camera={cam_slug}");
        return;
    }

    let Ok(token) = std::env::var("SUPERVISOR_TOKEN") else {
        println!("generic_camera_skip_no_supervisor_token camera={cam_slug}");
        return;
    };

    // ── Start config-flow ─────────────────────────────────────────────────
    let start_payload = crate::supervisor::build_generic_camera_flow_start_payload();
    let flow_resp = match crate::supervisor::supervisor_post_body(
        "http://supervisor/core/api/config/config_entries/flow",
        &token,
        &start_payload,
    ) {
        Ok(resp) => resp,
        Err(e) => {
            println!("generic_camera_flow_start_error camera={cam_slug} error={e}");
            return;
        }
    };

    let flow_id = match crate::supervisor::parse_flow_id(&flow_resp) {
        Some(id) => id,
        None => {
            println!(
                "generic_camera_flow_id_missing camera={cam_slug} response={}",
                &flow_resp[..flow_resp.len().min(200)]
            );
            return;
        }
    };

    // ── Submit stream_source user step ────────────────────────────────────
    // stream_source is the camera's own RTSP URL, reached directly by HA Core;
    // HA serves WebRTC via its built-in go2rtc with no separate registration.
    let step_payload = crate::supervisor::build_generic_camera_flow_step_payload(rtsp_url, None);
    let step_url = format!("http://supervisor/core/api/config/config_entries/flow/{flow_id}");
    let step_resp = match crate::supervisor::supervisor_post_body(&step_url, &token, &step_payload)
    {
        Ok(resp) => resp,
        Err(e) => {
            println!("generic_camera_flow_step_error camera={cam_slug} error={e}");
            return;
        }
    };

    // ── Handle optional confirm/preview intermediate step ─────────────────
    // Some HA versions present an extra "form" step (preview, confirm, or verify)
    // before completing the entry.  Submit an empty body to accept defaults.
    let final_resp = if crate::supervisor::is_flow_create_entry(&step_resp) {
        step_resp
    } else {
        match crate::supervisor::supervisor_post_body(&step_url, &token, "{}") {
            Ok(resp) => resp,
            Err(e) => {
                println!("generic_camera_flow_confirm_error camera={cam_slug} error={e}");
                return;
            }
        }
    };

    if crate::supervisor::is_flow_create_entry(&final_resp) {
        // Write sentinel so we skip on next restart.
        if let Err(e) = std::fs::write(&sentinel, b"ok") {
            println!("generic_camera_sentinel_write_error camera={cam_slug} error={e}");
        }
        println!("generic_camera_registered camera={cam_slug}");
    } else {
        println!(
            "generic_camera_flow_unexpected_result camera={cam_slug} response={}",
            &final_resp[..final_resp.len().min(300)]
        );
    }
}

/// Copy the bundled Lovelace card to HA's config/www/ and register it as a
/// Lovelace module resource.  The card file is bundled into the add-on image
/// at `/www/vigil-event-gallery-card.js` by the Dockerfile COPY instruction.
///
/// The `homeassistant_config:rw` map entry in config.yaml mounts HA's config
/// directory at `/homeassistant` (current Supervisor schema — NOT the legacy
/// `/config` path).  Writing to `/homeassistant/www/` places the file in HA's
/// real config/www/, which HA serves as /local/.
fn install_lovelace_card() {
    let card_src = std::path::Path::new("/www/vigil-event-gallery-card.js");
    if !card_src.exists() {
        println!("lovelace_card_src_missing path={}", card_src.display());
        return;
    }
    // /homeassistant is where Supervisor mounts `homeassistant_config:rw`.
    let _ = std::fs::create_dir_all("/homeassistant/www");
    let dst = "/homeassistant/www/vigil-event-gallery-card.js";
    if let Err(e) = std::fs::copy(card_src, dst) {
        println!("lovelace_card_copy_error={e}");
        return;
    }
    println!("lovelace_card_installed=true");
    register_lovelace_resource();
}

/// Register the Vigil gallery card as a Lovelace frontend module.
///
/// HA has no REST endpoint for Lovelace resource management — it is
/// WebSocket-only.  This function connects to `ws://supervisor/core/websocket`,
/// authenticates with SUPERVISOR_TOKEN, lists existing resources to skip
/// re-registration (idempotent), and creates the module entry if absent.
/// Failures are surfaced loudly (printed) rather than silently swallowed.
fn register_lovelace_resource() {
    let Ok(token) = std::env::var("SUPERVISOR_TOKEN") else {
        println!("lovelace_skip_no_supervisor_token");
        return;
    };
    match lovelace_register_ws(&token) {
        Ok(true) => println!("lovelace_resource_registered=true"),
        Ok(false) => println!("lovelace_resource_already_registered=true"),
        Err(e) => println!("lovelace_resource_register_error={e}"),
    }
}

/// Perform the HA WebSocket auth + `lovelace/resources` list + optional
/// `lovelace/resources/create` sequence.
///
/// Returns `Ok(true)` when the resource was newly registered,
/// `Ok(false)` when it was already present, `Err(...)` on any failure.
fn lovelace_register_ws(token: &str) -> Result<bool, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;

    // Connect to the Supervisor's HA Core WebSocket proxy (HTTP, not HTTPS —
    // internal Supervisor network).
    let stream = TcpStream::connect("supervisor:80").map_err(|e| format!("ws_connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("ws_timeout: {e}"))?;

    // Clone for writing; BufReader wraps the original for buffered reads.
    let mut writer = stream.try_clone().map_err(|e| format!("ws_clone: {e}"))?;
    let mut reader = BufReader::new(stream);

    // ── HTTP Upgrade ──────────────────────────────────────────────────────
    // A static nonce is fine — this connection is non-adversarial loopback.
    let upgrade = concat!(
        "GET /core/websocket HTTP/1.1\r\n",
        "Host: supervisor\r\n",
        "Upgrade: websocket\r\n",
        "Connection: Upgrade\r\n",
        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
        "Sec-WebSocket-Version: 13\r\n",
        "\r\n"
    );
    writer
        .write_all(upgrade.as_bytes())
        .map_err(|e| format!("ws_write_upgrade: {e}"))?;

    // Read HTTP response headers (line-by-line; BufReader buffers correctly
    // even if the server sends the first WS frame in the same TCP segment).
    let mut got_101 = false;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("ws_read_header: {e}"))?;
        if line.is_empty() {
            return Err("ws_handshake: connection closed during headers".to_string());
        }
        if line.contains("101") {
            got_101 = true;
        }
        if line == "\r\n" {
            break;
        }
    }
    if !got_101 {
        return Err(
            "ws_handshake: server did not respond with 101 Switching Protocols".to_string(),
        );
    }

    // ── HA auth sequence ──────────────────────────────────────────────────
    let auth_required = ws_recv_text(&mut reader)?;
    if !auth_required.contains("auth_required") {
        return Err(format!(
            "ws_auth: expected auth_required, got: {}",
            &auth_required[..auth_required.len().min(80)]
        ));
    }

    ws_send_text(
        &mut writer,
        &format!(r#"{{"type":"auth","access_token":"{token}"}}"#),
    )?;

    let auth_result = ws_recv_text(&mut reader)?;
    if !auth_result.contains("auth_ok") {
        return Err(format!(
            "ws_auth: authentication failed: {}",
            &auth_result[..auth_result.len().min(80)]
        ));
    }

    // ── List existing resources (idempotent guard) ────────────────────────
    ws_send_text(&mut writer, r#"{"id":1,"type":"lovelace/resources"}"#)?;
    let list_resp = ws_recv_text(&mut reader)?;

    let card_url = "/local/vigil-event-gallery-card.js";
    if list_resp.contains(card_url) {
        return Ok(false); // already registered — nothing to do
    }

    // ── Register as a module resource ─────────────────────────────────────
    ws_send_text(
        &mut writer,
        &format!(
            r#"{{"id":2,"type":"lovelace/resources/create","res_type":"module","url":"{card_url}"}}"#
        ),
    )?;
    let create_resp = ws_recv_text(&mut reader)?;
    if !create_resp.contains(r#""success":true"#) {
        return Err(format!(
            "ws_create: lovelace/resources/create failed: {}",
            &create_resp[..create_resp.len().min(120)]
        ));
    }

    Ok(true)
}

/// Send a masked WebSocket text frame (client → server, per RFC 6455 §5.3).
fn ws_send_text(writer: &mut impl std::io::Write, text: &str) -> Result<(), String> {
    let payload = text.as_bytes();
    let len = payload.len();
    // Fixed mask is fine for a non-adversarial loopback connection.
    let mask: [u8; 4] = [0x17, 0x42, 0x9c, 0xfe];

    let mut frame = Vec::with_capacity(len + 10);
    frame.push(0x81); // FIN=1, opcode=0x1 (text)
    if len < 126 {
        frame.push(0x80 | len as u8); // MASK bit set, 7-bit length
    } else if len < 65536 {
        frame.push(0x80 | 126u8);
        frame.push((len >> 8) as u8);
        frame.push((len & 0xff) as u8);
    } else {
        return Err(format!("ws_send_text: payload too large ({len} bytes)"));
    }
    frame.extend_from_slice(&mask);
    for (i, &b) in payload.iter().enumerate() {
        frame.push(b ^ mask[i % 4]);
    }
    writer
        .write_all(&frame)
        .map_err(|e| format!("ws_send_text_io: {e}"))
}

/// Receive one WebSocket text frame from the server (server → client,
/// unmasked).  Skips over ping frames (opcode 0x9).
fn ws_recv_text(reader: &mut impl std::io::BufRead) -> Result<String, String> {
    loop {
        let mut hdr = [0u8; 2];
        reader
            .read_exact(&mut hdr)
            .map_err(|e| format!("ws_recv_hdr: {e}"))?;

        let opcode = hdr[0] & 0x0f;
        let is_masked = (hdr[1] & 0x80) != 0;
        let len_byte = hdr[1] & 0x7f;

        let payload_len: usize = if len_byte < 126 {
            len_byte as usize
        } else if len_byte == 126 {
            let mut b = [0u8; 2];
            reader
                .read_exact(&mut b)
                .map_err(|e| format!("ws_recv_len16: {e}"))?;
            u16::from_be_bytes(b) as usize
        } else {
            let mut b = [0u8; 8];
            reader
                .read_exact(&mut b)
                .map_err(|e| format!("ws_recv_len64: {e}"))?;
            usize::try_from(u64::from_be_bytes(b)).unwrap_or(usize::MAX)
        };

        if payload_len > 65_536 {
            return Err(format!(
                "ws_recv_text: frame too large ({payload_len} bytes)"
            ));
        }

        // Servers SHOULD NOT mask frames (RFC 6455 §5.1), but handle it defensively.
        let mask_bytes = if is_masked {
            let mut m = [0u8; 4];
            reader
                .read_exact(&mut m)
                .map_err(|e| format!("ws_recv_mask: {e}"))?;
            Some(m)
        } else {
            None
        };

        let mut payload = vec![0u8; payload_len];
        reader
            .read_exact(&mut payload)
            .map_err(|e| format!("ws_recv_payload: {e}"))?;

        if let Some(mask) = mask_bytes {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }

        match opcode {
            0x8 => return Err("ws_recv_text: server sent close frame".to_string()),
            0x9 => continue, // ping — skip (session is too short to need pong)
            0x1 | 0x0 => {
                return String::from_utf8(payload).map_err(|e| format!("ws_recv_utf8: {e}"));
            }
            _ => continue, // unknown control frame — skip
        }
    }
}

fn log_startup(config: &config::RuntimeConfig) {
    println!(
        "vigil version={} startup_epoch={}",
        env!("CARGO_PKG_VERSION"),
        startup_epoch()
    );
    println!("data_dir={}", display(&config.data_dir));
    println!("store_path={}", display(&config.store_path));
    println!("health_port={}", config.health_port);
    if let Some(rtsp_url) = config.rtsp_url.as_ref() {
        println!("rtsp_url={}", media_pipeline::redact_rtsp_url(rtsp_url));
    }
    println!("site_name={}", config.site_name);
    println!("service_id={}", config.service_id);
    println!("camera_name={}", config.camera_name);
    println!("detector_model_id={}", config.detector_model_id);
    println!(
        "detector_confidence_threshold={}",
        config.detector_confidence_threshold
    );
    println!("detector_sample_frames={}", config.detector_sample_frames);
    if let Some(model_path) = config.detector_model_path.as_ref() {
        println!("detector_model_path={}", display(model_path));
    }
    if let Some(mqtt) = config.mqtt.as_ref() {
        println!("mqtt_host={}", mqtt.broker_host);
        println!("mqtt_port={}", mqtt.broker_port);
        println!(
            "mqtt_username={}",
            mqtt.username.as_deref().unwrap_or("<none>")
        );
    } else {
        println!("mqtt=disabled");
    }
}

fn startup_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

#[allow(clippy::too_many_arguments)]
fn start_rtsp_probe(
    rtsp_url: String,
    config: config::RuntimeConfig,
    store: Store,
    stats: RuntimeStatsState,
    health: HealthState,
    shutdown: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    detection_publisher: Option<Arc<crate::ha_mqtt_tasks::DetectionPublisher>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        // Wait until enabled (respects startup disable marker).
        while !shutdown.load(Ordering::SeqCst) && !enabled.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(5));
        }
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let rtsp_source = match media_pipeline::prepare_rtsp_source(
            &rtsp_url,
            config.rtsp_username.as_deref(),
            config.rtsp_password.as_deref(),
        ) {
            Ok(source) => source,
            Err(error) => {
                println!(
                    "rtsp probe failed url={} error={error}",
                    media_pipeline::redact_rtsp_url(&rtsp_url)
                );
                stats.update(|stats| {
                    stats.ingest_signal = "decode-error".to_string();
                    mark_health_condition(&mut stats.health, "ingest_failed");
                });
                health.set(HealthStatus::IngestFailed, "RTSP ingest failed");
                return;
            }
        };
        let rtsp_log_url = rtsp_source.session_url().to_string();
        println!("rtsp probe starting url={rtsp_log_url}");
        let detector = match yolox_detector::load_detector(config.detector_model_path.as_deref()) {
            Ok(detector) => {
                println!(
                    "detector model loaded id={} sha256={}",
                    config.detector_model_id, detector.model_sha256
                );
                Some(detector)
            }
            Err(error) => {
                println!("detector model load failed error={error}");
                stats.update(|stats| {
                    stats.ingest_signal = "detector-load-error".to_string();
                    mark_health_condition(&mut stats.health, "ingest_failed");
                });
                health.set(HealthStatus::IngestFailed, "detector model load failed");
                None
            }
        };
        let detector_queue_capacity = env_u64("VIGIL_DETECTOR_QUEUE_CAPACITY")
            .map(|capacity| capacity.max(1) as usize)
            .unwrap_or(DETECTOR_QUEUE_CAPACITY);
        let detector_work_delay =
            Duration::from_millis(env_u64("VIGIL_DETECTOR_WORK_DELAY_MS").unwrap_or_default());
        let (detector_tx, detector_rx) = sync_channel::<CapturedSegment>(detector_queue_capacity);
        let active_stream_generation = Arc::new(AtomicU64::new(0));
        let detector_handle = detector.map(|detector| {
            let config = config.clone();
            let store = store.clone();
            let stats = stats.clone();
            let health = health.clone();
            let shutdown = shutdown.clone();
            let active_stream_generation = active_stream_generation.clone();
            let detection_publisher = detection_publisher.clone();
            thread::spawn(move || {
                let mut detector_total = 0_u64;
                while !shutdown.load(Ordering::SeqCst) {
                    let segment = match detector_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(segment) => segment,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    };
                    if segment.stream_generation < active_stream_generation.load(Ordering::SeqCst) {
                        println!(
                            "stale_stream_segment_suppressed=true sequence={}",
                            segment.sequence
                        );
                        let _ = fs::remove_file(&segment.path);
                        continue;
                    }
                    detector_total = detector_total.saturating_add(1);
                    stats.update(|stats| {
                        stats.detector_invocations = stats.detector_invocations.saturating_add(1);
                    });
                    println!("detector_invocations={detector_total}");
                    sleep_shutdown_aware(&shutdown, detector_work_delay);
                    let detector_started = Instant::now();
                    let output = yolox_detector::detect_segment(
                        &detector,
                        &segment.media,
                        segment.clip_sha256.clone(),
                        config.detector_sample_frames,
                        config.detector_confidence_threshold,
                    );
                    let latency_ms = detector_started.elapsed().as_secs_f64() * 1000.0;
                    stats.update(|stats| {
                        stats.detector_latency_p50_ms = latency_ms;
                        stats.detector_latency_p95_ms = latency_ms;
                        stats.detector_latency_max_ms =
                            stats.detector_latency_max_ms.max(latency_ms);
                    });
                    match output {
                        Ok(output) => {
                            println!("detector_detections={}", output.detections.len());
                            if let Err(error) = record_detected_events(
                                &store,
                                &config,
                                &segment,
                                &output,
                                &stats,
                                &health,
                                detection_publisher.as_deref(),
                            ) {
                                println!("record_detection_failed error={error}");
                            }
                        }
                        Err(error) => {
                            println!("detector invocation failed error={error}");
                        }
                    }
                }
                for segment in detector_rx.try_iter() {
                    let _ = fs::remove_file(&segment.path);
                }
            })
        });
        let mut decoded_total = 0_u64;
        let mut reconnect_pending = false;
        let mut stream_generation = active_stream_generation.load(Ordering::SeqCst);
        let retry_initial_ms = env_u64("VIGIL_RTSP_RETRY_INITIAL_MS").unwrap_or(2_000);
        let retry_max_ms = env_u64("VIGIL_RTSP_RETRY_MAX_MS")
            .unwrap_or(30_000)
            .max(retry_initial_ms);
        let mut retry_delay_ms = retry_initial_ms;
        while !shutdown.load(Ordering::SeqCst) {
            // Per-camera disable: gate on the enabled flag without exiting the
            // thread so that a subsequent enable resumes ingest immediately.
            if !enabled.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(5));
                continue;
            }
            let capture_frames = env_u64("VIGIL_CAPTURE_FRAMES").unwrap_or(48).max(1) as usize;
            let capture_result = media_pipeline::capture_rtsp_segments(
                &rtsp_source,
                capture_frames,
                shutdown.clone(),
                || {
                    println!("rtsp opened url={rtsp_log_url}");
                    println!("rtsp play observed url={rtsp_log_url}");
                    Ok(())
                },
                |media| {
                    let segment = build_captured_segment(
                        media,
                        &config.data_dir,
                        &config.camera_name,
                        stream_generation,
                    )?;
                    let frames = segment.frames;
                    let fps = segment.fps;
                    let motion_positive = segment.motion_positive_frames;
                    decoded_total = decoded_total.saturating_add(frames);
                    stats.update(|stats| {
                        stats.frames_received = stats.frames_received.saturating_add(frames);
                        stats.motion_positive_frames =
                            stats.motion_positive_frames.saturating_add(motion_positive);
                        stats.stream_fps = fps;
                        stats.processing_lag_bound_ms = 1000.0_f64 / fps.max(1.0);
                        stats.ingest_signal = "ok".to_string();
                        if reconnect_pending {
                            stats.stream_reconnects = stats.stream_reconnects.saturating_add(1);
                        }
                    });
                    reconnect_pending = false;
                    retry_delay_ms = retry_initial_ms;
                    if motion_positive == 0 {
                        println!("motion_gate_suppressed_segment=true");
                        let _ = fs::remove_file(&segment.path);
                    } else if detector_handle.is_some() {
                        match detector_tx.try_send(segment) {
                            Ok(()) => {}
                            Err(TrySendError::Full(segment)) => {
                                let dropped = segment.motion_positive_frames.max(1);
                                stats.update(|stats| {
                                    stats.dropped_motion_positive_frames = stats
                                        .dropped_motion_positive_frames
                                        .saturating_add(dropped);
                                    stats.processing_lag_ms = stats
                                        .processing_lag_ms
                                        .max(stats.processing_lag_bound_ms + 1.0);
                                    mark_health_condition(&mut stats.health, "keep-pace-failed");
                                });
                                health.set(
                                    HealthStatus::KeepPaceFailed,
                                    "detector queue fell behind",
                                );
                                let _ = fs::remove_file(&segment.path);
                            }
                            Err(TrySendError::Disconnected(segment)) => {
                                println!("detector queue disconnected");
                                let _ = fs::remove_file(&segment.path);
                            }
                        }
                    } else {
                        println!("detector_unavailable_dropped_segment=true");
                        let _ = fs::remove_file(&segment.path);
                    }
                    println!("decoded_frames={decoded_total}");
                    Ok(())
                },
            );
            match capture_result {
                Ok(()) => {}
                Err(error) => {
                    if shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    println!("rtsp probe failed url={rtsp_log_url} error={error}");
                    println!("decoded_frames={decoded_total}");
                    stats.update(|stats| {
                        stats.stream_drops = stats.stream_drops.saturating_add(1);
                        stats.ingest_signal = "decode-error".to_string();
                        mark_health_condition(&mut stats.health, "ingest_failed");
                    });
                    reconnect_pending = true;
                    stream_generation = active_stream_generation.fetch_add(1, Ordering::SeqCst) + 1;
                    health.set(HealthStatus::IngestFailed, "RTSP ingest failed");
                    println!("rtsp_retry_after_ms={retry_delay_ms}");
                    sleep_shutdown_aware(&shutdown, Duration::from_millis(retry_delay_ms));
                    retry_delay_ms = retry_delay_ms.saturating_mul(2).min(retry_max_ms);
                }
            }
        }
        drop(detector_tx);
        if let Some(handle) = detector_handle {
            let _ = handle.join();
        }
    })
}

fn sleep_shutdown_aware(shutdown: &AtomicBool, duration: Duration) {
    let started = Instant::now();
    while !shutdown.load(Ordering::SeqCst) && started.elapsed() < duration {
        let remaining = duration.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

fn build_captured_segment(
    media: media_pipeline::DecodedVideoSegment,
    data_dir: &Path,
    camera_name: &str,
    stream_generation: u64,
) -> Result<CapturedSegment, String> {
    let staging_dir = data_dir.join("staging");
    let clip_dir = data_dir.join("clips");
    fs::create_dir_all(&clip_dir)
        .map_err(|error| format!("create clip dir {}: {error}", clip_dir.display()))?;
    let motion_positive_frames = media_pipeline::motion_gate(&media)
        .motion_positive_frames
        .min(media.frame_count());
    let observed_at = media.observed_at.unwrap_or_else(chrono::Utc::now);
    let stamp = startup_epoch();
    let sequence = CAPTURE_SEQUENCE.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or_default();
    let camera_slug = camera_slug(camera_name);
    let file_name = format!(
        "{camera_slug}-event-{stamp}-{sequence}-{nanos}.{}",
        media.codec.extension()
    );
    let staging_path = staging_dir.join(&file_name);
    let final_path = clip_dir.join(&file_name);
    let clip_sha256 = encoded_clip_sha256(&media);
    let decoded_frames_sha256 = decoded_frames_sha256(&media);
    let frames = media.frame_count();
    if frames == 0 {
        let _ = fs::remove_file(&staging_path);
        return Err(format!(
            "recorded RTSP segment {} has no decodable frames",
            staging_path.display()
        ));
    }
    Ok(CapturedSegment {
        path: staging_path,
        final_path,
        source_ref: format!("vigil-edge:clip/{file_name}"),
        sequence,
        stream_generation,
        frames,
        fps: media.fps,
        mime_type: media.codec.mime_type().to_string(),
        clip_sha256,
        decoded_frames_sha256,
        motion_positive_frames,
        observed_at,
        media,
    })
}

fn camera_slug(camera_name: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in camera_name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "camera".to_string()
    } else {
        slug
    }
}

/// Build a `ServiceConfig` for MQTT discovery from the loaded runtime config.
/// Uses the canonical `cameras` list so multi-camera configs are fully announced.
fn service_config_from_runtime(
    config: &config::RuntimeConfig,
) -> crate::ha_discovery::ServiceConfig {
    crate::ha_discovery::ServiceConfig {
        service_name: config.site_name.clone(),
        service_id: config.service_id.clone(),
        cameras: config
            .cameras
            .iter()
            .map(|c| crate::ha_discovery::CameraConfig {
                camera_id: camera_slug(&c.name),
                camera_label: c.name.clone(),
            })
            .collect(),
    }
}

fn finalize_clip(
    segment: &CapturedSegment,
    stats: &RuntimeStatsState,
    health: &HealthState,
) -> Result<(), String> {
    if let Err(error) = media_pipeline::write_encoded_clip(&segment.media, &segment.path) {
        return Err(clip_write_failure(
            stats,
            health,
            Some(&segment.path),
            format!("write staging clip {}: {error}", segment.path.display()),
        ));
    }
    if let Some(parent) = segment.final_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            clip_write_failure(
                stats,
                health,
                None,
                format!("create clip dir {}: {error}", parent.display()),
            )
        })?;
    }
    if let Err(error) = fs::copy(&segment.path, &segment.final_path) {
        return Err(clip_write_failure(
            stats,
            health,
            Some(&segment.final_path),
            format!(
                "write durable clip {}: {error}",
                segment.final_path.display()
            ),
        ));
    }
    let file = match fs::File::open(&segment.final_path) {
        Ok(file) => file,
        Err(error) => {
            return Err(clip_write_failure(
                stats,
                health,
                Some(&segment.final_path),
                format!(
                    "open durable clip {}: {error}",
                    segment.final_path.display()
                ),
            ));
        }
    };
    if let Err(error) = file.sync_all() {
        return Err(clip_write_failure(
            stats,
            health,
            Some(&segment.final_path),
            format!(
                "sync durable clip {}: {error}",
                segment.final_path.display()
            ),
        ));
    }
    if let Some(parent) = segment.final_path.parent() {
        let directory = match fs::File::open(parent) {
            Ok(directory) => directory,
            Err(error) => {
                return Err(clip_write_failure(
                    stats,
                    health,
                    Some(&segment.final_path),
                    format!("open clip directory {}: {error}", parent.display()),
                ));
            }
        };
        if let Err(error) = directory.sync_all() {
            return Err(clip_write_failure(
                stats,
                health,
                Some(&segment.final_path),
                format!("sync clip directory {}: {error}", parent.display()),
            ));
        }
    }
    Ok(())
}

fn clip_write_failure(
    stats: &RuntimeStatsState,
    health: &HealthState,
    partial_final_path: Option<&Path>,
    message: impl Into<String>,
) -> String {
    stats.update(|stats| {
        stats.clip_write_failures = stats.clip_write_failures.saturating_add(1);
        mark_health_condition(&mut stats.health, "disk-full");
    });
    health.set(HealthStatus::DiskFull, "clip write failed");
    if let Some(path) = partial_final_path {
        let _ = fs::remove_file(path);
    }
    message.into()
}

fn env_is(key: &str, expected: &str) -> bool {
    std::env::var(key).is_ok_and(|value| value == expected)
}

fn mark_health_condition(current: &mut String, condition: &str) {
    if current.is_empty() || current == "ready" {
        *current = condition.to_string();
        return;
    }
    if !current.split(',').any(|part| part == condition) {
        current.push(',');
        current.push_str(condition);
    }
}

fn decoded_frames_sha256(media: &media_pipeline::DecodedVideoSegment) -> String {
    let mut hasher = Sha256::new();
    for frame in &media.frames {
        hasher.update(frame.width.to_le_bytes());
        hasher.update(frame.height.to_le_bytes());
        hasher.update(&frame.rgb);
    }
    format!("{:x}", hasher.finalize())
}

fn encoded_clip_sha256(media: &media_pipeline::DecodedVideoSegment) -> String {
    let mut hasher = Sha256::new();
    for unit in &media.encoded_units {
        hasher.update(unit);
    }
    format!("{:x}", hasher.finalize())
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.parse().ok()
}

fn maybe_crash_after_startup_node(node: &str) {
    if env_is("VIGIL_FAULT_CRASH_AFTER_STARTUP_NODE", node) {
        std::process::exit(3);
    }
}

struct CapturedSegment {
    path: PathBuf,
    final_path: PathBuf,
    source_ref: String,
    sequence: u64,
    stream_generation: u64,
    frames: u64,
    fps: f64,
    mime_type: String,
    clip_sha256: String,
    decoded_frames_sha256: String,
    motion_positive_frames: u64,
    observed_at: chrono::DateTime<chrono::Utc>,
    media: media_pipeline::DecodedVideoSegment,
}

struct MemoryNodes {
    context_id: context_graph::ContextId,
    camera_id: context_graph::EntityId,
    decision_id: context_graph::DecisionId,
    intention_id: context_graph::IntentionId,
}

fn maintain_runtime_memory(
    store: &Store,
    config: &config::RuntimeConfig,
    rtsp_url: &str,
) -> Result<MemoryNodes, String> {
    let context = get_or_create_context(store, &config.site_name)?;
    let camera = get_or_create_camera(store, context.id, &config.camera_name, rtsp_url)?;
    let intention = get_or_create_intention(store, context.id, &config.camera_name)?;
    let decision = get_or_create_decision(store, config, context.id, camera.id, intention.id)?;
    maybe_crash_after_startup_node("detector-decision");
    Ok(MemoryNodes {
        context_id: context.id,
        camera_id: camera.id,
        decision_id: decision.id,
        intention_id: intention.id,
    })
}

fn record_detected_events(
    store: &Store,
    config: &config::RuntimeConfig,
    segment: &CapturedSegment,
    output: &yolox_detector::DetectorOutput,
    stats: &RuntimeStatsState,
    health: &HealthState,
    detection_publisher: Option<&crate::ha_mqtt_tasks::DetectionPublisher>,
) -> Result<(), String> {
    if output.detections.is_empty() {
        let _ = fs::remove_file(&segment.path);
        return Ok(());
    }
    let rtsp_url = config
        .rtsp_url
        .as_deref()
        .map(media_pipeline::redact_rtsp_url)
        .unwrap_or_default();
    let nodes = match maintain_runtime_memory(store, config, &rtsp_url) {
        Ok(nodes) => nodes,
        Err(error) => {
            let _ = fs::remove_file(&segment.path);
            return Err(error);
        }
    };
    if duplicate_detection_seen(store, segment, nodes.decision_id, nodes.context_id) {
        println!(
            "duplicate_segment_suppressed=true sequence={}",
            segment.sequence
        );
        let _ = fs::remove_file(&segment.path);
        return Ok(());
    }
    stats.update(|stats| {
        stats.detections_emitted = stats.detections_emitted.saturating_add(1);
    });
    if let Err(error) = finalize_clip(segment, stats, health) {
        let _ = fs::remove_file(&segment.path);
        return Err(error);
    }
    let _ = fs::remove_file(&segment.path);
    for detection in output.detections.iter().take(1) {
        match record_one_event(store, &nodes, detection, output, segment, config) {
            Ok((observation_id, snapshot_ref)) => {
                stats.update(|stats| {
                    stats.observations_written = stats.observations_written.saturating_add(1);
                    if stats.health.is_empty() {
                        stats.health = "ready".to_string();
                    }
                });
                health.set(HealthStatus::Ready, "event recorded");
                println!("observation_written=true");
                // Publish the detection event via the long-lived publisher when configured.
                if let Some(publisher) = detection_publisher {
                    let input = crate::ha_discovery::DetectionInput {
                        observation_id: observation_id.to_string(),
                        camera_name: config.camera_name.clone(),
                        object_class: detection.class_name.clone(),
                        confidence: detection.confidence,
                        timestamp_ms: segment.observed_at.timestamp_millis(),
                        evidence_ref: segment.source_ref.clone(),
                        snapshot_ref,
                        zone: None,
                    };
                    let cam_slug = camera_slug(&config.camera_name);
                    let topic = format!("vigil/{}/{}/detection", config.service_id, cam_slug);
                    let evt = crate::ha_discovery::map_detection_to_event_payload(&input);
                    match serde_json::to_string(&evt) {
                        Ok(json) => {
                            publisher.try_publish(topic, json);
                            // Feed the per-camera motion binary_sensor retained state.
                            let active_topic =
                                format!("vigil/{}/{}/active", config.service_id, cam_slug);
                            publisher.notify_active(active_topic);
                            println!("mqtt_detection_published=true");
                        }
                        Err(e) => println!("mqtt_detection_serialize_error={e}"),
                    }
                }
            }
            Err(error) => {
                stats.update(|stats| {
                    stats.observation_write_failures =
                        stats.observation_write_failures.saturating_add(1);
                });
                return Err(error);
            }
        }
    }
    Ok(())
}

fn duplicate_detection_seen(
    store: &Store,
    segment: &CapturedSegment,
    decision_id: context_graph::DecisionId,
    // Vigil is one-context-per-site; scope to the known context to avoid a full-store scan.
    context_id: context_graph::ContextId,
) -> bool {
    let decision_id = decision_id.to_string();
    store
        .list_observations(Some(context_id))
        .map(|observations| {
            observations.iter().any(|observation| {
                let same_decision = observation
                    .observed_properties
                    .get("detector_decision_id")
                    .and_then(Value::as_str)
                    == Some(decision_id.as_str());
                let same_clip = observation
                    .properties
                    .get("clip_sha256")
                    .and_then(Value::as_str)
                    == Some(segment.clip_sha256.as_str())
                    || observation
                        .properties
                        .get("decoded_frames_sha256")
                        .and_then(Value::as_str)
                        == Some(segment.decoded_frames_sha256.as_str());
                let same_stream_generation = observation
                    .properties
                    .get("stream_generation")
                    .and_then(Value::as_u64)
                    == Some(segment.stream_generation);
                same_decision && same_clip && same_stream_generation
            })
        })
        .unwrap_or(false)
}

/// Record a single detection event into cg and return the new observation id plus the
/// snapshot (detector evidence image) source_ref so the caller can publish the MQTT
/// detection event without re-querying the store.
fn record_one_event(
    store: &Store,
    nodes: &MemoryNodes,
    detection: &yolox_detector::Detection,
    output: &yolox_detector::DetectorOutput,
    segment: &CapturedSegment,
    config: &config::RuntimeConfig,
) -> Result<(ObservationId, String), String> {
    let observed_at = segment.observed_at;
    let video_evidence = EvidenceRef {
        id: context_graph::EvidenceId::new_v7(),
        kind: EvidenceKind::VideoSegment,
        source_ref: segment.source_ref.clone(),
        mime_type: Some(segment.mime_type.clone()),
        captured_at: Some(observed_at),
        producer: EvidenceProducer {
            system: "vigil".to_string(),
            model_name: config.detector_model_id.clone(),
            model_version: "0.1".to_string(),
            pipeline_version: "first-run".to_string(),
        },
        retention_status: RetentionStatus::RetainedExternal,
        ..EvidenceRef::default()
    };
    let detector_image = write_detector_evidence_image(segment, detection, config)?;
    let image_evidence = EvidenceRef {
        id: context_graph::EvidenceId::new_v7(),
        kind: EvidenceKind::ImageFrame,
        source_ref: detector_image.source_ref.clone(),
        mime_type: Some("image/png".to_string()),
        content_hash: Some(format!("sha256:{}", detector_image.sha256)),
        captured_at: Some(observed_at),
        frame_index: Some(detection.frame_index),
        region: Some(json!({
            "bbox": detection.bbox,
            "coordinate_space": "detector_input",
            "width": DETECTOR_INPUT_WIDTH,
            "height": DETECTOR_INPUT_HEIGHT
        })),
        producer: EvidenceProducer {
            system: "vigil".to_string(),
            model_name: config.detector_model_id.clone(),
            model_version: "0.1".to_string(),
            pipeline_version: "first-run".to_string(),
        },
        retention_status: RetentionStatus::RetainedExternal,
        ..EvidenceRef::default()
    };
    let mut observed_properties = BTreeMap::new();
    observed_properties.insert(
        "class".to_string(),
        Value::String(detection.class_name.clone()),
    );
    observed_properties.insert("confidence".to_string(), json!(detection.confidence));
    observed_properties.insert("bbox".to_string(), Value::String(detection.bbox.clone()));
    observed_properties.insert("frame_index".to_string(), json!(detection.frame_index));
    observed_properties.insert(
        "detector_decision_id".to_string(),
        Value::String(nodes.decision_id.to_string()),
    );
    let mut properties = BTreeMap::new();
    properties.insert("clip_frame_count".to_string(), json!(segment.frames));
    properties.insert(
        "baseline_intention_id".to_string(),
        Value::String(nodes.intention_id.to_string()),
    );
    properties.insert(
        "detector_backend".to_string(),
        Value::String(output.detector_backend.clone()),
    );
    properties.insert(
        "detector_session_id".to_string(),
        Value::String(output.detector_session_id.clone()),
    );
    properties.insert(
        "detector_model_sha256".to_string(),
        Value::String(output.model_sha256.clone()),
    );
    properties.insert(
        detector_digest_property_key("model_forward"),
        Value::String(output.model_forward_digest().to_string()),
    );
    properties.insert(
        detector_digest_property_key("nms"),
        Value::String(output.nms_digest().to_string()),
    );
    properties.insert(
        detector_digest_property_key("result"),
        Value::String(output.result_digest().to_string()),
    );
    properties.insert(
        "clip_sha256".to_string(),
        Value::String(output.clip_digest().to_string()),
    );
    properties.insert(
        "decoded_frames_sha256".to_string(),
        Value::String(segment.decoded_frames_sha256.clone()),
    );
    properties.insert(
        "stream_generation".to_string(),
        Value::Number(segment.stream_generation.into()),
    );
    properties.insert("capture_sequence".to_string(), json!(segment.sequence));
    properties.insert(
        "detector_evidence_ref".to_string(),
        Value::String(detector_image.source_ref.clone()),
    );
    properties.insert(
        "detector_evidence_sha256".to_string(),
        Value::String(detector_image.sha256.clone()),
    );
    properties.insert(
        "detector_input_width".to_string(),
        json!(DETECTOR_INPUT_WIDTH),
    );
    properties.insert(
        "detector_input_height".to_string(),
        json!(DETECTOR_INPUT_HEIGHT),
    );
    let observation_id = ObservationId::new_v7();
    if let Err(error) = store.record_observation(RecordObservation {
        id: observation_id,
        entity_id: nodes.camera_id,
        context_id: nodes.context_id,
        observation_type: "detection".to_string(),
        source: "vigil".to_string(),
        observed_at,
        evidence: vec![video_evidence, image_evidence],
        observed_properties,
        state_delta: BTreeMap::new(),
        properties,
        embeddings: Vec::new(),
    }) {
        let _ = fs::remove_file(&detector_image.path);
        return Err(format!("record observation: {error}"));
    }
    Ok((observation_id, detector_image.source_ref))
}

struct DetectorEvidenceImage {
    source_ref: String,
    sha256: String,
    path: PathBuf,
}

fn write_detector_evidence_image(
    segment: &CapturedSegment,
    detection: &yolox_detector::Detection,
    config: &config::RuntimeConfig,
) -> Result<DetectorEvidenceImage, String> {
    let parent = segment
        .final_path
        .parent()
        .ok_or_else(|| format!("clip path {} has no parent", segment.final_path.display()))?;
    let stem = segment
        .final_path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            format!(
                "clip path {} has no UTF-8 stem",
                segment.final_path.display()
            )
        })?;
    let class_name = detection
        .class_name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    let file_name = format!(
        "{stem}-detector-frame-{}-{class_name}.png",
        detection.frame_index
    );
    let path = parent.join(&file_name);
    media_pipeline::write_detector_evidence_png(
        &segment.media,
        detection.frame_index,
        &detection.bbox,
        &path,
        DETECTOR_INPUT_WIDTH,
        DETECTOR_INPUT_HEIGHT,
    )
    .map_err(|error| {
        format!(
            "write detector evidence for {}: {error}",
            config.camera_name
        )
    })?;
    Ok(DetectorEvidenceImage {
        source_ref: format!("vigil-edge:clip/{file_name}"),
        sha256: media_pipeline::sha256_path(&path)?,
        path,
    })
}

fn detector_digest_property_key(kind: &str) -> String {
    ["detector", kind, "sha256"].join("_")
}

fn get_or_create_context(store: &Store, name: &str) -> Result<context_graph::Context, String> {
    if let Some(context) = store
        .list_contexts()
        .map_err(|error| format!("list contexts: {error}"))?
        .into_iter()
        .find(|context| context.name == name)
    {
        return Ok(context);
    }
    store
        .create_context(CreateContext {
            name: name.to_string(),
            labels: vec!["site".to_string()],
            properties: BTreeMap::new(),
        })
        .map_err(|error| format!("create context: {error}"))
}

fn get_or_create_camera(
    store: &Store,
    context_id: context_graph::ContextId,
    name: &str,
    rtsp_url: &str,
) -> Result<context_graph::Entity, String> {
    if let Some(camera) = store
        .list_entities(ListEntityFilter {
            entity_type: Some(EntityType::Device),
            context_id: Some(context_id),
            ..ListEntityFilter::default()
        })
        .map_err(|error| format!("list cameras: {error}"))?
        .into_iter()
        .find(|camera| camera.name == name)
    {
        if camera.properties.get("rtsp_url").and_then(Value::as_str) == Some(rtsp_url) {
            return Ok(camera);
        }
        let mut properties = camera.properties.clone();
        properties.insert("rtsp_url".to_string(), Value::String(rtsp_url.to_string()));
        return store
            .update_entity(
                camera.id,
                EntityPatch {
                    properties: Some(properties),
                    ..EntityPatch::default()
                },
            )
            .map_err(|error| format!("update camera rtsp_url: {error}"));
    }
    let mut properties = BTreeMap::new();
    properties.insert("rtsp_url".to_string(), Value::String(rtsp_url.to_string()));
    store
        .create_entity(CreateEntity {
            entity_type: EntityType::Device,
            name: name.to_string(),
            properties,
            tags: vec!["camera".to_string()],
            context_id,
        })
        .map_err(|error| format!("create camera: {error}"))
}

fn get_or_create_intention(
    store: &Store,
    context_id: context_graph::ContextId,
    camera_name: &str,
) -> Result<context_graph::Intention, String> {
    let description = camera_intention_description(camera_name);
    for entry in store
        .audit_query(AuditFilter::default())
        .map_err(|error| format!("query audit: {error}"))?
    {
        let AuditTarget::Intention(id) = entry.target else {
            continue;
        };
        if let Some(intention) = store
            .get_intention(id)
            .map_err(|error| format!("get intention: {error}"))?
            && intention.context_id == context_id
            && intention.description == description
        {
            return Ok(intention);
        }
    }
    store
        .create_intention(CreateIntention {
            id: None,
            description,
            status: IntentionStatus::Active,
            origin: IntentionOrigin::Agent,
            context_id,
            properties: BTreeMap::new(),
            tags: vec!["camera".to_string()],
        })
        .map_err(|error| format!("create intention: {error}"))
}

fn camera_intention_description(camera_name: &str) -> String {
    format!("watch {}", camera_name.trim())
}

fn get_or_create_decision(
    store: &Store,
    config: &config::RuntimeConfig,
    context_id: context_graph::ContextId,
    camera_id: context_graph::EntityId,
    intention_id: context_graph::IntentionId,
) -> Result<context_graph::Decision, String> {
    for entry in store
        .audit_query(AuditFilter::default())
        .map_err(|error| format!("query audit: {error}"))?
    {
        let AuditTarget::Decision(id) = entry.target else {
            continue;
        };
        if let Some(decision) = store
            .get_decision(id)
            .map_err(|error| format!("get decision: {error}"))?
            && decision.context_id == context_id
            && decision.properties.get("model_id").and_then(Value::as_str)
                == Some(config.detector_model_id.as_str())
            && decision
                .properties
                .get("threshold")
                .and_then(Value::as_f64)
                .map(|value| (value - config.detector_confidence_threshold).abs() < f64::EPSILON)
                .unwrap_or(false)
        {
            return Ok(decision);
        }
    }
    let mut properties = BTreeMap::new();
    properties.insert(
        "model_id".to_string(),
        Value::String(config.detector_model_id.clone()),
    );
    properties.insert(
        "threshold".to_string(),
        json!(config.detector_confidence_threshold),
    );
    store
        .create_decision(CreateDecision {
            decision_type: "detector_config".to_string(),
            description: format!("Run local detector for {}", config.camera_name.trim()),
            reasoning: Vec::new(),
            confidence: Some(0.5),
            intention_ids: vec![intention_id],
            based_on_entity_ids: vec![camera_id],
            basis_fields: None,
            based_on_snapshots: Vec::new(),
            tags: vec!["detector".to_string()],
            properties,
            context_id,
            precedent_ids: Vec::new(),
            agent_id: None,
        })
        .map_err(|error| format!("create decision: {error}"))
}
