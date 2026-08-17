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
/// settings, and they DO still work. `VIGIL_FABRIC_WORKER_SLOT_DEADLINE_MS` is
/// the one to watch: it reads like a fabric setting and is not one — the source
/// that owns it declares it test-only with no shipped surface, so it stays in
/// the environment, keeps working, and is not reported here.
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

/// The three secrets the environment still carries, each with the variable
/// that backs it. One roster, so a test and the runtime never disagree about
/// the spelling.
const SECRET_VARIABLES: &[(&str, &str)] = &[
    ("rtsp_username", "VIGIL_RTSP_USERNAME"),
    ("rtsp_password", "VIGIL_RTSP_PASSWORD"),
    ("fabric_ticket", "VIGIL_FABRIC_TICKET"),
];

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
    crate::data_dir_from_env()
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
// reads only the names on the roster above, so it cannot become a back door
// for a value.
#[allow(clippy::disallowed_methods)]
pub fn ignored_behavior_variables() -> Vec<IgnoredBehaviorVariable> {
    BEHAVIOR_VARIABLES
        .iter()
        .filter(|variable| std::env::var_os(*variable).is_some())
        .map(|variable| IgnoredBehaviorVariable {
            variable: (*variable).to_string(),
            setting: setting_named_by(variable),
            reason: "the environment is not a settings surface: it holds no rank in the \
                     authority model, so this value authored nothing and changed nothing"
                .to_string(),
            set_it_here: "Set it in the add-on options, the config file, the startup options, \
                          or with a vigil settings change; the store is where behavior lives."
                .to_string(),
        })
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

/// Resolve one secret: the environment variable if present, otherwise the stored
/// value. Returns the effective secret alongside its source line.
// Secret material is one of the four categories that keep using the
// environment: a per-process injection from a secret manager or a rotation,
// which is why it WINS over the stored value. Enumerated and reviewed
// (`environment_read_surface.baseline.txt`), and confined to the three
// variables the secret roster names.
#[allow(clippy::disallowed_methods)]
pub fn resolve_secret(
    store_path: &Path,
    secret: &str,
) -> Result<(crate::secret::Secret, SecretSourceLine), SettingsError> {
    let stored = stored_secret(store_path, secret)?;
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

/// The stored value for one secret, resolved by the ordinary author rules and
/// never rendered anywhere.
fn stored_secret(store_path: &Path, secret: &str) -> Result<Option<String>, SettingsError> {
    let data_dir = store_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    if !crate::settings_store::store_exists(&data_dir) {
        // Nothing has ever been stored here, and asking is not a reason to
        // create somewhere to store it.
        return Ok(None);
    }
    let store = SettingsStore::open(&data_dir)?;
    let mut records: Vec<_> = store
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

/// The source lines for every secret this deployment knows about.
pub fn secret_source_lines(store_path: &Path) -> Result<Vec<SecretSourceLine>, SettingsError> {
    let mut lines = Vec::new();
    for (secret, _) in SECRET_VARIABLES {
        let (_value, line) = resolve_secret(store_path, secret)?;
        lines.push(line);
    }
    Ok(lines)
}
