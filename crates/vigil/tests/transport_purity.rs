//! Transport purity (criterion C9, §10 rule 2): no vigil code names the
//! transport. vigil reaches its peer transport ONLY through contextdb's
//! transport-neutral surface (`contextdb_server::PeerEndpoint`,
//! `PeerEndpointSpec`, `peer_bind_spec`, `peer_dial_spec`) — it never
//! depends on the concrete transport crate directly, never imports it bare,
//! and never spells the concrete transport's own name anywhere in its own
//! source, not even in a qualified path or a scheme-string literal.
//!
//! Pattern-matches `oss_cleanliness_scan.rs`'s source-scan idiom: walk
//! every production crate's own `src/`, strip comments, and flag any
//! occurrence of the transport name — a guard authored post-implementation
//! is expected for C9 (mapped to the acceptance commit from the start).
//!
//! Covers `crates/vigil/src`, `crates/vigil-ha/src`, and
//! `crates/vigil-bin/src` alike, sharing the crate list and the source
//! walker with `environment_read_surface.rs` and
//! `cli_secret_flag_surface.rs` via `source_scan_lexer.rs`. `vigil-bin` is
//! the one crate that actually forwards the `fabric` feature into a linked
//! binary (`fabric = ["vigil/fabric"]` in its own manifest), so it is
//! exactly where a transport name would first appear if the boundary ever
//! slipped; a `vigil`-only scan would never see it.
//!
//! Also covers the DECLARATION side, and covers it completely rather than
//! only the one shape a rename can take: a manifest entry shaped like
//! `mesh = { package = "iroh" }` links the transport crate while every
//! `.rs` source file spells the local alias (`use mesh::Endpoint;`), never
//! the literal identifier `iroh` — so a scan that only ever walked `.rs`
//! files (as this one did before) never saw it. But the PLAINEST form of
//! all, `iroh = "..."` declared directly as the dependency KEY with no
//! rename at all, is just as invisible to an `.rs`-only scan, and a check
//! that inspects only the `package =` field would still miss it.
//! `no_declared_dependency_names_the_transport_crate_in_any_production_manifest`
//! reads each production crate's own `Cargo.toml`, plus the workspace root
//! `Cargo.toml` (where a `[workspace.dependencies]` entry consumed via
//! `.workspace = true` could carry either shape instead), and flags a
//! forbidden crate named EITHER as the dependency key itself OR in its
//! `package = "..."` field — walked recursively so every dependency table
//! Cargo recognizes is covered: `[dependencies]`, `[dev-dependencies]`,
//! `[build-dependencies]`, any `[target.'cfg(...)'.dependencies]` (and its
//! dev/build siblings, easy to skip in a naive walk and an ordinary place
//! to put a dependency), and `[workspace.dependencies]` — optional entries
//! included, since a forbidden dependency gated behind a feature is exactly
//! the case the declaration-level design already closes on the sibling
//! `dependency_direction_contract.rs` guard. Declaration-level, so the
//! rename is caught regardless of the local alias, and the bare form is
//! caught with no alias in play at all.
//!
//! Documented residual: this reads only this repository's own `Cargo.toml`
//! files, and only their own declared dependency tables. A vendored fork
//! whose OWN `Cargo.toml` renames itself (a path dependency at some
//! `vendor/mesh` directory whose `[package] name = "mesh"` is not "iroh" at
//! all, even though the code inside is a copy of iroh) is not caught — nor
//! is a rename of a rename (an alias that itself re-exports through a
//! second alias declared in a dependency this repository does not own).
//! Both are unenumerable from this repository's own manifests and are out
//! of scope for a manifest-declaration scan of them.

use std::fs;
use std::path::{Path, PathBuf};

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::{PRODUCTION_CRATES, crates_root, rust_sources, workspace_root};

/// True when `text` names the transport, case-insensitively, anywhere in
/// it — the one matcher both the `.rs` line scan and the manifest
/// declaration scan share, so "what counts as naming the transport" is
/// defined exactly once.
fn text_names_the_transport(text: &str) -> bool {
    text.to_ascii_lowercase().contains("iroh")
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
    text_names_the_transport(code)
}

/// Every line in `source` that names the transport, labeled `{label}:{line
/// number}: {trimmed line text}`. Pure so the real scan and the
/// planted-fault proofs below share one implementation.
fn scan_source_for_transport_name(label: &str, source: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (line_no, line) in source.lines().enumerate() {
        if line_names_the_transport(line) {
            violations.push(format!("{label}:{}: {}", line_no + 1, line.trim()));
        }
    }
    violations
}

// ---------------------------------------------------------------------
// Declaration-level scan: a forbidden crate named either as the
// dependency KEY itself (`iroh = "..."`, the plainest form, no rename in
// play at all) or in a `package = "..."` field under that key (`mesh = {
// package = "iroh" }`) — checked in every dependency table Cargo
// recognizes, wherever it appears in the document, so a
// `[target.'cfg(...)'.dependencies]` table or a `[workspace.dependencies]`
// table is covered exactly like a plain `[dependencies]` table is.
// ---------------------------------------------------------------------

/// The three table names Cargo treats as dependency tables, regardless of
/// where they are nested (root, `[target.'cfg(...)'.*]`, or
/// `[workspace.*]`).
const DEPENDENCY_TABLE_NAMES: [&str; 3] =
    ["dependencies", "dev-dependencies", "build-dependencies"];

/// Every forbidden-crate declaration found anywhere under a parsed
/// manifest, described as a human-readable hit. Recurses into every table
/// and array in the document; at each table, it inspects the entries of
/// any child table named [`DEPENDENCY_TABLE_NAMES`] two ways: the
/// dependency's own KEY (`iroh = "..."`), and, when the dependency is
/// itself an inline/dotted table, its `package = "..."` field (`mesh = {
/// package = "iroh" }`). This reaches `[dependencies]`,
/// `[dev-dependencies]`, `[build-dependencies]`, every
/// `[target.'cfg(...)'.dependencies]` (and its dev/build siblings), and
/// `[workspace.dependencies]` alike, because the recursion does not stop
/// at any particular depth or path — it only keys off the dependency-table
/// NAME, wherever that name occurs. Pure over an already-parsed
/// `toml::Value` so it can be exercised directly against hand-built
/// fixtures, not only against real manifest files.
fn manifest_forbidden_dependency_declarations(value: &toml::Value, hits: &mut Vec<String>) {
    if let toml::Value::Table(table) = value {
        for section_name in DEPENDENCY_TABLE_NAMES {
            let Some(toml::Value::Table(deps)) = table.get(section_name) else {
                continue;
            };
            for (dep_key, dep_value) in deps {
                if text_names_the_transport(dep_key) {
                    hits.push(format!(
                        "[{section_name}] declares \"{dep_key}\" directly as the dependency key"
                    ));
                }
                if let toml::Value::Table(dep_table) = dep_value
                    && let Some(toml::Value::String(package)) = dep_table.get("package")
                    && text_names_the_transport(package)
                {
                    hits.push(format!(
                        "[{section_name}] declares \"{dep_key}\" with package = \"{package}\""
                    ));
                }
            }
        }
        for nested in table.values() {
            manifest_forbidden_dependency_declarations(nested, hits);
        }
    } else if let toml::Value::Array(items) = value {
        for item in items {
            manifest_forbidden_dependency_declarations(item, hits);
        }
    }
}

/// Parses `manifest_source` as TOML and reports every forbidden-crate
/// dependency declaration, labeled `{label}: ...`. A manifest that fails
/// to parse is a loud failure, not a silent skip — a guard that cannot
/// read a real `Cargo.toml` must not pass by accident.
fn scan_manifest_for_forbidden_transport_declarations(
    label: &str,
    manifest_source: &str,
) -> Vec<String> {
    let parsed: toml::Value = toml::from_str(manifest_source)
        .unwrap_or_else(|error| panic!("{label} must parse as TOML: {error}"));
    let mut hits = Vec::new();
    manifest_forbidden_dependency_declarations(&parsed, &mut hits);
    hits.into_iter()
        .map(|hit| format!("{label}: {hit} — the forbidden transport crate"))
        .collect()
}

/// The workspace root `Cargo.toml`, plus each production crate's own
/// `Cargo.toml`, labeled — the two places a forbidden declaration could
/// hide the transport, directly or under a `package =` rename.
fn production_manifest_paths(crates_root: &Path) -> Vec<(String, PathBuf)> {
    let mut paths = vec![(
        "Cargo.toml".to_string(),
        workspace_root().join("Cargo.toml"),
    )];
    for crate_name in PRODUCTION_CRATES {
        paths.push((
            format!("{crate_name}/Cargo.toml"),
            crates_root.join(crate_name).join("Cargo.toml"),
        ));
    }
    paths
}

#[test]
fn no_iroh_symbol_anywhere_in_production_crate_sources() {
    // Unfakeable because it scans every production crate's real `src/`
    // tree (not a fixture copy), so an accidental `use iroh::...` or a new
    // iroh-named type anywhere in the tree fails this test, not just at
    // the one call site the author remembered to check.
    let crates_root = crates_root();
    let mut violations = Vec::new();
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
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let relative = path
                .strip_prefix(&src_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let label = format!("{crate_name}/{relative}");
            violations.extend(scan_source_for_transport_name(&label, &text));
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

/// Plants a leak into a REAL file's actual text from each of the two
/// crates the scope widening added (`vigil-ha`, `vigil-bin`), through the
/// same read-scan path `no_iroh_symbol_anywhere_in_production_crate_sources`
/// uses — never a synthetic string. Proves the widened walk really reaches
/// those crates' sources, not just that the per-line detector recognizes
/// the shape in isolation: a scan that silently kept walking only
/// `crates/vigil/src` would pass this file's OTHER tests (the detector
/// itself is still correct) while missing exactly the crates the split put
/// closest to the transport boundary — `vigil-ha` wires the MQTT adapter
/// and `vigil-bin` is the composed product binary, so a leaked dependency
/// would plausibly surface in one of these two first.
#[test]
fn scan_detects_a_planted_leak_in_vigil_ha_and_vigil_bin() {
    let crates_root = crates_root();

    let vigil_ha_file = crates_root.join("vigil-ha").join("src").join("lib.rs");
    let vigil_ha_source = fs::read_to_string(&vigil_ha_file)
        .unwrap_or_else(|error| panic!("read {vigil_ha_file:?}: {error}"));
    assert!(
        scan_source_for_transport_name("vigil-ha/lib.rs", &vigil_ha_source).is_empty(),
        "canary host file must start with zero transport-name mentions"
    );
    let mut planted_vigil_ha = vigil_ha_source;
    planted_vigil_ha.push_str("\nconst __TRANSPORT_PURITY_CANARY: &str = \"iroh\";\n");
    assert!(
        !scan_source_for_transport_name("vigil-ha/lib.rs", &planted_vigil_ha).is_empty(),
        "a planted transport-name mention in a real vigil-ha source file must be detected"
    );

    let vigil_bin_file = crates_root.join("vigil-bin").join("src").join("main.rs");
    let vigil_bin_source = fs::read_to_string(&vigil_bin_file)
        .unwrap_or_else(|error| panic!("read {vigil_bin_file:?}: {error}"));
    assert!(
        scan_source_for_transport_name("vigil-bin/main.rs", &vigil_bin_source).is_empty(),
        "canary host file must start with zero transport-name mentions"
    );
    let mut planted_vigil_bin = vigil_bin_source;
    planted_vigil_bin.push_str("\nconst __TRANSPORT_PURITY_CANARY: &str = \"iroh\";\n");
    assert!(
        !scan_source_for_transport_name("vigil-bin/main.rs", &planted_vigil_bin).is_empty(),
        "a planted transport-name mention in a real vigil-bin source file must be detected"
    );
}

#[test]
fn no_declared_dependency_names_the_transport_crate_in_any_production_manifest() {
    let crates_root = crates_root();
    let mut violations = Vec::new();
    for (label, path) in production_manifest_paths(&crates_root) {
        assert!(
            path.is_file(),
            "expected {} to exist; a guard that cannot find the manifest it means to read must \
             fail loudly, not silently scan nothing",
            path.display()
        );
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        violations.extend(scan_manifest_for_forbidden_transport_declarations(
            &label, &source,
        ));
    }
    assert!(
        violations.is_empty(),
        "a production manifest declares a dependency on the transport crate — directly as the \
         key, or under a local alias via package = \"...\" — consume contextdb's \
         transport-neutral surface instead:\n{}",
        violations.join("\n")
    );
}

/// Reproduces both shapes a manifest declaration can take: the plainest
/// form, the transport named directly as the dependency KEY with no rename
/// in play (`iroh = "*"`), and the exact bypass this closes: the transport
/// named under a local alias via the `package =` field (`mesh = { package
/// = "iroh" }`) — both planted into a real copy of `vigil-bin`'s own
/// `Cargo.toml`, and both link the forbidden transport while no `.rs`
/// source file spells "iroh" anywhere.
///
/// Green before the fix: `rust_sources` — the walker the OLD, `.rs`-only
/// scan used, and still uses for the `.rs` half of this guard — filters
/// strictly by `.rs` extension, so `Cargo.toml` was categorically outside
/// what that walk ever read; neither shape declared only in a manifest was
/// visible to it, regardless of what its per-line detector would have done
/// had it ever been pointed at manifest text. Red after the fix: the
/// declaration-level manifest scan added here reads both the dependency
/// key and the `package =` field directly and catches both.
#[test]
fn scan_catches_a_transport_dependency_declared_directly_or_renamed_via_the_package_field_in_a_real_manifest()
 {
    let crates_root = crates_root();

    // Green before the fix: Cargo.toml was never even enumerated by the
    // walker the pre-fix scan relied on.
    let vigil_bin_src = crates_root.join("vigil-bin").join("src");
    let vigil_bin_manifest = crates_root.join("vigil-bin").join("Cargo.toml");
    assert!(
        !rust_sources(&vigil_bin_src).contains(&vigil_bin_manifest),
        "the pre-fix `.rs`-only walker must never enumerate Cargo.toml — that is exactly the \
         blind spot both a bare declaration and a package-field rename hide in"
    );

    let manifest_source = fs::read_to_string(&vigil_bin_manifest)
        .unwrap_or_else(|error| panic!("read {vigil_bin_manifest:?}: {error}"));
    assert!(
        scan_manifest_for_forbidden_transport_declarations(
            "vigil-bin/Cargo.toml",
            &manifest_source
        )
        .is_empty(),
        "canary host manifest must start with zero transport declarations"
    );

    // `[dev-dependencies]` already has an explicit header earlier in this
    // real manifest, so a second `[dev-dependencies]` header here would be
    // an illegal TOML table redefinition — `[build-dependencies]` (not
    // declared anywhere in the real file) is used for the bare-key form
    // instead, and stays just as real a dependency table as any other.
    let mut planted = manifest_source;
    planted.push_str(
        "\n[build-dependencies]\niroh = \"*\"\n\n\
         [dependencies.__test_estate_planted_mesh]\npackage = \"iroh\"\nversion = \"*\"\n",
    );

    // Red after the fix: the declaration-level scan catches BOTH the bare
    // key and the package-field rename, in two different dependency
    // tables.
    let violations =
        scan_manifest_for_forbidden_transport_declarations("vigil-bin/Cargo.toml", &planted);
    assert_eq!(violations.len(), 2, "got {violations:?}");
    assert!(violations.iter().any(|violation| {
        violation.contains("[build-dependencies] declares \"iroh\" directly as the dependency key")
    }));
    assert!(violations.iter().any(|violation| violation.contains(
        "[dependencies] declares \"__test_estate_planted_mesh\" with package = \"iroh\""
    )));
}
