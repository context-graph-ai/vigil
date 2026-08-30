//! This node's service identity: derived once at first start and then
//! persisted, because it names the messaging topics and the Home Assistant
//! device, so recomputing it from the site name renames the device and orphans
//! the history hanging off it.
//!
//! The store record is authoritative. A read-only sidecar under the data
//! directory is a NON-AUTHORITATIVE cache of that record, written only during
//! store-backed operation; on any mismatch the store record is truth. A
//! storeless first start derives the identity ephemerally and persists nothing;
//! persistence happens at the first successful store open.
//!
//! How the identity was arrived at is read off the record's AUTHOR — a typed,
//! persisted property — never off its human-readable reason, which is prose
//! written for a person and gets reworded.

use std::path::Path;

use crate::settings_model::{
    Author, Refusal, RefusalKind, SERVICE_IDENTITY_SETTING, Scope, ScopeTarget, SettingRecord,
    SettingValue, SettingsError, Surface,
};
use crate::settings_store::SettingsStore;

/// How the identity in force was arrived at, shown read-only beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityDerivation {
    /// Derived at first start and persisted.
    DerivedAtFirstStart,
    /// Set explicitly through the deliberate change operation.
    SetExplicitly,
    /// Derived for this run only, with no store to persist it into. The surface
    /// shows this alongside the continuous unmanaged statement.
    DerivedNotYetPersisted,
    /// Named by the deployment, with no store to persist it into. Kept apart
    /// from the derived case because the two are different facts: one value
    /// nobody chose was worked out from the site name, the other was stated by
    /// the person who installed this node, and reporting the second as derived
    /// tells that person their own choice was ignored.
    ConfiguredNotYetPersisted,
}

/// The identity in force, with how it was arrived at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceIdentity {
    pub value: String,
    pub derivation: IdentityDerivation,
}

/// Where a first start takes the identity it persists from. Later starts read
/// the persisted record and never consult a seed at all.
enum Seed<'a> {
    /// Nothing named this node, so its identity is derived from the site name
    /// it was installed under — once, and then never again.
    SiteName(&'a str),
    /// The deployment itself named this node before anything opened, which is
    /// as explicit as an identity gets.
    Configured(&'a str),
}

/// Resolve the identity for a store-backed start: the persisted value if one
/// exists, otherwise derive it once from the site name and persist it.
pub fn resolve_persisted(
    data_dir: &Path,
    store_path: &Path,
    site_name: &str,
) -> Result<ServiceIdentity, SettingsError> {
    resolve_seeded(data_dir, store_path, Seed::SiteName(site_name))
}

/// Resolve the identity for a store-backed start where the deployment supplied
/// the identifier itself. The persisted record still wins — an identity that
/// has already named a Home Assistant device moves only through the deliberate
/// operation — so the supplied value seeds a FIRST start and nothing else.
pub fn resolve_persisted_configured(
    data_dir: &Path,
    store_path: &Path,
    configured: &str,
) -> Result<ServiceIdentity, SettingsError> {
    resolve_seeded(data_dir, store_path, Seed::Configured(configured))
}

/// The deployment's own directory and its store file are BOTH given, never one
/// worked out from the other: the identity record lives in the store the
/// runtime resolved, while the scope it is written at and the sidecar cache
/// belong to the deployment directory the runtime resolved. Deriving the
/// directory from the store's parent moved this node's whole identity — key,
/// cache and all — into whatever directory an operator happened to put their
/// store file in, and created a second store there to hold it.
fn resolve_seeded(
    data_dir: &Path,
    store_path: &Path,
    seed: Seed<'_>,
) -> Result<ServiceIdentity, SettingsError> {
    let store = SettingsStore::open_at(store_path)?;
    let identity = match persisted(&store, data_dir)? {
        Some(identity) => identity,
        None => {
            // First start: derive once, exactly as the storeless path would,
            // and persist that value. Deriving something else here would move
            // the Home Assistant device the moment the store came back.
            let (value, record) = match seed {
                Seed::SiteName(site_name) => {
                    let derived = derive(site_name);
                    let record = SettingRecord::automatic(
                        SERVICE_IDENTITY_SETTING,
                        identity_scope(data_dir),
                        SettingValue::text(&derived),
                        DERIVED_AT_FIRST_START_REASON,
                    );
                    (derived, record)
                }
                Seed::Configured(configured) => {
                    let record = SettingRecord::local(
                        SERVICE_IDENTITY_SETTING,
                        Surface::StartupOptions,
                        identity_scope(data_dir),
                        SettingValue::text(configured),
                        CONFIGURED_AT_FIRST_START_REASON,
                    );
                    (configured.to_string(), record)
                }
            };
            let derivation = derivation_of(record.author);
            store.write_record(record)?;
            ServiceIdentity { value, derivation }
        }
    };
    write_sidecar(data_dir, &identity)?;
    Ok(identity)
}

/// The reason an identity derived at first start carries.
const DERIVED_AT_FIRST_START_REASON: &str =
    "derived at first start from the site name, then persisted so a later rename cannot move it";

/// The reason an identity supplied by the deployment at first start carries.
const CONFIGURED_AT_FIRST_START_REASON: &str =
    "named by the deployment at first start, then persisted so a later start cannot move it";

/// The reason an identity moved by the deliberate operation carries.
const SET_EXPLICITLY_REASON: &str =
    "set explicitly through the deliberate identity-change operation";

/// The identity is a fact about the node itself, so it is recorded at the
/// deployment's own scope — never at a scope derived from the identity VALUE.
/// A value-derived scope would give every change its own record identity, so a
/// deliberate change would sit beside the old one instead of replacing it, and
/// a later start would read back whichever came first.
///
/// The scope is this node's recorded key (`node_key`), which is generated
/// before any record is written and never read out of the store. That order is
/// what lets the identity record be addressable: a scope taken from the
/// identity would need the identity to already exist to find the identity.
fn identity_scope(data_dir: &Path) -> Scope {
    Scope::node(crate::node_key::scope_name(data_dir))
}

/// The deployment an identity read is asked about: THIS node, named by the key
/// its own records are written under. Resolution filters on it exactly as the
/// ordinary settings path does, so a record another node wrote — which arrives
/// in the travelling table at that node's key, at the pushed rank, outranking
/// anything automatic — is not this node's identity and is never read as one.
/// A record written FOR this node, at this node's key, still outranks: rank is
/// compared only among the records that name this node.
///
/// Reading never generates a key: a deployment that has never started resolves
/// at its directory's name, which is where it wrote before a key existed and
/// where an unwritable directory still writes. So the never-started and
/// fallback reads keep resolving exactly what they resolved before.
fn identity_target(data_dir: &Path) -> ScopeTarget {
    let name = crate::node_key::recorded(data_dir)
        .unwrap_or_else(|| crate::node_key::fallback_name(data_dir));
    ScopeTarget {
        tenant: name.clone(),
        site: name.clone(),
        node: name,
        camera: None,
    }
}

/// How an identity was arrived at, read off the record's AUTHOR — a typed,
/// persisted property. A value someone stated is authored explicitly; one
/// Vigil derived for them is authored automatically. Recovering this by
/// matching the human-readable reason instead would re-read every stored
/// identity as automatic the moment that sentence was reworded, which tells an
/// operator their deliberate choice was made for them.
fn derivation_of(author: Author) -> IdentityDerivation {
    match author {
        Author::Automatic => IdentityDerivation::DerivedAtFirstStart,
        Author::LocalExplicit | Author::Pushed => IdentityDerivation::SetExplicitly,
    }
}

/// Which of two records is the identity in force. An explicitly-authored
/// record beats a derived one whatever order they come off storage and
/// whatever their stamps say, because the derived record is the value the
/// operator deliberately moved away from. Within one rank the newest wins.
fn precedence(record: &SettingRecord) -> (u8, i64) {
    let rank = match derivation_of(record.author) {
        IdentityDerivation::DerivedAtFirstStart
        | IdentityDerivation::DerivedNotYetPersisted
        | IdentityDerivation::ConfiguredNotYetPersisted => 0,
        IdentityDerivation::SetExplicitly => 1,
    };
    (rank, record.written_at_ms)
}

/// The identity recorded in the store for THIS node, if one is. The store
/// record is the authority; the sidecar under the data directory only caches
/// it. Records naming another node are not this node's identity — see
/// [`identity_target`].
pub fn persisted(
    store: &SettingsStore,
    data_dir: &Path,
) -> Result<Option<ServiceIdentity>, SettingsError> {
    let target = identity_target(data_dir);
    let records = store.records(SERVICE_IDENTITY_SETTING)?;
    let Some(record) = records
        .iter()
        .filter(|record| !record.reset && record.scope.covers(&target))
        .max_by_key(|record| precedence(record))
    else {
        return Ok(None);
    };
    Ok(Some(ServiceIdentity {
        value: record.value.to_string(),
        derivation: derivation_of(record.author),
    }))
}

/// The identity derived from a site name. Deterministic, so the value a
/// storeless run uses is exactly the value the first successful store open
/// persists.
fn derive(site_name: &str) -> String {
    site_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Resolve the identity for a storeless start. Derives ephemerally and persists
/// nothing — including no sidecar write.
pub fn resolve_ephemeral(site_name: &str) -> ServiceIdentity {
    ServiceIdentity {
        value: derive(site_name),
        derivation: IdentityDerivation::DerivedNotYetPersisted,
    }
}

/// The identity this process resolved for itself at startup, once it has
/// resolved one.
///
/// A run announces its identity on the broker topics and on its startup lines,
/// and answers for it again on the operator surface. Those are two reads of one
/// fact, so the second reads what the first resolved instead of working the
/// question out a second time from different inputs — which is how a run came
/// to announce one identifier and report another. A process that never started
/// a runtime (a plain command-line read) has nothing recorded here and resolves
/// the question its own way.
static IN_FORCE: std::sync::OnceLock<std::sync::Mutex<Option<ServiceIdentity>>> =
    std::sync::OnceLock::new();

fn in_force_slot() -> &'static std::sync::Mutex<Option<ServiceIdentity>> {
    IN_FORCE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Record the identity this run resolved, for every later read in this process.
pub fn record_in_force(identity: &ServiceIdentity) {
    if let Ok(mut slot) = in_force_slot().lock() {
        *slot = Some(identity.clone());
    }
}

/// The identity this run resolved, if this process is a run at all.
pub fn in_force() -> Option<ServiceIdentity> {
    in_force_slot().lock().ok().and_then(|slot| slot.clone())
}

/// Where the non-authoritative sidecar cache lives for a data directory, and
/// where the store it caches lives. One place fixes the relationship so a
/// caller never has to assume a filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPaths {
    pub data_dir: std::path::PathBuf,
    pub store: std::path::PathBuf,
    pub sidecar: std::path::PathBuf,
}

/// Resolve the identity-bearing paths for one data directory. The sidecar is
/// joined UNDER the data directory, the same way every other persisted file
/// is (`SettingsStore::store_path`, the startup-values cache) — the data
/// directory is the one path a deployment guarantees is writable; its parent
/// is not (a container's data directory is typically a mounted volume under a
/// read-only root, so a sidecar path built by string concatenation onto the
/// data directory's own name — `format!("{name}-service-identity")` joined
/// onto the PARENT — used to land beside it on that read-only root and fail
/// to persist).
pub fn identity_paths(data_dir: &Path) -> IdentityPaths {
    let sidecar = data_dir.join("service-identity");
    IdentityPaths {
        data_dir: data_dir.to_path_buf(),
        store: SettingsStore::store_path(data_dir),
        sidecar,
    }
}

/// Write the non-authoritative cache under the data directory. It is written
/// only during store-backed operation; the degraded path never reaches here.
fn write_sidecar(data_dir: &Path, identity: &ServiceIdentity) -> Result<(), SettingsError> {
    let paths = identity_paths(data_dir);
    let derivation = match identity.derivation {
        IdentityDerivation::DerivedAtFirstStart => "derived-at-first-start",
        IdentityDerivation::SetExplicitly => "set-explicitly",
        IdentityDerivation::DerivedNotYetPersisted
        | IdentityDerivation::ConfiguredNotYetPersisted => return Ok(()),
    };
    let contents = format!("{}\n{derivation}\n", identity.value);
    if let Ok(existing) = std::fs::read_to_string(&paths.sidecar)
        && existing == contents
    {
        // The cache already says exactly this. Rewriting it would be a write
        // nobody asked for, and a write is what a degraded run must never do.
        return Ok(());
    }
    std::fs::write(&paths.sidecar, contents).map_err(|error| {
        SettingsError::Store(format!(
            "could not write the service-identity cache {}: {error}",
            paths.sidecar.display()
        ))
    })
}

/// Read the non-authoritative sidecar cache, when one exists.
pub fn read_sidecar(data_dir: &Path) -> Option<ServiceIdentity> {
    let contents = std::fs::read_to_string(identity_paths(data_dir).sidecar).ok()?;
    let mut lines = contents.lines();
    let value = lines.next()?.to_string();
    let derivation = match lines.next()? {
        "set-explicitly" => IdentityDerivation::SetExplicitly,
        _ => IdentityDerivation::DerivedAtFirstStart,
    };
    Some(ServiceIdentity { value, derivation })
}

/// What a deliberate identity change does, stated before it takes effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityChangeConsequence {
    pub current: String,
    pub proposed: String,
    /// The plain-language consequence: Home Assistant sees a new device and the
    /// history attached to the old one is orphaned.
    pub statement: String,
}

/// Describe what changing the identity to `proposed` would do, without doing it.
pub fn describe_change(
    data_dir: &Path,
    store_path: &Path,
    proposed: &str,
) -> Result<IdentityChangeConsequence, SettingsError> {
    // Stating a consequence is a QUESTION, so it takes the read-only door: an
    // operator who has not confirmed anything must not have a store created for
    // them, and must not contend with the deployment they are asking about.
    let store = SettingsStore::open_to_read(store_path)?;
    let current = persisted(&store, data_dir)?
        .map(|identity| identity.value)
        .unwrap_or_default();
    Ok(IdentityChangeConsequence {
        current,
        proposed: proposed.to_string(),
        statement: consequence_statement(proposed),
    })
}

/// The consequence, in plain language, that both the deliberate operation and
/// the ordinary-edit refusal state. One wording, so the operator reads the
/// same thing whichever way they arrive at it.
fn consequence_statement(proposed: &str) -> String {
    format!(
        "changing this node's service identity to {proposed} makes Home Assistant see an \
         entirely new device, and the entity history attached to the old device is orphaned"
    )
}

/// The deliberate change operation. Applies only after the consequence has been
/// stated.
pub fn change_deliberately(
    data_dir: &Path,
    store_path: &Path,
    proposed: &str,
) -> Result<ServiceIdentity, SettingsError> {
    // A confirmed change is a WRITE, and it takes the atomic door that refuses
    // a store which is not there rather than creating one: moving the identity
    // of a deployment nobody has started is not a rename, it is an invention.
    let store = SettingsStore::open_existing_for_change(store_path)?;
    let record = SettingRecord::local(
        SERVICE_IDENTITY_SETTING,
        Surface::VigilSettings,
        identity_scope(data_dir),
        SettingValue::text(proposed),
        SET_EXPLICITLY_REASON,
    );
    let derivation = derivation_of(record.author);
    store.write_record(record)?;
    let identity = ServiceIdentity {
        value: proposed.to_string(),
        derivation,
    };
    write_sidecar(data_dir, &identity)?;
    Ok(identity)
}

/// An ordinary settings edit aimed at the identity. Always refused, with the
/// same orphaned-history explanation the deliberate operation states.
pub fn refuse_ordinary_edit(proposed: &str) -> Refusal {
    Refusal {
        kind: RefusalKind::IdentityOrdinaryEdit,
        cause: format!(
            "this node's service identity is not an ordinary setting: {}.",
            consequence_statement(proposed)
        ),
        remedy: "Run the deliberate identity-change operation, which states that consequence \
                 before it takes effect, if that is genuinely what you want."
            .to_string(),
    }
}
