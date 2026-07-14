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
    // UPDATED for fix cycle 7 (pressure hysteresis): recovery now requires the
    // queue to genuinely drain (depth==0) for K consecutive observations, not
    // just drop-growth to stop with the queue still non-empty. The prior
    // version asserted flip-back after `drop_growth_window` with depth=1; under
    // hysteresis a depth=1 trough is NOT recovery (C2 asymmetry), so the
    // recovery leg below drains to depth=0 and feeds past K.
    let config = OffloadPolicyConfig {
        drop_growth_window: 3,
        pressure_exit_observations: 3,
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
    // resets to zero) AND the queue drains to depth 0. The node must recover
    // to KeepLocal even though the lifetime total is still nonzero — this is
    // the case that fails on any wiring that feeds lifetime totals straight
    // into `decide` instead of a windowed delta. Under hysteresis it takes the
    // window draining plus K consecutive depth-0 observations; 10 is ample.
    let frozen_total = dropped_total;
    let mut last_decision = None;
    for _ in 0..10 {
        last_decision =
            Some(seam.decide_for_segment(lifetime(0, 8, frozen_total, frozen_total), &remotes));
    }
    assert!(
        matches!(last_decision, Some(Decision::KeepLocal { .. })),
        "once drop growth genuinely stops and the queue drains for a full hysteresis window, \
         the node must recover to keeping pace even though the lifetime total ({frozen_total}) \
         never resets to zero: {last_decision:?}"
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

// ── Fix cycle 7: pressure hysteresis ──────────────────────────────────────
// The S3 limit cycle: with the memoryless default (drop_growth_window=1) a
// 180s-pinned local pile drops, one decision offloads (growth>0), the NEXT
// decision sees zero new drops + post-dequeue depth 0 and flips back to
// keep-local, re-committing to another 180s pin — ~4 offloads / 815 dmpf in a
// 10-min saturated window. Hysteresis holds pressure mode across the zero-
// growth troughs (C2 asymmetry: a wrong keep-local costs a full local period
// of drops; a wrong offload costs nothing) and exits only after K consecutive
// genuinely-pressure-free observations.

fn idle_remote() -> RemoteCapability {
    burn_cpu_remote()
}

#[test]
fn sustained_pressure_stays_offloaded_across_zero_growth_troughs_until_k_clear() {
    // drop_growth_window=1 is the memoryless default that causes the flip-back;
    // K=4 keeps the trace short. An idle remote is present throughout.
    let config = OffloadPolicyConfig {
        drop_growth_window: 1,
        pressure_exit_observations: 4,
        ..OffloadPolicyConfig::default()
    };
    let remotes = vec![idle_remote()];
    let mut seam = RuntimeOffloadSeam::new(config);

    // Seed baseline (keeping pace) then a drops burst that enters pressure mode.
    assert!(matches!(
        seam.decide_for_segment(lifetime(0, 8, 0, 0), &remotes),
        Decision::KeepLocal { .. }
    ));
    assert!(
        matches!(
            seam.decide_for_segment(lifetime(8, 8, 100, 100), &remotes),
            Decision::Offload { .. }
        ),
        "a drops burst with a remote present must offload"
    );

    // Zero-growth, depth-0 troughs: for N < K these MUST stay Offload (this is
    // exactly what fails today — the policy flips to keep-local on the first
    // trough). Frozen lifetime total (never resets to zero), depth 0.
    for n in 1..config.pressure_exit_observations {
        let decision = seam.decide_for_segment(lifetime(0, 8, 100, 100), &remotes);
        assert!(
            matches!(decision, Decision::Offload { .. }),
            "observation {n} of the zero-growth trough (N < K={}) must STAY offloaded under \
             hysteresis, not flip back to keep-local: {decision:?}",
            config.pressure_exit_observations
        );
    }

    // The K-th consecutive genuinely-pressure-free observation exits pressure
    // mode and returns to keeping pace.
    let exit = seam.decide_for_segment(lifetime(0, 8, 100, 100), &remotes);
    assert!(
        matches!(exit, Decision::KeepLocal { .. }),
        "the K-th consecutive pressure-free observation must exit pressure mode: {exit:?}"
    );
}

#[test]
fn a_node_never_under_pressure_never_offloads_through_the_seam() {
    // C2 sentence one, at the seam: hysteresis only affects the EXIT side, so a
    // node that never enters pressure must never offload, even with an idle
    // remote present every observation.
    let remotes = vec![idle_remote()];
    let mut seam = RuntimeOffloadSeam::new(OffloadPolicyConfig::default());
    for _ in 0..20 {
        let decision = seam.decide_for_segment(lifetime(0, 8, 0, 0), &remotes);
        assert_eq!(
            decision,
            Decision::KeepLocal {
                why: "keeping-pace".to_string()
            },
            "a node keeping pace must never offload — hysteresis must not spuriously enter: \
             {decision:?}"
        );
    }
}

#[test]
fn in_mode_receipts_name_sustained_pressure_distinctly() {
    use vigil::offload_policy::render_offload_decision_receipt;
    let config = OffloadPolicyConfig {
        drop_growth_window: 1,
        pressure_exit_observations: 10,
        ..OffloadPolicyConfig::default()
    };
    let remotes = vec![idle_remote()];
    let mut seam = RuntimeOffloadSeam::new(config);

    seam.decide_for_segment(lifetime(0, 8, 0, 0), &remotes);
    // Instantaneous pressure: reason is the instantaneous cause, not sustained.
    let instantaneous = seam.decide_for_segment(lifetime(8, 8, 100, 100), &remotes);
    let instantaneous_receipt = render_offload_decision_receipt(&instantaneous);
    assert!(
        matches!(&instantaneous, Decision::Offload { why, .. } if why != "sustained-pressure"),
        "an offload driven by instantaneous growth must NOT read as sustained-pressure: \
         {instantaneous_receipt}"
    );

    // A zero-growth trough offload is a hysteresis decision: distinct reason.
    let sustained = seam.decide_for_segment(lifetime(0, 8, 100, 100), &remotes);
    let sustained_receipt = render_offload_decision_receipt(&sustained);
    assert!(
        matches!(&sustained, Decision::Offload { why, .. } if why == "sustained-pressure"),
        "a zero-growth in-mode offload must render why=sustained-pressure so receipts \
         distinguish hysteresis from instantaneous decisions: {sustained_receipt}"
    );
    assert!(
        sustained_receipt.contains("why=sustained-pressure"),
        "the rendered receipt must carry the distinct sustained-pressure reason: \
         {sustained_receipt}"
    );
}
