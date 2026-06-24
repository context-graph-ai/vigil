use serde_json::{Value, json};

use crate::correction::{CorrectionRequest, CorrectionType};
use crate::health::HealthStatus;

// ── Service / camera config types ─────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub service_name: String,
    pub service_id: String,
    pub cameras: Vec<CameraConfig>,
}

#[derive(Debug, Clone)]
pub struct CameraConfig {
    pub camera_id: String,
    pub camera_label: String,
}

// ── Discovery payload ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DiscoveryPayload {
    pub topic: String,
    pub payload: Value,
}

// ── Detection event payload ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DetectionInput {
    pub observation_id: String,
    pub camera_name: String,
    pub object_class: String,
    pub confidence: f64,
    pub timestamp_ms: i64,
    pub evidence_ref: String,
    pub snapshot_ref: String,
    pub zone: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EventPayload {
    pub detection_id: String,
    pub camera: String,
    pub object_class: String,
    pub confidence: f64,
    pub timestamp_ms: i64,
    pub evidence_ref: String,
    pub snapshot_ref: String,
    pub zone: Option<String>,
}

// ── Command-topic wire type ────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CommandTopicMessage {
    pub detection_id: String,
    pub label: Option<String>,
    pub correction_type: String,
}

#[derive(Debug)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "parse error: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

// ── Wrong stubs ────────────────────────────────────────────────────────────

/// WRONG STUB: generates a flat discovery tree with per-call random UUIDs as
/// entity ids, emits `object_id` instead of `default_entity_id`, and includes
/// a per-camera health entity, causing:
///   - discovery_registers_device_and_per_camera_subdevice   → flat tree → FAIL
///   - discovery_payload_uses_default_entity_id_not_object_id → object_id present → FAIL
///   - entity_ids_and_topics_stable_across_regeneration       → UUIDs differ per call → FAIL
///   - discovery_has_no_per_camera_health_entity              → health entity present → FAIL
pub fn generate_discovery_payloads(config: &ServiceConfig) -> Vec<DiscoveryPayload> {
    let mut payloads = Vec::new();

    // wrong stub: use uuid-v4 per call — different on each generation, breaking stability
    let device_id = uuid::Uuid::new_v4().to_string();

    // wrong stub: flat device entry (no sub-devices, no via_device linkage)
    let device_payload = json!({
        "device": {
            "identifiers": [device_id.clone()],
            "name": config.service_name.clone(),
        },
        // wrong stub: emits object_id (removed in HA 2026.4) instead of default_entity_id
        "object_id": format!("vigil_{}_running", device_id),
        "name": "Running Condition",
        "component": "sensor",
    });
    payloads.push(DiscoveryPayload {
        topic: format!("homeassistant/sensor/{device_id}/running_condition/config"),
        payload: device_payload,
    });

    for camera in &config.cameras {
        let cam_id = uuid::Uuid::new_v4().to_string(); // wrong stub: per-call random

        // wrong stub: per-camera health entity — this is the forbidden entity the test rejects
        let health_payload = json!({
            "device": {
                "identifiers": [cam_id.clone()],
                "name": camera.camera_label.clone(),
                // wrong stub: no via_device linkage to parent
            },
            "object_id": format!("cam_{cam_id}_health"),
            "name": "Camera Health",
            "component": "sensor",
            "entity_role": "camera_health", // the role the test discriminates on
        });
        payloads.push(DiscoveryPayload {
            topic: format!("homeassistant/sensor/{cam_id}/camera_health/config"),
            payload: health_payload,
        });

        // wrong stub: event entity without sub-device linkage to parent device
        let event_payload = json!({
            "device": {
                "identifiers": [cam_id.clone()],
                "name": camera.camera_label.clone(),
            },
            "object_id": format!("cam_{cam_id}_detection"),
            "name": "Detection Event",
            "component": "event",
        });
        payloads.push(DiscoveryPayload {
            topic: format!("homeassistant/event/{cam_id}/detection/config"),
            payload: event_payload,
        });
    }

    payloads
}

/// WRONG STUB: hardcodes object_class = "person" regardless of input, drops
/// detection_id, drops evidence_ref, and omits snapshot_ref.
///
/// Fails on `detection_event_payload_carries_contract_with_person_empty_zone`:
/// the non-"person" class value-equality assertion when input is "vehicle".
pub fn map_detection_to_event_payload(detection: &DetectionInput) -> EventPayload {
    EventPayload {
        detection_id: String::new(), // wrong stub: dropped
        camera: detection.camera_name.clone(),
        object_class: "person".to_string(), // wrong stub: hardcoded, ignores detection.object_class
        confidence: detection.confidence,
        timestamp_ms: detection.timestamp_ms,
        evidence_ref: String::new(), // wrong stub: dropped
        snapshot_ref: String::new(), // wrong stub: dropped
        zone: detection.zone.clone(),
    }
}

/// WRONG STUB: always returns "running" regardless of HealthStatus, causing:
///   - running_condition_maps_each_named_fault → faults all map to "running" → FAIL
// Called only from the unit-test module below; suppress the dead-code lint
// for the non-test lib build (the correct implementation will call this from
// publish_discovery_to_broker / publish_availability_online).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn map_health_to_running_condition(health: HealthStatus) -> &'static str {
    let _ = health; // wrong stub: ignores input
    "running"
}

/// WRONG STUB: maps correction_type string into detection_id and leaves label
/// empty, causing:
///   - command_topic_parses_to_channel_agnostic_request → detection_id wrong value → FAIL
pub fn parse_command_topic(msg: &CommandTopicMessage) -> Result<CorrectionRequest, ParseError> {
    // wrong stub: puts correction_type into detection_id and discards label
    Ok(CorrectionRequest {
        detection_id: msg.correction_type.clone(), // wrong: should be msg.detection_id
        label: None,                               // wrong: should be msg.label
        correction_type: CorrectionType::FalseAlarm, // wrong: should parse msg.correction_type
    })
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn one_camera_config() -> ServiceConfig {
        ServiceConfig {
            service_name: "Vigil".to_string(),
            service_id: "vigil-site-alpha".to_string(),
            cameras: vec![CameraConfig {
                camera_id: "cam-lower-gate".to_string(),
                camera_label: "lower gate".to_string(),
            }],
        }
    }

    fn two_camera_config() -> ServiceConfig {
        ServiceConfig {
            service_name: "Vigil".to_string(),
            service_id: "vigil-site-alpha".to_string(),
            cameras: vec![
                CameraConfig {
                    camera_id: "cam-lower-gate".to_string(),
                    camera_label: "lower gate".to_string(),
                },
                CameraConfig {
                    camera_id: "cam-driveway".to_string(),
                    camera_label: "driveway".to_string(),
                },
            ],
        }
    }

    /// RED — wrong stub: flat tree, per-call UUIDs, extra health entity.
    /// Fails on sub-device linkage / closed-entity-set / camera-name assertions.
    #[test]
    fn discovery_registers_device_and_per_camera_subdevice() {
        let config_one = one_camera_config();
        let payloads = generate_discovery_payloads(&config_one);

        // Collect sub-device payloads (those with via_device linking to parent)
        let sub_devices: Vec<&DiscoveryPayload> = payloads
            .iter()
            .filter(|p| {
                p.payload
                    .get("device")
                    .and_then(|d| d.get("via_device"))
                    .is_some()
            })
            .collect();

        // Assert exactly one sub-device linked to parent for one-camera config
        assert_eq!(
            sub_devices.len(),
            1,
            "expected exactly one per-camera sub-device linked to parent via via_device; \
             got {}: wrong stub emits flat structure",
            sub_devices.len()
        );

        // Assert sub-device name equals configured camera label, not a generic string
        let sub_name = sub_devices[0]
            .payload
            .get("device")
            .and_then(|d| d.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        assert_eq!(
            sub_name, "lower gate",
            "expected sub-device name to equal the configured camera label 'lower gate'; \
             got '{sub_name}'"
        );

        // Assert exact closed component set across all via_device-linked payloads (one-camera).
        let via_device_components: std::collections::BTreeSet<&str> = payloads
            .iter()
            .filter(|p| {
                p.payload
                    .get("device")
                    .and_then(|d| d.get("via_device"))
                    .is_some()
            })
            .filter_map(|p| p.payload.get("component").and_then(|c| c.as_str()))
            .collect();
        let expected_components: std::collections::BTreeSet<&str> =
            ["event", "image", "binary_sensor", "camera", "button"]
                .iter()
                .copied()
                .collect();
        assert_eq!(
            via_device_components, expected_components,
            "per-camera sub-device must carry exactly the closed component set \
             {{event, image, binary_sensor, camera, button}} and no extras; \
             wrong stub emits flat structure with no via_device → empty component set"
        );

        // Two-camera config → two sub-devices
        let config_two = two_camera_config();
        let payloads_two = generate_discovery_payloads(&config_two);
        let sub_devices_two: Vec<&DiscoveryPayload> = payloads_two
            .iter()
            .filter(|p| {
                p.payload
                    .get("device")
                    .and_then(|d| d.get("via_device"))
                    .is_some()
            })
            .collect();
        assert_eq!(
            sub_devices_two.len(),
            2,
            "expected two per-camera sub-devices for two-camera config; got {}",
            sub_devices_two.len()
        );
    }

    /// RED — wrong stub emits object_id and omits default_entity_id.
    /// Fails on default_entity_id presence / object_id absence assertions.
    #[test]
    fn discovery_payload_uses_default_entity_id_not_object_id() {
        let config = one_camera_config();
        let payloads = generate_discovery_payloads(&config);

        for p in &payloads {
            let component = p
                .payload
                .get("component")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            let default_entity_id = p
                .payload
                .get("default_entity_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            // assert default_entity_id is present and STARTS WITH "{component}."
            assert!(
                !default_entity_id.is_empty(),
                "payload on topic '{}' is missing default_entity_id; \
                 wrong stub emits object_id instead",
                p.topic
            );
            assert!(
                default_entity_id.starts_with(&format!("{component}.")),
                "payload on topic '{}' has default_entity_id '{}' that does not start with \
                 the domain prefix '{component}.'; default_entity_id must be \
                 '<component>.<slug>' (e.g. 'event.vigil_cam_detection')",
                p.topic,
                default_entity_id
            );

            // assert object_id is absent
            assert!(
                p.payload.get("object_id").is_none(),
                "payload on topic '{}' must not contain object_id (removed in HA 2026.4); \
                 wrong stub emits it",
                p.topic
            );
        }
    }

    /// RED — wrong stub uses per-call random UUIDs; same config → different ids.
    /// Fails because two generations produce different (topic, entity-id) sets.
    #[test]
    fn entity_ids_and_topics_stable_across_regeneration() {
        let config = one_camera_config();
        let gen1 = generate_discovery_payloads(&config);
        let gen2 = generate_discovery_payloads(&config);

        let topics1: Vec<&str> = gen1.iter().map(|p| p.topic.as_str()).collect();
        let topics2: Vec<&str> = gen2.iter().map(|p| p.topic.as_str()).collect();

        assert_eq!(
            topics1, topics2,
            "discovery topics must be stable across regeneration from the same config; \
             wrong stub uses per-call random UUIDs so topics differ"
        );

        // And a different config must yield different ids
        let config2 = ServiceConfig {
            service_name: "Vigil".to_string(),
            service_id: "vigil-site-beta".to_string(), // different service_id
            cameras: vec![CameraConfig {
                camera_id: "cam-back-door".to_string(),
                camera_label: "back door".to_string(),
            }],
        };
        let gen3 = generate_discovery_payloads(&config2);
        let topics3: Vec<&str> = gen3.iter().map(|p| p.topic.as_str()).collect();

        assert_ne!(
            topics1, topics3,
            "discovery topics must differ for different camera/service configs; \
             ids must derive from identity, not be constant"
        );
    }

    /// RED — wrong stub hardcodes class="person", drops detection_id, drops evidence_ref.
    /// Fails on the object_class value-equality with the NON-"person" input "vehicle".
    #[test]
    fn detection_event_payload_carries_contract_with_person_empty_zone() {
        let detection = DetectionInput {
            observation_id: "11111111-2222-3333-4444-555555555555".to_string(),
            camera_name: "lower gate".to_string(),
            object_class: "vehicle".to_string(), // deliberately NOT "person"
            confidence: 0.87,
            timestamp_ms: 1_700_000_000_000,
            evidence_ref: "clips/2024-01-01/clip-abc.mp4".to_string(),
            snapshot_ref: "snapshots/2024-01-01/snap-abc.jpg".to_string(), // distinct from evidence_ref
            zone: None,
        };

        let payload = map_detection_to_event_payload(&detection);

        // detection_id must value-equal the ObservationId
        assert_eq!(
            payload.detection_id, detection.observation_id,
            "event payload detection_id must equal the observation id; \
             wrong stub drops it (got empty string)"
        );

        // object_class must READ from input, not hardcode "person"
        assert_eq!(
            payload.object_class, "vehicle",
            "event payload object_class must value-equal the detection's class 'vehicle'; \
             wrong stub hardcodes 'person'"
        );

        // evidence_ref must be carried
        assert_eq!(
            payload.evidence_ref, detection.evidence_ref,
            "event payload must carry evidence_ref; wrong stub drops it"
        );

        // snapshot_ref must be present and distinct from evidence_ref
        assert!(
            !payload.snapshot_ref.is_empty(),
            "snapshot_ref must be present"
        );
        assert_ne!(
            payload.snapshot_ref, payload.evidence_ref,
            "snapshot_ref must be distinct from evidence_ref"
        );

        // confidence, timestamp_ms, camera, zone value-equality
        assert_eq!(
            payload.confidence, detection.confidence,
            "event payload confidence must value-equal detection.confidence; \
             wrong stub passes confidence through but this assertion adds coverage"
        );
        assert_eq!(
            payload.timestamp_ms, detection.timestamp_ms,
            "event payload timestamp_ms must value-equal detection.timestamp_ms; \
             wrong stub passes timestamp_ms through"
        );
        assert_eq!(
            payload.camera, detection.camera_name,
            "event payload camera must value-equal detection.camera_name; \
             wrong stub sets camera = camera_name so this tightens the contract"
        );
        assert!(
            payload.zone.is_none(),
            "event payload zone must be None when detection.zone is None; \
             got {:?}",
            payload.zone
        );
    }

    /// RED — wrong stub always returns "running"; faults collapse to a constant.
    /// Fails on each fault's specific named value assertion.
    #[test]
    fn running_condition_maps_each_named_fault() {
        let cases: &[(HealthStatus, &str)] = &[
            (HealthStatus::Ready, "running"),
            (HealthStatus::StoreOpenFailed, "store-open-failed"),
            (HealthStatus::IngestFailed, "ingest-failed"),
            (HealthStatus::DiskFull, "disk-full"),
            (HealthStatus::KeepPaceFailed, "keep-pace-failed"),
        ];

        for (status, expected) in cases {
            let result = map_health_to_running_condition(*status);
            assert_eq!(
                result, *expected,
                "running condition for {status:?} must be '{expected}'; \
                 wrong stub returns 'running' for all states"
            );
        }
    }

    /// RED — wrong stub includes a per-camera health entity with role "camera_health".
    /// Fails on the no-per-camera-health-role assertion.
    #[test]
    fn discovery_has_no_per_camera_health_entity() {
        let config = one_camera_config();
        let payloads = generate_discovery_payloads(&config);

        // Broadened discriminator — any health/diagnostic role or health-ish device_class
        // on ANY entity (health roles regardless of via_device; device_class/availability
        // only on per-camera via_device-linked entities).
        let health_entities: Vec<&DiscoveryPayload> = payloads
            .iter()
            .filter(|p| {
                let entity_role = p
                    .payload
                    .get("entity_role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("");
                let is_per_camera = p
                    .payload
                    .get("device")
                    .and_then(|d| d.get("via_device"))
                    .is_some();
                let device_class = p
                    .payload
                    .get("device_class")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                let component = p
                    .payload
                    .get("component")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");

                // Any health-ish entity_role (not just three hardcoded strings)
                let has_health_role = entity_role.contains("health")
                    || entity_role.contains("problem")
                    || entity_role.contains("status")
                    || entity_role.contains("diagnostic");

                // Per-camera entity with health-ish device_class
                let has_health_device_class =
                    is_per_camera && matches!(device_class, "connectivity" | "problem" | "running");

                // Per-camera entity typed as availability component
                let has_availability_component = is_per_camera && component == "availability";

                has_health_role || has_health_device_class || has_availability_component
            })
            .collect();

        assert!(
            health_entities.is_empty(),
            "discovery must contain no per-camera health/availability entity (discriminated \
             by entity role, not name); wrong stub registers {} health entities: {:?}",
            health_entities.len(),
            health_entities
                .iter()
                .map(|p| p.topic.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// RED — wrong stub maps correction_type into detection_id, drops label.
    /// Fails on the detection_id and label value-equality assertions.
    #[test]
    fn command_topic_parses_to_channel_agnostic_request() {
        let msg = CommandTopicMessage {
            detection_id: "aabbccdd-1234-5678-9abc-ddeeff001122".to_string(),
            label: Some("that's Roshan".to_string()),
            correction_type: "identity".to_string(),
        };

        let req = parse_command_topic(&msg).expect("parse must not error on valid input");

        assert_eq!(
            req.detection_id, msg.detection_id,
            "CorrectionRequest.detection_id must value-equal the wire detection_id; \
             wrong stub maps correction_type string into it"
        );

        assert_eq!(
            req.label, msg.label,
            "CorrectionRequest.label must value-equal the owner's free text; \
             wrong stub discards it"
        );

        // correction_type must parse "identity" as CorrectionType::Identity
        assert_eq!(
            req.correction_type,
            CorrectionType::Identity,
            "CorrectionRequest.correction_type must parse 'identity' as CorrectionType::Identity; \
             wrong stub hardcodes FalseAlarm regardless of the wire value"
        );

        // The "no platform field" property is structural: CorrectionRequest has no
        // topic, entity_id, or HA concept fields — compile-time guarantee by its definition.
    }
}
