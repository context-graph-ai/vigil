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
browser_bin="${VIGIL_BROWSER_BIN:-}"
health_url="${VIGIL_HEALTH_URL:-}"
review_url="${VIGIL_REVIEW_URL:-}"
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
expected_review_port="${VIGIL_EXPECTED_REVIEW_PORT:-8098}"
expected_run_uid="${VIGIL_EXPECTED_RUN_UID:-1000}"
haos_render_device="${VIGIL_HAOS_RENDER_DEVICE:-/dev/dri/renderD128}"
th02_dwell_seconds="$(bounded_int "${TH02_DWELL_SECONDS:-600}" 600 7200 600)"
th04_sample_seconds="$(bounded_int "${TH04_SAMPLE_SECONDS:-30}" 30 3600 30)"
th04_mqtt_sample_seconds="$(bounded_int "${TH04_MQTT_SAMPLE_SECONDS:-5}" 5 600 5)"
th05_sample_seconds="$(bounded_int "${TH05_SAMPLE_SECONDS:-30}" 30 3600 30)"
th06_sample_seconds="$(bounded_int "${TH06_SAMPLE_SECONDS:-30}" 30 3600 30)"
th09_sample_seconds="$(bounded_int "${TH09_SAMPLE_SECONDS:-15}" 15 600 15)"

# HA integration TH checks (TH-12..TH-25)
ha_api_token="${HA_API_TOKEN:-}"
ha_api_base="${HA_API_BASE:-http://127.0.0.1:8123}"
vigil_discovery_prefix="${VIGIL_DISCOVERY_PREFIX:-homeassistant}"
vigil_event_topic="${VIGIL_EVENT_TOPIC:-vigil/events}"
vigil_correction_topic="${VIGIL_CORRECTION_TOPIC:-vigil/correction/command}"
vigil_availability_topic="${VIGIL_AVAILABILITY_TOPIC:-vigil/availability}"
vigil_running_condition_topic="${VIGIL_RUNNING_CONDITION_TOPIC:-vigil/running_condition}"
vigil_control_topic="${VIGIL_CONTROL_TOPIC:-vigil/commands/control}"
vigil_availability_offline_payload="${VIGIL_AVAILABILITY_OFFLINE_PAYLOAD:-offline}"
go2rtc_api_base="${GO2RTC_API_BASE:-http://127.0.0.1:1984}"
mosquitto_addon_slug="${MOSQUITTO_ADDON_SLUG:-core_mosquitto}"
th14_detection_wait_seconds="$(bounded_int "${TH14_DETECTION_WAIT_SECONDS:-120}" 30 600 120)"
th22_observation_window_seconds="$(bounded_int "${TH22_OBSERVATION_WINDOW_SECONDS:-60}" 30 600 60)"
th25_broker_restart_wait_seconds="$(bounded_int "${TH25_BROKER_RESTART_WAIT_SECONDS:-60}" 30 300 60)"

config_fail() {
  printf 'TH-CONFIG FAIL %s\n' "$1" >&2
  exit 1
}

if [[ -n "$haos_ssh_target" ]]; then
  ha_cli="ssh $haos_ssh_target sudo docker exec hassio_cli ha"
  docker_cli="ssh $haos_ssh_target sudo docker"
  [[ -n "$health_url" ]] || config_fail "VIGIL_HEALTH_URL is required when HAOS_SSH_TARGET is set"
  [[ -n "$review_url" ]] || config_fail "VIGIL_REVIEW_URL is required when HAOS_SSH_TARGET is set"
  [[ -n "$frigate_url" ]] || config_fail "FRIGATE_URL is required when HAOS_SSH_TARGET is set"
  [[ -n "$mqtt_host" ]] || config_fail "MQTT_HOST is required when HAOS_SSH_TARGET is set"
else
  ha_cli="${ha_cli:-ha}"
  docker_cli="${docker_cli:-docker}"
  curl_cli="${curl_cli:-curl}"
  health_url="${health_url:-http://127.0.0.1:8099/health}"
  review_url="${review_url:-http://127.0.0.1:8098}"
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

receipt_block_from_surface() {
  local header="$1"
  awk -v header="$header" '
    index($0, header) > 0 {
      if (in_block) {
        exit
      }
      in_block = 1
      found = 1
      print
      next
    }
    in_block && (index($0, "[decode.hardware]") > 0 || index($0, "[detect.acceleration]") > 0) {
      exit
    }
    in_block {
      print
    }
    END {
      if (!found) {
        exit 1
      }
    }
  '
}

receipt_field_value() {
  local block="$1"
  local field="$2"
  printf '%s\n' "$block" | awk -v field="$field" '
    {
      line = $0
      sub(/^[[:space:]]*/, "", line)
      if (index(line, field ":") == 1) {
        sub(/^[^:]*:[[:space:]]*/, "", line)
        print line
        exit
      }
    }
  '
}

assert_receipt_field_equals() {
  local id="$1"
  local label="$2"
  local header="$3"
  local block="$4"
  local field="$5"
  local expected="$6"
  local actual
  actual="$(receipt_field_value "$block" "$field")"
  if [[ "$actual" != "$expected" ]]; then
    fail "$id" "$label $header must report $field: $expected, got ${actual:-<missing>}"
    return 1
  fi
}

assert_receipt_field_nonempty() {
  local id="$1"
  local label="$2"
  local header="$3"
  local block="$4"
  local field="$5"
  local actual
  actual="$(receipt_field_value "$block" "$field")"
  if [[ -z "$actual" ]]; then
    fail "$id" "$label $header must report non-empty $field"
    return 1
  fi
}

assert_receipt_field_positive_int() {
  local id="$1"
  local label="$2"
  local header="$3"
  local block="$4"
  local field="$5"
  local actual
  actual="$(receipt_field_value "$block" "$field")"
  if ! [[ "$actual" =~ ^[0-9]+$ ]] || (( actual < 1 )); then
    fail "$id" "$label $header must report positive integer $field, got ${actual:-<missing>}"
    return 1
  fi
}

assert_receipt_surfaces_match() {
  local id="$1"
  local header="$2"
  local doctor_output="$3"
  local runtime_surface="$4"
  local field doctor_block runtime_block doctor_value runtime_value
  doctor_block="$(printf '%s\n' "$doctor_output" | receipt_block_from_surface "$header")" || return 0
  runtime_block="$(printf '%s\n' "$runtime_surface" | receipt_block_from_surface "$header")" || return 0
  for field in "status" "active_backend" "failure_code" "action_kind"; do
    doctor_value="$(receipt_field_value "$doctor_block" "$field")"
    runtime_value="$(receipt_field_value "$runtime_block" "$field")"
    if [[ "$doctor_value" != "$runtime_value" ]]; then
      fail "$id" "doctor and health/log $header receipts disagree on $field: doctor=${doctor_value:-<missing>} runtime=${runtime_value:-<missing>}"
      return 1
    fi
  done
}

assert_decode_receipt_has_probe_evidence() {
  local id="$1"
  local label="$2"
  local header="$3"
  local block="$4"
  local selected_decoder
  assert_receipt_field_nonempty "$id" "$label" "$header" "$block" "selected_device" || return 1
  assert_receipt_field_equals "$id" "$label" "$header" "$block" "evidence_kind" "selected_backend" || return 1
  assert_receipt_field_nonempty "$id" "$label" "$header" "$block" "selected_decoder" || return 1
  assert_receipt_field_positive_int "$id" "$label" "$header" "$block" "probe_units_consumed" || return 1
  selected_decoder="$(receipt_field_value "$block" "selected_decoder")"
  case "${selected_decoder,,}" in
    *software*|*openh264*|*avdec*|*libav*)
      fail "$id" "$label $header selected_decoder must be a hardware decoder, got $selected_decoder"
      return 1
      ;;
  esac
}

assert_runtime_surface_has_real_decode_activity() {
  local id="$1"
  local label="$2"
  local surface="$3"
  local frames whole
  frames="$(printf '%s\n' "$surface" | awk -F= '/^frames-received=/ { print $2; exit }')"
  if [[ -z "$frames" ]]; then
    fail "$id" "$label must include frames-received from a real runtime stats pass"
    return 1
  fi
  case "$frames" in
    ''|*[!0-9.]*)
      fail "$id" "$label frames-received is not numeric: $frames"
      return 1
      ;;
  esac
  whole="${frames%%.*}"
  if [[ -z "$whole" || "$whole" -le 0 ]]; then
    fail "$id" "$label must consume at least one frame before accepting acceleration receipts; frames-received=$frames"
    return 1
  fi
}

assert_acceleration_receipt_block() {
  local id="$1"
  local label="$2"
  local surface="$3"
  local header="$4"
  local block expected
  block="$(printf '%s\n' "$surface" | receipt_block_from_surface "$header")" || {
    fail "$id" "$label is missing receipt block: $header"
    return 1
  }
  for expected in "configured:" "status:" "active_backend:" "failure_code:" "action_kind:"; do
    if [[ "$block" != *"$expected"* ]]; then
      fail "$id" "$label $header block is missing fixed receipt row: $expected"
      return 1
    fi
  done
  assert_receipt_field_equals "$id" "$label" "$header" "$block" "configured" "true" || return 1
  if [[ "$block" == *"failure_code: no_device_visible"* ]]; then
    if [[ "$block" != *"action_kind: run_haos_precheck"* ]]; then
      fail "$id" "$label $header no_device_visible receipt must carry a run_haos_precheck action"
      return 1
    fi
    if [[ "$block" != *"action_payload:"* || "$block" != *"precheck"* ]]; then
      fail "$id" "$label $header no_device_visible receipt must name the host-side precheck in action_payload"
      return 1
    fi
  fi
  if [[ "$header" == "[decode.hardware]" ]]; then
    if [[ "${th27_render_device_openable:-0}" == "1" ]]; then
      assert_receipt_field_equals "$id" "$label" "$header" "$block" "status" "active" || return 1
      assert_receipt_field_equals "$id" "$label" "$header" "$block" "failure_code" "none" || return 1
      assert_receipt_field_equals "$id" "$label" "$header" "$block" "hardware_accelerated" "true" || return 1
      if [[ "$block" == *"active_backend: software"* || "$block" == *"unsupported_by_this_artifact"* ]]; then
        fail "$id" "$label decode receipt must use the hardware decode backend when the mapped render device is openable"
        return 1
      fi
      assert_decode_receipt_has_probe_evidence "$id" "$label" "$header" "$block" || return 1
    else
      fail "$id" "$label decode receipt was checked before the mapped render device was proven openable as the runtime user"
      return 1
    fi
  fi
  if [[ "$header" == "[detect.acceleration]" ]]; then
    # The shipped add-on carries the accelerated detector with live promotion:
    # a Vulkan-capable box whose cold compile outlives the startup deadline
    # boots on the honest CPU fallback and PROMOTES to Active once the probe
    # passes. Both states are truthful; the receipt must be one of them.
    if [[ "$block" == *"status: active"* ]]; then
      assert_receipt_field_equals "$id" "$label" "$header" "$block" "active_backend" "burn-wgpu" || return 1
      assert_receipt_field_equals "$id" "$label" "$header" "$block" "hardware_accelerated" "true" || return 1
      assert_receipt_field_equals "$id" "$label" "$header" "$block" "failure_code" "none" || return 1
      assert_receipt_field_nonempty "$id" "$label" "$header" "$block" "selected_device" || return 1
      return 0
    fi
    assert_receipt_field_equals "$id" "$label" "$header" "$block" "status" "fallback" || return 1
    assert_receipt_field_equals "$id" "$label" "$header" "$block" "active_backend" "burn-cpu" || return 1
    assert_receipt_field_equals "$id" "$label" "$header" "$block" "hardware_accelerated" "false" || return 1
    assert_receipt_field_equals "$id" "$label" "$header" "$block" "failure_code" "probe_failed" || return 1
    for lie in \
      "not in this build" \
      "not part of this build" \
      "not included in this build" \
      "no accelerated detector backend" \
      "install a build with an accelerated detector backend"; do
      if [[ "$block" == *"$lie"* ]]; then
        fail "$id" "$label detection fallback must not claim the accelerated backend is missing on a build that carries it: $lie"
        return 1
      fi
    done
    if [[ "$block" != *"VIGIL_DETECTION_PROBE_DEADLINE_SECS"* \
          && "$block" != *"usable"* \
          && "$block" != *"verify the GPU"* ]]; then
      fail "$id" "$label detection probe-failure fallback must name the real next steps (raise VIGIL_DETECTION_PROBE_DEADLINE_SECS or verify the GPU is usable)"
      return 1
    fi
    if [[ "$block" == *"status: fallback"* && "$block" != *"CPU"* && "$block" != *"cpu"* ]]; then
      fail "$id" "$label fallback detection receipt must say CPU detection is the supported path"
      return 1
    fi
  fi
}

valid_child_pid() {
  local pid="${1:-}"
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  (( pid > 1 )) || return 1
  local parent
  parent="$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d '[:space:]')" || return 1
  [[ "$parent" == "$$" ]]
}

signal_child_pid() {
  local pid="${1:-}"
  valid_child_pid "$pid" || {
    printf 'refusing to signal non-child or unsafe pid: %s\n' "${pid:-<empty>}" >&2
    return 1
  }
  kill "$pid"
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
    signal_child_pid "$sub_pid" >/dev/null 2>&1 || true
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
  [[ "$ports" == $'8098\n8099' ]]
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

current_supervisor_options() {
  # Supervisor validates a POSTed options object against the WHOLE add-on
  # schema — required keys missing from a partial payload are a 400 even
  # when they carry defaults. So the payload always starts from the add-on's
  # current, schema-complete options and merges the suite's fields on top.
  if [[ -n "$haos_ssh_target" ]]; then
    local remote_script quoted_script
    remote_script='curl -fsS -H "Authorization: Bearer $SUPERVISOR_TOKEN" http://supervisor/addons/'"$addon_slug"'/info'
    printf -v quoted_script '%q' "$remote_script"
    ssh -o BatchMode=yes "$haos_ssh_target" \
      "sudo docker exec -i hassio_cli sh -c $quoted_script" | jq -ce '.data.options'
  else
    ha_cli_run addons info "$addon_slug" --raw-json | jq -ce '.data.options'
  fi
}

supervisor_options_payload() {
  # store_path + health_port are always set. A camera is added only when
  # VIGIL_TEST_CAMERA_RTSP is exported — the detection/entity/correction/live-view
  # checks (TH-12..16) need Vigil pointed at a moving RTSP source; the first-light
  # lifecycle checks (TH-01..11) run cameraless (empty list). The add-on reads this
  # `cameras` list from /data/options.json (config.rs read_options_json).
  local cams='[]'
  if [[ -n "${VIGIL_TEST_CAMERA_RTSP:-}" ]]; then
    cams="$(jq -cn --arg url "$VIGIL_TEST_CAMERA_RTSP" --arg name "${VIGIL_TEST_CAMERA_NAME:-test-cam}" \
      '[{name: $name, rtsp_url: $url}]')"
  fi
  local current
  current="$(current_supervisor_options)" || return 1
  jq -cn --argjson current "$current" \
    --arg store_path "$expected_runtime_store_path" \
    --argjson health_port "$expected_health_port" \
    --argjson review_port "$expected_review_port" \
    --argjson cameras "$cams" \
    '$current + {store_path: $store_path, health_port: $health_port, review_port: $review_port, cameras: $cameras}'
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
  for expected_text in "store_path=$expected_runtime_store_path" "health_port=$expected_health_port" "review_port=$expected_review_port"; do
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
  if [[ "$ports" != $'8098/tcp\n8099/tcp' ]]; then
    fail "$id" "config.yaml must declare exactly 8098/tcp and 8099/tcp and no other ports; found: ${ports:-none}"
    return
  fi
  start_addon "$id" "for runtime port audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for runtime port audit"; return; }
  addon_store_runtime_ready "$id" || return
  addon_runtime_ports_clean || {
    fail "$id" "runtime port audit did not show exactly 8098 and 8099 inside the Vigil container"
    return
  }
  pass "$id" "only the health and review ports are declared"
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

# ─── HA integration helpers ───────────────────────────────────────────────

ha_rest_api_run() {
  # Query the HA REST API.  Inside the VM the call goes through the
  # Supervisor proxy so SUPERVISOR_TOKEN is available; outside it uses
  # HA_API_TOKEN against HA_API_BASE.
  local path="$1"
  if [[ -n "$haos_ssh_target" ]]; then
    local remote_script
    printf -v remote_script \
      'curl -fsS -H "Authorization: Bearer $SUPERVISOR_TOKEN" "http://supervisor/ha%s"' \
      "$path"
    ssh -n -o BatchMode=yes "$haos_ssh_target" \
      "sudo docker exec hassio_cli sh -c $(printf '%q' "$remote_script")"
  else
    curl_run -fsS -H "Authorization: Bearer ${ha_api_token}" "${ha_api_base}${path}"
  fi
}

ha_camera_proxy_sha256() {
  # Fetch a camera entity's current JPEG frame via the Core camera_proxy and
  # return its sha256.  The hash is computed where the bytes are (inside the VM
  # for the ssh path) so raw JPEG never crosses ssh as a shell variable.
  local entity="$1"
  if [[ -n "$haos_ssh_target" ]]; then
    ssh -n -o BatchMode=yes "$haos_ssh_target" \
      "sudo docker exec hassio_cli sh -c 'curl -fsS --max-time 5 -H \"Authorization: Bearer \$SUPERVISOR_TOKEN\" \"http://supervisor/ha/api/camera_proxy/${entity}\" | sha256sum | cut -d\" \" -f1'" \
      2>/dev/null || true
  else
    curl_run -fsS --max-time 5 -H "Authorization: Bearer ${ha_api_token}" \
      "${ha_api_base}/api/camera_proxy/${entity}" 2>/dev/null | sha256sum | cut -d' ' -f1 || true
  fi
}

vigil_exec_cmd() {
  # Execute a command inside the running Vigil add-on container.
  local container_id
  container_id="$(addon_container_id)" || return 1
  [[ -n "$container_id" ]] || return 1
  docker_cli_run exec "$container_id" "$@"
}

mqtt_wait_for_message() {
  # Subscribe to $1 and wait up to $2 seconds for one message; print to stdout.
  local topic="$1"
  local wait_seconds="$2"
  local capture sub_err sub_status
  capture="$(mktemp)"
  sub_err="$(mktemp)"
  timeout "$wait_seconds" mosquitto_sub -h "$mqtt_host" -p "$mqtt_port" \
    "${mosquitto_auth_args[@]}" -C 1 -W "$wait_seconds" -t "$topic" \
    > "$capture" 2>"$sub_err"
  sub_status=$?
  if [[ "$sub_status" -ne 0 && "$sub_status" -ne 124 && "$sub_status" -ne 1 ]]; then
    cat "$sub_err" >&2
  fi
  cat "$capture"
  rm -f "$capture" "$sub_err"
}

mqtt_count_messages() {
  # Collect messages on $1 for $2 seconds; print count.
  local topic="$1"
  local wait_seconds="$2"
  local capture sub_err sub_status
  capture="$(mktemp)"
  sub_err="$(mktemp)"
  timeout "$wait_seconds" mosquitto_sub -h "$mqtt_host" -p "$mqtt_port" \
    "${mosquitto_auth_args[@]}" -t "$topic" > "$capture" 2>"$sub_err"
  sub_status=$?
  if [[ "$sub_status" -ne 0 && "$sub_status" -ne 124 ]]; then
    rm -f "$capture" "$sub_err"
    printf '0\n'
    return
  fi
  wc -l < "$capture"
  rm -f "$capture" "$sub_err"
}

ha_vigil_entity_ids() {
  # List entity_ids in HA that belong to Vigil (mqtt platform, vigil in entity_id).
  have_cmd jq || return 1
  ha_rest_api_run /api/states 2>/dev/null \
    | jq -er '[.[] | select(.entity_id | test("vigil"; "i"))] | .[].entity_id' \
      2>/dev/null \
    | sort -u
}

addon_reachable_after_restart() {
  local id="$1"
  restart_addon "$id" "for restart" || return 1
  wait_for_health || { fail "$id" "add-on did not reach health after restart"; return 1; }
}

broker_restart_and_wait() {
  local id="$1"
  ha_cli_run apps restart "$mosquitto_addon_slug" >/dev/null || {
    fail "$id" "could not restart Mosquitto broker add-on ($mosquitto_addon_slug)"
    return 1
  }
  local deadline=$((SECONDS + th25_broker_restart_wait_seconds))
  while (( SECONDS < deadline )); do
    mqtt_ok && return 0
    sleep 2
  done
  fail "$id" "MQTT broker did not become reachable within ${th25_broker_restart_wait_seconds}s after restart"
  return 1
}

# ─── TH-12..TH-25: HA integration acceptance checks ──────────────────────

th12() {
  local id="TH-12"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  mqtt_ok || { fail "$id" "MQTT broker is not reachable at $mqtt_host:$mqtt_port"; return; }
  have_cmd mosquitto_sub || { fail "$id" "mosquitto_sub is required for discovery topic audit"; return; }
  have_cmd jq || { fail "$id" "jq is required for entity registry audit"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for device registration audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for device registration audit"; return; }
  # Collect discovery payloads published on the HA discovery prefix for up to 10 s
  local discovery_count
  discovery_count="$(mqtt_count_messages "${vigil_discovery_prefix}/#" 10)"
  if ! [[ "$discovery_count" =~ ^[0-9]+$ ]] || (( discovery_count == 0 )); then
    fail "$id" "no MQTT discovery payloads published to '${vigil_discovery_prefix}/#' within 10 s \
of add-on start; publish_discovery_to_broker must publish the Vigil device + per-camera sub-device \
discovery tree so Home Assistant auto-registers entities without manual YAML"
    return
  fi
  # Query HA state machine for registered Vigil entities
  local entity_ids
  entity_ids="$(ha_vigil_entity_ids 2>/dev/null)"
  if [[ -z "$entity_ids" ]]; then
    fail "$id" "no Vigil entities found in HA state machine after discovery publish; \
Home Assistant must auto-register the Vigil device and per-camera sub-device on add-on start"
    return
  fi
  # Assert at least one camera/event-class entity (per-camera sub-device must be present)
  local camera_entity_count
  camera_entity_count="$(printf '%s\n' "$entity_ids" | grep -cE '(camera|event)\.' || echo 0)"
  if (( camera_entity_count == 0 )); then
    fail "$id" "no camera or event entities found under the Vigil device in HA; \
the per-camera sub-device carrying the detection event entity must be registered as a distinct \
linked sub-device, not collapsed into the top-level Vigil device"
    return
  fi
  pass "$id" "Vigil device and per-camera sub-device auto-registered in Home Assistant via MQTT discovery"
}

th13() {
  local id="TH-13"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  have_cmd jq || { fail "$id" "jq is required for entity id stability audit"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for entity id stability audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for entity id stability audit"; return; }
  local ids_before
  ids_before="$(ha_vigil_entity_ids 2>/dev/null)"
  if [[ -z "$ids_before" ]]; then
    fail "$id" "no Vigil entities found in HA before stability audit; entity ids and discovery \
topics must be deterministic from camera/service identity and survive restarts — entity must first \
be registered by discovery publish"
    return
  fi
  # Restart Vigil add-on
  restart_addon "$id" "for entity id stability across Vigil restart" || return
  wait_for_health || { fail "$id" "Vigil did not reach health after restart for stability audit"; return; }
  local ids_after_vigil_restart
  ids_after_vigil_restart="$(ha_vigil_entity_ids 2>/dev/null)"
  if [[ "$ids_before" != "$ids_after_vigil_restart" ]]; then
    fail "$id" "Vigil entity ids changed across Vigil add-on restart (before: $(printf '%s' "$ids_before" | head -3) …); \
entity ids must be a pure function of camera/service identity and must not regenerate per process"
    return
  fi
  # Restart Mosquitto broker
  mqtt_ok || { fail "$id" "MQTT broker is not reachable before broker restart audit"; return; }
  broker_restart_and_wait "$id" || return
  wait_for_health || { fail "$id" "Vigil health did not recover after broker restart"; return; }
  local ids_after_broker_restart
  ids_after_broker_restart="$(ha_vigil_entity_ids 2>/dev/null)"
  if [[ "$ids_before" != "$ids_after_broker_restart" ]]; then
    fail "$id" "Vigil entity ids changed after broker restart (before: $(printf '%s' "$ids_before" | head -3) …); \
discovery re-announce must reproduce the same entity ids"
    return
  fi
  pass "$id" "entity ids and discovery topics stable across Vigil restart and broker restart"
}

th14() {
  local id="TH-14"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  mqtt_ok || { fail "$id" "MQTT broker is not reachable at $mqtt_host:$mqtt_port"; return; }
  have_cmd mosquitto_sub || { fail "$id" "mosquitto_sub is required for event topic audit"; return; }
  have_cmd jq || { fail "$id" "jq is required for event payload audit"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for detection event audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for detection event audit"; return; }
  # Wait for a detection event on the event topic (synthetic RTSP source is running)
  local event_payload
  event_payload="$(mqtt_wait_for_message "${vigil_event_topic}" "$th14_detection_wait_seconds")"
  if [[ -z "$event_payload" ]]; then
    fail "$id" "no detection event published to '${vigil_event_topic}' within \
${th14_detection_wait_seconds}s; publish_detection_event must publish the detection payload to the \
broker when a real detection lands so it is reachable as an event entity in Home Assistant"
    return
  fi
  # Assert event payload carries the required contract fields
  local detection_id class confidence
  detection_id="$(jq -er '.detection_id // empty' <<< "$event_payload" 2>/dev/null)"
  class="$(jq -er '.class // .object_class // empty' <<< "$event_payload" 2>/dev/null)"
  confidence="$(jq -er '.confidence // empty' <<< "$event_payload" 2>/dev/null)"
  [[ -n "$detection_id" ]] || {
    fail "$id" "event payload is missing the detection_id field; the event payload must carry the \
detection id so the owner's automation and the correction card can source it"
    return
  }
  [[ -n "$class" ]] || {
    fail "$id" "event payload is missing the object class field (class/object_class)"
    return
  }
  [[ -n "$confidence" ]] || {
    fail "$id" "event payload is missing the confidence field"
    return
  }
  # F15: value-equality via vigil why — assert cg observation fields match the event payload.
  # A correct implementation writes the detection to cg and publishes matching fields to MQTT;
  # wrong stub publishes to MQTT without writing to cg so vigil why returns no output.
  if docker_cli_available; then
    local why_output
    why_output="$(vigil_exec_cmd vigil why "$detection_id" 2>/dev/null)"
    if [[ -z "$why_output" ]]; then
      fail "$id" "'vigil why $detection_id' returned no output inside the add-on container; \
the detection must be written to cg authority so vigil why can resolve the observation from \
the store — wrong stub publishes the event to MQTT but writes nothing to cg"
      return
    fi
    # Assert the event payload class value-equals the cg observation's class field.
    if [[ -n "$class" ]] && [[ "$why_output" != *"$class"* ]]; then
      fail "$id" "'vigil why $detection_id' output does not contain the object class '$class' \
from the event payload; the published event must value-equal the cg observation's class field — \
wrong stub hardcodes a different class in the event than what cg holds"
      return
    fi
  fi
  pass "$id" "real detection surfaced as MQTT event entity and vigil why resolves value-equal cg observation"
}

th15() {
  local id="TH-15"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  docker_cli_available || { fail "$id" "docker command '$docker_cli' is required for network namespace codec probe"; return; }
  have_cmd jq || { fail "$id" "jq is required for go2rtc stream discovery"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for live-view stream codec audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for live-view stream codec audit"; return; }
  # F16: Discover the go2rtc stream name, then probe it with ffprobe from inside the HA network
  # namespace to assert a video codec is present.  A correct implementation wires the camera to
  # go2rtc at an address reachable from inside HA's network; wrong stub registers a host-only path
  # that resolves from the host but is unreachable from the HA network namespace.
  local stream_name
  if [[ -n "$haos_ssh_target" ]]; then
    stream_name="$(ssh -n -o BatchMode=yes "$haos_ssh_target" \
      "curl -fsS --max-time 5 '${go2rtc_api_base}/api/streams' 2>/dev/null \
       | jq -er 'keys[] | select(test(\"vigil\"; \"i\"))' 2>/dev/null | head -1" \
      2>/dev/null || true)"
  else
    stream_name="$(curl_run -fsS --max-time 5 "${go2rtc_api_base}/api/streams" 2>/dev/null \
      | jq -er 'keys[] | select(test("vigil"; "i"))' 2>/dev/null | head -1 || true)"
  fi
  if [[ -z "$stream_name" ]]; then
    fail "$id" "no stream whose name contains 'vigil' found in go2rtc at ${go2rtc_api_base}/api/streams; \
the camera entity must be registered as a go2rtc stream under a vigil-prefixed name — wrong stub never \
registers the stream so no entry appears in the API"
    return
  fi
  local rtsp_url="rtsp://127.0.0.1:8554/${stream_name}"
  # Probe from inside the HA network namespace via the hassio_cli container, which shares the HA
  # host network and can reach go2rtc RTSP at 127.0.0.1:8554.
  local probe_output
  probe_output="$(docker_cli_run exec hassio_cli sh -c \
    "ffprobe -rtsp_transport tcp -v quiet -show_streams -of json '${rtsp_url}' 2>/dev/null" \
    2>/dev/null || true)"
  local video_codec
  video_codec="$(jq -er \
    '[.streams[] | select(.codec_type == "video") | .codec_name][0] // empty' \
    <<< "$probe_output" 2>/dev/null || true)"
  if [[ -z "$video_codec" ]]; then
    fail "$id" "ffprobe on '${rtsp_url}' from inside the HA network namespace (hassio_cli) returned \
no video codec; the live-view stream must deliver decodable video through the HA-internal go2rtc path — \
wrong stub wires go2rtc at a host-only address that is unreachable from inside the HA network namespace"
    return
  fi
  # ── Live-view ENTITY + MOVING-VIDEO assertion (the real owner-facing gate) ──
  # The codec probe above proves the go2rtc stream side. This proves the entity
  # the OWNER opens exists as a streaming camera AND renders MOVING video: a
  # black/frozen tile (wrong stream_source host, or an image-only entity) yields
  # identical frames across time. Discovery is by scanning camera.* (robust to
  # the Generic Camera's default entity_id naming). In the test VM the only
  # cameras are Vigil's, so "some camera.* streams moving video" == Vigil's works.
  # NOTE: this assertion is validated/tuned during HA-OS VM bring-up alongside the
  # config-flow registration (register_generic_camera) — it must NOT be removed to
  # make the suite green; identical-frames is a real black-tile failure.
  local states_json camera_list cam moving_cam=""
  states_json="$(ha_rest_api_run /api/states 2>/dev/null || true)"
  camera_list="$(jq -er '.[] | select(.entity_id | startswith("camera.")) | .entity_id' <<< "$states_json" 2>/dev/null || true)"
  if [[ -z "$camera_list" ]]; then
    fail "$id" "no camera.* entity in HA states; the live-view Generic Camera config entry must be created \
via the config-flow API (register_generic_camera) — wrong stub registers the go2rtc stream but never the entity"
    return
  fi
  while IFS= read -r cam; do
    [[ -n "$cam" ]] || continue
    local stream_type h1 h2
    # frontend_stream_type present == HA treats it as a streaming camera (web_rtc on 2024.11+, else hls)
    stream_type="$(jq -er --arg e "$cam" \
      '.[] | select(.entity_id == $e) | .attributes.frontend_stream_type // empty' \
      <<< "$states_json" 2>/dev/null || true)"
    [[ -n "$stream_type" ]] || continue
    h1="$(ha_camera_proxy_sha256 "$cam")"
    sleep 1
    h2="$(ha_camera_proxy_sha256 "$cam")"
    if [[ -n "$h1" && -n "$h2" && "$h1" != "$h2" ]]; then
      moving_cam="${cam} (frontend_stream_type=${stream_type})"
      break
    fi
  done <<< "$camera_list"
  if [[ -z "$moving_cam" ]]; then
    fail "$id" "no streaming camera entity delivered MOVING video: every camera.* with a frontend_stream_type \
returned identical JPEG frames across 1s — a black/frozen tile means a wrong stream_source host (must be the \
HA-Core-loopback rtsp://127.0.0.1:8554/<slug>) or an image-only entity. The looping fixture must yield differing frames"
    return
  fi
  pass "$id" "live view: go2rtc stream codec '${video_codec}' (probed inside HA net), and camera entity \
${moving_cam} renders MOVING video (frame-difference across 1s)"
}

th16() {
  local id="TH-16"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  mqtt_ok || { fail "$id" "MQTT broker is not reachable at $mqtt_host:$mqtt_port"; return; }
  have_cmd mosquitto_sub || { fail "$id" "mosquitto_sub is required for event/correction audit"; return; }
  have_cmd mosquitto_pub || { fail "$id" "mosquitto_pub is required for correction command publish"; return; }
  have_cmd jq || { fail "$id" "jq is required for event payload parsing"; return; }
  docker_cli_available || { fail "$id" "docker command '$docker_cli' is required for vigil why audit"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for correction round-trip audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for correction round-trip audit"; return; }
  # Capture a real detection from the event topic
  local event_payload detection_id
  event_payload="$(mqtt_wait_for_message "${vigil_event_topic}" "$th14_detection_wait_seconds")"
  if [[ -z "$event_payload" ]]; then
    fail "$id" "no detection event on '${vigil_event_topic}' within ${th14_detection_wait_seconds}s; \
cannot source detection_id for correction round-trip"
    return
  fi
  detection_id="$(jq -er '.detection_id // empty' <<< "$event_payload" 2>/dev/null)"
  if [[ -z "$detection_id" ]]; then
    fail "$id" "detection event payload is missing the detection_id field; \
the correction card sources its id from this payload"
    return
  fi
  # Publish a correction command with the detection id sourced from the event payload
  local correction_payload
  printf -v correction_payload '{"detection_id":"%s","label":"th16 test correction","correction_type":"Identity"}' \
    "$detection_id"
  mosquitto_pub -h "$mqtt_host" -p "$mqtt_port" "${mosquitto_auth_args[@]}" \
    -t "$vigil_correction_topic" -m "$correction_payload" >/dev/null 2>&1 || {
    fail "$id" "could not publish correction command to '$vigil_correction_topic'"
    return
  }
  sleep 3
  # Run vigil why inside the add-on container and assert the correction appears
  local why_output
  why_output="$(vigil_exec_cmd vigil why "$detection_id" 2>/dev/null)" || {
    fail "$id" "vigil why $detection_id failed inside the add-on container"
    return
  }
  if [[ "$why_output" != *"th16 test correction"* ]]; then
    fail "$id" "'vigil why $detection_id' inside the add-on does not list the correction \
published to the command topic (label 'th16 test correction' not found); the subscriber must \
receive the correction command, call record_correction, and write the correction to cg so that \
vigil why can read it back"
    return
  fi
  pass "$id" "correction published to command topic reads back in vigil why inside the add-on"
}

th17() {
  local id="TH-17"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  mqtt_ok || { fail "$id" "MQTT broker is not reachable at $mqtt_host:$mqtt_port"; return; }
  have_cmd mosquitto_pub || { fail "$id" "mosquitto_pub is required for operator action commands"; return; }
  have_cmd mosquitto_sub || { fail "$id" "mosquitto_sub is required for event observation"; return; }
  have_cmd jq || { fail "$id" "jq is required for event payload parsing"; return; }
  docker_cli_available || { fail "$id" "docker command '$docker_cli' is required for acknowledge audit"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for operator action audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for operator action audit"; return; }
  # Step 1: capture a detection id for the acknowledge sub-check
  local event_payload detection_id
  event_payload="$(mqtt_wait_for_message "${vigil_event_topic}" "$th14_detection_wait_seconds")"
  if [[ -z "$event_payload" ]]; then
    fail "$id" "no detection event on '${vigil_event_topic}' within ${th14_detection_wait_seconds}s; \
cannot source detection_id for operator acknowledge sub-check"
    return
  fi
  detection_id="$(jq -er '.detection_id // empty' <<< "$event_payload" 2>/dev/null)"
  if [[ -z "$detection_id" ]]; then
    fail "$id" "event payload is missing detection_id; cannot run acknowledge sub-check"
    return
  fi
  # Step 2a (F17): disable-camera sub-check — publishing disable on the control topic must make
  # the camera go silent (no new events for that camera while it is disabled).
  # Extract camera_id from the event payload; fall back to a configurable default.
  local camera_id_for_ops
  camera_id_for_ops="$(jq -er '.camera_id // empty' <<< "$event_payload" 2>/dev/null || true)"
  if [[ -z "$camera_id_for_ops" ]]; then
    camera_id_for_ops="${TH17_TEST_CAMERA_ID:-lower-gate}"
  fi
  local disable_payload
  printf -v disable_payload '{"camera_id":"%s","action":"disable"}' "$camera_id_for_ops"
  mosquitto_pub -h "$mqtt_host" -p "$mqtt_port" "${mosquitto_auth_args[@]}" \
    -t "$vigil_control_topic" -m "$disable_payload" >/dev/null 2>&1 || {
    fail "$id" "could not publish disable-camera command to '$vigil_control_topic'"
    return
  }
  # Allow the disable to propagate, then listen for events on the disabled camera for 10 s.
  # A correctly-wired subscriber effects the disable so the camera produces no further detections;
  # wrong stub never connects to the broker so the camera keeps detecting.
  sleep 1
  local camera_event_after_disable
  camera_event_after_disable="$(
    timeout 10 mosquitto_sub -h "$mqtt_host" -p "$mqtt_port" "${mosquitto_auth_args[@]}" \
      -t "$vigil_event_topic" -C 1 2>/dev/null \
    | jq -er --arg cid "$camera_id_for_ops" 'select(.camera_id == $cid) | .detection_id // empty' \
      2>/dev/null || true
  )"
  if [[ -n "$camera_event_after_disable" ]]; then
    fail "$id" "camera '${camera_id_for_ops}' produced a new detection event after the disable-camera \
command was published (detection_id='${camera_event_after_disable}'); the subscriber must effect the \
disable on the named camera so it stops contributing detections — wrong stub never connects and the \
camera keeps detecting"
    return
  fi
  # Step 2b (F17): snapshot sub-check — publishing a snapshot command must write an artifact to
  # the add-on's snapshots directory.
  local snapshot_payload
  printf -v snapshot_payload '{"camera_id":"%s","action":"snapshot"}' "$camera_id_for_ops"
  mosquitto_pub -h "$mqtt_host" -p "$mqtt_port" "${mosquitto_auth_args[@]}" \
    -t "$vigil_control_topic" -m "$snapshot_payload" >/dev/null 2>&1 || {
    fail "$id" "could not publish snapshot command to '$vigil_control_topic'"
    return
  }
  sleep 3
  # Assert a snapshot file was written inside the add-on container under /data/snapshots/.
  local snapshot_count
  snapshot_count="$(vigil_exec_cmd sh -c 'ls /data/snapshots/ 2>/dev/null | wc -l' 2>/dev/null || echo 0)"
  if [[ "${snapshot_count//[[:space:]]/}" == "0" ]]; then
    fail "$id" "no snapshot file found at /data/snapshots/ inside the add-on container after the \
snapshot command was published; wrong stub subscriber never connects to the broker so the snapshot \
command is never processed and no artifact is written"
    return
  fi
  # Step 2c: re-enable camera before proceeding to the acknowledge sub-check.
  local enable_payload
  printf -v enable_payload '{"camera_id":"%s","action":"enable"}' "$camera_id_for_ops"
  mosquitto_pub -h "$mqtt_host" -p "$mqtt_port" "${mosquitto_auth_args[@]}" \
    -t "$vigil_control_topic" -m "$enable_payload" >/dev/null 2>&1 || true
  sleep 1
  # Acknowledge sub-check — publish a correction command with correction_type consistent
  # with TH-16 / TH-24 (correction_type field, not "action") and assert it reads back via vigil why.
  local ack_payload
  printf -v ack_payload \
    '{"detection_id":"%s","correction_type":"identity","label":"th17 operator ack"}' "$detection_id"
  mosquitto_pub -h "$mqtt_host" -p "$mqtt_port" "${mosquitto_auth_args[@]}" \
    -t "$vigil_correction_topic" -m "$ack_payload" >/dev/null 2>&1 || {
    fail "$id" "could not publish acknowledge command to '$vigil_correction_topic'"
    return
  }
  sleep 3
  # Step 4: assert the acknowledge correction reads back from vigil why inside the add-on.
  local why_output
  why_output="$(vigil_exec_cmd vigil why "$detection_id" 2>/dev/null)"
  if [[ "$why_output" != *"th17 operator ack"* ]]; then
    fail "$id" "'vigil why $detection_id' inside the add-on does not show the acknowledge label \
'th17 operator ack' after the operator action command was published; the subscriber must receive \
the correction command, call record_correction, and write it to cg authority — wrong stub never \
connects so the command is never processed"
    return
  fi
  pass "$id" "operator action commands effect disable-camera silence, snapshot artifact, and acknowledge correction on the named detection"
}

th18() {
  local id="TH-18"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  mqtt_ok || { fail "$id" "MQTT broker is not reachable at $mqtt_host:$mqtt_port"; return; }
  have_cmd mosquitto_sub || { fail "$id" "mosquitto_sub is required for availability topic audit"; return; }
  docker_cli_available || { fail "$id" "docker command '$docker_cli' is required to kill the add-on container"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for availability last-will audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for availability audit"; return; }
  local container_id
  container_id="$(addon_container_id)" || { fail "$id" "could not identify Vigil add-on container"; return; }
  [[ -n "$container_id" ]] || { fail "$id" "could not identify Vigil add-on container"; return; }
  # Subscribe to availability topic before killing; last-will must arrive within 15 s of the kill
  local th18_wait=15
  local avail_message
  avail_message="$(
    (
      sleep 1
      docker_cli_run kill "$container_id" >/dev/null 2>&1
    ) &
    mqtt_wait_for_message "${vigil_availability_topic}" "$th18_wait"
  )"
  if [[ -z "$avail_message" ]]; then
    fail "$id" "no message on availability topic '${vigil_availability_topic}' within \
${th18_wait}s after killing the Vigil add-on container; the MQTT last-will must publish the \
offline/unavailable value when the Vigil service dies so the device and all its entities go \
unavailable in Home Assistant"
    return
  fi
  # F18: assert the exact payload_not_available string — a partial "not online" substring check
  # is too weak and lets an impl that publishes "online" pass if it also emits "offline" anywhere.
  if [[ "$avail_message" != "$vigil_availability_offline_payload" ]]; then
    fail "$id" "availability topic published '${avail_message}' after kill — expected the exact \
payload_not_available value '${vigil_availability_offline_payload}'; the MQTT last-will must carry \
exactly the declared payload_not_available string so all Home Assistant entities go unavailable — \
wrong stub never sets up a last-will so the value is absent or mis-matched"
    return
  fi
  pass "$id" "MQTT last-will publishes the exact payload_not_available value '${vigil_availability_offline_payload}' when the Vigil service is killed"
}

th19() {
  local id="TH-19"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  mqtt_ok || { fail "$id" "MQTT broker is not reachable at $mqtt_host:$mqtt_port"; return; }
  have_cmd mosquitto_sub || { fail "$id" "mosquitto_sub is required for running-condition audit"; return; }
  have_cmd jq || { fail "$id" "jq is required for running-condition state audit"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for running-condition audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for running-condition audit"; return; }
  # Assert the running-condition state reads "running" while healthy
  local rc_state
  rc_state="$(mqtt_wait_for_message "${vigil_running_condition_topic}" 10)"
  if [[ -z "$rc_state" ]]; then
    fail "$id" "no message on running-condition topic '${vigil_running_condition_topic}' within 10 s; \
the running-condition entity must publish its state so Home Assistant can surface it"
    return
  fi
  if [[ "$rc_state" != "running" ]]; then
    fail "$id" "running-condition state while healthy is '${rc_state}', expected 'running'; \
the mapping must yield the specific string 'running' for the Ready operational state"
    return
  fi
  # F19: actually induce a store-open fault by making the data dir read-only before restart.
  # The previous impl just stopped and started the add-on without any fault, so a correct impl
  # that publishes "running" on a clean restart would falsely fail this test.  With the data dir
  # chmod 000'd, the store cannot be opened and the add-on must publish a named fault string.
  ha_cli_run apps stop "$addon_slug" >/dev/null 2>&1 || true
  sleep 1
  # Make the data directory read-only so the store cannot open on next start.
  if [[ -n "$haos_ssh_target" ]]; then
    ssh -n -o BatchMode=yes "$haos_ssh_target" \
      "sudo chmod 000 $(printf '%q' "$addon_data_dir")" 2>/dev/null || {
      fail "$id" "could not set $addon_data_dir to read-only on the remote host; cannot induce \
store-open fault — ensure the SSH user has sudo privileges"
      return
    }
  else
    chmod 000 "$addon_data_dir" 2>/dev/null || {
      fail "$id" "could not set $addon_data_dir to read-only; cannot induce store-open fault \
(may need elevated privileges to chmod the Supervisor data directory)"
      return
    }
  fi
  ha_cli_run apps start "$addon_slug" >/dev/null 2>&1 || true
  local disk_fault_state
  disk_fault_state="$(mqtt_wait_for_message "${vigil_running_condition_topic}" 20)"
  # Restore data dir permissions unconditionally before asserting so a test failure does not
  # leave the add-on in a broken state.
  if [[ -n "$haos_ssh_target" ]]; then
    ssh -n -o BatchMode=yes "$haos_ssh_target" \
      "sudo chmod 755 $(printf '%q' "$addon_data_dir")" 2>/dev/null || true
  else
    chmod 755 "$addon_data_dir" 2>/dev/null || true
  fi
  ha_cli_run apps stop "$addon_slug" >/dev/null 2>&1 || true
  if [[ "$disk_fault_state" == "running" ]] || [[ -z "$disk_fault_state" ]]; then
    fail "$id" "running-condition published '${disk_fault_state:-<no message>}' after a \
store-open fault (data dir chmod 000 before restart); a correct implementation detects the fault \
at startup and publishes a named fault string (e.g. 'store-open-failed', 'disk-full') — wrong stub \
publishes 'running' regardless of whether the store can be opened"
    return
  fi
  pass "$id" "running-condition reports 'running' while healthy and named fault string '${disk_fault_state}' when the store cannot be opened"
}

th20() {
  local id="TH-20"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  have_cmd jq || { fail "$id" "jq is required for entity registry audit"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for per-camera health entity audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for per-camera health entity audit"; return; }
  local entity_ids
  entity_ids="$(ha_vigil_entity_ids 2>/dev/null)"
  if [[ -z "$entity_ids" ]]; then
    fail "$id" "no Vigil entities found in HA state machine; the device and per-camera sub-device must \
be registered so the entity set can be audited for per-camera health entities"
    return
  fi
  # Assert: no entity with a health/availability role under any camera sub-device
  # Check entity_ids for patterns indicating a per-camera health/availability entity
  local per_camera_health_entities
  per_camera_health_entities="$(printf '%s\n' "$entity_ids" \
    | grep -E '(camera_health|camera_status|camera_problem|camera_available|cam.*health|cam.*status)' \
    || true)"
  if [[ -n "$per_camera_health_entities" ]]; then
    fail "$id" "per-camera health/availability entities found in HA: ${per_camera_health_entities}; \
the add-on must register exactly one whole-service running-condition entity and NO per-camera health \
entities — a per-camera health entity misleads the owner into thinking a camera is green while it may \
be down once an estate has more than one camera"
    return
  fi
  # Also query entity details for any binary_sensor or sensor entity whose attributes
  # indicate health/availability role under a camera device
  local health_role_count
  health_role_count="$(ha_rest_api_run /api/states 2>/dev/null \
    | jq -er '[.[] | select(.entity_id | test("vigil"; "i")) |
        select(.attributes.device_class? | . == "problem" or . == "connectivity" or . == "running")] |
        length' 2>/dev/null || echo 0)"
  if [[ "$health_role_count" =~ ^[0-9]+$ ]] && (( health_role_count > 1 )); then
    fail "$id" "${health_role_count} health-role entities found under the Vigil device; \
exactly one is expected (the whole-service running-condition) and no per-camera health entities \
may be registered, even under a different name"
    return
  fi
  pass "$id" "no per-camera health entity registered; exactly the whole-service running-condition is present"
}

th21() {
  local id="TH-21"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for legacy frontend absence audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for legacy frontend absence audit"; return; }
  # Query HA for registered Lovelace dashboard resources.
  local resources_json legacy_count
  resources_json="$(ha_rest_api_run /api/lovelace/resources 2>/dev/null)"
  legacy_count="$(printf '%s' "$resources_json" \
    | jq -er '[.[] | select(.url == "/local/vigil-event-gallery-card.js")] | length' 2>/dev/null || echo 0)"
  if ! [[ "$legacy_count" =~ ^[0-9]+$ ]]; then
    fail "$id" "could not inspect Home Assistant Lovelace resources for the obsolete Vigil gallery card"
    return
  fi
  if (( legacy_count > 0 )); then
    fail "$id" "obsolete /local/vigil-event-gallery-card.js resource is still registered; \
the owner UI must come from Advanced Camera Card, not the add-on's old gallery card"
    return
  fi
  pass "$id" "obsolete Vigil gallery card is not registered; owner UI belongs to Advanced Camera Card"
}

th22() {
  local id="TH-22"
  mqtt_ok || { fail "$id" "MQTT broker is not reachable at $mqtt_host:$mqtt_port"; return; }
  have_cmd mosquitto_sub || { fail "$id" "mosquitto_sub is required for event topic audit"; return; }
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for empty-stream honesty audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for empty-stream honesty audit"; return; }
  # Collect event topic messages over the observation window; expect zero
  local event_count
  event_count="$(mqtt_count_messages "${vigil_event_topic}" "$th22_observation_window_seconds")"
  if [[ "$event_count" =~ ^[0-9]+$ ]] && (( event_count > 0 )); then
    fail "$id" "${event_count} detection event(s) published to '${vigil_event_topic}' during the \
${th22_observation_window_seconds}s observation window on an empty/garbage stream; the event \
publish path must be gated on a real detection, not a timer or fabricated event"
    return
  fi
  pass "$id" "empty/garbage stream publishes zero detection events to the broker"
}

th23() {
  local id="TH-23"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  have_cmd jq || { fail "$id" "jq is required for entity registry audit"; return; }
  have_cmd mosquitto_pub || { fail "$id" "mosquitto_pub is required for correction round-trip"; return; }
  have_cmd mosquitto_sub || { fail "$id" "mosquitto_sub is required for event observation"; return; }
  docker_cli_available || { fail "$id" "docker command '$docker_cli' is required for network audit"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for local privacy audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for local privacy audit"; return; }
  # Assert: no HA media_source entity for Vigil
  local media_source_entities
  media_source_entities="$(ha_vigil_entity_ids 2>/dev/null | grep -E 'media_source\.' || true)"
  if [[ -n "$media_source_entities" ]]; then
    fail "$id" "HA media_source entity found for Vigil (${media_source_entities}); the full \
recorded clip must be reachable only through Vigil's own vigil events/vigil why evidence reference, \
not via Home Assistant's native Media panel — corrections and clips must not be browsable via \
the HA media source"
    return
  fi
  # Assert: correction path opens no outbound network beyond the local broker
  # The wrong stub opens an outbound socket to 127.0.0.1:19876; a real socket monitor inside
  # the container catches this.  A negative control (known port) proves the monitor works.
  local container_id
  container_id="$(addon_container_id)" || { fail "$id" "could not identify Vigil add-on container"; return; }
  [[ -n "$container_id" ]] || { fail "$id" "could not identify Vigil add-on container"; return; }
  # Check for unexpected non-broker outbound connections from the Vigil process
  local runtime_pid unexpected_conns
  runtime_pid="$(addon_container_runtime_pid "$container_id")" || {
    fail "$id" "could not identify Vigil runtime pid for network audit"
    return
  }
  unexpected_conns="$(docker_cli_run exec "$container_id" sh -c '
    pid="$1"
    broker_port="$2"
    hex_broker="$(printf "%04X" "$broker_port")"
    awk -v pid="$pid" -v broker="$hex_broker" '"'"'
      $4 == "01" {
        split($3, remote, ":")
        if (remote[2] == broker) next
        if (remote[2] == "0000") next
        inode=$10
        found[inode]=1
      }
    '"'"' /proc/net/tcp /proc/net/tcp6 2>/dev/null
    for fd in /proc/"$pid"/fd/*; do
      target=$(readlink "$fd" 2>/dev/null || true)
      case "$target" in
        socket:*)
          inode="${target#socket:[}"
          inode="${inode%]}"
          if [ "${found[$inode]+x}" ]; then printf "outbound socket found\n"; exit 0; fi
          ;;
      esac
    done
  ' sh "$runtime_pid" "$mqtt_port" 2>/dev/null)"
  if [[ "$unexpected_conns" == *"outbound socket found"* ]]; then
    fail "$id" "Vigil add-on has an unexpected outbound network connection beyond the local broker; \
the correction path must open no outbound network beyond the broker — the wrong stub opens a socket \
to 127.0.0.1:19876 to simulate this failure"
    return
  fi
  pass "$id" "clips reachable only via Vigil surface; no HA media_source entity; no unexpected outbound connections"
}

th24() {
  local id="TH-24"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  mqtt_ok || { fail "$id" "MQTT broker is not reachable at $mqtt_host:$mqtt_port"; return; }
  have_cmd mosquitto_pub || { fail "$id" "mosquitto_pub is required for correction command publish"; return; }
  have_cmd mosquitto_sub || { fail "$id" "mosquitto_sub is required for event observation"; return; }
  have_cmd jq || { fail "$id" "jq is required for event payload parsing"; return; }
  docker_cli_available || { fail "$id" "docker command '$docker_cli' is required for vigil why audit"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for correction restart durability audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for correction restart durability audit"; return; }
  # Capture a detection id
  local event_payload detection_id
  event_payload="$(mqtt_wait_for_message "${vigil_event_topic}" "$th14_detection_wait_seconds")"
  if [[ -z "$event_payload" ]]; then
    fail "$id" "no detection event on '${vigil_event_topic}' within ${th14_detection_wait_seconds}s; \
cannot source detection_id for restart durability audit"
    return
  fi
  detection_id="$(jq -er '.detection_id // empty' <<< "$event_payload" 2>/dev/null)"
  if [[ -z "$detection_id" ]]; then
    fail "$id" "event payload is missing detection_id; cannot run restart durability audit"
    return
  fi
  # Publish a correction command
  local correction_payload
  printf -v correction_payload \
    '{"detection_id":"%s","label":"th24 restart test","correction_type":"FalseAlarm"}' \
    "$detection_id"
  mosquitto_pub -h "$mqtt_host" -p "$mqtt_port" "${mosquitto_auth_args[@]}" \
    -t "$vigil_correction_topic" -m "$correction_payload" >/dev/null 2>&1 || {
    fail "$id" "could not publish correction command for restart durability audit"
    return
  }
  sleep 3
  # Restart the local_vigil add-on (the correction must survive this)
  restart_addon "$id" "for correction restart durability audit" || return
  wait_for_health || { fail "$id" "Vigil health did not recover after restart for durability audit"; return; }
  # Run vigil why after restart and assert the correction is still present
  local why_output
  why_output="$(vigil_exec_cmd vigil why "$detection_id" 2>/dev/null)" || {
    fail "$id" "vigil why $detection_id failed inside the restarted add-on container"
    return
  }
  if [[ "$why_output" != *"th24 restart test"* ]]; then
    fail "$id" "'vigil why $detection_id' does not list the correction after the add-on restarted; \
the correction must be written to cg authority (not a retained-MQTT or in-memory shadow) so it \
survives a full add-on restart"
    return
  fi
  pass "$id" "correction published to command topic survives add-on restart and reads back in vigil why"
}

th25() {
  local id="TH-25"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  mqtt_ok || { fail "$id" "MQTT broker is not reachable at $mqtt_host:$mqtt_port"; return; }
  have_cmd mosquitto_sub || { fail "$id" "mosquitto_sub is required for broker recovery audit"; return; }
  ensure_addon_installed "$id" || return
  start_addon "$id" "for broker restart recovery audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for broker restart recovery audit"; return; }
  # Restart the Mosquitto broker add-on; Vigil must reconnect and re-announce
  broker_restart_and_wait "$id" || return
  wait_for_health || { fail "$id" "Vigil health did not recover after broker restart"; return; }
  # Assert discovery is re-announced (or retained discovery survives) after broker restart
  local rediscovery_count
  rediscovery_count="$(mqtt_count_messages "${vigil_discovery_prefix}/#" 15)"
  if ! [[ "$rediscovery_count" =~ ^[0-9]+$ ]] || (( rediscovery_count == 0 )); then
    fail "$id" "no MQTT discovery payloads on '${vigil_discovery_prefix}/#' within 15 s after \
broker restart; Vigil must reconnect to the broker and re-announce discovery (or configure \
retained discovery so the broker re-serves the payloads) so Home Assistant does not lose the \
device after a broker restart"
    return
  fi
  # Assert availability comes back online after broker reconnect
  local avail_message
  avail_message="$(mqtt_wait_for_message "${vigil_availability_topic}" 15)"
  local avail_lower
  avail_lower="$(tr '[:upper:]' '[:lower:]' <<< "$avail_message")"
  if [[ "$avail_lower" != *"online"* ]]; then
    fail "$id" "availability topic '${vigil_availability_topic}' did not publish an online value \
within 15 s after broker restart (got: '${avail_message}'); Vigil must re-publish the online \
availability value after reconnecting to the broker"
    return
  fi
  # Assert a new detection still publishes after broker restart
  local detection_count
  detection_count="$(mqtt_count_messages "${vigil_event_topic}" 30)"
  if ! [[ "$detection_count" =~ ^[0-9]+$ ]] || (( detection_count == 0 )); then
    fail "$id" "no detection events on '${vigil_event_topic}' within 30 s after broker restart; \
the event publish path must recover after broker reconnect so fresh detections still surface in \
Home Assistant"
    return
  fi
  pass "$id" "broker restart recovery: discovery re-announced, availability online, fresh detection publishes"
}

find_browser_bin() {
  if [[ -n "$browser_bin" ]]; then
    if [[ -x "$browser_bin" ]]; then
      printf '%s\n' "$browser_bin"
      return 0
    fi
    if have_cmd "$browser_bin"; then
      printf '%s\n' "$browser_bin"
      return 0
    fi
  fi
  local candidate
  for candidate in chromium chromium-browser google-chrome google-chrome-stable chrome; do
    if have_cmd "$candidate"; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

urlencode() {
  python3 - "$1" <<'PY'
import sys
from urllib.parse import quote
print(quote(sys.argv[1], safe=""))
PY
}

free_local_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

wait_for_local_http() {
  local url="$1"
  local deadline=$((SECONDS + 10))
  while (( SECONDS < deadline )); do
    if curl -fsS --max-time 1 "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

review_events_have_rows() {
  local events_json
  events_json="$(curl -fsS --max-time 5 "$review_url/events" 2>/dev/null)" || return 1
  jq -e 'type == "array" and length > 0' <<< "$events_json" >/dev/null 2>&1
}

th26() {
  local id="TH-26"
  ensure_addon_installed "$id" || return
  start_addon "$id" "for browser review data-plane audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 before browser data-plane audit"; return; }
  have_cmd python3 || { fail "$id" "python3 is required to host the foreign-origin browser test page"; return; }
  have_cmd curl || { fail "$id" "curl is required to wait for the local browser test page"; return; }
  have_cmd jq || { fail "$id" "jq is required to verify the browser event fixture"; return; }
  review_events_have_rows || {
    fail "$id" "Vigil review data plane at $review_url has no event rows; seed a real detection before running browser review acceptance"
    return
  }
  local browser
  browser="$(find_browser_bin)" || {
    fail "$id" "a real headless browser is required; set VIGIL_BROWSER_BIN to chromium or chrome"
    return
  }

  local origin_port page_url encoded_base server_log server_pid dump browser_status
  origin_port="$(free_local_port)" || { fail "$id" "could not allocate foreign-origin page port"; return; }
  encoded_base="$(urlencode "$review_url")" || { fail "$id" "could not encode review URL"; return; }
  page_url="http://127.0.0.1:${origin_port}/browser-cross-origin-fetch.html?base=${encoded_base}"
  server_log="$(mktemp)"
  (
    cd "$script_dir" || exit 1
    python3 -m http.server "$origin_port" --bind 127.0.0.1
  ) >"$server_log" 2>&1 &
  server_pid=$!
  if ! valid_child_pid "$server_pid"; then
    fail "$id" "foreign-origin page server pid was unsafe: ${server_pid:-<empty>}"
    rm -f "$server_log"
    return
  fi

  if ! wait_for_local_http "http://127.0.0.1:${origin_port}/browser-cross-origin-fetch.html"; then
    signal_child_pid "$server_pid" >/dev/null 2>&1 || true
    wait "$server_pid" >/dev/null 2>&1 || true
    fail "$id" "foreign-origin page server did not start: $(tr '\n' ' ' < "$server_log")"
    rm -f "$server_log"
    return
  fi

  dump="$("$browser" --headless=new --disable-gpu --no-sandbox \
    --virtual-time-budget=15000 --dump-dom "$page_url" 2>&1)"
  browser_status=$?
  signal_child_pid "$server_pid" >/dev/null 2>&1 || true
  wait "$server_pid" >/dev/null 2>&1 || true
  rm -f "$server_log"

  if (( browser_status != 0 )); then
    fail "$id" "headless browser failed to load the foreign-origin check: ${dump:0:500}"
    return
  fi
  if [[ "$dump" != *'data-status="PASS"'* ]]; then
    fail "$id" "browser_cross_origin_fetch_renders_media_and_posts_correction failed from foreign origin ${page_url} to ${review_url}: ${dump:0:700}"
    return
  fi
  pass "$id" "browser cross-origin fetch renders snapshot, range-fetches clip transport, and posts correction"
}

th27() {
  local id="TH-27"
  if [[ -z "$haos_ssh_target" ]]; then
    pass "$id" "skipped; set HAOS_SSH_TARGET to run the live add-on acceleration check"
    return
  fi
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  docker_cli_available || { fail "$id" "docker command '$docker_cli' is required for add-on acceleration audit"; return; }
  have_cmd jq || { fail "$id" "jq is required to inspect add-on options"; return; }
  ensure_addon_installed "$id" || return
  apply_supervisor_options "$id" || return
  start_addon "$id" "for acceleration receipt audit" || return
  wait_for_health || { fail "$id" "Vigil health did not reach 200 before acceleration receipt audit"; return; }

  local container_id doctor_output options health_body logs stats_output th27_render_device_openable render_device_evidence
  container_id="$(addon_container_id)" || { fail "$id" "could not identify Vigil add-on container"; return; }
  [[ -n "$container_id" ]] || { fail "$id" "could not identify Vigil add-on container"; return; }

  th27_render_device_openable=0
  render_device_evidence="$(docker_cli_run exec --user "$expected_run_uid" "$container_id" sh -c '
    set -eu
    device="$1"
    test -c "$device"
    test -r "$device"
    test -w "$device"
    exec 9<>"$device"
    base="$(basename "$device")"
    case "$base" in
      renderD[0-9]*|card[0-9]*) ;;
      *) echo "not a DRM render/card node: $base" >&2; exit 1 ;;
    esac
    major_hex="$(stat -c "%t" "$device")"
    minor_hex="$(stat -c "%T" "$device")"
    major_dec="$(printf "%d" "0x$major_hex")"
    minor_dec="$(printf "%d" "0x$minor_hex")"
    test "$major_dec" -eq 226
    if [ ! -e "/sys/class/drm/$base" ] && [ ! -e "/sys/dev/char/${major_dec}:${minor_dec}" ]; then
      echo "missing DRM sysfs backing for $base (${major_dec}:${minor_dec})" >&2
      exit 1
    fi
    exec 9>&-
    printf "%s|%s|%s|%s:%s\n" "$device" "$(stat -c "%F" "$device")" "$base" "$major_dec" "$minor_dec"
  ' sh "$haos_render_device" 2>&1)" || {
    fail "$id" "mapped render device $haos_render_device is not a readable/writable DRM character device inside the add-on as runtime uid $expected_run_uid: ${render_device_evidence:0:500}"
    return
  }
  th27_render_device_openable=1

  # docker exec defaults to root; the proof the smoke needs is what the
  # DROPPED service user can reach, so the doctor is asked about that user
  # explicitly rather than reporting root capability.
  doctor_output="$(docker_cli_run exec "$container_id" vigil doctor acceleration --service-user "$expected_run_uid" 2>&1)" || {
    fail "$id" "in-container acceleration doctor failed: ${doctor_output:0:700}"
    return
  }
  assert_acceleration_receipt_block "$id" "doctor acceleration output" "$doctor_output" "[decode.hardware]" || return
  assert_acceleration_receipt_block "$id" "doctor acceleration output" "$doctor_output" "[detect.acceleration]" || return

  options="$(addon_options_snapshot)" || { fail "$id" "could not snapshot Supervisor options"; return; }
  for option in "hardware_decoding" "accelerated_detection"; do
    if [[ "$options" != *"$option"* ]]; then
      fail "$id" "Supervisor options surface is missing $option"
      return
    fi
  done

  # A 200 from /health precedes the first decode receipt: the receipt lands
  # only after the runtime has actually consumed stream frames, so the proof
  # WAITS (bounded, read-only) for real decode activity plus both receipt
  # blocks on both surfaces before judging — a single early snapshot would
  # fail a correctly-working add-on on startup timing. On timeout the last
  # surfaces are printed verbatim so the failure is diagnosable, and the
  # assertions below still render the specific failure.
  # The detection startup probe now admits a real GPU's cold shader compile
  # (default deadline 60 s), so on a device-present-but-compute-dead VM the
  # first [detect.acceleration] receipt only lands after that bounded probe
  # times out; the wait budget must clear the probe deadline plus startup and
  # decode warm-up, so it is sized above 60 s (still bounded, overridable).
  local receipt_wait_secs="${VIGIL_TH27_RECEIPT_WAIT_SECS:-180}"
  local waited=0 frames_seen
  while :; do
    health_body="$(curl_run -fsS --max-time 5 "$health_url" 2>/dev/null || true)"
    stats_output="$(docker_cli_run exec "$container_id" vigil stats 2>/dev/null || true)"
    frames_seen="$(printf '%s\n' "$stats_output" \
      | awk -F= '/^frames-received=/ { print $2; exit }')"
    frames_seen="${frames_seen%%.*}"
    if [[ -n "$frames_seen" && "$frames_seen" != *[!0-9]* && "${frames_seen:-0}" -gt 0 ]] \
      && [[ "$health_body" == *"[decode.hardware]"* ]] \
      && [[ "$health_body" == *"[detect.acceleration]"* ]] \
      && [[ "$stats_output" == *"[decode.hardware]"* ]] \
      && [[ "$stats_output" == *"[detect.acceleration]"* ]]; then
      break
    fi
    if (( waited >= receipt_wait_secs )); then
      echo "th27: acceleration receipts did not appear on health/stats within ${receipt_wait_secs}s; last surfaces follow" >&2
      printf 'th27 last health body:\n%s\n' "$health_body" >&2
      printf 'th27 last stats output:\n%s\n' "$stats_output" >&2
      ha_cli_run apps logs "$addon_slug" 2>/dev/null | tail -40 >&2 || true
      break
    fi
    sleep 3
    waited=$((waited + 3))
  done
  logs="$(ha_cli_run apps logs "$addon_slug" 2>/dev/null || true)"
  assert_runtime_surface_has_real_decode_activity "$id" "stats acceleration surface" "$stats_output" || return
  assert_acceleration_receipt_block "$id" "health acceleration surface" "$health_body" "[decode.hardware]" || return
  assert_acceleration_receipt_block "$id" "health acceleration surface" "$health_body" "[detect.acceleration]" || return
  assert_acceleration_receipt_block "$id" "stats acceleration surface" "$stats_output" "[decode.hardware]" || return
  assert_acceleration_receipt_block "$id" "stats acceleration surface" "$stats_output" "[detect.acceleration]" || return
  assert_receipt_surfaces_match "$id" "[decode.hardware]" "$doctor_output" "$health_body" || return
  assert_receipt_surfaces_match "$id" "[detect.acceleration]" "$doctor_output" "$health_body" || return
  assert_receipt_surfaces_match "$id" "[decode.hardware]" "$doctor_output" "$stats_output" || return
  assert_receipt_surfaces_match "$id" "[detect.acceleration]" "$doctor_output" "$stats_output" || return

  pass "$id" "add-on acceleration receipts, options, and mapped device are visible"
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
    12|th12|th-12) th12 ;;
    13|th13|th-13) th13 ;;
    14|th14|th-14) th14 ;;
    15|th15|th-15) th15 ;;
    16|th16|th-16) th16 ;;
    17|th17|th-17) th17 ;;
    18|th18|th-18) th18 ;;
    19|th19|th-19) th19 ;;
    20|th20|th-20) th20 ;;
    21|th21|th-21) th21 ;;
    22|th22|th-22) th22 ;;
    23|th23|th-23) th23 ;;
    24|th24|th-24) th24 ;;
    25|th25|th-25) th25 ;;
    26|th26|th-26|browser_cross_origin_fetch_renders_media_and_posts_correction) th26 ;;
    27|th27|th-27) th27 ;;
    *) fail "TH-RUN" "unknown TH_RUN_LIST entry: $1" ;;
  esac
}

th_requires_destructive_opt_in() {
  case "$1" in
    5|05|th05|th-05|6|06|th06|th-06|7|07|th07|th-07|9|09|th09|th-09|11|th11|th-11|\
    17|th17|th-17|18|th18|th-18|19|th19|th-19|24|th24|th-24|25|th25|th-25)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

if [[ -z "$th_run_list" ]]; then
  th_run_list="TH-01 TH-02 TH-03 TH-04 TH-26 TH-27"
fi

for th_name in $th_run_list; do
  run_th "$th_name"
done

exit "$status"
