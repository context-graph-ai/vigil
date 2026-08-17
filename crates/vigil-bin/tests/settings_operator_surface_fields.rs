//! Criterion 2: the operator surface shows, for every setting this arc
//! touches, the effective value, the control state, the author, the surface,
//! the scope, and the reason — plus requested, running and pending side by
//! side, and this node's service identifier read-only with how it was arrived
//! at.
//!
//! Driven through the REAL compiled `vigil settings` command as a separate OS
//! process, because the promise is about what an operator sees when they type
//! the command, not about what a projection struct can hold. `vigil settings`
//! does not exist on `dev` at all (`crates/vigil/src/lib.rs:322-330` dispatches
//! only `events`/`why`/`stats`/`enroll`/`forget`/`doctor`/`fabric`/
//! `detector-probe`/`run`), so every test in this file fails today and can only
//! pass once the command, the settings store, and the one projection in
//! `settings_projection.rs` exist.
//!
//! Assertions are made against the line prefixes and field keys declared in
//! `vigil::settings_projection`, the setting names declared in
//! `vigil::settings_model`, and the rendered spellings `Author::as_str` and
//! `PendingCause::as_str` — imported, never retyped — so a rename of the
//! operator surface's own vocabulary moves these tests with it instead of
//! leaving them asserting a stale string. Four spellings are still literals
//! here because nothing declares them yet: the `value=` and `name=` field keys
//! (see `VALUE_KEY`/`NAME_KEY` below), and the identity line's `derivation=`
//! and `access=` keys. Only the implementation can fix that, by declaring them
//! alongside the field keys it already exports.

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vigil::settings_model::{
    Author, DETECTOR_CLASSES_SETTING, DETECTOR_QUEUE_CAPACITY_SETTING,
    DETECTOR_SAMPLE_FRAMES_SETTING, PendingCause, SERVICE_IDENTITY_SETTING,
};
use vigil::settings_projection::{
    AUTHOR_KEY, CONTROL_STATE_KEY, IDENTITY_LINE_PREFIX, PENDING_KEY, REASON_KEY, REQUESTED_KEY,
    RUNNING_KEY, SCOPE_KEY, SETTING_LINE_PREFIX, SURFACE_KEY,
};
use vigil::settings_reflection::RESTART_ON_REFLECT_SETTING;

/// The effective-value field. There is no constant for it in
/// `settings_projection` — unlike control/author/surface/scope/reason it is not
/// declared there — so the spelling is retyped here deliberately. Only the
/// implementation can fix that by declaring it alongside the others.
const VALUE_KEY: &str = "value";

/// The setting-name field. Same situation as `VALUE_KEY`: no declared constant,
/// so the literal stands until the implementation declares one.
const NAME_KEY: &str = "name";

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, TcpPortReservation, capture_pipe, vigil_binary_path, wait_until,
};

/// The startup receipt naming the identity the runtime announces. Spelled here
/// because the runtime prints it as a plain receipt line and nothing declares
/// the key; the implementation can fix that by declaring it where the other
/// surface vocabulary lives.
const ANNOUNCED_IDENTITY_PREFIX: &str = "service_id=";

/// The settings this arc authors or re-points at the store. Every one of them
/// must appear on the listing carrying all six fields; a surface that renders
/// one well-formed line and drops the rest cannot satisfy this.
const SETTINGS_THIS_ARC_TOUCHES: [&str; 6] = [
    // The two automatic-management domain switches, each an ordinary setting.
    "accelerated_detection",
    "hardware_decoding",
    // The two operator-facing backend settings this arc authors so the
    // take-back promise has something to land on.
    "detection_backend",
    "decode_backend",
    // The class list, re-pointed at the store.
    DETECTOR_CLASSES_SETTING,
    // The reflection policy setting.
    RESTART_ON_REFLECT_SETTING,
];

/// The six fields criterion 2 requires on EVERY setting line. `reason` is
/// checked separately because it is free prose running to the end of the line;
/// every other field's value is a single whitespace-free token. The field keys
/// come from `settings_projection`, so a rename of the surface's own vocabulary
/// moves these assertions with it.
const REQUIRED_TOKEN_FIELDS: [&str; 5] = [
    VALUE_KEY,
    CONTROL_STATE_KEY,
    AUTHOR_KEY,
    SURFACE_KEY,
    SCOPE_KEY,
];

/// Requested, running and pending, side by side on the same line.
const SIDE_BY_SIDE_FIELDS: [&str; 3] = [REQUESTED_KEY, RUNNING_KEY, PENDING_KEY];

// ── Process fixture ────────────────────────────────────────────────────────

struct LiveVigil {
    child: Child,
    stdout: Arc<Mutex<String>>,
}

impl LiveVigil {
    /// A real `vigil run` process owning `data_dir`. Zero cameras: the
    /// legitimate worker/discovery deployment shape, so the runtime reaches its
    /// control listener without hardware.
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
            // The queue-capacity environment read site is the diagnostic and
            // pressure lever, not the operator's control; it is cleared here so
            // the store leg is what this suite exercises.
            .env_remove("VIGIL_DETECTOR_QUEUE_CAPACITY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Held until the moment of spawn: a released port is a race another
        // process can steal before this one binds it.
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn vigil run");
        // Drain both pipes so a full one can never stall the child. Readiness
        // is proven by the control socket accepting connections, never by
        // anything printed. The stdout buffer is retained because the runtime
        // states the identity it is announcing on it, and that statement is
        // what the rendered identity has to agree with.
        let stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());
        Self { child, stdout }
    }

    /// The service identity this process is ANNOUNCING, read off the startup
    /// receipt the runtime prints. This is the identity Home Assistant sees as
    /// the device and the messaging topics are namespaced under, so it is the
    /// only thing the operator surface's identity line can honestly be
    /// compared against.
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
                "{error}. Without it there is nothing to check the rendered identity against; \
                 output so far:\n{}",
                self.stdout.lock().expect("stdout buffer")
            )
        })
    }

    /// Stop the process through the same validated path every other binary
    /// suite uses: `Child::kill` on this test's own child handle, which
    /// signals a PID the standard library owns rather than a formatted direct
    /// or negative-group target.
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

/// Wait for the runtime's control socket to APPEAR and accept a connection.
/// Never a sleep and never a printed line: the "runtime loop ready" line is
/// emitted before the listener binds, so a reader that trusts it races the
/// listener and silently falls through to the direct store read.
fn wait_for_control_socket(data_dir: &Path) {
    let socket_path = data_dir.join("control.sock");
    wait_until(
        &format!(
            "the control socket at {} to appear and accept a connection",
            socket_path.display()
        ),
        Duration::from_secs(30),
        || Ok(UnixStream::connect(&socket_path).ok().map(|_| ())),
    )
    .expect("the runtime must publish its control socket");
}

/// `vigil settings …` against `data_dir`. `VIGIL_DATA_DIR` is the bootstrap
/// location the direction document keeps in the environment (a location, not
/// behavior); it is what both the control-socket path and the default store
/// path derive from, so one variable points the command at the deployment.
fn vigil_settings(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(vigil_binary_path())
        .arg("settings")
        .args(args)
        .env("VIGIL_DATA_DIR", data_dir)
        .env_remove("VIGIL_STORE_PATH")
        .env_remove("VIGIL_CONTROL_SOCKET")
        .env_remove("VIGIL_DETECTOR_QUEUE_CAPACITY")
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

// ── Line parsing ───────────────────────────────────────────────────────────

fn lines_with_prefix<'a>(rendered: &'a str, prefix: &str) -> Vec<&'a str> {
    rendered
        .lines()
        .filter(|line| line.starts_with(&format!("{prefix} ")))
        .collect()
}

/// The value of a single-token field, e.g. `author=local-explicit`.
fn token_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// The reason runs to the end of the line, so it is read as a tail rather than
/// as a token.
fn reason_field(line: &str) -> Option<&str> {
    let needle = format!(" {REASON_KEY}=");
    let start = line.find(&needle)? + needle.len();
    Some(line[start..].trim())
}

fn setting_named<'a>(rendered: &'a str, name: &str) -> Option<&'a str> {
    lines_with_prefix(rendered, SETTING_LINE_PREFIX)
        .into_iter()
        .find(|line| token_field(line, NAME_KEY) == Some(name))
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// Unfakeable because it asserts over EVERY rendered setting line rather than
/// over a chosen sample, and requires all six fields on each one: a surface can
/// pass only by carrying value, control state, author, surface, scope and a
/// non-empty reason for every setting it renders. Rendering the six for one
/// setting, or leaving `reason` blank on the automatic records (the exact
/// defect the direction document calls out — an automatic record owes a reason
/// naming its derivation input, and a blank field is a defect), fails it.
#[test]
fn listing_reports_value_state_author_surface_scope_and_reason_for_every_touched_setting() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = tmp.path().join("vigil.toml");
    fs::write(&config_path, "cameras = []\n").expect("write config");

    let live = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_control_socket(&data_dir);

    let output = vigil_settings(&data_dir, &[]);
    live.stop();

    assert!(
        output.status.success(),
        "`vigil settings` must exit 0 while the runtime owns the store, stderr:\n{}",
        stderr_of(&output)
    );
    let rendered = stdout_of(&output);

    let setting_lines = lines_with_prefix(&rendered, SETTING_LINE_PREFIX);
    assert!(
        !setting_lines.is_empty(),
        "the operator surface must render at least one `{SETTING_LINE_PREFIX}` line; got:\n{rendered}"
    );

    for line in &setting_lines {
        let name = token_field(line, NAME_KEY).unwrap_or_else(|| {
            panic!("every `{SETTING_LINE_PREFIX}` line must name its setting; got: {line}")
        });
        assert!(
            !name.is_empty(),
            "a `{SETTING_LINE_PREFIX}` line's name must not be empty; got: {line}"
        );
        for field in REQUIRED_TOKEN_FIELDS {
            let value = token_field(line, field).unwrap_or_else(|| {
                panic!(
                    "criterion 2 requires every setting to show its value, control state, author, \
                     surface, scope and reason — `{name}` is missing `{field}=`; got: {line}"
                )
            });
            assert!(
                !value.is_empty(),
                "`{name}` renders `{field}=` with nothing after it; an empty field is not a \
                 reported field; got: {line}"
            );
        }
        let reason = reason_field(line)
            .unwrap_or_else(|| panic!("`{name}` is missing `{REASON_KEY}=`; got: {line}"));
        assert!(
            !reason.is_empty(),
            "`{name}` renders an empty reason. Every record owes a reason — an automatic record \
             names its derivation input — and a record with no reason is a defect, not a blank \
             field; got: {line}"
        );
    }

    for name in SETTINGS_THIS_ARC_TOUCHES {
        assert!(
            setting_named(&rendered, name).is_some(),
            "the listing must carry a line for `{name}`, one of the settings this arc touches — \
             criterion 2 is about EVERY touched setting, not a sample; got:\n{rendered}"
        );
    }
}

/// Unfakeable because it forces requested and running APART before reading
/// them: a pin written into the store while the process is up is authoritative
/// immediately, but a value the runtime can only take on at startup is not yet
/// what runs. A surface that reports one value under three names, or that
/// echoes the pin back as `running`, cannot satisfy this — and reporting a
/// pending value as effective is precisely the confusion the model exists to
/// remove.
#[test]
fn listing_reports_requested_running_and_pending_side_by_side() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = tmp.path().join("vigil.toml");
    fs::write(&config_path, "cameras = []\n").expect("write config");

    let live = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_control_socket(&data_dir);

    // The detector queue capacity has TWO ways in, and they are not the same
    // thing. `VIGIL_DETECTOR_QUEUE_CAPACITY` stays exactly where it is: a
    // deterministic diagnostic and pressure lever, read at the environment read
    // site so a test or an operator chasing a backlog can force a size for one
    // process. OPERATOR CONTROL of the capacity goes through the STORE, which is
    // the leg this test — and criterion 2 — is about. So the environment
    // variable is removed from every process this test starts, and nothing here
    // asserts anything about it; the pin below is the store leg alone.
    //
    // 2000 is deliberate: it is the working queue capacity a narrowed range
    // once refused, the install that produced the configurability ruling. It is
    // also four digits, so a substring check cannot match it by accident.
    let pinned = vigil_settings(&data_dir, &["set", DETECTOR_QUEUE_CAPACITY_SETTING, "2000"]);
    assert!(
        pinned.status.success(),
        "pinning an ungoverned setting through `vigil settings set` must succeed while the \
         runtime owns the store — no domain governs the rate controls today, so no disabling \
         step exists to take; stderr:\n{}",
        stderr_of(&pinned)
    );

    let output = vigil_settings(&data_dir, &[]);
    live.stop();

    assert!(
        output.status.success(),
        "`vigil settings` must exit 0, stderr:\n{}",
        stderr_of(&output)
    );
    let rendered = stdout_of(&output);

    for line in lines_with_prefix(&rendered, SETTING_LINE_PREFIX) {
        let name = token_field(line, NAME_KEY).unwrap_or("<unnamed>");
        for field in SIDE_BY_SIDE_FIELDS {
            assert!(
                token_field(line, field).is_some_and(|value| !value.is_empty()),
                "requested, running and pending are answered side by side for EVERY setting; \
                 `{name}` is missing `{field}=`; got: {line}"
            );
        }
    }

    let queue_line = setting_named(&rendered, DETECTOR_QUEUE_CAPACITY_SETTING)
        .unwrap_or_else(|| panic!("no {DETECTOR_QUEUE_CAPACITY_SETTING} line in:\n{rendered}"));
    let requested = token_field(queue_line, REQUESTED_KEY).expect("requested field");
    let running = token_field(queue_line, RUNNING_KEY).expect("running field");
    let pending = token_field(queue_line, PENDING_KEY).expect("pending field");

    assert_eq!(
        requested, "2000",
        "the pin is authoritative the moment it is recorded, so requested must be the value just \
         written; got: {queue_line}"
    );
    assert_ne!(
        running, requested,
        "the queue is sized when the runtime starts, so a pin written mid-run cannot already be \
         what runs. A surface reporting requested and running as the same value is reporting a \
         pending value as effective; got: {queue_line}"
    );
    // The three spellings come from `PendingCause::as_str()`, the single source
    // for what closes a requested/running gap, rather than being retyped here.
    let pending_causes = [
        PendingCause::Restart.as_str(),
        PendingCause::CameraReconnect.as_str(),
        PendingCause::ModelReload.as_str(),
    ];
    assert!(
        pending_causes.contains(&pending),
        "when requested and running differ, pending must name what closes the gap — one of \
         {pending_causes:?} — never `none` and never a blank; got: {queue_line}"
    );
}

/// Unfakeable because the runtime is fully stopped before the question is
/// asked, and the pin was made through a surface the stopped process cannot
/// recompute from: the answer can only come from the store. The closed run's
/// behavior — answering with the LAST RUN's resolution, so an offline query
/// described the past — is what this pins against, and the `running=none`
/// assertion is what stops a stale resolution being re-served as if it were
/// live.
#[test]
fn listing_answers_requested_from_the_store_while_the_runtime_is_stopped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = tmp.path().join("vigil.toml");
    fs::write(&config_path, "cameras = []\n").expect("write config");

    {
        let live = LiveVigil::spawn(&config_path, &data_dir);
        wait_for_control_socket(&data_dir);
        // Two digits deliberately, and not the product default: a substring or
        // character-level match cannot land on "23" by accident the way it can
        // on a bare "7".
        let pinned = vigil_settings(&data_dir, &["set", DETECTOR_SAMPLE_FRAMES_SETTING, "23"]);
        assert!(
            pinned.status.success(),
            "pinning {DETECTOR_SAMPLE_FRAMES_SETTING} must succeed, stderr:\n{}",
            stderr_of(&pinned)
        );
        live.stop();
    }

    let socket_path = data_dir.join("control.sock");
    assert!(
        UnixStream::connect(&socket_path).is_err(),
        "the runtime must be genuinely stopped before the offline read, or this test proves \
         nothing about answering from the store"
    );

    let output = vigil_settings(&data_dir, &[]);
    assert!(
        output.status.success(),
        "the requested value must be answerable at any time, running or stopped; \
         `vigil settings` exited {:?}, stderr:\n{}",
        output.status.code(),
        stderr_of(&output)
    );
    let rendered = stdout_of(&output);

    let line = setting_named(&rendered, DETECTOR_SAMPLE_FRAMES_SETTING)
        .unwrap_or_else(|| panic!("no {DETECTOR_SAMPLE_FRAMES_SETTING} line in:\n{rendered}"));
    assert_eq!(
        token_field(line, REQUESTED_KEY),
        Some("23"),
        "the stopped deployment must answer the requested value from the store; got: {line}"
    );
    assert_eq!(
        token_field(line, RUNNING_KEY),
        Some("none"),
        "with no runtime up, nothing is running — the value must be labelled requested rather \
         than reported as running; got: {line}"
    );
    assert_eq!(
        token_field(line, AUTHOR_KEY),
        Some(Author::LocalExplicit.as_str()),
        "a `vigil settings` change is a human at this deployment, the top of the ranking; \
         got: {line}"
    );
}

/// Unfakeable because it asserts both halves of "read-only" AND ties the value
/// to the running process: the identity is rendered with its derivation, it
/// EQUALS the identity the runtime is announcing, and an ordinary settings edit
/// aimed at it is refused with the orphaned-history explanation. A surface that
/// prints the identifier while quietly accepting `settings set` on it has not
/// made it read-only; one that refuses everything without ever showing the
/// derivation has not made it visible; and one that renders any non-empty
/// string it can reach — the data directory's own name, say — has made it
/// visible and wrong, which is worse than absent on a line an operator is meant
/// to trust.
#[test]
fn listing_shows_the_service_identifier_read_only_with_how_it_was_arrived_at() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = tmp.path().join("vigil.toml");
    fs::write(&config_path, "cameras = []\nsite_name = \"home farm\"\n").expect("write config");

    let live = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_control_socket(&data_dir);

    let announced = live.announced_service_id();
    let output = vigil_settings(&data_dir, &[]);
    let identity_edit = vigil_settings(
        &data_dir,
        &["set", SERVICE_IDENTITY_SETTING, "renamed-node"],
    );
    live.stop();

    assert!(
        output.status.success(),
        "`vigil settings` must exit 0, stderr:\n{}",
        stderr_of(&output)
    );
    let rendered = stdout_of(&output);

    let identity_lines = lines_with_prefix(&rendered, IDENTITY_LINE_PREFIX);
    assert_eq!(
        identity_lines.len(),
        1,
        "the operator surface carries exactly one `{IDENTITY_LINE_PREFIX}` line — this node's \
         service identifier; got:\n{rendered}"
    );
    let identity_line = identity_lines[0];

    let value = token_field(identity_line, VALUE_KEY).unwrap_or_else(|| {
        panic!("the identity line must carry the identifier in force; got: {identity_line}")
    });
    assert!(
        !value.is_empty(),
        "the identity line must carry a non-empty identifier; got: {identity_line}"
    );
    assert_eq!(
        value, announced,
        "the identity line must carry THIS NODE'S identifier — the one the running process is \
         announcing as the Home Assistant device and the messaging topic namespace. A surface \
         that renders some other string is not showing the operator a value they can act on; it \
         is stating a falsehood on the one line whose entire purpose is that the value deciding \
         where their history lives is never invisible. Rendered {value:?} against an announced \
         {announced:?}; got: {identity_line}"
    );

    let derivation = token_field(identity_line, "derivation").unwrap_or_else(|| {
        panic!(
            "the identity is shown WITH how it was arrived at — derived at first start, set \
             explicitly, or derived for this run only; got: {identity_line}"
        )
    });
    assert!(
        [
            "derived-at-first-start",
            "set-explicitly",
            "derived-not-yet-persisted"
        ]
        .contains(&derivation),
        "the derivation must be one of the three the model knows; got: {identity_line}"
    );
    assert_eq!(
        token_field(identity_line, "access"),
        Some("read-only"),
        "the identifier is shown READ-ONLY: the surface itself must say so, because it names the \
         Home Assistant device and the messaging topics; got: {identity_line}"
    );

    assert!(
        setting_named(&rendered, SERVICE_IDENTITY_SETTING).is_none(),
        "the identity must not also be rendered as an ordinary `{SETTING_LINE_PREFIX}` line: that \
         is the line shape the generic set/reset path operates on, and offering it there is \
         offering a write path for a read-only value; got:\n{rendered}"
    );

    assert!(
        !identity_edit.status.success(),
        "an ordinary settings edit aimed at the service identity must be REFUSED — the surface \
         offers no write path for it — but `vigil settings set service_identity` exited 0; \
         stdout:\n{}",
        stdout_of(&identity_edit)
    );
    let refusal = format!("{}{}", stdout_of(&identity_edit), stderr_of(&identity_edit));
    let refusal_lower = refusal.to_ascii_lowercase();
    assert!(
        refusal_lower.contains("device"),
        "the refusal must carry the same explanation the deliberate change operation states — \
         Home Assistant sees a NEW DEVICE; got:\n{refusal}"
    );
    assert!(
        refusal_lower.contains("history"),
        "the refusal must say the history attached to the old identity is orphaned; got:\n{refusal}"
    );
}
