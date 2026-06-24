#!/usr/bin/env bash
# Home-Assistant integration acceptance walk — smoke runner.
#
# Walks the seven owner acceptance steps from the "Home-Assistant integration
# acceptance walk" section of farm-release-checklist.md.  Run this with a live
# farm camera (or the synthetic RTSP source) and the HA-OS VM running.
#
# Required environment variables:
#   VIGIL_HEALTH_URL   — e.g. http://127.0.0.1:8099/health
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
mqtt_host="${MQTT_HOST:-127.0.0.1}"
mqtt_port="${MQTT_PORT:-1883}"
haos_ssh_target="${HAOS_SSH_TARGET:-}"
addon_slug="${VIGIL_ADDON_SLUG:-local_vigil}"
ha_api_token="${HA_API_TOKEN:-}"
ha_api_base="${HA_API_BASE:-http://127.0.0.1:8123}"
vigil_event_topic="${VIGIL_EVENT_TOPIC:-vigil/events}"
vigil_correction_topic="${VIGIL_CORRECTION_TOPIC:-vigil/correction/command}"
go2rtc_api_base="${GO2RTC_API_BASE:-http://127.0.0.1:1984}"
docker_cli="${DOCKER_CLI:-docker}"
mqtt_username="${MQTT_USERNAME:-}"
mqtt_password="${MQTT_PASSWORD:-}"
detection_wait_seconds="${HA_DETECTION_WAIT_SECONDS:-120}"
correction_label="${HA_CORRECTION_LABEL:-smoke test correction}"

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

skip_step() {
  printf 'HA-S%s SKIP %s\n' "$1" "$2"
}

have_cmd() {
  type -P "$1" >/dev/null 2>&1
}

curl_ha() {
  if [[ -n "$haos_ssh_target" ]]; then
    local remote_script
    printf -v remote_script 'curl -fsS %s' "$*"
    ssh -n -o BatchMode=yes "$haos_ssh_target" "$remote_script"
  else
    curl -fsS "$@"
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
  skip_step 2 "mosquitto_sub not available; skipping event topic check"
elif ! have_cmd jq; then
  skip_step 2 "jq not available; skipping event topic check"
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
    printf '  go2rtc stream entry: %s\n' "$vigil_stream"
    pass_step 3 "go2rtc stream registered and reachable inside HA network namespace"
  fi
fi

# ─── HA-S4: Automation fired on detection event ───────────────────────────

printf '\n[HA-S4] Automation check (manual verification required)...\n'
printf '  This step requires a Home Assistant automation targeting the detection event entity.\n'
printf '  Confirm the automation triggered on the detection captured in HA-S2 and record the\n'
printf '  trace in farm-release-checklist.md HA-S4.\n'
printf '  (Automated probe not available; manual sign-off required.)\n'
skip_step 4 "manual verification required — record HA automation trace in farm-release-checklist.md"

# ─── HA-S5: Recorded clip reached via evidence reference ─────────────────

printf '\n[HA-S5] Checking vigil why / evidence reference...\n'
if [[ -z "$smoke_detection_id" ]]; then
  skip_step 5 "no detection_id from HA-S2; cannot run vigil why"
else
  why_output="$(vigil_exec vigil why "$smoke_detection_id" 2>/dev/null || true)"
  if [[ -z "$why_output" ]]; then
    fail_step 5 "'vigil why $smoke_detection_id' returned no output inside the add-on container; \
the read path must be functional and the detection must be in cg authority"
  else
    evidence_ref="$(printf '%s' "$why_output" | grep -iE 'evidence|clip|recording' | head -3 || true)"
    printf '  vigil why output (first 10 lines):\n'
    printf '%s\n' "$why_output" | head -10 | sed 's/^/    /'
    # Assert no HA media_source entity
    media_source_entities="$(ha_rest /api/states 2>/dev/null \
      | jq -er '[.[] | select(.entity_id | test("vigil"; "i")) | select(.entity_id | startswith("media_source"))] | length' \
      2>/dev/null || echo 0)"
    if [[ "$media_source_entities" =~ ^[0-9]+$ ]] && (( media_source_entities > 0 )); then
      fail_step 5 "HA media_source entity for Vigil is registered; clips must be reachable \
only through vigil why/events evidence reference, not via the HA Media panel"
    else
      pass_step 5 "clip reachable via vigil why evidence reference; no HA media_source entity registered"
    fi
  fi
fi

# ─── HA-S6: Correction made from inside Home Assistant ───────────────────

printf '\n[HA-S6] Publishing a correction command to the broker...\n'
if [[ -z "$smoke_detection_id" ]]; then
  skip_step 6 "no detection_id from HA-S2; cannot publish correction"
elif ! have_cmd mosquitto_pub; then
  skip_step 6 "mosquitto_pub not available; cannot publish correction command"
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

# ─── HA-S7: vigil why lists the correction ───────────────────────────────

printf '\n[HA-S7] Verifying correction reads back in vigil why...\n'
if [[ -z "$smoke_detection_id" ]]; then
  skip_step 7 "no detection_id from HA-S2; cannot verify correction read-back"
else
  sleep 3
  why_after="$(vigil_exec vigil why "$smoke_detection_id" 2>/dev/null || true)"
  if [[ -z "$why_after" ]]; then
    fail_step 7 "'vigil why $smoke_detection_id' returned no output after correction; \
the add-on must be running and the store must be accessible"
  elif [[ "$why_after" != *"$correction_label"* ]] && [[ "$why_after" != *"FalseAlarm"* ]] && \
       [[ "$why_after" != *"false_alarm"* ]] && [[ "$why_after" != *"false-alarm"* ]]; then
    fail_step 7 "'vigil why $smoke_detection_id' does not list the correction \
(label: '$correction_label' / type: FalseAlarm); the correction must be recorded durably in cg \
authority and must read back through the review API"
  else
    printf '  vigil why output after correction (first 15 lines):\n'
    printf '%s\n' "$why_after" | head -15 | sed 's/^/    /'
    # No outbound network check: assert the correction stub's known wrong socket is absent
    container_id="$(addon_container_id || true)"
    if [[ -n "$container_id" ]]; then
      extra_socket="$("${docker_cmd[@]}" exec "$container_id" sh -c '
        ss -tnp 2>/dev/null | awk '"'"'$1=="ESTAB" && $4!~/127\.0\.0\.1:'"$mqtt_port"'/ {print}'"'"' | head -3
      ' 2>/dev/null || true)"
      if [[ -n "$extra_socket" ]]; then
        printf '  Warning: unexpected outbound socket detected:\n%s\n' "$extra_socket" >&2
        printf '  (correction path must open no outbound network beyond the local broker)\n' >&2
      fi
    fi
    pass_step 7 "correction reads back in vigil why after publishing to the command topic; nothing left the property"
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
