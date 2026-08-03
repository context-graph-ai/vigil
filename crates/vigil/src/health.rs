use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// The liveness-probe route. The Home Assistant Supervisor watchdog and any
/// external monitor polls this path; renaming it is a deliberate, reviewed
/// change to a published identifier, not a routine refactor — see
/// `crates/vigil/tests/http_route_contract.rs`.
pub const HEALTH_PATH: &str = "/health";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthStatus {
    Starting,
    Ready,
    StoreOpenFailed,
    IngestFailed,
    DiskFull,
    KeepPaceFailed,
    /// Alive and functioning, but this deployment declares zero cameras — a
    /// legitimate worker/discovery node. Kept distinct from `Ready` so a box
    /// watching nothing can never present as an ordinary, camera-serving,
    /// unqualified "ready" box (still a 2xx liveness answer).
    NoCamerasConfigured,
}

impl HealthStatus {
    /// A stable numeric code for this status. Used by an integration adapter
    /// to detect a health change worth re-announcing without comparing the
    /// enum variant directly.
    pub fn as_u16(self) -> u16 {
        match self {
            Self::Starting => 0,
            Self::Ready => 1,
            Self::StoreOpenFailed => 2,
            Self::IngestFailed => 3,
            Self::DiskFull => 4,
            Self::KeepPaceFailed => 5,
            Self::NoCamerasConfigured => 6,
        }
    }

    fn from_u16(value: u16) -> Self {
        match value {
            1 => Self::Ready,
            2 => Self::StoreOpenFailed,
            3 => Self::IngestFailed,
            4 => Self::DiskFull,
            5 => Self::KeepPaceFailed,
            6 => Self::NoCamerasConfigured,
            _ => Self::Starting,
        }
    }

    /// The watchdog-facing wire code: a LIVENESS signal, not a readiness one.
    /// The Home Assistant Supervisor stops+restarts the add-on on any non-2xx
    /// response, so this answers 2xx whenever the runtime is alive and its
    /// pipeline is functioning — EVEN degraded (`KeepPaceFailed`: detection has
    /// fallen behind on CPU fallback, which is slow, not dead). Non-2xx is
    /// reserved for genuinely dead/wedged states a restart can help: still
    /// `Starting`, the store never opened, ingest/detector failed, or the box
    /// cannot write clips (`DiskFull` fails the NVR's primary recording
    /// contract). The precise degraded state stays named in the `/health` body.
    pub fn liveness_status_code(self) -> u16 {
        match self {
            Self::Ready | Self::KeepPaceFailed | Self::NoCamerasConfigured => 200,
            Self::Starting | Self::StoreOpenFailed | Self::IngestFailed | Self::DiskFull => 503,
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
            Self::NoCamerasConfigured => "no_cameras_configured",
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

/// The fixed camera-source-kind vocabulary the `/health` body's
/// `cameras_by_kind` object always carries an entry for, in this order —
/// every kind this artifact knows about, whether or not it can currently
/// load one (`config::artifact_supports_source_kind`), so the structure is
/// complete rather than only covering whichever kind happens to be
/// configured.
const CAMERA_SOURCE_KINDS: [crate::config::CameraSourceKind; 4] = [
    crate::config::CameraSourceKind::Rtsp,
    crate::config::CameraSourceKind::Usb,
    crate::config::CameraSourceKind::Csi,
    crate::config::CameraSourceKind::Mjpeg,
];

/// Camera names grouped by resolved source kind, in [`CAMERA_SOURCE_KINDS`]
/// order — computed once at bind time from the loaded config (the by-kind
/// grouping is a configuration fact, fixed for the process lifetime) and
/// reused on every `/health` request. Only the per-camera `status` field
/// varies per request, filled in from the same live snapshot every other
/// field on the body reads.
fn group_cameras_by_kind(
    cameras: &[crate::config::CameraHealthEntry],
) -> Vec<(&'static str, Vec<String>)> {
    CAMERA_SOURCE_KINDS
        .iter()
        .map(|kind| {
            let names = cameras
                .iter()
                .filter(|camera| camera.kind == Some(*kind))
                .map(|camera| camera.name.clone())
                .collect();
            (kind.as_str(), names)
        })
        .collect()
}

/// Render the `cameras_by_kind` object as a leading-comma JSON fragment
/// (composes with `acceleration_field` the same way), reporting every
/// camera at the given request's current status label — the runtime
/// tracks one shared liveness state today, so a per-camera entry must
/// never invent a diverging value from the same snapshot's top-level
/// `status`.
fn render_cameras_by_kind(groups: &[(&'static str, Vec<String>)], status_label: &str) -> String {
    let mut out = String::from(r#","cameras_by_kind":{"#);
    for (kind_index, (kind, names)) in groups.iter().enumerate() {
        if kind_index > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(kind);
        out.push_str(r#"":{"count":"#);
        out.push_str(&names.len().to_string());
        out.push_str(r#","cameras":["#);
        for (camera_index, name) in names.iter().enumerate() {
            if camera_index > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                r#"{{"name":"{}","status":"{}"}}"#,
                json_escape(name),
                status_label
            ));
        }
        out.push_str("]}");
    }
    out.push('}');
    out
}

pub(crate) struct HealthServer {
    handle: Option<JoinHandle<()>>,
}

impl HealthServer {
    /// `stats`, when given, lets `/health` render the SAME non-secret
    /// `fabric-status=` line `vigil stats`/`vigil doctor` do (criterion C7) —
    /// read from the live snapshot, never re-derived, so the surfaces
    /// structurally cannot disagree. The fabric enrollment ticket (the
    /// `fabric-join` line's credential) is a separate matter: `stats` is
    /// [`crate::runtime_stats::HealthFabricStatus`], a type with no field or
    /// accessor that can ever yield it — `/health` cannot serve the ticket
    /// no matter what `RuntimeStats` grows later. Retrieving the ticket is a
    /// deliberate local act (`vigil fabric ticket`) or the sanctioned
    /// startup log instruction, never this served HTTP surface.
    // VIGIL_TEST_EPHEMERAL_HEALTH_PORT is an enumerated, reviewed test-only
    // read (`environment_read_surface.baseline.txt`), not an ad-hoc one.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn bind(
        port: u16,
        state: HealthState,
        shutdown: Arc<AtomicBool>,
        acceleration: Option<Arc<crate::acceleration::AccelerationState>>,
        stats: Option<crate::runtime_stats::HealthFabricStatus>,
        cameras: Vec<crate::config::CameraHealthEntry>,
    ) -> Result<Self, String> {
        let test_ephemeral_bind =
            port == 0 && std::env::var("VIGIL_TEST_EPHEMERAL_HEALTH_PORT").as_deref() == Ok("1");
        if port == 0 && !test_ephemeral_bind {
            return Err(
                "health port 0 is reserved for VIGIL_TEST_EPHEMERAL_HEALTH_PORT=1 test probes"
                    .to_string(),
            );
        }
        let bind_host = if test_ephemeral_bind {
            "127.0.0.1"
        } else {
            "0.0.0.0"
        };
        let listener = TcpListener::bind((bind_host, port))
            .map_err(|error| format!("health port {port} bind failed: {error}"))?;
        let bound_port = listener
            .local_addr()
            .map_err(|error| format!("health port {port} local-address receipt failed: {error}"))?
            .port();
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("health port {port} nonblocking setup failed: {error}"))?;
        if test_ephemeral_bind {
            println!("test_health_port_receipt=bound address=127.0.0.1 port={bound_port}");
        }
        let cameras_by_kind = group_cameras_by_kind(&cameras);
        let handle = thread::spawn(move || {
            while !shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => handle_client(
                        stream,
                        &state,
                        acceleration.as_deref(),
                        stats.as_ref(),
                        &cameras_by_kind,
                    ),
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
    stats: Option<&crate::runtime_stats::HealthFabricStatus>,
    cameras_by_kind: &[(&'static str, Vec<String>)],
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
    if path != HEALTH_PATH {
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
            let receipts = state.snapshot();
            // "ok" is an achieved state, never a default: a stage with no
            // recorded receipt yet (no stream configured, nothing decoded)
            // reports "pending", so this surface cannot claim working
            // acceleration before a real receipt exists.
            let stage_value = |stage: crate::acceleration::AccelStage| {
                let degraded = degradations
                    .iter()
                    .any(|degradation| degradation.stage == stage);
                let has_receipt = receipts.iter().any(|receipt| receipt.stage == stage);
                if degraded {
                    "degraded"
                } else if has_receipt {
                    "ok"
                } else {
                    "pending"
                }
            };
            format!(
                r#","acceleration":{{"decode":"{}","detection":"{}"}}"#,
                stage_value(crate::acceleration::AccelStage::Decode),
                stage_value(crate::acceleration::AccelStage::Detection)
            )
        })
        .unwrap_or_default();
    let cameras_by_kind_field = render_cameras_by_kind(cameras_by_kind, status.label());
    let mut body = format!(
        r#"{{"status":"{}","version":"{}","detail":"{}"{}{}}}"#,
        status.label(),
        env!("CARGO_PKG_VERSION"),
        json_escape(&detail),
        acceleration_field,
        cameras_by_kind_field
    );
    // A Home Assistant user reading /health gets the same honest,
    // fixed-format acceleration block doctor renders — not just the
    // degraded/ok JSON summary above — so the plain hardware-or-CPU verdict
    // is visible on this surface too, not only the CLI.
    if let Some(acceleration) = acceleration {
        for receipt in acceleration.snapshot() {
            body.push('\n');
            body.push_str(&crate::acceleration::render_receipt_block(&receipt));
        }
    }
    // The same non-secret fabric-status line `vigil stats`/`vigil doctor`
    // print (criterion C7) — read from the live snapshot, never re-derived.
    // The fabric-join credential is never threaded here at all; see
    // `HealthServer::bind`'s doc comment.
    if let Some(stats) = stats {
        let fabric_status = stats.read();
        if !fabric_status.is_empty() {
            body.push('\n');
            body.push_str(&fabric_status);
        }
    }
    write_response(&mut stream, status.liveness_status_code(), &body);
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
