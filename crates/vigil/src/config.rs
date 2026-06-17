use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone)]
pub(crate) struct RuntimeConfig {
    pub(crate) data_dir: PathBuf,
    pub(crate) store_path: PathBuf,
    pub(crate) health_port: u16,
    pub(crate) site_name: String,
    pub(crate) camera_name: String,
    pub(crate) rtsp_url: Option<String>,
    pub(crate) rtsp_username: Option<String>,
    pub(crate) rtsp_password: Option<String>,
    pub(crate) detector_model_id: String,
    pub(crate) detector_model_path: Option<PathBuf>,
    pub(crate) detector_confidence_threshold: f64,
    pub(crate) detector_sample_frames: usize,
}

#[derive(Debug, Default, Deserialize)]
struct PartialConfig {
    data_dir: Option<PathBuf>,
    store_path: Option<PathBuf>,
    health_port: Option<u16>,
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

#[derive(Debug, Default)]
struct CliOverrides {
    config_path: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    store_path: Option<PathBuf>,
    health_port: Option<u16>,
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
            site_name: cli.site_name,
            camera_name: cli.camera_name,
            rtsp_url: cli.rtsp_url,
            rtsp_username: cli.rtsp_username,
            rtsp_password: cli.rtsp_password,
            detector_model_id: cli.detector_model_id,
            detector_model_path: cli.detector_model_path,
            detector_confidence_threshold: cli.detector_confidence_threshold,
            detector_sample_frames: cli.detector_sample_frames,
        },
    );
    merge(&mut partial, env_overrides()?);

    let data_dir = partial.data_dir.unwrap_or_else(default_data_dir);
    let store_path = partial
        .store_path
        .unwrap_or_else(|| data_dir.join("store.contextgraph"));
    let health_port = partial.health_port.unwrap_or(8099);
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

    Ok(RuntimeConfig {
        data_dir,
        store_path,
        health_port,
        site_name,
        camera_name,
        rtsp_url: partial.rtsp_url,
        rtsp_username: partial.rtsp_username,
        rtsp_password: partial.rtsp_password,
        detector_model_id,
        detector_model_path: partial.detector_model_path,
        detector_confidence_threshold,
        detector_sample_frames,
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
