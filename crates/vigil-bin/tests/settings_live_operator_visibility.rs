//! `vigil settings` is the operator surface for asking what a deployment is
//! running at — while it is running, while it is stopped, and before it has
//! ever started — and asking is never the thing that brings a store into
//! existence.
//!
//! It is answered by the same dispatch `events`/`why`/`stats` are answered by
//! (`crates/vigil/src/lib.rs`, `print_settings` beside `print_control_or_direct`):
//! ask the process that owns this deployment's store, through Context Graph's
//! authenticated owner channel, and read the store directly only when no
//! runtime is holding it. There is no Vigil control socket, and no HTTP
//! settings route.
//!
//! The four behaviors below are the two legs of that dispatch, the persistence
//! witness and the read-purity witness. Each drives two real, separate
//! operating-system processes contending for one store file, which is the only
//! way to exercise inter-process store ownership; the projection they assert
//! against is the field set `settings_projection` renders.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use vigil::settings_model::{Author, DETECTOR_SAMPLE_FRAMES_SETTING, Surface};
use vigil::settings_projection::{
    AUTHOR_KEY, CONTROL_STATE_KEY, REASON_KEY, SCOPE_KEY, SETTING_LINE_PREFIX, SURFACE_KEY,
};
use vigil::settings_store::SettingsStore;

/// The effective-value field key. `settings_projection` declares the control,
/// author, surface, scope and reason keys but not this one, so the spelling is
/// retyped here deliberately; only the implementation can fix that by declaring
/// it alongside the others.
const VALUE_KEY: &str = "value";

/// The setting-name field key. Same situation as `VALUE_KEY`.
const NAME_KEY: &str = "name";

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_for_store_owner,
    wait_for_store_owner_to_release,
};

/// The prefix `live_read::handle_owner_request` puts on an answer served by the
/// running owner. Its presence is what distinguishes the owner-served answer
/// from the direct store read; both must render the same projection beneath it.
/// Read from the one place that writes it rather than retyped, so a test never
/// freezes the name of the road the answer travelled.
fn owner_served_marker() -> &'static str {
    vigil::OWNER_SERVED_PREFIX.trim_end_matches('\n')
}

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
        // Drain both pipes so a full one can never stall the child; readiness
        // is proven by the owner route answering, never by anything printed.
        let _stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());
        Self { child }
    }

    /// The validated stop: `Child::kill` against this test's own child handle,
    /// so the PID belongs to the standard library and nothing here formats a
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

fn vigil_settings(data_dir: &Path) -> Output {
    Command::new(vigil_binary_path())
        .arg("settings")
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

fn setting_lines(rendered: &str) -> Vec<&str> {
    rendered
        .lines()
        .filter(|line| line.starts_with(&format!("{SETTING_LINE_PREFIX} ")))
        .collect()
}

fn token_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(&rest[..end])
}

fn setting_named<'a>(rendered: &'a str, name: &str) -> Option<&'a str> {
    setting_lines(rendered)
        .into_iter()
        .find(|line| token_field(line, NAME_KEY) == Some(name))
}

/// Unfakeable because a second OS process genuinely cannot open a store the
/// running owner holds: the answer can only come from the owner over that
/// store's own owner channel, and the owner marker proves which path served it.
/// This is the ordinary case — an operator checking settings on a running
/// system — and the defect it holds shut is a settings command that skips the
/// dispatch every other read-only surface already uses and fails exactly here.
#[test]
fn listing_answers_while_the_runtime_owns_the_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = tmp.path().join("vigil.toml");
    fs::write(&config_path, "cameras = []\n").expect("write config");

    let live = LiveVigil::spawn(&config_path, &data_dir);
    wait_for_the_store_owner(&data_dir);

    let output = vigil_settings(&data_dir);
    live.stop();

    assert!(
        output.status.success(),
        "`vigil settings` must succeed while `vigil run` owns the store — an operator must be \
         able to inspect settings while Vigil is running; exited {:?}, stderr:\n{}",
        output.status.code(),
        stderr_of(&output)
    );
    let rendered = stdout_of(&output);
    assert!(
        rendered.contains(owner_served_marker()),
        "reading settings while the runtime owns the store must be served by the OWNER over that \
         store's own owner channel, not by a direct-read fallback that would hit the store lock; \
         got:\n{rendered}"
    );

    let lines = setting_lines(&rendered);
    assert!(
        !lines.is_empty(),
        "the owner's answer must carry the real settings report, not just the transport marker; \
         got:\n{rendered}"
    );
    for line in lines {
        let name = token_field(line, NAME_KEY).unwrap_or("<unnamed>");
        for field in [
            VALUE_KEY,
            CONTROL_STATE_KEY,
            AUTHOR_KEY,
            SURFACE_KEY,
            SCOPE_KEY,
        ] {
            assert!(
                token_field(line, field).is_some_and(|value| !value.is_empty()),
                "the owner-served answer renders the same projection as a direct read — \
                 `{name}` is missing `{field}=`; got: {line}"
            );
        }
        assert!(
            line.contains(&format!(" {REASON_KEY}=")),
            "`{name}` is missing `{REASON_KEY}=`; got: {line}"
        );
    }
}

/// Unfakeable because the owner is gone before the question is asked and the
/// store is proven unowned first, so the answer cannot have come over the owner
/// channel — and
/// the absence of the owner marker is asserted, not assumed. This is the
/// fallback leg of the dispatch: same command, same projection, different path.
#[test]
fn listing_answers_by_direct_read_when_no_runtime_owns_the_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = tmp.path().join("vigil.toml");
    fs::write(&config_path, "cameras = []\n").expect("write config");

    {
        let live = LiveVigil::spawn(&config_path, &data_dir);
        wait_for_the_store_owner(&data_dir);
        live.stop();
    }
    wait_for_the_store_to_be_free(&data_dir);

    let store_path = data_dir.join("store.contextgraph");
    assert!(
        store_path.exists(),
        "the stopped run must have left its store on disk at {}",
        store_path.display()
    );

    let output = vigil_settings(&data_dir);
    assert!(
        output.status.success(),
        "`vigil settings` must succeed against an existing, unowned store via the direct read; \
         stderr:\n{}",
        stderr_of(&output)
    );
    let rendered = stdout_of(&output);
    assert!(
        !rendered.contains(owner_served_marker()),
        "with no runtime owning the store, the answer cannot have been served by an owner — a \
         surface still claiming it was is lying about where its answer came from; got:\n{rendered}"
    );
    assert!(
        !setting_lines(&rendered).is_empty(),
        "the direct read must render the settings report; got:\n{rendered}"
    );
}

/// Unfakeable because the value asserted is one a REAL run wrote into the store
/// from a real surface, and it is deliberately not the product default: a
/// listing that recomputed the answer in this second process could coincide
/// with the number, but not with the author and surface that recorded it. That
/// is the persistence witness — the settings tables could be silently absent
/// and a recompute-only listing would still look right.
#[test]
fn listing_after_the_run_stops_serves_what_was_persisted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let config_path = tmp.path().join("vigil.toml");
    // Two digits deliberately: a single-digit fixture can be matched by a
    // substring or character check that never actually read the field.
    fs::write(&config_path, "cameras = []\ndetector_sample_frames = 16\n").expect("write config");

    {
        let live = LiveVigil::spawn(&config_path, &data_dir);
        wait_for_the_store_owner(&data_dir);
        live.stop();
    }
    wait_for_the_store_to_be_free(&data_dir);

    // The second process is given the data directory and nothing else: it does
    // not see the config file, so it cannot re-derive the file's assertion.
    let output = vigil_settings(&data_dir);
    assert!(
        output.status.success(),
        "`vigil settings` must succeed reading the stopped run's store, stderr:\n{}",
        stderr_of(&output)
    );
    let rendered = stdout_of(&output);

    let line = setting_named(&rendered, DETECTOR_SAMPLE_FRAMES_SETTING)
        .unwrap_or_else(|| panic!("no {DETECTOR_SAMPLE_FRAMES_SETTING} line in:\n{rendered}"));
    assert_eq!(
        token_field(line, VALUE_KEY),
        Some("16"),
        "the listing must serve what the stopped run recorded from its config file, not a bare \
         product default; got: {line}"
    );
    assert_eq!(
        token_field(line, AUTHOR_KEY),
        Some(Author::LocalExplicit.as_str()),
        "a value a human put in the config file is authored at the local-explicit rank; got: {line}"
    );
    assert_eq!(
        token_field(line, SURFACE_KEY),
        Some(Surface::ConfigFile.as_str()),
        "the record must name the surface it came from — this is the fact a recompute cannot \
         invent, and the fact that stops a later `vigil settings` change being reverted by the \
         file at every boot; got: {line}"
    );
}

/// Unfakeable because it checks the filesystem after the read: asking what a
/// never-started deployment would run at must be purely a READ. A listing that
/// opens its store to answer creates one on a fresh path, so merely asking the
/// question would leave a store on disk that nobody chose to create — and the
/// answer must still be a real report, so an implementation cannot pass by
/// refusing to answer at all.
#[test]
fn listing_an_unstarted_deployment_leaves_no_store_on_disk() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let store_path = data_dir.join("store.contextgraph");
    assert!(
        !store_path.exists(),
        "sanity: nothing must exist at the store path before Vigil has ever run"
    );

    let output = vigil_settings(&data_dir);
    assert!(
        output.status.success(),
        "`vigil settings` must exit 0 against a data directory Vigil has never run in, \
         stderr:\n{}",
        stderr_of(&output)
    );
    let rendered = stdout_of(&output);

    let lines = setting_lines(&rendered);
    assert!(
        !lines.is_empty(),
        "a never-started deployment must still report what it would run at; got:\n{rendered}"
    );
    for line in &lines {
        let name = token_field(line, NAME_KEY).unwrap_or("<unnamed>");
        assert_eq!(
            token_field(line, CONTROL_STATE_KEY),
            // The `control=` token spelling has no single source:
            // `ControlState::label()` renders the operator PHRASE ("Automatic"),
            // not the whitespace-free token a field carries. Only the
            // implementation can fix that by exporting the token spelling.
            Some("automatic"),
            "with no records above automatic, every setting reads Automatic — `{name}` did not; \
             got: {line}"
        );
        assert_eq!(
            token_field(line, AUTHOR_KEY),
            Some(Author::Automatic.as_str()),
            "`{name}` must be attributed to Vigil's own choice on a deployment nobody has \
             configured; got: {line}"
        );
    }

    assert!(
        !store_path.exists(),
        "asking `vigil settings` what a never-started deployment would run at must not \
         materialize a store on disk — found one at {}",
        store_path.display()
    );
    let stray: Vec<String> = fs::read_dir(&data_dir)
        .expect("read data dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    assert!(
        stray.is_empty(),
        "a read must leave the data directory untouched; found {stray:?}"
    );
}
