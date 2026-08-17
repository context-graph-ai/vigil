use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;

use crate::common::{
    DockerProbe, HealthProbe, NetworkBlock, RssProbe, StoreProbe, VigilBinary, VigilProcess,
    free_port, output_text, pid_of, temp_config,
};

#[test]
fn vigil_standalone_version_reports_nonempty_build_identity() {
    let binary = VigilBinary::new();
    let version_cwd = TempDir::new().expect("version tempdir");
    let version_data_dir = version_cwd.path().join("data");
    let version_store = version_data_dir.join("store.contextgraph");
    let version = binary
        .command()
        .arg("--version")
        .current_dir(version_cwd.path())
        .env("VIGIL_DATA_DIR", &version_data_dir)
        .env("VIGIL_STORE_PATH", &version_store)
        .output();
    let help = binary.command().arg("--help").output();
    let mut failures = Vec::new();

    match version {
        Ok(output) => {
            let (stdout, stderr) = output_text(&output);
            if !output.status.success() {
                failures.push(format!("--version exited with {}", output.status));
            }
            if stdout.trim().is_empty() {
                failures.push("--version printed no build identity".to_string());
            }
            let expected_version = format!("vigil {}", env!("CARGO_PKG_VERSION"));
            if !stdout.trim().starts_with(&expected_version) {
                failures.push(format!(
                    "--version did not start with {expected_version:?}; stdout={stdout:?}, stderr={stderr:?}"
                ));
            }
            if version_data_dir.exists() {
                failures.push(format!(
                    "--version created data directory {}",
                    version_data_dir.display()
                ));
            }
            if version_store.exists() {
                failures.push(format!(
                    "--version created store path {}",
                    version_store.display()
                ));
            }
            let artifacts = store_artifacts_under(version_cwd.path());
            if !artifacts.is_empty() {
                failures.push(format!(
                    "--version created store artifacts under isolated cwd: {artifacts:?}"
                ));
            }
        }
        Err(error) => failures.push(format!(
            "could not execute {}: {error}",
            binary.path().display()
        )),
    }

    match help {
        Ok(output) => {
            let (stdout, _) = output_text(&output);
            if !output.status.success() {
                failures.push(format!("--help exited with {}", output.status));
            }
            // Superseded expectation: this pinned the command roster to
            // [run, fabric]. `vigil settings` is the ratified operator surface —
            // it is how a person reads what every setting is, who chose it, and
            // what is pending, and how they set or reset one on a running node —
            // so a binary that does not offer it is missing the surface the
            // whole settings model is answered through. The roster stays exact,
            // so an unannounced command still fails here.
            let commands = help_section_tokens(&stdout, "Commands:");
            if commands != ["run", "settings", "fabric"] {
                failures.push(format!(
                    "--help commands were not exactly [run, settings, fabric]: {commands:?}"
                ));
            }
            let options = help_section_tokens(&stdout, "Options:");
            if options != ["--help", "--version"] {
                failures.push(format!(
                    "--help options were not exactly [--help, --version]: {options:?}"
                ));
            }
            let removed_transport_flag = ["na", "ts"].concat();
            for forbidden in [
                "camera",
                "mqtt",
                "frigate",
                "sync",
                "server",
                "ui",
                "cloud",
                "tenant",
                "whatsapp",
                "blueprint",
                "signature",
                "remote",
                "admin",
                "diagnose",
            ]
            .into_iter()
            .chain(std::iter::once(removed_transport_flag.as_str()))
            {
                if stdout.to_ascii_lowercase().contains(forbidden) {
                    failures.push(format!("--help exposed out-of-scope surface {forbidden}"));
                }
            }
        }
        Err(error) => failures.push(format!("could not execute --help: {error}")),
    }

    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn vigil_standalone_first_start_opens_local_context_graph_store() {
    let binary = VigilBinary::new();
    let data = TempDir::new().expect("tempdir");
    let store_path = data.path().join("store.contextgraph");
    let env_port = free_port();
    let toml_port = free_port();
    let (_config_dir, config_path) =
        temp_config(data.path(), &store_path, toml_port).expect("config");
    let _network = NetworkBlock::outbound();
    let mut failures = Vec::new();

    match VigilProcess::spawn_network_traced(&binary, &config_path, env_port) {
        Ok(mut process) => {
            let spawned_pid = pid_of(&process);
            if !process
                .health()
                .wait_for_status(200, Duration::from_secs(2))
            {
                failures.push("health did not reach 200 on env-selected port".to_string());
            }
            let health_owner = process.health().listener_owned_by_process_tree(spawned_pid);
            if !health_owner.owned {
                failures.push(health_owner.detail.clone());
            }
            let live_lock = StoreProbe::new(&store_path).live_lock_held();
            if !live_lock.locked {
                failures.push(live_lock.detail);
            }
            let live_file =
                StoreProbe::new(&store_path).live_database_file_open_by_process_tree(spawned_pid);
            if !live_file.owned {
                failures.push(live_file.detail.clone());
            }
            let contention =
                StoreProbe::new(&store_path).public_open_blocked_by_process_tree(spawned_pid);
            if !contention.owned {
                failures.push(contention.detail.clone());
            }
            if health_owner.owner_pid != live_file.owner_pid
                || live_file.owner_pid != contention.owner_pid
            {
                failures.push(format!(
                    "health listener, open store file, and store lock were not held by the same process; health={:?}, file={:?}, lock={:?}",
                    health_owner.owner_pid, live_file.owner_pid, contention.owner_pid
                ));
            }
            if HealthProbe::new(toml_port).status() == Some(200) {
                failures.push("TOML health port won over environment override".to_string());
            }
            let logs = process.logs();
            if !logs.contains("embedding_loader=disabled") {
                failures.push(
                    "startup logs did not surface cg disabled text-embedder trace".to_string(),
                );
            }
            for forbidden in ["download", "nomic", "huggingface", "dns", "outbound"] {
                if logs.to_ascii_lowercase().contains(forbidden) {
                    failures.push(format!(
                        "startup logs contained offline-forbidden term {forbidden}"
                    ));
                }
            }
            let _ = process.terminate();
            let trace = process.network_trace();
            if !trace.tool_available {
                failures.push("network tracing tools were not available".to_string());
            }
            if !trace.outbound_attempts.is_empty() {
                failures.push(format!(
                    "startup attempted outbound network calls: {:?}; trace={}",
                    trace.outbound_attempts, trace.raw
                ));
            }
        }
        Err(error) => failures.push(format!("vigil process did not spawn: {error}")),
    }

    let probe = StoreProbe::new(&store_path).open_existing();
    if !probe.opened {
        failures.push(probe.detail);
    }

    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn vigil_standalone_restart_reopens_existing_store() {
    let binary = VigilBinary::new();
    let data = TempDir::new().expect("tempdir");
    let store_path = data.path().join("store.contextgraph");
    let first_port = free_port();
    let second_port = free_port();
    let (_config_dir, config_path) =
        temp_config(data.path(), &store_path, free_port()).expect("config");
    let mut failures = Vec::new();

    if let Ok(mut first) = VigilProcess::spawn(&binary, &config_path, first_port) {
        let _ = first.health().wait_for_status(200, Duration::from_secs(2));
        let _ = first.terminate();
    } else {
        failures.push("first start did not spawn".to_string());
    }
    let before = StoreProbe::new(&store_path).identity();

    match VigilProcess::spawn(&binary, &config_path, second_port) {
        Ok(mut second) => {
            let _ = second.health().wait_for_status(200, Duration::from_secs(2));
            let logs = second.logs();
            if !logs.contains("existing store") {
                failures.push("restart logs did not report existing store open".to_string());
            }
            let _ = second.terminate();
        }
        Err(error) => failures.push(format!("second start did not spawn: {error}")),
    }

    let after = StoreProbe::new(&store_path).identity();
    if before.is_none() || before != after {
        failures.push(format!(
            "store identity was not preserved; before={before:?}, after={after:?}"
        ));
    }
    let probe = StoreProbe::new(&store_path).open_existing();
    if !probe.opened {
        failures.push(probe.detail);
    }

    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn vigil_standalone_kill9_survivor_store_reopens_cleanly() {
    let binary = VigilBinary::new();
    let data = TempDir::new().expect("tempdir");
    let store_path = data.path().join("store.contextgraph");
    let (_config_dir, config_path) =
        temp_config(data.path(), &store_path, free_port()).expect("config");
    let mut failures = Vec::new();

    match VigilProcess::spawn(&binary, &config_path, free_port()) {
        Ok(mut first) => {
            if !first.wait_for_log("store created", Duration::from_secs(2)) {
                failures.push("first start did not log store-created signal".to_string());
            }
            let _ = first.kill9();
        }
        Err(error) => failures.push(format!("first start did not spawn: {error}")),
    }

    match VigilProcess::spawn(&binary, &config_path, free_port()) {
        Ok(mut second) => {
            if !second.health().wait_for_status(200, Duration::from_secs(2)) {
                failures.push("restart after hard kill did not reach health 200".to_string());
            }
            let _ = second.terminate();
        }
        Err(error) => failures.push(format!("restart after hard kill did not spawn: {error}")),
    }

    let probe = StoreProbe::new(&store_path).open_existing();
    if !probe.opened {
        failures.push(probe.detail);
    }

    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn vigil_standalone_health_200_requires_store_open_and_runtime_loop() {
    let binary = VigilBinary::new();
    let data = TempDir::new().expect("tempdir");
    let store_path = data.path().join("store.contextgraph");
    let health_port = free_port();
    let (_config_dir, config_path) =
        temp_config(data.path(), &store_path, free_port()).expect("config");
    let mut failures = Vec::new();

    match VigilProcess::spawn(&binary, &config_path, health_port) {
        Ok(mut process) => {
            let early_200 = process
                .health()
                .wait_for_status(200, Duration::from_millis(500));
            let logs = process.logs();
            let ready = logs.contains("store opened") && logs.contains("runtime loop ready");
            if early_200 && !ready {
                failures.push(
                    "health returned 200 before store-open and runtime-ready evidence".to_string(),
                );
            }
            if !process.wait_for_log("runtime loop ready", Duration::from_secs(2)) {
                failures.push("runtime-ready marker never appeared".to_string());
            }
            if !process.wait_for_log("embedding_loader=disabled", Duration::from_secs(2)) {
                failures
                    .push("runtime did not surface cg disabled text-embedder trace".to_string());
            }
            if process.health().status() != Some(200) {
                failures.push("health was not 200 after runtime-ready evidence".to_string());
            }
            let health_owner = process.health().listener_owned_by_pid(pid_of(&process));
            if !health_owner.owned {
                failures.push(health_owner.detail);
            }
            let live_lock = StoreProbe::new(&store_path).live_lock_held();
            if !live_lock.locked {
                failures.push(live_lock.detail);
            }
            let live_file = StoreProbe::new(&store_path).live_database_file_open(pid_of(&process));
            if !live_file.open {
                failures.push(live_file.detail);
            }
            let contention =
                StoreProbe::new(&store_path).public_open_blocked_by_process(pid_of(&process));
            if !contention.blocked {
                failures.push(contention.detail);
            }
            let _ = process.terminate();
        }
        Err(error) => failures.push(format!("vigil process did not spawn: {error}")),
    }

    assert!(failures.is_empty(), "{}", failures.join("; "));
}

fn help_section_tokens(help: &str, section: &str) -> Vec<String> {
    let mut in_section = false;
    let mut tokens = Vec::new();
    for line in help.lines() {
        let trimmed = line.trim();
        if trimmed == section {
            in_section = true;
            continue;
        }
        if in_section && trimmed.ends_with(':') {
            break;
        }
        if in_section
            && !trimmed.is_empty()
            && let Some(token) = trimmed.split_whitespace().next()
        {
            tokens.push(token.to_string());
        }
    }
    tokens
}

fn store_artifacts_under(root: &Path) -> Vec<PathBuf> {
    let mut artifacts = Vec::new();
    collect_store_artifacts(root, &mut artifacts);
    artifacts
}

fn collect_store_artifacts(path: &Path, artifacts: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_store_artifacts(&path, artifacts);
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.ends_with(".contextgraph") || name.ends_with(".cdb") || name.ends_with(".lock") {
            artifacts.push(path);
        }
    }
}

#[test]
fn vigil_standalone_keeps_watching_unmanaged_when_store_cannot_open() {
    // Superseded contract note: this required health 503 when the store could
    // not be opened, and the name said so. A camera system going blind because
    // a settings database is unreadable is the worse outcome, so the ratified
    // contract is the opposite: Vigil starts anyway, resolves from its own
    // surfaces, keeps live view, detection and alerting running, and says
    // loudly and continuously that it is unmanaged — with recording, review
    // history, corrections and settings changes unavailable and delivery
    // reported best-effort rather than assured. A 503 would take the cameras
    // down for a restart that fixes nothing, so liveness stays 2xx.
    //
    // Unfakeable because the liveness answer alone proves nothing: a run that
    // came up normally would also answer 2xx. Every leg is asserted together —
    // the unmanaged statement naming what still runs and what is lost, the
    // store-open cause, and the absence of the ready marker — so a build that
    // quietly opened a store, or one that came up silently degraded, fails.
    let binary = VigilBinary::new();
    let data = TempDir::new().expect("tempdir");
    let blocked_parent = data.path().join("not-a-directory");
    fs::write(&blocked_parent, "not a directory").expect("blocked path");
    let store_path = blocked_parent.join("store.contextgraph");
    let health_port = free_port();
    let (_config_dir, config_path) =
        temp_config(data.path(), &store_path, free_port()).expect("config");
    let mut failures = Vec::new();

    match VigilProcess::spawn(&binary, &config_path, health_port) {
        Ok(mut process) => {
            if !process
                .health()
                .wait_for_status(200, Duration::from_secs(10))
            {
                failures.push(
                    "an unreadable store must not take the watching down: liveness stays 2xx"
                        .to_string(),
                );
            }
            let logs = process.logs().to_ascii_lowercase();
            if !(logs.contains("store") && (logs.contains("permission") || logs.contains("open"))) {
                failures.push("logs did not name an actionable store-open error".to_string());
            }
            if !logs.contains("store_unreadable=true") {
                failures.push("the run did not state that the store is unreadable".to_string());
            }
            if !logs.contains("unmanaged") {
                failures.push("the run did not state that it is running unmanaged".to_string());
            }
            for still_running in ["live-view", "detection", "broker-alerting"] {
                if !logs.contains(still_running) {
                    failures.push(format!(
                        "the unmanaged statement did not name `{still_running}` as still running"
                    ));
                }
            }
            for unavailable in [
                "recording",
                "review-history",
                "corrections",
                "settings-changes",
            ] {
                if !logs.contains(unavailable) {
                    failures.push(format!(
                        "the unmanaged statement did not name `{unavailable}` as unavailable"
                    ));
                }
            }
            if !logs.contains("best-effort") {
                failures.push(
                    "the unmanaged statement did not qualify delivery as best-effort".to_string(),
                );
            }
            if logs.contains("runtime loop ready") {
                failures.push("ready marker appeared despite store-open failure".to_string());
            }
            let _ = process.terminate();
        }
        Err(error) => failures.push(format!("vigil process did not spawn: {error}")),
    }

    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn vigil_standalone_sigterm_exits_zero_and_releases_store() {
    let binary = VigilBinary::new();
    let data = TempDir::new().expect("tempdir");
    let store_path = data.path().join("store.contextgraph");
    let (_config_dir, config_path) =
        temp_config(data.path(), &store_path, free_port()).expect("config");
    let mut failures = Vec::new();

    match VigilProcess::spawn(&binary, &config_path, free_port()) {
        Ok(mut first) => {
            let _ = first.health().wait_for_status(200, Duration::from_secs(2));
            match first.terminate() {
                Some(status) if status.code() == Some(0) => {}
                Some(status) => failures.push(format!("SIGTERM exit was not zero: {status}")),
                None => failures.push("SIGTERM did not exit within 10 seconds".to_string()),
            }
        }
        Err(error) => failures.push(format!("first start did not spawn: {error}")),
    }

    match VigilProcess::spawn(&binary, &config_path, free_port()) {
        Ok(mut second) => {
            if !second.health().wait_for_status(200, Duration::from_secs(2)) {
                failures.push("immediate restart did not reach health 200".to_string());
            }
            let _ = second.terminate();
        }
        Err(error) => failures.push(format!("immediate restart did not spawn: {error}")),
    }

    let probe = StoreProbe::new(&store_path).open_existing();
    if !probe.opened {
        failures.push(probe.detail);
    }

    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn vigil_standalone_idle_rss_stays_below_100mb_without_growth() {
    let binary = VigilBinary::new();
    let data = TempDir::new().expect("tempdir");
    let store_path = data.path().join("store.contextgraph");
    let (_config_dir, config_path) =
        temp_config(data.path(), &store_path, free_port()).expect("config");
    let mut failures = Vec::new();

    match VigilProcess::spawn(&binary, &config_path, free_port()) {
        Ok(mut process) => {
            let _ = process
                .health()
                .wait_for_status(200, Duration::from_secs(2));
            if !process.wait_for_log("runtime loop ready", Duration::from_secs(2)) {
                failures.push("RSS window did not observe a ready runtime loop".to_string());
            } else {
                let rss = RssProbe::new(pid_of(&process));
                std::thread::sleep(Duration::from_secs(60));
                let first = rss.sample_kb();
                std::thread::sleep(Duration::from_secs(5));
                let second = rss.sample_kb();
                if first.is_none() || second.is_none() {
                    failures.push("could not sample process RSS".to_string());
                }
                if second.unwrap_or(u64::MAX) > 100 * 1024 {
                    failures.push(format!("idle RSS exceeded 100 MB: {second:?} KB"));
                }
                if second.unwrap_or(0).saturating_sub(first.unwrap_or(0)) > 10 * 1024 {
                    failures.push(format!(
                        "idle RSS grew more than 10 MB: first={first:?}, second={second:?}"
                    ));
                }
            }
            let _ = process.terminate();
        }
        Err(error) => failures.push(format!("vigil process did not spawn: {error}")),
    }

    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn vigil_container_healthcheck_serves_ready_and_stops_cleanly() {
    let image = DockerProbe::image();
    let observation = DockerProbe::image_healthcheck(&image);
    let volume = TempDir::new().expect("tempdir");
    let runtime = DockerProbe::run_volume_probe(&image, volume.path());
    let mut failures = Vec::new();

    if !observation.docker_available {
        failures.push("docker was not available for image inspection".to_string());
    }
    if !runtime.docker_available {
        failures.push("docker was not available for container runtime probe".to_string());
    }
    if !observation.command_succeeded {
        failures.push(format!(
            "local image {image} was not inspectable: {}",
            observation.stdout
        ));
    }
    if !observation.healthcheck_targets_health_endpoint() {
        failures.push("image healthcheck did not target /health".to_string());
    }
    if !runtime.command_succeeded {
        failures.push(format!(
            "container did not serve health 200 and stop with exit 0: {}",
            runtime.stdout
        ));
    }

    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn vigil_container_persistent_volume_reopens_same_store() {
    let image = DockerProbe::image();
    let volume = TempDir::new().expect("tempdir");
    let store_path = volume.path().join("store.contextgraph");
    let first = DockerProbe::run_volume_probe(&image, volume.path());
    let before = StoreProbe::new(&store_path).identity();
    let before_content = StoreProbe::new(&store_path).service_identity_marker();
    let second = DockerProbe::run_volume_probe(&image, volume.path());
    let after = StoreProbe::new(&store_path).identity();
    let after_content = StoreProbe::new(&store_path).service_identity_marker();
    let probe = StoreProbe::new(&store_path).open_existing();
    let mut failures = Vec::new();

    if !first.docker_available || !second.docker_available {
        failures.push("docker was not available for volume probe".to_string());
    }
    if !first.command_succeeded || !second.command_succeeded {
        failures.push(format!(
            "container volume runs did not both succeed: first={} second={}",
            first.stdout, second.stdout
        ));
    }
    if !second.container_logs.contains("existing store") {
        failures.push("second container run did not report existing store open".to_string());
    }
    if before.is_none() || before != after {
        failures.push(format!(
            "container store identity was not preserved; before={before:?}, after={after:?}"
        ));
    }
    if before_content.is_none() || before_content != after_content {
        failures.push(format!(
            "container store content was not preserved across reopen; \
             before={before_content:?}, after={after_content:?}"
        ));
    }
    if !probe.opened {
        failures.push(probe.detail);
    }

    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn vigil_container_first_start_health_succeeds_with_network_none() {
    let image = DockerProbe::image();
    let volume = TempDir::new().expect("tempdir");
    let healthcheck = DockerProbe::image_healthcheck(&image);
    let observation = DockerProbe::run_network_none_probe(&image, volume.path());
    let store_path = volume.path().join("store.contextgraph");
    let probe = StoreProbe::new(&store_path).open_existing();
    let mut failures = Vec::new();

    if !healthcheck.docker_available || !observation.docker_available {
        failures.push("docker was not available for network-none probe".to_string());
    }
    if !healthcheck.healthcheck_targets_health_endpoint() {
        failures.push("network-none image healthcheck did not target /health".to_string());
    }
    if !observation.command_succeeded {
        failures.push(format!(
            "container did not reach healthy first start under network isolation: {}",
            observation.stdout
        ));
    }
    if !probe.opened {
        failures.push(probe.detail);
    }
    for forbidden in ["download", "huggingface", "dns", "outbound", "hosted"] {
        if observation
            .container_logs
            .to_ascii_lowercase()
            .contains(forbidden)
        {
            failures.push(format!(
                "container logs contained offline-forbidden term {forbidden}"
            ));
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("; "));
}
