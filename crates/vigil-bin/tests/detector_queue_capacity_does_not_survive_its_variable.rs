//! A lever that is gone is gone. When `VIGIL_DETECTOR_QUEUE_CAPACITY` drove
//! one run's queue depth and the next run on the SAME data directory is
//! started without it, nothing an operator reads may still be carrying the old
//! number.
//!
//! `vigil stats` asks the runtime owner over the control socket and, on any
//! socket failure, falls back to the on-disk `runtime-stats.json` in the data
//! directory. That file is read with no ownership, freshness or
//! process-identity guard of any kind, so a stopped process's last snapshot is
//! served as though it were current — which is exactly how a removed
//! environment variable came back to life on the operator's surface: the
//! override survived its own removal.
//!
//! Reusing ONE data directory across the two runs is the whole test. A fresh
//! temporary directory for the second run would prove nothing, because the
//! stale file is the defect.

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use vigil::settings_model::DETECTOR_QUEUE_CAPACITY_SETTING;
use vigil::settings_projection::{
    NAME_KEY, RUNNING_SOURCE_KEY, SETTING_LINE_PREFIX, SHADOWED_SETTING_KEY,
};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, RtspFixture, TcpPortReservation, capture_pipe, rtsp_fixture_lock,
    vigil_binary_path, wait_until, workspace_root,
};

const QUEUE_CAPACITY_VARIABLE: &str = "VIGIL_DETECTOR_QUEUE_CAPACITY";
/// The depth the first run is levered to.
const LEVER_CAPACITY: &str = "3";
/// The depth configured in the deployment, which is what the second run — the
/// one started without the variable — must be seen queueing at.
const CONFIGURED_CAPACITY: &str = "1";
/// The stats line the detector stage renders its queue state on.
const DETECTOR_QUEUE_PREFIX: &str = "detector-queue=";
/// The model identity this deployment declares for the estate's weights.
const DETECTOR_MODEL_ID: &str = "detector-under-test";

fn person_clip() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("fixtures")
        .join("video")
        .join("one-by-one-person-detection.mp4")
}

fn detector_model() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("fixtures")
        .join("models")
        .join("yolox-tiny-coco.pth")
}

// ── One deployment, two runs ───────────────────────────────────────────────

struct Deployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    config_path: PathBuf,
}

impl Deployment {
    fn with_config(body: &str) -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment directory");
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&data_dir).expect("create the data directory");
        let config_path = tmp.path().join("vigil.toml");
        fs::write(&config_path, body).expect("write the configuration file");
        Self {
            _tmp: tmp,
            data_dir,
            config_path,
        }
    }

    /// A real `vigil run` against THIS deployment's data directory, with or
    /// without the lever in its environment, returned once its control socket
    /// accepts a connection.
    fn start(&self, lever: Option<&str>) -> LiveVigil {
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match lever {
            Some(capacity) => command.env(QUEUE_CAPACITY_VARIABLE, capacity),
            None => command.env_remove(QUEUE_CAPACITY_VARIABLE),
        };
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn `vigil run`");
        let stdout = capture_pipe(child.stdout.take());
        let stderr = capture_pipe(child.stderr.take());
        let run = LiveVigil {
            child,
            stdout,
            stderr,
        };
        let socket_path = self.data_dir.join("control.sock");
        wait_until(
            &format!("the control socket at {} to appear", socket_path.display()),
            RUNTIME_STARTUP_TIMEOUT,
            || Ok(UnixStream::connect(&socket_path).ok().map(|_| ())),
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}. The runtime must publish its control socket. Output so far:\n{}",
                run.logs()
            )
        });
        run
    }

    /// `vigil stats` as a separate process, with its exit status, so a caller
    /// can hold the answer to account for having done what was asked as well
    /// as for what it says.
    fn stats_answer(&self) -> (std::process::ExitStatus, String) {
        let output = Command::new(vigil_binary_path())
            .arg("stats")
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .env_remove(QUEUE_CAPACITY_VARIABLE)
            .stdin(Stdio::null())
            .output()
            .expect("spawn `vigil stats`");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output.status, text)
    }

    /// `vigil stats` as a separate process, never carrying the lever itself.
    fn stats(&self) -> String {
        let output = Command::new(vigil_binary_path())
            .arg("stats")
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .env_remove(QUEUE_CAPACITY_VARIABLE)
            .stdin(Stdio::null())
            .output()
            .expect("spawn `vigil stats`");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    fn settings_listing(&self) -> String {
        let output = Command::new(vigil_binary_path())
            .arg("settings")
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .env_remove(QUEUE_CAPACITY_VARIABLE)
            .stdin(Stdio::null())
            .output()
            .expect("spawn `vigil settings`");
        assert!(
            output.status.success(),
            "`vigil settings` must answer; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    /// The queue state the surface currently reports, if it reports one at
    /// all.
    fn detector_queue_line(&self) -> Option<String> {
        self.stats()
            .lines()
            .find_map(|line| line.strip_prefix(DETECTOR_QUEUE_PREFIX))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    /// Wait for the running process to report a queue state of its own — a
    /// state it reaches by genuinely queueing a decoded segment, never a
    /// sleep.
    fn wait_for_a_queue_report(&self, run: &LiveVigil) -> String {
        wait_until(
            "the running process to report the detector queue it is feeding",
            RUNTIME_STARTUP_TIMEOUT,
            || Ok(self.detector_queue_line()),
        )
        .unwrap_or_else(|error| panic!("{error}. Run output:\n{}", run.logs()))
    }

    fn wait_for_the_control_socket_to_go_away(&self) {
        let socket_path = self.data_dir.join("control.sock");
        wait_until(
            &format!(
                "the control socket at {} to stop accepting connections",
                socket_path.display()
            ),
            RUNTIME_STARTUP_TIMEOUT,
            || {
                Ok(match UnixStream::connect(&socket_path) {
                    Ok(_) => None,
                    Err(_) => Some(()),
                })
            },
        )
        .expect("the first run must be genuinely down before the second one starts");
    }

    fn snapshot_path(&self) -> PathBuf {
        self.data_dir.join("runtime-stats.json")
    }
}

struct LiveVigil {
    child: Child,
    stdout: std::sync::Arc<std::sync::Mutex<String>>,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
}

impl LiveVigil {
    fn logs(&self) -> String {
        format!(
            "{}\n{}",
            self.stdout.lock().expect("stdout lock"),
            self.stderr.lock().expect("stderr lock")
        )
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

fn capacity_of(queue_line: &str) -> Option<&str> {
    queue_line
        .split_whitespace()
        .find_map(|token| token.strip_prefix("capacity="))
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

/// Unfakeable because both runs are real processes against ONE data directory
/// and the first run's number is read off the surface before it is stopped —
/// so the second half is a statement about what survived a process boundary,
/// not about a value this test wrote. The middle arm, with nothing running at
/// all, is the deterministic core: at that moment there is no process whose
/// queue could be `capacity=3`, so anything reporting it is reporting a dead
/// process's numbers as current.
#[test]
fn a_queue_depth_levered_by_an_environment_variable_does_not_outlive_it() {
    let _serialized = rtsp_fixture_lock().lock().unwrap_or_else(|poisoned| {
        // A previous fixture user panicking must not disable the serialization
        // this fixture needs; the lock's job is exclusion, not state.
        poisoned.into_inner()
    });
    // A camera that genuinely connects and a model that genuinely loads: the
    // queue only reports a state once a decoded segment has been offered to
    // it, so this proof needs real ingest.
    let camera = RtspFixture::start(&person_clip()).expect("serve the estate's clip over RTSP");
    let deployment = Deployment::with_config(&format!(
        "site_name = \"home farm\"\ndetector_model_id = \"{DETECTOR_MODEL_ID}\"\n\
         detector_model_path = \"{}\"\ndetector_sample_frames = 1\n\
         detector_queue_capacity = {CONFIGURED_CAPACITY}\n\
         \n[[cameras]]\nname = \"loading dock\"\nrtsp_url = \"{}\"\n",
        deterministic_fixture_support::toml_path(&detector_model()),
        deterministic_fixture_support::toml_string(&camera.url),
    ));

    // Run one: the lever drives the queue, and the surface says so.
    let levered = deployment.start(Some(LEVER_CAPACITY));
    let levered_queue = deployment.wait_for_a_queue_report(&levered);
    assert_eq!(
        capacity_of(&levered_queue),
        Some(LEVER_CAPACITY),
        "sanity: the first run must genuinely queue at the lever's depth, or there is no stale \
         number for the rest of this test to be about: {levered_queue}\n\nRun output:\n{}",
        levered.logs()
    );
    levered.stop();
    deployment.wait_for_the_control_socket_to_go_away();
    assert!(
        deployment.snapshot_path().exists(),
        "sanity: the stopped run left its snapshot behind at {} — that file is what the next two \
         assertions are about",
        deployment.snapshot_path().display()
    );

    // Nothing is running. There is no process with a queue, so there is no
    // honest way for the surface to report one.
    let between_runs = deployment.stats();
    assert_ne!(
        between_runs
            .lines()
            .find_map(|line| line.strip_prefix(DETECTOR_QUEUE_PREFIX))
            .and_then(capacity_of),
        Some(LEVER_CAPACITY),
        "with no process running, the stats surface must not serve the stopped process's queue \
         depth as though it were current. A snapshot file with no ownership guard is not a \
         reading of this deployment; it is the last thing somebody else wrote:\n{between_runs}"
    );

    // Run two: same data directory, no lever.
    let plain = deployment.start(None);
    let plain_queue = deployment.wait_for_a_queue_report(&plain);
    let rendered = deployment.settings_listing();
    let logs = plain.logs();
    plain.stop();

    assert_eq!(
        capacity_of(&plain_queue),
        Some(CONFIGURED_CAPACITY),
        "the second run was started without {QUEUE_CAPACITY_VARIABLE}, so it queues at the \
         configured depth and that is what the operator must read. Seeing {LEVER_CAPACITY} here \
         is a removed environment variable surviving its own removal: {plain_queue}\n\nRun \
         output:\n{logs}"
    );

    let line = setting_line(&rendered, DETECTOR_QUEUE_CAPACITY_SETTING);
    assert_eq!(
        token_field(line, RUNNING_SOURCE_KEY),
        None,
        "and with no lever set, the settings line must name no environment source: {line}"
    );
    assert_eq!(
        token_field(line, SHADOWED_SETTING_KEY),
        None,
        "and nothing is shadowing the operator's own depth, so no shadowed-setting either: {line}"
    );
}

/// A snapshot left behind by some other process is not a reading of this
/// deployment, and the honest answer to a question about a data directory no
/// running process owns is that no runtime owns it — never a zeroed block that
/// positively asserts `health=ready` and `ingest=ok` for a node that is not
/// running at all. Those are runtime facts, and a surface that states them
/// when nothing is running is stating something it cannot know.
///
/// Nothing is started here on purpose: the whole question is what the surface
/// says when there is no owner, so a live process would defeat it.
#[test]
fn a_stats_read_with_no_owner_says_no_runtime_owns_the_data_directory() {
    let deployment = Deployment::with_config("site_name = \"home farm\"\n");
    // A foreign snapshot: an ownership stamp is absent, so its provenance is
    // unknown, which is exactly the case the refusal exists for. Its numbers
    // are deliberately healthy-looking so a surface that serves them can be
    // told apart from one that refuses.
    fs::write(
        deployment.snapshot_path(),
        r#"{
  "frames_received": 4210,
  "detector_invocations": 512,
  "detections_emitted": 37,
  "observations_written": 37,
  "clip_write_failures": 0,
  "observation_write_failures": 0,
  "motion_positive_frames": 91,
  "stream_drops": 0,
  "stream_reconnects": 0,
  "stream_fps": 12.5,
  "detector_latency_p50_ms": 8.0,
  "detector_latency_p95_ms": 19.0,
  "detector_latency_max_ms": 42.0,
  "dropped_motion_positive_frames": 0,
  "processing_lag_ms": 3.0,
  "processing_lag_bound_ms": 500.0,
  "false_positive_count": 0,
  "crops_embedded": 0,
  "recognition_matches": 0,
  "recognition_unknowns": 0,
  "recognition_failures": 0,
  "embed_latency_max_ms": 0.0,
  "health": "ready",
  "ingest_signal": "ok",
  "detector_queue": "depth=0 capacity=3 queued=12 replaced=0"
}
"#,
    )
    .expect("write the foreign snapshot");

    let (status, answer) = deployment.stats_answer();

    assert!(
        !answer.contains("health=ready"),
        "with no process running, `vigil stats` must not assert this deployment is ready — that \
         is a runtime fact and there is no runtime:\n{answer}"
    );
    assert!(
        !answer.contains("ingest=ok"),
        "and it must not assert that ingest is fine on a node that is not ingesting anything:\n\
         {answer}"
    );
    assert!(
        answer.contains("no runtime owns")
            && answer.contains(&deployment.data_dir.display().to_string()),
        "the answer must say that no runtime owns this data directory, and name the directory it \
         is talking about, so an operator knows the difference between a quiet node and a stopped \
         one:\n{answer}"
    );
    assert!(
        answer.contains("no establishable writer identity"),
        "and it must name the condition it actually found — a snapshot nothing can be attributed \
         from — because that is what sends the operator to the right place to look:\n{answer}"
    );
    assert!(
        !answer.contains("no longer running"),
        "and it must not assert that the writer has stopped: nothing here established who wrote \
         this snapshot, so nothing here can say whether that writer is still alive — a \
         permission-walled file may be being written right now:\n{answer}"
    );
    assert!(
        !status.success(),
        "and it did not do what was asked — there were no live figures to read — so the exit \
         status has to say so:\n{answer}"
    );
}

/// A data directory where nothing has ever run is not the same condition as a
/// directory holding a snapshot nobody can be shown to have written, and an
/// operator is entitled to be told which one they are looking at. Zero frames
/// received is the true and complete answer for a node that has never started;
/// what must not be said alongside it is that the node is `ready` and its
/// ingest is `ok`, because those are runtime facts about a process that does
/// not exist.
///
/// Nothing is started here on purpose, and no snapshot is written: the absence
/// of the file IS the case under test.
#[test]
fn a_stats_read_on_a_never_started_directory_answers_zeros_under_a_not_started_line() {
    let deployment = Deployment::with_config("site_name = \"home farm\"\n");
    assert!(
        !deployment.snapshot_path().exists(),
        "the case under test is a directory nothing has ever run in, so there must be no \
         snapshot at {}",
        deployment.snapshot_path().display()
    );

    let (status, answer) = deployment.stats_answer();

    assert!(
        status.success(),
        "a node that has never run has a true answer — nothing has happened here — so the \
         surface must answer it rather than refuse:\n{answer}"
    );
    for zero in [
        "frames-received=0",
        "detector-invocations=0",
        "detections-emitted=0",
        "observations-written=0",
        "clip-write-failures=0",
        "observation-write-failures=0",
        "motion-positive-frames=0",
        "stream-drops=0",
        "stream-reconnects=0",
    ] {
        assert!(
            answer.contains(zero),
            "nothing has run, so every counter reads zero; missing {zero}:\n{answer}"
        );
    }
    assert!(
        answer.contains("processing-lag-bound-ms="),
        "the keep-pace bound is a configured figure, not a runtime observation, so it is still \
         part of the answer:\n{answer}"
    );
    assert!(
        !answer.contains("health=ready"),
        "nothing is running, so the surface must not assert this deployment is ready:\n{answer}"
    );
    assert!(
        !answer.contains("ingest=ok"),
        "and it must not assert that ingest is fine on a node that has never ingested \
         anything:\n{answer}"
    );
    assert!(
        !answer.contains("no runtime owns"),
        "and it must not blame a stopped process for a directory no process has ever written \
         to — there is nothing here to have stopped:\n{answer}"
    );
    assert!(
        answer.contains("nothing has run"),
        "the answer must say in plain words that nothing has run in this directory yet, so an \
         operator can tell a never-started node from a stopped one:\n{answer}"
    );
    assert!(
        answer.contains(&deployment.data_dir.display().to_string()),
        "and it must name the directory it looked in: the commonest way to reach this answer is \
         a mistyped data directory, and all-zeros without an address gives that operator nothing \
         to notice it by:\n{answer}"
    );
    let lowered = answer.to_ascii_lowercase();
    for forbidden in [
        "n/a",
        "placeholder",
        "todo",
        "panic",
        "backtrace",
        "usage",
        "error",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "a first-run answer is an ordinary answer, not a failure report; it used the \
             forbidden token {forbidden}:\n{answer}"
        );
    }
}

/// This deployment's own last run, since ended, wrote figures that really
/// happened here. Refusing them destroys the readback of every fault that is
/// only observable after the run that produced it — a disk-full clip write, a
/// dead stream, a motion-gated non-detection. They are served, and the answer
/// says plainly that they are the last run's record and not a reading of
/// anything running now.
///
/// The stamp names a pid that has been reaped, so its identity can never be
/// mistaken for a live process: that is deterministic, with no runtime spawned.
#[test]
fn a_stats_read_of_this_deployments_own_stopped_run_serves_its_last_figures() {
    let deployment = Deployment::with_config("site_name = \"home farm\"\n");
    // A process this test started and waited on: it is gone, and gone
    // processes stay gone, so `is_still_running` is false for it forever.
    let mut reaped = Command::new("/bin/true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn a process to reap");
    let dead_pid = reaped.id();
    reaped.wait().expect("reap it");

    fs::write(
        deployment.snapshot_path(),
        format!(
            r#"{{
  "frames_received": 4210,
  "detector_invocations": 512,
  "detections_emitted": 37,
  "observations_written": 36,
  "clip_write_failures": 1,
  "observation_write_failures": 0,
  "motion_positive_frames": 91,
  "stream_drops": 0,
  "stream_reconnects": 0,
  "stream_fps": 12.5,
  "detector_latency_p50_ms": 8.0,
  "detector_latency_p95_ms": 19.0,
  "detector_latency_max_ms": 42.0,
  "dropped_motion_positive_frames": 0,
  "processing_lag_ms": 3.0,
  "processing_lag_bound_ms": 500.0,
  "false_positive_count": 0,
  "crops_embedded": 0,
  "recognition_matches": 0,
  "recognition_unknowns": 0,
  "recognition_failures": 0,
  "embed_latency_max_ms": 0.0,
  "health": "disk-full",
  "ingest_signal": "ok",
  "written_by": {{ "pid": {dead_pid}, "started_at_ticks": 1 }}
}}
"#
        ),
    )
    .expect("write this deployment's own stopped-run snapshot");

    let (status, answer) = deployment.stats_answer();

    assert!(
        status.success(),
        "these figures were really produced in this directory, so reading them back is a \
         served request:\n{answer}"
    );
    assert!(
        answer.contains("frames-received=4210") && answer.contains("clip-write-failures=1"),
        "the last run's counters are what the operator asked for, including the failed clip \
         write that is only visible here:\n{answer}"
    );
    assert!(
        answer.contains("disk-full"),
        "and the fault that run ended in must still reach the operator:\n{answer}"
    );
    assert!(
        !answer.contains("health=ready") && !answer.contains("ingest=ok"),
        "but nothing is running now, so no line may assert readiness or healthy ingest:\n{answer}"
    );
    assert!(
        answer.contains("stopped"),
        "the answer must say these are a stopped run's figures, not a reading of anything \
         running now:\n{answer}"
    );
}

/// A snapshot file that exists and cannot be read back is not a directory
/// nothing has ever run in. The one fact such a file settles is that something
/// DID run here — so answering it with `nothing has run in this data directory
/// yet` and a block of zeros asserts the single condition the file rules out.
/// Nothing about the file can be attributed to any process, which is the
/// unattributable case, and the operator gets the refusal.
///
/// This arm is a truncated file: the bytes are valid text and are read fine,
/// and the JSON parse is what fails — the shape a crash outside the atomic
/// replace window, or a half-flushed disk, leaves behind.
#[test]
fn a_stats_read_of_a_truncated_snapshot_refuses_instead_of_claiming_nothing_ran() {
    let deployment = Deployment::with_config("site_name = \"home farm\"\n");
    fs::write(
        deployment.snapshot_path(),
        "{\n  \"frames_received\": 4210,\n  \"detector_invocation",
    )
    .expect("write a truncated snapshot");

    let (status, answer) = deployment.stats_answer();

    assert!(
        !answer.contains("nothing has run in this data directory yet"),
        "a file is sitting in the data directory, so something ran here — `vigil stats` must not \
         answer that nothing ever has:\n{answer}"
    );
    assert!(
        !answer.contains("stats-provenance=not-started"),
        "and it must not stamp the answer as a never-started directory, which is the one \
         condition this file rules out:\n{answer}"
    );
    assert!(
        answer.contains("no runtime owns")
            && answer.contains(&deployment.data_dir.display().to_string()),
        "nothing in an unreadable snapshot can be attributed to any process, so the answer is \
         the no-owner refusal, naming the directory it is talking about:\n{answer}"
    );
    assert!(
        !status.success(),
        "and it did not serve what was asked — there were no figures it could stand behind — so \
         the exit status has to say so:\n{answer}"
    );
}

/// The same promise on the other failure path: bytes that are not text at all,
/// so the read itself fails before any parse is attempted. A file that exists
/// and cannot even be read is still evidence that something ran here, and
/// still nothing that can be attributed to a process.
#[test]
fn a_stats_read_of_an_unreadable_snapshot_refuses_instead_of_claiming_nothing_ran() {
    let deployment = Deployment::with_config("site_name = \"home farm\"\n");
    // Not valid UTF-8, so reading the file to a string fails with an io error
    // rather than reaching the JSON parser at all.
    fs::write(
        deployment.snapshot_path(),
        [0xff_u8, 0xfe, 0x00, 0x9c, 0xff],
    )
    .expect("write an unreadable snapshot");

    let (status, answer) = deployment.stats_answer();

    assert!(
        !answer.contains("nothing has run in this data directory yet")
            && !answer.contains("stats-provenance=not-started"),
        "an unreadable file is still a file: this directory has been run in, and the answer may \
         not say otherwise:\n{answer}"
    );
    assert!(
        answer.contains("no runtime owns")
            && answer.contains(&deployment.data_dir.display().to_string()),
        "so the operator gets the refusal for a snapshot nothing can be attributed from:\n{answer}"
    );
    assert!(
        !answer.contains("no longer running"),
        "and it must not tell them the writer has stopped. This file's bytes settle nothing \
         about its writer, and the same refusal serves a permission-walled snapshot whose writer \
         may be running right now:\n{answer}"
    );
    assert!(
        !status.success(),
        "and the exit status says no figures were served:\n{answer}"
    );
}
