//! The one test in `detection_accel_backend.rs`'s area that drives the real
//! compiled `vigil` binary (`runtime_detection_receipt_action_line_is_honest`)
//! — it lives with the composition root that produces that binary. Every
//! other detection-acceleration test exercises the selection/receipt seam
//! in-process and stays in `crates/vigil/tests/detection_accel_backend.rs`.

#![cfg(not(feature = "detect-burn-wgpu"))]

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[path = "../../vigil/tests/deterministic_fixture_support.rs"]
mod deterministic_fixture_support;
use deterministic_fixture_support::{
    assert_cpu_detection_action_in_rendered_block, vigil_binary_path,
};

const MODEL_ID: &str = "detector-under-test";

struct RuntimeProbe {
    child: Option<std::process::Child>,
    stdout: std::sync::Arc<std::sync::Mutex<String>>,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
}

impl RuntimeProbe {
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

    fn terminate(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
    }
}

impl Drop for RuntimeProbe {
    fn drop(&mut self) {
        self.terminate();
    }
}

enum RuntimeStatsAttempt {
    Stats(String),
    NoStats(String),
}

fn test_health_port_receipt(logs: &str) -> Option<u16> {
    logs.lines().find_map(|line| {
        line.strip_prefix("test_health_port_receipt=bound address=127.0.0.1 port=")?
            .parse()
            .ok()
    })
}

fn runtime_stats_text_from_real_runtime() -> Option<String> {
    let fixture_lock = deterministic_fixture_support::rtsp_fixture_lock().lock();
    assert!(
        fixture_lock.is_ok(),
        "RTSP fixture lock must be available for runtime stats probe"
    );
    let Ok(_fixture_lock) = fixture_lock else {
        return None;
    };

    let root = deterministic_fixture_support::workspace_root();
    let clip = root
        .join("tests")
        .join("fixtures")
        .join("video")
        .join("one-by-one-person-detection.mp4");
    assert!(
        clip.is_file(),
        "runtime stats probe needs the person-detection fixture at {}",
        clip.display()
    );
    let model = root
        .join("tests")
        .join("fixtures")
        .join("models")
        .join("yolox-tiny-coco.pth");
    assert!(
        model.is_file(),
        "runtime stats probe needs the detector model fixture at {}",
        model.display()
    );

    match runtime_stats_text_attempt(&clip, &model) {
        RuntimeStatsAttempt::Stats(stats) => Some(stats),
        RuntimeStatsAttempt::NoStats(logs) => {
            let published_stats = false;
            assert!(
                published_stats,
                "runtime must publish its test-gated health-port receipt and detection acceleration into public stats and /health; logs:\n{logs}"
            );
            None
        }
    }
}

fn runtime_stats_text_attempt(clip: &Path, model: &Path) -> RuntimeStatsAttempt {
    let temp = tempfile::tempdir();
    assert!(
        temp.is_ok(),
        "temp directory must be available for runtime stats probe"
    );
    let Ok(temp) = temp else {
        return RuntimeStatsAttempt::NoStats(String::new());
    };
    let data_dir = temp.path().join("data");
    let store_path = data_dir.join("store.contextgraph");
    let create_data_dir = fs::create_dir_all(&data_dir);
    assert!(
        create_data_dir.is_ok(),
        "runtime stats probe data dir must be writable"
    );
    let rtsp = deterministic_fixture_support::RtspFixture::start(clip);
    assert!(
        rtsp.is_ok(),
        "RTSP fixture must start for runtime stats probe"
    );
    let Ok(rtsp) = rtsp else {
        return RuntimeStatsAttempt::NoStats(String::new());
    };

    let config_path = temp.path().join("runtime-stats-probe.toml");
    let config = format!(
        "data_dir = \"{}\"\nstore_path = \"{}\"\nhealth_port = {}\nreview_port = {}\n\
         site_name = \"local test\"\ncamera_name = \"test camera\"\nrtsp_url = \"{}\"\n\
         detector_model_id = \"{}\"\ndetector_model_path = \"{}\"\ndetector_sample_frames = 1\n",
        deterministic_fixture_support::toml_path(&data_dir),
        deterministic_fixture_support::toml_path(&store_path),
        0,
        0,
        deterministic_fixture_support::toml_string(&rtsp.url),
        MODEL_ID,
        deterministic_fixture_support::toml_path(model),
    );
    let write_config = fs::write(&config_path, config);
    assert!(
        write_config.is_ok(),
        "runtime stats probe config must be writable"
    );

    let ungated = Command::new(vigil_binary_path())
        .arg("run")
        .arg("--config")
        .arg(&config_path)
        .env_remove("VIGIL_TEST_EPHEMERAL_HEALTH_PORT")
        .stdin(Stdio::null())
        .output();
    assert!(
        ungated.is_ok(),
        "ordinary runtime must execute the port-0 rejection probe"
    );
    let Ok(ungated) = ungated else {
        return RuntimeStatsAttempt::NoStats(String::new());
    };
    let ungated_logs = format!(
        "{}{}",
        String::from_utf8_lossy(&ungated.stdout),
        String::from_utf8_lossy(&ungated.stderr)
    );
    assert!(
        !ungated.status.success()
            && ungated_logs.contains(
                "health port 0 is reserved for VIGIL_TEST_EPHEMERAL_HEALTH_PORT=1 test probes"
            ),
        "ordinary production startup must reject an ungated ephemeral health port: {ungated_logs}"
    );

    let spawn = Command::new(vigil_binary_path())
        .arg("run")
        .arg("--config")
        .arg(&config_path)
        .env("VIGIL_RTSP_RETRY_INITIAL_MS", "200")
        .env("VIGIL_RTSP_RETRY_MAX_MS", "1000")
        .env("VIGIL_TEST_EPHEMERAL_HEALTH_PORT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    assert!(spawn.is_ok(), "vigil runtime must start for stats probe");
    let Ok(mut child) = spawn else {
        return RuntimeStatsAttempt::NoStats(String::new());
    };
    let stdout = deterministic_fixture_support::capture_pipe(child.stdout.take());
    let stderr = deterministic_fixture_support::capture_pipe(child.stderr.take());
    let mut runtime = RuntimeProbe {
        child: Some(child),
        stdout,
        stderr,
    };

    let health_port = match deterministic_fixture_support::wait_until(
        "test-gated health server to publish its actual bound port",
        Duration::from_secs(5),
        || Ok(test_health_port_receipt(&runtime.logs())),
    ) {
        Ok(port) => port,
        Err(error) => {
            let logs = runtime.logs();
            runtime.terminate();
            return RuntimeStatsAttempt::NoStats(format!("{error}; logs:\n{logs}"));
        }
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut latest_surfaces = None;
    while Instant::now() < deadline {
        let stats = read_public_stats(&data_dir);
        let health = read_health_endpoint(health_port);
        if let (Some(stats), Some(health)) = (stats, health) {
            let has_detection_stats = stats.contains("[detect.acceleration]");
            let has_detection_health = health.contains("[detect.acceleration]");
            if has_detection_stats && has_detection_health {
                latest_surfaces = Some(format!(
                    "[runtime.stats]\n{stats}\n[runtime.health]\n{health}"
                ));
                break;
            }
        }
        // A pipeline-terminal log line is not the readiness contract under
        // test. It can arrive between the two surface reads above and this log
        // snapshot, in which case returning those stale reads makes the test
        // race the synchronous receipt publication. Keep polling for the exact
        // public receipt and use the deadline only as the fail-closed bound.
        std::thread::sleep(Duration::from_millis(100));
    }
    let logs = runtime.logs();
    runtime.terminate();
    if let Some(surfaces) = latest_surfaces {
        RuntimeStatsAttempt::Stats(surfaces)
    } else {
        RuntimeStatsAttempt::NoStats(logs)
    }
}

fn read_public_stats(data_dir: &Path) -> Option<String> {
    let output = Command::new(vigil_binary_path())
        .arg("stats")
        .env("VIGIL_DATA_DIR", data_dir)
        .output();
    let Ok(output) = output else {
        return None;
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.status.success().then_some(text)
}

fn read_health_endpoint(port: u16) -> Option<String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(200)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    stream
        .write_all(b"GET /health HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

fn runtime_surface_section<'a>(surfaces: &'a str, marker: &str) -> &'a str {
    let Some(start) = surfaces.find(marker) else {
        return "";
    };
    let after = &surfaces[start + marker.len()..];
    if let Some(end) = after.find("\n[runtime.") {
        &after[..end]
    } else {
        after
    }
}

#[test]
fn runtime_detection_receipt_action_line_is_honest() {
    // Unfakeable because the runtime receipt producer must stop carrying its
    // own stale action line into the public stats and health surfaces.
    let Some(surfaces) = runtime_stats_text_from_real_runtime() else {
        return;
    };
    let stats = runtime_surface_section(&surfaces, "[runtime.stats]");
    let health = runtime_surface_section(&surfaces, "[runtime.health]");
    assert!(
        stats.contains("[detect.acceleration]"),
        "runtime stats must expose the rendered detection receipt block: {stats}"
    );
    assert!(
        stats.contains("detection-acceleration=") || stats.contains("work-receipt="),
        "runtime stats must carry the detection receipt through a public stats row: {stats}"
    );
    assert_cpu_detection_action_in_rendered_block(stats, "runtime stats");
    assert!(
        health.contains("[detect.acceleration]"),
        "runtime /health must expose the rendered detection receipt block because Home Assistant reads that surface: {surfaces}"
    );
    assert!(
        health.contains("detection-acceleration")
            || health.contains("work-receipt")
            || health.contains("backend_not_compiled"),
        "runtime /health must carry the detection receipt through the public health body: {health}"
    );
    assert_cpu_detection_action_in_rendered_block(health, "runtime health");
}
