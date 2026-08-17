//! The automatic-management gate: turning a domain off is what opens a
//! governed value to being set, a write while the domain is on is refused
//! naming the domain AND how to turn it off, and the only other accepted
//! shape is one take-over instruction that names the domain it disables,
//! applied disable-then-set, all-or-nothing.
//!
//! Why these are unfakeable: every assertion here reads back through the
//! real store — the typed refusal cause, the stored records, and the
//! resolved effective value — never through a return value the caller
//! under test also produced. A gate that "refused" without naming the
//! domain, or a take-over that set the value after failing to disable the
//! domain, leaves an observable difference in the store or in the typed
//! refusal, so neither can be papered over by wording.

use vigil::settings_backends::{DETECTION_BACKEND_SETTING, available_detection_backends};
use vigil::settings_domains::{ACCELERATED_DETECTION_DOMAIN, TakeOverInstruction, apply_take_over};
use vigil::settings_model::{
    Author, ControlState, RefusalKind, Scope, ScopeTarget, SettingValue, SettingsError, Surface,
};
use vigil::settings_store::SettingsStore;

const TENANT: &str = "acme";
const SITE: &str = "harbour-yard";
const NODE: &str = "node-a";

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: TENANT.to_string(),
        site: SITE.to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

/// A detection backend this artifact genuinely carries, read from the
/// compiled inventory rather than named here, so the gate tests never fail
/// for the unrelated reason that the value itself was unavailable.
fn a_real_backend() -> String {
    let available = available_detection_backends();
    assert!(
        !available.is_empty(),
        "every artifact carries at least one detection backend; the compiled inventory is empty"
    );
    available[0].to_string()
}

/// Lowercased, punctuation-flattened text with `_` preserved, so a phrase
/// check reads the same whether the surface writes `accelerated detection`,
/// `accelerated_detection`, or wraps either in punctuation.
fn normalized(text: &str) -> String {
    let flattened: String = text
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == '_' {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    flattened.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn turning_a_domain_off_then_pinning_a_governed_value_succeeds_and_the_pin_is_effective() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let store = SettingsStore::open(deployment.path()).expect("open the node-side settings store");
    let scope = Scope::node(NODE);
    let backend = a_real_backend();

    // The domain defaults to on, so the value is closed before anything is
    // done — this is the precondition the whole take-back promise rests on.
    let closed = store.set_local(
        DETECTION_BACKEND_SETTING,
        Surface::VigilSettings,
        scope.clone(),
        SettingValue::text(&backend),
    );
    assert!(
        closed.is_err(),
        "the accelerated-detection domain defaults to on, so the detection backend must not be \
         settable before it is turned off"
    );

    store
        .set_local(
            ACCELERATED_DETECTION_DOMAIN,
            Surface::VigilSettings,
            scope.clone(),
            SettingValue::Bool(false),
        )
        .expect("a domain switch is an ordinary setting and turning it off is an ordinary write");

    let record = store
        .set_local(
            DETECTION_BACKEND_SETTING,
            Surface::VigilSettings,
            scope.clone(),
            SettingValue::text(&backend),
        )
        .expect("with the domain off the governed value is open to being set");
    assert_eq!(record.author, Author::LocalExplicit);
    assert_eq!(record.surface, Surface::VigilSettings);
    assert_eq!(record.value, SettingValue::text(&backend));

    let effective = store
        .resolve(DETECTION_BACKEND_SETTING, &target())
        .expect("resolve the detection backend");
    assert_eq!(
        effective.requested,
        SettingValue::text(&backend),
        "the pin is what the node asks for once the domain is off"
    );
    assert_eq!(
        effective.control_state,
        ControlState::SetByYou,
        "a value the operator pinned with the domain off reads Set by you, not managed"
    );
    assert_eq!(effective.author, Author::LocalExplicit);
    assert_eq!(effective.surface, Surface::VigilSettings);
    assert!(
        !effective.grandfathered,
        "this pin was written after the domain existed, so it is not a grandfathered pin"
    );
    assert!(
        effective
            .dormant()
            .iter()
            .all(|held| held.record.setting != DETECTION_BACKEND_SETTING),
        "nothing is dormant while the domain is off"
    );
}

#[test]
fn a_write_to_a_governed_value_is_refused_naming_the_domain_and_how_to_turn_it_off() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let store = SettingsStore::open(deployment.path()).expect("open the node-side settings store");
    let scope = Scope::node(NODE);
    let backend = a_real_backend();

    let error = store
        .set_local(
            DETECTION_BACKEND_SETTING,
            Surface::VigilSettings,
            scope,
            SettingValue::text(&backend),
        )
        .expect_err("a write to a value inside an on domain is refused, never quietly stored");

    let SettingsError::Refused(refusal) = error else {
        panic!("a governed-value write is refused with a refusal, not another error class");
    };

    // The typed cause is the machine-readable half: whoever handles this
    // knows which domain closed the value without parsing prose.
    assert_eq!(
        refusal.kind,
        RefusalKind::GovernedValue {
            domain: ACCELERATED_DETECTION_DOMAIN.to_string()
        }
    );

    // Half one, asserted on its own: the cause names the governing domain.
    assert!(
        normalized(&refusal.cause).contains(ACCELERATED_DETECTION_DOMAIN),
        "the refusal cause must name the governing domain; got {:?}",
        refusal.cause
    );

    // Half two, asserted on its own: the remedy tells the operator how to
    // take the wheel — turn that domain off. A refusal carrying only the
    // cause fails this criterion.
    let remedy = normalized(&refusal.remedy);
    assert!(
        remedy.contains(ACCELERATED_DETECTION_DOMAIN),
        "the refusal remedy must name the domain switch to turn off; got {:?}",
        refusal.remedy
    );
    assert!(
        remedy.contains("turn") && remedy.contains("off"),
        "the refusal remedy must tell the operator to turn the domain off; got {:?}",
        refusal.remedy
    );

    let statement = refusal.statement();
    assert!(
        statement.contains(&refusal.cause) && statement.contains(&refusal.remedy),
        "the rendered refusal carries both halves; got {statement:?}"
    );

    assert!(
        store
            .records(DETECTION_BACKEND_SETTING)
            .expect("read the stored records")
            .is_empty(),
        "a refused write stores nothing — refusing at write time is the point"
    );
}

#[test]
fn a_hub_take_over_that_names_the_domain_applies_disable_then_set_all_or_nothing() {
    let backend = a_real_backend();

    // The accepted shape: the instruction names the domain it disables, and
    // both writes land in that order.
    let accepted = tempfile::tempdir().expect("temporary deployment directory");
    let instruction = TakeOverInstruction {
        disable_domain: Some(ACCELERATED_DETECTION_DOMAIN.to_string()),
        setting: DETECTION_BACKEND_SETTING.to_string(),
        scope: Scope::node(NODE),
        value: SettingValue::text(&backend),
        surface: Surface::ManagementServer,
        reason: "operator chose the detector for this node".to_string(),
    };
    let outcome = apply_take_over(accepted.path(), &instruction)
        .expect("a take-over naming its domain applies");
    assert_eq!(
        outcome.applied,
        vec![
            ACCELERATED_DETECTION_DOMAIN.to_string(),
            DETECTION_BACKEND_SETTING.to_string(),
        ],
        "the order is the contract: the domain is disabled first, then the value is set"
    );

    let store = SettingsStore::open(accepted.path()).expect("open the node-side settings store");
    let effective = store
        .resolve(DETECTION_BACKEND_SETTING, &target())
        .expect("resolve the detection backend after the take-over");
    assert_eq!(effective.requested, SettingValue::text(&backend));
    assert_eq!(
        effective.control_state,
        ControlState::SetByManagementServer,
        "the value the hub set reads as set by the management server"
    );
    assert_eq!(effective.author, Author::Pushed);

    // All-or-nothing: when the disable cannot become effective — here the
    // operator's own pin on the switch outranks the hub — the value is not
    // set either. This is the half that makes the take-over safe.
    let blocked = tempfile::tempdir().expect("temporary deployment directory");
    {
        let operator_handle =
            SettingsStore::open(blocked.path()).expect("open the node-side settings store");
        operator_handle
            .set_local(
                ACCELERATED_DETECTION_DOMAIN,
                Surface::VigilSettings,
                Scope::node(NODE),
                SettingValue::Bool(true),
            )
            .expect("the operator pins the domain on");
    }

    let error = apply_take_over(blocked.path(), &instruction)
        .expect_err("a take-over whose disable cannot take effect must not set the value");
    let SettingsError::Refused(refusal) = error else {
        panic!("the blocked take-over is refused with a refusal carrying its cause and remedy");
    };
    assert_eq!(
        refusal.kind,
        RefusalKind::GovernedValue {
            domain: ACCELERATED_DETECTION_DOMAIN.to_string()
        },
        "the value is still governed, because the domain never went off"
    );

    let blocked_store =
        SettingsStore::open(blocked.path()).expect("reopen the node-side settings store");
    let stored = blocked_store
        .records(DETECTION_BACKEND_SETTING)
        .expect("read the stored records");
    assert!(
        stored.is_empty(),
        "the value must NOT be set when the disable failed; found {stored:?}"
    );

    // All-or-nothing means NEITHER write happened, so the domain switch has
    // to be checked too — the operator's own pin is already on it from the
    // setup above, so "no records at all" would be the wrong assertion. What
    // must be absent is anything the take-over itself authored: the hub
    // writes at the pushed rank through the management-server surface.
    let switch_records = blocked_store
        .records(ACCELERATED_DETECTION_DOMAIN)
        .expect("read the stored domain-switch records");
    assert!(
        switch_records
            .iter()
            .all(|record| record.author != Author::Pushed
                && record.surface != Surface::ManagementServer),
        "the blocked take-over must leave no record of its own on the domain switch either; \
         found {switch_records:?}"
    );
    assert!(
        switch_records
            .iter()
            .any(|record| record.author == Author::LocalExplicit
                && record.surface == Surface::VigilSettings
                && record.value == SettingValue::Bool(true)),
        "the operator's own pin on the switch is untouched by the refused take-over; found \
         {switch_records:?}"
    );
    let still_governed = blocked_store
        .resolve(DETECTION_BACKEND_SETTING, &target())
        .expect("resolve the detection backend after the blocked take-over");
    assert_eq!(
        still_governed.control_state,
        ControlState::ManagedBy(ACCELERATED_DETECTION_DOMAIN.to_string()),
        "the domain is still on and still choosing the value"
    );
}

#[test]
fn a_hub_take_over_that_does_not_name_the_domain_is_refused() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let backend = a_real_backend();
    let instruction = TakeOverInstruction {
        disable_domain: None,
        setting: DETECTION_BACKEND_SETTING.to_string(),
        scope: Scope::node(NODE),
        value: SettingValue::text(&backend),
        surface: Surface::ManagementServer,
        reason: "pin the detector".to_string(),
    };

    let error = apply_take_over(deployment.path(), &instruction)
        .expect_err("a take-over that names no domain is the refused case, not a shortcut");
    let SettingsError::Refused(refusal) = error else {
        panic!("the unnamed-domain take-over is refused with a refusal");
    };
    assert_eq!(refusal.kind, RefusalKind::TakeOverWithoutDomain);
    assert!(
        !refusal.cause.trim().is_empty() && !refusal.remedy.trim().is_empty(),
        "every refusal carries a cause and a remedy; got {refusal:?}"
    );
    let statement = refusal.statement();
    assert!(
        statement.contains(&refusal.cause) && statement.contains(&refusal.remedy),
        "the rendered refusal carries both halves; got {statement:?}"
    );

    let store = SettingsStore::open(deployment.path()).expect("open the node-side settings store");
    assert!(
        store
            .records(DETECTION_BACKEND_SETTING)
            .expect("read the stored records")
            .is_empty(),
        "a refused take-over stores no value"
    );
    assert!(
        store
            .records(ACCELERATED_DETECTION_DOMAIN)
            .expect("read the stored domain-switch records")
            .is_empty(),
        "a refused take-over must not silently disable the domain either"
    );
}
