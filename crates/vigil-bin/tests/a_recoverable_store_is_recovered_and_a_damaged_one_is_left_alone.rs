//! A store that only needs settling is settled and answered; a store that
//! genuinely cannot be read is answered as such, and nothing opens it for
//! writing.
//!
//! Vigil's stopped-deployment settings read goes through Context Graph's
//! read-only reader, and one condition that reader reports is not a fault at
//! all: a deployment whose last runtime did not shut down cleanly — killed,
//! power lost, container stopped hard — leaves a committed image the storage
//! engine will not hand out until ONE writable open has settled it. That is
//! `StoreNeedsWritableRecovery`, and it is the one refusal a question may
//! answer by opening the store for writing: nothing is damaged, nothing needs
//! diagnosing, the deployment is stopped so nothing is contending for it, and
//! the writable open is the only thing in existence that makes the store
//! readable again. An operator whose node crashed gets their settings.
//!
//! Every OTHER unreadable store is the opposite instruction. A permission, a
//! mount that is gone, a file that is not a store: reaching for a writable
//! handle on damage nobody has diagnosed is the worst available move, because a
//! mutating open is how a store somebody could still have recovered by hand
//! stops being recoverable at all. So the fallback fires on the recoverable
//! condition and on nothing else.
//!
//! What makes this pair worth pinning together is that both arms look identical
//! from outside: two stores that will not open read-only, two answers that have
//! to differ. Pinning only the recovery arm would be satisfied by a build that
//! reached for the writable door on everything.
//!
//! Nothing here sleeps, reads a clock, or asserts on elapsed time: the crashed
//! store is produced by genuinely killing a runtime and waiting on the store's
//! own ownership breadcrumb, and the damaged store is produced by writing bytes
//! that are not a store.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use vigil::settings_projection::{SETTING_LINE_PREFIX, UNAVAILABLE_LINE_PREFIX};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_for_store_owner,
    wait_for_store_owner_to_release,
};

/// Everything in the deployment directory, with each file's size and bytes, so
/// a writable open that touched a store it was told not to touch is caught
/// rather than only a store that changed shape.
fn fingerprint(root: &Path) -> Vec<(String, Vec<u8>)> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut snapshot: Vec<(String, Vec<u8>)> = entries
        .flatten()
        .filter(|entry| entry.path().is_file())
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().to_string(),
                fs::read(entry.path()).unwrap_or_default(),
            )
        })
        .collect();
    snapshot.sort();
    snapshot
}

fn ask_settings(data_dir: &Path) -> Output {
    Command::new(vigil_binary_path())
        .arg("settings")
        .env("VIGIL_DATA_DIR", data_dir)
        .env_remove("VIGIL_STORE_PATH")
        .env_remove("VIGIL_CONTROL_SOCKET")
        .stdin(Stdio::null())
        .output()
        .expect("spawn `vigil settings`")
}

fn text_of(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A deployment whose runtime was KILLED rather than asked to stop — the state
/// a crashed node, a power cut, or a hard container stop leaves behind.
fn a_deployment_whose_runtime_was_killed() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("temporary deployment directory");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create the deployment directory");
    let store_path = data_dir.join("store.contextgraph");
    let config_path = tmp.path().join("vigil.toml");
    fs::write(&config_path, "cameras = []\n").expect("write the configuration file");

    let health = TcpPortReservation::reserve_loopback().expect("reserve health port");
    let review = TcpPortReservation::reserve_loopback().expect("reserve review port");
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .arg("--config")
        .arg(&config_path)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--health-port")
        .arg(health.port().to_string())
        .arg("--review-port")
        .arg(review.port().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    health.release();
    review.release();
    let mut child = command.spawn().expect("spawn `vigil run`");
    let _stdout = capture_pipe(child.stdout.take());
    let _stderr = capture_pipe(child.stderr.take());
    wait_for_store_owner(&data_dir, &store_path)
        .expect("the runtime must own this deployment's store before it is killed");
    // Killed, never asked to stop: a clean shutdown settles the committed image
    // and there would be nothing here to recover.
    let _ = child.kill();
    let _ = child.wait();
    wait_for_store_owner_to_release(&data_dir, &store_path)
        .expect("the killed runtime must release the store");
    (tmp, data_dir)
}

/// Unfakeable because the runtime is a real process that really is killed, and
/// the answer is the shipped `vigil settings` output read off a second process:
/// a build that gave up on the recoverable condition answers `unavailable`
/// here, and one that never settled the image answers nothing at all.
#[test]
fn a_settings_read_settles_the_image_a_killed_runtime_left_and_answers() {
    let (_tmp, data_dir) = a_deployment_whose_runtime_was_killed();

    let answer = ask_settings(&data_dir);
    let rendered = text_of(&answer);
    assert_eq!(
        answer.status.code(),
        Some(0),
        "a node whose runtime was killed still has its settings, and the one writable open that \
         settles its committed image is a step this command may take: nothing is damaged and \
         nothing needs diagnosing. The answer was:\n{rendered}"
    );
    assert!(
        rendered
            .lines()
            .any(|line| line.starts_with(&format!("{SETTING_LINE_PREFIX} "))),
        "and it is this deployment's own settings that come back, not an explanation of why they \
         cannot:\n{rendered}"
    );
}

/// The other arm, and the reason the pair is one file: a store that cannot be
/// read must be answered as such WITHOUT any writable open, because a mutating
/// open on damage nobody has diagnosed can destroy what a person could still
/// have recovered by hand.
///
/// Unfakeable because the store's bytes are compared before and after: a build
/// that reached for the writable door here would have rewritten the file it was
/// asked about, and the comparison catches it whatever the answer said.
#[test]
fn a_store_that_is_not_a_store_is_answered_unreadable_and_never_opened_for_writing() {
    let tmp = tempfile::tempdir().expect("temporary deployment directory");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create the deployment directory");
    fs::write(tmp.path().join("vigil.toml"), "cameras = []\n").expect("write the configuration");
    // Bytes that are not a store, at the exact path this deployment's store
    // lives at. Not absent — absent is the never-started journey and its own
    // answer — and not recoverable either.
    fs::write(
        data_dir.join("store.contextgraph"),
        b"this is not a context-graph store, and settling it is not what it needs",
    )
    .expect("write a file that is not a store");

    let before = fingerprint(&data_dir);
    let answer = ask_settings(&data_dir);
    let after = fingerprint(&data_dir);
    let rendered = text_of(&answer);

    assert!(
        rendered.contains(UNAVAILABLE_LINE_PREFIX),
        "a store that cannot be read is said to be unreadable, and the operator is pointed at the \
         store rather than at a node that has never started. The answer was:\n{rendered}"
    );
    assert!(
        !rendered
            .lines()
            .any(|line| line.starts_with(&format!("{SETTING_LINE_PREFIX} "))),
        "and no setting line may stand where this deployment's values belong — every one of them \
         would be this artifact's own default with nothing on the line to say so:\n{rendered}"
    );
    assert_eq!(
        after, before,
        "and nothing opened this store for writing on the way to saying so. A mutating open on \
         damage nobody has diagnosed is how a store somebody could still have recovered by hand \
         stops being recoverable at all, and the writable door is reserved for the ONE condition \
         that asks for it. The answer was:\n{rendered}"
    );
}
