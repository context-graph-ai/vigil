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
/// An unparseable list fails loud; it is never silently ignored.
pub fn privilege_drop_plan(
    uid: u32,
    gid: u32,
    supplemental_gids: Option<&str>,
) -> Result<Vec<PrivilegeStep>, String> {
    let _ = (uid, gid, supplemental_gids);
    unimplemented!("scaffold: privilege drop plan is not implemented yet")
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
    let setgid = unsafe { libc::setgid(gid) };
    if setgid != 0 {
        return Err(format!(
            "setgid({gid}) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let setuid = unsafe { libc::setuid(uid) };
    if setuid != 0 {
        return Err(format!(
            "setuid({uid}) failed: {}",
            std::io::Error::last_os_error()
        ));
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
fn chown_if_exists(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let c_path = std::ffi::CString::new(bytes)
        .map_err(|_| format!("path contains an interior NUL byte: {}", path.display()))?;
    let result = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
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
