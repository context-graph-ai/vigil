//! What the environment is still for, now that it is not a behavior-settings
//! surface: secrets, with one deterministic precedence, and a report that an
//! environment variable naming a behavior setting did nothing.
//!
//! An environment variable naming a behavior setting is never silently dropped.
//! The secret leg is an override, not an author: it writes no record, it syncs
//! nowhere, and it disappears when the variable does.

use std::path::{Path, PathBuf};

use crate::settings_model::SettingsError;
use crate::settings_store::SettingsStore;

/// The environment variables that name a behavior setting. Every one of them
/// moved into the store, so naming one here does nothing but say so.
///
/// The declared internal diagnostics are deliberately absent — the fault
/// injections, the deterministic pressure levers, the test-harness controls.
/// Reporting one of those as ignored would be its own lie: they are not
/// settings, and they DO still work. `VIGIL_FABRIC_BRINGUP_DELAY_MS` is the one
/// to watch: it reads like a fabric setting and is not one — the source that
/// owns it declares it test-only with no shipped surface, so it stays in the
/// environment, keeps working, and is not reported here.
const BEHAVIOR_VARIABLES: &[&str] = &[
    "VIGIL_SITE_NAME",
    "VIGIL_CAMERA_NAME",
    "VIGIL_RTSP_URL",
    "VIGIL_LIVE_RTSP_URL",
    "VIGIL_HEALTH_PORT",
    "VIGIL_REVIEW_PORT",
    "VIGIL_DETECTOR_MODEL_ID",
    "VIGIL_DETECTOR_MODEL_PATH",
    "VIGIL_RECOGNITION_WEIGHTS_DIR",
    "VIGIL_RECOGNITION_SPACE_ID",
    "VIGIL_RECOGNITION_THRESHOLD",
    "VIGIL_DETECTOR_CONFIDENCE_THRESHOLD",
    "VIGIL_DETECTOR_SAMPLE_FRAMES",
    "VIGIL_DETECTOR_STATIONARY_INTERVAL_SECS",
    "VIGIL_HARDWARE_DECODING",
    "VIGIL_ACCELERATED_DETECTION",
    "VIGIL_FABRIC_HUB",
    "VIGIL_FABRIC_ALLOW_FRAME_OFFLOAD",
    "VIGIL_FABRIC_WORKER_LEASE_MS",
    "VIGIL_FABRIC_FALLBACK_HORIZON_MS",
    "VIGIL_RTSP_RETRY_INITIAL_MS",
    "VIGIL_RTSP_RETRY_MAX_MS",
    "VIGIL_DECODE_PROBE_DEADLINE_SECS",
    "VIGIL_HARDWARE_PROBE_DEADLINE_SECS",
];

/// The environment variables that USED to place something and no longer place
/// anything at all.
///
/// These were never settings, so they are not on the roster above: this one
/// named the pathname the running runtime published its control socket at.
/// There is no control socket — the process that owns the store answers
/// through the store itself, addressed by the store path alone — so the
/// variable cannot move anything any more. A value that quietly does nothing
/// is the worst answer available: the operator who set it believes their
/// deployment is wired the way they wrote it, and every later diagnosis starts
/// from that false premise. So a set value is reported, and the surfaces that
/// can refuse do refuse.
const RETIRED_VARIABLES: &[&str] = &["VIGIL_CONTROL_SOCKET"];

/// The three secrets the environment still carries, each with the variable
/// that backs it. One roster, so a test and the runtime never disagree about
/// the spelling.
const SECRET_VARIABLES: &[(&str, &str)] = &[
    ("rtsp_username", "VIGIL_RTSP_USERNAME"),
    ("rtsp_password", "VIGIL_RTSP_PASSWORD"),
    ("fabric_ticket", "VIGIL_FABRIC_TICKET"),
];

/// Whether this variable is one that was withdrawn rather than one that moved
/// into the store.
fn is_retired(variable: &str) -> bool {
    RETIRED_VARIABLES.contains(&variable)
}

/// Why this variable did nothing. A behavior variable did nothing because the
/// environment holds no rank in the authority model; a retired variable did
/// nothing because the thing it used to place no longer exists.
fn reason_for(variable: &str) -> String {
    if is_retired(variable) {
        return format!(
            "{variable} has no effect: it named the pathname the runtime published a control \
             socket at, and there is no control socket — the process that owns the store answers \
             through the store itself, addressed by the store path alone"
        );
    }
    "the environment is not a settings surface: it holds no rank in the authority model, so this \
     value authored nothing and changed nothing"
        .to_string()
}

/// Where the operator should go instead. A behavior variable has a store
/// surface to name; a retired variable has nothing to replace it with, so the
/// honest instruction is to unset it.
fn remedy_for(variable: &str) -> String {
    if is_retired(variable) {
        return format!(
            "Nothing replaces it — unset {variable}. The store path is the whole address, so \
             there is no location left to configure."
        );
    }
    "Set it in the add-on options, the config file, the startup options, or with a vigil settings \
     change; the store is where behavior lives."
        .to_string()
}

/// The setting one behavior variable was reaching for.
fn setting_named_by(variable: &str) -> String {
    variable
        .strip_prefix("VIGIL_")
        .unwrap_or(variable)
        .to_ascii_lowercase()
}

/// The deployment directory this process was pointed at. A location, not
/// behavior: a store cannot say where the store is.
pub fn bootstrap_data_dir() -> PathBuf {
    // Deliberately not its own read: the deployment directory is resolved in
    // ONE place for the whole crate, so a process that answers `vigil settings`
    // and a process that runs the cameras can never disagree about which
    // deployment they are talking about — and the environment surface keeps one
    // read site rather than two spellings of the same question.
    crate::configured_locations()
        .unwrap_or_else(|_| {
            // An add-on options file this process cannot read states nothing it
            // can act on; the environment it was started with still does, and
            // this bootstrap answer has no caller to report a failure to.
            crate::config::store_location_from_environment(&crate::store_location_environment())
        })
        .data_dir
}

/// One environment variable that named a behavior setting and was ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredBehaviorVariable {
    pub variable: String,
    /// The setting it named.
    pub setting: String,
    /// Why it did nothing: the environment is not a settings surface.
    pub reason: String,
    /// Where to set it instead — the store surface, never another environment
    /// variable.
    pub set_it_here: String,
}

/// Every environment variable present right now that names a behavior setting,
/// reported as ignored with the reason and the place to set it instead.
// The one read that exists BECAUSE the environment is not a settings surface:
// telling an operator their variable did nothing requires looking to see
// whether they set it. Enumerated and reviewed
// (`environment_read_surface.baseline.txt`), never an ad-hoc read — and it
// reads only the names on the two rosters above, so it cannot become a back
// door for a value.
#[allow(clippy::disallowed_methods)]
pub fn ignored_behavior_variables() -> Vec<IgnoredBehaviorVariable> {
    BEHAVIOR_VARIABLES
        .iter()
        .chain(RETIRED_VARIABLES.iter())
        .filter(|variable| std::env::var_os(*variable).is_some())
        .map(|variable| IgnoredBehaviorVariable {
            variable: (*variable).to_string(),
            setting: setting_named_by(variable),
            reason: reason_for(variable),
            set_it_here: remedy_for(variable),
        })
        .collect()
}

/// The line that opens the report for one ignored variable, so an operator or
/// a script can find it in a log without reading prose.
pub const IGNORED_VARIABLE_LINE_PREFIX: &str = "ignored-environment-variable";

/// What every command says out loud when a variable naming a behavior setting
/// is sitting in this process's environment doing nothing.
///
/// Empty when the operator set none of them, because the report describes what
/// is present right now rather than reciting a roster. It is an ADVISORY about
/// the environment and never the answer to the question asked: it leaves on the
/// error stream, changes no exit status, and refuses nothing — and it is said
/// on the surfaces the statement belongs to, the settings surface and the
/// startup output of the run that would have honored the variable, rather than
/// stapled to a review answer that has nothing to do with it. The alternative
/// is the one answer this model exists to refuse — silence, which leaves the
/// operator believing their deployment is wired the way they wrote it while
/// every later diagnosis starts from that false premise.
///
/// A RETIRED variable never reaches this: the surfaces that can refuse refuse
/// it outright, before any command is dispatched, so it is reported once and as
/// the refusal it is.
pub fn ignored_variable_report() -> String {
    ignored_behavior_variables()
        .into_iter()
        .filter(|found| !is_retired(&found.variable))
        .map(|found| {
            format!(
                "{IGNORED_VARIABLE_LINE_PREFIX} variable={} setting={}\n{}\n{}\n",
                found.variable, found.setting, found.reason, found.set_it_here
            )
        })
        .collect()
}

/// The withdrawn variables an operator has set right now, each already carrying
/// the sentence that says it has no effect and the instruction to unset it.
///
/// This filters what [`ignored_behavior_variables`] already found rather than
/// looking at the environment again: one read site answers "which of the names
/// we know about are present", and everything downstream is a decision about
/// that answer.
pub fn retired_variables_in_force() -> Vec<IgnoredBehaviorVariable> {
    ignored_behavior_variables()
        .into_iter()
        .filter(|found| is_retired(&found.variable))
        .collect()
}

/// The environment variable that backs one secret setting, when the secret is
/// one of the three the environment still carries. One roster, so a test and
/// the runtime never disagree about the spelling.
pub fn secret_environment_variable(secret: &str) -> Option<&'static str> {
    SECRET_VARIABLES
        .iter()
        .find(|(name, _)| *name == secret)
        .map(|(_, variable)| *variable)
}

/// Where a secret's effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretSource {
    /// A per-process injection. Wins whenever it is present.
    Environment,
    /// The stored value, resolved by the ordinary author rules.
    Stored,
}

/// A secret's source line for the operator surface. The value type is
/// non-printable; what the surface shows is the source, never the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretSourceLine {
    pub secret: String,
    /// Which source is effective.
    pub effective: SecretSource,
    /// Whether a stored value also exists underneath an environment one, so a
    /// secret set in two places is visible without being exposed.
    pub stored_value_exists: bool,
    /// The rendered line.
    pub statement: String,
}

/// Where a secret's STORED value is read from.
///
/// One resolution, two doors. A caller holding nothing but a pathname opens the
/// store to look; a caller ALREADY holding this deployment's store looks through
/// the handle it has. The second exists because a second open contends with the
/// first, and on a store somebody else is momentarily reading it is the open
/// that fails — costing an answer that was already fully built from the handle
/// that succeeded.
pub trait StoredSecrets {
    /// This deployment's recorded value for `secret`, if it has one.
    fn stored_secret(&self, secret: &str) -> Result<Option<String>, SettingsError>;
}

impl StoredSecrets for Path {
    fn stored_secret(&self, secret: &str) -> Result<Option<String>, SettingsError> {
        // The caller named the store file, and the read is a no-create one: a
        // deployment that has never stored anything says so through the open's
        // own typed refusal, and asking is never a reason to create somewhere
        // to store it. Opening a file rebuilt from the parent directory would
        // read a different store.
        match SettingsStore::open_to_read(self) {
            Ok(store) => store.stored_secret(secret),
            Err(SettingsError::StoreMissing { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

impl StoredSecrets for PathBuf {
    fn stored_secret(&self, secret: &str) -> Result<Option<String>, SettingsError> {
        self.as_path().stored_secret(secret)
    }
}

impl StoredSecrets for SettingsStore {
    fn stored_secret(&self, secret: &str) -> Result<Option<String>, SettingsError> {
        let mut records: Vec<_> = self
            .records(secret)?
            .into_iter()
            .filter(|record| !record.reset)
            .collect();
        records.sort_by(|left, right| {
            left.author
                .cmp(&right.author)
                .then_with(|| left.written_at_ms.cmp(&right.written_at_ms))
        });
        Ok(records.last().map(|record| record.value.to_string()))
    }
}

/// Resolve one secret: the environment variable if present, otherwise the stored
/// value. Returns the effective secret alongside its source line.
///
/// `store` is either this deployment's store PATH or a handle already open over
/// it — see [`StoredSecrets`]. Only the stored half differs; which value wins,
/// and what the operator is told about where it came from, are decided once,
/// here, whichever door the caller came through.
// Secret material is one of the four categories that keep using the
// environment: a per-process injection from a secret manager or a rotation,
// which is why it WINS over the stored value. Enumerated and reviewed
// (`environment_read_surface.baseline.txt`), and confined to the three
// variables the secret roster names.
#[allow(clippy::disallowed_methods)]
pub fn resolve_secret(
    store: &(impl StoredSecrets + ?Sized),
    secret: &str,
) -> Result<(crate::secret::Secret, SecretSourceLine), SettingsError> {
    let stored = store.stored_secret(secret)?;
    let injected = secret_environment_variable(secret).and_then(|variable| {
        std::env::var(variable)
            .ok()
            .filter(|value| !value.is_empty())
    });
    let (value, effective) = match injected {
        // The per-process injection wins: a rotation just handed this run a
        // credential, and letting a stored value shadow it is the silent
        // stale-credential failure this model refuses everywhere else.
        Some(value) => (value, SecretSource::Environment),
        None => (stored.clone().unwrap_or_default(), SecretSource::Stored),
    };
    let line = SecretSourceLine {
        secret: secret.to_string(),
        effective,
        stored_value_exists: stored.is_some(),
        statement: source_statement(secret, effective, stored.is_some()),
    };
    Ok((crate::secret::Secret::new(value), line))
}

/// The source line an operator reads. It names WHERE the secret is coming from
/// and whether another value sits underneath — never the value itself, which
/// keeps a secret set in two places visible without exposing either.
fn source_statement(secret: &str, effective: SecretSource, stored_exists: bool) -> String {
    match (effective, stored_exists) {
        (SecretSource::Environment, true) => format!(
            "{secret} is coming from the environment on this machine, and a stored value exists \
             underneath it"
        ),
        (SecretSource::Environment, false) => {
            format!("{secret} is coming from the environment on this machine")
        }
        (SecretSource::Stored, true) => {
            format!("{secret} is coming from the stored value on this deployment")
        }
        (SecretSource::Stored, false) => format!("{secret} is not set anywhere on this deployment"),
    }
}

/// The source lines for every secret this deployment knows about, for a caller
/// holding nothing but the store's pathname.
///
/// ONE no-create read of the store answers the whole roster, and a deployment
/// that has never started is answered from that read's own typed refusal rather
/// than from an existence question asked before it.
pub fn secret_source_lines(store_path: &Path) -> Result<Vec<SecretSourceLine>, SettingsError> {
    match SettingsStore::open_to_read(store_path) {
        Ok(store) => secret_source_lines_from(&store),
        Err(SettingsError::StoreMissing { .. }) => {
            secret_source_lines_for_a_store_that_is_not_there()
        }
        Err(error) => Err(error),
    }
}

/// The roster a deployment that has never started answers with: every secret
/// this artifact knows about, resolved through the ordinary rules against a
/// deployment that has stored nothing.
///
/// It opens nothing and names no path, so there is no store for it to create
/// and no second moment for it to disagree with the read that already decided
/// this deployment has never started. The rules themselves are not restated
/// here — an environment injection still wins, and the wording still comes from
/// the one place that words it — because a second copy of them is how the
/// never-started answer and the ordinary answer start disagreeing.
pub fn secret_source_lines_for_a_store_that_is_not_there()
-> Result<Vec<SecretSourceLine>, SettingsError> {
    secret_source_lines_from(&NothingIsStored)
}

/// A deployment with nothing stored, as the secret roster reads it.
struct NothingIsStored;

impl StoredSecrets for NothingIsStored {
    fn stored_secret(&self, _secret: &str) -> Result<Option<String>, SettingsError> {
        Ok(None)
    }
}

/// The source lines for every secret, read through the door the caller already
/// has — the handle it is holding, or the pathname it was given.
///
/// ONE open per listing, which is the whole point: the roster used to open this
/// deployment's store once more for every secret on it — three writable opens
/// beside the one the listing was already holding — so a listing an operator
/// asked for could be refused by contention it created itself, and the answer
/// already built from the live handle was thrown away with it.
pub fn secret_source_lines_from(
    store: &(impl StoredSecrets + ?Sized),
) -> Result<Vec<SecretSourceLine>, SettingsError> {
    let mut lines = Vec::new();
    for (secret, _) in SECRET_VARIABLES {
        let (_value, line) = resolve_secret(store, secret)?;
        lines.push(line);
    }
    Ok(lines)
}
