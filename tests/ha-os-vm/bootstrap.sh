#!/usr/bin/env bash
set -euo pipefail

id="TH-00"

fail() {
  printf '%s FAIL %s\n' "$id" "$1" >&2
  exit 1
}

type -P virsh >/dev/null 2>&1 || fail "missing virsh; install libvirt tooling and provide a HA OS VM baseline"
type -P curl >/dev/null 2>&1 || fail "missing curl; cannot verify Home Assistant, Mosquitto, and Frigate endpoints"
type -P jq >/dev/null 2>&1 || fail "missing jq; cannot verify HA and Frigate JSON"
type -P mosquitto_pub >/dev/null 2>&1 || fail "missing mosquitto_pub; cannot verify Mosquitto publish"
type -P mosquitto_sub >/dev/null 2>&1 || fail "missing mosquitto_sub; cannot verify Mosquitto subscribe"

vm_name="${HA_OS_VM_NAME:-vigil-ha-os}"
ha_url="${HA_URL:-http://127.0.0.1:8123}"
ha_token="${HA_TOKEN:-}"
frigate_url="${FRIGATE_URL:-http://127.0.0.1:5000}"
mqtt_host="${MQTT_HOST:-127.0.0.1}"
mqtt_port="${MQTT_PORT:-1883}"
max_vm_bytes="$((10 * 1024 * 1024 * 1024))"

virsh dominfo "$vm_name" >/dev/null 2>&1 || fail "VM '$vm_name' is not defined"
if [[ "$(virsh domstate "$vm_name")" != "running" ]]; then
  virsh start "$vm_name" >/dev/null || fail "VM '$vm_name' did not start"
fi

virsh dumpxml "$vm_name" | grep -q 'arch=.x86_64.' || fail "VM '$vm_name' is not configured as x86_64"

total_bytes=0
while IFS= read -r source; do
  [[ -e "$source" ]] || continue
  bytes="$(du -sb "$source" | awk '{print $1}')"
  total_bytes=$((total_bytes + bytes))
done < <(virsh domblklist "$vm_name" --details | awk '$3 == "disk" && $4 != "-" {print $4}')
(( total_bytes > 0 )) || fail "VM '$vm_name' has no measurable disk footprint"
(( total_bytes <= max_vm_bytes )) || fail "VM footprint exceeds 10 GB: ${total_bytes} bytes"

[[ -n "$ha_token" ]] || fail "HA_TOKEN is required to verify the Home Assistant API baseline"
ha_config="$(curl -fsS --max-time 5 -H "Authorization: Bearer $ha_token" "$ha_url/api/config")" || fail "Home Assistant API is not reachable at $ha_url"
jq -e '.location_name and .version' <<< "$ha_config" >/dev/null || fail "Home Assistant API did not return config identity"

frigate_version="$(curl -fsS --max-time 5 "$frigate_url/api/version")" || fail "Frigate API is not reachable at $frigate_url"
[[ -n "$frigate_version" ]] || fail "Frigate version endpoint was empty"
frigate_config="$(curl -fsS --max-time 5 "$frigate_url/api/config")" || fail "Frigate config API is not reachable"
frigate_stats="$(curl -fsS --max-time 5 "$frigate_url/api/stats")" || fail "Frigate stats API is not reachable"
frigate_events="$(curl -fsS --max-time 5 "$frigate_url/api/events?limit=1")" || fail "Frigate events API is not reachable"
jq -e '.cameras | length > 0' <<< "$frigate_config" >/dev/null || fail "Frigate config has no cameras"
jq -e '[.. | objects | .path? // empty | strings | select(test("rtsp://"))] | length > 0' <<< "$frigate_config" >/dev/null || fail "Frigate config has no RTSP-backed camera input"
jq -e '.cameras | length > 0' <<< "$frigate_stats" >/dev/null || fail "Frigate stats have no camera metrics"
jq -e 'length > 0' <<< "$frigate_events" >/dev/null || fail "Frigate has no visible detection events"

mqtt_topic="vigil/bootstrap/$RANDOM"
mqtt_payload="baseline-$RANDOM"
mosquitto_sub -h "$mqtt_host" -p "$mqtt_port" -C 1 -W 5 -t "$mqtt_topic" > /tmp/vigil-bootstrap-mqtt.txt &
sub_pid=$!
sleep 1
mosquitto_pub -h "$mqtt_host" -p "$mqtt_port" -t "$mqtt_topic" -m "$mqtt_payload" || fail "MQTT publish failed at $mqtt_host:$mqtt_port"
wait "$sub_pid" || fail "MQTT subscribe did not receive baseline message"
grep -qx "$mqtt_payload" /tmp/vigil-bootstrap-mqtt.txt || fail "MQTT baseline payload did not round-trip"
rm -f /tmp/vigil-bootstrap-mqtt.txt

(( SECONDS <= 600 )) || fail "baseline reconciliation exceeded 10 minutes"

printf '%s PASS baseline VM, HA, Mosquitto, and Frigate verified\n' "$id"
