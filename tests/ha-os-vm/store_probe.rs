use std::env;
use std::fs;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use context_graph::{EmbedderConfig, Store, StoreConfig};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("open-existing") => {
            let path = next_path(&mut args, "store path")?;
            open_existing(&path)
        }
        Some("live") => {
            let path = next_path(&mut args, "store path")?;
            let host_pid = next_u32(&mut args, "host pid")?;
            let lock_pid = args.next().and_then(|value| value.parse::<u32>().ok());
            live_store(&path, host_pid, lock_pid)
        }
        Some("locked") => {
            let path = next_path(&mut args, "store path")?;
            let lock_pid = args.next().and_then(|value| value.parse::<u32>().ok());
            live_store_locked(&path, lock_pid)
        }
        _ => Err("usage: vigil-store-probe open-existing <store-path> | live <store-path> <host-pid> [lock-pid] | locked <store-path> [lock-pid]".to_string()),
    }
}

fn open_existing(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("store path does not exist: {}", path.display()));
    }
    let store = Store::open(store_config(path))
        .map_err(|error| format!("public Store::open failed for {}: {error}", path.display()))?;
    println!("opened {}", store.db_path().display());
    Ok(())
}

fn live_store(path: &Path, host_pid: u32, lock_pid: Option<u32>) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("store path does not exist: {}", path.display()));
    }
    if !process_has_file_open(path, host_pid)? {
        return Err(format!(
            "process {host_pid} does not have store db file open: {}",
            path.display()
        ));
    }
    live_store_locked(path, lock_pid)
}

fn live_store_locked(path: &Path, lock_pid: Option<u32>) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("store path does not exist: {}", path.display()));
    }
    if !lock_is_held(path)? {
        return Err(format!(
            "store lock was not held for live store: {}",
            lock_path(path).display()
        ));
    }
    match Store::open(store_config(path)) {
        Ok(store) => Err(format!(
            "public Store::open unexpectedly succeeded while live add-on should hold {}",
            store.db_path().display()
        )),
        Err(error) => {
            let detail = format!("{error:?}");
            if !detail.contains("database is locked") {
                return Err(format!(
                    "public Store::open did not report live lock for {}: {detail}",
                    path.display()
                ));
            }
            if let Some(pid) = lock_pid {
                let pid_text = format!("pid {pid}");
                if !detail.contains(&pid_text) {
                    return Err(format!(
                        "public Store::open lock did not name live process {pid}: {detail}"
                    ));
                }
            }
            println!("live store blocked public open: {detail}");
            Ok(())
        }
    }
}

fn store_config(path: &Path) -> StoreConfig {
    StoreConfig {
        db_path: path.to_path_buf(),
        default_text_embedder: Some(EmbedderConfig::disabled()),
        ..StoreConfig::default()
    }
}

fn process_has_file_open(path: &Path, pid: u32) -> Result<bool, String> {
    let expected = fs::metadata(path)
        .map_err(|error| format!("could not stat store path {}: {error}", path.display()))?;
    let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
    let entries = fs::read_dir(&fd_dir)
        .map_err(|error| format!("could not inspect {}: {error}", fd_dir.display()))?;
    for entry in entries.flatten() {
        if let Ok(metadata) = fs::metadata(entry.path())
            && metadata.dev() == expected.dev()
            && metadata.ino() == expected.ino()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn lock_is_held(path: &Path) -> Result<bool, String> {
    let lock_path = lock_path(path);
    if !lock_path.exists() {
        return Err(format!(
            "store lock file does not exist: {}",
            lock_path.display()
        ));
    }
    let file = File::open(&lock_path)
        .map_err(|error| format!("could not open lock {}: {error}", lock_path.display()))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        Ok(false)
    } else {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
        ) {
            Ok(true)
        } else {
            Err(format!(
                "could not check lock {}: {error}",
                lock_path.display()
            ))
        }
    }
}

fn lock_path(path: &Path) -> PathBuf {
    contextdb_core::store_companion_path(path)
}

fn next_path(args: &mut impl Iterator<Item = String>, label: &str) -> Result<PathBuf, String> {
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {label}"))
}

fn next_u32(args: &mut impl Iterator<Item = String>, label: &str) -> Result<u32, String> {
    args.next()
        .ok_or_else(|| format!("missing {label}"))?
        .parse::<u32>()
        .map_err(|error| format!("invalid {label}: {error}"))
}
