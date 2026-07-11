//! The Home Assistant Supervisor watchdog polls the add-on's health endpoint
//! (`addons/vigil/config.yaml` `watchdog`) and stops+restarts the add-on on any
//! non-2xx response. That endpoint's status code must therefore be a LIVENESS
//! signal: 2xx whenever the runtime is alive and its capture/detection pipeline
//! is functioning, EVEN WHEN degraded — the owner-witnessed case is CPU-fallback
//! detection whose queue has fallen behind the stream on a weak box. A
//! degraded-but-alive box that answers non-2xx gets stopped, restarted, falls
//! behind again, and loops forever (Supervisor "Watchdog missing application
//! response"). Non-2xx is reserved for genuinely dead/wedged states where a
//! restart can actually help.
//!
//! This pins the liveness partition as a public, tested contract via
//! `HealthStatus::liveness_status_code`. The precise degraded state stays named
//! in the `/health` body (status label + detail + acceleration receipts), which
//! the existing endpoint tests cover and this fix leaves unchanged.

use vigil::HealthStatus;

fn is_2xx(code: u16) -> bool {
    (200..300).contains(&code)
}

#[test]
fn detector_behind_on_cpu_fallback_stays_alive_for_the_watchdog() {
    // On a weak box the detector falls back to burn-cpu and its queue falls
    // behind the stream (HealthStatus::KeepPaceFailed, "detector queue fell
    // behind"). The process is alive and decode works; detection is merely
    // slow. The watchdog-facing status code must be 2xx so the Supervisor does
    // not stop+restart a healthy-but-degraded box into a permanent loop.
    let code = HealthStatus::KeepPaceFailed.liveness_status_code();
    assert!(
        is_2xx(code),
        "a detector that has fallen behind on CPU-fallback detection is degraded, not dead: the watchdog endpoint must answer 2xx so the add-on is never restart-looped, but the status code was {code}"
    );
}

#[test]
fn ready_runtime_is_alive_for_the_watchdog() {
    assert_eq!(
        HealthStatus::Ready.liveness_status_code(),
        200,
        "a ready runtime must answer 200 to the watchdog"
    );
}

#[test]
fn genuinely_dead_states_answer_non_2xx() {
    // A restart can plausibly help these: the store never opened, or the
    // ingest/detector pipeline failed or panicked. The watchdog SHOULD see a
    // non-2xx here so the Supervisor restarts a genuinely wedged add-on.
    for dead in [HealthStatus::StoreOpenFailed, HealthStatus::IngestFailed] {
        let code = dead.liveness_status_code();
        assert!(
            !is_2xx(code),
            "{dead:?} is a genuinely dead/wedged state a restart can help: the watchdog endpoint must answer non-2xx, but the status code was {code}"
        );
    }
}
