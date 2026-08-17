//! Top-level Auto-adjusted belongs to an ungoverned value that Vigil's own
//! tuning moved anyway, and this build has none — no tuner writes settings
//! yet. A value inside an on domain that the domain revised reads Managed by
//! that domain, with the revision underneath. This scan is what keeps the
//! second thing from quietly becoming the first: only the settings model
//! that declares the operator-facing control state may name its
//! Auto-adjusted variant at all, so a renderer or a detection path
//! elsewhere cannot start producing an instance of it without this failing.
//!
//! Why this is unfakeable: the crate list, the source walk, and the
//! comment/string lexing all come from the shared `source_scan_lexer`
//! module every other crate-scope guard uses — a scan carrying its own copy
//! of the production-crate list is exactly how one scan later misses a crate
//! its sibling covers. The detector is proven to bite three ways over before
//! its verdict is trusted: once against real production text (the variant's
//! own declaration in `settings_model.rs` must be found), once against a
//! planted synthetic write, where a construction in live code must be found
//! while identical text inside a comment and inside a string literal must
//! not, and once against a planted file that names the identifier without
//! the settings-model type in scope, which must NOT count. A scan that
//! silently matched nothing would fail the canaries rather than passing
//! quietly. Every allowance is required to name a file the walk visited AND
//! to contain at least one real site, so an allowance for a file that no
//! longer writes the state fails here instead of silently widening the scan.
//!
//! ## Two different enums share the spelling `AutoAdjusted`
//!
//! `settings.rs` declares the legacy in-memory settings REGISTRY's own
//! three-state `ControlState` (Automatic / AutoAdjusted / Manual) — a
//! different type from the operator-facing
//! [`vigil::settings_model::ControlState`] this scan governs, and its
//! `SettingCore::apply_automatic` really does write its own `AutoAdjusted`.
//! Matching a bare identifier across both would conflate them: the scan
//! would be reporting on a type it does not govern, and an allowance for
//! `settings.rs` would be quietly excusing a live writer. So the match is
//! SCOPED to the settings-model type — a file counts only if it declares
//! that type or brings it into scope — and the registry writer is handled
//! deliberately, on its own terms, by
//! [`no_production_caller_mints_the_automation_handle_that_reaches_the_registry_auto_adjusted_writer`]:
//! criterion 3's "no instance in this build" holds for the registry state
//! too, because the only path to that writer is an `AutomationHandle`, and
//! no production code mints one.

use std::fs;

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::{
    PRODUCTION_CRATES, collect_cfg_test_ranges, crates_root, in_any_range, is_ident_start, lex,
    read_ident_forward, rust_sources, skip_ws_backward, skip_ws_forward,
};

/// The control-state variant this scan governs. Named once.
const TARGET_VARIANT: &str = "AutoAdjusted";

/// The module owning the operator-facing control state, and the type
/// itself. A file names the state this scan governs only if it declares
/// that type or brings it into scope.
const MODEL_MODULE: &str = "settings_model";
const CONTROL_STATE_TYPE: &str = "ControlState";

/// The file declaring the operator-facing control state. Paths throughout
/// are relative to `crates/`.
const DECLARING_FILE: &str = "vigil/src/settings_model.rs";

/// The only production files that may name the operator-facing
/// Auto-adjusted control state: the settings model that declares it and
/// renders its operator label. Nothing else — no store path produces the
/// state (criterion 3: this build has no instance of it, because no tuner
/// writes settings), and no renderer needs to name it, since every surface
/// renders through `ControlState::label`. An entry here is a claim that the
/// file genuinely names the state, and the walk below enforces that claim.
const ALLOWED_AUTHORING_FILES: &[&str] = &[DECLARING_FILE];

/// The method that mints an `AutomationHandle`, the only capability that
/// reaches the legacy registry's own auto-adjust writer.
const AUTOMATION_MINT_METHOD: &str = "automation";

/// Every identifier in `masked[start..end]`, in source order.
fn identifiers_in(masked: &[char], start: usize, end: usize) -> Vec<String> {
    let mut names = Vec::new();
    let mut position = start;
    while position < end {
        if is_ident_start(masked[position]) {
            let (name, next) = read_ident_forward(masked, position)
                .expect("an identifier-start character reads as an identifier");
            names.push(name);
            position = next.max(position + 1);
        } else {
            position += 1;
        }
    }
    names
}

/// Whether `source` can name the SETTINGS-MODEL control state at all —
/// either it imports the type through the settings-model module (`use
/// crate::settings_model::{..., ControlState, ...}`, alias or not) or it
/// names it inline by path (`settings_model::ControlState::...`). A file
/// doing neither cannot construct that type's variants no matter what it
/// spells, so a bare `AutoAdjusted` in it belongs to some other enum — the
/// legacy registry's, for instance — and is none of this scan's business.
fn brings_the_model_control_state_into_scope(source: &str) -> bool {
    let masked = lex(source).masked;
    let length = masked.len();
    let mut position = 0usize;
    while position < length {
        if !is_ident_start(masked[position]) {
            position += 1;
            continue;
        }
        let (name, end) = read_ident_forward(&masked, position)
            .expect("an identifier-start character reads as an identifier");
        if name == "use" {
            // The whole `use` statement, to its own terminating `;` — brace
            // depth tracked so a nested group (`use a::{b::{c}, d};`) is
            // read whole rather than cut at its first inner delimiter.
            let mut depth = 0i32;
            let mut probe = end;
            while probe < length {
                match masked[probe] {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    ';' if depth <= 0 => break,
                    _ => {}
                }
                probe += 1;
            }
            let names = identifiers_in(&masked, end, probe.min(length));
            if names.iter().any(|word| word == MODEL_MODULE)
                && names.iter().any(|word| word == CONTROL_STATE_TYPE)
            {
                return true;
            }
            position = probe.max(end);
            continue;
        }
        if name == MODEL_MODULE {
            let after = skip_ws_forward(&masked, end);
            if masked.get(after) == Some(&':')
                && masked.get(after + 1) == Some(&':')
                && let Some((following, _)) =
                    read_ident_forward(&masked, skip_ws_forward(&masked, after + 2))
                && following == CONTROL_STATE_TYPE
            {
                return true;
            }
        }
        position = end;
    }
    false
}

/// Every position in `source` where the Auto-adjusted control state is
/// named in LIVE code — comments and string literals masked out first, so a
/// mention in prose or in a message is not mistaken for a write. Matching
/// on the variant identifier itself (rather than on the two-token
/// `ControlState::AutoAdjusted` spelling) is deliberate: importing the
/// variant and then naming it bare is the obvious evasion, and it reaches
/// the same construction.
fn auto_adjusted_sites(source: &str) -> Vec<usize> {
    let masked = lex(source).masked;
    let mut sites = Vec::new();
    let mut position = 0usize;
    while position < masked.len() {
        if is_ident_start(masked[position]) {
            let (name, end) = read_ident_forward(&masked, position)
                .expect("an identifier-start character reads as an identifier");
            if name == TARGET_VARIANT {
                sites.push(position);
            }
            position = end;
        } else {
            position += 1;
        }
    }
    sites
}

/// [`auto_adjusted_sites`], scoped to the operator-facing control state:
/// empty for a file that neither declares the settings-model type nor
/// brings it into scope, since such a file's `AutoAdjusted` is a different
/// enum's variant.
fn model_auto_adjusted_sites(source: &str, is_declaring_file: bool) -> Vec<usize> {
    if !is_declaring_file && !brings_the_model_control_state_into_scope(source) {
        return Vec::new();
    }
    auto_adjusted_sites(source)
}

/// Every position where an `AutomationHandle` is minted in LIVE,
/// NON-TEST code — a `.automation()` call. `#[cfg(test)]` bodies are
/// excluded: the crate's own unit tests exercise the capability
/// deliberately, and a test that could not build one would prove nothing
/// about the production path.
fn automation_mint_sites(source: &str) -> Vec<usize> {
    let masked = lex(source).masked;
    let test_ranges = collect_cfg_test_ranges(source, &masked);
    let mut sites = Vec::new();
    let mut position = 0usize;
    while position < masked.len() {
        if !is_ident_start(masked[position]) {
            position += 1;
            continue;
        }
        let (name, end) = read_ident_forward(&masked, position)
            .expect("an identifier-start character reads as an identifier");
        if name == AUTOMATION_MINT_METHOD {
            let before = skip_ws_backward(&masked, position);
            let after = skip_ws_forward(&masked, end);
            if before > 0
                && masked[before - 1] == '.'
                && masked.get(after) == Some(&'(')
                && !in_any_range(&test_ranges, position)
            {
                sites.push(position);
            }
        }
        position = end;
    }
    sites
}

#[test]
fn no_production_code_outside_the_settings_model_writes_auto_adjusted() {
    let crate_roots = crates_root();

    // Canary one: the detector really bites on real production text. The
    // variant is declared in the settings model, so a scan finding nothing
    // there is a scan that is not reading files or not matching identifiers.
    let model_path = crate_roots.join(DECLARING_FILE);
    let model_source = fs::read_to_string(&model_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", model_path.display()));
    assert!(
        !auto_adjusted_sites(&model_source).is_empty(),
        "the detector must find the control state where it is declared, in {}; finding nothing \
         there means the scan below proves nothing",
        model_path.display()
    );

    // Canary two: a planted write in synthetic source is caught, while the
    // same text inside a comment and inside a string literal is not. This
    // is what rules out a detector that matches nothing at all, and a
    // detector that matches everything.
    let planted = concat!(
        "// a comment naming ControlState::AutoAdjusted must not count\n",
        "fn render(state: &str) -> ControlState {\n",
        "    let _message = \"ControlState::AutoAdjusted\";\n",
        "    ControlState::AutoAdjusted\n",
        "}\n",
    );
    assert_eq!(
        auto_adjusted_sites(planted).len(),
        1,
        "exactly the live construction is caught in the planted source — not the comment, not \
         the string literal"
    );
    let commented_only = concat!(
        "// ControlState::AutoAdjusted\n",
        "fn render() -> ControlState { ControlState::Automatic }\n",
    );
    assert!(
        auto_adjusted_sites(commented_only).is_empty(),
        "a mention in a comment is not a write"
    );

    // Canary three: the scoping bites in both directions. The same planted
    // write counts when the settings-model type is in scope and does not
    // count when the file declares an unrelated enum of its own — which is
    // exactly the `settings.rs` situation, and the reason a bare-identifier
    // match would have conflated two different types.
    let with_the_model_type_in_scope = concat!(
        "use crate::settings_model::{Author, ControlState, Surface};\n",
        "fn render() -> ControlState { ControlState::AutoAdjusted }\n",
    );
    assert_eq!(
        model_auto_adjusted_sites(with_the_model_type_in_scope, false).len(),
        1,
        "a file importing the settings-model control state and naming Auto-adjusted is exactly \
         what this scan governs"
    );
    let another_enum_entirely = concat!(
        "pub enum ControlState { Automatic, AutoAdjusted, Manual }\n",
        "fn revise(state: &mut ControlState) { *state = ControlState::AutoAdjusted; }\n",
    );
    assert!(
        model_auto_adjusted_sites(another_enum_entirely, false).is_empty(),
        "a file declaring its own unrelated control-state enum is not naming the operator-facing \
         one; counting it would report on a type this scan does not govern"
    );
    assert!(
        !auto_adjusted_sites(another_enum_entirely).is_empty(),
        "the unscoped matcher DOES see that text — which is the conflation the scoping removes, \
         not a case the matcher was blind to anyway"
    );

    // The scan itself.
    let mut scanned_files = 0usize;
    let mut seen_allowed: Vec<(String, usize)> = Vec::new();
    let mut offenders: Vec<String> = Vec::new();

    for crate_name in PRODUCTION_CRATES {
        let source_root = crate_roots.join(crate_name).join("src");
        assert!(
            source_root.is_dir(),
            "expected production source tree {}",
            source_root.display()
        );
        let sources = rust_sources(&source_root);
        assert!(
            !sources.is_empty(),
            "expected Rust sources under {}",
            source_root.display()
        );
        for path in sources {
            scanned_files += 1;
            let relative = relative_to(&crate_roots, &path);
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let sites = model_auto_adjusted_sites(&source, relative == DECLARING_FILE);
            if ALLOWED_AUTHORING_FILES.contains(&relative.as_str()) {
                seen_allowed.push((relative, sites.len()));
                continue;
            }
            if !sites.is_empty() {
                offenders.push(format!("{relative} ({} site(s))", sites.len()));
            }
        }
    }

    assert!(
        scanned_files > 20,
        "the walk covered only {scanned_files} files across {PRODUCTION_CRATES:?}; a scan that \
         visits almost nothing cannot report a clean verdict"
    );

    let mut visited_allowed: Vec<String> =
        seen_allowed.iter().map(|(path, _)| path.clone()).collect();
    visited_allowed.sort();
    let mut expected_allowed: Vec<String> = ALLOWED_AUTHORING_FILES
        .iter()
        .map(|path| (*path).to_string())
        .collect();
    expected_allowed.sort();
    assert_eq!(
        visited_allowed, expected_allowed,
        "every allowed-authoring path must name a file the walk actually visited; an allowance \
         for a path that does not exist silently widens this scan"
    );

    // An allowance is a claim that the file genuinely names the state. A
    // stale one — for a file that no longer does — would sit here excusing
    // nothing while quietly staying available to excuse a future write, so
    // it fails now rather than later.
    let stale: Vec<&String> = seen_allowed
        .iter()
        .filter(|(_, sites)| *sites == 0)
        .map(|(path, _)| path)
        .collect();
    assert!(
        stale.is_empty(),
        "an allowed-authoring path that names no site is a stale allowance: {stale:?} no longer \
         names the Auto-adjusted control state, so its entry must be removed rather than left \
         standing"
    );

    assert!(
        offenders.is_empty(),
        "the operator-facing Auto-adjusted control state may be named only by the settings model \
         ({ALLOWED_AUTHORING_FILES:?}); a governed value that a domain revised reads Managed by \
         that domain, never top-level Auto-adjusted. Found: {offenders:?}"
    );
}

/// The deliberate disposition of `settings.rs`, the one production file
/// that writes an `AutoAdjusted` of any kind: it writes the LEGACY REGISTRY's
/// own control state, a different enum from the operator-facing one, and it
/// is unreachable in production — the only path to
/// `SettingCore::apply_automatic` is `AutomationHandle::apply_automatic`,
/// and nothing outside the crate's own unit tests mints an
/// `AutomationHandle`. That is what makes criterion 3's "top-level
/// Auto-adjusted has no instance in this build, because no tuner writes
/// settings" true of the registry too, rather than true only of the type
/// the scan above governs. Widening the allowlist to cover `settings.rs`
/// would have hidden this instead of stating it.
#[test]
fn no_production_caller_mints_the_automation_handle_that_reaches_the_registry_auto_adjusted_writer()
{
    let crate_roots = crates_root();

    // The premise, asserted rather than assumed: `settings.rs` really does
    // name an `AutoAdjusted`, and really does NOT have the settings-model
    // control state in scope — so its sites belong to the other enum.
    let registry_path = crate_roots.join("vigil/src/settings.rs");
    let registry_source = fs::read_to_string(&registry_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", registry_path.display()));
    assert!(
        !auto_adjusted_sites(&registry_source).is_empty(),
        "{} is the file this test exists to dispose of; if it no longer names an Auto-adjusted \
         state at all, this test and the scoping it justifies are stale",
        registry_path.display()
    );
    assert!(
        !brings_the_model_control_state_into_scope(&registry_source),
        "{} must not import the operator-facing control state: if it did, its Auto-adjusted \
         writes would be instances of the very state criterion 3 says this build has none of, \
         and the scan above would have to fail rather than scope them out",
        registry_path.display()
    );

    // The mint detector bites: a live call is found, the same call inside a
    // `#[cfg(test)]` body is not.
    let live_mint = "fn tune(handle: &SettingHandle<u64>) { let _ = handle.automation(); }\n";
    assert_eq!(
        automation_mint_sites(live_mint).len(),
        1,
        "a live .automation() call must be found"
    );
    let test_only_mint = concat!(
        "#[cfg(test)]\n",
        "mod tests {\n",
        "    fn tune(handle: &SettingHandle<u64>) { let _ = handle.automation(); }\n",
        "}\n",
    );
    assert!(
        automation_mint_sites(test_only_mint).is_empty(),
        "a mint inside a #[cfg(test)] body is the crate's own unit test exercising the \
         capability, not a production caller"
    );

    // The verdict, over every production crate.
    let mut minters: Vec<String> = Vec::new();
    for crate_name in PRODUCTION_CRATES {
        let source_root = crate_roots.join(crate_name).join("src");
        for path in rust_sources(&source_root) {
            let relative = relative_to(&crate_roots, &path);
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let sites = automation_mint_sites(&source);
            if !sites.is_empty() {
                minters.push(format!("{relative} ({} site(s))", sites.len()));
            }
        }
    }
    assert!(
        minters.is_empty(),
        "no production code may mint an AutomationHandle: that capability is the only path to \
         the registry's own auto-adjust writer, so while nothing mints one, no tuner writes a \
         setting and the build genuinely has no Auto-adjusted instance. Found: {minters:?}"
    );
}

/// A `crates/`-relative path with forward slashes, so the allowance list
/// reads the same on any platform.
fn relative_to(root: &std::path::Path, path: &std::path::Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}
