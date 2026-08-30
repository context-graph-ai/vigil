//! Environment-read inventory for every production crate's `src/`:
//! `crates/vigil/src`, `crates/vigil-ha/src`, and `crates/vigil-bin/src`.
//!
//! The product rule this guard enforces: an adjustable value must be a
//! declared setting with a visible outside control (add-on option, CLI
//! flag, config file field), not an ad-hoc environment-variable read
//! scattered through the source. Today's reads are grandfathered into a
//! frozen baseline (checked in beside this test); this guard's job is to
//! stop a NEW read site from appearing anywhere else, and to make sure the
//! baseline itself can only shrink as reads are migrated to declared
//! settings, never grow to make room for a new one.
//!
//! `vigil-ha` and `vigil-bin` hold zero environment reads today — the
//! adapter and the composition root have no reason to read the process
//! environment directly, so their share of the baseline starts empty and
//! stays empty until a reviewed addition earns an entry. Scanning them from
//! day one closes the blind spot before a wrapper read can appear in the
//! crate holding the MQTT adapter, which is exactly where a
//! `crates/vigil/src`-only scan could never see it.
//!
//! Each site is identified by (source file, enclosing function or item,
//! environment variable name, occurrence) — deliberately NOT a line
//! number, which rots on the very next unrelated edit to the file. The
//! occurrence is the 1-based count of that exact (file, function,
//! variable) triple in file-position order, so a SECOND read of an
//! already-baselined variable inside the same function is its own new
//! site rather than folding invisibly into the first. When the variable
//! name is computed rather than a literal (e.g. a small helper that takes
//! the name as a parameter), that is recorded honestly as `<dynamic>`
//! rather than guessed at.
//!
//! A read hidden behind a small in-crate wrapper is still inventoried: the
//! detector first finds every such wrapper by looking for a function whose
//! own body reads the environment using one of its own parameters (today
//! that set happens to be `runtime::env_u64`, `runtime::env_is`,
//! `privilege::env_u32`, `doctor::env_var`, and `config::env_intent_bool`,
//! but nothing below names them — the set is derived fresh from the source
//! every run), and then scans the whole tree again for calls to that
//! wrapper. A call passing a string literal becomes an identity naming
//! that literal, exactly like a direct `std::env::var` call; a call that
//! genuinely computes the name is recorded as `<dynamic>`, same as today.
//! Wrapper detection deliberately excludes anything defined inside a
//! `#[cfg(test)]` item: test-only scaffolding (for example a fixture that
//! both reads and rewrites the process environment to fence one test from
//! the next) is not a production adjustable-value surface, and its own
//! name (`set`, `remove`, ...) is usually far too generic a word to safely
//! scan call sites for.
//!
//! The `use`-statement resolver follows direct imports, aliased imports
//! (`use std::env::var as fetch;`), the `self` form
//! (`use std::env::{self, var};`), and a one-level glob (`use
//! std::env::*;`, which binds all four `std::env` read functions as bare
//! names, and `use std::*;`, which binds `env` itself as a module alias).
//! It does NOT follow re-export chains, a glob more than one level away
//! from `std::env`, or indirection through an alias of an alias — none of
//! that is resolvable by a lexer without a real name-resolution pass, and
//! this guard does not pretend otherwise.
//!
//! The scanner is a pure function over source text (`environment_read_sites`)
//! shared by the real scan and its own planted-fault proofs below, so the
//! two cannot drift apart. Comment/string-literal lexing (and the tiny
//! identifier/whitespace primitives built on it) lives in
//! `source_scan_lexer.rs`, shared with `cli_secret_flag_surface.rs` and
//! `settings_registry_declare_boundary.rs` so these guards cannot silently
//! drift apart on that shared concern either.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::{
    PRODUCTION_CRATES, collect_cfg_test_ranges, crates_root, in_any_range, is_ident_char,
    is_ident_start, lex, match_delim, match_paren, read_ident_backward, read_ident_forward,
    rust_sources, skip_ws_backward, skip_ws_forward,
};

const DYNAMIC_VARIABLE: &str = "<dynamic>";
const ALL_VARIABLES: &str = "<all>";
const MODULE_LEVEL: &str = "<module level>";

/// One `std::env::{var, var_os, vars, vars_os}` read site. `occurrence` is
/// the 1-based count of this exact (file, function, variable) triple seen
/// so far, in file-position order: two separate reads of the SAME variable
/// in the SAME function are two different sites, not one — folding them
/// into a single `(file, function, variable)` identity would let a second
/// read of an already-baselined variable slip in for free, which
/// contradicts the guard's own promise that a new site fails.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EnvReadSite {
    file: String,
    function: String,
    variable: String,
    occurrence: usize,
}

fn is_word_at(chars: &[char], i: usize, word: &str) -> bool {
    let wc: Vec<char> = word.chars().collect();
    if i + wc.len() > chars.len() || chars[i..i + wc.len()] != wc[..] {
        return false;
    }
    let before_ok = i == 0 || !is_ident_char(chars[i - 1]);
    let after_idx = i + wc.len();
    let after_ok = after_idx >= chars.len() || !is_ident_char(chars[after_idx]);
    before_ok && after_ok
}

/// A position that is exactly the start of a recorded string literal is
/// where "skip whitespace" must stop, even though a masked string body
/// reads as blank space; otherwise a call's string argument would be
/// skipped right over as if it were padding.
fn skip_trivial_forward(
    masked: &[char],
    strings_by_start: &HashMap<usize, &(usize, usize, String)>,
    mut pos: usize,
) -> usize {
    loop {
        if strings_by_start.contains_key(&pos) {
            return pos;
        }
        if pos < masked.len() && masked[pos].is_whitespace() {
            pos += 1;
            continue;
        }
        return pos;
    }
}

// ---------------------------------------------------------------------
// `use` statement resolution: local names bound to the `std::env` module,
// and local bare-callable names bound directly to one of its four
// environment-read functions.
// ---------------------------------------------------------------------

#[derive(Default)]
struct UseBindings {
    module_aliases: BTreeSet<String>,
    fn_aliases: BTreeMap<String, String>,
}

fn collect_use_bindings(masked: &[char]) -> UseBindings {
    let mut bindings = UseBindings::default();
    let n = masked.len();
    let mut i = 0usize;
    while i < n {
        if is_word_at(masked, i, "use") {
            let stmt_start = i + 3;
            let mut depth = 0i32;
            let mut j = stmt_start;
            while j < n {
                match masked[j] {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    ';' if depth <= 0 => break,
                    _ => {}
                }
                j += 1;
            }
            let body: String = masked[stmt_start..j.min(n)].iter().collect();
            apply_use_body(&body, &mut bindings);
            i = j + 1;
            continue;
        }
        i += 1;
    }
    bindings
}

fn apply_use_body(body: &str, bindings: &mut UseBindings) {
    expand_use_segment(&[], body.trim(), bindings);
}

fn expand_use_segment(prefix: &[String], text: &str, bindings: &mut UseBindings) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if let Some(brace_pos) = text.find('{') {
        let Some(close) = match_brace(text, brace_pos) else {
            return;
        };
        let before = text[..brace_pos].trim_end_matches(':');
        let mut path_prefix: Vec<String> = before
            .split("::")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let mut full_prefix = prefix.to_vec();
        full_prefix.append(&mut path_prefix);
        let inner = &text[brace_pos + 1..close];
        for leaf in split_top_level_commas(inner) {
            expand_use_segment(&full_prefix, leaf, bindings);
        }
        return;
    }
    expand_use_leaf(prefix, text, bindings);
}

fn expand_use_leaf(prefix: &[String], leaf: &str, bindings: &mut UseBindings) {
    let leaf = leaf.trim();
    if leaf.is_empty() {
        return;
    }
    let (name_part, alias_part) = match leaf.split_once(" as ") {
        Some((n, a)) => (n.trim(), Some(a.trim().to_string())),
        None => (leaf, None),
    };
    let mut full: Vec<String> = prefix.to_vec();
    if name_part != "self" && !name_part.is_empty() {
        full.push(name_part.to_string());
    }
    let full_path = full.join("::");
    // Absent an explicit `as` alias, the bound name is the LAST path
    // segment — for a braced leaf like `{var}` `name_part` is already just
    // that one segment, but for a plain unbraced import (`use std::env;`,
    // `use std::env::var;`) `name_part` is the WHOLE dotted path (see the
    // `full`/`full_path` construction above, which relies on exactly that
    // to build the match key), so the bound name must be split out of it
    // here rather than reused whole — otherwise a bare `use std::env;`
    // binds the unusable literal name "std::env" instead of "env", and a
    // later `env::var(...)` call in the same file is never recognized.
    let bound_name = alias_part.unwrap_or_else(|| {
        if name_part == "self" {
            prefix.last().cloned().unwrap_or_default()
        } else {
            name_part
                .rsplit("::")
                .next()
                .unwrap_or(name_part)
                .trim()
                .to_string()
        }
    });
    if bound_name.is_empty() {
        return;
    }
    match full_path.as_str() {
        "std::env" => {
            bindings.module_aliases.insert(bound_name);
        }
        "std::env::var" => {
            bindings.fn_aliases.insert(bound_name, "var".to_string());
        }
        "std::env::var_os" => {
            bindings.fn_aliases.insert(bound_name, "var_os".to_string());
        }
        "std::env::vars" => {
            bindings.fn_aliases.insert(bound_name, "vars".to_string());
        }
        "std::env::vars_os" => {
            bindings
                .fn_aliases
                .insert(bound_name, "vars_os".to_string());
        }
        // A glob one level below `std::env` binds all four read functions
        // as bare names; a glob on `std` itself binds `env` as a module
        // alias so `env::var(...)` resolves bare too. Both spellings
        // reduce to these exact literal paths regardless of whether the
        // source wrote them with or without an enclosing `use` brace group
        // (see `expand_use_segment`/`expand_use_leaf` above), so no extra
        // case-handling is needed beyond these two match arms. Nothing
        // further than one level is followed — see the module doc.
        "std::env::*" => {
            for name in ["var", "var_os", "vars", "vars_os"] {
                bindings
                    .fn_aliases
                    .insert(name.to_string(), name.to_string());
            }
        }
        "std::*" => {
            bindings.module_aliases.insert("env".to_string());
        }
        _ => {}
    }
}

fn match_brace(text: &str, open_pos: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (idx, ch) in text.char_indices() {
        if idx < open_pos {
            continue;
        }
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_commas(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut last_split = 0usize;
    for (idx, ch) in text.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&text[last_split..idx]);
                last_split = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[last_split..]);
    parts
}

// ---------------------------------------------------------------------
// `const NAME: &str = "VALUE";` resolution, so a call like
// `env::var_os(SOME_CONST)` still recovers the real variable name instead
// of being reported as dynamic.
// ---------------------------------------------------------------------

fn collect_const_string_map(
    masked: &[char],
    strings_by_start: &HashMap<usize, &(usize, usize, String)>,
) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let n = masked.len();
    let mut i = 0usize;
    while i < n {
        if is_word_at(masked, i, "const") {
            let after_kw = skip_ws_forward(masked, i + 5);
            if let Some((name, after_name)) = read_ident_forward(masked, after_kw) {
                let mut k = after_name;
                while k < n && masked[k] != '=' && masked[k] != ';' {
                    k += 1;
                }
                if k < n && masked[k] == '=' {
                    let arg_pos = skip_trivial_forward(masked, strings_by_start, k + 1);
                    if let Some(entry) = strings_by_start.get(&arg_pos) {
                        map.insert(name, entry.2.clone());
                    }
                }
                i = after_name;
                continue;
            }
        }
        i += 1;
    }
    map
}

// ---------------------------------------------------------------------
// Enclosing-function attribution via brace-range extraction.
// ---------------------------------------------------------------------

fn collect_fn_ranges(masked: &[char]) -> Vec<(String, usize, usize)> {
    let mut ranges = Vec::new();
    let n = masked.len();
    let mut i = 0usize;
    while i < n {
        if is_word_at(masked, i, "fn") {
            let after_kw = skip_ws_forward(masked, i + 2);
            if let Some((name, after_name)) = read_ident_forward(masked, after_kw) {
                let mut depth = 0i32;
                let mut k = after_name;
                let mut found_open: Option<usize> = None;
                while k < n {
                    if masked[k] == '-' && masked.get(k + 1) == Some(&'>') {
                        k += 2;
                        continue;
                    }
                    match masked[k] {
                        '(' | '[' | '<' => depth += 1,
                        ')' | ']' => depth -= 1,
                        '>' => {
                            if depth > 0 {
                                depth -= 1;
                            }
                        }
                        '{' if depth <= 0 => found_open = Some(k),
                        ';' if depth <= 0 => break,
                        _ => {}
                    }
                    if found_open.is_some() {
                        break;
                    }
                    k += 1;
                }
                if let Some(open) = found_open {
                    let mut bdepth = 1i32;
                    let mut m = open + 1;
                    while m < n && bdepth > 0 {
                        match masked[m] {
                            '{' => bdepth += 1,
                            '}' => bdepth -= 1,
                            _ => {}
                        }
                        m += 1;
                    }
                    ranges.push((name, open, m));
                    i = m;
                    continue;
                }
                i = after_name;
                continue;
            }
        }
        i += 1;
    }
    ranges
}

fn enclosing_function(ranges: &[(String, usize, usize)], pos: usize) -> &str {
    ranges
        .iter()
        .filter(|(_, open, close)| *open <= pos && pos < *close)
        .min_by_key(|(_, open, close)| close - open)
        .map(|(name, _, _)| name.as_str())
        .unwrap_or(MODULE_LEVEL)
}

// ---------------------------------------------------------------------
// Call-site detection.
// ---------------------------------------------------------------------

fn preceded_by_path_or_method(chars: &[char], ident_start: usize) -> bool {
    let pos = skip_ws_backward(chars, ident_start);
    pos > 0 && (chars[pos - 1] == ':' || chars[pos - 1] == '.')
}

/// Returns `Some(true)` when the identifier ending right before
/// `ident_start` qualifies this call as a genuine `std::env` read (either
/// fully qualified, or through a local module alias established by a `use`
/// statement), `Some(false)` when it is qualified by something else
/// entirely, and `None` when there is no qualifying path at all.
fn qualifier_is_env(chars: &[char], ident_start: usize, bindings: &UseBindings) -> Option<bool> {
    let pos = skip_ws_backward(chars, ident_start);
    if pos < 2 || chars[pos - 1] != ':' || chars[pos - 2] != ':' {
        return None;
    }
    let pos = skip_ws_backward(chars, pos - 2);
    let (qual_start, qualifier) = read_ident_backward(chars, pos)?;
    if qualifier == "env" {
        let p2 = skip_ws_backward(chars, qual_start);
        if p2 >= 2 && chars[p2 - 1] == ':' && chars[p2 - 2] == ':' {
            let p3 = skip_ws_backward(chars, p2 - 2);
            if let Some((_, before)) = read_ident_backward(chars, p3)
                && before == "std"
            {
                return Some(true);
            }
        }
        return Some(bindings.module_aliases.contains("env"));
    }
    Some(bindings.module_aliases.contains(&qualifier))
}

fn extract_variable_value(
    masked: &[char],
    strings_by_start: &HashMap<usize, &(usize, usize, String)>,
    const_map: &BTreeMap<String, String>,
    arg_pos: usize,
    canonical_fn: &str,
) -> String {
    if canonical_fn == "vars" || canonical_fn == "vars_os" {
        return ALL_VARIABLES.to_string();
    }
    let pos = skip_trivial_forward(masked, strings_by_start, arg_pos);
    if let Some(entry) = strings_by_start.get(&pos) {
        return entry.2.clone();
    }
    if let Some((name, _)) = read_ident_forward(masked, pos)
        && let Some(value) = const_map.get(&name)
    {
        return value.clone();
    }
    DYNAMIC_VARIABLE.to_string()
}

/// Every direct `std::env::{var, var_os, vars, vars_os}` call site, as
/// `(call_start, first_argument_position, resolved_variable)`. Keeping the
/// raw argument position (not just the resolved variable) lets a second
/// pass below tell whether a `<dynamic>` read is forwarding one of the
/// enclosing function's OWN parameters — the shape that makes a function a
/// wrapper worth inventorying calls to.
fn find_call_sites(
    masked: &[char],
    strings_by_start: &HashMap<usize, &(usize, usize, String)>,
    const_map: &BTreeMap<String, String>,
    bindings: &UseBindings,
) -> Vec<(usize, usize, String)> {
    let mut sites = Vec::new();
    let n = masked.len();
    let mut i = 0usize;
    while i < n {
        // Delegates to the shared `read_ident_forward` (rather than
        // inlining its own identifier-scan loop) so a raw-identifier call
        // site (`std::env::r#var(...)`) is read the same, correct way
        // every other identifier read in this file is: this exact spot
        // used to hand-roll the loop locally, which meant fixing the raw
        // identifier gap in the shared reader alone did not fix THIS call
        // site — the duplicate had to be found and removed, not just the
        // shared function patched.
        if let Some((ident, end)) = read_ident_forward(masked, i) {
            let start = i;
            // A bare (unqualified) name bound by a `use` import — including
            // a glob's own bare names — is checked FIRST, but only when it
            // is genuinely unqualified: a glob also binds the four
            // canonical names to themselves (`"var" -> "var"`, ...), and
            // that must never shadow the SEPARATE fully-qualified check
            // below for a `std::env::var(...)` or module-aliased
            // `alias::var(...)` call later in the same file.
            let canonical = if !preceded_by_path_or_method(masked, start)
                && let Some(target) = bindings.fn_aliases.get(&ident)
            {
                Some(target.clone())
            } else if matches!(ident.as_str(), "var" | "var_os" | "vars" | "vars_os") {
                match qualifier_is_env(masked, start, bindings) {
                    Some(true) => Some(ident.clone()),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(canonical_fn) = canonical {
                let k = skip_ws_forward(masked, end);
                if k < n && masked[k] == '(' {
                    let arg_pos = k + 1;
                    let variable = extract_variable_value(
                        masked,
                        strings_by_start,
                        const_map,
                        arg_pos,
                        &canonical_fn,
                    );
                    sites.push((start, arg_pos, variable));
                }
            }
            i = end;
            continue;
        }
        i += 1;
    }
    sites
}

// ---------------------------------------------------------------------
// Wrapper-function detection: a wrapper is any function whose own body
// forwards one of ITS OWN parameters straight into a direct
// `std::env::{var,var_os,vars,vars_os}` call — discovered structurally
// (never a hardcoded name list) by looking at what `find_call_sites`
// already resolves to `<dynamic>` and checking whether that dynamic
// argument is literally the name of a parameter in scope. Once a wrapper
// is known, every CALL to it anywhere else in the same file is inventoried
// the same way a direct `std::env::var` call would be.
// ---------------------------------------------------------------------

struct FnSig {
    name: String,
    params: Vec<String>,
    body_open: usize,
    body_close: usize,
}

/// The parameter's bare name from one comma-separated slice of a parameter
/// list (`"name: &str"`, `"mut name: &str"`, `"&self"`, `"&mut self"`,
/// `"self"`), or `None` for a receiver parameter or anything not shaped
/// like `name: Type`.
fn param_name_from_text(part: &str) -> Option<String> {
    let mut s = part.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix('&') {
        s = rest.trim_start();
        if let Some(rest) = s.strip_prefix("mut ") {
            s = rest.trim_start();
        }
    }
    if let Some(rest) = s.strip_prefix("mut ") {
        s = rest.trim_start();
    }
    if s == "self" || s.starts_with("self:") || s.starts_with("self ") {
        return None;
    }
    let name = s.split(':').next().unwrap_or("").trim();
    if name.is_empty() || !name.chars().next().is_some_and(is_ident_start) {
        None
    } else {
        Some(name.to_string())
    }
}

fn parse_param_names(masked: &[char], start: usize, end: usize) -> Vec<String> {
    if start >= end {
        return Vec::new();
    }
    let text: String = masked[start..end].iter().collect();
    split_top_level_commas(&text)
        .into_iter()
        .filter_map(param_name_from_text)
        .collect()
}

/// The starting position of each top-level (not nested inside `()`, `[]`,
/// or `{}`) comma-separated argument in `[start, end)`.
fn split_top_level_arg_starts(masked: &[char], start: usize, end: usize) -> Vec<usize> {
    let mut starts = vec![start];
    let mut depth = 0i32;
    let mut i = start;
    while i < end {
        match masked[i] {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => starts.push(i + 1),
            _ => {}
        }
        i += 1;
    }
    starts
}

/// Every function (or method) signature with a body, recording its
/// non-receiver parameter names alongside its body's brace range. A
/// function-pointer TYPE position (`fn(&T) -> bool`) has no identifier
/// where a name is expected and is silently skipped, exactly like
/// `collect_fn_ranges` already does.
fn collect_fn_signatures(masked: &[char]) -> Vec<FnSig> {
    let mut sigs = Vec::new();
    let n = masked.len();
    let mut i = 0usize;
    while i < n {
        if is_word_at(masked, i, "fn") {
            let after_kw = skip_ws_forward(masked, i + 2);
            if let Some((name, after_name)) = read_ident_forward(masked, after_kw) {
                let mut k = skip_ws_forward(masked, after_name);
                if k < n
                    && masked[k] == '<'
                    && let Some(close) = match_delim(masked, k, '<', '>')
                {
                    k = skip_ws_forward(masked, close + 1);
                }
                let mut params = Vec::new();
                if k < n
                    && masked[k] == '('
                    && let Some(close_paren) = match_paren(masked, k)
                {
                    params = parse_param_names(masked, k + 1, close_paren);
                    k = close_paren + 1;
                }
                let mut depth = 0i32;
                let mut m = k;
                let mut found_open: Option<usize> = None;
                while m < n {
                    match masked[m] {
                        '(' | '[' => depth += 1,
                        ')' | ']' => depth -= 1,
                        '<' => depth += 1,
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
                if let Some(open) = found_open {
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
                    sigs.push(FnSig {
                        name,
                        params,
                        body_open: open,
                        body_close: z,
                    });
                    i = z;
                    continue;
                }
                i = k;
                continue;
            }
        }
        i += 1;
    }
    sigs
}

fn preceded_by_fn_keyword(chars: &[char], ident_start: usize) -> bool {
    let pos = skip_ws_backward(chars, ident_start);
    matches!(read_ident_backward(chars, pos), Some((_, word)) if word == "fn")
}

/// The bare identifier an argument position starts with, when the argument
/// is literally an identifier (as opposed to a string literal, which
/// `extract_variable_value` already resolves, or some other expression).
/// Deliberately does NOT consult the const map: the caller wants to know
/// whether this is a raw name forwarded from the enclosing scope, which a
/// resolved constant is not.
fn arg_leading_identifier(
    masked: &[char],
    strings_by_start: &HashMap<usize, &(usize, usize, String)>,
    arg_pos: usize,
) -> Option<String> {
    let pos = skip_trivial_forward(masked, strings_by_start, arg_pos);
    if strings_by_start.contains_key(&pos) {
        return None;
    }
    read_ident_forward(masked, pos).map(|(name, _)| name)
}

/// Every wrapper function found in this file, as `name -> the 0-based
/// index (receiver excluded) of the parameter it forwards to a direct
/// `std::env` read`. A function qualifies when one of its OWN direct env
/// reads (already found by `find_call_sites`) is `<dynamic>` and that
/// dynamic argument is literally one of its own parameter names — and it
/// is not defined inside a `#[cfg(test)]` item.
fn detect_wrapper_functions(
    masked: &[char],
    strings_by_start: &HashMap<usize, &(usize, usize, String)>,
    direct_sites: &[(usize, usize, String)],
    signatures: &[FnSig],
    cfg_test_ranges: &[(usize, usize)],
) -> BTreeMap<String, usize> {
    let mut wrappers = BTreeMap::new();
    for (call_pos, arg_pos, resolved) in direct_sites {
        if resolved.as_str() != DYNAMIC_VARIABLE {
            continue;
        }
        let Some(raw_ident) = arg_leading_identifier(masked, strings_by_start, *arg_pos) else {
            continue;
        };
        let Some(sig) = signatures
            .iter()
            .filter(|sig| sig.body_open <= *call_pos && *call_pos < sig.body_close)
            .min_by_key(|sig| sig.body_close - sig.body_open)
        else {
            continue;
        };
        if in_any_range(cfg_test_ranges, sig.body_open) {
            continue;
        }
        if let Some(index) = sig.params.iter().position(|p| *p == raw_ident) {
            wrappers.insert(sig.name.clone(), index);
        }
    }
    wrappers
}

/// Every call to a known wrapper, as `(call_start, resolved_variable)` —
/// the literal at the wrapper's name-parameter position, or `<dynamic>`
/// when that argument is itself computed. Matches both free-function call
/// syntax (`env_u64(...)`) and method-call syntax (`facts.env_var(...)`);
/// excludes the wrapper's own `fn NAME(` definition line.
fn find_wrapper_call_sites(
    masked: &[char],
    strings_by_start: &HashMap<usize, &(usize, usize, String)>,
    const_map: &BTreeMap<String, String>,
    wrappers: &BTreeMap<String, usize>,
) -> Vec<(usize, String)> {
    let mut sites = Vec::new();
    let n = masked.len();
    let mut i = 0usize;
    while i < n {
        // Same delegation-not-duplication fix as `find_call_sites`: a raw
        // identifier calling a wrapper (`r#env_reader(...)`, or a wrapper
        // itself named with a raw marker) is read correctly only because
        // this loop calls the shared reader instead of re-scanning
        // `is_ident_char` locally.
        if let Some((ident, end)) = read_ident_forward(masked, i) {
            let start = i;
            if let Some(&param_index) = wrappers.get(&ident) {
                let k = skip_ws_forward(masked, end);
                if k < n
                    && masked[k] == '('
                    && !preceded_by_fn_keyword(masked, start)
                    && let Some(close) = match_paren(masked, k)
                {
                    let arg_starts = split_top_level_arg_starts(masked, k + 1, close);
                    if let Some(&arg_start) = arg_starts.get(param_index) {
                        let variable = extract_variable_value(
                            masked,
                            strings_by_start,
                            const_map,
                            arg_start,
                            "",
                        );
                        sites.push((start, variable));
                    }
                }
            }
            i = end;
            continue;
        }
        i += 1;
    }
    sites
}

/// The pure detector: every `std::env::{var, var_os, vars, vars_os}` read
/// (fully qualified, reached through an aliased/glob `use` import, or
/// reached through an in-crate wrapper function's own literal-keyed call
/// site) found in `source`, labeled with the file identity the caller
/// supplies. Both the real scan and the planted-fault proofs below call
/// this one function.
///
/// Direct sites and wrapper-call sites are found by two separate
/// left-to-right passes over the file, so they are merged and re-sorted by
/// character position BEFORE the per-`(function, variable)` occurrence
/// counter runs — otherwise a wrapper-call site would always be numbered
/// after every direct site regardless of which one actually comes first in
/// the file, which would make the assigned occurrence non-deterministic
/// with respect to genuine text order (still deterministic run-to-run, but
/// not meaningfully ordered, which defeats the point of numbering at all).
fn environment_read_sites(file_label: &str, source: &str) -> Vec<EnvReadSite> {
    let lexed = lex(source);
    let bindings = collect_use_bindings(&lexed.masked);
    let strings_by_start: HashMap<usize, &(usize, usize, String)> =
        lexed.strings.iter().map(|entry| (entry.0, entry)).collect();
    let const_map = collect_const_string_map(&lexed.masked, &strings_by_start);
    let fn_ranges = collect_fn_ranges(&lexed.masked);
    let signatures = collect_fn_signatures(&lexed.masked);
    let cfg_test_ranges = collect_cfg_test_ranges(source, &lexed.masked);

    let direct_sites = find_call_sites(&lexed.masked, &strings_by_start, &const_map, &bindings);

    let mut positioned: Vec<(usize, String, String)> = direct_sites
        .iter()
        .map(|(pos, _arg_pos, variable)| {
            (
                *pos,
                enclosing_function(&fn_ranges, *pos).to_string(),
                variable.clone(),
            )
        })
        .collect();

    let wrappers = detect_wrapper_functions(
        &lexed.masked,
        &strings_by_start,
        &direct_sites,
        &signatures,
        &cfg_test_ranges,
    );
    if !wrappers.is_empty() {
        let wrapper_sites =
            find_wrapper_call_sites(&lexed.masked, &strings_by_start, &const_map, &wrappers);
        positioned.extend(wrapper_sites.into_iter().map(|(pos, variable)| {
            (
                pos,
                enclosing_function(&fn_ranges, pos).to_string(),
                variable,
            )
        }));
    }
    positioned.sort_by_key(|(pos, _, _)| *pos);

    let mut occurrence_counts: HashMap<(String, String), usize> = HashMap::new();
    positioned
        .into_iter()
        .map(|(_, function, variable)| {
            let counter = occurrence_counts
                .entry((function.clone(), variable.clone()))
                .or_insert(0);
            *counter += 1;
            EnvReadSite {
                file: file_label.to_string(),
                function,
                variable,
                occurrence: *counter,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------
// Baseline loading + the real scan.
// ---------------------------------------------------------------------

/// This crate's own manifest directory (`crates/vigil`), used only to
/// locate this file's own baseline text file — the crate-scope source walk
/// itself (`PRODUCTION_CRATES`, `crates_root`, `rust_sources`) is shared
/// with `cli_secret_flag_surface.rs` via `source_scan_lexer.rs`, so the two
/// scans cannot silently drift onto different crate lists.
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The baseline is a flat, hand-editable text file: one `file|function|
/// variable` record per line, optionally followed by a fourth
/// `|occurrence` field when a SECOND (or later) read of the same variable
/// in the same function is a deliberate, reviewed addition — an omitted
/// fourth field means occurrence 1, so the common case (one read) stays
/// uncluttered. `#`-prefixed comments and blank lines are ignored. The
/// starting ceiling is DERIVED from the two frozen lists below —
/// [`ORIGINAL_ENVIRONMENT_READ_SITES`] plus [`AMENDED_ENVIRONMENT_READ_SITES`]
/// — rather than a hand-maintained number, so the file itself cannot be
/// padded with entries that never existed in source just to make room for a
/// new one, and a genuine amendment cannot silently drift the ceiling out of
/// step with what the two lists actually contain.
const BASELINE_STARTING_SITE_COUNT: usize =
    ORIGINAL_ENVIRONMENT_READ_SITES.len() + AMENDED_ENVIRONMENT_READ_SITES.len();

/// The ORIGINAL membership of `environment_read_surface.baseline.txt` as
/// first recorded when this subset guard was added — frozen HERE, in this
/// file, and never edited since. A genuinely new, reviewed environment read
/// discovered after that point is never added here: it goes into
/// [`AMENDED_ENVIRONMENT_READ_SITES`] below, an explicit, separately
/// documented amendment list that discloses growth instead of hiding it
/// inside an edit to a constant whose own doc comment claims immutability
/// (cold-review-r4 finding 19 — `BASELINE_STARTING_SITE_COUNT` was raised
/// 70→73 and three entries were appended straight into this list, which
/// this split undoes). [`authorized_environment_read_sites`] is the combined
/// membership — original plus amended — the subset checks below actually
/// run against; nothing here claims the environment-read surface itself
/// "has shrunk, never grown" — only that every site it is allowed to grow
/// by is named, reviewed, and disclosed, never smuggled in by editing a
/// number.
///
/// The equality check in
/// `no_new_environment_read_sites_and_baseline_only_shrinks`
/// (`observed == baseline`) does not catch a COORDINATED one-for-one swap:
/// add a hidden read to source and add a matching entry to the baseline,
/// remove one real baselined read from both — both files were edited to
/// match each other, so the equality holds and the count stays at the
/// ceiling; the swap self-approves. This constant and the subset check
/// below close that: a baseline entry that was never in this frozen
/// original or the disclosed amendment list fails regardless of what else
/// changed in the same edit, while a legitimate burn-down (removing an
/// entry as a read migrates to a declared setting) still leaves a SUBSET of
/// the authorized membership and stays green — the ratchet the baseline
/// exists to enable is preserved.
///
/// "Frozen" is about what this list may GRANT, not about its length. An
/// entry whose variable no longer exists anywhere — neither in source nor
/// in the baseline — is struck from it, because leaving it standing tells a
/// reader that a read exists which does not, and it keeps authorizing a
/// baseline entry nobody could justify today. A removal can only ever
/// tighten the authorized membership, so it cannot smuggle a read in; an
/// ADDITION still may, which is why additions go to
/// [`AMENDED_ENVIRONMENT_READ_SITES`] below and never here.
const ORIGINAL_ENVIRONMENT_READ_SITES: &[(&str, &str, &str)] = &[
    ("vigil/config.rs", "env_overrides", "VIGIL_DATA_DIR"),
    ("vigil/config.rs", "env_overrides", "VIGIL_STORE_PATH"),
    ("vigil/config.rs", "env_overrides", "VIGIL_HEALTH_PORT"),
    ("vigil/config.rs", "env_overrides", "VIGIL_REVIEW_PORT"),
    ("vigil/config.rs", "env_overrides", "VIGIL_SITE_NAME"),
    ("vigil/config.rs", "env_overrides", "VIGIL_CAMERA_NAME"),
    ("vigil/config.rs", "env_overrides", "VIGIL_RTSP_URL"),
    ("vigil/config.rs", "env_overrides", "VIGIL_LIVE_RTSP_URL"),
    ("vigil/config.rs", "env_overrides", "VIGIL_RTSP_USERNAME"),
    ("vigil/config.rs", "env_overrides", "VIGIL_RTSP_PASSWORD"),
    (
        "vigil/config.rs",
        "env_overrides",
        "VIGIL_DETECTOR_MODEL_ID",
    ),
    (
        "vigil/config.rs",
        "env_overrides",
        "VIGIL_DETECTOR_MODEL_PATH",
    ),
    (
        "vigil/config.rs",
        "env_overrides",
        "VIGIL_RECOGNITION_WEIGHTS_DIR",
    ),
    (
        "vigil/config.rs",
        "env_overrides",
        "VIGIL_RECOGNITION_SPACE_ID",
    ),
    (
        "vigil/config.rs",
        "env_overrides",
        "VIGIL_RECOGNITION_THRESHOLD",
    ),
    (
        "vigil/config.rs",
        "env_overrides",
        "VIGIL_DETECTOR_CONFIDENCE_THRESHOLD",
    ),
    (
        "vigil/config.rs",
        "env_overrides",
        "VIGIL_DETECTOR_SAMPLE_FRAMES",
    ),
    (
        "vigil/config.rs",
        "env_overrides",
        "VIGIL_DETECTOR_STATIONARY_INTERVAL_SECS",
    ),
    ("vigil/config.rs", "env_overrides", "MQTT_HOST"),
    ("vigil/config.rs", "env_overrides", "MQTT_PORT"),
    ("vigil/config.rs", "env_overrides", "MQTT_USER"),
    ("vigil/config.rs", "env_overrides", "MQTT_USERNAME"),
    ("vigil/config.rs", "env_overrides", "MQTT_PASSWORD"),
    ("vigil/config.rs", "env_overrides", "VIGIL_SERVICE_ID"),
    ("vigil/config.rs", "env_overrides", "VIGIL_FABRIC_TICKET"),
    (
        "vigil/config.rs",
        "env_overrides",
        "VIGIL_FABRIC_WORKER_LEASE_MS",
    ),
    (
        "vigil/config.rs",
        "env_overrides",
        "VIGIL_FABRIC_FALLBACK_HORIZON_MS",
    ),
    (
        "vigil/config.rs",
        "env_overrides",
        "VIGIL_HARDWARE_DECODING",
    ),
    (
        "vigil/config.rs",
        "env_overrides",
        "VIGIL_ACCELERATED_DETECTION",
    ),
    ("vigil/config.rs", "env_overrides", "VIGIL_FABRIC_HUB"),
    (
        "vigil/config.rs",
        "env_overrides",
        "VIGIL_FABRIC_ALLOW_FRAME_OFFLOAD",
    ),
    ("vigil/config.rs", "env_intent_bool", DYNAMIC_VARIABLE),
    (
        "vigil/config.rs",
        "default_options_json_path",
        "VIGIL_TEST_OPTIONS_JSON",
    ),
    ("vigil/lib.rs", "store_path_from_env", "VIGIL_STORE_PATH"),
    ("vigil/lib.rs", "data_dir_from_env", "VIGIL_DATA_DIR"),
    ("vigil/lib.rs", "data_dir_from_env", "VIGIL_STORE_PATH"),
    (
        "vigil/control_socket.rs",
        "control_socket_path",
        "VIGIL_CONTROL_SOCKET",
    ),
    (
        "vigil/privilege.rs",
        "prepare_runtime_user",
        "VIGIL_DROP_PRIVILEGES",
    ),
    ("vigil/privilege.rs", "env_u32", DYNAMIC_VARIABLE),
    (
        "vigil/privilege.rs",
        "prepare_runtime_user",
        "VIGIL_RUN_UID",
    ),
    (
        "vigil/privilege.rs",
        "prepare_runtime_user",
        "VIGIL_RUN_GID",
    ),
    (
        "vigil/privilege.rs",
        "drop_privileges",
        "VIGIL_RUN_SUPPLEMENTAL_GIDS",
    ),
    (
        "vigil/supervisor.rs",
        "fetch_supervisor_mqtt",
        "SUPERVISOR_TOKEN",
    ),
    (
        "vigil/health.rs",
        "bind",
        "VIGIL_TEST_EPHEMERAL_HEALTH_PORT",
    ),
    (
        "vigil/yolox_detector.rs",
        "from_env",
        "VIGIL_DETECTOR_FORWARD_PROBE_PATH",
    ),
    (
        "vigil/yolox_detector.rs",
        "from_env",
        "VIGIL_DETECTOR_FORWARD_PROBE_NONCE",
    ),
    (
        "vigil/media_pipeline.rs",
        "hardware_probe_deadline",
        "VIGIL_HARDWARE_PROBE_DEADLINE_SECS",
    ),
    (
        "vigil/decode_gstreamer.rs",
        "decode_probe_deadline",
        "VIGIL_DECODE_PROBE_DEADLINE_SECS",
    ),
    ("vigil/doctor.rs", "env_var", DYNAMIC_VARIABLE),
    (
        "vigil/doctor.rs",
        "user_can_access_device",
        "VIGIL_RUN_SUPPLEMENTAL_GIDS",
    ),
    (
        "vigil/doctor.rs",
        "resolve_service_user",
        "VIGIL_SERVICE_USER",
    ),
    (
        "vigil/doctor.rs",
        "doctor_decode_receipt_with_hardware_backend",
        "USER",
    ),
    ("vigil/doctor.rs", "live_device_access_finding", "USER"),
    (
        "vigil/fabric.rs",
        "resolved_worker_lease_duration_ms",
        "VIGIL_FABRIC_WORKER_LEASE_MS",
    ),
    (
        "vigil/fabric.rs",
        "fabric_bring_up",
        "VIGIL_FABRIC_BRINGUP_DELAY_MS",
    ),
    (
        "vigil/detection_accel.rs",
        "detection_probe_deadline",
        "VIGIL_DETECTION_PROBE_DEADLINE_SECS",
    ),
    (
        "vigil/detection_accel.rs",
        "detection_late_window",
        "VIGIL_DETECTION_LATE_WINDOW_SECS",
    ),
    (
        "vigil/ha_camera_registration.rs",
        "register_generic_camera",
        "SUPERVISOR_TOKEN",
    ),
    ("vigil/runtime.rs", "env_is", DYNAMIC_VARIABLE),
    ("vigil/runtime.rs", "env_u64", DYNAMIC_VARIABLE),
    (
        "vigil/runtime.rs",
        "start_rtsp_probe",
        "VIGIL_DETECTOR_QUEUE_CAPACITY",
    ),
    (
        "vigil/runtime.rs",
        "start_rtsp_probe",
        "VIGIL_DETECTOR_WORK_DELAY_MS",
    ),
    (
        "vigil/runtime.rs",
        "start_rtsp_probe",
        "VIGIL_DETECTOR_DECISION_DELAY_MS",
    ),
    (
        "vigil/runtime.rs",
        "start_rtsp_probe",
        "VIGIL_RTSP_RETRY_INITIAL_MS",
    ),
    (
        "vigil/runtime.rs",
        "start_rtsp_probe",
        "VIGIL_RTSP_RETRY_MAX_MS",
    ),
    (
        "vigil/runtime.rs",
        "start_rtsp_probe",
        "VIGIL_CAPTURE_FRAMES",
    ),
    (
        "vigil/runtime.rs",
        "maybe_crash_after_startup_node",
        "VIGIL_FAULT_CRASH_AFTER_STARTUP_NODE",
    ),
    ("vigil/config.rs", "set", DYNAMIC_VARIABLE),
    ("vigil/config.rs", "remove", DYNAMIC_VARIABLE),
];

/// Sites read after the original freeze above, disclosed HERE by name
/// instead of appended silently into [`ORIGINAL_ENVIRONMENT_READ_SITES`] —
/// each is named against one of `vigil-settings-autority-direction.md`'s
/// kept-in-the-environment categories, per the settings-authority arc.
/// [`ORIGINAL_ENVIRONMENT_READ_SITES`] is immutable forever; an addition
/// here is permitted ONLY when it implements an already-ratified
/// non-behavior category — today: reporting that a behavior variable was
/// ignored, resolving a secret environment override, or reading the
/// Supervisor-issued token for platform service access — is individually
/// disclosed and reviewed, and creates no behavior authority. A new
/// category, or any behavior-input path, requires a new owner decision; it
/// is never added here on the strength of this comment alone. Criterion
/// 11's "has shrunk, never grown" is read against behavior-authoring
/// reads, which stay at zero growth forever — this mechanism only
/// discloses the narrow, ratified non-behavior growth it is permitted
/// (owner ruling 2026-08-13, `vigil-settings-finish-criteria.md` criterion
/// 11 addendum).
const AMENDED_ENVIRONMENT_READ_SITES: &[(&str, &str, &str)] = &[
    // Direction-doc category: the ignored-behavior-variable reporting
    // requirement itself — "An environment variable naming a behavior
    // setting is reported as ignored, with the reason and the place to set
    // it instead" is never a silent drop, and telling an operator a
    // variable did nothing structurally requires reading it first.
    (
        "vigil/settings_environment.rs",
        "ignored_behavior_variables",
        DYNAMIC_VARIABLE,
    ),
    // Direction-doc category: Secret material — the per-process environment
    // override leg for a stored secret (the direction doc's "Secret
    // material" bullet: the environment value wins over a stored one when
    // both name the same secret).
    (
        "vigil/settings_environment.rs",
        "resolve_secret",
        DYNAMIC_VARIABLE,
    ),
    // Direction-doc category: Platform-injected service discovery —
    // `SUPERVISOR_TOKEN` is listed by name in that category, alongside the two
    // already-frozen
    // `SUPERVISOR_TOKEN` sites above (`supervisor.rs::fetch_supervisor_mqtt`,
    // `ha_camera_registration.rs::register_generic_camera`); this is the
    // container's own issued token, read to reach the Supervisor for the
    // options self-write, never a human-configured value.
    (
        "vigil/settings_reflection.rs",
        "from_environment",
        "SUPERVISOR_TOKEN",
    ),
    // Not a new read: the frozen `start_rtsp_probe` site for this same
    // variable, relocated verbatim into the single helper that now performs
    // it. The read moved off a camera thread so a cameraless node reports the
    // same lever; nothing about what the variable may author changed, no
    // second read was created, and the frozen `start_rtsp_probe` entry above
    // is now unobservable in source. The behavior-authoring count is
    // unchanged, which is what criterion 11's "has shrunk, never grown" is
    // read against. ORIGINAL is immutable, and it is keyed on the function
    // name, so a relocation can only be disclosed here.
    (
        "vigil/runtime.rs",
        "detector_queue_capacity_lever",
        "VIGIL_DETECTOR_QUEUE_CAPACITY",
    ),
];

/// The combined membership the subset checks below actually run against —
/// [`ORIGINAL_ENVIRONMENT_READ_SITES`] (truly frozen) union
/// [`AMENDED_ENVIRONMENT_READ_SITES`] (disclosed, reviewed growth). A
/// baseline entry outside this combined set is unauthorized regardless of
/// which of the two lists it resembles.
fn authorized_environment_read_sites() -> BTreeSet<EnvReadSite> {
    original_environment_read_sites()
        .union(&amended_environment_read_sites())
        .cloned()
        .collect()
}

fn amended_environment_read_sites() -> BTreeSet<EnvReadSite> {
    let sites: BTreeSet<EnvReadSite> = AMENDED_ENVIRONMENT_READ_SITES
        .iter()
        .map(|(file, function, variable)| EnvReadSite {
            file: (*file).to_string(),
            function: (*function).to_string(),
            variable: (*variable).to_string(),
            occurrence: 1,
        })
        .collect();
    assert_eq!(
        sites.len(),
        AMENDED_ENVIRONMENT_READ_SITES.len(),
        "AMENDED_ENVIRONMENT_READ_SITES must not contain a duplicate (file, function, variable) \
         triple at occurrence 1"
    );
    sites
}

fn original_environment_read_sites() -> BTreeSet<EnvReadSite> {
    let sites: BTreeSet<EnvReadSite> = ORIGINAL_ENVIRONMENT_READ_SITES
        .iter()
        .map(|(file, function, variable)| EnvReadSite {
            file: (*file).to_string(),
            function: (*function).to_string(),
            variable: (*variable).to_string(),
            occurrence: 1,
        })
        .collect();
    assert_eq!(
        sites.len(),
        ORIGINAL_ENVIRONMENT_READ_SITES.len(),
        "ORIGINAL_ENVIRONMENT_READ_SITES must not contain a duplicate (file, function, variable) \
         triple at occurrence 1 — a real second read of the same variable in the same function \
         needs its own occurrence field, which this frozen 3-tuple list has no room for; none of \
         today's baseline needs that, so a duplicate here is a transcription error, not a real \
         second site"
    );
    sites
}

fn load_baseline() -> BTreeSet<EnvReadSite> {
    let path = crate_root()
        .join("tests")
        .join("environment_read_surface.baseline.txt");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    parse_baseline(&text)
}

fn parse_baseline(text: &str) -> BTreeSet<EnvReadSite> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.splitn(4, '|');
            let file = fields.next().unwrap_or_default().to_string();
            let function = fields.next().unwrap_or_default().to_string();
            let variable = fields.next().unwrap_or_default().to_string();
            let occurrence = fields
                .next()
                .and_then(|raw| raw.trim().parse::<usize>().ok())
                .unwrap_or(1);
            EnvReadSite {
                file,
                function,
                variable,
                occurrence,
            }
        })
        .collect()
}

fn scan_real_tree() -> BTreeSet<EnvReadSite> {
    let crates_root = crates_root();
    let mut observed = BTreeSet::new();
    for crate_name in PRODUCTION_CRATES {
        let src_root = crates_root.join(crate_name).join("src");
        assert!(
            src_root.is_dir(),
            "expected {} to exist; a guard that cannot find the source tree must fail loudly, \
             not silently scan nothing",
            src_root.display()
        );
        let sources = rust_sources(&src_root);
        assert!(
            !sources.is_empty(),
            "found zero .rs files under {}; a guard that cannot find the source tree must fail \
             loudly, not silently scan nothing",
            src_root.display()
        );
        for path in sources {
            let relative = path
                .strip_prefix(&src_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let label = format!("{crate_name}/{relative}");
            let Ok(source) = fs::read_to_string(&path) else {
                continue;
            };
            observed.extend(environment_read_sites(&label, &source));
        }
    }
    observed
}

#[test]
fn no_new_environment_read_sites_and_baseline_only_shrinks() {
    let baseline = load_baseline();
    // No standalone "baseline.len() <= BASELINE_STARTING_SITE_COUNT" ceiling
    // check here (there used to be one): it can never fire on its own. The
    // SUBSET check below (`smuggled.is_empty()`, `baseline ⊆ authorized`)
    // already implies it — a subset can never be larger than the set it is
    // a subset of, and `authorized_environment_read_sites()` can never
    // exceed `BASELINE_STARTING_SITE_COUNT` (it IS the union
    // `ORIGINAL_ENVIRONMENT_READ_SITES` plus `AMENDED_ENVIRONMENT_READ_SITES`
    // that constant is computed from). Keeping a redundant assertion that
    // cannot independently fail would misstate what this file actually
    // proves (cold-review-arc2-r5 finding 9).

    // A POSITIVE CONTROL, not a real-site count floor and not a
    // baseline-must-be-non-empty check (both existed here before and both
    // are removed): neither a count floor nor a non-empty requirement can
    // tell "every real site was migrated away" — the successful TERMINAL
    // state this baseline exists to reach as the configuration-surface
    // work proceeds — apart from "the scanner silently broke"; both look
    // identical as a low or zero count/empty baseline. This proves the
    // detector itself still recognizes a real environment read when one
    // exists, using a fixture wholly independent of how many real sites
    // remain, so a genuinely empty `observed`/`baseline` stays provably a
    // clean result rather than a silent failure, and full burn-down can
    // complete without this guard blocking it.
    let positive_control_sample = r#"
        fn __test_estate_environment_scan_positive_control() -> Option<String> {
            std::env::var("VIGIL_TEST_ESTATE_POSITIVE_CONTROL_SITE").ok()
        }
    "#;
    let positive_control_sites =
        environment_read_sites("positive-control.rs", positive_control_sample);
    assert_eq!(
        positive_control_sites.len(),
        1,
        "positive control: the scanner must always recognize this fixture read, independent of \
         how many real sites remain in the baseline — if this fails, the scanner itself is \
         broken, not merely burned down: {positive_control_sites:#?}"
    );

    let observed = scan_real_tree();
    // An EQUALITY check, not a one-directional subset check: a baseline
    // entry the real scan never observes must fail exactly as loudly as a
    // new site the baseline never listed. A subset check in only one
    // direction would let a resolver regression that makes the scan blind
    // to a real read pass silently — the stale baseline entry would simply
    // sit there unobserved forever, looking identical to a clean run.
    let new_sites: Vec<&EnvReadSite> = observed.difference(&baseline).collect();
    let stale_baseline_entries: Vec<&EnvReadSite> = baseline.difference(&observed).collect();
    assert!(
        new_sites.is_empty() && stale_baseline_entries.is_empty(),
        "environment_read_surface.baseline.txt and the real scan disagree.\n\
         site(s) found by the scan but missing from the baseline (add a deliberate, reviewed \
         entry, or migrate the read to a declared setting instead): {new_sites:#?}\n\
         site(s) listed in the baseline but never observed by the scan (either the read was \
         removed — update the baseline to match — or the scan can no longer see it, which is a \
         resolver bug to fix, not an entry to delete): {stale_baseline_entries:#?}"
    );

    // A SUBSET check against the authorized membership — the frozen original
    // (`ORIGINAL_ENVIRONMENT_READ_SITES`) plus the disclosed amendments
    // (`AMENDED_ENVIRONMENT_READ_SITES`) — not just the equality check above:
    // see `ORIGINAL_ENVIRONMENT_READ_SITES`'s own doc comment for why the
    // equality check alone cannot catch a coordinated swap (a new hidden
    // read added to source and the baseline together with an unrelated
    // baselined read removed from both, which keeps `observed == baseline`
    // and the starting-count ceiling unchanged).
    let authorized = authorized_environment_read_sites();
    let smuggled: Vec<&EnvReadSite> = baseline.difference(&authorized).collect();
    assert!(
        smuggled.is_empty(),
        "environment_read_surface.baseline.txt lists site(s) that are neither part of the frozen \
         original membership ORIGINAL_ENVIRONMENT_READ_SITES nor the disclosed \
         AMENDED_ENVIRONMENT_READ_SITES locks in, however the overall count and observed/baseline \
         equality balance out — a hidden read cannot enter through this baseline: {smuggled:#?}"
    );
}

// ---------------------------------------------------------------------
// Planted-fault proofs: the detector function itself, exercised directly
// (no real files touched), on samples that must be flagged and samples
// built only from allowed shapes that must not be.
// ---------------------------------------------------------------------

#[test]
fn detector_flags_a_planted_environment_read_and_leaves_allowed_shapes_alone() {
    let violation_sample = r#"
        use std::env as sneaky;
        use std::env::*;
        use std::*;

        const KNOWN_NAME: &str = "TOTALLY_FINE_LOOKING_NAME";

        fn totally_fine_looking_helper() -> Option<String> {
            std::env::var("REAL_LITERAL_READ").ok()
        }

        fn aliased_helper() -> Option<std::ffi::OsString> {
            sneaky::var_os(KNOWN_NAME)
        }

        fn bare_glob_helper() -> Option<String> {
            var("PLANTED_ENV_GLOB_BARE").ok()
        }

        fn qualified_glob_helper() -> Option<std::ffi::OsString> {
            env::var_os("PLANTED_ENV_GLOB_QUALIFIED")
        }
    "#;
    let sites: BTreeSet<EnvReadSite> = environment_read_sites("violation.rs", violation_sample)
        .into_iter()
        .collect();
    assert_eq!(
        sites,
        BTreeSet::from([
            EnvReadSite {
                file: "violation.rs".to_string(),
                function: "aliased_helper".to_string(),
                variable: "TOTALLY_FINE_LOOKING_NAME".to_string(),
                occurrence: 1,
            },
            EnvReadSite {
                file: "violation.rs".to_string(),
                function: "totally_fine_looking_helper".to_string(),
                variable: "REAL_LITERAL_READ".to_string(),
                occurrence: 1,
            },
            EnvReadSite {
                file: "violation.rs".to_string(),
                function: "bare_glob_helper".to_string(),
                variable: "PLANTED_ENV_GLOB_BARE".to_string(),
                occurrence: 1,
            },
            EnvReadSite {
                file: "violation.rs".to_string(),
                function: "qualified_glob_helper".to_string(),
                variable: "PLANTED_ENV_GLOB_QUALIFIED".to_string(),
                occurrence: 1,
            },
        ]),
        "the detector must flag the fully-qualified call, the aliased-import call, and both \
         glob-import spellings (`use std::env::*;` and `use std::*;`)"
    );

    // A raw-identifier spelling of the fully-qualified call
    // (`std::env::r#var(...)`) must be flagged exactly like
    // `std::env::var(...)` — `r#var` is not a distinct name, it is Rust's
    // own escape hatch for spelling an ordinary name next to a keyword
    // (verified: `type Alias = Foo; type r#Alias = Bar;` is rejected as
    // "the name `Alias` is defined multiple times", E0428, on `rustc
    // 1.94.0` — the two spellings name the SAME thing). Deliberately its
    // own isolated sample, with no `use std::env::*;` glob import in
    // scope: `violation_sample` above already binds a bare `var` name via
    // its own glob imports, so a raw-identifier call planted into THAT
    // sample would still get flagged through the unrelated bare-name path
    // even with the raw-identifier fix reverted — a false assurance this
    // isolated sample avoids by construction, leaving the fully-qualified
    // path (`qualifier_is_env`, which itself reads the "env"/"std"
    // qualifiers through the shared `read_ident_backward`) as the ONLY
    // possible route to detection. `find_call_sites` used to hand-roll its
    // own identifier-scan loop instead of calling the shared
    // `read_ident_forward`, which read `r#var` as the one-character
    // identifier `"r"` and silently dropped the rest — fixing the shared
    // reader alone would not have caught this site until that duplicate
    // loop was replaced with a call to it (confirmed by temporarily
    // disabling the reader's raw-identifier branch and re-running this
    // exact assertion: it goes red, an empty result, exactly as the
    // pre-fix code would have produced).
    let raw_identifier_sample = r#"
        fn raw_identifier_helper() -> Option<String> {
            std::env::r#var("PLANTED_ENV_RAW_IDENTIFIER").ok()
        }
    "#;
    let raw_identifier_sites = environment_read_sites("raw_identifier.rs", raw_identifier_sample);
    assert_eq!(
        raw_identifier_sites,
        vec![EnvReadSite {
            file: "raw_identifier.rs".to_string(),
            function: "raw_identifier_helper".to_string(),
            variable: "PLANTED_ENV_RAW_IDENTIFIER".to_string(),
            occurrence: 1,
        }],
        "a raw-identifier spelling of the fully-qualified call must be flagged: \
         {raw_identifier_sites:#?}"
    );

    // A Unicode-named site: the enclosing function's own name contains a
    // combining diacritical mark (U+0303 COMBINING TILDE, raw bytes right
    // after the ASCII `n`, rendering as "contraseña_password" — Spanish for
    // "password"; a genuinely benign, realistic name, not a contrived probe
    // string). Built with `format!` and a `'\u{0303}'` char literal, never
    // typed as a literal combining-mark byte in this test file's own
    // source, so the source is never at the mercy of an editor or tool
    // silently normalizing NFD (base + combining mark) to NFC (one
    // precomposed character) and quietly erasing the very divergence being
    // tested. Genuinely bites: `char::is_alphanumeric` returns `false` for a
    // bare combining mark (Unicode category Mark, not Letter or Number) —
    // verified — so the OLD reader stopped at the `n` and never read the
    // mark or the `a_password` after it, mislabeling this site under the
    // truncated function name `contrasen` rather than missing it outright;
    // `rustc 1.94.0` accepts the full name (verified by actually compiling a
    // two-character identifier — the letter `a` followed by the raw U+0301
    // COMBINING ACUTE ACCENT bytes, the same divergence one character
    // earlier; there is no `\u{}` escape in identifier syntax, so that
    // spelling is not itself compilable Rust). Confirmed by temporarily forcing
    // `is_ident_char` back to `char::is_alphanumeric() || c == '_'` and
    // re-running this exact assertion: it goes red (the recorded function
    // name comes back truncated to `contrasen`, not the full name),
    // restored before committing.
    let unicode_identifier_sample = format!(
        "fn contrasen{}a_password() -> Option<String> {{\n    \
             std::env::var(\"PLANTED_ENV_UNICODE_IDENTIFIER\").ok()\n\
         }}\n",
        '\u{0303}'
    );
    let unicode_identifier_sites =
        environment_read_sites("unicode_identifier.rs", &unicode_identifier_sample);
    assert_eq!(
        unicode_identifier_sites,
        vec![EnvReadSite {
            file: "unicode_identifier.rs".to_string(),
            function: format!("contrasen{}a_password", '\u{0303}'),
            variable: "PLANTED_ENV_UNICODE_IDENTIFIER".to_string(),
            occurrence: 1,
        }],
        "a Unicode-named enclosing function (a combining mark mid-name) must be read in full, \
         not truncated at the mark: {unicode_identifier_sites:#?}"
    );

    let allowed_sample = r#"
        use std::env;

        /// Mentions std::env::var("NOT_REAL") only in prose; a comment is
        /// not a read.
        fn describe_the_rule() -> &'static str {
            "call std::env::var(\"NOT_REAL\") yourself if you must"
        }

        fn read_argv() -> Vec<String> {
            std::env::args().collect()
        }

        fn read_argv_os() -> std::env::ArgsOs {
            env::args_os()
        }
    "#;
    let allowed_sites = environment_read_sites("allowed.rs", allowed_sample);
    assert!(
        allowed_sites.is_empty(),
        "argv reads, comments, and string literals that merely mention the call must not be \
         flagged as environment reads: {allowed_sites:#?}"
    );
}

#[test]
fn detector_flags_a_planted_wrapper_call_with_a_literal_key() {
    let sample = r#"
        fn env_flag(name: &str) -> bool {
            std::env::var(name).is_ok()
        }

        fn caller_with_literal() -> bool {
            env_flag("PLANTED_WRAPPER_LITERAL_KEY")
        }

        fn caller_with_computed(dynamic_name: &str) -> bool {
            env_flag(dynamic_name)
        }
    "#;
    let sites: BTreeSet<EnvReadSite> = environment_read_sites("wrapper.rs", sample)
        .into_iter()
        .collect();
    assert_eq!(
        sites,
        BTreeSet::from([
            EnvReadSite {
                file: "wrapper.rs".to_string(),
                function: "env_flag".to_string(),
                variable: DYNAMIC_VARIABLE.to_string(),
                occurrence: 1,
            },
            EnvReadSite {
                file: "wrapper.rs".to_string(),
                function: "caller_with_literal".to_string(),
                variable: "PLANTED_WRAPPER_LITERAL_KEY".to_string(),
                occurrence: 1,
            },
            EnvReadSite {
                file: "wrapper.rs".to_string(),
                function: "caller_with_computed".to_string(),
                variable: DYNAMIC_VARIABLE.to_string(),
                occurrence: 1,
            },
        ]),
        "a wrapper function's own definition stays <dynamic>, a call through it with a literal \
         key must be inventoried by that literal at its OWN call site, and a call that forwards \
         a genuinely computed value must stay honestly <dynamic> rather than disappearing: \
         {sites:#?}"
    );
}

#[test]
fn detector_resolves_plain_unaliased_use_imports() {
    // `use std::env;` (bare module import: no alias, no glob) followed by
    // a QUALIFIED `env::var(...)` call — the exact shape
    // `crates/vigil/src/yolox_detector.rs` uses for its forward-probe
    // reads. This is a regression proof for a real bug: an unbraced `use
    // std::env;` used to bind the unusable literal name "std::env" (the
    // whole leaf text) instead of "env", because `bound_name` reused the
    // raw multi-segment leaf whenever no explicit `as` alias was written —
    // so the module-alias lookup for a later bare `env::` qualifier always
    // missed.
    let module_import_sample = r#"
        use std::env;

        fn plain_module_import_helper() -> Option<String> {
            env::var("PLANTED_PLAIN_MODULE_IMPORT").ok()
        }
    "#;
    let sites: BTreeSet<EnvReadSite> =
        environment_read_sites("module_import.rs", module_import_sample)
            .into_iter()
            .collect();
    assert_eq!(
        sites,
        BTreeSet::from([EnvReadSite {
            file: "module_import.rs".to_string(),
            function: "plain_module_import_helper".to_string(),
            variable: "PLANTED_PLAIN_MODULE_IMPORT".to_string(),
            occurrence: 1,
        }]),
        "a plain `use std::env;` (no alias, no glob) must resolve a later qualified \
         `env::var(...)` call, not silently miss it: {sites:#?}"
    );

    // `use std::env::var;` (bare direct-function import: no alias, no
    // braces) followed by a BARE `var(...)` call — the same
    // whole-leaf-reused-as-bound-name bug, on the function-import arm
    // instead of the module arm.
    let fn_import_sample = r#"
        use std::env::var;

        fn plain_fn_import_helper() -> Option<String> {
            var("PLANTED_PLAIN_FN_IMPORT").ok()
        }
    "#;
    let sites: BTreeSet<EnvReadSite> = environment_read_sites("fn_import.rs", fn_import_sample)
        .into_iter()
        .collect();
    assert_eq!(
        sites,
        BTreeSet::from([EnvReadSite {
            file: "fn_import.rs".to_string(),
            function: "plain_fn_import_helper".to_string(),
            variable: "PLANTED_PLAIN_FN_IMPORT".to_string(),
            occurrence: 1,
        }]),
        "a plain `use std::env::var;` (no alias, no braces) must resolve a later bare \
         `var(...)` call, not silently miss it: {sites:#?}"
    );
}

#[test]
fn detector_numbers_repeated_reads_of_the_same_variable_as_distinct_sites() {
    let sample = r#"
        fn reads_the_same_variable_twice() -> (Option<String>, Option<String>) {
            let first = std::env::var("PLANTED_REPEATED_READ").ok();
            let second = std::env::var("PLANTED_REPEATED_READ").ok();
            (first, second)
        }
    "#;
    let sites: BTreeSet<EnvReadSite> = environment_read_sites("repeated.rs", sample)
        .into_iter()
        .collect();
    assert_eq!(
        sites,
        BTreeSet::from([
            EnvReadSite {
                file: "repeated.rs".to_string(),
                function: "reads_the_same_variable_twice".to_string(),
                variable: "PLANTED_REPEATED_READ".to_string(),
                occurrence: 1,
            },
            EnvReadSite {
                file: "repeated.rs".to_string(),
                function: "reads_the_same_variable_twice".to_string(),
                variable: "PLANTED_REPEATED_READ".to_string(),
                occurrence: 2,
            },
        ]),
        "two separate reads of the SAME variable in the SAME function must be two distinct \
         sites (occurrence 1 and occurrence 2), not one entry that silently absorbs the second \
         read for free: {sites:#?}"
    );
}

#[test]
fn baseline_parses_into_the_expected_count() {
    let baseline = load_baseline();
    // A CEILING, not an equality pin: the checked-in baseline must parse
    // into at most the frozen starting count. Pinning this to exact
    // equality would block the very terminal state the subset guard below
    // exists to allow — a baseline burned all the way down to empty (every
    // real site migrated to a declared setting) is a strict SUBSET of the
    // starting count, not equal to it, and must still pass.
    assert!(
        baseline.len() <= BASELINE_STARTING_SITE_COUNT,
        "the checked-in baseline now lists {} site(s), above the frozen starting ceiling of \
         BASELINE_STARTING_SITE_COUNT ({BASELINE_STARTING_SITE_COUNT}); the baseline may only \
         shrink as reads are migrated to declared settings — including all the way to empty — \
         never grow to make room for a new one, and never re-pinned to the starting value",
        baseline.len()
    );

    // Canary for the subset guard against `ORIGINAL_ENVIRONMENT_READ_SITES`
    // (asserted in `no_new_environment_read_sites_and_baseline_only_shrinks`),
    // exercised directly on parsed baseline text — no real files touched.
    // Both outcomes matter equally: a coordinated swap must go RED, and a
    // legitimate burn-down must stay GREEN, because a fix that also broke
    // burn-down would defeat the very migration this baseline exists to
    // allow.
    let authorized = authorized_environment_read_sites();
    // Both frozen lists together — the pool the RED/GREEN scenarios below
    // draw from, so the planted counts line up with the DERIVED
    // BASELINE_STARTING_SITE_COUNT (original.len() + amended.len()) rather
    // than assuming the original list alone accounts for the whole ceiling.
    let combined: Vec<(&str, &str, &str)> = ORIGINAL_ENVIRONMENT_READ_SITES
        .iter()
        .chain(AMENDED_ENVIRONMENT_READ_SITES.iter())
        .copied()
        .collect();

    // RED: a coordinated one-for-one swap — a site that was never in the
    // authorized membership, paired with removing a real authorized site,
    // so the total COUNT stays exactly at today's baseline size (the shape
    // a hidden knob would actually take, since a swap that changed the
    // count would already be caught by the ceiling check).
    let mut swapped_text = String::new();
    for (index, (file, function, variable)) in combined.iter().enumerate() {
        if index == 0 {
            // Drop the very first authorized site...
            continue;
        }
        swapped_text.push_str(&format!("{file}|{function}|{variable}\n"));
    }
    // ...and add one that was never authorized in its place.
    swapped_text.push_str("vigil/config.rs|env_overrides|VIGIL_TEST_ESTATE_PLANTED_HIDDEN_KNOB\n");
    let swapped_baseline = parse_baseline(&swapped_text);
    assert_eq!(
        swapped_baseline.len(),
        BASELINE_STARTING_SITE_COUNT,
        "sanity: the planted swap must keep the same total count a coordinated edit would (this \
         is exactly the count-preserving shape the subset guard exists to still catch)"
    );
    let smuggled: Vec<&EnvReadSite> = swapped_baseline.difference(&authorized).collect();
    assert_eq!(
        smuggled.len(),
        1,
        "a coordinated swap (one non-authorized site added, one authorized site removed, count \
         unchanged) must be caught by the subset check even though the count matches: \
         {smuggled:#?}"
    );
    assert_eq!(
        smuggled[0].variable, "VIGIL_TEST_ESTATE_PLANTED_HIDDEN_KNOB",
        "got {smuggled:#?}"
    );

    // GREEN: a legitimate burn-down — remove one authorized site, add
    // nothing back — must still pass the subset check (it stays a SUBSET
    // of the authorized membership), because the subset guard must never
    // block the migration this baseline exists to allow.
    let mut burned_down_text = String::new();
    for (index, (file, function, variable)) in combined.iter().enumerate() {
        if index == 0 {
            continue;
        }
        burned_down_text.push_str(&format!("{file}|{function}|{variable}\n"));
    }
    let burned_down_baseline = parse_baseline(&burned_down_text);
    assert_eq!(
        burned_down_baseline.len(),
        BASELINE_STARTING_SITE_COUNT - 1,
        "sanity: the planted burn-down must remove exactly one site"
    );
    let burned_down_smuggled: Vec<&EnvReadSite> =
        burned_down_baseline.difference(&authorized).collect();
    assert!(
        burned_down_smuggled.is_empty(),
        "a legitimate burn-down (one authorized site removed, nothing added) must stay a subset \
         of the authorized membership and pass the subset check cleanly: {burned_down_smuggled:#?}"
    );

    // GREEN, the TERMINAL case: the baseline burned all the way down to
    // EMPTY — every real site migrated to a declared setting, the
    // successful end state this whole baseline exists to reach. The empty
    // set is trivially a subset of anything, so the subset check must stay
    // silent; the positive control (independent of baseline content
    // entirely) must still catch a real read, proving this is a genuinely
    // clean terminal state rather than a broken scanner that would also
    // report nothing.
    let fully_burned_down_baseline = parse_baseline("");
    assert!(
        fully_burned_down_baseline.is_empty(),
        "sanity: the planted terminal burn-down must leave zero sites"
    );
    let fully_burned_down_smuggled: Vec<&EnvReadSite> =
        fully_burned_down_baseline.difference(&authorized).collect();
    assert!(
        fully_burned_down_smuggled.is_empty(),
        "an empty baseline (full migration complete) must stay a subset of the authorized \
         membership and pass the subset check cleanly: {fully_burned_down_smuggled:#?}"
    );
    let positive_control_sample = r#"
        fn __test_estate_environment_scan_positive_control() -> Option<String> {
            std::env::var("VIGIL_TEST_ESTATE_POSITIVE_CONTROL_SITE").ok()
        }
    "#;
    let positive_control_sites =
        environment_read_sites("positive-control.rs", positive_control_sample);
    assert_eq!(
        positive_control_sites.len(),
        1,
        "the positive control must still catch a real read against a fully burned-down (empty) \
         baseline — proving an empty baseline is a genuinely clean terminal state, not a broken \
         scanner: {positive_control_sites:#?}"
    );
}
