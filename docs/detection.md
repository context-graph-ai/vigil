# Detection

Vigil decodes the configured RTSP analysis stream, motion-gates segments, samples frames, and runs a
YOLOX detector through an engine-neutral detector interface. Object detection uses Burn; it does
not use a hosted model or the removed local-understanding runtime.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent runtime contract binds Burn detection and exclusion of hosted/local-understanding paths.` -->

## Motion gating

Motion-positive segments go to the detector. Motion-free segments are normally suppressed, with an
optional periodic look-anyway scan for stationary people or objects.

<!-- vigil-claim: `vigil.docs-detection.motionpositive-segments-go-to-the-detector-motionfree` -->
<!-- enforced by: `vigil::first_light_loop::motion_gate_suppresses_non_motion_frames` -->
<!-- enforced by: `vigil::runtime::tests::detector_gate_still_enqueues_motion_positive_segments` -->
<!-- enforced by: `vigil::runtime::tests::detector_gate_periodically_enqueues_motion_free_segments_for_stationary_scan` -->

`detector_stationary_interval_secs` controls the look-anyway interval and defaults to 30 seconds in
both the Home Assistant add-on and the standalone runtime. Setting it to `0` explicitly disables
the periodic pass.

<!-- vigil-claim: `vigil.docs-detection.stationary-scan-defaults-to-30-seconds` -->
<!-- enforced by: `vigil::config::tests::addon_options_json_recognition_fields_enable_runtime_config_and_startup_line` -->
<!-- enforced by: `vigil::runtime::tests::detector_gate_periodically_enqueues_motion_free_segments_for_stationary_scan` -->
<!-- enforced by: `vigil::runtime::tests::detector_gate_suppresses_motion_free_segments_when_stationary_scan_is_disabled` -->

Per-camera motion sensitivity and automatic calibration are not implemented. The release adds a
simple visible per-camera sensitivity setting; automatic calibration remains later work.
<!-- vigil-unenforced: classification=future-surface; reason=`Per-camera sensitivity and automatic calibration are not implemented in the current runtime.` -->

## Object detection

The current baseline model is YOLOX Tiny executed by Burn. The live acceptance test loads the real
checkpoint, runs it over decoded frames, and compares the detector path with an independent oracle.

<!-- vigil-claim: `vigil.docs-detection.the-current-baseline-model-is-yolox-tiny` -->
<!-- enforced by: `vigil::first_light_loop::detector_loads_and_runs_over_real_frames` -->

`detector_confidence_threshold` defaults to `0.5`. A result below the threshold is not emitted as a
detection. The value must be finite and between `0.0` and `1.0`.

<!-- vigil-claim: `vigil.docs-detection.detectorconfidencethreshold-defaults-to-05-a-result-below` -->
<!-- enforced by: `vigil::config::tests::review_port_cli_override_is_documented_and_loaded` -->
<!-- enforced by: `vigil::first_light_loop::detector_confidence_threshold_filters_detector_output` -->

`detector_sample_frames` defaults to five frames per segment and accepts values from 1 through 64.
It is available in TOML, the environment, and the standalone command line, but it is not yet an
add-on option. Adding the operator-facing setting and the automatic-analysis-rate status is pre-OSS
work.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No unified test binds sample-frame default, range, and TOML/env/CLI availability.` -->

## Detection classes

With recognition off, the current detector path is person-focused. Enabling recognition widens
detector outputs to the configured recognition-covered COCO classes, always including person.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No direct contract binds recognition-dependent detector-class widening and person inclusion.` -->

The release contract is different and not yet implemented: detector classes will have their own
control, independent of recognition, and the full COCO set will be user-selectable with a sensible
default subset. Recognition configuration must not be used as a substitute for detector-class
configuration.

<!-- vigil-unenforced: classification=product-decision; reason=`Independent detector-class control and its default subset are approved release behavior.` -->

## More than one subject

The current runtime does not yet satisfy the release contract for a busy frame. Before OSS release,
every above-threshold detection becomes an event, with indexed duplicate suppression. A person,
animal, and vehicle in the same scene must not collapse to a single top result.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Busy scenes can collapse multiple above-threshold subjects into one event.` -->

This is an implementation gap. The existing single-detection acceptance does not enforce the
multi-subject promise, so no enforcement tag is attached here.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Multi-subject release behavior has no enforcing acceptance test or implementation.` -->

## Hardware video decoding

`hardware_decoding = true` means "probe and use hardware when proven," not "claim hardware is
active." The hardware add-on and hardware Docker artifacts use GStreamer and a narrowly mapped DRM
render device. If the device, permission, codec, or probe is unsuitable, Vigil continues with
software decoding and records why.

<!-- vigil-claim: `vigil.docs-detection.hardwaredecoding-true-means-probe-and-use-hardware` -->
<!-- enforced by: `vigil::addon_config_surface::addon_config_maps_video_device_and_forbids_full_access` -->
<!-- enforced by: `vigil::decode_backend_contract::hardware_backend_failure_activates_software_fallback_with_visible_reason` -->

The static musl artifact is software-only. ARM hardware images are build-proven but do not carry a
real-board acceleration performance promise yet.
<!-- vigil-unenforced: classification=external-procedure; reason=`ARM acceleration performance still requires measurement on a physical supported board.` -->

## Accelerated object detection

`accelerated_detection = true` asks Vigil to probe the Burn/wgpu backend in an artifact that ships
it. A successful real forward pass selects the GPU backend. A missing backend, missing device,
failed probe, or disabled setting keeps Burn/CPU active and records a distinct receipt.

<!-- vigil-claim: `vigil.docs-detection.accelerateddetection-true-asks-vigil-to-probe-the` -->
<!-- enforced by: `vigil::detection_accel_backend::detection_accel_hardware_flag_requires_recorded_passing_probe` -->
<!-- enforced by: `vigil::acceleration_receipts::cpu_only_artifact_with_accel_true_reports_backend_not_compiled_fallback` -->
<!-- enforced by: `vigil::acceleration_receipts::accel_false_is_disabled_not_fallback_and_they_are_distinct` -->

A slow first shader compilation can outlive the startup deadline. Vigil falls back immediately so
camera startup is not blocked and lets the probe finish. The tested promotion seam can swap a
detector and propagate the late receipt without a restart, using an injected probe and receipt sink.
Actual running-worker promotion remains live-smoke evidence rather than deterministic runtime
acceptance evidence.

<!-- vigil-claim: `vigil.docs-detection.a-slow-first-shader-compilation-can-outlive` -->
<!-- enforced by: `vigil::detection_probe_promotes_after_deadline::late_pass_promotes_and_the_promoted_receipt_reaches_every_surface` -->

Use this implemented diagnostic for the detailed acceleration report:

```console
vigil doctor acceleration
```

The doctor classifies a missing render device separately from a visible device with denied
permission, reports disabled intent without probing, and renders the current backend and probe
receipt in a fixed format. A permission finding includes a ready-to-run group-membership action.

<!-- vigil-claim: `vigil.docs-detection.the-doctor-classifies-a-missing-render-device` -->
<!-- enforced by: `vigil::doctor_acceleration::missing_device_is_no_device_visible_not_permission` -->
<!-- enforced by: `vigil::doctor_acceleration::blocked_device_classifies_permission_denied_with_ready_fix` -->
<!-- enforced by: `vigil::doctor_acceleration::disabled_intent_skips_probes_and_reports_disabled` -->
<!-- enforced by: `vigil::doctor_acceleration::doctor_renders_receipts_in_fixed_format` -->

## Zones and masks

The add-on configuration accepts no `zones` or `masks` block, and current Home Assistant and review
HTTP event rows carry no `zone` field.

<!-- vigil-claim: `vigil.docs-detection.zones-and-masks-are-not-yet-available` -->
<!-- enforced by: `vigil::source_scan_contract::addon_config_exposes_recognition_options_and_schema` -->
<!-- enforced by: `vigil::ha_discovery::tests::detection_event_payload_carries_current_contract_without_future_zone_field` -->
<!-- enforced by: `vigil::http_data_plane::event_list_serves_full_review_row_fieldset` -->

Zone matching and polygon masks are future runtime work, so automations must not depend on a zone
value yet.

<!-- vigil-unenforced: classification=future-surface; reason=`Zone matching and polygon masks are not implemented and therefore have no direct deterministic runtime witness.` -->

## Recognition

The deterministic recognition seam embeds crop bytes, enrolls a site-local reference, matches a
later same-subject sighting, and keeps a below-threshold sighting unknown. Repository tests use a
content-hash embedder for this repeatable storage-and-matching proof.

<!-- vigil-claim: `vigil.docs-detection.recognition-has-an-implemented-optional-storage-and` -->
<!-- enforced by: `vigil::recognition_slice::below_threshold_sighting_stays_unknown` -->
<!-- enforced by: `vigil::recognition_slice::enroll_correction_creates_named_entity_and_later_sightings_match` -->

Production recognition is intended to use the local Candle/SigLIP embedder only after an operator
stages weights. The preview has no supported weight-delivery workflow, and the deterministic seam
does not establish real SigLIP identity quality.

<!-- vigil-unenforced: classification=external-procedure; reason=`Real SigLIP weights and identity-quality acceptance require an operator-staged model workflow that is not currently delivered.` -->

Recognition matching defaults to `0.90`. An explicitly configured finite threshold from `0.0`
through `1.0` is honored without a hidden floor; startup reports an accuracy warning when the
configured value is below `0.90`.

<!-- vigil-claim: `vigil.docs-detection.recognition-matching-defaults-to-0-90` -->
<!-- enforced by: `vigil::recognition_slice::configured_recognition_threshold_is_honored_without_a_hidden_floor` -->
<!-- enforced by: `vigil::config::tests::addon_options_json_recognition_fields_enable_runtime_config_and_startup_line` -->

Sighting-vector retention and unexpected-class review are still pre-OSS work.

## Honest health and coverage

An ingest-failed health state answers non-success. A disk-full clip failure prevents an event from
referencing evidence that was never made durable.

<!-- vigil-claim: `vigil.docs-detection.a-detector-or-ingest-failure-changes-health` -->
<!-- enforced by: `vigil::first_light_loop::disk_full_on_clip_write_surfaces_and_drops_no_evidence` -->
<!-- enforced by: `vigil::first_light_loop::observation_never_references_undurable_clip` -->
<!-- enforced by: `vigil::health_watchdog_liveness::genuinely_dead_states_answer_non_2xx` -->

A separate detector-dead health transition is intended to keep a runtime from appearing ready with
no detector, but the mapped health witness currently enumerates store-open and ingest failures.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The mapped health-status test does not enumerate a separate detector-dead state or prove its runtime transition.` -->

The release adds user-visible coverage accounting for dropped frames, skipped work, stale streams,
and newest-wins queue replacement. Those conditions are not yet fully surfaced outside logs.
<!-- vigil-unenforced: classification=implementation-blocker; reason=`Coverage-loss accounting is not fully exposed beyond logs for operator diagnosis.` -->
