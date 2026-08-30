//! The review plane an operator browses must never compete with the operator's
//! own command line for the owner's attention.
//!
//! The owner channel a running Vigil publishes has a fixed, small number of
//! reader slots — four, the ceiling context-graph declares for every store it
//! opens. `vigil why`, `vigil events` and `vigil stats` arrive through those
//! slots, and they are what an operator falls back on when something is wrong.
//! The HTTP review plane runs INSIDE the owning process and already holds the
//! store, so it has no reason to ask the owner anything — and if it did, a
//! browser left open on the review page would be spending the operator's own
//! slots.
//!
//! The proof is occupation, not inference. Four external readers are parked
//! inside the owner's handler at once, so every slot is genuinely taken; the
//! review plane is then asked for a page and must still answer. That the slots
//! really were full is proved in the same breath by a fifth reader, which is
//! refused with the typed at-capacity error rather than served. Everything
//! asserted here is a counter read or a typed refusal after a deterministic
//! drive — nothing is inferred from how long anything took.
//!
//! This is a file of its own rather than another case inside
//! `http_data_plane.rs` because it opens the store through the owner-carrying
//! door; `http_data_plane.rs` opens plain stores and must keep compiling
//! untouched if that door ever moves.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use context_graph::owner_control::request_owner;
use context_graph::{ControlHandler, EmbedderConfig, OwnerControlError, Store, StoreConfig};
use vigil::{
    EVENTS_ROUTE, PersistedClock, ReviewDataPlaneHandle, spawn_review_data_plane_with_clock,
};

#[path = "deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    HttpResponse, TcpPortReservation, fresh_store_copy, get, wait_until,
};

/// Detections in the seeded store, so the review plane has real rows to render
/// rather than an empty answer that could pass without reading anything.
const SEEDED_DETECTIONS: usize = 2;

/// Every owner reader slot context-graph gives a store. Declared upstream as
/// the default owner read concurrency; a deployment cannot move it, which is
/// exactly why the review plane must not spend any of them.
const OWNER_READER_SLOTS: usize = 4;

/// How long a parked reader is allowed to take to reach the handler before the
/// test gives up and releases everyone. A bound on a hang, never a
/// synchronization step — the assertions all read state.
const OCCUPATION_TIMEOUT: Duration = Duration::from_secs(30);

/// A handler that parks every frame it is asked to run until it is released,
/// counting arrivals as it goes. A frame sitting in here is a reader slot
/// genuinely held, not one assumed to be held.
struct ParkingHandler {
    arrived: Arc<AtomicUsize>,
    released: Arc<AtomicBool>,
}

impl ParkingHandler {
    fn install() -> (ControlHandler, Self) {
        let arrived = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(false));
        let handler_arrived = Arc::clone(&arrived);
        let handler_released = Arc::clone(&released);
        let handler: ControlHandler = Arc::new(move |frame: String| {
            handler_arrived.fetch_add(1, Ordering::SeqCst);
            while !handler_released.load(Ordering::SeqCst) {
                thread::yield_now();
            }
            format!("owner-answered:{}", frame.trim())
        });
        (handler, Self { arrived, released })
    }

    fn arrived(&self) -> usize {
        self.arrived.load(Ordering::SeqCst)
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }
}

fn open_owning_store(store_path: &Path, handler: ControlHandler) -> Store {
    Store::open_with_control_handler(
        StoreConfig {
            db_path: store_path.to_path_buf(),
            default_text_embedder: Some(EmbedderConfig::disabled()),
            ..StoreConfig::default()
        },
        handler,
    )
    .expect("open the store that owns this deployment and serves owner requests")
}

fn spawn_plane(store: Store, data_dir: &Path) -> (ReviewDataPlaneHandle, u16) {
    let reservation =
        TcpPortReservation::reserve_loopback().expect("reserve the review plane's port");
    let port = reservation.release();
    let handle = spawn_review_data_plane_with_clock(
        store,
        data_dir.to_path_buf(),
        port,
        PersistedClock::contextdb(),
    )
    .expect("spawn the review data plane");
    let port = handle.local_addr().port();
    deterministic_fixture_support::wait_for_tcp_port(port, Duration::from_secs(2))
        .expect("the review data plane must open its TCP port");
    (handle, port)
}

fn assert_served(response: &HttpResponse, label: &str) {
    assert_eq!(
        response.status,
        200,
        "the review plane must serve {label}; got status {} body:\n{}",
        response.status,
        response.body_text()
    );
}

#[test]
fn the_review_plane_still_answers_while_every_owner_reader_slot_is_occupied() {
    let (tmp, store_path) =
        fresh_store_copy(SEEDED_DETECTIONS).expect("seed a deterministic detection fixture");
    let data_dir = store_path
        .parent()
        .expect("the seeded store sits inside a data directory")
        .to_path_buf();
    let (handler, parking) = ParkingHandler::install();
    let store = open_owning_store(&store_path, handler);
    let (plane, port) = spawn_plane(store, &data_dir);

    // Park one external reader in every slot the owner has.
    let answers: Arc<Mutex<Vec<Result<String, OwnerControlError>>>> =
        Arc::new(Mutex::new(Vec::new()));
    let mut parked = Vec::new();
    for slot in 0..OWNER_READER_SLOTS {
        let path = store_path.clone();
        let sink = Arc::clone(&answers);
        parked.push(thread::spawn(move || {
            let answer = request_owner(&path, &format!("stats slot-{slot}\n"));
            sink.lock().expect("collected owner answers").push(answer);
        }));
    }
    let occupied = wait_until(
        "every owner reader slot to be occupied by a parked reader",
        OCCUPATION_TIMEOUT,
        || Ok((parking.arrived() >= OWNER_READER_SLOTS).then_some(())),
    );
    if occupied.is_err() {
        parking.release();
        for thread in parked {
            let _ = thread.join();
        }
        panic!(
            "only {} of {OWNER_READER_SLOTS} owner readers reached the handler, so this run never \
             established the full-slot condition it exists to test",
            parking.arrived()
        );
    }

    // Every slot is held. The review plane still answers, because it never
    // needed one.
    let served = get(port, EVENTS_ROUTE);

    // The same breath: a fifth reader cannot get in, which is what proves the
    // four really are held rather than merely counted.
    let refused = request_owner(&store_path, "stats overflow\n");

    parking.release();
    for thread in parked {
        thread.join().expect("a parked owner reader panicked");
    }

    assert_served(
        &served,
        "the review events route while every owner slot was held",
    );
    assert!(
        !served.body_text().trim().is_empty(),
        "the review plane answered with nothing while the owner slots were held — it must serve \
         the real page from the store it already holds"
    );

    match refused {
        Err(OwnerControlError::OwnerAtCapacity { .. }) => {}
        other => panic!(
            "a fifth owner reader was not refused at capacity, so the four parked readers were \
             not holding every slot and this run proved nothing about the review plane; got \
             {other:?}"
        ),
    }

    let collected = answers.lock().expect("collected owner answers");
    assert_eq!(
        collected.len(),
        OWNER_READER_SLOTS,
        "every parked owner reader must have finished once released"
    );
    for answer in collected.iter() {
        let answer = answer
            .as_ref()
            .expect("a parked owner reader must be served once the handler is released");
        assert!(
            answer.starts_with("owner-answered:"),
            "the owner's own handler must have produced the answer; got {answer:?}"
        );
    }
    assert_eq!(
        parking.arrived(),
        OWNER_READER_SLOTS,
        "the review plane ran the owner handler — every frame it sends is a reader slot taken \
         away from `vigil why` and `vigil events`"
    );

    drop(plane);
    drop(tmp);
}
