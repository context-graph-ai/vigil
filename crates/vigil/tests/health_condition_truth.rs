//! The ratified liveness contract: an unmanaged run answers the watchdog with
//! a live 2xx code and carries its unmanaged statement on every `/health`
//! answer; a LATER, independent failure (a camera that stopped ingesting, a
//! full disk) is reported as the genuinely dead/wedged state it is, with that
//! state's own liveness code — laundering it into the unmanaged state would
//! hide a real, restart-fixable fault behind a state nobody can act on; and
//! recovery from such a failure on an unmanaged run returns to
//! `RunningUnmanaged`, never to the ordinary `Ready` a store-backed run
//! recovers to — a box that cannot remember anything must never present as
//! an ordinary box.
//!
//! These assert against `HealthStatus::liveness_status_code()` and
//! `HealthState::healthy_baseline()` directly — the two pieces the `/health`
//! response body is actually built from (`liveness_code` delegates to the
//! former; a caller recovering from a fault calls the latter, never a
//! hardcoded `Ready`) — rather than a bare `HealthState::set`/`snapshot`
//! round-trip, which proves nothing about the contract itself. The real HTTP
//! round trip (a live 200 with the unmanaged line, then a real 503 once
//! ingest fails against a closed loopback port) is covered end to end by
//! `the_liveness_answer_stays_protected_while_the_body_names_the_real_condition`
//! in `crates/vigil-bin/tests/storeless_runtime_degraded_mode.rs`; these tests
//! pin the same contract at the unit level so it stays provable without a
//! socket.
//!
//! Why these are unfakeable: each assertion binds the status this test set to
//! both `liveness_status_code()` (what the watchdog actually reads) and
//! `unmanaged_statement()` (what the body actually appends) — a change that
//! kept the label right but broke either mapping fails here, and so does one
//! that hardcodes `Ready` on recovery instead of routing through
//! `healthy_baseline()`.

use vigil::settings_degraded::unmanaged_statement;
use vigil::{HealthState, HealthStatus};

#[test]
fn an_unmanaged_run_answers_200_with_the_unmanaged_statement_carried() {
    let health = HealthState::new();
    health.declare_unmanaged(unmanaged_statement());
    health.set(
        health.healthy_baseline(),
        "store unreadable: live view, detection and alerting are running unmanaged",
    );

    let (status, _detail) = health.snapshot();
    assert_eq!(
        status,
        HealthStatus::RunningUnmanaged,
        "a successful storeless start records RunningUnmanaged, not Ready"
    );
    assert_eq!(
        status.liveness_status_code(),
        200,
        "an unmanaged run must answer the watchdog with a live 2xx code -- restarting it cannot \
         make the store readable, so a non-2xx answer would take working cameras down for nothing"
    );
    assert_eq!(
        health.unmanaged_statement(),
        Some(unmanaged_statement()),
        "the /health body appends this exact statement on every answer for as long as the run is \
         unmanaged"
    );
}

#[test]
fn a_later_independent_failure_on_an_unmanaged_run_answers_its_own_liveness_code() {
    let health = HealthState::new();
    health.declare_unmanaged(unmanaged_statement());
    health.set(health.healthy_baseline(), "store unreadable");

    // The camera thread then finds its stream is gone -- a camera fact, not
    // a store fact, and it happened after the store was already known to be
    // unreadable.
    health.set(HealthStatus::IngestFailed, "RTSP ingest failed");
    let (status, detail) = health.snapshot();
    assert_eq!(
        status,
        HealthStatus::IngestFailed,
        "an ingest failure on an unmanaged run is an ingest failure: laundering it into the \
         unmanaged state tells an operator looking for the fault that nothing is wrong except the \
         store"
    );
    assert_eq!(detail, "RTSP ingest failed");
    assert_eq!(
        status.liveness_status_code(),
        503,
        "ingest failure is a genuinely dead/wedged state a restart can help; the watchdog must see \
         the failing code, not the unmanaged run's usual 200"
    );
    assert_eq!(
        health.unmanaged_statement(),
        Some(unmanaged_statement()),
        "unmanaged is a standing fact about the run and is not spent by reporting a second \
         condition: it must still be there for every surface that restates it"
    );

    // A second, unrelated dead state proves this is not special-cased to
    // IngestFailed alone.
    health.set(HealthStatus::DiskFull, "no space left");
    assert_eq!(
        health.snapshot().0.liveness_status_code(),
        503,
        "DiskFull fails the NVR's primary recording contract and is its own dead state too"
    );
}

#[test]
fn recovery_on_an_unmanaged_run_returns_to_running_unmanaged_not_ready() {
    let health = HealthState::new();
    health.declare_unmanaged(unmanaged_statement());
    health.set(health.healthy_baseline(), "store unreadable");

    health.set(HealthStatus::KeepPaceFailed, "detector queue fell behind");
    assert_eq!(health.snapshot().0, HealthStatus::KeepPaceFailed);

    // The pipeline recovers. A caller recovering from a fault calls
    // `healthy_baseline()` -- never a hardcoded `Ready` -- so a run with no
    // store behind it returns to declaring itself unmanaged, not to the
    // ordinary `Ready` state a healthy store-backed run recovers to.
    health.set(health.healthy_baseline(), "RTSP ingest active");
    let (status, _detail) = health.snapshot();
    assert_eq!(
        status,
        HealthStatus::RunningUnmanaged,
        "recovery on an unmanaged run must land back on RunningUnmanaged, never Ready -- a box \
         that cannot remember anything must never present as an ordinary box"
    );
    assert_eq!(status.liveness_status_code(), 200);
    assert_eq!(
        health.unmanaged_statement(),
        Some(unmanaged_statement()),
        "the statement survives a full fail-then-recover cycle unchanged"
    );
}

#[test]
fn an_ordinary_store_backed_run_recovers_to_ready_not_unmanaged() {
    // The other leg of `healthy_baseline`: a run that never declared itself
    // unmanaged returns to plain `Ready`, proving the baseline genuinely
    // depends on the standing declaration rather than always landing on
    // `RunningUnmanaged`.
    let health = HealthState::new();
    assert_eq!(
        health.unmanaged_statement(),
        None,
        "this run never declared itself unmanaged"
    );

    health.set(HealthStatus::IngestFailed, "RTSP ingest failed");
    assert_eq!(health.snapshot().0, HealthStatus::IngestFailed);

    health.set(health.healthy_baseline(), "RTSP ingest active");
    let (status, _detail) = health.snapshot();
    assert_eq!(status, HealthStatus::Ready);
    assert_eq!(status.liveness_status_code(), 200);
}
