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

pub(crate) fn prepare_runtime_user(store_path: &Path) -> Result<(), String> {
    if std::env::var("VIGIL_DROP_PRIVILEGES").ok().as_deref() != Some("1") {
        return Ok(());
    }
    let uid = env_u32("VIGIL_RUN_UID", 1000)?;
    let gid = env_u32("VIGIL_RUN_GID", 1000)?;
    drop_privileges(store_path, uid, gid)
}

fn env_u32(name: &str, default: u32) -> Result<u32, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u32>()
            .map_err(|error| format!("{name} must be numeric: {error}")),
        Err(_) => Ok(default),
    }
}

#[cfg(unix)]
fn drop_privileges(store_path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let store_parent = store_path.parent().unwrap_or_else(|| Path::new("."));
    if !store_parent.as_os_str().is_empty() {
        fs::create_dir_all(store_parent).map_err(|error| {
            format!(
                "could not create store parent {} before privilege drop: {error}",
                store_parent.display()
            )
        })?;
    }
    chown_if_exists(store_parent, uid, gid)?;
    chown_if_exists(store_path, uid, gid)?;
    chown_if_exists(&store_path.with_extension("lock"), uid, gid)?;
    let supplemental = std::env::var("VIGIL_RUN_SUPPLEMENTAL_GIDS").ok();
    let plan = privilege_drop_plan(uid, gid, supplemental.as_deref())?;
    for step in plan {
        execute_privilege_step(step)?;
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
fn drop_privileges(_store_path: &Path, _uid: u32, _gid: u32) -> Result<(), String> {
    Ok(())
}
