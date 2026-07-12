//! Worker-side work-class execution (criterion C3): the same vigil binary
//! advertises detector capability into the ledger as tags, claims a
//! submitted `vigil.detector` job via the ledger's claim/lease surface,
//! materializes the frames via the blob resolver, runs its local detector,
//! and records the result + receipt exactly once. The worker loop is
//! contextdb's `run_worker_loop`/`WorkExecutor` (via `poll_and_execute_once`
//! here, for one deterministic pass) — never a vigil-private protocol
//! (architecture Section 3 / Rule 5 / §10 rule 5).
//!
//! RED: `vigil::fabric::DetectorWorkExecutor::execute` is `todo!()` pending
//! the implementation pass; this fixture is the contract it must satisfy.
//! Modeled on contextdb-server's own
//! `distributed_blob_job_worker_tests.rs::blob_ref_job_resolves_and_executes_via_worker_loop`.

#![cfg(feature = "fabric")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use contextdb_core::{TenantId, Value};
use contextdb_engine::Database;
use contextdb_engine::sync_types::{ConflictPolicies, ConflictPolicy};
use contextdb_engine::work_ledger::{
    BlobHash, InputRef, JobSpec, JobState, MovementPolicy, advertise_capability, advertised_tags,
    install_work_ledger_schema, job_result, job_state, record_result, submit_job,
};
use contextdb_server::blob_resolver::BlobService;
use contextdb_server::transport::iroh::IrohServer;
use contextdb_server::work_ledger::{PollOutcome, WorkerConfig, poll_and_execute_once};
use contextdb_server::{FabricIdentity, InProcessBroker, SyncClient, SyncServer};
use sha2::{Digest, Sha256};

use vigil::DecodedRgbFrame;
use vigil::detector_workclass::{
    DETECTOR_CLASS_TAG, DETECTOR_MODE, DETECTOR_SCHEMA_VERSION, DETECTOR_WORK_CLASS,
    DetectorDetection, DetectorJobBuilder, FrameBlobRef, OrderedF64, WireVideoCodec,
    WireWorkEnvelope, encode_length_framed_units,
};
use vigil::fabric::{DetectorWorkExecutor, FabricDetectorBackend, detector_capability_id};

const T0: i64 = 1_700_000_000_000;
const MINUTE: i64 = 60_000;
const LEASE: i64 = 5 * MINUTE;

async fn within<F: std::future::Future>(fut: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .expect("bounded detector-worker operation exceeded 30s")
}

fn identity_file(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("fabric-identity.key")
}

fn bind_spec(identity: &Path) -> String {
    format!("iroh:?identity={}", identity.display())
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

fn count_rows(db: &Database, table: &str) -> usize {
    db.execute(&format!("SELECT * FROM {table}"), &HashMap::new())
        .unwrap_or_else(|err| panic!("scan {table}: {err}"))
        .rows
        .len()
}

/// A minimal synthetic H.264 annex-B stream produced by the OpenH264
/// encoder at runtime — content-faithful input, no fixture bytes (same
/// pattern as `decode_backend_contract.rs::encoded_h264_units`).
fn encoded_h264_units(frames: usize) -> Vec<Vec<u8>> {
    use openh264::encoder::Encoder;
    use openh264::formats::{RgbSliceU8, YUVBuffer};

    let width = 32usize;
    let height = 32usize;
    let mut encoder = Encoder::new().expect("create OpenH264 encoder");
    let mut units = Vec::new();
    for index in 0..frames {
        let mut rgb = vec![0u8; width * height * 3];
        for (pixel, chunk) in rgb.chunks_exact_mut(3).enumerate() {
            let x = (pixel % width) as u8;
            chunk[0] = x.wrapping_add(index as u8 * 8);
            chunk[1] = (pixel / width) as u8;
            chunk[2] = 128;
        }
        let yuv = YUVBuffer::from_rgb_source(RgbSliceU8::new(&rgb, (width, height)));
        let bitstream = encoder.encode(&yuv).expect("encode synthetic frame");
        let bytes = bitstream.to_vec();
        if !bytes.is_empty() {
            units.push(bytes);
        }
    }
    assert!(!units.is_empty(), "encoder must produce access units");
    units
}

/// The REAL sha256 over a set of encoded (compressed) units, computed the
/// same way the production path computes it (`runtime.rs::encoded_clip_sha256`
/// — hash each unit's raw bytes in order): the fixture's clip_sha256 must be
/// content-faithful so the executor's clip-hash-mismatch guard is exercised
/// honestly rather than trivially skipped.
fn real_encoded_clip_sha256(units: &[Vec<u8>]) -> String {
    let mut hasher = Sha256::new();
    for unit in units {
        hasher.update(unit);
    }
    format!("{:x}", hasher.finalize())
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

/// A spy backend recording every call and returning a fixed detection —
/// content-faithful (it records the REAL decoded frame count) never
/// fixture-keyed.
struct SpyBackend {
    tag: String,
    model_sha256: String,
    calls: Mutex<Vec<usize>>,
}

impl FabricDetectorBackend for SpyBackend {
    fn backend_tag(&self) -> &str {
        &self.tag
    }

    fn model_sha256(&self) -> &str {
        &self.model_sha256
    }

    fn detect(
        &self,
        frames: &[DecodedRgbFrame],
        _clip_sha256: &str,
        _sample_frames: usize,
        _confidence_threshold: f64,
    ) -> Result<Vec<DetectorDetection>, String> {
        self.calls.lock().expect("calls lock").push(frames.len());
        Ok(vec![DetectorDetection {
            class_name: "person".to_string(),
            confidence: OrderedF64(0.87),
            bbox: "1,1,10,10".to_string(),
            frame_index: 0,
        }])
    }
}

/// Submit a `vigil.detector` job. `blob_hash` is `None` for the adversarial
/// arm (an unsupported `schema_version` must fail BEFORE any blob
/// resolution is attempted — so that arm never needs the frames blob at
/// all).
fn submit_detector_job(
    db: &Database,
    job_id: &str,
    submitter: &str,
    blob_hash: Option<&BlobHash>,
    schema_version: u32,
    clip_sha256: &str,
) {
    let placeholder_hash = blob_hash
        .cloned()
        .unwrap_or_else(|| BlobHash::of(b"unused"));
    let job = DetectorJobBuilder::new(
        wire_envelope(&format!("0192f6a0-0000-7000-8000-{job_id:0>12}")),
        FrameBlobRef::from_blob_hash(&placeholder_hash),
    )
    .codec(WireVideoCodec::H264)
    .fps(15.0)
    .sample_frames(4)
    .confidence_threshold(0.5)
    .clip_sha256(clip_sha256.to_string())
    .decoded_frames_sha256("b".repeat(64))
    .model_id("spy-backend".to_string())
    .build();
    let mut metadata = serde_json::to_value(&job).expect("serialize DetectorJob metadata");
    metadata["schema_version"] = serde_json::json!(schema_version);
    let metadata_bytes = serde_json::to_vec(&metadata).expect("encode metadata bytes");

    let mut input_refs = vec![InputRef::ledger_input()];
    if let Some(hash) = blob_hash {
        input_refs.push(InputRef::blob_ref(hash.clone()));
    }

    let spec = JobSpec::builder(job_id, DETECTOR_WORK_CLASS, DETECTOR_MODE, submitter)
        .requirement_tags(vec![DETECTOR_CLASS_TAG.to_string()])
        .input_refs(input_refs)
        .submitted_at_ms(T0)
        .build();
    submit_job(db, &spec, &[metadata_bytes]).expect("submit detector job");
}

#[tokio::test]
async fn worker_advertises_truthful_backend_claims_materializes_runs_records_once() {
    let broker = InProcessBroker::new();
    let tenant = "vd-worker-01";
    let (hub_db, shutdown, task) = start_hub(&broker, tenant);

    // Submitter (holder): has the encoded frames, submits the job.
    let holder_dir = tempfile::tempdir().expect("holder dir");
    let holder_key = identity_file(&holder_dir);
    let holder_node = node_id_of(&holder_key);
    let encoded_units = encoded_h264_units(6);
    let framed = encode_length_framed_units(&encoded_units);
    let hash = BlobHash::of(&framed);

    let holder_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&holder_db).expect("holder ledger schema");
    let holder_client = Arc::new(SyncClient::with_transport(
        holder_db.clone(),
        broker.client(),
        TenantId::from(tenant),
    ));
    let holder_blob_service = Arc::new(BlobService::new(
        holder_db.clone(),
        MovementPolicy {
            auto_propagate: true,
        },
        holder_key.clone(),
    ));
    holder_blob_service.set_test_clock(T0);
    let refresh_client = holder_client.clone();
    holder_blob_service.set_claim_refresh(Some(Arc::new(move || {
        let refresh_client = refresh_client.clone();
        Box::pin(async move {
            let _ = refresh_client.pull_default().await;
        })
    })));
    assert_eq!(
        holder_blob_service.ingest_bytes(&framed).expect("ingest"),
        hash
    );
    let holder_endpoint = within(IrohServer::bind(&bind_spec(&holder_key)))
        .await
        .expect("bind holder endpoint");
    holder_blob_service.serve_on(&holder_endpoint);

    let real_clip_sha256 = real_encoded_clip_sha256(&encoded_units);
    submit_detector_job(
        &holder_db,
        "job-detect-01",
        &holder_node,
        Some(&hash),
        DETECTOR_SCHEMA_VERSION,
        &real_clip_sha256,
    );
    within(holder_client.push()).await.expect("holder push");

    // Worker: a different node, advertises its truthful backend, claims and
    // executes via vigil's own WorkExecutor + a spy detector backend.
    let worker_dir = tempfile::tempdir().expect("worker dir");
    let worker_key = identity_file(&worker_dir);
    let worker_node = node_id_of(&worker_key);

    let worker_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&worker_db).expect("worker ledger schema");
    let worker_client =
        SyncClient::with_transport(worker_db.clone(), broker.client(), TenantId::from(tenant));
    let worker_blob_service = Arc::new(BlobService::new(
        worker_db.clone(),
        MovementPolicy {
            auto_propagate: true,
        },
        worker_key,
    ));
    worker_blob_service.set_test_clock(T0);

    let backend_tag = "burn-cpu";
    let capability_id = detector_capability_id(backend_tag);
    assert_eq!(capability_id, "vigil-detector-burn-cpu");
    let tags = vec![
        DETECTOR_CLASS_TAG.to_string(),
        format!("backend:{backend_tag}"),
    ];
    advertise_capability(&worker_db, &worker_node, &capability_id, &tags, T0)
        .expect("advertise capability");
    assert_eq!(
        advertised_tags(&worker_db, &worker_node).expect("advertised tags"),
        tags,
        "the worker must advertise its truthful class + backend tags"
    );

    let backend = Arc::new(SpyBackend {
        tag: backend_tag.to_string(),
        model_sha256: "spy-model-sha".to_string(),
        calls: Mutex::new(Vec::new()),
    });
    let executor = DetectorWorkExecutor::new(backend.clone(), worker_node.clone());

    let config = WorkerConfig {
        node_id: worker_node.clone(),
        advertised_tags: tags.clone(),
        movement_policy: MovementPolicy {
            auto_propagate: true,
        },
        lease_duration_ms: LEASE,
        blob_service: Some(worker_blob_service),
        defer_own_submissions_until_deadline: false,
    };

    let outcome = within(poll_and_execute_once(
        &worker_client,
        &config,
        &executor,
        T0 + 1,
    ))
    .await
    .expect("poll must not error");
    assert_eq!(
        outcome,
        PollOutcome::Executed {
            job_id: "job-detect-01".to_string(),
            attempt: 1
        },
        "the worker must resolve the blob_ref frames, decode them, run its detector, and record \
         a result — not fail at materialization"
    );
    assert_eq!(
        *backend.calls.lock().expect("calls lock"),
        vec![6],
        "the spy backend must be called exactly once, over the REAL decoded frame count"
    );

    assert_eq!(
        job_state(&worker_db, "job-detect-01", T0 + 2).expect("state"),
        JobState::Done
    );
    let result = job_result(&worker_db, "job-detect-01")
        .expect("read result")
        .expect("result row");
    let receipt = &result.receipt;
    assert_eq!(
        receipt.get("executor_node_id"),
        Some(&serde_json::json!(worker_node)),
        "receipt must carry executor_node_id: {receipt:?}"
    );
    assert_eq!(
        receipt.get("backend"),
        Some(&serde_json::json!(backend_tag)),
        "receipt must carry the truthful backend: {receipt:?}"
    );
    assert!(
        receipt.get("wall_clock_ms").is_some(),
        "receipt must carry wall_clock_ms: {receipt:?}"
    );
    assert!(
        receipt.get("detections_count").is_some(),
        "receipt must carry detections_count: {receipt:?}"
    );
    let wire_result: vigil::detector_workclass::DetectorResult =
        serde_json::from_slice(&result.output)
            .expect("output must be the DetectorResult wire payload");
    assert_eq!(wire_result.detector_backend, backend_tag);
    assert_eq!(wire_result.detections.len(), 1);
    assert_eq!(
        wire_result.clip_sha256, real_clip_sha256,
        "the result must carry the same REAL clip hash the job was submitted with, proving the \
         executor's integrity check accepted the matching content"
    );

    // Exactly-once: a second identical record is a no-op (row count stays 1).
    record_result(
        &worker_db,
        "job-detect-01",
        1,
        &worker_node,
        &result.output,
        result.receipt.clone(),
        T0 + 3,
    )
    .expect("idempotent re-record must not error");
    assert_eq!(
        count_rows(&worker_db, "work_results"),
        1,
        "a duplicate identical record must never double-write"
    );

    // Adversarial arm: an unknown schema_version fails the job typed, never
    // panics, and never touches blob resolution.
    // This arm carries NO blob (`blob_hash: None`) — the unsupported
    // schema_version must fail before the frames blob is ever touched, so
    // clip_sha256 here names no real content and is never hashed against
    // anything; the fixed placeholder is honest (there is nothing to fake).
    submit_detector_job(
        &worker_db,
        "job-detect-unsupported",
        &worker_node,
        None,
        DETECTOR_SCHEMA_VERSION + 1,
        &"a".repeat(64),
    );
    let outcome = within(poll_and_execute_once(
        &worker_client,
        &config,
        &executor,
        T0 + 4,
    ))
    .await
    .expect("poll must not error even on an unsupported schema_version");
    assert_eq!(
        outcome,
        PollOutcome::Failed {
            job_id: "job-detect-unsupported".to_string(),
            attempt: 1
        },
        "an unsupported schema_version must fail the job typed, never panic"
    );
    let mut params = HashMap::new();
    params.insert(
        "job_id".to_string(),
        Value::Text("job-detect-unsupported".to_string()),
    );
    let failures = worker_db
        .execute(
            "SELECT error FROM work_failures WHERE job_id = $job_id",
            &params,
        )
        .expect("scan work_failures")
        .rows;
    assert_eq!(failures.len(), 1);
    let error_idx = 0usize;
    let Value::Text(error_text) = &failures[0][error_idx] else {
        panic!("work_failures.error must be TEXT");
    };
    assert!(
        error_text.contains("unsupported vigil.detector payload schema_version"),
        "failure text must name the fix: {error_text}"
    );

    within(worker_client.push())
        .await
        .expect("worker push result");
    assert_eq!(
        count_rows(&hub_db, "work_results"),
        1,
        "exactly one result row on the hub"
    );

    shutdown.store(true, Ordering::SeqCst);
    let _ = task.await;
    holder_endpoint.close().await;
}
