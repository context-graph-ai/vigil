use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use chrono::{DateTime, Utc};
use context_graph::{
    AuditFilter, AuditTarget, Context, CreateContext, CreateEntity, Decision, EmbedderConfig,
    Entity, EntityType, EvidenceId, EvidenceKind, EvidenceProducer, EvidenceRef, Intention,
    ListEntityFilter, Observation, ObservationId, RecordObservation, RetentionStatus, Store,
    StoreConfig,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const SITE_NAME: &str = "home farm";
const CAMERA_NAME: &str = "lower gate";
const CAMERA_RTSP_URL: &str = "rtsp://127.0.0.1:8554/lower-gate";
const RTSP_AUTH_USERNAME: &str = "viewer";
const RTSP_AUTH_PASSWORD: &str = "viewpass";
const DETECTOR_MODEL_ID: &str = "yolox-tiny-burn-cpu";
const DETECTOR_THRESHOLD: f64 = 0.5;
const BASELINE_INTENTION_DESCRIPTION: &str = "watch lower gate";
const DETECTOR_ARTIFACT_SHA256: &str =
    "9de513de589ac98bb92d3bca53b5af7b9acfa9b0bacb831f7999d0f7afaee8f0";
const PERSON_MODEL_BACKEND: &str = "burn-yolox-tiny-cpu";
const PERSON_MODEL_FORWARD_SHA256: &str =
    "dfbadc5001668fd31242eb2c39877229f586d02d37eae150f9d8fb8d78a2b3b9";
const PERSON_NMS_SHA256: &str = "dfbadc5001668fd31242eb2c39877229f586d02d37eae150f9d8fb8d78a2b3b9";
const PERSON_RESULT_SHA256: &str =
    "acb5da2a172ff09ea32aca516d5f9253214a437d0b74bbbc2a16d6b083b4b631";
const PERSON_GOLDEN_BBOX: &str = "315,10,439,581";
const PERSON_GOLDEN_CONFIDENCE: f64 = 0.631198;
const PERSON_GOLDEN_CONFIDENCE_TOLERANCE: f64 = 0.03;
const CG_READ_PROBE_ENV: &str = "CG_READ_PROBE_PATH";
const CG_READ_PROBE_NONCE_ENV: &str = "CG_READ_PROBE_NONCE";
const CG_READ_OBSERVER_TRAIT: &str = "StoreReadObserver";
const CG_READ_EVENT_TYPE: &str = "StoreReadEvent";
const DETECTOR_FORWARD_PROBE_ENV: &str = "VIGIL_DETECTOR_FORWARD_PROBE_PATH";
const DETECTOR_FORWARD_PROBE_NONCE_ENV: &str = "VIGIL_DETECTOR_FORWARD_PROBE_NONCE";
const DETECTOR_FORWARD_OBSERVER_TRAIT: &str = "DetectorForwardObserver";
const DETECTOR_FORWARD_EVENT_TYPE: &str = "DetectorForwardEvent";
const PRESSURE_CAPTURE_FRAMES_ENV: (&str, &str) = ("VIGIL_CAPTURE_FRAMES", "12");
const PRESSURE_DETECTOR_QUEUE_ENV: (&str, &str) = ("VIGIL_DETECTOR_QUEUE_CAPACITY", "1");
const PRESSURE_DETECTOR_WORK_DELAY_ENV: (&str, &str) = ("VIGIL_DETECTOR_WORK_DELAY_MS", "5000");
const DEFAULT_DETECTOR_SUBPROCESS_TIMEOUT_SECS: u64 = 180;

struct SourceFile {
    path: PathBuf,
    text: String,
}

struct LiveReadProbeExpectation<'a> {
    command_label: &'a str,
    text: &'a str,
    authority: &'a CgAuthority,
    proof_path: &'a Path,
    nonce: &'a str,
    request_started_at: SystemTime,
    expected_methods: &'a [&'a str],
}

#[derive(Clone, Copy)]
enum FixtureClip {
    Person,
    AuthenticatedPersonUrl,
    AuthenticatedPersonFields,
    EmptyScene,
    ZeroMedia,
    UndecodableBytes,
}

struct FirstLightWorld {
    _tmp: TempDir,
    data_dir: PathBuf,
    store_path: PathBuf,
    config_path: PathBuf,
    health_port: u16,
    person_clip: PathBuf,
    empty_clip: PathBuf,
    detector_artifact: PathBuf,
    rtsp_url: String,
    rtsp_username: Option<String>,
    rtsp_password: Option<String>,
    _rtsp: Option<RtspFixture>,
    _rtsp_lock: Option<MutexGuard<'static, ()>>,
}

impl FirstLightWorld {
    fn new() -> Result<Self, String> {
        Self::build(None)
    }

    fn new_with_rtsp() -> Result<Self, String> {
        Self::build(Some(FixtureClip::Person))
    }

    fn new_with_authenticated_rtsp_url() -> Result<Self, String> {
        Self::build(Some(FixtureClip::AuthenticatedPersonUrl))
    }

    fn new_with_authenticated_rtsp_fields() -> Result<Self, String> {
        Self::build(Some(FixtureClip::AuthenticatedPersonFields))
    }

    fn new_with_empty_rtsp() -> Result<Self, String> {
        Self::build(Some(FixtureClip::EmptyScene))
    }

    fn new_with_zero_media_rtsp() -> Result<Self, String> {
        Self::build(Some(FixtureClip::ZeroMedia))
    }

    fn new_with_undecodable_rtsp() -> Result<Self, String> {
        Self::build(Some(FixtureClip::UndecodableBytes))
    }

    fn build(rtsp_clip: Option<FixtureClip>) -> Result<Self, String> {
        let tmp = tempfile::tempdir().map_err(|error| format!("temp dir: {error}"))?;
        let data_dir = tmp.path().join("data");
        let store_path = data_dir.join("store.contextgraph");
        let config_path = tmp.path().join("vigil.toml");
        let health_port = free_port()?;
        let root = workspace_root();
        let person_clip = root
            .join("tests")
            .join("fixtures")
            .join("video")
            .join("one-by-one-person-detection.mp4");
        let empty_clip = root
            .join("tests")
            .join("fixtures")
            .join("video")
            .join("empty-scene-from-one-by-one-person-detection.mp4");
        let detector_artifact = root
            .join("tests")
            .join("fixtures")
            .join("models")
            .join("yolox-tiny-coco.pth");
        let rtsp_lock = if rtsp_clip.is_some() {
            Some(
                rtsp_fixture_lock()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            )
        } else {
            None
        };
        let rtsp = match rtsp_clip {
            Some(FixtureClip::Person) => Some(RtspFixture::start(&person_clip)?),
            Some(FixtureClip::AuthenticatedPersonUrl)
            | Some(FixtureClip::AuthenticatedPersonFields) => {
                Some(RtspFixture::start_authenticated(
                    &person_clip,
                    RTSP_AUTH_USERNAME,
                    RTSP_AUTH_PASSWORD,
                )?)
            }
            Some(FixtureClip::EmptyScene) => Some(RtspFixture::start(&empty_clip)?),
            Some(FixtureClip::ZeroMedia) => Some(RtspFixture::start_zero_media()?),
            Some(FixtureClip::UndecodableBytes) => {
                fs::create_dir_all(&data_dir)
                    .map_err(|error| format!("create data dir {}: {error}", data_dir.display()))?;
                let undecodable = data_dir.join("undecodable-video.h264");
                fs::write(&undecodable, b"not an h264 access unit")
                    .map_err(|error| format!("write undecodable fixture: {error}"))?;
                Some(RtspFixture::start_undecodable(&undecodable)?)
            }
            None => None,
        };
        let mut rtsp_url = rtsp
            .as_ref()
            .map(RtspFixture::url)
            .unwrap_or_else(|| CAMERA_RTSP_URL.to_string());
        let mut rtsp_username = None;
        let mut rtsp_password = None;
        if matches!(rtsp_clip, Some(FixtureClip::AuthenticatedPersonUrl))
            && let Some(rtsp) = rtsp.as_ref()
        {
            rtsp_url = rtsp.credentialed_url(RTSP_AUTH_USERNAME, RTSP_AUTH_PASSWORD);
        }
        if matches!(rtsp_clip, Some(FixtureClip::AuthenticatedPersonFields)) {
            rtsp_username = Some(RTSP_AUTH_USERNAME.to_string());
            rtsp_password = Some(RTSP_AUTH_PASSWORD.to_string());
        }
        let config = format!(
            "data_dir = \"{}\"\nstore_path = \"{}\"\nhealth_port = {}\nsite_name = \"{}\"\ncamera_name = \"{}\"\nrtsp_url = \"{}\"\n{}{}detector_model_id = \"{}\"\ndetector_model_path = \"{}\"\ndetector_confidence_threshold = {}\n",
            toml_path(&data_dir),
            toml_path(&store_path),
            health_port,
            SITE_NAME,
            CAMERA_NAME,
            toml_string(&rtsp_url),
            rtsp_username
                .as_deref()
                .map(|value| format!("rtsp_username = \"{}\"\n", toml_string(value)))
                .unwrap_or_default(),
            rtsp_password
                .as_deref()
                .map(|value| format!("rtsp_password = \"{}\"\n", toml_string(value)))
                .unwrap_or_default(),
            DETECTOR_MODEL_ID,
            toml_path(&detector_artifact),
            DETECTOR_THRESHOLD
        );
        fs::write(&config_path, config)
            .map_err(|error| format!("write config {}: {error}", config_path.display()))?;
        Ok(Self {
            _tmp: tmp,
            data_dir,
            store_path,
            config_path,
            health_port,
            person_clip,
            empty_clip,
            detector_artifact,
            rtsp_url,
            rtsp_username,
            rtsp_password,
            _rtsp: rtsp,
            _rtsp_lock: rtsp_lock,
        })
    }

    fn run_runtime_once(&self) -> RuntimeObservation {
        let mut runtime = LiveRuntime::spawn(self);
        let observation = runtime.observe().with_fixture_evidence(self);
        let _ = runtime.terminate();
        observation
    }

    fn run_runtime_until_decoded_frames(&self, minimum_frames: u64) -> RuntimeObservation {
        let mut runtime = LiveRuntime::spawn(self);
        let observation = runtime
            .observe_until_decoded_frames(minimum_frames, Duration::from_secs(60))
            .with_fixture_evidence(self);
        let _ = runtime.terminate();
        observation
    }

    fn run_runtime_until_log_contains(&self, needle: &str) -> RuntimeObservation {
        let mut runtime = LiveRuntime::spawn(self);
        let observation = runtime
            .observe_until_log_contains(needle, Duration::from_secs(300))
            .with_fixture_evidence(self);
        let _ = runtime.terminate();
        observation
    }

    fn run_runtime_until_log_contains_with_env(
        &self,
        needle: &str,
        extra_env: &[(&str, &str)],
    ) -> RuntimeObservation {
        let mut runtime = LiveRuntime::spawn_with_env(self, extra_env);
        let observation = runtime
            .observe_until_log_contains(needle, Duration::from_secs(360))
            .with_fixture_evidence(self);
        let _ = runtime.terminate();
        observation
    }

    fn run_runtime_until_observation_count(&self, minimum: usize) -> RuntimeObservation {
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            let mut observation = self.run_runtime_until_log_contains("observation_written=true");
            let count = self.observation_count();
            observation.logs.push_str(&format!(
                "\nstore_observation_count={count} expected_min_observations={minimum}\n"
            ));
            if count >= minimum || Instant::now() >= deadline {
                return observation;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn observation_count(&self) -> usize {
        let store = self.open_store().ok();
        list_observations(store.as_ref()).len()
    }

    fn set_detector_threshold(&self, threshold: f64) -> Result<(), String> {
        let config = fs::read_to_string(&self.config_path)
            .map_err(|error| format!("read config {}: {error}", self.config_path.display()))?;
        let replaced = config
            .lines()
            .map(|line| {
                if line.starts_with("detector_confidence_threshold = ") {
                    format!("detector_confidence_threshold = {threshold}")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&self.config_path, replaced)
            .map_err(|error| format!("write config {}: {error}", self.config_path.display()))
    }

    fn set_camera_name(&self, camera_name: &str) -> Result<(), String> {
        let config = fs::read_to_string(&self.config_path)
            .map_err(|error| format!("read config {}: {error}", self.config_path.display()))?;
        let replaced = config
            .lines()
            .map(|line| {
                if line.starts_with("camera_name = ") {
                    format!("camera_name = \"{}\"", toml_string(camera_name))
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&self.config_path, replaced)
            .map_err(|error| format!("write config {}: {error}", self.config_path.display()))
    }

    fn stop_rtsp_publisher(&mut self) -> Result<(), String> {
        self._rtsp
            .as_mut()
            .ok_or_else(|| "RTSP fixture was not started".to_string())?
            .stop_publisher()
    }

    fn start_rtsp_publisher(&mut self) -> Result<(), String> {
        self._rtsp
            .as_mut()
            .ok_or_else(|| "RTSP fixture was not started".to_string())?
            .start_current_publisher()
    }

    fn switch_rtsp_clip(&mut self, clip: &Path) -> Result<(), String> {
        self._rtsp
            .as_mut()
            .ok_or_else(|| "RTSP fixture was not started".to_string())?
            .switch_clip(clip)
    }

    fn run_cli<I, S>(&self, args: I) -> CommandObservation
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run_vigil_command(args, Some(self))
    }

    fn run_cli_with_control_socket<I, S>(&self, args: I, socket_path: &Path) -> CommandObservation
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run_vigil_command_with_control(args, Some(self), Some(socket_path))
    }

    fn open_store(&self) -> Result<Store, String> {
        open_store(&self.store_path)
    }

    fn rtsp_logs(&self) -> String {
        self._rtsp
            .as_ref()
            .map(RtspFixture::logs)
            .unwrap_or_default()
    }

    fn rtsp_port(&self) -> Option<u16> {
        rtsp_port(&self.rtsp_url)
    }

    fn credential_free_rtsp_url(&self) -> String {
        self._rtsp
            .as_ref()
            .map(RtspFixture::url)
            .unwrap_or_else(|| self.rtsp_url.clone())
    }

    #[cfg(unix)]
    fn make_clip_dir_read_only(&self) -> Result<(), String> {
        let clip_dir = self.data_dir.join("clips");
        fs::create_dir_all(&clip_dir)
            .map_err(|error| format!("create clip dir {}: {error}", clip_dir.display()))?;
        fs::set_permissions(&clip_dir, fs::Permissions::from_mode(0o555))
            .map_err(|error| format!("make clip dir read-only {}: {error}", clip_dir.display()))
    }

    #[cfg(unix)]
    fn make_clip_dir_writable(&self) -> Result<(), String> {
        let clip_dir = self.data_dir.join("clips");
        fs::set_permissions(&clip_dir, fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("make clip dir writable {}: {error}", clip_dir.display()))
    }
}

struct RuntimeObservation {
    spawned: bool,
    health_ready: bool,
    store_opened_after_exit: bool,
    rtsp_opened: bool,
    rtsp_play_observed: bool,
    decoded_frames: u64,
    detector_invocations: u64,
    outbound_network_attempts: Vec<String>,
    network_trace_lines: Vec<String>,
    rtsp_network_connected: bool,
    rtsp_fixture_published: bool,
    rtsp_fixture_read_observed: bool,
    logs: String,
}

impl RuntimeObservation {
    fn with_fixture_evidence(mut self, world: &FirstLightWorld) -> Self {
        let Some(port) = world.rtsp_port() else {
            self.outbound_network_attempts = self.network_trace_lines.clone();
            return self;
        };
        let traced_rtsp_connect = self
            .network_trace_lines
            .iter()
            .any(|line| network_line_connects_to_port(line, port));
        let runtime_opened_exact_url = self.logs.contains(&format!("127.0.0.1:{port}"));
        let fixture_logs = world.rtsp_logs().to_ascii_lowercase();
        self.rtsp_network_connected = traced_rtsp_connect
            || runtime_opened_exact_url
            || fixture_logs.contains("is reading")
            || fixture_logs.contains("play");
        self.outbound_network_attempts = self
            .network_trace_lines
            .iter()
            .filter(|line| !network_line_connects_to_port(line, port))
            .cloned()
            .collect();
        self.rtsp_fixture_published =
            fixture_logs.contains("is publishing") || fixture_logs.contains("publisher");
        self.rtsp_fixture_read_observed =
            fixture_logs.contains("is reading") || fixture_logs.contains("play");
        self
    }
}

struct LiveRuntime {
    child: Option<Child>,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    trace_prefix: Option<PathBuf>,
    _trace_dir: Option<TempDir>,
    store_locked_while_live: bool,
    health_ready: bool,
    spawn_error: Option<String>,
}

impl LiveRuntime {
    fn spawn(world: &FirstLightWorld) -> Self {
        Self::spawn_with_env(world, &[])
    }

    fn spawn_with_env(world: &FirstLightWorld, extra_env: &[(&str, &str)]) -> Self {
        let trace_dir = None;
        let trace_prefix = None;
        let mut command = Command::new(vigil_binary_path());
        command
            .arg("run")
            .arg("--config")
            .arg(&world.config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_world_runtime_env(&mut command, world);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let spawn = command.spawn();
        let Ok(mut child) = spawn else {
            return Self {
                child: None,
                stdout: Arc::new(Mutex::new(String::new())),
                stderr: Arc::new(Mutex::new(String::new())),
                trace_prefix,
                _trace_dir: trace_dir,
                store_locked_while_live: false,
                health_ready: false,
                spawn_error: spawn.err().map(|error| error.to_string()),
            };
        };
        let health_ready = wait_for_health(world.health_port, 200, Duration::from_secs(2));
        let store_locked_while_live = store_open_is_blocked(&world.store_path);
        let stdout = capture_pipe(child.stdout.take());
        let stderr = capture_pipe(child.stderr.take());
        Self {
            child: Some(child),
            stdout,
            stderr,
            trace_prefix,
            _trace_dir: trace_dir,
            store_locked_while_live,
            health_ready,
            spawn_error: None,
        }
    }

    fn observe(&self) -> RuntimeObservation {
        let start = Instant::now();
        let mut logs = self.logs();
        while start.elapsed() < Duration::from_secs(60)
            && !runtime_reached_pipeline_terminal_signal(&logs)
        {
            thread::sleep(Duration::from_millis(50));
            logs = self.logs();
        }
        self.observation_from_logs(logs)
    }

    fn observation_from_logs(&self, logs: String) -> RuntimeObservation {
        RuntimeObservation {
            spawned: self.spawn_error.is_none(),
            health_ready: self.health_ready,
            store_opened_after_exit: false,
            rtsp_opened: logs.contains("rtsp opened"),
            rtsp_play_observed: logs.contains("rtsp play observed"),
            decoded_frames: parsed_log_counter(&logs, "decoded_frames").unwrap_or_default(),
            detector_invocations: parsed_log_counter(&logs, "detector_invocations")
                .unwrap_or_default(),
            network_trace_lines: self
                .trace_prefix
                .as_ref()
                .map(|prefix| read_network_trace_lines(prefix))
                .unwrap_or_default(),
            outbound_network_attempts: self
                .trace_prefix
                .as_ref()
                .map(|prefix| read_network_trace(prefix))
                .unwrap_or_else(|| vec!["runtime network trace unavailable".to_string()]),
            rtsp_network_connected: false,
            rtsp_fixture_published: false,
            rtsp_fixture_read_observed: false,
            logs,
        }
    }

    fn observe_until_decoded_frames(
        &self,
        minimum_frames: u64,
        timeout: Duration,
    ) -> RuntimeObservation {
        let start = Instant::now();
        let mut observation = self.observe();
        while start.elapsed() < timeout && observation.decoded_frames < minimum_frames {
            thread::sleep(Duration::from_millis(50));
            observation = self.observe();
        }
        observation
    }

    fn observe_until_log_contains(&self, needle: &str, timeout: Duration) -> RuntimeObservation {
        let start = Instant::now();
        let mut logs = self.logs();
        while start.elapsed() < timeout && !logs.contains(needle) {
            thread::sleep(Duration::from_millis(50));
            logs = self.logs();
        }
        self.observation_from_logs(logs)
    }

    fn observe_until_log_occurrences(
        &self,
        needle: &str,
        minimum_occurrences: usize,
        timeout: Duration,
    ) -> RuntimeObservation {
        let start = Instant::now();
        let mut logs = self.logs();
        while start.elapsed() < timeout && logs.matches(needle).count() < minimum_occurrences {
            thread::sleep(Duration::from_millis(50));
            logs = self.logs();
        }
        self.observation_from_logs(logs)
    }

    fn logs(&self) -> String {
        let stdout = self
            .stdout
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        let stderr = self
            .stderr
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        format!("{stdout}{stderr}")
    }

    fn terminate(&mut self) -> Option<ExitStatus> {
        let child = self.child.as_mut()?;
        kill_tracked_child_with_timeout(child, "vigil camera-loop runtime", Duration::from_secs(3))
    }

    fn kill_without_flush(&mut self) -> Option<ExitStatus> {
        let child = self.child.as_mut()?;
        kill_tracked_child_with_timeout(
            child,
            "vigil camera-loop runtime hard stop",
            Duration::from_secs(10),
        )
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let child = self.child.as_mut()?;
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
                Ok(None) | Err(_) => return None,
            }
        }
    }
}

fn runtime_reached_pipeline_terminal_signal(logs: &str) -> bool {
    logs.contains("rtsp opened")
        && logs.contains("decoded_frames=")
        && (logs.contains("observation_written=true")
            || logs.contains("motion_gate_suppressed_segment=true")
            || logs.contains("detector_detections=0")
            || logs.contains("record_detection_failed")
            || logs.contains("detector invocation failed")
            || logs.contains("rtsp probe failed"))
}

impl Drop for LiveRuntime {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

fn kill_tracked_child(child: &mut Child, label: &str) -> Option<ExitStatus> {
    kill_tracked_child_with_timeout(child, label, Duration::from_secs(10))
}

fn kill_tracked_child_with_timeout(
    child: &mut Child,
    label: &str,
    timeout: Duration,
) -> Option<ExitStatus> {
    let pid = child.id();
    match child.try_wait() {
        Ok(Some(status)) => return Some(status),
        Ok(None) => {}
        Err(error) => {
            eprintln!("camera-loop harness could not read child status for {label}: {error}");
            return None;
        }
    }

    if let Err(error) = direct_child_target_is_safe(pid) {
        eprintln!("camera-loop harness refused direct process cleanup target for {label}: {error}");
        return child.try_wait().ok().flatten();
    }

    if let Err(error) = child.kill() {
        eprintln!("camera-loop harness direct child cleanup failed for {label} pid {pid}: {error}");
        return child.try_wait().ok().flatten();
    }
    wait_for_child_exit(child, timeout)
}

fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(25)),
            Ok(None) => return None,
            Err(_) => return None,
        }
    }
}

fn direct_child_target_is_safe(pid: u32) -> Result<(), String> {
    if pid <= 1 {
        return Err(format!(
            "invalid child pid {pid}; direct kill would target the current process group or init"
        ));
    }

    let current_pid = std::process::id();
    let ppid = process_parent_id(pid)
        .ok_or_else(|| format!("child pid {pid} has no readable parent process in /proc"))?;
    if ppid != current_pid {
        return Err(format!(
            "pid {pid} is not a tracked direct child of this camera-loop process; parent pid is {ppid}, expected {current_pid}"
        ));
    }

    Ok(())
}

fn process_parent_id(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat_ppid(&stat)
}

fn parse_stat_ppid(stat: &str) -> Option<u32> {
    let close = stat.rfind(") ")?;
    stat[close + 2..].split_whitespace().nth(1)?.parse().ok()
}

struct CommandObservation {
    status_success: bool,
    status_code: Option<i32>,
    stdout: String,
    stderr: String,
}

struct FixtureReadiness {
    person_clip_present: bool,
    empty_clip_present: bool,
    detector_artifact_present: bool,
    ffmpeg_present: bool,
    ffprobe_present: bool,
    mediamtx_present: bool,
}

impl FixtureReadiness {
    fn probe(world: &FirstLightWorld) -> Self {
        Self {
            person_clip_present: world.person_clip.is_file(),
            empty_clip_present: world.empty_clip.is_file(),
            detector_artifact_present: world.detector_artifact.is_file(),
            ffmpeg_present: tool_path("VIGIL_FFMPEG_BIN", "ffmpeg").is_some(),
            ffprobe_present: tool_path("VIGIL_FFPROBE_BIN", "ffprobe").is_some(),
            mediamtx_present: tool_path("VIGIL_MEDIAMTX_BIN", "mediamtx").is_some(),
        }
    }

    fn missing_messages(&self) -> Vec<String> {
        let mut failures = Vec::new();
        if !self.person_clip_present {
            failures.push("person-walking fixture is missing".to_string());
        }
        if !self.empty_clip_present {
            failures.push(
                "empty-scene derivative fixture is missing; run cargo xtask setup-harness"
                    .to_string(),
            );
        }
        if !self.detector_artifact_present {
            failures.push("detector model artifact fixture is missing".to_string());
        }
        if !self.ffmpeg_present {
            failures.push("ffmpeg is not available to the RTSP fixture harness".to_string());
        }
        if !self.ffprobe_present {
            failures.push("ffprobe is not available for durable clip probing".to_string());
        }
        if !self.mediamtx_present {
            failures.push("mediamtx is not available to serve real RTSP fixtures".to_string());
        }
        failures
    }
}

fn assert_contract(failures: Vec<String>) {
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

fn world_or_fail() -> FirstLightWorld {
    match FirstLightWorld::new() {
        Ok(world) => world,
        Err(error) => {
            assert!(error.is_empty(), "{error}");
            unreachable!("assertion above always fails");
        }
    }
}

fn rtsp_world_or_fail() -> FirstLightWorld {
    match FirstLightWorld::new_with_rtsp() {
        Ok(world) => world,
        Err(error) => {
            assert!(error.is_empty(), "{error}");
            unreachable!("assertion above always fails");
        }
    }
}

fn authenticated_rtsp_url_world_or_fail() -> FirstLightWorld {
    match FirstLightWorld::new_with_authenticated_rtsp_url() {
        Ok(world) => world,
        Err(error) => {
            assert!(error.is_empty(), "{error}");
            unreachable!("assertion above always fails");
        }
    }
}

fn authenticated_rtsp_fields_world_or_fail() -> FirstLightWorld {
    match FirstLightWorld::new_with_authenticated_rtsp_fields() {
        Ok(world) => world,
        Err(error) => {
            assert!(error.is_empty(), "{error}");
            unreachable!("assertion above always fails");
        }
    }
}

fn empty_rtsp_world_or_fail() -> FirstLightWorld {
    match FirstLightWorld::new_with_empty_rtsp() {
        Ok(world) => world,
        Err(error) => {
            assert!(error.is_empty(), "{error}");
            unreachable!("assertion above always fails");
        }
    }
}

fn zero_media_rtsp_world_or_fail() -> FirstLightWorld {
    match FirstLightWorld::new_with_zero_media_rtsp() {
        Ok(world) => world,
        Err(error) => {
            assert!(error.is_empty(), "{error}");
            unreachable!("assertion above always fails");
        }
    }
}

fn undecodable_rtsp_world_or_fail() -> FirstLightWorld {
    match FirstLightWorld::new_with_undecodable_rtsp() {
        Ok(world) => world,
        Err(error) => {
            assert!(error.is_empty(), "{error}");
            unreachable!("assertion above always fails");
        }
    }
}

fn apply_world_runtime_env(command: &mut Command, world: &FirstLightWorld) {
    command
        .env("VIGIL_HEALTH_PORT", world.health_port.to_string())
        .env("VIGIL_DATA_DIR", &world.data_dir)
        .env("VIGIL_STORE_PATH", &world.store_path)
        .env("VIGIL_RTSP_URL", &world.rtsp_url)
        .env("VIGIL_DETECTOR_MODEL_PATH", &world.detector_artifact)
        .env("VIGIL_RTSP_RETRY_INITIAL_MS", "200")
        .env("VIGIL_RTSP_RETRY_MAX_MS", "1000");
    if let Some(username) = world.rtsp_username.as_ref() {
        command.env("VIGIL_RTSP_USERNAME", username);
    }
    if let Some(password) = world.rtsp_password.as_ref() {
        command.env("VIGIL_RTSP_PASSWORD", password);
    }
}

fn rtsp_fixture_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn run_runtime_and_open_store(world: &FirstLightWorld) -> (RuntimeObservation, Option<Store>) {
    run_runtime_and_open_store_with_env(world, &[])
}

fn run_runtime_and_open_store_with_env(
    world: &FirstLightWorld,
    extra_env: &[(&str, &str)],
) -> (RuntimeObservation, Option<Store>) {
    let mut runtime = LiveRuntime::spawn_with_env(world, extra_env);
    let mut observation = runtime.observe().with_fixture_evidence(world);
    let _ = runtime.terminate();
    let store = world.open_store().ok();
    observation.store_opened_after_exit = store.is_some();
    (observation, store)
}

fn run_runtime_until_log_contains_and_open_store_with_env(
    world: &FirstLightWorld,
    extra_env: &[(&str, &str)],
    needle: &str,
) -> (RuntimeObservation, Option<Store>) {
    let mut runtime = LiveRuntime::spawn_with_env(world, extra_env);
    let mut observation = runtime
        .observe_until_log_contains(needle, Duration::from_secs(600))
        .with_fixture_evidence(world);
    let _ = runtime.terminate();
    let store = world.open_store().ok();
    observation.store_opened_after_exit = store.is_some();
    (observation, store)
}

fn open_store(path: &Path) -> Result<Store, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create store parent {}: {error}", parent.display()))?;
    }
    Store::open(StoreConfig {
        db_path: path.to_path_buf(),
        default_text_embedder: Some(EmbedderConfig::disabled()),
        ..StoreConfig::default()
    })
    .map_err(|error| format!("open store {}: {error}", path.display()))
}

fn assert_no_runtime_answer_labels(world: &FirstLightWorld, failures: &mut Vec<String>) {
    match fs::read_to_string(&world.config_path) {
        Ok(config) => {
            for forbidden in [
                "fixture_scenario",
                "two_events_observed_at_out_of_commit_order",
                "stats_forced_clip_failure_keep_pace_drop",
                "expected_event",
                "expected_observation",
                "expected_stats",
            ] {
                if config.contains(forbidden) {
                    failures.push(format!(
                        "runtime config leaked answer-bearing label {forbidden}"
                    ));
                }
            }
        }
        Err(error) => failures.push(format!(
            "could not read runtime config {}: {error}",
            world.config_path.display()
        )),
    }
}

fn assert_live_request_has_cg_read_probe(
    expectation: LiveReadProbeExpectation<'_>,
    failures: &mut Vec<String>,
) {
    if expectation.text.contains("cg-read-receipt=") {
        failures.push(format!(
            "live {} returned a printable cg-read-receipt instead of a test-side Store read probe",
            expectation.command_label
        ));
    }
    let metadata = match fs::metadata(expectation.proof_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            failures.push(format!(
                "live {} did not write Store read probe {}: {error}",
                expectation.command_label,
                expectation.proof_path.display()
            ));
            return;
        }
    };
    match metadata.modified() {
        Ok(modified) if modified >= expectation.request_started_at => {}
        Ok(_) => failures.push(format!(
            "Store read probe {} was not updated during the live {} request",
            expectation.proof_path.display(),
            expectation.command_label
        )),
        Err(error) => failures.push(format!(
            "could not read Store read probe mtime {}: {error}",
            expectation.proof_path.display()
        )),
    }
    let proof = match fs::read_to_string(expectation.proof_path) {
        Ok(proof) => proof,
        Err(error) => {
            failures.push(format!(
                "could not read Store read probe {}: {error}",
                expectation.proof_path.display()
            ));
            return;
        }
    };
    if proof.trim().is_empty() {
        failures.push(format!(
            "Store read probe {} was empty",
            expectation.proof_path.display()
        ));
        return;
    }
    if !proof.contains(expectation.nonce) {
        failures.push("Store read probe omitted the test nonce".to_string());
    }
    if proof.to_ascii_lowercase().contains("writer=vigil")
        || proof.to_ascii_lowercase().contains("crate=vigil")
    {
        failures.push(
            "Store read probe identified Vigil as the writer instead of context-graph".to_string(),
        );
    }
    let mut expected_values = vec![
        CG_READ_EVENT_TYPE.to_string(),
        CG_READ_OBSERVER_TRAIT.to_string(),
        expectation.authority.observation.id.to_string(),
    ];
    expected_values.extend(
        expectation
            .expected_methods
            .iter()
            .map(|method| method.to_string()),
    );
    for expected in expected_values {
        if !proof.contains(&expected) {
            failures.push(format!("Store read probe omitted {expected}"));
        }
    }
    let read_events = proof
        .lines()
        .filter(|line| line.contains(CG_READ_EVENT_TYPE) && line.contains(expectation.nonce))
        .collect::<Vec<_>>();
    if read_events.len() < 6 {
        failures.push(format!(
            "Store read probe recorded {} observer events for live {command_label}, expected at least 6",
            read_events.len(),
            command_label = expectation.command_label
        ));
    }
    let sequences = read_events
        .iter()
        .filter_map(|line| parsed_u64_field(line, "seq"))
        .collect::<Vec<_>>();
    if sequences.len() != read_events.len() {
        failures
            .push("Store read probe omitted a seq field on at least one read event".to_string());
    }
    if sequences.windows(2).any(|window| window[1] <= window[0]) {
        failures.push("Store read probe seq fields were not strictly increasing".to_string());
    }
    for event in &read_events {
        if parsed_line_field(event, "writer") != Some("context-graph") {
            failures.push(format!(
                "Store read probe event was not emitted by context-graph: {event}"
            ));
        }
    }
    for forbidden in ["mirror", "snapshot", "sidecar", "cache", "readback"] {
        if proof.to_ascii_lowercase().contains(forbidden) {
            failures.push(format!(
                "Store read probe referenced forbidden {forbidden} path"
            ));
        }
    }
}

fn vigil_source_files() -> Vec<SourceFile> {
    collect_rust_source_files(&workspace_root().join("crates").join("vigil").join("src"))
}

fn vigil_production_source_files() -> Vec<SourceFile> {
    vigil_source_files()
        .into_iter()
        .filter(|source| !source_is_oracle_or_nonproduction_detector_helper(source))
        .collect()
}

fn source_is_oracle_or_nonproduction_detector_helper(source: &SourceFile) -> bool {
    let path_lower = source.path.display().to_string().to_ascii_lowercase();
    let file_name = source
        .path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    path_lower.contains("/src/bin/")
        || path_lower.contains("/oracle")
        || path_lower.contains("/fixtures")
        || path_lower.contains("/fixture")
        || path_lower.contains("/test_")
        || path_lower.contains("/tests")
        || file_name.contains("oracle")
        || file_name.contains("fixture")
        || file_name.contains("golden")
        || file_name.contains("probe_helper")
        || source.text.to_ascii_lowercase().contains("oracle-only")
}

fn collect_rust_source_files(root: &Path) -> Vec<SourceFile> {
    let mut sources = Vec::new();
    collect_rust_source_files_into(root, &mut sources);
    sources
}

fn collect_rust_source_files_into(dir: &Path, sources: &mut Vec<SourceFile>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_source_files_into(&path, sources);
        } else if path.extension() == Some(OsStr::new("rs"))
            && let Ok(source) = fs::read_to_string(&path)
        {
            sources.push(SourceFile { path, text: source });
        }
    }
}

fn function_source_slice(source: &str, function_name: &str) -> Option<String> {
    let start = source.find(&format!("fn {function_name}"))?;
    let body_start = source[start..].find('{')? + start;
    let mut depth = 0usize;
    for (offset, character) in source[body_start..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(source[start..=body_start + offset].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn assert_detector_source_has_model_execution_path(failures: &mut Vec<String>) {
    let sources = vigil_production_source_files();
    let source = sources
        .iter()
        .map(|source| source.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "model-forward-sha256",
        "detector_backend",
        "detector_session_id",
        "detector_nms_sha256",
        DETECTOR_FORWARD_PROBE_ENV,
        DETECTOR_FORWARD_PROBE_NONCE_ENV,
        DETECTOR_FORWARD_OBSERVER_TRAIT,
        DETECTOR_FORWARD_EVENT_TYPE,
        "observe_forward",
        "model_forward_sha256",
        "result_sha256",
        "clip_sha256",
        "burn",
        "Tensor",
        "forward",
        "nms",
        "load_record",
    ] {
        if !source.contains(expected) {
            failures.push(format!(
                "detector source omitted model-execution marker {expected}"
            ));
        }
    }
    for forbidden in [
        "classify_pgm",
        "fixture_scenario",
        "synthetic_detector",
        "threshold + 0.35",
        "PERSON_MODEL_FORWARD_SHA256",
        "PERSON_NMS_SHA256",
        "PERSON_RESULT_SHA256",
        "PERSON_GOLDEN",
    ] {
        if source.contains(forbidden) {
            failures.push(format!(
                "detector source retained forbidden synthetic marker {forbidden}"
            ));
        }
    }
    for forbidden in [
        PERSON_MODEL_FORWARD_SHA256,
        PERSON_NMS_SHA256,
        PERSON_RESULT_SHA256,
        PERSON_GOLDEN_BBOX,
        "0.84",
    ] {
        if source.contains(forbidden) {
            failures.push(format!(
                "detector source hardcoded pinned golden output {forbidden}"
            ));
        }
    }
    for forbidden in [
        "classical_person_background",
        "heuristic_detector",
        "writer=vigil",
        "crate=vigil",
    ] {
        if source.contains(forbidden) {
            failures.push(format!(
                "detector source retained forbidden forged-forward marker {forbidden}"
            ));
        }
    }
    assert_production_detector_does_not_delegate_to_oracle(&sources, failures);
    let backend_sources = sources
        .iter()
        .filter(|source| source_is_production_detector_backend(source))
        .collect::<Vec<_>>();
    if backend_sources.is_empty() {
        failures.push(
            "detector source guard found no reachable production detector backend module under crates/vigil/src"
                .to_string(),
        );
    }
    for backend in &backend_sources {
        for expected in [
            "burn",
            "Tensor",
            "forward",
            "nms",
            "model_forward_sha256",
            "result_sha256",
            "clip_sha256",
            "emitter=detector-backend-forward",
            "stage=model-tensor-forward",
        ] {
            if !backend.text.contains(expected) {
                failures.push(format!(
                    "detector forward event source {} omitted backend marker {expected}",
                    backend.path.display()
                ));
            }
        }
    }
    let observer_sources = sources
        .iter()
        .filter(|source| source.text.contains("observe_forward"))
        .collect::<Vec<_>>();
    for observer_source in &observer_sources {
        if !source_is_production_detector_backend(observer_source) {
            failures.push(format!(
                "detector forward observer is emitted outside the production detector backend: {}",
                observer_source.path.display()
            ));
        }
    }
    assert_detector_backend_is_reachable_from_vigil_binary_path(
        &sources,
        &backend_sources,
        failures,
    );
    for source_file in &sources {
        let file_name = source_file
            .path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let is_probe_or_runtime_surface = file_name == "lib.rs"
            || file_name == "main.rs"
            || file_name == "runtime.rs"
            || file_name.contains("detector_probe");
        if is_probe_or_runtime_surface {
            for forbidden in [
                DETECTOR_FORWARD_PROBE_ENV,
                DETECTOR_FORWARD_PROBE_NONCE_ENV,
                DETECTOR_FORWARD_EVENT_TYPE,
                "observe_forward",
                "model_forward_sha256",
                "result_sha256",
                PERSON_MODEL_FORWARD_SHA256,
                PERSON_NMS_SHA256,
                PERSON_RESULT_SHA256,
                PERSON_GOLDEN_BBOX,
            ] {
                if source_file.text.contains(forbidden) {
                    failures.push(format!(
                        "detector probe/runtime surface {} owns forward proof marker {forbidden}",
                        source_file.path.display()
                    ));
                }
            }
        }
    }
    assert_forward_observer_is_inside_model_forward_window(&backend_sources, failures);
}

fn source_is_production_detector_backend(source: &SourceFile) -> bool {
    if source_is_oracle_or_nonproduction_detector_helper(source) {
        return false;
    }
    let path_lower = source.path.display().to_string().to_ascii_lowercase();
    let file_name = source
        .path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        file_name.as_str(),
        "lib.rs"
            | "main.rs"
            | "runtime.rs"
            | "config.rs"
            | "store.rs"
            | "health.rs"
            | "shutdown.rs"
    ) {
        return false;
    }
    let source_lower = source.text.to_ascii_lowercase();
    (path_lower.contains("detector")
        || path_lower.contains("vision")
        || path_lower.contains("inference")
        || path_lower.contains("yolox")
        || path_lower.contains("burn"))
        && source_lower.contains("detector")
        && source_lower.contains("burn")
        && source.text.contains("Tensor")
        && (source.text.contains(".forward(") || source.text.contains("model.forward("))
        && source_lower.contains("nms")
        && source.text.contains("observe_forward")
}

fn assert_detector_backend_is_reachable_from_vigil_binary_path(
    sources: &[SourceFile],
    backend_sources: &[&SourceFile],
    failures: &mut Vec<String>,
) {
    let runtime_or_probe_text = sources
        .iter()
        .filter(|source| {
            let file_name = source
                .path
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default();
            matches!(file_name, "lib.rs" | "main.rs" | "runtime.rs")
                || source.text.contains("detector-probe")
                || file_name.contains("detector_probe")
        })
        .map(|source| source.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let backend_names = backend_sources
        .iter()
        .filter_map(|backend| detector_backend_module_name(&backend.path))
        .collect::<Vec<_>>();
    for backend in backend_sources {
        let Some(module_name) = detector_backend_module_name(&backend.path) else {
            failures.push(format!(
                "production detector backend path is not module-addressable: {}",
                backend.path.display()
            ));
            continue;
        };
        let reachable = sources
            .iter()
            .filter(|source| source.path.as_path() != backend.path.as_path())
            .any(|source| {
                source.text.contains(&format!("mod {module_name}"))
                    || source.text.contains(&format!("crate::{module_name}"))
                    || source.text.contains(&format!("{module_name}::"))
                    || source.text.contains(&format!("use super::{module_name}"))
            });
        if !reachable {
            failures.push(format!(
                "production detector backend module `{module_name}` is not reachable from the vigil binary/runtime/probe path"
            ));
        }
        if !runtime_or_probe_text.contains(&module_name)
            && !runtime_or_probe_text.contains("DetectorBackend")
            && !runtime_or_probe_text.contains("BurnYolox")
            && !runtime_or_probe_text.contains("YoloxDetector")
        {
            failures.push(format!(
                "vigil runtime/probe path does not use production detector backend module `{module_name}`"
            ));
        }
    }
    if !backend_names.is_empty() {
        for function in ["run_detector_probe", "load_detector", "detect_segment"] {
            let backend_owns_function = backend_sources
                .iter()
                .any(|backend| backend.text.contains(&format!("fn {function}")));
            let runtime_calls_function = runtime_or_probe_text.contains(&format!("{function}("));
            if !backend_owns_function || !runtime_calls_function {
                failures.push(format!(
                    "vigil runtime/probe path is not bound to concrete production detector backend function {function}"
                ));
            }
        }
        if !backend_sources
            .iter()
            .any(|backend| backend.text.contains("fn detect_frame"))
        {
            failures.push(
                "production detector backend does not own concrete detector-probe frame decoding"
                    .to_string(),
            );
        }
    }
    let production_text = sources
        .iter()
        .map(|source| source.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !production_text.contains("detector-probe") && !production_text.contains("detector_probe") {
        failures.push(
            "vigil detector-probe command is not wired through the production detector backend"
                .to_string(),
        );
    }
}

fn assert_production_detector_does_not_delegate_to_oracle(
    sources: &[SourceFile],
    failures: &mut Vec<String>,
) {
    for source in sources {
        let lower = source.text.to_ascii_lowercase();
        for forbidden in [
            "yolox-burn-oracle",
            "yolox_burn_oracle",
            "detector_oracle",
            "run_independent_detector_oracle",
            "cargo_bin_exe_yolox-burn-oracle",
            "src/bin/yolox",
            "oracle_path",
        ] {
            if lower.contains(forbidden) {
                failures.push(format!(
                    "production detector source {} delegates to or references the test oracle marker {forbidden}",
                    source.path.display()
                ));
            }
        }
        if source_is_detector_execution_surface(source) {
            for spawn_marker in [
                "std::process::command",
                "process::command",
                "command::new",
                ".spawn(",
                ".output(",
            ] {
                if lower.contains(spawn_marker) {
                    failures.push(format!(
                        "production detector source {} can spawn or shell out instead of calling the detector backend directly: {spawn_marker}",
                        source.path.display()
                    ));
                }
            }
            if lower.contains("oracle")
                && (lower.contains("command::new") || lower.contains("process::command"))
            {
                failures.push(format!(
                    "production detector source {} can shell out to an oracle helper",
                    source.path.display()
                ));
            }
        }
    }

    let source_bin_files = vigil_source_files()
        .into_iter()
        .filter(|source| {
            source
                .path
                .display()
                .to_string()
                .to_ascii_lowercase()
                .contains("/src/bin/")
        })
        .collect::<Vec<_>>();
    for bin_source in source_bin_files {
        let Some(bin_name) = bin_source.path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        let bin_file_name = bin_source
            .path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let dashed_bin = bin_name.replace('_', "-");
        let production_refs_bin = sources.iter().any(|source| {
            let lower = source.text.to_ascii_lowercase();
            lower.contains(bin_name)
                || lower.contains(&dashed_bin)
                || lower.contains(&bin_file_name)
        });
        if production_refs_bin {
            failures.push(format!(
                "production detector source references non-test helper binary {}",
                bin_source.path.display()
            ));
        }
    }
}

fn source_is_detector_execution_surface(source: &SourceFile) -> bool {
    let file_name = source
        .path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    file_name == "runtime.rs" || source_is_production_detector_backend(source)
}

fn detector_backend_module_name(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(OsStr::to_str)
        .map(|name| name.to_string())
}

fn assert_forward_observer_is_inside_model_forward_window(
    sources: &[&SourceFile],
    failures: &mut Vec<String>,
) {
    let mut observe_forward_count = 0_u64;
    for source in sources {
        let lines = source.text.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            if !line.contains("observe_forward") {
                continue;
            }
            observe_forward_count += 1;
            let start = index.saturating_sub(80);
            let end = (index + 80).min(lines.len().saturating_sub(1));
            let window = lines[start..=end].join("\n");
            let window_lower = window.to_ascii_lowercase();
            for expected in [
                "DetectorForwardEvent",
                "Tensor",
                "model_forward_sha256",
                "result_sha256",
                "clip_sha256",
            ] {
                if !window.contains(expected) {
                    failures.push(format!(
                        "observe_forward in {} is not tied to backend event field {expected}",
                        source.path.display()
                    ));
                }
            }
            if !(window.contains(".forward(") || window.contains("model.forward(")) {
                failures.push(format!(
                    "observe_forward in {} is not in the concrete model.forward(tensor) function window",
                    source.path.display()
                ));
            }
            let forward_binding = bound_variable_for_forward(&window);
            match forward_binding.as_deref() {
                Some(binding) => {
                    if !nms_or_result_consumes_forward_binding(&window, binding) {
                        failures.push(format!(
                            "observe_forward in {} does not prove NMS/result formatting consumes model.forward output `{binding}`",
                            source.path.display()
                        ));
                    }
                }
                None => failures.push(format!(
                    "observe_forward in {} does not bind the result of model.forward(tensor)",
                    source.path.display()
                )),
            }
            if !(window_lower.contains("burn") && window_lower.contains("tensor")) {
                failures.push(format!(
                    "observe_forward in {} is not in the Burn tensor backend window",
                    source.path.display()
                ));
            }
            if !window_lower.contains("nms") {
                failures.push(format!(
                    "observe_forward in {} is not in the NMS/result-formatting function window",
                    source.path.display()
                ));
            }
            if let (Some(forward), Some(observe)) =
                (window.find(".forward("), window.find("observe_forward"))
                && forward > observe
            {
                failures.push(format!(
                    "observe_forward in {} appears before model.forward(tensor)",
                    source.path.display()
                ));
            }
        }
    }
    if observe_forward_count == 0 {
        failures.push("detector backend never calls observe_forward".to_string());
    }
}

fn bound_variable_for_forward(source: &str) -> Option<String> {
    source.lines().find_map(|line| {
        if !(line.contains(".forward(") && line.contains("let ") && line.contains('=')) {
            return None;
        }
        let lhs = line.split_once('=')?.0;
        let variable = lhs
            .trim()
            .trim_start_matches("let")
            .trim()
            .trim_start_matches("mut")
            .trim()
            .split_once(':')
            .map(|(name, _)| name)
            .unwrap_or_else(|| {
                lhs.trim()
                    .trim_start_matches("let")
                    .trim()
                    .trim_start_matches("mut")
                    .trim()
            })
            .trim()
            .to_string();
        (!variable.is_empty()).then_some(variable)
    })
}

fn nms_or_result_consumes_forward_binding(source: &str, binding: &str) -> bool {
    let mut saw_forward = false;
    source.lines().any(|line| {
        if line.contains(".forward(") && line.contains(binding) {
            saw_forward = true;
            return false;
        }
        saw_forward
            && line.contains(binding)
            && (line.to_ascii_lowercase().contains("nms")
                || line.contains("decode")
                || line.contains("boxes")
                || line.contains("result_sha256")
                || line.contains("DetectorForwardEvent"))
    })
}

fn store_open_is_blocked(path: &Path) -> bool {
    match open_store(path) {
        Ok(_) => false,
        Err(error) => error.to_ascii_lowercase().contains("lock"),
    }
}

fn list_context_count(store: Option<&Store>) -> usize {
    store
        .and_then(|store| store.list_contexts().ok())
        .map(|contexts| contexts.len())
        .unwrap_or_default()
}

fn list_site_context_names(store: Option<&Store>) -> Vec<String> {
    store
        .and_then(|store| store.list_contexts().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|context| context.name)
        .collect()
}

fn device_entities(store: Option<&Store>) -> Vec<Entity> {
    store
        .and_then(|store| {
            store
                .list_entities(ListEntityFilter {
                    entity_type: Some(EntityType::Device),
                    ..ListEntityFilter::default()
                })
                .ok()
        })
        .unwrap_or_default()
}

fn list_observations(store: Option<&Store>) -> Vec<Observation> {
    store
        .and_then(|store| store.list_observations(None).ok())
        .unwrap_or_default()
}

fn decision_audit_count(store: Option<&Store>) -> usize {
    store
        .and_then(|store| store.audit_query(AuditFilter::default()).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| matches!(entry.target, AuditTarget::Decision(_)))
        .count()
}

fn first_camera_rtsp(cameras: &[Entity]) -> Option<&str> {
    cameras
        .iter()
        .find(|camera| camera.name == CAMERA_NAME)
        .and_then(|camera| camera.properties.get("rtsp_url"))
        .and_then(|value| value.as_str())
}

fn assert_authenticated_rtsp_runtime(
    world: &FirstLightWorld,
    runtime: &RuntimeObservation,
    store: Option<&Store>,
    failures: &mut Vec<String>,
) {
    if !runtime.spawned {
        failures.push("authenticated RTSP runtime did not spawn".to_string());
    }
    if !runtime.rtsp_opened {
        failures.push("authenticated RTSP runtime did not open the configured stream".to_string());
    }
    if !runtime.rtsp_network_connected || !runtime.rtsp_fixture_read_observed {
        failures.push("authenticated RTSP fixture did not observe Vigil reading the stream".into());
    }
    if runtime.decoded_frames == 0 {
        failures.push("authenticated RTSP runtime did not decode any frames".to_string());
    }
    if runtime.logs.contains("URL must not contain credentials") {
        failures.push("authenticated RTSP runtime still passed URL userinfo to Retina".to_string());
    }
    if runtime.logs.contains(RTSP_AUTH_PASSWORD)
        || runtime
            .logs
            .contains(&format!("{RTSP_AUTH_USERNAME}:{RTSP_AUTH_PASSWORD}"))
    {
        failures.push("authenticated RTSP runtime logs leaked the camera password".to_string());
    }
    let cameras = device_entities(store);
    let expected_rtsp_url = world.credential_free_rtsp_url();
    let Some(camera_rtsp_url) = first_camera_rtsp(&cameras) else {
        failures.push("authenticated RTSP runtime did not persist a camera entity".to_string());
        return;
    };
    if camera_rtsp_url != expected_rtsp_url.as_str() {
        failures.push(format!(
            "authenticated RTSP camera URL persisted {camera_rtsp_url:?}, expected credential-free {expected_rtsp_url:?}"
        ));
    }
    if camera_rtsp_url.contains(RTSP_AUTH_PASSWORD)
        || camera_rtsp_url.contains(&format!("{RTSP_AUTH_USERNAME}:"))
    {
        failures.push("authenticated RTSP camera URL persisted credentials into cg".to_string());
    }
}

fn parsed_stat(text: &str, key: &str) -> Option<f64> {
    text.lines().find_map(|line| {
        let (line_key, value) = line.split_once('=')?;
        (line_key.trim() == key).then(|| value.trim().parse::<f64>().ok())?
    })
}

fn command_text(command: &CommandObservation) -> String {
    format!("{}{}", command.stdout, command.stderr)
}

fn wait_for_live_events_row(world: &FirstLightWorld, timeout: Duration) -> CommandObservation {
    let start = Instant::now();
    loop {
        let response = world.run_cli(["events"]);
        let text = command_text(&response);
        if response.status_success
            && text.contains("served-by=af_unix")
            && text.contains("observation_id=")
        {
            return response;
        }
        if start.elapsed() >= timeout {
            return response;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_live_event_after(
    world: &FirstLightWorld,
    after: DateTime<Utc>,
    timeout: Duration,
) -> CommandObservation {
    let start = Instant::now();
    loop {
        let response = world.run_cli(["events"]);
        let text = command_text(&response);
        if response.status_success && events_text_has_observed_after(&text, after) {
            return response;
        }
        if start.elapsed() >= timeout {
            return response;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn events_text_has_observed_after(text: &str, after: DateTime<Utc>) -> bool {
    text.lines().any(|line| {
        let Some(value) = parsed_line_field(line, "observed_at")
            .or_else(|| parsed_line_field(line, "event_time"))
        else {
            return false;
        };
        DateTime::parse_from_rfc3339(value)
            .map(|observed_at| observed_at.with_timezone(&Utc) > after)
            .unwrap_or(false)
    })
}

fn run_vigil_command<I, S>(args: I, world: Option<&FirstLightWorld>) -> CommandObservation
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_vigil_command_with_control(args, world, None)
}

fn run_vigil_command_with_control<I, S>(
    args: I,
    world: Option<&FirstLightWorld>,
    control_socket: Option<&Path>,
) -> CommandObservation
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(vigil_binary_path());
    command.args(args);
    if let Some(world) = world {
        apply_world_runtime_env(&mut command, world);
    }
    if let Some(socket_path) = control_socket {
        command.env("VIGIL_CONTROL_SOCKET", socket_path);
    }
    match command.output() {
        Ok(output) => output_observation(output),
        Err(error) => CommandObservation {
            status_success: false,
            status_code: None,
            stdout: String::new(),
            stderr: format!("could not execute vigil binary: {error}"),
        },
    }
}

fn run_vigil_command_traced<I, S>(args: I, world: Option<&FirstLightWorld>) -> TracedCommand
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let trace_dir = tempfile::tempdir();
    let Ok(trace_dir) = trace_dir else {
        return TracedCommand {
            command: CommandObservation {
                status_success: false,
                status_code: None,
                stdout: String::new(),
                stderr: "could not create network trace directory".to_string(),
            },
            outbound_attempts: vec!["network trace directory unavailable".to_string()],
            trace_available: false,
            trace_file_count: 0,
            raw_trace_lines: 0,
        };
    };
    let trace_prefix = trace_dir.path().join("network");
    let Some(strace) = tool_path("VIGIL_STRACE_BIN", "strace") else {
        return TracedCommand {
            command: CommandObservation {
                status_success: false,
                status_code: None,
                stdout: String::new(),
                stderr: "strace is not available for fail-closed no-egress tracing".to_string(),
            },
            outbound_attempts: vec!["strace unavailable".to_string()],
            trace_available: false,
            trace_file_count: 0,
            raw_trace_lines: 0,
        };
    };
    let mut command = Command::new(strace);
    command
        .arg("-ff")
        .arg("-e")
        .arg("trace=network")
        .arg("-o")
        .arg(&trace_prefix)
        .arg(vigil_binary_path())
        .args(args);
    if let Some(world) = world {
        apply_world_runtime_env(&mut command, world);
    }
    let output = command.output();
    let command = match output {
        Ok(output) => output_observation(output),
        Err(error) => CommandObservation {
            status_success: false,
            status_code: None,
            stdout: String::new(),
            stderr: format!("could not run strace network command: {error}"),
        },
    };
    let raw_trace = read_network_trace_raw(&trace_prefix);
    TracedCommand {
        command,
        outbound_attempts: observed_outbound_network_lines(&raw_trace),
        trace_available: true,
        trace_file_count: network_trace_file_count(&trace_prefix),
        raw_trace_lines: raw_trace.lines().count(),
    }
}

struct TracedCommand {
    command: CommandObservation,
    outbound_attempts: Vec<String>,
    trace_available: bool,
    trace_file_count: usize,
    raw_trace_lines: usize,
}

fn output_observation(output: Output) -> CommandObservation {
    CommandObservation {
        status_success: output.status.success(),
        status_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn run_command_with_timeout(
    mut command: Command,
    label: &str,
    timeout: Duration,
) -> CommandObservation {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let spawn = command.spawn();
    let Ok(mut child) = spawn else {
        return CommandObservation {
            status_success: false,
            status_code: None,
            stdout: String::new(),
            stderr: format!(
                "could not execute {label}: {}",
                spawn
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_default()
            ),
        };
    };
    let stdout = capture_pipe(child.stdout.take());
    let stderr = capture_pipe(child.stderr.take());
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                thread::sleep(Duration::from_millis(25));
                return CommandObservation {
                    status_success: status.success(),
                    status_code: status.code(),
                    stdout: stdout.lock().map(|logs| logs.clone()).unwrap_or_default(),
                    stderr: stderr.lock().map(|logs| logs.clone()).unwrap_or_default(),
                };
            }
            Ok(None) if started.elapsed() < timeout => {
                thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let elapsed = started.elapsed();
                let status =
                    kill_tracked_child_with_timeout(&mut child, label, Duration::from_secs(10));
                thread::sleep(Duration::from_millis(25));
                let mut stderr_text = stderr.lock().map(|logs| logs.clone()).unwrap_or_default();
                if !stderr_text.is_empty() && !stderr_text.ends_with('\n') {
                    stderr_text.push('\n');
                }
                stderr_text.push_str(&format!(
                    "{label} timed out after {:.1}s; child_status={status:?}",
                    elapsed.as_secs_f64()
                ));
                return CommandObservation {
                    status_success: false,
                    status_code: status.and_then(|status| status.code()),
                    stdout: stdout.lock().map(|logs| logs.clone()).unwrap_or_default(),
                    stderr: stderr_text,
                };
            }
            Err(error) => {
                return CommandObservation {
                    status_success: false,
                    status_code: None,
                    stdout: stdout.lock().map(|logs| logs.clone()).unwrap_or_default(),
                    stderr: format!("{label} status check failed: {error}"),
                };
            }
        }
    }
}

struct RtspFixture {
    url: String,
    clip: Option<PathBuf>,
    ffmpeg_bin: PathBuf,
    mediamtx: Child,
    ffmpeg: Option<Child>,
    mediamtx_stdout: Arc<Mutex<String>>,
    mediamtx_stderr: Arc<Mutex<String>>,
    ffmpeg_stdout: Arc<Mutex<String>>,
    ffmpeg_stderr: Arc<Mutex<String>>,
}

type CapturedChild = (Child, Arc<Mutex<String>>, Arc<Mutex<String>>);

impl RtspFixture {
    fn start(clip: &Path) -> Result<Self, String> {
        Self::start_with_publisher(clip, Self::spawn_publisher)
    }

    fn start_authenticated(clip: &Path, username: &str, password: &str) -> Result<Self, String> {
        Self::start_with_publisher_from_base(
            Self::start_zero_media_with_auth(Some((username, password)))?,
            clip,
            Self::spawn_publisher,
        )
    }

    fn start_undecodable(bytes: &Path) -> Result<Self, String> {
        Self::start_with_publisher(bytes, Self::spawn_undecodable_publisher)
    }

    fn start_zero_media() -> Result<Self, String> {
        Self::start_zero_media_with_auth(None)
    }

    fn start_zero_media_with_auth(read_auth: Option<(&str, &str)>) -> Result<Self, String> {
        let mediamtx = tool_path("VIGIL_MEDIAMTX_BIN", "mediamtx")
            .ok_or_else(|| "mediamtx is not available".to_string())?;
        let ffmpeg = tool_path("VIGIL_FFMPEG_BIN", "ffmpeg")
            .ok_or_else(|| "ffmpeg is not available".to_string())?;
        let port = free_port()?;
        let rtp_port = free_port()?;
        let rtcp_port = free_port()?;
        let url = format!("rtsp://127.0.0.1:{port}/lower-gate");
        let config_dir = tempfile::tempdir().map_err(|error| format!("rtsp temp dir: {error}"))?;
        let config_path = config_dir.path().join("mediamtx.yml");
        let auth_config = read_auth
            .map(|(username, password)| {
                format!(
                    "rtspAuthMethods: [digest]\npaths:\n  lower-gate:\n    source: publisher\n    readUser: {}\n    readPass: {}\n",
                    yaml_scalar(username),
                    yaml_scalar(password)
                )
            })
            .unwrap_or_else(|| {
                "paths:\n  lower-gate:\n    source: publisher\n".to_string()
            });
        fs::write(
            &config_path,
            format!(
                "rtspTransports: [tcp]\nrtspAddress: 127.0.0.1:{port}\nrtpAddress: 127.0.0.1:{rtp_port}\nrtcpAddress: 127.0.0.1:{rtcp_port}\nrtmp: no\nhls: no\nwebrtc: no\nsrt: no\nplayback: no\nmoq: no\n{auth_config}"
            ),
        )
        .map_err(|error| format!("write mediamtx config: {error}"))?;
        let mut mediamtx_child = Command::new(mediamtx)
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("spawn mediamtx: {error}"))?;
        let mediamtx_stdout = capture_pipe(mediamtx_child.stdout.take());
        let mediamtx_stderr = capture_pipe(mediamtx_child.stderr.take());
        if mediamtx_child.id() <= 1 {
            let _ = kill_tracked_child(&mut mediamtx_child, "invalid mediamtx child");
            return Err("mediamtx reported an invalid child pid".to_string());
        }
        if let Err(error) = wait_for_tcp_port(port, Duration::from_secs(3)) {
            let _ = kill_tracked_child(&mut mediamtx_child, "mediamtx startup failure");
            return Err(error);
        }
        thread::sleep(Duration::from_millis(400));
        Ok(Self {
            url,
            clip: None,
            ffmpeg_bin: ffmpeg,
            mediamtx: mediamtx_child,
            ffmpeg: None,
            mediamtx_stdout,
            mediamtx_stderr,
            ffmpeg_stdout: Arc::new(Mutex::new(String::new())),
            ffmpeg_stderr: Arc::new(Mutex::new(String::new())),
        })
    }

    fn start_with_publisher(
        clip: &Path,
        spawn_publisher: fn(&Path, &Path, &str) -> Result<CapturedChild, String>,
    ) -> Result<Self, String> {
        Self::start_with_publisher_from_base(Self::start_zero_media()?, clip, spawn_publisher)
    }

    fn start_with_publisher_from_base(
        mut fixture: Self,
        clip: &Path,
        spawn_publisher: fn(&Path, &Path, &str) -> Result<CapturedChild, String>,
    ) -> Result<Self, String> {
        let (ffmpeg_child, ffmpeg_stdout, ffmpeg_stderr) =
            spawn_publisher(&fixture.ffmpeg_bin, clip, &fixture.url)?;
        if ffmpeg_child.id() <= 1 {
            let mut ffmpeg_child = ffmpeg_child;
            let _ = kill_tracked_child(&mut ffmpeg_child, "invalid ffmpeg child");
            return Err("ffmpeg reported an invalid child pid".to_string());
        }
        fixture.clip = Some(clip.to_path_buf());
        fixture.ffmpeg = Some(ffmpeg_child);
        fixture.ffmpeg_stdout = ffmpeg_stdout;
        fixture.ffmpeg_stderr = ffmpeg_stderr;
        thread::sleep(Duration::from_millis(400));
        Ok(fixture)
    }

    fn spawn_publisher(ffmpeg: &Path, clip: &Path, url: &str) -> Result<CapturedChild, String> {
        let mut child = Command::new(ffmpeg)
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-copyts")
            .arg("-re")
            .arg("-stream_loop")
            .arg("-1")
            .arg("-i")
            .arg(clip)
            .arg("-an")
            .arg("-c:v")
            .arg("copy")
            .arg("-f")
            .arg("rtsp")
            .arg("-rtsp_transport")
            .arg("tcp")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("spawn ffmpeg: {error}"))?;
        let stdout = capture_pipe(child.stdout.take());
        let stderr = capture_pipe(child.stderr.take());
        Ok((child, stdout, stderr))
    }

    fn spawn_undecodable_publisher(
        ffmpeg: &Path,
        bytes: &Path,
        url: &str,
    ) -> Result<CapturedChild, String> {
        let mut child = Command::new(ffmpeg)
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-re")
            .arg("-stream_loop")
            .arg("-1")
            .arg("-f")
            .arg("h264")
            .arg("-i")
            .arg(bytes)
            .arg("-an")
            .arg("-c:v")
            .arg("copy")
            .arg("-f")
            .arg("rtsp")
            .arg("-rtsp_transport")
            .arg("tcp")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("spawn undecodable ffmpeg publisher: {error}"))?;
        let stdout = capture_pipe(child.stdout.take());
        let stderr = capture_pipe(child.stderr.take());
        Ok((child, stdout, stderr))
    }

    fn stop_publisher(&mut self) -> Result<(), String> {
        if let Some(ffmpeg) = self.ffmpeg.as_mut() {
            let _ = kill_tracked_child(ffmpeg, "ffmpeg publisher before stop");
        }
        self.ffmpeg = None;
        Ok(())
    }

    fn start_current_publisher(&mut self) -> Result<(), String> {
        let clip = self
            .clip
            .clone()
            .ok_or_else(|| "RTSP fixture has no publisher clip to start".to_string())?;
        let (child, stdout, stderr) = Self::spawn_publisher(&self.ffmpeg_bin, &clip, &self.url)?;
        if child.id() <= 1 {
            let mut child = child;
            let _ = kill_tracked_child(&mut child, "invalid restarted ffmpeg child");
            return Err("restarted ffmpeg reported an invalid child pid".to_string());
        }
        self.ffmpeg = Some(child);
        self.ffmpeg_stdout = stdout;
        self.ffmpeg_stderr = stderr;
        thread::sleep(Duration::from_millis(400));
        Ok(())
    }

    fn switch_clip(&mut self, clip: &Path) -> Result<(), String> {
        self.stop_publisher()?;
        self.clip = Some(clip.to_path_buf());
        let (child, stdout, stderr) = Self::spawn_publisher(&self.ffmpeg_bin, clip, &self.url)?;
        if child.id() <= 1 {
            let mut child = child;
            let _ = kill_tracked_child(&mut child, "invalid restarted ffmpeg child");
            return Err("restarted ffmpeg reported an invalid child pid".to_string());
        }
        self.ffmpeg = Some(child);
        self.ffmpeg_stdout = stdout;
        self.ffmpeg_stderr = stderr;
        thread::sleep(Duration::from_millis(400));
        Ok(())
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    fn credentialed_url(&self, username: &str, password: &str) -> String {
        let Some((scheme, rest)) = self.url.split_once("://") else {
            return self.url.clone();
        };
        format!(
            "{scheme}://{}:{}@{rest}",
            percent_encode_userinfo(username),
            percent_encode_userinfo(password)
        )
    }

    fn logs(&self) -> String {
        let mediamtx_stdout = self
            .mediamtx_stdout
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        let mediamtx_stderr = self
            .mediamtx_stderr
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        let ffmpeg_stdout = self
            .ffmpeg_stdout
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        let ffmpeg_stderr = self
            .ffmpeg_stderr
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        format!("{mediamtx_stdout}{mediamtx_stderr}{ffmpeg_stdout}{ffmpeg_stderr}")
    }
}

impl Drop for RtspFixture {
    fn drop(&mut self) {
        if let Some(ffmpeg) = self.ffmpeg.as_mut() {
            let _ = kill_tracked_child(ffmpeg, "ffmpeg RTSP publisher");
        }
        let _ = kill_tracked_child(&mut self.mediamtx, "mediamtx RTSP server");
    }
}

fn wait_for_tcp_port(port: u16, timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    while start.elapsed() < timeout {
        if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!("mediamtx did not open TCP port {port}"))
}

fn capture_pipe<T>(pipe: Option<T>) -> Arc<Mutex<String>>
where
    T: Read + Send + 'static,
{
    let logs = Arc::new(Mutex::new(String::new()));
    if let Some(mut pipe) = pipe {
        let captured = Arc::clone(&logs);
        thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if let Ok(mut logs) = captured.lock() {
                            logs.push_str(&String::from_utf8_lossy(&buffer[..read]));
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    logs
}

fn parsed_log_counter(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .filter_map(|line| {
            let (line_key, value) = line.split_once('=')?;
            (line_key.trim() == key).then(|| value.trim().parse::<u64>().ok())?
        })
        .next_back()
}

fn last_log_lines(text: &str, count: usize) -> String {
    let mut lines = text.lines().rev().take(count).collect::<Vec<_>>();
    lines.reverse();
    lines.join("\n")
}

fn count_video_frames(path: &Path) -> Result<u64, String> {
    let ffprobe =
        tool_path("VIGIL_FFPROBE_BIN", "ffprobe").ok_or_else(|| "ffprobe missing".to_string())?;
    let output = Command::new(ffprobe)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-count_frames")
        .arg("-show_entries")
        .arg("stream=nb_read_frames")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(path)
        .output()
        .map_err(|error| format!("run ffprobe for {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().parse::<u64>().ok())
        .ok_or_else(|| format!("ffprobe returned no frame count for {}", path.display()))
}

fn video_fps(path: &Path) -> Result<f64, String> {
    let ffprobe =
        tool_path("VIGIL_FFPROBE_BIN", "ffprobe").ok_or_else(|| "ffprobe missing".to_string())?;
    let output = Command::new(ffprobe)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-show_entries")
        .arg("stream=avg_frame_rate")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(path)
        .output()
        .map_err(|error| format!("run ffprobe fps for {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe fps failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let value = String::from_utf8_lossy(&output.stdout);
    let value = value.trim();
    if let Some((numerator, denominator)) = value.split_once('/') {
        let numerator = numerator
            .parse::<f64>()
            .map_err(|error| format!("parse fps numerator {numerator}: {error}"))?;
        let denominator = denominator
            .parse::<f64>()
            .map_err(|error| format!("parse fps denominator {denominator}: {error}"))?;
        if denominator != 0.0 {
            return Ok(numerator / denominator);
        }
    }
    value
        .parse::<f64>()
        .map_err(|error| format!("parse fps {value}: {error}"))
}

fn motion_positive_frame_count(path: &Path) -> Result<u64, String> {
    const WIDTH: usize = 64;
    const HEIGHT: usize = 36;
    let ffmpeg =
        tool_path("VIGIL_FFMPEG_BIN", "ffmpeg").ok_or_else(|| "ffmpeg missing".to_string())?;
    let output = Command::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(path)
        .arg("-vf")
        .arg(format!("scale={WIDTH}:{HEIGHT},format=gray"))
        .arg("-f")
        .arg("rawvideo")
        .arg("-")
        .output()
        .map_err(|error| format!("run ffmpeg motion probe for {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "ffmpeg motion probe failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let frame_size = WIDTH * HEIGHT;
    let mut previous: Option<&[u8]> = None;
    let mut motion_positive = 0_u64;
    for frame in output.stdout.chunks_exact(frame_size) {
        if let Some(previous) = previous {
            let diff: u64 = frame
                .iter()
                .zip(previous.iter())
                .map(|(current, previous)| current.abs_diff(*previous) as u64)
                .sum();
            if diff > frame_size as u64 * 3 {
                motion_positive += 1;
            }
        }
        previous = Some(frame);
    }
    Ok(motion_positive)
}

fn generate_transformed_person_clip(world: &FirstLightWorld) -> Result<PathBuf, String> {
    generate_person_clip_with_filter(
        world,
        "generated-detector-inputs",
        "person-transformed",
        "select='between(n\\,192\\,239)',setpts=N/10/TB,hflip,format=yuv420p",
    )
}

fn generate_motion_positive_person_clip(world: &FirstLightWorld) -> Result<PathBuf, String> {
    generate_person_clip_with_filter(
        world,
        "generated-detector-inputs",
        "person-motion-positive",
        "select='between(n\\,228\\,239)',setpts=N/10/TB,hflip,eq=brightness='if(gte(mod(n\\,20)\\,10)\\,0.20\\,-0.05)':eval=frame,format=yuv420p",
    )
}

fn generate_challenge_person_clip(world: &FirstLightWorld) -> Result<PathBuf, String> {
    generate_person_clip_with_filter(
        world,
        "generated-detector-inputs",
        "person-challenge",
        "format=yuv420p",
    )
}

fn generate_motion_positive_person_free_clip(world: &FirstLightWorld) -> Result<PathBuf, String> {
    generate_empty_scene_clip_with_filter(
        world,
        "generated-detector-inputs",
        "motion-positive-person-free",
        "drawbox=x=64:y='ih/2-48':w=96:h=96:color=white:t=fill:enable='lt(mod(n\\,20)\\,10)',drawbox=x='iw-160':y='ih/2-48':w=96:h=96:color=white:t=fill:enable='gte(mod(n\\,20)\\,10)',format=yuv420p",
    )
}

fn generate_empty_scene_clip_with_filter(
    world: &FirstLightWorld,
    directory: &str,
    stem: &str,
    video_filter: &str,
) -> Result<PathBuf, String> {
    generate_video_clip_with_filter(
        world,
        &world.empty_clip,
        directory,
        stem,
        video_filter,
        "mp4",
        None,
    )
}

fn generate_person_clip_with_filter(
    world: &FirstLightWorld,
    directory: &str,
    stem: &str,
    video_filter: &str,
) -> Result<PathBuf, String> {
    generate_video_clip_with_filter(
        world,
        &world.person_clip,
        directory,
        stem,
        video_filter,
        "mp4",
        None,
    )
}

fn generate_video_clip_with_filter(
    world: &FirstLightWorld,
    source_clip: &Path,
    directory: &str,
    stem: &str,
    video_filter: &str,
    extension: &str,
    output_format: Option<&str>,
) -> Result<PathBuf, String> {
    let ffmpeg =
        tool_path("VIGIL_FFMPEG_BIN", "ffmpeg").ok_or_else(|| "ffmpeg missing".to_string())?;
    let transformed_dir = world.data_dir.join(directory);
    fs::create_dir_all(&transformed_dir)
        .map_err(|error| format!("create {}: {error}", transformed_dir.display()))?;
    let output_path = transformed_dir.join(format!(
        "{stem}-{}.{}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        extension
    ));
    let mut command = Command::new(ffmpeg);
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(source_clip)
        .arg("-vf")
        .arg(video_filter)
        .arg("-frames:v")
        .arg("240")
        .arg("-an")
        .arg("-c:v")
        .arg("libx264")
        .arg("-profile:v")
        .arg("baseline")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-preset")
        .arg("ultrafast")
        .arg("-tune")
        .arg("zerolatency")
        .arg("-x264-params")
        .arg("repeat-headers=1")
        .arg("-g")
        .arg("12")
        .arg("-keyint_min")
        .arg("12")
        .arg("-sc_threshold")
        .arg("0");
    if let Some(format) = output_format {
        command.arg("-f").arg(format);
    }
    let output = command
        .arg(&output_path)
        .output()
        .map_err(|error| format!("run ffmpeg transform {stem}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffmpeg transform {stem} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    count_video_frames(&output_path).and_then(|frames| {
        if frames > 1 {
            Ok(output_path)
        } else {
            Err(format!("{stem} detector clip did not contain >1 frame"))
        }
    })
}

fn corrupt_detector_artifact(world: &FirstLightWorld) -> Result<PathBuf, String> {
    let path = world.data_dir.join("corrupt-yolox-tiny-coco.pth");
    fs::write(&path, b"not a valid detector checkpoint\n").map_err(|error| {
        format!(
            "write corrupt detector artifact {}: {error}",
            path.display()
        )
    })?;
    Ok(path)
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    Ok(format!("{digest:x}"))
}

struct DetectorProbe {
    status_success: bool,
    text: String,
    forward_event_nonce: Option<String>,
    forward_event_seq: Option<u64>,
    model_sha256: Option<String>,
    result_sha256: Option<String>,
    session_id: Option<String>,
    backend_id: Option<String>,
    model_forward_sha256: Option<String>,
    nms_sha256: Option<String>,
    detections: Option<u64>,
    class_name: Option<String>,
    confidence: Option<f64>,
    bbox: Option<String>,
    frame_index: Option<u64>,
}

struct DetectorOracle {
    status_success: bool,
    text: String,
    model_forward_sha256: Option<String>,
    result_sha256: Option<String>,
    detections: Option<u64>,
    class_name: Option<String>,
    confidence: Option<f64>,
    bbox: Option<String>,
}

fn run_detector_probe(world: &FirstLightWorld, clip: &Path) -> DetectorProbe {
    run_detector_probe_with_model(&world.detector_artifact, clip)
}

fn run_detector_probe_with_model(model: &Path, clip: &Path) -> DetectorProbe {
    run_detector_probe_with_model_and_env(model, clip, &[])
}

fn run_detector_probe_with_model_and_env(
    model: &Path,
    clip: &Path,
    extra_env: &[(&str, &str)],
) -> DetectorProbe {
    run_detector_probe_with_model_threshold_and_env(model, clip, DETECTOR_THRESHOLD, extra_env)
}

fn run_detector_probe_with_model_threshold(
    model: &Path,
    clip: &Path,
    confidence_threshold: f64,
) -> DetectorProbe {
    run_detector_probe_with_model_threshold_and_env(model, clip, confidence_threshold, &[])
}

fn run_detector_probe_with_model_threshold_and_env(
    model: &Path,
    clip: &Path,
    confidence_threshold: f64,
    extra_env: &[(&str, &str)],
) -> DetectorProbe {
    let mut command = Command::new(vigil_binary_path());
    command
        .arg("detector-probe")
        .arg("--model")
        .arg(model)
        .arg("--clip")
        .arg(clip)
        .arg("--sample-frames")
        .arg("5")
        .arg("--confidence-threshold")
        .arg(format!("{confidence_threshold}"));
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let observation = run_command_with_timeout(
        command,
        "vigil detector-probe",
        detector_subprocess_timeout(),
    );
    let status_success = observation.status_success;
    let text = command_text(&observation);
    DetectorProbe {
        status_success,
        forward_event_nonce: parsed_text_field(&text, "forward-event-nonce"),
        forward_event_seq: parsed_text_field(&text, "forward-event-seq")
            .and_then(|value| value.parse().ok()),
        model_sha256: parsed_text_field(&text, "model-sha256"),
        result_sha256: parsed_text_field(&text, "result-sha256"),
        session_id: parsed_text_field(&text, "detector-session-id"),
        backend_id: parsed_text_field(&text, "detector-backend"),
        model_forward_sha256: parsed_text_field(&text, "model-forward-sha256"),
        nms_sha256: parsed_text_field(&text, "nms-sha256"),
        detections: parsed_text_field(&text, "detections").and_then(|value| value.parse().ok()),
        class_name: parsed_text_field(&text, "class"),
        confidence: parsed_text_field(&text, "confidence").and_then(|value| value.parse().ok()),
        bbox: parsed_text_field(&text, "bbox"),
        frame_index: parsed_text_field(&text, "frame-index").and_then(|value| value.parse().ok()),
        text,
    }
}

fn run_independent_detector_oracle(model: &Path, clip: &Path) -> DetectorOracle {
    let oracle = detector_oracle_binary_path();
    let mut command = Command::new(&oracle);
    command
        .arg("--model")
        .arg(model)
        .arg("--clip")
        .arg(clip)
        .arg("--sample-frames")
        .arg("5");
    let observation = run_command_with_timeout(
        command,
        "independent detector oracle",
        detector_subprocess_timeout(),
    );
    let status_success = observation.status_success;
    let text = command_text(&observation);
    DetectorOracle {
        status_success,
        model_forward_sha256: parsed_text_field(&text, "model-forward-sha256"),
        result_sha256: parsed_text_field(&text, "result-sha256"),
        detections: parsed_text_field(&text, "detections").and_then(|value| value.parse().ok()),
        class_name: parsed_text_field(&text, "class"),
        confidence: parsed_text_field(&text, "confidence").and_then(|value| value.parse().ok()),
        bbox: parsed_text_field(&text, "bbox"),
        text,
    }
}

fn detector_subprocess_timeout() -> Duration {
    let seconds = std::env::var("VIGIL_DETECTOR_SUBPROCESS_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_DETECTOR_SUBPROCESS_TIMEOUT_SECS);
    Duration::from_secs(seconds)
}

fn detector_oracle_binary_path() -> PathBuf {
    option_env!("CARGO_BIN_EXE_yolox-burn-oracle")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            vigil_binary_path()
                .parent()
                .map(|parent| parent.join("yolox-burn-oracle"))
                .unwrap_or_else(|| PathBuf::from("target/debug/yolox-burn-oracle"))
        })
}

fn detector_oracle_source_path() -> PathBuf {
    workspace_root()
        .join("crates")
        .join("vigil")
        .join("src")
        .join("bin")
        .join("yolox_burn_oracle.rs")
}

fn assert_detector_oracle_is_repo_owned_and_independent(failures: &mut Vec<String>) {
    let oracle_source_path = detector_oracle_source_path();
    let oracle_binary = detector_oracle_binary_path();
    let vigil_binary = vigil_binary_path();
    if oracle_binary == vigil_binary {
        failures.push("detector oracle resolved to the production vigil binary".to_string());
    }
    if oracle_binary
        .file_name()
        .and_then(OsStr::to_str)
        .is_none_or(|name| name == "vigil" || name.contains("detector-probe"))
    {
        failures.push(format!(
            "detector oracle binary is not a distinct repo-owned oracle target: {}",
            oracle_binary.display()
        ));
    }
    let source = match fs::read_to_string(&oracle_source_path) {
        Ok(source) => source,
        Err(error) => {
            failures.push(format!(
                "repo-owned detector oracle source was not readable at {}: {error}",
                oracle_source_path.display()
            ));
            return;
        }
    };
    for required in [
        "Burn YOLOX",
        "Tensor",
        "Yolox::yolox_tiny",
        "PytorchStore::from_file",
        "decode_sampled_rgb_frames",
        "media_pipeline::decode_video_file",
        "sampled_detector_rgb",
        "Tensor::<B, 4>",
        ".forward(",
        "load_record",
        "nms",
        "run_nms",
        "model-forward-sha256",
        "result-sha256",
    ] {
        if !source.contains(required) {
            failures.push(format!(
                "repo-owned detector oracle source omitted independent model-forward marker {required}"
            ));
        }
    }
    for forbidden in [
        "use vigil",
        "vigil::",
        "use crate::",
        "crate::detector",
        "detector-probe",
        "detector_probe",
        "run_detector_probe",
        "vigil_binary_path",
        "yolox_tiny_pretrained",
        ".download(",
        "#[allow(dead_code)]",
        "dead_code",
        PERSON_MODEL_FORWARD_SHA256,
        PERSON_NMS_SHA256,
        PERSON_GOLDEN_BBOX,
    ] {
        if source.contains(forbidden) {
            failures.push(format!(
                "repo-owned detector oracle source can share or hardcode the production probe path: {forbidden}"
            ));
        }
    }
    for forbidden in [
        "fs::read(clip",
        "fs::read(&args.clip",
        "read_to_end",
        "byte-sampling",
        "hash-only",
        "fake Tensor",
        "concat",
    ] {
        if source.contains(forbidden) {
            failures.push(format!(
                "repo-owned detector oracle source can derive results from clip bytes instead of decoded model input: {forbidden}"
            ));
        }
    }
    let main_source = function_source_slice(&source, "main").unwrap_or_default();
    if !main_source.contains("run_oracle") {
        failures.push(
            "repo-owned detector oracle main does not execute a run_oracle inference path"
                .to_string(),
        );
    }
    if main_source
        .find("process::exit(2)")
        .zip(main_source.find("run_oracle"))
        .is_some_and(|(exit, run)| exit < run)
    {
        failures.push(
            "repo-owned detector oracle exits before executing the inference path".to_string(),
        );
    }
    let run_source = function_source_slice(&source, "run_oracle").unwrap_or_default();
    for required in [
        "load_record",
        "decode_frames_to_tensor",
        ".forward(",
        "nms(",
        "model-forward-sha256",
        "result-sha256",
        "println!",
    ] {
        if !run_source.contains(required) {
            failures.push(format!(
                "repo-owned detector oracle run_oracle path omitted executed inference marker {required}"
            ));
        }
    }
    if source.contains(&PERSON_GOLDEN_CONFIDENCE.to_string()) {
        failures.push(
            "repo-owned detector oracle source hardcodes the golden confidence value".to_string(),
        );
    }
}

fn parsed_text_field(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (line_key, value) = line.split_once('=')?;
        (line_key.trim() == key).then(|| value.trim().to_string())
    })
}

fn parsed_line_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split(|character: char| character.is_whitespace() || character == ',' || character == ';')
        .filter_map(|token| token.split_once('='))
        .find_map(|(line_key, value)| {
            (line_key.trim() == key).then(|| value.trim().trim_matches('"').trim_matches('\''))
        })
}

fn parsed_u64_field(line: &str, key: &str) -> Option<u64> {
    parsed_line_field(line, key).and_then(|value| value.parse().ok())
}

fn unique_probe_nonce(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    )
}

fn read_probe_events(
    path: &Path,
    nonce: &str,
    event_type: &str,
    label: &str,
    failures: &mut Vec<String>,
) -> Vec<String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            failures.push(format!(
                "{label} probe {} was not readable: {error}",
                path.display()
            ));
            return Vec::new();
        }
    };
    if !text.contains(nonce) {
        failures.push(format!("{label} probe omitted the test nonce"));
    }
    if text.to_ascii_lowercase().contains("writer=vigil")
        || text.to_ascii_lowercase().contains("crate=vigil")
    {
        failures.push(format!(
            "{label} probe identified Vigil as the event writer"
        ));
    }
    let events = text
        .lines()
        .filter(|line| line.contains(event_type) && line.contains(nonce))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if events.is_empty() {
        failures.push(format!("{label} probe recorded no {event_type} events"));
    }
    let sequences = events
        .iter()
        .filter_map(|line| parsed_u64_field(line, "seq"))
        .collect::<Vec<_>>();
    if sequences.len() != events.len() {
        failures.push(format!("{label} probe omitted seq on at least one event"));
    }
    if sequences.windows(2).any(|window| window[1] <= window[0]) {
        failures.push(format!(
            "{label} probe seq fields were not strictly increasing"
        ));
    }
    events
}

fn assert_detector_forward_event(
    events: &[String],
    label: &str,
    clip_sha256: Option<&str>,
    model_sha256: &str,
    model_forward_sha256: Option<&str>,
    result_sha256: Option<&str>,
    failures: &mut Vec<String>,
) {
    let Some(clip_sha256) = clip_sha256 else {
        failures.push(format!("{label} detector forward proof had no clip digest"));
        return;
    };
    let matching = events
        .iter()
        .filter(|event| event.contains(clip_sha256))
        .collect::<Vec<_>>();
    if matching.is_empty() {
        failures.push(format!(
            "{label} detector forward observer did not record clip_sha256={clip_sha256}"
        ));
        return;
    }
    let event = matching[0];
    for expected in [
        DETECTOR_FORWARD_EVENT_TYPE,
        DETECTOR_FORWARD_OBSERVER_TRAIT,
        model_sha256,
        clip_sha256,
        "emitter=detector-backend-forward",
        "stage=model-tensor-forward",
        "nms=true",
    ] {
        if !event.contains(expected) {
            failures.push(format!(
                "{label} detector forward event omitted expected proof field {expected}"
            ));
        }
    }
    if let Some(model_forward_sha256) = model_forward_sha256
        && !event.contains(model_forward_sha256)
    {
        failures.push(format!(
            "{label} detector forward event did not match model-forward digest {model_forward_sha256}"
        ));
    }
    if let Some(result_sha256) = result_sha256
        && !event.contains(result_sha256)
    {
        failures.push(format!(
            "{label} detector forward event did not match result digest {result_sha256}"
        ));
    }
}

fn assert_detector_probe_is_derived_from_forward_event(
    events: &[String],
    label: &str,
    clip_sha256: Option<&str>,
    expected_nonce: &str,
    probe: Option<&DetectorProbe>,
    failures: &mut Vec<String>,
) {
    let Some(probe) = probe else {
        failures.push(format!("{label} detector probe result was unavailable"));
        return;
    };
    let Some(clip_sha256) = clip_sha256 else {
        failures.push(format!("{label} detector probe had no clip digest"));
        return;
    };
    let Some(event) = events.iter().find(|event| event.contains(clip_sha256)) else {
        return;
    };
    let Some(event_seq) = parsed_u64_field(event, "seq") else {
        failures.push(format!(
            "{label} detector forward event omitted seq for probe linkage"
        ));
        return;
    };
    if probe.forward_event_nonce.as_deref() != Some(expected_nonce) {
        failures.push(format!(
            "{label} detector probe did not identify the backend event nonce"
        ));
    }
    if probe.forward_event_seq != Some(event_seq) {
        failures.push(format!(
            "{label} detector probe was not derived from backend event seq={event_seq}"
        ));
    }
}

fn assert_detector_probe_matches_oracle(
    label: &str,
    probe: Option<&DetectorProbe>,
    oracle: Option<&DetectorOracle>,
    failures: &mut Vec<String>,
) {
    let Some(probe) = probe else {
        failures.push(format!("{label} detector probe result was unavailable"));
        return;
    };
    let Some(oracle) = oracle else {
        failures.push(format!(
            "{label} independent detector oracle result was unavailable"
        ));
        return;
    };
    if !oracle.status_success {
        failures.push(format!(
            "{label} independent detector oracle did not run successfully: {}",
            oracle.text
        ));
    }
    for (field, probe_value, oracle_value) in [
        (
            "model-forward-sha256",
            probe.model_forward_sha256.as_deref(),
            oracle.model_forward_sha256.as_deref(),
        ),
        (
            "result-sha256",
            probe.result_sha256.as_deref(),
            oracle.result_sha256.as_deref(),
        ),
        (
            "class",
            probe.class_name.as_deref(),
            oracle.class_name.as_deref(),
        ),
        ("bbox", probe.bbox.as_deref(), oracle.bbox.as_deref()),
    ] {
        if probe_value != oracle_value {
            failures.push(format!(
                "{label} detector probe {field} did not match independent oracle"
            ));
        }
    }
    if probe.detections != oracle.detections {
        failures.push(format!(
            "{label} detector probe detection count did not match independent oracle"
        ));
    }
    if probe.confidence != oracle.confidence {
        failures.push(format!(
            "{label} detector probe confidence did not match independent oracle"
        ));
    }
}

fn read_network_trace(prefix: &Path) -> Vec<String> {
    observed_outbound_network_lines(&read_network_trace_raw(prefix))
}

fn read_network_trace_lines(prefix: &Path) -> Vec<String> {
    read_network_trace_raw(prefix)
        .lines()
        .filter(|line| network_line_is_inet_event(line))
        .map(str::to_string)
        .collect()
}

fn read_network_trace_raw(prefix: &Path) -> String {
    let dir = prefix.parent().unwrap_or_else(|| Path::new("."));
    let prefix_name = prefix
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("network");
    let mut raw = String::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if (name == prefix_name || name.starts_with(&format!("{prefix_name}.")))
                && let Ok(text) = fs::read_to_string(&path)
            {
                raw.push_str(&text);
            }
        }
    }
    raw
}

fn network_trace_file_count(prefix: &Path) -> usize {
    let dir = prefix.parent().unwrap_or_else(|| Path::new("."));
    let prefix_name = prefix
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("network");
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry
                        .path()
                        .file_name()
                        .and_then(OsStr::to_str)
                        .is_some_and(|name| {
                            name == prefix_name || name.starts_with(&format!("{prefix_name}."))
                        })
                })
                .count()
        })
        .unwrap_or_default()
}

fn observed_outbound_network_lines(raw: &str) -> Vec<String> {
    raw.lines()
        .filter(|line| network_line_is_inet_event(line))
        .map(str::to_string)
        .collect()
}

fn network_line_is_inet_event(line: &str) -> bool {
    let traced_connect = line.contains("connect(") || line.contains("sendto(");
    let internet_socket = line.contains("AF_INET") || line.contains("AF_INET6");
    traced_connect && internet_socket
}

fn network_line_connects_to_port(line: &str, port: u16) -> bool {
    line.contains("connect(")
        && line.contains("127.0.0.1")
        && line.contains(&format!("sin_port=htons({port})"))
}

fn rtsp_port(rtsp_url: &str) -> Option<u16> {
    let after_scheme = rtsp_url.split_once("://")?.1;
    let authority = after_scheme.split('/').next()?;
    authority.rsplit_once(':')?.1.parse().ok()
}

fn vigil_binary_path() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_vigil") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_vigil") {
        return PathBuf::from(path);
    }
    workspace_root()
        .join("target")
        .join("debug")
        .join(if cfg!(windows) { "vigil.exe" } else { "vigil" })
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn tool_path(env_key: &str, binary: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(env_key).map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(binary))
                .find(|candidate| candidate.is_file())
        })
        .or_else(|| {
            let cached = workspace_root()
                .join("target")
                .join("vigil-test-tools")
                .join("mediamtx")
                .join("v1.19.1")
                .join(binary);
            cached.is_file().then_some(cached)
        })
}

fn free_port() -> Result<u16, String> {
    static ALLOCATED_PORTS: OnceLock<Mutex<BTreeSet<u16>>> = OnceLock::new();

    let allocated = ALLOCATED_PORTS.get_or_init(|| Mutex::new(BTreeSet::new()));
    for _ in 0..128 {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("allocate local TCP port: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("read local TCP port: {error}"))?
            .port();
        if allocated
            .lock()
            .map(|mut ports| ports.insert(port))
            .unwrap_or(false)
        {
            return Ok(port);
        }
    }
    Err("could not allocate a unique local TCP port".to_string())
}

fn wait_for_health(port: u16, expected: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if health_status(port) == Some(expected) {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

fn wait_for_unhealthy_health(port: u16, timeout: Duration) -> Option<u16> {
    let start = Instant::now();
    let mut last = None;
    while start.elapsed() < timeout {
        last = health_status(port);
        if matches!(last, Some(status) if status != 200) {
            return last;
        }
        thread::sleep(Duration::from_millis(25));
    }
    last
}

fn health_status(port: u16) -> Option<u16> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(100)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = stream.write_all(b"GET /health HTTP/1.1\r\nhost: localhost\r\n\r\n");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
}

fn toml_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

fn toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn yaml_scalar(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn percent_encode_userinfo(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn clip_path_count(observations: &[Observation]) -> usize {
    observations
        .iter()
        .flat_map(|observation| observation.evidence.iter())
        .filter(|evidence| evidence.kind == EvidenceKind::VideoSegment)
        .map(|evidence| evidence.source_ref.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

fn observation_confidences(observations: &[Observation]) -> Vec<f64> {
    observations
        .iter()
        .filter_map(|observation| {
            observation
                .observed_properties
                .get("confidence")
                .and_then(|value| value.as_f64())
        })
        .collect()
}

fn observation_detector_result_digests(observations: &[Observation]) -> Vec<String> {
    observations
        .iter()
        .filter_map(|observation| {
            observation
                .properties
                .get("detector_result_sha256")
                .and_then(|value| value.as_str())
                .map(ToString::to_string)
        })
        .collect()
}

fn observation_property<'a>(observation: &'a Observation, key: &str) -> Option<&'a str> {
    observation
        .properties
        .get(key)
        .and_then(|value| value.as_str())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn observation_has_linked_detector_forward_event(
    observation: &Observation,
    forward_events: &[String],
) -> bool {
    let Some(clip_sha256) = observation_property(observation, "clip_sha256") else {
        return false;
    };
    let Some(model_forward_sha256) =
        observation_property(observation, "detector_model_forward_sha256")
    else {
        return false;
    };
    let Some(nms_sha256) = observation_property(observation, "detector_nms_sha256") else {
        return false;
    };
    let Some(result_sha256) = observation_property(observation, "detector_result_sha256") else {
        return false;
    };
    if ![clip_sha256, model_forward_sha256, nms_sha256, result_sha256]
        .into_iter()
        .all(is_sha256_hex)
    {
        return false;
    }
    forward_events.iter().any(|event| {
        event.contains(clip_sha256)
            && event.contains(model_forward_sha256)
            && event.contains(nms_sha256)
            && event.contains(result_sha256)
    })
}

fn run_out_of_order_observed_at_fixture(world: &mut FirstLightWorld, failures: &mut Vec<String>) {
    let _ = world.run_runtime_until_observation_count(1);
    let Ok(store) = world.open_store() else {
        failures.push("event-order fixture could not open cg store after runtime seed".to_string());
        return;
    };
    let authority = match cg_authority(Some(&store)) {
        Ok(authority) => authority,
        Err(error) => {
            failures.push(format!(
                "event-order fixture could not read seeded cg authority: {error}"
            ));
            return;
        }
    };
    let baseline = CgBaseline::from(&authority);
    record_out_of_order_observed_at_fixture(&store, &baseline, failures);
}

fn record_out_of_order_observed_at_fixture(
    store: &Store,
    baseline: &CgBaseline,
    failures: &mut Vec<String>,
) {
    let early_observed_at = Utc::now();
    let late_observed_at = early_observed_at + chrono::Duration::seconds(600);
    if let Err(error) = record_ordering_observation(store, baseline, "late", late_observed_at, 0.91)
    {
        failures.push(format!(
            "could not record late observed_at fixture: {error}"
        ));
    }
    thread::sleep(Duration::from_millis(10));
    if let Err(error) =
        record_ordering_observation(store, baseline, "early", early_observed_at, 0.82)
    {
        failures.push(format!(
            "could not record early observed_at fixture: {error}"
        ));
    }
}

fn record_ordering_observation(
    store: &Store,
    baseline: &CgBaseline,
    label: &str,
    observed_at: DateTime<Utc>,
    confidence: f64,
) -> Result<(), String> {
    let observation_id = ObservationId::new_v7();
    let evidence_id = EvidenceId::new_v7();
    let source_ref = format!("vigil-edge:clip/event-order-{label}.h264");
    let detector_image_ref = format!("vigil-edge:clip/event-order-{label}-detector-frame-24.png");
    let video_evidence = EvidenceRef {
        id: evidence_id,
        observation_id,
        context_id: baseline.context.id,
        kind: EvidenceKind::VideoSegment,
        source_ref: source_ref.clone(),
        mime_type: Some("video/h264".to_string()),
        captured_at: Some(observed_at),
        producer: EvidenceProducer {
            system: "vigil".to_string(),
            model_name: DETECTOR_MODEL_ID.to_string(),
            model_version: "0.1".to_string(),
            pipeline_version: "first-run".to_string(),
        },
        retention_status: RetentionStatus::RetainedExternal,
        ..EvidenceRef::default()
    };
    let image_evidence = EvidenceRef {
        id: EvidenceId::new_v7(),
        observation_id,
        context_id: baseline.context.id,
        kind: EvidenceKind::ImageFrame,
        source_ref: detector_image_ref.clone(),
        mime_type: Some("image/png".to_string()),
        content_hash: Some(format!(
            "sha256:{:x}",
            Sha256::digest(detector_image_ref.as_bytes())
        )),
        captured_at: Some(observed_at),
        frame_index: Some(24),
        region: Some(json!({
            "bbox": PERSON_GOLDEN_BBOX,
            "coordinate_space": "detector_input",
            "width": 640,
            "height": 640
        })),
        producer: EvidenceProducer {
            system: "vigil".to_string(),
            model_name: DETECTOR_MODEL_ID.to_string(),
            model_version: "0.1".to_string(),
            pipeline_version: "first-run".to_string(),
        },
        retention_status: RetentionStatus::RetainedExternal,
        ..EvidenceRef::default()
    };
    let mut observed_properties = BTreeMap::new();
    observed_properties.insert("class".to_string(), Value::String("person".to_string()));
    observed_properties.insert("confidence".to_string(), json!(confidence));
    observed_properties.insert(
        "bbox".to_string(),
        Value::String(PERSON_GOLDEN_BBOX.to_string()),
    );
    observed_properties.insert("frame_index".to_string(), json!(24));
    observed_properties.insert(
        "detector_decision_id".to_string(),
        Value::String(baseline.decision.id.to_string()),
    );
    let mut properties = BTreeMap::new();
    properties.insert("clip_frame_count".to_string(), json!(48));
    properties.insert(
        "baseline_intention_id".to_string(),
        Value::String(baseline.intention.id.to_string()),
    );
    properties.insert(
        "detector_backend".to_string(),
        Value::String(PERSON_MODEL_BACKEND.to_string()),
    );
    properties.insert(
        "detector_session_id".to_string(),
        Value::String(format!("event-order-{label}")),
    );
    properties.insert(
        "detector_model_sha256".to_string(),
        Value::String(DETECTOR_ARTIFACT_SHA256.to_string()),
    );
    properties.insert(
        "detector_model_forward_sha256".to_string(),
        Value::String(PERSON_MODEL_FORWARD_SHA256.to_string()),
    );
    properties.insert(
        "detector_nms_sha256".to_string(),
        Value::String(PERSON_NMS_SHA256.to_string()),
    );
    properties.insert(
        "detector_result_sha256".to_string(),
        Value::String(PERSON_RESULT_SHA256.to_string()),
    );
    let clip_digest = Sha256::digest(source_ref.as_bytes());
    properties.insert(
        "clip_sha256".to_string(),
        Value::String(format!("{clip_digest:x}")),
    );
    properties.insert(
        "detector_evidence_ref".to_string(),
        Value::String(detector_image_ref.clone()),
    );
    properties.insert(
        "detector_evidence_sha256".to_string(),
        Value::String(format!(
            "{:x}",
            Sha256::digest(detector_image_ref.as_bytes())
        )),
    );
    properties.insert("detector_input_width".to_string(), json!(640));
    properties.insert("detector_input_height".to_string(), json!(640));
    store
        .record_observation(RecordObservation {
            id: observation_id,
            entity_id: baseline.camera.id,
            context_id: baseline.context.id,
            observation_type: "detection".to_string(),
            source: "vigil".to_string(),
            observed_at,
            evidence: vec![video_evidence, image_evidence],
            observed_properties,
            state_delta: BTreeMap::new(),
            properties,
            embeddings: Vec::new(),
        })
        .map_err(|error| format!("record observation: {error}"))?;
    Ok(())
}

#[derive(Clone)]
struct CgBaseline {
    context: Context,
    camera: Entity,
    decision: Decision,
    intention: Intention,
}

impl From<&CgAuthority> for CgBaseline {
    fn from(authority: &CgAuthority) -> Self {
        Self {
            context: authority.context.clone(),
            camera: authority.camera.clone(),
            decision: authority.decision.clone(),
            intention: authority.intention.clone(),
        }
    }
}

struct CgAuthority {
    context: Context,
    camera: Entity,
    decision: Decision,
    intention: Intention,
    observation: Observation,
    evidence: EvidenceRef,
}

fn cg_authority(store: Option<&Store>) -> Result<CgAuthority, String> {
    let store = store.ok_or_else(|| "cg store was not available".to_string())?;
    let mut observations = store
        .list_observations(None)
        .map_err(|error| format!("list observations: {error}"))?;
    observations.sort_by(|a, b| {
        b.observed_at
            .cmp(&a.observed_at)
            .then_with(|| b.id.as_uuid().cmp(&a.id.as_uuid()))
    });
    let observation = observations
        .first()
        .cloned()
        .ok_or_else(|| "no cg Observation was landed".to_string())?;
    let evidence = observation
        .evidence
        .first()
        .cloned()
        .ok_or_else(|| "landed Observation carried no evidence ref".to_string())?;
    let context = store
        .get_context(observation.context_id)
        .map_err(|error| format!("get context: {error}"))?
        .ok_or_else(|| format!("context {} was not found", observation.context_id))?;
    let camera = store
        .get_entity(observation.entity_id)
        .map_err(|error| format!("get camera entity: {error}"))?
        .ok_or_else(|| format!("camera entity {} was not found", observation.entity_id))?;
    let decisions = detector_decisions(store)?;
    let decision = decisions
        .into_iter()
        .find(|decision| {
            decision.context_id == observation.context_id
                && decision
                    .properties
                    .get("model_id")
                    .and_then(|value| value.as_str())
                    == Some(DETECTOR_MODEL_ID)
                && decision
                    .properties
                    .get("threshold")
                    .and_then(|value| value.as_f64())
                    .map(|value| (value - DETECTOR_THRESHOLD).abs() < f64::EPSILON)
                    .unwrap_or(false)
                && !decision.intention_ids.is_empty()
        })
        .ok_or_else(|| "no detector Decision matched the configured model/threshold".to_string())?;
    let intention_id = *decision
        .intention_ids
        .first()
        .ok_or_else(|| "detector Decision served no Intention".to_string())?;
    let intention = store
        .get_intention(intention_id)
        .map_err(|error| format!("get intention: {error}"))?
        .ok_or_else(|| format!("intention {intention_id} was not found"))?;
    if context.name != SITE_NAME {
        return Err(format!(
            "cg Context name was {}, expected {SITE_NAME}",
            context.name
        ));
    }
    if camera.name != CAMERA_NAME {
        return Err(format!(
            "camera Entity name was {}, expected {CAMERA_NAME}",
            camera.name
        ));
    }
    if decision.context_id != context.id || decision.context_id != observation.context_id {
        return Err("detector Decision was not under the Observation site Context".to_string());
    }
    if !decision.intention_ids.contains(&intention.id) {
        return Err("detector Decision did not serve the exact baseline Intention".to_string());
    }
    if intention.context_id != context.id {
        return Err("baseline Intention was not under the site Context".to_string());
    }
    if intention.description != BASELINE_INTENTION_DESCRIPTION {
        return Err(format!(
            "baseline Intention description was {}, expected {BASELINE_INTENTION_DESCRIPTION}",
            intention.description
        ));
    }
    if !intention.tags.iter().any(|tag| tag == "camera") {
        return Err("baseline Intention did not carry the camera tag".to_string());
    }
    Ok(CgAuthority {
        context,
        camera,
        decision,
        intention,
        observation,
        evidence,
    })
}

fn detector_decisions(store: &Store) -> Result<Vec<Decision>, String> {
    let mut decisions = Vec::new();
    for entry in store
        .audit_query(AuditFilter::default())
        .map_err(|error| format!("query audit: {error}"))?
    {
        let AuditTarget::Decision(id) = entry.target else {
            continue;
        };
        if let Some(decision) = store
            .get_decision(id)
            .map_err(|error| format!("get decision {id}: {error}"))?
        {
            decisions.push(decision);
        }
    }
    Ok(decisions)
}

fn assert_why_matches_authority(text: &str, authority: &CgAuthority, failures: &mut Vec<String>) {
    let detector_image_ref = authority_detector_image_ref(authority);
    for (label, expected) in [
        ("Observation id", authority.observation.id.to_string()),
        ("clip ref", authority.evidence.source_ref.clone()),
        ("Decision id", authority.decision.id.to_string()),
        ("Intention id", authority.intention.id.to_string()),
        ("Context id", authority.context.id.to_string()),
        ("camera id", authority.camera.id.to_string()),
        ("site name", authority.context.name.clone()),
        ("camera name", authority.camera.name.clone()),
        (
            "Intention description",
            authority.intention.description.clone(),
        ),
        ("model id", DETECTOR_MODEL_ID.to_string()),
        ("threshold", format!("threshold={DETECTOR_THRESHOLD}")),
    ] {
        if !text.contains(&expected) {
            failures.push(format!("why output omitted cg {label}: {expected}"));
        }
    }
    if let Some(detector_image_ref) = detector_image_ref {
        if !text.contains(detector_image_ref) {
            failures.push(format!(
                "why output omitted detector image evidence ref: {detector_image_ref}"
            ));
        }
    } else {
        failures.push("cg Observation omitted detector image evidence for why check".to_string());
    }
    assert_text_contains_detector_bbox_and_frame("why", text, &authority.observation, failures);
}

fn assert_why_excludes_non_requested_event(
    text: &str,
    label: &str,
    observation: Option<&Observation>,
    decision: Option<&Decision>,
    failures: &mut Vec<String>,
) {
    if let Some(observation) = observation {
        let observation_id = observation.id.to_string();
        if text.contains(&observation_id) {
            failures.push(format!(
                "{label} why output included non-requested Observation id {observation_id}"
            ));
        }
        let observed_at = observation.observed_at.to_rfc3339();
        if text.contains(&observed_at) {
            failures.push(format!(
                "{label} why output included non-requested event time {observed_at}"
            ));
        }
        for evidence in &observation.evidence {
            if text.contains(&evidence.source_ref) {
                failures.push(format!(
                    "{label} why output included non-requested clip ref {}",
                    evidence.source_ref
                ));
            }
        }
    }
    if let Some(decision) = decision {
        let decision_id = decision.id.to_string();
        if text.contains(&decision_id) {
            failures.push(format!(
                "{label} why output included non-requested Decision id {decision_id}"
            ));
        }
        if let Some(threshold) = decision
            .properties
            .get("threshold")
            .and_then(|value| value.as_f64())
        {
            let exact = format!("threshold={threshold}");
            if text.contains(&exact) {
                failures.push(format!(
                    "{label} why output included non-requested detector threshold {exact}"
                ));
            }
        }
    }
}

fn assert_events_match_authority(text: &str, authority: &CgAuthority, failures: &mut Vec<String>) {
    let detector_image_ref = authority_detector_image_ref(authority);
    for (label, expected) in [
        ("Observation id", authority.observation.id.to_string()),
        ("event time", authority.observation.observed_at.to_rfc3339()),
        ("camera name", authority.camera.name.clone()),
        ("clip ref", authority.evidence.source_ref.clone()),
    ] {
        if !text.contains(&expected) {
            failures.push(format!("events output omitted cg {label}: {expected}"));
        }
    }
    if let Some(class_name) = authority
        .observation
        .observed_properties
        .get("class")
        .and_then(|value| value.as_str())
    {
        if !text.contains(class_name) {
            failures.push(format!("events output omitted detector class {class_name}"));
        }
    } else {
        failures
            .push("cg Observation omitted detector class for events authority check".to_string());
    }
    if let Some(confidence) = authority
        .observation
        .observed_properties
        .get("confidence")
        .and_then(|value| value.as_f64())
    {
        let confidence_text = format!("{confidence:.2}");
        if !text.contains(&confidence_text) && !text.contains(&confidence.to_string()) {
            failures.push(format!(
                "events output omitted detector confidence from cg Observation: {confidence}"
            ));
        }
    } else {
        failures.push(
            "cg Observation omitted detector confidence for events authority check".to_string(),
        );
    }
    if let Some(detector_image_ref) = detector_image_ref {
        if !text.contains(detector_image_ref) {
            failures.push(format!(
                "events output omitted detector image evidence ref: {detector_image_ref}"
            ));
        }
    } else {
        failures
            .push("cg Observation omitted detector image evidence for events check".to_string());
    }
    assert_text_contains_detector_bbox_and_frame("events", text, &authority.observation, failures);
}

fn authority_detector_image_ref(authority: &CgAuthority) -> Option<&str> {
    authority
        .observation
        .evidence
        .iter()
        .find(|evidence| evidence.kind == EvidenceKind::ImageFrame)
        .map(|evidence| evidence.source_ref.as_str())
}

fn assert_text_contains_detector_bbox_and_frame(
    label: &str,
    text: &str,
    observation: &Observation,
    failures: &mut Vec<String>,
) {
    if let Some(bbox) = observation
        .observed_properties
        .get("bbox")
        .and_then(Value::as_str)
    {
        if !text.contains(&format!("bbox={bbox}")) {
            failures.push(format!("{label} output omitted detector bbox {bbox}"));
        }
    } else {
        failures.push(format!(
            "cg Observation omitted detector bbox for {label} check"
        ));
    }
    if let Some(frame_index) = observation
        .observed_properties
        .get("frame_index")
        .and_then(Value::as_u64)
    {
        if !text.contains(&format!("frame_index={frame_index}")) {
            failures.push(format!(
                "{label} output omitted detector frame_index {frame_index}"
            ));
        }
    } else {
        failures.push(format!(
            "cg Observation omitted detector frame_index for {label} check"
        ));
    }
}

fn poison_non_cg_sidecars(world: &FirstLightWorld) {
    let _ = fs::write(
        world.data_dir.join("vigil-readback.json"),
        "{\"observation\":\"poison-sidecar\"}",
    );
    let _ = fs::write(world.data_dir.join("events.json"), "poison-sidecar");
}

fn baseline_intentions(store: Option<&Store>) -> Vec<Intention> {
    let Some(store) = store else {
        return Vec::new();
    };
    store
        .audit_query(AuditFilter::default())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| match entry.target {
            AuditTarget::Intention(id) => store.get_intention(id).ok().flatten(),
            _ => None,
        })
        .filter(|intention| intention.description == BASELINE_INTENTION_DESCRIPTION)
        .collect()
}

fn resolved_clip_paths(world: &FirstLightWorld, observations: &[Observation]) -> Vec<PathBuf> {
    observations
        .iter()
        .flat_map(|observation| observation.evidence.iter())
        .filter(|evidence| evidence.kind == EvidenceKind::VideoSegment)
        .filter_map(|evidence| resolve_clip_path(world, &evidence.source_ref))
        .collect()
}

fn resolved_detector_image_paths(
    world: &FirstLightWorld,
    observations: &[Observation],
) -> Vec<PathBuf> {
    observations
        .iter()
        .flat_map(|observation| observation.evidence.iter())
        .filter(|evidence| evidence.kind == EvidenceKind::ImageFrame)
        .filter_map(|evidence| resolve_clip_path(world, &evidence.source_ref))
        .collect()
}

fn resolve_clip_path(world: &FirstLightWorld, source_ref: &str) -> Option<PathBuf> {
    if let Some(rest) = source_ref.strip_prefix("file://") {
        return Some(PathBuf::from(rest));
    }
    if let Some(rest) = source_ref.strip_prefix("vigil-edge:clip/") {
        return Some(world.data_dir.join("clips").join(rest));
    }
    None
}

fn decodable_clip_count(world: &FirstLightWorld, observations: &[Observation]) -> usize {
    resolved_clip_paths(world, observations)
        .into_iter()
        .filter(|path| path.is_file() && count_video_frames(path).unwrap_or_default() > 1)
        .count()
}

fn decodable_detector_image_count(world: &FirstLightWorld, observations: &[Observation]) -> usize {
    resolved_detector_image_paths(world, observations)
        .into_iter()
        .filter(|path| {
            path.is_file()
                && image::open(path)
                    .map(|image| image.width() == 640 && image.height() == 640)
                    .unwrap_or(false)
        })
        .count()
}

fn video_frame_fingerprints(path: &Path) -> Result<BTreeSet<String>, String> {
    const WIDTH: usize = 16;
    const HEIGHT: usize = 16;
    let ffmpeg =
        tool_path("VIGIL_FFMPEG_BIN", "ffmpeg").ok_or_else(|| "ffmpeg missing".to_string())?;
    let output = Command::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-i")
        .arg(path)
        .arg("-vf")
        .arg(format!("scale={WIDTH}:{HEIGHT},format=gray"))
        .arg("-f")
        .arg("rawvideo")
        .arg("-")
        .output()
        .map_err(|error| format!("fingerprint frames in {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "fingerprint frames in {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let frame_size = WIDTH * HEIGHT;
    let fingerprints = output
        .stdout
        .chunks_exact(frame_size)
        .map(average_hash_frame)
        .collect::<BTreeSet<_>>();
    if fingerprints.is_empty() {
        return Err(format!("{} produced no frame fingerprints", path.display()));
    }
    Ok(fingerprints)
}

fn average_hash_frame(frame: &[u8]) -> String {
    let average = frame.iter().map(|value| *value as u64).sum::<u64>() / frame.len() as u64;
    let bits = frame
        .iter()
        .map(|value| if *value as u64 >= average { b'1' } else { b'0' })
        .collect::<Vec<_>>();
    let digest = Sha256::digest(&bits);
    format!("{digest:x}")
}

fn assert_clip_evidence_matches_stream(
    world: &FirstLightWorld,
    observations: &[Observation],
    source_clip: &Path,
    label: &str,
    failures: &mut Vec<String>,
) {
    let clip_paths = resolved_clip_paths(world, observations);
    if clip_paths.is_empty() {
        failures.push(format!(
            "{label} Observation had no resolvable clip evidence"
        ));
        return;
    }
    let source_frames = count_video_frames(source_clip).unwrap_or_default();
    let source_fingerprints = match video_frame_fingerprints(source_clip) {
        Ok(fingerprints) => fingerprints,
        Err(error) => {
            failures.push(format!(
                "{label} source clip was not fingerprintable: {error}"
            ));
            return;
        }
    };
    let fixture_digests = [world.person_clip.as_path(), world.empty_clip.as_path()]
        .into_iter()
        .filter_map(|path| sha256_file(path).ok())
        .collect::<BTreeSet<_>>();
    for clip_path in clip_paths {
        let clip_digest = sha256_file(&clip_path).ok();
        if clip_digest
            .as_ref()
            .is_some_and(|digest| fixture_digests.contains(digest))
        {
            failures.push(format!(
                "{label} clip evidence {} is a byte-for-byte committed fixture copy",
                clip_path.display()
            ));
        }
        let clip_frames = count_video_frames(&clip_path).unwrap_or_default();
        if source_frames > 1 && clip_frames >= source_frames {
            failures.push(format!(
                "{label} clip evidence {} contains the full source fixture instead of an event segment",
                clip_path.display()
            ));
        }
        match video_frame_fingerprints(&clip_path) {
            Ok(clip_fingerprints) => {
                if source_fingerprints
                    .intersection(&clip_fingerprints)
                    .next()
                    .is_none()
                {
                    failures.push(format!(
                        "{label} clip evidence {} did not share decoded frames with the RTSP stimulus",
                        clip_path.display()
                    ));
                }
            }
            Err(error) => failures.push(format!(
                "{label} clip evidence {} was not fingerprintable: {error}",
                clip_path.display()
            )),
        }
    }
}

fn assert_clip_evidence_overlaps_each_stream(
    world: &FirstLightWorld,
    observations: &[Observation],
    sources: &[(&Path, &str)],
    failures: &mut Vec<String>,
) {
    let clip_paths = resolved_clip_paths(world, observations);
    let mut clip_fingerprints = Vec::new();
    for clip_path in &clip_paths {
        match video_frame_fingerprints(clip_path) {
            Ok(fingerprints) => clip_fingerprints.push((clip_path, fingerprints)),
            Err(error) => failures.push(format!(
                "clip evidence {} was not fingerprintable for multi-stimulus check: {error}",
                clip_path.display()
            )),
        }
    }
    for (source, label) in sources {
        let source_fingerprints = match video_frame_fingerprints(source) {
            Ok(fingerprints) => fingerprints,
            Err(error) => {
                failures.push(format!("{label} stimulus was not fingerprintable: {error}"));
                continue;
            }
        };
        if !clip_fingerprints.iter().any(|(_, fingerprints)| {
            source_fingerprints
                .intersection(fingerprints)
                .next()
                .is_some()
        }) {
            failures.push(format!(
                "no landed clip evidence shared decoded frames with the {label} stimulus"
            ));
        }
    }
}

fn assert_distinct_clip_contents(clip_paths: &[PathBuf], label: &str, failures: &mut Vec<String>) {
    let mut seen_digests = BTreeSet::new();
    let mut seen_fingerprints = BTreeSet::new();
    for clip_path in clip_paths {
        match sha256_file(clip_path) {
            Ok(digest) => {
                if !seen_digests.insert(digest) {
                    failures.push(format!(
                        "{label} reused byte-identical clip evidence {}",
                        clip_path.display()
                    ));
                }
            }
            Err(error) => failures.push(format!(
                "{label} clip evidence {} was not readable: {error}",
                clip_path.display()
            )),
        }
        match video_frame_fingerprints(clip_path) {
            Ok(fingerprints) => {
                if !seen_fingerprints.insert(fingerprints) {
                    failures.push(format!(
                        "{label} reused decoded-frame-identical clip evidence {}",
                        clip_path.display()
                    ));
                }
            }
            Err(error) => failures.push(format!(
                "{label} clip evidence {} was not fingerprintable: {error}",
                clip_path.display()
            )),
        }
    }
}

fn local_clip_artifact_count(world: &FirstLightWorld) -> usize {
    local_clip_artifact_paths(world).len()
}

fn local_clip_artifact_paths(world: &FirstLightWorld) -> Vec<PathBuf> {
    regular_file_paths(&world.data_dir.join("clips"))
}

fn local_staging_artifact_paths(world: &FirstLightWorld) -> Vec<PathBuf> {
    regular_file_paths(&world.data_dir.join("staging"))
}

fn regular_file_paths(path: &Path) -> Vec<PathBuf> {
    let Ok(metadata) = fs::metadata(path) else {
        return Vec::new();
    };
    if metadata.is_file() {
        return vec![path.to_path_buf()];
    }
    if !metadata.is_dir() {
        return Vec::new();
    }
    fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .flat_map(|entry| regular_file_paths(&entry.path()))
                .collect()
        })
        .unwrap_or_default()
}

fn event_rows<'a>(text: &'a str, observations: &[Observation]) -> Vec<&'a str> {
    let ids = observations
        .iter()
        .map(|observation| observation.id.to_string())
        .collect::<Vec<_>>();
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| ids.iter().any(|id| line.contains(id)))
        .collect()
}

fn assert_event_rows_match_observed_at(
    rows: &[&str],
    observations: &[Observation],
    failures: &mut Vec<String>,
) {
    for observation in observations {
        let id = observation.id.to_string();
        let Some(row) = rows.iter().copied().find(|row| row.contains(&id)) else {
            failures.push(format!("event row omitted cg Observation id {id}"));
            continue;
        };
        let Some(value) =
            parsed_line_field(row, "observed_at").or_else(|| parsed_line_field(row, "event_time"))
        else {
            failures.push(format!(
                "event row for Observation {id} omitted observed_at/event_time"
            ));
            continue;
        };
        let parsed = match DateTime::parse_from_rfc3339(value) {
            Ok(parsed) => parsed.with_timezone(&Utc),
            Err(error) => {
                failures.push(format!(
                    "event row for Observation {id} carried an unparsable observed_at/event_time {value:?}: {error}"
                ));
                continue;
            }
        };
        if parsed != observation.observed_at {
            failures.push(format!(
                "event row for Observation {id} reported observed_at {parsed}, expected {}",
                observation.observed_at
            ));
        }
    }
}

fn assert_event_rows_match_observation_fields(
    rows: &[&str],
    observations: &[Observation],
    store: Option<&Store>,
    failures: &mut Vec<String>,
) {
    let Some(store) = store else {
        failures.push("cg store was not available for per-row event authority check".to_string());
        return;
    };
    for observation in observations {
        let id = observation.id.to_string();
        let Some(row) = rows.iter().copied().find(|row| row.contains(&id)) else {
            failures.push(format!("event row omitted cg Observation id {id}"));
            continue;
        };
        let camera = match store.get_entity(observation.entity_id) {
            Ok(Some(camera)) => camera,
            Ok(None) => {
                failures.push(format!(
                    "event row authority camera {} for Observation {id} was not found",
                    observation.entity_id
                ));
                continue;
            }
            Err(error) => {
                failures.push(format!(
                    "event row authority camera {} for Observation {id} could not be read: {error}",
                    observation.entity_id
                ));
                continue;
            }
        };
        if !row.contains(&camera.name) {
            failures.push(format!(
                "event row for Observation {id} omitted exact camera name {}",
                camera.name
            ));
        }
        let Some(class_name) = observation
            .observed_properties
            .get("class")
            .and_then(|value| value.as_str())
        else {
            failures.push(format!(
                "cg Observation {id} omitted detector class for per-row authority check"
            ));
            continue;
        };
        if parsed_line_field(row, "class") != Some(class_name) {
            failures.push(format!(
                "event row for Observation {id} did not carry exact detector class {class_name}"
            ));
        }
        let Some(confidence) = observation
            .observed_properties
            .get("confidence")
            .and_then(|value| value.as_f64())
        else {
            failures.push(format!(
                "cg Observation {id} omitted detector confidence for per-row authority check"
            ));
            continue;
        };
        match parsed_line_field(row, "confidence").and_then(|value| value.parse::<f64>().ok()) {
            Some(row_confidence) if (row_confidence - confidence).abs() <= f64::EPSILON => {}
            Some(row_confidence) => failures.push(format!(
                "event row for Observation {id} reported confidence {row_confidence}, expected {confidence}"
            )),
            None => failures.push(format!(
                "event row for Observation {id} omitted parseable confidence"
            )),
        }
        for evidence in &observation.evidence {
            if !row.contains(&evidence.source_ref) {
                failures.push(format!(
                    "event row for Observation {id} omitted exact clip ref {}",
                    evidence.source_ref
                ));
            }
        }
    }
}

fn observations_newest_observed_first(observations: &[Observation]) -> Vec<Observation> {
    let mut ordered = observations.to_vec();
    ordered.sort_by(|a, b| {
        b.observed_at
            .cmp(&a.observed_at)
            .then_with(|| b.id.as_uuid().cmp(&a.id.as_uuid()))
    });
    ordered
}

fn observations_newest_committed_first(observations: &[Observation]) -> Vec<Observation> {
    let mut ordered = observations.to_vec();
    ordered.sort_by(|a, b| {
        b.created_at
            .0
            .cmp(&a.created_at.0)
            .then_with(|| b.id.as_uuid().cmp(&a.id.as_uuid()))
    });
    ordered
}

fn observation_ids(observations: &[Observation]) -> Vec<String> {
    observations
        .iter()
        .map(|observation| observation.id.to_string())
        .collect()
}

fn assert_observed_time_conflicts_with_commit_order(
    observed_order: &[Observation],
    commit_order: &[Observation],
    failures: &mut Vec<String>,
) {
    let observed_ids = observation_ids(observed_order);
    let commit_ids = observation_ids(commit_order);
    if observed_ids.len() >= 2 && observed_ids == commit_ids {
        failures.push(
            "event-order fixture did not prove observed_at order differs from created_at commit order"
                .to_string(),
        );
    }
}

#[test]
fn camera_registered_by_rtsp_url_persists_as_site_entity() {
    let world = world_or_fail();
    let (_runtime, store) = run_runtime_and_open_store(&world);
    let cameras = device_entities(store.as_ref());
    let mut failures = Vec::new();

    if cameras.len() != 1 {
        failures.push(format!(
            "manual RTSP registration must persist one camera entity, found {}",
            cameras.len()
        ));
    }
    if !cameras.iter().any(|camera| camera.name == CAMERA_NAME) {
        failures.push(format!("camera entity did not retain name {CAMERA_NAME:?}"));
    }
    if first_camera_rtsp(&cameras) != Some(world.rtsp_url.as_str()) {
        failures.push(format!(
            "camera entity did not retain RTSP URL {:?}",
            world.rtsp_url
        ));
    }

    assert_contract(failures);
}

#[test]
fn camera_survives_store_reopen() {
    let world = world_or_fail();
    let _ = world.run_runtime_once();
    let first = world.open_store().ok();
    drop(first);
    let reopened = world.open_store().ok();
    let cameras = device_entities(reopened.as_ref());
    let mut failures = Vec::new();

    if cameras.len() != 1 {
        failures.push(format!(
            "camera must survive store reopen exactly once, found {}",
            cameras.len()
        ));
    }
    if !cameras.iter().any(|camera| camera.name == CAMERA_NAME) {
        failures.push("camera name did not survive store reopen".to_string());
    }
    if first_camera_rtsp(&cameras) != Some(world.rtsp_url.as_str()) {
        failures.push("camera RTSP URL did not survive store reopen".to_string());
    }

    assert_contract(failures);
}

#[test]
fn reregistration_is_idempotent_no_duplicate() {
    let world = world_or_fail();
    let first = world.run_runtime_once();
    let second = world.run_runtime_once();
    let store = world.open_store().ok();
    let cameras = device_entities(store.as_ref());
    let mut failures = Vec::new();

    if !first.spawned || !first.health_ready {
        failures
            .push("first startup did not accept the camera config through the runtime".to_string());
    }
    if !second.spawned || !second.health_ready {
        failures.push("second startup returned an error instead of create-or-get".to_string());
    }
    if cameras.len() != 1 {
        failures.push(format!(
            "re-registration should leave one camera entity, found {}",
            cameras.len()
        ));
    }
    if first_camera_rtsp(&cameras) != Some(world.rtsp_url.as_str()) {
        failures.push("idempotent startup did not preserve the camera RTSP URL".to_string());
    }

    assert_contract(failures);
}

#[test]
fn site_context_created_and_name_retained() {
    let world = world_or_fail();
    let _ = world.run_runtime_once();
    let store = world.open_store().ok();
    let names = list_site_context_names(store.as_ref());
    drop(store);
    let reopened = world.open_store().ok();
    let reopened_names = list_site_context_names(reopened.as_ref());
    let mut failures = Vec::new();

    if names
        .iter()
        .filter(|name| name.as_str() == SITE_NAME)
        .count()
        != 1
    {
        failures.push(format!(
            "site context {SITE_NAME:?} was not created exactly once"
        ));
    }
    if reopened_names
        .iter()
        .filter(|name| name.as_str() == SITE_NAME)
        .count()
        != 1
    {
        failures.push(format!(
            "site context {SITE_NAME:?} was not retained after store reopen"
        ));
    }

    assert_contract(failures);
}

#[test]
fn detector_config_recorded_as_decision() {
    let world = world_or_fail();
    let _ = world.run_runtime_once();
    let store = world.open_store().ok();
    let contexts = store
        .as_ref()
        .and_then(|store| store.list_contexts().ok())
        .unwrap_or_default();
    let cameras = device_entities(store.as_ref());
    let intentions = baseline_intentions(store.as_ref());
    let decisions = store
        .as_ref()
        .and_then(|store| detector_decisions(store).ok())
        .unwrap_or_default();
    let mut failures = Vec::new();

    if decisions.len() != 1 {
        failures.push(format!(
            "detector config must be one Decision audit target, found {decisions}",
            decisions = decisions.len()
        ));
    }
    if contexts.len() != 1 {
        failures.push(format!(
            "detector config must create one site Context, found {}",
            contexts.len()
        ));
    }
    if cameras.len() != 1 {
        failures.push(format!(
            "detector config must create one lower-gate camera Entity, found {}",
            cameras.len()
        ));
    }
    if intentions.len() != 1 {
        failures.push(format!(
            "detector config must create one baseline Intention, found {}",
            intentions.len()
        ));
    }
    if let Some(context) = contexts.first()
        && context.name != SITE_NAME
    {
        failures.push(format!(
            "site Context name was {}, expected {SITE_NAME}",
            context.name
        ));
    }
    if let Some(camera) = cameras.first()
        && camera.name != CAMERA_NAME
    {
        failures.push(format!(
            "camera Entity name was {}, expected {CAMERA_NAME}",
            camera.name
        ));
    }
    if let Some(intention) = intentions.first()
        && intention.description != BASELINE_INTENTION_DESCRIPTION
    {
        failures.push(format!(
            "baseline Intention description was {}, expected {BASELINE_INTENTION_DESCRIPTION}",
            intention.description
        ));
    }
    if let (Some(context), Some(camera), Some(intention), Some(decision)) = (
        contexts.first(),
        cameras.first(),
        intentions.first(),
        decisions.first(),
    ) {
        if decision.context_id != context.id {
            failures.push("detector Decision was not in the site Context".to_string());
        }
        if camera.context_id != context.id {
            failures.push("camera Entity was not in the site Context".to_string());
        }
        if intention.context_id != context.id {
            failures.push("baseline Intention was not in the site Context".to_string());
        }
        if !decision.intention_ids.contains(&intention.id) {
            failures.push("detector Decision did not serve the baseline Intention".to_string());
        }
        if decision.based_on_snapshots.is_empty() {
            failures.push(
                "detector Decision did not persist a basis snapshot for the lower-gate camera Entity"
                    .to_string(),
            );
        }
        if decision
            .properties
            .get("model_id")
            .and_then(|value| value.as_str())
            != Some(DETECTOR_MODEL_ID)
        {
            failures.push(format!(
                "detector Decision did not record model_id={DETECTOR_MODEL_ID}"
            ));
        }
        if !decision
            .properties
            .get("threshold")
            .and_then(|value| value.as_f64())
            .map(|value| (value - DETECTOR_THRESHOLD).abs() < f64::EPSILON)
            .unwrap_or(false)
        {
            failures.push(format!(
                "detector Decision did not record threshold={DETECTOR_THRESHOLD}"
            ));
        }
    } else {
        failures.push(
            "detector config did not create the complete Context/Entity/Intention/Decision graph"
                .to_string(),
        );
    }

    assert_contract(failures);
}

#[test]
fn detection_produces_observation_referencing_clip() {
    let world = rtsp_world_or_fail();
    let readiness = FixtureReadiness::probe(&world);
    let (runtime, store) = run_runtime_until_log_contains_and_open_store_with_env(
        &world,
        &[],
        "observation_written=true",
    );
    let observations = list_observations(store.as_ref());
    let mut failures = readiness.missing_messages();

    if !runtime.rtsp_opened {
        failures.push("runtime did not open the configured RTSP source".to_string());
    }
    if !runtime.rtsp_network_connected {
        failures.push("runtime did not connect to the configured RTSP endpoint".to_string());
    }
    if !runtime.rtsp_fixture_read_observed {
        failures.push("RTSP fixture did not observe the runtime reading the stream".to_string());
    }
    if runtime.decoded_frames <= 1 {
        failures
            .push("runtime did not decode enough real frames to prove clip durability".to_string());
    }
    if runtime.detector_invocations == 0 {
        failures.push("detector was not invoked on decoded RTSP frames".to_string());
    }
    if observations.len() != 1 {
        failures.push(format!(
            "expected one clip-backed Observation, found {}",
            observations.len()
        ));
    }
    if clip_path_count(&observations) != 1 {
        failures.push("Observation did not reference exactly one durable clip path".to_string());
    }
    if resolved_clip_paths(&world, &observations).len() != observations.len() {
        failures.push("Observation clip evidence did not resolve to local clip paths".to_string());
    }
    if decodable_clip_count(&world, &observations) != observations.len() {
        failures.push("Observation clip evidence was not decodable with >1 frame".to_string());
    }
    if resolved_detector_image_paths(&world, &observations).len() != observations.len() {
        failures.push(
            "Observation did not include exactly one detector image evidence path".to_string(),
        );
    }
    if decodable_detector_image_count(&world, &observations) != observations.len() {
        failures.push("Observation detector image evidence was not a decodable 640x640 PNG".into());
    }
    let leaked_staging = local_staging_artifact_paths(&world);
    if !leaked_staging.is_empty() {
        failures.push(format!(
            "Observation recording leaked staging clip artifacts: {leaked_staging:?}"
        ));
    }
    if observations.iter().any(|observation| {
        observation
            .observed_properties
            .get("bbox")
            .and_then(Value::as_str)
            .is_none_or(|bbox| bbox.split(',').count() != 4)
            || observation
                .observed_properties
                .get("frame_index")
                .and_then(Value::as_u64)
                .is_none_or(|frame_index| frame_index >= 48)
    }) {
        failures.push("Observation did not retain detector bbox and sampled frame index".into());
    }
    assert_clip_evidence_matches_stream(
        &world,
        &observations,
        &world.person_clip,
        "single detection",
        &mut failures,
    );

    assert_contract(failures);
}

#[test]
fn configured_camera_name_drives_provenance_and_clip_prefix() {
    let world = rtsp_world_or_fail();
    if let Err(error) = world.set_camera_name("front entry") {
        assert_contract(vec![error]);
    }
    let readiness = FixtureReadiness::probe(&world);
    let (runtime, store) = run_runtime_until_log_contains_and_open_store_with_env(
        &world,
        &[],
        "observation_written=true",
    );
    let observations = list_observations(store.as_ref());
    let cameras = device_entities(store.as_ref());
    let intentions = store
        .as_ref()
        .and_then(|store| store.audit_query(AuditFilter::default()).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| match entry.target {
            AuditTarget::Intention(id) => store
                .as_ref()
                .and_then(|store| store.get_intention(id).ok().flatten()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let decisions = store
        .as_ref()
        .and_then(|store| detector_decisions(store).ok())
        .unwrap_or_default();
    drop(store);
    let why = command_text(&world.run_cli(["why", "--latest"]));
    let mut failures = readiness.missing_messages();

    if !runtime.rtsp_network_connected || !runtime.rtsp_fixture_read_observed {
        failures.push("front-entry fixture was not consumed through RTSP".to_string());
    }
    if cameras
        .iter()
        .filter(|camera| camera.name == "front entry")
        .count()
        != 1
    {
        failures.push("configured camera name did not persist as the camera Entity".to_string());
    }
    if !intentions
        .iter()
        .any(|intention| intention.description == "watch front entry")
    {
        failures.push("baseline Intention did not derive from configured camera name".to_string());
    }
    if !decisions
        .iter()
        .any(|decision| decision.description == "Run local detector for front entry")
    {
        failures.push("detector Decision did not derive from configured camera name".to_string());
    }
    if observations.iter().any(|observation| {
        observation
            .evidence
            .iter()
            .filter(|evidence| evidence.kind == EvidenceKind::VideoSegment)
            .any(|evidence| {
                !evidence
                    .source_ref
                    .starts_with("vigil-edge:clip/front-entry-event-")
            })
    }) {
        failures.push("durable clip source_ref did not use the configured camera slug".to_string());
    }
    if !why.contains("camera_name=front entry")
        || !why.contains("intention_description=watch front entry")
        || !why.contains("clip_ref=vigil-edge:clip/front-entry-event-")
    {
        failures.push(format!(
            "why output did not expose front-entry provenance:\n{why}"
        ));
    }

    assert_contract(failures);
}

#[test]
fn credentialed_rtsp_url_authenticates_and_redacts_runtime_surface() {
    let world = authenticated_rtsp_url_world_or_fail();
    let readiness = FixtureReadiness::probe(&world);
    let mut runtime = LiveRuntime::spawn(&world);
    let observation = runtime
        .observe_until_decoded_frames(1, Duration::from_secs(90))
        .with_fixture_evidence(&world);
    let _ = runtime.terminate();
    let store = world.open_store().ok();
    let mut failures = readiness.missing_messages();

    assert_authenticated_rtsp_runtime(&world, &observation, store.as_ref(), &mut failures);
    if !world.rtsp_url.contains('@') {
        failures.push("authenticated URL fixture did not exercise URL-embedded credentials".into());
    }

    assert_contract(failures);
}

#[test]
fn credentialed_existing_camera_rtsp_url_is_redacted_on_startup() {
    let world = authenticated_rtsp_url_world_or_fail();
    let setup_store = match world.open_store() {
        Ok(store) => store,
        Err(error) => {
            panic!("open setup store: {error}");
        }
    };
    let context = match setup_store.create_context(CreateContext {
        name: SITE_NAME.to_string(),
        labels: vec!["site".to_string()],
        properties: BTreeMap::new(),
    }) {
        Ok(context) => context,
        Err(error) => {
            panic!("create setup context: {error}");
        }
    };
    let mut stale_properties = BTreeMap::new();
    stale_properties.insert(
        "rtsp_url".to_string(),
        Value::String(world.rtsp_url.clone()),
    );
    if let Err(error) = setup_store.create_entity(CreateEntity {
        entity_type: EntityType::Device,
        name: CAMERA_NAME.to_string(),
        properties: stale_properties,
        tags: vec!["camera".to_string()],
        context_id: context.id,
    }) {
        panic!("create setup camera: {error}");
    }
    drop(setup_store);

    let readiness = FixtureReadiness::probe(&world);
    let mut runtime = LiveRuntime::spawn(&world);
    let observation = runtime
        .observe_until_decoded_frames(1, Duration::from_secs(90))
        .with_fixture_evidence(&world);
    let _ = runtime.terminate();
    let store = world.open_store().ok();
    let mut failures = readiness.missing_messages();

    assert_authenticated_rtsp_runtime(&world, &observation, store.as_ref(), &mut failures);
    if !world.rtsp_url.contains('@') {
        failures.push("existing-camera fixture did not seed credentialed RTSP userinfo".into());
    }

    assert_contract(failures);
}

#[test]
fn separate_rtsp_credentials_authenticate_without_url_userinfo() {
    let world = authenticated_rtsp_fields_world_or_fail();
    let readiness = FixtureReadiness::probe(&world);
    let mut runtime = LiveRuntime::spawn(&world);
    let observation = runtime
        .observe_until_decoded_frames(1, Duration::from_secs(90))
        .with_fixture_evidence(&world);
    let _ = runtime.terminate();
    let store = world.open_store().ok();
    let mut failures = readiness.missing_messages();

    assert_authenticated_rtsp_runtime(&world, &observation, store.as_ref(), &mut failures);
    if world.rtsp_url.contains('@') || world.rtsp_url.contains(RTSP_AUTH_PASSWORD) {
        failures.push("separate-credential fixture kept credentials in the RTSP URL".into());
    }
    if world.rtsp_username.as_deref() != Some(RTSP_AUTH_USERNAME)
        || world.rtsp_password.as_deref() != Some(RTSP_AUTH_PASSWORD)
    {
        failures.push("separate-credential fixture did not pass RTSP auth fields".into());
    }

    assert_contract(failures);
}

#[test]
fn detector_loads_and_runs_over_real_frames() {
    let probe_world = world_or_fail();
    let readiness = FixtureReadiness::probe(&probe_world);
    let forward_probe_path = probe_world.data_dir.join("detector-forward-probe.log");
    let forward_probe_path_string = forward_probe_path.display().to_string();
    let forward_probe_nonce = unique_probe_nonce("detector-forward");
    let forward_probe_env = [
        (
            DETECTOR_FORWARD_PROBE_ENV,
            forward_probe_path_string.as_str(),
        ),
        (
            DETECTOR_FORWARD_PROBE_NONCE_ENV,
            forward_probe_nonce.as_str(),
        ),
    ];
    let model_digest = sha256_file(&probe_world.detector_artifact).unwrap_or_default();
    let model_loaded = model_digest == DETECTOR_ARTIFACT_SHA256;
    let person_clip_digest = sha256_file(&probe_world.person_clip).ok();
    let person_frames = count_video_frames(&probe_world.person_clip).unwrap_or_default();
    let background_frames = count_video_frames(&probe_world.empty_clip).unwrap_or_default();
    let transformed_clip = generate_transformed_person_clip(&probe_world);
    let transformed_clip_digest = transformed_clip
        .as_ref()
        .ok()
        .and_then(|clip| sha256_file(clip).ok());
    let transformed_oracle = transformed_clip
        .as_ref()
        .ok()
        .map(|clip| run_independent_detector_oracle(&probe_world.detector_artifact, clip));
    let transformed_probe = transformed_clip.as_ref().ok().map(|clip| {
        run_detector_probe_with_model_and_env(
            &probe_world.detector_artifact,
            clip,
            &forward_probe_env,
        )
    });
    let challenge_clip = generate_challenge_person_clip(&probe_world);
    let challenge_clip_digest = challenge_clip
        .as_ref()
        .ok()
        .and_then(|clip| sha256_file(clip).ok());
    let challenge_oracle = challenge_clip
        .as_ref()
        .ok()
        .map(|clip| run_independent_detector_oracle(&probe_world.detector_artifact, clip));
    let challenge_probe = challenge_clip.as_ref().ok().map(|clip| {
        run_detector_probe_with_model_and_env(
            &probe_world.detector_artifact,
            clip,
            &forward_probe_env,
        )
    });
    let corrupt_model = corrupt_detector_artifact(&probe_world);
    let corrupt_probe = corrupt_model.as_ref().ok().map(|model| {
        run_detector_probe_with_model_and_env(model, &probe_world.person_clip, &forward_probe_env)
    });
    let person_probe = run_detector_probe_with_model_and_env(
        &probe_world.detector_artifact,
        &probe_world.person_clip,
        &forward_probe_env,
    );
    let person_oracle =
        run_independent_detector_oracle(&probe_world.detector_artifact, &probe_world.person_clip);
    let background_oracle =
        run_independent_detector_oracle(&probe_world.detector_artifact, &probe_world.empty_clip);
    let background_probe = run_detector_probe(&probe_world, &probe_world.empty_clip);

    let person_world = rtsp_world_or_fail();
    let (person_runtime, person_store) = run_runtime_until_log_contains_and_open_store_with_env(
        &person_world,
        &forward_probe_env,
        "observation_written=true",
    );
    let person_observations = list_observations(person_store.as_ref());
    let person_detections = person_observations.len() as u64;
    let mut failures = readiness.missing_messages();
    let forward_events = read_probe_events(
        &forward_probe_path,
        &forward_probe_nonce,
        DETECTOR_FORWARD_EVENT_TYPE,
        "detector forward",
        &mut failures,
    );
    assert_detector_forward_event(
        &forward_events,
        "committed person",
        person_clip_digest.as_deref(),
        &model_digest,
        person_probe.model_forward_sha256.as_deref(),
        person_probe.result_sha256.as_deref(),
        &mut failures,
    );
    assert_detector_probe_is_derived_from_forward_event(
        &forward_events,
        "committed person",
        person_clip_digest.as_deref(),
        &forward_probe_nonce,
        Some(&person_probe),
        &mut failures,
    );
    assert_detector_probe_matches_oracle(
        "committed person",
        Some(&person_probe),
        Some(&person_oracle),
        &mut failures,
    );
    assert_detector_forward_event(
        &forward_events,
        "transformed person",
        transformed_clip_digest.as_deref(),
        &model_digest,
        transformed_probe
            .as_ref()
            .and_then(|probe| probe.model_forward_sha256.as_deref()),
        transformed_probe
            .as_ref()
            .and_then(|probe| probe.result_sha256.as_deref()),
        &mut failures,
    );
    assert_detector_probe_is_derived_from_forward_event(
        &forward_events,
        "transformed person",
        transformed_clip_digest.as_deref(),
        &forward_probe_nonce,
        transformed_probe.as_ref(),
        &mut failures,
    );
    assert_detector_probe_matches_oracle(
        "transformed person",
        transformed_probe.as_ref(),
        transformed_oracle.as_ref(),
        &mut failures,
    );
    assert_detector_forward_event(
        &forward_events,
        "challenge person",
        challenge_clip_digest.as_deref(),
        &model_digest,
        challenge_probe
            .as_ref()
            .and_then(|probe| probe.model_forward_sha256.as_deref()),
        challenge_probe
            .as_ref()
            .and_then(|probe| probe.result_sha256.as_deref()),
        &mut failures,
    );
    assert_detector_probe_is_derived_from_forward_event(
        &forward_events,
        "challenge person",
        challenge_clip_digest.as_deref(),
        &forward_probe_nonce,
        challenge_probe.as_ref(),
        &mut failures,
    );
    assert_detector_probe_matches_oracle(
        "challenge person",
        challenge_probe.as_ref(),
        challenge_oracle.as_ref(),
        &mut failures,
    );
    drop(person_store);
    drop(person_world);

    let background_world = empty_rtsp_world_or_fail();
    let (background_runtime, background_store) = run_runtime_and_open_store(&background_world);
    let background_detections = list_observations(background_store.as_ref()).len() as u64;

    assert_detector_oracle_is_repo_owned_and_independent(&mut failures);
    assert_detector_source_has_model_execution_path(&mut failures);
    if !model_loaded {
        failures
            .push("detector backend did not load the pinned YOLOX-Tiny COCO artifact".to_string());
    }
    if person_frames == 0 {
        failures.push("person fixture did not decode into real frames".to_string());
    }
    if background_frames == 0 {
        failures.push("empty-scene fixture did not decode into real frames".to_string());
    }
    if let Err(error) = transformed_clip.as_ref() {
        failures.push(format!(
            "could not generate transformed detector input: {error}"
        ));
    }
    if let Err(error) = challenge_clip.as_ref() {
        failures.push(format!(
            "could not generate challenge detector input: {error}"
        ));
    }
    if let Err(error) = corrupt_model.as_ref() {
        failures.push(format!(
            "could not generate corrupt detector artifact: {error}"
        ));
    }
    if !person_runtime.rtsp_opened || person_runtime.decoded_frames == 0 {
        failures
            .push("person RTSP fixture did not flow through the runtime frame path".to_string());
    }
    if !person_runtime.rtsp_network_connected || !person_runtime.rtsp_fixture_read_observed {
        failures.push("person RTSP fixture did not observe runtime stream consumption".to_string());
    }
    if !background_runtime.rtsp_opened || background_runtime.decoded_frames == 0 {
        failures.push(
            "empty-scene RTSP fixture did not flow through the runtime frame path".to_string(),
        );
    }
    if !background_runtime.rtsp_network_connected || !background_runtime.rtsp_fixture_read_observed
    {
        failures.push(
            "empty-scene RTSP fixture did not observe runtime stream consumption".to_string(),
        );
    }
    if person_runtime.detector_invocations == 0 {
        failures.push("detector was not invoked on the person RTSP frame path".to_string());
    }
    if background_runtime.detector_invocations != 0 {
        failures.push(
            "motion-free empty-scene RTSP path invoked the detector instead of stopping at the gate"
                .to_string(),
        );
    }
    if !person_probe.status_success {
        failures.push(format!(
            "detector probe over person frames did not run successfully: {}",
            person_probe.text
        ));
    }
    if person_probe.model_sha256.as_deref() != Some(model_digest.as_str()) {
        failures.push("detector probe did not report the configured model digest".to_string());
    }
    if person_probe.backend_id.as_deref() != Some(PERSON_MODEL_BACKEND) {
        failures.push("detector probe did not report the pinned Burn YOLOX backend id".to_string());
    }
    if person_probe.session_id.as_deref().is_none_or(str::is_empty) {
        failures.push("detector probe did not report a detector-session-id".to_string());
    }
    if person_probe
        .model_forward_sha256
        .as_deref()
        .is_none_or(str::is_empty)
    {
        failures.push("detector probe did not report a raw model-forward digest".to_string());
    }
    if person_probe.nms_sha256.as_deref().is_none_or(str::is_empty) {
        failures.push("detector probe did not report an NMS-input digest".to_string());
    }
    if person_probe.nms_sha256 == person_probe.result_sha256 {
        failures.push("detector probe NMS and result digests were identical".to_string());
    }
    if person_probe
        .result_sha256
        .as_deref()
        .is_none_or(str::is_empty)
    {
        failures.push("detector probe did not report a result digest".to_string());
    }
    if person_probe.detections.unwrap_or_default() == 0 {
        failures.push("detector probe produced no person detections".to_string());
    }
    if person_probe.class_name.as_deref() != Some("person") {
        failures.push("detector probe did not identify the person class".to_string());
    }
    if person_probe.bbox.as_deref() != Some(PERSON_GOLDEN_BBOX) {
        failures.push("detector probe bbox did not match the pinned golden output".to_string());
    }
    if !person_probe
        .confidence
        .map(|confidence| {
            (confidence - PERSON_GOLDEN_CONFIDENCE).abs() <= PERSON_GOLDEN_CONFIDENCE_TOLERANCE
        })
        .unwrap_or(false)
    {
        failures.push(
            "detector probe confidence did not match the pinned golden output tolerance"
                .to_string(),
        );
    }
    if !matches!(person_probe.confidence, Some(confidence) if confidence > 0.0 && confidence <= 1.0)
    {
        failures.push("detector probe did not report a numeric confidence in (0,1]".to_string());
    }
    if person_probe
        .bbox
        .as_deref()
        .is_none_or(|bbox| bbox == "0,0,0,0" || bbox.trim().is_empty())
    {
        failures.push("detector probe did not report a non-empty bbox".to_string());
    }
    if !background_probe.status_success {
        failures.push(format!(
            "detector probe over background frames did not run successfully: {}",
            background_probe.text
        ));
    }
    if background_probe.detections != Some(0) {
        failures.push("detector probe produced detections on the background clip".to_string());
    }
    assert_detector_probe_matches_oracle(
        "background",
        Some(&background_probe),
        Some(&background_oracle),
        &mut failures,
    );
    match transformed_probe.as_ref() {
        Some(probe) => {
            if !probe.status_success {
                failures.push(format!(
                    "detector probe over transformed temp person frames did not run successfully: {}",
                    probe.text
                ));
            }
            if probe.model_sha256.as_deref() != Some(model_digest.as_str()) {
                failures.push(
                    "transformed detector probe did not report the configured model digest"
                        .to_string(),
                );
            }
            if probe.backend_id.as_deref() != Some(PERSON_MODEL_BACKEND) {
                failures.push(
                    "transformed detector probe did not report the Burn YOLOX backend id"
                        .to_string(),
                );
            }
            if probe.detections.unwrap_or_default() == 0 {
                failures
                    .push("transformed detector probe produced no person detections".to_string());
            }
            if probe.class_name.as_deref() != Some("person") {
                failures.push(
                    "transformed detector probe did not identify the person class".to_string(),
                );
            }
            if !matches!(probe.confidence, Some(confidence) if confidence > 0.0 && confidence <= 1.0)
            {
                failures.push(
                    "transformed detector probe did not report confidence in (0,1]".to_string(),
                );
            }
            if probe
                .bbox
                .as_deref()
                .is_none_or(|bbox| bbox == "0,0,0,0" || bbox.trim().is_empty())
            {
                failures
                    .push("transformed detector probe did not report a non-empty bbox".to_string());
            }
            if probe.result_sha256.as_deref().is_none_or(str::is_empty) {
                failures
                    .push("transformed detector probe did not report a result digest".to_string());
            }
            if probe
                .model_forward_sha256
                .as_deref()
                .is_none_or(str::is_empty)
            {
                failures.push(
                    "transformed detector probe did not report a raw model-forward digest"
                        .to_string(),
                );
            }
            if probe.model_forward_sha256 == person_probe.model_forward_sha256 {
                failures.push(
                    "transformed detector probe raw model-forward digest matched the committed clip"
                        .to_string(),
                );
            }
            if probe.result_sha256 == person_probe.result_sha256 {
                failures.push(
                    "transformed detector probe result digest was identical to the committed clip digest"
                        .to_string(),
                );
            }
        }
        None => failures.push("transformed detector probe did not run".to_string()),
    }
    match challenge_probe.as_ref() {
        Some(probe) => {
            if !probe.status_success {
                failures.push(format!(
                    "detector probe over challenge temp person frames did not run successfully: {}",
                    probe.text
                ));
            }
            if probe.model_sha256.as_deref() != Some(model_digest.as_str()) {
                failures.push(
                    "challenge detector probe did not report the configured model digest"
                        .to_string(),
                );
            }
            if probe.backend_id.as_deref() != Some(PERSON_MODEL_BACKEND) {
                failures.push(
                    "challenge detector probe did not report the Burn YOLOX backend id".to_string(),
                );
            }
            if probe.detections.unwrap_or_default() == 0 {
                failures.push("challenge detector probe produced no person detections".to_string());
            }
            if probe.class_name.as_deref() != Some("person") {
                failures
                    .push("challenge detector probe did not identify the person class".to_string());
            }
            if !matches!(probe.confidence, Some(confidence) if confidence > 0.0 && confidence <= 1.0)
            {
                failures.push(
                    "challenge detector probe did not report confidence in (0,1]".to_string(),
                );
            }
            if probe
                .bbox
                .as_deref()
                .is_none_or(|bbox| bbox == "0,0,0,0" || bbox.trim().is_empty())
            {
                failures
                    .push("challenge detector probe did not report a non-empty bbox".to_string());
            }
            if probe.result_sha256.as_deref().is_none_or(str::is_empty) {
                failures
                    .push("challenge detector probe did not report a result digest".to_string());
            }
            if probe
                .model_forward_sha256
                .as_deref()
                .is_none_or(str::is_empty)
            {
                failures.push(
                    "challenge detector probe did not report a raw model-forward digest"
                        .to_string(),
                );
            }
            if probe.model_forward_sha256 == person_probe.model_forward_sha256 {
                failures.push(
                    "challenge detector probe raw model-forward digest matched the committed clip"
                        .to_string(),
                );
            }
            if probe.result_sha256 == person_probe.result_sha256 {
                failures.push(
                    "challenge detector probe result digest matched the committed clip".to_string(),
                );
            }
            if let Some(transformed_probe) = transformed_probe.as_ref() {
                if probe.model_forward_sha256 == transformed_probe.model_forward_sha256 {
                    failures.push(
                        "challenge detector probe raw model-forward digest matched the transformed clip"
                            .to_string(),
                    );
                }
                if probe.result_sha256 == transformed_probe.result_sha256 {
                    failures.push(
                        "challenge detector probe result digest matched the transformed clip"
                            .to_string(),
                    );
                }
            }
        }
        None => failures.push("challenge detector probe did not run".to_string()),
    }
    match corrupt_probe.as_ref() {
        Some(probe) => {
            if probe.status_success {
                failures.push(
                    "detector probe succeeded with a deliberately corrupted model artifact"
                        .to_string(),
                );
            }
            let lower = probe.text.to_ascii_lowercase();
            if !(lower.contains("model") || lower.contains("checkpoint") || lower.contains("load"))
            {
                failures.push(
                    "corrupt-model detector probe did not fail as a model-load error".to_string(),
                );
            }
            if probe.detections.unwrap_or_default() > 0 {
                failures.push("corrupt-model detector probe produced detections".to_string());
            }
        }
        None => failures.push("corrupt-model detector probe did not run".to_string()),
    }
    if !person_runtime.logs.contains("detector model loaded")
        || !person_runtime
            .logs
            .contains(&format!("sha256={model_digest}"))
    {
        failures.push(
            "runtime did not emit a model-load receipt tied to the configured artifact digest"
                .to_string(),
        );
    }
    if !person_observations.iter().any(|observation| {
        observation
            .properties
            .get("detector_model_sha256")
            .and_then(|value| value.as_str())
            == Some(model_digest.as_str())
    }) {
        failures.push(
            "landed detector result did not carry the loaded model artifact digest".to_string(),
        );
    }
    if !person_observations.iter().any(|observation| {
        observation_property(observation, "detector_backend") == Some(PERSON_MODEL_BACKEND)
            && observation_has_linked_detector_forward_event(observation, &forward_events)
    }) {
        failures.push(
            "landed detector result did not carry linked runtime model-execution proof metadata"
                .to_string(),
        );
    }
    if !person_observations.iter().any(|observation| {
        observation_property(observation, "detector_session_id")
            .is_some_and(|session| !session.is_empty())
    }) {
        failures.push("landed detector result did not carry detector-session metadata".to_string());
    }
    if !person_observations.iter().any(|observation| {
        observation
            .observed_properties
            .get("class")
            .and_then(|value| value.as_str())
            == Some("person")
            && observation
                .observed_properties
                .get("bbox")
                .and_then(|value| value.as_str())
                .is_some_and(|bbox| !bbox.trim().is_empty() && bbox != "0,0,0,0")
            && observation
                .observed_properties
                .get("confidence")
                .and_then(|value| value.as_f64())
                .is_some_and(|confidence| confidence > 0.0 && confidence <= 1.0)
    }) {
        failures.push(
            "landed detector output did not carry a valid person class/confidence/bbox".to_string(),
        );
    }
    if person_detections == 0 {
        failures.push("real person frame produced no detector hit".to_string());
    }
    if background_detections != 0 {
        failures.push(format!(
            "real empty-scene frame produced {background_detections} detector hits"
        ));
    }

    assert_contract(failures);
}

#[test]
fn empty_or_undecodable_stream_lands_no_event() {
    let mut failures = Vec::new();
    assert_no_event_for_stream_case(
        "empty-scene",
        empty_rtsp_world_or_fail(),
        true,
        &mut failures,
    );
    assert_no_event_for_stream_case(
        "zero-media",
        zero_media_rtsp_world_or_fail(),
        false,
        &mut failures,
    );
    assert_no_event_for_stream_case(
        "undecodable-bytes",
        undecodable_rtsp_world_or_fail(),
        false,
        &mut failures,
    );

    assert_contract(failures);
}

fn assert_no_event_for_stream_case(
    label: &str,
    world: FirstLightWorld,
    expect_decoded_frames: bool,
    failures: &mut Vec<String>,
) {
    let runtime = world.run_runtime_once();
    let events = world.run_cli(["events"]);
    let stats = world.run_cli(["stats"]);
    let store = world.open_store().ok();
    let observations = list_observations(store.as_ref());

    if !observations.is_empty() {
        failures.push(format!(
            "{label} stream must land zero Observations, found {}",
            observations.len()
        ));
    }
    if !runtime.rtsp_network_connected {
        failures.push(format!(
            "{label} RTSP fixture was not opened by the runtime"
        ));
    }
    if expect_decoded_frames {
        if !runtime.rtsp_fixture_read_observed || runtime.decoded_frames == 0 {
            failures.push(format!(
                "{label} RTSP fixture was not actually consumed as a real frame stream"
            ));
        }
        if runtime.detector_invocations != 0 {
            failures.push(format!(
                "{label} motion-free stream invoked detector {} times instead of being suppressed by the gate",
                runtime.detector_invocations
            ));
        }
        if !runtime.logs.contains("motion_gate_suppressed_segment=true") {
            failures.push(format!(
                "{label} stream did not emit the motion-gate suppression receipt"
            ));
        }
    } else {
        if runtime.decoded_frames != 0 {
            failures.push(format!(
                "{label} stream decoded {} frames; zero-media/undecodable input must decode none",
                runtime.decoded_frames
            ));
        }
        if runtime.detector_invocations != 0 {
            failures.push(format!(
                "{label} stream invoked detector {} times despite no decodable media",
                runtime.detector_invocations
            ));
        }
        let clip_artifacts = local_clip_artifact_count(&world);
        if clip_artifacts != 0 {
            failures.push(format!(
                "{label} stream wrote {clip_artifacts} local clip artifacts despite no decodable media"
            ));
        }
        let stats_text = command_text(&stats).to_ascii_lowercase();
        if !(stats_text.contains("decode")
            || stats_text.contains("ingest")
            || stats_text.contains("stream-fail")
            || stats_text.contains("frames-received=0"))
        {
            failures.push(format!(
                "{label} stream did not surface an ingest/decode failure in stats"
            ));
        }
    }
    if !command_text(&events).trim().is_empty() {
        failures.push(format!(
            "events command was not a clean empty list for {label} stream"
        ));
    }
    if expect_decoded_frames
        && parsed_stat(&command_text(&stats), "frames-received").unwrap_or_default() <= 0.0
    {
        failures.push(format!(
            "{label} stream consumption was not visible through stats"
        ));
    }
}

#[test]
fn confidence_is_detector_output_not_threshold_derived() {
    let mut world = rtsp_world_or_fail();
    let _ = world.run_runtime_until_log_contains("observation_written=true");
    let transformed_clip = generate_transformed_person_clip(&world);
    if let Ok(clip) = transformed_clip.as_ref()
        && let Err(error) = world.switch_rtsp_clip(clip)
    {
        assert_contract(vec![format!(
            "could not switch RTSP publisher to transformed detector clip: {error}"
        )]);
    }
    let _ = world.run_runtime_until_log_contains("observation_written=true");
    let store = world.open_store().ok();
    let observations = list_observations(store.as_ref());
    let confidences = observation_confidences(&observations);
    let detector_digests = observation_detector_result_digests(&observations);
    let mut failures = Vec::new();

    if let Err(error) = transformed_clip.as_ref() {
        failures.push(format!(
            "could not generate second real confidence stimulus: {error}"
        ));
    }
    if observations.len() != 2 {
        failures.push(format!(
            "two explicit real detections are required to compare confidence, found {} Observations",
            observations.len()
        ));
    }
    if confidences.len() < 2 {
        failures.push(format!(
            "two real detections are required to compare confidence, found {}",
            confidences.len()
        ));
    }
    if confidences
        .iter()
        .any(|confidence| (*confidence - (DETECTOR_THRESHOLD + 0.35)).abs() < f64::EPSILON)
    {
        failures
            .push("at least one confidence was derived from the configured threshold".to_string());
    }
    if confidences.len() >= 2 && (confidences[0] - confidences[1]).abs() < f64::EPSILON {
        failures.push("two distinct detections carried identical confidence".to_string());
    }
    let distinct_ids = observations
        .iter()
        .map(|observation| observation.id)
        .collect::<BTreeSet<_>>()
        .len();
    if distinct_ids != observations.len() {
        failures.push("confidence comparison reused the same cg Observation id".to_string());
    }
    if clip_path_count(&observations) != observations.len() {
        failures.push(
            "confidence comparison did not land a distinct durable clip per detection".to_string(),
        );
    }
    if detector_digests.len() < 2 {
        failures.push("confidence comparison omitted detector result digests".to_string());
    }
    if detector_digests.iter().collect::<BTreeSet<_>>().len() < detector_digests.len() {
        failures
            .push("two explicit detections carried the same detector result digest".to_string());
    }
    if let Ok(clip) = transformed_clip.as_ref() {
        assert_clip_evidence_overlaps_each_stream(
            &world,
            &observations,
            &[
                (&world.person_clip, "committed person"),
                (clip.as_path(), "transformed person"),
            ],
            &mut failures,
        );
    }
    assert_distinct_clip_contents(
        &resolved_clip_paths(&world, &observations),
        "confidence comparison",
        &mut failures,
    );

    assert_contract(failures);
}

#[test]
fn detector_confidence_threshold_filters_detector_output() {
    let world = world_or_fail();
    let default_probe = run_detector_probe(&world, &world.person_clip);
    let strict_probe =
        run_detector_probe_with_model_threshold(&world.detector_artifact, &world.person_clip, 0.99);
    let mut failures = Vec::new();

    if !default_probe.status_success {
        failures.push(format!(
            "default-threshold detector probe failed: {}",
            default_probe.text
        ));
    }
    if default_probe.detections.unwrap_or_default() == 0 {
        failures.push("default-threshold detector probe produced no person detections".to_string());
    }
    if !strict_probe.status_success {
        failures.push(format!(
            "strict-threshold detector probe failed: {}",
            strict_probe.text
        ));
    }
    if strict_probe.detections.unwrap_or_default() != 0 {
        failures.push(format!(
            "strict detector threshold should suppress the fixture person, got {} detections",
            strict_probe.detections.unwrap_or_default()
        ));
    }
    if !matches!(default_probe.confidence, Some(confidence) if confidence < 0.99) {
        failures.push(
            "default detector fixture did not prove confidence is below the strict threshold"
                .to_string(),
        );
    }
    if default_probe.frame_index.is_none() {
        failures.push("detector probe did not report the sampled frame index".to_string());
    }

    assert_contract(failures);
}

#[test]
fn motion_gate_suppresses_non_motion_frames() {
    let person_world = rtsp_world_or_fail();
    let readiness = FixtureReadiness::probe(&person_world);
    let person_runtime = person_world.run_runtime_until_log_contains("observation_written=true");
    let person_store = person_world.open_store().ok();
    let person_observations = list_observations(person_store.as_ref());
    let mut failures = readiness.missing_messages();

    if !person_runtime.rtsp_network_connected || !person_runtime.rtsp_fixture_read_observed {
        failures.push("person segment was not consumed over the RTSP fixture".to_string());
    }
    if person_observations.is_empty() {
        failures.push(format!(
            "real motion segment did not produce a confirmed Observation; runtime logs:\n{}",
            person_runtime.logs
        ));
    }
    if person_runtime.detector_invocations == 0 {
        failures.push(format!(
            "motion-positive person segment did not invoke the detector; runtime logs:\n{}",
            person_runtime.logs
        ));
    }
    drop(person_store);
    drop(person_world);

    let mut empty_world = empty_rtsp_world_or_fail();
    let empty_runtime = empty_world.run_runtime_once();
    let motion_positive_person_free_clip = generate_motion_positive_person_free_clip(&empty_world);
    if let Ok(clip) = motion_positive_person_free_clip.as_ref()
        && let Err(error) = empty_world.switch_rtsp_clip(clip)
    {
        failures.push(format!(
            "could not switch RTSP publisher to motion-positive person-free clip: {error}"
        ));
    }
    let motion_positive_runtime =
        empty_world.run_runtime_until_log_contains("detector_detections=0");
    let empty_store = empty_world.open_store().ok();
    let empty_observations = list_observations(empty_store.as_ref());
    let motion_positive_frames = motion_positive_person_free_clip
        .as_ref()
        .ok()
        .and_then(|clip| motion_positive_frame_count(clip).ok())
        .unwrap_or_default();
    if let Err(error) = motion_positive_person_free_clip.as_ref() {
        failures.push(format!(
            "could not generate motion-positive person-free stimulus: {error}"
        ));
    }
    if !empty_runtime.rtsp_network_connected || !empty_runtime.rtsp_fixture_read_observed {
        failures.push("empty segment was not consumed over the RTSP fixture".to_string());
    }
    if empty_runtime.detector_invocations != 0 {
        failures.push(format!(
            "motion-free empty segment invoked detector {} times",
            empty_runtime.detector_invocations
        ));
    }
    if !empty_runtime
        .logs
        .contains("motion_gate_suppressed_segment=true")
    {
        failures.push(
            "motion-free empty segment did not emit the motion-gate suppression receipt"
                .to_string(),
        );
    }
    if motion_positive_frames == 0 {
        failures.push("person-free detector-negative stimulus was not motion-positive".to_string());
    }
    if !motion_positive_runtime.rtsp_network_connected
        || !motion_positive_runtime.rtsp_fixture_read_observed
    {
        failures.push(
            "motion-positive person-free segment was not consumed over the RTSP fixture"
                .to_string(),
        );
    }
    if motion_positive_runtime.detector_invocations == 0 {
        failures.push(format!(
            "motion-positive person-free segment did not reach the detector before rejection; runtime log tail:\n{}\nRTSP fixture log tail:\n{}",
            last_log_lines(&motion_positive_runtime.logs, 40),
            last_log_lines(&empty_world.rtsp_logs(), 40),
        ));
    }
    if !empty_observations.is_empty() {
        failures.push(format!(
            "empty or motion-positive person-free detector-negative segment produced {} Observations",
            empty_observations.len()
        ));
    }

    assert_contract(failures);
}

#[test]
fn observation_never_references_undurable_clip() {
    let world = rtsp_world_or_fail();
    let _ = world.run_runtime_until_log_contains("observation_written=true");
    let store = world.open_store().ok();
    let observations = list_observations(store.as_ref());
    let mut failures = Vec::new();

    if observations.is_empty() {
        failures
            .push("complete detection did not land a durable clip-backed Observation".to_string());
    }
    if resolved_clip_paths(&world, &observations).len() != observations.len() {
        failures.push("not every landed Observation referenced a local clip path".to_string());
    }
    if decodable_clip_count(&world, &observations) != observations.len() {
        failures.push("landed Observation referenced a missing or undecodable clip".to_string());
    }
    assert_clip_evidence_matches_stream(
        &world,
        &observations,
        &world.person_clip,
        "complete detection",
        &mut failures,
    );
    drop(store);
    drop(world);

    let failed_world = rtsp_world_or_fail();
    if let Err(error) = failed_world.make_clip_dir_read_only() {
        failures.push(error);
    }
    let _ = failed_world.run_runtime_until_log_contains("record_detection_failed");
    let failed_store = failed_world.open_store().ok();
    let failed_observations = list_observations(failed_store.as_ref());
    let failed_stats = command_text(&failed_world.run_cli(["stats"]));
    if parsed_stat(&failed_stats, "clip-write-failures").unwrap_or_default() <= 0.0 {
        failures.push("ENOSPC clip write was not counted in stats".to_string());
    }
    if failed_observations
        .iter()
        .flat_map(|observation| observation.evidence.iter())
        .any(|evidence| !evidence.source_ref.is_empty())
    {
        failures.push("an Observation referenced evidence from an ENOSPC clip write".to_string());
    }
    if !failed_observations.is_empty() {
        failures.push(format!(
            "failed clip write created {} Observations; faulted events must land no evidence",
            failed_observations.len()
        ));
    }
    drop(failed_store);

    let failed_artifacts = local_clip_artifact_paths(&failed_world);
    if let Err(error) = failed_world.make_clip_dir_writable() {
        failures.push(error);
    }
    let _ = failed_world.run_runtime_until_log_contains("observation_written=true");
    let recovered_store = failed_world.open_store().ok();
    let recovered_observations = list_observations(recovered_store.as_ref());
    let recovered_clip_paths = resolved_clip_paths(&failed_world, &recovered_observations);
    if recovered_observations.is_empty() {
        failures.push("recovered clip write did not land a fresh Observation".to_string());
    }
    if decodable_clip_count(&failed_world, &recovered_observations) != recovered_observations.len()
    {
        failures.push("recovered Observation referenced a missing or undecodable clip".to_string());
    }
    assert_clip_evidence_matches_stream(
        &failed_world,
        &recovered_observations,
        &failed_world.person_clip,
        "recovered detection",
        &mut failures,
    );
    for failed_artifact in &failed_artifacts {
        let failed_artifact_text = failed_artifact.to_string_lossy();
        if recovered_observations
            .iter()
            .flat_map(|observation| observation.evidence.iter())
            .any(|evidence| {
                evidence.source_ref == failed_artifact_text
                    || evidence.source_ref.contains(failed_artifact_text.as_ref())
            })
        {
            failures.push(format!(
                "a later Observation referenced an earlier failed clip artifact {}",
                failed_artifact.display()
            ));
        }
        if recovered_clip_paths
            .iter()
            .any(|path| path == failed_artifact)
        {
            failures.push(format!(
                "a later Observation resolved to the earlier failed clip artifact {}",
                failed_artifact.display()
            ));
        }
    }

    assert_contract(failures);
}

#[test]
fn two_rapid_events_get_distinct_observations_and_clips() {
    let world = rtsp_world_or_fail();
    let _ = world.run_runtime_until_log_contains("observation_written=true");
    let _ = world.run_runtime_until_log_contains("observation_written=true");
    let store = world.open_store().ok();
    let observations = list_observations(store.as_ref());
    let mut failures = Vec::new();

    if observations.len() != 2 {
        failures.push(format!(
            "two rapid detections must produce two Observations, found {}",
            observations.len()
        ));
    }
    if clip_path_count(&observations) != 2 {
        failures.push("two rapid detections did not produce two distinct clip paths".to_string());
    }
    let clip_paths = resolved_clip_paths(&world, &observations);
    assert_clip_evidence_matches_stream(
        &world,
        &observations,
        &world.person_clip,
        "rapid detections",
        &mut failures,
    );
    assert_distinct_clip_contents(&clip_paths, "rapid detections", &mut failures);

    assert_contract(failures);
}

#[test]
fn vigil_why_walks_observation_to_clip_to_decision_to_context() {
    let world = rtsp_world_or_fail();
    let _ = world.run_runtime_until_log_contains("observation_written=true");
    let store = world.open_store().ok();
    let authority = cg_authority(store.as_ref());
    drop(store);
    poison_non_cg_sidecars(&world);
    let why = world.run_cli(["why", "--latest"]);
    let text = command_text(&why);
    let mut failures = Vec::new();

    if !why.status_success {
        failures.push(format!("why command exited {:?}: {text}", why.status_code));
    }
    match authority {
        Ok(authority) => assert_why_matches_authority(&text, &authority, &mut failures),
        Err(error) => failures.push(error),
    }

    assert_contract(failures);
}

#[test]
fn vigil_why_served_over_socket_while_store_is_locked() {
    let world = rtsp_world_or_fail();
    let probe_path = world.data_dir.join("cg-live-read-probe.log");
    let probe_path_string = probe_path.display().to_string();
    let probe_nonce = unique_probe_nonce("cg-read");
    let mut runtime = LiveRuntime::spawn_with_env(
        &world,
        &[
            (CG_READ_PROBE_ENV, probe_path_string.as_str()),
            (CG_READ_PROBE_NONCE_ENV, probe_nonce.as_str()),
        ],
    );
    let runtime_observation = runtime
        .observe_until_log_contains("observation_written=true", Duration::from_secs(180))
        .with_fixture_evidence(&world);
    let live_events_ready = wait_for_live_events_row(&world, Duration::from_secs(180));
    poison_non_cg_sidecars(&world);
    let request_started_at = SystemTime::now();
    let why = world.run_cli(["why", "--latest"]);
    let text = command_text(&why);
    let mut failures = Vec::new();

    if runtime_observation.decoded_frames == 0 || runtime_observation.detector_invocations == 0 {
        failures.push(
            "live runtime did not consume frames and invoke the detector before live why"
                .to_string(),
        );
    }
    if !runtime.store_locked_while_live {
        failures.push("runtime did not hold the cg store lock while the CLI ran".to_string());
    }
    if !why.status_success {
        failures.push("why command did not answer while runtime was live".to_string());
    }
    if !live_events_ready.status_success
        || !command_text(&live_events_ready).contains("observation_id=")
    {
        failures
            .push("live events command did not observe a landed event before live why".to_string());
    }
    if !text.contains("served-by=af_unix") {
        failures
            .push("live why command did not cross the runtime-owned AF_UNIX socket".to_string());
    }
    let events_request_started_at = SystemTime::now();
    let events = world.run_cli(["events"]);
    let events_text = command_text(&events);
    if !events.status_success {
        failures.push("events command did not answer while runtime was live".to_string());
    }
    if !events_text.contains("served-by=af_unix") {
        failures
            .push("live events command did not cross the runtime-owned AF_UNIX socket".to_string());
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("poison-sidecar")
        || lower.contains("sidecar")
        || lower.contains("snapshot")
        || lower.contains("mirror")
    {
        failures.push("live why command read a sidecar, snapshot, or mirror path".to_string());
    }
    let events_lower = events_text.to_ascii_lowercase();
    if events_lower.contains("poison-sidecar")
        || events_lower.contains("sidecar")
        || events_lower.contains("snapshot")
        || events_lower.contains("mirror")
    {
        failures.push("live events command read a sidecar, snapshot, or mirror path".to_string());
    }
    if runtime.kill_without_flush().is_none() {
        failures.push("runtime child was not killed before cg corroboration".to_string());
    }
    let store = world.open_store().ok();
    let authority = cg_authority(store.as_ref());
    match authority {
        Ok(authority) => {
            assert_live_request_has_cg_read_probe(
                LiveReadProbeExpectation {
                    command_label: "why",
                    text: &text,
                    authority: &authority,
                    proof_path: &probe_path,
                    nonce: &probe_nonce,
                    request_started_at,
                    expected_methods: &[
                        "audit_query",
                        "get_observation",
                        "get_decision",
                        "get_intention",
                        "get_entity",
                        "get_context",
                    ],
                },
                &mut failures,
            );
            assert_live_request_has_cg_read_probe(
                LiveReadProbeExpectation {
                    command_label: "events",
                    text: &events_text,
                    authority: &authority,
                    proof_path: &probe_path,
                    nonce: &probe_nonce,
                    request_started_at: events_request_started_at,
                    expected_methods: &["list_observations", "get_observation", "get_entity"],
                },
                &mut failures,
            );
            assert_why_matches_authority(&text, &authority, &mut failures);
            assert_events_match_authority(&events_text, &authority, &mut failures);
        }
        Err(error) => failures.push(error),
    }

    assert_contract(failures);
}

#[test]
fn cli_read_when_runtime_busy_errors_cleanly() {
    let world = world_or_fail();
    let mut runtime = LiveRuntime::spawn(&world);
    let unavailable_socket = world.data_dir.join("missing-control.sock");
    let response = world.run_cli_with_control_socket(
        ["why", "00000000-0000-0000-0000-000000000000"],
        &unavailable_socket,
    );
    let text = command_text(&response);
    let mut failures = Vec::new();

    if !runtime.store_locked_while_live {
        failures.push("runtime did not own the store during busy-path probe".to_string());
    }
    if response.status_success {
        failures.push(
            "busy direct-open fallback returned success instead of retryable busy".to_string(),
        );
    }
    if !text.to_ascii_lowercase().contains("busy") && !text.to_ascii_lowercase().contains("retry") {
        failures.push("busy path did not return an observable retry message".to_string());
    }
    if !text.contains("error_kind=database_locked") {
        failures
            .push("busy path did not return a structured database-locked error kind".to_string());
    }
    let _ = runtime.terminate();

    assert_contract(failures);
}

#[test]
fn vigil_why_on_unknown_event_id_errors_cleanly() {
    let world = rtsp_world_or_fail();
    let _ = world.run_runtime_until_log_contains("observation_written=true");
    let store = world.open_store().ok();
    let authority = cg_authority(store.as_ref());
    drop(store);
    poison_non_cg_sidecars(&world);
    let known = world.run_cli(["why", "--latest"]);
    let missing = world.run_cli(["why", "00000000-0000-0000-0000-000000000000"]);
    let known_text = command_text(&known);
    let missing_text = command_text(&missing);
    let mut failures = Vec::new();

    if !known.status_success {
        failures.push("known event did not walk successfully before missing-id check".to_string());
    }
    match authority {
        Ok(authority) => assert_why_matches_authority(&known_text, &authority, &mut failures),
        Err(error) => failures.push(error),
    }
    if missing.status_success {
        failures.push("unknown event id returned success instead of clean not-found".to_string());
    }
    if !missing_text.to_ascii_lowercase().contains("not found") {
        failures.push("unknown event id did not produce an observable not-found".to_string());
    }

    assert_contract(failures);
}

#[test]
fn vigil_why_reports_config_as_of_event_time_not_current() {
    let mut world = rtsp_world_or_fail();
    let _ = world.run_runtime_until_observation_count(1);
    if let Err(error) = world.set_detector_threshold(0.45) {
        assert_contract(vec![error]);
    }
    let transformed_clip = match generate_transformed_person_clip(&world) {
        Ok(clip) => clip,
        Err(error) => {
            assert_contract(vec![format!(
                "could not generate second config-change stimulus: {error}"
            )]);
            return;
        }
    };
    if let Err(error) = world.switch_rtsp_clip(&transformed_clip) {
        assert_contract(vec![format!(
            "could not switch RTSP publisher to second config-change stimulus: {error}"
        )]);
    }
    let _ = world.run_runtime_until_observation_count(2);
    let store = world.open_store().ok();
    let mut observations = list_observations(store.as_ref());
    observations.sort_by(|a, b| a.observed_at.cmp(&b.observed_at));
    let decisions = store
        .as_ref()
        .and_then(|store| detector_decisions(store).ok())
        .unwrap_or_default();
    let old_decision = decisions.iter().find(|decision| {
        decision
            .properties
            .get("threshold")
            .and_then(|value| value.as_f64())
            == Some(0.5)
    });
    let new_decision = decisions.iter().find(|decision| {
        decision
            .properties
            .get("threshold")
            .and_then(|value| value.as_f64())
            == Some(0.45)
    });
    let old_observation = observations.first().cloned();
    let new_observation = observations.last().cloned();
    drop(store);
    poison_non_cg_sidecars(&world);
    let old_id = old_observation
        .as_ref()
        .map(|observation| observation.id.to_string())
        .unwrap_or_else(|| "missing-old-observation".to_string());
    let new_id = new_observation
        .as_ref()
        .map(|observation| observation.id.to_string())
        .unwrap_or_else(|| "missing-new-observation".to_string());
    let old = command_text(&world.run_cli(["why", old_id.as_str()]));
    let new = command_text(&world.run_cli(["why", new_id.as_str()]));
    let mut failures = Vec::new();

    if observations.len() < 2 {
        failures.push(format!(
            "config-change fixture did not land two real Observations, found {}",
            observations.len()
        ));
    }
    if old_decision.is_none() {
        failures.push("old detector Decision with threshold=0.5 was not recorded".to_string());
    }
    if new_decision.is_none() {
        failures.push("new detector Decision with threshold=0.45 was not recorded".to_string());
    }
    if let Some(decision) = old_decision
        && !old.contains(&decision.id.to_string())
    {
        failures.push("old event why output omitted its exact detector Decision id".to_string());
    }
    if let Some(decision) = new_decision
        && !new.contains(&decision.id.to_string())
    {
        failures.push("new event why output omitted its exact detector Decision id".to_string());
    }
    if !old.contains("threshold=0.5") {
        failures.push(
            "old event did not report the detector threshold active at event time".to_string(),
        );
    }
    if old.contains("threshold=0.45") {
        failures.push("old event included the current detector threshold".to_string());
    }
    if !new.contains("threshold=0.45") {
        failures.push("new event did not report the newer detector threshold".to_string());
    }
    assert_why_excludes_non_requested_event(
        &old,
        "old-event",
        new_observation.as_ref(),
        new_decision,
        &mut failures,
    );
    assert_why_excludes_non_requested_event(
        &new,
        "new-event",
        old_observation.as_ref(),
        old_decision,
        &mut failures,
    );
    if old == new {
        failures.push("old and new why walks were not event-specific".to_string());
    }

    assert_contract(failures);
}

#[test]
fn event_history_survives_store_reopen() {
    let world = rtsp_world_or_fail();
    let _ = world.run_runtime_until_log_contains("observation_written=true");
    let first = world.open_store().ok();
    let before_observations = list_observations(first.as_ref());
    drop(first);
    let reopened = world.open_store().ok();
    let after_observations = list_observations(reopened.as_ref());
    let authority = cg_authority(reopened.as_ref());
    drop(reopened);
    poison_non_cg_sidecars(&world);
    let why = world.run_cli(["why", "--latest"]);
    let text = command_text(&why);
    let mut failures = Vec::new();

    if before_observations.is_empty() || after_observations.is_empty() {
        failures.push("landed event was not present after store reopen".to_string());
    }
    match authority {
        Ok(authority) => {
            assert_why_matches_authority(&text, &authority, &mut failures);
            if !text.contains(
                authority
                    .camera
                    .properties
                    .get("rtsp_url")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default(),
            ) {
                failures.push("reopened event walk omitted camera RTSP URL".to_string());
            }
            if !text.contains(
                authority
                    .observation
                    .observed_properties
                    .get("class")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default(),
            ) {
                failures.push("reopened event walk omitted detection class".to_string());
            }
        }
        Err(error) => failures.push(error),
    }

    assert_contract(failures);
}

#[test]
fn transient_rtsp_drop_recovers_without_losing_camera() {
    let mut world = rtsp_world_or_fail();
    let seed_runtime = world.run_runtime_until_observation_count(1);
    let seeded_store = world.open_store().ok();
    let seeded_observations = list_observations(seeded_store.as_ref());
    let seeded_ids = seeded_observations
        .iter()
        .map(|observation| observation.id.to_string())
        .collect::<BTreeSet<_>>();
    drop(seeded_store);

    let mut runtime = LiveRuntime::spawn(&world);
    let before_drop = runtime
        .observe_until_log_contains("observation_written=true", Duration::from_secs(180))
        .with_fixture_evidence(&world);
    let opened_before_reconnect = before_drop.logs.matches("rtsp opened").count();
    let stats_before_drop = command_text(&world.run_cli(["stats"]));
    let drop_started_at = Utc::now();
    if let Err(error) = world.stop_rtsp_publisher() {
        assert_contract(vec![error]);
    }
    let after_drop = runtime
        .observe_until_log_contains("rtsp probe failed", Duration::from_secs(30))
        .with_fixture_evidence(&world);
    if !after_drop.logs.contains("rtsp probe failed") {
        assert_contract(vec![
            "runtime did not observe a real RTSP ingest failure after publisher stop".to_string(),
        ]);
    }
    if let Err(error) = world.start_rtsp_publisher() {
        assert_contract(vec![error]);
    }
    let after_session_reopen = runtime
        .observe_until_log_occurrences(
            "rtsp opened",
            opened_before_reconnect.saturating_add(1),
            Duration::from_secs(60),
        )
        .with_fixture_evidence(&world);
    let _after_reconnect_segment = runtime
        .observe_until_decoded_frames(
            after_session_reopen.decoded_frames.saturating_add(1),
            Duration::from_secs(60),
        )
        .with_fixture_evidence(&world);
    let event_after_reconnect =
        wait_for_live_event_after(&world, drop_started_at, Duration::from_secs(360));
    let after_reconnect = runtime.observe().with_fixture_evidence(&world);
    let stats_after_reconnect = command_text(&world.run_cli(["stats"]));
    let _ = runtime.terminate();
    let store = world.open_store().ok();
    let cameras = device_entities(store.as_ref());
    let observations = list_observations(store.as_ref());
    let new_observations = observations
        .iter()
        .filter(|observation| !seeded_ids.contains(&observation.id.to_string()))
        .collect::<Vec<_>>();
    let mut failures = Vec::new();

    if !seed_runtime.spawned || !seed_runtime.rtsp_opened || seeded_observations.is_empty() {
        failures.push("pre-drop snapshot did not land a baseline Observation".to_string());
    }
    if cameras.len() != 1 {
        failures.push(format!(
            "camera Entity must survive reconnect exactly once, found {}",
            cameras.len()
        ));
    }
    if new_observations.is_empty() {
        failures.push("no new Observation landed after reconnect".to_string());
    }
    if !events_text_has_observed_after(&command_text(&event_after_reconnect), drop_started_at) {
        failures.push(
            "live event surface did not expose an Observation after the RTSP reconnect".to_string(),
        );
    }
    if !new_observations
        .iter()
        .any(|observation| observation.observed_at > drop_started_at)
    {
        failures.push("no Observation carried an event time after the RTSP reconnect".to_string());
    }
    let stream_drops_before = parsed_stat(&stats_before_drop, "stream-drops").unwrap_or_default();
    let stream_drops_after =
        parsed_stat(&stats_after_reconnect, "stream-drops").unwrap_or_default();
    let stream_reconnects_before =
        parsed_stat(&stats_before_drop, "stream-reconnects").unwrap_or_default();
    let stream_reconnects_after =
        parsed_stat(&stats_after_reconnect, "stream-reconnects").unwrap_or_default();
    if stream_drops_after <= stream_drops_before
        || stream_reconnects_after <= stream_reconnects_before
    {
        failures.push(format!(
            "stream drop/reconnect counters did not record a real recovery: drops {stream_drops_before}->{stream_drops_after}, reconnects {stream_reconnects_before}->{stream_reconnects_after}"
        ));
    }
    let frames_before = parsed_stat(&stats_before_drop, "frames-received").unwrap_or_default();
    let frames_after = parsed_stat(&stats_after_reconnect, "frames-received").unwrap_or_default();
    if frames_after <= frames_before {
        failures.push("RTSP frame counter did not increase after reconnect".to_string());
    }
    if after_reconnect.decoded_frames <= before_drop.decoded_frames {
        failures.push(
            "runtime logs did not show additional decoded frames after reconnect".to_string(),
        );
    }
    let detector_before =
        parsed_stat(&stats_before_drop, "detector-invocations").unwrap_or_default();
    let detector_after =
        parsed_stat(&stats_after_reconnect, "detector-invocations").unwrap_or_default();
    if detector_after <= detector_before {
        failures.push("detector invocation counter did not increase after reconnect".to_string());
    }
    if after_reconnect.detector_invocations <= before_drop.detector_invocations {
        failures.push(
            "runtime logs did not show another detector invocation after reconnect".to_string(),
        );
    }
    if !after_reconnect.rtsp_network_connected || !after_reconnect.rtsp_fixture_read_observed {
        failures
            .push("RTSP fixture did not observe runtime consumption after reconnect".to_string());
    }

    assert_contract(failures);
}

#[test]
fn startup_crash_mid_config_heals_on_restart() {
    let world = rtsp_world_or_fail();
    let mut faulted = LiveRuntime::spawn_with_env(
        &world,
        &[("VIGIL_FAULT_CRASH_AFTER_STARTUP_NODE", "detector-decision")],
    );
    let fault_status = faulted.wait_for_exit(Duration::from_secs(3));
    if fault_status.is_none() {
        let _ = faulted.kill_without_flush();
    }
    let second = world.run_runtime_until_log_contains("observation_written=true");
    let store = world.open_store().ok();
    let cameras = device_entities(store.as_ref());
    let observations = list_observations(store.as_ref());
    let intentions = baseline_intentions(store.as_ref());
    let decisions = store
        .as_ref()
        .and_then(|store| detector_decisions(store).ok())
        .unwrap_or_default();
    let mut failures = Vec::new();

    if !matches!(fault_status, Some(status) if !status.success()) {
        failures.push(
            "startup fault hook did not kill the runtime during config-node creation".to_string(),
        );
    }
    if !second.spawned {
        failures.push("startup/restart sequence did not run through the binary".to_string());
    }
    if !second.logs.contains("runtime_memory_ready") {
        failures.push("restart did not rebuild the runtime memory graph".to_string());
    }
    if list_context_count(store.as_ref()) != 1 {
        failures.push("restart did not heal to exactly one site Context".to_string());
    }
    if cameras.len() != 1 {
        failures.push(format!(
            "restart did not heal to exactly one camera Entity, found {}",
            cameras.len()
        ));
    }
    if decision_audit_count(store.as_ref()) != 1 {
        failures.push("restart did not heal to exactly one detector Decision".to_string());
    }
    if intentions.len() != 1 {
        failures.push(format!(
            "restart did not heal to exactly one baseline Intention, found {}",
            intentions.len()
        ));
    }
    if let Some(intention) = intentions.first()
        && !decisions
            .iter()
            .any(|decision| decision.intention_ids.contains(&intention.id))
    {
        failures.push(
            "healed detector Decision did not serve the healed baseline Intention".to_string(),
        );
    }
    if observations.is_empty() {
        failures.push("post-restart event did not walk the healed graph".to_string());
    }

    assert_contract(failures);
}

#[test]
fn disk_full_on_clip_write_surfaces_and_drops_no_evidence() {
    let world = rtsp_world_or_fail();
    let mut failures = Vec::new();
    if let Err(error) = world.make_clip_dir_read_only() {
        failures.push(error);
    }
    let mut runtime = LiveRuntime::spawn(&world);
    let faulted_runtime = runtime
        .observe_until_log_contains("record_detection_failed", Duration::from_secs(360))
        .with_fixture_evidence(&world);
    let faulted_health = wait_for_unhealthy_health(world.health_port, Duration::from_secs(2));
    let _ = runtime.terminate();
    let _ = world.make_clip_dir_writable();
    let store = world.open_store().ok();
    let observations = list_observations(store.as_ref());
    let failed_artifacts = local_clip_artifact_paths(&world);
    let stats = command_text(&world.run_cli(["stats"]));

    if !faulted_runtime.rtsp_network_connected || !faulted_runtime.rtsp_fixture_read_observed {
        failures.push("disk-full stimulus was not consumed over the RTSP fixture".to_string());
    }
    if !faulted_runtime.logs.contains("record_detection_failed") {
        failures.push(format!(
            "disk-full runtime did not reach the clip-write failure path; logs:\n{}",
            faulted_runtime.logs
        ));
    }
    if faulted_health.is_none() || faulted_health == Some(200) {
        failures.push(format!(
            "/health did not degrade during ENOSPC clip-write fault; observed status {faulted_health:?}"
        ));
    }
    if parsed_stat(&stats, "clip-write-failures").unwrap_or_default() <= 0.0 {
        failures.push(format!(
            "disk-full clip write was not visible in stats; stats:\n{stats}\nruntime logs:\n{}",
            faulted_runtime.logs
        ));
    }
    if !observations.is_empty() {
        failures.push(format!(
            "disk-full path created {} Observations; failed clip writes must not land evidence",
            observations.len()
        ));
    }
    if observations
        .iter()
        .flat_map(|observation| observation.evidence.iter())
        .any(|evidence| !evidence.source_ref.is_empty())
    {
        failures.push("disk-full path created EvidenceRefs for a failed event".to_string());
    }
    if !stats.contains("health=disk-full") && !stats.contains("disk-full") {
        failures.push(format!(
            "health did not surface disk-full recording failure; stats:\n{stats}\nruntime logs:\n{}",
            faulted_runtime.logs
        ));
    }
    drop(store);

    let _ = world.run_runtime_until_log_contains("observation_written=true");
    let recovered_store = world.open_store().ok();
    let recovered_observations = list_observations(recovered_store.as_ref());
    let recovered_clip_paths = resolved_clip_paths(&world, &recovered_observations);
    if recovered_observations.is_empty() {
        failures.push("post-ENOSPC recovery did not land a later Observation".to_string());
    }
    for failed_artifact in &failed_artifacts {
        let failed_artifact_text = failed_artifact.to_string_lossy();
        if recovered_observations
            .iter()
            .flat_map(|observation| observation.evidence.iter())
            .any(|evidence| {
                evidence.source_ref == failed_artifact_text
                    || evidence.source_ref.contains(failed_artifact_text.as_ref())
            })
        {
            failures.push(format!(
                "a later Observation referenced failed disk-full artifact {}",
                failed_artifact.display()
            ));
        }
        if recovered_clip_paths
            .iter()
            .any(|path| path == failed_artifact)
        {
            failures.push(format!(
                "a later Observation resolved to failed disk-full artifact {}",
                failed_artifact.display()
            ));
        }
    }
    assert_no_runtime_answer_labels(&world, &mut failures);

    assert_contract(failures);
}

#[test]
fn vigil_events_lists_recent_events_newest_first() {
    let mut world = rtsp_world_or_fail();
    let mut failures = Vec::new();
    run_out_of_order_observed_at_fixture(&mut world, &mut failures);
    let store = world.open_store().ok();
    let landed = list_observations(store.as_ref());
    let observations = observations_newest_observed_first(&landed);
    let committed = observations_newest_committed_first(&landed);
    drop(store);
    poison_non_cg_sidecars(&world);
    let events = world.run_cli(["events"]);
    let text = command_text(&events);
    let readback_store = world.open_store().ok();

    assert_no_runtime_answer_labels(&world, &mut failures);
    if observations.len() < 2 {
        failures.push(format!(
            "event-order fixture did not land at least two Observations, found {}",
            observations.len()
        ));
    }
    assert_observed_time_conflicts_with_commit_order(&observations, &committed, &mut failures);
    let rows = event_rows(&text, &observations);
    assert_event_rows_match_observed_at(&rows, &observations, &mut failures);
    assert_event_rows_match_observation_fields(
        &rows,
        &observations,
        readback_store.as_ref(),
        &mut failures,
    );
    if text.trim().is_empty() {
        failures.push("event list was empty despite landed Observations".to_string());
    }
    if rows.len() != observations.len() {
        failures.push(format!(
            "event rows were not a bijection with landed Observations: rows={}, observations={}",
            rows.len(),
            observations.len()
        ));
    }
    if !text.contains(CAMERA_NAME) {
        failures.push("event rows did not resolve the camera name".to_string());
    }
    if parsed_confidence_fields(&text).is_empty() {
        failures.push("event rows did not include parseable numeric confidence values".to_string());
    }
    if !text.contains("clip") {
        failures.push("event rows did not include clip paths".to_string());
    }
    let event_ids = observations
        .iter()
        .map(|observation| observation.id.to_string())
        .collect::<Vec<_>>();
    let unexpected_rows = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !event_ids.iter().any(|id| line.contains(id)))
        .collect::<Vec<_>>();
    if !unexpected_rows.is_empty() {
        failures.push(format!(
            "event output included rows that did not map to landed Observations: {unexpected_rows:?}"
        ));
    }
    for id in &event_ids {
        if !text.contains(id) {
            failures.push(format!("event row omitted cg Observation id {id}"));
        }
    }
    if event_ids.len() >= 2 {
        let first = rows.iter().position(|row| row.contains(&event_ids[0]));
        let second = rows.iter().position(|row| row.contains(&event_ids[1]));
        if !matches!((first, second), (Some(a), Some(b)) if a < b) {
            failures.push(
                "event rows were not ordered newest-observed-at first against cg authority"
                    .to_string(),
            );
        }
    }

    assert_contract(failures);
}

#[test]
fn vigil_why_latest_walks_the_newest_event() {
    let mut world = rtsp_world_or_fail();
    let mut failures = Vec::new();
    run_out_of_order_observed_at_fixture(&mut world, &mut failures);
    let store = world.open_store().ok();
    let landed = list_observations(store.as_ref());
    let observed = observations_newest_observed_first(&landed);
    let committed = observations_newest_committed_first(&landed);
    let authority = cg_authority(store.as_ref());
    drop(store);
    poison_non_cg_sidecars(&world);
    let why = command_text(&world.run_cli(["why", "--latest"]));

    assert_no_runtime_answer_labels(&world, &mut failures);
    if !why.contains("newest") {
        failures.push("why --latest did not identify the newest event by observed_at".to_string());
    }
    assert_observed_time_conflicts_with_commit_order(&observed, &committed, &mut failures);
    if let (Some(observed_newest), Some(commit_newest)) = (observed.first(), committed.first())
        && observed_newest.id != commit_newest.id
        && why.contains(&commit_newest.id.to_string())
    {
        failures.push(
            "why --latest resolved the newest committed event instead of max observed_at"
                .to_string(),
        );
    }
    match authority {
        Ok(authority) => assert_why_matches_authority(&why, &authority, &mut failures),
        Err(error) => failures.push(error),
    }

    assert_contract(failures);
}

#[test]
fn vigil_events_shows_only_landed_events_not_motion_only() {
    let mut world = rtsp_world_or_fail();
    let _ = world.run_runtime_until_log_contains("observation_written=true");
    let stats_after_person = command_text(&world.run_cli(["stats"]));
    let clip_count_after_person = local_clip_artifact_count(&world);
    let staging_after_person = local_staging_artifact_paths(&world)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let motion_positive_person_free_clip = generate_motion_positive_person_free_clip(&world);
    if let Ok(clip) = motion_positive_person_free_clip.as_ref()
        && let Err(error) = world.switch_rtsp_clip(clip)
    {
        assert_contract(vec![format!(
            "could not switch RTSP publisher to motion-positive person-free clip: {error}"
        )]);
    }
    let forward_probe_path = world
        .data_dir
        .join("motion-person-free-detector-forward-probe.log");
    let forward_probe_path_string = forward_probe_path.display().to_string();
    let forward_probe_nonce = unique_probe_nonce("motion-person-free");
    let forward_probe_env = [
        (
            DETECTOR_FORWARD_PROBE_ENV,
            forward_probe_path_string.as_str(),
        ),
        (
            DETECTOR_FORWARD_PROBE_NONCE_ENV,
            forward_probe_nonce.as_str(),
        ),
    ];
    let rejected_runtime =
        world.run_runtime_until_log_contains_with_env("detector_detections=0", &forward_probe_env);
    let stats_after_rejected = command_text(&world.run_cli(["stats"]));
    let clip_count_after_rejected = local_clip_artifact_count(&world);
    let store = world.open_store().ok();
    let landed = list_observations(store.as_ref());
    let landed_ids = landed
        .iter()
        .map(|observation| observation.id.to_string())
        .collect::<Vec<_>>();
    drop(store);
    poison_non_cg_sidecars(&world);
    let events = command_text(&world.run_cli(["events"]));
    let rows = event_rows(&events, &landed);
    let mut failures = Vec::new();
    let rejected_motion_frames = motion_positive_person_free_clip
        .as_ref()
        .ok()
        .and_then(|clip| motion_positive_frame_count(clip).ok())
        .unwrap_or_default();
    let rejected_staging_paths = local_staging_artifact_paths(&world)
        .into_iter()
        .filter(|path| !staging_after_person.contains(path))
        .collect::<Vec<_>>();
    let model_digest = sha256_file(&world.detector_artifact).unwrap_or_default();
    let forward_events = read_probe_events(
        &forward_probe_path,
        &forward_probe_nonce,
        DETECTOR_FORWARD_EVENT_TYPE,
        "motion-positive person-free detector forward",
        &mut failures,
    );
    if let Err(error) = motion_positive_person_free_clip.as_ref() {
        failures.push(format!(
            "could not generate detector-negative motion-positive stimulus: {error}"
        ));
    }
    if rejected_motion_frames == 0 {
        failures.push("detector-negative stimulus was not motion-positive".to_string());
    }
    if !rejected_runtime.rtsp_network_connected || !rejected_runtime.rtsp_fixture_read_observed {
        failures.push("detector-negative motion-positive stimulus was not consumed".to_string());
    }
    if rejected_runtime.detector_invocations == 0 {
        failures.push(
            "detector-negative motion-positive stimulus did not invoke the detector".to_string(),
        );
    }
    if !rejected_staging_paths.is_empty() {
        failures.push(format!(
            "detector-negative motion-positive stimulus leaked staging clips: {rejected_staging_paths:?}"
        ));
    }
    let rejected_clip_digest = forward_events
        .iter()
        .find_map(|event| parsed_line_field(event, "clip_sha256"))
        .map(ToString::to_string);
    assert_detector_forward_event(
        &forward_events,
        "motion-positive person-free",
        rejected_clip_digest.as_deref(),
        &model_digest,
        None,
        None,
        &mut failures,
    );
    let motion_before =
        parsed_stat(&stats_after_person, "motion-positive-frames").unwrap_or_default();
    let motion_after =
        parsed_stat(&stats_after_rejected, "motion-positive-frames").unwrap_or_default();
    if motion_after <= motion_before {
        failures.push(
            "motion-positive frame counter did not increase for person-free motion".to_string(),
        );
    }
    let detector_before =
        parsed_stat(&stats_after_person, "detector-invocations").unwrap_or_default();
    let detector_after =
        parsed_stat(&stats_after_rejected, "detector-invocations").unwrap_or_default();
    if detector_after <= detector_before {
        failures.push(
            "detector invocation counter did not increase for person-free motion".to_string(),
        );
    }
    let detections_before =
        parsed_stat(&stats_after_person, "detections-emitted").unwrap_or_default();
    let detections_after =
        parsed_stat(&stats_after_rejected, "detections-emitted").unwrap_or_default();
    if detections_after != detections_before {
        failures.push("person-free motion changed the detector-confirmed event count".to_string());
    }
    if landed.len() != 1 {
        failures.push(format!(
            "expected exactly one landed Observation, found {}",
            landed.len()
        ));
    }
    if rows.len() != landed.len() {
        failures.push(format!(
            "event surface did not return exactly one row per landed Observation: rows={}, observations={}",
            rows.len(),
            landed.len()
        ));
    }
    for id in landed_ids {
        if !events.contains(&id) {
            failures.push(format!("event surface omitted landed Observation id {id}"));
        }
    }
    if events.contains("motion-only") || events.contains("placeholder") {
        failures.push("event surface listed a non-landed or placeholder row".to_string());
    }
    if clip_count_after_rejected != clip_count_after_person {
        failures.push(format!(
            "detector-negative motion-positive stimulus created clip artifacts: before={clip_count_after_person}, after={clip_count_after_rejected}"
        ));
    }

    assert_contract(failures);
}

#[test]
fn vigil_stats_reports_live_pipeline_counters() {
    let mut world = rtsp_world_or_fail();
    let mut failures = Vec::new();
    if let Err(error) = world.switch_rtsp_clip(&world.empty_clip.clone()) {
        failures.push(format!(
            "could not switch RTSP publisher to empty stats baseline: {error}"
        ));
    }
    let baseline_frames = count_video_frames(&world.empty_clip).unwrap_or(48).min(48);
    let first_runtime = world.run_runtime_until_decoded_frames(baseline_frames);
    let normal_stats_text = command_text(&world.run_cli(["stats"]));
    let normal_motion_positive =
        parsed_stat(&normal_stats_text, "motion-positive-frames").unwrap_or_default();
    let normal_dropped =
        parsed_stat(&normal_stats_text, "dropped-motion-positive-frames").unwrap_or_default();
    let normal_lag = parsed_stat(&normal_stats_text, "processing-lag-ms").unwrap_or_default();
    let normal_lag_bound =
        parsed_stat(&normal_stats_text, "processing-lag-bound-ms").unwrap_or_default();

    let workload_clip = match generate_motion_positive_person_clip(&world) {
        Ok(clip) => clip,
        Err(error) => {
            failures.push(format!(
                "could not generate bounded stats workload: {error}"
            ));
            world.person_clip.clone()
        }
    };
    let expected_pressure_motion =
        motion_positive_frame_count(&workload_clip).unwrap_or_default() as f64;
    let expected_fps = video_fps(&workload_clip).unwrap_or_default();
    if let Err(error) = world.switch_rtsp_clip(&workload_clip) {
        failures.push(format!(
            "could not switch RTSP publisher to bounded stats workload: {error}"
        ));
    }
    if expected_pressure_motion <= 1.0 {
        failures.push(format!(
            "pressure workload has {expected_pressure_motion} motion-positive frames, not enough to prove bounded-backlog drops"
        ));
    }
    if let Err(error) = world.make_clip_dir_read_only() {
        failures.push(error);
    }
    let second_runtime = world.run_runtime_until_log_contains_with_env(
        "record_detection_failed",
        &[
            PRESSURE_CAPTURE_FRAMES_ENV,
            PRESSURE_DETECTOR_QUEUE_ENV,
            PRESSURE_DETECTOR_WORK_DELAY_ENV,
        ],
    );
    let _ = world.make_clip_dir_writable();
    let stats_text = command_text(&world.run_cli(["stats"]));
    let store = world.open_store().ok();
    let k = list_observations(store.as_ref()).len() as f64;
    let frames_received = parsed_stat(&stats_text, "frames-received").unwrap_or_default();
    let detections_emitted = parsed_stat(&stats_text, "detections-emitted").unwrap_or_default();
    let observations_written = parsed_stat(&stats_text, "observations-written").unwrap_or_default();
    let clip_write_failures = parsed_stat(&stats_text, "clip-write-failures").unwrap_or_default();
    let observation_write_failures =
        parsed_stat(&stats_text, "observation-write-failures").unwrap_or_default();
    let motion_positive_frames =
        parsed_stat(&stats_text, "motion-positive-frames").unwrap_or_default();
    let stream_fps = parsed_stat(&stats_text, "stream-fps").unwrap_or_default();
    let latency_p50 = parsed_stat(&stats_text, "detector-latency-p50-ms").unwrap_or_default();
    let latency_p95 = parsed_stat(&stats_text, "detector-latency-p95-ms").unwrap_or_default();
    let latency_max = parsed_stat(&stats_text, "detector-latency-max-ms").unwrap_or_default();
    let dropped_motion_positive =
        parsed_stat(&stats_text, "dropped-motion-positive-frames").unwrap_or_default();
    let processing_lag = parsed_stat(&stats_text, "processing-lag-ms").unwrap_or_default();
    let processing_lag_bound =
        parsed_stat(&stats_text, "processing-lag-bound-ms").unwrap_or_default();

    assert_no_runtime_answer_labels(&world, &mut failures);
    if normal_dropped != 0.0 {
        failures.push(format!(
            "empty baseline dropped {normal_dropped} motion-positive frames before pressure"
        ));
    }
    if normal_lag_bound <= 0.0 {
        failures.push("normal stats omitted a positive processing-lag bound".to_string());
    }
    if normal_lag_bound > 0.0 && normal_lag > normal_lag_bound {
        failures.push(format!(
            "empty baseline exceeded keep-pace bound before pressure: lag={normal_lag}, bound={normal_lag_bound}"
        ));
    }
    if normal_motion_positive != 0.0 {
        failures.push(format!(
            "empty baseline reported {normal_motion_positive} motion-positive frames"
        ));
    }
    if normal_stats_text.contains("health=keep-pace-failed")
        || normal_stats_text.contains("keep-pace-health=failed")
    {
        failures.push("empty baseline reported keep-pace failure before pressure".to_string());
    }
    if first_runtime.decoded_frames < baseline_frames {
        failures.push(format!(
            "empty stats baseline consumed {} frames, expected at least {baseline_frames}",
            first_runtime.decoded_frames
        ));
    }
    if !second_runtime.logs.contains("record_detection_failed") {
        failures.push("pressure workload did not reach a real clip-write failure".to_string());
    }
    let delivered_frames = (first_runtime.decoded_frames + second_runtime.decoded_frames) as f64;
    if frames_received < delivered_frames {
        failures.push(format!(
            "frames-received {frames_received} was below delivered workload frames {delivered_frames}"
        ));
    }
    if detections_emitted < k + clip_write_failures {
        failures.push(format!(
            "detections-emitted should cover observations plus failed clips; got {detections_emitted}, observations {k}, clip failures {clip_write_failures}"
        ));
    }
    if detections_emitted < observations_written + clip_write_failures + observation_write_failures
    {
        failures.push("stats durability counters did not cover failed writes".to_string());
    }
    if clip_write_failures < 1.0 {
        failures.push("forced clip-write failure was not counted".to_string());
    }
    if observation_write_failures != 0.0 {
        failures.push("observation-write-failures should be zero in this scenario".to_string());
    }
    if motion_positive_frames < normal_motion_positive {
        failures.push(format!(
            "motion-positive-frames regressed from normal {normal_motion_positive} to total {motion_positive_frames}"
        ));
    }
    let pressure_motion_positive = motion_positive_frames - normal_motion_positive;
    if pressure_motion_positive <= 0.0
        || pressure_motion_positive > second_runtime.decoded_frames as f64
    {
        failures.push(format!(
            "pressure workload motion-positive-frames {pressure_motion_positive} was not within delivered pressure frames {}",
            second_runtime.decoded_frames
        ));
    }
    if expected_fps == 0.0 {
        failures.push("pressure workload did not expose a fixture FPS".to_string());
    }
    if stream_fps <= 0.0 || !stream_fps.is_finite() {
        failures.push(format!(
            "stream-fps {stream_fps} did not report a positive live cadence"
        ));
    }
    if latency_p50 <= 0.0 || latency_p95 <= 0.0 || latency_max <= 0.0 {
        failures.push("detector latency p50/p95/max were absent or zero".to_string());
    }
    if !(latency_p50 <= latency_p95 && latency_p95 <= latency_max) {
        failures.push("detector latency p50/p95/max were not ordered".to_string());
    }
    if dropped_motion_positive <= normal_dropped {
        failures.push(format!(
            "keep-pace pressure did not increase dropped motion-positive frames: normal={normal_dropped}, pressure-total={dropped_motion_positive}"
        ));
    }
    if dropped_motion_positive <= 0.0 {
        failures.push("bounded-backlog pressure did not drop a motion-positive frame".to_string());
    }
    if processing_lag <= processing_lag_bound {
        failures.push("keep-pace failure did not exceed the processing-lag bound".to_string());
    }
    if !stats_text.contains("health=keep-pace-failed")
        && !stats_text.contains("keep-pace-health=failed")
    {
        failures.push("keep-pace health did not flip to failed".to_string());
    }
    if !stats_text.contains("health=disk-full") && !stats_text.contains("disk-full") {
        failures.push("disk-full health did not reflect the ENOSPC clip-write fault".to_string());
    }

    assert_contract(failures);
}

#[test]
fn vigil_events_on_empty_store_returns_clean_empty_list() {
    let world = world_or_fail();
    let events = world.run_cli(["events"]);
    let text = command_text(&events);
    let store = world.open_store().ok();
    let observations = list_observations(store.as_ref());
    let mut failures = Vec::new();

    if !events.status_success {
        failures.push("events command did not return success on an empty store".to_string());
    }
    if !text.trim().is_empty() {
        failures.push("events command did not return a clean empty list".to_string());
    }
    if !observations.is_empty() {
        failures.push(format!(
            "fresh store had {} Observations",
            observations.len()
        ));
    }

    assert_contract(failures);
}

#[test]
fn vigil_why_latest_on_empty_store_is_clean_not_found() {
    let world = world_or_fail();
    let response = world.run_cli(["why", "--latest"]);
    let text = command_text(&response);
    let mut failures = Vec::new();

    if response.status_success {
        failures.push(
            "why --latest on an empty store returned success instead of not-found".to_string(),
        );
    }
    if !text.to_ascii_lowercase().contains("not found") {
        failures.push("why --latest did not return an observable not-found".to_string());
    }
    if text.contains("runtime owner unavailable") {
        failures.push(
            "why --latest leaked the owner-routing failure instead of a clean not-found"
                .to_string(),
        );
    }
    if text.to_ascii_lowercase().contains("usage") {
        failures.push("why --latest returned usage text instead of not-found".to_string());
    }

    assert_contract(failures);
}

#[test]
fn vigil_stats_before_any_frame_is_all_zero_no_panic() {
    let world = world_or_fail();
    let response = world.run_cli(["stats"]);
    let stats = command_text(&response);
    let mut failures = Vec::new();

    if !response.status_success {
        failures.push(format!(
            "stats command before any frame did not exit successfully: {stats}"
        ));
    }
    for (key, expected) in [
        ("frames-received", 0.0),
        ("detections-emitted", 0.0),
        ("observations-written", 0.0),
        ("motion-positive-frames", 0.0),
        ("clip-write-failures", 0.0),
        ("observation-write-failures", 0.0),
        ("stream-drops", 0.0),
        ("stream-reconnects", 0.0),
        ("processing-lag-ms", 0.0),
        ("dropped-motion-positive-frames", 0.0),
        ("detector-latency-p50-ms", 0.0),
        ("detector-latency-p95-ms", 0.0),
        ("detector-latency-max-ms", 0.0),
        ("stream-fps", 0.0),
    ] {
        if parsed_stat(&stats, key) != Some(expected) {
            failures.push(format!("pre-frame counter {key} was not {expected}"));
        }
    }
    if parsed_stat(&stats, "processing-lag-bound-ms").is_none() {
        failures.push("stats output omitted processing-lag-bound-ms".to_string());
    }
    let stats_lower = stats.to_ascii_lowercase();
    for forbidden in [
        "n/a",
        "placeholder",
        "todo",
        "panic",
        "backtrace",
        "usage",
        "error",
    ] {
        if stats_lower.contains(forbidden) {
            failures.push(format!(
                "stats output used forbidden first-run failure/placeholder token {forbidden}"
            ));
        }
    }

    assert_contract(failures);
}

#[test]
fn review_and_stats_surfaces_make_no_network_call() {
    let world = rtsp_world_or_fail();
    let _ = world.run_runtime_until_observation_count(1);
    let store = world.open_store().ok();
    let observations = list_observations(store.as_ref());
    let authority = cg_authority(store.as_ref());
    drop(store);

    let events = run_vigil_command_traced(["events"], Some(&world));
    let why = run_vigil_command_traced(["why", "--latest"], Some(&world));
    let stats = run_vigil_command_traced(["stats"], Some(&world));
    let event_text = command_text(&events.command);
    let why_text = command_text(&why.command);
    let stats_text = command_text(&stats.command);
    let observed_network_attempts = events
        .outbound_attempts
        .iter()
        .chain(why.outbound_attempts.iter())
        .chain(stats.outbound_attempts.iter())
        .cloned()
        .collect::<Vec<_>>();
    let mut failures = Vec::new();

    if observations.is_empty() {
        failures.push(
            "no-egress review fixture did not land a cg Observation before tracing".to_string(),
        );
    }
    if let Ok(authority) = authority.as_ref() {
        assert_events_match_authority(&event_text, authority, &mut failures);
        assert_why_matches_authority(&why_text, authority, &mut failures);
    } else {
        failures.push(format!(
            "no-egress review fixture did not load cg authority before tracing: {}",
            authority
                .err()
                .unwrap_or_else(|| "unknown authority error".to_string())
        ));
    }
    for (label, traced) in [("events", &events), ("why", &why), ("stats", &stats)] {
        if !traced.trace_available {
            failures.push(format!(
                "{label} command did not run under fail-closed strace network tracing"
            ));
        }
        if traced.trace_file_count == 0 {
            failures.push(format!("{label} command produced no strace output files"));
        }
        if traced.raw_trace_lines == 0 {
            failures.push(format!("{label} command produced an empty strace capture"));
        }
        if !traced.command.status_success {
            failures.push(format!(
                "{label} command failed under no-egress trace: {}",
                command_text(&traced.command)
            ));
        }
    }
    if !observed_network_attempts.is_empty() {
        failures.push(format!(
            "review/stats default path made outbound network attempts: {observed_network_attempts:?}"
        ));
    }
    if event_text.contains("placeholder")
        || why_text.contains("placeholder")
        || stats_text.contains("n/a")
    {
        failures.push(
            "review/stats commands returned placeholder output instead of local data".to_string(),
        );
    }

    assert_contract(failures);
}

#[test]
fn store_opens_with_text_embedder_disabled_no_model_fetch() {
    let world = world_or_fail();
    let opened =
        match vigil::open_context_graph_store_with_text_embedder_disabled(&world.store_path) {
            Ok(store) => store,
            Err(error) => {
                assert!(error.is_empty(), "runtime store open failed: {error}");
                unreachable!("assertion above always fails");
            }
        };
    let trace = opened
        .last_query_trace()
        .unwrap_or_else(|error| format!("trace-error={error}"));

    assert_eq!(
        trace, "embedding_loader=disabled",
        "runtime store open must keep the bundled text embedder disabled"
    );
}

#[test]
fn first_light_loop_makes_no_network_call_beyond_rtsp() {
    let world = rtsp_world_or_fail();
    let readiness = FixtureReadiness::probe(&world);
    let (runtime, store) = run_runtime_until_log_contains_and_open_store_with_env(
        &world,
        &[],
        "observation_written=true",
    );
    let observations = list_observations(store.as_ref());
    let mut failures = readiness.missing_messages();

    if !runtime.rtsp_opened {
        failures.push("runtime did not open the configured direct RTSP source".to_string());
    }
    if !runtime.rtsp_network_connected {
        failures.push("runtime did not connect to the exact configured RTSP endpoint".to_string());
    }
    if !runtime.rtsp_fixture_published || !runtime.rtsp_fixture_read_observed {
        failures.push("RTSP fixture did not observe publish and read activity".to_string());
    }
    if !runtime.rtsp_play_observed {
        failures.push("runtime did not report RTSP PLAY after consuming the fixture".to_string());
    }
    if runtime.decoded_frames == 0 {
        failures.push("runtime did not decode real frames from the RTSP source".to_string());
    }
    if runtime.detector_invocations == 0 {
        failures.push("detector was not invoked on decoded frames".to_string());
    }
    if !runtime.outbound_network_attempts.is_empty() {
        failures.push(format!(
            "runtime made disallowed network attempts: {:?}",
            runtime.outbound_network_attempts
        ));
    }
    if observations.len() != 1 {
        failures.push(format!(
            "runtime did not land exactly one durable Observation, found {}",
            observations.len()
        ));
    }

    assert_contract(failures);
}

fn parsed_confidence_fields(text: &str) -> BTreeMap<String, f64> {
    text.split_whitespace()
        .filter_map(|field| {
            let (key, value) = field.split_once('=')?;
            if key != "confidence" {
                return None;
            }
            value
                .parse::<f64>()
                .ok()
                .map(|confidence| (key.to_string(), confidence))
        })
        .collect()
}
