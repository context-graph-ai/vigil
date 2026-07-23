//! Proves `AutomationHandle` never regains the owner-only pin/unpin
//! capability: no `impl` block for the type may define `set_manual` or
//! `return_to_automatic`.
//!
//! Privacy alone does not prove this. The two `compile_fail` doctests
//! beside `AutomationHandle` in `settings.rs` prove only that no caller
//! OUTSIDE this crate can reach an `AutomationHandle` at all — every path
//! to one runs through `SettingsRegistry::new`/`declare`, both
//! `pub(crate)`, so an external doctest fails to compile on
//! `SettingsRegistry::new()` itself, before it ever reaches
//! `handle.automation()` or the method call after it. That would stay
//! green even if `set_manual` were added straight back to
//! `AutomationHandle`, because the privacy error fires first and the
//! method-lookup line is never actually reached. This scan is the thing
//! that actually proves method absence, lexically, over the type's own
//! `impl` blocks in `crates/vigil/src`.
//!
//! Shares the comment/string-literal lexer with `environment_read_surface.rs`,
//! `cli_secret_flag_surface.rs`, and `settings_registry_declare_boundary.rs`
//! via `source_scan_lexer.rs`.
//!
//! Also resolves a DIRECT type alias of `AutomationHandle` before deciding
//! which `impl` headers name the watched type: without that, `impl
//! AutomationHandleAlias { fn set_manual ... }` compiles, reaches the
//! private owner handle exactly like an impl on `AutomationHandle` itself
//! would, and the header-text match this scan runs never sees the literal
//! identifier `AutomationHandle` at all, so it stayed green through the
//! alias.
//!
//! ## The enumeration
//!
//! This is the deliverable, not just the code: every DIRECT syntactic form
//! by which an `impl` block's own header can be made to name
//! `AutomationHandle`, worked out from Rust's grammar (verified against
//! `rustc 1.94.0`, not guessed), each closed and each proven to bite with a
//! planted-fault canary that shows the relevant resolution OFF (a real,
//! reproducible miss) and back ON (a real, reproducible catch) — never a
//! born-green assertion.
//!
//! **Covered — the `impl` header directly names the type, no alias in play:**
//! 1. A plain inherent impl (`impl AutomationHandle { ... }` / `impl<'a, T>
//!    AutomationHandle<'a, T> { ... }`).
//! 2. A trait impl (`impl SomeTrait for AutomationHandle<'a, T> { ... }`) —
//!    the header text is scanned whole, "for" included, so either impl
//!    shape is caught the same way.
//! 3. A path-qualified impl target with no alias at all (`impl
//!    crate::settings::AutomationHandle<'a, T> { ... }`) — the identifier
//!    walk finds `AutomationHandle` as the path's own final segment; this
//!    was already caught by the base, alias-unaware header scan before any
//!    alias work existed, and is proven directly rather than assumed.
//! 4. An impl carrying its own trailing `where` clause (`impl<'a, T>
//!    AutomationHandle<'a, T> where T: Clone { ... }`) — the header-to-`{`
//!    search does not stop at the `where` keyword, so the clause is just
//!    more scanned header text; the target identifier is still found.
//!
//! **Covered — via a DIRECT type alias (one indirection level, RHS or
//! generics of the alias itself literally names the target):**
//! 5. Bare (`type AutomationHandleAlias = AutomationHandle<...>;`).
//! 6. Generic-preserving (`type Foo<'a, T> = AutomationHandle<'a, T>;`).
//! 7. Declared inside a nested module, referenced by its module-qualified
//!    path (`mod m { type X = super::AutomationHandle<'a, T>; } impl
//!    m::X<'a, T> { ... }`).
//! 8. A path-qualified right-hand side (`type X = crate::settings::
//!    AutomationHandle<'a, T>;`) — matched by exact token equality, never a
//!    substring test (a decoy test locks this down: a type merely named
//!    `MyAutomationHandleWrapper` is never mistaken for the watched type or
//!    an alias of it).
//! 9. A DIRECT alias declared with a pre-`=` `where` clause (`type
//!    Foo<'a, T> where T: Clone = AutomationHandle<'a, T>;`) — stable,
//!    valid Rust (`rustc` only lints that the bound itself goes
//!    unenforced, tracked at rust-lang/rust#112792; the alias still
//!    declares and resolves exactly as if the clause were absent). The
//!    original alias scan stopped as soon as the character after the
//!    generics wasn't `=`, so it walked straight past this form without
//!    ever finding the `=` at all — the demonstrated bypass this form
//!    closes.
//! 10. A DIRECT alias whose own generic-parameter list gives one parameter
//!     a default value that names the target (`type X<T = AutomationHandle>
//!     = T;`), used bare in the `impl` header (`impl X { ... }`) so Rust
//!     fills the omitted argument from the default — verified to compile
//!     and to resolve `Self` to the defaulted type on `rustc 1.94.0`, the
//!     "generic impl whose bound resolves to the type" form. The original
//!     alias scan only ever inspected the alias's RHS (the text after
//!     `=`); it never looked inside the alias's own `<...>` generics list,
//!     so a default hiding there was invisible even though the RHS itself
//!     (bare `T`) never mentions `AutomationHandle` at all.
//!
//! Forms 9 and 10 compose (a defaulted generic parameter plus a `where`
//! clause on the same alias) and are handled by the same code path.
//!
//! ## A second axis: identifier LEXICAL form, not syntactic form
//!
//! Every form above is about the SHAPE of the `impl`/`type` construct.
//! Orthogonal to that is the LEXICAL SPELLING of any one identifier
//! inside it — `AutomationHandle` itself, an alias name, a forbidden
//! method name.
//!
//! **Correction:** an earlier version of this document claimed Rust has
//! "exactly two" identifier lexical forms, regular and raw. That was
//! wrong, and the error was in the claim, not in any code: "regular"
//! itself spans ASCII *and* Unicode (`rustc`'s own
//! identifier grammar is `XID_Start XID_Continue* | _ XID_Continue+` per
//! the Unicode identifier profile UAX #31 — no ASCII restriction anywhere
//! in it), and the "exactly two" count was asserted without tracing that
//! grammar first. The fix below is NOT "add a third form to the list" —
//! enumerating forms is exactly the failure mode this file exists to stop.
//! It is closing the axis by matching the compiler's own grammar via the
//! compiler's own tooling, so there is no fourth form left to be found
//! later, because the rule is no longer a list anyone thought of.
//!
//! **The close: `unicode-ident` (the crate `rustc` itself uses for
//! `XID_Start`/`XID_Continue`), not a hand-rolled character predicate.**
//! `is_ident_start`/`is_ident_char` in `source_scan_lexer.rs` used to be
//! built on `char::is_alphabetic`/`char::is_alphanumeric` — themselves
//! Unicode-aware, which is exactly why this was not caught as a gap by
//! inspection: most Unicode letters (Greek, Cyrillic, CJK, ...) already
//! passed, so a plausible-looking "Unicode identifier" canary using an
//! ordinary letter would have been BORN GREEN and proved nothing about
//! whether the fix mattered. The real, verified divergence is narrower and
//! sharper: a bare Unicode combining mark (e.g. U+0301 COMBINING ACUTE
//! ACCENT, U+0303 COMBINING TILDE) is Unicode category Mark, not Letter or
//! Number, so `char::is_alphanumeric` returns `false` for it — but it IS
//! `XID_Continue`, and `rustc 1.94.0` genuinely accepts a base letter plus
//! a following combining mark as one identifier (verified by actually
//! compiling one; there is no `\u{}` escape in identifier syntax, only in a
//! char/string literal, so that spelling is not itself compilable Rust).
//! `is_ident_start`/
//! `is_ident_char` now call `unicode_ident::is_xid_start`/
//! `is_xid_continue` directly (plus the one explicit `_` case Rust's own
//! grammar carves out, since `_` is `XID_Continue` but deliberately not
//! `XID_Start`) — see their own doc comments in `source_scan_lexer.rs` for
//! the full design note. `unicode-ident` is a new `crates/vigil`
//! dev-dependency (test-only; `source_scan_lexer.rs` is compiled into test
//! binaries via `#[path]`, never into the library); the dependency-
//! direction guard (`dependency_direction_contract.rs`) forbids only
//! substrate and transport-adapter crates by name, not general utilities,
//! and stays green with it present, and `--locked` builds succeed against
//! the regenerated lockfile.
//!
//! This axis composes with EVERY syntactic form above (any alias name, in
//! any of forms 5-10, could be spelled with a Unicode identifier, raw or
//! not) rather than being an eleventh form of its own — the same relation
//! the raw-identifier finding already established, now widened to cover
//! the ASCII/Unicode split within "regular" too.
//!
//! Two of the four sibling scans had their OWN locally hand-rolled
//! identifier-scan loops (`environment_read_surface.rs`'s
//! `find_call_sites` and `find_wrapper_call_sites`;
//! `settings_registry_declare_boundary.rs`'s `declare_call_positions`) —
//! duplicating, rather than calling, the shared reader. Fixing the shared
//! reader alone did not fix those three call sites; each was found and
//! rewritten to delegate to `read_ident_forward` instead, which is the
//! only way "fixed once, at the shared lexer" is actually true rather
//! than aspirational. `free_port_call_sites.rs` already delegated
//! correctly and needed no such change. The raw-identifier fix and the
//! Unicode grammar fix share this exact same set of consumers, since both
//! live in the same two functions.
//!
//! This file's own raw-identifier canary and its Unicode-identifier canary
//! (a combining-mark alias name, built with `format!` and an explicit
//! `\u{0303}` escape rather than typed literally, so this test file's own
//! source is never at the mercy of an editor silently NFC-normalizing the
//! very divergence being tested) both live inside
//! [`detector_catches_the_reviewed_type_alias_bypass_that_the_target_type_only_check_missed`]
//! (respellings of the bare alias, form 5), each verified to bite the same
//! before/after way every syntactic form above is.
//!
//! **Weight differs by scan, and it is stated here rather than left
//! implicit:** for THIS file, both the raw-identifier and Unicode-identifier
//! findings are now belt-and-braces, same as the syntactic-form scan
//! itself is relative to `SettingCore` (see the section below) —
//! `AutomationHandle` cannot reach `set_manual`/`return_to_automatic`
//! structurally, regardless of what any alias of it is named, ASCII or
//! Unicode, raw or not. For `environment_read_surface.rs` and
//! `settings_registry_declare_boundary.rs`, which have no such type
//! backstop, both findings are LOAD-BEARING: a Unicode-named binding
//! (`let π_password = std::env::var(...)`) or a Unicode-named `declare`
//! qualifier really would have evaded the pre-fix scan.
//!
//! Each of the four sibling scans was checked for the raw-identifier form
//! AND, separately, for the Unicode-identifier form — never assumed
//! symmetric between the two, since they are different divergences from
//! different old behavior (a hard truncation at `#` vs. a narrower
//! ASCII-alphanumeric-adjacent gap):
//! - `environment_read_surface.rs` — genuinely fix-dependent for BOTH:
//!   verified by temporarily forcing the shared reader's raw-identifier
//!   branch off (raw) and, separately, `is_ident_char` back to
//!   `char::is_alphanumeric() || c == '_'` (Unicode), re-running its own
//!   canaries each time; both go red.
//! - `settings_registry_declare_boundary.rs` — genuinely fix-dependent for
//!   the raw-identifier form (verified the same way). No DISTINCT
//!   Unicode-identifier canary was constructed here, for a checked,
//!   structural reason rather than by omission: the only two identifiers
//!   this scan ever reads are the fixed, hardcoded literal names `declare`
//!   and `SettingsRegistry` themselves (via the exact same shared
//!   `read_ident_forward`/`read_ident_backward` `environment_read_surface.rs`'s
//!   Unicode canary already exercises and proves) — neither the receiver
//!   in `registry.declare(...)` nor the arguments in the UFCS form are
//!   ever read as identifiers at all by `is_settings_registry_declare_call`,
//!   so there is no ARBITRARY, user-chosen identifier position in this
//!   scan's own logic for a Unicode spelling to occupy the way an alias
//!   name or an enclosing function name is one elsewhere. The fix is
//!   inherited through the proven shared functions; there was no distinct
//!   code path left to construct a meaningful canary against.
//! - `free_port_call_sites.rs` — its raw-identifier canary passes even
//!   with that fix disabled, for a structural reason recorded in its own
//!   test's comment (its match has no backward-context requirement, so
//!   the pre-fix reader's truncation coincidentally re-syncs onto the
//!   untouched tail of the identifier and still matches) — a real,
//!   checked finding, not an assumption of symmetry. The same structural
//!   reason means a Unicode-identifier canary there would prove nothing
//!   either, so none was added.
//! - `cli_secret_flag_surface.rs` — checked again for the Unicode form
//!   specifically (not assumed to inherit the raw-identifier finding) and
//!   confirmed to be the SAME categorical non-applicability: its
//!   `flag_tokens` scanner requires `chars[i + 2].is_ascii_lowercase()` to
//!   even start recognizing a `--flag` token, and continues only through
//!   `is_ascii_lowercase() || is_ascii_digit() || '-'` — an ASCII-only
//!   pattern matcher over STRING LITERAL content, never a Rust identifier,
//!   raw or Unicode. A CLI flag's NAME is argv text, not a binding this
//!   crate's own code names; there is no Rust-identifier-lexing step in
//!   this scan's mechanism for `unicode-ident` to plug into. No canary was
//!   added here, for the same reason (and same rigor) as
//!   `transport_purity.rs` below — reporting the checked finding plainly
//!   rather than forcing a canary that would prove nothing.
//! - `transport_purity.rs` — re-checked, not assumed to still hold: its
//!   `text_names_the_transport` is still a plain case-insensitive SUBSTRING
//!   search (`text.to_ascii_lowercase().contains("iroh")`), unchanged.
//!   Any Unicode-named alias of the transport crate — `use iroh::Endpoint
//!   as π_transport;` — still requires the literal ASCII text `"iroh"` to
//!   appear somewhere in the `use` statement itself for the substring
//!   search to have anything to find; renaming what comes AFTER `as` to a
//!   Unicode identifier cannot remove the crate name that comes BEFORE it.
//!   Structurally immune to identifier spelling — ASCII, raw, or Unicode —
//!   by the same substring-matching mechanism, independent of whether it
//!   uses the shared identifier reader at all (it does not).
//!
//! **Residual — consciously NOT covered, with the reason:**
//! - Alias-of-an-alias (`type Second = AutomationHandleAlias;`) or a
//!   default-of-a-default at chain depth ≥2 — resolving an arbitrarily deep
//!   indirection chain has no natural stopping point for a lexical scan.
//! - Cross-module indirection via a `pub use ... as ...` re-export (rather
//!   than a `type` alias) — a different keyword form the alias scan does
//!   not recognize at all.
//! - A macro invocation that expands to `impl AutomationHandle { ... }`
//!   (e.g. `some_macro!(AutomationHandle);`) — seeing through macro
//!   expansion needs the compiler's own expansion pass, not a lexical scan
//!   over unexpanded source; same "no natural stopping point" class as an
//!   alias chain.
//! - A trailing (post-`=`) `where` clause on a type alias (`type X = Foo
//!   where T: Bound;`) — verified NOT valid on the stable channel this
//!   crate builds with: `rustc 1.94.0` rejects it outright ("where clauses
//!   are not allowed after the type for type aliases"; the form needs the
//!   unstable `lazy_type_alias` feature). Not a real evasion against this
//!   toolchain, so left unhandled rather than coded against a shape that
//!   cannot compile here.
//! - A default type parameter on the `impl` block itself (`impl<T = u8>
//!   Foo<T> { ... }`) — verified to be a hard compiler error on `rustc
//!   1.94.0` ("defaults for generic parameters are not allowed here"),
//!   already a `deny`-by-default future-incompatibility lint. Not a real
//!   evasion.
//! - A trait's associated-type default that names the target (`trait T {
//!   type X = AutomationHandle; }`) — an inherent impl cannot target a
//!   bare associated-type projection (`impl <Y as T>::X { ... }` is not
//!   legal Rust; E0118), so this does not open a distinct route to an
//!   inherent method the way a `type`-alias default does.
//!
//! Answering the two questions this file exists to make answerable
//! without re-deriving the above, one per axis:
//!
//! **Is there any remaining direct SYNTACTIC form that reaches
//! `AutomationHandle`?** No — every direct form (impl header literal,
//! single-level type-alias RHS, single-level type-alias generic default,
//! each with or without a `where` clause) is enumerated above and
//! covered; what remains is exactly the residual list, each entry either
//! genuinely unbounded (alias chains, re-export chains, macros) or not
//! valid Rust on this toolchain.
//!
//! **Is there any remaining identifier LEXICAL form that reaches
//! `AutomationHandle`?** No — but the reason is NOT "every form has been
//! enumerated," and stating it that way is the exact mistake this document
//! corrected above. The reason is that identifier acceptance in the shared
//! reader is now DEFINED as `rustc`'s own `XID_Start`/`XID_Continue` grammar
//! (via `unicode-ident`, the crate `rustc` itself uses), not a list of
//! spellings this file's authors thought of. Any identifier `rustc` accepts
//! — ASCII, raw, Unicode letters, Unicode combining marks, anything the
//! grammar admits, including forms nobody has spelled out here — the
//! reader accepts too, by construction, because it is running the same
//! rule. That is what makes "closed" durable here in a way "regular and
//! raw, exactly two" never was: the completeness anchor for a
//! text-property scan like this one is the language's own grammar via the
//! language's own tooling, never a hand-maintained list of forms, because
//! a list can always be wrong about its own completeness in a way nobody
//! notices until the next character class surfaces it.
//!
//! ## The type is the primary guarantee for its CURRENT shape; this scan is defense-in-depth
//!
//! As of the `SettingCore<T>` extraction (see `settings.rs`'s own module
//! doc and the doc comment on [`AutomationHandle`] / `SettingCore`),
//! `AutomationHandle` structurally borrows `&SettingCore<T>` and nothing
//! else, and `SettingCore` itself has no `set_manual` or
//! `return_to_automatic` — so, AS THE TYPE IS SHAPED TODAY, no accessor or
//! `Deref` added to `AutomationHandle` could expose either method, because
//! the type it borrows does not have them to expose. That is a claim about
//! the current shape of these two types, inspectable in source, not a
//! promise that survives every future edit: a later change to
//! `AutomationHandle`'s own field (back to a `SettingHandle<T>`, say)
//! would reopen the path immediately, and this scan — which watches
//! `impl` blocks for forbidden method names, never struct field
//! declarations — would not notice that specific change either. What
//! THIS scan does cover, load-bearing on its own terms: it catches
//! `set_manual`/`return_to_automatic` being added straight back onto
//! `AutomationHandle` (or a direct alias of it) by a future edit, in any
//! of the ten forms above, spelled with any identifier `rustc` itself
//! would accept — the moment it happens, lexically, every time this test
//! runs. A reader relying on this file should know the type-shape
//! argument does NOT originate here, and that neither it nor this scan
//! covers a changed field type — only a re-added method.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::{
    is_ident_start, lex, match_delim, read_ident_forward, rust_sources, skip_ws_forward,
};

/// The type this scan watches.
const TARGET_TYPE: &str = "AutomationHandle";

/// The owner-only capability methods that must never appear on the
/// automation-facing type.
const FORBIDDEN_METHODS: [&str; 2] = ["set_manual", "return_to_automatic"];

/// Every identifier found in masked source at or after `start` and before
/// `end`, in source order — used both to find an `impl` block's own target
/// type in its header and to walk a found block's body for a forbidden
/// method name.
fn identifiers_in(masked: &[char], start: usize, end: usize) -> Vec<String> {
    let mut idents = Vec::new();
    let mut i = start;
    while i < end {
        if is_ident_start(masked[i]) {
            let (ident, next) = read_ident_forward(masked, i).expect("checked ident start");
            idents.push(ident);
            i = next.max(i + 1);
        } else {
            i += 1;
        }
    }
    idents
}

/// Every `impl` block's body range `[open, close)` whose header (the text
/// between `impl` and the block's own opening `{`, generics included) names
/// any of `target_names` — an inherent impl (`impl<'a, T> AutomationHandle<'a,
/// T> {`) or a trait impl (`impl SomeTrait for AutomationHandle<'a, T>
/// {`) alike, since either shape could carry a method. A `<` inside the
/// header (a generic-args list) is tracked so it never gets mistaken for
/// the block's own opening brace. `target_names` is normally just
/// [`TARGET_TYPE`] alone, but a caller that has already resolved direct
/// type aliases of it (via [`direct_type_aliases_of`]) passes those in too,
/// so an `impl` written against the alias is caught exactly like one
/// written against `AutomationHandle` itself.
fn target_impl_body_ranges(
    masked: &[char],
    target_names: &BTreeSet<String>,
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
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
            match match_delim(masked, header_start, '<', '>') {
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
        if identifiers_in(masked, header_start, header_end)
            .iter()
            .any(|word| target_names.contains(word))
        {
            ranges.push((open, close));
        }
        i = close;
    }
    ranges
}

/// Whether the generic-parameter list `masked[open..=close]` (the alias's
/// own opening `<` at `open`, its matching closing `>` at `close`) gives
/// any parameter a DEFAULT value that names `target` — e.g. the `T =
/// AutomationHandle` in `type X<T = AutomationHandle> = T;`. A bound alone
/// (`T: SomeTrait<AutomationHandle>`) does not count: only text after a
/// TOP-LEVEL `=` inside the list (i.e. a real default, not a bound's own
/// generic argument) is scanned. This is what makes form 10 in the module
/// doc comment real: `type X<T = AutomationHandle> = T;` used bare as
/// `impl X { ... }` fills the omitted argument from this default, so `X`
/// resolves to `AutomationHandle` even though the alias's OWN right-hand
/// side (just `T`) never mentions the target at all.
fn generic_param_list_declares_default_of(
    masked: &[char],
    open: usize,
    close: usize,
    target: &str,
) -> bool {
    let mut i = open + 1;
    let mut depth = 0i32;
    while i < close {
        match masked[i] {
            '<' | '(' | '[' => {
                depth += 1;
                i += 1;
            }
            '>' | ')' | ']' => {
                depth -= 1;
                i += 1;
            }
            '=' if depth <= 0 => {
                let default_start = i + 1;
                let mut inner_depth = 0i32;
                let mut j = default_start;
                while j < close {
                    match masked[j] {
                        '<' | '(' | '[' => inner_depth += 1,
                        '>' | ')' | ']' => inner_depth -= 1,
                        ',' if inner_depth <= 0 => break,
                        _ => {}
                    }
                    j += 1;
                }
                if identifiers_in(masked, default_start, j)
                    .iter()
                    .any(|word| word == target)
                {
                    return true;
                }
                i = j;
            }
            _ => {
                i += 1;
            }
        }
    }
    false
}

/// Every type alias name declared anywhere in `masked` whose right-hand
/// side directly names `target` as one of its own identifiers — e.g. `type
/// AutomationHandleAlias = AutomationHandle;` or `type Foo<'a, T> =
/// AutomationHandle<'a, T>;` — OR whose own generic-parameter list gives a
/// parameter a default value naming `target` (see
/// [`generic_param_list_declares_default_of`]), which resolves to `target`
/// the same way when the alias is named bare. Also tolerates a DIRECT
/// alias declared with a pre-`=` `where` clause (`type X<T> where T: Bound
/// = AutomationHandle<T>;`, stable and valid — the clause is unenforced
/// but the alias still declares and resolves normally) by scanning past it
/// with the same depth-tracked technique used everywhere else in this file
/// to find a delimiter that might itself be nested inside angle brackets
/// (a bound like `T: Iterator<Item = U>` has its own `=` that must NOT be
/// mistaken for the one ending the clause). Only a DIRECT alias is
/// resolved: an alias of one of the names returned here is not walked
/// further (see the module doc comment's residual note).
fn direct_type_aliases_of(masked: &[char], target: &str) -> Vec<String> {
    let mut aliases = Vec::new();
    let n = masked.len();
    let mut i = 0usize;
    while i < n {
        if !is_ident_start(masked[i]) {
            i += 1;
            continue;
        }
        let (ident, end) = read_ident_forward(masked, i).expect("checked ident start");
        if ident != "type" {
            i = end;
            continue;
        }
        let name_start = skip_ws_forward(masked, end);
        let Some((alias_name, name_end)) = read_ident_forward(masked, name_start) else {
            i = end;
            continue;
        };
        let mut cursor = skip_ws_forward(masked, name_end);
        let mut generics_default_hit = false;
        if cursor < n && masked[cursor] == '<' {
            match match_delim(masked, cursor, '<', '>') {
                Some(close) => {
                    generics_default_hit =
                        generic_param_list_declares_default_of(masked, cursor, close, target);
                    cursor = close + 1;
                }
                None => {
                    i = end;
                    continue;
                }
            }
            cursor = skip_ws_forward(masked, cursor);
        }
        // Skip an optional pre-`=` where clause. `type X<T> where T: Bound
        // = AutomationHandle<T>;` is stable, valid Rust: rustc only lints
        // that the bound goes unenforced (rust-lang/rust#112792), the
        // alias itself declares and resolves exactly as if the clause
        // were absent. A where clause can carry its own angle-bracketed
        // generics (`T: Iterator<Item = U>`), so the `=`/`;` that
        // actually ends the clause is found by depth tracking, the same
        // technique the RHS scan below and the impl-header scan both use
        // to find their own terminator.
        if let Some((word, after_where)) = read_ident_forward(masked, cursor)
            && word == "where"
        {
            let mut depth = 0i32;
            let mut probe = after_where;
            while probe < n {
                match masked[probe] {
                    '<' | '(' | '[' => depth += 1,
                    '>' | ')' | ']' => depth -= 1,
                    '=' | ';' if depth <= 0 => break,
                    _ => {}
                }
                probe += 1;
            }
            cursor = probe;
        }
        if cursor >= n || masked[cursor] != '=' {
            i = end;
            continue;
        }
        let rhs_start = cursor + 1;
        let mut depth = 0i32;
        let mut rhs_end = rhs_start;
        while rhs_end < n {
            match masked[rhs_end] {
                '<' | '(' | '[' => depth += 1,
                '>' | ')' | ']' => depth -= 1,
                ';' if depth <= 0 => break,
                _ => {}
            }
            rhs_end += 1;
        }
        let rhs_hit = identifiers_in(masked, rhs_start, rhs_end)
            .iter()
            .any(|word| word == target);
        if rhs_hit || generics_default_hit {
            aliases.push(alias_name);
        }
        i = rhs_end;
    }
    aliases
}

/// Same detection as [`automation_handle_capability_violations`], but the
/// `impl` header match additionally counts any name in `extra_target_names`
/// as equivalent to [`TARGET_TYPE`] — used to fold in type aliases found by
/// [`direct_type_aliases_of`] so an `impl` written against the ALIAS is
/// caught exactly like one written against `AutomationHandle` itself.
fn automation_handle_capability_violations_with_aliases(
    file_label: &str,
    source: &str,
    extra_target_names: &BTreeSet<String>,
) -> Vec<String> {
    let mut target_names = extra_target_names.clone();
    target_names.insert(TARGET_TYPE.to_string());
    automation_handle_capability_violations_over(file_label, source, &target_names)
}

fn automation_handle_capability_violations_over(
    file_label: &str,
    source: &str,
    target_names: &BTreeSet<String>,
) -> Vec<String> {
    let masked = lex(source).masked;
    let mut violations = Vec::new();
    for (open, close) in target_impl_body_ranges(&masked, target_names) {
        for method in identifiers_in(&masked, open, close) {
            if FORBIDDEN_METHODS.contains(&method.as_str()) {
                violations.push(format!(
                    "{file_label}: an impl block for {TARGET_TYPE} (directly or via a direct \
                     type alias) contains `{method}` — the automation-facing capability must \
                     never regain the owner-only pin/unpin methods"
                ));
            }
        }
    }
    violations
}

/// This scan is core-only, deliberately: `AutomationHandle` and its impl
/// blocks live in `crates/vigil/src/settings.rs` alone, so there is no
/// sibling crate to widen into. `rust_sources` itself is the same shared
/// walker `environment_read_surface.rs`, `cli_secret_flag_surface.rs`, and
/// `transport_purity.rs` call to cover more than one crate; this scan just
/// points it at one `src/` directory instead of looping over
/// `PRODUCTION_CRATES`.
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn automation_handle_never_gains_a_manual_pin_or_unpin_method() {
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

    let mut labeled_sources = Vec::new();
    for path in sources {
        let label = path
            .strip_prefix(&src_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(source) = fs::read_to_string(&path) else {
            continue;
        };
        labeled_sources.push((label, source));
    }

    // First pass, over every file: collect direct type aliases of
    // AutomationHandle wherever one is declared, so an `impl` on the alias
    // in ANY file is caught, not just one declared in the same file the
    // alias itself lives in.
    let mut alias_names = BTreeSet::new();
    for (_, source) in &labeled_sources {
        let masked = lex(source).masked;
        alias_names.extend(direct_type_aliases_of(&masked, TARGET_TYPE));
    }
    let mut all_target_names = alias_names.clone();
    all_target_names.insert(TARGET_TYPE.to_string());

    let mut found_target_impl = false;
    let mut violations = Vec::new();
    for (label, source) in &labeled_sources {
        let masked = lex(source).masked;
        if !target_impl_body_ranges(&masked, &all_target_names).is_empty() {
            found_target_impl = true;
        }
        violations.extend(automation_handle_capability_violations_with_aliases(
            label,
            source,
            &alias_names,
        ));
    }
    assert!(
        found_target_impl,
        "no impl block for {TARGET_TYPE} (directly or via a direct type alias) was found \
         anywhere under {}; that smells like the scan silently walked an empty or wrong tree \
         rather than crates/vigil/src, so it would pass green while checking nothing",
        src_root.display()
    );
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

// ---------------------------------------------------------------------
// Planted-fault proof: the detector function itself, exercised directly
// (no real files touched), on samples that must be flagged and samples
// built only from allowed shapes that must not be.
// ---------------------------------------------------------------------

#[test]
fn detector_flags_a_planted_pin_method_on_automation_handle_and_leaves_the_owner_handle_alone() {
    let set_manual_sample = r#"
        impl<'a, T: Clone> AutomationHandle<'a, T> {
            pub fn apply_automatic(&self, value: T) -> Result<(), String> {
                Ok(())
            }

            pub fn set_manual(&self, value: T) -> Result<(), String> {
                Ok(())
            }
        }
    "#;
    let violations = automation_handle_capability_violations_with_aliases(
        "settings.rs",
        set_manual_sample,
        &BTreeSet::new(),
    );
    assert_eq!(
        violations.len(),
        1,
        "a planted set_manual method inside an AutomationHandle impl block must be flagged: \
         {violations:?}"
    );
    assert!(violations[0].contains("set_manual"));

    let return_to_automatic_sample = r#"
        impl<'a, T: Clone> AutomationHandle<'a, T> {
            pub fn return_to_automatic(&self) {}
        }
    "#;
    let return_to_automatic_violations = automation_handle_capability_violations_with_aliases(
        "settings.rs",
        return_to_automatic_sample,
        &BTreeSet::new(),
    );
    assert_eq!(
        return_to_automatic_violations.len(),
        1,
        "got {return_to_automatic_violations:?}"
    );
    assert!(return_to_automatic_violations[0].contains("return_to_automatic"));

    // A trait impl (`impl Trait for AutomationHandle`) must be caught the
    // same way an inherent impl is — either shape could carry a method.
    let trait_impl_sample = r#"
        impl<'a, T: Clone> SomeTrait for AutomationHandle<'a, T> {
            fn set_manual(&self, value: T) -> Result<(), String> {
                Ok(())
            }
        }
    "#;
    let trait_impl_violations = automation_handle_capability_violations_with_aliases(
        "settings.rs",
        trait_impl_sample,
        &BTreeSet::new(),
    );
    assert_eq!(
        trait_impl_violations.len(),
        1,
        "got {trait_impl_violations:?}"
    );
    assert!(trait_impl_violations[0].contains("set_manual"));

    // An impl block for a DIFFERENT type carrying the same method names
    // must not be flagged — this scan is specifically about
    // AutomationHandle, not every type in the crate. The owner-facing
    // SettingHandle keeps both methods legitimately.
    let owner_handle_sample = r#"
        impl<T: Clone> SettingHandle<T> {
            pub fn set_manual(&self, value: T) -> Result<(), String> {
                Ok(())
            }

            pub fn return_to_automatic(&self) {}
        }
    "#;
    let owner_violations = automation_handle_capability_violations_with_aliases(
        "settings.rs",
        owner_handle_sample,
        &BTreeSet::new(),
    );
    assert!(
        owner_violations.is_empty(),
        "the owner-facing SettingHandle keeps both methods legitimately and must not be flagged \
         by a scan that watches AutomationHandle specifically: {owner_violations:?}"
    );

    // A comment mentioning the forbidden names inside a REAL
    // AutomationHandle impl block must not be flagged — only live code
    // counts.
    let comment_only_sample = r#"
        impl<'a, T: Clone> AutomationHandle<'a, T> {
            // Deliberately no set_manual and no return_to_automatic here.
            pub fn apply_automatic(&self, value: T) -> Result<(), String> {
                Ok(())
            }
        }
    "#;
    let comment_violations = automation_handle_capability_violations_with_aliases(
        "settings.rs",
        comment_only_sample,
        &BTreeSet::new(),
    );
    assert!(
        comment_violations.is_empty(),
        "a comment mentioning the forbidden method names must not be flagged: \
         {comment_violations:?}"
    );

    // Form 3 (module doc comment): a path-qualified impl target with NO
    // alias in play at all — `impl crate::settings::AutomationHandle {
    // ... }`. The identifier walk over the header text finds
    // `AutomationHandle` as the path's own final segment regardless of the
    // `crate::settings::` prefix, so this needs no alias resolution; it is
    // caught by the same base, alias-unaware detector used above.
    let path_qualified_impl_target_sample = r#"
        impl<'a, T: Clone> crate::settings::AutomationHandle<'a, T> {
            pub fn set_manual(&self, value: T) -> Result<(), String> {
                Ok(())
            }
        }
    "#;
    let path_qualified_impl_target_violations =
        automation_handle_capability_violations_with_aliases(
            "settings.rs",
            path_qualified_impl_target_sample,
            &BTreeSet::new(),
        );
    assert_eq!(
        path_qualified_impl_target_violations.len(),
        1,
        "a path-qualified impl target naming AutomationHandle as its own final path segment, \
         with no alias at all, must be flagged: {path_qualified_impl_target_violations:?}"
    );
    assert!(path_qualified_impl_target_violations[0].contains("set_manual"));

    // Form 4 (module doc comment): an impl carrying its own trailing
    // `where` clause between the target type and the opening `{`. The
    // header-to-`{` search does not stop at the `where` keyword, so the
    // clause is scanned as ordinary header text and the target identifier
    // is still found in it.
    let impl_where_clause_sample = r#"
        impl<'a, T> AutomationHandle<'a, T> where T: Clone {
            pub fn return_to_automatic(&self) {}
        }
    "#;
    let impl_where_clause_violations = automation_handle_capability_violations_with_aliases(
        "settings.rs",
        impl_where_clause_sample,
        &BTreeSet::new(),
    );
    assert_eq!(
        impl_where_clause_violations.len(),
        1,
        "an impl block carrying its own trailing where clause must still be flagged: \
         {impl_where_clause_violations:?}"
    );
    assert!(impl_where_clause_violations[0].contains("return_to_automatic"));
}

/// Reproduces the exact bypass on a real copy of `settings.rs`'s
/// own text: `type AutomationHandleAlias = AutomationHandle; impl
/// AutomationHandleAlias { fn set_manual ...; fn return_to_automatic ... }`
/// compiles, reaches the private owner handle through the alias, and
/// exposes both owner-only methods to automation.
///
/// Proves BOTH sides of the fix in one committed test, calling the ONE
/// real detector, [`automation_handle_capability_violations_with_aliases`],
/// twice: once with an EMPTY alias set (a real, reproducible measurement
/// of what the header-only match alone sees — the shape this scan used
/// before alias resolution existed — not a narrated claim), which reports
/// nothing on the planted bypass, i.e. the evasion sails through
/// undetected; and once fed the aliases [`direct_type_aliases_of`]
/// actually finds in the planted text, which goes red on the identical
/// text (it reports both forbidden methods).
#[test]
fn detector_catches_the_reviewed_type_alias_bypass_that_the_target_type_only_check_missed() {
    let settings_path = crate_root().join("src").join("settings.rs");
    let settings_source = fs::read_to_string(&settings_path)
        .unwrap_or_else(|error| panic!("read {settings_path:?}: {error}"));
    assert!(
        automation_handle_capability_violations_with_aliases(
            "settings.rs",
            &settings_source,
            &BTreeSet::new()
        )
        .is_empty(),
        "canary host file must start with zero violations"
    );

    let mut planted = settings_source.clone();
    planted.push_str(
        "\n\
         type AutomationHandleAlias<'a, T> = AutomationHandle<'a, T>;\n\
         \n\
         impl<'a, T: Clone> AutomationHandleAlias<'a, T> {\n    \
             pub fn set_manual(&self, value: T) -> Result<(), String> {\n        \
                 Ok(())\n    \
             }\n\n    \
             pub fn return_to_automatic(&self) {}\n\
         }\n",
    );

    // Green with an empty alias set: the header-only match never sees an
    // `impl` header naming `AutomationHandleAlias` as a match for
    // `AutomationHandle`, so it reports nothing at all on the planted
    // bypass — the exact evasion this form takes.
    let before_fix = automation_handle_capability_violations_with_aliases(
        "settings.rs",
        &planted,
        &BTreeSet::new(),
    );
    assert!(
        before_fix.is_empty(),
        "documents the vulnerability this canary closes: with no alias resolved, the header-only \
         match must report nothing on the planted alias bypass, proving it is blind to this \
         evasion without alias resolution: {before_fix:?}"
    );

    // Red after the fix: alias resolution finds the planted direct alias,
    // and the alias-aware detector then treats an impl on the alias
    // exactly like one on AutomationHandle itself.
    let masked = lex(&planted).masked;
    let alias_names: BTreeSet<String> = direct_type_aliases_of(&masked, TARGET_TYPE)
        .into_iter()
        .collect();
    assert!(
        alias_names.contains("AutomationHandleAlias"),
        "the alias scan must find the planted AutomationHandleAlias: {alias_names:?}"
    );
    let after_fix =
        automation_handle_capability_violations_with_aliases("settings.rs", &planted, &alias_names);
    assert_eq!(
        after_fix.len(),
        2,
        "the alias-resolved detector must catch both forbidden methods on the impl targeting the \
         alias: {after_fix:?}"
    );
    assert!(
        after_fix
            .iter()
            .any(|violation| violation.contains("set_manual"))
    );
    assert!(
        after_fix
            .iter()
            .any(|violation| violation.contains("return_to_automatic"))
    );

    // Form 5 specifically: the truly BARE alias — no `<...>` on the alias
    // name at all (the block above is actually form 6, generic-preserving:
    // its alias name carries its own `<'a, T>`). A bare alias needs a
    // CONCRETE lifetime on the right-hand side (`'static` here) since the
    // alias itself declares no lifetime parameter to fill `AutomationHandle`'s
    // own `'a` — verified this compiles on `rustc 1.94.0` in isolation
    // (`type Bare = Foo<'static, u8>; impl Bare { ... }`); omitting the
    // concrete lifetime is a hard `E0261` "undeclared lifetime" error, not
    // a real alternative form. Same before/after bite proof as every other
    // form in this file.
    let mut bare_planted = settings_source.clone();
    bare_planted.push_str(
        "\n\
         type AutomationHandleBareAlias = AutomationHandle<'static, u64>;\n\
         \n\
         impl AutomationHandleBareAlias {\n    \
             pub fn set_manual(&self, value: u64) -> Result<(), String> {\n        \
                 Ok(())\n    \
             }\n\
         }\n",
    );
    let bare_before_fix = automation_handle_capability_violations_with_aliases(
        "settings.rs",
        &bare_planted,
        &BTreeSet::new(),
    );
    assert!(
        bare_before_fix.is_empty(),
        "with no alias resolved, the header-only match must report nothing on a planted BARE alias bypass: \
         {bare_before_fix:?}"
    );
    let bare_masked = lex(&bare_planted).masked;
    let bare_alias_names: BTreeSet<String> = direct_type_aliases_of(&bare_masked, TARGET_TYPE)
        .into_iter()
        .collect();
    assert!(
        bare_alias_names.contains("AutomationHandleBareAlias"),
        "the alias scan must find the planted bare AutomationHandleBareAlias: \
         {bare_alias_names:?}"
    );
    let bare_after_fix = automation_handle_capability_violations_with_aliases(
        "settings.rs",
        &bare_planted,
        &bare_alias_names,
    );
    assert_eq!(
        bare_after_fix.len(),
        1,
        "the alias-resolved detector must catch the forbidden method on the bare alias's impl: \
         {bare_after_fix:?}"
    );
    assert!(bare_after_fix[0].contains("set_manual"));

    // The identifier LEXICAL FORM axis (see the module doc comment): the
    // same bare alias, respelled with a raw identifier
    // (`r#AutomationHandleAlias`). `r#Name` and `Name` name the identical
    // thing in Rust — verified: `type Alias = Foo; type r#Alias = Bar;` is
    // rejected as "the name `Alias` is defined multiple times" (E0428) on
    // `rustc 1.94.0`, not accepted as two distinct aliases — so this must
    // be caught exactly like the plain-spelled bare alias just above.
    // Before the shared `read_ident_forward`/`read_ident_backward` in
    // `source_scan_lexer.rs` learned to recognize `r#`, `r#AutomationHandleAlias`
    // read as the one-character identifier `"r"` and the rest was silently
    // dropped — invisible to BOTH the type-alias RHS scan and the impl
    // header's own identifier walk, independent of any of forms 5-10 above
    // (this is a different axis, not an eleventh form: it composes with
    // ANY of them). Verified by temporarily forcing the reader's
    // raw-identifier branch off and re-running this exact assertion: it
    // goes red (`alias_names` comes back empty), the same way the sibling
    // scans' raw-identifier canaries do; restored before committing.
    let mut raw_identifier_planted = settings_source.clone();
    raw_identifier_planted.push_str(
        "\n\
         type r#AutomationHandleAlias = AutomationHandle<'static, u64>;\n\
         \n\
         impl r#AutomationHandleAlias {\n    \
             pub fn return_to_automatic(&self) {}\n\
         }\n",
    );
    let raw_identifier_before_fix = automation_handle_capability_violations_with_aliases(
        "settings.rs",
        &raw_identifier_planted,
        &BTreeSet::new(),
    );
    assert!(
        raw_identifier_before_fix.is_empty(),
        "with no alias resolved, the header-only match must report nothing on a planted raw-identifier alias \
         bypass: {raw_identifier_before_fix:?}"
    );
    let raw_identifier_masked = lex(&raw_identifier_planted).masked;
    let raw_identifier_alias_names: BTreeSet<String> =
        direct_type_aliases_of(&raw_identifier_masked, TARGET_TYPE)
            .into_iter()
            .collect();
    assert!(
        raw_identifier_alias_names.contains("AutomationHandleAlias"),
        "the alias scan must find the planted raw-identifier alias, under its STRIPPED \
         (canonical) name: {raw_identifier_alias_names:?}"
    );
    let raw_identifier_after_fix = automation_handle_capability_violations_with_aliases(
        "settings.rs",
        &raw_identifier_planted,
        &raw_identifier_alias_names,
    );
    assert_eq!(
        raw_identifier_after_fix.len(),
        1,
        "the alias-resolved detector must catch the forbidden method on the raw-identifier \
         alias's impl: {raw_identifier_after_fix:?}"
    );
    assert!(raw_identifier_after_fix[0].contains("return_to_automatic"));

    // The identifier LEXICAL FORM axis, corrected (see the module doc
    // comment): "regular" spans ASCII *and* Unicode, so a bare alias
    // spelled with a Unicode name must be caught too — lower weight here
    // than for the environment/secret-flag/transport scans, since
    // `SettingCore` (see `settings.rs`) backstops this file's own property
    // regardless of spelling, but the scan should still work correctly on
    // its own terms. Uses a combining mark (U+0303 COMBINING TILDE) for the
    // same reason `environment_read_surface.rs`'s Unicode canary does: it
    // is a REAL divergence from the old `char::is_alphabetic`-based reader
    // (a bare combining mark is Unicode category Mark, not Letter, so
    // `char::is_alphabetic` returns false for it, while
    // `unicode_ident::is_xid_continue` returns true and `rustc 1.94.0`
    // genuinely accepts the resulting name), built with `format!` and an
    // explicit `\u{0303}` escape so this test file's own source is never
    // at the mercy of NFD-to-NFC normalization erasing the divergence.
    // Confirmed by temporarily forcing `is_ident_char` back to
    // `char::is_alphanumeric() || c == '_'` and re-running this exact
    // assertion: it goes red (the alias scan finds only the truncated name
    // `contrasen`, never the full alias name), restored before committing.
    let unicode_alias_name = format!("contrasen{}aAlias", '\u{0303}');
    let mut unicode_identifier_planted = settings_source;
    unicode_identifier_planted.push_str(&format!(
        "\n\
         type {unicode_alias_name} = AutomationHandle<'static, u64>;\n\
         \n\
         impl {unicode_alias_name} {{\n    \
             pub fn return_to_automatic(&self) {{}}\n\
         }}\n"
    ));
    let unicode_identifier_before_fix = automation_handle_capability_violations_with_aliases(
        "settings.rs",
        &unicode_identifier_planted,
        &BTreeSet::new(),
    );
    assert!(
        unicode_identifier_before_fix.is_empty(),
        "with no alias resolved, the header-only match must report nothing on a planted Unicode-identifier \
         alias bypass: {unicode_identifier_before_fix:?}"
    );
    let unicode_identifier_masked = lex(&unicode_identifier_planted).masked;
    let unicode_identifier_alias_names: BTreeSet<String> =
        direct_type_aliases_of(&unicode_identifier_masked, TARGET_TYPE)
            .into_iter()
            .collect();
    assert!(
        unicode_identifier_alias_names.contains(&unicode_alias_name),
        "the alias scan must find the planted Unicode-identifier alias in full, not truncated \
         at the combining mark: {unicode_identifier_alias_names:?}"
    );
    let unicode_identifier_after_fix = automation_handle_capability_violations_with_aliases(
        "settings.rs",
        &unicode_identifier_planted,
        &unicode_identifier_alias_names,
    );
    assert_eq!(
        unicode_identifier_after_fix.len(),
        1,
        "the alias-resolved detector must catch the forbidden method on the Unicode-identifier \
         alias's impl: {unicode_identifier_after_fix:?}"
    );
    assert!(unicode_identifier_after_fix[0].contains("return_to_automatic"));
}

/// A DIRECT alias declared INSIDE a nested module, referenced from the
/// `impl` by its module-qualified path — a shape the forms proven above
/// did not use but which the same lexical mechanism must still resolve,
/// since the scan is flat over the whole file's tokens and does not track
/// module scoping either way.
///
/// "Bites" proof, not a born-green assertion: this plants the SAME text
/// through the ONE real detector,
/// [`automation_handle_capability_violations_with_aliases`], called twice
/// — once with an EMPTY alias set (resolution OFF, the exact pre-fix
/// behavior) and once with the alias set [`direct_type_aliases_of`]
/// actually finds in the sample (resolution ON) — and asserts the OFF
/// call stays silent (red: it would miss this if alias resolution were
/// ever removed) while the ON call catches it (green: restored). A test
/// that only asserted the ON call would be born green and would not
/// prove it can ever go red.
#[test]
fn detector_catches_a_direct_alias_declared_inside_a_nested_module_and_referenced_by_its_module_path()
 {
    let sample = r#"
        mod owner_only {
            pub type LocalAlias<'a, T> = super::AutomationHandle<'a, T>;
        }

        impl<'a, T: Clone> owner_only::LocalAlias<'a, T> {
            pub fn set_manual(&self, value: T) -> Result<(), String> {
                Ok(())
            }
        }
    "#;

    // Resolution OFF (the pre-fix shape): silent — proves this would be a
    // real miss if alias resolution regressed away.
    let resolution_off =
        automation_handle_capability_violations_with_aliases("nested.rs", sample, &BTreeSet::new());
    assert!(
        resolution_off.is_empty(),
        "resolution-off detector must miss a nested-module alias: {resolution_off:?}"
    );

    // Resolution ON: the alias scan finds `LocalAlias` even though its
    // declaration sits inside `mod owner_only` and its right-hand side is
    // itself path-qualified (`super::AutomationHandle`), because the scan
    // is a flat token walk over the whole file, not a module-scoped one.
    let masked = lex(sample).masked;
    let alias_names: BTreeSet<String> = direct_type_aliases_of(&masked, TARGET_TYPE)
        .into_iter()
        .collect();
    assert!(
        alias_names.contains("LocalAlias"),
        "the alias scan must find LocalAlias despite its nested-module declaration and \
         path-qualified right-hand side: {alias_names:?}"
    );
    let resolution_on =
        automation_handle_capability_violations_with_aliases("nested.rs", sample, &alias_names);
    assert_eq!(resolution_on.len(), 1, "got {resolution_on:?}");
    assert!(resolution_on[0].contains("set_manual"));
}

/// Three DIRECT aliases whose declaration carries extra syntax beyond the
/// bare/generic-preserving basics, each proven the same "bites" way:
/// calling [`automation_handle_capability_violations_with_aliases`] with
/// an EMPTY alias set (resolution OFF) must miss it, calling it with the
/// alias set [`direct_type_aliases_of`] actually finds (resolution ON)
/// must catch it.
///
/// - A right-hand side that is itself path-qualified
///   (`crate::settings::AutomationHandle`, no nested module in play) — the
///   identifier walk over the alias's right-hand side finds
///   `AutomationHandle` as the path's own final segment, a genuine token
///   match (exact identifier equality, never a substring test), not a
///   coincidence that would break under a differently-named alias.
/// - A pre-`=` `where` clause (form 9 in the module doc comment) — stable,
///   valid Rust (`rustc` only lints the bound itself unenforced); the
///   original alias scan required the character right after the generics
///   to be `=`, so it walked straight past the `where` keyword and never
///   found the alias at all — the demonstrated bypass this file closes.
/// - A generic-parameter default naming the target (form 10) — `type
///   LocalAlias<T = AutomationHandle> = T;` used bare in `impl LocalAlias {
///   ... }` fills the omitted argument from the default (verified to
///   compile and resolve on `rustc 1.94.0`); the alias's own right-hand
///   side (bare `T`) never mentions `AutomationHandle`, so only scanning
///   the RHS — what the alias scan did before this form was resolved —
///   misses it.
#[test]
fn detector_catches_a_direct_alias_whose_right_hand_side_is_path_qualified() {
    let sample = r#"
        type LocalAlias<'a, T> = crate::settings::AutomationHandle<'a, T>;

        impl<'a, T: Clone> LocalAlias<'a, T> {
            pub fn return_to_automatic(&self) {}
        }
    "#;

    let resolution_off = automation_handle_capability_violations_with_aliases(
        "qualified.rs",
        sample,
        &BTreeSet::new(),
    );
    assert!(
        resolution_off.is_empty(),
        "resolution-off detector must miss a path-qualified-RHS alias: {resolution_off:?}"
    );

    let masked = lex(sample).masked;
    let alias_names: BTreeSet<String> = direct_type_aliases_of(&masked, TARGET_TYPE)
        .into_iter()
        .collect();
    assert!(
        alias_names.contains("LocalAlias"),
        "the alias scan must find LocalAlias from a path-qualified right-hand side: \
         {alias_names:?}"
    );
    let resolution_on =
        automation_handle_capability_violations_with_aliases("qualified.rs", sample, &alias_names);
    assert_eq!(resolution_on.len(), 1, "got {resolution_on:?}");
    assert!(resolution_on[0].contains("return_to_automatic"));

    // Form 9: a DIRECT alias declared with a pre-`=` where clause — the
    // same alias bypass above, restated in its where-clause shape.
    let where_clause_sample = r#"
        type LocalAlias<'a, T> where T: Clone = AutomationHandle<'a, T>;

        impl<'a, T: Clone> LocalAlias<'a, T> {
            pub fn set_manual(&self, value: T) -> Result<(), String> {
                Ok(())
            }
        }
    "#;
    let where_masked = lex(where_clause_sample).masked;
    let where_alias_names: BTreeSet<String> = direct_type_aliases_of(&where_masked, TARGET_TYPE)
        .into_iter()
        .collect();
    assert!(
        where_alias_names.contains("LocalAlias"),
        "the fixed alias scan must find LocalAlias despite its pre-`=` where clause: \
         {where_alias_names:?}"
    );
    let where_resolution_on = automation_handle_capability_violations_with_aliases(
        "where_clause.rs",
        where_clause_sample,
        &where_alias_names,
    );
    assert_eq!(where_resolution_on.len(), 1, "got {where_resolution_on:?}");
    assert!(where_resolution_on[0].contains("set_manual"));

    // Form 10: a DIRECT alias whose own generic-parameter list defaults to
    // the target, used bare in the impl header. Mirrors AutomationHandle's
    // real `<'a, T>` shape: the lifetime is supplied explicitly at the
    // impl site (`LocalAlias<'x>`), the type parameter is left to its
    // default (verified to compile and resolve `Self` to the defaulted
    // type on `rustc 1.94.0`).
    let generic_default_sample = r#"
        type LocalAlias<'x, T = AutomationHandle<'x, u8>> = T;

        impl<'x> LocalAlias<'x> {
            pub fn return_to_automatic(&self) {}
        }
    "#;
    let default_masked = lex(generic_default_sample).masked;
    let default_alias_names: BTreeSet<String> =
        direct_type_aliases_of(&default_masked, TARGET_TYPE)
            .into_iter()
            .collect();
    assert!(
        default_alias_names.contains("LocalAlias"),
        "the fixed alias scan must find LocalAlias from its generic-parameter default: \
         {default_alias_names:?}"
    );
    let default_resolution_on = automation_handle_capability_violations_with_aliases(
        "generic_default.rs",
        generic_default_sample,
        &default_alias_names,
    );
    assert_eq!(
        default_resolution_on.len(),
        1,
        "got {default_resolution_on:?}"
    );
    assert!(default_resolution_on[0].contains("return_to_automatic"));
}

/// A decoy proving the match is genuinely by whole-identifier equality, not
/// a substring coincidence: an alias/impl pair whose name merely CONTAINS
/// `AutomationHandle` as a substring, but is lexically a different
/// identifier, must never be treated as the type itself and must never be
/// treated as an alias of it either (its right-hand side does not name
/// `AutomationHandle` at all).
#[test]
fn detector_does_not_treat_a_substring_lookalike_name_as_the_type_or_an_alias_of_it() {
    let sample = r#"
        struct MyAutomationHandleWrapper;

        impl MyAutomationHandleWrapper {
            pub fn set_manual(&self) {}
        }
    "#;
    let violations =
        automation_handle_capability_violations_with_aliases("decoy.rs", sample, &BTreeSet::new());
    assert!(
        violations.is_empty(),
        "a type whose name merely contains AutomationHandle as a substring must not be flagged: \
         {violations:?}"
    );
    let masked = lex(sample).masked;
    let aliases = direct_type_aliases_of(&masked, TARGET_TYPE);
    assert!(
        aliases.is_empty(),
        "a struct definition is not a type alias and must not be found by the alias scan: \
         {aliases:?}"
    );
}
