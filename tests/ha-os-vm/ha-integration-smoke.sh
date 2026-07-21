#!/usr/bin/env bash
# Home-Assistant integration acceptance walk — smoke runner.
#
# Walks the seven owner acceptance steps from the "Home-Assistant integration
# acceptance walk" section of farm-release-checklist.md.  Run this with a live
# farm camera (or the synthetic RTSP source) and the HA-OS VM running.
#
# Required environment variables:
#   VIGIL_HEALTH_URL   — e.g. http://127.0.0.1:8099/health
#   VIGIL_REVIEW_URL   — e.g. http://127.0.0.1:8098
#   MQTT_HOST          — broker hostname/IP
#   MQTT_PORT          — broker port (default 1883)
#   HAOS_SSH_TARGET    — ssh target for the HA-OS VM (optional; if not set,
#                        commands run locally assuming HA is reachable)
#   VIGIL_ADDON_SLUG   — add-on slug (default: local_vigil)
#   HA_API_TOKEN       — Home Assistant long-lived access token (for REST API)
#   HA_API_BASE        — e.g. http://127.0.0.1:8123 (default)
#   VIGIL_EVENT_TOPIC  — MQTT event topic (default: vigil/events)
#   VIGIL_CORRECTION_TOPIC — MQTT correction command topic
#   GO2RTC_API_BASE    — go2rtc REST API base (default: http://127.0.0.1:1984)
#   DOCKER_CLI         — docker CLI command (default: docker)
#   HA_S4_MANUAL_PROOF — exported HA automation trace JSON for the HA-S2 detection
#   HA_S5_MEDIA_BROWSE_PROOF — exported media_source/browse_media root response JSON
#
# Optional:
#   MQTT_USERNAME / MQTT_PASSWORD  — broker credentials
#   HA_DETECTION_WAIT_SECONDS      — seconds to wait for a detection (default 120)
#   HA_CORRECTION_LABEL            — correction label to record (default: smoke test correction)
set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

status=0
smoke_detection_id=""

health_url="${VIGIL_HEALTH_URL:-http://127.0.0.1:8099/health}"
review_url="${VIGIL_REVIEW_URL:-http://127.0.0.1:8098}"
mqtt_host="${MQTT_HOST:-127.0.0.1}"
mqtt_port="${MQTT_PORT:-1883}"
haos_ssh_target="${HAOS_SSH_TARGET:-}"
addon_slug="${VIGIL_ADDON_SLUG:-local_vigil}"
ha_api_token="${HA_API_TOKEN:-}"
ha_api_base="${HA_API_BASE:-http://127.0.0.1:8123}"
vigil_event_topic="${VIGIL_EVENT_TOPIC:-vigil/events}"
vigil_correction_topic="${VIGIL_CORRECTION_TOPIC:-vigil/commands/correct}"
go2rtc_api_base="${GO2RTC_API_BASE:-http://127.0.0.1:1984}"
docker_cli="${DOCKER_CLI:-docker}"
mqtt_username="${MQTT_USERNAME:-}"
mqtt_password="${MQTT_PASSWORD:-}"
detection_wait_seconds="${HA_DETECTION_WAIT_SECONDS:-120}"
correction_label="${HA_CORRECTION_LABEL:-smoke-test-correction}-$$-$RANDOM"

mosquitto_auth_args=()
if [[ -n "$mqtt_username" ]]; then
  mosquitto_auth_args+=("-u" "$mqtt_username")
fi
if [[ -n "$mqtt_password" ]]; then
  mosquitto_auth_args+=("-P" "$mqtt_password")
fi

read -r -a docker_cmd <<< "$docker_cli"

pass_step() {
  printf 'HA-S%s PASS %s\n' "$1" "$2"
}

fail_step() {
  printf 'HA-S%s FAIL %s\n' "$1" "$2" >&2
  status=1
}

not_run_step() {
  printf 'HA-S%s NOT-RUN %s\n' "$1" "$2" >&2
  status=1
}

have_cmd() {
  type -P "$1" >/dev/null 2>&1
}

curl_ha() {
  if [[ -n "$haos_ssh_target" ]]; then
    local quoted
    printf -v quoted '%q ' "$@"
    ssh -n -o BatchMode=yes "$haos_ssh_target" "curl -fsS $quoted"
  else
    curl -fsS "$@"
  fi
}

review_curl_available() {
  if [[ -n "$haos_ssh_target" ]]; then
    have_cmd ssh \
      && ssh -n -o BatchMode=yes "$haos_ssh_target" 'command -v curl >/dev/null 2>&1'
  else
    have_cmd curl
  fi
}

ha_rest() {
  local path="$1"
  if [[ -n "$haos_ssh_target" ]]; then
    local remote_script
    printf -v remote_script \
      'curl -fsS -H "Authorization: Bearer $SUPERVISOR_TOKEN" "http://supervisor/ha%s"' "$path"
    ssh -n -o BatchMode=yes "$haos_ssh_target" \
      "sudo docker exec hassio_cli sh -c $(printf '%q' "$remote_script")"
  else
    curl -fsS -H "Authorization: Bearer ${ha_api_token}" "${ha_api_base}${path}"
  fi
}

addon_container_id() {
  "${docker_cmd[@]}" ps -a --format '{{.ID}}\t{{.Names}}' 2>/dev/null | awk -v slug="$addon_slug" '
    {
      name=tolower($2); slug_l=tolower(slug)
      if (index(name, slug_l) > 0) { print $1; exit }
    }
  '
}

vigil_exec() {
  local container_id
  container_id="$(addon_container_id)" || return 1
  [[ -n "$container_id" ]] || return 1
  "${docker_cmd[@]}" exec "$container_id" "$@"
}

health_ok() {
  curl_ha --max-time 5 "$health_url" >/dev/null 2>&1
}

wait_for_health() {
  local deadline=$((SECONDS + 60))
  while (( SECONDS < deadline )); do
    health_ok && return 0
    sleep 2
  done
  return 1
}

mqtt_wait_for_message() {
  local topic="$1" wait="$2"
  local capture sub_err
  capture="$(mktemp)"
  sub_err="$(mktemp)"
  timeout "$wait" mosquitto_sub -h "$mqtt_host" -p "$mqtt_port" \
    "${mosquitto_auth_args[@]}" -C 1 -W "$wait" -t "$topic" \
    > "$capture" 2>"$sub_err"
  cat "$capture"
  rm -f "$capture" "$sub_err"
}

printf '\n=== Home-Assistant integration acceptance walk ===\n\n'

# ─── HA-S1: Device assembled itself ──────────────────────────────────────

printf '[HA-S1] Checking Vigil device registration in Home Assistant...\n'
if ! wait_for_health; then
  fail_step 1 "Vigil health endpoint at $health_url did not reach 200; add-on must be running"
else
  vigil_entity_ids="$(ha_rest /api/states 2>/dev/null \
    | jq -er '[.[] | select(.entity_id | test("vigil"; "i"))] | .[].entity_id' 2>/dev/null \
    | sort -u || true)"
  if [[ -z "$vigil_entity_ids" ]]; then
    fail_step 1 "no Vigil entities found in HA state machine; discovery payloads must be published \
on add-on start so HA auto-registers the device and per-camera sub-device"
  else
    camera_entities="$(printf '%s\n' "$vigil_entity_ids" | grep -cE '(camera|event)\.' || echo 0)"
    if (( camera_entities == 0 )); then
      fail_step 1 "Vigil entities registered but no camera/event entity found; \
per-camera sub-device must carry detection event and camera entities"
    else
      printf '  Vigil entity ids:\n'
      printf '%s\n' "$vigil_entity_ids" | sed 's/^/    /'
      pass_step 1 "device and per-camera sub-device registered in Home Assistant"
    fi
  fi
fi

# ─── HA-S2: Real detection surfaced as event entity ───────────────────────

printf '\n[HA-S2] Waiting for a real detection event on the event topic...\n'
if ! have_cmd mosquitto_sub; then
  not_run_step 2 "mosquitto_sub is required for the event-topic acceptance check"
elif ! have_cmd jq; then
  not_run_step 2 "jq is required to validate the detection-event contract"
else
  printf '  Waiting up to %ss for a detection event on %s...\n' \
    "$detection_wait_seconds" "$vigil_event_topic"
  event_payload="$(mqtt_wait_for_message "$vigil_event_topic" "$detection_wait_seconds")"
  if [[ -z "$event_payload" ]]; then
    fail_step 2 "no detection event on '$vigil_event_topic' within ${detection_wait_seconds}s; \
ensure the RTSP camera feed is live and producing detectable content"
  else
    smoke_detection_id="$(jq -er '.detection_id // empty' <<< "$event_payload" 2>/dev/null || true)"
    detection_class="$(jq -er '.class // .object_class // empty' <<< "$event_payload" 2>/dev/null || true)"
    detection_confidence="$(jq -er '.confidence // empty' <<< "$event_payload" 2>/dev/null || true)"
    if [[ -z "$smoke_detection_id" ]]; then
      fail_step 2 "detection event payload is missing detection_id; event contract is broken"
    else
      printf '  detection_id: %s\n' "$smoke_detection_id"
      printf '  class: %s  confidence: %s\n' "$detection_class" "$detection_confidence"
      pass_step 2 "real detection surfaced as MQTT event entity carrying the required contract fields"
    fi
  fi
fi

# ─── HA-S3: Live tile played ──────────────────────────────────────────────

printf '\n[HA-S3] Checking go2rtc live stream reachability...\n'
go2rtc_streams="$(curl_ha --max-time 5 "${go2rtc_api_base}/api/streams" 2>/dev/null || true)"
if [[ -z "$go2rtc_streams" ]]; then
  fail_step 3 "go2rtc REST API at ${go2rtc_api_base}/api/streams is not reachable; \
the camera entity must be wired to go2rtc with a stream resolving inside HA's network namespace"
else
  vigil_stream="$(printf '%s' "$go2rtc_streams" \
    | grep -i 'vigil\|lower.gate\|camera' | head -1 || true)"
  if [[ -z "$vigil_stream" ]]; then
    fail_step 3 "go2rtc is reachable but no Vigil stream is registered; \
the camera entity stream source must be registered with go2rtc so the live tile renders inside HA"
  else
    camera_entity="$(ha_rest /api/states 2>/dev/null \
      | jq -er '[.[] | select(.entity_id | startswith("camera.")) | select(.entity_id | test("vigil"; "i"))][0].entity_id // empty' \
        2>/dev/null || true)"
    if [[ -z "$camera_entity" ]]; then
      fail_step 3 "go2rtc has a Vigil stream but Home Assistant has no Vigil camera entity to play it"
    else
      first_frame_sha="$(ha_rest "/api/camera_proxy/${camera_entity}" 2>/dev/null | sha256sum | awk '{print $1}' || true)"
      sleep 2
      second_frame_sha="$(ha_rest "/api/camera_proxy/${camera_entity}" 2>/dev/null | sha256sum | awk '{print $1}' || true)"
      if ! [[ "$first_frame_sha" =~ ^[0-9a-f]{64}$ && "$second_frame_sha" =~ ^[0-9a-f]{64}$ ]]; then
        fail_step 3 "Home Assistant camera_proxy returned no hashable frames for ${camera_entity}"
      elif [[ "$first_frame_sha" == "$second_frame_sha" ]]; then
        fail_step 3 "Home Assistant camera_proxy returned the same frame twice for ${camera_entity}; stream registration alone does not prove live playback"
      else
        printf '  go2rtc stream entry: %s\n' "$vigil_stream"
        pass_step 3 "Home Assistant camera entity delivered two distinct live frames from the registered go2rtc stream"
      fi
    fi
  fi
fi

# ─── HA-S4: Automation fired on detection event ───────────────────────────

printf '\n[HA-S4] Automation check (manual verification required)...\n'
printf '  This step requires a Home Assistant automation targeting the detection event entity.\n'
printf '  Confirm the automation triggered on the detection captured in HA-S2 and record the\n'
printf '  trace in farm-release-checklist.md HA-S4.\n'
manual_proof="${HA_S4_MANUAL_PROOF:-}"
if [[ -z "$manual_proof" ]] || [[ ! -s "$manual_proof" ]]; then
  not_run_step 4 "set HA_S4_MANUAL_PROOF to an exported HA automation trace for the HA-S2 detection"
elif [[ -z "$smoke_detection_id" ]]; then
  not_run_step 4 "HA-S2 produced no detection_id, so the automation trace cannot be causally matched"
elif ! have_cmd jq; then
  not_run_step 4 "jq is required to validate the exported automation trace"
elif ! jq -e --arg detection_id "$smoke_detection_id" '
    .trace.state == "stopped"
    and .trace.script_execution == "finished"
    and ([.. | strings] | index($detection_id) != null)
  ' "$manual_proof" >/dev/null 2>&1; then
  fail_step 4 "automation trace must be valid JSON with trace.state=stopped, trace.script_execution=finished, and the exact HA-S2 detection_id"
else
  printf '  Automation trace receipt: %s\n' "$manual_proof"
  pass_step 4 "exported HA automation trace completed successfully for the exact HA-S2 detection"
fi

# ─── HA-S5: Recorded clip reached via evidence reference ─────────────────

printf '\n[HA-S5] Checking vigil why / evidence reference...\n'
if [[ -z "$smoke_detection_id" ]]; then
  not_run_step 5 "HA-S2 produced no detection_id, so evidence lookup cannot run"
elif ! have_cmd jq; then
  not_run_step 5 "jq is required to validate the /why evidence reference"
elif ! review_curl_available; then
  not_run_step 5 "curl access to the Vigil review data plane is required"
else
  why_json="$(curl_ha --max-time 5 "${review_url%/}/why/$smoke_detection_id" 2>/dev/null || true)"
  evidence_ref="$(jq -er '[.. | objects | .evidence_ref? // empty | strings | select(length > 0)][0] // empty' \
    <<< "$why_json" 2>/dev/null || true)"
  if [[ -z "$why_json" ]]; then
    fail_step 5 "GET ${review_url%/}/why/$smoke_detection_id returned no JSON; the detection must be in cg authority"
  elif [[ -z "$evidence_ref" ]]; then
    fail_step 5 "GET /why/$smoke_detection_id returned no non-empty evidence_ref"
  elif [[ "$evidence_ref" == http://* || "$evidence_ref" == https://* ]]; then
    fail_step 5 "evidence_ref must remain a same-origin review path, not an external URL: $evidence_ref"
  elif ! curl_ha -fsS --max-time 10 -o /dev/null "${review_url%/}/${evidence_ref#/}"; then
    fail_step 5 "evidence_ref '$evidence_ref' was not fetchable from the shipped review data plane"
  else
    printf '  fetchable evidence_ref: %s\n' "$evidence_ref"
    media_browse_proof="${HA_S5_MEDIA_BROWSE_PROOF:-}"
    if [[ -z "$media_browse_proof" ]] || [[ ! -s "$media_browse_proof" ]]; then
      not_run_step 5 "set HA_S5_MEDIA_BROWSE_PROOF to the exported Home Assistant media_source/browse_media root response"
    elif ! jq -e '
        .success == true
        and .result.media_content_id == "media-source://media_source"
        and ([.. | strings | select(test("vigil"; "i"))] | length == 0)
      ' "$media_browse_proof" >/dev/null 2>&1; then
      fail_step 5 "HA media-source browse proof must be a successful root response with no Vigil entry"
    else
      pass_step 5 "clip bytes are reachable through the /why evidence_ref and the HA Media root contains no Vigil source"
    fi
  fi
fi

# ─── HA-S6: Correction made from inside Home Assistant ───────────────────

printf '\n[HA-S6] Publishing a correction command to the broker...\n'
if [[ -z "$smoke_detection_id" ]]; then
  not_run_step 6 "HA-S2 produced no detection_id, so correction publishing cannot run"
elif ! have_cmd mosquitto_pub; then
  not_run_step 6 "mosquitto_pub is required for the correction-command acceptance check"
else
  correction_payload="$(printf '{"detection_id":"%s","label":"%s","correction_type":"FalseAlarm"}' \
    "$smoke_detection_id" "$correction_label")"
  if mosquitto_pub -h "$mqtt_host" -p "$mqtt_port" "${mosquitto_auth_args[@]}" \
      -t "$vigil_correction_topic" -m "$correction_payload" >/dev/null 2>&1; then
    printf '  Correction published:\n    topic: %s\n    payload: %s\n' \
      "$vigil_correction_topic" "$correction_payload"
    pass_step 6 "correction command published to broker command topic"
  else
    fail_step 6 "mosquitto_pub to '$vigil_correction_topic' failed; \
broker must be reachable and the correction command topic must be subscribed by Vigil"
  fi
fi

# ─── HA-S7: review JSON lists the correction ─────────────────────────────

printf '\n[HA-S7] Verifying correction reads back through the review API...\n'
if [[ -z "$smoke_detection_id" ]]; then
  not_run_step 7 "HA-S2 produced no detection_id, so correction read-back cannot run"
elif ! have_cmd jq; then
  not_run_step 7 "jq is required to validate the exact correction label and type"
elif ! review_curl_available; then
  not_run_step 7 "curl access to the Vigil review data plane is required"
else
  why_json=""
  correction_deadline=$((SECONDS + 10))
  while (( SECONDS < correction_deadline )); do
    why_json="$(curl_ha --max-time 5 \
      "${review_url%/}/why/$smoke_detection_id" 2>/dev/null || true)"
    if jq -e --arg label "$correction_label" --arg detection_id "$smoke_detection_id" \
      'any(.corrections[]?; .label == $label and .correction_type == "FalseAlarm" and .anchored_detection_id == $detection_id)' \
      <<< "$why_json" >/dev/null 2>&1; then
      break
    fi
    sleep 0.1
  done
  if [[ -z "$why_json" ]]; then
    fail_step 7 "GET ${review_url%/}/why/$smoke_detection_id returned no JSON after correction; \
the review data plane must be reachable and the detection must exist in cg authority"
  elif ! jq -e --arg label "$correction_label" --arg detection_id "$smoke_detection_id" \
    'any(.corrections[]?; .label == $label and .correction_type == "FalseAlarm" and .anchored_detection_id == $detection_id)' \
    <<< "$why_json" >/dev/null 2>&1; then
    fail_step 7 "GET /why/$smoke_detection_id did not return the exact anchored correction \
(label: '$correction_label' / correction_type: FalseAlarm / anchored_detection_id: '$smoke_detection_id') in .corrections"
  else
    printf '  Matching /why correction: '
    jq -c --arg label "$correction_label" --arg detection_id "$smoke_detection_id" \
      '.corrections[] | select(.label == $label and .correction_type == "FalseAlarm" and .anchored_detection_id == $detection_id)' \
      <<< "$why_json" | head -1
    pass_step 7 "the exact anchored correction object read back from cg through /why JSON; TH-23 separately owns correction-writer egress proof"
  fi
fi

# ─── Summary ─────────────────────────────────────────────────────────────

printf '\n=== Smoke walk complete ===\n'
if (( status == 0 )); then
  printf 'ALL STEPS PASSED — record the receipts in farm-release-checklist.md HA-S1..HA-S7\n'
else
  printf 'ONE OR MORE STEPS FAILED — see HA-Sn FAIL lines above\n' >&2
fi

exit "$status"
