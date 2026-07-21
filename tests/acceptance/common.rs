use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use context_graph::{EmbedderConfig, Store, StoreConfig};
use tempfile::TempDir;

pub(crate) struct VigilBinary {
    path: PathBuf,
}

impl VigilBinary {
    pub(crate) fn new() -> Self {
        if let Some(path) = option_env!("CARGO_BIN_EXE_vigil") {
            return Self {
                path: PathBuf::from(path),
            };
        }
        if let Some(path) = std::env::var_os("CARGO_BIN_EXE_vigil") {
            return Self {
                path: PathBuf::from(path),
            };
        }
        let path = workspace_root()
            .join("target")
            .join("debug")
            .join(binary_name("vigil"));
        ensure_vigil_binary();
        Self { path }
    }

    pub(crate) fn command(&self) -> Command {
        Command::new(&self.path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) struct VigilProcess {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    port: u16,
    trace_prefix: Option<PathBuf>,
    _trace_dir: Option<TempDir>,
    _isolation_dir: TempDir,
}

impl VigilProcess {
    pub(crate) fn spawn(
        binary: &VigilBinary,
        config_path: &Path,
        health_port: u16,
    ) -> std::io::Result<Self> {
        Self::spawn_inner(binary, config_path, health_port, false)
    }

    pub(crate) fn spawn_network_traced(
        binary: &VigilBinary,
        config_path: &Path,
        health_port: u16,
    ) -> std::io::Result<Self> {
        Self::spawn_inner(binary, config_path, health_port, true)
    }

    fn spawn_inner(
        binary: &VigilBinary,
        config_path: &Path,
        health_port: u16,
        trace_network: bool,
    ) -> std::io::Result<Self> {
        let isolation_dir = tempfile::tempdir()?;
        let trace_dir = if trace_network {
            Some(tempfile::tempdir()?)
        } else {
            None
        };
        let trace_prefix = trace_dir.as_ref().map(|dir| dir.path().join("network"));
        let mut command = if let Some(prefix) = trace_prefix.as_ref() {
            let mut command = Command::new("setsid");
            command
                .arg("strace")
                .arg("-ff")
                .arg("-e")
                .arg("trace=network")
                .arg("-o")
                .arg(prefix)
                .arg(binary.path());
            command
        } else {
            binary.command()
        };
        let mut child = command
            .arg("run")
            .arg("--config")
            .arg(config_path)
            .env("VIGIL_HEALTH_PORT", health_port.to_string())
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("HOME", isolation_dir.path())
            .env("XDG_CACHE_HOME", isolation_dir.path().join("cache"))
            .env("XDG_CONFIG_HOME", isolation_dir.path().join("config"))
            .env("HF_HOME", isolation_dir.path().join("hf-home"))
            .env(
                "TRANSFORMERS_CACHE",
                isolation_dir.path().join("transformers"),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if child.id() <= 1 {
            let invalid_pid = child.id();
            let _ = kill_tracked_child_with_timeout(
                &mut child,
                "invalid spawned vigil child",
                Duration::from_secs(1),
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("spawned process reported invalid child pid {invalid_pid}"),
            ));
        }
        let stdout = capture_pipe(child.stdout.take());
        let stderr = capture_pipe(child.stderr.take());
        Ok(Self {
            child,
            stdout,
            stderr,
            port: health_port,
            trace_prefix,
            _trace_dir: trace_dir,
            _isolation_dir: isolation_dir,
        })
    }

    pub(crate) fn health(&self) -> HealthProbe {
        HealthProbe::new(self.port)
    }

    pub(crate) fn logs(&self) -> String {
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

    pub(crate) fn wait_for_log(&self, needle: &str, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.logs().contains(needle) {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
    }

    pub(crate) fn terminate(&mut self) -> Option<ExitStatus> {
        terminate_tracked_child_with_timeout(
            &mut self.child,
            "vigil acceptance child",
            Duration::from_secs(10),
        )
    }

    pub(crate) fn kill9(&mut self) -> Option<ExitStatus> {
        kill_tracked_child_with_timeout(
            &mut self.child,
            "vigil acceptance child hard stop",
            Duration::from_secs(10),
        )
    }

    pub(crate) fn network_trace(&self) -> NetworkTrace {
        let Some(prefix) = self.trace_prefix.as_ref() else {
            return NetworkTrace {
                tool_available: true,
                outbound_attempts: Vec::new(),
                raw: String::new(),
            };
        };
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
        let outbound_attempts = raw
            .lines()
            .filter(|line| network_line_is_outbound(line))
            .map(str::to_string)
            .collect();
        NetworkTrace {
            tool_available: command_status("strace", ["-V"])
                && command_status("setsid", ["--version"]),
            outbound_attempts,
            raw,
        }
    }

    pub(crate) fn runtime_pid(&self) -> Option<u32> {
        if self.trace_prefix.is_none() {
            return Some(self.child.id());
        }
        descendant_pids(self.child.id())
            .into_iter()
            .find(|pid| process_comm(*pid).as_deref() == Some("vigil"))
    }
}

impl Drop for VigilProcess {
    fn drop(&mut self) {
        let _ = kill_tracked_child_with_timeout(
            &mut self.child,
            "vigil acceptance drop cleanup",
            Duration::from_secs(10),
        );
    }
}

pub(crate) struct HealthProbe {
    address: SocketAddr,
}

impl HealthProbe {
    pub(crate) fn new(port: u16) -> Self {
        Self {
            address: SocketAddr::from(([127, 0, 0, 1], port)),
        }
    }

    pub(crate) fn status(&self) -> Option<u16> {
        let mut stream =
            TcpStream::connect_timeout(&self.address, Duration::from_millis(200)).ok()?;
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = stream.write_all(b"GET /health HTTP/1.1\r\nhost: localhost\r\n\r\n");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|status| status.parse::<u16>().ok())
    }

    pub(crate) fn wait_for_status(&self, expected: u16, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.status() == Some(expected) {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
    }

    pub(crate) fn listener_owned_by_pid(&self, pid: u32) -> HealthOwnerProbeResult {
        let inodes = listening_socket_inodes(self.address.port());
        if inodes.is_empty() {
            return HealthOwnerProbeResult {
                owned: false,
                detail: format!(
                    "no listening socket inode found for health port {}",
                    self.address.port()
                ),
            };
        }
        let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
        let Ok(entries) = fs::read_dir(&fd_dir) else {
            return HealthOwnerProbeResult {
                owned: false,
                detail: format!(
                    "could not inspect process fd directory {} for health listener ownership",
                    fd_dir.display()
                ),
            };
        };
        for entry in entries.flatten() {
            if let Ok(target) = fs::read_link(entry.path()) {
                let target = target.to_string_lossy();
                if inodes
                    .iter()
                    .any(|inode| target.as_ref() == format!("socket:[{inode}]"))
                {
                    return HealthOwnerProbeResult {
                        owned: true,
                        detail: format!(
                            "health port {} listener is owned by pid {pid}",
                            self.address.port()
                        ),
                    };
                }
            }
        }
        HealthOwnerProbeResult {
            owned: false,
            detail: format!(
                "health port {} listener socket inodes {inodes:?} were not owned by pid {pid}",
                self.address.port()
            ),
        }
    }
}

pub(crate) struct StoreProbe {
    path: PathBuf,
}

impl StoreProbe {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn open_existing(&self) -> StoreProbeResult {
        if !self.path.exists() {
            return StoreProbeResult {
                opened: false,
                detail: format!("store path does not exist: {}", self.path.display()),
            };
        }
        let config = StoreConfig {
            db_path: self.path.clone(),
            default_text_embedder: Some(EmbedderConfig::disabled()),
            ..StoreConfig::default()
        };
        match Store::open(config) {
            Ok(store) => StoreProbeResult {
                opened: true,
                detail: format!("opened {}", store.db_path().display()),
            },
            Err(error) => StoreProbeResult {
                opened: false,
                detail: format!("store open failed for {}: {error}", self.path.display()),
            },
        }
    }

    pub(crate) fn identity(&self) -> Option<StoreIdentity> {
        let metadata = fs::metadata(&self.path).ok()?;
        Some(StoreIdentity {
            path: self.path.clone(),
            len: metadata.len(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
        })
    }

    pub(crate) fn live_lock_held(&self) -> StoreLockProbeResult {
        let lock_path = self.path.with_extension("lock");
        if !lock_path.exists() {
            return StoreLockProbeResult {
                locked: false,
                detail: format!("store lock file does not exist: {}", lock_path.display()),
            };
        }
        match Command::new("flock")
            .arg("-n")
            .arg(&lock_path)
            .arg("true")
            .output()
        {
            Ok(output) if output.status.success() => StoreLockProbeResult {
                locked: false,
                detail: format!("store lock was not held: {}", lock_path.display()),
            },
            Ok(output) => StoreLockProbeResult {
                locked: true,
                detail: format!("store lock was held; flock exited with {}", output.status),
            },
            Err(error) => StoreLockProbeResult {
                locked: false,
                detail: format!("could not run flock for {}: {error}", lock_path.display()),
            },
        }
    }

    pub(crate) fn live_database_file_open(&self, pid: u32) -> StoreFileProbeResult {
        let Ok(expected_metadata) = fs::metadata(&self.path) else {
            return StoreFileProbeResult {
                open: false,
                detail: format!("store path was not statable: {}", self.path.display()),
            };
        };
        let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
        let Ok(entries) = fs::read_dir(&fd_dir) else {
            return StoreFileProbeResult {
                open: false,
                detail: format!(
                    "could not inspect process fd directory {}",
                    fd_dir.display()
                ),
            };
        };
        for entry in entries.flatten() {
            #[cfg(unix)]
            if let Ok(metadata) = fs::metadata(entry.path())
                && metadata.dev() == expected_metadata.dev()
                && metadata.ino() == expected_metadata.ino()
            {
                return StoreFileProbeResult {
                    open: true,
                    detail: format!("store database file is open: {}", self.path.display()),
                };
            }
            #[cfg(not(unix))]
            if let Ok(target) = fs::read_link(entry.path())
                && target == self.path
            {
                return StoreFileProbeResult {
                    open: true,
                    detail: format!("store database file is open: {}", self.path.display()),
                };
            }
        }
        StoreFileProbeResult {
            open: false,
            detail: format!(
                "process {pid} did not have the store database file open: {}",
                self.path.display()
            ),
        }
    }

    pub(crate) fn public_open_blocked_by_process(&self, pid: u32) -> StoreContentionProbeResult {
        let config = StoreConfig {
            db_path: self.path.clone(),
            default_text_embedder: Some(EmbedderConfig::disabled()),
            ..StoreConfig::default()
        };
        match Store::open(config) {
            Ok(store) => StoreContentionProbeResult {
                blocked: false,
                detail: format!(
                    "public Store::open unexpectedly succeeded while process {pid} should hold {}",
                    store.db_path().display()
                ),
            },
            Err(error) => {
                // Upstream context-graph (cg dev 99ea2d3) surfaces a locked
                // store as the typed `CgError::StoreLocked { holder_pid, path }`,
                // whose Debug no longer carries the engine's "database is
                // locked … pid N" wording. Classify by the typed variant
                // (confirming the holder is exactly the live process), the same
                // move vigil's own store-open surfaces make, rather than
                // substring-matching Debug text that rots when the format
                // shifts. Assertion INTENT is unchanged — the store is still
                // required to be contended by THIS pid.
                let blocked = matches!(
                    &error,
                    context_graph::CgError::StoreLocked { holder_pid, .. } if *holder_pid == pid
                );
                StoreContentionProbeResult {
                    blocked,
                    detail: format!("public Store::open contention result: {error:?}"),
                }
            }
        }
    }
}

pub(crate) struct StoreProbeResult {
    pub(crate) opened: bool,
    pub(crate) detail: String,
}

pub(crate) struct StoreLockProbeResult {
    pub(crate) locked: bool,
    pub(crate) detail: String,
}

pub(crate) struct StoreFileProbeResult {
    pub(crate) open: bool,
    pub(crate) detail: String,
}

pub(crate) struct StoreContentionProbeResult {
    pub(crate) blocked: bool,
    pub(crate) detail: String,
}

pub(crate) struct HealthOwnerProbeResult {
    pub(crate) owned: bool,
    pub(crate) detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoreIdentity {
    pub(crate) path: PathBuf,
    pub(crate) len: u64,
    #[cfg(unix)]
    pub(crate) dev: u64,
    #[cfg(unix)]
    pub(crate) ino: u64,
}

pub(crate) struct DockerProbe;

impl DockerProbe {
    pub(crate) fn image_healthcheck(image: &str) -> DockerObservation {
        let build = Self::build_current_image(image);
        if !build.command_succeeded {
            return build;
        }
        let inspect = command_output(
            "docker",
            [
                "image",
                "inspect",
                image,
                "--format",
                "{{json .Config.Healthcheck}}",
            ],
        );
        DockerObservation {
            docker_available: true,
            command_succeeded: inspect
                .as_ref()
                .map(|output| output.status.success())
                .unwrap_or(false),
            stdout: build.stdout,
            container_logs: String::new(),
            healthcheck: inspect.map(output_combined_text).unwrap_or_default(),
        }
    }

    pub(crate) fn run_volume_probe(image: &str, volume: &Path) -> DockerObservation {
        Self::run_detached_probe(image, volume, Some(free_port()), false)
    }

    pub(crate) fn run_network_none_probe(image: &str, volume: &Path) -> DockerObservation {
        Self::run_detached_probe(image, volume, None, true)
    }

    fn run_detached_probe(
        image: &str,
        volume: &Path,
        host_port: Option<u16>,
        network_none: bool,
    ) -> DockerObservation {
        let build = Self::build_current_image(image);
        if !build.command_succeeded {
            return build;
        }
        let name = unique_container_name();
        let volume_arg = format!("{}:/data", volume.display());
        #[cfg(unix)]
        let user_arg = format!("{}:{}", unsafe { libc::getuid() }, unsafe {
            libc::getgid()
        });
        let mut args = vec![
            "run",
            "-d",
            "--pull",
            "never",
            "--name",
            &name,
            "-e",
            "VIGIL_STORE_PATH=/data/store.contextgraph",
            "-e",
            "VIGIL_HEALTH_PORT=8099",
            "-e",
            "VIGIL_DROP_PRIVILEGES=0",
            "-v",
            &volume_arg,
        ];
        #[cfg(unix)]
        args.extend(["--user", &user_arg]);
        let port_arg;
        if network_none {
            args.extend(["--network", "none"]);
        } else if let Some(port) = host_port {
            port_arg = format!("127.0.0.1:{port}:8099");
            args.extend(["-p", &port_arg]);
        }
        args.extend([image, "run"]);

        let _ = command_output("docker", ["rm", "-f", &name]);
        let mut output = build.stdout;
        let mut container_logs = String::new();
        let started = command_output("docker", args)
            .map(|run| {
                let success = run.status.success();
                output.push_str(&output_combined_text(run));
                success
            })
            .unwrap_or(false);
        if !started {
            let _ = command_output("docker", ["rm", "-f", &name]);
            return DockerObservation {
                docker_available: true,
                command_succeeded: false,
                stdout: output,
                container_logs,
                healthcheck: String::new(),
            };
        }

        let ready = if network_none {
            wait_for_docker_health(&name, Duration::from_secs(30), &mut output)
        } else {
            host_port
                .map(|port| HealthProbe::new(port).wait_for_status(200, Duration::from_secs(30)))
                .unwrap_or(false)
        };
        let live_runtime =
            ready && verify_container_live_store_and_health(&name, volume, &mut output);
        if let Some(logs) = command_output("docker", ["logs", &name]) {
            let logs = output_combined_text(logs);
            container_logs.push_str(&logs);
            output.push_str(&logs);
        }
        let stopped = command_output("docker", ["stop", "--time", "10", &name])
            .map(|stop| {
                let success = stop.status.success();
                output.push_str(&output_combined_text(stop));
                success
            })
            .unwrap_or(false);
        let clean_exit = inspect_clean_container_exit(&name, &mut output);
        let _ = command_output("docker", ["rm", "-f", &name]);

        DockerObservation {
            docker_available: true,
            command_succeeded: ready && live_runtime && stopped && clean_exit,
            stdout: output,
            container_logs,
            healthcheck: String::new(),
        }
    }

    fn build_current_image(image: &str) -> DockerObservation {
        static BUILDS: OnceLock<Mutex<BTreeMap<String, DockerObservation>>> = OnceLock::new();
        let builds = BUILDS.get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut builds = builds
            .lock()
            .expect("Docker image build cache lock poisoned");
        if let Some(observation) = builds.get(image) {
            return observation.clone();
        }
        let observation = Self::build_current_image_uncached(image);
        builds.insert(image.to_string(), observation.clone());
        observation
    }

    fn build_current_image_uncached(image: &str) -> DockerObservation {
        if !docker_available() {
            return DockerObservation::unavailable();
        }
        let mut stdout = String::new();
        let (target, docker_arch) = if cfg!(target_arch = "x86_64") {
            ("x86_64-unknown-linux-musl", "amd64")
        } else if cfg!(target_arch = "aarch64") {
            ("aarch64-unknown-linux-musl", "arm64")
        } else {
            return DockerObservation {
                docker_available: true,
                command_succeeded: false,
                stdout: format!(
                    "container acceptance has no static image target for host architecture {}\n",
                    std::env::consts::ARCH
                ),
                container_logs: String::new(),
                healthcheck: String::new(),
            };
        };
        let cargo_args = [
            "build",
            "-p",
            "vigil",
            "--release",
            "--target",
            target,
            "--features",
            "fabric",
        ];
        let Some(build) = command_output_in_dir(env!("CARGO"), cargo_args, &workspace_root())
        else {
            return DockerObservation {
                docker_available: true,
                command_succeeded: false,
                stdout: "could not run cargo build before docker build\n".to_string(),
                container_logs: String::new(),
                healthcheck: String::new(),
            };
        };
        let success = build.status.success();
        stdout.push_str(&output_combined_text(build));
        if !success {
            return DockerObservation {
                docker_available: true,
                command_succeeded: false,
                stdout,
                container_logs: String::new(),
                healthcheck: String::new(),
            };
        }

        let context = match tempfile::tempdir() {
            Ok(context) => context,
            Err(error) => {
                stdout.push_str(&format!("create minimal Docker context: {error}\n"));
                return DockerObservation {
                    docker_available: true,
                    command_succeeded: false,
                    stdout,
                    container_logs: String::new(),
                    healthcheck: String::new(),
                };
            }
        };
        let staged_dir = context.path().join("dist/docker").join(docker_arch);
        let source_binary = workspace_root()
            .join("target")
            .join(target)
            .join("release")
            .join(binary_name("vigil"));
        let staged = fs::create_dir_all(&staged_dir)
            .and_then(|()| {
                fs::copy(
                    workspace_root().join("Dockerfile"),
                    context.path().join("Dockerfile"),
                )
            })
            .and_then(|_| fs::copy(&source_binary, staged_dir.join("vigil")));
        if let Err(error) = staged {
            stdout.push_str(&format!(
                "stage exact Docker context from {}: {error}\n",
                source_binary.display()
            ));
            return DockerObservation {
                docker_available: true,
                command_succeeded: false,
                stdout,
                container_logs: String::new(),
                healthcheck: String::new(),
            };
        }
        let build = Command::new("docker")
            .env("DOCKER_BUILDKIT", "0")
            .args(["build", "--pull=false", "--build-arg"])
            .arg(format!("TARGETARCH={docker_arch}"))
            .args(["-t", image])
            .arg(context.path())
            .output()
            .ok();
        if let Some(output) = build.as_ref() {
            stdout.push_str(&output_combined_text_ref(output));
        }
        DockerObservation {
            docker_available: true,
            command_succeeded: build
                .as_ref()
                .map(|output| output.status.success())
                .unwrap_or(false),
            stdout,
            container_logs: String::new(),
            healthcheck: String::new(),
        }
    }
}

fn verify_container_live_store_and_health(name: &str, volume: &Path, output: &mut String) -> bool {
    let Some(pid_output) =
        command_output("docker", ["inspect", "--format", "{{.State.Pid}}", name])
    else {
        output.push_str("container pid inspect failed\n");
        return false;
    };
    let pid_text = output_combined_text_ref(&pid_output);
    output.push_str(&pid_text);
    if !pid_output.status.success() {
        return false;
    }
    let Some(pid) = pid_text.trim().parse::<u32>().ok().filter(|pid| *pid != 0) else {
        output.push_str("container pid was not a nonzero integer\n");
        return false;
    };
    let store_path = volume.join("store.contextgraph");
    let mut ok = true;
    let live_lock = StoreProbe::new(&store_path).live_lock_held();
    if !live_lock.locked {
        output.push_str(&live_lock.detail);
        output.push('\n');
        ok = false;
    }
    let live_file = StoreProbe::new(&store_path).live_database_file_open(pid);
    if !live_file.open {
        output.push_str(&live_file.detail);
        output.push('\n');
        ok = false;
    }
    let contention = StoreProbe::new(&store_path).public_open_blocked_by_process(pid);
    if !contention.blocked {
        output.push_str(&contention.detail);
        output.push('\n');
        ok = false;
    }
    let health_owner = listener_owned_by_pid_in_namespace(pid, 8099);
    if !health_owner.owned {
        output.push_str(&health_owner.detail);
        output.push('\n');
        ok = false;
    }
    ok
}

#[derive(Clone)]
pub(crate) struct DockerObservation {
    pub(crate) docker_available: bool,
    pub(crate) command_succeeded: bool,
    pub(crate) stdout: String,
    /// Output emitted by the running container only. Build diagnostics stay
    /// in `stdout` for failure reporting but cannot satisfy or fail runtime
    /// isolation assertions.
    pub(crate) container_logs: String,
    pub(crate) healthcheck: String,
}

impl DockerObservation {
    pub(crate) fn healthcheck_targets_health_endpoint(&self) -> bool {
        let metadata = self.healthcheck.to_ascii_lowercase();
        let exec_curl = metadata.contains(r#""test":["cmd","curl""#);
        let exec_wget =
            metadata.contains(r#""test":["cmd","wget""#) || metadata.contains(r#","wget","#);
        let target = metadata.contains("/health")
            && (metadata.contains("127.0.0.1") || metadata.contains("localhost"));
        let shell_or_noop = metadata.contains("cmd-shell")
            || metadata.contains("echo")
            || metadata.contains(r#""true""#)
            || metadata.contains("#");
        target && (exec_curl || exec_wget) && !shell_or_noop
    }

    fn unavailable() -> Self {
        Self {
            docker_available: false,
            command_succeeded: false,
            stdout: String::new(),
            container_logs: String::new(),
            healthcheck: String::new(),
        }
    }
}

pub(crate) struct NetworkBlock;

impl NetworkBlock {
    pub(crate) fn outbound() -> Self {
        Self
    }
}

pub(crate) struct NetworkTrace {
    pub(crate) tool_available: bool,
    pub(crate) outbound_attempts: Vec<String>,
    pub(crate) raw: String,
}

pub(crate) struct RssProbe {
    pid: u32,
}

impl RssProbe {
    pub(crate) fn new(pid: u32) -> Self {
        Self { pid }
    }

    pub(crate) fn sample_kb(&self) -> Option<u64> {
        let status = fs::read_to_string(format!("/proc/{}/status", self.pid)).ok()?;
        status.lines().find_map(|line| {
            line.strip_prefix("VmRSS:").and_then(|value| {
                value
                    .split_whitespace()
                    .next()
                    .and_then(|kb| kb.parse::<u64>().ok())
            })
        })
    }
}

pub(crate) fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub(crate) fn temp_config(
    data_dir: &Path,
    store_path: &Path,
    toml_health_port: u16,
) -> std::io::Result<(TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("vigil.toml");
    let config = format!(
        "data_dir = \"{}\"\nstore_path = \"{}\"\nhealth_port = {}\n",
        escape_toml_path(data_dir),
        escape_toml_path(store_path),
        toml_health_port
    );
    fs::write(&config_path, config)?;
    Ok((dir, config_path))
}

pub(crate) fn free_port() -> u16 {
    static ALLOCATED_PORTS: OnceLock<Mutex<BTreeSet<u16>>> = OnceLock::new();

    let allocated = ALLOCATED_PORTS.get_or_init(|| Mutex::new(BTreeSet::new()));
    for _ in 0..128 {
        let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
            continue;
        };
        let Ok(address) = listener.local_addr() else {
            continue;
        };
        let port = address.port();
        if allocated
            .lock()
            .map(|mut ports| ports.insert(port))
            .unwrap_or(false)
        {
            return port;
        }
    }
    panic!("could not allocate a unique test health port")
}

pub(crate) fn output_text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

pub(crate) fn pid_of(process: &VigilProcess) -> u32 {
    let pid = process.runtime_pid().unwrap_or_else(|| process.child.id());
    assert!(
        pid > 1,
        "acceptance harness resolved invalid process pid {pid}"
    );
    pid
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

pub(crate) fn kill_tracked_child(child: &mut Child, label: &str) -> Option<ExitStatus> {
    kill_tracked_child_with_timeout(child, label, Duration::from_secs(10))
}

fn terminate_tracked_child_with_timeout(
    child: &mut Child,
    label: &str,
    timeout: Duration,
) -> Option<ExitStatus> {
    let pid = child.id();
    match child.try_wait() {
        Ok(Some(status)) => return Some(status),
        Ok(None) => {}
        Err(error) => {
            eprintln!("acceptance harness could not read child status for {label}: {error}");
            return None;
        }
    }

    if signal_tracked_child_tree(pid, libc::SIGTERM, label) {
        wait_for_child_exit(child, timeout)
            .or_else(|| kill_tracked_child_with_timeout(child, label, timeout))
    } else {
        kill_tracked_child_with_timeout(child, label, timeout)
    }
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
            eprintln!("acceptance harness could not read child status for {label}: {error}");
            return None;
        }
    }

    if !signal_tracked_child_tree(pid, libc::SIGKILL, label) {
        return child.try_wait().ok().flatten();
    }
    wait_for_child_exit(child, timeout)
}

fn signal_tracked_child_tree(root_pid: u32, signal: libc::c_int, label: &str) -> bool {
    if let Err(error) = direct_child_target_is_safe(root_pid) {
        eprintln!("acceptance harness refused tracked tree cleanup root for {label}: {error}");
        return false;
    }
    let descendants = descendant_pids(root_pid);
    for descendant in descendants.into_iter().rev() {
        let _ = signal_descendant(root_pid, descendant, signal, label);
    }
    signal_direct_child(root_pid, signal, label)
}

fn signal_direct_child(pid: u32, signal: libc::c_int, label: &str) -> bool {
    if signal != libc::SIGTERM && signal != libc::SIGKILL {
        eprintln!("acceptance harness refused unsupported cleanup signal {signal} for {label}");
        return false;
    }
    if let Err(error) = direct_child_target_is_safe(pid) {
        eprintln!("acceptance harness refused direct process cleanup target for {label}: {error}");
        return false;
    }

    let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if result == 0 {
        return true;
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return true;
    }
    eprintln!(
        "acceptance harness direct child signal {signal} failed for {label} pid {pid}: {error}"
    );
    false
}

fn signal_descendant(root_pid: u32, pid: u32, signal: libc::c_int, label: &str) -> bool {
    if signal != libc::SIGTERM && signal != libc::SIGKILL {
        eprintln!("acceptance harness refused unsupported cleanup signal {signal} for {label}");
        return false;
    }
    if let Err(error) = descendant_target_is_safe(root_pid, pid) {
        eprintln!("acceptance harness refused descendant cleanup target for {label}: {error}");
        return false;
    }

    let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if result == 0 {
        return true;
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return true;
    }
    eprintln!(
        "acceptance harness descendant signal {signal} failed for {label} pid {pid}: {error}"
    );
    false
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
            "pid {pid} is not a tracked direct child of this acceptance process; parent pid is {ppid}, expected {current_pid}"
        ));
    }

    Ok(())
}

fn descendant_target_is_safe(root_pid: u32, pid: u32) -> Result<(), String> {
    if pid <= 1 {
        return Err(format!(
            "invalid descendant pid {pid}; direct kill would target the current process group or init"
        ));
    }
    direct_child_target_is_safe(root_pid)?;
    if !descendant_pids(root_pid)
        .into_iter()
        .any(|descendant| descendant == pid)
    {
        return Err(format!(
            "pid {pid} is not a descendant of tracked child pid {root_pid}"
        ));
    }
    Ok(())
}

fn process_parent_id(pid: u32) -> Option<u32> {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| parse_stat_ppid(&stat))
}

fn descendant_pids(root: u32) -> Vec<u32> {
    let mut descendants = Vec::new();
    let mut queue = vec![root];
    while let Some(parent) = queue.pop() {
        for child in child_pids(parent) {
            descendants.push(child);
            queue.push(child);
        }
    }
    descendants
}

fn child_pids(parent: u32) -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let ppid = parse_stat_ppid(&stat)?;
            (ppid == parent).then_some(pid)
        })
        .collect()
}

fn process_comm(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|value| value.trim().to_string())
}

fn parse_stat_ppid(stat: &str) -> Option<u32> {
    let close = stat.rfind(") ")?;
    stat[close + 2..].split_whitespace().nth(1)?.parse().ok()
}

fn ensure_vigil_binary() {
    static BUILD_ONCE: OnceLock<()> = OnceLock::new();
    BUILD_ONCE.get_or_init(|| {
        let status = Command::new(env!("CARGO"))
            .current_dir(workspace_root())
            .args(["build", "-p", "vigil", "--bin", "vigil"])
            .status()
            .expect("cargo build -p vigil --bin vigil should run");
        assert!(status.success(), "cargo build -p vigil --bin vigil failed");
    });
}

fn command_status<I, S>(program: &str, args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn docker_available() -> bool {
    command_status("docker", ["version", "--format", "{{.Server.Version}}"])
}

fn command_output<I, S>(program: &str, args: I) -> Option<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(program).args(args).output().ok()
}

fn command_output_in_dir<I, S>(program: &str, args: I, dir: &Path) -> Option<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(program)
        .current_dir(dir)
        .args(args)
        .output()
        .ok()
}

fn output_combined_text(output: Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("{stdout}{stderr}")
}

fn output_combined_text_ref(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("{stdout}{stderr}")
}

fn listening_socket_inodes(port: u16) -> Vec<String> {
    let mut inodes = Vec::new();
    collect_listening_socket_inodes(Path::new("/proc/net/tcp"), port, &mut inodes);
    collect_listening_socket_inodes(Path::new("/proc/net/tcp6"), port, &mut inodes);
    inodes.sort();
    inodes.dedup();
    inodes
}

fn listener_owned_by_pid_in_namespace(pid: u32, port: u16) -> HealthOwnerProbeResult {
    let mut inodes = Vec::new();
    collect_listening_socket_inodes(
        Path::new(&format!("/proc/{pid}/net/tcp")),
        port,
        &mut inodes,
    );
    collect_listening_socket_inodes(
        Path::new(&format!("/proc/{pid}/net/tcp6")),
        port,
        &mut inodes,
    );
    inodes.sort();
    inodes.dedup();
    if inodes.is_empty() {
        return HealthOwnerProbeResult {
            owned: false,
            detail: format!("no listening socket inode found for container health port {port}"),
        };
    }
    let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
    let Ok(entries) = fs::read_dir(&fd_dir) else {
        return HealthOwnerProbeResult {
            owned: false,
            detail: format!(
                "could not inspect container process fd directory {}",
                fd_dir.display()
            ),
        };
    };
    for entry in entries.flatten() {
        if let Ok(target) = fs::read_link(entry.path()) {
            let target = target.to_string_lossy();
            if inodes
                .iter()
                .any(|inode| target.as_ref() == format!("socket:[{inode}]"))
            {
                return HealthOwnerProbeResult {
                    owned: true,
                    detail: format!("container health port {port} listener is owned by pid {pid}"),
                };
            }
        }
    }
    HealthOwnerProbeResult {
        owned: false,
        detail: format!(
            "container health port {port} listener socket inodes {inodes:?} were not owned by pid {pid}"
        ),
    }
}

fn collect_listening_socket_inodes(path: &Path, port: u16, inodes: &mut Vec<String>) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() <= 9 || fields[3] != "0A" {
            continue;
        }
        let Some(local_port) = fields[1]
            .rsplit_once(':')
            .and_then(|(_, port)| u16::from_str_radix(port, 16).ok())
        else {
            continue;
        };
        if local_port == port {
            inodes.push(fields[9].to_string());
        }
    }
}

fn inspect_clean_container_exit(name: &str, output: &mut String) -> bool {
    let Some(inspect) = command_output(
        "docker",
        [
            "inspect",
            "--format",
            "exit_code={{.State.ExitCode}} oom_killed={{.State.OOMKilled}}",
            name,
        ],
    ) else {
        output.push_str("container exit inspect failed\n");
        return false;
    };
    let success = inspect.status.success();
    let text = output_combined_text(inspect);
    output.push_str(&text);
    success && text.contains("exit_code=0") && text.contains("oom_killed=false")
}

fn network_line_is_outbound(line: &str) -> bool {
    let traced_connect = line.contains("connect(") || line.contains("sendto(");
    let internet_socket = line.contains("AF_INET") || line.contains("AF_INET6");
    if !(traced_connect && internet_socket) {
        return false;
    }
    !(line.contains("127.0.0.1")
        || line.contains("127.0.1.1")
        || line.contains("inet_addr(\"0.0.0.0\")")
        || line.contains("\"::1\"")
        || line.contains("sin6_addr=inet_pton(AF_INET6, \"::\")")
        || line.contains("sin6_addr=inet_pton(AF_INET6, \"::1\")"))
}

fn wait_for_docker_health(name: &str, timeout: Duration, output: &mut String) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(inspect) = command_output(
            "docker",
            [
                "inspect",
                "--format",
                "{{if .State.Health}}{{.State.Health.Status}}{{else}}missing{{end}}",
                name,
            ],
        ) {
            let text = output_combined_text(inspect);
            let status = text.trim();
            if status == "healthy" {
                output.push_str(&text);
                return true;
            }
            if status == "unhealthy" || status == "missing" {
                output.push_str(&text);
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    false
}

fn unique_container_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("vigil-acceptance-{}-{nanos}", std::process::id())
}

fn escape_toml_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

fn binary_name(binary: &str) -> String {
    if cfg!(windows) {
        format!("{binary}.exe")
    } else {
        binary.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{direct_child_target_is_safe, free_port, network_line_is_outbound, workspace_root};

    #[test]
    fn free_port_allocates_unique_ports_within_acceptance_process() {
        let first = free_port();
        let second = free_port();

        assert_ne!(
            first, second,
            "acceptance harness must not hand the same health port to parallel child processes"
        );
    }

    #[test]
    fn signal_target_rejects_wildcard_process_targets() {
        for pid in [0, 1] {
            assert!(
                direct_child_target_is_safe(pid).is_err(),
                "pid {pid} must not reach direct child cleanup"
            );
        }
    }

    #[test]
    fn direct_child_cleanup_rejects_current_acceptance_process() {
        assert!(
            direct_child_target_is_safe(std::process::id()).is_err(),
            "direct child cleanup must refuse the current acceptance process"
        );
    }

    #[test]
    fn network_trace_does_not_classify_unspecified_local_connect_as_outbound() {
        let line = r#"connect(8, {sa_family=AF_INET, sin_port=htons(8098), sin_addr=inet_addr("0.0.0.0")}, 16) = 0"#;

        assert!(
            !network_line_is_outbound(line),
            "0.0.0.0 is a local unspecified address, not outbound egress"
        );
    }

    #[test]
    fn process_cleanup_has_no_wildcard_or_shell_kill_paths() {
        let root = workspace_root();
        let mut rust_files = Vec::new();
        collect_rust_files(&root.join("tests"), &mut rust_files);
        collect_rust_files(&root.join("crates/vigil/tests"), &mut rust_files);

        let banned = [
            ("raw shell kill", ["Command::new(", "\"kill\""].concat()),
            ("process-group kill", ["kill", "pg"].concat()),
            ("process-group lookup", ["get", "pgid"].concat()),
            ("nix signal kill", ["nix::sys::signal", "::kill"].concat()),
            ("process-group target enum", ["Signal", "Target"].concat()),
            (
                "process-group target variant",
                ["Process", "Group"].concat(),
            ),
            (
                "negative kill argument formatting",
                ["format!", "(\"-", "{"].concat(),
            ),
        ];
        let allowed_libc_kill_path = root.join("tests/acceptance/common.rs");
        let libc_kill = ["libc", "::", "kill", "("].concat();
        let mut libc_kill_locations = Vec::new();
        let mut violations = Vec::new();

        for path in rust_files {
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
            for (name, pattern) in &banned {
                if source.contains(pattern) {
                    violations.push(format!("{} contains {name}", path.display()));
                }
            }
            if source.contains(&libc_kill) {
                libc_kill_locations.push(path);
            }
        }

        assert!(
            violations.is_empty(),
            "acceptance cleanup must not contain wildcard/process-group kill paths:\n{}",
            violations.join("\n")
        );
        assert_eq!(
            libc_kill_locations,
            vec![allowed_libc_kill_path],
            "raw signal syscalls must stay centralized in the positive direct-child cleanup helper"
        );
    }

    fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rust_files(&path, files);
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
}
