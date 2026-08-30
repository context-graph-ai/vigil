//! A node never authors a record at the pushed rank — and that is an
//! authorization boundary, not a transport direction.
//!
//! A down-only table stops a node's row from travelling; it does not stop the
//! row from existing locally and being honored locally. Without a check where
//! the record is applied, a node could manufacture a fleet instruction for
//! itself and honor it, and every promise about who outranks whom becomes a
//! convention. The store beneath Vigil carries scope-constrained write handles,
//! so the constraint is declared on the pushed table and enforced by the
//! engine: the ordinary node-side handle is REFUSED, and the hub-role handle —
//! which exists in the harness and a future hub binary, never behind a
//! command-line flag — succeeds.
//!
//! RED: `vigil::settings_store` is skeleton-only (`todo!()` bodies) pending the
//! settings-store implementation.

use vigil::settings_model::{Author, Scope, SettingRecord, SettingValue, SettingsError, Surface};
use vigil::settings_store::{HandleRole, SettingsStore};

const SETTING: &str = "detector_stationary_interval_secs";

fn pushed_record() -> SettingRecord {
    SettingRecord {
        setting: SETTING.to_string(),
        author: Author::Pushed,
        surface: Surface::ManagementServer,
        scope: Scope::node("node-a"),
        value: SettingValue::Int(5),
        reason: "the hub set this for the fleet".to_string(),
        written_at_ms: 1_700_000_000_000,
        domain_generation: 0,
        reset: false,
    }
}

/// Unfakeable: the refusal is matched as the TYPED
/// `SettingsError::ScopeLabelViolation`, which only the engine's scope-label
/// constraint produces — a Vigil-side "is this handle allowed?" check invented
/// downstream would have to fabricate that variant, and the follow-up read
/// proves nothing was written even so. Matching the type rather than a message
/// string also means the boundary cannot be satisfied by wording.
#[test]
fn the_ordinary_node_side_handle_is_refused_a_write_to_the_pushed_table() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let node = SettingsStore::open(directory.path()).expect("open the node-side settings store");
    assert_eq!(
        node.role(),
        HandleRole::Node,
        "the ordinary handle a running Vigil opens is the node-side one"
    );

    let refusal = node
        .write_pushed_record(pushed_record())
        .expect_err("the node-side handle must not be able to author at the pushed rank");

    match refusal {
        SettingsError::ScopeLabelViolation { requested, allowed } => {
            assert!(
                !requested.is_empty() && !allowed.is_empty(),
                "the refusal names the scope label the write needed and the one this handle \
                 carries: requested={requested:?} allowed={allowed:?}"
            );
            assert_ne!(
                requested, allowed,
                "the refusal exists because the two differ: requested={requested:?} \
                 allowed={allowed:?}"
            );
        }
        other => panic!(
            "the refusal must be the engine's typed scope-label violation, not a Vigil-side check \
             or a generic store error: {other:?}"
        ),
    }

    let stored = node.records(SETTING).expect("read the stored records");
    assert!(
        stored.iter().all(|record| record.author != Author::Pushed),
        "a refused write stores nothing: no pushed record exists at this node: {stored:?}"
    );
}

/// Unfakeable: the same record, through the same method, on the same shape of
/// store — only the handle's role differs — so a test that passed by refusing
/// every pushed write, or by never enforcing anything, fails one of this pair.
/// The record is read back with its pushed author to prove the write landed
/// rather than being silently swallowed.
#[test]
fn the_hub_role_handle_writes_the_pushed_table_successfully() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let hub =
        SettingsStore::open_hub_role(directory.path()).expect("open the hub-role settings store");
    assert_eq!(
        hub.role(),
        HandleRole::Hub,
        "the harness-side handle opens in the hub role"
    );

    hub.write_pushed_record(pushed_record())
        .expect("the hub-role handle writes the down-only pushed table");

    let stored = hub.records(SETTING).expect("read the stored records");
    let pushed: Vec<&SettingRecord> = stored
        .iter()
        .filter(|record| record.author == Author::Pushed)
        .collect();
    assert_eq!(
        pushed.len(),
        1,
        "exactly the one pushed record the hub wrote is stored: {stored:?}"
    );
    assert_eq!(pushed[0].value, SettingValue::Int(5));
    assert_eq!(pushed[0].surface, Surface::ManagementServer);
}

/// The label a node writes under is not the same question as what it may READ,
/// and a stopped node must still see what its hub pushed.
///
/// The pushed table declares the plain `SCOPE_LABEL ('server')` form, which
/// constrains WRITES — the refusal the two tests above hold. It says nothing
/// about visibility, and the engine narrows reads only from a
/// `SCOPE_LABEL_READ` set, which this table does not carry. So the stopped-store
/// settings reader declares NO scope label and is served the whole of Vigil's
/// own tables. A reader that carried the node's `edge` label instead would be
/// asking to be narrowed over a column that narrows nothing, which
/// context-graph refuses at open — and a reader that was somehow served under
/// that label would show an operator a node with no fleet instruction on it at
/// all, on a node the hub had just configured.
///
/// Unfakeable because it drives the SHIPPED stopped-store projection — the same
/// call `vigil settings` makes when no runtime holds the store — against a
/// store a real writer wrote and then closed, and reads the pushed record back
/// out of the answer an operator would see.
#[test]
fn a_stopped_node_reads_the_record_its_hub_pushed_and_says_what_holds_it() {
    use vigil::settings_model::{ControlState, HeldReason, ScopeTarget};
    use vigil::settings_projection::report_by_direct_read_at;

    const NODE: &str = "node-a";
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store_path = SettingsStore::store_path(directory.path());
    let target = ScopeTarget {
        tenant: NODE.to_string(),
        site: NODE.to_string(),
        node: NODE.to_string(),
        camera: None,
    };

    // The one legitimate road a pushed record travels: written at the pushed
    // rank by a hub-role handle, then that writer LET GO — which is the state
    // an operator's node is in between runs.
    let hub =
        SettingsStore::open_hub_role(directory.path()).expect("open the hub-role settings store");
    hub.write_pushed_record(pushed_record())
        .expect("the hub-role handle writes the down-only pushed table");
    drop(hub);

    let report = report_by_direct_read_at(directory.path(), &store_path, &target)
        .expect("a stopped deployment answers its own settings");
    let pushed = report
        .settings
        .iter()
        .find(|setting| setting.setting == SETTING)
        .unwrap_or_else(|| panic!("the listing carries {SETTING}: {:?}", report.settings));
    assert_eq!(
        (
            pushed.requested.clone(),
            pushed.author,
            pushed.control_state.clone()
        ),
        (
            SettingValue::Int(5),
            Author::Pushed,
            ControlState::SetByManagementServer
        ),
        "a stopped node reports the value its hub pushed, attributed to the hub, and says on the \
         line that the management server is what set it — the node's own write label narrows what \
         it may author, never what it may see: {pushed:?}"
    );

    // And the relationship, not just the row: a value this deployment's own
    // operator sets outranks the hub's, and the hub's record stays visible
    // underneath it saying what is holding it down.
    let node = SettingsStore::open(directory.path()).expect("open the node-side settings store");
    node.write_record(SettingRecord {
        setting: SETTING.to_string(),
        author: Author::LocalExplicit,
        surface: Surface::VigilSettings,
        scope: Scope::node(NODE),
        value: SettingValue::Int(9),
        reason: "the operator set this at the node".to_string(),
        written_at_ms: 1_700_000_001_000,
        domain_generation: 0,
        reset: false,
    })
    .expect("a node authors its own local record");
    drop(node);

    let report = report_by_direct_read_at(directory.path(), &store_path, &target)
        .expect("a stopped deployment answers its own settings");
    let effective = report
        .settings
        .iter()
        .find(|setting| setting.setting == SETTING)
        .unwrap_or_else(|| panic!("the listing carries {SETTING}: {:?}", report.settings));
    assert_eq!(
        (effective.requested.clone(), effective.author),
        (SettingValue::Int(9), Author::LocalExplicit),
        "the operator's own value is what this node runs: {effective:?}"
    );
    let held = effective
        .held
        .iter()
        .find(|held| held.record.author == Author::Pushed)
        .unwrap_or_else(|| {
            panic!(
                "the hub's record is still readable underneath the operator's, on the face of the \
                 answer rather than in history: {effective:?}"
            )
        });
    assert_eq!(
        (held.record.value.clone(), held.reason.clone()),
        (
            SettingValue::Int(5),
            HeldReason::Shadowed {
                by_author: Author::LocalExplicit,
                by_surface: Surface::VigilSettings,
            }
        ),
        "and the answer says exactly what is holding the hub's value down: {held:?}"
    );
}
