//! OSS cleanliness guards.
//!
//! 1. Backend/vendor names (decoder elements, GPU APIs, engine names) belong
//!    in receipts, stats, health, logs, and doctor output ONLY. They must not
//!    become MQTT/Home Assistant semantic event fields, context-graph
//!    observation semantics, correction records, or evidence schemas.
//! 2. The public repo carries nothing environment- or owner-specific: no
//!    private hostnames, LAN addresses, personal usernames, or site/camera
//!    names from any private test estate. A neutral floor list is built in;
//!    a private, out-of-repo extension list can be supplied via
//!    `VIGIL_PRIVATE_NOUN_SCAN_FILE` (one lowercase needle per line).

use std::fs;
use std::path::{Path, PathBuf};

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

/// String literals in a Rust source text, with line numbers. Good enough
/// for a guard: normal quoted literals, comments stripped line-wise.
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

#[test]
fn backend_nouns_stay_out_of_product_domain_outputs() {
    // Files that BUILD product-domain payloads: MQTT discovery + events,
    // correction records, and the review data plane rows.
    let product_output_files = [
        "src/ha_discovery.rs",
        "src/ha_mqtt_tasks.rs",
        "src/correction.rs",
        "src/live_read.rs",
        "src/http_data_plane.rs",
    ];
    // Backend nouns that must never become product-domain fields or values.
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
        let path = crate_root().join(file);
        let text = fs::read_to_string(&path).unwrap_or_else(|_| panic!("read {file}"));
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

#[test]
fn no_private_environment_nouns_in_repo_sources() {
    // Neutral floor list: patterns that identify SOME private deployment
    // rather than any specific one — private LAN literals and obviously
    // machine-specific markers. The full private list (owner names, real
    // hostnames, camera names) lives OUTSIDE this repo and is supplied by
    // the private smoke via VIGIL_PRIVATE_NOUN_SCAN_FILE.
    let mut needles: Vec<String> = ["192.168.", "10.0.0.", "@sha256:deadbeef"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Ok(private_list) = std::env::var("VIGIL_PRIVATE_NOUN_SCAN_FILE") {
        let text = fs::read_to_string(&private_list).expect(
            "private noun list file named by VIGIL_PRIVATE_NOUN_SCAN_FILE must be readable",
        );
        needles.extend(
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(|line| line.to_ascii_lowercase()),
        );
    }

    let mut violations = Vec::new();
    for dir in ["crates", "addons", "tests", "xtask"] {
        let root = repo_root().join(dir);
        if !root.exists() {
            continue;
        }
        for path in rust_sources(&root) {
            // The scan's own needle definitions are not violations.
            if path.file_name().and_then(|n| n.to_str()) == Some("oss_cleanliness_scan.rs") {
                continue;
            }
            let text = fs::read_to_string(&path).expect("read source");
            let lower = text.to_ascii_lowercase();
            for needle in &needles {
                if lower.contains(needle.as_str()) {
                    violations.push(format!("{}: contains `{needle}`", path.display()));
                }
            }
        }
        // Non-Rust deployment surfaces: config yaml + Dockerfiles.
        for name in ["vigil/config.yaml", "vigil/Dockerfile"] {
            let path = repo_root().join(dir).join(name);
            if let Ok(text) = fs::read_to_string(&path) {
                let lower = text.to_ascii_lowercase();
                for needle in &needles {
                    if lower.contains(needle.as_str()) {
                        violations.push(format!("{}: contains `{needle}`", path.display()));
                    }
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "owner/environment-specific content found in the public repo:\n{}",
        violations.join("\n")
    );
}
