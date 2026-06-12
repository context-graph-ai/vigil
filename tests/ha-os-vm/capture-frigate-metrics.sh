#!/usr/bin/env bash
set -euo pipefail

frigate_url="${FRIGATE_URL:-}"
mqtt_host="${MQTT_HOST:-}"
mqtt_port="${MQTT_PORT:-1883}"
haos_ssh_target="${HAOS_SSH_TARGET:-}"
curl_cli="${CURL_CLI:-}"
ha_cli="${HA_CLI:-}"
addon_slug="${VIGIL_ADDON_SLUG:-local_vigil}"
window_seconds="${WINDOW_SECONDS:-1800}"
output="${1:-frigate-metrics.txt}"
read -r -a curl_cmd <<< "$curl_cli"
read -r -a ha_cmd <<< "$ha_cli"

curl_run() {
  if [[ -n "$haos_ssh_target" ]]; then
    local quoted
    printf -v quoted '%q ' "$@"
    ssh -n -o BatchMode=yes "$haos_ssh_target" "curl $quoted"
  elif [[ -n "${curl_cmd[0]:-}" ]]; then
    "${curl_cmd[@]}" "$@"
  else
    curl "$@"
  fi
}

if [[ -n "$haos_ssh_target" ]]; then
  [[ -n "$frigate_url" ]] || { printf 'FRIGATE_URL is required when HAOS_SSH_TARGET is set\n' >&2; exit 1; }
  [[ -n "$mqtt_host" ]] || { printf 'MQTT_HOST is required when HAOS_SSH_TARGET is set\n' >&2; exit 1; }
  ha_cli="ssh $haos_ssh_target sudo docker exec hassio_cli ha"
else
  frigate_url="${frigate_url:-http://127.0.0.1:5000}"
  mqtt_host="${mqtt_host:-127.0.0.1}"
  curl_cli="${curl_cli:-curl}"
  ha_cli="${ha_cli:-ha}"
fi

read -r -a curl_cmd <<< "$curl_cli"
read -r -a ha_cmd <<< "$ha_cli"

type -P jq >/dev/null 2>&1 || {
  printf 'jq is required for Frigate metric capture\n' >&2
  exit 1
}

stats_json="$(curl_run -fsS --max-time 5 "$frigate_url/api/stats")"
recordings_json="$(curl_run -fsS --max-time 5 "$frigate_url/api/recordings/summary")"
version="$(curl_run -fsS --max-time 5 "$frigate_url/api/version")"
if [[ -n "$haos_ssh_target" ]]; then
  load1="$(ssh -o BatchMode=yes "$haos_ssh_target" "cut -d' ' -f1 /proc/loadavg")"
  mem_used_mb="$(ssh -o BatchMode=yes "$haos_ssh_target" "free -m | tr -s ' ' | sed -n 's/^Mem: [0-9][0-9]* \\([0-9][0-9]*\\).*/\\1/p'")"
else
  load1="$(awk '{print $1}' /proc/loadavg)"
  mem_used_mb="$(free -m | awk '/^Mem:/ {print $3}')"
fi
addon_status="unavailable"
if [[ -n "${ha_cmd[0]:-}" ]] && type -P "${ha_cmd[0]}" >/dev/null 2>&1; then
  addon_status="$("${ha_cmd[@]}" addons info "$addon_slug" 2>/dev/null | tr '\n' ' ' || true)"
  [[ -n "$addon_status" ]] || addon_status="unavailable"
fi
recording_metrics="$(
  jq -rc '.cameras | keys[]' <<< "$stats_json" | while IFS= read -r camera; do
    camera_recordings="$(curl_run -fsS --max-time 5 "$frigate_url/api/$camera/recordings")"
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
  if [[ -n "$haos_ssh_target" ]]; then
    ssh -o BatchMode=yes "$haos_ssh_target" 'cat /proc/loadavg'
  else
    cat /proc/loadavg
  fi
  printf 'memory='
  if [[ -n "$haos_ssh_target" ]]; then
    ssh -o BatchMode=yes "$haos_ssh_target" 'free -m' || true
  else
    free -m || true
  fi
  printf 'mqtt_reachable='
  if timeout 3 bash -c "cat < /dev/null > /dev/tcp/$mqtt_host/$mqtt_port" >/dev/null 2>&1; then
    printf 'yes\n'
  else
    printf 'no\n'
  fi
} > "$output"

printf 'captured metrics in %s\n' "$output"
