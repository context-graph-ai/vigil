use std::fs;
use std::path::{Path, PathBuf};

const ADDON_CONFIG_PATH: &str = "addons/vigil/config.yaml";
const ADDON_TRANSLATIONS_PATH: &str = "addons/vigil/translations/en.yaml";

// The two startup-probe deadlines are env vars today (VIGIL_DECODE_PROBE_DEADLINE_SECS
// in decode_gstreamer.rs, VIGIL_DETECTION_PROBE_DEADLINE_SECS in detection_accel.rs).
// A Home Assistant user cannot see or set an env var; these must be documented add-on
// options that the start path exports to those env vars.
const DECODE_OPTION: &str = "decode_probe_deadline_secs";
const DETECTION_OPTION: &str = "detection_probe_deadline_secs";
const DECODE_ENV_VAR: &str = "VIGIL_DECODE_PROBE_DEADLINE_SECS";
const DETECTION_ENV_VAR: &str = "VIGIL_DETECTION_PROBE_DEADLINE_SECS";
// Effective defaults read from source: Duration::from_secs(5) and
// Duration::from_millis(1800) => 5 seconds and 1.8 seconds.
const DECODE_DEFAULT_SECS: &str = "5";
const DETECTION_DEFAULT_SECS: &str = "1.8";

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

// Pull `configuration.<option>.description` out of the HA translations file, the
// same option-description surface addon_config_surface.rs reads, generalized to any
// option key. Handles inline, "|", and ">" scalar forms.
fn configuration_description(text: &str, option: &str) -> Option<String> {
    let mut in_configuration = false;
    let mut in_option = false;
    let mut collecting_block = false;
    let mut block = String::new();

    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !raw.starts_with(' ') {
            in_configuration = trimmed == "configuration:";
            in_option = false;
            collecting_block = false;
            continue;
        }
        if in_configuration && raw.starts_with("  ") && !raw.starts_with("    ") {
            in_option = trimmed == format!("{option}:");
            collecting_block = false;
            continue;
        }
        if in_configuration && in_option && raw.starts_with("    ") && !raw.starts_with("      ") {
            if let Some((key, value)) = trimmed.split_once(':')
                && key == "description"
            {
                let value = value.trim();
                if value == "|" || value == ">" {
                    collecting_block = true;
                    block.clear();
                    continue;
                }
                return Some(value.trim_matches('"').to_string());
            }
            collecting_block = false;
            continue;
        }
        if collecting_block && raw.starts_with("      ") {
            if !block.is_empty() {
                block.push(' ');
            }
            block.push_str(trimmed);
        }
    }

    (!block.is_empty()).then_some(block)
}

fn read_config() -> Option<String> {
    let path = repo_root().join(ADDON_CONFIG_PATH);
    let text = fs::read_to_string(&path);
    assert!(
        text.is_ok(),
        "add-on config must be readable at {}",
        path.display()
    );
    text.ok()
}

fn read_translations() -> Option<String> {
    let path = repo_root().join(ADDON_TRANSLATIONS_PATH);
    assert!(
        path.exists(),
        "add-on option descriptions must exist at {}",
        path.display()
    );
    let text = fs::read_to_string(&path);
    assert!(
        text.is_ok(),
        "add-on option descriptions must be readable at {}",
        path.display()
    );
    text.ok()
}

#[test]
fn addon_config_declares_both_probe_deadline_knobs_as_optional_integers() {
    // The probe deadlines must be schema options a Home Assistant user can find,
    // and optional (absent from the default options block) so leaving them unset
    // keeps the source default.
    let Some(text) = read_config() else {
        return;
    };

    for option in [DECODE_OPTION, DETECTION_OPTION] {
        assert_eq!(
            section_scalar(&text, "schema", option).as_deref(),
            Some("int?"),
            "{option} must be a schema option declared as an optional integer (int?) so a Home Assistant user can set the probe deadline"
        );
        assert_eq!(
            section_scalar(&text, "options", option),
            None,
            "{option} is optional and must not appear in the default options block, so leaving it unset keeps the source default"
        );
    }
}

#[test]
fn addon_help_documents_decode_probe_deadline_option() {
    let Some(text) = read_translations() else {
        return;
    };
    assert_probe_deadline_help(
        &text,
        DECODE_OPTION,
        DECODE_ENV_VAR,
        DECODE_DEFAULT_SECS,
        "decode",
    );
}

#[test]
fn addon_help_documents_detection_probe_deadline_option() {
    let Some(text) = read_translations() else {
        return;
    };
    assert_probe_deadline_help(
        &text,
        DETECTION_OPTION,
        DETECTION_ENV_VAR,
        DETECTION_DEFAULT_SECS,
        "detection",
    );
}

fn assert_probe_deadline_help(
    text: &str,
    option: &str,
    env_var: &str,
    default_secs: &str,
    subject: &str,
) {
    let description = configuration_description(text, option);
    assert!(
        description.is_some(),
        "configuration.{option}.description must be present so a Home Assistant user sees this probe deadline in the add-on help"
    );
    let Some(description) = description else {
        return;
    };
    let lower = description.to_ascii_lowercase();

    // (a) names the real default deadline in seconds.
    assert!(
        lower.contains(default_secs) && lower.contains("second"),
        "description must name the default {subject} probe deadline of {default_secs} seconds: {description}"
    );

    // (b) plain-language statement of what the knob does: how long the startup
    // hardware probe waits before falling back.
    assert!(
        lower.contains("probe"),
        "description must say this controls the startup hardware probe: {description}"
    );
    assert!(
        lower.contains("wait"),
        "description must say how long the probe waits: {description}"
    );
    assert!(
        lower.contains("fall back")
            || lower.contains("falls back")
            || lower.contains("fallback")
            || lower.contains("falling back"),
        "description must say the probe falls back when the deadline passes: {description}"
    );

    // (c) precedence: the option is written to the env var at add-on start, so it
    // wins over a manually set env var, and leaving it unset keeps the default.
    assert!(
        description.contains(env_var),
        "description must name the {env_var} environment variable so a docker or bare-binary user can map the option: {description}"
    );
    assert!(
        lower.contains("environment variable") || lower.contains("env var"),
        "description must state the option is written to the environment variable at add-on start: {description}"
    );
    assert!(
        lower.contains("wins") || lower.contains("overrides") || lower.contains("takes precedence"),
        "description must state the option value wins over a manually set environment variable: {description}"
    );
    assert!(
        (lower.contains("unset") || lower.contains("not set") || lower.contains("leave it"))
            && lower.contains("default"),
        "description must state that leaving the option unset keeps the default: {description}"
    );

    // (d) no internal planning vocabulary in shipped user help.
    for forbidden in [
        "§",
        "ledger",
        "dl-",
        "codename",
        "first-light",
        "capability a",
        "capability b",
        "mode-1",
        "mode-2",
        "moat",
    ] {
        assert!(
            !lower.contains(forbidden),
            "user-facing description must not carry internal planning vocabulary `{forbidden}`: {description}"
        );
    }
}
