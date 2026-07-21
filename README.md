[![CI](https://github.com/context-graph-ai/vigil/actions/workflows/ci.yml/badge.svg)](https://github.com/context-graph-ai/vigil/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](https://www.apache.org/licenses/LICENSE-2.0)

# Vigil

Vigil is a local-first camera-intelligence runtime for Home Assistant. It is being built as a
Frigate replacement for self-hosters who want detections, evidence, corrections, and the reason
behind an alert to survive together on their own machine.

> **Developer preview:** the end-to-end core works, but Vigil is not ready to replace a production
> network video recorder (NVR) yet. There is no published add-on or container release. See
> [What is available now](#what-is-available-now) and
> [What is not yet available](#what-is-not-yet-available) before trying it.

<!-- vigil-unenforced: classification=external-procedure; reason=`Public add-on and container publication has no completed release pipeline.` -->

## What is available now

A Vigil process can read a directly configured RTSP stream, decode real frames, run the local
detector, write a detection and a locally decodable evidence clip, and walk that event back through
the camera, detector configuration, and reason for watching it.
<!-- vigil-claim: `vigil.readme.a-vigil-process-can-read-a-directly` -->
<!-- enforced by: `vigil-acceptance::acceptance::one_camera_acceptance::frigate_replacement_loop_runs_over_direct_rtsp_synthetic` -->

The current Home Assistant path publishes discovery and detection data through MQTT. It exposes a
service device, per-camera devices, a stateful camera-enable switch, a snapshot action, and a
correction command path whose durable result is stored in the local Context Graph store.
<!-- vigil-claim: `vigil.readme.the-current-home-assistant-path-publishes-discovery` -->
<!-- enforced by: `vigil::ha_discovery::tests::discovery_registers_device_and_per_camera_subdevice` -->
<!-- enforced by: `vigil::ha_mqtt_broker::discovery_published_to_real_broker_on_start` -->
<!-- enforced by: `vigil::ha_mqtt_broker::camera_enabled_switch_state_is_retained_and_updates_on_control_commands` -->
<!-- enforced by: `vigil::ha_mqtt_broker::detection_publishes_event_to_real_broker` -->
<!-- enforced by: `vigil::ha_mqtt_broker::correction_command_on_broker_lands_in_cg` -->

The local review service lists detections, walks the provenance of one detection, serves snapshots
and clips—including byte ranges for video—and records corrections through the same durable write
path used by MQTT.
<!-- vigil-claim: `vigil.readme.the-local-review-service-lists-detections-walks` -->
<!-- enforced by: `vigil::http_data_plane::event_list_serves_full_review_row_fieldset` -->
<!-- enforced by: `vigil::http_data_plane::why_walk_back_serves_provenance_excludes_rtsp_url_and_lists_correction` -->
<!-- enforced by: `vigil::http_data_plane::clip_read_serves_range_206_partial_content_transport` -->
<!-- enforced by: `vigil::http_data_plane::snapshot_read_serves_image_bytes_with_image_content_type` -->
<!-- enforced by: `vigil::http_data_plane::http_correction_post_lands_through_record_correction_seam` -->
<!-- enforced by: `vigil::ha_mqtt_broker::correction_command_on_broker_lands_in_cg` -->

The implemented recognition storage and matching seam can enroll a detection with a name, resolve a
later same-site sighting, keep a below-threshold sighting unknown, and show the matched reference,
score, and enrolling correction in `why`. Its repository tests use a deterministic content-hash
embedder. Real SigLIP weight delivery and identity-quality acceptance are not yet an operator-ready
preview workflow.
<!-- vigil-claim: `vigil.readme.the-implemented-recognition-storage-and-matching-seam` -->
<!-- enforced by: `vigil::recognition_slice::enroll_correction_creates_named_entity_and_later_sightings_match` -->
<!-- enforced by: `vigil::recognition_slice::below_threshold_sighting_stays_unknown` -->
<!-- enforced by: `vigil::recognition_slice::why_view_shows_match_provenance_reference_score_and_enrolling_correction` -->

Hardware acceleration is intent, not a label. Native hardware artifacts probe the available decode
and detection backends; if the requested backend cannot run, Vigil keeps CPU/software operation
alive and reports the actual backend and reason for fallback.
<!-- vigil-claim: `vigil.readme.hardware-acceleration-is-intent-not-a-label` -->
<!-- enforced by: `vigil::decode_backend_contract::probe_reports_decoded_frames_and_classification` -->
<!-- enforced by: `vigil::detection_accel_backend::detection_accel_compiled_in_receipt_agrees_with_recorded_probe` -->
<!-- enforced by: `vigil::acceleration_receipts::cpu_only_artifact_with_accel_true_reports_backend_not_compiled_fallback` -->
<!-- enforced by: `vigil::acceleration_receipts::health_reports_degraded_decode_and_detection_when_configured_but_fallback` -->

## Why this fired

The useful unit in Vigil is not an isolated detection row. The current direct-RTSP path stores a
chain like this:

<!-- vigil-unenforced: classification=non-contract-context; reason=`This paragraph only introduces the already documented provenance-chain diagram.` -->

```text
reason to watch the camera
  -> detector configuration in force
    -> camera and site
      -> detection
        -> local evidence clip
```

`vigil why <event-id>` and `vigil why --latest` read that chain from the local store rather than
reconstructing it from logs.
<!-- vigil-claim: `vigil.readme.vigil-why-eventid-and-vigil-why-latest` -->
<!-- enforced by: `vigil-acceptance::acceptance::one_camera_acceptance::frigate_replacement_loop_runs_over_direct_rtsp_synthetic` -->
<!-- enforced by: `vigil::first_light_loop::vigil_why_reports_config_as_of_event_time_not_current` -->

See [Why Vigil?](docs/why-vigil.md) for the product problem and
[the promise](docs/promise.md) for the exact line between the working preview and the release
target.
<!-- vigil-unenforced: classification=non-contract-context; reason=`This paragraph only links to product-problem and release-boundary documentation.` -->

## Install

There is no supported public install yet. The repository has no GitHub release, published add-on
repository, or published container image. Source checkout also depends on sibling Context Graph and
ContextDB repositories, so it is a contributor workflow rather than an operator install path.

<!-- vigil-unenforced: classification=external-procedure; reason=`A public release, image, and clean operator install procedure do not exist.` -->

The first public release adds the tested add-on, container, and local-service instructions here.
Until then, an unversioned build from `dev` should not be installed over a working NVR.
<!-- vigil-unenforced: classification=external-procedure; reason=`Public artifact publication and tested installation instructions do not exist yet.` -->

## What is not yet available

The public OSS release still needs the following work. These are not present-tense product claims:
<!-- vigil-unenforced: classification=non-contract-context; reason=`This sentence only introduces the separately classified unfinished-capability list.` -->

- a published, clean-clone Home Assistant add-on and container install;
- ONVIF discovery, Hikvision/NVR estate setup, and durable multi-camera feed identity;
- full multi-subject event recording, zones, masks, object tracking, and local rules/actions;
- continuous recording, snapshots with content controls, retention, deletion, and disk-budget
  enforcement;
- assured HA event delivery with visible loss accounting;
- the same-origin machinery dashboard, deployment-boundary hardening, and complete first-run
  configuration;
- the full Frigate configuration translation and migration path.

<!-- vigil-unenforced: classification=future-surface; reason=`The listed Frigate-replacement capabilities remain unimplemented release scope.` -->

Until those are complete and validated, “Frigate replacement” describes the product destination,
not the readiness of this developer preview.
<!-- vigil-unenforced: classification=non-contract-context; reason=`This sentence limits product-positioning language and makes no shipped-feature promise.` -->

## Project direction

Vigil OSS is for one self-hosted site at a time. The working direct-camera path writes its evidence
and operational memory locally and opens no unrelated outbound connection. The release contract
keeps raw camera media on the site by default. Hosted fleet management, cross-site control, managed
onboarding, and learning across customers belong to a separate Vigil Enterprise product; they are
not hidden dependencies of the OSS runtime.
<!-- vigil-claim: `vigil.readme.vigil-oss-is-for-one-selfhosted-site` -->
<!-- enforced by: `vigil-acceptance::acceptance::one_camera_acceptance::frigate_replacement_loop_runs_over_direct_rtsp_synthetic` -->

Read next:
<!-- vigil-unenforced: classification=non-contract-context; reason=`This is a documentation navigation list, not an independent runtime contract.` -->

- [Why Vigil?](docs/why-vigil.md)
- [What Vigil promises](docs/promise.md)
- [Product strategy and boundaries](docs/strategy.md)
- [Getting started](docs/getting-started.md) — staged separately; do not treat it as an install
  guide until a public artifact exists
<!-- vigil-unenforced: classification=non-contract-context; reason=`These links navigate documentation and warn that the draft install guide is not public.` -->

## License

Vigil is licensed under the [Apache License 2.0](LICENSE).
