//! Pins the camera-track hub's producer/consumer split: `publish` and
//! `begin_epoch` (today both `&self` on the single `CameraTrackHub` type,
//! `crates/vigil/src/camera_hub.rs:439`/`:551`) move onto a distinct,
//! EXCLUSIVE producer handle, while `subscribe` (`:593`) stays on a
//! cheaply cloneable, shareable consumer handle. The product rule this
//! makes structural: one owner supervises each source role — a single
//! component opens, reads, reconnects, and closes one physical camera
//! source, and publishes to the track. Two producers writing to the same
//! camera track must become inexpressible, not merely undetected.
//!
//! The target shape this file drives (chosen here because no such types
//! exist yet — the test author defines the surface, the implementer
//! conforms): `CameraTrackHub::new(..)` returns a `(CameraTrackHubProducer,
//! CameraTrackHub)` pair. `CameraTrackHubProducer::publish`/`begin_epoch`
//! take `&mut self`. `CameraTrackHub` (the consumer side) keeps exactly
//! its current subscribing surface (`subscribe`, `detach`, and its query
//! methods) and is `Clone`. Every test below currently fails to COMPILE
//! against the unmodified tree, because `CameraTrackHubProducer` does not
//! exist and `CameraTrackHub::new` returns a single `Self` — that
//! compile failure is this file's RED state.
//!
//! ## What this file can and cannot prove
//!
//! Everything below is a BEHAVIORAL proof: it drives real publishes and
//! subscriptions and asserts on delivered events, exactly like
//! `camera_hub_fanout.rs`. Three of the requirements named in the driving
//! brief are pure type-level absences that no BEHAVIORAL test — in this
//! file or any other — can exercise, because there is nothing to call
//! that would prove a negative:
//!
//! - **`CameraTrackHubProducer` is not `Clone`.**
//! - **No method on `CameraTrackHub` (the consumer) hands back a
//!   producer, and there is no second constructor that yields one.**
//! - **`&mut self` is the EXCLUSIVITY mechanism, not `!Clone` alone** — a
//!   non-`Clone` producer would still allow two holders to publish
//!   concurrently through a shared `Arc<CameraTrackHubProducer>` if
//!   `publish`/`begin_epoch` took `&self`.
//!
//! These three are now CLOSED, not left open: `compile_fail` doctests
//! beside `CameraTrackHubProducer` in `camera_hub.rs`, following the
//! `SettingHandle`/`AutomationHandle` precedent in `src/settings.rs`
//! exactly, assert all three — one attempting `producer.clone()`, one
//! attempting to construct a `CameraTrackHubProducer` directly (its only
//! field is private), one attempting `publish` through an
//! `Arc<CameraTrackHubProducer>` (which hands out `&T`, never `&mut T`).
//! Each was verified genuinely fix-dependent by temporarily relaxing the
//! real constraint it names (adding `#[derive(Clone)]`; making the field
//! and its type `pub`; switching `publish` back to `&self`) and
//! confirming the doctest then COMPILES, before reverting. A lexical
//! backstop, `camera_hub_capability_scan.rs`, re-checks the two
//! constraints a future edit could reopen without any doctest noticing
//! (`CameraTrackHubProducer` gaining `Clone`; a non-`new` method on
//! `CameraTrackHub` naming `CameraTrackHubProducer`) on every test run —
//! a doctest only proves TODAY's source fails to compile, it cannot
//! re-run itself against tomorrow's edit the way a checked-in scan can.
//!
//! Every assertion below is stated in STREAM terms — event contents,
//! sequence numbers, `Arc` identity — never elapsed time, never a sleep.

use std::num::NonZeroUsize;
use std::sync::Arc;

use bytes::Bytes;

use vigil::VideoCodec;
use vigil::camera_hub::{CameraTrackHub, EpochReason, LossContract, SubscribeOptions, TrackEvent};
use vigil::camera_track::{CameraId, EncodedAccessUnit, SourceRole};
use vigil::workgraph::StreamId;

/// Mirrors `camera_hub_fanout.rs`'s own fixture interval: small enough to
/// keep derived automatic capacity manageable while still exercising the
/// real derivation, never a bare literal duplicated from what the hub
/// itself computes.
const FIXTURE_KEYFRAME_INTERVAL_FRAMES: u32 = 30;
const FIXTURE_CAMERA_ID: &str = "driveway-durable-id";

/// Builds a durable `CameraId` fixture through the real per-source-kind
/// constructor (USB, arbitrarily chosen among the four — this file is not
/// exercising `CameraId`'s own construction rules, only using a stable
/// identity value), now that the unrestricted `CameraId::new` is gone.
fn camera_id(value: &str) -> CameraId {
    CameraId::from_usb("test-node", value).expect("fixture literal is a durable identity")
}

/// Builds a fresh hub split and returns `(producer, consumer)` — the
/// target shape of `CameraTrackHub::new`. Named distinctly from
/// `camera_hub_fanout.rs`'s own `hub_with_fixture_interval` (which builds
/// the pre-split, single-type hub) so both files can exist side by side
/// without colliding on production surface — each integration test file
/// compiles as its own binary, so there is no symbol clash either way,
/// but the distinct name keeps the two files honest about which API
/// generation each one drives.
fn split_hub_with_fixture_interval() -> (vigil::camera_hub::CameraTrackHubProducer, CameraTrackHub)
{
    CameraTrackHub::new(
        StreamId::new("driveway"),
        camera_id(FIXTURE_CAMERA_ID),
        SourceRole::Analysis,
        VideoCodec::H264,
        FIXTURE_KEYFRAME_INTERVAL_FRAMES,
    )
}

fn unit(epoch: u64, sequence: u64, keyframe: bool, discontinuity: bool) -> EncodedAccessUnit {
    EncodedAccessUnit {
        stream_id: StreamId::new("driveway"),
        stream_epoch: epoch,
        codec: VideoCodec::H264,
        codec_config: if keyframe {
            Some(Bytes::from(vec![0, 0, 0, 1, 0x67]))
        } else {
            None
        },
        keyframe,
        data: Bytes::from(vec![sequence as u8; 8]),
        timing: None,
        observed_at: None,
        camera: camera_id(FIXTURE_CAMERA_ID),
        source_role: SourceRole::Analysis,
        sequence,
        discontinuity,
        format_change: false,
        segment_sequence: 0,
    }
}

fn capacity(value: usize) -> Option<NonZeroUsize> {
    Some(NonZeroUsize::new(value).expect("test capacities are always non-zero"))
}

fn access_unit(events: &[TrackEvent]) -> Option<Arc<EncodedAccessUnit>> {
    events.iter().find_map(|event| match event {
        TrackEvent::AccessUnit(unit) => Some(unit.clone()),
        _ => None,
    })
}

/// A helper whose signature names ONLY the consumer type — never the
/// producer type at all. If this compiles (and it must, for every other
/// test in this file to compile), it is a genuine structural fact about
/// the split: subscribing needs nothing from the producer side. This is
/// a positive compile-time proof (the split exists and `subscribe` lives
/// where it should), not a negative one, so it needs no `compile_fail`
/// harness to state.
fn subscribe_via_consumer_only(
    consumer: &CameraTrackHub,
    options: SubscribeOptions,
) -> vigil::camera_hub::Subscription {
    consumer.subscribe(options)
}

/// Baseline regression proof: the split must not change what a single
/// subscriber observes on an ordinary publish. Exercises the SAME join
/// -> publish -> drain shape `camera_hub_fanout.rs`'s
/// `n_subscribers_receive_the_identical_payload_buffer_for_one_published_unit`
/// already covers for the pre-split type, but through the new two-handle
/// API — proving the split is a pure capability move, not a behavior
/// change.
#[test]
fn splitting_the_hub_yields_a_producer_that_publishes_and_a_consumer_that_subscribes() {
    let (mut producer, consumer) = split_hub_with_fixture_interval();

    let subscription = subscribe_via_consumer_only(
        &consumer,
        SubscribeOptions {
            loss: LossContract::RejoinAtRandomAccess,
            capacity: capacity(8),
        },
    );

    producer
        .publish(unit(1, 0, true, true))
        .expect("first unit of an epoch is always accepted");

    let events = subscription.drain();
    let delivered = access_unit(&events).expect("the subscription received the published unit");
    assert_eq!(delivered.sequence, 0);
    assert!(
        matches!(events.first(), Some(TrackEvent::CodecConfig(_))),
        "a join still lands on codec-config-then-keyframe through the split API"
    );
}

/// Pins "the consumer side stays cheaply cloneable and shareable": two
/// independently owned clones of the SAME consumer handle, each
/// subscribing on its own, must both observe a single publish made
/// through the one producer — proving a clone is a shared VIEW onto the
/// same hub state, never an independent fork that silently stops
/// receiving what the other clone (or the original) sees.
#[test]
fn cloning_the_consumer_lets_every_clone_observe_the_same_producers_publications() {
    let (mut producer, consumer) = split_hub_with_fixture_interval();
    let consumer_clone = consumer.clone();

    let original_subscription = consumer.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    let cloned_subscription = consumer_clone.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(8),
    });

    producer
        .publish(unit(1, 0, true, true))
        .expect("first unit of an epoch is always accepted");

    let original_unit = access_unit(&original_subscription.drain())
        .expect("the subscription made via the original consumer handle received the publish");
    let cloned_unit = access_unit(&cloned_subscription.drain())
        .expect("the subscription made via the CLONED consumer handle received the same publish");

    assert_eq!(original_unit.sequence, 0);
    assert_eq!(cloned_unit.sequence, 0);
    assert!(
        Arc::ptr_eq(&original_unit, &cloned_unit),
        "a subscription reached through a cloned consumer handle must share the identical \
         Arc<EncodedAccessUnit> with one reached through the original handle — two clones of a \
         cheaply shareable consumer are views onto ONE hub, never two independently forked ones"
    );
}

/// Pins that the producer/consumer split does not fork state across
/// clones for MUTATING producer operations either: `begin_epoch`, called
/// once through the producer, must be visible through EVERY consumer
/// clone — one that already held a subscription before the clone was
/// made, and one that only subscribes after cloning.
#[test]
fn begin_epoch_through_the_producer_is_visible_through_every_consumer_clone() {
    let (mut producer, consumer) = split_hub_with_fixture_interval();

    let pre_clone_subscription = consumer.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch-1 keyframe");
    let _ = pre_clone_subscription.drain();

    // Clone the consumer AFTER the first subscription already exists, and
    // AFTER the first epoch's keyframe was already delivered — proving
    // the clone is not a snapshot taken at clone time.
    let consumer_clone = consumer.clone();
    let post_clone_subscription = consumer_clone.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });

    let new_epoch = producer
        .begin_epoch(EpochReason::Reconnect, Bytes::from(vec![0, 0, 0, 1, 0x67]))
        .expect("epoch bump succeeds before any terminal");
    producer
        .publish(unit(new_epoch, 0, true, false))
        .expect("the new epoch's own opening keyframe");

    let pre_clone_delivered = access_unit(&pre_clone_subscription.drain())
        .expect("the pre-clone subscription observes the reconnect's opening keyframe");
    let post_clone_delivered = access_unit(&post_clone_subscription.drain())
        .expect("the subscription made via the CLONED consumer also observes it");

    assert_eq!(pre_clone_delivered.stream_epoch, new_epoch);
    assert_eq!(post_clone_delivered.stream_epoch, new_epoch);
    assert!(
        pre_clone_delivered.discontinuity,
        "the reconnect's own opening keyframe is marked discontinuous for the pre-clone subscriber"
    );
    assert!(
        post_clone_delivered.discontinuity,
        "the reconnect's own opening keyframe is marked discontinuous for the post-clone \
         subscriber too — the clone shares the SAME hub the producer bumped the epoch on"
    );
}

/// Pins that the producer is a genuinely separate, singly-owned handle:
/// it can be MOVED onto its own thread (a real source-supervisor would
/// own it there, reading/reconnecting/closing one physical camera and
/// publishing from that thread) while the consumer side keeps being used
/// concurrently from the caller thread — something a single, shared-`&self`
/// type could not express without an `Arc<Mutex<_>>` wrapper the caller
/// would have to build itself. This is a real behavioral/structural
/// proof (the code must compile AND the publish from the spawned thread
/// must actually reach a subscription made on the caller thread) rather
/// than a purely structural one.
#[test]
fn the_producer_can_be_moved_onto_its_own_thread_while_consumer_clones_stay_usable_here() {
    let (producer, consumer) = split_hub_with_fixture_interval();
    let subscription = consumer.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });

    let publisher_thread = std::thread::spawn(move || {
        let mut producer = producer;
        producer
            .publish(unit(1, 0, true, true))
            .expect("first unit of an epoch is always accepted");
    });
    publisher_thread
        .join()
        .expect("the thread that exclusively owned the producer must not panic");

    let delivered = access_unit(&subscription.drain())
        .expect("a publish made from the producer's OWN thread reaches a subscription made here");
    assert_eq!(delivered.sequence, 0);
}
