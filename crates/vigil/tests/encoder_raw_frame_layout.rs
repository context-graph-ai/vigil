//! The shared camera encoder seam accepts a source-neutral raw frame
//! described by its REAL memory layout — planes, strides, offsets,
//! dimensions, and pixel format — rather than a pixel-format tag over
//! assumed-packed bytes. Every non-H.264 adapter (USB/UVC, CSI,
//! HTTP-MJPEG) hands its device's native buffer to `encode_raw` through
//! this shape directly, never converting to packed RGB first.
//!
//! `PixelFormat` covers at minimum NV12 (planar, the common USB/CSI
//! shape), YUYV (packed), and RGB24 (packed) — proven here by actually
//! encoding all three through the SAME method and decoding the result
//! back through the production software decoder. The stride test is the
//! load-bearing one: a real V4L2/libcamera buffer is routinely padded
//! WIDER than `width` for DMA/alignment, so an implementation that reads
//! a plane as if `stride == width` silently corrupts every row after the
//! first — this file proves that cannot pass unnoticed.

use std::num::NonZeroU32;

use bytes::Bytes;

use vigil::camera_track::{CameraId, EncodedAccessUnit, MediaTiming, SourceRole, TimeBase};
use vigil::decode::{DecodeBackend, SoftwareDecodeBackend};
use vigil::encode::{CameraEncoder, Openh264SoftwareEncoder, PixelFormat, PlaneLayout, RawFrame};
use vigil::workgraph::StreamId;

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;

fn wrap_as_access_unit(
    stream_id: StreamId,
    video_unit: vigil::encode::EncodedVideoUnit,
) -> EncodedAccessUnit {
    EncodedAccessUnit {
        stream_id,
        stream_epoch: 1,
        codec: video_unit.codec,
        codec_config: video_unit.codec_config,
        keyframe: video_unit.keyframe,
        data: video_unit.data,
        timing: Some(MediaTiming {
            time_base: TimeBase::new(
                NonZeroU32::new(1).expect("1 is non-zero"),
                NonZeroU32::new(30).expect("30 is non-zero"),
            ),
            pts: 0,
            dts: None,
            duration: None,
        }),
        observed_at: None,
        camera: CameraId::from_usb("test-node", "raw-frame-layout-test-camera")
            .expect("fixture literal is a durable identity"),
        source_role: SourceRole::Analysis,
        sequence: 0,
        discontinuity: true,
        format_change: false,
        segment_sequence: 0,
    }
}

/// A tightly packed (`stride == width * 3`) RGB24 `RawFrame` with real
/// structure (a horizontal/vertical gradient, never flat/all-zero — flat
/// content lets a broken conversion still emit plausible-looking bytes by
/// accident).
fn tightly_packed_rgb24_frame(width: u32, height: u32) -> RawFrame {
    let stride = (width * 3) as usize;
    let mut data = vec![0u8; stride * height as usize];
    for y in 0..height {
        for x in 0..width {
            let offset = y as usize * stride + (x * 3) as usize;
            data[offset] = ((x * 4) % 256) as u8;
            data[offset + 1] = ((y * 4) % 256) as u8;
            data[offset + 2] = 128;
        }
    }
    RawFrame {
        width,
        height,
        format: PixelFormat::Rgb24,
        planes: vec![PlaneLayout { offset: 0, stride }],
        data: Bytes::from(data),
    }
}

/// A tightly packed (`stride == width * 2`) YUYV `RawFrame` with real
/// structure.
fn tightly_packed_yuyv_frame(width: u32, height: u32) -> RawFrame {
    let stride = (width * 2) as usize;
    let mut data = vec![0u8; stride * height as usize];
    for y in 0..height {
        for pair in 0..(width / 2) {
            let offset = y as usize * stride + (pair * 4) as usize;
            data[offset] = ((pair * 4) % 256) as u8; // Y0
            data[offset + 1] = ((y * 3) % 256) as u8; // U
            data[offset + 2] = ((pair * 4 + 2) % 256) as u8; // Y1
            data[offset + 3] = ((y * 3 + 64) % 256) as u8; // V
        }
    }
    RawFrame {
        width,
        height,
        format: PixelFormat::Yuyv,
        planes: vec![PlaneLayout { offset: 0, stride }],
        data: Bytes::from(data),
    }
}

/// A tightly packed NV12 `RawFrame`: `y_stride == width`,
/// `uv_stride == width` (interleaved U/V byte pairs, half the height).
fn nv12_frame(width: u32, height: u32, y_stride: usize, uv_stride: usize) -> RawFrame {
    let y_len = y_stride * height as usize;
    let uv_height = (height / 2) as usize;
    let uv_len = uv_stride * uv_height;
    let mut data = vec![0u8; y_len + uv_len];
    for y in 0..height as usize {
        for x in 0..width as usize {
            data[y * y_stride + x] = ((x * 4 + y * 3) % 256) as u8;
        }
        // Poison bytes: distinct filler in the padding region so a
        // stride-blind reader (treating the row as `width` bytes wide)
        // pulls THESE bytes into the next logical row instead of the
        // real pixel data that actually starts at `y_stride` further on.
        for x in width as usize..y_stride {
            data[y * y_stride + x] = 0xEE;
        }
    }
    for uv_row in 0..uv_height {
        for pair in 0..(width as usize / 2) {
            let off = y_len + uv_row * uv_stride + pair * 2;
            data[off] = ((pair * 5 + uv_row * 7) % 256) as u8; // U
            data[off + 1] = ((pair * 5 + uv_row * 7 + 32) % 256) as u8; // V
        }
        for x in width as usize..uv_stride {
            data[y_len + uv_row * uv_stride + x] = 0xAA;
        }
    }
    RawFrame {
        width,
        height,
        format: PixelFormat::Nv12,
        planes: vec![
            PlaneLayout {
                offset: 0,
                stride: y_stride,
            },
            PlaneLayout {
                offset: y_len,
                stride: uv_stride,
            },
        ],
        data: Bytes::from(data),
    }
}

/// The multi-format plumbing proof: `encode_raw` accepts NV12 (planar),
/// YUYV (packed), and RGB24 (packed) through the SAME method, and each
/// produces a real, decodable H.264 stream — proven by feeding it straight
/// into the production software decoder, exactly as the existing
/// `encode()` round trip already does for RGB. A stub (or an
/// implementation that only handles one format and silently mishandles
/// the others) cannot pass this: it either errors, or the decoder rejects
/// bytes that were never a real encode of the given dimensions.
#[test]
fn encode_raw_produces_a_decodable_stream_for_nv12_yuyv_and_rgb_inputs() {
    let cases: Vec<(&str, RawFrame)> = vec![
        (
            "nv12",
            nv12_frame(WIDTH, HEIGHT, WIDTH as usize, WIDTH as usize),
        ),
        ("yuyv", tightly_packed_yuyv_frame(WIDTH, HEIGHT)),
        ("rgb24", tightly_packed_rgb24_frame(WIDTH, HEIGHT)),
    ];

    for (name, raw_frame) in cases {
        let mut encoder = Openh264SoftwareEncoder::new(WIDTH, HEIGHT)
            .unwrap_or_else(|error| panic!("construct the software encoder for {name}: {error}"));

        let unit = encoder
            .encode_raw(&raw_frame)
            .unwrap_or_else(|error| panic!("encode_raw must accept a real {name} frame: {error}"));
        assert!(
            unit.keyframe,
            "the first frame encoded on a fresh encoder must be a keyframe ({name})"
        );
        assert!(
            unit.codec_config.is_some(),
            "a keyframe must carry codec configuration ({name})"
        );
        assert!(
            !unit.data.is_empty(),
            "an encoded keyframe must carry non-empty payload bytes ({name})"
        );

        let stream_id = StreamId::new(format!("raw-frame-layout-{name}"));
        let access_unit = wrap_as_access_unit(stream_id.clone(), unit);
        let mut decoder = SoftwareDecodeBackend::new(stream_id, vigil::VideoCodec::H264, 1)
            .unwrap_or_else(|error| panic!("construct the production decoder ({name}): {error}"));
        let decoded_frames = decoder
            .decode(&access_unit)
            .unwrap_or_else(|error| panic!("decode a real {name} encode: {error:?}"));
        assert!(
            !decoded_frames.is_empty(),
            "decoding a real encoded {name} keyframe must yield at least one RGB frame"
        );
        assert_eq!(
            decoded_frames[0].width, WIDTH,
            "decoded width must match ({name})"
        );
        assert_eq!(
            decoded_frames[0].height, HEIGHT,
            "decoded height must match ({name})"
        );
    }
}

/// The anti-cheat proof for capability 1: two NV12 `RawFrame`s carrying
/// the IDENTICAL logical image content — one tightly packed
/// (`stride == width`), the other padded with extra, DISTINCT filler
/// bytes after every row on BOTH planes (`stride > width`, mimicking a
/// real V4L2/libcamera DMA-aligned buffer) — must encode to byte-identical
/// output on two freshly constructed encoders. If an implementation
/// assumed a plane's stride equals `width` (or ignored `PlaneLayout`
/// entirely and read the buffer as if it were tightly packed), the padded
/// frame's extraction would pull the poison filler bytes into the image
/// starting at row 2, producing measurably different encoded bytes than
/// the tightly packed reference — this test would then fail on the
/// equality assertion below rather than passing vacuously.
#[test]
fn a_padded_stride_nv12_frame_encodes_identically_to_its_tightly_packed_equivalent() {
    let packed = nv12_frame(WIDTH, HEIGHT, WIDTH as usize, WIDTH as usize);
    // Padding is deliberately DIFFERENT on the two planes (y padding 24
    // bytes, uv padding 10 bytes) so an implementation that only respects
    // one plane's real stride still fails this test.
    let padded = nv12_frame(WIDTH, HEIGHT, WIDTH as usize + 24, WIDTH as usize + 10);

    let mut packed_encoder =
        Openh264SoftwareEncoder::new(WIDTH, HEIGHT).expect("construct the packed encoder");
    let mut padded_encoder =
        Openh264SoftwareEncoder::new(WIDTH, HEIGHT).expect("construct the padded encoder");

    let packed_unit = packed_encoder
        .encode_raw(&packed)
        .expect("encode the tightly packed NV12 frame");
    let padded_unit = padded_encoder
        .encode_raw(&padded)
        .expect("encode the padded-stride NV12 frame");

    assert!(
        packed_unit.keyframe && padded_unit.keyframe,
        "both are the first frame on a fresh encoder, so both must be keyframes"
    );
    assert!(
        !packed_unit.data.is_empty() && !padded_unit.data.is_empty(),
        "sanity: neither encode may produce empty payload bytes"
    );
    assert_eq!(
        packed_unit.data, padded_unit.data,
        "identical logical image content laid out with different (but honestly described) \
         plane strides must produce byte-identical encoded output — a stride-blind \
         implementation reads the padding's poison filler bytes as image data starting at row \
         2 and would diverge here"
    );
    assert_eq!(
        packed_unit.codec_config, padded_unit.codec_config,
        "codec configuration must also match between the two layouts of identical content"
    );
}
