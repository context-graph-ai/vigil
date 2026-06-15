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
