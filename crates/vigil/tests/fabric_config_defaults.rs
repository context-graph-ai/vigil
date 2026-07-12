//! Configurable-with-sane-defaults contract for every new fabric/offload
//! knob this run introduces (criterion C10). Every operator-facing value
//! must be a config knob with a sane default; defaults must be VISIBLE on
//! the shape's normal surface (the HAOS add-on options yaml); and nothing
//! ships hardcoded outside the defaults struct.
//!
//! This file is RED by construction: the config-surface wiring (part a) is
//! real inert scaffold, but the HAOS add-on options yaml (part b) is
//! deliberately NOT edited here — editing it is implementation, not
//! test-authoring — so this test fails at the yaml assertion.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use vigil::offload_policy::OffloadPolicyConfig;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn section_scalar(text: &str, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !line.starts_with(' ') && trimmed.ends_with(':') {
            in_section = trimmed.trim_end_matches(':') == section;
            continue;
        }
        if in_section
            && line.starts_with("  ")
            && !line.starts_with("    ")
            && let Some((candidate, value)) = trimmed.split_once(':')
            && candidate == key
        {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

struct SourceFile {
    path: PathBuf,
    text: String,
}

fn collect_rust_source_files(root: &Path) -> Vec<SourceFile> {
    let mut sources = Vec::new();
    collect_rust_source_files_into(root, &mut sources);
    sources
}

fn collect_rust_source_files_into(dir: &Path, sources: &mut Vec<SourceFile>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_source_files_into(&path, sources);
        } else if path.extension() == Some(OsStr::new("rs"))
            && let Ok(text) = fs::read_to_string(&path)
        {
            sources.push(SourceFile { path, text });
        }
    }
}

/// C10, all three arms in one named test (per the frozen mapping):
/// (a) every new knob has a working Default and `RuntimeConfig`/the fabric
///     intent build with NONE of them provided;
/// (b) the HAOS add-on options yaml shows the fabric knob defaults —
///     DELIBERATELY not yet true (implementation, not this pass), so this
///     file is RED here;
/// (c) no source file outside `offload_policy.rs`'s defaults struct
///     hardcodes one of the new threshold literals in control flow.
#[test]
fn all_offload_and_fabric_knobs_have_visible_defaults_and_work_unset() {
    // (a) Every new knob has a working Default; the fabric intent resolves
    // with nothing provided.
    let offload_defaults = OffloadPolicyConfig::default();
    assert!(
        offload_defaults.saturation_fraction > 0.0 && offload_defaults.saturation_fraction <= 1.0,
        "saturation_fraction must have a sane in-range default, got {}",
        offload_defaults.saturation_fraction
    );
    assert!(
        offload_defaults.fallback_horizon_ms > 0,
        "fallback_horizon_ms must have a sane positive default"
    );
    assert!(
        offload_defaults.max_attempts > 0,
        "max_attempts must have a sane positive default"
    );
    assert!(
        offload_defaults.blob_cap_bytes > 0,
        "blob_cap_bytes must have a sane positive default"
    );

    let fabric_intent = vigil::fabric_intent_from_args(Vec::new())
        .expect("vigil must build with NONE of the fabric knobs provided");
    assert_eq!(
        fabric_intent.fabric_ticket, None,
        "fabric_ticket must default absent, never a hardcoded value"
    );
    assert!(
        !fabric_intent.fabric_hub,
        "fabric_hub must default false — no silent new network surface on existing installs"
    );

    // (b) The HAOS add-on options yaml must show the fabric knob defaults —
    // both `options:` (the operator-visible default) and `schema:` (the
    // type contract), mirroring the `addon_config_surface` pattern. This is
    // the RED arm: the yaml is not edited by this test-authoring pass.
    let addon_config_path = repo_root().join("addons/vigil/config.yaml");
    let addon_config = fs::read_to_string(&addon_config_path).unwrap_or_else(|error| {
        panic!(
            "add-on config must be readable at {}: {error}",
            addon_config_path.display()
        )
    });
    for key in ["fabric_ticket", "fabric_hub"] {
        assert!(
            section_scalar(&addon_config, "options", key).is_some(),
            "{key} must be present under the HAOS add-on options: block with a visible default"
        );
        assert!(
            section_scalar(&addon_config, "schema", key).is_some(),
            "{key} must be present under the HAOS add-on schema: block"
        );
    }

    // (c) Source-scan arm: no hardcoded threshold literal for the C10 knobs
    // outside the defaults struct in offload_policy.rs. Mirrors the
    // `source_scan_contract` grep-for-markers idiom.
    let vigil_src = repo_root().join("crates/vigil/src");
    let sources = collect_rust_source_files(&vigil_src);
    let mut failures = Vec::new();
    for source in &sources {
        if source.path.file_name() == Some(OsStr::new("offload_policy.rs")) {
            // The defaults struct itself is the one legal home for these
            // literals.
            continue;
        }
        for forbidden in ["0.8_f64", "5_000u64", "fallback_horizon_ms: 5000"] {
            if source.text.contains(forbidden) {
                failures.push(format!(
                    "{} hardcodes offload-policy threshold literal `{forbidden}` outside offload_policy.rs's defaults struct",
                    source.path.display()
                ));
            }
        }
    }
    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}
