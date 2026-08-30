//! Asking a deployment that has never started brings none into existence —
//! for every command in the live-command family, not just for `settings`.
//!
//! `AGENTS.md` states this as a blanket invariant over `events`, `why`,
//! `stats`, `settings`, `enroll` and `forget`: "a read or an edit brings no
//! deployment into existence". Only the settings leg was ever witnessed
//! (`settings_live_operator_visibility::listing_an_unstarted_deployment_leaves_no_store_on_disk`),
//! and the two review commands quietly materialized a real store — a
//! ~282KB `store.contextgraph` and its lock file — in a directory the operator
//! had never run anything in. What that costs: a directory an operator pointed
//! a command at BY MISTAKE now looks like a deployment, `./vigil-data` becomes
//! a real store the moment somebody runs `vigil events` in the wrong shell, and
//! the file a later `vigil run` opens is one no runtime ever wrote.
//!
//! Each command runs in a directory of its OWN, so a footprint can only be
//! blamed on the command that left it, and the check is the directory's whole
//! contents afterwards rather than the store path alone — the lock file,
//! companions and anything else a store open leaves behind all count.
//!
//! What each command must ANSWER is pinned beside the footprint, because the
//! honest answer is the point: a deployment that has never started genuinely
//! holds no events, so `vigil events` answers with none of them rather than
//! refusing, and `vigil why` has no event to walk, so it refuses the way it
//! refuses any request it cannot serve. Neither answer needs a store to exist
//! to be true.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use vigil::settings_projection::{
    SETTING_LINE_PREFIX, UNAVAILABLE_LINE_PREFIX, UNMANAGED_LINE_PREFIX,
};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::vigil_binary_path;

/// A well-formed detection id, for the `why <id>` shape.
const SOME_ID: &str = "11111111-2222-3333-4444-555555555555";

/// The status a command exits with when it could not serve the request.
const REFUSED: i32 = 2;

/// One command as an operator meets it, and what it owes a deployment that has
/// never started.
struct Shape {
    label: &'static str,
    request: &'static [&'static str],
    /// The exit status this shape answers with here. `None` where only the
    /// footprint is pinned: what an enrollment or a forget says to a
    /// deployment that has never started is a refusal either way, and this
    /// test refuses to invent wording nobody has ruled.
    status: Option<i32>,
}

/// The whole family `AGENTS.md` makes the claim for.
const FAMILY: [Shape; 7] = [
    Shape {
        label: "vigil events",
        request: &["events"],
        // A deployment that has never started holds no events. That is a true
        // answer and a served one.
        status: Some(0),
    },
    Shape {
        label: "vigil why --latest",
        request: &["why", "--latest"],
        status: Some(REFUSED),
    },
    Shape {
        label: "vigil why <id>",
        request: &["why", SOME_ID],
        status: Some(REFUSED),
    },
    Shape {
        label: "vigil stats",
        request: &["stats"],
        status: Some(0),
    },
    Shape {
        label: "vigil settings",
        request: &["settings"],
        status: Some(0),
    },
    Shape {
        label: "vigil enroll",
        request: &["enroll", SOME_ID, "somebody"],
        status: None,
    },
    Shape {
        label: "vigil forget",
        request: &["forget", "somebody"],
        status: None,
    },
];

fn run(data_dir: &Path, request: &[&str]) -> Output {
    Command::new(vigil_binary_path())
        .args(request)
        .env("VIGIL_DATA_DIR", data_dir)
        .env_remove("VIGIL_STORE_PATH")
        .env_remove("VIGIL_CONTROL_SOCKET")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("spawn `vigil {}`: {error}", request.join(" ")))
}

/// Everything in the deployment directory, so a footprint of any shape is
/// caught rather than only the store file this test could think to name.
fn contents(data_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(data_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| {
            entry
                .expect("read the deployment directory")
                .file_name()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    names.sort();
    names
}

/// Unfakeable because the directory is inspected on the filesystem after a real
/// process has exited: a command that opened a store for itself left the file
/// behind, and no assertion here reads anything the command reported about
/// itself.
#[test]
fn no_live_command_brings_a_never_started_deployment_into_existence() {
    let tmp = tempfile::tempdir().expect("temporary root");
    let mut failures: Vec<String> = Vec::new();

    for shape in &FAMILY {
        // A directory of its own per command, so a footprint names the command
        // that left it.
        let data_dir: PathBuf = tmp.path().join(shape.label.replace(' ', "-"));
        fs::create_dir_all(&data_dir).expect("create the deployment directory");
        assert!(
            contents(&data_dir).is_empty(),
            "sanity: {} must start against an empty deployment directory",
            shape.label
        );

        let answer = run(&data_dir, shape.request);
        let stdout = String::from_utf8_lossy(&answer.stdout).to_string();
        let stderr = String::from_utf8_lossy(&answer.stderr).to_string();
        let left_behind = contents(&data_dir);

        if !left_behind.is_empty() {
            failures.push(format!(
                "{}: asking a deployment that has never started must leave its directory exactly \
                 as it was, and this left {left_behind:?} behind. A directory an operator pointed \
                 a command at by mistake is not a deployment, and the next `vigil run` must not \
                 open a store no runtime ever wrote.\nstandard output was:\n{stdout}\nstandard \
                 error was:\n{stderr}",
                shape.label
            ));
        }
        if let Some(expected) = shape.status
            && answer.status.code() != Some(expected)
        {
            failures.push(format!(
                "{}: exited {:?}, and this deployment's honest answer exits {expected}.\nstandard \
                 output was:\n{stdout}\nstandard error was:\n{stderr}",
                shape.label,
                answer.status.code()
            ));
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The answers themselves, pinned separately from the footprint so a fix that
/// stops creating a store cannot quietly turn a served answer into a refusal.
/// A deployment that has never started really does hold no events, and really
/// does have no event to walk — both are true without a store existing to say
/// so.
#[test]
fn the_review_commands_answer_a_never_started_deployment_honestly() {
    let tmp = tempfile::tempdir().expect("temporary root");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create the deployment directory");

    let events = run(&data_dir, &["events"]);
    let events_stdout = String::from_utf8_lossy(&events.stdout).to_string();
    let events_stderr = String::from_utf8_lossy(&events.stderr).to_string();
    assert_eq!(
        events.status.code(),
        Some(0),
        "`vigil events` on a deployment that has never started is a SERVED request — there are no \
         events, which is the answer.\nstandard output was:\n{events_stdout}\nstandard error \
         was:\n{events_stderr}"
    );
    assert!(
        events_stdout.trim().is_empty(),
        "and the answer is empty rather than a manufactured row: {events_stdout}"
    );

    let why = run(&data_dir, &["why", "--latest"]);
    let why_stdout = String::from_utf8_lossy(&why.stdout).to_string();
    let why_stderr = String::from_utf8_lossy(&why.stderr).to_string();
    assert_eq!(
        why.status.code(),
        Some(REFUSED),
        "`vigil why --latest` could not serve the request — there is no event to walk — so it \
         refuses.\nstandard output was:\n{why_stdout}\nstandard error was:\n{why_stderr}"
    );
    assert!(
        why_stdout.trim().is_empty(),
        "the refusal arrives on one stream, and standard output carried:\n{why_stdout}"
    );
    assert!(
        why_stderr.to_ascii_lowercase().contains("not found"),
        "and it says there is no event, rather than sending the operator to repair a store that \
         was never created:\n{why_stderr}"
    );
}

/// Every EDIT an operator can make against a stopped deployment. An edit brings
/// no deployment into existence either — `AGENTS.md` says so of a read "or an
/// edit", and a `settings set` that materializes a store has recorded a value
/// into a deployment nobody started, at a path nobody meant, which the next
/// `vigil run` will then open as though a runtime had written it.
const EDITS: [Shape; 5] = [
    Shape {
        label: "vigil settings set",
        request: &["settings", "set", "detector_sample_frames", "3"],
        status: None,
    },
    Shape {
        label: "vigil settings reset",
        request: &["settings", "reset", "detector_sample_frames"],
        status: None,
    },
    Shape {
        label: "vigil settings identity change",
        request: &["settings", "identity", "change", "new-name", "--confirm"],
        status: None,
    },
    Shape {
        label: "vigil enroll edit",
        request: &["enroll", SOME_ID, "somebody"],
        status: None,
    },
    Shape {
        label: "vigil forget edit",
        request: &["forget", "somebody"],
        status: None,
    },
];

/// Unfakeable for the same reason the read inventory is: the directory is read
/// off the filesystem after a real process has exited, and each edit runs in a
/// directory of its own so a footprint names the command that left it.
#[test]
fn no_edit_shape_brings_a_never_started_deployment_into_existence() {
    let tmp = tempfile::tempdir().expect("temporary root");
    let mut failures: Vec<String> = Vec::new();

    for shape in &EDITS {
        let data_dir: PathBuf = tmp.path().join(shape.label.replace(' ', "-"));
        fs::create_dir_all(&data_dir).expect("create the deployment directory");

        let answer = run(&data_dir, shape.request);
        let left_behind = contents(&data_dir);
        if !left_behind.is_empty() {
            failures.push(format!(
                "{}: an edit against a deployment that has never started must not bring one into \
                 existence, and this left {left_behind:?} behind. What stands afterwards is a \
                 store no runtime ever wrote, holding a value recorded at a path nobody \
                 meant.\nstandard output was:\n{}\nstandard error was:\n{}",
                shape.label,
                String::from_utf8_lossy(&answer.stdout),
                String::from_utf8_lossy(&answer.stderr)
            ));
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// A store path this process cannot resolve is NOT a deployment that has never
/// started, and must never be answered as one.
///
/// Both cases here look identical to a bare existence check — it answers false
/// for a dangling symlink and false for a path whose parent cannot be read —
/// and the two are different facts about different worlds. An operator whose
/// configured store points at a volume that failed to mount, or whose data
/// directory lost its permissions, is told their node is brand new and shown
/// the defaults this artifact would pick: a settings answer that is not their
/// deployment's, with nothing on the line to say so.
#[test]
fn a_store_path_that_cannot_be_resolved_is_never_answered_as_a_first_start() {
    let tmp = tempfile::tempdir().expect("temporary root");
    let mut failures: Vec<String> = Vec::new();

    // A configured store that points at nothing: the volume never mounted, or
    // the target was removed under it.
    let dangling_dir = tmp.path().join("dangling");
    fs::create_dir_all(&dangling_dir).expect("create the deployment directory");
    let dangling = dangling_dir.join("store.contextgraph");
    symlink(
        tmp.path().join("no-such-volume/store.contextgraph"),
        &dangling,
    )
    .expect("point the configured store at a target that is not there");

    // A store whose parent this process cannot look inside: the permissions
    // moved under a running deployment.
    let walled_parent = tmp.path().join("walled");
    fs::create_dir_all(&walled_parent).expect("create the walled directory");
    let walled = walled_parent.join("store.contextgraph");
    drop(fs::File::create(&walled).expect("a real file inside the walled directory"));
    fs::set_permissions(&walled_parent, fs::Permissions::from_mode(0o000))
        .expect("close the walled directory");

    for (label, store_path, data_dir) in [
        (
            "a configured store pointing at nothing",
            &dangling,
            &dangling_dir,
        ),
        (
            "a store whose parent cannot be read",
            &walled,
            &walled_parent,
        ),
    ] {
        let listing = Command::new(vigil_binary_path())
            .arg("settings")
            .env("VIGIL_DATA_DIR", data_dir)
            .env("VIGIL_STORE_PATH", store_path)
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .output()
            .expect("spawn `vigil settings`");
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&listing.stdout),
            String::from_utf8_lossy(&listing.stderr)
        );
        let says_it_cannot_read =
            rendered.contains(UNMANAGED_LINE_PREFIX) || rendered.contains(UNAVAILABLE_LINE_PREFIX);
        let rendered_a_floor = rendered
            .lines()
            .any(|line| line.starts_with(&format!("{SETTING_LINE_PREFIX} ")));
        if !says_it_cannot_read || rendered_a_floor {
            failures.push(format!(
                "{label}: this deployment's store could not be resolved, and the answer reads as \
                 a node that has never started — the artifact's own defaults standing where the \
                 operator's values belong, with nothing on the line to say so. The answer \
                 was:\n{rendered}"
            ));
        }
    }

    // Restore the permissions so the temporary directory can be cleaned up
    // whatever this test concluded.
    let _ = fs::set_permissions(&walled_parent, fs::Permissions::from_mode(0o755));
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
