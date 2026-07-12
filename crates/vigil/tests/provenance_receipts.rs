//! Provenance + honesty receipts (criterion C7): every remotely-executed
//! detection carries executor node + backend; the fabric-status / work-
//! receipt / offload lines render IDENTICALLY through stats, doctor, and
//! health surfaces — ONE vocabulary (USR-10/AGT-12), never a per-surface
//! reimplementation whose divergence could go unnoticed.
//!
//! RED: `vigil::offload_policy::render_remote_detection_receipt` and
//! `::render_remote_detection_receipt_for_every_surface` are `todo!()`
//! pending the implementation pass.

#![cfg(feature = "fabric")]

use vigil::offload_policy::{
    render_remote_detection_receipt, render_remote_detection_receipt_for_every_surface,
};

#[test]
fn remote_detection_carries_node_and_backend_one_vocabulary() {
    let line = render_remote_detection_receipt("home-frigate", "burn-cpu");
    assert!(
        line.contains("home-frigate"),
        "the receipt must carry the executor node id: {line}"
    );
    assert!(
        line.contains("burn-cpu"),
        "the receipt must carry the truthful backend: {line}"
    );

    // Content-faithful: different inputs render different text, never a
    // fixture-keyed constant.
    let other = render_remote_detection_receipt("lucid-laptop", "burn-wgpu");
    assert_ne!(
        line, other,
        "the rendered line must reflect the actual node + backend, not a fixed string"
    );

    // Deterministic: the same inputs always render the SAME line (no hidden
    // per-call state — a fresh-eyes review can diff two calls to prove
    // divergence).
    assert_eq!(
        render_remote_detection_receipt("home-frigate", "burn-cpu"),
        line
    );

    // One vocabulary across every surface (stats, doctor, health): each
    // must call the SAME renderer, nothing else — so the three lines are
    // identical by construction. A surface that grew its own per-surface
    // formatting would diverge here.
    let [stats_line, doctor_line, health_line] =
        render_remote_detection_receipt_for_every_surface("home-frigate", "burn-cpu");
    assert_eq!(
        stats_line, line,
        "stats must render the same provenance line as the canonical renderer"
    );
    assert_eq!(
        doctor_line, line,
        "doctor must render the same provenance line as the canonical renderer"
    );
    assert_eq!(
        health_line, line,
        "health must render the same provenance line as the canonical renderer"
    );
}
