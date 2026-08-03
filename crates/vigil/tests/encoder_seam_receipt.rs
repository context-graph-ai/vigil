//! The shared camera encoder seam: one trait, an openh264 software
//! implementation behind it that genuinely encodes (proven by a real
//! encode→decode round trip and a forced-keyframe test that cannot pass
//! against a stub), and backend selection reported as an achieved-state
//! receipt derived from what selection actually observed — never from the
//! requested setting alone. Mirrors the discipline
//! `vigil::decode::select_decode_backend` already applies on the decode
//! side.

use std::num::NonZeroU32;

use vigil::DecodedRgbFrame;
use vigil::VideoCodec;
use vigil::acceleration::{AccelStage, ProbeStatus};
use vigil::camera_track::{CameraId, EncodedAccessUnit, MediaTiming, SourceRole, TimeBase};
use vigil::decode::{DecodeBackend, SoftwareDecodeBackend};
use vigil::encode::{CameraEncoder, Openh264SoftwareEncoder, select_camera_encoder};
use vigil::workgraph::StreamId;

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;

/// A synthetic RGB frame with real structure (a horizontal gradient),
/// never flat/all-zero — a flat frame lets a broken encoder still emit
/// "plausible looking" bytes by accident.
fn synthetic_rgb_frame(index: u64) -> DecodedRgbFrame {
    let mut rgb = vec![0u8; (WIDTH * HEIGHT * 3) as usize];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let offset = ((y * WIDTH + x) * 3) as usize;
            rgb[offset] = ((x * 4) % 256) as u8;
            rgb[offset + 1] = ((y * 4) % 256) as u8;
            rgb[offset + 2] = 128;
        }
    }
    DecodedRgbFrame {
        index,
        width: WIDTH,
        height: HEIGHT,
        rgb,
    }
}

fn wrap_as_access_unit(
    stream_id: StreamId,
    epoch: u64,
    sequence: u64,
    video_unit: vigil::encode::EncodedVideoUnit,
) -> EncodedAccessUnit {
    EncodedAccessUnit {
        stream_id,
        stream_epoch: epoch,
        codec: video_unit.codec,
        codec_config: video_unit.codec_config,
        keyframe: video_unit.keyframe,
        data: video_unit.data,
        timing: Some(MediaTiming {
            time_base: TimeBase::new(
                NonZeroU32::new(1).expect("1 is non-zero"),
                NonZeroU32::new(30).expect("30 is non-zero"),
            ),
            pts: sequence as i64,
            dts: None,
            duration: None,
        }),
        observed_at: None,
        camera: CameraId::from_usb("test-node", "encoder-seam-test-camera")
            .expect("fixture literal is a durable identity"),
        source_role: SourceRole::Analysis,
        sequence,
        discontinuity: sequence == 0,
        format_change: false,
        segment_sequence: 0,
    }
}

#[test]
fn the_camera_encoder_trait_and_the_openh264_software_backend_exist_behind_it() {
    // Compiles only if `Openh264SoftwareEncoder` really implements
    // `CameraEncoder` — the seam's whole point (adapter producers consume
    // the trait object, never a concrete encoder type).
    fn assert_is_camera_encoder<E: CameraEncoder>() {}
    assert_is_camera_encoder::<Openh264SoftwareEncoder>();

    let encoder = Openh264SoftwareEncoder::new(640, 480).expect("construct the software encoder");
    assert_eq!(encoder.id(), "openh264-software");
}

/// The real proof point Fix 3 requires: an openh264 encode of a real,
/// structured RGB frame, fed straight into the software DECODE backend
/// (`vigil::decode::SoftwareDecodeBackend`, the same production decoder
/// used on the analysis path) — this cannot pass while `encode()` is a
/// stub, because a stub either errors or produces bytes the decoder
/// rejects outright.
#[test]
fn a_real_encode_round_trips_through_the_production_software_decoder() {
    let mut encoder =
        Openh264SoftwareEncoder::new(WIDTH, HEIGHT).expect("construct the software encoder");

    let first_unit = encoder
        .encode(&synthetic_rgb_frame(0))
        .expect("encoding the first frame of a stream must succeed");
    assert!(
        first_unit.keyframe,
        "the first frame encoded on a fresh encoder must be a keyframe"
    );
    assert!(
        first_unit.codec_config.is_some(),
        "a keyframe must carry codec configuration (SPS/PPS)"
    );
    assert!(
        !first_unit.data.is_empty(),
        "an encoded keyframe must carry non-empty payload bytes"
    );
    assert_eq!(first_unit.codec, VideoCodec::H264);

    let stream_id = StreamId::new("encoder-seam-round-trip");
    let access_unit = wrap_as_access_unit(stream_id.clone(), 1, 0, first_unit);

    let mut decoder = SoftwareDecodeBackend::new(stream_id, VideoCodec::H264, 1)
        .expect("construct the production software decode backend");
    let decoded_frames = decoder
        .decode(&access_unit)
        .expect("the production decoder must accept a real openh264 encode of a real frame");
    assert!(
        !decoded_frames.is_empty(),
        "decoding a real encoded keyframe must yield at least one RGB frame"
    );
    let decoded = &decoded_frames[0];
    assert_eq!(decoded.width, WIDTH);
    assert_eq!(decoded.height, HEIGHT);
}

/// `request_keyframe` must genuinely force the NEXT encoded frame to be an
/// IDR/keyframe even though it would ordinarily be a delta frame — a stub
/// implementation of `request_keyframe` (a no-op) cannot pass this, because
/// the second frame of a stream is a delta frame by default.
#[test]
fn request_keyframe_forces_the_next_encoded_frame_to_be_a_keyframe() {
    let mut encoder =
        Openh264SoftwareEncoder::new(WIDTH, HEIGHT).expect("construct the software encoder");

    let first = encoder
        .encode(&synthetic_rgb_frame(0))
        .expect("encode first frame");
    assert!(first.keyframe, "the first frame is always a keyframe");

    let second = encoder
        .encode(&synthetic_rgb_frame(1))
        .expect("encode second frame");
    assert!(
        !second.keyframe,
        "sanity: the second frame of a stream is ordinarily a delta frame, not a keyframe — \
         otherwise this test cannot distinguish a real force from a no-op"
    );

    encoder.request_keyframe();
    let forced = encoder
        .encode(&synthetic_rgb_frame(2))
        .expect("encode the frame after request_keyframe");
    assert!(
        forced.keyframe,
        "request_keyframe must force the NEXT encoded frame to be a keyframe, even though it \
         would ordinarily be a delta frame"
    );
    assert!(
        forced.codec_config.is_some(),
        "a forced keyframe must carry codec configuration exactly like a natural one"
    );
}

#[test]
fn a_selection_on_a_build_with_no_hardware_encoder_reports_software_regardless_of_the_requested_setting()
 {
    // This is the test that would FAIL if the receipt were populated from
    // the requested flag rather than from what selection actually
    // achieved: `hardware_encoding` is requested TRUE, but this test host
    // has no proven hardware encode path, so the ACHIEVED state must be
    // software, never a receipt that echoes the request back unchanged.
    let selection = select_camera_encoder(640, 480, true)
        .expect("selection always yields a working encode path");

    assert_eq!(
        selection.encoder.id(),
        "openh264-software",
        "the achieved backend must be the honest software fallback, not the requested hardware path"
    );
    assert_eq!(
        selection.receipt.stage,
        AccelStage::Encode,
        "an encoder receipt must be classified as the Encode stage, never misfiled as Decode/Detection"
    );
    assert!(
        !selection.receipt.hardware_accelerated,
        "hardware_accelerated must reflect what was OBSERVED (no working hardware encoder), \
         never the requested setting (which was true)"
    );
    assert_eq!(
        selection.receipt.active_backend, "software",
        "active_backend names what is actually running, not what was asked for"
    );
    // The requested INTENT is still visible on the receipt (so an operator
    // can see hardware was asked for and not achieved) — `configured` is
    // allowed to reflect the request; only the ACHIEVED fields above may
    // not.
    assert!(
        selection.receipt.configured,
        "the receipt still honestly records that hardware was requested"
    );
    // Honesty of WHY, not just WHAT: no hardware camera-encode backend is
    // compiled into this seam at all — there is no runtime dependency
    // that could be installed to fix this. `MissingRuntimeDependency`
    // would send an operator chasing a dependency that was never the
    // problem; the honest classification is `UnsupportedByThisArtifact`,
    // mirroring `decode.rs`'s own no-hardware-backend-compiled fallback
    // receipt.
    assert_eq!(
        selection.receipt.failure_code,
        vigil::acceleration::FailureCode::UnsupportedByThisArtifact,
        "no hardware camera-encode backend is compiled at all, so the honest failure code is \
         UnsupportedByThisArtifact, never MissingRuntimeDependency (which implies installing \
         something would fix it)"
    );
    assert_eq!(
        selection.receipt.evidence_kind,
        Some(vigil::acceleration::EvidenceKind::SelectedBackend),
        "the evidence must name what selection actually observed (the backend it selected), \
         matching the failure code"
    );
    assert_eq!(
        selection.receipt.action_kind,
        vigil::acceleration::ActionKind::InstallSupportedArtifact,
        "the matching operator action is installing a different artifact, not running a \
         dependency-install command"
    );
    assert!(
        selection.receipt.action_payload.is_some(),
        "an actionable receipt names the fix, not just the failure"
    );
}

#[test]
fn disabled_intent_never_probes_and_is_reported_as_disabled_not_a_failure() {
    let selection =
        select_camera_encoder(640, 480, false).expect("software-only selection always succeeds");

    assert_eq!(selection.encoder.id(), "openh264-software");
    assert_eq!(selection.receipt.stage, AccelStage::Encode);
    assert!(
        !selection.receipt.configured,
        "intent was hardware_encoding=false"
    );
    assert_eq!(
        selection.receipt.probe_status,
        ProbeStatus::Disabled,
        "skipping a hardware probe by intent is not a failure — it must be reported Disabled"
    );
}
