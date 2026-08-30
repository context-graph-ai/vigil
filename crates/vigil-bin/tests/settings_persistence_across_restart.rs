//! Criterion 6: a pin and its attribution survive a full process restart.
//!
//! Not "the number came back" — the AUTHOR and the SURFACE came back too. A
//! store that persists the value but re-derives its attribution at every start
//! silently turns a human's pin into Vigil's own automatic choice, at which
//! point the tuner may revise it and a server push may overrule it. That is the
//! whole authority model failing quietly, so the value alone is not the
//! assertion.
//!
//! Driven as two genuinely separate `vigil run` OS processes against the same
//! data directory, with the pin written through the real `vigil settings set`
//! command in between — never a helper writing rows, and never one process
//! restarting a thread. `vigil settings` does not exist on `dev`
//! (`crates/vigil/src/lib.rs:322-330`), so this fails today.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use vigil::settings_model::{Author, DETECTOR_SAMPLE_FRAMES_SETTING, Surface};
use vigil::settings_projection::{AUTHOR_KEY, CONTROL_STATE_KEY, SETTING_LINE_PREFIX, SURFACE_KEY};
use vigil::settings_store::SettingsStore;

/// The effective-value field key, and the setting-name field key. Neither is
/// declared in `settings_projection` the way control/author/surface/scope/reason
/// are, so both spellings are retyped here deliberately; only the
/// implementation can fix that by declaring them alongside the others.
const VALUE_KEY: &str = "value";
const NAME_KEY: &str = "name";

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_for_store_owner,
    wait_for_store_owner_to_release,
};

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
        let _stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());
        Self { child }
    }

    /// The validated stop path: `Child::kill` against this test's own child
    /// handle — the standard library owns the PID, so nothing here formats a
    /// direct or negative process-group target.
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

/// The inverse wait: nobody owns the store any more, so the previous process
/// is genuinely down and the store is free. That is what makes the next start a
/// real second process rather than a second question to the first one — and a
/// store still held would refuse it outright.
fn wait_for_the_store_to_be_free(data_dir: &Path) {
    wait_for_store_owner_to_release(data_dir, &SettingsStore::store_path(data_dir))
        .expect("the previous runtime must release the store before the next one starts");
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

/// Unfakeable because the pinned value is deliberately NOT the product default
/// and NOT anything the config file says, so a surface that re-resolves from
/// the file and the automatic layer cannot produce it; and because attribution
/// is asserted alongside it, so a store that keeps the number while
/// re-deriving the author fails. The config file below asserts a DIFFERENT
/// value for the same setting, which is the adversarial half: if the two
/// surfaces shared one record, the file would look like the author changing
/// their mind on every boot and would silently revert the pin at restart.
#[test]
fn a_pin_and_its_attribution_survive_a_full_process_restart() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = tmp.path().join("vigil.toml");
    // The config file says 12. The operator will pin 41 through
    // `vigil settings`. Both surfaces keep their own record; the more recent
    // local author wins, and it must still win after the file is read again at
    // the next start. Both fixtures are two digits and share no digit, so a
    // substring or character-level match cannot confuse one for the other or
    // land on either by accident.
    fs::write(&config_path, "cameras = []\ndetector_sample_frames = 12\n").expect("write config");

    let first_run = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_the_store_owner(&data_dir);

    let pinned = vigil_settings(&data_dir, &["set", DETECTOR_SAMPLE_FRAMES_SETTING, "41"]);
    assert!(
        pinned.status.success(),
        "`vigil settings set {DETECTOR_SAMPLE_FRAMES_SETTING} 41` must succeed against the \
         running node — no domain governs the rate controls today; stderr:\n{}",
        stderr_of(&pinned)
    );

    let before = vigil_settings(&data_dir, &[]);
    assert!(
        before.status.success(),
        "`vigil settings` must exit 0 before the restart, stderr:\n{}",
        stderr_of(&before)
    );
    let before_rendered = stdout_of(&before);
    let before_line = setting_named(&before_rendered, DETECTOR_SAMPLE_FRAMES_SETTING)
        .unwrap_or_else(|| {
            panic!("no {DETECTOR_SAMPLE_FRAMES_SETTING} line in:\n{before_rendered}")
        });
    assert_eq!(
        token_field(before_line, VALUE_KEY),
        Some("41"),
        "sanity: the pin must be effective before the restart, or the restart proves nothing; \
         got: {before_line}"
    );

    first_run.stop();
    wait_for_the_store_to_be_free(&data_dir);

    // A genuinely second process against the same data directory — the restart.
    let second_run = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_the_store_owner(&data_dir);
    let after = vigil_settings(&data_dir, &[]);
    second_run.stop();

    assert!(
        after.status.success(),
        "`vigil settings` must exit 0 after the restart, stderr:\n{}",
        stderr_of(&after)
    );
    let after_rendered = stdout_of(&after);
    let after_line =
        setting_named(&after_rendered, DETECTOR_SAMPLE_FRAMES_SETTING).unwrap_or_else(|| {
            panic!("no {DETECTOR_SAMPLE_FRAMES_SETTING} line in:\n{after_rendered}")
        });

    assert_eq!(
        token_field(after_line, VALUE_KEY),
        Some("41"),
        "the pin must still be what runs after a full process restart — the config file's 12 must \
         not revert it, because the file and the `vigil settings` change keep separate records \
         and an unchanged file re-asserts nothing; got: {after_line}"
    );
    assert_eq!(
        token_field(after_line, AUTHOR_KEY),
        Some(Author::LocalExplicit.as_str()),
        "the pin must still be attributed to the human who set it. A value that survives while \
         its author decays to `automatic` is a value Vigil's own tuning may now revise and a \
         server push may now overrule — the authority model failing silently; got: {after_line}"
    );
    assert_eq!(
        token_field(after_line, SURFACE_KEY),
        Some(Surface::VigilSettings.as_str()),
        "the pin must still name the surface it was authored through, so the operator can see \
         which of their own surfaces is winning; got: {after_line}"
    );
    assert_eq!(
        token_field(after_line, CONTROL_STATE_KEY),
        // The `control=` token spelling has no single source:
        // `ControlState::label()` renders the operator PHRASE ("Set by you"),
        // not the whitespace-free token a field carries. Only the
        // implementation can fix that by exporting the token spelling.
        Some("set-by-you"),
        "the control state an operator reads must still be `set-by-you` after the restart; \
         got: {after_line}"
    );
}
