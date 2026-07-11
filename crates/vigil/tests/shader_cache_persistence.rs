//! The cold Vulkan/SPIR-V shader compile that the accelerated (wgpu) detector
//! pays on first boot is measured at ~2m36s on a real ADL-N GPU. Mesa can keep
//! that compiled shader cache on disk so a first boot warms every later boot —
//! but only if the cache lives on the add-on's PERSISTENT data volume (a
//! container-local default is wiped every restart, so the box re-pays the
//! compile forever). The detector runs in-process via wgpu, so the cache env
//! must be exported into vigil's OWN process before wgpu initialises.
//!
//! This pins the contract: a helper places the Mesa shader cache under the
//! persistent data root, creates it if absent, and exports MESA_SHADER_CACHE_DIR
//! to it so the in-process wgpu/Vulkan detector (and the doctor probe) reuse it.

#![cfg(feature = "detect-burn-wgpu")]

use std::path::PathBuf;

use vigil::detection_accel::configure_persistent_shader_cache;

const CACHE_ENV: &str = "MESA_SHADER_CACHE_DIR";

#[test]
fn persistent_shader_cache_lives_under_data_root_and_exports_env() {
    let data_root = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os(CACHE_ENV);

    let cache = configure_persistent_shader_cache(data_root.path())
        .expect("configuring the persistent shader cache under a writable data root must succeed");

    // (1) under the persistent data path — not a container-local default that
    // a restart wipes.
    assert!(
        cache.starts_with(data_root.path()),
        "the shader cache must live under the persistent data root {} so a first boot's compile survives restarts, got {}",
        data_root.path().display(),
        cache.display()
    );
    // (2) created if absent — the detector must not race an uncreated dir.
    assert!(
        cache.is_dir(),
        "the shader cache directory must be created if absent, but {} is not a directory",
        cache.display()
    );
    // (3) the env reaches this process (and thus the in-process wgpu detector).
    let exported = std::env::var_os(CACHE_ENV);

    // Restore the ambient environment before asserting so a failure cannot leak
    // MESA_SHADER_CACHE_DIR into another test in this process.
    unsafe {
        match previous {
            Some(value) => std::env::set_var(CACHE_ENV, value),
            None => std::env::remove_var(CACHE_ENV),
        }
    }

    let exported = exported
        .map(PathBuf::from)
        .expect("configuring the shader cache must export MESA_SHADER_CACHE_DIR so the in-process wgpu/Vulkan detector reuses the persistent cache");
    assert_eq!(
        exported, cache,
        "the exported MESA_SHADER_CACHE_DIR must point at the created persistent cache directory"
    );
}
