# Rules

## Current availability

User-authored Vigil rules are **not available in the current developer preview**. There is no `rules:` configuration block, schedule evaluator, zone-and-object matcher, `ActionBinding` model, rule pause command, or rule action runner in the current binary.

<!-- vigil-unenforced: classification=future-surface; reason=`User rules, schedules, matching, actions, and pause controls are not implemented.` -->

Runtime startup creates a baseline watch Intention and a detector-config Decision linked to the
camera and site.

<!-- vigil-claim: `vigil.docs-rules.vigil-does-create-a-baseline-watch-intention` -->
<!-- enforced by: `vigil::first_light_loop::detector_config_recorded_as_decision` -->

Those records are generated provenance rather than an operator-authored rule engine; user rule
creation and lifecycle controls remain outside the current product surface.

<!-- vigil-unenforced: classification=product-decision; reason=`Operator-authored rules and lifecycle controls are outside the current Vigil product surface and lack an executable absence proof.` -->

## What you can automate today

Home Assistant receives a `vigil_detection` event entity for each configured camera. Its event payload includes the detection ID, camera, object class, confidence, timestamp, evidence and snapshot references, and optional recognition name and score. A `zone` field is not exposed until zone configuration and runtime matching ship.

<!-- vigil-claim: `vigil.docs-rules.home-assistant-receives-a-vigildetection-event-entity` -->
<!-- enforced by: `vigil::ha_discovery::tests::detection_event_payload_carries_current_contract_without_future_zone_field` -->

Build present-day reactions as Home Assistant automations against that event. Home Assistant owns the condition and action: Vigil does not persist that automation as a rule, execute its action, or explain why Home Assistant chose to run it.

<!-- vigil-unenforced: classification=product-decision; reason=`Home Assistant owns present-day automation conditions/actions rather than Vigil persistence.` -->

## Camera controls are not rules

Each camera has an Enabled switch and Snapshot Trigger button in Home Assistant. Disabling one camera stops that camera without changing another camera's enabled state; the state is retained and restored through the MQTT control plane. These are direct controls, not scheduled or conditional rules.

<!-- vigil-claim: `vigil.docs-rules.each-camera-has-an-enabled-switch-and` -->
<!-- enforced by: `vigil::ha_mqtt_broker::operator_action_command_effects_action` -->
<!-- enforced by: `vigil::ha_mqtt_broker::camera_enabled_switch_state_is_retained_and_updates_on_control_commands` -->

## Corrections are not rules

Confirming a detection, marking a false alarm, or correcting its class writes a durable typed
correction anchored to that detection. Enrolling a subject creates a site-local recognition
reference that can match a later sighting.

<!-- vigil-claim: `vigil.docs-rules.confirming-a-detection-marking-a-false-alarm` -->
<!-- enforced by: `vigil::ha_correction_seam::correction_writes_durably_through_cg_record_path` -->
<!-- enforced by: `vigil::ha_correction_seam::false_alarm_and_wrong_class_corrections_record_and_read_back` -->
<!-- enforced by: `vigil::recognition_slice::enroll_correction_creates_named_entity_and_later_sightings_match` -->

Corrections do not currently rewrite detector thresholds or synthesize standing rules; automatic
learning from those audit facts is future product work.

<!-- vigil-unenforced: classification=future-surface; reason=`Automatic threshold updates and standing-rule synthesis from corrections are not implemented.` -->

## Planned rule surface

The product direction includes local watch rules with schedules, object and zone matching, action bindings, pause/disable controls, and invalidation when a rule's basis breaks. None of those promises should be used to configure or evaluate the current developer preview. This section will be replaced by concrete syntax only after the rule implementation and its acceptance tests ship.

<!-- vigil-unenforced: classification=future-surface; reason=`Local rule syntax and acceptance remain deferred until the rule implementation ships.` -->
