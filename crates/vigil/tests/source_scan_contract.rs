use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn live_read_and_owner_control_source_contracts_are_fast() {
    let vigil_sources = collect_rust_source_files(&workspace_root().join("crates/vigil/src"));
    let context_graph_sources = collect_rust_source_files(
        &workspace_root()
            .parent()
            .expect("workspace parent")
            .join("context-graph/crates/context-graph/src"),
    );

    let mut failures = Vec::new();
    assert_context_graph_owns_generic_owner_control_transport(
        &context_graph_sources,
        &mut failures,
    );
    assert_vigil_keeps_only_control_path_and_handler_layer(&vigil_sources, &mut failures);
    assert_live_reads_stay_store_backed(&vigil_sources, &context_graph_sources, &mut failures);

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

#[test]
fn ha_os_vm_shell_signals_stay_behind_child_pid_validator() {
    let script_path = workspace_root().join("tests/ha-os-vm/run-th-suite.sh");
    let script = fs::read_to_string(&script_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", script_path.display()));
    let mut failures = Vec::new();

    for required in [
        "valid_child_pid()",
        "signal_child_pid()",
        "(( pid > 1 ))",
        "ps -o ppid= -p \"$pid\"",
        "[[ \"$parent\" == \"$$\" ]]",
        "signal_child_pid \"$sub_pid\"",
    ] {
        if !script.contains(required) {
            failures.push(format!(
                "HA-OS VM harness omitted guarded shell-signal marker {required}"
            ));
        }
    }

    for (line_number, line) in script.lines().enumerate() {
        let trimmed = line.trim();
        if !trimmed.starts_with("kill ") {
            continue;
        }
        if trimmed == "kill \"$pid\"" {
            continue;
        }
        failures.push(format!(
            "{}:{} uses raw shell kill outside signal_child_pid: {}",
            script_path.display(),
            line_number + 1,
            trimmed
        ));
    }

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

#[test]
fn vigil_addon_does_not_auto_install_legacy_lovelace_gallery() {
    let root = workspace_root();
    let mut failures = Vec::new();

    let legacy_card_path = root.join("addons/vigil/www/vigil-event-gallery-card.js");
    if legacy_card_path.exists() {
        failures.push(format!(
            "Vigil add-on still ships the obsolete Lovelace gallery card at {}; the owner UI must come from Advanced Camera Card",
            legacy_card_path.display()
        ));
    }

    for relative_path in [
        "addons/vigil/Dockerfile",
        "addons/vigil/config.yaml",
        "crates/vigil/src/runtime.rs",
    ] {
        let path = root.join(relative_path);
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for forbidden in [
            "vigil-event-gallery-card",
            "install_lovelace_card",
            "register_lovelace_resource",
            "lovelace_register_ws",
            "lovelace/resources/create",
            "homeassistant_config:rw",
            "COPY www/",
            "/www/",
        ] {
            if text.contains(forbidden) {
                failures.push(format!(
                    "{} contains obsolete frontend auto-install marker {forbidden}",
                    path.display()
                ));
            }
        }
    }

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

#[test]
fn yolox_oracle_owns_decode_path_without_dead_code_link_shims() {
    let root = workspace_root();
    let oracle_path = root.join("crates/vigil/src/bin/yolox_burn_oracle.rs");
    let media_path = root.join("crates/vigil/src/media_pipeline.rs");
    let oracle = fs::read_to_string(&oracle_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", oracle_path.display()));
    let media = fs::read_to_string(&media_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", media_path.display()));
    let mut failures = Vec::new();

    for forbidden in [
        "#[path = \"../media_pipeline.rs\"]",
        "keep_shared_media_symbols_linked",
        "VIGIL_ORACLE_EXERCISE_UNUSED_MEDIA_PATHS",
        "vigil::decode_sampled_detector_rgb_frames",
        "use vigil",
    ] {
        if oracle.contains(forbidden) {
            failures.push(format!(
                "{} contains shared decode or dead-code warning shim {forbidden}",
                oracle_path.display()
            ));
        }
    }

    for forbidden in [
        "fn write_encoded_clip",
        "fn extension(self)",
        "fn mime_type(self)",
    ] {
        if media.contains(forbidden) {
            failures.push(format!(
                "{} retained unused raw-media helper {forbidden}",
                media_path.display()
            ));
        }
    }

    for required in [
        "mod media_pipeline",
        "media_pipeline::decode_video_file",
        "sampled_detector_rgb",
        "decode_h264_units",
        "Mp4Reader::read_header",
        "H264Decoder::with_api_config",
    ] {
        if !oracle.contains(required) {
            failures.push(format!(
                "{} does not contain independent oracle decode marker {required}",
                oracle_path.display()
            ));
        }
    }
    if !oracle.contains("decode_sampled_rgb_frames") {
        failures.push(format!(
            "{} does not call the oracle-owned sampled decode path",
            oracle_path.display()
        ));
    }

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

#[test]
fn generic_camera_registration_uses_live_rtsp_url_not_detection_rtsp_url() {
    let root = workspace_root();
    let config_path = root.join("crates/vigil/src/config.rs");
    let runtime_path = root.join("crates/vigil/src/runtime.rs");
    let addon_config_path = root.join("addons/vigil/config.yaml");
    let config = fs::read_to_string(&config_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", config_path.display()));
    let runtime = fs::read_to_string(&runtime_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", runtime_path.display()));
    let addon_config = fs::read_to_string(&addon_config_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", addon_config_path.display()));
    let mut failures = Vec::new();

    if !addon_config.contains("live_rtsp_url: str?") {
        failures.push(format!(
            "{} must expose optional cameras[].live_rtsp_url in the add-on schema",
            addon_config_path.display()
        ));
    }
    for required in [
        "live_rtsp_url: Option<String>",
        "live_rtsp_url: c.live_rtsp_url",
        "live_rtsp_url: partial.live_rtsp_url.clone()",
    ] {
        if !config.contains(required) {
            failures.push(format!(
                "{} does not preserve camera live RTSP marker {required}",
                config_path.display()
            ));
        }
    }
    for required in [
        "fn generic_camera_url(camera: &config::CameraEntry) -> Option<&str>",
        ".live_rtsp_url",
        ".or(camera.rtsp_url.as_deref())",
        "register_generic_camera(&cam_id, generic_camera_url, &config.data_dir)",
    ] {
        if !runtime.contains(required) {
            failures.push(format!(
                "{} does not route Generic Camera registration through the live RTSP URL marker {required}",
                runtime_path.display()
            ));
        }
    }

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

#[test]
fn generic_camera_registration_confirms_home_assistant_preview_step() {
    let root = workspace_root();
    let runtime_path = root.join("crates/vigil/src/runtime.rs");
    let supervisor_path = root.join("crates/vigil/src/supervisor.rs");
    let runtime = fs::read_to_string(&runtime_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", runtime_path.display()));
    let supervisor = fs::read_to_string(&supervisor_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", supervisor_path.display()));
    let mut failures = Vec::new();

    for required in ["build_generic_camera_flow_confirm_payload", "confirmed_ok"] {
        if !supervisor.contains(required) {
            failures.push(format!(
                "{} does not define Generic Camera confirmation marker {required}",
                supervisor_path.display()
            ));
        }
    }
    if !runtime.contains("build_generic_camera_flow_confirm_payload") {
        failures.push(format!(
            "{} does not call the Generic Camera confirmation payload builder",
            runtime_path.display()
        ));
    }
    if runtime.contains("supervisor_post_body(&step_url, &token, \"{}\")") {
        failures.push(format!(
            "{} still submits an empty body to the Generic Camera confirmation step",
            runtime_path.display()
        ));
    }
    if runtime.contains("confirmed_ok") {
        failures.push(format!(
            "{} should not inline the Generic Camera confirmation JSON; use the supervisor payload builder",
            runtime_path.display()
        ));
    }

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

#[test]
fn generic_camera_registration_logs_validation_errors_and_deletes_failed_flows() {
    let root = workspace_root();
    let runtime_path = root.join("crates/vigil/src/runtime.rs");
    let supervisor_path = root.join("crates/vigil/src/supervisor.rs");
    let runtime = fs::read_to_string(&runtime_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", runtime_path.display()));
    let supervisor = fs::read_to_string(&supervisor_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", supervisor_path.display()));
    let mut failures = Vec::new();

    for required in ["flow_step_errors", "delete_flow"] {
        if !supervisor.contains(required) {
            failures.push(format!(
                "{} does not define Generic Camera failed-flow helper {required}",
                supervisor_path.display()
            ));
        }
        if !runtime.contains(required) {
            failures.push(format!(
                "{} does not use Generic Camera failed-flow helper {required}",
                runtime_path.display()
            ));
        }
    }
    for required in [
        "generic_camera_flow_step_validation_error",
        "generic_camera_flow_deleted",
    ] {
        if !runtime.contains(required) {
            failures.push(format!(
                "{} does not log Generic Camera failure marker {required}",
                runtime_path.display()
            ));
        }
    }

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

#[test]
fn review_data_plane_bounds_request_fanout_and_survives_accept_panics() {
    let root = workspace_root();
    let path = root.join("crates/vigil/src/http_data_plane.rs");
    let source = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let mut failures = Vec::new();

    for required in [
        "MAX_REVIEW_DATA_PLANE_MEDIA_HANDLERS",
        "AtomicUsize",
        "fetch_add",
        "fetch_sub",
        "bind_review_server",
        "review_data_plane_rebind_failed",
        "review_data_plane_rebound=true",
        "catch_unwind",
        "review_data_plane_accept_panic",
        "review_data_plane_busy",
    ] {
        if !source.contains(required) {
            failures.push(format!(
                "{} is missing bounded review data-plane marker {required}",
                path.display()
            ));
        }
    }

    for forbidden in [
        "handlers.push(thread::spawn(move || {\n                            handle_request",
        "thread::spawn(move || {\n                            handle_media_request(request, data_dir, shutdown);",
    ] {
        if source.contains(forbidden) {
            failures.push(format!(
                "{} still contains unbounded review request fanout marker {forbidden:?}",
                path.display()
            ));
        }
    }

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

#[test]
fn rtsp_success_recovers_health_after_transient_ingest_failure() {
    let root = workspace_root();
    let runtime_path = root.join("crates/vigil/src/runtime.rs");
    let runtime = fs::read_to_string(&runtime_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", runtime_path.display()));
    let mut failures = Vec::new();

    for required in [
        "stats.ingest_signal = \"ok\".to_string()",
        "set_ready_unless_latched_fault(&health, \"RTSP ingest active\")",
        "HealthStatus::DiskFull | HealthStatus::KeepPaceFailed",
    ] {
        if !runtime.contains(required) {
            failures.push(format!(
                "{} does not recover health on successful RTSP ingest marker {required}",
                runtime_path.display()
            ));
        }
    }

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

#[test]
fn addon_config_exposes_recognition_options_and_schema() {
    let root = workspace_root();
    let addon_config_path = root.join("addons/vigil/config.yaml");
    let addon_config = fs::read_to_string(&addon_config_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", addon_config_path.display()));
    let mut failures = Vec::new();

    let options = top_level_yaml_section(&addon_config, "options");
    let schema = top_level_yaml_section(&addon_config, "schema");

    if yaml_section_contains_key(&options, "recognition_weights_dir") {
        failures.push(format!(
            "{} must not set options.recognition_weights_dir to null/empty by default; Home Assistant Supervisor treats optional strings as omitted, not nullable, and rejects the add-on before the owner can configure it",
            addon_config_path.display()
        ));
    }

    for key in [
        "detector_stationary_interval_secs",
        "recognition_space_id",
        "recognition_threshold",
        "recognition_covered_classes",
    ] {
        if !yaml_section_contains_key(&options, key) {
            failures.push(format!(
                "{} must expose top-level options.{key} so a Home Assistant add-on user can see the active recognition setting",
                addon_config_path.display()
            ));
        }
    }
    if !yaml_section_contains_entry(&options, "recognition_threshold", "0.9") {
        failures.push(format!(
            "{} must keep options.recognition_threshold at the owner-approved visible default 0.9",
            addon_config_path.display()
        ));
    }
    for future_key in ["zones", "masks"] {
        if yaml_section_contains_key(&options, future_key)
            || yaml_section_contains_key(&schema, future_key)
        {
            failures.push(format!(
                "{} must not expose future {future_key} configuration before runtime support exists",
                addon_config_path.display()
            ));
        }
    }
    for class_name in ["person", "dog"] {
        let expected = format!("- {class_name}");
        if !options.iter().any(|line| line.trim() == expected) {
            failures.push(format!(
                "{} must default options.recognition_covered_classes to include {class_name} for the person-and-dog recognition owner smoke",
                addon_config_path.display()
            ));
        }
    }
    for class_name in ["car", "truck", "bus", "motorcycle", "bicycle"] {
        let forbidden = format!("- {class_name}");
        if options.iter().any(|line| line.trim() == forbidden) {
            failures.push(format!(
                "{} must not default options.recognition_covered_classes to include traffic class {class_name}; traffic classes auto-matched the only enrolled person before the class guard existed",
                addon_config_path.display()
            ));
        }
    }

    for (key, expected_schema) in [
        ("detector_stationary_interval_secs", "int"),
        ("recognition_weights_dir", "str?"),
        ("recognition_space_id", "str?"),
        ("recognition_threshold", "float?"),
    ] {
        if !yaml_section_contains_entry(&schema, key, expected_schema) {
            failures.push(format!(
                "{} must declare top-level schema.{key}: {expected_schema} so Home Assistant validates the recognition option",
                addon_config_path.display()
            ));
        }
    }
    if !yaml_section_contains_key(&schema, "recognition_covered_classes")
        || !schema.iter().any(|line| {
            line.trim()
                == "- \"list(person|dog|cat|bird|horse|sheep|cow|car|truck|bus|motorcycle|bicycle)\""
        })
    {
        failures.push(format!(
            "{} must declare schema.recognition_covered_classes as a Home Assistant list enum of supported COCO classes so the detector/recognizer allowlist is configurable outside Rust",
            addon_config_path.display()
        ));
    }

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

fn assert_context_graph_owns_generic_owner_control_transport(
    sources: &[SourceFile],
    failures: &mut Vec<String>,
) {
    let transport = sources
        .iter()
        .find(|source| source.path.ends_with("control_transport.rs"));
    let Some(transport) = transport else {
        failures.push("context-graph is missing src/control_transport.rs".to_string());
        return;
    };

    for required in [
        "pub type ControlHandler",
        "pub fn start_control_listener",
        "pub fn request_control",
        "UnixListener",
        "UnixStream",
        ".accept()",
        "read_to_string",
        "write_all",
        "flush",
        "handler",
    ] {
        if !transport.text.contains(required) {
            failures.push(format!(
                "context-graph owner-control transport omitted {required}"
            ));
        }
    }

    for forbidden in [
        "\"why\"",
        "\"events\"",
        "\"stats\"",
        "Observation",
        "Decision",
        "Intention",
        "Entity",
        "context_graph::Context",
        "RuntimeStats",
        "Detector",
        "VIGIL_CONTROL_SOCKET",
    ] {
        if transport.text.contains(forbidden) {
            failures.push(format!(
                "context-graph owner-control transport contains vigil domain marker {forbidden}"
            ));
        }
    }
}

fn assert_vigil_keeps_only_control_path_and_handler_layer(
    sources: &[SourceFile],
    failures: &mut Vec<String>,
) {
    let vigil_text = join_sources(sources);
    for forbidden in [
        "UnixListener",
        "UnixStream",
        "tokio::net::UnixListener",
        "tokio::net::UnixStream",
        "std::os::unix::net",
    ] {
        if vigil_text.contains(forbidden) {
            failures.push(format!(
                "vigil source still owns AF_UNIX primitive {forbidden}; transport belongs in context-graph"
            ));
        }
    }

    for required in [
        "VIGIL_CONTROL_SOCKET",
        "control.sock",
        "request_control",
        "start_control_listener",
        "handle_owner_request",
        "\"why\"",
        "\"events\"",
        "\"stats\"",
        "served-by=af_unix",
    ] {
        if !vigil_text.contains(required) {
            failures.push(format!(
                "vigil source omitted required control handler/path marker {required}"
            ));
        }
    }
}

fn assert_live_reads_stay_store_backed(
    vigil_sources: &[SourceFile],
    context_graph_sources: &[SourceFile],
    failures: &mut Vec<String>,
) {
    let vigil_text = join_sources(vigil_sources);
    let cg_text = join_sources(context_graph_sources);

    for expected in [
        "trait StoreReadObserver",
        "struct StoreReadEvent",
        "read_observer",
        "observe_read",
        "writer: \"context-graph\"",
        "StoreReadEvent StoreReadObserver writer={}",
    ] {
        if !cg_text.contains(expected) {
            failures.push(format!(
                "context-graph source omitted Store read observer marker {expected}"
            ));
        }
    }

    for expected in [
        "handle_owner_request",
        "handle_why_read",
        "handle_events_read",
        "StoreBackedWhyResponse",
        "StoreBackedEventsResponse",
        "list_observations",
        "audit_query",
        "get_observation",
        "get_decision",
        "get_intention",
        "get_entity",
        "get_context",
        "from_store_reads",
    ] {
        if !vigil_text.contains(expected) {
            failures.push(format!(
                "vigil live-read source omitted store-backed marker {expected}"
            ));
        }
    }

    for forbidden in [
        "provenance_mirror",
        "shadow_provenance",
        "event_cache",
        "ObservationCache",
        "DecisionCache",
    ] {
        if vigil_text.contains(forbidden) {
            failures.push(format!(
                "vigil live-read source retained forbidden mirror marker {forbidden}"
            ));
        }
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn collect_rust_source_files(root: &Path) -> Vec<SourceFile> {
    let mut sources = Vec::new();
    collect_rust_source_files_into(root, &mut sources);
    sources
}

fn collect_rust_source_files_into(dir: &Path, sources: &mut Vec<SourceFile>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_source_files_into(&path, sources);
        } else if path.extension() == Some(OsStr::new("rs"))
            && let Ok(text) = fs::read_to_string(&path)
        {
            sources.push(SourceFile { path, text });
        }
    }
}

fn join_sources(sources: &[SourceFile]) -> String {
    sources
        .iter()
        .map(|source| source.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn top_level_yaml_section<'a>(yaml: &'a str, name: &str) -> Vec<&'a str> {
    let header = format!("{name}:");
    let mut in_section = false;
    let mut lines = Vec::new();

    for line in yaml.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let is_top_level = !line.starts_with(' ') && !line.starts_with('\t');
        if is_top_level {
            if line.trim_end() == header {
                in_section = true;
                continue;
            }
            if in_section {
                break;
            }
        }
        if in_section {
            lines.push(line);
        }
    }

    lines
}

fn yaml_section_contains_key(lines: &[&str], key: &str) -> bool {
    let prefix = format!("  {key}:");
    lines
        .iter()
        .any(|line| line.starts_with(&prefix) || line.trim() == format!("{key}:"))
}

fn yaml_section_contains_entry(lines: &[&str], key: &str, expected_value: &str) -> bool {
    let expected = format!("{key}: {expected_value}");
    lines.iter().any(|line| line.trim() == expected)
}

struct SourceFile {
    path: PathBuf,
    text: String,
}
