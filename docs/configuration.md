# Configuration

> **Pre-OSS draft:** this is a reference for the configuration parser on the current `dev` line.
> Fields identified as release work are owner-approved product behavior but are not accepted by the
> parser yet.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This banner explains draft conventions and separates parser facts from release work.` -->

Vigil currently uses a flat TOML file for standalone runs and the Home Assistant add-on's
`/data/options.json` for add-on runs. The nested `mqtt:`, `detectors:`, `rules:`, `recording:`,
`site:`, and `health:` blocks from the older outline do not exist.
<!-- vigil-unenforced: classification=documentation-gap; reason=`No parser-shape contract binds flat configuration and rejection/absence of old nested blocks.` -->

## Config file location

For a standalone run, pass the TOML path explicitly:

```console
vigil run --config /etc/vigil/vigil.toml
```

Without `--config`, a non-add-on process does not search a default TOML location; it uses defaults,
environment variables, and command-line overrides. In the Home Assistant add-on, Vigil reads
`/data/options.json` automatically.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No parser test binds the absence of default TOML search outside the add-on.` -->

Most settings resolve in this order: built-in default, configuration file or add-on options,
command line, then environment. The fabric enrollment ticket and hub switch are exceptions: their
order is add-on/config or `DATA_DIR/fabric.toml`, environment, then command line.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No precedence matrix test binds ordinary settings and the two fabric exceptions.` -->

## Minimal standalone configuration

```toml
data_dir = "/var/lib/vigil"
site_name = "home"
health_port = 8099
review_port = 8098

detector_confidence_threshold = 0.5
detector_model_path = "/var/lib/vigil/models/yolox-tiny-coco.pth"
detector_sample_frames = 5
detector_stationary_interval_secs = 30
hardware_decoding = true
accelerated_detection = true

[[cameras]]
name = "front gate"
rtsp_url = "rtsp://camera.local:554/detection-stream"
live_rtsp_url = "rtsp://camera.local:554/live-stream"
username = "vigil"
password = "replace-me"
```

An empty `cameras` list falls back to the legacy single-camera fields. If those fields also omit
`rtsp_url`, the current runtime opens the store and reports ready without starting ingest. This
unsupported healthy-looking zero-camera state is a pre-OSS implementation blocker.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Zero cameras can report ready while no ingest path is running.` -->

## Storage fields

### `data_dir`

Directory for clips, detector images, runtime statistics, control state, and the default store.
The standalone default is `vigil-data` below the process working directory. The add-on uses its
persistent `/data` volume.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent config contract binds standalone and add-on data-directory defaults.` -->

### `store_path`

Path to the local Context Graph store. The default is `DATA_DIR/store.contextgraph`.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent config contract binds store_path derivation below data_dir.` -->

### `health_port` and `review_port`

The health/watchdog port defaults to `8099`; the HTTP review data plane defaults to `8098`.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent config contract binds both health and review port defaults.` -->

Both services currently listen on every interface and the review data plane has no Vigil login.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Both listeners bind every interface while the review plane has no authentication.` -->

The review service does not emit cross-origin access headers.

<!-- vigil-claim: `vigil.docs-configuration.review-service-does-not-emit-cross-origin-access-headers` -->
<!-- enforced by: `vigil::http_data_plane::review_data_plane_does_not_enable_cross_origin_access` -->

Configurable listen interfaces and same-origin UI delivery are still required before OSS release.
See [Privacy](privacy.md#access-boundary).

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Configurable binds and same-origin UI delivery are required before public release.` -->

## Site and service identity fields

### `site_name`

Human-readable site context. The current fallback is `site-1`.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent parser test binds the site_name fallback value.` -->

### `service_id`

Stable MQTT topic and Home Assistant device namespace. When absent, Vigil derives a lowercase,
underscore-separated value from `site_name`. The current parser accepts `service_id` from TOML or
add-on JSON and `VIGIL_SERVICE_ID` from the environment; there is no command-line flag.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No complete test binds service_id derivation and TOML/add-on/environment availability.` -->

First-run site naming and collision-proof durable camera identity are pre-OSS work. The current
camera record is matched by display name, which is not safe when two nodes use the same label.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Display-name camera identity permits collisions across same-labeled nodes.` -->

## Camera fields

Use repeated `[[cameras]]` entries. Each accepts:

| Field | Required | Meaning |
|---|---:|---|
| `name` | yes | Display label and current camera identifier input. |
| `rtsp_url` | for camera ingest | Stream Vigil decodes and analyzes. |
| `live_rtsp_url` | no | Separate stream registered for Home Assistant live view. |
| `username` | no | RTSP username kept separate from the URL. |
| `password` | no | Plain RTSP password value loaded from the file or add-on options. |

<!-- vigil-unenforced: classification=documentation-gap; reason=`The mapped camera tests do not bind the complete required/optional field matrix or every listed source for password loading.` -->

Configuration preserves a distinct detection `rtsp_url` and Home Assistant `live_rtsp_url` when
both are supplied. Separate username and password fields authenticate RTSP without putting user
information in the URL.

<!-- vigil-claim: `vigil.docs-configuration.field-required-meaning-name-yes-display-label` -->
<!-- enforced by: `vigil::config::tests::live_rtsp_url_is_distinct_from_detection_rtsp_url` -->
<!-- enforced by: `vigil::first_light_loop::separate_rtsp_credentials_authenticate_without_url_userinfo` -->

Legacy one-camera fields also exist: `camera_name`, `rtsp_url`, `live_rtsp_url`, `rtsp_username`,
and `rtsp_password`. Prefer `[[cameras]]` for a new configuration.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent compatibility test binds all legacy single-camera fields.` -->

The current binary still parses `--rtsp-password`; do not use or document it as a supported secret
path. The binding release contract removes passwords from process arguments because they can be
visible in `ps`. TOML, add-on options, and `VIGIL_RTSP_PASSWORD` keep the password out of process
arguments, but none is intrinsically a secret vault. Restrict access to the TOML file, add-on
options, and `/data/options.json`.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`The still-parsed password argv flag violates the release secret boundary.` -->

## Detection fields

| Field | Default | Validation and behavior |
|---|---:|---|
| `detector_model_id` | `yolox-tiny-burn-cpu` | Identifier written into detector-config provenance. |
| `detector_model_path` | none | Optional local checkpoint loaded by detector execution. |
| `detector_confidence_threshold` | `0.5` | Must be finite and between `0.0` and `1.0`. |
| `detector_sample_frames` | `5` | Must be between 1 and 64. |
| `detector_stationary_interval_secs` | `30` | Periodic detector pass over motion-free segments in every deployment. |
| `hardware_decoding` | `true` | Probe hardware video decoding; fall back visibly when unavailable. |
| `accelerated_detection` | `true` | Probe Burn/wgpu detection where compiled; otherwise run Burn/CPU. |

<!-- vigil-claim: `vigil.docs-configuration.field-default-validation-and-behavior-detectormodelid-yoloxtinyburncpu` -->
<!-- enforced by: `vigil::config::tests::addon_options_json_recognition_fields_enable_runtime_config_and_startup_line` -->
<!-- enforced by: `vigil::config::tests::review_port_cli_override_is_documented_and_loaded` -->
<!-- enforced by: `vigil::first_light_loop::detector_confidence_threshold_filters_detector_output` -->
<!-- enforced by: `vigil::config_acceleration_intent::toml_config_can_disable_each_boolean_independently` -->
<!-- enforced by: `vigil::runtime::tests::detector_gate_periodically_enqueues_motion_free_segments_for_stationary_scan` -->
<!-- enforced by: `vigil::config_acceleration_intent::acceleration_booleans_default_true_everywhere` -->
<!-- enforced by: `vigil::acceleration_receipts::cpu_only_artifact_with_accel_true_reports_backend_not_compiled_fallback` -->
<!-- enforced by: `vigil::decode_backend_contract::hardware_backend_failure_activates_software_fallback_with_visible_reason` -->
<!-- enforced by: `vigil::first_light_loop::detector_config_recorded_as_decision` -->
<!-- enforced by: `vigil::first_light_loop::detector_loads_and_runs_over_real_frames` -->

The Home Assistant schema currently exposes `detector_model_path` and
`detector_stationary_interval_secs`, but not confidence threshold or sampled-frame count. Adding
those supported options, per-camera motion sensitivity, and detector-class selection is pre-OSS
work.

<!-- vigil-unenforced: classification=future-surface; reason=`Confidence, frame-count, sensitivity, and class controls remain absent add-on options.` -->

Vigil does not download a detector checkpoint at startup. The file must already exist at the
configured local path. How released images receive that checkpoint is still part of the public
artifact work.

<!-- vigil-unenforced: classification=external-procedure; reason=`Released images lack a supported detector-checkpoint delivery or upload procedure.` -->

## Recognition fields

Recognition is off unless `recognition_weights_dir` points to locally staged SigLIP weights. The
current fields are:

<!-- vigil-unenforced: classification=non-contract-context; reason=`This sentence only introduces the separately bound recognition-fields table below it.` -->

| Field | Configured behavior |
|---|---|
| `recognition_weights_dir` | A supplied local directory enables recognition. |
| `recognition_space_id` | The supplied embedding-space identifier reaches runtime configuration. |
| `recognition_threshold` | Values must be between `0.0` and `1.0`; the matching default is `0.90`. |
| `recognition_covered_classes` | The supplied class list controls the recognition allowlist. |

<!-- vigil-claim: `vigil.docs-configuration.field-default-recognitionweightsdir-none-recognition-off-recognitionspaceid` -->
<!-- enforced by: `vigil::config::tests::addon_options_json_recognition_fields_enable_runtime_config_and_startup_line` -->

The intended operator defaults use no weights directory, `vigil_site_vision_v1` as the space, and a
person-plus-generic-animal/vehicle class subset. The mapped options-loader test does not bind those
space and class-list defaults as one contract.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The options-loader witness proves supplied values and the threshold default, but not the complete default space and covered-class inventory.` -->

An explicit value from `0.0` through `1.0` is honored as the actual matching threshold. Values
below the recommended `0.90` default emit an accuracy warning in the recognition startup line;
out-of-range values stop configuration loading with the accepted range.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The config test exists but this threshold range, warning, and rejection paragraph is unbound.` -->

## MQTT fields

For non-Supervisor deployments, the flat TOML fields are `mqtt_host`, `mqtt_port` (default `1883`),
`mqtt_username`, and `mqtt_password`. The corresponding environment variables are `MQTT_HOST`,
`MQTT_PORT`, `MQTT_USER` or `MQTT_USERNAME`, and `MQTT_PASSWORD`.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent parser inventory test binds MQTT names and the 1883 default.` -->

In the add-on, the broker is optional. If no host was configured and a Supervisor token is present,
Vigil reads the Home Assistant MQTT service connection. With no broker configuration, MQTT tasks do
not start.

<!-- vigil-claim: `vigil.docs-configuration.in-the-addon-the-broker-is-optional` -->
<!-- enforced by: `vigil::ha_mqtt_broker::mqtt_gated_off_when_no_broker_configured` -->

## Acceleration probe deadlines

The add-on schema accepts optional `decode_probe_deadline_secs` and
`detection_probe_deadline_secs`. They bound the startup probe, not normal frame processing. A slow
accelerated-detection probe may complete later. The promotion seam can swap a detector and propagate
the late receipt without a restart; its repository test uses an injected probe and receipt sink.
Actual running-worker promotion remains live-smoke evidence rather than deterministic runtime
acceptance evidence.

<!-- vigil-claim: `vigil.docs-configuration.the-addon-schema-accepts-optional-decodeprobedeadlinesecs-and` -->
<!-- enforced by: `vigil::probe_deadline_knob_surface::addon_config_declares_both_probe_deadline_knobs_as_optional_integers` -->
<!-- enforced by: `vigil::detection_probe_promotes_after_deadline::late_pass_promotes_and_the_promoted_receipt_reaches_every_surface` -->

## Distributed compute fields

The parser accepts `fabric_ticket`, `fabric_hub`, `fabric_allow_frame_offload`,
`fabric_worker_lease_ms`, and `fabric_fallback_horizon_ms`. Do not enable distributed compute as a
release feature yet: movement and recovery have been live-proven, but remote/rescued detector jobs
can currently return empty detection content. The two tuning fields are present on configuration
surfaces but are not both production-wired. This is an implementation blocker, not optional polish.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Fabric can return empty detections and its tuning fields are not fully production-wired.` -->

## Fields not yet available

The current parser has no zones, masks, recording mode, retention, rule/action, listen-interface,
analysis-rate, detector-class selection, per-camera motion sensitivity, snapshot-overlay, or
scheduled-check blocks. Those are pre-OSS work. Examples using them would be fictional and do not
belong in an operator guide yet.

<!-- vigil-unenforced: classification=future-surface; reason=`Zones, retention, rules, binds, classes, and other listed parser blocks are absent.` -->

There is also no `vigil config check` or `vigil config validate` command. Configuration is validated
when `vigil run` loads it.

<!-- vigil-unenforced: classification=future-surface; reason=`Config check and config validate commands have not been implemented.` -->

## Environment variables

The current parser reads `VIGIL_HARDWARE_DECODING` and `VIGIL_ACCELERATED_DETECTION`, and
environment values override ordinary file and command-line values for those two settings.

<!-- vigil-claim: `vigil.docs-configuration.the-current-parser-reads-vigilhardwaredecoding-and-vigilaccelerateddetection` -->
<!-- enforced by: `vigil::config_acceleration_intent::env_vars_are_understood` -->

The source parser also accepts environment values for data/store paths, ports, site and legacy
single-camera identity, RTSP values, detector values, some recognition values, and fabric values.
There is no `VIGIL_RECOGNITION_COVERED_CLASSES`, and no table-driven documentation test currently
binds this broader inventory. Password environment variables avoid command-line exposure but are not
secret storage.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The broader environment-variable inventory explicitly lacks a table-driven contract.` -->
