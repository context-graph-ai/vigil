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

/// Every base-10 number (integer or decimal) appearing in `text`, in order.
fn base_ten_numbers(text: &str) -> Vec<f64> {
    let mut numbers = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() || (ch == '.' && !current.is_empty()) {
            current.push(ch);
        } else if !current.is_empty() {
            if let Ok(value) = current.trim_end_matches('.').parse::<f64>() {
                numbers.push(value);
            }
            current.clear();
        }
    }
    if let Ok(value) = current.trim_end_matches('.').parse::<f64>() {
        numbers.push(value);
    }
    numbers
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

    // Measured truth, not an unconditional promise. On a box whose warm start
    // still exceeds the default deadline, "active on the next start" is a lie;
    // the action must report the probe's ACTUAL completion time and name the
    // deadline option with a number derived from it (>= the measured time).
    for false_promise in ["next start", "next boot", "on next start", "next restart"] {
        assert!(
            !action.contains(false_promise),
            "a late PASS action must not unconditionally promise activation (`{false_promise}`) — it is false where even a warm start exceeds the default deadline: {action}"
        );
    }
    assert!(
        action.contains("verified") || action.contains("usable"),
        "a late PASS action must still tell the operator the GPU is verified/usable: {action}"
    );
    assert!(
        action.contains("complet"),
        "a late PASS action must state the probe completed (its measured outcome): {action}"
    );
    assert!(
        action.contains("detection_probe_deadline_secs")
            || action.contains("vigil_detection_probe_deadline_secs"),
        "a late PASS action must name the detection probe deadline option/env so the operator can raise it to the measured time: {action}"
    );
    let numbers = base_ten_numbers(&action);
    let measured = *numbers.first().expect(
        "a late PASS action must report the probe's measured completion time as a number of seconds",
    );
    assert!(
        measured >= 1.0,
        "the reported measured completion time must reflect the real >1s probe run, got {measured}: {action}"
    );
    assert!(
        action.contains("at least"),
        "a late PASS action must recommend a deadline of at least the measured completion time: {action}"
    );
    let recommended = numbers.iter().copied().fold(measured, f64::max);
    assert!(
        recommended >= measured,
        "the recommended deadline threshold must be >= the measured completion time ({measured}s): {action}"
    );
}
