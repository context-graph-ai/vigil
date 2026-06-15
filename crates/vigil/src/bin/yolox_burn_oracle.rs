use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};

use burn::tensor::{Device, Tensor, TensorData, backend::Backend};
use burn_flex::Flex;
use sha2::{Digest, Sha256};
use yolox_burn::model::{BoundingBox, boxes::nms, weights, yolox::Yolox};

const HEIGHT: usize = 640;
const WIDTH: usize = 640;
const PERSON_CLASS_INDEX: usize = 0;

#[derive(Debug)]
struct OracleArgs {
    model: PathBuf,
    clip: PathBuf,
    sample_frames: usize,
}

fn main() {
    match parse_args().and_then(run_oracle) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("{error}");
            process::exit(2);
        }
    }
}

fn run_oracle(args: OracleArgs) -> Result<(), String> {
    let model_sha256 = load_record(&args.model)?;
    seed_yolox_burn_cache(&args.model)?;

    let device = Default::default();
    let model: Yolox<Flex> = Yolox::yolox_tiny_pretrained(weights::YoloxTiny::Coco, &device)
        .map_err(|error| format!("load_record {}: {error}", args.model.display()))?;
    let tensor = decode_frames_to_tensor::<Flex>(&args.clip, args.sample_frames, &device)?;

    let model_output = model.forward(tensor);
    let model_forward_sha256 = tensor_digest(model_output.clone());
    let detections = run_nms(model_output);
    let result_sha256 = sha256_hex(result_digest_material(&detections).as_bytes());

    println!("backend=Burn YOLOX oracle");
    println!("model-sha256={model_sha256}");
    println!("model-forward-sha256={model_forward_sha256}");
    println!("result-sha256={result_sha256}");
    println!("detections={}", detections.len());
    if let Some(detection) = detections.first() {
        println!("class={}", coco_class_name(detection.class_index));
        println!("confidence={:.6}", detection.bbox.confidence);
        println!(
            "bbox={:.0},{:.0},{:.0},{:.0}",
            detection.bbox.xmin, detection.bbox.ymin, detection.bbox.xmax, detection.bbox.ymax
        );
    }

    Ok(())
}

fn parse_args() -> Result<OracleArgs, String> {
    let mut args = env::args_os().skip(1);
    let mut model = None;
    let mut clip = None;
    let mut sample_frames = None;

    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--model" => model = args.next().map(PathBuf::from),
            "--clip" => clip = args.next().map(PathBuf::from),
            "--sample-frames" => {
                let Some(value) = args.next() else {
                    return Err("--sample-frames requires a value".to_string());
                };
                sample_frames = Some(
                    value
                        .to_string_lossy()
                        .parse::<usize>()
                        .map_err(|error| format!("invalid --sample-frames: {error}"))?,
                );
            }
            other => return Err(format!("unknown oracle argument {other}")),
        }
    }

    Ok(OracleArgs {
        model: model.ok_or_else(|| "--model is required".to_string())?,
        clip: clip.ok_or_else(|| "--clip is required".to_string())?,
        sample_frames: sample_frames.unwrap_or(1),
    })
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

fn seed_yolox_burn_cache(model: &Path) -> Result<(), String> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is required to seed the yolox-burn model cache".to_string())?;
    let cache_dir = home.join(".cache").join("yolox-burn");
    fs::create_dir_all(&cache_dir)
        .map_err(|error| format!("create {}: {error}", cache_dir.display()))?;
    let cache_model = cache_dir.join("yolox_tiny.pth");
    fs::copy(model, &cache_model).map_err(|error| {
        format!(
            "seed yolox-burn cache {} from {}: {error}",
            cache_model.display(),
            model.display()
        )
    })?;
    Ok(())
}

fn decode_frames_to_tensor<B: Backend>(
    clip: &Path,
    sample_frames: usize,
    device: &Device<B>,
) -> Result<Tensor<B, 4>, String> {
    let (rgb, frame_count) = decode_sampled_rgb_frames(clip, sample_frames)?;
    let tensor = Tensor::<B, 4>::from_data(
        TensorData::new(rgb, [frame_count, HEIGHT, WIDTH, 3]).convert::<B::FloatElem>(),
        device,
    )
    .permute([0, 3, 1, 2]);
    Ok(tensor)
}

fn decode_sampled_rgb_frames(
    clip: &Path,
    sample_frames: usize,
) -> Result<(Vec<u8>, usize), String> {
    let indices = sampled_frame_indices(clip, sample_frames)?;
    let select = indices
        .iter()
        .map(|index| format!("eq(n\\,{index})"))
        .collect::<Vec<_>>()
        .join("+");
    let ffmpeg = env::var_os("VIGIL_FFMPEG_BIN").unwrap_or_else(|| "ffmpeg".into());
    let output = Command::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(clip)
        .arg("-vf")
        .arg(format!(
            "select='{select}',scale={WIDTH}:{HEIGHT}:flags=bilinear"
        ))
        .arg("-vsync")
        .arg("0")
        .arg("-frames:v")
        .arg(indices.len().to_string())
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("rgb24")
        .arg("-")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("decode frame from {}: {error}", clip.display()))?;
    if !output.status.success() {
        return Err(format!(
            "decode frame from {} failed: {}",
            clip.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let expected = indices.len() * WIDTH * HEIGHT * 3;
    if output.stdout.len() != expected {
        return Err(format!(
            "decode frame from {} produced {} bytes, expected {expected}",
            clip.display(),
            output.stdout.len()
        ));
    }
    Ok((output.stdout, indices.len()))
}

fn sampled_frame_indices(clip: &Path, sample_frames: usize) -> Result<Vec<usize>, String> {
    let frame_count = video_frame_count(clip)?;
    if frame_count == 0 {
        return Err(format!("{} has no decodable frames", clip.display()));
    }
    let wanted = sample_frames.max(1).min(frame_count);
    let mut indices = BTreeSet::new();
    if wanted == 1 {
        indices.insert(frame_count / 2);
    } else {
        for sample in 0..wanted {
            indices.insert((frame_count - 1) * sample / (wanted - 1));
        }
    }
    Ok(indices.into_iter().collect())
}

fn video_frame_count(clip: &Path) -> Result<usize, String> {
    let ffprobe = env::var_os("VIGIL_FFPROBE_BIN").unwrap_or_else(|| "ffprobe".into());
    let output = Command::new(ffprobe)
        .arg("-v")
        .arg("error")
        .arg("-count_frames")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-show_entries")
        .arg("stream=nb_read_frames")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(clip)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("count frames in {}: {error}", clip.display()))?;
    if !output.status.success() {
        return Err(format!(
            "count frames in {} failed: {}",
            clip.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().parse::<usize>().ok())
        .ok_or_else(|| format!("ffprobe returned no frame count for {}", clip.display()))
}

struct Detection {
    class_index: usize,
    bbox: BoundingBox,
}

fn run_nms(model_output: Tensor<Flex, 3>) -> Vec<Detection> {
    let [batch_size, num_boxes, num_outputs] = model_output.dims();
    let boxes = model_output
        .clone()
        .slice([0..batch_size, 0..num_boxes, 0..4]);
    let obj_scores = model_output
        .clone()
        .slice([0..batch_size, 0..num_boxes, 4..5]);
    let cls_scores = model_output.slice([0..batch_size, 0..num_boxes, 5..num_outputs]);
    let scores = cls_scores * obj_scores;
    let boxes = nms(boxes, scores, 0.65, 0.5);

    let mut detections = boxes
        .iter()
        .flat_map(|classes| {
            classes.iter().enumerate().flat_map(|(class_index, boxes)| {
                boxes.iter().map(move |bbox| Detection {
                    class_index,
                    bbox: clone_bbox(bbox),
                })
            })
        })
        .filter(|detection| detection.class_index == PERSON_CLASS_INDEX)
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

fn clone_bbox(bbox: &BoundingBox) -> BoundingBox {
    BoundingBox {
        xmin: bbox.xmin,
        ymin: bbox.ymin,
        xmax: bbox.xmax,
        ymax: bbox.ymax,
        confidence: bbox.confidence,
    }
}

fn tensor_digest(tensor: Tensor<Flex, 3>) -> String {
    let mut bytes = Vec::new();
    for value in tensor.into_data().iter::<f32>() {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    sha256_hex(&bytes)
}

fn result_digest_material(detections: &[Detection]) -> String {
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

fn coco_class_name(index: usize) -> &'static str {
    match index {
        PERSON_CLASS_INDEX => "person",
        _ => "other",
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}
