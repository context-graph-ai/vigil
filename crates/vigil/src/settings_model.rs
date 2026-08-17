//! The vocabulary every settings surface shares: who authored a value, which
//! surface they used, what scope it applies at, what control state the operator
//! sees, and the shape of a stored record.
//!
//! Nothing in this module resolves anything. Resolution lives in
//! [`crate::settings_store`]; the gate that runs before it lives in
//! [`crate::settings_domains`].

use std::fmt;

/// The detector's class allowlist.
pub const DETECTOR_CLASSES_SETTING: &str = "detector_classes";

/// How many frames the detector samples.
pub const DETECTOR_SAMPLE_FRAMES_SETTING: &str = "detector_sample_frames";

/// The stationary-scan interval, in seconds.
pub const DETECTOR_STATIONARY_INTERVAL_SETTING: &str = "detector_stationary_interval_secs";

/// The detector's confidence threshold.
pub const DETECTOR_CONFIDENCE_THRESHOLD_SETTING: &str = "detector_confidence_threshold";

/// The per-camera motion sensitivity, on a 1..=10 scale.
pub const MOTION_SENSITIVITY_SETTING: &str = "motion_sensitivity";

/// The detector queue capacity — the operator-control leg, which lives in the
/// store. Its deterministic-pressure-lever leg stays in the environment; two
/// different things sharing one spelling.
pub const DETECTOR_QUEUE_CAPACITY_SETTING: &str = "detector_queue_capacity";

/// The classes recognition covers. Never widens or narrows the detector's own
/// explicit class list.
pub const RECOGNITION_COVERED_CLASSES_SETTING: &str = "recognition_covered_classes";

/// This node's service identity. Shown read-only; changed only by the
/// deliberate operation, never by an ordinary edit.
pub const SERVICE_IDENTITY_SETTING: &str = "service_identity";

// ── The behavior values leaving the environment ────────────────────────────
//
// Each of these was read from an environment variable, which is not a settings
// surface and holds no rank in the authority model. They are settings a person
// sets, so they are named here, resolved from the store like any other, and
// their environment variable is reported as ignored when someone sets it.
//
// The spelling is the variable's own name without its prefix, lowercased —
// exactly what the ignored-variable report says a variable was reaching for —
// so an operator who is told "this named `site_name`" can find `site_name` on
// the operator surface rather than a second vocabulary for the same value.

/// What this deployment calls itself.
pub const SITE_NAME_SETTING: &str = "site_name";

/// The first camera's name, for the single-camera shape the multi-camera list
/// grew out of.
pub const CAMERA_NAME_SETTING: &str = "camera_name";

/// The first camera's stream, in the same single-camera shape.
pub const RTSP_URL_SETTING: &str = "rtsp_url";

/// The stream a live view uses when it differs from the detection ingest.
pub const LIVE_RTSP_URL_SETTING: &str = "live_rtsp_url";

/// The port the liveness surface answers on.
pub const HEALTH_PORT_SETTING: &str = "health_port";

/// The port the review data plane answers on.
pub const REVIEW_PORT_SETTING: &str = "review_port";

/// Which detection model this node runs.
pub const DETECTOR_MODEL_ID_SETTING: &str = "detector_model_id";

/// Where that model's weights are read from. A setting rather than packaging:
/// which model file is loaded changes what Vigil detects.
pub const DETECTOR_MODEL_PATH_SETTING: &str = "detector_model_path";

/// Where the recognition weights are read from; absent means recognition is
/// off. A setting for the same reason as the detector model path.
pub const RECOGNITION_WEIGHTS_DIR_SETTING: &str = "recognition_weights_dir";

/// The embedding space recognition matches within.
pub const RECOGNITION_SPACE_ID_SETTING: &str = "recognition_space_id";

/// How close a match has to be before recognition claims it.
pub const RECOGNITION_THRESHOLD_SETTING: &str = "recognition_threshold";

/// Whether this node carries the fabric hub role.
pub const FABRIC_HUB_SETTING: &str = "fabric_hub";

/// Whether this node may move a clip of a motion event to another node under
/// queue pressure — the per-source privacy opt-out.
pub const FABRIC_ALLOW_FRAME_OFFLOAD_SETTING: &str = "fabric_allow_frame_offload";

/// How long a fabric worker holds its lease.
pub const FABRIC_WORKER_LEASE_MS_SETTING: &str = "fabric_worker_lease_ms";

/// How long a node waits for a remote result before falling back to local work.
pub const FABRIC_FALLBACK_HORIZON_MS_SETTING: &str = "fabric_fallback_horizon_ms";

/// The first wait after a camera stream drops, before the backoff widens.
pub const RTSP_RETRY_INITIAL_MS_SETTING: &str = "rtsp_retry_initial_ms";

/// The widest that backoff gets.
pub const RTSP_RETRY_MAX_MS_SETTING: &str = "rtsp_retry_max_ms";

/// How long the decode probe may take before the software path is used.
pub const DECODE_PROBE_DEADLINE_SECS_SETTING: &str = "decode_probe_deadline_secs";

/// How long the hardware-decode probe may take.
pub const HARDWARE_PROBE_DEADLINE_SECS_SETTING: &str = "hardware_probe_deadline_secs";

/// The three authors, lowest authority first. Rank is a rank of authors, never
/// a race of clocks: between two different authors, time is irrelevant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Author {
    /// Vigil's own product-selected value, including one its tuning adjusted.
    Automatic,
    /// A value that arrived from a management server.
    Pushed,
    /// A value a human set at this deployment.
    LocalExplicit,
}

/// The concrete surface a record was authored through. Each local surface keeps
/// its own record; they coexist rather than overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Surface {
    /// Vigil chose it; no human surface is involved.
    Automatic,
    /// A management server pushed it.
    ManagementServer,
    /// The Home Assistant add-on options file.
    AddonOptions,
    /// A standalone configuration file.
    ConfigFile,
    /// The process startup options (the command line).
    StartupOptions,
    /// A `vigil settings` change made against this deployment.
    VigilSettings,
}

impl Author {
    /// The single rendered spelling for this author on every surface.
    pub fn as_str(self) -> &'static str {
        match self {
            Author::Automatic => "automatic",
            Author::Pushed => "management-server",
            Author::LocalExplicit => "local-explicit",
        }
    }
}

impl Surface {
    /// The single rendered spelling for this surface on every surface.
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Automatic => "automatic",
            Surface::ManagementServer => "management-server",
            Surface::AddonOptions => "add-on-options",
            Surface::ConfigFile => "config-file",
            Surface::StartupOptions => "startup-options",
            Surface::VigilSettings => "vigil-settings",
        }
    }

    /// The author rank this surface writes at.
    pub fn author(self) -> Author {
        match self {
            Surface::Automatic => Author::Automatic,
            Surface::ManagementServer => Author::Pushed,
            Surface::AddonOptions
            | Surface::ConfigFile
            | Surface::StartupOptions
            | Surface::VigilSettings => Author::LocalExplicit,
        }
    }

    /// Whether this surface can clear a record by dropping the key. Only a
    /// persistent file surface can, and only when it is present and parses.
    pub fn clears_by_absence(self) -> bool {
        matches!(self, Surface::AddonOptions | Surface::ConfigFile)
    }

    /// How this local surface is ordered against another that authored in the
    /// same pass, lowest first: startup options, then a `vigil settings`
    /// change, then the file surface. A tie between two local surfaces on one
    /// startup is decided here rather than by whichever ran last.
    pub fn first_start_precedence(self) -> u8 {
        match self {
            Surface::StartupOptions => 3,
            Surface::VigilSettings => 2,
            Surface::AddonOptions | Surface::ConfigFile => 1,
            Surface::ManagementServer | Surface::Automatic => 0,
        }
    }
}

/// The four scope levels a record can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScopeLevel {
    Tenant,
    Site,
    Node,
    Camera,
}

/// A record's scope: the level plus the thing it names at that level.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Scope {
    pub level: ScopeLevel,
    pub target: String,
}

impl Scope {
    pub fn node(target: impl Into<String>) -> Self {
        Self {
            level: ScopeLevel::Node,
            target: target.into(),
        }
    }

    pub fn camera(target: impl Into<String>) -> Self {
        Self {
            level: ScopeLevel::Camera,
            target: target.into(),
        }
    }

    pub fn site(target: impl Into<String>) -> Self {
        Self {
            level: ScopeLevel::Site,
            target: target.into(),
        }
    }

    pub fn tenant(target: impl Into<String>) -> Self {
        Self {
            level: ScopeLevel::Tenant,
            target: target.into(),
        }
    }

    /// Whether this scope names the same target the resolution is asked about,
    /// directly or by inheritance.
    pub fn covers(&self, target: &ScopeTarget) -> bool {
        match self.level {
            ScopeLevel::Tenant => self.target == target.tenant,
            ScopeLevel::Site => self.target == target.site,
            ScopeLevel::Node => self.target == target.node,
            ScopeLevel::Camera => target.camera.as_deref() == Some(self.target.as_str()),
        }
    }

    /// The rendered spelling of one scope, level and target together.
    pub fn as_display(&self) -> String {
        format!("{}:{}", self.level.as_str(), self.target)
    }
}

impl ScopeLevel {
    /// The single rendered spelling for one scope level.
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeLevel::Tenant => "tenant",
            ScopeLevel::Site => "site",
            ScopeLevel::Node => "node",
            ScopeLevel::Camera => "camera",
        }
    }

    /// How specific this level is, widest first. Resolution takes the most
    /// specific scope naming the target WITHIN an author, before authors are
    /// compared at all.
    pub fn specificity(self) -> u8 {
        match self {
            ScopeLevel::Tenant => 0,
            ScopeLevel::Site => 1,
            ScopeLevel::Node => 2,
            ScopeLevel::Camera => 3,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_display())
    }
}

/// What a resolution is being asked about: a node, or a camera on a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeTarget {
    pub tenant: String,
    pub site: String,
    pub node: String,
    pub camera: Option<String>,
}

/// The value a record carries. Settings are declared values, never expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    List(Vec<String>),
}

impl SettingValue {
    pub fn text(value: impl Into<String>) -> Self {
        SettingValue::Text(value.into())
    }

    pub fn list<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        SettingValue::List(values.into_iter().map(Into::into).collect())
    }

    /// Whether this value is null or empty in the sense the add-on authoring
    /// rules use: a posted key an interface submitted with nothing in it.
    pub fn is_null_or_empty(&self) -> bool {
        match self {
            SettingValue::Text(text) => text.trim().is_empty(),
            SettingValue::List(values) => values.is_empty(),
            SettingValue::Bool(_) | SettingValue::Int(_) | SettingValue::Float(_) => false,
        }
    }

    /// The type name this value carries, for a refusal that has to say what
    /// arrived rather than only what was wanted.
    pub fn type_name(&self) -> &'static str {
        match self {
            SettingValue::Bool(_) => "boolean",
            SettingValue::Int(_) => "integer",
            SettingValue::Float(_) => "number",
            SettingValue::Text(_) => "text",
            SettingValue::List(_) => "list",
        }
    }
}

impl fmt::Display for SettingValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingValue::Bool(value) => write!(formatter, "{value}"),
            SettingValue::Int(value) => write!(formatter, "{value}"),
            SettingValue::Float(value) => write!(formatter, "{value}"),
            SettingValue::Text(value) => formatter.write_str(value),
            SettingValue::List(values) => formatter.write_str(&values.join(",")),
        }
    }
}

/// The five control states an operator sees. `ManagedBy` names its domain so
/// the state carries its own instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlState {
    /// Vigil chose it and may revise it.
    Automatic,
    /// Vigil's own tuning moved an ungoverned value; reason and effect shown.
    AutoAdjusted,
    /// Vigil is choosing it inside a domain that is currently on.
    ManagedBy(String),
    /// A management server set it.
    SetByManagementServer,
    /// A human set it here.
    SetByYou,
}

impl ControlState {
    /// The single source for the phrase the operator reads for this state —
    /// "Automatic", "Auto-adjusted", "Managed by <domain>", "Set by your
    /// management server", "Set by you". Every surface renders through this, so
    /// the wording has one home rather than one per caller.
    pub fn label(&self) -> String {
        match self {
            ControlState::Automatic => "Automatic".to_string(),
            ControlState::AutoAdjusted => "Auto-adjusted".to_string(),
            ControlState::ManagedBy(domain) => format!("Managed by {domain}"),
            ControlState::SetByManagementServer => "Set by your management server".to_string(),
            ControlState::SetByYou => "Set by you".to_string(),
        }
    }

    /// The whitespace-free TOKEN a `control=` field carries — `automatic`,
    /// `auto-adjusted`, `managed-by:<domain>`, `set-by-management-server`,
    /// `set-by-you`. Distinct from [`ControlState::label`], which renders the
    /// operator PHRASE; a field value cannot carry spaces, so the two spellings
    /// are genuinely different and each needs its own single source.
    pub fn token(&self) -> String {
        match self {
            ControlState::Automatic => "automatic".to_string(),
            ControlState::AutoAdjusted => "auto-adjusted".to_string(),
            ControlState::ManagedBy(domain) => format!("managed-by:{domain}"),
            ControlState::SetByManagementServer => "set-by-management-server".to_string(),
            ControlState::SetByYou => "set-by-you".to_string(),
        }
    }
}

/// One stored record. Identity is (setting, author, surface, scope); records
/// coexist and are never overwritten by a different author or surface.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingRecord {
    pub setting: String,
    pub author: Author,
    pub surface: Surface,
    pub scope: Scope,
    pub value: SettingValue,
    /// Why this record exists. An automatic record owes one naming its
    /// derivation input; a record with no reason is a defect, not a blank field.
    pub reason: String,
    /// Written through the seamed wall clock, never `SystemTime::now`.
    pub written_at_ms: i64,
    /// The domain-membership generation in force when the record was written.
    /// A pin whose generation predates a domain's membership entry is
    /// grandfathered.
    pub domain_generation: u64,
    /// An explicit reset is a record, not a delete. A reset record names what
    /// the setting drops to.
    pub reset: bool,
}

impl SettingRecord {
    /// A local-explicit record authored through `surface`.
    pub fn local(
        setting: impl Into<String>,
        surface: Surface,
        scope: Scope,
        value: SettingValue,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            setting: setting.into(),
            author: Author::LocalExplicit,
            surface,
            scope,
            value,
            reason: reason.into(),
            written_at_ms: 0,
            domain_generation: 0,
            reset: false,
        }
    }

    /// A pushed record as a hub would apply it.
    pub fn pushed(
        setting: impl Into<String>,
        scope: Scope,
        value: SettingValue,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            setting: setting.into(),
            author: Author::Pushed,
            surface: Surface::ManagementServer,
            scope,
            value,
            reason: reason.into(),
            written_at_ms: 0,
            domain_generation: 0,
            reset: false,
        }
    }

    /// An automatic record with the derivation input as its reason.
    pub fn automatic(
        setting: impl Into<String>,
        scope: Scope,
        value: SettingValue,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            setting: setting.into(),
            author: Author::Automatic,
            surface: Surface::Automatic,
            scope,
            value,
            reason: reason.into(),
            written_at_ms: 0,
            domain_generation: 0,
            reset: false,
        }
    }
}

/// Why a lower-ranked record is not effective, reported on the face of the
/// operator surface rather than in history.
#[derive(Debug, Clone, PartialEq)]
pub enum HeldReason {
    /// A higher-ranked record is effective over this one.
    Shadowed {
        by_author: Author,
        by_surface: Surface,
    },
    /// A domain that is currently on holds this record.
    DormantUnderDomain { domain: String },
    /// Another local surface authored more recently.
    OutrankedByLocalSurface { by_surface: Surface },
}

/// A stored record that is not effective, with the reason it is not.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldRecord {
    pub record: SettingRecord,
    pub reason: HeldReason,
    /// The sentence the operator surface renders for this held record.
    pub statement: String,
}

/// What closes the gap between requested and running.
///
/// A cause is an instruction to the person reading it, so a build may only name
/// a transition it actually performs. Today it performs one: a restart. The two
/// below it describe a node adopting a new camera endpoint, or a new detector
/// model, while it runs — neither of which this build does, since both are read
/// as the process starts and by nothing else. They are kept as vocabulary for
/// the seam that will do it, and nothing emits them until that seam exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingCause {
    Restart,
    CameraReconnect,
    ModelReload,
    /// The node is closing this gap itself, right now: a preparation it started
    /// is outstanding and the value takes effect when that preparation lands.
    /// Naming a restart here would send an operator to power-cycle the thing
    /// watching their property while the machine is already doing the work.
    LiveTransition,
}

impl PendingCause {
    /// The single rendered spelling for what closes the requested/running gap.
    pub fn as_str(self) -> &'static str {
        match self {
            PendingCause::Restart => "restart",
            PendingCause::CameraReconnect => "camera-reconnect",
            PendingCause::ModelReload => "model-reload",
            PendingCause::LiveTransition => "live-transition",
        }
    }
}

/// The resolved answer for one setting at one scope target.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveSetting {
    pub setting: String,
    /// The effective value by the rules: gate first, then author rank, then
    /// most specific scope within the author.
    pub requested: SettingValue,
    /// What the process is actually using right now, when a runtime is up.
    pub running: Option<SettingValue>,
    /// Present when requested and running differ.
    pub pending: Option<PendingCause>,
    pub control_state: ControlState,
    pub author: Author,
    pub surface: Surface,
    pub scope: Scope,
    pub reason: String,
    /// When the effective record was authored, through the seamed persisted
    /// clock. A pushed value carries what the server said AND when it said it;
    /// without this the surface could not tell an operator that two pushes
    /// happened at all.
    pub authored_at_ms: i64,
    /// Every stored record that is not effective, with its held reason.
    pub held: Vec<HeldRecord>,
    /// Set when the effective record predates the domain now governing this
    /// setting, so the gate did not close over it.
    pub grandfathered: bool,
    /// Whether the camera being asked about is inheriting a wider scope's
    /// value rather than running its own override.
    pub inherited: bool,
}

impl EffectiveSetting {
    /// The held records that are shadowed by a higher-ranked record.
    pub fn shadowed(&self) -> Vec<&HeldRecord> {
        self.held
            .iter()
            .filter(|held| matches!(held.reason, HeldReason::Shadowed { .. }))
            .collect()
    }

    /// The held records a domain is holding dormant.
    pub fn dormant(&self) -> Vec<&HeldRecord> {
        self.held
            .iter()
            .filter(|held| matches!(held.reason, HeldReason::DormantUnderDomain { .. }))
            .collect()
    }
}

/// Every refusal in the settings surface has one shape: it names the cause and
/// the remedy. A refusal that carries only one of the two is a defect.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub kind: RefusalKind,
    /// What went wrong, in plain language.
    pub cause: String,
    /// What the operator does about it.
    pub remedy: String,
}

/// The refusal causes this surface knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusalKind {
    /// A write aimed at a value inside a domain that is currently on.
    GovernedValue { domain: String },
    /// The value fails the setting's declared validation.
    InvalidValue { setting: String },
    /// A backend this artifact does not carry.
    UnavailableBackend { setting: String },
    /// A detection class name the model inventory does not carry.
    InvalidClass { setting: String },
    /// An ordinary edit aimed at the service identity.
    IdentityOrdinaryEdit,
    /// A take-over instruction that does not name the domain it disables.
    TakeOverWithoutDomain,
    /// A settings change attempted while the store is unreadable.
    StoreUnreadable,
}

impl Refusal {
    /// The single rendered sentence, cause then remedy.
    pub fn statement(&self) -> String {
        format!("{} {}", self.cause.trim_end(), self.remedy.trim_start())
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.statement())
    }
}

/// Anything the settings surface can fail with.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsError {
    /// The write was refused, with its cause and remedy.
    Refused(Refusal),
    /// The store could not be opened or read.
    Store(String),
    /// Another Vigil runtime owns this data directory. Carries the holder so a
    /// caller — and a test — binds the identity typed rather than parsing it
    /// back out of a message, and so the refusal can name the holder without a
    /// second lookup.
    LockedByAnotherRuntime { holder_pid: u32 },
    /// The write was refused by the engine's scope-label constraint: this
    /// handle may not write at the pushed rank.
    ScopeLabelViolation { requested: String, allowed: String },
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingsError::Refused(refusal) => formatter.write_str(&refusal.statement()),
            SettingsError::Store(detail) => {
                write!(formatter, "the settings store could not be read: {detail}")
            }
            SettingsError::LockedByAnotherRuntime { holder_pid } => write!(
                formatter,
                "database is locked by another process (holder pid {holder_pid}); \
                 stop that runtime before starting a second one against this data directory"
            ),
            SettingsError::ScopeLabelViolation { requested, allowed } => write!(
                formatter,
                "this handle may not write at the {requested} rank; it carries {allowed}. \
                 Only a hub writes a pushed record."
            ),
        }
    }
}

impl std::error::Error for SettingsError {}
