// The dev-box-only `detect-burn-wgpu` feature pulls in Burn's cubecl/wgpu
// backend, whose deeply generic tensor/kernel types need more headroom than
// the default recursive-type-checking limit.
#![cfg_attr(feature = "detect-burn-wgpu", recursion_limit = "256")]

pub mod acceleration;
pub mod camera_hub;
pub mod camera_track;
mod clock;
mod config;
mod control_socket;
pub mod correction;
pub mod decode;
#[cfg(feature = "decode-gstreamer")]
pub mod decode_gstreamer;
pub mod detection_accel;
mod detector;
pub mod detector_workclass;
pub mod doctor;
pub mod encode;
#[cfg(feature = "fabric")]
pub mod fabric;
mod ha_camera_registration;
mod health;
mod http_data_plane;
mod live_read;
mod media_pipeline;
pub mod offload_policy;
mod privilege;
pub mod recognition;
mod runtime;
mod runtime_stats;
pub mod secret;
pub mod settings;
mod shutdown;
pub mod site_channel;
mod store;
mod supervisor;
pub mod workgraph;
mod yolox_detector;

pub use clock::PersistedClock;
pub use correction::{
    CorrectionError, CorrectionReceipt, CorrectionRequest, CorrectionType, EventRow, EventsView,
    RecordedCorrection, ReviewError, WhyView, correction_execution_fingerprint, record_correction,
    record_correction_with_clock, review_events, review_why,
};
pub use health::{HEALTH_PATH, HealthState, HealthStatus};
pub use http_data_plane::{
    CORRECTION_ROUTE, EVENTS_ROUTE, MEDIA_ROUTE_PREFIX, ReviewDataPlaneHandle, WHY_ROUTE_PREFIX,
    spawn_review_data_plane, spawn_review_data_plane_with_clock,
};
pub use media_pipeline::{DecodedRgbFrame, VideoCodec};
pub use privilege::{PrivilegeStep, privilege_drop_plan};
pub use secret::Secret;
pub use site_channel::{
    CameraAnnouncement, CommandListener, ConnectionEndpoint, DetectionChannel, DetectionFact,
    NoSiteChannel, SiteAnnouncement, SiteChannelFactory, SiteControl, SubmitCorrectionError,
};

/// The operator-facing acceleration intent, resolved from every config
/// entry point with absent-means-true semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccelerationIntent {
    pub hardware_decoding: bool,
    pub accelerated_detection: bool,
}

/// Resolve the acceleration intent exactly as `vigil run` would from the
/// same arguments (config file, environment, CLI overrides).
pub fn acceleration_intent_from_args(args: Vec<OsString>) -> Result<AccelerationIntent, String> {
    config::load(args).map(|config| AccelerationIntent {
        hardware_decoding: config.hardware_decoding,
        accelerated_detection: config.accelerated_detection,
    })
}

/// The operator-facing fabric enrollment intent (criterion C10): every
/// knob has a sane default and resolves with nothing provided.
///
/// `Debug` is hand-written, not derived: `fabric_ticket` is an enrollment
/// credential, following the same discipline `config::RuntimeConfig`/
/// `PartialConfig`/`CliOverrides`/`FabricFileConfig` already apply to the
/// identical field.
#[derive(Clone, PartialEq, Eq)]
pub struct FabricIntent {
    pub fabric_ticket: Option<String>,
    pub fabric_hub: bool,
}

impl std::fmt::Debug for FabricIntent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FabricIntent")
            .field(
                "fabric_ticket",
                &self.fabric_ticket.as_ref().map(|_| "<redacted>"),
            )
            .field("fabric_hub", &self.fabric_hub)
            .finish()
    }
}

/// Resolve the fabric enrollment intent exactly as `vigil run` would from
/// the same arguments (config file, options.json, environment, CLI
/// overrides).
pub fn fabric_intent_from_args(args: Vec<OsString>) -> Result<FabricIntent, String> {
    config::load(args).map(|config| FabricIntent {
        fabric_ticket: config.fabric_ticket,
        fabric_hub: config.fabric_hub,
    })
}

/// The operator-facing fabric TUNING intent (criterion C10, fix cycle 9):
/// every knob has a sane default and resolves with nothing provided.
/// Mirrors [`FabricIntent`] (same precedent, 705b1ac). Inert scaffold: not
/// yet wired to `OffloadPolicyConfig`/`WorkerConfig` construction in
/// `fabric.rs` — see `crates/vigil/tests/fabric_config_defaults.rs` and
/// `crates/vigil/tests/fabric_worker_lease_knob.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FabricTuningIntent {
    pub fabric_worker_lease_ms: u64,
    pub fabric_fallback_horizon_ms: u64,
}

/// Resolve the fabric tuning intent exactly as `vigil run` would from the
/// same arguments (config file, options.json, environment, CLI overrides).
pub fn fabric_tuning_intent_from_args(args: Vec<OsString>) -> Result<FabricTuningIntent, String> {
    config::load(args).map(|config| FabricTuningIntent {
        fabric_worker_lease_ms: config.fabric_worker_lease_ms,
        fabric_fallback_horizon_ms: config.fabric_fallback_horizon_ms,
    })
}

/// One `[[cameras]]` entry's resolved source, exactly as `config::load`
/// determined it — a projection of the real `CameraEntry`/
/// `CameraSourceKind` resolution (`config::resolve_camera_source_kind`,
/// `config::artifact_supports_source_kind`), not a second config surface.
/// Mirrors the [`FabricIntent`]/[`FabricTuningIntent`] precedent above:
/// test/tooling support for driving the real loader from an integration
/// test without exposing the full `RuntimeConfig`/`CameraEntry` surface.
/// Never carries a raw secret — MJPEG credentials via the separate
/// `password` field are represented only as `has_password`.
///
/// `Debug` is hand-written, not derived: `rtsp_url`/`live_rtsp_url`/
/// `endpoint_url` can carry an embedded PASSWORD in their userinfo the
/// same way the real `mjpeg_url`/`rtsp_url` config fields can (this is
/// this run's OWN new type, so unlike `CameraEntry` there is no
/// pre-existing shape to preserve — the derive was simply wrong from the
/// start). The separate `username` field is a diagnostic, not a secret
/// (owner ruling), and prints plainly — only `has_password` stands in for
/// the real password, exactly as the type's own doc above says.
/// `PartialEq`/`Eq` stay derived and compare the real, unredacted values,
/// so equality assertions in tests are unaffected; only how the type
/// PRINTS changes.
#[derive(Clone, PartialEq, Eq)]
pub enum CameraSourceSummary {
    Rtsp {
        rtsp_url: String,
        live_rtsp_url: Option<String>,
    },
    Usb {
        hardware_identity: String,
    },
    Csi {
        hardware_identity: String,
    },
    Mjpeg {
        endpoint_url: String,
        username: Option<String>,
        has_password: bool,
    },
}

impl std::fmt::Debug for CameraSourceSummary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CameraSourceSummary::Rtsp {
                rtsp_url,
                live_rtsp_url,
            } => formatter
                .debug_struct("Rtsp")
                .field(
                    "rtsp_url",
                    &config::redact_url_userinfo(rtsp_url, config::UrlRedactionPolicy::Display),
                )
                .field(
                    "live_rtsp_url",
                    &live_rtsp_url.as_deref().map(|url| {
                        config::redact_url_userinfo(url, config::UrlRedactionPolicy::Display)
                    }),
                )
                .finish(),
            CameraSourceSummary::Usb { hardware_identity } => formatter
                .debug_struct("Usb")
                .field("hardware_identity", hardware_identity)
                .finish(),
            CameraSourceSummary::Csi { hardware_identity } => formatter
                .debug_struct("Csi")
                .field("hardware_identity", hardware_identity)
                .finish(),
            CameraSourceSummary::Mjpeg {
                endpoint_url,
                username,
                has_password,
            } => formatter
                .debug_struct("Mjpeg")
                .field(
                    "endpoint_url",
                    &config::redact_url_userinfo(endpoint_url, config::UrlRedactionPolicy::Display),
                )
                .field("username", username)
                .field("has_password", has_password)
                .finish(),
        }
    }
}

/// Load camera configuration exactly as `vigil run` would (config file,
/// options.json, environment, CLI overrides), resolving each configured
/// camera's real source kind through the SAME path the runtime uses —
/// `config::load`, which itself calls `config::resolve_camera_source_kind`
/// and `config::artifact_supports_source_kind`. A camera whose source kind
/// this artifact cannot carry, or whose fields are missing/conflicting,
/// makes the WHOLE load fail with `config::load`'s own actionable error —
/// there is no way to obtain a partial or best-effort camera list here that
/// production code does not also see.
pub fn camera_source_summaries_from_args(
    args: Vec<OsString>,
) -> Result<Vec<CameraSourceSummary>, String> {
    let loaded = config::load(args)?;
    Ok(loaded
        .cameras
        .into_iter()
        .map(|camera| {
            if let Some(rtsp_url) = camera.rtsp_url {
                CameraSourceSummary::Rtsp {
                    rtsp_url,
                    live_rtsp_url: camera.live_rtsp_url,
                }
            } else if let Some(hardware_identity) = camera.usb_device {
                CameraSourceSummary::Usb { hardware_identity }
            } else if let Some(hardware_identity) = camera.csi_module {
                CameraSourceSummary::Csi { hardware_identity }
            } else if let Some(endpoint_url) = camera.mjpeg_url {
                CameraSourceSummary::Mjpeg {
                    endpoint_url,
                    username: camera.username,
                    has_password: camera.password.is_some(),
                }
            } else {
                // Unreachable in practice: `config::load` never constructs
                // a `CameraEntry` whose kind failed
                // `resolve_camera_source_kind` (it returns `Err` first),
                // and the single-camera legacy fallback either carries
                // `rtsp_url` or is the well-known "no source configured"
                // default — never a `[[cameras]]`-list entry, which is the
                // only path that reaches this closure at all.
                CameraSourceSummary::Rtsp {
                    rtsp_url: String::new(),
                    live_rtsp_url: None,
                }
            }
        })
        .collect())
}

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use context_graph::{Store, request_control};

/// Every setting currently declared through the typed settings registry
/// (see [`settings`]). Calls [`config::declare_settings`] — the single
/// production function that declares every setting `config::load` resolves
/// — so this can never fall out of sync with what `load` actually declares;
/// a second declared setting is picked up here with no further edit.
/// Test/tooling support for the settings-registry coverage check.
pub fn declared_settings() -> Vec<settings::SettingCoverageEntry> {
    let mut registry = settings::SettingsRegistry::new();
    let _declared = config::declare_settings(&mut registry);
    registry.coverage_entries().to_vec()
}

/// Whether a standalone-config TOML fragment assigning `raw_value` to
/// `name` actually sets the field it names, using the real config-file
/// parser. Test/tooling support for the settings-registry coverage check.
pub fn config_file_recognizes_setting(name: &str, raw_value: &str) -> bool {
    config::config_file_fragment_sets(name, raw_value)
}

/// The single CLI entry point. A composition root (`vigil-bin`'s `main`)
/// supplies `factory`, the integration seam every command but `run` ignores.
/// There is deliberately no default-integration-free variant: an embedder
/// calling this always states what site channel it wants, rather than
/// silently getting [`site_channel::NoSiteChannel`] (no Home Assistant
/// integration at all) by omission.
pub fn run_cli_with_site_channel<I>(
    args: I,
    factory: &dyn site_channel::SiteChannelFactory,
) -> ExitCode
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let _program = args.next();

    // Add-on start path: write the probe-deadline options into the environment
    // the decode/detection startup probes read, before any command dispatches.
    config::apply_addon_probe_deadline_env();

    match args.next().and_then(|arg| arg.into_string().ok()) {
        Some(flag) if flag == "--version" => {
            println!("vigil {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(flag) if flag == "--help" || flag == "-h" => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(command) if command == "events" => print_control_or_direct("events", ""),
        Some(command) if command == "why" => {
            let request = args
                .next()
                .and_then(|arg| arg.into_string().ok())
                .unwrap_or_else(|| "--latest".to_string());
            print_control_or_direct("why", &request)
        }
        Some(command) if command == "stats" => print_control_or_direct("stats", ""),
        Some(command) if command == "enroll" => {
            let detection_id = args.next().and_then(|a| a.into_string().ok());
            let name = args.next().and_then(|a| a.into_string().ok());
            match (detection_id, name) {
                (Some(detection_id), Some(name)) => {
                    print_control_or_direct("enroll", &format!("{detection_id} {name}"))
                }
                _ => {
                    eprintln!("usage: vigil enroll <detection-id> <name>");
                    ExitCode::from(2)
                }
            }
        }
        Some(command) if command == "forget" => {
            match args.next().and_then(|a| a.into_string().ok()) {
                Some(name) => print_control_or_direct("forget", &name),
                None => {
                    eprintln!("usage: vigil forget <name>");
                    ExitCode::from(2)
                }
            }
        }
        Some(command) if command == "doctor" => match doctor::run(args.collect()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(2)
            }
        },
        Some(command) if command == "fabric" => {
            match args
                .next()
                .and_then(|arg| arg.into_string().ok())
                .as_deref()
            {
                Some("ticket") => match run_fabric_ticket_command(args.collect()) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => {
                        eprintln!("{error}");
                        ExitCode::from(2)
                    }
                },
                _ => {
                    eprintln!("usage: vigil fabric ticket [--data-dir PATH]");
                    ExitCode::from(2)
                }
            }
        }
        Some(command) if command == "detector-probe" => runtime::run_detector_probe(args.collect()),
        Some(command) if command == "run" => runtime::run(args.collect(), factory),
        _ => {
            print_help();
            ExitCode::from(2)
        }
    }
}

pub fn open_context_graph_store_with_text_embedder_disabled(path: &Path) -> Result<Store, String> {
    store::open(path).map(|open| open.handle)
}

pub fn decode_sampled_detector_rgb_frames(
    clip: &Path,
    sample_frames: usize,
    width: u32,
    height: u32,
) -> Result<(Vec<u8>, usize), String> {
    let segment = media_pipeline::decode_video_file(clip)?;
    media_pipeline::sampled_detector_rgb(&segment.frames, sample_frames, width, height)
}

fn print_control_or_direct(command: &str, request: &str) -> ExitCode {
    match ask_runtime_owner(command, request) {
        Ok(response) => {
            print!("{response}");
            ExitCode::SUCCESS
        }
        Err(socket_error) => match direct_read_local(command, request) {
            Ok(response) => {
                print!("{response}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                if error.to_ascii_lowercase().contains("not found") {
                    eprintln!("{error}");
                } else {
                    eprintln!("{error}; runtime owner unavailable: {socket_error}");
                }
                ExitCode::from(2)
            }
        },
    }
}

#[cfg(unix)]
fn ask_runtime_owner(command: &str, request: &str) -> Result<String, String> {
    let socket_path = control_socket::control_socket_path(&data_dir_from_env());
    request_control(&socket_path, &format!("{command} {request}\n"))
}

#[cfg(not(unix))]
fn ask_runtime_owner(_command: &str, _request: &str) -> Result<String, String> {
    Err("runtime owner control socket is only available on Unix".to_string())
}

fn direct_read_local(command: &str, request: &str) -> Result<String, String> {
    if command == "stats" {
        let stats = runtime_stats::read_snapshot(&data_dir_from_env()).unwrap_or_default();
        return Ok(runtime_stats::format_stats(&stats));
    }

    let store_path = store_path_from_env();
    let open = store::open(&store_path).map_err(|error| {
        let error_kind = if is_database_locked_error(&error) {
            "database_locked"
        } else {
            "store_open_failed"
        };
        format!(
            "runtime busy error_kind={error_kind}, retry through the running owner for {}: {error}",
            store_path.display()
        )
    })?;
    match command {
        "events" => live_read::handle_events_read(&open.handle, 100)
            .map(|response| live_read::format_events_cli(&response)),
        "why" => live_read::handle_why_read(&open.handle, request)
            .map(|response| live_read::format_why_cli(&response)),
        // Offline enroll/forget work on a plain re-open: enrollment reads the
        // sighting's stored probe vector (no embedder needed) and the embedding
        // space is already persisted in the store.
        "enroll" => {
            let mut pieces = request.splitn(2, ' ');
            let detection_id = pieces.next().unwrap_or_default().trim().to_string();
            let name = pieces.next().unwrap_or_default().trim().to_string();
            if detection_id.is_empty() || name.is_empty() {
                return Err("usage: vigil enroll <detection-id> <name>".to_string());
            }
            correction::record_correction(
                &open.handle,
                correction::CorrectionRequest {
                    detection_id,
                    label: Some(name.clone()),
                    correction_type: correction::CorrectionType::Enroll,
                },
            )
            .map(|receipt| {
                format!(
                    "enrolled=true name={name} correction_id={}\n",
                    receipt.correction_id
                )
            })
            .map_err(|error| format!("{error:?}"))
        }
        "forget" => {
            let name = request.trim();
            if name.is_empty() {
                return Err("usage: vigil forget <name>".to_string());
            }
            recognition::forget_named_entity(&open.handle, name, None)
                .map(|removed| format!("forgotten=true name={name} references_removed={removed}\n"))
        }
        _ => Err(format!("unknown control command {command}")),
    }
}

fn is_database_locked_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("database is locked") || (lower.contains("locked") && lower.contains("process"))
}

// VIGIL_STORE_PATH is an enumerated, reviewed override
// (`environment_read_surface.baseline.txt`), not an ad-hoc read.
#[allow(clippy::disallowed_methods)]
fn store_path_from_env() -> std::path::PathBuf {
    std::env::var_os("VIGIL_STORE_PATH")
        .map(Into::into)
        .unwrap_or_else(|| data_dir_from_env().join("store.contextgraph"))
}

// VIGIL_DATA_DIR/VIGIL_STORE_PATH are enumerated, reviewed overrides
// (`environment_read_surface.baseline.txt`), not an ad-hoc read.
#[allow(clippy::disallowed_methods)]
fn data_dir_from_env() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("VIGIL_DATA_DIR") {
        return path.into();
    }
    if let Some(path) = std::env::var_os("VIGIL_STORE_PATH").map(std::path::PathBuf::from)
        && let Some(parent) = path.parent()
    {
        return parent.to_path_buf();
    }
    std::env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .join("vigil-data")
}

fn print_help() {
    println!("Usage: vigil <COMMAND>");
    println!();
    println!("Commands:");
    println!("  run");
    println!("  fabric ticket");
    println!();
    println!("Options:");
    println!("  --help");
    println!("  --version");
}

/// `vigil fabric ticket`'s dispatch target, named regardless of whether this
/// artifact was built with the `fabric` cargo feature (discoverability on
/// `--help` must not depend on which artifact was built, the same way
/// `--fabric-ticket`/`--fabric-hub` are documented in every `vigil run`
/// build). A build without the feature names the fix, exactly like
/// `run`'s own `validate_compiled_capability_requests`.
fn run_fabric_ticket_command(args: Vec<OsString>) -> Result<(), String> {
    #[cfg(feature = "fabric")]
    {
        fabric::run_ticket_command(args)
    }
    #[cfg(not(feature = "fabric"))]
    {
        let _ = args;
        Err(
            "fabric was configured, but this Vigil binary was built without the fabric \
             capability; use an amd64/aarch64 normal release artifact or rebuild with \
             --features fabric"
                .to_string(),
        )
    }
}
