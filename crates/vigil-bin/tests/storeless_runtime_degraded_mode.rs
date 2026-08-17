//! Criterion 10: an unreadable store keeps the property watched, and says
//! exactly what it lost.
//!
//! A camera system going blind because a settings database is unreadable is a
//! worse outcome than one running on its files and shouting about it. So with
//! the store file corrupt, Vigil starts anyway: live view, detection and broker
//! alerting keep working; delivery is reported best-effort rather than assured,
//! because the durable path is exactly what is missing; recording, review
//! history, corrections and settings changes are unavailable and each says so
//! when attempted; the unmanaged state is restated on every operator surface
//! for as long as it lasts; and the run writes nothing at all, including the
//! persisted service identity.
//!
//! This is NEW capability rather than a described fallback: today everything —
//! camera startup, the review service, the detection channel, broker setup — is
//! built inside the branch taken when the store opens
//! (`crates/vigil/src/runtime.rs:232`), and the branch for a failed open starts
//! nothing (`:421`). Every test here therefore fails on `dev`.
//!
//! The store is made genuinely unreadable — a real file holding bytes that are
//! not a store, with its permission bits cleared — never merely absent: an
//! absent store is a first start on a new machine, which Vigil creates rather
//! than degrades over, so a missing file would prove the wrong branch.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use vigil::settings_command::SETTINGS_ERROR_PREFIX;
use vigil::settings_degraded::{
    CAPABILITY_REFUSAL_PREFIX, DegradedCapability, DeliveryAssurance, UnavailableCapability,
};
use vigil::settings_projection::UNMANAGED_LINE_PREFIX;
use vigil::settings_store::SettingsStore;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RtspFixture, TcpPortReservation, capture_pipe, get, request, rtsp_fixture_lock, toml_path,
    vigil_binary_path, wait_for_tcp_port, wait_until, workspace_root,
};

const FIRST_CAMERA_NAME: &str = "loading dock";
const SECOND_CAMERA_NAME: &str = "north fence";

/// The three capabilities the degraded contract keeps, as the unmanaged line
/// names them.
const RUNNING_CAPABILITIES: [&str; 3] = ["live-view", "detection", "broker-alerting"];

/// The four it loses, as the unmanaged line names them.
const UNAVAILABLE_CAPABILITIES: [&str; 4] = [
    "recording",
    "review-history",
    "corrections",
    "settings-changes",
];

/// The repo's real detector-model fixture (the same raw yolox-tiny checkpoint
/// `crates/vigil-bin/tests/cameraless_worker.rs` stages), passed to every
/// degraded run here via `--detector-model-path`. Without it, `detector
/// model load failed` (an absent model path) latches `IngestFailed` on every
/// camera within milliseconds of startup — before any test can observe the
/// genuinely functional `RunningUnmanaged`/200 phase a storeless run answers
/// while its cameras are still coming up. Staging a real, loadable model
/// removes that self-inflicted race, so each camera's condition here comes
/// from what its OWN configured stream actually does (never connects, for
/// the unroutable TEST-NET-1 fixtures; refused immediately, for
/// `refused_camera_url()`), not from a model nobody configured.
fn fixture_detector_model_path() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("fixtures")
        .join("models")
        .join("yolox-tiny-coco.pth")
}

// ── The degraded deployment ────────────────────────────────────────────────

struct DegradedDeployment {
    /// The audited tree. Everything the write audit hashes lives under here,
    /// including the data directory and anywhere a service-identity sidecar
    /// beside it would land.
    tmp: tempfile::TempDir,
    /// Holds the control socket, outside the audited tree entirely.
    _socket_tmp: tempfile::TempDir,
    data_dir: PathBuf,
    config_path: PathBuf,
    socket_path: PathBuf,
    store_path: PathBuf,
}

impl DegradedDeployment {
    /// A data directory holding a store file that exists and cannot be opened.
    ///
    /// The control socket is deliberately placed in a SEPARATE temporary
    /// directory, outside the audited tree, so the write audit is about what
    /// Vigil recorded rather than about a transport endpoint the runtime needs
    /// in order to answer at all — while the audit itself still covers the whole
    /// directory the data directory sits in, because the identity sidecar lives
    /// beside the data directory rather than inside it.
    fn prepare() -> Self {
        // Well-formed but unreachable RTSP URLs on the closed TEST-NET-1 block:
        // the camera stack must come up from the configuration surfaces alone,
        // with no store beneath it and without any stream ever connecting.
        Self::prepare_with_camera_urls(&[
            (FIRST_CAMERA_NAME, "rtsp://192.0.2.10:554/stream"),
            (SECOND_CAMERA_NAME, "rtsp://192.0.2.11:554/stream"),
        ])
    }

    /// The same degraded deployment against named camera streams, so a test can
    /// ask what a degraded run does when a stream actually connects — or when
    /// one is refused outright rather than left hanging.
    fn prepare_with_camera_urls(cameras: &[(&str, &str)]) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket_tmp = tempfile::tempdir().expect("tempdir for the control socket");
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&data_dir).expect("data dir");

        let store_path = data_dir.join("store.contextgraph");
        fs::write(
            &store_path,
            b"this file exists and is not a store: a corrupt volume, not a first start\n",
        )
        .expect("write corrupt store file");
        // Clear the permission bits too, so the file is unreadable by content
        // AND by access — whichever the storage layer notices first, the class
        // is Unreadable rather than Absent.
        fs::set_permissions(&store_path, fs::Permissions::from_mode(0o000))
            .expect("clear store permissions");

        let config_path = tmp.path().join("vigil.toml");
        let mut config = String::from("site_name = \"home farm\"\n");
        for (name, url) in cameras {
            config.push_str(&format!(
                "\n[[cameras]]\nname = \"{name}\"\nrtsp_url = \"{url}\"\n"
            ));
        }
        fs::write(&config_path, config).expect("write the camera config");

        let socket_path = socket_tmp.path().join("control.sock");
        Self {
            tmp,
            _socket_tmp: socket_tmp,
            data_dir,
            config_path,
            socket_path,
            store_path,
        }
    }

    fn start(&self) -> DegradedRun {
        self.start_with_service_id(None)
    }

    /// The same degraded start, with `VIGIL_SERVICE_ID` set when a caller
    /// wants to observe how a run with no store behind it treats a
    /// deployment-configured identifier rather than one it derives.
    fn start_with_service_id(&self, service_id: Option<&str>) -> DegradedRun {
        let health = TcpPortReservation::reserve_loopback().expect("reserve health port");
        let review = TcpPortReservation::reserve_loopback().expect("reserve review port");
        let health_port = health.port();
        let review_port = review.port();

        let model_path = fixture_detector_model_path();
        assert!(
            model_path.is_file(),
            "the repo's real detector-model fixture must be present so a degraded run's cameras \
             can actually load a detector: {}",
            model_path.display()
        );

        let mut command = Command::new(vigil_binary_path());
        command
            .arg("run")
            .arg("--config")
            .arg(&self.config_path)
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--health-port")
            .arg(health_port.to_string())
            .arg("--review-port")
            .arg(review_port.to_string())
            .arg("--detector-model-path")
            .arg(&model_path)
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env("VIGIL_CONTROL_SOCKET", &self.socket_path)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_FABRIC_TICKET")
            .env_remove("VIGIL_FABRIC_HUB")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match service_id {
            Some(service_id) => {
                command.env("VIGIL_SERVICE_ID", service_id);
            }
            None => {
                command.env_remove("VIGIL_SERVICE_ID");
            }
        }
        // Held until the moment of spawn: a released port is a race.
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn vigil run");
        let stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());

        let run = DegradedRun {
            child,
            health_port,
            review_port,
            stdout,
        };

        // Readiness is the control socket APPEARING and accepting a
        // connection. Never a sleep and never a printed line: a degraded run
        // that only printed a warning and started nothing would satisfy a
        // log-line wait while failing the contract entirely.
        wait_until(
            &format!(
                "the degraded runtime's control socket at {} to appear",
                self.socket_path.display()
            ),
            Duration::from_secs(30),
            || Ok(UnixStream::connect(&self.socket_path).ok().map(|_| ())),
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}. An unreadable store must not stop Vigil starting — the storeless \
                 runtime path must come up and serve its operator surface. Process output so \
                 far:\n{}",
                run.stdout()
            )
        });

        run
    }

    /// The storage engine's own advisory PID lock file, sitting beside the
    /// store file (`<store>.lock`). The engine deliberately never unlinks it —
    /// the kernel's exclusive lock releases when the holding process exits, and
    /// leaving the file in place avoids an unlink race with a concurrent
    /// opener — so every open attempt against this store, including one that
    /// goes on to fail, leaves this one file newly present. It is expected
    /// open mechanics, not a write the degraded-run promise is about, so the
    /// write audits below permit exactly this one path and nothing else.
    fn advisory_lock_path(&self) -> PathBuf {
        self.store_path.with_extension("lock")
    }

    /// A `vigil` command aimed at this degraded deployment.
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(vigil_binary_path())
            .args(args)
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env("VIGIL_CONTROL_SOCKET", &self.socket_path)
            .env_remove("VIGIL_STORE_PATH")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("spawn vigil {args:?}: {error}"))
    }
}

struct DegradedRun {
    child: Child,
    health_port: u16,
    review_port: u16,
    stdout: std::sync::Arc<std::sync::Mutex<String>>,
}

impl DegradedRun {
    fn stdout(&self) -> String {
        self.stdout.lock().expect("stdout lock").clone()
    }

    /// The validated stop: `Child::kill` on this test's own child handle. The
    /// standard library owns the PID, so nothing here formats a direct or
    /// negative process-group signal target.
    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DegradedRun {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── Surfaces ───────────────────────────────────────────────────────────────

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The one `unmanaged` line, as rendered by the single projection every
/// operator surface goes through.
fn unmanaged_line(rendered: &str) -> Option<&str> {
    rendered
        .lines()
        .find(|line| line.starts_with(&format!("{UNMANAGED_LINE_PREFIX} ")))
}

fn token_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(&rest[..end])
}

fn settings_surface(deployment: &DegradedDeployment) -> String {
    let output = deployment.cli(&["settings"]);
    assert!(
        output.status.success(),
        "the operator surface must answer while the run is degraded — that is where the \
         unmanaged statement lives; `vigil settings` exited {:?}, output:\n{}",
        output.status.code(),
        combined(&output)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// The `/health` body carries JSON on its first line, with plain-text receipt
/// lines after it, so the structured half is parsed from the first line only.
fn health_first_line_json(body: &str) -> Value {
    let first_line = body.lines().next().unwrap_or_default();
    serde_json::from_str(first_line).unwrap_or_else(|error| {
        panic!("first line of /health must be JSON: {error}; body:\n{body}")
    })
}

/// The general-purpose `/health` read used by tests that are not themselves
/// about the liveness-code/status relationship (that relationship, across
/// every recorded status including a later independent failure, is proven by
/// `the_liveness_answer_stays_protected_while_the_body_names_the_real_condition`
/// below, which drives the status transition itself rather than mirroring
/// `health.rs`'s code table). A storeless run with no fault injected — every
/// call site of this helper — has nothing to make it anything OTHER than
/// `running_unmanaged`, so this asserts against that ONE fixed literal
/// expectation rather than a table that would have to (and, before this fix,
/// wrongly did) also accept `ready`: a functioning storeless run reporting
/// `ready` is exactly the defect this contract exists to prevent (health.rs's
/// own doc comment on `RunningUnmanaged` — "a box that cannot remember
/// anything must never present as an ordinary box").
fn health_surface(port: u16) -> String {
    let response = get(port, "/health");
    let body = response.body_text();
    let status = health_first_line_json(&body)
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        status, "running_unmanaged",
        "a storeless run with no fault injected must report running_unmanaged, never `ready` or \
         any other status — a functioning box that cannot remember anything must never present as \
         an ordinary box; body:\n{body}"
    );
    assert_eq!(
        response.status, 200,
        "running_unmanaged is a live, functional status and must answer the watchdog 2xx; \
         body:\n{body}"
    );
    assert!(
        unmanaged_line(&body).is_some(),
        "the standing unmanaged statement belongs on every /health answer while this run lasts; \
         body:\n{body}"
    );
    body
}

// ── The write audit ────────────────────────────────────────────────────────

/// Every regular file under `root`, mapped to the SHA-256 of its contents.
/// Sockets and directories are skipped: a transport endpoint is not a record,
/// and this deployment keeps its socket outside the data directory anyway.
fn snapshot_regular_files(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => panic!("read {}: {error}", directory.display()),
        };
        for entry in entries {
            let entry = entry.expect("directory entry");
            let path = entry.path();
            let file_type = entry.file_type().expect("file type");
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            // A file whose permissions were deliberately cleared still counts:
            // it is hashed by whatever can be read, and a run that changed it
            // would have had to make it readable first, which is itself a
            // write.
            let bytes = fs::read(&path).unwrap_or_default();
            let digest = Sha256::digest(&bytes);
            found.insert(path, format!("{digest:x}:{}", bytes.len()));
        }
    }
    found
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// Unfakeable because the store file genuinely cannot be opened, so nothing on
/// the store-backed path can produce any of this: the process must reach a
/// serving state, the two configured cameras must be counted from the
/// configuration surfaces alone, and the operator surface must name live view,
/// detection and broker alerting as the capabilities that came up. On `dev` the
/// failed-open branch starts nothing at all, so the control-socket wait alone
/// already fails.
#[test]
fn an_unreadable_store_still_starts_cameras_detection_and_broker_alerting() {
    let deployment = DegradedDeployment::prepare();
    let run = deployment.start();

    let health_body = health_surface(run.health_port);
    let rendered = settings_surface(&deployment);
    let logs = run.stdout();
    run.stop();

    let body = health_first_line_json(&health_body);
    let cameras_by_kind = body
        .get("cameras_by_kind")
        .and_then(Value::as_object)
        .unwrap_or_else(|| {
            panic!("the degraded node's /health must still report cameras_by_kind; body:\n{health_body}")
        });
    let rtsp = cameras_by_kind.get("rtsp").unwrap_or_else(|| {
        panic!("cameras_by_kind must carry an rtsp entry; body:\n{health_body}")
    });
    assert_eq!(
        rtsp.get("count").and_then(Value::as_u64),
        Some(2),
        "both configured cameras must come up with no store beneath them — the camera stack \
         resolves from the configuration surfaces in the degraded path; body:\n{health_body}\n\
         logs:\n{logs}"
    );
    let listed = rtsp
        .get("cameras")
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            panic!("cameras_by_kind.rtsp.cameras must be a list; body:\n{health_body}")
        });
    let names: Vec<String> = listed
        .iter()
        .filter_map(|entry| entry.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    for expected in [FIRST_CAMERA_NAME, SECOND_CAMERA_NAME] {
        assert!(
            names.iter().any(|name| name == expected),
            "the degraded run must be watching \"{expected}\"; got {names:?}"
        );
    }

    let line = unmanaged_line(&rendered).unwrap_or_else(|| {
        panic!(
            "the degraded operator surface must carry an `{UNMANAGED_LINE_PREFIX}` line; \
             got:\n{rendered}"
        )
    });
    let running = token_field(line, "running").unwrap_or_else(|| {
        panic!("the unmanaged line must name the capabilities that kept working; got: {line}")
    });
    for capability in RUNNING_CAPABILITIES {
        assert!(
            running.split(',').any(|entry| entry == capability),
            "the degraded contract keeps live view, detection and broker alerting — `{capability}` \
             is missing from `running=`; got: {line}"
        );
    }
    let unavailable = token_field(line, "unavailable")
        .unwrap_or_else(|| panic!("the unmanaged line must name what is unavailable; got: {line}"));
    for capability in UNAVAILABLE_CAPABILITIES {
        assert!(
            unavailable.split(',').any(|entry| entry == capability),
            "the unmanaged state names what it lost — `{capability}` is missing from \
             `unavailable=`; got: {line}"
        );
    }
}

/// Unfakeable because it asserts the negative as well as the positive: the
/// degraded surfaces must report best-effort AND must never anywhere report
/// delivery as assured. The assured-delivery promise says loss is visible and
/// retried through a durable path; the durable path is precisely what an
/// unreadable store removes, so a surface still claiming assurance is claiming
/// a guarantee it cannot keep.
#[test]
fn degraded_delivery_is_reported_best_effort_never_assured() {
    let deployment = DegradedDeployment::prepare();
    let run = deployment.start();

    let rendered = settings_surface(&deployment);
    let health_body = health_surface(run.health_port);
    run.stop();

    for (surface, text) in [("vigil settings", &rendered), ("/health", &health_body)] {
        let line = unmanaged_line(text).unwrap_or_else(|| {
            panic!("the {surface} surface must carry the unmanaged line; got:\n{text}")
        });
        assert_eq!(
            token_field(line, "delivery"),
            Some("best-effort"),
            "while degraded, delivery is reported best-effort on {surface}; got: {line}"
        );
        assert!(
            !text.contains("delivery=assured"),
            "no degraded surface may report delivery as assured — the durable path is what is \
             missing; {surface} said:\n{text}"
        );
    }
}

/// Unfakeable because all four capabilities are genuinely attempted through the
/// surfaces an operator would actually use, and each refusal must name its own
/// capability AND the unreadable store. Four refusals that are the same generic
/// error, or one capability silently succeeding — writing a clip, accepting a
/// correction, recording a settings change into a store that cannot be read —
/// fails it. Nothing here is approximated quietly.
#[test]
fn recording_review_corrections_and_settings_changes_each_refuse_with_a_statement_of_what_is_unavailable()
 {
    let deployment = DegradedDeployment::prepare();
    let run = deployment.start();

    // The recording surface is the review data plane's media route: asking for
    // a recorded clip. It must ANSWER — "each says so when attempted" is not
    // satisfied by a port that refuses connections.
    wait_for_tcp_port(run.review_port, Duration::from_secs(30)).unwrap_or_else(|error| {
        panic!(
            "{error}. A degraded run must still answer the surfaces it lost, so it can say what \
             is unavailable rather than leaving an operator with a connection error."
        )
    });
    let recording = request(run.review_port, "GET", "/media/any-clip", &[], b"");
    let recording_text = recording.body_text();
    let recording_refused = recording.status;

    // Review history.
    let review = deployment.cli(&["events"]);
    let review_text = combined(&review);

    // Corrections: an enrollment is a correction in the product's own
    // vocabulary, and it is the correction path the command line offers.
    let correction = deployment.cli(&["enroll", "detection-1", "alice"]);
    let correction_text = combined(&correction);

    // A settings change.
    let settings_change = deployment.cli(&["settings", "set", "detector_sample_frames", "7"]);
    let settings_change_text = combined(&settings_change);

    run.stop();

    assert!(
        !(200..300).contains(&recording_refused),
        "asking for a recorded clip while degraded must be refused, not served — recording needs \
         the store; got status {recording_refused}, body:\n{recording_text}"
    );
    assert!(
        !review.status.success(),
        "review history must refuse while degraded; `vigil events` exited 0 with:\n{review_text}"
    );
    assert!(
        !correction.status.success(),
        "corrections must refuse while degraded; `vigil enroll` exited 0 with:\n{correction_text}"
    );
    assert!(
        !settings_change.status.success(),
        "a settings change must refuse while degraded; `vigil settings set` exited 0 with:\n\
         {settings_change_text}"
    );

    // The marker each refusal answers with: only a SETTINGS refusal carries
    // `settings-error` — the marker `vigil settings`'s own success/failure
    // reading (`answer_failed`) and `print_control_or_direct`'s exit-code
    // check key on — the three CAPABILITY refusals now carry the distinct,
    // honest `capability-error` marker instead of borrowing the settings
    // one. Pinned here because nothing else in the estate does: a capability
    // refusal aliased back onto the settings marker would leave every other
    // assertion in this test green.
    let attempts = [
        (
            "recording",
            "recording",
            recording_text.as_str(),
            CAPABILITY_REFUSAL_PREFIX,
        ),
        (
            "review history",
            "review",
            review_text.as_str(),
            CAPABILITY_REFUSAL_PREFIX,
        ),
        (
            "corrections",
            "correction",
            correction_text.as_str(),
            CAPABILITY_REFUSAL_PREFIX,
        ),
        (
            "settings changes",
            "setting",
            settings_change_text.as_str(),
            SETTINGS_ERROR_PREFIX,
        ),
    ];
    for (capability, word, statement, expected_prefix) in attempts {
        let lower = statement.to_ascii_lowercase();
        assert!(
            lower.contains(word),
            "the refusal for {capability} must name the capability it is refusing, not fail \
             generically; got:\n{statement}"
        );
        assert!(
            lower.contains("unavailable"),
            "the refusal for {capability} must state that the capability is unavailable; \
             got:\n{statement}"
        );
        assert!(
            lower.contains("store") && (lower.contains("unreadable") || lower.contains("read")),
            "every degraded refusal shares one shape naming the store as unreadable, so the \
             operator learns the cause rather than only the symptom; the {capability} refusal did \
             not:\n{statement}"
        );
        assert!(
            statement
                .lines()
                .any(|line| line.trim_start().starts_with(expected_prefix)),
            "the {capability} refusal must begin a line with `{expected_prefix}` — a settings \
             refusal and a capability refusal must stay distinguishable by marker, not only by \
             wording; got:\n{statement}"
        );
    }
    // The complement, both directions: a capability refusal never ALSO
    // carries the settings marker, and the settings refusal never carries
    // the capability marker — proving the two markers are exclusive, not
    // merely that the expected one is present somewhere in a longer line.
    for (capability, statement, forbidden_prefix) in [
        ("recording", recording_text.as_str(), SETTINGS_ERROR_PREFIX),
        (
            "review history",
            review_text.as_str(),
            SETTINGS_ERROR_PREFIX,
        ),
        (
            "corrections",
            correction_text.as_str(),
            SETTINGS_ERROR_PREFIX,
        ),
        (
            "settings changes",
            settings_change_text.as_str(),
            CAPABILITY_REFUSAL_PREFIX,
        ),
    ] {
        assert!(
            !statement
                .lines()
                .any(|line| line.trim_start().starts_with(forbidden_prefix)),
            "the {capability} refusal must never ALSO begin a line with `{forbidden_prefix}`; \
             got:\n{statement}"
        );
    }

    let distinct: std::collections::BTreeSet<String> = attempts
        .iter()
        .map(|(_, word, _, _)| (*word).to_string())
        .collect();
    assert_eq!(
        distinct.len(),
        4,
        "the four capabilities must be distinguishable in their refusals"
    );
}

/// Unfakeable because it reads the surfaces REPEATEDLY, long after startup, and
/// on more than one of them. A startup warning that scrolls away passes a
/// single read and fails this; a statement that appears only in the process's
/// own log and not on the operator surfaces fails it too. Nothing here waits on
/// a duration — the proof is repeated reads and the process's own log being
/// insufficient, not elapsed time.
/// The unmanaged line is not merely PRESENT — it carries the degraded
/// capability contract itself, which is what criterion 11 asks the rendered
/// projection to prove: which capabilities keep working, which are unavailable,
/// and that delivery is reported best-effort rather than assured.
///
/// The vocabularies come from the degraded enums' own renderings, never from
/// the setting-line field keys. That distinction is deliberate: `running=` here
/// is the list of capabilities still working, which is a different thing from a
/// setting's running value, and sourcing it from `RUNNING_KEY` would conflate
/// two unrelated contracts that happen to share an English word.
///
/// Unfakeable because every member of both rosters is required by name and the
/// delivery field must read best-effort — a build that emitted a well-worded
/// statement while dropping a capability from the list, or that quietly claimed
/// assured delivery while the durable path is exactly what is missing, fails
/// here rather than reading as honest prose.
fn assert_unmanaged_line_carries_the_capability_contract(line: &str, read: usize, surface: &str) {
    for capability in DegradedCapability::all() {
        assert!(
            line.contains(capability.as_str()),
            "read {read} of the {surface} surface omitted `{}` from the capabilities the degraded \
             run keeps working. Someone watching a property still sees what is happening, and the \
             line has to say so; got: {line}",
            capability.as_str()
        );
    }
    for capability in UnavailableCapability::all() {
        assert!(
            line.contains(capability.as_str()),
            "read {read} of the {surface} surface omitted `{}` from what is unavailable. The \
             degraded contract names what it lost rather than leaving the operator to discover \
             it; got: {line}",
            capability.as_str()
        );
    }
    assert!(
        line.contains(DeliveryAssurance::BestEffort.as_str()),
        "read {read} of the {surface} surface must report delivery as `{}` — the durable delivery \
         path is exactly what is missing while degraded; got: {line}",
        DeliveryAssurance::BestEffort.as_str()
    );
    assert!(
        !line.contains(DeliveryAssurance::Assured.as_str()),
        "read {read} of the {surface} surface claimed `{}` delivery during a degraded run, which \
         is the one thing the honest-qualification rule forbids; got: {line}",
        DeliveryAssurance::Assured.as_str()
    );
}

#[test]
fn the_unmanaged_state_is_restated_on_every_operator_surface_for_as_long_as_it_lasts() {
    let deployment = DegradedDeployment::prepare();
    let run = deployment.start();

    let mut settings_reads = Vec::new();
    let mut health_reads = Vec::new();
    for _ in 0..3 {
        settings_reads.push(settings_surface(&deployment));
        health_reads.push(health_surface(run.health_port));
    }

    // Interleave a further round after other traffic has gone through the same
    // surfaces, so a statement that is emitted once and consumed is caught.
    let _ = deployment.cli(&["events"]);
    settings_reads.push(settings_surface(&deployment));
    health_reads.push(health_surface(run.health_port));

    run.stop();

    for (index, rendered) in settings_reads.iter().enumerate() {
        let line = unmanaged_line(rendered).unwrap_or_else(|| {
            panic!(
                "read {} of the `vigil settings` surface lost the `{UNMANAGED_LINE_PREFIX}` line. \
                 The unmanaged state is restated for as long as it lasts, not announced once; \
                 got:\n{rendered}",
                index + 1
            )
        });
        assert!(
            !line.trim_end().ends_with("statement="),
            "the unmanaged line must carry the statement naming what is unavailable, not an \
             empty field; read {} got: {line}",
            index + 1
        );
        assert_unmanaged_line_carries_the_capability_contract(line, index + 1, "vigil settings");
    }
    for (index, body) in health_reads.iter().enumerate() {
        let line = unmanaged_line(body).unwrap_or_else(|| {
            panic!(
                "read {} of the /health surface lost the `{UNMANAGED_LINE_PREFIX}` line. The state \
                 is stated on EVERY operator surface, not just the one the operator happened to \
                 check first; got:\n{body}",
                index + 1
            )
        });
        assert_unmanaged_line_carries_the_capability_contract(line, index + 1, "/health");
    }

    let first_settings = unmanaged_line(&settings_reads[0]).expect("first settings read");
    let last_settings = unmanaged_line(settings_reads.last().expect("last settings read"))
        .expect("last settings read");
    assert_eq!(
        first_settings, last_settings,
        "the statement must not decay between reads — it is the same one projection every time"
    );
    let first_health = unmanaged_line(&health_reads[0]).expect("first health read");
    assert_eq!(
        first_settings, first_health,
        "both operator surfaces render the SAME projection, so the statements are byte-identical; \
         `vigil settings` said {first_settings:?} and /health said {first_health:?}"
    );
}

/// Unfakeable because it hashes the contents of every regular file in the whole
/// tree the data directory sits in — before and after the run — so a single byte
/// written anywhere fails it, including a persisted service identity and
/// including the sidecar cache, which lives BESIDE the data directory rather
/// than inside it and would sit outside a data-directory-only audit entirely. An
/// unmanaged run must never later be mistaken for a recorded one, and a
/// storeless first start derives its identity for that run only. The control
/// socket lives in a separate temporary directory outside this tree, so no
/// transport artifact can be mistaken for a record — or used to excuse one.
#[test]
fn a_degraded_run_writes_nothing_including_the_persisted_service_identity() {
    let deployment = DegradedDeployment::prepare();

    let audited_root = deployment.tmp.path().to_path_buf();
    let before = snapshot_regular_files(&audited_root);
    assert!(
        before.contains_key(&deployment.store_path),
        "sanity: the corrupt store file must be present before the run — an ABSENT store is a \
         first start Vigil creates, which is the wrong branch entirely"
    );

    let run = deployment.start();
    // Exercise the surfaces before stopping: a run that writes only when
    // someone asks it something would otherwise slip through.
    let rendered = settings_surface(&deployment);
    let _ = health_surface(run.health_port);
    let _ = deployment.cli(&["settings", "set", "detector_sample_frames", "7"]);
    let _ = deployment.cli(&["events"]);
    run.stop();

    assert!(
        unmanaged_line(&rendered).is_some(),
        "sanity: this must be a genuinely degraded run; got:\n{rendered}"
    );

    let after = snapshot_regular_files(&audited_root);

    let lock_path = deployment.advisory_lock_path();
    let new_paths: Vec<&PathBuf> = after
        .keys()
        .filter(|path| !before.contains_key(*path) && *path != &lock_path)
        .collect();
    assert!(
        new_paths.is_empty(),
        "a degraded run writes NOTHING — including the persisted service identity and any \
         identity sidecar beside the data directory — but these files appeared: {new_paths:?} \
         (the storage engine's own advisory lock at {} is the sole permitted new artifact)",
        lock_path.display()
    );
    let removed: Vec<&PathBuf> = before
        .keys()
        .filter(|path| !after.contains_key(*path))
        .collect();
    assert!(
        removed.is_empty(),
        "a degraded run must not remove files either; these disappeared: {removed:?}"
    );
    for (path, digest) in &before {
        assert_eq!(
            after.get(path),
            Some(digest),
            "a degraded run changed {} — every store data file must be byte-identical before and \
             after, lock file aside",
            path.display()
        );
    }
}

// ── A degraded run with a stream that actually connects ────────────────────

/// Every path under `root`, directories included, so a working area created but
/// left empty is caught as well as a file written into it. The file audit above
/// hashes contents; this one is about EXISTENCE, which is the half a run that
/// only ever prepares a directory would otherwise slip through.
fn snapshot_all_paths(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => panic!("read {}: {error}", directory.display()),
        };
        for entry in entries {
            let entry = entry.expect("directory entry");
            let path = entry.path();
            let file_type = entry.file_type().expect("file type");
            if file_type.is_dir() {
                pending.push(path.clone());
                found.insert(path, "directory".to_string());
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let bytes = fs::read(&path).unwrap_or_default();
            let digest = Sha256::digest(&bytes);
            found.insert(path, format!("{digest:x}:{}", bytes.len()));
        }
    }
    found
}

/// The person clip the estate already carries, used here only as something real
/// for a camera to decode.
fn person_clip() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("fixtures")
        .join("video")
        .join("one-by-one-person-detection.mp4")
}

/// A loopback address nobody is listening on: reserved so no other test in this
/// run holds it, then released, so a connection to it is refused immediately
/// rather than left hanging on an unroutable address.
fn refused_camera_url() -> String {
    let reservation = TcpPortReservation::reserve_loopback().expect("reserve a closed port");
    let port = reservation.port();
    reservation.release();
    format!("rtsp://127.0.0.1:{port}/stream")
}

/// Unfakeable because frames genuinely flow: a real RTSP source serves the
/// estate's own clip, the run is waited on until it reports decoded frames, and
/// only then is the tree compared — directories included. The existing audit
/// above never reaches this path, because its cameras never connect, so a
/// capture working area created the moment a stream arrives sits exactly in
/// that blind spot. A degraded run writes nothing means nothing, including a
/// staging or clip area prepared for a recording this run is never allowed to
/// make.
#[test]
fn a_degraded_run_writes_nothing_even_when_a_stream_connects() {
    let _serialized = rtsp_fixture_lock().lock().unwrap_or_else(|poisoned| {
        // A poisoned lock means an earlier RTSP test panicked; the fixture is
        // still ours to take, and failing here would report the wrong defect.
        poisoned.into_inner()
    });
    let stream = RtspFixture::start(&person_clip()).expect("serve the fixture clip over RTSP");
    let deployment =
        DegradedDeployment::prepare_with_camera_urls(&[(FIRST_CAMERA_NAME, stream.url.as_str())]);

    let audited_root = deployment.tmp.path().to_path_buf();
    let before = snapshot_all_paths(&audited_root);

    let run = deployment.start();
    // The state that says the stream really arrived and was decoded — not a
    // timer, and not the mere fact that the process is up.
    wait_until(
        "the degraded run to decode frames from a connected camera",
        Duration::from_secs(90),
        || {
            let logs = run.stdout();
            Ok((logs.contains("decoded_frames=") && logs.contains("rtsp opened")).then_some(()))
        },
    )
    .unwrap_or_else(|error| {
        panic!(
            "{error}. This test is about what a degraded run writes once a stream connects, so \
             the stream connecting is the precondition. Output so far:\n{}",
            run.stdout()
        )
    });
    // The one estate proof that a storeless run reporting `ready` on an
    // ordinary decoded frame (the r4/round-5 finding-6 fix,
    // `HealthState::healthy_baseline()` at `runtime.rs:2974`) is actually
    // exercised by a run that decoded a real frame — every other call site of
    // `health_surface` in this file runs a camera that never connects, so
    // reverting the production fix would leave the whole suite green. This
    // assertion runs AFTER the decode wait above, so it observes `/health`
    // only once a frame has genuinely been decoded.
    let health_body = health_surface(run.health_port);
    let rendered = settings_surface(&deployment);
    let logs = run.stdout();
    run.stop();

    assert!(
        unmanaged_line(&health_body).is_some(),
        "sanity: /health must carry the standing unmanaged statement once a frame has decoded; \
         got:\n{health_body}"
    );
    assert!(
        unmanaged_line(&rendered).is_some(),
        "sanity: this must be a genuinely degraded run; got:\n{rendered}"
    );

    let after = snapshot_all_paths(&audited_root);
    let lock_path = deployment.advisory_lock_path();
    let appeared: Vec<&PathBuf> = after
        .keys()
        .filter(|path| !before.contains_key(*path) && *path != &lock_path)
        .collect();
    assert!(
        appeared.is_empty(),
        "a degraded run writes nothing under the data directory even once frames are flowing — \
         recording is one of the four capabilities it does not have, so a capture working area is \
         a trace of a recording that is not allowed to happen. These appeared: {appeared:?} (the \
         storage engine's own advisory lock at {} is the sole permitted new artifact)\n\
         Output:\n{logs}",
        lock_path.display()
    );
    for (path, digest) in &before {
        assert_eq!(
            after.get(path),
            Some(digest),
            "a degraded run changed {} while a stream was connected — every store data file must \
             be byte-identical before and after, lock file aside",
            path.display()
        );
    }
}

// ── What a degraded run says is wrong with it ──────────────────────────────

/// The labels that describe the NODE rather than a camera. A per-camera entry
/// carrying one of these is reporting the node's condition in a camera's field,
/// which is the laundering this rules out. Spelled here because the production
/// label vocabulary is not exported; the implementation gives the camera facts
/// their own type and this list then reads from it.
const NODE_SCOPE_STATUS_LABELS: [&str; 3] = [
    "running_unmanaged",
    "store_open_failed",
    "no_cameras_configured",
];

/// A successful storeless start answers alive while it is genuinely functional
/// (live view, detection and broker alerting are up), and stays alive through
/// non-fatal camera trouble. But a LATER, independent failure — the camera's
/// own ingest giving out — is a different fact than "no store behind this
/// run," and it answers its own normal liveness code: a restart can
/// plausibly clear a wedged ingest path, so the watchdog has to be able to
/// see it. What never moves, on either answer, is the standing unmanaged
/// statement: it is a fact about the store, and the ingest condition is a
/// fact about the camera, so reporting one must not cost the other.
///
/// Unfakeable because the camera is pointed at a closed loopback port, so its
/// ingest genuinely fails: a build that keeps forcing every unmanaged answer
/// to 200 fails the failure-path assertion below, and one that drops the
/// unmanaged statement once a real condition is being reported fails the
/// body assertion on either path.
#[test]
fn the_liveness_answer_stays_protected_while_the_body_names_the_real_condition() {
    let closed_stream = refused_camera_url();
    let deployment = DegradedDeployment::prepare_with_camera_urls(&[(
        FIRST_CAMERA_NAME,
        closed_stream.as_str(),
    )]);
    let run = deployment.start();

    let mut observed_functional_answer = false;
    let failure_body = wait_until(
        "the degraded run to move from a functional storeless start to reporting the camera's \
         own ingest failure",
        Duration::from_secs(60),
        || {
            let response = get(run.health_port, "/health");
            let body = response.body_text();
            let status = health_first_line_json(&body)
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match status.as_str() {
                "running_unmanaged" => {
                    assert_eq!(
                        response.status, 200,
                        "a freshly started storeless run is alive and functional, so the \
                         watchdog-facing answer is 2xx; body:\n{body}"
                    );
                    assert!(
                        unmanaged_line(&body).is_some(),
                        "a functional storeless answer still says it has no store behind it; \
                         got:\n{body}"
                    );
                    observed_functional_answer = true;
                    Ok(None)
                }
                "ingest_failed" => {
                    assert_eq!(
                        response.status, 503,
                        "the camera's own ingest failure is a later, independent condition from \
                         the storeless start, so it answers ingest_failed's own normal liveness \
                         code rather than being folded into the always-alive unmanaged answer; \
                         body:\n{body}"
                    );
                    assert!(
                        unmanaged_line(&body).is_some(),
                        "reporting the camera's real condition must not cost the standing \
                         unmanaged statement, which is what tells the operator why recording \
                         and settings changes are gone; got:\n{body}"
                    );
                    Ok(Some(body))
                }
                _ => Ok(None),
            }
        },
    )
    .unwrap_or_else(|error| {
        panic!(
            "{error}. The camera cannot reach its stream, and that is a fact about the camera, not \
             about the store: reporting it as the unmanaged state hides a fault an operator can \
             actually act on behind one they cannot. Output:\n{}",
            run.stdout()
        )
    });
    assert!(
        observed_functional_answer,
        "never observed the storeless start answering as functional (running_unmanaged, 2xx) \
         before the camera's ingest failure landed; the run went straight to a failure answer \
         with nothing to compare it against. Output:\n{}",
        run.stdout()
    );
    run.stop();

    assert!(
        unmanaged_line(&failure_body).is_some(),
        "and the run still says it is unmanaged on the failure answer: reporting the real \
         condition must not cost the standing statement; got:\n{failure_body}"
    );
}

/// Unfakeable because both halves are asserted on the same body: each camera
/// must carry its own condition AND that condition must not be the node's. A
/// build that copies one shared node status into every camera entry fails, and
/// so does one that drops the per-camera status field to avoid the question.
#[test]
fn each_camera_reports_its_own_condition_rather_than_the_nodes_unmanaged_state() {
    let first_stream = refused_camera_url();
    let second_stream = refused_camera_url();
    let deployment = DegradedDeployment::prepare_with_camera_urls(&[
        (FIRST_CAMERA_NAME, first_stream.as_str()),
        (SECOND_CAMERA_NAME, second_stream.as_str()),
    ]);
    let run = deployment.start();
    let body_text = health_surface(run.health_port);
    let logs = run.stdout();
    run.stop();

    let body = health_first_line_json(&body_text);
    let cameras = body
        .get("cameras_by_kind")
        .and_then(Value::as_object)
        .and_then(|kinds| kinds.get("rtsp"))
        .and_then(|entry| entry.get("cameras"))
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            panic!("the degraded node must still list its cameras; body:\n{body_text}")
        });
    assert_eq!(
        cameras.len(),
        2,
        "both configured cameras are listed; body:\n{body_text}"
    );

    for camera in cameras {
        let name = camera
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("each camera entry carries its name; body:\n{body_text}"));
        let status = camera
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                panic!("each camera entry carries its own status; body:\n{body_text}")
            });
        assert!(
            !status.trim().is_empty(),
            "the status for \"{name}\" is a token a reader can act on, never a blank; \
             body:\n{body_text}"
        );
        assert_eq!(
            status.split_whitespace().count(),
            1,
            "a camera status is one whitespace-free token; \"{name}\" reported {status:?}"
        );
        assert!(
            !NODE_SCOPE_STATUS_LABELS.contains(&status),
            "\"{name}\" reported {status:?}, which describes the NODE. Whether this node's store \
             is readable is not a fact about a camera: a per-camera field carrying it tells an \
             operator every camera is in the same trouble, whatever each camera is actually \
             doing. Output:\n{logs}"
        );
    }
}

// ── A degraded run whose configured store lives outside data_dir ──────────

/// The point this pins is NOT "storeless writes nothing" — that is proven
/// above for a store-unreadable run, where every settings change is refused
/// outright and nothing lands anywhere. This is the OTHER degraded cause: a
/// recognition component that failed to load never touches the store at
/// all, so the store itself is genuinely fine and a settings change lands —
/// and it must land where the operator configured the store, at
/// `--store-path`'s own directory, never inside `data_dir` just because that
/// is the deployment directory this run was pointed at. Before
/// `crates/vigil/src/runtime.rs`'s `degraded_control_data_dir` followed
/// `store_path` rather than always answering `data_dir`, a change made here
/// would have created a SECOND, healthy store under `data_dir` — one the
/// operator never configured and would never find at their real store
/// location.
#[test]
fn a_degraded_run_with_the_store_outside_data_dir_writes_no_store_under_data_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let socket_tmp = tempfile::tempdir().expect("tempdir for the control socket");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("data dir");
    let elsewhere = tmp.path().join("store-elsewhere");
    fs::create_dir_all(&elsewhere).expect("elsewhere dir");
    let store_path = elsewhere.join("store.contextgraph");
    // A real, empty weights directory: the recognition component fails to
    // load (no model.safetensors), which is a fault the store is never even
    // reached for — the store at `store_path` stays genuinely openable, so a
    // settings change actually lands rather than being refused outright.
    let weights_dir = tmp.path().join("weights");
    fs::create_dir_all(&weights_dir).expect("empty weights dir");

    let config_path = tmp.path().join("vigil.toml");
    let config = format!(
        "site_name = \"home farm\"\nrecognition_weights_dir = \"{}\"\n\n[[cameras]]\n\
         name = \"{FIRST_CAMERA_NAME}\"\nrtsp_url = \"rtsp://192.0.2.10:554/stream\"\n",
        toml_path(&weights_dir)
    );
    fs::write(&config_path, config).expect("write the camera config");

    let socket_path = socket_tmp.path().join("control.sock");
    let health = TcpPortReservation::reserve_loopback().expect("reserve health port");
    let review = TcpPortReservation::reserve_loopback().expect("reserve review port");
    let health_port = health.port();
    let review_port = review.port();
    let model_path = fixture_detector_model_path();
    assert!(
        model_path.is_file(),
        "the repo's real detector-model fixture must be present: {}",
        model_path.display()
    );

    let mut command = Command::new(vigil_binary_path());
    command
        .arg("run")
        .arg("--config")
        .arg(&config_path)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--store-path")
        .arg(&store_path)
        .arg("--health-port")
        .arg(health_port.to_string())
        .arg("--review-port")
        .arg(review_port.to_string())
        .arg("--detector-model-path")
        .arg(&model_path)
        .env("VIGIL_DATA_DIR", &data_dir)
        .env("VIGIL_CONTROL_SOCKET", &socket_path)
        // The CLI flag above carries the store location; the env var stays
        // unset so the two can never disagree about where it points.
        .env_remove("VIGIL_STORE_PATH")
        .env_remove("VIGIL_FABRIC_TICKET")
        .env_remove("VIGIL_FABRIC_HUB")
        .env_remove("VIGIL_RECOGNITION_WEIGHTS_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    health.release();
    review.release();
    let mut child = command.spawn().expect("spawn vigil run");
    let stdout = capture_pipe(child.stdout.take());
    let _stderr = capture_pipe(child.stderr.take());

    wait_until(
        &format!(
            "the degraded runtime's control socket at {} to appear",
            socket_path.display()
        ),
        Duration::from_secs(30),
        || Ok(UnixStream::connect(&socket_path).ok().map(|_| ())),
    )
    .unwrap_or_else(|error| {
        let logs = stdout.lock().expect("stdout lock").clone();
        panic!("{error}. Process output so far:\n{logs}")
    });

    let before_data_dir = snapshot_regular_files(&data_dir);

    let settings_output = Command::new(vigil_binary_path())
        .args(["settings", "set", "detector_sample_frames", "7"])
        .env("VIGIL_DATA_DIR", &data_dir)
        .env("VIGIL_CONTROL_SOCKET", &socket_path)
        .env_remove("VIGIL_STORE_PATH")
        .stdin(Stdio::null())
        .output()
        .expect("spawn vigil settings set");

    let logs = stdout.lock().expect("stdout lock").clone();
    let _ = child.kill();
    let _ = child.wait();

    let settings_text = format!(
        "{}{}",
        String::from_utf8_lossy(&settings_output.stdout),
        String::from_utf8_lossy(&settings_output.stderr)
    );
    assert!(
        settings_output.status.success(),
        "a component-failure degraded run must let a settings change land — the store at \
         --store-path is genuinely readable; exited {:?}, output:\n{settings_text}\nlogs:\n{logs}",
        settings_output.status.code()
    );

    let after_data_dir = snapshot_regular_files(&data_dir);
    assert_eq!(
        before_data_dir, after_data_dir,
        "a landed settings change must create nothing at all under data_dir when the store lives \
         outside it — the operator's real store location is --store-path's own directory, not the \
         deployment directory this run happened to be pointed at; logs:\n{logs}"
    );
    assert!(
        !SettingsStore::store_path(&data_dir).exists(),
        "no second, healthy store may appear under data_dir just because the configured store \
         lives elsewhere: {}",
        SettingsStore::store_path(&data_dir).display()
    );

    assert!(
        store_path.exists(),
        "the settings change must land at the operator's actual configured store location, {}, \
         which never appeared; logs:\n{logs}",
        store_path.display()
    );
}

// ── What a storeless run says about its own identity ───────────────────────

/// A run with no store behind it derives its identity for that run only, and
/// two places say what it derived: the startup line an operator watches
/// scroll by, and the degraded settings listing they read afterwards. If
/// those two came from separate derivations, a rename between them — or a
/// build that fixed one path and not the other — would let this node
/// announce one identity on the broker and answer a different one to `vigil
/// settings`, leaving an operator unable to tell which node they are even
/// looking at. This fixture's site name (`home farm`) already differs from
/// its data directory's own name (`data`), so a build that silently fell
/// back to the directory name on one of the two surfaces cannot pass by
/// accident.
#[test]
fn a_storeless_run_reports_the_same_identity_on_the_startup_line_and_the_degraded_listing() {
    let deployment = DegradedDeployment::prepare();
    let run = deployment.start();
    let logs = run.stdout();
    let rendered = settings_surface(&deployment);
    run.stop();

    let announced = logs
        .lines()
        .find_map(|line| line.strip_prefix("service_id="))
        .unwrap_or_else(|| panic!("the startup log must announce `service_id=`; logs:\n{logs}"))
        .to_string();
    assert_ne!(
        announced, "data",
        "sanity: the fixture's data directory is literally named \"data\" — a build that fell \
         back to the directory name would coincidentally still pass a bare equality check \
         without this guard"
    );

    let identity_line = rendered
        .lines()
        .find(|line| line.starts_with("identity "))
        .unwrap_or_else(|| {
            panic!("the degraded listing must carry an `identity` line; got:\n{rendered}")
        });
    let listed = token_field(identity_line, "value")
        .unwrap_or_else(|| panic!("the identity line must carry `value=`; got: {identity_line}"));

    assert_eq!(
        announced, listed,
        "the startup announcement and the degraded listing must name the SAME identity; \
         startup said {announced:?}, the listing said {listed:?}. logs:\n{logs}\nrendered:\n\
         {rendered}"
    );
}

/// A deployment that configured a `service_id` but has no store to persist it
/// into stated one, rather than working one out — and the derivation the
/// listing carries has to say which of the two happened. Reporting
/// `derived-not-yet-persisted` here would tell an operator this node picked
/// its own name, when it was actually told one and just could not write it
/// down yet.
#[test]
fn a_configured_service_id_on_a_storeless_run_reports_configured_not_yet_persisted() {
    let deployment = DegradedDeployment::prepare();
    let configured = "front-porch-node";
    let run = deployment.start_with_service_id(Some(configured));
    let rendered = settings_surface(&deployment);
    let logs = run.stdout();
    run.stop();

    let identity_line = rendered
        .lines()
        .find(|line| line.starts_with("identity "))
        .unwrap_or_else(|| {
            panic!("the degraded listing must carry an `identity` line; got:\n{rendered}")
        });
    assert_eq!(
        token_field(identity_line, "value"),
        Some(configured),
        "a storeless run must announce the identifier it was configured with, not one it \
         derived; got: {identity_line}\nlogs:\n{logs}"
    );
    assert_eq!(
        token_field(identity_line, "derivation"),
        Some("configured-not-yet-persisted"),
        "a configured identifier on a storeless run must be labelled \
         `configured-not-yet-persisted`, distinguishing it from an identifier this node worked \
         out for itself; got: {identity_line}\nlogs:\n{logs}"
    );
}
