// The dev-box-only `detect-burn-wgpu` feature pulls in Burn's cubecl/wgpu
// backend, whose deeply generic tensor/kernel types need more headroom than
// the default recursive-type-checking limit.
#![cfg_attr(feature = "detect-burn-wgpu", recursion_limit = "256")]

pub mod acceleration;
pub mod camera_hub;
pub mod camera_track;
mod clock;
mod config;
pub mod correction;
pub mod decode;
#[cfg(feature = "decode-gstreamer")]
pub mod decode_gstreamer;
pub mod detection_accel;
pub mod detection_transition;
mod detector;
pub mod detector_workclass;
pub mod doctor;
pub mod encode;
#[cfg(feature = "fabric")]
pub mod fabric;
mod ha_camera_registration;
mod health;
mod http_data_plane;
pub mod live_backends;
mod live_read;
mod media_pipeline;
pub mod node_key;
pub mod offload_policy;
mod privilege;
pub mod recognition;
mod runtime;
mod runtime_stats;
pub mod secret;
pub mod service_identity;
pub mod settings;
pub mod settings_application;
pub mod settings_backends;
mod settings_cache;
pub mod settings_command;
pub mod settings_degraded;
pub mod settings_domains;
pub mod settings_environment;
pub mod settings_model;
pub mod settings_projection;
pub mod settings_reflection;
pub mod settings_store;
mod shutdown;
pub mod site_channel;
mod store;
mod supervisor;
pub mod workgraph;
mod yolox_detector;

pub use clock::PersistedClock;
pub use config::{StoreLocation, StoreLocationEnvironment, resolve_store_location};
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
/// The marker every answer served by the running runtime carries, re-exported
/// so a caller distinguishes an owner-served answer from a direct read without
/// re-spelling it.
pub use live_read::OWNER_SERVED_PREFIX;
pub use media_pipeline::{DecodedRgbFrame, VideoCodec};
pub use privilege::{
    PrivilegeStep, RuntimeUserStep, privilege_drop_plan, runtime_user_plan_for_daemon,
    runtime_user_plan_for_live_command,
};
pub use secret::Secret;
pub use site_channel::{
    CameraAnnouncement, CommandListener, ConnectionEndpoint, DetectionChannel, DetectionFact,
    NoSiteChannel, SiteAnnouncement, SiteChannelFactory, SiteControl, SitePresence,
    SubmitCorrectionError,
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

/// What one input surface asserts into the settings store, exactly as
/// `config::load` built it. A projection of the loader's own per-surface
/// assertions rather than a second reading of the file, mirroring the
/// [`CameraSourceSummary`] precedent above: it lets a caller observe which
/// setting a surface authored without exposing the whole `RuntimeConfig`.
///
/// `Debug` is derived, and safely: the loader deliberately keeps the camera
/// stream out of these entries because its userinfo carries the camera's
/// password, so nothing a surface asserts here is a secret.
#[derive(Debug, Clone)]
pub struct SurfaceAuthoring {
    pub surface: settings_model::Surface,
    pub entries: Vec<(String, settings_model::SettingValue)>,
    /// What this surface says about each camera it lists, camera by camera —
    /// the per-camera twin of `entries` above, exactly as `config::load`
    /// built it. Exposed so a caller can observe whether a `cameras[].<key>`
    /// value the surface named actually reached a camera's own record,
    /// without exposing the whole `RuntimeConfig`.
    pub camera_entries: Vec<(String, Vec<(String, settings_model::SettingValue)>)>,
}

/// Every input surface's assertions, exactly as `vigil run` would resolve them
/// from the same arguments.
pub fn surface_authoring_from_args(args: Vec<OsString>) -> Result<Vec<SurfaceAuthoring>, String> {
    config::load(args).map(|loaded| {
        [loaded.file_surface, loaded.startup_surface]
            .into_iter()
            .flatten()
            .map(|assertions| SurfaceAuthoring {
                surface: assertions.surface,
                entries: assertions.entries,
                camera_entries: assertions.camera_entries,
            })
            .collect()
    })
}

/// The classes recognition covers, exactly as `vigil run` would resolve them
/// from the same arguments.
pub fn recognition_covered_classes_from_args(args: Vec<OsString>) -> Result<Vec<String>, String> {
    config::load(args).map(|loaded| loaded.recognition.covered_classes)
}

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use context_graph::Store;
use context_graph::owner_control::{self, OwnerControlError};

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
    // Before anything is dispatched: a variable that was withdrawn cannot be
    // allowed to sit in the environment doing nothing. Whatever the operator
    // asked for, they asked for it believing a deployment wired the way they
    // wrote it, so the answer is the migration refusal and not the command.
    if let Some(refusal) = retired_variable_refusal() {
        eprint!("{refusal}");
        return ExitCode::from(2);
    }

    let mut args = args.into_iter();
    let _program = args.next();

    let command = args.next().and_then(|arg| arg.into_string().ok());

    // A variable that names a behavior setting placed nothing either. That one
    // is not a refusal — the command still runs and still answers — but it is
    // never silently dropped: the operator is told which variable did nothing
    // and where the value belongs instead. It goes on the error stream, because
    // it is an advisory about their environment and never the answer to their
    // question, and it is said on the two surfaces the statement belongs to:
    // the settings surface, which is where what a value is and who authored it
    // is discussed, and the startup output of the run that would have honored
    // the variable if the environment still placed anything. Stapling it to a
    // review answer instead would put an unrelated note on the thing the
    // operator actually asked for.
    if matches!(command.as_deref(), Some("settings") | Some("run")) {
        let ignored = settings_environment::ignored_variable_report();
        if !ignored.is_empty() {
            eprint!("{ignored}");
        }
    }

    let asks_running_deployment = command
        .as_deref()
        .is_some_and(|name| commands_that_ask_the_running_deployment().contains(&name));

    // Supervisor owns the add-on options file and deliberately makes it
    // root-readable only. Resolve the deployment's location while this command
    // still has that narrow startup privilege, then carry only the resulting
    // paths across the identity change below. No store or owner channel is
    // opened here.
    let live_locations = if asks_running_deployment {
        match configured_locations() {
            Ok(locations) => Some(locations),
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };

    // The identity step for the WHOLE family, decided here from the
    // deployment's own list rather than re-decided at each dispatch arm below.
    // The owner channel every one of these subcommands answers over authorizes
    // on the peer's operating-system user and nothing else, so a dispatch that
    // skipped this opens that channel as whoever ran it and is refused for a
    // mismatch the operator never chose and cannot see — which is exactly what
    // `vigil settings` did while the read commands beside it were served. A
    // subcommand added to the family is covered the day it is added.
    if asks_running_deployment && let Err(error) = privilege::adopt_runtime_user_for_live_command()
    {
        eprintln!("{error}");
        return ExitCode::from(2);
    }

    match command {
        Some(flag) if flag == "--version" => {
            println!("vigil {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(flag) if flag == "--help" || flag == "-h" => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(command) if command == "events" => print_control_or_direct(
            live_locations
                .as_ref()
                .expect("live-command locations were resolved before dispatch"),
            "events",
            "",
        ),
        Some(command) if command == "why" => {
            let request = args
                .next()
                .and_then(|arg| arg.into_string().ok())
                .unwrap_or_else(|| "--latest".to_string());
            print_control_or_direct(
                live_locations
                    .as_ref()
                    .expect("live-command locations were resolved before dispatch"),
                "why",
                &request,
            )
        }
        Some(command) if command == "stats" => print_control_or_direct(
            live_locations
                .as_ref()
                .expect("live-command locations were resolved before dispatch"),
            "stats",
            "",
        ),
        Some(command) if command == "settings" => {
            let request = args
                .filter_map(|arg| arg.into_string().ok())
                .collect::<Vec<String>>()
                .join(" ");
            print_settings(
                live_locations
                    .as_ref()
                    .expect("live-command locations were resolved before dispatch"),
                &request,
            )
        }
        Some(command) if command == "enroll" => {
            let detection_id = args.next().and_then(|a| a.into_string().ok());
            let name = args.next().and_then(|a| a.into_string().ok());
            match (detection_id, name) {
                (Some(detection_id), Some(name)) => print_control_or_direct(
                    live_locations
                        .as_ref()
                        .expect("live-command locations were resolved before dispatch"),
                    "enroll",
                    &format!("{detection_id} {name}"),
                ),
                _ => {
                    eprintln!("usage: vigil enroll <detection-id> <name>");
                    ExitCode::from(2)
                }
            }
        }
        Some(command) if command == "forget" => {
            match args.next().and_then(|a| a.into_string().ok()) {
                Some(name) => print_control_or_direct(
                    live_locations
                        .as_ref()
                        .expect("live-command locations were resolved before dispatch"),
                    "forget",
                    &name,
                ),
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

/// The settings surface answers through the runtime that owns the store when
/// one is up, and directly from the store when none is. Both render the same
/// projection; a refusal is a real answer that names its cause and its remedy,
/// and it exits non-zero because it did not do what was asked.
fn print_settings(locations: &config::StoreLocation, request: &str) -> ExitCode {
    // Both were resolved ONCE at dispatch, before the Supervisor runtime-user
    // transition, and carried here: the deployment's own directory and the
    // exact store file this command is about.
    let data_dir = locations.data_dir.clone();
    let store_path = locations.store_path.clone();
    let answer = match ask_runtime_owner(locations, "settings", request) {
        Ok(response) => response,
        Err(error) if may_read_the_store_directly(&error) => {
            settings_command::answer(&data_dir, &store_path, request)
        }
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    if settings_command::answer_failed(&answer) {
        eprint!("{answer}");
        return ExitCode::from(2);
    }
    print!("{answer}");
    ExitCode::SUCCESS
}

/// How a stats answer opens when the snapshot in the data directory cannot be
/// attributed to any process. It is a refusal like any other — it names its
/// cause and its remedy and it did not serve what was asked — so the caller
/// prints it plainly and exits non-zero, with no transport note appended.
const NO_RUNTIME_OWNER_REFUSAL: &str = "no runtime owns ";

/// Why a command could not be answered from this deployment's own store, and
/// whether what it says is already a complete answer.
///
/// The distinction is delivered, not decorative. A REFUSAL names its own cause
/// and its own remedy — the store is momentarily busy with a reader, this
/// capability is unavailable on a degraded run, the snapshot belongs to nobody
/// — so nothing may be appended to it. A FAILURE does not, so the note about
/// the owner route is worth having beside it.
///
/// It is carried as a classification rather than recovered by reading the
/// answer's own text: the busy answer is produced from context-graph's typed
/// `StoreHeldByReaders` two layers down, and a caller that re-derived its
/// meaning from a message prefix appended "no process is holding the store" to
/// an answer that had just named the process holding it.
struct DirectReadFailure {
    text: String,
    refusal: bool,
}

impl DirectReadFailure {
    /// A complete answer that is not the one asked for.
    fn refusal(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            refusal: true,
        }
    }

    /// The command genuinely failed.
    fn failed(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            refusal: false,
        }
    }
}

fn print_control_or_direct(
    locations: &config::StoreLocation,
    command: &str,
    request: &str,
) -> ExitCode {
    // The location was resolved once while the Supervisor-owned options file
    // was readable. The identity step then ran before the owner channel or
    // store below was touched, because that channel authorizes on the peer's
    // operating-system user and nothing else.
    match ask_runtime_owner(locations, command, request) {
        // A refusal is a real answer — it names its cause and its remedy — but
        // it is never a served request: an operator who asked for review
        // history and got an explanation of why there is none did not get
        // review history, and the exit status has to say so.
        Ok(response) if settings_degraded::is_refusal(&response) => {
            eprint!("{response}");
            ExitCode::from(2)
        }
        // The owner answered, and the answer is that it could not serve this
        // request: no event under that id, or an id that is not one. Same
        // delivery as the direct read gives, because a caller reading the
        // status cannot see which road the answer came down.
        Ok(response) if live_read::answer_failed(&response) => {
            eprint!("{response}");
            ExitCode::from(2)
        }
        Ok(response) => {
            print!("{response}");
            ExitCode::SUCCESS
        }
        // A runtime IS holding this store and did not answer. The store is
        // not free to read behind it, so the refusal is the answer.
        Err(owner_error) if !may_read_the_store_directly(&owner_error) => {
            eprintln!("{owner_error}");
            ExitCode::from(2)
        }
        Err(owner_error) => match direct_read_local(locations, command, request) {
            Ok(response) => {
                print!("{response}");
                ExitCode::SUCCESS
            }
            Err(failure) if failure.refusal => {
                // The refusal already names its cause and its remedy; a note
                // about the owner route appended to it would only tell the
                // operator about a road they never asked about — and on a
                // store somebody is reading it would contradict the sentence
                // in front of it, naming the holder and then saying nobody
                // holds it.
                eprintln!("{}", failure.text);
                ExitCode::from(2)
            }
            Err(failure) => {
                eprintln!("{}; runtime owner unavailable: {owner_error}", failure.text);
                ExitCode::from(2)
            }
        },
    }
}

/// What every vigil command prints instead of running, when the environment
/// still carries a variable this product withdrew.
///
/// Silently ignoring it is the one behavior that is never available: the
/// operator set it on purpose, and a deployment that reads as configured while
/// the value places nothing makes every later diagnosis start from a false
/// premise. Reinterpreting it is no better — it would hand a retired knob
/// authority over the road that replaced it. So the refusal names the
/// variable, says it has no effect, and says to unset it.
fn retired_variable_refusal() -> Option<String> {
    let withdrawn = settings_environment::retired_variables_in_force();
    if withdrawn.is_empty() {
        return None;
    }
    let mut refusal = String::new();
    for variable in withdrawn {
        refusal.push_str(&format!("{}\n{}\n", variable.reason, variable.set_it_here));
    }
    Some(refusal)
}

/// The refusal a command aimed at an unreadable store gets, whichever surface
/// it arrived through. A store that exists and cannot be opened is the
/// degraded condition, so the answer names the capability, says it is
/// unavailable, and names the store — never a bare open error the operator has
/// to interpret.
fn degraded_refusal_for(
    command: &str,
    class: &settings_degraded::StoreOpenClass,
) -> Option<String> {
    match class {
        // Another runtime owns this store. That is a different failure with a
        // different answer, and it must not be dressed up as degraded.
        settings_degraded::StoreOpenClass::LockedByAnotherRuntime { .. } => None,
        // Somebody is READING it. The operator keeps their review history —
        // they are told who is holding the store and that the same question
        // answers in a moment, rather than that this deployment is degraded and
        // its history unavailable.
        settings_degraded::StoreOpenClass::HeldByReaders {
            observed_direct_readers,
            readers,
            path,
        } => Some(settings_projection::render_busy_with_readers(
            path,
            *observed_direct_readers,
            readers,
        )),
        settings_degraded::StoreOpenClass::Absent
        | settings_degraded::StoreOpenClass::Unreadable { .. } => {
            settings_degraded::capability_for_command(command).map(settings_degraded::refusal_line)
        }
        // An open that was REFUSED cannot have classified as opening cleanly.
        // Enumerated rather than swept into a catch-all so that a future class
        // stops the build here instead of quietly taking a refusal's answer.
        settings_degraded::StoreOpenClass::Opens => None,
    }
}

/// Ask the process that owns this deployment's store to answer `command`.
///
/// The store path is the whole address: there is no transport location to
/// configure, publish or clean up, and the frame is the same text the handler
/// has always parsed. A refusal comes back typed, because the caller has to
/// decide something on it — see [`may_read_the_store_directly`].
#[cfg(unix)]
fn ask_runtime_owner(
    locations: &config::StoreLocation,
    command: &str,
    request: &str,
) -> Result<String, OwnerControlError> {
    owner_control::request_owner(&locations.store_path, &format!("{command} {request}\n"))
}

/// Off Unix there is no local owner channel to reach, which is the same
/// situation as nobody holding the store: the caller does the work itself.
#[cfg(not(unix))]
fn ask_runtime_owner(
    locations: &config::StoreLocation,
    _command: &str,
    _request: &str,
) -> Result<String, OwnerControlError> {
    Err(OwnerControlError::OwnerNotRunning {
        store_path: locations.store_path.clone(),
    })
}

/// Whether a refused owner request leaves this command free to open the store
/// and answer from it.
///
/// Two things say the file is free. "Nobody is holding this store" and "there
/// is no store there" say it outright. And a request that never got an ANSWER
/// out of the channel — it timed out, the channel took the connection and went
/// quiet — says nothing about the store at all: the road failed, and the store
/// beside it may be sitting there perfectly readable. Refusing to look costs
/// the operator their review history over a transport they never asked about,
/// and hands them a sentence naming "the process holding the store" when no
/// process is holding it. The store's own lock is the real arbiter: if a
/// runtime does hold it, the direct open is refused and that refusal is the
/// answer they get.
///
/// And a holder whose channel does not serve this inspection at all is not an
/// owner refusing this request. Every writable open of a context-graph store
/// stands a channel up whether or not the opener asked to serve anything, so a
/// store held by a process that registered no owner-request handler — another
/// vigil command that opened it to read, a tool of the operator's own —
/// answers on the channel and says it serves no such route. That answer will
/// not change while that holder lives, so there is nothing to wait for and the
/// caller answers for itself; it is the one refusal that names what a caller
/// may still get. `vigil stats` is the sharpest case, because its figures come
/// from the deployment's own snapshot and never from the store at all.
///
/// Every other variant is a live owner speaking for itself — it is winding
/// down, it is already serving as many requests as it admits, it is holding
/// the store and has not yet published a serving decision, it refused this one
/// — and opening the store behind it would either be refused by the lock or,
/// worse, put a second writer behind the first one's back. Answering from
/// there is not a fallback; it is a different deployment's answer.
fn may_read_the_store_directly(error: &OwnerControlError) -> bool {
    error.owner_absent()
        || matches!(
            error,
            OwnerControlError::OwnerTimedOut { .. }
                | OwnerControlError::OwnerRouteUnsupported { .. }
        )
}

/// What each live command answers on a deployment that has never started —
/// decided from the deployment's own emptiness rather than from a store opened
/// to discover it.
///
/// `events` and `why` keep exactly the answers they always gave here, because
/// they were always the true ones: there are no events, and there is no event
/// under the id asked for. `enroll` and `forget` are edits, and an edit has
/// nothing to act on: they refuse, naming the deployment directory and what to
/// do about it rather than a store to repair. `stats` never reaches this — it
/// reads the deployment's own snapshot and opens no store at all.
fn never_started_answer(
    command: &str,
    request: &str,
    data_dir: &Path,
    store_path: &Path,
) -> Result<String, DirectReadFailure> {
    match command {
        // No rows, which is the whole answer, and it is a SERVED one.
        "events" => Ok(String::new()),
        "why" => Err(DirectReadFailure::refusal(
            live_read::why_refusal_without_a_store(request),
        )),
        "enroll" | "forget" => Err(DirectReadFailure::refusal(
            settings_degraded::never_started_refusal(
                settings_degraded::UnavailableCapability::Corrections,
                data_dir,
                store_path,
            )
            .trim_end()
            .to_string(),
        )),
        other => Err(DirectReadFailure::failed(format!(
            "unknown control command {other}"
        ))),
    }
}

fn direct_read_local(
    locations: &config::StoreLocation,
    command: &str,
    request: &str,
) -> Result<String, DirectReadFailure> {
    if command == "stats" {
        let data_dir = locations.data_dir.clone();
        // Three different questions arrive here as one, and each has its own
        // true answer. A directory nothing has run in has a real answer —
        // nothing has happened — which is served, minus any runtime fact about
        // the process that is not there. A snapshot nobody can be shown to
        // have written cannot be attributed to this deployment at all, so none
        // of it is presented. This deployment's own last run really did
        // produce its figures here, and they are the only surface through
        // which a fault that ended a run stays visible, so they are served
        // under a line saying whose they are and that they are not current.
        return match runtime_stats::read_snapshot_provenance(&data_dir) {
            runtime_stats::SnapshotProvenance::NeverStarted => {
                Ok(runtime_stats::format_not_started_stats(&data_dir))
            }
            // The condition is the snapshot's own: no writer identity can be
            // established for it — either it carries no stamp, or its bytes
            // cannot be read back at all. Neither says the writer has stopped;
            // a file behind a permission wall may be being written right now.
            // Naming a stopped process here points an operator debugging a
            // hand-copied, permission-walled or corrupt snapshot at a cause
            // that was never established.
            runtime_stats::SnapshotProvenance::Unattributable => {
                Err(DirectReadFailure::refusal(format!(
                    "{NO_RUNTIME_OWNER_REFUSAL}{}: the stats snapshot there carries no \
                     establishable writer identity, so nothing in it can be read as this \
                     deployment's figures; start the runtime to read what it is doing",
                    data_dir.display()
                )))
            }
            runtime_stats::SnapshotProvenance::StoppedRun(stats) => {
                Ok(runtime_stats::format_stopped_run_stats(&stats))
            }
            runtime_stats::SnapshotProvenance::LiveRun(stats) => {
                Ok(runtime_stats::format_stats(&stats))
            }
        };
    }

    let store_path = locations.store_path.clone();
    // Each command takes the door that matches what it is DOING, and neither
    // door asks whether the store is there first. `why` and `events` only ask
    // questions, so they take Context Graph's no-create reader and its typed
    // graph reads: two operators asking at once are both served, and the
    // store's bytes are the same after the answer as before it. `enroll` and
    // `forget` change this deployment's recognition, so they take the atomic
    // open-existing-for-change door, which refuses an absent store instead of
    // creating one.
    //
    // The existence check that used to stand in front of both is gone. It
    // answered about a different moment than the open behind it, it answered
    // `false` for a configured store whose volume never mounted exactly as it
    // did for a directory nobody had run anything in, and the open it guarded
    // CREATED the store it opened — so a mistyped path, or `./vigil-data` in
    // the wrong shell, became a deployment. A deployment with no store holds no
    // events and no event under any id, and both are true without a store
    // existing to say so; the open's own typed refusal is what says it.
    let rendered = match command {
        "why" | "events" => match store::open_for_review_read(&store_path) {
            Ok(reader) => live_read::render_read_only(&reader.graph(), command, request),
            Err(refusal) => {
                return refused_open_answer(command, request, locations, &refusal);
            }
        },
        _ => match store::open_existing_for_edit(&store_path) {
            Ok(store) => live_read::render_store_backed(&store, command, request),
            Err(refusal) => {
                return refused_open_answer(command, request, locations, &refusal);
            }
        },
    };
    // The SAME rendering the owner route serves: one projection, both roads.
    // The direct fallback used to format its own failures, so a stopped
    // deployment answered a failed enrollment with a bare debug string nothing
    // recognized as a refusal — and vigil then stapled the owner-route note
    // onto it, telling the operator about a road they never asked about.
    let Some(body) = rendered else {
        return Err(DirectReadFailure::failed(format!(
            "unknown control command {command}"
        )));
    };
    // Classified once, on the markers that rendering already applied: an
    // answer that names its own cause is a complete answer, and nothing is
    // appended to it.
    if live_read::answer_failed(&body) {
        return Err(DirectReadFailure::refusal(body.trim_end().to_string()));
    }
    Ok(body)
}

/// What a live command answers when the door it reached for refused to open.
///
/// The refusal this command's OWN open returned is the whole of what the answer
/// is built from — nothing goes back to look again, because two looks are two
/// moments and a holder that lets go in between makes the second one disagree
/// with the world the operator's command actually met.
///
/// A store that is NOT THERE is answered on the type rather than on the class:
/// a deployment that has never started and a store this process could not
/// resolve share the `Absent` class in one direction only, and just one of them
/// means nothing has ever run here.
fn refused_open_answer(
    command: &str,
    request: &str,
    locations: &config::StoreLocation,
    refusal: &store::StoreOpenRefusal,
) -> Result<String, DirectReadFailure> {
    if matches!(
        *refusal.settings_error,
        settings_model::SettingsError::StoreMissing { .. }
    ) {
        return never_started_answer(command, request, &locations.data_dir, &locations.store_path);
    }
    // A store somebody is READING renders the busy answer, and it travels on as
    // a refusal — a complete answer nothing may be appended to — rather than as
    // a failure whose meaning a later caller has to guess back out of its
    // wording.
    if let Some(answer) = degraded_refusal_for(command, &refusal.class) {
        return Err(DirectReadFailure::refusal(answer.trim_end().to_string()));
    }
    // Read off the classification this open already produced, never by matching
    // the shape of its own message.
    let error_kind = match refusal.class {
        settings_degraded::StoreOpenClass::LockedByAnotherRuntime { .. } => "database_locked",
        _ => "store_open_failed",
    };
    Err(DirectReadFailure::failed(format!(
        "runtime busy error_kind={error_kind}, retry through the running owner for {}: {}",
        locations.store_path.display(),
        refusal.message
    )))
}

pub(crate) fn is_database_locked_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("database is locked") || (lower.contains("locked") && lower.contains("process"))
}

/// Every `vigil` subcommand whose answer comes from this deployment's RUNNING
/// daemon over the owner channel.
///
/// The channel authorizes on the peer's operating-system user and nothing
/// else, so membership of this list is exactly the question "must this
/// subcommand become the deployment's runtime user before it asks?" — and the
/// answer is stated once, here, rather than re-decided at each dispatch arm.
pub fn commands_that_ask_the_running_deployment() -> &'static [&'static str] {
    &["events", "why", "stats", "settings", "enroll", "forget"]
}

/// What the dispatch of `command` does to become this deployment's runtime
/// user, before it opens a channel, a store, or anything else. `None` for a
/// subcommand that performs no identity work at all.
///
/// Every member of [`commands_that_ask_the_running_deployment`] returns the
/// plan [`runtime_user_plan_for_live_command`] builds: the daemon's identity
/// drop, ordered the same way, and NOT the daemon's store preparation — a
/// read or an edit brings no deployment into existence, and a command that
/// created a store directory or moved the ownership of a store a daemon is
/// holding would be rewriting the deployment it came to ask about.
///
/// The dispatch executes this same plan rather than a second spelling of it:
/// `privilege::adopt_runtime_user_for_live_command` runs what
/// `privilege::runtime_user_plan_for_live_command` builds, which is what this
/// returns, so what is pinned here and what runs cannot drift.
pub fn runtime_user_plan_for_dispatched_command(
    command: &str,
    uid: u32,
    gid: u32,
    supplemental_gids: Option<&str>,
) -> Result<Option<Vec<RuntimeUserStep>>, String> {
    if !commands_that_ask_the_running_deployment().contains(&command) {
        return Ok(None);
    }
    runtime_user_plan_for_live_command(uid, gid, supplemental_gids).map(Some)
}

/// Where this deployment keeps its store and its directory, resolved ONCE at
/// the entry point through the SAME location-only rule the daemon resolves
/// them with ([`config::resolve_store_location`]) — the add-on's own
/// `store_path` included.
///
/// The resolution happens at the entry point and the answer is carried from
/// there — never re-read part-way through an operation, and never rebuilt by
/// joining a default filename onto whatever directory happened to be in hand.
/// A process that resolved the path twice could act on two different files in
/// one command; a process that resolved it differently from the daemon looks
/// for an owner that was never there.
fn configured_locations() -> Result<config::StoreLocation, String> {
    config::configured_store_location()
}

/// What this process's own environment states about where things are, read
/// through the two accessors below and handed to the shared resolution.
fn store_location_environment() -> config::StoreLocationEnvironment {
    config::StoreLocationEnvironment {
        data_dir: data_dir_from_env(),
        store_path: store_path_from_env(),
    }
}

/// `VIGIL_STORE_PATH`: the store pathname this process's environment states,
/// or nothing when it states none.
// VIGIL_STORE_PATH is an enumerated, reviewed override
// (`environment_read_surface.baseline.txt`), not an ad-hoc read.
#[allow(clippy::disallowed_methods)]
fn store_path_from_env() -> Option<std::path::PathBuf> {
    std::env::var_os("VIGIL_STORE_PATH").map(Into::into)
}

/// `VIGIL_DATA_DIR`: the deployment directory this process's environment
/// states.
///
/// A store pathname stated with no deployment directory beside it names the
/// directory too — the deployment is where its store is — which is why this
/// accessor also reads `VIGIL_STORE_PATH`. What neither variable states is
/// left unstated here, for the shared resolution to default.
// VIGIL_DATA_DIR/VIGIL_STORE_PATH are enumerated, reviewed overrides
// (`environment_read_surface.baseline.txt`), not an ad-hoc read.
#[allow(clippy::disallowed_methods)]
fn data_dir_from_env() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("VIGIL_DATA_DIR") {
        return Some(path.into());
    }
    std::env::var_os("VIGIL_STORE_PATH")
        .map(std::path::PathBuf::from)
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
}

/// Every subcommand this binary dispatches AND offers to an operator, paired
/// with the arguments it takes, in the order `--help` prints them.
///
/// One list, rendered by [`print_help`] and read by the inventory regression
/// beside it, so what the binary advertises and what an operator can actually
/// run cannot become two different answers — which is what a hand-written help
/// block had already let happen: `events`, `why`, `stats`, `settings`,
/// `enroll`, `forget` and `doctor acceleration` all worked and none of them was
/// named here, so the one place an operator looks said they did not exist.
///
/// `detector-probe` is dispatched and deliberately absent: it is the
/// detection-probe machinery's own subprocess entry point with a wire argument
/// list, not an operator workflow, and naming it here would invite somebody to
/// run it by hand. That is the only dispatched subcommand this list omits.
pub fn public_commands() -> &'static [(&'static str, &'static str)] {
    &[
        ("run", "[OPTIONS]"),
        ("events", ""),
        ("why", "[<DETECTION-ID> | --latest]"),
        ("stats", ""),
        (
            "settings",
            "[list | find <TEXT> | set <SETTING> <VALUE> | reset <SETTING> | \
             identity [change <IDENTIFIER> --confirm]]",
        ),
        ("enroll", "<DETECTION-ID> <NAME>"),
        ("forget", "<NAME>"),
        ("doctor acceleration", "[--service-user USER] [RUN OPTIONS]"),
        ("fabric ticket", "[--data-dir PATH]"),
    ]
}

fn print_help() {
    println!("Usage: vigil <COMMAND>");
    println!();
    println!("Commands:");
    for (command, arguments) in public_commands() {
        if arguments.is_empty() {
            println!("  {command}");
        } else {
            println!("  {command} {arguments}");
        }
    }
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
