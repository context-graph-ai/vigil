//! The decoder backend seam.
//!
//! Stream ingress (Retina) owns the RTSP session and depacketization and
//! produces an explicit [`EncodedAccessUnit`] sequence; a [`DecodeBackend`]
//! turns that sequence into decoded RGB frames plus a decode receipt. The
//! backend owns codec state only within one stream epoch: mixing access
//! units from different streams or epochs into one backend is invalid.
//!
//! `hardware_decoding=true` means: probe, and use hardware only when a real
//! decode probe succeeds AND the selected decoder is explicitly classified
//! hardware. An unclassifiable selection never claims hardware. Every
//! hardware failure falls back to software decode with a visible reason, and
//! both paths emit the same scheduler-ready decoded work shape.

use chrono::{DateTime, Utc};

use crate::media_pipeline::{DecodedRgbFrame, VideoCodec};
use crate::workgraph::StreamId;

// The canonical encoded-unit type lives in `camera_track`; re-exported here
// so every existing `crate::decode::EncodedAccessUnit` path keeps compiling
// unchanged.
pub use crate::camera_track::{CameraId, EncodedAccessUnit, MediaTiming, SourceRole, TimeBase};

/// Derives the access-unit contract fields from raw depacketized units.
/// Pure: same bytes in, same flags out.
///
/// Camera identity and source role are fixed for the lifetime of one
/// assembler (set via [`AccessUnitAssembler::new`] or defaulted from the
/// stream identity); real source `MediaTiming` is not derived here, so it
/// stays `None` on the RTSP path. It is filled when the source producer
/// learns the camera's media timeline, and is never substituted with the
/// wall clock.
pub struct AccessUnitAssembler {
    stream_id: StreamId,
    codec: VideoCodec,
    stream_epoch: u64,
    camera: CameraId,
    source_role: SourceRole,
    next_sequence: u64,
    previous_parameter_sets: Option<Vec<u8>>,
}

impl AccessUnitAssembler {
    pub fn new(stream_id: StreamId, codec: VideoCodec, stream_epoch: u64) -> Self {
        // Defaulted camera identity from the stream identity: today's only
        // caller (the retina capture path) has one stream per camera, so
        // this is an honest placeholder, not a guess. A producer that knows
        // its real camera/role should use `with_camera_role`. Built through
        // the same durable-identity constructor every real source kind
        // uses (never the removed unrestricted `CameraId::new`); the
        // literal fallback only engages for a stream identity that cannot
        // itself become a durable value (empty, or a transient `/dev/...`
        // path), and can never fail.
        let camera = CameraId::from_usb("unassigned", stream_id.as_str()).unwrap_or_else(|_| {
            CameraId::from_usb("unassigned", "unnamed-stream")
                .expect("a fixed non-empty, non-/dev/ literal always yields a durable identity")
        });
        Self {
            stream_id,
            codec,
            stream_epoch,
            camera,
            source_role: SourceRole::Analysis,
            next_sequence: 0,
            previous_parameter_sets: None,
        }
    }

    /// Set the real camera identity and source role this assembler
    /// produces units for. Chainable so a producer can set it right after
    /// `new`.
    pub fn with_camera_role(mut self, camera: CameraId, source_role: SourceRole) -> Self {
        self.camera = camera;
        self.source_role = source_role;
        self
    }

    /// Wrap the next raw unit, deriving parameter-set presence, keyframe
    /// flag, format-change marker, sequence, and segment membership.
    pub fn assemble(
        &mut self,
        data: Vec<u8>,
        observed_at: Option<DateTime<Utc>>,
        discontinuity: bool,
        segment_sequence: u64,
    ) -> EncodedAccessUnit {
        let codec_config = extract_parameter_sets(self.codec, &data);
        let format_change = match (&codec_config, &self.previous_parameter_sets) {
            (Some(current), Some(previous)) => current != previous,
            _ => false,
        };
        if let Some(current) = &codec_config {
            self.previous_parameter_sets = Some(current.clone());
        }
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        EncodedAccessUnit {
            stream_id: self.stream_id.clone(),
            stream_epoch: self.stream_epoch,
            codec: self.codec,
            keyframe: unit_carries_keyframe(self.codec, &data),
            codec_config: codec_config.map(Into::into),
            timing: None,
            observed_at,
            camera: self.camera.clone(),
            source_role: self.source_role,
            sequence,
            discontinuity,
            format_change,
            segment_sequence,
            data: data.into(),
        }
    }
}

/// Concatenated parameter-set NAL bytes when — and only when — the unit
/// carries the COMPLETE codec configuration (H.264: SPS and PPS; H.265:
/// VPS, SPS, and PPS). An isolated parameter-set NAL is NOT codec
/// configuration: starting a segment (or seeding a hardware decoder) on a
/// partial set would hand the backend broken config, so partial sets
/// return None — the pre-seam segment-boundary rule.
fn extract_parameter_sets(codec: VideoCodec, unit: &[u8]) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    match codec {
        VideoCodec::H264 => {
            let mut has_sps = false;
            let mut has_pps = false;
            for nal in openh264::nal_units(unit) {
                if let Some(header) = annex_b_nal_header(nal) {
                    match header & 0x1f {
                        7 => {
                            has_sps = true;
                            bytes.extend_from_slice(nal);
                        }
                        8 => {
                            has_pps = true;
                            bytes.extend_from_slice(nal);
                        }
                        _ => {}
                    }
                }
            }
            if !(has_sps && has_pps) {
                return None;
            }
        }
        VideoCodec::H265 => {
            use rust_h265::NalUnitType;
            let mut has_vps = false;
            let mut has_sps = false;
            let mut has_pps = false;
            for nal in rust_h265::parse_annex_b(unit) {
                match nal.nal_unit_type {
                    NalUnitType::Vps => has_vps = true,
                    NalUnitType::Sps => has_sps = true,
                    NalUnitType::Pps => has_pps = true,
                    _ => continue,
                }
                bytes.extend_from_slice(&[0, 0, 0, 1]);
                // rbsp: the parameter-set payload (EPB-stripped) —
                // deterministic identity for format-change detection.
                bytes.extend_from_slice(&nal.rbsp);
            }
            if !(has_vps && has_sps && has_pps) {
                return None;
            }
        }
    }
    if bytes.is_empty() { None } else { Some(bytes) }
}

/// Whether the unit carries (part of) a random-access picture.
fn unit_carries_keyframe(codec: VideoCodec, unit: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 => openh264::nal_units(unit)
            .filter_map(annex_b_nal_header)
            .any(|header| header & 0x1f == 5),
        VideoCodec::H265 => {
            use rust_h265::NalUnitType;
            rust_h265::parse_annex_b(unit).into_iter().any(|nal| {
                matches!(
                    nal.nal_unit_type,
                    NalUnitType::BlaWLp
                        | NalUnitType::BlaWRadl
                        | NalUnitType::BlaNLp
                        | NalUnitType::IdrWRadl
                        | NalUnitType::IdrNLp
                        | NalUnitType::Cra
                )
            })
        }
    }
}

/// The one-byte NAL header after an annex-B start code (or the first byte
/// when the slice is already header-first).
fn annex_b_nal_header(nal: &[u8]) -> Option<u8> {
    let mut zeros = 0usize;
    for (index, byte) in nal.iter().copied().enumerate() {
        match byte {
            0 => zeros += 1,
            1 if zeros >= 2 => return nal.get(index + 1).copied(),
            _ => zeros = 0,
        }
    }
    nal.first().copied()
}

/// Explicit classification of the selected decoder path. `Unclassified`
/// exists so an unknown selection can never masquerade as hardware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendClassification {
    Hardware {
        element: String,
        device: Option<String>,
    },
    Software {
        element: String,
    },
    Unclassified {
        element: String,
    },
}

impl BackendClassification {
    /// Only an explicit Hardware classification counts as hardware.
    pub fn is_hardware(&self) -> bool {
        matches!(self, BackendClassification::Hardware { .. })
    }
}

/// Outcome of a real startup decode probe on the stream/codec path.
#[derive(Debug, Clone)]
pub enum ProbeOutcome {
    /// Decoded at least one frame; carries the selected classification.
    Decoded {
        classification: BackendClassification,
        frames_decoded: u64,
    },
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeBackendError {
    /// The unit belongs to a different stream or epoch than this backend
    /// instance was opened for.
    EpochViolation {
        expected: u64,
        got: u64,
    },
    StreamViolation,
    /// The unit carries a different codec than this backend was opened for.
    CodecViolation,
    Decode(String),
}

/// A swappable decoder backend at the encoded-access-unit boundary.
/// One instance per stream per epoch, OWNED by that stream's ingress
/// thread: decoder state is stream-local and never crosses threads, so the
/// trait is deliberately not `Send`.
pub trait DecodeBackend {
    /// Stable backend identifier for receipts (e.g. software decoder id).
    fn id(&self) -> &'static str;

    /// Run a real decode probe with sample access units; hardware backends
    /// must report their selected-decoder classification.
    fn probe(&mut self, sample: &[EncodedAccessUnit]) -> ProbeOutcome;

    /// Decode one access unit into zero or more RGB frames.
    fn decode(
        &mut self,
        unit: &EncodedAccessUnit,
    ) -> Result<Vec<DecodedRgbFrame>, DecodeBackendError>;

    /// The classification of the currently selected decode path.
    fn classification(&self) -> BackendClassification;
}

/// The pure-software backend: the existing OpenH264 / H.265 decode path
/// behind the seam. Always classified Software; the honest fallback.
pub struct SoftwareDecodeBackend {
    stream_id: StreamId,
    stream_epoch: u64,
    codec: VideoCodec,
    decoder: crate::media_pipeline::StreamingDecoder,
}

impl SoftwareDecodeBackend {
    pub fn new(stream_id: StreamId, codec: VideoCodec, stream_epoch: u64) -> Result<Self, String> {
        Ok(Self {
            stream_id,
            stream_epoch,
            codec,
            decoder: crate::media_pipeline::StreamingDecoder::new(codec)?,
        })
    }

    fn check_unit(&self, unit: &EncodedAccessUnit) -> Result<(), DecodeBackendError> {
        if unit.stream_id != self.stream_id {
            return Err(DecodeBackendError::StreamViolation);
        }
        if unit.stream_epoch != self.stream_epoch {
            return Err(DecodeBackendError::EpochViolation {
                expected: self.stream_epoch,
                got: unit.stream_epoch,
            });
        }
        if unit.codec != self.codec {
            return Err(DecodeBackendError::CodecViolation);
        }
        Ok(())
    }
}

impl DecodeBackend for SoftwareDecodeBackend {
    fn id(&self) -> &'static str {
        "software"
    }

    fn probe(&mut self, sample: &[EncodedAccessUnit]) -> ProbeOutcome {
        let mut frames_decoded = 0u64;
        for unit in sample {
            match self.decode(unit) {
                Ok(frames) => frames_decoded += frames.len() as u64,
                Err(error) => {
                    return ProbeOutcome::Failed {
                        reason: format!("software decode probe failed: {error:?}"),
                    };
                }
            }
        }
        if frames_decoded == 0 {
            return ProbeOutcome::Failed {
                reason: "software decode probe produced no frames".to_string(),
            };
        }
        ProbeOutcome::Decoded {
            classification: self.classification(),
            frames_decoded,
        }
    }

    fn decode(
        &mut self,
        unit: &EncodedAccessUnit,
    ) -> Result<Vec<DecodedRgbFrame>, DecodeBackendError> {
        self.check_unit(unit)?;
        self.decoder
            .decode_unit(&unit.data)
            .map_err(DecodeBackendError::Decode)
    }

    fn classification(&self) -> BackendClassification {
        BackendClassification::Software {
            element: "software".to_string(),
        }
    }
}

/// How the runtime picked the decode path for one stream, with the receipt
/// describing why. Selection never trusts configuration alone: hardware is
/// selected only off a passed probe with a hardware classification.
pub struct DecoderSelection {
    pub backend: Box<dyn DecodeBackend>,
    pub receipt: crate::acceleration::AccelerationReceipt,
}

/// Select the decode backend for a stream honoring `hardware_decoding`
/// intent: false skips hardware probes entirely; true probes and falls back
/// visibly on any failure or non-hardware classification.
pub fn select_decode_backend(
    stream_id: &StreamId,
    codec: VideoCodec,
    stream_epoch: u64,
    hardware_decoding: bool,
    probe_sample: &[EncodedAccessUnit],
) -> Result<DecoderSelection, String> {
    use crate::acceleration::{
        AccelStage, AccelerationReceipt, ActionKind, FailureCode, ProbeStatus,
    };
    use std::collections::BTreeMap;

    let codec_label = match codec {
        VideoCodec::H264 => "H264",
        VideoCodec::H265 => "H265",
    };
    let base_receipt = |attempted: &str| AccelerationReceipt {
        stage: AccelStage::Decode,
        work_id: None,
        parent_work_id: None,
        stream_id: Some(stream_id.clone()),
        media_item: None,
        configured: hardware_decoding,
        attempted_backend: attempted.to_string(),
        active_backend: "software".to_string(),
        hardware_accelerated: false,
        selected_device: None,
        codec: Some(codec_label.to_string()),
        model_id: None,
        model_version: None,
        input_shape: None,
        probe_status: ProbeStatus::Fallback,
        failure_code: FailureCode::None,
        evidence_kind: None,
        evidence_fields: BTreeMap::new(),
        action_kind: ActionKind::NoAction,
        action_payload: None,
    };

    if !hardware_decoding {
        // Intent says software: no hardware probe is attempted at all.
        let backend = SoftwareDecodeBackend::new(stream_id.clone(), codec, stream_epoch)?;
        let mut receipt = base_receipt("software");
        receipt.probe_status = ProbeStatus::Disabled;
        return Ok(DecoderSelection {
            backend: Box::new(backend),
            receipt,
        });
    }

    #[cfg(feature = "decode-gstreamer")]
    {
        // Device access FIRST: probing GStreamer without device access would
        // misclassify a permission problem as a missing plugin (the va
        // elements silently fail to register). The runtime's receipt must
        // tell the same classified truth doctor does.
        if let Some(blocked) = crate::doctor::live_device_access_finding() {
            let backend = SoftwareDecodeBackend::new(stream_id.clone(), codec, stream_epoch)?;
            let mut receipt = base_receipt("gstreamer");
            receipt.probe_status = ProbeStatus::Fallback;
            receipt.failure_code = blocked.failure_code;
            receipt.evidence_kind = Some(blocked.evidence_kind);
            receipt.evidence_fields = blocked.evidence_fields;
            receipt.action_kind = blocked.action_kind;
            receipt.action_payload = blocked.action_payload;
            return Ok(DecoderSelection {
                backend: Box::new(backend),
                receipt,
            });
        }
        match crate::decode_gstreamer::GstreamerDecodeBackend::probe_and_build(
            stream_id.clone(),
            codec,
            stream_epoch,
            probe_sample,
        ) {
            Ok(mut selection) => {
                if selection.receipt.selected_device.is_none() {
                    // Element metadata did not expose the device path; the
                    // runtime knows which device it can open — say that one.
                    selection.receipt.selected_device = crate::doctor::first_usable_render_device()
                        .map(|device| device.display().to_string());
                }
                Ok(selection)
            }
            Err(fallback) => {
                let backend = SoftwareDecodeBackend::new(stream_id.clone(), codec, stream_epoch)?;
                let mut receipt = base_receipt("gstreamer");
                receipt.probe_status = ProbeStatus::Fallback;
                receipt.failure_code = fallback.failure_code;
                receipt.evidence_kind = Some(fallback.evidence_kind);
                receipt.evidence_fields = fallback.evidence_fields;
                // The probe consumed real stream units even though it failed.
                receipt.evidence_fields.insert(
                    "probe_units_consumed".to_string(),
                    probe_sample.len().to_string(),
                );
                receipt.action_kind = fallback.action_kind;
                receipt.action_payload = fallback.action_payload;
                Ok(DecoderSelection {
                    backend: Box::new(backend),
                    receipt,
                })
            }
        }
    }

    #[cfg(not(feature = "decode-gstreamer"))]
    {
        let _ = probe_sample;
        // This artifact ships no hardware decode backend: honest fallback.
        let backend = SoftwareDecodeBackend::new(stream_id.clone(), codec, stream_epoch)?;
        let mut receipt = base_receipt("none");
        receipt.probe_status = ProbeStatus::Fallback;
        receipt.failure_code = FailureCode::UnsupportedByThisArtifact;
        receipt.evidence_kind = Some(crate::acceleration::EvidenceKind::SelectedBackend);
        receipt.evidence_fields.insert(
            "compiled_decode_backends".to_string(),
            "software".to_string(),
        );
        receipt.action_kind = ActionKind::InstallSupportedArtifact;
        receipt.action_payload =
            Some("install a hardware-enabled artifact, or keep software decode".to_string());
        Ok(DecoderSelection {
            backend: Box::new(backend),
            receipt,
        })
    }
}

/// Receipt for a mid-stream hardware decode failure: the visible reason the
/// runtime switched a live stream to the software path.
pub fn mid_stream_fallback_receipt(
    stream_id: &StreamId,
    codec: VideoCodec,
    error: &str,
) -> crate::acceleration::AccelerationReceipt {
    use crate::acceleration::{
        AccelStage, AccelerationReceipt, ActionKind, EvidenceKind, FailureCode, ProbeStatus,
    };
    AccelerationReceipt {
        stage: AccelStage::Decode,
        work_id: None,
        parent_work_id: None,
        stream_id: Some(stream_id.clone()),
        media_item: None,
        configured: true,
        attempted_backend: "gstreamer".to_string(),
        active_backend: "software".to_string(),
        hardware_accelerated: false,
        selected_device: None,
        codec: Some(
            match codec {
                VideoCodec::H264 => "H264",
                VideoCodec::H265 => "H265",
            }
            .to_string(),
        ),
        model_id: None,
        model_version: None,
        input_shape: None,
        probe_status: ProbeStatus::Fallback,
        failure_code: FailureCode::ProbeFailed,
        evidence_kind: Some(EvidenceKind::UpstreamError),
        evidence_fields: std::collections::BTreeMap::from([(
            "error".to_string(),
            error.to_string(),
        )]),
        action_kind: ActionKind::ManualActionRequired,
        action_payload: Some(
            "hardware decode failed mid-stream; software decode is active".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h264_unit(nal_types: &[u8]) -> Vec<u8> {
        let mut unit = Vec::new();
        for nal_type in nal_types {
            unit.extend_from_slice(&[0, 0, 0, 1, nal_type & 0x1f | 0x60, 0xAC, 0x2B, 0x40]);
        }
        unit
    }

    fn h265_unit(nal_types: &[u8]) -> Vec<u8> {
        let mut unit = Vec::new();
        for nal_type in nal_types {
            unit.extend_from_slice(&[0, 0, 0, 1, nal_type << 1, 0x01, 0x0C, 0x01, 0xFF]);
        }
        unit
    }

    #[test]
    fn split_h264_parameter_sets_are_not_codec_config() {
        // An isolated SPS (or PPS) is NOT complete codec configuration: a
        // segment must not start on it and a hardware decoder must not be
        // seeded with it.
        assert!(extract_parameter_sets(VideoCodec::H264, &h264_unit(&[7])).is_none());
        assert!(extract_parameter_sets(VideoCodec::H264, &h264_unit(&[8])).is_none());
        assert!(
            extract_parameter_sets(VideoCodec::H264, &h264_unit(&[7, 8])).is_some(),
            "SPS and PPS together are complete codec configuration"
        );
    }

    #[test]
    fn split_h265_parameter_sets_are_not_codec_config() {
        assert!(extract_parameter_sets(VideoCodec::H265, &h265_unit(&[32])).is_none());
        assert!(extract_parameter_sets(VideoCodec::H265, &h265_unit(&[33])).is_none());
        assert!(extract_parameter_sets(VideoCodec::H265, &h265_unit(&[32, 33])).is_none());
        assert!(
            extract_parameter_sets(VideoCodec::H265, &h265_unit(&[32, 33, 34])).is_some(),
            "VPS+SPS+PPS together are complete codec configuration"
        );
    }
}
