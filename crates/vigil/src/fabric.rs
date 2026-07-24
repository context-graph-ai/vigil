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
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

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
    /// (`SyncServer::with_transport` enables relay internally), never a
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
    ) -> Result<Self, String> {
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
        let db =
            Arc::new(Database::open(&db_path).map_err(|err| {
                format!("fabric: open ledger database {}: {err}", db_path.display())
            })?);
        contextdb_engine::work_ledger::install_work_ledger_schema(&db)
            .map_err(|err| format!("fabric: install work ledger schema: {err}"))?;
        let _ = contextdb_engine::peer_directory::install_peer_directory_schema(&db);

        let bind_spec = contextdb_server::peer_bind_spec(&identity_path);
        let own_endpoint = Arc::new(
            contextdb_server::PeerEndpoint::bind(&bind_spec)
                .await
                .map_err(|err| format!("fabric: bind this node's endpoint: {err}"))?,
        );

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
            let server = contextdb_server::SyncServer::with_transport(
                db.clone(),
                own_endpoint.transport(),
                tenant.clone(),
                contextdb_engine::sync_types::ConflictPolicies::uniform(
                    contextdb_engine::sync_types::ConflictPolicy::LatestWins,
                ),
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
            Ok(_) => Err(format!(
                "not a valid fabric enrollment ticket: {trimmed:?} — paste the exact ticket \
                 printed by the hub node's fabric-join line, unedited"
            )),
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
        encoded_units: &[Vec<u8>],
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
        let executor = DetectorWorkExecutor::new(backend, node_id.clone());
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
            if !writes_are_canonical && let Err(error) = client.push().await {
                println!("fabric_capability_push_failed=true backend={backend_tag} error={error}");
            }
            let _ = contextdb_server::work_ledger::run_worker_loop(
                &client,
                &config,
                &executor,
                std::time::Duration::from_secs(2),
                shutdown,
            )
            .await;
        })
    }
}

/// Resolve the exact lease duration passed to ContextDB's standing worker.
///
/// Production normally supplies `configured` from the already-resolved Vigil
/// configuration. The environment fallback supports direct `FabricRuntime`
/// construction, and the final fallback preserves the historical five-minute
/// lease. Kept as one production-used seam so config tests do not need a live
/// Iroh hub merely to observe a value before it enters `WorkerConfig`.
#[doc(hidden)]
pub fn resolved_worker_lease_duration_ms(configured: Option<u64>) -> i64 {
    configured.unwrap_or_else(|| {
        std::env::var("VIGIL_FABRIC_WORKER_LEASE_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(300_000)
    }) as i64
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
    /// Shared with the camera detector threads (which populate it the moment a
    /// detector loads) and, for a cameraless worker box, the bootstrap thread.
    /// An `Arc<OnceLock<..>>` rather than an owned `OnceLock` so it is created in
    /// `run_inner` BEFORE this bundle exists: fabric now attaches asynchronously,
    /// so a camera's detector may load before the bundle is built, and the
    /// worker loop must still observe that detector the instant the bundle
    /// appears.
    pub(crate) worker_detector_slot:
        Arc<OnceLock<(Arc<crate::detector::PromotableDetector>, String)>>,
    /// Whether this node's fabric worker loop is actually running and serving
    /// the fleet — flipped `true` the moment the loop starts. The status task
    /// renders it onto the shared `fabric-status` line so a not-serving node
    /// says so loudly (owner steer 2026-07-13 / criterion C6), rather than
    /// looking identical to a healthy idle one.
    pub(crate) worker_serving: Arc<AtomicBool>,
    /// The named reason the worker loop is not serving (enrollment rejected,
    /// no detector loaded in time) — rendered next to `fabric-worker-serving=
    /// false`. Empty once serving.
    pub(crate) worker_serving_reason: Arc<Mutex<String>>,
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
    pub(crate) store: Store,
    pub(crate) stats: RuntimeStatsState,
    pub(crate) health: HealthState,
    pub(crate) detection_publisher: Option<Arc<dyn crate::site_channel::DetectionChannel>>,
    pub(crate) recognition_embedder: Option<Arc<dyn context_graph::Embedder>>,
    pub(crate) receipts: Arc<crate::workgraph::StageReceiptLog>,
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
    worker_detector_slot: Arc<OnceLock<(Arc<crate::detector::PromotableDetector>, String)>>,
) -> Option<Arc<FabricBundle>> {
    if config.fabric_ticket.is_none() && !config.fabric_hub {
        return None;
    }
    // Test-only bring-up delay lever (default absent → ZERO cost; no shipped
    // surface — add-on options/env, fabric.toml, CLI — ever sets it): lets the
    // boot-readiness test make fabric bring-up deliberately slow so it can prove
    // /health reaches Ready WITHOUT waiting for the fabric ledger to open. Inert
    // in production, mirroring VIGIL_FABRIC_WORKER_SLOT_DEADLINE_MS.
    if let Some(delay_ms) = std::env::var("VIGIL_FABRIC_BRINGUP_DELAY_MS")
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
    let serves_role = fabric_runtime.serves_fabric_role();
    let has_camera_source = config
        .cameras
        .iter()
        .any(|camera| camera.rtsp_url.is_some());
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
    // first camera detector wins the `OnceLock`); this only fills the gap for a
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
                    let _ = boot_bundle
                        .worker_detector_slot
                        .set((detector, backend_tag));
                }
                None => {
                    // No executable capability: name the staging fix and do
                    // NOT advertise a detector row (PO ruling 2026-07-13).
                    if let Ok(mut reason) = boot_bundle.worker_serving_reason.lock() {
                        *reason = "no-model — stage a loadable detection model on this worker \
                                   (set detector_model_path / VIGIL_DETECTOR_MODEL_PATH)"
                            .to_string();
                    }
                }
            }
        });
    }

    // Worker loop, started once the first camera's detector (or a
    // detector-less standalone worker box, bounded below) is ready. Claims +
    // executes ANY matching `vigil.detector` job, including this node's own
    // deferred submissions once their deadline passes (criterion C5/C8).
    let worker_bundle = bundle.clone();
    handle.spawn(async move {
        // Test-only override for the bounded wait below (default unchanged:
        // 60s) — lets an integration test shorten the wait instead of
        // sleeping through the real deadline. Inert in production: no
        // shipped surface (add-on options/env, fabric.toml, CLI) ever sets
        // this variable.
        let worker_slot_deadline_ms = std::env::var("VIGIL_FABRIC_WORKER_SLOT_DEADLINE_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60_000);
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(worker_slot_deadline_ms);
        loop {
            if let Some((detector, backend_tag)) = worker_bundle.worker_detector_slot.get() {
                let backend = Arc::new(crate::fabric::FabricProductionDetectorBackend::new(
                    detector.clone(),
                    backend_tag.clone(),
                ));
                let shutdown = Arc::new(AtomicBool::new(false));
                worker_bundle.runtime.spawn_worker_loop(backend, shutdown);
                worker_bundle
                    .worker_serving
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                if let Ok(mut reason) = worker_bundle.worker_serving_reason.lock() {
                    reason.clear();
                }
                println!("fabric_worker_loop_started=true backend={backend_tag}");
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                // Do not overwrite a more specific reason already set (a
                // rejected ticket, or a detector-load failure) — only fill in
                // the deadline reason for a node that had a serving role and
                // simply never had a detector become available in time.
                if let Ok(mut reason) = worker_bundle.worker_serving_reason.lock()
                    && (reason.is_empty() || reason.as_str() == "worker-loop-starting")
                {
                    *reason = "no-local-detector-loaded-within-deadline".to_string();
                }
                println!(
                    "fabric_worker_loop_not_started=true reason=no_local_detector_loaded_within_deadline"
                );
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }
    });

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
            let join_line = format!("fabric-join={}", status_bundle.runtime.join_instruction());
            status_stats.update(|stats| {
                stats.fabric_status = status_line.clone();
                stats.fabric_join = join_line.clone();
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
            if let Err(error) = crate::runtime::record_detected_events(
                &entry.store,
                &entry.config,
                &entry.segment,
                &output,
                &entry.stats,
                &entry.health,
                entry.detection_publisher.as_deref(),
                entry.recognition_embedder.as_deref(),
                &entry.detection_work,
                &entry.receipts,
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
