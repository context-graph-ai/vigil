// Recognition-adjacent wire-shape tests that exercise the Home Assistant
// adapter's own types. Split out of `recognition_slice.rs` (core) because
// these two tests are the only ones in that suite that touched adapter
// types; the rest of that suite stays store/recognition-only and belongs in
// core.

use vigil::CorrectionType;
use vigil_ha::{
    CommandTopicMessage, DetectionInput, map_detection_to_event_payload, parse_command_topic,
};

#[test]
fn enroll_wire_shape_parses_from_the_command_topic() {
    let request = parse_command_topic(&CommandTopicMessage {
        detection_id: "det-1".to_string(),
        label: Some("Arjun".to_string()),
        correction_type: "enroll".to_string(),
    })
    .expect("enroll parses");
    assert_eq!(request.correction_type, CorrectionType::Enroll);
    assert_eq!(request.label.as_deref(), Some("Arjun"));
}

#[test]
fn event_payload_carries_the_entity_name_when_matched() {
    let payload = map_detection_to_event_payload(&DetectionInput {
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
