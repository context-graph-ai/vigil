use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use image::{
    ColorType, GrayImage, ImageBuffer, ImageEncoder, Luma, Rgb, RgbImage, codecs::png::PngEncoder,
    imageops::FilterType,
};
use imageproc::contrast::{ThresholdType, threshold};
use imageproc::region_labelling::{Connectivity, connected_components};
use mp4::{AvcConfig, Bytes, MediaType, Mp4Config, Mp4Reader, Mp4Sample, Mp4Writer, TrackConfig};
use openh264::decoder::{
    DecodeOptions, Decoder as H264Decoder, DecoderConfig as H264DecoderConfig, Flush,
};
use openh264::encoder::{
    BitRate, Encoder as H264Encoder, EncoderConfig as H264EncoderConfig, FrameRate,
    RateControlMode, SpsPpsStrategy,
};
use openh264::formats::{RgbSliceU8, YUVBuffer, YUVSource};
use retina::client::{
    Credentials as RetinaCredentials, PlayOptions, Session, SessionOptions, SetupOptions,
};
use retina::codec::{CodecItem, FrameFormat};
use rust_h265::Decoder as H265Decoder;
use sha2::{Digest, Sha256};
use tokio::runtime::Builder;
use url::Url;

const PLAYABLE_MAX_WIDTH: u32 = 1280;
const PLAYABLE_MAX_HEIGHT: u32 = 720;
const MOTION_WIDTH: u32 = 64;
const MOTION_HEIGHT: u32 = 36;

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct RtspCredentials {
    pub(crate) username: String,
    pub(crate) password: String,
}

impl std::fmt::Debug for RtspCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RtspCredentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl From<RtspCredentials> for RetinaCredentials {
    fn from(credentials: RtspCredentials) -> Self {
        Self {
            username: credentials.username,
            password: credentials.password,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RtspSource {
    session_url: Url,
    credentials: Option<RtspCredentials>,
}

impl RtspSource {
    pub(crate) fn session_url(&self) -> &str {
        self.session_url.as_str()
    }

    fn retina_credentials(&self) -> Option<RetinaCredentials> {
        self.credentials.clone().map(Into::into)
    }
}

pub(crate) fn prepare_rtsp_source(
    rtsp_url: &str,
    explicit_username: Option<&str>,
    explicit_password: Option<&str>,
) -> Result<RtspSource, String> {
    let mut url = Url::parse(rtsp_url)
        .map_err(|error| format!("parse RTSP URL {}: {error}", redact_rtsp_url(rtsp_url)))?;
    let embedded_credentials = extract_url_credentials(&url);
    strip_url_credentials(&mut url)?;
    let credentials = resolve_rtsp_credentials(
        embedded_credentials,
        non_empty(explicit_username),
        explicit_password,
    )?;
    Ok(RtspSource {
        session_url: url,
        credentials,
    })
}

pub(crate) fn redact_rtsp_url(rtsp_url: &str) -> String {
    match Url::parse(rtsp_url) {
        Ok(mut url) => {
            if strip_url_credentials(&mut url).is_ok() {
                url.to_string()
            } else {
                redact_rtsp_url_lossy(rtsp_url)
            }
        }
        Err(_) => redact_rtsp_url_lossy(rtsp_url),
    }
}

fn resolve_rtsp_credentials(
    embedded_credentials: Option<RtspCredentials>,
    explicit_username: Option<&str>,
    explicit_password: Option<&str>,
) -> Result<Option<RtspCredentials>, String> {
    if let Some(username) = explicit_username {
        return Ok(Some(RtspCredentials {
            username: username.to_string(),
            password: explicit_password.unwrap_or_default().to_string(),
        }));
    }
    if let Some(password) = explicit_password {
        let Some(embedded) = embedded_credentials else {
            return Err(
                "rtsp_password requires rtsp_username or a username in the RTSP URL".into(),
            );
        };
        return Ok(Some(RtspCredentials {
            username: embedded.username,
            password: password.to_string(),
        }));
    }
    Ok(embedded_credentials)
}

fn extract_url_credentials(url: &Url) -> Option<RtspCredentials> {
    if url.username().is_empty() && url.password().is_none() {
        return None;
    }
    Some(RtspCredentials {
        username: percent_decode_utf8_lossy(url.username()),
        password: url
            .password()
            .map(percent_decode_utf8_lossy)
            .unwrap_or_default(),
    })
}

fn strip_url_credentials(url: &mut Url) -> Result<(), String> {
    url.set_username("")
        .map_err(|_| "RTSP URL cannot clear username for client session".to_string())?;
    url.set_password(None)
        .map_err(|_| "RTSP URL cannot clear password for client session".to_string())?;
    Ok(())
}

fn redact_rtsp_url_lossy(rtsp_url: &str) -> String {
    let Some((scheme, rest)) = rtsp_url.split_once("://") else {
        return rtsp_url.to_string();
    };
    let Some((_, after_userinfo)) = rest.rsplit_once('@') else {
        return rtsp_url.to_string();
    };
    format!("{scheme}://{after_userinfo}")
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

fn percent_decode_utf8_lossy(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            decoded.push((high << 4) | low);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(decoded).unwrap_or_else(|error| {
        let bytes = error.into_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoCodec {
    H264,
    H265,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedRgbFrame {
    pub index: u64,
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

#[derive(Clone)]
pub(crate) struct DecodedVideoSegment {
    pub(crate) frames: Vec<DecodedRgbFrame>,
    pub(crate) encoded_units: Vec<Vec<u8>>,
    pub(crate) fps: f64,
    pub(crate) observed_at: Option<DateTime<Utc>>,
}

impl DecodedVideoSegment {
    pub(crate) fn frame_count(&self) -> u64 {
        self.frames.len() as u64
    }
}

/// How the capture session selects its decoder backend.
pub(crate) struct CaptureDecodeOptions {
    pub(crate) stream_id: crate::workgraph::StreamId,
    pub(crate) stream_epoch: u64,
    pub(crate) hardware_decoding: bool,
}

/// Real stream units collected before running the hardware decode probe.
const HARDWARE_PROBE_UNIT_COUNT: usize = 12;
/// A low-fps camera cannot deliver the full probe buffer quickly; after
/// this deadline the probe runs on whatever real units were collected so
/// slow streams still select a backend instead of timing out forever.
/// Advanced override: VIGIL_HARDWARE_PROBE_DEADLINE_SECS.
const HARDWARE_PROBE_DEADLINE: Duration = Duration::from_secs(10);

fn hardware_probe_deadline() -> Duration {
    std::env::var("VIGIL_HARDWARE_PROBE_DEADLINE_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.max(1)))
        .unwrap_or(HARDWARE_PROBE_DEADLINE)
}

pub(crate) fn capture_rtsp_segments<OnSessionStarted, OnSegment, OnDecodeReceipt>(
    rtsp_source: &RtspSource,
    max_frames: usize,
    shutdown: Arc<AtomicBool>,
    decode_options: CaptureDecodeOptions,
    mut on_decode_receipt: OnDecodeReceipt,
    mut on_session_started: OnSessionStarted,
    mut on_segment: OnSegment,
) -> Result<(), String>
where
    OnSessionStarted: FnMut() -> Result<(), String>,
    OnSegment: FnMut(DecodedVideoSegment) -> Result<(), String>,
    OnDecodeReceipt: FnMut(crate::acceleration::AccelerationReceipt),
{
    let rt = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|error| format!("create media tokio runtime: {error}"))?;
    rt.block_on(capture_rtsp_segments_async(
        rtsp_source,
        max_frames.max(1),
        shutdown,
        decode_options,
        &mut on_decode_receipt,
        &mut on_session_started,
        &mut on_segment,
    ))
}

async fn capture_rtsp_segments_async<OnSessionStarted, OnSegment, OnDecodeReceipt>(
    rtsp_source: &RtspSource,
    max_frames: usize,
    shutdown: Arc<AtomicBool>,
    decode_options: CaptureDecodeOptions,
    on_decode_receipt: &mut OnDecodeReceipt,
    on_session_started: &mut OnSessionStarted,
    on_segment: &mut OnSegment,
) -> Result<(), String>
where
    OnSessionStarted: FnMut() -> Result<(), String>,
    OnSegment: FnMut(DecodedVideoSegment) -> Result<(), String>,
    OnDecodeReceipt: FnMut(crate::acceleration::AccelerationReceipt),
{
    let mut session = Session::describe(
        rtsp_source.session_url.clone(),
        SessionOptions::default().creds(rtsp_source.retina_credentials()),
    )
    .await
    .map_err(|error| format!("RTSP DESCRIBE failed: {error}"))?;
    let (stream_i, codec, fps) = session
        .streams()
        .iter()
        .enumerate()
        .find_map(|(index, stream)| {
            let codec = codec_for_stream(stream.media(), stream.encoding_name())?;
            Some((index, codec, stream.framerate().unwrap_or(0.0) as f64))
        })
        .ok_or_else(|| "RTSP session has no H.264/H.265 video stream".to_string())?;
    session
        .setup(
            stream_i,
            SetupOptions::default().frame_format(FrameFormat::SIMPLE),
        )
        .await
        .map_err(|error| format!("RTSP SETUP failed: {error}"))?;
    let playing = session
        .play(PlayOptions::default())
        .await
        .map_err(|error| format!("RTSP PLAY failed: {error}"))?;
    let mut demuxed = playing
        .demuxed()
        .map_err(|error| format!("RTSP demux setup failed: {error}"))?;
    on_session_started()?;
    let mut assembler = crate::decode::AccessUnitAssembler::new(
        decode_options.stream_id.clone(),
        codec,
        decode_options.stream_epoch,
    );
    // With no hardware probe to run (intent off, a software-only artifact,
    // or a device the process cannot even open), the backend is selected
    // immediately: a blocked device reports its classified receipt without
    // waiting for stream units, and behavior matches the pre-seam software
    // path exactly. Only a genuinely possible hardware probe waits for
    // REAL stream units.
    // hardware_decoding=false means NO hardware probing of any kind —
    // including the device-access preflight, which opens /dev/dri nodes.
    // Short-circuit on intent before any device is touched.
    let hardware_probe_possible = decode_options.hardware_decoding && {
        #[cfg(feature = "decode-gstreamer")]
        {
            crate::doctor::live_device_access_finding().is_none()
        }
        #[cfg(not(feature = "decode-gstreamer"))]
        {
            false
        }
    };
    let needs_stream_probe = hardware_probe_possible;
    let mut backend: Option<Box<dyn crate::decode::DecodeBackend>> = None;
    let mut probe_buffer: Vec<crate::decode::EncodedAccessUnit> = Vec::new();
    if !needs_stream_probe {
        let selection = crate::decode::select_decode_backend(
            &decode_options.stream_id,
            codec,
            decode_options.stream_epoch,
            decode_options.hardware_decoding,
            &[],
        )?;
        on_decode_receipt(selection.receipt);
        backend = Some(selection.backend);
    }
    let mut segment_counter = 0u64;
    let mut first_unit_of_session = true;
    // Units awaiting normal processing. Probe units are REPLAYED through
    // here after backend selection, so the stream's first parameter-set
    // boundary is never lost — a camera that sends codec config only once
    // still produces its first segment.
    let mut pending_units: std::collections::VecDeque<crate::decode::EncodedAccessUnit> =
        std::collections::VecDeque::new();
    // The most recent COMPLETE codec-config unit. A camera may send SPS/PPS
    // only once at stream start; a mid-stream fallback decoder replays this
    // so decode (and segment assembly) resumes without waiting for a repeat
    // that may never come.
    let mut last_codec_config_unit: Option<crate::decode::EncodedAccessUnit> = None;
    let mut encoded_units = Vec::new();
    let mut decoded_frames = Vec::new();
    let mut segment_started = false;
    let mut segment_observed_at = None;
    let started = tokio::time::Instant::now();
    let mut last_video_at = started;
    let mut segment_started_at = None;
    let mut any_frames_decoded = false;
    while !shutdown.load(Ordering::SeqCst) {
        // The decodable-frames watchdog runs on EVERY iteration: a stream
        // that keeps sending undecodable units (no complete codec config)
        // never pauses long enough for the poll-timeout branch to see it.
        if !any_frames_decoded && started.elapsed() > Duration::from_secs(30) {
            return Err("RTSP capture timed out waiting for decodable video frames".to_string());
        }
        match tokio::time::timeout(Duration::from_millis(500), demuxed.next()).await {
            Ok(Some(Ok(CodecItem::VideoFrame(frame)))) if frame.stream_id() == stream_i => {
                let data = frame.into_data();
                last_video_at = tokio::time::Instant::now();
                let unit = assembler.assemble(
                    data,
                    Some(Utc::now()),
                    std::mem::take(&mut first_unit_of_session),
                    segment_counter,
                );
                if backend.is_none() {
                    // Collect real stream units (from the first parameter-set
                    // boundary) and run the hardware decode probe on them.
                    if probe_buffer.is_empty() && unit.codec_config.is_none() {
                        continue;
                    }
                    probe_buffer.push(unit);
                    let probe_deadline_reached =
                        !probe_buffer.is_empty() && started.elapsed() > hardware_probe_deadline();
                    if probe_buffer.len() >= HARDWARE_PROBE_UNIT_COUNT || probe_deadline_reached {
                        let selection = crate::decode::select_decode_backend(
                            &decode_options.stream_id,
                            codec,
                            decode_options.stream_epoch,
                            decode_options.hardware_decoding,
                            &probe_buffer,
                        )?;
                        on_decode_receipt(selection.receipt);
                        backend = Some(selection.backend);
                        // Replay the probe units into normal processing so
                        // the stream start (and its codec config) is kept.
                        pending_units.extend(probe_buffer.drain(..));
                    }
                } else {
                    pending_units.push_back(unit);
                }
                while let Some(mut unit) = pending_units.pop_front() {
                    if unit.codec_config.is_some() {
                        last_codec_config_unit = Some(unit.clone());
                    }
                    let active_backend = backend.as_mut().expect("decode backend selected");
                    // Boundary-first segment membership: the parameter-set
                    // unit that STARTS segment N belongs to segment N.
                    unit.segment_sequence = next_segment_membership(
                        segment_started,
                        unit.codec_config.is_some(),
                        segment_counter,
                    );
                    let decoded = active_backend.decode(&unit);
                    let mut unit_frames = match decoded {
                        Ok(frames) => frames,
                        Err(error) => {
                            if active_backend.id() == "software" {
                                // The software path failing is a stream fault,
                                // exactly as before the seam existed.
                                return Err(format!("software decode failed: {error:?}"));
                            }
                            // Mid-stream hardware failure: visible software
                            // fallback; the in-progress segment is abandoned
                            // and assembly resumes at the next parameter-set
                            // boundary.
                            on_decode_receipt(crate::decode::mid_stream_fallback_receipt(
                                &decode_options.stream_id,
                                codec,
                                &format!("{error:?}"),
                            ));
                            *active_backend = Box::new(crate::decode::SoftwareDecodeBackend::new(
                                decode_options.stream_id.clone(),
                                codec,
                                decode_options.stream_epoch,
                            )?);
                            encoded_units.clear();
                            decoded_frames.clear();
                            segment_started = false;
                            // Replay the cached codec config so the fresh
                            // software decoder is seeded and assembly can
                            // restart even when the camera never repeats
                            // SPS/PPS. (Recovery replay: its sequence
                            // number is historical by design.)
                            if let Some(config_unit) = last_codec_config_unit.clone() {
                                pending_units.push_front(config_unit);
                            }
                            continue;
                        }
                    };
                    if !segment_started && unit.codec_config.is_some() {
                        encoded_units.clear();
                        decoded_frames.clear();
                        segment_started = true;
                        segment_counter += 1;
                        segment_observed_at = Some(Utc::now());
                        segment_started_at = Some(last_video_at);
                    }
                    if !segment_started {
                        continue;
                    }
                    encoded_units.push(unit.data);
                    if !unit_frames.is_empty() && decoded_frames.is_empty() {
                        segment_started_at.get_or_insert(last_video_at);
                    }
                    decoded_frames.append(&mut unit_frames);
                    any_frames_decoded = any_frames_decoded || !decoded_frames.is_empty();
                    if decoded_frames.len() >= max_frames {
                        let units = std::mem::take(&mut encoded_units);
                        let mut frames = std::mem::take(&mut decoded_frames);
                        segment_started = false;
                        reindex_frames(&mut frames);
                        let segment_fps = segment_fps(fps, segment_started_at.take(), frames.len());
                        on_segment(DecodedVideoSegment {
                            frames,
                            encoded_units: units,
                            fps: segment_fps,
                            observed_at: segment_observed_at.take(),
                        })?;
                    }
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => return Err(format!("RTSP demux failed: {error}")),
            Ok(None) => {
                if segment_started && !decoded_frames.is_empty() {
                    let units = std::mem::take(&mut encoded_units);
                    let mut frames = std::mem::take(&mut decoded_frames);
                    reindex_frames(&mut frames);
                    let segment_fps = segment_fps(fps, segment_started_at.take(), frames.len());
                    on_segment(DecodedVideoSegment {
                        frames,
                        encoded_units: units,
                        fps: segment_fps,
                        observed_at: segment_observed_at.take(),
                    })?;
                }
                return Err("RTSP stream ended".to_string());
            }
            Err(_) => {
                if last_video_at.elapsed() > Duration::from_secs(8) {
                    return Err("RTSP capture timed out waiting for video access units".to_string());
                }
                if !any_frames_decoded && started.elapsed() > Duration::from_secs(30) {
                    return Err(
                        "RTSP capture timed out waiting for decodable video frames".to_string()
                    );
                }
            }
        }
    }
    if segment_started && !decoded_frames.is_empty() {
        reindex_frames(&mut decoded_frames);
        let segment_fps = segment_fps(fps, segment_started_at.take(), decoded_frames.len());
        on_segment(DecodedVideoSegment {
            frames: decoded_frames,
            encoded_units,
            fps: segment_fps,
            observed_at: segment_observed_at.take(),
        })?;
    }
    Ok(())
}

/// Boundary-first segment membership: a parameter-set-carrying unit that
/// will START a new segment belongs to that NEW segment; every other unit
/// belongs to the current one.
fn next_segment_membership(segment_started: bool, carries_config: bool, counter: u64) -> u64 {
    if !segment_started && carries_config {
        counter + 1
    } else {
        counter
    }
}

fn segment_fps(declared_fps: f64, started_at: Option<tokio::time::Instant>, frames: usize) -> f64 {
    if declared_fps > 0.0 {
        return declared_fps;
    }
    let Some(started_at) = started_at else {
        return 0.0;
    };
    let elapsed = started_at.elapsed().as_secs_f64();
    if elapsed <= 0.0 {
        return 0.0;
    }
    frames as f64 / elapsed
}

pub(crate) enum StreamingDecoder {
    H264 {
        decoder: H264Decoder,
        decode_options: DecodeOptions,
    },
    H265 {
        decoder: Box<H265Decoder>,
    },
}

impl StreamingDecoder {
    pub(crate) fn new(codec: VideoCodec) -> Result<Self, String> {
        match codec {
            VideoCodec::H264 => {
                let config = H264DecoderConfig::new().flush_after_decode(Flush::NoFlush);
                let decoder =
                    H264Decoder::with_api_config(openh264::OpenH264API::from_source(), config)
                        .map_err(|error| format!("create OpenH264 decoder: {error}"))?;
                Ok(Self::H264 {
                    decoder,
                    decode_options: DecodeOptions::new().flush_after_decode(Flush::NoFlush),
                })
            }
            VideoCodec::H265 => Ok(Self::H265 {
                decoder: Box::new(H265Decoder::new()),
            }),
        }
    }

    pub(crate) fn decode_unit(&mut self, unit: &[u8]) -> Result<Vec<DecodedRgbFrame>, String> {
        match self {
            Self::H264 {
                decoder,
                decode_options,
            } => {
                let mut frames = Vec::new();
                let mut saw_nal = false;
                for nal in openh264::nal_units(unit) {
                    saw_nal = true;
                    match decoder.decode_with_options(nal, decode_options.clone()) {
                        Ok(Some(frame)) => frames.push(openh264_to_rgb(frame)?),
                        Ok(None) => {}
                        Err(_) => {}
                    }
                }
                if !saw_nal && !unit.is_empty() {
                    match decoder.decode_with_options(unit, decode_options.clone()) {
                        Ok(Some(frame)) => frames.push(openh264_to_rgb(frame)?),
                        Ok(None) => {}
                        Err(_) => {}
                    }
                }
                Ok(frames)
            }
            Self::H265 { decoder } => {
                let mut frames = Vec::new();
                for nal in rust_h265::parse_annex_b(unit) {
                    match decoder.decode_nal(&nal) {
                        Ok(Some(frame)) => frames.push(h265_to_rgb(frame)?),
                        Ok(None) => {}
                        Err(_) => {}
                    }
                }
                Ok(frames)
            }
        }
    }
}

pub(crate) fn decode_video_file(path: &Path) -> Result<DecodedVideoSegment, String> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("h264" | "264") => decode_annex_b_file(path, VideoCodec::H264),
        Some("h265" | "265" | "hevc") => decode_annex_b_file(path, VideoCodec::H265),
        _ => decode_mp4_file(path),
    }
}

pub(crate) fn write_browser_playable_mp4_clip(
    segment: &DecodedVideoSegment,
    path: &Path,
) -> Result<(), String> {
    let (width, height) = playable_frame_dimensions(segment)?;
    let fps = playable_fps(segment.fps);
    let frame_duration = (1000.0 / fps).round().max(1.0) as u32;
    let mut encoder = H264Encoder::with_api_config(
        openh264::OpenH264API::from_source(),
        H264EncoderConfig::new()
            .bitrate(BitRate::from_bps(playable_bitrate_bps(width, height, fps)))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .rate_control_mode(RateControlMode::Off)
            .skip_frames(false)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId),
    )
    .map_err(|error| format!("create H.264 review encoder: {error}"))?;
    encoder.force_intra_frame();

    let mut sps = Vec::new();
    let mut pps = Vec::new();
    let mut samples = Vec::with_capacity(segment.frames.len());
    let mut start_time = 0_u64;
    for frame in &segment.frames {
        let rgb = frame_rgb_for_dimensions(frame, width, height)?;
        let rgb_source = RgbSliceU8::new(&rgb, (width as usize, height as usize));
        let yuv = YUVBuffer::from_rgb8_source(rgb_source);
        let bitstream = encoder
            .encode(&yuv)
            .map_err(|error| format!("encode H.264 review frame {}: {error}", frame.index))?;
        let mut sample = Vec::new();
        let mut is_sync = false;
        for layer_index in 0..bitstream.num_layers() {
            let Some(layer) = bitstream.layer(layer_index) else {
                continue;
            };
            for nal_index in 0..layer.nal_count() {
                let Some(raw_nal) = layer.nal_unit(nal_index) else {
                    continue;
                };
                let nal = strip_h264_start_code(raw_nal);
                let Some(header) = nal.first().copied() else {
                    continue;
                };
                match header & 0x1f {
                    7 => sps = nal.to_vec(),
                    8 => pps = nal.to_vec(),
                    5 => {
                        is_sync = true;
                        append_avcc_nal(&mut sample, nal)?;
                    }
                    _ => append_avcc_nal(&mut sample, nal)?,
                }
            }
        }
        if sample.is_empty() {
            continue;
        }
        samples.push(Mp4Sample {
            start_time,
            duration: frame_duration,
            rendering_offset: 0,
            is_sync,
            bytes: Bytes::from(sample),
        });
        start_time = start_time.saturating_add(u64::from(frame_duration));
    }

    if sps.is_empty() || pps.is_empty() {
        return Err("H.264 review encoder did not emit SPS/PPS metadata".to_string());
    }
    if samples.is_empty() {
        return Err("H.264 review encoder did not emit playable frame samples".to_string());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create clip dir {}: {error}", parent.display()))?;
    }
    let mut file =
        fs::File::create(path).map_err(|error| format!("create {}: {error}", path.display()))?;
    let mp4_config = Mp4Config {
        major_brand: "isom".parse().expect("isom is a valid MP4 brand"),
        minor_version: 512,
        compatible_brands: vec![
            "isom".parse().expect("isom is a valid MP4 brand"),
            "iso2".parse().expect("iso2 is a valid MP4 brand"),
            "avc1".parse().expect("avc1 is a valid MP4 brand"),
            "mp41".parse().expect("mp41 is a valid MP4 brand"),
        ],
        timescale: 1000,
    };
    let avc_config = AvcConfig {
        width: u16::try_from(width).map_err(|_| format!("review MP4 width {width} exceeds u16"))?,
        height: u16::try_from(height)
            .map_err(|_| format!("review MP4 height {height} exceeds u16"))?,
        seq_param_set: sps,
        pic_param_set: pps,
    };
    let mut writer = Mp4Writer::write_start(&mut file, &mp4_config)
        .map_err(|error| format!("start MP4 writer {}: {error}", path.display()))?;
    writer
        .add_track(&TrackConfig::from(avc_config))
        .map_err(|error| format!("add H.264 review MP4 track {}: {error}", path.display()))?;
    for sample in &samples {
        writer.write_sample(1, sample).map_err(|error| {
            format!("write H.264 review MP4 sample {}: {error}", path.display())
        })?;
    }
    writer
        .write_end()
        .map_err(|error| format!("finish MP4 writer {}: {error}", path.display()))?;
    drop(writer);
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))
}

fn playable_frame_dimensions(segment: &DecodedVideoSegment) -> Result<(u32, u32), String> {
    let first = segment
        .frames
        .first()
        .ok_or_else(|| "video segment has no decoded frames for review MP4".to_string())?;
    let source_width = first.width - (first.width % 2);
    let source_height = first.height - (first.height % 2);
    let (width, height) = playable_scaled_dimensions(
        source_width,
        source_height,
        PLAYABLE_MAX_WIDTH,
        PLAYABLE_MAX_HEIGHT,
    );
    if width < 2 || height < 2 {
        return Err(format!(
            "review MP4 frame dimensions {}x{} are too small",
            first.width, first.height
        ));
    }
    Ok((width, height))
}

fn playable_scaled_dimensions(
    source_width: u32,
    source_height: u32,
    max_width: u32,
    max_height: u32,
) -> (u32, u32) {
    if source_width <= max_width && source_height <= max_height {
        return (source_width, source_height);
    }
    let source_width_u64 = u64::from(source_width);
    let source_height_u64 = u64::from(source_height);
    let width_limited_height = (source_height_u64 * u64::from(max_width) / source_width_u64) as u32;
    let (width, height) = if width_limited_height <= max_height {
        (max_width, width_limited_height)
    } else {
        (
            (source_width_u64 * u64::from(max_height) / source_height_u64) as u32,
            max_height,
        )
    };
    (width - (width % 2), height - (height % 2))
}

fn playable_fps(fps: f64) -> f64 {
    if fps.is_finite() && (1.0..=60.0).contains(&fps) {
        fps
    } else {
        5.0
    }
}

fn playable_bitrate_bps(width: u32, height: u32, fps: f64) -> u32 {
    let estimated = (width as f64 * height as f64 * fps * 0.12).round() as u32;
    estimated.clamp(800_000, 4_000_000)
}

fn frame_rgb_for_dimensions(
    frame: &DecodedRgbFrame,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    if frame.width == width && frame.height == height {
        return Ok(frame.rgb.clone());
    }
    resize_rgb_frame(frame, width, height)
}

fn strip_h264_start_code(nal: &[u8]) -> &[u8] {
    if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        nal
    }
}

fn append_avcc_nal(output: &mut Vec<u8>, nal: &[u8]) -> Result<(), String> {
    let len = u32::try_from(nal.len())
        .map_err(|_| format!("H.264 NAL too large for MP4 sample: {} bytes", nal.len()))?;
    output.extend_from_slice(&len.to_be_bytes());
    output.extend_from_slice(nal);
    Ok(())
}

pub(crate) fn sha256_path(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

pub(crate) fn sampled_frame_indices(frame_count: usize, sample_frames: usize) -> Vec<usize> {
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

pub(crate) fn sampled_detector_rgb(
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

pub(crate) fn write_detector_evidence_png(
    segment: &DecodedVideoSegment,
    frame_index: u64,
    bbox: &str,
    path: &Path,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let frame = segment
        .frames
        .get(frame_index as usize)
        .ok_or_else(|| format!("detector evidence frame index {frame_index} is not in segment"))?;
    let rgb = resize_rgb_frame(frame, width, height)?;
    let mut image = RgbImage::from_raw(width, height, rgb)
        .ok_or_else(|| "detector evidence RGB dimensions do not match buffer length".to_string())?;
    let bbox = parse_bbox(bbox)?;
    draw_bbox(&mut image, bbox);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!("create detector evidence dir {}: {error}", parent.display())
        })?;
    }
    let file = fs::File::create(path)
        .map_err(|error| format!("create detector evidence image {}: {error}", path.display()))?;
    PngEncoder::new(&file)
        .write_image(image.as_raw(), width, height, ColorType::Rgb8.into())
        .map_err(|error| format!("write detector evidence image {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync detector evidence image {}: {error}", path.display()))?;
    if let Some(parent) = path.parent() {
        let directory = fs::File::open(parent)
            .map_err(|error| format!("open detector evidence dir {}: {error}", parent.display()))?;
        directory
            .sync_all()
            .map_err(|error| format!("sync detector evidence dir {}: {error}", parent.display()))?;
    }
    Ok(())
}

pub(crate) struct MotionGateResult {
    pub(crate) motion_positive_frames: u64,
}

fn parse_bbox(bbox: &str) -> Result<(i32, i32, i32, i32), String> {
    let mut parts = bbox.split(',');
    let x1 = parse_bbox_coord(parts.next(), "x1")?;
    let y1 = parse_bbox_coord(parts.next(), "y1")?;
    let x2 = parse_bbox_coord(parts.next(), "x2")?;
    let y2 = parse_bbox_coord(parts.next(), "y2")?;
    if parts.next().is_some() {
        return Err(format!(
            "detector bbox {bbox:?} has more than four coordinates"
        ));
    }
    Ok((x1, y1, x2, y2))
}

fn parse_bbox_coord(value: Option<&str>, label: &str) -> Result<i32, String> {
    value
        .ok_or_else(|| format!("detector bbox omitted {label}"))?
        .parse::<i32>()
        .map_err(|error| format!("parse detector bbox {label}: {error}"))
}

fn draw_bbox(image: &mut RgbImage, bbox: (i32, i32, i32, i32)) {
    let width = image.width() as i32;
    let height = image.height() as i32;
    if width == 0 || height == 0 {
        return;
    }
    let (mut x1, mut y1, mut x2, mut y2) = bbox;
    x1 = x1.clamp(0, width - 1);
    x2 = x2.clamp(0, width - 1);
    y1 = y1.clamp(0, height - 1);
    y2 = y2.clamp(0, height - 1);
    if x1 > x2 {
        std::mem::swap(&mut x1, &mut x2);
    }
    if y1 > y2 {
        std::mem::swap(&mut y1, &mut y2);
    }
    let color = Rgb([255, 0, 0]);
    for thickness in 0..3 {
        let left = (x1 - thickness).clamp(0, width - 1);
        let right = (x2 + thickness).clamp(0, width - 1);
        let top = (y1 - thickness).clamp(0, height - 1);
        let bottom = (y2 + thickness).clamp(0, height - 1);
        for x in left..=right {
            image.put_pixel(x as u32, top as u32, color);
            image.put_pixel(x as u32, bottom as u32, color);
        }
        for y in top..=bottom {
            image.put_pixel(left as u32, y as u32, color);
            image.put_pixel(right as u32, y as u32, color);
        }
    }
}

pub(crate) fn motion_gate(segment: &DecodedVideoSegment) -> MotionGateResult {
    let mut previous = None;
    let mut motion_positive_frames = 0_u64;
    for frame in &segment.frames {
        let gray = motion_thumbnail(frame);
        if let Some(previous_gray) = previous.as_ref()
            && motion_positive(previous_gray, &gray)
        {
            motion_positive_frames = motion_positive_frames.saturating_add(1);
        }
        previous = Some(gray);
    }
    MotionGateResult {
        motion_positive_frames,
    }
}

fn codec_for_stream(media: &str, encoding_name: &str) -> Option<VideoCodec> {
    if media != "video" {
        return None;
    }
    match encoding_name.to_ascii_lowercase().as_str() {
        "h264" => Some(VideoCodec::H264),
        "h265" => Some(VideoCodec::H265),
        _ => None,
    }
}

fn decode_annex_b_file(path: &Path, codec: VideoCodec) -> Result<DecodedVideoSegment, String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    decode_encoded_units(codec, vec![bytes], 0.0)
}

fn decode_mp4_file(path: &Path) -> Result<DecodedVideoSegment, String> {
    let file = fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let size = file
        .metadata()
        .map_err(|error| format!("metadata {}: {error}", path.display()))?
        .len();
    let mut reader =
        Mp4Reader::read_header(file, size).map_err(|error| format!("read MP4 header: {error}"))?;
    let (track_id, sample_count, fps, sps, pps, media_type) = {
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
        (
            track_id,
            track.sample_count(),
            track.frame_rate(),
            sps,
            pps,
            media_type,
        )
    };
    if media_type == MediaType::H265 {
        return Err("MP4 H.265 demux is not implemented in the first-light decoder".to_string());
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
    decode_encoded_units(VideoCodec::H264, encoded_units, fps)
}

/// Widened to `pub(crate)` for the fabric detector-work executor
/// (`crate::fabric::DetectorWorkExecutor`), which decodes a claimed job's
/// resolved `encoded_units` blob the same way a local capture session does.
pub(crate) fn decode_encoded_units(
    codec: VideoCodec,
    encoded_units: Vec<Vec<u8>>,
    fps: f64,
) -> Result<DecodedVideoSegment, String> {
    let decoded = match codec {
        VideoCodec::H264 => decode_h264_units(&encoded_units)?,
        VideoCodec::H265 => decode_h265_units(&encoded_units)?,
    };
    if decoded.is_empty() {
        return Err(format!("{codec:?} segment has no decodable frames"));
    }
    Ok(DecodedVideoSegment {
        frames: decoded,
        encoded_units,
        fps,
        observed_at: None,
    })
}

fn decode_h264_units(encoded_units: &[Vec<u8>]) -> Result<Vec<DecodedRgbFrame>, String> {
    let config = H264DecoderConfig::new().flush_after_decode(Flush::NoFlush);
    let mut decoder = H264Decoder::with_api_config(openh264::OpenH264API::from_source(), config)
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
    reindex_frames(&mut frames);
    Ok(frames)
}

fn decode_h265_units(encoded_units: &[Vec<u8>]) -> Result<Vec<DecodedRgbFrame>, String> {
    let mut decoder = H265Decoder::new();
    let mut frames = Vec::new();
    let mut first_error = None;
    for unit in encoded_units {
        for nal in rust_h265::parse_annex_b(unit) {
            match decoder.decode_nal(&nal) {
                Ok(Some(frame)) => frames.push(h265_to_rgb(frame)?),
                Ok(None) => {}
                Err(error) if first_error.is_none() => first_error = Some(error.to_string()),
                Err(_) => {}
            }
        }
    }
    while let Some(frame) = decoder.flush() {
        frames.push(h265_to_rgb(frame)?);
    }
    frames.sort_by_key(|frame| frame.index);
    if frames.is_empty()
        && let Some(error) = first_error
    {
        return Err(format!("decode H.265 frame: {error}"));
    }
    reindex_frames(&mut frames);
    Ok(frames)
}

fn openh264_to_rgb(frame: openh264::decoder::DecodedYUV<'_>) -> Result<DecodedRgbFrame, String> {
    let (width, height) = frame.dimensions();
    let (y_stride, u_stride, v_stride) = frame.strides();
    let y = copy_plane(frame.y(), width, height, y_stride)?;
    let u = copy_plane(frame.u(), width / 2, height / 2, u_stride)?;
    let v = copy_plane(frame.v(), width / 2, height / 2, v_stride)?;
    let width = width as u32;
    let height = height as u32;
    let rgb = yuv420_to_rgb8(&y, &u, &v, width, height, width / 2)?;
    Ok(DecodedRgbFrame {
        index: 0,
        width,
        height,
        rgb,
    })
}

fn h265_to_rgb(frame: rust_h265::Frame) -> Result<DecodedRgbFrame, String> {
    let y = plane_to_u8(&frame.y, frame.bit_depth)?;
    let u = plane_to_u8(&frame.u, frame.bit_depth)?;
    let v = plane_to_u8(&frame.v, frame.bit_depth)?;
    let rgb = yuv420_to_rgb8(&y, &u, &v, frame.width, frame.height, frame.width / 2)?;
    Ok(DecodedRgbFrame {
        index: frame.pic_order_cnt.max(0) as u64,
        width: frame.width,
        height: frame.height,
        rgb,
    })
}

fn plane_to_u8(plane: &rust_h265::PixelData, bit_depth: u8) -> Result<Vec<u8>, String> {
    match plane {
        rust_h265::PixelData::U8(values) => Ok(values.clone()),
        rust_h265::PixelData::U16(values) => {
            let max_value = (1_u32 << bit_depth) - 1;
            if max_value == 0 {
                return Err(format!("invalid H.265 bit depth {bit_depth}"));
            }
            Ok(values
                .iter()
                .map(|value| ((*value as u32 * 255 + max_value / 2) / max_value) as u8)
                .collect())
        }
    }
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
    let mut rgb = Vec::with_capacity(width_usize.saturating_mul(height_usize).saturating_mul(3));
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

fn resize_rgb_frame(frame: &DecodedRgbFrame, width: u32, height: u32) -> Result<Vec<u8>, String> {
    let image = RgbImage::from_raw(frame.width, frame.height, frame.rgb.clone())
        .ok_or_else(|| "decoded RGB frame dimensions do not match buffer length".to_string())?;
    Ok(image::imageops::resize(&image, width, height, FilterType::Triangle).into_raw())
}

fn motion_thumbnail(frame: &DecodedRgbFrame) -> GrayImage {
    let Some(image) = RgbImage::from_raw(frame.width, frame.height, frame.rgb.clone()) else {
        return GrayImage::new(MOTION_WIDTH, MOTION_HEIGHT);
    };
    let resized =
        image::imageops::resize(&image, MOTION_WIDTH, MOTION_HEIGHT, FilterType::Triangle);
    ImageBuffer::from_fn(MOTION_WIDTH, MOTION_HEIGHT, |x, y| {
        let pixel = resized.get_pixel(x, y);
        let luma =
            (77_u32 * pixel[0] as u32 + 150_u32 * pixel[1] as u32 + 29_u32 * pixel[2] as u32 + 128)
                >> 8;
        Luma([luma as u8])
    })
}

fn motion_positive(previous: &GrayImage, current: &GrayImage) -> bool {
    let mut diff_sum = 0_u64;
    let diff = ImageBuffer::from_fn(MOTION_WIDTH, MOTION_HEIGHT, |x, y| {
        let value = current.get_pixel(x, y)[0].abs_diff(previous.get_pixel(x, y)[0]);
        diff_sum = diff_sum.saturating_add(value as u64);
        Luma([value])
    });
    let mask = threshold(&diff, 3, ThresholdType::Binary);
    let labels = connected_components(&mask, Connectivity::Eight, Luma([0_u8]));
    let has_component = labels.pixels().any(|pixel| pixel[0] != 0);
    diff_sum > u64::from(MOTION_WIDTH * MOTION_HEIGHT) * 3 && has_component
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

fn reindex_frames(frames: &mut [DecodedRgbFrame]) {
    for (index, frame) in frames.iter_mut().enumerate() {
        frame.index = index as u64;
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {

    #[test]
    fn boundary_unit_belongs_to_the_segment_it_starts() {
        // The SPS/PPS unit that STARTS segment N carries segment N
        // membership, never N-1.
        let mut counter = 0u64;
        // First boundary: no segment running, unit carries config → next segment.
        let membership = super::next_segment_membership(false, true, counter);
        assert_eq!(membership, 1);
        counter += 1;
        // Units inside the running segment keep its id.
        assert_eq!(super::next_segment_membership(true, false, counter), 1);
        assert_eq!(
            super::next_segment_membership(true, true, counter),
            1,
            "a mid-segment parameter-set repeat stays in the current segment"
        );
        // After the segment flushes, the next boundary starts segment 2.
        assert_eq!(super::next_segment_membership(false, true, counter), 2);
        // A non-boundary unit between segments belongs to the old segment id
        // (it is discarded by assembly, never emitted).
        assert_eq!(super::next_segment_membership(false, false, counter), 1);
    }
    use super::*;

    #[test]
    fn browser_playable_mp4_writer_round_trips_to_h264_mp4() {
        let tmp = tempfile::TempDir::new().expect("create temp dir");
        let path = tmp.path().join("review.mp4");
        let segment = DecodedVideoSegment {
            frames: (0..6)
                .map(|index| synthetic_rgb_frame(index, 64, 48))
                .collect(),
            encoded_units: vec![b"raw-camera-evidence".to_vec()],
            fps: 6.0,
            observed_at: None,
        };

        write_browser_playable_mp4_clip(&segment, &path)
            .expect("write browser-playable review MP4");
        let bytes = std::fs::read(&path).expect("read review MP4");
        assert!(
            bytes.windows(4).any(|window| window == b"avc1"),
            "review MP4 must advertise an H.264 avc1 track"
        );
        assert!(
            bytes.windows(4).any(|window| window == b"moov"),
            "review MP4 must be finalized with a moov box"
        );

        let decoded = decode_video_file(&path).expect("decode generated review MP4");
        assert!(
            decoded.frame_count() >= 2,
            "generated review MP4 must contain multiple decodable frames"
        );
    }

    #[test]
    fn playable_dimensions_cap_large_review_clips_without_upscaling() {
        assert_eq!(
            playable_scaled_dimensions(1920, 1080, PLAYABLE_MAX_WIDTH, PLAYABLE_MAX_HEIGHT),
            (1280, 720)
        );
        assert_eq!(
            playable_scaled_dimensions(1080, 1920, PLAYABLE_MAX_WIDTH, PLAYABLE_MAX_HEIGHT),
            (404, 720)
        );
        assert_eq!(
            playable_scaled_dimensions(640, 360, PLAYABLE_MAX_WIDTH, PLAYABLE_MAX_HEIGHT),
            (640, 360)
        );
    }

    fn synthetic_rgb_frame(index: u64, width: u32, height: u32) -> DecodedRgbFrame {
        let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
        for y in 0..height {
            for x in 0..width {
                let marker =
                    (8 + index as u32..24 + index as u32).contains(&x) && (12..34).contains(&y);
                if marker {
                    rgb.extend_from_slice(&[230, 50, 40]);
                } else {
                    rgb.extend_from_slice(&[
                        ((x * 3 + index as u32) % 255) as u8,
                        ((y * 5) % 255) as u8,
                        80,
                    ]);
                }
            }
        }
        DecodedRgbFrame {
            index,
            width,
            height,
            rgb,
        }
    }

    #[test]
    fn credentialed_rtsp_url_is_stripped_for_session_and_kept_for_auth() {
        let source = prepare_rtsp_source(
            "rtsp://admin:p%40ss%3Aword@192.0.2.10:554/Streaming/Channels/102",
            None,
            None,
        )
        .expect("credentialed URL should prepare");

        assert_eq!(
            source.session_url(),
            "rtsp://192.0.2.10:554/Streaming/Channels/102"
        );
        assert_eq!(
            source.credentials,
            Some(RtspCredentials {
                username: "admin".to_string(),
                password: "p@ss:word".to_string(),
            })
        );
    }

    #[test]
    fn explicit_rtsp_credentials_override_url_userinfo() {
        let source = prepare_rtsp_source(
            "rtsp://wrong:wrong-password@192.0.2.10:554/Streaming/Channels/102",
            Some("operator"),
            Some("camera-secret"),
        )
        .expect("explicit credentials should prepare");

        assert_eq!(
            source.session_url(),
            "rtsp://192.0.2.10:554/Streaming/Channels/102"
        );
        assert_eq!(
            source.credentials,
            Some(RtspCredentials {
                username: "operator".to_string(),
                password: "camera-secret".to_string(),
            })
        );
    }

    #[test]
    fn separate_password_can_pair_with_url_username() {
        let source = prepare_rtsp_source(
            "rtsp://admin@192.0.2.10:554/Streaming/Channels/102",
            None,
            Some("camera-secret"),
        )
        .expect("separate password should pair with URL username");

        assert_eq!(
            source.credentials,
            Some(RtspCredentials {
                username: "admin".to_string(),
                password: "camera-secret".to_string(),
            })
        );
    }

    #[test]
    fn rtsp_url_redaction_removes_userinfo_from_parse_errors() {
        let error = prepare_rtsp_source("rtsp://admin:sec@ret@[", None, None)
            .expect_err("malformed host should fail");

        assert!(!error.contains("admin"));
        assert!(!error.contains("sec"));
        assert!(!error.contains("ret"));
        assert!(error.contains("rtsp://["));
    }

    #[test]
    fn rtsp_credentials_debug_redacts_password() {
        let text = format!(
            "{:?}",
            RtspCredentials {
                username: "admin".to_string(),
                password: "secret".to_string(),
            }
        );

        assert!(text.contains("admin"));
        assert!(!text.contains("secret"));
        assert!(text.contains("<redacted>"));
    }
}
