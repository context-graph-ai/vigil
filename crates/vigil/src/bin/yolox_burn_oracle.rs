use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use burn::tensor::{Device, Tensor, TensorData, backend::Backend};
use burn_flex::Flex;
use burn_store::{ModuleSnapshot, PytorchStore};
use image::{RgbImage, imageops::FilterType};
use mp4::{MediaType, Mp4Reader};
use openh264::decoder::{
    DecodeOptions, Decoder as H264Decoder, DecoderConfig as H264DecoderConfig, Flush,
};
use openh264::formats::YUVSource;
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
    let segment = media_pipeline::decode_video_file(clip)?;
    media_pipeline::sampled_detector_rgb(
        &segment.frames,
        sample_frames,
        WIDTH as u32,
        HEIGHT as u32,
    )
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

mod media_pipeline {
    use super::*;

    pub(super) struct DecodedRgbFrame {
        width: u32,
        height: u32,
        rgb: Vec<u8>,
    }

    pub(super) struct DecodedVideoSegment {
        pub(super) frames: Vec<DecodedRgbFrame>,
    }

    pub(super) fn decode_video_file(path: &Path) -> Result<DecodedVideoSegment, String> {
        decode_mp4_file(path)
    }

    pub(super) fn sampled_detector_rgb(
        frames: &[DecodedRgbFrame],
        sample_frames: usize,
        width: u32,
        height: u32,
    ) -> Result<(Vec<u8>, usize), String> {
        let indices = sampled_frame_indices(frames.len(), sample_frames);
        if indices.is_empty() {
            return Err("video segment has no decoded frames".to_string());
        }
        let mut rgb = Vec::with_capacity(indices.len() * width as usize * height as usize * 3);
        for index in indices {
            rgb.extend(resize_rgb_frame(&frames[index], width, height)?);
        }
        let frame_count = rgb.len() / (width as usize * height as usize * 3);
        Ok((rgb, frame_count))
    }

    fn decode_mp4_file(path: &Path) -> Result<DecodedVideoSegment, String> {
        let file =
            fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
        let size = file
            .metadata()
            .map_err(|error| format!("metadata {}: {error}", path.display()))?
            .len();
        let mut reader = Mp4Reader::read_header(file, size)
            .map_err(|error| format!("read MP4 header: {error}"))?;
        let (track_id, sample_count, sps, pps, media_type) = {
            let (track_id, track, media_type) = reader
                .tracks()
                .iter()
                .find_map(|(track_id, track)| {
                    let media_type = track.media_type().ok()?;
                    matches!(media_type, MediaType::H264 | MediaType::H265)
                        .then_some((*track_id, track, media_type))
                })
                .ok_or_else(|| format!("{} has no H.264/H.265 video track", path.display()))?;
            let sps = if media_type == MediaType::H264 {
                track
                    .sequence_parameter_set()
                    .map_err(|error| format!("read MP4 SPS: {error}"))?
                    .to_vec()
            } else {
                Vec::new()
            };
            let pps = if media_type == MediaType::H264 {
                track
                    .picture_parameter_set()
                    .map_err(|error| format!("read MP4 PPS: {error}"))?
                    .to_vec()
            } else {
                Vec::new()
            };
            (track_id, track.sample_count(), sps, pps, media_type)
        };
        if media_type == MediaType::H265 {
            return Err("MP4 H.265 demux is not implemented in the oracle decoder".to_string());
        }

        let mut encoded_units = Vec::with_capacity(sample_count as usize + 1);
        let mut parameter_unit = Vec::new();
        append_annex_b_nal(&mut parameter_unit, &sps);
        append_annex_b_nal(&mut parameter_unit, &pps);
        encoded_units.push(parameter_unit);
        for sample_id in 1..=sample_count {
            let Some(sample) = reader
                .read_sample(track_id, sample_id)
                .map_err(|error| format!("read MP4 sample {sample_id}: {error}"))?
            else {
                continue;
            };
            let nals = split_avcc_nals(sample.bytes.as_ref())
                .ok_or_else(|| format!("parse MP4 sample {sample_id} length-prefixed NAL units"))?;
            let mut unit = Vec::new();
            for nal in nals {
                append_annex_b_nal(&mut unit, nal);
            }
            if !unit.is_empty() {
                encoded_units.push(unit);
            }
        }

        let frames = decode_h264_units(&encoded_units)?;
        if frames.is_empty() {
            return Err("H.264 segment has no decodable frames".to_string());
        }
        Ok(DecodedVideoSegment { frames })
    }

    fn decode_h264_units(encoded_units: &[Vec<u8>]) -> Result<Vec<DecodedRgbFrame>, String> {
        let config = H264DecoderConfig::new().flush_after_decode(Flush::NoFlush);
        let mut decoder =
            H264Decoder::with_api_config(openh264::OpenH264API::from_source(), config)
                .map_err(|error| format!("create OpenH264 decoder: {error}"))?;
        let decode_options = DecodeOptions::new().flush_after_decode(Flush::NoFlush);
        let mut frames = Vec::new();
        let mut first_error = None;
        for unit in encoded_units {
            for nal in openh264::nal_units(unit) {
                match decoder.decode_with_options(nal, decode_options.clone()) {
                    Ok(Some(frame)) => frames.push(openh264_to_rgb(frame)?),
                    Ok(None) => {}
                    Err(error) if first_error.is_none() => first_error = Some(error.to_string()),
                    Err(_) => {}
                }
            }
        }
        match decoder.flush_remaining() {
            Ok(flushed) => {
                for frame in flushed {
                    frames.push(openh264_to_rgb(frame)?);
                }
            }
            Err(error) if first_error.is_none() => first_error = Some(error.to_string()),
            Err(_) => {}
        }
        if frames.is_empty()
            && let Some(error) = first_error
        {
            return Err(format!("decode H.264 frame: {error}"));
        }
        Ok(frames)
    }

    fn openh264_to_rgb(
        frame: openh264::decoder::DecodedYUV<'_>,
    ) -> Result<DecodedRgbFrame, String> {
        let (width, height) = frame.dimensions();
        let (y_stride, u_stride, v_stride) = frame.strides();
        let y = copy_plane(frame.y(), width, height, y_stride)?;
        let u = copy_plane(frame.u(), width / 2, height / 2, u_stride)?;
        let v = copy_plane(frame.v(), width / 2, height / 2, v_stride)?;
        let width = width as u32;
        let height = height as u32;
        let rgb = yuv420_to_rgb8(&y, &u, &v, width, height, width / 2)?;
        Ok(DecodedRgbFrame { width, height, rgb })
    }

    fn copy_plane(
        source: &[u8],
        width: usize,
        height: usize,
        stride: usize,
    ) -> Result<Vec<u8>, String> {
        if source.len() < stride.saturating_mul(height) {
            return Err("decoded H.264 plane is shorter than its stride".to_string());
        }
        let mut output = Vec::with_capacity(width.saturating_mul(height));
        for row in 0..height {
            let start = row.saturating_mul(stride);
            let end = start.saturating_add(width);
            let Some(slice) = source.get(start..end) else {
                return Err("decoded H.264 plane row is shorter than its width".to_string());
            };
            output.extend_from_slice(slice);
        }
        Ok(output)
    }

    fn yuv420_to_rgb8(
        y_plane: &[u8],
        u_plane: &[u8],
        v_plane: &[u8],
        width: u32,
        height: u32,
        chroma_width: u32,
    ) -> Result<Vec<u8>, String> {
        let width_usize = width as usize;
        let height_usize = height as usize;
        let chroma_width = chroma_width as usize;
        if y_plane.len() < width_usize.saturating_mul(height_usize) {
            return Err("decoded luma plane is shorter than frame dimensions".to_string());
        }
        let mut rgb =
            Vec::with_capacity(width_usize.saturating_mul(height_usize).saturating_mul(3));
        for y in 0..height_usize {
            for x in 0..width_usize {
                let luma = y_plane[y * width_usize + x] as i32;
                let chroma_index = (y / 2) * chroma_width + (x / 2);
                let cb = *u_plane.get(chroma_index).unwrap_or(&128) as i32;
                let cr = *v_plane.get(chroma_index).unwrap_or(&128) as i32;
                let c = (luma - 16).max(0);
                let d = cb - 128;
                let e = cr - 128;
                rgb.push(clamp_rgb((298 * c + 409 * e + 128) >> 8));
                rgb.push(clamp_rgb((298 * c - 100 * d - 208 * e + 128) >> 8));
                rgb.push(clamp_rgb((298 * c + 516 * d + 128) >> 8));
            }
        }
        Ok(rgb)
    }

    fn clamp_rgb(value: i32) -> u8 {
        value.clamp(0, 255) as u8
    }

    fn sampled_frame_indices(frame_count: usize, sample_frames: usize) -> Vec<usize> {
        if frame_count == 0 {
            return Vec::new();
        }
        let wanted = sample_frames.max(1).min(frame_count);
        if wanted == 1 {
            return vec![frame_count / 2];
        }
        let mut indices = Vec::with_capacity(wanted);
        for sample in 0..wanted {
            let index = (frame_count - 1) * sample / (wanted - 1);
            if indices.last().copied() != Some(index) {
                indices.push(index);
            }
        }
        indices
    }

    fn resize_rgb_frame(
        frame: &DecodedRgbFrame,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, String> {
        let image = RgbImage::from_raw(frame.width, frame.height, frame.rgb.clone())
            .ok_or_else(|| "decoded RGB frame dimensions do not match buffer length".to_string())?;
        Ok(image::imageops::resize(&image, width, height, FilterType::Triangle).into_raw())
    }

    fn split_avcc_nals(data: &[u8]) -> Option<Vec<&[u8]>> {
        [4_usize, 2, 1]
            .into_iter()
            .find_map(|length_size| split_avcc_nals_with_length(data, length_size))
    }

    fn split_avcc_nals_with_length(data: &[u8], length_size: usize) -> Option<Vec<&[u8]>> {
        let mut offset = 0_usize;
        let mut nals = Vec::new();
        while offset < data.len() {
            if offset + length_size > data.len() {
                return None;
            }
            let length = match length_size {
                1 => data[offset] as usize,
                2 => u16::from_be_bytes([data[offset], data[offset + 1]]) as usize,
                4 => u32::from_be_bytes([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                ]) as usize,
                _ => return None,
            };
            offset += length_size;
            if length == 0 || offset + length > data.len() {
                return None;
            }
            nals.push(&data[offset..offset + length]);
            offset += length;
        }
        Some(nals)
    }

    fn append_annex_b_nal(output: &mut Vec<u8>, nal: &[u8]) {
        if nal.is_empty() {
            return;
        }
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(nal);
    }
}
