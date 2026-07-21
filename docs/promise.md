# What Vigil promises

This page separates code that works in the developer preview from the contract for the first public
OSS release. A release-target section is not a claim that the behavior ships today.
<!-- vigil-unenforced: classification=non-contract-context; reason=`This is the page legend separating shipped behavior from future release targets.` -->

## Available in the developer preview

### One local process owns runtime state

`vigil run` opens the local Context Graph store before reporting ready, keeps ownership of that
store for the process lifetime, serves liveness through `/health`, and releases the store on clean
shutdown. A first start also works without outbound network access.
<!-- vigil-claim: `vigil.docs-promise.vigil-run-opens-the-local-context-graph` -->
<!-- enforced by: `vigil-acceptance::acceptance::installable_substrate::vigil_standalone_first_start_opens_local_context_graph_store` -->
<!-- enforced by: `vigil-acceptance::acceptance::installable_substrate::vigil_standalone_health_200_requires_store_open_and_runtime_loop` -->
<!-- enforced by: `vigil-acceptance::acceptance::installable_substrate::vigil_standalone_sigterm_exits_zero_and_releases_store` -->

The repository contains Home Assistant add-on and container artifact definitions for amd64 and
aarch64.
<!-- vigil-claim: `vigil.docs-promise.the-repository-contains-home-assistant-addon-and` -->
<!-- enforced by: `vigil::runtime_packages_manifest::runtime_packages_manifest_has_amd64_and_aarch64_profiles_with_backends_and_packages` -->
<!-- enforced by: `vigil::runtime_packages_manifest::addon_dockerfile_installs_only_manifest_declared_packages` -->
<!-- enforced by: `vigil::artifact_profile_honesty::release_notes_name_the_hardware_generic_docker_artifact` -->

No public release channel is available yet.

<!-- vigil-unenforced: classification=external-procedure; reason=`No public add-on or container release channel currently publishes these artifact definitions.` -->

### Direct RTSP detection with local evidence

The current vertical slice connects to one configured RTSP source, decodes frames, invokes the
local detector, writes a detection, and records a locally decodable clip made from that stream.
The acceptance test also rejects unrelated outbound connections on this path.
<!-- vigil-claim: `vigil.docs-promise.the-current-vertical-slice-connects-to-one` -->
<!-- enforced by: `vigil-acceptance::acceptance::one_camera_acceptance::frigate_replacement_loop_runs_over_direct_rtsp_synthetic` -->

### Provenance on each current detection

The direct-RTSP path records the site context, camera entity, baseline watch intention, active
detector-configuration decision, detection observation, and evidence reference. `vigil why` walks
those exact stored identifiers rather than presenting a canned explanation.
<!-- vigil-claim: `vigil.docs-promise.the-directrtsp-path-records-the-site-context` -->
<!-- enforced by: `vigil-acceptance::acceptance::one_camera_acceptance::frigate_replacement_loop_runs_over_direct_rtsp_synthetic` -->

The HTTP `why` response carries the same detection-specific provenance and omits the camera RTSP
URL so stored credentials do not leak onto the review surface.
<!-- vigil-claim: `vigil.docs-promise.the-http-why-response-carries-the-same` -->
<!-- enforced by: `vigil::http_data_plane::why_walk_back_serves_provenance_excludes_rtsp_url_and_lists_correction` -->

### Local event and media review

The review service exposes detection rows, detection-specific `why` data, snapshot bytes, and clip
bytes. Clip reads support HTTP range requests, and a media reference returned by the event list
resolves back to the served local media.
<!-- vigil-claim: `vigil.docs-promise.the-review-service-exposes-detection-rows-detectionspecific` -->
<!-- enforced by: `vigil::http_data_plane::event_list_serves_full_review_row_fieldset` -->
<!-- enforced by: `vigil::http_data_plane::snapshot_read_serves_image_bytes_with_image_content_type` -->
<!-- enforced by: `vigil::http_data_plane::clip_read_serves_range_206_partial_content_transport` -->
<!-- enforced by: `vigil::http_data_plane::media_reference_from_event_list_resolves_to_media_bytes` -->

### Corrections are durable audit records

Identity, false-alarm, and wrong-class corrections are anchored to the named detection and read
back from the local store. A correction survives a daemon restart, and HTTP and MQTT both use the
same durable correction seam.
<!-- vigil-claim: `vigil.docs-promise.identity-falsealarm-and-wrongclass-corrections-are-anchored` -->
<!-- enforced by: `vigil::ha_correction_seam::false_alarm_and_wrong_class_corrections_record_and_read_back` -->
<!-- enforced by: `vigil::ha_correction_seam::correction_anchored_to_named_detection_only` -->
<!-- enforced by: `vigil::ha_correction_seam::correction_survives_daemon_restart` -->
<!-- enforced by: `vigil::http_data_plane::http_correction_post_lands_through_record_correction_seam` -->
<!-- enforced by: `vigil::ha_mqtt_broker::correction_command_on_broker_lands_in_cg` -->

Corrections do not yet form a general learning loop. Enrollment affects later recognition; false
alarms and wrong classes are currently memory and review facts, not automatic detector tuning or
standing-rule changes.
<!-- vigil-unenforced: classification=future-surface; reason=`Automatic detector tuning and standing-rule changes from corrections are not implemented.` -->

### Site-local enrollment and recognition seam

An enrollment correction creates a named entity and reference from a detection. The deterministic
storage and matching seam can match a later same-site sighting; below-threshold sightings stay
unknown, and forgetting the entity removes its references so a later sighting returns to unknown.
<!-- vigil-claim: `vigil.docs-promise.an-enrollment-correction-creates-a-named-entity` -->
<!-- enforced by: `vigil::recognition_slice::enroll_correction_creates_named_entity_and_later_sightings_match` -->
<!-- enforced by: `vigil::recognition_slice::below_threshold_sighting_stays_unknown` -->
<!-- enforced by: `vigil::recognition_slice::forget_removes_references_and_the_subject_reverts_to_unknown` -->

The event and `why` surfaces expose the server-authoritative name and match provenance.
<!-- vigil-claim: `vigil.docs-promise.the-event-and-why-surfaces-expose-the` -->
<!-- enforced by: `vigil::recognition_slice::http_event_rows_and_why_carry_recognition_for_the_card` -->

These tests inject a deterministic content-hash embedder. The preview does not deliver SigLIP
weights or provide a supported operator installation path for them, and repository CI does not
validate real-model identity quality. Recognition is therefore an implemented developer seam, not
yet a supported end-user workflow.

<!-- vigil-unenforced: classification=external-procedure; reason=`Real SigLIP weight delivery and identity-quality acceptance need physical model validation.` -->

### A Home Assistant control and review seam

Vigil publishes MQTT discovery for a service device and per-camera devices, publishes detection
events to a real broker, exposes a retained camera-enable switch and snapshot action, and consumes
correction commands from MQTT into the local store.
<!-- vigil-claim: `vigil.docs-promise.vigil-publishes-mqtt-discovery-for-a-service` -->
<!-- enforced by: `vigil::ha_discovery::tests::discovery_registers_device_and_per_camera_subdevice` -->
<!-- enforced by: `vigil::ha_mqtt_broker::detection_publishes_event_to_real_broker` -->
<!-- enforced by: `vigil::ha_mqtt_broker::camera_enabled_switch_state_is_retained_and_updates_on_control_commands` -->
<!-- enforced by: `vigil::ha_mqtt_broker::operator_action_command_effects_action` -->
<!-- enforced by: `vigil::ha_mqtt_broker::correction_command_on_broker_lands_in_cg` -->

The complete rich review path also depends on a separate Vigil Home Assistant integration and a
Vigil engine in an Advanced Camera Card fork. Those companion repositories are not yet distributed
as one public Vigil release.
<!-- vigil-unenforced: classification=external-procedure; reason=`Companion integration and card repositories are not yet distributed as one public release.` -->

### Acceleration reports achieved state

Hardware decoding and accelerated detection are independent intents. When enabled, Vigil probes
for a usable backend; when a backend cannot run, software/CPU continues and health and stats name
the fallback. Explicitly disabling acceleration remains distinguishable from a failed acceleration
attempt.
<!-- vigil-claim: `vigil.docs-promise.hardware-decoding-and-accelerated-detection-are-independent` -->
<!-- enforced by: `vigil::config_acceleration_intent::toml_config_can_disable_each_boolean_independently` -->
<!-- enforced by: `vigil::decode_backend_contract::probe_reports_decoded_frames_and_classification` -->
<!-- enforced by: `vigil::detection_accel_backend::detection_accel_compiled_in_receipt_agrees_with_recorded_probe` -->
<!-- enforced by: `vigil::acceleration_receipts::cpu_only_artifact_with_accel_true_reports_backend_not_compiled_fallback` -->
<!-- enforced by: `vigil::acceleration_receipts::accel_false_is_disabled_not_fallback_and_they_are_distinct` -->
<!-- enforced by: `vigil::acceleration_receipts::health_reports_degraded_decode_and_detection_when_configured_but_fallback` -->

## Target for the first public OSS release

### HA-native install

The release target is one Vigil runtime packaged as a Home Assistant add-on, container, or local
service, with a published install path and a clean-clone build. The current repository artifacts do
not yet satisfy that public-install contract.

<!-- vigil-unenforced: classification=external-procedure; reason=`Published add-on/container/service installs and clean-clone builds need release infrastructure.` -->

### Frigate replacement, not companion

The release target covers the practical replacement surface: heterogeneous camera ingest,
multi-camera detection, local recording and review, zones and masks, explicit retention, HA
events and controls, and migration guidance. Frigate may coexist during migration, but it is not a
runtime dependency.
<!-- vigil-unenforced: classification=product-decision; reason=`The practical Frigate-replacement surface and no-runtime-dependency rule define release scope.` -->

### Local media with explicit retention

The release target keeps raw video, clips, snapshots, and bulky recognition evidence on the site.
It applies one space budget with explicit retention defaults and per-camera overrides, makes
deletion behavior visible, and keeps compact memory separately from prunable evidence.

<!-- vigil-unenforced: classification=product-decision; reason=`Site-local media, retention defaults, budget overrides, and visible deletion are release choices.` -->

Today, detection clips are local, but continuous recording, the retention sweeper, disk-budget
rebalancing, snapshot content controls, and irreversible post-retention forgetting are not
available.
<!-- vigil-unenforced: classification=implementation-blocker; reason=`Continuous recording, retention, disk budgeting, and irreversible forgetting remain unavailable.` -->

### Assured events and visible coverage loss

The release target delivers HA events through an assured path and makes queue overflow, skipped
analysis, stale streams, and replaced work visible. “No event” must be distinguishable from “Vigil
could not verify the scene.” The current MQTT surface does not yet satisfy that complete delivery
and loss-accounting contract.

<!-- vigil-unenforced: classification=product-decision; reason=`Assured HA delivery and explicit coverage-loss reporting are approved release contracts.` -->

### Complete explanations, replay, and invalidation

The current `why` walk is real but thin. The release target expands it across the complete event,
rule, action, correction, and recognition surface. Historical state replay and automatic
invalidation when a decision's basis breaks are not available in the current preview.

<!-- vigil-unenforced: classification=future-surface; reason=`Complete explanations, historical replay, and invalidation are not implemented.` -->

### Single-site OSS

The release target is a complete self-hosted Vigil at one site. A person may run an independent
local Vigil at each property, but hosted multi-site rollup, fleet operations, and cross-customer
learning are Vigil Enterprise capabilities rather than OSS dependencies.
<!-- vigil-unenforced: classification=product-decision; reason=`Single-site OSS and hosted multi-site Enterprise ownership define the product boundary.` -->

### Deployment-owned access control

Vigil does not add its own login. The release contract relies on Home Assistant Ingress, an
operator-provided ingress or proxy, or an explicitly trusted network boundary. The review UI and
API become same-origin, bind addresses become explicit configuration, and corrections retain
honest channel and author provenance. That hardening is not complete in the current preview.

<!-- vigil-unenforced: classification=product-decision; reason=`Deployment-owned authentication, same-origin UI, explicit binds, and author provenance are approved.` -->
