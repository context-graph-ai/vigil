use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use context_graph::{
    CreateContext, CreateDecision, CreateEntity, CreateIntention, EntityType, EvidenceId,
    EvidenceKind, EvidenceProducer, EvidenceRef, IntentionOrigin, IntentionStatus, Observation,
    ObservationId, RecordObservation, RetentionStatus, Store,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use vigil::{
    CorrectionRequest, CorrectionType, EventRow, ReviewDataPlaneHandle, record_correction,
    review_events, review_why, spawn_review_data_plane,
};

#[path = "ha_test_support.rs"]
mod ha_test_support;

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> HttpResponse {
    request_limited(port, method, path, headers, body, usize::MAX)
}

fn request_limited(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
    max_body_bytes: usize,
) -> HttpResponse {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .expect("data-plane port must accept TCP connections");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    let request_path = if path.is_empty() { "/" } else { path };
    let mut wire = format!(
        "{method} {request_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n"
    );
    for (name, value) in headers {
        wire.push_str(name);
        wire.push_str(": ");
        wire.push_str(value);
        wire.push_str("\r\n");
    }
    if !body.is_empty() {
        wire.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    wire.push_str("\r\n");
    stream
        .write_all(wire.as_bytes())
        .expect("write HTTP request headers");
    if !body.is_empty() {
        stream.write_all(body).expect("write HTTP request body");
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let mut raw = Vec::new();
    let mut buf = [0_u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > max_body_bytes.saturating_add(16_384) {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => panic!("read HTTP response: {error}"),
        }
    }
    parse_response(&raw, max_body_bytes)
}

fn parse_response(raw: &[u8], max_body_bytes: usize) -> HttpResponse {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response must include header/body delimiter");
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.lines();
    let status_line = lines
        .next()
        .expect("HTTP response must include status line");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .expect("HTTP status line must include status code")
        .parse::<u16>()
        .expect("HTTP status code must be numeric");
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect::<BTreeMap<_, _>>();
    let body_start = split + 4;
    let body_end = raw.len().min(body_start.saturating_add(max_body_bytes));
    HttpResponse {
        status,
        headers,
        body: raw[body_start..body_end].to_vec(),
    }
}

fn json_body(response: &HttpResponse) -> Value {
    serde_json::from_slice(&response.body).unwrap_or_else(|error| {
        panic!(
            "response body must be JSON, status={} body={} error={error}",
            response.status,
            response.body_text()
        )
    })
}

fn rows(response: &HttpResponse) -> Vec<Value> {
    match json_body(response) {
        Value::Array(rows) => rows,
        other => panic!("events response must be a JSON array, got {other:?}"),
    }
}

fn spawn_server(store_path: &Path) -> (ReviewDataPlaneHandle, u16) {
    let store = ha_test_support::open_store_at(store_path).expect("store must open for data plane");
    let data_dir = store_path
        .parent()
        .expect("store path must have data dir parent")
        .to_path_buf();
    let port = ha_test_support::free_port().expect("allocate review port");
    let handle = spawn_review_data_plane(store, data_dir, port).expect("spawn review data plane");
    let port = handle.local_addr().port();
    ha_test_support::wait_for_tcp_port(port, Duration::from_secs(2))
        .expect("review data plane must open TCP port");
    (handle, port)
}

fn get(port: u16, path: &str) -> HttpResponse {
    request(port, "GET", path, &[], b"")
}

fn get_with_headers(port: u16, path: &str, headers: &[(&str, String)]) -> HttpResponse {
    request(port, "GET", path, headers, b"")
}

fn post_json(port: u16, path: &str, json_body: &str) -> HttpResponse {
    request(
        port,
        "POST",
        path,
        &[("Content-Type", "application/json".to_string())],
        json_body.as_bytes(),
    )
}

fn field_str(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn field_f64(value: &Value, field: &str) -> f64 {
    value.get(field).and_then(Value::as_f64).unwrap_or_default()
}

fn field_u64(value: &Value, field: &str) -> u64 {
    value.get(field).and_then(Value::as_u64).unwrap_or_default()
}

fn field_bool(value: &Value, field: &str) -> bool {
    value
        .get(field)
        .and_then(Value::as_bool)
        .unwrap_or_default()
}

fn field_opt_str<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

fn assert_null_field(value: &Value, field: &str, row_label: &str) {
    match value.get(field) {
        Some(Value::Null) => {}
        other => panic!("{row_label} must serialize {field}=null, got {other:?}"),
    }
}

fn event_row_by_id<'a>(rows: &'a [Value], detection_id: &str) -> &'a Value {
    rows.iter()
        .find(|row| field_str(row, "observation_id") == detection_id)
        .unwrap_or_else(|| panic!("events response must include detection row {detection_id}"))
}

fn assert_event_row_matches_transport(row: &Value, expected: &EventRow) {
    assert_eq!(
        field_str(row, "observation_id"),
        expected.observation_id,
        "served event row observation_id must value-equal review_events; this id is the renderer's /why join"
    );
    assert_eq!(field_str(row, "observed_at"), expected.observed_at);
    assert_eq!(field_str(row, "camera_name"), expected.camera_name);
    assert_eq!(field_str(row, "class_name"), expected.class_name);
    assert!(
        (field_f64(row, "confidence") - expected.confidence).abs() < f64::EPSILON,
        "served confidence must value-equal review_events"
    );
    assert_eq!(field_str(row, "bbox"), expected.bbox);
    assert_eq!(field_u64(row, "frame_index"), expected.frame_index);
    assert_eq!(
        field_bool(row, "correction_recorded"),
        expected.correction_recorded
    );
    assert_eq!(field_bool(row, "confirmed"), expected.confirmed);
}

fn first_detection_id(store: &Store) -> String {
    store
        .list_observations(None)
        .expect("list observations")
        .into_iter()
        .find(|observation| observation.observation_type == "detection")
        .expect("seeded store must include a detection observation")
        .id
        .to_string()
}

fn list_all_observations(store: &Store) -> Vec<Observation> {
    store.list_observations(None).expect("list observations")
}

fn internal_media_path(data_dir: &Path, internal_ref: &str) -> PathBuf {
    let name = internal_ref
        .strip_prefix("vigil-edge:clip/")
        .unwrap_or_else(|| {
            panic!("internal media ref must use vigil-edge:clip/, got {internal_ref}")
        });
    data_dir.join("clips").join(name)
}

fn review_store_copy(minimum_observations: usize) -> Result<(TempDir, PathBuf), String> {
    let tmp =
        tempfile::TempDir::new().map_err(|error| format!("create review tmp dir: {error}"))?;
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(data_dir.join("clips"))
        .map_err(|error| format!("create review clips dir: {error}"))?;
    let store_path = data_dir.join("store.contextgraph");
    let store = ha_test_support::open_store_at(&store_path)?;
    seed_review_store(&store, &data_dir, minimum_observations.max(1))?;
    Ok((tmp, store_path))
}

fn seed_review_store(
    store: &Store,
    data_dir: &Path,
    observation_count: usize,
) -> Result<(), String> {
    let context = store
        .create_context(CreateContext {
            name: ha_test_support::SITE_NAME.to_string(),
            labels: vec!["site".to_string()],
            properties: BTreeMap::new(),
        })
        .map_err(|error| format!("create review context: {error}"))?;
    let mut camera_properties = BTreeMap::new();
    camera_properties.insert(
        "rtsp_url".to_string(),
        Value::String("rtsp://user:pass@127.0.0.1/lower-gate".to_string()),
    );
    let camera = store
        .create_entity(CreateEntity {
            entity_type: EntityType::Device,
            name: ha_test_support::CAMERA_NAME.to_string(),
            properties: camera_properties,
            tags: vec!["camera".to_string()],
            context_id: context.id,
        })
        .map_err(|error| format!("create review camera: {error}"))?;
    let intention = store
        .create_intention(CreateIntention {
            id: None,
            description: format!("watch {}", ha_test_support::CAMERA_NAME),
            status: IntentionStatus::Active,
            origin: IntentionOrigin::Agent,
            context_id: context.id,
            properties: BTreeMap::new(),
            tags: vec!["camera".to_string()],
        })
        .map_err(|error| format!("create review intention: {error}"))?;
    let mut decision_properties = BTreeMap::new();
    decision_properties.insert(
        "model_id".to_string(),
        Value::String(ha_test_support::DETECTOR_MODEL_ID.to_string()),
    );
    decision_properties.insert(
        "threshold".to_string(),
        json!(ha_test_support::DETECTOR_THRESHOLD),
    );
    let decision = store
        .create_decision(CreateDecision {
            decision_type: "detector_config".to_string(),
            description: format!("Run local detector for {}", ha_test_support::CAMERA_NAME),
            reasoning: Vec::new(),
            confidence: Some(0.5),
            intention_ids: vec![intention.id],
            based_on_entity_ids: vec![camera.id],
            basis_fields: None,
            based_on_snapshots: Vec::new(),
            tags: vec!["detector".to_string()],
            properties: decision_properties,
            context_id: context.id,
            precedent_ids: Vec::new(),
            agent_id: None,
        })
        .map_err(|error| format!("create review decision: {error}"))?;

    let base_time = chrono::Utc::now();
    let nodes = ReviewNodes {
        context_id: context.id,
        entity_id: camera.id,
        decision_id: decision.id,
        intention_id: intention.id,
    };
    for index in 0..observation_count {
        seed_review_detection(
            store,
            data_dir,
            &nodes,
            base_time - chrono::Duration::seconds((observation_count - index) as i64),
            index,
        )?;
    }
    Ok(())
}

struct ReviewNodes {
    context_id: context_graph::ContextId,
    entity_id: context_graph::EntityId,
    decision_id: context_graph::DecisionId,
    intention_id: context_graph::IntentionId,
}

fn seed_review_detection(
    store: &Store,
    data_dir: &Path,
    nodes: &ReviewNodes,
    observed_at: chrono::DateTime<chrono::Utc>,
    index: usize,
) -> Result<(), String> {
    let clips_dir = data_dir.join("clips");
    let clip_name = format!("review-fixture-{index}.h264");
    let image_name = format!("review-fixture-{index}.png");
    let clip_path = clips_dir.join(&clip_name);
    let image_path = clips_dir.join(&image_name);
    fs::write(&clip_path, deterministic_clip_bytes(index))
        .map_err(|error| format!("write fixture clip {}: {error}", clip_path.display()))?;
    fs::write(&image_path, PNG_BYTES)
        .map_err(|error| format!("write fixture image {}: {error}", image_path.display()))?;

    let observation_id = ObservationId::new_v7();
    let clip_ref = format!("vigil-edge:clip/{clip_name}");
    let image_ref = format!("vigil-edge:clip/{image_name}");
    let producer = EvidenceProducer {
        system: "vigil".to_string(),
        model_name: ha_test_support::DETECTOR_MODEL_ID.to_string(),
        model_version: "0.1".to_string(),
        pipeline_version: "review-http-test".to_string(),
    };
    let evidence = vec![
        EvidenceRef {
            id: EvidenceId::new_v7(),
            observation_id,
            context_id: nodes.context_id,
            kind: EvidenceKind::VideoSegment,
            source_ref: clip_ref.clone(),
            mime_type: Some("video/h264".to_string()),
            captured_at: Some(observed_at),
            producer: producer.clone(),
            retention_status: RetentionStatus::RetainedExternal,
            ..Default::default()
        },
        EvidenceRef {
            id: EvidenceId::new_v7(),
            observation_id,
            context_id: nodes.context_id,
            kind: EvidenceKind::ImageFrame,
            source_ref: image_ref.clone(),
            mime_type: Some("image/png".to_string()),
            captured_at: Some(observed_at),
            frame_index: Some(7 + index as u64),
            producer,
            retention_status: RetentionStatus::RetainedExternal,
            ..Default::default()
        },
    ];
    let mut observed_properties = BTreeMap::new();
    observed_properties.insert("class".to_string(), Value::String("person".to_string()));
    observed_properties.insert(
        "confidence".to_string(),
        json!(0.82 + (index as f64 * 0.01)),
    );
    observed_properties.insert(
        "bbox".to_string(),
        Value::String(format!("{},{},{},{}", 10 + index, 20, 30, 40)),
    );
    observed_properties.insert("frame_index".to_string(), json!(7_u64 + index as u64));
    observed_properties.insert(
        "detector_decision_id".to_string(),
        Value::String(nodes.decision_id.to_string()),
    );
    let mut properties = BTreeMap::new();
    properties.insert(
        "baseline_intention_id".to_string(),
        Value::String(nodes.intention_id.to_string()),
    );
    properties.insert(
        "detector_evidence_ref".to_string(),
        Value::String(image_ref),
    );
    properties.insert("clip_frame_count".to_string(), json!(12_u64));
    store
        .record_observation(RecordObservation {
            id: observation_id,
            entity_id: nodes.entity_id,
            context_id: nodes.context_id,
            observation_type: "detection".to_string(),
            source: "vigil".to_string(),
            observed_at,
            evidence,
            observed_properties,
            state_delta: BTreeMap::new(),
            properties,
            embeddings: Vec::new(),
        })
        .map_err(|error| format!("record review detection: {error}"))?;
    Ok(())
}

fn deterministic_clip_bytes(seed: usize) -> Vec<u8> {
    (0..4096)
        .map(|offset| ((offset + seed * 17) % 251) as u8)
        .collect()
}

const PNG_BYTES: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

fn first_expected_media_paths(store: &Store, data_dir: &Path) -> (PathBuf, PathBuf) {
    let events = review_events(store, 100).expect("review_events must succeed");
    let row = events
        .rows
        .first()
        .expect("seeded store must include event row");
    (
        internal_media_path(data_dir, &row.detector_image_ref),
        internal_media_path(data_dir, &row.clip_ref),
    )
}

fn served_media_refs(row: &Value) -> (String, String) {
    (
        field_str(row, "detector_image_ref"),
        field_str(row, "clip_ref"),
    )
}

fn assert_allow_origin(response: &HttpResponse, origin: &str, label: &str) {
    let Some(value) = response.header("access-control-allow-origin") else {
        panic!("{label} response must include Access-Control-Allow-Origin for browser fetches");
    };
    assert!(
        value == "*" || value == origin,
        "{label} allow-origin must permit {origin}, got {value}"
    );
}

fn correction_matches(value: &Value, correction_type: &str, label: Option<&str>) -> bool {
    value
        .get("corrections")
        .and_then(Value::as_array)
        .map(|items| {
            items.iter().any(|item| {
                field_str(item, "correction_type") == correction_type
                    && item.get("label").and_then(Value::as_str) == label
            })
        })
        .unwrap_or(false)
}

fn correction_count(value: &Value, correction_type: &str, label: Option<&str>) -> usize {
    value
        .get("corrections")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    field_str(item, "correction_type") == correction_type
                        && item.get("label").and_then(Value::as_str) == label
                })
                .count()
        })
        .unwrap_or_default()
}

fn count_corrections_anchored(store: &Store, detection_id: &str, correction_type: &str) -> usize {
    list_all_observations(store)
        .iter()
        .filter(|observation| {
            let anchored = observation
                .observed_properties
                .get("anchored_detection_id")
                .or_else(|| observation.properties.get("anchored_detection_id"))
                .and_then(Value::as_str);
            let ct = observation
                .observed_properties
                .get("correction_type")
                .or_else(|| observation.properties.get("correction_type"))
                .and_then(Value::as_str);
            anchored == Some(detection_id) && ct == Some(correction_type)
        })
        .count()
}

fn record_marker_detection(
    store: &Store,
    context_id: context_graph::ContextId,
    entity_id: context_graph::EntityId,
    class_name: &str,
    source_ref: &str,
) {
    let observation_id = ObservationId::new_v7();
    let observed_at = chrono::Utc::now();
    let evidence = vec![EvidenceRef {
        id: EvidenceId::new_v7(),
        observation_id,
        context_id,
        kind: EvidenceKind::StructuredSignal,
        source_ref: source_ref.to_string(),
        captured_at: Some(observed_at),
        producer: EvidenceProducer {
            system: "vigil-test".to_string(),
            pipeline_version: "review-http-test".to_string(),
            ..Default::default()
        },
        retention_status: RetentionStatus::NotStored,
        ..Default::default()
    }];
    let mut observed_properties = BTreeMap::new();
    observed_properties.insert("class".to_string(), Value::String(class_name.to_string()));
    observed_properties.insert("confidence".to_string(), json!(0.42));
    observed_properties.insert("bbox".to_string(), Value::String("1,2,3,4".to_string()));
    observed_properties.insert("frame_index".to_string(), json!(7_u64));
    store
        .record_observation(RecordObservation {
            id: observation_id,
            entity_id,
            context_id,
            observation_type: "detection".to_string(),
            source: "vigil-test".to_string(),
            observed_at,
            evidence,
            observed_properties,
            state_delta: BTreeMap::new(),
            properties: BTreeMap::new(),
            embeddings: Vec::new(),
        })
        .expect("record marker detection");
}

fn seed_foreign_context_detection(store: &Store) {
    let context = store
        .create_context(CreateContext {
            name: "foreign site".to_string(),
            labels: vec!["site".to_string()],
            properties: BTreeMap::new(),
        })
        .expect("create foreign context");
    let entity = store
        .create_entity(CreateEntity {
            entity_type: EntityType::Device,
            name: "foreign camera".to_string(),
            properties: BTreeMap::new(),
            tags: vec!["camera".to_string()],
            context_id: context.id,
        })
        .expect("create foreign camera");
    record_marker_detection(
        store,
        context.id,
        entity.id,
        "foreign-marker",
        "vigil-edge:test/foreign-marker",
    );
}

fn seed_no_media_detection_in_review_context(store: &Store) {
    let events = review_events(store, 100).expect("review_events must locate reviewed context");
    let first = events
        .rows
        .first()
        .expect("seeded store must include reviewed event");
    let uuid = uuid::Uuid::parse_str(&first.observation_id).expect("event id must be uuid");
    let observation = store
        .get_observation(ObservationId::from(uuid))
        .expect("get reviewed observation");
    record_marker_detection(
        store,
        observation.context_id,
        observation.entity_id,
        "no-media-marker",
        "vigil-edge:signal/no-media-marker",
    );
}

fn is_4xx(status: u16) -> bool {
    (400..500).contains(&status)
}

fn port_accepts(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}

#[test]
fn event_list_serves_full_review_row_fieldset() {
    let (_tmp, store_path) = review_store_copy(2).expect("seeded store for event list fieldset");
    let expected_store = ha_test_support::open_store_at(&store_path).expect("expected store opens");
    let (_server, port) = spawn_server(&store_path);

    let response = get(port, "/events");
    assert_eq!(response.status, 200, "GET /events must return 200");
    let served = rows(&response);
    let expected = review_events(&expected_store, 100).expect("review_events must succeed");

    assert_eq!(
        served.len(),
        expected.rows.len(),
        "served event list length must equal review_events before per-row indexing"
    );
    for (served_row, expected_row) in served.iter().zip(expected.rows.iter()) {
        assert_event_row_matches_transport(served_row, expected_row);
    }
    assert!(
        served
            .iter()
            .any(|row| field_str(row, "class_name") == "person"),
        "seeded person class must be visible in the served event list"
    );
    assert!(
        served
            .iter()
            .any(|row| field_str(row, "camera_name") == ha_test_support::CAMERA_NAME),
        "seeded camera must be visible in the served event list"
    );
    assert!(
        served.iter().any(|row| {
            (field_f64(row, "confidence") - expected.rows[0].confidence).abs() < f64::EPSILON
        }),
        "stored confidence must be visible in the served event list"
    );

    let served_id = field_str(&served[0], "observation_id");
    let why_response = get(port, &format!("/why/{served_id}"));
    assert_eq!(
        why_response.status, 200,
        "served event id must be a click-through join to /why"
    );
    let why = json_body(&why_response);
    assert_eq!(
        field_str(&why, "observation_id"),
        served_id,
        "GET /why/{{served observation_id}} must return the same observation_id"
    );
}

#[test]
fn why_walk_back_serves_provenance_excludes_rtsp_url_and_lists_correction() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for why walk-back");
    let store = ha_test_support::open_store_at(&store_path).expect("store opens");
    let detection_id = first_detection_id(&store);
    record_correction(
        &store,
        CorrectionRequest {
            detection_id: detection_id.clone(),
            label: Some("vehicle".to_string()),
            correction_type: CorrectionType::WrongClass,
        },
    )
    .expect("direct correction write succeeds");
    let (_server, port) = spawn_server(&store_path);

    let response = get(port, &format!("/why/{detection_id}"));
    assert_eq!(response.status, 200, "GET /why/{{id}} must return 200");
    let served = json_body(&response);
    let expected = review_why(&store, &detection_id).expect("review_why must succeed");

    assert_eq!(
        field_str(&served, "observation_id"),
        expected.observation_id
    );
    assert_eq!(field_str(&served, "observed_at"), expected.observed_at);
    assert_eq!(field_str(&served, "model_id"), expected.model_id);
    assert!(
        (field_f64(&served, "threshold") - expected.threshold).abs() < f64::EPSILON,
        "threshold must value-equal review_why"
    );
    assert_eq!(field_str(&served, "class_name"), expected.class_name);
    assert!(
        (field_f64(&served, "confidence") - expected.confidence).abs() < f64::EPSILON,
        "confidence must value-equal review_why"
    );
    assert_eq!(field_str(&served, "bbox"), expected.bbox);
    assert_eq!(field_u64(&served, "frame_index"), expected.frame_index);
    assert_eq!(field_str(&served, "decision_id"), expected.decision_id);
    assert_eq!(field_str(&served, "intention_id"), expected.intention_id);
    assert_eq!(
        field_str(&served, "intention_description"),
        expected.intention_description
    );
    assert_eq!(field_str(&served, "camera_name"), expected.camera_name);
    assert_eq!(field_str(&served, "site_name"), expected.site_name);
    assert!(
        correction_matches(&served, "WrongClass", Some("vehicle")),
        "served why walk-back must list the just-recorded WrongClass vehicle correction"
    );
    let served_text = response.body_text();
    if !expected.camera_rtsp_url.is_empty() {
        assert!(
            !served_text.contains(&expected.camera_rtsp_url),
            "served /why must exclude the credential-capable camera RTSP URL"
        );
    }
    assert!(
        !served_text.contains("rtsp://"),
        "served /why must not expose an RTSP URL substring"
    );
}

#[test]
fn snapshot_read_serves_image_bytes_with_image_content_type() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for snapshot read");
    let data_dir = store_path.parent().expect("data dir").to_path_buf();
    let expected_store = ha_test_support::open_store_at(&store_path).expect("expected store opens");
    let (snapshot_path, _) = first_expected_media_paths(&expected_store, &data_dir);
    let (_server, port) = spawn_server(&store_path);

    let event_response = get(port, "/events");
    let served = rows(&event_response);
    assert!(
        !served.is_empty(),
        "events response must include a row before extracting snapshot ref"
    );
    let (snapshot_ref, _) = served_media_refs(&served[0]);
    let snapshot_response = get(port, &snapshot_ref);

    assert_eq!(
        snapshot_response.status, 200,
        "snapshot ref must return 200"
    );
    let content_type = snapshot_response.header("content-type").unwrap_or_default();
    assert!(
        content_type.starts_with("image/"),
        "snapshot must be served with image/* content-type, got {content_type}"
    );
    let expected_snapshot_len = snapshot_response.body.len().to_string();
    assert_eq!(
        snapshot_response.header("content-length"),
        Some(expected_snapshot_len.as_str()),
        "snapshot response must carry Content-Length so HA/browser proxies can stream it without treating it as an indeterminate body"
    );
    assert!(
        snapshot_response.body.starts_with(b"\x89PNG\r\n"),
        "snapshot body must begin with PNG magic bytes"
    );
    assert_eq!(
        snapshot_response.body,
        fs::read(&snapshot_path).expect("read expected snapshot bytes"),
        "snapshot body must equal the on-disk detector image"
    );
}

#[test]
fn large_media_reads_use_fixed_content_length_not_chunked_transfer() {
    let (_tmp, store_path) =
        review_store_copy(1).expect("seeded store for large media fixed-length read");
    let data_dir = store_path.parent().expect("data dir").to_path_buf();
    let expected_store = ha_test_support::open_store_at(&store_path).expect("expected store opens");
    let (snapshot_path, clip_path) = first_expected_media_paths(&expected_store, &data_dir);
    let mut large_snapshot = PNG_BYTES.to_vec();
    large_snapshot.extend((0..(64 * 1024)).map(|offset| (offset % 251) as u8));
    let large_clip = (0..(96 * 1024))
        .map(|offset| ((offset * 3) % 251) as u8)
        .collect::<Vec<_>>();
    fs::write(&snapshot_path, &large_snapshot).expect("write large snapshot fixture");
    fs::write(&clip_path, &large_clip).expect("write large clip fixture");

    let (_server, port) = spawn_server(&store_path);
    let served = rows(&get(port, "/events"));
    let (snapshot_ref, clip_ref) = served_media_refs(&served[0]);

    let snapshot = get(port, &snapshot_ref);
    let expected_snapshot_len = large_snapshot.len().to_string();
    assert_eq!(snapshot.status, 200, "large snapshot must return 200");
    assert_eq!(
        snapshot.header("content-length"),
        Some(expected_snapshot_len.as_str()),
        "large snapshot must carry Content-Length for HA/browser proxying"
    );
    assert_eq!(
        snapshot.header("transfer-encoding"),
        None,
        "large snapshot must not switch to chunked transfer"
    );
    assert_eq!(snapshot.body, large_snapshot);

    let clip = get(port, &clip_ref);
    let expected_clip_len = large_clip.len().to_string();
    assert_eq!(clip.status, 200, "large full clip read must return 200");
    assert_eq!(
        clip.header("content-length"),
        Some(expected_clip_len.as_str()),
        "large full clip read must carry Content-Length for HA/browser proxying"
    );
    assert_eq!(
        clip.header("transfer-encoding"),
        None,
        "large full clip read must not switch to chunked transfer"
    );
    assert_eq!(clip.body, large_clip);
}

#[test]
fn clip_read_serves_range_206_partial_content_transport() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for clip range read");
    let data_dir = store_path.parent().expect("data dir").to_path_buf();
    let expected_store = ha_test_support::open_store_at(&store_path).expect("expected store opens");
    let (_, clip_path) = first_expected_media_paths(&expected_store, &data_dir);
    let clip_bytes = fs::read(&clip_path).expect("read expected clip bytes");
    assert!(
        clip_bytes.len() > 1024,
        "seeded clip must be larger than 1 KiB"
    );
    let clip_size = clip_bytes.len();
    let (_server, port) = spawn_server(&store_path);
    let event_response = get(port, "/events");
    let served = rows(&event_response);
    let (_, clip_ref) = served_media_refs(&served[0]);

    let first_range = get_with_headers(port, &clip_ref, &[("Range", "bytes=0-1023".to_string())]);
    assert_eq!(
        first_range.status, 206,
        "clip range bytes=0-1023 must return 206"
    );
    assert_eq!(
        first_range.header("content-range"),
        Some(format!("bytes 0-1023/{clip_size}").as_str())
    );
    assert_eq!(first_range.header("accept-ranges"), Some("bytes"));
    assert!(
        first_range
            .header("content-type")
            .unwrap_or_default()
            .starts_with("video/"),
        "clip range must be served as video/*"
    );
    assert_eq!(first_range.header("content-length"), Some("1024"));
    assert_eq!(&first_range.body, &clip_bytes[..1024]);

    let tail_start = clip_size - 256;
    let tail = get_with_headers(
        port,
        &clip_ref,
        &[("Range", format!("bytes={tail_start}-"))],
    );
    assert_eq!(tail.status, 206, "tail clip range must return 206");
    assert_eq!(
        tail.header("content-range"),
        Some(format!("bytes {tail_start}-{}/{clip_size}", clip_size - 1).as_str())
    );
    assert_eq!(&tail.body, &clip_bytes[tail_start..]);

    let full = get(port, &clip_ref);
    assert_eq!(full.status, 200, "full clip read must return 200");
    assert_eq!(full.header("accept-ranges"), Some("bytes"));
    assert_eq!(full.body, clip_bytes);
}

#[test]
fn cross_origin_allow_header_present_on_event_why_and_media_reads() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for CORS reads");
    let (_server, port) = spawn_server(&store_path);
    let origin = "http://vigil-card.test:8123";
    let origin_header = [("Origin", origin.to_string())];

    let events = get_with_headers(port, "/events", &origin_header);
    let served = rows(&events);
    let id = field_str(&served[0], "observation_id");
    let (snapshot_ref, clip_ref) = served_media_refs(&served[0]);
    let why = get_with_headers(port, &format!("/why/{id}"), &origin_header);
    let snapshot = get_with_headers(port, &snapshot_ref, &origin_header);
    let clip = get_with_headers(port, &clip_ref, &origin_header);

    assert_allow_origin(&events, origin, "events");
    assert_allow_origin(&why, origin, "why");
    assert_allow_origin(&snapshot, origin, "snapshot");
    assert_allow_origin(&clip, origin, "clip");
}

#[test]
fn media_reference_from_event_list_resolves_to_media_bytes() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for media ref resolution");
    let data_dir = store_path.parent().expect("data dir").to_path_buf();
    let expected_store = ha_test_support::open_store_at(&store_path).expect("expected store opens");
    let (snapshot_path, clip_path) = first_expected_media_paths(&expected_store, &data_dir);
    let (_server, port) = spawn_server(&store_path);

    let events = get(port, "/events");
    let served = rows(&events);
    let (snapshot_ref, clip_ref) = served_media_refs(&served[0]);
    assert!(
        !snapshot_ref.starts_with("vigil-edge:clip/") && !clip_ref.starts_with("vigil-edge:clip/"),
        "served media refs must be browser-resolvable, not the internal vigil-edge scheme"
    );

    let snapshot = get(port, &snapshot_ref);
    assert_eq!(snapshot.status, 200, "served snapshot ref must resolve");
    assert!(
        snapshot
            .header("content-type")
            .unwrap_or_default()
            .starts_with("image/"),
        "served snapshot ref must return image bytes"
    );
    assert_eq!(
        snapshot.body,
        fs::read(snapshot_path).expect("read snapshot")
    );

    let clip = get(port, &clip_ref);
    assert_eq!(clip.status, 200, "served clip ref must resolve");
    assert!(
        clip.header("content-type")
            .unwrap_or_default()
            .starts_with("video/"),
        "served clip ref must return video bytes"
    );
    assert_eq!(clip.body, fs::read(clip_path).expect("read clip"));
}

#[test]
fn http_correction_post_lands_through_record_correction_seam() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for HTTP correction");
    let store = ha_test_support::open_store_at(&store_path).expect("store opens");
    let detection_id = first_detection_id(&store);
    let count_before = list_all_observations(&store).len();
    let (server, port) = spawn_server(&store_path);

    let body = format!(r#"{{"detection_id":"{detection_id}","correction_type":"FalseAlarm"}}"#);
    let response = post_json(port, "/correction", &body);
    assert!(
        (200..300).contains(&response.status),
        "valid HTTP correction POST must return success"
    );
    drop(store);
    server.shutdown();

    let fresh_store = ha_test_support::open_store_at(&store_path).expect("fresh store reopens");
    let observations_after = list_all_observations(&fresh_store);
    assert_eq!(
        observations_after.len(),
        count_before + 1,
        "HTTP correction must write exactly one new cg observation through record_correction"
    );
    assert_eq!(
        count_corrections_anchored(&fresh_store, &detection_id, "FalseAlarm"),
        1,
        "fresh cg handle must read the HTTP FalseAlarm correction anchored to the detection"
    );

    let (_server2, port2) = spawn_server(&store_path);
    let why = json_body(&get(port2, &format!("/why/{detection_id}")));
    assert!(
        correction_matches(&why, "FalseAlarm", None),
        "/why must list the FalseAlarm correction written by HTTP"
    );
}

#[test]
fn http_correction_post_labelled_wrong_class_survives_to_why_and_is_idempotent() {
    let (_tmp, store_path) =
        review_store_copy(1).expect("seeded store for labelled HTTP correction");
    let store = ha_test_support::open_store_at(&store_path).expect("store opens");
    let detection_id = first_detection_id(&store);
    let (_server, port) = spawn_server(&store_path);

    let wrong_class = format!(
        r#"{{"detection_id":"{detection_id}","correction_type":"WrongClass","label":"vehicle"}}"#
    );
    let response = post_json(port, "/correction", &wrong_class);
    assert!((200..300).contains(&response.status));
    let why = json_body(&get(port, &format!("/why/{detection_id}")));
    assert_eq!(
        correction_count(&why, "WrongClass", Some("vehicle")),
        1,
        "/why must list exactly one WrongClass vehicle correction after first POST"
    );

    let identity = format!(
        r#"{{"detection_id":"{detection_id}","correction_type":"Identity","label":"delivery van"}}"#
    );
    let response = post_json(port, "/correction", &identity);
    assert!((200..300).contains(&response.status));
    let why = json_body(&get(port, &format!("/why/{detection_id}")));
    assert!(
        correction_matches(&why, "Identity", Some("delivery van")),
        "/why must list Identity labels carried over HTTP"
    );

    let response = post_json(port, "/correction", &wrong_class);
    assert!((200..300).contains(&response.status));
    let why = json_body(&get(port, &format!("/why/{detection_id}")));
    assert_eq!(
        correction_count(&why, "WrongClass", Some("vehicle")),
        1,
        "repeat WrongClass vehicle POST must be idempotent"
    );
}

#[test]
fn event_list_serializes_current_correction_authority_fields() {
    let (_tmp, store_path) =
        review_store_copy(4).expect("seeded store for event-list correction authority fields");
    let store = ha_test_support::open_store_at(&store_path).expect("store opens");
    let mut detections = list_all_observations(&store)
        .into_iter()
        .filter(|observation| observation.observation_type == "detection")
        .collect::<Vec<_>>();
    assert!(
        detections.len() >= 4,
        "at least four detections are needed to cover Identity, latest WrongClass, FalseAlarm, and unreviewed rows"
    );
    detections.sort_by_key(|observation| observation.observed_at);
    let identity_id = detections[0].id.to_string();
    let wrong_class_id = detections[1].id.to_string();
    let false_alarm_id = detections[2].id.to_string();
    let unreviewed_id = detections[3].id.to_string();
    let (_server, port) = spawn_server(&store_path);

    let identity = format!(
        r#"{{"detection_id":"{identity_id}","correction_type":"Identity","label":"operator confirmed"}}"#
    );
    let response = post_json(port, "/correction", &identity);
    assert!((200..300).contains(&response.status));

    let initial_identity = format!(
        r#"{{"detection_id":"{wrong_class_id}","correction_type":"Identity","label":"initial confirmation"}}"#
    );
    let response = post_json(port, "/correction", &initial_identity);
    assert!((200..300).contains(&response.status));
    thread::sleep(Duration::from_millis(2));
    let wrong_class = format!(
        r#"{{"detection_id":"{wrong_class_id}","correction_type":"WrongClass","label":"cow"}}"#
    );
    let response = post_json(port, "/correction", &wrong_class);
    assert!((200..300).contains(&response.status));

    let false_alarm =
        format!(r#"{{"detection_id":"{false_alarm_id}","correction_type":"FalseAlarm"}}"#);
    let response = post_json(port, "/correction", &false_alarm);
    assert!((200..300).contains(&response.status));

    let response = get(port, "/events");
    assert_eq!(response.status, 200, "GET /events must return 200");
    let served = rows(&response);

    let identity_row = event_row_by_id(&served, &identity_id);
    assert_eq!(
        field_opt_str(identity_row, "current_correction"),
        Some("Identity"),
        "Identity-reviewed row must serialize current_correction as PascalCase Identity"
    );
    assert_null_field(identity_row, "corrected_label", "Identity-reviewed row");

    let wrong_class_row = event_row_by_id(&served, &wrong_class_id);
    assert_eq!(
        field_opt_str(wrong_class_row, "current_correction"),
        Some("WrongClass"),
        "when multiple corrections exist, /events must serialize the latest server-decided correction type"
    );
    assert_eq!(
        field_opt_str(wrong_class_row, "corrected_label"),
        Some("cow"),
        "WrongClass row must serialize corrected_label for Home Assistant card reload"
    );

    let false_alarm_row = event_row_by_id(&served, &false_alarm_id);
    assert_eq!(
        field_opt_str(false_alarm_row, "current_correction"),
        Some("FalseAlarm"),
        "FalseAlarm-reviewed row must serialize current_correction as PascalCase FalseAlarm"
    );
    assert_null_field(
        false_alarm_row,
        "corrected_label",
        "FalseAlarm-reviewed row",
    );

    let unreviewed_row = event_row_by_id(&served, &unreviewed_id);
    assert_null_field(unreviewed_row, "current_correction", "unreviewed row");
    assert_null_field(unreviewed_row, "corrected_label", "unreviewed row");
}

#[test]
fn correction_cors_preflight_allows_post_and_content_type() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for correction preflight");
    let (_server, port) = spawn_server(&store_path);
    let origin = "http://vigil-card.test:8123";
    let response = request(
        port,
        "OPTIONS",
        "/correction",
        &[
            ("Origin", origin.to_string()),
            ("Access-Control-Request-Method", "POST".to_string()),
            ("Access-Control-Request-Headers", "content-type".to_string()),
        ],
        b"",
    );
    let methods = response
        .header("access-control-allow-methods")
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        methods.split(',').any(|method| method.trim() == "post"),
        "correction preflight must allow POST"
    );
    let headers = response
        .header("access-control-allow-headers")
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        headers
            .split(',')
            .any(|header| header.trim() == "content-type"),
        "correction preflight must allow content-type"
    );
    assert_allow_origin(&response, origin, "correction preflight");
}

#[test]
fn pruned_media_returns_clean_not_found_while_event_still_lists_and_walks_back() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for pruned media");
    let data_dir = store_path.parent().expect("data dir").to_path_buf();
    let store = ha_test_support::open_store_at(&store_path).expect("store opens");
    let detection_id = first_detection_id(&store);
    let (snapshot_path, clip_path) = first_expected_media_paths(&store, &data_dir);
    fs::remove_file(snapshot_path).expect("delete snapshot to simulate retention");
    fs::remove_file(clip_path).expect("delete clip to simulate retention");
    let (_server, port) = spawn_server(&store_path);

    let event_response = get(port, "/events");
    assert_eq!(event_response.status, 200);
    let served = rows(&event_response);
    assert!(
        !served.is_empty(),
        "event must still list after media files are pruned"
    );
    let (snapshot_ref, clip_ref) = served_media_refs(&served[0]);
    let snapshot = get(port, &snapshot_ref);
    assert!(
        matches!(snapshot.status, 404 | 410),
        "pruned snapshot must return 404 or 410, not {}",
        snapshot.status
    );
    let clip = get(port, &clip_ref);
    assert!(
        matches!(clip.status, 404 | 410),
        "pruned clip must return 404 or 410, not {}",
        clip.status
    );
    let why = get(port, &format!("/why/{detection_id}"));
    assert_eq!(why.status, 200, "why walk-back must still resolve");
    assert!(
        !why.body.is_empty(),
        "why walk-back must remain non-empty after media pruning"
    );
}

#[test]
fn event_list_does_not_leak_foreign_context_events() {
    let (_tmp, store_path) = review_store_copy(2).expect("seeded store for context isolation");
    let store = ha_test_support::open_store_at(&store_path).expect("store opens");
    seed_foreign_context_detection(&store);
    let (_server, port) = spawn_server(&store_path);

    let response = get(port, "/events");
    assert_eq!(response.status, 200);
    let served = rows(&response);
    let expected = review_events(&store, 100).expect("review_events must succeed");
    let served_ids = served
        .iter()
        .map(|row| field_str(row, "observation_id"))
        .collect::<BTreeSet<_>>();
    let expected_ids = expected
        .rows
        .iter()
        .map(|row| row.observation_id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        served_ids, expected_ids,
        "served event ids must be exactly the context-scoped review_events ids"
    );

    let expected_has_foreign = expected
        .rows
        .iter()
        .any(|row| row.class_name == "foreign-marker");
    if !expected_has_foreign {
        assert!(
            !served
                .iter()
                .any(|row| field_str(row, "class_name") == "foreign-marker"),
            "foreign context marker must not leak when review_events did not select that context"
        );
    }
}

#[test]
fn event_list_ordering_matches_review_events_newest_first() {
    let (_tmp, store_path) = review_store_copy(3).expect("seeded store for event ordering");
    let store = ha_test_support::open_store_at(&store_path).expect("store opens");
    let (_server, port) = spawn_server(&store_path);

    let response = get(port, "/events");
    assert_eq!(response.status, 200);
    let served = rows(&response);
    let expected = review_events(&store, 100).expect("review_events must succeed");
    assert_eq!(
        served.len(),
        expected.rows.len(),
        "served event list length must equal review_events before sequence comparison"
    );
    let served_ids = served
        .iter()
        .map(|row| field_str(row, "observation_id"))
        .collect::<Vec<_>>();
    let expected_ids = expected
        .rows
        .iter()
        .map(|row| row.observation_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        served_ids, expected_ids,
        "served event list order must match review_events newest-first order"
    );
}

#[test]
fn why_unknown_or_malformed_detection_id_returns_not_found() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for unknown why");
    let (_server, port) = spawn_server(&store_path);

    let unknown = get(port, "/why/00000000-0000-0000-0000-000000000001");
    assert_eq!(
        unknown.status, 404,
        "unknown well-formed detection id must return clean 404"
    );
    let malformed = get(port, "/why/not-a-uuid-at-all");
    assert!(
        matches!(malformed.status, 400 | 404),
        "malformed detection id must return 400 or 404, not {}",
        malformed.status
    );
}

#[test]
fn media_read_for_event_with_no_media_returns_not_found() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for no-media event");
    let store = ha_test_support::open_store_at(&store_path).expect("store opens");
    seed_no_media_detection_in_review_context(&store);
    let (_server, port) = spawn_server(&store_path);

    let response = get(port, "/events");
    assert_eq!(response.status, 200);
    let served = rows(&response);
    let row = served
        .iter()
        .find(|row| field_str(row, "class_name") == "no-media-marker");
    assert!(
        row.is_some(),
        "served events must include the seeded no-media marker row"
    );
    let row = row.unwrap();
    let (snapshot_ref, clip_ref) = served_media_refs(row);
    let media_ref = if snapshot_ref.is_empty() {
        clip_ref
    } else {
        snapshot_ref
    };
    let media = get(port, &media_ref);
    assert_eq!(
        media.status, 404,
        "empty or absent media ref for a no-media event must return clean 404"
    );
}

#[test]
fn clip_unsatisfiable_range_returns_416_with_content_range_star() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for unsatisfiable range");
    let data_dir = store_path.parent().expect("data dir").to_path_buf();
    let store = ha_test_support::open_store_at(&store_path).expect("store opens");
    let (_, clip_path) = first_expected_media_paths(&store, &data_dir);
    let clip_size = fs::metadata(&clip_path).expect("clip metadata").len();
    let (_server, port) = spawn_server(&store_path);
    let served = rows(&get(port, "/events"));
    let (_, clip_ref) = served_media_refs(&served[0]);

    let response = get_with_headers(
        port,
        &clip_ref,
        &[("Range", format!("bytes={clip_size}-{}", clip_size + 100))],
    );
    assert_eq!(response.status, 416, "over-EOF clip range must return 416");
    assert_eq!(
        response.header("content-range"),
        Some(format!("bytes */{clip_size}").as_str()),
        "416 response must carry Content-Range: bytes */S"
    );
    assert!(
        response.body.len() < 1024,
        "416 must not return a full media body"
    );
}

#[test]
fn media_path_traversal_rejected_serves_no_out_of_tree_bytes() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for traversal rejection");
    let (_server, port) = spawn_server(&store_path);
    let served = rows(&get(port, "/events"));
    let (_, clip_ref) = served_media_refs(&served[0]);
    let prefix = clip_ref
        .rfind('/')
        .map(|idx| &clip_ref[..=idx])
        .unwrap_or("/");
    let traversal_refs = vec![
        format!("{prefix}../../../../etc/passwd"),
        format!("{prefix}%2e%2e/%2e%2e/%2e%2e/%2e%2e/etc/passwd"),
        format!("{prefix}/etc/passwd"),
    ];

    for traversal_ref in traversal_refs {
        let response = get(port, &traversal_ref);
        assert!(
            matches!(response.status, 400 | 404),
            "traversal-shaped media ref {traversal_ref} must return 400 or 404, not {}",
            response.status
        );
        let body = response.body_text();
        assert!(
            !body.contains("root:"),
            "traversal response must never contain passwd-shaped out-of-tree bytes"
        );
    }
}

#[test]
fn correction_post_bad_payload_or_unknown_id_and_bad_method_return_4xx_not_5xx() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for correction bad input");
    let store = ha_test_support::open_store_at(&store_path).expect("store opens");
    let count_before = list_all_observations(&store).len();
    let (_server, port) = spawn_server(&store_path);

    let malformed = post_json(port, "/correction", "{not-json");
    assert!(
        is_4xx(malformed.status),
        "malformed correction JSON must return 4xx, not {}",
        malformed.status
    );
    assert!(
        malformed.status < 500,
        "malformed correction JSON must never return 5xx"
    );

    let unknown = post_json(
        port,
        "/correction",
        r#"{"detection_id":"00000000-0000-0000-0000-000000000001","correction_type":"FalseAlarm"}"#,
    );
    assert!(
        is_4xx(unknown.status),
        "unknown correction id must return 4xx, not {}",
        unknown.status
    );
    let fresh = ha_test_support::open_store_at(&store_path).expect("fresh store opens");
    assert_eq!(
        list_all_observations(&fresh).len(),
        count_before,
        "unknown-id correction POST must write zero cg observations"
    );

    let wrong_method = request(port, "POST", "/events", &[], b"");
    assert!(
        is_4xx(wrong_method.status),
        "wrong method on /events must return 4xx, not {}",
        wrong_method.status
    );
    let unknown_route = get(port, "/no-such-route");
    assert_eq!(unknown_route.status, 404, "unknown route must return 404");
}

#[test]
fn clip_range_streams_from_large_file_without_whole_file_transfer() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for large clip range");
    let data_dir = store_path.parent().expect("data dir").to_path_buf();
    let store = ha_test_support::open_store_at(&store_path).expect("store opens");
    let (_, clip_path) = first_expected_media_paths(&store, &data_dir);

    let large_len = 256_u64 * 1024 * 1024;
    let head_marker = (0..1024).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let tail_marker = (0..256)
        .map(|i| 255_u8.saturating_sub((i % 251) as u8))
        .collect::<Vec<_>>();
    {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&clip_path)
            .expect("open clip for sparse growth");
        file.set_len(large_len).expect("grow clip fixture");
        file.seek(SeekFrom::Start(0)).expect("seek clip head");
        file.write_all(&head_marker).expect("write head marker");
        file.seek(SeekFrom::Start(large_len - tail_marker.len() as u64))
            .expect("seek clip tail");
        file.write_all(&tail_marker).expect("write tail marker");
    }

    let (_server, port) = spawn_server(&store_path);
    let served = rows(&get(port, "/events"));
    let (_, clip_ref) = served_media_refs(&served[0]);

    let start = Instant::now();
    let head = request_limited(
        port,
        "GET",
        &clip_ref,
        &[("Range", "bytes=0-1023".to_string())],
        b"",
        2 * 1024 * 1024,
    );
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "small head range against large clip must complete promptly"
    );
    assert_eq!(head.status, 206, "large clip head range must return 206");
    assert_eq!(
        head.header("content-range"),
        Some(format!("bytes 0-1023/{large_len}").as_str())
    );
    assert_eq!(head.header("content-length"), Some("1024"));
    assert_eq!(head.body, head_marker);

    let tail_start = large_len - tail_marker.len() as u64;
    let tail = request_limited(
        port,
        "GET",
        &clip_ref,
        &[("Range", format!("bytes={tail_start}-"))],
        b"",
        2 * 1024 * 1024,
    );
    assert_eq!(tail.status, 206, "large clip tail range must return 206");
    assert_eq!(
        tail.header("content-range"),
        Some(format!("bytes {tail_start}-{}/{large_len}", large_len - 1).as_str())
    );
    assert_eq!(tail.body, tail_marker);
}

#[test]
fn data_plane_serves_in_process_then_stops_on_shutdown() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for shutdown");
    let (server, port) = spawn_server(&store_path);
    let response = get(port, "/events");
    assert_eq!(
        response.status, 200,
        "in-process data plane must serve events"
    );

    server.shutdown();
    thread::sleep(Duration::from_millis(150));
    assert!(
        !port_accepts(port),
        "data-plane shutdown handle must stop the port from accepting new connections"
    );
}

struct ChildGuard {
    child: Child,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn health_liveness_still_serves_alongside_data_plane_in_single_binary() {
    let (_tmp, store_path) = review_store_copy(1).expect("seeded store for binary coexistence");
    let data_dir = store_path.parent().expect("data dir").to_path_buf();
    let config_tmp = tempfile::TempDir::new().expect("config tempdir");
    let config_path = config_tmp.path().join("vigil-review-coexistence.toml");
    let health_port = ha_test_support::free_port().expect("health port");
    let review_port = ha_test_support::free_port().expect("review port");
    fs::write(
        &config_path,
        format!(
            "data_dir = \"{}\"\nstore_path = \"{}\"\nhealth_port = {}\nreview_port = {}\ncameras = []\n",
            ha_test_support::toml_path(&data_dir),
            ha_test_support::toml_path(&store_path),
            health_port,
            review_port,
        ),
    )
    .expect("write config");

    let child = Command::new(ha_test_support::vigil_binary_path())
        .arg("run")
        .arg("--config")
        .arg(&config_path)
        .env("VIGIL_DATA_DIR", &data_dir)
        .env("VIGIL_STORE_PATH", &store_path)
        .env("VIGIL_HEALTH_PORT", health_port.to_string())
        .env("VIGIL_REVIEW_PORT", review_port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn vigil run");
    let _guard = ChildGuard { child };

    ha_test_support::wait_for_tcp_port(health_port, Duration::from_secs(10))
        .expect("health port must open");
    ha_test_support::wait_for_tcp_port(review_port, Duration::from_secs(10))
        .expect("review port must open");

    let health = get(health_port, "/health");
    assert!(
        matches!(health.status, 200 | 503),
        "health endpoint must return liveness status, got {}",
        health.status
    );
    let health_json = json_body(&health);
    assert!(
        health_json.get("status").is_some(),
        "/health must return its status JSON shape"
    );

    let events = get(review_port, "/events");
    assert_eq!(events.status, 200, "review data plane must serve /events");
    let events_json = json_body(&events);
    assert!(
        events_json.as_array().is_some(),
        "/events must be distinguishable from /health by returning an event row list"
    );
    assert!(
        events_json.get("status").is_none(),
        "/events must not hijack the /health status JSON shape"
    );
}
