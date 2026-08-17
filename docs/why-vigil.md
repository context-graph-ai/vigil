# Why Vigil?

## Cameras report events but lose the reason

Conventional camera stacks are good at producing a clip, a label, and a timestamp. The hard
question comes later: what configuration was active, what was the camera supposed to watch for,
which evidence produced the alert, and what did the owner say after reviewing it?

<!-- vigil-unenforced: classification=non-contract-context; reason=`This is the motivating problem statement, not a current Vigil behavior promise.` -->

Vigil keeps those facts as one local chain. In the working direct-RTSP slice, a detection is linked
to its site, camera, watch intention, detector-configuration decision, and locally decodable clip;
the `why` command reads the chain back by event id.
<!-- vigil-claim: `vigil.docs-why-vigil.vigil-keeps-those-facts-as-one-local` -->
<!-- enforced by: `vigil-acceptance::acceptance::one_camera_acceptance::frigate_replacement_loop_runs_over_direct_rtsp_synthetic` -->

This is the first difference Vigil is designed around: an alert is not complete when the notifier
fires. It is complete when the evidence and the explanation remain reviewable after the moment has
passed.
<!-- vigil-unenforced: classification=non-contract-context; reason=`This is product motivation for durable review, not an additional executable surface.` -->

## Corrections should not disappear into a user interface

A correction in the current preview is a durable observation anchored to the detection it changes.
It survives reopening the store, appears on the event and `why` read surfaces, and cannot silently
attach itself to a different event.
<!-- vigil-claim: `vigil.docs-why-vigil.a-correction-in-the-current-preview-is` -->
<!-- enforced by: `vigil::ha_correction_seam::correction_survives_daemon_restart` -->
<!-- enforced by: `vigil::ha_correction_seam::correction_anchored_to_named_detection_only` -->
<!-- enforced by: `vigil::ha_correction_seam::review_events_rows_expose_current_correction_authority_fields` -->

MQTT and HTTP do not own separate correction stores. Both routes land through the same local write
path, so a broker drop does not erase the correction authority.
<!-- vigil-claim: `vigil.docs-why-vigil.mqtt-and-http-do-not-own-separate` -->
<!-- enforced by: `vigil-bin::correction_core_paths::broker_drop_does_not_affect_durable_cg_record` -->
<!-- enforced by: `vigil::http_data_plane::http_correction_post_lands_through_record_correction_seam` -->

The distinction matters because a UI-only “saved” state is not memory. It cannot safely drive later
recognition, review, or audit behavior.
<!-- vigil-unenforced: classification=non-contract-context; reason=`This sentence explains why UI-local state is insufficient rather than promising a new API.` -->

## Known and unknown must remain honest

The implemented recognition storage and matching seam can turn an enrolled detection into a named,
site-local entity and match a later sighting to it. A below-threshold result remains unknown, and a
vehicle cannot inherit a person's identity even when the test vectors are identical. These
repository tests inject a deterministic content-hash embedder; real SigLIP weight delivery and
identity-quality acceptance are not yet an operator-ready preview workflow.
<!-- vigil-claim: `vigil.docs-why-vigil.the-implemented-recognition-storage-and-matching-seam` -->
<!-- enforced by: `vigil::recognition_slice::enroll_correction_creates_named_entity_and_later_sightings_match` -->
<!-- enforced by: `vigil::recognition_slice::below_threshold_sighting_stays_unknown` -->
<!-- enforced by: `vigil::recognition_slice::vehicle_sighting_cannot_match_a_person_identity_even_with_identical_vector` -->

The explanation includes the score, the exact enrolled reference that matched, and the correction
that created that reference.
<!-- vigil-claim: `vigil.docs-why-vigil.the-explanation-includes-the-score-the-exact` -->
<!-- enforced by: `vigil::recognition_slice::recognition_provenance_joins_enroll_correction_by_matched_reference_label` -->

False-alarm and wrong-class corrections are durable and reviewable today.
<!-- vigil-claim: `vigil.docs-why-vigil.falsealarm-and-wrongclass-corrections-are-durable-and` -->
<!-- enforced by: `vigil::ha_correction_seam::false_alarm_and_wrong_class_corrections_record_and_read_back` -->

They do **not** yet tune the detector, create a standing local rule, or guarantee changed future
alert behavior. That learning loop is pre-release work.

<!-- vigil-unenforced: classification=future-surface; reason=`Correction-driven detector tuning and standing local rules are not implemented.` -->

## Hardware claims must describe what is running

Camera software often exposes an acceleration switch without proving that frames or inference
actually use the selected device. Vigil records achieved backend state. Requested-but-unavailable
acceleration is reported as fallback, while an intentional CPU setting is reported as disabled;
the two states are not collapsed.
<!-- vigil-claim: `vigil.docs-why-vigil.camera-software-often-exposes-an-acceleration-switch` -->
<!-- enforced by: `vigil::decode_backend_contract::probe_reports_decoded_frames_and_classification` -->
<!-- enforced by: `vigil::detection_accel_backend::detection_accel_compiled_in_receipt_agrees_with_recorded_probe` -->
<!-- enforced by: `vigil::acceleration_receipts::accel_false_is_disabled_not_fallback_and_they_are_distinct` -->
<!-- enforced by: `vigil::acceleration_receipts::stats_show_active_decoder_per_stream_and_detector_backend` -->

The tested promotion seam can swap a detector when its background preparation completes and
propagate the receipt without a restart. The repository test drives an injected preparation and
receipt sink; promotion of the actual running worker and its health/stats wiring remains dev-box
smoke evidence.
<!-- vigil-claim: `vigil.docs-why-vigil.the-tested-promotion-seam-can-swap-a` -->
<!-- enforced by: `vigil::detection_preparation_outcomes_reach_every_surface::a_completed_preparation_promotes_and_the_same_account_reaches_every_surface` -->

## Frigate is the baseline, not a dependency

Frigate establishes the practical baseline for a Home Assistant NVR: real camera ingestion,
detection, recording, review, acceleration, and automation. Vigil's target is to provide that
surface while keeping provenance and corrections as first-class local memory. It is not designed
as a sidecar that requires Frigate to remain in the runtime path.
<!-- vigil-unenforced: classification=product-decision; reason=`Replacing the Frigate runtime rather than depending on it defines Vigil product scope.` -->

The developer preview has proved one direct RTSP end-to-end path without a Frigate runtime.
<!-- vigil-claim: `vigil.docs-why-vigil.the-developer-preview-has-proved-one-direct` -->
<!-- enforced by: `vigil-acceptance::acceptance::one_camera_acceptance::frigate_replacement_loop_runs_over_direct_rtsp_synthetic` -->

It does not yet provide the complete replacement surface. ONVIF discovery, full recording and
retention, zones and masks, tracking, local rules, and a field-by-field migration guide remain
pre-release work.

<!-- vigil-unenforced: classification=future-surface; reason=`The complete Frigate-replacement surface listed here remains pre-release work.` -->

## What Vigil does not promise

Vigil OSS has no built-in login and will not add one. Access control belongs to the deployment
boundary: Home Assistant Ingress on Home Assistant OS (HAOS), an operator-provided ingress or proxy
for a standalone deployment, or an explicitly trusted LAN. OSS also excludes natural-language
onboarding, cross-customer learning, and a hosted multi-property control plane.

<!-- vigil-unenforced: classification=product-decision; reason=`Deployment-owned access and OSS/Enterprise exclusions are explicit product-boundary choices.` -->

The preview has not completed that deployment-boundary work yet. Its current review listener must
not be treated as the release security contract.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`The current listener does not yet satisfy the approved deployment security boundary.` -->

Managed onboarding, remote fleet operations, hosted multi-site control, reusable intent catalogs,
cross-tenant patterns, and automatic tuning across customers belong to Vigil Enterprise. None is a
prerequisite for the one-site OSS runtime.
