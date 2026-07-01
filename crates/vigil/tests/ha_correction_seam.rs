// Integration tests for the correction seam and lifted review API.
// All tests require a real running vigil instance with the live detector,
// so the whole file is gated on first-light-acceptance.
//
// REGRESSION GUARDs pass at scaffold.
// RED tests fail on assertions via the deliberate wrong stubs.

use std::{thread, time::Duration};

use context_graph::{Observation, Store};
use vigil::{
    CorrectionError, CorrectionRequest, CorrectionType, record_correction, review_events,
    review_why,
};

#[path = "ha_test_support.rs"]
mod ha_test_support;

/// List all observations from the store (across all contexts).
fn list_all_observations(store: &Store) -> Vec<Observation> {
    store.list_observations(None).unwrap_or_default()
}

// ── REGRESSION GUARD ──────────────────────────────────────────────────────

/// The `vigil events` read path lifted into the public `review_events` library
/// API still lists recently landed events from cg authority.
///
/// REGRESSION GUARD: passes at scaffold — the lifted read delegates to the
/// real event-listing internals; only the per-event `correction_recorded` flag
/// is the RED `review_events_row_flags_corrected_events`.
#[test]
fn vigil_events_lists_recent_events_through_review_api() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1).expect(
        "seeded store for REGRESSION GUARD vigil_events_lists_recent_events_through_review_api",
    );
    let store =
        ha_test_support::open_store_at(&store_path).expect("store must open from seeded copy");

    let view = review_events(&store, 100).expect("review_events must not error on a valid store");

    assert!(
        !view.rows.is_empty(),
        "review_events must list at least one event row after a detection was recorded; \
         the lifted read path must not regress the existing event listing"
    );

    // Basic shape: each row must carry a non-empty observation id
    for row in &view.rows {
        assert!(
            !row.observation_id.is_empty(),
            "review_events row must carry a non-empty observation_id; got empty string"
        );
    }
}

// ── RED: correction-seam integration tests ────────────────────────────────

/// RED — wrong stub writes nothing to cg, so the returned receipt's id does not
/// match any observation in the store and the observation count does not grow.
#[test]
fn correction_writes_durably_through_cg_record_path() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for correction_writes_durably_through_cg_record_path");
    let store =
        ha_test_support::open_store_at(&store_path).expect("store must open from seeded copy");

    let observations = list_all_observations(&store);
    assert!(
        !observations.is_empty(),
        "at least one detection must be in the store for the correction seam test"
    );
    let detection_id = observations[0].id.to_string();

    let count_before = observations.len();

    let request = CorrectionRequest {
        detection_id: detection_id.clone(),
        label: Some("that's Roshan".to_string()),
        correction_type: CorrectionType::Identity,
    };

    let receipt =
        record_correction(&store, request).expect("record_correction must not return Err");

    // The receipt's durable id must value-equal the new correction observation's id.
    // Wrong stub: returns a fixed fake id "00000000-0000-0000-0000-000000000042" and
    // writes no observation, so the observation count does not grow.
    let observations_after = list_all_observations(&store);
    assert_eq!(
        observations_after.len(),
        count_before + 1,
        "record_correction must write exactly one new observation into the cg store; \
         wrong stub writes nothing — count stayed at {} instead of {}",
        observations_after.len(),
        count_before + 1,
    );

    // The receipt id must identify the written correction observation.
    let written = observations_after
        .iter()
        .find(|o| o.id.to_string() == receipt.correction_id);
    assert!(
        written.is_some(),
        "CorrectionReceipt.correction_id '{}' must value-equal the written correction \
         observation's id; wrong stub returns a fixed fake id that matches nothing in the store",
        receipt.correction_id
    );

    // The written observation must carry the anchored_detection_id property.
    let correction_obs = written.unwrap();
    let anchored = correction_obs
        .observed_properties
        .get("anchored_detection_id")
        .or_else(|| correction_obs.properties.get("anchored_detection_id"))
        .and_then(|v| v.as_str());
    assert_eq!(
        anchored,
        Some(detection_id.as_str()),
        "correction observation must carry anchored_detection_id equal to the original \
         detection id; got {anchored:?}"
    );

    // After reopening the store (simulating restart), the correction is still present.
    drop(store);
    let store2 = ha_test_support::open_store_at(&store_path).expect("store must reopen");
    let obs_after_reopen = list_all_observations(&store2);
    assert_eq!(
        obs_after_reopen.len(),
        count_before + 1,
        "correction must survive a store reopen; wrong stub wrote nothing so nothing persists"
    );

    // After reopen, verify label and correction_type content identity via cg public read.
    let correction_after_reopen = obs_after_reopen
        .iter()
        .find(|o| o.id.to_string() == receipt.correction_id);
    assert!(
        correction_after_reopen.is_some(),
        "correction observation '{}' must be findable after reopen; \
         wrong stub writes nothing to cg so the id matches nothing",
        receipt.correction_id
    );
    let reopen_obs = correction_after_reopen.unwrap();
    let label_after_reopen = reopen_obs
        .observed_properties
        .get("label")
        .or_else(|| reopen_obs.properties.get("label"))
        .and_then(|v| v.as_str());
    assert_eq!(
        label_after_reopen,
        Some("that's Roshan"),
        "correction observation label must be 'that\\'s Roshan' after reopen via cg public read; \
         wrong stub writes nothing so label is absent"
    );
    let correction_type_after_reopen = reopen_obs
        .observed_properties
        .get("correction_type")
        .or_else(|| reopen_obs.properties.get("correction_type"))
        .and_then(|v| v.as_str());
    assert_eq!(
        correction_type_after_reopen,
        Some("Identity"),
        "correction observation correction_type must be 'Identity' after reopen via cg public read; \
         wrong stub writes nothing so correction_type is absent"
    );
}

/// RED — wrong stub returns empty corrections list; `review_why` shows the
/// provenance walk but no corrections.
#[test]
fn correction_reads_back_via_review_why() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for correction_reads_back_via_review_why");
    let store =
        ha_test_support::open_store_at(&store_path).expect("store must open from seeded copy");

    let observations = list_all_observations(&store);
    assert!(
        !observations.is_empty(),
        "at least one detection must be in the store"
    );
    let detection_id = observations[0].id.to_string();

    let request = CorrectionRequest {
        detection_id: detection_id.clone(),
        label: Some("that's the neighbour".to_string()),
        correction_type: CorrectionType::Identity,
    };
    record_correction(&store, request).expect("record_correction must not error");

    let why = review_why(&store, &detection_id)
        .expect("review_why must not error on a valid detection id");

    // The provenance walk must still reach the expected fields.
    assert!(
        !why.observation_id.is_empty(),
        "review_why must return a non-empty observation_id (provenance walk must not regress)"
    );

    // The corrections list must contain the correction we just recorded.
    // Wrong stub: always returns an empty corrections list.
    assert_eq!(
        why.corrections.len(),
        1,
        "review_why({detection_id}).corrections must contain the recorded correction; \
         wrong stub returns an empty list"
    );

    let recorded = &why.corrections[0];
    assert_eq!(
        recorded.correction_type,
        CorrectionType::Identity,
        "recorded correction type must be Identity; got {:?}",
        recorded.correction_type
    );
    assert_eq!(
        recorded.label.as_deref(),
        Some("that's the neighbour"),
        "recorded correction label must value-equal the written label; \
         wrong stub returns empty corrections so this is never reached"
    );
}

/// RED — wrong stub hardcodes correction_recorded = false for every row; a
/// corrected event's row still reads false.
#[test]
fn review_events_row_flags_corrected_events() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(2)
        .expect("seeded store for review_events_row_flags_corrected_events");
    let store =
        ha_test_support::open_store_at(&store_path).expect("store must open from seeded copy");

    let mut observations = list_all_observations(&store);
    assert!(
        observations.len() >= 2,
        "at least two detections must be in the store for this test"
    );
    // Sort by id to get a deterministic order.
    observations.sort_by_key(|o| o.id.to_string());
    let first_id = observations[0].id.to_string();
    let second_id = observations[1].id.to_string();

    // Record a correction against the first detection only.
    let request = CorrectionRequest {
        detection_id: first_id.clone(),
        label: None,
        correction_type: CorrectionType::FalseAlarm,
    };
    record_correction(&store, request).expect("record_correction must not error");

    let events = review_events(&store, 100).expect("review_events must not error");

    // The row for the first (corrected) detection must carry correction_recorded = true.
    let first_row = events
        .rows
        .iter()
        .find(|r| r.observation_id == first_id)
        .expect("review_events must include a row for the corrected detection");

    assert!(
        first_row.correction_recorded,
        "review_events row for detection {first_id} (which has a correction) must carry \
         correction_recorded = true; wrong stub hardcodes false for every row"
    );

    // The row for the second (uncorrected) detection must carry correction_recorded = false.
    let second_row = events
        .rows
        .iter()
        .find(|r| r.observation_id == second_id)
        .expect("review_events must include a row for the uncorrected detection");

    assert!(
        !second_row.correction_recorded,
        "review_events row for uncorrected detection {second_id} must carry \
         correction_recorded = false"
    );
}

/// RED — correction held outside the cg store fails the fresh-handle read-back.
///
/// Proof: after dropping all in-process state we reopen ONLY the cg Store and
/// read via public `get_observation`/`list_observations` (never `review_why`,
/// which a mirror could answer).  The wrong stub stores nothing in cg.
#[test]
fn correction_held_outside_cg_fails_readback() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for correction_held_outside_cg_fails_readback");

    let observations = {
        let store = ha_test_support::open_store_at(&store_path).expect("store must open");
        list_all_observations(&store)
    };
    assert!(
        !observations.is_empty(),
        "at least one detection must be in the store"
    );
    let detection_id = observations[0].id.to_string();

    // Record a correction — wrong stub writes nothing to cg.
    {
        let store =
            ha_test_support::open_store_at(&store_path).expect("store must open for correction");
        let request = CorrectionRequest {
            detection_id: detection_id.clone(),
            label: Some("correction-test-label".to_string()),
            correction_type: CorrectionType::WrongClass,
        };
        record_correction(&store, request).expect("record_correction must not error");
    }
    // All in-process state dropped here (store dropped at end of block above).

    // Reopen ONLY the cg Store as a fresh handle.
    let fresh_store = ha_test_support::open_store_at(&store_path).expect("fresh store must open");

    // Read via cg public methods (not review_why).
    let all_observations = fresh_store
        .list_observations(None)
        .expect("list_observations must succeed");

    // Find any observation whose observed_properties or properties carry
    // anchored_detection_id == detection_id.
    let correction_in_cg = all_observations.iter().any(|o| {
        o.observed_properties
            .get("anchored_detection_id")
            .or_else(|| o.properties.get("anchored_detection_id"))
            .and_then(|v| v.as_str())
            == Some(detection_id.as_str())
    });

    assert!(
        correction_in_cg,
        "correction must be readable from a fresh cg Store handle via list_observations; \
         wrong stub writes the correction outside cg (or not at all), so it vanishes on reopen"
    );
}

/// RED — correction must survive a full store reopen (daemon restart simulation).
#[test]
fn correction_survives_daemon_restart() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for correction_survives_daemon_restart");

    let detection_id = {
        let store = ha_test_support::open_store_at(&store_path).expect("store must open");
        let obs = list_all_observations(&store);
        assert!(
            !obs.is_empty(),
            "at least one detection must be in the store"
        );
        obs[0].id.to_string()
    };

    // Record a correction then drop the store handle (simulates daemon exit).
    {
        let store =
            ha_test_support::open_store_at(&store_path).expect("store must open for correction");
        let request = CorrectionRequest {
            detection_id: detection_id.clone(),
            label: Some("false alarm".to_string()),
            correction_type: CorrectionType::FalseAlarm,
        };
        record_correction(&store, request).expect("record_correction must not error");
    }

    // Reopen the store (simulates daemon restart) and rebuild the review API on it.
    let store_after_restart = ha_test_support::open_store_at(&store_path)
        .expect("store must reopen after simulated restart");

    // Corrections must read back via the rebuilt review API.
    let why = review_why(&store_after_restart, &detection_id)
        .expect("review_why must not error after restart");

    assert_eq!(
        why.corrections.len(),
        1,
        "correction must survive daemon restart: review_why must list it after store reopen; \
         wrong stub writes nothing so the in-process mirror vanishes on reopen"
    );

    // Also verify via raw cg public methods.
    let all_obs = list_all_observations(&store_after_restart);
    let anchored_correction = all_obs.iter().any(|o| {
        o.observed_properties
            .get("anchored_detection_id")
            .or_else(|| o.properties.get("anchored_detection_id"))
            .and_then(|v| v.as_str())
            == Some(detection_id.as_str())
    });
    assert!(
        anchored_correction,
        "correction must read back via list_observations after restart; \
         wrong stub writes nothing to cg"
    );
}

/// RED — wrong stub always returns Ok; an unknown detection id must return
/// a typed CorrectionError::NoAnchor with zero cg observation written.
#[test]
fn record_correction_with_unknown_detection_id_returns_typed_error() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(1)
        .expect("seeded store for record_correction_with_unknown_detection_id_returns_typed_error");
    let store = ha_test_support::open_store_at(&store_path).expect("store must open");
    let count_before = list_all_observations(&store).len();

    // (a) Well-formed but unknown UUID.
    let unknown_id = "00000000-0000-0000-0000-000000000001".to_string();
    let result_a = record_correction(
        &store,
        CorrectionRequest {
            detection_id: unknown_id.clone(),
            label: None,
            correction_type: CorrectionType::FalseAlarm,
        },
    );

    assert!(
        matches!(result_a, Err(CorrectionError::NoAnchor(_))),
        "record_correction with unknown detection id must return Err(NoAnchor); \
         wrong stub returns Ok(receipt) instead — result was: {result_a:?}"
    );

    // (b) Malformed id (not a UUID).
    let malformed_id = "not-a-uuid-at-all".to_string();
    let result_b = record_correction(
        &store,
        CorrectionRequest {
            detection_id: malformed_id.clone(),
            label: None,
            correction_type: CorrectionType::FalseAlarm,
        },
    );

    assert!(
        matches!(result_b, Err(CorrectionError::NoAnchor(_))),
        "record_correction with malformed id must return Err(NoAnchor); \
         wrong stub returns Ok(receipt) — result was: {result_b:?}"
    );

    // Zero new observations written for either bad id.
    let count_after = list_all_observations(&store).len();
    assert_eq!(
        count_after, count_before,
        "record_correction against unknown/malformed ids must write zero cg observations; \
         wrong stub always returns Ok and may write a spurious observation"
    );
}

/// RED — correction must anchor to the named (middle) detection; wrong stubs
/// that anchor to first or last are each defeated.
#[test]
fn correction_anchored_to_named_detection_only() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(3)
        .expect("seeded store for correction_anchored_to_named_detection_only");
    let store = ha_test_support::open_store_at(&store_path).expect("store must open");
    let mut observations = list_all_observations(&store);
    assert!(
        observations.len() >= 3,
        "at least three detections must be in the store for the anchoring test; \
         got {}",
        observations.len()
    );
    observations.sort_by_key(|o| o.observed_at);
    let id_a = observations[0].id.to_string();
    let id_b = observations[1].id.to_string();
    let id_c = observations[2].id.to_string();

    // Record a correction against the MIDDLE detection (B).
    let request = CorrectionRequest {
        detection_id: id_b.clone(),
        label: Some("middle-detection-correction".to_string()),
        correction_type: CorrectionType::Identity,
    };
    record_correction(&store, request).expect("record_correction must not error");

    // review_why(B) must list the correction.
    let why_b = review_why(&store, &id_b).expect("review_why(B) must not error");
    assert_eq!(
        why_b.corrections.len(),
        1,
        "review_why(B=middle detection) must list the correction; \
         wrong stub returns empty corrections for all detections"
    );

    // review_why(A) must NOT list it — defeats anchor-to-first stubs.
    let why_a = review_why(&store, &id_a).expect("review_why(A) must not error");
    assert!(
        why_a.corrections.is_empty(),
        "review_why(A=first detection) must NOT list the correction anchored to B; \
         a stub that anchors to the first detection would fail here"
    );

    // review_why(C) must NOT list it — defeats anchor-to-last stubs.
    let why_c = review_why(&store, &id_c).expect("review_why(C) must not error");
    assert!(
        why_c.corrections.is_empty(),
        "review_why(C=last detection) must NOT list the correction anchored to B; \
         a stub that anchors to the last detection would fail here"
    );
}

/// RED — all three correction kinds record and read back with their distinct
/// label/type content.
#[test]
fn false_alarm_and_wrong_class_corrections_record_and_read_back() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(2)
        .expect("seeded store for false_alarm_and_wrong_class_corrections_record_and_read_back");
    let store = ha_test_support::open_store_at(&store_path).expect("store must open");
    let mut observations = list_all_observations(&store);
    assert!(
        observations.len() >= 2,
        "at least two detections must be in the store"
    );
    observations.sort_by_key(|o| o.observed_at);
    let id_first = observations[0].id.to_string();
    let id_second = observations[1].id.to_string();

    // False-alarm correction on first detection (label is specifically None).
    let req_false_alarm = CorrectionRequest {
        detection_id: id_first.clone(),
        label: None,
        correction_type: CorrectionType::FalseAlarm,
    };
    record_correction(&store, req_false_alarm).expect("false-alarm correction must not error");

    // Wrong-class correction on second detection (label carries the corrected class).
    let req_wrong_class = CorrectionRequest {
        detection_id: id_second.clone(),
        label: Some("vehicle".to_string()),
        correction_type: CorrectionType::WrongClass,
    };
    record_correction(&store, req_wrong_class).expect("wrong-class correction must not error");

    // Read back false-alarm via review_why.
    let why_first = review_why(&store, &id_first).expect("review_why(first) must not error");

    assert_eq!(
        why_first.corrections.len(),
        1,
        "review_why(first) must list the false-alarm correction; \
         wrong stub returns empty corrections"
    );
    let fa_correction = &why_first.corrections[0];
    assert_eq!(
        fa_correction.correction_type,
        CorrectionType::FalseAlarm,
        "false-alarm correction type must read back as FalseAlarm; got {:?}",
        fa_correction.correction_type
    );
    assert!(
        fa_correction.label.is_none(),
        "false-alarm correction label must be specifically None; \
         got {:?}",
        fa_correction.label
    );

    // Read back wrong-class via review_why.
    let why_second = review_why(&store, &id_second).expect("review_why(second) must not error");

    assert_eq!(
        why_second.corrections.len(),
        1,
        "review_why(second) must list the wrong-class correction; \
         wrong stub returns empty corrections"
    );
    let wc_correction = &why_second.corrections[0];
    assert_eq!(
        wc_correction.correction_type,
        CorrectionType::WrongClass,
        "wrong-class correction type must read back as WrongClass; got {:?}",
        wc_correction.correction_type
    );
    assert_eq!(
        wc_correction.label.as_deref(),
        Some("vehicle"),
        "wrong-class correction label must value-equal the corrected class 'vehicle'; \
         wrong stub drops the label"
    );
}

/// RED — wrong stub sets correction_recorded=true for ALL correction types, including
/// Identity.  Identity is a POSITIVE signal ("confirmed") and must NOT set
/// correction_recorded; it must set confirmed=true instead.
///
/// FalseAlarm is a negative signal: correction_recorded=true, confirmed=false.
/// Identity ("confirmed"): correction_recorded=false, confirmed=true.
#[test]
fn confirmed_correction_does_not_set_correction_recorded() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(2)
        .expect("seeded store for confirmed_correction_does_not_set_correction_recorded");
    let store = ha_test_support::open_store_at(&store_path).expect("store must open");
    let mut observations = list_all_observations(&store);
    assert!(
        observations.len() >= 2,
        "at least two detections must be in the store for this test; got {}",
        observations.len()
    );
    observations.sort_by_key(|o| o.id.to_string());
    let id_a = observations[0].id.to_string();
    let id_b = observations[1].id.to_string();

    // Identity ("confirmed") on detection A.
    let req_a = CorrectionRequest {
        detection_id: id_a.clone(),
        label: Some("confirmed correct".to_string()),
        correction_type: CorrectionType::Identity,
    };
    record_correction(&store, req_a).expect("Identity correction must not error");

    // FalseAlarm on detection B.
    let req_b = CorrectionRequest {
        detection_id: id_b.clone(),
        label: None,
        correction_type: CorrectionType::FalseAlarm,
    };
    record_correction(&store, req_b).expect("FalseAlarm correction must not error");

    let events = review_events(&store, 100).expect("review_events must not error");

    let row_a = events
        .rows
        .iter()
        .find(|r| r.observation_id == id_a)
        .expect("review_events must include a row for detection A");

    // Identity is a positive signal: confirmed=true, correction_recorded=false.
    assert!(
        !row_a.correction_recorded,
        "Identity ('confirmed') correction on detection A must NOT set correction_recorded; \
         wrong stub sets correction_recorded for ALL correction types — got correction_recorded=true"
    );
    assert!(
        row_a.confirmed,
        "Identity ('confirmed') correction on detection A must set confirmed=true; \
         wrong stub hardcodes confirmed=false — got false"
    );

    let row_b = events
        .rows
        .iter()
        .find(|r| r.observation_id == id_b)
        .expect("review_events must include a row for detection B");

    // FalseAlarm is a negative signal: correction_recorded=true, confirmed=false.
    assert!(
        row_b.correction_recorded,
        "FalseAlarm correction on detection B must set correction_recorded=true; got false"
    );
    assert!(
        !row_b.confirmed,
        "FalseAlarm correction on detection B must NOT set confirmed=true; got confirmed=true"
    );
}

/// RED — the Home Assistant card no longer derives durable correction state
/// from local `confirmed` / `correction_recorded` booleans.  The event-list
/// authority must expose the server-decided current correction and corrected
/// label on each row.
#[test]
fn review_events_rows_expose_current_correction_authority_fields() {
    let (_tmp, store_path) = ha_test_support::fresh_store_copy(4)
        .expect("seeded store for review_events_rows_expose_current_correction_authority_fields");
    let store = ha_test_support::open_store_at(&store_path).expect("store must open");
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

    record_correction(
        &store,
        CorrectionRequest {
            detection_id: identity_id.clone(),
            label: Some("operator confirmed".to_string()),
            correction_type: CorrectionType::Identity,
        },
    )
    .expect("Identity correction must record");

    record_correction(
        &store,
        CorrectionRequest {
            detection_id: wrong_class_id.clone(),
            label: Some("initial confirmation".to_string()),
            correction_type: CorrectionType::Identity,
        },
    )
    .expect("initial Identity correction must record");
    thread::sleep(Duration::from_millis(2));
    record_correction(
        &store,
        CorrectionRequest {
            detection_id: wrong_class_id.clone(),
            label: Some("cow".to_string()),
            correction_type: CorrectionType::WrongClass,
        },
    )
    .expect("latest WrongClass correction must record");

    record_correction(
        &store,
        CorrectionRequest {
            detection_id: false_alarm_id.clone(),
            label: None,
            correction_type: CorrectionType::FalseAlarm,
        },
    )
    .expect("FalseAlarm correction must record");

    let events = review_events(&store, 100).expect("review_events must not error");

    let identity_row = events
        .rows
        .iter()
        .find(|row| row.observation_id == identity_id)
        .expect("review_events must include the Identity-reviewed detection");
    assert_eq!(
        identity_row.current_correction.as_ref(),
        Some(&CorrectionType::Identity),
        "Identity-reviewed event row must expose current_correction=Identity for card reload authority"
    );
    assert_eq!(
        identity_row.corrected_label.as_deref(),
        None,
        "Identity confirmation must not invent a corrected_label"
    );

    let wrong_class_row = events
        .rows
        .iter()
        .find(|row| row.observation_id == wrong_class_id)
        .expect("review_events must include the detection with a latest WrongClass correction");
    assert_eq!(
        wrong_class_row.current_correction.as_ref(),
        Some(&CorrectionType::WrongClass),
        "when multiple corrections exist, the row must expose the latest server-decided correction type"
    );
    assert_eq!(
        wrong_class_row.corrected_label.as_deref(),
        Some("cow"),
        "WrongClass event row must expose the corrected class label for Home Assistant reload"
    );

    let false_alarm_row = events
        .rows
        .iter()
        .find(|row| row.observation_id == false_alarm_id)
        .expect("review_events must include the FalseAlarm-reviewed detection");
    assert_eq!(
        false_alarm_row.current_correction.as_ref(),
        Some(&CorrectionType::FalseAlarm),
        "FalseAlarm event row must expose current_correction=FalseAlarm"
    );
    assert_eq!(
        false_alarm_row.corrected_label.as_deref(),
        None,
        "FalseAlarm correction must not invent a corrected_label"
    );

    let unreviewed_row = events
        .rows
        .iter()
        .find(|row| row.observation_id == unreviewed_id)
        .expect("review_events must include the unreviewed detection");
    assert_eq!(
        unreviewed_row.current_correction.as_ref(),
        None,
        "unreviewed event row must expose current_correction=None, not a local card-derived fallback"
    );
    assert_eq!(
        unreviewed_row.corrected_label.as_deref(),
        None,
        "unreviewed event row must expose corrected_label=None"
    );
}
