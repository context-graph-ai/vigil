use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde::Serialize;

const NEXTTEST_TEST_FAILED: i32 = 100;
const NEXTTEST_BUILD_FAILED: i32 = 101;
const DEFAULT_BUILD_JOBS: &str = "2";
const DEFAULT_TEST_THREADS: &str = "2";

const DEV_LANES: &[&str] = &[
    "discipline",
    "default",
    "feature",
    "slow-preflight",
    "slow-shard",
    "install-binary",
    "install-image",
    "install-smoke",
];
const RELEASE_LANES: &[&str] = &[
    "source",
    "static",
    "artifact-amd64",
    "artifact-arm64",
    "image-install",
    "reproducibility",
];

pub(crate) fn run(args: &[OsString]) -> Result<(), String> {
    if args.len() == 1 && matches!(args[0].to_str(), Some("--help" | "-h")) {
        println!("{}", usage());
        return Ok(());
    }
    let root = super::repo_root()?;
    let started = Instant::now();
    let (lock, queue_wait, launcher) = ResourceLock::acquire(&root)?;

    if args.first().and_then(|arg| arg.to_str()) == Some("workflow") {
        let result = run_workflow_entrypoint(&root, &args[1..]);
        drop(lock);
        return result;
    }
    if args.iter().any(|arg| arg == "--rehearse") {
        let result = run_closeout_rehearsal(&root, args);
        drop(lock);
        return result;
    }

    let options = Options::parse(args)?;

    let identities = RepoIdentities::read(&root)?;
    options.validate_identities(&identities)?;
    let command = options.command(&root)?;
    let resources_before = ResourceSnapshot::read(&root)?;
    validate_cache_state(&options.cache_state, resources_before.cargo_build_bytes)?;
    let observed_cache_state = observed_cache_state(resources_before.cargo_build_bytes);
    let environment = EnvironmentReceipt::read();
    let step = run_step(&root, command, options.output_path(&root).as_deref())?;
    let command_verdict = options.verdict(&step);
    let identities_after = RepoIdentities::read(&root)?;
    let source_verdict = options.validate_unchanged(&identities, &identities_after);
    let verdict = command_verdict.and(source_verdict);
    let resources_after = ResourceSnapshot::read(&root)?;

    let receipt_path = options.receipt_path(&root);
    let receipt_path = absolute_path(&receipt_path)?;
    let receipt = Receipt {
        schema_version: 2,
        tier: options.tier.as_str(),
        lane: options.lane.as_deref(),
        command_contract: options.command_contract(),
        shape: options.shape.as_deref(),
        filter: options.filter.as_deref(),
        expectation: options.expect.map(Expectation::as_str),
        vigil_sha: &identities.vigil.sha,
        context_graph_sha: &identities.context_graph.sha,
        contextdb_sha: &identities.contextdb.sha,
        dirty: DirtyState {
            vigil: identities.vigil.dirty,
            context_graph: identities.context_graph.dirty,
            contextdb: identities.contextdb.dirty,
        },
        dirty_after: DirtyState {
            vigil: identities_after.vigil.dirty,
            context_graph: identities_after.context_graph.dirty,
            contextdb: identities_after.contextdb.dirty,
        },
        source_unchanged: same_identities(&identities, &identities_after),
        cache_state: &options.cache_state,
        observed_cache_state,
        steps: vec![step],
        total_wall_ms: millis(started.elapsed())
            .saturating_add(launcher.queue_wait_ms)
            .saturating_add(launcher.bootstrap_wall_ms),
        queue_wait_ms: millis(queue_wait),
        bootstrap_wall_ms: launcher.bootstrap_wall_ms,
        bootstrap_max_rss_bytes: launcher.bootstrap_max_rss_bytes,
        bootstrap_target_bytes_before: launcher.bootstrap_target_bytes_before,
        bootstrap_target_bytes_after: launcher.bootstrap_target_bytes_after,
        bootstrap_disk_available_before: launcher.bootstrap_disk_available_before,
        bootstrap_disk_available_after: launcher.bootstrap_disk_available_after,
        resources_before,
        resources_after,
        rustc_version: environment.rustc_version,
        cargo_version: environment.cargo_version,
        cargo_nextest_version: environment.cargo_nextest_version,
        kernel_release: environment.kernel_release,
        architecture: environment.architecture,
        environment_measurement_status: environment.measurement_status,
        cpu_count: std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1),
        effective_cargo_build_jobs: DEFAULT_BUILD_JOBS,
        effective_nextest_test_threads: DEFAULT_TEST_THREADS,
        receipt_path: receipt_path.display().to_string(),
    };
    write_receipt(&receipt_path, &receipt)?;
    println!("verification receipt: {}", receipt_path.display());

    drop(lock);
    verdict
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tier {
    Change,
    DevCloseout,
    Release,
}

impl Tier {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "change" => Ok(Self::Change),
            "dev-closeout" => Ok(Self::DevCloseout),
            "release" => Ok(Self::Release),
            _ => Err(format!(
                "unknown verify tier `{value}`; expected change, dev-closeout, or release"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Change => "change",
            Self::DevCloseout => "dev-closeout",
            Self::Release => "release",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Expectation {
    Red,
    Green,
}

impl Expectation {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "red" => Ok(Self::Red),
            "green" => Ok(Self::Green),
            _ => Err(format!(
                "invalid expectation `{value}`; expected red or green"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Green => "green",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Options {
    tier: Tier,
    lane: Option<String>,
    shape: Option<String>,
    filter: Option<String>,
    expect: Option<Expectation>,
    cache_state: String,
    vigil_sha: Option<String>,
    context_graph_sha: Option<String>,
    contextdb_sha: Option<String>,
    receipt: Option<PathBuf>,
    step: Option<String>,
    shard: Option<String>,
    lane_command: Vec<String>,
}

impl Options {
    fn parse(args: &[OsString]) -> Result<Self, String> {
        let mut values = args.iter();
        let tier = values
            .next()
            .and_then(|value| value.to_str())
            .ok_or_else(usage)
            .and_then(Tier::parse)?;
        let mut options = Self {
            tier,
            lane: None,
            shape: None,
            filter: None,
            expect: None,
            cache_state: String::new(),
            vigil_sha: None,
            context_graph_sha: None,
            contextdb_sha: None,
            receipt: None,
            step: None,
            shard: None,
            lane_command: Vec::new(),
        };

        while let Some(argument) = values.next() {
            let argument = argument
                .to_str()
                .ok_or_else(|| "verify arguments must be valid UTF-8".to_string())?;
            if argument == "--" {
                options.lane_command = values
                    .map(|value| {
                        value
                            .to_str()
                            .map(str::to_owned)
                            .ok_or_else(|| "lane command must be valid UTF-8".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                break;
            }
            let value = values
                .next()
                .and_then(|value| value.to_str())
                .ok_or_else(|| format!("{argument} requires a value"))?;
            match argument {
                "--lane" => set_once(&mut options.lane, value, argument)?,
                "--shape" => set_once(&mut options.shape, value, argument)?,
                "--filter" => set_once(&mut options.filter, value, argument)?,
                "--expect" => {
                    if options.expect.replace(Expectation::parse(value)?).is_some() {
                        return Err("--expect was supplied more than once".to_string());
                    }
                }
                "--cache-state" => {
                    if !options.cache_state.is_empty() {
                        return Err("--cache-state was supplied more than once".to_string());
                    }
                    options.cache_state = value.to_string();
                }
                "--vigil-sha" => set_once(&mut options.vigil_sha, value, argument)?,
                "--context-graph-sha" => set_once(&mut options.context_graph_sha, value, argument)?,
                "--contextdb-sha" => set_once(&mut options.contextdb_sha, value, argument)?,
                "--receipt" => {
                    if options.receipt.replace(PathBuf::from(value)).is_some() {
                        return Err("--receipt was supplied more than once".to_string());
                    }
                }
                "--step" => set_once(&mut options.step, value, argument)?,
                "--shard" => set_once(&mut options.shard, value, argument)?,
                _ => return Err(format!("unknown verify option `{argument}`\n{}", usage())),
            }
        }
        options.validate()?;
        Ok(options)
    }

    fn validate(&self) -> Result<(), String> {
        if !matches!(
            self.cache_state.as_str(),
            "warm" | "cold" | "mixed" | "unknown"
        ) {
            return Err("--cache-state must be warm, cold, mixed, or unknown".to_string());
        }
        match self.tier {
            Tier::Change => {
                require_nonempty(&self.shape, "--shape")?;
                require_nonempty(&self.filter, "--filter")?;
                if self.expect.is_none() {
                    return Err("change verification requires --expect red|green".to_string());
                }
                if self.lane.is_some()
                    || self.vigil_sha.is_some()
                    || self.context_graph_sha.is_some()
                    || self.contextdb_sha.is_some()
                    || self.step.is_some()
                    || self.shard.is_some()
                    || !self.lane_command.is_empty()
                {
                    return Err(
                        "change verification accepts a registered shape and filter, not a lane command or supplied SHAs"
                            .to_string(),
                    );
                }
            }
            Tier::DevCloseout | Tier::Release => {
                if self.filter.is_some() || self.expect.is_some() {
                    return Err(
                        "closeout and release tiers accept --lane, not change-test options"
                            .to_string(),
                    );
                }
                let lane = require_nonempty(&self.lane, "--lane")?;
                let allowed = match self.tier {
                    Tier::DevCloseout => DEV_LANES,
                    Tier::Release => RELEASE_LANES,
                    Tier::Change => unreachable!(),
                };
                if !allowed.contains(&lane) {
                    return Err(format!(
                        "lane `{lane}` is not registered for {}; expected one of {}",
                        self.tier.as_str(),
                        allowed.join(", ")
                    ));
                }
                for (name, sha) in [
                    ("--vigil-sha", &self.vigil_sha),
                    ("--context-graph-sha", &self.context_graph_sha),
                    ("--contextdb-sha", &self.contextdb_sha),
                ] {
                    let sha = require_nonempty(sha, name)?;
                    validate_sha(sha).map_err(|error| format!("{name}: {error}"))?;
                }
                if self.lane_command.is_empty() && self.step.is_none() {
                    return Err(
                        "a closeout/release lane requires --step NAME or `-- COMMAND [ARG]...`"
                            .to_string(),
                    );
                }
                if !self.lane_command.is_empty() && self.step.is_some() {
                    return Err(
                        "--step and an explicit lane command are mutually exclusive".to_string()
                    );
                }
                if self.step.is_some() {
                    validate_named_lane_step(self.tier, lane, self)?;
                } else {
                    if self.shape.is_some() || self.shard.is_some() {
                        return Err(
                            "--shape and --shard are only accepted with a named --step".to_string()
                        );
                    }
                    validate_lane_command(self.tier, lane, &self.lane_command)?;
                }
            }
        }
        Ok(())
    }

    fn validate_identities(&self, identities: &RepoIdentities) -> Result<(), String> {
        if self.tier == Tier::Change {
            return Ok(());
        }
        let dirty_repositories = [
            ("Vigil", identities.vigil.dirty),
            ("Context Graph", identities.context_graph.dirty),
            ("ContextDB", identities.contextdb.dirty),
        ]
        .into_iter()
        .filter_map(|(name, dirty)| dirty.then_some(name))
        .collect::<Vec<_>>();
        if !dirty_repositories.is_empty() {
            return Err(format!(
                "{} verification requires clean exact inputs; dirty repositories: {}",
                self.tier.as_str(),
                dirty_repositories.join(", ")
            ));
        }
        for (name, supplied, observed) in [
            (
                "Vigil",
                self.vigil_sha.as_deref(),
                identities.vigil.sha.as_str(),
            ),
            (
                "Context Graph",
                self.context_graph_sha.as_deref(),
                identities.context_graph.sha.as_str(),
            ),
            (
                "ContextDB",
                self.contextdb_sha.as_deref(),
                identities.contextdb.sha.as_str(),
            ),
        ] {
            if supplied != Some(observed) {
                return Err(format!(
                    "{name} exact SHA mismatch: supplied {}, observed {observed}",
                    supplied.unwrap_or("<missing>")
                ));
            }
        }
        Ok(())
    }

    fn validate_unchanged(
        &self,
        before: &RepoIdentities,
        after: &RepoIdentities,
    ) -> Result<(), String> {
        if same_identities(before, after) {
            self.validate_identities(after)
        } else {
            Err(format!(
                "{} verification changed a source commit or working tree while running; the receipt is invalid",
                self.tier.as_str()
            ))
        }
    }

    fn command(&self, root: &Path) -> Result<Vec<String>, String> {
        match self.tier {
            Tier::Change => change_command(
                root,
                self.shape.as_deref().expect("validated shape"),
                self.filter.as_deref().expect("validated filter"),
            ),
            Tier::DevCloseout | Tier::Release => {
                let command = if self.step.is_some() {
                    named_lane_command(root, self)?
                } else {
                    self.lane_command.clone()
                };
                Ok(ensure_locked(bound_nextest(command)))
            }
        }
    }

    fn command_contract(&self) -> Option<&'static str> {
        self.lane
            .as_deref()
            .map(|lane| lane_command_contract(self.tier, lane).expect("validated lane"))
    }

    fn output_path(&self, root: &Path) -> Option<PathBuf> {
        match (
            self.tier,
            self.lane.as_deref(),
            self.step.as_deref(),
            self.shape.as_deref(),
        ) {
            (Tier::DevCloseout, Some("default"), Some("inventory"), _) => {
                Some(root.join("target/nextest-default.json"))
            }
            (Tier::DevCloseout, Some("feature"), Some("inventory"), Some(shape)) => {
                Some(root.join(format!("target/nextest-{shape}.json")))
            }
            (Tier::DevCloseout, Some("slow-preflight"), Some("inventory"), _) => {
                Some(root.join("target/nextest-slow.json"))
            }
            _ => None,
        }
    }

    fn verdict(&self, step: &StepReceipt) -> Result<(), String> {
        let code = step.exit_code;
        match (self.expect, code) {
            (Some(Expectation::Green), Some(0)) | (None, Some(0)) => Ok(()),
            (Some(Expectation::Red), Some(NEXTTEST_TEST_FAILED)) => Ok(()),
            (Some(Expectation::Red), Some(NEXTTEST_BUILD_FAILED)) => Err(
                "RED verification did not reach a test failure: nextest reported a build failure (101)"
                    .to_string(),
            ),
            (Some(Expectation::Red), Some(0)) => {
                Err("RED verification unexpectedly passed".to_string())
            }
            (Some(Expectation::Red), other) => Err(format!(
                "RED verification did not produce nextest's test-failure exit 100; observed {}",
                display_code(other)
            )),
            (Some(Expectation::Green), Some(NEXTTEST_TEST_FAILED)) => {
                Err("GREEN verification reached tests, but tests failed (nextest exit 100)".to_string())
            }
            (Some(Expectation::Green), Some(NEXTTEST_BUILD_FAILED)) => {
                Err("GREEN verification failed to build (nextest exit 101)".to_string())
            }
            (Some(Expectation::Green), other) | (None, other) => Err(format!(
                "verification command failed with {}",
                display_code(other)
            )),
        }
    }

    fn receipt_path(&self, root: &Path) -> PathBuf {
        self.receipt.clone().unwrap_or_else(|| {
            root.join("target/verify-receipts").join(format!(
                "{}-{}-{}.json",
                self.tier.as_str(),
                self.lane
                    .as_deref()
                    .or(self.shape.as_deref())
                    .unwrap_or("run"),
                std::process::id()
            ))
        })
    }
}

fn usage() -> String {
    "usage:\n  scripts/verify change --shape NAME --filter EXPR --expect red|green --cache-state STATE [--receipt PATH]\n  scripts/verify dev-closeout --rehearse\n  scripts/verify dev-closeout --lane NAME --step NAME --vigil-sha SHA --context-graph-sha SHA --contextdb-sha SHA --cache-state STATE [--shape NAME] [--shard N] [--receipt PATH]\n  scripts/verify release --lane NAME --step NAME --vigil-sha SHA --context-graph-sha SHA --contextdb-sha SHA --cache-state STATE [--receipt PATH]\n  scripts/verify workflow --step NAME [OPTION VALUE]...".to_string()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn run_workflow_entrypoint(root: &Path, args: &[OsString]) -> Result<(), String> {
    if args.len() < 2 || args[0] != "--step" {
        return Err("usage: scripts/verify workflow --step NAME [OPTION VALUE]...".to_string());
    }
    let step = args[1]
        .to_str()
        .ok_or_else(|| "workflow step must be valid UTF-8".to_string())?;
    let extra = &args[2..];
    match step {
        "runtime-harness" if extra.is_empty() => super::setup_runtime_harness(),
        "setup-harness" if extra.is_empty() => super::setup_harness(),
        "closeout-impact" => super::closeout_impact::run(extra),
        "fmt" if extra.is_empty() => run_success(
            root,
            strings(&[
                "cargo",
                "fmt",
                "-p",
                "vigil",
                "-p",
                "vigil-ha",
                "-p",
                "vigil-bin",
                "-p",
                "xtask",
                "-p",
                "vigil-acceptance",
                "--check",
            ]),
            None,
        ),
        "clippy" if extra.is_empty() => run_success(
            root,
            strings(&[
                "cargo",
                "clippy",
                "--locked",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ]),
            None,
        ),
        "default-inventory" if extra.is_empty() => run_success(
            root,
            registered_list_command(root, "default")?,
            Some(&root.join("target/nextest-default.json")),
        ),
        "default-estate" if extra.is_empty() => super::test_estate::check(&[
            OsString::from("--nextest-json"),
            OsString::from("default=target/nextest-default.json"),
        ]),
        "default-tests" if extra.is_empty() => {
            run_success(root, registered_run_command(root, "default")?, None)
        }
        _ => Err(format!("unknown or malformed workflow step `{step}`")),
    }
}

fn run_success(root: &Path, argv: Vec<String>, stdout_path: Option<&Path>) -> Result<(), String> {
    let step = run_step(root, ensure_locked(argv), stdout_path)?;
    if step.exit_code == Some(0) {
        Ok(())
    } else {
        Err(format!(
            "workflow entrypoint `{}` failed with {}",
            step.argv.join(" "),
            display_code(step.exit_code)
        ))
    }
}

fn run_closeout_rehearsal(root: &Path, args: &[OsString]) -> Result<(), String> {
    if args != [OsString::from("dev-closeout"), OsString::from("--rehearse")] {
        return Err("usage: scripts/verify dev-closeout --rehearse".to_string());
    }

    super::setup_runtime_harness()?;
    run_success(
        root,
        strings(&[
            "cargo",
            "fmt",
            "-p",
            "vigil",
            "-p",
            "vigil-ha",
            "-p",
            "vigil-bin",
            "-p",
            "xtask",
            "-p",
            "vigil-acceptance",
            "--check",
        ]),
        None,
    )?;
    run_success(
        root,
        registered_list_command(root, "default")?,
        Some(&root.join("target/nextest-default.json")),
    )?;
    super::test_estate::check(&[
        OsString::from("--nextest-json"),
        OsString::from("default=target/nextest-default.json"),
    ])?;
    run_success(root, registered_run_command(root, "default")?, None)?;

    let closeout = root.join("target/closeout");
    fs::create_dir_all(&closeout)
        .map_err(|error| format!("create closeout directory {}: {error}", closeout.display()))?;
    let archive = closeout.join("nextest-slow.tar.zst");
    run_success(
        root,
        strings(&[
            "cargo",
            "nextest",
            "archive",
            "--locked",
            "--release",
            "--workspace",
            "--features",
            "first-light-acceptance,acceptance",
            "--archive-file",
            "target/closeout/nextest-slow.tar.zst",
        ]),
        None,
    )?;
    run_success(
        root,
        vec![
            "cargo".to_string(),
            "nextest".to_string(),
            "list".to_string(),
            "--archive-file".to_string(),
            archive.display().to_string(),
            "--workspace-remap".to_string(),
            root.display().to_string(),
            "-T".to_string(),
            "json".to_string(),
        ],
        Some(&root.join("target/nextest-slow.json")),
    )?;
    super::test_estate::check(&[
        OsString::from("--nextest-json"),
        OsString::from("slow=target/nextest-slow.json"),
    ])?;

    let restored = rehearse_archive_handoff(root, &archive)?;
    let restored_archive = restored.join("nextest-slow.tar.zst");
    let restored_binary = restored.join("vigil-acceptance-bin");
    let mut slow = Command::new("cargo");
    slow.current_dir(root)
        .env("CARGO_BUILD_JOBS", DEFAULT_BUILD_JOBS)
        .env("VIGIL_ACCEPTANCE_BIN", &restored_binary)
        .args([
            "nextest",
            "run",
            "--archive-file",
            restored_archive
                .to_str()
                .ok_or_else(|| "restored archive path is not UTF-8".to_string())?,
            "--workspace-remap",
            root.to_str()
                .ok_or_else(|| "workspace path is not UTF-8".to_string())?,
            "--profile",
            "ci-full",
            "-E",
            "not test(vigil_container_)",
        ]);
    let status = slow
        .status()
        .map_err(|error| format!("run one-pass slow archive rehearsal: {error}"))?;
    if !status.success() {
        return Err(format!(
            "one-pass slow archive rehearsal failed with {status}"
        ));
    }
    Ok(())
}

fn rehearse_archive_handoff(root: &Path, archive: &Path) -> Result<PathBuf, String> {
    let boundary = root.join("target/closeout/rehearsal-handoff");
    if boundary.exists() {
        fs::remove_dir_all(&boundary)
            .map_err(|error| format!("clear prior rehearsal handoff: {error}"))?;
    }
    let staged = boundary.join("staged");
    let restored = boundary.join("restored");
    fs::create_dir_all(&staged).map_err(|error| format!("create handoff staging: {error}"))?;
    fs::create_dir_all(&restored).map_err(|error| format!("create handoff restore: {error}"))?;
    fs::copy(archive, staged.join("nextest-slow.tar.zst"))
        .map_err(|error| format!("stage slow archive: {error}"))?;
    let source_binary = root.join("target/release/vigil");
    let staged_binary = staged.join("vigil-acceptance-bin");
    fs::copy(&source_binary, &staged_binary).map_err(|error| {
        format!(
            "stage archive-built binary {}: {error}",
            source_binary.display()
        )
    })?;
    #[cfg(unix)]
    fs::set_permissions(&staged_binary, fs::Permissions::from_mode(0o644))
        .map_err(|error| format!("strip staged binary mode: {error}"))?;

    let transfer = boundary.join("artifact.tar");
    command_success(
        Command::new("tar")
            .arg("-cf")
            .arg(&transfer)
            .arg("-C")
            .arg(&staged)
            .arg("."),
        "create mode-stripped artifact boundary",
    )?;
    command_success(
        Command::new("tar")
            .arg("-xf")
            .arg(&transfer)
            .arg("-C")
            .arg(&restored)
            .arg("--no-same-permissions"),
        "restore mode-stripped artifact boundary",
    )?;

    let restored_binary = restored.join("vigil-acceptance-bin");
    #[cfg(unix)]
    {
        let mode = fs::metadata(&restored_binary)
            .map_err(|error| format!("inspect restored binary mode: {error}"))?
            .permissions()
            .mode();
        if mode & 0o111 != 0 {
            return Err(format!(
                "artifact rehearsal did not strip the executable mode: observed {mode:o}"
            ));
        }
        fs::set_permissions(&restored_binary, fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("restore executable mode: {error}"))?;
    }
    let output = Command::new(&restored_binary)
        .arg("--version")
        .output()
        .map_err(|error| format!("spawn restored archive-built binary: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "restored archive-built binary failed to spawn: {}",
            output.status
        ));
    }
    Ok(restored)
}

fn command_success(command: &mut Command, label: &str) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|error| format!("{label}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{label} failed with {status}"))
    }
}

fn validate_named_lane_step(tier: Tier, lane: &str, options: &Options) -> Result<(), String> {
    let step = options.step.as_deref().expect("named step");
    let accepted = match (tier, lane, step) {
        (Tier::DevCloseout, "discipline", "fmt" | "clippy" | "estate-default") => {
            options.shape.is_none() && options.shard.is_none()
        }
        (Tier::DevCloseout, "discipline", "estate-feature") => {
            options.shape.as_deref().is_some_and(is_feature_shape) && options.shard.is_none()
        }
        (Tier::DevCloseout, "default", "inventory" | "tests") => {
            options.shape.is_none() && options.shard.is_none()
        }
        (Tier::DevCloseout, "feature", "inventory" | "tests") => {
            options.shape.as_deref().is_some_and(is_feature_shape) && options.shard.is_none()
        }
        (
            Tier::DevCloseout,
            "slow-preflight",
            "archive" | "inventory" | "estate" | "setup-harness",
        ) => options.shape.is_none() && options.shard.is_none(),
        (Tier::DevCloseout, "slow-shard", "tests") => {
            options.shape.is_none()
                && options
                    .shard
                    .as_deref()
                    .is_some_and(|shard| matches!(shard.parse::<u8>(), Ok(1..=4)))
        }
        (Tier::DevCloseout, "install-binary", "build")
        | (Tier::DevCloseout, "install-smoke", "tests")
        | (Tier::DevCloseout, "install-image", "build" | "inspect" | "save" | "load")
        | (Tier::Release, "source", "tests")
        | (Tier::Release, "static", "amd64" | "arm64") => {
            options.shape.is_none() && options.shard.is_none()
        }
        _ => false,
    };
    if accepted {
        Ok(())
    } else {
        Err(format!(
            "named step `{step}` with shape {:?} and shard {:?} is not registered for the `{lane}` lane",
            options.shape, options.shard
        ))
    }
}

fn is_feature_shape(shape: &str) -> bool {
    matches!(
        shape,
        "decode" | "detect" | "combined" | "fabric" | "production"
    )
}

fn named_lane_command(root: &Path, options: &Options) -> Result<Vec<String>, String> {
    let lane = options.lane.as_deref().expect("validated lane");
    let step = options.step.as_deref().expect("validated step");
    let command = match (options.tier, lane, step) {
        (Tier::DevCloseout, "discipline", "fmt") => strings(&[
            "cargo",
            "fmt",
            "-p",
            "vigil",
            "-p",
            "vigil-ha",
            "-p",
            "vigil-bin",
            "-p",
            "xtask",
            "-p",
            "vigil-acceptance",
            "--check",
        ]),
        (Tier::DevCloseout, "discipline", "clippy") => strings(&[
            "cargo",
            "clippy",
            "--locked",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ]),
        (Tier::DevCloseout, "discipline", "estate-default") => strings(&[
            "cargo",
            "xtask",
            "test-estate-check",
            "--nextest-json",
            "default=target/nextest-default.json",
        ]),
        (Tier::DevCloseout, "discipline", "estate-feature") => {
            let shape = options.shape.as_deref().expect("validated feature shape");
            vec![
                "cargo".to_string(),
                "xtask".to_string(),
                "test-estate-check".to_string(),
                "--nextest-json".to_string(),
                format!("{shape}=target/nextest-{shape}.json"),
            ]
        }
        (Tier::DevCloseout, "slow-preflight", "estate") => strings(&[
            "cargo",
            "xtask",
            "test-estate-check",
            "--nextest-json",
            "slow=target/nextest-slow.json",
        ]),
        (Tier::DevCloseout, "default", "inventory") => registered_list_command(root, "default")?,
        (Tier::DevCloseout, "default", "tests") => registered_run_command(root, "default")?,
        (Tier::DevCloseout, "feature", "inventory") => {
            registered_list_command(root, options.shape.as_deref().expect("validated shape"))?
        }
        (Tier::DevCloseout, "feature", "tests") => {
            registered_run_command(root, options.shape.as_deref().expect("validated shape"))?
        }
        (Tier::DevCloseout, "slow-preflight", "archive") => strings(&[
            "cargo",
            "nextest",
            "archive",
            "--locked",
            "--release",
            "--workspace",
            "--features",
            "first-light-acceptance,acceptance",
            "--archive-file",
            "target/closeout/nextest-slow.tar.zst",
        ]),
        (Tier::DevCloseout, "slow-preflight", "inventory") => vec![
            "cargo".to_string(),
            "nextest".to_string(),
            "list".to_string(),
            "--archive-file".to_string(),
            "target/closeout/nextest-slow.tar.zst".to_string(),
            "--workspace-remap".to_string(),
            root.display().to_string(),
            "-T".to_string(),
            "json".to_string(),
        ],
        (Tier::DevCloseout, "slow-preflight", "setup-harness") => {
            strings(&["cargo", "xtask", "setup-harness"])
        }
        (Tier::DevCloseout, "slow-shard", "tests") => vec![
            "cargo".to_string(),
            "nextest".to_string(),
            "run".to_string(),
            "--archive-file".to_string(),
            "target/closeout/nextest-slow.tar.zst".to_string(),
            "--workspace-remap".to_string(),
            root.display().to_string(),
            "--profile".to_string(),
            "ci-full".to_string(),
            "--partition".to_string(),
            format!(
                "hash:{}/4",
                options.shard.as_deref().expect("validated shard")
            ),
            "-E".to_string(),
            "not test(vigil_container_)".to_string(),
        ],
        (Tier::DevCloseout, "install-binary", "build") => strings(&[
            "cargo",
            "build",
            "--locked",
            "-p",
            "vigil-bin",
            "--release",
            "--target",
            "x86_64-unknown-linux-musl",
            "--features",
            "fabric",
        ]),
        (Tier::DevCloseout, "install-image", image_step) => {
            let sha = options.vigil_sha.as_deref().expect("validated sha");
            let tag = format!("vigil-closeout:{sha}");
            match image_step {
                "build" => vec![
                    "docker".to_string(),
                    "build".to_string(),
                    "--tag".to_string(),
                    tag,
                    "target/closeout/docker-context".to_string(),
                ],
                "inspect" => vec![
                    "docker".to_string(),
                    "image".to_string(),
                    "inspect".to_string(),
                    tag,
                ],
                "save" => vec![
                    "docker".to_string(),
                    "save".to_string(),
                    "--output".to_string(),
                    "target/closeout/vigil-acceptance-image.tar".to_string(),
                    tag,
                ],
                "load" => strings(&[
                    "docker",
                    "load",
                    "--input",
                    "target/closeout/vigil-acceptance-image.tar",
                ]),
                _ => unreachable!(),
            }
        }
        (Tier::DevCloseout, "install-smoke", "tests") => vec![
            "cargo".to_string(),
            "nextest".to_string(),
            "run".to_string(),
            "--archive-file".to_string(),
            "target/closeout/nextest-slow.tar.zst".to_string(),
            "--workspace-remap".to_string(),
            root.display().to_string(),
            "--profile".to_string(),
            "ci-full".to_string(),
            "-E".to_string(),
            "test(vigil_container_)".to_string(),
        ],
        (Tier::Release, "source", "tests") => registered_run_command(root, "default")?,
        (Tier::Release, "static", architecture) => strings(&[
            "cargo",
            "build",
            "--locked",
            "--release",
            "--target",
            if architecture == "amd64" {
                "x86_64-unknown-linux-musl"
            } else {
                "aarch64-unknown-linux-musl"
            },
            "--features",
            "fabric",
        ]),
        _ => unreachable!("validated named lane step"),
    };
    validate_lane_command(options.tier, lane, &command)?;
    Ok(command)
}

fn registered_list_command(root: &Path, shape: &str) -> Result<Vec<String>, String> {
    let registry = fs::read_to_string(root.join(".config/nextest-inventories.toml"))
        .map_err(|error| format!("read nextest shape registry: {error}"))?;
    let registry: toml::Value = toml::from_str(&registry)
        .map_err(|error| format!("parse nextest shape registry: {error}"))?;
    let command = registry
        .get("shape")
        .and_then(toml::Value::as_array)
        .and_then(|shapes| {
            shapes.iter().find_map(|entry| {
                (entry.get("name").and_then(toml::Value::as_str) == Some(shape))
                    .then(|| entry.get("command").and_then(toml::Value::as_str))
                    .flatten()
            })
        })
        .ok_or_else(|| format!("nextest shape `{shape}` is not registered"))?;
    let argv = command
        .split_ascii_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if !matches_prefix(&argv, &["cargo", "nextest", "list"]) {
        return Err(format!(
            "registered shape `{shape}` is not a nextest list command"
        ));
    }
    Ok(ensure_locked(argv))
}

fn registered_run_command(root: &Path, shape: &str) -> Result<Vec<String>, String> {
    let mut argv = registered_list_command(root, shape)?;
    argv[2] = "run".to_string();
    if let Some(index) = argv.iter().position(|argument| argument == "-T") {
        argv.drain(index..=(index + 1));
    }
    argv.extend([
        "--profile".to_string(),
        "pr".to_string(),
        "--test-threads".to_string(),
        DEFAULT_TEST_THREADS.to_string(),
    ]);
    Ok(argv)
}

fn ensure_locked(mut argv: Vec<String>) -> Vec<String> {
    let needs_lock = matches!(
        argv.get(0..2)
            .map(|items| [items[0].as_str(), items[1].as_str()]),
        Some(["cargo", "build" | "check" | "test" | "clippy"])
    ) || (matches_prefix(&argv, &["cargo", "nextest", "run"])
        && !has_option(&argv, "--archive-file"))
        || matches_prefix(&argv, &["cargo", "nextest", "list"])
            && !has_option(&argv, "--archive-file")
        || matches_prefix(&argv, &["cargo", "nextest", "archive"]);
    if needs_lock && !has_option(&argv, "--locked") {
        let insert_at = if argv.get(1).map(String::as_str) == Some("nextest") {
            3
        } else {
            2
        };
        argv.insert(insert_at, "--locked".to_string());
    }
    argv
}

fn set_once(slot: &mut Option<String>, value: &str, option: &str) -> Result<(), String> {
    if slot.replace(value.to_string()).is_some() {
        Err(format!("{option} was supplied more than once"))
    } else {
        Ok(())
    }
}

fn require_nonempty<'a>(value: &'a Option<String>, option: &str) -> Result<&'a str, String> {
    value
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{option} is required and must be nonempty"))
}

fn validate_sha(value: &str) -> Result<(), String> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("expected a full 40-character hexadecimal commit SHA".to_string())
    }
}

fn validate_cache_state(cache_state: &str, cargo_build_bytes: u64) -> Result<(), String> {
    match (cache_state, cargo_build_bytes) {
        ("cold", 0) | ("warm", 1..) | ("mixed" | "unknown", _) => Ok(()),
        ("cold", bytes) => Err(format!(
            "cache state `cold` requires zero Cargo build bytes before the command; found {bytes} bytes"
        )),
        ("warm", 0) => Err(
            "cache state `warm` requires existing Cargo build outputs before the command; found 0 bytes"
                .to_string(),
        ),
        (other, _) => Err(format!("unsupported cache state `{other}`")),
    }
}

fn observed_cache_state(cargo_build_bytes: u64) -> &'static str {
    if cargo_build_bytes == 0 {
        "cold"
    } else {
        "warm"
    }
}

fn same_identities(before: &RepoIdentities, after: &RepoIdentities) -> bool {
    before.vigil == after.vigil
        && before.context_graph == after.context_graph
        && before.contextdb == after.contextdb
}

fn change_command(root: &Path, shape: &str, filter: &str) -> Result<Vec<String>, String> {
    let registry = fs::read_to_string(root.join(".config/nextest-inventories.toml"))
        .map_err(|error| format!("read nextest shape registry: {error}"))?;
    let registry: toml::Value = toml::from_str(&registry)
        .map_err(|error| format!("parse nextest shape registry: {error}"))?;
    let shapes = registry
        .get("shape")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| "nextest shape registry has no [[shape]] entries".to_string())?;
    let registered = shapes
        .iter()
        .find(|entry| entry.get("name").and_then(toml::Value::as_str) == Some(shape))
        .ok_or_else(|| format!("nextest shape `{shape}` is not registered"))?;
    let list_command = registered
        .get("command")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| format!("nextest shape `{shape}` has no command"))?;
    nextest_run_from_list(list_command, filter)
}

fn nextest_run_from_list(command: &str, filter: &str) -> Result<Vec<String>, String> {
    let tokens: Vec<&str> = command.split_ascii_whitespace().collect();
    if tokens.get(0..3) != Some(&["cargo", "nextest", "list"]) {
        return Err(format!(
            "registered shape command is not a cargo nextest list command: `{command}`"
        ));
    }
    let mut argv = vec![
        "cargo".to_string(),
        "nextest".to_string(),
        "run".to_string(),
    ];
    let mut index = 3;
    while index < tokens.len() {
        match tokens[index] {
            "-T" | "--message-format" => index += 2,
            token if token.starts_with("--message-format=") => index += 1,
            token => {
                argv.push(token.to_string());
                index += 1;
            }
        }
    }
    argv.extend([
        "--profile".to_string(),
        "pr".to_string(),
        "--ignore-default-filter".to_string(),
        "-E".to_string(),
        filter.to_string(),
        "--no-tests".to_string(),
        "fail".to_string(),
        "--test-threads".to_string(),
        DEFAULT_TEST_THREADS.to_string(),
    ]);
    Ok(ensure_locked(argv))
}

fn bound_nextest(argv: Vec<String>) -> Vec<String> {
    let is_nextest_run = argv.first().map(String::as_str) == Some("cargo")
        && argv.get(1).map(String::as_str) == Some("nextest")
        && argv.get(2).map(String::as_str) == Some("run");
    if !is_nextest_run {
        return argv;
    }
    let mut bounded = Vec::with_capacity(argv.len() + 2);
    let mut values = argv.into_iter();
    while let Some(argument) = values.next() {
        if argument == "-j" || argument == "--test-threads" {
            values.next();
        } else if argument.starts_with("--test-threads=")
            || (argument.starts_with("-j") && argument.len() > 2)
        {
            continue;
        } else {
            bounded.push(argument);
        }
    }
    bounded.extend([
        "--test-threads".to_string(),
        DEFAULT_TEST_THREADS.to_string(),
    ]);
    bounded
}

fn validate_lane_command(tier: Tier, lane: &str, argv: &[String]) -> Result<(), String> {
    if tier == Tier::DevCloseout && lane != "install-binary" && has_option(argv, "--target") {
        return Err(format!(
            "dev-closeout lane `{lane}` must not perform a cross-target build; static and artifact builds belong to release"
        ));
    }
    if tier == Tier::DevCloseout
        && !matches!(lane, "slow-preflight" | "slow-shard" | "install-binary")
        && has_option(argv, "--release")
    {
        return Err(format!(
            "dev-closeout lane `{lane}` must not use a release build; the release-profile test archive belongs only to slow-preflight"
        ));
    }
    let accepted = match (tier, lane) {
        (Tier::DevCloseout, "discipline") => {
            matches_prefix(argv, &["cargo", "fmt"])
                || matches_prefix(argv, &["cargo", "clippy"])
                || matches_prefix(argv, &["cargo", "xtask", "test-estate-check"])
        }
        (Tier::DevCloseout, "default") => {
            (matches_prefix(argv, &["cargo", "nextest", "run"])
                || matches_prefix(argv, &["cargo", "nextest", "list"]))
                && has_option(argv, "--workspace")
                && !has_option(argv, "--features")
                && !has_short_option(argv, "-F")
                && !has_option(argv, "--all-features")
        }
        (Tier::DevCloseout, "feature") => {
            (matches_prefix(argv, &["cargo", "nextest", "run"])
                || matches_prefix(argv, &["cargo", "nextest", "list"]))
                && has_option(argv, "--features")
                && !has_option(argv, "--all-features")
        }
        (Tier::DevCloseout, "slow-preflight") => {
            matches_prefix(argv, &["cargo", "nextest", "list"])
                || is_exact_slow_archive(argv)
                || matches_prefix(argv, &["cargo", "xtask", "setup-harness"])
                || matches_prefix(argv, &["cargo", "xtask", "test-estate-check"])
        }
        (Tier::DevCloseout, "slow-shard") => is_exact_slow_shard(argv),
        (Tier::DevCloseout, "install-binary") => {
            matches_prefix(argv, &["cargo", "build"])
                && has_option(argv, "--release")
                && option_values(argv, "--package", "-p") == ["vigil-bin"]
                && option_values(argv, "--target", "") == ["x86_64-unknown-linux-musl"]
                && option_values(argv, "--features", "") == ["fabric"]
                && !has_option(argv, "--all-features")
        }
        (Tier::DevCloseout, "install-image") => {
            let is_build = matches_prefix(argv, &["docker", "build"]);
            let local_command = is_build
                || matches_prefix(argv, &["docker", "image", "inspect"])
                || matches_prefix(argv, &["docker", "save"])
                || matches_prefix(argv, &["docker", "load"]);
            local_command
                && !matches_prefix(argv, &["docker", "buildx"])
                && !has_option(argv, "--push")
                && !has_option(argv, "--platform")
                && (!is_build
                    || (!has_option(argv, "--output") && !argv.iter().any(|arg| arg == "-o")))
        }
        (Tier::DevCloseout, "install-smoke") => is_exact_install_smoke(argv),
        (Tier::Release, "source") => {
            matches_prefix(argv, &["cargo", "test"])
                || matches_prefix(argv, &["cargo", "nextest", "run"])
        }
        (Tier::Release, "static") => {
            matches_prefix(argv, &["cargo", "build"])
                && has_option(argv, "--release")
                && has_option(argv, "--target")
        }
        (Tier::Release, "artifact-amd64" | "artifact-arm64" | "image-install") => {
            matches_prefix(argv, &["docker", "buildx", "build"])
        }
        (Tier::Release, "reproducibility") => {
            matches_prefix(argv, &["cargo", "build"])
                || matches_prefix(argv, &["docker", "buildx", "build"])
                || matches_prefix(argv, &["sha256sum"])
        }
        _ => false,
    };
    if accepted {
        Ok(())
    } else {
        Err(format!(
            "command `{}` does not satisfy the registered `{lane}` lane contract: {}",
            argv.join(" "),
            lane_command_contract(tier, lane).unwrap_or("unregistered lane")
        ))
    }
}

fn lane_command_contract(tier: Tier, lane: &str) -> Option<&'static str> {
    match (tier, lane) {
        (Tier::DevCloseout, "discipline") => {
            Some("cargo fmt, cargo clippy, or cargo xtask test-estate-check")
        }
        (Tier::DevCloseout, "default") => {
            Some("cargo nextest list/run for the default workspace without feature flags")
        }
        (Tier::DevCloseout, "feature") => {
            Some("cargo nextest list/run with an explicit feature set")
        }
        (Tier::DevCloseout, "slow-preflight") => {
            Some("nextest inventory/archive, harness setup, or test-estate cross-check")
        }
        (Tier::DevCloseout, "slow-shard") => Some(
            "cargo nextest run from the frozen archive with workspace remap, ci-full profile, and hash:m/4 partition",
        ),
        (Tier::DevCloseout, "install-binary") => Some(
            "cargo build -p vigil-bin --release --target x86_64-unknown-linux-musl --features fabric",
        ),
        (Tier::DevCloseout, "install-image") => {
            Some("local docker build/image inspect/save/load without buildx, platform, or push")
        }
        (Tier::DevCloseout, "install-smoke") => Some(
            "cargo nextest run from the frozen archive with only test(vigil_container_) selected",
        ),
        (Tier::Release, "source") => Some("cargo test or nextest source qualification"),
        (Tier::Release, "static") => Some("release cargo build for an explicit target"),
        (Tier::Release, "artifact-amd64" | "artifact-arm64" | "image-install") => {
            Some("docker buildx build")
        }
        (Tier::Release, "reproducibility") => {
            Some("cargo build, docker buildx build, or sha256sum")
        }
        _ => None,
    }
}

fn matches_prefix(argv: &[String], prefix: &[&str]) -> bool {
    argv.iter()
        .map(String::as_str)
        .zip(prefix.iter().copied())
        .all(|(a, b)| a == b)
        && argv.len() >= prefix.len()
}

fn has_option(argv: &[String], option: &str) -> bool {
    argv.iter()
        .any(|argument| argument == option || argument.starts_with(&format!("{option}=")))
}

fn has_short_option(argv: &[String], option: &str) -> bool {
    argv.iter()
        .any(|argument| argument == option || argument.starts_with(option))
}

fn is_exact_slow_archive(argv: &[String]) -> bool {
    argv.iter().map(String::as_str).eq([
        "cargo",
        "nextest",
        "archive",
        "--locked",
        "--release",
        "--workspace",
        "--features",
        "first-light-acceptance,acceptance",
        "--archive-file",
        "target/closeout/nextest-slow.tar.zst",
    ])
}

fn is_exact_slow_shard(argv: &[String]) -> bool {
    if argv.len() != 13 || !matches_prefix(argv, &["cargo", "nextest", "run"]) {
        return false;
    }
    let archive = option_values(argv, "--archive-file", "");
    let remap = option_values(argv, "--workspace-remap", "");
    let profile = option_values(argv, "--profile", "-P");
    let partition = option_values(argv, "--partition", "");
    let filter = option_values(argv, "--filter-expr", "-E");
    archive == ["target/closeout/nextest-slow.tar.zst"]
        && remap.len() == 1
        && Path::new(remap[0]).is_absolute()
        && profile == ["ci-full"]
        && partition.len() == 1
        && is_four_way_hash_partition(partition[0])
        && filter == ["not test(vigil_container_)"]
}

fn is_exact_install_smoke(argv: &[String]) -> bool {
    if argv.len() != 11 || !matches_prefix(argv, &["cargo", "nextest", "run"]) {
        return false;
    }
    let archive = option_values(argv, "--archive-file", "");
    let remap = option_values(argv, "--workspace-remap", "");
    let profile = option_values(argv, "--profile", "-P");
    let filter = option_values(argv, "--filter-expr", "-E");
    archive == ["target/closeout/nextest-slow.tar.zst"]
        && remap.len() == 1
        && Path::new(remap[0]).is_absolute()
        && profile == ["ci-full"]
        && filter == ["test(vigil_container_)"]
}

fn is_four_way_hash_partition(value: &str) -> bool {
    let Some((member, total)) = value
        .strip_prefix("hash:")
        .and_then(|value| value.split_once('/'))
    else {
        return false;
    };
    total == "4" && matches!(member.parse::<u8>(), Ok(1..=4))
}

fn option_values<'a>(argv: &'a [String], long: &str, short: &str) -> Vec<&'a str> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < argv.len() {
        if argv[index] == long || (!short.is_empty() && argv[index] == short) {
            if let Some(value) = argv.get(index + 1) {
                values.push(value.as_str());
            }
            index += 2;
        } else if let Some(value) = argv[index].strip_prefix(&format!("{long}=")) {
            values.push(value);
            index += 1;
        } else {
            index += 1;
        }
    }
    values
}

fn run_step(
    root: &Path,
    argv: Vec<String>,
    stdout_path: Option<&Path>,
) -> Result<StepReceipt, String> {
    let program = argv
        .first()
        .ok_or_else(|| "verification step has no program".to_string())?;
    println!("verification step: {}", argv.join(" "));
    let started = Instant::now();
    let mut command = Command::new(program);
    command.args(&argv[1..]).current_dir(root);
    command.env("CARGO_BUILD_JOBS", DEFAULT_BUILD_JOBS);
    if let Some(path) = stdout_path {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create verification output directory: {error}"))?;
        }
        let file = fs::File::create(path)
            .map_err(|error| format!("create verification output {}: {error}", path.display()))?;
        command.stdout(Stdio::from(file));
    }
    let measurement = measured_status(&mut command)
        .map_err(|error| format!("run verification step `{program}`: {error}"))?;
    let status = measurement.status;
    Ok(StepReceipt {
        argv,
        status: classify_status(status).to_string(),
        exit_code: status.code(),
        wall_ms: millis(started.elapsed()),
        cpu_user_ms: measurement.cpu_user_ms,
        cpu_system_ms: measurement.cpu_system_ms,
        max_rss_bytes: measurement.max_rss_bytes,
        sampled_min_available_memory_bytes: measurement.sampled_min_available_memory_bytes,
        sampled_process_tree_peak_rss_bytes: measurement.sampled_process_tree_peak_rss_bytes,
        measurement_status: measurement.measurement_status,
    })
}

struct StepMeasurement {
    status: ExitStatus,
    cpu_user_ms: Option<u64>,
    cpu_system_ms: Option<u64>,
    max_rss_bytes: Option<u64>,
    sampled_min_available_memory_bytes: Option<u64>,
    sampled_process_tree_peak_rss_bytes: Option<u64>,
    measurement_status: String,
}

#[cfg(target_os = "linux")]
fn measured_status(command: &mut Command) -> io::Result<StepMeasurement> {
    use std::os::unix::process::ExitStatusExt;

    let child = command.spawn()?;
    let pid = child.id() as libc::pid_t;
    let done = Arc::new(AtomicBool::new(false));
    let monitor_done = Arc::clone(&done);
    let monitor = thread::spawn(move || sample_process_tree(pid, monitor_done));

    let mut raw_status = 0;
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    let waited = unsafe { libc::wait4(pid, &mut raw_status, 0, usage.as_mut_ptr()) };
    let wait_error = io::Error::last_os_error();
    done.store(true, Ordering::Release);
    let samples = monitor.join().unwrap_or_default();
    if waited < 0 {
        return Err(wait_error);
    }
    // wait4 reaped the process. Child has no cleanup work left to do.
    drop(child);
    let usage = unsafe { usage.assume_init() };
    Ok(StepMeasurement {
        status: ExitStatus::from_raw(raw_status),
        cpu_user_ms: Some(timeval_ms(usage.ru_utime)),
        cpu_system_ms: Some(timeval_ms(usage.ru_stime)),
        max_rss_bytes: u64::try_from(usage.ru_maxrss)
            .ok()
            .map(|kib| kib.saturating_mul(1024)),
        sampled_min_available_memory_bytes: samples.min_available_memory_bytes,
        sampled_process_tree_peak_rss_bytes: samples.process_tree_peak_rss_bytes,
        measurement_status: if samples.sample_count == 0 {
            "rusage_complete; process_tree_sampling_unavailable".to_string()
        } else {
            format!(
                "rusage_complete; process_tree_samples={}",
                samples.sample_count
            )
        },
    })
}

#[cfg(not(target_os = "linux"))]
fn measured_status(command: &mut Command) -> io::Result<StepMeasurement> {
    Ok(StepMeasurement {
        status: command.status()?,
        cpu_user_ms: None,
        cpu_system_ms: None,
        max_rss_bytes: None,
        sampled_min_available_memory_bytes: None,
        sampled_process_tree_peak_rss_bytes: None,
        measurement_status: "unsupported_platform".to_string(),
    })
}

#[cfg(target_os = "linux")]
fn timeval_ms(value: libc::timeval) -> u64 {
    u64::try_from(value.tv_sec)
        .unwrap_or(0)
        .saturating_mul(1_000)
        .saturating_add(u64::try_from(value.tv_usec).unwrap_or(0) / 1_000)
}

#[derive(Default)]
struct ProcessSamples {
    min_available_memory_bytes: Option<u64>,
    process_tree_peak_rss_bytes: Option<u64>,
    sample_count: u64,
}

fn sample_process_tree(root_pid: libc::pid_t, done: Arc<AtomicBool>) -> ProcessSamples {
    let mut samples = ProcessSamples::default();
    loop {
        if let Ok(memory) = read_memory() {
            samples.min_available_memory_bytes = Some(
                samples
                    .min_available_memory_bytes
                    .map_or(memory.available_bytes, |prior| {
                        prior.min(memory.available_bytes)
                    }),
            );
        }
        if let Some(rss) = process_tree_rss(root_pid) {
            samples.process_tree_peak_rss_bytes = Some(
                samples
                    .process_tree_peak_rss_bytes
                    .map_or(rss, |prior| prior.max(rss)),
            );
        }
        samples.sample_count = samples.sample_count.saturating_add(1);
        if done.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    samples
}

fn process_tree_rss(root_pid: libc::pid_t) -> Option<u64> {
    let mut processes = Vec::new();
    for entry in fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else { continue };
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Ok(pid) = file_name.parse::<libc::pid_t>() else {
            continue;
        };
        if let Some((parent, rss)) = process_identity(pid) {
            processes.push((pid, parent, rss));
        }
    }
    let mut tree = vec![root_pid];
    loop {
        let prior = tree.len();
        for (pid, parent, _) in &processes {
            if tree.contains(parent) && !tree.contains(pid) {
                tree.push(*pid);
            }
        }
        if tree.len() == prior {
            break;
        }
    }
    Some(
        processes
            .iter()
            .filter(|(pid, _, _)| tree.contains(pid))
            .map(|(_, _, rss)| *rss)
            .sum(),
    )
}

fn process_identity(pid: libc::pid_t) -> Option<(libc::pid_t, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(") ")?.1;
    let parent = after_name
        .split_ascii_whitespace()
        .nth(1)?
        .parse::<libc::pid_t>()
        .ok()?;
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let rss = status.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")?
            .trim()
            .strip_suffix(" kB")?
            .trim()
            .parse::<u64>()
            .ok()
            .map(|kib| kib.saturating_mul(1024))
    })?;
    Some((parent, rss))
}

fn classify_status(status: ExitStatus) -> &'static str {
    match status.code() {
        Some(0) => "passed",
        Some(NEXTTEST_TEST_FAILED) => "test_failed",
        Some(NEXTTEST_BUILD_FAILED) => "build_failed",
        Some(_) => "command_failed",
        None => "terminated",
    }
}

fn display_code(code: Option<i32>) -> String {
    code.map(|code| format!("exit {code}"))
        .unwrap_or_else(|| "termination by signal".to_string())
}

enum ResourceLock {
    Inherited(libc::c_int),
}

struct LauncherMetrics {
    queue_wait_ms: u64,
    bootstrap_wall_ms: u64,
    bootstrap_max_rss_bytes: u64,
    bootstrap_target_bytes_before: u64,
    bootstrap_target_bytes_after: u64,
    bootstrap_disk_available_before: u64,
    bootstrap_disk_available_after: u64,
}

impl ResourceLock {
    #[cfg(target_os = "linux")]
    fn acquire(root: &Path) -> Result<(Self, Duration, LauncherMetrics), String> {
        let fd = env_u64("VIGIL_VERIFY_LOCK_FD")? as libc::c_int;
        let common_dir = command_stdout(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .arg("rev-parse")
                .arg("--git-common-dir"),
            "locate Vigil git common directory",
        )?;
        let common_dir = PathBuf::from(common_dir.trim());
        let common_dir = if common_dir.is_absolute() {
            common_dir
        } else {
            root.join(common_dir)
        };
        let expected_lock = fs::canonicalize(common_dir.join("vigil-verify.lock"))
            .map_err(|error| format!("resolve expected Vigil resource lease: {error}"))?;
        let inherited_lock = fs::canonicalize(format!("/proc/self/fd/{fd}"))
            .map_err(|error| format!("resolve inherited Vigil resource lease: {error}"))?;
        if inherited_lock != expected_lock
            || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0
            || unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0
        {
            return Err(
                "verification requires the inherited repository lease; run `scripts/verify`, not the xtask binary directly"
                    .to_string(),
            );
        }
        let queue_wait_ms = env_u64("VIGIL_VERIFY_QUEUE_WAIT_MS")?;
        let bootstrap_wall_ms = env_u64("VIGIL_VERIFY_BOOTSTRAP_WALL_MS")?;
        let bootstrap_max_rss_bytes = env_u64("VIGIL_VERIFY_BOOTSTRAP_MAX_RSS_BYTES")?;
        println!("compile resource acquired after {queue_wait_ms} ms");
        Ok((
            Self::Inherited(fd),
            Duration::from_millis(queue_wait_ms),
            LauncherMetrics {
                queue_wait_ms,
                bootstrap_wall_ms,
                bootstrap_max_rss_bytes,
                bootstrap_target_bytes_before: env_u64(
                    "VIGIL_VERIFY_BOOTSTRAP_TARGET_BYTES_BEFORE",
                )?,
                bootstrap_target_bytes_after: env_u64("VIGIL_VERIFY_BOOTSTRAP_TARGET_BYTES_AFTER")?,
                bootstrap_disk_available_before: env_u64(
                    "VIGIL_VERIFY_BOOTSTRAP_DISK_AVAILABLE_BEFORE",
                )?,
                bootstrap_disk_available_after: env_u64(
                    "VIGIL_VERIFY_BOOTSTRAP_DISK_AVAILABLE_AFTER",
                )?,
            },
        ))
    }

    #[cfg(not(target_os = "linux"))]
    fn acquire(_root: &Path) -> Result<(Self, Duration, LauncherMetrics), String> {
        Err("scripts/verify currently requires Linux advisory locking".to_string())
    }
}

impl Drop for ResourceLock {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        unsafe {
            let Self::Inherited(fd) = self;
            libc::flock(*fd, libc::LOCK_UN);
            libc::close(*fd);
        }
    }
}

// Reads an operator-declared verification knob by name; xtask itself is the
// build/verification tool, not a product surface the settings-declaration
// guard covers.
#[allow(clippy::disallowed_methods)]
fn env_u64(name: &str) -> Result<u64, String> {
    env::var(name)
        .map_err(|_| format!("{name} is missing; run verification through `scripts/verify`"))?
        .parse::<u64>()
        .map_err(|_| format!("{name} must be an unsigned integer"))
}

#[derive(Debug)]
struct RepoIdentities {
    vigil: RepoIdentity,
    context_graph: RepoIdentity,
    contextdb: RepoIdentity,
}

impl RepoIdentities {
    fn read(root: &Path) -> Result<Self, String> {
        let parent = root
            .parent()
            .ok_or_else(|| "Vigil workspace has no parent for sibling repositories".to_string())?;
        Ok(Self {
            vigil: RepoIdentity::read(root)?,
            context_graph: RepoIdentity::read(&parent.join("context-graph"))?,
            contextdb: RepoIdentity::read(&parent.join("contextdb"))?,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
struct RepoIdentity {
    sha: String,
    dirty: bool,
}

impl RepoIdentity {
    fn read(path: &Path) -> Result<Self, String> {
        let sha = command_stdout(
            Command::new("git")
                .arg("-C")
                .arg(path)
                .arg("rev-parse")
                .arg("HEAD"),
            &format!("read git SHA for {}", path.display()),
        )?;
        let sha = sha.trim().to_string();
        validate_sha(&sha)
            .map_err(|error| format!("invalid git SHA for {}: {error}", path.display()))?;
        let dirty = !command_stdout(
            Command::new("git")
                .arg("-C")
                .arg(path)
                .arg("status")
                .arg("--porcelain=v1"),
            &format!("read dirty state for {}", path.display()),
        )?
        .trim()
        .is_empty();
        Ok(Self { sha, dirty })
    }
}

fn command_stdout(command: &mut Command, label: &str) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("{label}: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[derive(Serialize)]
struct Receipt<'a> {
    schema_version: u32,
    tier: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    lane: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_contract: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shape: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filter: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expectation: Option<&'a str>,
    vigil_sha: &'a str,
    context_graph_sha: &'a str,
    contextdb_sha: &'a str,
    dirty: DirtyState,
    dirty_after: DirtyState,
    source_unchanged: bool,
    cache_state: &'a str,
    observed_cache_state: &'a str,
    steps: Vec<StepReceipt>,
    total_wall_ms: u64,
    queue_wait_ms: u64,
    bootstrap_wall_ms: u64,
    bootstrap_max_rss_bytes: u64,
    bootstrap_target_bytes_before: u64,
    bootstrap_target_bytes_after: u64,
    bootstrap_disk_available_before: u64,
    bootstrap_disk_available_after: u64,
    resources_before: ResourceSnapshot,
    resources_after: ResourceSnapshot,
    rustc_version: Option<String>,
    cargo_version: Option<String>,
    cargo_nextest_version: Option<String>,
    kernel_release: Option<String>,
    architecture: String,
    environment_measurement_status: Vec<String>,
    cpu_count: usize,
    effective_cargo_build_jobs: &'static str,
    effective_nextest_test_threads: &'static str,
    receipt_path: String,
}

#[derive(Serialize)]
struct DirtyState {
    vigil: bool,
    context_graph: bool,
    contextdb: bool,
}

#[derive(Debug, Serialize)]
struct StepReceipt {
    argv: Vec<String>,
    status: String,
    exit_code: Option<i32>,
    wall_ms: u64,
    cpu_user_ms: Option<u64>,
    cpu_system_ms: Option<u64>,
    max_rss_bytes: Option<u64>,
    sampled_min_available_memory_bytes: Option<u64>,
    sampled_process_tree_peak_rss_bytes: Option<u64>,
    measurement_status: String,
}

#[derive(Debug, Serialize, Eq, PartialEq)]
struct Memory {
    available_bytes: u64,
    total_bytes: u64,
}

#[derive(Debug, Serialize, Eq, PartialEq)]
struct ResourceSnapshot {
    memory: Memory,
    target_bytes: u64,
    cargo_build_bytes: u64,
    disk_available_bytes: u64,
}

impl ResourceSnapshot {
    fn read(root: &Path) -> Result<Self, String> {
        let target = root.join("target");
        Ok(Self {
            memory: read_memory()?,
            target_bytes: directory_bytes(&target)?,
            cargo_build_bytes: cargo_build_bytes(&target)?,
            disk_available_bytes: disk_available(root)?,
        })
    }
}

fn cargo_build_bytes(target: &Path) -> Result<u64, String> {
    let entries = match fs::read_dir(target) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("read {}: {error}", target.display())),
    };
    let mut total = 0_u64;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {} entry: {error}", target.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("stat {}: {error}", entry.path().display()))?;
        if file_type.is_dir() && is_cargo_build_directory(&entry.file_name().to_string_lossy()) {
            total = total.saturating_add(directory_bytes(&entry.path())?);
        }
    }
    Ok(total)
}

fn is_cargo_build_directory(name: &str) -> bool {
    matches!(name, "debug" | "release")
        || (name.contains('-')
            && !name.starts_with('.')
            && !matches!(name, "vigil-test-tools" | "verify-receipts" | "closeout"))
}

struct EnvironmentReceipt {
    rustc_version: Option<String>,
    cargo_version: Option<String>,
    cargo_nextest_version: Option<String>,
    kernel_release: Option<String>,
    architecture: String,
    measurement_status: Vec<String>,
}

impl EnvironmentReceipt {
    fn read() -> Self {
        let mut measurement_status = Vec::new();
        let rustc_version = optional_version(
            Command::new("rustc").arg("--version"),
            "rustc",
            &mut measurement_status,
        );
        let cargo_version = optional_version(
            Command::new("cargo").arg("--version"),
            "cargo",
            &mut measurement_status,
        );
        let cargo_nextest_version = optional_version(
            Command::new("cargo").arg("nextest").arg("--version"),
            "cargo-nextest",
            &mut measurement_status,
        );
        let kernel_release = match fs::read_to_string("/proc/sys/kernel/osrelease") {
            Ok(value) => Some(value.trim().to_string()),
            Err(error) => {
                measurement_status.push(format!("kernel-release unavailable: {error}"));
                None
            }
        };
        Self {
            rustc_version,
            cargo_version,
            cargo_nextest_version,
            kernel_release,
            architecture: env::consts::ARCH.to_string(),
            measurement_status,
        }
    }
}

fn optional_version(
    command: &mut Command,
    name: &str,
    measurement_status: &mut Vec<String>,
) -> Option<String> {
    match command.output() {
        Ok(output) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(output) => {
            measurement_status.push(format!(
                "{name} unavailable: exit {}: {}",
                output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_string(), |code| code.to_string()),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
            None
        }
        Err(error) => {
            measurement_status.push(format!("{name} unavailable: {error}"));
            None
        }
    }
}

fn read_memory() -> Result<Memory, String> {
    parse_meminfo(
        &fs::read_to_string("/proc/meminfo")
            .map_err(|error| format!("read /proc/meminfo: {error}"))?,
    )
}

fn parse_meminfo(content: &str) -> Result<Memory, String> {
    fn kib(content: &str, key: &str) -> Option<u64> {
        content.lines().find_map(|line| {
            let value = line.strip_prefix(key)?.trim();
            value.strip_suffix(" kB")?.trim().parse::<u64>().ok()
        })
    }
    Ok(Memory {
        available_bytes: kib(content, "MemAvailable:")
            .ok_or_else(|| "/proc/meminfo has no MemAvailable".to_string())?
            .saturating_mul(1024),
        total_bytes: kib(content, "MemTotal:")
            .ok_or_else(|| "/proc/meminfo has no MemTotal".to_string())?
            .saturating_mul(1024),
    })
}

fn directory_bytes(path: &Path) -> Result<u64, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("stat {}: {error}", path.display())),
    };
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut total = 0_u64;
    for entry in fs::read_dir(path).map_err(|error| format!("read {}: {error}", path.display()))? {
        let entry = entry.map_err(|error| format!("read {} entry: {error}", path.display()))?;
        total = total.saturating_add(directory_bytes(&entry.path())?);
    }
    Ok(total)
}

#[cfg(target_os = "linux")]
fn disk_available(path: &Path) -> Result<u64, String> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("disk path contains NUL: {}", path.display()))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return Err(format!("statvfs: {}", io::Error::last_os_error()));
    }
    let stats = unsafe { stats.assume_init() };
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

#[cfg(not(target_os = "linux"))]
fn disk_available(_path: &Path) -> Result<u64, String> {
    Err("disk availability reporting currently requires Linux".to_string())
}

fn write_receipt(path: &Path, receipt: &Receipt<'_>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create receipt directory {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(receipt)
        .map_err(|error| format!("serialize verification receipt: {error}"))?;
    fs::write(path, bytes).map_err(|error| format!("write receipt {}: {error}", path.display()))
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        env::current_dir()
            .map(|current| current.join(path))
            .map_err(|error| format!("read current directory: {error}"))
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn change_requires_registered_shape_filter_expectation_and_cache_state() {
        let parsed = Options::parse(&args(&[
            "change",
            "--shape",
            "default",
            "--filter",
            "test(exact_test)",
            "--expect",
            "red",
            "--cache-state",
            "warm",
        ]))
        .expect("valid change options");
        assert_eq!(parsed.tier, Tier::Change);
        assert_eq!(parsed.expect, Some(Expectation::Red));
        assert!(
            Options::parse(&args(&[
                "change",
                "--shape",
                "default",
                "--expect",
                "red",
                "--cache-state",
                "warm",
            ]))
            .unwrap_err()
            .contains("--filter")
        );
    }

    #[test]
    fn closeout_requires_full_shas_registered_lane_and_command() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let parsed = Options::parse(&args(&[
            "dev-closeout",
            "--lane",
            "default",
            "--vigil-sha",
            sha,
            "--context-graph-sha",
            sha,
            "--contextdb-sha",
            sha,
            "--cache-state",
            "cold",
            "--",
            "cargo",
            "nextest",
            "run",
            "--workspace",
        ]))
        .expect("valid closeout options");
        assert_eq!(parsed.lane.as_deref(), Some("default"));
        assert!(
            Options::parse(&args(&[
                "dev-closeout",
                "--lane",
                "unknown",
                "--vigil-sha",
                sha,
                "--context-graph-sha",
                sha,
                "--contextdb-sha",
                sha,
                "--cache-state",
                "cold",
                "--",
                "true",
            ]))
            .unwrap_err()
            .contains("not registered")
        );
    }

    #[test]
    fn closeout_lane_rejects_an_arbitrary_command() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let error = Options::parse(&args(&[
            "dev-closeout",
            "--lane",
            "default",
            "--vigil-sha",
            sha,
            "--context-graph-sha",
            sha,
            "--contextdb-sha",
            sha,
            "--cache-state",
            "warm",
            "--",
            "true",
        ]))
        .unwrap_err();
        assert!(error.contains("does not satisfy"));
    }

    #[test]
    fn dev_lanes_reject_release_and_cross_target_builds() {
        assert!(
            validate_lane_command(
                Tier::DevCloseout,
                "feature",
                &strings(&[
                    "cargo",
                    "nextest",
                    "run",
                    "--features",
                    "fabric",
                    "--release",
                ]),
            )
            .unwrap_err()
            .contains("must not use a release build")
        );
        assert!(
            validate_lane_command(
                Tier::DevCloseout,
                "default",
                &strings(&[
                    "cargo",
                    "nextest",
                    "run",
                    "--workspace",
                    "--target",
                    "x86_64-unknown-linux-musl",
                ]),
            )
            .unwrap_err()
            .contains("cross-target")
        );
        assert!(
            validate_lane_command(
                Tier::DevCloseout,
                "slow-preflight",
                &strings(&[
                    "cargo",
                    "nextest",
                    "archive",
                    "--locked",
                    "--release",
                    "--workspace",
                    "--features",
                    "first-light-acceptance,acceptance",
                    "--archive-file",
                    "target/closeout/nextest-slow.tar.zst",
                ]),
            )
            .is_ok()
        );
        assert!(
            validate_lane_command(
                Tier::DevCloseout,
                "slow-preflight",
                &strings(&["cargo", "nextest", "archive", "--release", "--workspace",]),
            )
            .is_err()
        );
        for lane in DEV_LANES {
            assert!(
                validate_lane_command(
                    Tier::DevCloseout,
                    lane,
                    &strings(&["cargo", "build", "--release"]),
                )
                .is_err(),
                "{lane} accepted a release build"
            );
            assert!(
                validate_lane_command(
                    Tier::DevCloseout,
                    lane,
                    &strings(&["cargo", "build", "--target", "x86_64-unknown-linux-musl",]),
                )
                .is_err(),
                "{lane} accepted a cross-target build"
            );
            assert!(
                validate_lane_command(
                    Tier::DevCloseout,
                    lane,
                    &strings(&["docker", "buildx", "build", "."]),
                )
                .is_err(),
                "{lane} accepted a Docker build"
            );
        }
    }

    #[test]
    fn default_and_feature_lanes_accept_only_their_registered_inventories() {
        assert!(
            validate_lane_command(
                Tier::DevCloseout,
                "default",
                &strings(&["cargo", "nextest", "list", "--workspace"]),
            )
            .is_ok()
        );
        assert!(
            validate_lane_command(
                Tier::DevCloseout,
                "feature",
                &strings(&[
                    "cargo",
                    "nextest",
                    "list",
                    "-p",
                    "vigil",
                    "--features",
                    "fabric",
                ]),
            )
            .is_ok()
        );
        assert!(
            validate_lane_command(
                Tier::DevCloseout,
                "default",
                &strings(&[
                    "cargo",
                    "nextest",
                    "list",
                    "--workspace",
                    "--features=fabric",
                ]),
            )
            .is_err()
        );
        assert!(
            validate_lane_command(
                Tier::DevCloseout,
                "feature",
                &strings(&["cargo", "nextest", "list", "--workspace"]),
            )
            .is_err()
        );
    }

    #[test]
    fn slow_shards_run_only_the_frozen_archive_without_build_selection() {
        let accepted = strings(&[
            "cargo",
            "nextest",
            "run",
            "--archive-file",
            "target/closeout/nextest-slow.tar.zst",
            "--workspace-remap",
            "/workspace/vigil",
            "--profile",
            "ci-full",
            "--partition",
            "hash:2/4",
            "-E",
            "not test(vigil_container_)",
        ]);
        assert!(validate_lane_command(Tier::DevCloseout, "slow-shard", &accepted).is_ok());

        for filter in ["test(vigil_container_)", "all()", "not test(container)"] {
            let mut rejected = accepted.clone();
            let filter_index = rejected
                .iter()
                .position(|argument| argument == "-E")
                .expect("filter option")
                + 1;
            rejected[filter_index] = filter.to_string();
            assert!(
                validate_lane_command(Tier::DevCloseout, "slow-shard", &rejected).is_err(),
                "slow shard accepted filter {filter}"
            );
        }

        for extra in [
            vec!["--release"],
            vec!["--workspace"],
            vec!["--features", "fabric"],
            vec!["-p", "vigil"],
            vec!["--target", "x86_64-unknown-linux-musl"],
        ] {
            let mut rejected = accepted.clone();
            rejected.extend(extra.into_iter().map(str::to_string));
            assert!(
                validate_lane_command(Tier::DevCloseout, "slow-shard", &rejected).is_err(),
                "slow shard accepted build selection: {rejected:?}"
            );
        }
        let relative_remap = strings(&[
            "cargo",
            "nextest",
            "run",
            "--archive-file",
            "target/closeout/nextest-slow.tar.zst",
            "--workspace-remap",
            ".",
            "--profile",
            "ci-full",
            "--partition",
            "hash:2/4",
            "-E",
            "not test(vigil_container_)",
        ]);
        assert!(validate_lane_command(Tier::DevCloseout, "slow-shard", &relative_remap).is_err());
    }

    #[test]
    fn install_smoke_runs_only_container_identities_once_from_the_archive() {
        let accepted = strings(&[
            "cargo",
            "nextest",
            "run",
            "--archive-file",
            "target/closeout/nextest-slow.tar.zst",
            "--workspace-remap",
            "/workspace/vigil",
            "--profile",
            "ci-full",
            "-E",
            "test(vigil_container_)",
        ]);
        assert!(validate_lane_command(Tier::DevCloseout, "install-smoke", &accepted).is_ok());

        for filter in [
            "not test(vigil_container_)",
            "test(vigil_container)",
            "all()",
        ] {
            let mut rejected = accepted.clone();
            let filter_index = rejected
                .iter()
                .position(|argument| argument == "-E")
                .expect("filter option")
                + 1;
            rejected[filter_index] = filter.to_string();
            assert!(
                validate_lane_command(Tier::DevCloseout, "install-smoke", &rejected).is_err(),
                "install smoke accepted filter {filter}"
            );
        }
        for extra in [
            vec!["--partition", "hash:1/4"],
            vec!["--workspace"],
            vec!["--features", "fabric"],
            vec!["--release"],
        ] {
            let mut rejected = accepted.clone();
            rejected.extend(extra.into_iter().map(str::to_string));
            assert!(
                validate_lane_command(Tier::DevCloseout, "install-smoke", &rejected).is_err(),
                "install smoke accepted extra arguments: {rejected:?}"
            );
        }
    }

    #[test]
    fn install_lanes_are_local_smoke_inputs_not_release_artifact_exports() {
        let exact_binary = strings(&[
            "cargo",
            "build",
            "-p",
            "vigil-bin",
            "--release",
            "--target",
            "x86_64-unknown-linux-musl",
            "--features",
            "fabric",
        ]);
        assert!(validate_lane_command(Tier::DevCloseout, "install-binary", &exact_binary).is_ok());
        for rejected in [
            strings(&[
                "cargo",
                "build",
                "-p",
                "vigil-bin",
                "--release",
                "--target",
                "aarch64-unknown-linux-musl",
                "--features",
                "fabric",
            ]),
            strings(&[
                "cargo",
                "build",
                "-p",
                "vigil-bin",
                "--release",
                "--target",
                "x86_64-unknown-linux-musl",
                "--features",
                "decode-gstreamer,detect-burn-wgpu,fabric",
            ]),
            // Core has no binary target at all: a build selecting core's own
            // package name can never produce the shipped `vigil` executable.
            strings(&[
                "cargo",
                "build",
                "-p",
                "vigil",
                "--release",
                "--target",
                "x86_64-unknown-linux-musl",
                "--features",
                "fabric",
            ]),
        ] {
            assert!(validate_lane_command(Tier::DevCloseout, "install-binary", &rejected).is_err());
        }

        for accepted in [
            strings(&["docker", "build", "-t", "vigil:closeout", "."]),
            strings(&["docker", "image", "inspect", "vigil:closeout"]),
            strings(&["docker", "save", "-o", "vigil.tar", "vigil:closeout"]),
            strings(&["docker", "load", "-i", "vigil.tar"]),
        ] {
            assert!(validate_lane_command(Tier::DevCloseout, "install-image", &accepted).is_ok());
        }
        for rejected in [
            strings(&["docker", "buildx", "build", "."]),
            strings(&["docker", "build", "--push", "."]),
            strings(&["docker", "build", "--platform=linux/amd64", "."]),
            strings(&["docker", "build", "--target", "vigil-hw-binary-amd64", "."]),
            strings(&[
                "docker",
                "build",
                "--output",
                "type=oci,dest=vigil.tar",
                ".",
            ]),
        ] {
            assert!(validate_lane_command(Tier::DevCloseout, "install-image", &rejected).is_err());
        }
    }

    #[test]
    fn closeout_rejects_dirty_dependency_inputs() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let options = Options::parse(&args(&[
            "dev-closeout",
            "--lane",
            "default",
            "--vigil-sha",
            sha,
            "--context-graph-sha",
            sha,
            "--contextdb-sha",
            sha,
            "--cache-state",
            "warm",
            "--",
            "cargo",
            "nextest",
            "run",
            "--workspace",
        ]))
        .expect("valid closeout options");
        let identities = RepoIdentities {
            vigil: RepoIdentity {
                sha: sha.to_string(),
                dirty: false,
            },
            context_graph: RepoIdentity {
                sha: sha.to_string(),
                dirty: true,
            },
            contextdb: RepoIdentity {
                sha: sha.to_string(),
                dirty: false,
            },
        };
        assert!(
            options
                .validate_identities(&identities)
                .unwrap_err()
                .contains("Context Graph")
        );
        let clean = RepoIdentities {
            vigil: RepoIdentity {
                sha: sha.to_string(),
                dirty: false,
            },
            context_graph: RepoIdentity {
                sha: sha.to_string(),
                dirty: false,
            },
            contextdb: RepoIdentity {
                sha: sha.to_string(),
                dirty: false,
            },
        };
        assert!(
            options
                .validate_unchanged(&clean, &identities)
                .unwrap_err()
                .contains("changed a source commit or working tree")
        );
    }

    #[test]
    fn registered_list_command_becomes_bounded_filtered_run() {
        let command = nextest_run_from_list(
            "cargo nextest list -p vigil --features fabric -T json",
            "test(my_test)",
        )
        .expect("convert command");
        assert_eq!(
            command,
            args(&[
                "cargo",
                "nextest",
                "run",
                "--locked",
                "-p",
                "vigil",
                "--features",
                "fabric",
                "--profile",
                "pr",
                "--ignore-default-filter",
                "-E",
                "test(my_test)",
                "--no-tests",
                "fail",
                "--test-threads",
                "2",
            ])
            .into_iter()
            .map(|value| value.into_string().expect("UTF-8"))
            .collect::<Vec<_>>()
        );
        for command in [
            strings(&["cargo", "build", "--workspace"]),
            strings(&["cargo", "check", "--workspace"]),
            strings(&["cargo", "test", "--workspace"]),
            strings(&["cargo", "nextest", "list", "--workspace"]),
            strings(&["cargo", "nextest", "run", "--workspace"]),
            strings(&["cargo", "nextest", "archive", "--workspace"]),
        ] {
            let locked = ensure_locked(command);
            assert_eq!(
                locked
                    .iter()
                    .filter(|argument| *argument == "--locked")
                    .count(),
                1,
                "source-building verification command was not locked: {locked:?}"
            );
        }
        let already_locked = ensure_locked(strings(&["cargo", "test", "--locked", "--workspace"]));
        assert_eq!(
            already_locked
                .iter()
                .filter(|argument| *argument == "--locked")
                .count(),
            1
        );
    }

    #[test]
    fn arbitrary_nextest_lane_is_forcibly_bounded_once() {
        let command = bound_nextest(
            vec!["cargo", "nextest", "run", "--workspace"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        );
        assert_eq!(&command[command.len() - 2..], ["--test-threads", "2"]);
        let previously_unbounded = bound_nextest(
            vec!["cargo", "nextest", "run", "-j", "40"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        );
        assert!(!previously_unbounded.iter().any(|arg| arg == "-j"));
        assert_eq!(
            &previously_unbounded[previously_unbounded.len() - 2..],
            ["--test-threads", "2"]
        );
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn red_accepts_only_nextest_test_failure_not_build_failure() {
        let mut options = Options::parse(&args(&[
            "change",
            "--shape",
            "default",
            "--filter",
            "test(x)",
            "--expect",
            "red",
            "--cache-state",
            "warm",
        ]))
        .expect("options");
        let mut step = StepReceipt {
            argv: Vec::new(),
            status: "test_failed".to_string(),
            exit_code: Some(NEXTTEST_TEST_FAILED),
            wall_ms: 1,
            cpu_user_ms: Some(1),
            cpu_system_ms: Some(1),
            max_rss_bytes: Some(1),
            sampled_min_available_memory_bytes: Some(1),
            sampled_process_tree_peak_rss_bytes: Some(1),
            measurement_status: "test".to_string(),
        };
        assert!(options.verdict(&step).is_ok());
        step.exit_code = Some(NEXTTEST_BUILD_FAILED);
        assert!(
            options
                .verdict(&step)
                .unwrap_err()
                .contains("build failure")
        );
        options.expect = Some(Expectation::Green);
        assert!(
            options
                .verdict(&step)
                .unwrap_err()
                .contains("failed to build")
        );
    }

    #[test]
    fn parses_linux_memory_bytes() {
        assert_eq!(
            parse_meminfo("MemTotal:       1000 kB\nMemAvailable:    250 kB\n").unwrap(),
            Memory {
                available_bytes: 256_000,
                total_bytes: 1_024_000,
            }
        );
    }

    #[test]
    fn only_full_hex_commit_ids_are_accepted() {
        assert!(validate_sha("0123456789abcdef0123456789abcdef01234567").is_ok());
        assert!(validate_sha("0123456").is_err());
        assert!(validate_sha("g123456789abcdef0123456789abcdef01234567").is_err());
    }

    #[test]
    fn cache_labels_must_match_pre_command_target_state() {
        assert!(validate_cache_state("cold", 0).is_ok());
        assert!(validate_cache_state("warm", 1).is_ok());
        assert!(validate_cache_state("mixed", 0).is_ok());
        assert!(validate_cache_state("mixed", 100).is_ok());
        assert!(validate_cache_state("unknown", 0).is_ok());
        assert!(validate_cache_state("unknown", 100).is_ok());
        assert_eq!(observed_cache_state(0), "cold");
        assert_eq!(observed_cache_state(1), "warm");
        assert!(
            validate_cache_state("cold", 1)
                .unwrap_err()
                .contains("found 1 bytes")
        );
        assert!(
            validate_cache_state("warm", 0)
                .unwrap_err()
                .contains("found 0 bytes")
        );
    }

    #[test]
    fn cargo_build_size_excludes_harness_receipts_closeout_and_markers() {
        let directory = tempfile::tempdir().expect("temporary target");
        let target = directory.path();
        for (path, bytes) in [
            ("debug/deps/test", 3_usize),
            ("release/vigil", 5),
            ("x86_64-unknown-linux-musl/release/vigil", 7),
            ("vigil-test-tools/mediamtx", 11),
            ("verify-receipts/receipt.json", 13),
            ("closeout/nextest-slow.tar.zst", 17),
            (".rustc_info.json", 19),
            ("CACHEDIR.TAG", 23),
        ] {
            let path = target.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create fixture directory");
            }
            fs::write(path, vec![0_u8; bytes]).expect("write fixture");
        }
        assert_eq!(cargo_build_bytes(target).expect("measure build bytes"), 15);
        assert!(directory_bytes(target).expect("measure total target") > 15);
        assert!(is_cargo_build_directory("aarch64-unknown-linux-musl"));
        assert!(!is_cargo_build_directory("vigil-test-tools"));
        assert!(!is_cargo_build_directory("verify-receipts"));
        assert!(!is_cargo_build_directory("closeout"));
    }
}
