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
fn hardware_decoding_and_accelerated_detection_default_true_in_vigils_own_loader() {
    // Superseded contract note: this test used to require both switches
    // present under `options:` defaulting to `true`. Under the
    // settings-authority direction, a behavior key that still declared a
    // default in `options:` would turn every fresh install into a pin and
    // block reset-by-absence, so the switches now carry no declared default
    // in the manifest at all — the true-by-default semantics moved into
    // Vigil's own config loader. Unfakeable the same way as before: the
    // manifest's schema-optional declaration and the loader's own
    // `unwrap_or(true)` are checked independently, so satisfying only one
    // half still fails the test.
    let text = addon_config_text();
    for key in ["hardware_decoding", "accelerated_detection"] {
        assert_eq!(
            section_scalar(&text, "schema", key).as_deref(),
            Some("bool?"),
            "{key} must be present under schema as an optional boolean"
        );
        assert_eq!(
            section_scalar(&text, "options", key),
            None,
            "{key} must not declare a default in the options block"
        );
    }

    let config_src_path = repo_root().join("crates/vigil/src/config.rs");
    let config_src = fs::read_to_string(&config_src_path).unwrap_or_else(|error| {
        panic!(
            "vigil config loader must be readable at {}: {error}",
            config_src_path.display()
        )
    });
    for expected in [
        "hardware_decoding: partial.hardware_decoding.unwrap_or(true),",
        "accelerated_detection: partial.accelerated_detection.unwrap_or(true),",
    ] {
        assert!(
            config_src.contains(expected),
            "Vigil's own config loader must default `{expected}` now that the add-on stops declaring it, found no match in {}",
            config_src_path.display()
        );
    }
}

#[test]
fn addon_config_camera_schema_exposes_usb_csi_and_mjpeg_source_fields() {
    // Unfakeable in the same way as the boolean test above: the three
    // source-kind fields already exist on the real Rust config types
    // (CameraEntry/CameraEntryPartial), but were entirely absent from the
    // add-on's `cameras` list schema until this test's own fix landed —
    // this reads the real per-camera schema block, not a copy.
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
    let lines: Vec<&str> = text.lines().collect();
    let schema_line = lines
        .iter()
        .position(|line| *line == "schema:")
        .expect("add-on config has a top-level schema section");
    let cameras_line = lines[schema_line..]
        .iter()
        .position(|line| *line == "  cameras:")
        .map(|offset| schema_line + offset)
        .expect("schema section has a cameras list");
    // The camera-item block runs from the line after `  cameras:` up to
    // (but not including) the next line indented at the SAME two-space
    // top-level-key depth — never a naive substring search, which cannot
    // distinguish "starts with two spaces" (a sibling top-level key) from
    // "starts with two spaces, followed by two more" (a nested list-item
    // field, which must stay inside the block).
    let block_end = lines[cameras_line + 1..]
        .iter()
        .position(|line| !line.starts_with("    "))
        .map(|offset| cameras_line + 1 + offset)
        .unwrap_or(lines.len());
    let camera_item_schema = lines[cameras_line..block_end].join("\n");
    for field in ["usb_device: str?", "csi_module: str?", "mjpeg_url: str?"] {
        assert!(
            camera_item_schema.contains(field),
            "the add-on's per-camera schema must declare `{field}`, got:\n{camera_item_schema}"
        );
    }
}

#[test]
fn ratified_keyframe_and_bitrate_defaults_stay_vigils_own_and_schema_declares_them_optional() {
    // Superseded contract note: this test used to require the eight
    // keyframe/bitrate keys present under `options:` at their ratified
    // value. Under the settings-authority direction those behavior keys
    // carry no declared default in the manifest at all, so the ratified
    // numeric values now live in Vigil's own `crate::encode`
    // `*_AUTOMATIC_DEFAULT` constants instead. Unfakeable the same way as
    // before: the schema declaration, the absence of an options default, and
    // the exact ratified constant are all checked independently, so
    // satisfying only some of the three still fails the test.
    let text = addon_config_text();
    let encode_src_path = repo_root().join("crates/vigil/src/encode.rs");
    let encode_src = fs::read_to_string(&encode_src_path).unwrap_or_else(|error| {
        panic!(
            "vigil encode defaults must be readable at {}: {error}",
            encode_src_path.display()
        )
    });

    for (key, const_name, expected_default) in [
        (
            "keyframe_interval_fps_multiplier",
            "KEYFRAME_INTERVAL_FPS_MULTIPLIER_AUTOMATIC_DEFAULT",
            "2",
        ),
        (
            "keyframe_interval_min_frames",
            "KEYFRAME_INTERVAL_MIN_FRAMES_AUTOMATIC_DEFAULT",
            "15",
        ),
        (
            "keyframe_interval_max_frames",
            "KEYFRAME_INTERVAL_MAX_FRAMES_AUTOMATIC_DEFAULT",
            "300",
        ),
        (
            "bitrate_bps_up_to_640x480",
            "BITRATE_BPS_UP_TO_640X480_AUTOMATIC_DEFAULT",
            "1_000_000",
        ),
        (
            "bitrate_bps_up_to_1280x720",
            "BITRATE_BPS_UP_TO_1280X720_AUTOMATIC_DEFAULT",
            "2_000_000",
        ),
        (
            "bitrate_bps_up_to_1920x1080",
            "BITRATE_BPS_UP_TO_1920X1080_AUTOMATIC_DEFAULT",
            "4_000_000",
        ),
        (
            "bitrate_bps_up_to_2560x1440",
            "BITRATE_BPS_UP_TO_2560X1440_AUTOMATIC_DEFAULT",
            "6_000_000",
        ),
        (
            "bitrate_bps_above_2560x1440",
            "BITRATE_BPS_ABOVE_2560X1440_AUTOMATIC_DEFAULT",
            "10_000_000",
        ),
    ] {
        assert_eq!(
            section_scalar(&text, "schema", key).as_deref(),
            Some("int?"),
            "{key} must be present under schema as an optional integer"
        );
        assert_eq!(
            section_scalar(&text, "options", key),
            None,
            "{key} must not declare a default in the options block"
        );
        let expected_decl = format!("pub const {const_name}: u32 = {expected_default};");
        assert!(
            encode_src.contains(&expected_decl),
            "Vigil's own encode defaults must keep {key}'s ratified value, expected `{expected_decl}` in {}",
            encode_src_path.display()
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

// ---------------------------------------------------------------------------
// The settings-surface contract on the add-on manifest.
//
// The Supervisor cannot show anyone the difference between an option nobody
// touched and one typed to the same value as the declared default, and the
// web interface posts the whole rendered form on save. So a behavior key that
// still declares a default in the `options:` block turns every fresh install
// into a screenful of pins, and removing such a key from the user's record
// refills the default rather than producing absence. Behavior keys therefore
// carry no declared default and are declared schema-optional, so "nothing set
// here" is expressible and Vigil owns the default.
// ---------------------------------------------------------------------------

/// The schema keys that are NOT behavior settings, and why each one is exempt.
/// Everything else the schema declares is a behavior key. Kept as one named
/// list because the exemption reason is not readable from the YAML itself:
///   - `store_path` is a bootstrap location — a store cannot say where the
///     store is, so it must resolve before the store opens.
///   - `fabric_ticket` is an enrollment credential, on the same footing as a
///     camera password rather than a behavior knob.
///   - `cameras` is the camera-list field, not a single setting.
const NON_BEHAVIOR_SCHEMA_KEYS: &[&str] = &["store_path", "fabric_ticket", "cameras"];

/// One top-level key of the add-on `schema:` block: its scalar declaration when
/// it has one, and its nested entries when it is declared as a list.
struct SchemaEntry {
    key: String,
    scalar: Option<String>,
    nested: Vec<String>,
}

/// Every top-level key declared under `schema:`, in declaration order. The
/// existing helpers answer "what does this one key say"; enumerating the block
/// is what makes the behavior-key set derived rather than hand-listed.
fn schema_entries(text: &str) -> Vec<SchemaEntry> {
    let mut entries: Vec<SchemaEntry> = Vec::new();
    let mut in_schema = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !line.starts_with(' ') {
            in_schema = trimmed.trim_end_matches(':') == "schema" && trimmed.ends_with(':');
            continue;
        }
        if !in_schema {
            continue;
        }
        if line.starts_with("  ") && !line.starts_with("   ") && !trimmed.starts_with("- ") {
            if let Some((key, value)) = trimmed.split_once(':') {
                let value = value.trim();
                entries.push(SchemaEntry {
                    key: key.trim().to_string(),
                    scalar: (!value.is_empty()).then(|| value.trim_matches('"').to_string()),
                    nested: Vec::new(),
                });
            }
            continue;
        }
        if trimmed.starts_with("- ")
            && let Some(current) = entries.last_mut()
        {
            current.nested.push(
                trimmed
                    .trim_start_matches("- ")
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            );
        }
    }
    entries
}

fn behavior_schema_keys(text: &str) -> Vec<String> {
    schema_entries(text)
        .into_iter()
        .filter(|entry| !NON_BEHAVIOR_SCHEMA_KEYS.contains(&entry.key.as_str()))
        .map(|entry| entry.key)
        .collect()
}

fn addon_config_text() -> String {
    let path = repo_root().join(ADDON_CONFIG_PATH);
    fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "add-on config must be readable at {}: {error}",
            path.display()
        )
    })
}

#[test]
fn no_behavior_key_carries_a_declared_default_in_the_options_block() {
    // Unfakeable because the behavior-key set is DERIVED from the schema block
    // in the same file rather than hand-listed, so adding a new behavior key
    // with a declared default fails this test the moment it is declared. The
    // exemptions are a short named list with a stated reason each, not a
    // silent skip.
    let text = addon_config_text();
    let behavior_keys = behavior_schema_keys(&text);
    assert!(
        behavior_keys.len() >= 20,
        "the schema must still declare the behavior surface, derived {behavior_keys:?}"
    );

    let mut offenders: Vec<String> = Vec::new();
    for key in &behavior_keys {
        if let Some(declared) = section_scalar(&text, "options", key) {
            offenders.push(format!("{key} = {declared}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "no behavior key may declare a default in the options block, found {offenders:?}"
    );

    // The exemptions are exemptions from the behavior rule, not from the file.
    assert!(
        section_scalar(&text, "schema", "store_path").is_some(),
        "the bootstrap store location stays declared"
    );
}

#[test]
fn every_behavior_key_is_declared_schema_optional() {
    // Unfakeable because it reads each key's own schema declaration and
    // requires the optional marker on it. A key declared `int` rather than
    // `int?` is required in every write, which both blocks absence and makes
    // any partial write rejected outright — the failure this criterion exists
    // to prevent. A list-declared key carries its optionality on its ELEMENT
    // line rather than beside the key, so it is checked there instead of here;
    // a bare `- str` element makes the list mandatory and refuses a fresh
    // installation outright, which is a live-measured fact and not the "lists
    // are optional whatever they say" belief this exemption once rested on.
    let text = addon_config_text();
    let entries = schema_entries(&text);

    let mut required: Vec<String> = Vec::new();
    for entry in &entries {
        if NON_BEHAVIOR_SCHEMA_KEYS.contains(&entry.key.as_str()) {
            continue;
        }
        match (&entry.scalar, entry.nested.is_empty()) {
            (Some(declaration), _) => {
                if !declaration.ends_with('?') {
                    required.push(format!("{} = {declaration}", entry.key));
                }
            }
            // A list-declared key: rendered optional by the platform.
            (None, false) => {}
            (None, true) => panic!(
                "schema key `{}` declares neither a type nor a list",
                entry.key
            ),
        }
    }
    assert!(
        required.is_empty(),
        "every behavior key must be declared schema-optional, found required {required:?}"
    );
}

#[test]
fn the_detection_class_list_is_a_loose_list_of_strings_not_an_enumeration() {
    // Superseded contract note: this test read the class-list rule off
    // `recognition_covered_classes` and called that key "the detection class
    // list". It is not one — it is what recognition puts a name to. Naming one
    // key for both is the coupling that makes a person who widens recognition
    // silently widen what the machine detects, and leaves a person who wants to
    // detect vehicles without recognizing anything with no field to say it in.
    // So the rule is asserted where it belongs, on the detector's own class key,
    // and the recognition key is checked to still carry it separately — the two
    // are one contract each, never one key serving two meanings.
    //
    // Unfakeable in the same way as before, now twice over: each key is
    // asserted for the positive loose-list form AND for the absence of the
    // pipe-separated enumeration and of any frozen class name, so widening the
    // closed twelve-value enumeration to eighty values satisfies no part of it,
    // and collapsing the two keys back into one fails the key that disappears.
    let text = addon_config_text();
    for key in ["detector_classes", "recognition_covered_classes"] {
        let entry = schema_entries(&text)
            .into_iter()
            .find(|entry| entry.key == key)
            .unwrap_or_else(|| panic!("the schema must declare {key}"));

        let declarations: Vec<String> = match (&entry.scalar, entry.nested.is_empty()) {
            (Some(scalar), _) => vec![scalar.clone()],
            (None, false) => entry.nested.clone(),
            (None, true) => panic!("{key} declares nothing"),
        };

        assert!(
            declarations
                .iter()
                .any(|declaration| declaration.trim_end_matches('?') == "str"),
            "{key} must be declared as the documented Home Assistant loose-list-of-strings \
             grammar — a `- str?` element line, never the choice-validator spelling \
             `list(str)` (which means \"the value must equal the literal string `str`\" in the \
             Supervisor's schema grammar) — got {declarations:?}"
        );
        for declaration in &declarations {
            assert!(
                !declaration.contains('|'),
                "{key} must not enumerate the model's classes in the packaging, got {declaration}"
            );
            for class in ["person", "dog", "cat", "bird", "car", "truck"] {
                assert!(
                    !declaration.contains(class),
                    "the class name `{class}` must not be frozen into {key}, got {declaration}"
                );
            }
        }
    }
}

#[test]
fn the_detector_queue_capacity_frame_sampling_and_confidence_threshold_have_schema_keys() {
    // Unfakeable because widening the detection class set with nothing to
    // adjust is the dead end this checks for: these three controls exist in the
    // runtime today but appear in neither the options nor the schema block, so
    // an operator whose machine cannot keep up has no add-on surface to reach
    // them through. Each is checked for a schema declaration and for the
    // absence of an options default, so satisfying it by re-adding a default
    // fails the other half.
    let text = addon_config_text();
    let declared: Vec<String> = schema_entries(&text)
        .into_iter()
        .map(|entry| entry.key)
        .collect();

    for key in [
        "detector_queue_capacity",
        "detector_sample_frames",
        "detector_confidence_threshold",
    ] {
        assert!(
            declared.iter().any(|candidate| candidate == key),
            "the add-on schema must declare `{key}`, declared {declared:?}"
        );
        let declaration = section_scalar(&text, "schema", key)
            .unwrap_or_else(|| panic!("`{key}` must carry a scalar schema declaration"));
        assert!(
            declaration.ends_with('?'),
            "`{key}` must be declared schema-optional, got {declaration}"
        );
        assert_eq!(
            section_scalar(&text, "options", key),
            None,
            "`{key}` must not declare a default in the options block"
        );
    }
}

#[test]
fn the_two_backend_settings_are_declared_as_loose_strings() {
    // Unfakeable because it requires the loose string form specifically:
    // enumerating the backend names in the manifest would freeze a property of
    // the BUILD into the packaging, when the accelerated backend exists only
    // where its feature is compiled in. Vigil validates the value against what
    // the running artifact actually carries; the manifest must not pre-empt
    // that.
    let text = addon_config_text();
    for key in ["detection_backend", "decode_backend"] {
        let declaration = section_scalar(&text, "schema", key)
            .unwrap_or_else(|| panic!("the add-on schema must declare `{key}`"));
        assert_eq!(
            declaration, "str?",
            "`{key}` must be a loose optional string, got {declaration}"
        );
        assert_eq!(
            section_scalar(&text, "options", key),
            None,
            "`{key}` must not declare a default in the options block"
        );
    }
}

#[test]
fn the_manifest_declares_supervisor_api_access_with_the_role_the_self_write_requires() {
    // Unfakeable because it reads the manifest's own grant rather than any
    // prose about it: without a Supervisor API declaration the token Vigil's
    // own container holds cannot write Vigil's own options, so the whole
    // reflection promise is undelivered no matter what the code does. The role
    // is asserted present and not `admin`, because admin is not pre-approved —
    // reaching for it is a decision to surface, never one to take in passing.
    let text = addon_config_text();
    assert_eq!(
        top_level_bool(&text, "hassio_api"),
        Some(true),
        "the manifest must declare Supervisor API access for the self options-write"
    );
    let role = top_level_scalar(&text, "hassio_role")
        .expect("the manifest must declare the Supervisor role the self options-write needs");
    let role = role.trim().trim_matches('\'').to_ascii_lowercase();
    assert!(
        !role.is_empty(),
        "the declared Supervisor role must not be blank"
    );
    assert_ne!(
        role, "admin",
        "admin is not the least role that permits an add-on to write its own options"
    );
}

// ---------------------------------------------------------------------------
// The manifest and the reflection roster are one declaration, and the manifest
// carries every setting an operator is promised.
//
// Reflection pre-validates a value against the add-on's declared schema type
// before writing it, and posts the complete record because a write is a full
// replace. Both of those read the roster in `settings_reflection`, so a roster
// that disagrees with the manifest does not merely go stale: a key the roster
// invents is posted into a schema that does not declare it, a key the roster
// omits can never reflect at all, and a type the roster gets wrong inverts the
// pre-validation — judging the value the Supervisor would accept unwritable and
// the value it would reject safe.
//
// A per-camera key is addressed as `cameras[].<name>`, because a camera-scoped
// setting and a site-scoped one of the same name are two different declarations
// and a flat name cannot tell them apart.
// ---------------------------------------------------------------------------

/// The prefix that addresses one key of the `cameras:` list-element schema.
const CAMERA_KEY_PREFIX: &str = "cameras[].";

/// One declared key: what the schema calls it, the type it declares, and
/// whether every write has to carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeclaredKey {
    key: String,
    declared_type: String,
    required: bool,
}

/// The declared type and optionality of one scalar schema declaration, as the
/// manifest spells it: a trailing `?` is the optional marker.
fn declared_scalar(declaration: &str) -> DeclaredKey {
    let declaration = declaration.trim();
    let required = !declaration.ends_with('?');
    DeclaredKey {
        key: String::new(),
        declared_type: declaration.trim_end_matches('?').to_string(),
        required,
    }
}

/// Every key of the `cameras:` list-element schema, addressed with the
/// per-camera prefix. The block is read on its own because the list-element
/// entries sit two levels in and are a separate declaration from the top level.
fn camera_schema_keys(text: &str) -> Vec<DeclaredKey> {
    let mut keys = Vec::new();
    let mut in_schema = false;
    let mut in_cameras = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !line.starts_with(' ') {
            in_schema = trimmed.ends_with(':') && trimmed.trim_end_matches(':') == "schema";
            in_cameras = false;
            continue;
        }
        if !in_schema {
            continue;
        }
        if line.starts_with("  ") && !line.starts_with("   ") {
            in_cameras = trimmed.trim_end_matches(':') == "cameras";
            continue;
        }
        if !in_cameras {
            continue;
        }
        let entry = trimmed.trim_start_matches("- ").trim();
        if let Some((key, declaration)) = entry.split_once(':') {
            let mut declared = declared_scalar(declaration);
            declared.key = format!("{CAMERA_KEY_PREFIX}{}", key.trim());
            keys.push(declared);
        }
    }
    keys
}

/// The complete manifest schema as a name/type/optionality map: every top-level
/// key plus every per-camera key. The `cameras` container itself contributes
/// its element keys rather than a key of its own — it is a list, not a setting.
fn manifest_declared_keys(text: &str) -> Vec<DeclaredKey> {
    let mut keys: Vec<DeclaredKey> = Vec::new();
    for entry in schema_entries(text) {
        if entry.key == "cameras" {
            continue;
        }
        match (&entry.scalar, entry.nested.first()) {
            (Some(scalar), _) => {
                let mut declared = declared_scalar(scalar);
                declared.key = entry.key.clone();
                keys.push(declared);
            }
            // A list-declared key: the platform renders a list schema as
            // optional whatever marker it carries. The manifest's own element
            // line just says `str` (the documented HA loose-list grammar), so
            // a list-of-str key and a scalar `str` key would otherwise
            // collide on this comparison's vocabulary; `list_of()` marks it
            // as a list distinctly, in the SAME internal spelling
            // `roster_type_spelling` below uses for `AddonSchemaType::ListOfStr`
            // — never the Supervisor's real `list(str)` choice-validator
            // grammar, which this manifest text does not use.
            (None, Some(nested)) => keys.push(DeclaredKey {
                key: entry.key.clone(),
                declared_type: list_of(nested.trim_end_matches('?')),
                required: false,
            }),
            (None, None) => panic!("schema key `{}` declares nothing", entry.key),
        }
    }
    keys.extend(camera_schema_keys(text));
    keys
}

/// A list-of-`element` type, in this test's own internal comparison
/// vocabulary — deliberately NOT the Supervisor's `list(a|b)` choice-validator
/// grammar (a different construct the manifest never uses here), just a
/// label distinct from a bare scalar `element` key so the two sides of
/// `the_reflection_roster_and_the_manifest_declare_the_same_keys_types_and_optionality`
/// can agree without either claiming a wire spelling neither of them writes.
fn list_of(element: &str) -> String {
    format!("list_of({element})")
}

/// The reflection roster's own spelling of a declared type, so the two
/// declarations are compared in one vocabulary rather than through a guess.
fn roster_type_spelling(declared: vigil::settings_reflection::AddonSchemaType) -> String {
    use vigil::settings_reflection::AddonSchemaType;
    match declared {
        AddonSchemaType::Bool => "bool".to_string(),
        AddonSchemaType::Int => "int".to_string(),
        AddonSchemaType::Float => "float".to_string(),
        AddonSchemaType::Str => "str".to_string(),
        AddonSchemaType::ListOfStr => list_of("str"),
    }
}

fn roster_declared_keys() -> Vec<DeclaredKey> {
    vigil::settings_reflection::addon_schema_keys()
        .into_iter()
        .map(|declared| DeclaredKey {
            key: declared.key.to_string(),
            declared_type: roster_type_spelling(declared.declared_type).to_string(),
            required: declared.required,
        })
        .collect()
}

#[test]
fn the_reflection_roster_and_the_manifest_declare_the_same_keys_types_and_optionality() {
    // Unfakeable because it is a two-way comparison of two independently
    // authored declarations, and every leg is asserted separately: a roster
    // trimmed down to whatever the manifest happens to declare fails the
    // manifest-side leg, a manifest widened to whatever the roster claims fails
    // the roster-side leg, and a name-only match with the wrong type or the
    // wrong optionality fails the two legs after them. Neither declaration can
    // be satisfied by editing the other alone.
    let text = addon_config_text();
    let manifest = manifest_declared_keys(&text);
    let roster = roster_declared_keys();
    assert!(
        manifest.len() > 20 && roster.len() > 20,
        "sanity: both declarations must have been read, got {} manifest keys and {} roster keys",
        manifest.len(),
        roster.len()
    );

    let missing_from_roster: Vec<&str> = manifest
        .iter()
        .filter(|declared| !roster.iter().any(|other| other.key == declared.key))
        .map(|declared| declared.key.as_str())
        .collect();
    assert!(
        missing_from_roster.is_empty(),
        "the manifest declares keys the reflection roster does not, so a value for each of them \
         can never be mirrored onto the page the user trusts: {missing_from_roster:?}"
    );

    let missing_from_manifest: Vec<&str> = roster
        .iter()
        .filter(|declared| !manifest.iter().any(|other| other.key == declared.key))
        .map(|declared| declared.key.as_str())
        .collect();
    assert!(
        missing_from_manifest.is_empty(),
        "the reflection roster claims keys the manifest schema does not declare, and a write is a \
         full replace validated against the posted content alone, so posting them fails the write \
         outright: {missing_from_manifest:?}"
    );

    let mut wrong_type: Vec<String> = Vec::new();
    let mut wrong_optionality: Vec<String> = Vec::new();
    for declared in &manifest {
        let Some(other) = roster.iter().find(|other| other.key == declared.key) else {
            continue;
        };
        if other.declared_type != declared.declared_type {
            wrong_type.push(format!(
                "{}: manifest says {}, roster says {}",
                declared.key, declared.declared_type, other.declared_type
            ));
        }
        if other.required != declared.required {
            wrong_optionality.push(format!(
                "{}: manifest required={}, roster required={}",
                declared.key, declared.required, other.required
            ));
        }
    }
    assert!(
        wrong_type.is_empty(),
        "a declared type the roster gets wrong inverts the pre-validation — the value the \
         Supervisor would accept is judged unwritable and the value it would coerce is judged \
         safe: {wrong_type:?}"
    );
    assert!(
        wrong_optionality.is_empty(),
        "a required key must appear in every write and an optional key the user set is erased by \
         a write that omits it, so the two declarations must agree on which is which: \
         {wrong_optionality:?}"
    );
}

/// The settings whose scope is one camera rather than the whole node, and the
/// per-camera key each is declared under. Named here because the mapping is a
/// product fact — a stream belongs to a camera — that the setting name alone
/// does not carry.
const CAMERA_SCOPED_SETTINGS: &[(&str, &str)] = &[
    ("camera_name", "cameras[].name"),
    ("rtsp_url", "cameras[].rtsp_url"),
    ("live_rtsp_url", "cameras[].live_rtsp_url"),
];

/// The settings an operator sets per camera AND for the whole node, so the
/// manifest owes them a key at both scopes.
const BOTH_SCOPED_SETTINGS: &[&str] = &["motion_sensitivity"];

#[test]
fn the_manifest_declares_a_key_for_every_setting_the_operator_surface_answers_for() {
    // Unfakeable because the promised set is READ OFF the operator surface a
    // real deployment renders, never retyped here: a setting an operator can
    // see and set with no add-on key is a setting a Home Assistant user cannot
    // reach at all, and one added later without a key fails here rather than
    // being discovered by the person it was promised to. The per-camera mapping
    // is a short named list with its reason, not a silent skip.
    //
    // Withdrawing a setting is therefore one act, not two: a setting leaves the
    // declared roster AND its add-on key goes at the same time. Dropping the key
    // while the surface still answers for the setting fails here — correctly,
    // because that state is a control a person can see and cannot reach — and
    // the fix is to finish the withdrawal, never to put the key back.
    let text = addon_config_text();
    let declared: Vec<String> = manifest_declared_keys(&text)
        .into_iter()
        .map(|declared| declared.key)
        .collect();

    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = vigil::settings_store::SettingsStore::open(directory.path())
        .expect("open the node-side settings store");
    drop(store);
    let target = vigil::settings_model::ScopeTarget {
        tenant: "node-a".to_string(),
        site: "node-a".to_string(),
        node: "node-a".to_string(),
        camera: None,
    };
    let report = vigil::settings_projection::report_by_direct_read_at(directory.path(), &target)
        .expect("the operator report for this deployment");
    let prefix = format!(
        "{} {}=",
        vigil::settings_projection::SETTING_LINE_PREFIX,
        vigil::settings_projection::NAME_KEY
    );
    let answered: Vec<String> = report
        .render_lines()
        .iter()
        .filter_map(|line| line.strip_prefix(prefix.as_str()).map(str::to_string))
        .filter_map(|rest| rest.split_whitespace().next().map(str::to_string))
        .collect();
    assert!(
        answered.len() > 20,
        "sanity: the surface answers for the declared roster, got {answered:?}"
    );

    let mut unreachable: Vec<String> = Vec::new();
    for setting in &answered {
        let expected: Vec<String> = match CAMERA_SCOPED_SETTINGS
            .iter()
            .find(|(name, _)| name == setting)
        {
            Some((_, camera_key)) => vec![(*camera_key).to_string()],
            None if BOTH_SCOPED_SETTINGS.contains(&setting.as_str()) => {
                vec![setting.clone(), format!("{CAMERA_KEY_PREFIX}{setting}")]
            }
            None => vec![setting.clone()],
        };
        for key in expected {
            if !declared.contains(&key) {
                unreachable.push(format!("{setting} -> {key}"));
            }
        }
    }
    assert!(
        unreachable.is_empty(),
        "every setting the operator surface answers for is one a Home Assistant user must be able \
         to set, and the add-on schema is the only place they can. Either declare the key, or — \
         if this setting governs nothing any more — withdraw it from the roster too, so the \
         surface stops answering for it: {unreachable:?}"
    );

    // The four the promise names in as many words, asserted by name as well, so
    // a roster that quietly stopped answering for one of them cannot make this
    // pass by shrinking the derived set.
    for key in [
        "detector_classes",
        "motion_sensitivity",
        "cameras[].motion_sensitivity",
        "site_name",
        "restart_on_reflect",
    ] {
        assert!(
            declared.contains(&key.to_string()),
            "the manifest must declare `{key}`, got {declared:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The manifest and the config-file reader are one declaration: the twin of
// the manifest<->operator-surface parity check above, but reversed. That
// check reads what the operator surface already answers for and requires the
// manifest to carry a key for it; it says nothing about a key the manifest
// declares that the config-file loader has no field for at all — exactly the
// silent-drop shape the five previously-undeliverable add-on keys took
// (`PartialConfig`/`CameraEntryPartial` had no field, so a value typed there
// was dropped by `serde` before it ever reached `surface_assertions`). So the
// next key added to `config.yaml` without a matching reader field must fail
// HERE, not be discovered on a Home Assistant user's options page.
// ---------------------------------------------------------------------------

/// Deployment-wide behavior keys that do NOT go through the config-file
/// loader's `surface_assertions` reader this test drives — each one is
/// deliverable through a DIFFERENT, already-proven reader instead, so
/// exempting it here is not a silent skip:
///   - the four keyframe keys and the five bitrate keys route through the
///     separate typed settings registry (`SettingSpec`/`SettingHandle`) that
///     `ratified_keyframe_and_bitrate_defaults_stay_vigils_own` above checks
///     against its own `*_AUTOMATIC_DEFAULT` constants.
const CONFIG_LOADER_EXEMPT_BEHAVIOR_KEYS: &[&str] = &[
    "keyframe_interval_fps_multiplier",
    "keyframe_interval_min_frames",
    "keyframe_interval_max_frames",
    "bitrate_bps_up_to_640x480",
    "bitrate_bps_up_to_1280x720",
    "bitrate_bps_up_to_1920x1080",
    "bitrate_bps_up_to_2560x1440",
    "bitrate_bps_above_2560x1440",
];

/// A minimal, syntactically valid TOML literal for one scalar schema
/// declaration (`int`, `int?`, `str`, ...), shared by the deployment-wide and
/// the per-camera probes below — both name a single key and need only a
/// value of the right shape, never a particular one.
fn probe_scalar_literal(declared_type: &str, key: &str) -> String {
    match declared_type.trim_end_matches('?') {
        "bool" => "true".to_string(),
        "int" => "7".to_string(),
        "float" => "0.5".to_string(),
        "str" => "\"probe-value\"".to_string(),
        other => panic!("`{key}`: unhandled scalar schema declaration `{other}`"),
    }
}

/// A minimal, syntactically valid TOML literal for one schema key's declared
/// type, so a fragment naming just that key parses through the REAL loader.
/// The literal's own value is never asserted — only that the loader reads it
/// at all — so any value of the right shape does the job.
fn probe_toml_literal(entry: &SchemaEntry) -> String {
    match &entry.scalar {
        Some(scalar) => probe_scalar_literal(scalar, &entry.key),
        // A list-declared key (`detector_classes`, `recognition_covered_classes`).
        None => "[\"probe-item\"]".to_string(),
    }
}

/// Per-camera schema keys that are NOT deliverable through the settings-store
/// camera reader (`camera_surface_entries`) this test's second half drives —
/// each one is deliverable through a DIFFERENT, already-proven reader
/// instead, so exempting it here is not a silent skip:
///   - `name` is the address a camera's own settings are filed under, not a
///     setting itself — every probe below names it to give the camera an
///     identity, and it is what the delivery check below looks the probed
///     entry up by.
///   - `rtsp_url`/`live_rtsp_url`/`username`/`password`/`usb_device`/
///     `csi_module`/`mjpeg_url` are the camera's stream identity and
///     credentials, kept OUT of the settings store deliberately (a settings
///     record is read back on an operator surface, and a stream URL can
///     carry the camera's credentials in its userinfo — the same exclusion
///     `surface_assertions` documents for the deployment-wide stream). They
///     reach the runtime instead through `CameraEntry`/`CameraSourceKind`
///     resolution, proven end-to-end by `camera_config_schema.rs` against
///     the same production loader (`camera_source_summaries_from_args`).
const CAMERA_CONFIG_LOADER_EXEMPT_KEYS: &[&str] = &[
    "name",
    "rtsp_url",
    "live_rtsp_url",
    "username",
    "password",
    "usb_device",
    "csi_module",
    "mjpeg_url",
];

/// A syntactically valid TOML value for a camera source field, so the probe
/// camera entry below always resolves a source kind and passes the real
/// loader's exactly-one-source-kind check — the check the deployment-wide
/// probe above never has to satisfy, since it names no camera at all.
const CAMERA_PROBE_RTSP_URL: &str = "\"rtsp://probe.local/stream\"";

/// Unfakeable because it drives the REAL production config loader
/// (`vigil::surface_authoring_from_args`, the same seam
/// `retry_and_probe_setting_authoring.rs` uses) with a file naming ONLY the
/// one key under test, and requires that key to appear among what the loader
/// says the config-file surface asserts. A key the manifest declares that
/// `PartialConfig` has no field for is silently dropped by `serde` before
/// `surface_authoring_from_args` ever sees it — exactly the defect class this
/// guards, and exactly what left `motion_sensitivity`, `restart_on_reflect`,
/// `detector_queue_capacity`, `detection_backend` and `decode_backend`
/// silent before this arc's fix landed.
///
/// The second half repeats the same probe one scope down, over the
/// `cameras:` list-element schema (`camera_schema_keys`), against the
/// per-camera reader (`SurfaceAuthoring::camera_entries`) instead of the
/// deployment-wide one — so a future `cameras[].foo` key declared without a
/// matching `CameraEntryPartial` field and a `camera_surface_entries` arm
/// fails HERE too, rather than passing silently one level below where the
/// first half already looks.
#[test]
fn every_manifest_declared_deployment_wide_setting_is_deliverable_by_the_config_loader() {
    let text = addon_config_text();
    let behavior_keys = behavior_schema_keys(&text);
    let candidates: Vec<&str> = behavior_keys
        .iter()
        .map(String::as_str)
        .filter(|key| !CONFIG_LOADER_EXEMPT_BEHAVIOR_KEYS.contains(key))
        .collect();
    assert!(
        candidates.len() >= 15,
        "sanity: expected a substantial set of loader-checked keys, got {candidates:?}"
    );

    let entries = schema_entries(&text);
    let directory = tempfile::tempdir().expect("temporary data directory");
    let mut unreachable: Vec<String> = Vec::new();
    for key in candidates {
        let entry = entries
            .iter()
            .find(|entry| entry.key == key)
            .unwrap_or_else(|| panic!("`{key}` must be a declared schema entry"));
        let fragment = format!("cameras = []\n{key} = {}\n", probe_toml_literal(entry));
        let config_path = directory.path().join(format!("{key}.toml"));
        fs::write(&config_path, &fragment)
            .unwrap_or_else(|error| panic!("write probe config for `{key}`: {error}"));

        let surfaces = vigil::surface_authoring_from_args(vec![
            std::ffi::OsString::from("--config"),
            config_path.as_os_str().to_owned(),
        ])
        .unwrap_or_else(|error| {
            panic!("the real config loader must accept a file naming only `{key}`: {error}")
        });
        let delivered = surfaces
            .into_iter()
            .flat_map(|surface| surface.entries)
            .any(|(name, _)| name == key);
        if !delivered {
            unreachable.push(key.to_string());
        }
    }
    assert!(
        unreachable.is_empty(),
        "the manifest declares these keys but the config-file loader has no reader for them, so \
         a value typed on the add-on options page for any of them is silently dropped: \
         {unreachable:?}"
    );

    // The per-camera half: every `cameras[].<key>` the schema declares, minus
    // the identity/credential keys proven through the other reader above,
    // must reach the SAME camera's record in `camera_entries`.
    let camera_keys = camera_schema_keys(&text);
    let camera_candidates: Vec<&DeclaredKey> = camera_keys
        .iter()
        .filter(|declared| {
            let bare = declared
                .key
                .strip_prefix(CAMERA_KEY_PREFIX)
                .unwrap_or(declared.key.as_str());
            !CAMERA_CONFIG_LOADER_EXEMPT_KEYS.contains(&bare)
        })
        .collect();
    assert!(
        !camera_candidates.is_empty(),
        "sanity: expected at least one loader-checked per-camera key, got none from {camera_keys:?}"
    );

    let mut camera_unreachable: Vec<String> = Vec::new();
    for declared in camera_candidates {
        let bare = declared
            .key
            .strip_prefix(CAMERA_KEY_PREFIX)
            .expect("camera_schema_keys always prefixes with CAMERA_KEY_PREFIX");
        let value = probe_scalar_literal(&declared.declared_type, &declared.key);
        let fragment = format!(
            "cameras = [{{ name = \"probe-camera\", rtsp_url = {CAMERA_PROBE_RTSP_URL}, \
             {bare} = {value} }}]\n"
        );
        let config_path = directory.path().join(format!("camera-{bare}.toml"));
        fs::write(&config_path, &fragment)
            .unwrap_or_else(|error| panic!("write probe config for `{}`: {error}", declared.key));

        let surfaces = vigil::surface_authoring_from_args(vec![
            std::ffi::OsString::from("--config"),
            config_path.as_os_str().to_owned(),
        ])
        .unwrap_or_else(|error| {
            panic!(
                "the real config loader must accept a probe camera naming only `{}`: {error}",
                declared.key
            )
        });
        let delivered = surfaces.into_iter().any(|surface| {
            surface.camera_entries.iter().any(|(camera, entries)| {
                camera == "probe-camera" && entries.iter().any(|(name, _)| name == bare)
            })
        });
        if !delivered {
            camera_unreachable.push(declared.key.clone());
        }
    }
    assert!(
        camera_unreachable.is_empty(),
        "the manifest declares these per-camera keys but the config-file loader has no reader \
         that delivers them to that camera's own record, so a value typed into a camera's row \
         on the add-on options page for any of them is silently dropped: {camera_unreachable:?}"
    );
}

/// Every top-level key `configuration:` in `translations/en.yaml` carries a
/// `description:` for — a 2-space-indented key under `configuration:` with a
/// 4-space-indented `description:` line nested directly under it. Camera
/// fields are declared once at `cameras.description` (Home Assistant has no
/// per-nested-field translation address for a list schema, and the manifest
/// itself declares them the same way — see `camera_schema_keys` above), so
/// this returns only top-level keys, matching what `manifest_declared_keys`
/// returns before its own camera-field expansion.
fn translated_configuration_keys(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut in_configuration = false;
    let mut current_key: Option<String> = None;
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !raw.starts_with(' ') {
            in_configuration = trimmed == "configuration:";
            current_key = None;
            continue;
        }
        if !in_configuration {
            continue;
        }
        if raw.starts_with("  ") && !raw.starts_with("    ") {
            current_key = trimmed.trim_end_matches(':').to_string().into();
            continue;
        }
        if raw.starts_with("    ")
            && !raw.starts_with("      ")
            && trimmed.starts_with("description:")
            && let Some(key) = current_key.take()
        {
            keys.push(key);
        }
    }
    keys
}

/// Beside the manifest/roster two-way test above: every schema key the
/// manifest declares must carry a translation, so the 26-of-41 hole
/// cold-review-arc2-r5 finding 13 found (only `accelerated_detection` and
/// the probe-deadline keys were asserted anywhere) cannot reopen on the next
/// key added to `config.yaml` without a matching translation entry. This is
/// a ONE-DIRECTION check — the manifest must never outrun the translations
/// — not a two-way equality: a translation entry describing a key the
/// manifest no longer declares is stale prose, not a broken promise to an
/// operator, and is not this test's job to police.
#[test]
fn every_manifest_schema_key_carries_a_translation() {
    let manifest_text = addon_config_text();
    let manifest_keys: Vec<String> = manifest_declared_keys(&manifest_text)
        .into_iter()
        .map(|declared| declared.key)
        // Camera-nested fields (`cameras[].name`, ...) have no per-field
        // translation address; `cameras` itself, asserted below, is what
        // Home Assistant actually renders a description for.
        .filter(|key| !key.starts_with(CAMERA_KEY_PREFIX))
        .collect();
    assert!(
        manifest_keys.len() > 20,
        "sanity: expected to read a substantial manifest key set, got {}: {manifest_keys:?}",
        manifest_keys.len()
    );

    let translations_path = repo_root().join(ADDON_TRANSLATIONS_PATH);
    let translations_text = fs::read_to_string(&translations_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", translations_path.display()));
    let translated_keys = translated_configuration_keys(&translations_text);
    assert!(
        translated_keys.len() > 20,
        "sanity: expected to read a substantial translated key set, got {}: {translated_keys:?}",
        translated_keys.len()
    );

    let untranslated: Vec<&String> = manifest_keys
        .iter()
        .filter(|key| !translated_keys.contains(key))
        .collect();
    assert!(
        untranslated.is_empty(),
        "every key `config.yaml`'s schema declares must carry a `configuration.<key>.description` \
         entry in {} — a Home Assistant user reading this key's row on the add-on options page \
         must never see a blank description; missing: {untranslated:?}",
        ADDON_TRANSLATIONS_PATH
    );

    // `cameras` itself, named explicitly: the one key every camera-nested
    // field above is filtered out in favor of.
    assert!(
        translated_keys.contains(&"cameras".to_string()),
        "the manifest's `cameras` key must carry its own translation, since its nested fields \
         carry none: {translated_keys:?}"
    );
}
