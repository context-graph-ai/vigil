//! The detection backend is declared loosely on the add-on surface — a
//! plain string, never an enumeration — and Vigil validates it against the
//! backends the running artifact actually carries, refusing with a reason
//! that names what IS available. Freezing the list into the manifest would
//! repeat the class-enumeration mistake one layer over.
//!
//! Why these are unfakeable: the "what is available" half of the refusal is
//! asserted against [`available_detection_backends`] itself, never against a
//! list written here — an artifact that carries a different backend set
//! still has to name its own, and a refusal that hardcoded one would fail
//! on the other shape. The offending value is proven unavailable by asking
//! the same inventory first, so this can never accidentally reject a real
//! backend. And acceptance is asserted through the store rather than
//! through the validator's own return value: the pin must be stored,
//! resolve as the requested value, and be reported honestly against what is
//! actually running on that deployment — which no validation stub can
//! satisfy by returning `Ok`. Nothing here brings a runtime up, so what is
//! asserted about running is what is true with none: nothing is running,
//! and the surface names what closes the gap.

use vigil::settings_backends::{
    DETECTION_BACKEND_SETTING, available_detection_backends, validate_backend,
};
use vigil::settings_domains::ACCELERATED_DETECTION_DOMAIN;
#[cfg(feature = "detect-burn-wgpu")]
use vigil::settings_model::{ControlState, ScopeTarget};
use vigil::settings_model::{RefusalKind, Scope, SettingValue, SettingsError, Surface};
use vigil::settings_store::SettingsStore;

const NODE: &str = "node-a";

/// A backend name no artifact carries, checked against the compiled
/// inventory rather than assumed.
const NOT_A_BACKEND: &str = "detector-from-a-brochure";

#[cfg(feature = "detect-burn-wgpu")]
fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: "acme".to_string(),
        site: "harbour-yard".to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

/// Open a store with the accelerated-detection domain already turned off,
/// so the gate is open and what is being tested is backend validation
/// rather than the gate's refusal.
fn store_with_the_domain_off(deployment: &std::path::Path) -> SettingsStore {
    let store = SettingsStore::open(deployment).expect("open the node-side settings store");
    store
        .set_local(
            ACCELERATED_DETECTION_DOMAIN,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::Bool(false),
        )
        .expect("turning the domain off is an ordinary write");
    store
}

#[test]
fn an_unavailable_detection_backend_is_refused_naming_what_this_artifact_carries() {
    let available = available_detection_backends();
    assert!(
        !available.is_empty(),
        "every artifact carries at least one detection backend; the compiled inventory is empty"
    );
    assert!(
        !available.contains(&NOT_A_BACKEND),
        "the probe value must genuinely be unavailable, or this test proves nothing; inventory: \
         {available:?}"
    );

    let refusal = validate_backend(DETECTION_BACKEND_SETTING, NOT_A_BACKEND)
        .expect_err("a backend this artifact does not carry is refused");
    assert_eq!(
        refusal.kind,
        RefusalKind::UnavailableBackend {
            setting: DETECTION_BACKEND_SETTING.to_string()
        }
    );
    assert!(
        refusal.cause.contains(NOT_A_BACKEND),
        "the refusal names the value that was asked for; got {:?}",
        refusal.cause
    );
    let statement = refusal.statement();
    for carried in &available {
        assert!(
            statement.contains(carried),
            "the refusal names what this artifact actually carries — every compiled backend, \
             read from the artifact rather than from a frozen list. Missing {carried:?} in \
             {statement:?}"
        );
    }
    assert!(
        statement.contains(&refusal.cause) && statement.contains(&refusal.remedy),
        "the rendered refusal carries both cause and remedy; got {statement:?}"
    );

    // The same refusal on the path an operator actually walks, and nothing
    // is stored: an invalid value is refused at write time rather than
    // stored and then ignored.
    let deployment = tempfile::tempdir().expect("temporary deployment directory");
    let store = store_with_the_domain_off(deployment.path());
    let error = store
        .set_local(
            DETECTION_BACKEND_SETTING,
            Surface::AddonOptions,
            Scope::node(NODE),
            SettingValue::text(NOT_A_BACKEND),
        )
        .expect_err("an unavailable backend is refused at write time");
    let SettingsError::Refused(write_refusal) = error else {
        panic!("an unavailable backend is refused with a refusal, not another error class");
    };
    assert_eq!(
        write_refusal.kind,
        RefusalKind::UnavailableBackend {
            setting: DETECTION_BACKEND_SETTING.to_string()
        }
    );
    assert!(
        store
            .records(DETECTION_BACKEND_SETTING)
            .expect("read the stored records")
            .is_empty(),
        "nothing is stored when the value is refused"
    );
}

/// Acceptance needs a backend that is genuinely compiled in; the
/// accelerated-detection feature is the shape that carries more than one,
/// so every compiled backend is exercised there.
///
/// Each backend gets its OWN deployment directory. "What is running here"
/// is a fact about one deployment, read through the per-instance
/// [`SettingsStore::running_backend`] — two deployments in one test process
/// must be able to disagree, which is exactly what a no-argument
/// process-global accessor could only deliver through hidden mutable state.
/// Writing every backend into one store would also have each write shadow
/// the last at the same surface and scope.
///
/// No runtime is brought up here, so this deliberately does NOT claim the
/// named backend is already running. It claims the honest shape: nothing is
/// running, and the surface says so by naming what closes the gap.
#[test]
#[cfg(feature = "detect-burn-wgpu")]
fn a_compiled_in_backend_is_accepted_and_is_pending_until_a_runtime_runs_it() {
    let available = available_detection_backends();
    assert!(
        available.len() >= 2,
        "an artifact compiling the accelerated detection backend carries the processor backend \
         as well; inventory: {available:?}"
    );

    for backend in &available {
        validate_backend(DETECTION_BACKEND_SETTING, backend).unwrap_or_else(|refusal| {
            panic!("{backend} is compiled in but was refused: {refusal:?}")
        });

        let deployment = tempfile::tempdir().expect("temporary deployment directory");
        let store = store_with_the_domain_off(deployment.path());

        let record = store
            .set_local(
                DETECTION_BACKEND_SETTING,
                Surface::AddonOptions,
                Scope::node(NODE),
                SettingValue::text(*backend),
            )
            .unwrap_or_else(|error| panic!("setting {backend} with the domain off: {error:?}"));
        assert_eq!(record.value, SettingValue::text(*backend));

        let effective = store
            .resolve(DETECTION_BACKEND_SETTING, &target())
            .expect("resolve the detection backend");
        assert_eq!(effective.requested, SettingValue::text(*backend));
        assert_eq!(
            effective.control_state,
            ControlState::SetByYou,
            "the surface attributes the value to the operator who set it"
        );

        // Requested, running and pending are one model and must agree with
        // each other. Nothing is running on this deployment, so the honest
        // answer is that running is absent and something — a restart —
        // closes the gap. A surface reporting no pending cause here would
        // be claiming the operator's backend is already in force when no
        // process is using it.
        let running = vigil::settings_application::in_force(DETECTION_BACKEND_SETTING);
        assert_eq!(
            running, None,
            "no runtime is up on this deployment, so no backend is running yet — and this is the \
             same read the detection path itself makes, not an accessor that exists for this \
             assertion"
        );
        assert_eq!(
            effective.running, None,
            "the projection and the per-deployment accessor answer the same question and must \
             not disagree; got {:?}",
            effective.running
        );
        assert!(
            effective.pending.is_some(),
            "a backend named while nothing is running is a pending value, and the surface names \
             what closes the gap; got {:?}",
            effective.pending
        );
    }
}
