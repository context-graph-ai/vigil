//! Two deployments that share the Home Assistant add-on's fixed directory
//! basename (`/data` on every node) must still be two different nodes to the
//! travelling settings tables — otherwise a fleet converges on one shared
//! scope target and the newest write silently replaces every other node's
//! record, service identity first. `crates/vigil/src/node_key.rs` is the fix:
//! a generated key, persisted once and read back forever, stands in for the
//! directory name as the scope target.
//!
//! These tests pin the contract from the outside, at the public `node_key`
//! and `settings_projection` surfaces, never by asserting anything about the
//! key's own shape.

use std::fs;

use uuid::Uuid;
use vigil::node_key;
use vigil::settings_model::{Scope, ScopeTarget, SettingValue, Surface};
use vigil::settings_projection::{report_by_direct_read, report_degraded};
use vigil::settings_store::SettingsStore;

const SETTING: &str = "detector_sample_frames";

fn target_for_key(key: &str) -> ScopeTarget {
    ScopeTarget {
        tenant: key.to_string(),
        site: key.to_string(),
        node: key.to_string(),
        camera: None,
    }
}

/// Two deployment directories named `data`, under different parents — the
/// exact shape the add-on gives every node — generate distinct node keys;
/// once their rows land in one shared store (what a synced hub converges
/// them into), each deployment resolves only the record it wrote; and the
/// key each deployment generated survives being asked for again, the way a
/// restart would ask for it.
#[test]
fn two_deployments_sharing_a_data_dir_basename_resolve_at_different_scope_targets() {
    let root = tempfile::tempdir().expect("enclosing temp dir");
    let a_data = root.path().join("a").join("data");
    let b_data = root.path().join("b").join("data");
    fs::create_dir_all(&a_data).expect("create deployment a's data dir");
    fs::create_dir_all(&b_data).expect("create deployment b's data dir");
    assert_eq!(
        a_data.file_name(),
        b_data.file_name(),
        "both deployments share the basename `data`, the add-on's fixed shape"
    );

    let key_a = node_key::scope_name(&a_data);
    let key_b = node_key::scope_name(&b_data);
    assert_ne!(
        key_a, key_b,
        "two deployments must generate distinct node keys even though their \
         directories share a name"
    );

    // Both nodes' rows land in ONE shared store, the shape a synced hub
    // converges separate deployments' pushes into.
    let shared = tempfile::tempdir().expect("shared store dir");
    let store = SettingsStore::open(shared.path()).expect("open the shared store");
    store
        .set_local(
            SETTING,
            Surface::VigilSettings,
            Scope::node(key_a.clone()),
            SettingValue::Int(11),
        )
        .expect("deployment a writes its own record");
    store
        .set_local(
            SETTING,
            Surface::VigilSettings,
            Scope::node(key_b.clone()),
            SettingValue::Int(22),
        )
        .expect("deployment b writes its own record");

    let resolved_a = store
        .resolve(SETTING, &target_for_key(&key_a))
        .expect("resolve at deployment a's scope target");
    let resolved_b = store
        .resolve(SETTING, &target_for_key(&key_b))
        .expect("resolve at deployment b's scope target");
    assert_eq!(
        resolved_a.requested,
        SettingValue::Int(11),
        "deployment a reads back only its own record, not b's: {resolved_a:?}"
    );
    assert_eq!(
        resolved_b.requested,
        SettingValue::Int(22),
        "deployment b reads back only its own record, not a's: {resolved_b:?}"
    );

    // The key survives a restart: asking again for the same directory
    // returns the identical key rather than generating a new one.
    let key_a_again = node_key::scope_name(&a_data);
    let key_b_again = node_key::scope_name(&b_data);
    assert_eq!(
        key_a, key_a_again,
        "deployment a's node key must survive being asked for again"
    );
    assert_eq!(
        key_b, key_b_again,
        "deployment b's node key must survive being asked for again"
    );
}

/// A pure read of a deployment that has never started — no store, no key —
/// must leave the directory exactly as it found it: no `node-key` file
/// appears just because someone asked what this deployment would run.
#[test]
fn a_read_of_a_never_started_deployment_creates_no_node_key_file() {
    let root = tempfile::tempdir().expect("enclosing temp dir");
    let data_dir = root.path().join("data");
    fs::create_dir_all(&data_dir).expect("create the never-started data dir");

    assert_eq!(
        node_key::recorded(&data_dir),
        None,
        "a never-started deployment has no recorded key"
    );

    let report = report_by_direct_read(&data_dir, &SettingsStore::store_path(&data_dir))
        .expect("read a never-started deployment");
    assert!(
        !report.settings.is_empty(),
        "the read still answers with the automatic floor: {report:?}"
    );

    assert!(
        !node_key::key_path(&data_dir).exists(),
        "a pure read must never create {}",
        node_key::key_path(&data_dir).display()
    );
    let entries: Vec<_> = fs::read_dir(&data_dir)
        .expect("list the data dir")
        .filter_map(|entry| entry.ok())
        .collect();
    assert!(
        entries.is_empty(),
        "a pure read must leave a never-started deployment directory exactly as it found it, \
         empty: {entries:?}"
    );
}

/// The identity an operator reads is never the node key itself.
///
/// The never-started listing answers with the deployment directory's own name,
/// even once a key has been generated for unrelated writes — that half is
/// unchanged, and it is the half that carries the point: the key is a scope
/// target, not a name for a person to read.
///
/// The degraded half is the owner ruling of 2026-08-25 (folded into
/// `vigil-settings-autority-direction.md`). A process that is NOT the run has
/// no store it can read and no route to the run that holds the answer, so it
/// reports NO identity and says why. It used to answer with the directory's
/// name here, and that derivation is exactly what the ruling retires: it is
/// not a vaguer answer than the truth, it is a different node's answer, with
/// nothing on the line to say so — an operator reading it against their broker
/// or against another node cannot tell.
#[test]
fn the_operator_facing_identity_is_never_the_node_key_on_either_surface() {
    let root = tempfile::tempdir().expect("enclosing temp dir");
    let data_dir = root.path().join("workshopnode");
    fs::create_dir_all(&data_dir).expect("create the data dir");
    let directory_name = data_dir
        .file_name()
        .expect("the data dir has a name")
        .to_string_lossy()
        .to_string();

    // Generate a key first, the way an earlier write on this deployment
    // would have — proving the surfaces below ignore it even when one
    // exists, not merely when one is absent.
    let key = node_key::scope_name(&data_dir);
    assert_ne!(
        key, directory_name,
        "the generated key must not coincide with the directory name in this fixture"
    );
    assert!(
        Uuid::parse_str(&key).is_ok(),
        "the node key is a generated v4 UUID: {key:?}"
    );

    let store_path = SettingsStore::store_path(&data_dir);
    let never_started_report =
        report_by_direct_read(&data_dir, &store_path).expect("read the never-started listing");
    let never_started_identity = never_started_report.identity.as_ref().unwrap_or_else(|| {
        panic!(
            "a never-started deployment's store is absent, not unreadable — this process can \
                 answer for what it WOULD run at, and that includes what it would call itself: \
                 {never_started_report:?}"
        )
    });
    assert_eq!(
        never_started_identity.value, directory_name,
        "the never-started listing must answer with the deployment directory's name, not the \
         node key: {never_started_report:?}"
    );
    assert!(
        Uuid::parse_str(&never_started_identity.value).is_err(),
        "the never-started identity value must not itself parse as the node key's UUID shape"
    );

    let degraded_report = report_degraded(&data_dir);
    assert!(
        degraded_report.identity.is_none(),
        "this process is not the run, so it has no identity to report: its store is unreadable \
         and there is no route to the process that knows. Answering with the directory's name — \
         which is what this surface used to do, and what {directory_name:?} would be here — \
         reports a DIFFERENT node with nothing on the line to say so. What the command-side \
         answer owes an operator INSTEAD is pinned end to end in \
         `crates/vigil-bin/tests/storeless_command_answers_without_guessing.rs`; what is held \
         here is only that nothing is invented. Got {degraded_report:?}"
    );
}
