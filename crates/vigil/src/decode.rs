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
}

impl AccessUnitAssembler {
    pub fn new(stream_id: StreamId, codec: VideoCodec, stream_epoch: u64) -> Self {
        Self {
            stream_id,
            codec,
            stream_epoch,
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
        let _ = (data, media_timestamp, discontinuity, segment_sequence);
        let _ = (&self.stream_id, self.codec, self.stream_epoch);
        unimplemented!("scaffold: access-unit assembly is not implemented yet")
    }
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
    Decode(String),
}

/// A swappable decoder backend at the encoded-access-unit boundary.
/// One instance per stream per epoch.
pub trait DecodeBackend: Send {
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
}

impl SoftwareDecodeBackend {
    pub fn new(stream_id: StreamId, codec: VideoCodec, stream_epoch: u64) -> Result<Self, String> {
        let _ = (&stream_id, codec, stream_epoch);
        unimplemented!("scaffold: software decode backend construction is not implemented yet")
    }
}

impl DecodeBackend for SoftwareDecodeBackend {
    fn id(&self) -> &'static str {
        "software"
    }

    fn probe(&mut self, sample: &[EncodedAccessUnit]) -> ProbeOutcome {
        let _ = sample;
        unimplemented!("scaffold: software decode probe is not implemented yet")
    }

    fn decode(
        &mut self,
        unit: &EncodedAccessUnit,
    ) -> Result<Vec<DecodedRgbFrame>, DecodeBackendError> {
        let _ = unit;
        let _ = (&self.stream_id, self.stream_epoch, self.codec);
        unimplemented!("scaffold: software decode is not implemented yet")
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
    let _ = (
        stream_id,
        codec,
        stream_epoch,
        hardware_decoding,
        probe_sample,
    );
    unimplemented!("scaffold: decode backend selection is not implemented yet")
}
