//! Work class #2 (`vigil.detector`) payload schema contract (criterion C1,
//! payload-schema half). Job rows must register onto the shared ledger under
//! `class:vigil.detector` and must carry frame REFERENCES only — never raw
//! frame bytes. This file is RED: `detector_workclass` is skeleton-only
//! (`todo!()` bodies) pending the ledger-wiring implementation.

use vigil::VideoCodec;
use vigil::detector_workclass::{
    DETECTOR_CLASS_TAG, DETECTOR_MODE, DETECTOR_SCHEMA_VERSION, DETECTOR_WORK_CLASS,
    DetectorDetection, DetectorJobBuilder, DetectorResult, FrameBlobRef, OrderedF64,
    WireResultEnvelope, WireVideoCodec, WireWorkEnvelope, decode_length_framed_units,
    detector_job_spec_fields, encode_length_framed_units,
};

fn wire_envelope() -> WireWorkEnvelope {
    WireWorkEnvelope {
        work_id: "0192f6a0-0000-7000-8000-000000000001".to_string(),
        parent_work_id: Some("0192f6a0-0000-7000-8000-000000000000".to_string()),
        contributing_work_ids: vec!["0192f6a0-0000-7000-8000-000000000002".to_string()],
        stage: "detection".to_string(),
        stream_id: "front-yard".to_string(),
        ordering_stream_epoch: 1,
        ordering_stream_sequence: 42,
        schema_version: DETECTOR_SCHEMA_VERSION,
    }
}

fn built_job() -> vigil::detector_workclass::DetectorJob {
    DetectorJobBuilder::new(
        wire_envelope(),
        FrameBlobRef("blake3:deadbeefcafefeedfacefeed".to_string()),
    )
    .codec(WireVideoCodec::from(VideoCodec::H264))
    .fps(15.0)
    .sample_frames(8)
    .confidence_threshold(0.5)
    .clip_sha256("a".repeat(64))
    .decoded_frames_sha256("b".repeat(64))
    .model_id("yolox-tiny-coco".to_string())
    .build()
}

/// C1 anchor: a job built through the ONLY public path registers under the
/// `vigil.detector` work class, the `object-detection` mode, and the
/// `class:vigil.detector` requirement tag — and its frame reference is a
/// [`FrameBlobRef`], never raw bytes. There is no builder method and
/// no `DetectorJob` field that accepts `Vec<u8>` frame bytes; the refs-only
/// promise is enforced by the type the builder accepts, not by a runtime
/// check on this test's own construction.
#[test]
fn detector_job_registers_as_vigil_detector_class_refs_only() {
    let job = built_job();

    assert_eq!(job.schema_version, DETECTOR_SCHEMA_VERSION);
    assert_eq!(
        job.frames_blob_ref,
        FrameBlobRef("blake3:deadbeefcafefeedfacefeed".to_string()),
        "frames must be named by a content-addressed reference, never inline bytes"
    );

    let spec_fields = detector_job_spec_fields(&job);
    assert_eq!(
        spec_fields.work_class, DETECTOR_WORK_CLASS,
        "job must register under the vigil.detector work class"
    );
    assert_eq!(spec_fields.work_class, "vigil.detector");
    assert_eq!(
        spec_fields.mode, DETECTOR_MODE,
        "job must register under the object-detection mode"
    );
    assert_eq!(spec_fields.mode, "object-detection");
    assert_eq!(
        spec_fields.requirement_tags,
        vec![DETECTOR_CLASS_TAG.to_string()],
        "claiming workers must be selected by the class:vigil.detector tag, refs-only"
    );
    assert_eq!(spec_fields.requirement_tags, vec!["class:vigil.detector"]);
}

/// C1: `DetectorJob` round-trips through the wire (serde) shape bit-exact —
/// the schema is a byte contract between nodes, not merely an in-process
/// struct.
#[test]
fn detector_job_serde_round_trips_bit_exact() {
    let job = built_job();
    let encoded = serde_json::to_string(&job).expect("DetectorJob must serialize");
    let decoded: vigil::detector_workclass::DetectorJob =
        serde_json::from_str(&encoded).expect("DetectorJob must deserialize");
    assert_eq!(
        decoded, job,
        "a DetectorJob must round-trip byte-for-byte through the wire shape"
    );
}

/// C1: `DetectorResult` round-trips through the wire (serde) shape
/// bit-exact, carrying full detector-attempt provenance so class-map
/// authority and NMS ownership stay vigil-side regardless of which node ran
/// the model.
#[test]
fn detector_result_serde_round_trips_bit_exact() {
    let result = DetectorResult {
        schema_version: DETECTOR_SCHEMA_VERSION,
        result_envelope: WireResultEnvelope {
            work_id: "0192f6a0-0000-7000-8000-000000000001".to_string(),
            parent_work_id: Some("0192f6a0-0000-7000-8000-000000000000".to_string()),
            contributing_work_ids: vec![],
            stage: "detection".to_string(),
            stream_id: "front-yard".to_string(),
            result_schema_version: DETECTOR_SCHEMA_VERSION,
            receipt_id: "0192f6a0-0000-7000-8000-0000000000ff".to_string(),
        },
        detections: vec![DetectorDetection {
            class_name: "person".to_string(),
            confidence: OrderedF64(0.91),
            bbox: "10,10,50,80".to_string(),
            frame_index: 3,
        }],
        detector_backend: "burn-cpu".to_string(),
        detector_session_id: "session-1".to_string(),
        model_sha256: "c".repeat(64),
        model_forward_sha256: "d".repeat(64),
        detector_nms_sha256: "e".repeat(64),
        result_sha256: "f".repeat(64),
        clip_sha256: "a".repeat(64),
    };

    let encoded = serde_json::to_string(&result).expect("DetectorResult must serialize");
    let decoded: DetectorResult =
        serde_json::from_str(&encoded).expect("DetectorResult must deserialize");
    assert_eq!(
        decoded, result,
        "a DetectorResult must round-trip byte-for-byte through the wire shape"
    );
}

/// C1: the moved artifact is the compressed `encoded_units`, length-framed
/// for blob content-addressing — round-trips exactly, including an empty
/// unit and a multi-unit segment.
#[test]
fn length_framed_encoded_units_round_trip() {
    let units: Vec<Vec<u8>> = vec![vec![0x00, 0x00, 0x00, 0x01, 0x67], vec![], vec![0xAB; 300]];
    let framed = encode_length_framed_units(&units);
    let decoded = decode_length_framed_units(&framed).expect("framed blob must decode");
    assert_eq!(
        decoded, units,
        "length-framed encoded_units must round-trip exactly, including empty units"
    );
}
