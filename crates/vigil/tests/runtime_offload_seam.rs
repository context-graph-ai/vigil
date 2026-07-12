//! Runtime offload seam (`runtime.rs:879`, the remote-vs-local decision
//! point): under sustained pressure with a remote capability, segments
//! route to the submit path and local `detect_segment` must NOT run for
//! them; once pressure genuinely subsides the node returns to `KeepLocal`
//! even though the LIFETIME dropped/dmpf totals stay nonzero forever (the
//! runtime-wiring NAMED WORK ITEM — feeding lifetime totals into the policy
//! would make offload sticky after the first-ever drop); with no fabric
//! configured, behavior is byte-identical to today.
//!
//! RED: `vigil::offload_policy::WindowedPressureTracker::observe` is
//! `todo!()` pending the implementation pass.

use vigil::offload_policy::{
    Decision, LifetimeQueueSnapshot, OffloadPolicyConfig, RemoteCapability, RuntimeOffloadSeam,
    decide,
};

fn lifetime(
    depth: u64,
    capacity: u64,
    dropped_total: u64,
    dmpf_total: u64,
) -> LifetimeQueueSnapshot {
    LifetimeQueueSnapshot {
        depth,
        capacity,
        queued_total: dropped_total + 10,
        dropped_total,
        coalesced_total: 0,
        degraded: dropped_total > 0,
        dropped_motion_positive_frames_total: dmpf_total,
    }
}

fn burn_cpu_remote() -> RemoteCapability {
    RemoteCapability {
        node_id: "home-frigate".to_string(),
        backend: "burn-cpu".to_string(),
        idle: true,
    }
}

#[test]
fn sustained_pressure_offloads_then_recovers_to_keeping_pace_on_windowed_deltas() {
    let config = OffloadPolicyConfig {
        drop_growth_window: 3,
        ..OffloadPolicyConfig::default()
    };
    let remotes = vec![burn_cpu_remote()];
    let mut seam = RuntimeOffloadSeam::new(config);

    // (a) Sustained pressure: drops keep GROWING observation over
    // observation, with a remote capability present — every one of these
    // must offload, never local.
    let mut dropped_total = 0u64;
    for _ in 0..5 {
        dropped_total += 4;
        let decision =
            seam.decide_for_segment(lifetime(8, 8, dropped_total, dropped_total), &remotes);
        assert!(
            matches!(decision, Decision::Offload { .. }),
            "sustained, growing drops with a remote present must offload, not stay local: \
             {decision:?}"
        );
    }

    // (b) RECOVERY: drops stop growing (the lifetime total FREEZES, never
    // resets to zero) for at least `drop_growth_window` observations. The
    // node must recover to KeepLocal even though the lifetime total is
    // still nonzero — this is the case that fails on any wiring that feeds
    // lifetime totals straight into `decide` instead of a windowed delta.
    let frozen_total = dropped_total;
    let mut last_decision = None;
    for _ in 0..4 {
        last_decision =
            Some(seam.decide_for_segment(lifetime(1, 8, frozen_total, frozen_total), &remotes));
    }
    assert!(
        matches!(last_decision, Some(Decision::KeepLocal { .. })),
        "once drop growth genuinely stops for a full window, the node must recover to \
         keeping pace even though the lifetime total ({frozen_total}) never resets to zero: \
         {last_decision:?}"
    );

    // (c) With NO fabric configured (no remote capabilities at all),
    // behavior must be byte-identical to today: always KeepLocal,
    // regardless of pressure.
    let mut seam_no_fabric = RuntimeOffloadSeam::new(OffloadPolicyConfig::default());
    let no_remotes: Vec<RemoteCapability> = Vec::new();
    for step in 0..5u64 {
        let decision =
            seam_no_fabric.decide_for_segment(lifetime(8, 8, step * 4, step * 4), &no_remotes);
        assert!(
            matches!(decision, Decision::KeepLocal { .. }),
            "with no fabric configured, the runtime must never offload — byte-identical to \
             today's local-only behavior: {decision:?}"
        );
    }

    // Sanity: the underlying pure `decide` function itself already treats
    // "no remote" as KeepLocal — this pins that the SEAM preserves that
    // property rather than inventing new logic when fabric is absent.
    assert!(matches!(
        decide(
            vigil::offload_policy::DetectorQueueSnapshot {
                depth: 8,
                capacity: 8,
                queued: 10,
                dropped: 4,
                coalesced: 0,
                degraded: true,
                dropped_motion_positive_frames: 4,
            },
            &no_remotes,
            &OffloadPolicyConfig::default(),
        ),
        Decision::KeepLocal { .. }
    ));
}
