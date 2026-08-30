//! Where a deployment keeps its store and where it keeps its data directory
//! are the operator's two separate choices, and Vigil must honour both.
//!
//! `vigil run --store-path` puts the store on a data volume; `--data-dir` names
//! the deployment directory beside it. Nothing says those are the same place
//! and nothing says the store is called `store.contextgraph`. So a change an
//! operator makes has to land in THE store they configured, and nowhere else:
//! a second store appearing under the deployment directory is not a smaller
//! failure than losing the change, it is worse, because the deployment now has
//! two stores, the operator is reading one and Vigil is writing the other, and
//! nothing on any surface says so.
//!
//! Every fixture here is built to make that failure visible rather than
//! survivable:
//!
//! - the store file has a NON-DEFAULT name, so a path derived from a directory
//!   instead of read from the configuration lands somewhere else rather than
//!   coincidentally on the same file;
//! - the store's directory is not the data directory, and the data directory is
//!   not called `data`, so neither can stand in for the other by accident;
//! - `--store-path` carries the location and `VIGIL_STORE_PATH` is unset for
//!   the runtime, because a flag is how an operator configures a service and a
//!   test that set the variable too would prove only that the variable works;
//! - the configuration file lives outside both directories, where an
//!   operator's really does.
//!
//! Both routes are checked, because they are two different code paths to the
//! same promise: the running node answering through the store it owns, and a
//! command answering directly when nothing is running.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use sha2::{Digest, Sha256};

use vigil::OWNER_SERVED_PREFIX;
use vigil::settings_model::DETECTOR_SAMPLE_FRAMES_SETTING;
use vigil::settings_projection::{NAME_KEY, REQUESTED_KEY, SCOPE_KEY, SETTING_LINE_PREFIX};
use vigil::settings_store::SettingsStore;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_for_store_owner,
    wait_for_store_owner_to_release,
};

/// The deployment directory's own name. Deliberately not `data`, and
/// deliberately not the store directory's name, so no answer derived from
/// either directory can coincide with the other.
const DEPLOYMENT_DIRECTORY: &str = "deployment-state";

/// The store directory's own name — a data volume, in the shape an operator
/// actually deploys.
const STORE_VOLUME_DIRECTORY: &str = "graph-volume";

/// The store's own file name. NOT the product's default, which is the whole
/// point: a store path derived from a directory rather than read from the
/// operator's configuration cannot land on this file by accident.
const CONFIGURED_STORE_FILE: &str = "farm-graph.contextgraph";

/// The value written, two digits and not any product default, so a substring
/// match cannot land on it.
const PINNED_VALUE: &str = "23";

struct Deployment {
    tmp: tempfile::TempDir,
    _config_home: tempfile::TempDir,
    data_dir: PathBuf,
    store_path: PathBuf,
    config_path: PathBuf,
}

impl Deployment {
    fn prepare() -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment tree");
        let config_home = tempfile::tempdir().expect("temporary configuration directory");
        let data_dir = tmp.path().join(DEPLOYMENT_DIRECTORY);
        fs::create_dir_all(&data_dir).expect("deployment directory");
        let store_volume = tmp.path().join(STORE_VOLUME_DIRECTORY);
        fs::create_dir_all(&store_volume).expect("store volume");
        let store_path = store_volume.join(CONFIGURED_STORE_FILE);

        let config_path = config_home.path().join("vigil.toml");
        fs::write(&config_path, "site_name = \"home farm\"\ncameras = []\n")
            .expect("write the configuration file");

        Self {
            tmp,
            _config_home: config_home,
            data_dir,
            store_path,
            config_path,
        }
    }

    /// Start `vigil run` against this deployment, configured by FLAGS alone.
    ///
    /// `VIGIL_STORE_PATH` is removed rather than set: a service is configured
    /// by its command line, and a run that only worked because the variable
    /// agreed would prove nothing about the flag.
    fn start(&self) -> RunningVigil {
        let health = TcpPortReservation::reserve_loopback().expect("reserve a liveness port");
        let review = TcpPortReservation::reserve_loopback().expect("reserve a review port");
        let mut command = Command::new(vigil_binary_path());
        command
            .arg("run")
            .arg("--config")
            .arg(&self.config_path)
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--store-path")
            .arg(&self.store_path)
            .arg("--health-port")
            .arg(health.port().to_string())
            .arg("--review-port")
            .arg(review.port().to_string())
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn `vigil run`");
        let stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());
        let run = RunningVigil { child, stdout };
        wait_for_store_owner(&self.data_dir, &self.store_path).unwrap_or_else(|error| {
            panic!(
                "{error}. The runtime must own the store it was configured with, at {}. Output so \
                 far:\n{}",
                self.store_path.display(),
                run.logs()
            )
        });
        run
    }

    /// A `vigil` command aimed at this deployment, exactly as an operator's
    /// environment would aim it: the deployment directory and the store the
    /// operator configured.
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(vigil_binary_path())
            .args(args)
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env("VIGIL_STORE_PATH", &self.store_path)
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("spawn vigil {args:?}: {error}"))
    }

    /// The node key this deployment records, which every scope on every
    /// surface has to resolve to. Read, never generated, so asking cannot
    /// create the thing being checked.
    fn recorded_node_key(&self) -> Option<String> {
        fs::read_to_string(self.data_dir.join("node-key"))
            .ok()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
    }
}

struct RunningVigil {
    child: Child,
    stdout: std::sync::Arc<std::sync::Mutex<String>>,
}

impl RunningVigil {
    fn logs(&self) -> String {
        self.stdout
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for RunningVigil {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Every regular file under `root`, mapped to a digest of its contents, so a
/// change to any one of them is visible rather than inferred.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => panic!("read {}: {error}", directory.display()),
        };
        for entry in entries {
            let entry = entry.expect("directory entry");
            let file_type = entry.file_type().expect("file type");
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                let bytes = fs::read(entry.path()).unwrap_or_default();
                found.insert(entry.path(), format!("{:x}", Sha256::digest(&bytes)));
            }
        }
    }
    found
}

/// Every path under `root` carrying the product's DEFAULT store file name. A
/// deployment that configured its store elsewhere must have none: one here is
/// a store the operator never asked for and will never look in.
fn default_named_stores(root: &Path) -> Vec<PathBuf> {
    let default_name = SettingsStore::store_path(Path::new("x"))
        .file_name()
        .expect("the product's default store file name")
        .to_os_string();
    let mut found: Vec<PathBuf> = snapshot(root)
        .into_keys()
        .filter(|path| path.file_name() == Some(default_name.as_os_str()))
        .collect();
    found.sort();
    found
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn token_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(&rest[..end])
}

fn setting_line_named<'a>(rendered: &'a str, name: &str) -> Option<&'a str> {
    rendered
        .lines()
        .filter(|line| line.starts_with(&format!("{SETTING_LINE_PREFIX} ")))
        .find(|line| token_field(line, NAME_KEY) == Some(name))
}

fn assert_change_landed(deployment: &Deployment, rendered: &str) {
    let line = setting_line_named(rendered, DETECTOR_SAMPLE_FRAMES_SETTING)
        .unwrap_or_else(|| panic!("no {DETECTOR_SAMPLE_FRAMES_SETTING} line in:\n{rendered}"));
    assert_eq!(
        token_field(line, REQUESTED_KEY),
        Some(PINNED_VALUE),
        "the change must be readable back from the store the operator configured, {}; got: {line}",
        deployment.store_path.display()
    );
}

/// Unfakeable because the store's name is not the product's default and its
/// directory is not the deployment directory, so a path derived from either
/// cannot coincide with the configured one; because the whole tree is hashed
/// before and after, so a second store anywhere is visible rather than
/// inferred; and because the answer's route is read off the marker the product
/// itself writes, so the running node is proven to be the one that answered.
#[test]
fn a_change_made_while_the_runtime_owns_the_store_lands_only_in_that_store() {
    let deployment = Deployment::prepare();
    let run = deployment.start();

    let before = snapshot(deployment.tmp.path());
    let change = deployment.cli(&[
        "settings",
        "set",
        DETECTOR_SAMPLE_FRAMES_SETTING,
        PINNED_VALUE,
    ]);
    let logs = run.logs();
    assert!(
        change.status.success(),
        "the change must land while the node is running; exited {:?}, output:\n{}\nlogs:\n{logs}",
        change.status.code(),
        combined(&change)
    );
    assert!(
        stdout_of(&change).starts_with(OWNER_SERVED_PREFIX),
        "and the RUNNING node must be the one that answered — it holds the store the change is \
         about; output:\n{}\nlogs:\n{logs}",
        combined(&change)
    );

    let listing = deployment.cli(&["settings"]);
    run.stop();
    assert_change_landed(&deployment, &stdout_of(&listing));

    let stray = default_named_stores(deployment.tmp.path());
    assert!(
        stray.is_empty(),
        "this deployment configured its store as {}, so no store carrying the product's default \
         name may exist anywhere in it — one does now, holding records the operator will never \
         see: {stray:?}\nlogs:\n{logs}",
        deployment.store_path.display()
    );

    let after = snapshot(deployment.tmp.path());
    let changed: Vec<&PathBuf> = after
        .iter()
        .filter(|(path, digest)| before.get(*path) != Some(*digest))
        .map(|(path, _)| path)
        .filter(|path| *path != &deployment.store_path)
        .collect();
    let permitted = |path: &Path| {
        path.parent() == Some(deployment.store_path.parent().expect("store volume"))
            || path.file_name().is_some_and(|name| name == "node-key")
    };
    let unexpected: Vec<&&PathBuf> = changed
        .iter()
        .filter(|path| !permitted(path.as_path()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "only the configured store and its own volume may change when a setting is written — the \
         deployment directory is not a second place to record things: {unexpected:?}\nlogs:\n{logs}"
    );
}

/// The same promise on the other route: nothing is running, so the command
/// answers for itself — and it has to answer about the SAME deployment.
///
/// Unfakeable for the same reasons, plus one more: the runtime is reaped and
/// the store proven free before the change is made, so the answer cannot have
/// come from a process that was still holding it.
#[test]
fn a_change_made_with_nothing_running_lands_only_in_the_configured_store() {
    let deployment = Deployment::prepare();
    // A run first, so the deployment exists the way an operator's does — the
    // store created where they configured it, the node key recorded — and then
    // genuinely stopped.
    deployment.start().stop();
    wait_for_store_owner_to_release(&deployment.data_dir, &deployment.store_path)
        .expect("the runtime must release the store before the direct change");

    let before = snapshot(deployment.tmp.path());
    let change = deployment.cli(&[
        "settings",
        "set",
        DETECTOR_SAMPLE_FRAMES_SETTING,
        PINNED_VALUE,
    ]);
    assert!(
        change.status.success(),
        "a change made with nothing running must land; exited {:?}, output:\n{}",
        change.status.code(),
        combined(&change)
    );
    assert!(
        !stdout_of(&change).starts_with(OWNER_SERVED_PREFIX),
        "nothing owns this store, so no owner can have served the change; output:\n{}",
        combined(&change)
    );

    assert_change_landed(&deployment, &stdout_of(&deployment.cli(&["settings"])));

    let stray = default_named_stores(deployment.tmp.path());
    assert!(
        stray.is_empty(),
        "a command that could not reach a running node must still write to the store the operator \
         configured, {}, and never make itself a new one: {stray:?}",
        deployment.store_path.display()
    );
    let after = snapshot(deployment.tmp.path());
    let appeared: Vec<&PathBuf> = after
        .keys()
        .filter(|path| !before.contains_key(*path))
        .filter(|path| path.parent() != deployment.store_path.parent())
        .collect();
    assert!(
        appeared.is_empty(),
        "and it must create nothing outside that store's own volume: {appeared:?}"
    );
}

/// One deployment is one node, whichever way it is asked.
///
/// Settings are recorded against this deployment's node key, which lives in the
/// data directory. If a command resolved its scope from somewhere else — the
/// store's directory, say, which is a different place entirely here — then a
/// value written through the running node would be invisible to the next
/// command and vice versa, and neither surface would say why.
///
/// Unfakeable because the key is READ off disk, never asked for through a
/// surface that could generate it, and because the store directory and the
/// deployment directory have different names, so a scope resolved from the
/// wrong one cannot match by coincidence.
#[test]
fn the_running_node_and_a_separate_command_record_against_one_node_key() {
    let deployment = Deployment::prepare();
    let run = deployment.start();
    let owner_served = deployment.cli(&[
        "settings",
        "set",
        DETECTOR_SAMPLE_FRAMES_SETTING,
        PINNED_VALUE,
    ]);
    let owner_listing = stdout_of(&deployment.cli(&["settings"]));
    let logs = run.logs();
    run.stop();
    wait_for_store_owner_to_release(&deployment.data_dir, &deployment.store_path)
        .expect("the runtime must release the store");
    let direct_listing = stdout_of(&deployment.cli(&["settings"]));

    assert!(
        owner_served.status.success(),
        "the change must land; output:\n{}\nlogs:\n{logs}",
        combined(&owner_served)
    );
    let key = deployment.recorded_node_key().unwrap_or_else(|| {
        panic!("the deployment must record a node key under its data directory; logs:\n{logs}")
    });

    for (label, rendered) in [
        ("served by the running node", &owner_listing),
        ("answered directly", &direct_listing),
    ] {
        let line = setting_line_named(rendered, DETECTOR_SAMPLE_FRAMES_SETTING)
            .unwrap_or_else(|| panic!("no setting line {label}:\n{rendered}"));
        let scope = token_field(line, SCOPE_KEY)
            .unwrap_or_else(|| panic!("the setting line must carry a scope {label}: {line}"));
        assert!(
            scope.contains(&key),
            "the answer {label} recorded against {scope:?}, but this deployment's node key is \
             {key:?}. A surface that resolves its scope from anywhere but the deployment \
             directory is answering about a different node, and the operator is never told; \
             rendered:\n{rendered}"
        );
        assert_eq!(
            token_field(line, REQUESTED_KEY),
            Some(PINNED_VALUE),
            "and both surfaces must see the one value that was written; {label}: {line}"
        );
    }
}
