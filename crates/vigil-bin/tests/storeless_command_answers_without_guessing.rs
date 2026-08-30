//! A `vigil settings` command whose store cannot be read must not fill the
//! silence with a manufactured deployment.
//!
//! Under the owner ruling of 2026-08-25 (folded into
//! `vigil-settings-autority-direction.md`) such a command says the selected
//! store is unreadable, says exact live settings and this node's identity are
//! unavailable through it, and points at the running service's own output. The
//! danger is not that it says too little — it is that a report built for the
//! RUN, which legitimately manufactures an automatic floor and a domain roster
//! from nothing, gets reused by a process that knows nothing. Every one of
//! those lines then reads as this deployment's live state, and none of them is.
//!
//! What makes that lie expensive rather than merely untidy: the deployment
//! under test is started with values that are NOT the product's defaults, so
//! the floor a guessing report renders is visibly a different deployment's —
//! and an operator diagnosing a node they have just been told is unreadable
//! has no way to tell a manufactured floor from a real reading.
//!
//! Filtering is checked because it is where an explanation is easiest to lose:
//! `settings find <text>` and `settings identity` narrow the answer, and an
//! implementation that filters the report AFTER assembling it drops the one
//! line that made the rest honest. The explanation must survive every request
//! shape, not just the bare one.
//!
//! Unfakeable: the store genuinely cannot be opened — real bytes that are not
//! a store, permission bits cleared — so nothing on the store-backed path can
//! produce any of this; the runtime is a separate operating-system process
//! that really is up and really does hold the configured values, so the values
//! the command must not invent are known independently; and every assertion is
//! on the operator's own rendered lines rather than on any internal shape.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use vigil::settings_projection::{
    DOMAIN_LINE_PREFIX, HELD_LINE_PREFIX, IDENTITY_LINE_PREFIX, SECRET_LINE_PREFIX,
    SETTING_LINE_PREFIX, UNAVAILABLE_LINE_PREFIX, UNMANAGED_LINE_PREFIX,
};
use vigil::settings_store::SettingsStore;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_for_tcp_port, workspace_root,
};

/// Startup values that are NOT the product's defaults. A guessing report
/// renders the automatic floor, which carries the defaults — so if these are
/// what the running node holds, any answer showing the defaults is provably
/// not a reading of this deployment.
const CONFIGURED_SAMPLE_FRAMES: &str = "3";
const CONFIGURED_CONFIDENCE: &str = "0.75";
const CONFIGURED_SITE: &str = "west orchard";

/// The lines a command that cannot read the store must not produce. Each one
/// states something only a real reading could know.
const GUESSED_STATE_PREFIXES: [&str; 5] = [
    SETTING_LINE_PREFIX,
    HELD_LINE_PREFIX,
    DOMAIN_LINE_PREFIX,
    IDENTITY_LINE_PREFIX,
    SECRET_LINE_PREFIX,
];

fn fixture_detector_model_path() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("fixtures")
        .join("models")
        .join("yolox-tiny-coco.pth")
}

struct UnreadableDeployment {
    _tmp: tempfile::TempDir,
    data_dir: PathBuf,
    store_path: PathBuf,
    config_path: PathBuf,
}

impl UnreadableDeployment {
    fn prepare() -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment tree");
        let data_dir = tmp.path().join("orchard-node");
        fs::create_dir_all(&data_dir).expect("deployment directory");
        let store_path = SettingsStore::store_path(&data_dir);
        fs::write(
            &store_path,
            b"this file exists and is not a store: a corrupt volume, not a first start\n",
        )
        .expect("write the corrupt store");
        fs::set_permissions(&store_path, fs::Permissions::from_mode(0o000))
            .expect("clear the store's permission bits");

        let config_path = tmp.path().join("vigil.toml");
        fs::write(
            &config_path,
            format!(
                "site_name = \"{CONFIGURED_SITE}\"\n\
                 detector_sample_frames = {CONFIGURED_SAMPLE_FRAMES}\n\
                 detector_confidence_threshold = {CONFIGURED_CONFIDENCE}\n\
                 cameras = []\n"
            ),
        )
        .expect("write the configuration");

        Self {
            _tmp: tmp,
            data_dir,
            store_path,
            config_path,
        }
    }

    /// Start the node. It comes up degraded — its store cannot be opened — and
    /// it holds the configured values, which is what makes the command's
    /// silence below a refusal to guess rather than an absence of anything to
    /// guess AT.
    fn start(&self) -> DegradedRun {
        let health = TcpPortReservation::reserve_loopback().expect("reserve a liveness port");
        let review = TcpPortReservation::reserve_loopback().expect("reserve a review port");
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
            .env_remove("VIGIL_STORE_PATH")
            .env_remove("VIGIL_CONTROL_SOCKET")
            .env_remove("VIGIL_SERVICE_ID")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        health.release();
        review.release();
        let mut child = command.spawn().expect("spawn `vigil run`");
        let stdout = capture_pipe(child.stdout.take());
        let _stderr = capture_pipe(child.stderr.take());
        let run = DegradedRun { child, stdout };
        wait_for_tcp_port(health_port, Duration::from_secs(30)).unwrap_or_else(|error| {
            panic!(
                "{error}. An unreadable store must not stop Vigil starting. Output so far:\n{}",
                run.logs()
            )
        });
        wait_for_tcp_port(review_port, Duration::from_secs(30))
            .unwrap_or_else(|error| panic!("{error}. Output so far:\n{}", run.logs()));
        run
    }

    /// A separate `vigil settings` against this deployment — a different
    /// process, with no route to the run, aimed at the store the operator
    /// configured.
    fn settings(&self, request: &[&str]) -> Output {
        let mut args = vec!["settings"];
        args.extend_from_slice(request);
        Command::new(vigil_binary_path())
            .args(&args)
            .env("VIGIL_DATA_DIR", &self.data_dir)
            .env("VIGIL_STORE_PATH", &self.store_path)
            .env_remove("VIGIL_CONTROL_SOCKET")
            .env_remove("VIGIL_SERVICE_ID")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("spawn vigil settings {request:?}: {error}"))
    }
}

struct DegradedRun {
    child: Child,
    stdout: std::sync::Arc<std::sync::Mutex<String>>,
}

impl DegradedRun {
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

impl Drop for DegradedRun {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn rendered(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// What every answer shape owes, checked the same way each time so no request
/// can be honest in one form and guessing in another.
fn assert_answers_without_guessing(label: &str, answer: &str, store_path: &std::path::Path) {
    let explanation = answer
        .lines()
        .find(|line| line.starts_with(&format!("{UNAVAILABLE_LINE_PREFIX} ")))
        .unwrap_or_else(|| {
            panic!(
                "{label}: the explanation is what makes every other line in this answer readable, \
                 so it must survive whatever narrowed the request — an implementation that \
                 filtered the report after assembling it drops exactly this line; got:\n{answer}"
            )
        });
    assert!(
        explanation.contains(&store_path.display().to_string()),
        "{label}: the explanation must name the store the operator repairs; got: {explanation}"
    );
    assert!(
        !explanation.contains("   "),
        "{label}: the explanation renders with a run of embedded whitespace, which reads as a \
         broken line to the operator it is written for; got: {explanation}"
    );
    assert!(
        answer
            .lines()
            .any(|line| line.starts_with(&format!("{UNMANAGED_LINE_PREFIX} "))),
        "{label}: the promised unmanaged line is the other half of the answer and must stay; \
         got:\n{answer}"
    );

    for prefix in GUESSED_STATE_PREFIXES {
        let guessed: Vec<&str> = answer
            .lines()
            .filter(|line| line.starts_with(&format!("{prefix} ")))
            .collect();
        assert!(
            guessed.is_empty(),
            "{label}: a process that cannot read the store cannot know any of this, so a \
             `{prefix}` line states something it made up — and an operator reading it alongside \
             the explanation has no way to tell which half is real: {guessed:#?}"
        );
    }
}

/// Unfakeable because the running node holds values that are not the product's
/// defaults: an answer rendering the automatic floor is provably not a reading
/// of this deployment, whatever it claims. The bare request, the list request,
/// a searched request and an identity request are all checked, because
/// filtering is where an explanation is easiest to lose.
#[test]
fn a_command_that_cannot_read_the_store_answers_without_inventing_this_deployments_state() {
    let deployment = UnreadableDeployment::prepare();
    let run = deployment.start();

    let requests: [(&str, Vec<&str>); 4] = [
        ("the bare listing", vec![]),
        ("an explicit list", vec!["list"]),
        ("a searched read", vec!["find", "detection"]),
        ("an identity read", vec!["identity"]),
    ];
    let answers: Vec<(&str, String)> = requests
        .iter()
        .map(|(label, request)| (*label, rendered(&deployment.settings(request))))
        .collect();
    let logs = run.logs();
    run.stop();

    for (label, answer) in &answers {
        assert_answers_without_guessing(label, answer, &deployment.store_path);
        for configured in [CONFIGURED_SAMPLE_FRAMES, CONFIGURED_CONFIDENCE] {
            assert!(
                !answer.contains(&format!("={configured}")),
                "{label}: this answer carries {configured:?}, which this deployment really is \
                 running — so it was either read out of a store that cannot be opened or fetched \
                 from the run through a route that does not exist; got:\n{answer}\nlogs:\n{logs}"
            );
        }
    }
}

/// The identity half on its own, unconditionally: no answer shape, filtered or
/// not, may carry an identity line.
///
/// A command with an unreadable store could only produce one by deriving it
/// from the directory it was handed or by guessing at a configuration nobody
/// named it. That is not a vaguer answer than the truth — it is a DIFFERENT
/// node's answer, with nothing on the line to say so.
#[test]
fn no_answer_shape_carries_an_identity_when_the_store_cannot_be_read() {
    let deployment = UnreadableDeployment::prepare();
    let run = deployment.start();

    let shapes: [Vec<&str>; 4] = [
        vec![],
        vec!["list"],
        vec!["identity"],
        vec!["find", "detection"],
    ];
    let answers: Vec<(Vec<&str>, String)> = shapes
        .iter()
        .map(|request| (request.clone(), rendered(&deployment.settings(request))))
        .collect();
    let logs = run.logs();
    run.stop();

    let directory_name = deployment
        .data_dir
        .file_name()
        .expect("the data directory has a name")
        .to_string_lossy()
        .to_string();
    for (request, answer) in &answers {
        assert!(
            !answer
                .lines()
                .any(|line| line.starts_with(&format!("{IDENTITY_LINE_PREFIX} "))),
            "`vigil settings {}` answered with an identity out of a store it cannot read; \
             got:\n{answer}\nlogs:\n{logs}",
            request.join(" ")
        );
        let without_the_path = answer.replace(&deployment.store_path.display().to_string(), "<s>");
        assert!(
            !without_the_path.contains(&directory_name),
            "`vigil settings {}` named this node after the directory it was handed, outside the \
             store path it is required to name; got:\n{answer}",
            request.join(" ")
        );
        for word in CONFIGURED_SITE.split_whitespace() {
            assert!(
                !without_the_path.contains(word),
                "`vigil settings {}` produced the configured site name out of a configuration it \
                 was never given; got:\n{answer}",
                request.join(" ")
            );
        }
    }
}

/// How one read shape is delivered: the status a caller reads, and the stream
/// the answer arrives on. Both are read BEFORE a word of the text is, because
/// both are read that way by the thing that consumes this surface — a script,
/// a supervisor, a Home Assistant add-on log — and neither is recoverable from
/// prose.
struct Delivery {
    label: &'static str,
    request: &'static [&'static str],
    /// The process status this shape exits with.
    exit_code: i32,
    /// Whether the answer belongs on standard error.
    on_standard_error: bool,
    /// Why this shape is delivered that way, in the words the failure prints.
    because: &'static str,
}

/// A read that could not be served is a FAILED request, and a read that was
/// served is not — whatever either one has to say. `identity` is the shape
/// where that distinction is load-bearing: an operator asks a node what it is
/// called, the command cannot see the store, and an answer that leaves on
/// standard output with status 0 tells every script that consumed it that the
/// lookup SUCCEEDED and this node has no identity. The other three shapes are
/// listed with the delivery they have today, so that a change to any of them
/// is a decision somebody makes rather than a side effect of moving this one.
const DELIVERIES: [Delivery; 4] = [
    Delivery {
        label: "the bare listing",
        request: &[],
        exit_code: 0,
        on_standard_error: false,
        because: "the listing is served — the unmanaged condition and the reason the store \
                  cannot be read are what the operator asked for and what they got",
    },
    Delivery {
        label: "an explicit list",
        request: &["list"],
        exit_code: 0,
        on_standard_error: false,
        because: "`list` is the bare listing spelled out, so it is delivered identically",
    },
    Delivery {
        label: "a searched read",
        request: &["find", "detection"],
        exit_code: 0,
        on_standard_error: false,
        because: "a narrowed listing is still a listing; narrowing changes what is shown, \
                  never whether the request was served",
    },
    Delivery {
        label: "an identity read",
        request: &["identity"],
        exit_code: 2,
        on_standard_error: true,
        because: "an operator asked this node what it is called and no answer exists to give \
                  them; a status of 0 on standard output says the lookup succeeded and this \
                  deployment has no identity, which is a different and false statement, and \
                  every script reading this surface believes it",
    },
];

/// The honest explanation, wherever it is delivered: the line naming the store
/// the operator repairs, and the unmanaged line beside it.
fn carries_the_explanation(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with(&format!("{UNAVAILABLE_LINE_PREFIX} ")))
}

/// Unfakeable because the status comes from a real process's exit and the two
/// streams are captured separately from that same process — neither is a shape
/// this test chose, and a rendering that merges them cannot make this pass.
#[test]
fn each_read_shape_keeps_its_own_status_and_stream_when_the_store_cannot_be_read() {
    let deployment = UnreadableDeployment::prepare();
    let run = deployment.start();

    let observed: Vec<(&Delivery, Output)> = DELIVERIES
        .iter()
        .map(|delivery| (delivery, deployment.settings(delivery.request)))
        .collect();
    let logs = run.logs();
    run.stop();

    for (delivery, output) in &observed {
        let Delivery {
            label,
            exit_code,
            on_standard_error,
            because,
            ..
        } = delivery;
        let out = String::from_utf8_lossy(&output.stdout).to_string();
        let err = String::from_utf8_lossy(&output.stderr).to_string();

        assert_eq!(
            output.status.code(),
            Some(*exit_code),
            "{label}: {because}. standard output was:\n{out}\nstandard error was:\n{err}\n\
             the deployment's own output was:\n{logs}"
        );

        let (carrier, quiet, carrier_name, quiet_name) = if *on_standard_error {
            (&err, &out, "standard error", "standard output")
        } else {
            (&out, &err, "standard output", "standard error")
        };
        assert!(
            carries_the_explanation(carrier),
            "{label}: {because}, so the explanation belongs on {carrier_name}; {carrier_name} \
             was:\n{carrier}\nand {quiet_name} was:\n{quiet}"
        );
        assert!(
            !quiet.contains(UNAVAILABLE_LINE_PREFIX) && !quiet.contains(UNMANAGED_LINE_PREFIX),
            "{label}: the answer must arrive on {carrier_name} alone — an operator who \
             redirected one stream away must not still be reading half of it on the other; \
             {quiet_name} was:\n{quiet}"
        );
        assert!(
            carrier.contains(&deployment.store_path.display().to_string()),
            "{label}: whatever stream carries this answer must name the store the operator \
             repairs; {carrier_name} was:\n{carrier}"
        );
    }
}
