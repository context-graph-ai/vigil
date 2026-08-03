//! The encoded-camera-track contract: every field the shared seam is
//! required to carry, and the two structural promises that make fan-out
//! cheap and honest — the payload is an immutable shared buffer (no
//! per-consumer deep copy), and source presentation timing is never
//! conflated with wall-clock observation time.

use std::num::NonZeroU32;

use bytes::Bytes;
use chrono::TimeZone;
use chrono::Utc;

use vigil::VideoCodec;
use vigil::camera_track::{CameraId, EncodedAccessUnit, MediaTiming, SourceRole, TimeBase};
use vigil::workgraph::StreamId;

fn sample_timing() -> MediaTiming {
    MediaTiming {
        time_base: TimeBase::new(
            NonZeroU32::new(1).expect("1 is non-zero"),
            NonZeroU32::new(90_000).expect("90000 is non-zero"),
        ),
        pts: 270_000,
        dts: Some(261_000),
        duration: Some(3_000),
    }
}

fn base_unit(data: Bytes) -> EncodedAccessUnit {
    EncodedAccessUnit {
        stream_id: StreamId::new("front-gate"),
        stream_epoch: 3,
        codec: VideoCodec::H264,
        codec_config: Some(Bytes::from(vec![0, 0, 0, 1, 0x67])),
        keyframe: true,
        data,
        timing: Some(sample_timing()),
        observed_at: Some(Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()),
        camera: CameraId::from_usb("test-node", "front-gate-durable-id")
            .expect("fixture literal is a durable identity"),
        source_role: SourceRole::Analysis,
        sequence: 42,
        discontinuity: false,
        format_change: false,
        segment_sequence: 7,
    }
}

#[test]
fn encoded_access_unit_carries_camera_identity_and_source_role() {
    let unit = base_unit(Bytes::from(vec![1, 2, 3]));

    assert_eq!(
        unit.camera,
        CameraId::from_usb("test-node", "front-gate-durable-id")
            .expect("fixture literal is a durable identity")
    );
    assert_eq!(unit.source_role, SourceRole::Analysis);

    let live_unit = EncodedAccessUnit {
        source_role: SourceRole::Live,
        ..base_unit(Bytes::from(vec![1, 2, 3]))
    };
    assert_eq!(live_unit.source_role, SourceRole::Live);
    assert_ne!(
        unit.source_role, live_unit.source_role,
        "the same camera's analysis and live roles are distinguishable on the unit"
    );
}

#[test]
fn encoded_access_unit_carries_codec_and_codec_configuration_bytes() {
    let unit = base_unit(Bytes::from(vec![9, 9, 9]));

    assert_eq!(unit.codec, VideoCodec::H264);
    assert!(
        unit.codec_config.is_some(),
        "a unit carrying parameter sets exposes codec configuration bytes"
    );

    let no_config_unit = EncodedAccessUnit {
        codec_config: None,
        ..base_unit(Bytes::from(vec![9, 9, 9]))
    };
    assert!(
        no_config_unit.codec_config.is_none(),
        "a delta unit carries no codec configuration"
    );
}

#[test]
fn encoded_access_unit_carries_keyframe_epoch_discontinuity_format_change_and_sequence() {
    let unit = base_unit(Bytes::from(vec![4, 5, 6]));

    assert!(unit.keyframe);
    assert_eq!(unit.stream_epoch, 3);
    assert!(!unit.discontinuity);
    assert!(!unit.format_change);
    assert_eq!(unit.sequence, 42);
    assert_eq!(unit.segment_sequence, 7);

    let boundary_unit = EncodedAccessUnit {
        discontinuity: true,
        format_change: true,
        sequence: 0,
        stream_epoch: 4,
        ..base_unit(Bytes::from(vec![4, 5, 6]))
    };
    assert!(
        boundary_unit.discontinuity,
        "a reconnect/timeline-break boundary unit is marked discontinuous"
    );
    assert!(
        boundary_unit.format_change,
        "a unit whose parameter sets differ from the previous ones is marked format-changed"
    );
    assert_eq!(
        boundary_unit.stream_epoch, 4,
        "a new epoch is a distinct number under the same stream identity"
    );
    assert_eq!(
        boundary_unit.stream_id, unit.stream_id,
        "stream/camera identity is stable across an epoch bump"
    );
}

#[test]
fn encoded_access_unit_carries_source_timing_with_timebase_decode_order_and_duration() {
    let unit = base_unit(Bytes::from(vec![7, 7]));
    let timing = unit.timing.expect("this unit carries source timing");

    assert_eq!(timing.time_base.numerator.get(), 1);
    assert_eq!(timing.time_base.denominator.get(), 90_000);
    assert_eq!(timing.pts, 270_000);
    assert_eq!(
        timing.dts,
        Some(261_000),
        "decode order is carried separately from presentation order when the codec needs it"
    );
    assert_eq!(timing.duration, Some(3_000));
}

#[test]
fn payload_and_codec_config_are_immutable_shared_buffers_never_deep_copied_on_fan_out() {
    let data = Bytes::from(vec![10u8; 64]);
    let unit = base_unit(data.clone());

    // Simulate fan-out to N subscribers: cloning the unit (as the hub does
    // per subscriber) must share the SAME backing storage, never allocate
    // a copy. `Bytes` has no `Arc::ptr_eq`-shaped API, but two `Bytes`
    // sharing an allocation always report the identical data pointer, so
    // pointer equality on `as_ptr()` is the shared-storage proof for this
    // type.
    let fanned_out: Vec<EncodedAccessUnit> = (0..5).map(|_| unit.clone()).collect();
    for consumer_copy in &fanned_out {
        assert_eq!(
            consumer_copy.data.as_ptr(),
            unit.data.as_ptr(),
            "every fanned-out consumer must share the identical payload allocation"
        );
    }
    assert_eq!(
        unit.data.as_ptr(),
        data.as_ptr(),
        "constructing the unit must not copy the payload either"
    );

    let codec_config = unit.codec_config.clone().expect("codec config present");
    for consumer_copy in &fanned_out {
        let consumer_config = consumer_copy
            .codec_config
            .clone()
            .expect("codec config present on every fanned-out copy");
        assert_eq!(
            consumer_config.as_ptr(),
            codec_config.as_ptr(),
            "codec configuration bytes are shared the same way the payload is"
        );
    }
}

/// Against the original `Vec<u8>` payload type this test compiles and
/// FAILS (a `Vec<u8>::clone()` always allocates and copies) — a genuine
/// behavioral RED rather than a compile error, because `as_ptr()` exists
/// on both `Vec<u8>` and `Bytes`. That is what proves adopting `Bytes` is
/// what fixed the per-frame copy on the capture path, not merely a type
/// rename that happens to keep everything else green.
#[test]
fn cloning_a_unit_shares_the_original_payload_storage_rather_than_copying_it() {
    let unit = base_unit(Bytes::from(vec![42u8; 128]));
    let cloned = unit.clone();

    assert_eq!(
        cloned.data.as_ptr(),
        unit.data.as_ptr(),
        "cloning an EncodedAccessUnit must share the original payload's backing storage, \
         never allocate a copy"
    );
}

#[test]
fn observed_at_and_source_timing_are_independent_and_never_derive_each_other() {
    // A unit can carry real source timing with NO wall-clock observation
    // time (the RTSP path, today, before it fills in RTP timing).
    let timing_only = EncodedAccessUnit {
        timing: Some(sample_timing()),
        observed_at: None,
        ..base_unit(Bytes::from(vec![1]))
    };
    assert!(timing_only.timing.is_some());
    assert!(
        timing_only.observed_at.is_none(),
        "source timing must not manufacture an observation time"
    );

    // A unit can carry a wall-clock observation time with NO source timing
    // at all (a producer that has not yet filled real presentation time).
    let observed_only = EncodedAccessUnit {
        timing: None,
        observed_at: Some(Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()),
        ..base_unit(Bytes::from(vec![1]))
    };
    assert!(
        observed_only.timing.is_none(),
        "a wall-clock observation must not manufacture source timing"
    );
    assert!(observed_only.observed_at.is_some());

    // Both present, and with unrelated values, proves neither is derived
    // from the other's clock or numeric domain.
    let both = EncodedAccessUnit {
        timing: Some(sample_timing()),
        observed_at: Some(Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()),
        ..base_unit(Bytes::from(vec![1]))
    };
    let timing = both.timing.expect("timing present");
    assert_eq!(
        timing.pts, 270_000,
        "source PTS stays in its own media timebase, unaffected by observed_at existing"
    );
    assert!(both.observed_at.is_some());
}
