// Recognition slice: crop → embed → enroll → match → named entity.
//
// These tests drive the recognition library surface against a real
// context-graph store with a deterministic, content-faithful embedder double
// (hash-of-bytes → unit vector) injected through the SAME registration seam
// production uses for the real vision embedder. Controlled vectors make the
// threshold behavior deterministic; the real model's quality is the owner
// live smoke and the identity-depth measurement, never CI.

use std::collections::BTreeMap;
use std::sync::Arc;

use context_graph::{
    CreateContext, CreateDecision, CreateEntity, CreateIntention, Embedder, EmbeddingInput,
    EmbeddingOutput, EntityType, EvidenceKind, EvidenceProducer, EvidenceRef, IntentionOrigin,
    IntentionStatus, ObservationId, RecordObservation, RetentionStatus, Store,
};
use vigil::recognition::{
    MatchOutcome, RecognitionConfig, class_is_covered, crop_png, entity_type_for_class,
    forget_named_entity, map_bbox_to_frame, match_crop_for_class, match_vector,
    match_vector_for_class, open_store_with_embedder, record_enrollment, record_match_observation,
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

fn axis_vector(slot: usize) -> Vec<f32> {
    let mut vector = vec![0.0f32; DIM];
    vector[slot] = 1.0;
    vector
}

fn vector_with_cosine_to_axis(score: f32) -> Vec<f32> {
    let mut vector = vec![0.0f32; DIM];
    vector[0] = score;
    vector[1] = (1.0 - score * score).sqrt();
    vector
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
        &dir.path()
            .join("created/by/open-store")
            .join(format!("{name}.db")),
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

fn create_same_name_decoy_in_other_context(
    world: &World,
    name: &str,
) -> (context_graph::ContextId, context_graph::EntityId) {
    let context = world
        .store
        .create_context(CreateContext {
            name: format!("other-site-{name}"),
            labels: Vec::new(),
            properties: BTreeMap::new(),
        })
        .expect("other context");
    let entity = world
        .store
        .create_entity(CreateEntity {
            entity_type: EntityType::Person,
            name: name.to_string(),
            properties: BTreeMap::new(),
            tags: vec!["cross-context-decoy".to_string()],
            context_id: context.id,
        })
        .expect("same-name entity in other context");
    (context.id, entity.id)
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
            observed_at: chrono::DateTime::from_timestamp_millis(1_700_000_000_000)
                .expect("fixed recognition fixture timestamp is valid"),
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
            observed_properties: BTreeMap::from([("class".to_string(), serde_json::json!(class))]),
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
    let outcome = match_crop_for_class(
        &world.store,
        &embedder,
        SPACE,
        world.context_id,
        crop,
        0.6,
        &class,
    )
    .expect("match runs");
    let probe = hash_vector(crop);
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
    let rect =
        map_bbox_to_frame("64,128,320,384", 640, 640, 1920, 1080).expect("well-formed bbox maps");
    assert_eq!(
        rect,
        (192, 216, 768, 432),
        "independent x/y scale, no padding offset"
    );
}

#[test]
fn bbox_mapping_clamps_to_frame_and_rejects_degenerate_boxes() {
    let clamped = map_bbox_to_frame("600,600,700,700", 640, 640, 1000, 1000)
        .expect("overhanging bbox clamps");
    assert_eq!(
        clamped,
        (937, 937, 63, 63),
        "overhanging coordinates clamp to the exact remaining frame extent"
    );
    assert!(map_bbox_to_frame("10,10,10,10", 640, 640, 1000, 1000).is_none());
    assert!(map_bbox_to_frame("10,10,10,20", 640, 640, 1000, 1000).is_none());
    assert!(map_bbox_to_frame("10,10,20,10", 640, 640, 1000, 1000).is_none());
    for dimensions in [
        (0, 640, 1000, 1000),
        (640, 0, 1000, 1000),
        (640, 640, 0, 1000),
        (640, 640, 1000, 0),
    ] {
        assert!(
            map_bbox_to_frame(
                "10,10,20,20",
                dimensions.0,
                dimensions.1,
                dimensions.2,
                dimensions.3,
            )
            .is_none(),
            "every zero detector/frame dimension must reject: {dimensions:?}"
        );
    }
    assert_eq!(
        map_bbox_to_frame("0,0,640,640", 640, 640, 1920, 1080),
        Some((0, 0, 1920, 1080)),
        "full detector extent maps exactly to the full source frame"
    );
    assert!(
        map_bbox_to_frame("-20,10,-10,20", 640, 640, 1000, 1000).is_none(),
        "a box clamped to zero width must reject even when height remains positive"
    );
    assert!(
        map_bbox_to_frame("10,-20,20,-10", 640, 640, 1000, 1000).is_none(),
        "a box clamped to zero height must reject even when width remains positive"
    );
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
    let right_edge = crop_png(&rgb, w, h, (190, 0, 10, 1)).expect("right edge is inclusive");
    let bottom_edge = crop_png(&rgb, w, h, (0, 90, 1, 10)).expect("bottom edge is inclusive");
    let decode = |bytes: &[u8]| image::load_from_memory(bytes).expect("decode").to_rgb8();
    assert!(decode(&marker).pixels().all(|p| p.0 == [255, 255, 255]));
    assert!(decode(&elsewhere).pixels().all(|p| p.0 == [0, 0, 0]));
    assert_eq!(decode(&right_edge).dimensions(), (10, 1));
    assert_eq!(decode(&bottom_edge).dimensions(), (1, 10));
    assert!(crop_png(&rgb[..rgb.len() - 1], w, h, (0, 0, 1, 1)).is_err());
    for invalid in [(190, 0, 11, 1), (0, 90, 1, 11), (0, 0, 0, 1), (0, 0, 1, 0)] {
        assert!(
            crop_png(&rgb, w, h, invalid).is_err(),
            "invalid crop boundary must fail: {invalid:?}"
        );
    }
}

// ── Class routing ──────────────────────────────────────────────────────────

#[test]
fn covered_classes_and_entity_types_route_all_classes_one_mechanism() {
    let config = RecognitionConfig::default();
    for class in ["person", "dog", "car"] {
        assert!(
            class_is_covered(&config, class),
            "{class} covered by default"
        );
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
    let (_, decoy_id) = create_same_name_decoy_in_other_context(&world, "Arjun");

    // The owner marks the detection "Arjun" through the correction seam —
    // the same door the card and HTTP plane drive.
    let receipt = record_correction(
        &world.store,
        CorrectionRequest {
            detection_id: detection.to_string(),
            label: Some("Arjun".to_string()),
            correction_type: CorrectionType::Enroll,
        },
    )
    .expect("enroll correction succeeds");
    assert!(!receipt.correction_id.is_empty());

    let entities = world.store.list_entities(Default::default()).expect("list");
    let people = entities
        .iter()
        .filter(|entity| entity.name == "Arjun")
        .collect::<Vec<_>>();
    assert_eq!(
        people.len(),
        2,
        "enrollment must create a site-local Arjun instead of reusing the same name from another context"
    );
    let person = people
        .iter()
        .find(|entity| entity.context_id == world.context_id)
        .expect("first enroll creates the named entity in the detection context");
    assert_ne!(person.id, decoy_id);
    assert_eq!(person.entity_type, EntityType::Person);

    // The next sighting (same subject ⇒ same crop bytes ⇒ same vector through
    // the content-faithful double) matches on ANY camera in the site.
    let second_detection = seed_detection(&world, "person");
    let outcome = embed_match_record(&world, second_detection, &crop);
    assert_eq!(outcome.name.as_deref(), Some("Arjun"));
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
        label: Some("Arjun".to_string()),
        correction_type: "enroll".to_string(),
    })
    .expect("enroll parses");
    assert_eq!(request.correction_type, CorrectionType::Enroll);
    assert_eq!(request.label.as_deref(), Some("Arjun"));
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
    record_enrollment(&world.store, &detection.to_string(), "Arjun", SPACE).expect("enroll");

    // A different subject: different bytes, near-orthogonal vector.
    let visitor_detection = seed_detection(&world, "person");
    let outcome = embed_match_record(&world, visitor_detection, &png(200));
    assert!(
        outcome.name.is_none(),
        "an un-enrolled visitor stays unknown, got {outcome:?}"
    );
    assert!(outcome.score < 0.6);
}

#[test]
fn configured_recognition_threshold_is_honored_without_a_hidden_floor() {
    let world = world("visible-recognition-threshold");
    let person_detection = seed_detection(&world, "person");
    record_match_observation(
        &world.store,
        world.camera_id,
        world.context_id,
        &person_detection.to_string(),
        &MatchOutcome {
            entity_id: None,
            name: None,
            reference_label: None,
            score: 0.0,
            embedding_space_id: SPACE.to_string(),
        },
        &axis_vector(0),
        "person",
        &format!("vigil-edge:clip/{person_detection}-frame.png"),
    )
    .expect("reference sighting records");
    record_enrollment(&world.store, &person_detection.to_string(), "Kiran", SPACE)
        .expect("enroll the person");

    // Regression seed: a different person/rider matched the only enrolled
    // person at 0.8489186167. The 0.90 default rejects it, while an operator
    // who explicitly selects 0.60 gets the configured behavior—not a hidden
    // 0.90 floor.
    let smoke_false_positive_score = 0.848_918_6f32;
    let probe = vector_with_cosine_to_axis(smoke_false_positive_score);
    let default_outcome = match_vector_for_class(
        &world.store,
        SPACE,
        world.context_id,
        &probe,
        RecognitionConfig::default().match_threshold,
        "person",
    )
    .expect("default-threshold match runs");
    let configured_outcome = match_vector_for_class(
        &world.store,
        SPACE,
        world.context_id,
        &probe,
        0.60,
        "person",
    )
    .expect("configured-threshold match runs");
    let exact_boundary = match_vector(&world.store, SPACE, world.context_id, &axis_vector(0), 1.0)
        .expect("exact-threshold match runs");

    assert!(
        default_outcome.name.is_none(),
        "the 0.90 default must keep the observed smoke false-positive unknown, got {default_outcome:?}"
    );
    assert_eq!(
        configured_outcome.name.as_deref(),
        Some("Kiran"),
        "an explicit 0.60 threshold must be honored without a hidden 0.90 floor"
    );
    assert_eq!(
        exact_boundary.name.as_deref(),
        Some("Kiran"),
        "a score equal to the configured threshold is a match"
    );
    assert!(
        (default_outcome.score - smoke_false_positive_score as f64).abs() < 1e-5,
        "the unknown outcome must preserve the nearest-neighbor score for review provenance, got {}",
        default_outcome.score
    );
}

// ── Match memory + provenance (AC4) ────────────────────────────────────────

#[test]
fn match_records_observation_against_the_matched_entity_with_vector_and_score() {
    let world = world("provenance");
    let crop = png(3);
    let detection = seed_detection(&world, "person");
    embed_match_record(&world, detection, &crop);
    record_enrollment(&world.store, &detection.to_string(), "Arjun", SPACE).expect("enroll");

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
    let person = entities.iter().find(|e| e.name == "Arjun").expect("entity");
    assert_eq!(
        match_obs.entity_id, person.id,
        "the match observation is recorded AGAINST the matched entity"
    );
    assert!(match_obs.observed_properties.contains_key("score"));
    let producer = &match_obs.evidence[0].producer;
    assert_eq!(producer.system, "vigil");
    assert_eq!(producer.model_name, "vision-embedder");
    assert_eq!(producer.model_version, "1");
    assert_eq!(producer.pipeline_version, "recognition-v1");
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
    let max = entities
        .iter()
        .find(|e| e.name == "Max")
        .expect("Max exists");
    assert_eq!(max.entity_type, EntityType::Animal);

    let sighting = seed_detection(&world, "dog");
    let outcome = embed_match_record(&world, sighting, &crop);
    assert_eq!(outcome.name.as_deref(), Some("Max"));
}

#[test]
fn vehicle_sighting_cannot_match_a_person_identity_even_with_identical_vector() {
    let world = world("type-guard");
    let crop = png(41);
    let person_detection = seed_detection(&world, "person");
    embed_match_record(&world, person_detection, &crop);
    record_correction(
        &world.store,
        CorrectionRequest {
            detection_id: person_detection.to_string(),
            label: Some("Kiran".to_string()),
            correction_type: CorrectionType::Enroll,
        },
    )
    .expect("enroll the person");

    let vehicle_detection = seed_detection(&world, "bicycle");
    let outcome = embed_match_record(&world, vehicle_detection, &crop);

    assert!(
        outcome.name.is_none(),
        "a bicycle crop must not resolve to a Person entity named after the enrolled person, even if its vector is identical; got {outcome:?}"
    );
    assert!(
        outcome.score > 0.9,
        "the guard must reject the high-scoring wrong entity type, not rely on score tuning"
    );
    let view = vigil::review_events(&world.store, 50).expect("review_events");
    let row = view
        .rows
        .iter()
        .find(|row| row.observation_id == vehicle_detection.to_string())
        .expect("vehicle sighting row exists");
    assert!(
        row.entity_name.is_none(),
        "the HA event row must not publish the enrolled person's name for a vehicle detection"
    );
}

// ── Deletion (AC6) ─────────────────────────────────────────────────────────

#[test]
fn forget_removes_references_and_the_subject_reverts_to_unknown() {
    let world = world("forget");
    let crop = png(11);
    let detection = seed_detection(&world, "person");
    embed_match_record(&world, detection, &crop);
    record_enrollment(&world.store, &detection.to_string(), "Arjun", SPACE).expect("enroll");

    let (other_context, other_entity) = create_same_name_decoy_in_other_context(&world, "Arjun");
    world
        .store
        .enroll_entity_reference(
            other_entity,
            SPACE,
            Some("other-context-reference".to_string()),
            hash_vector(&crop),
            producer(),
        )
        .expect("other context reference enrolls");

    let removed_other = forget_named_entity(&world.store, "Arjun", Some(other_context))
        .expect("context-scoped forget runs");
    assert!(
        removed_other >= 1,
        "the selected context loses its reference"
    );
    let still_known = embed_match_record(&world, seed_detection(&world, "person"), &crop);
    assert_eq!(
        still_known.name.as_deref(),
        Some("Arjun"),
        "forgetting a same-name subject in another context must not erase this site's subject"
    );

    let removed =
        forget_named_entity(&world.store, "Arjun", Some(world.context_id)).expect("forget runs");
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
        entity_name: Some("Arjun".to_string()),
        match_score: Some(0.93),
    });
    assert_eq!(payload.entity_name.as_deref(), Some("Arjun"));
    let json = serde_json::to_value(&payload).expect("serialize");
    assert_eq!(json["entity_name"], serde_json::json!("Arjun"));
}

#[test]
fn review_events_row_carries_the_entity_name() {
    let world = world("event-row");
    let crop = png(13);
    let detection = seed_detection(&world, "person");
    embed_match_record(&world, detection, &crop);
    record_enrollment(&world.store, &detection.to_string(), "Arjun", SPACE).expect("enroll");
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
        Some("Arjun"),
        "the HTTP event row is the durable authority the card reloads against"
    );
}

#[test]
fn review_events_enroll_row_carries_server_authoritative_name() {
    let world = world("enroll-event-row");
    let crop = png(29);
    let detection = seed_detection(&world, "person");
    embed_match_record(&world, detection, &crop);
    record_correction(
        &world.store,
        CorrectionRequest {
            detection_id: detection.to_string(),
            label: Some("Arjun".to_string()),
            correction_type: CorrectionType::Enroll,
        },
    )
    .expect("enroll correction succeeds");

    let view = vigil::review_events(&world.store, 50).expect("review_events");
    let row = view
        .rows
        .iter()
        .find(|r| r.observation_id == detection.to_string())
        .expect("the enrolled source detection row exists");

    assert_eq!(
        row.current_correction,
        Some(CorrectionType::Enroll),
        "the anchored source detection must read back the latest correction as Enroll"
    );
    assert!(
        row.confirmed,
        "Enroll is the owner's positive confirmation that this detection is the named subject"
    );
    assert!(
        !row.correction_recorded,
        "Enroll must not be treated as a negative correction or false-alarm-style edit"
    );
    assert_eq!(
        row.entity_name.as_deref(),
        Some("Arjun"),
        "the HA card must reload the server-authoritative enrolled name for the source detection, not a local-only saved state"
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
            label: Some("Arjun".to_string()),
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
    assert_eq!(recognition.name, "Arjun");
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

#[test]
fn recognition_provenance_joins_enroll_correction_by_matched_reference_label() {
    let world = world("why-reference-label");
    let first_crop = png(31);
    let second_crop = png(32);

    let first_detection = seed_detection(&world, "person");
    let first_detection_id = first_detection.to_string();
    embed_match_record(&world, first_detection, &first_crop);
    let first_receipt = record_correction(
        &world.store,
        CorrectionRequest {
            detection_id: first_detection_id,
            label: Some("Arjun".to_string()),
            correction_type: CorrectionType::Enroll,
        },
    )
    .expect("first Arjun enrollment succeeds");

    let second_detection = seed_detection(&world, "person");
    let second_detection_id = second_detection.to_string();
    embed_match_record(&world, second_detection, &second_crop);
    let second_receipt = record_correction(
        &world.store,
        CorrectionRequest {
            detection_id: second_detection_id.clone(),
            label: Some("Arjun".to_string()),
            correction_type: CorrectionType::Enroll,
        },
    )
    .expect("second Arjun reference enrollment succeeds");
    assert_ne!(
        first_receipt.correction_id, second_receipt.correction_id,
        "two distinct source detections must keep distinct Enroll correction records"
    );

    let later_sighting = seed_detection(&world, "person");
    let outcome = embed_match_record(&world, later_sighting, &second_crop);
    assert_eq!(outcome.name.as_deref(), Some("Arjun"));
    assert_eq!(
        outcome.reference_label.as_deref(),
        Some(second_detection_id.as_str()),
        "the exact second enrolled reference must win for the second crop"
    );

    let direct =
        vigil::recognition::recognition_provenance(&world.store, &later_sighting.to_string())
            .expect("matched sighting carries direct recognition provenance");
    assert_eq!(direct.name, "Arjun");
    assert_eq!(
        direct.reference_label, second_detection_id,
        "recognition provenance must preserve the matched reference label"
    );
    assert_eq!(
        direct.enrolled_by_correction_id.as_deref(),
        Some(second_receipt.correction_id.as_str()),
        "recognition provenance must join to the Enroll correction anchored to the matched reference label, not the first same-name correction"
    );

    let why = vigil::review_why(&world.store, &later_sighting.to_string()).expect("review_why");
    let recognition = why
        .recognition
        .expect("why view carries recognition provenance");
    assert_eq!(recognition.name, "Arjun");
    assert_eq!(
        recognition.reference_label, second_detection_id,
        "review_why must report the matched enrolled reference"
    );
    assert_eq!(
        recognition.enrolled_by_correction_id.as_deref(),
        Some(second_receipt.correction_id.as_str()),
        "review_why must name the Enroll correction for the matched reference label"
    );
}

// ── External-review findings (2026-07-04): surface + atomicity guards ──────

#[test]
fn enroll_failure_writes_no_correction_row() {
    // Enrollment must be atomic with its correction row: a detection with NO
    // recognition observation (no stored probe vector) cannot enroll, and the
    // failed attempt must not strand an Enroll correction row — a stranded row
    // dedups later retries into silent no-ops.
    let world = world("enroll-atomic");
    let detection = seed_detection(&world, "person");
    let result = record_correction(
        &world.store,
        CorrectionRequest {
            detection_id: detection.to_string(),
            label: Some("Arjun".to_string()),
            correction_type: CorrectionType::Enroll,
        },
    );
    assert!(
        result.is_err(),
        "enroll without a sighting vector must fail"
    );
    let stranded = world
        .store
        .list_observations(None)
        .expect("list")
        .into_iter()
        .filter(|o| o.observation_type == "correction")
        .count();
    assert_eq!(
        stranded, 0,
        "a failed enrollment must not persist a correction row"
    );
}

#[test]
fn http_event_rows_and_why_carry_recognition_for_the_card() {
    // The card reads the HTTP JSON, not vigil's internal structs: the event
    // row must carry entity_name and the why payload the match provenance.
    let world = world("http-surface");
    let crop = png(23);
    let detection = seed_detection(&world, "person");
    embed_match_record(&world, detection, &crop);
    record_correction(
        &world.store,
        CorrectionRequest {
            detection_id: detection.to_string(),
            label: Some("Arjun".to_string()),
            correction_type: CorrectionType::Enroll,
        },
    )
    .expect("enroll");
    let sighting = seed_detection(&world, "person");
    embed_match_record(&world, sighting, &crop);

    let plane =
        vigil::spawn_review_data_plane(world.store.clone(), world._dir.path().to_path_buf(), 0)
            .expect("data plane");
    let port = plane.local_addr().port();
    let fetch = |path: &str| -> serde_json::Value {
        let body = std::process::Command::new("curl")
            .arg("-s")
            .arg(format!("http://127.0.0.1:{port}{path}"))
            .output()
            .expect("curl runs");
        serde_json::from_slice(&body.stdout).expect("json")
    };

    let events = fetch("/events");
    let row = events
        .as_array()
        .expect("events is a json array")
        .iter()
        .find(|r| r["observation_id"] == serde_json::json!(sighting.to_string()))
        .expect("sighting row")
        .clone();
    assert_eq!(
        row["entity_name"],
        serde_json::json!("Arjun"),
        "the HTTP event row the card renders must carry the name, got {row}"
    );

    let why = fetch(&format!("/why/{sighting}"));
    let recognition = &why["recognition"];
    assert_eq!(recognition["name"], serde_json::json!("Arjun"));
    assert!(
        recognition["score"].as_f64().unwrap_or(0.0) > 0.9,
        "why JSON carries the match score, got {why}"
    );
    assert!(
        recognition["enrolled_by_correction_id"].is_string(),
        "why JSON names the enrolling correction, got {why}"
    );
}

#[test]
fn covered_class_indices_map_names_to_coco_indices_including_person() {
    // The detector emits COCO class indices; recognition targets a set of
    // names. The mapping must be exact so a "dog" detection actually reaches
    // the animal-recognition path — and person is always covered for the
    // baseline NVR promise.
    use vigil::recognition::{coco_class_index, covered_class_indices};
    assert_eq!(coco_class_index("person"), Some(0));
    assert_eq!(coco_class_index("car"), Some(2));
    assert_eq!(coco_class_index("dog"), Some(16));
    assert_eq!(coco_class_index("cow"), Some(19));
    assert_eq!(coco_class_index("truck"), Some(7));
    assert_eq!(coco_class_index("kite"), Some(33));
    assert_eq!(coco_class_index("not-a-class"), None);

    let config = RecognitionConfig::default();
    let indices = covered_class_indices(&config);
    assert!(indices.contains(&0), "person is always covered");
    assert!(indices.contains(&16), "dog maps into the covered set");
    assert!(indices.contains(&2), "car maps into the covered set");
}
