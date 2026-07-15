//! Pressure-offload policy (criterion C2). The offload decision derives
//! ONLY from the node's own observed detector-queue state plus whether a
//! remote detector capability is currently known — never from a routing
//! config or a happy-path knob. A node keeping pace (no drops, queue not
//! saturated) NEVER offloads, even when a remote is present. The decision
//! is receipt-visible both ways (why offloading / why not).
//!
//! [`decide`] and [`render_offload_decision_receipt`] implement that policy.

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
    /// Pressure HYSTERESIS exit width (fix cycle 7): once a node has entered
    /// pressure mode it keeps offloading (idle remote present) until this many
    /// CONSECUTIVE genuinely-pressure-free observations (`dropped == 0` AND
    /// `depth == 0` at decision time). Sized so re-probing local capacity is
    /// infrequent relative to the C2 cost of a wrong keep-local: a wrong
    /// keep-local costs a whole local-processing period of drops, a wrong
    /// offload costs nothing. Only affects the EXIT side — a node never under
    /// pressure never offloads.
    pub pressure_exit_observations: u32,
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
            pressure_exit_observations: 10,
            fallback_horizon_ms: 5_000,
            max_attempts: 3,
            blob_cap_bytes: 16 * 1024 * 1024,
        }
    }
}

impl OffloadPolicyConfig {
    /// Every field at its sane default (identical to [`Default::default`])
    /// except `fallback_horizon_ms`, taken from the config surface
    /// (criterion C10, fix cycle 9). The one legal construction for a
    /// caller that has resolved an operator-configured fallback horizon but
    /// wants every other threshold left at its own default — callers
    /// outside this file must never spell a bare `::default()` for that
    /// case, since it would silently drop the configured value.
    pub fn with_fallback_horizon_ms(fallback_horizon_ms: u64) -> Self {
        Self {
            fallback_horizon_ms,
            ..Self::default()
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
pub fn render_offload_fallback_receipt(why: &str) -> String {
    format!("offload-fallback=local why={why}")
}

/// The C5 dead-worker steal decision (fix cycle 8): may the SUBMITTER reclaim a
/// stranded offloaded job by abandoning its dead claimant's attempt? True only
/// when BOTH hold — the fallback horizon has elapsed (`now_ms > deadline_ms`),
/// AND the claimant has fallen out of the live capability set. A job with no
/// fallback deadline is never an offload job and is never stolen.
///
/// Capability liveness — not the wall clock — is the discriminator, and that is
/// the whole point: a merely SLOW-but-alive worker keeps re-advertising on its
/// poll cadence, so it stays in `live_capability_node_ids` and is NEVER stolen
/// no matter how far past the deadline it runs. Only a claimant whose heartbeat
/// has genuinely stopped (aged out of the fresh set) is reclaimed. This is what
/// makes the steal safe where a blunt lease-cap would not be: capping the lease
/// at the horizon would yank work from a live-but-slow worker and double-execute
/// it; keying on sustained heartbeat absence cannot.
pub fn should_steal_stranded_claim(
    now_ms: i64,
    deadline_ms: Option<i64>,
    claimant_node_id: &str,
    live_capability_node_ids: &[String],
) -> bool {
    let Some(deadline_ms) = deadline_ms else {
        return false;
    };
    if now_ms <= deadline_ms {
        return false;
    }
    !live_capability_node_ids
        .iter()
        .any(|node_id| node_id == claimant_node_id)
}

/// Render one remote-detection provenance line (criterion C7): every
/// remotely-executed detection's node + backend, in the ONE shape stats/
/// doctor/health all print — never a per-surface reimplementation, so
/// divergence between surfaces is structurally impossible.
pub fn render_remote_detection_receipt(node_id: &str, backend: &str) -> String {
    format!("node={node_id} backend={backend}")
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
    node_id: &str,
    backend: &str,
) -> [String; 3] {
    let line = render_remote_detection_receipt(node_id, backend);
    [line.clone(), line.clone(), line]
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
    ///
    /// The baseline is the OLDEST retained observation (up to `window`
    /// observations back), never the immediately-previous one and never a
    /// running lifetime origin: `dropped`/`dropped_motion_positive_frames`
    /// are this observation's lifetime total minus that baseline's —
    /// growth over the window, not since process start. `degraded` is
    /// likewise derived from that same windowed growth (never passed
    /// through from the lifetime snapshot's own possibly-sticky flag), so a
    /// node recovers to `KeepLocal` once growth has genuinely stopped for a
    /// full window — even though the lifetime counters never reset to
    /// zero. `depth`/`capacity` pass through as instantaneous values.
    pub fn observe(&mut self, lifetime: LifetimeQueueSnapshot) -> DetectorQueueSnapshot {
        let baseline = self.history.front().copied().unwrap_or(lifetime);
        let dropped_growth = lifetime
            .dropped_total
            .saturating_sub(baseline.dropped_total);
        let dmpf_growth = lifetime
            .dropped_motion_positive_frames_total
            .saturating_sub(baseline.dropped_motion_positive_frames_total);

        self.history.push_back(lifetime);
        while self.history.len() > self.window as usize {
            self.history.pop_front();
        }

        DetectorQueueSnapshot {
            depth: lifetime.depth,
            capacity: lifetime.capacity,
            queued: lifetime.queued_total,
            dropped: dropped_growth,
            coalesced: lifetime.coalesced_total,
            degraded: dropped_growth > 0 || dmpf_growth > 0,
            dropped_motion_positive_frames: dmpf_growth,
        }
    }
}

/// This node's fabric enrollment facts, for the ONE `fabric-status` receipt
/// line every operator surface renders (criterion C7): whether fabric is
/// running at all, this node's role (`hub`/`edge`), the remote detector
/// capabilities currently known, and whether any are actually in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricStatusFacts {
    pub enrolled: bool,
    pub role: &'static str,
    pub remote_detectors: Vec<RemoteCapability>,
    pub in_use: bool,
    /// Whether this node's OWN fabric worker loop is actually running and
    /// serving detection to the fleet. `enrolled=true` alone never means
    /// "serving" — a node with a rejected ticket, or one that never loaded a
    /// worker detector, is enrolled-looking but idle. This is the distinct,
    /// greppable not-serving signal every surface prints (owner steer
    /// 2026-07-13 / criterion C6).
    pub worker_serving: bool,
    /// When not serving, the named reason (why the worker loop is not
    /// running) so the operator reads an action, not just a bare `false`.
    pub worker_serving_reason: String,
}

/// Render the `fabric-status=` line every surface (stats/doctor/health)
/// prints — the ONE vocabulary (criterion C7): a surface that formatted its
/// own fabric summary instead of calling this function would diverge, and a
/// fresh-eyes review can diff this single renderer's callers to catch it.
pub fn render_fabric_status_receipt(facts: &FabricStatusFacts) -> String {
    let remotes = facts
        .remote_detectors
        .iter()
        .map(|remote| format!("{}:{}", remote.node_id, remote.backend))
        .collect::<Vec<_>>()
        .join(",");
    let serving = if facts.worker_serving {
        "fabric-worker-serving=true".to_string()
    } else {
        format!(
            "fabric-worker-serving=false reason={}",
            if facts.worker_serving_reason.is_empty() {
                "not-serving"
            } else {
                facts.worker_serving_reason.as_str()
            }
        )
    };
    format!(
        "fabric-status=enrolled={} role={} remote-detectors={} in-use={} {}",
        facts.enrolled,
        facts.role,
        if remotes.is_empty() {
            "-".to_string()
        } else {
            remotes
        },
        facts.in_use,
        serving
    )
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
    /// Pressure HYSTERESIS state (fix cycle 7). `drop_growth_window` alone is
    /// memoryless: at the default window=1, a 180s-pinned local pile drops, one
    /// decision offloads (growth>0), the next sees zero new drops + post-dequeue
    /// depth 0 and flips back to keep-local, re-committing to another 180s pin —
    /// the observed S3 limit cycle (~4 offloads / 815 dmpf in a 10-min saturated
    /// window). Once entered, pressure mode holds across zero-growth troughs and
    /// exits only after `pressure_exit_observations` CONSECUTIVE genuinely-
    /// pressure-free observations. Enter is unchanged (`under_pressure`), so a
    /// node never under pressure never offloads (C2 sentence one).
    in_pressure_mode: bool,
    pressure_free_streak: u32,
}

impl RuntimeOffloadSeam {
    pub fn new(config: OffloadPolicyConfig) -> Self {
        Self {
            tracker: WindowedPressureTracker::new(config.drop_growth_window),
            config,
            in_pressure_mode: false,
            pressure_free_streak: 0,
        }
    }

    pub fn decide_for_segment(
        &mut self,
        lifetime: LifetimeQueueSnapshot,
        remotes: &[RemoteCapability],
    ) -> Decision {
        let snapshot = self.tracker.observe(lifetime);
        let saturated = snapshot.capacity > 0
            && (snapshot.depth as f64)
                >= self.config.saturation_fraction * (snapshot.capacity as f64);
        let under_pressure = snapshot.degraded || saturated || snapshot.dropped > 0;
        // Genuinely pressure-free: no new drops AND the queue has drained. A
        // trough with depth still > 0 has NOT recovered, so it must not count
        // toward exiting pressure mode (C2 asymmetry — re-probe local capacity
        // conservatively).
        let pressure_free = snapshot.dropped == 0 && snapshot.depth == 0;

        if under_pressure {
            self.in_pressure_mode = true;
            self.pressure_free_streak = 0;
        } else if self.in_pressure_mode {
            if pressure_free {
                self.pressure_free_streak += 1;
                if self.pressure_free_streak >= self.config.pressure_exit_observations {
                    self.in_pressure_mode = false;
                    self.pressure_free_streak = 0;
                }
            } else {
                self.pressure_free_streak = 0;
            }
        }

        // Instantaneous pressure: the stateless decision + its instantaneous
        // reason (degraded / saturated / drops-growing), unchanged.
        if under_pressure {
            return decide(snapshot, remotes, &self.config);
        }

        // Not under pressure this observation, but pressure mode is still held:
        // keep offloading to an idle remote across the trough. Distinct reason so
        // receipts separate hysteresis decisions from instantaneous ones.
        if self.in_pressure_mode {
            return match remotes.iter().find(|remote| remote.idle) {
                Some(remote) => Decision::Offload {
                    why: "sustained-pressure".to_string(),
                    remote: remote.clone(),
                },
                None => Decision::KeepLocal {
                    why: "no-remote-capability".to_string(),
                },
            };
        }

        // Genuinely keeping pace (never entered, or just exited): never offload.
        decide(snapshot, remotes, &self.config)
    }
}
