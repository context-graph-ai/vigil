//! What an operator gets from `why`, `events` and `stats` cannot change because
//! the answer took a different road home.
//!
//! Vigil's read commands ask the process that owns the store first and read the
//! store directly when nobody owns it. That dispatch is the promise: the same
//! command, the same answer, whether or not Vigil is running. The road the
//! owner's answer travels is moving off vigil's own unix socket and onto the
//! context-graph store's owner channel, addressed by the store path alone, and
//! an operator must not be able to tell from the answer that anything moved:
//! the same fields, the same bytes under the marker, the same exit status, and
//! the same fallback when there is no owner to ask.
//!
//! Nothing here waits on a clock. Readiness is proven by asking the owner a
//! real question and getting an owner-served answer back; the fallback leg is
//! proven after the owning process is reaped, so the answer cannot have come
//! from it.
//!
//! The route marker itself is held to its ratified bytes here, not questioned:
//! route reporting is part of what this change must leave untouched, so the
//! marker is read through `OWNER_SERVED_PREFIX` and its spelling is whatever
//! that constant says it is.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use vigil::OWNER_SERVED_PREFIX;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    RUNTIME_STARTUP_TIMEOUT, TcpPortReservation, capture_pipe, fresh_store_copy, vigil_binary_path,
    wait_until,
};

/// The observations a seeded deployment carries, so `events` has rows to
/// render and `why --latest` has a chain to walk.
const SEEDED_DETECTIONS: usize = 2;

/// Every field `format_events_cli` renders for one row.
const EVENTS_FIELDS: [&str; 9] = [
    "observation_id=",
    "observed_at=",
    "camera=",
    "class=",
    "confidence=",
    "bbox=",
    "frame_index=",
    "clip=",
    "detector_image=",
];

/// Every field `format_why_cli` renders on its selection line.
const WHY_FIELDS: [&str; 19] = [
    "selection=",
    "observation_id=",
    "observed_at=",
    "class=",
    "confidence=",
    "bbox=",
    "frame_index=",
    "clip_ref=",
    "detector_image_ref=",
    "camera_id=",
    "camera_name=",
    "camera_rtsp_url=",
    "context_id=",
    "site_name=",
    "decision_id=",
    "intention_id=",
    "intention_description=",
    "model_id=",
    "threshold=",
];

/// The counters `format_stats` renders. Named here so a stats answer served by
/// the owner is checked against the same roster
/// `first_light_loop.rs::vigil_stats_before_any_frame_is_all_zero_no_panic`
/// checks a direct one against.
const STATS_FIELDS: [&str; 5] = [
    "frames-received",
    "detections-emitted",
    "observations-written",
    "stream-drops",
    "processing-lag-ms",
];

/// A deployment whose store already holds detections, so every read command
/// has something real to answer with — the seeded fixture the rest of the
/// estate uses, opened and closed before any runtime touches it.
struct SeededDeployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    config_path: PathBuf,
}

impl SeededDeployment {
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
            config_path,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        vigil_command(&self.data_dir, args)
    }

    /// Start `vigil run` and wait for it to own the store, WITHOUT reading the
    /// store while it comes up.
    ///
    /// Readiness cannot be probed with `vigil stats` here: until the runtime
    /// owns the store, every read command falls back to a direct read, and a
    /// direct reader hydrating the store makes the runtime's own writable open
    /// fail (`1 direct readers are hydrating this store`) — the probe would be
    /// the thing that stopped the runtime from ever becoming ready. So the
    /// process's own boot line is the signal, and only once it is up is the
    /// owner asked anything.
    fn start(&self) -> LiveVigil {
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
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn `vigil run`");
        // Drain both pipes so a full one can never stall the child.
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

        // Up, and holding the store: from here a read command reaches the owner
        // instead of opening the store itself, so asking is safe.
        wait_until(
            "the running owner to serve a store-backed read",
            RUNTIME_STARTUP_TIMEOUT,
            || {
                let asked = vigil_command(&self.data_dir, &["events"]);
                Ok(
                    (owner_served(&asked) && !body_of(&asked).starts_with("owner-error"))
                        .then_some(()),
                )
            },
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}. A runtime that reports itself up must answer a store-backed read \
                 through the owner route. stdout:\n{}\nstderr:\n{}",
                read(&stdout),
                read(&stderr)
            )
        });
        live
    }
}

struct LiveVigil {
    child: Child,
}

impl LiveVigil {
    /// Reap the owner before the fallback leg is asked anything, so a direct
    /// read cannot have been served by a process that was still alive.
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

fn vigil_command(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(vigil_binary_path())
        .args(args)
        .env("VIGIL_DATA_DIR", data_dir)
        .env_remove("VIGIL_STORE_PATH")
        // Removing a variable that is already unset is a no-op, and this one
        // must never be inherited from whatever shell ran the suite: while it
        // still exists it would reroute the command, and once it is retired it
        // is a hard refusal.
        .env_remove("VIGIL_CONTROL_SOCKET")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("spawn `vigil {}`: {error}", args.join(" ")))
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn owner_served(output: &Output) -> bool {
    output.status.success() && stdout_of(output).starts_with(OWNER_SERVED_PREFIX)
}

/// The answer with the route marker stripped — what the operator reads, minus
/// the line saying who served it.
fn body_of(output: &Output) -> String {
    stdout_of(output)
        .strip_prefix(OWNER_SERVED_PREFIX)
        .map(str::to_string)
        .unwrap_or_else(|| stdout_of(output))
}

fn assert_fields(label: &str, body: &str, fields: &[&str]) {
    for field in fields {
        assert!(
            body.contains(field),
            "the owner-served `{label}` answer dropped `{field}` — an operator reading a \
             running Vigil must get the same fields a stopped one prints; got:\n{body}"
        );
    }
}

/// Unfakeable because a second process genuinely cannot open a store the
/// running owner holds: the answer can only have come from the owner, and the
/// marker says which route served it.
#[test]
fn why_events_and_stats_answer_over_the_live_route_while_the_runtime_owns_the_store() {
    let deployment = SeededDeployment::new();
    let live = deployment.start();

    let events = deployment.run(&["events"]);
    let why = deployment.run(&["why", "--latest"]);
    let stats = deployment.run(&["stats"]);
    live.stop();

    for (label, output) in [("events", &events), ("why", &why), ("stats", &stats)] {
        assert!(
            output.status.success(),
            "`vigil {label}` must answer while the runtime owns the store; exited {:?}, \
             stderr:\n{}",
            output.status.code(),
            stderr_of(output)
        );
        assert!(
            stdout_of(output).starts_with(OWNER_SERVED_PREFIX),
            "`vigil {label}` was not served by the running owner — it fell back to a direct read \
             the store lock should have refused; got:\n{}",
            stdout_of(output)
        );
    }

    assert_fields("events", &body_of(&events), &EVENTS_FIELDS);
    assert_fields("why", &body_of(&why), &WHY_FIELDS);
    assert_fields("stats", &body_of(&stats), &STATS_FIELDS);
}

/// The preservation pin proper: the same deployment, the same store, asked
/// once through the owner and once directly, must print the same bytes.
#[test]
fn the_live_answer_is_byte_for_byte_the_direct_answer_under_the_route_marker() {
    let deployment = SeededDeployment::new();
    let live = deployment.start();
    let live_events = deployment.run(&["events"]);
    let live_why = deployment.run(&["why", "--latest"]);
    live.stop();

    let direct_events = deployment.run(&["events"]);
    let direct_why = deployment.run(&["why", "--latest"]);

    for (label, live_output, direct_output) in [
        ("events", &live_events, &direct_events),
        ("why", &live_why, &direct_why),
    ] {
        assert!(
            !stdout_of(direct_output).contains("served-by="),
            "with the owner reaped, `vigil {label}` cannot have been served by one; got:\n{}",
            stdout_of(direct_output)
        );
        assert_eq!(
            body_of(live_output),
            stdout_of(direct_output),
            "`vigil {label}` printed different bytes over the two routes — the transport is not \
             allowed to change what the operator reads"
        );
        assert_eq!(
            live_output.status.code(),
            direct_output.status.code(),
            "`vigil {label}` exited differently over the two routes"
        );
    }
}

/// The fallback leg. The owner is gone before anything is asked, so the answer
/// cannot have come over the owner route — and the marker's absence is
/// asserted, not assumed.
#[test]
fn every_read_command_falls_back_to_the_direct_path_when_no_runtime_owns_the_store() {
    let deployment = SeededDeployment::new();

    for (label, args, fields) in [
        ("events", vec!["events"], EVENTS_FIELDS.as_slice()),
        ("why", vec!["why", "--latest"], WHY_FIELDS.as_slice()),
        ("stats", vec!["stats"], STATS_FIELDS.as_slice()),
    ] {
        let output = deployment.run(&args);
        assert!(
            output.status.success(),
            "`vigil {label}` must answer by direct read when nothing owns the store; exited \
             {:?}, stderr:\n{}",
            output.status.code(),
            stderr_of(&output)
        );
        let rendered = stdout_of(&output);
        assert!(
            !rendered.contains("served-by="),
            "nothing owns this store, so `vigil {label}` cannot claim an owner served it; \
             got:\n{rendered}"
        );
        assert_fields(label, &rendered, fields);
    }
}
