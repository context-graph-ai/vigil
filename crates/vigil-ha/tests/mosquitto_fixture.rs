// A real, locally-spawned Mosquitto broker for MQTT integration tests.
// MQTT-specific: stays in this crate's test support (not core's shared
// support) — core's test builds should be as MQTT-free as its production
// builds. Reused cross-crate by `vigil-bin`'s own broker-backed product
// tests via `#[path]`, the same idiom `deterministic_fixture_support.rs`
// uses for its own cross-crate reuse.

#![allow(dead_code)]

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use vigil_ha::MqttConfig;

// Declared here too (not just by every including file) so this file compiles
// standalone — Cargo auto-discovers every `.rs` directly under `tests/` as
// its own test binary target regardless of `#[path]` inclusion elsewhere.
// Pulled from the context-graph-FREE `deterministic_test_support.rs` (not
// `deterministic_fixture_support.rs`, which needs context-graph) — this
// fixture must never pull that dependency in.
#[path = "../../vigil/tests/deterministic_test_support.rs"]
mod deterministic_test_support;
use deterministic_test_support::{capture_pipe, wait_until};

#[path = "mqtt_test_probe.rs"]
mod mqtt_test_probe;
use mqtt_test_probe::{MqttProbe, required_tool};

pub struct MosquittoFixture {
    pub host: String,
    pub port: u16,
    pub child: Child,
    _stdout: Arc<Mutex<String>>,
    _stderr: Arc<Mutex<String>>,
    _config: File,
    _exclusive: MutexGuard<'static, ()>,
}

static MOSQUITTO_FIXTURE_LOCK: Mutex<()> = Mutex::new(());
const MOSQUITTO_FIXTURE_PORT: u16 = 41_883;

impl MosquittoFixture {
    pub fn start() -> Result<Self, String> {
        let exclusive = MOSQUITTO_FIXTURE_LOCK
            .lock()
            .map_err(|_| "Mosquitto fixture exclusivity lock was poisoned".to_string())?;
        let mosquitto_bin = required_tool(
            "VIGIL_MOSQUITTO_BIN",
            "mosquitto",
            &[Path::new("/usr/sbin/mosquitto")],
        )?;
        let host = process_unique_loopback()?.to_string();
        let port = MOSQUITTO_FIXTURE_PORT;
        let config = mosquitto_memfd_config(&host, port)?;
        let config_path = format!("/proc/self/fd/{}", config.as_raw_fd());
        let mut child = Command::new(&mosquitto_bin)
            // A memfd is a real file for Mosquitto but never traverses an
            // AppArmor-sensitive temporary path. With CLOEXEC deliberately
            // absent, the broker inherits this exact immutable config file.
            .arg("-c")
            .arg(config_path)
            .arg("-v")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn mosquitto: {e}"))?;
        let stdout = capture_pipe(child.stdout.take());
        let stderr = capture_pipe(child.stderr.take());
        let started = wait_until("Mosquitto TCP listener", Duration::from_secs(5), || {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| format!("inspect Mosquitto: {e}"))?
            {
                return Err(format!("Mosquitto exited early with {status}"));
            }
            Ok(TcpStream::connect((host.as_str(), port)).ok().map(|_| ()))
        });
        let ready = started
            .and_then(|()| MqttProbe::connect(&host, port, Duration::from_secs(2)).map(|_| ()));
        if ready.is_ok() && child.try_wait().map_err(|e| e.to_string())?.is_none() {
            return Ok(Self {
                host,
                port,
                child,
                _stdout: stdout,
                _stderr: stderr,
                _config: config,
                _exclusive: exclusive,
            });
        }
        let _ = child.kill();
        let status = child.wait().ok();
        let _ = wait_until(
            "Mosquitto stdout/stderr capture to drain",
            Duration::from_millis(250),
            || {
                let has_output = stdout.lock().is_ok_and(|value| !value.is_empty())
                    || stderr.lock().is_ok_and(|value| !value.is_empty());
                Ok(has_output.then_some(()))
            },
        );
        let out = stdout.lock().map(|v| v.clone()).unwrap_or_default();
        let err = stderr.lock().map(|v| v.clone()).unwrap_or_default();
        let detail = ready
            .err()
            .unwrap_or_else(|| format!("exited after readiness with {status:?}"));
        Err(format!(
            "single causally exclusive Mosquitto startup failed at {host}:{port}: {detail}; stdout={out:?}; stderr={err:?}"
        ))
    }

    pub fn mqtt_config(&self) -> MqttConfig {
        MqttConfig {
            broker_host: self.host.clone(),
            broker_port: self.port,
            username: None,
            password: None,
        }
    }
}

fn process_unique_loopback() -> Result<Ipv4Addr, String> {
    let pid = std::process::id();
    let host_id = pid
        .checked_add(0x40_0000)
        .filter(|value| *value <= 0xff_ffff)
        .ok_or_else(|| format!("process id {pid} cannot map injectively into 127/8"))?;
    Ok(Ipv4Addr::new(
        127,
        ((host_id >> 16) & 0xff) as u8,
        ((host_id >> 8) & 0xff) as u8,
        (host_id & 0xff) as u8,
    ))
}

fn mosquitto_memfd_config(host: &str, port: u16) -> Result<File, String> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: the C string points to immutable storage for the duration of
        // the call. Flags 0 intentionally leaves CLOEXEC off so Mosquitto can
        // open /proc/self/fd/<fd> after exec.
        let fd = unsafe { libc::memfd_create(c"vigil-mosquitto-config".as_ptr(), 0) };
        if fd < 0 {
            return Err(format!(
                "create Mosquitto memfd config: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: memfd_create returned a new owned descriptor on success.
        let mut file = unsafe { File::from_raw_fd(fd) };
        write!(
            file,
            "listener {port} {host}\nallow_anonymous true\npersistence false\n"
        )
        .map_err(|error| format!("write Mosquitto memfd config: {error}"))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| format!("rewind Mosquitto memfd config: {error}"))?;
        Ok(file)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (host, port);
        Err("real-broker Mosquitto fixture requires Linux memfd_create".to_string())
    }
}

impl Drop for MosquittoFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
