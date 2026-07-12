//! Degraded-never-dead fallback (criterion C5): a worker's death mid-lease
//! must never strand the submitter's pipeline. Kill-the-worker is ordinary
//! lease expiry (no bespoke fallback path — Design decision C5): the
//! submitter's own standing worker loop, configured with
//! `defer_own_submissions_until_deadline`, reclaims its own deadline-bearing
//! submission once the deadline has passed and no one else finished it, and
//! executes it locally with a NAMED receipt. The pipeline continues: a
//! subsequent segment detects locally too, no crash, no dead pipeline.
//!
//! RED: `vigil::fabric::DetectorWorkExecutor::execute` and
//! `vigil::offload_policy::render_offload_fallback_receipt` are `todo!()`
//! pending the implementation pass. Modeled on contextdb-server's own
//! `worker_defers_own_submission_tests.rs::worker_defers_then_reclaims_own_submission_after_deadline`.

#![cfg(feature = "fabric")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use contextdb_core::TenantId;
use contextdb_engine::Database;
use contextdb_engine::sync_types::{ConflictPolicies, ConflictPolicy};
use contextdb_engine::work_ledger::{
    BlobHash, InputRef, JobSpec, JobState, MovementPolicy, install_work_ledger_schema, job_state,
    submit_job,
};
use contextdb_server::work_ledger::{
    ClaimOutcome, PollOutcome, WorkerConfig, claim_job, poll_and_execute_once,
};
use contextdb_server::{FabricIdentity, InProcessBroker, SyncClient, SyncServer};

use vigil::DecodedRgbFrame;
use vigil::detector_workclass::{
    BlobRefPlaceholder, DETECTOR_CLASS_TAG, DETECTOR_MODE, DETECTOR_SCHEMA_VERSION,
    DETECTOR_WORK_CLASS, DetectorDetection, DetectorJobBuilder, OrderedF64, WireVideoCodec,
    WireWorkEnvelope,
};
use vigil::fabric::{DetectorWorkExecutor, FabricDetectorBackend};
use vigil::offload_policy::render_offload_fallback_receipt;

const T0: i64 = 1_700_000_000_000;
const MINUTE: i64 = 60_000;
const LEASE: i64 = 5 * MINUTE;
const DEADLINE_OFFSET: i64 = 2 * MINUTE;

async fn within<F: std::future::Future>(fut: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .expect("bounded kill-worker-fallback operation exceeded 30s")
}

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
    let server = Arc::new(SyncServer::with_transport(
        hub_db.clone(),
        broker.server(),
        TenantId::from(tenant),
        ConflictPolicies::uniform(ConflictPolicy::LatestWins),
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

/// A spy local-fallback backend recording every call.
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

/// Submit a `vigil.detector` job carrying only a ledger-carried metadata
/// chunk (no `blob_ref`) — the local-fallback path this test exercises
/// re-decodes from THIS node's own already-decoded frames, never a remote
/// blob, so no blob service is needed for the fallback leg itself.
fn submit_local_detector_job(
    db: &Database,
    job_id: &str,
    submitter: &str,
    deadline_ms: Option<i64>,
) {
    let placeholder_hash = BlobHash::of(job_id.as_bytes());
    let job = DetectorJobBuilder::new(
        wire_envelope(&format!("0192f6a0-0000-7000-8000-{job_id:0>12}")),
        BlobRefPlaceholder::from_blob_hash(&placeholder_hash),
    )
    .codec(WireVideoCodec::H264)
    .fps(15.0)
    .sample_frames(4)
    .confidence_threshold(0.5)
    // This job carries NO blob_ref input at all (see the doc comment above):
    // the local-fallback path never fetches or decodes a frames blob, so
    // there is no real encoded content to hash against — the executor's
    // clip-hash-mismatch guard is only reached when a blob is actually
    // present (see detector_worker.rs), so this placeholder is inert by
    // construction, not a fabricated value standing in for a real one.
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
        blob_service: None,
        defer_own_submissions_until_deadline: true,
    }
}

#[tokio::test]
async fn worker_death_midlease_falls_back_local_with_named_receipt() {
    let broker = InProcessBroker::new();
    let tenant = "vd-fallback-01";
    let (hub_db, shutdown, task) = start_hub(&broker, tenant);

    // The submitter (node A) will also run its own standing worker loop.
    let a_dir = tempfile::tempdir().expect("a dir");
    let a_key = identity_file(&a_dir);
    let a_node = node_id_of(&a_key);
    let a_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&a_db).expect("A ledger schema");
    let a_client =
        SyncClient::with_transport(a_db.clone(), broker.client(), TenantId::from(tenant));

    let deadline_ms = T0 + DEADLINE_OFFSET;
    submit_local_detector_job(&a_db, "job-fallback-01", &a_node, Some(deadline_ms));
    within(a_client.push()).await.expect("A push");

    // The worker (node B): claims the job with a SHORT lease, then "dies" —
    // it never records a result or a failure. Simulated directly via
    // `claim_job` (never followed by execution), the same primitive
    // `poll_and_execute_once` itself uses — no bespoke fallback protocol.
    let b_dir = tempfile::tempdir().expect("b dir");
    let b_key = identity_file(&b_dir);
    let b_node = node_id_of(&b_key);
    let b_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&b_db).expect("B ledger schema");
    let b_client =
        SyncClient::with_transport(b_db.clone(), broker.client(), TenantId::from(tenant));
    within(b_client.pull_default())
        .await
        .expect("B pulls the job");

    let short_lease_deadline_ms = T0 + 1_000; // dies long before job's own deadline_ms
    let claim = within(claim_job(
        &b_client,
        "job-fallback-01",
        1,
        &b_node,
        short_lease_deadline_ms,
        T0 + 10,
    ))
    .await
    .expect("B claims the job");
    assert_eq!(
        claim,
        ClaimOutcome::Won { synced: true },
        "the worker must actually win the claim before it dies mid-lease"
    );
    // Node B never calls record_result/record_failure from here on — this
    // IS the simulated death.

    // Before the job's own deadline and before the lease has expired: node
    // A (deferring its own submissions) must not reclaim yet.
    let too_early = within(poll_and_execute_once(
        &a_client,
        &deferring_config(&a_node),
        &DetectorWorkExecutor::new(
            Arc::new(SpyBackend {
                calls: Mutex::new(0),
            }),
            a_node.clone(),
        ),
        T0 + 500,
    ))
    .await
    .expect("A poll before lease/deadline expiry");
    assert_eq!(
        too_early,
        PollOutcome::NoMatchingWork,
        "node A must not reclaim while node B's lease is still live and the deadline has not passed"
    );
    assert_eq!(
        job_state(&a_db, "job-fallback-01", T0 + 500).expect("state before expiry"),
        JobState::Leased {
            node_id: b_node.clone(),
            attempt: 1,
            lease_deadline_ms: short_lease_deadline_ms,
        }
    );

    // After BOTH the lease has expired AND the job's own deadline has
    // passed: node A must reclaim and execute locally.
    let after = deadline_ms + 1;
    let backend = Arc::new(SpyBackend {
        calls: Mutex::new(0),
    });
    let executor = DetectorWorkExecutor::new(backend.clone(), a_node.clone());
    let outcome = within(poll_and_execute_once(
        &a_client,
        &deferring_config(&a_node),
        &executor,
        after,
    ))
    .await
    .expect("A poll after lease + deadline expiry");
    assert_eq!(
        outcome,
        PollOutcome::Executed {
            job_id: "job-fallback-01".to_string(),
            attempt: 2,
        },
        "once the dead worker's lease AND the job's own deadline have passed, the submitter must \
         reclaim and execute the segment locally — the pipeline must never stay stuck"
    );
    assert_eq!(*backend.calls.lock().expect("calls lock"), 1);
    assert_eq!(
        job_state(&a_db, "job-fallback-01", after + 1).expect("state after fallback"),
        JobState::Done
    );

    // The fallback must carry a NAMED receipt — never a silent recovery.
    let receipt_line = render_offload_fallback_receipt("lease-expired");
    assert_eq!(receipt_line, "offload-fallback=local why=lease-expired");

    // Pipeline continues: a SUBSEQUENT segment must also detect locally,
    // proving the worker loop (and the process) survived the dead peer.
    submit_local_detector_job(&a_db, "job-fallback-02", &a_node, None);
    let continued = within(poll_and_execute_once(
        &a_client,
        &deferring_config(&a_node),
        &executor,
        after + 1,
    ))
    .await
    .expect("A poll for the next segment");
    assert_eq!(
        continued,
        PollOutcome::Executed {
            job_id: "job-fallback-02".to_string(),
            attempt: 1,
        },
        "the pipeline must keep detecting locally after the fallback, not just once"
    );
    assert_eq!(*backend.calls.lock().expect("calls lock"), 2);

    within(a_client.push()).await.expect("A push results");
    let mut params = HashMap::new();
    params.insert(
        "job_id".to_string(),
        contextdb_core::Value::Text("job-fallback-01".to_string()),
    );
    let hub_rows = hub_db
        .execute("SELECT * FROM work_results WHERE job_id = $job_id", &params)
        .expect("scan hub work_results")
        .rows;
    assert_eq!(
        hub_rows.len(),
        1,
        "the fallback result must reach the hub exactly once"
    );

    shutdown.store(true, Ordering::SeqCst);
    let _ = task.await;
}
