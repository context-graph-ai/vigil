// Shared test harness support for ha_correction_seam.rs and ha_mqtt_broker.rs.
//
// Included via:
//   #[path = "ha_test_support.rs"]
//   mod ha_test_support;
//
// Provides a OnceLock-seeded template store (3 real observations, driven once per
// test binary), per-test copies via `fresh_store_copy`, and all shared helpers.
//
// Seeder mirrors first_light_loop.rs's proven harness EXACTLY:
//   - 60 s per vigil run, exits on `runtime_reached_pipeline_terminal_signal`
//     (includes motion_gate_suppressed_segment=true — the hang root cause)
//   - restart vigil and retry until the observation count is satisfied or the
//     600 s outer deadline expires

#![allow(dead_code)]

use std::fs;
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use context_graph::{EmbedderConfig, Store, StoreConfig};
use tempfile::TempDir;

// ── Product constants (used by the seeder; exported for test fallbacks) ────

pub const SITE_NAME: &str = "home farm";
pub const CAMERA_NAME: &str = "lower gate";
pub const DETECTOR_MODEL_ID: &str = "yolox-tiny-burn-cpu";
pub const DETECTOR_THRESHOLD: f64 = 0.5;

// ── Store helpers ──────────────────────────────────────────────────────────

pub fn open_store_at(path: &Path) -> Result<Store, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create store parent {}: {e}", parent.display()))?;
    }
    Store::open(StoreConfig {
        db_path: path.to_path_buf(),
        default_text_embedder: Some(EmbedderConfig::disabled()),
        ..StoreConfig::default()
    })
    .map_err(|e| format!("open store {}: {e}", path.display()))
}

// ── Free port ──────────────────────────────────────────────────────────────

pub fn free_port() -> Result<u16, String> {
    // Simple OS-allocated port: bind to :0 and read back the assigned port.
    // Tests run sequentially so we don't need cross-process dedup.
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("allocate TCP port: {e}"))?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("read allocated TCP port: {e}"))
}

// ── Wait for TCP port ──────────────────────────────────────────────────────

pub fn wait_for_tcp_port(port: u16, timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    while start.elapsed() < timeout {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "port {port} did not open within {}s",
        timeout.as_secs()
    ))
}

// ── Capture pipe ───────────────────────────────────────────────────────────

pub fn capture_pipe<T: Read + Send + 'static>(pipe: Option<T>) -> Arc<Mutex<String>> {
    let buf = Arc::new(Mutex::new(String::new()));
    if let Some(mut pipe) = pipe {
        let captured = Arc::clone(&buf);
        thread::spawn(move || {
            let mut tmp = [0u8; 4096];
            loop {
                match pipe.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut g) = captured.lock() {
                            g.push_str(&String::from_utf8_lossy(&tmp[..n]));
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    buf
}

// ── Tool / binary paths ────────────────────────────────────────────────────

pub fn tool_path(env_key: &str, binary: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(env_key).map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    std::env::var_os("PATH")
        .and_then(|path_var| {
            std::env::split_paths(&path_var)
                .map(|dir| dir.join(binary))
                .find(|p| p.is_file())
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

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn vigil_binary_path() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_vigil") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_vigil") {
        return PathBuf::from(path);
    }
    workspace_root().join("target").join("debug").join("vigil")
}

// ── TOML encode helpers ────────────────────────────────────────────────────

pub fn toml_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

pub fn toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

// ── RTSP fixture lock (serialises RTSP fixture use within one test binary) ─

pub fn rtsp_fixture_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ── RTSP fixture (mediamtx + ffmpeg) ──────────────────────────────────────

pub struct RtspFixture {
    pub url: String,
    mediamtx: Child,
    ffmpeg: Option<Child>,
}

impl RtspFixture {
    pub fn start(clip: &Path) -> Result<Self, String> {
        let mediamtx_bin = tool_path("VIGIL_MEDIAMTX_BIN", "mediamtx")
            .ok_or_else(|| "mediamtx not available".to_string())?;
        let ffmpeg_bin = tool_path("VIGIL_FFMPEG_BIN", "ffmpeg")
            .ok_or_else(|| "ffmpeg not available".to_string())?;
        let port = free_port()?;
        let rtp_port = free_port()?;
        let rtcp_port = free_port()?;
        let url = format!("rtsp://127.0.0.1:{port}/lower-gate");
        let conf_dir = tempfile::tempdir().map_err(|e| format!("rtsp conf dir: {e}"))?;
        let conf_path = conf_dir.path().join("mediamtx.yml");
        fs::write(
            &conf_path,
            format!(
                "rtspTransports: [tcp]\nrtspAddress: 127.0.0.1:{port}\n\
                 rtpAddress: 127.0.0.1:{rtp_port}\nrtcpAddress: 127.0.0.1:{rtcp_port}\n\
                 rtmp: no\nhls: no\nwebrtc: no\nsrt: no\nplayback: no\nmoq: no\n\
                 paths:\n  lower-gate:\n    source: publisher\n"
            ),
        )
        .map_err(|e| format!("write mediamtx config: {e}"))?;

        // mediamtx: stdout/stderr null — mirrors first_light_loop.rs exactly.
        let mut mediamtx_child = Command::new(&mediamtx_bin)
            .arg(&conf_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn mediamtx: {e}"))?;

        wait_for_tcp_port(port, Duration::from_secs(5)).inspect_err(|_| {
            let _ = mediamtx_child.kill();
        })?;
        thread::sleep(Duration::from_millis(300));

        // ffmpeg: looping re-stream exactly as first_light_loop.rs does it.
        let ffmpeg_child = Command::new(&ffmpeg_bin)
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
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                let _ = mediamtx_child.kill();
                format!("spawn ffmpeg: {e}")
            })?;
        thread::sleep(Duration::from_millis(400));
        // conf_dir leaked so the config file stays present for mediamtx's lifetime.
        std::mem::forget(conf_dir);
        Ok(Self {
            url,
            mediamtx: mediamtx_child,
            ffmpeg: Some(ffmpeg_child),
        })
    }
}

impl Drop for RtspFixture {
    fn drop(&mut self) {
        if let Some(f) = self.ffmpeg.as_mut() {
            let _ = f.kill();
            let _ = f.wait();
        }
        let _ = self.mediamtx.kill();
        let _ = self.mediamtx.wait();
    }
}

// ── Terminal-signal predicate (mirrors first_light_loop.rs exactly) ────────

/// Returns true when vigil has completed one pipeline run — including the case
/// where the motion gate suppresses the segment.  The harness MUST exit on this
/// signal and restart vigil so it reconnects at a different position in the
/// looping RTSP stream; staying alive for 300 s causes the motion gate to reset
/// on every loop cut and the harness never accumulates enough consecutive
/// motion-positive segments to write an observation.
pub fn runtime_reached_pipeline_terminal_signal(logs: &str) -> bool {
    logs.contains("rtsp opened")
        && logs.contains("decoded_frames=")
        && (logs.contains("observation_written=true")
            || logs.contains("motion_gate_suppressed_segment=true")
            || logs.contains("detector_detections=0")
            || logs.contains("record detection failed")
            || logs.contains("detector invocation failed")
            || logs.contains("rtsp probe failed"))
}

// ── Seeder vigil child wrapper ─────────────────────────────────────────────

struct SeederVigilChild {
    child: Option<Child>,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

impl SeederVigilChild {
    fn logs(&self) -> String {
        let out = self.stdout.lock().map(|l| l.clone()).unwrap_or_default();
        let err = self.stderr.lock().map(|l| l.clone()).unwrap_or_default();
        format!("{out}{err}")
    }

    fn terminate(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
    }
}

impl Drop for SeederVigilChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn spawn_seeder_vigil(
    config_path: &Path,
    data_dir: &Path,
    store_path: &Path,
    rtsp_url: &str,
    detector_artifact: &Path,
    health_port: u16,
) -> SeederVigilChild {
    let mut cmd = Command::new(vigil_binary_path());
    cmd.arg("run")
        .arg("--config")
        .arg(config_path)
        .env("VIGIL_HEALTH_PORT", health_port.to_string())
        .env("VIGIL_DATA_DIR", data_dir)
        .env("VIGIL_STORE_PATH", store_path)
        .env("VIGIL_RTSP_URL", rtsp_url)
        .env("VIGIL_DETECTOR_MODEL_PATH", detector_artifact)
        .env("VIGIL_RTSP_RETRY_INITIAL_MS", "200")
        .env("VIGIL_RTSP_RETRY_MAX_MS", "1000")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match cmd.spawn() {
        Err(_) => SeederVigilChild {
            child: None,
            stdout: Arc::new(Mutex::new(String::new())),
            stderr: Arc::new(Mutex::new(String::new())),
        },
        Ok(mut child) => {
            let stdout = capture_pipe(child.stdout.take());
            let stderr = capture_pipe(child.stderr.take());
            SeederVigilChild {
                child: Some(child),
                stdout,
                stderr,
            }
        }
    }
}

// ── Recursive directory copy ───────────────────────────────────────────────

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("create dst dir {}: {e}", dst.display()))?;
    for entry in fs::read_dir(src).map_err(|e| format!("read dir {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| format!("read dir entry: {e}"))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let metadata = entry
            .metadata()
            .map_err(|e| format!("read metadata {}: {e}", src_path.display()))?;
        if metadata.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if metadata.is_file() {
            // Skip non-regular files (sockets, pipes, symlinks to non-files).
            // fs::copy refuses sockets with ENXIO; the vigil runtime leaves a
            // control.sock in the data dir that must not be propagated into test copies.
            fs::copy(&src_path, &dst_path).map_err(|e| {
                format!("copy {} → {}: {e}", src_path.display(), dst_path.display())
            })?;
        }
        // else: skip sockets, FIFOs, device nodes — not relevant for cg store copies.
    }
    Ok(())
}

// ── On-disk seeded template store ─────────────────────────────────────────
//
// nextest runs each test in a separate OS process, so an in-process OnceLock
// only serialises within a single test.  Instead we maintain ONE on-disk
// template under `target/ha-test-template/` for the whole cargo session:
//
//   target/ha-test-template/data/   — the seeded store directory
//   target/ha-test-template/SEEDED  — marker written once seeding is complete
//   target/ha-test-template.lock    — O_CREAT|O_EXCL advisory lock, held only
//                                     during the actual seeder run
//
// First process to start seeds; the others poll `SEEDED` and inherit.
// Stale locks (process crashed before releasing) are detected by mtime > 720 s.

const TEMPLATE_MIN_OBSERVATIONS: usize = 3;

fn template_data_dir() -> Option<&'static PathBuf> {
    // In-process cache so a single process only seeds/polls once even if
    // multiple tests in the same process call fresh_store_copy concurrently.
    static CACHED: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHED
        .get_or_init(|| resolve_or_seed_template(TEMPLATE_MIN_OBSERVATIONS).ok())
        .as_ref()
}

/// Returns the on-disk template data dir, seeding it exactly once per cargo
/// session.  Concurrent test processes wait until the winner finishes.
fn resolve_or_seed_template(minimum: usize) -> Result<PathBuf, String> {
    let base = workspace_root().join("target").join("ha-test-template");
    let done_marker = base.join("SEEDED");
    let data_dir = base.join("data");
    let store_path = data_dir.join("store.contextgraph");
    let lock_path = base.with_file_name("ha-test-template.lock");

    // ── Fast path ─────────────────────────────────────────────────────────
    // Template is valid if the done marker exists AND the store has at least
    // `minimum` observations.  Skip re-seeding even across test runs.
    if done_marker.is_file() {
        let count = open_store_at(&store_path)
            .ok()
            .and_then(|s| s.list_observations(None).ok())
            .map(|v| v.len())
            .unwrap_or(0);
        if count >= minimum {
            return Ok(data_dir);
        }
        // Marker exists but store is stale/empty (e.g. seeded under an older
        // cg schema version, which fails every open) — remove the marker AND
        // the old data dir, else the seeder's children keep hitting the stale
        // store and burn the full deadline without ever ingesting.
        let _ = fs::remove_file(&done_marker);
        let _ = fs::remove_dir_all(&data_dir);
    }

    // ── Try to win the seeding race with an atomic O_CREAT|O_EXCL lock ───
    let _ = fs::create_dir_all(&base);
    let won_lock = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_path)
        .is_ok();

    if won_lock {
        // We hold the lock — run the seeder.
        let result = seed_template_into(&data_dir, minimum);
        if result.is_ok() {
            let _ = fs::write(&done_marker, b"");
        }
        let _ = fs::remove_file(&lock_path); // release
        result.and(Ok(data_dir))
    } else {
        // Another process holds the lock.  Wait for the done marker (up to 700 s).
        // Also check for stale locks: if the lock file is older than 720 s the
        // seeder process has crashed; remove the lock and retry.
        let deadline = Instant::now() + Duration::from_secs(700);
        loop {
            if done_marker.is_file() {
                return Ok(data_dir);
            }
            if Instant::now() >= deadline {
                break;
            }
            // Stale lock check.
            let stale = fs::metadata(&lock_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|d| d.as_secs() > 720)
                .unwrap_or(false);
            if stale {
                let _ = fs::remove_file(&lock_path);
                // Recurse once to either win the next race or find the done marker.
                return resolve_or_seed_template(minimum);
            }
            thread::sleep(Duration::from_millis(500));
        }
        Err("template seeder did not complete within 700 s; \
             check target/ha-test-template for a stale lock"
            .to_string())
    }
}

/// Seed the template cg store into `data_dir`.  Runs vigil repeatedly until
/// `minimum` observations are present (up to 600 s total).  Mirrors
/// `first_light_loop.rs::observe()` exactly: 60 s per vigil run, exit on any
/// terminal signal including `motion_gate_suppressed_segment=true`.
fn seed_template_into(data_dir: &Path, minimum: usize) -> Result<(), String> {
    let root = workspace_root();
    let person_clip = root
        .join("tests")
        .join("fixtures")
        .join("video")
        .join("one-by-one-person-detection.mp4");
    let detector_artifact = root
        .join("tests")
        .join("fixtures")
        .join("models")
        .join("yolox-tiny-coco.pth");

    // Fail fast if required tools or fixtures are absent.
    tool_path("VIGIL_MEDIAMTX_BIN", "mediamtx")
        .ok_or_else(|| "mediamtx not available for template seeding".to_string())?;
    tool_path("VIGIL_FFMPEG_BIN", "ffmpeg")
        .ok_or_else(|| "ffmpeg not available for template seeding".to_string())?;
    if !person_clip.is_file() {
        return Err(format!(
            "person-detection clip not found: {}",
            person_clip.display()
        ));
    }
    if !detector_artifact.is_file() {
        return Err(format!(
            "detector model artifact not found: {}",
            detector_artifact.display()
        ));
    }

    fs::create_dir_all(data_dir).map_err(|e| format!("create data dir: {e}"))?;
    let store_path = data_dir.join("store.contextgraph");

    // Config written to a TempDir so it is cleaned up on drop.
    let config_tmp = tempfile::TempDir::new().map_err(|e| format!("seeder config tmp dir: {e}"))?;
    let config_path = config_tmp.path().join("vigil-seed.toml");
    let health_port = free_port()?;

    let rtsp = RtspFixture::start(&person_clip)?;

    let config = format!(
        "data_dir = \"{}\"\nstore_path = \"{}\"\nhealth_port = {}\n\
         site_name = \"{SITE_NAME}\"\ncamera_name = \"{CAMERA_NAME}\"\n\
         rtsp_url = \"{}\"\ndetector_model_id = \"{DETECTOR_MODEL_ID}\"\n\
         detector_model_path = \"{}\"\ndetector_confidence_threshold = {DETECTOR_THRESHOLD}\n",
        toml_path(data_dir),
        toml_path(&store_path),
        health_port,
        toml_string(&rtsp.url),
        toml_path(&detector_artifact),
    );
    fs::write(&config_path, &config).map_err(|e| format!("write seeder config: {e}"))?;

    // Drive vigil until `minimum` observations are present.
    // Each run mirrors first_light_loop.rs::observe(): 60 s max, exit on any
    // terminal signal (including motion_gate_suppressed_segment=true).
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let count = open_store_at(&store_path)
            .ok()
            .and_then(|s| s.list_observations(None).ok())
            .map(|v| v.len())
            .unwrap_or(0);
        if count >= minimum {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }

        let mut vigil = spawn_seeder_vigil(
            &config_path,
            data_dir,
            &store_path,
            &rtsp.url,
            &detector_artifact,
            health_port,
        );

        let run_start = Instant::now();
        loop {
            if runtime_reached_pipeline_terminal_signal(&vigil.logs()) {
                break;
            }
            if run_start.elapsed() >= Duration::from_secs(60) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        vigil.terminate();
        thread::sleep(Duration::from_millis(100));
    }

    let final_count = open_store_at(&store_path)
        .ok()
        .and_then(|s| s.list_observations(None).ok())
        .map(|v| v.len())
        .unwrap_or(0);
    if final_count < minimum {
        return Err(format!(
            "template seeder could not produce {minimum} observations in 600 s; \
             got {final_count} — check that the RTSP tools, detector model, and \
             person-detection clip are installed"
        ));
    }
    Ok(())
}

// ── Public: per-test store copy ────────────────────────────────────────────

/// Returns an isolated copy of the seeded template store for one test.
///
/// The returned `TempDir` owns the copy's lifetime — hold it until after all
/// assertions, then drop it.  The `PathBuf` is the store path inside that dir.
///
/// Seeding (via `seed_template`) happens once per test binary (OnceLock); each
/// call here just copies the already-seeded dir, which takes milliseconds.
pub fn fresh_store_copy(minimum_observations: usize) -> Result<(TempDir, PathBuf), String> {
    let template = template_data_dir().ok_or_else(|| {
        "seeded template store is not available; \
         check that mediamtx, ffmpeg, the person-detection clip, and the \
         detector model artifact are installed"
            .to_string()
    })?;

    let tmp = tempfile::TempDir::new().map_err(|e| format!("create per-test tmp dir: {e}"))?;
    let dest_data = tmp.path().join("data");
    copy_dir_recursive(template, &dest_data)?;
    let store_path = dest_data.join("store.contextgraph");

    // Verify the copy satisfies the caller's minimum.
    let store = open_store_at(&store_path)?;
    let count = store
        .list_observations(None)
        .map_err(|e| format!("list observations in store copy: {e}"))?
        .len();
    assert!(
        count >= minimum_observations,
        "seeded template has {count} observation(s) but this test requires \
         {minimum_observations}; the seeder did not produce enough observations"
    );

    Ok((tmp, store_path))
}
