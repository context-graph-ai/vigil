//! The one projection the operator surface renders through, so the answer over
//! the control socket and the answer read directly from the store are
//! byte-identical.
//!
//! It renders every touched setting's value, control state, author, surface,
//! scope and reason; requested, running and pending side by side; shadowed and
//! dormant records on the face rather than in history; the domain roster; the
//! read-only service identity with its derivation; and, while degraded, the
//! continuous unmanaged statement.

use crate::service_identity::{IdentityDerivation, ServiceIdentity};
use crate::settings_domains::DomainView;
use crate::settings_environment::SecretSourceLine;
use crate::settings_model::{Author, EffectiveSetting, SettingValue, SettingsError};

/// The line prefix carrying one setting.
pub const SETTING_LINE_PREFIX: &str = "setting";

/// The line prefix carrying one stored-but-not-effective record.
pub const HELD_LINE_PREFIX: &str = "held";

/// The line prefix carrying one automatic-management domain.
pub const DOMAIN_LINE_PREFIX: &str = "domain";

/// The line prefix carrying the read-only service identity.
pub const IDENTITY_LINE_PREFIX: &str = "identity";

/// The line prefix carrying one secret's source, never its value.
pub const SECRET_LINE_PREFIX: &str = "secret";

/// The line prefix carrying the continuous unmanaged statement.
pub const UNMANAGED_LINE_PREFIX: &str = "unmanaged";

/// The line a command uses to say that what was asked for is not available
/// through it, and where it IS available.
pub const UNAVAILABLE_LINE_PREFIX: &str = "unavailable";

/// The line carrying a store that is momentarily being read by somebody else.
/// Deliberately NOT the unmanaged line: this deployment is healthy and managed,
/// and the condition clears on its own.
pub const BUSY_LINE_PREFIX: &str = "busy";

/// The line naming one process that is reading the store right now.
pub const READER_LINE_PREFIX: &str = "reader";

/// The key naming the store an answer is about.
pub const PATH_KEY: &str = "path";

/// Everything one operator-surface answer carries.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsReport {
    pub settings: Vec<EffectiveSetting>,
    pub domains: Vec<DomainView>,
    /// The identity in force, when the process rendering this report is in a
    /// position to KNOW one. A separate command whose store cannot be read is
    /// not: it has no store to read the identity out of and no route to the run
    /// that holds it, so it reports none rather than deriving one.
    pub identity: Option<ServiceIdentity>,
    pub secrets: Vec<SecretSourceLine>,
    /// Present for as long as a degraded run lasts.
    pub unmanaged_statement: Option<String>,
}

impl SettingsReport {
    /// Render the report as the operator surface's lines. This is the single
    /// rendering; no caller formats settings output of its own.
    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(statement) = &self.unmanaged_statement {
            lines.push(format!("{UNMANAGED_LINE_PREFIX} {statement}"));
        }
        if let Some(line) = self.render_identity_line() {
            lines.push(line);
        }
        for setting in &self.settings {
            lines.push(render_setting(setting));
            for held in &setting.held {
                lines.push(format!(
                    "{HELD_LINE_PREFIX} {NAME_KEY}={} {VALUE_KEY}={} {AUTHOR_KEY}={} \
                     {SURFACE_KEY}={} {SCOPE_KEY}={} {REASON_KEY}={}",
                    held.record.setting,
                    held.record.value,
                    held.record.author.as_str(),
                    held.record.surface.as_str(),
                    held.record.scope.as_display(),
                    held.statement,
                ));
            }
        }
        for domain in &self.domains {
            for member in &domain.members {
                lines.push(format!(
                    "{DOMAIN_LINE_PREFIX} {SWITCH_KEY}={} {ON_KEY}={} {MEMBER_KEY}={} \
                     {CHOICE_KEY}={} {REASON_KEY}={}",
                    domain.switch, domain.on, member.setting, member.current_choice, member.reason,
                ));
            }
        }
        for secret in &self.secrets {
            lines.push(format!(
                "{SECRET_LINE_PREFIX} {NAME_KEY}={} {SOURCE_KEY}={} {STORED_KEY}={} \
                 {REASON_KEY}={}",
                secret.secret,
                match secret.effective {
                    crate::settings_environment::SecretSource::Environment => "environment",
                    crate::settings_environment::SecretSource::Stored => "stored",
                },
                secret.stored_value_exists,
                secret.statement,
            ));
        }
        lines
    }

    /// This node's identity as the operator reads it: the identifier, how it
    /// was arrived at, and the fact that it is shown rather than offered as
    /// something to set. One rendering, so the listing and the identity
    /// operation can never disagree about it.
    pub fn render_identity_line(&self) -> Option<String> {
        let identity = self.identity.as_ref()?;
        Some(format!(
            "{IDENTITY_LINE_PREFIX} {VALUE_KEY}={} {DERIVATION_KEY}={} {ACCESS_KEY}={READ_ONLY}",
            identity.value,
            derivation_token(&identity.derivation),
        ))
    }
}

/// The access the identity line reports: it is shown, never offered as a value
/// the generic set path operates on.
const READ_ONLY: &str = "read-only";

/// The key naming which domain a domain line is about.
pub const SWITCH_KEY: &str = "switch";

/// The key naming whether a domain is currently on.
pub const ON_KEY: &str = "on";

/// The key naming one setting a domain governs.
pub const MEMBER_KEY: &str = "member";

/// The key naming what the domain currently chooses for that member.
pub const CHOICE_KEY: &str = "choice";

/// The key naming where a secret's effective value came from.
pub const SOURCE_KEY: &str = "source";

/// The key naming whether a stored secret exists underneath an environment one.
pub const STORED_KEY: &str = "stored";

/// The whitespace-free token for how the identity was arrived at. One spelling,
/// so the startup receipt and the operator surface never name the same
/// derivation two different ways.
pub fn derivation_token(derivation: &IdentityDerivation) -> &'static str {
    match derivation {
        IdentityDerivation::DerivedAtFirstStart => "derived-at-first-start",
        IdentityDerivation::SetExplicitly => "set-explicitly",
        IdentityDerivation::DerivedNotYetPersisted => "derived-not-yet-persisted",
        IdentityDerivation::ConfiguredNotYetPersisted => "configured-not-yet-persisted",
    }
}

/// One setting, as the operator reads it. `reason` runs to the end of the line
/// because it is prose; every other field is a single whitespace-free token,
/// with the control state carried twice — once as the token a field can hold
/// and once as the phrase a person reads.
fn render_setting(setting: &EffectiveSetting) -> String {
    let value = rendered_value(setting);
    format!(
        "{SETTING_LINE_PREFIX} {NAME_KEY}={} {VALUE_KEY}={} {CONTROL_STATE_KEY}={} \
         {AUTHOR_KEY}={} {SURFACE_KEY}={} {SCOPE_KEY}={} {REQUESTED_KEY}={} {RUNNING_KEY}={} \
         {PENDING_KEY}={} {APPLIES_KEY}={} {WHEN_KEY}={} {CONTROL_PHRASE_KEY}={}{} \
         {REASON_KEY}={}",
        setting.setting,
        value,
        setting.control_state.token(),
        setting.author.as_str(),
        setting.surface.as_str(),
        setting.scope.as_display(),
        value,
        setting
            .running
            .as_ref()
            .map(rendered_running)
            .unwrap_or_else(|| NONE.to_string()),
        setting
            .pending
            .as_ref()
            .map(|cause| cause.as_str().to_string())
            .unwrap_or_else(|| NONE.to_string()),
        applies_token(&setting.setting),
        setting.authored_at_ms,
        setting.control_state.label(),
        {
            let mut extra = String::new();
            if setting.grandfathered {
                extra.push_str(&format!(" {GRANDFATHERED_KEY}=true"));
            }
            // A transition this node is carrying itself says when it began.
            // Present only while a preparation is outstanding, so its presence
            // is the whole message: the operator is watching work that is
            // genuinely running rather than deciding whether to restart.
            let transition = crate::settings_application::live_transition(&setting.setting);
            if let Some(transition) = &transition {
                extra.push_str(&format!(
                    " {PREPARING_SINCE_KEY}={}",
                    transition.preparing_since_ms
                ));
            }
            // An environment lever that supplied the running value names itself
            // here, beside the value it supplied, so the operator reads why the
            // number running is not the number they pinned.
            if let Some(source) = running_source(&setting.setting) {
                extra.push_str(&format!(
                    " {RUNNING_SOURCE_KEY}={ENVIRONMENT_SOURCE}:{}",
                    source.variable
                ));
                if let Some(shadowed) = source.shadowed {
                    extra.push_str(&format!(" {SHADOWED_SETTING_KEY}={shadowed}"));
                }
            }
            // Last, because it is the one field that is prose: what the
            // preparation itself last said about what it is doing, which is
            // what tells a long cold build apart from a wedged one. Preparing-
            // since alone says only that something started.
            if let Some(progress) = transition.and_then(|transition| transition.latest_progress) {
                extra.push_str(&format!(" {PROGRESS_KEY}={progress}"));
            }
            extra
        },
        setting.reason,
    )
}

/// What a field says when there is nothing to report — never a blank, because
/// a blank field and an absent one read the same and mean different things.
pub const NONE: &str = "none";

/// What a setting nobody has given a value renders as. Distinct from [`NONE`],
/// which is a field with nothing to report: this one says the value itself is
/// missing, which is a fact about the setting rather than about the field.
pub const ABSENT: &str = "absent";

/// What a value somebody deliberately set to nothing renders as. Setting a path
/// to nothing at all is a choice, and nobody having set it is not, so the two
/// never share a spelling.
pub const EMPTY: &str = "empty";

/// One setting's value as the operator reads it. A value field is always a
/// whitespace-free token, so the two ways a value can be blank get their own
/// words: absence is what Vigil's own floor answers when the value it would
/// carry is nothing at all, and an empty value is what an operator chose.
fn rendered_value(setting: &EffectiveSetting) -> String {
    if renders_as_source(&setting.setting) {
        // A camera endpoint carries the camera's credentials in its userinfo,
        // so what is answered is where this deployment's endpoint came from: a
        // record somebody authored, or nothing at all.
        return if setting.author == Author::Automatic {
            ABSENT.to_string()
        } else {
            SOURCE_STORED.to_string()
        };
    }
    match &setting.requested {
        SettingValue::Text(text) if text.is_empty() => {
            if setting.author == Author::Automatic {
                ABSENT.to_string()
            } else {
                EMPTY.to_string()
            }
        }
        value => value.to_string(),
    }
}

/// What the process is running one setting at, as the operator reads it. A
/// running field is always a whitespace-free token: a process running nothing
/// at all for a setting says [`ABSENT`], which is a fact about the value,
/// rather than leaving a blank a reader has to interpret as either "nothing" or
/// "this field is broken".
fn rendered_running(value: &SettingValue) -> String {
    match value {
        SettingValue::Text(text) if text.is_empty() => ABSENT.to_string(),
        value => value.to_string(),
    }
}

/// Where an endpoint the process took on came from, for the two settings whose
/// spelling is secret material. What an operator needs from these is the same
/// thing they need from a password: where it is coming from, never what it is.
pub const SOURCE_STORED: &str = "stored";

/// The counterpart for an endpoint this run took from its own configuration
/// rather than from a stored record.
pub const SOURCE_CONFIGURED: &str = "configured";

/// Whether one setting's spelling is secret material, so the surface answers
/// for it with a source and never with its value. Declared once, beside the
/// rendering that depends on it.
pub fn renders_as_source(setting: &str) -> bool {
    setting == crate::settings_model::RTSP_URL_SETTING
        || setting == crate::settings_model::LIVE_RTSP_URL_SETTING
}

/// The key naming when the effective record was authored. A value a management
/// server set carries what the server said and WHEN it said it; two pushes
/// that differ only in time have to read differently.
pub const WHEN_KEY: &str = "when";

/// The key carrying the control state as the PHRASE a person reads, beside the
/// token a field value can hold.
pub const CONTROL_PHRASE_KEY: &str = "control_phrase";

/// The key naming that the effective record predates the domain now governing
/// the setting, so the gate did not close over it.
pub const GRANDFATHERED_KEY: &str = "predates_domain";

/// The key naming where a running value came from when the store did not
/// supply it. Absent from every line the store does supply, so its presence is
/// the whole message.
pub const RUNNING_SOURCE_KEY: &str = "running-source";

/// The one spelling for a running value the environment supplied.
pub const ENVIRONMENT_SOURCE: &str = "environment";

/// The key naming the value this node would be running for that setting if the
/// environment lever were not there — the operator's pin where they set one,
/// and what this artifact would otherwise choose where they did not.
pub const SHADOWED_SETTING_KEY: &str = "shadowed-setting";

/// The key naming which setting a line is about.
pub const NAME_KEY: &str = "name";

/// The key naming a setting line's effective value.
pub const VALUE_KEY: &str = "value";

/// The key naming how the service identity was arrived at.
pub const DERIVATION_KEY: &str = "derivation";

/// The key naming that the service identity is shown read-only.
pub const ACCESS_KEY: &str = "access";

/// The key naming a setting line's control state.
pub const CONTROL_STATE_KEY: &str = "control";

/// The key naming a setting line's author.
pub const AUTHOR_KEY: &str = "author";

/// The key naming the surface a setting line's record was authored through.
pub const SURFACE_KEY: &str = "surface";

/// The key naming a setting line's scope.
pub const SCOPE_KEY: &str = "scope";

/// The key naming why a setting line's record exists.
pub const REASON_KEY: &str = "reason";

/// The key naming the effective value a setting line requests.
pub const REQUESTED_KEY: &str = "requested";

/// The key naming what the process is actually using right now.
pub const RUNNING_KEY: &str = "running";

/// The key naming what closes the gap between requested and running.
pub const PENDING_KEY: &str = "pending";

/// The key naming when the preparation now under way began. Rendered only while
/// one is outstanding: a person watching a long cold build reads this to tell a
/// slow move from a wedged one, and a node with nothing in flight has nothing
/// to say here.
pub const PREPARING_SINCE_KEY: &str = "preparing-since";

/// The key carrying what that preparation last said about itself — its own
/// words, never words put in its mouth. Prose, so it is rendered last of the
/// fields that precede the reason.
pub const PROGRESS_KEY: &str = "progress";

/// The key naming when a value takes effect. Requested, running and pending
/// alone leave the one question an operator is actually deciding unanswered —
/// do I wait, or do I restart the thing watching my property — because pending
/// says only that the two differ, never whether this machine closes the gap by
/// itself.
pub const APPLIES_KEY: &str = "applies";

/// What the applies field says for a value the running process takes on without
/// a restart.
pub const LIVE_TOKEN: &str = "live";

/// And for a value the process takes on at startup, so a change waits for the
/// next one.
pub const NEXT_RESTART_TOKEN: &str = "next-restart";

/// When one setting takes effect, as the operator reads it. Read from the
/// per-setting declaration that the consumers themselves are held to, so the
/// line an operator acts on and the timing Vigil actually implements are one
/// statement rather than two that can drift.
///
/// A setting with no declared timing renders as waiting for the next restart:
/// the conservative direction is the honest one, since claiming live
/// application for a value only read at startup would report a change as
/// running when nothing changed.
fn applies_token(setting: &str) -> &'static str {
    match crate::settings_application::application_timing(setting) {
        Some(crate::settings_application::ApplicationTiming::Live) => LIVE_TOKEN,
        _ => NEXT_RESTART_TOKEN,
    }
}

/// Build the report by reading the store directly, for a deployment whose
/// runtime is not up. Never creates a store.
pub fn report_by_direct_read(
    data_dir: &std::path::Path,
    store_path: &std::path::Path,
) -> Result<SettingsReport, SettingsError> {
    let target = crate::settings_model::ScopeTarget {
        tenant: default_deployment_name(data_dir),
        site: default_deployment_name(data_dir),
        node: default_deployment_name(data_dir),
        camera: None,
    };
    report_by_direct_read_at(data_dir, store_path, &target)
}

/// The deployment identity a direct read resolves at when the caller named
/// none: the key this node recorded for itself, which is the name its own
/// records are written under. A deployment that has never started has no key
/// and nothing recorded under one either, so that case answers at the
/// deployment directory's name and reads back the automatic floor. Reading
/// never generates a key — answering must leave the directory as it was.
fn default_deployment_name(data_dir: &std::path::Path) -> String {
    crate::node_key::recorded(data_dir).unwrap_or_else(|| crate::node_key::fallback_name(data_dir))
}

/// Build the report by direct read against an explicit deployment identity,
/// so a caller resolving records it wrote itself binds them to the same
/// tenant, site, node and camera the report resolves at.
pub fn report_by_direct_read_at(
    data_dir: &std::path::Path,
    store_path: &std::path::Path,
    target: &crate::settings_model::ScopeTarget,
) -> Result<SettingsReport, SettingsError> {
    // ONE no-create read of the store, and its own outcome decides everything
    // below. Nothing asks the filesystem whether a store is there first: an
    // existence question answers about a different moment than the open that
    // follows it, answers `false` for a configured store whose volume never
    // mounted exactly as it does for a directory nobody has run anything in,
    // and the open it guarded was a WRITABLE one — so a listing an operator
    // asked for contended with the very store it was reporting on, and created
    // that store when it was not there.
    let store = match crate::settings_store::SettingsStore::open_to_read(store_path) {
        Ok(store) => store,
        // Nothing has ever run here. The deployment still reports what it WOULD
        // run at, and answering leaves its directory exactly as it was.
        Err(SettingsError::StoreMissing { .. }) => {
            return Ok(SettingsReport {
                settings: crate::settings_store::automatic_floor_listing(target),
                domains: automatic_domain_views(),
                identity: Some(crate::service_identity::read_sidecar(data_dir).unwrap_or(
                    ServiceIdentity {
                        // The identity is a name a person reads, so a deployment
                        // that has never written one down answers with the
                        // deployment's own directory name — never with the key its
                        // records are stored under, which is an identifier and
                        // names nothing.
                        value: crate::node_key::fallback_name(data_dir),
                        derivation: IdentityDerivation::DerivedNotYetPersisted,
                    },
                )),
                secrets:
                    crate::settings_environment::secret_source_lines_for_a_store_that_is_not_there(
                    )?,
                unmanaged_statement: None,
            });
        }
        Err(error) => return Err(error),
    };
    let settings = store.listing(target)?;
    let domains = domain_views(&store, target)?;
    // The stored record is the identity in force — the same one the running
    // node announces as its Home Assistant device. The sidecar answers only
    // for a deployment whose store holds no identity yet, and the deployment
    // directory's own name only when neither can say.
    let identity = Some(
        crate::service_identity::persisted(&store, data_dir)?
            .or_else(|| crate::service_identity::read_sidecar(data_dir))
            .unwrap_or_else(|| ServiceIdentity {
                value: default_deployment_name(data_dir),
                derivation: IdentityDerivation::DerivedNotYetPersisted,
            }),
    );
    Ok(SettingsReport {
        settings,
        domains,
        identity,
        // Through the handle this listing is already holding. Opening the store
        // again — once per secret — contended with that very handle, and on a
        // deployment somebody is momentarily reading it was the open that
        // failed, throwing away the settings, domains and identity already read
        // back through the handle that succeeded.
        secrets: crate::settings_environment::secret_source_lines_from(&store)?,
        unmanaged_statement: None,
    })
}

/// The report a run with no store behind it answers with. Nothing stored can
/// be read, so every setting reads at the automatic floor this artifact would
/// choose for itself, the identity is the one this run derived for itself and
/// did not write down, and no secret's stored side is claimed either way — the
/// unmanaged statement leading the answer is what says so.
pub fn report_degraded(data_dir: &std::path::Path) -> SettingsReport {
    let name = default_deployment_name(data_dir);
    let target = crate::settings_model::ScopeTarget {
        tenant: name.clone(),
        site: name.clone(),
        node: name.clone(),
        camera: None,
    };
    SettingsReport {
        settings: crate::settings_store::automatic_floor_listing(&target),
        domains: automatic_domain_views(),
        // The identity this run resolved and is announcing, when this process
        // IS the run. It answers for itself out of what it already resolved,
        // rather than working the question out a second time from different
        // inputs — a node that names itself one thing on the broker and another
        // on the surface an operator reads is a node they cannot identify.
        //
        // Any other process gets no identity at all, and says why. It could
        // only produce one by deriving it from the directory it was handed or
        // by guessing at a configuration file nobody named it, and that is not
        // a vaguer answer than the truth — it is a DIFFERENT node's answer,
        // with nothing on the line to say so. The operator who reads it against
        // their broker, or against another node, has no way to tell.
        identity: crate::service_identity::in_force(),
        secrets: Vec::new(),
        unmanaged_statement: Some(crate::settings_degraded::unmanaged_statement()),
    }
}

/// The whole answer a command gives when the store it selected cannot be read.
///
/// Two lines and nothing else. The unmanaged line, because the degraded
/// capability contract is true of this deployment however the operator reached
/// it; and one line naming the store — the thing they actually repair — saying
/// that this deployment's settings and this node's identity are not available
/// THROUGH THIS COMMAND, and where they are.
///
/// What is deliberately absent is the point. There is no settings listing, no
/// domain roster, no secret line and no identity: this process could not read
/// the store, so every one of those would be a value it made up — the
/// artifact's own defaults standing where an operator expects their
/// deployment's, and a node name derived from a directory. That is not a vaguer
/// answer than the truth, it is a DIFFERENT deployment's answer, and nothing on
/// the line would say so. One command's worth of redirection costs an operator
/// far less than a confidently wrong reading costs their diagnosis.
pub fn render_unreadable(store_path: &std::path::Path) -> String {
    // Assembled from whole sentences rather than one wrapped literal: a
    // continued string literal carries its own source indentation into the
    // operator's line, and this line is read by a person.
    let mut reason = String::new();
    reason.push_str("the selected store is unreadable, so this command cannot report ");
    reason.push_str("this deployment's settings or this node's identity; the running service ");
    reason.push_str("prints its identity on its startup output, and its health surface reports ");
    reason.push_str("this unmanaged condition");
    let lines = [
        format!(
            "{UNMANAGED_LINE_PREFIX} {}",
            crate::settings_degraded::unmanaged_statement()
        ),
        format!(
            "{UNAVAILABLE_LINE_PREFIX} {PATH_KEY}={} {REASON_KEY}={reason}",
            store_path.display()
        ),
    ];
    format!("{}\n", lines.join("\n"))
}

/// The whole answer a command gives when the store it selected is being READ by
/// another process right now.
///
/// This is not the unreadable answer with softer words. A store held by readers
/// is healthy, managed, and free again the moment they finish — so the answer
/// says who is holding it, how many of them there are, and which store, and it
/// never tells an operator to go and repair working storage or that this node
/// is unmanaged. Those two conditions send a person to two completely different
/// places, and only one of them is a problem.
///
/// The count and the named readers come apart on purpose, exactly as the layers
/// below report them: a reader that was counted but could not be identified
/// still holds the store, so an answer that took its count from the list would
/// under-report who is in the way. Every reader the layers below DID identify
/// gets a line of its own, so a name with a space in it cannot run into the
/// next field.
pub fn render_busy_with_readers(
    store_path: &std::path::Path,
    observed_direct_readers: u64,
    readers: &[context_graph::ReaderIdentity],
) -> String {
    // Assembled from whole sentences rather than one wrapped literal: a
    // continued string literal carries its own source indentation into the
    // operator's line, and this line is read by a person.
    let mut reason = String::new();
    reason.push_str("another process is reading this store right now, so this command cannot ");
    reason.push_str("open it yet; the store itself is healthy and nothing needs repairing, and ");
    reason.push_str("the same command answers as soon as the readers below finish");
    let mut lines = vec![format!(
        "{BUSY_LINE_PREFIX} {PATH_KEY}={} {READER_LINE_PREFIX}s={observed_direct_readers} \
         {REASON_KEY}={reason}",
        store_path.display()
    )];
    lines.extend(readers.iter().map(|reader| {
        format!(
            "{READER_LINE_PREFIX} pid={} process={}",
            reader.process_id, reader.process_name
        )
    }));
    format!("{}\n", lines.join("\n"))
}

/// The degraded answer as one operator-surface response, rendered through the
/// same projection every other answer goes through.
pub fn render_degraded(data_dir: &std::path::Path) -> String {
    format!("{}\n", report_degraded(data_dir).render_lines().join("\n"))
}

/// The domains as they stand on a deployment nobody has configured: on, which
/// is their default, each naming what it governs and what vigil currently
/// chooses for it.
fn automatic_domain_views() -> Vec<DomainView> {
    crate::settings_domains::domain_roster()
        .into_iter()
        .map(|domain| DomainView {
            switch: domain.switch.to_string(),
            on: true,
            members: domain
                .members
                .iter()
                .filter_map(|member| {
                    let (value, reason) = crate::settings_backends::automatic_default(member)?;
                    // Read as one act, and both halves from the same read: this
                    // answer is given while the store cannot be, and a machine
                    // that has moved onto an accelerated backend has still
                    // moved. Naming the pre-move choice here, or keeping the
                    // pre-move sentence beside a moved one, describes a
                    // deployment nobody has.
                    let _published = crate::settings_application::hold_publication();
                    Some(crate::settings_domains::DomainMemberChoice {
                        setting: (*member).to_string(),
                        current_choice: automatic_choice(member, Author::Automatic, &value),
                        reason: automatic_reason(member, Author::Automatic, &value, &reason),
                    })
                })
                .collect(),
        })
        .collect()
}

/// Every declared domain as the operator surface renders it: whether it is on,
/// what it governs, and what Vigil currently chooses for each member.
fn domain_views(
    store: &crate::settings_store::SettingsStore,
    target: &crate::settings_model::ScopeTarget,
) -> Result<Vec<DomainView>, SettingsError> {
    let mut views = Vec::new();
    for domain in crate::settings_domains::domain_roster() {
        let switch = store.resolve(domain.switch, target)?;
        let mut members = Vec::new();
        for member in &domain.members {
            let effective = store.resolve(member, target)?;
            members.push(crate::settings_domains::DomainMemberChoice {
                setting: (*member).to_string(),
                current_choice: effective.requested.clone(),
                reason: effective.reason.clone(),
            });
        }
        views.push(DomainView {
            switch: domain.switch.to_string(),
            on: matches!(
                switch.requested,
                crate::settings_model::SettingValue::Bool(true)
            ),
            members,
        });
    }
    Ok(views)
}

/// What this process is actually using for one setting, recorded as the
/// runtime applies it. In-process by construction: "running" is a fact about
/// one live process, not something a store read can answer.
static RUNNING_VALUES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<String, crate::settings_model::SettingValue>>,
> = std::sync::OnceLock::new();

fn running_registry() -> &'static std::sync::Mutex<
    std::collections::BTreeMap<String, crate::settings_model::SettingValue>,
> {
    RUNNING_VALUES.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}

/// Record what this process actually applied for one setting. Called by the
/// runtime as it brings each value into force, so the surface can answer
/// requested and running side by side without either standing in for the other.
pub fn record_running(setting: &str, value: crate::settings_model::SettingValue) {
    if let Ok(mut running) = running_registry().lock() {
        running.insert(setting.to_string(), value);
    }
}

/// What this process applied for one setting, if it applied anything.
pub fn running_value(setting: &str) -> Option<crate::settings_model::SettingValue> {
    running_registry()
        .lock()
        .ok()
        .and_then(|running| running.get(setting).cloned())
}

/// What automatic management currently chooses for one setting, as the surface
/// reports it.
///
/// Two authorities meet here, and this is the seam between them. The store owns
/// the operator's choice: where a person authored a value, that value is what
/// the surface answers with, and a running value that differs is a gap the
/// pending field explains. Where nobody authored one AND the setting is a value
/// automatic management picks for itself — the backends its domains govern —
/// the choice is not a stored floor waiting to be taken on: it is DECIDED by
/// what the machine entered, so the choice reported is the backend running. A
/// surface that kept naming the pre-move backend after the detector moved would
/// name a backend no frame goes through, and show a live backend as waiting for
/// a restart.
///
/// Nothing else follows the running value. A floor value this artifact would
/// pick — a sensitivity, a frame count — is a real request that a differing
/// running value is genuinely a gap against, and an environment lever is
/// deliberately standing in front of the value this deployment would otherwise
/// run, with the line already naming the lever and what it shadows.
pub fn automatic_choice(
    setting: &str,
    author: Author,
    stored_choice: &SettingValue,
) -> SettingValue {
    promoted_choice(setting, author).unwrap_or_else(|| stored_choice.clone())
}

/// The value automatic management entered for a setting it decides for itself,
/// when that is what the reported choice is. The one place the conditions for
/// following the running value are stated, so the choice and the sentence
/// beside it cannot be answered from two different reads.
fn promoted_choice(setting: &str, author: Author) -> Option<SettingValue> {
    if author != Author::Automatic
        || running_source(setting).is_some()
        || crate::settings_domains::governing_domain(setting).is_none()
    {
        return None;
    }
    running_value(setting)
}

/// Why automatic management is on the choice it reports.
///
/// The reason travels with the choice. Where the choice is DECIDED by what the
/// machine entered, the floor's stored sentence describes the deployment before
/// the move — it says no accelerated backend has proved itself, beside the
/// accelerated backend that just did — so the sentence is derived from the same
/// read the value came from. Where nothing moved, the floor's own reason is the
/// truth and is answered unchanged.
pub fn automatic_reason(
    setting: &str,
    author: Author,
    stored_choice: &SettingValue,
    stored_reason: &str,
) -> String {
    match promoted_choice(setting, author) {
        Some(entered) if &entered != stored_choice => format!(
            "{entered} proved itself on this machine, and automatic management moved this node \
             onto it"
        ),
        _ => stored_reason.to_string(),
    }
}

/// Where a running value came from, when it did not come from the store.
///
/// A handful of environment variables are deliberate pressure levers rather
/// than settings, and one of them names a depth the operator can also pin. A
/// lever that quietly wins would have the surface report the operator's own
/// setting as running at a number they never chose, so the lever says so: the
/// variable that supplied the value, and the pin it is standing in front of.
#[derive(Debug, Clone, PartialEq)]
pub struct RunningSource {
    pub variable: String,
    pub shadowed: Option<crate::settings_model::SettingValue>,
}

static RUNNING_SOURCES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<String, RunningSource>>,
> = std::sync::OnceLock::new();

fn running_source_registry()
-> &'static std::sync::Mutex<std::collections::BTreeMap<String, RunningSource>> {
    RUNNING_SOURCES.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}

/// Record that an environment lever, not the store, supplied what this process
/// is running one setting at — together with the stored value it is standing in
/// front of, when there was one.
pub fn record_running_from_environment(
    setting: &str,
    variable: &str,
    value: crate::settings_model::SettingValue,
    shadowed: Option<crate::settings_model::SettingValue>,
) {
    record_running(setting, value);
    if let Ok(mut sources) = running_source_registry().lock() {
        sources.insert(
            setting.to_string(),
            RunningSource {
                variable: variable.to_string(),
                shadowed,
            },
        );
    }
}

/// Where this process's running value for one setting came from, when it came
/// from somewhere other than the store.
pub fn running_source(setting: &str) -> Option<RunningSource> {
    running_source_registry()
        .lock()
        .ok()
        .and_then(|sources| sources.get(setting).cloned())
}
