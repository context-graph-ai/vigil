//! Home Assistant Generic Camera entity registration: turns a configured
//! Vigil camera into a live-viewable HA entity via the Core config-flow
//! API. Split out of `runtime.rs` so the camera-capture pipeline and this
//! publisher-facing concern stop sharing one file — a pure relocation, no
//! behavior change: every entity id, topic, registered URL, the
//! sentinel-based idempotency, and every log string are unchanged.

use crate::config;

/// Register each camera as an HA Generic Camera config entry via the Core config-flow API.
///
/// The MQTT camera platform is image-only (no `stream_source` key), so live video requires
/// a real streaming camera entity. This creates one Generic Camera per camera, pointed at
/// the camera's configured live RTSP URL when present, otherwise its detection RTSP URL.
/// HA Core reaches the camera directly and serves the entity over WebRTC via its built-in
/// go2rtc (2024.11+) with no separate stream registration.
///
/// Flow:
/// 1. Sentinel guard — if `data_dir/generic_camera_<slug>.registered` exists, skip.
/// 2. POST /core/api/config/config_entries/flow `{"handler":"generic"}` → flow_id.
/// 3. POST .../flow/<flow_id> with `stream_source` + the required `advanced` object
///    (`framerate` / `verify_ssl` / `rtsp_transport:"tcp"`).
/// 4. If HA returns an intermediate "form" step (confirm/preview), submit `{}`.
/// 5. On `"type":"create_entry"`, write the sentinel and log success.
///
/// HA version floor: 2024.11 (built-in go2rtc + WebRTC).  Below that, the entity falls back
/// to HLS (laggier but functional).  H.265 sources register but don't negotiate WebRTC.
///
/// Failures are logged and never panic; the add-on continues without live view.
// SUPERVISOR_TOKEN is an enumerated, reviewed read
// (`environment_read_surface.baseline.txt`), not an ad-hoc one — it is
// injected by the Home Assistant Supervisor, not user-adjustable.
#[allow(clippy::disallowed_methods)]
pub(crate) fn register_generic_camera(cam_slug: &str, rtsp_url: &str, data_dir: &std::path::Path) {
    // ── Sentinel-based idempotency ─────────────────────────────────────────
    // The Generic Camera config-flow API is NOT inherently idempotent — calling it
    // twice creates duplicate camera entities.  Write a marker the first time we
    // succeed so subsequent add-on restarts skip the API call entirely.
    let sentinel = data_dir.join(format!("generic_camera_{cam_slug}.registered"));
    if sentinel.exists() {
        println!("generic_camera_already_registered camera={cam_slug}");
        return;
    }

    let Ok(token) = std::env::var("SUPERVISOR_TOKEN") else {
        println!("generic_camera_skip_no_supervisor_token camera={cam_slug}");
        return;
    };

    // ── Start config-flow ─────────────────────────────────────────────────
    let start_payload = crate::supervisor::build_generic_camera_flow_start_payload();
    let flow_resp = match crate::supervisor::supervisor_post_body(
        "http://supervisor/core/api/config/config_entries/flow",
        &token,
        &start_payload,
    ) {
        Ok(resp) => resp,
        Err(e) => {
            println!("generic_camera_flow_start_error camera={cam_slug} error={e}");
            return;
        }
    };

    let flow_id = match crate::supervisor::parse_flow_id(&flow_resp) {
        Some(id) => id,
        None => {
            println!(
                "generic_camera_flow_id_missing camera={cam_slug} response={}",
                &flow_resp[..flow_resp.len().min(200)]
            );
            return;
        }
    };

    // ── Submit stream_source user step ────────────────────────────────────
    // stream_source is the camera's own RTSP URL, reached directly by HA Core;
    // HA serves WebRTC via its built-in go2rtc with no separate registration.
    let step_payload = crate::supervisor::build_generic_camera_flow_step_payload(rtsp_url, None);
    let step_url = format!("http://supervisor/core/api/config/config_entries/flow/{flow_id}");
    let step_resp = match crate::supervisor::supervisor_post_body(&step_url, &token, &step_payload)
    {
        Ok(resp) => resp,
        Err(e) => {
            println!("generic_camera_flow_step_error camera={cam_slug} error={e}");
            delete_generic_camera_flow(cam_slug, &flow_id, &token);
            return;
        }
    };
    if let Some(errors) = crate::supervisor::flow_step_errors(&step_resp) {
        println!("generic_camera_flow_step_validation_error camera={cam_slug} errors={errors}");
        delete_generic_camera_flow(cam_slug, &flow_id, &token);
        return;
    }

    // ── Handle optional confirm/preview intermediate step ─────────────────
    // Some HA versions present an extra preview/confirm form before completing
    // the entry. Accept it explicitly.
    let final_resp = if crate::supervisor::is_flow_create_entry(&step_resp) {
        step_resp
    } else {
        let confirm_payload = crate::supervisor::build_generic_camera_flow_confirm_payload();
        match crate::supervisor::supervisor_post_body(&step_url, &token, &confirm_payload) {
            Ok(resp) => resp,
            Err(e) => {
                println!("generic_camera_flow_confirm_error camera={cam_slug} error={e}");
                delete_generic_camera_flow(cam_slug, &flow_id, &token);
                return;
            }
        }
    };

    if crate::supervisor::is_flow_create_entry(&final_resp) {
        // Write sentinel so we skip on next restart.
        if let Err(e) = std::fs::write(&sentinel, b"ok") {
            println!("generic_camera_sentinel_write_error camera={cam_slug} error={e}");
        }
        println!("generic_camera_registered camera={cam_slug}");
    } else {
        if let Some(errors) = crate::supervisor::flow_step_errors(&final_resp) {
            println!(
                "generic_camera_flow_confirm_validation_error camera={cam_slug} errors={errors}"
            );
        }
        println!(
            "generic_camera_flow_unexpected_result camera={cam_slug} response={}",
            &final_resp[..final_resp.len().min(300)]
        );
        delete_generic_camera_flow(cam_slug, &flow_id, &token);
    }
}

fn delete_generic_camera_flow(cam_slug: &str, flow_id: &str, token: &str) {
    match crate::supervisor::delete_flow(flow_id, token) {
        Ok(()) => println!("generic_camera_flow_deleted camera={cam_slug} flow_id={flow_id}"),
        Err(error) => {
            println!(
                "generic_camera_flow_delete_error camera={cam_slug} flow_id={flow_id} error={error}"
            );
        }
    }
}

pub(crate) fn generic_camera_url(camera: &config::CameraEntry) -> Option<&str> {
    camera
        .live_rtsp_url
        .as_deref()
        .or(camera.rtsp_url.as_deref())
}
