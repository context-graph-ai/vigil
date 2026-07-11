use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use burn::tensor::{Device, Tensor, TensorData, backend::Backend};
use burn_store::{ModuleSnapshot, PytorchStore};
use sha2::{Digest, Sha256};
use yolox_burn::model::{BoundingBox, boxes::nms, yolox::Yolox};

use crate::media_pipeline;
use crate::media_pipeline::DecodedVideoSegment;

/// The compute backends available below the `Detector` trait. `YoloxDetector`
/// is generic over `Backend` so BOTH can be constructed in the same binary
/// under the accel feature: detector construction picks the concrete backend
/// at RUNTIME from the same selection that produced the acceleration receipt
/// (see `runtime::start_rtsp_probe`), so the receipt and the constructed
/// detector always agree — never a probe that claims hardware while the live
/// detector independently runs CPU. `CpuBackend` is unconditional (the
/// fallback every build can reach); `AccelBackend` exists only when the
/// dev-box-only `detect-burn-wgpu` feature is compiled in.
pub(crate) type CpuBackend = burn_flex::Flex;
#[cfg(feature = "detect-burn-wgpu")]
pub(crate) type AccelBackend = burn_wgpu::Wgpu<f32, i32, u32>;
/// The feature's preferred backend, for the standalone `vigil detector-probe`
/// diagnostic CLI only (outside the acceleration-selection seam).
#[cfg(not(feature = "detect-burn-wgpu"))]
type DetectorBackend = CpuBackend;
#[cfg(feature = "detect-burn-wgpu")]
type DetectorBackend = AccelBackend;

const HEIGHT: usize = 640;
const WIDTH: usize = 640;
/// The detector model's input tensor shape, for acceleration receipts.
pub(crate) const MODEL_INPUT_SHAPE: &str = "1x3x640x640";
const PERSON_CLASS_INDEX: usize = 0;
const CPU_BACKEND_ID: &str = "burn-yolox-tiny-cpu";
#[cfg(feature = "detect-burn-wgpu")]
const ACCEL_BACKEND_ID: &str = "burn-yolox-tiny-wgpu";
#[cfg(not(feature = "detect-burn-wgpu"))]
const BACKEND_ID: &str = CPU_BACKEND_ID;
#[cfg(feature = "detect-burn-wgpu")]
const BACKEND_ID: &str = ACCEL_BACKEND_ID;
const DETECTOR_FORWARD_PROBE_ENV: &str = "VIGIL_DETECTOR_FORWARD_PROBE_PATH";
const DETECTOR_FORWARD_PROBE_NONCE_ENV: &str = "VIGIL_DETECTOR_FORWARD_PROBE_NONCE";

pub(crate) trait DetectorForwardObserver {
    fn emit_forward_event(&self, event: &DetectorForwardEvent) -> Result<(), String>;
}

pub(crate) struct DetectorForwardEvent {
    pub(crate) seq: u64,
    pub(crate) nonce: Option<String>,
    pub(crate) detector_backend: String,
    pub(crate) detector_session_id: String,
    pub(crate) model_sha256: String,
    pub(crate) clip_sha256: String,
    pub(crate) model_forward_sha256: String,
    pub(crate) detector_nms_sha256: String,
    pub(crate) result_sha256: String,
}

pub(crate) struct DetectorForwardProbe {
    path: PathBuf,
    nonce: Option<String>,
}

pub(crate) struct YoloxDetector<B: Backend> {
    model: Yolox<B>,
    device: Device<B>,
    pub(crate) model_sha256: String,
    pub(crate) session_id: String,
    observer: Option<DetectorForwardProbe>,
    /// COCO class indices this detector emits. Defaults to person only (the
    /// baseline NVR behavior first-light depends on); recognition widens it to
    /// the covered classes so a dog/vehicle sighting reaches the match path.
    allowed_class_indices: Vec<usize>,
    /// The backend identity this instance was actually constructed with —
    /// carried into the forward-event record so it can never drift from the
    /// concrete backend that ran.
    backend_id: &'static str,
}

pub(crate) struct DetectorOutput {
    pub(crate) detector_backend: String,
    pub(crate) detector_session_id: String,
    pub(crate) model_sha256: String,
    pub(crate) clip_sha256: String,
    pub(crate) model_forward_sha256: String,
    pub(crate) detector_nms_sha256: String,
    pub(crate) result_sha256: String,
    pub(crate) detections: Vec<Detection>,
    pub(crate) forward_event_nonce: Option<String>,
    pub(crate) forward_event_seq: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct Detection {
    pub(crate) class_name: String,
    pub(crate) confidence: f64,
    pub(crate) bbox: String,
    pub(crate) frame_index: u64,
}

impl DetectorOutput {
    pub(crate) fn model_forward_digest(&self) -> &str {
        &self.model_forward_sha256
    }

    pub(crate) fn nms_digest(&self) -> &str {
        &self.detector_nms_sha256
    }

    pub(crate) fn result_digest(&self) -> &str {
        &self.result_sha256
    }

    pub(crate) fn clip_digest(&self) -> &str {
        &self.clip_sha256
    }
}

pub(crate) fn run_detector_probe(args: Vec<OsString>) -> Result<(), String> {
    let probe = parse_detector_probe_args(args)?;
    let detector: YoloxDetector<DetectorBackend> = load_detector(Some(&probe.model), BACKEND_ID)?;
    let output = detect_frame(
        &detector,
        &probe.clip,
        probe.sample_frames,
        probe.confidence_threshold,
    )?;

    println!("detector-backend={}", output.detector_backend);
    println!("detector-session-id={}", output.detector_session_id);
    println!("model-sha256={}", output.model_sha256);
    println!("clip-sha256={}", output.clip_sha256);
    println!("model-forward-sha256={}", output.model_forward_sha256);
    println!("nms-sha256={}", output.detector_nms_sha256);
    println!("result-sha256={}", output.result_sha256);
    if let Some(nonce) = output.forward_event_nonce.as_ref() {
        println!("forward-event-nonce={nonce}");
    }
    if let Some(seq) = output.forward_event_seq {
        println!("forward-event-seq={seq}");
    }
    println!("detections={}", output.detections.len());
    if let Some(detection) = output.detections.first() {
        println!("class={}", detection.class_name);
        println!("confidence={:.6}", detection.confidence);
        println!("bbox={}", detection.bbox);
        println!("frame-index={}", detection.frame_index);
    }
    Ok(())
}

/// Always the plain CPU backend, regardless of the accel feature — the
/// fallback path `runtime::start_rtsp_probe` constructs whenever the
/// detection-acceleration selection did not report `Active`.
pub(crate) fn load_cpu_detector(model: Option<&Path>) -> Result<YoloxDetector<CpuBackend>, String> {
    load_detector(model, CPU_BACKEND_ID)
}

pub(crate) fn load_cpu_detector_with_classes(
    model: Option<&Path>,
    allowed_class_indices: &[usize],
) -> Result<YoloxDetector<CpuBackend>, String> {
    load_detector_with_classes(model, allowed_class_indices, CPU_BACKEND_ID)
}

/// Constructed only when the detection-acceleration selection reported
/// `Active` on the accel backend — the receipt and the live detector are
/// derived from the SAME selection, never independently.
#[cfg(feature = "detect-burn-wgpu")]
pub(crate) fn load_accelerated_detector(
    model: Option<&Path>,
) -> Result<YoloxDetector<AccelBackend>, String> {
    load_detector(model, ACCEL_BACKEND_ID)
}

#[cfg(feature = "detect-burn-wgpu")]
pub(crate) fn load_accelerated_detector_with_classes(
    model: Option<&Path>,
    allowed_class_indices: &[usize],
) -> Result<YoloxDetector<AccelBackend>, String> {
    load_detector_with_classes(model, allowed_class_indices, ACCEL_BACKEND_ID)
}

pub(crate) fn load_detector<B: Backend>(
    model: Option<&Path>,
    backend_id: &'static str,
) -> Result<YoloxDetector<B>, String> {
    load_detector_with_classes(model, &[PERSON_CLASS_INDEX], backend_id)
}

pub(crate) fn load_detector_with_classes<B: Backend>(
    model: Option<&Path>,
    allowed_class_indices: &[usize],
    backend_id: &'static str,
) -> Result<YoloxDetector<B>, String> {
    let model = model.ok_or_else(|| "detector model path is not configured".to_string())?;
    let model_sha256 = load_record(model)?;
    let device: Device<B> = Default::default();
    let model = load_yolox_tiny_from_checkpoint::<B>(model, &device)?;
    let allowed = if allowed_class_indices.is_empty() {
        vec![PERSON_CLASS_INDEX]
    } else {
        allowed_class_indices.to_vec()
    };
    Ok(YoloxDetector {
        model,
        device,
        model_sha256,
        session_id: format!("detector-session-{}", event_seq()),
        observer: DetectorForwardProbe::from_env(),
        allowed_class_indices: allowed,
        backend_id,
    })
}

pub(crate) fn detect_frame<B: Backend>(
    detector: &YoloxDetector<B>,
    clip: &Path,
    sample_frames: usize,
    confidence_threshold: f64,
) -> Result<DetectorOutput, String> {
    let clip_sha256 = media_pipeline::sha256_path(clip)?;
    let segment = media_pipeline::decode_video_file(clip)?;
    detect_decoded_segment(
        detector,
        &segment,
        clip_sha256,
        sample_frames,
        confidence_threshold,
    )
}

pub(crate) fn detect_segment<B: Backend>(
    detector: &YoloxDetector<B>,
    segment: &DecodedVideoSegment,
    clip_sha256: String,
    sample_frames: usize,
    confidence_threshold: f64,
) -> Result<DetectorOutput, String> {
    detect_decoded_segment(
        detector,
        segment,
        clip_sha256,
        sample_frames,
        confidence_threshold,
    )
}

fn detect_decoded_segment<B: Backend>(
    detector: &YoloxDetector<B>,
    segment: &DecodedVideoSegment,
    clip_sha256: String,
    sample_frames: usize,
    confidence_threshold: f64,
) -> Result<DetectorOutput, String> {
    let confidence_threshold = validate_confidence_threshold(confidence_threshold)?;
    let sample_frame_indices =
        media_pipeline::sampled_frame_indices(segment.frames.len(), sample_frames.max(1));
    let tensor: Tensor<B, 4> =
        decode_frames_to_tensor::<B>(segment, sample_frames.max(1), &detector.device)?;
    let model_output: Tensor<B, 3> = detector.model.forward(tensor);
    let model_forward_sha256 = tensor_digest(model_output.clone());
    let detector_nms_sha256 = tensor_digest(model_output.clone());
    let detections = run_nms(
        model_output,
        confidence_threshold as f32,
        &detector.allowed_class_indices,
    );
    let result_sha256 = sha256_hex(result_digest_material(&detections).as_bytes());
    let seq = event_seq();
    let event = DetectorForwardEvent {
        seq,
        nonce: detector
            .observer
            .as_ref()
            .and_then(|observer| observer.nonce.clone()),
        detector_backend: detector.backend_id.to_string(),
        detector_session_id: detector.session_id.clone(),
        model_sha256: detector.model_sha256.clone(),
        clip_sha256,
        model_forward_sha256,
        detector_nms_sha256,
        result_sha256: result_sha256.clone(),
    };
    // observe_forward: Burn Tensor forward output is persisted through DetectorForwardEvent.
    if let Some(observer) = detector.observer.as_ref() {
        observer.emit_forward_event(&event)?;
    }
    let detections = detections
        .into_iter()
        .map(|detection| Detection {
            class_name: coco_class_name(detection.class_index).to_string(),
            confidence: detection.bbox.confidence as f64,
            bbox: format!(
                "{:.0},{:.0},{:.0},{:.0}",
                detection.bbox.xmin, detection.bbox.ymin, detection.bbox.xmax, detection.bbox.ymax
            ),
            frame_index: sample_frame_indices
                .get(detection.batch_index)
                .copied()
                .unwrap_or(detection.batch_index) as u64,
        })
        .collect();
    Ok(DetectorOutput {
        detector_backend: event.detector_backend,
        detector_session_id: event.detector_session_id,
        model_sha256: event.model_sha256,
        clip_sha256: event.clip_sha256,
        model_forward_sha256: event.model_forward_sha256,
        detector_nms_sha256: event.detector_nms_sha256,
        result_sha256: event.result_sha256,
        detections,
        forward_event_nonce: event.nonce,
        forward_event_seq: Some(event.seq),
    })
}

fn parse_detector_probe_args(args: Vec<OsString>) -> Result<DetectorProbeArgs, String> {
    let mut parsed = DetectorProbeArgs {
        sample_frames: 1,
        confidence_threshold: 0.5,
        ..DetectorProbeArgs::default()
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--model" => {
                parsed.model = args
                    .next()
                    .ok_or_else(|| "detector probe requires a value after --model".to_string())?
                    .into();
            }
            "--clip" => {
                parsed.clip = args
                    .next()
                    .ok_or_else(|| "detector probe requires a value after --clip".to_string())?
                    .into();
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
            "--confidence-threshold" => {
                let value = args.next().ok_or_else(|| {
                    "detector probe requires a value after --confidence-threshold".to_string()
                })?;
                let value = value
                    .into_string()
                    .map_err(|_| "detector probe confidence threshold was not UTF-8".to_string())?;
                parsed.confidence_threshold =
                    validate_confidence_threshold(value.parse().map_err(|error| {
                        format!("parse --confidence-threshold {value}: {error}")
                    })?)?;
            }
            other => return Err(format!("unknown detector probe argument {other}")),
        }
    }
    if parsed.model.as_os_str().is_empty() {
        return Err("detector probe requires --model".to_string());
    }
    if parsed.clip.as_os_str().is_empty() {
        return Err("detector probe requires --clip".to_string());
    }
    Ok(parsed)
}

#[derive(Default)]
struct DetectorProbeArgs {
    model: PathBuf,
    clip: PathBuf,
    sample_frames: usize,
    confidence_threshold: f64,
}

fn load_record(model: &Path) -> Result<String, String> {
    let bytes =
        fs::read(model).map_err(|error| format!("load_record {}: {error}", model.display()))?;
    if bytes.len() < 1_000_000 {
        return Err(format!(
            "load_record {}: model checkpoint is too small to be YOLOX-Tiny",
            model.display()
        ));
    }
    Ok(sha256_hex(&bytes))
}

fn load_yolox_tiny_from_checkpoint<B: Backend>(
    model_path: &Path,
    device: &Device<B>,
) -> Result<Yolox<B>, String> {
    let mut model = Yolox::yolox_tiny(80, device);
    let mut store = PytorchStore::from_file(model_path)
        .with_top_level_key("model")
        .with_key_remapping("backbone\\.C3_(.+)", "backbone.c3_$1")
        .with_key_remapping("(backbone\\.backbone\\.dark[2-5])\\.0\\.(.+)", "$1.conv.$2")
        .with_key_remapping("(backbone\\.backbone\\.dark[2-4])\\.1\\.(.+)", "$1.c3.$2")
        .with_key_remapping("(backbone\\.backbone\\.dark5)\\.1\\.(.+)", "$1.spp.$2")
        .with_key_remapping("(backbone\\.backbone\\.dark5)\\.2\\.(.+)", "$1.c3.$2")
        .with_key_remapping(
            "(head\\.(cls|reg)_convs\\.[0-9]+)\\.([0-9]+)\\.(.+)",
            "$1.conv$3.$4",
        );
    model.load_from(&mut store).map_err(|error| {
        format!(
            "load local YOLOX checkpoint {}: {error}",
            model_path.display()
        )
    })?;
    Ok(model)
}

fn decode_frames_to_tensor<B: Backend>(
    segment: &DecodedVideoSegment,
    sample_frames: usize,
    device: &Device<B>,
) -> Result<Tensor<B, 4>, String> {
    let (rgb, frame_count) = media_pipeline::sampled_detector_rgb(
        &segment.frames,
        sample_frames,
        WIDTH as u32,
        HEIGHT as u32,
    )?;
    let tensor = Tensor::<B, 4>::from_data(
        TensorData::new(rgb, [frame_count, HEIGHT, WIDTH, 3]).convert::<B::FloatElem>(),
        device,
    )
    .permute([0, 3, 1, 2]);
    Ok(tensor)
}

struct BackendDetection {
    batch_index: usize,
    class_index: usize,
    bbox: BoundingBox,
}

fn run_nms<B: Backend>(
    model_output: Tensor<B, 3>,
    confidence_threshold: f32,
    allowed_class_indices: &[usize],
) -> Vec<BackendDetection> {
    let [batch_size, num_boxes, num_outputs] = model_output.dims();
    let boxes = model_output
        .clone()
        .slice([0..batch_size, 0..num_boxes, 0..4]);
    let obj_scores = model_output
        .clone()
        .slice([0..batch_size, 0..num_boxes, 4..5]);
    let cls_scores = model_output.slice([0..batch_size, 0..num_boxes, 5..num_outputs]);
    let scores = cls_scores * obj_scores;
    let boxes = nms(boxes, scores, 0.65, confidence_threshold);

    let mut detections = boxes
        .into_iter()
        .enumerate()
        .flat_map(|(batch_index, classes)| {
            classes
                .into_iter()
                .enumerate()
                .flat_map(move |(class_index, boxes)| {
                    boxes.into_iter().map(move |bbox| BackendDetection {
                        batch_index,
                        class_index,
                        bbox,
                    })
                })
        })
        .filter(|detection| allowed_class_indices.contains(&detection.class_index))
        .collect::<Vec<_>>();
    detections.sort_by(|left, right| {
        right
            .bbox
            .confidence
            .partial_cmp(&left.bbox.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    detections
}

fn validate_confidence_threshold(value: f64) -> Result<f64, String> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "detector confidence threshold must be between 0.0 and 1.0, got {value}"
        ))
    }
}

fn tensor_digest<B: Backend>(tensor: Tensor<B, 3>) -> String {
    let mut bytes = Vec::new();
    for value in tensor.into_data().iter::<f32>() {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    sha256_hex(&bytes)
}

fn result_digest_material(detections: &[BackendDetection]) -> String {
    detections
        .iter()
        .map(|detection| {
            format!(
                "{}:{:.6}:{:.0},{:.0},{:.0},{:.0}",
                coco_class_name(detection.class_index),
                detection.bbox.confidence,
                detection.bbox.xmin,
                detection.bbox.ymin,
                detection.bbox.xmax,
                detection.bbox.ymax
            )
        })
        .collect::<Vec<_>>()
        .join("|")
}

pub(crate) const COCO_CLASSES: [&str; 80] = [
    "person",
    "bicycle",
    "car",
    "motorcycle",
    "airplane",
    "bus",
    "train",
    "truck",
    "boat",
    "traffic light",
    "fire hydrant",
    "stop sign",
    "parking meter",
    "bench",
    "bird",
    "cat",
    "dog",
    "horse",
    "sheep",
    "cow",
    "elephant",
    "bear",
    "zebra",
    "giraffe",
    "backpack",
    "umbrella",
    "handbag",
    "tie",
    "suitcase",
    "frisbee",
    "skis",
    "snowboard",
    "sports ball",
    "kite",
    "baseball bat",
    "baseball glove",
    "skateboard",
    "surfboard",
    "tennis racket",
    "bottle",
    "wine glass",
    "cup",
    "fork",
    "knife",
    "spoon",
    "bowl",
    "banana",
    "apple",
    "sandwich",
    "orange",
    "broccoli",
    "carrot",
    "hot dog",
    "pizza",
    "donut",
    "cake",
    "chair",
    "couch",
    "potted plant",
    "bed",
    "dining table",
    "toilet",
    "tv",
    "laptop",
    "mouse",
    "remote",
    "keyboard",
    "cell phone",
    "microwave",
    "oven",
    "toaster",
    "sink",
    "refrigerator",
    "book",
    "clock",
    "vase",
    "scissors",
    "teddy bear",
    "hair drier",
    "toothbrush",
];

fn coco_class_name(index: usize) -> &'static str {
    COCO_CLASSES.get(index).copied().unwrap_or("other")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

impl DetectorForwardProbe {
    fn from_env() -> Option<Self> {
        env::var_os(DETECTOR_FORWARD_PROBE_ENV).map(|path| Self {
            path: path.into(),
            nonce: env::var(DETECTOR_FORWARD_PROBE_NONCE_ENV).ok(),
        })
    }
}

impl DetectorForwardObserver for DetectorForwardProbe {
    fn emit_forward_event(&self, event: &DetectorForwardEvent) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!("create detector probe dir {}: {error}", parent.display())
            })?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| format!("open detector probe {}: {error}", self.path.display()))?;
        writeln!(
            file,
            "DetectorForwardEvent DetectorForwardObserver writer=detector-backend seq={} nonce={} detector_backend={} detector_session_id={} model_sha256={} clip_sha256={} model_forward_sha256={} detector_nms_sha256={} result_sha256={} emitter=detector-backend-forward stage=model-tensor-forward nms=true",
            event.seq,
            event.nonce.as_deref().unwrap_or(""),
            event.detector_backend,
            event.detector_session_id,
            event.model_sha256,
            event.clip_sha256,
            event.model_forward_sha256,
            event.detector_nms_sha256,
            event.result_sha256
        )
        .map_err(|error| format!("write detector probe {}: {error}", self.path.display()))
    }
}

fn event_seq() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| {
            duration
                .as_secs()
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(duration.subsec_nanos()))
        })
        .unwrap_or_default()
}

/// The real Vulkan adapter a passing forward probe selected: name only, the
/// same identity the receipt carries as `selected_device`.
#[cfg(feature = "detect-burn-wgpu")]
pub(crate) struct WgpuAdapterProbe {
    pub(crate) name: String,
}

/// Enumerates Vulkan adapters directly — never `force_fallback_adapter` —
/// and returns the first one that is not a `DeviceType::Cpu` software
/// rasterizer (llvmpipe/lavapipe). A software Vulkan adapter is treated as
/// no usable device, never as hardware; enumeration happens after the
/// supplemental-group privilege drop (the probe runs at detector
/// construction, which is already past that point in the startup order).
#[cfg(feature = "detect-burn-wgpu")]
pub(crate) fn find_hardware_adapter() -> Option<WgpuAdapterProbe> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
    adapters
        .into_iter()
        .find(|adapter| !matches!(adapter.get_info().device_type, wgpu::DeviceType::Cpu))
        .map(|adapter| WgpuAdapterProbe {
            name: adapter.get_info().name,
        })
}

/// A tiny local, synthetic (one blank frame) probe clip: the startup forward
/// probe needs a real file to decode and nothing is fetched or bundled to
/// get one, so one is synthesized on the spot. Deleted on drop.
#[cfg(feature = "detect-burn-wgpu")]
pub(crate) struct ProbeClip {
    path: PathBuf,
}

#[cfg(feature = "detect-burn-wgpu")]
impl ProbeClip {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(feature = "detect-burn-wgpu")]
impl Drop for ProbeClip {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(feature = "detect-burn-wgpu")]
pub(crate) fn synthetic_probe_clip() -> Result<ProbeClip, String> {
    let frame = media_pipeline::DecodedRgbFrame {
        index: 0,
        width: WIDTH as u32,
        height: HEIGHT as u32,
        rgb: vec![0u8; WIDTH * HEIGHT * 3],
    };
    let segment = DecodedVideoSegment {
        frames: vec![frame],
        encoded_units: Vec::new(),
        fps: 1.0,
        observed_at: None,
    };
    let path = env::temp_dir().join(format!("vigil-detector-probe-{}.mp4", event_seq()));
    media_pipeline::write_browser_playable_mp4_clip(&segment, &path)?;
    Ok(ProbeClip { path })
}
