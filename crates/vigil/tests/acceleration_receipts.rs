//! The shared acceleration receipt model and its surfaces: fixed field
//! vocabulary, honest detector receipts on a CPU-only artifact, active
//! backends visible in stats, degraded health when configured-but-fallback,
//! and bounded logging.

use std::collections::BTreeMap;

use vigil::acceleration::{
    AccelStage, AccelerationReceipt, AccelerationState, ActionKind, EvidenceKind, FailureCode,
    ProbeStatus, SELECTION_EPOCH_FIELD, render_receipt_block,
};
use vigil::workgraph::StreamId;

fn decode_receipt(stream: &str, probe_status: ProbeStatus) -> AccelerationReceipt {
    AccelerationReceipt {
        stage: AccelStage::Decode,
        work_id: None,
        parent_work_id: None,
        stream_id: Some(StreamId::new(stream)),
        media_item: None,
        configured: !matches!(probe_status, ProbeStatus::Disabled),
        attempted_backend: "gstreamer".to_string(),
        active_backend: if matches!(probe_status, ProbeStatus::Active) {
            "gstreamer".to_string()
        } else {
            "software".to_string()
        },
        hardware_accelerated: matches!(probe_status, ProbeStatus::Active),
        selected_device: matches!(probe_status, ProbeStatus::Active)
            .then(|| "/dev/dri/renderD0".to_string()),
        codec: Some("H264".to_string()),
        model_id: None,
        model_version: None,
        input_shape: None,
        probe_status,
        failure_code: if matches!(probe_status, ProbeStatus::Fallback) {
            FailureCode::PermissionDenied
        } else {
            FailureCode::None
        },
        evidence_kind: matches!(probe_status, ProbeStatus::Fallback)
            .then_some(EvidenceKind::DevicePath),
        evidence_fields: BTreeMap::new(),
        action_kind: if matches!(probe_status, ProbeStatus::Fallback) {
            ActionKind::RunCommand
        } else {
            ActionKind::NoAction
        },
        action_payload: None,
    }
}

fn detection_receipt(configured: bool, accelerated_compiled: bool) -> AccelerationReceipt {
    AccelerationReceipt {
        stage: AccelStage::Detection,
        work_id: None,
        parent_work_id: None,
        stream_id: None,
        media_item: None,
        configured,
        attempted_backend: if accelerated_compiled {
            "burn-accelerated".to_string()
        } else {
            "burn-cpu".to_string()
        },
        active_backend: "burn-cpu".to_string(),
        hardware_accelerated: false,
        selected_device: None,
        codec: None,
        model_id: Some("detector-model".to_string()),
        model_version: None,
        input_shape: Some("1x3x416x416".to_string()),
        probe_status: if configured {
            ProbeStatus::Fallback
        } else {
            ProbeStatus::Disabled
        },
        failure_code: if configured {
            FailureCode::BackendNotCompiled
        } else {
            FailureCode::None
        },
        evidence_kind: configured.then_some(EvidenceKind::SelectedBackend),
        evidence_fields: BTreeMap::new(),
        action_kind: if configured {
            ActionKind::InstallSupportedArtifact
        } else {
            ActionKind::NoAction
        },
        action_payload: None,
    }
}

#[test]
fn receipt_carries_fixed_field_vocabulary() {
    let mut receipt = decode_receipt("front-yard", ProbeStatus::Fallback);
    receipt
        .evidence_fields
        .insert("path".to_string(), "/dev/dri/renderD0".to_string());
    receipt
        .evidence_fields
        .insert("owner".to_string(), "root".to_string());
    receipt.action_payload = Some("usermod -aG render <service-user>".to_string());

    let block = render_receipt_block(&receipt);

    // The rendered block is the seed's fixed operator format.
    assert!(block.starts_with("[decode.hardware]\n"), "block: {block}");
    for required_line in [
        "configured: true",
        "status: fallback",
        "attempted_backend: gstreamer",
        "active_backend: software",
        "failure_code: permission_denied",
        "evidence_kind: device_path",
        "evidence_fields:",
        "  path: /dev/dri/renderD0",
        "  owner: root",
        "action_kind: run_command",
        "action_payload:",
        "  usermod -aG render <service-user>",
    ] {
        assert!(
            block.contains(required_line),
            "missing `{required_line}` in rendered block:\n{block}"
        );
    }

    let detection = render_receipt_block(&detection_receipt(true, false));
    assert!(
        detection.starts_with("[detect.acceleration]\n"),
        "block: {detection}"
    );
    assert!(detection.contains("failure_code: backend_not_compiled"));
    assert!(detection.contains("model_id: detector-model"));
    assert!(detection.contains("input_shape: 1x3x416x416"));
}

#[test]
fn cpu_only_artifact_with_accel_true_reports_backend_not_compiled_fallback() {
    // accelerated_detection=true on an artifact with no accelerated Burn
    // backend compiled: the receipt says so, honestly, with CPU fallback
    // active — the artifact never silently pretends.
    let receipt = detection_receipt(true, false);
    assert!(receipt.configured);
    assert_eq!(receipt.probe_status, ProbeStatus::Fallback);
    assert_eq!(receipt.failure_code, FailureCode::BackendNotCompiled);
    assert_eq!(receipt.active_backend, "burn-cpu");
    assert!(!receipt.hardware_accelerated);
}

#[test]
fn accel_false_is_disabled_not_fallback_and_they_are_distinct() {
    // An intentional CPU-only configuration must be observably different
    // from a wanted-but-unavailable acceleration.
    let intentional = detection_receipt(false, false);
    let unavailable = detection_receipt(true, false);

    assert_eq!(intentional.probe_status, ProbeStatus::Disabled);
    assert_eq!(intentional.failure_code, FailureCode::None);
    assert_eq!(unavailable.probe_status, ProbeStatus::Fallback);
    assert_eq!(unavailable.failure_code, FailureCode::BackendNotCompiled);

    let intentional_block = render_receipt_block(&intentional);
    let unavailable_block = render_receipt_block(&unavailable);
    assert_ne!(
        intentional_block, unavailable_block,
        "disabled-by-intent and fallback-from-intent must not look identical"
    );
    assert!(intentional_block.contains("status: disabled"));
    assert!(unavailable_block.contains("status: fallback"));
}

#[test]
fn stats_show_active_decoder_per_stream_and_detector_backend() {
    let state = AccelerationState::new();
    state.record(decode_receipt("front-yard", ProbeStatus::Active));
    state.record(decode_receipt("garage", ProbeStatus::Fallback));
    state.record(detection_receipt(true, false));

    assert_eq!(
        state
            .active_decoder(&StreamId::new("front-yard"))
            .as_deref(),
        Some("gstreamer"),
        "per-stream active decoder comes from the observed receipt"
    );
    assert_eq!(
        state.active_decoder(&StreamId::new("garage")).as_deref(),
        Some("software"),
        "the fallen-back stream shows its real active backend"
    );
    assert_eq!(state.active_detector_backend().as_deref(), Some("burn-cpu"));
}

#[test]
fn health_reports_degraded_decode_and_detection_when_configured_but_fallback() {
    let state = AccelerationState::new();

    // Nothing degraded when hardware decode is active and detection was
    // intentionally disabled.
    state.record(decode_receipt("front-yard", ProbeStatus::Active));
    state.record(detection_receipt(false, false));
    assert!(
        state.health_degradations().is_empty(),
        "active + intentionally-disabled is not degraded"
    );

    // Configured-but-fallback IS degraded — for each stage independently.
    state.record(decode_receipt("front-yard", ProbeStatus::Fallback));
    state.record(detection_receipt(true, false));
    let degradations = state.health_degradations();
    assert!(
        degradations
            .iter()
            .any(|d| d.stage == AccelStage::Decode && !d.reason.is_empty()),
        "hardware_decoding=true with software active must surface degraded decode: {degradations:?}"
    );
    assert!(
        degradations
            .iter()
            .any(|d| d.stage == AccelStage::Detection && !d.reason.is_empty()),
        "accelerated_detection=true with CPU active must surface degraded detection: {degradations:?}"
    );
}

#[test]
fn logs_are_bounded_per_stream_selection_plus_fallback() {
    let state = AccelerationState::new();

    // First selection line for a stream logs.
    let first = decode_receipt("front-yard", ProbeStatus::Active);
    assert!(state.should_log(&first), "first selection line logs");
    state.record(first.clone());

    // The same outcome repeated does not spam.
    assert!(
        !state.should_log(&first),
        "an unchanged outcome must not log per segment/frame"
    );

    // A CHANGED outcome (fallback) logs again — bounded, per reason.
    let fallback = decode_receipt("front-yard", ProbeStatus::Fallback);
    assert!(state.should_log(&fallback), "a state change logs");
    state.record(fallback.clone());
    assert!(
        !state.should_log(&fallback),
        "the same fallback reason must not repeat unbounded"
    );
}

#[test]
fn every_decode_selection_epoch_emits_its_own_proof_line() {
    // An operator who changes the decode path is entitled to read, in the
    // pipeline's own words, which path the camera came back on. The only
    // per-selection evidence this build emits is the selection line, and a
    // reverse lands on an outcome an earlier session already reported — same
    // stream, codec, status and backend — so an outcome-only bound swallows
    // it and the reverse cannot be told from nothing happening at all.
    //
    // The session a receipt belongs to is what distinguishes them: a camera
    // that reconnects under a newly named decode path opens a new session
    // under a new stream epoch. So the discriminating case here is a RETURN to
    // a previously seen outcome under a NEW epoch, which is exactly the case
    // the bound above never reaches.
    let state = AccelerationState::new();

    let software_first_session = {
        let mut receipt = decode_receipt("front-yard", ProbeStatus::Fallback);
        receipt
            .evidence_fields
            .insert(SELECTION_EPOCH_FIELD.to_string(), "1".to_string());
        receipt
    };
    assert!(
        state.should_log(&software_first_session),
        "the first session's selection is a first outcome and logs"
    );
    state.record(software_first_session.clone());

    // Within that one session the same outcome repeats per segment, and the
    // bound the budget exists for still holds.
    assert!(
        !state.should_log(&software_first_session),
        "an unchanged outcome inside one session must not log per segment"
    );

    // The operator pins hardware: a new session, a different outcome.
    let hardware_session = {
        let mut receipt = decode_receipt("front-yard", ProbeStatus::Active);
        receipt
            .evidence_fields
            .insert(SELECTION_EPOCH_FIELD.to_string(), "2".to_string());
        receipt
    };
    assert!(state.should_log(&hardware_session), "a new path logs");
    state.record(hardware_session);

    // And the reverse: a genuinely new, working session that happens to land
    // on the outcome the first one reported. It is its own event and says so.
    let software_after_reverse = {
        let mut receipt = decode_receipt("front-yard", ProbeStatus::Fallback);
        receipt
            .evidence_fields
            .insert(SELECTION_EPOCH_FIELD.to_string(), "3".to_string());
        receipt
    };
    assert!(
        state.should_log(&software_after_reverse),
        "a fresh session that returns to a previously seen decode outcome must still emit its \
         own selection proof: without it an operator reading the log cannot tell the reverse \
         happened from nothing happening, and the only remaining evidence is a stored value"
    );
    state.record(software_after_reverse.clone());
    assert!(
        !state.should_log(&software_after_reverse),
        "and that fresh session is still bounded to one line: the per-frame spam the budget \
         exists to prevent must not come back with it"
    );
}
