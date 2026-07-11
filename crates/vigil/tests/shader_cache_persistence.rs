//! The cold Vulkan/SPIR-V shader compile that the accelerated (wgpu) detector
//! pays on first boot is measured at ~2m36s on a real ADL-N GPU. Mesa can keep
//! that compiled shader cache on disk so a first boot warms every later boot —
//! but only if the cache lives on the add-on's PERSISTENT data volume (a
//! container-local default is wiped every restart, so the box re-pays the
//! compile forever). The detector runs in-process via wgpu, so the cache env
//! must be exported into vigil's OWN process before wgpu initialises.
//!
//! This pins the contract: a helper places the persistent GPU caches under the
//! data root, creates them if absent, and exports BOTH env levers into vigil's
//! OWN process before wgpu initialises — MESA_SHADER_CACHE_DIR (Mesa's shader
//! cache) and XDG_CACHE_HOME. XDG_CACHE_HOME is the load-bearing one: a
//! container has no durable HOME, so the cubecl AUTOTUNE cache (the real ~44s
//! cost, stored under $XDG_CACHE_HOME/cubecl) — plus the gstreamer registry and
//! the Mesa fallback path — is discarded every restart without it. Warm start
//! measured 21.0s vs 44.7s cold on the 680M once XDG_CACHE_HOME is set; Mesa's
//! cache alone gave zero speedup.

#![cfg(feature = "detect-burn-wgpu")]

use std::path::PathBuf;

use vigil::detection_accel::configure_persistent_shader_cache;

const MESA_ENV: &str = "MESA_SHADER_CACHE_DIR";
const XDG_ENV: &str = "XDG_CACHE_HOME";

#[test]
fn persistent_gpu_caches_live_under_data_root_and_export_their_env() {
    let data_root = tempfile::tempdir().expect("tempdir");
    let previous_mesa = std::env::var_os(MESA_ENV);
    let previous_xdg = std::env::var_os(XDG_ENV);

    let cache = configure_persistent_shader_cache(data_root.path())
        .expect("configuring the persistent GPU caches under a writable data root must succeed");

    // Capture what the call exported before restoring the ambient environment,
    // so a failed assertion cannot leak these vars into another test in this
    // process.
    let exported_mesa = std::env::var_os(MESA_ENV).map(PathBuf::from);
    let exported_xdg = std::env::var_os(XDG_ENV).map(PathBuf::from);
    unsafe {
        match previous_mesa {
            Some(value) => std::env::set_var(MESA_ENV, value),
            None => std::env::remove_var(MESA_ENV),
        }
        match previous_xdg {
            Some(value) => std::env::set_var(XDG_ENV, value),
            None => std::env::remove_var(XDG_ENV),
        }
    }

    // Mesa shader cache: under the persistent data root, created if absent, and
    // exported so the in-process wgpu/Vulkan detector reuses it.
    assert!(
        cache.starts_with(data_root.path()),
        "the shader cache must live under the persistent data root {} so a first boot's compile survives restarts, got {}",
        data_root.path().display(),
        cache.display()
    );
    assert!(
        cache.is_dir(),
        "the shader cache directory must be created if absent, but {} is not a directory",
        cache.display()
    );
    let exported_mesa = exported_mesa
        .expect("configuring the caches must export MESA_SHADER_CACHE_DIR so the in-process wgpu/Vulkan detector reuses the persistent Mesa cache");
    assert_eq!(
        exported_mesa, cache,
        "the exported MESA_SHADER_CACHE_DIR must point at the created persistent cache directory"
    );

    // XDG_CACHE_HOME: the single lever that persists the cubecl AUTOTUNE cache
    // (the real ~44s cost — a container has no durable HOME, so cubecl's default
    // $HOME/.cache/cubecl is discarded every restart), plus the gstreamer
    // registry and the Mesa fallback path. Without it the box re-pays autotune
    // on every start even with the Mesa cache present.
    let exported_xdg = exported_xdg
        .expect("configuring the caches must export XDG_CACHE_HOME so the cubecl autotune cache persists across restarts (a container has no durable HOME)");
    assert!(
        exported_xdg.starts_with(data_root.path()),
        "XDG_CACHE_HOME must live under the persistent data root {} so the cubecl autotune cache survives restarts, got {}",
        data_root.path().display(),
        exported_xdg.display()
    );
    assert!(
        exported_xdg.is_dir(),
        "the XDG_CACHE_HOME directory must be created if absent, but {} is not a directory",
        exported_xdg.display()
    );
}
