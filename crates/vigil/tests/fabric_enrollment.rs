//! Enrollment surfaces + errors (criterion C6, the runtime-bring-up half —
//! the HAOS options-surface half is `addon_config_surface.rs`): the main
//! vigil's own status/doctor/log surfaces print the ready-to-use join
//! instruction (ticket + command) when this node carries the hub; a
//! hub-off node still teaches the one-line grow instruction; a malformed
//! ticket at enroll produces an error that names the fix and the process
//! continues standalone — no panic, no hang.
//!
//! RED: `vigil::fabric::FabricRuntime::start`, `::join_instruction`, and
//! `::validate_fabric_ticket` are `todo!()` pending the implementation
//! pass.

#![cfg(feature = "fabric")]

use vigil::fabric::FabricRuntime;

#[tokio::test]
async fn main_prints_join_instruction_and_malformed_ticket_names_fix() {
    // A hub-role node (fabric_hub = true, no ticket to join with — it IS the
    // join point): its own status/doctor/log surfaces must print the
    // ready-to-use join instruction: the current ticket plus the exact
    // command a second machine runs.
    let hub_dir = tempfile::tempdir().expect("hub data dir");
    let hub_runtime = FabricRuntime::start(hub_dir.path(), None, true)
        .await
        .expect("a hub-role node must stand up even with no ticket configured");
    assert!(
        hub_runtime.hub_endpoint.is_some(),
        "fabric_hub=true must embed the hub in this process (Design decision E)"
    );
    let instruction = hub_runtime.join_instruction();
    assert!(
        instruction.contains("fabric-join"),
        "the join instruction must be named, not free text: {instruction}"
    );
    assert!(
        instruction.contains("ticket="),
        "the join instruction must carry the CURRENT ticket: {instruction}"
    );
    assert!(
        instruction.contains("command="),
        "the join instruction must carry the exact command a second machine runs: {instruction}"
    );

    // A hub-off node (today's default: fabric_hub = false, no ticket): it
    // still stands up standalone, and its own output teaches the ONE-LINE
    // grow instruction (how to become a join point), never silence about
    // the fabric feature's existence.
    let lone_dir = tempfile::tempdir().expect("lone node data dir");
    let lone_runtime = FabricRuntime::start(lone_dir.path(), None, false)
        .await
        .expect("an unenrolled, hub-off node must still stand up standalone");
    assert!(
        lone_runtime.hub_endpoint.is_none(),
        "fabric_hub=false must never silently embed a hub"
    );
    let grow_instruction = lone_runtime.join_instruction();
    assert!(
        !grow_instruction.contains("ticket="),
        "a hub-off node has no ticket to advertise yet: {grow_instruction}"
    );
    assert!(
        grow_instruction.to_ascii_lowercase().contains("hub"),
        "the grow instruction must name enabling the hub role: {grow_instruction}"
    );

    // A malformed ticket at enroll: the error names the fix, never a panic
    // or a hang, and the node still comes up standalone rather than
    // refusing to start entirely.
    let malformed_dir = tempfile::tempdir().expect("malformed-ticket data dir");
    let malformed_ticket = "not-a-real-fabric-ticket";
    assert!(
        FabricRuntime::validate_fabric_ticket(malformed_ticket).is_err(),
        "a malformed ticket must fail validation before ever dialing it"
    );
    let error = FabricRuntime::validate_fabric_ticket(malformed_ticket)
        .expect_err("validation must reject a malformed ticket");
    assert!(
        error.to_ascii_lowercase().contains("ticket"),
        "the error must name what is wrong: {error}"
    );

    let result = FabricRuntime::start(malformed_dir.path(), Some(malformed_ticket), false).await;
    match result {
        Ok(runtime) => {
            // Enrollment failure must never prevent standalone startup.
            assert!(
                runtime.hub_endpoint.is_none(),
                "a hub_off node with a malformed ticket must still stand up without a hub"
            );
        }
        Err(error) => {
            assert!(
                error.to_string().to_ascii_lowercase().contains("ticket"),
                "a malformed-ticket enrollment failure must name the fix: {error}"
            );
        }
    }
}
