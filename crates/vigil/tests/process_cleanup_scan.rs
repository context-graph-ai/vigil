//! Every acceptance/integration test source file in this workspace must
//! avoid two hazards: unsafe process cleanup (a wildcard or shell-spawned
//! `kill`, rather than the one centralized, target-checked helper) and a
//! nested artifact build spawned from inside a test process (a real
//! build/test invocation, as opposed to a read-only `cargo metadata` or
//! `cargo tree` query). This scan enforces both, over every file under
//! `tests/` and every production crate's own `tests/` directory.
//!
//! Lives under `crates/vigil/tests/` — beside `environment_read_surface.rs`,
//! `cli_secret_flag_surface.rs`, and `transport_purity.rs`, which share the
//! same lexer and the same crate-scope walk — rather than inside the
//! `vigil-acceptance` crate's own support module where this scan used to
//! live: a source-scanning guard belongs in the file family every sibling
//! scan already sits in, protected by the same control-plane refusal
//! lists, not bundled into an unrelated test-support file those lists
//! never named.
//!
//! **Recognition, not just attribution, is fail-closed.** Every
//! `Command::new(X)` call anywhere in scanned sources is found — not just
//! ones already spelling a known cargo/docker marker — and `X` is
//! classified before anything downstream (subcommand, receiver) is even
//! considered:
//!
//! - a whole string literal, compared directly against the program names
//!   this scan cares about (`"cargo"`, the container runtime's name):
//!   provably not either one unless it matches, no allowance needed;
//! - the `env!("CARGO")` macro form, recognized as cargo;
//! - one of a small, REVIEWED set of benign non-literal expressions this
//!   file names explicitly, each with a one-line justification for why it
//!   cannot yield cargo or the container runtime (see
//!   [`BENIGN_COMMAND_NEW_EXPRESSIONS`]);
//! - the one reviewed generic pass-through helper shape (see
//!   [`is_reviewed_generic_passthrough`]);
//! - anything else — a variable, a function return, a computed value this
//!   scan cannot otherwise account for — is REFUSED outright, before any
//!   subcommand is ever read. A `let program = "cargo";
//!   Command::new(program)` is not "unattributable, therefore banned"; it
//!   is now "unrecognized, therefore refused" at the SAME layer, which is
//!   the fix — recognizing only known SPELLINGS of cargo/docker and
//!   silently letting everything else through was the same
//!   one-scope-too-shallow mistake the subcommand-attribution fix already
//!   closed one layer up.
//!
//! **This is fail-closed with a reviewed benign set, not a definitional
//! guarantee.** A text scan cannot resolve what a variable's value
//! actually is. The residual this leaves, stated plainly rather than
//! implied away: a binding given a name that matches one of the reviewed
//! benign expressions below, but assigned the cargo or container-runtime
//! program instead, would evade this scan. Closing that would require
//! deliberately misleading naming at the point the variable is bound —
//! visible to a reader of that same file in review, not a silent bypass
//! this scan could ever be expected to catch on its own.
//!
//! Only ONCE a `Command::new(X)` (or a same-call-argument helper's own
//! program argument — see [`same_call_argument_violations`]) is
//! recognized as cargo or the container runtime does the receiver-
//! attributed subcommand resolution below run: the subcommand is read
//! from the SPECIFIC receiver's own chained `.arg`/`.args` call, or from
//! the identifier that receiver was bound to if it was assigned to a
//! variable first — never the first `.arg`/`.args` call that merely
//! appears textually nearby, which could belong to a completely different
//! builder. See [`resolve_command_new_subcommand`]'s own doc comment for
//! the two attributed shapes.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::{
    PRODUCTION_CRATES, is_ident_char, is_ident_start, lex, match_paren, qualified_path_match,
    read_ident_backward, read_ident_forward, rust_sources, skip_optional_turbofish,
    skip_ws_backward, workspace_root,
};

/// The reviewed set of non-literal `Command::new(X)` program expressions
/// this scan accepts as benign without further scrutiny, each with a
/// one-line justification for why `X` cannot yield cargo or the container
/// runtime. Matched against the EXACT trimmed argument text — not a
/// substring, not a name pattern — so adding an entry is a deliberate,
/// reviewed, one-line-at-a-time act. Measured against the real corpus
/// this scan walks (`tests/` plus every production crate's own `tests/`):
/// widening this list is how a genuinely new benign fixture-tool
/// invocation gets added; anything not listed here (and not one of the
/// two recognized cargo/docker forms) is refused by default.
const BENIGN_COMMAND_NEW_EXPRESSIONS: &[(&str, &str)] = &[
    (
        "vigil_binary_path()",
        "the resolved path to the vigil product binary under test",
    ),
    (
        "deterministic_fixture_support::vigil_binary_path()",
        "the same resolved vigil product binary path, fully qualified from another crate's test module",
    ),
    (
        "&self.path",
        "VigilBinary's own resolved product-binary path field, set once via resolve_vigil_binary",
    ),
    (
        "&oracle",
        "the resolved yolox-burn-oracle detector fixture binary path",
    ),
    ("ffmpeg", "the resolved ffmpeg media-fixture binary path"),
    (
        "&ffmpeg_bin",
        "the resolved ffmpeg media-fixture binary path, held in a fixture struct/local",
    ),
    ("ffprobe", "the resolved ffprobe media-fixture binary path"),
    (
        "mediamtx",
        "the resolved mediamtx media-server fixture binary path",
    ),
    (
        "&mediamtx_bin",
        "the resolved mediamtx media-server fixture binary path, held in a fixture struct/local",
    ),
    (
        "strace",
        "the resolved strace syscall-tracer fixture binary path",
    ),
    (
        "&mosquitto_bin",
        "the resolved mosquitto broker fixture binary path",
    ),
    (
        "&lock_holder_binary",
        "this test binary's own resolved path, re-executed as the settings-store lock holder so the \
         holder pid the classification reports can only come from real cross-process lock ownership",
    ),
];

/// Whether the exact token `token` occurs at `masked[pos..]` — matched
/// against the MASKED text, never raw source, so a token sitting inside a
/// comment or a string literal (both blanked to spaces there) can never
/// spell it and is naturally excluded, with no separate liveness check
/// needed. When `token` is identifier-shaped (starts with an XID_Start
/// character or `_`), a WORD-BOUNDARY check on both sides guards against
/// matching a substring of a longer identifier — `Command` must not match
/// inside `MyCommand` or `Commander`. Punctuation tokens (`::`, `!`, `(`,
/// `)`) need no such check.
/// identifier-shaped tokens (starting with an XID_Start character or
/// `_`) are read through the shared [`read_ident_forward`] reader — the
/// SAME reader every other identifier read in this file already goes
/// through — comparing its returned SYMBOL against `token`, rather than
/// a literal character compare. This is what lets a raw identifier
/// (`r#new`, `r#kill`, ...) match: `read_ident_forward` recognizes
/// `r#Name` exactly the way rustc does, returning the stripped symbol
/// `Name` and the position past the FULL `r#Name` token, so `r#kill` and
/// `kill` compare equal here the same way they name the identical item
/// to the compiler. A literal character compare cannot see through `r#`
/// at all and would treat `r#kill` as unrelated text — the same class of
/// evasion the whitespace tolerance already closes: the real invariant is
/// "no spelling variant the compiler treats as identical can evade,"
/// whichever token-stream-visible form (whitespace, a raw identifier, or
/// anything else determinable from the tokens alone) it takes. A leading
/// `is_ident_char` check on `pos - 1`
/// guards the one thing `read_ident_forward`'s own maximal-munch read
/// cannot: that `pos` is a true identifier BOUNDARY, not partway through
/// a longer one (`kill` must not match at the `kill` inside `unkill`).
/// Punctuation tokens (`::`, `!`, `(`, `)`) have no raw form and are
/// compared as literal characters. Returns the position immediately
/// after the match, since a raw identifier's own consumed length differs
/// from `token`'s.
fn matches_token_at(masked: &[char], pos: usize, token: &str) -> Option<usize> {
    if token.chars().next().is_some_and(is_ident_start) {
        if pos > 0 && is_ident_char(masked[pos - 1]) {
            return None;
        }
        let (name, end) = read_ident_forward(masked, pos)?;
        (name == token).then_some(end)
    } else {
        let token_chars: Vec<char> = token.chars().collect();
        let end = pos + token_chars.len();
        (end <= masked.len() && masked[pos..end] == token_chars[..]).then_some(end)
    }
}

/// Every position where the exact TOKEN SEQUENCE `tokens` occurs in live
/// code, tolerating insignificant whitespace (which already includes
/// masked-out comments) between EVERY pair of adjacent tokens — the same
/// tolerance every resolver in this file already applies around `.`,
/// `(`, and array/string arguments, now pushed one layer further out to
/// marker RECOGNITION itself. Returns each match's `(sequence_start,
/// sequence_end)`: the position of the first token's own first
/// character, and the position immediately after the last token's own
/// last character.
///
/// This closes a real gap a plain contiguous substring search left open:
/// `Command :: new ( tool )` is token-identical to `Command::new(tool)`
/// under Rust's own grammar — insignificant whitespace between tokens —
/// so a bare `"Command::new("` substring search never finds it at all;
/// the call is never even reached, let alone classified or refused. No
/// formatter emits spaced tokens and no ordinary author writes them, but
/// this is an anti-evasion guard: the bar is whether a determined author
/// can slip a nested build past it, and spacing tokens to do so is
/// trivial, so accidental-likelihood is not the weight that matters here.
///
/// The FIRST token additionally accepts the qualified-path spelling — see
/// [`qualified_path_match`] — measured directly against the pinned
/// toolchain (`<Command>::new("cargo")` compiles under 1.94.0) rather than
/// assumed. And whenever the token about to be matched is `(`, an optional
/// TURBOFISH is consumed first — see [`skip_optional_turbofish`] —
/// likewise measured against the pinned toolchain
/// (`Command::new::<&str>("cargo")` compiles clean under `-D warnings`)
/// rather than assumed: an explicit type-argument list on the call is the
/// same call, spelled with its type parameters written out.
///
/// The CRITERION marker recognition here is held to, stated once so it
/// never needs restating as a number: every spelling of the construct
/// DETERMINABLE FROM THE TOKEN STREAM ALONE — the construct's own name is
/// written right there in the source, just formatted differently
/// (insignificant whitespace, a raw identifier, a qualified path, a
/// turbofish, or any future spelling variant meeting the same
/// token-stream-determinable bar) — is recognized. A newly found
/// token-determinable spelling that slips past this function is a BUG
/// against that criterion, to be fixed here, not a count to bump.
///
/// It does NOT resolve, and cannot ever resolve, ALIASING OR INDIRECTION
/// THAT RENAMES THE CONSTRUCT — a `use ... as` alias, a `type` alias, a
/// function-pointer binding, a re-export, or any other name a later
/// reference could stand for the construct under — NOR a macro-generated
/// invocation of it. Both require resolving a NAME to its DEFINITION,
/// which is a name-resolution problem, not a spelling one, and a text scan
/// has no way to perform it. This is the same residual class the
/// benign-expression list's misleading-naming note already carries, and it
/// stays a documented, unbounded residual rather than something this
/// function silently claims to handle.
fn find_live_token_sequence(masked: &[char], tokens: &[&str]) -> Vec<(usize, usize)> {
    let Some(&first) = tokens.first() else {
        return Vec::new();
    };
    let no_string_exceptions = BTreeSet::new();
    let mut matches = Vec::new();
    let mut i = 0usize;
    while i < masked.len() {
        let first_match =
            matches_token_at(masked, i, first).or_else(|| qualified_path_match(masked, i, first));
        if let Some(mut pos) = first_match {
            let mut matched = true;
            for token in &tokens[1..] {
                pos = skip_insignificant(masked, &no_string_exceptions, pos);
                if *token == "(" {
                    pos = skip_insignificant(
                        masked,
                        &no_string_exceptions,
                        skip_optional_turbofish(masked, pos),
                    );
                }
                match matches_token_at(masked, pos, token) {
                    Some(next) => pos = next,
                    None => {
                        matched = false;
                        break;
                    }
                }
            }
            if matched {
                matches.push((i, pos));
            }
        }
        i += 1;
    }
    matches
}

/// Every string literal's own start char-index, for the recurring
/// "skip whitespace, but never walk INTO a masked string body" guard
/// every resolver below shares (see [`skip_insignificant`]).
fn string_start_positions(strings: &[(usize, usize, String)]) -> BTreeSet<usize> {
    strings.iter().map(|(start, _, _)| *start).collect()
}

/// Advances `cursor` past masked whitespace, EXCEPT never past a position
/// that is itself the start of a real string literal. A masked string
/// body (its quotes included) is blanked to spaces by the shared lexer,
/// so it reads as whitespace to a naive `char::is_whitespace` skip —
/// which would walk straight past the very string a resolver needs to
/// land ON, out the far side, to whatever comes after it.
fn skip_insignificant(
    masked: &[char],
    string_starts: &BTreeSet<usize>,
    mut cursor: usize,
) -> usize {
    while cursor < masked.len()
        && masked[cursor].is_whitespace()
        && !string_starts.contains(&cursor)
    {
        cursor += 1;
    }
    cursor
}

/// The bounds of the substring of `raw` between `[start, end)` with
/// leading/trailing whitespace trimmed off — used to compare a call's own
/// argument text exactly against a reviewed expression, independent of
/// incidental formatting.
fn trim_char_range(raw: &[char], mut start: usize, mut end: usize) -> (usize, usize) {
    while start < end && raw[start].is_whitespace() {
        start += 1;
    }
    while end > start && raw[end - 1].is_whitespace() {
        end -= 1;
    }
    (start, end)
}

/// If `[start, end)` (already trimmed) is EXACTLY one string literal —
/// its own recorded span matches the range precisely, not merely
/// contained within it — that literal's decoded content; `None`
/// otherwise (a non-literal expression, or a literal that is only PART of
/// a larger expression, e.g. `foo("x")`).
fn whole_argument_literal_content(
    strings: &[(usize, usize, String)],
    start: usize,
    end: usize,
) -> Option<String> {
    strings
        .iter()
        .find(|(s, e, _)| *s == start && *e == end)
        .map(|(_, _, content)| content.clone())
}

/// Whether the argument span `[start, end)` is token-identical to
/// `env!("CARGO")` — the macro invocation that names the cargo binary
/// running this test process itself — tolerating insignificant
/// whitespace between EVERY token, including around the string literal
/// (`env ! ( "CARGO" )`), and requiring the match to consume the WHOLE
/// span exactly (no trailing content). A plain text compare against the
/// fixed spelling `"env!(\"CARGO\")"` would miss this same token-spaced
/// form, exactly the marker-recognition gap [`find_live_token_sequence`]
/// closes for `Command::new(`.
fn is_env_cargo_argument(
    masked: &[char],
    strings: &[(usize, usize, String)],
    string_starts: &BTreeSet<usize>,
    start: usize,
    end: usize,
) -> bool {
    let Some(mut pos) = matches_token_at(masked, start, "env") else {
        return false;
    };
    pos = skip_insignificant(masked, string_starts, pos);
    let Some(next) = matches_token_at(masked, pos, "!") else {
        return false;
    };
    pos = skip_insignificant(masked, string_starts, next);
    let Some(next) = matches_token_at(masked, pos, "(") else {
        return false;
    };
    // The string's OWN recorded start is its opening quote, which the
    // masker blanks along with the rest of its body — indistinguishable
    // from whitespace to a naive skip. `string_starts` is the same
    // "never skip past a string's own start" exception every other
    // resolver in this file already relies on for exactly this reason.
    pos = skip_insignificant(masked, string_starts, next);
    let Some((_, string_end, content)) = strings.iter().find(|(s, _, _)| *s == pos) else {
        return false;
    };
    if content != "CARGO" {
        return false;
    }
    pos = skip_insignificant(masked, string_starts, *string_end);
    let Some(next) = matches_token_at(masked, pos, ")") else {
        return false;
    };
    next == end
}

/// A small chained-token match starting EXACTLY at `pos` — not a search
/// like [`find_live_token_sequence`], a fixed sequence anchored at the
/// position the caller already landed on. Each token in `tokens` must
/// match in order, tolerating insignificant whitespace between them.
/// `None` if any token fails to match; otherwise the position immediately
/// after the last token.
fn match_token_chain(
    masked: &[char],
    string_starts: &BTreeSet<usize>,
    mut pos: usize,
    tokens: &[&str],
) -> Option<usize> {
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 {
            pos = skip_insignificant(masked, string_starts, pos);
        }
        pos = matches_token_at(masked, pos, token)?;
    }
    Some(pos)
}

/// Whether the expression starting at `pos` (right after a `let <name> =`)
/// is `std::env::var("CARGO")` or `env::var("CARGO")` followed by
/// `.unwrap_or_else(<closure>)` whose closure's own body carries EXACTLY
/// one string literal, and it reads `"cargo"` — nothing else, so a
/// fallback that named some other program could never pass this. Proves
/// BOTH arms of the binding resolve to cargo: the `Ok` arm reads the
/// `CARGO` env var cargo's own test harness sets to the cargo binary's
/// path for every test process it runs; the `Err` arm's fallback is the
/// literal `"cargo"`, walked by `$PATH`.
fn binding_is_cargo_env_lookup(
    masked: &[char],
    strings: &[(usize, usize, String)],
    string_starts: &BTreeSet<usize>,
    pos: usize,
) -> bool {
    let mut cursor = skip_insignificant(masked, string_starts, pos);
    let mut matched_prefix = false;
    for prefix in [
        ["std", "::", "env", "::", "var", "("].as_slice(),
        ["env", "::", "var", "("].as_slice(),
    ] {
        if let Some(next) = match_token_chain(masked, string_starts, cursor, prefix) {
            cursor = next;
            matched_prefix = true;
            break;
        }
    }
    if !matched_prefix {
        return false;
    }
    cursor = skip_insignificant(masked, string_starts, cursor);
    let Some((_, string_end, content)) = strings.iter().find(|(s, _, _)| *s == cursor) else {
        return false;
    };
    if content != "CARGO" {
        return false;
    }
    cursor = skip_insignificant(masked, string_starts, *string_end);
    let Some(next) = matches_token_at(masked, cursor, ")") else {
        return false;
    };
    cursor = skip_insignificant(masked, string_starts, next);
    let Some(next) =
        match_token_chain(masked, string_starts, cursor, &[".", "unwrap_or_else", "("])
    else {
        return false;
    };
    let open_paren = next - 1;
    let Some(close_paren) = match_paren(masked, open_paren) else {
        return false;
    };
    let inner: Vec<&(usize, usize, String)> = strings
        .iter()
        .filter(|(s, e, _)| *s >= open_paren && *e <= close_paren)
        .collect();
    inner.len() == 1 && inner[0].2 == "cargo"
}

/// Whether the argument span `[start, end)` is a reference (`&<name>`) to
/// a local bound, earlier in the same file, to the RUNTIME analogue of
/// `env!("CARGO")`: `let <name> =
/// std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());` (see
/// [`binding_is_cargo_env_lookup`] for the exact shape required). Chosen
/// at runtime instead of compile time so a missing `CARGO` var degrades to
/// a `$PATH` lookup rather than a compile-time panic — the same tradeoff
/// `vigil_binary_path`'s own resolution makes, just for a different
/// binary. Every path through a binding matching that shape resolves to
/// cargo, so a `Command::new(&name)` reached through it is recognized as
/// cargo here — not filed as a benign non-cargo expression — and still
/// goes through the identical subcommand check every other cargo
/// invocation gets.
fn is_runtime_cargo_env_argument(
    masked: &[char],
    strings: &[(usize, usize, String)],
    string_starts: &BTreeSet<usize>,
    start: usize,
    end: usize,
) -> bool {
    let Some(after_amp) = matches_token_at(masked, start, "&") else {
        return false;
    };
    let pos = skip_insignificant(masked, string_starts, after_amp);
    let Some((ident, ident_end)) = read_ident_forward(masked, pos) else {
        return false;
    };
    if ident_end != end {
        return false;
    }

    for (_, after_eq) in find_live_token_sequence(masked, &["let", ident.as_str(), "="]) {
        if binding_is_cargo_env_lookup(masked, strings, string_starts, after_eq) {
            return true;
        }
    }
    false
}

/// Subcommands of `cargo` this scan treats as read-only queries rather
/// than a nested build/test invocation — `metadata` (parses the
/// manifest/lock graph) and `tree` (walks the already-resolved
/// dependency/feature graph via the same resolver `cargo build` itself
/// uses, and prints it — no compilation, no crate fetch beyond what the
/// existing `Cargo.lock` already pins). Anything else is refused.
fn is_readonly_cargo_subcommand(subcommand: Option<&str>) -> bool {
    matches!(subcommand, Some("metadata") | Some("tree"))
}

/// The literal subcommand `open_delim` (a `(` or `[`) opens onto: skip
/// insignificant whitespace, one optional leading `&`, one optional
/// leading `[` (for `.args(&["metadata", ...])` spellings), then require
/// the resulting position to land EXACTLY on a real string literal's
/// start — never scan past unrelated code to find "some string literal
/// eventually". `None` covers everything this cannot pin to an exact
/// position: a computed or conditional argument, an empty call, a
/// non-literal element.
fn resolve_literal_at_delimiter(
    masked: &[char],
    strings: &[(usize, usize, String)],
    string_starts: &BTreeSet<usize>,
    open_delim: usize,
) -> Option<String> {
    let mut cursor = skip_insignificant(masked, string_starts, open_delim + 1);
    if cursor < masked.len() && masked[cursor] == '&' && !string_starts.contains(&cursor) {
        cursor = skip_insignificant(masked, string_starts, cursor + 1);
    }
    if cursor < masked.len() && masked[cursor] == '[' && !string_starts.contains(&cursor) {
        cursor = skip_insignificant(masked, string_starts, cursor + 1);
    }
    strings
        .iter()
        .find(|(start, _, _)| *start == cursor)
        .map(|(_, _, content)| content.clone())
}

/// SAME-CALL-ARGUMENT resolution: `after_program_literal` is the position
/// right after a helper call's own program-name argument (the string
/// literal itself); the subcommand is that SAME call's own second,
/// array-literal argument — either an INLINE array literal, or (see
/// [`identifier_bound_array_literal_start`]) a plain identifier bound
/// earlier to one — never a later `.arg`/`.args` call, because this shape
/// has none.
fn resolve_same_call_argument_subcommand(
    masked: &[char],
    strings: &[(usize, usize, String)],
    string_starts: &BTreeSet<usize>,
    after_program_literal: usize,
) -> Option<String> {
    let mut pos = skip_insignificant(masked, string_starts, after_program_literal);
    if pos >= masked.len() || masked[pos] != ',' {
        return None;
    }
    pos = skip_insignificant(masked, string_starts, pos + 1);
    if pos < masked.len() && masked[pos] == '[' {
        return resolve_literal_at_delimiter(masked, strings, string_starts, pos);
    }
    let (ident, _) = read_ident_forward(masked, pos)?;
    let bracket = identifier_bound_array_literal_start(masked, string_starts, &ident, pos)?;
    resolve_literal_at_delimiter(masked, strings, string_starts, bracket)
}

/// Whether `ident` has ANY binding (`let`/`const`, or a plain
/// reassignment) STRICTLY BETWEEN `search_start` and `before` — the same
/// closest-preceding-binding scan [`identifier_bound_array_literal_start`]
/// already performs, generalized to any right-hand side (not only an
/// array literal), because a shadow's own VALUE never matters here — only
/// its EXISTENCE does. Used to detect a shadowing local inside a
/// pass-through helper's body: once `program` is rebound to anything
/// else after the parameter, it is a KNOWN value at every later use, not
/// the opaque caller-supplied parameter the pass-through recognition
/// exists to defer to the call site.
fn has_any_binding(
    masked: &[char],
    string_starts: &BTreeSet<usize>,
    ident: &str,
    search_start: usize,
    before: usize,
) -> bool {
    let mut pos = search_start;
    while pos < before && pos < masked.len() {
        if !is_ident_start(masked[pos]) {
            pos += 1;
            continue;
        }
        let ident_start = pos;
        let Some((name, end)) = read_ident_forward(masked, pos) else {
            pos += 1;
            continue;
        };
        pos = end;
        if name != ident || ident_start >= before {
            continue;
        }
        let eq_pos = skip_insignificant(masked, string_starts, pos);
        if eq_pos < before
            && eq_pos < masked.len()
            && masked[eq_pos] == '='
            && masked.get(eq_pos + 1) != Some(&'=')
        {
            return true;
        }
    }
    false
}

/// The `[` of the CLOSEST array literal (`vec![...]` or a bare `[...]`)
/// bound to `ident` via `let (mut)? <ident> = ...` (or a plain
/// `<ident> = ...` reassignment) STRICTLY BEFORE `before` — matching real
/// Rust scoping direction (a binding must precede its use) and, among
/// several same-named bindings, preferring the CLOSEST preceding one, the
/// same way shadowing would resolve at the use site. Supports the real
/// shape `tests/acceptance/common.rs` uses for a genuine `docker run`:
/// the argument list is bound to a variable, built from a literal
/// `vec![...]` whose OWN first element is the real subcommand, then
/// WIDENED by one or more later `.extend(...)` calls before being passed
/// by name — the first element stays the subcommand however much the vec
/// grows afterward.
///
/// Documented residual: an EARLIER, unrelated binding of the same
/// identifier name in a different function could be picked up instead of
/// the true nearest one if it happens to sit closer to `before` in the
/// file — this scan has no real per-function scope boundary. That is a
/// same-name-reuse risk to note, not a silent one: it can only ever cause
/// a MISATTRIBUTION to a different literal array, never a resolution from
/// a genuinely non-literal source, so an attacker-controlled bypass still
/// requires authoring a same-named decoy binding in the same file, which
/// review can see. Checked against the brace-tracked function boundaries
/// [`is_reviewed_generic_passthrough`] now computes, in case that
/// machinery closed this for free: it does not, without real added work
/// — this function's own callers never know which function's body they
/// are resolving inside (they walk from a CALL site's own identifier,
/// not a known, already-located signature the way the pass-through check
/// starts from), so bounding this search the same way would need its own
/// backward, nesting-aware "find the enclosing function" scan, not a
/// reuse of what already exists. Left as the same documented residual,
/// not folded in.
fn identifier_bound_array_literal_start(
    masked: &[char],
    string_starts: &BTreeSet<usize>,
    ident: &str,
    before: usize,
) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut pos = 0usize;
    while pos < before && pos < masked.len() {
        if !is_ident_start(masked[pos]) {
            pos += 1;
            continue;
        }
        let ident_start = pos;
        let Some((name, end)) = read_ident_forward(masked, pos) else {
            pos += 1;
            continue;
        };
        pos = end;
        if name != ident || ident_start >= before {
            continue;
        }
        let eq_pos = skip_insignificant(masked, string_starts, pos);
        if eq_pos >= masked.len() || masked[eq_pos] != '=' || masked.get(eq_pos + 1) == Some(&'=') {
            continue;
        }
        let mut rhs = skip_insignificant(masked, string_starts, eq_pos + 1);
        if let Some((macro_name, macro_end)) = read_ident_forward(masked, rhs)
            && macro_name == "vec"
            && masked.get(macro_end) == Some(&'!')
        {
            rhs = skip_insignificant(masked, string_starts, macro_end + 1);
        }
        if rhs < masked.len() && masked[rhs] == '[' {
            best = Some(rhs);
        }
    }
    best
}

/// The identifier a `Command::new(...)` call starting at `call_start` was
/// bound to — a `let` binding or a plain reassignment immediately before
/// the call — read backward from the `=` immediately preceding it
/// (skipping only whitespace, and rejecting a `==`/`!=`/`<=`/`>=` false
/// match). `None` means no such binding was found immediately before the
/// call — an unsupported shape (an alias, a builder passed straight into
/// a function call, ...).
fn binding_identifier_before(masked: &[char], call_start: usize) -> Option<String> {
    let mut pos = skip_ws_backward(masked, call_start);
    if pos == 0 || masked[pos - 1] != '=' {
        return None;
    }
    pos -= 1;
    if pos > 0 && matches!(masked[pos - 1], '=' | '!' | '<' | '>') {
        return None;
    }
    pos = skip_ws_backward(masked, pos);
    let (_, ident) = read_ident_backward(masked, pos)?;
    Some(ident)
}

/// VARIABLE-BOUND resolution: scans forward from `pos` for the FIRST
/// occurrence of the exact identifier `ident` immediately followed by
/// `.arg(`/`.args(` (a whole-identifier match — a same-named but
/// unrelated variable elsewhere would only make this scan MORE
/// conservative, never less, since an unresolved/misattributed read stays
/// banned either way), resolving that call's own literal argument.
fn first_identifier_arg_subcommand(
    masked: &[char],
    strings: &[(usize, usize, String)],
    string_starts: &BTreeSet<usize>,
    mut pos: usize,
    ident: &str,
) -> Option<String> {
    while pos < masked.len() {
        if !is_ident_start(masked[pos]) {
            pos += 1;
            continue;
        }
        let Some((name, end)) = read_ident_forward(masked, pos) else {
            pos += 1;
            continue;
        };
        pos = end;
        if name != ident {
            continue;
        }
        let dot_pos = skip_insignificant(masked, string_starts, pos);
        if dot_pos >= masked.len() || masked[dot_pos] != '.' {
            continue;
        }
        let method_pos = skip_insignificant(masked, string_starts, dot_pos + 1);
        let Some((method_name, method_end)) = read_ident_forward(masked, method_pos) else {
            continue;
        };
        if method_name != "arg" && method_name != "args" {
            continue;
        }
        let paren_pos = skip_insignificant(masked, string_starts, method_end);
        if paren_pos >= masked.len() || masked[paren_pos] != '(' {
            continue;
        }
        return resolve_literal_at_delimiter(masked, strings, string_starts, paren_pos);
    }
    None
}

/// CHAINED-temporary resolution: walks forward one `.method(...)` call at
/// a time from `pos` (the position right after `Command::new(...)`'s own
/// call), skipping any chained call that is not `.arg`/`.args` (e.g. an
/// unrelated `.current_dir(...)` before the real `.arg(...)`), until
/// either an `.arg`/`.args` call is found — whose own literal argument is
/// then resolved — or the chain ends (anything other than `.` next)
/// without one, which returns `None`: a chain that never calls
/// `.arg`/`.args` establishes no subcommand at all.
fn resolve_chain_subcommand(
    masked: &[char],
    strings: &[(usize, usize, String)],
    string_starts: &BTreeSet<usize>,
    mut pos: usize,
) -> Option<String> {
    loop {
        pos = skip_insignificant(masked, string_starts, pos);
        if pos >= masked.len() || masked[pos] != '.' {
            return None;
        }
        pos = skip_insignificant(masked, string_starts, pos + 1);
        let (method_name, ident_end) = read_ident_forward(masked, pos)?;
        let paren_pos = skip_insignificant(masked, string_starts, ident_end);
        if paren_pos >= masked.len() || masked[paren_pos] != '(' {
            return None;
        }
        let close = match_paren(masked, paren_pos)?;
        if method_name == "arg" || method_name == "args" {
            return resolve_literal_at_delimiter(masked, strings, string_starts, paren_pos);
        }
        pos = close + 1;
    }
}

/// COMMAND-NEW subcommand resolution: the subcommand actually passed to
/// the SPECIFIC `Command` receiver constructed by the `Command::new(...)`
/// call spanning `[call_start, call_end)` (`call_end` already past its
/// own matching close paren) — never a different builder's argument that
/// merely appears nearby. Only reached once the caller has already
/// recognized this call's program argument as cargo or the container
/// runtime; this function's only job is the subcommand. Two
/// receiver-attributed shapes, tried in order:
///
/// 1. CHAINED temporary (`Command::new(program).arg("build")`, or with
///    intervening chained calls like `.current_dir("x").arg("build")`).
/// 2. VARIABLE-BOUND (`let mut c = Command::new(program); ... c.arg("build");`)
///    — only attempted once the call is confirmed to END in a bare `;`
///    (i.e. it did NOT continue as a chain at all), so this never
///    conflates "a chain that never called `.arg`/`.args`" with "a
///    variable binding".
///
/// Anything neither shape positively attributes this way — an alias, a
/// builder passed into a function, an argument list assembled elsewhere —
/// resolves to `None`; the caller already treats `None` as banned, so a
/// harder-to-attribute site can only narrow what this scan allows, never
/// widen it.
fn resolve_command_new_subcommand(
    masked: &[char],
    strings: &[(usize, usize, String)],
    string_starts: &BTreeSet<usize>,
    call_start: usize,
    call_end: usize,
) -> Option<String> {
    if let Some(subcommand) = resolve_chain_subcommand(masked, strings, string_starts, call_end) {
        return Some(subcommand);
    }
    let after_call = skip_insignificant(masked, string_starts, call_end);
    if after_call >= masked.len() || masked[after_call] != ';' {
        return None;
    }
    let ident = binding_identifier_before(masked, call_start)?;
    first_identifier_arg_subcommand(masked, strings, string_starts, after_call + 1, &ident)
}

/// The position of the `}` matching the `{` at `open_brace`, by simple
/// depth counting over the MASKED text — safe because comment and string
/// bodies are blanked there, so a stray `{`/`}` inside either (a comment
/// describing a brace, for instance) can never desynchronize the count.
fn find_matching_brace(masked: &[char], open_brace: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open_brace;
    while i < masked.len() {
        match masked[i] {
            '{' => depth += 1,
            '}' => {
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

/// The ONE reviewed generic pass-through helper shape this scan lets a
/// non-literal `Command::new(X)` argument through on: `X` is exactly the
/// identifier `program`, in `tests/acceptance/common.rs` — the acceptance
/// harness's own centralized process-spawn helpers, which forward
/// whatever program name their CALLER passed. This is not "provably
/// benign" the way the entries in [`BENIGN_COMMAND_NEW_EXPRESSIONS`] are
/// — `program` could genuinely be anything — but it is not a NEW,
/// unscrutinized invocation site either: every real call to
/// `command_output`/`command_status` already has its own program
/// argument read and checked independently by
/// [`same_call_argument_violations`], at the point the ACTUAL value is
/// supplied. Recognizing it here only avoids asking this scan to prove
/// something structurally unprovable about a pure pass-through parameter.
///
/// Deliberately narrow and NOT name-matched the way the benign-expression
/// list is: `program` alone is too generic a word to trust by bare text
/// (unlike `ffmpeg`/`mediamtx`, which are self-describing tool names).
/// What this checks, exactly — no more than this, and ALL of this:
///
/// 1. The file is `tests/acceptance/common.rs`.
/// 2. A signature named EXACTLY `command_output` or `command_status`
///    exists earlier in the file and declares a parameter literally
///    named `program`.
/// 3. `call_start` sits STRICTLY INSIDE that signature's own function
///    body — its body's opening `{`, found after the signature's
///    parameter list (skipping the return type and any `where` clause,
///    neither of which contains a brace in the shape these two reviewed
///    helpers use), and the `}` that brace-counting matches it to. This
///    condition is load-bearing and was previously MISSING despite an
///    earlier version of this doc claiming otherwise: without a body
///    bound, any LATER `Command::new(program)` anywhere in the file —
///    including a decoy in a completely unrelated function — was wrongly
///    covered once the two real helpers were merely defined somewhere
///    above it in the same file.
/// 4. `program` at the call site still resolves to the PARAMETER — no
///    `let`/`const` binding of `program` between the body's own opening
///    `{` and the call rebinds it to something else first. Containment
///    (condition 3) establishes WHERE the call is, not that `program`
///    there still means the opaque, caller-supplied parameter — the
///    entire premise this recognition rests on. A shadowing
///    `let program = "cargo";` inside the body, followed by
///    `Command::new(program)`, is inside the helper and would satisfy
///    condition 3 alone, but `program` at that point is a KNOWN cargo
///    binding, not the parameter — exactly the case this recognition
///    must refuse, not cover. Checked with the same closest-preceding-
///    binding scan [`identifier_bound_array_literal_start`] already
///    performs for a same-named array variable, generalized here to any
///    binding at all (see [`has_any_binding`]), since a shadow's VALUE
///    does not matter — only its existence does.
///
/// A same-named-and-shaped helper defined in a DIFFERENT file, or under a
/// different name, is still NOT covered by this recognition and falls
/// through to the default refusal, still requiring its own reviewed
/// entry.
fn is_reviewed_generic_passthrough(path_display: &str, masked: &[char], call_start: usize) -> bool {
    if !path_display.ends_with("tests/acceptance/common.rs") {
        return false;
    }
    let argument = "program";
    let no_string_exceptions = BTreeSet::new();
    let after_paren = skip_insignificant(
        masked,
        &no_string_exceptions,
        call_start + "Command::new(".chars().count(),
    );
    let Some((name, end)) = read_ident_forward(masked, after_paren) else {
        return false;
    };
    let end = skip_insignificant(masked, &no_string_exceptions, end);
    if name != argument || masked.get(end) != Some(&')') {
        return false;
    }

    for helper_name in ["command_output", "command_status"] {
        for (_, after_name) in find_live_token_sequence(masked, &["fn", helper_name]) {
            let mut pos = skip_insignificant(masked, &no_string_exceptions, after_name);
            // Skip an optional `<...>` generic parameter list before the
            // real `(` — angle brackets never nest in a plain generic
            // parameter list of the shape this scan's own two reviewed
            // helpers use (`<I, S>`), so a flat depth count is enough.
            if masked.get(pos) == Some(&'<') {
                let mut depth = 0i32;
                while pos < masked.len() {
                    match masked[pos] {
                        '<' => depth += 1,
                        '>' => {
                            depth -= 1;
                            if depth == 0 {
                                pos += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    pos += 1;
                }
                pos = skip_insignificant(masked, &no_string_exceptions, pos);
            }
            if masked.get(pos) != Some(&'(') {
                continue;
            }
            let param_open = pos;
            let Some((param_name, _)) = read_ident_forward(masked, param_open + 1) else {
                continue;
            };
            if param_name != argument {
                continue;
            }
            let Some(param_close) = match_paren(masked, param_open) else {
                continue;
            };
            // Scan forward from the parameter list's own close paren for
            // the function's body-opening `{`.
            let mut body_open = param_close + 1;
            while body_open < masked.len() && masked[body_open] != '{' {
                body_open += 1;
            }
            if body_open >= masked.len() {
                continue;
            }
            let Some(body_close) = find_matching_brace(masked, body_open) else {
                continue;
            };
            if call_start > body_open
                && call_start < body_close
                && !has_any_binding(
                    masked,
                    &no_string_exceptions,
                    argument,
                    body_open,
                    call_start,
                )
            {
                return true;
            }
        }
    }
    false
}

/// Every violation the general `Command::new(X)` recognition axis finds
/// over `source` (from the file at `path_display`, used only by
/// [`is_reviewed_generic_passthrough`]): EVERY live `Command::new(` call
/// is found, its argument classified, and anything that is neither a
/// literal proven not to be cargo/the container runtime, the `env!`
/// cargo-binary form, a reviewed benign expression, nor the one reviewed
/// generic pass-through shape is refused OUTRIGHT — before any
/// subcommand is ever read. This is the fail-closed-at-recognition fix:
/// the old design only ever searched for KNOWN spellings of cargo/docker
/// (`Command::new("cargo")`, `env!("CARGO")`, a literal `"docker"`), so a
/// program name arriving through ANY other expression — most simply, a
/// variable — was invisible to it entirely and passed through
/// unconditionally allowed, regardless of what it actually pointed at.
fn command_new_violations(path_display: &str, source: &str) -> Vec<String> {
    let raw: Vec<char> = source.chars().collect();
    let lexed = lex(source);
    let masked = &lexed.masked;
    let string_starts = string_start_positions(&lexed.strings);

    let mut violations = Vec::new();
    for (call_start, marker_end) in find_live_token_sequence(masked, &["Command", "::", "new", "("])
    {
        let open_paren = marker_end - 1;
        let Some(close_paren) = match_paren(masked, open_paren) else {
            continue;
        };
        let (arg_start, arg_end) = trim_char_range(&raw, open_paren + 1, close_paren);
        let call_end = close_paren + 1;

        if let Some(content) = whole_argument_literal_content(&lexed.strings, arg_start, arg_end) {
            match content.as_str() {
                "cargo" => {
                    let subcommand = resolve_command_new_subcommand(
                        masked,
                        &lexed.strings,
                        &string_starts,
                        call_start,
                        call_end,
                    );
                    if !is_readonly_cargo_subcommand(subcommand.as_deref()) {
                        violations.push(describe_violation("cargo", subcommand));
                    }
                }
                docker_name if docker_name == container_runtime_name() => {
                    let subcommand = resolve_command_new_subcommand(
                        masked,
                        &lexed.strings,
                        &string_starts,
                        call_start,
                        call_end,
                    );
                    if matches!(subcommand.as_deref(), None | Some("build")) {
                        violations.push(describe_violation("the container runtime", subcommand));
                    }
                }
                _ => {} // a literal proven to be neither program: benign.
            }
            continue;
        }

        if is_env_cargo_argument(masked, &lexed.strings, &string_starts, arg_start, arg_end) {
            let subcommand = resolve_command_new_subcommand(
                masked,
                &lexed.strings,
                &string_starts,
                call_start,
                call_end,
            );
            if !is_readonly_cargo_subcommand(subcommand.as_deref()) {
                violations.push(describe_violation("cargo", subcommand));
            }
            continue;
        }

        if is_runtime_cargo_env_argument(masked, &lexed.strings, &string_starts, arg_start, arg_end)
        {
            let subcommand = resolve_command_new_subcommand(
                masked,
                &lexed.strings,
                &string_starts,
                call_start,
                call_end,
            );
            if !is_readonly_cargo_subcommand(subcommand.as_deref()) {
                violations.push(describe_violation("cargo", subcommand));
            }
            continue;
        }

        let argument_text: String = raw[arg_start..arg_end].iter().collect();
        if BENIGN_COMMAND_NEW_EXPRESSIONS
            .iter()
            .any(|(expression, _)| *expression == argument_text)
        {
            continue;
        }

        if is_reviewed_generic_passthrough(path_display, masked, call_start) {
            continue;
        }

        violations.push(format!(
            "Command::new({argument_text}) invokes an unrecognized program expression that this \
             scan cannot prove is not cargo or the container runtime — refused (add a reviewed \
             benign-expression entry if it genuinely is one)"
        ));
    }
    violations
}

fn describe_violation(program_label: &str, subcommand: Option<String>) -> String {
    let described = subcommand
        .map(|value| format!("{value:?}"))
        .unwrap_or_else(|| "<unresolved>".to_string());
    format!("invokes {program_label} inside a test process with subcommand {described}")
}

/// The container runtime's own program name, built from two literal
/// pieces so the word never appears as one contiguous token in this
/// file's own source — a decoy sample constructed elsewhere in this same
/// file (see the canaries below) could otherwise be found by this scan's
/// own live-code check as a real occurrence of itself.
fn container_runtime_name() -> String {
    ["dock", "er"].concat()
}

/// Every violation the same-call-argument axis finds: a helper taking the
/// program name and its argument list as two positional arguments of ONE
/// call (`command_output`/`command_status`, the acceptance harness's own
/// helpers). The call's OPENING text is found first, independent of the
/// program argument's own position, then whitespace is skipped
/// (tolerating a real multi-line call whose program string sits on its
/// own indented line) before requiring that position to land on a real
/// string literal. A program argument that is NOT a whole string literal
/// cannot be proven to be anything — the fail-closed-at-recognition fix
/// applies here too — and is refused outright, the same as an
/// unrecognized `Command::new(X)`.
fn same_call_argument_violations(source: &str) -> Vec<String> {
    let lexed = lex(source);
    let masked = &lexed.masked;
    let string_starts = string_start_positions(&lexed.strings);

    let mut violations = Vec::new();
    for call_prefix in [["command_output", "("], ["command_status", "("]] {
        for (_, marker_end) in find_live_token_sequence(masked, &call_prefix) {
            let open_paren = marker_end - 1;
            let arg_pos = skip_insignificant(masked, &string_starts, open_paren + 1);
            let Some((_, string_end, content)) =
                lexed.strings.iter().find(|(start, _, _)| *start == arg_pos)
            else {
                let helper_name = call_prefix[0];
                violations.push(format!(
                    "{helper_name}(...) passes a program argument that is not a literal string — \
                     this scan cannot prove it is not cargo or the container runtime — refused"
                ));
                continue;
            };
            match content.as_str() {
                "cargo" => {
                    let subcommand = resolve_same_call_argument_subcommand(
                        masked,
                        &lexed.strings,
                        &string_starts,
                        *string_end,
                    );
                    if !is_readonly_cargo_subcommand(subcommand.as_deref()) {
                        violations.push(describe_violation("cargo", subcommand));
                    }
                }
                docker_name if docker_name == container_runtime_name() => {
                    let subcommand = resolve_same_call_argument_subcommand(
                        masked,
                        &lexed.strings,
                        &string_starts,
                        *string_end,
                    );
                    if matches!(subcommand.as_deref(), None | Some("build")) {
                        violations.push(describe_violation("the container runtime", subcommand));
                    }
                }
                _ => {} // a literal proven to be neither program: benign.
            }
        }
    }
    violations
}

/// The raw-shell-kill pattern (`Command::new("kill")`) — multi-token AND
/// carrying a string argument, so it needs both the token-sequence
/// tolerance [`find_live_token_sequence`] gives every other marker in
/// this file and a landing check against a real string literal reading
/// exactly `"kill"`, the same string-boundary care
/// [`resolve_literal_at_delimiter`] already takes. Evades a contiguous
/// substring match on ANY whitespace in the sequence — not only spaced
/// `::`, but a single space before the string argument itself
/// (`Command::new( "kill")`), which a plain `"Command::new(\"kill\")"`
/// text compare cannot tolerate at all.
fn contains_raw_shell_kill(
    masked: &[char],
    strings: &[(usize, usize, String)],
    string_starts: &BTreeSet<usize>,
) -> bool {
    for (_, marker_end) in find_live_token_sequence(masked, &["Command", "::", "new", "("]) {
        let pos = skip_insignificant(masked, string_starts, marker_end);
        if strings
            .iter()
            .any(|(start, _, content)| *start == pos && content == "kill")
        {
            return true;
        }
    }
    false
}

/// The negative-kill-argument-formatting pattern (a `format!(...)` call
/// whose own format string starts with the literal text `-{`, the shape
/// used to build a negative-PID argument for a raw process-group kill).
/// Token-tolerant over `format`, `!`, `(`, then a real string literal
/// checked by PREFIX (the full format string varies — `"-{pid}"`,
/// `"-{}"`, ...) rather than exact content, unlike
/// [`contains_raw_shell_kill`]'s exact `"kill"` match.
fn contains_negative_kill_argument_formatting(
    masked: &[char],
    strings: &[(usize, usize, String)],
    string_starts: &BTreeSet<usize>,
) -> bool {
    for (_, marker_end) in find_live_token_sequence(masked, &["format", "!", "("]) {
        let pos = skip_insignificant(masked, string_starts, marker_end);
        if strings
            .iter()
            .any(|(start, _, content)| *start == pos && content.starts_with("-{"))
        {
            return true;
        }
    }
    false
}

/// Every unsafe-cleanup or nested-artifact-build violation found in one
/// file's source text (from the file at `path_display`), plus whether it
/// contains the allowed `libc`-`kill` call — pure so the real scan below
/// and this test's own positive and negative canaries share the
/// identical logic, never two copies that could quietly drift apart.
///
/// The banned patterns split into two real classes, not one blanket
/// treatment: four of `single_token_banned` below are each a SINGLE
/// identifier — whitespace cannot appear INSIDE a token without
/// producing a different identifier entirely, so a contiguous substring
/// match is already sufficient, and converting them to token-sequence
/// matching would add machinery that buys nothing (their own names,
/// deliberately, are never spelled out directly in THIS comment either —
/// they are built by the exact same `.concat()` self-avoidance the
/// multi-token ones used to need, and a raw-text substring check, unlike
/// the masked-buffer checks below, cannot tell a doc comment's mention
/// apart from a live occurrence). The other four ARE multi-token — a raw
/// shell kill call, the nix signal-kill path, the negative-kill-argument
/// format string, and the `libc`-`kill(` call itself (the one ALLOWED-path
/// detection, so spacing it changes which file this scan judges to hold
/// the permitted call, the opposite direction from the other three but
/// the identical root cause) — and are exactly as evadable by
/// insignificant whitespace as the invocation markers were, so they get
/// the identical [`find_live_token_sequence`] treatment. None of the four
/// `.concat()` self-avoidance the old contiguous versions required
/// either: matching now happens against the MASKED buffer, so a token's
/// own text only ever needs to appear as a plain string literal in this
/// file's OWN source (inside the token array below) — which the shared
/// lexer blanks before this scan ever reads itself back, the same reason
/// `Command::new(`'s own search needs no such trick.
fn process_cleanup_violations_in(path_display: &str, source: &str) -> (Vec<String>, bool) {
    let single_token_banned = [
        ("process-group kill", ["kill", "pg"].concat()),
        ("process-group lookup", ["get", "pgid"].concat()),
        ("process-group target enum", ["Signal", "Target"].concat()),
        (
            "process-group target variant",
            ["Process", "Group"].concat(),
        ),
    ];

    let lexed = lex(source);
    let masked = &lexed.masked;
    let string_starts = string_start_positions(&lexed.strings);

    let mut violations = Vec::new();
    for (name, pattern) in &single_token_banned {
        if source.contains(pattern) {
            violations.push(format!("contains {name}"));
        }
    }
    if contains_raw_shell_kill(masked, &lexed.strings, &string_starts) {
        violations.push("contains raw shell kill".to_string());
    }
    if !find_live_token_sequence(masked, &["nix", "::", "sys", "::", "signal", "::", "kill"])
        .is_empty()
    {
        violations.push("contains nix signal kill".to_string());
    }
    if contains_negative_kill_argument_formatting(masked, &lexed.strings, &string_starts) {
        violations.push("contains negative kill argument formatting".to_string());
    }
    let has_libc_kill = !find_live_token_sequence(masked, &["libc", "::", "kill", "("]).is_empty();

    violations.extend(command_new_violations(path_display, source));
    violations.extend(same_call_argument_violations(source));

    (violations, has_libc_kill)
}

/// The one file allowed to call `libc::kill` directly — the acceptance
/// harness's own positive, target-checked direct-child cleanup helper.
/// Kept as a real path (not a bare filename) so the check below is exact
/// about WHICH file, not merely that some file somewhere is allowed.
fn allowed_libc_kill_path(root: &std::path::Path) -> PathBuf {
    root.join("tests/acceptance/common.rs")
}

#[test]
fn no_unsafe_cleanup_or_nested_artifact_builds_in_test_sources() {
    let root = workspace_root();
    let mut rust_files = rust_sources(&root.join("tests"));
    // Walk the ONE shared crate-name list every crate-scope guard in this
    // workspace already walks (`environment_read_surface.rs`,
    // `cli_secret_flag_surface.rs`, `transport_purity.rs`), rather than a
    // second, independently hardcoded directory pair: a test relocated
    // from `crates/vigil/tests` into `crates/vigil-ha/tests` or
    // `crates/vigil-bin/tests` must not evade this scan just because a
    // narrower copy of the crate set stopped at one name.
    for production_crate in PRODUCTION_CRATES {
        rust_files.extend(rust_sources(
            &root.join("crates").join(production_crate).join("tests"),
        ));
    }
    // A guard that silently walked an empty or wrong directory would pass
    // green while checking nothing — proven wrong, not assumed: at least
    // one file from EVERY production crate's own `tests/` must actually
    // be present in what was collected.
    for production_crate in PRODUCTION_CRATES {
        let crate_tests_dir = root.join("crates").join(production_crate).join("tests");
        assert!(
            rust_files
                .iter()
                .any(|path| path.starts_with(&crate_tests_dir)),
            "found zero .rs files under {}; this scan would otherwise pass green while silently \
             checking nothing for that crate's tests",
            crate_tests_dir.display()
        );
    }

    let allowed_libc_kill_path = allowed_libc_kill_path(&root);
    let mut libc_kill_locations = Vec::new();
    let mut violations = Vec::new();

    for path in rust_files {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
        let path_display = path.display().to_string();
        let (file_violations, has_libc_kill) =
            process_cleanup_violations_in(&path_display, &source);
        violations.extend(
            file_violations
                .into_iter()
                .map(|violation| format!("{path_display}: {violation}")),
        );
        if has_libc_kill {
            libc_kill_locations.push(path.clone());
        }
    }

    assert!(
        violations.is_empty(),
        "acceptance tests must not contain unsafe cleanup or nested artifact builds:\n{}",
        violations.join("\n")
    );
    assert_eq!(
        libc_kill_locations,
        vec![allowed_libc_kill_path.clone()],
        "raw signal syscalls must stay centralized in the positive direct-child cleanup helper"
    );

    // Cargo-subcommand awareness. `dependency_direction_contract.rs` runs
    // a real, read-only `cargo metadata --locked` dependency-graph fetch,
    // so this scan must stay green against exactly that call — a blanket
    // ban on the mere presence of a cargo-invoking marker would ban it
    // identically to a real nested build, which is wrong.
    let other_path = "crates/vigil/tests/some_other_file.rs";
    let common_rs_path = "tests/acceptance/common.rs";

    // A bare metadata read must be ALLOWED — the exact shape
    // `dependency_direction_contract.rs` uses.
    let metadata_only_sample = "fn fetch_cargo_metadata() {\n    \
             let output = Command::new(env!(\"CARGO\"))\n        \
                 .args([\"metadata\", \"--format-version\", \"1\", \"--locked\"])\n        \
                 .output();\n\
         }\n";
    let metadata_only_violations =
        process_cleanup_violations_in(other_path, metadata_only_sample).0;
    assert!(
        metadata_only_violations.is_empty(),
        "a bare cargo metadata read must be allowed, never flagged as a nested artifact build: \
         {metadata_only_violations:?}"
    );

    // A real build invocation, written the same way, must still be
    // BANNED — narrowing the allowance must not widen it.
    let build_sample = "fn accidentally_builds() {\n    \
             Command::new(env!(\"CARGO\")).args([\"build\", \"--release\"]);\n\
         }\n";
    let build_violations = process_cleanup_violations_in(other_path, build_sample).0;
    assert_eq!(
        build_violations.len(),
        1,
        "a real cargo build invocation must still be banned after narrowing to \
         subcommand-awareness: {build_violations:?}"
    );

    // A file mixing one allowed metadata read with one real build must
    // still fail, on the build occurrence specifically — the allowance is
    // per-occurrence, not per-file.
    let mixed_sample = "fn one_metadata_one_build() {\n    \
             Command::new(env!(\"CARGO\")).args([\"metadata\", \"--locked\"]);\n    \
             Command::new(env!(\"CARGO\")).args([\"build\"]);\n\
         }\n";
    let mixed_violations = process_cleanup_violations_in(other_path, mixed_sample).0;
    assert_eq!(
        mixed_violations.len(),
        1,
        "a file mixing an allowed metadata read with a real build invocation must still fail: \
         {mixed_violations:?}"
    );

    // An argument list assembled from a variable, not a literal, must
    // ALSO stay banned: this scan cannot read a subcommand out of the
    // source at all in that shape, so it is UNKNOWN, never assumed safe.
    let dynamic_argv_sample = "fn dynamic_argv_is_not_readable_from_source() {\n    \
             let computed_args = compute_args_elsewhere();\n    \
             Command::new(env!(\"CARGO\")).args(computed_args);\n\
         }\n";
    let dynamic_argv_violations = process_cleanup_violations_in(other_path, dynamic_argv_sample).0;
    assert_eq!(
        dynamic_argv_violations.len(),
        1,
        "a cargo invocation whose argument list is assembled from a variable must stay banned: \
         this scan cannot resolve its subcommand from the source at all: \
         {dynamic_argv_violations:?}"
    );

    // The decoy shape a design reading "the first string literal anywhere
    // in the window" would miss: a distractor string literal passed to an
    // UNRELATED builder method (`current_dir`), textually before the real
    // argument, in the SAME chain. This scan must resolve the subcommand
    // actually passed through `.arg(`/`.args(`, never a string literal
    // that merely appears earlier in the same builder chain.
    let decoy_sample = "fn decoy_current_dir_then_build() {\n    \
             Command::new(\"cargo\").current_dir(\"metadata\").arg(\"build\");\n\
         }\n";
    let decoy_violations = process_cleanup_violations_in(other_path, decoy_sample).0;
    assert_eq!(
        decoy_violations.len(),
        1,
        "a real build invocation preceded by an unrelated string literal in the same builder \
         chain must still be caught, not falsely allowed: {decoy_violations:?}"
    );

    // The cross-builder decoy bug, reproduced and closed: a DIFFERENT
    // builder's `.arg("metadata")` call, textually between the cargo
    // builder's own binding and its `.arg("build")` call. A design that
    // resolves the subcommand as "the nearest `.arg`/`.args` call after
    // the marker", irrespective of receiver, misattributes the unrelated
    // call to the cargo invocation and waves the real build through.
    // Receiver attribution must follow the SPECIFIC identifier the cargo
    // builder was bound to, never the first matching call textually
    // nearby.
    let cross_builder_decoy_sample = "fn cross_builder_variable_bound_decoy() {\n    \
             let mut cargo_cmd = Command::new(env!(\"CARGO\"));\n    \
             let mut other = Command::new(\"something_else\");\n    \
             other.arg(\"metadata\");\n    \
             cargo_cmd.arg(\"build\");\n\
         }\n";
    let cross_builder_decoy_violations =
        process_cleanup_violations_in(other_path, cross_builder_decoy_sample).0;
    assert_eq!(
        cross_builder_decoy_violations.len(),
        1,
        "an unrelated builder's own `.arg` call must never be misattributed to a DIFFERENT, \
         variable-bound cargo invocation that itself really calls `.arg(\"build\")`: \
         {cross_builder_decoy_violations:?}"
    );

    // The container-runtime axis, given the identical treatment: a real
    // non-build invocation (as `tests/acceptance/common.rs` genuinely
    // makes today, e.g. an inspect/logs/stop call) must be ALLOWED, not
    // banned outright the way the old single-spelling check treated
    // every such invocation.
    let runtime_program = container_runtime_name();
    let runtime_inspect_sample = format!(
        "fn inspects_a_container() {{\n    \
             let _ = command_output(\"{runtime_program}\", [\"inspect\", \"--format\", \"{{{{.State.Pid}}}}\", name]);\n\
         }}\n"
    );
    let runtime_inspect_violations =
        process_cleanup_violations_in(other_path, &runtime_inspect_sample).0;
    assert!(
        runtime_inspect_violations.is_empty(),
        "a real, non-build container-runtime invocation (an inspect) must be allowed, never \
         flagged: {runtime_inspect_violations:?}"
    );

    // The SAME real shape, written across multiple lines the way every
    // real caller in `tests/acceptance/common.rs` actually writes it
    // (the program name and its argument array each on their own
    // indented line) — proves this scan is not fooled by a real,
    // legitimate line break the way a single fixed contiguous spelling
    // would be.
    let runtime_multiline_inspect_sample = format!(
        "fn inspects_a_container_multiline() {{\n    \
             let _ = command_output(\n        \"{runtime_program}\",\n        [\n            \"inspect\",\n            \"--format\",\n            \"{{{{.State.Pid}}}}\",\n            name,\n        ],\n    );\n\
         }}\n"
    );
    let runtime_multiline_inspect_violations =
        process_cleanup_violations_in(other_path, &runtime_multiline_inspect_sample).0;
    assert!(
        runtime_multiline_inspect_violations.is_empty(),
        "a real, non-build container-runtime invocation split across multiple lines (the shape \
         every real caller in this workspace actually uses) must be allowed, never flagged: \
         {runtime_multiline_inspect_violations:?}"
    );

    // A real build must still be BANNED, and caught regardless of
    // whitespace around the array literal — the old check matched one
    // exact textual spelling only.
    let runtime_build_sample = format!(
        "fn accidentally_builds_the_runtime_image() {{\n    \
             let _ = command_output( \"{runtime_program}\" , [ \"build\", \".\" ] );\n\
         }}\n"
    );
    let runtime_build_violations =
        process_cleanup_violations_in(other_path, &runtime_build_sample).0;
    assert_eq!(
        runtime_build_violations.len(),
        1,
        "a real container-runtime build invocation must still be banned even with different \
         surrounding whitespace than one fixed spelling: {runtime_build_violations:?}"
    );

    // A container-runtime invocation whose subcommand cannot be resolved
    // at all must stay banned — the same fail-closed default as the
    // cargo axis.
    let runtime_dynamic_sample = format!(
        "fn dynamic_runtime_argv_is_not_readable_from_source() {{\n    \
             let computed_args = compute_runtime_args_elsewhere();\n    \
             let _ = command_output(\"{runtime_program}\", computed_args);\n\
         }}\n"
    );
    let runtime_dynamic_violations =
        process_cleanup_violations_in(other_path, &runtime_dynamic_sample).0;
    assert_eq!(
        runtime_dynamic_violations.len(),
        1,
        "a container-runtime invocation whose argument list is assembled from a variable must \
         stay banned: {runtime_dynamic_violations:?}"
    );

    // The same-call-argument helper's own PROGRAM name, not just its
    // argument list, must be fail-closed at recognition too: a program
    // name arriving through a plain variable — never a literal string
    // this scan can read — must be refused, because it cannot be proven
    // to be neither cargo nor the container runtime.
    let runtime_program_via_variable_sample = "fn container_runtime_program_via_a_plain_variable() {\n    \
             let runtime = compute_runtime_name_elsewhere();\n    \
             let _ = command_output(runtime, [\"build\"]);\n\
         }\n";
    let runtime_program_via_variable_violations =
        process_cleanup_violations_in(other_path, runtime_program_via_variable_sample).0;
    assert_eq!(
        runtime_program_via_variable_violations.len(),
        1,
        "a same-call-argument helper whose OWN program name is a plain variable, not a literal \
         this scan can read, must be refused outright: {runtime_program_via_variable_violations:?}"
    );

    // The fail-open-at-recognition bug, reproduced and closed: recognition
    // itself, not just argument attribution, must be fail-closed. A
    // program name arriving through a plain `let` variable — never
    // spelling `"cargo"` or `Command::new("cargo")` anywhere in the
    // source — must still be refused, because this scan cannot prove
    // that variable is not bound to cargo. The binding is named
    // `the_cargo_program`, deliberately NOT `program`, so this proof
    // cannot be mistaken for exercising the separate, narrowly-scoped
    // pass-through recognition below, which keys on that exact name.
    let cargo_via_let_sample = "fn cargo_bound_through_a_plain_let_variable() {\n    \
             let the_cargo_program = \"cargo\";\n    \
             Command::new(the_cargo_program).arg(\"build\");\n\
         }\n";
    let cargo_via_let_violations =
        process_cleanup_violations_in(other_path, cargo_via_let_sample).0;
    assert_eq!(
        cargo_via_let_violations.len(),
        1,
        "Command::new(X) where X is a plain `let`-bound variable holding the cargo program must \
         be refused outright — unrecognized, not unattributed: {cargo_via_let_violations:?}"
    );

    // The identical proof through a `const` binding, since a constant is
    // just as opaque to this text scan as a `let` variable — neither is
    // read for its VALUE, only recognized by NAME against the reviewed
    // benign list, which a program-holding constant is deliberately not
    // on.
    let cargo_via_const_sample = "const THE_CARGO_TOOL: &str = \"cargo\";\n\
         fn cargo_bound_through_a_const() {\n    \
             Command::new(THE_CARGO_TOOL).arg(\"build\");\n\
         }\n";
    let cargo_via_const_violations =
        process_cleanup_violations_in(other_path, cargo_via_const_sample).0;
    assert_eq!(
        cargo_via_const_violations.len(),
        1,
        "Command::new(X) where X is a `const` bound to the cargo program must be refused \
         outright: {cargo_via_const_violations:?}"
    );

    // The container-runtime axis gets the identical two proofs: a plain
    // `let` variable and a `const`, each bound to the container runtime's
    // own program name and spawning a build through
    // `Command::new(...)` directly (not through the
    // `command_output`/`command_status` helpers the runtime's real
    // callers happen to use today).
    let runtime_via_let_sample = format!(
        "fn container_runtime_bound_through_a_plain_let_variable() {{\n    \
             let the_runtime_program = \"{runtime_program}\";\n    \
             Command::new(the_runtime_program).arg(\"build\");\n\
         }}\n"
    );
    let runtime_via_let_violations =
        process_cleanup_violations_in(other_path, &runtime_via_let_sample).0;
    assert_eq!(
        runtime_via_let_violations.len(),
        1,
        "Command::new(X) where X is a plain `let`-bound variable holding the container-runtime \
         program must be refused outright: {runtime_via_let_violations:?}"
    );
    let runtime_via_const_sample = format!(
        "const THE_RUNTIME_TOOL: &str = \"{runtime_program}\";\n\
         fn container_runtime_bound_through_a_const() {{\n    \
             Command::new(THE_RUNTIME_TOOL).arg(\"build\");\n\
         }}\n"
    );
    let runtime_via_const_violations =
        process_cleanup_violations_in(other_path, &runtime_via_const_sample).0;
    assert_eq!(
        runtime_via_const_violations.len(),
        1,
        "Command::new(X) where X is a `const` bound to the container-runtime program must be \
         refused outright: {runtime_via_const_violations:?}"
    );

    // The same call-layer-up shape for the same-call-argument helpers:
    // `command_output(TOOL, args)` where `TOOL` is a `const` bound to the
    // cargo program, never a literal this scan can read at the call
    // site. Confirmed BANNED, not left as an unstated residual.
    let cargo_const_via_helper_sample = "const THE_CARGO_TOOL: &str = \"cargo\";\n\
         fn cargo_bound_through_a_const_passed_to_the_helper() {\n    \
             let _ = command_output(THE_CARGO_TOOL, [\"build\"]);\n\
         }\n";
    let cargo_const_via_helper_violations =
        process_cleanup_violations_in(other_path, cargo_const_via_helper_sample).0;
    assert_eq!(
        cargo_const_via_helper_violations.len(),
        1,
        "command_output(TOOL, ...) where TOOL is a `const` bound to the cargo program must be \
         refused outright, the identical treatment as Command::new(TOOL): \
         {cargo_const_via_helper_violations:?}"
    );

    // A DIFFERENT, entirely unrelated variable holding some other real,
    // harmless program must still be refused too — the point of the
    // fail-closed default is that NOTHING outside the reviewed benign set
    // or the recognized cargo/docker forms is let through, regardless of
    // what it actually turns out to hold.
    let unreviewed_benign_looking_sample = "fn some_other_tool_not_in_the_reviewed_set() {\n    \
             let jq_binary = resolve_jq_path();\n    \
             Command::new(jq_binary).arg(\"--version\");\n\
         }\n";
    let unreviewed_benign_looking_violations =
        process_cleanup_violations_in(other_path, unreviewed_benign_looking_sample).0;
    assert_eq!(
        unreviewed_benign_looking_violations.len(),
        1,
        "a program expression that is not in the reviewed benign set must be refused even when \
         it is plausibly harmless — widening the set is a deliberate, reviewed act, never an \
         automatic inference: {unreviewed_benign_looking_violations:?}"
    );

    // Every reviewed benign expression must actually be recognized and
    // allowed — proven positively, one at a time, rather than trusted
    // because it is merely listed.
    for (expression, _justification) in BENIGN_COMMAND_NEW_EXPRESSIONS {
        let sample = format!(
            "fn uses_a_reviewed_benign_expression() {{\n    \
                 let mut command = Command::new({expression});\n\
             }}\n"
        );
        let violations = process_cleanup_violations_in(other_path, &sample).0;
        assert!(
            violations.is_empty(),
            "reviewed benign expression {expression:?} must be allowed, never flagged: \
             {violations:?}"
        );
    }

    // The one reviewed generic pass-through helper shape — `program` as
    // the identifier, inside a function named `command_output` or
    // `command_status`, in `tests/acceptance/common.rs` specifically —
    // must be allowed.
    let passthrough_sample = "fn command_output<I, S>(program: &str, args: I) -> Option<Output> {\n    \
             Command::new(program).args(args).output().ok()\n\
         }\n";
    let passthrough_violations =
        process_cleanup_violations_in(common_rs_path, passthrough_sample).0;
    assert!(
        passthrough_violations.is_empty(),
        "the reviewed command_output/command_status generic pass-through shape in \
         tests/acceptance/common.rs must be allowed: {passthrough_violations:?}"
    );

    // The SAME shape, defined in a DIFFERENT file, must NOT be covered by
    // that recognition — proving it is scoped to the one reviewed
    // definition, not "any function anywhere using a parameter named
    // `program`".
    let passthrough_elsewhere_violations =
        process_cleanup_violations_in(other_path, passthrough_sample).0;
    assert_eq!(
        passthrough_elsewhere_violations.len(),
        1,
        "the generic pass-through recognition must be scoped to \
         tests/acceptance/common.rs's own command_output/command_status, not any \
         similarly-shaped function elsewhere: {passthrough_elsewhere_violations:?}"
    );

    // The SAME shape, in the right file, but under a DIFFERENT function
    // name, must ALSO not be covered — proving the recognition checks the
    // function's identity, not merely "a program-shaped parameter exists
    // somewhere in this file".
    let differently_named_passthrough_sample = "fn a_decoy_helper_with_the_same_shape(program: &str, args: I) -> Option<Output> {\n    \
             Command::new(program).args(args).output().ok()\n\
         }\n";
    let differently_named_passthrough_violations =
        process_cleanup_violations_in(common_rs_path, differently_named_passthrough_sample).0;
    assert_eq!(
        differently_named_passthrough_violations.len(),
        1,
        "the generic pass-through recognition must check the enclosing function's OWN name, not \
         just its file: {differently_named_passthrough_violations:?}"
    );

    // The generic pass-through recognition must check that the call sits
    // INSIDE the reviewed helper's own body, not merely somewhere later
    // in the same file. A real, unrelated function defined AFTER
    // `command_output` in `common.rs` — the exact file the recognition is
    // scoped to — with its own `program` variable bound to the cargo
    // program, must still be refused.
    let call_after_the_real_helpers_sample = "fn command_output<I, S>(program: &str, args: I) -> Option<Output> {\n    \
             Command::new(program).args(args).output().ok()\n\
         }\n\
         fn command_status<I, S>(program: &str, args: I) -> bool {\n    \
             Command::new(program).args(args).status().map(|status| status.success()).unwrap_or(false)\n\
         }\n\
         fn an_unrelated_later_function_with_its_own_program_variable() {\n    \
             let program = \"cargo\";\n    \
             Command::new(program).arg(\"build\");\n\
         }\n";
    let call_after_the_real_helpers_violations =
        process_cleanup_violations_in(common_rs_path, call_after_the_real_helpers_sample).0;
    assert_eq!(
        call_after_the_real_helpers_violations.len(),
        1,
        "a Command::new(program) call in a real, unrelated function defined AFTER the reviewed \
         helpers in common.rs must still be refused — the recognition must bound the call to \
         the helper's OWN body, not merely require the call to come somewhere after the helper's \
         signature: {call_after_the_real_helpers_violations:?}"
    );

    // Containment inside the reviewed helper's body is NECESSARY but not
    // SUFFICIENT: a shadowing `let program = "cargo";` INSIDE the body,
    // before the call, rebinds `program` to a KNOWN value — it is no
    // longer the opaque, caller-supplied parameter the pass-through
    // recognition exists to defer to the call site. This must be
    // refused, not covered, even though the call is genuinely inside
    // `command_output`'s own body.
    let shadowed_parameter_inside_helper_sample = "fn command_output<I, S>(program: &str, args: I) -> Option<Output> {\n    \
             let program = \"cargo\";\n    \
             Command::new(program).arg(\"build\")\n\
         }\n";
    let shadowed_parameter_inside_helper_violations =
        process_cleanup_violations_in(common_rs_path, shadowed_parameter_inside_helper_sample).0;
    assert_eq!(
        shadowed_parameter_inside_helper_violations.len(),
        1,
        "a shadowing `let program = \"cargo\";` inside the reviewed helper's own body, before \
         the call, must still be refused — containment inside the body is not enough once \
         `program` no longer means the parameter: {shadowed_parameter_inside_helper_violations:?}"
    );

    // Marker recognition itself must be fail-closed, not just argument
    // attribution: Rust accepts insignificant whitespace between every
    // token, so `Command :: new ( ... )` is token-identical to
    // `Command::new(...)` even though it contains no `Command::new(`
    // substring at all. A bare contiguous substring search never finds
    // this call, so it is never classified, never refused — fail-open at
    // the very first step. An unrecognized program expression reached
    // through token-spaced syntax must still be refused.
    let token_spaced_unrecognized_sample = "fn token_spaced_call_with_an_unrecognized_program() {\n    \
             let tool = resolve_some_tool_elsewhere();\n    \
             Command :: new (tool).arg(\"build\");\n\
         }\n";
    let token_spaced_unrecognized_violations =
        process_cleanup_violations_in(other_path, token_spaced_unrecognized_sample).0;
    assert_eq!(
        token_spaced_unrecognized_violations.len(),
        1,
        "a token-spaced Command :: new (...) call must be found and refused exactly like its \
         compact spelling — marker recognition, not just argument attribution, must be \
         fail-closed: {token_spaced_unrecognized_violations:?}"
    );

    // The identical token-spacing gap in the raw-shell-kill ban itself —
    // the guard's own original purpose, not the later cargo/docker
    // recognition work — evading on a single space anywhere in the
    // sequence, including before the string argument (never only spaced
    // `::`).
    let token_spaced_raw_shell_kill_sample = "fn token_spaced_raw_shell_kill() {\n    \
             Command::new (\"kill\").arg(\"-9\").arg(\"1234\");\n\
         }\n";
    let token_spaced_raw_shell_kill_violations =
        process_cleanup_violations_in(other_path, token_spaced_raw_shell_kill_sample).0;
    assert_eq!(
        token_spaced_raw_shell_kill_violations.len(),
        1,
        "a token-spaced Command::new (\"kill\") call must still be caught as a raw shell kill, \
         evading on a single space before the string argument, not only spaced `::`: \
         {token_spaced_raw_shell_kill_violations:?}"
    );

    // The identical gap in the nix signal-kill ban, spaced at the `::`
    // punctuation specifically.
    let token_spaced_nix_signal_kill_sample = "fn token_spaced_nix_signal_kill() {\n    \
             nix::sys::signal :: kill(pid, Signal::SIGKILL)?;\n\
         }\n";
    let token_spaced_nix_signal_kill_violations =
        process_cleanup_violations_in(other_path, token_spaced_nix_signal_kill_sample).0;
    assert_eq!(
        token_spaced_nix_signal_kill_violations.len(),
        1,
        "a token-spaced nix::sys::signal :: kill call must still be caught: \
         {token_spaced_nix_signal_kill_violations:?}"
    );

    // The whitespace axis is not the only spelling variant the compiler
    // treats as identical to a plain identifier: a raw identifier
    // (`r#new`, `r#kill`, ...) names the SAME item. `Command::r#new(...)`
    // must still be recognized as a cargo invocation and banned for a
    // real build.
    let raw_ident_command_new_sample = "fn raw_identifier_command_new() {\n    \
             Command::r#new(env!(\"CARGO\")).args([\"build\"]);\n\
         }\n";
    let raw_ident_command_new_violations =
        process_cleanup_violations_in(other_path, raw_ident_command_new_sample).0;
    assert_eq!(
        raw_ident_command_new_violations.len(),
        1,
        "Command::r#new(...) must be recognized exactly like Command::new(...) and a real \
         build through it still banned: {raw_ident_command_new_violations:?}"
    );

    // The identical raw-identifier tolerance for the nix signal-kill ban,
    // on its own `kill` segment.
    let raw_ident_nix_signal_kill_sample = "fn raw_identifier_nix_signal_kill() {\n    \
             nix::sys::signal::r#kill(pid, Signal::SIGKILL)?;\n\
         }\n";
    let raw_ident_nix_signal_kill_violations =
        process_cleanup_violations_in(other_path, raw_ident_nix_signal_kill_sample).0;
    assert_eq!(
        raw_ident_nix_signal_kill_violations.len(),
        1,
        "nix::sys::signal::r#kill(...) must still be caught: \
         {raw_ident_nix_signal_kill_violations:?}"
    );

    // The raw-identifier axis on `libc::kill(` inherits the INVERTED
    // consequence: this is the allowed-path detection, so a miss here
    // does not drop a ban — it misjudges which file this scan believes
    // holds the one permitted call. Written in the allowed file itself,
    // `libc::r#kill(` must still be recognized as the same call
    // `allowed_libc_kill_path` is trusted to hold.
    let raw_ident_libc_kill_sample = "fn raw_identifier_libc_kill() {\n    \
             unsafe { libc::r#kill(pid, sig) };\n\
         }\n";
    let raw_ident_libc_kill_has_libc_kill =
        process_cleanup_violations_in(common_rs_path, raw_ident_libc_kill_sample).1;
    assert!(
        raw_ident_libc_kill_has_libc_kill,
        "libc::r#kill(...) must still be recognized as the same libc::kill(...) call the \
         allowed-path check trusts common.rs to hold"
    );

    // A token-stream-determinable spelling variant, measured against the
    // pinned toolchain rather than assumed: Rust's qualified-path syntax,
    // `<Type>::method(...)`, names the IDENTICAL item a bare
    // `Type::method(...)` does. `<Command>::new("cargo")` compiles under
    // 1.94.0. The plain token sequence `Command`, `::`, `new`, `(` breaks
    // immediately on the real spelling `<Command>::new(` (the token right
    // after `Command` is `>`, not `::`), so this must be recognized
    // through the alternate qualified-path start, not the bare-identifier
    // one.
    let angle_bracket_command_new_sample =
        "fn angle_bracket_command_new() {\n    <Command>::new(\"cargo\").arg(\"build\");\n}\n";
    let angle_bracket_command_new_violations =
        process_cleanup_violations_in(other_path, angle_bracket_command_new_sample).0;
    assert_eq!(
        angle_bracket_command_new_violations.len(),
        1,
        "<Command>::new(\"cargo\").arg(\"build\") must be recognized and banned exactly like \
         Command::new(\"cargo\").arg(\"build\"): {angle_bracket_command_new_violations:?}"
    );

    // The identical qualified-path form with a full module path inside
    // the brackets — the type name is still `Command`, just qualified.
    let full_path_qualified_command_new_sample = "fn full_path_qualified_command_new() {\n    \
             <std::process::Command>::new(\"cargo\").arg(\"build\");\n\
         }\n";
    let full_path_qualified_command_new_violations =
        process_cleanup_violations_in(other_path, full_path_qualified_command_new_sample).0;
    assert_eq!(
        full_path_qualified_command_new_violations.len(),
        1,
        "<std::process::Command>::new(\"cargo\").arg(\"build\") must be recognized and banned: \
         {full_path_qualified_command_new_violations:?}"
    );

    // The `<Type as Trait>::` form is NOT the actual target — it resolves
    // to the trait's own method, so a real evader would need a trait
    // implementation elsewhere that itself constructs the command
    // through a visible form, which moves the call rather than hiding
    // it; that dispatch question is residual, not something this scan
    // claims to defend against. Proven anyway because the same
    // last-path-segment read covers it INCIDENTALLY, with no extra
    // machinery built for it.
    let as_trait_qualified_command_new_sample = "fn as_trait_qualified_command_new() {\n    \
             <Command as SomeTrait>::new(\"cargo\").arg(\"build\");\n\
         }\n";
    let as_trait_qualified_command_new_violations =
        process_cleanup_violations_in(other_path, as_trait_qualified_command_new_sample).0;
    assert_eq!(
        as_trait_qualified_command_new_violations.len(),
        1,
        "<Command as SomeTrait>::new(\"cargo\").arg(\"build\") happens to be covered by the \
         same last-path-segment read as the bare qualified-path form, incidentally, not because \
         it was built for: {as_trait_qualified_command_new_violations:?}"
    );

    // A real, allowed metadata read must still be ALLOWED through the
    // qualified-path spelling too — narrowing the allowance must not
    // widen the ban to something that was never dangerous.
    let angle_bracket_metadata_sample = "fn angle_bracket_metadata() {\n    \
             <Command>::new(env!(\"CARGO\")).args([\"metadata\", \"--locked\"]);\n\
         }\n";
    let angle_bracket_metadata_violations =
        process_cleanup_violations_in(other_path, angle_bracket_metadata_sample).0;
    assert!(
        angle_bracket_metadata_violations.is_empty(),
        "<Command>::new(env!(\"CARGO\")).args([\"metadata\", ...]) must still be allowed: \
         {angle_bracket_metadata_violations:?}"
    );

    // The negative-kill-argument-formatting ban had token-aware matching
    // added with no planted sample exercising it — an unproven branch is
    // unproven regardless of how confident the code looks. Both
    // directions: the real shape must be caught, and an unrelated
    // `format!("...")` call must not be.
    let negative_kill_argument_format_sample = "fn negative_kill_argument_formatting() {\n    \
             let pgid_arg = format!(\"-{pid}\");\n\
         }\n";
    let negative_kill_argument_format_violations =
        process_cleanup_violations_in(other_path, negative_kill_argument_format_sample).0;
    assert_eq!(
        negative_kill_argument_format_violations.len(),
        1,
        "a format!(\"-{{...}}\") building a negative process-group PID argument must be caught: \
         {negative_kill_argument_format_violations:?}"
    );
    let unrelated_format_call_sample = "fn unrelated_format_call() {\n    \
             let message = format!(\"hello {name}\");\n\
         }\n";
    let unrelated_format_call_violations =
        process_cleanup_violations_in(other_path, unrelated_format_call_sample).0;
    assert!(
        unrelated_format_call_violations.is_empty(),
        "an unrelated format!(...) call whose string does not start with \"-{{\" must not be \
         flagged: {unrelated_format_call_violations:?}"
    );

    // The identical token-spacing tolerance for the `env!("CARGO")`
    // recognition, which used a fixed whole-text compare and so missed
    // `env! ( "CARGO" )` the same way a bare substring search misses a
    // spaced `Command::new(`. Both directions proven: an allowed metadata
    // read and a banned build, each written with token spacing.
    let token_spaced_env_cargo_metadata_sample = "fn token_spaced_cargo_metadata() {\n    \
             Command :: new (env ! ( \"CARGO\" )).args([\"metadata\", \"--locked\"]);\n\
         }\n";
    let token_spaced_env_cargo_metadata_violations =
        process_cleanup_violations_in(other_path, token_spaced_env_cargo_metadata_sample).0;
    assert!(
        token_spaced_env_cargo_metadata_violations.is_empty(),
        "a token-spaced env ! ( \"CARGO\" ) metadata read must be recognized as cargo and \
         allowed, exactly like its compact spelling: {token_spaced_env_cargo_metadata_violations:?}"
    );
    let token_spaced_env_cargo_build_sample = "fn token_spaced_cargo_build() {\n    \
             Command :: new (env ! ( \"CARGO\" )).args([\"build\"]);\n\
         }\n";
    let token_spaced_env_cargo_build_violations =
        process_cleanup_violations_in(other_path, token_spaced_env_cargo_build_sample).0;
    assert_eq!(
        token_spaced_env_cargo_build_violations.len(),
        1,
        "a token-spaced env ! ( \"CARGO\" ) build must still be recognized as cargo and banned: \
         {token_spaced_env_cargo_build_violations:?}"
    );

    // The identical token-spacing tolerance for the same-call-argument
    // helper prefixes (`command_output(`/`command_status(`), proven on
    // the container-runtime axis: a token-spaced real inspect must be
    // allowed, and a token-spaced build must still be banned.
    let token_spaced_runtime_inspect_sample = format!(
        "fn token_spaced_runtime_inspect() {{\n    \
             let _ = command_output (\"{runtime_program}\", [\"inspect\"]);\n\
         }}\n"
    );
    let token_spaced_runtime_inspect_violations =
        process_cleanup_violations_in(other_path, &token_spaced_runtime_inspect_sample).0;
    assert!(
        token_spaced_runtime_inspect_violations.is_empty(),
        "a token-spaced command_output (...) real inspect call must be recognized and allowed: \
         {token_spaced_runtime_inspect_violations:?}"
    );
    let token_spaced_runtime_build_sample = format!(
        "fn token_spaced_runtime_build() {{\n    \
             let _ = command_output (\"{runtime_program}\", [\"build\"]);\n\
         }}\n"
    );
    let token_spaced_runtime_build_violations =
        process_cleanup_violations_in(other_path, &token_spaced_runtime_build_sample).0;
    assert_eq!(
        token_spaced_runtime_build_violations.len(),
        1,
        "a token-spaced command_output (...) build must still be recognized and banned: \
         {token_spaced_runtime_build_violations:?}"
    );

    // The turbofish axis: `Command::new::<&str>("cargo")` is the exact
    // same call as `Command::new("cargo")` with the caller writing out
    // the argument type Rust would otherwise infer — confirmed to compile
    // clean under `-D warnings` on the pinned toolchain rather than
    // assumed. Marker recognition consumes an optional turbofish right
    // before the call's own opening `(` (see [`skip_optional_turbofish`]),
    // so a turbofish-spelled build is banned exactly like the bare form,
    // and a turbofish-spelled metadata read stays allowed.
    let turbofish_command_new_build_sample = "fn turbofish_command_new_build() {\n    \
             Command::new::<&str>(\"cargo\").args([\"build\"]);\n\
         }\n";
    let turbofish_command_new_build_violations =
        process_cleanup_violations_in(other_path, turbofish_command_new_build_sample).0;
    assert_eq!(
        turbofish_command_new_build_violations.len(),
        1,
        "Command::new::<&str>(\"cargo\").args([\"build\"]) must be recognized and banned \
         exactly like Command::new(\"cargo\").args([\"build\"]): \
         {turbofish_command_new_build_violations:?}"
    );
    let turbofish_command_new_metadata_sample = "fn turbofish_command_new_metadata() {\n    \
             Command::new::<&str>(env!(\"CARGO\")).args([\"metadata\", \"--locked\"]);\n\
         }\n";
    let turbofish_command_new_metadata_violations =
        process_cleanup_violations_in(other_path, turbofish_command_new_metadata_sample).0;
    assert!(
        turbofish_command_new_metadata_violations.is_empty(),
        "Command::new::<&str>(env!(\"CARGO\")).args([\"metadata\", ...]) must still be \
         allowed: {turbofish_command_new_metadata_violations:?}"
    );

    // The raw-shell-kill axis inherits the identical tolerance through the
    // same shared `["Command", "::", "new", "("]` sequence.
    let turbofish_raw_shell_kill_sample =
        "fn turbofish_raw_shell_kill() {\n    Command::new::<&str>(\"kill\");\n}\n";
    let turbofish_raw_shell_kill_violations =
        process_cleanup_violations_in(other_path, turbofish_raw_shell_kill_sample).0;
    assert_eq!(
        turbofish_raw_shell_kill_violations.len(),
        1,
        "Command::new::<&str>(\"kill\") must still be recognized as a raw shell kill and \
         banned: {turbofish_raw_shell_kill_violations:?}"
    );

    // The same-call-argument helper axis (`command_output`/
    // `command_status`) is itself genuinely generic, so its own turbofish
    // spelling must be tolerated the same way.
    let turbofish_command_output_build_sample = format!(
        "fn turbofish_command_output_build() {{\n    \
             let _ = command_output::<_, _>(\"{runtime_program}\", [\"build\"]);\n\
         }}\n"
    );
    let turbofish_command_output_build_violations =
        process_cleanup_violations_in(other_path, &turbofish_command_output_build_sample).0;
    assert_eq!(
        turbofish_command_output_build_violations.len(),
        1,
        "command_output::<_, _>(...) build must still be recognized and banned: \
         {turbofish_command_output_build_violations:?}"
    );
}
