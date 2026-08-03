use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub(crate) struct RuntimeStatsState {
    inner: Arc<Mutex<RuntimeStats>>,
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RuntimeStats {
    pub(crate) frames_received: u64,
    pub(crate) detector_invocations: u64,
    pub(crate) detections_emitted: u64,
    pub(crate) observations_written: u64,
    pub(crate) clip_write_failures: u64,
    pub(crate) observation_write_failures: u64,
    pub(crate) motion_positive_frames: u64,
    pub(crate) stream_drops: u64,
    pub(crate) stream_reconnects: u64,
    pub(crate) stream_fps: f64,
    pub(crate) detector_latency_p50_ms: f64,
    pub(crate) detector_latency_p95_ms: f64,
    pub(crate) detector_latency_max_ms: f64,
    pub(crate) dropped_motion_positive_frames: u64,
    pub(crate) processing_lag_ms: f64,
    pub(crate) processing_lag_bound_ms: f64,
    pub(crate) false_positive_count: u64,
    pub(crate) crops_embedded: u64,
    pub(crate) recognition_matches: u64,
    pub(crate) recognition_unknowns: u64,
    pub(crate) recognition_failures: u64,
    pub(crate) embed_latency_max_ms: f64,
    pub(crate) health: String,
    pub(crate) ingest_signal: String,
    /// Active decode backend per stream, from observed receipts.
    #[serde(default)]
    pub(crate) active_decoder: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) active_detector_backend: String,
    /// probe_status:failure_code from the latest decode receipt.
    #[serde(default)]
    pub(crate) decode_acceleration: String,
    #[serde(default)]
    pub(crate) detection_acceleration: String,
    /// The fixed-format `[detect.acceleration]` receipt block (same render
    /// doctor uses), so `vigil stats` and `/health` carry the full honest
    /// receipt, not just the summary line above.
    #[serde(default)]
    pub(crate) detection_receipt_block: String,
    /// The fixed-format `[decode.hardware]` receipt block per stream (same
    /// render doctor uses) — decode receipts are per-stream, so each stream's
    /// latest block is kept, keeping `vigil stats` at receipt parity with
    /// `/health` and the doctor.
    #[serde(default)]
    pub(crate) decode_receipt_blocks: std::collections::BTreeMap<String, String>,
    /// Detector stage queue: depth/capacity/queued/replaced counters.
    #[serde(default)]
    pub(crate) detector_queue: String,
    /// Fabric enrollment status (criterion C7): empty when fabric is not
    /// configured, so this line never appears on an unenrolled node.
    #[serde(default)]
    pub(crate) fabric_status: String,
    /// Non-secret fabric join advice (criterion C6) — structurally
    /// incapable of carrying the enrollment ticket. See
    /// [`FabricJoinAdvice`].
    #[serde(default)]
    pub(crate) fabric_join_advice: FabricJoinAdvice,
    /// Recent work-graph stage receipts (bounded, newest last).
    #[serde(default)]
    pub(crate) recent_work_receipts: Vec<String>,
}

/// Bounded push for the recent-receipts window.
pub(crate) fn push_recent_receipt(stats: &mut RuntimeStats, line: String) {
    const RECENT_WORK_RECEIPTS: usize = 16;
    stats.recent_work_receipts.push(line);
    let excess = stats
        .recent_work_receipts
        .len()
        .saturating_sub(RECENT_WORK_RECEIPTS);
    if excess > 0 {
        stats.recent_work_receipts.drain(..excess);
    }
}

impl Default for RuntimeStats {
    fn default() -> Self {
        Self {
            frames_received: 0,
            detector_invocations: 0,
            detections_emitted: 0,
            observations_written: 0,
            clip_write_failures: 0,
            observation_write_failures: 0,
            motion_positive_frames: 0,
            stream_drops: 0,
            stream_reconnects: 0,
            stream_fps: 0.0,
            detector_latency_p50_ms: 0.0,
            detector_latency_p95_ms: 0.0,
            detector_latency_max_ms: 0.0,
            dropped_motion_positive_frames: 0,
            processing_lag_ms: 0.0,
            processing_lag_bound_ms: 1.0,
            false_positive_count: 0,
            crops_embedded: 0,
            recognition_matches: 0,
            recognition_unknowns: 0,
            recognition_failures: 0,
            embed_latency_max_ms: 0.0,
            health: "ready".to_string(),
            ingest_signal: "ok".to_string(),
            active_decoder: std::collections::BTreeMap::new(),
            active_detector_backend: String::new(),
            decode_acceleration: String::new(),
            detection_acceleration: String::new(),
            detection_receipt_block: String::new(),
            decode_receipt_blocks: std::collections::BTreeMap::new(),
            detector_queue: String::new(),
            fabric_status: String::new(),
            fabric_join_advice: FabricJoinAdvice::default(),
            recent_work_receipts: Vec::new(),
        }
    }
}

impl RuntimeStatsState {
    pub(crate) fn new(data_dir: &Path) -> Self {
        let path = snapshot_path(data_dir);
        let stats = read_snapshot(data_dir).unwrap_or_default();
        Self {
            inner: Arc::new(Mutex::new(stats)),
            path,
        }
    }

    pub(crate) fn snapshot(&self) -> RuntimeStats {
        self.inner
            .lock()
            .map(|stats| stats.clone())
            .unwrap_or_default()
    }

    pub(crate) fn update(&self, update: impl FnOnce(&mut RuntimeStats)) {
        // Mutate in memory under the lock, but never hold the lock across
        // disk IO (every camera and detector thread shares it). The disk
        // write is SYNCHRONOUS per update on purpose: the live runtime
        // serves stats from memory over the control socket, so the file's
        // one job is being correct after an abrupt exit — a coalescing
        // window here silently loses the final counters a crash-path
        // acceptance (and a post-mortem operator) reads back.
        let snapshot = {
            let Ok(mut stats) = self.inner.lock() else {
                return;
            };
            update(&mut stats);
            stats.clone()
        };
        let _ = write_snapshot_path(&self.path, &snapshot);
    }
}

/// Non-secret fabric join advice, printed on `vigil stats`/`vigil doctor`
/// and persisted to `runtime-stats.json`. Unlike the join instruction
/// [`crate::fabric::FabricRuntime::join_instruction`] prints on the
/// sanctioned startup log (which embeds the current ticket as a
/// `ticket=<value>` field), this type carries exactly three FIXED
/// variants and has no field of any kind — no `String`, no wrapped value —
/// that any future edit could ever assign the ticket to. Retrieving the
/// actual ticket is confined to that startup log and the deliberate
/// `vigil fabric ticket` command
/// ([`crate::fabric::FabricRuntime::own_ticket`]); this type structurally
/// cannot become a fourth path no matter what it grows.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub(crate) enum FabricJoinAdvice {
    /// Fabric is not configured on this node — nothing to print.
    #[default]
    NotConfigured,
    /// This node does not carry the hub role: the one-line instruction for
    /// growing it into a join point. Never carries a ticket (there is
    /// nothing yet to advertise).
    GrowHint,
    /// This node carries the hub role and has a ticket ready — the fixed,
    /// ticket-free pointer to the retrieval command.
    HubReady,
}

impl FabricJoinAdvice {
    /// The fixed line to print, or `None` when there is nothing to say
    /// ([`FabricJoinAdvice::NotConfigured`]).
    pub(crate) fn render(self) -> Option<&'static str> {
        match self {
            FabricJoinAdvice::NotConfigured => None,
            FabricJoinAdvice::GrowHint => Some(
                "fabric-join grow this node into a fabric join point by enabling the hub role \
                 (fabric_hub = true) — once enrolled, retrieve a ready-to-use join instruction \
                 with `vigil fabric ticket`",
            ),
            FabricJoinAdvice::HubReady => {
                Some("fabric-join ready=true retrieve-with=\"vigil fabric ticket\"")
            }
        }
    }
}

/// A live view onto the non-secret `fabric-status=` line alone. Unlike
/// [`RuntimeStats`] (whose `fabric_join` field carries the fabric enrollment
/// ticket — a credential), this type has no field and no accessor that can
/// ever yield the join instruction or the ticket it carries. `/health`'s HTTP
/// responder is threaded this handle rather than the full stats snapshot, so
/// it structurally cannot serve the ticket regardless of what fields
/// `RuntimeStats` grows later.
#[derive(Clone)]
pub(crate) struct HealthFabricStatus {
    inner: RuntimeStatsState,
}

impl HealthFabricStatus {
    pub(crate) fn from_state(state: &RuntimeStatsState) -> Self {
        Self {
            inner: state.clone(),
        }
    }

    /// The current `fabric-status=` line, read live — empty when fabric is
    /// not configured.
    pub(crate) fn read(&self) -> String {
        self.inner.snapshot().fabric_status
    }
}

pub(crate) fn read_snapshot(data_dir: &Path) -> Option<RuntimeStats> {
    let text = fs::read_to_string(snapshot_path(data_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_snapshot_path(path: &Path, stats: &RuntimeStats) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create stats directory {}: {error}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(stats).map_err(|error| error.to_string())?;
    // Atomic replace: a reader never sees a torn file and a crash mid-write
    // never resets counters on the next boot.
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, text).map_err(|error| format!("write stats {}: {error}", temp.display()))?;
    fs::rename(&temp, path).map_err(|error| format!("commit stats {}: {error}", path.display()))
}

fn snapshot_path(data_dir: &Path) -> PathBuf {
    data_dir.join("runtime-stats.json")
}

pub(crate) fn format_stats(stats: &RuntimeStats) -> String {
    let base = format!(
        "frames-received={}\n\
detector-invocations={}\n\
detections-emitted={}\n\
observations-written={}\n\
clip-write-failures={}\n\
observation-write-failures={}\n\
motion-positive-frames={}\n\
stream-drops={}\n\
stream-reconnects={}\n\
stream-fps={:.6}\n\
detector-latency-p50-ms={:.6}\n\
detector-latency-p95-ms={:.6}\n\
detector-latency-max-ms={:.6}\n\
dropped-motion-positive-frames={}\n\
processing-lag-ms={:.6}\n\
processing-lag-bound-ms={:.6}\n\
false-positive-count={}\n\
crops-embedded={}\n\
recognition-matches={}\n\
recognition-unknowns={}\n\
recognition-failures={}\n\
embed-latency-max-ms={:.6}\n\
health={}\n\
ingest={}\n\
telemetry-sink=local\n",
        stats.frames_received,
        stats.detector_invocations,
        stats.detections_emitted,
        stats.observations_written,
        stats.clip_write_failures,
        stats.observation_write_failures,
        stats.motion_positive_frames,
        stats.stream_drops,
        stats.stream_reconnects,
        stats.stream_fps,
        stats.detector_latency_p50_ms,
        stats.detector_latency_p95_ms,
        stats.detector_latency_max_ms,
        stats.dropped_motion_positive_frames,
        stats.processing_lag_ms,
        stats.processing_lag_bound_ms,
        stats.false_positive_count,
        stats.crops_embedded,
        stats.recognition_matches,
        stats.recognition_unknowns,
        stats.recognition_failures,
        stats.embed_latency_max_ms,
        stats.health,
        stats.ingest_signal
    );
    let mut out = base;
    for (stream, backend) in &stats.active_decoder {
        out.push_str(&format!("active-decoder[{stream}]={backend}\n"));
    }
    if !stats.active_detector_backend.is_empty() {
        out.push_str(&format!(
            "active-detector-backend={}\n",
            stats.active_detector_backend
        ));
    }
    if !stats.decode_acceleration.is_empty() {
        out.push_str(&format!(
            "decode-acceleration={}\n",
            stats.decode_acceleration
        ));
    }
    if !stats.detection_acceleration.is_empty() {
        out.push_str(&format!(
            "detection-acceleration={}\n",
            stats.detection_acceleration
        ));
    }
    for block in stats.decode_receipt_blocks.values() {
        out.push_str(block);
    }
    if !stats.detection_receipt_block.is_empty() {
        out.push_str(&stats.detection_receipt_block);
    }
    if !stats.detector_queue.is_empty() {
        out.push_str(&format!("detector-queue={}\n", stats.detector_queue));
    }
    if !stats.fabric_status.is_empty() {
        out.push_str(&format!("{}\n", stats.fabric_status));
    }
    if let Some(line) = stats.fabric_join_advice.render() {
        out.push_str(&format!("{line}\n"));
    }
    for receipt in &stats.recent_work_receipts {
        out.push_str(&format!("work-receipt={receipt}\n"));
    }
    out
}
