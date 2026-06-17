use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::Read;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use context_graph::{
    AuditFilter, AuditTarget, Decision, EmbedderConfig, EntityType, ListEntityFilter, Observation,
    Store, StoreConfig,
};
use tempfile::TempDir;

use crate::common::{
    StoreProbe, VigilBinary, VigilProcess, free_port, kill_tracked_child, output_text,
};

const CAMERA_RTSP_URL: &str = "rtsp://127.0.0.1:8554/lower-gate";

struct FixtureReadiness {
    person_clip_present: bool,
    empty_clip_present: bool,
    detector_artifact_present: bool,
    ffmpeg_present: bool,
    ffprobe_present: bool,
    mediamtx_present: bool,
}

impl FixtureReadiness {
    fn probe() -> Self {
        let root = workspace_root();
        Self {
            person_clip_present: root
                .join("tests/fixtures/video/one-by-one-person-detection.mp4")
                .is_file(),
            empty_clip_present: root
                .join("tests/fixtures/video/empty-scene-from-one-by-one-person-detection.mp4")
                .is_file(),
            detector_artifact_present: root
                .join("tests/fixtures/models/yolox-tiny-coco.pth")
                .is_file(),
            ffmpeg_present: tool_path("VIGIL_FFMPEG_BIN", "ffmpeg").is_some(),
            ffprobe_present: tool_path("VIGIL_FFPROBE_BIN", "ffprobe").is_some(),
            mediamtx_present: tool_path("VIGIL_MEDIAMTX_BIN", "mediamtx").is_some(),
        }
    }

    fn failures(&self) -> Vec<String> {
        let mut failures = Vec::new();
        if !self.person_clip_present {
            failures.push("real person clip fixture is missing".to_string());
        }
        if !self.empty_clip_present {
            failures.push("carved empty-scene fixture is missing".to_string());
        }
        if !self.detector_artifact_present {
            failures.push("YOLOX-Tiny COCO checkpoint fixture is missing".to_string());
        }
        if !self.ffmpeg_present {
            failures.push("ffmpeg is unavailable for the RTSP fixture sidecar".to_string());
        }
        if !self.ffprobe_present {
            failures.push("ffprobe is unavailable for clip decode probing".to_string());
        }
        if !self.mediamtx_present {
            failures.push("mediamtx is unavailable for the direct RTSP fixture server".to_string());
        }
        failures
    }
}

#[test]
fn frigate_replacement_loop_runs_over_direct_rtsp_synthetic() {
    let binary = VigilBinary::new();
    let readiness = FixtureReadiness::probe();
    let rtsp_result = RtspFixture::start();
    let mut fixture_failures = Vec::new();
    if let Err(error) = rtsp_result.as_ref() {
        fixture_failures.push(format!("RTSP fixture did not start: {error}"));
    }
    let rtsp = rtsp_result.ok();
    let rtsp_url = rtsp
        .as_ref()
        .map(RtspFixture::url)
        .unwrap_or_else(|| CAMERA_RTSP_URL.to_string());
    let data = TempDir::new().expect("tempdir");
    let store_path = data.path().join("store.contextgraph");
    let (_config_dir, config_path) =
        first_light_config(data.path(), &store_path, free_port(), &rtsp_url).expect("config");
    let version = binary.command().arg("--version").output();
    let mut failures = readiness.failures();
    failures.extend(fixture_failures);

    match version {
        Ok(output) => {
            let (stdout, stderr) = output_text(&output);
            if !output.status.success() {
                failures.push(format!(
                    "vigil binary was not executable: {}",
                    output.status
                ));
            }
            if stdout.trim().is_empty() {
                failures.push(format!(
                    "vigil binary printed no version; stderr={stderr:?}"
                ));
            }
        }
        Err(error) => failures.push(format!("could not execute vigil binary: {error}")),
    }

    let mut runtime_logs = String::new();
    let mut forbidden_connections = Vec::new();
    let mut trace_raw = String::new();
    match VigilProcess::spawn_network_traced(&binary, &config_path, free_port()) {
        Ok(mut process) => {
            let _ = process
                .health()
                .wait_for_status(200, Duration::from_secs(2));
            let _ = process.wait_for_log("observation_written=true", Duration::from_secs(300));
            runtime_logs = process.logs();
            trace_raw = process.network_trace().raw;
            forbidden_connections = disallowed_network_lines(&trace_raw, Some(&rtsp_url));
            let live_lock = StoreProbe::new(&store_path).live_lock_held();
            if !live_lock.locked {
                failures.push(live_lock.detail);
            }
            let _ = process.terminate();
        }
        Err(error) => failures.push(format!("vigil runtime did not start: {error}")),
    }

    let opened_rtsp = trace_connects_to_rtsp(&trace_raw, &rtsp_url);
    let rtsp_logs = rtsp.as_ref().map(RtspFixture::logs).unwrap_or_default();
    let rtsp_play_observed = rtsp_logs.to_ascii_lowercase().contains("is reading")
        || rtsp_logs.to_ascii_lowercase().contains("play");
    let decoded_frames = parsed_log_counter(&runtime_logs, "decoded_frames").unwrap_or_default();
    let detector_invocations =
        parsed_log_counter(&runtime_logs, "detector_invocations").unwrap_or_default();
    let store = open_store(&store_path).ok();
    let observation_count = store
        .as_ref()
        .and_then(|store| store.list_observations(None).ok())
        .map(|observations| observations.len())
        .unwrap_or_default();
    let authority = exact_five_node_authority(store.as_ref(), &rtsp_url);
    drop(store);
    let why = binary
        .command()
        .arg("why")
        .arg("--latest")
        .env("VIGIL_STORE_PATH", &store_path)
        .env("VIGIL_DATA_DIR", data.path())
        .output();
    let why_walked = why
        .as_ref()
        .map(|output| {
            let (stdout, stderr) = output_text(output);
            let text = format!("{stdout}{stderr}");
            output.status.success()
                && authority
                    .as_ref()
                    .map(|authority| authority.matches_why_text(&text))
                    .unwrap_or(false)
        })
        .unwrap_or(false);
    if !opened_rtsp {
        failures.push(format!(
            "binary did not connect to the configured direct RTSP source {rtsp_url}"
        ));
    }
    if !rtsp_play_observed {
        failures.push("RTSP server did not observe a PLAY request from the binary".to_string());
    }
    if decoded_frames == 0 {
        failures.push("binary did not decode real frames from the RTSP source".to_string());
    }
    if detector_invocations == 0 {
        failures.push("binary did not invoke the detector on decoded frames".to_string());
    }
    if observation_count != 1 {
        failures.push(format!(
            "binary did not land one clip-backed Observation, found {observation_count}"
        ));
    }
    if let Err(error) = authority.as_ref() {
        failures.push(format!(
            "binary did not land the exact five memory nodes: {error}"
        ));
    }
    if let Ok(authority) = authority.as_ref()
        && !clip_ref_is_decodable(data.path(), &authority.clip_ref)
    {
        failures.push(format!(
            "binary landed clip evidence that was not locally decodable: {}",
            authority.clip_ref
        ));
    }
    if let Ok(authority) = authority.as_ref()
        && let Err(error) = clip_ref_matches_stream_fixture(
            data.path(),
            &authority.clip_ref,
            &workspace_root().join("tests/fixtures/video/one-by-one-person-detection.mp4"),
        )
    {
        failures.push(format!(
            "binary landed clip evidence without RTSP fixture frame provenance: {error}"
        ));
    }
    if !why_walked {
        failures
            .push("binary did not walk the landed event through the review command".to_string());
    }
    if !forbidden_connections.is_empty() {
        failures.push(format!(
            "binary opened non-RTSP network sockets: {forbidden_connections:?}"
        ));
    }
    if !failures.is_empty() {
        failures.push(format!(
            "runtime log tail: {}",
            last_log_lines(&runtime_logs, 80)
        ));
    }

    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn frigate_replacement_real_camera_event_lands_and_walks() {
    let mut failures = Vec::new();

    match env::var("VIGIL_ACCEPTANCE_RTSP_URL") {
        Ok(url) if !url.trim().is_empty() => {
            let hard_window = hard_event_window(&mut failures);
            let ambient_window_minutes = env::var("VIGIL_ACCEPTANCE_AMBIENT_MINUTES")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(30);
            if ambient_window_minutes < 30 {
                failures.push(format!(
                    "ambient window was {ambient_window_minutes} minutes; real-camera gate requires at least 30"
                ));
            }
            let Some((hard_start_epoch, hard_end_epoch)) = hard_window else {
                assert!(failures.is_empty(), "{}", failures.join("; "));
                return;
            };
            if !failures.is_empty() {
                assert!(failures.is_empty(), "{}", failures.join("; "));
                return;
            }
            let binary = VigilBinary::new();
            let data = TempDir::new().expect("tempdir");
            let store_path = data.path().join("store.contextgraph");
            let (_config_dir, config_path) =
                first_light_config(data.path(), &store_path, free_port(), &url).expect("config");
            let mut runtime_started = false;
            let mut runtime_logs = String::new();
            let mut trace_raw = String::new();

            match VigilProcess::spawn_network_traced(&binary, &config_path, free_port()) {
                Ok(mut process) => {
                    runtime_started = process
                        .health()
                        .wait_for_status(200, Duration::from_secs(10));
                    let _ = process.wait_for_log("decoded_frames=", Duration::from_secs(30));
                    thread::sleep(Duration::from_secs(ambient_window_minutes * 60));
                    runtime_logs = process.logs();
                    trace_raw = process.network_trace().raw;
                    let _ = process.terminate();
                }
                Err(error) => failures.push(format!("real-camera runtime did not start: {error}")),
            }

            let opened_real_rtsp = trace_connects_to_rtsp(&trace_raw, &url);
            let frames_received = parsed_log_counter(&runtime_logs, "decoded_frames")
                .or_else(|| {
                    let stats = command_text(
                        binary
                            .command()
                            .arg("stats")
                            .env("VIGIL_STORE_PATH", &store_path)
                            .env("VIGIL_DATA_DIR", data.path())
                            .output(),
                    );
                    parsed_stat(&stats, "frames-received").map(|value| value as u64)
                })
                .unwrap_or_default();
            let store = open_store(&store_path).ok();
            let observations = store
                .as_ref()
                .and_then(|store| store.list_observations(None).ok())
                .unwrap_or_default();
            let hard_events = observations
                .iter()
                .filter(|observation| {
                    observed_at_in_window(observation, hard_start_epoch, hard_end_epoch)
                })
                .collect::<Vec<_>>();
            let ambient_observation_count = observations.len().saturating_sub(hard_events.len());
            let hard_event_id = hard_events
                .first()
                .map(|observation| observation.id.to_string());
            let authority = hard_event_id
                .as_deref()
                .map(|id| exact_five_node_authority_for_observation(store.as_ref(), &url, id))
                .unwrap_or_else(|| Err("no hard-window Observation was available".to_string()));
            let hard_event_landed = hard_events
                .first()
                .and_then(|observation| first_clip_ref(observation))
                .map(|clip_ref| clip_ref_is_decodable(data.path(), clip_ref))
                .unwrap_or(false);
            for hard_event in &hard_events {
                assert_hard_detection_identity(hard_event, &mut failures);
            }
            drop(store);
            let stats_text = command_text(
                binary
                    .command()
                    .arg("stats")
                    .env("VIGIL_STORE_PATH", &store_path)
                    .env("VIGIL_DATA_DIR", data.path())
                    .output(),
            );
            let why_output = if let Some(event_id) = hard_event_id.as_deref() {
                binary
                    .command()
                    .arg("why")
                    .arg(event_id)
                    .env("VIGIL_STORE_PATH", &store_path)
                    .env("VIGIL_DATA_DIR", data.path())
                    .output()
            } else {
                binary
                    .command()
                    .arg("why")
                    .arg("missing-hard-event")
                    .env("VIGIL_STORE_PATH", &store_path)
                    .env("VIGIL_DATA_DIR", data.path())
                    .output()
            };
            let (why_success, why_text) = why_output
                .map(|output| {
                    let (stdout, stderr) = output_text(&output);
                    (output.status.success(), format!("{stdout}{stderr}"))
                })
                .unwrap_or_else(|error| (false, format!("could not run why: {error}")));
            let why_walked = why_success
                && authority
                    .as_ref()
                    .map(|authority| authority.matches_why_text(&why_text))
                    .unwrap_or(false);
            let false_positive_count = parsed_stat(&stats_text, "false-positive-count");
            let false_positive_window_minutes =
                parsed_stat(&stats_text, "false-positive-window-minutes");
            let keep_pace = matches!(
                (
                    parsed_stat(&stats_text, "dropped-motion-positive-frames"),
                    parsed_stat(&stats_text, "processing-lag-ms"),
                    parsed_stat(&stats_text, "processing-lag-bound-ms")
                ),
                (Some(0.0), Some(lag), Some(bound)) if lag <= bound
            );
            let latency_p50 = parsed_stat(&stats_text, "detector-latency-p50-ms");
            let latency_p95 = parsed_stat(&stats_text, "detector-latency-p95-ms");
            let latency_max = parsed_stat(&stats_text, "detector-latency-max-ms");
            let latency_reported = matches!(
                (latency_p50, latency_p95, latency_max),
                (Some(p50), Some(p95), Some(max))
                    if p50 > 0.0 && p50 <= p95 && p95 <= max
            );
            let disallowed_network = disallowed_network_lines(&trace_raw, Some(&url));

            if !runtime_started {
                failures.push("runtime did not reach ready state for real-camera run".to_string());
            }
            if !opened_real_rtsp {
                failures.push(format!("runtime did not open configured RTSP source {url}"));
            }
            if frames_received == 0 {
                failures.push("runtime consumed no frames from the real RTSP source".to_string());
            }
            if hard_events.len() != 1 {
                failures.push(format!(
                    "real-camera hard-event gate expected one Observation inside epoch window {hard_start_epoch}..={hard_end_epoch}, found {}",
                    hard_events.len()
                ));
            }
            if !hard_event_landed {
                failures
                    .push("hard real-camera event did not land with durable evidence".to_string());
            }
            if let Some(clip_ref) = hard_events.first().and_then(|event| first_clip_ref(event))
                && let Err(error) = clip_ref_rejects_committed_fixture_copy(data.path(), clip_ref)
            {
                failures.push(format!(
                    "hard real-camera event clip evidence was not live-camera-owned: {error}"
                ));
            }
            if let Err(error) = authority.as_ref() {
                failures.push(format!(
                    "hard real-camera event did not land exact cg authority: {error}"
                ));
            }
            if !why_walked {
                failures.push("review command did not walk the hard event by id".to_string());
            }
            if false_positive_count != Some(ambient_observation_count as f64) {
                failures.push(format!(
                    "stats false-positive-count {false_positive_count:?} did not equal cg ambient Observation count {ambient_observation_count}"
                ));
            }
            if !false_positive_window_minutes
                .map(|minutes| minutes >= ambient_window_minutes as f64)
                .unwrap_or(false)
            {
                failures
                    .push("ambient false-positive window was not bounded and recorded".to_string());
            }
            if !keep_pace {
                failures.push("pipeline did not report bounded backlog keep-pace".to_string());
            }
            if !latency_reported {
                failures.push(
                    "detector latency p50/p95/max was absent, zero, or unordered".to_string(),
                );
            }
            if !disallowed_network.is_empty() {
                failures.push(format!(
                    "real-camera run opened non-RTSP network sockets: {disallowed_network:?}"
                ));
            }
        }
        _ => {
            println!(
                "skipped-pending-source: set VIGIL_ACCEPTANCE_RTSP_URL to run the real-camera gate"
            );
            return;
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("; "));
}

fn open_store(path: &Path) -> Result<Store, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create store parent {}: {error}", parent.display()))?;
    }
    Store::open(StoreConfig {
        db_path: path.to_path_buf(),
        default_text_embedder: Some(EmbedderConfig::disabled()),
        ..StoreConfig::default()
    })
    .map_err(|error| format!("open store {}: {error}", path.display()))
}

struct AcceptanceAuthority {
    context_id: String,
    camera_id: String,
    camera_name: String,
    rtsp_url: String,
    observation_id: String,
    clip_ref: String,
    decision_id: String,
    intention_id: String,
    model_id: String,
}

impl AcceptanceAuthority {
    fn matches_why_text(&self, text: &str) -> bool {
        [
            self.context_id.as_str(),
            self.camera_id.as_str(),
            self.camera_name.as_str(),
            self.rtsp_url.as_str(),
            self.observation_id.as_str(),
            self.clip_ref.as_str(),
            self.decision_id.as_str(),
            self.intention_id.as_str(),
            self.model_id.as_str(),
        ]
        .into_iter()
        .all(|expected| text.contains(expected))
    }
}

fn exact_five_node_authority(
    store: Option<&Store>,
    rtsp_url: &str,
) -> Result<AcceptanceAuthority, String> {
    exact_five_node_authority_matching(store, rtsp_url, None)
}

fn exact_five_node_authority_for_observation(
    store: Option<&Store>,
    rtsp_url: &str,
    observation_id: &str,
) -> Result<AcceptanceAuthority, String> {
    exact_five_node_authority_matching(store, rtsp_url, Some(observation_id))
}

fn exact_five_node_authority_matching(
    store: Option<&Store>,
    rtsp_url: &str,
    observation_id: Option<&str>,
) -> Result<AcceptanceAuthority, String> {
    let store = store.ok_or_else(|| "store did not open after runtime exit".to_string())?;
    let contexts = store
        .list_contexts()
        .map_err(|error| format!("list contexts: {error}"))?;
    if contexts.len() != 1 {
        return Err(format!("expected one Context, found {}", contexts.len()));
    }
    let cameras = store
        .list_entities(ListEntityFilter {
            entity_type: Some(EntityType::Device),
            context_id: Some(contexts[0].id),
            ..ListEntityFilter::default()
        })
        .map_err(|error| format!("list camera entities: {error}"))?;
    if cameras.len() != 1 {
        return Err(format!(
            "expected one camera Entity, found {}",
            cameras.len()
        ));
    }
    let camera_rtsp = cameras[0]
        .properties
        .get("rtsp_url")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if camera_rtsp != rtsp_url {
        return Err(format!(
            "camera RTSP URL {camera_rtsp:?} did not equal configured {rtsp_url:?}"
        ));
    }
    let observations = store
        .list_observations(None)
        .map_err(|error| format!("list observations: {error}"))?;
    let observation = match observation_id {
        Some(expected_id) => observations
            .iter()
            .find(|observation| observation.id.to_string() == expected_id)
            .ok_or_else(|| format!("hard Observation {expected_id} did not resolve"))?,
        None => {
            if observations.len() != 1 {
                return Err(format!(
                    "expected one Observation, found {}",
                    observations.len()
                ));
            }
            &observations[0]
        }
    };
    if observation.entity_id != cameras[0].id || observation.context_id != contexts[0].id {
        return Err("Observation did not point at the camera Entity and site Context".to_string());
    }
    let clip_ref = observation
        .evidence
        .first()
        .map(|evidence| evidence.source_ref.clone())
        .ok_or_else(|| "Observation carried no clip evidence".to_string())?;
    let audit = store
        .audit_query(AuditFilter::default())
        .map_err(|error| format!("query audit: {error}"))?;
    let decision_ids = audit
        .iter()
        .filter_map(|entry| match entry.target {
            AuditTarget::Decision(id) => Some(id),
            _ => None,
        })
        .collect::<Vec<_>>();
    if decision_ids.len() != 1 {
        return Err(format!(
            "expected one Decision, found {}",
            decision_ids.len()
        ));
    }
    let decision: Decision = store
        .get_decision(decision_ids[0])
        .map_err(|error| format!("get decision: {error}"))?
        .ok_or_else(|| "Decision audit id did not resolve".to_string())?;
    let model_id = decision
        .properties
        .get("model_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "Decision did not carry model_id".to_string())?
        .to_string();
    if decision.intention_ids.len() != 1 {
        return Err(format!(
            "expected Decision to serve one Intention, found {}",
            decision.intention_ids.len()
        ));
    }
    let intention_ids = audit
        .iter()
        .filter_map(|entry| match entry.target {
            AuditTarget::Intention(id) => Some(id),
            _ => None,
        })
        .collect::<Vec<_>>();
    if intention_ids.len() != 1 || intention_ids[0] != decision.intention_ids[0] {
        return Err("Intention audit row did not match the detector Decision parent".to_string());
    }
    Ok(AcceptanceAuthority {
        context_id: contexts[0].id.to_string(),
        camera_id: cameras[0].id.to_string(),
        camera_name: cameras[0].name.clone(),
        rtsp_url: camera_rtsp.to_string(),
        observation_id: observation.id.to_string(),
        clip_ref,
        decision_id: decision.id.to_string(),
        intention_id: decision.intention_ids[0].to_string(),
        model_id,
    })
}

fn hard_event_window(failures: &mut Vec<String>) -> Option<(i64, i64)> {
    let start = required_epoch_env("VIGIL_ACCEPTANCE_HARD_EVENT_START_EPOCH", failures);
    let end = required_epoch_env("VIGIL_ACCEPTANCE_HARD_EVENT_END_EPOCH", failures);
    match (start, end) {
        (Some(start), Some(end)) if start < end => Some((start, end)),
        (Some(start), Some(end)) => {
            failures.push(format!(
                "hard-event epoch window was invalid: start {start} must be before end {end}"
            ));
            None
        }
        _ => None,
    }
}

fn required_epoch_env(key: &str, failures: &mut Vec<String>) -> Option<i64> {
    match env::var(key) {
        Ok(value) if !value.trim().is_empty() => match value.trim().parse::<i64>() {
            Ok(epoch) => Some(epoch),
            Err(error) => {
                failures.push(format!("{key} was not a unix epoch second: {error}"));
                None
            }
        },
        _ => {
            failures.push(format!(
                "{key} is required when VIGIL_ACCEPTANCE_RTSP_URL is set"
            ));
            None
        }
    }
}

fn observed_at_in_window(observation: &Observation, start_epoch: i64, end_epoch: i64) -> bool {
    let observed_epoch = observation.observed_at.timestamp();
    observed_epoch >= start_epoch && observed_epoch <= end_epoch
}

fn first_clip_ref(observation: &Observation) -> Option<&str> {
    observation
        .evidence
        .first()
        .map(|evidence| evidence.source_ref.as_str())
}

fn assert_hard_detection_identity(observation: &Observation, failures: &mut Vec<String>) {
    if observation
        .observed_properties
        .get("class")
        .and_then(|value| value.as_str())
        != Some("person")
    {
        failures.push("hard event did not carry class=person".to_string());
    }
    if !observation
        .observed_properties
        .get("confidence")
        .and_then(|value| value.as_f64())
        .map(|confidence| confidence > 0.0 && confidence <= 1.0)
        .unwrap_or(false)
    {
        failures.push("hard event did not carry a confidence in (0,1]".to_string());
    }
    if observation
        .properties
        .get("detector_model_sha256")
        .and_then(|value| value.as_str())
        .is_none_or(str::is_empty)
    {
        failures.push("hard event did not carry the detector model digest".to_string());
    }
    if observation
        .properties
        .get("detector_result_sha256")
        .and_then(|value| value.as_str())
        .is_none_or(str::is_empty)
    {
        failures.push("hard event did not carry the detector result digest".to_string());
    }
}

struct RtspFixture {
    url: String,
    mediamtx: Child,
    ffmpeg: Child,
    mediamtx_stdout: Arc<Mutex<String>>,
    mediamtx_stderr: Arc<Mutex<String>>,
    ffmpeg_stdout: Arc<Mutex<String>>,
    ffmpeg_stderr: Arc<Mutex<String>>,
}

impl RtspFixture {
    fn start() -> Result<Self, String> {
        let root = workspace_root();
        let clip = root
            .join("tests")
            .join("fixtures")
            .join("video")
            .join("one-by-one-person-detection.mp4");
        let mediamtx = tool_path("VIGIL_MEDIAMTX_BIN", "mediamtx")
            .ok_or_else(|| "mediamtx unavailable".to_string())?;
        let ffmpeg = tool_path("VIGIL_FFMPEG_BIN", "ffmpeg")
            .ok_or_else(|| "ffmpeg unavailable".to_string())?;
        let port = free_port();
        let rtp_port = free_port();
        let rtcp_port = free_port();
        let url = format!("rtsp://127.0.0.1:{port}/lower-gate");
        let config_dir = tempfile::tempdir().map_err(|error| format!("rtsp tempdir: {error}"))?;
        let config_path = config_dir.path().join("mediamtx.yml");
        fs::write(
            &config_path,
            format!(
                "rtspTransports: [tcp]\nrtspAddress: 127.0.0.1:{port}\nrtpAddress: 127.0.0.1:{rtp_port}\nrtcpAddress: 127.0.0.1:{rtcp_port}\nrtmp: no\nhls: no\nwebrtc: no\nsrt: no\nplayback: no\nmoq: no\npaths:\n  lower-gate:\n    source: publisher\n"
            ),
        )
        .map_err(|error| format!("write mediamtx config: {error}"))?;
        let mut mediamtx_child = Command::new(mediamtx)
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("spawn mediamtx: {error}"))?;
        let mediamtx_stdout = capture_pipe(mediamtx_child.stdout.take());
        let mediamtx_stderr = capture_pipe(mediamtx_child.stderr.take());
        if mediamtx_child.id() <= 1 {
            let _ = kill_tracked_child(&mut mediamtx_child, "invalid mediamtx child");
            return Err("mediamtx reported an invalid child pid".to_string());
        }
        if let Err(error) = wait_for_tcp_port(port, Duration::from_secs(3)) {
            let _ = kill_tracked_child(&mut mediamtx_child, "mediamtx startup failure");
            return Err(error);
        }
        let mut ffmpeg_command = Command::new(ffmpeg);
        let mut ffmpeg_child = match ffmpeg_command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-re")
            .arg("-stream_loop")
            .arg("-1")
            .arg("-i")
            .arg(&clip)
            .arg("-an")
            .arg("-c:v")
            .arg("copy")
            .arg("-f")
            .arg("rtsp")
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ =
                    kill_tracked_child(&mut mediamtx_child, "mediamtx after ffmpeg spawn failure");
                return Err(format!("spawn ffmpeg: {error}"));
            }
        };
        let ffmpeg_stdout = capture_pipe(ffmpeg_child.stdout.take());
        let ffmpeg_stderr = capture_pipe(ffmpeg_child.stderr.take());
        if ffmpeg_child.id() <= 1 {
            let _ = kill_tracked_child(&mut ffmpeg_child, "invalid ffmpeg child");
            let _ = kill_tracked_child(&mut mediamtx_child, "mediamtx after invalid ffmpeg child");
            return Err("ffmpeg reported an invalid child pid".to_string());
        }
        thread::sleep(Duration::from_millis(400));
        Ok(Self {
            url,
            mediamtx: mediamtx_child,
            ffmpeg: ffmpeg_child,
            mediamtx_stdout,
            mediamtx_stderr,
            ffmpeg_stdout,
            ffmpeg_stderr,
        })
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    fn logs(&self) -> String {
        let mediamtx_stdout = self
            .mediamtx_stdout
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        let mediamtx_stderr = self
            .mediamtx_stderr
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        let ffmpeg_stdout = self
            .ffmpeg_stdout
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        let ffmpeg_stderr = self
            .ffmpeg_stderr
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        format!("{mediamtx_stdout}{mediamtx_stderr}{ffmpeg_stdout}{ffmpeg_stderr}")
    }
}

impl Drop for RtspFixture {
    fn drop(&mut self) {
        let _ = kill_tracked_child(&mut self.ffmpeg, "ffmpeg RTSP publisher");
        let _ = kill_tracked_child(&mut self.mediamtx, "mediamtx RTSP server");
    }
}

fn wait_for_tcp_port(port: u16, timeout: Duration) -> Result<(), String> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(100),
        )
        .is_ok()
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!("mediamtx did not open TCP port {port}"))
}

fn capture_pipe<T>(pipe: Option<T>) -> Arc<Mutex<String>>
where
    T: Read + Send + 'static,
{
    let logs = Arc::new(Mutex::new(String::new()));
    if let Some(mut pipe) = pipe {
        let captured = Arc::clone(&logs);
        thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if let Ok(mut logs) = captured.lock() {
                            logs.push_str(&String::from_utf8_lossy(&buffer[..read]));
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    logs
}

fn first_light_config(
    data_dir: &Path,
    store_path: &Path,
    health_port: u16,
    rtsp_url: &str,
) -> std::io::Result<(TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("vigil.toml");
    let detector_model_path = workspace_root().join("tests/fixtures/models/yolox-tiny-coco.pth");
    let config = format!(
        "data_dir = \"{}\"\nstore_path = \"{}\"\nhealth_port = {}\nrtsp_url = \"{}\"\ndetector_model_path = \"{}\"\n",
        escape_toml_path(data_dir),
        escape_toml_path(store_path),
        health_port,
        rtsp_url,
        escape_toml_path(&detector_model_path),
    );
    fs::write(&config_path, config)?;
    Ok((dir, config_path))
}

fn parsed_log_counter(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (line_key, value) = line.split_once('=')?;
        (line_key.trim() == key).then(|| value.trim().parse::<u64>().ok())?
    })
}

fn trace_connects_to_rtsp(raw: &str, rtsp_url: &str) -> bool {
    let Some(endpoint) = RtspEndpoint::parse(rtsp_url) else {
        return false;
    };
    raw.lines()
        .any(|line| network_line_connects_to_endpoint(line, &endpoint))
}

fn disallowed_network_lines(raw: &str, allowed_rtsp_url: Option<&str>) -> Vec<String> {
    let allowed_endpoint = allowed_rtsp_url.and_then(RtspEndpoint::parse);
    raw.lines()
        .filter(|line| network_line_is_inet_event(line))
        .filter(|line| {
            !allowed_endpoint
                .as_ref()
                .map(|endpoint| network_line_connects_to_endpoint(line, endpoint))
                .unwrap_or(false)
        })
        .map(str::to_string)
        .collect()
}

fn network_line_is_inet_event(line: &str) -> bool {
    let traced_connect = line.contains("connect(") || line.contains("sendto(");
    let internet_socket = line.contains("AF_INET") || line.contains("AF_INET6");
    traced_connect && internet_socket
}

fn network_line_connects_to_endpoint(line: &str, endpoint: &RtspEndpoint) -> bool {
    line.contains("connect(")
        && line_contains_port(line, endpoint.port)
        && endpoint
            .addresses
            .iter()
            .any(|address| line.contains(address))
}

fn line_contains_port(line: &str, port: u16) -> bool {
    line.contains(&format!("sin_port=htons({port})"))
        || line.contains(&format!("sin6_port=htons({port})"))
}

struct RtspEndpoint {
    port: u16,
    addresses: BTreeSet<String>,
}

impl RtspEndpoint {
    fn parse(rtsp_url: &str) -> Option<Self> {
        let after_scheme = rtsp_url.strip_prefix("rtsp://")?;
        let authority = after_scheme.split('/').next()?.rsplit('@').next()?;
        let (host, port) = split_host_port(authority)?;
        let addresses = (host.as_str(), port)
            .to_socket_addrs()
            .ok()?
            .map(|address| address.ip().to_string())
            .collect::<BTreeSet<_>>();
        (!addresses.is_empty()).then_some(Self { port, addresses })
    }
}

fn split_host_port(authority: &str) -> Option<(String, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after_host) = rest.split_once(']')?;
        let port = after_host
            .strip_prefix(':')
            .and_then(|port| port.parse().ok())
            .unwrap_or(554);
        return Some((host.to_string(), port));
    }
    if let Some((host, port)) = authority.rsplit_once(':') {
        return Some((host.to_string(), port.parse().ok()?));
    }
    Some((authority.to_string(), 554))
}

fn clip_ref_is_decodable(data_dir: &Path, source_ref: &str) -> bool {
    resolve_clip_path(data_dir, source_ref)
        .filter(|path| path.is_file())
        .and_then(|path| count_video_frames(&path).ok())
        .map(|frames| frames > 1)
        .unwrap_or(false)
}

fn clip_ref_matches_stream_fixture(
    data_dir: &Path,
    source_ref: &str,
    source_clip: &Path,
) -> Result<(), String> {
    clip_ref_rejects_committed_fixture_copy(data_dir, source_ref)?;
    let clip_path = resolve_clip_path(data_dir, source_ref)
        .ok_or_else(|| format!("clip ref {source_ref} did not resolve to a local path"))?;
    let source_frames = count_video_frames(source_clip)?;
    let clip_frames = count_video_frames(&clip_path)?;
    if clip_frames >= source_frames {
        return Err(format!(
            "{} contains the full source fixture instead of an event segment",
            clip_path.display()
        ));
    }
    let source_fingerprints = video_frame_fingerprints(source_clip)?;
    let clip_fingerprints = video_frame_fingerprints(&clip_path)?;
    if source_fingerprints
        .intersection(&clip_fingerprints)
        .next()
        .is_none()
    {
        return Err(format!(
            "{} did not share decoded frames with the RTSP fixture source",
            clip_path.display()
        ));
    }
    Ok(())
}

fn clip_ref_rejects_committed_fixture_copy(
    data_dir: &Path,
    source_ref: &str,
) -> Result<(), String> {
    let clip_path = resolve_clip_path(data_dir, source_ref)
        .ok_or_else(|| format!("clip ref {source_ref} did not resolve to a local path"))?;
    if !clip_path.is_file() {
        return Err(format!("{} is not a local clip file", clip_path.display()));
    }
    if count_video_frames(&clip_path)? <= 1 {
        return Err(format!(
            "{} did not decode into >1 frames",
            clip_path.display()
        ));
    }
    let clip_bytes =
        fs::read(&clip_path).map_err(|error| format!("read {}: {error}", clip_path.display()))?;
    for fixture in [
        workspace_root().join("tests/fixtures/video/one-by-one-person-detection.mp4"),
        workspace_root()
            .join("tests/fixtures/video/empty-scene-from-one-by-one-person-detection.mp4"),
    ] {
        if fixture.is_file()
            && fs::read(&fixture)
                .map(|bytes| bytes == clip_bytes)
                .unwrap_or(false)
        {
            return Err(format!(
                "{} is a byte-for-byte committed fixture copy",
                clip_path.display()
            ));
        }
    }
    Ok(())
}

fn video_frame_fingerprints(path: &Path) -> Result<BTreeSet<Vec<u8>>, String> {
    const WIDTH: usize = 16;
    const HEIGHT: usize = 16;
    let ffmpeg =
        tool_path("VIGIL_FFMPEG_BIN", "ffmpeg").ok_or_else(|| "ffmpeg unavailable".to_string())?;
    let output = Command::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(path)
        .arg("-vf")
        .arg(format!("scale={WIDTH}:{HEIGHT},format=gray"))
        .arg("-f")
        .arg("rawvideo")
        .arg("-")
        .output()
        .map_err(|error| format!("fingerprint frames in {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "fingerprint frames in {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let frame_size = WIDTH * HEIGHT;
    let fingerprints = output
        .stdout
        .chunks_exact(frame_size)
        .map(average_hash_frame)
        .collect::<BTreeSet<_>>();
    if fingerprints.is_empty() {
        return Err(format!("{} produced no frame fingerprints", path.display()));
    }
    Ok(fingerprints)
}

fn average_hash_frame(frame: &[u8]) -> Vec<u8> {
    let average = frame.iter().map(|value| *value as u64).sum::<u64>() / frame.len() as u64;
    frame
        .iter()
        .map(|value| if *value as u64 >= average { b'1' } else { b'0' })
        .collect()
}

fn resolve_clip_path(data_dir: &Path, source_ref: &str) -> Option<PathBuf> {
    if let Some(rest) = source_ref.strip_prefix("file://") {
        return Some(PathBuf::from(rest));
    }
    source_ref
        .strip_prefix("vigil-edge:clip/")
        .map(|rest| data_dir.join("clips").join(rest))
}

fn count_video_frames(path: &Path) -> Result<u64, String> {
    let ffprobe = tool_path("VIGIL_FFPROBE_BIN", "ffprobe")
        .ok_or_else(|| "ffprobe unavailable".to_string())?;
    let output = Command::new(ffprobe)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-count_frames")
        .arg("-show_entries")
        .arg("stream=nb_read_frames")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(path)
        .output()
        .map_err(|error| format!("run ffprobe for {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().parse::<u64>().ok())
        .ok_or_else(|| "ffprobe returned no frame count".to_string())
}

fn parsed_stat(text: &str, key: &str) -> Option<f64> {
    text.lines().find_map(|line| {
        let (line_key, value) = line.split_once('=')?;
        (line_key.trim() == key).then(|| value.trim().parse::<f64>().ok())?
    })
}

fn command_text(output: std::io::Result<std::process::Output>) -> String {
    output
        .map(|output| {
            let (stdout, stderr) = output_text(&output);
            format!("{stdout}{stderr}")
        })
        .unwrap_or_default()
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn tool_path(env_key: &str, binary: &str) -> Option<PathBuf> {
    if let Some(path) = env::var_os(env_key).map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    env::var_os("PATH")
        .and_then(|path| {
            env::split_paths(&path)
                .map(|dir| dir.join(binary))
                .find(|candidate| candidate.is_file())
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

fn escape_toml_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

fn last_log_lines(logs: &str, count: usize) -> String {
    let lines = logs.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(count);
    lines[start..].join(" | ")
}
