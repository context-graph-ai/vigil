use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use burn::tensor::{Device, Tensor, TensorData, backend::Backend};
use burn_flex::Flex;
use burn_store::{ModuleSnapshot, PytorchStore};
use sha2::{Digest, Sha256};
use yolox_burn::model::{BoundingBox, boxes::nms, yolox::Yolox};

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

    let device = Default::default();
    let model = load_yolox_tiny_from_checkpoint(&args.model, &device)?;
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

fn load_yolox_tiny_from_checkpoint(
    model_path: &Path,
    device: &Device<Flex>,
) -> Result<Yolox<Flex>, String> {
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
    vigil::decode_sampled_detector_rgb_frames(clip, sample_frames, WIDTH as u32, HEIGHT as u32)
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
