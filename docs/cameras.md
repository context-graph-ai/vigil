# Cameras

> **Current support:** Vigil's shipped camera ingest path is manual RTSP. ONVIF discovery,
> Hikvision-specific discovery/configuration, automatic NVR enumeration, zones, and masks are not
> implemented yet.

<!-- vigil-unenforced: classification=future-surface; reason=`ONVIF, vendor discovery, NVR enumeration, zones, and masks are not implemented.` -->

## Direct RTSP

Give each camera a name and an RTSP URL in the `cameras` list. Vigil connects directly to that
endpoint; Frigate is not a runtime dependency or companion.
<!-- vigil-unenforced: classification=product-decision; reason=`Direct RTSP ownership and no Frigate runtime dependency define the product boundary.` -->

```toml
[[cameras]]
name = "front gate"
rtsp_url = "rtsp://camera.local:554/stream"
```

On startup, Vigil records the configured camera as a local site entity. The camera and its RTSP
source survive a store reopen, and restarting with the same configuration does not create a second
camera row.

<!-- vigil-claim: `vigil.docs-cameras.on-startup-vigil-records-the-configured-camera` -->
<!-- enforced by: `vigil-bin::first_light_loop::camera_registered_by_rtsp_url_persists_as_site_entity` -->
<!-- enforced by: `vigil-bin::first_light_loop::camera_survives_store_reopen` -->
<!-- enforced by: `vigil-bin::first_light_loop::reregistration_is_idempotent_no_duplicate` -->

The current implementation identifies an existing camera by display name. That is wrong for a
multi-node installation because a same-named camera can rebind the earlier entity. Before OSS
release, camera identity will use owning node plus RTSP feed; `name` will remain only a display
label.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Display-name camera identity can rebind a same-named feed across nodes.` -->

## Separate analysis and live streams

`rtsp_url` is the stream Vigil decodes and sends through motion gating and detection.
`live_rtsp_url` is the stream Home Assistant's Generic Camera entry displays. When
`live_rtsp_url` is absent, the live view uses `rtsp_url`.

```toml
[[cameras]]
name = "driveway"
rtsp_url = "rtsp://camera.local:554/substream"
live_rtsp_url = "rtsp://camera.local:554/mainstream"
```

For Home Assistant Generic Camera registration, Vigil passes `live_rtsp_url` when it is present and
otherwise falls back to `rtsp_url`.

<!-- vigil-claim: `vigil.docs-cameras.this-lets-a-deployment-analyze-a-lowerbandwidth` -->
<!-- enforced by: `vigil::runtime::tests::generic_camera_url_prefers_live_rtsp_url_over_detection_rtsp_url` -->
<!-- enforced by: `vigil::runtime::tests::generic_camera_url_falls_back_to_detection_rtsp_url_for_single_stream_cameras` -->

This selection lets a deployment analyze a lower-bandwidth stream while showing the camera's main
stream in Home Assistant. Vigil is not intended to proxy or transcode that live URL for Generic
Camera; in particular, an H.265 source may fail browser WebRTC negotiation. The current URL-selection
tests do not enforce that no-proxy boundary.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The Generic Camera URL-selection tests do not prove that Vigil never proxies or transcodes the selected live stream.` -->

## USB, CSI, and MJPEG source fields

Every `[[cameras]]` entry declares exactly one of four source kinds: `rtsp_url` (native RTSP,
`rtsp://` or `rtsps://`), `usb_device` (a USB/UVC camera), `csi_module` (a MIPI CSI-2 camera), or
`mjpeg_url` (an HTTP/HTTPS multipart-MJPEG camera, the ESP32-CAM class of device). `usb_device` and
`csi_module` must be a durable hardware identity — a vendor:product:serial string or a stable module
identity — never a transient `/dev/...` device path, because a transient path is not guaranteed to
name the same physical camera across a reboot or a USB re-enumeration.
The add-on options schema and this file's TOML surface both accept all four fields today. Filling in
`usb_device`, `csi_module`, or `mjpeg_url` is **rejected when Vigil loads its configuration**, before
any camera starts: this artifact carries no USB, CSI, or MJPEG capture/encode path yet (the shared
encoder seam in `crate::encode` has no producer wired to it), so the load fails loud, naming the
camera and the field, with the text `unsupported_by_this_artifact` — the identical honest-rejection
shape a well-formed value this artifact merely cannot carry always gets, never conflated with a
malformed value. Configure `rtsp_url` today; a USB/CSI/MJPEG capture path is future work.

<!-- vigil-claim: `vigil.docs-cameras.every-cameras-entry-declares-exactly-one-of` -->
<!-- enforced by: `vigil::camera_config_schema::every_adapter_source_kind_is_honestly_rejected_by_the_real_load_as_unsupported_by_this_artifact` -->
<!-- enforced by: `vigil::camera_config_schema::usb_and_csi_identity_would_be_durable_hardware_identity_not_a_transient_device_path` -->
<!-- enforced by: `vigil::camera_config_schema::a_transient_dev_video_path_for_usb_device_is_the_real_loads_invalid_outcome` -->
<!-- enforced by: `vigil::camera_config_schema::a_well_formed_but_unsupported_mjpeg_url_is_the_real_loads_unavailable_outcome_not_invalid` -->
<!-- enforced by: `vigil::addon_config_surface::addon_config_camera_schema_exposes_usb_csi_and_mjpeg_source_fields` -->

## Credentials

Credentials can be embedded in an RTSP URL or supplied as separate `username` and `password`
fields. Separate fields take precedence over URL user information. Runtime logs and parse errors
redact URL credentials.

```toml
[[cameras]]
name = "side gate"
rtsp_url = "rtsp://camera.local:554/stream"
username = "vigil"
password = "replace-me"
```

<!-- vigil-claim: `vigil.docs-cameras.toml-cameras-name-side-gate-rtspurl-rtspcameralocal554stream` -->
<!-- enforced by: `vigil-bin::first_light_loop::separate_rtsp_credentials_authenticate_without_url_userinfo` -->
<!-- enforced by: `vigil-bin::first_light_loop::credentialed_rtsp_url_authenticates_and_redacts_runtime_surface` -->
<!-- enforced by: `vigil::media_pipeline::tests::explicit_rtsp_credentials_override_url_userinfo` -->
<!-- enforced by: `vigil::media_pipeline::tests::rtsp_url_redaction_removes_the_password_but_keeps_the_username_in_parse_errors` -->

Do not put a password on the command line. The current binary still accepts a legacy
`--rtsp-password` flag, but the release contract removes it because process arguments can be read
by other software on the host. Add-on options, TOML, and an environment variable avoid argv
exposure, but they store or carry a plain string rather than placing it in a secret vault. Restrict
access to those configuration surfaces.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`The legacy password argv flag exposes secrets through host process arguments.` -->

## Multiple cameras

Add one `[[cameras]]` entry per stream. The runtime code loops over configured cameras and starts an
ingest path for each enabled entry with an RTSP URL, but the pre-OSS suite does not yet contain a
deterministic two-camera runtime acceptance. Multi-camera Home Assistant discovery does generate
stable per-camera topics and identifiers.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No deterministic two-camera runtime acceptance proves both configured ingest paths.` -->

The add-on uses the equivalent JSON/YAML list:

```yaml
cameras:
  - name: front_gate
    rtsp_url: rtsp://front-gate.local:554/stream
  - name: parking
    rtsp_url: rtsp://parking.local:554/stream
```

<!-- vigil-unenforced: classification=documentation-gap; reason=`No direct add-on options parser test binds this multi-camera JSON/YAML example.` -->

Repeated Home Assistant discovery generation derives stable entity identifiers and topics from the
configured camera names.

<!-- vigil-claim: `vigil.docs-cameras.yaml-cameras-name-frontgate-rtspurl-rtspfrontgatelocal554stream-name` -->
<!-- enforced by: `vigil-ha::ha_discovery::tests::entity_ids_and_topics_stable_across_regeneration` -->

Camera ordering is not a user contract. Use names to identify cameras in Home Assistant and review
output.
<!-- vigil-unenforced: classification=product-decision; reason=`Camera names rather than list ordering are the approved operator identity convention.` -->

## Enable and disable a camera

With MQTT/Home Assistant integration active, each camera has an enabled switch. Enable/disable
commands update the live processing flag for only the named camera and publish the reflected state.

<!-- vigil-claim: `vigil.docs-cameras.with-mqtthome-assistant-integration-active-each-camera` -->
<!-- enforced by: `vigil-ha::ha_mqtt_broker::camera_enabled_switch_state_is_retained_and_updates_on_control_commands` -->

The current runtime also writes a local disabled marker and reads it on startup, but no end-to-end
test proves the restart behavior yet. That persistence claim remains untagged until the test exists.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No restart acceptance proves the local disabled marker is restored into live state.` -->

There is no supported camera-management CLI yet.

<!-- vigil-unenforced: classification=future-surface; reason=`A supported camera-management CLI has not been designed or implemented.` -->

## Reconnect behavior

A transient RTSP drop changes health, retries with shutdown-aware backoff, and can recover without
losing the camera entity. A successful reconnect restores ingest health.

<!-- vigil-claim: `vigil.docs-cameras.a-transient-rtsp-drop-changes-health-retries` -->
<!-- enforced by: `vigil-bin::first_light_loop::transient_rtsp_drop_recovers_without_losing_camera` -->

If a source is empty or undecodable, Vigil must not fabricate a detection event.

<!-- vigil-claim: `vigil.docs-cameras.if-a-source-is-empty-or-undecodable` -->
<!-- enforced by: `vigil-bin::first_light_loop::empty_or_undecodable_stream_lands_no_event` -->

Pre-OSS work adds an explicit Home Assistant coverage signal so “nothing happened” can be
distinguished from “Vigil could not analyze the stream.”

<!-- vigil-unenforced: classification=future-surface; reason=`The explicit Home Assistant analysis-coverage signal is not implemented yet.` -->

## ONVIF discovery

**Not yet available.** There is no `vigil scan onvif` command and no ONVIF discovery implementation.
Use a known RTSP URL.

<!-- vigil-unenforced: classification=future-surface; reason=`ONVIF scanning and discovery commands are not implemented or acceptance-tested.` -->

## Hikvision and NVR-fronted cameras

**No vendor-specific path is implemented.** A Hikvision camera or NVR channel can be used only when
the operator already knows a compatible RTSP endpoint and credentials. Vigil currently treats it
as an ordinary RTSP source; it does not discover channels, read vendor events, or configure the
device.

<!-- vigil-unenforced: classification=future-surface; reason=`Vendor-specific Hikvision and NVR discovery/configuration paths do not exist.` -->

## Zones and masks

Detection event payloads currently carry no `zone` field, and the add-on accepts no `zones` or
`masks` configuration block.

<!-- vigil-claim: `vigil.docs-cameras.zones-and-masks-are-not-yet-available` -->
<!-- enforced by: `vigil::source_scan_contract::addon_config_exposes_recognition_options_and_schema` -->
<!-- enforced by: `vigil-ha::ha_discovery::tests::detection_event_payload_carries_current_contract_without_future_zone_field` -->

Polygon-zone matching, privacy and motion masks, precedence, and per-zone rules are future release
work rather than current operator surfaces.

<!-- vigil-unenforced: classification=future-surface; reason=`No deterministic runtime contract covers polygon matching, masks, precedence, or per-zone rules because those surfaces are not implemented.` -->
