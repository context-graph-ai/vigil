//! Promotion re-advertise (deferral VD-23, working doc: "on a wgpu node
//! that promotes after startup, the ADVERTISED tag stays burn-cpu until
//! restart"). `runtime.rs`'s live-promotion `promote` closure
//! (runtime.rs:643-668) swaps the running `PromotableDetector` handle in
//! place so every camera thread's DETECTION calls immediately use the
//! accelerated backend — but nothing re-drives the fabric worker loop's
//! OWN capability advertisement, so the ledger keeps telling the fleet this
//! node is `vigil-detector-burn-cpu` forever, even after a real promotion.
//!
//! Simulates a promotion WITHOUT a real GPU by driving the exact mechanism
//! the fabric worker loop uses to advertise a backend at all —
//! `FabricRuntime::spawn_worker_loop` — a second time with a different
//! truthful `FabricDetectorBackend` (the way a fix would re-advertise after
//! a live promotion), and asserts the write-once contract: a NEW
//! `vigil-detector-burn-wgpu` row must exist, and the original
//! `vigil-detector-burn-cpu` row must still be there (additive, never
//! edited/removed).
//!
//! RED today for a compounding, already-real reason worth flagging on its
//! own (same root cause `cameraless_worker.rs` names): `spawn_worker_loop`
//! never advertises `vigil-detector-<backend>` at all — it delegates to
//! contextdb's `work_ledger::run_worker_loop`, which advertises the FIXED
//! literal capability id `"worker-loop"` (contextdb-server/src/
//! work_ledger.rs:590), and `advertise_capability` is idempotent per
//! `(node_id, capability_id)` (working doc: "re-advertising with changed
//! tags is a silent no-op") — so calling `spawn_worker_loop` a second time
//! with a different backend cannot create a new row under the CURRENT
//! capability id scheme even once the id itself is corrected: two
//! independent gaps must both be fixed before this test passes.

#![cfg(feature = "fabric")]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use vigil::DecodedRgbFrame;
use vigil::detector_workclass::{DetectorDetection, OrderedF64};
use vigil::fabric::{FabricDetectorBackend, FabricRuntime};

async fn within<F: std::future::Future>(fut: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .expect("bounded fabric operation exceeded 30s")
}

struct SpyBackend {
    tag: &'static str,
}

impl FabricDetectorBackend for SpyBackend {
    fn backend_tag(&self) -> &str {
        self.tag
    }

    fn model_sha256(&self) -> &str {
        "spy-model-sha"
    }

    fn detect(
        &self,
        _frames: &[DecodedRgbFrame],
        _clip_sha256: &str,
        _sample_frames: usize,
        _confidence_threshold: f64,
    ) -> Result<Vec<DetectorDetection>, String> {
        Ok(vec![DetectorDetection {
            class_name: "person".to_string(),
            confidence: OrderedF64(0.8),
            bbox: "0,0,1,1".to_string(),
            frame_index: 0,
        }])
    }
}

/// This node's own advertised capability ids, straight off its ledger —
/// mirrors `FabricRuntime::remote_detector_capabilities`'s query but WITHOUT
/// excluding this node's own rows (that method is for REMOTE peers; a
/// promotion re-advertise is a claim about THIS node).
async fn own_capability_ids(runtime: &FabricRuntime) -> Vec<String> {
    let result = within(async {
        runtime.db.execute(
            "SELECT node_id, capability_id FROM work_capabilities",
            &HashMap::new(),
        )
    })
    .await
    .expect("scan work_capabilities");
    let node_idx = result
        .columns
        .iter()
        .position(|c| c == "node_id")
        .expect("node_id column");
    let capability_idx = result
        .columns
        .iter()
        .position(|c| c == "capability_id")
        .expect("capability_id column");
    result
        .rows
        .iter()
        .filter_map(|row| {
            let contextdb_core::Value::Text(node) = &row[node_idx] else {
                return None;
            };
            if node != &runtime.node_id {
                return None;
            }
            let contextdb_core::Value::Text(capability_id) = &row[capability_idx] else {
                return None;
            };
            Some(capability_id.clone())
        })
        .collect()
}

#[tokio::test]
async fn late_promotion_advertises_a_new_backend_row_and_keeps_the_old_one() {
    let dir = tempfile::tempdir().expect("data dir");
    let runtime = within(FabricRuntime::start(dir.path(), None, false))
        .await
        .expect("standalone node must stand up");

    // Cameraless bootstrap: the fabric worker loop starts on burn-cpu (the
    // real entry point runtime.rs's bring-up task calls once a detector is
    // available at all — see cameraless_worker.rs for the separate gap
    // that a camera-less node never reaches this call today).
    let cpu_backend = Arc::new(SpyBackend { tag: "burn-cpu" });
    let shutdown_cpu = Arc::new(AtomicBool::new(false));
    let _cpu_task = runtime.spawn_worker_loop(cpu_backend, Arc::clone(&shutdown_cpu));

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut ids = Vec::new();
    while Instant::now() < deadline {
        ids = own_capability_ids(&runtime).await;
        if ids.iter().any(|id| id == "vigil-detector-burn-cpu") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        ids.iter().any(|id| id == "vigil-detector-burn-cpu"),
        "cameraless bootstrap must advertise vigil-detector-burn-cpu before \
         any promotion can be simulated; rows found: {ids:?}"
    );

    // Simulated promotion (no real GPU): drive the SAME advertise mechanism
    // a second time with a different, truthful backend — exactly what a
    // fix re-driving the fabric worker loop after a live promotion would
    // do.
    let wgpu_backend = Arc::new(SpyBackend { tag: "burn-wgpu" });
    let shutdown_wgpu = Arc::new(AtomicBool::new(false));
    let _wgpu_task = runtime.spawn_worker_loop(wgpu_backend, Arc::clone(&shutdown_wgpu));

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut ids_after = Vec::new();
    while Instant::now() < deadline {
        ids_after = own_capability_ids(&runtime).await;
        if ids_after.iter().any(|id| id == "vigil-detector-burn-wgpu") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    shutdown_cpu.store(true, std::sync::atomic::Ordering::SeqCst);
    shutdown_wgpu.store(true, std::sync::atomic::Ordering::SeqCst);

    assert!(
        ids_after.iter().any(|id| id == "vigil-detector-burn-wgpu"),
        "a late promotion must advertise a NEW vigil-detector-burn-wgpu \
         capability row (write-once — a distinct id per backend, never an \
         edit of the old one); rows found: {ids_after:?}"
    );
    assert!(
        ids_after.iter().any(|id| id == "vigil-detector-burn-cpu"),
        "the original vigil-detector-burn-cpu row must still be present — \
         promotion is additive, never a removal/edit of the prior truthful \
         claim; rows found: {ids_after:?}"
    );
}
