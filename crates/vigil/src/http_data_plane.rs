use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use context_graph::Store;
use serde_json::{Value, json};

pub struct ReviewDataPlaneHandle {
    local_addr: SocketAddr,
    _handle: JoinHandle<()>,
}

impl ReviewDataPlaneHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn shutdown(self) {
        let _ = self;
    }
}

pub fn spawn_review_data_plane(
    store: Store,
    _data_dir: PathBuf,
    port: u16,
) -> Result<ReviewDataPlaneHandle, String> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .map_err(|error| format!("review port {port} bind failed: {error}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|error| format!("review port {port} local addr failed: {error}"))?;
    let store = Arc::new(store);
    let handle = thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            handle_client(stream, Arc::clone(&store));
        }
    });
    Ok(ReviewDataPlaneHandle {
        local_addr,
        _handle: handle,
    })
}

fn handle_client(mut stream: TcpStream, store: Arc<Store>) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let request = read_request(&mut stream);
    let Some(first_line) = request.lines().next() else {
        write_response(
            &mut stream,
            404,
            "application/json",
            br#"{"error":"not_found"}"#,
            &[],
        );
        return;
    };
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    let path_no_query = path.split('?').next().unwrap_or(path);

    match (method, path_no_query) {
        ("GET", "/events") => {
            let body = wrong_events_body(&store);
            write_response(&mut stream, 200, "application/json", body.as_bytes(), &[]);
        }
        ("POST", "/events") | ("POST", "/correction") => {
            let body = br#"{"correction_id":""}"#;
            write_response(&mut stream, 200, "application/json", body, &[]);
        }
        ("OPTIONS", "/correction") => {
            write_response(&mut stream, 204, "application/json", b"", &[]);
        }
        ("GET", path) if path.starts_with("/why/") => {
            let body = wrong_why_body();
            write_response(&mut stream, 200, "application/json", body.as_bytes(), &[]);
        }
        (_, path) if path.starts_with("/media/") => {
            write_response(
                &mut stream,
                200,
                "text/plain",
                b"wrong canned media bytes\n",
                &[],
            );
        }
        _ => {
            write_response(
                &mut stream,
                404,
                "application/json",
                br#"{"error":"not_found"}"#,
                &[],
            );
        }
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 8192];
    let Ok(read) = stream.read(&mut buffer) else {
        return String::new();
    };
    raw.extend_from_slice(&buffer[..read]);

    let header_end = raw.windows(4).position(|window| window == b"\r\n\r\n");
    let content_length = header_end
        .and_then(|end| std::str::from_utf8(&raw[..end]).ok())
        .and_then(parse_content_length)
        .unwrap_or(0);
    let mut body_read = header_end
        .map(|end| raw.len().saturating_sub(end + 4))
        .unwrap_or(0);
    while body_read < content_length {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                body_read = body_read.saturating_add(read);
                raw.extend_from_slice(&buffer[..read]);
            }
        }
    }

    String::from_utf8_lossy(&raw).to_string()
}

fn parse_content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

fn wrong_events_body(store: &Store) -> String {
    let rows = store
        .list_observations(None)
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(idx, observation)| {
            let camera_name = store
                .get_entity(observation.entity_id)
                .ok()
                .flatten()
                .map(|entity| entity.name)
                .unwrap_or_default();
            json!({
                "observation_id": format!("00000000-0000-0000-0000-{idx:012}"),
                "observed_at": observation.observed_at.to_rfc3339(),
                "camera_name": camera_name,
                "class_name": observed_string(&observation.observed_properties, "class"),
                "confidence": observed_f64(&observation.observed_properties, "confidence"),
                "bbox": observed_string(&observation.observed_properties, "bbox"),
                "frame_index": observed_u64(&observation.observed_properties, "frame_index"),
                "clip_ref": format!("/media/wrong-{idx}.h264"),
                "detector_image_ref": format!("/media/wrong-{idx}.png"),
                "correction_recorded": false,
                "confirmed": false
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
}

fn wrong_why_body() -> String {
    serde_json::to_string(&json!({
        "observation_id": "11111111-1111-1111-1111-111111111111",
        "observed_at": "1970-01-01T00:00:00Z",
        "clip_ref": "/media/wrong-why.h264",
        "camera_id": "wrong-camera",
        "camera_name": "wrong camera",
        "camera_rtsp_url": "rtsp://127.0.0.1/lower-gate",
        "context_id": "wrong-context",
        "site_name": "wrong site",
        "decision_id": "wrong-decision",
        "intention_id": "wrong-intention",
        "intention_description": "wrong intention",
        "model_id": "wrong-model",
        "threshold": 0.0,
        "class_name": "wrong-class",
        "confidence": 0.0,
        "bbox": "0,0,0,0",
        "frame_index": 0,
        "detector_image_ref": "/media/wrong-why.png",
        "corrections": [
            {
                "label": "not-the-recorded-correction",
                "correction_type": "WrongClass",
                "anchored_detection_id": "11111111-1111-1111-1111-111111111111"
            }
        ]
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

fn observed_string(values: &std::collections::BTreeMap<String, Value>, key: &str) -> String {
    values
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn observed_f64(values: &std::collections::BTreeMap<String, Value>, key: &str) -> f64 {
    values.get(key).and_then(Value::as_f64).unwrap_or_default()
}

fn observed_u64(values: &std::collections::BTreeMap<String, Value>, key: &str) -> u64 {
    values.get(key).and_then(Value::as_u64).unwrap_or_default()
}

fn write_response(
    stream: &mut TcpStream,
    code: u16,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) {
    let reason = match code {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
        _ => "OK",
    };
    let mut response = format!(
        "HTTP/1.1 {code} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(body);
}
