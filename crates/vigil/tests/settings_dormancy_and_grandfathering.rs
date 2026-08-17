//! Nothing in the settings model deletes a record. Turning a domain back on
//! over a pin puts the pin to sleep and labels it held; turning the domain
//! off again wakes exactly the value that was pinned. And a pin written
//! before a domain grew to govern its setting keeps running — the gate
//! closes for new writes, not for values already standing — until the
//! operator resets it deliberately.
//!
//! Why these are unfakeable: dormancy is asserted twice over, once through
//! [`SettingsStore::records`] (the record is still stored) and once through
//! the resolved [`EffectiveSetting::held`] list (it is stored *and*
//! labelled with why it is not effective), so an implementation that
//! deleted the pin and re-derived a plausible label fails the first check
//! while one that hid the label fails the second. The wake assertion
//! compares the woken value byte-for-byte against what was pinned, so a
//! re-derived or normalized value is not accepted. Grandfathering is driven
//! entirely by the record's `domain_generation` against the domain's own
//! declared `generation` — never by comparing a timestamp against a code
//! release, which no test could control and no deployment could reproduce.

use vigil::settings_backends::{DETECTION_BACKEND_SETTING, available_detection_backends};
use vigil::settings_domains::{
    ACCELERATED_DETECTION_DOMAIN, DomainDeclaration, GateDecision, gate_write, governing_domain,
};
use vigil::settings_model::{
    Author, ControlState, HeldReason, Scope, ScopeTarget, SettingRecord, SettingValue, Surface,
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

fn a_real_backend() -> String {
    let available = available_detection_backends();
    assert!(
        !available.is_empty(),
        "every artifact carries at least one detection backend; the compiled inventory is empty"
    );
    available[0].to_string()
}

/// The exact bytes of a text setting value, so "unchanged" means unchanged
/// rather than "compares equal after some normalization".
fn text_bytes(value: &SettingValue) -> Vec<u8> {
    match value {
        SettingValue::Text(text) => text.as_bytes().to_vec(),
        other => panic!("expected a text setting value, got {other:?}"),
    }
}

fn accelerated_detection_declaration() -> DomainDeclaration {
    governing_domain(DETECTION_BACKEND_SETTING).unwrap_or_else(|| {
        panic!("the accelerated-detection domain must govern {DETECTION_BACKEND_SETTING}")
    })
}

/// Turn the domain off, pin the backend, and hand back the store, the pin's
/// value, and the scope — the state both dormancy tests start from.
fn store_with_a_pin_under_an_off_domain(
    deployment: &std::path::Path,
) -> (SettingsStore, SettingValue, Scope) {
    let store = SettingsStore::open(deployment).expect("open the node-side settings store");
    let scope = Scope::node(NODE);
    let pinned = SettingValue::text(a_real_backend());

    store
        .set_local(
            ACCELERATED_DETECTION_DOMAIN,
            Surface::VigilSettings,
            scope.clone(),
            SettingValue::Bool(false),
        )
        .expect("turning the domain off is an ordinary write");
    store
        .set_local(
            DETECTION_BACKEND_SETTING,
            Surface::VigilSettings,
            scope.clone(),
            pinned.clone(),
        )
        .expect("with the domain off the governed value is open to being set");

    let effective = store
        .resolve(DETECTION_BACKEND_SETTING, &target())
        .expect("resolve the detection backend");
    assert_eq!(
        effective.control_state,
        ControlState::SetByYou,
        "the pin must really be effective before the dormancy behavior means anything"
    );

    (store, pinned, scope)
}

#[test]
fn turning_the_domain_back_on_holds_the_pin_dormant_and_never_deletes_it() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let (store, pinned, scope) = store_with_a_pin_under_an_off_domain(deployment.path());

    store
        .set_local(
            ACCELERATED_DETECTION_DOMAIN,
            Surface::VigilSettings,
            scope,
            SettingValue::Bool(true),
        )
        .expect("turning the domain back on is an ordinary write");

    // Still stored: nothing in this model deletes a record.
    let stored = store
        .records(DETECTION_BACKEND_SETTING)
        .expect("read the stored records");
    assert!(
        stored.iter().any(|record| record.value == pinned
            && record.surface == Surface::VigilSettings
            && record.author == Author::LocalExplicit
            && !record.reset),
        "turning the domain on must not delete the operator's pin; stored records: {stored:?}"
    );

    let effective = store
        .resolve(DETECTION_BACKEND_SETTING, &target())
        .expect("resolve the detection backend with the domain on");
    assert_eq!(
        effective.control_state,
        ControlState::ManagedBy(ACCELERATED_DETECTION_DOMAIN.to_string()),
        "with the domain on, the domain is choosing the value again"
    );
    // What carries "the domain's own choice is what runs, not the sleeping
    // pin" is WHO the effective value is attributed to, never the value's
    // own text: in the default feature shape the compiled inventory holds a
    // single detection backend, so the domain's choice and the pin name the
    // same string. A value-inequality assertion here would be unsatisfiable
    // however faithfully the domain is implemented.
    assert_eq!(
        effective.author,
        Author::Automatic,
        "the effective value is vigil's own choice under the on domain, not the operator's \
         record, whatever string each of them names"
    );

    // Labelled held, on the face of the surface, with the reason.
    let dormant: Vec<_> = effective
        .held
        .iter()
        .filter(|held| {
            held.reason
                == HeldReason::DormantUnderDomain {
                    domain: ACCELERATED_DETECTION_DOMAIN.to_string(),
                }
        })
        .collect();
    assert_eq!(
        dormant.len(),
        1,
        "the pin appears exactly once as held dormant under the domain; held: {:?}",
        effective.held
    );
    assert_eq!(dormant[0].record.value, pinned);
    assert!(
        !dormant[0].statement.trim().is_empty(),
        "a held record carries the sentence the operator surface renders for it"
    );
    assert_eq!(
        effective.dormant().len(),
        1,
        "the dormant selector reports the same one record"
    );
}

#[test]
fn turning_the_domain_off_again_wakes_the_dormant_pin_unchanged() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let (store, pinned, scope) = store_with_a_pin_under_an_off_domain(deployment.path());

    store
        .set_local(
            ACCELERATED_DETECTION_DOMAIN,
            Surface::VigilSettings,
            scope.clone(),
            SettingValue::Bool(true),
        )
        .expect("turning the domain back on is an ordinary write");
    store
        .set_local(
            ACCELERATED_DETECTION_DOMAIN,
            Surface::VigilSettings,
            scope,
            SettingValue::Bool(false),
        )
        .expect("turning the domain off again is an ordinary write");

    let effective = store
        .resolve(DETECTION_BACKEND_SETTING, &target())
        .expect("resolve the detection backend after the domain goes off again");
    assert_eq!(
        text_bytes(&effective.requested),
        text_bytes(&pinned),
        "the woken value is byte-identical to what was pinned, not re-derived"
    );
    assert_eq!(effective.control_state, ControlState::SetByYou);
    assert_eq!(effective.author, Author::LocalExplicit);
    assert_eq!(effective.surface, Surface::VigilSettings);
    assert!(
        effective.dormant().is_empty(),
        "nothing is dormant once the domain is off again; held: {:?}",
        effective.held
    );
}

#[test]
fn a_pin_that_predates_a_domain_widening_stays_effective_and_is_labelled_as_predating() {
    let declaration = accelerated_detection_declaration();
    assert!(
        declaration.generation >= 1,
        "membership generations must start at 1 so a record written before the membership \
         existed is representable; {ACCELERATED_DETECTION_DOMAIN} declares generation {}",
        declaration.generation
    );
    let earlier_generation = declaration.generation - 1;

    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let store = SettingsStore::open(deployment.path()).expect("open the node-side settings store");
    let scope = Scope::node(NODE);
    let pinned = SettingValue::text(a_real_backend());

    // A pin written while the setting was ungoverned: same record shape, an
    // earlier membership generation. The gate is what reads that difference.
    let mut record = SettingRecord::local(
        DETECTION_BACKEND_SETTING,
        Surface::AddonOptions,
        scope.clone(),
        pinned.clone(),
        "the operator chose this detector before the domain governed it",
    );
    record.domain_generation = earlier_generation;
    store
        .write_record(record)
        .expect("a record written at an earlier membership generation is stored");

    assert_eq!(
        gate_write(
            DETECTION_BACKEND_SETTING,
            &scope,
            &[(ACCELERATED_DETECTION_DOMAIN.to_string(), true)],
            Some(earlier_generation),
        ),
        GateDecision::Grandfathered {
            domain: ACCELERATED_DETECTION_DOMAIN.to_string()
        },
        "the gate closes for new writes, not for values already standing"
    );

    let effective = store
        .resolve(DETECTION_BACKEND_SETTING, &target())
        .expect("resolve the detection backend with the domain on over a predating pin");
    assert_eq!(
        text_bytes(&effective.requested),
        text_bytes(&pinned),
        "the predating pin is still what runs"
    );
    assert_eq!(effective.author, Author::LocalExplicit);
    assert_eq!(effective.surface, Surface::AddonOptions);
    assert!(
        effective.grandfathered,
        "the surface labels the effective value as predating the domain that now governs it"
    );
    assert!(
        effective.dormant().is_empty(),
        "a predating pin is not put to sleep; that would change what the machine does with \
         nobody having decided anything. held: {:?}",
        effective.held
    );

    // A pin written at the current generation under the same on domain is
    // the contrasting case — it is not grandfathered — so the flag above is
    // reading the generation, not merely reporting a constant.
    let contemporary_deployment = tempfile::tempdir().expect("temporary deployment directory");
    let contemporary_store = SettingsStore::open(contemporary_deployment.path())
        .expect("open a second node-side settings store");
    let mut contemporary = SettingRecord::local(
        DETECTION_BACKEND_SETTING,
        Surface::AddonOptions,
        scope,
        pinned,
        "the operator chose this detector after the domain governed it",
    );
    contemporary.domain_generation = declaration.generation;
    contemporary_store
        .write_record(contemporary)
        .expect("a record written at the current membership generation is stored");
    let contemporary_effective = contemporary_store
        .resolve(DETECTION_BACKEND_SETTING, &target())
        .expect("resolve the detection backend over a contemporary pin");
    assert!(
        !contemporary_effective.grandfathered,
        "a pin written at the current membership generation does not predate the domain"
    );
    assert_eq!(
        contemporary_effective.control_state,
        ControlState::ManagedBy(ACCELERATED_DETECTION_DOMAIN.to_string()),
        "a contemporary pin under an on domain sleeps; the domain chooses"
    );
}

#[test]
fn only_an_explicit_reset_hands_a_grandfathered_pin_to_its_new_domain() {
    let declaration = accelerated_detection_declaration();
    assert!(
        declaration.generation >= 1,
        "membership generations must start at 1 so a predating record is representable"
    );

    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let scope = Scope::node(NODE);
    let pinned = SettingValue::text(a_real_backend());

    {
        let store =
            SettingsStore::open(deployment.path()).expect("open the node-side settings store");
        let mut record = SettingRecord::local(
            DETECTION_BACKEND_SETTING,
            Surface::AddonOptions,
            scope.clone(),
            pinned.clone(),
            "the operator chose this detector before the domain governed it",
        );
        record.domain_generation = declaration.generation - 1;
        store
            .write_record(record)
            .expect("a record written at an earlier membership generation is stored");
    }

    // Restarting is not handing the value over. The pin is still what runs.
    let store = SettingsStore::open(deployment.path()).expect("reopen the settings store");
    let after_restart = store
        .resolve(DETECTION_BACKEND_SETTING, &target())
        .expect("resolve the detection backend after a restart");
    assert!(
        after_restart.grandfathered,
        "a restart does not hand a predating pin to the domain"
    );
    assert_eq!(
        text_bytes(&after_restart.requested),
        text_bytes(&pinned),
        "the predating pin still runs after a restart"
    );

    // The deliberate act: the operator resets their own pin.
    let outcome = store
        .reset_local(DETECTION_BACKEND_SETTING, Surface::AddonOptions, &scope)
        .expect("an explicit reset of the operator's own record");
    assert_eq!(outcome.setting, DETECTION_BACKEND_SETTING);
    assert_eq!(
        outcome.dropped_to.control_state,
        ControlState::ManagedBy(ACCELERATED_DETECTION_DOMAIN.to_string()),
        "the reset hands the value to the domain that now governs it"
    );
    assert!(
        !outcome.dropped_to.grandfathered,
        "there is no predating pin left to grandfather"
    );
    // "What the setting dropped to is the domain's choice, not the reset
    // pin" is carried by the control state and the author — the domain is
    // choosing now — not by the value's text: with one detection backend
    // compiled in, the domain's choice and the reset pin name the same
    // string, so a value-inequality assertion could never hold.
    assert_eq!(
        outcome.dropped_to.author,
        Author::Automatic,
        "the setting dropped to vigil's own choice under the domain, not back onto the operator's \
         reset record"
    );
    assert!(
        !outcome.statement.trim().is_empty(),
        "a reset says what the setting dropped to"
    );

    // A reset is a record, not a delete.
    let stored = store
        .records(DETECTION_BACKEND_SETTING)
        .expect("read the stored records");
    assert!(
        stored
            .iter()
            .any(|record| record.surface == Surface::AddonOptions && record.reset),
        "the reset is stored as a record on the surface that authored it; records: {stored:?}"
    );

    let after_reset = store
        .resolve(DETECTION_BACKEND_SETTING, &target())
        .expect("resolve the detection backend after the reset");
    assert_eq!(
        after_reset.control_state,
        ControlState::ManagedBy(ACCELERATED_DETECTION_DOMAIN.to_string())
    );
    assert!(!after_reset.grandfathered);
}
