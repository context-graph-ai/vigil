//! Pressure-offload policy contract (criterion C2). Table-driven per the
//! frozen mapping: a node keeping pace never offloads (even with a remote
//! present — the adversarial anchor); a degraded node with a remote
//! offloads; a degraded node with no remote keeps local, naming the reason;
//! a slow-but-idle CPU remote is still eligible under pressure (eligibility
//! is "would reduce drops", not "is faster than local"). This file is RED:
//! `offload_policy::decide`/`render_offload_decision_receipt` are
//! skeleton-only (`todo!()` bodies) pending the implementation pass.

use vigil::offload_policy::{
    Decision, DetectorQueueSnapshot, OffloadPolicyConfig, RemoteCapability, decide,
    render_offload_decision_receipt,
};

fn keeping_pace_snapshot() -> DetectorQueueSnapshot {
    DetectorQueueSnapshot {
        depth: 0,
        capacity: 8,
        queued: 40,
        dropped: 0,
        coalesced: 0,
        degraded: false,
        dropped_motion_positive_frames: 0,
    }
}

fn degraded_snapshot() -> DetectorQueueSnapshot {
    DetectorQueueSnapshot {
        depth: 8,
        capacity: 8,
        queued: 120,
        dropped: 37,
        coalesced: 4,
        degraded: true,
        dropped_motion_positive_frames: 19,
    }
}

fn idle_gpu_remote() -> RemoteCapability {
    RemoteCapability {
        node_id: "home-frigate".to_string(),
        backend: "burn-wgpu".to_string(),
        idle: true,
    }
}

fn slow_but_idle_cpu_remote() -> RemoteCapability {
    RemoteCapability {
        node_id: "lucid-laptop".to_string(),
        backend: "burn-cpu".to_string(),
        idle: true,
    }
}

struct Case {
    name: &'static str,
    snapshot: DetectorQueueSnapshot,
    remotes: Vec<RemoteCapability>,
    expect_offload: bool,
    why_contains: &'static str,
}

/// C2, table-driven over the frozen mapping. Every arm asserts BOTH the
/// decision variant AND the field-visible receipt line — never a literal
/// concatenated string, per the frozen receipt-assertion rule.
#[test]
fn keeping_pace_never_offloads_and_pressure_with_remote_offloads() {
    let config = OffloadPolicyConfig::default();

    let cases = vec![
        Case {
            name: "keeping pace, remote present -> KeepLocal (adversarial anchor)",
            snapshot: keeping_pace_snapshot(),
            remotes: vec![idle_gpu_remote()],
            expect_offload: false,
            why_contains: "keeping-pace",
        },
        Case {
            name: "degraded, remote present -> Offload",
            snapshot: degraded_snapshot(),
            remotes: vec![idle_gpu_remote()],
            expect_offload: true,
            why_contains: "degraded",
        },
        Case {
            name: "degraded, no remote -> KeepLocal{no-remote-capability}",
            snapshot: degraded_snapshot(),
            remotes: vec![],
            expect_offload: false,
            why_contains: "no-remote-capability",
        },
        Case {
            name: "degraded, slow-but-idle CPU remote -> still eligible (Offload)",
            snapshot: degraded_snapshot(),
            remotes: vec![slow_but_idle_cpu_remote()],
            expect_offload: true,
            why_contains: "degraded",
        },
    ];

    for case in cases {
        let decision = decide(case.snapshot, &case.remotes, &config);

        match (&decision, case.expect_offload) {
            (Decision::KeepLocal { why }, false) => {
                assert!(
                    why.contains(case.why_contains),
                    "case `{}`: KeepLocal why `{why}` must contain `{}`",
                    case.name,
                    case.why_contains
                );
            }
            (Decision::Offload { why, remote }, true) => {
                assert!(
                    why.contains(case.why_contains),
                    "case `{}`: Offload why `{why}` must contain `{}`",
                    case.name,
                    case.why_contains
                );
                assert!(
                    case.remotes.iter().any(|r| r.node_id == remote.node_id),
                    "case `{}`: offloaded-to remote must be one of the known remotes",
                    case.name
                );
            }
            (other, expected) => panic!(
                "case `{}`: expected offload={expected} but got {other:?}",
                case.name
            ),
        }

        let receipt = render_offload_decision_receipt(&decision);
        assert!(
            receipt.starts_with("offload-decision="),
            "case `{}`: receipt `{receipt}` must start with offload-decision=",
            case.name
        );
        assert!(
            receipt.contains("why="),
            "case `{}`: receipt `{receipt}` must be receipt-visible with why=",
            case.name
        );
        if case.expect_offload {
            assert!(
                receipt.contains("offload-decision=offload"),
                "case `{}`: receipt `{receipt}` must name the offload outcome",
                case.name
            );
        } else {
            assert!(
                receipt.contains("offload-decision=keep-local"),
                "case `{}`: receipt `{receipt}` must name the keep-local outcome",
                case.name
            );
        }
    }
}

/// C2 + C10: the happy path (keeping pace, remote present) never touches a
/// single threshold knob — `OffloadPolicyConfig::default()` is sufficient.
#[test]
fn happy_path_never_touches_a_threshold_knob() {
    let decision = decide(
        keeping_pace_snapshot(),
        &[idle_gpu_remote()],
        &OffloadPolicyConfig::default(),
    );
    assert_eq!(
        decision,
        Decision::KeepLocal {
            why: "keeping-pace".to_string()
        },
        "the happy path must resolve to KeepLocal{{keeping-pace}} with defaults only"
    );
}
