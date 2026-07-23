# Concepts

This page names the concepts that exist in the current Vigil developer preview. A concept marked **unavailable** is part of the product direction, not a feature you can configure today.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This is a legend for reading concept availability, not runtime behavior.` -->

## Site

Runtime startup creates one local Context Graph context with the configured `site_name`, and that
named context survives a store reopen.

<!-- vigil-claim: `vigil.docs-concepts.a-site-is-one-vigil-installation-and` -->
<!-- enforced by: `vigil-bin::first_light_loop::site_context_created_and_name_retained` -->

`site_name` defaults to `site-1`; the site is intended to group cameras, detector configuration,
detections, evidence, corrections, and optional recognition references in one local store. The
mapped startup test does not bind that default or the complete grouping inventory.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The site-context persistence witness does not prove the site_name default or every record type described as site-local.` -->

## Camera

Vigil stores the configured camera name as a site device entity and carries that name into event
provenance and clip naming. Configuration preserves separate detection and Home Assistant live-view
RTSP URLs.

<!-- vigil-claim: `vigil.docs-concepts.a-camera-is-a-named-rtsp-source` -->
<!-- enforced by: `vigil-bin::first_light_loop::configured_camera_name_drives_provenance_and_clip_prefix` -->
<!-- enforced by: `vigil::config::tests::live_rtsp_url_is_distinct_from_detection_rtsp_url` -->

MQTT topics and Home Assistant device names are also intended to derive from the configured camera
identity, but the mapped camera witnesses do not exercise those Home Assistant surfaces.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The camera entity and URL tests do not prove MQTT topic or Home Assistant device-name derivation.` -->

## Stream

The deterministic RTSP fixtures recover after a transient source loss and land no event for empty
or undecodable input.

<!-- vigil-claim: `vigil.docs-concepts.a-stream-is-the-rtsp-media-vigil` -->
<!-- enforced by: `vigil-bin::first_light_loop::empty_or_undecodable_stream_lands_no_event` -->
<!-- enforced by: `vigil-bin::first_light_loop::transient_rtsp_drop_recovers_without_losing_camera` -->

The repository's live fixture currently uses H.264. An H.265 decoder path has owner-live evidence,
but no deterministic repository H.265 fixture makes that evidence repeatable in CI.

<!-- vigil-unenforced: classification=external-procedure; reason=`H.265 has owner-live evidence but no checked-in deterministic fixture that can repeat the decoder proof.` -->

## Detector

Vigil executes the configured YOLOX object detector through Burn on decoded frames and records its
model identity and confidence threshold in a detector-config Decision.

<!-- vigil-claim: `vigil.docs-concepts.the-detector-is-the-configured-yolox-objectdetection` -->
<!-- enforced by: `vigil-bin::first_light_loop::detector_loads_and_runs_over_real_frames` -->
<!-- enforced by: `vigil-bin::first_light_loop::detector_config_recorded_as_decision` -->

Motion gating and segment sampling precede that detector in the intended pipeline. Hardware
detection belongs to compatible native artifacts after a successful probe, while the portable
artifact is intended to use CPU detection; these pipeline-order and artifact clauses need separate
direct witnesses.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The mapped detector execution and Decision tests do not bind motion-gate ordering, sampling, or native-versus-portable artifact composition.` -->

## Zone

The add-on configuration, Home Assistant event payload, and review HTTP event rows expose no
`zones`, `masks`, or placeholder `zone` field.

<!-- vigil-claim: `vigil.docs-concepts.zone-is-unavailable-and-not-exposed` -->
<!-- enforced by: `vigil::source_scan_contract::addon_config_exposes_recognition_options_and_schema` -->
<!-- enforced by: `vigil-ha::ha_discovery::tests::detection_event_payload_carries_current_contract_without_future_zone_field` -->
<!-- enforced by: `vigil::http_data_plane::event_list_serves_full_review_row_fieldset` -->

Polygon-zone runtime matching is future work, so automations must not depend on zone values yet.

<!-- vigil-unenforced: classification=future-surface; reason=`Polygon-zone runtime matching is not implemented and therefore has no direct deterministic runtime witness.` -->

## Mask

**Unavailable.** The current configuration and detection path do not provide privacy masks or motion masks.

<!-- vigil-unenforced: classification=future-surface; reason=`Privacy masks and motion masks have no configuration or detection implementation.` -->

## Recording

The current recording unit is an event clip, not continuous NVR recording. Vigil finalizes the clip before writing an Observation that refers to it; if clip persistence fails, it writes no dangling event. The review data plane can stream the clip, including HTTP byte ranges used by video players.

<!-- vigil-claim: `vigil.docs-concepts.the-current-recording-unit-is-an-event` -->
<!-- enforced by: `vigil-bin::first_light_loop::observation_never_references_undurable_clip` -->
<!-- enforced by: `vigil::http_data_plane::clip_read_serves_range_206_partial_content_transport` -->

## Event

A landed event is a durable detection Observation with event time, camera, detected class,
confidence, bounding box, sampled frame index, local clip reference, and detector-image reference.
The review HTTP event list exposes the stored row fields.

<!-- vigil-claim: `vigil.docs-concepts.an-event-is-a-durable-detection-observation` -->
<!-- enforced by: `vigil-bin::first_light_loop::detection_produces_observation_referencing_clip` -->
<!-- enforced by: `vigil::http_data_plane::event_list_serves_full_review_row_fieldset` -->

The CLI and Home Assistant are also intended to expose views of the same stored event, but this
mapping does not directly compare all three surfaces or bind the detector-config Decision field.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No mapped cross-surface test compares CLI, HTTP, and Home Assistant views of one event and its detector-config Decision.` -->

## Rule

**Unavailable.** Vigil currently records a generated baseline watch Intention for each camera, but it does not expose user-authored rules, schedules, zone/object matching, or rule lifecycle controls. See [Rules](rules.md).

<!-- vigil-unenforced: classification=future-surface; reason=`User rules, schedules, matching, and lifecycle controls are not implemented.` -->

## Actionbinding

**Unavailable.** There is no current `ActionBinding` configuration or Vigil action runner. Home Assistant automations can consume detection events, but those automations are owned and executed by Home Assistant.

<!-- vigil-unenforced: classification=future-surface; reason=`ActionBinding configuration and a Vigil action runner do not exist.` -->

## Observation

A correction is written as a durable Context Graph Observation, carries its anchored detection
Observation ID, and survives closing and reopening the store.

<!-- vigil-claim: `vigil.docs-concepts.an-observation-is-the-durable-context-graph` -->
<!-- enforced by: `vigil::ha_correction_seam::correction_writes_durably_through_cg_record_path` -->

Vigil also uses Observation rows for detections and recognition sightings, but the correction-path
witness mapped here does not enforce that complete Observation-type inventory.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The mapped correction test does not prove that every detection and recognition sighting is represented by the stated Observation taxonomy.` -->

## Intention

The current runtime creates a baseline watch Intention that `vigil why` walks together with the
detection's Decision, camera, site, and evidence.

<!-- vigil-claim: `vigil.docs-concepts.an-intention-records-why-a-decision-exists` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_why_walks_observation_to_clip_to_decision_to_context` -->

Intentions are generated provenance rather than a current user-authored surface; the mapped why
walk does not itself prove that no create or edit path exists elsewhere.

<!-- vigil-unenforced: classification=product-decision; reason=`User-authored Intention creation and editing are outside the current Vigil product surface and lack an executable absence proof.` -->

## Decision

A Decision is the detector configuration serving the baseline watch Intention. Current decisions record the model ID and confidence threshold and are linked to the camera and site. When configuration changes, a later event's `why` view uses the decision that existed for that event rather than substituting the newest settings.

<!-- vigil-claim: `vigil.docs-concepts.a-decision-is-the-detector-configuration-serving` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_why_reports_config_as_of_event_time_not_current` -->

## Outcome

False-alarm and wrong-class corrections are stored and read back as typed correction Observations,
not relabelled as Outcomes in these docs.

<!-- vigil-claim: `vigil.docs-concepts.unavailable-as-a-vigil-product-surface-context` -->
<!-- enforced by: `vigil::ha_correction_seam::false_alarm_and_wrong_class_corrections_record_and_read_back` -->

Context Graph supports Outcome rows, while the broader Vigil correction inventory also includes
`Identity` and `Enroll`; this mapping does not bind that Context Graph capability or the complete
correction-type taxonomy.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The mapped test covers FalseAlarm and WrongClass observations only, not Context Graph Outcome support or every correction type.` -->

## Invalidation

**Unavailable in the Vigil runtime.** Context Graph has invalidation machinery, but Vigil does not currently evaluate observations against user rules, flip decisions, or notify Home Assistant that a rule basis broke.

<!-- vigil-unenforced: classification=future-surface; reason=`Vigil has no rule evaluation, decision invalidation, or HA notification loop.` -->

## Evidenceref

The detection provenance returned by `GET /why/{id}` carries video and detector-image evidence
references while omitting the camera RTSP URL.

<!-- vigil-claim: `vigil.docs-concepts.an-evidenceref-is-the-typed-link-from` -->
<!-- enforced by: `vigil::http_data_plane::why_walk_back_serves_provenance_excludes_rtsp_url_and_lists_correction` -->

Evidence references are intended to retain producer and capture metadata and translate into local
`/media/...` routes. The mapped why test does not bind that complete metadata and routing contract.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The why-response witness does not directly enforce complete EvidenceRef producer/capture metadata and media-route translation.` -->

## Sitememory

The site-local recognition seam can enroll a named subject from a detection, match a later same-site
sighting, keep a below-threshold sighting unknown, and remove the subject's references so later
sightings become unknown again.

<!-- vigil-claim: `vigil.docs-concepts.site-memory-is-the-optional-recognition-library` -->
<!-- enforced by: `vigil::recognition_slice::enroll_correction_creates_named_entity_and_later_sightings_match` -->
<!-- enforced by: `vigil::recognition_slice::below_threshold_sighting_stays_unknown` -->
<!-- enforced by: `vigil::recognition_slice::forget_removes_references_and_the_subject_reverts_to_unknown` -->

Operator-staged weights enable production recognition, and review surfaces are intended to show
match provenance. The mapped storage-and-matching tests use a deterministic embedder and do not bind
weight delivery or the rendered provenance surface.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The deterministic recognition seam tests do not prove production weight delivery or rendered match-provenance output.` -->
