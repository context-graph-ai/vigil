//! Correction/event core-behavior tests that need a real `context_graph::Store`
//! (disk-limit write failures, cg durability, event filtering, anchor-type
//! rejection) or a real broker plus real cg data (detection event mapping).
//! None of these construct a `SiteControl` — they call `record_correction`/
//! `review_events`/`review_why`/the adapter's payload functions directly — so
//! they are not blocked by `store_backed_site_control` staying private to
//! core. They moved here from `vigil-ha`'s test suite because proving them
//! needs `context_graph::Store`, and the adapter crate's own manifest must
//! name neither context-graph nor contextdb in any section.

#![cfg(feature = "first-light-acceptance")]
// This file and `mosquitto_fixture.rs` (included below) both declare the
// same shared support modules by path, in separate namespaces — the same
// pattern `ha_mqtt_broker.rs` uses.
#![allow(clippy::duplicate_mod)]

use std::time::Duration;

use vigil::{
    CorrectionError, CorrectionRequest, CorrectionType, record_correction, review_events,
    review_why,
};
use vigil_ha::{DetectionInput, map_detection_to_event_payload, publish_detection_event};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
// Not called directly in this file; re-exported so `mosquitto_fixture.rs`'s
// `crate::wait_until` path resolves when included below.
#[allow(unused_imports)]
use deterministic_fixture_support::wait_until;

#[path = "../../vigil-ha/tests/mqtt_test_probe.rs"]
mod mqtt_test_probe;
use mqtt_test_probe::MqttProbe;

#[path = "../../vigil-ha/tests/mosquitto_fixture.rs"]
mod mosquitto_fixture;
use mosquitto_fixture::MosquittoFixture;

/// RED — wrong stub `record_correction` always returns Ok;
/// a disk-constrained write against a REAL detection must return
/// Err(CorrectionError::WriteFailed).
///
/// Use a real cg detection record so a real ObservationId
/// exists, then constrain the data dir (read-only), then assert Err(WriteFailed) specifically
/// — the empty-store+fake-id path only tests NoAnchor, never the disk-full path.
#[test]
fn correction_and_event_under_disk_full_fail_loudly() {
    let (_tmp, store_path) = deterministic_fixture_support::fresh_store_copy(1)
        .expect("seeded store for correction_and_event_under_disk_full_fail_loudly");

    let detection_id = {
        let s = deterministic_fixture_support::open_store_at(&store_path)
            .expect("open store for detection_id read");
        let obs = s.list_observations(None).expect("list_observations");
        assert!(
            !obs.is_empty(),
            "at least one cg detection must be in the store for disk-full path"
        );
        obs[0].id.to_string()
    };

    // Open the store and cap the database at its current size so the next INSERT fails.
    // contextdb's DiskBudgetExceeded error propagates up through record_observation →
    // record_correction as WriteFailed — this is the real cg write path, not a probe.
    let store = deterministic_fixture_support::open_store_at(&store_path)
        .expect("open store for disk-constrained write");

    let db = store.sync_database();
    let current_size = db
        .disk_file_size()
        .expect("store must be file-backed for disk-limit test");
    db.set_disk_limit(Some(current_size))
        .expect("set disk limit to current file size");

    let req = CorrectionRequest {
        detection_id: detection_id.clone(),
        label: None,
        correction_type: CorrectionType::FalseAlarm,
    };
    let result = record_correction(&store, req);

    // Must return Err(WriteFailed) specifically — a real id exists so NoAnchor is wrong.
    // Wrong stub always returns Ok(receipt) → assertion FAILS.
    assert!(
        matches!(result, Err(CorrectionError::WriteFailed(_))),
        "record_correction must return Err(WriteFailed) when the cg write fails against \
         a disk-limit-capped store with a real detection id; wrong stub always returns Ok — \
         got: {result:?}"
    );
}

/// RED — wrong stub `publish_detection_event` is a no-op;
/// the detection event built from REAL cg data never arrives on the broker topic.
///
/// Resolve one deterministic cg detection through `review_why`, map that exact
/// authority record with the production event mapper, publish it through a real
/// broker, and compare the received id, class, and confidence back to the same
/// `review_why` record. Separate fixtures cannot satisfy the causal join.
#[test]
fn detection_publishes_event_to_real_broker() {
    let (_tmp, store_path) = deterministic_fixture_support::fresh_store_copy(1)
        .expect("seeded store for detection_publishes_event_to_real_broker");
    let broker = MosquittoFixture::start().expect("mosquitto must start for acceptance test");

    // Read detection data from cg authority.
    let store = deterministic_fixture_support::open_store_at(&store_path)
        .expect("open store for detection read");
    let observations = store.list_observations(None).expect("list_observations");
    assert!(
        !observations.is_empty(),
        "at least one detection must be in the store"
    );
    let detection_obs = &observations[0];
    let real_detection_id = detection_obs.id.to_string();
    let why_before = review_why(&store, &real_detection_id)
        .expect("the authoritative cg detection must resolve through review_why");
    let event_payload = map_detection_to_event_payload(&DetectionInput {
        observation_id: why_before.observation_id.clone(),
        camera_name: why_before.camera_name.clone(),
        object_class: why_before.class_name.clone(),
        confidence: why_before.confidence,
        timestamp_ms: detection_obs.observed_at.timestamp_millis(),
        evidence_ref: why_before.clip_ref.clone(),
        snapshot_ref: why_before.detector_image_ref.clone(),
        entity_name: why_before
            .recognition
            .as_ref()
            .map(|recognition| recognition.name.clone()),
        match_score: why_before
            .recognition
            .as_ref()
            .map(|recognition| recognition.score),
    });
    let event_json =
        serde_json::to_string(&event_payload).expect("the production event payload must serialize");
    let topic = format!("vigil/test/{}/detection", why_before.camera_id);

    let mut probe = MqttProbe::connect_and_subscribe(
        &broker.host,
        broker.port,
        &[&topic],
        Duration::from_secs(2),
    )
    .expect("detection probe must receive SUBACK");

    let result = publish_detection_event(&broker.mqtt_config(), &event_json, &topic);
    assert!(
        result.is_ok(),
        "publish_detection_event returned Err: {:?}",
        result.err()
    );

    let received = probe
        .recv_matching(
            "published detection event",
            Duration::from_secs(2),
            |message| message.topic == topic && message.payload == event_json.as_bytes(),
        )
        .expect("publish_detection_event must deliver the value-equal event");
    let received: serde_json::Value =
        serde_json::from_slice(&received.payload).expect("detection event must be JSON");
    let why_after = review_why(&store, &real_detection_id)
        .expect("the broker event detection id must still resolve through review_why");
    assert_eq!(
        received
            .get("detection_id")
            .and_then(serde_json::Value::as_str),
        Some(why_after.observation_id.as_str()),
        "broker detection_id must identify the exact cg /why record"
    );
    assert_eq!(
        received
            .get("object_class")
            .and_then(serde_json::Value::as_str),
        Some(why_after.class_name.as_str()),
        "broker object_class must value-equal the same cg /why record"
    );
    let broker_confidence = received
        .get("confidence")
        .and_then(serde_json::Value::as_f64)
        .expect("broker event must carry numeric confidence");
    assert!(
        (broker_confidence - why_after.confidence).abs() < f64::EPSILON,
        "broker confidence {broker_confidence} must value-equal the same cg /why confidence {}",
        why_after.confidence
    );
}

/// RED — wrong stub `record_correction` writes nothing; after broker drops the
/// correction must be durable in cg (cg is the authority).
///
/// Durability assertion reads via `list_observations` on a fresh store handle
/// (not `review_why`) so the no-shadow requirement is satisfied — an in-process mirror
/// could answer review_why, but only a fresh-opened store proves cg durability.
#[test]
fn broker_drop_does_not_affect_durable_cg_record() {
    let (_tmp, store_path) = deterministic_fixture_support::fresh_store_copy(1)
        .expect("seeded store for broker_drop_does_not_affect_durable_cg_record");
    let mut broker = MosquittoFixture::start().expect("mosquitto");

    let store = deterministic_fixture_support::open_store_at(&store_path).expect("open store");
    let obs = store.list_observations(None).expect("list");
    assert!(!obs.is_empty(), "need at least one detection");
    let detection_id = obs[0].id.to_string();

    let req = CorrectionRequest {
        detection_id: detection_id.clone(),
        label: None,
        correction_type: CorrectionType::FalseAlarm,
    };
    let _ = record_correction(&store, req);

    // Kill the broker — correction must persist in cg regardless of broker state.
    let _ = broker.child.kill();
    let _ = broker.child.wait();

    // drop all in-process state; reopen ONLY the cg Store.
    drop(store);
    let store2 = deterministic_fixture_support::open_store_at(&store_path).expect("reopen store");
    let all_obs = store2
        .list_observations(None)
        .expect("list_observations after broker drop");

    // Find the correction observation anchored to this detection.
    // Wrong stub: record_correction writes nothing → anchored_detection_id absent → FAIL.
    let correction_in_cg = all_obs.iter().any(|o| {
        o.observed_properties
            .get("anchored_detection_id")
            .or_else(|| o.properties.get("anchored_detection_id"))
            .and_then(|v| v.as_str())
            == Some(detection_id.as_str())
    });
    assert!(
        correction_in_cg,
        "correction must persist in cg after broker drop (cg is the authority); \
         wrong stub wrote nothing — anchored_detection_id '{detection_id}' is absent \
         from list_observations on fresh store handle"
    );
}

/// RED — `review_events` must return only detection observations; corrections and
/// other cg-internal observation types must not surface.  `review_why("--latest")`
/// must pick the newest detection even when a more-recent correction exists.
///
/// Wrong stub: no filter applied → corrections appear in `review_events` rows; and
/// `--latest` might pick the correction instead of the detection.
#[test]
fn detection_only_events_excludes_corrections() {
    let (_tmp, store_path) = deterministic_fixture_support::fresh_store_copy(1)
        .expect("seeded store for detection_only_events_excludes_corrections");
    let store = deterministic_fixture_support::open_store_at(&store_path).expect("open store");

    // The fixture contains the exact detection count requested by the test.
    // Use the NEWEST detection as the anchor so we can verify that after writing a correction
    // (which gets an even newer id), `--latest` still picks the newest DETECTION, not the correction.
    let obs = store.list_observations(None).expect("list_observations");
    let detection = obs
        .iter()
        .filter(|o| o.observation_type == "detection")
        .max_by_key(|o| o.observed_at)
        .expect("at least one detection required in seeded store");
    let detection_id = detection.id.to_string();

    // Write a correction — its v7 UUID timestamp is newer than any existing detection.
    // It must NOT appear in review_events or be selected by --latest.
    let req = CorrectionRequest {
        detection_id: detection_id.clone(),
        label: Some("acknowledged".to_string()),
        correction_type: CorrectionType::Identity,
    };
    record_correction(&store, req).expect("record_correction must not error");

    // review_events must return ONLY detection rows — the correction must be absent.
    let events = review_events(&store, 100).expect("review_events must not error");
    for row in &events.rows {
        // Find the raw observation to verify its type.
        let raw = store
            .list_observations(None)
            .unwrap_or_default()
            .into_iter()
            .find(|o| o.id.to_string() == row.observation_id);
        if let Some(raw_obs) = raw {
            assert_eq!(
                raw_obs.observation_type, "detection",
                "review_events returned a row for observation {} which has type '{}', not 'detection'; \
                 only detection observations must surface on the events display path",
                row.observation_id, raw_obs.observation_type
            );
        }
    }

    // Verify the detection IS present (sanity: filter must not over-strip).
    let detection_present = events.rows.iter().any(|r| r.observation_id == detection_id);
    assert!(
        detection_present,
        "review_events must include the detection row for {detection_id}; \
         over-filtering dropped it"
    );

    // `vigil why --latest` (handle_why_read with "--latest") must select the detection,
    // not the newer correction.  Wrong stub: no filter → picks the newer correction →
    // provenance walk fails (no decision/intention).
    let why = review_why(&store, "--latest")
        .expect("review_why('--latest') must resolve to the detection, not the newer correction");
    assert_eq!(
        why.observation_id, detection_id,
        "review_why('--latest') must select the newest DETECTION ({}), not the newer \
         correction; wrong stub picks the correction and the provenance walk fails",
        detection_id
    );
}

/// RED — `record_correction` must return `Err(NoAnchor)` when the anchor observation
/// exists in cg but has `observation_type != "detection"`.  Zero new observations must
/// be written (the type guard fires before any write).
///
/// Wrong stub: no type check → returns Ok even for a non-detection anchor → assertions FAIL.
#[test]
fn correction_rejected_for_non_detection_anchor() {
    let (_tmp, store_path) = deterministic_fixture_support::fresh_store_copy(1)
        .expect("seeded store for correction_rejected_for_non_detection_anchor");
    let store = deterministic_fixture_support::open_store_at(&store_path).expect("open store");

    // Obtain the detection id and then write a correction anchored to it.
    let obs = store.list_observations(None).expect("list_observations");
    let detection = obs
        .iter()
        .find(|o| o.observation_type == "detection")
        .expect("at least one detection required");
    let detection_id = detection.id.to_string();

    let first_correction_req = CorrectionRequest {
        detection_id: detection_id.clone(),
        label: None,
        correction_type: CorrectionType::FalseAlarm,
    };
    let receipt = record_correction(&store, first_correction_req)
        .expect("initial correction on detection must succeed");

    // The correction observation now exists in cg with observation_type = "correction".
    // Attempt to anchor a second correction to the CORRECTION observation (not the detection).
    let obs_after = store
        .list_observations(None)
        .expect("list after first correction");
    let count_before_second = obs_after.len();

    let bad_req = CorrectionRequest {
        detection_id: receipt.correction_id.clone(), // ← anchoring to the correction, not the detection
        label: Some("bad anchor".to_string()),
        correction_type: CorrectionType::Identity,
    };
    let result = record_correction(&store, bad_req);

    // Must return Err(NoAnchor) — the anchor is valid (exists, well-formed UUID) but
    // has the wrong type.  Wrong stub: no type check → Ok(receipt) → FAILS.
    assert!(
        matches!(result, Err(CorrectionError::NoAnchor(_))),
        "record_correction anchored to a correction observation (type='correction') must \
         return Err(NoAnchor); wrong stub has no type check and returns Ok — got: {result:?}"
    );

    // Zero new observations must have been written (the guard fires before the write).
    let obs_final = store
        .list_observations(None)
        .expect("list after rejected correction");
    assert_eq!(
        obs_final.len(),
        count_before_second,
        "rejected non-detection anchor must write zero new observations; \
         wrong stub writes a spurious correction before checking the type (count delta = {})",
        obs_final.len() as i64 - count_before_second as i64
    );
}
