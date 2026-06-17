use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

pub(crate) type OwnerHandler = Arc<dyn Fn(String) -> String + Send + Sync + 'static>;

#[cfg(unix)]
pub(crate) fn start_live_owner(
    data_dir: &Path,
    shutdown: Arc<AtomicBool>,
    handler: OwnerHandler,
) -> Option<JoinHandle<()>> {
    let socket_path = live_owner_socket_path(data_dir);
    if let Some(parent) = socket_path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        eprintln!(
            "owner transport directory setup failed path={} error={error}",
            parent.display()
        );
        return None;
    }
    let _ = fs::remove_file(&socket_path);
    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!(
                "owner transport bind failed path={} error={error}",
                socket_path.display()
            );
            return None;
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        eprintln!("owner transport nonblocking setup failed error={error}");
        return None;
    }
    println!("owner transport listening path={}", socket_path.display());
    Some(thread::spawn(move || {
        accept_live_owner_loop(listener, socket_path, shutdown, handler);
    }))
}

#[cfg(not(unix))]
pub(crate) fn start_live_owner(
    _data_dir: &Path,
    _shutdown: Arc<AtomicBool>,
    _handler: OwnerHandler,
) -> Option<JoinHandle<()>> {
    None
}

#[cfg(unix)]
fn accept_live_owner_loop(
    listener: UnixListener,
    socket_path: PathBuf,
    shutdown: Arc<AtomicBool>,
    handler: OwnerHandler,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                let mut frame = String::new();
                let _ = stream.read_to_string(&mut frame);
                let response = handler(frame);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                eprintln!("owner transport accept failed error={error}");
                break;
            }
        }
    }
    let _ = fs::remove_file(&socket_path);
}

#[cfg(unix)]
pub(crate) fn request_live_owner(data_dir: &Path, frame: &str) -> Result<String, String> {
    let socket_path = live_owner_socket_path(data_dir);
    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|error| format!("could not connect to {}: {error}", socket_path.display()))?;
    stream
        .write_all(frame.as_bytes())
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
pub(crate) fn request_live_owner(_data_dir: &Path, _frame: &str) -> Result<String, String> {
    Err("live owner transport is only available on Unix".to_string())
}

#[cfg(unix)]
fn live_owner_socket_path(data_dir: &Path) -> PathBuf {
    std::env::var_os("VIGIL_CONTROL_SOCKET")
        .map(Into::into)
        .unwrap_or_else(|| data_dir.join("control.sock"))
}
