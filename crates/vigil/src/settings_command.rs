//! The `vigil settings` surface: one answer, rendered through the one
//! projection, whether it is served by the running runtime over its control
//! socket or read directly from the store of a stopped deployment.
//!
//! There is no second rendering and no second resolution. The runtime answers
//! because it owns the store while it runs; a stopped deployment answers from
//! the store itself, and the requested value is answerable either way. What
//! differs between the two is `running`, which is a fact about a live process
//! and nothing a store read can invent.

use std::path::Path;

use crate::settings_model::{Scope, ScopeTarget, SettingValue, SettingsError, Surface};
use crate::settings_projection::{SettingsReport, report_by_direct_read_at};
use crate::settings_store::SettingsStore;

/// The marker a refused or failed settings answer carries on its first line, so
/// a caller sets a failing exit status without parsing prose. A refusal is a
/// real answer — it names its cause and its remedy — but it is never a success.
pub const SETTINGS_ERROR_PREFIX: &str = "settings-error";

/// Whether an answer reports a refusal or a failure.
pub fn answer_failed(answer: &str) -> bool {
    answer
        .lines()
        .any(|line| line.trim_start().starts_with(SETTINGS_ERROR_PREFIX))
}

/// The deployment this answer is about. One helper, so a value written by
/// `settings set` and a value read back by `settings` are resolved at the same
/// tenant, site and node rather than at two subtly different ones.
fn deployment_name(data_dir: &Path) -> String {
    crate::node_key::recorded(data_dir).unwrap_or_else(|| crate::node_key::fallback_name(data_dir))
}

/// The name a WRITE from this surface records under. A write is the moment
/// this node needs a key of its own, so this is the one place that generates
/// one; a read never does, because answering a deployment that has never
/// started must leave its directory exactly as it was.
fn recording_scope(data_dir: &Path) -> Scope {
    Scope::node(crate::node_key::scope_name(data_dir))
}

fn target(data_dir: &Path) -> ScopeTarget {
    let name = deployment_name(data_dir);
    ScopeTarget {
        tenant: name.clone(),
        site: name.clone(),
        node: name,
        camera: None,
    }
}

/// A value as an operator typed it. Bare `true`/`false` is a switch, a bare
/// number is a number, a comma-separated list is a list, and anything else is
/// text — so `settings set accelerated_detection false` means the switch and
/// not the word.
fn parse_value(raw: &str) -> SettingValue {
    let trimmed = raw.trim();
    match trimmed {
        "true" => return SettingValue::Bool(true),
        "false" => return SettingValue::Bool(false),
        _ => {}
    }
    if let Ok(number) = trimmed.parse::<i64>() {
        return SettingValue::Int(number);
    }
    if let Ok(number) = trimmed.parse::<f64>() {
        return SettingValue::Float(number);
    }
    if trimmed.contains(',') {
        return SettingValue::list(trimmed.split(',').map(str::trim));
    }
    SettingValue::text(trimmed)
}

/// Answer one `vigil settings` request against `data_dir`.
///
/// `request` is the command line after `settings`: empty for the listing,
/// `set <name> <value>`, `reset <name>`, or `domain <name>`.
pub fn answer(data_dir: &Path, request: &str) -> String {
    match crate::settings_reflection::ContainerSupervisorClient::from_environment() {
        Ok(client) => answer_with_reflection(data_dir, request, &client),
        // No Supervisor to mirror onto: a bare binary, a plain container, or a
        // systemd service has no options file. The change still lands — the
        // store is the authority and reflection is a courtesy to the surface.
        Err(_) => answer_without_reflection(data_dir, request),
    }
}

/// The same answer, reflecting through `client`.
///
/// A local change made here reflects immediately, exactly as a pushed value
/// does: saving add-on options never restarts the add-on and never reaches the
/// running container, so mirroring costs nothing and the page the operator
/// trusts stops disagreeing with the store the moment the change lands.
pub fn answer_with_reflection(
    data_dir: &Path,
    request: &str,
    client: &dyn crate::settings_reflection::SupervisorOptionsClient,
) -> String {
    let answer = answer_without_reflection(data_dir, request);
    // Only a change that actually landed is mirrored: a refused or failed
    // request changed nothing, and mirroring it would put a value on the page
    // that this deployment is not holding.
    let Some(setting) = changed_setting(request) else {
        return answer;
    };
    if answer_failed(&answer) {
        return answer;
    }
    let mirrored = SettingsStore::open(data_dir).and_then(|store| {
        crate::settings_reflection::reflect_landed_change(
            &store,
            data_dir,
            &target(data_dir),
            client,
            &setting,
        )
    });
    match mirrored {
        // A mirror that did not happen is said out loud, on its own line and
        // never as an error: the change landed, the store is the authority, and
        // what failed is the courtesy the surface gets.
        Ok(crate::settings_reflection::ReflectionOutcome::NotAchieved(failure)) => {
            format!(
                "{answer}{}",
                unmirrored_line(&setting, &format!("{failure:?}"))
            )
        }
        Err(error) => format!("{answer}{}", unmirrored_line(&setting, &error.to_string())),
        Ok(_) => answer,
    }
}

/// The line naming a change that landed and did not reach the add-on options.
/// Deliberately not the error prefix: the operator's change IS in force.
const UNMIRRORED_LINE_PREFIX: &str = "reflection-not-achieved";

fn unmirrored_line(setting: &str, reason: &str) -> String {
    format!("{UNMIRRORED_LINE_PREFIX} setting={setting} reason={reason}\n")
}

/// The setting one request changes, or nothing for a request that only reads.
fn changed_setting(request: &str) -> Option<String> {
    let request = request.trim();
    let mut parts = request.splitn(2, ' ');
    let verb = parts.next().unwrap_or_default().trim();
    let rest = parts.next().unwrap_or_default().trim();
    match verb {
        "set" => rest
            .split_whitespace()
            .next()
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        "reset" => (!rest.is_empty()).then(|| rest.to_string()),
        _ => None,
    }
}

fn answer_without_reflection(data_dir: &Path, request: &str) -> String {
    match run(data_dir, request) {
        Ok(answer) => answer,
        Err(error) => degraded_answer(data_dir, request)
            .unwrap_or_else(|| format!("{SETTINGS_ERROR_PREFIX} {error}\n")),
    }
}

/// The answer this surface owes when the store exists and cannot be read.
///
/// The two halves of the surface part company there, and both halves matter:
/// the listing still answers, because that is where the unmanaged statement
/// lives and an operator reading it is exactly who needs to be told; every
/// change refuses, because a change nothing recorded is not a change. The
/// classification runs only after an ordinary answer has already failed, so a
/// healthy deployment pays nothing for it — and a store held by the running
/// runtime classifies as locked, not unreadable, so a live node's own reads
/// never take this path.
fn degraded_answer(data_dir: &Path, request: &str) -> Option<String> {
    if !matches!(
        crate::settings_degraded::classify_store_open(data_dir),
        Ok(crate::settings_degraded::StoreOpenClass::Unreadable { .. })
    ) {
        return None;
    }
    let request = request.trim();
    let mut parts = request.splitn(2, ' ');
    let verb = parts.next().unwrap_or_default().trim();
    let rest = parts.next().unwrap_or_default().trim();
    Some(match verb {
        "" | "list" => crate::settings_projection::render_degraded(data_dir),
        "domain" => filtered(&crate::settings_projection::render_degraded(data_dir), rest),
        _ => crate::settings_degraded::refusal_line(
            crate::settings_degraded::UnavailableCapability::SettingsChange,
        ),
    })
}

fn run(data_dir: &Path, request: &str) -> Result<String, SettingsError> {
    let request = request.trim();
    let mut parts = request.splitn(2, ' ');
    let verb = parts.next().unwrap_or_default().trim();
    let rest = parts.next().unwrap_or_default().trim();

    match verb {
        "" | "list" => render(&report(data_dir)?),
        "set" => {
            let mut pieces = rest.splitn(2, ' ');
            let name = pieces.next().unwrap_or_default().trim();
            let value = pieces.next().unwrap_or_default().trim();
            if name.is_empty() || value.is_empty() {
                return Ok(format!(
                    "{SETTINGS_ERROR_PREFIX} usage: vigil settings set <setting> <value>\n"
                ));
            }
            let store = SettingsStore::open(data_dir)?;
            store.set_local(
                name,
                Surface::VigilSettings,
                recording_scope(data_dir),
                parse_value(value),
            )?;
            crate::settings_cache::refresh(&store, data_dir, &target(data_dir));
            // A change made on the running node takes effect on it: this is the
            // process the cameras are running in, so the values that apply live
            // are brought into force before the answer is rendered and the
            // operator reads what is actually running.
            crate::settings_application::apply_live_change(&store, &target(data_dir));
            drop(store);
            render(&report(data_dir)?)
        }
        "reset" => {
            if rest.is_empty() {
                return Ok(format!(
                    "{SETTINGS_ERROR_PREFIX} usage: vigil settings reset <setting>\n"
                ));
            }
            let store = SettingsStore::open(data_dir)?;
            let outcome =
                store.reset_local(rest, Surface::VigilSettings, &recording_scope(data_dir))?;
            crate::settings_cache::refresh(&store, data_dir, &target(data_dir));
            crate::settings_application::apply_live_change(&store, &target(data_dir));
            drop(store);
            let mut rendered = format!("reset {}\n", outcome.statement);
            rendered.push_str(&render(&report(data_dir)?)?);
            Ok(rendered)
        }
        "identity" => identity_request(data_dir, rest),
        "domain" => {
            let report = report(data_dir)?;
            let lines: Vec<String> = report
                .render_lines()
                .into_iter()
                .filter(|line| rest.is_empty() || line.contains(rest))
                .collect();
            Ok(format!("{}\n", lines.join("\n")))
        }
        other => Ok(format!(
            "{SETTINGS_ERROR_PREFIX} unknown settings command {other}; \
             use `vigil settings`, `set`, `reset`, `domain`, or `identity`\n"
        )),
    }
}

/// The word that makes an identity change deliberate. Without it the operation
/// states its consequence and changes nothing, which is the whole point: the
/// consequence is read BEFORE the identity moves, never after.
const CONFIRM_FLAG: &str = "--confirm";

/// The deliberate identity-change operation, and the read-only view beside it.
///
/// This node's identity is not an ordinary setting — `settings set` refuses it,
/// naming the same orphaned-history consequence — so moving it has its own
/// operation, and that operation states what it will do before it does it.
fn identity_request(data_dir: &Path, rest: &str) -> Result<String, SettingsError> {
    let mut pieces = rest.split_whitespace();
    let verb = pieces.next().unwrap_or_default();
    let proposed = pieces.next().unwrap_or_default();
    let confirmed = pieces.any(|piece| piece == CONFIRM_FLAG) || proposed == CONFIRM_FLAG;
    let store_path = crate::settings_store::SettingsStore::store_path(data_dir);

    match verb {
        "" => {
            let report = report(data_dir)?;
            Ok(format!("{}\n", report.render_identity_line()))
        }
        "change" if !proposed.is_empty() && proposed != CONFIRM_FLAG => {
            let consequence = crate::service_identity::describe_change(&store_path, proposed)?;
            if !confirmed {
                // Nothing moved, so this is not a success: the operator asked
                // for a change and got the consequence to read first.
                return Ok(format!(
                    "{SETTINGS_ERROR_PREFIX} not applied: {}. Re-run with {CONFIRM_FLAG} if that \
                     is genuinely what you want.\n",
                    consequence.statement
                ));
            }
            println!("{}", consequence.statement);
            let changed = crate::service_identity::change_deliberately(&store_path, proposed)?;
            Ok(format!(
                "identity-changed from={} to={} — {}. It takes effect at the next start.\n",
                consequence.current, changed.value, consequence.statement
            ))
        }
        _ => Ok(format!(
            "{SETTINGS_ERROR_PREFIX} usage: vigil settings identity, or \
             vigil settings identity change <identifier> {CONFIRM_FLAG}\n"
        )),
    }
}

/// The lines of `rendered` a `domain` request asked for. Empty `filter` means
/// the whole answer, which is what `vigil settings domain` with no name asks
/// for.
fn filtered(rendered: &str, filter: &str) -> String {
    if filter.is_empty() {
        return rendered.to_string();
    }
    let lines: Vec<&str> = rendered
        .lines()
        .filter(|line| line.contains(filter))
        .collect();
    format!("{}\n", lines.join("\n"))
}

fn report(data_dir: &Path) -> Result<SettingsReport, SettingsError> {
    report_by_direct_read_at(data_dir, &target(data_dir))
}

fn render(report: &SettingsReport) -> Result<String, SettingsError> {
    Ok(format!("{}\n", report.render_lines().join("\n")))
}
