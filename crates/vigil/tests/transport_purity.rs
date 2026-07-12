//! Transport purity (criterion C9, §10 rule 2): no vigil code names the
//! transport. vigil reaches iroh ONLY through contextdb's own adapter
//! (`contextdb_server::transport::iroh`, and the `iroh:?...` bind-spec
//! string scheme that adapter's public API itself defines) — it never
//! depends on the `iroh` crate directly, never imports it bare, and never
//! authors its own iroh-named symbol.
//!
//! Pattern-matches `oss_cleanliness_scan.rs`'s source-scan idiom: walk the
//! crate's own `src/`, strip comments, and flag disallowed occurrences of
//! the transport name — a guard authored post-implementation is expected
//! for C9 (mapped to the acceptance commit from the start).

use std::fs;
use std::path::{Path, PathBuf};

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
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
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Allowed mentions of "iroh" in vigil's own source: a qualified reference
/// to contextdb's adapter module (`transport::iroh`, contextdb's own
/// naming, reached only via its public path — never a bare `use iroh::` of
/// the external crate), or the `iroh:?...` bind-spec/dial-spec URI scheme
/// string that same adapter's public `bind`/dial API defines and requires
/// callers to pass verbatim (an external contract's literal scheme name,
/// not a vigil-authored transport abstraction).
fn line_names_the_transport(line: &str) -> bool {
    let code = line.split("//").next().unwrap_or("");
    if !code.to_ascii_lowercase().contains("iroh") {
        return false;
    }
    let allowed = ["transport::iroh", "\"iroh:?"];
    !allowed.iter().any(|pattern| code.contains(pattern))
}

#[test]
fn no_iroh_symbol_in_vigil_src() {
    // Unfakeable because it scans this crate's real `src/` tree (not a
    // fixture copy), so an accidental `use iroh::...` or a new vigil-side
    // iroh-named type anywhere in the tree fails this test, not just at the
    // one call site the author remembered to check.
    let src_dir = crate_root().join("src");
    let mut violations = Vec::new();
    for path in rust_sources(&src_dir) {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let relative = path
            .strip_prefix(crate_root())
            .unwrap_or(&path)
            .display()
            .to_string();
        for (line_no, line) in text.lines().enumerate() {
            if line_names_the_transport(line) {
                violations.push(format!("{relative}:{}: {}", line_no + 1, line.trim()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "vigil source must reach iroh only through contextdb's adapter, never name the \
         transport itself (criterion C9 / §10 rule 2):\n{}",
        violations.join("\n")
    );
}

/// The guard's own allow-list is exercised both ways: a qualified
/// contextdb-adapter reference and the bind-spec scheme string must NOT
/// trip the guard, while a bare `use iroh::` (the disallowed shape this
/// guard exists to catch) MUST.
#[test]
fn guard_allows_contextdb_adapter_and_bind_spec_but_not_bare_iroh() {
    assert!(!line_names_the_transport(
        "use contextdb_server::transport::iroh::IrohServer;"
    ));
    assert!(!line_names_the_transport(
        "        let bind_spec = format!(\"iroh:?identity={}\", identity_path.display());"
    ));
    assert!(line_names_the_transport("use iroh::Endpoint;"));
    assert!(line_names_the_transport(
        "struct IrohRelayHandle { endpoint: iroh::Endpoint }"
    ));
}
