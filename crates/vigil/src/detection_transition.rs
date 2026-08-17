//! Moving the detection backend while the node keeps watching.
//!
//! An operator names a backend, or automatic management chooses one, and the
//! answer comes back straight away: what was asked for, what is still running,
//! and that a change is under way. The expensive part — building and
//! forward-testing a detector for every registered handle — happens away from
//! that answer, away from camera processing, and away from every read of the
//! settings surface. Only when every registered handle is running the new
//! backend does the node say it is running that backend.
//!
//! This module owns RUNTIME facts only: which backend each handle runs, what
//! is being prepared and under which request version, which instances are
//! already loaded, and the last real error a preparation reported. It never
//! writes the operator's stored choice — that authority stays with the
//! settings store, and the projection combines the two.
//!
//! A preparation under way ends exactly two ways: it completes, or the
//! preparation itself reports a real error. No clock ends one. The clock is
//! read once, when preparation begins, so a surface can say how long something
//! has been going and a person can tell slow from wedged; nothing here ever
//! compares it against anything.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// What a handle needing a detector is: a camera this node watches, or work
/// this node takes on for other nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleKind {
    Camera,
    DistributedWork,
}

/// One physical detector build: which handle it is for, the model artifact it
/// loads, the classes it emits, and the backend it targets. Two requests
/// naming the same identity are the same build, which is what stops repeated
/// attempts piling up abandoned work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparationIdentity {
    pub handle: String,
    pub model: String,
    pub classes: Vec<String>,
    pub backend: String,
}

/// A detector the preparation built and forward-tested, named by the token the
/// preparation minted for it. The token is what the coordinator carries; the
/// instance itself belongs to whoever built it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PreparedInstance(pub u64);

/// The instance a handle was already running when it was registered. The
/// coordinator did not mint it, so it carries the one token no preparation
/// ever returns — but it is a real loaded detector, which is why a move back
/// onto it needs no build.
const ALREADY_RUNNING_INSTANCE: PreparedInstance = PreparedInstance(0);

/// How one physical preparation ended. These two are the whole list: nothing
/// else ends an attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparationOutcome {
    Prepared(PreparedInstance),
    Failed(String),
}

/// What happened when a finished preparation asked to be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    /// The request this work belonged to has been superseded, so it installs
    /// nothing: the backend the operator now holds must stand.
    RejectedStaleVersion,
    UnknownHandle,
}

/// The deployment's one clock, read for information only.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A preparation under way, as a surface reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTransition {
    pub target_backend: String,
    /// When preparation began, stamped once.
    pub preparing_since_ms: u64,
    /// The last thing the preparation said about itself, if it has said
    /// anything. Never words put in its mouth.
    pub latest_progress: Option<String>,
}

/// Every runtime fact about detection backends, read as one answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionState {
    pub requested_backend: Option<String>,
    pub running_backend: String,
    pub pending: Option<PendingTransition>,
    pub failure_reason: Option<String>,
    pub moved_handles: Vec<String>,
    pub unmoved_handles: Vec<String>,
}

struct RegisteredHandle {
    name: String,
    kind: HandleKind,
    model: String,
    classes: Vec<String>,
    backend: String,
    installed: Option<PreparedInstance>,
}

struct Inner {
    running_backend: String,
    requested_backend: Option<String>,
    version: u64,
    target: Option<String>,
    preparing_since_ms: Option<u64>,
    latest_progress: Option<String>,
    failure_reason: Option<String>,
    handles: Vec<RegisteredHandle>,
    scheduled: Vec<PreparationIdentity>,
    retained: BTreeMap<(String, String), PreparedInstance>,
    physical_preparations: usize,
}

/// The one coordinator every backend move goes through — automatic startup
/// acceleration and an operator naming a backend alike.
pub struct DetectionTransitions {
    inner: Mutex<Inner>,
    clock: Clock,
}

impl DetectionTransitions {
    /// Build a coordinator for a node already running `running_backend`, with
    /// the deployment's clock.
    pub fn with_clock(running_backend: &str, clock: Clock) -> Self {
        Self {
            inner: Mutex::new(Inner {
                running_backend: running_backend.to_string(),
                requested_backend: None,
                version: 0,
                target: None,
                preparing_since_ms: None,
                latest_progress: None,
                failure_reason: None,
                handles: Vec::new(),
                scheduled: Vec::new(),
                retained: BTreeMap::new(),
                physical_preparations: 0,
            }),
            clock,
        }
    }

    /// Register a handle this node runs a detector for.
    ///
    /// Distributed work on a node that already watches a camera reuses that
    /// camera's detector rather than owning a second one; only a node with no
    /// camera owns a dedicated handle for it. So a camera registering displaces
    /// a dedicated distributed-work handle, and distributed work registering
    /// behind a camera adds nothing.
    pub fn register_handle(&self, name: &str, kind: HandleKind, model: &str, classes: &[&str]) {
        let mut inner = self.lock();
        match kind {
            HandleKind::Camera => inner
                .handles
                .retain(|handle| handle.kind != HandleKind::DistributedWork || handle.name == name),
            HandleKind::DistributedWork => {
                if inner
                    .handles
                    .iter()
                    .any(|handle| handle.kind == HandleKind::Camera)
                {
                    return;
                }
            }
        }
        if inner.handles.iter().any(|handle| handle.name == name) {
            return;
        }
        let backend = inner.running_backend.clone();
        inner.handles.push(RegisteredHandle {
            name: name.to_string(),
            kind,
            model: model.to_string(),
            classes: classes.iter().map(|class| (*class).to_string()).collect(),
            backend,
            installed: None,
        });
    }

    /// The handles this node prepares detectors for.
    pub fn registered_handles(&self) -> Vec<String> {
        self.lock()
            .handles
            .iter()
            .map(|handle| handle.name.clone())
            .collect()
    }

    /// Ask for a backend, and get back the version of this request.
    ///
    /// Every request is its own version, including a repeat of the same value:
    /// that is what makes re-issuing a command after a real error a fresh
    /// attempt doing real work, and what lets a later completion be told from
    /// an earlier one. Every handle not already on the target becomes
    /// outstanding work — including one whose instance for the target this node
    /// still has loaded.
    ///
    /// That handle's move costs nothing but a pointer swap, and it is still
    /// scheduled rather than recorded here, because this coordinator holds no
    /// detector: recording the move here would say the handle is on the new
    /// backend while the frames still go through the old one, and would leave
    /// nothing outstanding while the value the operator surface reads as
    /// running has not moved — a requested/running gap with nothing closing it,
    /// which is read as an instruction to restart a node that is already
    /// closing it. The preparation pass puts a retained instance back without
    /// loading a model, and the swap, this coordinator's account of it and the
    /// running value publish there as one act.
    pub fn request_backend(&self, backend: &str) -> u64 {
        let mut inner = self.lock();
        inner.version += 1;
        let version = inner.version;
        inner.requested_backend = Some(backend.to_string());
        inner.target = Some(backend.to_string());
        inner.failure_reason = None;
        inner.latest_progress = None;
        inner.scheduled.clear();

        let mut scheduled = Vec::new();
        let names: Vec<String> = inner
            .handles
            .iter()
            .filter(|handle| handle.backend != backend)
            .map(|handle| handle.name.clone())
            .collect();
        for name in names {
            scheduled.extend(inner.identity_for(&name, backend));
        }
        inner.scheduled = scheduled;
        if inner.scheduled.is_empty() {
            inner.preparing_since_ms = None;
        } else {
            inner.preparing_since_ms = Some((self.clock)());
        }
        inner.settle_running_backend();
        version
    }

    /// What a preparation last said about itself. Saying something does not end
    /// it and does not restart it.
    pub fn report_progress(&self, version: u64, progress: &str) {
        let mut inner = self.lock();
        if inner.version == version && !inner.scheduled.is_empty() {
            inner.latest_progress = Some(progress.to_string());
        }
    }

    /// The physical work still outstanding for the request now held.
    pub fn scheduled_preparations(&self) -> Vec<PreparationIdentity> {
        self.lock().scheduled.clone()
    }

    /// How many physical preparations this coordinator has performed. One per
    /// identity per attempt — never one per consumer, and never a second for
    /// work already under way.
    pub fn physical_preparation_count(&self) -> usize {
        self.lock().physical_preparations
    }

    /// Carry out the outstanding preparations.
    ///
    /// `prepare` is the physical work: it builds and forward-tests one
    /// detector and hands back the exact instance it tested, or the real error
    /// it hit. It runs with no lock held, so reads of the surface and every
    /// other command stay answerable throughout. A completion belonging to a
    /// superseded request installs nothing.
    pub fn drive_preparations(&self, prepare: impl Fn(&PreparationIdentity) -> PreparationOutcome) {
        let (work, version) = {
            let inner = self.lock();
            (inner.scheduled.clone(), inner.version)
        };
        for identity in work {
            let outcome = prepare(&identity);
            let mut inner = self.lock();
            inner.physical_preparations += 1;
            if inner.version != version {
                continue;
            }
            inner.scheduled.retain(|scheduled| scheduled != &identity);
            match outcome {
                PreparationOutcome::Prepared(instance) => {
                    inner.install(&identity.handle, &identity.backend, instance);
                }
                PreparationOutcome::Failed(reason) => {
                    inner.failure_reason = Some(reason);
                }
            }
            if inner.scheduled.is_empty() {
                inner.preparing_since_ms = None;
            }
            inner.settle_running_backend();
        }
    }

    /// Install a finished preparation against the request it belongs to.
    pub fn complete_preparation(
        &self,
        handle: &str,
        version: u64,
        instance: PreparedInstance,
    ) -> InstallOutcome {
        let mut inner = self.lock();
        if inner.version != version {
            return InstallOutcome::RejectedStaleVersion;
        }
        if !inner.handles.iter().any(|entry| entry.name == handle) {
            return InstallOutcome::UnknownHandle;
        }
        let target = inner
            .target
            .clone()
            .unwrap_or_else(|| inner.running_backend.clone());
        inner.install(handle, &target, instance);
        inner
            .scheduled
            .retain(|scheduled| scheduled.handle != handle);
        if inner.scheduled.is_empty() {
            inner.preparing_since_ms = None;
        }
        inner.settle_running_backend();
        InstallOutcome::Installed
    }

    /// The version of the request now held. Work carrying an older one
    /// installs nothing.
    pub fn request_version(&self) -> u64 {
        self.lock().version
    }

    /// The backend one handle is running.
    pub fn handle_backend(&self, handle: &str) -> Option<String> {
        self.lock()
            .handles
            .iter()
            .find(|entry| entry.name == handle)
            .map(|entry| entry.backend.clone())
    }

    /// Install a finished preparation onto the backend it was actually built
    /// for, which is not always the backend that was asked for: a request for
    /// a backend this machine turns out not to be able to enter is answered by
    /// the preparation with what it could build, and the node then names that
    /// rather than the name it was asked for.
    pub fn install_prepared(
        &self,
        handle: &str,
        version: u64,
        backend: &str,
        instance: PreparedInstance,
    ) -> InstallOutcome {
        self.install_prepared_taking_on(handle, version, backend, instance, &mut || {})
    }

    /// Put a prepared detector on its handle and record that it is on: the
    /// physical swap and the coordinator's account of it in ONE act.
    ///
    /// `take_on` is the swap itself, and it runs INSIDE this lock, after the
    /// request version has been checked and before anything is recorded. That
    /// order is the whole point: a preparation belonging to a superseded
    /// request is refused before it touches the detector, and once the
    /// detector HAS moved there is no moment at which this coordinator says
    /// the move is over while the frames still go through the old one.
    pub fn install_prepared_taking_on(
        &self,
        handle: &str,
        version: u64,
        backend: &str,
        instance: PreparedInstance,
        take_on: &mut dyn FnMut(),
    ) -> InstallOutcome {
        let mut inner = self.lock();
        if inner.version != version {
            return InstallOutcome::RejectedStaleVersion;
        }
        if !inner.handles.iter().any(|entry| entry.name == handle) {
            return InstallOutcome::UnknownHandle;
        }
        take_on();
        inner.install(handle, backend, instance);
        inner
            .scheduled
            .retain(|scheduled| scheduled.handle != handle);
        inner.settle_running_backend();
        InstallOutcome::Installed
    }

    /// End the attempt this version owns: it either completed or a preparation
    /// reported a real error. Nothing else ends one, so nothing else calls
    /// this. A newer request already holding the wheel is left untouched.
    pub fn conclude_attempt(&self, version: u64, failure: Option<String>) {
        let mut inner = self.lock();
        if inner.version != version {
            return;
        }
        inner.scheduled.clear();
        inner.preparing_since_ms = None;
        inner.failure_reason = failure;
        inner.settle_running_backend();
    }

    /// The instance one handle is running, when one was installed onto it.
    pub fn installed_instance(&self, handle: &str) -> Option<PreparedInstance> {
        self.lock()
            .handles
            .iter()
            .find(|entry| entry.name == handle)
            .and_then(|entry| entry.installed)
    }

    /// An instance kept loaded for one handle on one backend — the prepared way
    /// back, which is what makes a reverse move a pointer swap.
    pub fn retained_instance(&self, handle: &str, backend: &str) -> Option<PreparedInstance> {
        self.lock()
            .retained
            .get(&(handle.to_string(), backend.to_string()))
            .copied()
    }

    /// Every runtime fact, read as one answer.
    pub fn state(&self) -> TransitionState {
        let inner = self.lock();
        let reference = inner
            .requested_backend
            .clone()
            .unwrap_or_else(|| inner.running_backend.clone());
        let mut moved = Vec::new();
        let mut unmoved = Vec::new();
        for handle in &inner.handles {
            if handle.backend == reference {
                moved.push(handle.name.clone());
            } else {
                unmoved.push(handle.name.clone());
            }
        }
        let pending = (!inner.scheduled.is_empty())
            .then(|| {
                Some(PendingTransition {
                    target_backend: inner.target.clone()?,
                    preparing_since_ms: inner.preparing_since_ms?,
                    latest_progress: inner.latest_progress.clone(),
                })
            })
            .flatten();
        TransitionState {
            requested_backend: inner.requested_backend.clone(),
            running_backend: inner.running_backend.clone(),
            pending,
            failure_reason: inner.failure_reason.clone(),
            moved_handles: moved,
            unmoved_handles: unmoved,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    fn identity_for(&self, handle: &str, backend: &str) -> Option<PreparationIdentity> {
        self.handles
            .iter()
            .find(|entry| entry.name == handle)
            .map(|entry| PreparationIdentity {
                handle: entry.name.clone(),
                model: entry.model.clone(),
                classes: entry.classes.clone(),
                backend: backend.to_string(),
            })
    }

    /// Move one handle onto a backend, keeping the instance it steps away from
    /// as the prepared way back.
    fn install(&mut self, handle: &str, backend: &str, instance: PreparedInstance) {
        let Some(entry) = self.handles.iter_mut().find(|entry| entry.name == handle) else {
            return;
        };
        let previous_backend = entry.backend.clone();
        let replaced = entry.installed.unwrap_or(ALREADY_RUNNING_INSTANCE);
        entry.backend = backend.to_string();
        entry.installed = Some(instance);
        self.retained
            .insert((handle.to_string(), previous_backend.clone()), replaced);
        self.retained
            .insert((handle.to_string(), backend.to_string()), instance);
    }

    /// The node claims a backend only once every registered handle runs it.
    /// Until then the node names the backend the handles that have not moved
    /// are still on.
    fn settle_running_backend(&mut self) {
        let Some(first) = self.handles.first() else {
            return;
        };
        let candidate = first.backend.clone();
        if self
            .handles
            .iter()
            .all(|handle| handle.backend == candidate)
        {
            self.running_backend = candidate;
        }
    }
}
