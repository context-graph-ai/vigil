use std::fs;
use std::path::Path;

/// One step of the privilege-drop sequence, in execution order. Supplemental
/// groups are applied BEFORE setgid/setuid so a device-access group (e.g.
/// render/video) survives the drop and the final process can open mapped
/// hardware devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivilegeStep {
    SetSupplementalGroups(Vec<u32>),
    SetGid(u32),
    SetUid(u32),
}

/// Build the ordered privilege-drop plan from the run uid/gid plus the
/// `VIGIL_RUN_SUPPLEMENTAL_GIDS` value (comma-separated numeric gids).
/// An unparseable list fails loud; it is never silently ignored. Without
/// the variable the plan still clears supplemental groups so the dropped
/// process never inherits root's.
pub fn privilege_drop_plan(
    uid: u32,
    gid: u32,
    supplemental_gids: Option<&str>,
) -> Result<Vec<PrivilegeStep>, String> {
    let mut gids = Vec::new();
    if let Some(list) = supplemental_gids {
        for entry in list.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let parsed = entry.parse::<u32>().map_err(|error| {
                format!(
                    "VIGIL_RUN_SUPPLEMENTAL_GIDS entries must be numeric gids, \
                     got {entry:?}: {error}"
                )
            })?;
            if parsed == 0 {
                return Err(
                    "VIGIL_RUN_SUPPLEMENTAL_GIDS must not contain gid 0: keeping the root \
                     group across a privilege drop defeats the drop"
                        .to_string(),
                );
            }
            gids.push(parsed);
        }
    }
    Ok(vec![
        PrivilegeStep::SetSupplementalGroups(gids),
        PrivilegeStep::SetGid(gid),
        PrivilegeStep::SetUid(uid),
    ])
}

/// One step of becoming this deployment's runtime user, in execution order.
///
/// The DAEMON prepares the store it is about to own — it is the process that
/// brings a deployment into existence — and then drops. A live command drops
/// and nothing else: a `vigil why` that created a store directory, or moved
/// the ownership of a store a daemon is holding, would be a reader rewriting
/// the deployment it came to ask a question of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeUserStep {
    /// `create_dir_all` on the store's parent directory.
    CreateStoreParent(std::path::PathBuf),
    /// `chown` to the runtime uid/gid, if the path exists.
    ChownIfExists(std::path::PathBuf),
    /// One step of the identity drop itself.
    Drop(PrivilegeStep),
}

/// Everything the DAEMON does to become this deployment's runtime user, in
/// execution order: the store parent created, the parent, the store and the
/// companion lock contextdb names for it ([`contextdb_core::store_companion_path`])
/// chowned, then the drop plan [`privilege_drop_plan`] already builds.
pub fn runtime_user_plan_for_daemon(
    store_path: &Path,
    uid: u32,
    gid: u32,
    supplemental_gids: Option<&str>,
) -> Result<Vec<RuntimeUserStep>, String> {
    let store_parent = store_path.parent().unwrap_or_else(|| Path::new("."));
    let mut plan = Vec::new();
    if !store_parent.as_os_str().is_empty() {
        plan.push(RuntimeUserStep::CreateStoreParent(
            store_parent.to_path_buf(),
        ));
    }
    plan.push(RuntimeUserStep::ChownIfExists(store_parent.to_path_buf()));
    plan.push(RuntimeUserStep::ChownIfExists(store_path.to_path_buf()));
    // The store's companion lock is the one contextdb really creates, and
    // contextdb is the only place its name is derived: `store.contextgraph`
    // is guarded by `store.contextgraph.lock`, appended rather than swapped
    // for the extension. Deriving it here a second time named
    // `store.lock` — a path nothing creates — so the daemon chowned nothing
    // while it was still root and the runtime user could not open its own
    // store after the drop.
    plan.push(RuntimeUserStep::ChownIfExists(
        contextdb_core::store_companion_path(store_path),
    ));
    plan.extend(
        runtime_user_plan_for_live_command(uid, gid, supplemental_gids)?
            .into_iter()
            .filter(|step| matches!(step, RuntimeUserStep::Drop(_))),
    );
    Ok(plan)
}

/// Everything a LIVE COMMAND (`vigil why` / `vigil events` / `vigil stats`)
/// does to become this deployment's runtime user, in execution order.
/// Identity only, and it is handed no store pathname at all.
pub fn runtime_user_plan_for_live_command(
    uid: u32,
    gid: u32,
    supplemental_gids: Option<&str>,
) -> Result<Vec<RuntimeUserStep>, String> {
    Ok(privilege_drop_plan(uid, gid, supplemental_gids)?
        .into_iter()
        .map(RuntimeUserStep::Drop)
        .collect())
}

/// Become the deployment's runtime user for a live command: runs
/// [`runtime_user_plan_for_live_command`] and nothing else.
///
/// Called from live-command dispatch BEFORE the owner channel is opened
/// (`lib.rs`'s `print_control_or_direct`, ahead of `ask_runtime_owner`),
/// because the channel authorizes on the peer's operating-system user and
/// nothing else: a command that kept the exec's user is refused for a mismatch
/// the operator never chose and cannot see.
pub(crate) fn adopt_runtime_user_for_live_command() -> Result<(), String> {
    prepare_runtime_user(None)
}

/// Become this deployment's configured runtime user.
///
/// `store_path` is `Some` for the DAEMON, which prepares the store it is about
/// to own before it drops, and `None` for a live command, which brings no
/// deployment into existence and touches no path at all. Both sides read the
/// SAME `VIGIL_DROP_PRIVILEGES` / `VIGIL_RUN_UID` / `VIGIL_RUN_GID` /
/// `VIGIL_RUN_SUPPLEMENTAL_GIDS` surface here, and both execute a plan built
/// by the two plan functions above, so the identity they arrive at cannot
/// drift.
// VIGIL_DROP_PRIVILEGES is an enumerated, reviewed override
// (`environment_read_surface.baseline.txt`), not an ad-hoc read. The four
// reads stay in THIS function and in `drop_privileges` below, where the frozen
// inventory records them.
#[allow(clippy::disallowed_methods)]
pub(crate) fn prepare_runtime_user(store_path: Option<&Path>) -> Result<(), String> {
    if std::env::var("VIGIL_DROP_PRIVILEGES").ok().as_deref() != Some("1") {
        return Ok(());
    }
    let uid = env_u32("VIGIL_RUN_UID", 1000)?;
    let gid = env_u32("VIGIL_RUN_GID", 1000)?;
    drop_privileges(store_path, uid, gid)
}

// This wrapper's own callers (VIGIL_RUN_UID, VIGIL_RUN_GID) are enumerated,
// reviewed overrides (`environment_read_surface.baseline.txt`).
#[allow(clippy::disallowed_methods)]
fn env_u32(name: &str, default: u32) -> Result<u32, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u32>()
            .map_err(|error| format!("{name} must be numeric: {error}")),
        Err(_) => Ok(default),
    }
}

// VIGIL_RUN_SUPPLEMENTAL_GIDS is an enumerated, reviewed override
// (`environment_read_surface.baseline.txt`), not an ad-hoc read.
#[allow(clippy::disallowed_methods)]
#[cfg(unix)]
fn drop_privileges(store_path: Option<&Path>, uid: u32, gid: u32) -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let supplemental = std::env::var("VIGIL_RUN_SUPPLEMENTAL_GIDS").ok();
    // The plan IS the behavior: the daemon's carries the store preparation and
    // a live command's carries the identity drop alone, and nothing here can
    // touch a path the plan did not name.
    let plan = match store_path {
        Some(store_path) => {
            runtime_user_plan_for_daemon(store_path, uid, gid, supplemental.as_deref())?
        }
        None => runtime_user_plan_for_live_command(uid, gid, supplemental.as_deref())?,
    };
    for step in plan {
        execute_runtime_user_step(step, uid, gid)?;
    }
    let dumpable = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1) };
    if dumpable != 0 {
        return Err(format!(
            "dumpable process setup failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn execute_runtime_user_step(step: RuntimeUserStep, uid: u32, gid: u32) -> Result<(), String> {
    match step {
        RuntimeUserStep::CreateStoreParent(store_parent) => fs::create_dir_all(&store_parent)
            .map_err(|error| {
                format!(
                    "could not create store parent {} before privilege drop: {error}",
                    store_parent.display()
                )
            }),
        RuntimeUserStep::ChownIfExists(path) => chown_if_exists(&path, uid, gid),
        RuntimeUserStep::Drop(step) => execute_privilege_step(step),
    }
}

#[cfg(unix)]
fn execute_privilege_step(step: PrivilegeStep) -> Result<(), String> {
    match step {
        PrivilegeStep::SetSupplementalGroups(gids) => {
            let raw: Vec<libc::gid_t> = gids.iter().map(|gid| *gid as libc::gid_t).collect();
            let result = unsafe { libc::setgroups(raw.len(), raw.as_ptr()) };
            if result != 0 {
                return Err(format!(
                    "setgroups({gids:?}) failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }
        PrivilegeStep::SetGid(gid) => {
            let result = unsafe { libc::setgid(gid) };
            if result != 0 {
                return Err(format!(
                    "setgid({gid}) failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }
        PrivilegeStep::SetUid(uid) => {
            let result = unsafe { libc::setuid(uid) };
            if result != 0 {
                return Err(format!(
                    "setuid({uid}) failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }
    }
}

#[cfg(unix)]
fn chown_if_exists(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    // symlink_metadata + lchown: root must never follow a symlink a less
    // privileged writer planted at the store path.
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let c_path = std::ffi::CString::new(bytes)
        .map_err(|_| format!("path contains an interior NUL byte: {}", path.display()))?;
    let result = unsafe { libc::lchown(c_path.as_ptr(), uid, gid) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "chown {} to {uid}:{gid} failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(unix))]
fn drop_privileges(_store_path: Option<&Path>, _uid: u32, _gid: u32) -> Result<(), String> {
    Ok(())
}
