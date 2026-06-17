use context_graph::{
    AuditEntry, AuditFilter, AuditTarget, Context, Decision, Entity, EvidenceKind, EvidenceRef,
    Intention, Observation, ObservationId, Store,
};
use serde_json::Value;

use crate::runtime_stats::{RuntimeStats, format_stats};

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

pub(crate) fn handle_owner_request(store: &Store, stats: &RuntimeStats, request: &str) -> String {
    let request = request.trim();
    let mut parts = request.splitn(2, ' ');
    let command = parts.next().unwrap_or_default();
    let argument = parts.next().unwrap_or_default().trim();
    let body = match command {
        "why" => match handle_why_read(store, argument) {
            Ok(response) => format_why_cli(&response),
            Err(error) => format!("not-found {error}\n"),
        },
        "events" => match handle_events_read(store, 100) {
            Ok(response) => format_events_cli(&response),
            Err(error) => format!("events-error {error}\n"),
        },
        "stats" => format_stats(stats),
        _ => format!("owner-error unknown-command={command}\n"),
    };
    format!("served-by=af_unix\n{body}")
}

pub(crate) fn handle_why_read(
    store: &Store,
    request: &str,
) -> Result<StoreBackedWhyResponse, String> {
    let audit_entries = match store.audit_query(AuditFilter::default()) {
        Ok(audit_entries) => audit_entries,
        Err(error) => return Err(error.to_string()),
    };
    let observations = match store.list_observations(None) {
        Ok(observations) => observations,
        Err(error) => return Err(error.to_string()),
    };
    let observation_id = resolve_observation_id(&observations, request)?;
    let observation = match store.get_observation(observation_id) {
        Ok(observation) => observation,
        Err(error) => return Err(error.to_string()),
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
    let response = StoreBackedWhyResponse::from_store_reads(
        selection,
        audit_entries, // StoreBackedWhyResponse
        observation,   // StoreBackedWhyResponse
        decision,      // StoreBackedWhyResponse
        intention,     // StoreBackedWhyResponse
        entity,        // StoreBackedWhyResponse
        context,       // StoreBackedWhyResponse
    )?;
    Ok(response)
}

pub(crate) fn handle_events_read(
    store: &Store,
    limit: usize,
) -> Result<StoreBackedEventsResponse, String> {
    let mut observations = match store.list_observations(None) {
        Ok(observations) => observations,
        Err(error) => return Err(error.to_string()),
    };
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
    format!(
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
    )
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
        return observations
            .iter()
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
