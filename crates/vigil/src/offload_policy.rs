//! Pressure-offload policy (criterion C2). The offload decision derives
//! ONLY from the node's own observed detector-queue state plus whether a
//! remote detector capability is currently known — never from a routing
//! config or a happy-path knob. A node keeping pace (no drops, queue not
//! saturated) NEVER offloads, even when a remote is present. The decision
//! is receipt-visible both ways (why offloading / why not).
//!
//! This module is SKELETON ONLY — [`decide`] and
//! [`render_offload_decision_receipt`] are `todo!()` pending the
//! implementation pass.

/// The detector-queue state an offload decision is computed from — the same
/// fields already rendered on the `detector-queue=` stats line
/// (`runtime.rs`) plus the separate `dropped-motion-positive-frames=` line
/// (`detection_accel.rs`). No other input may feed the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectorQueueSnapshot {
    pub depth: u64,
    pub capacity: u64,
    pub queued: u64,
    pub dropped: u64,
    pub coalesced: u64,
    pub degraded: bool,
    pub dropped_motion_positive_frames: u64,
}

/// A known remote detector capability. `idle` names "not currently
/// saturated with other work" — NOT a speed claim. The criterion admits
/// slow-but-idle CPU workers: eligibility is "would reduce drops", not "is
/// faster than local".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCapability {
    pub node_id: String,
    pub backend: String,
    pub idle: bool,
}

/// The offload decision, always carrying a human-readable `why` so the
/// choice is receipt-visible in both directions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    KeepLocal {
        why: String,
    },
    Offload {
        why: String,
        remote: RemoteCapability,
    },
}

/// Operator-facing, config-as-data thresholds (criterion C10): every one of
/// these has a sane default and the happy path (`decide` with defaults)
/// never needs any of them touched.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OffloadPolicyConfig {
    /// Queue depth/capacity fraction at or above which the node is
    /// considered saturated.
    pub saturation_fraction: f64,
    /// Consecutive degraded observations required before offload considers
    /// the pressure sustained rather than a single blip.
    pub drop_growth_window: u32,
    /// How long a submitter waits for a claimed remote job before reclaiming
    /// it and running locally (the C5 fallback primitive's horizon).
    pub fallback_horizon_ms: u64,
    /// Maximum offload attempts before permanently falling back to local for
    /// a given work item.
    pub max_attempts: u32,
    /// Ceiling on the size of a single moved blob (bytes).
    pub blob_cap_bytes: u64,
}

impl Default for OffloadPolicyConfig {
    fn default() -> Self {
        Self {
            saturation_fraction: 0.8,
            drop_growth_window: 1,
            fallback_horizon_ms: 5_000,
            max_attempts: 3,
            blob_cap_bytes: 16 * 1024 * 1024,
        }
    }
}

/// A node is under pressure when it is already flagged degraded, its queue
/// depth has reached the configured saturation fraction of capacity, or it
/// has dropped anything at all (a snapshot starts at zero drops, so any
/// nonzero count means drops are growing). Any one of the three is enough —
/// this is deliberately OR, not AND: a node that is merely saturated but not
/// yet flagged degraded should still be eligible to shed load before it
/// starts dropping.
fn pressure_reason(snapshot: &DetectorQueueSnapshot, saturated: bool) -> &'static str {
    if snapshot.degraded {
        "degraded"
    } else if saturated {
        "saturated"
    } else {
        "drops-growing"
    }
}

/// Decide whether to keep detection local or offload to a remote, from the
/// queue snapshot and known remote capabilities ONLY — never from a routing
/// config or a happy-path knob (criterion C2). Eligibility for an offload
/// target is "idle" (would reduce drops), never a speed comparison: a
/// slow-but-idle CPU remote is exactly as eligible as a fast idle GPU one.
pub fn decide(
    snapshot: DetectorQueueSnapshot,
    remotes: &[RemoteCapability],
    config: &OffloadPolicyConfig,
) -> Decision {
    let saturated = snapshot.capacity > 0
        && (snapshot.depth as f64) >= config.saturation_fraction * (snapshot.capacity as f64);
    let drops_growing = snapshot.dropped > 0;
    let under_pressure = snapshot.degraded || saturated || drops_growing;

    if !under_pressure {
        return Decision::KeepLocal {
            why: "keeping-pace".to_string(),
        };
    }

    // Prefer an idle remote; eligibility is "not currently saturated with
    // other work", never a speed claim, so the first idle remote found is
    // as good a choice as any other.
    match remotes.iter().find(|remote| remote.idle) {
        Some(remote) => Decision::Offload {
            why: pressure_reason(&snapshot, saturated).to_string(),
            remote: remote.clone(),
        },
        None => Decision::KeepLocal {
            why: "no-remote-capability".to_string(),
        },
    }
}

/// Render the decision as the receipt line an operator surface prints —
/// `offload-decision=<keep-local|offload> why=<reason>` (plus
/// `remote=...`/`backend=...` when offloading).
pub fn render_offload_decision_receipt(decision: &Decision) -> String {
    match decision {
        Decision::KeepLocal { why } => format!("offload-decision=keep-local why={why}"),
        Decision::Offload { why, remote } => format!(
            "offload-decision=offload why={why} remote={} backend={}",
            remote.node_id, remote.backend
        ),
    }
}

/// Render the degraded-never-dead local-fallback receipt (criterion C5):
/// `offload-fallback=local why=<lease-expired|deadline>`. Named so status/
/// doctor/stats never guess at the reason — the SAME renderer as
/// [`render_offload_decision_receipt`] and
/// [`render_remote_detection_receipt`], so every fabric receipt line stays
/// one vocabulary (criterion C7) by construction: there is exactly one
/// function per line shape, never a per-surface reimplementation.
pub fn render_offload_fallback_receipt(_why: &str) -> String {
    todo!("render offload-fallback=local why=<lease-expired|deadline> (C5)")
}

/// Render one remote-detection provenance line (criterion C7): every
/// remotely-executed detection's node + backend, in the ONE shape stats/
/// doctor/health all print — never a per-surface reimplementation, so
/// divergence between surfaces is structurally impossible.
pub fn render_remote_detection_receipt(_node_id: &str, _backend: &str) -> String {
    todo!("render node=<node_id> backend=<backend> (C7)")
}

/// The IDENTICAL provenance line rendered by every operator surface that
/// reports a remote detection's node + backend today (`vigil stats`,
/// `vigil doctor`, `/health`) — order is `[stats, doctor, health]`. Each
/// surface must call [`render_remote_detection_receipt`] and NOTHING else
/// for this line, so the three entries are always equal by construction;
/// this function is the one seam a fresh-eyes review checks to catch a
/// surface that grew its own per-surface formatting instead (criterion
/// C7's "one vocabulary" requirement, USR-10/AGT-12).
pub fn render_remote_detection_receipt_for_every_surface(
    _node_id: &str,
    _backend: &str,
) -> [String; 3] {
    todo!("stats/doctor/health each call render_remote_detection_receipt, nothing else (C7)")
}

/// The runtime's raw, LIFETIME queue counters — the same numbers already
/// rendered on the `detector-queue=` stats line (`runtime.rs`) plus the
/// separate lifetime `dropped-motion-positive-frames=` line
/// (`detection_accel.rs`). These never feed [`decide`] directly: a single
/// historical drop would make offload sticky forever (the runtime-wiring
/// NAMED WORK ITEM — see the working doc). [`WindowedPressureTracker`]
/// turns this into the WINDOWED [`DetectorQueueSnapshot`] `decide` actually
/// consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifetimeQueueSnapshot {
    pub depth: u64,
    pub capacity: u64,
    pub queued_total: u64,
    pub dropped_total: u64,
    pub coalesced_total: u64,
    pub degraded: bool,
    pub dropped_motion_positive_frames_total: u64,
}

/// Turns the runtime's lifetime counters into the WINDOWED
/// [`DetectorQueueSnapshot`] the offload policy consumes: `dropped` and
/// `dropped_motion_positive_frames` become growth over the configured
/// `drop_growth_window` observations, never the raw lifetime total — so a
/// node recovers to `KeepLocal` once pressure genuinely subsides, even
/// though the lifetime totals stay nonzero forever. `depth`/`capacity`/
/// `degraded` pass through as instantaneous values (already
/// window-neutral).
pub struct WindowedPressureTracker {
    window: u32,
    history: std::collections::VecDeque<LifetimeQueueSnapshot>,
}

impl WindowedPressureTracker {
    pub fn new(window: u32) -> Self {
        Self {
            window: window.max(1),
            history: std::collections::VecDeque::new(),
        }
    }

    /// Feed one lifetime snapshot; returns the windowed
    /// [`DetectorQueueSnapshot`] `decide` should be called with for this
    /// observation.
    pub fn observe(&mut self, _lifetime: LifetimeQueueSnapshot) -> DetectorQueueSnapshot {
        todo!(
            "compute dropped/dropped_motion_positive_frames as GROWTH over the last \
             drop_growth_window observations, never the lifetime total (the runtime-wiring \
             NAMED WORK ITEM)"
        )
    }
}

/// The runtime's per-segment offload decision point (criterion C2 wiring,
/// `runtime.rs:879`): call this INSTEAD OF `detector.detect_segment`
/// directly. When the decision is `KeepLocal`, the caller must still run
/// its own local `detect_segment`; when `Offload`, the caller must submit
/// instead and must NEVER also run local detection for that same segment.
/// With `remotes` always empty (no fabric configured), this must behave
/// byte-identically to today: always `KeepLocal`, no receipts, no decision
/// lines.
pub struct RuntimeOffloadSeam {
    tracker: WindowedPressureTracker,
    config: OffloadPolicyConfig,
}

impl RuntimeOffloadSeam {
    pub fn new(config: OffloadPolicyConfig) -> Self {
        Self {
            tracker: WindowedPressureTracker::new(config.drop_growth_window),
            config,
        }
    }

    pub fn decide_for_segment(
        &mut self,
        lifetime: LifetimeQueueSnapshot,
        remotes: &[RemoteCapability],
    ) -> Decision {
        let snapshot = self.tracker.observe(lifetime);
        decide(snapshot, remotes, &self.config)
    }
}
