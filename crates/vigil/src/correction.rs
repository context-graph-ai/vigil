use std::net::TcpStream;

use context_graph::Store;

use crate::live_read::{handle_events_read, handle_why_read};

// ── Public correction types ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CorrectionRequest {
    pub detection_id: String,
    pub label: Option<String>,
    pub correction_type: CorrectionType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorrectionType {
    Identity,
    WrongClass,
    FalseAlarm,
}

#[derive(Debug, Clone)]
pub struct CorrectionReceipt {
    pub correction_id: String,
}

#[derive(Debug)]
pub enum CorrectionError {
    NoAnchor(String),
    WriteFailed(String),
}

impl std::fmt::Display for CorrectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAnchor(msg) => write!(f, "no anchor: {msg}"),
            Self::WriteFailed(msg) => write!(f, "write failed: {msg}"),
        }
    }
}

impl std::error::Error for CorrectionError {}

// ── Public review types ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RecordedCorrection {
    pub label: Option<String>,
    pub correction_type: CorrectionType,
    pub anchored_detection_id: String,
}

#[derive(Debug, Clone)]
pub struct WhyView {
    pub observation_id: String,
    pub observed_at: String,
    pub clip_ref: String,
    pub camera_id: String,
    pub camera_name: String,
    pub camera_rtsp_url: String,
    pub context_id: String,
    pub site_name: String,
    pub decision_id: String,
    pub intention_id: String,
    pub intention_description: String,
    pub model_id: String,
    pub threshold: f64,
    pub class_name: String,
    pub confidence: f64,
    pub bbox: String,
    pub frame_index: u64,
    pub detector_image_ref: String,
    pub corrections: Vec<RecordedCorrection>,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub observation_id: String,
    pub observed_at: String,
    pub camera_name: String,
    pub class_name: String,
    pub confidence: f64,
    pub bbox: String,
    pub frame_index: u64,
    pub clip_ref: String,
    pub detector_image_ref: String,
    pub correction_recorded: bool,
}

#[derive(Debug, Clone)]
pub struct EventsView {
    pub rows: Vec<EventRow>,
}

#[derive(Debug)]
pub enum ReviewError {
    NotFound(String),
    StoreError(String),
}

impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
            Self::StoreError(msg) => write!(f, "store error: {msg}"),
        }
    }
}

impl std::error::Error for ReviewError {}

// ── Wrong stubs ────────────────────────────────────────────────────────────

/// WRONG STUB: writes nothing to cg (correction held in neither cg nor any
/// durable store); always returns Ok carrying a fixed fake receipt id regardless
/// of whether the detection_id resolves, causing:
///   - correction_writes_durably_through_cg_record_path  → no correction in cg → FAIL
///   - correction_held_outside_cg_fails_readback         → no correction in cg → FAIL
///   - correction_survives_daemon_restart                → no correction after reopen → FAIL
///   - record_correction_with_unknown_detection_id_returns_typed_error → Ok not Err → FAIL
///   - correction_and_event_under_disk_full_fail_loudly  → Ok not WriteFailed → FAIL
///
/// Also opens an outbound socket for correction_path_makes_no_outbound_network_beyond_broker.
pub fn record_correction(
    _store: &Store,
    _request: CorrectionRequest,
) -> Result<CorrectionReceipt, CorrectionError> {
    // wrong stub: open an outbound socket — caught by the no-egress monitor test
    let _ = TcpStream::connect("127.0.0.1:19876");
    // wrong stub: return fake receipt without writing anything to cg
    Ok(CorrectionReceipt {
        correction_id: "00000000-0000-0000-0000-000000000042".to_string(),
    })
}

/// WRONG STUB: returns the real provenance walk (keeping vigil_why regression
/// guard passing) but always returns an empty corrections list, causing:
///   - correction_reads_back_via_review_why → corrections empty → FAIL
///   - correction_anchored_to_named_detection_only → corrections empty under B → FAIL
///   - false_alarm_and_wrong_class_corrections_record_and_read_back → empty → FAIL
pub fn review_why(store: &Store, detection_id: &str) -> Result<WhyView, ReviewError> {
    let resp = handle_why_read(store, detection_id).map_err(ReviewError::StoreError)?;
    Ok(WhyView {
        observation_id: resp.observation_id,
        observed_at: resp.observed_at,
        clip_ref: resp.clip_ref,
        camera_id: resp.camera_id,
        camera_name: resp.camera_name,
        camera_rtsp_url: resp.camera_rtsp_url,
        context_id: resp.context_id,
        site_name: resp.site_name,
        decision_id: resp.decision_id,
        intention_id: resp.intention_id,
        intention_description: resp.intention_description,
        model_id: resp.model_id,
        threshold: resp.threshold,
        class_name: resp.class_name,
        confidence: resp.confidence,
        bbox: resp.bbox,
        frame_index: resp.frame_index,
        detector_image_ref: resp.detector_image_ref,
        corrections: Vec::new(), // wrong stub: always empty, never joined from cg
    })
}

/// WRONG STUB: returns the real event rows (keeping vigil_events regression guard
/// passing) but hardcodes correction_recorded = false for every row, causing:
///   - review_events_row_flags_corrected_events → corrected row still false → FAIL
pub fn review_events(store: &Store, limit: usize) -> Result<EventsView, ReviewError> {
    let resp = handle_events_read(store, limit).map_err(ReviewError::StoreError)?;
    let rows = resp
        .rows
        .iter()
        .map(|row| EventRow {
            observation_id: row.observation_id.clone(),
            observed_at: row.observed_at.clone(),
            camera_name: row.camera_name.clone(),
            class_name: row.class_name.clone(),
            confidence: row.confidence,
            bbox: row.bbox.clone(),
            frame_index: row.frame_index,
            clip_ref: row.clip_ref.clone(),
            detector_image_ref: row.detector_image_ref.clone(),
            correction_recorded: false, // wrong stub: always false, never reads from cg
        })
        .collect();
    Ok(EventsView { rows })
}
