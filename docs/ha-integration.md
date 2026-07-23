# Home Assistant integration

> **Developer preview:** no public Home Assistant add-on repository, companion-integration
> package, or published container image exists yet. This page documents implemented control and
> review seams for contributors; it is not a supported installation guide.

<!-- vigil-unenforced: classification=external-procedure; reason=`Public add-on, companion package, and container publication are still absent.` -->

Vigil's current Home Assistant control plane uses MQTT discovery. Live video is registered
separately as a Home Assistant Generic Camera, and Vigil serves review data through its local HTTP
data plane. A companion integration and review card are planned but are not currently shipped.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No single integration contract binds this complete MQTT, Generic Camera, and review-plane overview.` -->

## Prerequisites

Install and run the Mosquitto broker add-on if you want Vigil entities and events in Home
Assistant. When Vigil runs as an add-on, it asks the Supervisor services API for MQTT connection
details. Outside the Supervisor, configure the broker with `MQTT_HOST`, `MQTT_PORT`, `MQTT_USER`,
and `MQTT_PASSWORD`.

<!-- vigil-unenforced: classification=external-procedure; reason=`Broker installation and Supervisor service discovery require a real Home Assistant deployment procedure.` -->

With no broker configured, Vigil makes no MQTT connection.

<!-- vigil-claim: `vigil.docs-ha-integration.install-and-run-the-mosquitto-broker-addon` -->
<!-- enforced by: `vigil-ha::ha_mqtt_broker::mqtt_gated_off_when_no_broker_configured` -->

## Devices and entities

MQTT discovery creates one parent Vigil device with a **Running Condition** sensor. Each configured camera appears as a child device with exactly these five MQTT entities:

<!-- vigil-unenforced: classification=documentation-gap; reason=`The exact five-entity inventory is not bound at this introductory paragraph.` -->

- **Detection** — an event entity for `vigil_detection` messages.
- **Snapshot** — an image entity showing the latest published detector image.
- **Active** — a motion-class binary sensor set to `ON` when a detection is published.
- **Enabled** — a stateful switch that enables or disables that camera.
- **Snapshot Trigger** — a button that republishes the latest available detector image.

<!-- vigil-claim: `vigil.docs-ha-integration.detection-an-event-entity-for-vigildetection-messages` -->
<!-- enforced by: `vigil-ha::ha_discovery::tests::discovery_registers_device_and_per_camera_subdevice` -->
<!-- enforced by: `vigil-ha::ha_discovery::tests::per_camera_entity_names_are_device_local_and_enabled_is_stateful_switch` -->

Entity IDs and discovery topics derive from configured service and camera IDs, so regenerating
discovery from the same configuration keeps the same identities. Every generated entity carries
Vigil's availability topic.

<!-- vigil-claim: `vigil.docs-ha-integration.entity-ids-and-discovery-topics-are-derived` -->
<!-- enforced by: `vigil-ha::ha_discovery::tests::entity_ids_and_topics_stable_across_regeneration` -->
<!-- enforced by: `vigil-ha::ha_discovery::tests::discovery_entities_carry_required_ha_fields` -->

An unclean MQTT disconnect is intended to make the broker publish `offline` through the last will;
the mapped discovery-generation tests do not exercise a real unclean disconnect.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No mapped broker test forces an unclean disconnect and observes the availability last-will payload.` -->

## Live view

Vigil discovery creates no MQTT camera entity. In the add-on path, Vigil uses the Supervisor
config-flow API to create a Generic Camera entry, passes `live_rtsp_url` when configured, and
otherwise falls back to the detection `rtsp_url`.

<!-- vigil-claim: `vigil.docs-ha-integration.vigil-does-not-create-an-mqtt-camera` -->
<!-- enforced by: `vigil-ha::ha_discovery::tests::discovery_registers_device_and_per_camera_subdevice` -->
<!-- enforced by: `vigil::runtime::tests::generic_camera_url_prefers_live_rtsp_url_over_detection_rtsp_url` -->
<!-- enforced by: `vigil::runtime::tests::generic_camera_url_falls_back_to_detection_rtsp_url_for_single_stream_cameras` -->
<!-- enforced by: `vigil::source_scan_contract::generic_camera_registration_confirms_home_assistant_preview_step` -->
<!-- enforced by: `vigil::source_scan_contract::generic_camera_registration_logs_validation_errors_and_deletes_failed_flows` -->

The product reason for this split is that Home Assistant's MQTT camera platform is image-oriented
and does not represent the configured RTSP live stream; that external platform constraint is not a
Vigil repository invariant.

<!-- vigil-unenforced: classification=external-procedure; reason=`Home Assistant's MQTT camera capability is an external platform fact rather than a Vigil repository contract.` -->

Generic Camera registration requires the add-on's Supervisor token and Home Assistant API permission. A standalone Vigil process can still detect, persist, publish MQTT events, and serve review HTTP, but it cannot ask a Home Assistant Supervisor to create the Generic Camera entry.

<!-- vigil-unenforced: classification=external-procedure; reason=`Supervisor token and API permission require a real Home Assistant deployment procedure.` -->

## Detection events

Each event payload carries `event_type: vigil_detection`, the durable detection ID, camera name, object class, confidence, event timestamp, evidence and snapshot references, and optional recognition name and score. The current broker test proves delivery of this payload, but it does not prove production-path ordering between local persistence and MQTT publication.

<!-- vigil-claim: `vigil.docs-ha-integration.each-event-payload-carries-eventtype-vigildetection-the` -->
<!-- enforced by: `vigil-ha::ha_discovery::tests::detection_event_payload_carries_current_contract_without_future_zone_field` -->
<!-- enforced by: `vigil-ha::ha_mqtt_broker::detection_publisher_delivers_to_broker` -->

The current event payload carries no `zone` field. When a recognition sighting is supplied with a
matched subject, the event carries that server-authoritative name.

<!-- vigil-claim: `vigil.docs-ha-integration.the-current-runtime-does-not-configure-zones` -->
<!-- enforced by: `vigil-ha::ha_discovery::tests::detection_event_payload_carries_current_contract_without_future_zone_field` -->
<!-- enforced by: `vigil-ha::recognition_wire_shapes::event_payload_carries_the_entity_name_when_matched` -->

Recognition events are also intended to carry the match score and omit a name for unmatched
sightings, but the mapped event-payload witnesses do not assert those two branches.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The mapped recognition payload test asserts the matched name only, not score propagation or the unmatched-name branch.` -->

## Running condition and availability

The Running Condition sensor reports `running`, `store-open-failed`, `ingest-failed`, `disk-full`, or `keep-pace-failed`. Degraded CPU fallback remains alive for the Home Assistant watchdog; genuinely dead startup states answer non-2xx on the health endpoint.

<!-- vigil-claim: `vigil.docs-ha-integration.the-running-condition-sensor-reports-running-storeopenfailed` -->
<!-- enforced by: `vigil-ha::ha_discovery::tests::running_condition_maps_each_named_fault` -->
<!-- enforced by: `vigil-ha::ha_mqtt_broker::running_condition_tracks_live_health_retained` -->
<!-- enforced by: `vigil::health_watchdog_liveness::detector_behind_on_cpu_fallback_stays_alive_for_the_watchdog` -->
<!-- enforced by: `vigil::health_watchdog_liveness::genuinely_dead_states_answer_non_2xx` -->

Vigil republishes retained discovery, availability, running condition, and camera enabled state after broker reconnection and when Home Assistant announces that it has restarted. This lets entities return without rebooting Vigil.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No reconnect acceptance binds republishing after broker recovery and HA restart announcements.` -->

## Control a camera

Use the camera's **Enabled** switch to enable or disable processing. Vigil reflects the retained state in Home Assistant and writes a local disabled marker. The command targets one camera; it does not disable its siblings. Restart restoration is not yet covered by an end-to-end acceptance test, so the developer preview does not guarantee that behavior.

<!-- vigil-claim: `vigil.docs-ha-integration.use-the-cameras-enabled-switch-to-enable` -->
<!-- enforced by: `vigil-ha::ha_mqtt_broker::camera_enabled_switch_state_is_retained_and_updates_on_control_commands` -->
<!-- enforced by: `vigil::site_channel::tests::recognized_camera_id_still_writes_the_disabled_marker` -->

Press **Snapshot Trigger** to publish the latest stored detector evidence image for that camera. It does not force the RTSP pipeline to capture a new frame at button-press time.

<!-- vigil-claim: `vigil.docs-ha-integration.press-snapshot-trigger-to-publish-the-latest` -->
<!-- enforced by: `vigil-bin::mqtt_composed_product::snapshot_command_publishes_the_latest_detector_evidence_through_the_composed_binary` -->

## Review and correct events

Vigil's local review server listens on `review_port` (default `8098`) and exposes event rows, a
per-detection provenance view, snapshots and clips, and correction writes.

<!-- vigil-claim: `vigil.docs-ha-integration.vigils-local-review-server-listens-on-reviewport` -->
<!-- enforced by: `vigil::config::tests::review_port_cli_override_is_documented_and_loaded` -->
<!-- enforced by: `vigil::http_data_plane::data_plane_serves_in_process_then_stops_on_shutdown` -->
<!-- enforced by: `vigil::http_data_plane::event_list_serves_full_review_row_fieldset` -->
<!-- enforced by: `vigil::http_data_plane::why_walk_back_serves_provenance_excludes_rtsp_url_and_lists_correction` -->
<!-- enforced by: `vigil::http_data_plane::snapshot_read_serves_image_bytes_with_image_content_type` -->
<!-- enforced by: `vigil::http_data_plane::clip_read_serves_range_206_partial_content_transport` -->
<!-- enforced by: `vigil::http_data_plane::http_correction_post_lands_through_record_correction_seam` -->

A future companion Home Assistant integration is intended to proxy those surfaces through Home
Assistant authentication for a review card. It is not currently shipped and must not become a
second source of event truth.

<!-- vigil-unenforced: classification=external-procedure; reason=`The companion integration and review card are not currently distributed as a public Vigil surface.` -->

Corrections supported by the current server are Confirmed (`Identity`), Wrong class (`WrongClass` with a label), and False alarm (`FalseAlarm`). The MQTT command seam additionally accepts enrollment. Repeating an identical correction returns the existing durable correction instead of writing a duplicate.

<!-- vigil-claim: `vigil.docs-ha-integration.corrections-supported-by-the-current-server-are` -->
<!-- enforced by: `vigil::http_data_plane::http_correction_post_labelled_wrong_class_survives_to_why_and_is_idempotent` -->
<!-- enforced by: `vigil-bin::mqtt_composed_product::correction_commands_via_mqtt_land_in_cg_through_the_composed_binary` -->
<!-- enforced by: `vigil-ha::recognition_wire_shapes::enroll_wire_shape_parses_from_the_command_topic` -->
<!-- enforced by: `vigil::ha_correction_seam::confirmed_correction_does_not_set_correction_recorded` -->

## Automations

Use the per-camera Detection event entity as the trigger for Home Assistant automations, then inspect event fields such as object class, confidence, camera, and detection ID in your conditions and actions. Vigil does not currently ship rule schedules, action bindings, or Home Assistant services for pausing a rule. See [Rules](rules.md) for the exact boundary.

<!-- vigil-unenforced: classification=future-surface; reason=`Vigil rule schedules, action bindings, and pause services are not implemented.` -->

## Assist and voice

**Unavailable.** Vigil currently registers no Home Assistant intents, Assist sentences, or voice controls. The user-facing command surface is MQTT/HA controls, the local review API, and the CLI.

<!-- vigil-unenforced: classification=future-surface; reason=`Home Assistant intents, Assist sentences, and voice controls do not exist.` -->
