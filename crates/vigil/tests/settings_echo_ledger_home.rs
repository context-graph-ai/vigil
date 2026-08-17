//! Where the deployment's echo record lives: in the store it already opens,
//! never in a file beside it.
//!
//! The record of what Vigil last wrote to a surface is a fact about this one
//! machine, so it belongs in the declared never-sync table with the rest of
//! them. A second durable home beside the store is a second thing to back up,
//! to restore, to keep consistent with the store after a wipe — and the day the
//! two disagree, Vigil re-authors the operator's options file with a record
//! nobody typed.
//!
//! Unfakeable because the proof is a MOVE: the deployment directory is copied
//! whole to a new path and the record is read back from the copy. A row held
//! only in this process's memory does not survive that, and a row written to a
//! file beside the store fails the same test's file check.

use std::cell::RefCell;

use vigil::settings_model::{Scope, ScopeTarget, SettingValue, Surface};
use vigil::settings_reflection::{
    AddonSchemaType, EchoLedger, OptionsRecord, ReflectionFailure, SupervisorOptionsClient,
    TokenSource, addon_schema_keys, reflect_landed_change,
};
use vigil::settings_store::SettingsStore;

const NODE: &str = "node-a";

/// The integer-typed option this test mirrors.
const INT_SETTING: &str = "detector_stationary_interval_secs";

const MIRRORED_VALUE: i64 = 45;

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

impl SupervisorOptionsClient for RecordingSupervisor {
    fn read_options(&self) -> Result<OptionsRecord, ReflectionFailure> {
        Ok(self.current.borrow().clone())
    }

    fn write_options(&self, record: &OptionsRecord) -> Result<(), ReflectionFailure> {
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
    OptionsRecord {
        entries: addon_schema_keys()
            .into_iter()
            .filter(|declared| declared.required)
            .map(|declared| {
                (
                    declared.key.to_string(),
                    placeholder(declared.declared_type),
                )
            })
            .collect(),
    }
}

/// Every file the deployment directory holds right now, by name.
fn files_in(directory: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .expect("read the deployment directory")
        .map(|entry| {
            entry
                .expect("read a deployment directory entry")
                .file_name()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    names.sort();
    names
}

fn copy_directory(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("create the copied deployment directory");
    for entry in std::fs::read_dir(from).expect("read the deployment directory") {
        let entry = entry.expect("read a deployment directory entry");
        if entry.file_type().expect("classify the entry").is_dir() {
            copy_directory(&entry.path(), &to.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), to.join(entry.file_name()))
                .expect("copy a deployment file");
        }
    }
}

#[test]
fn the_deployment_holds_its_echo_record_in_its_store_and_writes_no_file_beside_it() {
    let directory = tempfile::tempdir().expect("temporary deployment directory");
    let data_dir = directory.path();
    let client = RecordingSupervisor {
        current: RefCell::new(current_options()),
    };

    let store = SettingsStore::open(data_dir).expect("open the node-side settings store");
    store
        .set_local(
            INT_SETTING,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::Int(MIRRORED_VALUE),
        )
        .expect("pin the value through the operator surface");
    // Everything the store itself needs on disk exists by now, so a file that
    // appears across the reflection below is the reflection's own.
    let before = files_in(data_dir);

    reflect_landed_change(&store, data_dir, &target(), &client, INT_SETTING)
        .expect("mirroring a landed change must not error");
    let posted = client.read_options().expect("the surface reads back");

    assert_eq!(
        files_in(data_dir),
        before,
        "recording what Vigil wrote out adds no file to the deployment: the record belongs in the \
         store the deployment already opens, where it is declared never to travel and is restored \
         with everything else"
    );

    // The move: the deployment is copied whole and reopened at a new path. A
    // record living in this process's memory cannot cross that, and the file
    // check above has already ruled out a carrier beside the store.
    drop(store);
    let elsewhere = tempfile::tempdir().expect("second temporary deployment directory");
    let moved = elsewhere.path().join("deployment");
    copy_directory(data_dir, &moved);

    let ledger = EchoLedger::open(&moved).expect("the copied deployment's echo ledger opens");
    let entry = ledger
        .entries()
        .iter()
        .find(|entry| entry.surface == Surface::AddonOptions && entry.setting == INT_SETTING)
        .unwrap_or_else(|| {
            panic!(
                "the copied deployment carries the record of what Vigil last wrote out; without \
                 it the first boot after a restore re-authors the operator's options file with a \
                 record nobody typed. Rows found: {:?}",
                ledger.entries()
            )
        });
    assert_eq!(
        entry.last_write_out, posted,
        "and it is the complete posted record, which is what the echo guard compares against"
    );
}
