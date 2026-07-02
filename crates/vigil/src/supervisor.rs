/// Shared Supervisor HTTP helper for REST API calls (curl-based, no extra deps).
/// Gated on SUPERVISOR_TOKEN being present.
use serde::Deserialize;

use crate::ha_mqtt_tasks::MqttConfig;

/// GET from a Supervisor REST URL. Returns the response body on success, Err on failure.
pub(crate) fn supervisor_get(url: &str, token: &str) -> Result<String, String> {
    let auth = format!("Authorization: Bearer {token}");
    let output = std::process::Command::new("curl")
        .args(["-s", "-f", url, "-H", &auth])
        .output()
        .map_err(|e| format!("curl spawn: {e}"))?;
    if !output.status.success() {
        return Err(format!("supervisor GET {url} failed"));
    }
    String::from_utf8(output.stdout).map_err(|e| format!("curl output utf8: {e}"))
}

#[derive(Debug, Deserialize)]
struct SupervisorMqttData {
    host: Option<String>,
    port: Option<u16>,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SupervisorMqttResponse {
    result: Option<String>,
    data: Option<SupervisorMqttData>,
}

/// POST to a Supervisor REST URL and return the response body on 2xx, Err on failure.
/// Unlike `supervisor_post` which discards the body, this variant returns it so callers
/// can parse structured responses (e.g. config-flow flow_id + result type).
pub(crate) fn supervisor_post_body(url: &str, token: &str, body: &str) -> Result<String, String> {
    let auth = format!("Authorization: Bearer {token}");
    // `-w "\n%{http_code}"` appends a newline + status code after the body so we can
    // split on the LAST newline to get (body, code) cleanly even when body contains newlines.
    let output = std::process::Command::new("curl")
        .args([
            "-s",
            "-w",
            "\n%{http_code}",
            "-X",
            "POST",
            url,
            "-H",
            "Content-Type: application/json",
            "-H",
            &auth,
            "-d",
            body,
        ])
        .output()
        .map_err(|e| format!("curl spawn: {e}"))?;
    let raw = String::from_utf8_lossy(&output.stdout);
    let (response_body, code_str) = raw.rsplit_once('\n').unwrap_or(("", raw.as_ref()));
    let code: u16 = code_str.trim().parse().unwrap_or(0);
    if (200..300).contains(&code) {
        Ok(response_body.to_string())
    } else {
        Err(format!(
            "supervisor POST {url} returned {code}: {}",
            &response_body[..response_body.len().min(200)]
        ))
    }
}

// ── Generic Camera config-flow helpers ────────────────────────────────────────
//
// HA's MQTT camera platform is image-only (no `stream_source` key).  Live video
// requires a Generic Camera config entry created via the HA config-flow API.
// These pure functions build the payloads; `register_generic_camera` in runtime.rs
// drives the actual HTTP calls.

/// Build the flow-start payload that targets the HA Generic Camera integration.
pub(crate) fn build_generic_camera_flow_start_payload() -> String {
    r#"{"handler":"generic"}"#.to_string()
}

/// Build the user-step payload for the Generic Camera config-flow.
///
/// `stream_source` is the camera's own RTSP URL, reached directly by HA Core. On
/// HA 2024.11+ HA serves the entity over WebRTC via its built-in go2rtc with no
/// separate stream registration; `rtsp_transport: "tcp"` avoids UDP packet loss.
/// `still_image_url` is optional — a fallback JPEG snapshot for non-WebRTC clients.
///
/// Note: H.265 cameras fail browser WebRTC negotiation; transcoding is out of scope.
/// HA falls back to HLS automatically on older deployments / non-WebRTC clients.
pub(crate) fn build_generic_camera_flow_step_payload(
    stream_source: &str,
    still_image_url: Option<&str>,
) -> String {
    // HA's generic-camera config-flow user step nests rtsp_transport (plus the
    // REQUIRED framerate + verify_ssl) inside a required `advanced` object; a
    // top-level rtsp_transport is rejected ("extra keys not allowed").
    let mut payload = serde_json::json!({
        "stream_source": stream_source,
        "advanced": {
            "framerate": 2.0,
            "verify_ssl": false,
            "rtsp_transport": "tcp",
        },
    });
    if let Some(still) = still_image_url {
        payload["still_image_url"] = serde_json::Value::String(still.to_string());
    }
    payload.to_string()
}

/// Build the confirmation payload for HA's Generic Camera preview step.
pub(crate) fn build_generic_camera_flow_confirm_payload() -> String {
    r#"{"confirmed_ok":true}"#.to_string()
}

/// Parse the `flow_id` field from a config-flow initiation response.
/// Returns `None` if the field is absent or not a string.
pub(crate) fn parse_flow_id(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("flow_id")?.as_str().map(|s| s.to_string())
}

/// Returns `true` when a config-flow response signals a completed entry creation.
/// HA returns `{"type":"create_entry","result":{"entry_id":"...",...}}` on success.
pub(crate) fn is_flow_create_entry(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(|t| t.as_str())
                .map(|t| t == "create_entry")
        })
        .unwrap_or(false)
}

/// Parse the `GET /services/mqtt` response body into an `MqttConfig`.
/// Returns None if the response is missing, has a non-"ok" result, or lacks a host.
/// This is a pure function — testable without network I/O.
pub(crate) fn parse_supervisor_mqtt_response(json: &str) -> Option<MqttConfig> {
    let resp: SupervisorMqttResponse = serde_json::from_str(json).ok()?;
    if resp.result.as_deref() != Some("ok") {
        return None;
    }
    let data = resp.data?;
    let host = data.host.filter(|h| !h.is_empty())?;
    Some(MqttConfig {
        broker_host: host,
        broker_port: data.port.unwrap_or(1883),
        username: data.username.filter(|s| !s.is_empty()),
        password: data.password.filter(|s| !s.is_empty()),
    })
}

/// Attempt to discover the MQTT broker via the HA Supervisor services API.
/// Returns None if SUPERVISOR_TOKEN is absent, the API call fails, or the
/// response carries no host (e.g. Mosquitto add-on is not installed).
pub(crate) fn fetch_supervisor_mqtt() -> Option<MqttConfig> {
    let token = std::env::var("SUPERVISOR_TOKEN").ok()?;
    let body = supervisor_get("http://supervisor/services/mqtt", &token).ok()?;
    parse_supervisor_mqtt_response(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_supervisor_mqtt_response_valid() {
        let json = r#"{"result":"ok","data":{"host":"core-mosquitto","port":1883,"ssl":false,"username":"homeassistant","password":"secret"}}"#;
        let cfg = parse_supervisor_mqtt_response(json).expect("must parse valid response");
        assert_eq!(cfg.broker_host, "core-mosquitto");
        assert_eq!(cfg.broker_port, 1883);
        assert_eq!(cfg.username.as_deref(), Some("homeassistant"));
        assert_eq!(cfg.password.as_deref(), Some("secret"));
    }

    #[test]
    fn parse_supervisor_mqtt_response_missing_host_returns_none() {
        let json = r#"{"result":"ok","data":{"port":1883}}"#;
        assert!(
            parse_supervisor_mqtt_response(json).is_none(),
            "missing host must return None — no broker to connect to"
        );
    }

    #[test]
    fn parse_supervisor_mqtt_response_empty_host_returns_none() {
        let json = r#"{"result":"ok","data":{"host":"","port":1883}}"#;
        assert!(
            parse_supervisor_mqtt_response(json).is_none(),
            "empty host must return None"
        );
    }

    #[test]
    fn parse_supervisor_mqtt_response_non_ok_result_returns_none() {
        let json = r#"{"result":"error","data":{"host":"core-mosquitto"}}"#;
        assert!(
            parse_supervisor_mqtt_response(json).is_none(),
            "non-ok result must return None"
        );
    }

    #[test]
    fn parse_supervisor_mqtt_response_malformed_json_returns_none() {
        assert!(
            parse_supervisor_mqtt_response("not json").is_none(),
            "malformed JSON must return None — graceful fallback"
        );
    }

    // ── Generic Camera config-flow helper tests ────────────────────────────

    #[test]
    fn build_generic_camera_flow_start_payload_has_generic_handler() {
        let payload = build_generic_camera_flow_start_payload();
        let v: serde_json::Value =
            serde_json::from_str(&payload).expect("flow start payload must be valid JSON");
        assert_eq!(
            v["handler"].as_str(),
            Some("generic"),
            "flow start payload must use handler='generic' to target the Generic Camera integration; \
             wrong handler silently starts a different integration"
        );
    }

    #[test]
    fn build_generic_camera_flow_step_payload_nests_advanced_fields() {
        // HA's generic-camera flow rejects a top-level rtsp_transport; rtsp_transport,
        // framerate, and verify_ssl live inside the REQUIRED `advanced` object.
        let payload = build_generic_camera_flow_step_payload("rtsp://10.0.0.5:554/stream", None);
        let v: serde_json::Value =
            serde_json::from_str(&payload).expect("flow step payload must be valid JSON");
        assert_eq!(
            v["stream_source"].as_str(),
            Some("rtsp://10.0.0.5:554/stream"),
            "step payload must carry the stream_source URL verbatim"
        );
        assert!(
            v.get("rtsp_transport").is_none(),
            "rtsp_transport must NOT be a top-level key — HA rejects it ('extra keys not allowed')"
        );
        assert_eq!(
            v["advanced"]["rtsp_transport"].as_str(),
            Some("tcp"),
            "rtsp_transport=tcp must be nested inside the required `advanced` object"
        );
        assert!(
            v["advanced"].get("framerate").is_some() && v["advanced"].get("verify_ssl").is_some(),
            "advanced must include the required framerate + verify_ssl keys"
        );
        assert!(
            v.get("still_image_url").is_none(),
            "still_image_url must be absent when not provided"
        );
    }

    #[test]
    fn build_generic_camera_flow_step_payload_includes_still_image_url_when_provided() {
        let payload = build_generic_camera_flow_step_payload(
            "rtsp://127.0.0.1:8554/test-cam",
            Some("http://example.com/snapshot.jpg"),
        );
        let v: serde_json::Value =
            serde_json::from_str(&payload).expect("flow step payload must be valid JSON");
        assert_eq!(
            v["still_image_url"].as_str(),
            Some("http://example.com/snapshot.jpg"),
            "still_image_url must be included in the payload when provided"
        );
    }

    #[test]
    fn build_generic_camera_flow_confirm_payload_accepts_preview_step() {
        let payload = build_generic_camera_flow_confirm_payload();
        let v: serde_json::Value =
            serde_json::from_str(&payload).expect("flow confirm payload must be valid JSON");
        assert_eq!(
            v.get("confirmed_ok").and_then(serde_json::Value::as_bool),
            Some(true),
            "Generic Camera preview step requires confirmed_ok=true"
        );
    }

    #[test]
    fn flow_step_errors_extracts_generic_camera_validation_errors() {
        let response = r#"{
            "type": "form",
            "step_id": "user",
            "errors": {
                "base": "cannot_connect",
                "stream_source": "invalid_url"
            }
        }"#;

        let errors = flow_step_errors(response).expect("validation errors must be extracted");
        assert!(
            errors.contains("base=cannot_connect"),
            "base error must be visible in the log-safe summary: {errors}"
        );
        assert!(
            errors.contains("stream_source=invalid_url"),
            "field error must be visible in the log-safe summary: {errors}"
        );
    }

    #[test]
    fn flow_step_errors_ignores_non_error_responses() {
        assert!(
            flow_step_errors(r#"{"type":"create_entry","result":{"entry_id":"abc"}}"#).is_none(),
            "successful create_entry responses must not be treated as validation errors"
        );
        assert!(
            flow_step_errors(r#"{"type":"form","step_id":"confirm","errors":{}}"#).is_none(),
            "empty errors objects must not block the confirmation step"
        );
    }

    #[test]
    fn parse_flow_id_extracts_from_valid_response() {
        let json = r#"{"flow_id":"abc123","type":"form","step_id":"user"}"#;
        assert_eq!(
            parse_flow_id(json),
            Some("abc123".to_string()),
            "must extract flow_id from a valid config-flow initiation response"
        );
    }

    #[test]
    fn parse_flow_id_returns_none_on_missing_field() {
        let json = r#"{"type":"form","step_id":"user"}"#;
        assert!(
            parse_flow_id(json).is_none(),
            "must return None when flow_id field is absent"
        );
    }

    #[test]
    fn is_flow_create_entry_true_on_create_entry_type() {
        let json = r#"{"type":"create_entry","result":{"entry_id":"xyz789","title":"Camera"}}"#;
        assert!(
            is_flow_create_entry(json),
            "must return true for type=create_entry (HA signals a completed config entry)"
        );
    }

    #[test]
    fn is_flow_create_entry_false_on_form_type() {
        let json = r#"{"type":"form","step_id":"confirm","flow_id":"abc"}"#;
        assert!(
            !is_flow_create_entry(json),
            "must return false for an intermediate form/confirm step"
        );
    }
}
