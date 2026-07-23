//! Freezes the MQTT topic names a Home Assistant automation or MQTT client
//! binds to directly: the outbound state topics (running condition,
//! availability, per-camera enabled/active/detection, snapshot image) and the
//! inbound command/status topics the production subscriber listens on.
//!
//! Every value below is produced by calling the real production functions
//! and constants with fixed inputs and recording what they return — never by
//! scanning source text. Renaming any one of these topics changes what an
//! installed Home Assistant automation is subscribed to or publishing on; it
//! is a deliberate, reviewed transition to a published identifier, not a
//! routine refactor.

// Pre-move qualified paths (`vigil::ha_discovery::...`, `vigil::DiscoveryPayload`)
// keep resolving unchanged: this crate was `vigil`'s `ha_discovery`/
// `ha_mqtt_tasks` modules before the adapter split, so aliasing the new
// crate under the old name keeps every path below byte-identical.
use vigil_ha as vigil;

use vigil::ha_discovery::{CameraConfig, ServiceConfig, generate_discovery_payloads};
use vigil::ha_discovery::{running_condition_topic, service_availability_topic};
use vigil::ha_mqtt_tasks::{
    CONTROL_COMMAND_TOPIC, CORRECTION_COMMAND_TOPIC, HOME_ASSISTANT_STATUS_TOPIC,
    enabled_state_topic, snapshot_topic,
};

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/contract-goldens/mqtt_topic_contract.golden"
);

const FIXED_SERVICE_ID: &str = "vigil-golden-service";
const FIXED_CAMERA_ID: &str = "golden-camera-alpha";
const FIXED_CAMERA_LABEL: &str = "Golden Camera Alpha";

fn discovery_field<'a>(
    payloads: &'a [vigil::DiscoveryPayload],
    component: &str,
    field: &str,
) -> &'a str {
    payloads
        .iter()
        .find(|payload| {
            payload.payload.get("component").and_then(|v| v.as_str()) == Some(component)
        })
        .and_then(|payload| payload.payload.get(field))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            panic!("expected discovery payload component={component} to carry field={field}")
        })
}

/// Renders the currently-published MQTT topic surface by calling the real
/// production functions and constants with fixed inputs — this is the
/// "current truth" the golden fixture pins.
fn render_current_topic_surface() -> Vec<String> {
    let config = ServiceConfig {
        service_name: "Golden Vigil".to_string(),
        service_id: FIXED_SERVICE_ID.to_string(),
        cameras: vec![CameraConfig {
            camera_id: FIXED_CAMERA_ID.to_string(),
            camera_label: FIXED_CAMERA_LABEL.to_string(),
        }],
    };
    let payloads = generate_discovery_payloads(&config);
    let detection_topic = discovery_field(&payloads, "event", "state_topic");
    let active_topic = discovery_field(&payloads, "binary_sensor", "state_topic");
    let image_topic = discovery_field(&payloads, "image", "image_topic");
    let switch_state_topic = discovery_field(&payloads, "switch", "state_topic");
    let switch_command_topic = discovery_field(&payloads, "switch", "command_topic");
    let button_command_topic = discovery_field(&payloads, "button", "command_topic");

    let mut lines = vec![
        format!("active_topic={active_topic}"),
        format!("control_command_topic_const={CONTROL_COMMAND_TOPIC}",),
        format!("correction_command_topic_const={CORRECTION_COMMAND_TOPIC}"),
        format!("detection_topic={detection_topic}"),
        format!(
            "enabled_state_topic_fn={}",
            enabled_state_topic(FIXED_SERVICE_ID, FIXED_CAMERA_ID)
        ),
        format!("home_assistant_status_topic_const={HOME_ASSISTANT_STATUS_TOPIC}"),
        format!("image_topic={image_topic}"),
        format!(
            "running_condition_topic_fn={}",
            running_condition_topic(FIXED_SERVICE_ID)
        ),
        format!(
            "service_availability_topic_fn={}",
            service_availability_topic(FIXED_SERVICE_ID)
        ),
        format!("snapshot_topic_fn={}", snapshot_topic(FIXED_CAMERA_ID)),
        format!("switch_command_topic={switch_command_topic}"),
        format!("switch_state_topic={switch_state_topic}"),
        format!("button_command_topic={button_command_topic}"),
    ];
    lines.sort();
    lines
}

fn read_golden() -> String {
    std::fs::read_to_string(GOLDEN)
        .unwrap_or_else(|error| panic!("read golden fixture {GOLDEN}: {error}"))
}

#[test]
fn published_mqtt_topic_surface_matches_frozen_registry() {
    let rendered = render_current_topic_surface().join("\n") + "\n";
    let frozen = read_golden();
    assert_eq!(
        rendered, frozen,
        "the published MQTT topic surface (what a Home Assistant automation subscribes to or \
         publishes on) no longer matches the checked-in registry at {GOLDEN}. Changing a \
         published MQTT topic name is a deliberate, reviewed transition, not a side effect of a \
         refactor — if this rename is intentional, update the golden fixture as part of a \
         reviewed change; otherwise restore the original topic string."
    );
}
