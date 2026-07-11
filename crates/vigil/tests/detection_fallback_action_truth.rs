//! The accelerated-detection fallback receipt must tell the operator the
//! truth about THIS build.
//!
//! When the accelerated backend is compiled in, a failed forward probe is a
//! probe failure the operator can act on — it must NOT claim the build lacks
//! the accelerated detector, and it must name the real next steps (raise the
//! probe deadline, verify the GPU is usable). When the accelerated backend is
//! NOT compiled in, the action correctly says the build does not include it.

#[cfg(feature = "detect-burn-wgpu")]
use vigil::acceleration::ActionKind;
use vigil::acceleration::FailureCode;
#[cfg(not(feature = "detect-burn-wgpu"))]
use vigil::detection_accel::select_detection_acceleration;
#[cfg(feature = "detect-burn-wgpu")]
use vigil::detection_accel::{
    DetectionForwardProbe, DetectionForwardProbeOutcome, select_detection_acceleration_with_probe,
};

const MODEL_ID: &str = "detector-under-test";
const INPUT_SHAPE: &str = "1x3x640x640";

#[cfg(feature = "detect-burn-wgpu")]
struct FailingForwardProbe;

#[cfg(feature = "detect-burn-wgpu")]
impl DetectionForwardProbe for FailingForwardProbe {
    fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
        DetectionForwardProbeOutcome::Failed {
            reason: "adapter lost during forward pass".to_string(),
        }
    }
}

/// With the accelerated backend compiled in, a failed forward probe must
/// classify as a probe failure whose action names real next steps and never
/// claims the accelerated detector is missing from this build.
#[cfg(feature = "detect-burn-wgpu")]
#[test]
fn probe_failure_action_names_real_next_steps_when_accelerated_backend_is_compiled() {
    let selection =
        select_detection_acceleration_with_probe(true, MODEL_ID, INPUT_SHAPE, FailingForwardProbe);
    let receipt = selection.receipt;
    assert_eq!(
        receipt.failure_code,
        FailureCode::ProbeFailed,
        "a failing forward probe on a build that carries the accelerated detector must classify as a probe failure, not a missing backend"
    );
    assert_eq!(
        receipt.action_kind,
        ActionKind::ManualActionRequired,
        "a probe failure must ask the operator to act"
    );
    let action = receipt
        .action_payload
        .expect("a probe failure must carry an action the operator can take");
    let lower = action.to_ascii_lowercase();
    for lie in [
        "not in this build",
        "not part of this build",
        "not included in this build",
        "no accelerated detector backend",
        "install a build",
    ] {
        assert!(
            !lower.contains(lie),
            "the fallback action must not claim the accelerated backend is missing when it is compiled in: {action}"
        );
    }
    assert!(
        lower.contains("vigil_detection_probe_deadline_secs")
            || lower.contains("verify the gpu")
            || lower.contains("usable gpu")
            || lower.contains("check the gpu")
            || lower.contains("gpu is usable"),
        "the fallback action must name the real next steps (raise VIGIL_DETECTION_PROBE_DEADLINE_SECS or verify the GPU is usable): {action}"
    );
}

/// Without the accelerated backend compiled in, the fallback must plainly say
/// this build does not include accelerated detection — the honest complement.
#[cfg(not(feature = "detect-burn-wgpu"))]
#[test]
fn fallback_action_says_backend_absent_when_accelerated_detection_is_not_compiled() {
    let selection = select_detection_acceleration(true, MODEL_ID, INPUT_SHAPE);
    let receipt = selection.receipt;
    assert_eq!(
        receipt.failure_code,
        FailureCode::BackendNotCompiled,
        "without the accelerated feature the fallback must classify the backend as not compiled"
    );
    let action = receipt
        .action_payload
        .expect("the fallback must carry an action");
    let lower = action.to_ascii_lowercase();
    assert!(
        lower.contains("not in this build")
            || lower.contains("not part of this build")
            || lower.contains("not included in this build"),
        "without the accelerated feature the action must plainly say the build does not include accelerated detection: {action}"
    );
}
