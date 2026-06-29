mod config;
mod control_socket;
pub mod correction;
pub mod ha_discovery;
pub mod ha_mqtt_tasks;
mod health;
mod http_data_plane;
mod live_read;
mod media_pipeline;
mod privilege;
mod runtime;
mod runtime_stats;
mod shutdown;
mod store;
mod supervisor;
mod yolox_detector;

pub use correction::{
    CorrectionError, CorrectionReceipt, CorrectionRequest, CorrectionType, EventRow, EventsView,
    RecordedCorrection, ReviewError, WhyView, record_correction, review_events, review_why,
};
pub use ha_discovery::{
    CameraConfig, CommandTopicMessage, DetectionInput, DiscoveryPayload, EventPayload, ParseError,
    ServiceConfig, generate_discovery_payloads, map_detection_to_event_payload,
    parse_command_topic,
};
pub use ha_mqtt_tasks::{
    DetectionPublisher, DetectionPublisherHandle, MqttConfig, SubscriberHandle,
    WiredSubscriberConfig, mqtt_connect_intent, publish_detection_event,
    publish_discovery_to_broker, spawn_detection_publisher, spawn_production_subscriber,
};
pub use health::{HealthState, HealthStatus};
pub use http_data_plane::{ReviewDataPlaneHandle, spawn_review_data_plane};

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use context_graph::{Store, request_control};

pub fn run_cli<I>(args: I) -> ExitCode
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let _program = args.next();

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
        Some(command) if command == "detector-probe" => runtime::run_detector_probe(args.collect()),
        Some(command) if command == "run" => runtime::run(args.collect()),
        _ => {
            print_help();
            ExitCode::from(2)
        }
    }
}

pub fn open_context_graph_store_with_text_embedder_disabled(path: &Path) -> Result<Store, String> {
    store::open(path).map(|open| open.handle)
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
        _ => Err(format!("unknown control command {command}")),
    }
}

fn is_database_locked_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("database is locked") || (lower.contains("locked") && lower.contains("process"))
}

fn store_path_from_env() -> std::path::PathBuf {
    std::env::var_os("VIGIL_STORE_PATH")
        .map(Into::into)
        .unwrap_or_else(|| data_dir_from_env().join("store.contextgraph"))
}

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
    println!();
    println!("Options:");
    println!("  --help");
    println!("  --version");
}
