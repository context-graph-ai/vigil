//! The echo ledger: the loop between the settings store and the add-on options
//! file closed by construction rather than by timing.
//!
//! Every assertion is on recorded content and on call counters. Nothing here
//! waits for a quiet period or sleeps to let a write "settle"; termination is
//! proven by feeding the echo back repeatedly and reading a bounded write
//! count off the Supervisor double.

use std::cell::RefCell;

use vigil::settings_model::{Author, Scope, SettingValue, Surface};
use vigil::settings_reflection::{
    AddonSchemaType, EchoLedger, EchoVerdict, OptionsRecord, ReflectionFailure,
    SupervisorOptionsClient, TokenSource, addon_schema_keys, reflect,
};
use vigil::settings_store::{SettingsStore, SurfaceChange, SurfaceSnapshot};

/// The integer-typed option these tests reflect and then read back.
const INT_SETTING: &str = "detector_stationary_interval_secs";

/// The float-typed option the measured Int-into-Float Supervisor coercion
/// case lands on: an integral value posted here comes back on the next read
/// as the numerically-equal float.
const FLOAT_SETTING: &str = "detector_confidence_threshold";

/// A schema-optional key a user set, carried through every full-replace write.
const USER_SET_OPTIONAL_KEY: &str = "detector_model_path";
const USER_SET_OPTIONAL_VALUE: &str = "/share/vigil/detector.mpk";

// ---------------------------------------------------------------------------
// The deterministic Supervisor double. Test-support only; never in `src/`.
// ---------------------------------------------------------------------------

struct RecordingSupervisor {
    posted: RefCell<Vec<OptionsRecord>>,
    current: RefCell<OptionsRecord>,
}

impl RecordingSupervisor {
    fn new(current: OptionsRecord) -> Self {
        Self {
            posted: RefCell::new(Vec::new()),
            current: RefCell::new(current),
        }
    }

    fn posted_records(&self) -> Vec<OptionsRecord> {
        self.posted.borrow().clone()
    }

    fn write_count(&self) -> usize {
        self.posted.borrow().len()
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
        self.posted.borrow_mut().push(record.clone());
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

fn value_of<'a>(record: &'a OptionsRecord, key: &str) -> Option<&'a SettingValue> {
    record
        .entries
        .iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| value)
}

fn ledger_entry_for<'a>(
    ledger: &'a EchoLedger,
    setting: &str,
) -> &'a vigil::settings_reflection::EchoLedgerEntry {
    ledger
        .entries()
        .iter()
        .find(|entry| entry.surface == Surface::AddonOptions && entry.setting == setting)
        .unwrap_or_else(|| panic!("the ledger must hold a write-out row for `{setting}`"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn the_ledger_records_the_complete_posted_record_as_the_write_out() {
    // Unfakeable because the recorded write-out is compared against the record
    // the Supervisor double actually received, key by key. A ledger that stored
    // only the changed key would match neither the posted record nor the
    // untouched keys checked below — and it would then misclassify every later
    // read-back, since a read returns the whole file.
    let client = RecordingSupervisor::new(current_options());
    let mut ledger = EchoLedger::default();

    reflect(
        &client,
        &mut ledger,
        INT_SETTING,
        &SettingValue::Int(45),
        true,
    )
    .expect("reflection must not error");

    let posted = client.posted_records();
    assert_eq!(posted.len(), 1, "reflection posts exactly once");

    let entry = ledger_entry_for(&ledger, INT_SETTING);
    assert_eq!(
        entry.last_write_out, posted[0],
        "the ledger records the complete posted record, not just the key that changed"
    );
    assert_eq!(
        value_of(&entry.last_write_out, INT_SETTING),
        Some(&SettingValue::Int(45))
    );
    assert_eq!(
        value_of(&entry.last_write_out, USER_SET_OPTIONAL_KEY),
        Some(&SettingValue::text(USER_SET_OPTIONAL_VALUE)),
        "the recorded write-out carries the untouched user-set optional key too"
    );
}

#[test]
fn content_matching_the_recorded_write_out_is_recognized_as_an_echo_and_authors_nothing() {
    // Unfakeable because the content classified is what the double actually
    // holds on the surface after the write, not a copy the test assembled: the
    // read comes back through `read_options`. And "authors nothing" is checked
    // as state — the ledger row is unchanged and no second write-out was
    // recorded.
    let client = RecordingSupervisor::new(current_options());
    let mut ledger = EchoLedger::default();

    reflect(
        &client,
        &mut ledger,
        INT_SETTING,
        &SettingValue::Int(45),
        true,
    )
    .expect("reflection must not error");

    let recorded = ledger_entry_for(&ledger, INT_SETTING)
        .last_write_out
        .clone();
    let observed = client.read_options().expect("the surface reads back");
    assert_eq!(
        observed, recorded,
        "the Supervisor round trip is exact, so the surface holds what was posted"
    );

    let verdict = ledger.classify_read_back(Surface::AddonOptions, INT_SETTING, &observed);
    assert_eq!(
        verdict,
        EchoVerdict::Echo,
        "content matching the recorded write-out is Vigil's own echo"
    );

    assert_eq!(
        ledger_entry_for(&ledger, INT_SETTING).last_write_out,
        recorded,
        "classifying an echo authors nothing and bumps nothing"
    );
    assert_eq!(
        ledger
            .entries()
            .iter()
            .filter(|entry| entry.surface == Surface::AddonOptions && entry.setting == INT_SETTING)
            .count(),
        1,
        "an echo adds no ledger row"
    );
}

#[test]
fn an_integer_reflected_into_a_float_key_and_coerced_on_read_back_is_still_recognized_as_an_echo() {
    // Unfakeable because it reproduces the measured Supervisor coercion
    // directly: an integral value posted into a float-typed add-on option
    // comes back on the NEXT read as the coerced float (the direction
    // document records "an integer `3` into a float field becomes `3.0`"),
    // and a comparison that used `OptionsRecord`'s derived `PartialEq` would
    // see `Int(3) != Float(3.0)` and misclassify this as a human edit —
    // manufacturing a phantom operator pin nobody set (cold-review-r4
    // finding 10). The ledger's own write-out record stays exactly what was
    // posted (`Int(3)`, never silently upgraded), so this also proves the
    // fix normalizes at COMPARE time, not by lying about what Vigil sent.
    let client = RecordingSupervisor::new(current_options());
    let mut ledger = EchoLedger::default();

    reflect(
        &client,
        &mut ledger,
        FLOAT_SETTING,
        &SettingValue::Int(1),
        true,
    )
    .expect("reflection must not error: an Int is an admissible value for a Float-typed key");

    let recorded = ledger_entry_for(&ledger, FLOAT_SETTING)
        .last_write_out
        .clone();
    assert_eq!(
        value_of(&recorded, FLOAT_SETTING),
        Some(&SettingValue::Int(1)),
        "the write-out record is what Vigil actually posted, never a silently upgraded float"
    );

    // The Supervisor's own measured coercion: the SAME value, read back as a
    // float.
    let mut coerced_read_back = recorded.clone();
    for (key, value) in coerced_read_back.entries.iter_mut() {
        if key == FLOAT_SETTING {
            *value = SettingValue::Float(1.0);
        }
    }

    let verdict =
        ledger.classify_read_back(Surface::AddonOptions, FLOAT_SETTING, &coerced_read_back);
    assert_eq!(
        verdict,
        EchoVerdict::Echo,
        "an Int Vigil posted into a Float key, coerced to the numerically-equal Float on \
         read-back, is still Vigil's own echo — not a human edit"
    );
    assert_eq!(
        ledger
            .entries()
            .iter()
            .filter(|entry| {
                entry.surface == Surface::AddonOptions && entry.setting == FLOAT_SETTING
            })
            .count(),
        1,
        "recognizing the coerced value as an echo authors no second ledger row"
    );
}

#[test]
fn a_float_value_genuinely_differing_from_the_int_write_out_still_authors_a_human_record() {
    // The complement of the coercion-tolerance test above: the numeric
    // compare is float-key-scoped and equality-based, not "any Float is an
    // echo of any Int" — a float that does not match the posted integer's
    // numeric value must still be recognized as a human edit.
    let client = RecordingSupervisor::new(current_options());
    let mut ledger = EchoLedger::default();

    reflect(
        &client,
        &mut ledger,
        FLOAT_SETTING,
        &SettingValue::Int(1),
        true,
    )
    .expect("reflection must not error");

    let mut edited = client.surface_content();
    for (key, value) in edited.entries.iter_mut() {
        if key == FLOAT_SETTING {
            *value = SettingValue::Float(0.5);
        }
    }

    let verdict = ledger.classify_read_back(Surface::AddonOptions, FLOAT_SETTING, &edited);
    assert_eq!(
        verdict,
        EchoVerdict::HumanAuthored,
        "a float that is not numerically equal to the posted int is a real human edit, not \
         coercion noise"
    );
}

#[test]
fn a_value_differing_from_the_recorded_write_out_authors_a_local_explicit_record() {
    // Unfakeable because it takes the classification AND the record it must
    // produce: the differing content is fed through the real store's add-on
    // surface application and has to come back as a local-explicit record
    // attributed to the add-on options surface. A classifier that said
    // HumanAuthored while the store authored nothing fails the second half.
    let client = RecordingSupervisor::new(current_options());
    let mut ledger = EchoLedger::default();

    reflect(
        &client,
        &mut ledger,
        INT_SETTING,
        &SettingValue::Int(45),
        true,
    )
    .expect("reflection must not error");

    // A human edits the options page: same file, one different value.
    let mut edited = client.surface_content();
    for (key, value) in edited.entries.iter_mut() {
        if key == INT_SETTING {
            *value = SettingValue::Int(90);
        }
    }
    assert_ne!(
        edited,
        ledger_entry_for(&ledger, INT_SETTING).last_write_out,
        "the fixture must genuinely differ from the recorded write-out"
    );

    let verdict = ledger.classify_read_back(Surface::AddonOptions, INT_SETTING, &edited);
    assert_eq!(
        verdict,
        EchoVerdict::HumanAuthored,
        "only a value differing from what Vigil wrote can have come from a human"
    );

    let directory = tempfile::tempdir().expect("temp directory");
    let store = SettingsStore::open(directory.path()).expect("node-side settings store");
    let changes = store
        .apply_surface_snapshot(&SurfaceSnapshot {
            surface: Surface::AddonOptions,
            scope: Scope::node("node-a"),
            present_and_parsing: true,
            entries: edited.entries.clone(),
            declared_defaults: vec![(INT_SETTING.to_string(), SettingValue::Int(30))],
        })
        .expect("applying the add-on surface must not error");

    let authored = changes
        .iter()
        .find_map(|change| match change {
            SurfaceChange::Authored(record) if record.setting == INT_SETTING => Some(record),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the diverging value must author a record, got {changes:?}"));
    assert_eq!(authored.author, Author::LocalExplicit);
    assert_eq!(authored.surface, Surface::AddonOptions);
    assert_eq!(authored.value, SettingValue::Int(90));
}

#[test]
fn an_echo_never_triggers_another_write_so_the_cycle_terminates() {
    // Unfakeable because it is a bounded counter, not a timeout: the surface is
    // read back and classified many times, and a reflection is issued only when
    // the classification says a human wrote it. If the ledger ever misread its
    // own echo, each pass would issue another write and the count would climb
    // past one — which is exactly the runaway loop this design exists to make
    // impossible.
    let client = RecordingSupervisor::new(current_options());
    let mut ledger = EchoLedger::default();

    reflect(
        &client,
        &mut ledger,
        INT_SETTING,
        &SettingValue::Int(45),
        true,
    )
    .expect("reflection must not error");
    assert_eq!(
        client.write_count(),
        1,
        "the initial reflection writes once"
    );

    const PASSES: usize = 16;
    for pass in 0..PASSES {
        let observed = client.read_options().expect("the surface reads back");
        let verdict = ledger.classify_read_back(Surface::AddonOptions, INT_SETTING, &observed);
        assert_eq!(
            verdict,
            EchoVerdict::Echo,
            "pass {pass}: reading back Vigil's own write must stay an echo"
        );
        if verdict == EchoVerdict::HumanAuthored {
            reflect(
                &client,
                &mut ledger,
                INT_SETTING,
                &SettingValue::Int(45),
                true,
            )
            .expect("reflection must not error");
        }
    }

    assert_eq!(
        client.write_count(),
        1,
        "feeding the echo back {PASSES} times triggers no further write, so the cycle terminates"
    );
}

#[test]
fn a_write_out_recorded_before_a_restart_is_still_recognized_as_an_echo_after_it() {
    // The case that actually matters. Reading the options back after a
    // reflection is a step that, in production, spans a process restart: the
    // add-on is restarted so its own options file agrees, and the next boot
    // reads the surface again. A ledger that only ever lived in memory has no
    // recorded write-out at that point, so it classifies Vigil's own content as
    // human-authored and re-authors on every boot — the exact runaway the ledger
    // exists to make impossible, invisible to any test that never drops it.
    //
    // Unfakeable because the ledger is genuinely DROPPED between the write-out
    // and the read-back, and the two halves are asserted against each other: the
    // reopened ledger must hold the same complete posted record, and must return
    // Echo for content the test never handed it. The in-memory contrast below
    // proves the fixture is not trivially Echo for everyone — a ledger with no
    // history classifies the same content HumanAuthored.
    let directory = tempfile::tempdir().expect("temp directory");
    let ledger_path = directory.path();
    let client = RecordingSupervisor::new(current_options());

    let recorded = {
        let mut ledger =
            EchoLedger::open(ledger_path).expect("the ledger opens over its own durable path");
        reflect(
            &client,
            &mut ledger,
            INT_SETTING,
            &SettingValue::Int(45),
            true,
        )
        .expect("reflection must not error");
        ledger_entry_for(&ledger, INT_SETTING)
            .last_write_out
            .clone()
    };
    assert_eq!(client.write_count(), 1, "the reflection writes once");

    // The restart: the ledger that recorded the write-out is gone.
    let reopened =
        EchoLedger::open(ledger_path).expect("the ledger reopens over the same durable path");
    assert_eq!(
        ledger_entry_for(&reopened, INT_SETTING).last_write_out,
        recorded,
        "the complete recorded write-out survives the restart — a ledger that only lived in \
         memory comes back empty and has nothing to compare the surface against"
    );

    let observed = client.read_options().expect("the surface reads back");
    assert_eq!(
        reopened.classify_read_back(Surface::AddonOptions, INT_SETTING, &observed),
        EchoVerdict::Echo,
        "after a restart, content Vigil itself wrote before the restart is still Vigil's own \
         echo; a ledger that forgot it would re-author the user's options file on every boot"
    );
    assert_eq!(
        EchoLedger::default().classify_read_back(Surface::AddonOptions, INT_SETTING, &observed),
        EchoVerdict::HumanAuthored,
        "contrast: a ledger with no recorded write-out cannot recognize the echo, which is what \
         makes the surviving verdict above a fact about persistence rather than about the content"
    );
}
