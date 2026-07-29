//! Dead-CAPABILITY steal (criterion C5, fix cycle 8): a worker killed mid-lease
//! must not strand the submitter's pipeline for the worker's FULL lease
//! (5 minutes in production) — far past the 5s fallback horizon. The submitter,
//! and ONLY the submitter, reclaims a stranded job by abandoning the dead
//! claimant's attempt once (a) the job's fallback deadline has passed AND
//! (b) the claimant has fallen out of the live capability set (sustained
//! heartbeat absence, NOT mere slowness). That failure row turns the job
//! Pending again under the ledger's existing failure-aware rule, so the
//! submitter's own deferring worker claims and runs it locally with the named
//! `offload-fallback=local why=lease-expired` receipt — no lease mutation, no
//! engine state-machine change.
//!
//! This is the gap `kill_worker_fallback.rs` could not exercise: THAT test uses
//! a SHORT lease so ordinary lease expiry reclaims the job. Here the claimant
//! holds the REAL 5-minute lease and dies; only the capability-liveness steal
//! reclaims it inside the horizon.
//!
//! RED: `offload_policy::should_steal_stranded_claim` and
//! `fabric::try_steal_stranded_claim` are `todo!()` pending the implementation
//! pass. The late-remote-result discard receipt (a result racing an
//! already-resolved local fallback) is asserted in `result_join_authority.rs`
//! and not re-proven here.

#![cfg(feature = "fabric")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use contextdb_core::TenantId;
use contextdb_engine::Database;
use contextdb_engine::work_ledger::{
    BlobHash, InputRef, JobSpec, JobState, MovementPolicy, install_work_ledger_schema, job_state,
    submit_job,
};
use contextdb_server::work_ledger::{
    ClaimOutcome, ExecutionOutput, ExecutionVerdict, PollOutcome, WorkExecutor, WorkerConfig,
    claim_job, poll_and_execute_once,
};
use contextdb_server::{FabricIdentity, InProcessBroker, SyncClient, SyncServer};

use vigil::DecodedRgbFrame;
use vigil::detector_workclass::{
    DETECTOR_CLASS_TAG, DETECTOR_MODE, DETECTOR_SCHEMA_VERSION, DETECTOR_WORK_CLASS,
    DetectorDetection, DetectorJobBuilder, FrameBlobRef, OrderedF64, WireVideoCodec,
    WireWorkEnvelope,
};
use vigil::fabric::{DetectorWorkExecutor, FabricDetectorBackend, try_steal_stranded_claim};
use vigil::offload_policy::{render_offload_fallback_receipt, should_steal_stranded_claim};

const T0: i64 = 1_700_000_000_000;
const MINUTE: i64 = 60_000;
// The REAL production worker lease (fabric.rs: `lease_duration_ms: 5 * 60_000`).
// The claimant holds this full lease and dies — the whole point is that the
// steal reclaims WITHOUT waiting it out.
const LEASE: i64 = 5 * MINUTE;
// The fallback horizon a real offload deadline carries (~fallback_horizon_ms).
const HORIZON: i64 = 5_000;

async fn within<F: std::future::Future>(fut: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .expect("bounded dead-capability-steal operation exceeded 30s")
}

// ---------------------------------------------------------------------------
// Pure decision: horizon AND capability-liveness, together.
// ---------------------------------------------------------------------------

struct DecisionRow {
    name: &'static str,
    rationale: &'static str,
    now: i64,
    deadline: Option<i64>,
    claimant: &'static str,
    live_set: &'static [&'static str],
    expected: bool,
}

#[test]
fn should_steal_stranded_claim_decision_matrix() {
    let rows: &[DecisionRow] = &[
        DecisionRow {
            name: "dead_claimant_past_deadline_is_stolen",
            rationale: "deadline elapsed AND the claimant absent from the live set → steal",
            now: T0 + HORIZON + 1,
            deadline: Some(T0 + HORIZON),
            claimant: "node-dead",
            live_set: &["node-other"],
            expected: true,
        },
        DecisionRow {
            name: "slow_but_alive_claimant_is_never_stolen",
            rationale: "deadline long elapsed, but the claimant is STILL re-advertising \
                        (present in the live set) → never stolen; this is the \
                        anti-lease-cap guard: liveness, not the clock, decides",
            now: T0 + 10 * MINUTE,
            deadline: Some(T0 + HORIZON),
            claimant: "node-slow",
            live_set: &["node-slow", "node-other"],
            expected: false,
        },
        DecisionRow {
            name: "claimant_within_horizon_is_not_stolen",
            rationale: "claimant dead, but the horizon has NOT elapsed yet → do not steal",
            now: T0 + 1,
            deadline: Some(T0 + HORIZON),
            claimant: "node-dead",
            live_set: &[],
            expected: false,
        },
        DecisionRow {
            name: "deadlineless_job_is_never_stolen",
            rationale: "a job with no fallback deadline is not an offload job → never \
                        stolen, however dead the (nonexistent) claimant",
            now: T0 + 10 * MINUTE,
            deadline: None,
            claimant: "node-dead",
            live_set: &[],
            expected: false,
        },
    ];

    let mut failures: Vec<String> = Vec::new();
    for row in rows {
        let live_set: Vec<String> = row.live_set.iter().map(|s| s.to_string()).collect();
        let actual = should_steal_stranded_claim(row.now, row.deadline, row.claimant, &live_set);
        if actual != row.expected {
            failures.push(format!(
                "row `{}` failed: {}\n    inputs: now={} deadline={:?} claimant={:?} live_set={:?}\n    expected={} actual={}",
                row.name, row.rationale, row.now, row.deadline, row.claimant, row.live_set, row.expected, actual
            ));
        }
    }

    if !failures.is_empty() {
        panic!(
            "{} of {} should_steal_stranded_claim decision-matrix rows failed:\n{}",
            failures.len(),
            rows.len(),
            failures.join("\n")
        );
    }
}

// ---------------------------------------------------------------------------
// Integration scaffolding (mirrors kill_worker_fallback.rs).
// ---------------------------------------------------------------------------

fn identity_file(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("fabric-identity.key")
}

fn node_id_of(key: &Path) -> String {
    FabricIdentity::load_or_generate(key)
        .expect("identity")
        .node_id()
}

fn start_hub(
    broker: &InProcessBroker,
    tenant: &str,
) -> (Arc<Database>, Arc<AtomicBool>, tokio::task::JoinHandle<()>) {
    let hub_db = Arc::new(Database::open_memory());
    let server = Arc::new(SyncServer::with_authenticated_transport_for_test(
        hub_db.clone(),
        broker.server_as(&format!("hub-{tenant}")),
        TenantId::from(tenant),
    ));
    let shutdown = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn({
        let server = server.clone();
        let shutdown = shutdown.clone();
        async move { server.run_until(shutdown).await }
    });
    (hub_db, shutdown, task)
}

fn wire_envelope(work_id: &str) -> WireWorkEnvelope {
    WireWorkEnvelope {
        work_id: work_id.to_string(),
        parent_work_id: None,
        contributing_work_ids: vec![],
        stage: "detection".to_string(),
        stream_id: "front-yard".to_string(),
        ordering_stream_epoch: 1,
        ordering_stream_sequence: 1,
        schema_version: DETECTOR_SCHEMA_VERSION,
    }
}

struct SpyBackend {
    calls: Mutex<u32>,
}

impl FabricDetectorBackend for SpyBackend {
    fn backend_tag(&self) -> &str {
        "burn-cpu"
    }

    fn model_sha256(&self) -> &str {
        "spy-model-sha"
    }

    fn detect(
        &self,
        _frames: &[DecodedRgbFrame],
        _clip_sha256: &str,
        _sample_frames: usize,
        _confidence_threshold: f64,
    ) -> Result<Vec<DetectorDetection>, String> {
        *self.calls.lock().expect("calls lock") += 1;
        Ok(vec![DetectorDetection {
            class_name: "person".to_string(),
            confidence: OrderedF64(0.8),
            bbox: "0,0,5,5".to_string(),
            frame_index: 0,
        }])
    }
}

/// A remote worker that DID record a result — used only for the
/// result-before-steal no-op case, where the executor must never run.
struct RemoteEchoExecutor;

impl WorkExecutor for RemoteEchoExecutor {
    fn execute(
        &self,
        _job: &contextdb_engine::work_ledger::JobSnapshot,
        inputs: &contextdb_engine::work_ledger::ExecutionInputs,
        _should_abandon: &dyn Fn() -> bool,
    ) -> ExecutionVerdict {
        let mut all = Vec::new();
        for (_, chunk) in inputs {
            all.extend_from_slice(chunk);
        }
        ExecutionVerdict::Completed(ExecutionOutput {
            output: all,
            receipt: serde_json::json!({ "backend": "remote-double" }),
        })
    }
}

fn submit_local_detector_job(
    db: &Database,
    job_id: &str,
    submitter: &str,
    deadline_ms: Option<i64>,
) {
    let placeholder_hash = BlobHash::of(job_id.as_bytes());
    let job = DetectorJobBuilder::new(
        wire_envelope(&format!("0192f6a0-0000-7000-8000-{job_id:0>12}")),
        FrameBlobRef::from_blob_hash(&placeholder_hash),
    )
    .codec(WireVideoCodec::H264)
    .fps(15.0)
    .sample_frames(4)
    .confidence_threshold(0.5)
    .clip_sha256("a".repeat(64))
    .decoded_frames_sha256("b".repeat(64))
    .model_id("spy-backend".to_string())
    .build();
    let metadata_bytes = serde_json::to_vec(&job).expect("encode metadata bytes");

    let mut builder = JobSpec::builder(job_id, DETECTOR_WORK_CLASS, DETECTOR_MODE, submitter)
        .requirement_tags(vec![DETECTOR_CLASS_TAG.to_string()])
        .input_refs(vec![InputRef::ledger_input()])
        .submitted_at_ms(T0);
    if let Some(deadline_ms) = deadline_ms {
        builder = builder.deadline_ms(Some(deadline_ms));
    }
    submit_job(db, &builder.build(), &[metadata_bytes]).expect("submit detector job");
}

fn deferring_config(node_id: &str) -> WorkerConfig {
    WorkerConfig {
        node_id: node_id.to_string(),
        advertised_tags: vec![DETECTOR_CLASS_TAG.to_string()],
        movement_policy: MovementPolicy {
            auto_propagate: true,
        },
        lease_duration_ms: LEASE,
        blob_store: None,
        defer_own_submissions_until_deadline: true,
        writes_are_canonical: false,
    }
}

fn db_row_count(db: &Database, table: &str, job_id: &str) -> usize {
    let mut params = HashMap::new();
    params.insert(
        "job_id".to_string(),
        contextdb_core::Value::Text(job_id.to_string()),
    );
    db.execute(
        &format!("SELECT * FROM {table} WHERE job_id = $job_id"),
        &params,
    )
    .unwrap_or_else(|err| panic!("scan {table}: {err}"))
    .rows
    .len()
}

/// Have worker B pull, claim the job under the REAL 5-minute lease, then "die"
/// (never record a result or failure). Returns B's node id.
async fn claim_and_die(broker: &InProcessBroker, tenant: &str, job_id: &str) -> String {
    let b_dir = tempfile::tempdir().expect("b dir");
    let b_key = identity_file(&b_dir);
    let b_node = node_id_of(&b_key);
    let b_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&b_db).expect("B ledger schema");
    let b_client = SyncClient::with_authenticated_transport_for_test(
        b_db.clone(),
        broker.client_as(&b_node),
        TenantId::from(tenant),
    );
    within(b_client.pull_default())
        .await
        .expect("B pulls the job");

    let claim = within(claim_job(
        &b_client,
        job_id,
        1,
        &b_node,
        T0 + LEASE, // the REAL 5-minute lease — never expires within this test
        T0 + 10,
    ))
    .await
    .expect("B claims the job");
    assert_eq!(
        claim,
        ClaimOutcome::Won { synced: true },
        "the worker must win the claim under a full lease before it dies"
    );
    b_node
}

// ---------------------------------------------------------------------------
// The S5 shape: a dead worker holding the full lease is stolen back inside the
// horizon and re-executed locally with the named receipt.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dead_worker_steal_reclaims_locally_with_named_receipt() {
    let broker = InProcessBroker::new();
    let tenant = "vd-steal-01";
    let (hub_db, shutdown, task) = start_hub(&broker, tenant);

    let a_dir = tempfile::tempdir().expect("a dir");
    let a_key = identity_file(&a_dir);
    let a_node = node_id_of(&a_key);
    let a_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&a_db).expect("A ledger schema");
    let a_client = SyncClient::with_authenticated_transport_for_test(
        a_db.clone(),
        broker.client_as(&a_node),
        TenantId::from(tenant),
    );

    let deadline_ms = T0 + HORIZON;
    submit_local_detector_job(&a_db, "job-steal-01", &a_node, Some(deadline_ms));
    within(a_client.push()).await.expect("A push");

    let b_node = claim_and_die(&broker, tenant, "job-steal-01").await;
    within(a_client.pull_default())
        .await
        .expect("A pulls B's claim");

    assert_eq!(
        job_state(&a_db, "job-steal-01", T0 + 100).expect("state after claim"),
        JobState::Leased {
            node_id: b_node.clone(),
            attempt: 1,
            lease_deadline_ms: T0 + LEASE,
        },
        "the job is held under B's full 5-minute lease"
    );

    // Within the horizon: no steal even though B is already dead.
    assert!(
        !try_steal_stranded_claim(&a_db, &a_node, "job-steal-01", &[], T0 + 100)
            .expect("steal probe within horizon"),
        "the horizon has not passed — do not steal yet"
    );

    // Past the horizon but B still ALIVE (present in the live capability set):
    // no steal — the anti-lease-cap guard.
    assert!(
        !try_steal_stranded_claim(
            &a_db,
            &a_node,
            "job-steal-01",
            std::slice::from_ref(&b_node),
            deadline_ms + 1,
        )
        .expect("steal probe with live claimant"),
        "a slow-but-alive claimant is never stolen"
    );
    assert_eq!(
        db_row_count(&a_db, "work_failures", "job-steal-01"),
        0,
        "no abandon row may be written while the claimant is alive"
    );

    // Past the horizon AND B has fallen out of the live set: STEAL.
    let steal_at = deadline_ms + 1;
    assert!(
        try_steal_stranded_claim(&a_db, &a_node, "job-steal-01", &[], steal_at)
            .expect("steal a dead claimant"),
        "a dead claimant past the horizon must be stolen"
    );
    assert_eq!(
        job_state(&a_db, "job-steal-01", steal_at).expect("state after steal"),
        JobState::Pending,
        "the abandon row must turn the job Pending for the submitter to reclaim"
    );

    // A's own deferring worker now claims attempt 2 and runs it locally.
    let backend = Arc::new(SpyBackend {
        calls: Mutex::new(0),
    });
    let executor: Arc<dyn WorkExecutor> =
        Arc::new(DetectorWorkExecutor::new(backend.clone(), a_node.clone()));
    let outcome = within(poll_and_execute_once(
        &a_client,
        &deferring_config(&a_node),
        executor,
        steal_at + 1,
    ))
    .await
    .expect("A reclaim poll");
    assert_eq!(
        outcome,
        PollOutcome::Executed {
            job_id: "job-steal-01".to_string(),
            attempt: 2,
        },
        "the submitter must reclaim and execute the stolen job locally as attempt 2"
    );
    assert_eq!(*backend.calls.lock().expect("calls lock"), 1);
    assert_eq!(
        job_state(&a_db, "job-steal-01", steal_at + 2).expect("state after reclaim"),
        JobState::Done
    );

    // The named receipt — never a silent recovery.
    assert_eq!(
        render_offload_fallback_receipt("lease-expired"),
        "offload-fallback=local why=lease-expired"
    );

    // Exactly-once at the ledger: the reclaimed result is the only one.
    within(a_client.push()).await.expect("A push result");
    assert_eq!(
        db_row_count(&hub_db, "work_results", "job-steal-01"),
        1,
        "the reclaimed result must reach the hub exactly once"
    );

    shutdown.store(true, Ordering::SeqCst);
    let _ = task.await;
}

// ---------------------------------------------------------------------------
// The submitter must never steal its OWN in-flight claim. After a steal the
// submitter reclaims locally, so the job becomes Leased by the submitter's own
// node while it runs; a second steal on that live self-claim would abandon the
// in-flight local execution and loop the job to Failed. (A node's own node id
// is never in the REMOTE capability set, so without an explicit guard the pure
// policy would say "steal" here.)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn own_self_reclaimed_claim_is_never_stolen() {
    let broker = InProcessBroker::new();
    let tenant = "vd-steal-04";
    let (_hub_db, shutdown, task) = start_hub(&broker, tenant);

    let a_dir = tempfile::tempdir().expect("a dir");
    let a_node = node_id_of(&identity_file(&a_dir));
    let a_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&a_db).expect("A ledger schema");
    let a_client = SyncClient::with_authenticated_transport_for_test(
        a_db.clone(),
        broker.client_as(&a_node),
        TenantId::from(tenant),
    );

    let deadline_ms = T0 + HORIZON;
    submit_local_detector_job(&a_db, "job-steal-04", &a_node, Some(deadline_ms));
    within(a_client.push()).await.expect("A push");

    // A itself holds the live claim (the local reclaim in flight), well past
    // the deadline, with an empty live REMOTE set.
    let claim = within(claim_job(
        &a_client,
        "job-steal-04",
        1,
        &a_node,
        T0 + LEASE,
        deadline_ms + 1,
    ))
    .await
    .expect("A self-claims");
    assert_eq!(claim, ClaimOutcome::Won { synced: true });

    assert!(
        !try_steal_stranded_claim(&a_db, &a_node, "job-steal-04", &[], deadline_ms + MINUTE)
            .expect("self-claim steal probe"),
        "the submitter must never steal its own in-flight claim"
    );
    assert_eq!(
        db_row_count(&a_db, "work_failures", "job-steal-04"),
        0,
        "no abandon row may be written against the submitter's own live claim"
    );

    shutdown.store(true, Ordering::SeqCst);
    let _ = task.await;
}

// ---------------------------------------------------------------------------
// A foreign node (not the submitter) never steals, even with a dead claimant
// past the horizon.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn foreign_node_never_steals() {
    let broker = InProcessBroker::new();
    let tenant = "vd-steal-02";
    let (_hub_db, shutdown, task) = start_hub(&broker, tenant);

    let a_dir = tempfile::tempdir().expect("a dir");
    let a_node = node_id_of(&identity_file(&a_dir));
    let a_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&a_db).expect("A ledger schema");
    let a_client = SyncClient::with_authenticated_transport_for_test(
        a_db.clone(),
        broker.client_as(&a_node),
        TenantId::from(tenant),
    );

    let deadline_ms = T0 + HORIZON;
    submit_local_detector_job(&a_db, "job-steal-02", &a_node, Some(deadline_ms));
    within(a_client.push()).await.expect("A push");
    let _b_node = claim_and_die(&broker, tenant, "job-steal-02").await;

    // Node C: a different node that did NOT submit the job. It has the full
    // ledger (pulls it) and the claimant is long dead, but it must not steal —
    // the C5 fallback duty belongs to the submitter alone.
    let c_dir = tempfile::tempdir().expect("c dir");
    let c_node = node_id_of(&identity_file(&c_dir));
    let c_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&c_db).expect("C ledger schema");
    let c_client = SyncClient::with_authenticated_transport_for_test(
        c_db.clone(),
        broker.client_as(&c_node),
        TenantId::from(tenant),
    );
    within(c_client.pull_default())
        .await
        .expect("C pulls ledger");

    assert!(
        !try_steal_stranded_claim(&c_db, &c_node, "job-steal-02", &[], deadline_ms + MINUTE)
            .expect("C steal probe"),
        "a non-submitter must never steal another node's stranded job"
    );
    assert_eq!(
        db_row_count(&c_db, "work_failures", "job-steal-02"),
        0,
        "a foreign node must write no abandon row"
    );

    shutdown.store(true, Ordering::SeqCst);
    let _ = task.await;
}

// ---------------------------------------------------------------------------
// A result that lands before the steal makes the steal a no-op: nothing to
// reclaim, no abandon row. (The mirror case — a late result racing an
// already-resolved fallback discards with its receipt — is asserted in
// result_join_authority.rs.)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn result_before_steal_makes_steal_a_noop() {
    let broker = InProcessBroker::new();
    let tenant = "vd-steal-03";
    let (_hub_db, shutdown, task) = start_hub(&broker, tenant);

    let a_dir = tempfile::tempdir().expect("a dir");
    let a_node = node_id_of(&identity_file(&a_dir));
    let a_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&a_db).expect("A ledger schema");
    let a_client = SyncClient::with_authenticated_transport_for_test(
        a_db.clone(),
        broker.client_as(&a_node),
        TenantId::from(tenant),
    );

    let deadline_ms = T0 + HORIZON;
    submit_local_detector_job(&a_db, "job-steal-03", &a_node, Some(deadline_ms));
    within(a_client.push()).await.expect("A push");

    // Worker B pulls, claims, and this time DOES complete + record a result.
    let b_dir = tempfile::tempdir().expect("b dir");
    let b_node = node_id_of(&identity_file(&b_dir));
    let b_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&b_db).expect("B ledger schema");
    let b_client = SyncClient::with_authenticated_transport_for_test(
        b_db.clone(),
        broker.client_as(&b_node),
        TenantId::from(tenant),
    );
    within(b_client.pull_default())
        .await
        .expect("B pulls the job");
    let executor: Arc<dyn WorkExecutor> = Arc::new(RemoteEchoExecutor);
    let done = within(poll_and_execute_once(
        &b_client,
        &WorkerConfig {
            node_id: b_node.clone(),
            advertised_tags: vec![DETECTOR_CLASS_TAG.to_string()],
            movement_policy: MovementPolicy {
                auto_propagate: true,
            },
            lease_duration_ms: LEASE,
            blob_store: None,
            // B is a FOREIGN worker to this job: it never defers it.
            defer_own_submissions_until_deadline: true,
            writes_are_canonical: false,
        },
        executor,
        T0 + 10,
    ))
    .await
    .expect("B executes");
    assert_eq!(
        done,
        PollOutcome::Executed {
            job_id: "job-steal-03".to_string(),
            attempt: 1,
        },
        "B completes the job before any steal"
    );
    within(a_client.pull_default())
        .await
        .expect("A pulls the result");
    assert_eq!(
        job_state(&a_db, "job-steal-03", deadline_ms + MINUTE).expect("state"),
        JobState::Done,
        "the job is Done once B's result lands"
    );

    // Even far past the horizon with B absent from the live set, there is
    // nothing to steal — the job is Done, not Leased.
    assert!(
        !try_steal_stranded_claim(&a_db, &a_node, "job-steal-03", &[], deadline_ms + MINUTE)
            .expect("steal probe on a done job"),
        "a completed job is never stolen"
    );
    assert_eq!(
        db_row_count(&a_db, "work_failures", "job-steal-03"),
        0,
        "no abandon row may be written against a completed job"
    );

    shutdown.store(true, Ordering::SeqCst);
    let _ = task.await;
}
