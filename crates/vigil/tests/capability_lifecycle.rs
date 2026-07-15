//! Capability LIFECYCLE correctness (distributed-compute fix cycle 4, the S4
//! defect). A worker that served `burn-wgpu`, was restarted with acceleration
//! off, and truthfully re-advertised `burn-cpu` must render — on the main
//! (hub) node — as ONLY its current backend; a worker that has stopped
//! contacting the hub must age out of both the rendered remote-detectors line
//! and the offload-routing decision; and both surfaces must consume the SAME
//! live/current capability set (one vocabulary).
//!
//! These are in-process `FabricRuntime` tests. A remote worker's advertised
//! rows are landed directly into this node's ledger with
//! `advertise_capability` — exactly the rows a sync-apply of that worker's
//! push would land — so the currency/liveness contract is exercised without a
//! second process.
//!
//! RED today, for the defect's reasons:
//!   * `remote_detector_capabilities` (fabric.rs:541) returns EVERY
//!     `vigil-detector-*` row per node, so a restarted worker's stale
//!     `burn-wgpu` row renders alongside its current `burn-cpu` one — nothing
//!     picks the LATEST-advertised row as current (RED-4).
//!   * nothing ages a capability out on liveness — a row, once present, is
//!     offered forever even after the worker is gone (RED-5).
//!   * the offload decision (`offload_policy::decide`) consumes that same
//!     unfiltered set, so routing can pick a dead/stale worker the render
//!     should never have shown (RED-6).

#![cfg(feature = "fabric")]

use contextdb_engine::work_ledger::advertise_capability;

use vigil::fabric::{FabricRuntime, detector_capability_id};
use vigil::offload_policy::{
    Decision, DetectorQueueSnapshot, FabricStatusFacts, OffloadPolicyConfig, decide,
    render_fabric_status_receipt,
};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis() as i64
}

fn tags(list: &[&str]) -> Vec<String> {
    list.iter().map(|t| t.to_string()).collect()
}

/// The `remote-detectors=` fragment every operator surface renders, built the
/// same way `fabric_bring_up`'s status task builds it: the live/current remote
/// capability set fed through the ONE `render_fabric_status_receipt`.
async fn rendered_status_line(runtime: &FabricRuntime) -> String {
    let remotes = runtime
        .remote_detector_capabilities()
        .await
        .expect("scan remote detector capabilities");
    let facts = FabricStatusFacts {
        enrolled: true,
        role: "hub",
        in_use: !remotes.is_empty(),
        remote_detectors: remotes,
        worker_serving: true,
        worker_serving_reason: String::new(),
    };
    render_fabric_status_receipt(&facts)
}

/// A snapshot that is unambiguously under pressure (degraded + dropping), so
/// an eligible remote would be chosen — mirrors `offload_policy.rs`'s
/// degraded case.
fn pressured_snapshot() -> DetectorQueueSnapshot {
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

// RED-4 — a restarted worker renders ONLY its current backend ─────────────────
#[tokio::test]
async fn a_restarted_worker_renders_only_its_current_backend() {
    let dir = tempfile::tempdir().expect("data dir");
    let runtime = FabricRuntime::start(dir.path(), None, true)
        .await
        .expect("hub node must stand up");
    let now = now_ms();

    // The worker first served burn-wgpu, then was restarted with acceleration
    // off and re-advertised burn-cpu (the LATER, current claim). Both
    // write-once rows land on the hub — the exact S4 state.
    advertise_capability(
        &runtime.db,
        "worker-x",
        &detector_capability_id("burn-wgpu"),
        &tags(&["backend:burn-wgpu"]),
        now - 5_000,
    )
    .expect("worker advertises burn-wgpu");
    advertise_capability(
        &runtime.db,
        "worker-x",
        &detector_capability_id("burn-cpu"),
        &tags(&["backend:burn-cpu"]),
        now - 1_000,
    )
    .expect("worker re-advertises burn-cpu after the acceleration-off restart");

    let line = rendered_status_line(&runtime).await;
    assert!(
        line.contains("worker-x:burn-cpu"),
        "the worker's CURRENT backend (burn-cpu) must render; got: {line}"
    );
    assert!(
        !line.contains("worker-x:burn-wgpu"),
        "a restarted worker's stale burn-wgpu row must NOT render alongside its \
         current burn-cpu — the current capability is the LATEST advertised; \
         got: {line}"
    );
}

// RED-5 — a dead worker ages out of the rendered remote-detectors ─────────────
#[tokio::test]
async fn a_dead_worker_ages_out_of_remote_detectors() {
    let dir = tempfile::tempdir().expect("data dir");
    let runtime = FabricRuntime::start(dir.path(), None, true)
        .await
        .expect("hub node must stand up");
    let now = now_ms();

    // A worker advertised long ago and has not contacted the hub since. Its
    // last-contact (for a never-refreshed advertisement, the advertised_at
    // itself is the floor) is an hour old — far past any sane TTL.
    advertise_capability(
        &runtime.db,
        "dead-worker",
        &detector_capability_id("burn-cpu"),
        &tags(&["backend:burn-cpu"]),
        now - 3_600_000,
    )
    .expect("stale advertisement lands");

    let line = rendered_status_line(&runtime).await;
    assert!(
        !line.contains("dead-worker"),
        "a worker that stopped contacting the hub must age out of \
         remote-detectors within the TTL, not render forever; got: {line}"
    );
}

// RED-6 — offload routes to the live worker, never a dead one ─────────────────
#[tokio::test]
async fn offload_routes_to_the_live_worker_never_a_dead_one() {
    let dir = tempfile::tempdir().expect("data dir");
    let runtime = FabricRuntime::start(dir.path(), None, true)
        .await
        .expect("hub node must stand up");
    let now = now_ms();

    advertise_capability(
        &runtime.db,
        "live-worker",
        &detector_capability_id("burn-cpu"),
        &tags(&["backend:burn-cpu"]),
        now - 1_000,
    )
    .expect("live worker advertises");
    advertise_capability(
        &runtime.db,
        "dead-worker",
        &detector_capability_id("burn-wgpu"),
        &tags(&["backend:burn-wgpu"]),
        now - 3_600_000,
    )
    .expect("dead worker's stale advertisement lands");

    // ONE VOCABULARY: routing must consume exactly the set the render shows.
    let remotes = runtime
        .remote_detector_capabilities()
        .await
        .expect("scan remote detector capabilities");
    assert!(
        !remotes.iter().any(|remote| remote.node_id == "dead-worker"),
        "a dead worker must not appear in the routable/renderable capability \
         set — render and routing share one vocabulary; got: {remotes:?}"
    );

    let decision = decide(
        pressured_snapshot(),
        &remotes,
        &OffloadPolicyConfig::default(),
    );
    match decision {
        Decision::Offload { remote, .. } => assert_eq!(
            remote.node_id, "live-worker",
            "under pressure, offload must route to the LIVE worker, never the \
             stale one"
        ),
        Decision::KeepLocal { why } => panic!(
            "a live worker exists — offload must route to it, got keep-local \
             why={why}"
        ),
    }
}

// RED-6 (fallback arm) — with only a dead worker known, no offload ────────────
#[tokio::test]
async fn only_a_dead_worker_means_no_offload() {
    let dir = tempfile::tempdir().expect("data dir");
    let runtime = FabricRuntime::start(dir.path(), None, true)
        .await
        .expect("hub node must stand up");
    let now = now_ms();

    advertise_capability(
        &runtime.db,
        "dead-worker",
        &detector_capability_id("burn-cpu"),
        &tags(&["backend:burn-cpu"]),
        now - 3_600_000,
    )
    .expect("stale advertisement lands");

    let remotes = runtime
        .remote_detector_capabilities()
        .await
        .expect("scan remote detector capabilities");
    let decision = decide(
        pressured_snapshot(),
        &remotes,
        &OffloadPolicyConfig::default(),
    );
    assert!(
        matches!(decision, Decision::KeepLocal { .. }),
        "with only a dead worker known, offload must fall back to local \
         detection; got: {decision:?}"
    );
}

// A never-invoked executor: the worker loop below claims no work (there are no
// jobs), so `execute` is unreachable — it exists only to satisfy the trait.
struct NoopExecutor;

impl contextdb_server::work_ledger::WorkExecutor for NoopExecutor {
    fn execute(
        &self,
        _job: &contextdb_engine::work_ledger::JobSnapshot,
        _inputs: &contextdb_engine::work_ledger::ExecutionInputs,
        _should_abandon: &dyn Fn() -> bool,
    ) -> contextdb_server::work_ledger::ExecutionVerdict {
        contextdb_server::work_ledger::ExecutionVerdict::Abandoned
    }
}

// RED (fix cycle 4 review, finding 1) — a LIVE, re-advertising worker must
// STAY rendered/routed on a CONSUMER edge across several TTLs ───────────────
//
// The mirror of RED-5: where a DEAD worker (a frozen advertisement) must age
// out, a LIVE worker whose standing loop keeps advertising must NOT. A consumer
// edge has no hub-local last-contact clock (`work_node_contacts` never syncs to
// edges), so liveness there falls back to the freshest advertisement the node
// has synced. Fails before the fix: the worker's standing loop advertised once
// and only RE-PUSHED the unchanged row, so a perfectly live worker's
// advertisement went stale and it aged out of render + routing after the TTL —
// even though it never stopped polling.
#[tokio::test]
async fn a_live_worker_stays_rendered_on_a_consumer_edge_across_several_ttls() {
    let dir = tempfile::tempdir().expect("data dir");
    // A CONSUMER edge (not a hub): no hub-local work_node_contacts table, so
    // liveness uses the synced advertisement. A short TTL makes "several TTLs"
    // a fast wall-clock window.
    let runtime = FabricRuntime::start(dir.path(), None, false)
        .await
        .expect("consumer edge stands up")
        .with_capability_liveness_ttl_ms(150);

    // The remote worker's current detector backend, as its first sync landed it.
    advertise_capability(
        &runtime.db,
        "live-worker",
        &detector_capability_id("burn-cpu"),
        &tags(&["backend:burn-cpu"]),
        now_ms(),
    )
    .expect("worker's detector capability lands");

    // The remote worker's STANDING loop, writing into this consumer's store the
    // way its synced pushes would. An in-process hub receives its pushes so the
    // loop advances promptly (a server-less push would back off for seconds);
    // the hub's per-node contact clock lives in the SEPARATE hub db, so the
    // consumer store stays contact-free and liveness there is purely the synced
    // advertisement. The consumer reads the worker's re-advertisements from the
    // SHARED local store directly.
    let broker = contextdb_server::InProcessBroker::new();
    let hub_db = std::sync::Arc::new(contextdb_engine::Database::open_memory());
    let hub = std::sync::Arc::new(contextdb_server::SyncServer::with_transport(
        hub_db.clone(),
        broker.server(),
        contextdb_core::TenantId::from("vigil-fabric"),
        contextdb_engine::sync_types::ConflictPolicies::uniform(
            contextdb_engine::sync_types::ConflictPolicy::LatestWins,
        ),
    ));
    let hub_shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hub_task = {
        let hub = hub.clone();
        let hub_shutdown = hub_shutdown.clone();
        tokio::spawn(async move { hub.run_until(hub_shutdown).await })
    };

    let worker_client = std::sync::Arc::new(contextdb_server::SyncClient::with_transport(
        runtime.db.clone(),
        broker.client(),
        contextdb_core::TenantId::from("vigil-fabric"),
    ));
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = {
        let client = worker_client.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let config = contextdb_server::work_ledger::WorkerConfig {
                node_id: "live-worker".to_string(),
                advertised_tags: tags(&["backend:burn-cpu"]),
                movement_policy: contextdb_engine::work_ledger::MovementPolicy {
                    auto_propagate: true,
                },
                lease_duration_ms: 5 * 60_000,
                blob_service: None,
                defer_own_submissions_until_deadline: false,
                writes_are_canonical: false,
            };
            let _ = contextdb_server::work_ledger::run_worker_loop(
                &client,
                &config,
                &NoopExecutor,
                std::time::Duration::from_millis(30),
                shutdown,
            )
            .await;
        })
    };

    // Let the clock pass several TTLs (150ms each) while the worker keeps
    // advertising on its 30ms cadence.
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let line = rendered_status_line(&runtime).await;

    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    hub_shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = worker.await;
    let _ = hub_task.await;

    assert!(
        line.contains("live-worker:burn-cpu"),
        "a live worker that keeps re-advertising on its poll cadence must STAY \
         rendered on a consumer edge across several TTLs — it must not age out \
         like a dead one; got: {line}"
    );
}
