//! Freezes every call site of `deterministic_fixture_support::free_port` —
//! the port helper that binds a loopback socket, reads the assigned port,
//! and releases it immediately, leaving a window for something else to grab
//! the same port before the caller uses it. That function's own doc comment
//! already says a new process fixture must hold a `TcpPortReservation`
//! instead, but a comment cannot stop a new call site from being written —
//! only a check that fails the gate can. This file is that check: every
//! call site below is the exact, reviewed, currently-existing set; a call
//! site that is not already in [`FROZEN_FREE_PORT_CALL_SITES`] fails
//! [`free_port_call_sites_match_the_frozen_set`].
//!
//! The existing call sites keep their pre-existing race deliberately — this
//! file does not fix them and does not touch their bodies. Its only job is
//! to stop the set from growing silently.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::{
    PRODUCTION_CRATES, crates_root, lex, read_ident_forward, rust_sources, skip_ws_forward,
    workspace_root,
};

/// Whether `source` pulls in the released-port helper's home file by path
/// (`#[path = "…deterministic_fixture_support.rs"]`) — the only way a file
/// reaches its `free_port`, since the function is not re-exported from
/// anywhere else.
fn includes_released_port_helper(source: &str) -> bool {
    lex(source)
        .strings
        .iter()
        .any(|(_, _, content)| content.ends_with("deterministic_fixture_support.rs"))
}

/// Every call site of an identifier literally named `free_port` in
/// `source`, outside comments and string literals.
fn count_free_port_calls(source: &str) -> usize {
    let masked = lex(source).masked;
    let mut count = 0usize;
    let mut i = 0usize;
    while i < masked.len() {
        if let Some((ident, end)) = read_ident_forward(&masked, i) {
            if ident == "free_port" {
                let after = skip_ws_forward(&masked, end);
                if masked.get(after) == Some(&'(') {
                    count += 1;
                }
            }
            i = end;
        } else {
            i += 1;
        }
    }
    count
}

/// The exact current call sites of the released-port helper, as `(path
/// relative to the workspace root, call count)`. Grown only by a reviewed
/// edit to this list — never by a call site landing unnoticed. A file that
/// includes the helper module but never calls `free_port` (for example one
/// that only uses `TcpPortReservation` from the same module) is correctly
/// absent here.
const FROZEN_FREE_PORT_CALL_SITES: &[(&str, usize)] = &[
    ("crates/vigil-bin/tests/boot_readiness.rs", 3),
    ("crates/vigil-bin/tests/cameraless_worker.rs", 2),
    ("crates/vigil-bin/tests/capability_delivery.rs", 2),
    ("crates/vigil-bin/tests/two_process_fabric.rs", 20),
    ("crates/vigil-bin/tests/worker_intent_loudness.rs", 1),
];

fn observed_free_port_call_sites(root: &Path) -> BTreeMap<String, usize> {
    let mut observed = BTreeMap::new();
    let crates_root = crates_root();
    for crate_name in PRODUCTION_CRATES {
        for path in rust_sources(&crates_root.join(crate_name).join("tests")) {
            let Ok(source) = fs::read_to_string(&path) else {
                continue;
            };
            if !includes_released_port_helper(&source) {
                continue;
            }
            let count = count_free_port_calls(&source);
            if count > 0 {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                observed.insert(rel, count);
            }
        }
    }
    observed
}

#[test]
fn free_port_call_sites_match_the_frozen_set() {
    let observed = observed_free_port_call_sites(&workspace_root());
    let frozen: BTreeMap<String, usize> = FROZEN_FREE_PORT_CALL_SITES
        .iter()
        .map(|(path, count)| (path.to_string(), *count))
        .collect();
    assert_eq!(
        observed, frozen,
        "the released-port helper's call sites changed; a NEW call site must hold a \
         TcpPortReservation instead of calling free_port. If this diff removes or edits an \
         existing call site for a reviewed reason, update FROZEN_FREE_PORT_CALL_SITES to match."
    );
}

#[test]
fn scan_detects_a_planted_call_site_in_a_real_file() {
    // Unfakeable the same way the Home Assistant vocabulary guard is: the
    // canary is planted into a REAL file that already includes the helper
    // module but, as of this writing, never calls `free_port`, read through
    // the same path the guard above uses — never a synthetic string.
    let real_file = workspace_root().join("crates/vigil-bin/tests/correction_core_paths.rs");
    let source = fs::read_to_string(&real_file)
        .unwrap_or_else(|error| panic!("read {real_file:?}: {error}"));
    assert!(
        includes_released_port_helper(&source),
        "canary host file must include the released-port helper module for this proof to mean anything"
    );
    assert_eq!(
        count_free_port_calls(&source),
        0,
        "canary host file must start with zero free_port call sites"
    );

    let mut planted = source;
    planted.push_str("\nfn __free_port_canary_probe() { let _ = free_port(); }\n");
    assert_eq!(
        count_free_port_calls(&planted),
        1,
        "a newly added free_port call site must be detected by the scan"
    );

    // A raw-identifier spelling of the same call (`r#free_port()`) must be
    // counted exactly like `free_port()` — `r#free_port` is not a distinct
    // name, it is Rust's own escape hatch for spelling an ordinary name
    // next to a keyword, and calls the identical function (verified: `type
    // Alias = Foo; type r#Alias = Bar;` is rejected as "the name `Alias`
    // is defined multiple times", E0428, on `rustc 1.94.0`).
    //
    // Honest note on what this does and does not prove: unlike the
    // environment-read and declare-boundary scans' raw-identifier
    // canaries, this one does NOT depend on the shared reader's
    // raw-identifier fix to pass — checked by temporarily disabling that
    // fix and re-running this exact assertion, and it still went green.
    // The reason is structural, not a coincidence of this one input:
    // `count_free_port_calls` requires no backward context at all (just
    // "an identifier reading `free_port`, immediately followed by `(`"),
    // so even the PRE-FIX reader's truncation of `r#free_port` down to the
    // one-character identifier `"r"` leaves the scan re-syncing one
    // position later, right onto the untouched text `free_port(` — which
    // still matches on its own. A raw identifier is only a genuine evasion
    // against a check that requires something about what comes BEFORE the
    // name (a qualifier, a receiver, an impl-header match); a bare
    // name-plus-paren check was never actually at risk from this
    // particular form. Kept anyway as a positive proof that the scan's
    // real, current behavior is correct on this input — just not
    // mislabeled as evidence the fix was load-bearing here.
    let mut raw_identifier_planted = planted;
    raw_identifier_planted
        .push_str("\nfn __free_port_raw_identifier_probe() { let _ = r#free_port(); }\n");
    assert_eq!(
        count_free_port_calls(&raw_identifier_planted),
        2,
        "a raw-identifier spelling of a free_port call site must also be detected by the scan"
    );
}
