// Shared comment/string-literal lexing for environment_read_surface.rs,
// cli_secret_flag_surface.rs, and settings_registry_declare_boundary.rs.
//
// Included via:
//   #[path = "source_scan_lexer.rs"]
//   mod source_scan_lexer;
//
// All three guards scan Rust source text and all need the same thing
// first: line comments, block comments (nested), and string/char literals
// correctly recognized so that (a) a mention inside a comment or a
// commented-out literal is not mistaken for live code, and (b) format
// strings like `"{error}"` do not corrupt any brace-depth analysis a
// consumer layers on top. One lexer, one place it can go wrong. The tiny
// identifier/whitespace primitives below are shared for the same reason —
// each is a one-line rule two independent copies could silently drift on.
//
// The crate-scope walk at the bottom of this file (`PRODUCTION_CRATES`,
// `crates_root`, `rust_sources`) is shared for the identical reason: a scan
// that walks its own copy of the production-crate list is exactly how one
// scan later covers a crate its sibling misses — two divergent lists is the
// blind spot a crate-scope guard exists to close. `environment_read_surface.rs`
// and `cli_secret_flag_surface.rs` both call it; there is one list, one
// walker, one place to add the next crate.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

// `unicode-ident` is the exact crate rustc's own lexer uses to decide
// XID_Start/XID_Continue. `is_ident_start`/`is_ident_char` below build on it
// rather than on `char::is_alphabetic`/`char::is_alphanumeric` so that
// identifier acceptance here is DEFINITIONALLY the compiler's own grammar —
// not a hand-rolled approximation that happens to agree on common cases and
// silently diverges on ones nobody thought to check. A real, verified
// divergence: `char::is_alphanumeric` is false for a combining diacritical
// mark like U+0301 COMBINING ACUTE ACCENT (Unicode category Mark, not
// Letter or Number), but `unicode_ident::is_xid_continue('\u{0301}')` is
// true, and `rustc 1.94.0` genuinely accepts a two-character identifier
// consisting of the letter `a` followed by the raw U+0301 bytes (verified by
// compiling one; there is no `\u{}` escape in identifier syntax, only in a
// char/string literal, so that spelling is not itself compilable Rust) — so
// the old, hand-rolled predicate
// silently truncated that identifier one character early, exactly the kind
// of gap this crate exists to close by construction rather than by
// enumeration.
use unicode_ident::{is_xid_continue, is_xid_start};

/// The result of lexing one file's source text once: `masked` is the
/// original text with every comment and string/char literal body replaced
/// by spaces (newlines preserved) so a consumer can safely count braces or
/// scan for keywords without tripping over literal contents; `strings` is
/// every string literal found, as `(start_char_index, end_char_index,
/// decoded_content)`, in source order. `masked` and the original text share
/// the same char-index space, so a position found in one is valid in the
/// other.
pub struct Lexed {
    pub masked: Vec<char>,
    pub strings: Vec<(usize, usize, String)>,
}

pub fn lex(source: &str) -> Lexed {
    let chars: Vec<char> = source.chars().collect();
    let mut masked = chars.clone();
    let mut strings = Vec::new();
    let n = chars.len();
    let mut i = 0usize;
    while i < n {
        let c = chars[i];
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            let start = i;
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            for slot in masked.iter_mut().take(i).skip(start) {
                *slot = ' ';
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let start = i;
            i += 2;
            let mut depth = 1i32;
            while i < n && depth > 0 {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            for slot in masked.iter_mut().take(i).skip(start) {
                if *slot != '\n' {
                    *slot = ' ';
                }
            }
            continue;
        }
        if let Some((end, is_string, content)) = try_lex_string_or_char(&chars, i) {
            if is_string {
                strings.push((i, end, content));
            }
            for slot in masked.iter_mut().take(end).skip(i) {
                if *slot != '\n' {
                    *slot = ' ';
                }
            }
            i = end;
            continue;
        }
        i += 1;
    }
    Lexed { masked, strings }
}

/// Every string literal's decoded content, in source order, with comments
/// and commented-out code excluded. A thin convenience over [`lex`] for a
/// consumer that only needs literal text, not brace-safe masked source.
pub fn string_literals(source: &str) -> Vec<String> {
    lex(source)
        .strings
        .into_iter()
        .map(|(_, _, content)| content)
        .collect()
}

/// Whether `c` can start a Rust identifier — `_` (which is `XID_Continue`
/// but deliberately NOT `XID_Start` in Unicode's own identifier profile;
/// Rust's own grammar is `XID_Start XID_Continue* | _ XID_Continue+`, so
/// the leading-underscore form is handled as an explicit second case here,
/// not folded into `is_xid_start`) or any `unicode_ident::is_xid_start`
/// character — which is every character `rustc` itself accepts to start an
/// identifier, ASCII or not.
pub fn is_ident_start(c: char) -> bool {
    c == '_' || is_xid_start(c)
}

/// Whether `c` can continue a Rust identifier (after its first character) —
/// `unicode_ident::is_xid_continue`, which already includes `_` (Unicode's
/// own `XID_Continue` derived property folds `LOW LINE` in), so no separate
/// underscore case is needed here the way there is in [`is_ident_start`].
pub fn is_ident_char(c: char) -> bool {
    is_xid_continue(c)
}

/// Advance `pos` past any run of whitespace.
pub fn skip_ws_forward(chars: &[char], mut pos: usize) -> usize {
    while pos < chars.len() && chars[pos].is_whitespace() {
        pos += 1;
    }
    pos
}

/// Retreat `pos` past any run of whitespace immediately before it.
pub fn skip_ws_backward(chars: &[char], mut pos: usize) -> usize {
    while pos > 0 && chars[pos - 1].is_whitespace() {
        pos -= 1;
    }
    pos
}

/// The identifier starting at `pos`, and the position right after it, or
/// `None` if `pos` is not the start of an identifier.
///
/// Recognizes a raw identifier (`r#Name`) the same way rustc's own lexer
/// does: `r#Name` tokenizes as a SINGLE identifier whose *symbol* is
/// `Name` — the `r#` marker is not part of the symbol, which is exactly
/// why `type Alias = Foo; type r#Alias = Bar;` is rejected as "the name
/// `Alias` is defined multiple times" (E0428, verified against `rustc
/// 1.94.0`) rather than treated as two distinct names — but whose *span*
/// covers the full `r#Name` text. This reader returns the same pair: the
/// STRIPPED symbol text (so every consumer that compares identifier text,
/// e.g. against `TARGET_TYPE` or a forbidden method name, sees `r#Name`
/// and `Name` as identical, matching Rust's own name resolution) and an
/// end position past the FULL token, `r#` included, never past just the
/// leading `r`. Without this, `r#` reads as a bare one-character
/// identifier `"r"` and the rest is silently dropped — every scan built on
/// this reader would then miss whatever comes after the marker.
///
/// The lookahead that decides this is a raw identifier and not something
/// else (`r` immediately followed by `#`, immediately followed by another
/// identifier-start character) cannot collide with a raw string or raw
/// byte-string literal (`r"..."`, `r#"..."#`, `br#"..."#`, ...): those are
/// recognized and masked to spaces by [`lex`] before this function ever
/// runs (a raw string's `#` run is always followed by a `"`, never by an
/// identifier-start character), so any `r#` surviving into `masked` is
/// unambiguously a raw identifier.
pub fn read_ident_forward(chars: &[char], pos: usize) -> Option<(String, usize)> {
    if pos >= chars.len() || !is_ident_start(chars[pos]) {
        return None;
    }
    if chars[pos] == 'r'
        && chars.get(pos + 1) == Some(&'#')
        && chars.get(pos + 2).is_some_and(|c| is_ident_start(*c))
    {
        let mut end = pos + 2;
        while end < chars.len() && is_ident_char(chars[end]) {
            end += 1;
        }
        return Some((chars[pos + 2..end].iter().collect(), end));
    }
    let mut end = pos;
    while end < chars.len() && is_ident_char(chars[end]) {
        end += 1;
    }
    Some((chars[pos..end].iter().collect(), end))
}

/// The identifier ending right at `end`, and its start position, or `None`
/// if there is no identifier character immediately before `end`.
///
/// Mirrors [`read_ident_forward`]'s raw-identifier handling exactly, in
/// the direction backward search needs it: if the identifier run found
/// ending at `end` is itself immediately preceded by a raw-identifier
/// marker (`r#`), the returned start position is extended to cover the
/// full `r#Name` token (so a caller doing a further positional check —
/// "what comes right before this identifier" — sees the position before
/// the `r`, not before the `#`, matching rustc's own span), while the
/// returned string stays the STRIPPED, canonical name — so a caller
/// scanning backward from `r#Name` gets the identical name text a caller
/// scanning backward from plain `Name` would.
pub fn read_ident_backward(chars: &[char], end: usize) -> Option<(usize, String)> {
    let mut name_start = end;
    while name_start > 0 && is_ident_char(chars[name_start - 1]) {
        name_start -= 1;
    }
    if name_start == end {
        return None;
    }
    let name: String = chars[name_start..end].iter().collect();
    let token_start =
        if name_start >= 2 && chars[name_start - 1] == '#' && chars[name_start - 2] == 'r' {
            name_start - 2
        } else {
            name_start
        };
    Some((token_start, name))
}

/// The position of the delimiter matching `open_ch` at `open_pos` (which
/// must itself be `open_ch`), tracking nesting depth of that pair only.
pub fn match_delim(
    masked: &[char],
    open_pos: usize,
    open_ch: char,
    close_ch: char,
) -> Option<usize> {
    let mut depth = 0i32;
    let mut idx = open_pos;
    while idx < masked.len() {
        if masked[idx] == open_ch {
            depth += 1;
        } else if masked[idx] == close_ch {
            depth -= 1;
            if depth == 0 {
                return Some(idx);
            }
        }
        idx += 1;
    }
    None
}

/// The position of the `)` matching the `(` at `open_pos`.
pub fn match_paren(masked: &[char], open_pos: usize) -> Option<usize> {
    match_delim(masked, open_pos, '(', ')')
}

/// The position of the `]` matching the `[` at `open_pos`.
pub fn match_bracket(masked: &[char], open_pos: usize) -> Option<usize> {
    match_delim(masked, open_pos, '[', ']')
}

/// Every `#[cfg(test)]`-attributed item's body range (the attributed item's
/// own `{`..`}`, whatever kind of item it is — `mod`, `fn`, `impl`, ...).
/// Shared by scans that need to exclude test-only scaffolding from a
/// production-surface inventory. Only the exact `#[cfg(test)]` spelling is
/// recognized — the whole crate's test modules use it consistently — so
/// something conditioned more elaborately (`cfg(any(test, feature =
/// "x"))`, ...) is deliberately out of scope for this narrow exclusion.
pub fn collect_cfg_test_ranges(masked: &[char]) -> Vec<(usize, usize)> {
    let marker: Vec<char> = "#[cfg(test)]".chars().collect();
    let n = masked.len();
    let mut ranges = Vec::new();
    let mut i = 0usize;
    while i + marker.len() <= n {
        if masked[i..i + marker.len()] == marker[..] {
            let mut k = i + marker.len();
            loop {
                k = skip_ws_forward(masked, k);
                if k < n
                    && masked[k] == '#'
                    && masked.get(k + 1) == Some(&'[')
                    && let Some(close) = match_bracket(masked, k + 1)
                {
                    k = close + 1;
                    continue;
                }
                break;
            }
            let mut depth = 0i32;
            let mut m = k;
            let mut found: Option<usize> = None;
            while m < n {
                match masked[m] {
                    '(' | '[' => depth += 1,
                    ')' | ']' => depth -= 1,
                    '{' if depth <= 0 => {
                        found = Some(m);
                        break;
                    }
                    ';' if depth <= 0 => break,
                    _ => {}
                }
                m += 1;
            }
            if let Some(open) = found {
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
                ranges.push((open, z));
                i = z;
                continue;
            }
            i = k;
            continue;
        }
        i += 1;
    }
    ranges
}

/// Whether `pos` falls inside any of `ranges` (each `[start, end)`).
pub fn in_any_range(ranges: &[(usize, usize)], pos: usize) -> bool {
    ranges.iter().any(|(s, e)| *s <= pos && pos < *e)
}

/// Recognizes `"..."`, `r"..."`, `r#"..."#` (any hash count), the `b`/`br`
/// byte-string variants, and `'x'` char literals (as opposed to `'a`
/// lifetimes, which are left alone). Returns `(end, is_string_not_char,
/// decoded_content)`.
fn try_lex_string_or_char(chars: &[char], i: usize) -> Option<(usize, bool, String)> {
    let n = chars.len();
    let mut j = i;
    if chars.get(j) == Some(&'b') && matches!(chars.get(j + 1), Some('"') | Some('r')) {
        j += 1;
    }
    if chars.get(j) == Some(&'r') && matches!(chars.get(j + 1), Some('"') | Some('#')) {
        let mut k = j + 1;
        let mut hashes = 0usize;
        while chars.get(k) == Some(&'#') {
            hashes += 1;
            k += 1;
        }
        if chars.get(k) == Some(&'"') {
            let content_start = k + 1;
            let mut m = content_start;
            while m < n {
                if chars[m] == '"' {
                    let closes = (0..hashes).all(|h| chars.get(m + 1 + h) == Some(&'#'));
                    if closes {
                        let content: String = chars[content_start..m].iter().collect();
                        return Some((m + 1 + hashes, true, content));
                    }
                }
                m += 1;
            }
            return Some((n, true, chars[content_start..n].iter().collect()));
        }
        return None;
    }
    if chars.get(j) == Some(&'"') {
        let content_start = j + 1;
        let mut m = content_start;
        let mut content = String::new();
        while m < n {
            match chars[m] {
                '\\' if m + 1 < n => {
                    let esc = chars[m + 1];
                    match esc {
                        'n' => content.push('\n'),
                        't' => content.push('\t'),
                        'r' => content.push('\r'),
                        '0' => content.push('\0'),
                        other => content.push(other),
                    }
                    m += 2;
                }
                '"' => return Some((m + 1, true, content)),
                other => {
                    content.push(other);
                    m += 1;
                }
            }
        }
        return Some((n, true, content));
    }
    if chars.get(i) == Some(&'\'') {
        if chars.get(i + 1) == Some(&'\\') {
            let mut m = i + 2;
            match chars.get(m) {
                Some('x') => m += 3,
                Some('u') => {
                    m += 1;
                    if chars.get(m) == Some(&'{') {
                        while m < n && chars.get(m) != Some(&'}') {
                            m += 1;
                        }
                        m += 1;
                    }
                }
                Some(_) => m += 1,
                None => {}
            }
            if chars.get(m) == Some(&'\'') {
                return Some((m + 1, false, String::new()));
            }
            return None;
        }
        if chars.get(i + 1).is_some() && chars.get(i + 2) == Some(&'\'') {
            return Some((i + 3, false, String::new()));
        }
        return None; // a lifetime, not a char literal
    }
    None
}

// ---------------------------------------------------------------------
// Shared crate-scope walk: every production crate a scan that inventories
// something across "all of Vigil" must cover, and the walker that finds
// its Rust sources.
// ---------------------------------------------------------------------

/// Every production crate whose `src/` participates in a crate-scope
/// source scan (an environment-read inventory, a secret-CLI-flag
/// inventory, ...). One list: a scan that walked its own copy could drift
/// from a sibling scan's copy, which is exactly the blind spot a
/// crate-scope guard exists to close.
pub const PRODUCTION_CRATES: &[&str] = &["vigil", "vigil-ha", "vigil-bin"];

/// The `crates/` directory shared by every entry in [`PRODUCTION_CRATES`],
/// found relative to the CALLING crate's own manifest directory (i.e.
/// `env!("CARGO_MANIFEST_DIR")` as resolved for whichever crate compiles
/// this file — `crates/vigil`, since every consumer of this shared module
/// is a `crates/vigil/tests/*.rs` integration test — never assumed to be
/// the process's current directory.
pub fn crates_root() -> PathBuf {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_root
        .parent()
        .unwrap_or_else(|| panic!("{} has no parent directory", crate_root.display()))
        .to_path_buf()
}

/// The workspace root, one level up from [`crates_root`] — for a scan that
/// needs a workspace-relative path (rather than one relative to `crates/`)
/// or a directory outside `crates/` entirely (`addons/`, `docs/`, the root
/// `Cargo.toml`). One place to compute it so a consumer never hand-rolls
/// its own `CARGO_MANIFEST_DIR`-plus-`.parent()` chain.
pub fn workspace_root() -> PathBuf {
    let crates_root = crates_root();
    crates_root
        .parent()
        .unwrap_or_else(|| panic!("{} has no parent directory", crates_root.display()))
        .to_path_buf()
}

/// The position of the `>` matching the `<` at `open_pos`, by simple
/// depth counting. Safe here specifically because callers only ever
/// invoke this immediately after confirming `masked[open_pos] == '<'` at
/// a position they are trying to read as the START of a qualified-path
/// expression (`<Type>::` / `<Type as Trait>::`) — not a general
/// angle-bracket matcher run over arbitrary code, where `<`/`>` also mean
/// less-than/greater-than and would desynchronize a blind count.
pub fn match_angle_bracket(masked: &[char], open_pos: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open_pos;
    while i < masked.len() {
        match masked[i] {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The position of the `<` matching the `>` at `close_pos`, by simple
/// BACKWARD depth counting — the mirror of [`match_angle_bracket`],
/// needed when a caller is scanning backward from a position that might
/// be the tail of a qualified-path expression and needs to find where it
/// opened, rather than scanning forward from a known `<`. The same
/// narrow safety condition applies: only call this immediately after
/// confirming `masked[close_pos] == '>'` at a position genuinely
/// suspected of closing a qualified path.
pub fn match_angle_bracket_backward(masked: &[char], close_pos: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = close_pos;
    loop {
        match masked[i] {
            '>' => depth += 1,
            '<' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

/// The start position of a top-level ` as ` keyword between `start` and
/// `end` (the `<Type as Trait>` qualified-path form), or `None` if the
/// span is a bare `<Type>`. Word-boundary safe: only ever tested at a
/// position immediately following a complete identifier read (or at
/// `start` itself), so it cannot land mid-identifier.
pub fn find_as_keyword(masked: &[char], start: usize, end: usize) -> Option<usize> {
    let mut pos = start;
    while pos < end {
        if is_ident_start(masked[pos]) {
            let (name, ident_end) = read_ident_forward(masked, pos)?;
            if name == "as" {
                return Some(pos);
            }
            pos = ident_end;
        } else {
            pos += 1;
        }
    }
    None
}

/// Whether `masked[pos..]` opens a Rust QUALIFIED-PATH expression naming
/// `token` as its own type. `<Type>` (bare) and `<Type as Trait>` are the
/// complete grammar for a qualified path — Rust has no third form — but
/// the two are not equally significant to a caller checking for a
/// specific evasion: `<Command>::new(...)` and
/// `<std::process::Command>::new(...)` reach the SAME inherent method a
/// bare `Command::new(...)` does, with no indirection at all — a clean,
/// pure-spelling evasion, DETERMINABLE FROM THE TOKEN STREAM ALONE (the
/// type name is written right there, just wrapped in `<...>`).
/// `<Command as SomeTrait>::new(...)` resolves to the TRAIT's `new`, not
/// `Command`'s own, so it is covered here only INCIDENTALLY, because
/// reading the last path segment before an optional ` as Trait` clause
/// is the same one operation either way — a caller relying on this
/// function to close a specific evasion should judge for itself whether
/// the `as Trait` form is in its own threat model or merely incidental
/// coverage (see `process_cleanup_scan.rs`'s own use of this function for
/// a worked example of that judgment).
///
/// Reads the LAST path segment before any ` as Trait` clause (or before
/// the closing `>` if there is none) via [`read_ident_backward`], so a
/// leading module path (`std::process::Command`) is tolerated the same
/// way a bare `Command::new(...)` call ignores nothing before its own
/// `Command`. Returns the position immediately after the closing `>` on
/// a match.
pub fn qualified_path_match(masked: &[char], pos: usize, token: &str) -> Option<usize> {
    if masked.get(pos) != Some(&'<') {
        return None;
    }
    let close = match_angle_bracket(masked, pos)?;
    let segment_end = match find_as_keyword(masked, pos + 1, close) {
        Some(as_start) => skip_ws_backward(masked, as_start),
        None => skip_ws_backward(masked, close),
    };
    let (_, name) = read_ident_backward(masked, segment_end)?;
    (name == token).then_some(close + 1)
}

/// The BACKWARD counterpart of [`qualified_path_match`]: whether
/// `masked[..=close_pos]` is the TAIL of a qualified-path expression
/// (`<Type>` / `<Type as Trait>`) naming `token`, given `close_pos` is
/// suspected of being that expression's own closing `>`. Used by a
/// caller scanning BACKWARD from a position right after some `::` (for
/// instance, reading the qualifier of `<Type>::method(...)` backward
/// from the `::`) that needs to recognize the SAME qualified-path
/// wrapping [`qualified_path_match`] recognizes moving forward, without
/// a second, independently-drifting implementation of the "read the last
/// segment before an optional ` as Trait` clause" logic. Returns the
/// position of the qualified path's own OPENING `<` on a match, so a
/// caller can continue whatever word-boundary or further-backward
/// reasoning it needs from there.
pub fn qualified_path_match_backward(
    masked: &[char],
    close_pos: usize,
    token: &str,
) -> Option<usize> {
    if masked.get(close_pos) != Some(&'>') {
        return None;
    }
    let open = match_angle_bracket_backward(masked, close_pos)?;
    qualified_path_match(masked, open, token).map(|_| open)
}

/// If `masked[pos..]` (after skipping insignificant whitespace) opens a
/// TURBOFISH — `::` followed by a balanced `<...>`, the empty `::<>` form
/// included — the position immediately after its closing `>`; otherwise
/// `pos` UNCHANGED (never whitespace-advanced), so a caller that finds no
/// turbofish here can still apply its own whitespace-skip from the exact
/// position it started at.
///
/// A turbofish is an explicit type-argument list on an otherwise ordinary
/// path segment or call — `Command::new::<&str>("cargo")`,
/// `command_output::<_, _>(...)`, `registry.declare::<u32>(spec)` — none
/// of which is a different function from its turbofish-free spelling, only
/// the same call with its type parameters written out where they would
/// otherwise be inferred. Confirmed to compile clean under `-D warnings`
/// (including the spaced form) on the pinned toolchain rather than
/// assumed. This is DETERMINABLE FROM THE TOKEN STREAM ALONE — the exact
/// same closeable class whitespace, raw identifiers, and a qualified path
/// already are — so a caller requiring a construct's own opening `(`
/// (or, for a boundary scan reading a call by its bare identifier, a `(`
/// immediately after the name) should look past an optional turbofish
/// here, the same way it already looks past insignificant whitespace, or
/// the tolerance closes nothing for a caller that still requires a bare
/// `(` right after the name. Reuses [`match_angle_bracket`] for the
/// balanced-bracket read rather than a second one.
pub fn skip_optional_turbofish(masked: &[char], pos: usize) -> usize {
    let after_ws = skip_ws_forward(masked, pos);
    if masked.get(after_ws) != Some(&':') || masked.get(after_ws + 1) != Some(&':') {
        return pos;
    }
    let after_colons = skip_ws_forward(masked, after_ws + 2);
    if masked.get(after_colons) != Some(&'<') {
        return pos;
    }
    match match_angle_bracket(masked, after_colons) {
        Some(close) => close + 1,
        None => pos,
    }
}

/// Every `.rs` file under `dir`, recursively, sorted.
pub fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}
