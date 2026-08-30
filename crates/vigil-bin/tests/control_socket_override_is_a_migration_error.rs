//! `VIGIL_CONTROL_SOCKET` is gone, and an operator who still sets it is told
//! so.
//!
//! The variable used to move the runtime's control socket. There is no control
//! socket now — the process that owns the store answers through the store
//! itself — so the variable can no longer move anything. A value that quietly
//! does nothing is the worst of the three possible behaviors: the operator who
//! set it believes their deployment is wired the way they wrote it, and every
//! later diagnosis starts from a false premise. Nor may the value be
//! reinterpreted as some new address, which would silently give a retired knob
//! authority over the new road. So Vigil refuses: it names the variable, says
//! it has no effect, tells the operator to unset it, and exits non-zero
//! without serving the request.
//!
//! The refusal is asserted through the process environment of a spawned
//! command, never by mutating this test binary's own environment — the estate
//! drives a CLI's environment with `Command::env`, and the retired variable is
//! read at CLI startup, before anything a library call could reach.
//!
//! The second half is the absence pin, in the same form context-graph used
//! when it retired its own socket
//! (`reading_pass_through_replaces_the_control_socket.rs`): a scan of the
//! shipped source, not a `trybuild` case. A compile-fail case would prove one
//! name stopped resolving; the contract here is wider — nothing in vigil binds,
//! dials, computes or re-exports a control socket path anywhere — and only
//! reading the source proves that. It also cannot rot against compiler-message
//! wording.
//!
//! One constraint this pin puts on the fix, worth stating because it is easy
//! to trip: the refusal has to READ the variable, and every named environment
//! read in vigil src is enumerated in `environment_read_surface.baseline.txt`,
//! whose membership may only shrink within the frozen authorized set. The
//! retired `control_socket.rs|control_socket_path` record is deleted with this
//! commit and there is no authorized record to replace it with, so the read
//! belongs on the already-baselined roster site
//! (`settings_environment.rs::ignored_behavior_variables`, recorded
//! `<dynamic>`) rather than at a new named site of its own.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{vigil_binary_path, wait_until, workspace_root};

/// The retired variable, spelled once.
const RETIRED_VARIABLE: &str = "VIGIL_CONTROL_SOCKET";

/// The three things the refusal has to carry: the variable's own name, that it
/// changes nothing, and what to do about it.
const REFUSAL_MARKERS: [&str; 3] = [RETIRED_VARIABLE, "no effect", "unset"];

/// The production crates whose shipped source the absence pin reads.
const PRODUCTION_CRATES: [&str; 3] = ["vigil", "vigil-ha", "vigil-bin"];

fn crate_src(crate_name: &str) -> PathBuf {
    workspace_root().join("crates").join(crate_name).join("src")
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }
    found
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn with_retired_variable(args: &[&str], data_dir: &Path) -> Command {
    let mut command = Command::new(vigil_binary_path());
    command
        .args(args)
        .env("VIGIL_DATA_DIR", data_dir)
        .env(RETIRED_VARIABLE, data_dir.join("operator-chosen.sock"))
        .env_remove("VIGIL_STORE_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn assert_refused(label: &str, output: &Output) {
    let rendered = combined(output);
    assert!(
        !output.status.success(),
        "`vigil {label}` served the request with {RETIRED_VARIABLE} set — a retired variable that \
         silently changes nothing leaves the operator diagnosing from a false premise; \
         got:\n{rendered}"
    );
    for marker in REFUSAL_MARKERS {
        assert!(
            rendered.contains(marker),
            "the migration refusal for `vigil {label}` never said `{marker}` — it has to name the \
             variable, say it has no effect, and tell the operator to unset it; got:\n{rendered}"
        );
    }
}

/// Every read command an operator might have pointed at a hand-placed socket.
#[test]
fn a_read_command_refuses_to_run_while_the_retired_variable_is_set() {
    let tmp = tempfile::tempdir().expect("temporary deployment directory");
    for (label, args) in [
        ("events", vec!["events"]),
        ("why", vec!["why", "--latest"]),
        ("stats", vec!["stats"]),
        ("settings", vec!["settings"]),
    ] {
        let output = with_retired_variable(&args, tmp.path())
            .output()
            .unwrap_or_else(|error| panic!("spawn `vigil {label}`: {error}"));
        assert_refused(label, &output);
    }
}

/// The runtime itself — the process that used to publish the socket the
/// variable moved. Bounded and reaped either way, so a build that still
/// ignores the variable fails here instead of running forever.
#[test]
fn the_runtime_refuses_to_start_while_the_retired_variable_is_set() {
    let tmp = tempfile::tempdir().expect("temporary deployment directory");
    let config_path = tmp.path().join("vigil.toml");
    std::fs::write(&config_path, "cameras = []\n").expect("write the configuration file");
    let mut child = with_retired_variable(
        &["run", "--config", config_path.to_str().expect("utf-8 path")],
        tmp.path(),
    )
    .spawn()
    .expect("spawn `vigil run`");

    let exited = wait_until(
        "`vigil run` to refuse and exit while the retired variable is set",
        Duration::from_secs(30),
        || Ok(child.try_wait().ok().flatten()),
    );
    if let Err(error) = exited {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "{error}. A runtime started with {RETIRED_VARIABLE} set must refuse instead of \
             coming up as if the variable had placed its socket"
        );
    }
    let output = child
        .wait_with_output()
        .expect("collect the refused runtime's output");
    assert_refused("run", &output);
}

/// Vigil carries no control-socket path, no socket transport call, and no
/// re-export of either.
#[test]
fn vigil_carries_no_control_socket_path_or_socket_transport() {
    let vigil_src = crate_src("vigil");
    assert!(
        !vigil_src.join("control_socket.rs").exists(),
        "the control-socket path module must be gone from {}",
        vigil_src.display()
    );

    let lib = std::fs::read_to_string(vigil_src.join("lib.rs")).expect("read vigil's root module");
    for gone in [
        "control_socket",
        "control_socket_path",
        "request_control",
        "start_control_listener",
    ] {
        assert!(
            !lib.contains(gone),
            "`{gone}` must not appear in vigil's public surface (crates/vigil/src/lib.rs)"
        );
    }
    assert!(
        lib.contains("owner_control"),
        "the store-addressed owner channel replaces the socket and is what the CLI must reach for"
    );

    let mut offenders: Vec<String> = Vec::new();
    for crate_name in PRODUCTION_CRATES {
        for file in rust_sources(&crate_src(crate_name)) {
            let text = std::fs::read_to_string(&file).expect("read a shipped source file");
            for gone in [
                "control.sock",
                "control_socket_path",
                "start_control_listener",
                "request_control",
                "UnixListener",
                "UnixStream",
            ] {
                if text.contains(gone) {
                    offenders.push(format!("{}: {gone}", file.display()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the store path is the whole address now — no shipped source may name or dial a control \
         socket: {offenders:?}"
    );
}
