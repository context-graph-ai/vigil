//! The camera-track hub's fan-out contract: N subscribers share the same
//! payload buffers; each declares its own loss contract; queues are
//! bounded with no unbounded growth (including the one event class that
//! is never droppable — codec configuration — across repeated epoch
//! bumps); a stalled/failed subscriber, an explicit detach, and a dropped
//! `Subscription` handle are all detached without disturbing the producer
//! or any other subscriber; a join lands on codec-config-then-keyframe
//! and deterministically requests a keyframe from the producer; the
//! automatic capacity default really applies when a subscriber declares
//! none; reconnect/format-change open a new epoch under a stable stream
//! identity while resetting sequence numbering, discarding the prior
//! epoch's retained data, and marking the new epoch's own first unit;
//! and publish refuses a unit whose stream/epoch/camera/source-role/codec
//! identity is wrong, not only a duplicate sequence — checked against
//! identity the hub was BOUND to at construction, never adopted from
//! whatever unit happens to arrive first.
//!
//! Every assertion here is stated in STREAM terms (event contents, counts,
//! sequence numbers) — never elapsed time, never a sleep.

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use bytes::Bytes;

use vigil::VideoCodec;
use vigil::camera_hub::{
    CameraTrackHub, EpochReason, LossContract, PublishError, SubscribeOptions, SubscriptionEnd,
    SubscriptionEvent, TrackEvent, automatic_subscriber_capacity,
};
use vigil::camera_track::{CameraId, EncodedAccessUnit, SourceRole};
use vigil::workgraph::StreamId;

/// A fixture keyframe interval used across most tests, in output frames —
/// 30 output frames is small enough that the derived automatic capacity
/// (`automatic_subscriber_capacity`) stays a manageable size in test
/// assertions while still exercising the real derivation (never a bare
/// literal duplicated separately from what the hub itself would compute).
const FIXTURE_KEYFRAME_INTERVAL_FRAMES: u32 = 30;

const FIXTURE_CAMERA_ID: &str = "driveway-durable-id";

/// Builds a durable `CameraId` fixture through the real per-source-kind
/// constructor (USB, arbitrarily chosen among the four — this file is not
/// exercising `CameraId`'s own construction rules, only using a stable
/// identity value), now that the unrestricted `CameraId::new` is gone.
fn camera_id(value: &str) -> CameraId {
    CameraId::from_usb("test-node", value).expect("fixture literal is a durable identity")
}

fn hub_with_fixture_interval() -> (vigil::camera_hub::CameraTrackHubProducer, CameraTrackHub) {
    CameraTrackHub::new(
        StreamId::new("driveway"),
        camera_id(FIXTURE_CAMERA_ID),
        SourceRole::Analysis,
        VideoCodec::H264,
        FIXTURE_KEYFRAME_INTERVAL_FRAMES,
    )
}

fn unit(epoch: u64, sequence: u64, keyframe: bool, discontinuity: bool) -> EncodedAccessUnit {
    unit_for(
        StreamId::new("driveway"),
        camera_id(FIXTURE_CAMERA_ID),
        SourceRole::Analysis,
        VideoCodec::H264,
        epoch,
        sequence,
        keyframe,
        discontinuity,
    )
}

#[allow(clippy::too_many_arguments)]
fn unit_for(
    stream_id: StreamId,
    camera: CameraId,
    source_role: SourceRole,
    codec: VideoCodec,
    epoch: u64,
    sequence: u64,
    keyframe: bool,
    discontinuity: bool,
) -> EncodedAccessUnit {
    EncodedAccessUnit {
        stream_id,
        stream_epoch: epoch,
        codec,
        codec_config: if keyframe {
            Some(Bytes::from(vec![0, 0, 0, 1, 0x67]))
        } else {
            None
        },
        keyframe,
        data: Bytes::from(vec![sequence as u8; 8]),
        timing: None,
        observed_at: None,
        camera,
        source_role,
        sequence,
        discontinuity,
        format_change: false,
        segment_sequence: 0,
    }
}

fn capacity(value: usize) -> Option<NonZeroUsize> {
    Some(NonZeroUsize::new(value).expect("test capacities are always non-zero"))
}

fn access_unit_sequences(events: &[TrackEvent]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit.sequence),
            _ => None,
        })
        .collect()
}

/// A `Waker` that records whether it was ever actually invoked, so a test
/// can assert deterministic wakeup — never a wall-clock timeout — around a
/// `Subscription::recv` future: poll it, assert `Poll::Pending` and that
/// this flag is still false, publish/raise the terminal, assert the flag
/// flipped true, THEN poll again and assert `Poll::Ready`. `Waker::from`
/// (stable `std::task::Wake`) is used rather than a raw `RawWakerVTable`
/// so the only thing under test is the hub's own wakeup discipline, never
/// a hand-rolled vtable bug.
struct RecordingWake(AtomicBool);

impl Wake for RecordingWake {
    fn wake(self: StdArc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &StdArc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A fresh recording waker plus the flag it records into, paired so a test
/// can assert on the flag directly without downcasting the `Waker`.
fn recording_waker() -> (Waker, StdArc<RecordingWake>) {
    let flag = StdArc::new(RecordingWake(AtomicBool::new(false)));
    (Waker::from(StdArc::clone(&flag)), flag)
}

/// Poll one pinned future exactly once against the given waker — the
/// single deterministic primitive every async `recv`-driving test below is
/// built from. Never sleeps, never reads a clock: the only way this ever
/// observes `Poll::Ready` is a real value (or the terminal `None`) already
/// being available at the moment of the call.
fn poll_once<F: Future>(fut: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
    let mut cx = Context::from_waker(waker);
    fut.poll(&mut cx)
}

#[test]
fn n_subscribers_receive_the_identical_payload_buffer_for_one_published_unit() {
    let (mut producer, hub) = hub_with_fixture_interval();
    let a = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    let b = hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(8),
    });

    producer
        .publish(unit(1, 0, true, true))
        .expect("first unit of an epoch is always accepted");

    let a_events = a.drain();
    let b_events = b.drain();
    let a_unit = a_events
        .iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit.clone()),
            _ => None,
        })
        .expect("subscriber a received the published access unit");
    let b_unit = b_events
        .iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit.clone()),
            _ => None,
        })
        .expect("subscriber b received the published access unit");

    assert!(
        std::sync::Arc::ptr_eq(&a_unit, &b_unit),
        "two subscribers receiving the same publish must share the identical Arc<EncodedAccessUnit>"
    );
    assert_eq!(
        a_unit.data.as_ptr(),
        b_unit.data.as_ptr(),
        "the shared unit's payload buffer is the same allocation for every subscriber"
    );
}

#[test]
fn payload_storage_stays_shared_for_one_publish_even_when_one_subscriber_still_owes_an_epoch_marker()
 {
    // The test above (`n_subscribers_receive_the_identical_payload_buffer_...`)
    // never bumps the epoch at all, so both subscribers take the cheap
    // `Arc::clone` path and the same-Arc/shared-buffer guarantee is only
    // ever exercised where NO per-subscription cloning happens. A
    // subscription that still owes its own epoch marker gets a freshly
    // cloned `EncodedAccessUnit` wrapped in a NEW `Arc` (so its
    // `discontinuity`/`format_change` can differ from a subscriber that
    // already resolved its own marker on an earlier keyframe of the same
    // epoch) — that clone path is entirely unasserted by the existing
    // test. Cloning the STRUCT still shares the underlying `Bytes`
    // payload storage (a `Bytes` clone is a refcount bump, never a copy),
    // but nothing pins that: a later change to the marking logic that
    // deep-copies the payload would silently break the immutable-shared-
    // buffer contract with no test noticing.
    //
    // This drives one subscriber to have already resolved its own marker
    // (so it takes the cheap shared-Arc path) and a second subscriber to
    // still owe its own copy of the SAME epoch's marker (so it takes the
    // cloning path), then delivers ONE publish to both and asserts their
    // payload buffers still share the identical backing allocation, even
    // though the two delivered wrapper units are provably NOT the same
    // Arc (their discontinuity/format_change values provably differ).
    let (mut producer, hub) = hub_with_fixture_interval();

    let resolved = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch-1 keyframe");
    let _ = resolved.drain();

    let epoch = producer
        .begin_epoch(EpochReason::Reconnect, Bytes::from(vec![0, 0, 0, 1, 0x67]))
        .expect("epoch bump succeeds before any terminal");

    let mut opening_keyframe = unit(epoch, 0, true, false);
    opening_keyframe.format_change = false;
    producer
        .publish(opening_keyframe)
        .expect("the new epoch's own opening keyframe");
    let resolved_first = resolved
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the early subscriber resolves its own marker on the epoch's opening keyframe");
    assert!(
        resolved_first.discontinuity,
        "sanity: the early subscriber's own first post-epoch keyframe is marked discontinuous"
    );

    // Joins only now, strictly after `resolved` already consumed its own
    // copy of the epoch marker — `owing` still has its own copy pending.
    let owing = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });

    // ONE publish, delivered to both subscribers: `resolved` takes the
    // cheap shared-Arc path (it has nothing left to mark), `owing` takes
    // the per-subscription cloning path (it still owes its own marker).
    let mut second_keyframe = unit(epoch, 1, true, false);
    second_keyframe.format_change = false;
    producer
        .publish(second_keyframe)
        .expect("a later keyframe in the same epoch, delivered to both subscribers");

    let resolved_second = resolved
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the already-resolved subscriber still receives the second keyframe");
    let owing_first = owing
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the marker-owing subscriber receives its own first delivered keyframe");

    // Prove the two delivered units genuinely took DIFFERENT paths on this
    // SAME publish — otherwise the payload-sharing assertion below would
    // be exercising nothing new. A do-nothing/drop-everything hub, or one
    // that never actually resolves markers per-subscription, could not
    // produce this split at all (both `.expect()` calls above would
    // already have panicked).
    assert_eq!(resolved_second.sequence, 1);
    assert_eq!(owing_first.sequence, 1);
    assert!(
        !resolved_second.discontinuity,
        "the already-resolved subscriber must not be re-marked on a later keyframe of the same \
         epoch"
    );
    assert!(
        owing_first.discontinuity,
        "the marker-owing subscriber must be marked on its own first delivered keyframe"
    );
    assert!(
        !std::sync::Arc::ptr_eq(&resolved_second, &owing_first),
        "the two subscribers must NOT share the identical wrapper Arc here — one was cloned to \
         carry its own marker values, so asserting Arc::ptr_eq (as the no-epoch test above does) \
         would be the wrong check; the payload buffer identity below is the meaningful one"
    );

    // The meaningful check for `Bytes`: even through a struct clone and a
    // fresh wrapper `Arc`, the payload storage itself must still be the
    // SAME allocation, never deep-copied. Guarded against vacuous
    // equality on two empty buffers: the fixture payload is 8 bytes
    // (`unit()`'s `data: Bytes::from(vec![sequence as u8; 8])`), asserted
    // explicitly non-empty here so an implementation that accidentally
    // delivered empty payloads to both sides could not pass this by
    // comparing two empty slices.
    assert!(
        !resolved_second.data.is_empty(),
        "sanity: the fixture payload must be non-empty, or a shared-empty-buffer would pass this \
         check vacuously"
    );
    assert_eq!(
        resolved_second.data.len(),
        owing_first.data.len(),
        "both delivered copies must carry the identical payload length"
    );
    assert_eq!(
        resolved_second.data.as_ptr(),
        owing_first.data.as_ptr(),
        "the payload buffer must be the SAME backing allocation for both subscribers even though \
         one of them received a per-subscription clone of the wrapping EncodedAccessUnit to carry \
         its own epoch marker — a Bytes clone is a refcount bump, never a deep copy"
    );
}

#[test]
fn join_delivers_codec_config_then_the_first_post_join_keyframe_never_an_intervening_delta() {
    let (mut producer, hub) = hub_with_fixture_interval();

    // Units flowing before anyone has joined.
    producer
        .publish(unit(1, 0, true, true))
        .expect("keyframe unit");
    producer
        .publish(unit(1, 1, false, false))
        .expect("delta unit");

    let joiner = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });

    // A delta arriving right after join must NOT be delivered before the
    // next keyframe. The post-join keyframe (sequence 3) is published
    // with its OWN codec_config explicitly overridden to None — the
    // `unit()` helper would otherwise attach a (default) codec_config to
    // every keyframe, which would let a hub that retains NOTHING, and
    // instead just echoes whatever the delivered keyframe happens to
    // carry, pass the retained-config assertion below unchanged. With
    // codec_config forced to None here, only a hub that genuinely
    // retained the epoch's OWN opening bytes (from sequence 0) has
    // anything to deliver at all.
    producer
        .publish(unit(1, 2, false, false))
        .expect("delta unit after join");
    let post_join_keyframe_with_no_config = EncodedAccessUnit {
        codec_config: None,
        ..unit(1, 3, true, false)
    };
    producer
        .publish(post_join_keyframe_with_no_config)
        .expect("keyframe unit after join");
    producer
        .publish(unit(1, 4, false, false))
        .expect("delta unit after the post-join keyframe");

    let events = joiner.drain();
    assert!(
        matches!(events.first(), Some(TrackEvent::CodecConfig(_))),
        "the first event a joiner receives is codec configuration, got {events:?}"
    );

    // The retained codec configuration the hub hands a joiner must be the
    // bytes the HUB retained from the epoch's own opening keyframe (unit
    // 0) — proven by making sequence 3's post-join keyframe carry NO
    // codec_config of its own, so a hub that never actually retained
    // anything (and instead just echoed whatever the delivered keyframe
    // happened to carry) cannot pass this.
    if let Some(TrackEvent::CodecConfig(config)) = events.first() {
        assert_eq!(
            config.as_ref(),
            &[0, 0, 0, 1, 0x67][..],
            "the delivered codec configuration must be the exact bytes the hub retained from \
             the epoch's opening keyframe, proven here because the post-join keyframe (sequence \
             3) itself carries no codec_config of its own"
        );
    }

    let first_access_unit = events
        .iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit.sequence),
            _ => None,
        })
        .expect("a joiner receives at least one access unit");
    assert_eq!(
        first_access_unit, 3,
        "the first delivered access unit must be the first post-join KEYFRAME (sequence 3), \
         never an intervening delta (sequence 2)"
    );
}

#[test]
fn a_join_deterministically_requests_a_keyframe_from_the_producer() {
    // Re-expressed against the corrected capability placement: requesting a
    // keyframe stays subscriber-side (a join sets the flag), but
    // check-and-clear moves to the PRODUCER — the single owner that
    // actually acts on the request — instead of sitting on the cloneable
    // consumer handle, where any clone (including one that never joined on
    // anyone's behalf) could have stolen another consumer's, or the
    // producer's own, pending request. The property under test is
    // unchanged; only which handle performs the consuming check moved from
    // `hub.keyframe_requested_since_last_check()` to
    // `producer.keyframe_requested_since_last_check()`.
    let (mut producer, hub) = hub_with_fixture_interval();

    // Steady delta-only flow with no keyframe since the epoch's start.
    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch keyframe");
    for sequence in 1..5 {
        producer
            .publish(unit(1, sequence, false, false))
            .expect("delta unit");
    }

    assert!(
        !producer.keyframe_requested_since_last_check(),
        "before any join, nothing has requested a producer keyframe"
    );

    let _joiner = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });

    assert!(
        producer.keyframe_requested_since_last_check(),
        "a join on a Vigil-owned encoder must deterministically request an immediate keyframe \
         from the producer, proven as an observable producer-side signal a producer polls, \
         never a wall-clock wait for the next scheduled keyframe"
    );
    assert!(
        !producer.keyframe_requested_since_last_check(),
        "checking the signal must CONSUME it (dirty-flag semantics) — a second check \
         immediately after, with no new join in between, must report false, or a producer \
         polling in a loop would force a keyframe on every poll forever"
    );

    // Consuming the signal once must not permanently exhaust it: a SECOND
    // join, after the first has already been consumed, must re-arm the
    // signal — proving this is a real per-join dirty flag rather than a
    // one-shot-ever latch that only ever fires for the hub's very first
    // subscriber.
    let _second_joiner = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    assert!(
        producer.keyframe_requested_since_last_check(),
        "a second, later join must ALSO deterministically request a keyframe — the signal must \
         re-arm on every join, not fire once ever"
    );
}

#[test]
fn a_subscription_at_capacity_one_still_receives_codec_config_and_its_keyframe_on_join() {
    // Ruled explicitly: capacity counts buffered ACCESS UNITS ONLY.
    // CodecConfig and Fault events sit OUTSIDE the budget, so a capacity of
    // one ("latest keyframe only") is legitimate — the codec-configuration
    // delivery must never compete with the keyframe for the single slot.
    //
    // The keyframe must be delivered strictly AFTER join — subscribing
    // first, then publishing, is what actually proves "first POST-join
    // keyframe"; delivering a pre-join keyframe would only prove ordinary
    // retained-config delivery, not this capacity-one guarantee.
    let (mut producer, hub) = hub_with_fixture_interval();

    let joiner = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(1),
    });
    producer
        .publish(unit(1, 0, true, true))
        .expect("the epoch's opening keyframe, published AFTER join");

    let events = joiner.drain();
    assert!(
        matches!(events.first(), Some(TrackEvent::CodecConfig(_))),
        "a capacity-one subscriber must still receive codec configuration on join, got {events:?}"
    );
    let keyframe_sequence = events.iter().find_map(|event| match event {
        TrackEvent::AccessUnit(unit) if unit.keyframe => Some(unit.sequence),
        _ => None,
    });
    assert_eq!(
        keyframe_sequence,
        Some(0),
        "a capacity-one subscriber must still receive its post-join keyframe: codec \
         configuration must not have consumed the single access-unit slot, got {events:?}"
    );
}

#[test]
fn a_subscription_with_no_explicit_capacity_receives_the_hubs_automatic_default() {
    // No test previously subscribed with `capacity: None` at all, so an
    // implementation could ignore the automatic-default contract entirely
    // and still pass every other test. A `<=` bound alone is not enough
    // to catch that: a do-nothing/drop-everything implementation reports
    // buffered_len() == 0 forever, which trivially satisfies `<=
    // automatic`. This asserts the EXACT fill at capacity (ruling out
    // "always empty"), then the EXACT overflow boundary via a real
    // FaultOnGap subscriber — which a drop-everything hub could never
    // produce at all, since it would never even deliver the keyframe that
    // makes the fault's lost-range arithmetic meaningful.
    let (mut producer, hub) = hub_with_fixture_interval();
    let automatic = hub.automatic_capacity().get();
    assert_eq!(
        automatic,
        automatic_subscriber_capacity(FIXTURE_KEYFRAME_INTERVAL_FRAMES).get(),
        "sanity: the hub's own automatic_capacity() must match the pure derivation"
    );

    let subscription = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: None,
    });

    producer.publish(unit(1, 0, true, true)).expect("keyframe");
    for sequence in 1..automatic as u64 {
        producer
            .publish(unit(1, sequence, false, false))
            .expect("delta unit");
    }
    assert_eq!(
        subscription.buffered_len(),
        automatic,
        "a subscriber with NO explicit capacity must be genuinely FULL at exactly the hub's \
         automatic default ({automatic}) once that many units have been published — a \
         do-nothing implementation would report 0 here, satisfying only a `<=` check"
    );

    // The exact overflow boundary, via a SEPARATE hub/subscriber pair (kept
    // independent of the fill proof above): a FaultOnGap subscriber with
    // no explicit capacity must fault at EXACTLY the automatic bound — a
    // do-nothing hub could never reach this at all, since it would never
    // even deliver the keyframe that makes the fault's lost-range
    // arithmetic meaningful.
    let (mut fault_producer, fault_hub) = hub_with_fixture_interval();
    let strict = fault_hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: None,
    });
    fault_producer
        .publish(unit(1, 0, true, true))
        .expect("keyframe");
    for sequence in 1..automatic as u64 {
        fault_producer
            .publish(unit(1, sequence, false, false))
            .expect("delta unit filling the strict subscriber's automatic capacity exactly");
    }
    fault_producer
        .publish(unit(1, automatic as u64, false, false))
        .expect("the one delta that overflows the automatic bound by exactly one");

    let fault = strict.drain().into_iter().find_map(|event| match event {
        TrackEvent::Fault(fault) => Some(fault),
        _ => None,
    });
    let fault = fault.expect(
        "a FaultOnGap subscriber with no explicit capacity must still fault at its automatic \
         bound — a do-nothing hub would never even reach this point",
    );
    assert_eq!(
        fault.lost_sequence_range,
        (automatic as u64)..=(automatic as u64),
        "the automatic-capacity overflow must be lost at EXACTLY sequence {automatic} — the one \
         unit published past the automatic fill, got {:?}",
        fault.lost_sequence_range
    );
}

#[test]
fn automatic_capacity_is_derived_from_the_effective_keyframe_interval_and_an_explicit_override_is_unaffected()
 {
    // Pins the coupling: changing the effective keyframe interval changes
    // the AUTOMATIC derived capacity with it (never a parallel constant
    // that could silently drift out of step), while an explicit capacity
    // override stays exactly what the caller asked for regardless.
    let (_, short_interval_hub) = CameraTrackHub::new(
        StreamId::new("short"),
        camera_id("short-durable-id"),
        SourceRole::Analysis,
        VideoCodec::H264,
        20,
    );
    let (_, long_interval_hub) = CameraTrackHub::new(
        StreamId::new("long"),
        camera_id("long-durable-id"),
        SourceRole::Analysis,
        VideoCodec::H264,
        90,
    );

    assert_eq!(short_interval_hub.automatic_capacity().get(), 20);
    assert_eq!(long_interval_hub.automatic_capacity().get(), 90);
    assert_ne!(
        short_interval_hub.automatic_capacity(),
        long_interval_hub.automatic_capacity(),
        "the automatic capacity must move with the effective keyframe interval, not be pinned"
    );

    // The documented floor: an operator-overridden keyframe interval below
    // 15 frames must not drag the derived automatic capacity below 15
    // either.
    let (_, below_floor_hub) = CameraTrackHub::new(
        StreamId::new("below-floor"),
        camera_id("below-floor-durable-id"),
        SourceRole::Analysis,
        VideoCodec::H264,
        4,
    );
    assert_eq!(
        below_floor_hub.automatic_capacity().get(),
        15,
        "the derived automatic capacity must never fall below the 15-frame floor, even when the \
         keyframe interval itself is overridden below it"
    );

    // Direct pure-function check, matching the hub's own derivation.
    assert_eq!(automatic_subscriber_capacity(20).get(), 20);
    assert_eq!(automatic_subscriber_capacity(4).get(), 15);

    // An explicit capacity override is a fixed request, never re-derived
    // when the interval changes. Subscribing BEFORE the epoch's keyframe
    // (never after, with only deltas following) matters: a subscriber
    // that joins with no keyframe published afterward is still waiting
    // for its first post-join keyframe forever, so buffered_len() stays
    // 0 and a `<=` check on it proves nothing about capacity at all —
    // exactly the gap this rewrite closes.
    let (mut producer, hub) = CameraTrackHub::new(
        StreamId::new("driveway"),
        camera_id(FIXTURE_CAMERA_ID),
        SourceRole::Analysis,
        VideoCodec::H264,
        20,
    );
    let explicit = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(3),
    });
    producer.publish(unit(1, 0, true, true))
        .expect("epoch keyframe, published AFTER join so it satisfies explicit's own first-post-join-keyframe requirement");
    for sequence in 1..3u64 {
        producer
            .publish(unit(1, sequence, false, false))
            .expect("delta unit filling the explicit capacity exactly");
    }
    assert_eq!(
        explicit.buffered_len(),
        3,
        "an explicit capacity override must be genuinely FULL at exactly its declared bound (3) \
         — never merely <= 3, which a do-nothing implementation satisfies trivially at 0 — \
         regardless of the hub's own automatic keyframe interval (20)"
    );
}

#[test]
fn rejoin_at_random_access_discards_stale_data_and_resumes_at_the_exact_next_keyframe_on_overflow()
{
    let (mut producer, hub) = hub_with_fixture_interval();
    let subscription = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(2),
    });

    // Drain the initial join's codec config + keyframe first: otherwise an
    // undrained pending_codec_config from the ORIGINAL join would still
    // sit in the subscriber's buffer at the final drain below, masking
    // whether the OVERFLOW rejoin itself re-armed codec config delivery —
    // the very property this test exists to prove.
    producer.publish(unit(1, 0, true, true)).expect("keyframe");
    let _ = subscription.drain();

    // Overflow the small bounded queue with deltas the subscriber never
    // drains in between.
    for sequence in 1..6 {
        producer
            .publish(unit(1, sequence, false, false))
            .expect("delta unit");
    }
    producer
        .publish(unit(1, 6, true, false))
        .expect("the keyframe that ends the overflow run");

    let events = subscription.drain();

    // A rejoin is a random-access resync exactly like a fresh join: codec
    // configuration must precede the keyframe it rejoins on, never just
    // the bare access unit alone — a rejoined subscriber that lands on a
    // keyframe with no codec configuration in hand cannot actually decode
    // it.
    let codec_config_index = events
        .iter()
        .position(|event| matches!(event, TrackEvent::CodecConfig(_)));
    let keyframe_index = events.iter().position(
        |event| matches!(event, TrackEvent::AccessUnit(unit) if unit.sequence == 6 && unit.keyframe),
    );
    assert!(
        codec_config_index.is_some(),
        "a RejoinAtRandomAccess subscriber must receive codec configuration on rejoin, got \
         {events:?}"
    );
    assert!(
        keyframe_index.is_some(),
        "a RejoinAtRandomAccess subscriber must receive the rejoin keyframe (sequence 6), got \
         {events:?}"
    );
    assert!(
        codec_config_index.unwrap() < keyframe_index.unwrap(),
        "codec configuration must precede the rejoin keyframe, not follow or be missing, got \
         {events:?}"
    );

    let delivered_sequences = access_unit_sequences(&events);
    assert_eq!(
        delivered_sequences,
        vec![6],
        "an overflowed RejoinAtRandomAccess subscriber must deliver EXACTLY the next keyframe \
         (sequence 6) and nothing else — no stale delta, and no residual pre-overflow unit \
         either, got {delivered_sequences:?}"
    );
    assert!(
        !subscription.is_detached(),
        "a RejoinAtRandomAccess subscriber survives overflow by rejoining, never by detaching"
    );

    // Proves the rejoin is not a one-shot fluke: normal delivery must
    // resume cleanly for the next unit published after the rejoin.
    producer
        .publish(unit(1, 7, false, false))
        .expect("normal delta after the rejoin");
    let resumed_sequences = access_unit_sequences(&subscription.drain());
    assert_eq!(
        resumed_sequences,
        vec![7],
        "normal delivery must resume for units published after an overflow rejoin, got \
         {resumed_sequences:?}"
    );
}

#[test]
fn fault_on_gap_surfaces_the_exact_lost_range_stays_bounded_and_is_terminal() {
    let (mut producer, hub) = hub_with_fixture_interval();
    let strict = hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(2),
    });
    let tolerant = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(64),
    });

    // Capacity 2: the keyframe (sequence 0) and one delta (sequence 1)
    // fit; sequence 2 is the FIRST unit that overflows the queue. Drain
    // immediately after that first overflow — a detached subscriber must
    // not need its lost range "extended" by further publishes it never
    // even has a chance to receive.
    producer.publish(unit(1, 0, true, true)).expect("keyframe");
    producer
        .publish(unit(1, 1, false, false))
        .expect("delta that still fits in capacity 2");
    producer
        .publish(unit(1, 2, false, false))
        .expect("delta that overflows the strict subscriber's capacity-2 queue");

    let strict_events = strict.drain();
    let fault = strict_events.iter().find_map(|event| match event {
        TrackEvent::Fault(fault) => Some(fault.clone()),
        _ => None,
    });
    let fault = fault.expect("a FaultOnGap subscriber that cannot keep pace surfaces a Fault");
    assert_eq!(fault.stream_id, *hub.stream_id());
    assert_eq!(
        fault.camera,
        camera_id(FIXTURE_CAMERA_ID),
        "the fault must name the affected CAMERA — the governing contract is an explicit \
         coverage reduction with the affected camera and interval, and a real consumer cannot \
         tell which camera lost coverage without this field, got {:?}",
        fault.camera
    );
    assert_eq!(fault.epoch, 1);
    assert_eq!(
        fault.lost_sequence_range,
        2..=2,
        "the fault must name the EXACT lost sequence range — sequence 2 is the FIRST unit lost \
         (capacity 2 already holds 0 and 1), not some later extended range, got {:?}",
        fault.lost_sequence_range
    );
    assert!(
        strict.is_detached(),
        "the FaultOnGap subscriber is detached after surfacing its fault"
    );

    // Publish more after the fault: the OTHER subscriber must be entirely
    // unaffected, and the producer must not have restarted.
    producer
        .publish(unit(1, 3, false, false))
        .expect("publish continues after one subscriber's fault");
    let tolerant_sequences = access_unit_sequences(&tolerant.drain());
    assert!(
        tolerant_sequences.contains(&3),
        "the surviving subscriber keeps receiving units published after the other's fault, got {tolerant_sequences:?}"
    );
    assert_eq!(
        hub.active_subscription_count(),
        1,
        "exactly one subscription (the faulted one) was removed"
    );

    // Terminal: the faulted subscription must never receive anything ever
    // again, even after further publishes.
    producer
        .publish(unit(1, 4, false, false))
        .expect("publish continues after the fault");
    let post_fault_events = strict.drain();
    assert!(
        post_fault_events.is_empty(),
        "a detached FaultOnGap subscription must receive NOTHING after its terminal fault, got \
         {post_fault_events:?}"
    );
}

#[test]
fn per_consumer_loss_contract_is_honored_independently_at_the_same_capacity() {
    // The whole reason this hub exists (never tokio::broadcast, whose lag
    // behavior is implicit and uniform across every receiver) is that each
    // subscriber declares its OWN loss contract. The neighboring
    // `fault_on_gap_surfaces_the_exact_lost_range...` test never actually
    // proves that per-consumer independence: its tolerant subscriber has
    // capacity 64 and never overflows, so the two contracts are never
    // exercised AT THE SAME TIME under the SAME overflow — a hub that
    // stored only the FIRST subscriber's loss contract and applied it to
    // every subscriber would still pass every other test in this file.
    // Here both subscribers share the IDENTICAL small capacity and are
    // overflowed by the SAME sequence of publishes, so only a hub that
    // tracks each subscription's own declared contract independently can
    // pass this one.
    let (mut producer, hub) = hub_with_fixture_interval();
    let strict = hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(2),
    });
    let tolerant = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(2),
    });

    producer.publish(unit(1, 0, true, true)).expect("keyframe");
    producer
        .publish(unit(1, 1, false, false))
        .expect("delta that still fits in capacity 2 for both subscribers");
    for sequence in 2..6 {
        producer
            .publish(unit(1, sequence, false, false))
            .expect("delta unit overflowing both subscribers identically");
    }
    producer
        .publish(unit(1, 6, true, false))
        .expect("the keyframe that ends the overflow run for the tolerant subscriber");

    let strict_events = strict.drain();
    let strict_fault = strict_events.iter().find_map(|event| match event {
        TrackEvent::Fault(fault) => Some(fault.clone()),
        _ => None,
    });
    assert!(
        strict_fault.is_some(),
        "the FaultOnGap subscriber must surface a terminal Fault on overflow, got \
         {strict_events:?}"
    );
    assert!(
        strict.is_detached(),
        "the FaultOnGap subscriber is detached after its fault"
    );

    let tolerant_events = tolerant.drain();
    let tolerant_fault = tolerant_events.iter().find_map(|event| match event {
        TrackEvent::Fault(fault) => Some(fault.clone()),
        _ => None,
    });
    assert!(
        tolerant_fault.is_none(),
        "the RejoinAtRandomAccess subscriber, overflowed by the IDENTICAL publishes at the \
         IDENTICAL capacity as the FaultOnGap subscriber above, must never surface a Fault — \
         it discards and rejoins instead of faulting, got {tolerant_events:?}"
    );
    assert!(
        !tolerant.is_detached(),
        "a RejoinAtRandomAccess subscriber survives overflow by rejoining, never by detaching — \
         even though the OTHER subscriber (same capacity, same publishes) just detached on a \
         Fault, proving the two contracts are tracked independently, not hub-globally"
    );
    let tolerant_sequences = access_unit_sequences(&tolerant_events);
    assert_eq!(
        tolerant_sequences,
        vec![6],
        "the RejoinAtRandomAccess subscriber must discard the stale run and rejoin at exactly \
         the next keyframe (sequence 6), got {tolerant_sequences:?}"
    );
}

#[test]
fn a_subscription_queue_never_grows_past_its_declared_capacity() {
    // A `<=` check alone lets a do-nothing/drop-everything implementation
    // pass trivially (buffered_len() == 0 forever satisfies `<= 4`), so
    // this asserts the EXACT fill once capacity is reached, ruling out
    // "always empty". It deliberately stops there and does NOT also
    // assert the length stays pinned at exactly 4 for every publish
    // afterward: on overflow, a legitimate RejoinAtRandomAccess
    // implementation clears its queue and waits for the next keyframe
    // (see `rejoin_at_random_access_discards_stale_data_and_resumes_...`,
    // which already covers that exact post-overflow behavior) — a
    // stronger "must stay at exactly 4 forever" assertion here would
    // reject that valid implementation. A test that forbids a valid
    // implementation is its own defect, symmetric with one that admits
    // an invalid one.
    let (mut producer, hub) = hub_with_fixture_interval();
    let subscription = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(4),
    });

    producer.publish(unit(1, 0, true, true)).expect("keyframe");
    for sequence in 1..4 {
        producer
            .publish(unit(1, sequence, false, false))
            .expect("delta unit");
    }
    assert_eq!(
        subscription.buffered_len(),
        4,
        "the queue must be genuinely FULL at exactly its declared capacity 4 — a do-nothing \
         implementation would report 0 here, satisfying only a `<=` check"
    );

    // The exact-fill assertion above never publishes PAST capacity, so on
    // its own it cannot catch a hub that simply never enforces the bound
    // (unbounded growth). Publish one more delta to force an overflow and
    // assert the queue never holds MORE than the declared capacity
    // afterward — `<=`, not `==`, because a legitimate
    // RejoinAtRandomAccess implementation clears its queue and waits for
    // the next keyframe on overflow (proven end-to-end by
    // `rejoin_at_random_access_discards_stale_data_and_resumes_...`), so a
    // post-overflow length of 0 is valid; a post-overflow length greater
    // than 4 is not, under any implementation.
    producer
        .publish(unit(1, 4, false, false))
        .expect("delta unit that overflows the declared capacity");
    assert!(
        subscription.buffered_len() <= 4,
        "the queue must never hold MORE than its declared capacity 4, even immediately after an \
         overflow, got {}",
        subscription.buffered_len()
    );
}

#[test]
fn explicit_detach_stops_the_victim_from_receiving_anything_after_and_does_not_disturb_others() {
    let (mut producer, hub) = hub_with_fixture_interval();
    let victim = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    let survivor = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });

    producer
        .publish(unit(1, 0, true, true))
        .expect("keyframe before detach");
    hub.detach(victim.id());
    assert_eq!(hub.active_subscription_count(), 1);
    assert!(
        victim.is_detached(),
        "the victim's own handle must report itself detached immediately after CameraTrackHub::detach"
    );

    producer
        .publish(unit(1, 1, false, false))
        .expect("publish after an explicit detach must still succeed");

    let victim_events_after_detach = victim.drain();
    assert!(
        victim_events_after_detach.is_empty(),
        "an explicitly detached subscriber must receive NOTHING published after its detach, got \
         {victim_events_after_detach:?}"
    );

    let survivor_sequences = access_unit_sequences(&survivor.drain());
    assert!(
        survivor_sequences.contains(&0) && survivor_sequences.contains(&1),
        "the surviving subscriber must see units published both before and after the other's \
         detach, got {survivor_sequences:?}"
    );
}

#[test]
fn dropping_a_subscription_detaches_it_so_a_consumer_that_stops_holding_one_does_not_leak() {
    let (mut producer, hub) = hub_with_fixture_interval();
    producer.publish(unit(1, 0, true, true)).expect("keyframe");

    let subscription = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    assert_eq!(hub.active_subscription_count(), 1);

    drop(subscription);

    assert_eq!(
        hub.active_subscription_count(),
        0,
        "dropping a Subscription must detach it from the hub — an owner that stops holding a \
         subscription must not leak a permanent entry in the hub's subscriber table"
    );
}

#[test]
fn begin_epoch_reconnect_marks_the_new_epochs_first_delivered_unit_discontinuous() {
    let (mut producer, hub) = hub_with_fixture_interval();
    let subscriber = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch-1 keyframe");
    let _ = subscriber.drain();

    let epoch = producer
        .begin_epoch(EpochReason::Reconnect, Bytes::from(vec![0, 0, 0, 1, 0x67]))
        .expect("epoch bump succeeds before any terminal");

    // The prior `producer.publish(unit(1, 0, ...))` above only succeeded because
    // the hub's pre-transition epoch was 1 (publish enforces WrongEpoch
    // otherwise); this proves begin_epoch's first-ever call minted a
    // GENUINELY new epoch rather than returning the existing one unchanged
    // — a bug two-consecutive-calls-differ tests elsewhere cannot catch,
    // since only the SECOND call would then be forced to move.
    assert_ne!(
        epoch, 1,
        "the first begin_epoch call must mint a new epoch distinct from the pre-transition \
         epoch (1), not return the existing epoch unchanged"
    );

    // Deliberately publish the new epoch's first unit with discontinuity
    // and format_change AUTHORED false — proving the HUB enforces the
    // reconnect marker rather than merely trusting whatever the producer
    // happened to set.
    let mut first = unit(epoch, 0, true, false);
    first.format_change = false;
    producer
        .publish(first)
        .expect("the new epoch's own first unit is accepted starting from sequence 0");

    let delivered = subscriber
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the re-armed subscriber receives the new epoch's first access unit");

    assert!(
        delivered.discontinuity,
        "the new epoch's first delivered unit must be marked discontinuous after a Reconnect, \
         regardless of what the producer authored on the unit"
    );
    assert!(
        !delivered.format_change,
        "a plain reconnect must not also mark format_change"
    );
}

#[test]
fn begin_epoch_discontinuity_marks_the_new_epochs_first_delivered_unit_discontinuous() {
    // The third EpochReason variant — a producer-detected timeline break
    // that is neither a reconnect nor a format change. Reconnect and
    // FormatChange are covered above; an implementation could still
    // handle Discontinuity wrongly (e.g. treat it as a no-op, or as a
    // FormatChange) and pass both of those alone.
    let (mut producer, hub) = hub_with_fixture_interval();
    let subscriber = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch-1 keyframe");
    let _ = subscriber.drain();

    let epoch = producer
        .begin_epoch(
            EpochReason::Discontinuity,
            Bytes::from(vec![0, 0, 0, 1, 0x67]),
        )
        .expect("epoch bump succeeds before any terminal");
    assert_ne!(
        epoch, 1,
        "the first begin_epoch call must mint a new epoch distinct from the pre-transition \
         epoch (1), not return the existing epoch unchanged"
    );

    let mut first = unit(epoch, 0, true, false);
    first.format_change = false;
    producer
        .publish(first)
        .expect("the new epoch's own first unit is accepted starting from sequence 0");

    let delivered = subscriber
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the re-armed subscriber receives the new epoch's first access unit");

    assert!(
        delivered.discontinuity,
        "the new epoch's first delivered unit must be marked discontinuous after \
         EpochReason::Discontinuity, regardless of what the producer authored on the unit"
    );
    assert!(
        !delivered.format_change,
        "a plain discontinuity must not also mark format_change"
    );
}

#[test]
fn begin_epoch_format_change_marks_the_new_epochs_first_delivered_unit_format_changed() {
    let (mut producer, hub) = hub_with_fixture_interval();
    let subscriber = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch-1 keyframe");
    let _ = subscriber.drain();

    let epoch = producer
        .begin_epoch(
            EpochReason::FormatChange,
            Bytes::from(vec![0, 0, 0, 1, 0x68]),
        )
        .expect("epoch bump succeeds before any terminal");
    assert_ne!(
        epoch, 1,
        "the first begin_epoch call must mint a new epoch distinct from the pre-transition \
         epoch (1), not return the existing epoch unchanged"
    );

    // Same discipline as the reconnect test: author the markers false, so
    // only genuine hub enforcement can make this pass.
    let mut first = unit(epoch, 0, true, false);
    first.format_change = false;
    producer
        .publish(first)
        .expect("the new epoch's own first unit is accepted starting from sequence 0");

    let delivered = subscriber
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the re-armed subscriber receives the new epoch's first access unit");

    assert!(
        delivered.format_change,
        "the new epoch's first delivered unit must be marked format_change after a \
         FormatChange, regardless of what the producer authored on the unit"
    );
    assert!(
        delivered.discontinuity,
        "a format change is also a timeline break, so discontinuity must be set too"
    );
}

#[test]
fn begin_epoch_reconnect_opened_by_a_delta_lands_the_marker_on_the_first_keyframe_actually_delivered()
 {
    // A native camera resuming a mid-GOP reconnect cannot manufacture a
    // keyframe on demand, so the hub must accept a delta as the epoch's
    // opening unit. If the hub instead consumed its pending marker on the
    // first ACCEPTED unit regardless of whether that unit was ever
    // delivered, the marker would vanish along with the discarded delta
    // and the next keyframe would carry no discontinuity marker at all —
    // exactly the defect this pins.
    let (mut producer, hub) = hub_with_fixture_interval();
    let subscriber = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch-1 keyframe");
    let _ = subscriber.drain();

    let epoch = producer
        .begin_epoch(EpochReason::Reconnect, Bytes::from(vec![0, 0, 0, 1, 0x67]))
        .expect("epoch bump succeeds before any terminal");

    // The new epoch's opening unit is a DELTA, authored with
    // discontinuity/format_change false — only genuine hub enforcement
    // can mark the eventual keyframe.
    let mut opening_delta = unit(epoch, 0, false, false);
    opening_delta.format_change = false;
    producer
        .publish(opening_delta)
        .expect("a delta may open a new epoch — a mid-GOP reconnect is ordinary, never refused");

    // The delta is discarded (the subscriber is still awaiting random
    // access): it is never delivered as an access unit at all.
    let events_after_delta = subscriber.drain();
    assert!(
        access_unit_sequences(&events_after_delta).is_empty(),
        "the discarded epoch-opening delta must never be delivered as an access unit, got \
         {events_after_delta:?}"
    );

    let mut first_keyframe = unit(epoch, 1, true, false);
    first_keyframe.format_change = false;
    producer
        .publish(first_keyframe)
        .expect("the first natural keyframe following the delta epoch opener");

    let delivered = subscriber
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the subscriber receives the first delivered keyframe of the new epoch");

    assert_eq!(
        delivered.sequence, 1,
        "the delivered unit must be the keyframe (sequence 1), not the discarded delta (sequence 0)"
    );
    assert!(
        delivered.discontinuity,
        "the discontinuity marker must land on the first keyframe ACTUALLY DELIVERED to this \
         subscriber, even though the epoch was opened by a delta the subscriber never received"
    );
    assert!(
        !delivered.format_change,
        "a plain reconnect must not also mark format_change"
    );
}

#[test]
fn begin_epoch_format_change_opened_by_a_delta_lands_the_marker_on_the_first_keyframe_actually_delivered()
 {
    // Same defect, the other marked reason: a format-change epoch can
    // also be opened by a delta (the producer's next natural keyframe is
    // what actually carries the new format), and format_change must not
    // be lost along with a discarded opening delta either.
    let (mut producer, hub) = hub_with_fixture_interval();
    let subscriber = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch-1 keyframe");
    let _ = subscriber.drain();

    let epoch = producer
        .begin_epoch(
            EpochReason::FormatChange,
            Bytes::from(vec![0, 0, 0, 1, 0x68]),
        )
        .expect("epoch bump succeeds before any terminal");

    let mut opening_delta = unit(epoch, 0, false, false);
    opening_delta.format_change = false;
    producer
        .publish(opening_delta)
        .expect("a delta may open a new epoch — a mid-GOP reconnect is ordinary, never refused");

    let events_after_delta = subscriber.drain();
    assert!(
        access_unit_sequences(&events_after_delta).is_empty(),
        "the discarded epoch-opening delta must never be delivered as an access unit, got \
         {events_after_delta:?}"
    );

    let mut first_keyframe = unit(epoch, 1, true, false);
    first_keyframe.format_change = false;
    producer
        .publish(first_keyframe)
        .expect("the first natural keyframe following the delta epoch opener");

    let delivered = subscriber
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the subscriber receives the first delivered keyframe of the new epoch");

    assert_eq!(
        delivered.sequence, 1,
        "the delivered unit must be the keyframe (sequence 1), not the discarded delta (sequence 0)"
    );
    assert!(
        delivered.format_change,
        "the format_change marker must land on the first keyframe ACTUALLY DELIVERED to this \
         subscriber, even though the epoch was opened by a delta the subscriber never received"
    );
    assert!(
        delivered.discontinuity,
        "a format change is also a timeline break, so discontinuity must be set too"
    );
}

#[test]
fn begin_epoch_discontinuity_opened_by_a_delta_lands_the_marker_on_the_first_keyframe_actually_delivered()
 {
    // The third EpochReason variant shares the identical code path (the
    // marker-consuming logic in `publish` is not reason-specific), so it
    // is exposed to the identical defect: a Discontinuity epoch opened by
    // a delta must not lose its marker either.
    let (mut producer, hub) = hub_with_fixture_interval();
    let subscriber = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch-1 keyframe");
    let _ = subscriber.drain();

    let epoch = producer
        .begin_epoch(
            EpochReason::Discontinuity,
            Bytes::from(vec![0, 0, 0, 1, 0x67]),
        )
        .expect("epoch bump succeeds before any terminal");

    let mut opening_delta = unit(epoch, 0, false, false);
    opening_delta.format_change = false;
    producer
        .publish(opening_delta)
        .expect("a delta may open a new epoch — a mid-GOP reconnect is ordinary, never refused");

    let events_after_delta = subscriber.drain();
    assert!(
        access_unit_sequences(&events_after_delta).is_empty(),
        "the discarded epoch-opening delta must never be delivered as an access unit, got \
         {events_after_delta:?}"
    );

    let mut first_keyframe = unit(epoch, 1, true, false);
    first_keyframe.format_change = false;
    producer
        .publish(first_keyframe)
        .expect("the first natural keyframe following the delta epoch opener");

    let delivered = subscriber
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the subscriber receives the first delivered keyframe of the new epoch");

    assert_eq!(
        delivered.sequence, 1,
        "the delivered unit must be the keyframe (sequence 1), not the discarded delta (sequence 0)"
    );
    assert!(
        delivered.discontinuity,
        "the discontinuity marker must land on the first keyframe ACTUALLY DELIVERED to this \
         subscriber, even though the epoch was opened by a delta the subscriber never received"
    );
    assert!(
        !delivered.format_change,
        "a plain discontinuity must not also mark format_change"
    );
}

#[test]
fn a_late_joining_subscriber_sees_the_reconnect_marker_on_its_own_first_delivered_keyframe() {
    // The marker is epoch state held PER SUBSCRIPTION, until that
    // subscription has actually delivered a keyframe of the new epoch —
    // never a single one-shot marker that travels with the stream and
    // gets used up by whichever subscriber happens to receive the first
    // marked keyframe. A subscriber that joins strictly AFTER an earlier
    // subscriber has already consumed the marker on ITS own first
    // post-epoch keyframe must still see the marker on ITS OWN first
    // delivered keyframe of that same epoch.
    let (mut producer, hub) = hub_with_fixture_interval();
    let early = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch-1 keyframe");
    let _ = early.drain();

    let epoch = producer
        .begin_epoch(EpochReason::Reconnect, Bytes::from(vec![0, 0, 0, 1, 0x67]))
        .expect("epoch bump succeeds before any terminal");

    let mut opening_keyframe = unit(epoch, 0, true, false);
    opening_keyframe.format_change = false;
    producer
        .publish(opening_keyframe)
        .expect("the new epoch's own opening keyframe");

    let early_first = early
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the early subscriber receives the epoch's opening keyframe");
    assert!(
        early_first.discontinuity,
        "sanity: the early subscriber's own first post-epoch keyframe is marked discontinuous"
    );

    // A second subscriber joins only NOW — strictly after the marker was
    // already delivered to (and, under a one-shot-per-stream design,
    // would already be consumed by) the early subscriber.
    let late = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });

    let mut second_keyframe = unit(epoch, 1, true, false);
    second_keyframe.format_change = false;
    producer
        .publish(second_keyframe)
        .expect("a later keyframe in the same epoch");

    let late_first = late
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the late subscriber receives its own first post-join keyframe");
    assert_eq!(
        late_first.sequence, 1,
        "the late subscriber's first delivered access unit is sequence 1, its own first post-join keyframe"
    );
    assert!(
        late_first.discontinuity,
        "a subscriber that joins AFTER the epoch bump must still see the discontinuity marker on \
         its own first delivered keyframe of the new epoch, even though an earlier subscriber \
         already consumed a marked keyframe of the same epoch"
    );

    // The early subscriber, which already passed its own marker, must NOT
    // see the marker again on this second keyframe — proving the state is
    // genuinely per-subscription, not a shared flag that stays "on" for
    // every future keyframe once tripped once.
    let early_second = early
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the early subscriber also receives the second keyframe");
    assert_eq!(early_second.sequence, 1);
    assert!(
        !early_second.discontinuity,
        "the early subscriber must not see a second discontinuity marker on a later keyframe of \
         the SAME epoch it already resolved its own marker on"
    );
}

#[test]
fn a_late_joining_subscriber_sees_the_format_change_marker_on_its_own_first_delivered_keyframe() {
    // Same per-subscription requirement, for EpochReason::FormatChange.
    let (mut producer, hub) = hub_with_fixture_interval();
    let early = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch-1 keyframe");
    let _ = early.drain();

    let epoch = producer
        .begin_epoch(
            EpochReason::FormatChange,
            Bytes::from(vec![0, 0, 0, 1, 0x68]),
        )
        .expect("epoch bump succeeds before any terminal");

    let mut opening_keyframe = unit(epoch, 0, true, false);
    opening_keyframe.format_change = false;
    producer
        .publish(opening_keyframe)
        .expect("the new epoch's own opening keyframe");

    let early_first = early
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the early subscriber receives the epoch's opening keyframe");
    assert!(
        early_first.format_change,
        "sanity: the early subscriber's own first post-epoch keyframe is marked format_change"
    );

    let late = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });

    let mut second_keyframe = unit(epoch, 1, true, false);
    second_keyframe.format_change = false;
    producer
        .publish(second_keyframe)
        .expect("a later keyframe in the same epoch");

    let late_first = late
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the late subscriber receives its own first post-join keyframe");
    assert_eq!(
        late_first.sequence, 1,
        "the late subscriber's first delivered access unit is sequence 1, its own first post-join keyframe"
    );
    assert!(
        late_first.format_change,
        "a subscriber that joins AFTER the epoch bump must still see the format_change marker on \
         its own first delivered keyframe of the new epoch, even though an earlier subscriber \
         already consumed a marked keyframe of the same epoch"
    );
    assert!(
        late_first.discontinuity,
        "a format change is also a timeline break, so the late subscriber's own first delivered \
         keyframe must also carry discontinuity"
    );

    let early_second = early
        .drain()
        .into_iter()
        .find_map(|event| match event {
            TrackEvent::AccessUnit(unit) => Some(unit),
            _ => None,
        })
        .expect("the early subscriber also receives the second keyframe");
    assert_eq!(early_second.sequence, 1);
    assert!(
        !early_second.format_change,
        "the early subscriber must not see a second format_change marker on a later keyframe of \
         the SAME epoch it already resolved its own marker on"
    );
    assert!(
        !early_second.discontinuity,
        "the early subscriber must not see a second discontinuity marker on a later keyframe of \
         the SAME epoch it already resolved its own marker on"
    );
}

#[test]
fn reconnect_and_format_change_each_open_a_new_epoch_resetting_state_under_the_same_identity() {
    let (mut producer, hub) = hub_with_fixture_interval();
    let subscriber = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });

    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch-1 keyframe");
    producer
        .publish(unit(1, 1, false, false))
        .expect("epoch-1 delta");
    let _ = subscriber.drain();

    let epoch_1_config = Bytes::from(vec![0, 0, 0, 1, 0x67]);
    let epoch_2_config = Bytes::from(vec![0, 0, 0, 1, 0x68]);
    let epoch_1 = producer
        .begin_epoch(EpochReason::Reconnect, epoch_1_config)
        .expect("epoch bump succeeds before any terminal");
    let epoch_2 = producer
        .begin_epoch(EpochReason::FormatChange, epoch_2_config.clone())
        .expect("epoch bump succeeds before any terminal");

    assert_ne!(
        epoch_1, epoch_2,
        "reconnect and format change each mint a distinct epoch"
    );
    assert_eq!(
        hub.stream_id(),
        &StreamId::new("driveway"),
        "the camera/stream identity never changes across an epoch bump"
    );

    // Sequence numbering resets under the new epoch: the first unit
    // published in the new epoch is accepted at sequence 0 even though
    // epoch 1 had already reached sequence 1.
    producer
        .publish(unit(epoch_2, 0, true, false))
        .expect("the new epoch's own first unit is accepted starting from sequence 0");

    // The subscriber is re-armed under the new epoch and receives the
    // NEW epoch's codec configuration — never the prior epoch's retained
    // bytes, which must have been dropped along with the old epoch's
    // retained data.
    let events = subscriber.drain();
    let delivered_config = events.iter().find_map(|event| match event {
        TrackEvent::CodecConfig(config) => Some(config.clone()),
        _ => None,
    });
    assert_eq!(
        delivered_config.as_deref(),
        Some(&epoch_2_config[..]),
        "a re-armed subscriber must receive the NEW epoch's codec configuration, got {events:?}"
    );
    let delivered_sequence = events.iter().find_map(|event| match event {
        TrackEvent::AccessUnit(unit) => Some((unit.sequence, unit.stream_epoch)),
        _ => None,
    });
    assert_eq!(
        delivered_sequence,
        Some((0, epoch_2)),
        "a re-armed subscriber's first delivered access unit must be the new epoch's own \
         sequence-0 unit, got {events:?}"
    );

    // Camera identity and source role are unchanged by an epoch bump.
    if let Some(TrackEvent::AccessUnit(unit)) = events
        .iter()
        .find(|event| matches!(event, TrackEvent::AccessUnit(_)))
    {
        assert_eq!(unit.camera, camera_id(FIXTURE_CAMERA_ID));
        assert_eq!(unit.source_role, SourceRole::Analysis);
    }

    // The prior epoch's data is gone: publishing under the OLD epoch
    // number after the bump must be refused as a wrong-epoch unit.
    let rejected = producer.publish(unit(1, 2, false, false));
    assert!(
        matches!(
            rejected,
            Err(PublishError::WrongEpoch { expected, got }) if got == 1 && expected == epoch_2
        ),
        "a unit naming the prior, superseded epoch must be refused as WrongEpoch, got {rejected:?}"
    );
}

#[test]
fn repeated_epoch_bumps_without_draining_coalesce_codec_config_to_the_latest_not_grow_unbounded() {
    // Control events (`TrackEvent::CodecConfig`/`Fault`) sit OUTSIDE
    // `buffered_len`'s bounded budget precisely because they are never
    // droppable — which is exactly what makes them the one class a naive
    // `Vec<TrackEvent>` accumulator could grow without bound. Bump the
    // epoch many times with the subscriber never draining, and prove the
    // pending, undelivered codec configuration is bounded/coalesced to
    // the LATEST value rather than piling up one entry per bump.
    let (mut producer, hub) = hub_with_fixture_interval();
    let subscriber = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });

    let mut last_config = Bytes::new();
    for index in 0..200u32 {
        last_config = Bytes::from(format!("config-{index}").into_bytes());
        producer
            .begin_epoch(EpochReason::FormatChange, last_config.clone())
            .expect("epoch bump succeeds before any terminal");
    }

    let events = subscriber.drain();
    let codec_configs: Vec<Bytes> = events
        .iter()
        .filter_map(|event| match event {
            TrackEvent::CodecConfig(config) => Some(config.clone()),
            _ => None,
        })
        .collect();

    assert!(
        codec_configs.len() <= 1,
        "control events (never droppable) must be bounded/coalesced across repeated epoch \
         bumps — 200 undrained epoch bumps must not accumulate 200 CodecConfig events, got {}",
        codec_configs.len()
    );
    if let Some(config) = codec_configs.first() {
        assert_eq!(
            config.as_ref(),
            &last_config[..],
            "when coalesced, only the LATEST pending codec configuration may survive, never a \
             stale one from an earlier bump"
        );
    }
}

#[test]
fn publish_rejects_a_unit_that_does_not_continue_the_epoch_monotonic_sequence() {
    let (mut producer, _hub) = hub_with_fixture_interval();

    producer
        .publish(unit(1, 0, true, true))
        .expect("first unit");
    producer
        .publish(unit(1, 1, false, false))
        .expect("second unit");

    let rejected = producer.publish(unit(1, 1, false, false));
    assert!(
        matches!(
            rejected,
            Err(PublishError::SequenceNotMonotonic {
                expected_at_least: 2,
                got: 1,
            })
        ),
        "publishing a non-increasing sequence within the same epoch must be rejected with the \
         exact expected-vs-got sequence, not silently accepted, got {rejected:?}"
    );
}

#[test]
fn publish_rejects_a_unit_naming_the_wrong_stream() {
    let (mut producer, _hub) = hub_with_fixture_interval();
    producer
        .publish(unit(1, 0, true, true))
        .expect("first unit");

    let wrong_stream_unit = unit_for(
        StreamId::new("some-other-camera"),
        camera_id(FIXTURE_CAMERA_ID),
        SourceRole::Analysis,
        VideoCodec::H264,
        1,
        1,
        false,
        false,
    );
    let rejected = producer.publish(wrong_stream_unit);
    assert!(
        matches!(rejected, Err(PublishError::WrongStream { .. })),
        "a unit naming a different stream than this hub was CONSTRUCTED for must be refused, \
         not silently accepted into the wrong stream's fan-out, got {rejected:?}"
    );
}

#[test]
fn publish_rejects_a_unit_naming_the_wrong_camera() {
    let (mut producer, _hub) = hub_with_fixture_interval();
    producer
        .publish(unit(1, 0, true, true))
        .expect("first unit");

    let wrong_camera_unit = unit_for(
        StreamId::new("driveway"),
        camera_id("some-other-camera-durable-id"),
        SourceRole::Analysis,
        VideoCodec::H264,
        1,
        1,
        false,
        false,
    );
    let rejected = producer.publish(wrong_camera_unit);
    assert!(
        matches!(rejected, Err(PublishError::WrongCamera { .. })),
        "a unit naming a different camera identity than this hub was CONSTRUCTED for must be \
         refused — the hub never adopts identity from the first publish, got {rejected:?}"
    );
}

#[test]
fn publish_rejects_a_unit_naming_the_wrong_source_role() {
    let (mut producer, _hub) = hub_with_fixture_interval();
    producer
        .publish(unit(1, 0, true, true))
        .expect("first unit");

    let wrong_role_unit = unit_for(
        StreamId::new("driveway"),
        camera_id(FIXTURE_CAMERA_ID),
        SourceRole::Live,
        VideoCodec::H264,
        1,
        1,
        false,
        false,
    );
    let rejected = producer.publish(wrong_role_unit);
    assert!(
        matches!(rejected, Err(PublishError::WrongSourceRole { .. })),
        "a unit whose source role does not match what this hub was CONSTRUCTED for must be \
         refused, got {rejected:?}"
    );
}

#[test]
fn publish_rejects_a_unit_naming_the_wrong_codec() {
    let (mut producer, _hub) = hub_with_fixture_interval();
    producer
        .publish(unit(1, 0, true, true))
        .expect("first unit");

    let wrong_codec_unit = unit_for(
        StreamId::new("driveway"),
        camera_id(FIXTURE_CAMERA_ID),
        SourceRole::Analysis,
        VideoCodec::H265,
        1,
        1,
        false,
        false,
    );
    let rejected = producer.publish(wrong_codec_unit);
    assert!(
        matches!(rejected, Err(PublishError::WrongCodec { .. })),
        "a unit whose codec does not match what this hub was CONSTRUCTED for must be refused, \
         got {rejected:?}"
    );
}

#[test]
fn a_hub_constructed_for_one_camera_role_and_codec_never_adopts_a_different_first_publish() {
    // The structural proof point: publish a WRONG-identity unit FIRST,
    // before anything else has ever been accepted, and confirm it is
    // still refused — a hub that instead learned its identity from
    // whatever unit arrived first would wrongly accept this. Covers all
    // FOUR values a hub is bound to at construction (stream, camera,
    // source role, codec) — checking only camera would let a hub still
    // adopt the first unit's stream/role/codec and pass.
    struct FirstPublishCase {
        name: &'static str,
        stream_id: StreamId,
        camera: CameraId,
        source_role: SourceRole,
        codec: VideoCodec,
        // The SPECIFIC variant this case must produce — not just any
        // `Err(_)`. Without this, one blanket rejection reason (e.g.
        // always reporting WrongStream) would pass all four cases.
        expected_error: fn(&PublishError) -> bool,
        expected_error_name: &'static str,
    }
    let cases = [
        FirstPublishCase {
            name: "wrong stream",
            stream_id: StreamId::new("some-other-camera"),
            camera: camera_id(FIXTURE_CAMERA_ID),
            source_role: SourceRole::Analysis,
            codec: VideoCodec::H264,
            expected_error: |error| matches!(error, PublishError::WrongStream { .. }),
            expected_error_name: "WrongStream",
        },
        FirstPublishCase {
            name: "wrong camera",
            stream_id: StreamId::new("driveway"),
            camera: camera_id("not-the-constructed-camera"),
            source_role: SourceRole::Analysis,
            codec: VideoCodec::H264,
            expected_error: |error| matches!(error, PublishError::WrongCamera { .. }),
            expected_error_name: "WrongCamera",
        },
        FirstPublishCase {
            name: "wrong source role",
            stream_id: StreamId::new("driveway"),
            camera: camera_id(FIXTURE_CAMERA_ID),
            source_role: SourceRole::Live,
            codec: VideoCodec::H264,
            expected_error: |error| matches!(error, PublishError::WrongSourceRole { .. }),
            expected_error_name: "WrongSourceRole",
        },
        FirstPublishCase {
            name: "wrong codec",
            stream_id: StreamId::new("driveway"),
            camera: camera_id(FIXTURE_CAMERA_ID),
            source_role: SourceRole::Analysis,
            codec: VideoCodec::H265,
            expected_error: |error| matches!(error, PublishError::WrongCodec { .. }),
            expected_error_name: "WrongCodec",
        },
    ];

    for case in cases {
        let (mut producer, hub) = hub_with_fixture_interval();
        let wrong_first_unit = unit_for(
            case.stream_id,
            case.camera,
            case.source_role,
            case.codec,
            1,
            0,
            true,
            true,
        );
        let rejected = producer.publish(wrong_first_unit);
        match &rejected {
            Err(error) => assert!(
                (case.expected_error)(error),
                "[{}] expected {}, got {rejected:?}",
                case.name,
                case.expected_error_name
            ),
            Ok(()) => panic!(
                "[{}] the very FIRST publish to a freshly constructed hub must still be checked \
                 against the identity bound at construction, never adopted as the hub's \
                 identity, expected {}",
                case.name, case.expected_error_name
            ),
        }
        assert_eq!(
            hub.camera(),
            &camera_id(FIXTURE_CAMERA_ID),
            "[{}] a rejected first publish must not have changed the hub's bound camera \
             identity",
            case.name
        );
        assert_eq!(
            hub.source_role(),
            SourceRole::Analysis,
            "[{}] a rejected first publish must not have changed the hub's bound source role",
            case.name
        );
        assert_eq!(
            hub.codec(),
            VideoCodec::H264,
            "[{}] a rejected first publish must not have changed the hub's bound codec",
            case.name
        );
        assert_eq!(
            hub.stream_id(),
            &StreamId::new("driveway"),
            "[{}] a rejected first publish must not have changed the hub's bound stream id",
            case.name
        );
    }
}

// --- async recv + explicit terminal events -------------------------------
//
// `Subscription::recv` is the async counterpart to `Subscription::drain`:
// a consumer awaits the next event instead of busy-polling. Every test
// below drives the returned future by hand with a waker that records
// whether it actually fired — never a sleep, a clock read, or a
// wall-clock deadline — so "pending until woken" is asserted as a real
// causal fact, not inferred from timing.

#[test]
fn recv_is_pending_until_a_publish_wakes_it_then_delivers_codec_config_before_the_access_unit() {
    let (mut producer, hub) = hub_with_fixture_interval();
    let mut subscription = hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(8),
    });

    let mut first = Box::pin(subscription.recv());
    let (waker, flag) = recording_waker();
    assert!(
        matches!(poll_once(first.as_mut(), &waker), Poll::Pending),
        "a fresh join with nothing published yet must not fabricate an event"
    );
    assert!(
        !flag.0.load(Ordering::SeqCst),
        "the waker must not fire before anything happens"
    );

    producer
        .publish(unit(1, 0, true, true))
        .expect("first unit of an epoch is always accepted");

    assert!(
        flag.0.load(Ordering::SeqCst),
        "publishing a unit for a subscription with a pending recv must wake it"
    );
    match poll_once(first.as_mut(), &waker) {
        Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::CodecConfig(config)))) => {
            assert_eq!(config, Bytes::from(vec![0, 0, 0, 1, 0x67]));
        }
        other => panic!("expected the join's codec configuration first, got {other:?}"),
    }
    // `recv` takes `&mut self` (the one-receiver-per-subscription
    // guarantee), so the prior future must be dropped before a new one can
    // mutably borrow `subscription` again.
    drop(first);

    let mut second = Box::pin(subscription.recv());
    match poll_once(second.as_mut(), &waker) {
        Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::AccessUnit(access_unit)))) => {
            assert_eq!(access_unit.sequence, 0);
        }
        other => panic!("expected the published access unit second, got {other:?}"),
    }
    drop(second);

    let mut third = Box::pin(subscription.recv());
    assert!(
        matches!(poll_once(third.as_mut(), &waker), Poll::Pending),
        "nothing further is buffered, so recv must not fabricate a third event"
    );
}

#[test]
fn recv_delivers_every_buffered_track_event_before_the_terminal_source_ended_event_then_stays_ended()
 {
    let (mut producer, hub) = hub_with_fixture_interval();
    let mut subscription = hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(8),
    });

    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch-1 keyframe");
    producer
        .publish(unit(1, 1, false, false))
        .expect("epoch-1 delta following the keyframe");

    // Raised BEFORE anything has drained: two TrackEvents (the codec
    // config and both access units) are still owed to this subscription.
    producer.end_source();

    let (waker, _flag) = recording_waker();

    let mut codec = Box::pin(subscription.recv());
    match poll_once(codec.as_mut(), &waker) {
        Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::CodecConfig(_)))) => {}
        other => panic!(
            "a terminal raised while events were still owed must not jump the queue; expected \
             codec config first, got {other:?}"
        ),
    }
    drop(codec);

    let mut au0 = Box::pin(subscription.recv());
    match poll_once(au0.as_mut(), &waker) {
        Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::AccessUnit(access_unit)))) => {
            assert_eq!(access_unit.sequence, 0);
        }
        other => panic!("expected the first buffered access unit, got {other:?}"),
    }
    drop(au0);

    let mut au1 = Box::pin(subscription.recv());
    match poll_once(au1.as_mut(), &waker) {
        Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::AccessUnit(access_unit)))) => {
            assert_eq!(access_unit.sequence, 1);
        }
        other => panic!("expected the second buffered access unit, got {other:?}"),
    }
    drop(au1);

    let mut end = Box::pin(subscription.recv());
    match poll_once(end.as_mut(), &waker) {
        Poll::Ready(Some(SubscriptionEvent::End(SubscriptionEnd::SourceEnded))) => {}
        other => panic!("the terminal must arrive only after every owed TrackEvent, got {other:?}"),
    }
    drop(end);

    for attempt in 0..2 {
        let mut after = Box::pin(subscription.recv());
        assert!(
            matches!(poll_once(after.as_mut(), &waker), Poll::Ready(None)),
            "attempt {attempt}: once the terminal has been delivered, every later call must \
             resolve immediately to None, never repeat the terminal, never resurrect the \
             subscription"
        );
        drop(after);
    }
}

#[test]
fn abandon_source_is_terminal_carries_the_reason_and_the_first_raised_terminal_wins() {
    let (mut producer, hub) = hub_with_fixture_interval();
    let mut subscription = hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(8),
    });

    producer.abandon_source("RTSP handshake failed on every retry".to_string());
    // A second call, with a DIFFERENT reason, must never overwrite or
    // duplicate the terminal this subscription already owns.
    producer.abandon_source("a later, different reason".to_string());

    let (waker, _flag) = recording_waker();

    let mut first = Box::pin(subscription.recv());
    match poll_once(first.as_mut(), &waker) {
        Poll::Ready(Some(SubscriptionEvent::End(SubscriptionEnd::SourceFault { reason }))) => {
            assert_eq!(
                reason, "RTSP handshake failed on every retry",
                "the FIRST raised terminal's reason must win, never a later overwrite"
            );
        }
        other => panic!("expected a SourceFault terminal, got {other:?}"),
    }
    drop(first);

    let mut second = Box::pin(subscription.recv());
    assert!(
        matches!(poll_once(second.as_mut(), &waker), Poll::Ready(None)),
        "the terminal is yielded at most once; a second abandon_source call must not queue a \
         second one"
    );
}

#[test]
fn a_transient_fault_and_reconnect_never_raises_a_terminal_and_delivery_survives_many_retries() {
    let (mut producer, hub) = hub_with_fixture_interval();
    let mut subscription = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });

    producer
        .publish(unit(1, 0, true, true))
        .expect("epoch-1 keyframe");
    {
        let (waker, _flag) = recording_waker();
        let mut codec = Box::pin(subscription.recv());
        assert!(matches!(
            poll_once(codec.as_mut(), &waker),
            Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::CodecConfig(_))))
        ));
        drop(codec);
        let mut au = Box::pin(subscription.recv());
        assert!(matches!(
            poll_once(au.as_mut(), &waker),
            Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::AccessUnit(_))))
        ));
        drop(au);
    }

    // Supervision retries the camera twice. Each retry opens a new epoch
    // via `begin_epoch` — this alone must NEVER be mistaken for a
    // terminal, and delivery must keep working into every new epoch.
    for retry in 0..2 {
        let epoch = producer
            .begin_epoch(EpochReason::Reconnect, Bytes::from(vec![0, 0, 0, 1, 0x67]))
            .expect("epoch bump succeeds before any terminal");

        // begin_epoch retains the new epoch's codec configuration eagerly,
        // so it is immediately available — but it must be a Track event,
        // never the terminal.
        let (waker, _flag) = recording_waker();
        let mut codec = Box::pin(subscription.recv());
        match poll_once(codec.as_mut(), &waker) {
            Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::CodecConfig(_)))) => {}
            other => panic!(
                "retry {retry}: an epoch bump alone must never raise a terminal, got {other:?}"
            ),
        }
        drop(codec);

        // With nothing yet published in the new epoch, recv must be
        // pending — never End — until the producer actually publishes.
        let mut pending = Box::pin(subscription.recv());
        let (waker, flag) = recording_waker();
        assert!(
            matches!(poll_once(pending.as_mut(), &waker), Poll::Pending),
            "retry {retry}: an epoch bump with nothing yet published must not fabricate a \
             terminal"
        );
        assert!(!flag.0.load(Ordering::SeqCst));

        producer
            .publish(unit(epoch, 0, true, false))
            .expect("the new epoch's own opening keyframe");

        assert!(
            flag.0.load(Ordering::SeqCst),
            "retry {retry}: the new epoch's opening keyframe must wake the pending recv"
        );
        match poll_once(pending.as_mut(), &waker) {
            Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::AccessUnit(access_unit)))) => {
                assert!(
                    access_unit.discontinuity,
                    "retry {retry}: the reconnect must mark the new epoch's first delivered \
                     unit discontinuous"
                );
            }
            other => panic!("retry {retry}: expected the new epoch's access unit, got {other:?}"),
        }
    }
}

#[test]
fn hub_shutdown_is_terminal_for_every_attached_subscription_and_is_distinct_from_source_terminals()
{
    let (_producer, hub) = hub_with_fixture_interval();
    let mut a = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    let mut b = hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(8),
    });

    hub.shutdown();

    let (waker, _flag) = recording_waker();
    // `recv` takes `&mut self` (one receiver per subscription — pinned as a
    // `compile_fail` doctest beside `Subscription::recv` in camera_hub.rs,
    // since a runtime test cannot express a compile-time guarantee), so
    // each entry borrows its OWN subscription mutably rather than iterating
    // a shared `&Subscription`.
    for (name, subscription) in [("a", &mut a), ("b", &mut b)] {
        let mut first = Box::pin(subscription.recv());
        match poll_once(first.as_mut(), &waker) {
            Poll::Ready(Some(SubscriptionEvent::End(SubscriptionEnd::HubShutdown))) => {}
            other => panic!(
                "subscription {name}: hub shutdown must be a distinct HubShutdown terminal, \
                 got {other:?}"
            ),
        }
        drop(first);
        for attempt in 0..2 {
            let mut after = Box::pin(subscription.recv());
            assert!(
                matches!(poll_once(after.as_mut(), &waker), Poll::Ready(None)),
                "subscription {name}, attempt {attempt}: the terminal must not repeat and the \
                 subscription must not resurrect"
            );
            drop(after);
        }
    }
}

// --- Independent-review defects: terminal durability, FaultOnGap under
// `recv`, a detach's dropped waker, and a leak-proof `SourceFault` reason.
// Each test below pins exactly one absent case a do-nothing/buggy
// implementation currently passes through unnoticed. ---

#[test]
fn a_late_joining_subscriber_learns_the_source_ended_terminal_raised_before_it_joined() {
    // `end_source` (like `abandon_source`/`shutdown`) marks only the
    // subscribers already attached at the moment it runs. A subscriber
    // that joins AFTER must learn the source is gone too — not wait
    // forever on a stream that will never deliver anything.
    //
    // Publishing a real keyframe BEFORE the terminal, so the hub has
    // genuinely retained codec configuration, matters: `subscribe` seeds a
    // NEW subscriber's `pending_codec_config` from `retained_codec_config`
    // unconditionally, even when it is ALSO seeding `pending_end` from the
    // hub-wide terminal — a hub tested only against an EMPTY hub (nothing
    // ever retained) can never expose that the late joiner's first event is
    // stale codec configuration ahead of its terminal. `subscribe` also
    // unconditionally sets `keyframe_requested`, so a terminal-inheriting
    // join asks the producer for a keyframe it can never honor — proven
    // here too.
    let (mut producer, hub) = hub_with_fixture_interval();
    producer
        .publish(unit(1, 0, true, true))
        .expect("a real keyframe establishes retained codec configuration");
    producer.end_source();

    let mut late_joiner = hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(8),
    });

    let (waker, _flag) = recording_waker();
    let mut first = Box::pin(late_joiner.recv());
    match poll_once(first.as_mut(), &waker) {
        Poll::Ready(Some(SubscriptionEvent::End(SubscriptionEnd::SourceEnded))) => {}
        other => panic!(
            "a subscriber joining AFTER the source has already ended must learn that \
             immediately as its FIRST event — never hang forever waiting on a stream that will \
             never deliver anything, and never see the hub's retained codec configuration ahead \
             of the terminal it already owns, got {other:?}"
        ),
    }
    assert!(
        !producer.keyframe_requested_since_last_check(),
        "a join that inherits an already-raised terminal must not also request a producer \
         keyframe that a source which has already ended can never honor"
    );
}

#[test]
fn a_late_joining_subscriber_learns_hub_shutdown_raised_before_it_joined() {
    // Same treatment as the source-ended sibling above: a real keyframe is
    // published first so the hub genuinely retains codec configuration
    // ahead of the shutdown, and the keyframe-request signal is checked
    // too.
    let (mut producer, hub) = hub_with_fixture_interval();
    producer
        .publish(unit(1, 0, true, true))
        .expect("a real keyframe establishes retained codec configuration");
    hub.shutdown();

    let mut late_joiner = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });

    let (waker, _flag) = recording_waker();
    let mut first = Box::pin(late_joiner.recv());
    match poll_once(first.as_mut(), &waker) {
        Poll::Ready(Some(SubscriptionEvent::End(SubscriptionEnd::HubShutdown))) => {}
        other => panic!(
            "a subscriber joining AFTER the hub has already shut down must learn that \
             immediately as its FIRST event — never see the hub's retained codec configuration \
             ahead of the terminal it already owns, got {other:?}"
        ),
    }
    assert!(
        !producer.keyframe_requested_since_last_check(),
        "a join that inherits an already-raised terminal must not also request a producer \
         keyframe that a shut-down hub can never honor"
    );
}

#[test]
fn publish_is_refused_once_the_source_has_ended() {
    let (mut producer, hub) = hub_with_fixture_interval();
    let _subscription = hub.subscribe(SubscribeOptions {
        loss: LossContract::RejoinAtRandomAccess,
        capacity: capacity(8),
    });
    producer.end_source();

    let result = producer.publish(unit(1, 0, true, true));
    assert!(
        matches!(
            result,
            Err(PublishError::ProducerEnded(SubscriptionEnd::SourceEnded))
        ),
        "a publish after the source has already ended must be REFUSED, never silently accepted \
         into a stream every subscriber has already been told is over, got {result:?}"
    );
}

#[test]
fn begin_epoch_is_refused_once_the_hub_has_shut_down() {
    let (mut producer, hub) = hub_with_fixture_interval();
    hub.shutdown();

    let result = producer.begin_epoch(EpochReason::Reconnect, Bytes::from(vec![0, 0, 0, 1, 0x67]));
    assert!(
        matches!(
            result,
            Err(PublishError::ProducerEnded(SubscriptionEnd::HubShutdown))
        ),
        "an epoch bump after the hub has already shut down must be REFUSED, never silently \
         reopening a stream every subscriber has already been told is over, got {result:?}"
    );
}

#[test]
fn fault_on_gap_is_terminal_under_recv_not_only_drain() {
    // The neighboring `fault_on_gap_surfaces_the_exact_lost_range_stays_bounded_and_is_terminal`
    // test above only proves terminality via `drain`/`is_detached`. This
    // pins the SAME terminality under the async `recv` path: once the
    // coverage fault has been delivered, the next `recv` call must not
    // hang forever — it must resolve to EXACTLY `Ready(None)`, reporting
    // the subscription is over, exactly like every other exhausted
    // terminal path in this file. A looser `Poll::Ready(_)` check here
    // would also accept an invented `Ready(Some(SubscriptionEvent::End(..)))`
    // — an implementation that fabricates a terminal instead of reporting
    // the subscription simply ended must fail this test, not pass it.
    let (mut producer, hub) = hub_with_fixture_interval();
    let mut strict = hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(2),
    });

    producer.publish(unit(1, 0, true, true)).expect("keyframe");
    producer
        .publish(unit(1, 1, false, false))
        .expect("delta that still fits in capacity 2");
    producer
        .publish(unit(1, 2, false, false))
        .expect("delta that overflows the strict subscriber's capacity-2 queue");

    let (waker, _flag) = recording_waker();

    // Drain the codec config and the two buffered access units first — recv
    // delivers every already-buffered TrackEvent before anything terminal,
    // exactly like `drain`'s own ordering.
    for _ in 0..3 {
        let mut event = Box::pin(strict.recv());
        match poll_once(event.as_mut(), &waker) {
            Poll::Ready(Some(SubscriptionEvent::Track(_))) => {}
            other => panic!("expected an ordinary buffered Track event, got {other:?}"),
        }
    }

    let mut fault_event = Box::pin(strict.recv());
    match poll_once(fault_event.as_mut(), &waker) {
        Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::Fault(_)))) => {}
        other => panic!("expected the coverage fault, got {other:?}"),
    }
    drop(fault_event);

    let mut after_fault = Box::pin(strict.recv());
    match poll_once(after_fault.as_mut(), &waker) {
        Poll::Ready(None) => {}
        Poll::Pending => panic!(
            "a recv() call after a FaultOnGap coverage fault has already been delivered must \
             not hang forever — it must report the subscription has ended, exactly like every \
             other terminal path in this file"
        ),
        other => panic!(
            "a detached FaultOnGap subscription's terminal is reporting the subscription is \
             OVER (`Ready(None)`, exactly like every other exhausted terminal path in this \
             file) — never a fabricated event of any other shape, including an invented \
             `SubscriptionEvent::End`, got {other:?}"
        ),
    }
}

#[test]
fn detach_wakes_a_parked_recv_which_then_observes_the_end() {
    let (_producer, hub) = hub_with_fixture_interval();
    let mut subscription = hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(8),
    });
    let id = subscription.id();

    let mut pending = Box::pin(subscription.recv());
    let (waker, flag) = recording_waker();
    assert!(
        matches!(poll_once(pending.as_mut(), &waker), Poll::Pending),
        "a fresh join with nothing published yet must not fabricate an event"
    );
    assert!(
        !flag.0.load(Ordering::SeqCst),
        "the waker must not fire before anything happens"
    );

    hub.detach(id);

    assert!(
        flag.0.load(Ordering::SeqCst),
        "detaching a subscription with a parked recv() waker must wake it — dropping a \
         registered waker silently leaves the awaiting task hanging forever"
    );
    match poll_once(pending.as_mut(), &waker) {
        Poll::Ready(None) => {}
        other => panic!(
            "after being woken by its own detach, the parked recv() must resolve reporting the \
             subscription has ended, got {other:?}"
        ),
    }
}

#[test]
fn a_detached_fault_on_gap_subscription_never_yields_a_second_terminal_after_a_later_hub_wide_terminal()
 {
    // `fault_on_gap_is_terminal_under_recv_not_only_drain` (above) proves recv()
    // resolves once a FaultOnGap subscriber has been detached by its own
    // coverage fault. That resolution is NOT the same internal state as a
    // delivered `SubscriptionEnd`: the detached path returns `Ready(None)`
    // directly, without ever setting `end_delivered`. A hub-wide terminal
    // raised AFTER this detach walks every entry in the subscriber table,
    // including already-detached ones, and finds `pending_end` still `None`
    // and `end_delivered` still `false` — so it hands this subscription a
    // brand-new `pending_end`, and the next recv() yields a SECOND terminal,
    // `End(..)`, for a subscription that already reported itself over.
    let (mut producer, hub) = hub_with_fixture_interval();
    let mut strict = hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(2),
    });

    producer.publish(unit(1, 0, true, true)).expect("keyframe");
    producer
        .publish(unit(1, 1, false, false))
        .expect("delta that still fits in capacity 2");
    producer
        .publish(unit(1, 2, false, false))
        .expect("delta that overflows the strict subscriber's capacity-2 queue");

    let (waker, _flag) = recording_waker();

    // Drain the codec config and the two buffered access units, then the
    // coverage fault itself — the subscription's own, one and only terminal.
    for _ in 0..3 {
        let mut event = Box::pin(strict.recv());
        assert!(
            matches!(
                poll_once(event.as_mut(), &waker),
                Poll::Ready(Some(SubscriptionEvent::Track(_)))
            ),
            "expected an ordinary buffered Track event ahead of the coverage fault"
        );
    }
    let mut fault_event = Box::pin(strict.recv());
    assert!(
        matches!(
            poll_once(fault_event.as_mut(), &waker),
            Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::Fault(_))))
        ),
        "expected the coverage fault"
    );
    drop(fault_event);

    let mut first_none = Box::pin(strict.recv());
    assert!(
        matches!(poll_once(first_none.as_mut(), &waker), Poll::Ready(None)),
        "the FaultOnGap subscriber's own coverage fault is its one and only terminal — the next \
         recv() must already report the subscription over"
    );
    drop(first_none);

    // A later, hub-wide terminal fires for every other subscriber. It must
    // never resurrect this already-terminalized, detached subscription with
    // a second, different terminal event.
    producer.end_source();

    let mut second = Box::pin(strict.recv());
    match poll_once(second.as_mut(), &waker) {
        Poll::Ready(None) => {}
        other => panic!(
            "a subscription that already yielded its terminal (the coverage fault, observed via \
             the prior recv() resolving) must not yield a SECOND terminal after a later \
             hub-wide terminal is raised, got {other:?}"
        ),
    }
}

#[test]
fn a_hub_wide_terminal_raised_between_a_fault_on_gap_overflow_and_its_next_poll_never_yields_a_second_terminal()
 {
    // The test above (`a_detached_fault_on_gap_subscription_never_yields_a_second_terminal_after_a_later_hub_wide_terminal`)
    // only raises the hub-wide terminal AFTER the subscriber has already
    // polled its own `Ready(None)` — the safe ordering, where `end_delivered`
    // is already `true` and `raise_terminal`'s existing `end_delivered`
    // check alone is enough to protect it. It never exercises the actual
    // race: a FaultOnGap overflow sets `detached = true` and delivers the
    // coverage fault as a buffered `TrackEvent`, but does NOT set
    // `pending_end` or `end_delivered` — those are only set by the
    // subscriber's OWN next `poll_recv` call, once it finds nothing else
    // buffered. If a hub-wide terminal (`end_source`/`abandon_source`/
    // `shutdown`) is raised in the WINDOW between the overflow and that next
    // poll, `raise_terminal` finds `pending_end` still `None` and
    // `end_delivered` still `false` on this already-detached subscriber, and
    // hands it a brand-new `pending_end` — so the very next `recv()` yields
    // a SECOND terminal, `End(..)`, after the coverage fault, for a
    // subscription that had already been detached.
    let (mut producer, hub) = hub_with_fixture_interval();
    let mut strict = hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(2),
    });

    producer.publish(unit(1, 0, true, true)).expect("keyframe");
    producer
        .publish(unit(1, 1, false, false))
        .expect("delta that still fits in capacity 2");
    producer
        .publish(unit(1, 2, false, false))
        .expect("delta that overflows the strict subscriber's capacity-2 queue, detaching it");

    let (waker, _flag) = recording_waker();

    // Drain the codec config and the two buffered access units, then the
    // coverage fault itself. After this, the subscriber is `detached` but
    // has NOT yet polled its own `Ready(None)` — `pending_end` is still
    // `None` and `end_delivered` is still `false`. This is the exact window
    // the real defect lives in.
    for _ in 0..3 {
        let mut event = Box::pin(strict.recv());
        assert!(
            matches!(
                poll_once(event.as_mut(), &waker),
                Poll::Ready(Some(SubscriptionEvent::Track(_)))
            ),
            "expected an ordinary buffered Track event ahead of the coverage fault"
        );
    }
    let mut fault_event = Box::pin(strict.recv());
    assert!(
        matches!(
            poll_once(fault_event.as_mut(), &waker),
            Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::Fault(_))))
        ),
        "expected the coverage fault"
    );
    drop(fault_event);

    // Raise the hub-wide terminal RIGHT HERE — before this subscription has
    // ever polled `Ready(None)` for itself. A correct implementation must
    // recognize this subscriber as already over (via `detached`) and refuse
    // to hand it a fresh `pending_end`.
    producer.end_source();

    let mut after_overflow = Box::pin(strict.recv());
    match poll_once(after_overflow.as_mut(), &waker) {
        Poll::Ready(None) => {}
        other => panic!(
            "a FaultOnGap subscriber already detached by its own coverage fault must not yield \
             a SECOND terminal even when a hub-wide terminal is raised in the window before its \
             own next poll, got {other:?}"
        ),
    }
}

// The former `a_waker_invoked_from_inside_publish_can_safely_touch_the_hub_without_deadlocking`
// test lived here. It inferred "no deadlock" from `JoinHandle::is_finished`
// after up to 200,000 cooperative yields on a spawned thread — liveness
// evidence, not a proof: the outcome depends on OS/thread scheduling, and on
// a loaded box the bounded spin could plausibly report either way. It is
// replaced by a deterministic unit-level `try_lock` assertion,
// `a_waker_invoked_from_inside_publish_finds_the_hub_state_lock_already_released`,
// in `crates/vigil/src/camera_hub.rs`'s own `#[cfg(test)] mod tests` — the
// only place with direct access to the hub's private internal `Mutex`,
// which is what makes `try_lock` (an immediate, non-blocking check) usable
// as evidence at all instead of a second spin.

#[test]
fn source_fault_reason_cannot_leak_into_debug_output() {
    // No production adapter calls `abandon_source` yet, but the caller
    // contract accepts arbitrary adapter error text — which can carry a
    // credential-bearing string (e.g. an RTSP URL with an embedded
    // password). `SubscriptionEnd::SourceFault` must be structurally
    // leak-proof: nothing this type exposes may ever render that text into
    // a string, `Debug` included — a type that cannot carry a secret is
    // what closes this, never a redaction convention every future call
    // site would have to remember.
    let secret = "hunter2-super-secret-rtsp-password";
    let (mut producer, hub) = hub_with_fixture_interval();
    let mut subscription = hub.subscribe(SubscribeOptions {
        loss: LossContract::FaultOnGap,
        capacity: capacity(8),
    });

    producer.abandon_source(format!(
        "connect failed: rtsp://user:{secret}@camera.local/stream"
    ));

    let (waker, _flag) = recording_waker();
    let mut event = Box::pin(subscription.recv());
    let end = match poll_once(event.as_mut(), &waker) {
        Poll::Ready(Some(SubscriptionEvent::End(end))) => end,
        other => panic!("expected a SourceFault terminal, got {other:?}"),
    };

    let debug_repr = format!("{end:?}");
    assert!(
        !debug_repr.contains(secret),
        "a password-shaped adapter error string reached the Debug output of a terminal a real \
         consumer might log — SourceFault must be structurally leak-proof, got {debug_repr:?}"
    );
}
