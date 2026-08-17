//! Reflection: writing an effective value back into the add-on options so the
//! page the user trusts stops lying, and the echo ledger that closes the loop
//! between the store and the options file by construction rather than by timing.
//!
//! Saving options neither restarts the add-on nor reaches the running container,
//! so reflection is immediate and free. The restart-on-reflect setting governs
//! only whether Vigil then also triggers the restart that makes the container's
//! own options file agree.

use crate::settings_model::{SettingValue, SettingsError, Surface};

/// The ordinary store setting governing the restart, not the mirror. Default on,
/// ungoverned, reflected like any other setting.
pub const RESTART_ON_REFLECT_SETTING: &str = "restart_on_reflect";

/// The declared schema type of one add-on option key, used to pre-validate a
/// value before the write rather than after the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddonSchemaType {
    Bool,
    Int,
    Float,
    Str,
    ListOfStr,
}

/// One key the add-on schema declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddonSchemaKey {
    pub key: &'static str,
    pub declared_type: AddonSchemaType,
    /// A schema-required key must appear in every write; an optional key the
    /// user set is erased by a write that omits it.
    pub required: bool,
}

/// The add-on manifest itself.
///
/// Reflection pre-validates a value against the add-on's declared schema type
/// and posts the complete record, because a write is a full replace. Both read
/// the roster below — so a roster that disagreed with the manifest would not
/// merely go stale: a key it invented would be posted into a schema that does
/// not declare it and fail the write outright, a key it omitted could never be
/// mirrored at all, and a type it got wrong would invert the pre-validation,
/// judging the value the Supervisor accepts unwritable and the value it coerces
/// safe. The manifest is therefore the one declaration, read here rather than
/// retyped, and there is no second list to keep in step with it.
const ADDON_MANIFEST: &str = include_str!("../../../addons/vigil/config.yaml");

/// How one key of the `cameras:` list-element schema is addressed. A
/// camera-scoped setting and a deployment-wide one of the same name are two
/// declarations, and a flat name cannot tell them apart.
const CAMERA_KEY_PREFIX: &str = "cameras[].";

/// The add-on schema as declared in the manifest.
pub fn addon_schema_keys() -> Vec<AddonSchemaKey> {
    static DECLARED: std::sync::OnceLock<Vec<AddonSchemaKey>> = std::sync::OnceLock::new();
    DECLARED
        .get_or_init(|| declared_keys(ADDON_MANIFEST))
        .clone()
}

/// The declared type a manifest spelling names, or nothing where it names a
/// shape this roster does not carry. A key whose type is not understood is left
/// out rather than guessed at: reflecting through a guess is how a value gets
/// coerced silently, which is the one thing pre-validation exists to stop.
fn schema_type(spelling: &str) -> Option<AddonSchemaType> {
    match spelling {
        "bool" => Some(AddonSchemaType::Bool),
        "int" => Some(AddonSchemaType::Int),
        "float" => Some(AddonSchemaType::Float),
        "str" => Some(AddonSchemaType::Str),
        _ => None,
    }
}

/// One scalar declaration as the manifest spells it: a trailing `?` is the
/// optional marker, and its absence means every write has to carry the key.
fn scalar_key(key: &'static str, declaration: &str) -> Option<AddonSchemaKey> {
    let declaration = declaration.trim().trim_matches('"').trim_matches('\'');
    Some(AddonSchemaKey {
        key,
        declared_type: schema_type(declaration.trim_end_matches('?'))?,
        required: !declaration.ends_with('?'),
    })
}

/// Every key the manifest's `schema:` block declares: the top-level keys, plus
/// the `cameras:` list-element keys under their own prefix. The `cameras`
/// container contributes its element keys rather than a key of its own — it is
/// a list, not a setting.
fn declared_keys(manifest: &'static str) -> Vec<AddonSchemaKey> {
    let mut keys: Vec<AddonSchemaKey> = Vec::new();
    let mut in_schema = false;
    let mut in_cameras = false;
    // A key declared as a list opens on its own line and states its type on the
    // line under it, so the key is held until that line arrives.
    let mut list_declared: Option<&'static str> = None;

    for raw in manifest.lines() {
        let line = raw.split('#').next().unwrap_or("");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !line.starts_with(' ') {
            in_schema = trimmed.ends_with(':') && trimmed.trim_end_matches(':') == "schema";
            in_cameras = false;
            list_declared = None;
            continue;
        }
        if !in_schema {
            continue;
        }
        // A top-level key of the schema block.
        if line.starts_with("  ") && !line.starts_with("   ") && !trimmed.starts_with("- ") {
            let Some((key, declaration)) = trimmed.split_once(':') else {
                continue;
            };
            let key = key.trim();
            in_cameras = key == "cameras";
            list_declared = None;
            if declaration.trim().is_empty() {
                if !in_cameras {
                    list_declared = Some(key);
                }
                continue;
            }
            if let Some(declared) = scalar_key(key, declaration) {
                keys.push(declared);
            }
            continue;
        }
        // A key of the camera list-element schema, addressed per camera. The
        // prefixed name is composed once, on the first read, and then lives as
        // long as the process that read it.
        if in_cameras {
            let entry = trimmed.trim_start_matches("- ").trim();
            if let Some((key, declaration)) = entry.split_once(':') {
                let name: &'static str =
                    Box::leak(format!("{CAMERA_KEY_PREFIX}{}", key.trim()).into_boxed_str());
                if let Some(declared) = scalar_key(name, declaration) {
                    keys.push(declared);
                }
            }
            continue;
        }
        // The type line under a list-declared key. The documented Home
        // Assistant grammar for a loose list of free-text strings is the
        // element type `str` on its own line — so this line names what ONE
        // element of the list looks like, not the key's own declared type;
        // the key itself is always a list of that element type. (`list(a|b)`
        // is a different construct entirely — the enumeration/multi-select
        // validator — and is not produced by this parser.) The platform
        // renders a list schema as optional whatever marker it carries.
        if let Some(key) = list_declared.take()
            && trimmed.starts_with("- ")
        {
            let element = trimmed
                .trim_start_matches("- ")
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim_end_matches('?');
            if element == "str" {
                keys.push(AddonSchemaKey {
                    key,
                    declared_type: AddonSchemaType::ListOfStr,
                    required: false,
                });
            }
        }
    }
    keys
}

/// The key one setting reflects onto, when the add-on schema declares one.
fn schema_key(setting: &str) -> Option<AddonSchemaKey> {
    addon_schema_keys()
        .into_iter()
        .find(|declared| declared.key == setting)
}

/// Whether the add-on schema would faithfully carry this value, or would coerce
/// it. A fractional number into an integer option is the coercion case.
pub fn value_survives_schema_type(key: &str, value: &SettingValue) -> bool {
    let Some(declared) = schema_key(key) else {
        return false;
    };
    match (declared.declared_type, value) {
        (AddonSchemaType::Bool, SettingValue::Bool(_)) => true,
        // A fractional number written into an integer option is silently
        // truncated and reported as accepted, which is the measured hazard:
        // mirroring 30.7 as 30 while 30.7 runs makes the page lie in a new way.
        (AddonSchemaType::Int, SettingValue::Int(_)) => true,
        (AddonSchemaType::Int, SettingValue::Float(number)) => number.fract() == 0.0,
        (AddonSchemaType::Float, SettingValue::Float(_) | SettingValue::Int(_)) => true,
        (AddonSchemaType::Str, SettingValue::Text(_)) => true,
        (AddonSchemaType::ListOfStr, SettingValue::List(_)) => true,
        _ => false,
    }
}

/// Whether a value in this control state reflects onto the add-on options at
/// all. Vigil's own automatic and auto-adjusted values never do — they would
/// churn the user's record every time the product retuned itself, and the
/// honest representation of Automatic is ABSENCE: a key not present in the
/// options is a setting nobody has pinned. A value a domain is managing is
/// Vigil's choice too, so it does not reflect either.
pub fn control_state_reflects(state: &crate::settings_model::ControlState) -> bool {
    use crate::settings_model::ControlState;
    // Stated as the states that DO reflect, with everything else falling
    // through: a value somebody set here or a hub set for us. Vigil's own
    // values never reflect — they would churn the user's record every time the
    // product retuned itself, and the honest representation of Automatic is
    // ABSENCE. Naming the revised-by-vigil state here would also put a second
    // site for it outside the settings model, which the model owns alone.
    matches!(
        state,
        ControlState::SetByManagementServer | ControlState::SetByYou
    )
}

/// Why a reflection did not happen. Every one of these is user-visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReflectionFailure {
    /// A bare binary, a plain container, or a systemd service: there is no
    /// options file to write. Stated per setting, not discovered at write time.
    NoSupervisor,
    /// The setting has no key in the add-on schema. Stated per setting.
    NoSchemaTarget { setting: String },
    /// The schema would coerce the value, so it is not mirrored in coerced form.
    WouldBeCoerced {
        setting: String,
        /// What actually runs, named in the report.
        running: String,
    },
    /// The Supervisor rejected the write or was unreachable.
    SupervisorRejected { reason: String },
    /// The manifest does not declare the Supervisor permission this write needs.
    MissingSupervisorPermission { manifest_declaration: String },
}

/// What one reflection attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReflectionOutcome {
    /// The complete options record was posted and the value is mirrored.
    Mirrored {
        /// Whether Vigil then also requested the restart that makes the
        /// container's own file agree.
        restart_requested: bool,
    },
    /// The value applied where it could and the rest is pending, with the
    /// divergence reported on the operator surface.
    AppliedWithPendingDivergence { divergence: String },
    /// Not achieved, with the reason. The pushed value stays applied.
    NotAchieved(ReflectionFailure),
}

/// The complete options record a reflection write carries. Writing options is a
/// full replace validated against the posted content alone, so a write reads the
/// current record, merges the one change into it, and posts all of it.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionsRecord {
    pub entries: Vec<(String, SettingValue)>,
}

impl OptionsRecord {
    /// Merge one change into the current record, preserving every key the user
    /// set — including schema-optional keys a partial write would erase.
    pub fn merge(&self, key: &str, value: &SettingValue) -> OptionsRecord {
        // A write is a full replace validated against the posted content
        // alone, so every key the user set is carried forward: a partial post
        // is either an error or silent data loss on settings nobody touched.
        let mut entries = self.entries.clone();
        match entries.iter_mut().find(|(candidate, _)| candidate == key) {
            Some(existing) => existing.1 = value.clone(),
            None => entries.push((key.to_string(), value.clone())),
        }
        OptionsRecord { entries }
    }
}

/// The Supervisor client Vigil reflects through. Uses the container's own issued
/// token; a token belonging to another add-on proves nothing about this promise.
pub trait SupervisorOptionsClient {
    /// Read the add-on's current options record.
    fn read_options(&self) -> Result<OptionsRecord, ReflectionFailure>;

    /// Post the complete options record.
    fn write_options(&self, record: &OptionsRecord) -> Result<(), ReflectionFailure>;

    /// Request the restart that makes the container's own options file agree.
    fn restart_addon(&self) -> Result<(), ReflectionFailure>;

    /// The token source this client is using.
    fn token_source(&self) -> TokenSource;
}

/// Where the Supervisor token came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// The token this container was issued.
    OwnContainer,
    /// Any other token. Never sufficient proof of Vigil's own access.
    Other(String),
}

/// Where the Supervisor answers an add-on that talks to it from inside its own
/// container. One spelling, so the client a deployment builds and the client a
/// test points at a stand-in differ only in this address.
pub const SUPERVISOR_BASE_URL: &str = "http://supervisor/";

/// The production Supervisor client, reading the container's own token and
/// talking to the Supervisor's own surface over it.
pub struct ContainerSupervisorClient {
    base_url: String,
    token: String,
}

impl ContainerSupervisorClient {
    // SUPERVISOR_TOKEN is platform-injected service discovery, not a behavior
    // setting: the Supervisor issues it to this container and nobody configures
    // it, so it is an enumerated, reviewed read
    // (`environment_read_surface.baseline.txt`) rather than an ad-hoc one.
    #[allow(clippy::disallowed_methods)]
    pub fn from_environment() -> Result<Self, ReflectionFailure> {
        // The container's OWN issued token. A token belonging to another
        // add-on proves nothing about this promise.
        match std::env::var("SUPERVISOR_TOKEN") {
            Ok(token) if !token.is_empty() => Ok(Self::for_endpoint(SUPERVISOR_BASE_URL, token)),
            _ => Err(ReflectionFailure::NoSupervisor),
        }
    }

    /// The same client, addressed at `base_url`. The one production client
    /// type, so what a deployment runs and what is exercised against a
    /// Supervisor-shaped surface are the same code rather than two clients that
    /// can drift.
    pub fn for_endpoint(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: token.into(),
        }
    }

    /// Where this client talks to.
    pub fn endpoint(&self) -> &str {
        &self.base_url
    }

    /// The token this client presents. Never rendered on an operator surface —
    /// it is the container's credential, and what an operator is shown is the
    /// source, never the value.
    #[allow(dead_code)]
    fn token(&self) -> &str {
        &self.token
    }
}

/// Where the Supervisor answers for the add-on making the request — this
/// container's own add-on, reached with this container's own token.
const SELF_OPTIONS_PATH: &str = "/addons/self/options";

/// The add-on's own record, as the Supervisor renders it.
const SELF_INFO_PATH: &str = "/addons/self/info";

/// The restart that makes the container's own options file agree. Its own
/// request, because saving options never restarts anything.
const SELF_RESTART_PATH: &str = "/addons/self/restart";

impl SupervisorOptionsClient for ContainerSupervisorClient {
    fn read_options(&self) -> Result<OptionsRecord, ReflectionFailure> {
        let response = self.request("GET", SELF_INFO_PATH, None)?;
        let body: serde_json::Value = serde_json::from_str(&response).map_err(|error| {
            ReflectionFailure::SupervisorRejected {
                reason: format!("the supervisor's answer was not JSON: {error}"),
            }
        })?;
        // The record sits under the add-on's data, either as the data itself or
        // under an `options` member of it. Both spellings carry the same
        // content, so both are read rather than one being insisted on.
        let data = body.get("data").unwrap_or(&body);
        let object = data
            .get("options")
            .and_then(serde_json::Value::as_object)
            .or_else(|| data.as_object())
            .ok_or_else(|| ReflectionFailure::SupervisorRejected {
                reason: "the supervisor's answer carried no options record".to_string(),
            })?;
        Ok(OptionsRecord {
            entries: object
                .iter()
                .filter(|(key, _)| key.as_str() != "options")
                .map(|(key, value)| (key.clone(), setting_value_from_json(value)))
                .collect(),
        })
    }

    fn write_options(&self, record: &OptionsRecord) -> Result<(), ReflectionFailure> {
        // A write is a full replace validated against the posted content alone,
        // so the complete record travels — a partial post is either refused for
        // dropping a required key or silently erases the optional ones.
        let mut options = serde_json::Map::new();
        for (key, value) in &record.entries {
            options.insert(key.clone(), json_from_setting_value(value));
        }
        let body = serde_json::json!({ "options": options }).to_string();
        self.request("POST", SELF_OPTIONS_PATH, Some(&body))
            .map_err(|failure| redact_reflection_failure(failure, record))?;
        Ok(())
    }

    fn restart_addon(&self) -> Result<(), ReflectionFailure> {
        self.request("POST", SELF_RESTART_PATH, Some("{}"))?;
        Ok(())
    }

    fn token_source(&self) -> TokenSource {
        TokenSource::OwnContainer
    }
}

impl ContainerSupervisorClient {
    /// One request to the Supervisor, carrying the token this container was
    /// issued. Spoken directly over the connection rather than through a
    /// spawned tool, so a deployment with no shell utilities still reflects and
    /// a failure is this code's own to classify.
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<String, ReflectionFailure> {
        // Spoken through the crate's one Supervisor helper. Vigil is local-first
        // and opens no outbound connections of its own; the Supervisor's own
        // surface, reached from inside this container, is the single declared
        // exception and it has one home.
        let url = format!("{}{path}", self.base_url.trim_end_matches('/'));
        let (status, answer) =
            crate::supervisor::supervisor_request(method, &url, &self.token, body).map_err(
                |error| ReflectionFailure::SupervisorRejected {
                    reason: format!("the supervisor at {url} could not be reached: {error}"),
                },
            )?;
        match status {
            200..=299 => Ok(answer),
            // The Supervisor refusing an add-on's own write is the manifest
            // declaration missing, which is a thing an operator can fix — so it
            // is named rather than reported as a generic refusal.
            401 | 403 => Err(ReflectionFailure::MissingSupervisorPermission {
                manifest_declaration: MANIFEST_SUPERVISOR_DECLARATION.to_string(),
            }),
            other => Err(ReflectionFailure::SupervisorRejected {
                // The Supervisor's own explanation of what it refused is what
                // gives an operator something to act on; a bare status code
                // does not. Fall back to the status/path only when the answer
                // carries no such explanation.
                reason: supervisor_error_message(&answer).unwrap_or_else(|| {
                    format!("the supervisor answered {other} to {method} {path}")
                }),
            }),
        }
    }
}

/// The Supervisor's own explanation of a refusal, when its answer carries one.
///
/// That explanation can echo the record it was looking at, and a camera's
/// address carries the camera's credentials inside it. So every explanation is
/// scrubbed of addresses HERE, on the one path both reads and writes build
/// their reason through: a read has no posted record to compare against, so a
/// redaction that only knows what it just sent would have nothing to work with.
fn supervisor_error_message(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    parsed
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map(redact_addresses)
}

/// Every address in the text, reduced to the part of it that names a camera:
/// its scheme, host, port and path. The userinfo before the `@` and the whole
/// of the query and fragment after it are gone.
///
/// The rule carries no judgement about which part of an address is sensitive,
/// because that judgement is what keeps being wrong. `?usr=&pwd=` is one
/// vendor's convention, `?user=&password=` another's, and a camera naming its
/// secret `?k=` is doing nothing wrong; a list of credential-looking key names
/// can only enumerate the conventions somebody has already seen, and the next
/// one leaks in full. So the whole query goes, and what stays is exactly enough
/// for an operator to know which camera a refusal is about.
///
/// This recognises the address itself rather than a named field beside it,
/// which is what the manifest actually allows an operator to write: a camera
/// whose whole credential lives in its address has no password field to scrub.
///
/// The reduction is applied run by run rather than to address-shaped text only,
/// because an address does not have to keep its scheme to carry a credential:
/// `operator:secret@192.0.2.1` is the same secret written without one. So every
/// unbroken run of text loses whatever follows its first `?` or `#` and then
/// whatever precedes the last `@` still standing, and a run that names a scheme
/// keeps it. A run ends at whitespace or a quotation mark — ordinary punctuation
/// does NOT end it, because an operator's password may contain an apostrophe, a
/// brace, a comma, a bracket or a backslash, and a scan that stopped at one of
/// those would never reach the `@` that reveals the whole run as a credential,
/// printing the part before it in full.
///
/// A quotation mark has to stay a boundary, because the text this reads is a
/// refusal quoting a JSON record and the quote is what separates one field from
/// the next. That leaves one gap, and this closes it: a quotation mark written
/// INSIDE a credential would otherwise cut the run at it, and the tail — the
/// larger half of the secret — would begin a fresh run carrying no `?`, `#` or
/// `@` for anything to reduce, so it would print whole. So a run that was cut
/// at its query marker swallows whatever follows it up to the next boundary
/// that cannot itself be inside a credential: an UNESCAPED quotation mark, the
/// one closing the JSON string value the credential is written in. A quotation
/// mark an operator typed into a password arrives as `\"` once the record is
/// rendered, so an unescaped one is the string's own end and nothing else.
///
/// Stopping at whitespace instead is what this must not do, and the reason is
/// the shape of a real record: it is compact JSON with no whitespace anywhere,
/// so one query-cut address would swallow the entire remainder of the message
/// and a refusal about three cameras would name only the first. Whitespace
/// stays a stop as well, whichever comes first, and it remains the accepted
/// hole — a credential written with a space in it is two runs before any of
/// this looks at it.
fn redact_addresses(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(|character: char| !ends_a_run(character)) {
        let (before, after) = rest.split_at(start);
        out.push_str(before);
        let run_end = after.find(ends_a_run).unwrap_or(after.len());
        let (run, remainder) = after.split_at(run_end);
        let reduction = reduced_run(run);
        out.push_str(&reduction.kept);
        rest = remainder;
        if reduction.cut_at_query {
            rest = &rest[credential_tail_end(run, rest)..];
        }
    }
    out.push_str(rest);
    out
}

/// How much of the text after a query-cut run is still that run's credential.
///
/// The run stopped at a boundary character, and after a query cut that
/// character may be the credential's own — an apostrophe, a brace, a quotation
/// mark the operator typed. So the tail continues to the first boundary that
/// cannot be inside the credential: whitespace, or a quotation mark that is not
/// escaped. `run` is what precedes the tail, and it is read only to learn
/// whether the tail's first character is itself escaped by a backslash the run
/// ends with — the shape a typed quotation mark takes once the record is
/// rendered as JSON.
fn credential_tail_end(run: &str, rest: &str) -> usize {
    let mut escaped = run.chars().rev().take_while(|c| *c == '\\').count() % 2 == 1;
    for (index, character) in rest.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' || character.is_whitespace() {
            return index;
        }
    }
    rest.len()
}

/// Where the run of text an address can occupy stops.
fn ends_a_run(character: char) -> bool {
    character.is_whitespace() || character == '"'
}

/// One run of text with its credential-bearing parts gone, and whether the
/// query marker is what cut it — which is what tells the caller the text
/// immediately after this run is still the credential rather than the record
/// around it.
struct ReducedRun {
    kept: String,
    cut_at_query: bool,
}

/// One run of text with its credential-bearing parts gone.
///
/// The query goes FIRST and the userinfo second, because the two markers can
/// appear in either order and only this order is safe: a `@` written inside a
/// query value — `?pwd=p@secret` is a password an operator is entitled to
/// choose — is not userinfo at all, and cutting on it first would throw the
/// real host away and print the query tail in its place, which is the secret
/// itself.
///
/// A run's leading `://` is the scheme marker only when what precedes it can
/// actually be a scheme — a leading letter, then letters, digits, `+`, `-` and
/// `.` and nothing else. A prefix that starts with a digit or a dot names no
/// scheme however it is spelled after that, so `123://` and `...://` are the
/// head of a password and go with the rest of it.
/// An operator may omit the scheme, and a password holding a `://` then puts
/// that marker inside the credential itself; reading `operator:pa` as a scheme
/// keeps it and prints half the secret as the camera's address. Anything
/// carrying a `:` or an `@` names no scheme, so the run is schemeless and
/// reduces from its authority like any other.
fn reduced_run(run: &str) -> ReducedRun {
    const MARKER: &str = "://";
    let names_a_scheme = |candidate: &str| {
        let mut characters = candidate.chars();
        characters
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
            && characters.all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
            })
    };
    let (scheme, rest) = match run.find(MARKER) {
        Some(marker) if names_a_scheme(&run[..marker]) => run.split_at(marker + MARKER.len()),
        _ => ("", run),
    };
    let query = rest.find(['?', '#']);
    let kept = match query {
        Some(marker) => &rest[..marker],
        None => rest,
    };
    let kept = match kept.rfind('@') {
        Some(at) => &kept[at + '@'.len_utf8()..],
        None => kept,
    };
    let mut reduced = String::with_capacity(scheme.len() + kept.len());
    reduced.push_str(scheme);
    reduced.push_str(kept);
    ReducedRun {
        kept: reduced,
        cut_at_query: query.is_some(),
    }
}

/// Strip every credential value the record being posted carries out of a
/// refusal reason, so an operator gets Supervisor's own explanation without
/// the camera password (or username secret) that explanation may be quoting
/// straight out of the refused body.
fn redact_reflection_failure(
    failure: ReflectionFailure,
    record: &OptionsRecord,
) -> ReflectionFailure {
    match failure {
        ReflectionFailure::SupervisorRejected { reason } => ReflectionFailure::SupervisorRejected {
            reason: redact_credentials(&reason, record),
        },
        other => other,
    }
}

fn redact_credentials(reason: &str, record: &OptionsRecord) -> String {
    let mut redacted = reason.to_string();
    for secret in credential_values(record) {
        if !secret.is_empty() {
            redacted = redacted.replace(secret.as_str(), "[redacted]");
        }
    }
    redacted
}

/// Every credential-shaped value a structured entry in this record carries —
/// today, a camera's password. A camera's password also appears embedded
/// inside its own `rtsp_url`, so redacting this one value scrubs both.
fn credential_values(record: &OptionsRecord) -> Vec<String> {
    let mut values = Vec::new();
    for (_, value) in &record.entries {
        let SettingValue::List(elements) = value else {
            continue;
        };
        for element in elements {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(element) {
                collect_credential_strings(&parsed, &mut values);
            }
        }
    }
    values
}

fn collect_credential_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    let serde_json::Value::Object(map) = value else {
        return;
    };
    for (key, entry) in map {
        let key = key.to_ascii_lowercase();
        let is_credential =
            key.contains("password") || key.contains("secret") || key.contains("token");
        if is_credential && let serde_json::Value::String(text) = entry {
            out.push(text.clone());
        }
    }
}

/// One options value as the add-on's own record carries it.
fn setting_value_from_json(value: &serde_json::Value) -> SettingValue {
    match value {
        serde_json::Value::Bool(inner) => SettingValue::Bool(*inner),
        serde_json::Value::Number(number) => match number.as_i64() {
            Some(whole) => SettingValue::Int(whole),
            None => SettingValue::Float(number.as_f64().unwrap_or_default()),
        },
        serde_json::Value::Array(values) => {
            SettingValue::list(values.iter().map(|value| match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            }))
        }
        serde_json::Value::String(text) => SettingValue::text(text.clone()),
        other => SettingValue::text(other.to_string()),
    }
}

/// The same value on the way back out, in the shape the add-on schema declares.
fn json_from_setting_value(value: &SettingValue) -> serde_json::Value {
    match value {
        SettingValue::Bool(inner) => serde_json::Value::Bool(*inner),
        SettingValue::Int(inner) => serde_json::Value::from(*inner),
        SettingValue::Float(inner) => serde_json::Value::from(*inner),
        SettingValue::Text(inner) => serde_json::Value::String(inner.clone()),
        SettingValue::List(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|element| list_element_json(element))
                .collect(),
        ),
    }
}

/// One list element as it goes back onto the wire. `setting_value_from_json`
/// carries a structured element — a camera entry, never a plain setting — as
/// the compact JSON text of that structure, because `SettingValue::List` only
/// holds strings; that element round-trips as the same object or array here
/// rather than being wrapped as a quoted string, which is what let a camera
/// object reach the Supervisor as text where it declares an object. Every
/// other element — an ordinary class name or backend name — is not JSON-object
/// or JSON-array shaped and passes through as the plain string it always was.
fn list_element_json(element: &str) -> serde_json::Value {
    if !(element.starts_with('{') || element.starts_with('[')) {
        return serde_json::Value::String(element.to_string());
    }
    match serde_json::from_str::<serde_json::Value>(element) {
        Ok(parsed @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) => parsed,
        _ => serde_json::Value::String(element.to_string()),
    }
}

/// Whether two options records are the same content for echo purposes. Plain
/// equality is too strict for a float-typed key: the Supervisor coerces an
/// integral value posted into one (`3` becomes `3.0`), so what Vigil posted as
/// `Int(3)` reads back as `Float(3.0)` on the very next read with nobody
/// having touched the page. Treating that as a human edit manufactures a pin
/// nobody made, so a numerically-equal Int/Float pair on a float-typed key is
/// still Vigil's own echo.
fn options_records_equal_for_echo(written: &OptionsRecord, observed: &OptionsRecord) -> bool {
    written.entries.len() == observed.entries.len()
        && written.entries.iter().zip(&observed.entries).all(
            |((written_key, written_value), (observed_key, observed_value))| {
                written_key == observed_key
                    && values_equal_for_echo(written_key, written_value, observed_value)
            },
        )
}

/// Whether two values for one key are the same content for echo purposes, per
/// the coercion the schema declares for that key. Only the Int-into-Float
/// coercion is admitted here, matching `value_survives_schema_type`'s own
/// Int-into-Float leg — nothing else the Supervisor does to a value is a
/// faithful round-trip.
fn values_equal_for_echo(key: &str, written: &SettingValue, observed: &SettingValue) -> bool {
    if written == observed {
        return true;
    }
    let is_float_key = matches!(
        schema_key(key).map(|declared| declared.declared_type),
        Some(AddonSchemaType::Float)
    );
    if !is_float_key {
        return false;
    }
    let as_f64 = |value: &SettingValue| match value {
        SettingValue::Int(number) => Some(*number as f64),
        SettingValue::Float(number) => Some(*number),
        _ => None,
    };
    match (as_f64(written), as_f64(observed)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Reflect one effective value onto the add-on options surface.
pub fn reflect(
    client: &dyn SupervisorOptionsClient,
    ledger: &mut EchoLedger,
    setting: &str,
    value: &SettingValue,
    restart_on_reflect: bool,
) -> Result<ReflectionOutcome, SettingsError> {
    if client.token_source() != TokenSource::OwnContainer {
        // A token belonging to another add-on proves nothing about Vigil's own
        // access, so it is never used to write Vigil's own options.
        return Ok(ReflectionOutcome::NotAchieved(
            ReflectionFailure::SupervisorRejected {
                reason: "the Supervisor token this client carries is not the token this \
                         container was issued"
                    .to_string(),
            },
        ));
    }
    if schema_key(setting).is_none() {
        // Stated per setting, not discovered when a write fails.
        return Ok(ReflectionOutcome::NotAchieved(
            ReflectionFailure::NoSchemaTarget {
                setting: setting.to_string(),
            },
        ));
    }
    if !value_survives_schema_type(setting, value) {
        return Ok(ReflectionOutcome::NotAchieved(
            ReflectionFailure::WouldBeCoerced {
                setting: setting.to_string(),
                running: value.to_string(),
            },
        ));
    }
    let current = match client.read_options() {
        Ok(record) => record,
        Err(failure) => return Ok(ReflectionOutcome::NotAchieved(failure)),
    };
    let posted = current.merge(setting, value);
    if let Err(failure) = client.write_options(&posted) {
        // A failed reflection is never a reason to discard, alter, or un-apply
        // the value that was pushed.
        return Ok(ReflectionOutcome::NotAchieved(failure));
    }
    ledger.record_write_out(Surface::AddonOptions, setting, &posted)?;

    if !restart_on_reflect {
        return Ok(ReflectionOutcome::AppliedWithPendingDivergence {
            divergence: format!(
                "{setting} is mirrored in the add-on options, and the container's own options \
                 file still holds the old value until the next restart"
            ),
        });
    }
    match client.restart_addon() {
        Ok(()) => Ok(ReflectionOutcome::Mirrored {
            restart_requested: true,
        }),
        Err(failure) => Ok(ReflectionOutcome::NotAchieved(failure)),
    }
}

/// Reflect one effective value, taking the restart policy from the store rather
/// than from whoever happens to be calling.
///
/// `restart_on_reflect` is an ordinary setting — stored, authored, visible,
/// pushable — so the value that decides whether Vigil follows a mirror with a
/// restart is the resolved one, and a caller cannot hold an opinion of its own
/// about it.
///
/// The policy governs only the settings a restart is the remedy for. A setting
/// this process brings into force live is mirrored and never restarted for,
/// whatever the policy says: the restart exists to make a startup-only value
/// take effect, and there is nothing for it to do here except interrupt the
/// cameras and kill the command that asked for the change.
pub fn reflect_effective<S: crate::settings_store::ResolvesSettings>(
    store: &S,
    scope_target: &crate::settings_model::ScopeTarget,
    client: &dyn SupervisorOptionsClient,
    ledger: &mut EchoLedger,
    setting: &str,
    value: &SettingValue,
) -> Result<ReflectionOutcome, SettingsError> {
    let restart_on_reflect = match store.resolve(RESTART_ON_REFLECT_SETTING, scope_target) {
        Ok(effective) => !matches!(effective.requested, SettingValue::Bool(false)),
        // Unreadable is not a licence to invent a policy: the declared default
        // is on, and that is what a value nobody could read falls back to.
        Err(_) => true,
    };
    // What the restart is FOR is that a value the process only reads at startup
    // takes effect. A value this process brings into force itself has nothing a
    // restart could add — and taking one anyway replaces the container out from
    // under the command that asked for the change, stopping the cameras and
    // killing the settings surface mid-answer. So the timing the setting itself
    // declares decides this, read from the one roster that declares it rather
    // than from a second list here.
    if matches!(
        crate::settings_application::application_timing(setting),
        Some(crate::settings_application::ApplicationTiming::Live)
    ) {
        let outcome = reflect(client, ledger, setting, value, false)?;
        return Ok(match outcome {
            // The mirror happened and nothing is outstanding: the value is in
            // force on this process already, so the divergence `reflect`
            // reports for a withheld restart would name a wait that is not
            // happening.
            ReflectionOutcome::AppliedWithPendingDivergence { .. } => ReflectionOutcome::Mirrored {
                restart_requested: false,
            },
            other => other,
        });
    }
    reflect(client, ledger, setting, value, restart_on_reflect)
}

/// Mirror one landed change onto the add-on options for `data_dir`.
///
/// The echo ledger belongs to the deployment, so it is opened from the
/// deployment directory and persisted there: a ledger that only lived in the
/// memory of the process that wrote it would re-author the user's own options
/// as if a human had typed them on every boot.
///
/// A mirror that does not happen is reported and never un-applies anything: the
/// store is the authority and reflection is a courtesy to the surface.
pub fn reflect_landed_change<S: crate::settings_store::ResolvesSettings>(
    store: &S,
    data_dir: &std::path::Path,
    scope_target: &crate::settings_model::ScopeTarget,
    client: &dyn SupervisorOptionsClient,
    setting: &str,
) -> Result<ReflectionOutcome, SettingsError> {
    let effective = store.resolve(setting, scope_target)?;
    if !control_state_reflects(&effective.control_state) {
        // Vigil's own value is represented on the add-on options by ABSENCE, so
        // there is nothing to mirror and nothing has gone wrong.
        return Ok(ReflectionOutcome::NotAchieved(
            ReflectionFailure::NoSchemaTarget {
                setting: setting.to_string(),
            },
        ));
    }
    let mut ledger = EchoLedger::open(data_dir)?;
    reflect_effective(
        store,
        scope_target,
        client,
        &mut ledger,
        setting,
        &effective.requested,
    )
}

/// The per-surface, per-setting record of the complete content Vigil last wrote
/// out to a surface, and what it last read back from it. Content matching the
/// recorded write-out is Vigil's own echo and authors nothing, so an echo cannot
/// trigger another write and the cycle terminates.
///
/// The rows live in the deployment's own store, in the table declared never to
/// travel, so they are backed up, restored and moved with everything else. A
/// second durable home beside the store would be a second thing to keep
/// consistent with it, and the day the two disagreed Vigil would re-author the
/// operator's options file with a record nobody typed.
#[derive(Default)]
pub struct EchoLedger {
    _entries: Vec<EchoLedgerEntry>,
    /// The deployment's store the rows live in, when the ledger was opened over
    /// one. A ledger built in memory records the same rows and writes nowhere.
    home: Option<std::sync::Arc<context_graph::Store>>,
}

impl std::fmt::Debug for EchoLedger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EchoLedger")
            .field("entries", &self._entries)
            .field("durable", &self.home.is_some())
            .finish()
    }
}

impl Clone for EchoLedger {
    fn clone(&self) -> Self {
        Self {
            _entries: self._entries.clone(),
            home: self.home.clone(),
        }
    }
}

/// Two ledgers are the same ledger when they hold the same rows: the store
/// handle behind them is where they keep those rows, not part of what they say.
impl PartialEq for EchoLedger {
    fn eq(&self, other: &Self) -> bool {
        self._entries == other._entries
    }
}

/// One ledger row.
#[derive(Debug, Clone, PartialEq)]
pub struct EchoLedgerEntry {
    pub surface: Surface,
    pub setting: String,
    /// The complete posted record, not just the key that changed.
    pub last_write_out: OptionsRecord,
    /// The complete content last read back from the surface.
    pub last_read_back: Option<OptionsRecord>,
}

/// What reading a surface back concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EchoVerdict {
    /// The content matches the recorded write-out: Vigil's own echo. Authors
    /// nothing, bumps nothing, triggers no write.
    Echo,
    /// The content differs from the recorded write-out, so a human put it there.
    HumanAuthored,
}

/// The manifest declaration a self options-write needs, named in the failure
/// so an operator is told what is missing rather than that something failed.
const MANIFEST_SUPERVISOR_DECLARATION: &str = "hassio_api: true with hassio_role";

impl EchoLedger {
    /// Open the deployment's ledger. The caller names the deployment it is
    /// about, not a file: where the rows live inside it is Vigil's to decide,
    /// exactly as the store's own location is.
    pub fn open(path: &std::path::Path) -> Result<Self, SettingsError> {
        crate::settings_store::SettingsStore::open(path)?.echo_ledger()
    }

    /// The ledger over a store a caller already has open.
    pub(crate) fn over_store(
        store: std::sync::Arc<context_graph::Store>,
    ) -> Result<Self, SettingsError> {
        let mut entries = Vec::new();
        for row in crate::settings_store::read_echo_rows(&store)? {
            let surface =
                crate::settings_store::surface_from_token(&row.surface).ok_or_else(|| {
                    SettingsError::Store("the echo ledger names an unknown surface".to_string())
                })?;
            entries.push(EchoLedgerEntry {
                surface,
                setting: row.setting,
                last_write_out: decode_record(&row.last_write_out)?.ok_or_else(|| {
                    SettingsError::Store(
                        "an echo ledger row carries no record of what was written out".to_string(),
                    )
                })?,
                last_read_back: decode_record(&row.last_read_back)?,
            });
        }
        Ok(Self {
            _entries: entries,
            home: Some(store),
        })
    }

    /// Record the complete content just posted to a surface.
    pub fn record_write_out(
        &mut self,
        surface: Surface,
        setting: &str,
        posted: &OptionsRecord,
    ) -> Result<(), SettingsError> {
        match self
            ._entries
            .iter_mut()
            .find(|entry| entry.surface == surface && entry.setting == setting)
        {
            Some(entry) => entry.last_write_out = posted.clone(),
            None => self._entries.push(EchoLedgerEntry {
                surface,
                setting: setting.to_string(),
                last_write_out: posted.clone(),
                last_read_back: None,
            }),
        }
        self.persist(surface, setting)
    }

    /// Record the complete content just read back from a surface, against every
    /// setting Vigil has written out there. What was read back is a fact about
    /// the surface as a whole, so the next pass compares against the surface as
    /// it now stands rather than against content that is two edits old.
    pub fn record_read_back(
        &mut self,
        surface: Surface,
        observed: &OptionsRecord,
    ) -> Result<(), SettingsError> {
        let settings: Vec<String> = self
            ._entries
            .iter_mut()
            .filter(|entry| entry.surface == surface)
            .map(|entry| {
                entry.last_read_back = Some(observed.clone());
                entry.setting.clone()
            })
            .collect();
        for setting in settings {
            self.persist(surface, &setting)?;
        }
        Ok(())
    }

    /// Classify content read back from a surface.
    pub fn classify_read_back(
        &self,
        surface: Surface,
        setting: &str,
        observed: &OptionsRecord,
    ) -> EchoVerdict {
        let matches_write_out = self._entries.iter().any(|entry| {
            entry.surface == surface
                && entry.setting == setting
                && options_records_equal_for_echo(&entry.last_write_out, observed)
        });
        if matches_write_out {
            // Vigil's own echo: it authors nothing, bumps nothing, and
            // triggers no write, so the cycle terminates by construction.
            EchoVerdict::Echo
        } else {
            EchoVerdict::HumanAuthored
        }
    }

    pub fn entries(&self) -> &[EchoLedgerEntry] {
        &self._entries
    }

    /// Write one row through to the deployment's store. A ledger with no store
    /// behind it records the same row and writes nowhere, so a run with no
    /// store to open writes nothing at all.
    fn persist(&self, surface: Surface, setting: &str) -> Result<(), SettingsError> {
        let Some(store) = &self.home else {
            return Ok(());
        };
        let Some(entry) = self
            ._entries
            .iter()
            .find(|entry| entry.surface == surface && entry.setting == setting)
        else {
            return Ok(());
        };
        crate::settings_store::write_echo_row(
            store,
            &crate::settings_store::EchoRow {
                surface: entry.surface.as_str().to_string(),
                setting: entry.setting.clone(),
                last_write_out: encode_record(Some(&entry.last_write_out)),
                last_read_back: encode_record(entry.last_read_back.as_ref()),
            },
        )
    }
}

/// One surface record as the ledger stores it: the key/value pairs in the order
/// the surface carries them, each with the type it carries, so a ledger read
/// back compares equal to the record it was written from rather than calling a
/// genuine echo a human edit. Written as JSON, so a name or a value holding a
/// separator, a quote or a newline round-trips as itself.
fn encode_record(record: Option<&OptionsRecord>) -> String {
    let Some(record) = record else {
        // A row that has been written out but never read back says so, rather
        // than sharing a spelling with a surface that read back as empty.
        return serde_json::Value::Null.to_string();
    };
    serde_json::Value::Array(
        record
            .entries
            .iter()
            .map(|(key, value)| {
                serde_json::Value::Array(vec![
                    serde_json::Value::String(key.clone()),
                    serde_json::Value::String(value_type_tag(value).to_string()),
                    json_from_setting_value(value),
                ])
            })
            .collect(),
    )
    .to_string()
}

/// The type one stored value carries, kept beside it because JSON cannot tell a
/// whole number that is an integer setting from one that is a fractional
/// setting the surface happens to hold at a whole value.
fn value_type_tag(value: &SettingValue) -> &'static str {
    match value {
        SettingValue::Bool(_) => "bool",
        SettingValue::Int(_) => "int",
        SettingValue::Float(_) => "float",
        SettingValue::Text(_) => "text",
        SettingValue::List(_) => "list",
    }
}

fn decode_record(encoded: &str) -> Result<Option<OptionsRecord>, SettingsError> {
    let stored: serde_json::Value = serde_json::from_str(encoded).map_err(|error| {
        SettingsError::Store(format!(
            "an echo ledger row could not be read back: {error}"
        ))
    })?;
    let serde_json::Value::Array(rows) = stored else {
        return Ok(None);
    };
    let mut entries = Vec::new();
    for row in &rows {
        let fields = row.as_array().ok_or_else(|| {
            SettingsError::Store("an echo ledger row carries a malformed entry".to_string())
        })?;
        let (Some(key), Some(tag), Some(value)) = (
            fields.first().and_then(serde_json::Value::as_str),
            fields.get(1).and_then(serde_json::Value::as_str),
            fields.get(2),
        ) else {
            return Err(SettingsError::Store(
                "an echo ledger entry names no key, type, or value".to_string(),
            ));
        };
        entries.push((key.to_string(), decode_value(tag, value)));
    }
    Ok(Some(OptionsRecord { entries }))
}

/// One stored value back in the type the surface carried it at.
fn decode_value(tag: &str, value: &serde_json::Value) -> SettingValue {
    match tag {
        "bool" => SettingValue::Bool(value.as_bool().unwrap_or_default()),
        "int" => SettingValue::Int(value.as_i64().unwrap_or_default()),
        "float" => SettingValue::Float(value.as_f64().unwrap_or_default()),
        "list" => SettingValue::list(
            value
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .map(|value| match value {
                            serde_json::Value::String(text) => text.clone(),
                            other => other.to_string(),
                        })
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default(),
        ),
        _ => setting_value_from_json(value),
    }
}
