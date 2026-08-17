//! The engine-neutral detection seam.
//!
//! The trait signature names no inference engine — the engine lives BELOW the
//! trait, in the implementation, so a model or the whole engine can be swapped
//! without touching the pipeline. Moving a box from CPU to an accelerator is a
//! different implementation (or a backend selection inside one), never a
//! pipeline rework. The vision embedder follows the same pattern through the
//! memory library's embedder trait.

use std::sync::{Arc, RwLock};

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

/// A detector handle whose concrete backend can be swapped LIVE, without
/// touching the worker loop: the camera workers keep calling `detect_segment`
/// through this handle while a background preparation replaces the inner
/// detector.
///
/// A worker takes a cheap pointer copy of the active detector under a SHORT
/// lock and then runs its whole inference through that copy, so a replacement
/// owes nothing to how long any detection takes — which is what keeps an
/// operator's command off the back of a slow inference. An in-flight segment
/// finishes on the detector it started with; the next one uses the detector
/// that replaced it, never a torn mix. A swap hands back the instance it
/// replaced, so putting it back later is another pointer swap rather than
/// another model load.
pub(crate) struct PromotableDetector {
    detector: RwLock<Arc<dyn Detector>>,
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
            detector: RwLock::new(Arc::from(detector)),
            initial_model_sha256,
        }
    }

    /// The detector to run this segment through: a pointer copy taken under a
    /// short lock and used after the lock is released, so inference never holds
    /// the lock a replacement needs.
    pub(crate) fn snapshot(&self) -> Arc<dyn Detector> {
        let guard = self
            .detector
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(&guard)
    }

    /// Put a newly built detector in place and hand back the one it replaced,
    /// which stays loaded as the prepared way back. A poisoned lock is
    /// recovered (the guarded value is a plain pointer, not left in a broken
    /// state).
    pub(crate) fn swap(&self, detector: Box<dyn Detector>) -> Arc<dyn Detector> {
        self.swap_to(Arc::from(detector))
    }

    /// Put an already-loaded detector back in place, handing back the one it
    /// replaced. This is the whole of a reverse move: no model is loaded.
    pub(crate) fn swap_to(&self, detector: Arc<dyn Detector>) -> Arc<dyn Detector> {
        let mut guard = self
            .detector
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::replace(&mut guard, detector)
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
        let active = self.snapshot();
        active.detect_segment(media, clip_sha256, sample_frames, confidence_threshold)
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

// ── Detector ownership contract ────────────────────────────────────────────
//
// This module is the frozen statement of how a running detector is held and
// replaced. It lives here because the handle and the trait are both crate-
// private, which the crate already answers with an inline test module wherever
// a seam is not reachable from a file under `tests/` (clock.rs, settings.rs,
// decode.rs, live_backends.rs and others all do this). Nothing below is
// reachable from a shipped artifact: the whole module is compiled out.
#[cfg(test)]
mod ownership_contract {
    use super::*;
    use std::sync::Arc;

    /// A detector whose only observable behavior is the model hash it reports,
    /// which is how these contracts tell one instance from another. Detection
    /// is never run here — only ownership is under test — so it refuses
    /// loudly if that ever changes.
    struct NamedDetector(&'static str);

    impl Detector for NamedDetector {
        fn model_sha256(&self) -> &str {
            self.0
        }

        fn detect_segment(
            &self,
            _media: &DecodedVideoSegment,
            _clip_sha256: String,
            _sample_frames: usize,
            _confidence_threshold: f64,
        ) -> Result<DetectorOutput, String> {
            unreachable!("the ownership contracts never decode a segment")
        }
    }

    #[test]
    fn a_swap_does_not_wait_for_the_detector_a_worker_is_already_using() {
        // A worker copies the shared pointer to the active detector and runs
        // its whole detection through that copy. So a replacement is a pointer
        // swap that owes nothing to how long any detection takes — which is
        // what keeps an operator's command off the back of a slow inference.
        //
        // The snapshot held across the swap below IS a detection in progress:
        // an ownership shape that made a replacement wait for the reader could
        // not complete this function at all.
        let handle = PromotableDetector::new(Box::new(NamedDetector("first")));

        let in_flight = handle.snapshot();
        assert_eq!(in_flight.model_sha256(), "first");

        handle.swap(Box::new(NamedDetector("second")));

        assert_eq!(
            in_flight.model_sha256(),
            "first",
            "a detection already under way finishes on the detector it started with"
        );
        assert_eq!(
            handle.snapshot().model_sha256(),
            "second",
            "the next detection uses the detector that replaced it"
        );
    }

    #[test]
    fn a_swap_hands_back_the_detector_it_replaced() {
        // The detector stepped away from stays loaded and is handed back, so
        // moving back onto it is another pointer swap rather than another cold
        // model load on the operator's own command.
        let handle = PromotableDetector::new(Box::new(NamedDetector("processor")));

        let replaced = handle.swap(Box::new(NamedDetector("accelerated")));
        assert_eq!(
            replaced.model_sha256(),
            "processor",
            "the swap must return the instance it replaced, ready to be put back"
        );

        let back = handle.swap_to(Arc::clone(&replaced));
        assert_eq!(
            handle.snapshot().model_sha256(),
            "processor",
            "putting the retained instance back must take effect with no model load"
        );
        assert_eq!(
            back.model_sha256(),
            "accelerated",
            "and the instance stepped away from is itself retained"
        );
    }

    #[test]
    fn every_snapshot_of_an_unchanged_handle_is_the_same_instance() {
        // Taking a snapshot is a pointer copy, not a clone of the detector: a
        // shape that duplicated the detector per detection would multiply the
        // model in memory once per segment.
        let handle = PromotableDetector::new(Box::new(NamedDetector("only")));
        assert!(
            Arc::ptr_eq(&handle.snapshot(), &handle.snapshot()),
            "two snapshots taken with no swap between them must be one instance"
        );
    }
}
