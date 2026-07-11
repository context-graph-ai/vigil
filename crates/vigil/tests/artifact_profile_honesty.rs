use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const MANIFEST_PATH: &str = "addons/vigil/runtime-packages.yaml";
const ADDON_DOCKERFILE_PATH: &str = "addons/vigil/Dockerfile";
const GENERIC_DOCKERFILE_PATH: &str = "Dockerfile";
const RELEASE_NOTES_PATH: &str = "docs/release-notes.md";
const VIGIL_CARGO_TOML_PATH: &str = "crates/vigil/Cargo.toml";

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

#[derive(Debug, Clone)]
struct DockerStage {
    base: String,
    name: Option<String>,
    platform: Option<String>,
    commands: Vec<String>,
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

fn strip_package_version(package: &str) -> String {
    package
        .split(['=', '<', '>', '~'])
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// A real, hardware-backed Vulkan ICD (installable-conditional client
/// driver). The software rasterizer (`mesa-vulkan-swrast` / lavapipe) is
/// deliberately excluded: the detection receipt downgrades a software Vulkan
/// adapter to the honest CPU fallback, so shipping only swrast would advertise
/// acceleration a box can never actually get. Names verified resolvable on the
/// real Alpine 3.22 base images (amd64: intel/ati; aarch64: ati/broadcom/
/// panfrost/freedreno — Intel has no aarch64 driver) via `apk add --simulate`.
fn is_hardware_vulkan_icd(package: &str) -> bool {
    matches!(
        package,
        "mesa-vulkan-intel"
            | "mesa-vulkan-ati"
            | "mesa-vulkan-broadcom"
            | "mesa-vulkan-panfrost"
            | "mesa-vulkan-freedreno"
    )
}

/// Every hardware artifact that now ships the accelerated (wgpu → Vulkan)
/// detector must carry the Vulkan runtime: the loader `wgpu` dlopens at
/// startup, plus at least one real GPU ICD so a capable box can accelerate
/// (and fall back to CPU honestly otherwise). amd64 ships BOTH the Intel and
/// AMD ICDs so a single image accelerates on either vendor's iGPU/GPU.
fn assert_profile_ships_vulkan_runtime(profile: &RuntimeProfile) {
    assert!(
        profile.packages.iter().any(|pkg| pkg == "vulkan-loader"),
        "hardware profile {} must ship the Vulkan loader (vulkan-loader) so the wgpu accelerated detector can dlopen libvulkan at runtime",
        profile.name
    );
    assert!(
        profile
            .packages
            .iter()
            .any(|pkg| is_hardware_vulkan_icd(pkg)),
        "hardware profile {} must ship at least one real GPU Vulkan ICD (not just the software rasterizer) so accelerated detection can actually run on a capable box",
        profile.name
    );
    if profile.arch == "amd64" {
        for icd in ["mesa-vulkan-intel", "mesa-vulkan-ati"] {
            assert!(
                profile.packages.iter().any(|pkg| pkg == icd),
                "amd64 hardware profile {} must ship the {icd} Vulkan driver so both an Intel and an AMD box accelerate from the one image",
                profile.name
            );
        }
    }
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

fn parse_apk_packages_from_commands(commands: &[String]) -> Result<BTreeSet<String>, String> {
    let mut packages = BTreeSet::new();

    for command in commands {
        for install in command.split("&&").flat_map(|part| part.split(';')) {
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
            for token in tokens.iter().skip(add_at + 1) {
                if token.starts_with('-') {
                    continue;
                }
                if token.contains('$') || token.contains('`') {
                    return Err(format!(
                        "apk install command must resolve package names before install: {command}"
                    ));
                }
                let package = strip_package_version(token.as_str());
                if !package.is_empty() {
                    packages.insert(package);
                }
            }
        }
    }
    Ok(packages)
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

fn command_assigns_addon_package_variable_from_manifest(
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
        command_assigns_addon_package_variable_from_manifest(
            hardcoded_with_manifest_words,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject variables that hardcode a package list and merely mention the manifest selector words"
    );

    let all_packages_reader = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/packages:/ { in_packages = 1 } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            all_packages_reader,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that ignore the effective BUILD_ARCH profile and print every package"
    );

    let overwritten_profile_gate = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch; in_packages = 1 } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            overwritten_profile_gate,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that mention BUILD_ARCH selection but overwrite it before printing packages"
    );

    let boolean_bypass_reader = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact || 1 } /arch:/ { seen_arch = $2 == arch || 1 } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            boolean_bypass_reader,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that turn the artifact/arch selectors into always-true expressions"
    );

    let reassigned_after_manifest_query = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && PACKAGES="$PACKAGES bash" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            reassigned_after_manifest_query,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must validate the effective package variable assignment immediately before apk add, not the first plausible manifest assignment"
    );

    let truncated_manifest_query = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml | head -n 1)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            truncated_manifest_query,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that truncate or filter the selected profile package output"
    );

    let appended_output_query = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml; cat /tmp/extra-packages)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            appended_output_query,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject command substitutions that append extra package output after the manifest read"
    );

    let temp_manifest_reader = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' /tmp/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
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
        command_assigns_addon_package_variable_from_manifest(
            second_prefix,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must validate the effective assignment before each apk add invocation, including the second apk in one RUN"
    );

    let mutates_selector_state = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } { seen_artifact++ } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            mutates_selector_state,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that mutate selector flags after comparing artifact/arch"
    );

    let overrides_build_arch_selector = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" 'BEGIN { arch = "amd64" } /artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            overrides_build_arch_selector,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that override the BUILD_ARCH selector after wiring it"
    );

    let overrides_build_arch_before_reader = r#"RUN BUILD_ARCH=amd64 && PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            overrides_build_arch_before_reader,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject Dockerfile commands that override Home Assistant BUILD_ARCH before the manifest reader"
    );

    let begin_only_manifest_shape = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" 'BEGIN { seen_artifact = $2 == artifact } BEGIN { seen_arch = $2 == arch } BEGIN { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            begin_only_manifest_shape,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that only mimic selector syntax in BEGIN blocks without scanning manifest lines"
    );

    let exits_after_first_package = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2; exit }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            exits_after_first_package,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that stop after the first selected package"
    );

    let metadata_leaking_query = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            metadata_leaking_query,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that keep printing metadata from later profiles after the selected packages"
    );

    let incomplete_profile_query = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/^- name:/ { in_packages = 0 } /artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages && /^[[:space:]]*-/ && $2 != "gstreamer-vaapi" { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            incomplete_profile_query,
            "PACKAGES",
            &manifest_packages,
        )
        .is_err(),
        "manifest package guard must reject readers that filter the selected profile package set"
    );

    let manifest_query = r#"RUN PACKAGES="$(awk -v artifact=addon -v arch="$BUILD_ARCH" '/^- name:/ { in_packages = 0 } /artifact:/ { seen_artifact = $2 == artifact } /arch:/ { seen_arch = $2 == arch } /packages:/ { in_packages = seen_artifact && seen_arch } in_packages && /^[[:space:]]*-/ { print $2 }' addons/vigil/runtime-packages.yaml)" && apk add --no-cache $PACKAGES"#;
    assert!(
        command_assigns_addon_package_variable_from_manifest(
            manifest_query,
            "PACKAGES",
            &manifest_packages,
        )
        .is_ok(),
        "manifest package guard must allow a variable read from runtime-packages.yaml with artifact, BUILD_ARCH, arch, and packages selectors"
    );
}

fn parse_addon_apk_packages_by_arch_from_commands(
    commands: &[String],
    hardware_profiles: &[&RuntimeProfile],
) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
    assert_package_variable_manifest_guard_rejects_literal_lists();
    let mut packages_by_arch = hardware_profiles
        .iter()
        .map(|profile| (profile.arch.clone(), BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    let manifest_packages = hardware_profiles
        .iter()
        .flat_map(|profile| profile.packages.iter().cloned())
        .collect::<BTreeSet<_>>();
    let profile_packages_by_arch = hardware_profiles
        .iter()
        .map(|profile| {
            (
                profile.arch.clone(),
                profile.packages.iter().cloned().collect::<BTreeSet<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();

    for command in commands {
        let invocations = apk_add_invocations_with_prefix(command)?;
        for (prefix, packages) in invocations {
            for token in packages {
                if token.starts_with('-') {
                    continue;
                }
                if token.contains('`') {
                    return Err(format!(
                        "add-on apk install command must not use backtick package expansion: {command}"
                    ));
                }
                if token.contains('$') {
                    let Some(variable) = package_variable_name(&token) else {
                        return Err(format!(
                            "add-on apk install command has an unparseable package variable {token:?}: {command}"
                        ));
                    };
                    if let Err(reason) = command_assigns_addon_package_variable_from_manifest(
                        &prefix,
                        &variable,
                        &manifest_packages,
                    ) {
                        return Err(format!(
                            "add-on apk install variable ${variable} must be assigned from runtime-packages.yaml using the addon BUILD_ARCH profile immediately before this apk add: {reason}: {command}"
                        ));
                    }
                    for (arch, profile_packages) in &profile_packages_by_arch {
                        packages_by_arch
                            .entry(arch.clone())
                            .or_default()
                            .extend(profile_packages.iter().cloned());
                    }
                    continue;
                }
                let package = strip_package_version(token.as_str());
                if !package.is_empty() {
                    return Err(format!(
                        "add-on apk install command must pass manifest-expanded package variables only; literal package token {package:?} can drift from runtime-packages.yaml: {command}"
                    ));
                }
            }
        }
    }
    Ok(packages_by_arch)
}

fn destination_names_start_binary(destination: &str) -> bool {
    let destination = normalize_docker_path(destination);
    let relative_destination = destination.trim_start_matches("./").trim_start_matches('/');
    relative_destination == "vigil"
        || relative_destination.ends_with("/vigil")
        || relative_destination == "usr/local"
        || destination.ends_with("/usr/local/bin")
        || destination.ends_with("/usr/local/bin/")
}

fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

fn source_is_rust_target_vigil_binary(source: &str) -> bool {
    let name = basename(source).to_ascii_lowercase();
    let normalized = source.replace('\\', "/");
    name == "vigil"
        && (normalized.starts_with("target/") || normalized.contains("/target/"))
        && (normalized.contains("/release/") || normalized.contains("/debug/"))
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

fn copied_stage_name(command: &str) -> Option<String> {
    let tokens = shell_tokens(command);
    for (idx, token) in tokens.iter().enumerate() {
        if let Some(stage) = token.strip_prefix("--from=") {
            return Some(stage.to_string());
        }
        if token == "--from" {
            return tokens.get(idx + 1).cloned();
        }
    }
    None
}

fn copied_start_binary_sources(stage: &DockerStage) -> Vec<(String, Option<String>, String)> {
    let mut sources = Vec::new();
    let mut workdir = String::new();
    for command in &stage.commands {
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
        let stage_name = copied_stage_name(command);
        for source in &paths[..paths.len() - 1] {
            sources.push((source.clone(), stage_name.clone(), command.clone()));
        }
    }
    sources
}

fn cargo_feature_args_include(tokens: &[String], expected: &str) -> bool {
    fn feature_matches(feature: &str, expected: &str) -> bool {
        feature == expected || feature.rsplit('/').next() == Some(expected)
    }

    fn feature_list_contains(value: &str, expected: &str) -> bool {
        value
            .split(',')
            .map(str::trim)
            .any(|feature| feature_matches(feature, expected))
    }

    tokens.iter().enumerate().any(|(idx, token)| {
        if token == "--all-features" {
            return true;
        }
        if let Some(features) = token.strip_prefix("--features=") {
            return feature_list_contains(features, expected);
        }
        if feature_matches(token, expected) {
            return true;
        }
        token == "--features"
            && tokens
                .get(idx + 1)
                .is_some_and(|features| feature_list_contains(features, expected))
    })
}

fn cargo_default_features(text: &str) -> BTreeSet<String> {
    let mut in_features = false;
    let mut collecting_default = false;
    let mut default_value = String::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_features = line == "[features]";
            collecting_default = false;
            continue;
        }
        if !in_features {
            continue;
        }
        if collecting_default {
            default_value.push(' ');
            default_value.push_str(line);
            if line.contains(']') {
                break;
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "default" {
            default_value.push_str(value.trim());
            collecting_default = !value.contains(']');
            if !collecting_default {
                break;
            }
        }
    }
    default_value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|feature| {
            feature
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string()
        })
        .filter(|feature| !feature.is_empty())
        .collect()
}

fn assert_accelerated_detection_is_not_a_default_cargo_feature() {
    let path = repo_root().join(VIGIL_CARGO_TOML_PATH);
    let text = fs::read_to_string(&path);
    assert!(
        text.is_ok(),
        "Vigil Cargo.toml must be readable to prove accelerated detection is not enabled by default: {}",
        path.display()
    );
    let Ok(text) = text else {
        return;
    };
    let defaults = cargo_default_features(&text);
    assert!(
        !defaults.contains("detect-burn-wgpu"),
        "detect-burn-wgpu must stay opt-in: a plain `cargo build`/`docker build .` still produces the software-only static artifact, so accelerated detection must never be a default Cargo feature even though the hardware artifacts now enable it explicitly"
    );
}

fn cargo_feature_args_include_decode_gstreamer(tokens: &[String]) -> bool {
    cargo_feature_args_include(tokens, "decode-gstreamer")
}

fn command_segment_enables_cargo_feature(segment: &str, feature: &str) -> bool {
    let normalized = segment.replace(',', " ");
    let tokens = shell_tokens(&normalized);
    cargo_feature_args_include(&tokens, feature)
}

fn commands_enable_cargo_feature(commands: &[String], feature: &str) -> bool {
    commands
        .iter()
        .flat_map(|command| command.split("&&").flat_map(|part| part.split(';')))
        .any(|segment| command_segment_enables_cargo_feature(segment, feature))
}

fn assert_cargo_feature_parser_covers_comma_forms() {
    for command in [
        "RUN cargo build --features decode-gstreamer,detect-burn-wgpu",
        "RUN cargo build --features=decode-gstreamer,detect-burn-wgpu",
        "RUN cargo build --features vigil/decode-gstreamer,vigil/detect-burn-wgpu",
        "RUN cargo build --features=vigil/decode-gstreamer,vigil/detect-burn-wgpu",
    ] {
        let commands = vec![command.to_string()];
        assert!(
            commands_enable_cargo_feature(&commands, "detect-burn-wgpu"),
            "cargo feature parser must catch comma-listed and package-qualified accelerated-detection features: {command}"
        );
    }
}

fn cargo_command_builds_vigil_hardware_binary(tokens: &[String]) -> bool {
    cargo_command_builds_or_installs_vigil(tokens)
        && cargo_feature_args_include_decode_gstreamer(tokens)
        && cargo_feature_args_include(tokens, "detect-burn-wgpu")
        && !tokens
            .iter()
            .any(|token| token.contains("unknown-linux-musl"))
}

fn cargo_args_target_vigil_binary_or_package(tokens: &[String]) -> bool {
    let mut saw_package = false;
    let mut saw_bin = false;
    for (idx, token) in tokens.iter().enumerate() {
        match token.as_str() {
            "-p" | "--package" => {
                saw_package = true;
                if tokens.get(idx + 1).map(String::as_str) != Some("vigil") {
                    return false;
                }
            }
            "--bin" => {
                saw_bin = true;
                if tokens.get(idx + 1).map(String::as_str) != Some("vigil") {
                    return false;
                }
            }
            _ => {
                if let Some(package) = token
                    .strip_prefix("-p=")
                    .or_else(|| token.strip_prefix("--package="))
                {
                    saw_package = true;
                    if package != "vigil" {
                        return false;
                    }
                }
                if let Some(binary) = token.strip_prefix("--bin=") {
                    saw_bin = true;
                    if binary != "vigil" {
                        return false;
                    }
                }
            }
        }
    }
    saw_package || saw_bin
}

fn assert_cargo_vigil_target_guard_rejects_other_outputs() {
    assert!(
        cargo_command_builds_or_installs_vigil_inner(&shell_tokens(
            "cargo build -p vigil --bin vigil --features decode-gstreamer"
        )),
        "cargo provenance guard must accept an explicit Vigil package/binary build"
    );
    assert!(
        !cargo_command_builds_or_installs_vigil_inner(&shell_tokens(
            "cargo build -p xtask --features decode-gstreamer"
        )),
        "cargo provenance guard must reject decode-gstreamer builds of a non-Vigil package"
    );
    assert!(
        !cargo_command_builds_or_installs_vigil_inner(&shell_tokens(
            "cargo build --bin unrelated --features decode-gstreamer"
        )),
        "cargo provenance guard must reject decode-gstreamer builds of a non-Vigil binary"
    );
    assert!(
        !cargo_command_builds_or_installs_vigil_inner(&shell_tokens(
            "cargo build --features decode-gstreamer"
        )),
        "cargo provenance guard must reject implicit cargo builds because they do not prove the copied vigil binary was rebuilt"
    );
}

fn assert_cargo_shipped_binary_feature_guard_requires_accelerated_detection() {
    assert_accelerated_detection_is_not_a_default_cargo_feature();
    for command in [
        "cargo build -p vigil --bin vigil --features decode-gstreamer,detect-burn-wgpu",
        "cargo build -p vigil --bin vigil --features=decode-gstreamer,detect-burn-wgpu",
        "cargo build -p vigil --bin vigil --features decode-gstreamer --features detect-burn-wgpu",
        "cargo build -p vigil --bin vigil --features decode-gstreamer --all-features",
    ] {
        assert!(
            cargo_command_builds_vigil_hardware_binary(&shell_tokens(command)),
            "cargo provenance guard must accept shipped hardware-artifact Vigil builds that enable accelerated detection, now that the hardware artifacts ship the accelerated detector: {command}"
        );
    }

    assert!(
        !cargo_command_builds_vigil_hardware_binary(&shell_tokens(
            "cargo build -p vigil --bin vigil --features decode-gstreamer"
        )),
        "cargo provenance guard must reject a decode-only Vigil build for a hardware artifact: without detect-burn-wgpu the shipped binary cannot accelerate detection"
    );
}

fn cargo_target_dir(tokens: &[String]) -> Option<&str> {
    tokens.iter().enumerate().find_map(|(idx, token)| {
        if let Some(target_dir) = token.strip_prefix("--target-dir=") {
            return Some(target_dir);
        }
        (token == "--target-dir")
            .then(|| tokens.get(idx + 1).map(String::as_str))
            .flatten()
    })
}

fn cargo_command_builds_or_installs_vigil_inner(tokens: &[String]) -> bool {
    let mut idx = 0;
    if tokens
        .get(idx)
        .is_some_and(|token| token.eq_ignore_ascii_case("RUN"))
    {
        idx += 1;
        while tokens.get(idx).is_some_and(|token| token.starts_with("--")) {
            idx += 1;
        }
    }
    if tokens.get(idx).is_some_and(|token| token == "env") {
        idx += 1;
        while tokens.get(idx).is_some_and(|token| token.contains('=')) {
            idx += 1;
        }
    }
    tokens.get(idx).is_some_and(|token| token == "cargo")
        && tokens
            .get(idx + 1)
            .is_some_and(|subcommand| matches!(subcommand.as_str(), "build" | "install"))
        && cargo_args_target_vigil_binary_or_package(tokens)
}

fn cargo_subcommand(tokens: &[String]) -> Option<&str> {
    let mut idx = 0;
    if tokens
        .get(idx)
        .is_some_and(|token| token.eq_ignore_ascii_case("RUN"))
    {
        idx += 1;
        while tokens.get(idx).is_some_and(|token| token.starts_with("--")) {
            idx += 1;
        }
    }
    if tokens.get(idx).is_some_and(|token| token == "env") {
        idx += 1;
        while tokens.get(idx).is_some_and(|token| token.contains('=')) {
            idx += 1;
        }
    }
    (tokens.get(idx).is_some_and(|token| token == "cargo"))
        .then(|| tokens.get(idx + 1).map(String::as_str))
        .flatten()
}

fn cargo_command_builds_or_installs_vigil(tokens: &[String]) -> bool {
    assert_cargo_vigil_target_guard_rejects_other_outputs();
    cargo_command_builds_or_installs_vigil_inner(tokens)
}

fn cargo_install_root_is_usr_local(tokens: &[String]) -> bool {
    tokens.iter().enumerate().any(|(idx, token)| {
        token == "CARGO_INSTALL_ROOT=/usr/local"
            || token == "--root=/usr/local"
            || (token == "--root" && tokens.get(idx + 1).map(String::as_str) == Some("/usr/local"))
    })
}

fn cargo_command_installs_vigil_to_start_binary(tokens: &[String]) -> bool {
    cargo_subcommand(tokens) == Some("install")
        && cargo_command_builds_vigil_hardware_binary(tokens)
        && cargo_install_root_is_usr_local(tokens)
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

fn docker_path_forms(path: &str, workdir: &str) -> BTreeSet<String> {
    let canonical = canonical_docker_path(path);
    let resolved = resolved_docker_path(path, workdir);
    let mut forms = BTreeSet::new();
    if !canonical.is_empty() {
        forms.insert(canonical);
    }
    if !resolved.is_empty() {
        forms.insert(resolved);
    }
    if forms.is_empty() {
        forms.insert(String::new());
    }
    forms
}

fn destination_can_overwrite_source_path(source: &str, destination: &str, workdir: &str) -> bool {
    let sources = docker_path_forms(source, workdir);
    let destinations = docker_path_forms(destination, workdir);
    sources.iter().any(|source| {
        destinations.iter().any(|destination| {
            destination.is_empty()
                || source == destination
                || source.starts_with(&format!("{destination}/"))
        })
    })
}

fn destination_resolves_to_start_binary(destination: &str, workdir: &str) -> bool {
    if destination_names_start_binary(destination) {
        return true;
    }
    let destination = canonical_docker_path(destination);
    let workdir = normalize_docker_path(workdir);
    destination.is_empty() && (workdir == "/usr/local/bin" || workdir == "/usr/local")
}

fn assert_post_build_copy_guard_catches_root_destination() {
    assert!(
        destination_can_overwrite_source_path("target/release/vigil", ".", ""),
        "post-build COPY . . from the default/root workdir must count as able to overwrite target/release/vigil"
    );
    assert!(
        destination_can_overwrite_source_path("app/target/release/vigil", ".", "/app"),
        "post-build COPY . . from WORKDIR /app must count as able to overwrite /app/target/release/vigil"
    );
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
}

fn cargo_target_dir_matches_source(tokens: &[String], source: &str, workdir: &str) -> bool {
    let target_dir = cargo_target_dir(tokens).unwrap_or("target");
    let target_dir = resolved_docker_path(target_dir, workdir);
    let source = canonical_docker_path(source);
    source == target_dir || source.starts_with(&format!("{target_dir}/"))
}

fn shell_segment_cd_target(tokens: &[String]) -> Option<&str> {
    (tokens.first().map(String::as_str) == Some("cd"))
        .then(|| tokens.get(1).map(String::as_str))
        .flatten()
}

fn assert_default_cargo_target_dir_is_tied_to_workdir() {
    assert!(
        cargo_target_dir_matches_source(
            &shell_tokens("cargo build -p vigil --bin vigil --features decode-gstreamer"),
            "/app/target/release/vigil",
            "/app",
        ),
        "a default cargo build under WORKDIR /app proves /app/target/release/vigil"
    );
    assert!(
        !cargo_target_dir_matches_source(
            &shell_tokens("cargo build -p vigil --bin vigil --features decode-gstreamer"),
            "/tmp/target/release/vigil",
            "/app",
        ),
        "a default cargo build under WORKDIR /app must not prove a copied binary under /tmp/target"
    );
    assert!(
        cargo_target_dir_matches_source(
            &shell_tokens(
                "cargo build -p vigil --bin vigil --features decode-gstreamer --target-dir /tmp/target"
            ),
            "/tmp/target/release/vigil",
            "/app",
        ),
        "--target-dir /tmp/target can prove the copied /tmp/target binary"
    );
    assert!(
        !cargo_target_dir_matches_source(
            &shell_tokens("cargo build -p vigil --bin vigil --features decode-gstreamer"),
            "/app/target/release/vigil",
            "/tmp/hw",
        ),
        "a RUN-local cd /tmp/hw before a default cargo build must not prove /app/target/release/vigil"
    );
}

fn assert_stage_cargo_builds_are_decode_gstreamer(
    stage: &DockerStage,
    source: Option<&str>,
    label: &str,
) {
    assert_post_build_copy_guard_catches_root_destination();
    assert_start_binary_destination_guard_catches_workdir_relative_copy();
    assert_default_cargo_target_dir_is_tied_to_workdir();
    assert_cargo_shipped_binary_feature_guard_requires_accelerated_detection();
    let mut saw_build = false;
    let mut workdir = String::new();
    for command in &stage.commands {
        let lower = command.trim_start().to_ascii_lowercase();
        if lower.starts_with("workdir ") {
            let args = shell_tokens(command);
            if let Some(next_workdir) = args.get(1) {
                workdir = resolve_workdir(&workdir, next_workdir);
            }
            continue;
        }
        if saw_build
            && let Some(source) = source
            && (lower.starts_with("copy ") || lower.starts_with("add "))
        {
            let paths = copy_or_add_paths(command);
            let destination = paths.last().map(String::as_str).unwrap_or("");
            assert!(
                !destination_can_overwrite_source_path(source, destination, &workdir),
                "{label} hardware source stage must not copy/add over the proven target binary path or containing target directory after the decode-gstreamer cargo build; copied source {source} can be replaced by {command}"
            );
        }
        if !lower.starts_with("run ") {
            continue;
        }
        let mut run_workdir = workdir.clone();
        for segment in command.split("&&").flat_map(|part| part.split(';')) {
            let tokens = shell_tokens(segment);
            if tokens.is_empty() {
                continue;
            }
            if let Some(next_workdir) = shell_segment_cd_target(&tokens) {
                run_workdir = resolve_workdir(&run_workdir, next_workdir);
                continue;
            }
            if cargo_command_builds_or_installs_vigil(&tokens) {
                saw_build = true;
                assert!(
                    cargo_command_builds_vigil_hardware_binary(&tokens),
                    "{label} every cargo build/install in the hardware source stage must use --features decode-gstreamer,detect-burn-wgpu so the shipped binary carries hardware decode AND the accelerated detector: {tokens:?}"
                );
                if let Some(source) = source {
                    assert!(
                        cargo_target_dir_matches_source(&tokens, source, &run_workdir),
                        "{label} cargo build target-dir must match copied source {source}; a hardware build in another target dir does not prove this binary: {tokens:?}"
                    );
                }
                continue;
            }
            assert!(
                !saw_build,
                "{label} hardware source stage must not run an opaque command segment after the decode-gstreamer cargo build; it can overwrite the copied target binary without naming the target path: {segment}"
            );
        }
    }
    assert!(
        saw_build,
        "{label} must have a real cargo build/install for the copied Vigil binary"
    );
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

fn assert_stage_does_not_run_overwrite_start_binary(stage: &DockerStage, label: &str) {
    let mut workdir = String::new();
    for command in &stage.commands {
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
        let mut run_workdir = workdir.clone();
        for segment in command.split("&&").flat_map(|part| part.split(';')) {
            let tokens = shell_tokens(segment);
            if let Some(next_workdir) = shell_segment_cd_target(&tokens) {
                run_workdir = resolve_workdir(&run_workdir, next_workdir);
                continue;
            }
            assert!(
                !(cargo_subcommand(&tokens) == Some("install")
                    && cargo_install_root_is_usr_local(&tokens)),
                "{label} must not overwrite the proven /usr/local/bin/vigil start binary with a later cargo install: {segment}"
            );
            assert!(
                !run_layer_references_start_binary(segment, &run_workdir),
                "{label} must not write or modify the proven /usr/local/bin/vigil start binary in a later RUN layer: {segment}"
            );
        }
    }
}

fn assert_stage_does_not_overwrite_start_binary_after_copy(stage: &DockerStage, label: &str) {
    let mut workdir = String::new();
    let mut saw_start_binary_copy = false;
    for command in &stage.commands {
        let lower = command.trim_start().to_ascii_lowercase();
        if lower.starts_with("workdir ") {
            let args = shell_tokens(command);
            if let Some(next_workdir) = args.get(1) {
                workdir = resolve_workdir(&workdir, next_workdir);
            }
            continue;
        }
        if lower.starts_with("copy ") || lower.starts_with("add ") {
            let paths = copy_or_add_paths(command);
            let destination = paths.last().map(String::as_str).unwrap_or("");
            if destination_resolves_to_start_binary(destination, &workdir) {
                saw_start_binary_copy = true;
            }
            continue;
        }
        if saw_start_binary_copy && lower.starts_with("run ") {
            let post_copy_stage = DockerStage {
                base: stage.base.clone(),
                name: stage.name.clone(),
                platform: stage.platform.clone(),
                commands: vec![format!("WORKDIR {workdir}"), command.clone()],
            };
            assert_stage_does_not_run_overwrite_start_binary(&post_copy_stage, label);
        }
    }
}

fn assert_stage_cargo_installs_decode_gstreamer_to_start_binary(stage: &DockerStage, label: &str) {
    assert_cargo_shipped_binary_feature_guard_requires_accelerated_detection();
    let mut saw_install = false;
    for command in &stage.commands {
        let lower = command.trim_start().to_ascii_lowercase();
        if lower.starts_with("workdir ") {
            continue;
        }
        if !(lower.starts_with("run ")) {
            if saw_install {
                assert!(
                    !(lower.starts_with("copy ") || lower.starts_with("add ")),
                    "{label} must not overwrite a cargo-installed /usr/local/bin/vigil after proving the hardware binary: {command}"
                );
            }
            continue;
        }
        for segment in command.split("&&").flat_map(|part| part.split(';')) {
            let tokens = shell_tokens(segment);
            if tokens.is_empty() {
                continue;
            }
            if cargo_command_builds_or_installs_vigil(&tokens) {
                assert!(
                    cargo_command_installs_vigil_to_start_binary(&tokens),
                    "{label} has no COPY/ADD start-binary proof, so the final stage must use cargo install --root /usr/local with decode-gstreamer to produce /usr/local/bin/vigil; a cargo build alone is dead proof: {tokens:?}"
                );
                saw_install = true;
                continue;
            }
            assert!(
                !saw_install,
                "{label} must not run an opaque command segment after cargo installs the hardware start binary: {segment}"
            );
        }
    }
    assert!(
        saw_install,
        "{label} must copy the hardware start binary from a named decode-gstreamer stage or cargo install it to /usr/local/bin/vigil in the final stage"
    );
}

fn assert_stage_uses_decode_gstreamer_binary(
    stages: &[DockerStage],
    stage: &DockerStage,
    label: &str,
) {
    let copied_start_binaries = copied_start_binary_sources(stage);
    if copied_start_binaries.is_empty() {
        assert_stage_cargo_installs_decode_gstreamer_to_start_binary(stage, label);
        return;
    }
    let mut saw_valid_copied_binary = false;
    for (source, stage_name, command) in copied_start_binaries {
        assert!(
            source_is_rust_target_vigil_binary(&source),
            "{label} must copy the start binary from a Rust target vigil path produced by a decode-gstreamer build stage, not {source}: {command}"
        );
        assert!(
            stage_name.is_some(),
            "{label} must copy the hardware start binary from a named decode-gstreamer build stage, not from the local build context: {command}"
        );
        let Some(stage_name) = stage_name else {
            continue;
        };
        let copied_stage = stages
            .iter()
            .find(|candidate| candidate.name.as_deref() == Some(stage_name.as_str()));
        assert!(
            copied_stage.is_some(),
            "{label} copies the start binary from unknown stage {stage_name}"
        );
        let Some(copied_stage) = copied_stage else {
            continue;
        };
        assert_stage_cargo_builds_are_decode_gstreamer(copied_stage, Some(&source), label);
        saw_valid_copied_binary = true;
    }
    if saw_valid_copied_binary {
        assert_stage_does_not_overwrite_start_binary_after_copy(stage, label);
        return;
    }
    let command_text = stage.commands.join("\n");
    let has_decode_feature_binary = false;
    assert!(
        has_decode_feature_binary,
        "{label} must build or copy a Vigil binary built with the decode-gstreamer feature, not a software-only/static binary: {command_text}"
    );
}

fn parse_docker_stages(dockerfile: &str) -> Result<Vec<DockerStage>, String> {
    let mut stages = Vec::new();
    let mut current: Option<DockerStage> = None;

    for command in dockerfile_commands(dockerfile)? {
        let tokens = shell_tokens(&command);
        if tokens
            .first()
            .is_some_and(|instruction| instruction.eq_ignore_ascii_case("FROM"))
        {
            if let Some(stage) = current.take() {
                stages.push(stage);
            }
            let mut idx = 1usize;
            let mut platform = None;
            while idx < tokens.len() && tokens[idx].starts_with("--") {
                if let Some(value) = tokens[idx].strip_prefix("--platform=") {
                    platform = Some(value.to_string());
                    idx += 1;
                    continue;
                }
                if tokens[idx] == "--platform" {
                    platform = tokens.get(idx + 1).cloned();
                    idx += 2;
                    continue;
                }
                idx += 1;
            }
            let base = tokens
                .get(idx)
                .ok_or_else(|| format!("Dockerfile FROM line is missing a base: {command}"))?
                .to_string();
            let name = tokens
                .windows(2)
                .find_map(|pair| pair[0].eq_ignore_ascii_case("AS").then(|| pair[1].clone()));
            current = Some(DockerStage {
                base,
                name,
                platform,
                commands: Vec::new(),
            });
            continue;
        }

        let Some(stage) = current.as_mut() else {
            return Err(format!("Dockerfile command appears before FROM: {command}"));
        };
        stage.commands.push(command);
    }

    if let Some(stage) = current.take() {
        stages.push(stage);
    }
    if stages.is_empty() {
        return Err("Dockerfile has no stages".to_string());
    }
    Ok(stages)
}

fn release_note_identifier(text: &str, key: &str) -> Option<String> {
    for raw in text.lines() {
        let line = raw.trim();
        let line = line.strip_prefix("- ").unwrap_or(line);
        let Some((candidate, value)) = line.split_once(':') else {
            continue;
        };
        if candidate.trim() == key {
            let value = value.trim().trim_matches('`').trim_matches('"');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn release_note_identifiers(text: &str, key: &str) -> BTreeSet<String> {
    release_note_identifier(text, key)
        .into_iter()
        .flat_map(|value| {
            value
                .split([',', ';', ' ', '[', ']'])
                .map(|part| part.trim().trim_matches('`').trim_matches('"').to_string())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn docker_target_arch(arch: &str) -> &str {
    match arch {
        "aarch64" => "arm64",
        other => other,
    }
}

fn assert_stage_does_not_replace_decode_gstreamer_binary(
    stages: &[DockerStage],
    stage: &DockerStage,
    label: &str,
) {
    for (source, stage_name, command) in copied_start_binary_sources(stage) {
        assert!(
            source_is_rust_target_vigil_binary(&source),
            "{label} must not replace the inherited start binary with a non-target wrapper source {source}: {command}"
        );
        assert!(
            stage_name.is_some(),
            "{label} must not replace the inherited hardware start binary from the local build context: {command}"
        );
        let Some(stage_name) = stage_name else {
            continue;
        };
        let copied_stage = stages
            .iter()
            .find(|candidate| candidate.name.as_deref() == Some(stage_name.as_str()));
        assert!(
            copied_stage.is_some(),
            "{label} replaces the start binary from unknown stage {stage_name}"
        );
        let Some(copied_stage) = copied_stage else {
            continue;
        };
        assert_stage_cargo_builds_are_decode_gstreamer(copied_stage, Some(&source), label);
    }
}

fn final_stage_base_candidates(base: &str, profile: &RuntimeProfile) -> BTreeSet<String> {
    let docker_arch = docker_target_arch(profile.arch.as_str());
    let mut candidates = BTreeSet::from([base.to_string()]);
    for (needle, replacement) in [
        ("${TARGETARCH}", docker_arch),
        ("$TARGETARCH", docker_arch),
        ("${BUILD_ARCH}", profile.arch.as_str()),
        ("$BUILD_ARCH", profile.arch.as_str()),
    ] {
        candidates.insert(base.replace(needle, replacement));
    }
    candidates
}

fn final_stage_binds_profile(final_stage: &DockerStage, profile: &RuntimeProfile) -> bool {
    final_stage.name.as_deref() == Some(profile.name.as_str())
        || final_stage_base_candidates(final_stage.base.as_str(), profile)
            .iter()
            .any(|candidate| candidate == &profile.name)
}

fn stage_platform_allows_profile(stage: &DockerStage, profile: &RuntimeProfile) -> bool {
    let Some(platform) = stage.platform.as_deref() else {
        return true;
    };
    let platform = platform.to_ascii_lowercase();
    platform.contains("$targetarch")
        || platform.contains("${targetarch}")
        || platform.contains(docker_target_arch(profile.arch.as_str()))
        || platform.contains(profile.arch.as_str())
}

fn generic_hardware_profiles_by_arch(
    profiles: &[RuntimeProfile],
) -> BTreeMap<String, &RuntimeProfile> {
    profiles
        .iter()
        .filter(|profile| profile.artifact == "generic-docker" && profile.hardware_enabled)
        .map(|profile| (profile.arch.clone(), profile))
        .collect()
}

fn load_generic_docker_stages() -> Option<Vec<DockerStage>> {
    let dockerfile_path = repo_root().join(GENERIC_DOCKERFILE_PATH);
    let dockerfile = fs::read_to_string(&dockerfile_path);
    assert!(
        dockerfile.is_ok(),
        "generic Dockerfile must be readable at {}",
        dockerfile_path.display()
    );
    let Ok(dockerfile) = dockerfile else {
        return None;
    };
    let stages = parse_docker_stages(&dockerfile);
    assert!(
        stages.is_ok(),
        "generic Dockerfile stages must be parseable: {:?}",
        stages.err()
    );
    stages.ok()
}

fn append_stage_commands_with_named_bases(
    stages: &[DockerStage],
    stage: &DockerStage,
    seen: &mut BTreeSet<String>,
    commands: &mut Vec<String>,
) -> Result<(), String> {
    if let Some(name) = stage.name.as_ref()
        && !seen.insert(name.clone())
    {
        return Err(format!(
            "Dockerfile stage inheritance cycle includes {name}"
        ));
    }
    if let Some(base_stage) = stages
        .iter()
        .find(|candidate| candidate.name.as_deref() == Some(stage.base.as_str()))
    {
        append_stage_commands_with_named_bases(stages, base_stage, seen, commands)?;
    }
    commands.extend(stage.commands.iter().cloned());
    Ok(())
}

fn stage_commands_with_named_bases(
    stages: &[DockerStage],
    stage: &DockerStage,
) -> Result<Vec<String>, String> {
    let mut commands = Vec::new();
    let mut seen = BTreeSet::new();
    append_stage_commands_with_named_bases(stages, stage, &mut seen, &mut commands)?;
    Ok(commands)
}

fn label_or_env_instruction_values(command: &str) -> Vec<String> {
    let Some((instruction, rest)) = command.trim_start().split_once(char::is_whitespace) else {
        return Vec::new();
    };
    let instruction = instruction.to_ascii_uppercase();
    if instruction != "LABEL" && instruction != "ENV" {
        return Vec::new();
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return Vec::new();
    }
    if rest.contains('=') {
        rest.split_whitespace()
            .filter_map(|pair| {
                pair.split_once('=')
                    .map(|(_, value)| value.trim_matches('"').trim_matches('\'').to_string())
            })
            .collect()
    } else {
        // Legacy single key/value form: `ENV <KEY> <value...>` (also a valid,
        // if deprecated, LABEL form). The key is the first token; everything
        // after it is the value, spaces and all.
        let Some((_, value)) = rest.split_once(char::is_whitespace) else {
            return Vec::new();
        };
        vec![
            value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string(),
        ]
    }
}

fn value_names_backend_as_token(value: &str, backend_name: &str) -> bool {
    value
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
        .any(|token| token == backend_name)
}

/// True when `backend_name` is recorded as a whitespace/delimiter-bounded
/// token inside the VALUE of a LABEL or ENV instruction among `commands`.
/// Dockerfile comments are already stripped before commands are collected
/// (see `dockerfile_commands`), and any other instruction (RUN, ARG, CMD,
/// ...) is ignored, so an incidental mention of the backend name in a
/// comment or an unrelated instruction does not count as recording it.
fn stage_records_backend_truth(commands: &[String], backend_name: &str) -> bool {
    commands.iter().any(|command| {
        label_or_env_instruction_values(command)
            .iter()
            .any(|value| value_names_backend_as_token(value, backend_name))
    })
}

fn assert_generic_dockerfile_binds_hardware_profile(
    stages: &[DockerStage],
    profile: &RuntimeProfile,
) {
    let target = stages
        .iter()
        .find(|stage| stage.name.as_deref() == Some(profile.name.as_str()));
    assert!(
        target.is_some(),
        "generic Dockerfile must expose the hardware artifact profile {} for {} as a build target",
        profile.name,
        profile.arch
    );
    let Some(target) = target else {
        return;
    };
    assert!(
        stage_platform_allows_profile(target, profile),
        "generic Docker hardware target {} hard-codes a platform that cannot build the {} profile: {:?}",
        profile.name,
        profile.arch,
        target.platform
    );
    let final_stage = stages.last().expect("stage list is non-empty");
    // The hardware profile may be the default final image or a named build
    // target the release pipeline publishes; the release-notes identifier and
    // the full provenance battery on the target stage keep a named target from
    // being dead prose. The default final image staying the software-only
    // static shape keeps a plain `docker build .` working on a checkout that
    // has no sibling source trees. When the default final image DOES derive
    // from the hardware target, the derived stage must not replace the proven
    // binary or add hidden packages — enforced below.
    let final_stage_uses_target = final_stage_binds_profile(final_stage, profile);

    let target_text = target.commands.join("\n");
    assert!(
        target_text.contains(profile.name.as_str()),
        "generic Docker hardware target must record runtime profile {}",
        profile.name
    );
    assert!(
        target_text.contains(profile.arch.as_str())
            || target_text.contains(docker_target_arch(profile.arch.as_str())),
        "generic Docker hardware target {} must record architecture {}",
        profile.name,
        profile.arch
    );
    assert!(
        stage_records_backend_truth(&target.commands, profile.decode_backend.as_str()),
        "generic Docker hardware target must record decode backend {} from profile {}",
        profile.decode_backend,
        profile.name
    );
    assert!(
        stage_records_backend_truth(&target.commands, profile.detector_backend.as_str()),
        "generic Docker hardware target must record detector backend {} from profile {}",
        profile.detector_backend,
        profile.name
    );

    let package_commands = stage_commands_with_named_bases(stages, target);
    assert!(
        package_commands.is_ok(),
        "generic Docker hardware target {} must have an acyclic named local base chain: {:?}",
        profile.name,
        package_commands.err()
    );
    let Some(mut package_commands) = package_commands.ok() else {
        return;
    };
    if final_stage_uses_target && !std::ptr::eq(final_stage, target) {
        package_commands.extend(final_stage.commands.iter().cloned());
    }
    let installed = parse_apk_packages_from_commands(&package_commands);
    assert!(
        installed.is_ok(),
        "generic Docker final hardware image apk install lines must be parseable across target and inherited local stages: {:?}",
        installed.err()
    );
    let Some(installed) = installed.ok() else {
        return;
    };
    let expected: BTreeSet<String> = profile.packages.iter().cloned().collect();
    assert_eq!(
        installed, expected,
        "generic Docker final hardware image packages for {} must exactly match the manifest profile, including inherited local stages",
        profile.name
    );
    // The runtime stage COPYs the binary from a named builder stage; the
    // accelerated-detection feature requirement is enforced against that
    // builder lineage by `assert_stage_uses_decode_gstreamer_binary` below,
    // which now requires BOTH decode-gstreamer AND detect-burn-wgpu.
    assert_profile_ships_vulkan_runtime(profile);
    assert_stage_uses_decode_gstreamer_binary(
        stages,
        target,
        &format!("generic Docker hardware target {}", profile.name),
    );
    if final_stage_uses_target && !std::ptr::eq(final_stage, target) {
        assert!(
            stage_platform_allows_profile(final_stage, profile),
            "generic Docker final image for hardware target {} hard-codes a platform that cannot build the {} profile: {:?}",
            profile.name,
            profile.arch,
            final_stage.platform
        );
        assert_stage_does_not_replace_decode_gstreamer_binary(
            stages,
            final_stage,
            &format!(
                "generic Docker final image derived from hardware target {}",
                profile.name
            ),
        );
        assert_stage_does_not_run_overwrite_start_binary(
            final_stage,
            &format!(
                "generic Docker final image derived from hardware target {}",
                profile.name
            ),
        );
    }
}

#[test]
fn hardware_addon_image_carries_manifest_hardware_profile() {
    // Unfakeable because an empty hardware profile or a Dockerfile that never
    // installs the profile packages cannot satisfy the cross-artifact check.
    assert_cargo_feature_parser_covers_comma_forms();

    let Some(profiles) = load_manifest_for_assertions() else {
        return;
    };
    let hardware_addon_profiles: BTreeMap<&str, &RuntimeProfile> = profiles
        .iter()
        .filter(|profile| profile.artifact == "addon" && profile.hardware_enabled)
        .map(|profile| (profile.arch.as_str(), profile))
        .collect();
    assert!(
        !hardware_addon_profiles.is_empty(),
        "manifest must define at least one hardware-enabled add-on profile"
    );

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
    let stages = parse_docker_stages(&dockerfile);
    assert!(
        stages.is_ok(),
        "add-on Dockerfile stages must be parseable: {:?}",
        stages.err()
    );
    let Some(stages) = stages.ok() else {
        return;
    };
    let final_stage = stages.last().expect("stage list is non-empty");
    let hardware_profile_list = hardware_addon_profiles
        .values()
        .copied()
        .collect::<Vec<_>>();
    let installed_by_arch = parse_addon_apk_packages_by_arch_from_commands(
        &final_stage.commands,
        &hardware_profile_list,
    );
    assert!(
        installed_by_arch.is_ok(),
        "final add-on Dockerfile stage apk install lines must be parseable: {:?}",
        installed_by_arch.err()
    );
    let Some(installed_by_arch) = installed_by_arch.ok() else {
        return;
    };

    for profile in hardware_addon_profiles.values() {
        assert!(
            !profile.packages.is_empty(),
            "hardware add-on profile {} must declare non-empty runtime packages",
            profile.name
        );
        let expected: BTreeSet<String> = profile.packages.iter().cloned().collect();
        let installed = installed_by_arch.get(profile.arch.as_str());
        assert!(
            installed == Some(&expected),
            "add-on Dockerfile must install exactly the runtime packages from hardware profile {} when BUILD_ARCH selects {}",
            profile.name,
            profile.arch
        );
        assert!(
            stage_records_backend_truth(&final_stage.commands, profile.decode_backend.as_str()),
            "final add-on Dockerfile stage must select or document the manifest decode backend {}",
            profile.decode_backend
        );
        assert_eq!(
            profile.detector_backend, "burn-wgpu",
            "add-on hardware profile must now record the accelerated detection backend (burn-wgpu) the shipped add-on binary carries"
        );
        assert!(
            stage_records_backend_truth(&final_stage.commands, profile.detector_backend.as_str()),
            "final add-on Dockerfile stage must record the manifest detector backend {} so the image label matches the shipped binary",
            profile.detector_backend
        );
        assert_profile_ships_vulkan_runtime(profile);
        // Home Assistant Supervisor builds an add-on from the add-on's own
        // folder, so the shipped binary arrives STAGED by the release
        // pipeline (`COPY vigil /usr/local/bin/vigil`) — a build recipe that
        // compiles from source can never run there (the HA-OS harness's
        // add-on guard also rejects any cargo/rustc use in this Dockerfile).
        // For that staged shape the binary's accelerated-detector feature set
        // is proven where it is observable — the owner smoke's feature-gate
        // record and the in-container probe run — while this test pins the
        // manifest packages (now including the Vulkan runtime), the recorded
        // detector-backend truth, and the label above. A final stage that
        // instead builds or copies from a build stage must carry the full
        // provenance battery (which now requires detect-burn-wgpu).
        if !final_stage_copies_external_staged_binary(final_stage) {
            assert_stage_uses_decode_gstreamer_binary(
                &stages,
                final_stage,
                &format!("final add-on Dockerfile stage for {}", profile.name),
            );
        }
    }
}

fn final_stage_copies_external_staged_binary(stage: &DockerStage) -> bool {
    stage.commands.iter().any(|command| {
        let Some((instruction, rest)) = command.trim_start().split_once(char::is_whitespace) else {
            return false;
        };
        if !instruction.eq_ignore_ascii_case("COPY") {
            return false;
        }
        let rest = rest.trim();
        if rest.contains("--from") {
            return false;
        }
        let parts = rest
            .split_whitespace()
            .filter(|part| !part.starts_with("--"))
            .collect::<Vec<_>>();
        parts == ["vigil", "/usr/local/bin/vigil"]
    })
}

#[test]
fn release_notes_name_the_hardware_generic_docker_artifact() {
    // Unfakeable because two distinct structured identifiers are required;
    // two loose words in prose do not pass, and each identifier must map back
    // to a manifest-backed artifact profile.
    assert_cargo_feature_parser_covers_comma_forms();

    let path = repo_root().join(RELEASE_NOTES_PATH);
    assert!(
        path.exists(),
        "tracked release notes must exist at {}",
        path.display()
    );
    let text = fs::read_to_string(&path);
    assert!(
        text.is_ok(),
        "tracked release notes must be readable at {}",
        path.display()
    );
    let Ok(text) = text else {
        return;
    };
    // C10 — the release notes must tell the accelerated-detection truth per
    // artifact: the hardware artifacts now ship the accelerated detector, it
    // falls back to CPU honestly, the HA-OS add-on's VM-measured verdict was
    // the CPU fallback on that test VM while a capable GPU accelerates, and
    // the Vulkan packages cost image size. `normalized` collapses line wraps
    // so multi-line prose still matches.
    let normalized = text
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        normalized.contains("accelerated detection") || normalized.contains("accelerated object detection"),
        "release notes must state the accelerated-detection capability the hardware artifacts now carry"
    );
    assert!(
        normalized.contains("burn-wgpu"),
        "release notes must name the accelerated detection backend (burn-wgpu) the hardware artifacts now build with"
    );
    assert!(
        normalized.contains("fall back to cpu")
            || normalized.contains("falls back to cpu")
            || normalized.contains("fall back to burn-cpu")
            || normalized.contains("cpu fallback"),
        "release notes must state detection falls back to CPU when no usable GPU is present"
    );
    assert!(
        (normalized.contains("add-on") || normalized.contains("home assistant") || normalized.contains("haos"))
            && normalized.contains("test")
            && (normalized.contains("fell back")
                || normalized.contains("fall back")
                || normalized.contains("falls back")
                || normalized.contains("fallback")),
        "release notes must record the add-on's VM-measured verdict: detection classified the CPU fallback on the test VM"
    );
    assert!(
        normalized.contains("capable gpu")
            || normalized.contains("capable gpus")
            || normalized.contains("real gpu")
            || (normalized.contains("gpu") && normalized.contains("accelerat")),
        "release notes must state a capable GPU accelerates detection even though the test VM fell back to CPU"
    );
    assert!(
        normalized.contains("50 mb") || normalized.contains("50mb") || normalized.contains("+50"),
        "release notes must record the Vulkan runtime packages' image size cost (~+50 MB)"
    );
    assert!(
        !normalized.contains("stays on the cpu")
            && !normalized.contains("detection runs on cpu in this artifact")
            && !normalized.contains("cpu (`burn-cpu`) in every shipped shape"),
        "release notes must drop the pre-promotion claim that detection stays on CPU in every shipped shape"
    );
    let hardware_identifiers = release_note_identifiers(&text, "generic_docker_hardware_artifact");
    let software = release_note_identifier(&text, "static_musl_artifact");
    assert!(
        !hardware_identifiers.is_empty(),
        "release notes must name the hardware-capable generic Docker artifact"
    );
    assert!(
        software.is_some(),
        "release notes must name the software-only static artifact"
    );
    let Some(software) = software else {
        return;
    };
    assert!(
        !hardware_identifiers.contains(&software),
        "hardware-capable Docker artifact identifiers and software-only static artifact must be distinct"
    );

    let Some(profiles) = load_manifest_for_assertions() else {
        return;
    };
    let generic_hardware_profiles = generic_hardware_profiles_by_arch(&profiles);
    for arch in ["amd64", "aarch64"] {
        assert!(
            generic_hardware_profiles.contains_key(arch),
            "manifest must include a hardware-enabled generic Docker profile for {arch}"
        );
    }
    for profile in generic_hardware_profiles.values() {
        assert!(
            hardware_identifiers.contains(&profile.name),
            "release notes must name the hardware-capable generic Docker artifact for {}: {}",
            profile.arch,
            profile.name
        );
    }
    for hardware in &hardware_identifiers {
        let hardware_profile = profiles.iter().find(|profile| profile.name == *hardware);
        assert!(
            hardware_profile.is_some(),
            "release-noted generic Docker hardware artifact must match a runtime package profile: {hardware}"
        );
        let Some(hardware_profile) = hardware_profile else {
            continue;
        };
        assert_eq!(
            hardware_profile.artifact, "generic-docker",
            "release-noted hardware artifact must be the generic Docker install shape"
        );
        assert!(
            hardware_profile.hardware_enabled,
            "release-noted generic Docker artifact must be hardware-enabled"
        );
    }
    let Some(stages) = load_generic_docker_stages() else {
        return;
    };
    for profile in generic_hardware_profiles.values() {
        assert_eq!(
            profile.decode_backend, "gstreamer",
            "generic Docker hardware profile {} must carry the hardware decode backend truth",
            profile.name
        );
        assert_eq!(
            profile.detector_backend, "burn-wgpu",
            "generic Docker hardware profile {} must now record the accelerated detection backend (burn-wgpu) it ships",
            profile.name
        );
        assert!(
            !profile.packages.is_empty(),
            "generic Docker hardware profile {} must declare non-empty runtime packages",
            profile.name
        );
        assert_generic_dockerfile_binds_hardware_profile(&stages, profile);
    }

    let software_profile = profiles.iter().find(|profile| profile.name == software);
    assert!(
        software_profile.is_some(),
        "release-noted static artifact must match a runtime package profile: {software}"
    );
    let Some(software_profile) = software_profile else {
        return;
    };
    assert_eq!(
        software_profile.artifact, "static-musl",
        "release-noted software artifact must be the static musl install shape"
    );
    assert!(
        !software_profile.hardware_enabled,
        "release-noted static artifact must not advertise hardware acceleration"
    );
    assert_eq!(
        software_profile.decode_backend, "software",
        "static musl artifact must stay software decode only"
    );
    assert_eq!(
        software_profile.detector_backend, "burn-cpu",
        "static musl artifact must keep CPU detection"
    );
}
