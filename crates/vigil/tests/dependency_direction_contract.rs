//! Proves the direction of the boundary between Vigil's core crate, its
//! Home Assistant integration adapter, and the composed product binary:
//! core never links the adapter, and never links an MQTT/transport client
//! directly — in its production build or in its test build; the adapter
//! never reaches past core into the substrates core itself embeds
//! (context-graph, contextdb) as its own declared dependency — it depends
//! on core only; and the product binary (`vigil-bin`) never SHIPS a direct
//! dependency on either the substrates or a raw MQTT client — its shipped
//! job is composing `vigil` and `vigil-ha`, never reaching past either one.
//!
//! What this proves: (1) walking every NORMAL (build-time-linked, ships-
//! in-the-binary), DEV (test-only), and BUILD (build-script-time) dependency
//! edge reachable from the `vigil` package, at any depth, never reaches the
//! adapter crate or an MQTT/transport client library; (2) the `vigil-ha`
//! package's own declared dependencies — all three kinds alike — never
//! directly name context-graph or a contextdb crate, in any manifest
//! section. The adapter's own tests exercise it against a `SiteControl`
//! test double, never a real store, so there is no legitimate reason for
//! any dependency kind to appear; a broker test that needs a real store
//! belongs with `vigil-bin`'s suites; (3) `vigil-bin`'s own DIRECT NORMAL
//! (shipped) and BUILD (build-script-time) dependencies never name a
//! substrate or a raw MQTT client — its `[dev-dependencies]` legitimately
//! do, for exactly the acceptance suites (2) describes, so DEV is the one
//! documented exception for this arm and every other kind is in scope,
//! including a dependency declared `optional = true` or under a
//! `[target.'cfg(...)'.*]` table, neither of which changes which kind a
//! manifest entry carries; (4) every real workspace member is explicitly
//! classified as either a product crate this file's forbidden-dependency
//! arms cover, or an explicitly excluded non-product crate — so a fifth
//! workspace member added later fails loudly instead of silently sitting
//! outside every crate-scope scan in this repository.
//!
//! The forbidden-edge decision is factored into pure functions over a small
//! graph type ([`Graph`]) so it can be canaried directly, in this file,
//! against hand-built fixtures — never only against live `cargo metadata`.
//! The `#[test]` functions in the "the real guard" section fetch the real
//! graph and are the actual guards; every function above them is exercised
//! by the `graph_logic` fixture tests.
//!
//! The graph is built from `cargo metadata`'s `packages[].dependencies` —
//! each package's own DECLARED manifest dependency table — never from
//! `resolve.nodes[].deps[].dep_kinds`, the resolved graph. The resolved
//! graph is feature-dependent: an `optional = true` dependency that is not
//! activated by the default feature set (for example `crates/vigil/Cargo.toml`'s
//! `contextdb-server`/`contextdb-engine`, both optional behind the
//! `fabric` feature) is simply absent from `resolve.nodes[].deps` under a
//! default-features run, so a forbidden crate gated that way would be
//! invisible to a resolve-based scan. `packages[].dependencies` has no such
//! blind spot: it lists every dependency a manifest declares, optional or
//! not, feature-gated or not, so a forbidden crate named in any dependency
//! table is caught however it is gated. This declaration-level source is
//! also why a `package = "..."` local-alias rename never evades this file:
//! `cargo metadata` reports a dependency's real crate name in its own
//! `name` field regardless of the manifest's local key, and the local key
//! never appears in `packages[].dependencies` at all — a fixture below
//! locks that fact in so a future rewrite of the graph parser cannot
//! silently start trusting the local key instead.
//!
//! What this does NOT prove: neither arm proves the reverse direction (that
//! the adapter depends on core, or that core depends on nothing else) —
//! those are exercised simply by the crates compiling; neither proves no
//! NEW forbidden crate was ever introduced under a different name — only
//! the specific names listed here; and none of this reaches a VENDORED
//! fork (a path dependency whose own `Cargo.toml` renames itself at the
//! source, so `cargo metadata`'s `name` field is genuinely the alias, not
//! a rename this repository's own manifests declare) — that residual is
//! shared with, and documented alongside, `transport_purity.rs`'s
//! manifest-declaration scan.
//!
//! The workspace-member completeness check reads `cargo metadata`'s
//! `workspace_members` array, not `workspace_default_members` (which today
//! happens to equal `PRODUCTION_CRATES` but is a build-convenience field —
//! `default-members` exists to keep a plain `cargo build`/`cargo test`
//! cheap, not to declare which crates are "real" product crates, so tying
//! crate classification to it would silently stop covering a crate the
//! moment someone re-tuned the default set for build speed). `packages[]`
//! then maps each member id back to its real package name.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use serde_json::{Value, json};

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::PRODUCTION_CRATES;

/// Workspace members that are deliberately NOT product crates: build
/// tooling (`xtask`) and the repository's own acceptance-harness root
/// package (`vigil-acceptance`, the `[package]` declared at the workspace
/// root). Neither ships as part of the Vigil product, so neither belongs
/// in [`PRODUCTION_CRATES`] — but both must still be named somewhere, so a
/// member that is in NEITHER list fails loudly instead of silently
/// escaping every crate-scope scan.
const NON_PRODUCT_WORKSPACE_MEMBERS: &[&str] = &["xtask", "vigil-acceptance"];

/// Names that would break the boundary if they ever appeared as a NORMAL or
/// DEV dependency of the `vigil` package: the adapter crate itself, or an
/// MQTT/transport client library core has no business linking, in
/// production or in its own tests.
const FORBIDDEN_CORE_DEPENDENCIES: &[&str] = &["vigil-ha", "rumqttc"];

/// Names that would break the boundary if they ever appeared as a NORMAL or
/// DEV dependency of the `vigil-ha` adapter package: the substrates core
/// itself embeds. The adapter reaches them only by way of core's own public
/// seam, never by naming them directly in any manifest section — its own
/// tests exercise it against a `SiteControl` test double, never a real
/// store.
const FORBIDDEN_ADAPTER_DEPENDENCIES: &[&str] = &[
    "context-graph",
    "contextdb-core",
    "contextdb-engine",
    "contextdb-server",
];

/// Names that would break the boundary if they ever appeared as a NORMAL
/// (shipped-in-the-binary) or BUILD (build-script-time) dependency of the
/// `vigil-bin` product crate: the substrates core itself embeds, reached
/// only through `vigil`'s public seam, and a raw MQTT client, reached only
/// through `vigil-ha`'s own wiring. `vigil-ha` itself is deliberately
/// absent from this list — `vigil-bin`'s entire job is composing `vigil`
/// and `vigil-ha`, so depending on the adapter is correct, not a
/// violation. Unlike the other two arms, DEV dependencies are the one kind
/// out of scope here on purpose: this crate's own `[dev-dependencies]`
/// legitimately carry the substrate crates and `rumqttc` directly, for the
/// acceptance-journey suites that exercise the composed binary end to end
/// (see the module doc). BUILD is not given the same exception — a
/// forbidden crate pulled in through `[build-dependencies]` is otherwise
/// invisible to this arm, which is exactly the gap an optional,
/// target-specific build-dependency rename exploited before this list
/// grew build coverage.
const FORBIDDEN_PRODUCT_NORMAL_DEPENDENCIES: &[&str] = &[
    "rumqttc",
    "context-graph",
    "contextdb-core",
    "contextdb-engine",
    "contextdb-server",
];

// ── Pure graph logic (canaried below against fixtures, not just live metadata) ──

/// One dependency edge in a resolved graph: the target package's NAME
/// (already resolved from its package id) and the raw `dep_kinds[].kind`
/// strings it carries — `None` denotes a NORMAL edge. A real edge can carry
/// more than one kind (e.g. normal in one profile, dev in another); `cargo
/// metadata` reports that as multiple `dep_kinds` entries on one edge.
#[derive(Debug, Clone)]
struct Edge {
    to: String,
    kinds: Vec<Option<String>>,
}

/// A minimal resolved-dependency graph: package name -> its outgoing edges.
/// Built from real `cargo metadata` in production ([`Graph::from_cargo_metadata`]);
/// built by hand in the fixture tests below, so the forbidden-edge decision
/// itself is unit-tested without shelling out.
#[derive(Debug, Default)]
struct Graph {
    edges: BTreeMap<String, Vec<Edge>>,
}

impl Graph {
    fn fixture(edges: &[(&str, &str, &[Option<&str>])]) -> Self {
        let mut graph = Graph::default();
        for (from, to, kinds) in edges {
            graph
                .edges
                .entry((*from).to_string())
                .or_default()
                .push(Edge {
                    to: (*to).to_string(),
                    kinds: kinds.iter().map(|k| k.map(str::to_string)).collect(),
                });
        }
        graph
    }

    /// Pure: build the graph from `cargo metadata`'s `packages` array
    /// (already parsed JSON), reading each package's own DECLARED
    /// `dependencies` table directly — no resolve-graph lookup, no id
    /// indirection, because a declaration-level dependency entry already
    /// carries the real crate name in its own `name` field. Every entry
    /// counts, `optional` or not: an optional dependency's manifest row is
    /// present here regardless of whether the active feature set actually
    /// activates it, which is the whole point (see the module doc).
    /// Exercised directly by the canary fixture below, never only through
    /// live `cargo metadata`.
    fn from_packages(packages: &[Value]) -> Self {
        let mut graph = Graph::default();
        for package in packages {
            let Some(package_name) = package.get("name").and_then(Value::as_str) else {
                continue;
            };
            let deps = package
                .get("dependencies")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for dep in deps {
                let Some(dep_name) = dep.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let kind = dep.get("kind").and_then(Value::as_str).map(str::to_string);
                graph
                    .edges
                    .entry(package_name.to_string())
                    .or_default()
                    .push(Edge {
                        to: dep_name.to_string(),
                        kinds: vec![kind],
                    });
            }
        }
        graph
    }

    fn from_cargo_metadata() -> Self {
        let metadata = fetch_cargo_metadata();
        let packages = metadata
            .get("packages")
            .and_then(Value::as_array)
            .cloned()
            .expect("cargo metadata carries package manifests");
        Graph::from_packages(&packages)
    }
}

/// Runs real `cargo metadata` once and parses its JSON. Shared by
/// [`Graph::from_cargo_metadata`] and the workspace-member completeness
/// check below, so there is exactly one place that shells out.
fn fetch_cargo_metadata() -> Value {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--locked"])
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata prints valid JSON")
}

/// True when at least one of an edge's `dep_kinds` is NORMAL (ships in the
/// built binary, represented as `None`), `"dev"` (test-only), or `"build"`
/// (build-script-time). All three are in scope: a forbidden crate pulled in
/// through `[build-dependencies]` and used from a `build.rs` is otherwise
/// the one edge kind that would walk past this guard entirely.
fn edge_is_normal_dev_or_build(edge: &Edge) -> bool {
    edge.kinds
        .iter()
        .any(|kind| matches!(kind.as_deref(), None | Some("dev") | Some("build")))
}

/// Walk every normal/dev/build edge reachable from `start`, at any depth,
/// and report every forbidden name reached. Pure: no I/O, no subprocess.
fn reachable_forbidden_names(graph: &Graph, start: &str, forbidden: &[&str]) -> Vec<String> {
    let mut visited: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut stack = vec![start.to_string()];
    let mut violations = Vec::new();

    while let Some(name) = stack.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        for edge in graph.edges.get(&name).map(Vec::as_slice).unwrap_or(&[]) {
            if !edge_is_normal_dev_or_build(edge) {
                continue;
            }
            if forbidden.contains(&edge.to.as_str()) {
                violations.push(format!("{name} depends on forbidden crate {}", edge.to));
            }
            stack.push(edge.to.clone());
        }
    }
    violations
}

/// Check only `package`'s own DIRECT normal/dev/build edges against
/// `forbidden` — no transitive walk. Pure: no I/O, no subprocess.
fn direct_forbidden_names(graph: &Graph, package: &str, forbidden: &[&str]) -> Vec<String> {
    graph
        .edges
        .get(package)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|edge| edge_is_normal_dev_or_build(edge))
        .filter(|edge| forbidden.contains(&edge.to.as_str()))
        .map(|edge| {
            format!(
                "{package} depends (normal, dev, or build) directly on {}; it must reach that \
                 substrate only through core's public seam, and its own tests must use a \
                 SiteControl test double instead of a real store",
                edge.to
            )
        })
        .collect()
}

/// True when at least one of an edge's `dep_kinds` is NORMAL (ships in the
/// built binary, represented as `None`) or `"build"` (build-script-time) —
/// dev is the one kind deliberately excluded, unlike
/// [`edge_is_normal_dev_or_build`]. `optional = true` and a
/// `[target.'cfg(...)'.*]` placement are both orthogonal to `kind` in
/// `cargo metadata`'s own dependency shape (see the module doc), so neither
/// changes what this predicate sees.
fn edge_is_normal_or_build(edge: &Edge) -> bool {
    edge.kinds
        .iter()
        .any(|kind| matches!(kind.as_deref(), None | Some("build")))
}

/// Check only `package`'s own DIRECT NORMAL (shipped-in-the-binary) and
/// BUILD (build-script-time) edges against `forbidden` — no transitive
/// walk, and DEV is the one kind deliberately out of scope. Pure: no I/O,
/// no subprocess.
///
/// This is the arm `vigil-bin` needs and the other two arms above do not:
/// `vigil-bin`'s own `[dev-dependencies]` legitimately carry the substrate
/// crates and a real MQTT client directly, for the acceptance-journey
/// suites that exercise the composed binary end to end (the one place
/// core's `SiteControl` test double is not enough) — so DEV is excluded on
/// purpose. NORMAL and BUILD carry no such exception: a forbidden crate
/// pulled in through `[build-dependencies]` — optional, target-specific, or
/// plain — is otherwise invisible to this arm.
fn direct_forbidden_names_excluding_dev(
    graph: &Graph,
    package: &str,
    forbidden: &[&str],
) -> Vec<String> {
    graph
        .edges
        .get(package)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|edge| edge_is_normal_or_build(edge))
        .filter(|edge| forbidden.contains(&edge.to.as_str()))
        .map(|edge| {
            format!(
                "{package} depends, in its shipped production build or its build script, \
                 directly on {}; it must reach that substrate only through vigil (core)'s \
                 public seam, or a real MQTT broker only through vigil-ha (the adapter)'s own \
                 wiring",
                edge.to
            )
        })
        .collect()
}

// ── Workspace-member completeness (canaried below against fixtures) ──────

/// Every workspace member package name found in `metadata`'s own
/// `workspace_members` + `packages` arrays — declaration-level, and
/// deliberately reading `workspace_members` rather than
/// `workspace_default_members` (see the module doc for why).
fn workspace_member_package_names(metadata: &Value) -> Vec<String> {
    let workspace_members: BTreeSet<String> = metadata
        .get("workspace_members")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    metadata
        .get("packages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|package| {
            let id = package.get("id").and_then(Value::as_str)?;
            if !workspace_members.contains(id) {
                return None;
            }
            package
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// Every name in `members` that is in neither `production` nor
/// `non_product` — the loud failure a silently under-covered crate-scope
/// scan needs: a new workspace member fails this check until it is
/// explicitly classified one way or the other, instead of sitting outside
/// every all-crate scan unnoticed. Pure: no I/O, no subprocess.
fn unclassified_workspace_members(
    members: &[String],
    production: &[&str],
    non_product: &[&str],
) -> Vec<String> {
    members
        .iter()
        .filter(|member| {
            !production.contains(&member.as_str()) && !non_product.contains(&member.as_str())
        })
        .cloned()
        .collect()
}

// ── Canaries: prove the pure functions above actually catch a planted fault ──

#[cfg(test)]
mod graph_logic {
    use super::*;

    #[test]
    fn reachable_forbidden_names_is_silent_on_a_clean_transitive_graph() {
        let graph = Graph::fixture(&[
            ("vigil", "context-graph", &[None]),
            ("vigil", "serde", &[None, Some("dev")]),
            ("context-graph", "contextdb-core", &[None]),
        ]);
        assert!(reachable_forbidden_names(&graph, "vigil", FORBIDDEN_CORE_DEPENDENCIES).is_empty());
    }

    #[test]
    fn reachable_forbidden_names_catches_a_planted_normal_edge_at_depth_one() {
        let graph = Graph::fixture(&[
            ("vigil", "context-graph", &[None]),
            ("vigil", "rumqttc", &[None]),
        ]);
        let violations = reachable_forbidden_names(&graph, "vigil", FORBIDDEN_CORE_DEPENDENCIES);
        assert_eq!(violations.len(), 1, "got {violations:?}");
        assert!(violations[0].contains("rumqttc"));
    }

    #[test]
    fn reachable_forbidden_names_catches_a_planted_dev_edge() {
        let graph = Graph::fixture(&[
            ("vigil", "context-graph", &[None]),
            ("vigil", "vigil-ha", &[Some("dev")]),
        ]);
        let violations = reachable_forbidden_names(&graph, "vigil", FORBIDDEN_CORE_DEPENDENCIES);
        assert_eq!(violations.len(), 1, "got {violations:?}");
        assert!(violations[0].contains("vigil-ha"));
    }

    #[test]
    fn reachable_forbidden_names_catches_a_planted_edge_reached_transitively() {
        let graph = Graph::fixture(&[
            ("vigil", "some-helper", &[None]),
            ("some-helper", "rumqttc", &[None]),
        ]);
        let violations = reachable_forbidden_names(&graph, "vigil", FORBIDDEN_CORE_DEPENDENCIES);
        assert_eq!(violations.len(), 1, "got {violations:?}");
        assert!(violations[0].contains("rumqttc"));
    }

    #[test]
    fn reachable_forbidden_names_catches_a_planted_build_edge() {
        let graph = Graph::fixture(&[("vigil", "rumqttc", &[Some("build")])]);
        let violations = reachable_forbidden_names(&graph, "vigil", FORBIDDEN_CORE_DEPENDENCIES);
        assert_eq!(violations.len(), 1, "got {violations:?}");
        assert!(violations[0].contains("rumqttc"));
    }

    #[test]
    fn direct_forbidden_names_is_silent_on_a_clean_adapter_graph() {
        let graph = Graph::fixture(&[
            ("vigil-ha", "vigil", &[None]),
            ("vigil-ha", "rumqttc", &[None]),
        ]);
        assert!(
            direct_forbidden_names(&graph, "vigil-ha", FORBIDDEN_ADAPTER_DEPENDENCIES).is_empty()
        );
    }

    #[test]
    fn direct_forbidden_names_catches_a_planted_normal_edge() {
        let graph = Graph::fixture(&[
            ("vigil-ha", "vigil", &[None]),
            ("vigil-ha", "context-graph", &[None]),
        ]);
        let violations = direct_forbidden_names(&graph, "vigil-ha", FORBIDDEN_ADAPTER_DEPENDENCIES);
        assert_eq!(violations.len(), 1, "got {violations:?}");
        assert!(violations[0].contains("context-graph"));
    }

    #[test]
    fn direct_forbidden_names_catches_a_planted_dev_edge() {
        let graph = Graph::fixture(&[
            ("vigil-ha", "vigil", &[None]),
            ("vigil-ha", "context-graph", &[Some("dev")]),
        ]);
        let violations = direct_forbidden_names(&graph, "vigil-ha", FORBIDDEN_ADAPTER_DEPENDENCIES);
        assert_eq!(violations.len(), 1, "got {violations:?}");
        assert!(violations[0].contains("context-graph"));
    }

    #[test]
    fn direct_forbidden_names_catches_a_planted_build_edge() {
        let graph = Graph::fixture(&[
            ("vigil-ha", "vigil", &[None]),
            ("vigil-ha", "context-graph", &[Some("build")]),
        ]);
        let violations = direct_forbidden_names(&graph, "vigil-ha", FORBIDDEN_ADAPTER_DEPENDENCIES);
        assert_eq!(violations.len(), 1, "got {violations:?}");
        assert!(violations[0].contains("context-graph"));
    }

    #[test]
    fn from_packages_reaches_an_optional_dependency_behind_a_non_default_feature() {
        // Shaped exactly like the real gap this closes:
        // `crates/vigil/Cargo.toml` declares `contextdb-server` as
        // `optional = true`, activated only by the non-default `fabric`
        // feature. A resolve-graph scan run without that feature enabled
        // never sees the edge at all; `from_packages` reads the manifest
        // declaration directly, so it is feature-independent and catches
        // it regardless of what is active.
        let packages = vec![json!({
            "name": "vigil",
            "dependencies": [
                {
                    "name": "vigil-ha",
                    "kind": null,
                    "optional": true
                },
                {
                    "name": "serde",
                    "kind": null,
                    "optional": false
                }
            ]
        })];
        let graph = Graph::from_packages(&packages);
        let violations = reachable_forbidden_names(&graph, "vigil", FORBIDDEN_CORE_DEPENDENCIES);
        assert_eq!(
            violations.len(),
            1,
            "an optional=true dependency behind a non-default feature must still be reachable: \
             got {violations:?}"
        );
        assert!(violations[0].contains("vigil-ha"));
    }

    #[test]
    fn direct_forbidden_names_does_not_walk_transitively() {
        // vigil-ha -> some-helper -> context-graph: the adapter arm checks
        // ONLY vigil-ha's own direct edges, by design (see module doc) — a
        // forbidden name reached only through an intermediate dependency is
        // not this arm's job to catch.
        let graph = Graph::fixture(&[
            ("vigil-ha", "some-helper", &[None]),
            ("some-helper", "context-graph", &[None]),
        ]);
        assert!(
            direct_forbidden_names(&graph, "vigil-ha", FORBIDDEN_ADAPTER_DEPENDENCIES).is_empty()
        );
    }

    #[test]
    fn from_packages_reaches_a_forbidden_crate_declared_under_a_local_package_rename() {
        // `cargo metadata`'s own dependency entries carry the crate's REAL
        // name in `name` regardless of a local `package = "..."` rename
        // key on the manifest — the local key is not even present in
        // `packages[].dependencies` at all, only in `rename` (which this
        // parser never reads). A manifest entry shaped like
        // `graph_store = { package = "context-graph" }` still reports
        // `name: "context-graph"` here, so `from_packages` is already
        // immune to the rename; this fixture locks that fact in so a
        // future rewrite cannot silently regress it.
        let packages = vec![json!({
            "name": "vigil-ha",
            "dependencies": [
                {
                    "name": "context-graph",
                    "kind": null,
                    "optional": false,
                    "rename": "graph_store"
                }
            ]
        })];
        let graph = Graph::from_packages(&packages);
        let violations = direct_forbidden_names(&graph, "vigil-ha", FORBIDDEN_ADAPTER_DEPENDENCIES);
        assert_eq!(violations.len(), 1, "got {violations:?}");
        assert!(violations[0].contains("context-graph"));
    }

    #[test]
    fn direct_forbidden_names_excluding_dev_is_silent_on_the_real_shape_of_vigil_bins_dev_dependencies()
     {
        // vigil-bin's own real Cargo.toml: NORMAL deps are just vigil and
        // vigil-ha (both allowed); DEV deps legitimately carry the
        // substrate and rumqttc directly, for the acceptance-journey
        // suites. The excluding-dev arm must stay silent on this exact
        // shape.
        let graph = Graph::fixture(&[
            ("vigil-bin", "vigil", &[None]),
            ("vigil-bin", "vigil-ha", &[None]),
            ("vigil-bin", "context-graph", &[Some("dev")]),
            ("vigil-bin", "contextdb-core", &[Some("dev")]),
            ("vigil-bin", "rumqttc", &[Some("dev")]),
        ]);
        assert!(
            direct_forbidden_names_excluding_dev(
                &graph,
                "vigil-bin",
                FORBIDDEN_PRODUCT_NORMAL_DEPENDENCIES
            )
            .is_empty()
        );
    }

    #[test]
    fn direct_forbidden_names_excluding_dev_catches_a_planted_shipped_substrate_dependency() {
        let graph = Graph::fixture(&[
            ("vigil-bin", "vigil", &[None]),
            ("vigil-bin", "vigil-ha", &[None]),
            ("vigil-bin", "context-graph", &[None]),
        ]);
        let violations = direct_forbidden_names_excluding_dev(
            &graph,
            "vigil-bin",
            FORBIDDEN_PRODUCT_NORMAL_DEPENDENCIES,
        );
        assert_eq!(violations.len(), 1, "got {violations:?}");
        assert!(violations[0].contains("context-graph"));
    }

    #[test]
    fn direct_forbidden_names_excluding_dev_catches_a_planted_shipped_mqtt_dependency() {
        let graph = Graph::fixture(&[
            ("vigil-bin", "vigil", &[None]),
            ("vigil-bin", "vigil-ha", &[None]),
            ("vigil-bin", "rumqttc", &[None]),
        ]);
        let violations = direct_forbidden_names_excluding_dev(
            &graph,
            "vigil-bin",
            FORBIDDEN_PRODUCT_NORMAL_DEPENDENCIES,
        );
        assert_eq!(violations.len(), 1, "got {violations:?}");
        assert!(violations[0].contains("rumqttc"));
    }

    #[test]
    fn direct_forbidden_names_excluding_dev_catches_a_planted_build_edge_but_ignores_a_planted_dev_edge()
     {
        // The behavior that actually distinguishes this arm from
        // `direct_forbidden_names`: dev stays the one documented exception
        // (vigil-bin's real acceptance-suite dev-dependencies), so the SAME
        // edge, dev-only, is a violation under the all-kinds arm and NOT a
        // violation under the excluding-dev arm. But a build-only edge —
        // the exact shape an optional, target-specific build-dependency
        // rename took when it slipped past the old NORMAL-only filter this
        // arm used to have — must now be caught just like a normal edge.
        let dev_graph = Graph::fixture(&[("vigil-bin", "context-graph", &[Some("dev")])]);
        assert!(
            !direct_forbidden_names(
                &dev_graph,
                "vigil-bin",
                FORBIDDEN_PRODUCT_NORMAL_DEPENDENCIES
            )
            .is_empty(),
            "sanity: the all-kinds arm must still catch a dev-only edge"
        );
        assert!(
            direct_forbidden_names_excluding_dev(
                &dev_graph,
                "vigil-bin",
                FORBIDDEN_PRODUCT_NORMAL_DEPENDENCIES
            )
            .is_empty(),
            "the excluding-dev arm must still ignore a dev-only edge"
        );

        let build_graph = Graph::fixture(&[("vigil-bin", "context-graph", &[Some("build")])]);
        let violations = direct_forbidden_names_excluding_dev(
            &build_graph,
            "vigil-bin",
            FORBIDDEN_PRODUCT_NORMAL_DEPENDENCIES,
        );
        assert_eq!(
            violations.len(),
            1,
            "a build-only edge must be caught, unlike a dev-only edge: got {violations:?}"
        );
        assert!(violations[0].contains("context-graph"));
    }

    #[test]
    fn workspace_member_package_names_reads_declared_workspace_members_not_default_members() {
        // `workspace_default_members` deliberately omits `xtask` in this
        // fixture (mirroring the real workspace's own
        // `default-members`) — proving the function reads
        // `workspace_members` (which includes it), not
        // `workspace_default_members` (which would silently drop it).
        let metadata = json!({
            "workspace_members": ["id-a", "id-b"],
            "workspace_default_members": ["id-a"],
            "packages": [
                {"id": "id-a", "name": "vigil"},
                {"id": "id-b", "name": "xtask"},
                {"id": "id-c", "name": "not-a-member"}
            ]
        });
        let mut names = workspace_member_package_names(&metadata);
        names.sort();
        assert_eq!(names, vec!["vigil".to_string(), "xtask".to_string()]);
    }

    #[test]
    fn unclassified_workspace_members_is_silent_when_every_member_is_classified() {
        let members = vec!["vigil".to_string(), "xtask".to_string()];
        assert!(unclassified_workspace_members(&members, &["vigil"], &["xtask"]).is_empty());
    }

    #[test]
    fn unclassified_workspace_members_catches_a_planted_fourth_product_crate() {
        let members = vec![
            "vigil".to_string(),
            "vigil-ha".to_string(),
            "vigil-bin".to_string(),
            "xtask".to_string(),
            "vigil-acceptance".to_string(),
            "vigil-cloud-relay".to_string(),
        ];
        let gaps = unclassified_workspace_members(
            &members,
            PRODUCTION_CRATES,
            NON_PRODUCT_WORKSPACE_MEMBERS,
        );
        assert_eq!(gaps, vec!["vigil-cloud-relay".to_string()]);
    }
}

// ── The real guard: same pure functions, fed the live resolved graph ───────

#[test]
fn core_crate_never_depends_on_the_adapter_or_an_mqtt_transport_crate() {
    let graph = Graph::from_cargo_metadata();
    let violations = reachable_forbidden_names(&graph, "vigil", FORBIDDEN_CORE_DEPENDENCIES);
    assert!(
        violations.is_empty(),
        "core crate's dependency tree reaches a forbidden crate:\n{}",
        violations.join("\n")
    );
}

#[test]
fn adapter_crate_declares_no_dependency_of_either_kind_on_a_substrate_core_already_embeds() {
    let graph = Graph::from_cargo_metadata();
    let violations = direct_forbidden_names(&graph, "vigil-ha", FORBIDDEN_ADAPTER_DEPENDENCIES);
    assert!(
        violations.is_empty(),
        "adapter crate's dependencies name a substrate core already embeds:\n{}",
        violations.join("\n")
    );
}

#[test]
fn product_binary_crate_never_ships_a_direct_dependency_on_a_substrate_or_mqtt_transport_crate() {
    let graph = Graph::from_cargo_metadata();
    let violations = direct_forbidden_names_excluding_dev(
        &graph,
        "vigil-bin",
        FORBIDDEN_PRODUCT_NORMAL_DEPENDENCIES,
    );
    assert!(
        violations.is_empty(),
        "product binary crate's shipped or build-time dependencies name a substrate or MQTT \
         transport crate directly:\n{}",
        violations.join("\n")
    );
}

#[test]
fn every_workspace_member_is_classified_as_production_or_explicitly_excluded() {
    let metadata = fetch_cargo_metadata();
    let members = workspace_member_package_names(&metadata);
    assert!(
        !members.is_empty(),
        "cargo metadata must report at least one workspace member"
    );
    let gaps =
        unclassified_workspace_members(&members, PRODUCTION_CRATES, NON_PRODUCT_WORKSPACE_MEMBERS);
    assert!(
        gaps.is_empty(),
        "workspace member(s) {gaps:?} are neither in PRODUCTION_CRATES nor in \
         NON_PRODUCT_WORKSPACE_MEMBERS — classify the new crate explicitly before any \
         crate-scope scan can be trusted to cover it (or deliberately exclude it)"
    );
    let member_set: BTreeSet<&str> = members.iter().map(String::as_str).collect();
    for name in PRODUCTION_CRATES
        .iter()
        .chain(NON_PRODUCT_WORKSPACE_MEMBERS)
    {
        assert!(
            member_set.contains(name),
            "`{name}` is listed in PRODUCTION_CRATES or NON_PRODUCT_WORKSPACE_MEMBERS but is not \
             an actual workspace member; the classification lists must track real crates"
        );
    }
}
