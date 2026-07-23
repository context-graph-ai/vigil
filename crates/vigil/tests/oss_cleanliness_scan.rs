//! OSS cleanliness guards.
//!
//! Backend and device names belong in operator/runtime records such as
//! receipts, stats, health, logs, doctor output, package manifests, and
//! Dockerfiles. They must not become product-domain event fields, correction
//! records, review rows, or evidence semantics.
//!
//! The public repo also carries no private deployment nouns. A neutral canary
//! is built in for scan verification; a private out-of-repo extension list can
//! be supplied via `VIGIL_PRIVATE_NOUN_SCAN_FILE`, one lowercase needle per
//! line.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const RELEASE_NOTES_PATH: &str = "docs/release-notes.md";
const PRIVATE_CANARY: &str = "private-estate-canary";

#[derive(Debug, Clone, PartialEq, Eq)]
enum SurfaceTarget {
    File(&'static str),
    ShellScripts(&'static str),
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    crate_root()
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn packaging_docs_and_harness_surfaces() -> Vec<SurfaceTarget> {
    vec![
        SurfaceTarget::File("addons/vigil/runtime-packages.yaml"),
        SurfaceTarget::File("addons/vigil/config.yaml"),
        SurfaceTarget::File("addons/vigil/translations/en.yaml"),
        SurfaceTarget::File("addons/vigil/Dockerfile"),
        SurfaceTarget::File("Dockerfile"),
        SurfaceTarget::File("Dockerfile.hardware"),
        SurfaceTarget::File(RELEASE_NOTES_PATH),
        SurfaceTarget::ShellScripts("tests/ha-os-vm"),
    ]
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name != "target" && name != ".git" {
                    stack.push(path);
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn resolve_present_surfaces(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for target in packaging_docs_and_harness_surfaces() {
        match target {
            SurfaceTarget::File(relative) => {
                let path = root.join(relative);
                if path.exists() {
                    paths.push(path);
                }
            }
            SurfaceTarget::ShellScripts(relative_dir) => {
                let dir = root.join(relative_dir);
                let Ok(entries) = fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) == Some("sh") {
                        paths.push(path);
                    }
                }
            }
        }
    }
    paths.sort();
    paths
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// String literals in a Rust source text, with line numbers. Good enough for
/// a guard: normal quoted literals, comments stripped line-wise.
fn string_literals(text: &str) -> Vec<(usize, String)> {
    let mut literals = Vec::new();
    for (line_no, line) in text.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        let mut rest = code;
        while let Some(start) = rest.find('"') {
            let tail = &rest[start + 1..];
            let Some(end) = tail.find('"') else { break };
            literals.push((line_no + 1, tail[..end].to_string()));
            rest = &tail[end + 1..];
        }
    }
    literals
}

// VIGIL_PRIVATE_NOUN_SCAN_FILE is this guard's own operator-supplied scan
// input, not a product-adjustable value the settings-declaration guard
// covers.
#[allow(clippy::disallowed_methods)]
fn private_needles() -> Option<Vec<String>> {
    let mut needles = vec![
        PRIVATE_CANARY.to_string(),
        "@sha256:deadbeef".to_string(),
        "192.168.".to_string(),
    ];
    needles.extend((0..=255).map(|octet| format!("10.{octet}.")));
    needles.extend((16..=31).map(|octet| format!("172.{octet}.")));
    if let Ok(private_list) = std::env::var("VIGIL_PRIVATE_NOUN_SCAN_FILE") {
        let text = fs::read_to_string(&private_list);
        assert!(
            text.is_ok(),
            "private noun list file named by VIGIL_PRIVATE_NOUN_SCAN_FILE must be readable"
        );
        let Ok(text) = text else {
            return None;
        };
        needles.extend(
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(|line| line.to_ascii_lowercase()),
        );
    }
    Some(needles)
}

fn scan_private_needles(root: &Path, paths: &[PathBuf], needles: &[String]) -> Vec<String> {
    let mut violations = Vec::new();
    for path in paths {
        if path.file_name().and_then(|n| n.to_str()) == Some("oss_cleanliness_scan.rs") {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let lower = text.to_ascii_lowercase();
        for needle in needles {
            if lower.contains(needle.as_str()) {
                violations.push(format!(
                    "{}: contains `{needle}`",
                    relative_path(root, path)
                ));
            }
        }
    }
    violations
}

#[test]
fn packaging_and_docs_surfaces_are_covered_by_the_cleanliness_scan() {
    // Unfakeable because the coverage test consumes the same surface
    // enumeration as the guard and proves the scan reads file contents.
    let root = repo_root();
    let targets = packaging_docs_and_harness_surfaces();
    let declared_files: BTreeSet<&str> = targets
        .iter()
        .filter_map(|target| match target {
            SurfaceTarget::File(path) => Some(*path),
            SurfaceTarget::ShellScripts(_) => None,
        })
        .collect();
    for required in [
        "addons/vigil/runtime-packages.yaml",
        "addons/vigil/config.yaml",
        "addons/vigil/translations/en.yaml",
        "addons/vigil/Dockerfile",
        "Dockerfile",
        "Dockerfile.hardware",
        RELEASE_NOTES_PATH,
    ] {
        assert!(
            declared_files.contains(required),
            "cleanliness scan surface enumeration must include {required}"
        );
        assert!(
            root.join(required).exists(),
            "cleanliness scan target must exist: {required}"
        );
    }
    assert!(
        targets
            .iter()
            .any(|target| matches!(target, SurfaceTarget::ShellScripts("tests/ha-os-vm"))),
        "cleanliness scan must include the HA-OS harness shell scripts"
    );

    let surfaces = resolve_present_surfaces(&root);
    assert!(
        surfaces.iter().any(|path| {
            relative_path(&root, path).starts_with("tests/ha-os-vm/")
                && path.extension().and_then(|ext| ext.to_str()) == Some("sh")
        }),
        "cleanliness scan must resolve at least one HA-OS harness shell script"
    );

    let temp = tempfile::tempdir();
    assert!(
        temp.is_ok(),
        "temp directory must be available for canary scan"
    );
    let Ok(temp) = temp else {
        return;
    };
    for surface in &surfaces {
        let relative = relative_path(&root, surface);
        let text = fs::read_to_string(surface);
        assert!(text.is_ok(), "surface must be readable: {relative}");
        let Ok(mut text) = text else {
            return;
        };
        text.push('\n');
        text.push_str(PRIVATE_CANARY);
        text.push('\n');
        let temp_path = temp.path().join(relative.replace('/', "__"));
        let write_result = fs::write(&temp_path, text);
        assert!(write_result.is_ok(), "temp canary copy must be writable");
        let violations =
            scan_private_needles(temp.path(), &[temp_path], &[PRIVATE_CANARY.to_string()]);
        assert!(
            !violations.is_empty(),
            "cleanliness scan must read contents for {relative}"
        );
    }
}

#[test]
fn no_private_environment_nouns_in_repo_sources() {
    // Unfakeable because Rust sources and the shared non-Rust surface list are
    // scanned together; missing future surfaces are skipped only by this guard.
    let Some(needles) = private_needles() else {
        return;
    };
    let root = repo_root();
    let mut paths = Vec::new();
    for dir in ["crates", "addons", "tests", "xtask"] {
        let root_dir = root.join(dir);
        if root_dir.exists() {
            paths.extend(rust_sources(&root_dir));
        }
    }
    paths.extend(resolve_present_surfaces(&root));

    let violations = scan_private_needles(&root, &paths, &needles);
    assert!(
        violations.is_empty(),
        "private deployment content found in the public repo:\n{}",
        violations.join("\n")
    );
}

#[test]
fn backend_nouns_stay_out_of_product_domain_outputs() {
    // Unfakeable because it scans string literals in the code that builds
    // product-domain payloads, while operator records remain allowed to name
    // backends and devices. The Home Assistant adapter's payload builders
    // live in the sibling `vigil-ha` crate, addressed from the workspace
    // root; the rest are this crate's own product-domain sources.
    let product_output_files = [
        "crates/vigil-ha/src/ha_discovery.rs",
        "crates/vigil-ha/src/ha_mqtt_tasks.rs",
        "crates/vigil/src/correction.rs",
        "crates/vigil/src/live_read.rs",
        "crates/vigil/src/http_data_plane.rs",
    ];
    let backend_nouns = [
        "gstreamer",
        "vaapi",
        "va-api",
        "decodebin",
        "appsink",
        "vulkan",
        "wgpu",
        "cuda",
        "ffmpeg",
        "attempted_backend",
        "active_backend",
        "hardware_accelerated",
        "selected_device",
    ];

    let mut violations = Vec::new();
    for file in product_output_files {
        let path = repo_root().join(file);
        let text = fs::read_to_string(&path);
        assert!(
            text.is_ok(),
            "product-domain source must be readable: {file}"
        );
        let Ok(text) = text else {
            return;
        };
        for (line, literal) in string_literals(&text) {
            let lower = literal.to_ascii_lowercase();
            for noun in backend_nouns {
                if lower.contains(noun) {
                    violations.push(format!("{file}:{line}: \"{literal}\" contains `{noun}`"));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "backend names leaked into product-domain output builders:\n{}",
        violations.join("\n")
    );
}
