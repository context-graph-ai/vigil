//! Every way an accelerator preparation can end, and the surfaces that have to
//! agree about it.
//!
//! A preparation ends by completing or by an error it reports about itself.
//! Whichever happens, the same account reaches every surface an operator can
//! read — the health view and the statistics view — through one receipt path,
//! so no surface can keep naming a detector the workers are not running. A
//! completion promotes the running detector and says so. An error, or a
//! promotion that fails, leaves the processor detector standing and says that
//! instead; neither ever claims the accelerated backend.
//!
//! What must never happen is silence: a preparation that ends must say so.
//! What must equally never happen is a verdict nobody's machine produced — no
//! amount of time passing may end a preparation that is still working.
//!
//! The detector trait is crate-private, so the live swap is exercised through
//! the promote closure the runtime wires to its swappable detector handle, and
//! the all-surfaces propagation through the receipt sink the runtime wires to
//! its statistics surface. Every preparation here finishes exactly when this
//! file tells it to.

#![cfg(feature = "detect-burn-wgpu")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use vigil::acceleration::{
    AccelStage, AccelerationReceipt, AccelerationState, ActionKind, FailureCode, ProbeStatus,
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

/// A preparation that ends when this file sends it an outcome, and not before.
struct DrivenPreparation {
    finish: mpsc::Receiver<DetectionForwardProbeOutcome>,
}

impl DetectionForwardProbe for DrivenPreparation {
    fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
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

/// The statistics-surface sink the runtime wires to its stats state; here a
/// capture.
type ReceiptSink = Arc<Mutex<Option<AccelerationReceipt>>>;

fn new_sink() -> ReceiptSink {
    Arc::new(Mutex::new(None))
}

fn capture_into(sink: &ReceiptSink) -> impl Fn(&AccelerationReceipt) + Send + Sync + 'static {
    let sink = Arc::clone(sink);
    move |receipt: &AccelerationReceipt| {
        *sink.lock().expect("sink lock") = Some(receipt.clone());
    }
}

/// Both waits below are bounded by a count of attempts, not a length of time:
/// the bound exists so a receipt that never arrives fails loudly instead of
/// hanging the run, and reading a clock to decide when to give up would put the
/// one thing this file forbids into the file itself.
fn health_receipt(
    accel: &AccelerationState,
    ready: impl Fn(&AccelerationReceipt) -> bool,
) -> Option<AccelerationReceipt> {
    poll(|| {
        accel
            .snapshot()
            .into_iter()
            .find(|receipt| receipt.stage == AccelStage::Detection && ready(receipt))
    })
}

fn stats_receipt(
    sink: &ReceiptSink,
    ready: impl Fn(&AccelerationReceipt) -> bool,
) -> Option<AccelerationReceipt> {
    poll(|| sink.lock().expect("sink lock").clone().filter(&ready))
}

fn poll<T>(mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
    for _ in 0..ATTEMPTS {
        if let Some(value) = attempt() {
            return Some(value);
        }
        std::thread::yield_now();
    }
    None
}

#[test]
fn a_completed_preparation_promotes_and_the_same_account_reaches_every_surface() {
    let accel = Arc::new(AccelerationState::new());
    let promoted = Arc::new(AtomicBool::new(false));
    let stats_sink = new_sink();
    let (finish, wait_for_finish) = mpsc::channel();

    let promote_flag = Arc::clone(&promoted);
    let selection = spawn_detection_probe_with_promotion(
        true,
        MODEL_ID,
        INPUT_SHAPE,
        DrivenPreparation {
            finish: wait_for_finish,
        },
        Arc::clone(&accel),
        move || {
            promote_flag.store(true, Ordering::SeqCst);
            Ok(())
        },
        capture_into(&stats_sink),
    );

    assert_eq!(
        selection.receipt.probe_status,
        ProbeStatus::Preparing,
        "the answer comes back with the preparation still under way"
    );
    assert_eq!(selection.receipt.active_backend, CPU_DETECTION_BACKEND);

    finish
        .send(passing_outcome())
        .expect("release the preparation");

    let health = health_receipt(&accel, |receipt| {
        receipt.probe_status == ProbeStatus::Active
    })
    .expect("a completed preparation must record its result on the health surface");
    assert!(
        promoted.load(Ordering::SeqCst),
        "a completed preparation must promote the running detector"
    );
    let stats = stats_receipt(&stats_sink, |receipt| {
        receipt.probe_status == ProbeStatus::Active
    })
    .expect(
        "the same result must reach the statistics surface, which otherwise keeps naming the \
         processor detector after a real promotion",
    );

    for (surface, receipt) in [("health", &health), ("stats", &stats)] {
        assert!(
            receipt.hardware_accelerated,
            "{surface}: a promoted receipt is hardware-accelerated"
        );
        assert_eq!(
            receipt.active_backend, ACCELERATED_DETECTION_BACKEND,
            "{surface}: the receipt names the backend the workers now run"
        );
    }
    assert_eq!(
        health.active_backend, stats.active_backend,
        "the surfaces move together, never disagree"
    );
}

#[test]
fn a_promotion_that_fails_leaves_the_processor_standing_on_every_surface() {
    let accel = Arc::new(AccelerationState::new());
    let promote_called = Arc::new(AtomicBool::new(false));
    let stats_sink = new_sink();
    let (finish, wait_for_finish) = mpsc::channel();

    let called = Arc::clone(&promote_called);
    let selection = spawn_detection_probe_with_promotion(
        true,
        MODEL_ID,
        INPUT_SHAPE,
        // The preparation succeeds, but the live swap cannot load the
        // accelerated detector: the workers still run the processor, so no
        // surface may claim otherwise.
        DrivenPreparation {
            finish: wait_for_finish,
        },
        Arc::clone(&accel),
        move || {
            called.store(true, Ordering::SeqCst);
            Err("could not load accelerated detector for live promotion".to_string())
        },
        capture_into(&stats_sink),
    );
    assert_eq!(selection.receipt.active_backend, CPU_DETECTION_BACKEND);

    finish
        .send(passing_outcome())
        .expect("release the preparation");

    let names_promotion = |receipt: &AccelerationReceipt| {
        receipt.action_kind == ActionKind::ManualActionRequired
            && receipt
                .action_payload
                .as_deref()
                .is_some_and(|action| action.contains("promotion"))
    };
    let health = health_receipt(&accel, names_promotion)
        .expect("a failed promotion must be recorded on the health surface");
    let stats = stats_receipt(&stats_sink, names_promotion)
        .expect("a failed promotion must reach the statistics surface too");

    assert!(
        promote_called.load(Ordering::SeqCst),
        "the promotion must have been attempted"
    );
    for (surface, receipt) in [("health", &health), ("stats", &stats)] {
        assert_ne!(
            receipt.probe_status,
            ProbeStatus::Active,
            "{surface}: a failed promotion must never claim the accelerated backend is active"
        );
        assert!(
            !receipt.hardware_accelerated,
            "{surface}: a failed promotion must not claim hardware acceleration"
        );
        assert_eq!(
            receipt.active_backend, CPU_DETECTION_BACKEND,
            "{surface}: the processor detector is what stays standing"
        );
        assert_eq!(
            receipt.action_kind,
            ActionKind::ManualActionRequired,
            "{surface}: a failed promotion names an action rather than going quiet"
        );
    }
}

#[test]
fn a_preparation_that_reports_an_error_leaves_the_processor_standing_and_never_promotes() {
    let accel = Arc::new(AccelerationState::new());
    let promoted = Arc::new(AtomicBool::new(false));
    let stats_sink = new_sink();
    let (finish, wait_for_finish) = mpsc::channel();

    let promote_flag = Arc::clone(&promoted);
    let selection = spawn_detection_probe_with_promotion(
        true,
        MODEL_ID,
        INPUT_SHAPE,
        DrivenPreparation {
            finish: wait_for_finish,
        },
        Arc::clone(&accel),
        move || {
            promote_flag.store(true, Ordering::SeqCst);
            Ok(())
        },
        capture_into(&stats_sink),
    );
    assert_eq!(selection.receipt.active_backend, CPU_DETECTION_BACKEND);

    finish
        .send(DetectionForwardProbeOutcome::Failed {
            reason: "adapter never became usable".to_string(),
        })
        .expect("release the preparation with its own error");

    let failed = |receipt: &AccelerationReceipt| receipt.failure_code == FailureCode::ProbeFailed;
    let health = health_receipt(&accel, failed)
        .expect("an error the preparation reported must be recorded on the health surface");
    let stats = stats_receipt(&stats_sink, failed)
        .expect("the same error must reach the statistics surface");

    assert!(
        !promoted.load(Ordering::SeqCst),
        "a preparation that errored must never promote the running detector"
    );
    for (surface, receipt) in [("health", &health), ("stats", &stats)] {
        assert_eq!(
            receipt.active_backend, CPU_DETECTION_BACKEND,
            "{surface}: the processor detector stays standing"
        );
        assert!(
            !receipt.hardware_accelerated,
            "{surface}: an error must not claim a hardware-accelerated backend"
        );
    }
    assert!(
        health
            .evidence_fields
            .values()
            .any(|value| value.contains("adapter never became usable")),
        "the recorded error must be the one the preparation actually reported, not a generic \
         substitute: {:?}",
        health.evidence_fields
    );
}

#[test]
fn a_preparation_still_working_is_never_ended_for_it() {
    // The failure this replaces: a preparation that had not finished was
    // declared over, its request abandoned, and an operator told their working
    // hardware could not do the job — on a machine that later did it. Nothing
    // but the preparation's own outcome may end it, so the surface here is read
    // repeatedly, and asked to still say the same thing, while the preparation
    // is demonstrably unfinished. Silence is still the other defect: the
    // preparation is on the surface throughout, not missing from it.
    let accel = Arc::new(AccelerationState::new());
    let promoted = Arc::new(AtomicBool::new(false));
    let stats_sink = new_sink();
    let (finish, wait_for_finish) = mpsc::channel();

    let promote_flag = Arc::clone(&promoted);
    let selection = spawn_detection_probe_with_promotion(
        true,
        MODEL_ID,
        INPUT_SHAPE,
        DrivenPreparation {
            finish: wait_for_finish,
        },
        Arc::clone(&accel),
        move || {
            promote_flag.store(true, Ordering::SeqCst);
            Ok(())
        },
        capture_into(&stats_sink),
    );
    assert_eq!(selection.receipt.probe_status, ProbeStatus::Preparing);

    for read in 0..20 {
        let receipt = health_receipt(&accel, |receipt| {
            receipt.probe_status == ProbeStatus::Preparing
        })
        .unwrap_or_else(|| {
            panic!("read {read}: a preparation still working must stay on the surface")
        });
        assert_eq!(
            receipt.failure_code,
            FailureCode::None,
            "read {read}: nothing may classify a working preparation as failed"
        );
        assert_eq!(
            receipt.active_backend, CPU_DETECTION_BACKEND,
            "read {read}: the processor keeps running the cameras meanwhile"
        );
        assert!(
            !promoted.load(Ordering::SeqCst),
            "read {read}: nothing is promoted before the preparation says it passed"
        );
    }

    // And it is still able to end the only two ways it may: releasing it now
    // completes it and promotes, proving nothing had quietly written it off.
    finish
        .send(passing_outcome())
        .expect("release the preparation");
    health_receipt(&accel, |receipt| {
        receipt.probe_status == ProbeStatus::Active
    })
    .expect("a preparation nobody gave up on still completes and is entered");
    assert!(
        promoted.load(Ordering::SeqCst),
        "and its completion promotes the running detector"
    );
}

#[test]
fn every_outcome_flows_through_the_single_receipt_path() {
    // One authority constructs, sinks and records every detection receipt.
    // A preparation that finishes quickly must not bypass the statistics sink
    // and become a second source of truth whose surfaces drift apart.
    let accel = Arc::new(AccelerationState::new());
    let stats_sink = new_sink();
    let (finish, wait_for_finish) = mpsc::channel();
    finish
        .send(passing_outcome())
        .expect("this preparation's outcome is ready before it is asked for");

    spawn_detection_probe_with_promotion(
        true,
        MODEL_ID,
        INPUT_SHAPE,
        DrivenPreparation {
            finish: wait_for_finish,
        },
        Arc::clone(&accel),
        || Ok(()),
        capture_into(&stats_sink),
    );

    let stats = stats_receipt(&stats_sink, |receipt| {
        receipt.probe_status == ProbeStatus::Active
    })
    .expect(
        "the result must reach the statistics sink through the one receipt path; a path that \
         bypasses it is a second authority whose surfaces diverge",
    );
    let health = health_receipt(&accel, |receipt| {
        receipt.probe_status == ProbeStatus::Active
    })
    .expect("and the same path records it on the health surface");
    assert_eq!(stats.active_backend, ACCELERATED_DETECTION_BACKEND);
    assert_eq!(health.active_backend, stats.active_backend);
}
