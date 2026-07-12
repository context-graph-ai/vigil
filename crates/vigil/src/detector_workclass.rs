//! Work class #2 (`vigil.detector`) payload schemas for the shared contextdb
//! work ledger. Job rows carry references only: frame bytes travel as a
//! content-addressed blob reference, never as job-row bytes, never through
//! the hub (criterion C1).
//!
//! This module is SKELETON ONLY — every function is `todo!()`/`unimplemented!()`
//! pending the ledger-wiring implementation. Types are versioned via
//! `schema_version` so the wire shape can evolve without breaking older
//! nodes mid-rollout.

use crate::VideoCodec;

/// Envelope/payload compatibility version for the detector work class.
/// Distinct from [`crate::workgraph::WORK_ENVELOPE_SCHEMA_VERSION`] — this
/// versions the FABRIC wire shape, not the in-process work graph.
pub const DETECTOR_SCHEMA_VERSION: u32 = 1;

/// The work-class tag registered on the shared ledger.
pub const DETECTOR_WORK_CLASS: &str = "vigil.detector";

/// The `JobSpec` mode this work class registers under.
pub const DETECTOR_MODE: &str = "object-detection";

/// The capability/requirement tag a claiming worker must advertise.
pub const DETECTOR_CLASS_TAG: &str = "class:vigil.detector";

/// Placeholder for the upstream contextdb content-addressed blob reference
/// (`InputRef::blob_ref` / the ledger's `BlobHash`-shaped identity).
///
/// FLAGGED FOR IMPLEMENTER: this is a vigil-LOCAL newtype only so this RED
/// test can assert the refs-only shape without pulling the contextdb-server
/// dependency into the test-authoring pass. Swap for the real upstream type
/// (do not keep both — the abstraction-placement rule forbids a parallel
/// vigil-side shadow of an upstream-owned concept).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlobRefPlaceholder(pub String);

/// Wire mirror of [`crate::workgraph::WorkEnvelope`]. The work-graph type
/// carries no serde derives (it is a process-local identity discipline);
/// the fabric wire form is a distinct, versioned, serializable shape so a
/// job row never smuggles process-local types across the ledger boundary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WireWorkEnvelope {
    pub work_id: String,
    pub parent_work_id: Option<String>,
    pub contributing_work_ids: Vec<String>,
    pub stage: String,
    pub stream_id: String,
    pub ordering_stream_epoch: u64,
    pub ordering_stream_sequence: u64,
    pub schema_version: u32,
}

/// Wire mirror of [`crate::workgraph::ResultEnvelope`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WireResultEnvelope {
    pub work_id: String,
    pub parent_work_id: Option<String>,
    pub contributing_work_ids: Vec<String>,
    pub stage: String,
    pub stream_id: String,
    pub result_schema_version: u32,
    pub receipt_id: String,
}

/// The `vigil.detector` job payload: refs-only, versioned. There is no field
/// on this type (and no constructor path — see [`DetectorJobBuilder`]) that
/// accepts raw frame bytes; the only way to name a segment's frames is
/// [`BlobRefPlaceholder`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DetectorJob {
    pub schema_version: u32,
    pub envelope: WireWorkEnvelope,
    pub codec: WireVideoCodec,
    pub fps: OrderedF64,
    pub sample_frames: usize,
    pub confidence_threshold: OrderedF64,
    pub clip_sha256: String,
    pub decoded_frames_sha256: String,
    pub model_id: String,
    pub frames_blob_ref: BlobRefPlaceholder,
}

/// Wire mirror of [`crate::VideoCodec`] (the runtime enum has no serde
/// derives).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WireVideoCodec {
    H264,
    H265,
}

impl From<VideoCodec> for WireVideoCodec {
    fn from(codec: VideoCodec) -> Self {
        match codec {
            VideoCodec::H264 => WireVideoCodec::H264,
            VideoCodec::H265 => WireVideoCodec::H265,
        }
    }
}

/// `f64` wrapper carrying `PartialEq`/`Eq` for wire-shape round-trip
/// assertions (bit-identical after a JSON hop, not float-tolerance
/// comparison — the schema is a byte contract, not a numeric one).
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct OrderedF64(pub f64);

impl PartialEq for OrderedF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for OrderedF64 {}

/// One detection inside a [`DetectorResult`] — field-for-field the same
/// shape as `yolox_detector::Detection`, mirrored here because that type is
/// process-local (`pub(crate)`) and this is the wire contract.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DetectorDetection {
    pub class_name: String,
    pub confidence: OrderedF64,
    pub bbox: String,
    pub frame_index: u64,
}

/// The `vigil.detector` result payload: full detector-attempt provenance so
/// class-map authority and NMS ownership stay vigil-side regardless of
/// which node ran the model (criterion C4).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DetectorResult {
    pub schema_version: u32,
    pub result_envelope: WireResultEnvelope,
    pub detections: Vec<DetectorDetection>,
    pub detector_backend: String,
    pub detector_session_id: String,
    pub model_sha256: String,
    pub model_forward_sha256: String,
    pub detector_nms_sha256: String,
    pub result_sha256: String,
    pub clip_sha256: String,
}

/// The `JobSpec`-relevant fields a builder must produce for registration on
/// the shared ledger (mirrors `contextdb`'s `JobSpec::builder` inputs
/// without depending on the upstream type from this test-authoring pass).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectorJobSpecFields {
    pub work_class: String,
    pub mode: String,
    pub requirement_tags: Vec<String>,
}

/// Builds a [`DetectorJob`]. The ONLY way to name a segment's frames is
/// `frames_blob_ref: BlobRefPlaceholder` — there is no method on this
/// builder (and no field on [`DetectorJob`]) that accepts `Vec<u8>` frame
/// bytes. A job whose provenance would embed raw frame bytes cannot be
/// constructed through this API.
pub struct DetectorJobBuilder {
    envelope: WireWorkEnvelope,
    frames_blob_ref: BlobRefPlaceholder,
    codec: Option<WireVideoCodec>,
    fps: Option<f64>,
    sample_frames: Option<usize>,
    confidence_threshold: Option<f64>,
    clip_sha256: Option<String>,
    decoded_frames_sha256: Option<String>,
    model_id: Option<String>,
}

impl DetectorJobBuilder {
    /// The two fields every detector job must carry from the start: its
    /// work identity, and a reference (never bytes) to the frames.
    pub fn new(envelope: WireWorkEnvelope, frames_blob_ref: BlobRefPlaceholder) -> Self {
        Self {
            envelope,
            frames_blob_ref,
            codec: None,
            fps: None,
            sample_frames: None,
            confidence_threshold: None,
            clip_sha256: None,
            decoded_frames_sha256: None,
            model_id: None,
        }
    }

    pub fn codec(mut self, codec: WireVideoCodec) -> Self {
        self.codec = Some(codec);
        self
    }

    pub fn fps(mut self, fps: f64) -> Self {
        self.fps = Some(fps);
        self
    }

    pub fn sample_frames(mut self, sample_frames: usize) -> Self {
        self.sample_frames = Some(sample_frames);
        self
    }

    pub fn confidence_threshold(mut self, threshold: f64) -> Self {
        self.confidence_threshold = Some(threshold);
        self
    }

    pub fn clip_sha256(mut self, sha: String) -> Self {
        self.clip_sha256 = Some(sha);
        self
    }

    pub fn decoded_frames_sha256(mut self, sha: String) -> Self {
        self.decoded_frames_sha256 = Some(sha);
        self
    }

    pub fn model_id(mut self, id: String) -> Self {
        self.model_id = Some(id);
        self
    }

    /// Assemble the job. Every optional field must be set through the
    /// builder before calling this — a missing field is a build-time bug
    /// in the caller, not a runtime possibility this API needs to encode.
    pub fn build(self) -> DetectorJob {
        DetectorJob {
            schema_version: DETECTOR_SCHEMA_VERSION,
            envelope: self.envelope,
            codec: self
                .codec
                .expect("DetectorJobBuilder::codec must be set before build()"),
            fps: OrderedF64(
                self.fps
                    .expect("DetectorJobBuilder::fps must be set before build()"),
            ),
            sample_frames: self
                .sample_frames
                .expect("DetectorJobBuilder::sample_frames must be set before build()"),
            confidence_threshold: OrderedF64(self.confidence_threshold.expect(
                "DetectorJobBuilder::confidence_threshold must be set before build()",
            )),
            clip_sha256: self
                .clip_sha256
                .expect("DetectorJobBuilder::clip_sha256 must be set before build()"),
            decoded_frames_sha256: self
                .decoded_frames_sha256
                .expect("DetectorJobBuilder::decoded_frames_sha256 must be set before build()"),
            model_id: self
                .model_id
                .expect("DetectorJobBuilder::model_id must be set before build()"),
            frames_blob_ref: self.frames_blob_ref,
        }
    }
}

/// The `JobSpec` fields a submitter would register this job under on the
/// shared ledger. `work_class`/`mode`/`requirement_tags` are this module's
/// fixed vocabulary — every `DetectorJob` registers identically regardless
/// of its own field values, so this deliberately ignores `job`'s contents.
pub fn detector_job_spec_fields(_job: &DetectorJob) -> DetectorJobSpecFields {
    DetectorJobSpecFields {
        work_class: DETECTOR_WORK_CLASS.to_string(),
        mode: DETECTOR_MODE.to_string(),
        requirement_tags: vec![DETECTOR_CLASS_TAG.to_string()],
    }
}

/// Encode a set of encoded (compressed) NAL units as one length-framed blob
/// suitable for content-addressed blob storage: each unit is prefixed with
/// its length as a little-endian `u32`.
pub fn encode_length_framed_units(units: &[Vec<u8>]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(units.iter().map(|unit| unit.len() + 4).sum());
    for unit in units {
        let len = u32::try_from(unit.len())
            .expect("a single encoded unit must fit in a u32 length prefix");
        framed.extend_from_slice(&len.to_le_bytes());
        framed.extend_from_slice(unit);
    }
    framed
}

/// Inverse of [`encode_length_framed_units`]. Rejects truncated input (a
/// header that runs past the end of the buffer, or a declared unit length
/// longer than the bytes remaining) with a typed `Err`, never a panic —
/// this decodes bytes that arrived over the wire from another node.
pub fn decode_length_framed_units(bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut units = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let remaining = bytes.len() - offset;
        if remaining < 4 {
            return Err(format!(
                "truncated length-framed unit header at offset {offset}: need 4 bytes, have {remaining}"
            ));
        }
        let mut len_bytes = [0u8; 4];
        len_bytes.copy_from_slice(&bytes[offset..offset + 4]);
        let len = u32::from_le_bytes(len_bytes) as usize;
        offset += 4;

        let remaining = bytes.len() - offset;
        if len > remaining {
            return Err(format!(
                "truncated or oversized length-framed unit body at offset {offset}: declared length {len}, have {remaining}"
            ));
        }
        units.push(bytes[offset..offset + len].to_vec());
        offset += len;
    }
    Ok(units)
}
