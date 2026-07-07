//! Work-graph contract: envelopes join by identity, never by process-local
//! guesswork; stage vocabulary is open; provenance supports aggregation;
//! inter-stage queues are bounded with visible counters.

use std::time::Duration;

use chrono::{TimeZone, Utc};
use vigil::workgraph::{
    BoundedStageQueue, JoinRejection, MediaItemId, QueueRecv, ReceiptId, ResultEnvelope, StageId,
    StageReceipt, StreamId, WORK_ENVELOPE_SCHEMA_VERSION, WorkDeadline, WorkDisposition,
    WorkEnvelope, WorkId, WorkOrdering, WorkPriority, validate_result_join,
};

fn work_envelope(stage: &str) -> WorkEnvelope {
    WorkEnvelope {
        work_id: WorkId::generate(),
        parent_work_id: None,
        contributing_work_ids: Vec::new(),
        stage: StageId::new(stage),
        stream_id: StreamId::new("front-yard"),
        media_item: Some(MediaItemId::Segment {
            segment_sequence: 7,
        }),
        ordering: WorkOrdering {
            stream_epoch: 1,
            stream_sequence: 7,
        },
        observed_at: Some(Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()),
        received_at: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 6).unwrap(),
        priority: WorkPriority::MOTION,
        deadline: WorkDeadline::None,
        schema_version: WORK_ENVELOPE_SCHEMA_VERSION,
    }
}

fn matching_result(work: &WorkEnvelope, receipt_id: ReceiptId) -> ResultEnvelope {
    ResultEnvelope {
        work_id: work.work_id,
        parent_work_id: work.parent_work_id,
        contributing_work_ids: work.contributing_work_ids.clone(),
        stage: work.stage.clone(),
        stream_id: work.stream_id.clone(),
        media_item: work.media_item,
        ordering: work.ordering,
        observed_at: work.observed_at,
        result_schema_version: WORK_ENVELOPE_SCHEMA_VERSION,
        receipt_id,
    }
}

fn receipt_for(work: &WorkEnvelope) -> StageReceipt {
    StageReceipt {
        receipt_id: ReceiptId::generate(),
        work_id: work.work_id,
        parent_work_id: work.parent_work_id,
        stage: work.stage.clone(),
        stream_id: work.stream_id.clone(),
        configured_backend: Some("auto".to_string()),
        attempted_backend: Some("software".to_string()),
        active_backend: Some("software".to_string()),
        fallback_backend: None,
        selected_device: None,
        probe_result: Some("decoded".to_string()),
        fallback_reason: None,
        started_at: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 6).unwrap(),
        ended_at: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 7).unwrap(),
        output_count: 1,
        disposition: WorkDisposition::Completed,
    }
}

#[test]
fn result_envelope_joins_only_to_exact_work_identity() {
    let work = work_envelope("detection");
    let receipt = receipt_for(&work);
    let result = matching_result(&work, receipt.receipt_id);

    assert_eq!(
        validate_result_join(&work, &result, Some(&receipt)),
        Ok(()),
        "a result carrying the exact work identity and the exact backend-attempt receipt must join"
    );
}

#[test]
fn mismatched_stream_or_media_result_is_rejected_not_guessed() {
    let work = work_envelope("detection");
    let receipt = receipt_for(&work);

    let mut wrong_stream = matching_result(&work, receipt.receipt_id);
    wrong_stream.stream_id = StreamId::new("back-yard");
    assert_eq!(
        validate_result_join(&work, &wrong_stream, Some(&receipt)),
        Err(JoinRejection::StreamMismatch),
        "a result from another stream must be rejected, not adopted"
    );

    let mut wrong_media = matching_result(&work, receipt.receipt_id);
    wrong_media.media_item = Some(MediaItemId::Segment {
        segment_sequence: 8,
    });
    assert_eq!(
        validate_result_join(&work, &wrong_media, Some(&receipt)),
        Err(JoinRejection::MediaItemMismatch),
        "a result for another media item must be rejected"
    );

    let mut wrong_ordering = matching_result(&work, receipt.receipt_id);
    wrong_ordering.ordering = WorkOrdering {
        stream_epoch: 2,
        stream_sequence: 7,
    };
    assert_eq!(
        validate_result_join(&work, &wrong_ordering, Some(&receipt)),
        Err(JoinRejection::OrderingMismatch),
        "a result from another stream epoch must be rejected"
    );

    let mut wrong_work = matching_result(&work, receipt.receipt_id);
    wrong_work.work_id = WorkId::generate();
    assert_eq!(
        validate_result_join(&work, &wrong_work, Some(&receipt)),
        Err(JoinRejection::WorkIdMismatch),
        "a result for another work id must be rejected"
    );

    let mut incompatible = matching_result(&work, receipt.receipt_id);
    incompatible.result_schema_version = WORK_ENVELOPE_SCHEMA_VERSION + 1;
    assert_eq!(
        validate_result_join(&work, &incompatible, Some(&receipt)),
        Err(JoinRejection::SchemaIncompatible {
            result_schema_version: WORK_ENVELOPE_SCHEMA_VERSION + 1
        }),
        "a schema-incompatible result is handled by envelope rules, not stage guesswork"
    );
}

#[test]
fn receipt_links_result_to_exact_backend_attempt() {
    let work = work_envelope("detection");
    let receipt = receipt_for(&work);
    let result = matching_result(&work, receipt.receipt_id);

    assert_eq!(
        validate_result_join(&work, &result, None),
        Err(JoinRejection::ReceiptMissing),
        "a result that cannot reference the exact backend attempt must not join"
    );

    let mut foreign_receipt = receipt_for(&work);
    foreign_receipt.work_id = WorkId::generate();
    assert!(
        validate_result_join(&work, &result, Some(&foreign_receipt)).is_err(),
        "a receipt recorded for different work must not authenticate this result"
    );
}

#[test]
fn custom_stage_id_needs_no_vocabulary_change() {
    // Episode memory brings track/episode/scene_map stages; a world-model
    // annotator may follow. The envelope must carry them TODAY with no
    // vocabulary edit.
    for future_stage in ["track", "episode", "scene_map", "annotate"] {
        let mut work = work_envelope(future_stage);
        work.stage = StageId::new(future_stage);
        let receipt = receipt_for(&work);
        let result = matching_result(&work, receipt.receipt_id);
        assert_eq!(
            validate_result_join(&work, &result, Some(&receipt)),
            Ok(()),
            "stage `{future_stage}` must flow through the graph with no enum/vocabulary change"
        );
    }
}

#[test]
fn aggregation_stage_joins_two_parents_via_contributing_work_ids() {
    // A tracker-style stage consumes MANY inputs and emits one output whose
    // provenance names every contributor.
    let detection_a = work_envelope("detection");
    let detection_b = work_envelope("detection");

    let mut aggregate = work_envelope("track");
    aggregate.parent_work_id = Some(detection_a.work_id);
    aggregate.contributing_work_ids = vec![detection_a.work_id, detection_b.work_id];

    let receipt = receipt_for(&aggregate);
    let result = matching_result(&aggregate, receipt.receipt_id);

    assert_eq!(
        validate_result_join(&aggregate, &result, Some(&receipt)),
        Ok(()),
        "an aggregation result carrying all contributing parents must join"
    );
    assert_eq!(
        result.contributing_work_ids,
        vec![detection_a.work_id, detection_b.work_id],
        "the result must preserve full multi-parent provenance"
    );

    let mut dropped_parent = matching_result(&aggregate, receipt.receipt_id);
    dropped_parent.contributing_work_ids = vec![detection_a.work_id];
    assert_eq!(
        validate_result_join(&aggregate, &dropped_parent, Some(&receipt)),
        Err(JoinRejection::ParentMismatch),
        "silently dropping a contributing parent must reject the join"
    );
}

#[test]
fn stage_may_emit_zero_or_many_results_with_counted_disposition() {
    // 0..N results per work item is first-class: a receipt with zero
    // outputs and a Dropped/Coalesced disposition is a legal, counted
    // outcome — not an error and not silence.
    let work = work_envelope("motion");
    let mut receipt = receipt_for(&work);

    receipt.output_count = 0;
    receipt.disposition = WorkDisposition::Coalesced;
    assert_eq!(receipt.output_count, 0);
    assert_eq!(receipt.disposition, WorkDisposition::Coalesced);

    receipt.output_count = 48;
    receipt.disposition = WorkDisposition::Completed;
    assert_eq!(receipt.output_count, 48);
}

#[test]
fn bounded_queue_has_explicit_capacity_and_visible_counters() {
    let queue: BoundedStageQueue<u64> = BoundedStageQueue::new(2);
    assert_eq!(queue.capacity(), 2, "capacity is explicit, never unbounded");

    assert!(
        queue
            .push_latest(1)
            .expect("push within capacity")
            .is_none()
    );
    assert!(
        queue
            .push_latest(2)
            .expect("push within capacity")
            .is_none()
    );

    let counters = queue.counters();
    assert_eq!(counters.queued_total, 2);
    assert_eq!(counters.current_depth, 2);
    assert_eq!(counters.replaced_dropped_total, 0);
}

#[test]
fn keep_newest_replacement_is_counted_not_silent() {
    let queue: BoundedStageQueue<u64> = BoundedStageQueue::new(1);
    assert!(queue.push_latest(10).expect("first push").is_none());

    // Full queue: the NEWEST item wins, the oldest is returned to the
    // caller for visible cleanup, and the replacement is counted.
    let replaced = queue.push_latest(20).expect("replacement push");
    assert_eq!(replaced, Some(10), "the oldest pending item is handed back");

    match queue.recv_timeout(Duration::from_millis(100)) {
        QueueRecv::Item(item) => assert_eq!(item, 20, "the newest work survives"),
        _ => panic!("queue must deliver the surviving newest item"),
    }

    let counters = queue.counters();
    assert_eq!(counters.replaced_dropped_total, 1, "the drop is counted");
    assert_eq!(counters.queued_total, 2);
    assert_eq!(counters.current_depth, 0);

    queue.close();
    match queue.recv_timeout(Duration::from_millis(10)) {
        QueueRecv::Closed => {}
        _ => panic!("closed queue must report Closed"),
    }
}
