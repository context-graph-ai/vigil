//! Freezes the Home Assistant discovery identity surface: the discovery
//! config topics, `default_entity_id` / `unique_id` values, and the device /
//! sub-device `identifiers` a Home Assistant installation keys its entity
//! registry on. Every value below is produced by calling the real
//! `generate_discovery_payloads` production function with fixed inputs and
//! recording what it returns — never by scanning source text.
//!
//! Home Assistant's entity registry ties dashboards, automations, and
//! history to `unique_id` (and, on first discovery, `default_entity_id`).
//! Renaming any one of these is a deliberate, reviewed transition to a
//! published identifier: it makes Home Assistant treat the renamed thing as
//! a brand-new entity, orphaning the installed automation and history bound
//! to the old one.

// Pre-move qualified paths (`vigil::ha_discovery::...`) keep resolving
// unchanged: this crate was `vigil`'s `ha_discovery` module before the
// adapter split, so aliasing the new crate under the old name keeps every
// path below byte-identical.
use vigil_ha as vigil;

use vigil::ha_discovery::{
    CameraConfig, DiscoveryPayload, ServiceConfig, generate_discovery_payloads,
};

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/contract-goldens/ha_discovery_id_contract.golden"
);

const FIXED_SERVICE_NAME: &str = "Golden Vigil";
const FIXED_SERVICE_ID: &str = "vigil-golden-site";
const FIXED_CAMERA_ONE_ID: &str = "golden-cam-alpha";
const FIXED_CAMERA_ONE_LABEL: &str = "Golden Alpha";
const FIXED_CAMERA_TWO_ID: &str = "golden-cam-beta";
const FIXED_CAMERA_TWO_LABEL: &str = "Golden Beta";

fn fixed_two_camera_config() -> ServiceConfig {
    ServiceConfig {
        service_name: FIXED_SERVICE_NAME.to_string(),
        service_id: FIXED_SERVICE_ID.to_string(),
        cameras: vec![
            CameraConfig {
                camera_id: FIXED_CAMERA_ONE_ID.to_string(),
                camera_label: FIXED_CAMERA_ONE_LABEL.to_string(),
            },
            CameraConfig {
                camera_id: FIXED_CAMERA_TWO_ID.to_string(),
                camera_label: FIXED_CAMERA_TWO_LABEL.to_string(),
            },
        ],
    }
}

fn str_field(payload: &DiscoveryPayload, field: &str) -> String {
    payload
        .payload
        .get(field)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            panic!(
                "payload on topic '{}' is missing field '{field}'",
                payload.topic
            )
        })
}

fn device_identifier(payload: &DiscoveryPayload) -> String {
    payload
        .payload
        .get("device")
        .and_then(|device| device.get("identifiers"))
        .and_then(|identifiers| identifiers.get(0))
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            panic!(
                "payload on topic '{}' is missing device.identifiers[0]",
                payload.topic
            )
        })
}

fn device_via(payload: &DiscoveryPayload) -> String {
    payload
        .payload
        .get("device")
        .and_then(|device| device.get("via_device"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| "-".to_string())
}

/// Renders the currently-published discovery identity surface by calling
/// the real production function with fixed inputs — this is the "current
/// truth" the golden fixture pins.
fn render_current_identity_surface() -> Vec<String> {
    let payloads = generate_discovery_payloads(&fixed_two_camera_config());
    let mut lines = Vec::new();
    for payload in &payloads {
        let component = str_field(payload, "component");
        let default_entity_id = str_field(payload, "default_entity_id");
        let unique_id = str_field(payload, "unique_id");
        let identifier = device_identifier(payload);
        let via = device_via(payload);
        lines.push(format!(
            "config_topic={} | component={component} | default_entity_id={default_entity_id} | \
             unique_id={unique_id} | device_identifier={identifier} | via_device={via}",
            payload.topic
        ));
    }
    lines.sort();
    lines
}

fn read_golden() -> String {
    std::fs::read_to_string(GOLDEN)
        .unwrap_or_else(|error| panic!("read golden fixture {GOLDEN}: {error}"))
}

#[test]
fn published_discovery_identity_surface_matches_frozen_registry() {
    let rendered = render_current_identity_surface().join("\n") + "\n";
    let frozen = read_golden();
    assert_eq!(
        rendered, frozen,
        "the published Home Assistant discovery identity surface (config topics, \
         default_entity_id, unique_id, and device identifiers) no longer matches the checked-in \
         registry at {GOLDEN}. Home Assistant keys its entity registry, dashboards, and history \
         on these values, so changing one is a deliberate, reviewed transition to a published \
         identifier, not a side effect of a refactor — a rename here orphans an installed \
         user's existing entity. If this rename is intentional, update the golden fixture as \
         part of a reviewed change; otherwise restore the original identifier."
    );
}
