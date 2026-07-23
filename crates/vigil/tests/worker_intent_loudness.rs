//! Loud non-serving worker intent (owner steer 2026-07-13: "a remote node
//! started with detector/decode enabled must USE that ... worker service
//! must not depend on the node owning cameras"; and: "silent-standalone
//! continuation may also have swallowed a real enrollment error — C6
//! requires NAMED errors").
//!
//! The add-on options→env mapping can hand `FabricRuntime::start` an EMPTY
//! ticket string (`Some("")`, from a blank options.json field) rather than
//! `None`. `validate_fabric_ticket("")` correctly calls that malformed (an
//! operator-typed empty string), but a field the operator never touched at
//! all must never be treated as if they typed something and got it wrong —
//! `vigil::fabric_intent_from_args` (the exact resolution `vigil run` does)
//! must normalize an empty ticket to no ticket configured.
//!
//! The sibling honesty gap this diagnosis also names — a node with worker
//! intent that never actually serves must say so on its own doctor
//! rendering — drives the real compiled `vigil` binary, so it lives in
//! `crates/vigil-bin/tests/worker_intent_loudness.rs` instead.

#![cfg(feature = "fabric")]

#[test]
fn empty_string_ticket_from_the_addon_options_mapping_is_treated_as_no_ticket_configured() {
    // Serializes on process env — this crate's other env-driven config
    // tests carry the same caveat; run this file with --test-threads=1 if
    // run alongside anything else that touches VIGIL_FABRIC_TICKET/
    // VIGIL_FABRIC_HUB/VIGIL_DATA_DIR.
    let data_dir = tempfile::tempdir().expect("data dir");
    unsafe {
        std::env::set_var("VIGIL_DATA_DIR", data_dir.path());
        std::env::set_var("VIGIL_FABRIC_TICKET", "");
        std::env::remove_var("VIGIL_FABRIC_HUB");
    }

    let intent =
        vigil::fabric_intent_from_args(vec![]).expect("fabric intent must resolve from env");

    unsafe {
        std::env::remove_var("VIGIL_DATA_DIR");
        std::env::remove_var("VIGIL_FABRIC_TICKET");
    }

    assert_eq!(
        intent.fabric_ticket, None,
        "an empty-string ticket (a blank HAOS options.json field, mapped to \
         VIGIL_FABRIC_TICKET=\"\") must resolve to no ticket configured, the \
         same as the operator never having touched the field — never a \
         'ticket rejected'/'ticket is empty' enrollment error, which is for \
         an operator who actually typed a malformed value; got: {:?}",
        intent.fabric_ticket
    );
}
