use std::collections::BTreeMap;

use context_graph::{
    ContextId, EvidenceId, EvidenceKind, EvidenceProducer, EvidenceRef, ObservationId,
    RecordObservation, RetentionStatus, Store,
};
use serde_json::Value;

use crate::live_read::{handle_events_read, handle_why_read};

// ── Public correction types ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CorrectionRequest {
    pub detection_id: String,
    pub label: Option<String>,
    pub correction_type: CorrectionType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorrectionType {
    Identity,
    WrongClass,
    FalseAlarm,
}

#[derive(Debug, Clone)]
pub struct CorrectionReceipt {
    pub correction_id: String,
}

#[derive(Debug)]
pub enum CorrectionError {
    NoAnchor(String),
    WriteFailed(String),
}

impl std::fmt::Display for CorrectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAnchor(msg) => write!(f, "no anchor: {msg}"),
            Self::WriteFailed(msg) => write!(f, "write failed: {msg}"),
        }
    }
}

impl std::error::Error for CorrectionError {}

// ── Public review types ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RecordedCorrection {
    pub label: Option<String>,
    pub correction_type: CorrectionType,
    pub anchored_detection_id: String,
}

#[derive(Debug, Clone)]
pub struct WhyView {
    pub observation_id: String,
    pub observed_at: String,
    pub clip_ref: String,
    pub camera_id: String,
    pub camera_name: String,
    pub camera_rtsp_url: String,
    pub context_id: String,
    pub site_name: String,
    pub decision_id: String,
    pub intention_id: String,
    pub intention_description: String,
    pub model_id: String,
    pub threshold: f64,
    pub class_name: String,
    pub confidence: f64,
    pub bbox: String,
    pub frame_index: u64,
    pub detector_image_ref: String,
    pub corrections: Vec<RecordedCorrection>,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub observation_id: String,
    pub observed_at: String,
    pub camera_name: String,
    pub class_name: String,
    pub confidence: f64,
    pub bbox: String,
    pub frame_index: u64,
    pub zone: Option<String>,
    pub clip_ref: String,
    pub detector_image_ref: String,
    /// True when a WrongClass or FalseAlarm correction has been recorded against this
    /// detection.  An Identity ("confirmed") correction does NOT set this flag — it is
    /// a positive signal surfaced separately via `confirmed`.
    pub correction_recorded: bool,
    /// True when an Identity correction ("confirmed") has been recorded against this
    /// detection.  Distinct from `correction_recorded` which is for WrongClass / FalseAlarm.
    pub confirmed: bool,
    /// Server-decided latest correction kind for this detection.  This is the
    /// durable authority the Home Assistant card renders; it is not derived
    /// client-side from the legacy booleans.
    pub current_correction: Option<CorrectionType>,
    /// Latest corrected-to label from WrongClass corrections, if any.
    pub corrected_label: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EventsView {
    pub rows: Vec<EventRow>,
}

#[derive(Debug)]
pub enum ReviewError {
    NotFound(String),
    StoreError(String),
}

impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
            Self::StoreError(msg) => write!(f, "store error: {msg}"),
        }
    }
}

impl std::error::Error for ReviewError {}

// ── Correction type serialization helpers ──────────────────────────────────

fn correction_type_to_str(ct: &CorrectionType) -> &'static str {
    match ct {
        CorrectionType::Identity => "Identity",
        CorrectionType::WrongClass => "WrongClass",
        CorrectionType::FalseAlarm => "FalseAlarm",
    }
}

fn correction_type_from_str(s: &str) -> Option<CorrectionType> {
    match s {
        "Identity" => Some(CorrectionType::Identity),
        "WrongClass" => Some(CorrectionType::WrongClass),
        "FalseAlarm" => Some(CorrectionType::FalseAlarm),
        _ => None,
    }
}

// ── Correction implementation ──────────────────────────────────────────────

/// Write a human correction anchored to the named detection into cg authority.
///
/// The correction is stored as a cg Observation whose `observed_properties` carry:
///   - `anchored_detection_id`: the detection ObservationId string (the link)
///   - `correction_type`: "Identity" | "WrongClass" | "FalseAlarm"
///   - `label`: the label string, if provided
///
/// Idempotent: if an identical correction (same detection_id + correction_type
/// + label) already exists, returns the existing receipt without a second write.
///
/// Returns `NoAnchor` when the detection_id is malformed or unknown.
/// Returns `WriteFailed` when the cg write fails (e.g. read-only data dir).
pub fn record_correction(
    store: &Store,
    request: CorrectionRequest,
) -> Result<CorrectionReceipt, CorrectionError> {
    // Parse detection_id → ObservationId.
    let uuid = uuid::Uuid::parse_str(&request.detection_id)
        .map_err(|e| CorrectionError::NoAnchor(format!("invalid detection id: {e}")))?;
    let detection_obs_id = ObservationId::from(uuid);

    // Verify the detection exists in cg.
    let detection_obs = store
        .get_observation(detection_obs_id)
        .map_err(|e| CorrectionError::NoAnchor(format!("detection not found: {e}")))?;

    // Reject non-detection anchors — a correction must be anchored to a detection, not to
    // another correction or any other cg-internal observation type.  Anchoring a correction
    // to itself or to a prior correction would break the provenance model.
    if detection_obs.observation_type != "detection" {
        return Err(CorrectionError::NoAnchor(format!(
            "anchor {} has type '{}', expected 'detection'",
            request.detection_id, detection_obs.observation_type
        )));
    }

    let correction_type_str = correction_type_to_str(&request.correction_type);

    // Dedup: return existing receipt if an identical correction is already in cg.
    // Scope to the detection's context so the scan stays bounded — never the whole store.
    let all_obs = store
        .list_observations(Some(detection_obs.context_id))
        .map_err(|e| CorrectionError::WriteFailed(format!("list_observations failed: {e}")))?;

    for obs in &all_obs {
        let anchored = obs
            .observed_properties
            .get("anchored_detection_id")
            .or_else(|| obs.properties.get("anchored_detection_id"))
            .and_then(|v| v.as_str());
        let ct = obs
            .observed_properties
            .get("correction_type")
            .or_else(|| obs.properties.get("correction_type"))
            .and_then(|v| v.as_str());
        let lbl = obs
            .observed_properties
            .get("label")
            .or_else(|| obs.properties.get("label"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if anchored == Some(request.detection_id.as_str())
            && ct == Some(correction_type_str)
            && lbl == request.label
        {
            return Ok(CorrectionReceipt {
                correction_id: obs.id.to_string(),
            });
        }
    }

    // Build the correction observation.
    let correction_id = ObservationId::new_v7();
    let observed_at = chrono::Utc::now();

    let mut observed_properties: BTreeMap<String, Value> = BTreeMap::new();
    observed_properties.insert(
        "anchored_detection_id".to_string(),
        Value::String(request.detection_id.clone()),
    );
    observed_properties.insert(
        "correction_type".to_string(),
        Value::String(correction_type_str.to_string()),
    );
    if let Some(label) = &request.label {
        observed_properties.insert("label".to_string(), Value::String(label.clone()));
    }

    // cg requires at least one evidence ref.  A human correction is its own
    // provenance: a StructuredSignal evidence anchoring this correction act.
    let evidence_id = EvidenceId::new_v7();
    let evidence = vec![EvidenceRef {
        id: evidence_id,
        observation_id: correction_id,
        context_id: detection_obs.context_id,
        kind: EvidenceKind::StructuredSignal,
        source_ref: format!(
            "vigil-edge://correction/{correction_type_str}/{}",
            request.detection_id
        ),
        captured_at: Some(observed_at),
        producer: EvidenceProducer {
            system: "vigil".to_string(),
            pipeline_version: "correction-v1".to_string(),
            ..Default::default()
        },
        retention_status: RetentionStatus::NotStored,
        ..Default::default()
    }];

    store
        .record_observation(RecordObservation {
            id: correction_id,
            entity_id: detection_obs.entity_id,
            context_id: detection_obs.context_id,
            observation_type: "correction".to_string(),
            source: "vigil".to_string(),
            observed_at,
            evidence,
            observed_properties,
            state_delta: BTreeMap::new(),
            properties: BTreeMap::new(),
            embeddings: vec![],
        })
        .map_err(|e| CorrectionError::WriteFailed(format!("cg record_observation failed: {e}")))?;

    Ok(CorrectionReceipt {
        correction_id: correction_id.to_string(),
    })
}

/// Return the provenance walk for a detection plus all corrections anchored to it.
pub fn review_why(store: &Store, detection_id: &str) -> Result<WhyView, ReviewError> {
    if detection_id != "--latest" && !detection_id.is_empty() {
        let uuid = uuid::Uuid::parse_str(detection_id)
            .map_err(|e| ReviewError::NotFound(format!("invalid detection id: {e}")))?;
        let observation = store
            .get_observation(ObservationId::from(uuid))
            .map_err(|e| ReviewError::NotFound(format!("detection not found: {e}")))?;
        if observation.observation_type != "detection" {
            return Err(ReviewError::NotFound(format!(
                "observation {} has type '{}', expected 'detection'",
                detection_id, observation.observation_type
            )));
        }
    }
    let resp = handle_why_read(store, detection_id).map_err(ReviewError::StoreError)?;

    // Find all corrections anchored to this detection.
    // Scope to the detection's context so the scan stays bounded (vigil uses one
    // context per site; all corrections land in the same context as their detection).
    let context_id = uuid::Uuid::parse_str(&resp.context_id)
        .map(ContextId::from)
        .map_err(|e| ReviewError::StoreError(format!("parse detection context_id: {e}")))?;
    let all_obs = store
        .list_observations(Some(context_id))
        .map_err(|e| ReviewError::StoreError(e.to_string()))?;

    let mut corrections: Vec<_> = all_obs
        .iter()
        .filter_map(|obs| {
            let anchored = obs
                .observed_properties
                .get("anchored_detection_id")
                .or_else(|| obs.properties.get("anchored_detection_id"))
                .and_then(|v| v.as_str())?;
            if anchored != detection_id {
                return None;
            }
            let ct_str = obs
                .observed_properties
                .get("correction_type")
                .or_else(|| obs.properties.get("correction_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let correction_type = correction_type_from_str(ct_str)?;
            let label = obs
                .observed_properties
                .get("label")
                .or_else(|| obs.properties.get("label"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some((
                obs.observed_at,
                obs.id,
                RecordedCorrection {
                    label,
                    correction_type,
                    anchored_detection_id: anchored.to_string(),
                },
            ))
        })
        .collect();
    corrections.sort_by(|(left_at, left_id, _left), (right_at, right_id, _right)| {
        right_at
            .cmp(left_at)
            .then_with(|| right_id.as_uuid().cmp(&left_id.as_uuid()))
    });
    let corrections = corrections
        .into_iter()
        .map(|(_observed_at, _id, correction)| correction)
        .collect();

    Ok(WhyView {
        observation_id: resp.observation_id,
        observed_at: resp.observed_at,
        clip_ref: resp.clip_ref,
        camera_id: resp.camera_id,
        camera_name: resp.camera_name,
        camera_rtsp_url: resp.camera_rtsp_url,
        context_id: resp.context_id,
        site_name: resp.site_name,
        decision_id: resp.decision_id,
        intention_id: resp.intention_id,
        intention_description: resp.intention_description,
        model_id: resp.model_id,
        threshold: resp.threshold,
        class_name: resp.class_name,
        confidence: resp.confidence,
        bbox: resp.bbox,
        frame_index: resp.frame_index,
        detector_image_ref: resp.detector_image_ref,
        corrections,
    })
}

/// List recent detection events, each flagged whether a correction has been recorded.
pub fn review_events(store: &Store, limit: usize) -> Result<EventsView, ReviewError> {
    let resp = handle_events_read(store, limit).map_err(ReviewError::StoreError)?;

    // Build per-type sets of detection IDs that have corrections in cg.
    // Scope to the detection's context (vigil uses one context per site; all
    // corrections live in the same context as their detection).
    // If there are no event rows, no corrections are possible — skip the scan.
    //
    // Identity ("confirmed") is a POSITIVE signal: it must NOT set `correction_recorded`.
    // Only WrongClass and FalseAlarm corrections set `correction_recorded = true`.
    let correction_summary: BTreeMap<String, EventCorrectionSummary> =
        if let Some(first_row) = resp.rows.first() {
            let context_id_opt = uuid::Uuid::parse_str(&first_row.observation_id)
                .ok()
                .map(ObservationId::from)
                .and_then(|obs_id| store.get_observation(obs_id).ok())
                .map(|obs| obs.context_id);
            if let Some(ctx_id) = context_id_opt {
                let mut summary = BTreeMap::new();
                for obs in store.list_observations(Some(ctx_id)).unwrap_or_default() {
                    let anchored = obs
                        .observed_properties
                        .get("anchored_detection_id")
                        .or_else(|| obs.properties.get("anchored_detection_id"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let ct = obs
                        .observed_properties
                        .get("correction_type")
                        .or_else(|| obs.properties.get("correction_type"))
                        .and_then(|v| v.as_str())
                        .and_then(correction_type_from_str);
                    let label = obs
                        .observed_properties
                        .get("label")
                        .or_else(|| obs.properties.get("label"))
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                    if let Some(id) = anchored {
                        if let Some(correction_type) = ct {
                            summary
                                .entry(id)
                                .or_insert_with(EventCorrectionSummary::default)
                                .apply(&obs.observed_at, correction_type, label);
                        }
                    }
                }
                summary
            } else {
                BTreeMap::new()
            }
        } else {
            BTreeMap::new()
        };

    let rows = resp
        .rows
        .iter()
        .map(|row| EventRow {
            observation_id: row.observation_id.clone(),
            observed_at: row.observed_at.clone(),
            camera_name: row.camera_name.clone(),
            class_name: row.class_name.clone(),
            confidence: row.confidence,
            bbox: row.bbox.clone(),
            frame_index: row.frame_index,
            zone: row.zone.clone(),
            clip_ref: row.clip_ref.clone(),
            detector_image_ref: row.detector_image_ref.clone(),
            correction_recorded: correction_summary
                .get(&row.observation_id)
                .map(|summary| summary.corrected)
                .unwrap_or(false),
            confirmed: correction_summary
                .get(&row.observation_id)
                .map(|summary| summary.confirmed)
                .unwrap_or(false),
            current_correction: correction_summary
                .get(&row.observation_id)
                .and_then(|summary| summary.current_correction.clone()),
            corrected_label: correction_summary
                .get(&row.observation_id)
                .and_then(|summary| summary.corrected_label.clone()),
        })
        .collect();

    Ok(EventsView { rows })
}

#[derive(Default)]
struct EventCorrectionSummary {
    corrected: bool,
    confirmed: bool,
    current_correction: Option<CorrectionType>,
    current_observed_at: Option<chrono::DateTime<chrono::Utc>>,
    corrected_label: Option<String>,
}

impl EventCorrectionSummary {
    fn apply(
        &mut self,
        observed_at: &chrono::DateTime<chrono::Utc>,
        correction_type: CorrectionType,
        label: Option<String>,
    ) {
        match correction_type {
            CorrectionType::Identity => {
                self.confirmed = true;
            }
            CorrectionType::WrongClass => {
                self.corrected = true;
            }
            CorrectionType::FalseAlarm => {
                self.corrected = true;
            }
        }

        if self
            .current_observed_at
            .map(|current| *observed_at > current)
            .unwrap_or(true)
        {
            self.corrected_label = match correction_type {
                CorrectionType::WrongClass => label,
                CorrectionType::Identity | CorrectionType::FalseAlarm => None,
            };
            self.current_correction = Some(correction_type);
            self.current_observed_at = Some(*observed_at);
        }
    }
}

/// Read the PNG bytes for the most-recent detector evidence frame for the named camera.
///
/// Used by the snapshot MQTT control action to publish a real frame to the HA image entity.
/// The PNG was written by `write_detector_evidence_image` in runtime.rs and referenced as
/// `"vigil-edge:clip/{filename}"` in the detection observation's `properties["detector_evidence_ref"]`.
///
/// Returns `None` when no detection, no evidence ref, or the file has been cleaned up.
/// Callers tolerate None gracefully (log + skip the publish).
pub(crate) fn read_latest_detection_image(
    store: &Store,
    camera_id_slug: &str,
    data_dir: &std::path::Path,
) -> Option<Vec<u8>> {
    use context_graph::{EntityType, EvidenceKind, ListEntityFilter};

    let entities = store
        .list_entities(ListEntityFilter {
            entity_type: Some(EntityType::Device),
            ..Default::default()
        })
        .ok()?;

    let cam = entities
        .iter()
        .find(|e| camera_name_to_slug(&e.name) == camera_id_slug)?;

    // Scope the scan to the site context (vigil is one-context-per-site).
    // list_contexts() is cheaper than probing observations — typically returns one entry.
    let site_ctx = store
        .list_contexts()
        .ok()
        .and_then(|ctxs| ctxs.into_iter().next().map(|c| c.id));
    let all_obs = store.list_observations(site_ctx).ok()?;

    let detection = all_obs
        .iter()
        .filter(|o| o.observation_type == "detection" && o.entity_id == cam.id)
        .max_by_key(|o| o.observed_at)?;

    // The detector evidence PNG path is stored as either:
    // 1. An EvidenceRef with kind == ImageFrame whose source_ref = "vigil-edge:clip/{filename}"
    // 2. properties["detector_evidence_ref"] = "vigil-edge:clip/{filename}" (legacy fallback)
    // Try the evidence list first to match the live_read.rs / why-view read path.
    let evidence_ref_str = detection
        .evidence
        .iter()
        .find(|ev| ev.kind == EvidenceKind::ImageFrame)
        .map(|ev| ev.source_ref.as_str())
        .or_else(|| {
            detection
                .properties
                .get("detector_evidence_ref")
                .and_then(|v| v.as_str())
        })?;

    let file_name = evidence_ref_str.strip_prefix("vigil-edge:clip/")?;
    std::fs::read(data_dir.join("clips").join(file_name)).ok()
}

/// Convert a camera display name to the dash-slug used as `camera_id` in MQTT
/// control commands.  Mirrors the `camera_slug` function in `runtime.rs`.
fn camera_name_to_slug(name: &str) -> String {
    let mut result = String::new();
    let mut prev_dash = false;
    for ch in name.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            result.push(ch);
            prev_dash = false;
        } else if !prev_dash && !result.is_empty() {
            result.push('-');
            prev_dash = true;
        }
    }
    while result.ends_with('-') {
        result.pop();
    }
    if result.is_empty() {
        "camera".to_string()
    } else {
        result
    }
}
