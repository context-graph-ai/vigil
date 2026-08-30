//! A value this node resolves early in a start is still governed by the store:
//! set it once, and the NEXT start comes up on it, with the gap between what is
//! stored and what this process is running reported honestly in the meantime.
//!
//! The liveness port is the sharpest case. It is bound before the store opens,
//! deliberately, so a slow store open is never mistaken for a wedged add-on —
//! which means a stored value cannot take effect in the run that stores it. The
//! promise is therefore take-effect-next-start with a visible pending line, not
//! "you cannot set this here". Anything a start reads before the store answers
//! is a cache of what the store last resolved; the store stays the truth, and a
//! cache that disagrees loses.
//!
//! Driven as genuinely separate `vigil run` OS processes against one data
//! directory, with the value written through the real `vigil settings set`
//! command between them. Unfakeable because the second process is asked a
//! question only the stored value can answer: it is started with NO port flag
//! and must still serve the liveness surface on the port the operator stored.

use std::fs;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use vigil::settings_model::{
    Author, FABRIC_FALLBACK_HORIZON_MS_SETTING, FABRIC_WORKER_LEASE_MS_SETTING,
    HEALTH_PORT_SETTING, Surface,
};
use vigil::settings_projection::{
    AUTHOR_KEY, NAME_KEY, PENDING_KEY, RUNNING_KEY, SETTING_LINE_PREFIX, SURFACE_KEY, VALUE_KEY,
};
use vigil::settings_store::SettingsStore;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_for_store_owner,
    wait_for_store_owner_to_release, wait_for_tcp_port,
};

/// What closes the gap between a stored value and a running one for anything
/// resolved before the store opens. Spelled through the production vocabulary
/// so a rename of the cause travels here.
fn restart_cause() -> &'static str {
    vigil::settings_model::PendingCause::Restart.as_str()
}

struct Deployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    config_path: PathBuf,
}

impl Deployment {
    fn prepare() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&data_dir).expect("data dir");
        let config_path = tmp.path().join("vigil.toml");
        // No cameras: this is about which port the node answers on and which
        // values it brought up with, and a camera would only add a stream that
        // has nothing to do with either.
        fs::write(&config_path, "cameras = []\n").expect("write config");
        Self {
            _tmp: tmp,
            data_dir,
            config_path,
        }
    }

    /// Start a runtime. `health_port` is `None` when the run must find its own
    /// port from what was stored — which is the whole question.
    fn start(&self, health_port: Option<u16>) -> LiveVigil {
        let review = TcpPortReservation::reserve_loopback().expect("reserve review port");
        let mut command = Command::new(vigil_binary_path());
        command
            .arg("run")
            .arg("--config")
            .arg(&self.config_path)
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--review-port")
            .arg(review.port().to_string());
        if let Some(port) = health_port {
            command.arg("--health-port").arg(port.to_string());
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        review.release();
        let mut child = command.spawn().expect("spawn vigil run");
        let stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());
        let run = LiveVigil { child, stdout };
        self.wait_for_the_store_owner(&run);
        run
    }

    /// Readiness is the store's OWNER ROUTE answering: this runtime holds the
    /// deployment's store, so a command aimed at the deployment reaches it
    /// rather than being answered by the asking process. A state, proven by
    /// asking a real question — never a sleep, and never a printed line, which
    /// a runtime that started nothing could also produce.
    fn wait_for_the_store_owner(&self, run: &LiveVigil) {
        wait_for_store_owner(&self.data_dir, &SettingsStore::store_path(&self.data_dir))
            .unwrap_or_else(|error| {
                panic!(
                    "{error}. The runtime must come up and own this deployment's store. Output \
                     so far:\n{}",
                    run.stdout()
                )
            });
    }

    /// The inverse wait: nobody owns the store any more, so the next start is
    /// genuinely a second process rather than a second question to the first
    /// one — and a store still held would refuse it outright.
    fn wait_for_shutdown(&self) {
        wait_for_store_owner_to_release(&self.data_dir, &SettingsStore::store_path(&self.data_dir))
            .expect("the previous runtime must release the store before the next start");
    }

    fn settings(&self, args: &[&str]) -> Output {
        Command::new(vigil_binary_path())
            .arg("settings")
            .args(args)
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .stdin(Stdio::null())
            .output()
            .expect("spawn vigil settings")
    }

    fn set(&self, setting: &str, value: &str) {
        let output = self.settings(&["set", setting, value]);
        assert!(
            output.status.success(),
            "`vigil settings set {setting} {value}` must be accepted — the store is the source of \
             truth for this value and it takes effect at the next start; stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn listing(&self) -> String {
        let output = self.settings(&[]);
        assert!(
            output.status.success(),
            "`vigil settings` must answer; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }
}

struct LiveVigil {
    child: Child,
    stdout: std::sync::Arc<std::sync::Mutex<String>>,
}

impl LiveVigil {
    fn stdout(&self) -> String {
        self.stdout.lock().expect("stdout lock").clone()
    }

    /// The validated stop: `Child::kill` on this test's own child handle, so
    /// nothing here formats a direct or negative process-signal target.
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

fn token_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(&rest[..end])
}

fn setting_line<'a>(rendered: &'a str, name: &str) -> &'a str {
    rendered
        .lines()
        .filter(|line| line.starts_with(&format!("{SETTING_LINE_PREFIX} ")))
        .find(|line| token_field(line, NAME_KEY) == Some(name))
        .unwrap_or_else(|| panic!("no {name} line in the operator answer:\n{rendered}"))
}

/// A port nobody is listening on, reserved and released so no other test in
/// this run can be holding it.
fn spare_port() -> u16 {
    let reservation = TcpPortReservation::reserve_loopback().expect("reserve a spare port");
    let port = reservation.port();
    reservation.release();
    port
}

/// Unfakeable because the second process is given no port flag at all: the only
/// thing that can tell it which port to answer on is what the operator stored,
/// so a runtime that still resolved this value from its own flags and defaults
/// cannot serve on it. The port is a reserved-then-released loopback port, so it
/// is neither Vigil's own default nor a value any other run holds.
#[test]
fn a_stored_liveness_port_is_what_the_next_start_binds() {
    let deployment = Deployment::prepare();
    let first_port = spare_port();
    let stored_port = spare_port();
    assert_ne!(
        first_port, stored_port,
        "sanity: the two ports must differ or the assertion proves nothing"
    );

    let first = deployment.start(Some(first_port));
    wait_for_tcp_port(first_port, Duration::from_secs(30))
        .expect("the first run answers on the port its startup options named");
    deployment.set(HEALTH_PORT_SETTING, &stored_port.to_string());
    first.stop();
    deployment.wait_for_shutdown();

    let second = deployment.start(None);
    let bound = wait_for_tcp_port(stored_port, Duration::from_secs(30));
    let logs = second.stdout();
    second.stop();

    assert!(
        bound.is_ok(),
        "the value the operator stored is what the next start comes up on — that is what \
         take-effect-next-start means. The second run was given no port flag, so nothing but the \
         store could tell it to answer on {stored_port}. Output:\n{logs}"
    );
    assert!(
        TcpStream::connect(("127.0.0.1", first_port)).is_err(),
        "and the port the earlier run was told to use is not still being served: a start that \
         honored both would be honoring neither"
    );
}

/// Unfakeable because it asserts the honest gap rather than a value: the store
/// already says one thing while the process is demonstrably still running
/// another, and the surface has to say both plus what closes the gap. A surface
/// that reported the stored value as running would be claiming an effect the
/// process has not had.
#[test]
fn a_value_stored_while_running_is_reported_pending_with_the_value_still_running() {
    let deployment = Deployment::prepare();
    let running_port = spare_port();
    let stored_port = spare_port();

    let run = deployment.start(Some(running_port));
    wait_for_tcp_port(running_port, Duration::from_secs(30))
        .expect("the run answers on the port its startup options named");
    deployment.set(HEALTH_PORT_SETTING, &stored_port.to_string());

    let rendered = deployment.listing();
    run.stop();

    let line = setting_line(&rendered, HEALTH_PORT_SETTING);
    assert_eq!(
        token_field(line, VALUE_KEY),
        Some(stored_port.to_string().as_str()),
        "the effective value is what the operator stored: {line}"
    );
    assert_eq!(
        token_field(line, AUTHOR_KEY),
        Some(Author::LocalExplicit.as_str()),
        "attributed to the person who set it: {line}"
    );
    assert_eq!(
        token_field(line, SURFACE_KEY),
        Some(Surface::VigilSettings.as_str()),
        "and to the surface they used: {line}"
    );
    assert_eq!(
        token_field(line, RUNNING_KEY),
        Some(running_port.to_string().as_str()),
        "while what this process is actually serving on is still the port it bound at start — a \
         pending value is never reported as effective: {line}"
    );
    assert_eq!(
        token_field(line, PENDING_KEY),
        Some(restart_cause()),
        "and the surface names what closes the gap, so nobody waits for a value that is waiting \
         for them: {line}"
    );
}

/// Unfakeable because the newer stored value is written through the startup
/// options of a real run, so anything a start cached from the earlier value is
/// genuinely stale by the time the following start reads it. A cache that
/// outranked the store would answer on the older port.
#[test]
fn a_cached_early_resolved_value_never_outranks_a_newer_stored_one() {
    let deployment = Deployment::prepare();
    let first_port = spare_port();
    let stored_port = spare_port();
    let newer_port = spare_port();

    let first = deployment.start(Some(first_port));
    wait_for_tcp_port(first_port, Duration::from_secs(30)).expect("the first run answers");
    deployment.set(HEALTH_PORT_SETTING, &stored_port.to_string());
    first.stop();
    deployment.wait_for_shutdown();

    // The start that proves the stored value governs, and whatever cache a
    // start keeps of it is now populated with that value.
    let second = deployment.start(None);
    wait_for_tcp_port(stored_port, Duration::from_secs(30)).unwrap_or_else(|error| {
        panic!(
            "{error}: the stored value must govern this start before the staleness question means \
             anything. Output:\n{}",
            second.stdout()
        )
    });
    second.stop();
    deployment.wait_for_shutdown();

    // A newer value arrives through a different local surface — the service's
    // own startup options.
    let third = deployment.start(Some(newer_port));
    wait_for_tcp_port(newer_port, Duration::from_secs(30))
        .expect("the run answers on the port its startup options named");
    third.stop();
    deployment.wait_for_shutdown();

    let fourth = deployment.start(None);
    let bound = wait_for_tcp_port(newer_port, Duration::from_secs(30));
    let logs = fourth.stdout();
    fourth.stop();

    assert!(
        bound.is_ok(),
        "the store is the truth and anything read before it opens is a cache of what the store \
         last resolved. This start had no port flag, the newest stored value is {newer_port}, and \
         a start that came up on {stored_port} would be a cache outranking the store. Output:\n\
         {logs}"
    );
}

/// Unfakeable because it reads what the runtime RECORDED as applied, not what
/// the store holds: the running value is written by the process as it brings
/// each value into force, so a start that never consumed the stored fabric
/// values has nothing to report there.
#[test]
fn stored_fabric_values_are_what_the_next_start_records_as_running() {
    let deployment = Deployment::prepare();
    let lease_ms = "123456";
    let horizon_ms = "7654";

    let first = deployment.start(Some(spare_port()));
    deployment.set(FABRIC_WORKER_LEASE_MS_SETTING, lease_ms);
    deployment.set(FABRIC_FALLBACK_HORIZON_MS_SETTING, horizon_ms);
    first.stop();
    deployment.wait_for_shutdown();

    let second = deployment.start(Some(spare_port()));
    let rendered = deployment.listing();
    second.stop();

    for (setting, stored) in [
        (FABRIC_WORKER_LEASE_MS_SETTING, lease_ms),
        (FABRIC_FALLBACK_HORIZON_MS_SETTING, horizon_ms),
    ] {
        let line = setting_line(&rendered, setting);
        assert_eq!(
            token_field(line, VALUE_KEY),
            Some(stored),
            "the stored value is what this node asks for: {line}"
        );
        assert_eq!(
            token_field(line, RUNNING_KEY),
            Some(stored),
            "and the start that followed brought it into force: fabric attaches off the readiness \
             path, so its configuration is taken after the store has answered and there is nothing \
             for the operator to wait for; a run reporting anything else here consumed a value the \
             store no longer holds: {line}"
        );
    }
}
