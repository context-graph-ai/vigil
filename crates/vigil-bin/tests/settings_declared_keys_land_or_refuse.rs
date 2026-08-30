//! The five add-on keys a configuration-file edit could not deliver before
//! this arc's fix: `motion_sensitivity`, `restart_on_reflect`,
//! `detector_queue_capacity`, `detection_backend`, `decode_backend`.
//!
//! Each is a real setting the store governs and the operator surface answers
//! for (`vigil::declared_settings`), and each is declared in
//! `addons/vigil/config.yaml`. What none of them had was a reader: the config
//! file / add-on options file is deserialized straight into `PartialConfig`,
//! which had no field for any of the five, so a value typed there was dropped
//! silently rather than refused or authored. This drives the REAL compiled
//! `vigil run` against a real configuration file (the config-file surface
//! travels the identical `surface_assertions` -> `apply_surface` path the
//! add-on options file does) and asserts, per setting, that the store now
//! holds a record naming the config-file surface — or, for the two governed
//! backends, that the startup receipt carries an actionable refusal naming
//! the domain managing them. Silence — no record and no refusal — fails.
//!
//! RED before this arc's fix (`7e851dd`): `PartialConfig` had no field for
//! any of the five, so the config file below set nothing, `vigil settings`
//! answered `automatic`/`automatic-floor` for every one of them, and no
//! refusal line was ever printed for the two backends.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use vigil::settings_backends::{DECODE_BACKEND_SETTING, DETECTION_BACKEND_SETTING};
use vigil::settings_model::{Author, DETECTOR_QUEUE_CAPACITY_SETTING, MOTION_SENSITIVITY_SETTING};
use vigil::settings_projection::{AUTHOR_KEY, SETTING_LINE_PREFIX, SURFACE_KEY};
use vigil::settings_reflection::RESTART_ON_REFLECT_SETTING;
use vigil::settings_store::SettingsStore;

const VALUE_KEY: &str = "value";
const NAME_KEY: &str = "name";

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, TcpPortReservation, capture_pipe, vigil_binary_path,
    wait_for_store_owner, wait_until,
};

struct LiveVigil {
    child: Child,
    stdout: Arc<Mutex<String>>,
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
        let stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());
        Self { child, stdout }
    }

    fn stdout_snapshot(&self) -> String {
        self.stdout.lock().expect("stdout buffer").clone()
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

/// Readiness is the store's OWNER ROUTE answering: the runtime holds this
/// deployment's store, so a command aimed at the deployment reaches that
/// runtime instead of being answered by the asking process. Proven by asking a
/// real question and getting an owner-served answer — never a sleep, and never
/// a printed line, which a runtime that started nothing could also produce.
fn wait_for_the_store_owner(data_dir: &Path) {
    wait_for_store_owner(data_dir, &SettingsStore::store_path(data_dir))
        .expect("the runtime must own this deployment's store and answer through it");
}

fn vigil_settings(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(vigil_binary_path())
        .arg("settings")
        .args(args)
        .env("VIGIL_DATA_DIR", data_dir)
        .env_remove("VIGIL_STORE_PATH")
        .env_remove("VIGIL_CONTROL_SOCKET")
        .stdin(Stdio::null())
        .output()
        .expect("spawn vigil settings")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn token_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(&rest[..end])
}

fn setting_named<'a>(rendered: &'a str, name: &str) -> Option<&'a str> {
    rendered
        .lines()
        .filter(|line| line.starts_with(&format!("{SETTING_LINE_PREFIX} ")))
        .find(|line| token_field(line, NAME_KEY) == Some(name))
}

/// Unfakeable because each of the three plain settings is checked against a
/// value that is neither the product default nor anything the automatic floor
/// would produce, AND the surface + author are checked alongside the value —
/// so an implementation that resolves the number from somewhere else, or
/// authors it without attributing the config file, still fails. The two
/// governed backends are checked for the opposite: an actionable refusal
/// naming the domain that manages them, printed on the startup receipt, never
/// silence and never a record it had no business writing while a domain
/// governs it.
#[test]
fn the_five_previously_silent_add_on_keys_land_from_a_config_file_edit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = tmp.path().join("vigil.toml");
    // `detection_backend`/`decode_backend` are typed as whatever text a build
    // carries; neither "hardware" backend exists while its domain (on by
    // default) manages it, so the write must be refused rather than silently
    // accepted or silently dropped.
    fs::write(
        &config_path,
        "cameras = []\n\
         motion_sensitivity = 3\n\
         restart_on_reflect = false\n\
         detector_queue_capacity = 42\n\
         detection_backend = \"hardware\"\n\
         decode_backend = \"hardware\"\n",
    )
    .expect("write config");

    let live = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_the_store_owner(&data_dir);

    // The refusal is printed once, at the point the config-file surface is
    // applied at startup — waited for explicitly rather than read once,
    // because the control socket answering does not prove the earlier
    // startup-surface pass has finished printing yet.
    wait_until(
        "the startup receipt to report both governed-backend refusals",
        RUNTIME_STARTUP_TIMEOUT,
        || {
            let captured = live.stdout_snapshot();
            let both = captured.contains(&format!(
                "settings-refused setting={DETECTION_BACKEND_SETTING}"
            )) && captured.contains(&format!(
                "settings-refused setting={DECODE_BACKEND_SETTING}"
            ));
            Ok(both.then_some(()))
        },
    )
    .unwrap_or_else(|error| panic!("{error}; output so far:\n{}", live.stdout_snapshot()));

    let output = vigil_settings(&data_dir, &[]);
    live.stop();
    assert!(
        output.status.success(),
        "`vigil settings` must exit 0, stderr:\n{}",
        stderr_of(&output)
    );
    let rendered = stdout_of(&output);

    for (setting, expected_value) in [
        (MOTION_SENSITIVITY_SETTING, "3"),
        (RESTART_ON_REFLECT_SETTING, "false"),
        (DETECTOR_QUEUE_CAPACITY_SETTING, "42"),
    ] {
        let line = setting_named(&rendered, setting).unwrap_or_else(|| {
            panic!(
                "the config file named `{setting}`, so the surface must author a record for it \
                 rather than staying silent; got:\n{rendered}"
            )
        });
        assert_eq!(
            token_field(line, VALUE_KEY),
            Some(expected_value),
            "`{setting}` must carry the value the config file gave it; got: {line}"
        );
        assert_eq!(
            token_field(line, AUTHOR_KEY),
            Some(Author::LocalExplicit.as_str()),
            "`{setting}` must be attributed to the human who edited the file, not to Vigil's own \
             choice; got: {line}"
        );
        assert_eq!(
            token_field(line, SURFACE_KEY),
            Some(vigil::settings_model::Surface::ConfigFile.as_str()),
            "`{setting}` must name the config file as the surface it came from; got: {line}"
        );
    }

    for setting in [DETECTION_BACKEND_SETTING, DECODE_BACKEND_SETTING] {
        if let Some(line) = setting_named(&rendered, setting) {
            assert_ne!(
                token_field(line, AUTHOR_KEY),
                Some(Author::LocalExplicit.as_str()),
                "`{setting}` is governed while its domain is on, so the config file's write must \
                 have been refused rather than landed as a human pin; got: {line}"
            );
        }
    }
}
