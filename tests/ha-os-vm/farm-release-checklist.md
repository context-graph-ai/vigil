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
- `HA_S4_MANUAL_PROOF` points to the exported automation trace JSON
- `HA_S5_MEDIA_BROWSE_PROOF` points to the exported HA media-source root response JSON

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
- Camera entity id used by `camera_proxy`:
- First and second frame SHA-256 values are both present and distinct:
- Receipt (script output plus screenshot of live tile with visible frames):

### HA-S4 Automation fired on detection event

- HA automation targeting the detection event entity created or confirmed:
- Automation triggered on a real detection event (at least one trigger recorded):
- Automation action completed:
- Exported trace is JSON with `trace.state=stopped` and `trace.script_execution=finished`:
- Exported trace contains the exact HA-S2 `detection_id`:
- `HA_S4_MANUAL_PROOF` path:
- Receipt (exported HA automation trace):

### HA-S5 Recorded clip reached via evidence reference

- detection_id from HA-S2 used to query the shipped review endpoint:
  ```
  curl -fsS "$VIGIL_REVIEW_URL/why/<detection_id>" | jq .
  ```
- Nonempty same-origin `evidence_ref` present in `/why` JSON:
- Evidence URL returned bytes through the shipped review data plane:
- Exported `media_source/browse_media` root response is successful and contains no Vigil entry:
- `HA_S5_MEDIA_BROWSE_PROOF` path:
- Receipt (`/why` JSON, evidence fetch, and HA media browse response):

### HA-S6 Correction made from inside Home Assistant

- Advanced Camera Card Vigil view visible on the dashboard:
- Detection from HA-S2 tapped in the Advanced Camera Card correction UI:
- Label entered and correction type selected:
- Correction published to broker command topic (card sends `{detection_id, label, correction_type}`):
- correction_type used:
- label used:
- Receipt (correction card interaction screenshot or mosquitto_sub capture):

### HA-S7 `/why/<detection_id>` JSON lists the correction

- Shipped review endpoint queried after the correction:
  ```
  curl -fsS "$VIGIL_REVIEW_URL/why/<detection_id>" | jq .
  ```
- `.corrections` contains the exact label, `correction_type`, and `anchored_detection_id`:
- Correction still listed after add-on restart (daemon restart durability):
- Receipt (`/why` JSON attached or summarized):

### TH-23 Correction writer makes no network syscall

- Exact physical command run:
  ```
  TH_RUN_LIST=TH-23 tests/ha-os-vm/run-th-suite.sh
  ```
- Real strace positive control captured the deliberately refused loopback connection:
- Exactly one `vigil-correct` writer was attached before correction publication:
- Unique correction fingerprint received a `status=landed` writer receipt and its exact label/type/anchor read back through `/why/<detection_id>` JSON:
- The attached writer emitted zero network syscalls through read-back:
- Full `TH-23 PASS` output attached:

### HA-S8 Sign-off

- All seven steps passed:
- TH-23 correction-writer egress receipt passed:
- Operator initials:
- Timestamp:
- Notes:
