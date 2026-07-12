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
