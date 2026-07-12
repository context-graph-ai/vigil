//! C12 — late-pass promotion (the proper fix). When the detection probe
//! completes with a PASS AFTER the receipt deadline, the RUNNING process must
//! promote detection from burn-cpu to the accelerated backend WITHOUT a restart:
//! the detector the camera workers use is swapped live, and the receipt
//! transitions honestly (fallback-for-now at the deadline → Active with the
//! adapter named and evidence that this was a late promotion). A late FAIL
//! leaves CPU standing with its real classification. The receipt and the
//! actually-running detector move together — never a receipt that claims a
//! backend the workers do not run.
//!
//! The detector trait is crate-private, so the live swap is exercised through a
//! promote CLOSURE the runtime wires to its swappable detector handle: the
//! library invokes it exactly on a valid late PASS (and never on a FAIL), and
//! couples the recorded Active receipt to that promotion. The full worker-swap
//! (the closure calling the real handle's `promote`) is the runtime's wiring,
//! proven by the dev-box promotion smoke; this pins the library coupling.

#![cfg(feature = "detect-burn-wgpu")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use vigil::acceleration::{
    AccelStage, AccelerationReceipt, AccelerationState, FailureCode, ProbeStatus,
};
use vigil::detection_accel::{
    ACCELERATED_DETECTION_BACKEND, CPU_DETECTION_BACKEND, DetectionForwardProbe,
    DetectionForwardProbeOutcome, spawn_detection_probe_with_promotion,
};

const MODEL_ID: &str = "detector-under-test";
const INPUT_SHAPE: &str = "1x3x640x640";
const DEADLINE_ENV: &str = "VIGIL_DETECTION_PROBE_DEADLINE_SECS";

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

fn wait_for_detection_receipt(
    accel: &AccelerationState,
    timeout: Duration,
    ready: impl Fn(&AccelerationReceipt) -> bool,
) -> Option<AccelerationReceipt> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(receipt) = accel
            .snapshot()
            .into_iter()
            .find(|receipt| receipt.stage == AccelStage::Detection && ready(receipt))
        {
            return Some(receipt);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct EnvGuard {
    previous: Option<std::ffi::OsString>,
}
impl EnvGuard {
    fn set(value: &str) -> Self {
        let previous = std::env::var_os(DEADLINE_ENV);
        unsafe { std::env::set_var(DEADLINE_ENV, value) };
        Self { previous }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(DEADLINE_ENV, value),
                None => std::env::remove_var(DEADLINE_ENV),
            }
        }
    }
}

#[test]
fn late_pass_promotes_the_running_detector_and_the_receipt_moves_with_it() {
    let _env = EnvGuard::set("1");
    let accel = Arc::new(AccelerationState::new());
    let promoted = Arc::new(AtomicBool::new(false));

    let promote_flag = Arc::clone(&promoted);
    let selection = spawn_detection_probe_with_promotion(
        true,
        MODEL_ID,
        INPUT_SHAPE,
        SlowThenPassProbe {
            delay: Duration::from_millis(1500),
        },
        Arc::clone(&accel),
        // The runtime wires this to its swappable detector handle's `promote`;
        // here the double records that the live swap was performed.
        move || {
            promote_flag.store(true, Ordering::SeqCst);
            Ok(())
        },
    );

    // Fallback-for-now at the deadline: startup is never blocked on the cold
    // compile, and the immediate receipt is honest CPU fallback.
    assert_eq!(
        selection.receipt.probe_status,
        ProbeStatus::Fallback,
        "the deadline must return a fallback receipt immediately"
    );
    assert_eq!(selection.receipt.active_backend, CPU_DETECTION_BACKEND);

    // (b) The receipt transitions to Active with the adapter named and late-
    // promotion evidence once the probe completes.
    let late = wait_for_detection_receipt(&accel, Duration::from_secs(6), |receipt| {
        receipt.probe_status == ProbeStatus::Active
    })
    .expect("a late PASS must record an Active detection receipt after the deadline");

    // (a) The running detector was actually swapped — the promotion ran.
    assert!(
        promoted.load(Ordering::SeqCst),
        "a late PASS must promote the running detector (invoke the swap), not just rewrite the receipt"
    );

    // (d) The receipt and the running detector move together: an Active receipt
    // names the accelerated backend the promotion installed, never a backend the
    // workers do not run.
    assert!(
        late.hardware_accelerated,
        "a promoted receipt must be hardware-accelerated"
    );
    assert_eq!(
        late.active_backend, ACCELERATED_DETECTION_BACKEND,
        "a promoted Active receipt must name the accelerated backend the swap installed"
    );
    assert!(
        late.selected_device
            .as_deref()
            .is_some_and(|d| !d.trim().is_empty()),
        "a promoted receipt must name the adapter: {late:?}"
    );
    let action = late.action_payload.unwrap_or_default().to_ascii_lowercase();
    assert!(
        action.contains("promot") && action.contains("deadline"),
        "the promoted action must state detection was promoted after the startup deadline (late-promotion evidence): {action}"
    );
    assert!(
        action.contains("no action")
            || action.contains("now active")
            || action.contains("is active"),
        "the promoted action must say accelerated detection is now active / no action required: {action}"
    );
    for stale in ["next start", "next boot", "at least", "raise"] {
        assert!(
            !action.contains(stale),
            "a promoted action must not fall back to the pre-promotion wording (`{stale}`) — the process already promoted live: {action}"
        );
    }
}

#[test]
fn late_fail_leaves_cpu_standing_and_never_promotes() {
    let _env = EnvGuard::set("1");
    let accel = Arc::new(AccelerationState::new());
    let promoted = Arc::new(AtomicBool::new(false));

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
    );
    assert_eq!(selection.receipt.active_backend, CPU_DETECTION_BACKEND);

    // (c) A late FAIL records its real classification and never promotes.
    let late = wait_for_detection_receipt(&accel, Duration::from_secs(6), |receipt| {
        receipt.failure_code == FailureCode::ProbeFailed
    })
    .expect("a late FAIL must record the real probe-failure classification");

    assert!(
        !promoted.load(Ordering::SeqCst),
        "a late FAIL must never promote the running detector"
    );
    assert_eq!(
        late.active_backend, CPU_DETECTION_BACKEND,
        "a late FAIL leaves the CPU detector standing"
    );
    assert!(
        !late.hardware_accelerated,
        "a late FAIL must not claim a hardware-accelerated backend"
    );
}
