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

Without `--config`, a non-add-on process does not search a default TOML location; it runs on what is
already in its settings store, its own built-in defaults beneath that, and any command-line
overrides. In the Home Assistant add-on, Vigil reads `/data/options.json` automatically.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No parser test binds the absence of default TOML search outside the add-on.` -->

Settings do not resolve by merging surfaces. The settings store is the source of truth: the
configuration file or add-on options, the startup options, and `vigil settings` are authors that
write records into it, and the runtime resolves from the store. A value you set at this deployment
outranks one your management server pushed, which outranks Vigil's own automatic choice, and the
most specific scope wins within a given author; between two of your own surfaces, the one that
authored most recently wins. The environment holds no rank at all, because it is not an author.

<!-- vigil-claim: `vigil.docs-configuration.settings-do-not-resolve-by-merging-surfaces` -->
<!-- enforced by: `vigil::settings_authority_ranking::an_older_local_pin_outranks_a_newer_pushed_record` -->
<!-- enforced by: `vigil::settings_authority_ranking::an_older_pushed_record_outranks_a_newer_automatic_adjustment` -->
<!-- enforced by: `vigil::settings_authority_ranking::the_most_specific_scope_wins_within_one_author_before_authors_are_compared` -->
<!-- enforced by: `vigil::settings_record_coexistence::between_local_surfaces_the_most_recent_author_wins` -->
<!-- enforced by: `vigil::environment_behavior_var_reported_ignored::an_environment_variable_naming_a_behavior_setting_is_reported_as_ignored_with_the_reason_and_where_to_set_it` -->

The fabric enrollment ticket is the exception, and it is an exception about secrets rather than
about fabric: it is settable through the ordinary surfaces and through `VIGIL_FABRIC_TICKET`, and a
secret found in the environment wins, because that is the per-process injection a rotation just
handed this run. The hub switch is an ordinary setting resolved from the store like any other.

<!-- vigil-unenforced: classification=documentation-gap; reason=`Secret precedence is proven for the camera password by vigil::secret_source_precedence, and the fabric ticket is on the same product roster, but no test names the ticket or the hub switch as the instances this paragraph states.` -->

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

Stable MQTT topic and Home Assistant device namespace. It is derived ONCE — at the first start that
has a store to persist it into, as a lowercase underscore-separated value from `site_name` — and read
back from that persisted record on every later start. Renaming the site afterwards changes a display
name and nothing else: re-deriving the identifier would hand Home Assistant an entirely new device
and orphan the entity history attached to the old one. Moving it is its own deliberate operation,
`vigil settings identity change <identifier> --confirm`, which states that consequence before it
takes effect; an ordinary `vigil settings set service_identity` is refused with the same explanation.
The current parser accepts `service_id` from TOML or add-on JSON and `VIGIL_SERVICE_ID` from the
environment, and a value supplied that way seeds the first start only; there is no command-line flag.
A run with no readable store uses an identifier derived for that run alone and persists nothing.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No complete test binds service_id availability across TOML, add-on JSON and the environment together.` -->

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
<!-- enforced by: `vigil-bin::first_light_loop::separate_rtsp_credentials_authenticate_without_url_userinfo` -->

Legacy one-camera fields also exist: `camera_name`, `rtsp_url`, `live_rtsp_url`, `rtsp_username`,
and `rtsp_password`. Prefer `[[cameras]]` for a new configuration.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent compatibility test binds all legacy single-camera fields.` -->

The current binary still parses `--rtsp-password`; do not use or document it as a supported secret
path. The binding release contract removes passwords from process arguments because they can be
visible in `ps`. TOML, add-on options, and `VIGIL_RTSP_PASSWORD` keep the password out of process
arguments, but none is intrinsically a secret vault. Restrict access to the TOML file, add-on
options, and `/data/options.json`.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`The still-parsed password argv flag violates the release secret boundary.` -->

### Stream reconnection

`rtsp_retry_initial_ms` (default `2000`) and `rtsp_retry_max_ms` (default `30000`) govern how a
dropped camera stream reconnects; both are whole milliseconds between `1` and `86400000` (one day).
See [Cameras: Reconnect behavior](cameras.md#reconnect-behavior) for the doubling-backoff mechanism
the two values bound.

<!-- vigil-unenforced: classification=documentation-gap; reason=`retry_and_probe_setting_authoring.rs proves both values reach the loader from a real file; no adjacent config contract binds their declared range in one place.` -->

## Detection fields

| Field | Default | Validation and behavior |
|---|---:|---|
| `detector_model_id` | `yolox-tiny-burn-cpu` | Identifier written into detector-config provenance. |
| `detector_model_path` | none | Optional local checkpoint loaded by detector execution. |
| `detector_confidence_threshold` | `0.5` | Must be finite and between `0.0` and `1.0`. |
| `detector_sample_frames` | `5` | Must be between 1 and 64. |
| `detector_stationary_interval_secs` | `30` | Periodic detector pass over motion-free segments in every deployment. Set it to `0` to stop re-scanning a motion-free scene at all, so nothing but movement reaches the detector. |
| `hardware_decoding` | `true` | Probe hardware video decoding; fall back visibly when unavailable. |
| `accelerated_detection` | `true` | Probe Burn/wgpu detection where compiled; otherwise run Burn/CPU. |

<!-- vigil-claim: `vigil.docs-configuration.field-default-validation-and-behavior-detectormodelid-yoloxtinyburncpu` -->
<!-- enforced by: `vigil::config::tests::addon_options_json_recognition_fields_enable_runtime_config_and_startup_line` -->
<!-- enforced by: `vigil::config::tests::review_port_cli_override_is_documented_and_loaded` -->
<!-- enforced by: `vigil-bin::first_light_loop::detector_confidence_threshold_filters_detector_output` -->
<!-- enforced by: `vigil::config_acceleration_intent::toml_config_can_disable_each_boolean_independently` -->
<!-- enforced by: `vigil::runtime::tests::detector_gate_periodically_enqueues_motion_free_segments_for_stationary_scan` -->
<!-- enforced by: `vigil::config_acceleration_intent::acceleration_booleans_default_true_everywhere` -->
<!-- enforced by: `vigil::acceleration_receipts::cpu_only_artifact_with_accel_true_reports_backend_not_compiled_fallback` -->
<!-- enforced by: `vigil::decode_backend_contract::hardware_backend_failure_activates_software_fallback_with_visible_reason` -->
<!-- enforced by: `vigil-bin::first_light_loop::detector_config_recorded_as_decision` -->
<!-- enforced by: `vigil-bin::first_light_loop::detector_loads_and_runs_over_real_frames` -->

The Home Assistant schema exposes `detector_model_path`, `detector_stationary_interval_secs`,
`detector_confidence_threshold`, and `detector_sample_frames` as add-on options, using the same
defaults and behavior described in the table above. It also exposes `detector_queue_capacity`
(default `1`), the depth the detector's work queue holds before newer work replaces older; together
the three form the set an operator reaches for when the machine cannot keep up.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No table-driven contract binds the add-on schema exposure of these detector controls in one place.` -->

Detector-class selection ships as `detector_classes`: a loose list of class names, never a fixed
enumeration in the manifest. Today validation runs against the bundled detector's compiled class
inventory (`yolox_detector::COCO_CLASSES`), not against a loaded model instance, so a write refuses
correctly before any detector exists; the inventory changes only when the compiled-in detector
changes, which currently means a packaging release. Vigil names any entry the inventory does not
carry. `detector_classes` and
`recognition_covered_classes` are two separate operator choices — widening what the detector looks
for does not change what recognition puts a name to, and widening recognition does not silently
widen detection.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent config contract binds detector_classes validation against the model's class inventory in one place.` -->

### Motion sensitivity

`motion_sensitivity` is a deployment-wide value on the declared `1`–`10` scale; Vigil's own default
is `5` when nothing is set. Each `[[cameras]]` entry accepts its own `motion_sensitivity` override:
a camera whose own value is set runs at that value, and every other camera keeps running the
deployment-wide value, so turning one camera's sensitivity down does not touch the rest of the site.
Automatic per-camera calibration is not implemented; the setting is a manual per-camera knob.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent config contract binds the deployment-wide default, the 1-10 range, and the per-camera override precedence in one place.` -->

Vigil does not download a detector checkpoint at startup. The file must already exist at the
configured local path. How released images receive that checkpoint is still part of the public
artifact work.

<!-- vigil-unenforced: classification=external-procedure; reason=`Released images lack a supported detector-checkpoint delivery or upload procedure.` -->

## Video encoding fields

Vigil's shared camera encoder seam (`crate::encode`, not yet wired to a running capture path) derives
an automatic keyframe interval and an automatic bitrate. Both are operator-adjustable through the
typed settings registry, the same mechanism `detector_stationary_interval_secs` uses: an untouched
field keeps its automatic default, and a configured value is an explicit pin.
| Field | Default | Validation and behavior |
|---|---:|---|
| `keyframe_interval_fps_multiplier` | `2` | Must be between 1 and 10. Multiplied by a stream's effective output frame rate to derive its automatic keyframe interval, in output frames. |
| `keyframe_interval_min_frames` | `15` | Must be between 1 and 1800. The automatic keyframe interval's lower clamp, in output frames. |
| `keyframe_interval_max_frames` | `300` | Must be between 1 and 3600. The automatic keyframe interval's upper clamp, in output frames. |
| `bitrate_bps_up_to_640x480` | `1000000` | Must be between 100000 and 100000000. Automatic bitrate, in bits per second, for a stream at or below 640x480. |
| `bitrate_bps_up_to_1280x720` | `2000000` | Automatic bitrate for a stream above 640x480 and at or below 1280x720. |
| `bitrate_bps_up_to_1920x1080` | `4000000` | Automatic bitrate for a stream above 1280x720 and at or below 1920x1080. |
| `bitrate_bps_up_to_2560x1440` | `6000000` | Automatic bitrate for a stream above 1920x1080 and at or below 2560x1440. |
| `bitrate_bps_above_2560x1440` | `10000000` | Automatic bitrate for a stream above 2560x1440. |

<!-- vigil-claim: `vigil.docs-configuration.field-default-validation-and-behavior-keyframeintervalfpsmultiplier-2` -->
<!-- enforced by: `vigil::config::tests::keyframe_and_bitrate_settings_declare_with_ratified_defaults_and_real_surfaces` -->
<!-- enforced by: `vigil::encode::tests::automatic_keyframe_interval_frames_uses_the_multiplier_and_clamps_to_the_given_bounds` -->
<!-- enforced by: `vigil::encode::tests::automatic_bitrate_bps_selects_by_resolution_class_from_the_given_table` -->
<!-- enforced by: `vigil::settings_surface_coverage::declared_settings_are_covered_by_every_promised_surface` -->

The Home Assistant add-on schema exposes all eight fields above under the same names. None of the
eight has a command-line flag or an environment variable; the add-on options and a standalone TOML
file are the only surfaces, matching the `cameras` list and `recognition_covered_classes` precedent.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No table-driven test binds the absence of a CLI flag or environment variable for these eight fields specifically.` -->

A camera's live subscriber queue capacity is not itself a separate setting: it is always
`max(effective_keyframe_interval_frames, 15)`, so a subscriber that may drop and rejoin always has a
retained window containing at least one full keyframe to resume from. The `15` floor is the same
`keyframe_interval_min_frames` automatic default above, not an independent number.

<!-- vigil-claim: `vigil.docs-configuration.a-cameras-live-subscriber-queue-capacity-is-not` -->
<!-- enforced by: `vigil::camera_hub::tests::automatic_capacity_floor_derives_from_the_keyframe_interval_min_frames_default` -->
<!-- enforced by: `vigil::camera_hub_fanout::automatic_capacity_is_derived_from_the_effective_keyframe_interval_and_an_explicit_override_is_unaffected` -->

Vigil's frame rate is not a configurable field: the runtime carries one output frame per input frame
at the source's own reported rate, with no resampling step anywhere in the media pipeline. There is
nothing to pin because there is no second rate to choose between.

<!-- vigil-unenforced: classification=product-decision; reason=`Frame rate follows the source with no resampling step, so there is no operator-adjustable parameter to expose.` -->

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

The intended operator defaults use no weights directory and `vigil_site_vision_v1` as the space.
Recognition coverage defaults to `person` and `dog` (`default_recognition_covered_classes`) — the
deployment default this add-on shipped before recognition coverage had its own settings entry —
narrowed from the wider twelve-class engine baseline (`RecognitionConfig::default()`) that was live
earlier; an operator who relied on the wider set must now set `recognition_covered_classes`
explicitly. The mapped options-loader test does not bind the space and class-list defaults as one
contract.

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
<!-- enforced by: `vigil-ha::ha_mqtt_broker::mqtt_gated_off_when_no_broker_configured` -->

## Acceleration probe deadlines

The add-on schema accepts optional `decode_probe_deadline_secs` (default `5`). It is how long a
stream session gathers real video before it chooses a decode path, not a bound on normal frame
processing. Detection has no equivalent: preparing the accelerated detector runs in the background
and ends only on its own outcome, so there is no waiting period to set and none is offered. While it
runs, the surfaces report a preparation under way, since when, and the last thing it reported; on
completion the detector is swapped live and every surface moves together.

<!-- vigil-claim: `vigil.docs-configuration.the-addon-schema-accepts-optional-decodeprobedeadlinesecs` -->
<!-- enforced by: `vigil::probe_deadline_knob_surface::addon_config_declares_the_decode_probe_sample_wait_as_an_optional_integer` -->
<!-- enforced by: `vigil::probe_deadline_knob_surface::addon_help_documents_decode_probe_deadline_option` -->
<!-- enforced by: `vigil::probe_deadline_knob_surface::the_decode_probe_sample_wait_defaults_to_the_documented_number_of_seconds` -->
<!-- enforced by: `vigil::probe_timer_removal::neither_detection_timer_is_offered_as_an_add_on_option` -->
<!-- enforced by: `vigil::detection_preparation_outcomes_reach_every_surface::a_preparation_still_working_is_never_ended_for_it` -->
<!-- enforced by: `vigil::detection_preparation_promotes_when_it_completes::a_preparation_under_way_is_reported_as_such_and_promotes_when_it_completes` -->
<!-- enforced by: `vigil::detection_preparation_outcomes_reach_every_surface::a_completed_preparation_promotes_and_the_same_account_reaches_every_surface` -->

The schema also accepts optional `hardware_probe_deadline_secs` (default `10`), a whole number of
seconds bounding a further point in the same acceleration path: how long the hardware-decode probe
waits to collect its buffer of real stream units before deciding on whatever it has collected, so a
low-fps camera that cannot fill the buffer quickly still gets a backend decision instead of hanging —
distinct from `decode_probe_deadline_secs` above, which bounds the GStreamer pipeline probe itself.

<!-- vigil-unenforced: classification=documentation-gap; reason=`retry_and_probe_setting_authoring.rs proves the decode-side settings reach the loader from a real file; no table-driven contract binds this prose to them in one place.` -->

`detection_backend` reports which detection backend is actually running — `burn-cpu` in every
artifact, `burn-wgpu` only where the accelerated feature is compiled in — and an operator may also
pin it directly through this setting. A pin naming a backend the running artifact does not carry is
refused, naming the backends it does carry instead. Pinning `burn-wgpu` does not bypass the probe:
hardware is still entered only after a real forward pass succeeds on this machine, never off a stored
name alone.

<!-- vigil-unenforced: classification=documentation-gap; reason=`addon_config_surface::the_two_backend_settings_are_declared_as_loose_strings proves the schema shape; no adjacent contract binds detection_backend's pin-vs-report and probe-gated-pin behavior described here.` -->

`decode_backend` reports which decode backend is actually running — `software` in every artifact,
`hardware` only where the GStreamer decode feature is compiled in — and an operator may also pin it
directly through this setting. A pin naming a backend the running artifact does not carry is refused,
naming the backends it does carry instead. A name held in the store is only a request: pinning
`hardware` still goes through the probe, and hardware decode is entered only after a probe on this
machine succeeds, never off the stored name alone. The decode selection reports the outcome of that
probe back onto this same setting each time a stream settles, so the value read back is the answer,
not the request.

<!-- vigil-unenforced: classification=documentation-gap; reason=`settings_decode_backend_running_authority::turning_hardware_decoding_off_makes_the_pinned_decode_backend_what_runs proves the pin takes effect live; no adjacent contract binds decode_backend's full pin-vs-report and probe-gated-pin behavior described here.` -->

## Distributed compute fields

The parser accepts `fabric_ticket`, `fabric_hub`, `fabric_allow_frame_offload`,
`fabric_worker_lease_ms`, and `fabric_fallback_horizon_ms`. Do not enable distributed compute as a
release feature yet: movement and recovery have been live-proven, but remote/rescued detector jobs
can currently return empty detection content. The two tuning fields are present on configuration
surfaces but are not both production-wired. This is an implementation blocker, not optional polish.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Fabric can return empty detections and its tuning fields are not fully production-wired.` -->

## Reflecting effective values

Vigil mirrors an effective value it computes or applies back onto the add-on's own options page, so
the page an operator trusts does not go on showing a stale number. Posting the mirrored value is
immediate and free. `restart_on_reflect` (default `true`) governs only whether Vigil also triggers
the restart that makes the running container's own copy of the options file agree, and it is
consulted only where a restart is what brings the value into force: a setting this process takes on
while running is mirrored and never restarted for, and a setting only a restart brings into force is
mirrored and then restarted. Turned off, a startup-only value is still mirrored onto the options
page, but the container's own file keeps the old value until the next restart — Vigil reports that as
a pending divergence rather than claiming the mirror is fully applied.

<!-- vigil-unenforced: classification=documentation-gap; reason=`supervisor_options_reflection.rs proves each half separately — a_setting_this_process_takes_on_live_is_mirrored_without_replacing_the_container for the live-applied path, a_setting_only_a_restart_brings_into_force_is_mirrored_and_then_restarted for the startup-only path, and with_restart_on_reflect_off_the_value_applies_where_it_can_and_the_rest_is_reported_pending_with_the_divergence for the off path; no adjacent config contract binds this restart_on_reflect summary in one place.` -->

## Fields not yet available

The current parser has no zones, masks, recording mode, retention, rule/action, listen-interface,
analysis-rate, snapshot-overlay, or scheduled-check blocks. Those are pre-OSS work. Examples using
them would be fictional and do not belong in an operator guide yet.

<!-- vigil-unenforced: classification=future-surface; reason=`Zones, retention, rules, binds, and other listed parser blocks are absent.` -->

There is also no `vigil config check` or `vigil config validate` command. Configuration is validated
when `vigil run` loads it.

<!-- vigil-unenforced: classification=future-surface; reason=`Config check and config validate commands have not been implemented.` -->

## Environment variables

Environment variables no longer steer hardware decoding or accelerated detection. Exporting
`VIGIL_HARDWARE_DECODING` or `VIGIL_ACCELERATED_DETECTION` changes nothing about what runs: both
switches are set through the settings surfaces — the configuration file, the add-on options, startup
options, or `vigil settings` — and a variable found in the environment is reported as ignored, with
the place to set it instead.

<!-- vigil-claim: `vigil.docs-configuration.environment-variables-no-longer-steer-hardware-decoding` -->
<!-- enforced by: `vigil::config_acceleration_intent::environment_variables_no_longer_steer_acceleration` -->

That holds for every behavior setting but one. Paths, ports, the site and legacy single-camera
identity, detector values, recognition values and fabric values are settings that live in the store
and are written through those same surfaces; an environment variable naming one of them is reported
as ignored rather than honored.

The one documented exception is `VIGIL_DETECTOR_QUEUE_CAPACITY`: it deliberately overrides the stored
detector queue depth for the run, so the owner smoke can reproduce an overflow without waiting on
live scene traffic. When it is set, the running surface attributes the in-force capacity to that
variable rather than reporting it as ignored or as the stored value, and it does so from the first
moment the surface can be asked, on any node — including one with no cameras configured. The
attribution appears on the setting's line as two tokens beside `running=`:
`running-source=environment:<VAR>` names the environment variable that supplied the running value
(for example `running-source=environment:VIGIL_DETECTOR_QUEUE_CAPACITY`), and
`shadowed-setting=<value>` carries the depth this node would be running at if the lever were not
there — the operator's own stored pin where they set one, and the depth Vigil chooses for itself
where they did not — so the lever's number is never mistaken for the node's own setting.

<!-- vigil-claim: `vigil.docs-configuration.the-one-documented-exception-is-vigil-detector-queue-capacity` -->
<!-- enforced by: `vigil-bin::settings_queue_capacity_environment_attribution_live::a_cameraless_node_still_names_the_lever_that_supplied_its_queue_depth` -->
<!-- enforced by: `vigil-bin::settings_queue_capacity_environment_attribution_live::an_unpinned_node_shadows_the_depth_vigil_chose_for_itself_not_an_operator_pin` -->
<!-- enforced by: `vigil-bin::settings_queue_capacity_environment_attribution_live::the_queue_depth_a_camera_node_reports_names_its_lever_from_the_first_moment_it_can_be_asked` -->

What
the environment still carries otherwise is everything that is not a behavior setting: camera and
fabric secrets, the bootstrap locations that must be readable before the store opens, declared
internal diagnostics, the deployment's own process identity, and the values the platform injects for
service discovery. Password environment variables avoid command-line exposure but are not secret
storage.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The per-variable disposition of the broader environment inventory has no table-driven contract; the ignored-behavior reporting itself is covered by vigil::environment_behavior_var_reported_ignored.` -->
