//! Reflection onto the Home Assistant add-on options surface.
//!
//! Every assertion here is about state the product produces — the outcome
//! value, the record actually posted, and the ordered list of calls the
//! Supervisor double received. Nothing waits, sleeps, or measures elapsed
//! time: "pre-validation ran before the write" is proven by the absence of a
//! write call in the recorded order, not by a clock.

use std::cell::RefCell;

use vigil::settings_model::{
    Author, ControlState, Scope, ScopeTarget, SettingRecord, SettingValue, Surface,
};
use vigil::settings_reflection::{
    AddonSchemaKey, AddonSchemaType, EchoLedger, OptionsRecord, ReflectionFailure,
    ReflectionOutcome, SupervisorOptionsClient, TokenSource, addon_schema_keys, reflect,
    value_survives_schema_type,
};
use vigil::settings_store::SettingsStore;

/// The integer-typed option the measured Supervisor coercion case lands on:
/// `30.7` posted into it is silently truncated to `30` and reported accepted.
const INT_SETTING: &str = "detector_stationary_interval_secs";

/// A schema-optional key a user set. A write that omits it silently erases it,
/// so the merge has to carry it through untouched.
const USER_SET_OPTIONAL_KEY: &str = "detector_model_path";
const USER_SET_OPTIONAL_VALUE: &str = "/share/vigil/detector.mpk";

/// A setting name the add-on schema never declares.
const UNDECLARED_SETTING: &str = "vigil_setting_with_no_addon_schema_target";

// ---------------------------------------------------------------------------
// The deterministic Supervisor double. Test-support only; never in `src/`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Call {
    ReadOptions,
    WriteOptions(OptionsRecord),
    RestartAddon,
}

struct RecordingSupervisor {
    calls: RefCell<Vec<Call>>,
    current: OptionsRecord,
    read_failure: Option<ReflectionFailure>,
    write_failure: Option<ReflectionFailure>,
    restart_failure: Option<ReflectionFailure>,
    token: TokenSource,
}

impl RecordingSupervisor {
    fn new(current: OptionsRecord) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            current,
            read_failure: None,
            write_failure: None,
            restart_failure: None,
            token: TokenSource::OwnContainer,
        }
    }

    fn failing_read(mut self, failure: ReflectionFailure) -> Self {
        self.read_failure = Some(failure);
        self
    }

    fn failing_write(mut self, failure: ReflectionFailure) -> Self {
        self.write_failure = Some(failure);
        self
    }

    fn with_token(mut self, token: TokenSource) -> Self {
        self.token = token;
        self
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }

    fn posted_records(&self) -> Vec<OptionsRecord> {
        self.calls
            .borrow()
            .iter()
            .filter_map(|call| match call {
                Call::WriteOptions(record) => Some(record.clone()),
                _ => None,
            })
            .collect()
    }

    fn write_count(&self) -> usize {
        self.posted_records().len()
    }

    fn restart_count(&self) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|call| matches!(call, Call::RestartAddon))
            .count()
    }
}

impl SupervisorOptionsClient for RecordingSupervisor {
    fn read_options(&self) -> Result<OptionsRecord, ReflectionFailure> {
        self.calls.borrow_mut().push(Call::ReadOptions);
        match &self.read_failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(self.current.clone()),
        }
    }

    fn write_options(&self, record: &OptionsRecord) -> Result<(), ReflectionFailure> {
        self.calls
            .borrow_mut()
            .push(Call::WriteOptions(record.clone()));
        if let Some(failure) = &self.write_failure {
            return Err(failure.clone());
        }
        // The measured Supervisor behavior: a write is a full replace validated
        // against the posted content alone, so a post that drops a
        // schema-required key is rejected outright even though the key already
        // had a stored value. The required-key inventory is read from the one
        // helper below, so the double and the tests never diverge on it.
        for key in required_schema_keys() {
            if !record
                .entries
                .iter()
                .any(|(entry, _)| entry.as_str() == key)
            {
                return Err(ReflectionFailure::SupervisorRejected {
                    reason: format!("missing required option key {key}"),
                });
            }
        }
        Ok(())
    }

    fn restart_addon(&self) -> Result<(), ReflectionFailure> {
        self.calls.borrow_mut().push(Call::RestartAddon);
        match &self.restart_failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(()),
        }
    }

    fn token_source(&self) -> TokenSource {
        self.token.clone()
    }
}

// ---------------------------------------------------------------------------
// Fixtures derived from the production schema declaration, never hand-listed.
// ---------------------------------------------------------------------------

fn schema_key(key: &str) -> AddonSchemaKey {
    addon_schema_keys()
        .into_iter()
        .find(|declared| declared.key == key)
        .unwrap_or_else(|| panic!("the add-on schema must declare `{key}`"))
}

/// Every key the add-on schema declares required, read from the production
/// declaration. The single source for both the Supervisor double's full-replace
/// validation and the tests that assert against it, so neither can drift into a
/// second, differently-spelled copy of the same rule.
fn required_schema_keys() -> Vec<&'static str> {
    addon_schema_keys()
        .into_iter()
        .filter(|declared| declared.required)
        .map(|declared| declared.key)
        .collect()
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

/// The record the Supervisor currently holds: every schema-required key (built
/// from the production declaration so the fixture cannot drift away from it),
/// plus one schema-optional key the user set.
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

fn node_target() -> ScopeTarget {
    ScopeTarget {
        tenant: "tenant-a".to_string(),
        site: "site-a".to_string(),
        node: "node-a".to_string(),
        camera: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn a_pushed_value_is_reflected_without_requesting_a_restart() {
    // Unfakeable because the recorded call order shows the value reached the
    // options surface through the write alone: no restart call precedes it, so
    // the reflection cannot be an artifact of restarting the add-on. An
    // implementation that only made the page agree after a restart would place
    // a RestartAddon call before the write and fail here.
    let client = RecordingSupervisor::new(current_options());
    let mut ledger = EchoLedger::default();

    let outcome = reflect(
        &client,
        &mut ledger,
        INT_SETTING,
        &SettingValue::Int(45),
        true,
    )
    .expect("reflecting a representable value onto a declared key must not error");

    assert_eq!(
        outcome,
        ReflectionOutcome::Mirrored {
            restart_requested: true
        }
    );

    let calls = client.calls();
    let write_position = calls
        .iter()
        .position(|call| matches!(call, Call::WriteOptions(_)))
        .expect("reflection must post the options record");
    assert!(
        !calls[..write_position]
            .iter()
            .any(|call| matches!(call, Call::RestartAddon)),
        "no restart may be requested before the value is mirrored, got {calls:?}"
    );

    let posted = client.posted_records();
    assert_eq!(posted.len(), 1, "reflection posts exactly once");
    assert_eq!(
        value_of(&posted[0], INT_SETTING),
        Some(&SettingValue::Int(45)),
        "the posted record must carry the pushed value"
    );
}

#[test]
fn with_restart_on_reflect_at_its_default_vigil_requests_the_restart_that_makes_the_container_file_agree()
 {
    // Unfakeable because it asserts both halves separately: the outcome says a
    // restart was requested AND the double recorded exactly one restart call,
    // ordered after the write. Reporting `restart_requested: true` without ever
    // asking the Supervisor fails the counter; asking without reporting fails
    // the outcome.
    let client = RecordingSupervisor::new(current_options());
    let mut ledger = EchoLedger::default();

    // The setting's default is on; the caller passes that default here.
    let outcome = reflect(
        &client,
        &mut ledger,
        INT_SETTING,
        &SettingValue::Int(45),
        true,
    )
    .expect("reflection must not error");

    assert_eq!(
        outcome,
        ReflectionOutcome::Mirrored {
            restart_requested: true
        }
    );
    assert_eq!(
        client.restart_count(),
        1,
        "exactly one restart makes the container's own options file agree, got {:?}",
        client.calls()
    );

    let calls = client.calls();
    let write_position = calls
        .iter()
        .position(|call| matches!(call, Call::WriteOptions(_)))
        .expect("reflection must post the options record");
    let restart_position = calls
        .iter()
        .position(|call| matches!(call, Call::RestartAddon))
        .expect("the restart must be requested");
    assert!(
        write_position < restart_position,
        "the restart follows the write that mirrors the value, got {calls:?}"
    );
}

#[test]
fn with_restart_on_reflect_off_the_value_applies_where_it_can_and_the_rest_is_reported_pending_with_the_divergence()
 {
    // Unfakeable because it requires three independent facts at once: the value
    // was still mirrored (a write happened carrying it), no restart was asked
    // for (counter is zero), and the outcome carries a divergence the operator
    // surface can render. Silently reporting `Mirrored` with the setting off
    // would hide the container's stale file and fails the outcome assertion.
    let client = RecordingSupervisor::new(current_options());
    let mut ledger = EchoLedger::default();

    let outcome = reflect(
        &client,
        &mut ledger,
        INT_SETTING,
        &SettingValue::Int(45),
        false,
    )
    .expect("reflection must not error");

    match &outcome {
        ReflectionOutcome::AppliedWithPendingDivergence { divergence } => {
            assert!(
                divergence.contains(INT_SETTING),
                "the divergence must name the setting that is pending, got {divergence}"
            );
        }
        other => panic!("restart-on-reflect off must report a pending divergence, got {other:?}"),
    }

    assert_eq!(
        client.restart_count(),
        0,
        "restart-on-reflect off must ask for no restart, got {:?}",
        client.calls()
    );
    let posted = client.posted_records();
    assert_eq!(posted.len(), 1, "the value still applies where it can");
    assert_eq!(
        value_of(&posted[0], INT_SETTING),
        Some(&SettingValue::Int(45))
    );
}

#[test]
fn a_local_vigil_settings_change_reflects_immediately() {
    // Unfakeable because the value being mirrored is the one the production
    // store actually authored through the `vigil settings` surface, not a
    // literal the test invented: the assertion compares the posted record
    // against the stored record's own value and surface.
    let directory = tempfile::tempdir().expect("temp directory");
    let store = SettingsStore::open(directory.path()).expect("node-side settings store");

    let record = store
        .set_local(
            INT_SETTING,
            Surface::VigilSettings,
            Scope::node("node-a"),
            SettingValue::Int(45),
        )
        .expect("a local change through `vigil settings` must be accepted");
    assert_eq!(record.surface, Surface::VigilSettings);
    assert_eq!(record.author, Author::LocalExplicit);

    let client = RecordingSupervisor::new(current_options());
    let mut ledger = EchoLedger::default();
    let outcome = reflect(&client, &mut ledger, INT_SETTING, &record.value, false)
        .expect("a local change reflects like any other effective value");

    assert!(
        matches!(
            outcome,
            ReflectionOutcome::Mirrored { .. }
                | ReflectionOutcome::AppliedWithPendingDivergence { .. }
        ),
        "a local change must reach the add-on options surface, got {outcome:?}"
    );
    let posted = client.posted_records();
    assert_eq!(posted.len(), 1, "the local change is mirrored immediately");
    assert_eq!(value_of(&posted[0], INT_SETTING), Some(&record.value));
}

#[test]
fn automatic_and_auto_adjusted_values_never_reflect() {
    // Unfakeable because it asserts the gate in both directions on the same
    // function: the two states Vigil chooses for itself do not reflect, and the
    // two authored states do. A gate that simply returned false everywhere
    // would silence reflection entirely and fails the second half.
    assert!(
        !vigil::settings_reflection::control_state_reflects(&ControlState::Automatic),
        "an automatic value must never churn the user's options record"
    );
    assert!(
        !vigil::settings_reflection::control_state_reflects(&ControlState::AutoAdjusted),
        "an auto-adjusted value must never churn the user's options record"
    );
    assert!(
        !vigil::settings_reflection::control_state_reflects(&ControlState::ManagedBy(
            "accelerated_detection".to_string()
        )),
        "a value a domain currently governs is Vigil's choice, not a pin to mirror"
    );
    assert!(
        vigil::settings_reflection::control_state_reflects(&ControlState::SetByManagementServer),
        "a pushed value is exactly what reflection exists for"
    );
    assert!(
        vigil::settings_reflection::control_state_reflects(&ControlState::SetByYou),
        "a local pin reflects immediately"
    );
}

#[test]
fn no_supervisor_reports_the_value_as_mirrored_nowhere_with_the_reason() {
    // Unfakeable because it demands a specific reported reason and zero writes:
    // an implementation that swallowed the missing Supervisor and reported
    // success would fail the outcome, and one that tried the write anyway would
    // fail the counter.
    let client =
        RecordingSupervisor::new(current_options()).failing_read(ReflectionFailure::NoSupervisor);
    let mut ledger = EchoLedger::default();

    let outcome = reflect(
        &client,
        &mut ledger,
        INT_SETTING,
        &SettingValue::Int(45),
        true,
    )
    .expect("a missing Supervisor is a reported outcome, not an error the caller must handle");

    assert_eq!(
        outcome,
        ReflectionOutcome::NotAchieved(ReflectionFailure::NoSupervisor)
    );
    assert_eq!(client.write_count(), 0);
    assert_eq!(client.restart_count(), 0);
}

#[test]
fn a_setting_with_no_add_on_schema_target_is_stated_per_setting_not_discovered_at_write_time() {
    // Unfakeable because it asserts the double received NO calls at all. If the
    // absence of a reflection target were discovered by attempting the write
    // and reading the response, a read and a write would both be recorded.
    assert!(
        !addon_schema_keys()
            .iter()
            .any(|declared| declared.key == UNDECLARED_SETTING),
        "the fixture setting must genuinely have no add-on schema target"
    );

    let client = RecordingSupervisor::new(current_options());
    let mut ledger = EchoLedger::default();

    let outcome = reflect(
        &client,
        &mut ledger,
        UNDECLARED_SETTING,
        &SettingValue::Int(45),
        true,
    )
    .expect("a setting with no reflection target is a reported outcome");

    assert_eq!(
        outcome,
        ReflectionOutcome::NotAchieved(ReflectionFailure::NoSchemaTarget {
            setting: UNDECLARED_SETTING.to_string(),
        })
    );
    assert!(
        client.calls().is_empty(),
        "the answer comes from the declared schema, not from a failed write, got {:?}",
        client.calls()
    );
}

#[test]
fn a_rejected_or_unreachable_supervisor_never_discards_alters_or_un_applies_the_pushed_value() {
    // Unfakeable because the pushed value is read back out of the real store
    // before and after the failed reflection and compared field by field. An
    // implementation that rolled the value back, or downgraded its author, on a
    // reflection failure fails here.
    let directory = tempfile::tempdir().expect("temp directory");
    let hub = SettingsStore::open_hub_role(directory.path()).expect("hub-role settings store");
    hub.write_pushed_record(SettingRecord::pushed(
        INT_SETTING,
        Scope::node("node-a"),
        SettingValue::Int(45),
        "fleet policy",
    ))
    .expect("the hub-role handle writes the pushed record");

    let store = SettingsStore::open(directory.path()).expect("node-side settings store");
    let before = store
        .resolve(INT_SETTING, &node_target())
        .expect("the pushed value resolves");
    assert_eq!(before.requested, SettingValue::Int(45));
    assert_eq!(before.author, Author::Pushed);

    let client = RecordingSupervisor::new(current_options()).failing_write(
        ReflectionFailure::SupervisorRejected {
            reason: "supervisor unreachable".to_string(),
        },
    );
    let mut ledger = EchoLedger::default();
    let outcome = reflect(&client, &mut ledger, INT_SETTING, &before.requested, true)
        .expect("a rejected write is a reported outcome");

    assert_eq!(
        outcome,
        ReflectionOutcome::NotAchieved(ReflectionFailure::SupervisorRejected {
            reason: "supervisor unreachable".to_string(),
        })
    );

    let after = store
        .resolve(INT_SETTING, &node_target())
        .expect("the pushed value still resolves");
    assert_eq!(
        after.requested, before.requested,
        "a failed reflection never alters the pushed value"
    );
    assert_eq!(after.author, Author::Pushed);
    assert_eq!(
        store
            .records(INT_SETTING)
            .expect("stored records")
            .iter()
            .filter(|record| record.author == Author::Pushed)
            .count(),
        1,
        "a failed reflection never discards the pushed record"
    );
}

#[test]
fn a_value_the_add_on_schema_would_coerce_is_not_mirrored_and_is_reported_as_not_achieved_naming_what_runs()
 {
    // Unfakeable because the same function must answer both ways on the same
    // key: `30.7` into the integer option is refused, `30` is accepted. A
    // pre-validation that refused every float, or accepted everything, fails
    // one half. The reported failure must also name what actually runs, so an
    // outcome that merely says "not mirrored" fails too.
    assert_eq!(
        schema_key(INT_SETTING).declared_type,
        AddonSchemaType::Int,
        "the coercion fixture depends on this key being declared an integer"
    );
    assert!(
        !value_survives_schema_type(INT_SETTING, &SettingValue::Float(30.7)),
        "the schema silently truncates 30.7 to 30, so it does not survive"
    );
    assert!(
        value_survives_schema_type(INT_SETTING, &SettingValue::Int(30)),
        "an integer into an integer option survives untouched"
    );

    let client = RecordingSupervisor::new(current_options());
    let mut ledger = EchoLedger::default();
    let outcome = reflect(
        &client,
        &mut ledger,
        INT_SETTING,
        &SettingValue::Float(30.7),
        true,
    )
    .expect("an unrepresentable value is a reported outcome");

    match &outcome {
        ReflectionOutcome::NotAchieved(ReflectionFailure::WouldBeCoerced { setting, running }) => {
            assert_eq!(setting, INT_SETTING);
            assert!(
                running.contains("30.7"),
                "the report must name the value that actually runs, got {running}"
            );
        }
        other => panic!("a coercible value must be reported as not achieved, got {other:?}"),
    }
    assert_eq!(
        client.write_count(),
        0,
        "the coerced form is never mirrored"
    );
}

#[test]
fn schema_type_pre_validation_runs_before_the_write_not_after_the_response() {
    // Unfakeable because the double is configured so a write would SUCCEED,
    // and the assertion is on the recorded call order: no write call was ever
    // made. An implementation that posted first and inspected the response
    // afterwards would record a WriteOptions call here even though it reached
    // the same outcome, and would already have mangled the user's record.
    let client = RecordingSupervisor::new(current_options());
    let mut ledger = EchoLedger::default();

    let outcome = reflect(
        &client,
        &mut ledger,
        INT_SETTING,
        &SettingValue::Float(30.7),
        true,
    )
    .expect("an unrepresentable value is a reported outcome");

    assert!(
        matches!(
            outcome,
            ReflectionOutcome::NotAchieved(ReflectionFailure::WouldBeCoerced { .. })
        ),
        "got {outcome:?}"
    );
    assert!(
        !client
            .calls()
            .iter()
            .any(|call| matches!(call, Call::WriteOptions(_))),
        "pre-validation runs before the write, so no write was attempted, got {:?}",
        client.calls()
    );
    assert_eq!(client.restart_count(), 0);
}

#[test]
fn a_reflection_write_carries_the_complete_options_record() {
    // Unfakeable because every key of the record the Supervisor already held is
    // checked by name and value in the posted record, and the posted key count
    // is checked too. A write carrying only the changed key passes no part of
    // this.
    let current = current_options();
    let client = RecordingSupervisor::new(current.clone());
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
    assert_eq!(posted.len(), 1);
    let posted = &posted[0];

    for (key, value) in &current.entries {
        let carried = value_of(posted, key)
            .unwrap_or_else(|| panic!("the posted record must still carry `{key}`"));
        if key == INT_SETTING {
            assert_eq!(
                carried,
                &SettingValue::Int(45),
                "the one changed key carries the new value"
            );
        } else {
            assert_eq!(carried, value, "`{key}` must be posted unchanged");
        }
    }
    assert!(
        posted.entries.len() >= current.entries.len(),
        "a full replace never posts fewer keys than the record held, got {posted:?}"
    );
}

#[test]
fn a_write_omitting_a_schema_required_key_is_rejected_outright() {
    // Unfakeable because it proves the Supervisor's measured full-replace
    // validation bites (a hand-built partial post is refused) AND that the
    // production reflection path never produces such a post: every
    // schema-required key is present in the record it actually sends.
    let current = current_options();
    let required = required_schema_keys();
    assert!(
        !required.is_empty(),
        "the add-on schema must declare at least one required key for this contract to bite"
    );
    let dropped = required[0];

    let client = RecordingSupervisor::new(current.clone());
    let partial = OptionsRecord {
        entries: current
            .entries
            .iter()
            .filter(|(key, _)| key != dropped)
            .cloned()
            .collect(),
    };
    let refusal = client.write_options(&partial);
    assert!(
        matches!(refusal, Err(ReflectionFailure::SupervisorRejected { .. })),
        "a post omitting the required key `{dropped}` is rejected outright, got {refusal:?}"
    );

    let client = RecordingSupervisor::new(current);
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
    assert_eq!(posted.len(), 1);
    for key in &required {
        assert!(
            value_of(&posted[0], key).is_some(),
            "the reflection write must carry required key `{key}`"
        );
    }
}

#[test]
fn a_write_omitting_a_user_set_optional_key_would_erase_it_so_the_merge_preserves_it() {
    // Unfakeable because it checks the surviving optional key at two levels:
    // the merge itself, and the record actually posted after reflecting a
    // DIFFERENT key. Erasing a user-set optional key is silent on the wire, so
    // only reading the posted record catches it.
    assert!(
        !schema_key(USER_SET_OPTIONAL_KEY).required,
        "`{USER_SET_OPTIONAL_KEY}` must be declared schema-optional for this contract to bite"
    );

    let current = current_options();
    let merged = current.merge(INT_SETTING, &SettingValue::Int(45));
    assert_eq!(
        value_of(&merged, USER_SET_OPTIONAL_KEY),
        Some(&SettingValue::text(USER_SET_OPTIONAL_VALUE)),
        "the merge preserves the user-set optional key"
    );
    assert_eq!(
        value_of(&merged, INT_SETTING),
        Some(&SettingValue::Int(45)),
        "the merge applies the one change"
    );

    let client = RecordingSupervisor::new(current);
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
    assert_eq!(posted.len(), 1);
    assert_eq!(
        value_of(&posted[0], USER_SET_OPTIONAL_KEY),
        Some(&SettingValue::text(USER_SET_OPTIONAL_VALUE)),
        "the posted record must not silently erase the user's optional key"
    );
}

#[test]
fn the_reflection_client_uses_the_containers_own_supervisor_token() {
    // Unfakeable because a client reporting another add-on's token must produce
    // no write at all, while the same fixture with the container's own token
    // does write. A build that never checked the token source would post in
    // both cases and fail the first half.
    let borrowed = RecordingSupervisor::new(current_options())
        .with_token(TokenSource::Other("another add-on".to_string()));
    let mut ledger = EchoLedger::default();
    let outcome = reflect(
        &borrowed,
        &mut ledger,
        INT_SETTING,
        &SettingValue::Int(45),
        true,
    )
    .expect("a wrong-token client is a reported outcome");

    match &outcome {
        ReflectionOutcome::NotAchieved(ReflectionFailure::SupervisorRejected { reason }) => {
            assert!(
                reason.to_lowercase().contains("token"),
                "the reported reason must name the token, got {reason}"
            );
        }
        other => panic!("another add-on's token must not be used to write, got {other:?}"),
    }
    assert_eq!(
        borrowed.write_count(),
        0,
        "no options are written with a token that is not this container's"
    );

    let own = RecordingSupervisor::new(current_options());
    assert_eq!(own.token_source(), TokenSource::OwnContainer);
    let mut ledger = EchoLedger::default();
    reflect(&own, &mut ledger, INT_SETTING, &SettingValue::Int(45), true)
        .expect("reflection with the container's own token must not error");
    assert_eq!(
        own.write_count(),
        1,
        "the container's own token is what writes Vigil's own options"
    );
}

#[test]
fn a_missing_supervisor_permission_is_reported_as_a_reflection_failure_naming_the_manifest_declaration()
 {
    // Unfakeable because the reported failure has to carry the manifest
    // declaration an operator would add. A generic "reflection failed" carries
    // no remedy and fails the field assertion.
    let client = RecordingSupervisor::new(current_options()).failing_read(
        ReflectionFailure::MissingSupervisorPermission {
            manifest_declaration: "hassio_api".to_string(),
        },
    );
    let mut ledger = EchoLedger::default();

    let outcome = reflect(
        &client,
        &mut ledger,
        INT_SETTING,
        &SettingValue::Int(45),
        true,
    )
    .expect("a missing permission is a reported outcome");

    match &outcome {
        ReflectionOutcome::NotAchieved(ReflectionFailure::MissingSupervisorPermission {
            manifest_declaration,
        }) => {
            assert!(
                manifest_declaration.contains("hassio_api"),
                "the failure must name the manifest declaration the write needs, got {manifest_declaration}"
            );
        }
        other => panic!("a missing Supervisor permission must be reported, got {other:?}"),
    }
    assert_eq!(client.write_count(), 0);
    assert_eq!(client.restart_count(), 0);
}

// ---------------------------------------------------------------------------
// The full command path, under a Supervisor. The two halves above test the
// mirror and the restart in isolation; these two run the whole thing an
// operator runs — `vigil settings set …` with a Supervisor present — because
// that is the only place the restart policy and the per-setting application
// timing meet, and the seam between them is where the container was being
// replaced out from under the command that asked for the change.
// ---------------------------------------------------------------------------

/// A setting the running process takes on live, driven through the command an
/// operator types.
const LIVE_APPLIED_SETTING: &str = "detector_sample_frames";

/// A setting only a restart brings into force, driven the same way.
const STARTUP_ONLY_SETTING: &str = "decode_probe_deadline_secs";

#[test]
fn a_setting_this_process_takes_on_live_is_mirrored_without_replacing_the_container() {
    // Unfakeable because it asserts the mirror and the restart separately on
    // the real command path: the posted record has to carry the new value (so
    // an implementation that suppressed reflection entirely fails), and the
    // Supervisor must have been asked for no restart at all (so the shipped
    // unconditional restart fails). A value this process brings into force
    // itself has nothing a restart could add, and the restart it was taking
    // killed the very command that asked for the change.
    let directory = tempfile::tempdir().expect("temporary data directory");
    // A deployment that has been STARTED, which is the only kind a change can
    // be made against: `settings set` refuses a directory nobody has ever run
    // anything in rather than creating a store there, so a fixture that skipped
    // this would be asserting about a refusal.
    drop(
        vigil::settings_store::SettingsStore::open(directory.path())
            .expect("start this deployment's store"),
    );
    let client = RecordingSupervisor::new(current_options());

    let answer = vigil::settings_command::answer_with_reflection(
        directory.path(),
        // The store this deployment owns. Named rather than derived: a change
        // mirrored out of some other store would be another deployment's.
        &vigil::settings_store::SettingsStore::store_path(directory.path()),
        &format!("set {LIVE_APPLIED_SETTING} 4"),
        &client,
    );

    assert!(
        !answer.contains("settings-error"),
        "the change has to land before reflection is even reached, got {answer:?}"
    );
    let posted = client.posted_records();
    assert_eq!(posted.len(), 1, "the value is still mirrored, exactly once");
    assert_eq!(
        value_of(&posted[0], LIVE_APPLIED_SETTING),
        Some(&SettingValue::Int(4)),
        "the posted record must carry the value the operator set"
    );
    assert_eq!(
        client.restart_count(),
        0,
        "a value this process takes on live is never worth replacing the container for, got {:?}",
        client.calls()
    );
}

#[test]
fn a_setting_only_a_restart_brings_into_force_is_mirrored_and_then_restarted() {
    // The converse, so the fix above is a narrowing rather than a deletion: the
    // restart is exactly what makes a startup-only value take effect, and with
    // the policy at its default it still happens. An implementation that
    // dropped the restart for every setting fails here.
    let directory = tempfile::tempdir().expect("temporary data directory");
    // A deployment that has been STARTED, which is the only kind a change can
    // be made against: `settings set` refuses a directory nobody has ever run
    // anything in rather than creating a store there, so a fixture that skipped
    // this would be asserting about a refusal.
    drop(
        vigil::settings_store::SettingsStore::open(directory.path())
            .expect("start this deployment's store"),
    );
    let client = RecordingSupervisor::new(current_options());

    let answer = vigil::settings_command::answer_with_reflection(
        directory.path(),
        // The store this deployment owns. Named rather than derived: a change
        // mirrored out of some other store would be another deployment's.
        &vigil::settings_store::SettingsStore::store_path(directory.path()),
        &format!("set {STARTUP_ONLY_SETTING} 12"),
        &client,
    );

    assert!(
        !answer.contains("settings-error"),
        "the change has to land before reflection is even reached, got {answer:?}"
    );
    let posted = client.posted_records();
    assert_eq!(posted.len(), 1, "the value is mirrored, exactly once");
    assert_eq!(
        value_of(&posted[0], STARTUP_ONLY_SETTING),
        Some(&SettingValue::Int(12)),
        "the posted record must carry the value the operator set"
    );
    assert_eq!(
        client.restart_count(),
        1,
        "the restart is what brings a startup-only value into force, got {:?}",
        client.calls()
    );
}
