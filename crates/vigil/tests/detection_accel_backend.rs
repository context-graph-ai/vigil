use std::collections::BTreeMap;
#[cfg(not(feature = "detect-burn-wgpu"))]
use std::collections::BTreeSet;
use std::fs;
#[cfg(not(feature = "detect-burn-wgpu"))]
use std::io::{Read, Write};
#[cfg(not(feature = "detect-burn-wgpu"))]
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
#[cfg(not(feature = "detect-burn-wgpu"))]
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
#[cfg(not(feature = "detect-burn-wgpu"))]
use std::time::Instant;

use vigil::acceleration::{
    AccelStage, AccelerationReceipt, ActionKind, EvidenceKind, FailureCode, ProbeStatus,
};
use vigil::detection_accel::{
    ACCELERATED_DETECTION_BACKEND, CPU_DETECTION_BACKEND, DetectionForwardProbe,
    DetectionForwardProbeOutcome, detection_hardware_claim_is_valid, select_detection_acceleration,
    select_detection_acceleration_with_probe,
};
#[cfg(not(feature = "detect-burn-wgpu"))]
use vigil::doctor::{
    DeviceFacts, DeviceOpenError, DoctorRequest, HostFacts, acceleration_report, render_report,
};

#[cfg(not(feature = "detect-burn-wgpu"))]
#[path = "ha_test_support.rs"]
mod ha_test_support;

const MODEL_ID: &str = "detector-under-test";
const INPUT_SHAPE: &str = "1x3x640x640";
#[cfg(not(feature = "detect-burn-wgpu"))]
const MISLEADING_ACTION: &str = "install a build with an accelerated detector backend";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn vigil_binary_path() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_vigil") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_vigil") {
        return PathBuf::from(path);
    }
    workspace_root()
        .join("target")
        .join("debug")
        .join(if cfg!(windows) { "vigil.exe" } else { "vigil" })
}

#[cfg(not(feature = "detect-burn-wgpu"))]
struct RuntimeProbe {
    child: Option<Child>,
    stdout: Arc<std::sync::Mutex<String>>,
    stderr: Arc<std::sync::Mutex<String>>,
}

#[cfg(not(feature = "detect-burn-wgpu"))]
impl RuntimeProbe {
    fn logs(&self) -> String {
        let stdout = self
            .stdout
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        let stderr = self
            .stderr
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        format!("{stdout}{stderr}")
    }

    fn terminate(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
    }
}

#[cfg(not(feature = "detect-burn-wgpu"))]
impl Drop for RuntimeProbe {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(not(feature = "detect-burn-wgpu"))]
enum RuntimeStatsAttempt {
    Stats(String),
    TransientPortCollision(String),
    NoStats(String),
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn logs_show_transient_health_port_collision(logs: &str) -> bool {
    let lower = logs.to_ascii_lowercase();
    lower.contains("health port")
        && lower.contains("bind failed")
        && (lower.contains("address already in use") || lower.contains("os error 98"))
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn runtime_stats_text_from_real_runtime() -> Option<String> {
    let fixture_lock = ha_test_support::rtsp_fixture_lock().lock();
    assert!(
        fixture_lock.is_ok(),
        "RTSP fixture lock must be available for runtime stats probe"
    );
    let Ok(_fixture_lock) = fixture_lock else {
        return None;
    };

    let root = workspace_root();
    let clip = root
        .join("tests")
        .join("fixtures")
        .join("video")
        .join("one-by-one-person-detection.mp4");
    assert!(
        clip.is_file(),
        "runtime stats probe needs the person-detection fixture at {}",
        clip.display()
    );
    let model = root
        .join("tests")
        .join("fixtures")
        .join("models")
        .join("yolox-tiny-coco.pth");
    assert!(
        model.is_file(),
        "runtime stats probe needs the detector model fixture at {}",
        model.display()
    );

    let mut last_collision_logs = String::new();
    for _attempt in 0..3 {
        match runtime_stats_text_attempt(&clip, &model) {
            RuntimeStatsAttempt::Stats(stats) => return Some(stats),
            RuntimeStatsAttempt::TransientPortCollision(logs) => {
                last_collision_logs = logs;
                continue;
            }
            RuntimeStatsAttempt::NoStats(logs) => {
                let published_stats = false;
                assert!(
                    published_stats,
                    "runtime must publish detection acceleration into public stats and /health; logs:\n{logs}"
                );
                return None;
            }
        }
    }
    let avoided_collision = false;
    assert!(
        avoided_collision,
        "runtime stats probe could not avoid a transient health-port bind collision after retries; logs:\n{last_collision_logs}"
    );
    None
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn runtime_stats_text_attempt(clip: &Path, model: &Path) -> RuntimeStatsAttempt {
    let temp = tempfile::tempdir();
    assert!(
        temp.is_ok(),
        "temp directory must be available for runtime stats probe"
    );
    let Ok(temp) = temp else {
        return RuntimeStatsAttempt::NoStats(String::new());
    };
    let data_dir = temp.path().join("data");
    let store_path = data_dir.join("store.contextgraph");
    let create_data_dir = fs::create_dir_all(&data_dir);
    assert!(
        create_data_dir.is_ok(),
        "runtime stats probe data dir must be writable"
    );
    let health_port = ha_test_support::free_port();
    assert!(
        health_port.is_ok(),
        "health port must be available for runtime stats probe"
    );
    let Ok(health_port) = health_port else {
        return RuntimeStatsAttempt::NoStats(String::new());
    };
    let review_port = ha_test_support::free_port();
    assert!(
        review_port.is_ok(),
        "review port must be available for runtime stats probe"
    );
    let Ok(review_port) = review_port else {
        return RuntimeStatsAttempt::NoStats(String::new());
    };
    let rtsp = ha_test_support::RtspFixture::start(clip);
    assert!(
        rtsp.is_ok(),
        "RTSP fixture must start for runtime stats probe"
    );
    let Ok(rtsp) = rtsp else {
        return RuntimeStatsAttempt::NoStats(String::new());
    };

    let config_path = temp.path().join("runtime-stats-probe.toml");
    let config = format!(
        "data_dir = \"{}\"\nstore_path = \"{}\"\nhealth_port = {}\nreview_port = {}\n\
         site_name = \"local test\"\ncamera_name = \"test camera\"\nrtsp_url = \"{}\"\n\
         detector_model_id = \"{}\"\ndetector_model_path = \"{}\"\ndetector_sample_frames = 1\n",
        ha_test_support::toml_path(&data_dir),
        ha_test_support::toml_path(&store_path),
        health_port,
        review_port,
        ha_test_support::toml_string(&rtsp.url),
        MODEL_ID,
        ha_test_support::toml_path(model),
    );
    let write_config = fs::write(&config_path, config);
    assert!(
        write_config.is_ok(),
        "runtime stats probe config must be writable"
    );

    let spawn = Command::new(vigil_binary_path())
        .arg("run")
        .arg("--config")
        .arg(&config_path)
        .env("VIGIL_RTSP_RETRY_INITIAL_MS", "200")
        .env("VIGIL_RTSP_RETRY_MAX_MS", "1000")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    assert!(spawn.is_ok(), "vigil runtime must start for stats probe");
    let Ok(mut child) = spawn else {
        return RuntimeStatsAttempt::NoStats(String::new());
    };
    let stdout = ha_test_support::capture_pipe(child.stdout.take());
    let stderr = ha_test_support::capture_pipe(child.stderr.take());
    let mut runtime = RuntimeProbe {
        child: Some(child),
        stdout,
        stderr,
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut latest_surfaces = None;
    while Instant::now() < deadline {
        let stats = read_public_stats(&data_dir);
        let health = read_health_endpoint(health_port);
        if let (Some(stats), Some(health)) = (stats, health) {
            let has_detection_stats = public_surface_mentions_detection_receipt(&stats);
            let has_detection_health = public_surface_mentions_detection_receipt(&health);
            if has_detection_stats || has_detection_health {
                latest_surfaces = Some(format!(
                    "[runtime.stats]\n{stats}\n[runtime.health]\n{health}"
                ));
                break;
            }
        }
        if ha_test_support::runtime_reached_pipeline_terminal_signal(&runtime.logs()) {
            let stats = read_public_stats(&data_dir).unwrap_or_default();
            let health = read_health_endpoint(health_port).unwrap_or_default();
            latest_surfaces = Some(format!(
                "[runtime.stats]\n{stats}\n[runtime.health]\n{health}"
            ));
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let logs = runtime.logs();
    runtime.terminate();
    if let Some(surfaces) = latest_surfaces {
        RuntimeStatsAttempt::Stats(surfaces)
    } else if logs_show_transient_health_port_collision(&logs) {
        RuntimeStatsAttempt::TransientPortCollision(logs)
    } else {
        RuntimeStatsAttempt::NoStats(logs)
    }
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn read_public_stats(data_dir: &Path) -> Option<String> {
    let output = Command::new(vigil_binary_path())
        .arg("stats")
        .env("VIGIL_DATA_DIR", data_dir)
        .output();
    let Ok(output) = output else {
        return None;
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.status.success().then_some(text)
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn read_health_endpoint(port: u16) -> Option<String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(200)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    stream
        .write_all(b"GET /health HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn public_surface_mentions_detection_receipt(surface: &str) -> bool {
    surface.contains("detection-acceleration=")
        || surface.contains("[detect.acceleration]")
        || surface.contains("active-detector-backend=")
        || surface.contains("backend_not_compiled")
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn runtime_surface_section<'a>(surfaces: &'a str, marker: &str) -> &'a str {
    let Some(start) = surfaces.find(marker) else {
        return "";
    };
    let after = &surfaces[start + marker.len()..];
    if let Some(end) = after.find("\n[runtime.") {
        &after[..end]
    } else {
        after
    }
}

fn base_detection_receipt() -> AccelerationReceipt {
    AccelerationReceipt {
        stage: AccelStage::Detection,
        work_id: None,
        parent_work_id: None,
        stream_id: None,
        media_item: None,
        configured: true,
        attempted_backend: ACCELERATED_DETECTION_BACKEND.to_string(),
        active_backend: ACCELERATED_DETECTION_BACKEND.to_string(),
        hardware_accelerated: true,
        selected_device: Some("discrete accelerator".to_string()),
        codec: None,
        model_id: Some(MODEL_ID.to_string()),
        model_version: None,
        input_shape: Some(INPUT_SHAPE.to_string()),
        probe_status: ProbeStatus::Active,
        failure_code: FailureCode::None,
        evidence_kind: Some(EvidenceKind::BackendProbe),
        evidence_fields: BTreeMap::from([("forward_probe".to_string(), "passed".to_string())]),
        action_kind: ActionKind::NoAction,
        action_payload: None,
    }
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn parsed_backend_set(raw: &str) -> BTreeSet<&str> {
    raw.split([',', ';', '|', ' ', '[', ']'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect()
}

fn assert_invalid_hardware_claim(receipt: AccelerationReceipt, reason: &str) {
    assert!(
        !detection_hardware_claim_is_valid(&receipt),
        "validator must reject hardware=true when {reason}"
    );
}

fn assert_valid_hardware_claim(receipt: &AccelerationReceipt, reason: &str) {
    assert!(
        detection_hardware_claim_is_valid(receipt),
        "validator must accept hardware=true when {reason}"
    );
}

#[cfg(feature = "detect-burn-wgpu")]
fn selected_device_is_non_cpu(receipt: &AccelerationReceipt) -> bool {
    let Some(device) = receipt.selected_device.as_deref() else {
        return false;
    };
    if device.trim().is_empty() {
        return false;
    }
    let lower = device.to_ascii_lowercase();
    !["cpu", "llvmpipe", "lavapipe", "software", "swrast"]
        .iter()
        .any(|needle| lower.contains(needle))
}

#[cfg(feature = "detect-burn-wgpu")]
fn function_body(source: &str, name: &str) -> Option<String> {
    let signature = format!("pub fn {name}");
    let start = source.find(&signature)?;
    let body_start = source[start..].find('{')? + start;
    let mut depth = 0usize;
    for (offset, ch) in source[body_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(source[body_start..=body_start + offset].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(feature = "detect-burn-wgpu")]
fn split_top_level_args(args: &str) -> Vec<String> {
    let mut split = Vec::new();
    let mut start = 0usize;
    let mut paren_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut single_quoted = false;
    let mut double_quoted = false;
    let mut escaped = false;
    for (idx, ch) in args.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if single_quoted {
            if ch == '\'' {
                single_quoted = false;
            }
            continue;
        }
        if double_quoted {
            if ch == '"' {
                double_quoted = false;
            }
            continue;
        }
        match ch {
            '\'' => single_quoted = true,
            '"' => double_quoted = true,
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            ',' if paren_depth == 0 && brace_depth == 0 && bracket_depth == 0 => {
                split.push(args[start..idx].trim().to_string());
                start = idx + 1;
            }
            _ => {}
        }
    }
    split.push(args[start..].trim().to_string());
    if split.last().is_some_and(|arg| arg.is_empty()) {
        split.pop();
    }
    split
}

#[cfg(feature = "detect-burn-wgpu")]
fn detection_forward_probe_impls(source: &str) -> Vec<(String, String)> {
    let signature = "impl DetectionForwardProbe for";
    let mut impls = Vec::new();
    for (start, _) in source.match_indices(signature) {
        let type_start = start + signature.len();
        let Some(body_start) = source[type_start..]
            .find('{')
            .map(|offset| type_start + offset)
        else {
            continue;
        };
        let type_name = source[type_start..body_start].trim().to_string();
        if type_name.is_empty() {
            continue;
        }
        let mut depth = 0usize;
        for (offset, ch) in source[body_start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        impls.push((
                            type_name,
                            source[body_start..=body_start + offset].to_string(),
                        ));
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    impls
}

#[cfg(feature = "detect-burn-wgpu")]
fn probe_arg_names_impl(probe_arg: &str, impl_type: &str) -> bool {
    let type_tail = impl_type
        .rsplit("::")
        .next()
        .unwrap_or(impl_type)
        .trim()
        .trim_matches('&');
    !type_tail.is_empty()
        && (probe_arg.contains(impl_type)
            || probe_arg.contains(type_tail)
            || probe_arg.contains(&format!("{type_tail}::"))
            || probe_arg.contains(&format!("{type_tail} {{")))
}

#[cfg(feature = "detect-burn-wgpu")]
fn probe_body_has_live_yolox_forward(body: &str) -> bool {
    let compact = body
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();
    if [
        "iffalse",
        "ifcfg!(",
        "unreachable!()",
        "todo!()",
        "unimplemented!()",
        "DetectionForwardProbeOutcome::NoDeviceVisible",
        "returnDetectionForwardProbeOutcome::NoDeviceVisible",
        "returnDetectionForwardProbeOutcome::Failed",
    ]
    .iter()
    .any(|forbidden| compact.contains(forbidden))
    {
        return false;
    }
    let Some(load_at) = body.find("load_detector(") else {
        return false;
    };
    let Some(detect_at) = body.find("detect_frame(") else {
        return false;
    };
    let Some(passed_at) = body.find("DetectionForwardProbeOutcome::Passed") else {
        return false;
    };
    load_at < detect_at
        && detect_at < passed_at
        && body.contains("selected_device")
        && (body.contains("model_sha256")
            || body.contains("model_forward_sha256")
            || body.contains("result_sha256"))
}

#[cfg(feature = "detect-burn-wgpu")]
fn compact_source(source: &str) -> String {
    source
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
}

#[cfg(feature = "detect-burn-wgpu")]
fn assert_live_yolox_probe_guard_rejects_dead_text() {
    let dead_probe = r#"{
        fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
            if false {
                let detector = load_detector(self.model.as_deref())?;
                let output = detect_frame(&detector, &self.clip, 1, 0.25)?;
                return DetectionForwardProbeOutcome::Passed {
                    selected_device: "gpu".to_string(),
                    evidence_fields: BTreeMap::new(),
                };
            }
            DetectionForwardProbeOutcome::NoDeviceVisible
        }
    }"#;
    assert!(
        !probe_body_has_live_yolox_forward(dead_probe),
        "probe source guard must reject unreachable YOLOX-looking text"
    );
    let no_op_helper_probe = r#"{
        fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
            run_detector_probe();
            DetectionForwardProbeOutcome::NoDeviceVisible
        }
    }"#;
    assert!(
        !probe_body_has_live_yolox_forward(no_op_helper_probe),
        "probe source guard must reject a no-op helper call that does not load and detect through YOLOX"
    );
    let live_probe = r#"{
        fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
            let detector = load_detector(self.model.as_deref())?;
            let output = detect_frame(&detector, &self.clip, 1, 0.25)?;
            let selected_device = output.detector_backend.clone();
            let mut evidence_fields = BTreeMap::new();
            evidence_fields.insert("model_forward_sha256".to_string(), output.model_forward_sha256);
            DetectionForwardProbeOutcome::Passed {
                selected_device,
                evidence_fields,
            }
        }
    }"#;
    assert!(
        probe_body_has_live_yolox_forward(live_probe),
        "probe source guard must allow a direct detector load plus detect-frame path"
    );
}

#[cfg(feature = "detect-burn-wgpu")]
fn assert_production_selector_delegates_to_probe_seam() {
    assert_live_yolox_probe_guard_rejects_dead_text();
    let source_path = workspace_root()
        .join("crates")
        .join("vigil")
        .join("src")
        .join("detection_accel.rs");
    let source = fs::read_to_string(&source_path);
    assert!(
        source.is_ok(),
        "detection acceleration source must be readable at {}",
        source_path.display()
    );
    let Ok(source) = source else {
        return;
    };
    let body = function_body(&source, "select_detection_acceleration");
    assert!(
        body.is_some(),
        "production detection selector must be nameable in {}",
        source_path.display()
    );
    let Some(body) = body else {
        return;
    };
    let delegate_call = "select_detection_acceleration_with_probe(";
    let delegate_count = body.match_indices(delegate_call).count();
    assert_eq!(
        delegate_count, 1,
        "production detection selector must delegate exactly once to the observable probe seam, not mix the seam with another receipt path: {body}"
    );
    assert!(
        body.contains(delegate_call),
        "production detection selector must delegate to the observable probe seam, not build its own receipt: {body}"
    );
    let call_start = body.find(delegate_call).unwrap_or(usize::MAX);
    let call_open = call_start + delegate_call.len() - 1;
    let mut depth = 0usize;
    let mut call_end = None;
    for (offset, ch) in body[call_open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    call_end = Some(call_open + offset + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(
        call_end.is_some(),
        "production selector probe-seam call must parse as a complete tail expression: {body}"
    );
    let Some(call_end) = call_end else {
        return;
    };
    let call_text = &body[call_start..call_end];
    let call_args = split_top_level_args(&call_text[delegate_call.len()..call_text.len() - 1]);
    assert_eq!(
        call_args.len(),
        4,
        "production selector must pass intent, model, input shape, and one real probe into the probe seam: {call_text}"
    );
    let production_probe_arg = call_args[3].trim();
    let suffix = body[call_end..].trim();
    assert_eq!(
        suffix, "}",
        "production detection selector must tail-return the probe-seam result; it must not delegate, discard, and then return a canned helper receipt: {body}"
    );
    for forbidden in [
        "AccelerationReceipt",
        "NoopDetectionForwardProbe",
        "DetectionForwardProbeOutcome::NoDeviceVisible",
        "hardware_accelerated: true",
        "ProbeStatus::Active",
        "forward_probe",
        "accelerated_latency_ms",
        "cpu_baseline_latency_ms",
    ] {
        assert!(
            !body.contains(forbidden),
            "production detection selector must not inline canned Active receipt evidence `{forbidden}`: {body}"
        );
    }
    let source_without_comments = source
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        source_without_comments.contains("yolox")
            || source_without_comments.contains("Yolox")
            || source_without_comments.contains("YOLOX"),
        "production detection selector must be backed by the real YOLOX detector probe, not a no-op probe that always reports no device"
    );
    let probe_impls = detection_forward_probe_impls(&source_without_comments)
        .into_iter()
        .filter(|(impl_type, body)| {
            !impl_type.contains("NoopDetectionForwardProbe")
                && !body.contains("NoopDetectionForwardProbe")
        })
        .collect::<Vec<_>>();
    assert!(
        !probe_impls.is_empty(),
        "production detection selector must provide a non-noop DetectionForwardProbe implementation"
    );
    let selected_probe_impls = probe_impls
        .iter()
        .filter(|(impl_type, _)| probe_arg_names_impl(production_probe_arg, impl_type))
        .collect::<Vec<_>>();
    assert!(
        !selected_probe_impls.is_empty(),
        "production selector must pass the real non-noop DetectionForwardProbe implementation into the seam, not a synthetic probe plus a dead YOLOX-looking impl; probe arg was `{production_probe_arg}`, impls were {probe_impls:?}"
    );
    assert!(
        selected_probe_impls
            .iter()
            .any(|(_, body)| probe_body_has_live_yolox_forward(body)),
        "the DetectionForwardProbe actually passed by the production selector must run the real YOLOX forward path and carry a passing selected-device result, not a fake probe that always reports no device: probe arg `{production_probe_arg}`, selected impls {selected_probe_impls:?}"
    );
}

#[cfg(feature = "detect-burn-wgpu")]
fn assert_runtime_detection_receipt_uses_selection_seam() {
    let source_path = workspace_root()
        .join("crates")
        .join("vigil")
        .join("src")
        .join("runtime.rs");
    let source = fs::read_to_string(&source_path);
    assert!(
        source.is_ok(),
        "runtime source must be readable at {}",
        source_path.display()
    );
    let Ok(source) = source else {
        return;
    };
    let source_without_comments = source
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let private_receipt_builders = source_without_comments
        .match_indices("fn ")
        .filter_map(|(idx, _)| {
            let after_fn = &source_without_comments[idx + "fn ".len()..];
            let name = after_fn
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect::<String>();
            if !name.contains("receipt") || name == "receipt_line_sink" {
                return None;
            }
            let body = function_body(&source_without_comments, &name)?;
            (body.contains("AccelerationReceipt {")
                || body.contains("-> crate::acceleration::AccelerationReceipt"))
            .then_some(name)
        })
        .collect::<Vec<_>>();
    assert!(
        private_receipt_builders.is_empty(),
        "runtime must not keep renamed private receipt builders that can diverge from the detection selector seam: {private_receipt_builders:?}"
    );
    assert!(
        source.contains("select_detection_acceleration("),
        "runtime stats/health detection receipts must be produced through the public detection selector seam, not a private hardcoded receipt"
    );
    assert!(
        !source.contains("fn detection_acceleration_receipt("),
        "runtime must not keep a private detection_acceleration_receipt path that can diverge from the feature-enabled production selector"
    );
    assert!(
        !source.contains("install a build with an accelerated detector backend"),
        "runtime detection receipts must not keep the stale install-build action after the single selector seam is introduced"
    );
    let start_body = function_body(&source_without_comments, "start_rtsp_probe");
    assert!(
        start_body.is_some(),
        "runtime source guard must inspect the detector startup path"
    );
    let Some(start_body) = start_body else {
        return;
    };
    assert!(
        !start_body.contains("AccelerationReceipt {"),
        "runtime detector startup must not build a private detection receipt after the selector seam exists"
    );
    let compact = compact_source(&start_body);
    // The public selector seam is either the plain selector or the
    // late-recording variant the runtime now routes through
    // (`select_detection_acceleration_recording`, which keeps the probe alive
    // past the deadline and records the late outcome). Both derive the receipt
    // from the public selector — neither is a private receipt builder.
    let selector_call_forms = [
        "select_detection_acceleration(",
        "select_detection_acceleration_recording(",
    ];
    let direct_receipt = selector_call_forms
        .iter()
        .any(|form| compact.contains(&format!("letreceipt={form}")))
        && compact.contains(".receipt");
    let selection_receipt = selector_call_forms
        .iter()
        .any(|form| compact.contains(&format!("letselection={form}")))
        && compact.contains("letreceipt=selection.receipt");
    assert!(
        direct_receipt || selection_receipt,
        "runtime detector startup must derive its detection receipt from the public select_detection_acceleration(_recording) seam, not from a dead selector call plus private receipt builder: {start_body}"
    );
    for required in [
        "stats.active_detector_backend=receipt.active_backend.clone()",
        "stats.detection_acceleration=format!(",
        "accel.record(receipt)",
    ] {
        assert!(
            compact.contains(required),
            "runtime detector startup must feed the selector-derived receipt into stats and acceleration state; missing `{required}` in {start_body}"
        );
    }
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn rendered_receipt_block(surface: &str, header: &str) -> Option<String> {
    let mut in_block = false;
    let mut block = String::new();
    for line in surface.lines() {
        let trimmed = line.trim();
        if trimmed == header {
            if in_block {
                break;
            }
            in_block = true;
        } else if in_block && trimmed.starts_with('[') {
            break;
        }
        if in_block {
            if !block.is_empty() {
                block.push('\n');
            }
            block.push_str(line);
        }
    }
    (!block.is_empty()).then_some(block)
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn assert_cpu_detection_action(action_payload: Option<&str>) {
    assert!(
        action_payload.is_some(),
        "detection fallback must include an operator action payload"
    );
    let Some(action) = action_payload else {
        return;
    };
    assert_cpu_detection_action_text(action);
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn assert_cpu_detection_action_text(action: &str) {
    let lower = action.to_ascii_lowercase();
    assert!(
        lower.contains("cpu") && lower.contains("supported"),
        "action must say CPU detection is the supported path: {action}"
    );
    assert!(
        lower.contains("decode") && lower.contains("hardware"),
        "action must keep hardware decode distinct from CPU detection: {action}"
    );
    assert!(
        lower.contains("not in this build")
            || lower.contains("isn't in this build")
            || lower.contains("not available")
            || lower.contains("cannot accelerate"),
        "action must say accelerated detection is unavailable in this artifact: {action}"
    );
    assert!(
        !lower.contains(MISLEADING_ACTION),
        "action must not point at a nonexistent accelerated detector build: {action}"
    );
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn assert_cpu_detection_action_in_rendered_block(surface: &str, label: &str) {
    let block = rendered_receipt_block(surface, "[detect.acceleration]");
    assert!(
        block.is_some(),
        "{label} must render the detection acceleration block: {surface}"
    );
    let Some(block) = block else {
        return;
    };
    for (field, expected) in [
        ("status", "fallback"),
        ("active_backend", CPU_DETECTION_BACKEND),
        ("hardware_accelerated", "false"),
        ("failure_code", "backend_not_compiled"),
    ] {
        let expected_line = format!("{field}: {expected}");
        assert!(
            block.lines().any(|line| line.trim() == expected_line),
            "{label} detection block must render `{expected_line}` so the public surface cannot claim accelerated detection while showing CPU fallback advice: {block}"
        );
    }
    assert!(
        block.contains("action_kind:") && block.contains("action_payload:"),
        "{label} detection block must render the action rows: {block}"
    );
    assert_cpu_detection_action_text(&block);
}

struct CountingProbe {
    invocations: Arc<AtomicUsize>,
    outcome: DetectionForwardProbeOutcome,
}

impl DetectionForwardProbe for CountingProbe {
    fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        self.outcome.clone()
    }
}

#[cfg(feature = "detect-burn-wgpu")]
struct SlowProbe {
    invocations: Arc<AtomicUsize>,
    delay: Duration,
}

#[cfg(feature = "detect-burn-wgpu")]
impl DetectionForwardProbe for SlowProbe {
    fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        DetectionForwardProbeOutcome::Failed {
            reason: "probe exceeded deadline".to_string(),
        }
    }
}

#[cfg(feature = "detect-burn-wgpu")]
struct PanickingProbe {
    invocations: Arc<AtomicUsize>,
}

#[cfg(feature = "detect-burn-wgpu")]
impl DetectionForwardProbe for PanickingProbe {
    fn run_forward_probe(&mut self) -> DetectionForwardProbeOutcome {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        panic!("injected forward probe panic")
    }
}

#[cfg(not(feature = "detect-burn-wgpu"))]
struct EmptyHost;

#[cfg(not(feature = "detect-burn-wgpu"))]
impl HostFacts for EmptyHost {
    fn effective_uid(&self) -> u32 {
        1000
    }

    fn effective_gids(&self) -> Vec<u32> {
        vec![1000]
    }

    fn visible_render_devices(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    fn device_facts(&self, _path: &Path) -> Option<DeviceFacts> {
        None
    }

    fn open_device(&self, _path: &Path) -> Result<(), DeviceOpenError> {
        Err(DeviceOpenError::NotFound)
    }

    fn env_var(&self, _name: &str) -> Option<String> {
        None
    }

    fn systemd_service_user(&self) -> Option<String> {
        None
    }

    fn user_groups(&self, _user: &str) -> Option<Vec<String>> {
        None
    }
}

#[cfg(not(feature = "detect-burn-wgpu"))]
fn doctor_request() -> DoctorRequest {
    DoctorRequest {
        hardware_decoding: true,
        accelerated_detection: true,
        service_user_flag: None,
        detector_model_path: None,
    }
}

#[cfg(not(feature = "detect-burn-wgpu"))]
#[test]
fn detection_accel_compiled_out_is_backend_not_compiled_from_observed_state() {
    // Unfakeable because the receipt must record the compiled backend set, not
    // just echo the requested acceleration flag.
    let selection = select_detection_acceleration(true, MODEL_ID, INPUT_SHAPE);
    let receipt = selection.receipt;

    assert_eq!(selection.backend, CPU_DETECTION_BACKEND);
    assert_eq!(selection.backend, receipt.active_backend);
    assert_eq!(receipt.probe_status, ProbeStatus::Fallback);
    assert_eq!(receipt.failure_code, FailureCode::BackendNotCompiled);
    assert_eq!(receipt.attempted_backend, ACCELERATED_DETECTION_BACKEND);
    assert_eq!(receipt.active_backend, CPU_DETECTION_BACKEND);
    assert!(!receipt.hardware_accelerated);
    assert_eq!(receipt.evidence_kind, Some(EvidenceKind::SelectedBackend));
    let compiled_backends = receipt
        .evidence_fields
        .get("compiled_backends")
        .map(String::as_str);
    assert!(
        compiled_backends.is_some(),
        "receipt must record the observed compiled detection backend set"
    );
    let compiled_backends = parsed_backend_set(compiled_backends.unwrap_or_default());
    assert_eq!(
        compiled_backends,
        BTreeSet::from([CPU_DETECTION_BACKEND]),
        "compiled-out receipt must record exactly the compiled CPU backend set"
    );
    assert!(
        !compiled_backends.contains(ACCELERATED_DETECTION_BACKEND),
        "compiled-out receipt must not claim the accelerated backend is compiled"
    );
    assert!(
        !receipt.evidence_fields.contains_key("forward_probe"),
        "compiled-out path must not carry backend-probe evidence"
    );
}

#[test]
fn detection_accel_disabled_skips_probe_and_reports_disabled() {
    // Unfakeable because the injected probe invocation count proves the
    // disabled path attempted no forward probe at all.
    let invocations = Arc::new(AtomicUsize::new(0));
    let probe = CountingProbe {
        invocations: Arc::clone(&invocations),
        outcome: DetectionForwardProbeOutcome::Passed {
            selected_device: "accelerator".to_string(),
            evidence_fields: BTreeMap::from([("forward_probe".to_string(), "passed".to_string())]),
        },
    };

    let selection = select_detection_acceleration_with_probe(false, MODEL_ID, INPUT_SHAPE, probe);
    let receipt = selection.receipt;

    assert_eq!(selection.backend, CPU_DETECTION_BACKEND);
    assert_eq!(selection.backend, receipt.active_backend);
    assert_eq!(receipt.probe_status, ProbeStatus::Disabled);
    assert_eq!(receipt.failure_code, FailureCode::None);
    assert_eq!(receipt.active_backend, CPU_DETECTION_BACKEND);
    assert_eq!(receipt.selected_device, None);
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "disabled accelerated_detection must not invoke the forward probe"
    );
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "disabled accelerated_detection must not schedule a delayed background forward probe"
    );
    assert_ne!(receipt.evidence_kind, Some(EvidenceKind::BackendProbe));
}

#[cfg(feature = "detect-burn-wgpu")]
#[test]
fn detection_accel_compiled_in_receipt_agrees_with_recorded_probe() {
    // Unfakeable because a true hardware claim is valid only when the receipt
    // also contains the recorded passing probe and non-CPU device identity.
    assert_production_selector_delegates_to_probe_seam();
    assert_runtime_detection_receipt_uses_selection_seam();

    let invocations = Arc::new(AtomicUsize::new(0));
    let sentinel_device = "sentinel Vulkan accelerator from injected probe";
    let sentinel_probe_nonce = "probe-result-must-carry-through";
    let passing_probe = CountingProbe {
        invocations: Arc::clone(&invocations),
        outcome: DetectionForwardProbeOutcome::Passed {
            selected_device: sentinel_device.to_string(),
            evidence_fields: BTreeMap::from([
                ("forward_probe".to_string(), "passed".to_string()),
                ("probe_nonce".to_string(), sentinel_probe_nonce.to_string()),
            ]),
        },
    };
    let measured_selection =
        select_detection_acceleration_with_probe(true, MODEL_ID, INPUT_SHAPE, passing_probe);
    let measured_receipt = measured_selection.receipt;
    assert_eq!(measured_selection.backend, ACCELERATED_DETECTION_BACKEND);
    assert_eq!(measured_selection.backend, measured_receipt.active_backend);
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "enabled accelerated_detection must invoke the forward probe exactly once"
    );
    assert_eq!(measured_receipt.probe_status, ProbeStatus::Active);
    assert_eq!(measured_receipt.failure_code, FailureCode::None);
    assert_eq!(
        measured_receipt.active_backend,
        ACCELERATED_DETECTION_BACKEND
    );
    assert!(measured_receipt.hardware_accelerated);
    assert!(
        selected_device_is_non_cpu(&measured_receipt),
        "passing probe must record a selected non-CPU device: {measured_receipt:?}"
    );
    assert_eq!(
        measured_receipt.selected_device.as_deref(),
        Some(sentinel_device),
        "hardware receipt must carry the selected device returned by the injected probe, not a canned device name"
    );
    assert_eq!(
        measured_receipt.evidence_kind,
        Some(EvidenceKind::BackendProbe)
    );
    assert_eq!(
        measured_receipt
            .evidence_fields
            .get("forward_probe")
            .map(String::as_str),
        Some("passed"),
        "passing probe must persist forward_probe evidence"
    );
    assert_eq!(
        measured_receipt
            .evidence_fields
            .get("probe_nonce")
            .map(String::as_str),
        Some(sentinel_probe_nonce),
        "hardware receipt must carry the injected probe's evidence fields, not a forged passing receipt"
    );

    let failed_probe_invocations = Arc::new(AtomicUsize::new(0));
    let failed_probe = CountingProbe {
        invocations: Arc::clone(&failed_probe_invocations),
        outcome: DetectionForwardProbeOutcome::Failed {
            reason: "injected forward failure".to_string(),
        },
    };
    let failed_selection =
        select_detection_acceleration_with_probe(true, MODEL_ID, INPUT_SHAPE, failed_probe);
    let failed_receipt = failed_selection.receipt;
    assert_eq!(
        failed_probe_invocations.load(Ordering::SeqCst),
        1,
        "enabled accelerated_detection must invoke a failing injected probe exactly once"
    );
    assert_eq!(failed_selection.backend, CPU_DETECTION_BACKEND);
    assert_eq!(failed_receipt.active_backend, CPU_DETECTION_BACKEND);
    assert!(!failed_receipt.hardware_accelerated);
    assert_eq!(failed_receipt.probe_status, ProbeStatus::Fallback);
    assert_eq!(failed_receipt.failure_code, FailureCode::ProbeFailed);

    let no_device_invocations = Arc::new(AtomicUsize::new(0));
    let no_device_probe = CountingProbe {
        invocations: Arc::clone(&no_device_invocations),
        outcome: DetectionForwardProbeOutcome::NoDeviceVisible,
    };
    let no_device_selection =
        select_detection_acceleration_with_probe(true, MODEL_ID, INPUT_SHAPE, no_device_probe);
    let no_device_receipt = no_device_selection.receipt;
    assert_eq!(
        no_device_invocations.load(Ordering::SeqCst),
        1,
        "enabled accelerated_detection must invoke a no-device injected probe exactly once"
    );
    assert_eq!(no_device_selection.backend, CPU_DETECTION_BACKEND);
    assert_eq!(no_device_receipt.active_backend, CPU_DETECTION_BACKEND);
    assert!(!no_device_receipt.hardware_accelerated);
    assert_eq!(no_device_receipt.probe_status, ProbeStatus::Fallback);
    assert_eq!(no_device_receipt.failure_code, FailureCode::NoDeviceVisible);

    let selection = select_detection_acceleration(true, MODEL_ID, INPUT_SHAPE);
    let receipt = selection.receipt;
    assert_eq!(selection.backend, CPU_DETECTION_BACKEND);
    assert_eq!(selection.backend, receipt.active_backend);
    assert!(
        !receipt.hardware_accelerated,
        "headless CI must not accept a no-argument production Active claim; real hardware proof must come from the live-device gate: {receipt:?}"
    );
    assert_eq!(receipt.probe_status, ProbeStatus::Fallback);
    assert_eq!(receipt.active_backend, CPU_DETECTION_BACKEND);
    assert!(
        matches!(
            receipt.failure_code,
            FailureCode::ProbeFailed | FailureCode::NoDeviceVisible
        ),
        "compiled-in headless fallback must be a measured probe/device failure, not backend_not_compiled; got {:?}",
        receipt.failure_code
    );

    let impossible_selection =
        select_detection_acceleration(true, MODEL_ID, "not-a-valid-input-shape");
    let impossible_receipt = impossible_selection.receipt;
    assert_eq!(impossible_selection.backend, CPU_DETECTION_BACKEND);
    assert_eq!(
        impossible_selection.backend,
        impossible_receipt.active_backend
    );
    assert!(
        !impossible_receipt.hardware_accelerated,
        "production selector must not self-report hardware acceleration for an impossible probe input: {impossible_receipt:?}"
    );
    assert_eq!(impossible_receipt.probe_status, ProbeStatus::Fallback);
    assert_eq!(impossible_receipt.active_backend, CPU_DETECTION_BACKEND);
    assert!(
        matches!(impossible_receipt.failure_code, FailureCode::ProbeFailed),
        "impossible production probe input must be validated and classified as probe_failed, not hidden behind a no-device shortcut; got {:?}",
        impossible_receipt.failure_code
    );
}

#[test]
fn detection_accel_hardware_flag_requires_recorded_passing_probe() {
    // Unfakeable because the validator rejects canned hardware claims even
    // before any real GPU is present in the test environment.
    assert_valid_hardware_claim(
        &base_detection_receipt(),
        "active backend, selected device, backend-probe evidence, and passing forward probe agree",
    );

    let mut missing_probe_evidence = base_detection_receipt();
    missing_probe_evidence.evidence_kind = None;
    missing_probe_evidence.evidence_fields.clear();
    assert_invalid_hardware_claim(
        missing_probe_evidence,
        "recorded passing probe evidence is absent",
    );

    let mut missing_forward_probe = base_detection_receipt();
    missing_forward_probe.evidence_fields.clear();
    missing_forward_probe
        .evidence_fields
        .insert("backend_loaded".to_string(), "true".to_string());
    assert_invalid_hardware_claim(missing_forward_probe, "forward_probe evidence is absent");

    for forward_probe in [
        "failed",
        "timed_out",
        "ok",
        "not passed",
        "passed_without_running",
        "passed: false",
        " passed ",
        "Passed",
        "PASSED",
        "passed\n",
        "passed\t",
    ] {
        let mut non_passing_forward_probe = base_detection_receipt();
        non_passing_forward_probe
            .evidence_fields
            .insert("forward_probe".to_string(), forward_probe.to_string());
        assert_invalid_hardware_claim(
            non_passing_forward_probe,
            "forward_probe evidence does not record a passing probe",
        );
    }

    let mut wrong_evidence_kind = base_detection_receipt();
    wrong_evidence_kind.evidence_kind = Some(EvidenceKind::SelectedBackend);
    assert_invalid_hardware_claim(wrong_evidence_kind, "evidence_kind is not backend_probe");

    let mut missing_selected_device = base_detection_receipt();
    missing_selected_device.selected_device = None;
    assert_invalid_hardware_claim(missing_selected_device, "selected_device is absent");

    let mut fallback_status = base_detection_receipt();
    fallback_status.probe_status = ProbeStatus::Fallback;
    assert_invalid_hardware_claim(fallback_status, "probe_status is not active");

    let mut cpu_active_backend = base_detection_receipt();
    cpu_active_backend.active_backend = CPU_DETECTION_BACKEND.to_string();
    assert_invalid_hardware_claim(cpu_active_backend, "active_backend is CPU fallback");

    let mut classified_failure = base_detection_receipt();
    classified_failure.failure_code = FailureCode::ProbeFailed;
    assert_invalid_hardware_claim(classified_failure, "failure_code is not none");

    for device in [
        "",
        "   ",
        "CPU",
        "llvmpipe software adapter",
        "LAVAPIPE",
        "swrast renderer",
        "software adapter",
    ] {
        let mut software_adapter = base_detection_receipt();
        software_adapter.selected_device = Some(device.to_string());
        assert_invalid_hardware_claim(
            software_adapter,
            "selected_device is empty or a software adapter",
        );
    }
}

#[cfg(not(feature = "detect-burn-wgpu"))]
#[test]
fn detection_kill_path_action_names_cpu_detection_as_supported() {
    // Unfakeable because the fallback receipt has to carry useful operator
    // wording; absence or generic install guidance fails.
    let selection = select_detection_acceleration(true, MODEL_ID, INPUT_SHAPE);
    assert_cpu_detection_action(selection.receipt.action_payload.as_deref());
}

#[cfg(not(feature = "detect-burn-wgpu"))]
#[test]
fn doctor_detection_action_line_names_cpu_detection_as_supported() {
    // Unfakeable because it reads the rendered doctor detection block, a
    // separate operator surface from the selection seam.
    let report = acceleration_report(&doctor_request(), &EmptyHost);
    let rendered = render_report(&report);
    assert_cpu_detection_action_in_rendered_block(&rendered, "doctor report");
}

#[cfg(not(feature = "detect-burn-wgpu"))]
#[test]
fn runtime_detection_receipt_action_line_is_honest() {
    // Unfakeable because the runtime receipt producer must stop carrying its
    // own stale action line into the public stats and health surfaces.
    let Some(surfaces) = runtime_stats_text_from_real_runtime() else {
        return;
    };
    let stats = runtime_surface_section(&surfaces, "[runtime.stats]");
    let health = runtime_surface_section(&surfaces, "[runtime.health]");
    assert!(
        stats.contains("[detect.acceleration]"),
        "runtime stats must expose the rendered detection receipt block: {stats}"
    );
    assert!(
        stats.contains("detection-acceleration=") || stats.contains("work-receipt="),
        "runtime stats must carry the detection receipt through a public stats row: {stats}"
    );
    assert_cpu_detection_action_in_rendered_block(stats, "runtime stats");
    assert!(
        health.contains("[detect.acceleration]"),
        "runtime /health must expose the rendered detection receipt block because Home Assistant reads that surface: {surfaces}"
    );
    assert!(
        health.contains("detection-acceleration")
            || health.contains("work-receipt")
            || health.contains("backend_not_compiled"),
        "runtime /health must carry the detection receipt through the public health body: {health}"
    );
    assert_cpu_detection_action_in_rendered_block(health, "runtime health");
}

#[cfg(feature = "detect-burn-wgpu")]
#[test]
fn detection_probe_timeout_and_panic_classify_probe_failed_without_hang() {
    // Unfakeable because slow and panicking probes are injected deterministically
    // and must return as classified fallback, not hang or escape.
    //
    // This test pins its own 1 s deadline via VIGIL_DETECTION_PROBE_DEADLINE_SECS
    // instead of inheriting the shipped default: the shipped default is set high
    // enough to let a real GPU's cold shader compile reach ACTIVE, so the
    // bounded-classification contract must own a short deadline to stay a
    // deterministic test of the timeout path (an injected 3 s probe against a 1 s
    // deadline must return bounded and classify ProbeFailed). The env is restored
    // before any assertion can unwind.
    const DEADLINE_ENV: &str = "VIGIL_DETECTION_PROBE_DEADLINE_SECS";
    let previous_deadline = std::env::var_os(DEADLINE_ENV);
    unsafe {
        std::env::set_var(DEADLINE_ENV, "1");
    }

    let slow_invocations = Arc::new(AtomicUsize::new(0));
    let slow_probe = SlowProbe {
        invocations: Arc::clone(&slow_invocations),
        delay: Duration::from_secs(3),
    };
    let started = std::time::Instant::now();
    let slow_selection =
        select_detection_acceleration_with_probe(true, MODEL_ID, INPUT_SHAPE, slow_probe);
    let slow_elapsed = started.elapsed();

    let panic_invocations = Arc::new(AtomicUsize::new(0));
    let panic_probe = PanickingProbe {
        invocations: Arc::clone(&panic_invocations),
    };
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        select_detection_acceleration_with_probe(true, MODEL_ID, INPUT_SHAPE, panic_probe)
    }));

    // Restore the environment before any assertion can unwind out of the test.
    unsafe {
        match previous_deadline {
            Some(value) => std::env::set_var(DEADLINE_ENV, value),
            None => std::env::remove_var(DEADLINE_ENV),
        }
    }

    assert!(
        slow_elapsed < Duration::from_secs(2),
        "slow injected probe must be deadline-bounded"
    );
    assert_eq!(slow_selection.receipt.probe_status, ProbeStatus::Fallback);
    assert_eq!(
        slow_selection.receipt.failure_code,
        FailureCode::ProbeFailed
    );
    assert_eq!(
        slow_invocations.load(Ordering::SeqCst),
        1,
        "the slow probe path must actually invoke the injected probe"
    );

    assert!(
        caught.is_ok(),
        "panicking injected probe must be captured and classified"
    );
    let Ok(panic_selection) = caught else {
        return;
    };
    assert_eq!(panic_selection.receipt.probe_status, ProbeStatus::Fallback);
    assert_eq!(
        panic_selection.receipt.failure_code,
        FailureCode::ProbeFailed
    );
    assert_eq!(
        panic_invocations.load(Ordering::SeqCst),
        1,
        "the panic probe path must actually invoke the injected probe"
    );
}
