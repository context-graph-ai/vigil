//! Lexical backstop for two of the three structural guarantees the
//! `compile_fail` doctests beside `CameraTrackHubProducer` in
//! `camera_hub.rs` assert: (1) `CameraTrackHubProducer` never gains
//! `Clone` — derived or manually implemented; (2) no method on
//! `CameraTrackHub` (any `fn` inside its own `impl` block that is not the
//! `new` associated constructor — `new` has no `self` receiver at all,
//! which is exactly the carve-out the driving guarantee names: "the only
//! way to obtain a producer is `CameraTrackHub::new`, once, at
//! construction") ever names `CameraTrackHubProducer` in its signature.
//!
//! A `compile_fail` doctest only proves TODAY's source fails to compile
//! for the stated reason; it cannot re-run itself against a FUTURE edit
//! the way a checked-in scan, re-run on every test pass, can. This file
//! is what notices if a later change reopens either gap — the doctests
//! prove the property now, this proves it stays true.
//!
//! Mirrors `automation_handle_capability_scan.rs`'s core mechanism —
//! comment/string-stripped ("masked") source, scanned via the shared
//! `source_scan_lexer.rs` for `impl` blocks naming a watched type — but
//! deliberately NOT its full direct-type-alias detection (the ten
//! syntactic forms, plus the raw-identifier and Unicode-identifier axes,
//! documented in that file's own module doc). That sophistication closed
//! a SPECIFIC, demonstrated bypass class for `AutomationHandle` (a
//! reviewed finding, recorded there). No such alias-bypass has been
//! demonstrated against `CameraTrackHubProducer`/`CameraTrackHub`, and
//! both are types private to this crate's module tree with only two
//! constructors in the entire codebase (`CameraTrackHub::new` and this
//! module's own tests) — so the direct (non-aliased) `impl` forms this
//! file checks cover every realistic path. A determined future
//! alias-rename bypass is a real, NAMED, un-closed gap, not a claimed
//! one — closing it the way `automation_handle_capability_scan.rs` did
//! is future work, not silently assumed already covered here.
//!
//! Both types are crate-local (`CameraTrackHubProducer` is `pub`, but
//! `CameraTrackHubInner` — the type every method actually operates on —
//! is private to this module), so Rust's orphan rule already does most of
//! the work: an inherent `impl CameraTrackHubProducer` or a foreign-trait
//! impl like `impl Clone for CameraTrackHubProducer` can only be written
//! inside the crate that defines the type, which means scanning
//! `crates/vigil/src` is exhaustive for the direct forms below — no other
//! crate in the workspace could add either violation even if it wanted
//! to.

use std::fs;
use std::path::PathBuf;

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::{
    is_ident_start, lex, read_ident_forward, rust_sources, skip_ws_backward, skip_ws_forward,
};

/// The exclusive producer handle this scan watches.
const PRODUCER_TYPE: &str = "CameraTrackHubProducer";
/// The cloneable consumer handle this scan watches for a stray method
/// that would hand a producer back.
const CONSUMER_TYPE: &str = "CameraTrackHub";
/// The one associated function on `CameraTrackHub` allowed to name
/// `CameraTrackHubProducer` in its signature — it has no `self` receiver
/// at all, so it mints nothing FROM an existing consumer; it is the sole
/// constructor the driving guarantee already names.
const ALLOWED_PRODUCER_MINTING_FN: &str = "new";

/// Every identifier in `masked[start..end]`, in source order.
fn idents_in(masked: &[char], start: usize, end: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = start;
    while i < end {
        if is_ident_start(masked[i]) {
            let (ident, next) = read_ident_forward(masked, i).expect("checked ident start");
            out.push(ident);
            i = next.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// Every top-level `impl` block found in `masked`, as `(header_start,
/// header_end, body_open, body_close)` — the header is the text between
/// `impl` and the block's own opening `{` (generics included), so a
/// caller can inspect which type(s) the header names (an inherent impl
/// `impl Foo { .. }` or a trait impl `impl Trait for Foo { .. }` alike)
/// separately from walking the body.
fn all_impl_blocks(masked: &[char]) -> Vec<(usize, usize, usize, usize)> {
    let mut blocks = Vec::new();
    let n = masked.len();
    let mut i = 0usize;
    while i < n {
        if !is_ident_start(masked[i]) {
            i += 1;
            continue;
        }
        let (ident, end) = read_ident_forward(masked, i).expect("checked ident start");
        if ident != "impl" {
            i = end;
            continue;
        }
        let mut header_start = skip_ws_forward(masked, end);
        if header_start < n && masked[header_start] == '<' {
            match source_scan_lexer::match_angle_bracket(masked, header_start) {
                Some(close) => header_start = close + 1,
                None => {
                    i = end;
                    continue;
                }
            }
        }
        let mut depth = 0i32;
        let mut header_end = header_start;
        while header_end < n {
            match masked[header_end] {
                '<' => depth += 1,
                '>' if depth > 0 => depth -= 1,
                '{' if depth <= 0 => break,
                _ => {}
            }
            header_end += 1;
        }
        if header_end >= n {
            break;
        }
        let open = header_end;
        let mut bdepth = 1i32;
        let mut close = open + 1;
        while close < n && bdepth > 0 {
            match masked[close] {
                '{' => bdepth += 1,
                '}' => bdepth -= 1,
                _ => {}
            }
            close += 1;
        }
        blocks.push((header_start, header_end, open, close));
        i = close;
    }
    blocks
}

/// Whether `#[derive(...)]` directly above `struct {type_name}` (or `pub
/// struct {type_name}`) lists `Clone` — walked BACKWARD from the found
/// struct keyword (skipping the optional `pub`), through whitespace, to
/// the attribute's own closing `]`, then back to its matching `[` (simple
/// backward depth counting, mirroring `match_angle_bracket_backward` in
/// the shared lexer for the same reason: the caller has a known CLOSE
/// position and needs to find where the pair OPENED, not the reverse).
fn struct_has_clone_derive(masked: &[char], type_name: &str) -> bool {
    let n = masked.len();
    let mut i = 0usize;
    while i < n {
        if is_ident_start(masked[i]) {
            let (ident, end) = read_ident_forward(masked, i).expect("checked ident start");
            if ident == "struct" {
                let name_pos = skip_ws_forward(masked, end);
                if name_pos < n && is_ident_start(masked[name_pos]) {
                    let (name, _) =
                        read_ident_forward(masked, name_pos).expect("checked ident start");
                    if name == type_name && item_has_clone_derive(masked, i) {
                        return true;
                    }
                }
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    false
}

/// From `item_kw_start` (the position of the `struct`/`fn`/... keyword
/// that begins an item), walk backward past an optional `pub` and
/// whitespace to the item's own attribute list, if any, and report
/// whether a `#[derive(...)]` there names `Clone`.
fn item_has_clone_derive(masked: &[char], item_kw_start: usize) -> bool {
    let mut back = skip_ws_backward(masked, item_kw_start);
    // Skip one optional `pub` immediately before the item keyword.
    if back >= 3 {
        let candidate_start = back.saturating_sub(3);
        if masked[candidate_start..back].iter().collect::<String>() == "pub" {
            back = skip_ws_backward(masked, candidate_start);
        }
    }
    if back == 0 || masked[back - 1] != ']' {
        return false;
    }
    let close = back - 1;
    let mut depth = 1i32;
    let mut k = close;
    loop {
        if k == 0 {
            return false;
        }
        k -= 1;
        match masked[k] {
            ']' => depth += 1,
            '[' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    if k == 0 || masked[k - 1] != '#' {
        return false;
    }
    let attr_idents = idents_in(masked, k + 1, close);
    attr_idents.first().map(String::as_str) == Some("derive")
        && attr_idents.contains(&"Clone".to_string())
}

/// Whether any `impl` header in `masked` names both `Clone` and
/// `PRODUCER_TYPE` — catches a manual `impl Clone for
/// CameraTrackHubProducer { .. }` the way `struct_has_clone_derive`
/// catches the derive form.
fn manual_clone_impl_violations(masked: &[char]) -> Vec<String> {
    let mut violations = Vec::new();
    for &(header_start, header_end, _open, _close) in &all_impl_blocks(masked) {
        let header_idents = idents_in(masked, header_start, header_end);
        if header_idents.iter().any(|w| w == "Clone")
            && header_idents.iter().any(|w| w == PRODUCER_TYPE)
        {
            violations.push(format!(
                "an impl block header names both Clone and {PRODUCER_TYPE} (a manual Clone impl)"
            ));
        }
    }
    violations
}

/// `(fn_name, signature_start, signature_end)` for every `fn` found
/// anywhere in `masked[region_start..region_end]` — `signature_end` is
/// the position of the `fn`'s own top-level `{`/`;`, so the range never
/// includes the function's body. Paren/angle-bracket depth are tracked
/// together (not separately) while scanning forward for that top-level
/// terminator: sufficient for every signature in this crate today (none
/// carry generics or const-generic array bounds), and erring toward
/// treating MORE of a signature as in-scope, never less, keeps this a
/// conservative — over-, not under- — inclusive check.
fn fn_signature_ranges(
    masked: &[char],
    region_start: usize,
    region_end: usize,
) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut i = region_start;
    while i < region_end {
        if is_ident_start(masked[i]) {
            let (ident, end) = read_ident_forward(masked, i).expect("checked ident start");
            if ident == "fn" {
                let name_pos = skip_ws_forward(masked, end);
                if name_pos < region_end && is_ident_start(masked[name_pos]) {
                    let (name, name_end) =
                        read_ident_forward(masked, name_pos).expect("checked ident start");
                    let mut depth = 0i32;
                    let mut j = name_end;
                    while j < region_end {
                        match masked[j] {
                            '(' | '<' => depth += 1,
                            ')' | '>' => depth -= 1,
                            '{' | ';' if depth <= 0 => break,
                            _ => {}
                        }
                        j += 1;
                    }
                    out.push((name, i, j.min(region_end)));
                    i = j.max(name_end);
                    continue;
                }
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// Every non-`new` `fn` inside a `CONSUMER_TYPE` impl block whose
/// signature names `PRODUCER_TYPE` — the "hands a producer back" check.
fn producer_minting_method_violations(masked: &[char]) -> Vec<String> {
    let mut violations = Vec::new();
    for &(header_start, header_end, body_open, body_close) in &all_impl_blocks(masked) {
        let header_idents = idents_in(masked, header_start, header_end);
        if !header_idents.iter().any(|w| w == CONSUMER_TYPE) {
            continue;
        }
        for (name, sig_start, sig_end) in fn_signature_ranges(masked, body_open, body_close) {
            if name == ALLOWED_PRODUCER_MINTING_FN {
                continue;
            }
            if idents_in(masked, sig_start, sig_end)
                .iter()
                .any(|w| w == PRODUCER_TYPE)
            {
                violations.push(format!(
                    "{CONSUMER_TYPE}::{name} names {PRODUCER_TYPE} in its signature — only \
                     {CONSUMER_TYPE}::{ALLOWED_PRODUCER_MINTING_FN} (no self receiver) may"
                ));
            }
        }
    }
    violations
}

/// All violations this file's two checks find in one source string,
/// labeled with `label` (a file path, or a synthetic sample name).
fn camera_hub_capability_violations(label: &str, source: &str) -> Vec<String> {
    let masked = lex(source).masked;
    let mut violations = Vec::new();
    if struct_has_clone_derive(&masked, PRODUCER_TYPE) {
        violations.push(format!(
            "{label}: #[derive(...)] on struct {PRODUCER_TYPE} includes Clone"
        ));
    }
    violations.extend(
        manual_clone_impl_violations(&masked)
            .into_iter()
            .map(|violation| format!("{label}: {violation}")),
    );
    violations.extend(
        producer_minting_method_violations(&masked)
            .into_iter()
            .map(|violation| format!("{label}: {violation}")),
    );
    violations
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn producer_never_gains_clone_and_consumer_never_mints_one() {
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

    let mut found_producer_type = false;
    let mut found_consumer_impl = false;
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
        if source.contains(PRODUCER_TYPE) {
            found_producer_type = true;
        }
        let masked = lex(&source).masked;
        if all_impl_blocks(&masked).iter().any(|&(hs, he, _, _)| {
            idents_in(&masked, hs, he)
                .iter()
                .any(|w| w == CONSUMER_TYPE)
        }) {
            found_consumer_impl = true;
        }
        violations.extend(camera_hub_capability_violations(&label, &source));
    }
    assert!(
        found_producer_type,
        "{PRODUCER_TYPE} was not found anywhere under {}; that smells like the scan silently \
         walked an empty or wrong tree rather than crates/vigil/src, so it would pass green \
         while checking nothing",
        src_root.display()
    );
    assert!(
        found_consumer_impl,
        "no impl block for {CONSUMER_TYPE} was found anywhere under {}; same empty-tree concern \
         as above",
        src_root.display()
    );
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

// ---------------------------------------------------------------------
// Planted-fault proof: the detector functions themselves, exercised
// directly on in-memory samples (no real files touched) — samples that
// must be flagged, and one clean sample (mirroring the real shape,
// `new` included) that must not be.
// ---------------------------------------------------------------------

#[test]
fn detector_flags_a_planted_derive_clone_on_the_producer() {
    let sample = r#"
        #[derive(Debug, Clone)]
        pub struct CameraTrackHubProducer {
            inner: u8,
        }
    "#;
    let violations = camera_hub_capability_violations("sample", sample);
    assert_eq!(violations.len(), 1, "got {violations:?}");
    assert!(violations[0].contains("derive"), "got {violations:?}");
}

#[test]
fn detector_flags_a_planted_manual_clone_impl_on_the_producer() {
    let sample = r#"
        pub struct CameraTrackHubProducer {
            inner: u8,
        }

        impl Clone for CameraTrackHubProducer {
            fn clone(&self) -> Self {
                CameraTrackHubProducer { inner: self.inner }
            }
        }
    "#;
    let violations = camera_hub_capability_violations("sample", sample);
    assert_eq!(violations.len(), 1, "got {violations:?}");
    assert!(
        violations[0].contains("manual Clone impl"),
        "got {violations:?}"
    );
}

#[test]
fn detector_flags_a_planted_method_on_the_consumer_that_returns_a_producer() {
    let sample = r#"
        pub struct CameraTrackHub {
            inner: u8,
        }

        impl CameraTrackHub {
            pub fn new(x: u8) -> (CameraTrackHubProducer, CameraTrackHub) {
                (CameraTrackHubProducer { inner: x }, CameraTrackHub { inner: x })
            }

            pub fn steal_producer(&self) -> CameraTrackHubProducer {
                CameraTrackHubProducer { inner: self.inner }
            }
        }
    "#;
    let violations = camera_hub_capability_violations("sample", sample);
    assert_eq!(
        violations.len(),
        1,
        "exactly one violation expected — steal_producer flagged, new exempted: {violations:?}"
    );
    assert!(
        violations[0].starts_with("sample: CameraTrackHub::steal_producer "),
        "the FLAGGED method must be steal_producer, not new — the allowed constructor must \
         never itself be reported: {violations:?}"
    );
}

#[test]
fn detector_does_not_flag_the_real_shape_new_returning_a_producer_and_ordinary_methods() {
    let sample = r#"
        pub struct CameraTrackHubProducer {
            inner: u8,
        }

        pub struct CameraTrackHub {
            inner: u8,
        }

        impl CameraTrackHub {
            pub fn new(x: u8) -> (CameraTrackHubProducer, CameraTrackHub) {
                (CameraTrackHubProducer { inner: x }, CameraTrackHub { inner: x })
            }

            pub fn active_subscription_count(&self) -> usize {
                self.inner as usize
            }
        }
    "#;
    let violations = camera_hub_capability_violations("sample", sample);
    assert!(
        violations.is_empty(),
        "the real, allowed shape must never be flagged: {violations:?}"
    );
}

#[test]
fn detector_agrees_the_real_producer_struct_carries_no_clone_derive() {
    // A narrower, direct proof that the real source's own derive line for
    // CameraTrackHubProducer specifically is seen and correctly read as
    // NOT including Clone — distinct from the full-file scan above, which
    // could in principle pass vacuously if `struct_has_clone_derive`
    // never actually located the real struct at all.
    let source = fs::read_to_string(crate_root().join("src/camera_hub.rs"))
        .expect("crates/vigil/src/camera_hub.rs must exist and be readable");
    assert!(
        source.contains("pub struct CameraTrackHubProducer"),
        "sanity: the real struct declaration must be present in the file this test reads"
    );
    assert!(!struct_has_clone_derive(
        &lex(&source).masked,
        PRODUCER_TYPE
    ));
}
