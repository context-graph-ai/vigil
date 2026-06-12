use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone)]
pub(crate) struct RuntimeConfig {
    pub(crate) data_dir: PathBuf,
    pub(crate) store_path: PathBuf,
    pub(crate) health_port: u16,
}

#[derive(Debug, Default, Deserialize)]
struct PartialConfig {
    data_dir: Option<PathBuf>,
    store_path: Option<PathBuf>,
    health_port: Option<u16>,
}

#[derive(Debug, Default)]
struct CliOverrides {
    config_path: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    store_path: Option<PathBuf>,
    health_port: Option<u16>,
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
        },
    );
    merge(&mut partial, env_overrides()?);

    let data_dir = partial.data_dir.unwrap_or_else(default_data_dir);
    let store_path = partial
        .store_path
        .unwrap_or_else(|| data_dir.join("store.contextgraph"));
    let health_port = partial.health_port.unwrap_or(8099);

    Ok(RuntimeConfig {
        data_dir,
        store_path,
        health_port,
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
            "--help" | "-h" => return Err(run_usage()),
            other => return Err(format!("{other} is not a supported run option")),
        }
    }
    Ok(cli)
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
}

fn default_data_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("vigil-data")
}

fn run_usage() -> String {
    "Usage: vigil run [--config PATH] [--data-dir PATH] [--store-path PATH] [--health-port PORT]"
        .to_string()
}
