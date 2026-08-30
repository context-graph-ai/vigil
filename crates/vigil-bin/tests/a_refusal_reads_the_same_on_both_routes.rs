//! The same condition reads the same whichever road answered it.
//!
//! A live command is answered by the running owner when one holds the store and
//! by a direct read of the file when none does. Which of the two served an
//! operator is not something they chose, is not something a script can see
//! coming, and must not change what either of them is told: the same failed
//! enrollment, the same failed forget, the same everything — one payload, one
//! stream, one exit status. The only difference the routes are allowed is the
//! marker saying the running owner served it.
//!
//! What that was worth: the owner route rendered its failures through the
//! shared markers while the direct fallback formatted the error itself, so a
//! stopped deployment answered a failed enrollment with a bare debug string
//! that nothing recognized as a refusal — and vigil then stapled `runtime owner
//! unavailable: no process is holding the store` onto it, telling the operator
//! about a road they never asked about, beneath an answer that had nothing to
//! do with it.
//!
//! Both routes are driven against the SAME deployment in the same test: the
//! runtime is started, asked, stopped, and asked again, so the two answers
//! differ in nothing except which process produced them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use vigil::settings_store::SettingsStore;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, open_store_at, vigil_binary_path, wait_for_store_owner,
    wait_for_store_owner_to_release,
};

/// The note that must never be stapled onto an answer that names its own cause.
const OWNER_ROUTE_TRAILER: &str = "runtime owner unavailable";

/// A well-formed detection id no event carries, and a name nothing is enrolled
/// under: two real failures an operator meets by mistyping.
const UNKNOWN_ID: &str = "11111111-2222-3333-4444-555555555555";
const UNKNOWN_NAME: &str = "nobody-by-that-name";

/// One request asked on both roads.
struct Shape {
    label: &'static str,
    request: &'static [&'static str],
}

const SHAPES: [Shape; 3] = [
    Shape {
        label: "an enrollment naming a detection that is not there",
        request: &["enroll", UNKNOWN_ID, "somebody"],
    },
    Shape {
        label: "a forget naming a subject nothing is enrolled under",
        request: &["forget", UNKNOWN_NAME],
    },
    Shape {
        label: "a walk of a detection id nothing carries",
        request: &["why", UNKNOWN_ID],
    },
];

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
        let mut child = command.spawn().expect("spawn `vigil run`");
        let _stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());
        Self { child }
    }

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

struct Deployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    config_path: PathBuf,
}

impl Deployment {
    fn prepare() -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment directory");
        let data_dir = tmp.path().join("data");
        // A real store, so the direct read below reads an existing deployment
        // rather than the never-started answer, which is its own journey.
        drop(open_store_at(&data_dir.join("store.contextgraph")).expect("seed the store"));
        let config_path = tmp.path().join("vigil.toml");
        fs::write(&config_path, "cameras = []\n").expect("write the configuration file");
        Self {
            _tmp: tmp,
            data_dir,
            config_path,
        }
    }

    fn ask(&self, request: &[&str]) -> Output {
        Command::new(vigil_binary_path())
            .args(request)
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("spawn `vigil {}`: {error}", request.join(" ")))
    }
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// The answer with the owner-served marker taken off, which is the one
/// difference the two roads are allowed.
fn payload(output: &Output) -> String {
    let marker = vigil::OWNER_SERVED_PREFIX;
    let joined = format!("{}{}", stdout_of(output), stderr_of(output));
    joined.replace(marker, "")
}

/// Unfakeable because both answers come from the same deployment and the same
/// binary within one test: the runtime is genuinely up for the first answer
/// (proven by the owner route serving it) and genuinely gone for the second
/// (proven by the owner route no longer answering), so nothing but the road
/// differs.
#[test]
fn a_failed_request_reads_the_same_served_by_the_owner_and_read_directly() {
    let deployment = Deployment::prepare();
    let store_path = SettingsStore::store_path(&deployment.data_dir);

    let run = LiveVigil::spawn(&deployment.config_path, &deployment.data_dir);
    wait_for_store_owner(&deployment.data_dir, &store_path)
        .expect("the runtime must own this deployment's store and answer through it");
    let served: Vec<Output> = SHAPES
        .iter()
        .map(|shape| deployment.ask(shape.request))
        .collect();
    run.stop();
    wait_for_store_owner_to_release(&deployment.data_dir, &store_path)
        .expect("the runtime must release the store before the direct reads");
    let direct: Vec<Output> = SHAPES
        .iter()
        .map(|shape| deployment.ask(shape.request))
        .collect();

    let mut failures = Vec::new();
    for ((shape, owner), file) in SHAPES.iter().zip(&served).zip(&direct) {
        if owner.status.code() != file.status.code() {
            failures.push(format!(
                "{}: the owner-served answer exited {:?} and the direct read exited {:?}. Which \
                 process happened to be holding the store is not something the operator chose, \
                 and a script cannot branch on it.\nowner:\n{}\ndirect:\n{}",
                shape.label,
                owner.status.code(),
                file.status.code(),
                payload(owner),
                payload(file)
            ));
        }
        if stdout_of(owner).trim().is_empty() != stdout_of(file).trim().is_empty() {
            failures.push(format!(
                "{}: the two roads answered on different streams — owner standard output \
                 {:?}, direct standard output {:?}",
                shape.label,
                stdout_of(owner),
                stdout_of(file)
            ));
        }
        if payload(owner) != payload(file) {
            failures.push(format!(
                "{}: the same condition arrived as two different answers. Under the owner-served \
                 marker, the payload must be the same text.\nowner:\n{}\ndirect:\n{}",
                shape.label,
                payload(owner),
                payload(file)
            ));
        }
        for (route, answer) in [("owner-served", owner), ("read directly", file)] {
            let text = payload(answer);
            if text.contains(OWNER_ROUTE_TRAILER) {
                failures.push(format!(
                    "{}, {route}: the answer names its own cause, so nothing may be appended \
                     about the owner route the operator never asked about:\n{text}",
                    shape.label
                ));
            }
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
