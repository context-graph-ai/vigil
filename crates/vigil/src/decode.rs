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

/// One encoded access unit with everything a robust hardware decoder needs.
/// Raw bytes alone are not enough for the hardware decode seam.
#[derive(Debug, Clone)]
pub struct EncodedAccessUnit {
    pub stream_id: StreamId,
    /// Stream epoch: bumped on reconnect/session restart. A decoder instance
    /// is valid for exactly one epoch.
    pub stream_epoch: u64,
    pub codec: VideoCodec,
    /// Parameter-set bytes (SPS/PPS/VPS or equivalent) when this unit
    /// carries codec configuration; `None` otherwise.
    pub codec_config: Option<Vec<u8>>,
    /// This unit carries (part of) a random-access/keyframe picture.
    pub keyframe: bool,
    pub media_timestamp: Option<DateTime<Utc>>,
    /// Monotonic per-epoch sequence number.
    pub sequence: u64,
    /// Set on the first unit after a reconnect or timeline break.
    pub discontinuity: bool,
    /// Set when the carried parameter sets differ from the previous ones
    /// (resolution/format change boundary).
    pub format_change: bool,
    /// The segment this unit belongs to (segment assembly identity).
    pub segment_sequence: u64,
    pub data: Vec<u8>,
}

/// Derives the access-unit contract fields from raw depacketized units.
/// Pure: same bytes in, same flags out.
pub struct AccessUnitAssembler {
    stream_id: StreamId,
    codec: VideoCodec,
    stream_epoch: u64,
    next_sequence: u64,
    previous_parameter_sets: Option<Vec<u8>>,
}

impl AccessUnitAssembler {
    pub fn new(stream_id: StreamId, codec: VideoCodec, stream_epoch: u64) -> Self {
        Self {
            stream_id,
            codec,
            stream_epoch,
            next_sequence: 0,
            previous_parameter_sets: None,
        }
    }

    /// Wrap the next raw unit, deriving parameter-set presence, keyframe
    /// flag, format-change marker, sequence, and segment membership.
    pub fn assemble(
        &mut self,
        data: Vec<u8>,
        media_timestamp: Option<DateTime<Utc>>,
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
            codec_config,
            media_timestamp,
            sequence,
            discontinuity,
            format_change,
            segment_sequence,
            data,
        }
    }
}

/// Concatenated parameter-set NAL bytes (SPS/PPS for H.264, VPS/SPS/PPS for
/// H.265) when the unit carries codec configuration.
fn extract_parameter_sets(codec: VideoCodec, unit: &[u8]) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    match codec {
        VideoCodec::H264 => {
            for nal in openh264::nal_units(unit) {
                if let Some(header) = annex_b_nal_header(nal)
                    && matches!(header & 0x1f, 7 | 8)
                {
                    bytes.extend_from_slice(nal);
                }
            }
        }
        VideoCodec::H265 => {
            use rust_h265::NalUnitType;
            for nal in rust_h265::parse_annex_b(unit) {
                if matches!(
                    nal.nal_unit_type,
                    NalUnitType::Vps | NalUnitType::Sps | NalUnitType::Pps
                ) {
                    bytes.extend_from_slice(&[0, 0, 0, 1]);
                    // rbsp: the parameter-set payload (EPB-stripped) —
                    // deterministic identity for format-change detection.
                    bytes.extend_from_slice(&nal.rbsp);
                }
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
            Ok(selection) => Ok(selection),
            Err(fallback) => {
                let backend = SoftwareDecodeBackend::new(stream_id.clone(), codec, stream_epoch)?;
                let mut receipt = base_receipt("gstreamer");
                receipt.probe_status = ProbeStatus::Fallback;
                receipt.failure_code = fallback.failure_code;
                receipt.evidence_kind = Some(fallback.evidence_kind);
                receipt.evidence_fields = fallback.evidence_fields;
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
