//! Which detector a node's distributed work runs on.
//!
//! A node watching cameras already loads a detector per camera, and a backend
//! move reaches every one of them. Distributed work on such a node runs on
//! those same detectors — a second pair loaded beside them would double the
//! model in memory on the machine least able to afford it.
//!
//! A node with no camera has none of that. Its distributed work is the only
//! thing it detects with, so it owns a detector of its own — and that detector
//! has to be registered with the coordinator like any other, or a backend move
//! on that node reaches nothing at all and the operator's command silently
//! governs an empty set.
//!
//! Both handles are crate-private, so what this reads is the shipped source's
//! own registrations: which kind each caller registers, and that the camera
//! path registers exactly one kind.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn source(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The body of one function in a source file, from its signature to the line
/// that closes it at the same indentation.
fn function_body(text: &str, signature: &str) -> Option<String> {
    let start = text.find(signature)?;
    let rest = &text[start..];
    let indent: String = rest
        .lines()
        .next()?
        .chars()
        .take_while(|character| character.is_whitespace())
        .collect();
    let closing = format!("\n{indent}}}");
    let end = rest.find(&closing)? + closing.len();
    Some(rest[..end].to_string())
}

#[test]
fn a_node_with_no_camera_registers_the_detector_its_distributed_work_runs_on() {
    // The worker detector a camera-less node loads is registered as the
    // distributed-work handle it is. Without that registration the coordinator
    // has no handle to prepare for, so the one detector that node runs never
    // moves and every backend command on it reports success over an empty set.
    let runtime = source("crates/vigil/src/runtime.rs");
    let bootstrap = function_body(&runtime, "fn bootstrap_worker_detector(")
        .expect("the worker detector bootstrap must still be here");
    assert!(
        bootstrap.contains("DistributedWork"),
        "the detector a camera-less node loads for distributed work must be registered as a \
         distributed-work handle, or a backend move on that node reaches nothing"
    );
}

#[test]
fn the_camera_path_registers_camera_handles_and_nothing_else() {
    // The complement, and the half that keeps memory honest: registering a
    // camera registers exactly one handle, of one kind. A camera path that
    // also registered a distributed-work handle would prepare and retain a
    // second detector per node for work the camera detectors already do.
    let live_backends = source("crates/vigil/src/live_backends.rs");
    let register = function_body(&live_backends, "pub(crate) fn register_live_detector(")
        .expect("the camera registration must still be here");
    assert!(
        register.contains("HandleKind::Camera"),
        "a camera registers a camera handle"
    );
    assert!(
        !register.contains("DistributedWork"),
        "registering a camera must not also register a handle for distributed work; that work \
         runs on the camera's own detector"
    );
}
