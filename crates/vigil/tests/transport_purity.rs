//! Vigil's distributed-compute fabric uses ContextDB's typed authenticated
//! peer endpoint in production. Arbitrary transport injection is test-only,
//! and ContextDB owns sync declarations and arbitration.

use std::fs;
use std::path::PathBuf;

#[path = "source_scan_lexer.rs"]
mod source_scan_lexer;
use source_scan_lexer::{
    PRODUCTION_CRATES, crates_root, lex, match_paren, rust_sources, string_literals,
};
use vigil::settings_store::{SyncClass, table_declarations, vigil_consumer_schema_statements};

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

/// What every `format!`/`write!`/`writeln!` call in `source` would spell if
/// its own literal arguments were assembled — the same reconstruction
/// `format!` itself performs at runtime, done here at scan time to close a
/// keyword-split evasion the per-literal scan cannot see on its own
/// (cold-review-arc2-r5 finding 11): a DDL keyword split across the
/// template and a positional literal argument — `format!("{}TABLE
/// cg_evidence (", "CREATE ")` — produces two literals, neither of which
/// `ddl_literal` recognizes alone.
///
/// Scoped deliberately: only `{}` placeholders are substituted, in order,
/// by the call's OWN literal arguments (never a named/indexed placeholder
/// like `{0}`/`{name}`, and never a non-literal argument — a variable or a
/// runtime-computed string cannot be judged from source text, so a
/// placeholder with no literal available to fill it is left as `{}`
/// verbatim, which matches no keyword prefix and stays harmless to this
/// check). This is a reconstruction of what the call's literal material
/// COULD spell, not a full `format!` interpreter — good enough to defeat
/// the demonstrated evasion class without pretending to be a general
/// string-flow analyzer.
fn format_call_reconstructions(source: &str) -> Vec<String> {
    let lexed = lex(source);
    let masked = &lexed.masked;
    let mut reconstructions = Vec::new();
    for macro_name in ["format!(", "write!(", "writeln!("] {
        let needle: Vec<char> = macro_name.chars().collect();
        let mut i = 0usize;
        while i + needle.len() <= masked.len() {
            if masked[i..i + needle.len()] != needle[..] {
                i += 1;
                continue;
            }
            let open_paren = i + needle.len() - 1;
            let Some(close_paren) = match_paren(masked, open_paren) else {
                i += needle.len();
                continue;
            };
            let call_literals: Vec<&String> = lexed
                .strings
                .iter()
                .filter(|(start, _, _)| *start > open_paren && *start < close_paren)
                .map(|(_, _, content)| content)
                .collect();
            if let Some((template, args)) = call_literals.split_first() {
                let mut result = String::new();
                let mut remaining = template.as_str();
                let mut arg_iter = args.iter();
                while let Some(pos) = remaining.find("{}") {
                    result.push_str(&remaining[..pos]);
                    if let Some(arg) = arg_iter.next() {
                        result.push_str(arg);
                    } else {
                        result.push_str("{}");
                    }
                    remaining = &remaining[pos + 2..];
                }
                result.push_str(remaining);
                reconstructions.push(result);
            }
            i = close_paren + 1;
        }
    }
    reconstructions
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
                // The ONLY literal exempt from the namespace check below is
                // the exact bare clause fragment Vigil assembles its own DDL
                // from at runtime — `SyncClass::conflict_clause` in
                // `settings_store.rs` has exactly two arms, `" SYNC CONFLICT
                // KEEP LATEST"` (concatenated onto a separately-literal
                // `CREATE TABLE vigil_... (` head) and `""` (no clause at
                // all) — so this is the one literal the exemption needs to
                // cover, not a family of several. Naming this exemption by its exact
                // shape — rather than by "no table name could be parsed out
                // of it" — matters: a literal that DOES start a CREATE/ALTER/
                // DROP TABLE statement but whose name cannot be parsed (an
                // interpolated template like `"CREATE TABLE {} ("`) is
                // suspicious, not innocent, and must fall through to the
                // namespace check right below, where an unparseable name is
                // never recognized as `vigil_`-owned and so is flagged.
                // Pinned both ways by
                // `the_false_positive_clause_fragment_is_no_longer_flagged_but_teeth_are_preserved`
                // and `an_unparseable_table_name_is_flagged_not_exempted`.
                .filter(|value| !is_exempt_clause_fragment(value))
                // Vigil's OWN `vigil_`-prefixed tables, declared through
                // context-graph's consumer-table seam, are the one reviewed
                // exception: that seam's contract is that the consumer supplies
                // the CREATE TABLE text for tables under its own namespace and
                // context-graph executes it verbatim. Everything else — Context
                // Graph's schema, the sync work ledger, any foreign table, and
                // every other statement kind whatever name it mentions — stays
                // refused exactly as before. `is_vigil_owned_table_declaration`
                // is the single rule, exercised in both directions by
                // `vigil_namespace_exemption_only_covers_vigil_tables` below.
                .filter(|value| !is_vigil_owned_table_declaration(value))
            {
                let relative = path.strip_prefix(&crate_roots).unwrap_or(&path);
                ddl.push(format!("{}: {literal:?}", relative.display()));
            }
            // The same three filters, applied to what a `format!`/`write!`/
            // `writeln!` call's OWN literal arguments would spell once
            // assembled — closing the keyword-split evasion the per-literal
            // scan above cannot see (cold-review-arc2-r5 finding 11; see
            // `a_ddl_keyword_split_across_a_format_call_is_still_caught`).
            for reconstructed in format_call_reconstructions(&text)
                .into_iter()
                .filter(|value| ddl_literal(value))
                .filter(|value| !is_exempt_clause_fragment(value))
                .filter(|value| !is_vigil_owned_table_declaration(value))
            {
                let relative = path.strip_prefix(&crate_roots).unwrap_or(&path);
                ddl.push(format!(
                    "{}: (reconstructed from a format!/write! call) {reconstructed:?}",
                    relative.display()
                ));
            }
        }
    }
    assert!(
        ddl.is_empty(),
        "Vigil must not author Context Graph or work-ledger DDL outside its own `vigil_` \
         namespace; ContextDB owns those declarations. Found: {ddl:?}"
    );
}

/// The table name a `CREATE TABLE <name> ...` literal creates, or `None` for
/// anything else. An `ALTER TABLE` / `DROP TABLE` / index / bare `SYNC`
/// literal never creates a table, so it can never qualify for the namespace
/// exemption below — it stays refused unconditionally, whatever name it
/// mentions.
fn ddl_table_name(literal: &str) -> Option<String> {
    let rest = literal.trim_start();
    if !rest.to_ascii_uppercase().starts_with("CREATE TABLE") {
        return None;
    }
    let rest = rest["CREATE TABLE".len()..].trim_start();
    let end = rest
        .find(|ch: char| !(ch == '_' || ch.is_ascii_alphanumeric()))
        .unwrap_or(rest.len());
    let name = &rest[..end];
    (!name.is_empty()).then(|| name.to_string())
}

/// Vigil authors DDL only for tables it owns under its own `vigil_`
/// namespace, declared through context-graph's consumer-table seam: that
/// seam's contract is that the consumer supplies the CREATE TABLE text for
/// tables under its namespace and context-graph executes it verbatim, so a
/// `vigil_`-prefixed table declared that way is a legitimate, reviewed
/// exception to "Vigil must not author DDL." ContextDB and context-graph own
/// every other table — their own schema, the sync work ledger, and anything
/// else outside vigil's namespace — so a declaration naming any other table
/// stays refused, whatever statement kind it uses.
fn is_vigil_owned_table_declaration(literal: &str) -> bool {
    ddl_table_name(literal).is_some_and(|name| name.starts_with("vigil_") && name != "vigil_")
}

/// The table name a DDL literal identifies, for every statement kind that
/// names one at all — broader than [`ddl_table_name`], which only recognizes
/// `CREATE TABLE` (the one statement kind eligible for the `vigil_` namespace
/// exemption). `ALTER TABLE`/`DROP TABLE` name the table right after the
/// keyword pair; `CREATE INDEX`/`CREATE UNIQUE INDEX` name it after `ON`. A
/// bare `SYNC TWO WAY` / `SYNC OFF` / `SYNC CONFLICT ...` clause names no
/// table at all: production assembles those as a suffix concatenated at
/// runtime onto a separate `CREATE TABLE <name> (` head literal (see
/// `SyncClass::conflict_clause` in `settings_store.rs`), so as its OWN
/// literal it can never point at any table, foreign or Vigil's own — there is
/// nothing here for the namespace check to rule on, so it is not a complete
/// table-changing statement and this returns `None` for it.
fn ddl_named_table(literal: &str) -> Option<String> {
    let rest = literal.trim_start();
    let upper = rest.to_ascii_uppercase();

    let name_after = |prefix_len: usize| -> Option<String> {
        let after = rest[prefix_len..].trim_start();
        let end = after
            .find(|ch: char| !(ch == '_' || ch.is_ascii_alphanumeric()))
            .unwrap_or(after.len());
        let name = &after[..end];
        (!name.is_empty()).then(|| name.to_string())
    };

    for keyword in ["CREATE TABLE", "ALTER TABLE", "DROP TABLE"] {
        if upper.starts_with(keyword) {
            return name_after(keyword.len());
        }
    }
    for keyword in ["CREATE UNIQUE INDEX", "CREATE INDEX"] {
        if upper.starts_with(keyword) {
            return match upper.find(" ON ") {
                Some(on_at) => name_after(on_at + " ON ".len()),
                None => None,
            };
        }
    }
    None
}

/// The narrow, documented exemption: only these exact clause-fragment
/// literals — which `SyncClass::conflict_clause` in `settings_store.rs`
/// concatenates onto a separately-literal `CREATE TABLE <name> (` head at
/// runtime, so on their own they name no table at all — are exempt from the
/// namespace check. Nothing else is exempt on the grounds of "no name could
/// be parsed out of it": an unparseable name on an otherwise-complete
/// CREATE/ALTER/DROP TABLE literal is suspicious, not innocent, and falls
/// through to `is_vigil_owned_table_declaration`, which never recognizes an
/// unparseable name as `vigil_`-owned, so it is flagged. See
/// [`ddl_named_table`] for the broader (non-exempting) name parse used only
/// to demonstrate that contrast in
/// `an_unparseable_table_name_is_flagged_not_exempted`.
fn is_exempt_clause_fragment(literal: &str) -> bool {
    // Trimmed to the one literal the tree actually contains
    // (cold-review-arc2-r5 finding 11): `SyncClass::conflict_clause` in
    // `settings_store.rs` has exactly two arms — `" SYNC CONFLICT KEEP
    // LATEST"` for a table that travels, and `""` (no clause at all, so no
    // literal to exempt) for one that never syncs. Nothing in production
    // ever emits a bare `"SYNC OFF"` or `"SYNC TWO WAY"` literal — those
    // words describe the engine's OWN generated SQL direction
    // (`sync_class.engine_direction().sql()`), never a Vigil source
    // literal — so exempting them bought this guard nothing it needed and
    // would have quietly re-admitted either spelling the moment production
    // ever did emit it as its own bare fragment. Widen this again, by exact
    // literal, the day a new travelling-table clause shape genuinely needs
    // it.
    [" SYNC CONFLICT KEEP LATEST"].contains(&literal)
}

/// The exact predicate chain `vigil_production_fabric_uses_authenticated_iroh`
/// applies inside its scan loop, pulled out so the chain can be pinned by a
/// unit test independent of what any file on disk happens to contain right
/// now.
fn would_be_flagged_as_unauthorized_ddl(literal: &str) -> bool {
    ddl_literal(literal)
        && !is_exempt_clause_fragment(literal)
        && !is_vigil_owned_table_declaration(literal)
}

#[test]
fn vigil_namespace_exemption_only_covers_vigil_tables() {
    // Unfakeable: the exemption is exercised in both directions against
    // literals of the SAME shape. A rule that widened to "any CREATE TABLE
    // literal" passes the first assertion and fails on the Context Graph and
    // work-ledger names; a rule that keyed off the containing file or off the
    // presence of a SYNC clause rather than off the table name would pass the
    // foreign cases only by accident and fails on the statement kinds. No
    // production source is read here on purpose — this pins the RULE, so it
    // cannot go quietly vacuous on a tree that happens to declare nothing.
    assert!(
        is_vigil_owned_table_declaration(
            "CREATE TABLE vigil_settings_local (name TEXT PRIMARY KEY) SYNC TWO WAY"
        ),
        "a vigil_-prefixed table declaration is Vigil's own, declared through the consumer-table \
         seam, and must be exempt"
    );

    for foreign in [
        "CREATE TABLE decisions (id UUID PRIMARY KEY)",
        "CREATE TABLE work_inputs (id UUID PRIMARY KEY)",
        "CREATE TABLE cg_schema_version (id INTEGER PRIMARY KEY)",
        "CREATE TABLE vigil (id INTEGER PRIMARY KEY)",
        "CREATE TABLE vigil_ (id INTEGER PRIMARY KEY)",
    ] {
        assert!(
            !is_vigil_owned_table_declaration(foreign),
            "a table outside Vigil's own namespace stays refused; got exempt for {foreign:?}"
        );
    }

    // Statement kinds other than CREATE TABLE never create a table, so they
    // can never qualify for the exemption even when they name a vigil_ table.
    for non_creating in [
        "ALTER TABLE vigil_settings_local ADD COLUMN extra TEXT",
        "DROP TABLE vigil_settings_local",
        "CREATE INDEX vigil_settings_local_idx ON vigil_settings_local (name)",
        "CREATE UNIQUE INDEX vigil_settings_local_uniq ON vigil_settings_local (name)",
        "SYNC CONFLICT KEEP LATEST",
        "SYNC OFF",
    ] {
        assert!(
            !is_vigil_owned_table_declaration(non_creating),
            "only CREATE TABLE creates a table; every other statement kind stays refused, got \
             exempt for {non_creating:?}"
        );
    }

    // The DDL classifier and the exemption must agree about what they are
    // talking about: every literal above is a DDL literal, so the exemption is
    // genuinely the only thing standing between it and a refusal.
    for literal in [
        "CREATE TABLE vigil_settings_local (name TEXT PRIMARY KEY) SYNC TWO WAY",
        "CREATE TABLE decisions (id UUID PRIMARY KEY)",
        "ALTER TABLE vigil_settings_local ADD COLUMN extra TEXT",
        "SYNC OFF",
    ] {
        assert!(
            ddl_literal(literal),
            "the DDL classifier must recognize {literal:?}, or the exemption is never consulted \
             for it in the first place"
        );
    }
}

/// Pins the fix directly: the bare conflict-policy clause fragment that
/// caused `vigil_production_fabric_uses_authenticated_iroh` to false-positive
/// (it named no table, so the old scan could not tell it apart from foreign
/// DDL) must no longer be flagged — but the guard's teeth against genuinely
/// foreign DDL, in every statement kind the scan recognizes, are unchanged.
/// This is exercised on the predicate chain directly rather than via a
/// planted fault on disk, so it stays a permanent, always-run regression
/// pin instead of a one-off manual check.
#[test]
fn the_false_positive_clause_fragment_is_no_longer_flagged_but_teeth_are_preserved() {
    // The exact fragment `SyncClass::conflict_clause` appends to every
    // travelling settings table's DDL — the one literal `is_exempt_clause_
    // fragment` actually exempts, matched by its exact shape rather than by
    // "no name could be parsed out of it" (that broader ground is what
    // `an_unparseable_table_name_is_flagged_not_exempted` below proves is
    // REJECTED, not the reason this one is exempt).
    let clause_fragment = " SYNC CONFLICT KEEP LATEST";
    assert!(
        !would_be_flagged_as_unauthorized_ddl(clause_fragment),
        "a bare clause fragment names no table and must not be mistaken for foreign DDL: \
         {clause_fragment:?}"
    );

    // Teeth: a literal that actually names a foreign table — in every
    // statement kind the scan recognizes, including one carrying the exact
    // conflict-policy clause that caused the false positive — is still
    // caught.
    for foreign in [
        "CREATE TABLE decisions (id UUID PRIMARY KEY) SYNC CONFLICT KEEP LATEST",
        "CREATE TABLE work_inputs (id UUID PRIMARY KEY)",
        "CREATE TABLE cg_schema_version (id INTEGER PRIMARY KEY)",
        "ALTER TABLE decisions ADD COLUMN extra TEXT",
        "DROP TABLE decisions",
        "CREATE INDEX decisions_idx ON decisions (id)",
        "CREATE UNIQUE INDEX decisions_uniq ON decisions (id)",
    ] {
        assert!(
            would_be_flagged_as_unauthorized_ddl(foreign),
            "a literal that names a foreign table must still be flagged, whatever statement kind \
             it uses: {foreign:?}"
        );
    }

    // And Vigil's own CREATE TABLE declarations stay exempt, exactly as
    // before the fix — the fix only stops bare fragments from being judged
    // as if they named a table, it does not widen the exemption itself.
    assert!(!would_be_flagged_as_unauthorized_ddl(
        "CREATE TABLE vigil_settings_local (name TEXT PRIMARY KEY) SYNC CONFLICT KEEP LATEST"
    ));
    // Statement kinds other than CREATE TABLE still never qualify for the
    // exemption, even naming a vigil_ table — unchanged from
    // `vigil_namespace_exemption_only_covers_vigil_tables` above.
    assert!(would_be_flagged_as_unauthorized_ddl(
        "ALTER TABLE vigil_settings_local ADD COLUMN extra TEXT"
    ));
}

/// Pins cold-review-r4 finding 17 directly: before this fix, the scan filtered
/// on `literal_names_a_table` (a broad "could a name be parsed out of this at
/// all" test), which exempted an interpolated-name template exactly the same
/// way it exempted the legitimate bare conflict-policy suffix — the dodge the
/// production comment at `settings_store.rs:152-160` says the guard exists to
/// prevent ("a template whose name is interpolated hides which table is being
/// created from the one check that exists to see it"). The fix narrows the
/// exemption to the exact clause-fragment shape, so an unparseable name on an
/// otherwise-complete CREATE/ALTER/DROP TABLE literal is now suspicious, not
/// exempt, and falls through to the namespace check where it is flagged.
#[test]
fn an_unparseable_table_name_is_flagged_not_exempted() {
    for interpolated in [
        "CREATE TABLE {} (",
        "CREATE TABLE ",
        "ALTER TABLE {} ADD COLUMN extra TEXT",
        "DROP TABLE ",
    ] {
        // The broader name parse genuinely cannot recover a name from these —
        // that is the whole point of the scenario, and the fix does not
        // change this parse. If this ever started returning `Some`, the
        // scenario below would no longer be testing "unparseable", so pin it
        // explicitly rather than assuming it.
        assert!(
            ddl_named_table(interpolated).is_none(),
            "expected no parseable table name in {interpolated:?}, got {:?}",
            ddl_named_table(interpolated)
        );
        assert!(
            would_be_flagged_as_unauthorized_ddl(interpolated),
            "a DDL literal with no parseable table name must be treated as suspicious and \
             flagged, never as exempt: {interpolated:?}"
        );
    }

    // The exemption stays narrow: only the exact clause fragment is let
    // through, not merely "any literal ddl_named_table also fails to parse
    // a name for" — proving the exemption is keyed on shape, not on absence
    // of a name.
    let exempt = " SYNC CONFLICT KEEP LATEST";
    assert!(ddl_named_table(exempt).is_none());
    assert!(is_exempt_clause_fragment(exempt));
    assert!(!would_be_flagged_as_unauthorized_ddl(exempt));
}

/// A known NUISANCE, not a hole (cold-review-arc2-r5 finding 11): `CREATE
/// TABLE IF NOT EXISTS vigil_x (` fails this guard closed. `ddl_table_name`
/// reads the identifier right after the `CREATE TABLE ` keyword pair as the
/// table name — `IF`, here — which does not start with `vigil_`, so a
/// genuinely Vigil-owned declaration spelled with `IF NOT EXISTS` would be
/// refused as unauthorized DDL even though it names no foreign table at
/// all. This is the SAFE direction to fail (a legitimate declaration is
/// blocked and has to drop `IF NOT EXISTS` or the guard has to learn the
/// clause explicitly — never a foreign declaration slipping through), so it
/// is not fixed here; this test exists so the behavior is a documented,
/// asserted fact instead of a surprise the next author has to rediscover.
#[test]
fn create_table_if_not_exists_fails_closed_as_a_documented_nuisance_not_a_hole() {
    let literal = "CREATE TABLE IF NOT EXISTS vigil_x (";
    assert_eq!(
        ddl_table_name(literal),
        Some("IF".to_string()),
        "the parser reads the token right after CREATE TABLE literally, so IF NOT EXISTS reads \
         as a table named IF: {:?}",
        ddl_table_name(literal)
    );
    assert!(
        !is_vigil_owned_table_declaration(literal),
        "a name of `IF` never starts with vigil_, so this genuinely Vigil-owned declaration is \
         NOT recognized as owned"
    );
    assert!(
        would_be_flagged_as_unauthorized_ddl(literal),
        "and is therefore flagged — failing closed on a legitimate declaration rather than open \
         on a foreign one"
    );
}

/// The exact evasion this test demonstrates and permanently pins (cold-
/// review-arc2-r5 finding 11): splitting a DDL KEYWORD itself, not just the
/// table name, across two string literals that are concatenated at
/// runtime — `format!("{}TABLE cg_evidence (", "CREATE ")` — produces two
/// literals, `"{}TABLE cg_evidence ("` and `"CREATE "`, NEITHER of which
/// `ddl_literal` classifies as DDL on its own, so the per-literal scan in
/// `vigil_production_fabric_uses_authenticated_iroh` sees nothing to flag.
/// [`format_call_reconstructions`] closes this by reconstructing what a
/// `format!`/`write!`/`writeln!` call's own literal arguments actually spell
/// once assembled — the same class of adjacent-literal-concatenation dodge
/// the guard already existed to prevent (`settings_store.rs:152-160`),
/// caught at the call site instead of per literal.
#[test]
fn a_ddl_keyword_split_across_a_format_call_is_still_caught() {
    let planted = r#"
        fn __planted_split_keyword_evasion() -> String {
            format!("{}TABLE cg_evidence (", "CREATE ")
        }
    "#;
    let reconstructed = format_call_reconstructions(planted);
    assert!(
        reconstructed
            .iter()
            .any(|text| text == "CREATE TABLE cg_evidence ("),
        "the format! call's own literal arguments must reconstruct to the full DDL text the \
         split was built to hide: {reconstructed:?}"
    );
    assert!(
        reconstructed.iter().any(|text| ddl_literal(text)),
        "and the reconstruction must be classified as DDL, which is what actually closes the \
         evasion: {reconstructed:?}"
    );

    // Negative control: an ordinary format! call built entirely from
    // non-DDL text must reconstruct to something `ddl_literal` still
    // rejects, so this check has real teeth rather than flagging every
    // format! call unconditionally.
    let ordinary = r#"
        fn __ordinary_format_call() -> String {
            format!("{} connected on port {}", "camera", 8080)
        }
    "#;
    let ordinary_reconstructed = format_call_reconstructions(ordinary);
    assert!(
        ordinary_reconstructed.iter().all(|text| !ddl_literal(text)),
        "an ordinary format! call must not be misclassified as DDL: {ordinary_reconstructed:?}"
    );
}

/// The positive counterpart to the source scan above: rather than reading no
/// production DDL as proof of purity, this reads Vigil's own GENERATED
/// declarations (`settings_store::table_declarations` /
/// `vigil_consumer_schema_statements`, the exact statements the
/// consumer-table seam executes — see `settings_store_schema.rs` for the
/// established pattern of asserting against the EXECUTED statement rather
/// than only the Rust-side field) and proves the conflict policy actually
/// reaches the engine for the tables that need it, and only those:
/// every table that travels (`UpOnly`/`DownOnly`) declares `SYNC CONFLICT
/// KEEP LATEST`, every table that never syncs does not, and every created
/// table stays inside Vigil's own `vigil_` namespace.
#[test]
fn vigil_settings_tables_declare_sync_conflict_policy_exactly_where_they_travel() {
    let declarations = table_declarations();
    let statements = vigil_consumer_schema_statements();
    assert_eq!(
        declarations.len(),
        statements.len(),
        "one executed statement per declared table: {statements:?}"
    );
    assert!(
        !declarations.is_empty(),
        "expected Vigil to declare at least one settings table"
    );

    for (table, statement) in declarations.iter().zip(statements.iter()) {
        let created_table = ddl_table_name(statement).unwrap_or_else(|| {
            panic!(
                "{}'s executed statement must be a CREATE TABLE statement naming its table: \
                 {statement:?}",
                table.name
            )
        });
        assert!(
            created_table.starts_with("vigil_") && created_table != "vigil_",
            "{}'s executed statement must create a table inside Vigil's own `vigil_` namespace: \
             {statement:?}",
            table.name
        );

        let travels = matches!(table.sync_class, SyncClass::UpOnly | SyncClass::DownOnly);
        let declares_keep_latest = statement
            .to_ascii_uppercase()
            .contains("SYNC CONFLICT KEEP LATEST");
        assert_eq!(
            declares_keep_latest, travels,
            "{}'s {:?} table must declare `SYNC CONFLICT KEEP LATEST` in the statement the seam \
             executes if and only if it actually travels across machines — a table that never \
             syncs has no cross-machine conflict to arbitrate, and a table that DOES travel must \
             not silently keep the engine's write-once-wins default, which would drop every \
             operator change after the first push: {statement:?}",
            table.name, table.sync_class
        );
    }
}
