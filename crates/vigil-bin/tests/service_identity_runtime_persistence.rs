//! Criterion 2a: renaming the site does not rename the node.
//!
//! This node's service identity names the Home Assistant device and the
//! namespace the messaging topics hang off. Recomputing it from the site name
//! on every start means an operator who tidies up a display name comes back to
//! a second, empty device and a history stranded on the first one — the exact
//! harm the persisted identity exists to prevent.
//!
//! Driven through the REAL runtime as a spawned process, twice, against one
//! data directory, because the promise is about what a restarted node
//! announces. The library-level persistence proof lives beside this one in
//! `vigil::service_identity_persistence`; it exercises the resolve/persist
//! functions directly, so it cannot see whether the runtime ever calls them.
//! This test can only pass once startup resolves the identity through the
//! persisted record rather than deriving it from the configured site name.
//!
//! Unfakeable because the identity is read off what the process ANNOUNCES —
//! the value Home Assistant actually receives — on both starts, and the second
//! start runs against a configuration file whose site name has genuinely
//! changed. A build that persists a record but keeps announcing the derived
//! value fails the second assertion; one that announces a stable value by
//! ignoring the rename entirely fails the first, which pins the identity to
//! the site name at FIRST start.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, TcpPortReservation, capture_pipe, vigil_binary_path, wait_until,
};

/// The startup receipt naming the identity the runtime announces. Spelled here
/// because the runtime prints it as a plain receipt line and nothing declares
/// the key; the implementation can fix that by declaring it where the rest of
/// the surface vocabulary lives.
const ANNOUNCED_IDENTITY_PREFIX: &str = "service_id=";

/// The site name this deployment is first started under.
const ORIGINAL_SITE_NAME: &str = "home farm";

/// The identity that site name derives to, and therefore the identity the node
/// keeps for the rest of its life.
const IDENTITY_FROM_ORIGINAL_SITE: &str = "home_farm";

/// The display name an operator later tidies it up to.
const RENAMED_SITE_NAME: &str = "north field";

struct Deployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    config_path: PathBuf,
}

impl Deployment {
    fn prepare() -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment directory");
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&data_dir).expect("create the data directory");
        let config_path = tmp.path().join("vigil.toml");
        let deployment = Self {
            _tmp: tmp,
            data_dir,
            config_path,
        };
        deployment.write_site_name(ORIGINAL_SITE_NAME);
        deployment
    }

    /// Rewrite the configuration file the operator edits. Zero cameras: the
    /// identity is a fact about the node, and a camera would add hardware this
    /// promise does not depend on.
    fn write_site_name(&self, site_name: &str) {
        fs::write(
            &self.config_path,
            format!("cameras = []\nsite_name = \"{site_name}\"\n"),
        )
        .expect("write the configuration file");
    }

    fn start(&self) -> LiveVigil {
        LiveVigil::spawn(&self.config_path, &self.data_dir)
    }
}

struct LiveVigil {
    child: Child,
    stdout: Arc<Mutex<String>>,
}

impl LiveVigil {
    fn spawn(config_path: &Path, data_dir: &Path) -> Self {
        let health = TcpPortReservation::reserve_loopback().expect("reserve a liveness port");
        let review = TcpPortReservation::reserve_loopback().expect("reserve a review port");
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
        // Held until the moment of spawn: a released port is a race another
        // process can steal before this one binds it.
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn vigil run");
        let stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());
        Self { child, stdout }
    }

    /// The identity this process is announcing, read off the startup receipt.
    /// Waited for as a state the run reaches, never on the clock.
    fn announced_service_id(&self) -> String {
        wait_until(
            "the runtime to state the service identity it is announcing",
            RUNTIME_STARTUP_TIMEOUT,
            || {
                let captured = self.stdout.lock().expect("stdout buffer").clone();
                Ok(captured
                    .lines()
                    .find_map(|line| line.strip_prefix(ANNOUNCED_IDENTITY_PREFIX))
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string))
            },
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}. Output so far:\n{}",
                self.stdout.lock().expect("stdout buffer")
            )
        })
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

#[test]
fn a_site_rename_between_runs_does_not_change_the_identity_the_runtime_announces() {
    let deployment = Deployment::prepare();

    let first_run = deployment.start();
    let first_announced = first_run.announced_service_id();
    first_run.stop();

    assert_eq!(
        first_announced, IDENTITY_FROM_ORIGINAL_SITE,
        "a first start derives the identity from the site name it was given, so the node arrives \
         in Home Assistant under a name that means something to the person installing it"
    );

    // The operator tidies up the display name. Nothing else about the
    // deployment changes, and the data directory is the same one.
    deployment.write_site_name(RENAMED_SITE_NAME);

    let second_run = deployment.start();
    let second_announced = second_run.announced_service_id();
    second_run.stop();

    assert_eq!(
        second_announced, IDENTITY_FROM_ORIGINAL_SITE,
        "and a later rename must not move it. The identity persisted at first start is what this \
         node announces for the rest of its life: re-deriving it from the new site name hands \
         Home Assistant an entirely new device and strands every entity history hanging off the \
         old one, which is precisely what an operator renaming a site is not asking for. \
         Announced {second_announced:?} after the rename, against {first_announced:?} before it"
    );
}
