use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const MANIFEST_PATH: &str = "addons/vigil/runtime-packages.yaml";
const ADDON_DOCKERFILE_PATH: &str = "addons/vigil/Dockerfile";
const GENERIC_DOCKERFILE_PATH: &str = "Dockerfile";
const ADDON_BASE: &str = "ghcr.io/home-assistant/base:3.22";

#[derive(Debug, Clone, Default)]
struct RuntimeProfile {
    name: String,
    base: String,
    arch: String,
    artifact: String,
    hardware_enabled: bool,
    decode_backend: String,
    detector_backend: String,
    packages: Vec<String>,
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn strip_yaml_quotes(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
}

fn set_profile_field(profile: &mut RuntimeProfile, key: &str, value: &str) -> Result<(), String> {
    let value = strip_yaml_quotes(value);
    match key {
        "name" => profile.name = value,
        "base" => profile.base = value,
        "arch" => profile.arch = value,
        "artifact" => profile.artifact = value,
        "hardware_enabled" => {
            profile.hardware_enabled = match value.as_str() {
                "true" => true,
                "false" => false,
                other => return Err(format!("hardware_enabled must be boolean, got {other}")),
            }
        }
        "decode_backend" => profile.decode_backend = value,
        "detector_backend" => profile.detector_backend = value,
        "packages" => {
            if !value.is_empty() && value != "[]" {
                return Err("packages must be a YAML list".to_string());
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_runtime_manifest(text: &str) -> Result<Vec<RuntimeProfile>, String> {
    let mut profiles = Vec::new();
    let mut current: Option<RuntimeProfile> = None;
    let mut in_packages = false;

    for (line_no, raw) in text.lines().enumerate() {
        let without_comment = raw.split('#').next().unwrap_or("").trim();
        if without_comment.is_empty() || without_comment == "profiles:" {
            continue;
        }

        if let Some(rest) = without_comment.strip_prefix("- name:") {
            if let Some(profile) = current.take() {
                profiles.push(profile);
            }
            let profile = RuntimeProfile {
                name: strip_yaml_quotes(rest),
                ..RuntimeProfile::default()
            };
            current = Some(profile);
            in_packages = false;
            continue;
        }

        let Some(profile) = current.as_mut() else {
            return Err(format!("line {} appears before a profile", line_no + 1));
        };

        if in_packages && without_comment.starts_with("- ") {
            let package = strip_package_version(strip_yaml_quotes(&without_comment[2..]).as_str());
            if package.is_empty() {
                return Err(format!("line {} has an empty package", line_no + 1));
            }
            profile.packages.push(package);
            continue;
        }

        let Some((key, value)) = without_comment.split_once(':') else {
            return Err(format!("line {} is not parseable", line_no + 1));
        };
        in_packages = key.trim() == "packages";
        set_profile_field(profile, key.trim(), value.trim())?;
    }

    if let Some(profile) = current.take() {
        profiles.push(profile);
    }

    if profiles.is_empty() {
        return Err("manifest contains no profiles".to_string());
    }
    for profile in &profiles {
        for missing in [
            (profile.name.is_empty(), "name"),
            (profile.base.is_empty(), "base"),
            (profile.arch.is_empty(), "arch"),
            (profile.artifact.is_empty(), "artifact"),
            (profile.decode_backend.is_empty(), "decode_backend"),
            (profile.detector_backend.is_empty(), "detector_backend"),
        ] {
            if missing.0 {
                return Err(format!("profile is missing {}", missing.1));
            }
        }
    }
    Ok(profiles)
}

fn load_manifest_for_assertions() -> Option<Vec<RuntimeProfile>> {
    let path = repo_root().join(MANIFEST_PATH);
    assert!(
        path.exists(),
        "runtime package manifest must exist at {}",
        path.display()
    );
    let text = fs::read_to_string(&path);
    assert!(
        text.is_ok(),
        "runtime package manifest must be readable at {}",
        path.display()
    );
    let Ok(text) = text else {
        return None;
    };
    let parsed = parse_runtime_manifest(&text);
    assert!(
        parsed.is_ok(),
        "runtime package manifest must parse as structured profiles: {:?}",
        parsed.err()
    );
    parsed.ok()
}

fn strip_package_version(package: &str) -> String {
    package
        .split(['=', '<', '>', '~'])
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn shell_tokens(command: &str) -> Vec<String> {
    command
        .split(|ch: char| {
            ch.is_whitespace() || matches!(ch, '[' | ']' | ',' | '"' | '\'' | '(' | ')')
        })
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn shell_command_segments(command: &str) -> Vec<(usize, &str)> {
    let mut segments = Vec::new();
    let mut start = 0usize;
    let bytes = command.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b';' {
            segments.push((start, &command[start..idx]));
            start = idx + 1;
            idx += 1;
            continue;
        }
        if bytes[idx] == b'&' && bytes.get(idx + 1) == Some(&b'&') {
            segments.push((start, &command[start..idx]));
            start = idx + 2;
            idx += 2;
            continue;
        }
        idx += 1;
    }
    segments.push((start, &command[start..]));
    segments
}

fn apk_add_invocations_with_prefix(command: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut invocations = Vec::new();
    for (segment_start, install) in shell_command_segments(command) {
        let tokens = shell_tokens(install);
        let Some(apk_at) = tokens
            .iter()
            .position(|token| token == "apk" || token.ends_with("/apk"))
        else {
            continue;
        };
        let mut add_at = apk_at + 1;
        while add_at < tokens.len() && tokens[add_at].starts_with('-') {
            add_at += 1;
        }
        if tokens.get(add_at).map(String::as_str) != Some("add") {
            return Err(format!("unparseable apk install command: {command}"));
        }

        let packages = tokens
            .iter()
            .skip(add_at + 1)
            .filter(|token| !token.starts_with('-'))
            .map(|token| strip_package_version(token))
            .filter(|package| !package.is_empty())
            .collect::<Vec<_>>();
        let apk_token = tokens[apk_at].to_ascii_lowercase();
        let install_lower = install.to_ascii_lowercase();
        let apk_byte = install_lower.find(&apk_token).unwrap_or(0);
        let prefix = command[..segment_start + apk_byte].to_string();
        invocations.push((prefix, packages));
    }
    Ok(invocations)
}

fn contains_apk_add_invocation(text: &str) -> bool {
    let tokens = shell_tokens(text);
    for (idx, token) in tokens.iter().enumerate() {
        if token != "apk" && !token.ends_with("/apk") {
            continue;
        }
        let mut add_at = idx + 1;
        while add_at < tokens.len() && tokens[add_at].starts_with('-') {
            add_at += 1;
        }
        if tokens.get(add_at).map(String::as_str) == Some("add") {
            return true;
        }
    }
    false
}

fn package_variable_name(package_arg: &str) -> Option<String> {
    let dollar_at = package_arg.find('$')?;
    let after_dollar = &package_arg[dollar_at + 1..];
    if let Some(braced) = after_dollar.strip_prefix('{') {
        let end = braced.find('}')?;
        let variable = &braced[..end];
        return (!variable.is_empty()).then(|| variable.to_string());
    }
    let variable = after_dollar
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect::<String>();
    (!variable.is_empty()).then_some(variable)
}

fn manifest_assignment_slice<'a>(command_prefix: &'a str, variable: &str) -> Option<&'a str> {
    let variable = variable.to_ascii_lowercase();
    let assignment = format!("{variable}=");
    let lower = command_prefix.to_ascii_lowercase();
    let mut effective = None;
    for (idx, _) in lower.match_indices(&assignment) {
        let previous = lower[..idx].chars().next_back();
        if previous.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
            continue;
        }
        let slice = &command_prefix[idx..];
        let end = shell_assignment_end(slice).unwrap_or(slice.len());
        effective = Some(&slice[..end]);
    }
    effective
}

fn command_prefix_reassigns_build_arch(command_prefix: &str) -> bool {
    let lower = command_prefix.to_ascii_lowercase();
    lower.match_indices("build_arch=").any(|(idx, _)| {
        let previous = lower[..idx].chars().next_back();
        !previous.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    })
}

fn shell_assignment_end(slice: &str) -> Option<usize> {
    let mut single_quoted = false;
    let mut double_quoted = false;
    let mut escaped = false;
    let mut paren_depth = 0usize;
    for (idx, ch) in slice.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if single_quoted {
            if ch == '\'' {
                single_quoted = false;
            }
            continue;
        }
        if double_quoted {
            if ch == '"' {
                double_quoted = false;
            }
            continue;
        }
        match ch {
            '\'' => single_quoted = true,
            '"' => double_quoted = true,
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            ';' if paren_depth == 0 => return Some(idx),
            '&' if paren_depth == 0 && slice[idx..].starts_with("&&") => return Some(idx),
            _ => {}
        }
    }
    None
}

fn shell_assignment_rhs_uses_command_substitution(assignment: &str) -> bool {
    assignment
        .split_once('=')
        .map(|(_, rhs)| {
            rhs.trim_start()
                .trim_start_matches(['"', '\''])
                .starts_with("$(")
        })
        .unwrap_or(false)
}

fn token_names_command(token: &str, command_name: &str) -> bool {
    token == command_name || token.ends_with(&format!("/{command_name}"))
}

fn assignment_mentions_manifest_package_literal(
    assignment: &str,
    manifest_packages: &BTreeSet<String>,
) -> Option<String> {
    shell_tokens(assignment)
        .into_iter()
        .map(|token| strip_package_version(&token))
        .find(|token| manifest_packages.contains(token))
}

fn compact_manifest_assignment(assignment: &str) -> String {
    assignment
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| !ch.is_whitespace() && !matches!(ch, '"' | '\'' | '`'))
        .collect()
}

fn assignment_selects_addon_build_arch_profile(assignment: &str) -> bool {
    let compact = compact_manifest_assignment(assignment);
    let profile_gate = "in_packages=seen_artifact&&seen_arch";
    let reversed_profile_gate = "in_packages=seen_arch&&seen_artifact";
    let resets_at_next_profile = compact.contains("/^-name:/{in_packages=0}")
        || compact.contains("/^-[[:space:]]*name:/{in_packages=0}");
    let prints_selected_package = compact.contains("in_packages&&/^[[:space:]]*-/{print$2}")
        || compact.contains("/^[[:space:]]*-/&&in_packages{print$2}");
    compact.contains("-vartifact=addon")
        && compact.contains("-varch=$build_arch")
        && compact.contains("/artifact:/")
        && compact.contains("/arch:/")
        && compact.contains("/packages:/")
        && compact.matches("-vartifact=").count() == 1
        && compact.matches("-varch=").count() == 1
        && compact.matches("artifact=addon").count() == 1
        && compact.matches("arch=$build_arch").count() == 1
        && compact.contains("{seen_artifact=$2==artifact}")
        && compact.contains("{seen_arch=$2==arch}")
        && compact.matches("seen_artifact=").count() == 1
        && compact.matches("seen_arch=").count() == 1
        && (compact.contains(profile_gate) || compact.contains(reversed_profile_gate))
        && compact.matches("in_packages=").count() == 2
        && resets_at_next_profile
        && prints_selected_package
        && compact.matches("print").count() == 1
        && !compact.contains('|')
        && !compact.contains("||")
        && !compact.contains("++")
        && !compact.contains("--")
        && !compact.contains("+=")
        && !compact.contains("-=")
        && !compact.contains("arch=amd64")
        && !compact.contains("arch=aarch64")
        && !compact.contains("arch=arm64")
        && !compact.contains("artifact=generic-docker")
        && !compact.contains("artifact=static-musl")
        && !compact.contains("begin{")
        && !compact.contains("exit")
        && !compact.contains("next")
        && ![
            "head", "tail", "grep", "sed", "cut", "sort", "uniq", "xargs",
        ]
        .iter()
        .any(|filter| compact.contains(filter))
}

fn assignment_reader_ends_at_manifest(assignment: &str) -> bool {
    let lower = assignment.to_ascii_lowercase();
    let manifest_path = "addons/vigil/runtime-packages.yaml";
    if lower.matches("runtime-packages.yaml").count() != 1 {
        return false;
    }
    let Some(manifest_at) = lower.find(manifest_path) else {
        return false;
    };
    lower[manifest_at + manifest_path.len()..]
        .chars()
        .all(|ch| ch.is_whitespace() || matches!(ch, '"' | '\'' | ')'))
}

fn command_assigns_package_variable_from_manifest(
    command_prefix: &str,
    variable: &str,
    manifest_packages: &BTreeSet<String>,
) -> Result<(), String> {
    let Some(assignment) = manifest_assignment_slice(command_prefix, variable) else {
        return Err(format!(
            "variable ${variable} is not assigned before apk add"
        ));
    };
    let lower_assignment = assignment.to_ascii_lowercase();
    if !shell_assignment_rhs_uses_command_substitution(assignment) {
        return Err(format!(
            "variable ${variable} must be assigned by a command substitution that reads runtime-packages.yaml"
        ));
    }
    let tokens = shell_tokens(&lower_assignment);
    let has_manifest_reader = tokens.iter().any(|token| {
        ["awk", "yq", "python", "python3", "ruby"]
            .into_iter()
            .any(|command_name| token_names_command(token, command_name))
    });
    if !has_manifest_reader {
        return Err(format!(
            "variable ${variable} must be assigned by a structured manifest reader, not a shell literal"
        ));
    }
    if tokens
        .iter()
        .any(|token| token_names_command(token, "echo") || token_names_command(token, "printf"))
    {
        return Err(format!(
            "variable ${variable} must not be manufactured by echo/printf around a duplicated package list"
        ));
    }
    if let Some(package) =
        assignment_mentions_manifest_package_literal(&lower_assignment, manifest_packages)
    {
        return Err(format!(
            "variable ${variable} assignment hardcodes manifest package {package:?}"
        ));
    }
    for required in [
        "runtime-packages.yaml",
        "build_arch",
        "addon",
        "artifact",
        "arch",
        "packages",
    ] {
        if !lower_assignment.contains(required) {
            return Err(format!(
                "variable ${variable} assignment must select {required} from runtime-packages.yaml"
            ));
        }
    }
    if !assignment_selects_addon_build_arch_profile(assignment) {
        return Err(format!(
            "variable ${variable} assignment must extract packages only from the addon profile whose arch equals BUILD_ARCH"
        ));
    }
    if command_prefix_reassigns_build_arch(command_prefix) {
        return Err(format!(
            "variable ${variable} assignment must use Home Assistant BUILD_ARCH directly, without reassigning BUILD_ARCH before apk add"
        ));
    }
    if !assignment_reader_ends_at_manifest(assignment) {
        return Err(format!(
            "variable ${variable} assignment must stop after reading runtime-packages.yaml, without appending extra package output"
        ));
    }
    Ok(())
}

fn assert_package_variable_manifest_guard_rejects_literal_lists() {
    let manifest_packages =
        BTreeSet::from(["mesa-va-gallium".to_string(), "gstreamer-vaapi".to_string()]);
    let hardcoded_with_manifest_words = r#"RUN PACKAGES="$(echo mesa-va-gallium gstreamer-vaapi; echo runtime-packages.yaml build_arch addon artifact arch packages)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            hardcoded_with_manifest_words,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject variables that hardcode a package list and merely mention the manifest selector words"
    );

    let all_packages_reader = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/packages:/ { in_packages = 1 } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            all_packages_reader,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that ignore the effective BUILD_ARCH profile and print every package"
    );

    let overwritten_profile_gate = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch; in_packages = 1 } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            overwritten_profile_gate,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that mention BUILD_ARCH selection but overwrite it before printing packages"
    );

    let boolean_bypass_reader = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact || 1 } /arch:/ { seen_arch = $2 == arch || 1 } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            boolean_bypass_reader,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that turn the artifact/arch selectors into always-true expressions"
    );

    let reassigned_after_manifest_query = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && PACKAGES="$PACKAGES bash" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            reassigned_after_manifest_query,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must validate the effective package variable assignment immediately before apk add, not the first plausible manifest assignment"
    );

    let truncated_manifest_query = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml | head -n 1)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            truncated_manifest_query,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that truncate or filter the selected profile package output"
    );

    let appended_output_query = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml; cat /tmp/extra-packages)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            appended_output_query,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject command substitutions that append extra package output after the manifest read"
    );

    let temp_manifest_reader = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' /tmp/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            temp_manifest_reader,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that use an untracked runtime-packages.yaml copy"
    );

    let second_apk_reassignment = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES && PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml; cat /tmp/extra-packages)" && apk add --no-cache $PACKAGES"#;
    let second_invocations = apk_add_invocations_with_prefix(second_apk_reassignment)
        .expect("second apk reassignment sample must parse");
    let second_prefix = second_invocations
        .last()
        .map(|(prefix, _)| prefix.as_str())
        .expect("second apk reassignment sample must include an apk add");
    assert!(
        command_assigns_package_variable_from_manifest(
            second_prefix,
            "PACKAGES",
            &manifest_packages
        )
        .is_err(),
        "manifest package guard must validate the effective assignment before each apk add invocation, including the second apk in one RUN"
    );

    let mutates_selector_state = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } { seen_artifact++ } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            mutates_selector_state,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that mutate selector flags after comparing artifact/arch"
    );

    let overrides_build_arch_selector = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" 'BEGIN { arch = "amd64" } /artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            overrides_build_arch_selector,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that override the BUILD_ARCH selector after wiring it"
    );

    let overrides_build_arch_before_reader = r#"RUN BUILD_ARCH=amd64 && PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            overrides_build_arch_before_reader,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject Dockerfile commands that override Home Assistant BUILD_ARCH before the manifest reader"
    );

    let begin_only_manifest_shape = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" 'BEGIN { seen_artifact = $2 == artifact } BEGIN { seen_arch = $2 == arch } BEGIN { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            begin_only_manifest_shape,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that only mimic selector syntax in BEGIN blocks without scanning manifest lines"
    );

    let exits_after_first_package = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2; exit }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            exits_after_first_package,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that stop after the first selected package"
    );

    let metadata_leaking_query = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            metadata_leaking_query,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that keep printing metadata from later profiles after the selected packages"
    );

    let incomplete_profile_query = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/^- name:/ { in_packages = 0 } /artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages && /^[[:space:]]*-/ && $2 != "gstreamer-vaapi" { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            incomplete_profile_query,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that filter the selected profile package set"
    );

    let manifest_query = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/^- name:/ { in_packages = 0 } /artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages && /^[[:space:]]*-/ { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_package_variable_from_manifest(
            manifest_query,
            "PACKAGES",
            &manifest_packages,
        )
        .is_ok(),
        "manifest package guard must allow a variable read from runtime-packages.yaml with artifact, BUILD_ARCH, arch, and packages selectors"
    );
}

fn assert_addon_apk_installs_are_manifest_driven(dockerfile: &str, profiles: &[RuntimeProfile]) {
    assert_package_variable_manifest_guard_rejects_literal_lists();
    let manifest_packages = profiles
        .iter()
        .filter(|profile| profile.artifact == "addon" && profile.base == ADDON_BASE)
        .flat_map(|profile| profile.packages.iter().cloned())
        .collect::<BTreeSet<_>>();
    // The manifest single-source contract binds the SHIPPED image stage: the
    // final stage's apk installs must be manifest-expanded variables only. A
    // builder stage compiles the binary and never ships its filesystem, so a
    // literal toolchain/dev-header install there cannot drift the runtime
    // package story the store, docs, and diagnostics read.
    let commands = dockerfile_final_stage_commands(dockerfile);
    assert!(
        commands.is_ok(),
        "add-on Dockerfile commands must be parseable: {:?}",
        commands.err()
    );
    let Some(commands) = commands.ok() else {
        return;
    };
    let mut saw_apk_add = false;
    for command in commands {
        let invocations = apk_add_invocations_with_prefix(&command);
        assert!(
            invocations.is_ok(),
            "add-on Dockerfile apk install lines must be parseable: {:?}",
            invocations.err()
        );
        let Some(invocations) = invocations.ok() else {
            continue;
        };
        if invocations.is_empty() {
            continue;
        }
        saw_apk_add = true;
        for (prefix, packages) in invocations {
            for package_arg in packages {
                let variable = package_variable_name(&package_arg);
                assert!(
                    variable.is_some(),
                    "add-on apk install command must pass manifest-expanded package variables only; literal package token {package_arg:?} can drift from runtime-packages.yaml: {command}"
                );
                let Some(variable) = variable else {
                    continue;
                };
                let assignment_result = command_assigns_package_variable_from_manifest(
                    &prefix,
                    &variable,
                    &manifest_packages,
                );
                assert!(
                    assignment_result.is_ok(),
                    "add-on apk install command must assign ${variable} from runtime-packages.yaml using the addon BUILD_ARCH profile immediately before each apk add, not hide a duplicated literal list behind a variable: {command}"
                );
            }
        }
    }
    assert!(
        saw_apk_add,
        "add-on Dockerfile must install runtime packages in a build layer from runtime-packages.yaml"
    );
}

fn dockerfile_commands(dockerfile: &str) -> Result<Vec<String>, String> {
    let mut commands = Vec::new();
    let mut current = String::new();
    for raw in dockerfile.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(stripped) = line.strip_suffix('\\') {
            current.push_str(stripped);
            current.push(' ');
            continue;
        }
        current.push_str(line);
        commands.push(current.trim().to_string());
        current.clear();
    }
    if !current.trim().is_empty() {
        return Err("Dockerfile has an unfinished line continuation".to_string());
    }
    Ok(commands)
}

fn copied_runtime_sources(dockerfile: &str) -> Result<Vec<PathBuf>, String> {
    let mut sources = Vec::new();
    for command in dockerfile_commands(dockerfile)? {
        let Some((instruction, rest)) = command.split_once(char::is_whitespace) else {
            continue;
        };
        let instruction = instruction.to_ascii_uppercase();
        if instruction != "COPY" && instruction != "ADD" {
            continue;
        }
        let rest = rest.trim();
        if rest.contains("--from=") || rest.contains("--from ") {
            continue;
        }
        if rest.starts_with('[') {
            let quoted = rest
                .split('"')
                .enumerate()
                .filter_map(|(idx, part)| (idx % 2 == 1).then_some(part))
                .collect::<Vec<_>>();
            if quoted.len() > 1 {
                for source in &quoted[..quoted.len() - 1] {
                    if *source != "vigil" && !source.starts_with('/') {
                        sources.push(PathBuf::from(source));
                    }
                }
            }
            continue;
        }
        let parts = rest
            .split_whitespace()
            .filter(|part| !part.starts_with("--"))
            .map(|part| part.trim_matches('"').trim_matches('\''))
            .collect::<Vec<_>>();
        if parts.len() > 1 {
            for source in &parts[..parts.len() - 1] {
                if *source != "vigil" && !source.starts_with('/') {
                    sources.push(PathBuf::from(source));
                }
            }
        }
    }
    Ok(sources)
}

fn copy_or_add_paths(command: &str) -> Vec<String> {
    let Some((instruction, rest)) = command.split_once(char::is_whitespace) else {
        return Vec::new();
    };
    let instruction = instruction.to_ascii_uppercase();
    if instruction != "COPY" && instruction != "ADD" {
        return Vec::new();
    }
    let mut rest = rest.trim();
    while rest.starts_with("--") {
        let Some((_, tail)) = rest.split_once(char::is_whitespace) else {
            return Vec::new();
        };
        rest = tail.trim_start();
    }
    if rest.starts_with('[') {
        return rest
            .split('"')
            .enumerate()
            .filter_map(|(idx, part)| (idx % 2 == 1).then_some(part.to_string()))
            .collect();
    }
    rest.split_whitespace()
        .filter(|part| !part.starts_with("--"))
        .map(|part| part.trim_matches('"').trim_matches('\'').to_string())
        .collect()
}

fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

fn normalize_docker_path(path: &str) -> String {
    path.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_end_matches('/')
        .replace('\\', "/")
}

fn resolve_workdir(current: &str, next: &str) -> String {
    let next = normalize_docker_path(next);
    if next.starts_with('/') {
        return next;
    }
    let current = normalize_docker_path(current);
    if current.is_empty() || current == "/" {
        format!("/{next}")
    } else {
        format!("{current}/{next}")
    }
}

fn canonical_docker_path(path: &str) -> String {
    let normalized = normalize_docker_path(path);
    if normalized == "." {
        return String::new();
    }
    normalized
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

fn resolved_docker_path(path: &str, workdir: &str) -> String {
    let normalized = normalize_docker_path(path);
    if normalized.starts_with('/') {
        return canonical_docker_path(&normalized);
    }
    let workdir = canonical_docker_path(workdir);
    let relative = canonical_docker_path(&normalized);
    if workdir.is_empty() || relative.is_empty() {
        return [workdir, relative]
            .into_iter()
            .find(|part| !part.is_empty())
            .unwrap_or_default();
    }
    format!("{workdir}/{relative}")
}

fn destination_resolves_to_start_binary(destination: &str, workdir: &str) -> bool {
    if destination_names_start_binary(destination) {
        return true;
    }
    let destination = normalize_docker_path(destination);
    let workdir = normalize_docker_path(workdir);
    (destination.is_empty() || destination == ".")
        && (workdir == "/usr/local/bin" || workdir == "/usr/local")
}

fn run_layer_references_start_binary(body: &str, workdir: &str) -> bool {
    let normalized = body
        .to_ascii_lowercase()
        .replace('\\', "/")
        .replace(">>", " ")
        .replace(['>', ';'], " ");
    if normalized.contains("/usr/local/bin/vigil") || normalized.contains("/usr/local/bin/") {
        return true;
    }
    shell_tokens(&normalized).into_iter().any(|token| {
        let token = token
            .trim_start_matches(['>', '<'])
            .trim_end_matches([';', '&'])
            .trim();
        resolved_docker_path(token, workdir) == "usr/local/bin/vigil"
    })
}

fn assert_start_binary_destination_guard_catches_workdir_relative_copy() {
    assert!(
        destination_resolves_to_start_binary(".", "/usr/local/bin"),
        "COPY --from=builder /tmp/vigil . under WORKDIR /usr/local/bin must count as replacing /usr/local/bin/vigil"
    );
    assert!(
        destination_resolves_to_start_binary("/usr/local", "/"),
        "COPY overlay/ /usr/local must count as able to replace /usr/local/bin/vigil"
    );
    assert!(
        destination_resolves_to_start_binary("usr/local", "/"),
        "COPY overlay/ usr/local (without a leading slash) must count as able to replace /usr/local/bin/vigil"
    );
    assert!(
        destination_resolves_to_start_binary(".", "/usr/local"),
        "COPY overlay/ . under WORKDIR /usr/local must count as able to replace /usr/local/bin/vigil"
    );
    assert!(
        destination_resolves_to_start_binary("./vigil", "/app"),
        "relative COPY destinations named vigil must count as start-binary writes"
    );
    assert!(
        !destination_resolves_to_start_binary(".", "/app"),
        "COPY . under another workdir must not be confused with the direct start binary path"
    );
    assert!(
        run_layer_references_start_binary("printf x > bin/vigil", "/usr/local"),
        "RUN redirection to bin/vigil under WORKDIR /usr/local must count as modifying /usr/local/bin/vigil"
    );
    assert!(
        run_layer_references_start_binary("cp /tmp/wrapper ./vigil", "/usr/local/bin"),
        "RUN copy to ./vigil under WORKDIR /usr/local/bin must count as modifying /usr/local/bin/vigil"
    );
    assert!(
        !run_layer_references_start_binary("cargo build -p vigil", "/app"),
        "RUN commands in unrelated workdirs must not be confused with start-binary writes"
    );
}

fn destination_names_start_binary(destination: &str) -> bool {
    let destination = normalize_docker_path(destination);
    let relative_destination = destination.trim_start_matches("./").trim_start_matches('/');
    destination == "vigil"
        || relative_destination == "vigil"
        || relative_destination.ends_with("/vigil")
        || relative_destination == "usr/local"
        || destination.ends_with("/usr/local/bin")
        || destination.ends_with("/usr/local/bin/")
}

fn source_is_rust_target_vigil_binary(source: &str) -> bool {
    let name = basename(source).to_ascii_lowercase();
    let normalized = source.replace('\\', "/");
    name == "vigil"
        && (normalized.starts_with("target/") || normalized.contains("/target/"))
        && (normalized.contains("/release/") || normalized.contains("/debug/"))
}

fn command_uses_builder_stage(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("--from=") || lower.contains("--from ")
}

fn run_layer_can_modify_path(command: &str, path: &str) -> bool {
    let lower = command.to_ascii_lowercase().replace('\\', "/");
    let path = path.trim_start_matches("./").to_ascii_lowercase();
    let path_tail = path.trim_start_matches('/').to_string();
    let mentions_path = lower.contains(&path) || lower.contains(&path_tail);
    let mentions_target_vigil =
        lower.contains("target/release/vigil") || lower.contains("target/debug/vigil");
    mentions_path || mentions_target_vigil
}

fn assert_builder_target_binary_was_not_run_generated(
    relative: &str,
    commands: &[String],
    source: &str,
    copy_command: &str,
) {
    if !command_uses_builder_stage(copy_command) || !source_is_rust_target_vigil_binary(source) {
        return;
    }
    for command in commands {
        let lower = command.trim_start().to_ascii_lowercase();
        if !lower.starts_with("run ") {
            continue;
        }
        assert!(
            !run_layer_can_modify_path(command, source),
            "{relative} must not copy a builder-stage target-path file that a RUN layer can generate or modify as a wrapper; source {source} from {copy_command} is tainted by {command}"
        );
    }
}

fn assert_start_binary_copy_sources_are_provenance_checked(
    root: &Path,
    relative: &str,
    commands: &[String],
) {
    assert_start_binary_destination_guard_catches_workdir_relative_copy();
    let mut workdir = String::new();
    for command in commands {
        let lower = command.trim_start().to_ascii_lowercase();
        if lower.starts_with("workdir ") {
            let args = shell_tokens(command);
            if let Some(next_workdir) = args.get(1) {
                workdir = resolve_workdir(&workdir, next_workdir);
            }
            continue;
        }
        if !(lower.starts_with("copy ") || lower.starts_with("add ")) {
            continue;
        }
        let paths = copy_or_add_paths(command);
        if paths.len() < 2 {
            continue;
        }
        let destination = paths.last().map(String::as_str).unwrap_or("");
        if !destination_resolves_to_start_binary(destination, &workdir) {
            continue;
        }
        for source in &paths[..paths.len() - 1] {
            if source_is_rust_target_vigil_binary(source) {
                assert_builder_target_binary_was_not_run_generated(
                    relative, commands, source, command,
                );
                continue;
            }
            assert!(
                !command_uses_builder_stage(command),
                "{relative} must not copy a generated builder-stage wrapper into the direct vigil run start binary; builder-stage sources must be Rust target binaries: {command}"
            );
            assert_eq!(
                source, "vigil",
                "{relative} local start-binary copies must use only a Rust target binary or the external staged vigil binary, not {source}: {command}"
            );
            assert!(
                !root.join(source).exists(),
                "{relative} must not copy a repo-local file named vigil into the start binary; that can hide runtime downloads: {command}"
            );
        }
    }
}

fn assert_run_layers_do_not_create_start_binary_wrapper(relative: &str, commands: &[String]) {
    let mut workdir = String::new();
    for command in commands {
        let lower = command.trim_start().to_ascii_lowercase();
        if lower.starts_with("workdir ") {
            let args = shell_tokens(command);
            if let Some(next_workdir) = args.get(1) {
                workdir = resolve_workdir(&workdir, next_workdir);
            }
            continue;
        }
        if !lower.starts_with("run ") {
            continue;
        }
        let body = lower.trim_start_matches("run").trim_start();
        assert!(
            !run_layer_references_start_binary(body, &workdir),
            "{relative} must not create or modify the direct vigil start binary in a RUN layer: {command}"
        );
    }
}

fn collect_existing_files(root: &Path, relative: &Path, files: &mut Vec<PathBuf>) {
    let path = root.join(relative);
    if path.is_file() {
        files.push(path);
        return;
    }
    if !path.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(&path) else {
        return;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry_path.is_file() {
            files.push(entry_path);
        } else if entry_path.is_dir()
            && let Ok(child) = entry_path.strip_prefix(root)
        {
            collect_existing_files(root, child, files);
        }
    }
}

fn assert_no_runtime_fetch_text(label: &str, text: &str) {
    let lower = text.to_ascii_lowercase();
    for needle in [
        "wget ",
        "curl ",
        "pip install",
        "apk add",
        "apt-get",
        "dnf install",
        "yum install",
        "npm install",
        "model download",
        "download model",
        "driver download",
        "download driver",
        "from_pretrained",
        "hf_hub",
    ] {
        assert!(
            !lower.contains(needle),
            "{label} must not fetch acceleration runtime, packages, or models at container start: found `{needle}`"
        );
    }
    assert!(
        !contains_apk_add_invocation(text),
        "{label} must not fetch acceleration runtime, packages, or models at container start: found apk add invocation"
    );
}

fn source_runtime_fetch_violation(path: &Path, line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    let path_text = path.to_string_lossy();
    let supervisor_helper = path_text.ends_with("crates/vigil/src/supervisor.rs");
    let local_http_wake =
        path_text.ends_with("crates/vigil/src/http_data_plane.rs") && lower.contains("wake_addr");
    if lower.contains("https://") {
        return Some("external https URL");
    }
    if lower.contains("http://")
        && !lower.contains("http://supervisor/")
        && !lower.contains("http://example.com/")
    {
        return Some("external http URL");
    }
    if !supervisor_helper && lower.contains("command::new(") {
        for process in [
            "curl", "wget", "sh", "bash", "ash", "busybox", "python", "python3", "node", "perl",
        ] {
            if lower.contains(&format!("\"{process}\"")) || lower.contains(&format!("/{process}\""))
            {
                return Some("ad hoc network-capable process launch");
            }
        }
    }
    if !local_http_wake
        && (lower.contains("tcpstream::connect")
            || lower.contains("udpsocket::connect")
            || lower.contains("tokio::net::tcpstream::connect"))
    {
        return Some("raw outbound network connect");
    }
    if lower.contains("reqwest")
        || lower.contains("ureq")
        || lower.contains("hyper::client")
        || lower.contains("isahc")
        || lower.contains("curl::")
        || lower.contains("from_pretrained")
        || lower.contains("hf_hub")
    {
        return Some("runtime network/model fetch API");
    }
    None
}

fn assert_source_runtime_fetch_guard_catches_external_bootstrap() {
    assert!(
        source_runtime_fetch_violation(
            Path::new("crates/vigil/src/runtime.rs"),
            r#"const BOOTSTRAP_URL: &str = "https://example.invalid/model.bin";"#,
        )
        .is_some(),
        "source scan must reject external bootstrap URLs even when the line omits model/driver wording"
    );
    assert!(
        source_runtime_fetch_violation(
            Path::new("crates/vigil/src/runtime.rs"),
            r#"std::process::Command::new("curl").arg(BOOTSTRAP_URL).status()"#,
        )
        .is_some(),
        "source scan must reject ad hoc curl launches outside the Supervisor helper"
    );
    assert!(
        source_runtime_fetch_violation(
            Path::new("crates/vigil/src/runtime.rs"),
            r#"std::process::Command::new("python3").arg("download_driver.py").status()"#,
        )
        .is_some(),
        "source scan must reject Python launchers that can hide runtime downloads"
    );
    assert!(
        source_runtime_fetch_violation(
            Path::new("crates/vigil/src/runtime.rs"),
            r#"let stream = std::net::TcpStream::connect(("example.invalid", 443))?;"#,
        )
        .is_some(),
        "source scan must reject raw outbound TCP fetch paths"
    );
    assert!(
        source_runtime_fetch_violation(
            Path::new("crates/vigil/src/http_data_plane.rs"),
            r#"let _ = TcpStream::connect_timeout(&wake_addr, Duration::from_millis(100));"#,
        )
        .is_none(),
        "source scan must allow the existing local review data-plane wake connection"
    );
    assert!(
        source_runtime_fetch_violation(
            Path::new("crates/vigil/src/supervisor.rs"),
            r#"let body = supervisor_get("http://supervisor/services/mqtt", &token).ok()?;"#,
        )
        .is_none(),
        "source scan must allow existing local Supervisor API calls"
    );
    assert!(
        source_runtime_fetch_violation(
            Path::new("crates/vigil/src/runtime.rs"),
            r#"let body = supervisor_get("http://supervisor/services/mqtt", &token).ok()?;"#,
        )
        .is_none(),
        "source scan must allow existing local Supervisor API calls routed from runtime code"
    );
    assert!(
        source_runtime_fetch_violation(
            Path::new("crates/hidden_runtime/src/lib.rs"),
            r#"let body = reqwest::blocking::get(BOOTSTRAP_URL)?.bytes()?;"#,
        )
        .is_some(),
        "source scan must reject runtime fetch APIs in sibling production crates, not only crates/vigil/src"
    );
}

fn assert_no_source_model_or_driver_downloads(root: &Path) {
    assert_source_runtime_fetch_guard_catches_external_bootstrap();
    assert_workspace_manifests_do_not_add_runtime_fetch_clients(root);
    let crates_dir = root.join("crates");
    let mut stack = fs::read_dir(&crates_dir)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .map(|entry| entry.path().join("src"))
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    assert!(
        !stack.is_empty(),
        "source runtime-fetch guard must scan production crate src directories"
    );
    while let Some(path) = stack.pop() {
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().and_then(|name| name.to_str()) != Some("target") {
                    stack.push(path);
                }
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for (idx, raw) in text.lines().enumerate() {
                let line = raw.to_ascii_lowercase();
                assert!(
                    source_runtime_fetch_violation(&path, &line).is_none(),
                    "{}:{} must not add runtime network/bootstrap fetches to the local-first binary: {}",
                    path.display(),
                    idx + 1,
                    source_runtime_fetch_violation(&path, &line).unwrap_or("network fetch")
                );
                let model_or_driver =
                    line.contains("model") || line.contains("driver") || line.contains("runtime");
                let network_api = line.contains("http://")
                    || line.contains("https://")
                    || line.contains("download")
                    || line.contains("from_pretrained")
                    || line.contains("hf_hub");
                assert!(
                    !(model_or_driver && network_api),
                    "{}:{} must not add runtime model/driver downloads to the local-first binary",
                    path.display(),
                    idx + 1
                );
            }
        }
    }
}

fn assert_workspace_manifests_do_not_add_runtime_fetch_clients(root: &Path) {
    let mut manifests = vec![root.join("Cargo.toml")];
    let crates_dir = root.join("crates");
    if let Ok(entries) = fs::read_dir(&crates_dir) {
        manifests.extend(
            entries
                .flatten()
                .map(|entry| entry.path().join("Cargo.toml"))
                .filter(|path| path.exists()),
        );
    }
    assert!(
        manifests.len() > 1,
        "workspace manifest guard must include crate dependency manifests"
    );
    for path in manifests {
        let text = fs::read_to_string(&path);
        assert!(
            text.is_ok(),
            "dependency manifest must be readable at {}",
            path.display()
        );
        let Ok(text) = text else {
            continue;
        };
        let lower = text.to_ascii_lowercase();
        for forbidden in [
            "reqwest",
            "ureq",
            "hyper",
            "isahc",
            "curl",
            "hf-hub",
            "hf_hub",
            "huggingface",
            "native-tls",
            "openssl",
        ] {
            assert!(
                !lower.contains(forbidden),
                "{} must not add runtime network/model-fetch dependency `{forbidden}` to the local-first binary",
                path.display()
            );
        }
    }
}

fn addon_profiles_by_arch(profiles: &[RuntimeProfile]) -> BTreeMap<String, &RuntimeProfile> {
    profiles
        .iter()
        .filter(|profile| profile.artifact == "addon" && profile.base == ADDON_BASE)
        .map(|profile| (profile.arch.clone(), profile))
        .collect()
}

fn hardware_profiles_by_artifact_and_arch<'a>(
    profiles: &'a [RuntimeProfile],
    artifact: &str,
) -> BTreeMap<String, &'a RuntimeProfile> {
    profiles
        .iter()
        .filter(|profile| profile.artifact == artifact && profile.hardware_enabled)
        .map(|profile| (profile.arch.clone(), profile))
        .collect()
}

fn docker_instruction_args(command: &str) -> Vec<String> {
    let mut tokens = shell_tokens(command);
    if !tokens.is_empty() {
        tokens.remove(0);
    }
    tokens
}

fn docker_instruction_is_exec_form(command: &str) -> bool {
    let Some((_, rest)) = command.split_once(char::is_whitespace) else {
        return false;
    };
    rest.trim_start().starts_with('[')
}

fn assert_docker_start_instruction_is_exec_form(relative: &str, command: &str) {
    assert!(
        docker_instruction_is_exec_form(command),
        "{relative} start path must use Docker exec-form JSON so Docker does not insert /bin/sh -c: {command}"
    );
}

fn token_is_direct_vigil_binary(token: &str) -> bool {
    token == "/usr/local/bin/vigil"
}

fn assert_start_path_rejects_path_lookup() {
    assert!(
        !token_is_direct_vigil_binary("vigil"),
        "start path must reject bare vigil because PATH can shadow it with a generated wrapper"
    );
    assert!(
        token_is_direct_vigil_binary("/usr/local/bin/vigil"),
        "start path must allow the absolute shipped binary path"
    );
}

fn assert_start_path_execs_vigil_directly(relative: &str, commands: &[String]) {
    assert_start_path_rejects_path_lookup();
    let entrypoint_command = commands.iter().rev().find(|command| {
        command
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("entrypoint")
    });
    let cmd_command = commands
        .iter()
        .rev()
        .find(|command| command.trim_start().to_ascii_lowercase().starts_with("cmd"));
    let start_commands = [entrypoint_command, cmd_command]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert!(
        !start_commands.is_empty(),
        "{relative} must declare an ENTRYPOINT/CMD start path"
    );
    let entrypoint = entrypoint_command.map(|command| docker_instruction_args(command));
    let cmd = cmd_command.map(|command| docker_instruction_args(command));
    if let Some(entrypoint) = entrypoint.as_ref() {
        let entrypoint_command = entrypoint_command.expect("entrypoint command exists");
        assert_docker_start_instruction_is_exec_form(relative, entrypoint_command);
        assert!(
            entrypoint
                .first()
                .is_some_and(|token| token_is_direct_vigil_binary(token.as_str())),
            "{relative} ENTRYPOINT must execute the vigil binary directly as argv[0], not pass vigil run through a launcher: {start_commands:?}"
        );
        let entrypoint_supplies_run = entrypoint.get(1).is_some_and(|token| token == "run");
        let cmd_supplies_run = if cmd
            .as_ref()
            .and_then(|tokens| tokens.first())
            .is_some_and(|token| token == "run")
        {
            let cmd_command = cmd_command.expect("cmd command exists");
            assert_docker_start_instruction_is_exec_form(relative, cmd_command);
            true
        } else {
            false
        };
        assert!(
            entrypoint_supplies_run || cmd_supplies_run,
            "{relative} start path must run the vigil daemon directly with `run`: {start_commands:?}"
        );
    } else {
        let cmd_command = cmd_command.expect("cmd command exists");
        assert_docker_start_instruction_is_exec_form(relative, cmd_command);
        let cmd = cmd.as_ref();
        assert!(
            cmd.and_then(|tokens| tokens.first())
                .is_some_and(|token| token_is_direct_vigil_binary(token.as_str())),
            "{relative} CMD must execute the vigil binary directly as argv[0] when ENTRYPOINT is absent: {start_commands:?}"
        );
        assert!(
            cmd.and_then(|tokens| tokens.get(1))
                .is_some_and(|token| token == "run"),
            "{relative} CMD must run the vigil daemon directly: {start_commands:?}"
        );
    }
    let tokens = start_commands
        .iter()
        .flat_map(|command| shell_tokens(start_command_args_only(command)))
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>();
    for forbidden in ["sh", "bash", "ash", "entrypoint"] {
        let direct_path = format!("/{forbidden}");
        let script_name = format!("{forbidden}.sh");
        let script_path = format!("/{script_name}");
        assert!(
            !tokens.iter().any(|token| token.as_str() == forbidden
                || token == &script_name
                || token.ends_with(&direct_path)
                || token.ends_with(&script_path)),
            "{relative} start path must not hide startup behavior behind a generated shell wrapper: {start_commands:?}"
        );
    }
    assert!(
        !tokens.iter().any(|token| token.ends_with(".sh")),
        "{relative} start path must not execute a shell script that can hide runtime package downloads: {start_commands:?}"
    );
}

/// Drop the leading Docker instruction keyword (`ENTRYPOINT`/`CMD`) from a raw
/// start command before the forbidden-wrapper token scan, so the keyword itself
/// cannot collide with a forbidden token (e.g. the `ENTRYPOINT` keyword reading
/// as the `entrypoint` wrapper token). Only the instruction NAME is stripped;
/// the instruction's ARGUMENTS — where a real `entrypoint.sh` / `/entrypoint` /
/// `sh`/`bash`/`ash` / `*.sh` launcher would appear — are left intact to scan.
fn start_command_args_only(command: &str) -> &str {
    let trimmed = command.trim_start();
    let keyword_end = trimmed
        .find(|ch: char| ch.is_whitespace() || ch == '[')
        .unwrap_or(trimmed.len());
    let keyword = trimmed[..keyword_end].to_ascii_lowercase();
    if keyword == "entrypoint" || keyword == "cmd" {
        &trimmed[keyword_end..]
    } else {
        trimmed
    }
}

fn dockerfile_final_stage_commands(dockerfile: &str) -> Result<Vec<String>, String> {
    let mut final_stage = Vec::new();
    let mut saw_from = false;
    for command in dockerfile_commands(dockerfile)? {
        let lower = command.trim_start().to_ascii_lowercase();
        if lower.starts_with("from ") {
            saw_from = true;
            final_stage.clear();
            continue;
        }
        if !saw_from {
            return Err(format!("Dockerfile command appears before FROM: {command}"));
        }
        final_stage.push(command);
    }
    if !saw_from {
        return Err("Dockerfile has no FROM instruction".to_string());
    }
    Ok(final_stage)
}

fn assert_dockerfile_start_path_has_no_runtime_fetch(root: &Path, relative: &str) {
    let dockerfile_path = root.join(relative);
    let dockerfile = fs::read_to_string(&dockerfile_path);
    assert!(
        dockerfile.is_ok(),
        "Dockerfile must be readable at {}",
        dockerfile_path.display()
    );
    let Ok(dockerfile) = dockerfile else {
        return;
    };
    let commands = dockerfile_commands(&dockerfile);
    assert!(
        commands.is_ok(),
        "Dockerfile commands must parse at {}: {:?}",
        dockerfile_path.display(),
        commands.err()
    );
    let Some(commands) = commands.ok() else {
        return;
    };
    let final_stage_commands = dockerfile_final_stage_commands(&dockerfile);
    assert!(
        final_stage_commands.is_ok(),
        "Dockerfile final stage commands must parse at {}: {:?}",
        dockerfile_path.display(),
        final_stage_commands.err()
    );
    let Some(final_stage_commands) = final_stage_commands.ok() else {
        return;
    };
    assert_start_path_execs_vigil_directly(relative, &final_stage_commands);
    assert_start_binary_copy_sources_are_provenance_checked(root, relative, &commands);
    assert_run_layers_do_not_create_start_binary_wrapper(relative, &commands);
    let start_commands = final_stage_commands
        .iter()
        .filter(|command| {
            let lower = command.trim_start().to_ascii_lowercase();
            lower.starts_with("entrypoint") || lower.starts_with("cmd")
        })
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    assert_no_runtime_fetch_text(relative, &start_commands);

    let copied_sources = copied_runtime_sources(&dockerfile);
    assert!(
        copied_sources.is_ok(),
        "Dockerfile COPY/ADD lines must be parseable at {}: {:?}",
        dockerfile_path.display(),
        copied_sources.err()
    );
    let Some(copied_sources) = copied_sources.ok() else {
        return;
    };
    let mut copied_files = Vec::new();
    for source in copied_sources {
        collect_existing_files(root, &source, &mut copied_files);
    }
    for file in copied_files {
        // A copied source that is not valid UTF-8 is a compiled artifact (the
        // prebuilt static binaries the scratch stages copy), not a script that
        // could hide a runtime fetch; the scan reads text sources only, so the
        // test stays honest on a tree where the release binaries were already
        // built.
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        assert_no_runtime_fetch_text(file.to_string_lossy().as_ref(), &text);
    }
}

#[test]
fn runtime_packages_manifest_exists_and_parses() {
    // Unfakeable because a string blob cannot satisfy a structured non-empty
    // profile parse.
    let Some(profiles) = load_manifest_for_assertions() else {
        return;
    };
    assert!(
        !profiles.is_empty(),
        "runtime package manifest must contain at least one profile"
    );
}

#[test]
fn runtime_packages_manifest_has_amd64_and_aarch64_profiles_with_backends_and_packages() {
    // Unfakeable because each artifact profile has to name its architecture,
    // artifact shape, fixed backend vocabulary, and package list together.
    let Some(profiles) = load_manifest_for_assertions() else {
        return;
    };
    let allowed_decode = BTreeSet::from(["software", "gstreamer"]);
    let allowed_detection = BTreeSet::from([
        "burn-cpu",
        vigil::detection_accel::ACCELERATED_DETECTION_BACKEND,
    ]);
    let arches: BTreeSet<&str> = profiles
        .iter()
        .map(|profile| profile.arch.as_str())
        .collect();
    assert!(
        arches.contains("amd64") && arches.contains("aarch64"),
        "manifest must include amd64 and aarch64 profiles, got {arches:?}"
    );

    let mut saw_static_musl = false;
    for profile in &profiles {
        assert!(
            allowed_decode.contains(profile.decode_backend.as_str()),
            "profile {} uses unsupported decode backend {}",
            profile.name,
            profile.decode_backend
        );
        assert!(
            allowed_detection.contains(profile.detector_backend.as_str()),
            "profile {} uses unsupported detector backend {}",
            profile.name,
            profile.detector_backend
        );
        if profile.hardware_enabled {
            assert!(
                !profile.packages.is_empty(),
                "hardware profile {} must declare runtime packages",
                profile.name
            );
            assert_eq!(
                profile.decode_backend, "gstreamer",
                "hardware-enabled profile {} must select the hardware decode backend, not a software one",
                profile.name
            );
        }
        if profile.decode_backend != "software" || profile.detector_backend != "burn-cpu" {
            assert!(
                profile.artifact == "addon" || profile.artifact == "generic-docker",
                "hardware-capable profile {} must bind to an installable hardware artifact",
                profile.name
            );
        }
        if profile.artifact == "static-musl" {
            saw_static_musl = true;
            assert_eq!(
                profile.decode_backend, "software",
                "static-musl profile must not advertise hardware decode"
            );
            assert_eq!(
                profile.detector_backend, "burn-cpu",
                "static-musl profile must not advertise accelerated detection"
            );
        }
    }
    assert!(
        saw_static_musl,
        "manifest must include the software-only static artifact profile"
    );
    for artifact in ["addon", "generic-docker"] {
        let hardware_profiles = hardware_profiles_by_artifact_and_arch(&profiles, artifact);
        for arch in ["amd64", "aarch64"] {
            assert!(
                hardware_profiles.contains_key(arch),
                "manifest must include a hardware-enabled {artifact} profile for {arch}"
            );
        }
    }
}

#[test]
fn addon_dockerfile_installs_only_manifest_declared_packages() {
    // Unfakeable because the add-on build has to match the manifest package
    // set both ways for every architecture.
    let Some(profiles) = load_manifest_for_assertions() else {
        return;
    };
    let addon_profiles = addon_profiles_by_arch(&profiles);
    for arch in ["amd64", "aarch64"] {
        assert!(
            addon_profiles.contains_key(arch),
            "manifest must include an add-on profile for {arch}"
        );
    }

    let dockerfile_path = repo_root().join(ADDON_DOCKERFILE_PATH);
    let dockerfile = fs::read_to_string(&dockerfile_path);
    assert!(
        dockerfile.is_ok(),
        "add-on Dockerfile must be readable at {}",
        dockerfile_path.display()
    );
    let Ok(dockerfile) = dockerfile else {
        return;
    };
    assert_addon_apk_installs_are_manifest_driven(&dockerfile, &profiles);
    for arch in ["amd64", "aarch64"] {
        assert!(
            addon_profiles
                .get(arch)
                .is_some_and(|profile| !profile.packages.is_empty()),
            "manifest must provide the add-on package set that the Dockerfile expands for {arch}"
        );
    }
}

#[test]
fn runtime_packages_manifest_bakes_packages_with_no_runtime_download() {
    // Unfakeable because a runtime fetch on the container start path defeats
    // local offline startup even if the manifest exists.
    let Some(profiles) = load_manifest_for_assertions() else {
        return;
    };
    let network_needles = ["http://", "https://", "wget ", "curl ", "pip install"];
    for profile in &profiles {
        for package in &profile.packages {
            let lower = package.to_ascii_lowercase();
            for needle in network_needles {
                assert!(
                    !lower.contains(needle),
                    "manifest package entry must be a baked package name, not a runtime fetch: {package}"
                );
            }
        }
    }

    let dockerfile_path = repo_root().join(ADDON_DOCKERFILE_PATH);
    let dockerfile = fs::read_to_string(&dockerfile_path);
    assert!(
        dockerfile.is_ok(),
        "add-on Dockerfile must be readable at {}",
        dockerfile_path.display()
    );
    let Ok(dockerfile) = dockerfile else {
        return;
    };
    assert_addon_apk_installs_are_manifest_driven(&dockerfile, &profiles);
    let root = repo_root();
    assert_dockerfile_start_path_has_no_runtime_fetch(&root, ADDON_DOCKERFILE_PATH);
    assert_dockerfile_start_path_has_no_runtime_fetch(&root, GENERIC_DOCKERFILE_PATH);
    assert_no_source_model_or_driver_downloads(&root);
}
