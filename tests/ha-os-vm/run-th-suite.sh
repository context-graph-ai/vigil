#!/usr/bin/env bash
set -uo pipefail

status=0
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

addon_dir="${VIGIL_ADDON_DIR:-$repo_root/addons/vigil}"
addon_slug="${VIGIL_ADDON_SLUG:-local_vigil}"
ha_cli="${HA_CLI:-ha}"
health_url="${VIGIL_HEALTH_URL:-http://127.0.0.1:8099/health}"
frigate_url="${FRIGATE_URL:-http://127.0.0.1:5000}"
mqtt_host="${MQTT_HOST:-127.0.0.1}"
mqtt_port="${MQTT_PORT:-1883}"
update_addon_dir="${VIGIL_UPDATE_ADDON_DIR:-}"
addon_data_dir="${VIGIL_ADDON_DATA_DIR:-/mnt/data/supervisor/addons/data/$addon_slug}"

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
host_reboot_cmd="${VIGIL_HOST_REBOOT_CMD:-}"
host_wait_cmd="${VIGIL_HOST_WAIT_CMD:-}"
store_probe="${VIGIL_STORE_PROBE:-cargo run --quiet --manifest-path $repo_root/Cargo.toml --bin vigil-store-probe --}"
expected_runtime_store_path="${VIGIL_EXPECTED_RUNTIME_STORE_PATH:-/data/store.contextgraph}"
expected_addon_store_path="${VIGIL_EXPECTED_ADDON_STORE_PATH:-$addon_data_dir/store.contextgraph}"
expected_health_port="${VIGIL_EXPECTED_HEALTH_PORT:-8099}"
th02_dwell_seconds="$(bounded_int "${TH02_DWELL_SECONDS:-600}" 600 7200 600)"
th04_sample_seconds="$(bounded_int "${TH04_SAMPLE_SECONDS:-30}" 30 3600 30)"
th04_mqtt_sample_seconds="$(bounded_int "${TH04_MQTT_SAMPLE_SECONDS:-5}" 5 600 5)"
th05_sample_seconds="$(bounded_int "${TH05_SAMPLE_SECONDS:-30}" 30 3600 30)"
th06_sample_seconds="$(bounded_int "${TH06_SAMPLE_SECONDS:-30}" 30 3600 30)"
th09_sample_seconds="$(bounded_int "${TH09_SAMPLE_SECONDS:-15}" 15 600 15)"

read -r -a ha_cmd <<< "$ha_cli"
read -r -a store_probe_cmd <<< "$store_probe"

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

ha_cli_available() {
  [[ -n "${ha_cmd[0]:-}" ]] && have_cmd "${ha_cmd[0]}"
}

ha_cli_run() {
  "${ha_cmd[@]}" "$@"
}

store_probe_run() {
  "${store_probe_cmd[@]}" "$@"
}

require_file() {
  local id="$1"
  local path="$2"
  [[ -f "$path" ]] || { fail "$id" "missing required file $path"; return 1; }
}

ensure_addon_installed() {
  local id="$1"
  if ha_cli_run addons info "$addon_slug" >/dev/null 2>&1; then
    return 0
  fi
  require_file "$id" "$addon_dir/config.yaml" || return 1
  require_file "$id" "$addon_dir/Dockerfile" || return 1
  ha_cli_run addons reload >/dev/null || { fail "$id" "Supervisor add-on reload failed"; return 1; }
  ha_cli_run addons install "$addon_slug" >/dev/null || { fail "$id" "install failed for add-on slug $addon_slug"; return 1; }
}

health_200() {
  curl -fsS --max-time 3 "$health_url" >/dev/null 2>&1
}

health_refused() {
  ! curl -fsS --max-time 3 "$health_url" >/dev/null 2>&1
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
    ha_cli_run addons info "$addon_slug" | grep -Eiq 'started|running' || {
      fail "$id" "Supervisor status left started/running during ${th02_dwell_seconds}s dwell"
      return 1
    }
    sleep 10
  done
}

frigate_ok() {
  curl -fsS --max-time 5 "$frigate_url/api/version" >/dev/null 2>&1
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

compare_number_within() {
  local before="$1"
  local during="$2"
  local label="$3"
  awk -v before="$before" -v during="$during" -v tolerance="$metric_tolerance_percent" -v label="$label" '
    BEGIN {
      if (before == "" || during == "") {
        printf "%s missing numeric sample before=%s during=%s\n", label, before, during > "/dev/stderr";
        exit 1;
      }
      if (before + 0 < 0.01) {
        if (during + 0 <= 0.01) exit 0;
        printf "%s rose from near-zero baseline before=%s during=%s\n", label, before, during > "/dev/stderr";
        exit 1;
      }
      lower = before * (1 - tolerance / 100);
      upper = before * (1 + tolerance / 100);
      if (during < lower || during > upper) {
        printf "%s drifted beyond %s%%: before=%s during=%s\n", label, tolerance, before, during > "/dev/stderr";
        exit 1;
      }
    }
  '
}

compare_frigate_metric() {
  local before_file="$1"
  local during_file="$2"
  local jq_filter="$3"
  local label="$4"
  local before during
  before="$(capture_json_value "$before_file" frigate_stats "$jq_filter")" || return 1
  during="$(capture_json_value "$during_file" frigate_stats "$jq_filter")" || return 1
  compare_number_within "$before" "$during" "$label"
}

compare_capture_metric() {
  local before_file="$1"
  local during_file="$2"
  local key="$3"
  local label="$4"
  compare_number_within "$(capture_value "$before_file" "$key")" "$(capture_value "$during_file" "$key")" "$label"
}

compare_frigate_captures() {
  local before_file="$1"
  local during_file="$2"
  compare_frigate_metric "$before_file" "$during_file" '.detection_fps' "aggregate detection_fps" || return 1
  compare_capture_metric "$before_file" "$during_file" load1 "host load1" || return 1
  compare_capture_metric "$before_file" "$during_file" mem_used_mb "host memory used" || return 1
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
    compare_frigate_metric "$before_file" "$during_file" ".cameras[\"$camera\"].camera_fps" "$camera camera_fps" || return 1
    compare_frigate_metric "$before_file" "$during_file" ".cameras[\"$camera\"].detection_fps" "$camera detection_fps" || return 1
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
  local capture
  capture="$(mktemp)"
  timeout "$seconds" mosquitto_sub -h "$mqtt_host" -p "$mqtt_port" -v -t 'frigate/#' > "$capture" 2>/dev/null || true
  wc -l < "$capture"
  rm -f "$capture"
}

mqtt_subscription_roundtrip() {
  local topic="vigil/audit/$RANDOM"
  local payload="audit-$RANDOM"
  local capture
  capture="$(mktemp)"
  mosquitto_sub -h "$mqtt_host" -p "$mqtt_port" -C 1 -W 5 -t "$topic" > "$capture" 2>/dev/null &
  local sub_pid=$!
  sleep 1
  mosquitto_pub -h "$mqtt_host" -p "$mqtt_port" -t "$topic" -m "$payload" >/dev/null 2>&1 || {
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
  docker ps -a --format '{{.ID}}\t{{.Names}}' | awk -v slug="$addon_slug" '
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
  docker inspect --format '{{.Image}}' "$container_id"
}

addon_container_host_pid() {
  local container_id="$1"
  docker inspect --format '{{.State.Pid}}' "$container_id"
}

addon_container_runtime_pid() {
  local container_id="$1"
  docker exec "$container_id" sh -c 'for pid in $(pidof vigil 2>/dev/null); do echo "$pid"; exit 0; done; pgrep -x vigil 2>/dev/null | head -n 1 || echo 1'
}

container_pid_owns_listen_port() {
  local container_id="$1"
  local runtime_pid="$2"
  local port="$3"
  docker exec "$container_id" sh -c '
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

vigil_image_leftover() {
  local image_id="$1"
  if [[ -n "$image_id" ]] && docker image inspect "$image_id" >/dev/null 2>&1; then
    return 0
  fi
  docker images --format '{{.Repository}}:{{.Tag}}' | grep -Eiq '(^|/)(vigil|local_vigil|addon.*vigil)(:|$)'
}

addon_store_path() {
  [[ -d "$addon_data_dir" ]] || return 1
  find "$addon_data_dir" -type f \( -name '*.cdb' -o -name '*.contextgraph' \) | head -n 1
}

addon_store_identity() {
  local store_path canonical inode
  store_path="$(addon_store_path)" || return 1
  canonical="$(realpath "$store_path")" || return 1
  inode="$(stat -c '%d:%i' "$store_path")" || return 1
  printf '%s|%s\n' "$canonical" "$inode"
}

store_lock_path() {
  local store_path="$1"
  printf '%s.lock\n' "${store_path%.*}"
}

addon_logs_1000() {
  ha_cli_run addons logs "$addon_slug" --lines 1000 2>/dev/null
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
  logs="$(ha_cli_run addons logs "$addon_slug" 2>/dev/null || true)"
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
  have_cmd docker || { fail "$id" "docker is required for add-on store process audit"; return 1; }
  container_id="$(addon_container_id)" || {
    fail "$id" "could not identify Vigil add-on container"
    return 1
  }
  host_pid="$(addon_container_host_pid "$container_id")" || {
    fail "$id" "could not identify Vigil add-on host pid"
    return 1
  }
  runtime_pid="$(addon_container_runtime_pid "$container_id")" || {
    fail "$id" "could not identify Vigil runtime pid inside add-on container"
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
  ha_cli_run addons options "$addon_slug" 2>/dev/null | sed '/^[[:space:]]*$/d'
}

host_reboot_and_wait() {
  local id="$1"
  [[ -n "$host_reboot_cmd" ]] || { fail "$id" "VIGIL_HOST_REBOOT_CMD is required for host reboot audit"; return 1; }
  bash -lc "$host_reboot_cmd" || { fail "$id" "host reboot command failed"; return 1; }
  if [[ -n "$host_wait_cmd" ]]; then
    bash -lc "$host_wait_cmd" || { fail "$id" "host wait command failed after reboot"; return 1; }
    return 0
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
  local container_id ports
  container_id="$(addon_container_id)"
  [[ -n "$container_id" ]] || return 1
  docker exec "$container_id" ss -tlnp > "$script_dir/th10-ss.txt" 2>/dev/null || return 1
  ports="$(awk '
    /^LISTEN/ {
      address=$4;
      sub(/.*:/, "", address);
      if (address ~ /^[0-9]+$/) print address;
    }
  ' "$script_dir/th10-ss.txt" | sort -u)"
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
  local payload
  payload="$(supervisor_options_payload)" || { fail "$id" "could not build Supervisor options payload"; return 1; }
  ha_cli_run addons options "$addon_slug" --options "$payload" >/dev/null || {
    fail "$id" "Supervisor options were not accepted for store_path/health_port"
    return 1
  }
}

assert_supervisor_options_applied() {
  local id="$1"
  local logs store_path actual expected
  logs="$(ha_cli_run addons logs "$addon_slug" 2>/dev/null || true)"
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

th01() {
  local id="TH-01"
  require_file "$id" "$addon_dir/config.yaml" || return
  require_file "$id" "$addon_dir/Dockerfile" || return
  if addon_dockerfile_uses_rust_build; then
    fail "$id" "add-on Dockerfile compiles Rust inside Supervisor"
    return
  fi
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ha_cli_run addons reload >/dev/null || { fail "$id" "Supervisor add-on reload failed"; return; }
  ha_cli_run addons install "$addon_slug" >/dev/null || { fail "$id" "install failed for add-on slug $addon_slug"; return; }
  ha_cli_run addons info "$addon_slug" >/dev/null || { fail "$id" "installed add-on $addon_slug is not visible"; return; }
  pass "$id" "local add-on installs through Supervisor"
}

th02() {
  local id="TH-02"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ensure_addon_installed "$id" || return
  apply_supervisor_options "$id" || return
  ha_cli_run addons start "$addon_slug" >/dev/null || { fail "$id" "Supervisor did not start $addon_slug"; return; }
  wait_for_health || { fail "$id" "health endpoint did not reach 200 at $health_url"; return; }
  addon_store_runtime_ready "$id" || return
  assert_supervisor_options_applied "$id" || return
  ha_cli_run addons info "$addon_slug" | grep -Eiq 'started|running' || { fail "$id" "Supervisor info did not show started/running"; return; }
  dwell_supervisor_health "$id" || return
  pass "$id" "Supervisor start and health are green"
}

th03() {
  local id="TH-03"
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  local logs
  ha_cli_run addons restart "$addon_slug" >/dev/null || { fail "$id" "Supervisor did not restart $addon_slug for log recency audit"; return; }
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
  mqtt_ok || { fail "$id" "MQTT broker is not reachable at $mqtt_host:$mqtt_port"; return; }
  ha_cli_run addons stop "$addon_slug" >/dev/null 2>&1 || true
  frigate_ok || { fail "$id" "Frigate API is not reachable before/during Vigil run"; return; }
  "$script_dir/capture-frigate-metrics.sh" "$script_dir/th04-before.txt" >/dev/null || { fail "$id" "pre-run metric capture failed"; return; }
  local mqtt_before mqtt_during
  mqtt_before="$(mqtt_topic_count "$th04_mqtt_sample_seconds")"
  ha_cli_run addons start "$addon_slug" >/dev/null || { fail "$id" "Supervisor did not start $addon_slug for Frigate coexistence sample"; return; }
  wait_for_health || { fail "$id" "Vigil health is not 200 while sampling Frigate"; return; }
  addon_store_runtime_ready "$id" || return
  sleep "$th04_sample_seconds"
  "$script_dir/capture-frigate-metrics.sh" "$script_dir/th04-during.txt" >/dev/null || { fail "$id" "during-run metric capture failed"; return; }
  mqtt_during="$(mqtt_topic_count "$th04_mqtt_sample_seconds")"
  compare_frigate_captures "$script_dir/th04-before.txt" "$script_dir/th04-during.txt" || {
    fail "$id" "Frigate metrics drifted beyond ${metric_tolerance_percent}% while Vigil ran"
    return
  }
  compare_number_within "$mqtt_before" "$mqtt_during" "Frigate MQTT publish count" || {
    fail "$id" "Frigate MQTT publish count drifted beyond ${metric_tolerance_percent}% while Vigil ran"
    return
  }
  pass "$id" "Frigate metrics stayed within tolerance while Vigil ran"
}

th05() {
  local id="TH-05"
  have_cmd jq || { fail "$id" "jq is required for Frigate metric comparison"; return; }
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  frigate_ok || { fail "$id" "Frigate API was not reachable before Vigil stop"; return; }
  "$script_dir/capture-frigate-metrics.sh" "$script_dir/th05-before.txt" >/dev/null || { fail "$id" "pre-stop metric capture failed"; return; }
  ha_cli_run addons stop "$addon_slug" >/dev/null || { fail "$id" "Supervisor did not stop $addon_slug"; return; }
  health_refused || { fail "$id" "health endpoint still accepted connections after stop"; return; }
  sleep "$th05_sample_seconds"
  capture_and_compare_frigate_after "$id" "$script_dir/th05-before.txt" "$script_dir/th05-after.txt" "Vigil stop" || return
  pass "$id" "add-on stops cleanly and Frigate stays reachable"
}

th06() {
  local id="TH-06"
  have_cmd jq || { fail "$id" "jq is required for Frigate metric comparison"; return; }
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  have_cmd docker || { fail "$id" "docker is required for image cleanup audit"; return; }
  frigate_ok || { fail "$id" "Frigate API was not reachable before Vigil uninstall"; return; }
  "$script_dir/capture-frigate-metrics.sh" "$script_dir/th06-before.txt" >/dev/null || { fail "$id" "pre-uninstall metric capture failed"; return; }
  local image_id
  image_id="$(addon_image_id 2>/dev/null || true)"
  [[ -n "$image_id" ]] || { fail "$id" "could not identify Vigil container image before uninstall"; return; }
  ha_cli_run addons uninstall "$addon_slug" >/dev/null || { fail "$id" "Supervisor uninstall failed for $addon_slug"; return; }
  if ha_cli_run addons info "$addon_slug" >/dev/null 2>&1; then
    fail "$id" "add-on $addon_slug is still visible after uninstall"
    return
  fi
  if [[ -e "$addon_data_dir" ]] && find "$addon_data_dir" -mindepth 1 -print -quit | grep -q .; then
    fail "$id" "Vigil-owned add-on data remains after uninstall at $addon_data_dir"
    return
  fi
  if vigil_image_leftover "$image_id"; then
    fail "$id" "Vigil container image remains after uninstall: $image_id"
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
  ha_cli_run addons start "$addon_slug" >/dev/null || { fail "$id" "first restart start failed"; return; }
  wait_for_health || { fail "$id" "first restart did not reach health"; return; }
  addon_store_runtime_ready "$id" || return
  identity_before="$(addon_store_identity)" || { fail "$id" "could not identify add-on store before restart"; return; }
  ha_cli_run addons stop "$addon_slug" >/dev/null || { fail "$id" "restart stop failed"; return; }
  ha_cli_run addons start "$addon_slug" >/dev/null || { fail "$id" "second restart start failed"; return; }
  wait_for_health || { fail "$id" "second restart did not reach health"; return; }
  addon_store_runtime_ready "$id" || return
  identity_after_restart="$(addon_store_identity)" || { fail "$id" "could not identify add-on store after restart"; return; }
  [[ "$identity_before" == "$identity_after_restart" ]] || {
    fail "$id" "stop/start reopened a different add-on store: before=$identity_before after=$identity_after_restart"
    return
  }
  ha_cli_run addons logs "$addon_slug" | grep -qi 'existing store' || { fail "$id" "restart logs did not report existing store"; return; }
  host_reboot_and_wait "$id" || return
  wait_for_health || { fail "$id" "host reboot did not restore Vigil health"; return; }
  addon_store_runtime_ready "$id" || return
  identity_after_reboot="$(addon_store_identity)" || { fail "$id" "could not identify add-on store after host reboot"; return; }
  [[ "$identity_before" == "$identity_after_reboot" ]] || {
    fail "$id" "host reboot reopened a different add-on store: before=$identity_before after=$identity_after_reboot"
    return
  }
  ha_cli_run addons logs "$addon_slug" | grep -qi 'existing store' || { fail "$id" "host reboot logs did not report existing store"; return; }
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
  timeout "$th09_sample_seconds" mosquitto_sub -h "$mqtt_host" -p "$mqtt_port" -v -t 'homeassistant/#' -t 'frigate/#' > "$capture" 2>"$sub_err" &
  local sub_pid=$!
  sleep 2
  ha_cli_run addons start "$addon_slug" >/dev/null || { fail "$id" "Supervisor did not start $addon_slug for MQTT audit"; rm -f "$capture" "$sub_err"; return; }
  wait_for_health || { fail "$id" "Vigil health did not reach 200 for MQTT audit"; rm -f "$capture" "$sub_err"; return; }
  ha_cli_run addons stop "$addon_slug" >/dev/null || { fail "$id" "Supervisor did not stop $addon_slug for MQTT audit"; rm -f "$capture" "$sub_err"; return; }
  wait "$sub_pid"
  sub_status=$?
  if [[ "$sub_status" -ne 0 && "$sub_status" -ne 124 ]]; then
    fail "$id" "MQTT topic audit subscription failed: $(tr '\n' ' ' < "$sub_err")"
    rm -f "$capture" "$sub_err"
    return
  fi
  if [[ -s "$capture" ]]; then
    fail "$id" "MQTT messages appeared on Home Assistant or Frigate topics during Vigil lifecycle"
    rm -f "$capture" "$sub_err"
    return
  fi
  rm -f "$capture" "$sub_err"
  pass "$id" "no Vigil MQTT discovery or Frigate-topic messages observed"
}

th10() {
  local id="TH-10"
  require_file "$id" "$addon_dir/config.yaml" || return
  have_cmd docker || { fail "$id" "docker is required for runtime port audit"; return; }
  ha_cli_available || { fail "$id" "Home Assistant CLI '$ha_cli' is not available"; return; }
  ensure_addon_installed "$id" || return
  local ports
  ports="$(declared_addon_ports)"
  if [[ "$ports" != "8099/tcp" ]]; then
    fail "$id" "config.yaml must declare exactly 8099/tcp and no other ports; found: ${ports:-none}"
    return
  fi
  ha_cli_run addons start "$addon_slug" >/dev/null || { fail "$id" "Supervisor did not start $addon_slug for runtime port audit"; return; }
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
  ha_cli_run addons start "$addon_slug" >/dev/null || { fail "$id" "version-A start failed before update"; return; }
  wait_for_health || { fail "$id" "version-A did not reach health before update"; return; }
  addon_store_runtime_ready "$id" || return
  identity_before="$(addon_store_identity)" || { fail "$id" "could not identify add-on store before update"; return; }
  options_before="$(addon_options_snapshot)" || { fail "$id" "could not snapshot Supervisor options before update"; return; }
  cp -R "$update_addon_dir"/. "$addon_dir"/ || { fail "$id" "could not copy version-B add-on files"; return; }
  ha_cli_run addons rebuild "$addon_slug" >/dev/null || { fail "$id" "Supervisor rebuild failed for version B"; return; }
  ha_cli_run addons restart "$addon_slug" >/dev/null || { fail "$id" "Supervisor restart failed after update"; return; }
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
  ha_cli_run addons logs "$addon_slug" | grep -qi 'existing store' || { fail "$id" "update logs did not report existing store"; return; }
  pass "$id" "update preserves state and options"
}

th01
th02
th03
th04
th05
th06
th07
th08
th09
th10
th11

exit "$status"
