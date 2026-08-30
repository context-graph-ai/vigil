use context_graph::{
    AuditEntry, AuditFilter, AuditTarget, CgError, ConsumerGraphView, Context, ContextId, Decision,
    DecisionId, Entity, EntityId, EvidenceKind, EvidenceRef, Intention, IntentionId, Observation,
    ObservationId, Store,
};
use serde_json::Value;

use crate::runtime_stats::{RuntimeStats, format_stats};

/// The graph reads `why` and `events` are built from, whichever handle this
/// process happens to be holding.
///
/// Context Graph gives the SAME eight reads two ways — on the writable `Store`
/// the running runtime owns, and on the read-only `ConsumerGraphView` a stopped
/// deployment opens — with the same signatures and the same answer types, run
/// by the same code underneath. What is missing is only a name to call both by,
/// because the trait that unifies them upstream is crate-private. This is that
/// name and nothing else: every method here is one delegation, there is no
/// second decoder, no statement of Vigil's own, and no behaviour that differs
/// between the two roads. That is the whole point — `why` and `events` are
/// rendered ONCE, by [`render_read_only`], and an operator cannot tell which
/// handle answered them because nothing about the answer depends on it.
pub trait GraphReads {
    fn audit_query(&self, filter: AuditFilter) -> Result<Vec<AuditEntry>, CgError>;
    fn list_contexts(&self) -> Result<Vec<Context>, CgError>;
    fn get_context(&self, id: ContextId) -> Result<Option<Context>, CgError>;
    fn get_entity(&self, id: EntityId) -> Result<Option<Entity>, CgError>;
    fn get_intention(&self, id: IntentionId) -> Result<Option<Intention>, CgError>;
    fn get_decision(&self, id: DecisionId) -> Result<Option<Decision>, CgError>;
    fn get_observation(&self, id: ObservationId) -> Result<Observation, CgError>;
    fn list_observations(&self, context_id: Option<ContextId>)
    -> Result<Vec<Observation>, CgError>;
}

impl GraphReads for Store {
    fn audit_query(&self, filter: AuditFilter) -> Result<Vec<AuditEntry>, CgError> {
        Store::audit_query(self, filter)
    }
    fn list_contexts(&self) -> Result<Vec<Context>, CgError> {
        Store::list_contexts(self)
    }
    fn get_context(&self, id: ContextId) -> Result<Option<Context>, CgError> {
        Store::get_context(self, id)
    }
    fn get_entity(&self, id: EntityId) -> Result<Option<Entity>, CgError> {
        Store::get_entity(self, id)
    }
    fn get_intention(&self, id: IntentionId) -> Result<Option<Intention>, CgError> {
        Store::get_intention(self, id)
    }
    fn get_decision(&self, id: DecisionId) -> Result<Option<Decision>, CgError> {
        Store::get_decision(self, id)
    }
    fn get_observation(&self, id: ObservationId) -> Result<Observation, CgError> {
        Store::get_observation(self, id)
    }
    fn list_observations(
        &self,
        context_id: Option<ContextId>,
    ) -> Result<Vec<Observation>, CgError> {
        Store::list_observations(self, context_id)
    }
}

impl GraphReads for ConsumerGraphView<'_> {
    fn audit_query(&self, filter: AuditFilter) -> Result<Vec<AuditEntry>, CgError> {
        ConsumerGraphView::audit_query(self, filter)
    }
    fn list_contexts(&self) -> Result<Vec<Context>, CgError> {
        ConsumerGraphView::list_contexts(self)
    }
    fn get_context(&self, id: ContextId) -> Result<Option<Context>, CgError> {
        ConsumerGraphView::get_context(self, id)
    }
    fn get_entity(&self, id: EntityId) -> Result<Option<Entity>, CgError> {
        ConsumerGraphView::get_entity(self, id)
    }
    fn get_intention(&self, id: IntentionId) -> Result<Option<Intention>, CgError> {
        ConsumerGraphView::get_intention(self, id)
    }
    fn get_decision(&self, id: DecisionId) -> Result<Option<Decision>, CgError> {
        ConsumerGraphView::get_decision(self, id)
    }
    fn get_observation(&self, id: ObservationId) -> Result<Observation, CgError> {
        ConsumerGraphView::get_observation(self, id)
    }
    fn list_observations(
        &self,
        context_id: Option<ContextId>,
    ) -> Result<Vec<Observation>, CgError> {
        ConsumerGraphView::list_observations(self, context_id)
    }
}

pub(crate) struct StoreBackedWhyResponse {
    pub(crate) selection: String,
    pub(crate) observation_id: String,
    pub(crate) observed_at: String,
    pub(crate) clip_ref: String,
    pub(crate) camera_id: String,
    pub(crate) camera_name: String,
    pub(crate) camera_rtsp_url: String,
    pub(crate) context_id: String,
    pub(crate) site_name: String,
    pub(crate) decision_id: String,
    pub(crate) intention_id: String,
    pub(crate) intention_description: String,
    pub(crate) model_id: String,
    pub(crate) threshold: f64,
    pub(crate) class_name: String,
    pub(crate) confidence: f64,
    pub(crate) bbox: String,
    pub(crate) frame_index: u64,
    pub(crate) detector_image_ref: String,
    pub(crate) recognition_name: Option<String>,
    pub(crate) recognition_score: Option<f64>,
    pub(crate) recognition_reference_label: Option<String>,
    pub(crate) recognition_enrolled_by: Option<String>,
}

impl StoreBackedWhyResponse {
    pub(crate) fn from_store_reads(
        selection: String,
        audit_entries: Vec<AuditEntry>,
        observation: Observation,
        decision: Decision,
        intention: Intention,
        entity: Entity,
        context: Context,
    ) -> Result<Self, String> {
        let _audit_count = audit_entries.len();
        let evidence = first_evidence(&observation)?;
        let detector_image_ref = first_image_evidence(&observation)
            .map(|evidence| evidence.source_ref.clone())
            .unwrap_or_else(|| {
                observation
                    .properties
                    .get("detector_evidence_ref")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            });
        let model_id = decision
            .properties
            .get("model_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let threshold = decision
            .properties
            .get("threshold")
            .and_then(Value::as_f64)
            .unwrap_or_default();
        Ok(Self {
            selection,
            observation_id: observation.id.to_string(),
            observed_at: observation.observed_at.to_rfc3339(),
            clip_ref: evidence.source_ref.clone(),
            camera_id: entity.id.to_string(),
            camera_name: entity.name.clone(),
            camera_rtsp_url: entity
                .properties
                .get("rtsp_url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            context_id: context.id.to_string(),
            site_name: context.name.clone(),
            decision_id: decision.id.to_string(),
            intention_id: intention.id.to_string(),
            intention_description: intention.description.clone(),
            model_id,
            threshold,
            class_name: observation
                .observed_properties
                .get("class")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            confidence: observation
                .observed_properties
                .get("confidence")
                .and_then(Value::as_f64)
                .unwrap_or_default(),
            bbox: observation
                .observed_properties
                .get("bbox")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            frame_index: observation
                .observed_properties
                .get("frame_index")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            detector_image_ref,
            recognition_name: None,
            recognition_score: None,
            recognition_reference_label: None,
            recognition_enrolled_by: None,
        })
    }
}

pub(crate) struct StoreBackedEventsResponse {
    pub(crate) rows: Vec<StoreBackedEventRow>,
    pub(crate) observation_id: String,
    pub(crate) observed_at: String,
    pub(crate) camera_name: String,
    pub(crate) class_name: String,
    pub(crate) confidence: f64,
    pub(crate) bbox: String,
    pub(crate) frame_index: u64,
    pub(crate) clip_ref: String,
    pub(crate) detector_image_ref: String,
}

pub(crate) struct StoreBackedEventRow {
    pub(crate) observation_id: String,
    pub(crate) observed_at: String,
    pub(crate) camera_name: String,
    pub(crate) class_name: String,
    pub(crate) confidence: f64,
    pub(crate) bbox: String,
    pub(crate) frame_index: u64,
    pub(crate) clip_ref: String,
    pub(crate) detector_image_ref: String,
}

impl StoreBackedEventsResponse {
    pub(crate) fn from_store_reads(
        observations: Vec<Observation>,
        observation: Option<Observation>,
        entity: Option<Entity>,
    ) -> Result<Self, String> {
        let mut ordered = observations;
        ordered.sort_by(|left, right| {
            right
                .observed_at
                .cmp(&left.observed_at)
                .then_with(|| right.id.as_uuid().cmp(&left.id.as_uuid()))
        });
        let fallback_camera = entity
            .as_ref()
            .map(|entity| entity.name.clone())
            .unwrap_or_default();
        let rows = ordered
            .iter()
            .map(|observation| StoreBackedEventRow {
                observation_id: observation.id.to_string(),
                observed_at: observation.observed_at.to_rfc3339(),
                camera_name: fallback_camera.clone(),
                class_name: observation
                    .observed_properties
                    .get("class")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                confidence: observation
                    .observed_properties
                    .get("confidence")
                    .and_then(Value::as_f64)
                    .unwrap_or_default(),
                bbox: observation
                    .observed_properties
                    .get("bbox")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                frame_index: observation
                    .observed_properties
                    .get("frame_index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                clip_ref: observation
                    .evidence
                    .first()
                    .map(|evidence| evidence.source_ref.clone())
                    .unwrap_or_default(),
                detector_image_ref: first_image_evidence(observation)
                    .map(|evidence| evidence.source_ref.clone())
                    .unwrap_or_else(|| {
                        observation
                            .properties
                            .get("detector_evidence_ref")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string()
                    }),
            })
            .collect::<Vec<_>>();
        let observed_at = rows
            .first()
            .map(|row| row.observed_at.clone())
            .unwrap_or_default();
        let camera_name = rows
            .first()
            .map(|row| row.camera_name.clone())
            .unwrap_or_default();
        let class_name = rows
            .first()
            .map(|row| row.class_name.clone())
            .unwrap_or_default();
        let confidence = rows.first().map(|row| row.confidence).unwrap_or_default();
        let bbox = rows.first().map(|row| row.bbox.clone()).unwrap_or_default();
        let frame_index = rows.first().map(|row| row.frame_index).unwrap_or_default();
        let clip_ref = rows
            .first()
            .map(|row| row.clip_ref.clone())
            .unwrap_or_default();
        let detector_image_ref = rows
            .first()
            .map(|row| row.detector_image_ref.clone())
            .unwrap_or_default();
        Ok(Self {
            rows,
            observation_id: observation
                .as_ref()
                .map(|observation| observation.id.to_string())
                .unwrap_or_default(),
            observed_at,
            camera_name,
            class_name,
            confidence,
            bbox,
            frame_index,
            clip_ref,
            detector_image_ref,
        })
    }
}

pub(crate) fn handle_owner_request(
    store: &Store,
    stats: &RuntimeStats,
    data_dir: &std::path::Path,
    store_path: &std::path::Path,
    request: &str,
) -> String {
    let request = request.trim();
    let mut parts = request.splitn(2, ' ');
    let command = parts.next().unwrap_or_default();
    let argument = parts.next().unwrap_or_default().trim();
    let body = match command {
        "stats" => format_stats(stats),
        // The runtime owns the store while it runs, so it is the one that can
        // answer. It answers through the same projection a stopped deployment
        // reads directly — one rendering, two ways in.
        "settings" => crate::settings_command::answer(data_dir, store_path, argument),
        _ => match render_store_backed(store, command, argument) {
            Some(body) => body,
            None => format!("{OWNER_ERROR_PREFIX} unknown-command={command}\n"),
        },
    };
    format!("{OWNER_SERVED_PREFIX}{body}")
}

/// The ONE rendering of every store-backed live command, for both roads.
///
/// The owner serves these while it holds the store and the command line reads
/// the store itself when nobody does — and an operator did not choose which,
/// cannot see which is about to happen, and must not be told two different
/// things by the two. Rendering here rather than at each road is what makes
/// that structural: a failure gets its marker once, so the delivery boundary
/// classifies it the same either way, and the only difference left between the
/// roads is the owner-served marker the owner route puts in front.
///
/// `None` for a command this seam does not answer, so the caller says what an
/// unknown command means on its own road.
pub(crate) fn render_store_backed(store: &Store, command: &str, argument: &str) -> Option<String> {
    if let Some(body) = render_read_only(store, command, argument) {
        return Some(body);
    }
    let body = match command {
        "enroll" => {
            // "enroll <detection-id> <name...>" — the same enrollment the card
            // correction drives, via the shared correction seam.
            let mut pieces = argument.splitn(2, ' ');
            let detection_id = pieces.next().unwrap_or_default().trim().to_string();
            let name = pieces.next().unwrap_or_default().trim().to_string();
            if detection_id.is_empty() || name.is_empty() {
                format!("{ENROLL_ERROR_PREFIX} usage: vigil enroll <detection-id> <name>\n")
            } else {
                match crate::correction::record_correction(
                    store,
                    crate::correction::CorrectionRequest {
                        detection_id,
                        label: Some(name.clone()),
                        correction_type: crate::correction::CorrectionType::Enroll,
                    },
                ) {
                    Ok(receipt) => format!(
                        "enrolled=true name={name} correction_id={}\n",
                        receipt.correction_id
                    ),
                    Err(error) => format!("{ENROLL_ERROR_PREFIX} {error:?}\n"),
                }
            }
        }
        "forget" => {
            let name = argument.trim();
            if name.is_empty() {
                format!("{FORGET_ERROR_PREFIX} usage: vigil forget <name>\n")
            } else {
                match crate::recognition::forget_named_entity(store, name, None) {
                    Ok(removed) => {
                        format!("forgotten=true name={name} references_removed={removed}\n")
                    }
                    Err(error) => format!("{FORGET_ERROR_PREFIX} {error}\n"),
                }
            }
        }
        _ => return None,
    };
    Some(body)
}

/// The ONE rendering of the two live commands that only READ, for both roads.
///
/// `why` and `events` ask nothing of the store but questions, so they are
/// rendered here over whichever handle the caller has — the running runtime's
/// writable one, or the read-only view a stopped deployment opens. The owner
/// route reaches them through [`render_store_backed`], which tries this first;
/// the command line reaches them through the consumer reader. Neither road has
/// a rendering of its own, so a failure gets its marker once and the delivery
/// boundary classifies it the same either way.
///
/// `None` for a command this seam does not answer — `enroll` and `forget` are
/// EDITS and need a writable handle, so they stay on [`render_store_backed`].
pub(crate) fn render_read_only(
    store: &impl GraphReads,
    command: &str,
    argument: &str,
) -> Option<String> {
    let body = match command {
        "why" => match handle_why_read(store, argument) {
            Ok(response) => format_why_cli(&response),
            Err(error) => format!("{}\n", why_refusal(&error)),
        },
        "events" => match handle_events_read(store, 100) {
            Ok(response) => format_events_cli(&response),
            Err(error) => format!("{EVENTS_ERROR_PREFIX} {error}\n"),
        },
        _ => return None,
    };
    Some(body)
}

/// The owner-route answer from a runtime with no store behind it.
///
/// It answers everything, because a degraded run that stopped answering would
/// leave an operator staring at a connection error instead of an explanation.
/// The settings listing goes through the same command the store-backed path
/// uses, so the unmanaged statement is rendered once, in one projection;
/// everything that needs the store comes back as the one degraded refusal
/// naming the capability it is refusing.
pub(crate) fn handle_degraded_request(
    stats: &RuntimeStats,
    data_dir: &std::path::Path,
    store_path: &std::path::Path,
    request: &str,
) -> String {
    let request = request.trim();
    let mut parts = request.splitn(2, ' ');
    let command = parts.next().unwrap_or_default();
    let argument = parts.next().unwrap_or_default().trim();
    let body = match command {
        "settings" => {
            let answer = crate::settings_command::answer(data_dir, store_path, argument);
            // A run that degraded for a reason other than the store answers
            // from a store that opens, so the settings surface works and says
            // nothing about this run being unmanaged on its own. It says it
            // here: what landed, and what cannot be in force until the node is
            // repaired.
            match crate::settings_degraded::not_in_force_line() {
                Some(line) => format!("{answer}{line}"),
                None => answer,
            }
        }
        other => match crate::settings_degraded::capability_for_command(other) {
            Some(capability) => crate::settings_degraded::refusal_line(capability),
            // `stats` is the one live read that needs no store: the counters
            // are this process's own memory, and a degraded run has them.
            None if other == "stats" => format_stats(stats),
            None => format!("{OWNER_ERROR_PREFIX} unknown-command={other}\n"),
        },
    };
    format!("{OWNER_SERVED_PREFIX}{body}")
}

/// The marker every answer the running runtime serves over its store's own
/// owner route carries, so a caller can tell an owner-served answer from a direct read
/// without re-spelling it. Declared here, beside the one place that writes it.
pub const OWNER_SERVED_PREFIX: &str = "served-by=af_unix\n";

/// The marker a `why` answer carries when this deployment holds no event under
/// the id it was given.
pub const NOT_FOUND_PREFIX: &str = "not-found";

/// The marker an `events` answer carries when the read itself failed.
pub const EVENTS_ERROR_PREFIX: &str = "events-error";

/// And the two correction commands, when what they were asked to do did not
/// happen.
pub const ENROLL_ERROR_PREFIX: &str = "enroll-error";
pub const FORGET_ERROR_PREFIX: &str = "forget-error";

/// The marker the owner itself puts on a frame it could not answer — a command
/// it does not serve, or a runtime that is still opening its store.
pub const OWNER_ERROR_PREFIX: &str = "owner-error";

/// And when what it was given is not a detection id at all. It is a separate
/// marker because it is a separate thing to have gone wrong: an operator who
/// mistyped an id is not looking for a recording this deployment has lost, and
/// telling them the event was not found sends them hunting for one.
pub const MALFORMED_ID_PREFIX: &str = "malformed-id";

/// Whether an answer reports a request that was NOT served.
///
/// Read once, at the delivery boundary, so the road an answer travelled cannot
/// change what a caller does with it: an answer the owner served and an answer
/// read from the store file carry the same marker, arrive on the same stream
/// and exit with the same status. A not-found that left on standard output with
/// status `0` told every script that consumed it that the walk SUCCEEDED and
/// this deployment holds no such event, which is a different statement and a
/// false one.
pub fn answer_failed(answer: &str) -> bool {
    answer.lines().any(|line| {
        let line = line.trim_start();
        UNSERVED_MARKERS
            .iter()
            .any(|marker| line.starts_with(&format!("{marker} ")))
    })
}

/// Every marker an answer carries when the request behind it was not served.
///
/// One list, read at the ONE delivery boundary, so the road an answer travelled
/// cannot change what a caller does with it. The direct read has always
/// classified these as failures and refused on the error stream with status 2;
/// the owner route rendered the same conditions as body lines and delivered
/// them as successes, so whether a script could tell a served request from an
/// unserved one depended on whether a daemon happened to be running.
const UNSERVED_MARKERS: [&str; 6] = [
    NOT_FOUND_PREFIX,
    MALFORMED_ID_PREFIX,
    EVENTS_ERROR_PREFIX,
    ENROLL_ERROR_PREFIX,
    FORGET_ERROR_PREFIX,
    OWNER_ERROR_PREFIX,
];

/// What an argument that is not a detection id is told: what was typed, and
/// the form the id takes. One spelling, because the same operator meets it
/// whether or not this deployment has a store to look in.
pub(crate) fn malformed_id_message(request: &str) -> String {
    format!(
        "{MALFORMED_ID_PREFIX} {request} is not a detection id: a detection id is a UUID, as \
         `vigil events` prints one"
    )
}

/// The refusal a `why` request leaves as on a deployment that has NEVER
/// STARTED, decided before anything is opened.
///
/// A deployment with no store holds no event under any id — that is a true
/// answer and it needs no store to be true, so the question must not bring one
/// into existence to say it. The id is still read for its SHAPE first, because
/// an operator who mistyped one is owed the same answer here as anywhere else.
pub(crate) fn why_refusal_without_a_store(request: &str) -> String {
    let request = request.trim();
    if !request.is_empty() && request != "--latest" && uuid::Uuid::parse_str(request).is_err() {
        return why_refusal(&malformed_id_message(request));
    }
    why_refusal("event not found")
}

/// The line a `why` request that could not be served leaves as, marked with
/// what actually went wrong. A failure that already names itself keeps its own
/// marker; anything else is this deployment having no such event.
pub(crate) fn why_refusal(error: &str) -> String {
    if error.starts_with(&format!("{MALFORMED_ID_PREFIX} ")) {
        return error.to_string();
    }
    format!("{NOT_FOUND_PREFIX} {error}")
}

pub(crate) fn handle_why_read(
    store: &impl GraphReads,
    request: &str,
) -> Result<StoreBackedWhyResponse, String> {
    let audit_entries = match store.audit_query(AuditFilter::default()) {
        Ok(audit_entries) => audit_entries,
        Err(error) => return Err(error.to_string()),
    };
    // Resolve the observation, scoping to the site context (vigil is one-context-per-site).
    // For a direct UUID: fetch without scanning the store at all.
    // For --latest: use list_contexts() to get the site context (one per site),
    // then scope list_observations to that context — cheaper than probing all observations.
    let observation = if request != "--latest" && !request.is_empty() {
        // A detection id that will not parse is not an event this deployment is
        // missing, and answering as though it were leaves the operator looking
        // for a recording nobody ever named. The refusal says what was typed
        // and what the form is, which is the whole of what they need to retype
        // it.
        let uuid = uuid::Uuid::parse_str(request).map_err(|_| malformed_id_message(request))?;
        store
            .get_observation(ObservationId::from(uuid))
            .map_err(|_| "event not found".to_string())?
    } else {
        let site_ctx = store
            .list_contexts()
            .ok()
            .and_then(|ctxs| ctxs.into_iter().next().map(|c| c.id));
        let all_detections = store
            .list_observations(site_ctx)
            .map_err(|e| e.to_string())?;
        let obs_id = resolve_observation_id(&all_detections, "--latest")?;
        store.get_observation(obs_id).map_err(|e| e.to_string())?
    };
    let _observation_seen = observation.id;
    let decision_id = resolve_decision_id(&audit_entries, &observation)?;
    let decision = match store.get_decision(decision_id) {
        Ok(Some(decision)) => decision,
        Ok(None) => return Err("decision not found".to_string()),
        Err(error) => return Err(error.to_string()),
    };
    let _decision_seen = decision.id;
    let intention_id = decision
        .intention_ids
        .first()
        .copied()
        .ok_or_else(|| "decision omitted intention".to_string())?;
    let intention = match store.get_intention(intention_id) {
        Ok(Some(intention)) => intention,
        Ok(None) => return Err("intention not found".to_string()),
        Err(error) => return Err(error.to_string()),
    };
    let _intention_seen = intention.id;
    let entity = match store.get_entity(observation.entity_id) {
        Ok(Some(entity)) => entity,
        Ok(None) => return Err("camera entity not found".to_string()),
        Err(error) => return Err(error.to_string()),
    };
    let _entity_seen = entity.id;
    let context = match store.get_context(observation.context_id) {
        Ok(Some(context)) => context,
        Ok(None) => return Err("context not found".to_string()),
        Err(error) => return Err(error.to_string()),
    };
    let _context_seen = context.id;
    let selection = why_selection_label(request);
    let mut response = StoreBackedWhyResponse::from_store_reads(
        selection,
        audit_entries, // StoreBackedWhyResponse
        observation,   // StoreBackedWhyResponse
        decision,      // StoreBackedWhyResponse
        intention,     // StoreBackedWhyResponse
        entity,        // StoreBackedWhyResponse
        context,       // StoreBackedWhyResponse
    )?;
    // Recognition provenance from the shared read-surface helper — the CLI why
    // and the HTTP why must show the same match story.
    if let Some(provenance) =
        crate::recognition::recognition_provenance(store, &response.observation_id)
    {
        response.recognition_name = Some(provenance.name);
        response.recognition_score = Some(provenance.score);
        response.recognition_reference_label = Some(provenance.reference_label);
        response.recognition_enrolled_by = provenance.enrolled_by_correction_id;
    }
    Ok(response)
}

pub(crate) fn handle_events_read(
    store: &impl GraphReads,
    limit: usize,
) -> Result<StoreBackedEventsResponse, String> {
    // Scope the scan to the site context (vigil is one-context-per-site).
    // list_contexts() is cheaper than a full observation probe — typically returns one entry.
    let site_ctx = store
        .list_contexts()
        .ok()
        .and_then(|ctxs| ctxs.into_iter().next().map(|c| c.id));
    let mut observations = match store.list_observations(site_ctx) {
        Ok(observations) => observations,
        Err(error) => return Err(error.to_string()),
    };
    // Only surface detection observations — corrections and other cg-internal bookkeeping
    // (ingestion, digest, etc.) must never appear on the events display or events API surface.
    observations.retain(|o| o.observation_type == "detection");
    observations.sort_by(|left, right| {
        right
            .observed_at
            .cmp(&left.observed_at)
            .then_with(|| right.id.as_uuid().cmp(&left.id.as_uuid()))
    });
    observations.truncate(limit);
    let first_observation = observations.first();
    let observation = match first_observation.map(|row| store.get_observation(row.id)) {
        Some(Ok(observation)) => Some(observation),
        Some(Err(error)) => return Err(error.to_string()),
        None => None,
    };
    let _observation_seen = observation.as_ref().map(|observation| observation.id);
    let observed_entity = observation.as_ref();
    let entity = match observed_entity.map(|row| store.get_entity(row.entity_id)) {
        Some(Ok(entity)) => entity,
        Some(Err(error)) => return Err(error.to_string()),
        None => None,
    };
    let _entity_name = entity.as_ref().map(|entity| entity.name.clone());
    let _observations_response = &observations; // StoreBackedEventsResponse
    let _observation_response = &observation; // StoreBackedEventsResponse
    let _entity_response = &entity; // StoreBackedEventsResponse
    let response = StoreBackedEventsResponse::from_store_reads(observations, observation, entity)?;
    Ok(response)
}

pub(crate) fn format_why_cli(dto: &StoreBackedWhyResponse) -> String {
    let mut line = format!(
        "selection={} observation_id={} observed_at={} class={} confidence={:.6} bbox={} frame_index={} clip_ref={} detector_image_ref={} camera_id={} camera_name={} camera_rtsp_url={} context_id={} site_name={} decision_id={} intention_id={} intention_description={} model_id={} threshold={}\n",
        dto.selection,
        dto.observation_id,
        dto.observed_at,
        dto.class_name,
        dto.confidence,
        dto.bbox,
        dto.frame_index,
        dto.clip_ref,
        dto.detector_image_ref,
        dto.camera_id,
        dto.camera_name,
        dto.camera_rtsp_url,
        dto.context_id,
        dto.site_name,
        dto.decision_id,
        dto.intention_id,
        dto.intention_description,
        dto.model_id,
        dto.threshold
    );
    if let Some(name) = &dto.recognition_name {
        line.push_str(&format!(
            "recognition_name={name} recognition_score={:.6} recognition_reference={} recognition_enrolled_by={}\n",
            dto.recognition_score.unwrap_or_default(),
            dto.recognition_reference_label.as_deref().unwrap_or(""),
            dto.recognition_enrolled_by.as_deref().unwrap_or(""),
        ));
    }
    line
}

pub(crate) fn format_events_cli(dto: &StoreBackedEventsResponse) -> String {
    let _field_use = (
        &dto.rows,
        &dto.observation_id,
        &dto.observed_at,
        &dto.camera_name,
        &dto.class_name,
        dto.confidence,
        &dto.bbox,
        dto.frame_index,
        &dto.clip_ref,
        &dto.detector_image_ref,
    );
    let mut output = String::new();
    for row in &dto.rows {
        output.push_str(&format!(
            "observation_id={} observed_at={} camera={} class={} confidence={} bbox={} frame_index={} clip={} detector_image={}\n",
            row.observation_id,
            row.observed_at,
            row.camera_name,
            row.class_name,
            row.confidence,
            row.bbox,
            row.frame_index,
            row.clip_ref,
            row.detector_image_ref
        ));
    }
    output
}

fn resolve_observation_id(
    observations: &[Observation],
    request: &str,
) -> Result<ObservationId, String> {
    if request == "--latest" || request.is_empty() {
        // `--latest` must select the newest DETECTION, never a correction or other type.
        // `vigil why --latest` is a detection-review entry point; picking a correction
        // observation would break the provenance walk (no decision/intention chain).
        return observations
            .iter()
            .filter(|o| o.observation_type == "detection")
            .max_by(|left, right| {
                left.observed_at
                    .cmp(&right.observed_at)
                    .then_with(|| left.id.as_uuid().cmp(&right.id.as_uuid()))
            })
            .map(|observation| observation.id)
            .ok_or_else(|| "event not found".to_string());
    }
    observations
        .iter()
        .find(|observation| observation.id.to_string() == request)
        .map(|observation| observation.id)
        .ok_or_else(|| "event not found".to_string())
}

fn why_selection_label(request: &str) -> String {
    if request == "--latest" || request.is_empty() {
        "newest-observed-at".to_string()
    } else {
        "requested".to_string()
    }
}

fn resolve_decision_id(
    audit_entries: &[AuditEntry],
    observation: &Observation,
) -> Result<context_graph::DecisionId, String> {
    if let Some(expected) = observation
        .observed_properties
        .get("detector_decision_id")
        .and_then(Value::as_str)
    {
        for entry in audit_entries {
            if let AuditTarget::Decision(id) = entry.target
                && id.to_string() == expected
            {
                return Ok(id);
            }
        }
    }
    audit_entries
        .iter()
        .filter_map(|entry| match entry.target {
            AuditTarget::Decision(id) => Some(id),
            _ => None,
        })
        .next_back()
        .ok_or_else(|| "decision not found".to_string())
}

fn first_evidence(observation: &Observation) -> Result<&EvidenceRef, String> {
    observation
        .evidence
        .first()
        .ok_or_else(|| "observation omitted evidence".to_string())
}

fn first_image_evidence(observation: &Observation) -> Option<&EvidenceRef> {
    observation
        .evidence
        .iter()
        .find(|evidence| evidence.kind == EvidenceKind::ImageFrame)
}
