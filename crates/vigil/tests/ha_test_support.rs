// Shared test harness support for ha_correction_seam.rs and ha_mqtt_broker.rs.
//
// Included via:
//   #[path = "ha_test_support.rs"]
//   mod ha_test_support;
//
// Provides deterministic cg detection fixtures for correction/MQTT contract
// tests, plus shared process-fixture helpers used by live detector tests.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use context_graph::{
    CreateContext, CreateDecision, CreateEntity, CreateIntention, EmbedderConfig, EntityType,
    EvidenceId, EvidenceKind, EvidenceProducer, EvidenceRef, IntentionOrigin, IntentionStatus,
    ObservationId, RecordObservation, RetentionStatus, Store, StoreConfig,
};
use serde_json::{Value, json};
use tempfile::TempDir;

#[path = "deterministic_test_support.rs"]
mod deterministic_test_support;
use deterministic_test_support::TcpPortReservation;
pub use deterministic_test_support::wait_until;

// ── Product constants ──────────────────────────────────────────────────────

pub const SITE_NAME: &str = "home farm";
pub const CAMERA_NAME: &str = "lower gate";
pub const DETECTOR_MODEL_ID: &str = "yolox-tiny-burn-cpu";
pub const DETECTOR_THRESHOLD: f64 = 0.5;

// ── Store helpers ──────────────────────────────────────────────────────────

pub fn open_store_at(path: &Path) -> Result<Store, String> {
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

// ── Free port ──────────────────────────────────────────────────────────────

pub fn free_port() -> Result<u16, String> {
    // Compatibility for older test callers that still accept a bare u16.
    // New process fixtures must hold TcpPortReservation until spawn.
    Ok(TcpPortReservation::reserve_loopback()?.release())
}

// ── Wait for TCP port ──────────────────────────────────────────────────────

pub fn wait_for_tcp_port(port: u16, timeout: Duration) -> Result<(), String> {
    wait_until(&format!("TCP port {port} to open"), timeout, || {
        Ok(TcpStream::connect(("127.0.0.1", port)).ok().map(|_| ()))
    })
}

// ── Capture pipe ───────────────────────────────────────────────────────────

pub fn capture_pipe<T: Read + Send + 'static>(pipe: Option<T>) -> Arc<Mutex<String>> {
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

// ── Tool / binary paths ────────────────────────────────────────────────────

pub fn tool_path(env_key: &str, binary: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(env_key).map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    std::env::var_os("PATH")
        .and_then(|path_var| {
            std::env::split_paths(&path_var)
                .map(|dir| dir.join(binary))
                .find(|p| p.is_file())
        })
        .or_else(|| {
            let cached = workspace_root()
                .join("target")
                .join("vigil-test-tools")
                .join("mediamtx")
                .join("v1.19.1")
                .join(binary);
            cached.is_file().then_some(cached)
        })
}

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn vigil_binary_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_vigil") {
        return PathBuf::from(path);
    }
    if let Some(path) = option_env!("CARGO_BIN_EXE_vigil") {
        return PathBuf::from(path);
    }
    workspace_root().join("target").join("debug").join("vigil")
}

// ── TOML encode helpers ────────────────────────────────────────────────────

pub fn toml_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

pub fn toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

// ── RTSP fixture lock (serialises RTSP fixture use within one test binary) ─

pub fn rtsp_fixture_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ── RTSP fixture (mediamtx + ffmpeg) ──────────────────────────────────────

pub struct RtspFixture {
    pub url: String,
    mediamtx: Child,
    ffmpeg: Option<Child>,
    mediamtx_stdout: Arc<Mutex<String>>,
    mediamtx_stderr: Arc<Mutex<String>>,
    ffmpeg_stderr: Arc<Mutex<String>>,
    _conf_dir: TempDir,
}

impl RtspFixture {
    pub fn start(clip: &Path) -> Result<Self, String> {
        let mediamtx_bin = tool_path("VIGIL_MEDIAMTX_BIN", "mediamtx")
            .ok_or_else(|| "required test prerequisite `mediamtx` was not found".to_string())?;
        let ffmpeg_bin = tool_path("VIGIL_FFMPEG_BIN", "ffmpeg")
            .ok_or_else(|| "required test prerequisite `ffmpeg` was not found".to_string())?;
        let rtsp_reservation = TcpPortReservation::reserve_loopback()?;
        let rtp_reservation = TcpPortReservation::reserve_loopback()?;
        let rtcp_reservation = TcpPortReservation::reserve_loopback()?;
        let port = rtsp_reservation.port();
        let rtp_port = rtp_reservation.port();
        let rtcp_port = rtcp_reservation.port();
        let url = format!("rtsp://127.0.0.1:{port}/lower-gate");
        let conf_dir = tempfile::tempdir().map_err(|e| format!("rtsp conf dir: {e}"))?;
        let conf_path = conf_dir.path().join("mediamtx.yml");
        fs::write(
            &conf_path,
            format!(
                "rtspTransports: [tcp]\nrtspAddress: 127.0.0.1:{port}\n\
                 rtpAddress: 127.0.0.1:{rtp_port}\nrtcpAddress: 127.0.0.1:{rtcp_port}\n\
                 rtmp: no\nhls: no\nwebrtc: no\nsrt: no\nplayback: no\nmoq: no\n\
                 paths:\n  lower-gate:\n    source: publisher\n"
            ),
        )
        .map_err(|e| format!("write mediamtx config: {e}"))?;

        // Release immediately before the child binds. Startup validates the
        // protocol listener and reports the child output on failure.
        rtsp_reservation.release();
        rtp_reservation.release();
        rtcp_reservation.release();
        let mut mediamtx_child = Command::new(&mediamtx_bin)
            .arg(&conf_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn mediamtx: {e}"))?;
        let mediamtx_stdout = capture_pipe(mediamtx_child.stdout.take());
        let mediamtx_stderr = capture_pipe(mediamtx_child.stderr.take());
        if let Err(error) = wait_until("MediaMTX RTSP listener", Duration::from_secs(5), || {
            if let Some(status) = mediamtx_child
                .try_wait()
                .map_err(|e| format!("inspect MediaMTX: {e}"))?
            {
                return Err(format!("MediaMTX exited early with {status}"));
            }
            Ok(TcpStream::connect(("127.0.0.1", port)).ok().map(|_| ()))
        }) {
            let _ = mediamtx_child.kill();
            let _ = mediamtx_child.wait();
            return Err(format!(
                "{error}; MediaMTX stdout={:?}; stderr={:?}",
                snapshot(&mediamtx_stdout),
                snapshot(&mediamtx_stderr)
            ));
        }

        // ffmpeg: looping re-stream exactly as first_light_loop.rs does it.
        let mut ffmpeg_child = Command::new(&ffmpeg_bin)
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-copyts")
            .arg("-re")
            .arg("-stream_loop")
            .arg("-1")
            .arg("-i")
            .arg(clip)
            .arg("-an")
            .arg("-c:v")
            .arg("copy")
            .arg("-f")
            .arg("rtsp")
            .arg("-rtsp_transport")
            .arg("tcp")
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                let _ = mediamtx_child.kill();
                format!("spawn ffmpeg: {e}")
            })?;
        let ffmpeg_stderr = capture_pipe(ffmpeg_child.stderr.take());
        if let Err(error) = wait_until("FFmpeg RTSP publisher", Duration::from_secs(10), || {
            if let Some(status) = ffmpeg_child
                .try_wait()
                .map_err(|e| format!("inspect FFmpeg: {e}"))?
            {
                return Err(format!("FFmpeg exited early with {status}"));
            }
            let logs = format!(
                "{}{}",
                snapshot(&mediamtx_stdout),
                snapshot(&mediamtx_stderr)
            );
            Ok(
                (logs.contains("is publishing to path") || logs.contains("publisher is ready"))
                    .then_some(()),
            )
        }) {
            let _ = ffmpeg_child.kill();
            let _ = ffmpeg_child.wait();
            let _ = mediamtx_child.kill();
            let _ = mediamtx_child.wait();
            return Err(format!(
                "{error}; MediaMTX stdout={:?}; stderr={:?}; FFmpeg stderr={:?}",
                snapshot(&mediamtx_stdout),
                snapshot(&mediamtx_stderr),
                snapshot(&ffmpeg_stderr)
            ));
        }
        Ok(Self {
            url,
            mediamtx: mediamtx_child,
            ffmpeg: Some(ffmpeg_child),
            mediamtx_stdout,
            mediamtx_stderr,
            ffmpeg_stderr,
            _conf_dir: conf_dir,
        })
    }
}

impl Drop for RtspFixture {
    fn drop(&mut self) {
        if let Some(f) = self.ffmpeg.as_mut() {
            let _ = f.kill();
            let _ = f.wait();
        }
        let _ = self.mediamtx.kill();
        let _ = self.mediamtx.wait();
    }
}

impl RtspFixture {
    fn diagnostics(&self) -> String {
        format!(
            "MediaMTX stdout={:?}; stderr={:?}; FFmpeg stderr={:?}",
            snapshot(&self.mediamtx_stdout),
            snapshot(&self.mediamtx_stderr),
            snapshot(&self.ffmpeg_stderr)
        )
    }
}

fn snapshot(buffer: &Arc<Mutex<String>>) -> String {
    buffer
        .lock()
        .map(|value| value.clone())
        .unwrap_or_else(|_| "<capture poisoned>".to_string())
}

// ── Terminal-signal predicate (mirrors first_light_loop.rs exactly) ────────

/// Returns true when vigil has completed one pipeline run — including the case
/// where the motion gate suppresses the segment.  The harness MUST exit on this
/// signal and restart vigil so it reconnects at a different position in the
/// looping RTSP stream; staying alive for 300 s causes the motion gate to reset
/// on every loop cut and the harness never accumulates enough consecutive
/// motion-positive segments to write an observation.
pub fn runtime_reached_pipeline_terminal_signal(logs: &str) -> bool {
    logs.contains("rtsp opened")
        && logs.contains("decoded_frames=")
        && (logs.contains("observation_written=true")
            || logs.contains("motion_gate_suppressed_segment=true")
            || logs.contains("detector_detections=0")
            || logs.contains("record detection failed")
            || logs.contains("detector invocation failed")
            || logs.contains("rtsp probe failed"))
}

// ── Public: per-test store copy ────────────────────────────────────────────

const PNG_BYTES: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

fn seed_deterministic_detections(
    store: &Store,
    data_dir: &Path,
    count: usize,
) -> Result<(), String> {
    let context = store
        .create_context(CreateContext {
            name: SITE_NAME.to_string(),
            labels: vec!["site".to_string()],
            properties: BTreeMap::new(),
        })
        .map_err(|error| format!("create fixture context: {error}"))?;
    let camera = store
        .create_entity(CreateEntity {
            entity_type: EntityType::Device,
            name: CAMERA_NAME.to_string(),
            properties: BTreeMap::new(),
            tags: vec!["camera".to_string()],
            context_id: context.id,
        })
        .map_err(|error| format!("create fixture camera: {error}"))?;
    let intention = store
        .create_intention(CreateIntention {
            id: None,
            description: format!("watch {CAMERA_NAME}"),
            status: IntentionStatus::Active,
            origin: IntentionOrigin::Agent,
            context_id: context.id,
            properties: BTreeMap::new(),
            tags: vec!["camera".to_string()],
            blueprint_catalog_id: None,
        })
        .map_err(|error| format!("create fixture intention: {error}"))?;
    let decision = store
        .create_decision(CreateDecision {
            decision_type: "detector_config".to_string(),
            description: format!("Run local detector for {CAMERA_NAME}"),
            reasoning: Vec::new(),
            confidence: Some(DETECTOR_THRESHOLD as f32),
            intention_ids: vec![intention.id],
            based_on_entity_ids: vec![camera.id],
            basis_fields: None,
            based_on_snapshots: Vec::new(),
            tags: vec!["detector".to_string()],
            properties: BTreeMap::from([(
                "model_id".to_string(),
                Value::String(DETECTOR_MODEL_ID.to_string()),
            )]),
            context_id: context.id,
            precedent_ids: Vec::new(),
            agent_id: None,
        })
        .map_err(|error| format!("create fixture decision: {error}"))?;

    let clips = data_dir.join("clips");
    fs::create_dir_all(&clips).map_err(|error| format!("create fixture clips dir: {error}"))?;
    let base_time = chrono::DateTime::from_timestamp_millis(1_700_000_000_000)
        .expect("fixed Home Assistant fixture timestamp is valid");
    for index in 0..count {
        let clip_name = format!("ha-fixture-{index}.h264");
        let image_name = format!("ha-fixture-{index}.png");
        fs::write(
            clips.join(&clip_name),
            (0..4096)
                .map(|offset| ((offset + index * 17) % 251) as u8)
                .collect::<Vec<_>>(),
        )
        .map_err(|error| format!("write fixture clip {clip_name}: {error}"))?;
        fs::write(clips.join(&image_name), PNG_BYTES)
            .map_err(|error| format!("write fixture image {image_name}: {error}"))?;

        let observed_at = base_time - chrono::Duration::seconds((count - index) as i64);
        let observation_id = ObservationId::new_v7();
        let image_ref = format!("vigil-edge:clip/{image_name}");
        let producer = EvidenceProducer {
            system: "vigil".to_string(),
            model_name: DETECTOR_MODEL_ID.to_string(),
            model_version: "0.1".to_string(),
            pipeline_version: "ha-contract-fixture".to_string(),
        };
        store
            .record_observation(RecordObservation {
                id: observation_id,
                entity_id: camera.id,
                context_id: context.id,
                observation_type: "detection".to_string(),
                source: "vigil".to_string(),
                observed_at,
                evidence: vec![
                    EvidenceRef {
                        id: EvidenceId::new_v7(),
                        observation_id,
                        context_id: context.id,
                        kind: EvidenceKind::VideoSegment,
                        source_ref: format!("vigil-edge:clip/{clip_name}"),
                        mime_type: Some("video/h264".to_string()),
                        captured_at: Some(observed_at),
                        producer: producer.clone(),
                        retention_status: RetentionStatus::RetainedExternal,
                        ..Default::default()
                    },
                    EvidenceRef {
                        id: EvidenceId::new_v7(),
                        observation_id,
                        context_id: context.id,
                        kind: EvidenceKind::ImageFrame,
                        source_ref: image_ref.clone(),
                        mime_type: Some("image/png".to_string()),
                        captured_at: Some(observed_at),
                        frame_index: Some(7 + index as u64),
                        producer,
                        retention_status: RetentionStatus::RetainedExternal,
                        ..Default::default()
                    },
                ],
                observed_properties: BTreeMap::from([
                    ("class".to_string(), Value::String("person".to_string())),
                    ("confidence".to_string(), json!(0.82 + index as f64 * 0.01)),
                    (
                        "camera_name".to_string(),
                        Value::String(CAMERA_NAME.to_string()),
                    ),
                    ("evidence_ref".to_string(), Value::String(image_ref.clone())),
                    (
                        "detector_decision_id".to_string(),
                        Value::String(decision.id.to_string()),
                    ),
                ]),
                state_delta: BTreeMap::new(),
                properties: BTreeMap::from([
                    (
                        "baseline_intention_id".to_string(),
                        Value::String(intention.id.to_string()),
                    ),
                    (
                        "detector_evidence_ref".to_string(),
                        Value::String(image_ref),
                    ),
                ]),
                embeddings: Vec::new(),
            })
            .map_err(|error| format!("record fixture detection {index}: {error}"))?;
    }
    Ok(())
}

/// Returns an isolated deterministic cg detection fixture for one test.
///
/// The returned `TempDir` owns the copy's lifetime — hold it until after all
/// assertions, then drop it.  The `PathBuf` is the store path inside that dir.
///
pub fn fresh_store_copy(minimum_observations: usize) -> Result<(TempDir, PathBuf), String> {
    let tmp = tempfile::TempDir::new().map_err(|e| format!("create per-test tmp dir: {e}"))?;
    let data_dir = tmp.path().join("data");
    let store_path = data_dir.join("store.contextgraph");
    let store = open_store_at(&store_path)?;
    seed_deterministic_detections(&store, &data_dir, minimum_observations.max(1))?;
    let count = store
        .list_observations(None)
        .map_err(|e| format!("list observations in store copy: {e}"))?
        .len();
    assert!(
        count >= minimum_observations,
        "deterministic fixture has {count} observation(s) but this test requires \
         {minimum_observations}"
    );

    Ok((tmp, store_path))
}
