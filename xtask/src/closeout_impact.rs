use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

const CONFIG_PATH: &str = ".config/dev-closeout-impact.toml";
const CLASSIFIER_PATH: &str = "xtask/src/closeout_impact.rs";
const CLASSIFIER_BOOTSTRAP_RULE: &str = "classifier-bootstrap-self-protection";
const UNKNOWN_TOP_LEVEL_RULE: &str = "unknown-top-level-build-substrate";
const PLATFORM_SENSITIVE_RULE: &str = "platform-sensitive-diff";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    rule_version: u64,
    #[serde(default)]
    install_expanded: Vec<Rule>,
    unknown_top_level: UnknownTopLevel,
    platform_sensitive_diff: PlatformSensitiveDiff,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    id: String,
    #[serde(default)]
    exact: Vec<String>,
    #[serde(default)]
    prefix: Vec<String>,
    #[serde(default)]
    basename: Vec<String>,
    #[serde(default)]
    basename_prefix: Vec<String>,
    #[serde(default)]
    suffix: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnknownTopLevel {
    name_fragments: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlatformSensitiveDiff {
    suffixes: Vec<String>,
    markers: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Impact {
    Ordinary,
    InstallExpanded,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct MatchedPath {
    path: String,
    rules: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct Receipt {
    rule_version: u64,
    base: String,
    head: String,
    impact: Impact,
    changed_paths: Vec<String>,
    matched_paths: Vec<MatchedPath>,
}

pub(crate) fn run(args: &[OsString]) -> Result<(), String> {
    let parsed = Args::parse(args)?;
    let root = repository_root()?;
    let receipt = classify_repository(&root, &parsed.base, &parsed.head)?;
    let bytes = serde_json::to_vec_pretty(&receipt)
        .map_err(|error| format!("serialize closeout impact receipt: {error}"))?;
    if let Some(parent) = parsed
        .json
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create receipt directory {}: {error}", parent.display()))?;
    }
    fs::write(&parsed.json, [bytes.as_slice(), b"\n"].concat())
        .map_err(|error| format!("write {}: {error}", parsed.json.display()))?;
    println!("{}", parsed.json.display());
    Ok(())
}

struct Args {
    base: String,
    head: String,
    json: PathBuf,
}

impl Args {
    fn parse(args: &[OsString]) -> Result<Self, String> {
        let mut base = None;
        let mut head = None;
        let mut json = None;
        let mut index = 0;
        while index < args.len() {
            let flag = args[index]
                .to_str()
                .ok_or_else(|| "closeout-impact arguments must be UTF-8".to_string())?;
            index += 1;
            let value = args.get(index).ok_or_else(usage)?;
            index += 1;
            match flag {
                "--base" if base.is_none() => base = Some(utf8(value, "--base")?.to_string()),
                "--head" if head.is_none() => head = Some(utf8(value, "--head")?.to_string()),
                "--json" if json.is_none() => json = Some(PathBuf::from(value)),
                _ => return Err(usage()),
            }
        }
        let parsed = Self {
            base: base.ok_or_else(usage)?,
            head: head.ok_or_else(usage)?,
            json: json.ok_or_else(usage)?,
        };
        require_full_sha("--base", &parsed.base)?;
        require_full_sha("--head", &parsed.head)?;
        Ok(parsed)
    }
}

fn usage() -> String {
    "usage: cargo xtask closeout-impact --base <40-char commit SHA> --head <40-char commit SHA> --json <path>".to_string()
}

fn utf8<'a>(value: &'a OsStr, name: &str) -> Result<&'a str, String> {
    value
        .to_str()
        .ok_or_else(|| format!("{name} must be valid UTF-8"))
}

fn require_full_sha(name: &str, value: &str) -> Result<(), String> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(format!(
            "{name} must be an exact 40-character commit SHA, not a branch, tag, or abbreviated ref"
        ))
    }
}

fn repository_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("run git rev-parse --show-toplevel: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "find repository root: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(PathBuf::from(
        String::from_utf8(output.stdout)
            .map_err(|error| format!("repository root is not UTF-8: {error}"))?
            .trim(),
    ))
}

fn classify_repository(root: &Path, base: &str, head: &str) -> Result<Receipt, String> {
    require_full_sha("--base", base)?;
    require_full_sha("--head", head)?;
    let base = require_commit(root, "--base", base)?;
    let head = require_commit(root, "--head", head)?;
    require_ancestor(root, &base, &head)?;

    let config_object = format!("{head}:{CONFIG_PATH}");
    let config_text = git_output(root, ["show", config_object.as_str()])
        .map_err(|error| format!("read {CONFIG_PATH} from --head {head}: {error}"))?;
    let config: Config =
        toml::from_str(&config_text).map_err(|error| format!("parse {CONFIG_PATH}: {error}"))?;
    validate_config(&config)?;

    let mut changed_paths = diff_names(root, &base, &head)?;
    changed_paths.extend(deleted_names(root, &base, &head)?);
    changed_paths.sort();
    changed_paths.dedup();
    let rename_sources = renamed_sources(root, &base, &head)?;
    let platform_sensitive = platform_sensitive_paths(
        root,
        &base,
        &head,
        &changed_paths,
        &config.platform_sensitive_diff,
    )?;
    Ok(classify_paths(
        config,
        base,
        head,
        changed_paths,
        rename_sources,
        platform_sensitive,
    ))
}

fn require_commit(root: &Path, name: &str, sha: &str) -> Result<String, String> {
    let expression = format!("{sha}^{{commit}}");
    let output = git_output(root, ["rev-parse", "--verify", expression.as_str()])?;
    let resolved = output.trim().to_ascii_lowercase();
    if resolved != sha.to_ascii_lowercase() {
        return Err(format!(
            "{name} must name a commit object exactly; {sha} resolved to {resolved}"
        ));
    }
    Ok(resolved)
}

fn require_ancestor(root: &Path, base: &str, head: &str) -> Result<(), String> {
    let status = Command::new("git")
        .current_dir(root)
        .args(["merge-base", "--is-ancestor", base, head])
        .status()
        .map_err(|error| format!("run git merge-base --is-ancestor: {error}"))?;
    match status.code() {
        Some(0) => Ok(()),
        Some(1) => Err(format!("--base {base} is not an ancestor of --head {head}")),
        _ => Err(format!("git merge-base --is-ancestor failed with {status}")),
    }
}

fn diff_names(root: &Path, base: &str, head: &str) -> Result<Vec<String>, String> {
    let range = format!("{base}..{head}");
    let output = git_output(
        root,
        [
            "diff",
            "--name-only",
            "--diff-filter=ACMRT",
            "--find-renames",
            range.as_str(),
        ],
    )?;
    normalized_lines(&output)
}

// Deletions are deliberately collected separately so the closeout classifier
// retains the canonical ACMRT change inventory while preventing removal of a
// Cargo, Docker, add-on, or build input from bypassing install proof.
fn deleted_names(root: &Path, base: &str, head: &str) -> Result<Vec<String>, String> {
    let range = format!("{base}..{head}");
    let output = git_output(
        root,
        ["diff", "--name-only", "--diff-filter=D", range.as_str()],
    )?;
    normalized_lines(&output)
}

// `--name-only` reports the destination of a rename. Inspecting rename status as
// a second, fail-closed input prevents moving Docker/build substrate to an
// innocent-looking path from downgrading closeout.
fn renamed_sources(root: &Path, base: &str, head: &str) -> Result<Vec<String>, String> {
    let range = format!("{base}..{head}");
    let output = git_output(
        root,
        [
            "diff",
            "--name-status",
            "--diff-filter=R",
            "--find-renames",
            range.as_str(),
        ],
    )?;
    let mut sources = BTreeSet::new();
    for line in output.lines().filter(|line| !line.is_empty()) {
        let mut fields = line.split('\t');
        let status = fields.next().unwrap_or_default();
        let source = fields.next();
        let destination = fields.next();
        if !status.starts_with('R')
            || source.is_none()
            || destination.is_none()
            || fields.next().is_some()
        {
            return Err(format!("unexpected git rename record {line:?}"));
        }
        sources.insert(normalize_path(source.unwrap())?);
    }
    Ok(sources.into_iter().collect())
}

fn platform_sensitive_paths(
    root: &Path,
    base: &str,
    head: &str,
    changed_paths: &[String],
    config: &PlatformSensitiveDiff,
) -> Result<BTreeSet<String>, String> {
    let range = format!("{base}..{head}");
    let mut matches = BTreeSet::new();
    for path in changed_paths {
        if !config.suffixes.iter().any(|suffix| path.ends_with(suffix)) {
            continue;
        }
        let output = git_output(
            root,
            [
                "diff",
                "--unified=0",
                "--no-ext-diff",
                range.as_str(),
                "--",
                path,
            ],
        )?;
        if output.lines().any(|line| {
            (line.starts_with('+') || line.starts_with('-'))
                && !line.starts_with("+++")
                && !line.starts_with("---")
                && config.markers.iter().any(|marker| line.contains(marker))
        }) {
            matches.insert(path.clone());
        }
    }
    Ok(matches)
}

fn git_output<'a>(root: &Path, args: impl IntoIterator<Item = &'a str>) -> Result<String, String> {
    let args: Vec<&str> = args.into_iter().collect();
    let output = Command::new("git")
        .current_dir(root)
        .args(&args)
        .output()
        .map_err(|error| format!("run git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("git output is not UTF-8: {error}"))
}

fn normalized_lines(output: &str) -> Result<Vec<String>, String> {
    let mut paths = BTreeSet::new();
    for path in output.lines().filter(|line| !line.is_empty()) {
        paths.insert(normalize_path(path)?);
    }
    Ok(paths.into_iter().collect())
}

fn normalize_path(path: &str) -> Result<String, String> {
    let path = path.replace('\\', "/");
    if path.starts_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("git produced unsafe repository path {path:?}"));
    }
    Ok(path)
}

fn validate_config(config: &Config) -> Result<(), String> {
    if config.rule_version == 0 {
        return Err("dev closeout impact rule_version must be positive".to_string());
    }
    let mut ids = BTreeSet::new();
    for rule in &config.install_expanded {
        if rule.id.trim().is_empty() || !ids.insert(rule.id.as_str()) {
            return Err(format!(
                "dev closeout impact rule id {:?} is empty or duplicated",
                rule.id
            ));
        }
    }
    if config.unknown_top_level.name_fragments.is_empty() {
        return Err("unknown_top_level.name_fragments must not be empty".to_string());
    }
    if config.platform_sensitive_diff.suffixes.is_empty()
        || config.platform_sensitive_diff.markers.is_empty()
    {
        return Err("platform_sensitive_diff suffixes and markers must not be empty".to_string());
    }
    Ok(())
}

fn classify_paths(
    config: Config,
    base: String,
    head: String,
    changed_paths: Vec<String>,
    rename_sources: Vec<String>,
    platform_sensitive: BTreeSet<String>,
) -> Receipt {
    let mut matches: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in changed_paths.iter().chain(rename_sources.iter()) {
        if matches!(path.as_str(), CONFIG_PATH | CLASSIFIER_PATH) {
            matches
                .entry(path.clone())
                .or_default()
                .insert(CLASSIFIER_BOOTSTRAP_RULE.to_string());
        }
        for rule in &config.install_expanded {
            if rule.matches(path) {
                matches
                    .entry(path.clone())
                    .or_default()
                    .insert(rule.id.clone());
            }
        }
        if unknown_top_level_match(&config.unknown_top_level, path) {
            matches
                .entry(path.clone())
                .or_default()
                .insert(UNKNOWN_TOP_LEVEL_RULE.to_string());
        }
        if platform_sensitive.contains(path) {
            matches
                .entry(path.clone())
                .or_default()
                .insert(PLATFORM_SENSITIVE_RULE.to_string());
        }
    }
    let matched_paths = matches
        .into_iter()
        .map(|(path, rules)| MatchedPath {
            path,
            rules: rules.into_iter().collect(),
        })
        .collect::<Vec<_>>();
    Receipt {
        rule_version: config.rule_version,
        base,
        head,
        impact: if matched_paths.is_empty() {
            Impact::Ordinary
        } else {
            Impact::InstallExpanded
        },
        changed_paths,
        matched_paths,
    }
}

impl Rule {
    fn matches(&self, path: &str) -> bool {
        let basename = path.rsplit('/').next().unwrap_or(path);
        self.exact.iter().any(|item| item == path)
            || self.prefix.iter().any(|item| path.starts_with(item))
            || self.basename.iter().any(|item| item == basename)
            || self
                .basename_prefix
                .iter()
                .any(|item| basename.starts_with(item))
            || self.suffix.iter().any(|item| path.ends_with(item))
    }
}

fn unknown_top_level_match(config: &UnknownTopLevel, path: &str) -> bool {
    if path.contains('/') {
        return false;
    }
    let lowercase = path.to_ascii_lowercase();
    config
        .name_fragments
        .iter()
        .any(|fragment| lowercase.contains(&fragment.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestRepository {
        path: PathBuf,
    }

    impl TestRepository {
        fn new() -> Self {
            let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("vigil-closeout-impact-{}-{id}", std::process::id()));
            fs::create_dir(&path).unwrap();
            command(&path, ["init", "--quiet"]);
            command(&path, ["config", "user.email", "test@example.invalid"]);
            command(&path, ["config", "user.name", "Vigil Test"]);
            fs::create_dir(path.join(".config")).unwrap();
            fs::write(
                path.join(CONFIG_PATH),
                include_str!("../../.config/dev-closeout-impact.toml"),
            )
            .unwrap();
            fs::write(path.join("README.md"), "base\n").unwrap();
            command(&path, ["add", "."]);
            command(&path, ["commit", "--quiet", "-m", "base"]);
            Self { path }
        }

        fn write(&self, path: &str, contents: &str) {
            let path = self.path.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }

        fn commit(&self, message: &str) -> String {
            command(&self.path, ["add", "-A"]);
            command(&self.path, ["commit", "--quiet", "-m", message]);
            self.head()
        }

        fn head(&self) -> String {
            git_output(&self.path, ["rev-parse", "HEAD"])
                .unwrap()
                .trim()
                .to_string()
        }

        fn classify(&self, base: &str, head: &str) -> Receipt {
            classify_repository(&self.path, base, head).unwrap()
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn command<const N: usize>(root: &Path, args: [&str; N]) {
        let status = Command::new("git")
            .current_dir(root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn ordinary_source_change_stays_ordinary() {
        let repo = TestRepository::new();
        let base = repo.head();
        repo.write("crates/vigil/src/lib.rs", "pub fn changed() {}\n");
        let head = repo.commit("ordinary");
        let receipt = repo.classify(&base, &head);
        assert_eq!(receipt.impact, Impact::Ordinary);
        assert_eq!(receipt.changed_paths, ["crates/vigil/src/lib.rs"]);
        assert!(receipt.matched_paths.is_empty());
    }

    #[test]
    fn target_specific_rust_change_expands_install_proof() {
        let repo = TestRepository::new();
        let base = repo.head();
        repo.write(
            "crates/vigil/src/platform.rs",
            "#[cfg(target_env = \"musl\")]\npub fn changed() {}\n",
        );
        let head = repo.commit("platform-specific behavior");
        let receipt = repo.classify(&base, &head);
        assert_eq!(receipt.impact, Impact::InstallExpanded);
        assert_eq!(receipt.matched_paths[0].rules, [PLATFORM_SENSITIVE_RULE]);
    }

    #[test]
    fn cargo_docker_addon_runtime_and_acceptance_inputs_expand_install_proof() {
        for path in [
            "crates/new/Cargo.toml",
            "Dockerfile.hardware",
            "addons/vigil/config.yaml",
            "packaging/runtime-packages.yaml",
            "tests/acceptance/installable_substrate.rs",
            "xtask/src/build.rs",
        ] {
            let repo = TestRepository::new();
            let base = repo.head();
            repo.write(path, "changed\n");
            let head = repo.commit(path);
            let receipt = repo.classify(&base, &head);
            assert_eq!(receipt.impact, Impact::InstallExpanded, "{path}");
            assert_eq!(receipt.matched_paths[0].path, path);
        }
    }

    #[test]
    fn changing_classifier_rules_expands_its_own_closeout() {
        let repo = TestRepository::new();
        let base = repo.head();
        let changed = r#"
rule_version = 2
[unknown_top_level]
name_fragments = ["deliberately-no-match"]
[platform_sensitive_diff]
suffixes = [".never"]
markers = ["deliberately-no-match"]
"#;
        repo.write(CONFIG_PATH, changed);
        let head = repo.commit("change classifier");
        // Classification is bound to the exact head commit, not an agent's
        // subsequently weakened working-tree config.
        repo.write(CONFIG_PATH, "rule_version = 999\n");
        let receipt = repo.classify(&base, &head);
        assert_eq!(receipt.impact, Impact::InstallExpanded);
        assert_eq!(receipt.matched_paths[0].path, CONFIG_PATH);
        assert!(
            receipt.matched_paths[0]
                .rules
                .contains(&CLASSIFIER_BOOTSTRAP_RULE.to_string())
        );
        assert_eq!(receipt.rule_version, 2);
    }

    #[test]
    fn deleting_build_substrate_cannot_bypass_expanded_classification() {
        let repo = TestRepository::new();
        repo.write("Dockerfile.ci", "FROM scratch\n");
        repo.commit("add Dockerfile");
        let base = repo.head();
        fs::remove_file(repo.path.join("Dockerfile.ci")).unwrap();
        let head = repo.commit("delete Dockerfile");
        let receipt = repo.classify(&base, &head);
        assert_eq!(receipt.impact, Impact::InstallExpanded);
        assert_eq!(receipt.changed_paths, ["Dockerfile.ci"]);
    }

    #[test]
    fn renaming_build_substrate_cannot_bypass_expanded_classification() {
        let repo = TestRepository::new();
        repo.write("Dockerfile.hardware", "FROM scratch\n");
        repo.commit("add Dockerfile");
        let base = repo.head();
        command(&repo.path, ["mv", "Dockerfile.hardware", "notes.txt"]);
        let head = repo.commit("rename away");
        let receipt = repo.classify(&base, &head);
        assert_eq!(receipt.impact, Impact::InstallExpanded);
        assert_eq!(receipt.changed_paths, ["notes.txt"]);
        assert!(
            receipt
                .matched_paths
                .iter()
                .any(|entry| entry.path == "Dockerfile.hardware")
        );
    }

    #[test]
    fn unknown_top_level_build_like_file_fails_closed() {
        let repo = TestRepository::new();
        let base = repo.head();
        repo.write("ship-image.nu", "changed\n");
        let head = repo.commit("new build substrate");
        let receipt = repo.classify(&base, &head);
        assert_eq!(receipt.impact, Impact::InstallExpanded);
        assert_eq!(receipt.matched_paths[0].rules, [UNKNOWN_TOP_LEVEL_RULE]);
    }

    #[test]
    fn moving_and_invalid_refs_are_rejected() {
        let repo = TestRepository::new();
        let head = repo.head();
        assert!(
            Args::parse(&[
                OsString::from("--base"),
                OsString::from("HEAD"),
                OsString::from("--head"),
                OsString::from(&head),
                OsString::from("--json"),
                OsString::from("receipt.json"),
            ])
            .is_err()
        );
        let moving_ref = "1".repeat(40);
        command(&repo.path, ["branch", &moving_ref]);
        assert!(classify_repository(&repo.path, &moving_ref, &head).is_err());
        let invalid = "0".repeat(40);
        assert!(classify_repository(&repo.path, &invalid, &head).is_err());
    }
}
