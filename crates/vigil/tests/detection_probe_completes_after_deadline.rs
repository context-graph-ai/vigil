//! A real GPU's cold shader compile can exceed the startup probe's receipt
//! deadline (measured ~2m36s to first active burn-wgpu). Today the deadline
//! ABANDONS the in-flight probe thread (detection_accel.rs `run_probe_with_deadline`,
//! the `Err(_deadline)` arm), so the box re-pays the whole compile on the next
//! start and the operator is only ever told to "raise the deadline".
//!
//! The adopted contract: when the deadline expires the receipt still classifies
//! fallback NOW (so startup is never blocked), but the probe thread runs to its
//! REAL outcome and records it into the observable receipt store
//! (`AccelerationState`, which /health, `vigil stats`, and the doctor render).
//! A late PASS records an Active detection receipt whose action tells the truth
//! ("GPU verified usable; accelerated detection will be active on next start"),
//! not raise-the-deadline. A late FAIL records its real failure classification.
//! (Hot-swapping the already-running CPU detector is a ledgered follow-up and is
//! deliberately NOT pinned here.)

#![cfg(feature = "detect-burn-wgpu")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vigil::acceleration::{AccelStage, AccelerationState, FailureCode, ProbeStatus};
use vigil::detection_accel::{
    DetectionForwardProbe, DetectionForwardProbeOutcome, spawn_detection_probe_with_late_recording,
};

const MODEL_ID: &str = "detector-under-test";
const INPUT_SHAPE: &str = "1x3x640x640";
const DEADLINE_ENV: &str = "VIGIL_DETECTION_PROBE_DEADLINE_SECS";

/// A probe that outlives a short deadline and then passes — the injected stand-in
/// for a real GPU whose cold shader compile finishes after the receipt deadline.
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

fn wait_for_detection_receipt(
    accel: &AccelerationState,
    timeout: Duration,
    ready: impl Fn(&vigil::acceleration::AccelerationReceipt) -> bool,
) -> Option<vigil::acceleration::AccelerationReceipt> {
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

#[test]
fn deadline_returns_fallback_now_but_probe_runs_to_completion_and_records_late_pass() {
    // Pin a 1 s deadline; the probe takes ~1.5 s, so it must miss the deadline
    // (fallback now) and then finish and record its real PASS.
    let previous = std::env::var_os(DEADLINE_ENV);
    unsafe {
        std::env::set_var(DEADLINE_ENV, "1");
    }

    let accel = Arc::new(AccelerationState::new());
    let selection = spawn_detection_probe_with_late_recording(
        true,
        MODEL_ID,
        INPUT_SHAPE,
        SlowThenPassProbe {
            delay: Duration::from_millis(1500),
        },
        Arc::clone(&accel),
    );

    // The immediate receipt is the honest "fallback for now" — startup is never
    // blocked waiting on the cold compile.
    let immediate = &selection.receipt;
    assert_eq!(
        immediate.probe_status,
        ProbeStatus::Fallback,
        "the deadline must return a fallback receipt immediately so camera startup is never blocked on the cold shader compile"
    );
    assert_eq!(
        immediate.failure_code,
        FailureCode::ProbeFailed,
        "the immediate deadline receipt classifies as a probe timeout, not a missing backend"
    );

    // The probe must NOT have been abandoned: it runs to completion and records
    // its real PASS into the observable receipt store.
    let late = wait_for_detection_receipt(&accel, Duration::from_secs(6), |receipt| {
        receipt.probe_status == ProbeStatus::Active
    })
    .expect(
        "the probe thread must run to completion past the deadline and record its late PASS into the receipt store, not be abandoned",
    );

    unsafe {
        match previous {
            Some(value) => std::env::set_var(DEADLINE_ENV, value),
            None => std::env::remove_var(DEADLINE_ENV),
        }
    }

    assert!(
        late.hardware_accelerated,
        "a late PASS must record a hardware-accelerated detection receipt"
    );
    let action = late.action_payload.unwrap_or_default().to_ascii_lowercase();
    assert!(
        (action.contains("verified") || action.contains("usable"))
            && (action.contains("next start")
                || action.contains("next boot")
                || action.contains("restart")),
        "a late PASS action must tell the operator the GPU is verified usable and accelerated detection comes on the next start, not merely raise-the-deadline: {action}"
    );
    assert!(
        !action.contains("raise vigil_detection_probe_deadline_secs"),
        "a late PASS must stop telling the operator to raise the deadline once the probe has actually succeeded: {action}"
    );
}
