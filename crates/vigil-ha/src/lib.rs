//! The Home Assistant MQTT integration adapter for Vigil.
//!
//! This crate implements Vigil's [`vigil::SiteChannelFactory`] seam: it
//! translates Vigil's own resolved site facts into Home Assistant MQTT
//! discovery, publishes detection facts as MQTT events, and forwards owner
//! MQTT commands back in through the same product action (`record_correction`)
//! Vigil's own surfaces use. Vigil's core crate has no knowledge of this
//! crate; a composition root wires the two together.

// Kept `pub` (not just re-exported flat) so pre-move qualified paths
// (`vigil::ha_discovery::X`, `vigil::ha_mqtt_tasks::Y`) keep resolving
// unchanged from the moved test suites via a `use vigil_ha as vigil;` alias.
pub mod ha_discovery;
pub mod ha_mqtt_tasks;

pub use ha_discovery::{
    CameraConfig, CommandTopicMessage, DetectionInput, DiscoveryPayload, EventPayload, ParseError,
    ServiceConfig, generate_discovery_payloads, map_detection_to_event_payload,
    parse_command_topic, running_condition_topic, service_availability_topic,
};
pub use ha_mqtt_tasks::{
    CONTROL_COMMAND_TOPIC, CORRECTION_COMMAND_TOPIC, DetectionPublisher, DetectionPublisherHandle,
    HOME_ASSISTANT_STATUS_TOPIC, MqttConfig, SubscriberHandle, WiredSubscriberConfig,
    enabled_state_topic, mqtt_connect_intent, publish_availability_online, publish_detection_event,
    publish_discovery_to_broker, snapshot_topic, spawn_detection_publisher,
    spawn_production_subscriber, spawn_site_presence,
};

use std::sync::{Arc, Mutex};

use vigil::{
    CommandListener, ConnectionEndpoint, DetectionChannel, DetectionFact, HealthState,
    SiteAnnouncement, SiteChannelFactory, SiteControl, SitePresence,
};

/// The production Home Assistant MQTT adapter. Implements Vigil's
/// [`SiteChannelFactory`] seam; a composition root supplies one instance of
/// this to Vigil's runtime.
#[derive(Debug, Default, Clone, Copy)]
pub struct HomeAssistantMqtt;

fn mqtt_config_from_endpoint(endpoint: &ConnectionEndpoint) -> MqttConfig {
    MqttConfig {
        broker_host: endpoint.host.clone(),
        broker_port: endpoint.port,
        username: endpoint.username.clone(),
        // Cloned through with no exposure here; `ha_mqtt_tasks::apply_credentials`
        // is the one place that reads the real value, at the point it is
        // actually handed to the MQTT client.
        password: endpoint.password.clone(),
    }
}

/// Derive a stable lowercase slug for use as a stable MQTT client id.
fn slug_for_id(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

struct MqttDetectionChannel {
    publisher: Arc<DetectionPublisher>,
    handle: Mutex<Option<DetectionPublisherHandle>>,
    service_id: String,
}

impl DetectionChannel for MqttDetectionChannel {
    fn publish_detection(&self, event: DetectionFact) {
        let camera_id = event.camera_id.clone();
        let input = DetectionInput {
            observation_id: event.observation_id,
            camera_name: event.camera_name,
            object_class: event.object_class,
            confidence: event.confidence,
            timestamp_ms: event.timestamp_ms,
            evidence_ref: event.evidence_ref,
            snapshot_ref: event.snapshot_ref,
            entity_name: event.entity_name,
            match_score: event.match_score,
        };
        let topic = format!("vigil/{}/{}/detection", self.service_id, camera_id);
        let evt = map_detection_to_event_payload(&input);
        match serde_json::to_string(&evt) {
            Ok(json) => {
                self.publisher.try_publish(topic, json);
                // Feed the per-camera motion binary_sensor retained state.
                let active_topic = format!("vigil/{}/{}/active", self.service_id, camera_id);
                self.publisher.notify_active(active_topic);
                println!("mqtt_detection_published=true");
            }
            Err(e) => println!("mqtt_detection_serialize_error={e}"),
        }
    }

    fn shutdown_and_join(&self) {
        if let Some(handle) = self.handle.lock().expect("detection channel lock").take() {
            handle.shutdown_and_join();
        }
    }
}

impl CommandListener for SubscriberHandle {
    fn shutdown_and_join(self: Box<Self>) {
        SubscriberHandle::shutdown_and_join(*self);
    }
}

impl SitePresence for SubscriberHandle {
    fn shutdown_and_join(self: Box<Self>) {
        SubscriberHandle::shutdown_and_join(*self);
    }
}

impl SiteChannelFactory for HomeAssistantMqtt {
    fn connect(
        &self,
        endpoint: &ConnectionEndpoint,
        service_id: &str,
        health: HealthState,
    ) -> Option<Arc<dyn DetectionChannel>> {
        let mqtt_cfg = mqtt_config_from_endpoint(endpoint);
        let (publisher, handle) = spawn_detection_publisher(&mqtt_cfg, health);
        println!("mqtt_detection_publisher_started=true");
        Some(Arc::new(MqttDetectionChannel {
            publisher,
            handle: Mutex::new(Some(handle)),
            service_id: service_id.to_string(),
        }))
    }

    fn listen(
        &self,
        endpoint: &ConnectionEndpoint,
        site: SiteAnnouncement,
        health: HealthState,
        control: Arc<dyn SiteControl>,
    ) -> Option<Box<dyn CommandListener>> {
        let sub_cfg = announce_site(endpoint, site, health);
        let overflow = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let subscriber = spawn_production_subscriber(sub_cfg, control, overflow);
        println!("mqtt_subscriber_started=true");
        Some(Box::new(subscriber))
    }

    fn announce(
        &self,
        endpoint: &ConnectionEndpoint,
        site: SiteAnnouncement,
        health: HealthState,
    ) -> Option<Box<dyn SitePresence>> {
        let sub_cfg = announce_site(endpoint, site, health);
        let presence = spawn_site_presence(sub_cfg);
        println!("mqtt_presence_started=true");
        Some(Box::new(presence))
    }
}

/// Register this site's entities with Home Assistant, mark it available, and
/// build the long-lived connection's configuration.
///
/// The whole publish side lives here rather than inside the listening half, so
/// a run that must not listen still registers its entities, still marks itself
/// available, still reports the condition it is in, and still leaves a last
/// will behind. Home Assistant learns about a Vigil node from exactly one
/// place, whichever half of the adapter a run brings up.
fn announce_site(
    endpoint: &ConnectionEndpoint,
    site: SiteAnnouncement,
    health: HealthState,
) -> WiredSubscriberConfig {
    let mqtt_cfg = mqtt_config_from_endpoint(endpoint);
    let svc = ServiceConfig {
        service_name: site.service_name,
        service_id: site.service_id.clone(),
        cameras: site
            .cameras
            .into_iter()
            .map(|camera| CameraConfig {
                camera_id: camera.id,
                camera_label: camera.label,
            })
            .collect(),
    };
    let payloads = generate_discovery_payloads(&svc);
    match publish_discovery_to_broker(&mqtt_cfg, &payloads) {
        Ok(()) => println!("mqtt_discovery_published=true"),
        Err(e) => println!("mqtt_discovery_error={e}"),
    }
    let avail_topic = service_availability_topic(&site.service_id);
    match publish_availability_online(&mqtt_cfg, &avail_topic) {
        Ok(()) => println!("mqtt_availability_online=true"),
        Err(e) => println!("mqtt_availability_error={e}"),
    }
    let condition_topic = running_condition_topic(&site.service_id);
    // Derive a stable client id from the service_id so the broker can
    // correlate last-will across restarts.
    let client_id = format!("vigil-{}-sub", slug_for_id(&site.service_id));
    WiredSubscriberConfig {
        mqtt: mqtt_cfg,
        service_id: site.service_id,
        client_id,
        availability_topic: avail_topic,
        condition_topic,
        discovery_payloads: payloads,
        health,
    }
}
