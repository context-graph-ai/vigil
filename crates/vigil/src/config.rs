use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ha_mqtt_tasks::MqttConfig;

/// One camera entry in the multi-camera list.
#[derive(Debug, Clone)]
pub(crate) struct CameraEntry {
    pub(crate) name: String,
    pub(crate) rtsp_url: Option<String>,
    pub(crate) username: Option<String>,
    pub(crate) password: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeConfig {
    pub(crate) data_dir: PathBuf,
    pub(crate) store_path: PathBuf,
    pub(crate) health_port: u16,
    pub(crate) review_port: Option<u16>,
    pub(crate) site_name: String,
    /// First camera's name — retained for backward-compat with log_startup and
    /// single-camera deployments.
    pub(crate) camera_name: String,
    /// First camera's RTSP URL — retained for backward compat.
    pub(crate) rtsp_url: Option<String>,
    pub(crate) rtsp_username: Option<String>,
    pub(crate) rtsp_password: Option<String>,
    pub(crate) detector_model_id: String,
    pub(crate) detector_model_path: Option<PathBuf>,
    pub(crate) detector_confidence_threshold: f64,
    pub(crate) detector_sample_frames: usize,
    /// Canonical multi-camera list.  Always contains at least one entry (the
    /// single camera_name/rtsp_url for backward compat).
    pub(crate) cameras: Vec<CameraEntry>,
    /// MQTT broker connection, present when a broker is configured (e.g. via
    /// HA Supervisor MQTT service or env vars MQTT_HOST / MQTT_PORT).
    pub(crate) mqtt: Option<MqttConfig>,
    /// Stable service identifier derived from site_name or explicitly configured
    /// via VIGIL_SERVICE_ID.  Used as the MQTT topic namespace and HA device id.
    pub(crate) service_id: String,
}

/// Per-camera entry as it appears in TOML/JSON config files.
#[derive(Debug, Clone, Default, Deserialize)]
struct CameraEntryPartial {
    name: String,
    rtsp_url: Option<String>,
    username: Option<String>,
    password: Option<String>,
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
    rtsp_username: Option<String>,
    rtsp_password: Option<String>,
    detector_model_id: Option<String>,
    detector_model_path: Option<PathBuf>,
    detector_confidence_threshold: Option<f64>,
    detector_sample_frames: Option<usize>,
    /// Multi-camera list.  When present, supersedes camera_name/rtsp_url.
    cameras: Option<Vec<CameraEntryPartial>>,
    // MQTT broker — provided by HA Supervisor or env vars when broker is configured.
    mqtt_host: Option<String>,
    mqtt_port: Option<u16>,
    mqtt_username: Option<String>,
    mqtt_password: Option<String>,
    service_id: Option<String>,
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
    rtsp_username: Option<String>,
    rtsp_password: Option<String>,
    detector_model_id: Option<String>,
    detector_model_path: Option<PathBuf>,
    detector_confidence_threshold: Option<f64>,
    detector_sample_frames: Option<usize>,
}

pub(crate) fn load(args: Vec<OsString>) -> Result<RuntimeConfig, String> {
    let cli = parse_cli(args)?;
    let mut partial = PartialConfig::default();

    if let Some(path) = cli.config_path.as_ref() {
        merge(&mut partial, read_toml_config(path)?);
    } else if Path::new("/data/options.json").exists() {
        merge(
            &mut partial,
            read_options_json(Path::new("/data/options.json"))?,
        );
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
            rtsp_username: cli.rtsp_username,
            rtsp_password: cli.rtsp_password,
            detector_model_id: cli.detector_model_id,
            detector_model_path: cli.detector_model_path,
            detector_confidence_threshold: cli.detector_confidence_threshold,
            detector_sample_frames: cli.detector_sample_frames,
            // Multi-camera list not exposed as CLI flags; comes from config file or options.json.
            cameras: None,
            // MQTT fields are not exposed as CLI flags; they come from env vars or options.json.
            mqtt_host: None,
            mqtt_port: None,
            mqtt_username: None,
            mqtt_password: None,
            service_id: None,
        },
    );
    merge(&mut partial, env_overrides()?);

    // If MQTT_HOST was not supplied via options.json or env, attempt Supervisor services API.
    // Only called when SUPERVISOR_TOKEN is present (i.e. running as an HA add-on).
    if partial.mqtt_host.is_none()
        && let Some(cfg) = crate::supervisor::fetch_supervisor_mqtt()
    {
        partial.mqtt_host = Some(cfg.broker_host);
        // Only override port/creds if the env didn't provide them explicitly.
        if partial.mqtt_port.is_none() {
            partial.mqtt_port = Some(cfg.broker_port);
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
    let review_port = partial.review_port;
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

    // MQTT broker: present when a host is configured.
    let mqtt = partial.mqtt_host.map(|host| MqttConfig {
        broker_host: host,
        broker_port: partial.mqtt_port.unwrap_or(1883),
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
    let cameras: Vec<CameraEntry> = if let Some(cam_list) = partial.cameras {
        cam_list
            .into_iter()
            .map(|c| CameraEntry {
                name: c.name,
                rtsp_url: c.rtsp_url,
                username: c.username,
                password: c.password,
            })
            .collect()
    } else {
        vec![CameraEntry {
            name: camera_name.clone(),
            rtsp_url: partial.rtsp_url.clone(),
            username: partial.rtsp_username.clone(),
            password: partial.rtsp_password.clone(),
        }]
    };

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
        cameras,
        mqtt,
        service_id,
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
            "--health-port" => cli.health_port = Some(next_port(&mut iter, "--health-port")?),
            "--site-name" => cli.site_name = Some(next_string(&mut iter, "--site-name")?),
            "--camera-name" => cli.camera_name = Some(next_string(&mut iter, "--camera-name")?),
            "--rtsp-url" => cli.rtsp_url = Some(next_string(&mut iter, "--rtsp-url")?),
            "--rtsp-username" => {
                cli.rtsp_username = Some(next_string(&mut iter, "--rtsp-username")?)
            }
            "--rtsp-password" => {
                cli.rtsp_password = Some(next_string(&mut iter, "--rtsp-password")?)
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

fn validate_confidence_threshold(value: f64) -> Result<f64, String> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "detector_confidence_threshold must be between 0.0 and 1.0, got {value}"
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

fn read_toml_config(path: &Path) -> Result<PartialConfig, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read config {}: {error}", path.display()))?;
    toml::from_str(&text)
        .map_err(|error| format!("could not parse config {}: {error}", path.display()))
}

fn read_options_json(path: &Path) -> Result<PartialConfig, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read options {}: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("could not parse options {}: {error}", path.display()))
}

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
        rtsp_username: std::env::var("VIGIL_RTSP_USERNAME").ok(),
        rtsp_password: std::env::var("VIGIL_RTSP_PASSWORD").ok(),
        detector_model_id: std::env::var("VIGIL_DETECTOR_MODEL_ID").ok(),
        detector_model_path: std::env::var_os("VIGIL_DETECTOR_MODEL_PATH").map(Into::into),
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
        mqtt_password: std::env::var("MQTT_PASSWORD").ok(),
        service_id: std::env::var("VIGIL_SERVICE_ID").ok(),
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
    if source.detector_sample_frames.is_some() {
        target.detector_sample_frames = source.detector_sample_frames;
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

fn run_usage() -> String {
    "Usage: vigil run [--config PATH] [--data-dir PATH] [--store-path PATH] [--health-port PORT] [--site-name NAME] [--camera-name NAME] [--rtsp-url URL] [--rtsp-username USER] [--rtsp-password PASSWORD] [--detector-model-id ID] [--detector-model-path PATH] [--detector-confidence-threshold FLOAT] [--detector-sample-frames N]"
        .to_string()
}
