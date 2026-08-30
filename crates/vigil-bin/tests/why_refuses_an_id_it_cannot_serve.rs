//! `vigil why` given an id it cannot serve REFUSES — and a malformed id is not
//! a missing event.
//!
//! Two operator-facing statements are pinned here, and both are the family
//! contract this command already carries when it cannot open the store: an
//! unserved request leaves on standard error with exit status `2`, so a script
//! that reads the status can tell a walked detection from one that was never
//! there. An answer on standard output with status `0` tells that script the
//! walk SUCCEEDED and this deployment has no such event, which is a different
//! statement and a false one.
//!
//! The second statement is about the id itself. `not-a-uuid` is not an event
//! this deployment is missing — it is not a detection id at all, and an
//! operator who mistyped one is owed the difference. The answer to a malformed
//! id therefore names what a detection id looks like and never reports the
//! event as not found.
//!
//! Both ROUTES are exercised, because both are how an operator meets this: the
//! running owner answering over its store's own owner channel, and the direct
//! read of an idle store. The delivery is the same either way — the road an
//! answer travelled is not a reason for a script to read it differently.
//!
//! Nothing here waits on a clock to decide whether something happened:
//! readiness is the owner route answering, and the store being free is the
//! owner route no longer answering.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use vigil::settings_store::SettingsStore;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_for_store_owner,
    wait_for_store_owner_to_release,
};

/// A well-formed detection id no event in this deployment carries.
const UNKNOWN_ID: &str = "11111111-2222-3333-4444-555555555555";

/// What an operator types when they paste the wrong thing. It is not a
/// detection id, and it is not a detection id that is missing.
const MALFORMED_ID: &str = "not-a-uuid";

/// The status an unserved request exits with, for this whole command family.
const REFUSED: i32 = 2;

/// The note vigil staples onto a failure it did not recognize as a complete
/// answer. It reports on the OWNER route — a road the operator never asked
/// about — and on an answer that already names its own cause it is noise.
const OWNER_ROUTE_TRAILER: &str = "runtime owner unavailable";

struct LiveVigil {
    child: Child,
}

impl LiveVigil {
    fn spawn(config_path: &Path, data_dir: &Path) -> Self {
        let health = TcpPortReservation::reserve_loopback().expect("reserve health port");
        let review = TcpPortReservation::reserve_loopback().expect("reserve review port");
        let mut command = Command::new(vigil_binary_path());
        command
            .arg("run")
            .arg("--config")
            .arg(config_path)
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--health-port")
            .arg(health.port().to_string())
            .arg("--review-port")
            .arg(review.port().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn vigil run");
        // Drain both pipes so a full one can never stall the child.
        let _stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());
        Self { child }
    }

    /// The validated stop: `Child::kill` on this test's own handle, so nothing
    /// here formats a process-signal target of its own.
    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LiveVigil {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A deployment with a configuration file and a data directory, and no camera:
/// what an id is answered with does not depend on there being a camera, and a
/// cameraless node is a node an operator really runs.
struct Deployment {
    _tmp: tempfile::TempDir,
    data_dir: std::path::PathBuf,
    config_path: std::path::PathBuf,
}

impl Deployment {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment directory");
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&data_dir).expect("create the data directory");
        let config_path = tmp.path().join("vigil.toml");
        fs::write(&config_path, "cameras = []\n").expect("write the configuration file");
        Self {
            _tmp: tmp,
            data_dir,
            config_path,
        }
    }

    fn start(&self) -> LiveVigil {
        let run = LiveVigil::spawn(&self.config_path, &self.data_dir);
        // Readiness is the owner route answering: this runtime holds the
        // store, so the commands below reach it rather than opening the store
        // themselves.
        wait_for_store_owner(&self.data_dir, &SettingsStore::store_path(&self.data_dir))
            .expect("the runtime must own this deployment's store and answer through it");
        run
    }

    /// The store is free again, so the same questions take the direct read.
    fn wait_until_free(&self) {
        wait_for_store_owner_to_release(&self.data_dir, &SettingsStore::store_path(&self.data_dir))
            .expect("the runtime must release the store before the direct reads below");
    }

    fn why(&self, id: &str) -> Output {
        Command::new(vigil_binary_path())
            .arg("why")
            .arg(id)
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .output()
            .expect("spawn `vigil why`")
    }
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Every pin an unserved `why` carries, whichever route answered it: the
/// status a caller reads, the stream the answer arrives on, and the other
/// stream staying quiet.
fn assert_refused(route: &str, id: &str, answer: &Output) {
    let stdout = stdout_of(answer);
    let stderr = stderr_of(answer);
    assert_eq!(
        answer.status.code(),
        Some(REFUSED),
        "{route}: `vigil why {id}` did not serve the request, so the status has to say so — a \
         script that checks it cannot otherwise tell a walked detection from one that is not \
         there. standard output was:\n{stdout}\nstandard error was:\n{stderr}"
    );
    assert!(
        !stderr.trim().is_empty(),
        "{route}: the answer to `vigil why {id}` belongs on standard error, and standard error \
         was empty. standard output was:\n{stdout}"
    );
    assert!(
        stdout.trim().is_empty(),
        "{route}: the answer arrives on ONE stream; standard output must stay quiet, and it \
         carried:\n{stdout}"
    );
    assert!(
        !stderr.contains(OWNER_ROUTE_TRAILER),
        "{route}: the refusal names its own cause, so nothing may be appended about the owner \
         route the operator never asked about:\n{stderr}"
    );
}

/// Unfakeable because the store genuinely holds no such event and the answer is
/// read from the two operating-system streams and the exit status of a real
/// process, not from prose.
#[test]
fn an_unknown_detection_id_is_refused_on_both_routes() {
    let deployment = Deployment::new();

    let run = deployment.start();
    let served_by_the_owner = deployment.why(UNKNOWN_ID);
    run.stop();
    deployment.wait_until_free();
    let read_directly = deployment.why(UNKNOWN_ID);

    assert_refused("the running owner", UNKNOWN_ID, &served_by_the_owner);
    assert_refused("the direct read", UNKNOWN_ID, &read_directly);
    for (route, answer) in [
        ("the running owner", &served_by_the_owner),
        ("the direct read", &read_directly),
    ] {
        let stderr = stderr_of(answer).to_ascii_lowercase();
        assert!(
            stderr.contains("not found"),
            "{route}: an id no event carries is a not-found, and the operator has to be told \
             which of the two things went wrong: {stderr}"
        );
    }
}

/// Unfakeable because the two answers are compared against each other: an
/// operator who mistyped an id and an operator asking after an event that is
/// genuinely gone must not receive the same sentence.
#[test]
fn a_malformed_detection_id_is_refused_as_a_malformed_id_and_not_as_a_missing_event() {
    let deployment = Deployment::new();

    let run = deployment.start();
    let malformed_by_the_owner = deployment.why(MALFORMED_ID);
    let unknown_by_the_owner = deployment.why(UNKNOWN_ID);
    run.stop();
    deployment.wait_until_free();
    let malformed_directly = deployment.why(MALFORMED_ID);

    assert_refused("the running owner", MALFORMED_ID, &malformed_by_the_owner);
    assert_refused("the direct read", MALFORMED_ID, &malformed_directly);

    for (route, answer) in [
        ("the running owner", &malformed_by_the_owner),
        ("the direct read", &malformed_directly),
    ] {
        let stderr = stderr_of(answer);
        assert!(
            stderr.contains(MALFORMED_ID),
            "{route}: the refusal names the id that was typed, or the operator cannot see what \
             this is about: {stderr}"
        );
        assert!(
            !stderr.to_ascii_lowercase().contains("not found"),
            "{route}: `{MALFORMED_ID}` is not an event this deployment is missing — it is not a \
             detection id at all — and reporting it as not found sends the operator looking for a \
             recording that was never named: {stderr}"
        );
        assert!(
            stderr.to_ascii_lowercase().contains("uuid"),
            "{route}: the refusal names the form a detection id takes, so the operator can see \
             what to type instead: {stderr}"
        );
    }

    assert_ne!(
        stderr_of(&malformed_by_the_owner),
        stderr_of(&unknown_by_the_owner),
        "a mistyped id and a missing event are two different things and must not arrive as one \
         sentence"
    );
}
