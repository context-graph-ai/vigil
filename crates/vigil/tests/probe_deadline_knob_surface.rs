use std::fs;
use std::path::{Path, PathBuf};

const ADDON_CONFIG_PATH: &str = "addons/vigil/config.yaml";
const ADDON_TRANSLATIONS_PATH: &str = "addons/vigil/translations/en.yaml";

// The two startup-probe deadlines are declared store settings under the
// settings-authority model (`DECODE_PROBE_DEADLINE_SECS_SETTING` /
// `DETECTION_PROBE_DEADLINE_SECS_SETTING` in settings_model.rs, resolved
// through settings_backends.rs like any other behavior value) — a Home
// Assistant user reaches them as an add-on option, which authors into the
// store the same way `vigil settings set` or a config-file field would.
// `*_ENV_VAR` below are no longer a real surface: the old env vars are
// deleted and reported-as-ignored (settings_environment.rs), kept here only
// so the help text is asserted to have dropped the now-false promise that it
// used to make about them.
const DECODE_OPTION: &str = "decode_probe_deadline_secs";
const DECODE_ENV_VAR: &str = "VIGIL_DECODE_PROBE_DEADLINE_SECS";
// The effective default the shipped source must document: decode gathers real
// video for Duration::from_secs(5) => 5 seconds before it decides.
const DECODE_DEFAULT_SECS: &str = "5";

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
fn addon_config_declares_the_decode_probe_sample_wait_as_an_optional_integer() {
    // How long a stream session gathers real video before choosing a decode
    // path is a knob a Home Assistant user can find, and optional (absent from
    // the default options block) so leaving it unset keeps the source default.
    //
    // Detection has no equivalent. Its preparation is decided by its own
    // outcome, so there is no waiting period for anyone to lengthen and no
    // knob to offer; `probe_timer_removal.rs` holds that absence.
    let Some(text) = read_config() else {
        return;
    };

    let option = DECODE_OPTION;
    assert_eq!(
        section_scalar(&text, "schema", option).as_deref(),
        Some("int?"),
        "{option} must be a schema option declared as an optional integer (int?) so a Home Assistant user can set how long a stream gathers real video before choosing a decode path"
    );
    assert_eq!(
        section_scalar(&text, "options", option),
        None,
        "{option} is optional and must not appear in the default options block, so leaving it unset keeps the source default"
    );
}

#[test]
fn the_decode_probe_sample_wait_defaults_to_the_documented_number_of_seconds() {
    // The help beside the option tells an operator what leaving it unset gets
    // them, and this reads that same number off the setting itself. Without
    // it, the documented default and the one a deployment actually resolves
    // are two independent statements, and a person who trusts the first is the
    // one who finds out they disagree.
    let (value, reason) = vigil::settings_backends::automatic_default(
        vigil::settings_model::DECODE_PROBE_DEADLINE_SECS_SETTING,
    )
    .expect("vigil owns a default for the decode probe sample wait");
    assert_eq!(
        value,
        vigil::settings_model::SettingValue::Int(
            DECODE_DEFAULT_SECS
                .parse::<i64>()
                .expect("the documented default is a whole number of seconds")
        ),
        "the value a deployment resolves with nobody having set this must be the one the add-on \
         help documents"
    );
    assert!(
        !reason.trim().is_empty(),
        "a value vigil chooses for itself names why it chose it"
    );
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

    // (c) superseded contract note: this used to require the description name
    // the env var and claim the option "wins over" a manually set one. Under
    // the settings-authority direction the env var is deleted and
    // reported-as-ignored — the option authors straight into the store like
    // any other setting, so there is no environment variable left to win
    // against. Surviving content: the option stays discoverable and honest —
    // no reference to the old env var, no environment-variable-precedence
    // claim of any kind, and leaving it unset still keeps the default, which
    // is still true (it's the resolved setting's automatic floor now, not an
    // export target).
    assert!(
        !description.contains(env_var),
        "description must not name the deleted {env_var} environment variable, which is no longer a real surface: {description}"
    );
    assert!(
        !lower.contains("environment variable") && !lower.contains("env var"),
        "description must not claim environment-variable behavior — the env var is deleted and reported as ignored, not written to at add-on start: {description}"
    );
    assert!(
        !(lower.contains("wins")
            || lower.contains("overrides")
            || lower.contains("takes precedence")),
        "description must not claim the option wins over or overrides an environment variable that no longer exists: {description}"
    );
    assert!(
        (lower.contains("unset") || lower.contains("not set") || lower.contains("leave it"))
            && (lower.contains("default")
                || lower.contains("automatic")
                || lower.contains("vigil's own")),
        "description must state that leaving the option unset keeps Vigil's own value — the settings-authority model calls this state Automatic, so \"default\" is not the only honest wording: {description}"
    );
    // (c1) the description must name the real setting spelling — the same
    // key `vigil settings set` and the config file both address it by — so a
    // user reading the option screen and a user reading `vigil settings
    // list` are looking at the same named thing, not two unrelated surfaces
    // that happen to agree by coincidence.
    assert!(
        description.contains(option),
        "description must name the real setting spelling ({option}) so the option screen and `vigil settings` agree on what this is called: {description}"
    );

    // (c2) accuracy about where the setting lives beyond this one option
    // screen: a Home Assistant user reads this description once, but the
    // same setting is also reachable through the config file and
    // `vigil settings`, and staying silent about that would be its own
    // inaccuracy now that the setting is store-backed rather than
    // env-exported. Loose phrasing on purpose — the wording is the doc
    // author's call, this only checks the surfaces are named somewhere.
    assert!(
        lower.contains("config file")
            || lower.contains("vigil settings")
            || lower.contains("settings command"),
        "description must name at least one other surface this setting is reachable through (the config file or `vigil settings`), the same way `ignored_behavior_variables`'s message names every surface a behavior setting lives on: {description}"
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
