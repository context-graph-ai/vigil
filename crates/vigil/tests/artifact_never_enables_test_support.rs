//! The `test-support` feature (`crates/vigil/Cargo.toml`) compiles in the
//! hub-role door — `SettingsStore::open_hub_role`, `apply_hub_authored`, and
//! the take-over instruction that authors settings records at the pushed
//! rank. No shipped `vigil` build may enable it: a node that could obtain a
//! hub-role handle could author a fleet instruction nobody in the fleet
//! issued, exactly the second control path the settings model forbids. The
//! feature only reaches a test build through the crate's dev-dependency on
//! itself (`vigil = { path = ".", features = ["test-support"] }`).
//!
//! This guards the OTHER half of that promise: not that the door is gated in
//! source (`no_command_line_or_operator_path_writes_at_the_pushed_rank`,
//! beside this file), but that nothing which builds a shipped artifact ever
//! flips the gate open.
//!
//! Two legs read TEXT (the manifest's own `default` line; the argv a build
//! invocation names), which sees only enablements spelled out where these
//! legs happen to look. A third leg reads the GRAPH: `cargo tree`'s own
//! feature-resolution walk, restricted to feature-activation edges with
//! `dev` excluded, run for every real artifact build's feature set. This is
//! what actually catches a feature-forwarding dependency edge or a
//! `test-support`-listing feature definition — neither of which spells
//! `test-support` anywhere the text legs read, and both of which compile
//! into a shipped binary just the same. A raw `cargo metadata` JSON read
//! was tried and rejected during authoring: its `resolve.nodes[].features`
//! is a single GLOBAL union per package across every consumer, so `vigil`
//! already reads `test-support` there today (from its own legitimate
//! dev-dependency on itself) regardless of which edges are walked — the
//! false positive this guard exists not to have. `cargo tree -e features`
//! resolves per-edge-kind the way `cargo build` itself does, which is why
//! it is the graph read used here.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const FEATURE_NAME: &str = "test-support";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read(relative: &str) -> String {
    let path = workspace_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// `crates/vigil/Cargo.toml`'s `[features]` table declares `test-support` (so
/// this guard is not vacuous against a tree where the feature was removed
/// outright) but its `default` array never names it — a default feature
/// reaches every build that does not explicitly opt out, artifacts included.
#[test]
fn the_vigil_crate_declares_test_support_but_never_defaults_it_on() {
    let manifest = read("crates/vigil/Cargo.toml");
    assert!(
        manifest
            .lines()
            .any(|line| line.trim_start().starts_with(FEATURE_NAME)
                && line.contains('=')
                && !line.trim_start().starts_with("default")),
        "expected {FEATURE_NAME} to still be a declared feature in crates/vigil/Cargo.toml, or \
         this guard proves nothing"
    );
    let default_line = manifest
        .lines()
        .find(|line| line.trim_start().starts_with("default"))
        .unwrap_or_else(|| panic!("expected a `default = [...]` features line in {manifest:?}"));
    assert!(
        !default_line.contains(FEATURE_NAME),
        "crates/vigil/Cargo.toml's default feature set must never include {FEATURE_NAME}: \
         {default_line:?}"
    );
}

/// Every place a shipped `vigil` binary is actually built — the hardware
/// Dockerfile's own `cargo build`, and the release/install-binary lanes
/// `xtask` constructs the build argv for — never names `test-support` on a
/// `--features` list. The addon and scratch Dockerfiles stage a pre-built
/// binary rather than compiling one, so they carry no build invocation to
/// scan; they are included anyway so a future change that starts compiling
/// there is not silently unguarded.
#[test]
fn no_artifact_build_invocation_names_the_test_support_feature() {
    let candidates = [
        "Dockerfile",
        "Dockerfile.hardware",
        "addons/vigil/Dockerfile",
        "xtask/src/verify.rs",
        "xtask/src/test_estate.rs",
        "xtask/src/lib.rs",
        "xtask/src/main.rs",
        "xtask/src/closeout_impact.rs",
        "scripts/verify",
    ];
    for relative in candidates {
        let text = read(relative);
        assert!(
            !text.contains(FEATURE_NAME),
            "{relative} must never name the {FEATURE_NAME} feature — an artifact build path \
             would compile in the hub-role door"
        );
    }

    let workflows_dir = workspace_root().join(".github/workflows");
    let entries = fs::read_dir(&workflows_dir)
        .unwrap_or_else(|error| panic!("list {}: {error}", workflows_dir.display()));
    let mut checked = 0;
    for entry in entries {
        let entry = entry.expect("read workflow directory entry");
        let path = entry.path();
        let extension = path.extension().and_then(|extension| extension.to_str());
        if !matches!(extension, Some("yml") | Some("yaml")) {
            continue;
        }
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        assert!(
            !text.contains(FEATURE_NAME),
            "{} must never name the {FEATURE_NAME} feature on a build or test invocation",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "expected at least one workflow file under {} to scan, or this guard checked nothing",
        workflows_dir.display()
    );
}

/// Every real command line something actually invokes to produce a shipped
/// `vigil-bin` artifact — the bare release build, the `fabric`-enabled
/// install-binary/static-release build (`xtask/src/verify.rs`), and the
/// hardware Dockerfile's build (`Dockerfile.hardware`). Two ways a shipped
/// artifact could reach `test-support` without ever spelling the feature
/// name in one of these argv lists are both invisible to the string-search
/// legs above: a feature-forwarding dependency edge (`vigil = { path = "..",
/// features = ["test-support"] }` in `vigil-bin`'s or `vigil-ha`'s own
/// `Cargo.toml`), or another `vigil` feature — `fabric` is the one every one
/// of these argv lists actually turns on — itself listing `test-support` in
/// its own definition. Both compile fine and both ship the hub-role door;
/// neither leaves a trace anywhere the text legs look.
const ARTIFACT_FEATURE_SETS: &[&str] = &["", "fabric", "decode-gstreamer,detect-burn-wgpu,fabric"];

/// The real dependency graph and feature resolution `cargo` computes for a
/// `vigil-bin` build with one of [`ARTIFACT_FEATURE_SETS`] enabled never
/// activates `vigil`'s `test-support` feature. Unlike the string-search legs
/// above, this reads the manifest/feature GRAPH itself — via `cargo tree`'s
/// own resolver, the same one `cargo build` uses — so a feature-forwarding
/// dependency edge or a `test-support`-listing feature is caught regardless
/// of whether its argv ever names `test-support` directly.
///
/// `-e features` restricts the walked edges to feature-activation edges
/// (which, absent `dev` in that set, excludes every `[dev-dependencies]`
/// edge — including `vigil`'s own dev-dependency on itself with
/// `features = ["test-support"]`, the harness's legitimate door, which must
/// NOT trip this guard); `-i vigil` inverts the walk to "every feature
/// activation that reaches the `vigil` package," which is what actually
/// surfaces a `test-support` activation reached through an intermediate
/// feature like `fabric` — the forward (root-down) rendering silently
/// collapses a repeated node the first time it is fully printed and can
/// otherwise omit the very edge this guard exists to catch (verified by
/// planting both enablements against this exact command during authoring;
/// both showed only under `-i`, never under the forward rendering).
#[test]
// `CARGO` is cargo's own test-harness-injected variable (the path to the
// cargo binary that invoked this test run), not a product-adjustable value
// the settings-declaration guard covers — the same exemption
// `vigil_binary_path` in `deterministic_fixture_support.rs` documents for
// `CARGO_BIN_EXE_vigil`.
#[allow(clippy::disallowed_methods)]
fn no_artifact_feature_resolution_ever_activates_test_support() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut checked = 0;
    for features in ARTIFACT_FEATURE_SETS {
        let mut command = Command::new(&cargo);
        command
            .arg("tree")
            .arg("-p")
            .arg("vigil-bin")
            .arg("-e")
            .arg("features")
            .arg("-i")
            .arg("vigil")
            .current_dir(workspace_root());
        if !features.is_empty() {
            command.arg("--features").arg(features);
        }
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("run `cargo tree` for features {features:?}: {error}"));
        assert!(
            output.status.success(),
            "`cargo tree -p vigil-bin -e features -i vigil --features {features:?}` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.contains(FEATURE_NAME),
            "vigil-bin built with features {features:?} — the argv real build lanes actually \
             use — resolves vigil's own {FEATURE_NAME} feature as active; a feature-forwarding \
             dependency edge or a feature definition listing {FEATURE_NAME} has opened the \
             hub-role door in a shipped artifact. cargo tree output:\n{stdout}"
        );
        checked += 1;
    }
    assert!(
        checked == ARTIFACT_FEATURE_SETS.len(),
        "expected to check every entry in ARTIFACT_FEATURE_SETS, or this guard proves less than \
         it claims"
    );
}
