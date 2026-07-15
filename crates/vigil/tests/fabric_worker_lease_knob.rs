//! C10 config-knob RED (fix cycle 9): the worker lease duration is hardcoded
//! at `fabric.rs:800` (`lease_duration_ms: 5 * 60_000`) inside
//! `FabricRuntime::spawn_worker_loop` — a real, `pub fn` production
//! construction path (unlike `fabric_bring_up`/`FabricBundle`, which are
//! `pub(crate)` and unreachable from an external test crate; see the
//! `fabric_fallback_horizon_ms` knob's LIGHTER coverage in
//! `fabric_config_defaults.rs` for that one).
//!
//! This harness drives the REAL production path in-process — no subprocess,
//! no real detector model — exactly like `capability_delivery.rs`'s
//! `SpyBackend` + `FabricRuntime::start` + `spawn_worker_loop` idiom, plus
//! `kill_worker_fallback.rs`'s ledger-input-only detector job shape (proven
//! to execute against `DetectorWorkExecutor` with no real blob/video). A job
//! is submitted, the in-process worker claims it, and the claim's own
//! `work_claims.lease_deadline - work_claims.claimed_at` (read back via raw
//! SQL against the hub's ledger, matching `capability_delivery.rs`'s
//! raw-SQL-against-hub_db style) is the effective lease duration the worker
//! actually used — no wall-clock jitter, since both columns are stamped from
//! the SAME `now_ms` snapshot inside a single `poll_and_execute_once` call.
//!
//! Two arms: the default (nothing configured) pins today's hardcoded
//! behavior byte-identical — PASSES. The explicit-value arm sets
//! `VIGIL_FABRIC_WORKER_LEASE_MS`, the approved env-var name for this knob
//! (`fabric_worker_lease_ms` on the config surface, see
//! `fabric_config_defaults.rs`) — FAILS today, because `spawn_worker_loop`
//! reads nothing from it and the observed lease stays the hardcoded 300000ms
//! regardless of what is configured.

#![cfg(feature = "fabric")]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use contextdb_core::{TenantId, Value};
use contextdb_engine::Database;
use contextdb_engine::sync_types::{ConflictPolicies, ConflictPolicy};
use contextdb_engine::work_ledger::{
    BlobHash, InputRef, JobSpec, install_work_ledger_schema, submit_job,
};
use contextdb_server::{PeerEndpoint, SyncClient, SyncServer, peer_bind_spec};

use vigil::DecodedRgbFrame;
use vigil::detector_workclass::{
    DETECTOR_CLASS_TAG, DETECTOR_MODE, DETECTOR_SCHEMA_VERSION, DETECTOR_WORK_CLASS,
    DetectorDetection, DetectorJobBuilder, FrameBlobRef, OrderedF64, WireVideoCodec,
    WireWorkEnvelope,
};
use vigil::fabric::{FabricDetectorBackend, FabricRuntime};

/// `fabric.rs`'s own `FABRIC_TENANT` const is private; hand-assembled hubs
/// in tests use the same literal (precedent: `capability_delivery.rs`).
const FABRIC_TENANT: &str = "vigil-fabric";
/// Today's hardcoded literal at `fabric.rs:800`.
const HARDCODED_DEFAULT_LEASE_MS: i64 = 5 * 60_000;
const LEASE_ENV_VAR: &str = "VIGIL_FABRIC_WORKER_LEASE_MS";
const EXPLICIT_LEASE_MS: i64 = 2_000;

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn within<F: std::future::Future>(fut: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .expect("bounded fabric-worker-lease-knob operation exceeded 30s")
}

struct EnvVarGuard {
    name: &'static str,
}

impl EnvVarGuard {
    fn set(name: &'static str, value: &str) -> Self {
        // SAFETY: serialized by ENV_LOCK, held for the whole test.
        unsafe {
            std::env::set_var(name, value);
        }
        Self { name }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: serialized by ENV_LOCK, held for the whole test.
        unsafe {
            std::env::remove_var(self.name);
        }
    }
}

struct SpyBackend {
    tag: &'static str,
}

impl FabricDetectorBackend for SpyBackend {
    fn backend_tag(&self) -> &str {
        self.tag
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
        Ok(vec![DetectorDetection {
            class_name: "person".to_string(),
            confidence: OrderedF64(0.9),
            bbox: "1,1,2,2".to_string(),
            frame_index: 0,
        }])
    }
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

fn wall_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis() as i64
}

/// A `ledger_input`-only detector job — no `blob_ref`, so no blob service is
/// needed (precedent: `kill_worker_fallback.rs::submit_local_detector_job`,
/// proven to execute against the real `DetectorWorkExecutor`).
fn submit_claimable_job(db: &Database, job_id: &str, submitter: &str) {
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

    let spec = JobSpec::builder(job_id, DETECTOR_WORK_CLASS, DETECTOR_MODE, submitter)
        .requirement_tags(vec![DETECTOR_CLASS_TAG.to_string()])
        .input_refs(vec![InputRef::ledger_input()])
        .submitted_at_ms(wall_now_ms())
        .build();
    submit_job(db, &spec, &[metadata_bytes]).expect("submit detector job");
}

/// The claim's own `(lease_deadline, claimed_at)`, straight off `work_claims`
/// — a permanent, never-mutated row, so this stays readable even after the
/// job completes (unlike the computed `job_state`, which reports `Done` the
/// instant a result lands, losing the lease breadcrumb).
fn claim_lease_span(db: &Database, job_id: &str) -> Option<(i64, i64)> {
    let mut params = HashMap::new();
    params.insert("job_id".to_string(), Value::Text(job_id.to_string()));
    let result = db
        .execute(
            "SELECT lease_deadline, claimed_at FROM work_claims WHERE job_id = $job_id",
            &params,
        )
        .ok()?;
    let deadline_idx = result.columns.iter().position(|c| c == "lease_deadline")?;
    let claimed_idx = result.columns.iter().position(|c| c == "claimed_at")?;
    let row = result.rows.first()?;
    match (&row[deadline_idx], &row[claimed_idx]) {
        (Value::Timestamp(deadline), Value::Timestamp(claimed)) => Some((*deadline, *claimed)),
        _ => None,
    }
}

async fn wait_for_claim_lease_span(db: &Database, job_id: &str) -> (i64, i64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(span) = claim_lease_span(db, job_id) {
            return span;
        }
        assert!(
            Instant::now() < deadline,
            "the in-process worker never claimed {job_id} within 20s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Bind a real hub endpoint + `SyncServer` over it, and return
/// `(hub_db, ticket, shutdown, task)`. Caller must keep `hub_dir` alive for
/// the endpoint's lifetime (returned so it is not dropped early).
async fn bring_up_hub() -> (
    tempfile::TempDir,
    Arc<Database>,
    String,
    Arc<AtomicBool>,
    tokio::task::JoinHandle<()>,
) {
    let hub_dir = tempfile::tempdir().expect("hub identity dir");
    let hub_identity_path = hub_dir.path().join("fabric-identity.key");
    let bind_spec = peer_bind_spec(&hub_identity_path);
    let hub_endpoint = within(PeerEndpoint::bind(&bind_spec))
        .await
        .expect("bind hub endpoint");
    let ticket = hub_endpoint.ticket();
    let hub_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&hub_db).expect("install ledger schema on hub");
    let server = Arc::new(SyncServer::with_transport(
        hub_db.clone(),
        hub_endpoint.transport(),
        TenantId::from(FABRIC_TENANT),
        ConflictPolicies::uniform(ConflictPolicy::LatestWins),
    ));
    let shutdown = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn({
        let server = server.clone();
        let shutdown = shutdown.clone();
        async move { server.run_until(shutdown).await }
    });
    (hub_dir, hub_db, ticket, shutdown, task)
}

#[tokio::test]
async fn worker_lease_defaults_to_five_minutes_when_unset() {
    let _env_guard = ENV_LOCK.lock().await;

    let (_hub_dir, hub_db, ticket, hub_shutdown, hub_task) = bring_up_hub().await;

    let submitter_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&submitter_db).expect("submitter ledger schema");
    let submitter_client =
        SyncClient::new(submitter_db.clone(), &ticket, TenantId::from(FABRIC_TENANT));
    submit_claimable_job(&submitter_db, "job-lease-default", "test-submitter");
    within(submitter_client.push())
        .await
        .expect("submitter push");

    let worker_dir = tempfile::tempdir().expect("worker data dir");
    let worker_runtime = within(FabricRuntime::start(
        worker_dir.path(),
        Some(&ticket),
        false,
    ))
    .await
    .expect("worker must enroll against a valid ticket");
    let backend = Arc::new(SpyBackend { tag: "burn-cpu" });
    let worker_shutdown = Arc::new(AtomicBool::new(false));
    let _worker_task = worker_runtime.spawn_worker_loop(backend, Arc::clone(&worker_shutdown));

    let (lease_deadline, claimed_at) =
        wait_for_claim_lease_span(&hub_db, "job-lease-default").await;
    assert_eq!(
        lease_deadline - claimed_at,
        HARDCODED_DEFAULT_LEASE_MS,
        "with nothing configured, the claimed lease span must stay byte-identical to today's \
         hardcoded 5-minute default"
    );

    worker_shutdown.store(true, Ordering::SeqCst);
    hub_shutdown.store(true, Ordering::SeqCst);
    let _ = hub_task.await;
}

#[tokio::test]
async fn explicit_fabric_worker_lease_ms_flows_into_the_claimed_lease_deadline() {
    let _env_guard = ENV_LOCK.lock().await;
    let _lease_env = EnvVarGuard::set(LEASE_ENV_VAR, &EXPLICIT_LEASE_MS.to_string());

    let (_hub_dir, hub_db, ticket, hub_shutdown, hub_task) = bring_up_hub().await;

    let submitter_db = Arc::new(Database::open_memory());
    install_work_ledger_schema(&submitter_db).expect("submitter ledger schema");
    let submitter_client =
        SyncClient::new(submitter_db.clone(), &ticket, TenantId::from(FABRIC_TENANT));
    submit_claimable_job(&submitter_db, "job-lease-explicit", "test-submitter");
    within(submitter_client.push())
        .await
        .expect("submitter push");

    let worker_dir = tempfile::tempdir().expect("worker data dir");
    let worker_runtime = within(FabricRuntime::start(
        worker_dir.path(),
        Some(&ticket),
        false,
    ))
    .await
    .expect("worker must enroll against a valid ticket");
    let backend = Arc::new(SpyBackend { tag: "burn-cpu" });
    let worker_shutdown = Arc::new(AtomicBool::new(false));
    let _worker_task = worker_runtime.spawn_worker_loop(backend, Arc::clone(&worker_shutdown));

    let (lease_deadline, claimed_at) =
        wait_for_claim_lease_span(&hub_db, "job-lease-explicit").await;
    assert_eq!(
        lease_deadline - claimed_at,
        EXPLICIT_LEASE_MS,
        "VIGIL_FABRIC_WORKER_LEASE_MS={EXPLICIT_LEASE_MS} must flow into the claimed lease span \
         — today spawn_worker_loop ignores it and the hardcoded 300000ms wins regardless"
    );

    worker_shutdown.store(true, Ordering::SeqCst);
    hub_shutdown.store(true, Ordering::SeqCst);
    let _ = hub_task.await;
}
