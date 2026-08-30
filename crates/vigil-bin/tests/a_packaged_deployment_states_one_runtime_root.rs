//! A packaged Vigil states ONE runtime root, and every side of the deployment
//! meets there.
//!
//! Vigil's live command line is the owner plane: `vigil why`, `vigil events`
//! and `vigil stats` ask the running process through the store's own channel,
//! and that channel is bound below a runtime directory. On an ordinary desktop
//! the platform provides one — `XDG_RUNTIME_DIR`, or `/run/user/<uid>` behind
//! it. A packaged Vigil has neither: a container and a Home Assistant add-on
//! run as a service with no login session, so nothing creates a per-user
//! runtime directory and there is no platform answer to fall back on. Without
//! a stated root the runtime binds nowhere an operator's command can find, and
//! every live command answers about a store nobody is holding — on a
//! deployment whose runtime is up and serving.
//!
//! So the package states the root, once, for the whole owner plane:
//! `CONTEXTDB_OWNER_READ_RUNTIME_DIR` is the ONE variable both sides read
//! (context-graph's `owner_control::owner_read_runtime_dir`, consumed by the
//! owner's `owner_read_config` and by every client's `request_owner`), and a
//! directory supplied that way IS the channel directory rather than a base a
//! child is created inside.
//!
//! What is pinned here is ONE process surface: a daemon started with the root
//! stated and no `XDG_RUNTIME_DIR` in its environment publishes its channel in
//! that exact directory, and separate `vigil why` / `vigil events` /
//! `vigil stats` commands given the SAME root reach the live owner and say so.
//! Provisioning the directory in the image is the implementer's; whether the
//! binary honors it is this file's.
//!
//! The control arm holds the other half: a deployment that states nothing is
//! unchanged. An operator on an ordinary machine, where the platform does
//! provide a runtime directory, keeps exactly the behavior they have — the
//! stated root is a packaging affordance, never a new requirement.
//!
//! ## What this file does NOT prove, stated because it once read as if it did
//!
//! A container is not just a machine with a stated runtime root. It is a
//! machine where the daemon and the operator's shell are DIFFERENT operating-
//! system users and where the store's location is stated in the add-on's
//! options rather than handed to both sides by the harness. This fixture
//! crosses neither of those:
//!
//! * **Identity.** The daemon and all three commands below run as this
//!   harness's own user — the premise is asserted rather than assumed, in
//!   `assert_this_fixture_runs_both_sides_as_one_user`. A packaged Vigil drops
//!   to the configured runtime user before it opens its store while live-
//!   command dispatch keeps the exec's user, and the owner channel authorizes
//!   on that user alone, so the packaged deployment meets a refusal this file
//!   is structurally unable to see. That boundary is pinned at the source in
//!   `vigil/tests/a_packaged_live_command_becomes_the_deployments_runtime_user.rs`
//!   and end-to-end by the add-on image gate, which runs a container this
//!   harness cannot.
//! * **Store location.** Both sides here are handed the same deployment
//!   directory, so they cannot resolve different stores no matter how they
//!   resolve them. In the add-on the daemon reads `store_path` out of
//!   `/data/options.json` and the command never looks at that file, so an
//!   operator who moves their store loses every live command. That boundary is
//!   pinned in
//!   `vigil/tests/a_live_command_resolves_the_store_the_daemon_opened.rs`.
//!
//! Nothing here waits on a clock or asserts on elapsed time. Readiness is the
//! runtime's own boot line followed by a real owner-served answer, and the
//! channel is proven present by the file it binds rather than by waiting a
//! chosen interval for it.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use contextdb_engine::local_transport::{derive_channel_address, opaque_channel_basename};
use vigil::OWNER_SERVED_PREFIX;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, TcpPortReservation, capture_pipe, fresh_store_copy, vigil_binary_path,
    wait_until,
};

/// The one variable that states where this deployment's owner plane lives.
const RUNTIME_DIR_VARIABLE: &str = "CONTEXTDB_OWNER_READ_RUNTIME_DIR";

/// The platform variable a packaged service does not have. Scrubbed from every
/// child below so the stated root is the only answer available to them.
const PLATFORM_RUNTIME_VARIABLE: &str = "XDG_RUNTIME_DIR";

/// Enough seeded observations that every read command has something real to
/// answer with, so an empty answer cannot pass as a served one.
const SEEDED_DETECTIONS: usize = 2;

/// The three live reads an operator has, with the field each served answer
/// carries.
struct Shape {
    label: &'static str,
    request: &'static [&'static str],
    served_field: &'static str,
}

const LIVE_READS: [Shape; 3] = [
    Shape {
        label: "vigil events",
        request: &["events"],
        served_field: "observation_id=",
    },
    Shape {
        label: "vigil why --latest",
        request: &["why", "--latest"],
        served_field: "selection=",
    },
    Shape {
        label: "vigil stats",
        request: &["stats"],
        served_field: "frames-received",
    },
];

/// A runtime root of the shape a package provisions: a directory this
/// deployment owns, owner-only, at a shallow pathname (a local channel address
/// is capped by the kernel and the fixed channel basename is most of it).
struct StatedRoot {
    path: PathBuf,
}

impl StatedRoot {
    fn provision(label: &str) -> Self {
        let base = PathBuf::from("/tmp");
        let mut path = base.join(format!("vg-{label}-{}", std::process::id()));
        let mut suffix = 0;
        while path.exists() {
            suffix += 1;
            path = base.join(format!("vg-{label}-{}-{suffix}", std::process::id()));
        }
        fs::create_dir(&path).expect("provision the runtime root this deployment states");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("the runtime root a package provisions is owner-only");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StatedRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct Deployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    store_path: PathBuf,
    config_path: PathBuf,
}

impl Deployment {
    fn new() -> Self {
        let (tmp, store_path) =
            fresh_store_copy(SEEDED_DETECTIONS).expect("seed a deterministic detection fixture");
        let data_dir = store_path
            .parent()
            .expect("the seeded store sits inside a data directory")
            .to_path_buf();
        let config_path = tmp.path().join("vigil.toml");
        fs::write(&config_path, "cameras = []\n").expect("write the configuration file");
        Self {
            _tmp: tmp,
            data_dir,
            store_path,
            config_path,
        }
    }

    /// One read command, run the way a packaged deployment runs it: told the
    /// same root the daemon was told, and with no platform runtime directory
    /// in its environment.
    fn read(&self, stated_root: Option<&Path>, args: &[&str]) -> Output {
        let mut command = Command::new(vigil_binary_path());
        command
            .args(args)
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null());
        match stated_root {
            Some(root) => {
                command.env(RUNTIME_DIR_VARIABLE, root);
                command.env_remove(PLATFORM_RUNTIME_VARIABLE);
            }
            None => {
                command.env_remove(RUNTIME_DIR_VARIABLE);
            }
        }
        command
            .output()
            .unwrap_or_else(|error| panic!("spawn `vigil {}`: {error}", args.join(" ")))
    }

    /// Start the daemon and wait until it owns its store and answers over the
    /// owner plane the caller will use.
    fn start(&self, stated_root: Option<&Path>) -> LiveVigil {
        let health = TcpPortReservation::reserve_loopback().expect("reserve a liveness port");
        let review = TcpPortReservation::reserve_loopback().expect("reserve a review port");
        let mut command = Command::new(vigil_binary_path());
        command
            .arg("run")
            .arg("--config")
            .arg(&self.config_path)
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--health-port")
            .arg(health.port().to_string())
            .arg("--review-port")
            .arg(review.port().to_string())
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match stated_root {
            Some(root) => {
                command.env(RUNTIME_DIR_VARIABLE, root);
                command.env_remove(PLATFORM_RUNTIME_VARIABLE);
            }
            None => {
                command.env_remove(RUNTIME_DIR_VARIABLE);
            }
        }
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn `vigil run`");
        let stdout = capture_pipe(child.stdout.take());
        let stderr = capture_pipe(child.stderr.take());
        let live = LiveVigil { child };
        let read = |pipe: &std::sync::Arc<std::sync::Mutex<String>>| -> String {
            pipe.lock().map(|text| text.clone()).unwrap_or_default()
        };

        wait_until(
            "the runtime to finish coming up",
            RUNTIME_STARTUP_TIMEOUT,
            || {
                let logs = read(&stdout);
                if logs.contains("store_unreadable=true") {
                    return Err(format!(
                        "the runtime could not open the store it was pointed at, so it can own \
                         nothing and answer nothing:\n{logs}"
                    ));
                }
                Ok(logs.contains("boot_phase=pipeline-up").then_some(()))
            },
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}\nstdout:\n{}\nstderr:\n{}",
                read(&stdout),
                read(&stderr)
            )
        });
        live
    }

    /// Where this store's channel is bound below a stated root. Derived the
    /// same way both sides of the owner plane derive it — from the store's own
    /// resolved pathname — so a channel found here is this deployment's and no
    /// other's.
    fn channel_in(&self, root: &Path) -> PathBuf {
        let address = derive_channel_address(&self.store_path)
            .expect("derive this store's own channel address");
        root.join(opaque_channel_basename(address))
    }
}

struct LiveVigil {
    child: Child,
}

impl LiveVigil {
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

/// This fixture's PREMISE, asserted rather than assumed: the daemon and the
/// commands that read it are the same operating-system user.
///
/// It is stated as an assertion because the packaged deployment's premise is
/// the opposite one. The owner channel authorizes on the peer's operating-
/// system user and nothing else, so a container whose daemon dropped to the
/// configured runtime user while the operator's shell stayed root meets a
/// refusal — and this fixture, running both sides as one user, is structurally
/// unable to observe it. Reading the two identities back here is what stops a
/// later reader taking a green run of this file as evidence that the packaged
/// journey works.
fn assert_this_fixture_runs_both_sides_as_one_user(live: &LiveVigil) {
    let status = fs::read_to_string(format!("/proc/{}/status", live.child.id()))
        .expect("read the daemon's own identity from the operating system");
    let daemon_uid: u32 = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|values| values.split_whitespace().nth(1))
        .and_then(|effective| effective.parse().ok())
        .expect("the daemon's effective user");
    // Safe: `geteuid` reads this process's own identity and cannot fail.
    let command_uid = unsafe { libc::geteuid() };
    assert_eq!(
        daemon_uid, command_uid,
        "this file's proof is about the runtime ROOT, and it holds only while both sides of the \
         owner plane are one user. They are not one user in the package: the daemon drops to the \
         configured runtime user before it opens its store and live-command dispatch keeps the \
         exec's user, which is a refusal this fixture cannot see. If these two ever differ here, \
         this file is no longer testing what it says it is."
    );
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Assert every live read reached the running owner and came back with the
/// real answer, marked as owner-served.
fn assert_every_read_reached_the_owner(deployment: &Deployment, stated_root: Option<&Path>) {
    for shape in &LIVE_READS {
        let asked = wait_until(
            "the running owner to serve this read",
            RUNTIME_STARTUP_TIMEOUT,
            || {
                let asked = deployment.read(stated_root, shape.request);
                let served = asked.status.success()
                    && stdout_of(&asked).starts_with(OWNER_SERVED_PREFIX)
                    && !stdout_of(&asked)
                        .strip_prefix(OWNER_SERVED_PREFIX)
                        .unwrap_or_default()
                        .starts_with("owner-error");
                Ok(served.then_some(asked))
            },
        )
        .unwrap_or_else(|error| {
            let asked = deployment.read(stated_root, shape.request);
            panic!(
                "{error}. {}: the runtime is up and holding this store, so this command must \
                 reach it over the owner plane rather than answer about a store nobody is \
                 holding. standard output was:\n{}\nstandard error was:\n{}",
                shape.label,
                stdout_of(&asked),
                stderr_of(&asked)
            )
        });
        let body = stdout_of(&asked)
            .strip_prefix(OWNER_SERVED_PREFIX)
            .unwrap_or_default()
            .to_string();
        assert!(
            body.contains(shape.served_field),
            "{}: the owner answered, and the answer must be the real one — the field `{}` is \
             what a served answer carries. the body was:\n{body}",
            shape.label,
            shape.served_field
        );
    }
}

#[test]
fn a_daemon_told_where_its_runtime_root_is_serves_every_command_told_the_same_root() {
    let deployment = Deployment::new();
    let root = StatedRoot::provision("stated");
    let channel = deployment.channel_in(root.path());

    let live = deployment.start(Some(root.path()));
    assert_this_fixture_runs_both_sides_as_one_user(&live);

    // The channel is where the deployment said it would be. Proven by the
    // socket the owner bound, in the stated directory itself — a supplied
    // runtime directory IS the channel directory, so a channel that landed in
    // a child below it would be a directory the client never looks in.
    wait_until(
        "the owner's channel to appear in the runtime root this deployment stated",
        RUNTIME_STARTUP_TIMEOUT,
        || Ok(channel.exists().then_some(())),
    )
    .unwrap_or_else(|error| {
        let listing: Vec<String> = fs::read_dir(root.path())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        panic!(
            "{error}: a packaged deployment has no platform runtime directory to fall back on, \
             so an owner that did not bind in the root it was given is unreachable by every \
             command. Expected {} — the root held: {listing:?}",
            channel.display()
        )
    });

    assert_every_read_reached_the_owner(&deployment, Some(root.path()));

    live.stop();
}

#[test]
fn a_deployment_that_states_no_runtime_root_is_unchanged() {
    // The control: on a machine whose platform provides a runtime directory,
    // stating nothing must behave exactly as it always has. The stated root is
    // an affordance for deployments with no platform answer, never a new
    // requirement for the ones that have one.
    let deployment = Deployment::new();
    let live = deployment.start(None);
    assert_this_fixture_runs_both_sides_as_one_user(&live);
    assert_every_read_reached_the_owner(&deployment, None);
    live.stop();
}
