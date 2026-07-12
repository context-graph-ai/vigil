//! C12 — late-pass promotion (the proper fix). When the detection probe
//! completes with a PASS AFTER the receipt deadline, the RUNNING process must
//! promote detection from burn-cpu to the accelerated backend WITHOUT a restart:
//! the detector the camera workers use is swapped live, and the receipt
//! transitions honestly (fallback-for-now at the deadline → Active with the
//! adapter named and evidence that this was a late promotion). A late FAIL, and
//! a PASS whose live swap FAILS, leave CPU standing with an honest classification
//! and NEVER an Active claim. The receipt and the actually-running detector move
//! together on EVERY receipt surface — /health AND `vigil stats` — never a
//! receipt that claims a backend the workers do not run, and never a stats
//! surface still claiming burn-cpu after a real promotion.
//!
//! The detector trait is crate-private, so the live swap is exercised through a
//! promote CLOSURE the runtime wires to its swappable detector handle, and the
//! all-surfaces propagation through an `on_late_receipt` sink the runtime wires
//! to write BOTH the acceleration state (/health) and RuntimeStats (`vigil
//! stats` + the worker provenance read). The RED drives both with doubles; the
//! real worker swap + stats write are the runtime wiring, proven by the dev-box
//! promotion smoke.

#![cfg(feature = "detect-burn-wgpu")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vigil::acceleration::{
    AccelStage, AccelerationReceipt, AccelerationState, ActionKind, FailureCode, ProbeStatus,
};
use vigil::detection_accel::{
    ACCELERATED_DETECTION_BACKEND, CPU_DETECTION_BACKEND, DetectionForwardProbe,
    DetectionForwardProbeOutcome, spawn_detection_probe_with_promotion,
};

const MODEL_ID: &str = "detector-under-test";
const INPUT_SHAPE: &str = "1x3x640x640";
const DEADLINE_ENV: &str = "VIGIL_DETECTION_PROBE_DEADLINE_SECS";
// The bounded LATE window: how long the late recorder waits for a probe that
// overran the initial deadline before it gives up and records a terminal
// "probe did not complete" receipt. Default is generous (far above the VM's
// ~156s cold compile) so C12's wait-out-the-cold-compile is preserved; the
// tests set it short to prove the recorder is never silent on an overrun.
const LATE_ENV: &str = "VIGIL_DETECTION_LATE_PROBE_DEADLINE_SECS";

/// Outlives a short deadline, then passes — the injected stand-in for a real GPU
/// whose cold shader compile finishes after the receipt deadline.
struct SlowThenPassProbe {
    delay: Duration,
}
impl DetectionForwardProbe for SlowThenPassProbe {
    fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
        std::thread::sleep(self.delay);
        DetectionForwardProbeOutcome::Passed {
            selected_device: "test-accelerator".to_string(),
            evidence_fields: BTreeMap::new(),
        }
    }
}

/// Outlives the deadline, then fails — a box whose late probe never succeeds.
struct SlowThenFailProbe {
    delay: Duration,
}
impl DetectionForwardProbe for SlowThenFailProbe {
    fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
        std::thread::sleep(self.delay);
        DetectionForwardProbeOutcome::Failed {
            reason: "adapter never became usable".to_string(),
        }
    }
}

/// The stats-surface sink the runtime wires to RuntimeStats; here a capture.
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

fn wait_for_health_receipt(
    accel: &AccelerationState,
    timeout: Duration,
    ready: impl Fn(&AccelerationReceipt) -> bool,
) -> Option<AccelerationReceipt> {
    poll(timeout, || {
        accel
            .snapshot()
            .into_iter()
            .find(|receipt| receipt.stage == AccelStage::Detection && ready(receipt))
    })
}

fn wait_for_stats_receipt(
    sink: &ReceiptSink,
    timeout: Duration,
    ready: impl Fn(&AccelerationReceipt) -> bool,
) -> Option<AccelerationReceipt> {
    poll(timeout, || {
        sink.lock()
            .expect("sink lock")
            .clone()
            .filter(|receipt| ready(receipt))
    })
}

fn poll<T>(timeout: Duration, mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = attempt() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct EnvGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}
impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

#[test]
fn late_pass_promotes_and_the_promoted_receipt_reaches_every_surface() {
    let _env = EnvGuard::set(DEADLINE_ENV, "1");
    let accel = Arc::new(AccelerationState::new());
    let promoted = Arc::new(AtomicBool::new(false));
    let stats_sink = new_sink();

    let promote_flag = Arc::clone(&promoted);
    let selection = spawn_detection_probe_with_promotion(
        true,
        MODEL_ID,
        INPUT_SHAPE,
        SlowThenPassProbe {
            delay: Duration::from_millis(1500),
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
        ProbeStatus::Fallback,
        "the deadline must return a fallback receipt immediately"
    );
    assert_eq!(selection.receipt.active_backend, CPU_DETECTION_BACKEND);

    // /health surface (acceleration state).
    let health = wait_for_health_receipt(&accel, Duration::from_secs(6), |receipt| {
        receipt.probe_status == ProbeStatus::Active
    })
    .expect("a late PASS must record an Active detection receipt to /health");

    // The running detector was actually swapped.
    assert!(
        promoted.load(Ordering::SeqCst),
        "a late PASS must promote the running detector (invoke the swap)"
    );

    // Contract 1: the SAME promoted receipt must reach the stats surface — the
    // late path today records only into the acceleration state, so `vigil stats`
    // (and the worker provenance read) keeps claiming burn-cpu forever.
    let stats = wait_for_stats_receipt(&stats_sink, Duration::from_secs(6), |receipt| {
        receipt.probe_status == ProbeStatus::Active
    })
    .expect(
        "the promoted receipt must reach the stats surface too, not just /health — vigil stats must not keep claiming burn-cpu after a real promotion",
    );

    for (surface, receipt) in [("/health", &health), ("stats", &stats)] {
        assert!(
            receipt.hardware_accelerated,
            "{surface} promoted receipt must be hardware-accelerated"
        );
        assert_eq!(
            receipt.active_backend, ACCELERATED_DETECTION_BACKEND,
            "{surface} promoted Active receipt must name the accelerated backend the workers now run"
        );
    }
    assert_eq!(
        health.active_backend, stats.active_backend,
        "the receipt must move together across surfaces, never disagree"
    );
    let action = stats
        .action_payload
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        action.contains("promot") && (action.contains("no action") || action.contains("active")),
        "the promoted action must say detection was promoted and is now active / no action required: {action}"
    );
    for stale in ["next start", "next boot", "at least", "raise"] {
        assert!(
            !action.contains(stale),
            "a promoted action must not fall back to the pre-promotion wording (`{stale}`): {action}"
        );
    }
}

#[test]
fn promote_error_records_cpu_standing_fallback_on_every_surface_never_active() {
    let _env = EnvGuard::set(DEADLINE_ENV, "1");
    let accel = Arc::new(AccelerationState::new());
    let promote_called = Arc::new(AtomicBool::new(false));
    let stats_sink = new_sink();

    let called = Arc::clone(&promote_called);
    let selection = spawn_detection_probe_with_promotion(
        true,
        MODEL_ID,
        INPUT_SHAPE,
        // The probe PASSES, but the live swap fails to load the accelerated
        // detector: the workers still run CPU, so no surface may claim Active.
        SlowThenPassProbe {
            delay: Duration::from_millis(1500),
        },
        Arc::clone(&accel),
        move || {
            called.store(true, Ordering::SeqCst);
            Err("could not load accelerated detector for live promotion".to_string())
        },
        capture_into(&stats_sink),
    );
    assert_eq!(selection.receipt.active_backend, CPU_DETECTION_BACKEND);

    let health = wait_for_health_receipt(&accel, Duration::from_secs(6), |receipt| {
        receipt.action_kind == ActionKind::ManualActionRequired
            && receipt
                .action_payload
                .as_deref()
                .is_some_and(|action| action.contains("promotion"))
    })
    .expect("promote Err must record the promotion-failed receipt to /health");
    let stats = wait_for_stats_receipt(&stats_sink, Duration::from_secs(6), |receipt| {
        receipt.action_kind == ActionKind::ManualActionRequired
            && receipt
                .action_payload
                .as_deref()
                .is_some_and(|action| action.contains("promotion"))
    })
    .expect("promote Err must record the promotion-failed receipt to the stats surface too");

    assert!(
        promote_called.load(Ordering::SeqCst),
        "the promotion must have been attempted"
    );
    for (surface, receipt) in [("/health", &health), ("stats", &stats)] {
        assert_ne!(
            receipt.probe_status,
            ProbeStatus::Active,
            "{surface}: promote Err must NEVER claim Active"
        );
        assert!(
            !receipt.hardware_accelerated,
            "{surface}: promote Err must not claim hardware acceleration"
        );
        assert_eq!(
            receipt.active_backend, CPU_DETECTION_BACKEND,
            "{surface}: promote Err leaves the CPU detector standing"
        );
        assert_eq!(
            receipt.action_kind,
            ActionKind::ManualActionRequired,
            "{surface}: a failed promotion is a manual-action fallback, not silent"
        );
    }
}

#[test]
fn late_fail_leaves_cpu_standing_and_never_promotes() {
    let _env = EnvGuard::set(DEADLINE_ENV, "1");
    let accel = Arc::new(AccelerationState::new());
    let promoted = Arc::new(AtomicBool::new(false));
    let stats_sink = new_sink();

    let promote_flag = Arc::clone(&promoted);
    let selection = spawn_detection_probe_with_promotion(
        true,
        MODEL_ID,
        INPUT_SHAPE,
        SlowThenFailProbe {
            delay: Duration::from_millis(1200),
        },
        Arc::clone(&accel),
        move || {
            promote_flag.store(true, Ordering::SeqCst);
            Ok(())
        },
        capture_into(&stats_sink),
    );
    assert_eq!(selection.receipt.active_backend, CPU_DETECTION_BACKEND);

    let health = wait_for_health_receipt(&accel, Duration::from_secs(6), |receipt| {
        receipt.failure_code == FailureCode::ProbeFailed
    })
    .expect("a late FAIL must record the real probe-failure classification to /health");
    let stats = wait_for_stats_receipt(&stats_sink, Duration::from_secs(6), |receipt| {
        receipt.failure_code == FailureCode::ProbeFailed
    })
    .expect(
        "a late FAIL must record the real probe-failure classification to the stats surface too",
    );

    assert!(
        !promoted.load(Ordering::SeqCst),
        "a late FAIL must never promote the running detector"
    );
    for (surface, receipt) in [("/health", &health), ("stats", &stats)] {
        assert_eq!(
            receipt.active_backend, CPU_DETECTION_BACKEND,
            "{surface}: a late FAIL leaves the CPU detector standing"
        );
        assert!(
            !receipt.hardware_accelerated,
            "{surface}: a late FAIL must not claim a hardware-accelerated backend"
        );
    }
}

const LATE_WINDOW_ENV: &str = "VIGIL_DETECTION_LATE_WINDOW_SECS";

/// A probe that outlives even the late window — the stand-in for a hung or
/// leaked GPU path that never reports. The late recorder must still deliver a
/// terminal receipt to every surface; silence is never an outcome.
#[test]
fn overrunning_probe_yields_a_terminal_receipt_within_the_late_window_never_silence() {
    let _deadline = EnvGuard::set(DEADLINE_ENV, "1");
    let _window = EnvGuard::set(LATE_WINDOW_ENV, "2");
    let accel = Arc::new(AccelerationState::new());
    let promoted = Arc::new(AtomicBool::new(false));
    let stats_sink = new_sink();

    let promote_flag = Arc::clone(&promoted);
    let selection = spawn_detection_probe_with_promotion(
        true,
        MODEL_ID,
        INPUT_SHAPE,
        SlowThenPassProbe {
            delay: Duration::from_secs(60),
        },
        Arc::clone(&accel),
        move || {
            promote_flag.store(true, Ordering::SeqCst);
            Ok(())
        },
        capture_into(&stats_sink),
    );
    assert_eq!(selection.receipt.probe_status, ProbeStatus::Fallback);

    let health = wait_for_health_receipt(&accel, Duration::from_secs(8), |receipt| {
        receipt.failure_code != FailureCode::None
            && receipt
                .action_payload
                .as_deref()
                .is_some_and(|action| action.contains("did not complete"))
    })
    .expect(
        "an overrunning probe must yield a TERMINAL late receipt on the health surface within \
         the late window — a silent late recorder leaves the operator staring at the boot \
         receipt forever",
    );
    let stats = wait_for_stats_receipt(&stats_sink, Duration::from_secs(2), |receipt| {
        receipt.failure_code != FailureCode::None
            && receipt
                .action_payload
                .as_deref()
                .is_some_and(|action| action.contains("did not complete"))
    })
    .expect("the same terminal receipt must reach the stats surface — never silence");

    assert_eq!(health.active_backend, CPU_DETECTION_BACKEND);
    assert_eq!(stats.active_backend, CPU_DETECTION_BACKEND);
    assert!(
        !promoted.load(Ordering::SeqCst),
        "a probe that never completed must not promote"
    );
    let action = health
        .action_payload
        .clone()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        action.contains("did not complete") || action.contains("window"),
        "the terminal receipt must name the truth — the probe did not complete within the late \
         window: {action}"
    );
}

/// An in-time PASS must flow through the SAME single receipt path as a late
/// outcome: one authority constructs, sinks, and records every detection
/// receipt regardless of timing — a fast probe must not bypass the stats sink
/// and become a second, divergent source of truth.
#[test]
fn in_time_pass_flows_through_the_single_receipt_path() {
    let _deadline = EnvGuard::set(DEADLINE_ENV, "30");
    let accel = Arc::new(AccelerationState::new());
    let stats_sink = new_sink();

    let selection = spawn_detection_probe_with_promotion(
        true,
        MODEL_ID,
        INPUT_SHAPE,
        SlowThenPassProbe {
            delay: Duration::from_millis(50),
        },
        Arc::clone(&accel),
        || Ok(()),
        capture_into(&stats_sink),
    );

    assert_eq!(
        selection.receipt.probe_status,
        ProbeStatus::Active,
        "a fast probe still classifies Active in time"
    );
    let stats = wait_for_stats_receipt(&stats_sink, Duration::from_secs(4), |receipt| {
        receipt.probe_status == ProbeStatus::Active
    })
    .expect(
        "the in-time Active receipt must reach the stats sink through the single receipt path — \
         a bypassing in-time arm is a second authority whose surfaces diverge from the late flow",
    );
    assert_eq!(stats.active_backend, ACCELERATED_DETECTION_BACKEND);
    let health = wait_for_health_receipt(&accel, Duration::from_secs(4), |receipt| {
        receipt.probe_status == ProbeStatus::Active
    })
    .expect("the same in-time receipt must be recorded on the health surface by the single path");
    assert_eq!(health.active_backend, stats.active_backend);
}
