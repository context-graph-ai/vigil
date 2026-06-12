#!/usr/bin/env bash
set -uo pipefail

status=0
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

addon_dir="${VIGIL_ADDON_DIR:-$repo_root/addons/vigil}"
addon_slug="${VIGIL_ADDON_SLUG:-local_vigil}"
haos_ssh_target="${HAOS_SSH_TARGET:-}"
ha_cli="${HA_CLI:-}"
docker_cli="${DOCKER_CLI:-}"
curl_cli="${CURL_CLI:-}"
health_url="${VIGIL_HEALTH_URL:-}"
frigate_url="${FRIGATE_URL:-}"
mqtt_host="${MQTT_HOST:-}"
mqtt_port="${MQTT_PORT:-1883}"
mqtt_username="${MQTT_USERNAME:-}"
mqtt_password="${MQTT_PASSWORD:-}"
update_addon_dir="${VIGIL_UPDATE_ADDON_DIR:-}"
addon_data_dir="${VIGIL_ADDON_DATA_DIR:-/mnt/data/supervisor/addons/data/$addon_slug}"
addon_store_access="${VIGIL_ADDON_STORE_ACCESS:-host}"
store_probe_binary="${VIGIL_STORE_PROBE_BINARY:-}"
remote_addon_dir="${VIGIL_REMOTE_ADDON_DIR:-/addons/local/vigil}"
addon_binary="${VIGIL_ADDON_BINARY:-$repo_root/target/x86_64-unknown-linux-musl/release/vigil}"
th_run_list="${TH_RUN_LIST:-}"
allow_destructive_th="${VIGIL_ALLOW_DESTRUCTIVE_TH:-0}"
addon_current_package_ready=0

bounded_int() {
  local value="$1"
  local min="$2"
  local max="$3"
  local fallback="$4"
  [[ "$value" =~ ^[0-9]+$ ]] || value="$fallback"
  (( value < min )) && value="$min"
  (( value > max )) && value="$max"
  printf '%s\n' "$value"
}

metric_tolerance_percent="$(bounded_int "${VIGIL_METRIC_TOLERANCE_PERCENT:-10}" 0 10 10)"
fps_absolute_tolerance="${VIGIL_FPS_ABSOLUTE_TOLERANCE:-0.5}"
host_load_absolute_tolerance="${VIGIL_HOST_LOAD_ABSOLUTE_TOLERANCE:-1.0}"
host_memory_absolute_tolerance_mb="${VIGIL_HOST_MEMORY_ABSOLUTE_TOLERANCE_MB:-128}"
mqtt_absolute_tolerance="${VIGIL_MQTT_ABSOLUTE_TOLERANCE:-5}"
host_reboot_cmd="${VIGIL_HOST_REBOOT_CMD:-}"
host_wait_cmd="${VIGIL_HOST_WAIT_CMD:-}"
store_probe="${VIGIL_STORE_PROBE:-cargo run --quiet --manifest-path $repo_root/Cargo.toml -p vigil-acceptance --bin vigil-store-probe --}"
expected_runtime_store_path="${VIGIL_EXPECTED_RUNTIME_STORE_PATH:-/data/store.contextgraph}"
expected_addon_store_path="${VIGIL_EXPECTED_ADDON_STORE_PATH:-$addon_data_dir/store.contextgraph}"
expected_health_port="${VIGIL_EXPECTED_HEALTH_PORT:-8099}"
expected_run_uid="${VIGIL_EXPECTED_RUN_UID:-1000}"
th02_dwell_seconds="$(bounded_int "${TH02_DWELL_SECONDS:-600}" 600 7200 600)"
th04_sample_seconds="$(bounded_int "${TH04_SAMPLE_SECONDS:-30}" 30 3600 30)"
th04_mqtt_sample_seconds="$(bounded_int "${TH04_MQTT_SAMPLE_SECONDS:-5}" 5 600 5)"
th05_sample_seconds="$(bounded_int "${TH05_SAMPLE_SECONDS:-30}" 30 3600 30)"
th06_sample_seconds="$(bounded_int "${TH06_SAMPLE_SECONDS:-30}" 30 3600 30)"
th09_sample_seconds="$(bounded_int "${TH09_SAMPLE_SECONDS:-15}" 15 600 15)"

config_fail() {
  printf 'TH-CONFIG FAIL %s\n' "$1" >&2
  exit 1
}

if [[ -n "$haos_ssh_target" ]]; then
  ha_cli="ssh $haos_ssh_target sudo docker exec hassio_cli ha"
  docker_cli="ssh $haos_ssh_target sudo docker"
  [[ -n "$health_url" ]] || config_fail "VIGIL_HEALTH_URL is required when HAOS_SSH_TARGET is set"
  [[ -n "$frigate_url" ]] || config_fail "FRIGATE_URL is required when HAOS_SSH_TARGET is set"
  [[ -n "$mqtt_host" ]] || config_fail "MQTT_HOST is required when HAOS_SSH_TARGET is set"
else
  ha_cli="${ha_cli:-ha}"
  docker_cli="${docker_cli:-docker}"
  curl_cli="${curl_cli:-curl}"
  health_url="${health_url:-http://127.0.0.1:8099/health}"
  frigate_url="${frigate_url:-http://127.0.0.1:5000}"
  mqtt_host="${mqtt_host:-127.0.0.1}"
fi

read -r -a ha_cmd <<< "$ha_cli"
read -r -a docker_cmd <<< "$docker_cli"
read -r -a curl_cmd <<< "$curl_cli"
read -r -a store_probe_cmd <<< "$store_probe"

mosquitto_auth_args=()
if [[ -n "$mqtt_username" ]]; then
  mosquitto_auth_args+=("-u" "$mqtt_username")
fi
if [[ -n "$mqtt_password" ]]; then
  mosquitto_auth_args+=("-P" "$mqtt_password")
fi

pass() {
  printf '%s PASS %s\n' "$1" "$2"
}

fail() {
  printf '%s FAIL %s\n' "$1" "$2" >&2
  status=1
}

have_cmd() {
  type -P "$1" >/dev/null 2>&1
}

truthy() {
  case "${1,,}" in
    1|true|yes|y|on) return 0 ;;
    *) return 1 ;;
  esac
}

ha_cli_available() {
  if [[ -n "$haos_ssh_target" ]]; then
    have_cmd ssh
    return
  fi
  [[ -n "${ha_cmd[0]:-}" ]] && have_cmd "${ha_cmd[0]}"
}

ha_cli_run() {
  if [[ -n "$haos_ssh_target" ]]; then
    local quoted
    printf -v quoted '%q ' "$@"
    ssh -o BatchMode=yes "$haos_ssh_target" "sudo docker exec hassio_cli ha $quoted"
    return
  fi
  "${ha_cmd[@]}" "$@"
}

docker_cli_available() {
  if [[ -n "$haos_ssh_target" ]]; then
    have_cmd ssh
    return
  fi
  [[ -n "${docker_cmd[0]:-}" ]] && have_cmd "${docker_cmd[0]}"
}

docker_cli_run() {
  if [[ -n "$haos_ssh_target" ]]; then
    local quoted
    printf -v quoted '%q ' "$@"
    ssh -o BatchMode=yes "$haos_ssh_target" "sudo docker $quoted"
    return
  fi
  "${docker_cmd[@]}" "$@"
}

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

store_probe_run() {
  "${store_probe_cmd[@]}" "$@"
}

store_probe_binary_available() {
  if [[ -z "$store_probe_binary" ]]; then
    return 1
  fi
  if [[ -n "$haos_ssh_target" ]]; then
    ssh -o BatchMode=yes "$haos_ssh_target" "test -x '$store_probe_binary'"
  else
    [[ -x "$store_probe_binary" ]]
  fi
}

require_file() {
  local id="$1"
  local path="$2"
  [[ -f "$path" ]] || { fail "$id" "missing required file $path"; return 1; }
}

addon_installed() {
  ha_cli_run apps info "$addon_slug" 2>/dev/null | awk '
    /^state:[[:space:]]*(started|stopped)[[:space:]]*$/ { installed=1 }
    /^version:[[:space:]]*[^[:space:]]+[[:space:]]*$/ && $2 != "null" { installed=1 }
    END { exit(installed ? 0 : 1) }
  '
}

addon_started() {
  ha_cli_run apps info "$addon_slug" 2>/dev/null | awk '
    /^state:[[:space:]]*started[[:space:]]*$/ { started=1 }
    END { exit(started ? 0 : 1) }
  '
}

addon_info_started_or_running() {
  ha_cli_run apps info "$addon_slug" 2>/dev/null | awk '
    /started|running/ { found=1 }
    END { exit(found ? 0 : 1) }
  '
}

addon_update_available() {
  ha_cli_run apps info "$addon_slug" 2>/dev/null | awk '
    /^update_available:[[:space:]]*true[[:space:]]*$/ { found=1 }
    END { exit(found ? 0 : 1) }
  '
}

addon_logs_contain() {
  local expected="$1"
  local logs
  logs="$(ha_cli_run apps logs "$addon_slug" 2>/dev/null)" || return 1
  [[ "$logs" == *"$expected"* ]]
}

start_addon() {
  local id="$1"
  local reason="$2"
  addon_started && return 0
  ha_cli_run apps start "$addon_slug" >/dev/null || {
    fail "$id" "Supervisor did not start $addon_slug $reason"
    return 1
  }
}

restart_addon() {
  local id="$1"
  local reason="$2"
  ha_cli_run apps restart "$addon_slug" >/dev/null || {
    fail "$id" "Supervisor did not restart $addon_slug $reason"
    return 1
  }
}

sync_addon_source() {
  local id="$1"
  local source_dir="$2"
  require_file "$id" "$source_dir/config.yaml" || return 1
  require_file "$id" "$source_dir/Dockerfile" || return 1
  require_file "$id" "$addon_binary" || return 1
  if [[ -n "$haos_ssh_target" ]]; then
    ssh -o BatchMode=yes "$haos_ssh_target" "sudo rm -rf '$remote_addon_dir' && sudo mkdir -p '$remote_addon_dir'" &&
      tar -C "$source_dir" -cf - . | ssh -o BatchMode=yes "$haos_ssh_target" "sudo tar -xf - -C '$remote_addon_dir'" || {
        fail "$id" "could not stage add-on source from $source_dir to $haos_ssh_target:$remote_addon_dir"
        return 1
      }
    ssh -o BatchMode=yes "$haos_ssh_target" "cat >/tmp/vigil-addon-binary" < "$addon_binary" &&
      ssh -o BatchMode=yes "$haos_ssh_target" "sudo mv /tmp/vigil-addon-binary '$remote_addon_dir/vigil' && sudo chmod 755 '$remote_addon_dir/vigil'" || {
        fail "$id" "could not stage add-on binary from $addon_binary to $haos_ssh_target:$remote_addon_dir/vigil"
        return 1
      }
  else
    if [[ "$source_dir" != "$addon_dir" ]]; then
      cp -R "$source_dir"/. "$addon_dir"/ || {
        fail "$id" "could not copy add-on source from $source_dir to $addon_dir"
        return 1
      }
    fi
    cp "$addon_binary" "$addon_dir/vigil" || {
      fail "$id" "could not copy add-on binary from $addon_binary to $addon_dir/vigil"
      return 1
    }
  fi
}

ensure_addon_installed() {
  local id="$1"
  local rebuilt=0
  if (( addon_current_package_ready == 0 )); then
    sync_addon_source "$id" "$addon_dir" || return 1
    ha_cli_run store reload >/dev/null || { fail "$id" "Supervisor store reload failed"; return 1; }
    if addon_installed; then
      ha_cli_run apps rebuild "$addon_slug" >/dev/null || {
        fail "$id" "Supervisor rebuild failed for current add-on source"
        return 1
      }
      rebuilt=1
    else
      ha_cli_run apps install "$addon_slug" >/dev/null || { fail "$id" "install failed for add-on slug $addon_slug"; return 1; }
    fi
    addon_current_package_ready=1
  fi
  addon_installed || {
    fail "$id" "add-on $addon_slug is not marked installed after install/rebuild"
    return 1
  }
  if (( rebuilt == 1 )) && addon_started; then
    restart_addon "$id" "after rebuilding current add-on source" || return 1
    wait_for_health || {
      fail "$id" "rebuilt add-on did not reach health after restart"
      return 1
    }
  fi
}

health_200() {
  curl_run -fsS --max-time 3 "$health_url" >/dev/null 2>&1
}

health_refused() {
  ! curl_run -fsS --max-time 3 "$health_url" >/dev/null 2>&1
}

wait_for_health() {
  local deadline=$((SECONDS + 60))
  while (( SECONDS < deadline )); do
    health_200 && return 0
    sleep 1
  done
  return 1
}

dwell_supervisor_health() {
  local id="$1"
  local deadline=$((SECONDS + th02_dwell_seconds))
  while (( SECONDS < deadline )); do
    health_200 || { fail "$id" "health endpoint failed during ${th02_dwell_seconds}s dwell"; return 1; }
    addon_info_started_or_running || {
      fail "$id" "Supervisor status left started/running during ${th02_dwell_seconds}s dwell"
      return 1
    }
    sleep 10
  done
}

frigate_ok() {
  curl_run -fsS --max-time 5 "$frigate_url/api/version" >/dev/null 2>&1
}

mqtt_ok() {
  timeout 3 bash -c "cat < /dev/null > /dev/tcp/$mqtt_host/$mqtt_port" >/dev/null 2>&1
}

capture_value() {
  local file="$1"
  local key="$2"
  sed -n "s/^$key=//p" "$file" | head -n 1
}

capture_json_value() {
  local file="$1"
  local key="$2"
  local jq_filter="$3"
  local json
  json="$(capture_value "$file" "$key")"
  [[ -n "$json" ]] || return 1
  jq -er "$jq_filter" <<< "$json"
}

compare_number_not_decreased_beyond() {
  local before="$1"
  local during="$2"
  local label="$3"
  local absolute_tolerance="$4"
  awk -v before="$before" -v during="$during" -v tolerance="$metric_tolerance_percent" -v absolute_tolerance="$absolute_tolerance" -v label="$label" '
    BEGIN {
      if (before == "" || during == "") {
        printf "%s missing numeric sample before=%s during=%s\n", label, before, during > "/dev/stderr";
        exit 1;
      }
      lower = before * (1 - tolerance / 100) - absolute_tolerance;
      if (lower < 0) lower = 0;
      if (during < lower) {
        printf "%s dropped beyond %s%%/%s absolute tolerance: before=%s during=%s\n", label, tolerance, absolute_tolerance, before, during > "/dev/stderr";
        exit 1;
      }
    }
  '
}

compare_number_not_increased_beyond() {
  local before="$1"
  local during="$2"
  local label="$3"
  local absolute_tolerance="$4"
  awk -v before="$before" -v during="$during" -v tolerance="$metric_tolerance_percent" -v absolute_tolerance="$absolute_tolerance" -v label="$label" '
    BEGIN {
      if (before == "" || during == "") {
        printf "%s missing numeric sample before=%s during=%s\n", label, before, during > "/dev/stderr";
        exit 1;
      }
      upper = before * (1 + tolerance / 100) + absolute_tolerance;
      if (during > upper) {
        printf "%s rose beyond %s%%/%s absolute tolerance: before=%s during=%s\n", label, tolerance, absolute_tolerance, before, during > "/dev/stderr";
        exit 1;
      }
    }
  '
}

compare_frigate_metric_not_decreased() {
  local before_file="$1"
  local during_file="$2"
  local jq_filter="$3"
  local label="$4"
  local before during
  before="$(capture_json_value "$before_file" frigate_stats "$jq_filter")" || return 1
  during="$(capture_json_value "$during_file" frigate_stats "$jq_filter")" || return 1
  compare_number_not_decreased_beyond "$before" "$during" "$label" "$fps_absolute_tolerance"
}

compare_capture_metric_not_increased() {
  local before_file="$1"
  local during_file="$2"
  local key="$3"
  local label="$4"
  local absolute_tolerance="$5"
  compare_number_not_increased_beyond "$(capture_value "$before_file" "$key")" "$(capture_value "$during_file" "$key")" "$label" "$absolute_tolerance"
}

compare_frigate_captures() {
  local before_file="$1"
  local during_file="$2"
  compare_frigate_metric_not_decreased "$before_file" "$during_file" '.detection_fps' "aggregate detection_fps" || return 1
  compare_capture_metric_not_increased "$before_file" "$during_file" load1 "host load1" "$host_load_absolute_tolerance" || return 1
  compare_capture_metric_not_increased "$before_file" "$during_file" mem_used_mb "host memory used" "$host_memory_absolute_tolerance_mb" || return 1
  local before_recordings during_recordings
  before_recordings="$(capture_json_value "$before_file" frigate_recordings_summary 'keys | length')" || return 1
  during_recordings="$(capture_json_value "$during_file" frigate_recordings_summary 'keys | length')" || return 1
  if (( during_recordings < before_recordings )); then
    printf 'recording summary shrank: before=%s during=%s\n' "$before_recordings" "$during_recordings" >&2
    return 1
  fi
  local cameras
  cameras="$(capture_json_value "$before_file" frigate_stats '.cameras | keys[]')" || return 1
  while IFS= read -r camera; do
    [[ -n "$camera" ]] || continue
    compare_frigate_metric_not_decreased "$before_file" "$during_file" ".cameras[\"$camera\"].camera_fps" "$camera camera_fps" || return 1
    compare_frigate_metric_not_decreased "$before_file" "$during_file" ".cameras[\"$camera\"].detection_fps" "$camera detection_fps" || return 1
    compare_recording_metric_not_decreased "$before_file" "$during_file" "$camera" count || return 1
    compare_recording_metric_not_decreased "$before_file" "$during_file" "$camera" total_duration || return 1
    compare_recording_metric_not_decreased "$before_file" "$during_file" "$camera" total_size || return 1
    compare_recording_metric_not_decreased "$before_file" "$during_file" "$camera" latest_end_time || return 1
  done <<< "$cameras"
}

compare_recording_metric_not_decreased() {
  local before_file="$1"
  local during_file="$2"
  local camera="$3"
  local metric="$4"
  local before during
  before="$(capture_json_value "$before_file" frigate_recording_metrics ".[\"$camera\"].$metric")" || return 1
  during="$(capture_json_value "$during_file" frigate_recording_metrics ".[\"$camera\"].$metric")" || return 1
  awk -v before="$before" -v during="$during" -v label="$camera recording $metric" '
    BEGIN {
      if (before == "" || during == "") {
        printf "%s missing before=%s during=%s\n", label, before, during > "/dev/stderr";
        exit 1;
      }
      if (during + 0 < before + 0) {
        printf "%s decreased: before=%s during=%s\n", label, before, during > "/dev/stderr";
        exit 1;
      }
    }
  '
}

mqtt_topic_count() {
  local seconds="$1"
  local capture sub_err sub_status
  capture="$(mktemp)"
  sub_err="$(mktemp)"
  timeout "$seconds" mosquitto_sub -h "$mqtt_host" -p "$mqtt_port" "${mosquitto_auth_args[@]}" -v -t 'frigate/#' > "$capture" 2>"$sub_err"
  sub_status=$?
  if [[ "$sub_status" -ne 0 && "$sub_status" -ne 124 ]]; then
    printf 'MQTT subscription failed while sampling frigate/#: %s\n' "$(tr '\n' ' ' < "$sub_err")" >&2
    rm -f "$capture" "$sub_err"
    return 1
  fi
  wc -l < "$capture"
  rm -f "$capture" "$sub_err"
}

require_positive_mqtt_count() {
  local id="$1"
  local value="$2"
  local label="$3"
  if ! [[ "$value" =~ ^[0-9]+$ ]] || (( value == 0 )); then
    fail "$id" "$label MQTT Frigate topic sample was empty"
    return 1
  fi
}

mqtt_subscription_roundtrip() {
  local topic="vigil/audit/$RANDOM"
  local payload="audit-$RANDOM"
  local capture
  capture="$(mktemp)"
  mosquitto_sub -h "$mqtt_host" -p "$mqtt_port" "${mosquitto_auth_args[@]}" -C 1 -W 5 -t "$topic" > "$capture" 2>/dev/null &
  local sub_pid=$!
  sleep 1
  mosquitto_pub -h "$mqtt_host" -p "$mqtt_port" "${mosquitto_auth_args[@]}" -t "$topic" -m "$payload" >/dev/null 2>&1 || {
    kill "$sub_pid" >/dev/null 2>&1 || true
    wait "$sub_pid" >/dev/null 2>&1 || true
    rm -f "$capture"
    return 1
  }
  wait "$sub_pid" || { rm -f "$capture"; return 1; }
  grep -qx "$payload" "$capture"
  local matched=$?
  rm -f "$capture"
  return "$matched"
}

addon_container_id() {
  docker_cli_run ps -a --format '{{.ID}}\t{{.Names}}' | awk -v slug="$addon_slug" '
    {
      name=tolower($2);
      slug_l=tolower(slug);
      if (index(name, slug_l) > 0) { print $1; found=1; exit }
      if (index(name, "addon") > 0 && index(name, "vigil") > 0 && fallback == "") fallback=$1;
    }
    END { if (!found && fallback != "") print fallback }
  '
}

addon_image_id() {
  local container_id
  container_id="$(addon_container_id)"
  [[ -n "$container_id" ]] || return 1
  docker_cli_run inspect --format '{{.Image}}' "$container_id"
}

addon_container_data_source() {
  local container_id="$1"
  docker_cli_run inspect --format '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Source}}{{end}}{{end}}' "$container_id"
}

addon_data_empty_or_absent() {
  local data_source="$1"
  if [[ -n "$haos_ssh_target" ]]; then
    [[ -n "$data_source" ]] || return 1
    docker_cli_run run --rm -v /:/host:ro ghcr.io/home-assistant/base:3.22 sh -c '
      path="/host$1"
      if ! test -e "$path"; then
        exit 0
      fi
      ! find "$path" -mindepth 1 -print -quit | grep -q .
    ' sh "$data_source"
    return
  fi
  ! { [[ -e "$addon_data_dir" ]] && find "$addon_data_dir" -mindepth 1 -print -quit | grep -q .; }
}

addon_container_host_pid() {
  local container_id="$1"
  docker_cli_run inspect --format '{{.State.Pid}}' "$container_id"
}

addon_container_runtime_pid() {
  local container_id="$1"
  docker_cli_run exec "$container_id" sh -c 'for pid in $(pidof vigil 2>/dev/null); do echo "$pid"; exit 0; done; pgrep -x vigil 2>/dev/null | head -n 1 || echo 1'
}

container_pid_owns_listen_port() {
  local container_id="$1"
  local runtime_pid="$2"
  local port="$3"
  docker_cli_run exec "$container_id" sh -c '
    pid="$1"
    port="$2"
    hex_port="$(printf "%04X" "$port")"
    inodes="$(awk -v port="$hex_port" '"'"'$4 == "0A" { split($2, local, ":"); if (local[2] == port) print $10 }'"'"' /proc/net/tcp /proc/net/tcp6 2>/dev/null | sort -u)"
    [ -n "$inodes" ] || exit 2
    for fd in /proc/"$pid"/fd/*; do
      target="$(readlink "$fd" 2>/dev/null || true)"
      case "$target" in
        socket:*)
          inode="${target#socket:[}"
          inode="${inode%]}"
          for expected in $inodes; do
            [ "$inode" = "$expected" ] && exit 0
          done
          ;;
      esac
    done
    exit 1
  ' sh "$runtime_pid" "$port"
}

container_uid_owns_listen_port() {
  local container_id="$1"
  local port="$2"
  local uid="$3"
  docker_cli_run exec "$container_id" sh -c '
    port="$1"
    uid="$2"
    hex_port="$(printf "%04X" "$port")"
    awk -v port="$hex_port" -v uid="$uid" '"'"'
      $4 == "0A" {
        split($2, local, ":")
        if (local[2] == port && $8 == uid) found=1
      }
      END { exit(found ? 0 : 1) }
    '"'"' /proc/net/tcp /proc/net/tcp6 2>/dev/null
  ' sh "$port" "$uid"
}

addon_store_path() {
  if [[ "$addon_store_access" == "container" ]]; then
    local container_id
    container_id="$(addon_container_id)" || return 1
    [[ -n "$container_id" ]] || return 1
    docker_cli_run exec "$container_id" sh -c 'test -f "$1" && printf "%s\n" "$1"' sh "$expected_runtime_store_path"
    return
  fi
  [[ -d "$addon_data_dir" ]] || return 1
  find "$addon_data_dir" -type f \( -name '*.cdb' -o -name '*.contextgraph' \) | head -n 1
}

addon_store_identity() {
  local store_path canonical inode
  store_path="$(addon_store_path)" || return 1
  if [[ "$addon_store_access" == "container" ]]; then
    local container_id
    container_id="$(addon_container_id)" || return 1
    [[ -n "$container_id" ]] || return 1
    docker_cli_run exec "$container_id" sh -c 'canonical="$(realpath "$1")" && inode="$(stat -c "%d:%i" "$1")" && printf "%s|%s\n" "$canonical" "$inode"' sh "$store_path"
    return
  fi
  canonical="$(realpath "$store_path")" || return 1
  inode="$(stat -c '%d:%i' "$store_path")" || return 1
  printf '%s|%s\n' "$canonical" "$inode"
}

store_lock_path() {
  local store_path="$1"
  printf '%s.lock\n' "${store_path%.*}"
}

addon_logs_1000() {
  ha_cli_run apps logs "$addon_slug" --lines 1000 2>/dev/null
}

assert_recent_startup_logs() {
  local id="$1"
  local logs="$2"
  for expected in 'vigil version=' 'store_path=' 'health_port=8099' 'embedding_loader=disabled' 'store opened' 'runtime loop ready'; do
    if [[ "$logs" != *"$expected"* ]]; then
      fail "$id" "1000-line Supervisor logs do not contain startup evidence: $expected"
      return 1
    fi
  done
  local latest_epoch now delta
  latest_epoch="$(grep -Eo 'startup_epoch=[0-9]+' <<< "$logs" | tail -n 1 | cut -d= -f2)"
  [[ -n "$latest_epoch" ]] || { fail "$id" "1000-line Supervisor logs do not contain startup_epoch"; return 1; }
  now="$(date +%s)"
  delta=$((now - latest_epoch))
  (( delta < 0 )) && delta=$((-delta))
  if (( delta > 5 )); then
    fail "$id" "latest startup log is not within 5 seconds: delta=${delta}s"
    return 1
  fi
}

addon_store_runtime_ready() {
  local id="$1"
  local logs store_path lock_path container_id host_pid runtime_pid probe_output
  logs="$(ha_cli_run apps logs "$addon_slug" 2>/dev/null || true)"
  for expected in 'store opened' 'runtime loop ready' 'embedding_loader=disabled'; do
    if [[ "$logs" != *"$expected"* ]]; then
      fail "$id" "Supervisor logs do not contain cg-backed runtime evidence: $expected"
      return 1
    fi
  done
  store_path="$(addon_store_path)" || {
    fail "$id" "Vigil add-on data does not contain a cg store under $addon_data_dir"
    return 1
  }
  docker_cli_available || { fail "$id" "docker command '$docker_cli' is required for add-on store process audit"; return 1; }
  container_id="$(addon_container_id)" || {
    fail "$id" "could not identify Vigil add-on container"
    return 1
  }
  if [[ -z "$container_id" ]]; then
    fail "$id" "could not identify Vigil add-on container"
    return 1
  fi
  runtime_pid="$(addon_container_runtime_pid "$container_id")" || {
    fail "$id" "could not identify Vigil runtime pid inside add-on container"
    return 1
  }
  if [[ -z "$runtime_pid" ]]; then
    fail "$id" "could not identify Vigil runtime pid inside add-on container"
    return 1
  fi
  if [[ "$addon_store_access" == "container" ]]; then
    store_probe_binary_available || {
      fail "$id" "VIGIL_STORE_PROBE_BINARY must point to an executable static probe for container store access"
      return 1
    }
    probe_output="$(
      docker_cli_run cp "$store_probe_binary" "$container_id:/tmp/vigil-store-probe" &&
      docker_cli_run exec "$container_id" chmod 755 /tmp/vigil-store-probe &&
      docker_cli_run exec "$container_id" /tmp/vigil-store-probe locked "$store_path" "$runtime_pid"
    )" || {
      fail "$id" "public cg store live probe failed inside add-on container: $probe_output"
      return 1
    }
    container_uid_owns_listen_port "$container_id" "$expected_health_port" "$expected_run_uid" || {
      fail "$id" "Vigil run uid $expected_run_uid does not own health port $expected_health_port"
      return 1
    }
  else
    lock_path="$(store_lock_path "$store_path")"
    [[ -f "$lock_path" ]] || {
      fail "$id" "Vigil add-on store lock is missing at $lock_path"
      return 1
    }
    have_cmd flock || { fail "$id" "flock is required for add-on store lock audit"; return 1; }
    if flock -n "$lock_path" true; then
      fail "$id" "Vigil add-on store lock was not held while healthy: $lock_path"
      return 1
    fi
    host_pid="$(addon_container_host_pid "$container_id")" || {
      fail "$id" "could not identify Vigil add-on host pid"
      return 1
    }
    probe_output="$(store_probe_run live "$store_path" "$host_pid" "$runtime_pid" 2>&1)" || {
      fail "$id" "public cg store live probe failed: $probe_output"
      return 1
    }
    container_pid_owns_listen_port "$container_id" "$runtime_pid" "$expected_health_port" || {
      fail "$id" "Vigil runtime pid $runtime_pid does not own health port $expected_health_port"
      return 1
    }
  fi
}

capture_and_compare_frigate_after() {
  local id="$1"
  local before_file="$2"
  local after_file="$3"
  local label="$4"
  "$script_dir/capture-frigate-metrics.sh" "$after_file" >/dev/null || { fail "$id" "$label metric capture failed"; return 1; }
  compare_frigate_captures "$before_file" "$after_file" || {
    fail "$id" "Frigate metrics drifted beyond ${metric_tolerance_percent}% after $label"
    return 1
  }
}

addon_options_snapshot() {
  if [[ -n "$haos_ssh_target" ]]; then
    local remote_script quoted_script
    remote_script='curl -fsS -H "Authorization: Bearer $SUPERVISOR_TOKEN" http://supervisor/addons/'"$addon_slug"'/info | jq -c .data.options'
    printf -v quoted_script '%q' "$remote_script"
    ssh -o BatchMode=yes "$haos_ssh_target" "sudo docker exec hassio_cli sh -c $quoted_script" \
      | sed '/^[[:space:]]*$/d'
  else
    ha_cli_run addons options "$addon_slug" 2>/dev/null | sed '/^[[:space:]]*$/d'
  fi
}

host_reboot_and_wait() {
  local id="$1"
  [[ -n "$host_reboot_cmd" ]] || { fail "$id" "VIGIL_HOST_REBOOT_CMD is required for host reboot audit"; return 1; }
  local reboot_status=0
  bash -lc "$host_reboot_cmd" || reboot_status=$?
  if [[ -n "$host_wait_cmd" ]]; then
    bash -lc "$host_wait_cmd" || { fail "$id" "host wait command failed after reboot"; return 1; }
    return 0
  fi
  if (( reboot_status != 0 )); then
    fail "$id" "host reboot command failed"
    return 1
  fi
  local deadline=$((SECONDS + 300))
  while (( SECONDS < deadline )); do
    ha_cli_run supervisor info >/dev/null 2>&1 && return 0
    sleep 5
  done
  fail "$id" "Home Assistant did not become reachable after host reboot"
  return 1
}

addon_runtime_ports_clean() {
  local container_id hex_ports ports
  container_id="$(addon_container_id)"
  [[ -n "$container_id" ]] || return 1
  hex_ports="$(docker_cli_run exec "$container_id" sh -c '
    awk '"'"'
      $4 == "0A" {
        split($2, local, ":")
        if (local[1] == "0B00007F") next
        if (local[2] != "") print local[2]
      }
    '"'"' /proc/net/tcp /proc/net/tcp6 2>/dev/null | sort -u
  ')"
  ports="$(
    while IFS= read -r hex_port; do
      [[ -n "$hex_port" ]] || continue
      printf '%d\n' "0x$hex_port"
    done <<< "$hex_ports" | sort -nu
  )"
  [[ "$ports" == "8099" ]]
}

declared_addon_ports() {
  grep -E '^[[:space:]]*[0-9]+/(tcp|udp):' "$addon_dir/config.yaml" | sed -E 's/^[[:space:]]*([0-9]+\/(tcp|udp)):.*/\1/' | sort -u
}

addon_dockerfile_uses_rust_build() {
  grep -Eiq '(^|[^[:alnum:]_])(cargo|rustc|rustup|CARGO|RUSTC)([^[:alnum:]_]|$)' "$addon_dir/Dockerfile"
}

apparmor_default_or_stricter() {
  ! grep -Eiq '^[[:space:]]*apparmor:[[:space:]]*(false|unconfined|disable|disabled)[[:space:]]*$' "$addon_dir/config.yaml"
}

declares_privileged_resources() {
  grep -Eiq 'host_network:[[:space:]]*true|host_pid:[[:space:]]*true|devices:' "$addon_dir/config.yaml" && return 0
  awk '
    /^[[:space:]]*privileged:[[:space:]]*$/ { in_privileged=1; next }
    /^[^[:space:]][^:]*:/ { in_privileged=0 }
    /^[[:space:]]*privileged:[[:space:]]*(false|false[[:space:]]*)$/ { next }
    /^[[:space:]]*privileged:[[:space:]]+/ { found=1 }
    in_privileged && /^[[:space:]]*-[[:space:]]*[^[:space:]]/ { found=1 }
    END { exit(found ? 0 : 1) }
  ' "$addon_dir/config.yaml"
}

supervisor_options_payload() {
  jq -cn --arg store_path "$expected_runtime_store_path" --argjson health_port "$expected_health_port" \
    '{store_path: $store_path, health_port: $health_port}'
}

apply_supervisor_options() {
  local id="$1"
  have_cmd jq || { fail "$id" "jq is required to configure Supervisor options"; return 1; }
  local payload wrapped
  payload="$(supervisor_options_payload)" || { fail "$id" "could not build Supervisor options payload"; return 1; }
  if [[ -n "$haos_ssh_target" ]]; then
    wrapped="$(jq -cn --argjson options "$payload" '{options: $options, watchdog: true}')" || {
      fail "$id" "could not wrap Supervisor options payload"
      return 1
    }
    local remote_script quoted_script
    remote_script='cat >/tmp/vigil-options.json && curl -fsS -X POST -H "Authorization: Bearer $SUPERVISOR_TOKEN" -H "Content-Type: application/json" --data @/tmp/vigil-options.json http://supervisor/addons/'"$addon_slug"'/options >/dev/null'
    printf -v quoted_script '%q' "$remote_script"
    printf '%s' "$wrapped" | ssh -o BatchMode=yes "$haos_ssh_target" \
      "sudo docker exec -i hassio_cli sh -c $quoted_script" || {
        fail "$id" "Supervisor API did not accept store_path/health_port options"
        return 1
      }
  else
    ha_cli_run addons options "$addon_slug" --options "$payload" >/dev/null || {
      fail "$id" "Supervisor options were not accepted for store_path/health_port"
      return 1
    }
  fi
}

assert_supervisor_options_applied() {
  local id="$1"
  local logs store_path actual expected
  logs="$(ha_cli_run apps logs "$addon_slug" 2>/dev/null || true)"
  for expected_text in "store_path=$expected_runtime_store_path" "health_port=$expected_health_port"; do
    if [[ "$logs" != *"$expected_text"* ]]; then
      fail "$id" "Supervisor option did not reach runtime logs: $expected_text"
      return 1
    fi
  done
  store_path="$(addon_store_path)" || {
    fail "$id" "Vigil add-on data does not contain the configured store"
    return 1
  }
  if [[ "$addon_store_access" == "container" ]]; then
    [[ "$store_path" == "$expected_runtime_store_path" ]] || {
      fail "$id" "runtime store path did not match Supervisor option: actual=$store_path expected=$expected_runtime_store_path"
      return 1
    }
    return 0
  fi
  actual="$(realpath "$store_path")" || { fail "$id" "could not canonicalize runtime store path $store_path"; return 1; }
  expected="$(realpath "$expected_addon_store_path")" || {
    fail "$id" "configured store path was not created: $expected_addon_store_path"
    return 1
  }
  [[ "$actual" == "$expected" ]] || {
    fail "$id" "runtime store path did not match Supervisor option: actual=$actual expected=$expected"
    return 1
  }
}

addon_config_watchdog_targets_health() {
  awk '
    /^watchdog:/ {
      line=tolower($0)
      if (line ~ /\[host\]/ && line ~ /\[port:8099\]/ && line ~ /\/health/) found=1
    }
    END { exit(found ? 0 : 1) }
  ' "$addon_dir/config.yaml"
}

assert_supervisor_watchdog_declared() {
  local id="$1"
  addon_config_watchdog_targets_health || {
    fail "$id" "config.yaml does not declare a Supervisor watchdog targeting /health on 8099"
    return 1
  }
}

assert_supervisor_watchdog_enabled() {
  local id="$1"
  assert_supervisor_watchdog_declared "$id" || return 1
  ha_cli_run apps info "$addon_slug" | awk '
    /^watchdog:[[:space:]]*true[[:space:]]*$/ { found=1 }
    END { exit(found ? 0 : 1) }
  ' || {
    fail "$id" "Supervisor watchdog is not enabled for the Vigil add-on"
    return 1
  }
}

th01() {
  local id="TH-01"
  require_file "$id" "$addon_dir/config.yaml" || return
  require_file "$id" "$addon_dir/Dockerfile" || return
  if addon_dockerfile_uses_rust_build; then
    fail "$id" "add-on Dockerfile compiles Rust inside Supervisor"
    return
  fi
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ensure_addon_installed "$id" || return
  assert_supervisor_watchdog_declared "$id" || return
  pass "$id" "local add-on installs or rebuilds from current source through Supervisor"
}

th02() {
  local id="TH-02"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ensure_addon_installed "$id" || return
  apply_supervisor_options "$id" || return
  start_addon "$id" "" || return
  wait_for_health || { fail "$id" "health endpoint did not reach 200 at $health_url"; return; }
  addon_store_runtime_ready "$id" || return
  assert_supervisor_options_applied "$id" || return
  assert_supervisor_watchdog_enabled "$id" || return
  addon_info_started_or_running || { fail "$id" "Supervisor info did not show started/running"; return; }
  dwell_supervisor_health "$id" || return
  pass "$id" "Supervisor start and health are green"
}

th03() {
  local id="TH-03"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ensure_addon_installed "$id" || return
  local logs
  ha_cli_run apps restart "$addon_slug" >/dev/null || { fail "$id" "Supervisor did not restart $addon_slug for log recency audit"; return; }
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for log recency audit"; return; }
  logs="$(addon_logs_1000)" || { fail "$id" "Supervisor did not expose the most recent 1000 log lines"; return; }
  assert_recent_startup_logs "$id" "$logs" || return
  addon_store_runtime_ready "$id" || return
  pass "$id" "Home Assistant log surface contains Vigil startup evidence"
}

th04() {
  local id="TH-04"
  have_cmd jq || { fail "$id" "jq is required for Frigate metric comparison"; return; }
  have_cmd mosquitto_sub || { fail "$id" "mosquitto_sub is required for MQTT rate comparison"; return; }
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ensure_addon_installed "$id" || return
  mqtt_ok || { fail "$id" "MQTT broker is not reachable at $mqtt_host:$mqtt_port"; return; }
  ha_cli_run apps stop "$addon_slug" >/dev/null 2>&1 || true
  frigate_ok || { fail "$id" "Frigate API is not reachable before/during Vigil run"; return; }
  "$script_dir/capture-frigate-metrics.sh" "$script_dir/th04-before.txt" >/dev/null || { fail "$id" "pre-run metric capture failed"; return; }
  local mqtt_before mqtt_during
  mqtt_before="$(mqtt_topic_count "$th04_mqtt_sample_seconds")" || {
    fail "$id" "pre-run MQTT Frigate topic sample failed"
    return
  }
  require_positive_mqtt_count "$id" "$mqtt_before" "pre-run" || return
  ha_cli_run apps start "$addon_slug" >/dev/null || { fail "$id" "Supervisor did not start $addon_slug for Frigate coexistence sample"; return; }
  wait_for_health || { fail "$id" "Vigil health is not 200 while sampling Frigate"; return; }
  addon_store_runtime_ready "$id" || return
  sleep "$th04_sample_seconds"
  "$script_dir/capture-frigate-metrics.sh" "$script_dir/th04-during.txt" >/dev/null || { fail "$id" "during-run metric capture failed"; return; }
  mqtt_during="$(mqtt_topic_count "$th04_mqtt_sample_seconds")" || {
    fail "$id" "during-run MQTT Frigate topic sample failed"
    return
  }
  require_positive_mqtt_count "$id" "$mqtt_during" "during-run" || return
  compare_frigate_captures "$script_dir/th04-before.txt" "$script_dir/th04-during.txt" || {
    fail "$id" "Frigate metrics drifted beyond ${metric_tolerance_percent}% while Vigil ran"
    return
  }
  compare_number_not_decreased_beyond "$mqtt_before" "$mqtt_during" "Frigate MQTT publish count" "$mqtt_absolute_tolerance" || {
    fail "$id" "Frigate MQTT publish count dropped while Vigil ran"
    return
  }
  pass "$id" "Frigate metrics stayed within tolerance while Vigil ran"
}

th05() {
  local id="TH-05"
  have_cmd jq || { fail "$id" "jq is required for Frigate metric comparison"; return; }
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "before stop audit" || return
  frigate_ok || { fail "$id" "Frigate API was not reachable before Vigil stop"; return; }
  "$script_dir/capture-frigate-metrics.sh" "$script_dir/th05-before.txt" >/dev/null || { fail "$id" "pre-stop metric capture failed"; return; }
  ha_cli_run apps stop "$addon_slug" >/dev/null || { fail "$id" "Supervisor did not stop $addon_slug"; return; }
  health_refused || { fail "$id" "health endpoint still accepted connections after stop"; return; }
  sleep "$th05_sample_seconds"
  capture_and_compare_frigate_after "$id" "$script_dir/th05-before.txt" "$script_dir/th05-after.txt" "Vigil stop" || return
  pass "$id" "add-on stops cleanly and Frigate stays reachable"
}

th06() {
  local id="TH-06"
  have_cmd jq || { fail "$id" "jq is required for Frigate metric comparison"; return; }
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  docker_cli_available || { fail "$id" "docker command '$docker_cli' is required for image cleanup audit"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "before uninstall audit" || return
  frigate_ok || { fail "$id" "Frigate API was not reachable before Vigil uninstall"; return; }
  "$script_dir/capture-frigate-metrics.sh" "$script_dir/th06-before.txt" >/dev/null || { fail "$id" "pre-uninstall metric capture failed"; return; }
  local container_id data_source
  container_id="$(addon_container_id 2>/dev/null || true)"
  [[ -n "$container_id" ]] || { fail "$id" "could not identify Vigil container before uninstall"; return; }
  data_source="$(addon_container_data_source "$container_id" 2>/dev/null || true)"
  [[ -n "$data_source" ]] || { fail "$id" "could not identify Vigil /data host path before uninstall"; return; }
  ha_cli_run apps uninstall "$addon_slug" >/dev/null || { fail "$id" "Supervisor uninstall failed for $addon_slug"; return; }
  addon_current_package_ready=0
  if addon_installed; then
    fail "$id" "add-on $addon_slug is still visible after uninstall"
    return
  fi
  if docker_cli_run container inspect "$container_id" >/dev/null 2>&1; then
    fail "$id" "Vigil container remains after uninstall: $container_id"
    return
  fi
  if ! addon_data_empty_or_absent "$data_source"; then
    fail "$id" "Vigil-owned add-on data remains after uninstall at $data_source"
    return
  fi
  sleep "$th06_sample_seconds"
  capture_and_compare_frigate_after "$id" "$script_dir/th06-before.txt" "$script_dir/th06-after.txt" "Vigil uninstall" || return
  pass "$id" "add-on uninstalls cleanly"
}

th07() {
  local id="TH-07"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ensure_addon_installed "$id" || return
  local identity_before identity_after_restart identity_after_reboot
  start_addon "$id" "for first restart probe" || return
  wait_for_health || { fail "$id" "first restart did not reach health"; return; }
  addon_store_runtime_ready "$id" || return
  identity_before="$(addon_store_identity)" || { fail "$id" "could not identify add-on store before restart"; return; }
  ha_cli_run apps stop "$addon_slug" >/dev/null || { fail "$id" "restart stop failed"; return; }
  ha_cli_run apps start "$addon_slug" >/dev/null || { fail "$id" "second restart start failed"; return; }
  wait_for_health || { fail "$id" "second restart did not reach health"; return; }
  addon_store_runtime_ready "$id" || return
  identity_after_restart="$(addon_store_identity)" || { fail "$id" "could not identify add-on store after restart"; return; }
  [[ "$identity_before" == "$identity_after_restart" ]] || {
    fail "$id" "stop/start reopened a different add-on store: before=$identity_before after=$identity_after_restart"
    return
  }
  addon_logs_contain "existing store" || { fail "$id" "restart logs did not report existing store"; return; }
  host_reboot_and_wait "$id" || return
  wait_for_health || { fail "$id" "host reboot did not restore Vigil health"; return; }
  addon_store_runtime_ready "$id" || return
  identity_after_reboot="$(addon_store_identity)" || { fail "$id" "could not identify add-on store after host reboot"; return; }
  [[ "$identity_before" == "$identity_after_reboot" ]] || {
    fail "$id" "host reboot reopened a different add-on store: before=$identity_before after=$identity_after_reboot"
    return
  }
  addon_logs_contain "existing store" || { fail "$id" "host reboot logs did not report existing store"; return; }
  pass "$id" "restart preserves store"
}

th08() {
  local id="TH-08"
  require_file "$id" "$addon_dir/config.yaml" || return
  if declares_privileged_resources; then
    fail "$id" "config.yaml declares privileged host resources"
    return
  fi
  apparmor_default_or_stricter || {
    fail "$id" "config.yaml disables or weakens AppArmor"
    return
  }
  pass "$id" "add-on config declares no privileged host resources"
}

th09() {
  local id="TH-09"
  mqtt_ok || { fail "$id" "MQTT broker is not reachable at $mqtt_host:$mqtt_port"; return; }
  have_cmd mosquitto_sub || { fail "$id" "mosquitto_sub is required for topic audit"; return; }
  have_cmd mosquitto_pub || { fail "$id" "mosquitto_pub is required for subscription audit"; return; }
  mqtt_subscription_roundtrip || { fail "$id" "MQTT subscription round-trip failed before topic silence audit"; return; }
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ensure_addon_installed "$id" || return
  local capture sub_err sub_status
  capture="$(mktemp)"
  sub_err="$(mktemp)"
  timeout "$th09_sample_seconds" mosquitto_sub -h "$mqtt_host" -p "$mqtt_port" "${mosquitto_auth_args[@]}" -v -t 'homeassistant/#' -t 'frigate/#' > "$capture" 2>"$sub_err" &
  local sub_pid=$!
  sleep 2
  start_addon "$id" "for MQTT audit" || { rm -f "$capture" "$sub_err"; return; }
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for MQTT audit"; rm -f "$capture" "$sub_err"; return; }
  ha_cli_run apps stop "$addon_slug" >/dev/null || { fail "$id" "Supervisor did not stop $addon_slug for MQTT audit"; rm -f "$capture" "$sub_err"; return; }
  wait "$sub_pid"
  sub_status=$?
  if [[ "$sub_status" -ne 0 && "$sub_status" -ne 124 ]]; then
    fail "$id" "MQTT topic audit subscription failed: $(tr '\n' ' ' < "$sub_err")"
    rm -f "$capture" "$sub_err"
    return
  fi
  if awk '
    BEGIN { found=0 }
    {
      line=tolower($0)
      if (line ~ /(^|[\/ _.-])vigil([\/ _.-]|$)/ || line ~ /local_vigil/) found=1
    }
    END { exit(found ? 0 : 1) }
  ' "$capture"; then
    fail "$id" "Vigil-named MQTT messages appeared on Home Assistant or Frigate topics during Vigil lifecycle"
    rm -f "$capture" "$sub_err"
    return
  fi
  rm -f "$capture" "$sub_err"
  pass "$id" "no Vigil-named MQTT discovery or Frigate-topic messages observed"
}

th10() {
  local id="TH-10"
  require_file "$id" "$addon_dir/config.yaml" || return
  docker_cli_available || { fail "$id" "docker command '$docker_cli' is required for runtime port audit"; return; }
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ensure_addon_installed "$id" || return
  local ports
  ports="$(declared_addon_ports)"
  if [[ "$ports" != "8099/tcp" ]]; then
    fail "$id" "config.yaml must declare exactly 8099/tcp and no other ports; found: ${ports:-none}"
    return
  fi
  start_addon "$id" "for runtime port audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for runtime port audit"; return; }
  addon_store_runtime_ready "$id" || return
  addon_runtime_ports_clean || {
    fail "$id" "runtime port audit did not show only 8099 inside the Vigil container"
    return
  }
  pass "$id" "only the health port is declared"
}

th11() {
  local id="TH-11"
  [[ -n "$update_addon_dir" ]] || { fail "$id" "VIGIL_UPDATE_ADDON_DIR is required for version-B update probe"; return; }
  [[ -f "$update_addon_dir/config.yaml" ]] || { fail "$id" "version-B add-on config missing at $update_addon_dir"; return; }
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ensure_addon_installed "$id" || return
  local options_before options_after identity_before identity_after
  start_addon "$id" "before update" || return
  wait_for_health || { fail "$id" "version-A did not reach health before update"; return; }
  addon_store_runtime_ready "$id" || return
  identity_before="$(addon_store_identity)" || { fail "$id" "could not identify add-on store before update"; return; }
  options_before="$(addon_options_snapshot)" || { fail "$id" "could not snapshot Supervisor options before update"; return; }
  sync_addon_source "$id" "$update_addon_dir" || return
  ha_cli_run store reload >/dev/null || { fail "$id" "Supervisor store reload failed for version B"; return; }
  if addon_update_available; then
    ha_cli_run apps update "$addon_slug" >/dev/null || { fail "$id" "Supervisor update failed for version B"; return; }
  else
    ha_cli_run apps rebuild "$addon_slug" >/dev/null || { fail "$id" "Supervisor rebuild failed for version B"; return; }
  fi
  ha_cli_run apps restart "$addon_slug" >/dev/null || { fail "$id" "Supervisor restart failed after update"; return; }
  wait_for_health || { fail "$id" "version-B did not reach health after update"; return; }
  addon_store_runtime_ready "$id" || return
  identity_after="$(addon_store_identity)" || { fail "$id" "could not identify add-on store after update"; return; }
  [[ "$identity_before" == "$identity_after" ]] || {
    fail "$id" "update reopened a different add-on store: before=$identity_before after=$identity_after"
    return
  }
  options_after="$(addon_options_snapshot)" || { fail "$id" "could not snapshot Supervisor options after update"; return; }
  [[ "$options_before" == "$options_after" ]] || {
    fail "$id" "Supervisor options changed across update"
    return
  }
  addon_logs_contain "existing store" || { fail "$id" "update logs did not report existing store"; return; }
  pass "$id" "update preserves state and options"
}

run_th() {
  local name
  name="$(tr '[:upper:]' '[:lower:]' <<< "$1")"
  if th_requires_destructive_opt_in "$name" && ! truthy "$allow_destructive_th"; then
    fail "TH-RUN" "$1 is destructive; set VIGIL_ALLOW_DESTRUCTIVE_TH=1 to run stop/uninstall/reboot/update scenarios"
    return
  fi
  case "$name" in
    1|01|th01|th-01) th01 ;;
    2|02|th02|th-02) th02 ;;
    3|03|th03|th-03) th03 ;;
    4|04|th04|th-04) th04 ;;
    5|05|th05|th-05) th05 ;;
    6|06|th06|th-06) th06 ;;
    7|07|th07|th-07) th07 ;;
    8|08|th08|th-08) th08 ;;
    9|09|th09|th-09) th09 ;;
    10|th10|th-10) th10 ;;
    11|th11|th-11) th11 ;;
    *) fail "TH-RUN" "unknown TH_RUN_LIST entry: $1" ;;
  esac
}

th_requires_destructive_opt_in() {
  case "$1" in
    5|05|th05|th-05|6|06|th06|th-06|7|07|th07|th-07|9|09|th09|th-09|11|th11|th-11)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

if [[ -z "$th_run_list" ]]; then
  th_run_list="TH-01 TH-02 TH-03 TH-04"
fi

for th_name in $th_run_list; do
  run_th "$th_name"
done

exit "$status"
