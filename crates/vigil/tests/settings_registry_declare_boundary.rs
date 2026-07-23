//! Boundary scan: `SettingsRegistry::declare` may only ever be called from
//! `config::declare_settings` in `config.rs` — the single production
//! function that declares every setting this crate has (see its own doc
//! comment). Visibility alone cannot enforce this half of the guarantee:
//! `SettingsRegistry::new` and `SettingsRegistry::declare` are `pub(crate)`
//! (a compile-time wall against a caller OUTSIDE this crate — see the
//! `compile_fail` doctest on `SettingsRegistry` itself), but nothing in
//! the type system stops a future edit from calling `declare` on a
//! registry from a brand new module INSIDE the crate. This scan is that
//! second layer: it fails the moment `declare` on a settings registry is
//! CALLED anywhere in `crates/vigil/src` other than inside
//! `declare_settings`'s own body, under any spelling of the call
//! DETERMINABLE FROM THE TOKEN STREAM ALONE: a method call
//! (`registry.declare(spec)`); a fully-qualified associated-function
//! (UFCS) call (`SettingsRegistry::declare(&mut registry, spec)`, the
//! same call with the receiver spelled out as an ordinary first argument
//! instead of a dot-receiver); the qualified-path form of the same UFCS
//! call (`<SettingsRegistry>::declare(...)`); a raw identifier
//! (`r#declare`); or a turbofish making the call's own type argument
//! explicit (`registry.declare::<u32>(spec)`, `declare` being itself
//! generic). A newly found spelling meeting that same bar and slipping
//! past this scan is a bug against the criterion, not a form to enumerate
//! ahead of time.
//!
//! `settings.rs`'s own unit tests construct a registry and call `declare`
//! directly, by name, over and over — that is expected and correct, since
//! those tests exercise the registry primitive itself, not a real
//! production setting. They are excluded by SPAN, not by file identity:
//! only a call inside one of `settings.rs`'s own `#[cfg(test)]`-attributed
//! items is exempt (the shared `collect_cfg_test_ranges` scan, also used
//! by `environment_read_surface.rs`, finds those ranges). A future
//! PRODUCTION call added anywhere else in `settings.rs` — outside any
//! `#[cfg(test)]` item — is still flagged; being the file that owns the
//! primitive is not a blanket exemption.
//!
//! Masking of comments and string literals is shared with the sibling
//! scans via `source_scan_lexer.rs`, so a mention of `declare(` in a doc
//! comment (config.rs's own doc comment on `declare_settings` describes
//! this very guarantee) is never mistaken for a live call.

use std::fs;
use std::path::PathBuf;

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::{
    collect_cfg_test_ranges, in_any_range, lex, qualified_path_match_backward, read_ident_backward,
    read_ident_forward, rust_sources, skip_optional_turbofish, skip_ws_backward, skip_ws_forward,
};

/// The one file allowed to call `declare` at all, and the marker that
/// finds its one allowed function.
const DECLARE_HOME_FILE: &str = "config.rs";
const DECLARE_HOME_FUNCTION_MARKER: &str = "fn declare_settings";

/// The file whose OWN `#[cfg(test)]` items are the reviewed exception —
/// narrowed to those spans specifically, not the whole file (see module
/// doc).
const DECLARE_TEST_EXEMPT_FILE: &str = "settings.rs";

/// The type whose associated-function (UFCS) `declare` call form is
/// recognized (`SettingsRegistry::declare(&mut registry, spec)`); a
/// `::declare(` qualified by any OTHER type is left alone.
const REGISTRY_TYPE_NAME: &str = "SettingsRegistry";

/// Whether the `declare` identifier ending at `ident_start` is genuinely
/// CALLED (not merely named) either as a method call
/// (`registry.declare(...)`, any receiver expression) or as a
/// fully-qualified associated-function (UFCS) call whose immediate
/// qualifier is [`REGISTRY_TYPE_NAME`] — bare (`SettingsRegistry::declare(`)
/// or qualified-path spelled (`<SettingsRegistry>::declare(` /
/// `<SettingsRegistry as Trait>::declare(`, see
/// [`qualified_path_match_backward`]). A qualified-path qualifier ends in
/// `>` rather than an identifier character, so it is checked BEFORE falling
/// back to a plain [`read_ident_backward`] read, which would otherwise
/// simply fail to find an identifier there and let the call through
/// unrecognized.
fn is_settings_registry_declare_call(masked: &[char], ident_start: usize) -> bool {
    let pos = skip_ws_backward(masked, ident_start);
    if pos > 0 && masked[pos - 1] == '.' {
        return true;
    }
    if pos >= 2 && masked[pos - 1] == ':' && masked[pos - 2] == ':' {
        let qualifier_end = skip_ws_backward(masked, pos - 2);
        if qualifier_end > 0 && masked[qualifier_end - 1] == '>' {
            return qualified_path_match_backward(masked, qualifier_end - 1, REGISTRY_TYPE_NAME)
                .is_some();
        }
        if let Some((_, qualifier)) = read_ident_backward(masked, qualifier_end) {
            return qualifier == REGISTRY_TYPE_NAME;
        }
    }
    false
}

/// Every position of an identifier `declare` that is genuinely CALLED
/// (followed by an optional turbofish then `(`, and reached via
/// [`is_settings_registry_declare_call`]), found in masked source (so a
/// comment or string mentioning `declare(` is never mistaken for a call).
///
/// `declare` is itself generic, so `registry.declare::<u32>(spec)` is a
/// real, valid call — the turbofish is consumed via the shared
/// [`skip_optional_turbofish`] before requiring `(`, the identical
/// tolerance `process_cleanup_scan.rs` applies at its own call sites,
/// reused here rather than reimplemented: without it, `after` would land
/// on `:`, not `(`, and a turbofish-spelled call outside
/// `declare_settings` would pass this scan uncounted.
fn declare_call_positions(masked: &[char]) -> Vec<usize> {
    let mut positions = Vec::new();
    let n = masked.len();
    let mut i = 0usize;
    while i < n {
        // Delegates to the shared `read_ident_forward` rather than
        // inlining its own identifier-scan loop, so a raw-identifier call
        // (`registry.r#declare(spec)`) is read the same, correct way
        // `is_settings_registry_declare_call`'s own qualifier lookup
        // already is (it calls `read_ident_backward`) — a locally
        // hand-rolled loop here would have stayed blind to that form even
        // after the shared reader was fixed.
        if let Some((ident, end)) = read_ident_forward(masked, i) {
            let start = i;
            if ident == "declare" {
                let after = skip_ws_forward(masked, skip_optional_turbofish(masked, end));
                if after < n
                    && masked[after] == '('
                    && is_settings_registry_declare_call(masked, start)
                {
                    positions.push(start);
                }
            }
            i = end;
            continue;
        }
        i += 1;
    }
    positions
}

/// The brace range of the first function whose signature contains
/// `fn_name_marker` (e.g. `"fn declare_settings"`), or `None` if no such
/// function (with a body) is found.
fn function_body_range(masked: &[char], fn_name_marker: &str) -> Option<(usize, usize)> {
    let marker: Vec<char> = fn_name_marker.chars().collect();
    let n = masked.len();
    let mut i = 0usize;
    while i + marker.len() <= n {
        if masked[i..i + marker.len()] == marker[..] {
            let mut depth = 0i32;
            let mut m = i + marker.len();
            let mut found_open: Option<usize> = None;
            while m < n {
                match masked[m] {
                    '(' | '[' | '<' => depth += 1,
                    ')' | ']' => depth -= 1,
                    '>' => {
                        if depth > 0 {
                            depth -= 1;
                        }
                    }
                    '{' if depth <= 0 => {
                        found_open = Some(m);
                        break;
                    }
                    ';' if depth <= 0 => break,
                    _ => {}
                }
                m += 1;
            }
            let open = found_open?;
            let mut bdepth = 1i32;
            let mut z = open + 1;
            while z < n && bdepth > 0 {
                match masked[z] {
                    '{' => bdepth += 1,
                    '}' => bdepth -= 1,
                    _ => {}
                }
                z += 1;
            }
            return Some((open, z));
        }
        i += 1;
    }
    None
}

/// The pure detector: every `declare` CALL site in `source` that is NOT
/// inside the one allowed home (and, for `settings.rs` specifically, not
/// inside one of its own `#[cfg(test)]` items), described as a
/// human-readable violation. Shared by the real scan and the
/// planted-fault proof below.
fn declare_boundary_violations(file_label: &str, source: &str) -> Vec<String> {
    let masked = lex(source).masked;
    let positions = declare_call_positions(&masked);
    if positions.is_empty() {
        return Vec::new();
    }
    let allowed_range = if file_label == DECLARE_HOME_FILE {
        function_body_range(&masked, DECLARE_HOME_FUNCTION_MARKER)
    } else {
        None
    };
    let test_exempt_ranges = if file_label == DECLARE_TEST_EXEMPT_FILE {
        collect_cfg_test_ranges(&masked)
    } else {
        Vec::new()
    };
    positions
        .into_iter()
        .filter(|pos| {
            let in_home = allowed_range.is_some_and(|(open, close)| open <= *pos && *pos < close);
            let in_reviewed_test_span = in_any_range(&test_exempt_ranges, *pos);
            !in_home && !in_reviewed_test_span
        })
        .map(|pos| {
            format!(
                "{file_label} (character offset {pos}) calls a settings registry's declare \
                 outside config::declare_settings; every setting must be declared in that one \
                 function so the coverage check (vigil::declared_settings) can see it"
            )
        })
        .collect()
}

/// This scan is core-only, deliberately: `declare` is `pub(crate)` on
/// `SettingsRegistry`, so a call from outside `crates/vigil` is a compile
/// error, not a scan finding — there is no sibling crate to widen into.
/// `rust_sources` itself is the same shared walker
/// `environment_read_surface.rs`, `cli_secret_flag_surface.rs`, and
/// `transport_purity.rs` call to cover more than one crate; this scan just
/// points it at one `src/` directory instead of looping over
/// `PRODUCTION_CRATES`.
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn declare_is_called_only_from_the_single_declare_settings_function() {
    let src_root = crate_root().join("src");
    assert!(
        src_root.is_dir(),
        "expected {} to exist; a guard that cannot find the source tree must fail loudly, not \
         silently scan nothing",
        src_root.display()
    );
    let sources = rust_sources(&src_root);
    assert!(
        !sources.is_empty(),
        "found zero .rs files under {}; a guard that cannot find the source tree must fail \
         loudly, not silently scan nothing",
        src_root.display()
    );

    let mut violations = Vec::new();
    for path in sources {
        let label = path
            .strip_prefix(&src_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(source) = fs::read_to_string(&path) else {
            continue;
        };
        violations.extend(declare_boundary_violations(&label, &source));
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

// ---------------------------------------------------------------------
// Planted-fault proofs: the detector function itself, exercised directly
// (no real files touched).
// ---------------------------------------------------------------------

#[test]
fn detector_flags_a_planted_declare_call_outside_declare_settings() {
    let violation_sample = r#"
        fn something_else(registry: &mut SettingsRegistry) {
            registry.declare(some_spec());
        }
    "#;
    let violations = declare_boundary_violations("other_module.rs", violation_sample);
    assert_eq!(
        violations.len(),
        1,
        "a declare( call outside declare_settings must be flagged: {violations:?}"
    );

    let real_shaped_sample = r#"
        pub(crate) fn declare_settings(registry: &mut SettingsRegistry) -> DeclaredSettings {
            DeclaredSettings {
                stationary_interval: registry.declare(stationary_interval_setting_spec()),
            }
        }
    "#;
    let real_shaped_violations = declare_boundary_violations("config.rs", real_shaped_sample);
    assert!(
        real_shaped_violations.is_empty(),
        "the real declare_settings call site must not be flagged: {real_shaped_violations:?}"
    );

    // A SECOND declare( call added to config.rs, outside declare_settings,
    // must still be caught — being in the right file is not enough.
    let sneaky_in_config_sample = r#"
        pub(crate) fn declare_settings(registry: &mut SettingsRegistry) -> DeclaredSettings {
            DeclaredSettings {
                stationary_interval: registry.declare(stationary_interval_setting_spec()),
            }
        }

        fn sneaky_second_declare(registry: &mut SettingsRegistry) {
            registry.declare(some_other_spec());
        }
    "#;
    let sneaky_violations = declare_boundary_violations("config.rs", sneaky_in_config_sample);
    assert_eq!(
        sneaky_violations.len(),
        1,
        "a second declare( call in config.rs, outside declare_settings, must still be flagged: \
         {sneaky_violations:?}"
    );

    // A mention inside a doc comment (config.rs's own doc comment on
    // declare_settings describes this very guarantee) is not a call.
    let comment_only_sample = r#"
        /// Calling `registry.declare(...)` anywhere else is what this scan
        /// prevents.
        pub(crate) fn declare_settings(registry: &mut SettingsRegistry) -> DeclaredSettings {
            DeclaredSettings {
                stationary_interval: registry.declare(stationary_interval_setting_spec()),
            }
        }
    "#;
    let comment_violations = declare_boundary_violations("config.rs", comment_only_sample);
    assert!(
        comment_violations.is_empty(),
        "a doc-comment mention of declare( must not be flagged as a call: {comment_violations:?}"
    );

    // A raw-identifier spelling of the same method call
    // (`registry.r#declare(...)`) must be flagged exactly like
    // `registry.declare(...)` — `r#declare` is not a distinct name, it is
    // Rust's own escape hatch for spelling an ordinary name next to a
    // keyword, and names the identical method (verified: `type Alias =
    // Foo; type r#Alias = Bar;` is rejected as "the name `Alias` is
    // defined multiple times", E0428, on `rustc 1.94.0`). This is
    // self-contained (its own sample, no other `declare` call in scope),
    // so `declare_call_positions` finding it depends entirely on the
    // shared `read_ident_forward` reading `r#declare` as `declare` rather
    // than truncating to the one-character identifier `"r"` the way its
    // own former hand-rolled loop did.
    let raw_identifier_sample = r#"
        fn something_else_raw(registry: &mut SettingsRegistry) {
            registry.r#declare(some_spec());
        }
    "#;
    let raw_identifier_violations =
        declare_boundary_violations("raw_identifier.rs", raw_identifier_sample);
    assert_eq!(
        raw_identifier_violations.len(),
        1,
        "a raw-identifier spelling of the declare( call outside declare_settings must be \
         flagged: {raw_identifier_violations:?}"
    );
}

#[test]
fn detector_flags_a_planted_ufcs_declare_call_bypassing_the_dot_syntax_check() {
    // The exact bypass this closes: a call written as an associated
    // function with the receiver as an ordinary first argument, never as
    // `registry.declare(...)`, so a scan that only recognizes `.declare(`
    // walks straight past it.
    let ufcs_violation_sample = r#"
        fn something_else(registry: &mut SettingsRegistry) {
            SettingsRegistry::declare(registry, some_spec());
        }
    "#;
    let violations = declare_boundary_violations("other_module.rs", ufcs_violation_sample);
    assert_eq!(
        violations.len(),
        1,
        "a `SettingsRegistry::declare(...)` UFCS call outside declare_settings must be flagged \
         exactly like the dot-call form: {violations:?}"
    );

    // The real call site rewritten in UFCS form must still be recognized
    // as the ALLOWED one when it genuinely sits inside declare_settings.
    let ufcs_real_shaped_sample = r#"
        pub(crate) fn declare_settings(registry: &mut SettingsRegistry) -> DeclaredSettings {
            DeclaredSettings {
                stationary_interval: SettingsRegistry::declare(
                    registry,
                    stationary_interval_setting_spec(),
                ),
            }
        }
    "#;
    let ufcs_real_shaped_violations =
        declare_boundary_violations("config.rs", ufcs_real_shaped_sample);
    assert!(
        ufcs_real_shaped_violations.is_empty(),
        "a UFCS declare call genuinely inside declare_settings must not be flagged: \
         {ufcs_real_shaped_violations:?}"
    );

    // A `::declare(` qualified by some OTHER type must be left alone —
    // this scan is specifically about the settings registry, not every
    // associated function named `declare` anywhere in the crate.
    let unrelated_declare_sample = r#"
        fn unrelated() {
            SomeOtherRegistry::declare(&mut other, spec());
        }
    "#;
    let unrelated_violations =
        declare_boundary_violations("other_module.rs", unrelated_declare_sample);
    assert!(
        unrelated_violations.is_empty(),
        "a `::declare(` call qualified by an unrelated type must not be flagged: \
         {unrelated_violations:?}"
    );
}

#[test]
fn detector_flags_a_planted_qualified_path_declare_call_bypassing_the_ufcs_qualifier_read() {
    // The exact bypass this closes: `<SettingsRegistry>::declare(...)` is a
    // valid, real Rust qualified-path spelling of the same UFCS call
    // (confirmed by compiling it under the pinned toolchain) — the
    // qualifier ends in `>`, not an identifier character, so a plain
    // backward identifier read at the qualifier position failed and the
    // call was never counted as a declare call at all.
    let qualified_path_violation_sample = r#"
        fn something_else(registry: &mut SettingsRegistry) {
            <SettingsRegistry>::declare(registry, some_spec());
        }
    "#;
    let violations =
        declare_boundary_violations("other_module.rs", qualified_path_violation_sample);
    assert_eq!(
        violations.len(),
        1,
        "a `<SettingsRegistry>::declare(...)` qualified-path call outside declare_settings must \
         be flagged exactly like the bare UFCS form: {violations:?}"
    );

    // The `<Type as Trait>::declare(` spelling is covered by the same read
    // incidentally (reading the last path segment before ` as Trait` is the
    // same operation either way); confirm it is not silently exempted.
    let as_trait_violation_sample = r#"
        fn something_else(registry: &mut SettingsRegistry) {
            <SettingsRegistry as DeclareExt>::declare(registry, some_spec());
        }
    "#;
    let as_trait_violations =
        declare_boundary_violations("other_module.rs", as_trait_violation_sample);
    assert_eq!(
        as_trait_violations.len(),
        1,
        "a `<SettingsRegistry as Trait>::declare(...)` call outside declare_settings must be \
         flagged: {as_trait_violations:?}"
    );

    // The real call site rewritten in qualified-path form must still be
    // recognized as the ALLOWED one when it genuinely sits inside
    // declare_settings.
    let qualified_path_real_shaped_sample = r#"
        pub(crate) fn declare_settings(registry: &mut SettingsRegistry) -> DeclaredSettings {
            DeclaredSettings {
                stationary_interval: <SettingsRegistry>::declare(
                    registry,
                    stationary_interval_setting_spec(),
                ),
            }
        }
    "#;
    let qualified_path_real_shaped_violations =
        declare_boundary_violations("config.rs", qualified_path_real_shaped_sample);
    assert!(
        qualified_path_real_shaped_violations.is_empty(),
        "a qualified-path declare call genuinely inside declare_settings must not be flagged: \
         {qualified_path_real_shaped_violations:?}"
    );

    // A qualified-path `::declare(` qualified by some OTHER type must be
    // left alone, same as the bare UFCS form.
    let unrelated_qualified_path_sample = r#"
        fn unrelated() {
            <SomeOtherRegistry>::declare(&mut other, spec());
        }
    "#;
    let unrelated_violations =
        declare_boundary_violations("other_module.rs", unrelated_qualified_path_sample);
    assert!(
        unrelated_violations.is_empty(),
        "a qualified-path `::declare(` call qualified by an unrelated type must not be flagged: \
         {unrelated_violations:?}"
    );
}

#[test]
fn detector_flags_a_planted_turbofish_declare_call_bypassing_the_bare_paren_read() {
    // The exact bypass this closes: `declare` is itself generic, so
    // `registry.declare::<u32>(spec)` is a real, valid call — confirmed to
    // compile clean under `-D warnings` on the pinned toolchain rather
    // than assumed. `declare_call_positions` required `(` immediately
    // (after whitespace) following the `declare` identifier, so the `:`
    // that opens a turbofish landed there instead and the call was never
    // counted.
    let turbofish_violation_sample = r#"
        fn something_else(registry: &mut SettingsRegistry) {
            registry.declare::<u32>(some_spec());
        }
    "#;
    let violations = declare_boundary_violations("other_module.rs", turbofish_violation_sample);
    assert_eq!(
        violations.len(),
        1,
        "a `registry.declare::<u32>(...)` turbofish call outside declare_settings must be \
         flagged exactly like the plain dot-call form: {violations:?}"
    );

    // The UFCS turbofish spelling must be caught the same way.
    let ufcs_turbofish_violation_sample = r#"
        fn something_else(registry: &mut SettingsRegistry) {
            SettingsRegistry::declare::<u32>(registry, some_spec());
        }
    "#;
    let ufcs_turbofish_violations =
        declare_boundary_violations("other_module.rs", ufcs_turbofish_violation_sample);
    assert_eq!(
        ufcs_turbofish_violations.len(),
        1,
        "a `SettingsRegistry::declare::<u32>(...)` turbofish UFCS call outside \
         declare_settings must be flagged: {ufcs_turbofish_violations:?}"
    );

    // The real call site rewritten with a turbofish must still be
    // recognized as the ALLOWED one when it genuinely sits inside
    // declare_settings.
    let turbofish_real_shaped_sample = r#"
        pub(crate) fn declare_settings(registry: &mut SettingsRegistry) -> DeclaredSettings {
            DeclaredSettings {
                stationary_interval: registry.declare::<u32>(
                    stationary_interval_setting_spec(),
                ),
            }
        }
    "#;
    let turbofish_real_shaped_violations =
        declare_boundary_violations("config.rs", turbofish_real_shaped_sample);
    assert!(
        turbofish_real_shaped_violations.is_empty(),
        "a turbofish declare call genuinely inside declare_settings must not be flagged: \
         {turbofish_real_shaped_violations:?}"
    );
}

#[test]
fn settings_rs_own_registry_tests_are_the_reviewed_exception_not_the_whole_file() {
    let test_shaped_sample = r#"
        #[cfg(test)]
        mod tests {
            #[test]
            fn some_registry_test() {
                let mut registry = SettingsRegistry::new();
                let handle = registry.declare(example_spec());
                let _ = handle;
            }
        }
    "#;
    let violations = declare_boundary_violations("settings.rs", test_shaped_sample);
    assert!(
        violations.is_empty(),
        "settings.rs's own registry unit tests, inside a #[cfg(test)] item, are the reviewed \
         exception: {violations:?}"
    );

    // The narrower exemption's whole point: a PRODUCTION call added to
    // settings.rs OUTSIDE any #[cfg(test)] item is still flagged — being
    // the file that owns the primitive is not a blanket pass.
    let production_shaped_sample = r#"
        fn sneaky_production_helper(registry: &mut SettingsRegistry) {
            registry.declare(some_spec());
        }

        #[cfg(test)]
        mod tests {
            #[test]
            fn some_registry_test() {
                let mut registry = SettingsRegistry::new();
                let handle = registry.declare(example_spec());
                let _ = handle;
            }
        }
    "#;
    let production_violations =
        declare_boundary_violations("settings.rs", production_shaped_sample);
    assert_eq!(
        production_violations.len(),
        1,
        "a production declare( call added to settings.rs OUTSIDE any #[cfg(test)] item must \
         still be flagged, even though the file also owns the primitive: \
         {production_violations:?}"
    );
}
