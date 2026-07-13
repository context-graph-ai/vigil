//! Transport purity (criterion C9, §10 rule 2): no vigil code names the
//! transport. vigil reaches its peer transport ONLY through contextdb's
//! transport-neutral surface (`contextdb_server::PeerEndpoint`,
//! `PeerEndpointSpec`, `peer_bind_spec`, `peer_dial_spec`) — it never
//! depends on the concrete transport crate directly, never imports it bare,
//! and never spells the concrete transport's own name anywhere in its own
//! source, not even in a qualified path or a scheme-string literal.
//!
//! Pattern-matches `oss_cleanliness_scan.rs`'s source-scan idiom: walk the
//! crate's own `src/`, strip comments, and flag any occurrence of the
//! transport name — a guard authored post-implementation is expected for
//! C9 (mapped to the acceptance commit from the start).

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

/// No mention of "iroh" is allowed anywhere in vigil's own source: not a
/// bare `use iroh::` of the external crate, not a qualified reference to
/// contextdb's adapter module, and not the transport's own URI scheme
/// string. vigil consumes contextdb's transport-neutral surface
/// (`PeerEndpoint` / `PeerEndpointSpec` / `peer_bind_spec` /
/// `peer_dial_spec`) exclusively, so it never needs to spell the concrete
/// transport's name at all.
fn line_names_the_transport(line: &str) -> bool {
    let code = line.split("//").next().unwrap_or("");
    code.to_ascii_lowercase().contains("iroh")
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
        "vigil source must never name the transport at all — consume contextdb's \
         transport-neutral surface instead (criterion C9 / §10 rule 2):\n{}",
        violations.join("\n")
    );
}

/// The guard trips on ANY "iroh" mention, qualified or not, code or
/// scheme-string — vigil's transport-neutral surface
/// (`contextdb_server::PeerEndpoint`, `peer_bind_spec`) never needs one.
#[test]
fn guard_catches_every_shape_of_the_transport_name() {
    assert!(!line_names_the_transport(
        "use contextdb_server::PeerEndpoint;"
    ));
    assert!(!line_names_the_transport(
        "        let bind_spec = contextdb_server::peer_bind_spec(&identity_path);"
    ));
    assert!(line_names_the_transport("use iroh::Endpoint;"));
    assert!(line_names_the_transport(
        "struct IrohRelayHandle { endpoint: iroh::Endpoint }"
    ));
    assert!(line_names_the_transport(
        "use contextdb_server::transport::iroh::IrohServer;"
    ));
    assert!(line_names_the_transport(
        "        let bind_spec = format!(\"iroh:?identity={}\", identity_path.display());"
    ));
}
