//! The three sync classes Vigil's settings tables must declare, and the rule
//! that no settings table is ever keyed without scope.
//!
//! Sync direction and the write constraint are declared per TABLE in the store
//! beneath Vigil, so the three classes of settings record need three separately
//! declared tables: records authored at this node travel up only, records a hub
//! pushed travel down only and are writable at the pushed rank through a
//! scope-labelled handle alone, and automatic, auto-adjusted, and achieved
//! records never travel at all. A table keyed on the setting name without its
//! scope would spread one node's hardware-shaped values across a fleet and let
//! one node's reset clobber another node's pin.
//!
//! RED: `vigil::settings_store` is skeleton-only (`todo!()` bodies) pending the
//! settings-store implementation.

use vigil::settings_store::{
    ECHO_LEDGER_TABLE, LOCAL_RECORDS_TABLE, MACHINE_RECORDS_TABLE, PUSHED_RECORDS_TABLE,
    PUSHED_WRITE_SCOPE_LABEL, SyncClass, TableDeclaration, table_declarations,
    vigil_consumer_schema_statements,
};

fn declaration(name: &str) -> TableDeclaration {
    table_declarations()
        .into_iter()
        .find(|declaration| declaration.name == name)
        .unwrap_or_else(|| panic!("expected {name} to be a declared settings table"))
}

/// The statement the consumer-table seam actually EXECUTES for one table — not
/// the Rust-side declaration beside it. Every policy assertion below reads this,
/// because a policy that lives only in a Rust field is a policy the engine never
/// enforces.
fn executed_statement(name: &str) -> String {
    let statements = vigil_consumer_schema_statements();
    let matching: Vec<&String> = statements
        .iter()
        .filter(|statement| statement.contains(name))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "exactly one executed statement declares {name}: {statements:?}"
    );
    matching[0].clone()
}

/// The clause the store beneath Vigil reads a table's sync direction from. These
/// are that engine's own spellings; a direction that never becomes one of them
/// is a direction the engine never sees.
fn sync_clause(class: SyncClass) -> &'static str {
    match class {
        SyncClass::UpOnly => "SYNC PUSH ONLY",
        SyncClass::DownOnly => "SYNC PULL ONLY",
        SyncClass::Never => "SYNC OFF",
    }
}

/// Assert the declared sync class REACHES the executed statement, and that no
/// other direction is declared there — so a table whose Rust field says one
/// thing and whose executed DDL says another, or says nothing and inherits the
/// engine's travelling default, fails here.
fn assert_sync_class_reaches_the_executed_statement(table: &TableDeclaration) {
    // Whitespace-normalized so the assertion binds the clause the engine reads,
    // not how the DDL happens to be laid out across lines.
    let executed = executed_statement(table.name)
        .to_ascii_uppercase()
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ");
    let expected = sync_clause(table.sync_class);
    assert!(
        executed.contains(expected),
        "{}'s declared {:?} must reach the statement the seam executes as `{expected}`, or the \
         engine never applies it and the direction is a Rust-side comment: {executed}",
        table.name,
        table.sync_class
    );
    for other in [SyncClass::UpOnly, SyncClass::DownOnly, SyncClass::Never] {
        if other == table.sync_class {
            continue;
        }
        assert!(
            !executed.contains(sync_clause(other)),
            "{} declares one direction; `{}` is also present, so which one the engine applies is \
             a coin toss: {executed}",
            table.name,
            sync_clause(other)
        );
    }
    assert!(
        !executed.contains("SYNC TWO WAY"),
        "{} must never be declared two-way — every settings table travels in at most one \
         direction: {executed}",
        table.name
    );
}

/// The labels a statement constrains WRITES to, read out of the two forms the
/// store beneath Vigil accepts: `SCOPE_LABEL ('...')`, which constrains WRITES
/// ONLY, and `SCOPE_LABEL_READ ('...') WRITE ('...')`, where the two lists are
/// stated apart and only the WRITE list constrains writing. A read-only label
/// yields no write constraint here, which is the whole point: it does not stop
/// a node from authoring.
///
/// The plain form used to be described here as constraining "reads and writes
/// together", and it does not: the engine narrows a read from a
/// `SCOPE_LABEL_READ` set and from nothing else, so a table carrying only the
/// plain form is fully visible to every handle that may open it. That is why
/// the stopped-store settings reader declares no scope label — a node must
/// still SEE what its hub pushed, whatever it may write.
fn write_constrained_labels(statement: &str) -> Vec<String> {
    let upper = statement.to_ascii_uppercase();

    let labels_after = |from: usize| -> (Vec<String>, usize) {
        let open = from
            + statement[from..]
                .find('(')
                .unwrap_or_else(|| panic!("a scope-label clause names its labels: {statement}"));
        let close = open
            + statement[open..]
                .find(')')
                .unwrap_or_else(|| panic!("a scope-label clause closes its labels: {statement}"));
        let labels = statement[open + 1..close]
            .split(',')
            .map(|piece| {
                piece
                    .trim()
                    .trim_matches(|character| character == '\'' || character == '"')
                    .to_string()
            })
            .collect();
        (labels, close + 1)
    };

    if let Some(read_at) = upper.find("SCOPE_LABEL_READ") {
        let (_read_labels, after_read) = labels_after(read_at + "SCOPE_LABEL_READ".len());
        let write_at = after_read
            + upper[after_read..].find("WRITE").unwrap_or_else(|| {
                panic!("the split scope-label form names its WRITE labels: {statement}")
            });
        let (write_labels, _) = labels_after(write_at + "WRITE".len());
        return write_labels;
    }

    match upper.find("SCOPE_LABEL") {
        Some(simple_at) => labels_after(simple_at + "SCOPE_LABEL".len()).0,
        None => Vec::new(),
    }
}

/// Unfakeable: the direction is asserted BOTH on the declaration Vigil hands the
/// consumer-table seam and on the statement that seam executes, so a table whose
/// Rust field reads up-only while its executed DDL declares nothing — and
/// therefore travels two ways on the engine's default — fails here rather than
/// being caught only when a local pin has already reached another node. Up-only
/// is also asserted to carry NO write scope label: constraining local writes to
/// a server-labelled handle would stop the human at the machine from authoring
/// at all.
#[test]
fn local_records_are_declared_up_only() {
    let local = declaration(LOCAL_RECORDS_TABLE);

    assert_eq!(
        local.sync_class,
        SyncClass::UpOnly,
        "records authored at this node travel to the hub for visibility and restore, and never \
         come back down onto another node: {local:?}"
    );
    assert_sync_class_reaches_the_executed_statement(&local);
    assert_eq!(
        local.write_scope_label, None,
        "a local record is authored by the human at this machine through the ordinary node-side \
         handle; a write scope label on the local table would refuse them: {local:?}"
    );
    assert!(
        write_constrained_labels(&executed_statement(LOCAL_RECORDS_TABLE)).is_empty(),
        "and the executed statement constrains local writes to no label at all: {}",
        executed_statement(LOCAL_RECORDS_TABLE)
    );
}

/// Unfakeable: transport direction alone does not stop a node from
/// manufacturing a fleet instruction for itself and honoring it locally — a
/// down-only table stops a row from travelling, not from existing. The write
/// scope label is what makes "a node never authors a pushed record" a boundary
/// instead of a convention, so this asserts BOTH the direction and the label,
/// and that the label really reaches the declared DDL rather than living only
/// in a Rust-side field the engine never sees — and reaches it as a WRITE
/// constraint specifically, since a read-only label leaves a node perfectly able
/// to author a fleet instruction for itself.
#[test]
fn pushed_records_are_declared_down_only_with_the_write_scope_constraint() {
    let pushed = declaration(PUSHED_RECORDS_TABLE);

    assert_eq!(
        pushed.sync_class,
        SyncClass::DownOnly,
        "pushed records are authored at the hub and applied at nodes; a node's row must never \
         travel up as a fleet instruction: {pushed:?}"
    );
    assert_sync_class_reaches_the_executed_statement(&pushed);
    assert_eq!(
        pushed.write_scope_label,
        Some(PUSHED_WRITE_SCOPE_LABEL),
        "the pushed table is writable at the pushed rank only through a handle carrying the \
         server scope label: {pushed:?}"
    );

    let executed = executed_statement(PUSHED_RECORDS_TABLE);
    let write_labels = write_constrained_labels(&executed);
    assert!(
        write_labels
            .iter()
            .any(|label| label == PUSHED_WRITE_SCOPE_LABEL),
        "the WRITE constraint must be declared in the statement the seam executes — the simple \
         `SCOPE_LABEL ('{PUSHED_WRITE_SCOPE_LABEL}')` form or the split form's `WRITE` list. A \
         label that only constrains READS leaves every node free to author at the pushed rank, \
         which is exactly the second control path this boundary exists to close. Write labels \
         found: {write_labels:?} in: {executed}"
    );
}

/// Unfakeable: these are facts about one machine's hardware, cameras, and load.
/// Reading the declared class for both the machine-records table and the
/// per-surface echo ledger means a table that quietly defaults to travelling —
/// which is what an unregistered table does in the store beneath Vigil — fails
/// here instead of leaking one node's achieved values across a fleet.
#[test]
fn automatic_auto_adjusted_and_achieved_records_never_sync() {
    for name in [MACHINE_RECORDS_TABLE, ECHO_LEDGER_TABLE] {
        let table = declaration(name);
        assert_eq!(
            table.sync_class,
            SyncClass::Never,
            "{name} holds facts about this machine alone — automatic values, auto-adjusted \
             values, what this node achieved, and what Vigil last wrote to and read from each \
             surface — and none of it travels: {table:?}"
        );
        assert_sync_class_reaches_the_executed_statement(&table);
        assert!(
            write_constrained_labels(&executed_statement(name)).is_empty(),
            "{name} is written by this node about itself, so the executed statement constrains \
             its writes to no label: {}",
            executed_statement(name)
        );
        assert_eq!(
            table.write_scope_label, None,
            "{name} is written by this node about itself; a server scope label would refuse the \
             node its own machine record: {table:?}"
        );
    }
}

/// Unfakeable: the check runs over EVERY declared table rather than a named
/// list, so a table added later without scope in its key fails here on the day
/// it is declared. It also proves each key column really appears in the DDL the
/// seam executes, so a primary key that is complete in the Rust declaration and
/// absent from the created table cannot pass. Scope columns are matched by
/// meaning (a level column and a target column) rather than by one frozen
/// spelling, so the guard binds the property, not a naming choice.
#[test]
fn no_settings_table_is_keyed_without_scope() {
    let declarations = table_declarations();
    assert!(
        !declarations.is_empty(),
        "Vigil must declare its settings tables through the consumer-table seam"
    );

    for table in &declarations {
        let key = &table.primary_key;
        assert!(
            key.iter().any(|column| column.contains("setting")),
            "{}'s primary key must name the setting: {key:?}",
            table.name
        );
        assert!(
            key.iter()
                .any(|column| column.contains("scope") && column.contains("level")),
            "{}'s primary key must carry the scope LEVEL — tenant, site, node, or camera — so one \
             node's value can never be applied by another: {key:?}",
            table.name
        );
        assert!(
            key.iter()
                .any(|column| column.contains("scope") && column.contains("target")),
            "{}'s primary key must carry the thing the scope names at that level, so a per-camera \
             record is not confused with the node's: {key:?}",
            table.name
        );
        for column in key {
            assert!(
                table.ddl.contains(column),
                "{}'s primary-key column {column} must exist in the DDL the seam executes: {}",
                table.name,
                table.ddl
            );
        }
    }

    let statements = vigil_consumer_schema_statements();
    assert_eq!(
        statements.len(),
        declarations.len(),
        "every declared table is handed to the consumer-table seam as its own statement: \
         {statements:?}"
    );
    for table in &declarations {
        assert!(
            statements.contains(&table.ddl),
            "{}'s declared DDL must be what the seam is handed, executed verbatim: {statements:?}",
            table.name
        );
    }
}
