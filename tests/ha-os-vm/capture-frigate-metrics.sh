#!/usr/bin/env bash
set -euo pipefail

frigate_url="${FRIGATE_URL:-http://127.0.0.1:5000}"
mqtt_host="${MQTT_HOST:-127.0.0.1}"
mqtt_port="${MQTT_PORT:-1883}"
ha_cli="${HA_CLI:-ha}"
addon_slug="${VIGIL_ADDON_SLUG:-local_vigil}"
window_seconds="${WINDOW_SECONDS:-1800}"
output="${1:-frigate-metrics.txt}"
read -r -a ha_cmd <<< "$ha_cli"

type -P jq >/dev/null 2>&1 || {
  printf 'jq is required for Frigate metric capture\n' >&2
  exit 1
}

stats_json="$(curl -fsS --max-time 5 "$frigate_url/api/stats")"
recordings_json="$(curl -fsS --max-time 5 "$frigate_url/api/recordings/summary")"
version="$(curl -fsS --max-time 5 "$frigate_url/api/version")"
load1="$(awk '{print $1}' /proc/loadavg)"
mem_used_mb="$(free -m | awk '/^Mem:/ {print $3}')"
addon_status="unavailable"
if [[ -n "${ha_cmd[0]:-}" ]] && type -P "${ha_cmd[0]}" >/dev/null 2>&1; then
  addon_status="$("${ha_cmd[@]}" addons info "$addon_slug" 2>/dev/null | tr '\n' ' ' || true)"
  [[ -n "$addon_status" ]] || addon_status="unavailable"
fi
recording_metrics="$(
  jq -rc '.cameras | keys[]' <<< "$stats_json" | while IFS= read -r camera; do
    camera_recordings="$(curl -fsS --max-time 5 "$frigate_url/api/$camera/recordings")"
    jq -cn --arg camera "$camera" --argjson recordings "$camera_recordings" \
      '{($camera): {
        count: ($recordings | length),
        total_duration: ($recordings | map(.duration // 0) | add // 0),
        total_size: ($recordings | map(.segment_size // 0) | add // 0),
        latest_end_time: ($recordings | map(.end_time // 0) | max // 0)
      }}'
  done | jq -cs 'add'
)"

{
  printf 'timestamp=%s\n' "$(date -Is)"
  printf 'window_seconds=%s\n' "$window_seconds"
  printf 'frigate_url=%s\n' "$frigate_url"
  printf 'mqtt_host=%s\n' "$mqtt_host"
  printf 'mqtt_port=%s\n' "$mqtt_port"
  printf 'addon_slug=%s\n' "$addon_slug"
  printf 'addon_status=%s\n' "$addon_status"
  printf 'frigate_version=%s\n' "$version"
  printf 'frigate_stats=%s\n' "$stats_json"
  printf 'frigate_recordings_summary=%s\n' "$recordings_json"
  printf 'frigate_recording_metrics=%s\n' "$recording_metrics"
  printf 'load1=%s\n' "$load1"
  printf 'mem_used_mb=%s\n' "$mem_used_mb"
  printf 'load_average='
  cat /proc/loadavg
  printf 'memory='
  free -m || true
  printf 'mqtt_reachable='
  if timeout 3 bash -c "cat < /dev/null > /dev/tcp/$mqtt_host/$mqtt_port" >/dev/null 2>&1; then
    printf 'yes\n'
  else
    printf 'no\n'
  fi
} > "$output"

printf 'captured metrics in %s\n' "$output"
