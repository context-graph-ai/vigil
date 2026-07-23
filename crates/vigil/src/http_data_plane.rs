use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::net::{SocketAddr, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use context_graph::Store;
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::{
    CorrectionError, CorrectionRequest, CorrectionType, EventRow, PersistedClock, ReviewError,
    WhyView, record_correction_with_clock, review_events, review_why,
};

const DEFAULT_EVENT_LIMIT: usize = 100;
const MAX_CORRECTION_BODY_BYTES: usize = 64 * 1024;
const MAX_REVIEW_DATA_PLANE_MEDIA_HANDLERS: usize = 16;

// ── Published HTTP route surface ───────────────────────────────────────────
//
// These path strings are the caller-facing contract on this in-process
// review data plane — a Home Assistant automation or browser script may
// bind to them directly. Renaming one is a deliberate, reviewed change to a
// published identifier, not a routine refactor — see
// `crates/vigil/tests/http_route_contract.rs`.
pub const EVENTS_ROUTE: &str = "/events";
pub const WHY_ROUTE_PREFIX: &str = "/why/";
pub const CORRECTION_ROUTE: &str = "/correction";
pub const MEDIA_ROUTE_PREFIX: &str = "/media/";

struct ShutdownAwareReader<R> {
    inner: R,
    shutdown: Arc<AtomicBool>,
}

impl<R> ShutdownAwareReader<R> {
    fn new(inner: R, shutdown: Arc<AtomicBool>) -> Self {
        Self { inner, shutdown }
    }
}

impl<R: Read> Read for ShutdownAwareReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.shutdown.load(Ordering::SeqCst) {
            return Ok(0);
        }
        self.inner.read(buffer)
    }
}

pub struct ReviewDataPlaneHandle {
    local_addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ReviewDataPlaneHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn shutdown(self) {
        drop(self);
    }
}

impl Drop for ReviewDataPlaneHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let wake_addr = SocketAddr::from(([127, 0, 0, 1], self.local_addr.port()));
        let _ = TcpStream::connect_timeout(&wake_addr, Duration::from_millis(100));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn spawn_review_data_plane(
    store: Store,
    data_dir: PathBuf,
    port: u16,
) -> Result<ReviewDataPlaneHandle, String> {
    spawn_review_data_plane_with_clock(store, data_dir, port, PersistedClock::contextdb())
}

/// Spawn the review worker with an explicit persisted-time authority.
///
/// The clock is cloned into the worker instead of relying on thread-local test
/// state, so correction ordering can be tested across the real HTTP boundary.
pub fn spawn_review_data_plane_with_clock(
    store: Store,
    data_dir: PathBuf,
    port: u16,
    clock: PersistedClock,
) -> Result<ReviewDataPlaneHandle, String> {
    let (server, local_addr) = bind_review_server(port)?;
    let bind_port = local_addr.port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let store = Arc::new(store);
    let data_dir = Arc::new(data_dir);
    let active_media_handlers = Arc::new(AtomicUsize::new(0));
    let handle = thread::spawn(move || {
        let mut server = Some(server);
        let mut media_handlers: Vec<JoinHandle<()>> = Vec::new();
        while !worker_shutdown.load(Ordering::SeqCst) {
            if server.is_none() {
                match bind_review_server(bind_port) {
                    Ok((rebound_server, _)) => {
                        eprintln!("review_data_plane_rebound=true port={bind_port}");
                        server = Some(rebound_server);
                    }
                    Err(error) => {
                        eprintln!(
                            "review_data_plane_rebind_failed=true port={bind_port} error={error}"
                        );
                        thread::sleep(Duration::from_millis(250));
                        join_finished_handlers(&mut media_handlers);
                        continue;
                    }
                }
            }

            let active_server = server
                .as_ref()
                .expect("review data-plane server is rebound before accept");
            let accepted = panic::catch_unwind(AssertUnwindSafe(|| {
                active_server.recv_timeout(Duration::from_millis(100))
            }));
            match accepted {
                Ok(Ok(Some(request))) => {
                    if is_media_request(&request) {
                        let active_count = active_media_handlers.fetch_add(1, Ordering::SeqCst);
                        if active_count >= MAX_REVIEW_DATA_PLANE_MEDIA_HANDLERS {
                            active_media_handlers.fetch_sub(1, Ordering::SeqCst);
                            let _ = request.respond(json_error(503, "review_data_plane_busy"));
                        } else {
                            let data_dir = Arc::clone(&data_dir);
                            let shutdown = Arc::clone(&worker_shutdown);
                            let active_media_handlers = Arc::clone(&active_media_handlers);
                            media_handlers.push(thread::spawn(move || {
                                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                                    handle_media_request(request, data_dir, shutdown);
                                }));
                                active_media_handlers.fetch_sub(1, Ordering::SeqCst);
                                if result.is_err() {
                                    eprintln!("review_data_plane_media_panic=true");
                                }
                            }));
                        }
                    } else {
                        let store = Arc::clone(&store);
                        let result = panic::catch_unwind(AssertUnwindSafe(|| {
                            handle_request(request, store, &clock);
                        }));
                        if result.is_err() {
                            eprintln!("review_data_plane_request_panic=true");
                        }
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => {
                    eprintln!("review_data_plane_accept_error={error}");
                    server = None;
                }
                Err(_) => {
                    eprintln!("review_data_plane_accept_panic=true");
                    server = None;
                    thread::sleep(Duration::from_millis(100));
                }
            }
            join_finished_handlers(&mut media_handlers);
        }
        for handler in media_handlers {
            let _ = handler.join();
        }
    });

    Ok(ReviewDataPlaneHandle {
        local_addr,
        shutdown,
        handle: Some(handle),
    })
}

fn bind_review_server(port: u16) -> Result<(Server, SocketAddr), String> {
    let server = Server::http(("0.0.0.0", port))
        .map_err(|error| format!("review port {port} bind failed: {error}"))?;
    let local_addr = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| "review data plane did not bind an IP socket".to_string())?;
    Ok((server, local_addr))
}

fn is_media_request(request: &Request) -> bool {
    request.method() == &Method::Get
        && request
            .url()
            .split('?')
            .next()
            .unwrap_or(request.url())
            .starts_with(MEDIA_ROUTE_PREFIX)
}

fn join_finished_handlers(handlers: &mut Vec<JoinHandle<()>>) {
    let mut idx = 0;
    while idx < handlers.len() {
        if handlers[idx].is_finished() {
            let handle = handlers.swap_remove(idx);
            let _ = handle.join();
        } else {
            idx += 1;
        }
    }
}

fn handle_media_request(request: Request, data_dir: Arc<PathBuf>, shutdown: Arc<AtomicBool>) {
    let raw_url = request.url().to_string();
    let path = raw_url.split('?').next().unwrap_or(raw_url.as_str());
    let response = media_response(&data_dir, &request, path, shutdown);
    let _ = request.respond(response);
}

fn handle_request(mut request: Request, store: Arc<Store>, clock: &PersistedClock) {
    let method = request.method().clone();
    let raw_url = request.url().to_string();
    let path = raw_url.split('?').next().unwrap_or(raw_url.as_str());
    let response = match (&method, path) {
        (Method::Get, EVENTS_ROUTE) => events_response(&store, &raw_url),
        (Method::Get, path) if path.starts_with(WHY_ROUTE_PREFIX) => why_response(&store, path),
        (Method::Post, CORRECTION_ROUTE) => correction_response(&store, &mut request, clock),
        (Method::Post, EVENTS_ROUTE) => json_error(405, "method_not_allowed"),
        _ => json_error(404, "not_found"),
    };
    let _ = request.respond(response);
}

fn events_response(store: &Store, raw_url: &str) -> Response<Box<dyn Read + Send>> {
    match review_events(store, event_limit(raw_url)) {
        Ok(events) => {
            let rows = events.rows.iter().map(event_row_json).collect::<Vec<_>>();
            json_response(200, &Value::Array(rows))
        }
        Err(ReviewError::NotFound(_)) => json_error(404, "not_found"),
        Err(ReviewError::StoreError(_)) => json_error(500, "store_error"),
    }
}

fn event_limit(raw_url: &str) -> usize {
    raw_url
        .split_once('?')
        .and_then(|(_, query)| {
            query.split('&').find_map(|pair| {
                let (name, value) = pair.split_once('=')?;
                (name == "limit")
                    .then(|| value.parse::<usize>().ok())
                    .flatten()
            })
        })
        .filter(|limit| *limit > 0)
        .map(|limit| limit.min(500))
        .unwrap_or(DEFAULT_EVENT_LIMIT)
}

fn event_row_json(row: &EventRow) -> Value {
    json!({
        "observation_id": row.observation_id,
        "observed_at": row.observed_at,
        "camera_name": row.camera_name,
        "class_name": row.class_name,
        "confidence": row.confidence,
        "bbox": row.bbox,
        "frame_index": row.frame_index,
        "clip_ref": media_route(&row.clip_ref),
        "detector_image_ref": media_route(&row.detector_image_ref),
        "correction_recorded": row.correction_recorded,
        "confirmed": row.confirmed,
        "current_correction": row.current_correction.as_ref().map(correction_type_str),
        "corrected_label": row.corrected_label,
        "entity_name": row.entity_name,
    })
}

fn why_response(store: &Store, path: &str) -> Response<Box<dyn Read + Send>> {
    let detection_id = path.trim_start_matches(WHY_ROUTE_PREFIX);
    if detection_id.is_empty() || detection_id == "--latest" {
        return json_error(404, "not_found");
    }
    match review_why(store, detection_id) {
        Ok(why) => json_response(200, &why_json(&why)),
        Err(error) => why_error_response(error),
    }
}

fn why_json(why: &WhyView) -> Value {
    let corrections = why
        .corrections
        .iter()
        .map(|correction| {
            json!({
                "label": correction.label,
                "correction_type": correction_type_str(&correction.correction_type),
                "anchored_detection_id": correction.anchored_detection_id,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "observation_id": why.observation_id,
        "observed_at": why.observed_at,
        "clip_ref": media_route(&why.clip_ref),
        "camera_id": why.camera_id,
        "camera_name": why.camera_name,
        "context_id": why.context_id,
        "site_name": why.site_name,
        "decision_id": why.decision_id,
        "intention_id": why.intention_id,
        "intention_description": why.intention_description,
        "recognition": why.recognition.as_ref().map(|r| json!({
            "name": r.name,
            "score": r.score,
            "reference_label": r.reference_label,
            "enrolled_by_correction_id": r.enrolled_by_correction_id,
        })),
        "model_id": why.model_id,
        "threshold": why.threshold,
        "class_name": why.class_name,
        "confidence": why.confidence,
        "bbox": why.bbox,
        "frame_index": why.frame_index,
        "detector_image_ref": media_route(&why.detector_image_ref),
        "corrections": corrections,
    })
}

fn correction_response(
    store: &Store,
    request: &mut Request,
    clock: &PersistedClock,
) -> Response<Box<dyn Read + Send>> {
    let Some(body) = read_limited_body(request) else {
        return json_error(413, "payload_too_large");
    };
    let value = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(_) => return json_error(400, "bad_json"),
    };
    let detection_id = match value.get("detection_id").and_then(Value::as_str) {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => return json_error(400, "missing_detection_id"),
    };
    let correction_type = match value.get("correction_type").and_then(Value::as_str) {
        Some("Identity") => CorrectionType::Identity,
        Some("WrongClass") => CorrectionType::WrongClass,
        Some("FalseAlarm") => CorrectionType::FalseAlarm,
        _ => return json_error(400, "bad_correction_type"),
    };
    let label = value
        .get("label")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    match record_correction_with_clock(
        store,
        CorrectionRequest {
            detection_id,
            label,
            correction_type,
        },
        clock,
    ) {
        Ok(receipt) => json_response(200, &json!({ "correction_id": receipt.correction_id })),
        Err(CorrectionError::NoAnchor(_)) => json_error(404, "detection_not_found"),
        Err(CorrectionError::WriteFailed(_)) => json_error(500, "write_failed"),
    }
}

fn read_limited_body(request: &mut Request) -> Option<Vec<u8>> {
    let len = request.body_length().unwrap_or(0);
    if len > MAX_CORRECTION_BODY_BYTES {
        return None;
    }
    let mut body = Vec::with_capacity(len);
    let mut reader = request
        .as_reader()
        .take((MAX_CORRECTION_BODY_BYTES + 1) as u64);
    if reader.read_to_end(&mut body).is_err() || body.len() > MAX_CORRECTION_BODY_BYTES {
        return None;
    }
    Some(body)
}

fn media_response(
    data_dir: &Path,
    request: &Request,
    path: &str,
    shutdown: Arc<AtomicBool>,
) -> Response<Box<dyn Read + Send>> {
    let Some(file_name) = media_file_name(path) else {
        return json_error(400, "bad_media_ref");
    };
    let clips_dir = data_dir.join("clips");
    let Ok(clips_root) = clips_dir.canonicalize() else {
        return json_error(404, "media_not_found");
    };
    let file_path = clips_dir.join(&file_name);
    let Ok(file_path) = file_path.canonicalize() else {
        return json_error(404, "media_not_found");
    };
    if !file_path.starts_with(&clips_root) {
        return json_error(400, "bad_media_ref");
    }
    let Ok(mut file) = File::open(&file_path) else {
        return json_error(404, "media_not_found");
    };
    let Ok(metadata) = file.metadata() else {
        return json_error(404, "media_not_found");
    };
    let size = metadata.len();
    let content_type = media_content_type(&file_name);
    let range: Option<Result<(u64, u64), ()>> =
        request_header(request, "Range").and_then(|value| parse_range(value, size));
    match range {
        Some(Ok((start, end))) => {
            if file.seek(SeekFrom::Start(start)).is_err() {
                return json_error(500, "media_seek_failed");
            }
            let len = end.saturating_sub(start).saturating_add(1);
            let Ok(data_len) = usize::try_from(len) else {
                return json_error(500, "media_too_large");
            };
            let headers = media_headers(content_type)
                .into_iter()
                .chain([
                    header("Content-Range", &format!("bytes {start}-{end}/{size}")),
                    header("Content-Length", &len.to_string()),
                ])
                .collect::<Vec<_>>();
            Response::new(
                StatusCode(206),
                headers,
                Box::new(ShutdownAwareReader::new(file.take(len), shutdown))
                    as Box<dyn Read + Send>,
                Some(data_len),
                None,
            )
            .with_chunked_threshold(usize::MAX)
        }
        Some(Err(())) => {
            let headers = media_headers(content_type)
                .into_iter()
                .chain([
                    header("Content-Range", &format!("bytes */{size}")),
                    header("Content-Length", "0"),
                ])
                .collect::<Vec<_>>();
            Response::new(
                StatusCode(416),
                headers,
                Box::new(io::empty()) as Box<dyn Read + Send>,
                Some(0),
                None,
            )
            .with_chunked_threshold(usize::MAX)
        }
        None => {
            let Ok(data_len) = usize::try_from(size) else {
                return json_error(500, "media_too_large");
            };
            let headers = media_headers(content_type)
                .into_iter()
                .chain([header("Content-Length", &size.to_string())])
                .collect::<Vec<_>>();
            Response::new(
                StatusCode(200),
                headers,
                Box::new(ShutdownAwareReader::new(file, shutdown)) as Box<dyn Read + Send>,
                Some(data_len),
                None,
            )
            .with_chunked_threshold(usize::MAX)
        }
    }
}

fn media_file_name(path: &str) -> Option<String> {
    let encoded = path.strip_prefix(MEDIA_ROUTE_PREFIX)?;
    let decoded = percent_decode(encoded)?;
    if decoded.is_empty()
        || decoded.contains('/')
        || decoded.contains('\\')
        || decoded.contains('\0')
        || decoded == "."
        || decoded == ".."
        || decoded.contains("..")
    {
        return None;
    }
    Some(decoded)
}

fn media_route(source_ref: &str) -> String {
    source_ref
        .strip_prefix("vigil-edge:clip/")
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .map(|name| format!("{MEDIA_ROUTE_PREFIX}{}", percent_encode_path_segment(name)))
        .unwrap_or_default()
}

fn parse_range(value: &str, size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    if size == 0 {
        return Some(Err(()));
    }
    let (start, end) = spec.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        if suffix == 0 {
            return Some(Err(()));
        }
        let start = size.saturating_sub(suffix);
        return Some(Ok((start, size - 1)));
    }
    let start = start.parse::<u64>().ok()?;
    if start >= size {
        return Some(Err(()));
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    if end < start {
        return Some(Err(()));
    }
    Some(Ok((start, end)))
}

fn why_error_response(error: ReviewError) -> Response<Box<dyn Read + Send>> {
    match error {
        ReviewError::NotFound(_) => json_error(404, "not_found"),
        ReviewError::StoreError(_) => json_error(500, "store_error"),
    }
}

fn json_response(status: u16, value: &Value) -> Response<Box<dyn Read + Send>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let headers = vec![
        header("Content-Type", "application/json"),
        header("Content-Length", &body.len().to_string()),
    ];
    Response::new(
        StatusCode(status),
        headers,
        Box::new(Cursor::new(body)) as Box<dyn Read + Send>,
        None,
        None,
    )
}

fn json_error(status: u16, code: &str) -> Response<Box<dyn Read + Send>> {
    json_response(status, &json!({ "error": code }))
}

fn media_headers(content_type: &str) -> Vec<Header> {
    vec![
        header("Content-Type", content_type),
        header("Accept-Ranges", "bytes"),
    ]
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static HTTP header is valid")
}

fn request_header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}

fn correction_type_str(correction_type: &CorrectionType) -> &'static str {
    match correction_type {
        CorrectionType::Identity => "Identity",
        CorrectionType::WrongClass => "WrongClass",
        CorrectionType::FalseAlarm => "FalseAlarm",
        CorrectionType::Enroll => "Enroll",
    }
}

fn media_content_type(file_name: &str) -> &'static str {
    match Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "mp4" => "video/mp4",
        "h264" => "video/h264",
        "h265" | "hevc" => "video/h265",
        _ => "application/octet-stream",
    }
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' {
            let hi = *bytes.get(idx + 1)?;
            let lo = *bytes.get(idx + 2)?;
            out.push(hex_value(hi)? * 16 + hex_value(lo)?);
            idx += 3;
        } else {
            out.push(bytes[idx]);
            idx += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn percent_encode_path_segment(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
