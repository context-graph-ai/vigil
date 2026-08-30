//! A cold accelerator preparation at startup, and what the node says while it
//! runs.
//!
//! Detection starts on the processor and stays there until an accelerator has
//! actually proved itself, so startup never waits for a first shader compile.
//! The preparation then runs to its own conclusion: it ends by COMPLETING —
//! however long that took, and a completion promotes the running detector
//! without a restart — or by an error the preparation itself reports. Nothing
//! else ends it. A slow compile on hardware that can genuinely do the work is
//! not a failure and is never recorded as one.
//!
//! While it runs the node says so: the detection receipt reports a preparation
//! under way, when it began, and the last thing the preparation reported about
//! itself. That is what lets a person tell slow from wedged without the machine
//! guessing on their behalf.
//!
//! The preparation here is a stand-in that finishes exactly when this file
//! tells it to, so every state below is reached by an event and never by time
//! passing.

#![cfg(feature = "detect-burn-wgpu")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use vigil::acceleration::{
    AccelStage, AccelerationReceipt, AccelerationState, FailureCode, ProbeStatus,
};
use vigil::detection_accel::{
    ACCELERATED_DETECTION_BACKEND, CPU_DETECTION_BACKEND, DetectionForwardProbe,
    DetectionForwardProbeOutcome, spawn_detection_probe_with_promotion,
};

const MODEL_ID: &str = "detector-under-test";
const INPUT_SHAPE: &str = "1x3x640x640";

/// How many times a wait looks before it gives up and fails. Large enough that
/// a receipt a working implementation records is always seen, and finite so a
/// missing one is a failure rather than a hang.
const ATTEMPTS: usize = 2_000_000;

/// The evidence field naming when this preparation began.
const PREPARING_SINCE: &str = "preparing_since_ms";
/// The evidence field carrying the last thing the preparation reported.
const LATEST_PROGRESS: &str = "latest_progress";

/// A preparation that finishes when this file says so, and not before. It also
/// reports its own progress on the way, which is the only thing that changes
/// what the surface says while it runs.
struct DrivenPreparation {
    finish: mpsc::Receiver<DetectionForwardProbeOutcome>,
    accel: Arc<AccelerationState>,
    progress: Vec<&'static str>,
}

impl DetectionForwardProbe for DrivenPreparation {
    fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
        for note in std::mem::take(&mut self.progress) {
            self.accel.record_progress(AccelStage::Detection, note);
        }
        self.finish
            .recv()
            .unwrap_or(DetectionForwardProbeOutcome::NoDeviceVisible)
    }
}

fn passing_outcome() -> DetectionForwardProbeOutcome {
    DetectionForwardProbeOutcome::Passed {
        selected_device: "test-accelerator".to_string(),
        evidence_fields: BTreeMap::new(),
    }
}

/// Waits for a detection receipt matching `ready`. The bound is a count of
/// attempts, not a length of time: it exists so a receipt that never arrives
/// fails loudly instead of hanging the run, and reading a clock to decide when
/// to give up would put the one thing this file forbids into the file itself.
fn detection_receipt(
    accel: &AccelerationState,
    ready: impl Fn(&AccelerationReceipt) -> bool,
) -> Option<AccelerationReceipt> {
    for _ in 0..ATTEMPTS {
        if let Some(receipt) = accel
            .snapshot()
            .into_iter()
            .find(|receipt| receipt.stage == AccelStage::Detection && ready(receipt))
        {
            return Some(receipt);
        }
        std::thread::yield_now();
    }
    None
}

#[test]
fn a_preparation_under_way_is_reported_as_such_and_promotes_when_it_completes() {
    let accel = Arc::new(AccelerationState::new());
    let promoted = Arc::new(AtomicBool::new(false));
    let (finish, wait_for_finish) = mpsc::channel();

    let promote_flag = Arc::clone(&promoted);
    let selection = spawn_detection_probe_with_promotion(
        true,
        MODEL_ID,
        INPUT_SHAPE,
        DrivenPreparation {
            finish: wait_for_finish,
            accel: Arc::clone(&accel),
            progress: vec!["compiling shaders"],
        },
        Arc::clone(&accel),
        move || {
            promote_flag.store(true, Ordering::SeqCst);
            Ok(())
        },
        |_receipt: &AccelerationReceipt| {},
    );

    // Startup got its answer while the preparation is demonstrably unfinished —
    // this file has not released it yet — so the answer cannot have waited for
    // it.
    assert_eq!(
        selection.backend, CPU_DETECTION_BACKEND,
        "detection starts on the processor rather than waiting for an accelerator"
    );
    assert_eq!(
        selection.receipt.probe_status,
        ProbeStatus::Preparing,
        "a preparation still running is reported as under way, never as a fallback the machine \
         settled for"
    );
    assert_eq!(
        selection.receipt.failure_code,
        FailureCode::None,
        "nothing has failed: a preparation that is still working is not a failure, and \
         classifying it as one is what told an operator their usable hardware could not do the \
         work"
    );

    let preparing = detection_receipt(&accel, |receipt| {
        receipt.probe_status == ProbeStatus::Preparing
    })
    .expect("the preparation under way must be on the surface an operator reads");
    assert!(
        preparing
            .evidence_fields
            .get(PREPARING_SINCE)
            .is_some_and(|since| !since.trim().is_empty()),
        "the surface must say when this preparation began, so waiting is distinguishable from \
         wedged: {:?}",
        preparing.evidence_fields
    );
    let progressed = detection_receipt(&accel, |receipt| {
        receipt.evidence_fields.contains_key(LATEST_PROGRESS)
    })
    .expect("the preparation reported progress, so the surface must carry it");
    assert_eq!(
        progressed
            .evidence_fields
            .get(LATEST_PROGRESS)
            .map(String::as_str),
        Some("compiling shaders"),
        "the surface must carry the last thing the preparation said about itself"
    );
    assert!(
        !promoted.load(Ordering::SeqCst),
        "nothing is promoted while the preparation is still running"
    );

    // The one event that ends it.
    finish
        .send(passing_outcome())
        .expect("release the preparation");

    let active = detection_receipt(&accel, |receipt| {
        receipt.probe_status == ProbeStatus::Active
    })
    .expect("a completed preparation records its real result, however long it took");
    assert!(
        promoted.load(Ordering::SeqCst),
        "a completed preparation promotes the running detector without a restart"
    );
    assert!(active.hardware_accelerated);
    assert_eq!(active.active_backend, ACCELERATED_DETECTION_BACKEND);

    let action = active
        .action_payload
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        action.contains("promot") && (action.contains("no action") || action.contains("active")),
        "the action must say detection was promoted and is now active: {action}"
    );
    for stale in ["next start", "next boot", "at least", "raise"] {
        assert!(
            !action.contains(stale),
            "a promoted action must not send the operator to restart or to turn a knob \
             (`{stale}`): {action}"
        );
    }
}
