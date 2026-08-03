//! The camera-track hub: bounded, per-consumer fan-out over one camera's
//! [`EncodedAccessUnit`](crate::camera_track::EncodedAccessUnit) stream.
//!
//! Deliberately NOT built on `tokio::broadcast`: broadcast's lag behavior is
//! implicit and uniform across every receiver, which contradicts the
//! per-consumer DECLARED loss contract this hub exists to offer (a live
//! viewer may drop and rejoin at a keyframe; a future continuous recorder
//! must never silently drop and instead surfaces a classified fault). The
//! hub owns a bounded ring per subscription and its own cursor, so one
//! slow or failed consumer can be detached without touching the producer
//! or any other consumer.

use std::collections::{HashMap, VecDeque};
use std::future::poll_fn;
use std::num::NonZeroUsize;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use bytes::Bytes;

use crate::camera_track::{CameraId, EncodedAccessUnit, SourceRole};
use crate::media_pipeline::VideoCodec;
use crate::workgraph::StreamId;

/// A subscriber's declared behavior when it cannot keep pace with the
/// producer. Named for what each contract DOES, never for a consumer class
/// ("live"/"recorder") — the same hub serves any future consumer that
/// declares one of these two contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossContract {
    /// On overflow, discard the stale cursor and rejoin at the next
    /// decodable random-access point: codec configuration, then the first
    /// keyframe, with no intervening delta unit ever delivered.
    RejoinAtRandomAccess,
    /// Never silently drop. On overflow, surface a typed [`CoverageFault`]
    /// naming the lost sequence range and detach — inability to keep pace
    /// becomes a classified fault, never a quiet gap.
    FaultOnGap,
}

/// The floor under the derived automatic subscriber capacity (see
/// [`automatic_subscriber_capacity`]): under the automatic keyframe
/// interval (clamped to 15..=300 frames by default) this floor never
/// actually binds — it exists so an operator who explicitly overrides the
/// keyframe interval below 15 frames cannot also drag the derived capacity
/// down into a window too small to hold a full GOP. The derivation this
/// floor feeds is `max(effective_keyframe_interval_frames, 15)`: the
/// property it protects is that a subscriber's retained window always
/// contains at least one full keyframe, so a consumer that may drop and
/// rejoin always has a random-access point.
///
/// Deliberately NOT its own independently typed literal `15`: it reads
/// `crate::encode::KEYFRAME_INTERVAL_MIN_FRAMES_AUTOMATIC_DEFAULT` — the
/// exact same automatic default the `keyframe_interval_min_frames`
/// setting declares in `config::declare_settings` — so the ratified value
/// lives in exactly one place. A second, independently duplicated `15`
/// here would silently drift the moment that default ever changed.
pub const AUTOMATIC_CAPACITY_FLOOR_FRAMES: usize =
    crate::encode::KEYFRAME_INTERVAL_MIN_FRAMES_AUTOMATIC_DEFAULT as usize;

/// The automatic subscriber queue capacity, in ACCESS UNITS, for a stream
/// whose effective keyframe interval is `effective_keyframe_interval_frames`
/// output frames.
///
/// This is deliberately DERIVED from the keyframe interval rather than a
/// parallel constant: the property that matters is that a
/// [`LossContract::RejoinAtRandomAccess`] subscriber's retained window
/// always contains at least one full GOP, so there is always a
/// random-access point to rejoin at after an overflow. A parallel capacity
/// constant would make that property merely conventional — breakable the
/// moment someone changes the keyframe interval and forgets a capacity
/// constant had to move in step. Deriving it makes the property
/// structural instead.
///
/// An explicit [`SubscribeOptions::capacity`] override always wins over
/// this derivation and is unaffected by later keyframe-interval changes.
pub fn automatic_subscriber_capacity(effective_keyframe_interval_frames: u32) -> NonZeroUsize {
    let frames = (effective_keyframe_interval_frames as usize).max(AUTOMATIC_CAPACITY_FLOOR_FRAMES);
    NonZeroUsize::new(frames).expect("the floor is 15, so this is always non-zero")
}

/// What a subscriber declares when it joins the hub.
#[derive(Debug, Clone, Copy)]
pub struct SubscribeOptions {
    pub loss: LossContract,
    /// `None` means the automatic default applies: the capacity derived
    /// from the hub's effective keyframe interval by
    /// [`automatic_subscriber_capacity`]. `Some` is an explicit override
    /// that wins over the derived value and does not move when the
    /// keyframe interval later changes.
    ///
    /// A capacity of one access unit is legitimate — a "latest keyframe
    /// only" consumer — because codec configuration and fault events sit
    /// OUTSIDE this budget (see [`TrackEvent`]): they are never droppable
    /// and never counted against it, so a one-unit subscriber still
    /// receives its codec config and its keyframe on join.
    pub capacity: Option<NonZeroUsize>,
}

/// Why a new stream epoch began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochReason {
    Reconnect,
    FormatChange,
    Discontinuity,
}

/// A named, classified gap in a `FaultOnGap` subscription's delivered
/// sequence — never a silent drop. Names the affected camera and interval,
/// per the governing contract that a recording/analysis overrun becomes an
/// explicit coverage reduction with the affected camera and interval: a
/// real consumer receiving a fault must be able to tell which camera lost
/// coverage without cross-referencing which hub instance delivered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageFault {
    pub stream_id: StreamId,
    /// The camera identity this hub was constructed for (see
    /// [`CameraTrackHub::camera`]) — bound once at construction, never
    /// inferred from the lost unit.
    pub camera: CameraId,
    pub epoch: u64,
    pub lost_sequence_range: RangeInclusive<u64>,
    pub reason: String,
}

/// One event delivered to a subscription.
///
/// [`TrackEvent::CodecConfig`] and [`TrackEvent::Fault`] are NEVER counted
/// against [`SubscribeOptions::capacity`] — only [`TrackEvent::AccessUnit`]
/// consumes a slot in the bounded ring. This is what makes a capacity of
/// one legitimate: a join always delivers codec configuration, THEN the
/// first post-join keyframe, and the codec-configuration delivery never
/// competes with that keyframe for the single access-unit slot.
#[derive(Debug, Clone)]
pub enum TrackEvent {
    /// Codec configuration bytes, delivered before the first access unit a
    /// subscriber can decode. Outside the capacity budget.
    CodecConfig(Bytes),
    /// One encoded access unit, shared by `Arc` with every other
    /// subscriber that received the same publish — never copied per
    /// consumer. The one event kind that consumes a capacity slot.
    AccessUnit(Arc<EncodedAccessUnit>),
    /// Terminal: this subscription could not keep pace and was detached.
    /// Outside the capacity budget.
    Fault(CoverageFault),
}

/// Why a subscription's event stream ended — named so a consumer learns
/// the reason from the stream itself, never by inferring it from silence.
///
/// [`SubscriptionEnd::SourceFault`] is deliberately NOT raised for a
/// transient fault a retry can recover from: supervision may retry a
/// camera many times, and each retry opens a new epoch (via
/// [`CameraTrackHubProducer::begin_epoch`]) that a live subscription must
/// survive, continuing to deliver into it exactly as
/// [`Subscription::drain`] already does across an epoch bump. This
/// variant is raised ONLY via [`CameraTrackHubProducer::abandon_source`] —
/// the explicit signal that supervision has abandoned the source and it
/// will never come back. A design that treated the first fault as
/// terminal would tear down every consumer on an ordinary camera blip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionEnd {
    /// The source closed normally: no further access unit will ever be
    /// published, on this epoch or any future one.
    SourceEnded,
    /// Supervision abandoned the source after exhausting retries — the
    /// source will never come back. Never raised for a transient fault
    /// that goes on to reconnect; see the type-level doc above.
    SourceFault { reason: SourceFaultReason },
    /// The hub itself is going away, independent of anything about the
    /// source's own lifecycle.
    HubShutdown,
}

/// The adapter-supplied text passed to
/// [`CameraTrackHubProducer::abandon_source`], held so that it STRUCTURALLY
/// cannot reach any string a consumer might log. The caller contract
/// accepts arbitrary adapter error text — which can carry a credential (an
/// RTSP URL with an embedded password is the case this guards) — so the
/// fix is a type that never exposes that text through `Debug`, `Display`,
/// or any other rendering path, rather than a redaction convention every
/// future call site (and every future `derive(Debug)` on something that
/// contains it) would have to remember to apply.
///
/// Still comparable and clonable: equality is the ordinary Rust `==` any
/// caller expects when checking "which reason did I get" (see the
/// `abandon_source_is_terminal_carries_the_reason_and_the_first_raised_terminal_wins`
/// test, which compares a `SourceFaultReason` directly against a `&str`
/// literal) — comparing two values for equality never prints either one.
#[derive(Clone, Eq)]
pub struct SourceFaultReason(String);

impl SourceFaultReason {
    fn new(reason: String) -> Self {
        Self(reason)
    }
}

impl std::fmt::Debug for SourceFaultReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SourceFaultReason")
            .field(&"<redacted>")
            .finish()
    }
}

impl PartialEq for SourceFaultReason {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl PartialEq<str> for SourceFaultReason {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for SourceFaultReason {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// One item yielded by [`Subscription::recv`]: either the next buffered
/// track event, or (exactly once, terminal) the reason this
/// subscription's stream ended.
#[derive(Debug, Clone)]
pub enum SubscriptionEvent {
    /// The next event, in the same order [`Subscription::drain`] would
    /// have returned it.
    Track(TrackEvent),
    /// The terminal reason this subscription's stream ended. Delivered
    /// only after every [`TrackEvent`] already buffered for this
    /// subscription at the moment the terminal condition was raised —
    /// a terminal never jumps the queue and discards data this
    /// subscription was entitled to.
    End(SubscriptionEnd),
}

/// Stable handle to one subscription, returned by
/// [`CameraTrackHub::subscribe`] and accepted by [`CameraTrackHub::detach`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

/// Why [`CameraTrackHub::publish`] refused a unit. Every field the hub
/// checks before accepting a unit gets its own variant — a wrong stream,
/// epoch, camera, source role, or codec is refused exactly like a
/// non-monotonic sequence, never silently accepted into the wrong
/// stream's fan-out. Every one of these is checked against identity the
/// hub was told at CONSTRUCTION (see [`CameraTrackHub::new`]), never
/// inferred from whatever the first published unit happens to claim — a
/// hub that adopted its identity from the first publish could not
/// meaningfully reject a "wrong" one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// The unit's sequence does not continue the current epoch's monotonic
    /// order.
    SequenceNotMonotonic { expected_at_least: u64, got: u64 },
    /// The unit names a different stream than this hub was created for.
    WrongStream { expected: StreamId, got: StreamId },
    /// The unit's epoch does not match the hub's current epoch.
    WrongEpoch { expected: u64, got: u64 },
    /// The unit names a different camera than this hub was constructed for.
    WrongCamera { expected: CameraId, got: CameraId },
    /// The unit's source role does not match what this hub was
    /// constructed for.
    WrongSourceRole {
        expected: SourceRole,
        got: SourceRole,
    },
    /// The unit's codec does not match what this hub was constructed for.
    WrongCodec {
        expected: VideoCodec,
        got: VideoCodec,
    },
    /// This producer action was refused because a terminal has already been
    /// raised for this hub — via [`CameraTrackHubProducer::end_source`],
    /// [`CameraTrackHubProducer::abandon_source`], or
    /// [`CameraTrackHub::shutdown`]. Once any one of those fires, the FIRST
    /// raised terminal is definitive: no unit may ever be published again
    /// and no further epoch may ever begin, on this producer, forever —
    /// never silently accepted into a stream every subscriber has already
    /// learned (or will learn on join) is over.
    ProducerEnded(SubscriptionEnd),
}

/// A live subscription over the hub. Reading is non-blocking and
/// synchronous: [`Subscription::drain`] returns whatever this subscription
/// has buffered so far, never sleeping or waiting on a wall clock — join
/// and fault health are stated entirely in terms of which `TrackEvent`s
/// have arrived, in stream terms only.
///
/// Dropping a `Subscription` must detach it from the hub — an owner that
/// stops holding a subscription must not leak an entry in the hub's
/// subscriber table forever.
pub struct Subscription {
    id: SubscriptionId,
    hub: Arc<CameraTrackHubInner>,
}

impl Subscription {
    pub fn id(&self) -> SubscriptionId {
        self.id
    }

    /// Every event buffered for this subscription since the last drain,
    /// oldest first: pending codec configuration (if any), then buffered
    /// access units in delivery order, then a terminal fault (if any).
    pub fn drain(&self) -> Vec<TrackEvent> {
        let mut state = self
            .hub
            .state
            .lock()
            .expect("camera track hub state poisoned");
        let Some(subscriber) = state.subscribers.get_mut(&self.id) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if let Some(codec_config) = subscriber.pending_codec_config.take() {
            events.push(TrackEvent::CodecConfig(codec_config));
        }
        events.extend(subscriber.queue.drain(..).map(TrackEvent::AccessUnit));
        if let Some(fault) = subscriber.pending_fault.take() {
            events.push(TrackEvent::Fault(fault));
        }
        events
    }

    /// How many events are currently buffered for this subscription,
    /// without draining them — the bounded-queue-only proof point. Counts
    /// only `TrackEvent::AccessUnit` entries, per `SubscribeOptions::capacity`'s
    /// own contract; `CodecConfig`/`Fault` are outside the budget.
    pub fn buffered_len(&self) -> usize {
        let state = self
            .hub
            .state
            .lock()
            .expect("camera track hub state poisoned");
        state
            .subscribers
            .get(&self.id)
            .map(|subscriber| subscriber.queue.len())
            .unwrap_or(0)
    }

    /// Whether the hub has detached this subscription (after a
    /// `FaultOnGap` overflow, or an explicit [`CameraTrackHub::detach`]).
    pub fn is_detached(&self) -> bool {
        let state = self
            .hub
            .state
            .lock()
            .expect("camera track hub state poisoned");
        match state.subscribers.get(&self.id) {
            Some(subscriber) => subscriber.detached,
            None => true,
        }
    }

    /// Await the next event for this subscription — the async counterpart
    /// to [`Subscription::drain`], for a consumer that wants to await
    /// rather than busy-poll.
    ///
    /// Ordering: every [`TrackEvent`] already buffered for this
    /// subscription is delivered, oldest first, exactly as
    /// [`Subscription::drain`] would order it, BEFORE its terminal
    /// [`SubscriptionEvent::End`] — a terminal never jumps the queue.
    ///
    /// Exactly-once terminal: `SubscriptionEvent::End` is yielded at most
    /// once for this subscription. Every call after that resolves
    /// immediately (never `Poll::Pending`) to `None` — never repeating
    /// the terminal, never resurrecting the subscription with new data.
    ///
    /// A transient fault followed by a producer reconnect
    /// (`CameraTrackHubProducer::begin_epoch`) is NOT terminal on its
    /// own: this method keeps yielding `Some(SubscriptionEvent::Track(..))`
    /// into the new epoch. Only an explicit
    /// `CameraTrackHubProducer::abandon_source` (source fault),
    /// `CameraTrackHubProducer::end_source` (source ended), or
    /// `CameraTrackHub::shutdown` (hub shutdown) ever yields
    /// `SubscriptionEvent::End`.
    ///
    /// Takes `&mut self`, not `&self`: the honest contract is ONE receiver
    /// per subscription, never many. A shared `&self` would let two
    /// concurrent `recv` callers race for the same single stored
    /// [`SubscriberState::waker`] — whichever call polled second would
    /// silently clobber the first's registration, leaving the first
    /// permanently un-wakeable. `&mut self` makes a second concurrent call
    /// a compile error instead, so there is exactly one place a waker is
    /// ever registered and exactly one future the hub is obligated to wake.
    /// No consumer needs multiple simultaneous waiters on one subscription
    /// today, so this is enforced at the type level rather than built out
    /// as multi-waiter support beneath a shared reference:
    ///
    /// ```compile_fail
    /// # async fn go() {
    /// use vigil::VideoCodec;
    /// use vigil::camera_hub::{CameraTrackHub, LossContract, SubscribeOptions};
    /// use vigil::camera_track::{CameraId, SourceRole};
    /// use vigil::workgraph::StreamId;
    ///
    /// let (_producer, hub) = CameraTrackHub::new(
    ///     StreamId::new("driveway"),
    ///     CameraId::from_usb("driveway-node", "driveway-durable-id").expect("literal is durable"),
    ///     SourceRole::Analysis,
    ///     VideoCodec::H264,
    ///     30,
    /// );
    /// let mut subscription = hub.subscribe(SubscribeOptions {
    ///     loss: LossContract::FaultOnGap,
    ///     capacity: None,
    /// });
    /// let first = subscription.recv();
    /// let second = subscription.recv(); // second `&mut` borrow while `first` is still live
    /// let _ = (first, second);
    /// # }
    /// ```
    pub async fn recv(&mut self) -> Option<SubscriptionEvent> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// The actual poll logic behind [`Subscription::recv`], split out so the
    /// public method stays a one-line `poll_fn` wrapper. Locks the shared
    /// hub state once per poll, exactly like every other `Subscription`
    /// method.
    fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<SubscriptionEvent>> {
        let mut state = self
            .hub
            .state
            .lock()
            .expect("camera track hub state poisoned");
        let Some(subscriber) = state.subscribers.get_mut(&self.id) else {
            // Detached (explicitly, or via `Drop`): behaves exactly like a
            // subscription whose terminal has already been delivered.
            return Poll::Ready(None);
        };

        // Same ordering as `drain`: pending codec configuration, then
        // buffered access units oldest-first, then a subscriber-capacity
        // fault — every already-buffered `TrackEvent` before anything
        // terminal.
        if let Some(codec_config) = subscriber.pending_codec_config.take() {
            return Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::CodecConfig(
                codec_config,
            ))));
        }
        if let Some(access_unit) = subscriber.queue.pop_front() {
            return Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::AccessUnit(
                access_unit,
            ))));
        }
        if let Some(fault) = subscriber.pending_fault.take() {
            return Poll::Ready(Some(SubscriptionEvent::Track(TrackEvent::Fault(fault))));
        }

        if subscriber.end_delivered {
            // Exactly-once terminal: every call after it was yielded
            // resolves immediately, never pending, never repeating.
            return Poll::Ready(None);
        }
        if let Some(end) = subscriber.pending_end.take() {
            subscriber.end_delivered = true;
            return Poll::Ready(Some(SubscriptionEvent::End(end)));
        }
        if subscriber.detached {
            // Terminal under `FaultOnGap`: the coverage fault itself was
            // already delivered above (as a `TrackEvent::Fault`, ahead of
            // any terminal, same as `drain`'s ordering) and this
            // subscription carries no separate `SubscriptionEnd` for it —
            // a detached subscriber must still stop parking forever exactly
            // like every other terminal path, never fabricate a further
            // event. Mark the terminal delivered here too: without this, a
            // hub-wide terminal raised later would find `pending_end` still
            // `None` and `end_delivered` still `false` on this already-over
            // subscription and hand it a brand-new terminal to yield next —
            // a second terminal for a subscription that already reported
            // itself finished.
            subscriber.end_delivered = true;
            return Poll::Ready(None);
        }

        // Nothing available yet: store the waker so a later publish, epoch
        // bump, or terminal can wake this exact pending call.
        subscriber.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut state = self
            .hub
            .state
            .lock()
            .expect("camera track hub state poisoned");
        state.subscribers.remove(&self.id);
    }
}

/// One subscriber's ring plus everything the hub needs to keep its declared
/// contract independent of every other subscriber.
struct SubscriberState {
    loss: LossContract,
    capacity: NonZeroUsize,
    /// The bounded ring of access units, never longer than `capacity`.
    queue: VecDeque<Arc<EncodedAccessUnit>>,
    /// Undelivered codec configuration, outside the capacity budget and
    /// coalesced to the latest value rather than accumulated.
    pending_codec_config: Option<Bytes>,
    /// The terminal fault, set once and delivered on the next drain.
    pending_fault: Option<CoverageFault>,
    /// Set once this subscription can never receive anything again (after
    /// a `FaultOnGap` overflow). An explicit detach instead removes the
    /// subscriber's entry entirely.
    detached: bool,
    /// True from join, from an epoch bump, and from a
    /// `RejoinAtRandomAccess` overflow until the next keyframe lands: every
    /// delta arriving while this is true is discarded without consuming a
    /// capacity slot.
    awaiting_random_access: bool,
    /// This subscription's OWN copy of the current epoch's marker, held
    /// until THIS subscription has actually delivered a keyframe of that
    /// epoch — never a hub-global one-shot. Set from
    /// `HubState::current_epoch_marker` at join and at every epoch bump;
    /// taken (consumed) the moment this subscription delivers its own
    /// first access unit of the epoch, which — because delivery is gated
    /// by `awaiting_random_access` — is always that subscription's first
    /// delivered keyframe of the epoch, even when the epoch was actually
    /// opened by a delta this subscription never received.
    pending_epoch_marker: Option<PendingEpochMarker>,
    /// The terminal reason this subscription's stream ended, once raised by
    /// [`CameraTrackHubProducer::end_source`],
    /// [`CameraTrackHubProducer::abandon_source`], or
    /// [`CameraTrackHub::shutdown`] — set at most once (the first raised
    /// terminal wins) and taken by [`Subscription::recv`] only after every
    /// already-buffered [`TrackEvent`] has been delivered.
    pending_end: Option<SubscriptionEnd>,
    /// Whether [`SubscriptionEvent::End`] has already been yielded by
    /// [`Subscription::recv`] for this subscription — once true, every
    /// later `recv` resolves immediately to `None`, never pending, never
    /// repeating the terminal.
    end_delivered: bool,
    /// The waker of a currently-pending [`Subscription::recv`] call, if
    /// any. Stored on the one path where `recv` finds nothing available and
    /// taken (and woken) on every path that makes a new event available:
    /// [`CameraTrackHubProducer::publish`] (via [`deliver_to_subscriber`]),
    /// [`CameraTrackHubProducer::begin_epoch`]'s per-subscriber codec
    /// configuration, and every terminal raise.
    waker: Option<Waker>,
}

/// Take a subscription's pending [`Subscription::recv`] waker, if one is
/// currently registered, WITHOUT invoking it — the single place every
/// event-producing path (publish, epoch bump, terminal raise, detach)
/// routes through, so a missed wake is a bug in one function rather than a
/// risk repeated at every call site.
///
/// Deliberately does NOT call `.wake()` itself: every caller collects the
/// returned waker into a batch and invokes it only once the hub's lock has
/// been released (see [`wake_all`]). A waker is caller-supplied code — a
/// real async consumer may have it re-enter the hub (poll a `Subscription`,
/// call `detach`, drop a handle) the instant it runs, and invoking it while
/// still holding the hub's `Mutex` self-deadlocks that reentrant call on
/// the recursive re-lock.
fn take_waker(subscriber: &mut SubscriberState) -> Option<Waker> {
    subscriber.waker.take()
}

/// Wake every collected waker. Callers must only invoke this AFTER the
/// hub's lock has been released — see [`take_waker`] for why.
fn wake_all(wakers: Vec<Waker>) {
    for waker in wakers {
        waker.wake();
    }
}

/// Raise a terminal for one subscriber, honoring "the first raised terminal
/// wins": a terminal already pending, already delivered, OR already implied
/// by `detached` (a `FaultOnGap` overflow — its own coverage fault IS this
/// subscriber's one and only terminal, even though it never populates
/// `pending_end`/`end_delivered` itself; see `Subscription::poll_recv`'s
/// `detached` arm) is left alone — `make_end` is called (and any work it
/// does, such as cloning a fault reason, is paid) only when this subscriber
/// genuinely still owes a terminal. Without the `detached` check, a
/// hub-wide terminal raised in the window between that overflow and the
/// subscriber's own next poll would hand it a brand-new `pending_end`,
/// producing a second terminal for a subscription that was already over.
/// Collects a pending [`Subscription::recv`] waker into `wakers` exactly
/// when a new terminal was actually raised; the caller wakes it only after
/// releasing the hub's lock.
fn raise_terminal(
    subscriber: &mut SubscriberState,
    make_end: impl FnOnce() -> SubscriptionEnd,
    wakers: &mut Vec<Waker>,
) {
    if subscriber.detached || subscriber.pending_end.is_some() || subscriber.end_delivered {
        return;
    }
    subscriber.pending_end = Some(make_end());
    wakers.extend(take_waker(subscriber));
}

/// Raise the HUB-WIDE terminal — the first-wins record every later
/// `subscribe`/`publish`/`begin_epoch` consults — and propagate it to every
/// currently attached subscriber via `raise_terminal`. Living on the hub
/// (not only on each attached subscriber) is what makes a late join and a
/// post-terminal publish/epoch-bump structurally impossible rather than
/// merely handled: see [`HubState::terminal`]. Collects every woken
/// subscriber's waker into `wakers`, to be invoked by the caller only after
/// the hub's lock is released.
fn raise_hub_terminal(
    state: &mut HubState,
    make_end: impl FnOnce() -> SubscriptionEnd,
    wakers: &mut Vec<Waker>,
) {
    if state.terminal.is_some() {
        return;
    }
    let end = make_end();
    state.terminal = Some(end.clone());
    for subscriber in state.subscribers.values_mut() {
        raise_terminal(subscriber, || end.clone(), wakers);
    }
}

/// What the CURRENT epoch's first-delivered-per-subscription unit must be
/// marked with. Unlike a stream-wide one-shot, this describes the epoch
/// itself: it is handed out (cloned) to every subscriber's own
/// [`SubscriberState::pending_epoch_marker`] — at the epoch bump for every
/// already-attached subscriber, and at join time for any subscriber that
/// attaches later while this epoch is still current — so each subscription
/// applies and consumes its OWN copy on its OWN first delivered keyframe of
/// this epoch, independent of what any other subscription has already
/// resolved.
#[derive(Clone, Copy)]
struct PendingEpochMarker {
    format_change: bool,
}

struct HubState {
    epoch: u64,
    next_expected_sequence: u64,
    /// The current epoch's own retained codec configuration, handed to
    /// every joiner/rejoiner instead of whatever a later keyframe happens
    /// to carry.
    retained_codec_config: Option<Bytes>,
    /// The marker describing why the CURRENT epoch began, or `None` for an
    /// epoch that was never opened by `begin_epoch` (the hub's very first
    /// epoch). Seeds `SubscriberState::pending_epoch_marker` for every
    /// subscriber attached at the bump and every subscriber that joins
    /// later during this same epoch; it is never itself consumed — only
    /// each subscriber's own copy is.
    current_epoch_marker: Option<PendingEpochMarker>,
    subscribers: HashMap<SubscriptionId, SubscriberState>,
    next_subscription_id: u64,
    /// Consumed (dirty-flag) signal: a join has requested a producer
    /// keyframe since this was last checked.
    keyframe_requested: bool,
    /// The hub-wide terminal, once any one of `end_source`/`abandon_source`/
    /// `shutdown` has raised it — set at most once (the first raised
    /// terminal wins, matched by `raise_terminal`'s per-subscriber
    /// equivalent). Living HERE, on the hub itself rather than only on each
    /// already-attached `SubscriberState`, is what makes both the late-join
    /// case and the post-terminal-publish case structurally impossible
    /// rather than merely handled: `subscribe` seeds a new subscriber's
    /// `pending_end` from this field, so a subscriber joining after the
    /// terminal inherits it exactly as if it had been attached when the
    /// terminal was raised; `publish`/`begin_epoch` refuse outright the
    /// moment this is set, so there is no way to reach the per-subscriber
    /// delivery loop after a terminal at all.
    terminal: Option<SubscriptionEnd>,
}

struct CameraTrackHubInner {
    stream_id: StreamId,
    camera: CameraId,
    source_role: SourceRole,
    codec: VideoCodec,
    state: Mutex<HubState>,
}

/// The exclusive producer handle for one camera track: only the holder of
/// this type may [`publish`](CameraTrackHubProducer::publish) or
/// [`begin_epoch`](CameraTrackHubProducer::begin_epoch). It is deliberately
/// NOT `Clone`, and both methods take `&mut self` — exclusivity comes from
/// the mutable borrow, not merely from the absence of `Clone`: a
/// non-`Clone` type reached through `Arc<Self>` and `&self` methods would
/// still allow two holders to publish concurrently. There is no method on
/// [`CameraTrackHub`] (the consumer side) that hands one of these back, and
/// no second constructor that yields one — the only way to obtain a
/// producer is [`CameraTrackHub::new`], once, at construction.
///
/// This is a pure relocation: `publish`/`begin_epoch` carry the identical
/// logic that lived on the pre-split `CameraTrackHub` (only the receiver —
/// `&self` to `&mut self` — and the enclosing type changed), because
/// nothing about the split calls for new behavior, only for the same
/// behavior to be reachable exclusively.
///
/// All three structural guarantees above are ASSERTED, not merely stated
/// in prose — each has a `compile_fail` doctest below, following the
/// `SettingHandle`/`AutomationHandle` precedent in `crate::settings`, plus
/// a lexical backstop in `camera_hub_capability_scan.rs` for the two that
/// a future edit could reopen without any doctest noticing (a doctest only
/// ever proves today's shape fails to compile; it cannot re-run itself
/// against tomorrow's source the way a checked-in scan can).
///
/// **1. Not `Clone`** — cloning a producer does not compile:
///
/// ```compile_fail
/// use vigil::VideoCodec;
/// use vigil::camera_hub::CameraTrackHub;
/// use vigil::camera_track::{CameraId, SourceRole};
/// use vigil::workgraph::StreamId;
///
/// let (producer, _consumer) = CameraTrackHub::new(
///     StreamId::new("driveway"),
///     CameraId::from_usb("driveway-node", "driveway-durable-id").expect("literal is durable"),
///     SourceRole::Analysis,
///     VideoCodec::H264,
///     30,
/// );
/// let _also_producer = producer.clone();
/// ```
///
/// **2. No second constructor** — every field is private, so the only
/// public path to a producer is [`CameraTrackHub::new`]; constructing one
/// directly does not compile:
///
/// ```compile_fail
/// use vigil::camera_hub::CameraTrackHubProducer;
///
/// let _smuggled = CameraTrackHubProducer { inner: todo!() };
/// ```
///
/// **3. `&mut self` is the actual exclusivity mechanism, not `!Clone`
/// alone** — the load-bearing guarantee. `!Clone` on its own would still
/// let two holders publish concurrently through a shared `Arc<Producer>`
/// if `publish` took `&self`: `Arc<T>` hands out `&T` (via `Deref`) but
/// never `&mut T` (it implements no `DerefMut`), so a method that
/// genuinely requires `&mut self` cannot be called through an
/// `Arc<CameraTrackHubProducer>` at all, compiled or not:
///
/// ```compile_fail
/// use std::sync::Arc;
/// use vigil::VideoCodec;
/// use vigil::camera_hub::CameraTrackHub;
/// use vigil::camera_track::{CameraId, SourceRole};
/// use vigil::workgraph::StreamId;
///
/// let (producer, _consumer) = CameraTrackHub::new(
///     StreamId::new("driveway"),
///     CameraId::from_usb("driveway-node", "driveway-durable-id").expect("literal is durable"),
///     SourceRole::Analysis,
///     VideoCodec::H264,
///     30,
/// );
/// let shared = Arc::new(producer);
/// let _ = shared.publish(todo!());
/// ```
pub struct CameraTrackHubProducer {
    inner: Arc<CameraTrackHubInner>,
}

impl CameraTrackHubProducer {
    /// Publish one unit to every current subscriber. Rejects a unit whose
    /// stream, epoch, camera, source role, codec, or sequence does not
    /// match this hub's BOUND identity/format (set once at
    /// [`CameraTrackHub::new`]) instead of silently accepting a corrupted
    /// or cross-stream unit — or silently adopting a different identity
    /// from whatever unit happens to arrive first.
    pub fn publish(&mut self, unit: EncodedAccessUnit) -> Result<(), PublishError> {
        if unit.stream_id != self.inner.stream_id {
            return Err(PublishError::WrongStream {
                expected: self.inner.stream_id.clone(),
                got: unit.stream_id,
            });
        }
        if unit.camera != self.inner.camera {
            return Err(PublishError::WrongCamera {
                expected: self.inner.camera.clone(),
                got: unit.camera,
            });
        }
        if unit.source_role != self.inner.source_role {
            return Err(PublishError::WrongSourceRole {
                expected: self.inner.source_role,
                got: unit.source_role,
            });
        }
        if unit.codec != self.inner.codec {
            return Err(PublishError::WrongCodec {
                expected: self.inner.codec,
                got: unit.codec,
            });
        }

        let mut state = self
            .inner
            .state
            .lock()
            .expect("camera track hub state poisoned");

        // A terminal already raised (source ended, source abandoned, or hub
        // shutdown) refuses every further publish, on this producer,
        // forever: reading `state.terminal` here — the same hub-wide field
        // `subscribe` seeds a late joiner from — makes this refusal and the
        // late-join case two views of one fact rather than two independent
        // checks that could drift apart.
        if let Some(terminal) = &state.terminal {
            return Err(PublishError::ProducerEnded(terminal.clone()));
        }

        if unit.stream_epoch != state.epoch {
            return Err(PublishError::WrongEpoch {
                expected: state.epoch,
                got: unit.stream_epoch,
            });
        }
        if unit.sequence != state.next_expected_sequence {
            return Err(PublishError::SequenceNotMonotonic {
                expected_at_least: state.next_expected_sequence,
                got: unit.sequence,
            });
        }
        state.next_expected_sequence = unit.sequence + 1;

        // The hub, not the producer, is authoritative on why an epoch
        // began — but WHICH unit that discontinuity/format-change marker
        // actually lands on is decided per subscription, not here: a
        // subscriber awaiting random access may discard this very unit (if
        // it is a delta) and only see the marker on its own later, first
        // ACTUALLY DELIVERED keyframe. See `deliver_to_subscriber`.

        // Collected across every subscriber this publish wakes (the codec
        // bootstrap below and the per-subscriber delivery loop) and only
        // invoked once the hub's lock is released at the end of this
        // method — see `take_waker` for why waking under the lock is
        // unsafe.
        let mut wakers = Vec::new();

        // Bootstrap the retained codec configuration for an epoch that was
        // never opened by `begin_epoch` (the hub's very first epoch): take
        // it from the epoch's own opening unit, and hand it to every
        // subscriber already waiting on it — never overwritten by a later
        // unit's codec_config once established.
        if state.retained_codec_config.is_none()
            && let Some(codec_config) = unit.codec_config.clone()
        {
            state.retained_codec_config = Some(codec_config.clone());
            for subscriber in state.subscribers.values_mut() {
                if !subscriber.detached {
                    subscriber.pending_codec_config = Some(codec_config.clone());
                    wakers.extend(take_waker(subscriber));
                }
            }
        }

        let retained_codec_config = state.retained_codec_config.clone();
        let stream_id = self.inner.stream_id.clone();
        let camera = self.inner.camera.clone();
        let epoch = state.epoch;
        let shared_unit = Arc::new(unit);

        for subscriber in state.subscribers.values_mut() {
            deliver_to_subscriber(
                subscriber,
                &shared_unit,
                &retained_codec_config,
                &stream_id,
                &camera,
                epoch,
                &mut wakers,
            );
        }

        // Release the hub's lock BEFORE invoking any collected waker: a
        // waker that re-enters the hub (a real async consumer's `Waker` can
        // trivially do this) must find the lock free, not self-deadlocked
        // on a recursive re-lock.
        drop(state);
        wake_all(wakers);

        Ok(())
    }

    /// Begin a new stream epoch (reconnect, format change, or a
    /// discontinuity the producer detected) under the SAME stream/camera
    /// identity. Returns the new epoch number.
    ///
    /// The delivered form of the new epoch's FIRST published access unit
    /// must be marked by the reason `begin_epoch` was called for — never
    /// left to whatever the producer happened to set on the unit itself:
    /// `discontinuity` true for every reason (every epoch bump is a
    /// timeline break by definition), and `format_change` true
    /// additionally when `reason` is [`EpochReason::FormatChange`]. This
    /// is enforced on the DELIVERED copy, not merely trusted from the
    /// producer, because the producer's own bookkeeping is exactly the
    /// kind of thing that can drift; the hub is the single place that
    /// already knows why the epoch began.
    ///
    /// Repeated `begin_epoch` calls with no [`Subscription::drain`] in
    /// between must not accumulate one `TrackEvent::CodecConfig` per call
    /// forever: codec configuration sits OUTSIDE the bounded
    /// [`SubscribeOptions::capacity`] ring precisely because it is never
    /// droppable, which makes it the one event class a naive
    /// accumulate-everything implementation could grow without bound —
    /// pending, undelivered codec configuration must be bounded or
    /// coalesced to the latest value instead.
    ///
    /// Refused with [`PublishError::ProducerEnded`] once a terminal has
    /// already been raised (source ended, source abandoned, or hub
    /// shutdown): a new epoch can never reopen a stream every subscriber
    /// has already learned (or will learn on join) is over.
    pub fn begin_epoch(
        &mut self,
        reason: EpochReason,
        codec_config: Bytes,
    ) -> Result<u64, PublishError> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("camera track hub state poisoned");

        // Same hub-wide terminal check as `publish` above — a new epoch can
        // never reopen a stream every subscriber has already learned (or
        // will learn on join) is over.
        if let Some(terminal) = &state.terminal {
            return Err(PublishError::ProducerEnded(terminal.clone()));
        }

        state.epoch += 1;
        state.next_expected_sequence = 0;
        state.retained_codec_config = Some(codec_config.clone());
        let marker = PendingEpochMarker {
            format_change: matches!(reason, EpochReason::FormatChange),
        };
        state.current_epoch_marker = Some(marker);

        // Collected across every woken subscriber and only invoked once the
        // hub's lock is released below — see `take_waker` for why waking
        // under the lock is unsafe.
        let mut wakers = Vec::new();

        for subscriber in state.subscribers.values_mut() {
            if subscriber.detached {
                continue;
            }
            // Drop the prior epoch's retained data and re-arm: the next
            // decodable unit this subscriber may accept is the new epoch's
            // codec configuration followed by its first keyframe.
            subscriber.queue.clear();
            subscriber.awaiting_random_access = true;
            // Coalesce to the latest pending value rather than accumulate
            // one entry per repeated, undrained `begin_epoch` call.
            subscriber.pending_codec_config = Some(codec_config.clone());
            // Hand this subscription its OWN copy of the new epoch's
            // marker, replacing (never accumulating) whatever it was still
            // holding from a prior, superseded epoch bump — mirrors the
            // codec-config coalescing above.
            subscriber.pending_epoch_marker = Some(marker);
            wakers.extend(take_waker(subscriber));
        }

        let new_epoch = state.epoch;

        // Release the hub's lock BEFORE invoking any collected waker — same
        // reason as `publish` above.
        drop(state);
        wake_all(wakers);

        Ok(new_epoch)
    }

    /// Whether a subscriber join has requested a keyframe since the last
    /// time this was checked (consuming the signal, like a dirty flag) —
    /// this producer's own encoder loop polls this and calls
    /// `CameraEncoder::request_keyframe` when it reports true. This lives
    /// on the PRODUCER, not the cloneable consumer handle
    /// [`CameraTrackHub`]: the producer is the single owner that actually
    /// acts on the request (by asking its encoder for a keyframe), so it is
    /// the single owner allowed to consume the signal. A check-and-clear on
    /// the cloneable consumer side would let any clone — including one that
    /// never asked for a keyframe on anyone's behalf — steal the signal a
    /// join or the producer itself was entitled to see; requesting stays
    /// subscriber-side (`CameraTrackHub::subscribe` sets the flag, since a
    /// join is what creates the intent), but checking-and-clearing it moves
    /// here, to the one handle that owns acting on it. This is the
    /// observable proof that "a join deterministically requests a
    /// keyframe" is a real signal the hub emits, not merely an assumption a
    /// test cannot see.
    pub fn keyframe_requested_since_last_check(&self) -> bool {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("camera track hub state poisoned");
        std::mem::take(&mut state.keyframe_requested)
    }

    /// The source closed normally. Raises [`SubscriptionEnd::SourceEnded`]
    /// as the terminal for every currently attached subscription, after
    /// any [`TrackEvent`]s already buffered for each one.
    ///
    /// Idempotent: calling this more than once, or calling it after
    /// [`CameraTrackHubProducer::abandon_source`] already raised a
    /// terminal, must not re-raise or overwrite an already-raised
    /// terminal — the first terminal a subscription is owed wins.
    ///
    /// Also raised on [`HubState::terminal`], the hub-wide record: a
    /// subscriber that joins after this call inherits the terminal (see
    /// [`CameraTrackHub::subscribe`]) and a further
    /// [`CameraTrackHubProducer::publish`] / [`CameraTrackHubProducer::begin_epoch`]
    /// is refused.
    pub fn end_source(&mut self) {
        let mut wakers = Vec::new();
        {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("camera track hub state poisoned");
            raise_hub_terminal(&mut state, || SubscriptionEnd::SourceEnded, &mut wakers);
        }
        // The lock above is released (the guard went out of scope at the
        // end of the block) before any collected waker runs — see
        // `take_waker` for why waking under the lock is unsafe.
        wake_all(wakers);
    }

    /// Supervision has abandoned the source after exhausting retries — the
    /// source will never come back. Raises
    /// [`SubscriptionEnd::SourceFault`] with `reason` as the terminal for
    /// every currently attached subscription, after any [`TrackEvent`]s
    /// already buffered for each one.
    ///
    /// This must NEVER be confused with a transient fault that goes on to
    /// reconnect via [`CameraTrackHubProducer::begin_epoch`]: a plain
    /// epoch bump never calls this and never raises a terminal on its
    /// own — see [`SubscriptionEnd`]'s own doc comment. Idempotent like
    /// [`CameraTrackHubProducer::end_source`]: the first raised terminal
    /// wins.
    ///
    /// Also raised on [`HubState::terminal`] — see
    /// [`CameraTrackHubProducer::end_source`]'s own note.
    pub fn abandon_source(&mut self, reason: String) {
        let mut wakers = Vec::new();
        {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("camera track hub state poisoned");
            raise_hub_terminal(
                &mut state,
                || SubscriptionEnd::SourceFault {
                    reason: SourceFaultReason::new(reason),
                },
                &mut wakers,
            );
        }
        // Same reason as `end_source` above: wake only after the lock is
        // released.
        wake_all(wakers);
    }
}

/// The fan-out point for one camera's encoded track: the CONSUMER side.
/// Any number of consumers subscribe with their own declared loss contract
/// and capacity. Cheaply `Clone` (an `Arc` clone) — every clone is a shared
/// view onto the SAME hub state, never an independent fork: a publish or
/// `begin_epoch` made through the [`CameraTrackHubProducer`] returned
/// alongside a hub is visible through every clone, whether the clone was
/// made before or after. Publishing is deliberately NOT reachable from
/// this type at all — see [`CameraTrackHubProducer`]'s own doc comment for
/// why that split is structural, not merely conventional.
///
/// Track identity and format (camera, source role, codec — alongside the
/// stream id) are bound ONCE, at construction, and never adopted from
/// whatever the first published unit happens to claim: the producer's
/// `publish` checks every later unit against these bound values, so a
/// "wrong camera"/"wrong source role"/"wrong codec" rejection means
/// something — a hub that instead learned its identity from the first
/// publish could not meaningfully reject a later, different one as "wrong"
/// at all.
///
/// The keyframe-request check-and-clear
/// (`CameraTrackHubProducer::keyframe_requested_since_last_check`) is
/// deliberately NOT reachable from this type: `subscribe` still REQUESTS a
/// keyframe (a join is what creates the intent), but only the producer —
/// the single owner that actually acts on the request — may consume the
/// signal. Putting the check on this cloneable type would let any clone,
/// including one that never joined a subscriber on anyone's behalf, steal
/// a request another consumer or the producer itself was entitled to see:
///
/// ```compile_fail
/// use vigil::VideoCodec;
/// use vigil::camera_hub::CameraTrackHub;
/// use vigil::camera_track::{CameraId, SourceRole};
/// use vigil::workgraph::StreamId;
///
/// let (_producer, hub) = CameraTrackHub::new(
///     StreamId::new("driveway"),
///     CameraId::from_usb("driveway-node", "driveway-durable-id").expect("literal is durable"),
///     SourceRole::Analysis,
///     VideoCodec::H264,
///     30,
/// );
/// let _ = hub.keyframe_requested_since_last_check();
/// ```
#[derive(Clone)]
pub struct CameraTrackHub {
    inner: Arc<CameraTrackHubInner>,
    /// The stream's current effective keyframe interval, in OUTPUT FRAMES
    /// — never a duration. Feeds [`automatic_subscriber_capacity`] for any
    /// subscriber that does not supply an explicit
    /// [`SubscribeOptions::capacity`]. Updated whenever the producer's
    /// encoder changes its own effective interval, so the automatic
    /// default for FUTURE joiners tracks it; an already-joined subscriber
    /// with an automatic capacity keeps the capacity it was given at join
    /// time.
    effective_keyframe_interval_frames: u32,
}

impl CameraTrackHub {
    /// `camera`/`source_role`/`codec` are the track identity and format
    /// this hub is bound to for its whole lifetime — every later publish
    /// (through the returned [`CameraTrackHubProducer`]) is checked
    /// against these, never against whatever the first unit happens to
    /// claim. `effective_keyframe_interval_frames` is the stream's current
    /// keyframe interval in output frames (see
    /// `crate::encode::automatic_keyframe_interval_frames` for how a
    /// Vigil-owned encoder derives it); it seeds the automatic subscriber
    /// capacity default via [`automatic_subscriber_capacity`].
    ///
    /// Returns the exclusive producer handle and the cloneable consumer
    /// handle as a pair — the only way to obtain a
    /// [`CameraTrackHubProducer`] at all.
    pub fn new(
        stream_id: StreamId,
        camera: CameraId,
        source_role: SourceRole,
        codec: VideoCodec,
        effective_keyframe_interval_frames: u32,
    ) -> (CameraTrackHubProducer, CameraTrackHub) {
        let inner = Arc::new(CameraTrackHubInner {
            stream_id,
            camera,
            source_role,
            codec,
            state: Mutex::new(HubState {
                epoch: 1,
                next_expected_sequence: 0,
                retained_codec_config: None,
                current_epoch_marker: None,
                subscribers: HashMap::new(),
                next_subscription_id: 0,
                keyframe_requested: false,
                terminal: None,
            }),
        });
        (
            CameraTrackHubProducer {
                inner: Arc::clone(&inner),
            },
            CameraTrackHub {
                inner,
                effective_keyframe_interval_frames,
            },
        )
    }

    pub fn stream_id(&self) -> &StreamId {
        &self.inner.stream_id
    }

    /// The camera identity this hub was constructed for.
    pub fn camera(&self) -> &CameraId {
        &self.inner.camera
    }

    /// The source role this hub was constructed for.
    pub fn source_role(&self) -> SourceRole {
        self.inner.source_role
    }

    /// The codec this hub was constructed for.
    pub fn codec(&self) -> VideoCodec {
        self.inner.codec
    }

    /// The capacity a joiner with no explicit [`SubscribeOptions::capacity`]
    /// would receive right now.
    pub fn automatic_capacity(&self) -> NonZeroUsize {
        automatic_subscriber_capacity(self.effective_keyframe_interval_frames)
    }

    /// Join the hub with a declared loss contract and bounded capacity. A
    /// join delivers codec configuration then the first post-join
    /// keyframe — never an intervening delta unit — and deterministically
    /// requests a keyframe from the producer so that keyframe actually
    /// arrives.
    ///
    /// A join that lands AFTER a terminal has already been raised (source
    /// ended, source abandoned, or hub shutdown) is seeded with that
    /// terminal from [`HubState::terminal`] — its [`Subscription::recv`]
    /// reports the terminal immediately rather than waiting forever.
    pub fn subscribe(&self, options: SubscribeOptions) -> Subscription {
        let capacity = options
            .capacity
            .unwrap_or_else(|| self.automatic_capacity());
        let mut state = self
            .inner
            .state
            .lock()
            .expect("camera track hub state poisoned");

        let id = SubscriptionId(state.next_subscription_id);
        state.next_subscription_id += 1;
        // A joiner inherits the hub-wide terminal (see `HubState::terminal`)
        // exactly as if it had been attached at the moment the terminal was
        // raised: this is what makes the late-join case impossible rather
        // than merely handled — there is no separate "seed the late joiner"
        // codepath to forget, only this one field read at construction.
        let pending_end = state.terminal.clone();
        let joining_after_terminal = pending_end.is_some();
        // A join that inherits an already-raised terminal must see it as
        // its FIRST event: retained codec configuration and an owed epoch
        // marker describe a still-live stream this subscription is not
        // joining, and `poll_recv` delivers any pending codec configuration
        // ahead of a pending terminal, same as `drain`'s ordering — so
        // seeding either one here would hand a terminal-inheriting joiner
        // stale `Track` events in front of the `End` it already owns.
        let (pending_codec_config, pending_epoch_marker) = if joining_after_terminal {
            (None, None)
        } else {
            (
                state.retained_codec_config.clone(),
                // A subscriber joining during an active epoch has, by
                // definition, not yet delivered a keyframe of that epoch —
                // so it starts out owing the SAME marker every
                // already-attached subscriber was given at the epoch bump,
                // even if every one of them has already resolved its own
                // copy on its own earlier keyframe.
                state.current_epoch_marker,
            )
        };
        state.subscribers.insert(
            id,
            SubscriberState {
                loss: options.loss,
                capacity,
                queue: VecDeque::new(),
                pending_codec_config,
                pending_fault: None,
                detached: false,
                awaiting_random_access: true,
                pending_epoch_marker,
                pending_end,
                end_delivered: false,
                waker: None,
            },
        );
        // A join deterministically requests a producer keyframe, so this
        // subscriber's awaited random-access point actually arrives — but
        // only when it is actually joining a live stream: a join that
        // inherits an already-raised terminal can never have that keyframe
        // honored (there is no producer left to honor it), so requesting
        // one would be a request nothing will ever satisfy.
        if !joining_after_terminal {
            state.keyframe_requested = true;
        }

        Subscription {
            id,
            hub: Arc::clone(&self.inner),
        }
    }

    /// Detach one subscription without restarting the producer or
    /// disturbing any other subscription.
    ///
    /// A subscription detached while a [`Subscription::recv`] call is
    /// parked on it (its waker registered) wakes that waker before the
    /// entry is dropped — the removal itself already makes the next poll
    /// observe the subscription is gone (`poll_recv`'s `get_mut` misses and
    /// resolves `Ready(None)`); what a bare `remove` would silently drop is
    /// the wakeup that schedules that poll to actually run.
    pub fn detach(&self, id: SubscriptionId) {
        let waker = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("camera track hub state poisoned");
            state
                .subscribers
                .remove(&id)
                .and_then(|mut subscriber| take_waker(&mut subscriber))
        };
        // Woken only after the lock above is released (the guard went out
        // of scope at the end of the block) — see `take_waker` for why
        // waking under the lock is unsafe.
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// The hub itself is going away — distinct from anything about the
    /// source's own lifecycle (compare [`CameraTrackHubProducer::end_source`]
    /// and [`CameraTrackHubProducer::abandon_source`]). Raises
    /// [`SubscriptionEnd::HubShutdown`] as the terminal for every
    /// currently attached subscription, after any [`TrackEvent`]s already
    /// buffered for each one. Callable from any clone of this handle —
    /// every clone observes the identical live state.
    pub fn shutdown(&self) {
        let mut wakers = Vec::new();
        {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("camera track hub state poisoned");
            raise_hub_terminal(&mut state, || SubscriptionEnd::HubShutdown, &mut wakers);
        }
        // Same reason as `end_source`/`abandon_source` above: wake only
        // after the lock is released.
        wake_all(wakers);
    }

    /// How many subscriptions are currently attached — the proof point for
    /// "a stalled consumer is detached without disturbing others."
    pub fn active_subscription_count(&self) -> usize {
        let state = self
            .inner
            .state
            .lock()
            .expect("camera track hub state poisoned");
        state.subscribers.values().filter(|s| !s.detached).count()
    }
}

/// Apply one published unit to one subscriber's ring, honoring its
/// declared loss contract independently of every other subscriber.
fn deliver_to_subscriber(
    subscriber: &mut SubscriberState,
    unit: &Arc<EncodedAccessUnit>,
    retained_codec_config: &Option<Bytes>,
    stream_id: &StreamId,
    camera: &CameraId,
    epoch: u64,
    wakers: &mut Vec<Waker>,
) {
    if subscriber.detached {
        return;
    }

    if subscriber.awaiting_random_access {
        if !unit.keyframe {
            // Discard: a subscriber awaiting random access never receives
            // an intervening delta, and this does not consume a slot.
            return;
        }
        subscriber.awaiting_random_access = false;
    }

    if subscriber.queue.len() < subscriber.capacity.get() {
        let delivered = mark_for_delivery(subscriber, unit);
        subscriber.queue.push_back(delivered);
        wakers.extend(take_waker(subscriber));
        return;
    }

    match subscriber.loss {
        LossContract::RejoinAtRandomAccess => {
            // Discard the stale cursor and rejoin at the next decodable
            // random-access point: codec configuration, then a keyframe,
            // with no intervening delta ever delivered.
            subscriber.queue.clear();
            subscriber.awaiting_random_access = true;
            subscriber.pending_codec_config = retained_codec_config.clone();
            if unit.keyframe {
                subscriber.awaiting_random_access = false;
                let delivered = mark_for_delivery(subscriber, unit);
                subscriber.queue.push_back(delivered);
            }
            wakers.extend(take_waker(subscriber));
        }
        LossContract::FaultOnGap => {
            subscriber.pending_fault = Some(CoverageFault {
                stream_id: stream_id.clone(),
                camera: camera.clone(),
                epoch,
                lost_sequence_range: unit.sequence..=unit.sequence,
                reason: "subscriber could not keep pace with its declared capacity".to_string(),
            });
            subscriber.detached = true;
            wakers.extend(take_waker(subscriber));
        }
    }
}

/// The unit this subscription actually enqueues for one delivered access
/// unit: the shared, unmarked `Arc` when this subscription has already
/// resolved its own epoch marker (or never had one), or — the moment this
/// subscription still owes a marker — a per-subscription clone with
/// `discontinuity`/`format_change` overridden, consuming that owed marker
/// so it is never applied again on a later unit of the same epoch.
///
/// Every caller reaches this only for a unit that is genuinely about to be
/// delivered (never for a discarded delta), and delivery is gated by
/// `awaiting_random_access`, so the first call for any subscription after
/// an epoch bump is always that subscription's own first delivered
/// keyframe of the new epoch — exactly where the marker belongs, whether
/// or not the epoch happened to be opened by a delta this subscription
/// never received.
fn mark_for_delivery(
    subscriber: &mut SubscriberState,
    unit: &Arc<EncodedAccessUnit>,
) -> Arc<EncodedAccessUnit> {
    match subscriber.pending_epoch_marker.take() {
        Some(marker) => {
            let mut marked = (**unit).clone();
            marked.discontinuity = true;
            marked.format_change = marker.format_change;
            Arc::new(marked)
        }
        None => Arc::clone(unit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Wake;

    /// A `Waker` that, when invoked, immediately proves — deterministically,
    /// with no thread and no spin — whether the hub's internal `Mutex` is
    /// held at that exact instant, by calling `try_lock` on it directly.
    /// `try_lock` never blocks: it returns `Err` immediately if the lock is
    /// held by ANY thread, including this one, so there is nothing to wait
    /// on either way. This is the unit-level replacement for the old
    /// integration-level `a_waker_invoked_from_inside_publish_can_safely_touch_the_hub_without_deadlocking`
    /// test (formerly in `tests/camera_hub_fanout.rs`), which could only
    /// infer "no deadlock" from `JoinHandle::is_finished` after a bounded
    /// spin of cooperative yields on a spawned thread — real evidence of
    /// liveness, but not deterministic, since the outcome still depends on
    /// OS/thread scheduling. This test lives here rather than in the
    /// integration test file specifically because only code inside this
    /// module can reach the hub's private `inner.state: Mutex<HubState>`
    /// field to call `try_lock` on it at all.
    struct LockProbeWaker {
        hub: CameraTrackHub,
        invoked: AtomicBool,
        lock_was_free_when_invoked: AtomicBool,
    }

    impl Wake for LockProbeWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.invoked.store(true, Ordering::SeqCst);
            let lock_was_free = self.hub.inner.state.try_lock().is_ok();
            self.lock_was_free_when_invoked
                .store(lock_was_free, Ordering::SeqCst);
        }
    }

    /// Proves the property the old spin-based test was standing in for: a
    /// waker invoked from inside `publish` (the shape of a real async
    /// runtime whose task, once woken, is free to immediately call back
    /// into the very hub that woke it — e.g. polling a `Subscription` again
    /// from inside its own freshly-woken poll) always finds the hub's
    /// internal lock already released. A `std::sync::Mutex` is not
    /// reentrant: if `publish` ever invoked a waker WHILE STILL HOLDING its
    /// own lock, a waker that touched the hub would either deadlock (via
    /// `.lock()`) or, exactly as proven here, observe the lock still held
    /// (via `.try_lock()`) — this test asserts the lock is free, which a
    /// buggy "wake while still locked" implementation would fail
    /// immediately and deterministically, with no spin required either way.
    #[test]
    fn a_waker_invoked_from_inside_publish_finds_the_hub_state_lock_already_released() {
        let (mut producer, hub) = CameraTrackHub::new(
            StreamId::new("driveway"),
            CameraId::from_usb("test-node", "reentrant-waker-probe")
                .expect("fixture literal is a durable identity"),
            SourceRole::Analysis,
            VideoCodec::H264,
            30,
        );
        let mut subscription = hub.subscribe(SubscribeOptions {
            loss: LossContract::RejoinAtRandomAccess,
            capacity: NonZeroUsize::new(8),
        });

        let probe = Arc::new(LockProbeWaker {
            hub: hub.clone(),
            invoked: AtomicBool::new(false),
            lock_was_free_when_invoked: AtomicBool::new(false),
        });
        let waker: Waker = Waker::from(Arc::clone(&probe));

        // Register the probe waker on a pending recv() — nothing has been
        // published yet, so this must park.
        let mut pending = Box::pin(subscription.recv());
        let mut cx = Context::from_waker(&waker);
        assert!(
            matches!(Future::poll(pending.as_mut(), &mut cx), Poll::Pending),
            "a fresh join with nothing published yet must not fabricate an event"
        );
        drop(pending);

        // `publish` is what invokes the parked recv()'s waker. Unlike the
        // old test, this runs synchronously on THIS thread: if the
        // implementation still held its lock while calling `wake()`, the
        // probe's `try_lock` inside `wake_by_ref` would observe it held
        // (`Err`) and return immediately — no hang, no spin, just a false
        // assertion below.
        producer
            .publish(EncodedAccessUnit {
                stream_id: StreamId::new("driveway"),
                stream_epoch: 1,
                codec: VideoCodec::H264,
                codec_config: Some(Bytes::from(vec![0, 0, 0, 1, 0x67])),
                keyframe: true,
                data: Bytes::from(vec![0u8; 8]),
                timing: None,
                observed_at: None,
                camera: CameraId::from_usb("test-node", "reentrant-waker-probe")
                    .expect("fixture literal is a durable identity"),
                source_role: SourceRole::Analysis,
                sequence: 0,
                discontinuity: true,
                format_change: false,
                segment_sequence: 0,
            })
            .expect("publish of a fresh keyframe must succeed");

        assert!(
            probe.invoked.load(Ordering::SeqCst),
            "sanity: the probe waker must actually have run, or this test proves nothing"
        );
        assert!(
            probe.lock_was_free_when_invoked.load(Ordering::SeqCst),
            "the hub's internal lock must already be released before any waker is invoked, or a \
             real re-entrant waker (one that calls back into the hub) deadlocks"
        );
    }

    /// Proves `AUTOMATIC_CAPACITY_FLOOR_FRAMES` really derives from
    /// `encode::KEYFRAME_INTERVAL_MIN_FRAMES_AUTOMATIC_DEFAULT` rather than
    /// carrying its own independently typed `15` — a regression to a
    /// second, drifting literal would still likely equal 15 today, but
    /// this equality check is against the SOURCE constant by name, not a
    /// bare number, so it fails the moment the two are declared
    /// separately again.
    #[test]
    fn automatic_capacity_floor_derives_from_the_keyframe_interval_min_frames_default() {
        assert_eq!(
            AUTOMATIC_CAPACITY_FLOOR_FRAMES,
            crate::encode::KEYFRAME_INTERVAL_MIN_FRAMES_AUTOMATIC_DEFAULT as usize
        );
    }
}
