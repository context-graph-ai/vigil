//! The engine-neutral detection seam.
//!
//! The trait signature names no inference engine — the engine lives BELOW the
//! trait, in the implementation, so a model or the whole engine can be swapped
//! without touching the pipeline. Moving a box from CPU to an accelerator is a
//! different implementation (or a backend selection inside one), never a
//! pipeline rework. The vision embedder follows the same pattern through the
//! memory library's embedder trait.

use burn::tensor::backend::Backend;

use crate::media_pipeline::DecodedVideoSegment;
use crate::yolox_detector::DetectorOutput;

pub(crate) trait Detector: Send + Sync {
    /// Content hash of the loaded model artifact (provenance for the
    /// detector-config decision).
    fn model_sha256(&self) -> &str;

    /// Run detection over the sampled frames of a decoded segment.
    fn detect_segment(
        &self,
        media: &DecodedVideoSegment,
        clip_sha256: String,
        sample_frames: usize,
        confidence_threshold: f64,
    ) -> Result<DetectorOutput, String>;
}

// Generic over every backend `YoloxDetector` can be constructed with: the
// concrete backend is picked below the trait, at detector construction, from
// the same acceleration selection that produced the receipt — this impl
// itself names no engine.
impl<B: Backend> Detector for crate::yolox_detector::YoloxDetector<B> {
    fn model_sha256(&self) -> &str {
        &self.model_sha256
    }

    fn detect_segment(
        &self,
        media: &DecodedVideoSegment,
        clip_sha256: String,
        sample_frames: usize,
        confidence_threshold: f64,
    ) -> Result<DetectorOutput, String> {
        crate::yolox_detector::detect_segment(
            self,
            media,
            clip_sha256,
            sample_frames,
            confidence_threshold,
        )
    }
}
