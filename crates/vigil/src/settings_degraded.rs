//! The storeless runtime seam: starting the watching stack with no store behind
//! it, so a camera system does not go blind because a settings database is
//! unreadable.
//!
//! Live view, detection, and broker alerting keep working. Recording, review
//! history, corrections, and every settings change are unavailable and each says
//! so when attempted. The degraded path writes nothing — including the persisted
//! service identity.

use std::path::Path;

use crate::settings_model::{Refusal, RefusalKind};
use crate::settings_store::SettingsStore;

/// Why the store could not be opened, classified so the three failures get the
/// three different answers they are owed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreOpenClass {
    /// The store exists and opens cleanly. The ordinary case; no degraded path.
    Opens,
    /// A first start on a new machine. Vigil creates the store; not an error.
    Absent,
    /// Another Vigil runtime owns this data directory. Vigil refuses to start a
    /// second runtime and names the holder.
    LockedByAnotherRuntime { holder_pid: u32 },
    /// Corrupt file, unreadable volume, a storage layer that fails to open.
    /// Vigil starts anyway and says continuously that it is running unmanaged.
    Unreadable { detail: String },
}

/// Classify a store-open failure for `path`.
pub fn classify_store_open(path: &Path) -> Result<StoreOpenClass, String> {
    let store_file = SettingsStore::store_path(path);
    if !store_file.exists() {
        // A first start on a new machine. Classifying must not itself create
        // the store: creation is the runtime's decision, taken after this.
        return Ok(StoreOpenClass::Absent);
    }
    match SettingsStore::open(path) {
        Ok(_) => Ok(StoreOpenClass::Opens),
        Err(crate::settings_model::SettingsError::LockedByAnotherRuntime { holder_pid }) => {
            Ok(StoreOpenClass::LockedByAnotherRuntime { holder_pid })
        }
        Err(other) => Ok(StoreOpenClass::Unreadable {
            detail: other.to_string(),
        }),
    }
}

/// Why this run is unmanaged.
///
/// A run with no store behind it can have got there two different ways, and an
/// operator can only act on the one that actually happened: a store that cannot
/// be read is a storage problem, while a component the runtime could not load
/// is a missing or broken file on a node whose store is perfectly fine.
/// Reporting the second as the first sends the operator to repair something
/// that was never broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DegradedCause {
    /// The store exists and cannot be read.
    StoreUnreadable,
    /// A component the runtime loads before the store failed to load, so the
    /// store was never opened. `component` names the subsystem in the same
    /// words the operator's own configuration uses for it.
    ComponentUnavailable { component: String },
}

/// The cause this run declared, read by every surface that has to state or
/// refuse something because of it.
///
/// Declared once by the runtime that degraded, so the health answer, the
/// unmanaged statement, and every refusal name one cause rather than each
/// deciding for itself. A process that never started a runtime — a plain
/// command-line read against a store it cannot open — has nothing declared and
/// answers for the store, which is the only thing it looked at.
static DECLARED_CAUSE: std::sync::OnceLock<std::sync::Mutex<Option<DegradedCause>>> =
    std::sync::OnceLock::new();

fn declared_cause_slot() -> &'static std::sync::Mutex<Option<DegradedCause>> {
    DECLARED_CAUSE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Declare why this run is unmanaged, before anything states it.
pub fn declare_cause(cause: DegradedCause) {
    if let Ok(mut slot) = declared_cause_slot().lock() {
        *slot = Some(cause);
    }
}

/// Why this run is unmanaged, as every surface states it.
pub fn cause() -> DegradedCause {
    declared_cause_slot()
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or(DegradedCause::StoreUnreadable)
}

/// The component name a recognition-embedder failure carries. One spelling, so
/// the startup line and the operator surface name the same subsystem.
pub const RECOGNITION_EMBEDDER_COMPONENT: &str = "recognition-embedder";

/// The capabilities that keep working while degraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradedCapability {
    LiveView,
    Detection,
    BrokerAlerting,
    /// Only where the store itself is readable: a change lands and this run
    /// takes it on, for everything that does not need what is missing.
    SettingsChanges,
}

impl DegradedCapability {
    /// The single rendered spelling for this capability on the unmanaged line.
    /// Deliberately its OWN vocabulary, not the setting-line field keys: the
    /// list of capabilities still working is a different thing from a setting's
    /// running value, and sharing a key would conflate them.
    pub fn as_str(self) -> &'static str {
        match self {
            DegradedCapability::LiveView => "live-view",
            DegradedCapability::Detection => "detection",
            DegradedCapability::BrokerAlerting => "broker-alerting",
            DegradedCapability::SettingsChanges => "settings-changes",
        }
    }

    /// Every capability that keeps working while degraded, in render order —
    /// for the cause this run declared, because what survives depends on what
    /// went wrong. A store that cannot be read takes the settings surface down
    /// with it; a component that failed to load leaves it standing.
    pub fn all() -> &'static [DegradedCapability] {
        Self::for_cause(&cause())
    }

    /// Every capability that keeps working under one cause, in render order.
    pub fn for_cause(cause: &DegradedCause) -> &'static [DegradedCapability] {
        match cause {
            DegradedCause::StoreUnreadable => &[
                DegradedCapability::LiveView,
                DegradedCapability::Detection,
                DegradedCapability::BrokerAlerting,
            ],
            DegradedCause::ComponentUnavailable { .. } => &[
                DegradedCapability::LiveView,
                DegradedCapability::Detection,
                DegradedCapability::BrokerAlerting,
                DegradedCapability::SettingsChanges,
            ],
        }
    }
}

/// The capabilities that are unavailable while degraded. Each refuses with a
/// statement of what is unavailable, in one shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableCapability {
    Recording,
    ReviewHistory,
    Corrections,
    SettingsChange,
    /// Matching a detection against the enrolled site library. Unavailable on
    /// its own whenever the component that does the matching is what failed.
    Recognition,
}

impl UnavailableCapability {
    /// The single rendered spelling for this capability on the unmanaged line.
    pub fn as_str(self) -> &'static str {
        match self {
            UnavailableCapability::Recording => "recording",
            UnavailableCapability::ReviewHistory => "review-history",
            UnavailableCapability::Corrections => "corrections",
            UnavailableCapability::SettingsChange => "settings-changes",
            UnavailableCapability::Recognition => "recognition",
        }
    }

    /// Every capability that is unavailable while degraded, in render order —
    /// for the cause this run declared.
    pub fn all() -> &'static [UnavailableCapability] {
        Self::for_cause(&cause())
    }

    /// Every capability that is unavailable under one cause, in render order.
    pub fn for_cause(cause: &DegradedCause) -> &'static [UnavailableCapability] {
        match cause {
            DegradedCause::StoreUnreadable => &[
                UnavailableCapability::Recording,
                UnavailableCapability::ReviewHistory,
                UnavailableCapability::Corrections,
                UnavailableCapability::SettingsChange,
            ],
            // The store was never opened, so nothing that needs it works — but
            // the store file itself is fine, so a settings change still lands
            // and this run still takes it on.
            DegradedCause::ComponentUnavailable { .. } => &[
                UnavailableCapability::Recording,
                UnavailableCapability::ReviewHistory,
                UnavailableCapability::Corrections,
                UnavailableCapability::Recognition,
            ],
        }
    }
}

/// How delivery is reported while degraded: best-effort, never assured, because
/// the durable delivery path is exactly what is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryAssurance {
    Assured,
    BestEffort,
}

impl DeliveryAssurance {
    /// The single rendered spelling for how delivery is reported.
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryAssurance::Assured => "assured",
            DeliveryAssurance::BestEffort => "best-effort",
        }
    }
}

/// The one refusal shape every unavailable capability shares while degraded:
/// it names the capability, says it is unavailable, and names the store as
/// unreadable so the operator learns the cause rather than only the symptom.
pub fn degraded_refusal(capability: UnavailableCapability) -> Refusal {
    match cause() {
        DegradedCause::StoreUnreadable => Refusal {
            kind: RefusalKind::StoreUnreadable,
            cause: format!(
                "{} is unavailable: the settings store is unreadable, so nothing was recorded.",
                capability.as_str()
            ),
            remedy: "Vigil is running unmanaged — live view, detection and alerting continue, \
                     best-effort. Repair or replace the store to get this capability back."
                .to_string(),
        },
        // The store is not what failed here, and saying it was would send the
        // operator to repair a file that is perfectly fine. The refusal names
        // the component that actually did not load.
        DegradedCause::ComponentUnavailable { component } => Refusal {
            kind: RefusalKind::StoreUnreadable,
            cause: format!(
                "{} is unavailable: {component} could not be loaded, so this node started \
                 without opening its store and nothing was recorded.",
                capability.as_str()
            ),
            remedy: format!(
                "Vigil is running unmanaged — live view, detection and alerting continue, \
                 best-effort. Repair or replace {component} and restart to get this capability \
                 back; the store itself is intact."
            ),
        },
    }
}

/// The marker a refused capability carries on its first line, so a caller sets
/// a failing exit status without parsing prose — and so an answer that came
/// back over the control socket is never mistaken for a served request just
/// because the socket replied. Recording, review history, corrections and
/// recognition are not the settings surface: a clip that cannot be served is
/// not a settings error, and labelling it one sends the operator to look at
/// their settings.
pub const CAPABILITY_REFUSAL_PREFIX: &str = "capability-error";

/// The marker one refused capability carries. A refused settings CHANGE is a
/// settings error and keeps the settings surface's own marker, so that surface
/// reads the same whichever way an operator reaches it; every other capability
/// says plainly that a capability is unavailable.
pub fn refusal_prefix(capability: UnavailableCapability) -> &'static str {
    match capability {
        UnavailableCapability::SettingsChange => crate::settings_command::SETTINGS_ERROR_PREFIX,
        UnavailableCapability::Recording
        | UnavailableCapability::ReviewHistory
        | UnavailableCapability::Corrections
        | UnavailableCapability::Recognition => CAPABILITY_REFUSAL_PREFIX,
    }
}

/// Render a refusal as the line a surface hands back. One rendering, used by
/// the control socket, the command line, and the review data plane alike.
pub fn render_refusal(capability: UnavailableCapability, refusal: &Refusal) -> String {
    format!("{} {}\n", refusal_prefix(capability), refusal.statement())
}

/// The line prefix carrying what a settings answer cannot put in force,
/// deliberately not the error prefix: the answer is a real answer and a change
/// made through it is a real change.
pub const NOT_IN_FORCE_LINE_PREFIX: &str = "not-in-force";

/// What a settings answer owes an operator on a run that degraded for a reason
/// OTHER than the store, where the settings surface itself still works.
///
/// The change lands in a store that opens perfectly well and this run takes it
/// on where it can — so refusing it would be its own lie. What the operator
/// must not be left believing is that the whole of it is running: this process
/// holds no store handle and is missing the component that failed, so anything
/// resting on either is recorded and not in force. Nothing is added on a run
/// whose store is unreadable: there the change is refused outright and the
/// refusal already says why.
pub fn not_in_force_line() -> Option<String> {
    match cause() {
        DegradedCause::StoreUnreadable => None,
        DegradedCause::ComponentUnavailable { component } => Some(format!(
            "{NOT_IN_FORCE_LINE_PREFIX} component={component} {STATEMENT_KEY}=this node is \
             running unmanaged because {component} could not be loaded. a change made here is \
             recorded and this run takes it on where it can, but anything that needs {component} \
             or the store this run never opened is not in force until the node is repaired and \
             restarted.\n"
        )),
    }
}

/// The rendered refusal for one unavailable capability.
pub fn refusal_line(capability: UnavailableCapability) -> String {
    render_refusal(capability, &degraded_refusal(capability))
}

/// Whether an answer is a refusal rather than a served request. A caller that
/// prints this text exits non-zero: an operator who asked for a recording and
/// got an explanation did not get a recording. Both markers count — which one
/// a refusal carries says what was refused, never whether it was refused.
pub fn is_refusal(answer: &str) -> bool {
    answer.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with(CAPABILITY_REFUSAL_PREFIX)
            || line.starts_with(crate::settings_command::SETTINGS_ERROR_PREFIX)
    })
}

/// Which unavailable capability a command names, if it names one. The mapping
/// lives here so every surface refuses the same command with the same
/// capability rather than inventing its own wording.
pub fn capability_for_command(command: &str) -> Option<UnavailableCapability> {
    match command {
        "events" | "why" => Some(UnavailableCapability::ReviewHistory),
        "enroll" | "forget" => Some(UnavailableCapability::Corrections),
        "settings" => Some(UnavailableCapability::SettingsChange),
        _ => None,
    }
}

/// The continuous unmanaged statement: which capabilities keep working, which
/// are unavailable, and that delivery is best-effort rather than assured. It is
/// restated for as long as the degraded run lasts, never a startup warning that
/// scrolls away.
pub fn unmanaged_statement() -> String {
    let cause = cause();
    let statement = match &cause {
        DegradedCause::StoreUnreadable => {
            "the settings store is unreadable, so this node is running unmanaged: pins cannot be \
             read, pushed values are not honored, tuning is suspended, and changes are not \
             remembered. live view, detection and alerting continue, and delivery is best-effort \
             because the durable path is what is missing."
                .to_string()
        }
        DegradedCause::ComponentUnavailable { component } => format!(
            "{component} could not be loaded, so this node started without opening its store and \
             is running unmanaged: nothing is recorded, review history and corrections are \
             unavailable, and what that component does is off. the store itself is intact, so a \
             settings change still lands and this run still takes it on wherever it does not need \
             what is missing. live view, detection and alerting continue, and delivery is \
             best-effort because the durable path is what is missing."
        ),
    };
    format!(
        "{RUNNING_CAPABILITIES_KEY}={} {UNAVAILABLE_CAPABILITIES_KEY}={} {DELIVERY_KEY}={} \
         {STATEMENT_KEY}={statement}",
        DegradedCapability::for_cause(&cause)
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<&str>>()
            .join(","),
        UnavailableCapability::for_cause(&cause)
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<&str>>()
            .join(","),
        DeliveryAssurance::BestEffort.as_str(),
    )
}

/// The key naming which capabilities a degraded run keeps working. Its own
/// vocabulary, distinct from a setting line's `running` value: one is a list of
/// capabilities, the other is what a process applied for one setting.
pub const RUNNING_CAPABILITIES_KEY: &str = "running";

/// The key naming what a degraded run lost.
pub const UNAVAILABLE_CAPABILITIES_KEY: &str = "unavailable";

/// The key naming how a degraded run reports delivery.
pub const DELIVERY_KEY: &str = "delivery";

/// The key carrying the statement a person reads. Last on the line, because it
/// is prose and runs to the end.
pub const STATEMENT_KEY: &str = "statement";
