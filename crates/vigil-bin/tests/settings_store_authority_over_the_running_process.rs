//! A stored value governs the running process, over anything a surface said
//! before the store was opened.
//!
//! These settings already reach a consumer today — but they reach it through
//! the loader, which resolves the command line, the environment and the options
//! file BEFORE the store is opened. The database then records what the surfaces
//! already decided, and nothing a person sets with `vigil settings`, nothing a
//! management server pushes, can move them. That is the leg these tests pin:
//! the store is the source of truth, the surfaces are authors that write into
//! it, and the runtime resolves from it.
//!
//! Every test here is therefore a CONTEST, never an existence check. The
//! configuration file keeps saying one thing for the whole test while the store
//! is made to say another, so a run that still resolved from the file comes up
//! on the file's value and fails. And every assertion reads what the process
//! REPORTS AS RUNNING — what it actually brought into force — never what the
//! store holds, which a start that consumed nothing would still answer
//! correctly.
//!
//! Driven as real `vigil run` OS processes against one data directory, with the
//! contested value written through the real `vigil settings set` command
//! between two starts.

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

use vigil::settings_model::{
    Author, CAMERA_NAME_SETTING, DETECTOR_MODEL_PATH_SETTING, LIVE_RTSP_URL_SETTING,
    MOTION_SENSITIVITY_SETTING, RECOGNITION_COVERED_CLASSES_SETTING, RTSP_URL_SETTING,
    SITE_NAME_SETTING, Surface,
};
use vigil::settings_projection::{
    AUTHOR_KEY, NAME_KEY, NONE, RUNNING_KEY, SETTING_LINE_PREFIX, SURFACE_KEY, VALUE_KEY,
};
use vigil::settings_store::SettingsStore;

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    TcpPortReservation, capture_pipe, vigil_binary_path, wait_for_store_owner,
    wait_for_store_owner_to_release,
};

/// What the configuration file says for the whole of every test below — the
/// pre-store loader value the stored value has to beat.
const FILE_SITE_NAME: &str = "site-named-in-the-file";
const FILE_CAMERA_NAME: &str = "camera-named-in-the-file";

/// What an operator stores afterwards. Deliberately different from both the
/// file's value and Vigil's own choice, so neither can produce it by accident.
const STORED_SITE_NAME: &str = "site-the-operator-stored";
const STORED_CAMERA_NAME: &str = "camera-the-operator-stored";

struct Deployment {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    data_dir: PathBuf,
    config_path: PathBuf,
}

impl Deployment {
    /// A deployment whose configuration file already asserts `body`, so the
    /// loader has a value of its own for every setting under test.
    fn with_config(body: &str) -> Self {
        let tmp = tempfile::tempdir().expect("temporary deployment directory");
        let root = tmp.path().to_path_buf();
        let data_dir = root.join("data");
        fs::create_dir_all(&data_dir).expect("create the data directory");
        let config_path = root.join("vigil.toml");
        fs::write(&config_path, body).expect("write the configuration file");
        Self {
            _tmp: tmp,
            root,
            data_dir,
            config_path,
        }
    }

    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, contents).expect("write a deployment file");
        path
    }

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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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
            .expect("spawn `vigil settings`")
    }

    fn set(&self, setting: &str, value: &str) {
        let output = self.settings(&["set", setting, value]);
        assert!(
            output.status.success(),
            "`vigil settings set {setting} {value}` must be accepted — the store is the source of \
             truth for this value, so refusing the write leaves the operator with a value they can \
             see and cannot choose; stdout:\n{}\nstderr:\n{}",
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

    /// Start once so the configuration file authors its own record, store the
    /// contested value, and start again. What the second run reports as running
    /// is the whole question.
    fn contest(&self, setting: &str, stored: &str) -> Contest {
        let first = self.start();
        let first_output = format!("{}\n{}", first.stdout(), first.stderr());
        first.stop();
        self.wait_for_shutdown();

        self.set(setting, stored);

        let second = self.start();
        let listing = self.listing();
        let second_output = format!("{}\n{}", second.stdout(), second.stderr());
        second.stop();
        self.wait_for_shutdown();

        Contest {
            first_output,
            second_output,
            listing,
        }
    }
}

struct Contest {
    first_output: String,
    second_output: String,
    listing: String,
}

struct LiveVigil {
    child: Child,
    stdout: std::sync::Arc<std::sync::Mutex<String>>,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
}

impl LiveVigil {
    fn stdout(&self) -> String {
        self.stdout.lock().expect("stdout lock").clone()
    }

    fn stderr(&self) -> String {
        self.stderr.lock().expect("stderr lock").clone()
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
        .unwrap_or_else(|| {
            panic!(
                "a value an operator can set is a value the operator surface answers for; there is \
                 no {name} line in:\n{rendered}"
            )
        })
}

/// The shared shape of every contest below: the stored value is the effective
/// one, it is attributed to the operator and the surface they used, and — the
/// leg these tests exist for — it is what the process brought into force.
fn assert_stored_value_governs(contest: &Contest, setting: &str, stored: &str) {
    let line = setting_line(&contest.listing, setting);
    assert_eq!(
        token_field(line, VALUE_KEY),
        Some(stored),
        "the effective value is what the operator stored, not what the configuration file still \
         says: {line}"
    );
    assert_eq!(
        token_field(line, AUTHOR_KEY),
        Some(Author::LocalExplicit.as_str()),
        "attributed to the person who set it: {line}"
    );
    assert_eq!(
        token_field(line, SURFACE_KEY),
        Some(Surface::VigilSettings.as_str()),
        "and to the surface they used, rather than to the file it outranks: {line}"
    );
    assert_ne!(
        token_field(line, RUNNING_KEY),
        Some(NONE),
        "the process has to report what it is actually using for {setting}; reporting nothing \
         means nothing consumed the resolved value, so the store governs the record and not the \
         machine: {line}\n\nRun output:\n{}",
        contest.second_output
    );
    assert_eq!(
        token_field(line, RUNNING_KEY),
        Some(stored),
        "and what it brought into force is the stored value — a run still resolving this from the \
         file it read before the store opened comes up on the file's value: {line}\n\nRun \
         output:\n{}",
        contest.second_output
    );
}

/// Unfakeable: the configuration file names the site for the whole test, and
/// the file's own record is authored by the first run, so both values are
/// present in the store when the second run resolves. A run that consumed the
/// loader's merged value reports the file's name.
#[test]
fn a_stored_site_name_governs_the_running_process_over_the_configuration_file() {
    let deployment =
        Deployment::with_config(&format!("cameras = []\nsite_name = \"{FILE_SITE_NAME}\"\n"));
    let contest = deployment.contest(SITE_NAME_SETTING, STORED_SITE_NAME);
    assert_stored_value_governs(&contest, SITE_NAME_SETTING, STORED_SITE_NAME);
}

/// Unfakeable for the same reason as the site name, and it matters more here:
/// the camera name is what an operator reads on every event this node reports,
/// so a name that only exists in the store is a name nobody ever sees.
#[test]
fn a_stored_camera_name_governs_the_running_process_over_the_configuration_file() {
    let deployment = Deployment::with_config(&format!(
        "cameras = []\ncamera_name = \"{FILE_CAMERA_NAME}\"\n"
    ));
    let contest = deployment.contest(CAMERA_NAME_SETTING, STORED_CAMERA_NAME);
    assert_stored_value_governs(&contest, CAMERA_NAME_SETTING, STORED_CAMERA_NAME);
}

/// Unfakeable because which model file is loaded changes what Vigil detects:
/// both paths exist on disk, so a run cannot be reporting the stored one merely
/// because the file's one was unusable.
#[test]
fn a_stored_detector_model_path_governs_the_running_process_over_the_configuration_file() {
    let deployment = Deployment::with_config("cameras = []\n");
    let from_file = deployment.file("model-named-in-the-file", "");
    let stored = deployment.file("model-the-operator-stored", "");
    fs::write(
        &deployment.config_path,
        format!(
            "cameras = []\ndetector_model_path = \"{}\"\n",
            from_file.display()
        ),
    )
    .expect("rewrite the configuration file with the model path");

    let contest = deployment.contest(DETECTOR_MODEL_PATH_SETTING, &stored.display().to_string());
    assert_stored_value_governs(
        &contest,
        DETECTOR_MODEL_PATH_SETTING,
        &stored.display().to_string(),
    );
}

/// Unfakeable because the two lists share no member: a run that merged them, or
/// that kept the file's list, cannot report the stored one.
#[test]
fn a_stored_recognition_class_list_governs_the_running_process_over_the_configuration_file() {
    let deployment =
        Deployment::with_config("cameras = []\nrecognition_covered_classes = [\"person\"]\n");
    let contest = deployment.contest(RECOGNITION_COVERED_CLASSES_SETTING, "dog,cat");
    assert_stored_value_governs(&contest, RECOGNITION_COVERED_CLASSES_SETTING, "dog,cat");
}

/// Unfakeable because motion sensitivity is the sharpest case in this file: it
/// can be written, resolved and displayed today and reaches nothing at all, so
/// an operator who turns it down watches the machine ignore them. The proof is
/// the process reporting what it is running it at — a value nothing consumes
/// has nothing to report.
#[test]
fn a_stored_motion_sensitivity_reaches_the_running_process() {
    let deployment = Deployment::with_config("cameras = []\n");
    let contest = deployment.contest(MOTION_SENSITIVITY_SETTING, "8");
    let line = setting_line(&contest.listing, MOTION_SENSITIVITY_SETTING);
    assert_eq!(
        token_field(line, VALUE_KEY),
        Some("8"),
        "the effective value is what the operator stored: {line}"
    );
    assert_eq!(
        token_field(line, RUNNING_KEY),
        Some("8"),
        "and the process reports running it: a setting that is writable, resolvable and \
         displayable while changing nothing is a hidden non-knob — the operator turned the \
         sensitivity down and the machine never heard it: {line}\n\nRun output:\n{}",
        contest.second_output
    );
}

/// Unfakeable because the runtime names the stream it opened in its own startup
/// output: the stored endpoint has a path segment the file's endpoint does not,
/// so a run that dialled the file's camera cannot print it. The endpoints are
/// deliberately not served by anything — which camera the process brought up is
/// the question, and a stream that never connects still names itself.
#[test]
fn a_stored_camera_stream_is_the_one_the_running_process_brings_up() {
    let file_endpoint = "rtsp://127.0.0.1:1/named-in-the-file";
    let stored_endpoint = "rtsp://127.0.0.1:1/the-operator-stored-this";
    let deployment = Deployment::with_config(&format!(
        "cameras = []\ncamera_name = \"{FILE_CAMERA_NAME}\"\nrtsp_url = \"{file_endpoint}\"\n"
    ));

    let contest = deployment.contest(RTSP_URL_SETTING, stored_endpoint);

    assert!(
        contest.first_output.contains("named-in-the-file"),
        "sanity: the first run brought up the camera the file named, so the second run's answer is \
         a genuine change rather than the only thing that ever happened. Output:\n{}",
        contest.first_output
    );
    assert!(
        contest.second_output.contains("the-operator-stored-this"),
        "the camera the process brings up is the one the store names. A run that resolved this \
         from the file it read before the store opened dials the file's camera, and the operator's \
         change is a record nobody honors. Output:\n{}",
        contest.second_output
    );
    assert!(
        !contest.second_output.contains("named-in-the-file"),
        "and it does not also bring up the endpoint it was told to stop using — a run honoring \
         both is honoring neither. Output:\n{}",
        contest.second_output
    );
}

/// Unfakeable because the live-view endpoint is what a person watches rather
/// than what the detector analyses, so it has its own consumer and its own way
/// of being ignored. The operator surface answers for it as a SOURCE, never as
/// a value — the spelling can carry the camera's credentials — so what is
/// asserted is that the process has a running answer for it at all and that it
/// is attributed to the store rather than to the file it outranks.
#[test]
fn a_stored_live_view_endpoint_is_what_the_running_process_serves() {
    let file_endpoint = "rtsp://127.0.0.1:1/live-named-in-the-file";
    let stored_endpoint = "rtsp://127.0.0.1:1/live-the-operator-stored";
    let deployment = Deployment::with_config(&format!(
        "cameras = []\ncamera_name = \"{FILE_CAMERA_NAME}\"\n\
         rtsp_url = \"rtsp://127.0.0.1:1/analysis\"\nlive_rtsp_url = \"{file_endpoint}\"\n"
    ));

    let contest = deployment.contest(LIVE_RTSP_URL_SETTING, stored_endpoint);

    let line = setting_line(&contest.listing, LIVE_RTSP_URL_SETTING);
    assert_eq!(
        token_field(line, SURFACE_KEY),
        Some(Surface::VigilSettings.as_str()),
        "the effective live endpoint is the one the operator stored, attributed to the surface \
         they used rather than to the file it outranks: {line}"
    );
    assert_ne!(
        token_field(line, RUNNING_KEY),
        Some(NONE),
        "and the process reports what it is actually serving the live view from; reporting nothing \
         means the stored endpoint reached no consumer and the person watching is still being \
         shown the file's camera: {line}\n\nRun output:\n{}",
        contest.second_output
    );
}

/// The four settings above are the ones that DO reach a consumer today, through
/// the pre-store loader. This names them together so a later change that quietly
/// drops one from the store-authoritative path is a failure here rather than a
/// silent regression in a file nobody re-reads.
#[test]
fn every_setting_the_loader_reaches_is_also_answerable_from_the_store_alone() {
    let deployment = Deployment::with_config("cameras = []\n");
    let run = deployment.start();
    let listing = deployment.listing();
    run.stop();

    for setting in [
        SITE_NAME_SETTING,
        CAMERA_NAME_SETTING,
        DETECTOR_MODEL_PATH_SETTING,
        RECOGNITION_COVERED_CLASSES_SETTING,
        MOTION_SENSITIVITY_SETTING,
        RTSP_URL_SETTING,
        LIVE_RTSP_URL_SETTING,
    ] {
        let line = setting_line(&listing, setting);
        assert_ne!(
            token_field(line, RUNNING_KEY),
            Some(NONE),
            "a running node knows what it is running {setting} at; a blank running field on a live \
             process means the value reached no consumer: {line}"
        );
    }
}
