use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn live_read_and_owner_control_source_contracts_are_fast() {
    let vigil_sources = collect_rust_source_files(&workspace_root().join("crates/vigil/src"));
    let context_graph_sources = collect_rust_source_files(&context_graph_src());

    let mut failures = Vec::new();
    assert_context_graph_owns_generic_owner_control_transport(
        &context_graph_sources,
        &mut failures,
    );
    assert_vigil_keeps_only_the_owner_route_and_handler_layer(&vigil_sources, &mut failures);
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
    let ha_camera_registration_path = root.join("crates/vigil/src/ha_camera_registration.rs");
    let addon_config_path = root.join("addons/vigil/config.yaml");
    let config = fs::read_to_string(&config_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", config_path.display()));
    let runtime = fs::read_to_string(&runtime_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", runtime_path.display()));
    let ha_camera_registration = fs::read_to_string(&ha_camera_registration_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", ha_camera_registration_path.display()));
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
    ] {
        if !ha_camera_registration.contains(required) {
            failures.push(format!(
                "{} does not route Generic Camera registration through the live RTSP URL marker {required}",
                ha_camera_registration_path.display()
            ));
        }
    }
    for required in ["register_generic_camera(&cam_id, generic_camera_url, &config.data_dir)"] {
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
    let ha_camera_registration_path = root.join("crates/vigil/src/ha_camera_registration.rs");
    let supervisor_path = root.join("crates/vigil/src/supervisor.rs");
    let ha_camera_registration = fs::read_to_string(&ha_camera_registration_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", ha_camera_registration_path.display()));
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
    if !ha_camera_registration.contains("build_generic_camera_flow_confirm_payload") {
        failures.push(format!(
            "{} does not call the Generic Camera confirmation payload builder",
            ha_camera_registration_path.display()
        ));
    }
    if ha_camera_registration.contains("supervisor_post_body(&step_url, &token, \"{}\")") {
        failures.push(format!(
            "{} still submits an empty body to the Generic Camera confirmation step",
            ha_camera_registration_path.display()
        ));
    }
    if ha_camera_registration.contains("confirmed_ok") {
        failures.push(format!(
            "{} should not inline the Generic Camera confirmation JSON; use the supervisor payload builder",
            ha_camera_registration_path.display()
        ));
    }

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

#[test]
fn generic_camera_registration_logs_validation_errors_and_deletes_failed_flows() {
    let root = workspace_root();
    let ha_camera_registration_path = root.join("crates/vigil/src/ha_camera_registration.rs");
    let supervisor_path = root.join("crates/vigil/src/supervisor.rs");
    let ha_camera_registration = fs::read_to_string(&ha_camera_registration_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", ha_camera_registration_path.display()));
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
        if !ha_camera_registration.contains(required) {
            failures.push(format!(
                "{} does not use Generic Camera failed-flow helper {required}",
                ha_camera_registration_path.display()
            ));
        }
    }
    for required in [
        "generic_camera_flow_step_validation_error",
        "generic_camera_flow_deleted",
    ] {
        if !ha_camera_registration.contains(required) {
            failures.push(format!(
                "{} does not log Generic Camera failure marker {required}",
                ha_camera_registration_path.display()
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
fn recognition_settings_are_schema_optional_with_defaults_owned_by_vigils_settings_registry() {
    // Superseded contract note: this test used to require
    // detector_stationary_interval_secs / recognition_space_id /
    // recognition_threshold / recognition_covered_classes visible under
    // `options:` (including a hardcoded person+dog-only, no-traffic-classes
    // default for recognition_covered_classes) "so a Home Assistant add-on
    // user can see the active recognition setting". Under the
    // settings-authority direction the operator sees the ACTIVE setting
    // through `vigil settings`, not through a manifest default — a declared
    // options default is a pin on the whole-form save and blocks
    // reset-by-absence. That every behavior key (recognition_weights_dir
    // included) carries no options default and IS declared schema-optional
    // is already asserted generically, over every schema-derived behavior
    // key, by `addon_config_surface.rs`'s
    // `no_behavior_key_carries_a_declared_default_in_the_options_block` and
    // `every_behavior_key_is_declared_schema_optional` — not repeated here.
    //
    // What this test keeps checking: each recognition key's specific schema
    // TYPE (not just optionality), and that the value an operator gets when
    // they set nothing is Vigil's own, read from the real seam rather than
    // hand-copied — `crate::settings_backends::automatic_default` for the
    // two settings the registry governs, and
    // `crate::recognition::RecognitionConfig::default()` for the two it
    // does not (recognition_space_id/recognition_threshold resolve straight
    // from that struct in `config.rs`, never through the registry).
    let root = workspace_root();
    let addon_config_path = root.join("addons/vigil/config.yaml");
    let addon_config = fs::read_to_string(&addon_config_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", addon_config_path.display()));
    let mut failures = Vec::new();

    let options = top_level_yaml_section(&addon_config, "options");
    let schema = top_level_yaml_section(&addon_config, "schema");

    for (key, expected_schema) in [
        ("detector_stationary_interval_secs", "int?"),
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

    let recognition_defaults = vigil::recognition::RecognitionConfig::default();
    if recognition_defaults.embedding_space_id != "vigil_site_vision_v1" {
        failures.push(format!(
            "recognition_space_id's owning default, RecognitionConfig::default().embedding_space_id, must stay \"vigil_site_vision_v1\", got {:?}",
            recognition_defaults.embedding_space_id
        ));
    }
    if recognition_defaults.match_threshold != 0.9 {
        failures.push(format!(
            "recognition_threshold's owning default, RecognitionConfig::default().match_threshold, must stay the owner-approved 0.9, got {}",
            recognition_defaults.match_threshold
        ));
    }

    let stationary_interval_default = vigil::settings_backends::automatic_default(
        vigil::settings_model::DETECTOR_STATIONARY_INTERVAL_SETTING,
    )
    .map(|(value, _reason)| value);
    if stationary_interval_default != Some(vigil::settings_model::SettingValue::Int(30)) {
        failures.push(format!(
            "detector_stationary_interval_secs's owning default, settings_backends::automatic_default, must stay 30 seconds, got {stationary_interval_default:?}"
        ));
    }

    // The recognition_covered_classes automatic floor is Vigil's OWN
    // independent default (cold-review-r4 finding 22): resolving it from
    // `RecognitionConfig::default().covered_classes` (the full COCO set)
    // coupled it to `detector_classes` — the moment an operator widened
    // detection to include `car`, recognition silently covered it too,
    // reviving the "traffic classes auto-matched the only enrolled person"
    // regression this setting's own removed pre-arc default existed to
    // prevent. `default_recognition_covered_classes()` restores that
    // person+dog-only floor as its own single-sourced function, independent
    // of both `detector_classes` and of `RecognitionConfig::default()`.
    let covered_classes_default = vigil::settings_backends::automatic_default(
        vigil::settings_model::RECOGNITION_COVERED_CLASSES_SETTING,
    )
    .map(|(value, _reason)| value);
    let expected_covered_classes = vigil::settings_model::SettingValue::list(
        vigil::settings_backends::default_recognition_covered_classes()
            .iter()
            .copied(),
    );
    if covered_classes_default != Some(expected_covered_classes) {
        failures.push(format!(
            "recognition_covered_classes's automatic floor must stay settings_backends::default_recognition_covered_classes() (person+dog), independent of detector_classes and of RecognitionConfig::default(), got {covered_classes_default:?}"
        ));
    }
    if vigil::settings_backends::default_recognition_covered_classes() != ["person", "dog"] {
        failures.push(format!(
            "settings_backends::default_recognition_covered_classes() must stay [\"person\", \"dog\"], got {:?}",
            vigil::settings_backends::default_recognition_covered_classes()
        ));
    }
    if vigil::settings_backends::default_detection_classes() != ["person"] {
        failures.push(format!(
            "the detector's own default class list, settings_backends::default_detection_classes(), must stay person-only — this is what now keeps a passing vehicle from ever reaching recognition by default, got {:?}",
            vigil::settings_backends::default_detection_classes()
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
        .find(|source| source.path.ends_with("owner_control.rs"));
    let Some(transport) = transport else {
        failures.push("context-graph is missing src/owner_control.rs".to_string());
        return;
    };

    // The generic owner plane: a handler registered at the writable open, a
    // client that names the STORE PATH and nothing else, and a typed answer
    // for "nobody holds this store". The bytes on the wire are contextdb's
    // read session, so no socket primitive belongs here either.
    for required in [
        "pub type ControlHandler",
        "pub fn request_owner",
        "pub fn owner_read_config",
        "CONTROL_NAMESPACE",
        "OwnerControlError",
        "store_path",
        "handler",
    ] {
        if !transport.text.contains(required) {
            failures.push(format!(
                "context-graph owner-control plane omitted {required}"
            ));
        }
    }

    for retired in ["UnixListener", "UnixStream", "socket_path"] {
        if transport.text.contains(retired) {
            failures.push(format!(
                "context-graph owner-control plane still carries socket primitive {retired}; the \
                 store path is the whole address"
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

fn assert_vigil_keeps_only_the_owner_route_and_handler_layer(
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

    // `VIGIL_CONTROL_SOCKET` stays required for a different reason than it used
    // to be: the variable no longer places anything, and the only place it may
    // still appear is the refusal that tells an operator it has no effect.
    // `served-by=af_unix` stays required verbatim: route reporting is part of
    // what the transport change must leave untouched, so the marker keeps its
    // ratified bytes and a caller that already greps for them keeps working.
    for required in [
        "VIGIL_CONTROL_SOCKET",
        "owner_control",
        "request_owner",
        "owner_read_config",
        "handle_owner_request",
        "\"why\"",
        "\"events\"",
        "\"stats\"",
        "served-by=af_unix",
    ] {
        if !vigil_text.contains(required) {
            failures.push(format!(
                "vigil source omitted required owner-route/handler marker {required}"
            ));
        }
    }

    for retired in [
        "control.sock",
        "control_socket_path",
        "request_control",
        "start_control_listener",
    ] {
        if vigil_text.contains(retired) {
            failures.push(format!(
                "vigil source still names the retired control socket through {retired}"
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

/// The context-graph checkout this workspace actually COMPILES against, read
/// out of the path dependency the root manifest declares. A fixed
/// `../context-graph` guess reads whatever happens to sit beside the repo,
/// which during any co-development of the two repos is a different tree from
/// the one the build used — a scan that green-lights a checkout nothing links
/// against proves nothing.
fn context_graph_src() -> PathBuf {
    let manifest_path = workspace_root().join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", manifest_path.display()));
    let declared = manifest
        .lines()
        .find_map(|line| {
            let rest = line.trim().strip_prefix("context-graph")?;
            let rest = rest.trim_start().strip_prefix('=')?;
            let start = rest.find("path")?;
            let rest = &rest[start..];
            let opening = rest.find('"')? + 1;
            let closing = rest[opening..].find('"')? + opening;
            Some(rest[opening..closing].to_string())
        })
        .unwrap_or_else(|| {
            panic!(
                "{} must declare context-graph as a path dependency",
                manifest_path.display()
            )
        });
    workspace_root().join(declared).join("src")
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

// ---------------------------------------------------------------------
// Settings-arc source contracts: the detector's own documentation must
// stop claiming a behavior it no longer has, and no shipped path may
// author a settings record at the pushed rank.
// ---------------------------------------------------------------------

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::{
    PRODUCTION_CRATES, collect_cfg_test_ranges, crates_root, in_any_range, lex, rust_sources,
};

/// The whole file as one lowercase line: every run of whitespace and every
/// comment marker collapsed to a single space, so a claim written across
/// three wrapped `///` lines reads as the one sentence it is. A robust
/// phrasing check rather than a frozen byte string — the claim must be
/// gone, however it happened to be line-wrapped.
fn flattened_prose(text: &str) -> String {
    let mut flattened = String::with_capacity(text.len());
    let mut previous_was_space = false;
    for character in text.chars() {
        let normalized = if character.is_whitespace() {
            ' '
        } else {
            character
        };
        if normalized == ' ' {
            if !previous_was_space {
                flattened.push(' ');
            }
            previous_was_space = true;
        } else {
            flattened.push(normalized.to_ascii_lowercase());
            previous_was_space = false;
        }
    }
    flattened.replace("/// ", "").replace("// ", "")
}

/// The phrases that together make the stale claim: the detector emits person
/// only until recognition widens it. Recognition no longer decides the
/// detector's class breadth, so documentation that still says it does sends
/// a reader to the wrong place for the wrong reason.
const STALE_CLASS_BREADTH_CLAIMS: &[&str] = &["recognition widens", "defaults to person only"];

#[test]
fn detector_doc_comment_no_longer_claims_recognition_widens_classes() {
    // Unfakeable: the check runs against the real detector source, flattened
    // so a re-wrap cannot hide the sentence, and it carries its own canary —
    // the same detector against a synthetic string that DOES make the claim
    // must flag it. A normalizer that quietly stopped matching anything
    // therefore fails here rather than passing as a clean result.
    let detector_path = workspace_root().join("crates/vigil/src/yolox_detector.rs");
    let detector = fs::read_to_string(&detector_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", detector_path.display()));
    assert!(
        detector.len() > 1_000,
        "{} is implausibly small; a documentation guard that reads the wrong file must fail \
         loudly rather than pass on an empty read",
        detector_path.display()
    );

    let canary = "/// COCO class indices this detector emits. Defaults to person only (the\n\
                  /// baseline NVR behavior first-light depends on); recognition widens it to\n\
                  /// the covered classes so a dog sighting reaches the match path.";
    let flattened_canary = flattened_prose(canary);
    for claim in STALE_CLASS_BREADTH_CLAIMS {
        assert!(
            flattened_canary.contains(claim),
            "canary: the flattening must still recognize the stale claim {claim:?} when it is \
             present, or this guard proves nothing about the real file"
        );
    }

    let flattened = flattened_prose(&detector);
    let mut failures = Vec::new();
    for claim in STALE_CLASS_BREADTH_CLAIMS {
        if flattened.contains(claim) {
            failures.push(format!(
                "{} still documents the detector's class breadth as person-only-until-recognition \
                 ({claim:?}); recognition no longer widens the detector's classes, so the comment \
                 tells a reader the opposite of what the code does",
                detector_path.display()
            ));
        }
    }

    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}

/// The two files that legitimately name the pushed rank: the record model
/// declares the author and the single constructor that builds a pushed
/// record, and the settings store holds the hub-role write door. Every
/// other production file — the command line, the Home Assistant surface,
/// every operator-facing module — must never reach either.
///
/// `vigil/settings_domains.rs` is deliberately NOT listed here
/// (cold-review-arc2-r5 finding 1): it used to carry a whole-file exemption
/// because the scan's `#[cfg(test)]`-only masking could not see that
/// `apply_take_over` — the one function in that file that opens the
/// hub-role handle and authors at the pushed rank — is gated behind
/// `#[cfg(feature = "test-support")]`, the same door
/// `artifact_never_enables_test_support.rs` proves no shipped artifact
/// opens. `collect_cfg_test_ranges` (`source_scan_lexer.rs`) now masks that
/// gate too, so the scan sees `apply_take_over` is compiled out of every
/// shipped build on its own — the file no longer needs a blanket exemption,
/// and everything else in it (`gate_write`, `governed_value_refusal`,
/// `domain_roster`, ...) is scanned exactly like every other production
/// file, which is the whole point: the guard should read STRICTLY MORE of
/// the tree after a correction wave, never less.
const PUSHED_RANK_DEFINITION_FILES: &[&str] =
    &["vigil/settings_model.rs", "vigil/settings_store.rs"];

/// The spellings that author, or reach, a record at the pushed rank.
const PUSHED_RANK_TOKENS: &[&str] = &[
    "Author::Pushed",
    "Surface::ManagementServer",
    "write_pushed_record",
    "SettingRecord::pushed",
    "open_hub_role",
    "apply_hub_authored",
    // `apply_take_over` itself REACHES the pushed rank (it is the harness's
    // stand-in for a management server, and its own body calls
    // `open_hub_role`/`apply_hub_authored`/`SettingRecord::pushed`) — this
    // doc comment's own words are "author, or reach, a record at the pushed
    // rank," and a bare call to `settings_domains::apply_take_over(...)`
    // from an operator-facing module is exactly a reach-the-pushed-rank
    // site the prior wave's canary could not notice because it planted the
    // other three tokens but never this one (cold-review-arc2-r5 finding
    // 1). Its own definition in `settings_domains.rs` is masked out by the
    // `#[cfg(feature = "test-support")]` range around it, so listing it
    // here does not make that file's own legitimate definition a false
    // positive.
    "apply_take_over",
];

/// `masked` with every whitespace character dropped, alongside the original
/// index each surviving character came from — so a match found in the
/// compacted text (which tolerates `Author :: Pushed` and any line wrapping)
/// can still be tested against the `#[cfg(test)]` ranges computed over the
/// original positions.
fn compacted_with_positions(masked: &[char]) -> (Vec<char>, Vec<usize>) {
    let mut compacted = Vec::with_capacity(masked.len());
    let mut positions = Vec::with_capacity(masked.len());
    for (index, character) in masked.iter().enumerate() {
        if !character.is_whitespace() {
            compacted.push(*character);
            positions.push(index);
        }
    }
    (compacted, positions)
}

/// Every live-code occurrence of a pushed-rank spelling in one source file,
/// reported as `file: token`. Comments and string literals are masked out by
/// the shared lexer, and `#[cfg(test)]` bodies are excluded, so a unit test
/// exercising the hub-role door is not mistaken for a shipped path.
///
/// A token immediately followed by `=>` is the left-hand PATTERN of a match
/// arm — code scrutinizing an author a record already carries (`Author::Pushed
/// => ...`), not code authoring one. That shape is a read, so it is excluded
/// here; every other shape (a field initializer, an assignment, or a match
/// arm's own RIGHT-hand value) still matches and is still reported. This is
/// narrower than the bare token, not weaker: a construction site cannot be
/// spelled `Author::Pushed =>` and still count as a construction, so nothing
/// the scan exists to forbid can hide behind this exclusion.
fn pushed_rank_sites(label: &str, source: &str) -> Vec<String> {
    let lexed = lex(source);
    let cfg_test_ranges = collect_cfg_test_ranges(source, &lexed.masked);
    let (compacted, positions) = compacted_with_positions(&lexed.masked);
    let mut sites = Vec::new();
    for token in PUSHED_RANK_TOKENS {
        let needle: Vec<char> = token.chars().collect();
        if needle.len() > compacted.len() {
            continue;
        }
        for start in 0..=compacted.len() - needle.len() {
            if compacted[start..start + needle.len()] != needle[..] {
                continue;
            }
            if in_any_range(&cfg_test_ranges, positions[start]) {
                continue;
            }
            let end = start + needle.len();
            let is_match_arm_pattern =
                compacted.get(end) == Some(&'=') && compacted.get(end + 1) == Some(&'>');
            if is_match_arm_pattern {
                continue;
            }
            sites.push(format!("{label}: {token}"));
        }
    }
    sites
}

#[test]
fn no_command_line_or_operator_path_writes_at_the_pushed_rank() {
    // Unfakeable in both directions. The negative leg scans every production
    // crate's real sources for the pushed-rank spellings; the positive legs
    // stop it being vacuous — a planted synthetic source MUST be flagged, and
    // the one legitimate construction site MUST be found where it belongs. A
    // scan that silently matched nothing therefore fails rather than passing
    // as a clean result.
    let planted = r#"
        fn __planted_operator_surface_write(path: &Path) {
            let record = SettingRecord {
                author: Author :: Pushed,
                surface: Surface::ManagementServer,
            };
            let pushed = SettingRecord::pushed(name, scope, value, reason);
            let store = SettingsStore::open_hub_role(path).expect("planted");
            store.write_pushed_record(record).expect("planted");
            store.apply_hub_authored(vec![pushed]).expect("planted");
            settings_domains::apply_take_over(path, &instruction).expect("planted");
        }
    "#;
    let planted_sites = pushed_rank_sites("planted.rs", planted);
    assert!(
        planted_sites.len() >= PUSHED_RANK_TOKENS.len(),
        "canary: a planted pushed-rank write must be flagged by this scan — including the spaced \
         `Author :: Pushed` spelling — or the scan proves nothing about the real tree; flagged: \
         {planted_sites:?}"
    );

    let commented_out = r#"
        // let record = SettingRecord { author: Author::Pushed };
        /// A doc comment mentioning Surface::ManagementServer.
        fn __clean() { let message = "write_pushed_record"; let _ = message; }
    "#;
    assert!(
        pushed_rank_sites("commented-out.rs", commented_out).is_empty(),
        "a mention inside a comment or a string literal is not a shipped write path"
    );

    let crate_roots = crates_root();
    let mut failures = Vec::new();
    let mut definition_site_tokens = Vec::new();

    for crate_name in PRODUCTION_CRATES {
        let src_root = crate_roots.join(crate_name).join("src");
        assert!(
            src_root.is_dir(),
            "expected production source tree {}",
            src_root.display()
        );
        let sources = rust_sources(&src_root);
        assert!(
            !sources.is_empty(),
            "expected Rust sources under {}",
            src_root.display()
        );
        for path in sources {
            let relative = path
                .strip_prefix(&src_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let label = format!("{crate_name}/{relative}");
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let sites = pushed_rank_sites(&label, &text);
            if PUSHED_RANK_DEFINITION_FILES.contains(&label.as_str()) {
                definition_site_tokens.extend(sites);
            } else {
                failures.extend(sites);
            }
        }
    }

    assert!(
        failures.is_empty(),
        "no operator-facing or command-line path may author a settings record at the pushed rank; \
         the only writer is the harness through the hub-role handle, and a local operation that \
         could write at the pushed rank would be exactly the second control path the settings \
         model forbids. Found: {failures:?}"
    );

    assert!(
        definition_site_tokens
            .iter()
            .any(|site| site.starts_with("vigil/settings_model.rs")
                && site.ends_with("Author::Pushed")),
        "the pushed author must be constructed somewhere — in the record model's own pushed-record \
         constructor — or this guard is vacuous: it would pass just as well on a tree where the \
         pushed rank does not exist at all. Found definition-site tokens: \
         {definition_site_tokens:?}"
    );
}
