# Getting started

> **Pre-OSS draft:** Vigil is not installable from a public Home Assistant add-on repository yet.
> The runtime, add-on package, health endpoint, camera pipeline, Home Assistant discovery, and
> review surfaces exist, but the clean-clone build and public image/repository publication path
> are still release work. Do not add an invented repository URL or `docker run` command here.
<!-- vigil-unenforced: classification=external-procedure; reason=`Public add-on publication and clean-clone installation remain unfinished release procedures.` -->

This guide records the path that works in the current implementation and calls out the parts that
are not yet available to an ordinary user. It will become the public five-minute guide when the
release artifacts exist.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This paragraph explains the draft guide’s purpose and future transition.` -->

## Install the Home Assistant add-on

There is no supported public install step yet. The repository contains a Home Assistant add-on
definition, but its build expects a staged Vigil binary and no public pipeline currently produces
and publishes that image. A manually copied add-on directory is therefore not a supported install.

<!-- vigil-unenforced: classification=external-procedure; reason=`No public add-on image, repository URL, or supported install publication exists.` -->

**Not yet available before OSS release:** a Home Assistant add-on repository URL, published images,
version-coupled updates, and a clean-HAOS install check.
<!-- vigil-unenforced: classification=external-procedure; reason=`Repository publication, versioned updates, and clean-HAOS installation are not available.` -->

## Configure one camera

The current add-on accepts a `cameras` list. Each camera needs a display name and an RTSP URL for
analysis. `live_rtsp_url` is optional; use it when Home Assistant should display a different stream
from the one Vigil analyzes.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent schema contract binds required camera name/RTSP and optional live URL.` -->

```yaml
detector_model_path: /data/models/yolox-tiny-coco.pth
cameras:
  - name: front_gate
    rtsp_url: rtsp://camera.local:554/detection-stream
    live_rtsp_url: rtsp://camera.local:554/live-stream
    username: vigil
    password: use-a-real-secret-here
```

This block is illustrative only. The current add-on image contains no detector checkpoint and has no
supported checkpoint upload or install step. Vigil does not download weights at startup. A
contributor source run can point at `tests/fixtures/models/yolox-tiny-coco.pth`; an ordinary add-on
install cannot use this configuration until model delivery ships.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`The add-on has no checkpoint payload or supported model-install workflow.` -->

The current add-on options default hardware video decoding and accelerated detection intent to on.
When an acceleration backend is unavailable, Vigil continues on the software or CPU path and
reports the fallback reason.

<!-- vigil-claim: `vigil.docs-getting-started.the-current-addon-options-also-default-hardware` -->
<!-- enforced by: `vigil::acceleration_receipts::cpu_only_artifact_with_accel_true_reports_backend_not_compiled_fallback` -->
<!-- enforced by: `vigil::config_acceleration_intent::acceleration_booleans_default_true_everywhere` -->
<!-- enforced by: `vigil::decode_backend_contract::hardware_backend_failure_activates_software_fallback_with_visible_reason` -->

An accelerated backend is intended to be reported active only after a successful real probe. The
mapped default-and-fallback witnesses exercise unavailable backends rather than a successful
hardware forward path.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The mapped acceleration tests prove defaults and honest fallback, but not a successful hardware probe-to-active transition.` -->

See [Cameras](cameras.md) for credentials, split analysis/live streams, and current protocol
support. See [Configuration](configuration.md) for the exact file and add-on fields.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This paragraph only links to camera and configuration reference pages.` -->

## Start and verify health

After a development or locally built add-on starts, Vigil exposes its watchdog endpoint on port
8099:

```console
curl http://HOST:8099/health
```

A ready runtime returns HTTP 200. Store-open and ingest-failed states return non-success, while
`keep-pace-failed` remains successful so Home Assistant Supervisor does not restart-loop a live but
degraded CPU detector, and a deployment with zero `[[cameras]]` entries (a legitimate
worker/discovery node) also stays successful — a restart cannot conjure a camera, so
restart-looping it would only cause harm.

<!-- vigil-claim: `vigil.docs-getting-started.a-ready-runtime-returns-a-successful-http` -->
<!-- enforced by: `vigil::health_watchdog_liveness::cameraless_node_stays_alive_for_the_watchdog` -->
<!-- enforced by: `vigil::health_watchdog_liveness::ready_runtime_is_alive_for_the_watchdog` -->
<!-- enforced by: `vigil::health_watchdog_liveness::genuinely_dead_states_answer_non_2xx` -->
<!-- enforced by: `vigil::health_watchdog_liveness::detector_behind_on_cpu_fallback_stays_alive_for_the_watchdog` -->

The health response body is intended to name the `keep-pace-failed` degradation, but the mapped
status-code tests do not inspect a served response body.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The mapped health tests bind status-code policy only and do not verify the served degradation body.` -->

A disk-full state is also coded as non-success, but the current dead-state test does not cover that
case.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent health test currently binds the disk-full HTTP status code.` -->

The current server binds to all interfaces and has no Vigil login. That is a pre-OSS security
defect, not a deployment recommendation. Keep it on a trusted, firewalled network; read the
[privacy and access boundary](privacy.md#access-boundary) before exposing either port.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Unauthenticated all-interface listeners are unsafe outside a trusted firewall.` -->

## See the camera in Home Assistant

When Vigil runs as an add-on with Home Assistant API access, it asks the Supervisor for the MQTT
service if no broker was configured explicitly. With a broker available, Vigil publishes discovery
for a service device and per-camera event, latest-snapshot, recent-activity, enabled-switch, and
snapshot-button entities. With `SUPERVISOR_TOKEN`, it attempts to register live video separately as
a Home Assistant Generic Camera, using `live_rtsp_url` when present and `rtsp_url` otherwise.
Registration failure is logged and Vigil continues without that live view.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No combined acceptance binds Supervisor MQTT lookup, discovery, Generic Camera, and failure continuation.` -->

The camera enable switch is stateful: enable/disable commands change the live camera flag and the
reflected Home Assistant state.

<!-- vigil-claim: `vigil.docs-getting-started.the-camera-enable-switch-is-stateful-enabledisable` -->
<!-- enforced by: `vigil-ha::ha_mqtt_broker::camera_enabled_switch_state_is_retained_and_updates_on_control_commands` -->

The runtime writes a disabled marker and reads it at startup, but there is no end-to-end restart
test for that behavior yet; it is therefore not tagged as an enforced documentation claim.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No end-to-end restart test proves the disabled camera marker is restored.` -->

**Not yet release-complete:** public installation of the companion Home Assistant integration and
camera card. Their code exists in sibling repositories, but release packaging remains open.
<!-- vigil-unenforced: classification=external-procedure; reason=`Companion integration and camera-card packaging are not publicly distributed together.` -->

## See the first detection

When a motion-positive segment contains a detection above the configured confidence threshold,
Vigil writes a local event with a playable clip, a detector image, the bounding box, and the sampled
frame index. If MQTT is configured, the event metadata is also published to Home Assistant and the
latest detector image can be requested through the snapshot button.

<!-- vigil-claim: `vigil.docs-getting-started.when-a-motionpositive-segment-contains-a-detection` -->
<!-- enforced by: `vigil-bin::first_light_loop::detection_produces_observation_referencing_clip` -->
<!-- enforced by: `vigil-bin::first_light_loop::detector_confidence_threshold_filters_detector_output` -->
<!-- enforced by: `vigil-bin::correction_core_paths::detection_publishes_event_to_real_broker` -->
<!-- enforced by: `vigil-bin::mqtt_composed_product::snapshot_command_publishes_the_latest_detector_evidence_through_the_composed_binary` -->

The current detector path is not the final release behavior for busy scenes: it does not yet
guarantee one event for every above-threshold subject in a segment. Multi-detection, indexed
duplicate suppression, zones, masks, and explicit detector-class selection are pre-OSS work.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Multi-subject events, deduplication, zones, masks, and class control remain incomplete.` -->

## Review what happened

From the machine running Vigil, the implemented review commands are:

```console
VIGIL_DATA_DIR=/var/lib/vigil vigil events
VIGIL_DATA_DIR=/var/lib/vigil vigil why --latest
VIGIL_DATA_DIR=/var/lib/vigil vigil why EVENT_ID
VIGIL_DATA_DIR=/var/lib/vigil vigil stats
```

Replace `/var/lib/vigil` with the `data_dir` used by the running daemon. These commands do not read
the daemon's `--config` TOML. Without `VIGIL_DATA_DIR` or an exact `VIGIL_STORE_PATH`, they fall
back to `./vigil-data` and can silently show a different or empty store.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent CLI test binds data_dir selection and silent wrong-store fallback.` -->

`vigil events` lists recent detections. `vigil why` walks the selected observation back to its local
clip, detector configuration decision, watch intention, camera, and site context. `vigil stats`
prints local pipeline and acceleration receipts. These commands query the running owner over a Unix
socket when possible and fall back to a direct local read when the store is not owned by the daemon.

<!-- vigil-claim: `vigil.docs-getting-started.vigil-events-lists-recent-detections-vigil-why` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_why_walks_observation_to_clip_to_decision_to_context` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_events_lists_recent_events_newest_first` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_stats_reports_live_pipeline_counters` -->

Commands from the old documentation outline such as `vigil status`, `vigil config check`,
`vigil scan onvif`, `vigil state-at`, and `vigil support-bundle` do not exist and are not valid
instructions.

<!-- vigil-unenforced: classification=future-surface; reason=`The old status, config, scan, replay, and support commands do not exist.` -->

## Where next

- [Configuration](configuration.md) lists the fields the current parser actually accepts.
- [Cameras](cameras.md) explains the RTSP-first camera path.
- [Detection](detection.md) explains motion gating, detector selection, and acceleration receipts.
- [Privacy](privacy.md) explains what is stored and the current network boundary.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This is a navigation list, not an independent runtime promise.` -->
