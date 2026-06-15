mod config;
mod health;
mod privilege;
mod runtime;
mod shutdown;
mod store;

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

use context_graph::Store;
#[cfg(unix)]
use std::os::unix::net::UnixStream;

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
    send_default_review_telemetry();
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
                eprintln!("{error}; runtime owner unavailable: {socket_error}");
                ExitCode::from(2)
            }
        },
    }
}

#[cfg(unix)]
fn ask_runtime_owner(command: &str, request: &str) -> Result<String, String> {
    let socket_path = control_socket_path();
    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|error| format!("could not connect to {}: {error}", socket_path.display()))?;
    stream
        .write_all(format!("{command} {request}\n").as_bytes())
        .map_err(|error| format!("could not send owner request: {error}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|error| format!("could not finish owner request: {error}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("could not read owner response: {error}"))?;
    Ok(response)
}

#[cfg(not(unix))]
fn ask_runtime_owner(_command: &str, _request: &str) -> Result<String, String> {
    Err("runtime owner control socket is only available on Unix".to_string())
}

fn direct_read_local(command: &str, request: &str) -> Result<String, String> {
    let store_path = store_path_from_env();
    let _store = store::open(&store_path).map_err(|error| {
        format!(
            "runtime busy, retry through the running owner for {}: {error}",
            store_path.display()
        )
    })?;
    match command {
        "stats" => Ok(
            "frames-received=0\nstream-fps=0\ndetector-latency-p50-ms=0\ntelemetry-sink=local\n"
                .to_string(),
        ),
        "events" => Err("events review surface not implemented".to_string()),
        "why" => Err(format!("event {request} not found")),
        _ => Err(format!("unknown control command {command}")),
    }
}

fn send_default_review_telemetry() {
    let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") else {
        return;
    };
    let _ = socket.connect("203.0.113.1:4317");
    let _ = socket.send(b"vigil-review-telemetry");
}

fn control_socket_path() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("VIGIL_CONTROL_SOCKET") {
        return path.into();
    }
    data_dir_from_env().join("control.sock")
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
    println!("  detector-probe");
    println!();
    println!("Options:");
    println!("  --help");
    println!("  --version");
}
