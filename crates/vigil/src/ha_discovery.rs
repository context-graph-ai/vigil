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
    /// The recognized entity's name when the detection matched an enrolled
    /// subject; None keeps the event an honest "unknown <class>".
    pub entity_name: Option<String>,
    pub match_score: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EventPayload {
    /// Must be "vigil_detection" — required by the HA MQTT event entity so
    /// Home Assistant can match published messages to the declared event_types.
    pub event_type: String,
    pub detection_id: String,
    pub camera: String,
    pub object_class: String,
    pub confidence: f64,
    pub timestamp_ms: i64,
    pub evidence_ref: String,
    pub snapshot_ref: String,
    pub entity_name: Option<String>,
    pub match_score: Option<f64>,
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

// ── Slug helper ────────────────────────────────────────────────────────────

/// Convert an arbitrary string into a stable lowercase slug using underscores
/// as separators.  Derived from `service_id` or `camera_id` so HA entity IDs
/// are stable across regeneration from the same config.
/// Exposed pub(crate) so ha_mqtt_tasks can derive image-topic keys.
pub(crate) fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

// ── Discovery payload generation ───────────────────────────────────────────

/// Generate HA MQTT discovery payloads for a Vigil service and its cameras.
///
/// Structure:
/// - One service-hub payload (sensor "running condition") with no
///   `via_device` — registered under the service device.
/// - Per-camera: five entity payloads (event, image, binary_sensor,
///   switch [enabled], button [snapshot]), each carrying
///   `device.via_device` pointing to the service hub.
///   The live-view camera is registered as an HA Generic Camera config entry
///   via the config-flow API (POST /core/api/config/config_entries/flow,
///   stream_source = the configured live RTSP URL) and is NOT part of MQTT
///   discovery.
///   The MQTT camera platform is image-only and cannot serve live streams.
///
/// All IDs and topics derive deterministically from `service_id` and
/// `camera_id` so successive calls with identical configs produce identical
/// payloads.  No `object_id` is emitted (removed in HA 2026.4).
///
/// Topic conventions
/// -----------------
/// MQTT topics use raw `service_id` / `camera_id` values to match what
/// `runtime.rs` publishes (no additional slug transform on the topic path).
/// HA entity IDs (unique_id, default_entity_id) use the `slug()` transform
/// (underscores) which is the HA naming convention.
pub fn generate_discovery_payloads(config: &ServiceConfig) -> Vec<DiscoveryPayload> {
    let mut payloads = Vec::new();

    let service_slug = slug(&config.service_id);
    let service_identifier = format!("vigil_{service_slug}");

    // Availability topic — raw service_id (no slug) to match runtime.rs.
    // Published as "online" on startup, "offline" via last-will on disconnect.
    // Every entity carries this so HA marks all Vigil entities unavailable when
    // the add-on goes away.
    let availability_topic = service_availability_topic(&config.service_id);

    // ── Service hub device ─────────────────────────────────────────────────
    // One sensor entity for the running-condition string at the service level.
    // No `via_device` — this is the parent device.
    let hub_device = json!({
        "identifiers": [service_identifier.clone()],
        "name": config.service_name,
        "manufacturer": "Vigil",
        "model": "Vigil NVR",
    });
    let hub_entity_slug = format!("vigil_{service_slug}_running");
    // slug(service_id) matches running_condition_topic(service_id) used by runtime.
    let hub_state_topic = format!("vigil/{service_slug}/running-condition");
    payloads.push(DiscoveryPayload {
        topic: format!("homeassistant/sensor/{service_identifier}/running/config"),
        payload: json!({
            "component": "sensor",
            "device": hub_device,
            "name": "Running Condition",
            "default_entity_id": format!("sensor.{hub_entity_slug}"),
            "unique_id": format!("{service_identifier}_running"),
            "state_topic": hub_state_topic,
            "availability_topic": availability_topic,
            "payload_available": "online",
            "payload_not_available": "offline",
        }),
    });

    // ── Per-camera sub-device entities ─────────────────────────────────────
    for camera in &config.cameras {
        let cam_slug = slug(&camera.camera_id);
        let cam_identifier = format!("vigil_{cam_slug}");

        let cam_device = json!({
            "identifiers": [cam_identifier.clone()],
            "name": camera.camera_label,
            "manufacturer": "Vigil",
            "model": "Vigil Camera",
            "via_device": service_identifier,
        });

        // MQTT topic paths — raw camera_id (not slugged) matches runtime publishing.
        let detection_topic = format!("vigil/{}/{}/detection", config.service_id, camera.camera_id);
        let active_topic = format!("vigil/{}/{}/active", config.service_id, camera.camera_id);
        let enabled_topic = format!("vigil/{}/{}/enabled", config.service_id, camera.camera_id);
        // Snapshot topic has no service_id prefix so the snapshot handler can derive
        // it from cmd.camera_id alone without needing service context.
        let snapshot_topic = format!("vigil/{}/snapshot", camera.camera_id);

        // event — detection events.
        // HA event entity requires state_topic and the published JSON must include
        // event_type matching one of the declared event_types.
        payloads.push(DiscoveryPayload {
            topic: format!("homeassistant/event/{cam_identifier}/detection/config"),
            payload: json!({
                "component": "event",
                "device": cam_device.clone(),
                "name": "Detection",
                "default_entity_id": format!("event.{cam_identifier}_detection"),
                "unique_id": format!("{cam_identifier}_detection"),
                "state_topic": detection_topic,
                "event_types": ["vigil_detection"],
                "availability_topic": availability_topic,
                "payload_available": "online",
                "payload_not_available": "offline",
            }),
        });

        // image — latest detection snapshot (JPEG bytes published on snapshot_topic).
        // HA image entity requires image_topic where raw bytes are published.
        payloads.push(DiscoveryPayload {
            topic: format!("homeassistant/image/{cam_identifier}/snapshot/config"),
            payload: json!({
                "component": "image",
                "device": cam_device.clone(),
                "name": "Snapshot",
                "default_entity_id": format!("image.{cam_identifier}_snapshot"),
                "unique_id": format!("{cam_identifier}_snapshot"),
                "image_topic": snapshot_topic,
                "availability_topic": availability_topic,
                "payload_available": "online",
                "payload_not_available": "offline",
            }),
        });

        // binary_sensor — active/motion flag.
        // HA binary_sensor requires state_topic, payload_on, payload_off.
        payloads.push(DiscoveryPayload {
            topic: format!("homeassistant/binary_sensor/{cam_identifier}/active/config"),
            payload: json!({
                "component": "binary_sensor",
                "device": cam_device.clone(),
                "name": "Active",
                "default_entity_id": format!("binary_sensor.{cam_identifier}_active"),
                "unique_id": format!("{cam_identifier}_active"),
                "device_class": "motion",
                "state_topic": active_topic,
                "payload_on": "ON",
                "payload_off": "OFF",
                "availability_topic": availability_topic,
                "payload_available": "online",
                "payload_not_available": "offline",
            }),
        });

        // NOTE: no MQTT camera entity here.  The HA MQTT camera platform is
        // image-only (no stream_source key), so it cannot serve live video.
        // Live view is provided by a Generic Camera config entry created at
        // runtime via the HA config-flow API (`register_generic_camera` in
        // runtime.rs), with stream_source set from the configured live RTSP URL.

        // switch — enabled control with reflected retained state.
        payloads.push(DiscoveryPayload {
            topic: format!("homeassistant/switch/{cam_identifier}/enabled/config"),
            payload: json!({
                "component": "switch",
                "device": cam_device.clone(),
                "name": "Enabled",
                "default_entity_id": format!("switch.{cam_identifier}_enabled"),
                "unique_id": format!("{cam_identifier}_enabled"),
                "state_topic": enabled_topic,
                "command_topic": "vigil/commands/control",
                "payload_on": format!(
                    r#"{{"service_id":"{}","camera_id":"{}","action":"enable"}}"#,
                    config.service_id, camera.camera_id
                ),
                "payload_off": format!(
                    r#"{{"service_id":"{}","camera_id":"{}","action":"disable"}}"#,
                    config.service_id, camera.camera_id
                ),
                "state_on": "ON",
                "state_off": "OFF",
                "availability_topic": availability_topic,
                "payload_available": "online",
                "payload_not_available": "offline",
            }),
        });

        // button — snapshot trigger (publishes latest detector evidence PNG to image_topic)
        payloads.push(DiscoveryPayload {
            topic: format!("homeassistant/button/{cam_identifier}/snapshot/config"),
            payload: json!({
                "component": "button",
                "device": cam_device.clone(),
                "name": "Snapshot Trigger",
                "default_entity_id": format!("button.{cam_identifier}_snapshot_trigger"),
                "unique_id": format!("{cam_identifier}_snapshot_trigger"),
                "command_topic": "vigil/commands/control",
                "payload_press": format!(r#"{{"camera_id":"{}","action":"snapshot"}}"#, camera.camera_id),
                "availability_topic": availability_topic,
                "payload_available": "online",
                "payload_not_available": "offline",
            }),
        });
    }

    payloads
}

/// Return the MQTT topic on which the running-condition string is published.
/// Stable across restarts; derived from the service_id.
pub fn running_condition_topic(service_id: &str) -> String {
    format!("vigil/{}/running-condition", slug(service_id))
}

/// Return the MQTT availability topic for the Vigil service.
///
/// All Vigil HA entities carry this as their `availability_topic`.  The
/// runtime publishes `"online"` here on startup and the MQTT last-will
/// publishes `"offline"` here on unclean disconnect, making every Vigil
/// entity in HA show "unavailable" when the service goes away.
///
/// Uses the raw service_id (not slug-transformed) so it matches the
/// `avail_topic` that `runtime.rs` constructs inline.
pub fn service_availability_topic(service_id: &str) -> String {
    format!("vigil/{}/availability", service_id)
}

/// Map a detection observation to an event payload for publication to the broker.
pub fn map_detection_to_event_payload(detection: &DetectionInput) -> EventPayload {
    EventPayload {
        event_type: "vigil_detection".to_string(),
        detection_id: detection.observation_id.clone(),
        camera: detection.camera_name.clone(),
        object_class: detection.object_class.clone(),
        confidence: detection.confidence,
        timestamp_ms: detection.timestamp_ms,
        evidence_ref: detection.evidence_ref.clone(),
        snapshot_ref: detection.snapshot_ref.clone(),
        entity_name: detection.entity_name.clone(),
        match_score: detection.match_score,
    }
}

/// Map a health status to the running-condition string published on the broker.
pub(crate) fn map_health_to_running_condition(health: HealthStatus) -> &'static str {
    match health {
        HealthStatus::Starting => "running",
        HealthStatus::Ready => "running",
        HealthStatus::StoreOpenFailed => "store-open-failed",
        HealthStatus::IngestFailed => "ingest-failed",
        HealthStatus::DiskFull => "disk-full",
        HealthStatus::KeepPaceFailed => "keep-pace-failed",
    }
}

/// Parse a command-topic message into a channel-agnostic CorrectionRequest.
pub fn parse_command_topic(msg: &CommandTopicMessage) -> Result<CorrectionRequest, ParseError> {
    let correction_type = match msg.correction_type.as_str() {
        "identity" | "Identity" => CorrectionType::Identity,
        "wrong_class" | "WrongClass" => CorrectionType::WrongClass,
        "false_alarm" | "FalseAlarm" => CorrectionType::FalseAlarm,
        "enroll" | "Enroll" => CorrectionType::Enroll,
        other => {
            return Err(ParseError(format!(
                "unknown correction_type '{other}'; expected identity|wrong_class|false_alarm|enroll"
            )));
        }
    };
    Ok(CorrectionRequest {
        detection_id: msg.detection_id.clone(),
        label: msg.label.clone(),
        correction_type,
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

        // Each camera contributes several entity payloads that all share ONE sub-device
        // identity, so assert exactly one DISTINCT sub-device (by device identifiers) for a
        // one-camera config — counting payloads would always exceed one.
        let distinct_sub_devices: std::collections::BTreeSet<&str> = sub_devices
            .iter()
            .filter_map(|p| {
                p.payload
                    .get("device")
                    .and_then(|d| d.get("identifiers"))
                    .and_then(|i| i.get(0))
                    .and_then(|v| v.as_str())
            })
            .collect();
        assert_eq!(
            distinct_sub_devices.len(),
            1,
            "expected exactly one distinct per-camera sub-device; got {}: wrong stub emits a \
             flat structure or duplicate sub-device identities",
            distinct_sub_devices.len()
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
        // Per-camera MQTT entities: event (detection), image (snapshot display),
        // binary_sensor (active/motion), switch (enabled), and button (snapshot).
        // The live-view camera is a Generic Camera config entry (not MQTT) — see
        // `register_generic_camera` in runtime.rs.
        let expected_components: std::collections::BTreeSet<&str> =
            ["event", "image", "binary_sensor", "switch", "button"]
                .iter()
                .copied()
                .collect();
        assert_eq!(
            via_device_components, expected_components,
            "per-camera sub-device must carry exactly the closed MQTT component set \
             {{event, image, binary_sensor, switch, button}} — no camera component (live view is a \
             Generic Camera config entry, not MQTT); \
             wrong stub emits flat structure with no via_device → empty component set"
        );

        // Exact count: 5 per-camera MQTT payloads.
        // event(1) + image(1) + binary_sensor(1) + switch(1) + button(1) = 5.
        // The camera/live-view is a Generic Camera config entry, NOT an MQTT entity.
        // Goes RED if enable/disable remain blind buttons (would be 6).
        assert_eq!(
            sub_devices.len(),
            5,
            "per-camera MQTT discovery must register exactly 5 entities \
             {{event, image, binary_sensor, switch, button}} — the live-view camera is a \
             Generic Camera config entry; got {}",
            sub_devices.len()
        );

        // Two-camera config → two sub-devices
        let config_two = two_camera_config();
        let payloads_two = generate_discovery_payloads(&config_two);
        let distinct_sub_devices_two: std::collections::BTreeSet<&str> = payloads_two
            .iter()
            .filter(|p| {
                p.payload
                    .get("device")
                    .and_then(|d| d.get("via_device"))
                    .is_some()
            })
            .filter_map(|p| {
                p.payload
                    .get("device")
                    .and_then(|d| d.get("identifiers"))
                    .and_then(|i| i.get(0))
                    .and_then(|v| v.as_str())
            })
            .collect();
        assert_eq!(
            distinct_sub_devices_two.len(),
            2,
            "expected two distinct per-camera sub-devices for two-camera config; got {}",
            distinct_sub_devices_two.len()
        );
    }

    #[test]
    fn per_camera_entity_names_are_device_local_and_enabled_is_stateful_switch() {
        let config = one_camera_config();
        let payloads = generate_discovery_payloads(&config);

        let entity_by_id = |entity_id: &str| {
            payloads
                .iter()
                .find(|payload| {
                    payload
                        .payload
                        .get("default_entity_id")
                        .and_then(|value| value.as_str())
                        == Some(entity_id)
                })
                .unwrap_or_else(|| panic!("missing discovery payload for {entity_id}"))
        };

        for (entity_id, expected_name) in [
            ("event.vigil_cam_lower_gate_detection", "Detection"),
            ("image.vigil_cam_lower_gate_snapshot", "Snapshot"),
            ("binary_sensor.vigil_cam_lower_gate_active", "Active"),
            ("switch.vigil_cam_lower_gate_enabled", "Enabled"),
            (
                "button.vigil_cam_lower_gate_snapshot_trigger",
                "Snapshot Trigger",
            ),
        ] {
            let payload = entity_by_id(entity_id);
            let actual_name = payload
                .payload
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            assert_eq!(
                actual_name, expected_name,
                "per-camera entity {entity_id} must use the device-local name '{expected_name}', \
                 not a camera-prefixed name like 'lower gate {expected_name}' that Home Assistant \
                 displays as a doubled name under the camera device"
            );
        }

        let enabled = entity_by_id("switch.vigil_cam_lower_gate_enabled");
        assert_eq!(enabled.payload["component"].as_str(), Some("switch"));
        assert_eq!(
            enabled.payload["state_topic"].as_str(),
            Some("vigil/vigil-site-alpha/cam-lower-gate/enabled"),
            "enabled switch must have a retained state topic so HA reflects the live camera state"
        );
        assert_eq!(
            enabled.payload["command_topic"].as_str(),
            Some("vigil/commands/control"),
            "enabled switch must send enable/disable commands through the existing control topic"
        );
        assert_eq!(
            enabled.payload["payload_on"].as_str(),
            Some(
                r#"{"service_id":"vigil-site-alpha","camera_id":"cam-lower-gate","action":"enable"}"#
            ),
        );
        assert_eq!(
            enabled.payload["payload_off"].as_str(),
            Some(
                r#"{"service_id":"vigil-site-alpha","camera_id":"cam-lower-gate","action":"disable"}"#
            ),
        );
        assert_eq!(enabled.payload["state_on"].as_str(), Some("ON"));
        assert_eq!(enabled.payload["state_off"].as_str(), Some("OFF"));
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
    fn detection_event_payload_carries_current_contract_without_future_zone_field() {
        let detection = DetectionInput {
            observation_id: "11111111-2222-3333-4444-555555555555".to_string(),
            camera_name: "lower gate".to_string(),
            object_class: "vehicle".to_string(), // deliberately NOT "person"
            confidence: 0.87,
            timestamp_ms: 1_700_000_000_000,
            evidence_ref: "clips/2024-01-01/clip-abc.mp4".to_string(),
            snapshot_ref: "snapshots/2024-01-01/snap-abc.jpg".to_string(), // distinct from evidence_ref
            entity_name: None,
            match_score: None,
        };

        let payload = map_detection_to_event_payload(&detection);

        // detection_id must value-equal the ObservationId
        assert_eq!(
            payload.detection_id, detection.observation_id,
            "event payload detection_id must equal the observation id; \
             wrong stub drops it (got empty string)"
        );

        // event_type must be "vigil_detection" — required by the HA MQTT event entity.
        // The published JSON must carry this field matching the event_types declaration
        // in the discovery config or Home Assistant will reject the event.
        assert_eq!(
            payload.event_type, "vigil_detection",
            "event payload event_type must be 'vigil_detection' (HA MQTT event entity \
             requirement); missing or wrong value breaks HA event recognition"
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

        // confidence, timestamp_ms, camera value-equality
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
        let json = serde_json::to_value(&payload).expect("event payload serializes");
        assert!(
            json.get("zone").is_none(),
            "zone must stay off the public event schema until zone configuration and runtime matching ship, got {json}"
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

        let whole_service_running_conditions = payloads
            .iter()
            .filter(|payload| {
                payload
                    .payload
                    .get("device")
                    .and_then(|device| device.get("via_device"))
                    .is_none()
                    && payload
                        .payload
                        .get("component")
                        .and_then(|value| value.as_str())
                        == Some("sensor")
                    && payload
                        .payload
                        .get("state_topic")
                        .and_then(|value| value.as_str())
                        .is_some_and(|topic| topic.ends_with("/running-condition"))
            })
            .count();
        assert_eq!(
            whole_service_running_conditions, 1,
            "discovery must contain exactly one whole-service running-condition entity"
        );

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

    /// RED — wrong stub emits discovery payloads missing required HA MQTT fields.
    ///
    /// Each component type has fields that HA requires to function correctly:
    /// - event: state_topic + event_types
    /// - image: image_topic
    /// - binary_sensor: state_topic + payload_on + payload_off
    /// - switch: state_topic + command_topic + payload/state on/off
    /// - All per-camera entities: availability_topic + payload_available + payload_not_available
    ///
    /// Note: there is no MQTT camera entity — live view is a Generic Camera config
    /// entry created via the config-flow API (see `register_generic_camera` in runtime.rs).
    ///
    /// Fails on any missing required field.
    #[test]
    fn discovery_entities_carry_required_ha_fields() {
        let config = one_camera_config();
        let payloads = generate_discovery_payloads(&config);

        for p in &payloads {
            let component = p
                .payload
                .get("component")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            let is_per_camera = p
                .payload
                .get("device")
                .and_then(|d| d.get("via_device"))
                .is_some();
            let topic = &p.topic;

            // All per-camera entities must carry availability metadata so HA can
            // mark Vigil entities unavailable when the add-on disconnects.
            if is_per_camera {
                let avail = p
                    .payload
                    .get("availability_topic")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                assert!(
                    !avail.is_empty(),
                    "entity on topic '{topic}' (component={component}) is missing \
                     availability_topic; all per-camera entities must carry it"
                );
                let avail_payload = p
                    .payload
                    .get("payload_available")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                assert_eq!(
                    avail_payload, "online",
                    "entity on topic '{topic}' must have payload_available='online'; got '{avail_payload}'"
                );
                let unavail_payload = p
                    .payload
                    .get("payload_not_available")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                assert_eq!(
                    unavail_payload, "offline",
                    "entity on topic '{topic}' must have payload_not_available='offline'; got '{unavail_payload}'"
                );
            }

            match component {
                "event" => {
                    // HA MQTT event entity requires state_topic and event_types.
                    let state_topic = p
                        .payload
                        .get("state_topic")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    assert!(
                        !state_topic.is_empty(),
                        "event entity on topic '{topic}' is missing state_topic (required by HA MQTT event)"
                    );
                    let event_types = p.payload.get("event_types").and_then(|v| v.as_array());
                    assert!(
                        event_types.is_some() && !event_types.unwrap().is_empty(),
                        "event entity on topic '{topic}' is missing event_types (required by HA MQTT event)"
                    );
                }
                "image" => {
                    // HA MQTT image entity requires image_topic.
                    let image_topic = p
                        .payload
                        .get("image_topic")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    assert!(
                        !image_topic.is_empty(),
                        "image entity on topic '{topic}' is missing image_topic (required by HA MQTT image)"
                    );
                }
                "binary_sensor" => {
                    // HA MQTT binary_sensor requires state_topic, payload_on, payload_off.
                    let state_topic = p
                        .payload
                        .get("state_topic")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    assert!(
                        !state_topic.is_empty(),
                        "binary_sensor on topic '{topic}' is missing state_topic (required by HA MQTT binary_sensor)"
                    );
                    let payload_on = p
                        .payload
                        .get("payload_on")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    assert_eq!(
                        payload_on, "ON",
                        "binary_sensor on topic '{topic}' must have payload_on='ON'; got '{payload_on}'"
                    );
                    let payload_off = p
                        .payload
                        .get("payload_off")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    assert_eq!(
                        payload_off, "OFF",
                        "binary_sensor on topic '{topic}' must have payload_off='OFF'; got '{payload_off}'"
                    );
                }
                "switch" => {
                    let state_topic = p
                        .payload
                        .get("state_topic")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    assert!(
                        !state_topic.is_empty(),
                        "switch entity on topic '{topic}' is missing state_topic; the enabled control must reflect live state"
                    );
                    let command_topic = p
                        .payload
                        .get("command_topic")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    assert!(
                        !command_topic.is_empty(),
                        "switch entity on topic '{topic}' is missing command_topic"
                    );
                    for key in ["payload_on", "payload_off", "state_on", "state_off"] {
                        assert!(
                            p.payload
                                .get(key)
                                .and_then(|v| v.as_str())
                                .is_some_and(|value| !value.is_empty()),
                            "switch entity on topic '{topic}' is missing required field '{key}'"
                        );
                    }
                }
                _ => {
                    // No MQTT camera entity — live view is a Generic Camera config
                    // entry, not an MQTT discovery payload.
                }
            }
        }
    }

    /// RED — wrong stub maps correction_type into detection_id, drops label.
    /// Fails on the detection_id and label value-equality assertions.
    #[test]
    fn command_topic_parses_to_channel_agnostic_request() {
        let msg = CommandTopicMessage {
            detection_id: "aabbccdd-1234-5678-9abc-ddeeff001122".to_string(),
            label: Some("that's Arjun".to_string()),
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
