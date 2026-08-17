//! Deciding whether to collect real video before choosing a decode path.
//!
//! Hardware decoding is entered off a probe that passed on the machine in
//! front of you, and that probe needs real units from the real camera. Two
//! separate things can ask for it: the automatic domain being on, and an
//! operator turning that domain off and naming the hardware path themselves.
//! Reading only the first leaves the second collecting nothing and then asking
//! for a hardware verdict on an empty sample — which reports the hardware as
//! unusable when it was never shown anything.

use std::fs;
use std::path::{Path, PathBuf};

use vigil::decode::{real_probe_units_required, settled_decode_backend};
use vigil::settings_application::publish_pinned_backend;
use vigil::settings_backends::{
    DECODE_BACKEND_SETTING, HARDWARE_DECODE_BACKEND, SOFTWARE_DECODE_BACKEND,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn the_operator_pin_alone_is_enough_to_require_real_probe_units() {
    // The whole truth table in one test, because the pin is a single
    // process-wide fact and two tests moving it at once would read each
    // other's. The case the failure lives in is the second one: domain off,
    // hardware named by a person.
    for (domain_on, pin, required, why) in [
        (
            true,
            None,
            true,
            "automatic hardware decoding on asks for a real verdict",
        ),
        (
            false,
            Some(HARDWARE_DECODE_BACKEND),
            true,
            "an operator naming the hardware path with the automation off is asking for the \
             same real verdict, and is entitled to have their pin tested against real camera \
             data rather than refused on an empty sample",
        ),
        (
            false,
            Some(SOFTWARE_DECODE_BACKEND),
            false,
            "software named outright needs no probe at all",
        ),
        (
            false,
            None,
            false,
            "nothing on and nothing named means no device is touched",
        ),
    ] {
        publish_pinned_backend(DECODE_BACKEND_SETTING, pin.map(str::to_string));
        assert_eq!(
            real_probe_units_required(domain_on),
            required,
            "domain_on={domain_on} pin={pin:?}: {why}"
        );
        // The two answers are one decision seen from opposite sides: an answer
        // that settles without a probe is exactly an answer needing no units.
        assert_eq!(
            settled_decode_backend(domain_on).is_none(),
            required,
            "the settled answer and the probe requirement must never disagree \
             (domain_on={domain_on} pin={pin:?})"
        );
    }
    publish_pinned_backend(DECODE_BACKEND_SETTING, None);
}

#[test]
fn the_live_pipeline_gate_is_derived_from_effective_intent_not_from_the_domain_switch() {
    // The predicate above is only worth having if the live path asks it. The
    // stream session decides whether to collect units before it decides
    // anything else, so this reads that decision where it is made: gating on
    // the bare domain-switch field is the defect, and naming the predicate is
    // the fix.
    let path = repo_root().join("crates/vigil/src/media_pipeline.rs");
    let source = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    assert!(
        source.contains("real_probe_units_required"),
        "the live session must decide whether to collect probe units through the effective \
         intent, so a pinned hardware path collects real video like an automatic one"
    );
    assert!(
        !source.contains("decode_options.hardware_decoding && {"),
        "the probe gate must not be the automatic-domain switch alone; that is the read that \
         left a pinned hardware path with nothing to judge"
    );
}
