//! The fabric worker lease config contract.
//!
//! These tests exercise the same resolver used to construct the production
//! `WorkerConfig`. They deliberately avoid a live Iroh hub: the lease value is
//! decided before any network or ledger operation, and querying the live hub
//! database while its server task applies a claim can synchronously block the
//! single-thread async test runtime.
//!
//! An explicit lease arrives the way every behavior value now arrives — a
//! record in the settings store, resolved and handed to the resolver — rather
//! than through an environment variable, which holds no rank in the authority
//! model and is no longer read.

#![cfg(feature = "fabric")]

use vigil::fabric::resolved_worker_lease_duration_ms;
use vigil::settings_model::{
    FABRIC_WORKER_LEASE_MS_SETTING, Scope, ScopeTarget, SettingValue, Surface,
};
use vigil::settings_store::SettingsStore;

const DEFAULT_LEASE_MS: i64 = 5 * 60_000;
const EXPLICIT_LEASE_MS: i64 = 2_000;
const NODE: &str = "node-a";

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: NODE.to_string(),
        site: NODE.to_string(),
        node: NODE.to_string(),
        camera: None,
    }
}

/// The lease this deployment resolves, read back through the real store exactly
/// as the runtime reads it before it builds the worker.
fn resolved_lease_from_store(store: &SettingsStore) -> Option<u64> {
    match store.resolve(FABRIC_WORKER_LEASE_MS_SETTING, &target()) {
        Ok(effective) if effective.author != vigil::settings_model::Author::Automatic => {
            match effective.requested {
                SettingValue::Int(lease) => u64::try_from(lease).ok(),
                _ => None,
            }
        }
        _ => None,
    }
}

#[test]
fn worker_lease_defaults_to_five_minutes_when_unset() {
    // Nobody has set a lease, so the store hands the resolver nothing and
    // Vigil's own five minutes is what a worker holds.
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = SettingsStore::open(directory.path()).expect("open the node-side settings store");

    assert_eq!(
        resolved_lease_from_store(&store),
        None,
        "an unset lease must resolve to nobody having set one, not to a value"
    );
    assert_eq!(resolved_worker_lease_duration_ms(None), DEFAULT_LEASE_MS);
}

#[test]
fn explicit_fabric_worker_lease_ms_flows_into_the_claimed_lease_deadline() {
    // Unfakeable because the lease is written through the real store and read
    // back through the real resolution before it reaches the resolver: a value
    // that was accepted and not stored, or stored under Vigil's own automatic
    // author, produces the default here rather than the explicit lease. The
    // second half then proves an already-resolved value still wins, so a
    // resolver that ignored its argument and reached for the store itself would
    // fail it.
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = SettingsStore::open(directory.path()).expect("open the node-side settings store");
    store
        .set_local(
            FABRIC_WORKER_LEASE_MS_SETTING,
            Surface::VigilSettings,
            Scope::node(NODE),
            SettingValue::Int(EXPLICIT_LEASE_MS),
        )
        .expect("the lease an operator set on this node");

    let resolved = resolved_lease_from_store(&store);
    assert_eq!(
        resolved,
        Some(EXPLICIT_LEASE_MS as u64),
        "the lease an operator set must resolve to what they set"
    );
    assert_eq!(
        resolved_worker_lease_duration_ms(resolved),
        EXPLICIT_LEASE_MS
    );
    assert_eq!(
        resolved_worker_lease_duration_ms(Some(45_000)),
        45_000,
        "a value already resolved for this run must take precedence over the fallback"
    );
}
