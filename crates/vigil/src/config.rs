use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::secret::Secret;
use crate::settings::{SettingHandle, SettingSpec, SettingSurfaces, SettingsRegistry};
use crate::site_channel::ConnectionEndpoint;

/// One camera entry in the multi-camera list.
#[derive(Debug, Clone)]
pub(crate) struct CameraEntry {
    pub(crate) name: String,
    pub(crate) rtsp_url: Option<String>,
    pub(crate) live_rtsp_url: Option<String>,
    pub(crate) username: Option<String>,
    pub(crate) password: Option<Secret>,
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeConfig {
    pub(crate) data_dir: PathBuf,
    pub(crate) store_path: PathBuf,
    pub(crate) health_port: u16,
    pub(crate) review_port: u16,
    pub(crate) site_name: String,
    /// First camera's name — retained for backward-compat with log_startup and
    /// single-camera deployments.
    pub(crate) camera_name: String,
    /// First camera's RTSP URL — retained for backward compat.
    pub(crate) rtsp_url: Option<String>,
    pub(crate) rtsp_username: Option<String>,
    pub(crate) rtsp_password: Option<Secret>,
    pub(crate) detector_model_id: String,
    pub(crate) detector_model_path: Option<PathBuf>,
    pub(crate) detector_confidence_threshold: f64,
    pub(crate) detector_sample_frames: usize,
    pub(crate) detector_stationary_interval_secs: u64,
    /// Canonical multi-camera list.  Always contains at least one entry (the
    /// single camera_name/rtsp_url for backward compat).
    pub(crate) cameras: Vec<CameraEntry>,
    /// Outbound connection an integration adapter may use, present when a
    /// broker is configured (e.g. via HA Supervisor MQTT service or env vars
    /// MQTT_HOST / MQTT_PORT).
    pub(crate) mqtt: Option<ConnectionEndpoint>,
    /// Stable service identifier derived from site_name or explicitly configured
    /// via VIGIL_SERVICE_ID.  Used as the MQTT topic namespace and HA device id.
    pub(crate) service_id: String,
    /// Recognition: crop → embed → enroll → match. Off unless a weights
    /// directory is configured; a configured-but-missing weights dir fails
    /// loud at startup, never silently.
    pub(crate) recognition: crate::recognition::RecognitionConfig,
    /// Acceleration intent: probe and use hardware decode only when a real
    /// startup probe succeeds; missing means true.
    pub(crate) hardware_decoding: bool,
    /// Acceleration intent: probe and use an accelerated detector backend
    /// only when the artifact ships one and its probe succeeds; missing
    /// means true.
    pub(crate) accelerated_detection: bool,
    /// Fabric enrollment ticket (criterion C6/C10). Inert scaffold: present
    /// on the config surface with a sane default (absent) so every shape
    /// shows the knob; not yet wired to any fabric client behavior.
    pub(crate) fabric_ticket: Option<String>,
    /// Whether this node embeds the fabric hub (criterion C10). Sane
    /// default false — no silent new network surface on existing installs.
    /// Inert scaffold: not yet wired to any hub behavior.
    pub(crate) fabric_hub: bool,
    /// Per-source opt-out (default ON) for moving an ephemeral compressed
    /// clip of a motion event to a same-tenant fabric node under queue
    /// pressure (criterion C10 / the addon privacy wording). The owner's
    /// binding promise — offload becomes automatic on join — requires
    /// default movement; this is the documented knob to turn it back off
    /// for one node while keeping fabric enrollment (and claiming OTHER
    /// nodes' work) otherwise unaffected. Only read by the fabric offload
    /// path (`runtime.rs`, behind the `fabric` feature) — a no-feature
    /// build never reads it, hence the blanket allow rather than a
    /// cfg-gated one (the field itself is unconditional, so config
    /// resolution/precedence/CLI/env/addon-surface behavior is identical
    /// across builds).
    #[allow(dead_code)]
    pub(crate) fabric_allow_frame_offload: bool,
    /// Worker lease duration in milliseconds (criterion C10, fix cycle 9).
    /// Inert scaffold: present on the config surface with a sane default
    /// (300000ms = 5 minutes, today's hardcoded `fabric.rs`
    /// `lease_duration_ms: 5 * 60_000` literal) so every shape shows the
    /// knob; not yet wired to `WorkerConfig.lease_duration_ms` construction.
    /// Precedent: `fabric_ticket`/`fabric_hub` above (705b1ac).
    #[allow(dead_code)]
    pub(crate) fabric_worker_lease_ms: u64,
    /// Offload fallback horizon in milliseconds (criterion C10, fix cycle
    /// 9). Inert scaffold: sane default (5000ms, today's hardcoded
    /// `OffloadPolicyConfig::default()` value) so every shape shows the
    /// knob; not yet wired to `OffloadPolicyConfig.fallback_horizon_ms`
    /// construction. Precedent: `fabric_ticket`/`fabric_hub` above
    /// (705b1ac).
    #[allow(dead_code)]
    pub(crate) fabric_fallback_horizon_ms: u64,
}

/// Per-camera entry as it appears in TOML/JSON config files.
#[derive(Debug, Clone, Default, Deserialize)]
struct CameraEntryPartial {
    name: String,
    rtsp_url: Option<String>,
    live_rtsp_url: Option<String>,
    username: Option<String>,
    password: Option<Secret>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialConfig {
    data_dir: Option<PathBuf>,
    store_path: Option<PathBuf>,
    health_port: Option<u16>,
    review_port: Option<u16>,
    site_name: Option<String>,
    camera_name: Option<String>,
    rtsp_url: Option<String>,
    live_rtsp_url: Option<String>,
    rtsp_username: Option<String>,
    rtsp_password: Option<Secret>,
    detector_model_id: Option<String>,
    detector_model_path: Option<PathBuf>,
    detector_confidence_threshold: Option<f64>,
    detector_sample_frames: Option<usize>,
    detector_stationary_interval_secs: Option<u64>,
    /// Multi-camera list.  When present, supersedes camera_name/rtsp_url.
    cameras: Option<Vec<CameraEntryPartial>>,
    // MQTT broker — provided by HA Supervisor or env vars when broker is configured.
    mqtt_host: Option<String>,
    mqtt_port: Option<u16>,
    mqtt_username: Option<String>,
    mqtt_password: Option<Secret>,
    service_id: Option<String>,
    recognition_weights_dir: Option<PathBuf>,
    recognition_space_id: Option<String>,
    recognition_threshold: Option<f64>,
    recognition_covered_classes: Option<Vec<String>>,
    hardware_decoding: Option<bool>,
    accelerated_detection: Option<bool>,
    fabric_ticket: Option<String>,
    fabric_hub: Option<bool>,
    fabric_allow_frame_offload: Option<bool>,
    fabric_worker_lease_ms: Option<u64>,
    fabric_fallback_horizon_ms: Option<u64>,
}

#[derive(Debug, Default)]
struct CliOverrides {
    config_path: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    store_path: Option<PathBuf>,
    health_port: Option<u16>,
    review_port: Option<u16>,
    site_name: Option<String>,
    camera_name: Option<String>,
    rtsp_url: Option<String>,
    live_rtsp_url: Option<String>,
    rtsp_username: Option<String>,
    rtsp_password: Option<Secret>,
    detector_model_id: Option<String>,
    detector_model_path: Option<PathBuf>,
    detector_confidence_threshold: Option<f64>,
    detector_sample_frames: Option<usize>,
    detector_stationary_interval_secs: Option<u64>,
    recognition_weights_dir: Option<PathBuf>,
    hardware_decoding: Option<bool>,
    accelerated_detection: Option<bool>,
    fabric_ticket: Option<String>,
    fabric_hub: Option<bool>,
    fabric_allow_frame_offload: Option<bool>,
    fabric_worker_lease_ms: Option<u64>,
    fabric_fallback_horizon_ms: Option<u64>,
}

/// `<data_root>/fabric.toml` — the fabric enrollment file surface (criterion
/// C6/C10), mirroring cg's `fabric.toml` file convention (a plain TOML file
/// under the data root, read on every start). vigil's own knob vocabulary
/// (`fabric_ticket`/`fabric_hub`, the SAME names the HAOS add-on options
/// and env vars use) is kept rather than cg's `hub_endpoint`/`tenant` key
/// names: vigil's tenant is a fixed, non-operator-facing value by design
/// (`fabric.rs`'s `FABRIC_TENANT`), and a second key vocabulary for the
/// same two knobs would violate the one-vocabulary obligation (C7/C10)
/// this same run is chartered to uphold.
#[derive(Debug, Default, Deserialize)]
struct FabricFileConfig {
    fabric_ticket: Option<String>,
    fabric_hub: Option<bool>,
}

fn read_fabric_toml(data_dir: &Path) -> FabricFileConfig {
    let path = data_dir.join("fabric.toml");
    let Ok(text) = fs::read_to_string(&path) else {
        return FabricFileConfig::default();
    };
    toml::from_str(&text).unwrap_or_else(|error| {
        println!(
            "fabric_toml_parse_failed=true path={} error={error}",
            path.display()
        );
        FabricFileConfig::default()
    })
}

pub(crate) fn load(args: Vec<OsString>) -> Result<RuntimeConfig, String> {
    let cli = parse_cli(args)?;
    let mut partial = PartialConfig::default();

    if let Some(path) = cli.config_path.as_ref() {
        merge(&mut partial, read_toml_config(path)?);
    } else {
        let options_path = default_options_json_path();
        if options_path.exists() {
            merge(&mut partial, read_options_json(&options_path)?);
        }
    }

    merge(
        &mut partial,
        PartialConfig {
            data_dir: cli.data_dir,
            store_path: cli.store_path,
            health_port: cli.health_port,
            review_port: cli.review_port,
            site_name: cli.site_name,
            camera_name: cli.camera_name,
            rtsp_url: cli.rtsp_url,
            live_rtsp_url: cli.live_rtsp_url,
            rtsp_username: cli.rtsp_username,
            rtsp_password: cli.rtsp_password,
            detector_model_id: cli.detector_model_id,
            detector_model_path: cli.detector_model_path,
            detector_confidence_threshold: cli.detector_confidence_threshold,
            detector_sample_frames: cli.detector_sample_frames,
            detector_stationary_interval_secs: cli.detector_stationary_interval_secs,
            // Multi-camera list not exposed as CLI flags; comes from config file or options.json.
            cameras: None,
            // MQTT fields are not exposed as CLI flags; they come from env vars or options.json.
            mqtt_host: None,
            mqtt_port: None,
            mqtt_username: None,
            mqtt_password: None,
            service_id: None,
            recognition_weights_dir: cli.recognition_weights_dir,
            recognition_space_id: None,
            recognition_threshold: None,
            recognition_covered_classes: None,
            hardware_decoding: cli.hardware_decoding,
            accelerated_detection: cli.accelerated_detection,
            fabric_worker_lease_ms: cli.fabric_worker_lease_ms,
            fabric_fallback_horizon_ms: cli.fabric_fallback_horizon_ms,
            // Fabric enrollment knobs are deliberately left OUT of this
            // merge (unlike every other CLI flag above): their precedence
            // is CLI > env > options.json/fabric.toml, the REVERSE of this
            // merge's CLI-before-env order — applied explicitly, below,
            // once `data_dir` (needed for fabric.toml) is resolved.
            fabric_ticket: None,
            fabric_hub: None,
            // The per-source offload opt-out follows the SAME precedence as
            // hardware_decoding/accelerated_detection above (CLI here, env
            // last) — it is not a fabric.toml-eligible knob.
            fabric_allow_frame_offload: cli.fabric_allow_frame_offload,
        },
    );
    merge(&mut partial, env_overrides()?);

    // If MQTT_HOST was not supplied via options.json or env, attempt Supervisor services API.
    // Only called when SUPERVISOR_TOKEN is present (i.e. running as an HA add-on).
    if partial.mqtt_host.is_none()
        && let Some(cfg) = crate::supervisor::fetch_supervisor_mqtt()
    {
        partial.mqtt_host = Some(cfg.host);
        // Only override port/creds if the env didn't provide them explicitly.
        if partial.mqtt_port.is_none() {
            partial.mqtt_port = Some(cfg.port);
        }
        if partial.mqtt_username.is_none() {
            partial.mqtt_username = cfg.username;
        }
        if partial.mqtt_password.is_none() {
            partial.mqtt_password = cfg.password;
        }
    }

    let data_dir = partial.data_dir.unwrap_or_else(default_data_dir);
    let store_path = partial
        .store_path
        .unwrap_or_else(|| data_dir.join("store.contextgraph"));
    let health_port = partial.health_port.unwrap_or(8099);
    let review_port = partial.review_port.unwrap_or(8098);
    let site_name = partial.site_name.unwrap_or_else(|| "site-1".to_string());
    let camera_name = partial
        .camera_name
        .unwrap_or_else(|| "camera-1".to_string());
    let detector_model_id = partial
        .detector_model_id
        .unwrap_or_else(|| "yolox-tiny-burn-cpu".to_string());
    let detector_confidence_threshold =
        validate_confidence_threshold(partial.detector_confidence_threshold.unwrap_or(0.5))?;
    let detector_sample_frames = validate_detector_sample_frames(
        partial.detector_sample_frames.unwrap_or(5),
        "detector_sample_frames",
    )?;
    // Resolved through the typed settings registry: an operator-supplied
    // value (from any source already merged into `partial` above) is a
    // manual pin; nothing supplied leaves it Automatic at its declared
    // default. This is the one knob moved behind the registry so its
    // control states are real; the effective value is unchanged.
    // `declare_settings` is the single function that declares every
    // setting this crate has (see its own doc comment) — `load` and the
    // coverage check both go through it, so they can never disagree about
    // which settings exist.
    let mut settings_registry = SettingsRegistry::new();
    let declared_settings = declare_settings(&mut settings_registry);
    if let Some(value) = partial.detector_stationary_interval_secs {
        declared_settings
            .stationary_interval
            .set_manual(value)
            .map_err(|error| format!("detector_stationary_interval_secs {error}"))?;
    }
    let detector_stationary_interval_secs = declared_settings.stationary_interval.effective_value();
    // Fabric knobs: absent ticket, hub embedding defaults off (criterion
    // C10 — every knob has a sane default, works with nothing provided).
    // Precedence CLI > env > options.json/fabric.toml — `partial.fabric_*`
    // at this point already reflects env-over-(config-file/options.json)
    // from the merges above; fabric.toml is consulted as one more
    // lowest-priority source (data_dir-relative, so only readable once
    // `data_dir` itself is resolved, just above), and the CLI flag —
    // deliberately excluded from the earlier CLI merge — is applied last.
    let fabric_toml = read_fabric_toml(&data_dir);
    // An empty ticket string from ANY source (a blank HAOS options.json
    // field mapped to `VIGIL_FABRIC_TICKET=""`, an empty fabric.toml key)
    // means the operator never configured a ticket — normalize it to `None`
    // here so it is treated identically to an untouched field, never as an
    // operator-typed malformed value that earns a "ticket rejected" error.
    // A non-empty but malformed ticket still passes through and is rejected
    // loudly at enrollment (criterion C6).
    let fabric_ticket = cli
        .fabric_ticket
        .or(partial.fabric_ticket)
        .or(fabric_toml.fabric_ticket)
        .map(|ticket| ticket.trim().to_string())
        .filter(|ticket| !ticket.is_empty());
    let fabric_hub = cli
        .fabric_hub
        .or(partial.fabric_hub)
        .or(fabric_toml.fabric_hub)
        .unwrap_or(false);
    // Default ON: the owner's binding promise (automatic offload on join)
    // requires default movement; this is the documented per-source opt-out
    // (criterion C10 / the addon privacy wording).
    let fabric_allow_frame_offload = partial.fabric_allow_frame_offload.unwrap_or(true);
    // Fabric tuning knobs (criterion C10, fix cycle 9): sane defaults match
    // today's hardcoded literals byte-identical (fabric.rs's
    // `lease_duration_ms: 5 * 60_000` and `OffloadPolicyConfig::default()`'s
    // `fallback_horizon_ms: 5_000`) — inert scaffold, neither is consumed
    // yet.
    let fabric_worker_lease_ms = partial.fabric_worker_lease_ms.unwrap_or(300_000);
    let fabric_fallback_horizon_ms = partial.fabric_fallback_horizon_ms.unwrap_or(5_000);

    // Outbound connection: present when a host is configured. The password
    // stays wrapped in `Secret` all the way out of config resolution — an
    // integration adapter is the one that ultimately exposes it, at the
    // point it actually opens a connection.
    let mqtt = partial.mqtt_host.map(|host| ConnectionEndpoint {
        host,
        port: partial.mqtt_port.unwrap_or(1883),
        username: partial.mqtt_username,
        password: partial.mqtt_password,
    });

    // Stable service identifier — explicit override or derived from site_name.
    let service_id = partial
        .service_id
        .unwrap_or_else(|| config_slug(&site_name));

    // Build the canonical multi-camera list.
    // If a `cameras` list is provided in config/JSON it supersedes the single
    // camera_name / rtsp_url fields.  Otherwise synthesize a one-element list
    // from the single-camera fields (backward compat).
    // An empty cameras list would run a Ready, zero-camera runtime that
    // looks healthy while doing nothing: treat it as absent so the
    // single-camera synthesis (and its loud missing-URL behavior) applies.
    let cameras: Vec<CameraEntry> =
        if let Some(cam_list) = partial.cameras.filter(|list| !list.is_empty()) {
            cam_list
                .into_iter()
                .map(|c| CameraEntry {
                    name: c.name,
                    rtsp_url: c.rtsp_url,
                    live_rtsp_url: c.live_rtsp_url,
                    username: c.username,
                    password: c.password,
                })
                .collect()
        } else {
            vec![CameraEntry {
                name: camera_name.clone(),
                rtsp_url: partial.rtsp_url.clone(),
                live_rtsp_url: partial.live_rtsp_url.clone(),
                username: partial.rtsp_username.clone(),
                password: partial.rtsp_password.clone(),
            }]
        };

    // Recognition switches on when a weights directory is configured.
    let mut recognition = crate::recognition::RecognitionConfig::default();
    if let Some(weights_dir) = partial.recognition_weights_dir {
        recognition.enabled = true;
        recognition.weights_dir = Some(weights_dir);
    }
    if let Some(space) = partial.recognition_space_id {
        recognition.embedding_space_id = space;
    }
    if let Some(threshold) = partial.recognition_threshold {
        recognition.match_threshold = validate_recognition_threshold(threshold)?;
    }
    if let Some(classes) = partial.recognition_covered_classes {
        recognition.covered_classes = classes;
    }

    Ok(RuntimeConfig {
        data_dir,
        store_path,
        health_port,
        review_port,
        site_name,
        camera_name,
        rtsp_url: partial.rtsp_url,
        rtsp_username: partial.rtsp_username,
        rtsp_password: partial.rtsp_password,
        detector_model_id,
        detector_model_path: partial.detector_model_path,
        detector_confidence_threshold,
        detector_sample_frames,
        detector_stationary_interval_secs,
        cameras,
        mqtt,
        service_id,
        recognition,
        // Intent booleans: absent means true (probe, use only on a
        // passed probe, fall back visibly).
        hardware_decoding: partial.hardware_decoding.unwrap_or(true),
        accelerated_detection: partial.accelerated_detection.unwrap_or(true),
        fabric_ticket,
        fabric_hub,
        fabric_allow_frame_offload,
        fabric_worker_lease_ms,
        fabric_fallback_horizon_ms,
    })
}

fn parse_cli(args: Vec<OsString>) -> Result<CliOverrides, String> {
    let mut cli = CliOverrides::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let arg = arg
            .into_string()
            .map_err(|_| "run arguments must be valid UTF-8".to_string())?;
        match arg.as_str() {
            "--config" => cli.config_path = Some(next_path(&mut iter, "--config")?),
            "--data-dir" => cli.data_dir = Some(next_path(&mut iter, "--data-dir")?),
            "--store-path" => cli.store_path = Some(next_path(&mut iter, "--store-path")?),
            "--recognition-weights-dir" => {
                cli.recognition_weights_dir =
                    Some(next_path(&mut iter, "--recognition-weights-dir")?)
            }
            "--health-port" => cli.health_port = Some(next_port(&mut iter, "--health-port")?),
            "--review-port" => cli.review_port = Some(next_port(&mut iter, "--review-port")?),
            "--site-name" => cli.site_name = Some(next_string(&mut iter, "--site-name")?),
            "--camera-name" => cli.camera_name = Some(next_string(&mut iter, "--camera-name")?),
            "--rtsp-url" => cli.rtsp_url = Some(next_string(&mut iter, "--rtsp-url")?),
            "--live-rtsp-url" => {
                cli.live_rtsp_url = Some(next_string(&mut iter, "--live-rtsp-url")?)
            }
            "--rtsp-username" => {
                cli.rtsp_username = Some(next_string(&mut iter, "--rtsp-username")?)
            }
            "--rtsp-password" => {
                cli.rtsp_password = Some(Secret::new(next_string(&mut iter, "--rtsp-password")?))
            }
            "--detector-model-id" => {
                cli.detector_model_id = Some(next_string(&mut iter, "--detector-model-id")?)
            }
            "--detector-model-path" => {
                cli.detector_model_path = Some(next_path(&mut iter, "--detector-model-path")?)
            }
            "--detector-confidence-threshold" => {
                let value = next_string(&mut iter, "--detector-confidence-threshold")?;
                cli.detector_confidence_threshold = Some(value.parse().map_err(|error| {
                    format!("--detector-confidence-threshold must be a number: {error}")
                })?);
            }
            "--detector-sample-frames" => {
                let value = next_string(&mut iter, "--detector-sample-frames")?;
                cli.detector_sample_frames = Some(value.parse().map_err(|error| {
                    format!("--detector-sample-frames must be an integer: {error}")
                })?);
            }
            "--detector-stationary-interval-secs" => {
                let value = next_string(&mut iter, "--detector-stationary-interval-secs")?;
                cli.detector_stationary_interval_secs = Some(value.parse().map_err(|error| {
                    format!("--detector-stationary-interval-secs must be an integer: {error}")
                })?);
            }
            "--hardware-decoding" => {
                cli.hardware_decoding = Some(next_bool(&mut iter, "--hardware-decoding")?)
            }
            "--accelerated-detection" => {
                cli.accelerated_detection = Some(next_bool(&mut iter, "--accelerated-detection")?)
            }
            "--fabric-ticket" => {
                cli.fabric_ticket = Some(next_string(&mut iter, "--fabric-ticket")?)
            }
            "--fabric-hub" => cli.fabric_hub = Some(next_bool(&mut iter, "--fabric-hub")?),
            "--fabric-allow-frame-offload" => {
                cli.fabric_allow_frame_offload =
                    Some(next_bool(&mut iter, "--fabric-allow-frame-offload")?)
            }
            "--fabric-worker-lease-ms" => {
                let value = next_string(&mut iter, "--fabric-worker-lease-ms")?;
                cli.fabric_worker_lease_ms = Some(value.parse().map_err(|error| {
                    format!("--fabric-worker-lease-ms must be an integer: {error}")
                })?);
            }
            "--fabric-fallback-horizon-ms" => {
                let value = next_string(&mut iter, "--fabric-fallback-horizon-ms")?;
                cli.fabric_fallback_horizon_ms = Some(value.parse().map_err(|error| {
                    format!("--fabric-fallback-horizon-ms must be an integer: {error}")
                })?);
            }
            "--help" | "-h" => return Err(run_usage()),
            other => return Err(format!("{other} is not a supported run option")),
        }
    }
    Ok(cli)
}

fn next_string(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<String, String> {
    iter.next()
        .ok_or_else(|| format!("{flag} requires a value"))
        .and_then(|value| {
            value
                .into_string()
                .map_err(|_| format!("{flag} value must be valid UTF-8"))
        })
}

fn next_path(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<PathBuf, String> {
    iter.next()
        .ok_or_else(|| format!("{flag} requires a value"))
        .and_then(|value| {
            value
                .into_string()
                .map(PathBuf::from)
                .map_err(|_| format!("{flag} value must be valid UTF-8"))
        })
}

fn next_port(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<u16, String> {
    let value = next_path(iter, flag)?;
    value
        .to_string_lossy()
        .parse::<u16>()
        .map_err(|error| format!("{flag} must be a TCP port: {error}"))
}

fn next_bool(iter: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<bool, String> {
    let value = next_string(iter, flag)?;
    parse_intent_bool(&value).ok_or_else(|| format!("{flag} must be true or false, got {value}"))
}

/// Intent booleans accept exactly true/false (case-insensitive).
fn parse_intent_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

// This wrapper's own callers are baselined legacy reads (see
// `env_overrides` below), enumerated in
// `environment_read_surface.baseline.txt`, pending migration to the
// settings registry — not a declared setting today.
#[allow(clippy::disallowed_methods)]
fn env_intent_bool(name: &str) -> Result<Option<bool>, String> {
    match std::env::var(name) {
        Ok(value) => parse_intent_bool(&value)
            .map(Some)
            .ok_or_else(|| format!("{name} must be true or false, got {value}")),
        Err(_) => Ok(None),
    }
}

fn validate_confidence_threshold(value: f64) -> Result<f64, String> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "detector_confidence_threshold must be between 0.0 and 1.0, got {value}"
        ))
    }
}

fn validate_recognition_threshold(value: f64) -> Result<f64, String> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "recognition_threshold must be between 0.0 and 1.0, got {value}"
        ))
    }
}

fn validate_detector_sample_frames(value: usize, name: &str) -> Result<usize, String> {
    if (1..=64).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{name} must be between 1 and 64, got {value}"))
    }
}

/// Every setting this crate declares through the typed settings registry,
/// handed back to whoever declared them.
pub(crate) struct DeclaredSettings {
    pub(crate) stationary_interval: SettingHandle<u64>,
}

/// The single production function that declares every setting this crate
/// has. `load` (to resolve real values) and `declared_settings` at the
/// crate root (to check coverage) both call this and nothing else
/// constructs a config setting's handle, so the two can never disagree
/// about which settings exist: adding a setting means adding one field to
/// [`DeclaredSettings`] and one `registry.declare` call here, and both
/// callers pick it up without further edits.
///
/// This is a structural guarantee, enforced two ways, not a discipline
/// one:
/// - Outside this crate, it is a compile error: [`SettingsRegistry::new`]
///   and [`SettingsRegistry::declare`] are both `pub(crate)`, so no
///   external caller can build a registry at all, let alone declare a
///   setting on one (see the doctest on [`SettingsRegistry`] itself).
/// - Inside this crate, a `declare` call added to any function other than
///   this one is a gate failure: `settings_registry_declare_boundary.rs`
///   scans the crate's own source and fails the moment `declare(` appears
///   anywhere outside this function's body (its own unit tests in
///   `settings.rs` are the sole, reviewed exception, since they exist to
///   test the registry primitive itself, not to resolve a real setting).
///
/// [`SettingHandle::new`] is private to the `settings` module, so the only
/// way to obtain one — including the one this function returns — is
/// [`SettingsRegistry::declare`], which always records the setting on the
/// registry's coverage entries first. A handle that reaches
/// [`DeclaredSettings`] without having gone through that call cannot exist.
pub(crate) fn declare_settings(registry: &mut SettingsRegistry) -> DeclaredSettings {
    DeclaredSettings {
        stationary_interval: registry.declare(stationary_interval_setting_spec()),
    }
}

/// The stationary scan interval's settings-registry declaration: a stable
/// name, its today-unchanged default and validation, and where an owner
/// sees and changes it. Unconstrained, matching the resolution this
/// replaces (any interval was previously accepted).
fn stationary_interval_setting_spec() -> SettingSpec<u64> {
    SettingSpec {
        name: "detector_stationary_interval_secs",
        default: 30,
        validate: |_value| Ok(()),
        surfaces: SettingSurfaces {
            config_key: "detector_stationary_interval_secs",
            addon_option_key: "detector_stationary_interval_secs",
            documentation_page: "docs/configuration.md",
        },
    }
}

/// Whether a standalone-config TOML fragment assigning `raw_value` to
/// `name` actually sets the field it names, using the real config-file
/// parser (`PartialConfig`). Test/tooling support for the settings-registry
/// coverage check: parsing a fragment and confirming it lands on the right
/// field cannot be done generically over an arbitrary setting name, so this
/// dispatches by name, one declared setting at a time, and stays honest by
/// refusing to guess for a name it does not recognize.
pub(crate) fn config_file_fragment_sets(name: &str, raw_value: &str) -> bool {
    let fragment = format!("{name} = {raw_value}");
    let Ok(parsed) = toml::from_str::<PartialConfig>(&fragment) else {
        return false;
    };
    match name {
        "detector_stationary_interval_secs" => {
            parsed.detector_stationary_interval_secs == raw_value.parse().ok()
        }
        _ => false,
    }
}

fn read_toml_config(path: &Path) -> Result<PartialConfig, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read config {}: {error}", path.display()))?;
    toml::from_str(&text)
        .map_err(|error| format!("could not parse config {}: {error}", path.display()))
}

/// Add-on start path for the two startup-probe deadline knobs. A Home
/// Assistant user sets `decode_probe_deadline_secs` /
/// `detection_probe_deadline_secs` as add-on options; this writes them to the
/// `VIGIL_DECODE_PROBE_DEADLINE_SECS` / `VIGIL_DETECTION_PROBE_DEADLINE_SECS`
/// environment variables the decode and detection startup probes read, so the
/// option value wins over a manually set environment variable and an unset
/// option keeps the source default. The add-on's entry point is the vigil
/// binary itself (it reads `/data/options.json`), so this export lives here
/// instead of a separate shell run script; it is a no-op when no options file
/// is present (a non-add-on deployment).
pub(crate) fn apply_addon_probe_deadline_env() {
    let options_path = default_options_json_path();
    if !options_path.exists() {
        return;
    }
    let Ok(text) = fs::read_to_string(&options_path) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    for (option, env_var) in [
        (
            "decode_probe_deadline_secs",
            "VIGIL_DECODE_PROBE_DEADLINE_SECS",
        ),
        (
            "detection_probe_deadline_secs",
            "VIGIL_DETECTION_PROBE_DEADLINE_SECS",
        ),
    ] {
        if let Some(secs) = value.get(option).and_then(option_deadline_secs_string) {
            // SAFETY: called once at process start (top of `run_cli`), before
            // any camera or acceleration-probe thread is spawned, so no
            // concurrent env access races this write.
            unsafe {
                std::env::set_var(env_var, secs);
            }
        }
    }
}

fn option_deadline_secs_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::String(text) => {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_string())
        }
        _ => None,
    }
}

fn read_options_json(path: &Path) -> Result<PartialConfig, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read options {}: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("could not parse options {}: {error}", path.display()))
}

// Every read below is a baselined legacy environment read (an
// add-on/Supervisor-injected value), enumerated in
// `environment_read_surface.baseline.txt` and pending migration to the
// settings registry — only the stationary scan interval is a declared
// setting today (see `stationary_interval_setting_spec`); this is the one
// function that gathers the rest, not an ad-hoc scattering.
#[allow(clippy::disallowed_methods)]
fn env_overrides() -> Result<PartialConfig, String> {
    Ok(PartialConfig {
        data_dir: std::env::var_os("VIGIL_DATA_DIR").map(PathBuf::from),
        store_path: std::env::var_os("VIGIL_STORE_PATH").map(PathBuf::from),
        health_port: match std::env::var("VIGIL_HEALTH_PORT") {
            Ok(value) => Some(
                value
                    .parse::<u16>()
                    .map_err(|error| format!("VIGIL_HEALTH_PORT must be a TCP port: {error}"))?,
            ),
            Err(_) => None,
        },
        review_port: match std::env::var("VIGIL_REVIEW_PORT") {
            Ok(value) => Some(
                value
                    .parse::<u16>()
                    .map_err(|error| format!("VIGIL_REVIEW_PORT must be a TCP port: {error}"))?,
            ),
            Err(_) => None,
        },
        site_name: std::env::var("VIGIL_SITE_NAME").ok(),
        camera_name: std::env::var("VIGIL_CAMERA_NAME").ok(),
        rtsp_url: std::env::var("VIGIL_RTSP_URL").ok(),
        live_rtsp_url: std::env::var("VIGIL_LIVE_RTSP_URL").ok(),
        rtsp_username: std::env::var("VIGIL_RTSP_USERNAME").ok(),
        rtsp_password: std::env::var("VIGIL_RTSP_PASSWORD").ok().map(Secret::new),
        detector_model_id: std::env::var("VIGIL_DETECTOR_MODEL_ID").ok(),
        detector_model_path: std::env::var_os("VIGIL_DETECTOR_MODEL_PATH").map(Into::into),
        recognition_weights_dir: std::env::var_os("VIGIL_RECOGNITION_WEIGHTS_DIR").map(Into::into),
        recognition_space_id: std::env::var("VIGIL_RECOGNITION_SPACE_ID").ok(),
        recognition_threshold: match std::env::var("VIGIL_RECOGNITION_THRESHOLD") {
            Ok(value) => Some(value.parse::<f64>().map_err(|error| {
                format!("VIGIL_RECOGNITION_THRESHOLD must be a number: {error}")
            })?),
            Err(_) => None,
        },
        recognition_covered_classes: None,
        detector_confidence_threshold: match std::env::var("VIGIL_DETECTOR_CONFIDENCE_THRESHOLD") {
            Ok(value) => Some(value.parse::<f64>().map_err(|error| {
                format!("VIGIL_DETECTOR_CONFIDENCE_THRESHOLD must be a number: {error}")
            })?),
            Err(_) => None,
        },
        detector_sample_frames: match std::env::var("VIGIL_DETECTOR_SAMPLE_FRAMES") {
            Ok(value) => Some(validate_detector_sample_frames(
                value.parse::<usize>().map_err(|error| {
                    format!("VIGIL_DETECTOR_SAMPLE_FRAMES must be an integer: {error}")
                })?,
                "VIGIL_DETECTOR_SAMPLE_FRAMES",
            )?),
            Err(_) => None,
        },
        detector_stationary_interval_secs: match std::env::var(
            "VIGIL_DETECTOR_STATIONARY_INTERVAL_SECS",
        ) {
            Ok(value) => Some(value.parse::<u64>().map_err(|error| {
                format!("VIGIL_DETECTOR_STATIONARY_INTERVAL_SECS must be an integer: {error}")
            })?),
            Err(_) => None,
        },
        // MQTT credentials — HA Supervisor injects these via env when `services: [mqtt:want]`
        // is declared in the add-on config.yaml.
        mqtt_host: std::env::var("MQTT_HOST").ok(),
        mqtt_port: match std::env::var("MQTT_PORT") {
            Ok(value) => Some(
                value
                    .parse::<u16>()
                    .map_err(|error| format!("MQTT_PORT must be a TCP port: {error}"))?,
            ),
            Err(_) => None,
        },
        mqtt_username: std::env::var("MQTT_USER")
            .ok()
            .or_else(|| std::env::var("MQTT_USERNAME").ok()),
        mqtt_password: std::env::var("MQTT_PASSWORD").ok().map(Secret::new),
        service_id: std::env::var("VIGIL_SERVICE_ID").ok(),
        hardware_decoding: env_intent_bool("VIGIL_HARDWARE_DECODING")?,
        accelerated_detection: env_intent_bool("VIGIL_ACCELERATED_DETECTION")?,
        fabric_ticket: std::env::var("VIGIL_FABRIC_TICKET").ok(),
        fabric_hub: env_intent_bool("VIGIL_FABRIC_HUB")?,
        fabric_allow_frame_offload: env_intent_bool("VIGIL_FABRIC_ALLOW_FRAME_OFFLOAD")?,
        fabric_worker_lease_ms: match std::env::var("VIGIL_FABRIC_WORKER_LEASE_MS") {
            Ok(value) => Some(value.parse::<u64>().map_err(|error| {
                format!("VIGIL_FABRIC_WORKER_LEASE_MS must be an integer: {error}")
            })?),
            Err(_) => None,
        },
        fabric_fallback_horizon_ms: match std::env::var("VIGIL_FABRIC_FALLBACK_HORIZON_MS") {
            Ok(value) => Some(value.parse::<u64>().map_err(|error| {
                format!("VIGIL_FABRIC_FALLBACK_HORIZON_MS must be an integer: {error}")
            })?),
            Err(_) => None,
        },
        // Multi-camera list is not configurable via env vars; comes from config file only.
        cameras: None,
    })
}

fn merge(target: &mut PartialConfig, source: PartialConfig) {
    if source.data_dir.is_some() {
        target.data_dir = source.data_dir;
    }
    if source.store_path.is_some() {
        target.store_path = source.store_path;
    }
    if source.health_port.is_some() {
        target.health_port = source.health_port;
    }
    if source.review_port.is_some() {
        target.review_port = source.review_port;
    }
    if source.site_name.is_some() {
        target.site_name = source.site_name;
    }
    if source.camera_name.is_some() {
        target.camera_name = source.camera_name;
    }
    if source.rtsp_url.is_some() {
        target.rtsp_url = source.rtsp_url;
    }
    if source.live_rtsp_url.is_some() {
        target.live_rtsp_url = source.live_rtsp_url;
    }
    if source.rtsp_username.is_some() {
        target.rtsp_username = source.rtsp_username;
    }
    if source.rtsp_password.is_some() {
        target.rtsp_password = source.rtsp_password;
    }
    if source.detector_model_id.is_some() {
        target.detector_model_id = source.detector_model_id;
    }
    if source.detector_model_path.is_some() {
        target.detector_model_path = source.detector_model_path;
    }
    if source.detector_confidence_threshold.is_some() {
        target.detector_confidence_threshold = source.detector_confidence_threshold;
    }
    if source.recognition_weights_dir.is_some() {
        target.recognition_weights_dir = source.recognition_weights_dir;
    }
    if source.recognition_space_id.is_some() {
        target.recognition_space_id = source.recognition_space_id;
    }
    if source.recognition_threshold.is_some() {
        target.recognition_threshold = source.recognition_threshold;
    }
    if source.recognition_covered_classes.is_some() {
        target.recognition_covered_classes = source.recognition_covered_classes;
    }
    if source.detector_sample_frames.is_some() {
        target.detector_sample_frames = source.detector_sample_frames;
    }
    if source.detector_stationary_interval_secs.is_some() {
        target.detector_stationary_interval_secs = source.detector_stationary_interval_secs;
    }
    if source.mqtt_host.is_some() {
        target.mqtt_host = source.mqtt_host;
    }
    if source.mqtt_port.is_some() {
        target.mqtt_port = source.mqtt_port;
    }
    if source.mqtt_username.is_some() {
        target.mqtt_username = source.mqtt_username;
    }
    if source.mqtt_password.is_some() {
        target.mqtt_password = source.mqtt_password;
    }
    if source.cameras.is_some() {
        target.cameras = source.cameras;
    }
    if source.service_id.is_some() {
        target.service_id = source.service_id;
    }
    if source.hardware_decoding.is_some() {
        target.hardware_decoding = source.hardware_decoding;
    }
    if source.accelerated_detection.is_some() {
        target.accelerated_detection = source.accelerated_detection;
    }
    if source.fabric_ticket.is_some() {
        target.fabric_ticket = source.fabric_ticket;
    }
    if source.fabric_hub.is_some() {
        target.fabric_hub = source.fabric_hub;
    }
    if source.fabric_allow_frame_offload.is_some() {
        target.fabric_allow_frame_offload = source.fabric_allow_frame_offload;
    }
    if source.fabric_worker_lease_ms.is_some() {
        target.fabric_worker_lease_ms = source.fabric_worker_lease_ms;
    }
    if source.fabric_fallback_horizon_ms.is_some() {
        target.fabric_fallback_horizon_ms = source.fabric_fallback_horizon_ms;
    }
}

/// Derive a stable lowercase slug from a human-readable string.
/// Used to turn site_name into a service_id when no explicit id is configured.
fn config_slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn default_data_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("vigil-data")
}

fn default_options_json_path() -> PathBuf {
    // Test-only escape hatch so the suite can point this at a fixture path
    // instead of the real Supervisor-mounted file.
    #[cfg(test)]
    #[allow(clippy::disallowed_methods)]
    if let Some(path) = std::env::var_os("VIGIL_TEST_OPTIONS_JSON") {
        return PathBuf::from(path);
    }

    PathBuf::from("/data/options.json")
}

fn run_usage() -> String {
    "Usage: vigil run [--config PATH] [--data-dir PATH] [--store-path PATH] [--health-port PORT] [--review-port PORT] [--site-name NAME] [--camera-name NAME] [--rtsp-url URL] [--live-rtsp-url URL] [--rtsp-username USER] [--rtsp-password PASSWORD] [--detector-model-id ID] [--detector-model-path PATH] [--detector-confidence-threshold FLOAT] [--detector-sample-frames N] [--detector-stationary-interval-secs N] [--recognition-weights-dir PATH] [--hardware-decoding BOOL] [--accelerated-detection BOOL] [--fabric-ticket TICKET] [--fabric-hub BOOL] [--fabric-allow-frame-offload BOOL] [--fabric-worker-lease-ms MS] [--fabric-fallback-horizon-ms MS]"
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    use super::{load, run_usage};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn review_port_cli_override_is_documented_and_loaded() {
        let _guard = env_lock().lock().expect("env lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let _options_env = EnvVarGuard::set(
            "VIGIL_TEST_OPTIONS_JSON",
            tmp.path().join("absent-options.json"),
        );
        let _clean_env = [
            "VIGIL_DATA_DIR",
            "VIGIL_STORE_PATH",
            "VIGIL_HEALTH_PORT",
            "VIGIL_REVIEW_PORT",
            "VIGIL_SITE_NAME",
            "VIGIL_CAMERA_NAME",
            "VIGIL_RTSP_URL",
            "VIGIL_LIVE_RTSP_URL",
            "VIGIL_RTSP_USERNAME",
            "VIGIL_RTSP_PASSWORD",
            "VIGIL_DETECTOR_MODEL_ID",
            "VIGIL_DETECTOR_MODEL_PATH",
            "VIGIL_RECOGNITION_WEIGHTS_DIR",
            "VIGIL_RECOGNITION_SPACE_ID",
            "VIGIL_RECOGNITION_THRESHOLD",
            "VIGIL_DETECTOR_CONFIDENCE_THRESHOLD",
            "VIGIL_DETECTOR_SAMPLE_FRAMES",
            "VIGIL_DETECTOR_STATIONARY_INTERVAL_SECS",
            "VIGIL_SERVICE_ID",
            "VIGIL_HARDWARE_DECODING",
            "VIGIL_ACCELERATED_DETECTION",
            "VIGIL_FABRIC_TICKET",
            "VIGIL_FABRIC_HUB",
            "VIGIL_FABRIC_ALLOW_FRAME_OFFLOAD",
            "VIGIL_FABRIC_WORKER_LEASE_MS",
            "VIGIL_FABRIC_FALLBACK_HORIZON_MS",
            "MQTT_HOST",
            "MQTT_PORT",
            "MQTT_USER",
            "MQTT_USERNAME",
            "MQTT_PASSWORD",
        ]
        .map(EnvVarGuard::remove);
        let usage = run_usage();
        let cli_doc = include_str!("../../../docs/cli.md");
        let table_start = cli_doc
            .find("Common options:")
            .expect("CLI doc has the Common options heading");
        let table_end = cli_doc
            .find("<!-- vigil-claim: `vigil.docs-cli.run-options-and-current-defaults` -->")
            .expect("CLI doc has the run-options contract marker");
        let run_options_table = &cli_doc[table_start..table_end];
        for line in run_options_table
            .lines()
            .filter(|line| line.starts_with("| `--"))
        {
            let documented = line
                .split('`')
                .nth(1)
                .expect("CLI table row has a backtick-delimited option");
            assert!(
                usage.contains(documented),
                "documented run option {documented} is missing from the real usage surface"
            );
        }
        let config = load(vec![
            OsString::from("--review-port"),
            OsString::from("8765"),
            OsString::from("--detector-stationary-interval-secs"),
            OsString::from("30"),
        ])
        .expect("documented CLI overrides load");
        assert_eq!(config.review_port, 8765);
        assert_eq!(config.detector_stationary_interval_secs, 30);
        let defaults = load(Vec::<OsString>::new()).expect("load documented default run config");
        assert_eq!(
            defaults.data_dir.file_name().and_then(|name| name.to_str()),
            Some("vigil-data")
        );
        assert_eq!(
            defaults.store_path,
            defaults.data_dir.join("store.contextgraph")
        );
        assert!(defaults.rtsp_url.is_none());
        assert!(defaults.rtsp_username.is_none());
        assert!(defaults.rtsp_password.is_none());
        assert!(defaults.detector_model_path.is_none());
        assert!(!defaults.recognition.enabled);
        assert!(defaults.fabric_ticket.is_none());

        let detection_url = load(vec![
            OsString::from("--rtsp-url"),
            OsString::from("rtsp://camera.example/detection"),
        ])
        .expect("detection URL without a separate live URL loads");
        assert!(
            detection_url.cameras[0].live_rtsp_url.is_none(),
            "config must preserve an omitted live URL so runtime can apply the documented detection-URL fallback"
        );

        for documented_default in [
            "| `--config PATH` | Read a TOML configuration file | none |".to_string(),
            "| `--data-dir PATH` | Runtime data root | `./vigil-data` |".to_string(),
            "| `--store-path PATH` | Context Graph store | `<data-dir>/store.contextgraph` |".to_string(),
            format!("| `--health-port PORT` | Health HTTP port | `{}` |", defaults.health_port),
            format!("| `--review-port PORT` | Review HTTP port | `{}` |", defaults.review_port),
            format!("| `--site-name NAME` | Site/context name | `{}` |", defaults.site_name),
            format!("| `--camera-name NAME` | Single-camera name | `{}` |", defaults.camera_name),
            "| `--rtsp-url URL` | Detection stream | none |".to_string(),
            "| `--live-rtsp-url URL` | Separate Home Assistant live stream | detection URL |".to_string(),
            "| `--rtsp-username USER` | RTSP username outside the URL | none |".to_string(),
            "| `--rtsp-password PASSWORD` | RTSP password outside the URL | none |".to_string(),
            format!("| `--detector-model-id ID` | Model identity written to provenance | `{}` |", defaults.detector_model_id),
            "| `--detector-model-path PATH` | Detector weights path | artifact/config dependent |".to_string(),
            format!("| `--detector-confidence-threshold FLOAT` | Keep detections at or above this value | `{}` |", defaults.detector_confidence_threshold),
            format!("| `--detector-sample-frames N` | Frames sampled per segment | `{}` |", defaults.detector_sample_frames),
            format!("| `--detector-stationary-interval-secs N` | Sampling interval for stationary scenes | `{}` |", defaults.detector_stationary_interval_secs),
            "| `--recognition-weights-dir PATH` | Enable recognition with local weights | disabled |".to_string(),
            format!("| `--hardware-decoding BOOL` | Request hardware-decode probing | `{}` |", defaults.hardware_decoding),
            format!("| `--accelerated-detection BOOL` | Request accelerated-detector probing | `{}` |", defaults.accelerated_detection),
            "| `--fabric-ticket TICKET` | Join an existing configured fabric | none |".to_string(),
            format!("| `--fabric-hub BOOL` | Start the node as a fabric join point | `{}` |", defaults.fabric_hub),
            format!("| `--fabric-allow-frame-offload BOOL` | Permit this node's detector work to move | `{}` |", defaults.fabric_allow_frame_offload),
            format!("| `--fabric-worker-lease-ms MS` | Fabric worker lease setting | `{}` |", defaults.fabric_worker_lease_ms),
            format!("| `--fabric-fallback-horizon-ms MS` | Remote-result wait setting | `{}` |", defaults.fabric_fallback_horizon_ms),
        ] {
            assert!(
                run_options_table.contains(&documented_default),
                "CLI defaults table diverged from runtime config: {documented_default}"
            );
        }

        for (args, expected_error) in [
            (
                vec!["--detector-confidence-threshold", "-0.1"],
                "detector_confidence_threshold must be between 0.0 and 1.0",
            ),
            (
                vec!["--detector-sample-frames", "0"],
                "detector_sample_frames must be between 1 and 64",
            ),
            (
                vec!["--hardware-decoding", "sometimes"],
                "--hardware-decoding must be true or false",
            ),
        ] {
            let error = load(args.into_iter().map(OsString::from).collect())
                .expect_err("documented invalid CLI value must fail");
            assert!(
                error.contains(expected_error),
                "invalid CLI value must name its contract: expected {expected_error:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn live_rtsp_url_is_distinct_from_detection_rtsp_url() {
        assert!(run_usage().contains("--live-rtsp-url URL"));
        let config = load(vec![
            OsString::from("--rtsp-url"),
            OsString::from("rtsp://camera/detect"),
            OsString::from("--live-rtsp-url"),
            OsString::from("rtsp://camera/live"),
        ])
        .expect("live RTSP CLI override loads");

        assert_eq!(config.rtsp_url.as_deref(), Some("rtsp://camera/detect"));
        assert_eq!(
            config.cameras[0].rtsp_url.as_deref(),
            Some("rtsp://camera/detect")
        );
        assert_eq!(
            config.cameras[0].live_rtsp_url.as_deref(),
            Some("rtsp://camera/live")
        );
    }

    #[test]
    fn addon_options_json_recognition_fields_enable_runtime_config_and_startup_line() {
        let _guard = env_lock().lock().expect("env lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path().join("data");
        let weights_dir = tmp.path().join("recognition").join("siglip");
        fs::create_dir_all(&weights_dir).expect("create weights dir");
        let options_path = tmp.path().join("options.json");
        fs::write(
            &options_path,
            serde_json::json!({
                "data_dir": data_dir,
                "store_path": tmp.path().join("store.contextgraph"),
                "recognition_weights_dir": weights_dir,
                "recognition_space_id": "vigil_site_vision_smoke",
                "recognition_threshold": 0.73,
                "detector_stationary_interval_secs": 30,
                "recognition_covered_classes": ["person", "dog"]
            })
            .to_string(),
        )
        .expect("write add-on options json");

        let _options_env = EnvVarGuard::set("VIGIL_TEST_OPTIONS_JSON", &options_path);
        let _weights_env = EnvVarGuard::remove("VIGIL_RECOGNITION_WEIGHTS_DIR");
        let _space_env = EnvVarGuard::remove("VIGIL_RECOGNITION_SPACE_ID");
        let _threshold_env = EnvVarGuard::remove("VIGIL_RECOGNITION_THRESHOLD");
        let _stationary_env = EnvVarGuard::remove("VIGIL_DETECTOR_STATIONARY_INTERVAL_SECS");

        let config = load(Vec::<OsString>::new()).expect("load add-on options json");

        assert_eq!(
            crate::recognition::RecognitionConfig::default().match_threshold,
            0.90,
            "recognition's visible default must be the actual matching threshold"
        );

        assert!(
            config.recognition.enabled,
            "recognition_weights_dir in add-on options must enable local recognition"
        );
        assert_eq!(
            config.recognition.weights_dir.as_deref(),
            Some(weights_dir.as_path())
        );
        assert_eq!(
            config.recognition.embedding_space_id,
            "vigil_site_vision_smoke"
        );
        assert_eq!(config.recognition.match_threshold, 0.73);
        assert_eq!(
            config.detector_stationary_interval_secs, 30,
            "detector_stationary_interval_secs in add-on options must allow periodic detector scans on no-motion segments so a visible stationary person is not skipped before recognition"
        );
        assert_eq!(
            config.recognition.covered_classes,
            vec!["person".to_string(), "dog".to_string()],
            "recognition_covered_classes in add-on options must control the detector/recognizer class allowlist from HAOS, not require a Rust change"
        );
        assert_eq!(
            crate::runtime::recognition_enabled_startup_line(&config.recognition),
            "recognition_enabled=true space=vigil_site_vision_smoke threshold=0.73 accuracy_warning=below_recommended_default_0.9",
            "the runtime startup line must expose that local add-on recognition is active"
        );

        fs::write(
            &options_path,
            serde_json::json!({
                "data_dir": data_dir,
                "recognition_threshold": 1.01
            })
            .to_string(),
        )
        .expect("write invalid recognition threshold");
        let error = load(Vec::<OsString>::new()).expect_err("out-of-range threshold must fail");
        assert!(
            error.contains("recognition_threshold must be between 0.0 and 1.0"),
            "invalid recognition threshold must fail with its field and accepted range, got {error}"
        );

        fs::write(
            &options_path,
            serde_json::json!({ "data_dir": data_dir }).to_string(),
        )
        .expect("write config with no stationary override");
        let defaults = load(Vec::<OsString>::new()).expect("load shared deployment defaults");
        assert_eq!(
            defaults.detector_stationary_interval_secs, 30,
            "standalone and add-on configuration must share the 30-second look-anyway default"
        );
    }

    /// The RTSP password can arrive from a CLI flag, a TOML config file, or
    /// an environment variable — this must keep resolving to the identical
    /// effective value it did before the password field was wrapped in
    /// `Secret`, including the existing env-overrides-CLI-overrides-file
    /// precedence.
    #[test]
    fn rtsp_password_resolves_identically_from_cli_file_and_env() {
        let _guard = env_lock().lock().expect("env lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let _options_env = EnvVarGuard::set(
            "VIGIL_TEST_OPTIONS_JSON",
            tmp.path().join("absent-options.json"),
        );
        let _password_env = EnvVarGuard::remove("VIGIL_RTSP_PASSWORD");

        // CLI flag alone.
        let cli_only = load(vec![
            OsString::from("--rtsp-password"),
            OsString::from("secret-from-cli"),
        ])
        .expect("CLI-supplied password loads");
        assert_eq!(
            cli_only
                .rtsp_password
                .as_ref()
                .map(|password| password.expose_secret()),
            Some("secret-from-cli")
        );

        // TOML config file alone.
        let config_path = tmp.path().join("vigil.toml");
        fs::write(&config_path, "rtsp_password = \"secret-from-file\"\n")
            .expect("write TOML config file");
        let file_only = load(vec![
            OsString::from("--config"),
            OsString::from(config_path.clone()),
        ])
        .expect("file-supplied password loads");
        assert_eq!(
            file_only
                .rtsp_password
                .as_ref()
                .map(|password| password.expose_secret()),
            Some("secret-from-file")
        );

        // Environment variable alone.
        {
            let _password_env = EnvVarGuard::set("VIGIL_RTSP_PASSWORD", "secret-from-env");
            let env_only = load(Vec::<OsString>::new()).expect("env-supplied password loads");
            assert_eq!(
                env_only
                    .rtsp_password
                    .as_ref()
                    .map(|password| password.expose_secret()),
                Some("secret-from-env")
            );

            // Env still wins over both file and CLI, exactly as before Secret adoption.
            let env_over_file_and_cli = load(vec![
                OsString::from("--config"),
                OsString::from(config_path),
                OsString::from("--rtsp-password"),
                OsString::from("secret-from-cli"),
            ])
            .expect("all three sources supplied together still load");
            assert_eq!(
                env_over_file_and_cli
                    .rtsp_password
                    .as_ref()
                    .map(|password| password.expose_secret()),
                Some("secret-from-env"),
                "environment must still win over both a config file and a CLI flag"
            );
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        // Test-only fence that reads a variable's prior value so it can be
        // restored on drop; not a product read of an adjustable value.
        #[allow(clippy::disallowed_methods)]
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let guard = Self {
                key,
                previous: std::env::var_os(key),
            };
            unsafe {
                std::env::set_var(key, value);
            }
            guard
        }

        // Test-only fence that reads a variable's prior value so it can be
        // restored on drop; not a product read of an adjustable value.
        #[allow(clippy::disallowed_methods)]
        fn remove(key: &'static str) -> Self {
            let guard = Self {
                key,
                previous: std::env::var_os(key),
            };
            unsafe {
                std::env::remove_var(key);
            }
            guard
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            restore_env(self.key, self.previous.take());
        }
    }

    fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
        unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}
