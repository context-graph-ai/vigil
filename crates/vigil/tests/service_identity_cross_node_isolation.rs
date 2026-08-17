//! The read-side complement of `node_key_scope_isolation.rs`, for the
//! service identity specifically: a `service_identity` record stored at
//! ANOTHER node's scope target must never resolve as this node's identity,
//! and a record stored at THIS node's own target must. `service_identity::
//! persisted` (`crates/vigil/src/service_identity.rs`) is the seam under
//! test — see its `identity_target` doc comment. Resolution filters on the
//! reading node's own scope target; taking the highest-ranking record for
//! the setting from any node is what this file exists to catch.
//!
//! `node_key_scope_isolation.rs` pins the same isolation for an ordinary
//! setting, at the `SettingsStore::resolve` seam. This file pins it for the
//! identity record specifically, at the `service_identity::persisted` seam
//! it actually reads through — the two are separate call paths and neither
//! test proves the other.

use std::fs;

use vigil::node_key;
use vigil::service_identity::{identity_paths, persisted, resolve_persisted};
use vigil::settings_model::{SERVICE_IDENTITY_SETTING, Scope, SettingRecord, SettingValue};
use vigil::settings_store::SettingsStore;

/// A scope target fabricated to look exactly like another node's key — the
/// shape a synced hub converges into this store from a fleet, never one this
/// node generated for itself.
const FOREIGN_KEY: &str = "11111111-1111-4111-8111-111111111111";
const FOREIGN_IDENTITY: &str = "someone_elses_node";
const SITE_NAME: &str = "front house";

#[test]
fn a_service_identity_record_at_another_nodes_scope_target_never_resolves_here_but_this_nodes_own_does()
 {
    let root = tempfile::tempdir().expect("enclosing temp dir");
    let data_dir = root.path().join("data");
    fs::create_dir_all(&data_dir).expect("create the data directory");
    let store_path = identity_paths(&data_dir).store;

    let store = SettingsStore::open(&data_dir).expect("open the settings store");

    // A record another node wrote at ITS OWN key — exactly the shape a
    // synced hub converges into this store from a fleet.
    let foreign_record = SettingRecord::automatic(
        SERVICE_IDENTITY_SETTING,
        Scope::node(FOREIGN_KEY),
        SettingValue::text(FOREIGN_IDENTITY),
        "written by another node, at another node's own key",
    );
    store
        .write_record(foreign_record)
        .expect("store the foreign node's identity record");

    // This node has not written anything yet. Reading resolves to nothing —
    // never to the foreign node's row, even though it is the only record in
    // the table.
    let before_own = persisted(&store, &data_dir).expect("read this node's identity");
    assert!(
        before_own.is_none(),
        "a record at another node's scope target must never resolve as this node's identity \
         before this node has one of its own, but persisted() returned {before_own:?}"
    );

    // This node now goes through its real first-start path, deriving and
    // persisting its own record at its own generated key.
    let own = resolve_persisted(&store_path, SITE_NAME)
        .expect("this node's first start resolves and persists its own identity");
    assert_ne!(
        own.value, FOREIGN_IDENTITY,
        "this node's own derived identity must not coincide with the foreign fixture value, or \
         this test proves nothing"
    );

    // Sanity: this node's own generated key really does differ from the
    // fabricated foreign one.
    let own_key = node_key::recorded(&data_dir).expect("this node now has a recorded key");
    assert_ne!(
        own_key, FOREIGN_KEY,
        "sanity: this node's own key must differ from the fabricated foreign key"
    );

    // A record at THIS node's own target resolves.
    let resolved = persisted(&store, &data_dir)
        .expect("read this node's identity")
        .expect("this node's own record must resolve now that one exists");
    assert_eq!(
        resolved.value, own.value,
        "this node's own record, at its own scope target, must resolve as its identity"
    );

    // The foreign record, still sitting in the same shared table, is still
    // not picked — a fleet's converged store keeps every node's row, but
    // each node reads only its own.
    assert_ne!(
        resolved.value, FOREIGN_IDENTITY,
        "the foreign node's record, still sitting in the same shared table, must never be read \
         as this node's identity"
    );
}
