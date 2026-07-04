//! Recognition: crop → embed → enroll → match → named entity.
//!
//! A detection of a covered class is cropped from the native-resolution frame,
//! embedded by the site's vision embedder (registered into the context-graph
//! store through the same seam the bundled embedders use), and matched
//! open-set against the site's enrolled references. A match names the event;
//! below the threshold the sighting stays honestly unknown. Every match (and
//! covered non-match) is recorded as a "recognition" observation in site
//! memory, anchored to its detection — the provenance `vigil why` renders.
//! Enrollment rides the correction seam: marking a detection with a name
//! creates the typed entity (person / animal / vehicle) and enrolls the
//! sighting's own vector as its first reference. Everything is deterministic
//! vectors end to end — no model judgment anywhere in this path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use context_graph::{
    CreateEntity, Embedder, EmbedderConfig, EmbedderRegistration, EmbeddingInput,
    EmbeddingSpaceDecl, EntityId, EntityReferenceMatchOptions, EntityType, EvidenceKind,
    EvidenceProducer, EvidenceRef, ObservationId, RecordObservation, RetentionStatus, Store,
    StoreConfig,
};
use serde_json::Value;

/// Site recognition configuration. Class lists and the threshold are
/// operator-configurable; the defaults are generic COCO classes, never
/// site-specific.
#[derive(Debug, Clone)]
pub struct RecognitionConfig {
    pub enabled: bool,
    pub weights_dir: Option<PathBuf>,
    pub embedding_space_id: String,
    pub match_threshold: f64,
    pub covered_classes: Vec<String>,
}

impl Default for RecognitionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            weights_dir: None,
            embedding_space_id: "vigil_site_vision_v1".to_string(),
            match_threshold: 0.6,
            covered_classes: [
                "person",
                "dog",
                "cat",
                "bird",
                "horse",
                "sheep",
                "cow",
                "car",
                "truck",
                "bus",
                "motorcycle",
                "bicycle",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        }
    }
}

pub fn class_is_covered(config: &RecognitionConfig, class: &str) -> bool {
    config
        .covered_classes
        .iter()
        .any(|covered| covered.eq_ignore_ascii_case(class))
}

/// The COCO class index for a class name, or None if not a COCO class. The
/// detector emits indices; recognition config names classes — this is the
/// bridge. Mirrors `yolox_detector::COCO_CLASSES` order.
pub fn coco_class_index(name: &str) -> Option<usize> {
    crate::yolox_detector::COCO_CLASSES
        .iter()
        .position(|coco| coco.eq_ignore_ascii_case(name))
}

/// The COCO indices the detector should emit for a recognition config: every
/// covered class that maps to a COCO class, always including person (the
/// baseline NVR promise is never dropped by a recognition config).
pub fn covered_class_indices(config: &RecognitionConfig) -> Vec<usize> {
    let mut indices: Vec<usize> = std::iter::once(0)
        .chain(
            config
                .covered_classes
                .iter()
                .filter_map(|c| coco_class_index(c)),
        )
        .collect();
    indices.sort_unstable();
    indices.dedup();
    indices
}

/// The entity type a recognized subject of this detector class enrolls as.
pub fn entity_type_for_class(class: &str) -> EntityType {
    match class.to_ascii_lowercase().as_str() {
        "person" => EntityType::Person,
        "dog" | "cat" | "bird" | "horse" | "sheep" | "cow" | "elephant" | "bear" | "zebra"
        | "giraffe" => EntityType::Animal,
        "car" | "truck" | "bus" | "motorcycle" | "bicycle" | "boat" | "train" | "airplane" => {
            EntityType::Vehicle
        }
        _ => EntityType::Device,
    }
}

/// Map a detector-space bbox string ("x1,y1,x2,y2" in `det_w`×`det_h` pixels)
/// back to a clamped (x, y, w, h) rect in original-frame pixels. The detector
/// input is a plain squash resize, so the inverse is independent x/y scaling —
/// no padding offset. Returns `None` for malformed or degenerate boxes.
pub fn map_bbox_to_frame(
    bbox: &str,
    det_w: u32,
    det_h: u32,
    frame_w: u32,
    frame_h: u32,
) -> Option<(u32, u32, u32, u32)> {
    let parts: Vec<f64> = bbox
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .ok()?;
    let [x1, y1, x2, y2] = parts.as_slice() else {
        return None;
    };
    if x2 <= x1 || y2 <= y1 || det_w == 0 || det_h == 0 || frame_w == 0 || frame_h == 0 {
        return None;
    }
    let sx = frame_w as f64 / det_w as f64;
    let sy = frame_h as f64 / det_h as f64;
    let fx1 = (x1 * sx).max(0.0).min(frame_w as f64 - 1.0);
    let fy1 = (y1 * sy).max(0.0).min(frame_h as f64 - 1.0);
    let fx2 = (x2 * sx).max(0.0).min(frame_w as f64);
    let fy2 = (y2 * sy).max(0.0).min(frame_h as f64);
    let x = fx1.floor() as u32;
    let y = fy1.floor() as u32;
    let w = ((fx2 - fx1).round() as u32).min(frame_w - x);
    let h = ((fy2 - fy1).round() as u32).min(frame_h - y);
    if w == 0 || h == 0 {
        return None;
    }
    Some((x, y, w, h))
}

/// Crop a rect out of a packed-RGB8 frame and encode it as PNG bytes.
pub fn crop_png(
    rgb: &[u8],
    width: u32,
    height: u32,
    rect: (u32, u32, u32, u32),
) -> Result<Vec<u8>, String> {
    let expected = width as usize * height as usize * 3;
    if rgb.len() != expected {
        return Err(format!(
            "frame buffer is {} bytes, expected {expected} for {width}x{height} rgb8",
            rgb.len()
        ));
    }
    let image = image::RgbImage::from_raw(width, height, rgb.to_vec())
        .ok_or_else(|| "frame buffer did not form an image".to_string())?;
    let (x, y, w, h) = rect;
    if x + w > width || y + h > height || w == 0 || h == 0 {
        return Err(format!("crop rect {rect:?} outside {width}x{height} frame"));
    }
    let crop = image::imageops::crop_imm(&image, x, y, w, h).to_image();
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(crop)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .map_err(|e| format!("png encode failed: {e}"))?;
    Ok(bytes)
}

/// The outcome of matching one sighting against the site library.
#[derive(Debug, Clone)]
pub struct MatchOutcome {
    pub entity_id: Option<String>,
    pub name: Option<String>,
    pub reference_label: Option<String>,
    pub score: f64,
    pub embedding_space_id: String,
}

/// Embed a crop through the registered vision embedder and match it open-set
/// against the site's enrolled references. Below the threshold the outcome
/// carries no name — unknown stays unknown.
pub fn match_crop(
    store: &Store,
    embedder: &dyn Embedder,
    embedding_space_id: &str,
    context_id: context_graph::ContextId,
    crop_png_bytes: &[u8],
    threshold: f64,
) -> Result<MatchOutcome, String> {
    let output = embedder
        .embed(EmbeddingInput::ImageBytes(crop_png_bytes.to_vec()))
        .map_err(|e| format!("embed failed: {e}"))?;
    match_vector(
        store,
        embedding_space_id,
        context_id,
        &output.vector,
        threshold,
    )
}

/// Embed a crop and only allow matches to entities of the same semantic type as
/// the detector class. This prevents a high-scoring vehicle or animal crop from
/// resolving to a named person entity.
pub fn match_crop_for_class(
    store: &Store,
    embedder: &dyn Embedder,
    embedding_space_id: &str,
    context_id: context_graph::ContextId,
    crop_png_bytes: &[u8],
    threshold: f64,
    class: &str,
) -> Result<MatchOutcome, String> {
    let output = embedder
        .embed(EmbeddingInput::ImageBytes(crop_png_bytes.to_vec()))
        .map_err(|e| format!("embed failed: {e}"))?;
    match_vector_for_class(
        store,
        embedding_space_id,
        context_id,
        &output.vector,
        threshold,
        class,
    )
}

/// Match an already-computed probe vector (the hot path holds one).
pub fn match_vector(
    store: &Store,
    embedding_space_id: &str,
    context_id: context_graph::ContextId,
    probe: &[f32],
    threshold: f64,
) -> Result<MatchOutcome, String> {
    let matches = store
        .match_entity_reference(
            embedding_space_id,
            probe,
            EntityReferenceMatchOptions {
                context_id: Some(context_id),
                top_k: Some(1),
                min_score: None,
            },
        )
        .map_err(|e| format!("match failed: {e}"))?;
    let best = matches.first();
    let score = best.map(|m| m.score as f64).unwrap_or(0.0);
    if let Some(best) = best
        && score >= threshold
    {
        let name = store
            .get_entity(best.entity_id)
            .map_err(|e| format!("matched entity read failed: {e}"))?
            .map(|entity| entity.name);
        return Ok(MatchOutcome {
            entity_id: Some(best.entity_id.to_string()),
            name,
            reference_label: best.label.clone(),
            score,
            embedding_space_id: embedding_space_id.to_string(),
        });
    }
    Ok(MatchOutcome {
        entity_id: None,
        name: None,
        reference_label: None,
        score,
        embedding_space_id: embedding_space_id.to_string(),
    })
}

/// Match an already-computed probe vector, constrained to the entity type that
/// the detector class enrolls as.
pub fn match_vector_for_class(
    store: &Store,
    embedding_space_id: &str,
    context_id: context_graph::ContextId,
    probe: &[f32],
    threshold: f64,
    class: &str,
) -> Result<MatchOutcome, String> {
    let target_entity_type = entity_type_for_class(class);
    let matches = store
        .match_entity_reference(
            embedding_space_id,
            probe,
            EntityReferenceMatchOptions {
                context_id: Some(context_id),
                top_k: Some(8),
                min_score: None,
            },
        )
        .map_err(|e| format!("match failed: {e}"))?;
    let score = matches.first().map(|m| m.score as f64).unwrap_or(0.0);
    for candidate in matches.iter().filter(|candidate| {
        let candidate_score = candidate.score as f64;
        candidate_score >= threshold
    }) {
        let entity = store
            .get_entity(candidate.entity_id)
            .map_err(|e| format!("matched entity read failed: {e}"))?
            .ok_or_else(|| format!("matched entity {} was missing", candidate.entity_id))?;
        if entity.entity_type != target_entity_type {
            continue;
        }
        return Ok(MatchOutcome {
            entity_id: Some(candidate.entity_id.to_string()),
            name: Some(entity.name),
            reference_label: candidate.label.clone(),
            score: candidate.score as f64,
            embedding_space_id: embedding_space_id.to_string(),
        });
    }
    Ok(MatchOutcome {
        entity_id: None,
        name: None,
        reference_label: None,
        score,
        embedding_space_id: embedding_space_id.to_string(),
    })
}

fn recognition_producer() -> EvidenceProducer {
    EvidenceProducer {
        system: "vigil".to_string(),
        model_name: "vision-embedder".to_string(),
        model_version: "1".to_string(),
        pipeline_version: "recognition-v1".to_string(),
    }
}

/// Record the sighting into site memory: a "recognition" observation anchored
/// to its detection. A match is recorded AGAINST the matched entity; a covered
/// non-match against the camera. The probe vector rides the properties lane —
/// durable and replayable, never in the enrolled-reference match index.
#[allow(clippy::too_many_arguments)]
pub fn record_match_observation(
    store: &Store,
    camera_entity: EntityId,
    context_id: context_graph::ContextId,
    anchored_detection_id: &str,
    outcome: &MatchOutcome,
    probe: &[f32],
    class: &str,
    source_ref: &str,
) -> Result<ObservationId, String> {
    let id = ObservationId::new_v7();
    let observed_at = chrono::Utc::now();
    let entity_id = outcome
        .entity_id
        .as_deref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(EntityId::from)
        .unwrap_or(camera_entity);
    let mut observed_properties: BTreeMap<String, Value> = BTreeMap::from([
        (
            "anchored_detection_id".to_string(),
            Value::String(anchored_detection_id.to_string()),
        ),
        (
            "matched".to_string(),
            Value::Bool(outcome.entity_id.is_some()),
        ),
        ("score".to_string(), serde_json::json!(outcome.score)),
        ("class".to_string(), Value::String(class.to_string())),
        (
            "probe_vector".to_string(),
            serde_json::json!(probe.to_vec()),
        ),
        (
            "embedding_space_id".to_string(),
            Value::String(outcome.embedding_space_id.clone()),
        ),
    ]);
    if let Some(name) = &outcome.name {
        observed_properties.insert("matched_name".to_string(), Value::String(name.clone()));
    }
    if let Some(label) = &outcome.reference_label {
        observed_properties.insert("reference_label".to_string(), Value::String(label.clone()));
    }
    let evidence_id = context_graph::EvidenceId::new_v7();
    store
        .record_observation(RecordObservation {
            id,
            entity_id,
            context_id,
            observation_type: "recognition".to_string(),
            source: "vigil".to_string(),
            observed_at,
            evidence: vec![EvidenceRef {
                id: evidence_id,
                observation_id: id,
                context_id,
                kind: EvidenceKind::ImageFrame,
                source_ref: source_ref.to_string(),
                mime_type: Some("image/png".to_string()),
                captured_at: Some(observed_at),
                producer: recognition_producer(),
                retention_status: RetentionStatus::RetainedExternal,
                ..Default::default()
            }],
            observed_properties,
            state_delta: BTreeMap::new(),
            properties: BTreeMap::new(),
            embeddings: Vec::new(),
        })
        .map_err(|e| format!("recognition observation failed: {e}"))?;
    Ok(id)
}

/// The recognition provenance for a detection: which enrolled subject its
/// sighting matched, the similarity, the matched reference, and the enrolling
/// correction. Shared by every read surface (HTTP why JSON, CLI why) so they
/// never diverge. `resolved_detection_id` is the concrete observation id (after
/// any `--latest` resolution).
#[derive(Debug, Clone)]
pub struct RecognitionProvenance {
    pub name: String,
    pub score: f64,
    pub reference_label: String,
    pub enrolled_by_correction_id: Option<String>,
}

pub fn recognition_provenance(
    store: &Store,
    resolved_detection_id: &str,
) -> Option<RecognitionProvenance> {
    let uuid = uuid::Uuid::parse_str(resolved_detection_id).ok()?;
    let context_id = store
        .get_observation(ObservationId::from(uuid))
        .ok()?
        .context_id;
    let observations = store.list_observations(Some(context_id)).ok()?;
    let matched = observations.iter().find(|o| {
        o.observation_type == "recognition"
            && o.observed_properties.get("anchored_detection_id")
                == Some(&Value::String(resolved_detection_id.to_string()))
            && o.observed_properties.get("matched") == Some(&Value::Bool(true))
    })?;
    let name = matched
        .observed_properties
        .get("matched_name")
        .and_then(|v| v.as_str())?
        .to_string();
    let reference_label = matched
        .observed_properties
        .get("reference_label")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let enrolled_by_correction_id = if reference_label.is_empty() {
        observations
            .iter()
            .find(|c| {
                c.observation_type == "correction"
                    && c.observed_properties.get("correction_type")
                        == Some(&Value::String("Enroll".to_string()))
                    && c.observed_properties.get("label") == Some(&Value::String(name.clone()))
            })
            .map(|c| c.id.to_string())
    } else {
        observations
            .iter()
            .find(|c| {
                c.observation_type == "correction"
                    && c.observed_properties.get("correction_type")
                        == Some(&Value::String("Enroll".to_string()))
                    && c.observed_properties
                        .get("anchored_detection_id")
                        .or_else(|| c.properties.get("anchored_detection_id"))
                        == Some(&Value::String(reference_label.clone()))
            })
            .map(|c| c.id.to_string())
    };
    Some(RecognitionProvenance {
        name,
        score: matched
            .observed_properties
            .get("score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        reference_label,
        enrolled_by_correction_id,
    })
}

/// The receipt of an enrollment: the (possibly new) entity and its reference.
#[derive(Debug, Clone)]
pub struct EnrollmentReceipt {
    pub entity_id: String,
    pub entity_created: bool,
}

/// Find the "recognition" observation anchored to a detection — the sighting
/// whose stored probe vector becomes the enrolled reference.
fn anchored_recognition(
    store: &Store,
    detection_id: &str,
    context_id: context_graph::ContextId,
) -> Result<(Vec<f32>, String, String), String> {
    let observations = store
        .list_observations(Some(context_id))
        .map_err(|e| format!("list failed: {e}"))?;
    let recognition = observations
        .iter()
        .filter(|o| o.observation_type == "recognition")
        .find(|o| {
            o.observed_properties.get("anchored_detection_id")
                == Some(&Value::String(detection_id.to_string()))
        })
        .ok_or_else(|| {
            format!("no recognition observation anchored to detection {detection_id}")
        })?;
    let probe: Vec<f32> = recognition
        .observed_properties
        .get("probe_vector")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_f64())
                .map(|x| x as f32)
                .collect()
        })
        .ok_or_else(|| "recognition observation carries no probe vector".to_string())?;
    let class = recognition
        .observed_properties
        .get("class")
        .and_then(|v| v.as_str())
        .unwrap_or("person")
        .to_string();
    let space = recognition
        .observed_properties
        .get("embedding_space_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Ok((probe, class, space))
}

/// Enroll a detection's sighting as a named entity's reference. First
/// enrollment of a name creates the typed entity in the site context; further
/// enrollments add references to the same entity. The reference vector is the
/// sighting's own stored probe — never recomputed from annotated evidence.
pub fn record_enrollment(
    store: &Store,
    detection_id: &str,
    name: &str,
    embedding_space_id: &str,
) -> Result<EnrollmentReceipt, String> {
    let uuid = uuid::Uuid::parse_str(detection_id).map_err(|e| format!("invalid id: {e}"))?;
    let detection = store
        .get_observation(ObservationId::from(uuid))
        .map_err(|e| format!("detection not found: {e}"))?;
    let (probe, class, _space) = anchored_recognition(store, detection_id, detection.context_id)?;

    let entities = store
        .list_entities(context_graph::ListEntityFilter {
            context_id: Some(detection.context_id),
            ..Default::default()
        })
        .map_err(|e| format!("list entities failed: {e}"))?;
    let (entity_id, created) = match entities.iter().find(|e| e.name == name) {
        Some(existing) => (existing.id, false),
        None => {
            let created = store
                .create_entity(CreateEntity {
                    entity_type: entity_type_for_class(&class),
                    name: name.to_string(),
                    properties: BTreeMap::new(),
                    tags: vec!["enrolled".to_string()],
                    context_id: detection.context_id,
                })
                .map_err(|e| format!("create entity failed: {e}"))?;
            (created.id, true)
        }
    };
    store
        .enroll_entity_reference(
            entity_id,
            embedding_space_id,
            Some(detection_id.to_string()),
            probe,
            recognition_producer(),
        )
        .map_err(|e| format!("enroll failed: {e}"))?;
    Ok(EnrollmentReceipt {
        entity_id: entity_id.to_string(),
        entity_created: created,
    })
}

/// Enroll from the correction seam: the embedding space is recovered from the
/// sighting's own recognition observation, so the seam needs no recognition
/// config in scope.
pub fn record_enrollment_from_correction(
    store: &Store,
    detection_id: &str,
    name: &str,
) -> Result<EnrollmentReceipt, String> {
    let uuid = uuid::Uuid::parse_str(detection_id).map_err(|e| format!("invalid id: {e}"))?;
    let detection = store
        .get_observation(ObservationId::from(uuid))
        .map_err(|e| format!("detection not found: {e}"))?;
    let (_probe, _class, space) = anchored_recognition(store, detection_id, detection.context_id)?;
    if space.is_empty() {
        return Err("sighting carries no embedding space".to_string());
    }
    record_enrollment(store, detection_id, name, &space)
}

/// Forget a named entity: its enrolled reference vectors are removed (the
/// biometric data and the matching behavior are gone; the immutable
/// enrollment provenance rows remain in cg). Returns the number of references
/// removed.
pub fn forget_named_entity(
    store: &Store,
    name: &str,
    context_id: Option<context_graph::ContextId>,
) -> Result<usize, String> {
    let entities = store
        .list_entities(context_graph::ListEntityFilter {
            context_id,
            ..Default::default()
        })
        .map_err(|e| format!("list entities failed: {e}"))?;
    let entity = entities
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| format!("no entity named {name}"))?;
    store
        .remove_entity_references(entity.id)
        .map_err(|e| format!("reference removal failed: {e}"))
}

/// Open the vigil store with a vision embedder registered — the registration
/// declares the embedding space, so enroll and match are live from open.
/// Production passes the real vision embedder; tests inject a deterministic
/// double through this same seam.
pub fn open_store_with_embedder(
    path: &Path,
    embedding_space_id: &str,
    embedder: Arc<dyn Embedder>,
) -> Result<Store, String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("store parent: {e}"))?;
    }
    let space = embedder.embedding_space();
    let store = Store::open_with_embedder_registrations(
        StoreConfig {
            db_path: path.to_path_buf(),
            default_text_embedder: Some(EmbedderConfig::disabled()),
            ..StoreConfig::default()
        },
        vec![EmbedderRegistration {
            space: EmbeddingSpaceDecl {
                embedding_space_id: embedding_space_id.to_string(),
                dimension: space.dimension,
                metric: space.metric,
                model_family: space.model_family,
                model_name: space.model_name,
                model_version: space.model_version,
                supported_evidence_kinds: space.supported_evidence_kinds,
                default_for_evidence_kind: None,
                bindings: Vec::new(),
            },
            embedder,
        }],
    )
    .map_err(|e| format!("store open with vision embedder failed: {e}"))?;
    Ok(store)
}
