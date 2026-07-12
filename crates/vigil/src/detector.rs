//! The engine-neutral detection seam.
//!
//! The trait signature names no inference engine — the engine lives BELOW the
//! trait, in the implementation, so a model or the whole engine can be swapped
//! without touching the pipeline. Moving a box from CPU to an accelerator is a
//! different implementation (or a backend selection inside one), never a
//! pipeline rework. The vision embedder follows the same pattern through the
//! memory library's embedder trait.

use std::sync::RwLock;

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

/// A detector handle whose concrete backend can be swapped LIVE, under a
/// write lock, without touching the worker loop: the camera workers keep
/// calling `detect_segment` through this handle while a late-completing
/// acceleration probe promotes the inner detector from CPU to the accelerated
/// backend (C12 live promotion). The swap is atomic to any in-flight segment —
/// a `detect_segment` reader holds the read lock for its whole call, so it
/// runs entirely on the old or the new detector, never a torn mix.
pub(crate) struct PromotableDetector {
    detector: RwLock<Box<dyn Detector>>,
    /// The initially-loaded detector's model hash. The `model_sha256` trait
    /// method is only read at load-time logging on the raw detector before it
    /// is wrapped here; per-detection provenance uses
    /// `DetectorOutput.model_sha256`, so this handle's method is never on the
    /// per-segment hot path and reporting the initial hash is honest.
    initial_model_sha256: String,
}

impl PromotableDetector {
    pub(crate) fn new(detector: Box<dyn Detector>) -> Self {
        let initial_model_sha256 = detector.model_sha256().to_string();
        Self {
            detector: RwLock::new(detector),
            initial_model_sha256,
        }
    }

    /// Swap the inner detector under the write lock. Called from the late
    /// promotion path on a valid late PASS; a poisoned lock is recovered
    /// (the guarded value is a plain `Box`, not left in a broken state).
    #[cfg(feature = "detect-burn-wgpu")]
    pub(crate) fn promote(&self, detector: Box<dyn Detector>) {
        let mut guard = self
            .detector
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = detector;
    }
}

impl Detector for PromotableDetector {
    fn model_sha256(&self) -> &str {
        &self.initial_model_sha256
    }

    fn detect_segment(
        &self,
        media: &DecodedVideoSegment,
        clip_sha256: String,
        sample_frames: usize,
        confidence_threshold: f64,
    ) -> Result<DetectorOutput, String> {
        let guard = self
            .detector
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.detect_segment(media, clip_sha256, sample_frames, confidence_threshold)
    }
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
