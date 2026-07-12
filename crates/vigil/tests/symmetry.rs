//! Node symmetry (criterion C8): any enrolled node may both submit its own
//! detector jobs (its own cameras, decoded locally) and claim + execute
//! others' — proven at test level by ONE node doing both in a single run.
//! "Main" is a UX role, not a structural distinction (the symmetry
//! paragraph in the seed).
//!
//! RED: `vigil::fabric::DetectorWorkExecutor::execute` is `todo!()` pending
//! the implementation pass.

#![cfg(feature = "fabric")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use contextdb_core::TenantId;
use contextdb_engine::Database;
use contextdb_engine::sync_types::{ConflictPolicies, ConflictPolicy};
use contextdb_engine::work_ledger::{
    BlobHash, InputRef, JobSpec, JobState, MovementPolicy, advertise_capability,
    install_work_ledger_schema, job_state, submit_job,
};
use contextdb_server::work_ledger::{PollOutcome, WorkerConfig, poll_and_execute_once};
use contextdb_server::{FabricIdentity, InProcessBroker, SyncClient, SyncServer};

use vigil::DecodedRgbFrame;
use vigil::detector_workclass::{
    BlobRefPlaceholder, DETECTOR_CLASS_TAG, DETECTOR_MODE, DETECTOR_SCHEMA_VERSION,
    DETECTOR_WORK_CLASS, DetectorDetection, DetectorJobBuilder, OrderedF64, WireVideoCodec,
    WireWorkEnvelope,
};
use vigil::fabric::{DetectorWorkExecutor, FabricDetectorBackend};

const T0: i64 = 1_700_000_000_000;
const MINUTE: i64 = 60_000;
const LEASE: i64 = 5 * MINUTE;

async fn within<F: std::future::Future>(fut: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .expect("bounded symmetry operation exceeded 30s")
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

struct SpyBackend {
    calls: Mutex<Vec<String>>,
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
        clip_sha256: &str,
        _sample_frames: usize,
        _confidence_threshold: f64,
    ) -> Result<Vec<DetectorDetection>, String> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(clip_sha256.to_string());
        Ok(vec![DetectorDetection {
            class_name: "person".to_string(),
            confidence: OrderedF64(0.75),
            bbox: "2,2,6,6".to_string(),
            frame_index: 0,
        }])
    }
}

/// A `vigil.detector` job carrying only a ledger-carried metadata chunk (no
/// `blob_ref`) — the claim/execute path this test pins does not depend on
/// blob resolution, only on the ledger's claim/lease surface and vigil's
/// own `WorkExecutor`.
fn submit_local_detector_job(db: &Database, job_id: &str, submitter: &str, clip_sha256: &str) {
    let placeholder_hash = BlobHash::of(job_id.as_bytes());
    let job = DetectorJobBuilder::new(
        wire_envelope(&format!("0192f6a0-0000-7000-8000-{job_id:0>12}")),
        BlobRefPlaceholder::from_blob_hash(&placeholder_hash),
    )
    .codec(WireVideoCodec::H264)
    .fps(15.0)
    .sample_frames(4)
    .confidence_threshold(0.5)
    // This job carries NO blob_ref input (see the doc comment above): the
    // claim/execute path under test never fetches or decodes a frames blob,
    // so there is no real encoded content for `clip_sha256` to name — the
    // executor's clip-hash-mismatch guard only activates when a blob is
    // actually present (see detector_worker.rs), so the caller-supplied
    // value here is inert by construction, not a fabricated stand-in.
    .clip_sha256(clip_sha256.to_string())
    .decoded_frames_sha256("b".repeat(64))
    .model_id("spy-backend".to_string())
    .build();
    let metadata_bytes = serde_json::to_vec(&job).expect("encode metadata bytes");

    let spec = JobSpec::builder(job_id, DETECTOR_WORK_CLASS, DETECTOR_MODE, submitter)
        .requirement_tags(vec![DETECTOR_CLASS_TAG.to_string()])
        .input_refs(vec![InputRef::ledger_input()])
        .submitted_at_ms(T0)
        .build();
    submit_job(db, &spec, &[metadata_bytes]).expect("submit detector job");
}

#[tokio::test]
async fn one_node_submits_and_claims_in_one_run() {
    let broker = InProcessBroker::new();
    let tenant = "vd-symmetry-01";
    let (hub_db, shutdown, task) = start_hub(&broker, tenant);

    // Node A: the node under test — it owns a camera (submits its own job)
    // AND runs a worker loop (claims another node's job), in the SAME run.
    let a_dir = tempfile::tempdir().expect("a dir");
    let a_key = identity_file(&a_dir);
    let a_node = node_id_of(&a_key);
    let a_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&a_db).expect("A ledger schema");
    let a_client =
        SyncClient::with_transport(a_db.clone(), broker.client(), TenantId::from(tenant));
    let tags = vec![
        DETECTOR_CLASS_TAG.to_string(),
        "backend:burn-cpu".to_string(),
    ];
    advertise_capability(&a_db, &a_node, "vigil-detector-burn-cpu", &tags, T0)
        .expect("A advertises");

    // Node B: a second enrolled node, owns a DIFFERENT camera, submits its
    // own job into the SAME shared queue.
    let b_dir = tempfile::tempdir().expect("b dir");
    let b_key = identity_file(&b_dir);
    let b_node = node_id_of(&b_key);
    let b_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&b_db).expect("B ledger schema");
    let b_client =
        SyncClient::with_transport(b_db.clone(), broker.client(), TenantId::from(tenant));

    submit_local_detector_job(
        &b_db,
        "job-symmetry-remote",
        &b_node,
        "b".repeat(64).as_str(),
    );
    within(b_client.push()).await.expect("B push");

    // Node A also submits its OWN job (its own camera, decoded locally).
    submit_local_detector_job(&a_db, "job-symmetry-own", &a_node, "a".repeat(64).as_str());
    within(a_client.push()).await.expect("A push");

    let backend = Arc::new(SpyBackend {
        calls: Mutex::new(Vec::new()),
    });
    let executor = DetectorWorkExecutor::new(backend.clone(), a_node.clone());
    let config = WorkerConfig {
        node_id: a_node.clone(),
        advertised_tags: tags,
        movement_policy: MovementPolicy {
            auto_propagate: true,
        },
        lease_duration_ms: LEASE,
        blob_service: None,
        defer_own_submissions_until_deadline: false,
    };

    // ONE poll pass claims and executes ONE matching job (priority/deadline/
    // age ordered) — run it twice: node A must claim ITS OWN submission and
    // node B's submission across the two passes, in the SAME run.
    let mut executed_job_ids = Vec::new();
    for step in 0..2u32 {
        let outcome = within(poll_and_execute_once(
            &a_client,
            &config,
            &executor,
            T0 + 1 + step as i64,
        ))
        .await
        .expect("A poll must not error");
        match outcome {
            PollOutcome::Executed { job_id, .. } => executed_job_ids.push(job_id),
            other => panic!("expected A to execute a matching job on pass {step}: {other:?}"),
        }
    }
    executed_job_ids.sort();
    assert_eq!(
        executed_job_ids,
        vec![
            "job-symmetry-own".to_string(),
            "job-symmetry-remote".to_string()
        ],
        "node A must claim+execute BOTH its own submission and node B's, in one run \
         (criterion C8 symmetry)"
    );
    assert_eq!(
        backend.calls.lock().expect("calls lock").len(),
        2,
        "the local detector must actually run for both jobs, not just one"
    );

    assert_eq!(
        job_state(&a_db, "job-symmetry-own", T0 + 10).expect("state"),
        JobState::Done
    );
    assert_eq!(
        job_state(&a_db, "job-symmetry-remote", T0 + 10).expect("state"),
        JobState::Done
    );

    within(a_client.push()).await.expect("A push results");
    let mut params = std::collections::HashMap::new();
    params.insert(
        "job_id".to_string(),
        contextdb_core::Value::Text("job-symmetry-remote".to_string()),
    );
    let hub_rows = hub_db
        .execute(
            "SELECT executor_node_id FROM work_results WHERE job_id = $job_id",
            &params,
        )
        .expect("scan hub work_results")
        .rows;
    assert_eq!(hub_rows.len(), 1);
    let contextdb_core::Value::Text(executor_node_id) = &hub_rows[0][0] else {
        panic!("executor_node_id must be TEXT");
    };
    assert_eq!(
        executor_node_id, &a_node,
        "node A's own node id must be the executor of B's submitted job on the hub"
    );

    shutdown.store(true, Ordering::SeqCst);
    let _ = task.await;
}
