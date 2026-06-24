# Farm Release Checklist

This checklist is completed on the real Home Assistant host by the operator. Attach command output or screenshots for each section.

## TF-00 Pre-Install Baseline

- Add-on versions:
- Frigate version and status:
- 30-minute detection rate by camera:
- 30-minute aggregate detection rate:
- Recording disk growth:
- MQTT rate on `frigate/#`:
- Home Assistant load:
- Free memory:
- Metrics capture file:

## TF-01 Install

- Local add-on repository copied:
- Install completed without Supervisor errors:
- Supervisor logs attached or summarized:

## TF-02 Health

- Add-on started:
- Health endpoint green:
- Logs visible in Home Assistant:
- Store path shown in logs:

## TF-03 Post-Install Comparison

- Metrics capture file:
- Detection rate within 10 percent of baseline:
- Recording continuity unchanged:
- MQTT rate within 10 percent of baseline:
- Home Assistant load acceptable:
- No recording gaps observed:

## TF-04 Uninstall

- Add-on stopped:
- Add-on uninstalled:
- Owned data removed:
- Container image removed:
- Frigate metrics returned to baseline:

## TF-05 Sign-Off

- Operator initials:
- Timestamp:
- Notes:

## One-camera farm acceptance

- Lower-gate RTSP URL configured directly in Vigil:
- Hard far-gate night-IR event performed at operational distance:
- Event id:
- Durable clip path:
- `vigil events` output attached or summarized:
- `vigil why --latest` output attached or summarized:
- Ambient quiet window duration, minimum 30 minutes:
- Ambient false-positive count:
- Stream fps from `vigil stats`:
- Detector latency p50 from `vigil stats`:
- Detector latency p95 from `vigil stats`:
- Detector latency max from `vigil stats`:
- Processing lag or backlog from `vigil stats`:
- Dropped motion-positive frames from `vigil stats`:
- Keep-pace result, based on bounded backlog and no systematic motion-positive drops:
- Pass/fail notes:

## Home-Assistant integration acceptance walk

Run `tests/ha-os-vm/ha-integration-smoke.sh` with a live farm camera and the HA-OS VM running. Record the receipt for each step. Vigil is not done for this lane until every step passes with the receipt recorded.

**Prerequisites:**
- `local_vigil` add-on installed and running on the HA-OS VM
- Mosquitto broker add-on running; broker reachable
- Synthetic or real farm RTSP camera feed live
- `VIGIL_HEALTH_URL`, `MQTT_HOST`, `MQTT_PORT`, `HA_API_TOKEN`, `HA_API_BASE` set

### HA-S1 Device assembled itself

- Vigil add-on started without errors:
- Vigil device visible in Home Assistant Settings → Devices:
- Per-camera sub-device visible (named after configured camera label, e.g. "lower gate"):
- Entity list on the sub-device (event, image, binary_sensor, camera, operator-action controls):
- No manual YAML pasted:
- Receipt (screenshot or device registry API output):

### HA-S2 Real detection surfaced as event entity

- RTSP camera feed live (or synthetic RTSP source running):
- Detection event entity received a state update in Home Assistant:
- Event payload carries: detection_id, class, confidence, timestamp, evidence_ref, snapshot_ref:
- detection_id value noted (used in HA-S5 and HA-S6):
- Receipt (HA event entity state screenshot or mosquitto_sub output):

### HA-S3 Live tile played

- Camera entity in Home Assistant Lovelace card shows a live feed:
- Stream plays inside Home Assistant (not a host-only black tile):
- go2rtc stream URL resolves from inside HA network namespace:
- Receipt (screenshot of live tile with visible frames):

### HA-S4 Automation fired on detection event

- HA automation targeting the detection event entity created or confirmed:
- Automation triggered on a real detection event (at least one trigger recorded):
- Automation action completed:
- Receipt (HA automation trace or logbook entry):

### HA-S5 Recorded clip reached via evidence reference

- detection_id from HA-S2 used to run `vigil why <id>` inside the add-on:
  ```
  docker exec <vigil-container> vigil why <detection_id>
  ```
- evidence_ref field present in `vigil why` output:
- Clip file path or URL reachable via the evidence_ref:
- Clip is NOT browsable via HA Media panel (no media_source entity for Vigil):
- Receipt (`vigil why` output attached or summarized):

### HA-S6 Correction made from inside Home Assistant

- Vigil event-gallery Lovelace card visible on dashboard (no manual YAML paste):
- Detection from HA-S2 tapped in the correction card:
- Label entered and correction type selected:
- Correction published to broker command topic (card sends `{detection_id, label, correction_type}`):
- correction_type used:
- label used:
- Receipt (correction card interaction screenshot or mosquitto_sub capture):

### HA-S7 `vigil why` lists the correction

- `vigil why <detection_id>` run inside the add-on container after the correction:
  ```
  docker exec <vigil-container> vigil why <detection_id>
  ```
- Correction listed in output with correct label and correction_type:
- Correction still listed after add-on restart (daemon restart durability):
- No outbound network calls beyond the local broker observed:
- Receipt (`vigil why` output attached or summarized):

### HA-S8 Sign-off

- All seven steps passed:
- Operator initials:
- Timestamp:
- Notes:
