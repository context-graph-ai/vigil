//! The Vigil product surface has no broker transport. Home Assistant MQTT is
//! an operator integration and is not a ContextDB fabric transport.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(unix)]
fn tracked_path(root: &std::path::Path, raw: &[u8]) -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    root.join(std::path::Path::new(OsStr::from_bytes(raw)))
}

#[cfg(not(unix))]
fn tracked_path(root: &std::path::Path, raw: &[u8]) -> PathBuf {
    let relative = std::str::from_utf8(raw)
        .unwrap_or_else(|_| panic!("tracked path is not valid UTF-8: {raw:?}"));
    root.join(relative)
}

#[test]
fn vigil_has_no_broker_surface() {
    let root = workspace_root();
    let output = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "-z"])
        .output()
        .expect("list tracked files");
    assert!(output.status.success(), "git ls-files failed: {output:?}");

    let broker = [b"na".as_slice(), b"ts".as_slice()].concat();
    let client = [
        b"async".as_slice(),
        b"-".as_slice(),
        b"na".as_slice(),
        b"ts".as_slice(),
    ]
    .concat();
    let self_path: &[u8] = b"crates/vigil/tests/no_broker_surface.rs";
    let mut hits = Vec::new();

    for raw in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        if raw == self_path {
            continue;
        }
        let display = String::from_utf8_lossy(raw);
        if contains_ascii_case_insensitive(raw, &broker)
            || contains_ascii_case_insensitive(raw, &client)
        {
            hits.push(format!("path:{display}"));
            continue;
        }

        let path = tracked_path(&root, raw);
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path)
            .unwrap_or_else(|error| panic!("read tracked file {}: {error}", path.display()));
        if contains_ascii_case_insensitive(&bytes, &broker)
            || contains_ascii_case_insensitive(&bytes, &client)
        {
            hits.push(format!("content:{display}"));
        }
    }

    assert!(
        hits.is_empty(),
        "tracked Vigil manifests, lockfiles, source, tests, fixtures, workflows, benches, \
         examples, docs, flags, environment/config paths, and gate commands must contain no \
         broker transport residue: {hits:?}"
    );
}
