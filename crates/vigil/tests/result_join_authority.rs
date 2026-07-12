//! Result join + authority (criterion C4): remote results join back to
//! stream/frame identity through the existing typed envelope vocabulary
//! (`validate_result_join`); class-map authority and NMS ownership stay
//! vigil-side and identical for local and remote results. Duplicate/late
//! results are idempotent (exactly-once apply); a result arriving after a
//! local fallback already handled the segment is discarded with a receipt,
//! never double-counted.
//!
//! `workgraph::StageAttempt::finish`'s own doc note is explicit: in-process,
//! a mirrored result can never fail its own join — the reject path is only
//! falsifiable once a result arrives from OUTSIDE the process (a machine
//! boundary), which is exactly what this fixture constructs.
//!
//! RED: `vigil::fabric::PendingOffloads::apply_remote_result` (and
//! `mark_resolved_by_fallback`) are `todo!()` pending the implementation
//! pass.

#![cfg(feature = "fabric")]

use chrono::Utc;
use vigil::fabric::{PendingOffloads, RemoteResultOutcome};
use vigil::workgraph::{
    JoinRejection, MediaItemId, ReceiptId, ResultEnvelope, StageId, StageReceipt, StageReceiptLog,
    StreamId, WorkDeadline, WorkDisposition, WorkEnvelope, WorkId, WorkOrdering, WorkPriority,
};

const SCHEMA_VERSION: u32 = 1;

fn work_envelope(work_id: WorkId, stream_sequence: u64) -> WorkEnvelope {
    let now = Utc::now();
    WorkEnvelope {
        work_id,
        parent_work_id: None,
        contributing_work_ids: vec![],
        stage: StageId::new("detection"),
        stream_id: StreamId::new("front-yard"),
        media_item: Some(MediaItemId::Segment {
            segment_sequence: stream_sequence,
        }),
        ordering: WorkOrdering {
            stream_epoch: 1,
            stream_sequence,
        },
        observed_at: Some(now),
        received_at: now,
        priority: WorkPriority::MOTION,
        deadline: WorkDeadline::None,
        schema_version: SCHEMA_VERSION,
    }
}

/// A result + receipt that legitimately answers `work` — the shape a
/// well-formed remote node's `DetectorResult` translates into.
fn matching_result_and_receipt(work: &WorkEnvelope) -> (ResultEnvelope, StageReceipt) {
    let receipt_id = ReceiptId::generate();
    let result = ResultEnvelope {
        work_id: work.work_id,
        parent_work_id: work.parent_work_id,
        contributing_work_ids: work.contributing_work_ids.clone(),
        stage: work.stage.clone(),
        stream_id: work.stream_id.clone(),
        media_item: work.media_item,
        ordering: work.ordering,
        observed_at: work.observed_at,
        result_schema_version: work.schema_version,
        receipt_id,
    };
    let receipt = StageReceipt {
        receipt_id,
        work_id: work.work_id,
        parent_work_id: work.parent_work_id,
        stage: work.stage.clone(),
        stream_id: work.stream_id.clone(),
        configured_backend: None,
        attempted_backend: Some("burn-cpu".to_string()),
        active_backend: Some("burn-cpu".to_string()),
        fallback_backend: None,
        selected_device: None,
        probe_result: None,
        fallback_reason: None,
        started_at: Utc::now(),
        ended_at: Utc::now(),
        output_count: 1,
        disposition: WorkDisposition::Completed,
    };
    (result, receipt)
}

#[test]
fn remote_result_joins_or_is_rejected_and_is_idempotent() {
    let log = StageReceiptLog::new(16);

    // 1) A well-formed remote result joins and applies exactly once.
    let work_id = WorkId::generate();
    let work = work_envelope(work_id, 1);
    let pending = PendingOffloads::new();
    pending.track("job-detect-a".to_string(), work.clone());
    let (result, receipt) = matching_result_and_receipt(&work);

    let outcome = pending.apply_remote_result("job-detect-a", &result, &receipt, &log);
    assert!(
        matches!(outcome, RemoteResultOutcome::Applied),
        "a well-formed remote result must join and apply: {outcome:?}"
    );
    assert_eq!(
        log.rejected_joins(),
        0,
        "a valid join must never count as rejected"
    );

    // A duplicate identical remote result for the SAME job must be
    // idempotent: applied already, so the second offer is a no-op, never a
    // double count.
    let outcome_again = pending.apply_remote_result("job-detect-a", &result, &receipt, &log);
    assert!(
        matches!(outcome_again, RemoteResultOutcome::DiscardedLate),
        "a duplicate result for an already-applied job must discard, never re-apply: \
         {outcome_again:?}"
    );

    // 2) An envelope mismatch (wrong stream) is rejected via
    // validate_result_join, counted, and produces no events.
    let work_id_b = WorkId::generate();
    let work_b = work_envelope(work_id_b, 2);
    pending.track("job-detect-b".to_string(), work_b.clone());
    let (mut mismatched_result, mismatched_receipt) = matching_result_and_receipt(&work_b);
    mismatched_result.stream_id = vigil::workgraph::StreamId::new("back-yard");

    let before_rejected = log.rejected_joins();
    let outcome = pending.apply_remote_result(
        "job-detect-b",
        &mismatched_result,
        &mismatched_receipt,
        &log,
    );
    match outcome {
        RemoteResultOutcome::RejectedJoin(JoinRejection::StreamMismatch) => {}
        other => panic!("a stream-mismatched result must be rejected as StreamMismatch: {other:?}"),
    }
    assert_eq!(
        log.rejected_joins(),
        before_rejected + 1,
        "a rejected join must be counted, never silent"
    );

    // 3) Degraded-never-dead composition (criterion C5): once a local
    // fallback has already resolved a segment, a LATE remote result for the
    // same job discards — never double-counted — even though it would
    // otherwise validly join.
    let work_id_c = WorkId::generate();
    let work_c = work_envelope(work_id_c, 3);
    pending.track("job-detect-c".to_string(), work_c.clone());
    pending.mark_resolved_by_fallback("job-detect-c");
    let (late_result, late_receipt) = matching_result_and_receipt(&work_c);

    let outcome = pending.apply_remote_result("job-detect-c", &late_result, &late_receipt, &log);
    assert!(
        matches!(outcome, RemoteResultOutcome::DiscardedLate),
        "a remote result racing an already-resolved local fallback must discard, never \
         double-count: {outcome:?}"
    );
}
