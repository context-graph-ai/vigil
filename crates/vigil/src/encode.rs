//! The shared camera encoder seam.
//!
//! USB, CSI, and MJPEG producers each need one H.264 encode of their
//! captured frames; this module is the ONE place that encode happens —
//! adapter producers consume this seam, they never each author an encoder
//! (encode and decode each live at the edges of the track, once each).
//! Backend selection carries the same acceleration discipline `crate::decode`
//! already uses for decode: a selection is reported as an achieved-state
//! receipt derived from what was actually observed, never from the
//! requested setting alone.
//!
//! [`CameraEncoder::encode`] deliberately returns [`EncodedVideoUnit`], not
//! a whole `EncodedAccessUnit`: camera identity, epoch, sequence, and
//! timing belong to the producer/assembler that owns the stream, never to
//! the encoder. An encoder only knows encoded bytes, codec configuration,
//! and whether the frame it just produced is a keyframe.

use bytes::Bytes;

use crate::acceleration::{
    AccelStage, AccelerationReceipt, ActionKind, EvidenceKind, FailureCode, ProbeStatus,
};
use crate::media_pipeline::{DecodedRgbFrame, VideoCodec};
use openh264::formats::YUVSource as _;

/// The multiplier applied to the effective output frame rate to derive the
/// automatic keyframe interval, in OUTPUT FRAMES — never a duration or
/// timer (a live viewer joins on a decodable stream boundary, not a wall
/// clock). Owner-ratified automatic default (2026-08-01). This is the
/// SINGLE literal home of that default: `config::declare_settings`'s
/// `keyframe_interval_fps_multiplier` setting spec reads its `default`
/// straight from this const rather than repeating `2` a second time, and
/// [`automatic_keyframe_interval_frames`] takes the settings-resolved
/// value as a parameter instead of reading this const in its own
/// arithmetic — so an owner pin actually reaches the computation once a
/// producer calls it with `RuntimeConfig`'s resolved field.
pub const KEYFRAME_INTERVAL_FPS_MULTIPLIER_AUTOMATIC_DEFAULT: u32 = 2;

/// The minimum automatic keyframe interval, in output frames. Also the
/// single source `camera_hub::AUTOMATIC_CAPACITY_FLOOR_FRAMES` derives
/// from, rather than duplicating this literal a second time.
pub const KEYFRAME_INTERVAL_MIN_FRAMES_AUTOMATIC_DEFAULT: u32 = 15;

/// The maximum automatic keyframe interval, in output frames.
pub const KEYFRAME_INTERVAL_MAX_FRAMES_AUTOMATIC_DEFAULT: u32 = 300;

/// The automatic keyframe interval, in OUTPUT FRAMES, for a stream encoded
/// at `effective_fps`: `multiplier × effective_fps`, clamped to
/// `[min_frames, max_frames]`. At 30 fps and the ratified defaults (2×,
/// clamped 15..=300) that is 60 frames. This is deliberately a frame
/// COUNT: Vigil's live join contract is stated in stream terms (codec
/// configuration, then the next keyframe), never as a wall-clock deadline,
/// so the interval that governs it must be counted in frames too.
///
/// `multiplier`/`min_frames`/`max_frames` are NOT read from a module
/// constant here: they are the operator-adjustable values
/// `config::declare_settings` declares
/// (`keyframe_interval_fps_multiplier`/`_min_frames`/`_max_frames`) and
/// resolves onto `RuntimeConfig` — this function is the point of use a
/// caller reaches with those resolved values, so a manual pin on any of
/// the three actually changes what this function computes.
pub fn automatic_keyframe_interval_frames(
    effective_fps: f64,
    multiplier: u32,
    min_frames: u32,
    max_frames: u32,
) -> u32 {
    let raw = (effective_fps * f64::from(multiplier)).round();
    let raw = if raw.is_finite() { raw } else { 0.0 };
    let frames = raw.clamp(f64::from(min_frames), f64::from(max_frames));
    frames as u32
}

/// Automatic bitrate, in bits per second, by resolution class. Owner-
/// ratified automatic default (2026-08-01); each is the SINGLE literal
/// home its own `config::declare_settings` bitrate setting reads as its
/// `default`, mirroring
/// [`KEYFRAME_INTERVAL_FPS_MULTIPLIER_AUTOMATIC_DEFAULT`].
pub const BITRATE_BPS_UP_TO_640X480_AUTOMATIC_DEFAULT: u32 = 1_000_000;
pub const BITRATE_BPS_UP_TO_1280X720_AUTOMATIC_DEFAULT: u32 = 2_000_000;
pub const BITRATE_BPS_UP_TO_1920X1080_AUTOMATIC_DEFAULT: u32 = 4_000_000;
pub const BITRATE_BPS_UP_TO_2560X1440_AUTOMATIC_DEFAULT: u32 = 6_000_000;
pub const BITRATE_BPS_ABOVE_2560X1440_AUTOMATIC_DEFAULT: u32 = 10_000_000;

/// The automatic bitrate, in bits per second, for a `width`x`height`
/// encode, selecting among the five operator-adjustable per-resolution-
/// class values `config::declare_settings` resolves onto `RuntimeConfig`
/// (`bitrate_bps_up_to_640x480` .. `bitrate_bps_above_2560x1440`) — the
/// same point-of-use discipline as
/// [`automatic_keyframe_interval_frames`]: this function reads only its
/// parameters, never a module constant, so a manual pin on any class
/// actually changes what a caller with real resolved values gets back.
pub fn automatic_bitrate_bps(
    width: u32,
    height: u32,
    bitrate_bps_up_to_640x480: u32,
    bitrate_bps_up_to_1280x720: u32,
    bitrate_bps_up_to_1920x1080: u32,
    bitrate_bps_up_to_2560x1440: u32,
    bitrate_bps_above_2560x1440: u32,
) -> u32 {
    let pixels = u64::from(width) * u64::from(height);
    if pixels <= 640 * 480 {
        bitrate_bps_up_to_640x480
    } else if pixels <= 1280 * 720 {
        bitrate_bps_up_to_1280x720
    } else if pixels <= 1920 * 1080 {
        bitrate_bps_up_to_1920x1080
    } else if pixels <= 2560 * 1440 {
        bitrate_bps_up_to_2560x1440
    } else {
        bitrate_bps_above_2560x1440
    }
}

/// What one [`CameraEncoder::encode`] call produced: encoded bytes, codec
/// configuration, and keyframe facts — nothing about stream/camera
/// identity, epoch, sequence, or timing, all of which belong to the
/// producer/assembler that wraps this into a real
/// `crate::camera_track::EncodedAccessUnit`.
#[derive(Debug, Clone)]
pub struct EncodedVideoUnit {
    pub codec: VideoCodec,
    /// Parameter-set bytes (SPS/PPS), present exactly on a keyframe.
    pub codec_config: Option<Bytes>,
    pub keyframe: bool,
    pub data: Bytes,
}

/// The pixel format of a [`RawFrame`], covering the source-neutral inputs
/// a non-H.264 adapter can hand to the shared encoder seam directly,
/// without first converting to packed RGB itself: `Nv12` and `Yuyv` are
/// the formats a real USB/UVC or CSI device commonly hands back, and
/// `Rgb24` covers a source (or a test) that already has packed RGB.
/// Adding a future format is an additive enum arm, never a parallel
/// frame type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 4:2:0 semi-planar: one full-resolution luma (Y) plane, followed by
    /// one half-resolution, half-height plane of interleaved U/V byte
    /// pairs — two entries in [`RawFrame::planes`].
    Nv12,
    /// 4:2:2 packed single-plane: byte order Y0 U0 Y1 V0 per horizontal
    /// pixel pair — one entry in [`RawFrame::planes`].
    Yuyv,
    /// 24-bit packed single-plane RGB, byte order R G B per pixel — one
    /// entry in [`RawFrame::planes`].
    Rgb24,
}

/// One memory plane of a [`RawFrame`]: where it starts inside
/// [`RawFrame::data`] and how many bytes separate the start of one row
/// from the start of the next. `stride` is deliberately carried
/// separately from `width`: a real V4L2/libcamera buffer is frequently
/// padded WIDER than `width` times the format's bytes-per-pixel (for
/// DMA/alignment), so a plane description — or an implementation reading
/// one — that assumes `stride == width` silently reads garbage into every
/// row after the first the moment a device actually pads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneLayout {
    /// Byte offset of this plane's first row within `RawFrame::data`.
    pub offset: usize,
    /// Bytes from the start of one row to the start of the next, in this
    /// plane — NOT necessarily `width` (or `width` times bytes-per-pixel).
    pub stride: usize,
}

/// A source-neutral raw camera frame, described by its REAL memory
/// layout rather than a pixel-format tag over assumed-packed bytes: one
/// shared backing buffer (`data`), one [`PlaneLayout`] per plane the
/// `format` requires (indexing into `data` by that plane's own
/// `offset`/`stride`), plus the frame's logical `width`/`height`. This is
/// the ONE input shape every USB/UVC, CSI, and HTTP-MJPEG producer hands
/// to [`CameraEncoder::encode_raw`] — each converts its native device
/// buffer into this description of its OWN layout, never into packed RGB
/// first, removing a wasteful round trip every adapter would otherwise
/// author for itself.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// One entry per plane `format` requires, in format-defined order
    /// (e.g. `Nv12` is exactly `[luma, chroma]`).
    pub planes: Vec<PlaneLayout>,
    /// The single backing buffer every plane's `offset`/`stride` indexes
    /// into.
    pub data: Bytes,
}

/// Effective per-stream encode settings the shared encoder consumes
/// DIRECTLY, rather than deriving or holding its own numbers: the
/// operator-adjustable bitrate and keyframe-interval levers
/// `config::declare_settings` resolves onto `RuntimeConfig`
/// (`bitrate_bps_up_to_*`, `keyframe_interval_*_frames`) are turned into
/// concrete per-stream values ONCE, by a caller that knows the stream's
/// resolution and effective frame rate (the same point-of-use discipline
/// [`automatic_keyframe_interval_frames`]/[`automatic_bitrate_bps`]
/// already establish), and handed to the encoder as this struct — the
/// encoder never re-derives a bitrate from a resolution-class table of
/// its own once it has been given one here.
///
/// `effective_fps` is the stream's own observed/configured frame rate.
/// It is deliberately DISTINCT from [`RATE_CONTROL_FRAME_RATE_HINT_HZ`]
/// (see that constant's own doc comment): no caller threads a real
/// stream frame rate through this seam yet, so `encode.rs` does not
/// derive that hint from this field today — a caller wiring a real
/// producer through [`Openh264SoftwareEncoder::with_config`] is the
/// point where that standing follow-up becomes reachable, and doing so
/// is an explicit, named change, never a silent repurposing of the
/// existing hint constant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncoderConfig {
    pub effective_fps: f64,
    pub bitrate_bps: u32,
    /// The OUTPUT-FRAME period [`CameraEncoder::encode_raw`] must honor
    /// automatically: a keyframe every `keyframe_interval_frames`
    /// SUCCESSFULLY encoded output frames, counted since the last
    /// keyframe (manual or automatic) — never by a timer, and never
    /// advanced by an attempted input that failed to encode.
    pub keyframe_interval_frames: u32,
}

/// One encoder backend behind the shared seam. `openh264` is the only
/// software fallback: it is BSD-2-Clause and already a dependency, so no
/// GPL encoder is loaded; a hardware backend may be added later behind the
/// same trait without changing any adapter producer.
pub trait CameraEncoder {
    /// Stable backend identifier for receipts.
    fn id(&self) -> &'static str;

    /// Encode one decoded RGB frame.
    fn encode(&mut self, frame: &DecodedRgbFrame) -> Result<EncodedVideoUnit, String>;

    /// Encode one source-neutral [`RawFrame`], described by its real
    /// memory layout — planes, strides, offsets, dimensions, and pixel
    /// format. An implementation MUST read every plane strictly through
    /// its own [`PlaneLayout::offset`]/[`PlaneLayout::stride`], never by
    /// assuming a plane's stride equals `frame.width` (or `frame.width`
    /// times the format's bytes-per-pixel): a real V4L2/libcamera buffer
    /// is routinely padded wider than that for DMA/alignment, and an
    /// implementation that assumes otherwise silently corrupts every row
    /// after the first the moment a device actually pads.
    ///
    /// Like `encode`, this returns `Err` (without invoking the codec) on
    /// a `frame.width`/`frame.height` mismatch against the encoder's own
    /// configured dimensions, and otherwise applies the SAME shared,
    /// backend-owned periodic keyframe schedule described on
    /// [`EncoderConfig::keyframe_interval_frames`] — every adapter that
    /// calls this method gets that scheduling for free, instead of each
    /// source lane reimplementing (and drifting on) its own counter.
    fn encode_raw(&mut self, frame: &RawFrame) -> Result<EncodedVideoUnit, String>;

    /// Request the next encoded unit be a keyframe — used both for the
    /// configured keyframe interval and for an immediate subscriber-join
    /// keyframe (a join on a Vigil-owned encoder requests an immediate
    /// keyframe).
    fn request_keyframe(&mut self);
}

/// The pure-software fallback: openh264, reused from the same dependency
/// already in tree for decode and clip review encoding. Always classified
/// as the honest software backend.
pub struct Openh264SoftwareEncoder {
    width: u32,
    height: u32,
    encoder: openh264::encoder::Encoder,
    /// The configured periodic-keyframe period, in successfully encoded
    /// output frames — see [`EncoderConfig::keyframe_interval_frames`].
    /// `new` (no [`EncoderConfig`] given) sets this to `u32::MAX`,
    /// preserving `new`'s pre-existing behavior exactly: manual
    /// [`CameraEncoder::request_keyframe`] calls still work, but nothing
    /// fires an automatic periodic keyframe on a `new`-constructed
    /// encoder, matching every test already written against it.
    #[allow(dead_code)]
    keyframe_interval_frames: u32,
    /// How many output frames have been SUCCESSFULLY encoded since the
    /// last keyframe (manual or automatic) — never advanced by an
    /// attempted input that failed to encode. Reset to 0 whenever a
    /// keyframe is produced.
    #[allow(dead_code)]
    frames_since_keyframe: u32,
}

/// `openh264::encoder::EncoderConfig::max_frame_rate`'s construction-time
/// value. This is NOT the "frame rate follows the source, no resampling"
/// policy, and is never claimed to be: it feeds openh264's own internal
/// rate-control budget (`fMaxFrameRate`/the per-spatial-layer `fFrameRate`
/// in the vendored `openh264-sys2` encode parameters), which tells the
/// codec how to spread its configured BITRATE across frames — it does not
/// cause openh264 to drop, duplicate, or otherwise resample anything.
/// `Openh264SoftwareEncoder::encode` calls the underlying encoder exactly
/// once per `encode()` invocation, full stop; how many times a caller
/// invokes it — one call per decoded input frame, at the source's own
/// rate, never resampled — is a fact about the ASSEMBLER that will one day
/// drive this seam, entirely outside this constructor's control. No caller
/// of `Openh264SoftwareEncoder::new` today knows the real source frame
/// rate at construction time: the whole `crate::encode` seam is dormant
/// (no producer wired to it yet, `width`/`height` are its only real
/// inputs), and frame rate itself has no operator-adjustable default to
/// fall back to either — investigated for the same settings-registry pass
/// that made the keyframe-interval/bitrate defaults configurable, and
/// found to have no ratified value at all, only the fixed "one output
/// frame per input frame" policy (see `docs/configuration.md`'s Video
/// encoding fields section). A named constant, not a second bare literal,
/// so the one place this rate-control assumption lives is greppable.
const RATE_CONTROL_FRAME_RATE_HINT_HZ: f32 = 30.0;

impl Openh264SoftwareEncoder {
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        // See RATE_CONTROL_FRAME_RATE_HINT_HZ's own doc comment: this is a
        // rate-control budgeting hint, not the resampling policy, and the
        // real keyframe interval is applied by the caller via
        // `request_keyframe`, never derived here from a guess.
        //
        // The bitrate here uses the AUTOMATIC DEFAULT per-class constants,
        // not a resolved `RuntimeConfig` value: this constructor has no
        // config context (no producer wires a real stream through this
        // seam yet, matching the rate-control hint above), so it cannot
        // reach an owner's pin. It is still never a duplicated literal —
        // these are the exact same named constants
        // `config::declare_settings`'s bitrate settings use as their own
        // `default`, so there is one number, not two independently typed
        // copies of it, and the day a producer wires this constructor to
        // real config it becomes a single call-site change, not a value
        // hunt.
        let config = openh264::encoder::EncoderConfig::new()
            .max_frame_rate(openh264::encoder::FrameRate::from_hz(
                RATE_CONTROL_FRAME_RATE_HINT_HZ,
            ))
            .bitrate(openh264::encoder::BitRate::from_bps(automatic_bitrate_bps(
                width,
                height,
                BITRATE_BPS_UP_TO_640X480_AUTOMATIC_DEFAULT,
                BITRATE_BPS_UP_TO_1280X720_AUTOMATIC_DEFAULT,
                BITRATE_BPS_UP_TO_1920X1080_AUTOMATIC_DEFAULT,
                BITRATE_BPS_UP_TO_2560X1440_AUTOMATIC_DEFAULT,
                BITRATE_BPS_ABOVE_2560X1440_AUTOMATIC_DEFAULT,
            )));
        let encoder = openh264::encoder::Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            config,
        )
        .map_err(|error| format!("create OpenH264 encoder: {error}"))?;
        Ok(Self {
            width,
            height,
            encoder,
            keyframe_interval_frames: u32::MAX,
            frames_since_keyframe: 0,
        })
    }

    /// Construct the software encoder from a resolved [`EncoderConfig`]:
    /// `bitrate_bps` and `effective_fps` feed the underlying codec's rate
    /// control DIRECTLY instead of `new`'s resolution-class automatic
    /// defaults, and `keyframe_interval_frames` becomes the OUTPUT-FRAME
    /// period [`CameraEncoder::encode_raw`] honors automatically (see
    /// that method's own doc comment) — counted by successfully encoded
    /// output frames only, never by attempted inputs or a timer.
    pub fn with_config(width: u32, height: u32, config: EncoderConfig) -> Result<Self, String> {
        // Unlike `new`, a real per-stream frame rate IS known here (it is
        // `config.effective_fps`, not a guess), so it feeds the codec's
        // rate-control budget directly instead of the fixed
        // `RATE_CONTROL_FRAME_RATE_HINT_HZ` hint `new` still uses. The
        // requested bitrate is consumed directly too, and
        // `RateControlMode::Bitrate` is selected explicitly so that
        // number actually governs the encode rather than being a soft
        // hint under the quality-driven default rate-control mode.
        let encoder_config = openh264::encoder::EncoderConfig::new()
            .max_frame_rate(openh264::encoder::FrameRate::from_hz(
                config.effective_fps as f32,
            ))
            .bitrate(openh264::encoder::BitRate::from_bps(config.bitrate_bps))
            .rate_control_mode(openh264::encoder::RateControlMode::Bitrate);
        let encoder = openh264::encoder::Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            encoder_config,
        )
        .map_err(|error| format!("create OpenH264 encoder: {error}"))?;
        Ok(Self {
            width,
            height,
            encoder,
            keyframe_interval_frames: config.keyframe_interval_frames,
            frames_since_keyframe: 0,
        })
    }
}

/// Copies one plane's real pixel bytes for `row` (`byte_len` bytes, honest
/// to the plane's own `offset`/`stride` — never `width`) out of `data`.
/// The single read primitive [`raw_frame_to_i420`] and its per-format
/// helpers route through, so no call site can accidentally assume
/// `stride == width`.
fn plane_row<'a>(
    data: &'a [u8],
    plane: &PlaneLayout,
    row: usize,
    byte_len: usize,
) -> Result<&'a [u8], String> {
    let start = plane
        .offset
        .checked_add(
            row.checked_mul(plane.stride)
                .ok_or("plane row offset overflow")?,
        )
        .ok_or("plane row offset overflow")?;
    let end = start
        .checked_add(byte_len)
        .ok_or("plane row end overflow")?;
    data.get(start..end)
        .ok_or_else(|| format!("plane row [{start}..{end}) is out of bounds of the frame data"))
}

/// Owned, tightly packed 4:2:0 planar Y/U/V buffers: `(y, u, v)`.
type OwnedI420Planes = (Vec<u8>, Vec<u8>, Vec<u8>);

/// Converts a [`RawFrame`] (in whatever `format` it declares) into owned,
/// tightly packed 4:2:0 planar Y/U/V buffers the codec can consume,
/// reading every source row strictly through its own
/// [`PlaneLayout::offset`]/[`PlaneLayout::stride`] — never assuming a
/// plane's stride equals `width` (or `width` times the format's
/// bytes-per-pixel).
fn raw_frame_to_i420(frame: &RawFrame) -> Result<OwnedI420Planes, String> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let chroma_width = width / 2;
    let chroma_height = height / 2;

    match frame.format {
        PixelFormat::Nv12 => {
            let [y_plane, uv_plane] = frame.planes.as_slice() else {
                return Err(format!(
                    "NV12 requires exactly 2 planes, got {}",
                    frame.planes.len()
                ));
            };

            let mut y_out = vec![0u8; width * height];
            for row in 0..height {
                let src = plane_row(&frame.data, y_plane, row, width)?;
                y_out[row * width..(row + 1) * width].copy_from_slice(src);
            }

            let mut u_out = vec![0u8; chroma_width * chroma_height];
            let mut v_out = vec![0u8; chroma_width * chroma_height];
            for uv_row in 0..chroma_height {
                let src = plane_row(&frame.data, uv_plane, uv_row, chroma_width * 2)?;
                for pair in 0..chroma_width {
                    u_out[uv_row * chroma_width + pair] = src[pair * 2];
                    v_out[uv_row * chroma_width + pair] = src[pair * 2 + 1];
                }
            }
            Ok((y_out, u_out, v_out))
        }
        PixelFormat::Yuyv => {
            let [plane] = frame.planes.as_slice() else {
                return Err(format!(
                    "YUYV requires exactly 1 plane, got {}",
                    frame.planes.len()
                ));
            };

            let mut y_out = vec![0u8; width * height];
            let mut u_out = vec![0u8; chroma_width * chroma_height];
            let mut v_out = vec![0u8; chroma_width * chroma_height];
            for row in 0..height {
                let src = plane_row(&frame.data, plane, row, width * 2)?;
                for pair in 0..(width / 2) {
                    let base = pair * 4;
                    let y0 = src[base];
                    let u = src[base + 1];
                    let y1 = src[base + 2];
                    let v = src[base + 3];
                    y_out[row * width + pair * 2] = y0;
                    y_out[row * width + pair * 2 + 1] = y1;
                    // 4:2:2 carries chroma at full vertical resolution;
                    // 4:2:0 needs it at half. Take the even source rows
                    // as the representative chroma sample for the pair
                    // of output rows they cover — never averaging with a
                    // row read through the wrong stride.
                    if row % 2 == 0 {
                        let uv_row = row / 2;
                        u_out[uv_row * chroma_width + pair] = u;
                        v_out[uv_row * chroma_width + pair] = v;
                    }
                }
            }
            Ok((y_out, u_out, v_out))
        }
        PixelFormat::Rgb24 => {
            let [plane] = frame.planes.as_slice() else {
                return Err(format!(
                    "RGB24 requires exactly 1 plane, got {}",
                    frame.planes.len()
                ));
            };

            let mut packed = vec![0u8; width * height * 3];
            for row in 0..height {
                let src = plane_row(&frame.data, plane, row, width * 3)?;
                packed[row * width * 3..(row + 1) * width * 3].copy_from_slice(src);
            }
            let rgb_source = openh264::formats::RgbSliceU8::new(&packed, (width, height));
            let yuv = openh264::formats::YUVBuffer::from_rgb8_source(rgb_source);
            Ok((yuv.y().to_vec(), yuv.u().to_vec(), yuv.v().to_vec()))
        }
    }
}

impl CameraEncoder for Openh264SoftwareEncoder {
    fn id(&self) -> &'static str {
        "openh264-software"
    }

    fn encode(&mut self, frame: &DecodedRgbFrame) -> Result<EncodedVideoUnit, String> {
        if frame.width != self.width || frame.height != self.height {
            return Err(format!(
                "frame dimensions {}x{} do not match the encoder's configured {}x{}",
                frame.width, frame.height, self.width, self.height
            ));
        }
        let rgb_source = openh264::formats::RgbSliceU8::new(
            &frame.rgb,
            (self.width as usize, self.height as usize),
        );
        let yuv = openh264::formats::YUVBuffer::from_rgb_source(rgb_source);
        let stream = self
            .encoder
            .encode(&yuv)
            .map_err(|error| format!("openh264 encode: {error}"))?;

        let keyframe = stream.frame_type() == openh264::encoder::FrameType::IDR;
        // `data` carries the COMPLETE Annex-B access unit — every layer's
        // NALs, config layers included — matching the same contract the
        // RTSP assembler's `data` already carries (`decode.rs::assemble`,
        // whose own doc comment is explicit that parameter sets are
        // extracted into `codec_config` WITHOUT being removed from `data`).
        // `SoftwareDecodeBackend`/`StreamingDecoder::decode_unit` decode
        // straight from `data` via `openh264::nal_units`, so a decoder
        // that never saw the SPS/PPS NALs (because they were only in a
        // separate, never-delivered `codec_config`) could not decode a
        // single frame — `codec_config` is a CONVENIENCE COPY for late
        // joiners/hub retention, never the exclusive home of those bytes.
        let mut codec_config: Option<Vec<u8>> = None;
        let mut payload = Vec::new();
        for layer_index in 0..stream.num_layers() {
            let Some(layer) = stream.layer(layer_index) else {
                continue;
            };
            if !layer.is_video() && keyframe {
                let config_bytes = codec_config.get_or_insert_with(Vec::new);
                for nal_index in 0..layer.nal_count() {
                    if let Some(nal) = layer.nal_unit(nal_index) {
                        config_bytes.extend_from_slice(nal);
                    }
                }
            }
            for nal_index in 0..layer.nal_count() {
                if let Some(nal) = layer.nal_unit(nal_index) {
                    payload.extend_from_slice(nal);
                }
            }
        }

        Ok(EncodedVideoUnit {
            codec: VideoCodec::H264,
            codec_config: codec_config.map(Bytes::from),
            keyframe,
            data: Bytes::from(payload),
        })
    }

    fn encode_raw(&mut self, frame: &RawFrame) -> Result<EncodedVideoUnit, String> {
        if frame.width != self.width || frame.height != self.height {
            return Err(format!(
                "frame dimensions {}x{} do not match the encoder's configured {}x{}",
                frame.width, frame.height, self.width, self.height
            ));
        }

        let (y_plane, u_plane, v_plane) = raw_frame_to_i420(frame)?;
        let width = self.width as usize;
        let height = self.height as usize;
        let chroma_width = width / 2;
        let yuv = openh264::formats::YUVSlices::new(
            (&y_plane, &u_plane, &v_plane),
            (width, height),
            (width, chroma_width, chroma_width),
        );

        // The automatic periodic schedule, requested BEFORE the encode
        // call (exactly like a manual `request_keyframe`) — never after,
        // and never for an attempted-but-failed input. `frames_since_keyframe`
        // counts successful encodes since the last keyframe; this
        // upcoming encode is the one that COMPLETES a full
        // `keyframe_interval_frames` period once `frames_since_keyframe`
        // has already reached `keyframe_interval_frames - 1` (the fresh
        // encoder's own first frame is always naturally a keyframe
        // without needing this force, matching `frames_since_keyframe`
        // starting at 0 and `keyframe_interval_frames` never being 0 in
        // practice).
        if self.frames_since_keyframe >= self.keyframe_interval_frames.saturating_sub(1)
            && self.keyframe_interval_frames > 0
        {
            self.encoder.force_intra_frame();
        }

        let stream = self
            .encoder
            .encode(&yuv)
            .map_err(|error| format!("openh264 encode: {error}"))?;

        let keyframe = stream.frame_type() == openh264::encoder::FrameType::IDR;
        // Only a SUCCESSFUL encode reaches here, so only a successful
        // encode ever advances or resets this counter — see the struct
        // field's own doc comment.
        if keyframe {
            self.frames_since_keyframe = 0;
        } else {
            self.frames_since_keyframe += 1;
        }

        let mut codec_config: Option<Vec<u8>> = None;
        let mut payload = Vec::new();
        for layer_index in 0..stream.num_layers() {
            let Some(layer) = stream.layer(layer_index) else {
                continue;
            };
            if !layer.is_video() && keyframe {
                let config_bytes = codec_config.get_or_insert_with(Vec::new);
                for nal_index in 0..layer.nal_count() {
                    if let Some(nal) = layer.nal_unit(nal_index) {
                        config_bytes.extend_from_slice(nal);
                    }
                }
            }
            for nal_index in 0..layer.nal_count() {
                if let Some(nal) = layer.nal_unit(nal_index) {
                    payload.extend_from_slice(nal);
                }
            }
        }

        Ok(EncodedVideoUnit {
            codec: VideoCodec::H264,
            codec_config: codec_config.map(Bytes::from),
            keyframe,
            data: Bytes::from(payload),
        })
    }

    fn request_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }
}

/// A selected encoder backend plus the achieved-state receipt describing
/// how it was actually selected — never derived from the requested
/// setting, only from what was observed.
pub struct EncoderSelection {
    pub encoder: Box<dyn CameraEncoder>,
    pub receipt: AccelerationReceipt,
}

/// Select the camera encoder backend for one adapter source. `width` and
/// `height` are the target encode dimensions; `hardware_encoding` is the
/// operator's intent, mirroring `hardware_decoding`'s absent-means-true
/// semantics — but the returned receipt's `hardware_accelerated` and
/// `active_backend` fields reflect what this selection actually achieved,
/// never this requested flag directly: no hardware camera-encode backend
/// exists behind this seam yet, so the achieved backend is always the
/// honest openh264 software fallback, and a `true` request is reported as
/// `configured: true` alongside `hardware_accelerated: false` rather than
/// silently echoed back as if it were satisfied.
pub fn select_camera_encoder(
    width: u32,
    height: u32,
    hardware_encoding: bool,
) -> Result<EncoderSelection, String> {
    let encoder = Openh264SoftwareEncoder::new(width, height)?;
    let probe_status = if hardware_encoding {
        ProbeStatus::Fallback
    } else {
        ProbeStatus::Disabled
    };
    let receipt = AccelerationReceipt {
        stage: AccelStage::Encode,
        work_id: None,
        parent_work_id: None,
        stream_id: None,
        media_item: None,
        configured: hardware_encoding,
        attempted_backend: if hardware_encoding {
            "hardware-camera-encoder".to_string()
        } else {
            "openh264-software".to_string()
        },
        active_backend: "software".to_string(),
        hardware_accelerated: false,
        selected_device: None,
        codec: Some("h264".to_string()),
        model_id: None,
        model_version: None,
        input_shape: None,
        probe_status,
        // No hardware camera-encode backend is COMPILED into this seam at
        // all (unlike decode's gstreamer-feature-gated hardware path,
        // which can be present-but-failing and so genuinely earn
        // `MissingRuntimeDependency`/`ProbeFailed`). There is nothing
        // missing to install a dependency for — the honest classification
        // mirrors `decode.rs`'s own `#[cfg(not(feature = "decode-gstreamer"))]`
        // fallback receipt: `UnsupportedByThisArtifact`, naming what IS
        // compiled in as the evidence, never a dependency that was never
        // the problem.
        failure_code: if hardware_encoding {
            FailureCode::UnsupportedByThisArtifact
        } else {
            FailureCode::None
        },
        evidence_kind: if hardware_encoding {
            Some(EvidenceKind::SelectedBackend)
        } else {
            None
        },
        evidence_fields: if hardware_encoding {
            std::collections::BTreeMap::from([(
                "compiled_encode_backends".to_string(),
                "software".to_string(),
            )])
        } else {
            std::collections::BTreeMap::new()
        },
        action_kind: if hardware_encoding {
            ActionKind::InstallSupportedArtifact
        } else {
            ActionKind::NoAction
        },
        action_payload: if hardware_encoding {
            Some(
                "install a hardware-camera-encode-enabled artifact, or keep software encode"
                    .to_string(),
            )
        } else {
            None
        },
    };
    Ok(EncoderSelection {
        encoder: Box::new(encoder),
        receipt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves `automatic_keyframe_interval_frames` actually reads its
    /// `multiplier`/`min_frames`/`max_frames` PARAMETERS rather than a
    /// module constant: an operator-pinned multiplier of 4 changes the
    /// unclamped result, and both bounds are exercised at values distinct
    /// from the ratified defaults so a hardcoded-default regression would
    /// fail this test even though it would still pass against the
    /// defaults alone.
    #[test]
    fn automatic_keyframe_interval_frames_uses_the_multiplier_and_clamps_to_the_given_bounds() {
        assert_eq!(
            automatic_keyframe_interval_frames(30.0, 2, 15, 300),
            60,
            "the ratified defaults at 30fps must still compute 2x30=60"
        );
        assert_eq!(
            automatic_keyframe_interval_frames(30.0, 4, 15, 300),
            120,
            "a pinned multiplier of 4 must actually change the unclamped result to 4x30=120"
        );
        assert_eq!(
            automatic_keyframe_interval_frames(1.0, 2, 20, 300),
            20,
            "a low fps must clamp UP to a pinned min_frames of 20, not the ratified 15"
        );
        assert_eq!(
            automatic_keyframe_interval_frames(200.0, 2, 15, 100),
            100,
            "a high fps must clamp DOWN to a pinned max_frames of 100, not the ratified 300"
        );
    }

    /// Proves `automatic_bitrate_bps` selects among its FIVE per-class
    /// parameters, not a module constant, at every resolution-class
    /// boundary — with values distinct from the ratified defaults so a
    /// hardcoded-default regression would fail this even though it would
    /// still pass against the defaults alone.
    #[test]
    fn automatic_bitrate_bps_selects_by_resolution_class_from_the_given_table() {
        let (c1, c2, c3, c4, c5) = (11, 22, 33, 44, 55);
        assert_eq!(automatic_bitrate_bps(640, 480, c1, c2, c3, c4, c5), c1);
        assert_eq!(automatic_bitrate_bps(1280, 720, c1, c2, c3, c4, c5), c2);
        assert_eq!(automatic_bitrate_bps(1920, 1080, c1, c2, c3, c4, c5), c3);
        assert_eq!(automatic_bitrate_bps(2560, 1440, c1, c2, c3, c4, c5), c4);
        assert_eq!(automatic_bitrate_bps(3840, 2160, c1, c2, c3, c4, c5), c5);
    }
}
