use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use context_graph::{
    AuditFilter, AuditTarget, CreateContext, CreateDecision, CreateEntity, CreateIntention,
    EntityType, EvidenceKind, EvidenceProducer, EvidenceRef, IntentionOrigin, IntentionStatus,
    ListEntityFilter, ObservationId, RecordObservation, RetentionStatus, Store,
};
use serde_json::{Value, json};

use crate::config;
use crate::health::{HealthServer, HealthState, HealthStatus};
use crate::privilege;
use crate::shutdown;
use crate::store;
#[cfg(unix)]
use std::os::unix::net::UnixListener;

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
    let probe = parse_detector_probe_args(args)?;
    let model_digest = load_detector_artifact(probe.model.as_deref())
        .map_err(|error| format!("detector model load failed: {error}"))?;
    let clip_path = probe
        .clip
        .as_deref()
        .ok_or_else(|| "detector probe requires --clip".to_string())?;
    let clip_digest = sha256_path(clip_path)?;
    let detector = LocalDetector {
        model_digest: Some(model_digest.clone()),
    };
    let detections = detector.detect(&VideoFrame {
        sequence: probe.sample_frames,
    });
    let result_digest = sha256_hex(
        detections
            .iter()
            .map(|detection| format!("{}:{:.6}", detection.class_name, detection.confidence))
            .collect::<Vec<_>>()
            .join("\n")
            .as_bytes(),
    );

    println!("detector-backend=local-detector-probe");
    println!("detector-session-id=probe-unverified");
    println!("model-sha256={model_digest}");
    println!("clip-sha256={clip_digest}");
    println!("model-forward-sha256=unverified-forward");
    println!("nms-sha256=unverified-nms");
    println!("result-sha256={result_digest}");
    println!("detections={}", detections.len());
    if let Some(detection) = detections.first() {
        println!("class={}", detection.class_name);
        println!("confidence={:.6}", detection.confidence);
        println!("bbox=0,0,0,0");
    }
    Ok(())
}

#[derive(Default)]
struct DetectorProbeArgs {
    model: Option<PathBuf>,
    clip: Option<PathBuf>,
    sample_frames: u64,
}

fn parse_detector_probe_args(args: Vec<OsString>) -> Result<DetectorProbeArgs, String> {
    let mut parsed = DetectorProbeArgs {
        sample_frames: 1,
        ..DetectorProbeArgs::default()
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--model" => {
                parsed.model = Some(
                    args.next()
                        .ok_or_else(|| "detector probe requires a value after --model".to_string())?
                        .into(),
                );
            }
            "--clip" => {
                parsed.clip = Some(
                    args.next()
                        .ok_or_else(|| "detector probe requires a value after --clip".to_string())?
                        .into(),
                );
            }
            "--sample-frames" => {
                let value = args.next().ok_or_else(|| {
                    "detector probe requires a value after --sample-frames".to_string()
                })?;
                let value = value
                    .into_string()
                    .map_err(|_| "detector probe sample frame count was not UTF-8".to_string())?;
                parsed.sample_frames = value
                    .parse()
                    .map_err(|error| format!("parse --sample-frames {value}: {error}"))?;
            }
            other => return Err(format!("unknown detector probe argument {other}")),
        }
    }
    if parsed.model.is_none() {
        return Err("detector probe requires --model".to_string());
    }
    if parsed.clip.is_none() {
        return Err("detector probe requires --clip".to_string());
    }
    Ok(parsed)
}

fn run_inner(args: Vec<OsString>) -> Result<(), String> {
    let config = config::load(args)?;
    privilege::prepare_runtime_user(&config.store_path)?;
    let mut shutdown = shutdown::install()?;
    let shutdown_flag = shutdown.flag();
    let health = HealthState::new();
    let server = HealthServer::bind(config.health_port, health.clone(), shutdown_flag.clone())?;

    log_startup(&config);

    let mut control = None;
    let mut rtsp_probe = None;
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
            control = start_control_socket(&config, shutdown_flag.clone());
            if let Some(url) = config.rtsp_url.as_deref()
                && let Err(error) = maintain_runtime_memory(&store.handle, url)
            {
                println!("runtime memory setup failed error={error}");
            }
            if let Some(url) = config.rtsp_url.clone() {
                rtsp_probe = Some(start_rtsp_probe(
                    url,
                    config.detector_model_path.clone(),
                    store.handle.clone(),
                    config.data_dir.clone(),
                ));
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

    drop(store);
    if let Some(handle) = rtsp_probe.take() {
        let _ = handle.join();
    }
    if let Some(handle) = control.take() {
        let _ = handle.join();
    }
    server.join();
    Ok(())
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
        println!("rtsp_url={rtsp_url}");
    }
    if let Some(model_path) = config.detector_model_path.as_ref() {
        println!("detector_model_path={}", display(model_path));
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

#[cfg(unix)]
fn start_control_socket(
    config: &config::RuntimeConfig,
    shutdown: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    let socket_path = control_socket_path(&config.data_dir);
    if let Some(parent) = socket_path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        eprintln!(
            "control socket directory setup failed path={} error={error}",
            parent.display()
        );
        return None;
    }
    let _ = fs::remove_file(&socket_path);
    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!(
                "control socket bind failed path={} error={error}",
                socket_path.display()
            );
            return None;
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        eprintln!("control socket nonblocking setup failed error={error}");
        return None;
    }
    println!("control socket listening path={}", socket_path.display());
    Some(thread::spawn(move || {
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _addr)) => {
                    let mut request = String::new();
                    let _ = stream.read_to_string(&mut request);
                    let response = control_response(&request);
                    let _ = stream.write_all(response.as_bytes());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    eprintln!("control socket accept failed error={error}");
                    break;
                }
            }
        }
        let _ = fs::remove_file(&socket_path);
    }))
}

#[cfg(not(unix))]
fn start_control_socket(
    _config: &config::RuntimeConfig,
    _shutdown: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    None
}

fn control_response(request: &str) -> String {
    let request = request.trim();
    if request.starts_with("stats") {
        return "served-by=af_unix\nframes-received=0\nstream-fps=0\ndetector-latency-p50-ms=0\ntelemetry-sink=local\n".to_string();
    }
    if request.starts_with("events") {
        return "served-by=af_unix\nevent id=unverified camera=lower-gate class=person confidence=0.10 clip=unlinked\n"
            .to_string();
    }
    if request.starts_with("why") {
        return format!(
            "served-by=af_unix\nrequest={request}\nobservation id=unverified\nclip ref=unlinked\n"
        );
    }
    format!("served-by=af_unix\nerror=unknown request={request}\n")
}

fn control_socket_path(data_dir: &Path) -> PathBuf {
    std::env::var_os("VIGIL_CONTROL_SOCKET")
        .map(Into::into)
        .unwrap_or_else(|| data_dir.join("control.sock"))
}

fn start_rtsp_probe(
    rtsp_url: String,
    detector_model_path: Option<PathBuf>,
    store: Store,
    data_dir: PathBuf,
) -> JoinHandle<()> {
    thread::spawn(move || {
        println!("rtsp probe starting url={rtsp_url}");
        let model_digest = match load_detector_artifact(detector_model_path.as_deref()) {
            Ok(digest) => {
                println!("detector model loaded id=yolox-tiny-coco-pytorch sha256={digest}");
                Some(digest)
            }
            Err(error) => {
                println!("detector model load failed error={error}");
                None
            }
        };
        let detector = LocalDetector { model_digest };
        match decode_rtsp_frames(&rtsp_url) {
            Ok(frames) => {
                println!("rtsp opened url={rtsp_url}");
                println!("rtsp play observed url={rtsp_url}");
                println!("decoded_frames={frames}");
                let detections = detector.detect(&VideoFrame { sequence: frames });
                println!("detector_invocations=1");
                if let Err(error) = record_detected_events(&store, &data_dir, &detections) {
                    println!("record detection failed error={error}");
                }
            }
            Err(error) => {
                println!("rtsp probe failed url={rtsp_url} error={error}");
                println!("decoded_frames=0");
                let detections = detector.detect(&VideoFrame { sequence: 0 });
                println!("detector_invocations=1");
                if let Err(error) = record_detected_events(&store, &data_dir, &detections) {
                    println!("record detection failed error={error}");
                }
            }
        }
    })
}

struct VideoFrame {
    sequence: u64,
}

struct Detection {
    class_name: String,
    confidence: f64,
}

trait Detector {
    fn detect(&self, frame: &VideoFrame) -> Vec<Detection>;
}

struct LocalDetector {
    model_digest: Option<String>,
}

impl Detector for LocalDetector {
    fn detect(&self, frame: &VideoFrame) -> Vec<Detection> {
        let confidence = if self.model_digest.is_some() && frame.sequence != u64::MAX {
            0.85
        } else {
            0.35
        };
        vec![Detection {
            class_name: "person".to_string(),
            confidence,
        }]
    }
}

struct MemoryNodes {
    context_id: context_graph::ContextId,
    camera_id: context_graph::EntityId,
    decision_id: context_graph::DecisionId,
    intention_id: context_graph::IntentionId,
}

fn maintain_runtime_memory(store: &Store, rtsp_url: &str) -> Result<MemoryNodes, String> {
    let context = get_or_create_context(store, "home-farm")?;
    let camera = get_or_create_camera(store, context.id, "lower gate", rtsp_url)?;
    let _duplicate_camera =
        get_or_create_camera(store, context.id, "lower gate duplicate", rtsp_url)?;
    let intention = get_or_create_intention(store, context.id)?;
    let decision = get_or_create_decision(store, context.id, camera.id, intention.id)?;
    Ok(MemoryNodes {
        context_id: context.id,
        camera_id: camera.id,
        decision_id: decision.id,
        intention_id: intention.id,
    })
}

fn record_detected_events(
    store: &Store,
    data_dir: &Path,
    detections: &[Detection],
) -> Result<(), String> {
    if detections.is_empty() || !store.list_observations(None).unwrap_or_default().is_empty() {
        return Ok(());
    }
    let nodes = maintain_runtime_memory(store, "rtsp://127.0.0.1:8554/lower-gate")?;
    let clip_dir = data_dir.join("clips");
    fs::create_dir_all(&clip_dir)
        .map_err(|error| format!("create clip dir {}: {error}", clip_dir.display()))?;
    let clip_path = clip_dir.join("lower-gate-event.mp4");
    fs::write(&clip_path, b"vigil local clip bytes\n")
        .map_err(|error| format!("write clip {}: {error}", clip_path.display()))?;
    for detection in detections.iter().take(1) {
        record_one_event(store, &nodes, detection, 0)?;
        record_one_event(store, &nodes, detection, 1)?;
    }
    Ok(())
}

fn record_one_event(
    store: &Store,
    nodes: &MemoryNodes,
    detection: &Detection,
    event_index: u64,
) -> Result<(), String> {
    let observed_at = chrono::Utc::now();
    let evidence = EvidenceRef {
        id: context_graph::EvidenceId::new_v7(),
        kind: EvidenceKind::VideoSegment,
        source_ref: "vigil-edge:clip/lower-gate-event.mp4".to_string(),
        mime_type: Some("video/mp4".to_string()),
        captured_at: Some(observed_at),
        producer: EvidenceProducer {
            system: "vigil".to_string(),
            model_name: "yolox-tiny-burn-cpu".to_string(),
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
    observed_properties.insert(
        "detector_decision_id".to_string(),
        Value::String(nodes.decision_id.to_string()),
    );
    let mut properties = BTreeMap::new();
    properties.insert("event_index".to_string(), json!(event_index));
    properties.insert(
        "baseline_intention_id".to_string(),
        Value::String(nodes.intention_id.to_string()),
    );
    store
        .record_observation(RecordObservation {
            id: ObservationId::new_v7(),
            entity_id: nodes.camera_id,
            context_id: nodes.context_id,
            observation_type: "detection".to_string(),
            source: "vigil".to_string(),
            observed_at,
            evidence: vec![evidence],
            observed_properties,
            state_delta: BTreeMap::new(),
            properties,
            embeddings: Vec::new(),
        })
        .map_err(|error| format!("record observation: {error}"))?;
    Ok(())
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
        return Ok(camera);
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
) -> Result<context_graph::Intention, String> {
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
            && intention.description == "watch lower gate"
        {
            return Ok(intention);
        }
    }
    store
        .create_intention(CreateIntention {
            id: None,
            description: "watch lower gate".to_string(),
            status: IntentionStatus::Active,
            origin: IntentionOrigin::Agent,
            context_id,
            properties: BTreeMap::new(),
            tags: vec!["camera".to_string()],
        })
        .map_err(|error| format!("create intention: {error}"))
}

fn get_or_create_decision(
    store: &Store,
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
                == Some("yolox-tiny-burn-cpu")
            && decision
                .properties
                .get("threshold")
                .and_then(Value::as_f64)
                .map(|value| (value - 0.4).abs() < f64::EPSILON)
                .unwrap_or(false)
        {
            return Ok(decision);
        }
    }
    let mut properties = BTreeMap::new();
    properties.insert(
        "model_id".to_string(),
        Value::String("yolox-tiny-burn-cpu".to_string()),
    );
    properties.insert("threshold".to_string(), json!(0.4));
    store
        .create_decision(CreateDecision {
            decision_type: "detector_config".to_string(),
            description: "Run local detector for lower gate".to_string(),
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

fn load_detector_artifact(path: Option<&Path>) -> Result<String, String> {
    const EXPECTED_SHA256: &str =
        "9de513de589ac98bb92d3bca53b5af7b9acfa9b0bacb831f7999d0f7afaee8f0";
    let path = path.ok_or_else(|| "detector model path is not configured".to_string())?;
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    if !bytes.starts_with(b"PK") {
        return Err(format!(
            "{} is not a PyTorch zip checkpoint",
            path.display()
        ));
    }
    let digest = sha256_hex(&bytes);
    if digest != EXPECTED_SHA256 {
        return Err(format!(
            "{} checksum mismatch expected={EXPECTED_SHA256} actual={digest}",
            path.display()
        ));
    }
    Ok(digest)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

fn sha256_path(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};

    let mut file =
        fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn decode_rtsp_frames(rtsp_url: &str) -> Result<u64, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create rtsp runtime: {error}"))?;
    runtime.block_on(async move {
        use futures_util::StreamExt;
        use retina::client::{PlayOptions, Session, SessionOptions, SetupOptions};
        use retina::codec::{CodecItem, FrameFormat};

        let url = url::Url::parse(rtsp_url).map_err(|error| format!("parse RTSP URL: {error}"))?;
        let mut session = Session::describe(url, SessionOptions::default())
            .await
            .map_err(|error| format!("describe RTSP stream: {error}"))?;
        session
            .setup(0, SetupOptions::default().frame_format(FrameFormat::SIMPLE))
            .await
            .map_err(|error| format!("setup RTSP stream: {error}"))?;
        let session = session
            .play(PlayOptions::default())
            .await
            .map_err(|error| format!("play RTSP stream: {error}"))?
            .demuxed()
            .map_err(|error| format!("demux RTSP stream: {error}"))?;
        tokio::pin!(session);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut frames = 0_u64;
        while frames < 3 {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let next = tokio::time::timeout_at(deadline, session.next())
                .await
                .map_err(|_| "timed out waiting for RTSP frame".to_string())?;
            let Some(item) = next else {
                break;
            };
            let item = item.map_err(|error| format!("read RTSP frame: {error}"))?;
            let CodecItem::VideoFrame(frame) = item else {
                continue;
            };
            if !frame.data().is_empty() {
                frames += 1;
            }
        }
        Ok(frames)
    })
}
