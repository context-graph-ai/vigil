//! The seven values this node has to resolve before its store can answer are
//! ordinary store settings: an operator sets them with `vigil settings`, the
//! store is what governs them, and the value takes effect at the next start.
//!
//! The two ports, the recognition weights directory and its embedding space,
//! and the three fabric values are consumed early in a start — the liveness
//! surface binds before the store opens so a slow open is never mistaken for a
//! wedged add-on, and the store is opened WITH the vision embedder the
//! recognition pair builds. Being consumed early is a fact about WHEN a value
//! is read, not about who is allowed to choose it. Refusing to store a value
//! because this process already read something is the hidden-knob failure from
//! the other side: the operator is told to go somewhere else to set a value
//! the surface is otherwise happy to show them.
//!
//! Why these are unfakeable: every assertion reads back through the real store
//! — the stored records and the resolved effective value — never through the
//! return value of the call under test alone. A write that "succeeded" without
//! storing a record, or one stored under Vigil's own automatic author, leaves an
//! observable difference here.

use vigil::settings_model::{
    Author, ControlState, FABRIC_FALLBACK_HORIZON_MS_SETTING, FABRIC_HUB_SETTING,
    FABRIC_WORKER_LEASE_MS_SETTING, HEALTH_PORT_SETTING, RECOGNITION_SPACE_ID_SETTING,
    RECOGNITION_WEIGHTS_DIR_SETTING, REVIEW_PORT_SETTING, Scope, ScopeTarget, SettingValue,
    Surface,
};
use vigil::settings_store::SettingsStore;

const NODE: &str = "node-a";

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: NODE.to_string(),
        site: NODE.to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

/// The seven, each with a value that is deliberately not Vigil's own choice for
/// it, so a surface that quietly answered from the automatic floor could not
/// produce any of them by accident.
fn startup_resolved_values() -> Vec<(&'static str, SettingValue)> {
    vec![
        (HEALTH_PORT_SETTING, SettingValue::Int(18_391)),
        (REVIEW_PORT_SETTING, SettingValue::Int(18_392)),
        (
            RECOGNITION_WEIGHTS_DIR_SETTING,
            SettingValue::text("/srv/vigil/weights-chosen-by-the-operator"),
        ),
        (
            RECOGNITION_SPACE_ID_SETTING,
            SettingValue::text("faces-chosen-by-the-operator"),
        ),
        (FABRIC_HUB_SETTING, SettingValue::Bool(true)),
        (FABRIC_WORKER_LEASE_MS_SETTING, SettingValue::Int(123_456)),
        (FABRIC_FALLBACK_HORIZON_MS_SETTING, SettingValue::Int(7_654)),
    ]
}

#[test]
fn each_value_this_node_resolves_early_is_still_settable_through_vigil_settings() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let store = SettingsStore::open(deployment.path()).expect("open the node-side settings store");
    let scope = Scope::node(NODE);

    for (setting, value) in startup_resolved_values() {
        let record = store
            .set_local(
                setting,
                Surface::VigilSettings,
                scope.clone(),
                value.clone(),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "`vigil settings set {setting}` must be accepted: the store is the source of \
                     truth for this value and it takes effect at the next start, so refusing the \
                     write leaves the operator with a value they can see and cannot choose; got \
                     {error:?}"
                )
            });
        assert_eq!(
            record.value, value,
            "the record stores what the operator asked for, unaltered"
        );
        assert_eq!(
            record.author,
            Author::LocalExplicit,
            "a person at this deployment set it, so it is stored at the top of the ranking"
        );
        assert_eq!(record.surface, Surface::VigilSettings);

        let stored = store
            .records(setting)
            .expect("read the stored records back through the store");
        assert!(
            stored
                .iter()
                .any(|stored| stored.value == value && stored.author == Author::LocalExplicit),
            "the write is a stored record, not a return value: {setting} has no local record \
             carrying the operator's value; got {stored:?}"
        );
    }
}

#[test]
fn a_stored_early_resolved_value_outranks_vigils_own_choice_and_reads_set_by_you() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let store = SettingsStore::open(deployment.path()).expect("open the node-side settings store");
    let scope = Scope::node(NODE);

    for (setting, value) in startup_resolved_values() {
        let automatic = store
            .resolve(setting, &target())
            .unwrap_or_else(|error| panic!("resolve {setting} before anything is set: {error:?}"));
        assert_eq!(
            automatic.author,
            Author::Automatic,
            "sanity: nobody has chosen {setting} yet, so the floor is what answers"
        );
        assert_ne!(
            automatic.requested, value,
            "sanity: the fixture value for {setting} must differ from Vigil's own choice, or the \
             assertion below would pass without the store governing anything"
        );

        store
            .set_local(
                setting,
                Surface::VigilSettings,
                scope.clone(),
                value.clone(),
            )
            .unwrap_or_else(|error| panic!("set {setting} through `vigil settings`: {error:?}"));

        let effective = store
            .resolve(setting, &target())
            .unwrap_or_else(|error| panic!("resolve {setting} after it was set: {error:?}"));
        assert_eq!(
            effective.requested, value,
            "the stored value is what this node asks for; Vigil's own choice is the floor beneath \
             it, not the answer"
        );
        assert_eq!(
            effective.author,
            Author::LocalExplicit,
            "the value is attributed to the person who set it: {effective:?}"
        );
        assert_eq!(
            effective.surface,
            Surface::VigilSettings,
            "and to the surface they used: {effective:?}"
        );
        assert_eq!(
            effective.control_state,
            ControlState::SetByYou,
            "a value the operator chose reads Set by you — never Automatic, which would tell them \
             Vigil may revise a number Vigil is in fact honoring: {effective:?}"
        );
    }
}

#[test]
fn every_early_resolved_value_answers_on_the_operator_listing_with_the_stored_value() {
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let store = SettingsStore::open(deployment.path()).expect("open the node-side settings store");
    let scope = Scope::node(NODE);

    for (setting, value) in startup_resolved_values() {
        store
            .set_local(
                setting,
                Surface::VigilSettings,
                scope.clone(),
                value.clone(),
            )
            .unwrap_or_else(|error| panic!("set {setting} through `vigil settings`: {error:?}"));
    }

    let listing = store
        .listing(&target())
        .expect("read the operator listing for this deployment");
    for (setting, value) in startup_resolved_values() {
        let listed = listing
            .iter()
            .find(|entry| entry.setting == setting)
            .unwrap_or_else(|| {
                panic!(
                    "a value an operator can set is a value the operator surface answers for; \
                     {setting} is missing from the listing: {listing:?}"
                )
            });
        assert_eq!(
            listed.requested, value,
            "the listing shows what was stored for {setting}, not what this process happens to be \
             running: {listed:?}"
        );
        assert_eq!(
            listed.author,
            Author::LocalExplicit,
            "and names who chose it: {listed:?}"
        );
    }
}
