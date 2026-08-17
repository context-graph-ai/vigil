//! Reading the add-on options back is the half of the echo loop that decides
//! whether a person typed something.
//!
//! Vigil mirrors a landed change onto the add-on options, and the next time
//! that surface is ingested the same content comes back. Deciding what it means
//! by comparing it against the store misses the case this ledger exists for:
//! the mirrored value was authored through another surface, so the add-on
//! options have no record of their own and Vigil's own write reads as a human
//! edit. Every boot would then re-author the operator's options file with a
//! record nobody typed.
//!
//! So the ingest path consults what Vigil last wrote out, and records what it
//! read back. Both tests drive the production surface-ingest path against a
//! real store and the production reflection entry point; nothing here sleeps or
//! waits, and every assertion is on recorded state.

use std::cell::RefCell;

use vigil::settings_model::{Author, Scope, ScopeTarget, SettingValue, Surface};
use vigil::settings_reflection::{
    AddonSchemaType, EchoLedger, EchoVerdict, OptionsRecord, ReflectionFailure,
    SupervisorOptionsClient, TokenSource, addon_schema_keys, reflect_landed_change,
};
use vigil::settings_store::{SettingsStore, SurfaceChange, SurfaceSnapshot};

const NODE: &str = "node-a";

/// The integer-typed option these tests mirror and then read back.
const INT_SETTING: &str = "detector_stationary_interval_secs";

/// A schema-optional key a user set, carried through every full-replace write.
const USER_SET_OPTIONAL_KEY: &str = "detector_model_path";
const USER_SET_OPTIONAL_VALUE: &str = "/share/vigil/detector.mpk";

/// What the operator pinned, and what a human then types over it.
const MIRRORED_VALUE: i64 = 45;
const HUMAN_TYPED_VALUE: i64 = 90;

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: NODE.to_string(),
        site: NODE.to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

// ---------------------------------------------------------------------------
// The deterministic Supervisor double. Test-support only; never in `src/`.
// ---------------------------------------------------------------------------

struct RecordingSupervisor {
    current: RefCell<OptionsRecord>,
}

impl RecordingSupervisor {
    fn new(current: OptionsRecord) -> Self {
        Self {
            current: RefCell::new(current),
        }
    }

    fn surface_content(&self) -> OptionsRecord {
        self.current.borrow().clone()
    }
}

impl SupervisorOptionsClient for RecordingSupervisor {
    fn read_options(&self) -> Result<OptionsRecord, ReflectionFailure> {
        Ok(self.current.borrow().clone())
    }

    fn write_options(&self, record: &OptionsRecord) -> Result<(), ReflectionFailure> {
        // The measured Supervisor behavior: a write is a full replace, and the
        // round trip is exact, so what is posted is exactly what a later read
        // returns.
        *self.current.borrow_mut() = record.clone();
        Ok(())
    }

    fn restart_addon(&self) -> Result<(), ReflectionFailure> {
        Ok(())
    }

    fn token_source(&self) -> TokenSource {
        TokenSource::OwnContainer
    }
}

// ---------------------------------------------------------------------------
// Fixtures derived from the production schema declaration, never hand-listed.
// ---------------------------------------------------------------------------

fn placeholder(declared_type: AddonSchemaType) -> SettingValue {
    match declared_type {
        AddonSchemaType::Bool => SettingValue::Bool(true),
        AddonSchemaType::Int => SettingValue::Int(7),
        AddonSchemaType::Float => SettingValue::Float(0.5),
        AddonSchemaType::Str => SettingValue::text("placeholder"),
        AddonSchemaType::ListOfStr => SettingValue::list(["person"]),
    }
}

fn current_options() -> OptionsRecord {
    let mut entries: Vec<(String, SettingValue)> = addon_schema_keys()
        .into_iter()
        .filter(|declared| declared.required)
        .map(|declared| {
            (
                declared.key.to_string(),
                placeholder(declared.declared_type),
            )
        })
        .collect();
    entries.push((
        USER_SET_OPTIONAL_KEY.to_string(),
        SettingValue::text(USER_SET_OPTIONAL_VALUE),
    ));
    OptionsRecord { entries }
}

/// Pin the value through the operator's own surface and mirror it onto the
/// add-on options through the production reflection path.
fn pin_and_mirror(data_dir: &std::path::Path, client: &RecordingSupervisor) -> SettingsStore {
    let store = SettingsStore::open(data_dir).expect("open the node-side settings store");
    store
        .set_local(
            INT_SETTING,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::Int(MIRRORED_VALUE),
        )
        .expect("pin the value through the operator surface");
    reflect_landed_change(&store, data_dir, &target(), client, INT_SETTING)
        .expect("mirroring a landed change must not error");
    store
}

/// Ingest what the add-on options surface currently holds, exactly as a start
/// ingests the surfaces it finds.
fn ingest(store: &SettingsStore, content: &OptionsRecord) -> Vec<SurfaceChange> {
    store
        .apply_surface_snapshot(&SurfaceSnapshot {
            surface: Surface::AddonOptions,
            scope: Scope::node(NODE),
            present_and_parsing: true,
            entries: content.entries.clone(),
            declared_defaults: Vec::new(),
        })
        .expect("ingesting the add-on options surface must not error")
}

fn authored_value(changes: &[SurfaceChange], setting: &str) -> Option<SettingValue> {
    changes.iter().find_map(|change| match change {
        SurfaceChange::Authored(record) if record.setting == setting => Some(record.value.clone()),
        _ => None,
    })
}

fn read_back(data_dir: &std::path::Path, setting: &str) -> Option<OptionsRecord> {
    EchoLedger::open(data_dir)
        .expect("the deployment's echo ledger opens")
        .entries()
        .iter()
        .find(|entry| entry.surface == Surface::AddonOptions && entry.setting == setting)
        .unwrap_or_else(|| panic!("the ledger must hold a row for `{setting}`"))
        .last_read_back
        .clone()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn ingesting_vigils_own_mirrored_write_records_the_read_back_and_authors_nothing() {
    // Unfakeable because the mirrored value was pinned through a DIFFERENT
    // surface, so the add-on options hold no record of their own to compare
    // against: content comparison alone cannot tell this echo from a human
    // edit, and the store would author an add-on-options record attributed to
    // an operator who typed nothing. What the ledger read back is asserted as
    // recorded state, against the content the Supervisor double actually holds
    // — not a copy the test assembled.
    let directory = tempfile::tempdir().expect("temporary deployment directory");
    let data_dir = directory.path();
    let client = RecordingSupervisor::new(current_options());
    let store = pin_and_mirror(data_dir, &client);

    let observed = client.surface_content();
    let changes = ingest(&store, &observed);

    assert_eq!(
        authored_value(&changes, INT_SETTING),
        None,
        "reading back Vigil's own mirrored write authors nothing: nobody typed it, and a record \
         saying an operator did would outrank the very pin it came from; got {changes:?}"
    );
    assert_eq!(
        read_back(data_dir, INT_SETTING).as_ref(),
        Some(&observed),
        "the ingest records what it read back, so the ledger holds both halves of the loop and \
         the next start can tell Vigil's own content from a person's"
    );
    assert_eq!(
        EchoLedger::open(data_dir)
            .expect("the deployment's echo ledger reopens")
            .classify_read_back(Surface::AddonOptions, INT_SETTING, &observed),
        EchoVerdict::Echo,
        "content matching the recorded write-out is Vigil's own echo"
    );
}

#[test]
fn a_value_a_person_typed_over_the_mirrored_one_is_ingested_as_their_own_record() {
    // The contrast that keeps the test above honest: the same path, one value
    // changed on the surface. Suppressing the echo must not suppress the
    // operator — a ledger that classified everything as its own echo would pass
    // the first test and silently discard every edit a person makes on the
    // add-on options page.
    let directory = tempfile::tempdir().expect("temporary deployment directory");
    let data_dir = directory.path();
    let client = RecordingSupervisor::new(current_options());
    let store = pin_and_mirror(data_dir, &client);

    let mut edited = client.surface_content();
    for (key, value) in edited.entries.iter_mut() {
        if key == INT_SETTING {
            *value = SettingValue::Int(HUMAN_TYPED_VALUE);
        }
    }
    client
        .write_options(&edited)
        .expect("the person's edit lands on the surface");

    let observed = client.surface_content();
    let changes = ingest(&store, &observed);

    assert_eq!(
        authored_value(&changes, INT_SETTING),
        Some(SettingValue::Int(HUMAN_TYPED_VALUE)),
        "a value differing from what Vigil wrote can only have come from a person, and their edit \
         is authored: {changes:?}"
    );
    let authored = changes
        .iter()
        .find_map(|change| match change {
            SurfaceChange::Authored(record) if record.setting == INT_SETTING => Some(record),
            _ => None,
        })
        .expect("the edit authors a record");
    assert_eq!(authored.author, Author::LocalExplicit);
    assert_eq!(authored.surface, Surface::AddonOptions);

    assert_eq!(
        read_back(data_dir, INT_SETTING).as_ref(),
        Some(&observed),
        "and the ingest records what it read back either way, so the next pass compares against \
         the surface as it now stands rather than against content that is two edits old"
    );
    assert_eq!(
        EchoLedger::open(data_dir)
            .expect("the deployment's echo ledger reopens")
            .classify_read_back(Surface::AddonOptions, INT_SETTING, &observed),
        EchoVerdict::HumanAuthored,
        "the person's content is not Vigil's echo"
    );
}
