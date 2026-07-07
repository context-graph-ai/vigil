use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthStatus {
    Starting,
    Ready,
    StoreOpenFailed,
    IngestFailed,
    DiskFull,
    KeepPaceFailed,
}

impl HealthStatus {
    pub(crate) fn as_u16(self) -> u16 {
        match self {
            Self::Starting => 0,
            Self::Ready => 1,
            Self::StoreOpenFailed => 2,
            Self::IngestFailed => 3,
            Self::DiskFull => 4,
            Self::KeepPaceFailed => 5,
        }
    }

    fn from_u16(value: u16) -> Self {
        match value {
            1 => Self::Ready,
            2 => Self::StoreOpenFailed,
            3 => Self::IngestFailed,
            4 => Self::DiskFull,
            5 => Self::KeepPaceFailed,
            _ => Self::Starting,
        }
    }

    fn http_code(self) -> u16 {
        match self {
            Self::Ready => 200,
            Self::Starting
            | Self::StoreOpenFailed
            | Self::IngestFailed
            | Self::DiskFull
            | Self::KeepPaceFailed => 503,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::StoreOpenFailed => "store_open_failed",
            Self::IngestFailed => "ingest_failed",
            Self::DiskFull => "disk-full",
            Self::KeepPaceFailed => "keep-pace-failed",
        }
    }
}

#[derive(Clone)]
pub struct HealthState {
    status: Arc<AtomicU16>,
    detail: Arc<Mutex<String>>,
}

impl Default for HealthState {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthState {
    pub fn new() -> Self {
        Self {
            status: Arc::new(AtomicU16::new(HealthStatus::Starting.as_u16())),
            detail: Arc::new(Mutex::new(String::new())),
        }
    }

    pub fn set(&self, status: HealthStatus, detail: impl Into<String>) {
        if let Ok(mut guard) = self.detail.lock() {
            *guard = detail.into();
        }
        self.status.store(status.as_u16(), Ordering::SeqCst);
    }

    pub fn snapshot(&self) -> (HealthStatus, String) {
        let status = HealthStatus::from_u16(self.status.load(Ordering::SeqCst));
        let detail = self
            .detail
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default();
        (status, detail)
    }
}

pub(crate) struct HealthServer {
    handle: Option<JoinHandle<()>>,
}

impl HealthServer {
    pub(crate) fn listen(
        port: u16,
        state: HealthState,
        shutdown: Arc<AtomicBool>,
        acceleration: Option<Arc<crate::acceleration::AccelerationState>>,
    ) -> Result<Self, String> {
        Self::bind(port, state, shutdown, acceleration)
    }

    pub(crate) fn bind(
        port: u16,
        state: HealthState,
        shutdown: Arc<AtomicBool>,
        acceleration: Option<Arc<crate::acceleration::AccelerationState>>,
    ) -> Result<Self, String> {
        let listener = TcpListener::bind(("0.0.0.0", port))
            .map_err(|error| format!("health port {port} bind failed: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("health port {port} nonblocking setup failed: {error}"))?;
        let handle = thread::spawn(move || {
            while !shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => handle_client(stream, &state, acceleration.as_deref()),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        Ok(Self {
            handle: Some(handle),
        })
    }

    pub(crate) fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_client(
    mut stream: TcpStream,
    state: &HealthState,
    acceleration: Option<&crate::acceleration::AccelerationState>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let mut buffer = [0_u8; 1024];
    let read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..read]);
    let mut request_parts = request
        .lines()
        .next()
        .map(str::split_whitespace)
        .into_iter()
        .flatten();
    let method = request_parts.next().unwrap_or("");
    let path = request_parts.next().unwrap_or("/");
    if path != "/health" {
        write_response(&mut stream, 404, r#"{"status":"not_found"}"#);
        return;
    }
    if method != "GET" {
        write_response(
            &mut stream,
            405,
            r#"{"status":"method_not_allowed","allow":"GET"}"#,
        );
        return;
    }
    let (status, detail) = state.snapshot();
    // Degraded acceleration (configured-true but software/CPU active) is
    // reported additively; it is degraded acceleration, never a fault.
    let acceleration_field = acceleration
        .map(|state| {
            let degradations = state.health_degradations();
            let degraded = |stage: crate::acceleration::AccelStage| {
                degradations
                    .iter()
                    .any(|degradation| degradation.stage == stage)
            };
            format!(
                r#","acceleration":{{"decode":"{}","detection":"{}"}}"#,
                if degraded(crate::acceleration::AccelStage::Decode) {
                    "degraded"
                } else {
                    "ok"
                },
                if degraded(crate::acceleration::AccelStage::Detection) {
                    "degraded"
                } else {
                    "ok"
                }
            )
        })
        .unwrap_or_default();
    let body = format!(
        r#"{{"status":"{}","version":"{}","detail":"{}"{}}}"#,
        status.label(),
        env!("CARGO_PKG_VERSION"),
        json_escape(&detail),
        acceleration_field
    );
    write_response(&mut stream, status.http_code(), &body);
}

fn write_response(stream: &mut TcpStream, code: u16, body: &str) {
    let reason = match code {
        200 => "OK",
        405 => "Method Not Allowed",
        404 => "Not Found",
        _ => "Service Unavailable",
    };
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
