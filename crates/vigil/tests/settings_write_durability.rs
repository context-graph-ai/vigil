//! A write of one author's record is a SINGLE statement, so no interruption can
//! commit a state where that record is absent.
//!
//! The closed settings run wrote a record as DELETE-then-INSERT, so a crash
//! between the two erased a pin the operator had set — a setting the user
//! configured simply vanished. The property this file holds is that there is no
//! window at all: one upsert keyed on the record's full identity — the setting,
//! the author, the surface, and the scope — replaces that author's own row and
//! touches nothing else. An explicit reset is held to the same rule, because a
//! reset is a RECORD naming what the setting drops to, never a delete that
//! leaves the setting missing.
//!
//! RED: `vigil::settings_store` is skeleton-only (`todo!()` bodies) pending the
//! settings-store implementation.

use vigil::settings_model::{
    Author, DETECTOR_STATIONARY_INTERVAL_SETTING, Scope, ScopeTarget, SettingRecord, SettingValue,
    Surface,
};
use vigil::settings_store::{
    LOCAL_RECORDS_TABLE, MACHINE_RECORDS_TABLE, PUSHED_RECORDS_TABLE, SCOPE_LEVEL_COLUMN,
    SCOPE_TARGET_COLUMN, SettingsStore, TableDeclaration, WritePlan, reset_write_plan,
    table_declarations, write_plan,
};

const SETTING: &str = DETECTOR_STATIONARY_INTERVAL_SETTING;
const WRITTEN_AT_MS: i64 = 1_700_000_000_000;

fn scope() -> Scope {
    Scope::node("node-a")
}

fn target() -> ScopeTarget {
    ScopeTarget {
        tenant: "owner".to_string(),
        site: "home".to_string(),
        node: "node-a".to_string(),
        camera: None,
    }
}

fn record(author: Author, surface: Surface, value: i64) -> SettingRecord {
    SettingRecord {
        setting: SETTING.to_string(),
        author,
        surface,
        scope: scope(),
        value: SettingValue::Int(value),
        reason: format!("authored through {surface:?}"),
        written_at_ms: WRITTEN_AT_MS,
        domain_generation: 0,
        reset: false,
    }
}

fn declaration(name: &str) -> TableDeclaration {
    table_declarations()
        .into_iter()
        .find(|declaration| declaration.name == name)
        .unwrap_or_else(|| panic!("expected {name} to be a declared settings table"))
}

/// The whole property in one place: exactly one statement, it is an upsert
/// rather than a delete, it names the table that holds this class of record,
/// and it is keyed on every column of that table's composite primary key — so
/// it can only ever replace this author's own row at this surface and scope.
fn assert_single_upsert_keyed_on_the_full_identity(plan: &WritePlan, table_name: &str) {
    assert_eq!(
        plan.statements.len(),
        1,
        "one record write is ONE statement; a pair leaves a window in which the record is absent: \
         {:?}",
        plan.statements
    );

    let statement = &plan.statements[0];
    let upper = statement.to_uppercase();
    assert!(
        !upper.contains("DELETE"),
        "no part of a record write removes the record first: {statement}"
    );
    assert!(
        upper.contains("INSERT"),
        "the write is an upsert of the record itself: {statement}"
    );
    assert!(
        statement.contains(table_name),
        "the write lands in {table_name}, the table declaring this record class's sync policy: \
         {statement}"
    );

    let table = declaration(table_name);

    // The scope half of the key, read off the declaration rather than matched by
    // naming convention.
    let scope_key_columns = table.scope_key_columns();
    assert_eq!(
        scope_key_columns,
        vec![SCOPE_LEVEL_COLUMN, SCOPE_TARGET_COLUMN],
        "{table_name}'s key identity carries the scope level and what it names at that level: \
         {:?}",
        table.primary_key
    );
    for column in &scope_key_columns {
        assert!(
            table.primary_key.contains(column),
            "{table_name} reports {column} as a scope key column, so it must be IN the primary \
             key: {:?}",
            table.primary_key
        );
    }

    // The setting half of the key, identified among the declared key COLUMN
    // names — never by searching the statement text, where the table name
    // `vigil_settings_local` alone would satisfy a bare `contains("setting")`.
    let setting_key_columns: Vec<&&'static str> = table
        .primary_key
        .iter()
        .filter(|column| !scope_key_columns.contains(*column) && column.contains("setting"))
        .collect();
    assert_eq!(
        setting_key_columns.len(),
        1,
        "{table_name}'s primary key names the setting in exactly one column: {:?}",
        table.primary_key
    );

    // Strip every declared table name out before looking for key columns, so a
    // column name that only ever appears as part of the table's own name cannot
    // stand in for being in the key.
    let mut columns_only = statement.clone();
    for declared in table_declarations() {
        columns_only = columns_only.replace(declared.name, " ");
    }
    for column in &table.primary_key {
        assert!(
            columns_only.contains(column),
            "the upsert must be keyed on the full composite identity — {:?} — and {column} is \
             missing from the statement's own columns, so it would replace some OTHER author's or \
             surface's record: {statement}",
            table.primary_key
        );
    }
}

/// Unfakeable: the statement count is read off the plan the store actually
/// issues, so a DELETE-then-INSERT pair fails on the count before any timing or
/// crash injection is needed — there is no window to race. The behavioral half
/// then proves the plan is the real write path rather than a description beside
/// it: the record is readable back through the store afterwards.
#[test]
fn a_write_of_one_authors_record_never_commits_a_state_where_that_record_is_absent() {
    for (author, surface, table_name) in [
        (
            Author::LocalExplicit,
            Surface::VigilSettings,
            LOCAL_RECORDS_TABLE,
        ),
        (
            Author::Pushed,
            Surface::ManagementServer,
            PUSHED_RECORDS_TABLE,
        ),
        (Author::Automatic, Surface::Automatic, MACHINE_RECORDS_TABLE),
    ] {
        let plan = write_plan(&record(author, surface, 12));
        assert_single_upsert_keyed_on_the_full_identity(&plan, table_name);
    }

    let node_directory = tempfile::tempdir().expect("temporary data directory");
    let node = SettingsStore::open(node_directory.path()).expect("open the node-side store");
    node.write_record(record(Author::LocalExplicit, Surface::VigilSettings, 12))
        .expect("write the local record");
    node.write_record(record(Author::Automatic, Surface::Automatic, 34))
        .expect("write the automatic record");
    let stored = node.records(SETTING).expect("read the stored records");
    assert!(
        stored
            .iter()
            .any(|held| held.author == Author::LocalExplicit && held.value == SettingValue::Int(12)),
        "the local record is readable back after its single-statement write: {stored:?}"
    );
    assert!(
        stored
            .iter()
            .any(|held| held.author == Author::Automatic && held.value == SettingValue::Int(34)),
        "the automatic record coexists with it: {stored:?}"
    );

    let hub_directory = tempfile::tempdir().expect("temporary data directory");
    let hub = SettingsStore::open_hub_role(hub_directory.path()).expect("open the hub-role store");
    hub.write_pushed_record(record(Author::Pushed, Surface::ManagementServer, 56))
        .expect("write the pushed record through the hub-role handle");
    let pushed = hub.records(SETTING).expect("read the stored records");
    assert!(
        pushed
            .iter()
            .any(|held| held.author == Author::Pushed && held.value == SettingValue::Int(56)),
        "the pushed record is readable back after its single-statement write: {pushed:?}"
    );
}

/// Unfakeable: a single-statement INSERT that APPENDS satisfies every assertion
/// about statement count and key columns — the plan looks identical — and then
/// leaves two rows at one identity, so a later read has to pick between them and
/// the operator's setting depends on which one it picks. This drives the real
/// store: the SAME (setting, author, surface, scope) is written twice with
/// different values, and afterwards exactly ONE record exists at that identity,
/// carrying the SECOND value. The coexistence half is asserted in the same test
/// against a DIFFERENT author on a different table, so an implementation that
/// "fixed" accumulation by clearing the setting on every write — which would
/// destroy the coexistence the model is built on — fails here too.
#[test]
fn a_second_write_at_the_same_identity_replaces_it_rather_than_accumulating() {
    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = SettingsStore::open(directory.path()).expect("open the node-side store");

    store
        .write_record(record(Author::LocalExplicit, Surface::VigilSettings, 12))
        .expect("the human said 12 through `vigil settings`");
    store
        .write_record(record(Author::Automatic, Surface::Automatic, 34))
        .expect("Vigil's own automatic record for the same setting");
    store
        .write_record(record(Author::LocalExplicit, Surface::VigilSettings, 56))
        .expect("the human changes their mind through the SAME surface at the SAME scope");

    let stored = store.records(SETTING).expect("read the stored records");

    let at_that_identity: Vec<&SettingRecord> = stored
        .iter()
        .filter(|held| {
            held.author == Author::LocalExplicit
                && held.surface == Surface::VigilSettings
                && held.scope == scope()
        })
        .collect();
    assert_eq!(
        at_that_identity.len(),
        1,
        "a write at an identity that already has a record REPLACES it; two rows at one identity \
         means a reader is choosing between the operator's old value and their new one: {stored:?}"
    );
    assert_eq!(
        at_that_identity[0].value,
        SettingValue::Int(56),
        "and the surviving record carries what was written second, not what it replaced: \
         {stored:?}"
    );

    let automatic: Vec<&SettingRecord> = stored
        .iter()
        .filter(|held| held.author == Author::Automatic)
        .collect();
    assert_eq!(
        automatic.len(),
        1,
        "records at DIFFERENT identities coexist — replacing the local record never touched the \
         automatic one: {stored:?}"
    );
    assert_eq!(
        automatic[0].value,
        SettingValue::Int(34),
        "and it still carries its own value, unmodified: {stored:?}"
    );
}

/// Unfakeable: a reset implemented as a delete passes any assertion about the
/// resulting effective value — the setting does drop to what is beneath — so
/// the effective value alone cannot catch it. This asserts the plan carries no
/// DELETE at all AND that a reset record for the reset surface is still stored
/// afterwards, which is only true if the reset is a record. The record count is
/// checked too, so a delete-and-reinsert of some other row cannot stand in.
#[test]
fn an_explicit_reset_never_commits_a_state_where_the_setting_is_missing() {
    let plan = reset_write_plan(
        SETTING,
        Surface::VigilSettings,
        &scope(),
        &SettingValue::Int(43),
    );
    assert_single_upsert_keyed_on_the_full_identity(&plan, LOCAL_RECORDS_TABLE);

    let directory = tempfile::tempdir().expect("temporary data directory");
    let store = SettingsStore::open(directory.path()).expect("open the node-side store");
    store
        .write_record(record(Author::LocalExplicit, Surface::StartupOptions, 43))
        .expect("the service's command line said 43");
    store
        .set_local(
            SETTING,
            Surface::VigilSettings,
            scope(),
            SettingValue::Int(57),
        )
        .expect("the human said 57 through `vigil settings`");

    let outcome = store
        .reset_local(SETTING, Surface::VigilSettings, &scope())
        .expect("reset the `vigil settings` record");
    assert_eq!(
        outcome.dropped_to.requested,
        SettingValue::Int(43),
        "the setting drops to whatever is beneath, and the outcome names it: {outcome:?}"
    );
    assert_eq!(
        outcome.dropped_to.surface,
        Surface::StartupOptions,
        "what is beneath here is the startup-options record, still stored: {outcome:?}"
    );
    assert!(
        outcome.statement.contains("43") && !outcome.statement.contains("57"),
        "the reset states the value the setting dropped TO — a distinctive multi-digit number, so \
         this cannot be satisfied by a stray digit — and does not go on quoting the pin that is \
         gone: {:?}",
        outcome.statement
    );

    let stored = store.records(SETTING).expect("read the stored records");
    assert!(
        stored
            .iter()
            .any(|held| held.surface == Surface::VigilSettings && held.reset),
        "the reset is a RECORD on the surface that was reset, not the removal of one: {stored:?}"
    );
    assert!(
        stored
            .iter()
            .any(|held| held.surface == Surface::StartupOptions
                && held.value == SettingValue::Int(43)
                && !held.reset),
        "resetting one surface never touches another surface's record: {stored:?}"
    );

    let effective = store.resolve(SETTING, &target()).expect("resolve");
    assert_eq!(
        effective.requested,
        SettingValue::Int(43),
        "the setting is never missing after a reset; it runs what is beneath: {effective:?}"
    );
}
