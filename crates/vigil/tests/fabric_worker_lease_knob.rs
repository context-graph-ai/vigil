//! The fabric worker lease config contract.
//!
//! These tests exercise the same resolver used to construct the production
//! `WorkerConfig`. They deliberately avoid a live Iroh hub: the lease value is
//! decided before any network or ledger operation, and querying the live hub
//! database while its server task applies a claim can synchronously block the
//! single-thread async test runtime.

#![cfg(feature = "fabric")]

use std::sync::Mutex;

use vigil::fabric::resolved_worker_lease_duration_ms;

const DEFAULT_LEASE_MS: i64 = 5 * 60_000;
const LEASE_ENV_VAR: &str = "VIGIL_FABRIC_WORKER_LEASE_MS";
const EXPLICIT_LEASE_MS: i64 = 2_000;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvVarGuard {
    name: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(name: &'static str, value: &str) -> Self {
        let original = std::env::var_os(name);
        // SAFETY: serialized by ENV_LOCK, held for the whole test.
        unsafe {
            std::env::set_var(name, value);
        }
        Self { name, original }
    }

    fn unset(name: &'static str) -> Self {
        let original = std::env::var_os(name);
        // SAFETY: serialized by ENV_LOCK, held for the whole test.
        unsafe {
            std::env::remove_var(name);
        }
        Self { name, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: serialized by ENV_LOCK, held for the whole test.
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }
}

#[test]
fn worker_lease_defaults_to_five_minutes_when_unset() {
    let _env_lock = ENV_LOCK.lock().expect("lease environment lock");
    let _lease_env = EnvVarGuard::unset(LEASE_ENV_VAR);

    assert_eq!(resolved_worker_lease_duration_ms(None), DEFAULT_LEASE_MS);
}

#[test]
fn explicit_fabric_worker_lease_ms_flows_into_the_claimed_lease_deadline() {
    let _env_lock = ENV_LOCK.lock().expect("lease environment lock");
    let _lease_env = EnvVarGuard::set(LEASE_ENV_VAR, &EXPLICIT_LEASE_MS.to_string());

    assert_eq!(resolved_worker_lease_duration_ms(None), EXPLICIT_LEASE_MS);
    assert_eq!(
        resolved_worker_lease_duration_ms(Some(45_000)),
        45_000,
        "an already-resolved config value must take precedence over the direct environment fallback"
    );
}
