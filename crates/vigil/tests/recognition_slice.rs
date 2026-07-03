// Recognition slice: crop → embed → enroll → match → named entity.
//
// These tests drive the recognition library surface against a real
// context-graph store with a deterministic, content-faithful embedder double
// (hash-of-bytes → unit vector) injected through the SAME registration seam
// production uses for the real vision embedder. Controlled vectors make the
// threshold behavior deterministic; the real model's quality is the owner
// live smoke and the identity-depth measurement, never CI.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use context_graph::{
    CreateContext, CreateDecision, CreateEntity, CreateIntention, Embedder, EmbeddingInput,
    EmbeddingOutput, EntityType, EvidenceKind, EvidenceProducer, EvidenceRef, IntentionOrigin,
    IntentionStatus, ObservationId, RecordObservation, RetentionStatus, Store,
};
use vigil::recognition::{
    MatchOutcome, RecognitionConfig, class_is_covered, crop_png, entity_type_for_class,
    forget_named_entity, map_bbox_to_frame, match_crop, open_store_with_embedder,
    record_enrollment, record_match_observation,
};
use vigil::{CorrectionRequest, CorrectionType, parse_command_topic, record_correction};

const SPACE: &str = "vigil_site_vision_test";
const DIM: usize = 768;

/// Content-faithful deterministic double: the vector derives from the actual
/// image bytes (identical bytes → identical vector; different bytes →
/// effectively orthogonal vectors), L2-normalized. Never a rigged shape.
struct HashEmbedder;

fn hash_vector(bytes: &[u8]) -> Vec<f32> {
    let mut vector = vec![0.0f32; DIM];
    let mut state: u64 = 0xcbf2_9ce4_8422_2325;
    for (i, b) in bytes.iter().enumerate() {
        state = state.wrapping_mul(0x100_0000_01b3).wrapping_add(*b as u64);
        let slot = (state as usize) % DIM;
        vector[slot] += ((state >> 32) as f32 / u32::MAX as f32) - 0.5;
        let _ = i;
    }
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    vector.iter_mut().for_each(|x| *x /= norm);
    vector
}

impl Embedder for HashEmbedder {
    fn embed(&self, input: EmbeddingInput) -> context_graph::Result<EmbeddingOutput> {
        let bytes = match input {
            EmbeddingInput::ImageBytes(bytes) => bytes,
            EmbeddingInput::ImageFrame { uri } => std::fs::read(&uri).unwrap_or_default(),
            other => {
                return Err(context_graph::CgError::Engine(format!(
                    "test embedder handles images only, got {other:?}"
                )));
            }
        };
        Ok(EmbeddingOutput {
            embedding_space_id: SPACE.to_string(),
            dimension: DIM,
            producer: producer(),
            vector: hash_vector(&bytes),
        })
    }
    fn supported_inputs(&self) -> Vec<EvidenceKind> {
        vec![EvidenceKind::ImageFrame]
    }
    fn embedding_space(&self) -> context_graph::EmbeddingSpace {
        context_graph::EmbeddingSpace {
            embedding_space_id: SPACE.to_string(),
            dimension: DIM,
            metric: context_graph::DistanceMetric::Cosine,
            model_family: "vision".to_string(),
            model_name: "hash-double".to_string(),
            model_version: "1".to_string(),
            supported_evidence_kinds: vec![EvidenceKind::ImageFrame],
            default_for_evidence_kind: None,
            bindings: Vec::new(),
        }
    }
}

fn producer() -> EvidenceProducer {
    EvidenceProducer {
        system: "vigil-test".to_string(),
        model_name: "hash-double".to_string(),
        model_version: "1".to_string(),
        pipeline_version: "1".to_string(),
    }
}

/// A deterministic synthetic PNG, distinct per seed.
fn png(seed: u8) -> Vec<u8> {
    let img = image::RgbImage::from_fn(48, 48, |x, y| {
        image::Rgb([
            seed.wrapping_mul(31).wrapping_add(x as u8),
            seed ^ (y as u8),
            (x as u8).wrapping_add(y as u8).wrapping_mul(seed | 1),
        ])
    });
    let mut bytes = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .expect("encode png");
    bytes
}

struct World {
    store: Store,
    context_id: context_graph::ContextId,
    camera_id: context_graph::EntityId,
    _dir: tempfile::TempDir,
}

/// A minimal five-node site mirroring the runtime's memory shape: context →
/// intention → detector-config decision → camera entity, ready for detection
/// observations.
fn world(name: &str) -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store_with_embedder(
        &dir.path().join(format!("{name}.db")),
        SPACE,
        Arc::new(HashEmbedder),
    )
    .expect("store opens with the test embedder registered");
    let context = store
        .create_context(CreateContext {
            name: format!("{name}-site"),
            labels: Vec::new(),
            properties: BTreeMap::new(),
        })
        .expect("context");
    let camera = store
        .create_entity(CreateEntity {
            entity_type: EntityType::Device,
            name: format!("{name}-camera"),
            properties: BTreeMap::new(),
            tags: Vec::new(),
            context_id: context.id,
        })
        .expect("camera entity");
    let intention = store
        .create_intention(CreateIntention {
            id: None,
            description: "baseline watch".to_string(),
            status: IntentionStatus::Active,
            origin: IntentionOrigin::Human,
            context_id: context.id,
            properties: BTreeMap::new(),
            tags: Vec::new(),
            blueprint_catalog_id: None,
        })
        .expect("intention");
    store
        .create_decision(CreateDecision {
            decision_type: "detector-config".to_string(),
            description: "active detector config".to_string(),
            confidence: None,
            intention_ids: vec![intention.id],
            based_on_entity_ids: vec![camera.id],
            context_id: context.id,
            ..Default::default()
        })
        .expect("decision");
    World {
        store,
        context_id: context.id,
        camera_id: camera.id,
        _dir: dir,
    }
}

fn seed_detection(world: &World, class: &str) -> ObservationId {
    let id = ObservationId::new_v7();
    let evidence_id = context_graph::EvidenceId::new_v7();
    world
        .store
        .record_observation(RecordObservation {
            id,
            entity_id: world.camera_id,
            context_id: world.context_id,
            observation_type: "detection".to_string(),
            source: "vigil".to_string(),
            observed_at: chrono::Utc::now(),
            evidence: vec![EvidenceRef {
                id: evidence_id,
                observation_id: id,
                context_id: world.context_id,
                kind: EvidenceKind::ImageFrame,
                source_ref: format!("vigil-edge:clip/{id}-frame.png"),
                mime_type: Some("image/png".to_string()),
                producer: producer(),
                frame_index: Some(0),
                retention_status: RetentionStatus::RetainedExternal,
                ..Default::default()
            }],
            observed_properties: BTreeMap::from([(
                "class".to_string(),
                serde_json::json!(class),
            )]),
            state_delta: BTreeMap::new(),
            properties: BTreeMap::new(),
            embeddings: Vec::new(),
        })
        .expect("detection observation");
    id
}

/// Full pipeline step the runtime performs per covered detection: embed the
/// crop, match, record the match observation anchored to the detection.
fn embed_match_record(world: &World, detection: ObservationId, crop: &[u8]) -> MatchOutcome {
    let embedder = HashEmbedder;
    let outcome = match_crop(
        &world.store,
        &embedder,
        SPACE,
        world.context_id,
        crop,
        0.6,
    )
    .expect("match runs");
    let probe = hash_vector(crop);
    // The sighting's class comes from the seeded detection, exactly as the
    // runtime passes the detector's class — never assumed.
    let detection_row = world
        .store
        .get_observation(detection)
        .expect("detection read");
    let class = detection_row
        .observed_properties
        .get("class")
        .and_then(|value| value.as_str())
        .expect("seeded detection carries its class")
        .to_string();
    record_match_observation(
        &world.store,
        world.camera_id,
        world.context_id,
        &detection.to_string(),
        &outcome,
        &probe,
        &class,
        &format!("vigil-edge:clip/{detection}-frame.png"),
    )
    .expect("match observation records");
    outcome
}

// ── Geometry ───────────────────────────────────────────────────────────────

#[test]
fn bbox_maps_from_detector_space_to_original_frame_pixels() {
    // Detector space is a plain 640×640 squash of the full frame: mapping back
    // is independent x/y scaling. A box at (64,128)-(320,384) in detector
    // space on a 1920×1080 frame lands at (192,216)-(960,648).
    let rect = map_bbox_to_frame("64,128,320,384", 640, 640, 1920, 1080)
        .expect("well-formed bbox maps");
    assert_eq!(rect, (192, 216, 768, 432), "independent x/y scale, no padding offset");
}

#[test]
fn bbox_mapping_clamps_to_frame_and_rejects_degenerate_boxes() {
    let clamped = map_bbox_to_frame("600,600,700,700", 640, 640, 1000, 1000)
        .expect("overhanging bbox clamps");
    assert!(
        clamped.0 + clamped.2 <= 1000 && clamped.1 + clamped.3 <= 1000,
        "crop rect stays inside the frame, got {clamped:?}"
    );
    assert!(map_bbox_to_frame("10,10,10,10", 640, 640, 1000, 1000).is_none());
    assert!(map_bbox_to_frame("not-a-bbox", 640, 640, 1000, 1000).is_none());
}

#[test]
fn crop_png_extracts_the_requested_region() {
    // A white marker rectangle on a black frame: the crop of the marker region
    // is all-white; a crop elsewhere is all-black.
    let (w, h) = (200u32, 100u32);
    let mut rgb = vec![0u8; (w * h * 3) as usize];
    for y in 20..40u32 {
        for x in 50..90u32 {
            let i = ((y * w + x) * 3) as usize;
            rgb[i..i + 3].copy_from_slice(&[255, 255, 255]);
        }
    }
    let marker = crop_png(&rgb, w, h, (50, 20, 40, 20)).expect("crop marker");
    let elsewhere = crop_png(&rgb, w, h, (120, 60, 40, 20)).expect("crop elsewhere");
    let decode = |bytes: &[u8]| image::load_from_memory(bytes).expect("decode").to_rgb8();
    assert!(decode(&marker).pixels().all(|p| p.0 == [255, 255, 255]));
    assert!(decode(&elsewhere).pixels().all(|p| p.0 == [0, 0, 0]));
}

// ── Class routing ──────────────────────────────────────────────────────────

#[test]
fn covered_classes_and_entity_types_route_all_classes_one_mechanism() {
    let config = RecognitionConfig::default();
    for class in ["person", "dog", "car"] {
        assert!(class_is_covered(&config, class), "{class} covered by default");
    }
    assert!(!class_is_covered(&config, "kite"));
    assert_eq!(entity_type_for_class("person"), EntityType::Person);
    assert_eq!(entity_type_for_class("dog"), EntityType::Animal);
    assert_eq!(entity_type_for_class("cow"), EntityType::Animal);
    assert_eq!(entity_type_for_class("truck"), EntityType::Vehicle);
}

// ── Enroll → match round trip (AC1, AC2) ──────────────────────────────────

#[test]
fn enroll_correction_creates_named_entity_and_later_sightings_match() {
    let world = world("enroll-match");
    let crop = png(7);
    let detection = seed_detection(&world, "person");
    let first = embed_match_record(&world, detection, &crop);
    assert!(
        first.name.is_none(),
        "before enrollment the sighting is unknown, got {first:?}"
    );

    // The owner marks the detection "Roshan" through the correction seam —
    // the same door the card and HTTP plane drive.
    let receipt = record_correction(
        &world.store,
        CorrectionRequest {
            detection_id: detection.to_string(),
            label: Some("Roshan".to_string()),
            correction_type: CorrectionType::Enroll,
        },
    )
    .expect("enroll correction succeeds");
    assert!(!receipt.correction_id.is_empty());

    let entities = world.store.list_entities(Default::default()).expect("list");
    let person = entities
        .iter()
        .find(|e| e.name == "Roshan")
        .expect("first enroll creates the named entity");
    assert_eq!(person.entity_type, EntityType::Person);

    // The next sighting (same subject ⇒ same crop bytes ⇒ same vector through
    // the content-faithful double) matches on ANY camera in the site.
    let second_detection = seed_detection(&world, "person");
    let outcome = embed_match_record(&world, second_detection, &crop);
    assert_eq!(outcome.name.as_deref(), Some("Roshan"));
    assert!(
        outcome.score > 0.9,
        "same-subject probe scores high, got {}",
        outcome.score
    );
}

#[test]
fn enroll_wire_shape_parses_from_the_command_topic() {
    let request = parse_command_topic(&vigil::CommandTopicMessage {
        detection_id: "det-1".to_string(),
        label: Some("Roshan".to_string()),
        correction_type: "enroll".to_string(),
    })
    .expect("enroll parses");
    assert_eq!(request.correction_type, CorrectionType::Enroll);
    assert_eq!(request.label.as_deref(), Some("Roshan"));
}

#[test]
fn enroll_without_a_name_is_refused() {
    let world = world("enroll-no-name");
    let detection = seed_detection(&world, "person");
    embed_match_record(&world, detection, &png(9));
    let result = record_correction(
        &world.store,
        CorrectionRequest {
            detection_id: detection.to_string(),
            label: None,
            correction_type: CorrectionType::Enroll,
        },
    );
    assert!(result.is_err(), "enroll without a name must be refused");
}

// ── Unknown honesty (AC3) ──────────────────────────────────────────────────

#[test]
fn below_threshold_sighting_stays_unknown() {
    let world = world("unknown");
    let detection = seed_detection(&world, "person");
    embed_match_record(&world, detection, &png(1));
    record_enrollment(
        &world.store,
        &detection.to_string(),
        "Roshan",
        SPACE,
    )
    .expect("enroll");

    // A different subject: different bytes, near-orthogonal vector.
    let visitor_detection = seed_detection(&world, "person");
    let outcome = embed_match_record(&world, visitor_detection, &png(200));
    assert!(
        outcome.name.is_none(),
        "an un-enrolled visitor stays unknown, got {outcome:?}"
    );
    assert!(outcome.score < 0.6);
}

// ── Match memory + provenance (AC4) ────────────────────────────────────────

#[test]
fn match_records_observation_against_the_matched_entity_with_vector_and_score() {
    let world = world("provenance");
    let crop = png(3);
    let detection = seed_detection(&world, "person");
    embed_match_record(&world, detection, &crop);
    record_enrollment(&world.store, &detection.to_string(), "Roshan", SPACE).expect("enroll");

    let sighting = seed_detection(&world, "person");
    embed_match_record(&world, sighting, &crop);

    let observations = world.store.list_observations(None).expect("list");
    let match_obs = observations
        .iter()
        .find(|o| {
            o.observation_type == "recognition"
                && o.observed_properties.get("anchored_detection_id")
                    == Some(&serde_json::json!(sighting.to_string()))
                && o.observed_properties.get("matched") == Some(&serde_json::json!(true))
        })
        .expect("a match observation anchored to the sighting exists");
    let entities = world.store.list_entities(Default::default()).expect("list");
    let person = entities.iter().find(|e| e.name == "Roshan").expect("entity");
    assert_eq!(
        match_obs.entity_id, person.id,
        "the match observation is recorded AGAINST the matched entity"
    );
    assert!(match_obs.observed_properties.get("score").is_some());
    assert!(
        match_obs
            .observed_properties
            .get("probe_vector")
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.len() == DIM),
        "the sighting carries its precomputed vector in the properties lane"
    );
}

// ── All classes, one mechanism (AC5) ───────────────────────────────────────

#[test]
fn animal_class_enrolls_and_matches_as_an_animal_entity() {
    let world = world("animal");
    let crop = png(5);
    let detection = seed_detection(&world, "dog");
    embed_match_record(&world, detection, &crop);
    record_correction(
        &world.store,
        CorrectionRequest {
            detection_id: detection.to_string(),
            label: Some("Max".to_string()),
            correction_type: CorrectionType::Enroll,
        },
    )
    .expect("enroll Max");

    let entities = world.store.list_entities(Default::default()).expect("list");
    let max = entities.iter().find(|e| e.name == "Max").expect("Max exists");
    assert_eq!(max.entity_type, EntityType::Animal);

    let sighting = seed_detection(&world, "dog");
    let outcome = embed_match_record(&world, sighting, &crop);
    assert_eq!(outcome.name.as_deref(), Some("Max"));
}

// ── Deletion (AC6) ─────────────────────────────────────────────────────────

#[test]
fn forget_removes_references_and_the_subject_reverts_to_unknown() {
    let world = world("forget");
    let crop = png(11);
    let detection = seed_detection(&world, "person");
    embed_match_record(&world, detection, &crop);
    record_enrollment(&world.store, &detection.to_string(), "Roshan", SPACE).expect("enroll");

    let removed = forget_named_entity(&world.store, "Roshan", Some(world.context_id))
        .expect("forget runs");
    assert!(removed >= 1, "at least one reference removed");

    let sighting = seed_detection(&world, "person");
    let outcome = embed_match_record(&world, sighting, &crop);
    assert!(
        outcome.name.is_none(),
        "a forgotten subject must revert to unknown, got {outcome:?}"
    );
}

// ── Surfaces (AC2, AC4) ────────────────────────────────────────────────────

#[test]
fn event_payload_carries_the_entity_name_when_matched() {
    let payload = vigil::map_detection_to_event_payload(&vigil::DetectionInput {
        observation_id: "obs-1".to_string(),
        camera_name: "gate".to_string(),
        object_class: "person".to_string(),
        confidence: 0.9,
        timestamp_ms: 1,
        evidence_ref: "vigil-edge:clip/x".to_string(),
        snapshot_ref: "vigil-edge:clip/y".to_string(),
        zone: None,
        entity_name: Some("Roshan".to_string()),
        match_score: Some(0.93),
    });
    assert_eq!(payload.entity_name.as_deref(), Some("Roshan"));
    let json = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(json["entity_name"], serde_json::json!("Roshan"));
}

#[test]
fn review_events_row_carries_the_entity_name() {
    let world = world("event-row");
    let crop = png(13);
    let detection = seed_detection(&world, "person");
    embed_match_record(&world, detection, &crop);
    record_enrollment(&world.store, &detection.to_string(), "Roshan", SPACE).expect("enroll");
    let sighting = seed_detection(&world, "person");
    embed_match_record(&world, sighting, &crop);

    let view = vigil::review_events(&world.store, 50).expect("review_events");
    let row = view
        .rows
        .iter()
        .find(|r| r.observation_id == sighting.to_string())
        .expect("the sighting row exists");
    assert_eq!(
        row.entity_name.as_deref(),
        Some("Roshan"),
        "the HTTP event row is the durable authority the card reloads against"
    );
}

#[test]
fn why_view_shows_match_provenance_reference_score_and_enrolling_correction() {
    let world = world("why");
    let crop = png(17);
    let detection = seed_detection(&world, "person");
    embed_match_record(&world, detection, &crop);
    record_correction(
        &world.store,
        CorrectionRequest {
            detection_id: detection.to_string(),
            label: Some("Roshan".to_string()),
            correction_type: CorrectionType::Enroll,
        },
    )
    .expect("enroll");
    let sighting = seed_detection(&world, "person");
    embed_match_record(&world, sighting, &crop);

    let why = vigil::review_why(&world.store, &sighting.to_string()).expect("why");
    let recognition = why
        .recognition
        .expect("a matched sighting's why view carries recognition provenance");
    assert_eq!(recognition.name, "Roshan");
    assert!(recognition.score > 0.9);
    assert!(
        !recognition.reference_label.is_empty(),
        "which enrolled reference matched"
    );
    assert!(
        recognition.enrolled_by_correction_id.is_some(),
        "the enrolling correction is named"
    );
}
