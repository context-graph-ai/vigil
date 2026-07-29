//! Vigil's distributed-compute fabric uses ContextDB's typed authenticated
//! peer endpoint in production. Arbitrary transport injection is test-only,
//! and ContextDB owns sync declarations and arbitration.

use std::fs;
use std::path::PathBuf;

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::{PRODUCTION_CRATES, crates_root, lex, rust_sources, string_literals};

fn fabric_source() -> (PathBuf, String) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("fabric.rs");
    let source = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    (path, source)
}

fn compact_live_code(source: &str) -> String {
    lex(source)
        .masked
        .into_iter()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn ddl_literal(literal: &str) -> bool {
    let upper = literal.trim_start().to_ascii_uppercase();
    [
        "CREATE TABLE",
        "CREATE INDEX",
        "CREATE UNIQUE INDEX",
        "ALTER TABLE",
        "DROP TABLE",
        "SYNC TWO WAY",
        "SYNC OFF",
        "SYNC CONFLICT",
    ]
    .iter()
    .any(|prefix| upper.starts_with(prefix))
}

#[test]
fn vigil_production_fabric_uses_authenticated_iroh() {
    let (fabric_path, source) = fabric_source();
    let code = compact_live_code(&source);

    for required in [
        "contextdb_server::peer_bind_spec(&identity_path)",
        "contextdb_server::PeerEndpoint::bind(&bind_spec)",
        "contextdb_server::peer_dial_spec(",
        "SyncClient::new(",
        "contextdb_server::SyncServer::",
    ] {
        assert!(
            code.contains(required),
            "{} must construct the production fabric through ContextDB's typed authenticated \
             peer endpoint; missing live code `{required}`",
            fabric_path.display()
        );
    }

    for forbidden in [
        "SyncServer::with_transport(",
        "SyncClient::with_transport(",
        "ConflictPolicies",
        "ConflictPolicy",
        "set_conflict_policy(",
        "set_table_direction(",
    ] {
        assert!(
            !code.contains(forbidden),
            "{} must not select an arbitrary transport or supply a sync-policy map in production; \
             found live code `{forbidden}`",
            fabric_path.display()
        );
    }

    let crate_roots = crates_root();
    let mut ddl = Vec::new();
    for crate_name in PRODUCTION_CRATES {
        let src_root = crate_roots.join(crate_name).join("src");
        assert!(
            src_root.is_dir(),
            "expected production source tree {}",
            src_root.display()
        );
        let sources = rust_sources(&src_root);
        assert!(
            !sources.is_empty(),
            "expected Rust sources under {}",
            src_root.display()
        );
        for path in sources {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            for literal in string_literals(&text)
                .into_iter()
                .filter(|value| ddl_literal(value))
            {
                let relative = path.strip_prefix(&crate_roots).unwrap_or(&path);
                ddl.push(format!("{}: {literal:?}", relative.display()));
            }
        }
    }
    assert!(
        ddl.is_empty(),
        "Vigil must not author Context Graph or work-ledger DDL; ContextDB owns those \
         declarations. Found: {ddl:?}"
    );
}
