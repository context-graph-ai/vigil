use std::fs;
use std::path::{Path, PathBuf};

const ADDON_CONFIG_PATH: &str = "addons/vigil/config.yaml";
const ADDON_TRANSLATIONS_PATH: &str = "addons/vigil/translations/en.yaml";

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

fn top_level_scalar(text: &str, key: &str) -> Option<String> {
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("");
        let trimmed = line.trim();
        if trimmed.is_empty() || line.starts_with(' ') {
            continue;
        }
        if let Some((candidate, value)) = trimmed.split_once(':')
            && candidate == key
        {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn yaml_bool(value: &str) -> Option<bool> {
    match value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_ascii_lowercase()
        .as_str()
    {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn top_level_bool(text: &str, key: &str) -> Option<bool> {
    top_level_scalar(text, key).and_then(|value| yaml_bool(&value))
}

fn top_level_list_values(text: &str, section: &str) -> Vec<String> {
    let mut in_section = false;
    let mut values = Vec::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !line.starts_with(' ') {
            in_section = trimmed
                .strip_suffix(':')
                .map(|candidate| candidate == section)
                .unwrap_or(false);
            continue;
        }
        if in_section && line.starts_with("  - ") && !line.starts_with("    ") {
            let value = trimmed
                .trim_start_matches("- ")
                .trim()
                .trim_matches('"')
                .trim_matches('\'');
            if !value.is_empty() {
                values.push(value.to_string());
            }
        }
    }
    values
}

fn is_render_node(path: &str) -> bool {
    path.strip_prefix("/dev/dri/renderD")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()))
}

fn device_entry_is_narrow_graphics(entry: &str) -> bool {
    let mut parts = entry.split(':').map(str::trim);
    let source = parts.next().unwrap_or("");
    let destination = parts.next().unwrap_or("");
    let permissions = parts.next().unwrap_or("");
    parts.next().is_none()
        && is_render_node(source)
        && is_render_node(destination)
        && permissions == "rwm"
}

fn translation_description(text: &str) -> Option<String> {
    let mut in_configuration = false;
    let mut in_accelerated_detection = false;
    let mut collecting_block = false;
    let mut block = String::new();

    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !raw.starts_with(' ') {
            in_configuration = trimmed == "configuration:";
            in_accelerated_detection = false;
            collecting_block = false;
            continue;
        }
        if in_configuration && raw.starts_with("  ") && !raw.starts_with("    ") {
            in_accelerated_detection = trimmed == "accelerated_detection:";
            collecting_block = false;
            continue;
        }
        if in_configuration
            && in_accelerated_detection
            && raw.starts_with("    ")
            && !raw.starts_with("      ")
        {
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

#[test]
fn addon_config_exposes_both_booleans_in_options_and_schema_defaulting_true() {
    // Unfakeable because adding the switches only to options or only to
    // schema still fails the other half of the user-visible contract.
    let path = repo_root().join(ADDON_CONFIG_PATH);
    let text = fs::read_to_string(&path);
    assert!(
        text.is_ok(),
        "add-on config must be readable at {}",
        path.display()
    );
    let Ok(text) = text else {
        return;
    };

    for key in ["hardware_decoding", "accelerated_detection"] {
        assert_eq!(
            section_scalar(&text, "options", key).as_deref(),
            Some("true"),
            "{key} must be present under options and default true"
        );
        assert_eq!(
            section_scalar(&text, "schema", key).as_deref(),
            Some("bool"),
            "{key} must be present under schema as a boolean"
        );
    }
}

#[test]
fn addon_config_maps_video_device_and_forbids_full_access() {
    // Unfakeable because broad access and absent device mapping are checked
    // separately; either shortcut fails.
    let path = repo_root().join(ADDON_CONFIG_PATH);
    let text = fs::read_to_string(&path);
    assert!(
        text.is_ok(),
        "add-on config must be readable at {}",
        path.display()
    );
    let Ok(text) = text else {
        return;
    };

    assert_ne!(
        top_level_bool(&text, "full_access"),
        Some(true),
        "add-on must not use full_access for acceleration device access"
    );
    let privileged_scalar = top_level_scalar(&text, "privileged");
    assert!(
        privileged_scalar.as_deref().is_none_or(|value| {
            value.is_empty() || value == "[]" || yaml_bool(value) == Some(false)
        }),
        "add-on must reject every nonempty scalar privileged grant, got {privileged_scalar:?}"
    );
    let privileged_list = top_level_list_values(&text, "privileged");
    assert!(
        privileged_list.is_empty(),
        "add-on must reject every privileged capability list, got {privileged_list:?}"
    );
    assert_eq!(
        top_level_bool(&text, "host_network"),
        Some(true),
        "fabric requires host networking so advertised dynamic QUIC endpoints are reachable"
    );
    assert_eq!(
        top_level_bool(&text, "host_pid"),
        Some(false),
        "fabric networking does not authorize host PID namespace access"
    );
    assert_eq!(
        top_level_bool(&text, "apparmor"),
        Some(true),
        "the narrow host-network/device grant must retain AppArmor confinement"
    );
    let mapped_devices = top_level_list_values(&text, "devices");
    assert!(
        !mapped_devices.is_empty(),
        "add-on must map at least one narrow /dev/dri/renderD* device; video: true alone is not the reviewed host-access boundary"
    );
    for device in &mapped_devices {
        assert!(
            device_entry_is_narrow_graphics(device),
            "add-on devices entry must be an exact /dev/dri/renderD<number>:/dev/dri/renderD<number>:rwm mapping, got {device}"
        );
    }
}

#[test]
fn addon_config_states_accelerated_detection_gpu_with_cpu_fallback_in_help() {
    // Unfakeable because the assertion reads the Home Assistant option
    // description surface, not a separate prose file.
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
    let Ok(text) = text else {
        return;
    };
    let description = translation_description(&text);
    assert!(
        description.is_some(),
        "configuration.accelerated_detection.description must be present in add-on translations"
    );
    let Some(description) = description else {
        return;
    };
    let lower = description.to_ascii_lowercase();
    assert!(
        lower.contains("default") && (lower.contains("on") || lower.contains("true")),
        "description must say accelerated detection defaults on: {description}"
    );
    assert!(
        lower.contains("accelerat") && (lower.contains("gpu") || lower.contains("graphics")),
        "description must say detection accelerates when a usable GPU is present: {description}"
    );
    assert!(
        (lower.contains("fall back") || lower.contains("falls back") || lower.contains("fallback"))
            && lower.contains("cpu"),
        "description must say detection falls back to CPU when no usable GPU is present: {description}"
    );
    assert!(
        lower.contains("receipt")
            || lower.contains("why")
            || lower.contains("reason")
            || lower.contains("names"),
        "description must say the CPU fallback comes with a receipt naming why: {description}"
    );
    for forbidden in [
        "cannot accelerate detection",
        "does not accelerate detection",
        "doesn't accelerate detection",
        "accelerated detection is not available",
        "runs on cpu regardless",
        "cpu detection is the supported path",
    ] {
        assert!(
            !lower.contains(forbidden),
            "description must not keep the pre-promotion CPU-only wording `{forbidden}`: {description}"
        );
    }
}
