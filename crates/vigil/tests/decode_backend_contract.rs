//! Decode backend contract: the encoded-access-unit seam carries everything
//! a robust hardware decoder needs; backends are epoch-scoped; the software
//! path preserves today's decode behavior; classification is explicit and
//! an unclassifiable selection never claims hardware; hardware failure
//! activates software fallback with a visible reason.
//!
//! Hardware-active decode itself is proven by the pre-registered owner
//! smoke on a real device, not here.

use vigil::VideoCodec;
use vigil::decode::{
    AccessUnitAssembler, BackendClassification, DecodeBackend, DecodeBackendError,
    EncodedAccessUnit, ProbeOutcome, SoftwareDecodeBackend, select_decode_backend,
};
use vigil::workgraph::StreamId;

/// A minimal synthetic H.264 annex-B stream: SPS + PPS + IDR produced by the
/// OpenH264 encoder at runtime — content-faithful input, no fixture bytes,
/// nothing environment-specific.
fn encoded_h264_units(frames: usize) -> Vec<Vec<u8>> {
    use openh264::encoder::Encoder;
    use openh264::formats::{RgbSliceU8, YUVBuffer};

    let width = 64usize;
    let height = 64usize;
    let mut encoder = Encoder::new().expect("create OpenH264 encoder");
    let mut units = Vec::new();
    for index in 0..frames {
        // A moving gradient so successive frames differ.
        let mut rgb = vec![0u8; width * height * 3];
        for (pixel, chunk) in rgb.chunks_exact_mut(3).enumerate() {
            let x = (pixel % width) as u8;
            chunk[0] = x.wrapping_add(index as u8 * 8);
            chunk[1] = (pixel / width) as u8;
            chunk[2] = 128;
        }
        let yuv = YUVBuffer::from_rgb_source(RgbSliceU8::new(&rgb, (width, height)));
        let bitstream = encoder.encode(&yuv).expect("encode synthetic frame");
        let bytes = bitstream.to_vec();
        if !bytes.is_empty() {
            units.push(bytes);
        }
    }
    assert!(!units.is_empty(), "encoder must produce access units");
    units
}

fn assemble_units(units: Vec<Vec<u8>>, epoch: u64) -> Vec<EncodedAccessUnit> {
    let mut assembler =
        AccessUnitAssembler::new(StreamId::new("front-yard"), VideoCodec::H264, epoch);
    units
        .into_iter()
        .enumerate()
        .map(|(index, data)| assembler.assemble(data, None, index == 0, index as u64 / 48))
        .collect()
}

#[test]
fn encoded_access_unit_carries_epoch_codec_config_keyframe_sequence_markers() {
    let raw = encoded_h264_units(3);
    let units = assemble_units(raw, 5);

    let first = &units[0];
    assert_eq!(first.stream_id.as_str(), "front-yard");
    assert_eq!(first.stream_epoch, 5, "the unit names its stream epoch");
    assert_eq!(first.codec, VideoCodec::H264);
    assert!(
        first.codec_config.is_some(),
        "the first (parameter-set-carrying) unit must expose codec configuration bytes"
    );
    assert!(
        first.keyframe,
        "the IDR-carrying unit must be flagged as a keyframe carrier"
    );
    assert!(
        first.discontinuity,
        "the first unit after (re)connect carries the discontinuity marker"
    );

    let sequences: Vec<u64> = units.iter().map(|unit| unit.sequence).collect();
    let mut sorted = sequences.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sequences, sorted,
        "sequence numbers are monotonic and unique within the epoch"
    );

    for unit in &units[1..] {
        assert!(
            !unit.discontinuity,
            "only the boundary unit is discontinuous"
        );
        assert!(
            !unit.format_change,
            "no format change within a same-parameter-set run"
        );
    }
}

#[test]
fn cross_epoch_units_are_rejected_by_backend() {
    let raw = encoded_h264_units(3);
    let units = assemble_units(raw, 1);

    let mut backend = SoftwareDecodeBackend::new(StreamId::new("front-yard"), VideoCodec::H264, 1)
        .expect("software backend for epoch 1");

    // Same stream, WRONG epoch: a stateful decoder must refuse, not decode.
    let mut foreign_epoch = units[0].clone();
    foreign_epoch.stream_epoch = 2;
    assert_eq!(
        backend.decode(&foreign_epoch),
        Err(DecodeBackendError::EpochViolation {
            expected: 1,
            got: 2
        }),
        "mixing access units from different epochs into one decoder is invalid"
    );

    // Wrong stream entirely.
    let mut foreign_stream = units[0].clone();
    foreign_stream.stream_id = StreamId::new("back-yard");
    assert_eq!(
        backend.decode(&foreign_stream),
        Err(DecodeBackendError::StreamViolation),
        "mixing access units from different streams into one decoder is invalid"
    );
}

#[test]
fn software_backend_preserves_first_light_decode_behavior() {
    let raw = encoded_h264_units(6);
    let units = assemble_units(raw, 1);

    let mut backend = SoftwareDecodeBackend::new(StreamId::new("front-yard"), VideoCodec::H264, 1)
        .expect("software backend");

    assert_eq!(
        backend.classification(),
        BackendClassification::Software {
            element: "software".to_string()
        },
        "the software path is honestly classified Software, never hardware"
    );

    let mut decoded = 0usize;
    let mut width = 0u32;
    for unit in &units {
        let frames = backend.decode(unit).expect("software decode");
        for frame in &frames {
            width = frame.width;
            assert_eq!(
                frame.rgb.len() as u32,
                frame.width * frame.height * 3,
                "decoded frames stay CPU RGB — the shape every existing consumer needs"
            );
        }
        decoded += frames.len();
    }
    assert!(
        decoded > 0,
        "the synthetic stream must decode to RGB frames"
    );
    assert_eq!(width, 64, "decoded dimensions match the encoded stream");
}

#[test]
fn unclassified_selected_decoder_never_claims_hardware() {
    let unknown = BackendClassification::Unclassified {
        element: "mystery-decoder".to_string(),
    };
    assert!(
        !unknown.is_hardware(),
        "an unclassifiable selected decoder must not claim hardware acceleration"
    );

    let software = BackendClassification::Software {
        element: "software".to_string(),
    };
    assert!(!software.is_hardware());

    let hardware = BackendClassification::Hardware {
        element: "vah264dec".to_string(),
        device: Some("/dev/dri/renderD128".to_string()),
    };
    assert!(hardware.is_hardware());
}

#[test]
fn hardware_backend_failure_activates_software_fallback_with_visible_reason() {
    use vigil::acceleration::{FailureCode, ProbeStatus};

    let raw = encoded_h264_units(3);
    let sample = assemble_units(raw, 1);

    // hardware_decoding=true on a build/host where no hardware path is
    // usable (this test never assumes a GPU): selection MUST return the
    // software backend plus a receipt that names the fallback visibly.
    let selection = select_decode_backend(
        &StreamId::new("front-yard"),
        VideoCodec::H264,
        1,
        0,
        true,
        &sample,
    )
    .expect("selection always yields a working decode path");

    assert!(
        !selection.backend.classification().is_hardware() || selection.receipt.hardware_accelerated,
        "classification and receipt must agree"
    );

    let receipt = &selection.receipt;
    assert!(receipt.configured, "intent was hardware_decoding=true");
    if !receipt.hardware_accelerated {
        assert_eq!(receipt.probe_status, ProbeStatus::Fallback);
        assert_ne!(
            receipt.failure_code,
            FailureCode::None,
            "a fallback must carry a classified reason, never silence"
        );
        assert_eq!(receipt.active_backend, "software");
    }

    // Disabled intent: no probe, software selected, status Disabled.
    let disabled = select_decode_backend(
        &StreamId::new("front-yard"),
        VideoCodec::H264,
        1,
        0,
        false,
        &sample,
    )
    .expect("disabled intent still yields the software path");
    assert_eq!(disabled.receipt.probe_status, ProbeStatus::Disabled);
    assert!(!disabled.receipt.configured);
    assert_eq!(disabled.receipt.active_backend, "software");
    assert_eq!(
        disabled.receipt.failure_code,
        FailureCode::None,
        "skipping by intent is not a failure"
    );
}

#[test]
fn probe_reports_decoded_frames_and_classification() {
    let raw = encoded_h264_units(3);
    let sample = assemble_units(raw, 1);

    let mut backend = SoftwareDecodeBackend::new(StreamId::new("front-yard"), VideoCodec::H264, 1)
        .expect("software backend");
    match backend.probe(&sample) {
        ProbeOutcome::Decoded {
            classification,
            frames_decoded,
        } => {
            assert!(frames_decoded > 0, "a passed probe decoded real frames");
            assert!(!classification.is_hardware());
        }
        ProbeOutcome::Failed { reason } => {
            panic!("software probe must pass on a valid synthetic stream: {reason}")
        }
    }
}
