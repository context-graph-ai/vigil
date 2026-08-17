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
fn all_offload_and_fabric_knobs_have_sane_defaults_and_work_unset() {
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

    // (a, fix cycle 9) The two new tuning knobs resolve with nothing
    // provided to today's hardcoded fabric.rs literals, byte-identical —
    // introducing the knob changes nothing for an operator who sets
    // nothing.
    let tuning_intent = vigil::fabric_tuning_intent_from_args(Vec::new())
        .expect("vigil must build with NONE of the fabric tuning knobs provided");
    assert_eq!(
        tuning_intent.fabric_worker_lease_ms, 300_000,
        "fabric_worker_lease_ms must default to today's hardcoded fabric.rs worker lease \
         (5 minutes = 300000ms)"
    );
    assert_eq!(
        tuning_intent.fabric_fallback_horizon_ms, 5_000,
        "fabric_fallback_horizon_ms must default to today's hardcoded \
         OffloadPolicyConfig::default() value (5000ms)"
    );

    // (b) The HAOS add-on options yaml must show each knob's default on its
    // real surface. `fabric_ticket` is an enrollment credential, not a
    // behavior key (see `addon_config_surface.rs`'s `NON_BEHAVIOR_SCHEMA_KEYS`
    // exemption list), so it still carries a manifest-visible default under
    // `options:`, mirroring the `addon_config_surface` pattern. `fabric_hub`
    // IS a behavior key: under the settings-authority direction a behavior
    // key that still declared a default in `options:` would pin every fresh
    // install and break reset-by-absence, so it carries no `options:` entry
    // at all — its default-false-and-works-unset fact is the source-side
    // check already made above in arm (a) via the public
    // `fabric_intent_from_args` surface (`fabric_intent.fabric_hub` is
    // `false` with nothing supplied); here we only require the manifest to
    // declare it schema-optional with no options default.
    let addon_config_path = repo_root().join("addons/vigil/config.yaml");
    let addon_config = fs::read_to_string(&addon_config_path).unwrap_or_else(|error| {
        panic!(
            "add-on config must be readable at {}: {error}",
            addon_config_path.display()
        )
    });
    assert!(
        section_scalar(&addon_config, "options", "fabric_ticket").is_some(),
        "fabric_ticket must be present under the HAOS add-on options: block with a visible default"
    );
    assert!(
        section_scalar(&addon_config, "schema", "fabric_ticket").is_some(),
        "fabric_ticket must be present under the HAOS add-on schema: block"
    );
    assert_eq!(
        section_scalar(&addon_config, "options", "fabric_hub"),
        None,
        "fabric_hub must not declare a default in the options block — its default lives in Vigil's own loader, proven in arm (a) above"
    );
    assert_eq!(
        section_scalar(&addon_config, "schema", "fabric_hub").as_deref(),
        Some("bool?"),
        "fabric_hub must be present under the HAOS add-on schema block as an optional boolean"
    );

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

/// (i, fix cycle 9) Config-surface flow: an explicit value supplied via CLI
/// must reach the public `FabricTuningIntent` accessor — proves the inert
/// scaffold's OWN plumbing (PartialConfig/CliOverrides/merge/defaulting) is
/// wired correctly, independent of whether `fabric.rs` yet consumes it (that
/// is `fabric_rs_never_hardcodes_the_tuning_defaults_outside_their_config_owned_home`,
/// below).
#[test]
fn fabric_tuning_knobs_flow_through_the_config_surface_when_set() {
    let tuning_intent = vigil::fabric_tuning_intent_from_args(vec![
        std::ffi::OsString::from("--fabric-worker-lease-ms"),
        std::ffi::OsString::from("45000"),
        std::ffi::OsString::from("--fabric-fallback-horizon-ms"),
        std::ffi::OsString::from("1500"),
    ])
    .expect("vigil must build with explicit fabric tuning knobs provided");
    assert_eq!(
        tuning_intent.fabric_worker_lease_ms, 45_000,
        "--fabric-worker-lease-ms must flow into FabricTuningIntent.fabric_worker_lease_ms"
    );
    assert_eq!(
        tuning_intent.fabric_fallback_horizon_ms, 1_500,
        "--fabric-fallback-horizon-ms must flow into FabricTuningIntent.fabric_fallback_horizon_ms"
    );
}

/// (iii, fix cycle 9) Extends the `addon_config_surface` HAOS options/schema
/// contract (mirroring the `["fabric_ticket", "fabric_hub"]` check above) to
/// the two new tuning knobs.
///
/// Superseded contract note: this test used to require both knobs present
/// under `options:` with a visible default. Under the settings-authority
/// direction they are behavior keys: a declared options default would pin
/// every fresh install and block reset-by-absence, so the manifest now
/// declares them schema-optional with no options entry, and their default
/// value is Vigil's own. That default is asserted through the PUBLIC
/// resolved surface (`vigil::fabric_tuning_intent_from_args`), not by
/// grepping fabric.rs's source text for a literal — `fabric_worker_lease_ms`
/// is mid-move into the env-baseline work (block 4, in progress alongside
/// this pass), so a source-text assertion here would break under a rename
/// that leaves the resolved value unchanged. The exact values (300000ms /
/// 5000ms) are already proven against that same public surface by
/// `all_offload_and_fabric_knobs_have_sane_defaults_and_work_unset` above in
/// this file — not re-asserted here to avoid pinning the same fact twice;
/// this test's own job is the manifest's schema/options shape.
#[test]
fn addon_config_declares_the_new_fabric_tuning_knobs_as_schema_optional_with_vigil_owned_defaults()
{
    // The defaults exist and are reachable through the resolved public
    // surface right now — a knob whose loader wiring silently regressed
    // (e.g. block 4's move drops the fallback value) fails here even though
    // this test does not re-check the exact numbers.
    vigil::fabric_tuning_intent_from_args(Vec::new())
        .expect("vigil must build with NONE of the fabric tuning knobs provided");

    let addon_config_path = repo_root().join("addons/vigil/config.yaml");
    let addon_config = fs::read_to_string(&addon_config_path).unwrap_or_else(|error| {
        panic!(
            "add-on config must be readable at {}: {error}",
            addon_config_path.display()
        )
    });
    for key in ["fabric_worker_lease_ms", "fabric_fallback_horizon_ms"] {
        assert_eq!(
            section_scalar(&addon_config, "options", key),
            None,
            "{key} must not declare a default in the options block — its default lives in Vigil's own loader"
        );
        assert_eq!(
            section_scalar(&addon_config, "schema", key).as_deref(),
            Some("int?"),
            "{key} must be present under the HAOS add-on schema block as an optional integer"
        );
    }
}

/// (ii, fix cycle 9) Source-scan arm mirroring the offload-threshold check
/// above: `fabric.rs` must never carry its own copy of the worker-lease
/// literal, and must never construct `OffloadPolicyConfig` via a bare
/// `::default()` that silently drops whatever the operator configured — both
/// are true today (the live defect this run's RED 2/3 pins), so this is RED
/// until `fabric.rs` is rewired to consume the resolved config value.
#[test]
fn fabric_rs_never_hardcodes_the_tuning_defaults_outside_their_config_owned_home() {
    let vigil_src = repo_root().join("crates/vigil/src");
    let sources = collect_rust_source_files(&vigil_src);
    let mut failures = Vec::new();
    for source in &sources {
        if source.path.file_name() != Some(OsStr::new("fabric.rs")) {
            continue;
        }
        for forbidden in [
            "lease_duration_ms: 5 * 60_000",
            "OffloadPolicyConfig::default()",
        ] {
            if source.text.contains(forbidden) {
                failures.push(format!(
                    "{} hardcodes/bypasses the config-owned tuning default via `{forbidden}` — \
                     it must consume the resolved RuntimeConfig/FabricTuningIntent value instead",
                    source.path.display()
                ));
            }
        }
    }
    if !failures.is_empty() {
        panic!("{}", failures.join("\n"));
    }
}
