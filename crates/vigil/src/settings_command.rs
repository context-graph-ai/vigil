//! The `vigil settings` surface: one answer, rendered through the one
//! projection, whether it is served by the running runtime over its store's own
//! owner route or read directly from the store of a stopped deployment.
//!
//! With one honest exception. A command whose selected store cannot be READ has
//! nothing of this deployment to render, and says so instead
//! ([`crate::settings_projection::render_unreadable`]); the running node keeps
//! answering for itself, because it knows what it resolved. A listing said that
//! way was still SERVED and is delivered as a success; an `identity` read was
//! not — no answer to the question exists here — so it keeps the failed status
//! and the error stream a caller reads before any of the words.
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

/// Answer one `vigil settings` request for the deployment at `data_dir`, whose
/// store is the file at `store_path`.
///
/// The two are carried separately on purpose. `data_dir` is where this
/// deployment's own files are — the node key its records are written under, the
/// identity cache — and it stays exactly the directory the runtime resolved.
/// `store_path` is the exact file the runtime opened, which an operator may
/// have configured anywhere: answering about a file rebuilt by joining a
/// default filename onto the data directory would read, and CREATE, a store
/// nobody configured.
///
/// `request` is the command line after `settings`: empty for the listing,
/// `set <name> <value>`, `reset <name>`, or `find <text>`.
pub fn answer(data_dir: &Path, store_path: &Path, request: &str) -> String {
    match crate::settings_reflection::ContainerSupervisorClient::from_environment() {
        Ok(client) => answer_with_reflection(data_dir, store_path, request, &client),
        // No Supervisor to mirror onto: a bare binary, a plain container, or a
        // systemd service has no options file. The change still lands — the
        // store is the authority and reflection is a courtesy to the surface.
        Err(_) => answer_without_reflection(data_dir, store_path, request),
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
    store_path: &Path,
    request: &str,
    client: &dyn crate::settings_reflection::SupervisorOptionsClient,
) -> String {
    let answer = answer_without_reflection(data_dir, store_path, request);
    // Only a change that actually landed is mirrored: a refused or failed
    // request changed nothing, and mirroring it would put a value on the page
    // that this deployment is not holding.
    let Some(setting) = changed_setting(request) else {
        return answer;
    };
    if answer_failed(&answer) {
        return answer;
    }
    let mirrored = SettingsStore::open_existing_for_change(store_path).and_then(|store| {
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

fn answer_without_reflection(data_dir: &Path, store_path: &Path, request: &str) -> String {
    match run(data_dir, store_path, request) {
        Ok(answer) => answer,
        // The failure this request's OWN open returned is what the answer is
        // built from. Asking the store again would answer about a different
        // moment — and when the two disagreed, this line printed the very error
        // it had already decided was not good enough to answer with.
        Err(error) => degraded_answer(data_dir, store_path, request, &error)
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
fn degraded_answer(
    data_dir: &Path,
    store_path: &Path,
    request: &str,
    error: &SettingsError,
) -> Option<String> {
    // `None` means the store opened and then refused something — a governed
    // value, a rank this handle may not write at. That is not a store this
    // command could not read, and answering as though it were would hide a
    // decision the store actually made.
    let class = crate::settings_degraded::classify_open_failure(error)?;
    // Nothing has ever run in this directory. A read never arrives here — the
    // projection answers a never-started deployment with what it WOULD run at —
    // so this is a CHANGE against a deployment that does not exist yet, and the
    // ruled answer for that names the directory and says to start the runtime,
    // rather than sending the operator to repair a store nobody ever created.
    if matches!(class, crate::settings_degraded::StoreOpenClass::Absent) {
        return Some(crate::settings_degraded::never_started_refusal(
            crate::settings_degraded::UnavailableCapability::SettingsChange,
            data_dir,
            store_path,
        ));
    }
    // Somebody is READING this store. Nothing about this deployment is
    // degraded: the store is healthy, this process simply could not get in
    // yet, and the answer says who is holding it and that asking again works.
    // Every read-only shape gets that same answer, including the narrowed one
    // — an explanation is easiest to lose where the operator asked for less.
    if let crate::settings_degraded::StoreOpenClass::HeldByReaders {
        observed_direct_readers,
        readers,
        path,
    } = &class
    {
        return Some(busy_answer(
            request,
            *observed_direct_readers,
            readers,
            path.as_path(),
        ));
    }
    if !matches!(
        class,
        crate::settings_degraded::StoreOpenClass::Unreadable { .. }
    ) {
        return None;
    }
    let request = request.trim();
    let mut parts = request.splitn(2, ' ');
    let verb = parts.next().unwrap_or_default().trim();
    let rest = parts.next().unwrap_or_default().trim();
    // Whether this process can answer for the deployment at all, and the ONLY
    // thing that decides it: a run that resolved its own identity is answering
    // about itself and knows what it resolved. Any other process could not read
    // the store and has nothing of this deployment's to report.
    let is_the_run = crate::service_identity::in_force().is_some();
    let read = matches!(verb, "" | "list" | "find")
        // An identity READ is a read. Only the deliberate change is a change,
        // and it keeps the refusal below.
        || (verb == "identity" && rest.is_empty());
    Some(match (read, is_the_run) {
        // An identity read nobody can serve is a FAILED request, and says so.
        (true, false) if verb == "identity" => unreadable_identity_answer(store_path),
        // A `find` request narrows an answer. There is no answer to narrow
        // here, and the explanation is not a row of one: it is the answer, so
        // it survives whatever the operator asked to search.
        (true, false) => crate::settings_projection::render_unreadable(store_path),
        (true, true) if verb == "identity" => crate::settings_projection::report_degraded(data_dir)
            .render_identity_line()
            .map(|line| format!("{line}\n"))
            .unwrap_or_else(|| unreadable_identity_answer(store_path)),
        (true, true) if verb == "find" => {
            filtered(&crate::settings_projection::render_degraded(data_dir), rest)
        }
        (true, true) => crate::settings_projection::render_degraded(data_dir),
        (false, _) => crate::settings_degraded::refusal_line(
            crate::settings_degraded::UnavailableCapability::SettingsChange,
        ),
    })
}

/// The answer every read-only settings shape gives while readers hold the
/// store, and the refusal a change gets.
///
/// One busy statement, whatever was asked. A `find` narrows an answer and
/// there is no answer here; a listing has nothing to list; and a change was
/// not made, so it refuses. `identity` keeps the delivery it was ruled into —
/// an operator asked this node what it is called and no answer exists to give
/// them yet, so it stays a FAILED request carrying the failure marker, and the
/// caller prints it on the error stream and exits 2. What none of them do is
/// render a settings, domain, identity or unmanaged line: this process never
/// read the store, so every one of those would be invented.
fn busy_answer(
    request: &str,
    observed_direct_readers: u64,
    readers: &[context_graph::ReaderIdentity],
    store_path: &Path,
) -> String {
    let request = request.trim();
    let mut parts = request.splitn(2, ' ');
    let verb = parts.next().unwrap_or_default().trim();
    let rest = parts.next().unwrap_or_default().trim();
    let busy = crate::settings_projection::render_busy_with_readers(
        store_path,
        observed_direct_readers,
        readers,
    );
    let is_read = matches!(verb, "" | "list" | "find") || (verb == "identity" && rest.is_empty());
    if !is_read {
        // A change nothing recorded is not a change, so it stays a failed
        // request whatever kept the store shut — only the explanation differs.
        return format!(
            "{SETTINGS_ERROR_PREFIX} this change was not made: another process is reading the \
             store\n{busy}"
        );
    }
    if verb == "identity" {
        return format!(
            "{SETTINGS_ERROR_PREFIX} this node's identity is not available while another \
             process is reading the store\n{busy}"
        );
    }
    busy
}

/// The honest explanation, delivered as the failed request an identity read is.
///
/// A read that WAS served is a success whatever it has to say, and a read
/// nobody could serve is not — and `identity` is the shape where that
/// distinction is load-bearing. An operator asks this node what it is called,
/// this process cannot see the store, and an answer leaving on standard output
/// with status 0 tells every script that consumed it that the lookup SUCCEEDED
/// and this deployment has no identity. That is a different statement, and a
/// false one. So the ruled explanation is the text, and the failure marker
/// carries the status: the caller prints it on the error stream and exits 2,
/// exactly as an identity read against an unreadable store always did. The
/// listing shapes keep the delivery they have — they were served.
fn unreadable_identity_answer(store_path: &Path) -> String {
    format!(
        "{SETTINGS_ERROR_PREFIX} this node's identity is not available through this command\n{}",
        crate::settings_projection::render_unreadable(store_path)
    )
}

fn run(data_dir: &Path, store_path: &Path, request: &str) -> Result<String, SettingsError> {
    let request = request.trim();
    let mut parts = request.splitn(2, ' ');
    let verb = parts.next().unwrap_or_default().trim();
    let rest = parts.next().unwrap_or_default().trim();

    match verb {
        "" | "list" => render(&report(data_dir, store_path)?),
        "set" => {
            let mut pieces = rest.splitn(2, ' ');
            let name = pieces.next().unwrap_or_default().trim();
            let value = pieces.next().unwrap_or_default().trim();
            if name.is_empty() || value.is_empty() {
                return Ok(format!(
                    "{SETTINGS_ERROR_PREFIX} usage: vigil settings set <setting> <value>\n"
                ));
            }
            // Decided before any door is opened, so it is the same answer on a
            // running node, a stopped one, and one that has never started: an
            // operator aiming an ordinary verb at this node's identity is owed
            // the orphaned-history consequence, never a store-open failure that
            // says nothing about what they tried to do.
            crate::settings_store::refuse_setting_no_ordinary_verb_may_touch(name, value)?;
            let store = SettingsStore::open_existing_for_change(store_path)?;
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
            render(&report(data_dir, store_path)?)
        }
        "reset" => {
            if rest.is_empty() {
                return Ok(format!(
                    "{SETTINGS_ERROR_PREFIX} usage: vigil settings reset <setting>\n"
                ));
            }
            // The same gate, before the same door: dropping the identity back
            // moves it exactly as setting it does, and both verbs owe the one
            // consequence whatever state this deployment's store is in.
            crate::settings_store::refuse_setting_no_ordinary_verb_may_touch(rest, "")?;
            let store = SettingsStore::open_existing_for_change(store_path)?;
            let outcome =
                store.reset_local(rest, Surface::VigilSettings, &recording_scope(data_dir))?;
            crate::settings_cache::refresh(&store, data_dir, &target(data_dir));
            // A withdrawal on the running node takes effect on it, exactly as a
            // change does. It names the setting it withdrew, because what a
            // reset leaves in force is authored by nobody — and the ordinary
            // pass leaves an unauthored setting alone, which is what left the
            // withdrawn value running until a restart.
            crate::settings_application::apply_live_withdrawal(&store, &target(data_dir), rest);
            drop(store);
            let mut rendered = format!("reset {}\n", outcome.statement);
            rendered.push_str(&render(&report(data_dir, store_path)?)?);
            Ok(rendered)
        }
        "identity" => identity_request(data_dir, store_path, rest),
        // SEARCH over the whole rendered answer, and deliberately so: an
        // operator looking for what a line SAYS finds it, not only what it is
        // named. The old spelling of this verb promised a declared grouping and
        // did exactly this, which is how `machine` came back with two settings.
        "find" => {
            // Nothing was asked for, and the whole listing is not what was
            // asked for either. Decided before the store is touched, so the
            // answer is the same whatever state the store is in.
            if rest.is_empty() {
                return Ok(format!(
                    "{SETTINGS_ERROR_PREFIX} usage: vigil settings find <text>\n"
                ));
            }
            let report = report(data_dir, store_path)?;
            let lines: Vec<String> = report
                .render_lines()
                .into_iter()
                .filter(|line| line.contains(rest))
                .collect();
            // A search that matched nothing SUCCEEDED and found nothing. It is
            // not a refusal and it is not the whole listing: both would answer
            // a different question from the one asked.
            if lines.is_empty() {
                return Ok(String::new());
            }
            Ok(format!("{}\n", lines.join("\n")))
        }
        other => Ok(format!(
            "{SETTINGS_ERROR_PREFIX} unknown settings command {other}; \
             use `vigil settings`, `list`, `find <text>`, `set`, `reset`, or `identity`\n"
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
fn identity_request(
    data_dir: &Path,
    store_path: &Path,
    rest: &str,
) -> Result<String, SettingsError> {
    let mut pieces = rest.split_whitespace();
    let verb = pieces.next().unwrap_or_default();
    let proposed = pieces.next().unwrap_or_default();
    let confirmed = pieces.any(|piece| piece == CONFIRM_FLAG) || proposed == CONFIRM_FLAG;

    match verb {
        "" => {
            let report = report(data_dir, store_path)?;
            // A report with no identity line is a process that cannot see one.
            // It says so in the words the unreadable answer uses everywhere
            // else rather than printing an empty line or inventing a value to
            // fill it — which is the whole thing the honest answer exists to
            // stop.
            Ok(report.render_identity_line().map_or_else(
                || crate::settings_projection::render_unreadable(store_path),
                |line| format!("{line}\n"),
            ))
        }
        "change" if !proposed.is_empty() && proposed != CONFIRM_FLAG => {
            let consequence =
                crate::service_identity::describe_change(data_dir, store_path, proposed)?;
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
            let changed =
                crate::service_identity::change_deliberately(data_dir, store_path, proposed)?;
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

/// The lines of `rendered` a `find` request asked for. An empty `filter` is
/// the whole answer, which is what the degraded path hands back when there is
/// nothing to narrow.
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

fn report(data_dir: &Path, store_path: &Path) -> Result<SettingsReport, SettingsError> {
    report_by_direct_read_at(data_dir, store_path, &target(data_dir))
}

fn render(report: &SettingsReport) -> Result<String, SettingsError> {
    Ok(format!("{}\n", report.render_lines().join("\n")))
}
