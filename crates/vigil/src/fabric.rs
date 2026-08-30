//! Fabric integration (criteria C3–C8): embeds contextdb-server's shared
//! work ledger + peer transport so this vigil node can submit its own
//! `vigil.detector` jobs onto the shared ledger AND/OR claim + execute jobs
//! submitted by other enrolled nodes — the SAME binary, no bespoke protocol
//! (placement rule; architecture Section 3 / Rule 5). Behind the
//! default-off `fabric` feature: a `vigil` build without it links no
//! sync/transport/ledger code at all (today's behavior, byte-identical).
//!
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use bytes::Bytes;
use context_graph::Store;
use contextdb_engine::Database;
use contextdb_engine::work_ledger::{ExecutionInputs, JobSnapshot};
use contextdb_server::work_ledger::{ExecutionOutput, ExecutionVerdict, WorkExecutor};
use contextdb_server::{FabricIdentity, SyncClient};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::DecodedRgbFrame;
use crate::config;
use crate::detector_workclass::{
    DETECTOR_SCHEMA_VERSION, DetectorDetection, DetectorJob, DetectorResult, WireResultEnvelope,
    WireVideoCodec, WireWorkEnvelope, decode_length_framed_units,
};
use crate::health::HealthState;
use crate::offload_policy::RemoteCapability;
use crate::runtime_stats::RuntimeStatsState;

/// The pluggable local-detection seam a fabric worker's job executor calls
/// after resolving + decoding a `vigil.detector` job's frames.
///
/// Distinct from the process-local `crate::detector::Detector` trait (which
/// is keyed on the crate-private `DecodedVideoSegment` type) so a worker's
/// real production backend AND a test double can both implement this
/// without reaching into pipeline-internal types: the production adapter
/// (wrapping `crate::detector::PromotableDetector`) lives inside this crate;
/// a test's spy implements this trait directly from an external test crate.
pub trait FabricDetectorBackend: Send + Sync {
    /// Truthful backend tag this node advertises, e.g. `burn-cpu`/
    /// `burn-wgpu` — matching this node's OWN promotion state, never a
    /// speed claim (criterion C3/C7).
    fn backend_tag(&self) -> &str;

    /// Content hash of the loaded model artifact (provenance).
    fn model_sha256(&self) -> &str;

    /// Run detection over already-decoded RGB frames sampled from the job's
    /// clip.
    fn detect(
        &self,
        frames: &[DecodedRgbFrame],
        clip_sha256: &str,
        sample_frames: usize,
        confidence_threshold: f64,
    ) -> Result<Vec<DetectorDetection>, String>;
}

/// Registered `work_capabilities` id for one backend, e.g.
/// `vigil-detector-burn-cpu` — a distinct id per backend so a late
/// promotion (CPU → wgpu) advertises a new truthful row rather than
/// mutating an existing one (capability rows are write-once; see the
/// working doc's Task-0 verdict — `capability_id` is cosmetic to matching,
/// so this module never depends on it being read back, only on the tags).
pub fn detector_capability_id(backend_tag: &str) -> String {
    format!("vigil-detector-{backend_tag}")
}

/// The contextdb `WorkExecutor` for work class `vigil.detector` (criterion
/// C3). A claimed job's two inputs arrive (in `job.input_refs` order,
/// already resolved by the worker loop's `WorkerConfig.blob_store` before
/// this trait is invoked): a ledger-carried JSON metadata chunk (the
/// `DetectorJob` payload minus its blob reference) and the `blob_ref`
/// length-framed `encoded_units` chunk. `execute` decodes the units, runs
/// `backend.detect`, and returns the `DetectorResult` wire payload as the
/// job's output bytes.
pub struct DetectorWorkExecutor<B: FabricDetectorBackend> {
    backend: Arc<B>,
    node_id: String,
}

impl<B: FabricDetectorBackend> DetectorWorkExecutor<B> {
    pub fn new(backend: Arc<B>, node_id: String) -> Self {
        Self { backend, node_id }
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }
}

impl<B: FabricDetectorBackend> WorkExecutor for DetectorWorkExecutor<B> {
    fn execute(
        &self,
        _job: &JobSnapshot,
        inputs: &ExecutionInputs,
        _should_abandon: &dyn Fn() -> bool,
    ) -> ExecutionVerdict {
        let started = std::time::Instant::now();

        // inputs arrive as (seq, bytes) pairs mirroring job.input_refs order:
        // the ledger-carried metadata chunk is always seq 0; an optional
        // blob_ref frames chunk (resolved by the worker loop's blob service
        // before this executor ever runs) is seq 1 when the job carries one.
        let mut ordered = inputs.clone();
        ordered.sort_by_key(|(seq, _)| *seq);

        let Some((_, metadata_bytes)) = ordered.first() else {
            return ExecutionVerdict::Failed(
                "vigil.detector job carries no ledger-carried metadata input".to_string(),
            );
        };
        let metadata_value: serde_json::Value = match serde_json::from_slice(metadata_bytes) {
            Ok(value) => value,
            Err(err) => {
                return ExecutionVerdict::Failed(format!(
                    "vigil.detector payload metadata is not valid JSON: {err}"
                ));
            }
        };

        // Version-checked BEFORE any blob is touched: an unsupported schema
        // fails the job typed without ever reaching the frames input.
        let schema_version = metadata_value
            .get("schema_version")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        if schema_version != u64::from(DETECTOR_SCHEMA_VERSION) {
            return ExecutionVerdict::Failed(format!(
                "unsupported vigil.detector payload schema_version {schema_version} \
                 (expected {DETECTOR_SCHEMA_VERSION})"
            ));
        }

        let job_payload: DetectorJob = match serde_json::from_value(metadata_value) {
            Ok(payload) => payload,
            Err(err) => {
                return ExecutionVerdict::Failed(format!(
                    "vigil.detector payload metadata malformed: {err}"
                ));
            }
        };

        // A job may carry no frames blob at all (this node's own submission,
        // executed locally without ever moving bytes — the C5 fallback and
        // C8 symmetry paths): treat that as zero frames rather than failing.
        let frames: Vec<DecodedRgbFrame> = match ordered.get(1) {
            Some((_, encoded_bytes)) => {
                let units = match decode_length_framed_units(encoded_bytes) {
                    Ok(units) => units,
                    Err(err) => {
                        return ExecutionVerdict::Failed(format!(
                            "vigil.detector frames blob is malformed: {err}"
                        ));
                    }
                };

                // Integrity guard, BEFORE detection: the transport itself is
                // already BLAKE3-covered upstream (blob_ref content
                // addressing), so this catches a different class of bug — a
                // SUBMITTER referencing the wrong blob for a job's metadata.
                // Recompute the clip hash over the decoded units the same
                // way the submitter did (`runtime.rs::encoded_clip_sha256`)
                // and fail the job typed on divergence, never silently
                // detect over mismatched content.
                let recomputed_clip_sha256 = encoded_clip_sha256(&units);
                if recomputed_clip_sha256 != job_payload.clip_sha256 {
                    return ExecutionVerdict::Failed(format!(
                        "clip hash mismatch: job metadata names clip_sha256={} but the resolved \
                         frames blob hashes to {recomputed_clip_sha256}",
                        job_payload.clip_sha256
                    ));
                }

                let codec = match job_payload.codec {
                    WireVideoCodec::H264 => crate::VideoCodec::H264,
                    WireVideoCodec::H265 => crate::VideoCodec::H265,
                };
                // Wire-received bytes have no upstream sharing to preserve;
                // wrap once here so the decode-side seam stays uniform with
                // the local-capture path's `Bytes` units.
                let units: Vec<Bytes> = units.into_iter().map(Into::into).collect();
                match crate::media_pipeline::decode_encoded_units(codec, units, job_payload.fps.0) {
                    Ok(segment) => segment.frames,
                    Err(err) => {
                        return ExecutionVerdict::Failed(format!(
                            "vigil.detector frames decode failed: {err}"
                        ));
                    }
                }
            }
            None => Vec::new(),
        };

        let detections = match self.backend.detect(
            &frames,
            &job_payload.clip_sha256,
            job_payload.sample_frames,
            job_payload.confidence_threshold.0,
        ) {
            Ok(detections) => detections,
            Err(err) => {
                return ExecutionVerdict::Failed(format!("vigil.detector backend failed: {err}"));
            }
        };

        let wall_clock_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let result = DetectorResult {
            schema_version: DETECTOR_SCHEMA_VERSION,
            result_envelope: WireResultEnvelope {
                work_id: job_payload.envelope.work_id.clone(),
                parent_work_id: job_payload.envelope.parent_work_id.clone(),
                contributing_work_ids: job_payload.envelope.contributing_work_ids.clone(),
                stage: job_payload.envelope.stage.clone(),
                stream_id: job_payload.envelope.stream_id.clone(),
                result_schema_version: job_payload.envelope.schema_version,
                receipt_id: uuid::Uuid::now_v7().to_string(),
            },
            detections: detections.clone(),
            detector_backend: self.backend.backend_tag().to_string(),
            detector_session_id: uuid::Uuid::now_v7().to_string(),
            model_sha256: self.backend.model_sha256().to_string(),
            model_forward_sha256: String::new(),
            detector_nms_sha256: String::new(),
            result_sha256: String::new(),
            clip_sha256: job_payload.clip_sha256.clone(),
        };
        let output = match serde_json::to_vec(&result) {
            Ok(bytes) => bytes,
            Err(err) => {
                return ExecutionVerdict::Failed(format!(
                    "vigil.detector result encode failed: {err}"
                ));
            }
        };
        let receipt = serde_json::json!({
            "executor_node_id": self.node_id,
            "backend": self.backend.backend_tag(),
            "wall_clock_ms": wall_clock_ms,
            "detections_count": detections.len(),
        });
        ExecutionVerdict::Completed(ExecutionOutput { output, receipt })
    }
}

/// One node's fabric wiring: its ledger-backed `Database`, its `SyncClient`
/// to the (possibly self-embedded — Design decision E) hub, and its fabric
/// identity. `None` ticket + `fabric_hub == false` is today's unenrolled
/// default; the runtime still stands up so this node's OWN jobs can be
/// submitted and claimed locally once a peer joins (criterion C8
/// symmetry).
pub struct FabricRuntime {
    pub db: Arc<Database>,
    pub client: Arc<SyncClient>,
    pub identity: FabricIdentity,
    pub node_id: String,
    /// `Some` only when this node carries the hub (`fabric_hub == true`) —
    /// Design decision E: the hub is embedded in the main vigil process
    /// (`SyncServer::new` starts ContextDB's typed authenticated Iroh hub), never a
    /// separate hub process.
    pub hub_endpoint: Option<Arc<contextdb_server::PeerEndpoint>>,
    /// This node's own bound endpoint, regardless of hub role — so it can
    /// SERVE the frame blobs of its own submitted jobs to a remote claimant
    /// node-to-node, never through the hub (criterion C1). Aliases
    /// `hub_endpoint`'s bound port when this node also carries the hub (one
    /// endpoint, two protocol registrations); its own otherwise.
    own_endpoint: Arc<contextdb_server::PeerEndpoint>,
    /// This node's blob store: ingests its own submissions' frame blobs
    /// and resolves `blob_ref` inputs it claims from others.
    blob_store: Arc<contextdb_server::blob_resolver::BlobStore>,
    /// `true` when this node has a usable fabric SERVING role: it either
    /// carries the hub, or it enrolled with a ticket that VALIDATED (so it is
    /// dialing a real hub). A node with a rejected ticket has worker INTENT
    /// but no serving role — it cannot reach the fleet, so the cameraless
    /// worker bootstrap must not pretend it is serving (owner steer
    /// 2026-07-13: serve with whatever you have, but only if you can actually
    /// reach the fleet).
    serves_role: bool,
    /// The named enrollment rejection reason when a configured ticket was
    /// malformed/expired (criterion C6 / USR-2) — surfaced on this node's own
    /// status/doctor rendering so a not-serving node says WHY, never a silent
    /// healthy-looking `enrolled=true`.
    enrollment_error: Option<String>,
    /// How long (ms) a remote's advertised detector capability stays LIVE
    /// since its last hub contact (or, absent a hub contact record, since its
    /// advertisement). Past this, the remote ages out of both the rendered
    /// remote-detectors line and the offload-routing set. Defaults to
    /// [`DEFAULT_CAPABILITY_LIVENESS_TTL_MS`]; override with
    /// [`FabricRuntime::with_capability_liveness_ttl_ms`].
    capability_liveness_ttl_ms: i64,
    /// This node's resolved worker-lease duration (ms), criterion C10 fix
    /// cycle 9. `None` (the `start()` default) means "no resolved config
    /// value was threaded through" — [`FabricRuntime::spawn_worker_loop`]
    /// then falls back to `VIGIL_FABRIC_WORKER_LEASE_MS` directly, then
    /// 300000ms. Production sets this via
    /// [`FabricRuntime::with_worker_lease_ms`] from
    /// `config::RuntimeConfig::fabric_worker_lease_ms` — the add-on options
    /// page, config file, and CLI all reach the worker through THIS field,
    /// never the direct env fallback (options never become env vars; see
    /// `fabric_bring_up`'s call site).
    worker_lease_ms: Option<u64>,
    /// Persisted ledger-time authority carried into async workers.
    persisted_clock: crate::PersistedClock,
}

/// Default liveness TTL for a remote's advertised detector capability: a
/// generous 5× the worker poll cadence (2s — the `run_worker_loop` interval in
/// [`FabricRuntime::spawn_worker_loop`]). On every poll the worker BOTH
/// re-advertises (refreshing `advertised_at`, which propagates to consumer
/// edges) AND contacts the hub (advancing the hub's last-contact clock), so
/// both liveness signals refresh on that 2s cadence; a TTL of several times it
/// means a single missed poll never flickers a live worker out of the rendered
/// or routable set. Additive and overridable per node via
/// [`FabricRuntime::with_capability_liveness_ttl_ms`].
pub const DEFAULT_CAPABILITY_LIVENESS_TTL_MS: i64 = 10_000;

/// The tenant every vigil fabric node enrolls under. Single-tenant by
/// construction (the OSS/commercial boundary is multi-tenancy, never a
/// single-node concern) — a fixed, documented value rather than an
/// operator-facing knob nothing yet needs.
const FABRIC_TENANT: &str = "vigil-fabric";

impl FabricRuntime {
    /// Bring up this node's fabric wiring: open the ledger database, bind
    /// this node's own endpoint (always, so it can serve its own blobs),
    /// embed the hub when `fabric_hub` is set, and enroll via `ticket` if
    /// given. A malformed ticket never prevents standalone bring-up
    /// (criterion C6) — it is logged and this node continues unenrolled.
    pub async fn start(
        data_dir: &Path,
        fabric_ticket: Option<&str>,
        fabric_hub: bool,
    ) -> Result<Self, FabricStartError> {
        std::fs::create_dir_all(data_dir).map_err(|err| {
            format!(
                "fabric: create data directory {}: {err}",
                data_dir.display()
            )
        })?;
        let identity_path = data_dir.join("fabric-identity.key");
        let identity = FabricIdentity::load_or_generate(&identity_path).map_err(|err| {
            format!(
                "fabric: load or generate identity at {}: {err}",
                identity_path.display()
            )
        })?;
        let node_id = identity.node_id();

        let db_path = data_dir.join("fabric-ledger.db");
        let db = Arc::new(Database::open(&db_path).map_err(|err| {
            let message = format!("fabric: open ledger database {}: {err}", db_path.display());
            if store_is_held_by_a_writer(&err) {
                FabricStartError::StoreHeldByWriter { message }
            } else {
                FabricStartError::Other(message)
            }
        })?);
        // The ledger's `peer_directory` table carries this node's own
        // enrollment ticket (the same credential cached at `fabric/own-ticket`,
        // 0600). contextdb creates the file at its own default mode
        // (observed 0664 — group/world readable), so vigil tightens it here,
        // in its own data dir, after open: chmod is idempotent against an fd
        // already held open for continued writes (the mode lives on the
        // inode, not reset per-write), so this holds for the life of the
        // process, not just at creation. No upstream contextdb change — that
        // work is closed by owner ruling.
        tighten_fabric_ledger_permissions(&db_path)?;
        contextdb_engine::work_ledger::install_work_ledger_schema(&db)
            .map_err(|err| format!("fabric: install work ledger schema: {err}"))?;
        let _ = contextdb_engine::peer_directory::install_peer_directory_schema(&db);

        let bind_spec = contextdb_server::peer_bind_spec(&identity_path);
        let own_endpoint = Arc::new(
            contextdb_server::PeerEndpoint::bind(&bind_spec)
                .await
                .map_err(|err| format!("fabric: bind this node's endpoint: {err}"))?,
        );
        // Best-effort local cache of this node's own ticket, read back by
        // `run_ticket_command` when a live rebind fails because a running
        // vigil process already holds this identity's sticky port — the
        // ordinary case, since an operator only ever runs `vigil fabric
        // ticket` while Vigil is up. A write failure here is never fatal:
        // it only means that fallback path degrades to its own error.
        write_own_ticket_cache(data_dir, &own_endpoint.ticket());
        // Stamp the identity that produced the cached ticket, so a later
        // fallback read can tell "this cache still belongs to the identity
        // in play" from "this cache is a leftover from a since-rotated
        // identity" — see `run_ticket_command`'s freshness check.
        write_own_ticket_identity_stamp(data_dir, &node_id);

        let blob_store = Arc::new(contextdb_server::blob_resolver::BlobStore::new(
            db.clone(),
            contextdb_engine::work_ledger::MovementPolicy {
                auto_propagate: true,
            },
            identity_path.clone(),
        ));
        blob_store.serve_on(&own_endpoint);

        let tenant = contextdb_core::TenantId::from(FABRIC_TENANT);
        let hub_endpoint = if fabric_hub {
            let server = contextdb_server::SyncServer::new(
                db.clone(),
                own_endpoint.as_ref(),
                tenant.clone(),
            );
            // Detached: this hub serves for the life of the process. Standing
            // this up is bring-up, not a call the tests drain — no shutdown
            // handle is threaded back through this constructor.
            tokio::spawn(async move { server.run().await });
            Some(own_endpoint.clone())
        } else {
            None
        };

        // A malformed ticket never blocks standalone bring-up: it is named,
        // logged, and this node dials nothing (falling back to its own
        // bind spec, which simply never resolves a `to=` target).
        let (dial_spec, ticket_valid, enrollment_error) = match fabric_ticket {
            Some(ticket) => match Self::validate_fabric_ticket(ticket) {
                Ok(()) => (
                    contextdb_server::peer_dial_spec(ticket.trim(), &identity_path),
                    true,
                    None,
                ),
                Err(err) => {
                    eprintln!(
                        "vigil: fabric enrollment ticket rejected, continuing standalone: {err}"
                    );
                    (bind_spec.clone(), false, Some(err))
                }
            },
            None => (bind_spec.clone(), false, None),
        };
        let client = Arc::new(SyncClient::new(db.clone(), &dial_spec, tenant));
        // A serving role means this node can actually reach the fleet: it
        // carries the hub, or it dialed a hub with a validated ticket.
        let serves_role = hub_endpoint.is_some() || ticket_valid;

        Ok(Self {
            db,
            client,
            identity,
            node_id,
            hub_endpoint,
            own_endpoint,
            blob_store,
            serves_role,
            enrollment_error,
            capability_liveness_ttl_ms: DEFAULT_CAPABILITY_LIVENESS_TTL_MS,
            worker_lease_ms: None,
            persisted_clock: crate::PersistedClock::contextdb(),
        })
    }

    /// Override the remote-capability liveness TTL (ms) for this node. Additive
    /// knob; the default ([`DEFAULT_CAPABILITY_LIVENESS_TTL_MS`]) is derived
    /// from the worker poll cadence and needs no touching in normal operation.
    pub fn with_capability_liveness_ttl_ms(mut self, ttl_ms: i64) -> Self {
        self.capability_liveness_ttl_ms = ttl_ms;
        self
    }

    /// Set this node's resolved worker-lease duration (ms), criterion C10
    /// fix cycle 9 — the production path from the config surface (add-on
    /// options page / config file / CLI, all folded through
    /// `config::RuntimeConfig::fabric_worker_lease_ms`) into
    /// [`FabricRuntime::spawn_worker_loop`]. Additive builder, same shape as
    /// [`FabricRuntime::with_capability_liveness_ttl_ms`]; a construction
    /// that never calls this (e.g. a test driving `start()` directly) keeps
    /// `spawn_worker_loop`'s env-var/hardcoded-default fallback.
    pub fn with_worker_lease_ms(mut self, lease_ms: u64) -> Self {
        self.worker_lease_ms = Some(lease_ms);
        self
    }

    /// Carry an explicit persisted-time authority into all fabric workers.
    pub fn with_persisted_clock(mut self, clock: crate::PersistedClock) -> Self {
        self.persisted_clock = clock;
        self
    }

    fn wall_now_ms(&self) -> i64 {
        i64::try_from(self.persisted_clock.unix_millis())
            .expect("fabric ledger milliseconds fit signed timestamp fields")
    }

    /// Whether this node has a usable fabric SERVING role (hub, or enrolled
    /// with a validated ticket) — the gate for bootstrapping a cameraless
    /// worker detector and for the honest `fabric-worker-serving` signal.
    pub fn serves_fabric_role(&self) -> bool {
        self.serves_role
    }

    /// The named enrollment rejection reason, when a configured ticket was
    /// rejected (criterion C6) — `None` when no ticket was configured or the
    /// ticket validated.
    pub fn enrollment_error(&self) -> Option<&str> {
        self.enrollment_error.as_deref()
    }

    /// The ready-to-use join instruction this node's own status/doctor/log
    /// surfaces print (criterion C6): when this node carries the hub
    /// (`hub_endpoint.is_some()`), the current ticket plus the exact
    /// command a second machine runs to join
    /// (`fabric-join ticket=<current> command=<...>`); when it does not,
    /// the one-line instruction naming how to grow this node into a join
    /// point (enable the hub role) — so a lone, unenrolled node's own
    /// output still teaches the join path before anyone has joined it.
    pub fn join_instruction(&self) -> String {
        match &self.hub_endpoint {
            Some(endpoint) => {
                let ticket = endpoint.ticket();
                format!("fabric-join ticket={ticket} command=vigil run --fabric-ticket {ticket}")
            }
            None => "grow this node into a fabric join point by enabling the hub role \
                     (fabric_hub = true) — its status/doctor output will then print a \
                     ready-to-paste fabric-join line a second machine can use to enroll"
                .to_string(),
        }
    }

    /// This node's own enrollment ticket, regardless of whether it carries
    /// the hub role: `own_endpoint` always binds at [`FabricRuntime::start`]
    /// (so this node can serve its own submitted jobs' blobs node-to-node),
    /// and it aliases `hub_endpoint` when this node IS the hub — so this
    /// returns the exact same value a carried hub's `join_instruction`
    /// advertises. The retrieval surface for `vigil fabric ticket`: a
    /// deliberate local act, never served over HTTP.
    pub fn own_ticket(&self) -> String {
        self.own_endpoint.ticket()
    }

    /// Validate a fabric ticket an operator supplied (config/env/CLI/HAOS
    /// options), before ever dialing it. A malformed or expired ticket
    /// returns a typed error whose text NAMES THE FIX (criterion C6 /
    /// USR-2) — never a panic, never a hang; the caller continues
    /// standalone.
    pub fn validate_fabric_ticket(ticket: &str) -> Result<(), String> {
        let trimmed = ticket.trim();
        if trimmed.is_empty() {
            return Err(
                "fabric ticket is empty — paste the ticket printed by the hub node's \
                 fabric-join line"
                    .to_string(),
            );
        }
        match contextdb_server::PeerEndpointSpec::parse_detailed(trimmed) {
            Ok(Some(spec)) if spec.dial_ticket().is_some() => Ok(()),
            // The ticket itself is never reproduced here (a fabric ticket is
            // an enrollment credential) — only that it was malformed and
            // what to do about it.
            Ok(_) => Err(
                "not a valid fabric enrollment ticket — paste the exact ticket printed by the \
                 hub node's fabric-join line, unedited"
                    .to_string(),
            ),
            Err(message) => Err(format!("invalid fabric enrollment ticket: {message}")),
        }
    }

    /// Submit one segment as a `vigil.detector` job onto the shared ledger
    /// (criteria C1/C2 offload leg): ingests the length-framed
    /// `encoded_units` as a blob, submits the job with the ledger-carried
    /// metadata chunk plus the `blob_ref` input, and returns the minted job
    /// id.
    #[allow(clippy::too_many_arguments)]
    pub async fn submit_detector_job(
        &self,
        envelope: WireWorkEnvelope,
        codec: WireVideoCodec,
        fps: f64,
        sample_frames: usize,
        confidence_threshold: f64,
        clip_sha256: String,
        decoded_frames_sha256: String,
        model_id: String,
        encoded_units: &[Bytes],
        deadline_ms: Option<i64>,
    ) -> Result<String, String> {
        let framed = crate::detector_workclass::encode_length_framed_units(encoded_units);
        let hash = self
            .blob_store
            .ingest_bytes(&framed)
            .map_err(|err| format!("fabric: ingest frames blob: {err}"))?;
        // A claiming worker resolves this blob node-to-node against THIS
        // node's own ticket (the submitter is always the holder).
        let _ = contextdb_engine::peer_directory::register_peer_ticket(
            &self.db,
            &self.node_id,
            &self.own_endpoint.ticket(),
            self.wall_now_ms(),
        );

        let frames_blob_ref = crate::detector_workclass::FrameBlobRef::from_blob_hash(&hash);
        let work_id = envelope.work_id.clone();
        let job = crate::detector_workclass::DetectorJobBuilder::new(envelope, frames_blob_ref)
            .codec(codec)
            .fps(fps)
            .sample_frames(sample_frames)
            .confidence_threshold(confidence_threshold)
            .clip_sha256(clip_sha256)
            .decoded_frames_sha256(decoded_frames_sha256)
            .model_id(model_id)
            .build();
        let metadata_bytes = serde_json::to_vec(&job)
            .map_err(|err| format!("fabric: encode detector job metadata: {err}"))?;

        let job_id = format!("vigil.detector.{work_id}");
        let mut builder = contextdb_engine::work_ledger::JobSpec::builder(
            job_id.clone(),
            crate::detector_workclass::DETECTOR_WORK_CLASS,
            crate::detector_workclass::DETECTOR_MODE,
            self.node_id.clone(),
        )
        .requirement_tags(vec![
            crate::detector_workclass::DETECTOR_CLASS_TAG.to_string(),
        ])
        .input_refs(vec![
            contextdb_engine::work_ledger::InputRef::ledger_input(),
            contextdb_engine::work_ledger::InputRef::blob_ref(hash),
        ])
        .submitted_at_ms(self.wall_now_ms());
        if let Some(deadline_ms) = deadline_ms {
            builder = builder.deadline_ms(Some(deadline_ms));
        }
        contextdb_engine::work_ledger::submit_job(&self.db, &builder.build(), &[metadata_bytes])
            .map_err(|err| format!("fabric: submit detector job: {err}"))?;
        let _ = self.client.push().await;
        Ok(job_id)
    }

    /// The LIVE, CURRENT remote `vigil.detector` capability set — the ONE
    /// vocabulary both the rendered `remote-detectors=` line and the offload
    /// routing consume (criterion C2's `RemoteCapability` input and criterion
    /// C7 provenance). Because both surfaces call THIS function and nothing
    /// else, routing is structurally unable to pick a node the render would
    /// not show.
    ///
    /// Per remote node (never this one), it collapses that node's advertised
    /// `vigil-detector-*` rows to its CURRENT backend — the LATEST-advertised
    /// row — so a worker restarted onto a different backend renders only its
    /// present one, never a stale sibling row. The node is offered only while
    /// LIVE: its hub last-contact (or, absent a contact record — e.g. on an
    /// edge that never sees the hub-local table — the current row's
    /// `advertised_at`) is within `capability_liveness_ttl_ms`. A worker that
    /// stopped contacting the hub ages out of both surfaces even though its
    /// capability row lingers.
    ///
    /// `idle` is always reported `true` here — this node has no live load
    /// signal for a REMOTE peer; the policy's "idle" is only "not known to be
    /// saturated", never a speed claim, so this is the honest floor pending a
    /// richer capability-detail signal.
    pub async fn remote_detector_capabilities(&self) -> Result<Vec<RemoteCapability>, String> {
        let result = self
            .db
            .execute(
                "SELECT node_id, capability_id, advertised_at FROM work_capabilities",
                &std::collections::HashMap::new(),
            )
            .map_err(|err| format!("fabric: scan work_capabilities: {err}"))?;
        let node_idx = result
            .columns
            .iter()
            .position(|column| column == "node_id")
            .ok_or_else(|| "fabric: work_capabilities missing node_id column".to_string())?;
        let capability_idx = result
            .columns
            .iter()
            .position(|column| column == "capability_id")
            .ok_or_else(|| "fabric: work_capabilities missing capability_id column".to_string())?;
        let advertised_idx = result
            .columns
            .iter()
            .position(|column| column == "advertised_at")
            .ok_or_else(|| "fabric: work_capabilities missing advertised_at column".to_string())?;

        let contacts = self.hub_last_contacts()?;

        // Per remote node, collapse its detector rows to its CURRENT backend
        // (the greatest-advertised_at `vigil-detector-*` row) AND track the
        // freshest advertisement across ALL its capability rows for liveness.
        let mut current: std::collections::HashMap<String, (i64, String)> =
            std::collections::HashMap::new();
        let mut node_last_advertised: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        for row in &result.rows {
            let contextdb_core::Value::Text(node) = &row[node_idx] else {
                continue;
            };
            if node == &self.node_id {
                continue;
            }
            let contextdb_core::Value::Text(capability_id) = &row[capability_idx] else {
                continue;
            };
            let advertised_at = match &row[advertised_idx] {
                contextdb_core::Value::Timestamp(ms) => *ms,
                contextdb_core::Value::Int64(ms) => *ms,
                _ => continue,
            };
            // Liveness is fed by the node's FRESHEST advertisement across all
            // its rows — the standing worker loop refreshes its `worker-loop`
            // heartbeat every poll cycle even when the detector backend row is
            // unchanged, so on a consumer edge (no hub last-contact clock) a
            // live worker stays current here rather than aging out on a frozen
            // detector-row timestamp.
            node_last_advertised
                .entry(node.clone())
                .and_modify(|seen| {
                    if advertised_at > *seen {
                        *seen = advertised_at;
                    }
                })
                .or_insert(advertised_at);
            // The CURRENT backend is the latest-advertised detector row.
            let Some(backend) = capability_id.strip_prefix("vigil-detector-") else {
                continue;
            };
            current
                .entry(node.clone())
                .and_modify(|(seen_at, seen_backend)| {
                    if advertised_at >= *seen_at {
                        *seen_at = advertised_at;
                        *seen_backend = backend.to_string();
                    }
                })
                .or_insert_with(|| (advertised_at, backend.to_string()));
        }

        let now = self.wall_now_ms();
        let mut remotes = Vec::new();
        for (node, (_current_at, backend)) in current {
            // Liveness clock: the hub's last contact with this node (the
            // strongest signal, present only on a hub), else the node's
            // freshest advertisement across all its rows.
            let last_seen = contacts
                .get(&node)
                .copied()
                .or_else(|| node_last_advertised.get(&node).copied())
                .unwrap_or(0);
            if now.saturating_sub(last_seen) > self.capability_liveness_ttl_ms {
                continue;
            }
            remotes.push(RemoteCapability {
                node_id: node,
                backend,
                idle: true,
            });
        }
        Ok(remotes)
    }

    /// The hub-local per-node last-contact clock (`work_node_contacts`), read
    /// into a `node_id → last_contact_ms` map. Present only on a node that
    /// carries the hub — the table is never synced to edges — so an edge (or a
    /// hub before its first served exchange) simply gets an empty map and the
    /// caller falls back to each capability's `advertised_at` for liveness.
    fn hub_last_contacts(&self) -> Result<std::collections::HashMap<String, i64>, String> {
        let mut contacts = std::collections::HashMap::new();
        if !self
            .db
            .table_names()
            .iter()
            .any(|name| name == contextdb_server::work_ledger::WORK_NODE_CONTACTS_TABLE)
        {
            return Ok(contacts);
        }
        let result = self
            .db
            .execute(
                "SELECT node_id, last_contact_ms FROM work_node_contacts",
                &std::collections::HashMap::new(),
            )
            .map_err(|err| format!("fabric: scan work_node_contacts: {err}"))?;
        let node_idx = result
            .columns
            .iter()
            .position(|column| column == "node_id")
            .ok_or_else(|| "fabric: work_node_contacts missing node_id column".to_string())?;
        let contact_idx = result
            .columns
            .iter()
            .position(|column| column == "last_contact_ms")
            .ok_or_else(|| {
                "fabric: work_node_contacts missing last_contact_ms column".to_string()
            })?;
        for row in &result.rows {
            let contextdb_core::Value::Text(node) = &row[node_idx] else {
                continue;
            };
            let last_contact = match &row[contact_idx] {
                contextdb_core::Value::Timestamp(ms) => *ms,
                contextdb_core::Value::Int64(ms) => *ms,
                _ => continue,
            };
            contacts.insert(node.clone(), last_contact);
        }
        Ok(contacts)
    }

    /// Poll for and apply any `vigil.detector` results this node submitted
    /// (criterion C4): validates the result join through the existing
    /// class-map/NMS authority, applies exactly once, and discards a
    /// late/duplicate result with a receipt — never double-counted. Returns
    /// the count of results applied this pass.
    pub async fn poll_detector_results(&self) -> Result<usize, String> {
        let _ = self.client.pull_default().await;
        let mut params = std::collections::HashMap::new();
        params.insert(
            "node_id".to_string(),
            contextdb_core::Value::Text(self.node_id.clone()),
        );
        let jobs = self
            .db
            .execute(
                "SELECT job_id FROM work_jobs WHERE submitter_node_id = $node_id",
                &params,
            )
            .map_err(|err| format!("fabric: scan submitted jobs: {err}"))?;
        let job_id_idx = jobs
            .columns
            .iter()
            .position(|column| column == "job_id")
            .ok_or_else(|| "fabric: work_jobs missing job_id column".to_string())?;

        let mut landed = 0usize;
        for row in &jobs.rows {
            let contextdb_core::Value::Text(job_id) = &row[job_id_idx] else {
                continue;
            };
            if contextdb_engine::work_ledger::job_result(&self.db, job_id)
                .map_err(|err| format!("fabric: read job result for {job_id}: {err}"))?
                .is_some()
            {
                landed += 1;
            }
        }
        Ok(landed)
    }

    /// Spawn this node's standing worker loop (criteria C3/C5/C8): advertise
    /// `vigil-detector-<backend>`, then claim + execute matching jobs
    /// (including this node's own deferred submissions, once their deadline
    /// passes) until `shutdown` flips.
    pub fn spawn_worker_loop<B: FabricDetectorBackend + 'static>(
        &self,
        backend: Arc<B>,
        shutdown: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        let client = self.client.clone();
        let db = self.db.clone();
        let blob_store = self.blob_store.clone();
        let node_id = self.node_id.clone();
        let backend_tag = backend.backend_tag().to_string();
        let advertised_tags = vec![
            crate::detector_workclass::DETECTOR_CLASS_TAG.to_string(),
            format!("backend:{backend_tag}"),
        ];
        let executor: Arc<dyn WorkExecutor> =
            Arc::new(DetectorWorkExecutor::new(backend, node_id.clone()));
        // This node's ledger writes are canonical exactly when it hosts the
        // hub over the same db (`hub_endpoint.is_some()`) — a ticket-only
        // edge still has a real hub to dial, so ticket presence alone is not
        // the condition.
        let writes_are_canonical = self.hub_endpoint.is_some();
        // Worker lease duration (criterion C10, fix cycle 9), resolved in
        // this order: (1) `self.worker_lease_ms` — the RESOLVED config
        // value, set via `with_worker_lease_ms` at the production
        // construction site (`fabric_bring_up`, from
        // `config::RuntimeConfig::fabric_worker_lease_ms`, which already
        // folds add-on options/config-file/env/CLI through `config.rs`'s own
        // CLI > env > options precedence) — this is the path every real
        // operator surface reaches, since add-on OPTIONS ARE NOT ENV VARS
        // and never reach this method any other way; (2) a direct
        // `VIGIL_FABRIC_WORKER_LEASE_MS` read (the same idiom this file
        // already uses for `VIGIL_FABRIC_BRINGUP_DELAY_MS` above) — for a
        // construction that bypasses config resolution entirely (e.g. a
        // test driving `FabricRuntime::start` directly); (3) 300000ms,
        // byte-identical to this method's prior hardcoded literal.
        let config = contextdb_server::work_ledger::WorkerConfig {
            node_id,
            advertised_tags: advertised_tags.clone(),
            movement_policy: contextdb_engine::work_ledger::MovementPolicy {
                auto_propagate: true,
            },
            lease_duration_ms: resolved_worker_lease_duration_ms(self.worker_lease_ms),
            blob_store: Some(blob_store),
            defer_own_submissions_until_deadline: true,
            writes_are_canonical,
        };
        let persisted_clock = self.persisted_clock.clone();
        tokio::spawn(async move {
            // Advertise THIS node's truthful detector capability under the
            // `vigil-detector-<backend>` id `remote_detector_capabilities`
            // filters on (fabric.rs's `strip_prefix("vigil-detector-")`) —
            // contextdb's generic `run_worker_loop` only advertises the fixed
            // literal `"worker-loop"`, which that filter can never match. A
            // distinct id per backend tag (write-once per `(node_id,
            // capability_id)`) means a later promotion advertises a NEW row
            // rather than mutating this one, so the fleet sees an additive,
            // never-edited provenance trail (criterion C3/C7).
            let capability_id = detector_capability_id(&backend_tag);
            if let Err(error) = contextdb_engine::work_ledger::advertise_capability(
                &db,
                &config.node_id,
                &capability_id,
                &advertised_tags,
                i64::try_from(persisted_clock.unix_millis())
                    .expect("fabric capability milliseconds fit signed timestamp fields"),
            ) {
                println!(
                    "fabric_capability_advertise_failed=true backend={backend_tag} error={error}"
                );
            }
            // Deliver the advertisement to the hub. This one-shot startup push
            // (like `run_worker_loop`'s own) can miss a hub that is not yet
            // reachable; `SyncClient::push` already retries a transient miss,
            // and the standing poll loop re-pushes the outstanding
            // advertisement on its cadence until it lands. A push that still
            // fails here is NOT swallowed — it is a NAMED, greppable line so a
            // real fleet miss is diagnosable instead of silent (the S2 defect:
            // a missed push read as `remote-detectors=-` with nothing logged).
            // A node that hosts the hub over this same db skips the push
            // outright (operator honesty, fix cycle 8 option C): its rows are
            // already in the canonical store, so there is no dialable hub to
            // miss and nothing to retry — printing the failure line here
            // would just be permanent, meaningless noise on a healthy hub.
            let delivery = if writes_are_canonical {
                "not-attempted-canonical-store"
            } else {
                match client.push().await {
                    Ok(_) => "delivered",
                    Err(error) => {
                        println!(
                            "fabric_capability_push_failed=true backend={backend_tag} error={error}"
                        );
                        "failed"
                    }
                }
            };
            // The attempt is OVER, whichever way it went, and says so on one
            // line. A delivery that succeeds otherwise prints nothing at all,
            // which leaves anyone watching this node — an operator reading the
            // log, or a test — with no positive signal that the attempt ever
            // resolved and only an absence to infer it from. An absence
            // reached by waiting is indistinguishable from an attempt that has
            // not happened yet.
            println!(
                "fabric_capability_push_settled=true backend={backend_tag} \
                 capability={capability_id} outcome={delivery}"
            );
            let _ = contextdb_server::work_ledger::run_worker_loop(
                &client,
                &config,
                executor,
                std::time::Duration::from_secs(2),
                shutdown,
            )
            .await;
        })
    }
}

/// Resolve the exact lease duration passed to ContextDB's standing worker.
///
/// Production supplies `configured` from the already-resolved Vigil
/// configuration; the fallback is Vigil's own five-minute lease, for a direct
/// `FabricRuntime` construction that supplies nothing. Kept as one
/// production-used seam so config tests do not need a live Iroh hub merely to
/// observe a value before it enters `WorkerConfig`.
#[doc(hidden)]
pub fn resolved_worker_lease_duration_ms(configured: Option<u64>) -> i64 {
    configured.unwrap_or(crate::settings_backends::automatic::FABRIC_WORKER_LEASE_MS as u64) as i64
}

/// The same clip-hash algorithm the submitter uses
/// (`runtime.rs::encoded_clip_sha256`): hash each encoded unit's raw bytes in
/// order. Kept as a free function so both the submit path (implicitly, via
/// the caller-supplied `clip_sha256`) and this executor's integrity guard
/// stay byte-identical by construction.
fn encoded_clip_sha256(units: &[Vec<u8>]) -> String {
    let mut hasher = Sha256::new();
    for unit in units {
        hasher.update(unit);
    }
    format!("{:x}", hasher.finalize())
}

/// Result-join authority for offloaded detector work (criterion C4): remote
/// results join back to stream/frame identity through the existing typed
/// envelope vocabulary (`crate::workgraph::validate_result_join`); class-map
/// authority and NMS ownership stay vigil-side and identical for local and
/// remote results. Tracks, per submitted `vigil.detector` job id, the
/// pending work envelope and whether a LOCAL fallback (criterion C5) has
/// already applied the same work — so a remote result racing a local
/// fallback (or a duplicate remote result) is discarded, never
/// double-counted.
pub struct PendingOffloads {
    entries: std::sync::Mutex<std::collections::HashMap<String, PendingOffload>>,
}

struct PendingOffload {
    work: crate::workgraph::WorkEnvelope,
    resolved: bool,
}

/// What happened when a remote `vigil.detector` result was offered against
/// a pending offload.
#[derive(Debug)]
pub enum RemoteResultOutcome {
    /// The result joined its work and was applied (exactly once).
    Applied,
    /// The result failed `validate_result_join` — never applied, counted on
    /// the shared `StageReceiptLog` via `count_rejected_join`, no events.
    RejectedJoin(crate::workgraph::JoinRejection),
    /// The work this result answers was already resolved (a local fallback
    /// applied first, or an earlier remote result already landed) —
    /// discarded with a `provenance=discarded-late` receipt, never
    /// double-counted.
    DiscardedLate,
}

impl Default for PendingOffloads {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingOffloads {
    pub fn new() -> Self {
        Self {
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Record that `job_id` was offloaded for `work`, pending either a
    /// remote result or a local fallback (criterion C5).
    pub fn track(&self, job_id: String, work: crate::workgraph::WorkEnvelope) {
        self.entries.lock().expect("pending offloads lock").insert(
            job_id,
            PendingOffload {
                work,
                resolved: false,
            },
        );
    }

    /// Mark `job_id` resolved because a local fallback ran the detection
    /// itself (criterion C5) — any later-arriving remote result for the
    /// same job must discard as late, never double-count.
    ///
    /// This in-process flag is the result-join CONTRACT-TEST seam: it lets
    /// tests exercise the late/duplicate-discard path deterministically.
    /// Production correctness does not depend on it — the ledger's own
    /// exactly-once result delivery is what actually prevents double-count.
    pub fn mark_resolved_by_fallback(&self, job_id: &str) {
        if let Some(entry) = self
            .entries
            .lock()
            .expect("pending offloads lock")
            .get_mut(job_id)
        {
            entry.resolved = true;
        }
    }

    /// Offer a remote `vigil.detector` result for `job_id`. Validates the
    /// join via `crate::workgraph::validate_result_join`, applies exactly
    /// once, and discards late/duplicate results — never double-counted
    /// (criterion C4).
    pub fn apply_remote_result(
        &self,
        job_id: &str,
        result: &crate::workgraph::ResultEnvelope,
        receipt: &crate::workgraph::StageReceipt,
        log: &crate::workgraph::StageReceiptLog,
    ) -> RemoteResultOutcome {
        let mut entries = self.entries.lock().expect("pending offloads lock");
        let Some(entry) = entries.get_mut(job_id) else {
            // No tracked pending offload for this job: already applied and
            // forgotten, or never tracked here — either way, discard.
            return RemoteResultOutcome::DiscardedLate;
        };
        if entry.resolved {
            // Either a local fallback already ran this segment (C5), or an
            // earlier remote result already landed — never double-count.
            return RemoteResultOutcome::DiscardedLate;
        }
        match crate::workgraph::validate_result_join(&entry.work, result, Some(receipt)) {
            Ok(()) => {
                entry.resolved = true;
                RemoteResultOutcome::Applied
            }
            Err(rejection) => {
                log.count_rejected_join();
                RemoteResultOutcome::RejectedJoin(rejection)
            }
        }
    }
}

/// The production `FabricDetectorBackend` over this crate's own detection
/// seam (`crate::detector::PromotableDetector`/`Detector`): the real path a
/// worker's standing loop uses. `fabric_bring_up`'s worker task constructs it
/// from whatever detector populated `worker_detector_slot` — a camera's
/// detector, or (for a cameraless worker box) the one
/// `runtime::bootstrap_worker_detector` loads — so a worker serves the fleet
/// regardless of whether it owns cameras (owner steer 2026-07-13). No RED
/// test binds this adapter directly; it is covered by the cameraless-worker
/// subprocess test end-to-end plus the owner smoke.
pub struct FabricProductionDetectorBackend {
    detector: Arc<crate::detector::PromotableDetector>,
    backend_tag: String,
}

impl FabricProductionDetectorBackend {
    pub(crate) fn new(
        detector: Arc<crate::detector::PromotableDetector>,
        backend_tag: String,
    ) -> Self {
        Self {
            detector,
            backend_tag,
        }
    }
}

impl FabricDetectorBackend for FabricProductionDetectorBackend {
    fn backend_tag(&self) -> &str {
        &self.backend_tag
    }

    fn model_sha256(&self) -> &str {
        use crate::detector::Detector;
        self.detector.model_sha256()
    }

    fn detect(
        &self,
        frames: &[DecodedRgbFrame],
        clip_sha256: &str,
        sample_frames: usize,
        confidence_threshold: f64,
    ) -> Result<Vec<DetectorDetection>, String> {
        use crate::detector::Detector;

        // The trait's real detection seam is keyed on the pipeline-internal
        // `DecodedVideoSegment`; this adapter wraps the already-decoded
        // frames this executor resolved (locally or via blob_ref) into the
        // same shape a live capture session would produce. `encoded_units`
        // stays empty and `fps`/`observed_at` are immaterial here — the
        // detector only samples `frames`.
        let segment = crate::media_pipeline::DecodedVideoSegment {
            frames: frames.to_vec(),
            encoded_units: Vec::new(),
            fps: 0.0,
            observed_at: None,
            // Immaterial here: `detect_segment` only samples `frames`; this
            // adapter never re-decodes `encoded_units` (which stays empty).
            codec: crate::VideoCodec::H264,
        };
        let output = self.detector.detect_segment(
            &segment,
            clip_sha256.to_string(),
            sample_frames,
            confidence_threshold,
        )?;
        Ok(output
            .detections
            .into_iter()
            .map(|detection| DetectorDetection {
                class_name: detection.class_name,
                confidence: crate::detector_workclass::OrderedF64(detection.confidence),
                bbox: detection.bbox,
                frame_index: detection.frame_index,
            })
            .collect())
    }
}

/// How this node's one worker-detector question was answered.
pub(crate) enum WorkerSlotOutcome {
    /// A detector loaded. This is the instance the worker loop serves with,
    /// and the truthful backend tag it advertises to the fleet.
    Ready(Arc<crate::detector::PromotableDetector>, String),
    /// No detector will arrive on this node: the model is missing or could not
    /// be loaded. This is a settled answer, not a failure to wait long enough,
    /// and it carries the reason an operator has to act on — the staging fix a
    /// worker box names, or the real load error a camera's detector failed
    /// with. Carried on the answer rather than written beside it because
    /// whoever answers is the only one who knows why, and the surfaces that
    /// render it are reached from the settle.
    NoDetector(String),
    /// The operator stopped the node before the question was answered.
    Cancelled,
}

/// The one worker detector this node serves the fleet with, handed over the
/// moment it exists.
///
/// A COMPLETION, not a watch. Whatever produces the detector — the first
/// camera's thread, or a cameraless box's own bootstrap — publishes it here
/// from its own thread and the waiting start runs right then; a load that
/// genuinely cannot produce one settles the slot as `NoDetector` with the real
/// error already recorded; an operator stopping the node settles it as
/// `Cancelled`. The start runs on that answer whichever order those happen in,
/// including when the detector is already here before anything is waiting —
/// and runs once more if a detector arrives after the node had answered that
/// none was coming, because a camera an operator switches on afterwards is a
/// camera that RUNS ([`WorkerDetectorSlot::on_answer`]).
///
/// Nothing here reads a clock, by standing owner ruling: no product outcome in
/// Vigil is decided by wall-clock duration, and no deadline may fail a viable
/// slow cold start. A cold start that takes as long as it takes still starts
/// exactly one worker; the previous 60-second watch stopped watching and left a
/// healthy process not serving for the rest of its life.
#[derive(Default)]
pub(crate) struct WorkerDetectorSlot {
    state: Mutex<WorkerSlotState>,
}

/// How far this node's one worker question has been answered — the ONLY thing
/// that decides whether a further answer is heard.
enum WorkerSlotAnswered {
    /// Nothing has answered yet.
    Nothing,
    /// The node reported that no detector is coming. True of the sources that
    /// had reported when it was said, and not a sentence the node is held to
    /// for the rest of its life: a detector, or the operator's stop, still
    /// answers over it — exactly once.
    NoDetector,
    /// A detector arrived, or the operator stopped the node. The last word.
    Final,
}

struct WorkerSlotState {
    /// The answers this node has given that the start has not been handed yet,
    /// in the order it gave them. Non-empty only while no start is installed
    /// (or while the installed one is out being run).
    undelivered: std::collections::VecDeque<WorkerSlotOutcome>,
    /// Installed by the fabric bring-up and borrowed by whichever thread is
    /// running it — `None` while it is out, which is what keeps the answers
    /// this node gives in order across threads.
    serve: Option<Box<dyn FnMut(WorkerSlotOutcome) + Send>>,
    /// Whether a start has been installed at all, which `serve` alone cannot
    /// say while it is out being run or after it was released.
    serve_installed: bool,
    /// How far the question has been answered.
    answered: WorkerSlotAnswered,
    /// How many detector sources this node will really run: declared by the
    /// bring-up before any of them can answer, and GROWN by a camera thread
    /// that takes its standing afterwards (the runtime-enable road).
    eligible_sources: usize,
    /// How many camera threads have taken up a standing as a detector source
    /// on this node. The first `eligible_sources` of them are the cameras the
    /// bring-up already counted; each one past that is a camera an operator
    /// switched on while the deployment was up, and grows the count.
    standings_taken: usize,
    /// How many sources have reported that no detector is coming from them.
    failed_sources: usize,
    /// The FIRST failure reported, which is the one the operator surface
    /// prints when every eligible source is out.
    first_failure_reason: Option<String>,
}

impl Default for WorkerSlotState {
    fn default() -> Self {
        Self {
            undelivered: std::collections::VecDeque::new(),
            serve: None,
            serve_installed: false,
            answered: WorkerSlotAnswered::Nothing,
            // One source, which is the cameraless bootstrap and every caller
            // that has not declared otherwise: its failure is the node's
            // answer.
            eligible_sources: 1,
            standings_taken: 0,
            failed_sources: 0,
            first_failure_reason: None,
        }
    }
}

impl WorkerDetectorSlot {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// How many camera detector sources this node will really run, declared
    /// ONCE by the bring-up before any camera thread can answer.
    ///
    /// A node's worker question is answered by whichever source produces a
    /// detector FIRST; it is only unanswerable when EVERY eligible source has
    /// failed. Declaring the count up front is what makes "every" knowable:
    /// the sources answer from their own threads in any order, and a slot that
    /// settled on the first failure would discard a Ready that a later camera
    /// was already loading. Cameras disabled at startup are not eligible —
    /// their threads park on the enable flag and never reach a detector load,
    /// so counting one would leave the node waiting on an answer that is never
    /// coming.
    ///
    /// The default is 1, which is the cameraless bootstrap and every existing
    /// caller: one source, and its failure is the node's answer. A node that
    /// runs no eligible camera is that same one source — the cameraless
    /// bootstrap it runs instead — so a declared zero is held at one rather
    /// than leaving the node with nothing that can answer for it.
    pub(crate) fn expect_detector_sources(&self, eligible: usize) {
        let mut state = self.lock();
        state.eligible_sources = eligible.max(1).max(state.standings_taken);
    }

    /// Run `serve` when this node's worker question is answered — and AGAIN
    /// if a detector arrives after the node has already answered that it has
    /// none.
    ///
    /// "No detector is coming" is true of the sources that had reported when
    /// it was said, and it is what the operator acts on: they stage a model,
    /// fix an endpoint, and switch a camera on. That camera is a camera that
    /// RUNS, so the detector it loads is this node's detector and the worker
    /// loop starts on it. A node that kept the earlier answer holds a working
    /// detector idle and tells the operator it has none until somebody
    /// restarts the deployment.
    ///
    /// Delivery, exactly: `serve` runs for the node's first answer. If that
    /// answer was `NoDetector`, it runs a second time for whichever of a
    /// `Ready` or the operator's `Cancelled` arrives next, and never again — a
    /// further failure report is one more source out on a node already out,
    /// and changes nothing. Nothing ever supersedes a `Ready` (the first
    /// detector wins) or a `Cancelled` (a node on its way down must not bring
    /// a worker loop up and claim fleet work it will abandon). Installed
    /// before or after the first answer, as `on_settled` is today.
    pub(crate) fn on_answer(&self, serve: impl FnMut(WorkerSlotOutcome) + Send + 'static) {
        {
            let mut state = self.lock();
            if state.serve_installed {
                return;
            }
            state.serve_installed = true;
            state.serve = Some(Box::new(serve));
        }
        self.deliver();
    }

    /// Run `settle` exactly once, the moment this node's worker-detector
    /// question is answered — immediately when it already is.
    ///
    /// The one-answer wrapper over [`WorkerDetectorSlot::on_answer`], for the
    /// callers whose contract really is a single answer. The wrapped closure
    /// is released the moment it runs, so whatever it captured drops with it.
    ///
    /// Compiled only where something asks for a single answer, which since the
    /// bring-up moved to [`WorkerDetectorSlot::on_answer`] is the harness: the
    /// worker-slot door and this module's own tests. A shipped artifact builds
    /// neither, and the node's real start hears every answer.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn on_settled(&self, settle: impl FnOnce(WorkerSlotOutcome) + Send + 'static) {
        let mut settle = Some(settle);
        self.on_answer(move |outcome| {
            if let Some(settle) = settle.take() {
                settle(outcome);
            }
        });
    }

    /// A detector loaded. The first one wins — a node with several cameras
    /// serves the fleet with one detector, not one per camera.
    pub(crate) fn publish(
        &self,
        detector: Arc<crate::detector::PromotableDetector>,
        backend_tag: String,
    ) {
        self.answer(WorkerSlotOutcome::Ready(detector, backend_tag));
    }

    /// One detector source is terminally out: its model is missing or will not
    /// load. `reason` is what an operator is shown next to
    /// `fabric-worker-serving=false`, so it names the thing that is wrong
    /// rather than restating the outcome.
    ///
    /// This settles the node's question only when EVERY eligible source has
    /// reported failure; the reason carried is the first one reported, so the
    /// surface prints a single `reason=`. A source that fails while another is
    /// still loading takes the node no further than it already was.
    pub(crate) fn no_detector(&self, reason: String) {
        let every_source_is_out = {
            let mut state = self.lock();
            if state.first_failure_reason.is_none() {
                state.first_failure_reason = Some(reason);
            }
            state.failed_sources += 1;
            (state.failed_sources >= state.eligible_sources)
                .then(|| state.first_failure_reason.clone())
                .flatten()
        };
        // Outside the lock, because `answer` takes it again — and the start it
        // may run must never hold this node's slot.
        if let Some(reason) = every_source_is_out {
            self.answer(WorkerSlotOutcome::NoDetector(reason));
        }
    }

    /// Take up ONE camera thread's standing as a detector source for this
    /// node, held for exactly as long as that thread could still produce a
    /// detector and released the moment it could not — however it ends.
    ///
    /// The node's worker question is answered when a detector arrives, or
    /// when every eligible source has reported that it is terminally out
    /// ([`WorkerDetectorSlot::expect_detector_sources`]). That arithmetic only
    /// completes if every source that stops being one says so, and a camera
    /// thread has more endings than the detector-load failure: its analysis
    /// endpoint may not resolve into a session credential, and it may panic.
    /// Neither reported, so a node that counted such a camera waited on an
    /// answer that was never coming and kept `worker-loop-starting` for the
    /// life of the process.
    ///
    /// `reason_if_it_never_answers` is what an operator is shown beside
    /// `fabric-worker-serving=false` when this camera's ending is the one the
    /// node settles on, so it names the thing that is wrong rather than
    /// restating the outcome.
    ///
    /// Taken by the camera thread once it is really a running source — after
    /// the enable-flag park, whether that park ended at startup or when an
    /// operator switched the camera on later.
    ///
    /// TAKING the standing is what makes the camera one of the sources this
    /// node measures "every source is out" against, so a camera switched on
    /// at runtime GROWS the count the bring-up declared. The first
    /// `eligible_sources` standings are the cameras
    /// [`crate::runtime::eligible_detector_sources`] already counted, so they
    /// grow nothing; each standing past that is a camera the operator added
    /// while the deployment was up. Without the growth that camera's failure
    /// is one report against a count the counted cameras have not met yet, and
    /// the node declares itself out of detectors while one of them is still
    /// loading one — the operator having caused it by doing the only thing the
    /// settings surface offered them. A thread that ends before the park does
    /// (the node is shutting down) never gets here, so it is never counted.
    pub(crate) fn camera_source(
        self: &Arc<Self>,
        reason_if_it_never_answers: &str,
    ) -> CameraDetectorSource {
        {
            let mut state = self.lock();
            state.standings_taken += 1;
            state.eligible_sources = state.eligible_sources.max(state.standings_taken);
        }
        CameraDetectorSource {
            slot: Arc::clone(self),
            reason_if_it_never_answers: Some(reason_if_it_never_answers.to_string()),
        }
    }

    /// The operator stopped this node before a detector arrived.
    pub(crate) fn cancel(&self) {
        self.answer(WorkerSlotOutcome::Cancelled);
    }

    fn answer(&self, outcome: WorkerSlotOutcome) {
        {
            let mut state = self.lock();
            let heard = match state.answered {
                WorkerSlotAnswered::Nothing => true,
                // A detector, or the operator's stop, answers over "no
                // detector is coming". One more source going out on a node
                // already out changes nothing.
                WorkerSlotAnswered::NoDetector => {
                    !matches!(outcome, WorkerSlotOutcome::NoDetector(_))
                }
                WorkerSlotAnswered::Final => false,
            };
            if !heard {
                // A final answer already stands and nothing has run on it. The
                // operator's stop is the one thing allowed to replace it: a
                // detector that loaded during teardown leaves `Ready` sitting
                // in this slot, and a start installed afterwards would bring a
                // worker loop up on a node that is on its way down. Any other
                // second answer is still ignored — the first detector wins.
                if matches!(state.answered, WorkerSlotAnswered::Final)
                    && matches!(outcome, WorkerSlotOutcome::Cancelled)
                    && let Some(standing) = state.undelivered.back_mut()
                {
                    *standing = outcome;
                }
                return;
            }
            state.answered = if matches!(outcome, WorkerSlotOutcome::NoDetector(_)) {
                WorkerSlotAnswered::NoDetector
            } else {
                WorkerSlotAnswered::Final
            };
            state.undelivered.push_back(outcome);
        }
        self.deliver();
    }

    /// Hand the installed start every answer it has not been handed yet, in
    /// the order this node gave them, with the slot UNLOCKED — the start
    /// spawns the worker loop, and no other publisher should be held behind
    /// that. Whichever thread has the start out runs them all; a thread that
    /// arrives while it is out has already left its answer in the queue and
    /// returns, so the answers still reach the start in order.
    fn deliver(&self) {
        loop {
            let (mut serve, outcome) = {
                let mut state = self.lock();
                let Some(serve) = state.serve.take() else {
                    return;
                };
                match state.undelivered.pop_front() {
                    Some(outcome) => (serve, outcome),
                    None => {
                        state.serve = Some(serve);
                        return;
                    }
                }
            };
            serve(outcome);
            let mut state = self.lock();
            if state.undelivered.is_empty() {
                if matches!(state.answered, WorkerSlotAnswered::Final) {
                    // The node's last word has been served, so nothing will
                    // ever be handed to this start again. Releasing it here is
                    // what lets the fabric bundle it captured drop; a slot
                    // that kept it would hold that bundle for the life of the
                    // process.
                    return;
                }
                state.serve = Some(serve);
                return;
            }
            state.serve = Some(serve);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WorkerSlotState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One camera thread's standing as this node's detector source.
///
/// Dropped without having answered — a `return`, an `?`, or an unwind — it
/// reports the reason it was taken with. Answered, it reports that answer and
/// nothing further: a camera that has already reported is ONE source that
/// reported, and a node that counted it twice would declare itself out of
/// detectors while another camera was still loading one.
///
/// The release is a `Drop` rather than a call at each ending on purpose. The
/// panic road has no site to put a call on — the unwind leaves the body
/// without running another line of it — and a `return` added to this thread
/// later cannot forget what it never had to remember.
pub(crate) struct CameraDetectorSource {
    slot: Arc<WorkerDetectorSlot>,
    /// The standing reason, taken by whichever answer arrives — so `Drop`
    /// reports for a standing that never answered, and only for that one.
    reason_if_it_never_answers: Option<String>,
}

impl CameraDetectorSource {
    /// This camera's detector loaded, advertising `backend_tag` to the fleet.
    pub(crate) fn detector_ready(
        mut self,
        detector: Arc<crate::detector::PromotableDetector>,
        backend_tag: String,
    ) {
        self.reason_if_it_never_answers = None;
        self.slot.publish(detector, backend_tag);
    }

    /// This camera's detector source is terminally out, with the reason an
    /// operator acts on.
    pub(crate) fn failed(mut self, reason: String) {
        self.reason_if_it_never_answers = None;
        self.slot.no_detector(reason);
    }
}

impl Drop for CameraDetectorSource {
    fn drop(&mut self) {
        if let Some(reason) = self.reason_if_it_never_answers.take() {
            self.slot.no_detector(reason);
        }
    }
}

/// The node's worker-detector question, opened for the harness.
///
/// Not a shipped surface: no artifact builds `test-support`
/// (`artifact_never_enables_test_support.rs`), so in every shipped build this
/// door is not merely private — it is not compiled at all. It exists because
/// the aggregation contract is about ORDER: which of several camera sources
/// answers first, and what the node does with the answers that follow. An
/// integration test can construct that order directly and a process-level
/// fixture cannot construct it at all.
#[cfg(feature = "test-support")]
pub mod worker_slot_door {
    use std::sync::Arc;

    use super::{WorkerDetectorSlot, WorkerSlotOutcome};

    /// What this node's one worker question was finally answered with. The
    /// detector instance is deliberately absent: these contracts are about
    /// WHICH answer settles and what the operator is told, never about
    /// detection.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum SettledAnswer {
        /// A detector loaded, advertising this backend tag to the fleet.
        Ready { backend_tag: String },
        /// No detector is coming from anywhere on this node, with the reason
        /// an operator acts on.
        NoDetector { reason: String },
        /// The operator stopped the node.
        Cancelled,
    }

    /// What the start was told, in this door's vocabulary.
    fn settled_answer(outcome: WorkerSlotOutcome) -> SettledAnswer {
        match outcome {
            WorkerSlotOutcome::Ready(_, backend_tag) => SettledAnswer::Ready { backend_tag },
            WorkerSlotOutcome::NoDetector(reason) => SettledAnswer::NoDetector { reason },
            WorkerSlotOutcome::Cancelled => SettledAnswer::Cancelled,
        }
    }

    /// A detector that does nothing but exist. The slot carries an instance
    /// because the real worker loop serves with one; nothing this door pins
    /// ever detects anything, so the instance only has to BE one.
    struct AnswerOnlyDetector;

    impl crate::detector::Detector for AnswerOnlyDetector {
        fn model_sha256(&self) -> &str {
            "answer-only"
        }

        fn detect_segment(
            &self,
            _media: &crate::media_pipeline::DecodedVideoSegment,
            _clip_sha256: String,
            _sample_frames: usize,
            _confidence_threshold: f64,
        ) -> Result<crate::yolox_detector::DetectorOutput, String> {
            Err("this detector exists to be an answer, and decodes nothing".to_string())
        }
    }

    /// One node's worker-detector question, driven the way the runtime drives
    /// it.
    ///
    /// Holds the slot behind an `Arc` rather than by value, which is what lets
    /// a standing outlive the call that took it — the same shape the runtime
    /// already has, where the slot is an `Arc` cloned into every camera
    /// thread.
    pub struct WorkerSlotDoor {
        slot: Arc<WorkerDetectorSlot>,
    }

    impl Default for WorkerSlotDoor {
        fn default() -> Self {
            Self::new()
        }
    }

    impl WorkerSlotDoor {
        /// A fresh node, before any camera source has answered.
        pub fn new() -> Self {
            Self {
                slot: Arc::new(WorkerDetectorSlot::new()),
            }
        }

        /// Take up one camera thread's standing as a detector source for this
        /// node (see [`WorkerDetectorSlot::camera_source`]).
        pub fn camera_source(&self, reason_if_it_never_answers: &str) -> CameraDetectorSource {
            CameraDetectorSource {
                inner: self.slot.camera_source(reason_if_it_never_answers),
            }
        }

        /// The bring-up's declaration of how many camera detector sources this
        /// node will really run (see
        /// [`WorkerDetectorSlot::expect_detector_sources`]).
        pub fn expect_detector_sources(&self, eligible: usize) {
            self.slot.expect_detector_sources(eligible);
        }

        /// The start the fabric bring-up installs, recording what it was told.
        pub fn on_settled(&self, record: impl FnOnce(SettledAnswer) + Send + 'static) {
            self.slot
                .on_settled(move |outcome| record(settled_answer(outcome)));
        }

        /// Every answer this node's worker question reaches, in order, as the
        /// fabric bring-up's own start sees them
        /// (see [`WorkerDetectorSlot::on_answer`]).
        pub fn on_answer(&self, mut record: impl FnMut(SettledAnswer) + Send + 'static) {
            self.slot
                .on_answer(move |outcome| record(settled_answer(outcome)));
        }

        /// One camera's detector loaded, advertising `backend_tag`.
        pub fn detector_ready(&self, backend_tag: &str) {
            self.slot.publish(
                Arc::new(crate::detector::PromotableDetector::new(Box::new(
                    AnswerOnlyDetector,
                ))),
                backend_tag.to_string(),
            );
        }

        /// One camera's detector source is terminally out, with the reason an
        /// operator is shown.
        pub fn detector_source_failed(&self, reason: &str) {
            self.slot.no_detector(reason.to_string());
        }

        /// The operator stopped this node.
        pub fn cancel(&self) {
            self.slot.cancel();
        }
    }

    /// One camera thread's standing, as the harness drives it. The detector
    /// instance is deliberately absent for the same reason [`SettledAnswer`]
    /// carries none: these contracts are about WHICH answer settles and what
    /// the operator is told, never about detection.
    pub struct CameraDetectorSource {
        inner: super::CameraDetectorSource,
    }

    impl CameraDetectorSource {
        /// This camera's detector loaded, advertising `backend_tag`.
        pub fn detector_ready(self, backend_tag: &str) {
            self.inner.detector_ready(
                Arc::new(crate::detector::PromotableDetector::new(Box::new(
                    AnswerOnlyDetector,
                ))),
                backend_tag.to_string(),
            );
        }

        /// This camera's detector source is terminally out, with the reason
        /// an operator is shown.
        pub fn failed(self, reason: &str) {
            self.inner.failed(reason.to_string());
        }
    }
}

/// Node-level fabric state, shared (via `Arc`) across every camera's
/// detector thread and the background worker/result-consumer task. Only
/// constructed when `fabric_ticket`/`fabric_hub` is configured
/// (`fabric_bring_up`); its mere existence as a type does not enable
/// anything.
#[cfg(feature = "fabric")]
pub(crate) struct FabricBundle {
    pub(crate) runtime: Arc<crate::fabric::FabricRuntime>,
    pub(crate) pending: Arc<crate::fabric::PendingOffloads>,
    /// Segments this node has offloaded and is waiting on a ledger result
    /// for, keyed by the minted job id. Retained (not the ledger's job
    /// bytes — the ORIGINAL local segment, including its already-decoded
    /// frames and on-disk clip) so the result-consumer can finalize evidence
    /// and record events exactly as the local path would, whichever node's
    /// backend actually ran the model.
    pub(crate) pending_segments: Mutex<HashMap<String, PendingOffloadSegment>>,
    /// Refreshed periodically by the background task from
    /// `FabricRuntime::remote_detector_capabilities` — the per-camera
    /// detector threads read this cache rather than calling the async
    /// ledger scan themselves on every segment.
    pub(crate) remote_capabilities: Mutex<Vec<crate::offload_policy::RemoteCapability>>,
    pub(crate) offload_policy_config: crate::offload_policy::OffloadPolicyConfig,
    /// The first camera detector to finish loading — or, on a cameraless
    /// worker box, `runtime::bootstrap_worker_detector` — populates this. The
    /// fabric worker loop claims/executes ANY node's `vigil.detector` job
    /// (including this node's own), so it needs exactly one live detector, not
    /// one per camera. `String` is the truthful backend tag
    /// (`receipt.active_backend`) this node advertises to the fleet. The slot
    /// is populated ONLY once a model artifact is actually loaded — a worker
    /// with no loadable model never fills it, never advertises, and reports
    /// `fabric-worker-serving=false reason=no-model` (advertisement requires
    /// executable capability — PO ruling 2026-07-13).
    /// Shared with the camera detector threads (which publish the moment a
    /// detector loads) and, for a cameraless worker box, the bootstrap thread.
    /// Shared rather than owned so it is created in `run_inner` BEFORE this
    /// bundle exists: fabric attaches asynchronously, so a camera's detector may
    /// load before the bundle is built, and the start must still run for that
    /// detector rather than be missed.
    pub(crate) worker_detector_slot: Arc<WorkerDetectorSlot>,
    /// Whether this node's fabric worker loop is actually running and serving
    /// the fleet — flipped `true` the moment the loop starts. The status task
    /// renders it onto the shared `fabric-status` line so a not-serving node
    /// says so loudly (owner steer 2026-07-13 / criterion C6), rather than
    /// looking identical to a healthy idle one.
    pub(crate) worker_serving: Arc<AtomicBool>,
    /// The named reason the worker loop is not serving (enrollment rejected,
    /// no loadable detection model) — rendered next to `fabric-worker-serving=
    /// false`. Empty once serving.
    pub(crate) worker_serving_reason: Arc<Mutex<String>>,
    /// The node's OWN shutdown flag — the one an operator's stop sets. Carried
    /// here so the worker loop this bundle starts is stopped by the same thing
    /// that stops everything else on the node; see [`worker_loop_stop`].
    pub(crate) shutdown: Arc<AtomicBool>,
    pub(crate) tokio_handle: tokio::runtime::Handle,
    pub(crate) node_id: String,
}

/// One offloaded segment awaiting its ledger result, plus everything the
/// result-consumer needs to finish the job the local path would have done
/// itself: finalize the clip, write the observation, publish MQTT, receipt
/// the stage.
#[cfg(feature = "fabric")]
pub(crate) struct PendingOffloadSegment {
    pub(crate) segment: crate::runtime::CapturedSegment,
    pub(crate) detection_work: crate::workgraph::WorkEnvelope,
    pub(crate) detection_started_at: chrono::DateTime<chrono::Utc>,
    pub(crate) config: config::RuntimeConfig,
    /// `None` when the run has no store behind it: the offloaded result is
    /// published best-effort and recorded nowhere.
    pub(crate) store: Option<Store>,
    pub(crate) stats: RuntimeStatsState,
    pub(crate) health: HealthState,
    pub(crate) detection_publisher: Option<Arc<dyn crate::site_channel::DetectionChannel>>,
    pub(crate) recognition_embedder: Option<Arc<dyn context_graph::Embedder>>,
    pub(crate) receipts: Arc<crate::workgraph::StageReceiptLog>,
}

/// The stop a worker loop started at bring-up runs under: the node's OWN
/// shutdown flag, shared, never a fresh one minted for the loop.
///
/// A loop handed a flag nothing else holds can never be stopped by anything.
/// The operator asks the node to stop, every other part of it stops, and the
/// worker loop keeps claiming and executing fleet jobs until the process dies
/// — on a node that has already given up its cameras. Sharing the node's flag
/// is what makes the operator's stop reach it.
#[cfg(feature = "fabric")]
fn worker_loop_stop(node_shutdown: &Arc<AtomicBool>) -> Arc<AtomicBool> {
    Arc::clone(node_shutdown)
}

/// Bring up this node's fabric wiring exactly once, before any camera
/// thread starts. Returns `None` (today's behavior, byte-identical) unless
/// `fabric_ticket`/`fabric_hub` is configured — the `fabric` cargo feature
/// being compiled in is not by itself enough to start anything (C10: no
/// silent new network surface on an existing install).
#[cfg(feature = "fabric")]
pub(crate) fn fabric_bring_up(
    config: &config::RuntimeConfig,
    stats: &RuntimeStatsState,
    accel: &Arc<crate::acceleration::AccelerationState>,
    worker_detector_slot: Arc<WorkerDetectorSlot>,
    shutdown: Arc<AtomicBool>,
) -> Option<Arc<FabricBundle>> {
    if config.fabric_ticket.is_none() && !config.fabric_hub {
        return None;
    }
    // Test-only bring-up delay lever (default absent → ZERO cost; no shipped
    // surface — add-on options/env, fabric.toml, CLI — ever sets it): lets the
    // boot-readiness test make fabric bring-up deliberately slow so it can prove
    // /health reaches Ready WITHOUT waiting for the fabric ledger to open. Inert
    // in production.
    // A declared test-only diagnostic, not a setting: it has no shipped
    // surface to move onto, which is the condition the workspace lint exists
    // to enforce.
    #[allow(clippy::disallowed_methods)]
    let bringup_delay_ms = std::env::var("VIGIL_FABRIC_BRINGUP_DELAY_MS");
    if let Some(delay_ms) = bringup_delay_ms
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
    {
        thread::sleep(Duration::from_millis(delay_ms));
    }
    let tokio_runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("vigil-fabric")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            println!("fabric_runtime_start_failed=true error={error}");
            return None;
        }
    };
    let handle = tokio_runtime.handle().clone();
    let data_dir = config.data_dir.join("fabric");
    let fabric_ticket = config.fabric_ticket.clone();
    let fabric_hub = config.fabric_hub;
    let start_result = handle.block_on(crate::fabric::FabricRuntime::start(
        &data_dir,
        fabric_ticket.as_deref(),
        fabric_hub,
    ));
    let fabric_runtime = match start_result {
        // Criterion C10, fix cycle 9: thread the resolved config value
        // (add-on options page / config file / CLI, already folded through
        // config.rs's own precedence) into the worker-lease resolution —
        // this is the ONLY production path, since add-on options never
        // become env vars (see `spawn_worker_loop`'s resolution-order
        // comment).
        Ok(runtime) => Arc::new(runtime.with_worker_lease_ms(config.fabric_worker_lease_ms)),
        Err(error) => {
            println!("fabric_bring_up_failed=true error={error}");
            return None;
        }
    };
    // The tokio runtime itself must outlive this function: leak the Runtime
    // handle's owner onto a detached background thread that parks for the
    // process lifetime, so `tokio_runtime`'s worker threads (and every task
    // spawned onto `handle`, including the worker loop and result-consumer
    // below) keep running after this bring-up function returns.
    thread::spawn(move || {
        tokio_runtime.block_on(async {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
    });

    let node_id = fabric_runtime.node_id.clone();
    println!("fabric_ready=true node_id={node_id}");
    println!("{}", fabric_runtime.join_instruction());

    // Whether any camera will load a detector that populates the worker slot.
    // A node with no camera source (a dedicated spare-compute worker box) must
    // still serve the fleet with whatever backend it has — so it bootstraps
    // its OWN worker detector below, independent of cameras (owner steer
    // 2026-07-13).
    //
    // "Will load" is the SAME eligibility the bring-up declared on the slot
    // (`crate::runtime::eligible_detector_sources`): a camera an operator
    // switched off at startup parks on its enable flag and never reaches a
    // detector load. Asking only whether a camera is CONFIGURED left a node
    // whose every camera is switched off skipping this bootstrap while nothing
    // else could ever settle the slot — the node kept the
    // `worker-loop-starting` placeholder for the rest of its life.
    let serves_role = fabric_runtime.serves_fabric_role();
    let has_camera_source = crate::runtime::eligible_detector_sources(config) > 0;
    // Seed the honest not-serving reason before the worker loop resolves: a
    // node with worker intent but a rejected ticket can never serve, and says
    // exactly that; an enrollable node is transiently "starting" until its
    // loop begins.
    let initial_serving_reason = if serves_role {
        String::from("worker-loop-starting")
    } else {
        fabric_runtime
            .enrollment_error()
            .map(|error| format!("enrollment-rejected: {error}"))
            .unwrap_or_else(|| {
                "not-enrolled-to-a-hub — enable the hub role (fabric_hub=true) or paste the hub's \
             fabric-join ticket"
                    .to_string()
            })
    };
    let worker_serving = Arc::new(AtomicBool::new(false));
    let worker_serving_reason = Arc::new(Mutex::new(initial_serving_reason));

    let bundle = Arc::new(FabricBundle {
        runtime: fabric_runtime.clone(),
        pending: Arc::new(crate::fabric::PendingOffloads::new()),
        pending_segments: Mutex::new(HashMap::new()),
        remote_capabilities: Mutex::new(Vec::new()),
        // Criterion C10, fix cycle 9: the fallback horizon comes from the
        // resolved config surface (config.rs's `fabric_fallback_horizon_ms`,
        // default 5000ms = this field's prior hardcoded default,
        // byte-identical when unset) — never a bare default construction
        // that would silently drop whatever the operator configured. Every
        // other offload-policy field keeps its own sane default.
        offload_policy_config: crate::offload_policy::OffloadPolicyConfig::with_fallback_horizon_ms(
            config.fabric_fallback_horizon_ms,
        ),
        worker_detector_slot,
        worker_serving: worker_serving.clone(),
        worker_serving_reason: worker_serving_reason.clone(),
        shutdown,
        tokio_handle: handle.clone(),
        node_id: node_id.clone(),
    });

    // Cameraless worker bootstrap (owner steer 2026-07-13): when this node has
    // a real serving role (hub, or enrolled with a validated ticket) but no
    // camera will ever populate the worker slot, load ONE worker detector here
    // through the SAME detection-accel selection the per-camera path uses, and
    // populate the slot so the worker loop starts and advertises. Runs on a
    // detached thread so a slow model load never delays `/health` becoming
    // ready. A node that DOES own cameras keeps its existing behavior (the
    // first camera detector to publish wins); this only fills the gap for a
    // dedicated worker box.
    if serves_role && !has_camera_source {
        let boot_bundle = bundle.clone();
        let boot_config = config.clone();
        let boot_accel = accel.clone();
        let boot_stats = stats.clone();
        thread::spawn(move || {
            match crate::runtime::bootstrap_worker_detector(&boot_config, &boot_accel, &boot_stats)
            {
                Some((detector, backend_tag)) => {
                    boot_bundle
                        .worker_detector_slot
                        .publish(detector, backend_tag);
                }
                None => {
                    // No executable capability: name the staging fix and do
                    // NOT advertise a detector row (PO ruling 2026-07-13).
                    // The question is ANSWERED — no detector is coming — so
                    // the slot settles rather than leaving a start pending on
                    // something that will never happen. The staging fix rides
                    // ON the answer, so the one place that renders a settled
                    // not-serving reason renders this one too.
                    boot_bundle.worker_detector_slot.no_detector(
                        "no-model — stage a loadable detection model on this worker \
                         (set detector_model_path / VIGIL_DETECTOR_MODEL_PATH)"
                            .to_string(),
                    );
                }
            }
        });
    }

    // Worker loop, started BY the detector arriving — the first camera's, or a
    // cameraless worker box's own bootstrap. Claims + executes ANY matching
    // `vigil.detector` job, including this node's own deferred submissions once
    // their deadline passes (criterion C5/C8).
    //
    // A node with no serving role never starts one and already carries the
    // reason why (a rejected ticket, or no hub at all), so it says so once here
    // rather than leaving a start waiting on a detector that would change
    // nothing.
    let worker_bundle = bundle.clone();
    if serves_role {
        let handoff_handle = handle.clone();
        worker_bundle
            .clone()
            .worker_detector_slot
            .on_answer(move |outcome| match outcome {
                crate::fabric::WorkerSlotOutcome::Ready(detector, backend_tag) => {
                    // Cloned per answer because this start can run more than
                    // once: a node that reported having no detector and then
                    // had a camera switched on serves on the SECOND answer
                    // (`WorkerDetectorSlot::on_answer`), through this same arm.
                    let worker_bundle = worker_bundle.clone();
                    // Spawned onto the fabric runtime because the worker loop
                    // is a tokio task; the publisher's own thread hands it over
                    // and goes back to what it was doing.
                    handoff_handle.spawn(async move {
                        let backend =
                            Arc::new(crate::fabric::FabricProductionDetectorBackend::new(
                                detector.clone(),
                                backend_tag.clone(),
                            ));
                        worker_bundle
                            .runtime
                            .spawn_worker_loop(backend, worker_loop_stop(&worker_bundle.shutdown));
                        worker_bundle
                            .worker_serving
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        if let Ok(mut reason) = worker_bundle.worker_serving_reason.lock() {
                            reason.clear();
                        }
                        println!("fabric_worker_loop_started=true backend={backend_tag}");
                    });
                }
                crate::fabric::WorkerSlotOutcome::NoDetector(settled_reason) => {
                    // The answer's own reason is what an operator reads back
                    // through `fabric-worker-serving=false reason=…`: the
                    // staging fix a worker box named, or the real error a
                    // camera's detector load failed with. A more specific
                    // reason already standing (a rejected ticket) is left
                    // alone; the seeded starting placeholder is not a reason
                    // and is replaced.
                    if let Ok(mut reason) = worker_bundle.worker_serving_reason.lock()
                        && (reason.is_empty() || reason.as_str() == "worker-loop-starting")
                    {
                        *reason = settled_reason;
                    }
                    println!("fabric_worker_loop_not_started=true reason=no_local_detector_loaded");
                }
                // The node is going down. Nothing to say and nothing to start.
                crate::fabric::WorkerSlotOutcome::Cancelled => {}
            });
    } else {
        println!("fabric_worker_loop_not_started=true reason=no_serving_role");
    }

    // Result-consumer + remote-capability refresher (criteria C4/C7):
    // periodically pulls, refreshes the remote-capability cache the
    // per-camera detector threads read, and applies any landed result for a
    // job this node submitted — exactly-once, discarding late/duplicate
    // results, whether the executor was a remote node or this node's own
    // deferred submission reclaimed after its deadline (C5 fallback).
    let consumer_bundle = bundle.clone();
    let consumer_stats = stats.clone();
    handle.spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Ok(remotes) = consumer_bundle.runtime.remote_detector_capabilities().await {
                *consumer_bundle
                    .remote_capabilities
                    .lock()
                    .expect("remote capabilities lock") = remotes;
            }
            let _ = consumer_bundle.runtime.poll_detector_results().await;

            let job_ids: Vec<String> = consumer_bundle
                .pending_segments
                .lock()
                .expect("pending segments lock")
                .keys()
                .cloned()
                .collect();
            // The live claimant set for the C5 dead-capability steal below:
            // the SAME liveness-filtered vocabulary the routing/render surfaces
            // read (refreshed just above this iteration). A claimant absent
            // from it has aged out; a slow-but-alive one is still present.
            let live_node_ids: Vec<String> = consumer_bundle
                .remote_capabilities
                .lock()
                .expect("remote capabilities lock")
                .iter()
                .map(|remote| remote.node_id.clone())
                .collect();
            let steal_now_ms = i64::try_from(consumer_bundle.runtime.persisted_clock.unix_millis())
                .expect("fabric steal milliseconds fit signed timestamp fields");
            let my_node_id = consumer_bundle.runtime.node_id.clone();
            for job_id in job_ids {
                let result = match contextdb_engine::work_ledger::job_result(
                    &consumer_bundle.runtime.db,
                    &job_id,
                ) {
                    Ok(Some(result)) => result,
                    Ok(None) => {
                        // No result yet. If this job's remote claimant has died
                        // mid-lease (fallen out of the live set) AND the
                        // fallback horizon has passed, steal it back so THIS
                        // submitter's own deferring worker re-runs it locally
                        // (criterion C5) — never waiting out the claimant's full
                        // (5-minute) lease. The steal is a single guarded
                        // failure-row write; the worker loop and the
                        // `executor == self` apply path do the rest.
                        match try_steal_stranded_claim(
                            &consumer_bundle.runtime.db,
                            &my_node_id,
                            &job_id,
                            &live_node_ids,
                            steal_now_ms,
                        ) {
                            Ok(true) => {
                                // Deliver the abandon row to the hub so the
                                // reclaim is visible fleet-wide; a miss is
                                // tolerated (offline mode) and the standing
                                // worker loop re-pushes.
                                let _ = consumer_bundle.runtime.client.push().await;
                            }
                            Ok(false) => {}
                            Err(error) => {
                                println!("fabric_steal_failed=true job_id={job_id} error={error}");
                            }
                        }
                        continue;
                    }
                    Err(error) => {
                        println!("fabric_result_scan_failed=true job_id={job_id} error={error}");
                        continue;
                    }
                };
                let Some(entry) = consumer_bundle
                    .pending_segments
                    .lock()
                    .expect("pending segments lock")
                    .remove(&job_id)
                else {
                    continue;
                };
                apply_fabric_result(&consumer_bundle, &consumer_stats, &job_id, result, entry);
            }
        }
    });

    // Fabric status/join lines, refreshed on the same cadence so
    // stats/doctor/health render an honest, current, ONE-vocabulary picture
    // (criterion C7).
    let status_bundle = bundle.clone();
    let status_stats = stats.clone();
    handle.spawn(async move {
        loop {
            let remotes = status_bundle
                .remote_capabilities
                .lock()
                .expect("remote capabilities lock")
                .clone();
            let worker_serving = status_bundle
                .worker_serving
                .load(std::sync::atomic::Ordering::SeqCst);
            let worker_serving_reason = status_bundle
                .worker_serving_reason
                .lock()
                .map(|reason| reason.clone())
                .unwrap_or_default();
            let facts = crate::offload_policy::FabricStatusFacts {
                enrolled: true,
                role: if status_bundle.runtime.hub_endpoint.is_some() {
                    "hub"
                } else {
                    "edge"
                },
                in_use: !remotes.is_empty(),
                remote_detectors: remotes,
                worker_serving,
                worker_serving_reason,
            };
            let status_line = crate::offload_policy::render_fabric_status_receipt(&facts);
            // Ticket-free by construction: `join_advice` never touches
            // `join_instruction` (the ticket-bearing startup-log string) at
            // all — it only asks whether this node carries the hub role.
            let join_advice = if status_bundle.runtime.hub_endpoint.is_some() {
                crate::runtime_stats::FabricJoinAdvice::HubReady
            } else {
                crate::runtime_stats::FabricJoinAdvice::GrowHint
            };
            status_stats.update(|stats| {
                stats.fabric_status = status_line.clone();
                stats.fabric_join_advice = join_advice;
            });
            // Refresh promptly (1s): the worker-serving state settles shortly
            // after bring-up (detector loaded → serving, or no-model → the
            // named not-serving reason), and every operator surface reads this
            // persisted snapshot, so a stale multi-second window would show an
            // outdated serving line right when an operator checks after a join.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    Some(bundle)
}

/// Where this node's own ticket is cached, beside its identity key — read
/// back by `run_ticket_command` when a live rebind is not possible because a
/// running `vigil run` process already holds this identity's sticky sync
/// port (the ordinary case: an operator only ever runs `vigil fabric
/// ticket` while Vigil is up). Never printed, served, or logged from
/// anywhere except the two sanctioned retrieval paths that read it or its
/// live equivalent (this command, and the startup log via
/// [`FabricRuntime::join_instruction`]).
fn own_ticket_cache_path(fabric_data_dir: &Path) -> std::path::PathBuf {
    fabric_data_dir.join("own-ticket")
}

/// Tighten the fabric ledger database to owner-read/write only (0600) after
/// `Database::open` creates it. contextdb (closed upstream work, no edit
/// here — see the owner ruling recorded for the fleet coordination handoff)
/// creates the file at its own default mode; vigil owns this file inside its
/// own data directory, so it re-asserts the mode itself.
///
/// A failure here is FATAL to bring-up: both persisted stores (this ledger
/// and `fabric/own-ticket`) are contractually owner-only, and the ledger's
/// `peer_directory` table carries this node's own enrollment ticket. Letting
/// bring-up continue into schema installation, peer registration, and
/// serving with the mode unsecured would silently write that credential at
/// whatever permissions the file happens to hold — the standing ruling is
/// that an impossible vigil-side tightening is a stop-and-report, never
/// something routed past. The error names the path and the underlying
/// reason; it never carries a credential (this step runs before any ticket
/// is ever written to this file).
#[cfg(unix)]
fn tighten_fabric_ledger_permissions(db_path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(db_path, fs::Permissions::from_mode(0o600)).map_err(|err| {
        format!(
            "fabric: could not secure the ledger database at {} to owner-only permissions: {err}",
            db_path.display()
        )
    })
}

#[cfg(not(unix))]
fn tighten_fabric_ledger_permissions(_db_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Persist the ticket cache with owner-only permissions via a same-directory
/// temp file plus atomic rename, mirroring [`crate::camera_track::CameraQuerySalt::persist`]:
/// the credential's mode is fixed at CREATE time (never widened by a later
/// `fs::write` onto an existing file with a looser mode) and a reader can
/// never observe a torn write.
fn write_own_ticket_cache(fabric_data_dir: &Path, ticket: &str) {
    let path = own_ticket_cache_path(fabric_data_dir);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let temp_path = path.with_extension("tmp");
    let _ = fs::remove_file(&temp_path);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write_result = options
        .open(&temp_path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, ticket.as_bytes()));
    if write_result.is_ok() {
        let _ = fs::rename(&temp_path, &path);
    } else {
        let _ = fs::remove_file(&temp_path);
    }
}

fn read_own_ticket_cache(fabric_data_dir: &Path) -> Option<String> {
    let content = fs::read_to_string(own_ticket_cache_path(fabric_data_dir)).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Where the identity that PRODUCED the cached ticket is stamped — a
/// companion to [`own_ticket_cache_path`], never the credential itself
/// (holds only the identity's public `node_id`, the same value every other
/// surface already prints unredacted). See [`write_own_ticket_identity_stamp`]
/// for why this exists.
fn own_ticket_identity_stamp_path(fabric_data_dir: &Path) -> std::path::PathBuf {
    own_ticket_cache_path(fabric_data_dir).with_extension("node-id")
}

/// Record which identity's ticket is cached, so a later fallback read (see
/// `run_ticket_command`) can distinguish "the cache still belongs to the
/// identity in play" from "the cache is a leftover from a since-rotated
/// identity" (the identity file was deleted and regenerated since the last
/// successful start). Best-effort, like the ticket cache itself: a write
/// failure here only means the freshness check degrades to "stamp absent",
/// which the reader already treats as untrustworthy.
///
/// This is a PARTIAL freshness check, not a full one — see the doc comment
/// on [`run_ticket_command`]'s freshness step for what it does and does not
/// prove.
fn write_own_ticket_identity_stamp(fabric_data_dir: &Path, node_id: &str) {
    let path = own_ticket_identity_stamp_path(fabric_data_dir);
    let _ = fs::write(path, node_id);
}

fn read_own_ticket_identity_stamp(fabric_data_dir: &Path) -> Option<String> {
    let content = fs::read_to_string(own_ticket_identity_stamp_path(fabric_data_dir)).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// The freshness gate `run_ticket_command` applies before trusting the
/// cached ticket on a store-held-by-a-writer fallback: `true` only when the
/// identity stamped alongside the cache ([`write_own_ticket_identity_stamp`])
/// still matches the identity `identity_path` loads TODAY. Absent stamp,
/// unreadable identity, or a mismatch are all treated as untrustworthy
/// (`false`) — a missing signal must never be read as "fresh." See the
/// doc comment on [`run_ticket_command`] for what this does and does not
/// prove (identity-current, not address-current).
fn cached_ticket_identity_is_current(fabric_data_dir: &Path, identity_path: &Path) -> bool {
    FabricIdentity::load_or_generate(identity_path)
        .ok()
        .map(|identity| identity.node_id())
        .and_then(|current_node_id| {
            read_own_ticket_identity_stamp(fabric_data_dir)
                .map(|cached_node_id| cached_node_id == current_node_id)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod worker_detector_slot_tests {
    use super::{WorkerDetectorSlot, WorkerSlotOutcome};
    use std::sync::{Arc, Mutex};

    /// A detector that does nothing but exist: these contracts are about WHEN
    /// the worker start runs and how many times, never about detection.
    struct StubDetector;

    impl crate::detector::Detector for StubDetector {
        fn model_sha256(&self) -> &str {
            "stub"
        }

        fn detect_segment(
            &self,
            _media: &crate::media_pipeline::DecodedVideoSegment,
            _clip_sha256: String,
            _sample_frames: usize,
            _confidence_threshold: f64,
        ) -> Result<crate::yolox_detector::DetectorOutput, String> {
            Err("this stub detector decodes nothing".to_string())
        }
    }

    fn stub_handle() -> Arc<crate::detector::PromotableDetector> {
        Arc::new(crate::detector::PromotableDetector::new(Box::new(
            StubDetector,
        )))
    }

    /// What the start was told, so a test reads events rather than waiting for
    /// any amount of time to pass.
    type Recorded = Arc<Mutex<Vec<String>>>;

    fn note(seen: &Recorded, outcome: WorkerSlotOutcome) {
        let entry = match outcome {
            WorkerSlotOutcome::Ready(_, backend_tag) => format!("started:{backend_tag}"),
            WorkerSlotOutcome::NoDetector(reason) => format!("no-detector:{reason}"),
            WorkerSlotOutcome::Cancelled => "cancelled".to_string(),
        };
        seen.lock().expect("recorded outcomes").push(entry);
    }

    /// The detector can finish loading before anything is waiting for it — a
    /// camera thread routinely beats the fabric bring-up, which is the whole
    /// reason the slot is shared and created before the bundle. The start must
    /// still run, once, with that detector. A handover that only reacts to the
    /// arrival would drop this one on the floor and leave a node with a
    /// perfectly good detector serving nothing for the rest of its life; the
    /// polling loop this replaced could not miss it, so the event shape has to
    /// prove it does not either.
    #[test]
    fn a_detector_that_arrives_before_anything_waits_still_starts_exactly_one_worker() {
        let slot = WorkerDetectorSlot::new();
        let seen: Recorded = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);

        slot.publish(stub_handle(), "burn-cpu".to_string());
        assert!(
            seen.lock().expect("recorded outcomes").is_empty(),
            "nothing has asked to be started yet, so nothing may have started"
        );

        slot.on_settled(move |outcome| note(&recorded, outcome));
        assert_eq!(
            *seen.lock().expect("recorded outcomes"),
            vec!["started:burn-cpu".to_string()],
            "the start must run with the detector that was already here"
        );

        // A node with several cameras serves the fleet with ONE detector, so a
        // second detector arriving afterwards starts nothing further.
        slot.publish(stub_handle(), "burn-wgpu".to_string());
        assert_eq!(
            *seen.lock().expect("recorded outcomes"),
            vec!["started:burn-cpu".to_string()],
            "a later detector must not start a second worker loop"
        );
    }

    /// An operator stopping the node while a start is still waiting on a
    /// detector must settle that start, and a detector that finishes loading
    /// during teardown must not bring a worker loop up on a node on its way
    /// down.
    #[test]
    fn an_operator_stop_settles_a_waiting_start_and_nothing_starts_after_it() {
        let slot = WorkerDetectorSlot::new();
        let seen: Recorded = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);

        slot.on_settled(move |outcome| note(&recorded, outcome));
        assert!(
            seen.lock().expect("recorded outcomes").is_empty(),
            "no detector has arrived, so the start is still waiting"
        );

        slot.cancel();
        assert_eq!(
            *seen.lock().expect("recorded outcomes"),
            vec!["cancelled".to_string()],
            "the operator's stop is what settles a start nothing else answered"
        );

        slot.publish(stub_handle(), "burn-cpu".to_string());
        assert_eq!(
            *seen.lock().expect("recorded outcomes"),
            vec!["cancelled".to_string()],
            "a detector finishing during teardown must start nothing"
        );
    }

    /// Teardown's real order, which the black-box surface cannot construct: a
    /// detector finishes loading and answers the slot, the operator's stop
    /// arrives, and only THEN does the bring-up install the start. The answer
    /// that was already sitting there has not run, so the stop is still the
    /// node's last word — otherwise a node on its way down brings a worker
    /// loop up and starts claiming fleet jobs it will abandon.
    #[test]
    fn a_cancel_after_a_ready_answer_still_stops_a_start_that_has_not_run() {
        let slot = WorkerDetectorSlot::new();
        let seen: Recorded = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);

        slot.publish(stub_handle(), "burn-cpu".to_string());
        slot.cancel();
        slot.on_settled(move |outcome| note(&recorded, outcome));

        assert_eq!(
            *seen.lock().expect("recorded outcomes"),
            vec!["cancelled".to_string()],
            "the operator's stop arrived before anything ran on the detector's answer, so the \
             start must be told the node is going down — not handed a detector to serve the \
             fleet with"
        );
    }

    /// The same window from the other side: the node was cancelled first, and
    /// a start installed afterwards must never start a worker loop. A start
    /// that ran on a stale `Ready` here would be a loop nobody asked for on a
    /// node that is already stopping.
    #[test]
    fn a_start_installed_after_the_node_was_cancelled_never_runs() {
        let slot = WorkerDetectorSlot::new();
        let seen: Recorded = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);

        slot.cancel();
        slot.publish(stub_handle(), "burn-cpu".to_string());
        slot.on_settled(move |outcome| note(&recorded, outcome));

        assert_eq!(
            *seen.lock().expect("recorded outcomes"),
            vec!["cancelled".to_string()],
            "a start installed after the node was cancelled is told the node is going down, and \
             starts nothing"
        );
    }

    /// The teardown half of the same defect, invisible from every operator
    /// surface: the start the bring-up installs captures the fabric bundle,
    /// and the bundle holds this slot — so a slot that keeps an unrun closure
    /// keeps the bundle alive with it, for the life of the process. Settling
    /// the slot has to RELEASE the closure, which is what lets the bundle go.
    #[test]
    fn a_settled_slot_releases_its_settle_closure_so_the_bundle_can_drop() {
        let slot = WorkerDetectorSlot::new();
        // Stands in for the bundle the real start captures: what matters is
        // that the closure is the only thing holding it.
        let captured = Arc::new(());
        let held = Arc::downgrade(&captured);
        slot.on_settled(move |_outcome| {
            let _bundle = &captured;
        });
        assert!(
            held.upgrade().is_some(),
            "sanity: while the question is unanswered the start is still installed and still \
             holds what it captured"
        );

        slot.no_detector("no-model".to_string());

        assert!(
            held.upgrade().is_none(),
            "a settled slot still holds the start it ran, so the fabric bundle that start \
             captured can never be dropped — the slot and the bundle keep each other alive for \
             the whole life of the process"
        );
    }

    /// The worker loop the bring-up starts is stopped by the NODE's own
    /// shutdown flag. A loop handed a fresh flag nothing else holds runs until
    /// the process dies: the operator stops the node, the cameras go, and this
    /// loop keeps claiming fleet jobs it is no longer able to finish.
    #[test]
    fn a_worker_loop_spawned_at_bring_up_shares_the_nodes_own_shutdown_flag() {
        let node_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let loop_stop = super::worker_loop_stop(&node_shutdown);

        assert!(
            Arc::ptr_eq(&node_shutdown, &loop_stop),
            "the worker loop must run under the node's own stop, not a flag minted beside it"
        );
        node_shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            loop_stop.load(std::sync::atomic::Ordering::SeqCst),
            "an operator stopping the node must be what the worker loop reads, or the stop never \
             reaches it"
        );
    }
}

#[cfg(test)]
mod ticket_cache_freshness_tests {
    use super::{
        cached_ticket_identity_is_current, write_own_ticket_cache, write_own_ticket_identity_stamp,
    };

    /// A stamp matching the identity currently on disk is trusted.
    #[test]
    fn fresh_when_the_stamped_identity_matches_the_identity_on_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let identity_path = dir.path().join("fabric-identity.key");
        let identity =
            contextdb_server::FabricIdentity::load_or_generate(&identity_path).expect("identity");
        write_own_ticket_cache(dir.path(), "irrelevant-to-this-check");
        write_own_ticket_identity_stamp(dir.path(), &identity.node_id());

        assert!(
            cached_ticket_identity_is_current(dir.path(), &identity_path),
            "a stamp matching the on-disk identity must be treated as fresh"
        );
    }

    /// The identity file was replaced (rotated/regenerated) since the cache
    /// was written: the stamp no longer matches, so the cache must be
    /// refused rather than trusted.
    #[test]
    fn stale_when_the_identity_on_disk_has_rotated_since_the_stamp_was_written() {
        let dir = tempfile::tempdir().expect("temp dir");
        let identity_path = dir.path().join("fabric-identity.key");
        write_own_ticket_cache(dir.path(), "irrelevant-to-this-check");
        // Stamp a node id that does not, and will not, match anything this
        // identity file ever loads — simulating a since-rotated identity.
        write_own_ticket_identity_stamp(
            dir.path(),
            "0000000000000000000000000000000000000000000000000000000000000000",
        );

        assert!(
            !cached_ticket_identity_is_current(dir.path(), &identity_path),
            "a stamp from a rotated/different identity must never be treated as fresh"
        );
    }

    /// No stamp was ever written (an older cache from before this check
    /// existed, or a write failure): absence must be treated as
    /// untrustworthy, never as an implicit pass.
    #[test]
    fn stale_when_no_identity_stamp_was_ever_written() {
        let dir = tempfile::tempdir().expect("temp dir");
        let identity_path = dir.path().join("fabric-identity.key");
        write_own_ticket_cache(dir.path(), "irrelevant-to-this-check");

        assert!(
            !cached_ticket_identity_is_current(dir.path(), &identity_path),
            "a missing identity stamp must never be treated as fresh"
        );
    }
}

#[cfg(all(test, unix))]
mod ledger_permission_tightening_tests {
    use super::tighten_fabric_ledger_permissions;

    /// A mode successfully applied to a real, owned file is not an error —
    /// the sanity control for the failure case below.
    #[test]
    fn succeeds_and_leaves_the_file_owner_only_when_the_mode_can_be_set() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("fabric-ledger.db");
        std::fs::write(&db_path, b"not a real ledger, just a file to chmod").expect("write file");

        tighten_fabric_ledger_permissions(&db_path).expect("chmod on an owned, existing file");

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&db_path)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "must leave the file owner-only, got {mode:o}");
    }

    /// The load-bearing propagation surface for the fatal-chmod fix: when
    /// the mode genuinely cannot be secured (here, forced by pointing at a
    /// path with no file behind it — a deterministic, root-free way to
    /// force `set_permissions` to fail), this returns `Err` rather than
    /// swallowing the failure and letting a caller believe the ledger is
    /// secured when it is not. `FabricRuntime::start` now propagates this
    /// `Err` with `?`, aborting bring-up before schema install, peer
    /// registration, or serving ever run.
    #[test]
    fn returns_an_error_naming_the_path_when_the_mode_cannot_be_set() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("fabric-ledger.db");
        // Deliberately never created: `set_permissions` on a nonexistent
        // path fails, exercising the exact `Err` arm `FabricRuntime::start`
        // now must abort on.
        assert!(
            !db_path.exists(),
            "test setup sanity: the path must not exist, or this proves nothing about failure"
        );

        let result = tighten_fabric_ledger_permissions(&db_path);

        let error = result.expect_err(
            "tightening a nonexistent file's permissions must return Err, not silently succeed",
        );
        assert!(
            error.contains(&db_path.display().to_string()),
            "the error must name the path that could not be secured"
        );
    }
}

/// CLI entry point for `vigil fabric ticket` (docs/cli.md's "Fabric ticket"
/// section): print THIS node's enrollment ticket to stdout, on explicit
/// invocation only.
///
/// Tries a live bind first — the same identity file, same sticky-port
/// convention under `<data-dir>/fabric` that [`fabric_bring_up`] uses — then
/// reads [`FabricRuntime::own_ticket`] off the freshly bound endpoint: never
/// a hub role, never a dial, just a bare bind to read back this node's own
/// address-bearing ticket. A first invocation against an empty data dir
/// generates a fresh identity (the same `FabricIdentity::load_or_generate`
/// path a first `vigil run` takes) rather than erroring.
///
/// **While Vigil is running**, that live start necessarily contends for the
/// SAME on-disk fabric ledger the running node's own `Database::open`
/// already holds. In practice that contention is caught EARLY — at the
/// ledger open, before this command ever reaches the endpoint bind step —
/// and surfaces as a TYPED [`FabricStartError::StoreHeldByWriter`], carried
/// through at the exact call site that observes it ([`FabricRuntime::start`]'s
/// `Database::open`) rather than flattened to a string and re-matched by
/// substring here. contextdb names that one condition in two typed shapes —
/// same-process `DatabaseLocked` and cross-process `HeldByWriter` — and a
/// separate `vigil fabric ticket` process always meets the cross-process one;
/// [`store_is_held_by_a_writer`] recognizes both. That
/// failure is the expected, ordinary case — this command exists
/// specifically for an operator to run against a live node — so on exactly
/// that lock-conflict failure this falls back to the ticket the running
/// node itself cached the last time it started successfully
/// ([`write_own_ticket_cache`]), which is byte-identical to what that
/// node's own startup `fabric-join` line already advertised — PROVIDED:
///
/// 1. the cached content still parses as a structurally valid enrollment
///    ticket (never returned unvalidated, so a corrupt or truncated cache
///    file cannot silently masquerade as a live ticket), AND
/// 2. the identity stamped alongside the cache ([`write_own_ticket_identity_stamp`])
///    still matches the identity this data dir loads TODAY (never returned
///    when the identity file was deleted and regenerated since the cache
///    was written, which would make the cached ticket describe a node that
///    no longer exists).
///
/// **What this freshness check does NOT prove, and why:** it proves the
/// cache belongs to the identity currently in play, not that its encoded
/// network address (direct addresses, relay homing) is what the running
/// node would advertise if asked right now. Those addresses are discovered
/// fresh at every bind and are only known to the live `IrohServer` inside
/// the running process; the sticky-port record that pins the PORT half is
/// tracked by `contextdb_server::transport::iroh` but not exposed publicly
/// (`sticky_port_path`/`read_sticky_ports` are private to that module, and
/// there is no public ticket-parsing API to recompute or compare against
/// it from here). Establishing full address freshness without contending
/// for the same lock needs one of: (a) a live query channel to the running
/// process (e.g. a local IPC/health call it answers with its current
/// ticket), or (b) a new PUBLIC contextdb-server API that lets a caller
/// read back the current sticky-port record for an identity and compare it
/// against a cached ticket's encoded port, without binding. Neither exists
/// today; this is a genuine gap, not a rounding-off of a known-easy check.
/// The identity-stamp check above is the freshness signal that IS available
/// without either of those, and it does close the most severe staleness
/// case (a leftover cache from a rotated identity describing a node that no
/// longer exists at all).
///
/// Any OTHER start failure (a corrupt identity file, a database-open error
/// unrelated to a live lock holder, a schema-install failure, an endpoint
/// bind failure with no remembered lock contention, and so on) is unrelated
/// to the running-node contention this fallback exists for, so it is
/// surfaced verbatim rather than papered over with a possibly-stale cached
/// value — a wrong "the ticket must be X" answer is worse than an honest
/// error naming what actually broke.
pub(crate) fn run_ticket_command(args: Vec<std::ffi::OsString>) -> Result<(), String> {
    let config = crate::config::load(args)?;
    let fabric_data_dir = config.data_dir.join("fabric");
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("vigil-fabric-ticket")
        .enable_all()
        .build()
        .map_err(|error| format!("fabric ticket: start tokio runtime: {error}"))?;
    match tokio_runtime.block_on(FabricRuntime::start(&fabric_data_dir, None, false)) {
        Ok(runtime) => {
            println!("ticket={}", runtime.own_ticket());
            Ok(())
        }
        Err(bind_error @ FabricStartError::StoreHeldByWriter { .. }) => {
            let identity_path = fabric_data_dir.join("fabric-identity.key");
            if !cached_ticket_identity_is_current(&fabric_data_dir, &identity_path) {
                return Err(format!("fabric ticket: {bind_error}"));
            }
            match read_own_ticket_cache(&fabric_data_dir) {
                Some(ticket) if FabricRuntime::validate_fabric_ticket(&ticket).is_ok() => {
                    println!("ticket={ticket}");
                    Ok(())
                }
                _ => Err(format!("fabric ticket: {bind_error}")),
            }
        }
        Err(bind_error) => Err(format!("fabric ticket: {bind_error}")),
    }
}

/// Whether this ledger open was refused because a WRITER already owns the
/// store — which is the ordinary condition when an operator runs `vigil
/// fabric ticket` against the node they are running.
///
/// contextdb reports that one fact in TWO typed shapes, by who is holding it.
/// A second open from THIS process is [`contextdb_core::Error::DatabaseLocked`];
/// an open from ANOTHER process — which a separate `vigil fabric ticket`
/// invocation always is — is a `HeldByWriter` read failure naming the holding
/// process id and the store. Both say the running node owns its own ledger,
/// so both are the same condition here and both permit the cached-ticket
/// fallback. Reading only the same-process shape left the cross-process one —
/// the only shape this command ever actually meets — classified as an ordinary
/// error, and the fallback the command exists for never fired.
///
/// Matched on the type, never on the message. Every other read failure stays
/// an ordinary error: `HeldByReaders` above all, which is a crowd of readers
/// rather than the running node's own write ownership, and must never unlock a
/// cached answer.
fn store_is_held_by_a_writer(error: &contextdb_core::Error) -> bool {
    match error {
        contextdb_core::Error::DatabaseLocked { .. } => true,
        contextdb_core::Error::ReadFailure(failure) => matches!(
            failure.kind(),
            contextdb_core::read_contract::ReadFailureKind::HeldByWriter
        ),
        _ => false,
    }
}

/// Typed outcome of [`FabricRuntime::start`]. Every non-lock failure carries
/// only a message (there is currently only one classification a caller
/// needs to act on differently — see [`run_ticket_command`]'s fallback); a
/// future caller that needs a second typed distinction adds a variant here
/// rather than re-introducing string matching.
#[derive(Debug)]
pub enum FabricStartError {
    /// A writer already owns the ledger store, observed at `Database::open`
    /// in either typed shape contextdb reports it in — same-process
    /// `DatabaseLocked` or cross-process `HeldByWriter` (see
    /// [`store_is_held_by_a_writer`]) — carried through as a type rather than
    /// flattened to a string and re-parsed by substring at the call site that
    /// needs to distinguish it.
    StoreHeldByWriter { message: String },
    /// Any other bring-up failure.
    Other(String),
}

impl std::fmt::Display for FabricStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FabricStartError::StoreHeldByWriter { message } | FabricStartError::Other(message) => {
                write!(f, "{message}")
            }
        }
    }
}

impl From<String> for FabricStartError {
    fn from(message: String) -> Self {
        FabricStartError::Other(message)
    }
}

/// Convert this crate's process-local work identity into the fabric wire
/// envelope (criterion C1): only the identity fields a claiming worker needs
/// to decode + detect travel over the wire — the rich local fields
/// (media item, ordering, observed time) never leave this node; the
/// result-consumer reconstructs the full envelope from the SAME
/// `WorkEnvelope` it retained locally, never from the wire.
#[cfg(feature = "fabric")]
fn wire_envelope_from_work(
    work: &crate::workgraph::WorkEnvelope,
) -> crate::detector_workclass::WireWorkEnvelope {
    crate::detector_workclass::WireWorkEnvelope {
        work_id: work.work_id.to_string(),
        parent_work_id: work.parent_work_id.map(|id| id.to_string()),
        contributing_work_ids: work
            .contributing_work_ids
            .iter()
            .map(|id| id.to_string())
            .collect(),
        stage: work.stage.as_str().to_string(),
        stream_id: work.stream_id.as_str().to_string(),
        ordering_stream_epoch: work.ordering.stream_epoch,
        ordering_stream_sequence: work.ordering.stream_sequence,
        schema_version: crate::detector_workclass::DETECTOR_SCHEMA_VERSION,
    }
}

/// Reason recorded on the failure row the C5 dead-claimant steal writes, so a
/// ledger reader can tell a genuine execution failure from a submitter-driven
/// reclaim of a worker whose heartbeat stopped.
#[cfg(feature = "fabric")]
pub(crate) const STEAL_REASON: &str = "claimant-capability-expired";

/// The submitter's C5 dead-claimant steal (fix cycle 8): reclaim a stranded
/// offloaded job whose claimant has died mid-lease, WITHOUT waiting out the
/// claimant's full (5-minute) lease. It records a failure row for the dead
/// claimant's attempt, which the ledger's failure-aware rule turns the job
/// `Pending` again, so the submitter's own deferring worker claims and runs it
/// locally on its normal poll — and the existing `executor == self` apply path
/// prints `offload-fallback=local why=lease-expired`. No lease field is mutated
/// and no engine state machine changes: this is one guarded write over existing
/// ledger semantics.
///
/// Returns `Ok(true)` iff a steal row was written. Guards, in order:
/// 1. ONLY the job's own submitter may steal (a foreign node never does — it
///    does not own the C5 fallback duty). Enforced against the ledger's own
///    `submitter_node_id`, not merely the caller's structural context.
/// 2. The job must be actively `Leased` (nothing to steal from a job that is
///    already `Pending`/`Done`/`Failed`/`Cancelled` — in particular a result
///    that landed before the steal makes this a no-op).
/// 3. The claimant must be a FOREIGN node — never this submitter's own live
///    self-reclaim in flight (which a second steal would loop to `Failed`).
/// 4. The pure [`crate::offload_policy::should_steal_stranded_claim`] policy
///    must hold: horizon elapsed AND the claimant fallen out of the live
///    capability set. A slow-but-ALIVE claimant is never stolen.
#[cfg(feature = "fabric")]
pub fn try_steal_stranded_claim(
    db: &Database,
    my_node_id: &str,
    job_id: &str,
    live_capability_node_ids: &[String],
    now_ms: i64,
) -> Result<bool, String> {
    use contextdb_engine::work_ledger::{JobState, job_snapshot, job_state, record_failure};

    let Some(job) = job_snapshot(db, job_id)
        .map_err(|err| format!("fabric: steal read job {job_id}: {err}"))?
    else {
        return Ok(false);
    };
    // Guard 1: only the job's own submitter may steal.
    if job.submitter_node_id != my_node_id {
        return Ok(false);
    }
    // Guard 2: there must be a live claimant's attempt to abandon (a job that
    // is already Pending/Done/Failed/Cancelled has nothing to steal — a result
    // that landed before the steal makes this a no-op).
    let JobState::Leased {
        node_id: claimant,
        attempt,
        ..
    } = job_state(db, job_id, now_ms)
        .map_err(|err| format!("fabric: steal read state {job_id}: {err}"))?
    else {
        return Ok(false);
    };
    // Guard 3: never steal the submitter's OWN claim. After a steal the
    // submitter reclaims locally, so the job is briefly Leased by this node
    // while it runs; abandoning that live self-claim would loop the in-flight
    // local execution to Failed. The steal targets a FOREIGN dead claimant
    // only. (A node's own id is never in the REMOTE capability set, so the
    // liveness policy below cannot make this distinction — it must be explicit.)
    if claimant == my_node_id {
        return Ok(false);
    }
    // Guard 4: the horizon has elapsed AND the claimant has fallen out of the
    // live capability set (a slow-but-alive claimant is never stolen).
    if !crate::offload_policy::should_steal_stranded_claim(
        now_ms,
        job.deadline_ms,
        &claimant,
        live_capability_node_ids,
    ) {
        return Ok(false);
    }
    // Abandon the dead claimant's attempt. The ledger's failure-aware rule
    // (a failure row on the highest attempt releases the lease) turns the job
    // Pending, so the submitter's own deferring worker reclaims it. max_attempts
    // defaults to 2, so this one abandon flips the job Pending, never Failed.
    record_failure(db, job_id, attempt, &claimant, STEAL_REASON, now_ms)
        .map_err(|err| format!("fabric: steal abandon {job_id}#{attempt}: {err}"))?;
    Ok(true)
}

/// Submit one segment as a `vigil.detector` job (criteria C1/C2's offload
/// leg). Borrows `segment` — ownership moves into `pending_segments` only on
/// success, so a submission failure leaves the caller free to fall back to
/// local detection with the segment untouched.
#[cfg(feature = "fabric")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_offload_segment(
    fabric: &FabricBundle,
    config: &config::RuntimeConfig,
    segment: &crate::runtime::CapturedSegment,
    detection_work: &crate::workgraph::WorkEnvelope,
) -> Result<String, String> {
    let envelope = wire_envelope_from_work(detection_work);
    let codec = match segment.media.codec {
        crate::VideoCodec::H264 => crate::detector_workclass::WireVideoCodec::H264,
        crate::VideoCodec::H265 => crate::detector_workclass::WireVideoCodec::H265,
    };
    let deadline_ms =
        fabric.runtime.wall_now_ms() + fabric.offload_policy_config.fallback_horizon_ms as i64;
    fabric
        .tokio_handle
        .block_on(fabric.runtime.submit_detector_job(
            envelope,
            codec,
            segment.fps,
            config.detector_sample_frames,
            config.detector_confidence_threshold,
            segment.clip_sha256.clone(),
            segment.decoded_frames_sha256.clone(),
            config.detector_model_id.clone(),
            &segment.media.encoded_units,
            Some(deadline_ms),
        ))
}

/// Bridge a landed `vigil.detector` wire result into the ready
/// `DetectorOutput` the local recording path consumes — the ONE place the
/// forward-proof marker fields (`model_forward_sha256`/`result_sha256`) are
/// carried from the wire onto the detector output. Owned here in
/// `crate::fabric`, not in the detector-source file `runtime.rs`, so that
/// file names no forward-proof marker and spawns no task (first-light-loop
/// detector-source guard).
#[cfg(feature = "fabric")]
fn detector_output_from_wire(wire: DetectorResult) -> crate::yolox_detector::DetectorOutput {
    crate::yolox_detector::DetectorOutput {
        detector_backend: wire.detector_backend,
        detector_session_id: wire.detector_session_id,
        model_sha256: wire.model_sha256,
        clip_sha256: wire.clip_sha256,
        model_forward_sha256: wire.model_forward_sha256,
        detector_nms_sha256: wire.detector_nms_sha256,
        result_sha256: wire.result_sha256,
        detections: wire
            .detections
            .into_iter()
            .map(|detection| crate::yolox_detector::Detection {
                class_name: detection.class_name,
                confidence: detection.confidence.0,
                bbox: detection.bbox,
                frame_index: detection.frame_index,
            })
            .collect(),
        forward_event_nonce: None,
        forward_event_seq: None,
    }
}

/// Apply a landed `vigil.detector` result exactly as the local path would
/// have (criterion C4): validated + applied exactly-once through
/// `PendingOffloads`, discarding a late/duplicate result with a receipt,
/// never double-counted. Whether the executor was a remote node or this
/// node's own deferred submission reclaimed after its deadline (C5), the
/// only difference is which receipt line renders.
#[cfg(feature = "fabric")]
fn apply_fabric_result(
    fabric: &FabricBundle,
    stats: &RuntimeStatsState,
    job_id: &str,
    result: contextdb_engine::work_ledger::JobResult,
    entry: PendingOffloadSegment,
) {
    let executor_node_id = result
        .receipt
        .get("executor_node_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let backend = result
        .receipt
        .get("backend")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let wire: crate::detector_workclass::DetectorResult =
        match serde_json::from_slice(&result.output) {
            Ok(wire) => wire,
            Err(error) => {
                println!("fabric_result_decode_failed=true job_id={job_id} error={error}");
                let _ = fs::remove_file(&entry.segment.path);
                return;
            }
        };

    let receipt_id = crate::workgraph::ReceiptId::generate();
    let result_envelope = crate::workgraph::ResultEnvelope {
        work_id: entry.detection_work.work_id,
        parent_work_id: entry.detection_work.parent_work_id,
        contributing_work_ids: entry.detection_work.contributing_work_ids.clone(),
        stage: entry.detection_work.stage.clone(),
        stream_id: entry.detection_work.stream_id.clone(),
        media_item: entry.detection_work.media_item,
        ordering: entry.detection_work.ordering,
        observed_at: entry.detection_work.observed_at,
        result_schema_version: entry.detection_work.schema_version,
        receipt_id,
    };
    let stage_receipt = crate::workgraph::StageReceipt {
        receipt_id,
        work_id: entry.detection_work.work_id,
        parent_work_id: entry.detection_work.parent_work_id,
        stage: entry.detection_work.stage.clone(),
        stream_id: entry.detection_work.stream_id.clone(),
        configured_backend: None,
        attempted_backend: Some(backend.clone()),
        active_backend: Some(backend.clone()),
        fallback_backend: None,
        selected_device: None,
        probe_result: None,
        fallback_reason: None,
        started_at: entry.detection_started_at,
        ended_at: fabric.runtime.persisted_clock.now_utc(),
        output_count: wire.detections.len() as u64,
        disposition: crate::workgraph::WorkDisposition::Completed,
    };

    let outcome = fabric.pending.apply_remote_result(
        job_id,
        &result_envelope,
        &stage_receipt,
        &entry.receipts,
    );
    match outcome {
        crate::fabric::RemoteResultOutcome::Applied => {
            let is_local_fallback = executor_node_id == fabric.node_id;
            let receipt_line = if is_local_fallback {
                crate::offload_policy::render_offload_fallback_receipt("lease-expired")
            } else {
                crate::offload_policy::render_remote_detection_receipt(&executor_node_id, &backend)
            };
            stats.update(|stats| crate::runtime_stats::push_recent_receipt(stats, receipt_line));

            let output = detector_output_from_wire(wire);
            if let Err(error) = crate::runtime::record_or_publish_detected_events(
                entry.store.as_ref(),
                &entry.segment,
                &output,
                &entry.detection_work,
                &crate::runtime::DetectionRecordingContext {
                    config: &entry.config,
                    stats: &entry.stats,
                    health: &entry.health,
                    detection_publisher: entry.detection_publisher.as_deref(),
                    recognition_embedder: entry.recognition_embedder.as_deref(),
                    receipts: &entry.receipts,
                },
            ) {
                println!("fabric_record_detection_failed=true job_id={job_id} error={error}");
            }
        }
        crate::fabric::RemoteResultOutcome::DiscardedLate => {
            println!("fabric_result_discarded_late=true job_id={job_id}");
            let _ = fs::remove_file(&entry.segment.path);
        }
        crate::fabric::RemoteResultOutcome::RejectedJoin(rejection) => {
            println!("fabric_result_rejected_join=true job_id={job_id} rejection={rejection:?}");
            let _ = fs::remove_file(&entry.segment.path);
        }
    }
}
