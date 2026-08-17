//! The settings store: the tables Vigil declares inside the context-graph
//! database every install opens, and the reads and writes over them.
//!
//! The store is the source of truth. Input surfaces are authors that write into
//! it; the runtime resolves from it. Three classes of record need three sync
//! policies, so they need three tables: local records travel up only, pushed
//! records travel down only and are writable at the pushed rank by a
//! scope-labelled handle alone, and automatic, auto-adjusted, and achieved
//! records never travel at all.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use context_graph::{
    CgError, ConsumerSchema, ConsumerTable, EmbedderConfig, ScopeLabel, Store, StoreConfig,
};
use contextdb_core::Value;

use crate::PersistedClock;
use crate::settings_domains::{DomainDeclaration, governing_domain};
use crate::settings_model::{
    Author, ControlState, EffectiveSetting, HeldReason, HeldRecord, Scope, ScopeLevel, ScopeTarget,
    SettingRecord, SettingValue, SettingsError, Surface,
};

/// The namespace Vigil declares its tables under through the context-graph
/// consumer-table seam.
pub const VIGIL_TABLE_NAMESPACE: &str = "vigil";

/// The table holding records authored at this node by a human or by Vigil's own
/// surfaces. Up only.
pub const LOCAL_RECORDS_TABLE: &str = "vigil_settings_local";

/// The table holding records a hub pushed. Down only, and writable only through
/// a handle carrying the server scope label.
pub const PUSHED_RECORDS_TABLE: &str = "vigil_settings_pushed";

/// The table holding automatic values, auto-adjusted values, and the record of
/// what this node actually achieved. Never syncs.
pub const MACHINE_RECORDS_TABLE: &str = "vigil_settings_machine";

/// The per-surface, per-setting ledger of what Vigil last wrote out and last
/// read back. A fact about this machine; never syncs.
pub const ECHO_LEDGER_TABLE: &str = "vigil_settings_echo_ledger";

/// The column naming a record's scope level. Part of every settings table's
/// primary key: nothing about settings syncs until scope is in the key.
pub const SCOPE_LEVEL_COLUMN: &str = "scope_level";

/// The column naming what a record's scope applies to at that level.
pub const SCOPE_TARGET_COLUMN: &str = "scope_target";

/// The scope label a handle must carry to write the pushed table.
pub const PUSHED_WRITE_SCOPE_LABEL: &str = "server";

/// The scope label the ordinary node-side handle is opened with, always.
pub const NODE_SCOPE_LABEL: &str = "edge";

/// How a declared table travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncClass {
    /// Authored at the node, travels to the hub for visibility and restore.
    UpOnly,
    /// Authored at the hub, applied at nodes.
    DownOnly,
    /// A fact about one machine; never travels.
    Never,
}

/// One table Vigil declares, with the policy it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDeclaration {
    pub name: &'static str,
    /// The exact DDL Vigil hands the consumer-table seam, executed verbatim.
    pub ddl: String,
    pub sync_class: SyncClass,
    /// The scope label constraining writes, when the table carries one.
    pub write_scope_label: Option<&'static str>,
    /// The column names forming the record's primary key.
    pub primary_key: Vec<&'static str>,
}

/// The column naming which setting a record is about.
pub const SETTING_NAME_COLUMN: &str = "setting_name";

/// The column naming the surface a record was authored through.
pub const SURFACE_COLUMN: &str = "surface";

/// The column carrying the scope label a pushed row is written at, which is
/// what the engine constrains a handle's writes against.
pub const SCOPE_LABEL_COLUMN: &str = "scope_label";

/// The primary key every settings table carries: the setting, the surface that
/// authored it, and the scope it applies at. Author is implied by the table a
/// record lives in, so it is stored beside the key rather than inside it.
fn primary_key() -> Vec<&'static str> {
    vec![
        SETTING_NAME_COLUMN,
        SURFACE_COLUMN,
        SCOPE_LEVEL_COLUMN,
        SCOPE_TARGET_COLUMN,
    ]
}

/// The columns every settings record carries, in write order.
fn record_columns(include_scope_label: bool) -> Vec<&'static str> {
    let mut columns = vec![
        SETTING_NAME_COLUMN,
        "author",
        SURFACE_COLUMN,
        SCOPE_LEVEL_COLUMN,
        SCOPE_TARGET_COLUMN,
        "value_type",
        "value",
        "reason",
        "written_at_ms",
        "domain_generation",
        "reset_record",
    ];
    if include_scope_label {
        columns.push(SCOPE_LABEL_COLUMN);
    }
    columns
}

/// The column list, as declared, for one settings table. The pushed table
/// carries the scope-label column the engine reads a write constraint from;
/// the others do not, because a node writes them about itself.
fn column_declarations(include_scope_label: bool) -> String {
    let mut declared = String::from(
        "setting_name TEXT NOT NULL, author TEXT NOT NULL, surface TEXT NOT NULL, \
         scope_level TEXT NOT NULL, scope_target TEXT NOT NULL, value_type TEXT NOT NULL, \
         value TEXT NOT NULL, reason TEXT NOT NULL, written_at_ms INTEGER NOT NULL, \
         domain_generation INTEGER NOT NULL, reset_record INTEGER NOT NULL",
    );
    if include_scope_label {
        declared.push_str(", scope_label TEXT SCOPE_LABEL ('");
        declared.push_str(PUSHED_WRITE_SCOPE_LABEL);
        declared.push_str("')");
    }
    declared.push_str(", PRIMARY KEY (setting_name, surface, scope_level, scope_target)");
    declared
}

/// The declaration for one table. Two things are deliberate here.
///
/// The sync direction is spelled by the storage engine's own vocabulary rather
/// than by a string Vigil keeps beside it, so a direction Vigil declares and a
/// direction the engine applies can never drift.
///
/// And `head` is the whole `CREATE TABLE <name> (` opening, passed in as a
/// literal by each caller rather than assembled from the name. Vigil is
/// forbidden from authoring DDL for anything outside its own `vigil_`
/// namespace, and the guard that enforces that reads the SOURCE literals: a
/// template whose name is interpolated hides which table is being created from
/// the one check that exists to see it. Writing the name in the literal is what
/// makes the exemption verifiable. A head that named a different table than
/// `name` is caught by the schema tests, which look each declaration's own DDL
/// up by its name.
fn declare(
    name: &'static str,
    head: &'static str,
    sync_class: SyncClass,
    write_scope_label: Option<&'static str>,
) -> TableDeclaration {
    let ddl = format!(
        "{head}{columns}) {direction}{conflict}",
        columns = column_declarations(write_scope_label.is_some()),
        direction = sync_class.engine_direction().sql(),
        conflict = sync_class.conflict_clause(),
    );
    TableDeclaration {
        name,
        ddl,
        sync_class,
        write_scope_label,
        primary_key: primary_key(),
    }
}

impl SyncClass {
    /// The storage engine's own direction for this class. The DDL words, the
    /// persisted declaration and the sync filter all come from there.
    pub fn engine_direction(self) -> contextdb_core::table_meta::SyncDirection {
        use contextdb_core::table_meta::SyncDirection;
        match self {
            SyncClass::UpOnly => SyncDirection::Push,
            SyncClass::DownOnly => SyncDirection::Pull,
            SyncClass::Never => SyncDirection::None,
        }
    }

    /// Every row this store writes for a table that actually travels is
    /// current truth, written through an upsert (`ON CONFLICT ... DO
    /// UPDATE`, see `upsert_statement`) so a later write to the same key
    /// replaces the earlier one locally. The engine's own default conflict
    /// policy across machines is the opposite — write-once, keep the first
    /// value ever pushed for a key — which would let a hub silently drop
    /// every operator change after the first push of a setting. So a table
    /// that travels (`UpOnly`, `DownOnly`) declares `SYNC CONFLICT KEEP
    /// LATEST` explicitly; a table that never syncs (`Never`) has no
    /// cross-machine conflict to arbitrate.
    fn conflict_clause(self) -> &'static str {
        match self {
            SyncClass::UpOnly | SyncClass::DownOnly => " SYNC CONFLICT KEEP LATEST",
            SyncClass::Never => "",
        }
    }
}

impl TableDeclaration {
    /// The scope columns inside this table's primary key, so a test binds the
    /// exact key identity rather than a naming convention.
    pub fn scope_key_columns(&self) -> Vec<&'static str> {
        self.primary_key
            .iter()
            .copied()
            .filter(|column| *column == SCOPE_LEVEL_COLUMN || *column == SCOPE_TARGET_COLUMN)
            .collect()
    }
}

/// The column naming the content Vigil last posted to a surface.
pub const LAST_WRITE_OUT_COLUMN: &str = "last_write_out";

/// The column naming the content Vigil last read back from that surface.
pub const LAST_READ_BACK_COLUMN: &str = "last_read_back";

/// The scope level an echo-ledger row is keyed at. The row is about one
/// deployment's own conversation with one surface, which is not a value that
/// applies at a tenant, site, node or camera — so it says what it is rather
/// than borrowing a level it would then have to invent a target for.
pub const DEPLOYMENT_SCOPE_LEVEL: &str = "deployment";

/// And what that level names: this deployment itself.
pub const DEPLOYMENT_SCOPE_TARGET: &str = "self";

/// The echo ledger's own declaration. It carries what Vigil last wrote to and
/// last read back from one surface for one setting, which is neither a value
/// nor authored by anyone, so it gets its own columns rather than being pressed
/// into the settings-record shape. It is keyed like every other declared table,
/// at the deployment the row is a fact about.
fn declare_echo_ledger(head: &'static str) -> TableDeclaration {
    let ddl = format!(
        "{head}setting_name TEXT NOT NULL, surface TEXT NOT NULL, scope_level TEXT NOT NULL, \
         scope_target TEXT NOT NULL, {LAST_WRITE_OUT_COLUMN} TEXT NOT NULL, \
         {LAST_READ_BACK_COLUMN} TEXT NOT NULL, PRIMARY KEY (setting_name, surface, scope_level, \
         scope_target)) {direction}",
        direction = SyncClass::Never.engine_direction().sql(),
    );
    TableDeclaration {
        name: ECHO_LEDGER_TABLE,
        ddl,
        sync_class: SyncClass::Never,
        write_scope_label: None,
        primary_key: primary_key(),
    }
}

/// Every table Vigil declares, in declaration order.
pub fn table_declarations() -> Vec<TableDeclaration> {
    vec![
        declare(
            LOCAL_RECORDS_TABLE,
            "CREATE TABLE vigil_settings_local (",
            SyncClass::UpOnly,
            None,
        ),
        declare(
            PUSHED_RECORDS_TABLE,
            "CREATE TABLE vigil_settings_pushed (",
            SyncClass::DownOnly,
            Some(PUSHED_WRITE_SCOPE_LABEL),
        ),
        declare(
            MACHINE_RECORDS_TABLE,
            "CREATE TABLE vigil_settings_machine (",
            SyncClass::Never,
            None,
        ),
        declare_echo_ledger("CREATE TABLE vigil_settings_echo_ledger ("),
    ]
}

/// The consumer-schema declaration Vigil hands `Store::open_with_consumer_schema`.
/// Returned as its rendered statements so the shape is inspectable without a
/// store.
pub fn vigil_consumer_schema_statements() -> Vec<String> {
    table_declarations()
        .into_iter()
        .map(|table| table.ddl)
        .collect()
}

/// The table one author's records live in. The sync policy a record needs is a
/// property of the table it sits in, so the author decides the table.
fn table_for(author: Author) -> &'static str {
    match author {
        Author::LocalExplicit => LOCAL_RECORDS_TABLE,
        Author::Pushed => PUSHED_RECORDS_TABLE,
        Author::Automatic => MACHINE_RECORDS_TABLE,
    }
}

/// One upsert of one record at its full identity. Never a delete-then-insert:
/// a pair leaves a window in which the operator's setting is simply absent.
fn upsert_statement(table: &'static str) -> String {
    let include_scope_label = table == PUSHED_RECORDS_TABLE;
    let columns = record_columns(include_scope_label);
    let placeholders: Vec<String> = columns.iter().map(|column| format!("${column}")).collect();
    let key = primary_key();
    let updates: Vec<String> = columns
        .iter()
        .filter(|column| !key.contains(column))
        .map(|column| format!("{column} = ${column}"))
        .collect();
    format!(
        "INSERT INTO {table} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {}",
        columns.join(", "),
        placeholders.join(", "),
        key.join(", "),
        updates.join(", "),
    )
}

/// The statements one record write issues. A write of one author's record is a
/// single statement so no interruption can commit a state where that record is
/// absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritePlan {
    pub statements: Vec<String>,
}

/// The statements the store would issue to write `record`.
pub fn write_plan(record: &SettingRecord) -> WritePlan {
    WritePlan {
        statements: vec![upsert_statement(table_for(record.author))],
    }
}

/// The statements the store would issue for an explicit reset of one record.
/// A reset is a record, not a delete.
pub fn reset_write_plan(
    _setting: &str,
    surface: Surface,
    _scope: &Scope,
    _dropping_to: &SettingValue,
) -> WritePlan {
    WritePlan {
        statements: vec![upsert_statement(table_for(surface.author()))],
    }
}

/// Which role a handle opens the store in. The node-side handle is opened with
/// an edge scope label always; the hub-role handle exists only in the harness
/// and in a future hub binary, never behind a command-line flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleRole {
    /// The ordinary node-side handle. Refused a write to the pushed table.
    Node,
    /// The hub-role handle. Writes the pushed table successfully.
    Hub,
}

impl HandleRole {
    /// The scope label a handle in this role carries. The node-side handle
    /// carries the edge label always, so the engine — not a Vigil-side check —
    /// is what refuses it the pushed table.
    pub fn scope_label(self) -> &'static str {
        match self {
            HandleRole::Node => NODE_SCOPE_LABEL,
            HandleRole::Hub => PUSHED_WRITE_SCOPE_LABEL,
        }
    }
}

/// The consumer-schema declaration handed to the context-graph seam.
fn vigil_consumer_schema() -> ConsumerSchema {
    ConsumerSchema {
        namespace: VIGIL_TABLE_NAMESPACE.to_string(),
        tables: table_declarations()
            .into_iter()
            .map(|table| ConsumerTable {
                name: table.name.to_string(),
                ddl: table.ddl,
            })
            .collect(),
    }
}

/// A live handle over the settings tables.
pub struct SettingsStore {
    path: PathBuf,
    role: HandleRole,
    store: Arc<Store>,
    clock: PersistedClock,
}

impl SettingsStore {
    /// Open the ordinary node-side handle.
    ///
    /// `data_dir` is the DEPLOYMENT DIRECTORY, never a store file path — the
    /// store file's own location inside it is Vigil's to decide, and a caller
    /// that had to name the file would be re-deriving a bootstrap location the
    /// deployment already fixes. Reading a deployment that has never started
    /// never creates a store.
    pub fn open(data_dir: &Path) -> Result<Self, SettingsError> {
        Self::open_role(data_dir, HandleRole::Node, PersistedClock::default())
    }

    /// Open the hub-role handle over the same deployment directory.
    ///
    /// Harness-side only, and now structurally so: the door is compiled only
    /// under `test-support`, which no artifact enables. A shipped build has no
    /// way to obtain a handle that may author at the pushed rank, so a node
    /// cannot report a fleet instruction nobody in the fleet issued. Pushed
    /// records reach a real node the one way they are meant to — applied by
    /// the sync engine from a hub — never written by this node for itself.
    #[cfg(feature = "test-support")]
    pub fn open_hub_role(data_dir: &Path) -> Result<Self, SettingsError> {
        Self::open_role(data_dir, HandleRole::Hub, PersistedClock::default())
    }

    fn open_role(
        data_dir: &Path,
        role: HandleRole,
        clock: PersistedClock,
    ) -> Result<Self, SettingsError> {
        let path = Self::store_path(data_dir);
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                SettingsError::Store(format!(
                    "could not create the deployment directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let config = StoreConfig {
            db_path: path.clone(),
            default_text_embedder: Some(EmbedderConfig::disabled()),
            ..StoreConfig::default()
        };
        let labels = BTreeSet::from([ScopeLabel::new(role.scope_label())]);
        let store = Store::open_with_consumer_schema_scoped(
            config,
            Vec::new(),
            vigil_consumer_schema(),
            labels,
        )
        .map_err(map_open_error)?;
        Ok(Self {
            path,
            role,
            store: Arc::new(store),
            clock,
        })
    }

    /// Where the store file lives inside a deployment directory. One place
    /// fixes the relationship so no caller has to assume a filename.
    pub fn store_path(data_dir: &Path) -> PathBuf {
        data_dir.join("store.contextgraph")
    }

    /// The deployment's store file this handle is open over.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// This deployment's echo ledger, over the store this handle already holds.
    /// The ledger is rows in this same store, so a caller that has a handle
    /// never opens a second one to reach it.
    pub fn echo_ledger(&self) -> Result<crate::settings_reflection::EchoLedger, SettingsError> {
        crate::settings_reflection::EchoLedger::over_store(Arc::clone(&self.store))
    }

    /// The detector class allowlist, as INDICES, that the detector construction
    /// path consumes for this deployment — the name-to-index resolution itself,
    /// so a caller binds what the detector is actually built with rather than
    /// the resolved value one step upstream.
    pub fn detector_class_allowlist(
        &self,
        target: &ScopeTarget,
    ) -> Result<Vec<usize>, SettingsError> {
        let names = match self.resolve(crate::settings_model::DETECTOR_CLASSES_SETTING, target) {
            Ok(effective) => match effective.requested {
                SettingValue::List(values) => values,
                SettingValue::Text(value) => vec![value],
                _ => Vec::new(),
            },
            Err(SettingsError::Store(_)) => crate::settings_backends::default_detection_classes()
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            Err(other) => return Err(other),
        };
        Ok(names
            .iter()
            .filter_map(|name| crate::settings_backends::detection_class_index(name))
            .collect())
    }

    /// Open the node-side handle against a caller-supplied persisted-time
    /// source. Every timestamp a record carries comes through this seam, so a
    /// test pins ordering instead of sleeping for the clock to move.
    pub fn open_with_clock(
        path: &Path,
        clock: crate::PersistedClock,
    ) -> Result<Self, SettingsError> {
        Self::open_role(path, HandleRole::Node, clock)
    }

    /// Open the hub-role handle against a caller-supplied persisted-time
    /// source. Compiled only under `test-support`, for the reason
    /// [`SettingsStore::open_hub_role`] gives.
    #[cfg(feature = "test-support")]
    pub fn open_hub_role_with_clock(
        path: &Path,
        clock: crate::PersistedClock,
    ) -> Result<Self, SettingsError> {
        Self::open_role(path, HandleRole::Hub, clock)
    }

    /// A view of this same open handle for reads and node-authored local
    /// operations — no second open against the same directory, so a caller
    /// (a test writing a pushed row through the hub role and reading it back
    /// as an ordinary node would, or the take-over path bringing a just-landed
    /// pushed value into force) sees exactly what a node sees.
    ///
    /// Authoring at the pushed rank is structurally absent from the returned
    /// type — `NodeView` carries no `write_pushed_record` and no generic
    /// dispatch that could reach it — rather than refused at runtime, because
    /// the underlying handle is still the hub-labeled one this store opened
    /// and the engine cannot itself refuse a write on a handle that carries
    /// the hub's scope label (the upstream operation to re-narrow an open
    /// handle's label does not exist yet — a filed upstream gap, not closed
    /// here). A future write-capable node view needs that operation.
    pub fn node_view(&self) -> NodeView {
        NodeView {
            inner: SettingsStore {
                path: self.path.clone(),
                role: HandleRole::Node,
                store: Arc::clone(&self.store),
                clock: self.clock.clone(),
            },
        }
    }

    pub fn role(&self) -> HandleRole {
        self.role
    }

    /// Write one record at its own author, surface, and scope. Records coexist:
    /// this never removes another author's or another surface's record.
    pub fn write_record(&self, record: SettingRecord) -> Result<(), SettingsError> {
        if record.author == Author::Pushed {
            return self.write_pushed_record(record);
        }
        self.execute_record_write(record)
    }

    /// Write a record into the down-only pushed table, in exactly the shape hub
    /// sync would apply it. The engine itself refuses this write on a
    /// node-side handle — the handle is opened with its scope label at
    /// `open_role`, so the boundary is enforced where the handle's authority
    /// actually lives, not re-checked here on Vigil's own say-so.
    pub fn write_pushed_record(&self, mut record: SettingRecord) -> Result<(), SettingsError> {
        record.author = Author::Pushed;
        record.surface = Surface::ManagementServer;
        self.execute_record_write(record)
    }

    fn execute_record_write(&self, mut record: SettingRecord) -> Result<(), SettingsError> {
        record.written_at_ms = self.next_stamp(record.written_at_ms)?;
        let table = table_for(record.author);
        let params = write_params(&record, table == PUSHED_RECORDS_TABLE);
        self.store
            .sync_database()
            .execute(&upsert_statement(table), &params)
            .map_err(|error| map_engine_error(&error))?;
        Ok(())
    }

    /// The persisted stamp one write carries. A caller that pinned its own
    /// instant keeps it; otherwise the seamed clock supplies it, and a stamp is
    /// never allowed to land at or behind the newest record already stored, so
    /// "which local surface authored most recently" is answerable without any
    /// test having to sleep for the clock to move.
    fn next_stamp(&self, supplied: i64) -> Result<i64, SettingsError> {
        if supplied != 0 {
            return Ok(supplied);
        }
        let now = i64::try_from(self.clock.unix_millis()).unwrap_or(i64::MAX);
        let mut newest = 0_i64;
        for table in [
            LOCAL_RECORDS_TABLE,
            PUSHED_RECORDS_TABLE,
            MACHINE_RECORDS_TABLE,
        ] {
            let result = self
                .store
                .sync_database()
                .execute(
                    &format!("SELECT written_at_ms FROM {table}"),
                    &HashMap::new(),
                )
                .map_err(|error| map_engine_error(&error))?;
            for row in &result.rows {
                if let Some(Value::Int64(stamp)) = row.first() {
                    newest = newest.max(*stamp);
                }
            }
        }
        Ok(now.max(newest.saturating_add(1)))
    }

    /// Apply a run of hub-authored records in order, all-or-nothing in the
    /// sense that matters: the caller has already established that the whole
    /// instruction may proceed, and each record lands at the pushed rank
    /// through the one write path that exists for it.
    ///
    /// This seam exists so the pushed-rank write itself has exactly one home.
    /// A hub instruction is legitimate, but the ability to write at that rank
    /// must not be reachable by name from anywhere else in the tree — a second
    /// site for it is the second control path the settings model forbids.
    ///
    /// It runs only where a hub-role handle can be obtained at all, which is
    /// the harness — see [`SettingsStore::open_hub_role`].
    #[cfg(feature = "test-support")]
    pub(crate) fn apply_hub_authored(
        &self,
        records: Vec<SettingRecord>,
    ) -> Result<(), SettingsError> {
        for record in records {
            self.write_pushed_record(record)?;
        }
        Ok(())
    }

    /// Every stored record for one setting, effective or not, across all three
    /// tables.
    pub fn records(&self, setting: &str) -> Result<Vec<SettingRecord>, SettingsError> {
        let mut records = Vec::new();
        let params =
            HashMap::from([("setting_name".to_string(), Value::Text(setting.to_string()))]);
        for table in [
            LOCAL_RECORDS_TABLE,
            PUSHED_RECORDS_TABLE,
            MACHINE_RECORDS_TABLE,
        ] {
            let statement = format!(
                "SELECT setting_name, author, surface, scope_level, scope_target, value_type, \
                 value, reason, written_at_ms, domain_generation, reset_record FROM {table} \
                 WHERE setting_name = $setting_name"
            );
            let result = self
                .store
                .sync_database()
                .execute(&statement, &params)
                .map_err(|error| map_engine_error(&error))?;
            for row in &result.rows {
                records.push(record_from_row(row)?);
            }
        }
        records.sort_by(|left, right| {
            left.written_at_ms
                .cmp(&right.written_at_ms)
                .then_with(|| left.surface.cmp(&right.surface))
        });
        Ok(records)
    }

    /// Resolve one setting at one target: the gate first, then the author
    /// ranking, then the most specific scope within the winning author.
    /// Answerable from the store at any time, running or stopped.
    pub fn resolve(
        &self,
        setting: &str,
        target: &ScopeTarget,
    ) -> Result<EffectiveSetting, SettingsError> {
        self.resolve_excluding(setting, target, None)
    }

    /// Resolution with one record's identity left out, so a reset can ask what
    /// the setting drops to before its own reset record is written — one
    /// write, never a placeholder followed by a correction.
    fn resolve_excluding(
        &self,
        setting: &str,
        target: &ScopeTarget,
        excluded: Option<(Surface, &Scope)>,
    ) -> Result<EffectiveSetting, SettingsError> {
        let candidates: Vec<SettingRecord> = self
            .records(setting)?
            .into_iter()
            .filter(|record| record.scope.covers(target) && !record.reset)
            .filter(|record| {
                excluded
                    .map(|(surface, scope)| !(record.surface == surface && record.scope == *scope))
                    .unwrap_or(true)
            })
            .collect();

        let governing = governing_domain(setting);
        let domain_on = match governing.as_ref() {
            Some(domain) => self.domain_is_on(domain, target)?,
            None => false,
        };

        let mut grandfathered = false;
        let effective: Option<SettingRecord>;
        if domain_on {
            let domain = governing.as_ref().expect("a domain is on for this setting");
            let predating: Vec<SettingRecord> = candidates
                .iter()
                .filter(|record| {
                    record.author != Author::Automatic
                        && record.domain_generation < domain.generation
                })
                .cloned()
                .collect();
            if let Some(pin) = highest_ranked(&predating) {
                grandfathered = true;
                effective = Some(pin);
            } else {
                effective = highest_ranked(
                    &candidates
                        .iter()
                        .filter(|record| record.author == Author::Automatic)
                        .cloned()
                        .collect::<Vec<SettingRecord>>(),
                );
            }
        } else {
            effective = highest_ranked(&candidates);
        }

        let effective = match effective {
            Some(record) => record,
            None => automatic_floor(setting, target).ok_or_else(|| {
                SettingsError::Store(format!(
                    "{setting} has no stored record and no automatic value to fall back on"
                ))
            })?,
        };

        // A domain that has never asserted anything is not managing anything
        // yet: on a deployment where nothing at all is stored, the value is
        // simply Vigil's own choice. Managed-by is what the operator is told
        // when the domain — or its switch — has actually been written down.
        let domain_has_asserted = match governing.as_ref() {
            Some(domain) => {
                !self.records(setting)?.is_empty() || !self.records(domain.switch)?.is_empty()
            }
            None => false,
        };
        let control_state = if domain_on && domain_has_asserted && !grandfathered {
            ControlState::ManagedBy(
                governing
                    .as_ref()
                    .expect("a domain is on for this setting")
                    .switch
                    .to_string(),
            )
        } else {
            match effective.author {
                Author::Automatic => ControlState::Automatic,
                Author::Pushed => ControlState::SetByManagementServer,
                Author::LocalExplicit => ControlState::SetByYou,
            }
        };

        let mut held = Vec::new();
        for record in candidates {
            if record.author == effective.author
                && record.surface == effective.surface
                && record.scope == effective.scope
            {
                continue;
            }
            let reason = if domain_on
                && domain_has_asserted
                && !grandfathered
                && record.author != Author::Automatic
            {
                HeldReason::DormantUnderDomain {
                    domain: governing
                        .as_ref()
                        .expect("a domain is on for this setting")
                        .switch
                        .to_string(),
                }
            } else if record.author == effective.author && effective.author == Author::LocalExplicit
            {
                HeldReason::OutrankedByLocalSurface {
                    by_surface: effective.surface,
                }
            } else {
                HeldReason::Shadowed {
                    by_author: effective.author,
                    by_surface: effective.surface,
                }
            };
            let statement = held_statement(&record, &reason, &effective);
            held.push(HeldRecord {
                record,
                reason,
                statement,
            });
        }

        let inherited = target.camera.is_some() && effective.scope.level != ScopeLevel::Camera;
        // Requested comes from the store; running is a fact about a live
        // process. Asking the deployment and asking the process are different
        // questions, and a value that has arrived but is not yet running says
        // so instead of pretending.
        // Where nobody authored a value, the choice reported IS what this
        // process is running: the automatic choice and the running machine are
        // one authority, and answering with the pre-move choice describes a
        // machine that does not exist.
        // The choice, what is running, and what is still outstanding are read
        // together, so a change landing right now cannot be caught
        // half-published and read as a restart. All THREE are inside: where
        // nobody authored a value the choice is itself read from the registry
        // the landing publishes into, so taking it outside would read the
        // pre-move choice against the post-move value in force — a node
        // reported as needing a restart onto the backend it is already
        // running.
        let (requested, running, pending, reason) = {
            let _published = crate::settings_application::hold_publication();
            let requested = crate::settings_projection::automatic_choice(
                setting,
                effective.author,
                &effective.value,
            );
            // Read here with the choice, from the same act: a reason taken
            // outside the hold could describe the machine before the move it
            // is printed beside.
            let reason = crate::settings_projection::automatic_reason(
                setting,
                effective.author,
                &effective.value,
                &effective.reason,
            );
            let running = crate::settings_projection::running_value(setting);
            let pending = pending_cause_for(setting, &running, &requested);
            (requested, running, pending, reason)
        };
        Ok(EffectiveSetting {
            setting: setting.to_string(),
            requested,
            running,
            pending,
            control_state,
            author: effective.author,
            surface: effective.surface,
            scope: effective.scope,
            reason,
            authored_at_ms: effective.written_at_ms,
            held,
            grandfathered,
            inherited,
        })
    }

    /// Whether the domain governing a setting is currently on. The switch is
    /// an ordinary setting, resolved by the ordinary rules, and nothing
    /// governs a switch — so this never recurses.
    fn domain_is_on(
        &self,
        domain: &DomainDeclaration,
        target: &ScopeTarget,
    ) -> Result<bool, SettingsError> {
        let effective = self.resolve(domain.switch, target)?;
        Ok(matches!(effective.requested, SettingValue::Bool(true)))
    }

    /// Whether a pushed record on `switch` would become the effective value,
    /// asked before a take-over writes anything: a record already outranking
    /// the hub means the domain never goes off, so nothing may be written.
    /// Asked only by the take-over path, which is harness-side.
    #[cfg(feature = "test-support")]
    pub(crate) fn pushed_switch_would_take_effect(
        &self,
        switch: &str,
        scope: &Scope,
        target: &ScopeTarget,
    ) -> Result<bool, SettingsError> {
        let outranking = self.records(switch)?.into_iter().any(|record| {
            !record.reset
                && record.author == Author::LocalExplicit
                && (record.scope.covers(target) || record.scope == *scope)
        });
        Ok(!outranking)
    }

    /// An operator write through a local surface. Runs the automatic-management
    /// gate before the ranking, validates the value, and refuses at write time
    /// with a cause and a remedy rather than storing something that is then
    /// ignored.
    pub fn set_local(
        &self,
        setting: &str,
        surface: Surface,
        scope: Scope,
        value: SettingValue,
    ) -> Result<SettingRecord, SettingsError> {
        let target = target_for(&scope);
        self.refuse_unwritable(setting, &value)?;
        self.run_gate(setting, &scope, &target, None)?;
        let mut record = SettingRecord::local(
            setting,
            surface,
            scope,
            value,
            format!("set through the {}", surface.as_str()),
        );
        record.domain_generation = crate::settings_domains::membership_generation(setting);
        record.written_at_ms = self.next_stamp(0)?;
        self.execute_record_write(record.clone())?;
        Ok(record)
    }

    /// The write-time refusals: a value that fails its declared validation, a
    /// backend this artifact does not carry, a class the model does not know,
    /// and the service identity, which changes only through its own deliberate
    /// operation.
    fn refuse_unwritable(&self, setting: &str, value: &SettingValue) -> Result<(), SettingsError> {
        Self::refuse_setting_no_ordinary_verb_may_touch(setting, &value.to_string())?;
        crate::settings_backends::validate_setting_value(setting, value)
            .map_err(SettingsError::Refused)
    }

    /// The refusal both ordinary verbs owe. Setting a value and dropping one
    /// back are the same act against this node's service identity: either way
    /// the identifier the Home Assistant device hangs off moves, and either way
    /// the operator has to be told that before it does. One source, so the two
    /// verbs cannot come to state different consequences — or, as they did,
    /// one of them state none at all.
    ///
    /// It takes the proposed value as text rather than a resolved
    /// [`SettingValue`], because a reset must be refused BEFORE anything is
    /// resolved: resolution of the identity legitimately fails when no record
    /// is stored and nothing automatic stands behind it, and an operator who
    /// asked to reset the identity is owed the orphaned-history consequence,
    /// never a store-read error that says nothing about what they tried to do.
    fn refuse_setting_no_ordinary_verb_may_touch(
        setting: &str,
        proposed: &str,
    ) -> Result<(), SettingsError> {
        if setting == crate::settings_model::SERVICE_IDENTITY_SETTING {
            return Err(SettingsError::Refused(
                crate::service_identity::refuse_ordinary_edit(proposed),
            ));
        }
        Ok(())
    }

    /// The automatic-management gate, run before the ranking on every write.
    fn run_gate(
        &self,
        setting: &str,
        scope: &Scope,
        target: &ScopeTarget,
        record_generation: Option<u64>,
    ) -> Result<(), SettingsError> {
        let states = self.domain_states(target)?;
        match crate::settings_domains::gate_write(setting, scope, &states, record_generation) {
            crate::settings_domains::GateDecision::Refused(refusal) => {
                Err(SettingsError::Refused(refusal))
            }
            _ => Ok(()),
        }
    }

    /// Every domain switch and whether it is currently on.
    fn domain_states(&self, target: &ScopeTarget) -> Result<Vec<(String, bool)>, SettingsError> {
        let mut states = Vec::new();
        for domain in crate::settings_domains::domain_roster() {
            states.push((
                domain.switch.to_string(),
                self.domain_is_on(&domain, target)?,
            ));
        }
        Ok(states)
    }

    /// Explicit reset of one surface's record. Drops the setting to whatever is
    /// beneath and names it.
    pub fn reset_local(
        &self,
        setting: &str,
        surface: Surface,
        scope: &Scope,
    ) -> Result<ResetOutcome, SettingsError> {
        // A reset is a change too: dropping the identity back to whatever sits
        // beneath it moves the identifier just as surely as setting a new one,
        // so it meets the same refusal. It is refused BEFORE the drop is
        // resolved, because resolution is entitled to fail — the identity has
        // no automatic value standing behind it — and an operator who asked to
        // reset it is owed the orphaned-history consequence rather than a
        // store-read error about a value the refusal was never going to use.
        Self::refuse_setting_no_ordinary_verb_may_touch(
            setting,
            "whatever would sit beneath the record being reset",
        )?;
        let target = target_for(scope);
        let dropped_to = self.resolve_excluding(setting, &target, Some((surface, scope)))?;
        let mut record = SettingRecord::local(
            setting,
            surface,
            scope.clone(),
            dropped_to.requested.clone(),
            format!("reset through the {}", surface.as_str()),
        );
        record.reset = true;
        record.domain_generation = crate::settings_domains::membership_generation(setting);
        record.written_at_ms = self.next_stamp(0)?;
        self.execute_record_write(record)?;

        let statement = format!(
            "{setting} drops to {} — {}, {} ({}).",
            dropped_to.requested,
            dropped_to.control_state.label(),
            dropped_to.author.as_str(),
            dropped_to.surface.as_str(),
        );
        Ok(ResetOutcome {
            setting: setting.to_string(),
            dropped_to,
            statement,
        })
    }

    /// Apply what a file surface asserts right now. A surface authors only when
    /// what it says differs from what Vigil last read from or last wrote to that
    /// same surface; a key it no longer names clears that surface's record and
    /// nothing else. A missing or unparsable file surface asserts nothing and
    /// clears nothing, and startup options never clear by absence.
    pub fn apply_surface_snapshot(
        &self,
        snapshot: &SurfaceSnapshot,
    ) -> Result<Vec<SurfaceChange>, SettingsError> {
        self.apply_snapshot(snapshot, true)
    }

    /// Apply what a file surface asserts about ONE camera, at that camera's own
    /// scope: the same authoring and the same clearing by absence as the
    /// deployment-wide half, one scope down, so a value typed into a camera's
    /// row governs that camera and deleting it clears that camera's record.
    ///
    /// The echo ledger is deliberately not consulted here. It answers one
    /// question — is this content Vigil's own last write to this surface coming
    /// back? — against the whole options record Vigil posts, and Vigil mirrors
    /// only deployment-wide values: the add-on schema has no per-camera address
    /// to write to. Feeding one camera's entries in as if they were the whole
    /// surface would compare two different things and, worse, overwrite what
    /// the ledger holds about the surface as a whole. A camera value that
    /// matches its stored record is recognized as unchanged and rewritten
    /// nowhere, so nothing churns.
    pub fn apply_camera_surface_snapshot(
        &self,
        snapshot: &SurfaceSnapshot,
    ) -> Result<Vec<SurfaceChange>, SettingsError> {
        self.apply_snapshot(snapshot, false)
    }

    fn apply_snapshot(
        &self,
        snapshot: &SurfaceSnapshot,
        through_echo_ledger: bool,
    ) -> Result<Vec<SurfaceChange>, SettingsError> {
        if !snapshot.present_and_parsing {
            // A missing, unreadable, or half-written surface asserts nothing
            // and clears nothing. Absence of the file is not absence of the
            // values.
            return Ok(Vec::new());
        }

        let mut changes = Vec::new();
        let stored = self.records_authored_through(snapshot.surface, &snapshot.scope)?;
        // What Vigil last wrote to this surface. Deciding what came back by
        // comparing it against the store misses the case the ledger exists for:
        // a mirrored value authored through ANOTHER surface leaves this one
        // with no record of its own, so Vigil's own write reads as a human edit
        // and every boot re-authors the operator's options with a record nobody
        // typed.
        let mut ledger = match through_echo_ledger {
            true => Some(self.echo_ledger()?),
            false => None,
        };
        let observed = crate::settings_reflection::OptionsRecord {
            entries: snapshot.entries.clone(),
        };

        for (setting, value) in &snapshot.entries {
            if ledger.as_ref().is_some_and(|ledger| {
                ledger.classify_read_back(snapshot.surface, setting, &observed)
                    == crate::settings_reflection::EchoVerdict::Echo
            }) {
                changes.push(SurfaceChange::AuthorsNothing {
                    setting: setting.clone(),
                    why: "it is the content Vigil itself last wrote to this surface, so nobody \
                          typed it and a record saying an operator did would outrank the very pin \
                          it came from"
                        .to_string(),
                });
                continue;
            }
            if snapshot.surface == Surface::AddonOptions {
                if let Some((_, declared)) = snapshot
                    .declared_defaults
                    .iter()
                    .find(|(key, _)| key == setting)
                    && declared == value
                {
                    changes.push(SurfaceChange::AuthorsNothing {
                        setting: setting.clone(),
                        why: "it equals the add-on's own declared default, so nobody has said \
                              anything here"
                            .to_string(),
                    });
                    continue;
                }
                if value.is_null_or_empty() {
                    changes.push(SurfaceChange::AuthorsNothing {
                        setting: setting.clone(),
                        why: "the posted key is null or empty, which is a form field nobody \
                              filled in"
                            .to_string(),
                    });
                    continue;
                }
            }

            let unchanged = stored.iter().any(|record| {
                &record.setting == setting && &record.value == value && !record.reset
            });
            if unchanged {
                changes.push(SurfaceChange::Unchanged {
                    setting: setting.clone(),
                });
                continue;
            }

            match self.set_local(
                setting,
                snapshot.surface,
                snapshot.scope.clone(),
                value.clone(),
            ) {
                Ok(record) => changes.push(SurfaceChange::Authored(record)),
                Err(error) => changes.push(SurfaceChange::Refused {
                    setting: setting.clone(),
                    error,
                }),
            }
        }

        if snapshot.surface.clears_by_absence() {
            for record in stored {
                if record.reset
                    || snapshot
                        .entries
                        .iter()
                        .any(|(setting, _)| setting == &record.setting)
                {
                    continue;
                }
                self.reset_local(&record.setting, snapshot.surface, &snapshot.scope)?;
                changes.push(SurfaceChange::ClearedByAbsence {
                    setting: record.setting,
                    surface: snapshot.surface,
                });
            }
        }

        // Both halves of the loop are recorded, so the next pass compares
        // against the surface as it now stands rather than against content that
        // is two edits old.
        if let Some(ledger) = ledger.as_mut() {
            ledger.record_read_back(snapshot.surface, &observed)?;
        }

        Ok(changes)
    }

    /// Every record one surface authored at one scope, whatever setting it is
    /// about — what a surface "last said", which is how an unchanged surface
    /// is recognized as silent.
    fn records_authored_through(
        &self,
        surface: Surface,
        scope: &Scope,
    ) -> Result<Vec<SettingRecord>, SettingsError> {
        let table = table_for(surface.author());
        let params = HashMap::from([
            (
                "surface".to_string(),
                Value::Text(surface.as_str().to_string()),
            ),
            (
                "scope_level".to_string(),
                Value::Text(scope.level.as_str().to_string()),
            ),
            (
                "scope_target".to_string(),
                Value::Text(scope.target.clone()),
            ),
        ]);
        let statement = format!(
            "SELECT setting_name, author, surface, scope_level, scope_target, value_type, value, \
             reason, written_at_ms, domain_generation, reset_record FROM {table} WHERE \
             surface = $surface AND scope_level = $scope_level AND scope_target = $scope_target"
        );
        let result = self
            .store
            .sync_database()
            .execute(&statement, &params)
            .map_err(|error| map_engine_error(&error))?;
        result.rows.iter().map(|row| record_from_row(row)).collect()
    }

    /// Every camera one surface has a record at, whatever setting it is about.
    /// A camera the surface no longer lists is a camera it no longer says
    /// anything about, so the caller can give that camera its clearing pass
    /// too: dropping a camera's whole row has to mean the same thing as
    /// deleting the one value inside it.
    pub fn camera_scopes_authored_through(
        &self,
        surface: Surface,
    ) -> Result<Vec<String>, SettingsError> {
        let table = table_for(surface.author());
        let params = HashMap::from([
            (
                "surface".to_string(),
                Value::Text(surface.as_str().to_string()),
            ),
            (
                "scope_level".to_string(),
                Value::Text(ScopeLevel::Camera.as_str().to_string()),
            ),
        ]);
        let statement = format!(
            "SELECT scope_target FROM {table} WHERE surface = $surface AND \
             scope_level = $scope_level"
        );
        let result = self
            .store
            .sync_database()
            .execute(&statement, &params)
            .map_err(|error| map_engine_error(&error))?;
        let mut cameras: Vec<String> = Vec::new();
        for row in &result.rows {
            if let Some(Value::Text(camera)) = row.first()
                && !cameras.contains(camera)
            {
                cameras.push(camera.clone());
            }
        }
        Ok(cameras)
    }

    /// Every setting this deployment touches, resolved, for the operator
    /// surface. One projection; the socket answer and the direct read render
    /// from it identically.
    pub fn listing(&self, target: &ScopeTarget) -> Result<Vec<EffectiveSetting>, SettingsError> {
        let mut names: Vec<String> = Vec::new();
        for table in [
            LOCAL_RECORDS_TABLE,
            PUSHED_RECORDS_TABLE,
            MACHINE_RECORDS_TABLE,
        ] {
            let result = self
                .store
                .sync_database()
                .execute(
                    &format!("SELECT setting_name FROM {table}"),
                    &HashMap::new(),
                )
                .map_err(|error| map_engine_error(&error))?;
            for row in &result.rows {
                if let Some(Value::Text(name)) = row.first()
                    && !names.contains(name)
                {
                    names.push(name.clone());
                }
            }
        }
        // Two things never appear as ordinary setting lines. The service
        // identity is shown read-only on its own line, because the generic
        // set/reset path operates on setting lines and offering it there would
        // be offering a write path for a read-only value. A secret is shown as
        // a SOURCE line and never as a value: what the operator needs to see is
        // where it is coming from, not what it is.
        names.retain(|name| {
            name != crate::settings_model::SERVICE_IDENTITY_SETTING
                && crate::settings_environment::secret_environment_variable(name).is_none()
        });
        for declared in declared_settings() {
            if !names.iter().any(|name| name.as_str() == declared) {
                names.push(declared.to_string());
            }
        }
        names.sort();

        let mut listing = Vec::new();
        for name in names {
            match self.resolve(&name, target) {
                Ok(effective) => listing.push(effective),
                Err(SettingsError::Store(_)) => continue,
                Err(other) => return Err(other),
            }
        }
        Ok(listing)
    }
}

/// Anything that answers `resolve` the way a node-perspective read does.
/// [`SettingsStore::resolve`] never consults the handle's role, so the
/// full-access handle and [`NodeView`] answer identically — a caller that
/// only ever resolves settings (bringing a landed change into force,
/// mirroring it onto a surface) is written once against this trait instead
/// of twice against the two concrete types.
pub trait ResolvesSettings {
    fn resolve(
        &self,
        setting: &str,
        target: &ScopeTarget,
    ) -> Result<EffectiveSetting, SettingsError>;
}

impl ResolvesSettings for SettingsStore {
    fn resolve(
        &self,
        setting: &str,
        target: &ScopeTarget,
    ) -> Result<EffectiveSetting, SettingsError> {
        SettingsStore::resolve(self, setting, target)
    }
}

impl ResolvesSettings for NodeView {
    fn resolve(
        &self,
        setting: &str,
        target: &ScopeTarget,
    ) -> Result<EffectiveSetting, SettingsError> {
        self.inner.resolve(setting, target)
    }
}

/// A view over an already-open handle for reads and node-authored local
/// operations, returned by [`SettingsStore::node_view`]. Not a second open —
/// it shares the handle the store already holds — and not an authorization
/// check: authoring at the pushed rank (`write_pushed_record`, and the
/// generic `write_record` that can dispatch to it) is simply not a method
/// this type has, so there is nothing here to bypass or forget to refuse.
/// `set_local`, `reset_local`, and `apply_surface_snapshot` stay, because
/// node-authored local writes were never the fabricated boundary — the
/// engine genuinely permits them on any handle.
pub struct NodeView {
    inner: SettingsStore,
}

impl NodeView {
    /// The deployment's store file this view is open over.
    pub fn path(&self) -> &Path {
        self.inner.path()
    }

    pub fn role(&self) -> HandleRole {
        self.inner.role()
    }

    /// This deployment's echo ledger, over the store this view already holds.
    pub fn echo_ledger(&self) -> Result<crate::settings_reflection::EchoLedger, SettingsError> {
        self.inner.echo_ledger()
    }

    /// The detector class allowlist, as INDICES, that the detector
    /// construction path consumes for this deployment.
    pub fn detector_class_allowlist(
        &self,
        target: &ScopeTarget,
    ) -> Result<Vec<usize>, SettingsError> {
        self.inner.detector_class_allowlist(target)
    }

    /// Every stored record for one setting, effective or not, across all
    /// three tables.
    pub fn records(&self, setting: &str) -> Result<Vec<SettingRecord>, SettingsError> {
        self.inner.records(setting)
    }

    /// Resolve one setting at one target: the gate first, then the author
    /// ranking, then the most specific scope within the winning author.
    pub fn resolve(
        &self,
        setting: &str,
        target: &ScopeTarget,
    ) -> Result<EffectiveSetting, SettingsError> {
        self.inner.resolve(setting, target)
    }

    /// An operator write through a local surface. Node-authored, at the
    /// author ranks the engine already permits any handle to write.
    pub fn set_local(
        &self,
        setting: &str,
        surface: Surface,
        scope: Scope,
        value: SettingValue,
    ) -> Result<SettingRecord, SettingsError> {
        self.inner.set_local(setting, surface, scope, value)
    }

    /// Explicit reset of one surface's record. Drops the setting to whatever
    /// is beneath and names it.
    pub fn reset_local(
        &self,
        setting: &str,
        surface: Surface,
        scope: &Scope,
    ) -> Result<ResetOutcome, SettingsError> {
        self.inner.reset_local(setting, surface, scope)
    }

    /// Apply what a file surface asserts right now.
    pub fn apply_surface_snapshot(
        &self,
        snapshot: &SurfaceSnapshot,
    ) -> Result<Vec<SurfaceChange>, SettingsError> {
        self.inner.apply_surface_snapshot(snapshot)
    }

    /// Every setting this deployment touches, resolved, for the operator
    /// surface.
    pub fn listing(&self, target: &ScopeTarget) -> Result<Vec<EffectiveSetting>, SettingsError> {
        self.inner.listing(target)
    }
}

/// What an explicit reset dropped the setting to.
#[derive(Debug, Clone, PartialEq)]
pub struct ResetOutcome {
    pub setting: String,
    /// The record that is effective now the reset record is in place.
    pub dropped_to: EffectiveSetting,
    /// The sentence naming what the setting dropped to.
    pub statement: String,
}

/// What one file or startup surface asserts on one pass.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceSnapshot {
    pub surface: Surface,
    pub scope: Scope,
    /// Present and parsing, or not. A snapshot that is not present asserts
    /// nothing and clears nothing.
    pub present_and_parsing: bool,
    /// The keys this surface names right now, with their values.
    pub entries: Vec<(String, SettingValue)>,
    /// The add-on's own declared defaults, when this surface is the add-on
    /// options. A value equal to its declared default authors nothing.
    pub declared_defaults: Vec<(String, SettingValue)>,
}

/// What applying a surface snapshot did to one setting.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceChange {
    /// The surface authored a new or changed record.
    Authored(SettingRecord),
    /// The surface no longer names the key, so its own record is cleared.
    ClearedByAbsence { setting: String, surface: Surface },
    /// The surface said the same thing it last said; nothing was authored.
    Unchanged { setting: String },
    /// The value equals the add-on's declared default, or is null or empty, so
    /// it authors nothing.
    AuthorsNothing { setting: String, why: String },
    /// The write was refused; the refusal carries its cause and remedy.
    Refused {
        setting: String,
        error: SettingsError,
    },
}

/// Whether this deployment has a store at all. Asking what a never-started
/// deployment would run at is purely a READ: opening a store to answer would
/// create one, leaving a store on disk nobody chose to create.
pub fn store_exists(data_dir: &Path) -> bool {
    SettingsStore::store_path(data_dir).exists()
}

/// What closes the gap between what a listing answers with and what this
/// process is running, for one setting. One derivation, so a setting read
/// through the store and the same setting read off a never-started or degraded
/// deployment can never disagree about whether anything is waiting.
///
/// What an operator is waiting for differs per setting, and telling them to
/// restart when a camera reconnect or a model reload is what actually closes
/// the gap sends them to do the wrong thing. The per-setting declaration is the
/// one place that says which.
fn pending_cause_for(
    setting: &str,
    running: &Option<SettingValue>,
    effective: &SettingValue,
) -> Option<crate::settings_model::PendingCause> {
    match running {
        Some(running) if running == effective => None,
        // A gap this node is already closing by itself is not a restart. The
        // preparation the node started is outstanding, so what the operator is
        // waiting for is that preparation landing — and the surface says so
        // beside when it began and what it last reported.
        _ if crate::settings_application::live_transition(setting).is_some() => {
            Some(crate::settings_model::PendingCause::LiveTransition)
        }
        _ => crate::settings_application::pending_cause(setting)
            .or(Some(crate::settings_model::PendingCause::Restart)),
    }
}

/// What every setting resolves to with nothing stored anywhere: vigil's own
/// choice, with the value it actually runs and the reason it chose it. This is
/// the automatic floor standing on its own, for a deployment that has never
/// been started.
pub fn automatic_floor_listing(target: &ScopeTarget) -> Vec<EffectiveSetting> {
    let mut names: Vec<&'static str> = declared_settings();
    names.sort_unstable();
    names
        .into_iter()
        .filter_map(|setting| {
            let record = automatic_floor(setting, target)?;
            // Derived exactly as a stored setting's is, from what this process
            // is running against the value this listing itself answers with. A
            // declared-empty pending field would tell the operator of a machine
            // running a value nobody chose that there is nothing to wait for,
            // while the same setting read through the store names the remedy.
            // Read as one act — the choice included — for the reason the
            // stored listing above gives.
            let (requested, running, pending, reason) = {
                let _published = crate::settings_application::hold_publication();
                let requested = crate::settings_projection::automatic_choice(
                    setting,
                    record.author,
                    &record.value,
                );
                let reason = crate::settings_projection::automatic_reason(
                    setting,
                    record.author,
                    &record.value,
                    &record.reason,
                );
                let running = crate::settings_projection::running_value(setting);
                let pending = pending_cause_for(setting, &running, &requested);
                (requested, running, pending, reason)
            };
            Some(EffectiveSetting {
                setting: setting.to_string(),
                requested,
                running,
                pending,
                control_state: ControlState::Automatic,
                author: record.author,
                surface: record.surface,
                scope: record.scope,
                reason,
                authored_at_ms: 0,
                held: Vec::new(),
                grandfathered: false,
                inherited: false,
            })
        })
        .collect()
}

/// One echo-ledger row as the deployment's own store holds it. The content
/// itself is opaque here: what a surface's record looks like belongs to the
/// module that talks to that surface, and what belongs here is that the row is
/// stored, keyed, and declared never to travel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EchoRow {
    pub surface: String,
    pub setting: String,
    pub last_write_out: String,
    pub last_read_back: String,
}

/// Every echo-ledger row this deployment holds.
pub(crate) fn read_echo_rows(store: &Store) -> Result<Vec<EchoRow>, SettingsError> {
    let statement = format!(
        "SELECT setting_name, surface, {LAST_WRITE_OUT_COLUMN}, {LAST_READ_BACK_COLUMN} FROM \
         {ECHO_LEDGER_TABLE}"
    );
    let result = store
        .sync_database()
        .execute(&statement, &HashMap::new())
        .map_err(|error| map_engine_error(&error))?;
    result
        .rows
        .iter()
        .map(|row| {
            Ok(EchoRow {
                setting: text_at(row, 0)?,
                surface: text_at(row, 1)?,
                last_write_out: text_at(row, 2)?,
                last_read_back: text_at(row, 3)?,
            })
        })
        .collect()
}

/// Write one echo-ledger row at its own identity. One statement, so no
/// interruption can commit a state where the deployment's record of what it
/// wrote out is absent — which is the state in which it re-authors the
/// operator's own options as if a human had typed them.
pub(crate) fn write_echo_row(store: &Store, row: &EchoRow) -> Result<(), SettingsError> {
    let statement = format!(
        "INSERT INTO {ECHO_LEDGER_TABLE} (setting_name, surface, {SCOPE_LEVEL_COLUMN}, \
         {SCOPE_TARGET_COLUMN}, {LAST_WRITE_OUT_COLUMN}, {LAST_READ_BACK_COLUMN}) VALUES \
         ($setting_name, $surface, $scope_level, $scope_target, $last_write_out, $last_read_back) \
         ON CONFLICT (setting_name, surface, {SCOPE_LEVEL_COLUMN}, {SCOPE_TARGET_COLUMN}) DO \
         UPDATE SET {LAST_WRITE_OUT_COLUMN} = $last_write_out, {LAST_READ_BACK_COLUMN} = \
         $last_read_back"
    );
    let params = HashMap::from([
        (
            SETTING_NAME_COLUMN.to_string(),
            Value::Text(row.setting.clone()),
        ),
        (SURFACE_COLUMN.to_string(), Value::Text(row.surface.clone())),
        (
            SCOPE_LEVEL_COLUMN.to_string(),
            Value::Text(DEPLOYMENT_SCOPE_LEVEL.to_string()),
        ),
        (
            SCOPE_TARGET_COLUMN.to_string(),
            Value::Text(DEPLOYMENT_SCOPE_TARGET.to_string()),
        ),
        (
            LAST_WRITE_OUT_COLUMN.to_string(),
            Value::Text(row.last_write_out.clone()),
        ),
        (
            LAST_READ_BACK_COLUMN.to_string(),
            Value::Text(row.last_read_back.clone()),
        ),
    ]);
    store
        .sync_database()
        .execute(&statement, &params)
        .map_err(|error| map_engine_error(&error))?;
    Ok(())
}

/// The surface one stored token names, or nothing. The surface vocabulary is
/// read back in more than one place — stored records and the echo ledger — and
/// this is its one home, so no other module has to name the management-server
/// surface to recognize it.
pub fn surface_from_token(token: &str) -> Option<Surface> {
    [
        Surface::Automatic,
        Surface::ManagementServer,
        Surface::AddonOptions,
        Surface::ConfigFile,
        Surface::StartupOptions,
        Surface::VigilSettings,
    ]
    .into_iter()
    .find(|surface| surface.as_str() == token)
}

/// The settings this deployment always answers for, even before anything has
/// been written: the automatic floor is present from the first start.
fn declared_settings() -> Vec<&'static str> {
    use crate::settings_model::{
        DETECTOR_CLASSES_SETTING, DETECTOR_CONFIDENCE_THRESHOLD_SETTING,
        DETECTOR_QUEUE_CAPACITY_SETTING, DETECTOR_SAMPLE_FRAMES_SETTING,
        DETECTOR_STATIONARY_INTERVAL_SETTING, MOTION_SENSITIVITY_SETTING,
        RECOGNITION_COVERED_CLASSES_SETTING,
    };
    // Every setting this arc touches, so the operator surface answers for all
    // of them rather than for whichever ones happen to have a record. A
    // setting nobody has touched reads Automatic; a setting missing from the
    // listing reads as nothing at all.
    let mut settings = vec![
        crate::settings_backends::DETECTION_BACKEND_SETTING,
        crate::settings_backends::DECODE_BACKEND_SETTING,
        crate::settings_reflection::RESTART_ON_REFLECT_SETTING,
        DETECTOR_CLASSES_SETTING,
        DETECTOR_SAMPLE_FRAMES_SETTING,
        DETECTOR_STATIONARY_INTERVAL_SETTING,
        DETECTOR_CONFIDENCE_THRESHOLD_SETTING,
        DETECTOR_QUEUE_CAPACITY_SETTING,
        MOTION_SENSITIVITY_SETTING,
        RECOGNITION_COVERED_CLASSES_SETTING,
    ];
    // The behavior values that used to be environment variables. A value that
    // moved into the store and is not answered for here would be a setting an
    // operator can set and cannot see — which is the hidden knob the whole
    // model exists to refuse, one migration behind instead of one read behind.
    settings.extend_from_slice(&[
        crate::settings_model::SITE_NAME_SETTING,
        crate::settings_model::CAMERA_NAME_SETTING,
        crate::settings_model::HEALTH_PORT_SETTING,
        crate::settings_model::REVIEW_PORT_SETTING,
        crate::settings_model::DETECTOR_MODEL_ID_SETTING,
        crate::settings_model::DETECTOR_MODEL_PATH_SETTING,
        crate::settings_model::RECOGNITION_WEIGHTS_DIR_SETTING,
        crate::settings_model::RECOGNITION_SPACE_ID_SETTING,
        crate::settings_model::RECOGNITION_THRESHOLD_SETTING,
        crate::settings_model::FABRIC_HUB_SETTING,
        crate::settings_model::FABRIC_ALLOW_FRAME_OFFLOAD_SETTING,
        crate::settings_model::FABRIC_WORKER_LEASE_MS_SETTING,
        crate::settings_model::FABRIC_FALLBACK_HORIZON_MS_SETTING,
        crate::settings_model::RTSP_RETRY_INITIAL_MS_SETTING,
        crate::settings_model::RTSP_RETRY_MAX_MS_SETTING,
        crate::settings_model::DECODE_PROBE_DEADLINE_SECS_SETTING,
        crate::settings_model::HARDWARE_PROBE_DEADLINE_SECS_SETTING,
    ]);
    // The camera stream and its live-view twin are answered for like any other
    // setting an operator can set — a value they can choose and cannot see is
    // the hidden knob this whole model refuses. What is never shown is their
    // SPELLING: both carry the camera's credentials in their userinfo, so the
    // surface renders where the endpoint came from rather than what it says,
    // exactly as it does for a password.
    settings.extend_from_slice(&[
        crate::settings_model::RTSP_URL_SETTING,
        crate::settings_model::LIVE_RTSP_URL_SETTING,
    ]);
    //
    // The two path settings above them ARE listed even though they are absent
    // on an ordinary install: the projection renders absence in its own form,
    // distinct both from a value that IS empty and from a field with nothing to
    // report, so listing them tells the operator the truth rather than a blank
    // they have to interpret.
    for domain in crate::settings_domains::domain_roster() {
        settings.push(domain.switch);
    }
    settings
}

/// Whether the operator surface answers for `setting` on every deployment.
/// The declared roster has one home; asking it is how another module — the
/// per-setting application timing, say — stays in step with it rather than
/// keeping a list of its own that drifts.
pub fn is_declared_setting(setting: &str) -> bool {
    declared_settings().contains(&setting)
}

/// A resolution target standing for one record's own scope, for the paths that
/// hold a scope and need to ask the ordinary resolution a question about it.
fn target_for(scope: &Scope) -> ScopeTarget {
    ScopeTarget {
        tenant: scope.target.clone(),
        site: scope.target.clone(),
        node: scope.target.clone(),
        camera: Some(scope.target.clone()),
    }
}

/// The automatic value a setting falls back on when no record names it: Vigil's
/// own product-selected choice, with the derivation input as its reason. A
/// record with no reason is a defect, not a blank field.
fn automatic_floor(setting: &str, target: &ScopeTarget) -> Option<SettingRecord> {
    let scope = Scope::node(target.node.clone());
    let (value, reason) = crate::settings_backends::automatic_default(setting)?;
    Some(SettingRecord::automatic(setting, scope, value, reason))
}

/// The highest-ranked record among candidates: the author rank first, then the
/// most specific scope within that author, then — between two local surfaces,
/// which is one human changing their own mind — the most recent, with the
/// declared first-start order breaking a genuine tie.
fn highest_ranked(candidates: &[SettingRecord]) -> Option<SettingRecord> {
    candidates
        .iter()
        .max_by(|left, right| {
            left.author
                .cmp(&right.author)
                .then_with(|| {
                    left.scope
                        .level
                        .specificity()
                        .cmp(&right.scope.level.specificity())
                })
                .then_with(|| left.written_at_ms.cmp(&right.written_at_ms))
                .then_with(|| {
                    left.surface
                        .first_start_precedence()
                        .cmp(&right.surface.first_start_precedence())
                })
        })
        .cloned()
}

/// The sentence the operator surface renders for a record that is stored and
/// not effective. A shadowed or dormant value is the single most likely source
/// of confusion in this model, so it belongs on the face of the surface.
fn held_statement(
    record: &SettingRecord,
    reason: &HeldReason,
    effective: &SettingRecord,
) -> String {
    match reason {
        HeldReason::Shadowed {
            by_author,
            by_surface,
        } => format!(
            "your {} setting of {} is stored underneath; {} from the {} is what is running",
            record.surface.as_str(),
            record.value,
            effective.value,
            format_args!("{}/{}", by_author.as_str(), by_surface.as_str()),
        ),
        HeldReason::DormantUnderDomain { domain } => format!(
            "your setting of {} is held while {domain} is managing this",
            record.value
        ),
        // What this deployment HOLDS is the whole of what a store read can
        // answer. Whether the value is executing is a fact about a live
        // process, answered beside this line by the running field, and a
        // sentence built from records that claims it anyway is how an operator
        // came to read "burn-wgpu is running" next to a node running burn-cpu.
        HeldReason::OutrankedByLocalSurface { by_surface } => format!(
            "your {} says {}; you set {} later through the {}, and {} is the value this \
             deployment holds",
            record.surface.as_str(),
            record.value,
            effective.value,
            by_surface.as_str(),
            effective.value,
        ),
    }
}

/// The parameters one record write binds.
fn write_params(record: &SettingRecord, include_scope_label: bool) -> HashMap<String, Value> {
    let (value_type, value) = encode_value(&record.value);
    let mut params = HashMap::from([
        (
            SETTING_NAME_COLUMN.to_string(),
            Value::Text(record.setting.clone()),
        ),
        (
            "author".to_string(),
            Value::Text(record.author.as_str().to_string()),
        ),
        (
            SURFACE_COLUMN.to_string(),
            Value::Text(record.surface.as_str().to_string()),
        ),
        (
            SCOPE_LEVEL_COLUMN.to_string(),
            Value::Text(record.scope.level.as_str().to_string()),
        ),
        (
            SCOPE_TARGET_COLUMN.to_string(),
            Value::Text(record.scope.target.clone()),
        ),
        (
            "value_type".to_string(),
            Value::Text(value_type.to_string()),
        ),
        ("value".to_string(), Value::Text(value)),
        ("reason".to_string(), Value::Text(record.reason.clone())),
        (
            "written_at_ms".to_string(),
            Value::Int64(record.written_at_ms),
        ),
        (
            "domain_generation".to_string(),
            Value::Int64(i64::try_from(record.domain_generation).unwrap_or(i64::MAX)),
        ),
        (
            "reset_record".to_string(),
            Value::Int64(i64::from(record.reset)),
        ),
    ]);
    if include_scope_label {
        params.insert(
            SCOPE_LABEL_COLUMN.to_string(),
            Value::Text(PUSHED_WRITE_SCOPE_LABEL.to_string()),
        );
    }
    params
}

/// The list separator inside a stored list value: a unit separator, which no
/// class name, backend name or path carries.
const LIST_SEPARATOR: char = '\u{1f}';

fn encode_value(value: &SettingValue) -> (&'static str, String) {
    match value {
        SettingValue::Bool(inner) => ("bool", inner.to_string()),
        SettingValue::Int(inner) => ("int", inner.to_string()),
        SettingValue::Float(inner) => ("float", inner.to_string()),
        SettingValue::Text(inner) => ("text", inner.clone()),
        SettingValue::List(values) => (
            "list",
            values
                .iter()
                .map(String::as_str)
                .collect::<Vec<&str>>()
                .join(&LIST_SEPARATOR.to_string()),
        ),
    }
}

fn decode_value(value_type: &str, value: &str) -> Result<SettingValue, SettingsError> {
    match value_type {
        "bool" => Ok(SettingValue::Bool(value == "true")),
        "int" => value
            .parse()
            .map(SettingValue::Int)
            .map_err(|error| SettingsError::Store(format!("stored integer {value}: {error}"))),
        "float" => value
            .parse()
            .map(SettingValue::Float)
            .map_err(|error| SettingsError::Store(format!("stored number {value}: {error}"))),
        "text" => Ok(SettingValue::Text(value.to_string())),
        "list" => Ok(SettingValue::List(if value.is_empty() {
            Vec::new()
        } else {
            value
                .split(LIST_SEPARATOR)
                .map(str::to_string)
                .collect::<Vec<String>>()
        })),
        other => Err(SettingsError::Store(format!(
            "a stored record carries an unknown value type {other}"
        ))),
    }
}

fn text_at(row: &[Value], index: usize) -> Result<String, SettingsError> {
    match row.get(index) {
        Some(Value::Text(text)) => Ok(text.clone()),
        other => Err(SettingsError::Store(format!(
            "a stored record's column {index} is not text: {other:?}"
        ))),
    }
}

fn int_at(row: &[Value], index: usize) -> Result<i64, SettingsError> {
    match row.get(index) {
        Some(Value::Int64(value)) => Ok(*value),
        Some(Value::Timestamp(value)) => Ok(*value),
        other => Err(SettingsError::Store(format!(
            "a stored record's column {index} is not an integer: {other:?}"
        ))),
    }
}

fn record_from_row(row: &[Value]) -> Result<SettingRecord, SettingsError> {
    let author = match text_at(row, 1)?.as_str() {
        value if value == Author::Automatic.as_str() => Author::Automatic,
        value if value == Author::Pushed.as_str() => Author::Pushed,
        value if value == Author::LocalExplicit.as_str() => Author::LocalExplicit,
        other => {
            return Err(SettingsError::Store(format!(
                "a stored record names an unknown author {other}"
            )));
        }
    };
    let surface = surface_from_token(&text_at(row, 2)?).ok_or_else(|| {
        SettingsError::Store("a stored record names an unknown surface".to_string())
    })?;
    let level = [
        ScopeLevel::Tenant,
        ScopeLevel::Site,
        ScopeLevel::Node,
        ScopeLevel::Camera,
    ]
    .into_iter()
    .find(|level| level.as_str() == text_at(row, 3).unwrap_or_default())
    .ok_or_else(|| {
        SettingsError::Store("a stored record names an unknown scope level".to_string())
    })?;

    Ok(SettingRecord {
        setting: text_at(row, 0)?,
        author,
        surface,
        scope: Scope {
            level,
            target: text_at(row, 4)?,
        },
        value: decode_value(&text_at(row, 5)?, &text_at(row, 6)?)?,
        reason: text_at(row, 7)?,
        written_at_ms: int_at(row, 8)?,
        domain_generation: u64::try_from(int_at(row, 9)?).unwrap_or(0),
        reset: int_at(row, 10)? != 0,
    })
}

/// The engine's own refusals, carried to the caller with their meaning intact:
/// a scope-label violation stays a scope-label violation rather than becoming
/// a generic store error, because that is the boundary a node is being told it
/// crossed.
fn map_engine_error(error: &contextdb_core::Error) -> SettingsError {
    match error {
        contextdb_core::Error::ScopeLabelViolation { requested, allowed } => {
            let allowed: Vec<String> = allowed.iter().map(|label| label.0.clone()).collect();
            SettingsError::ScopeLabelViolation {
                requested: requested.0.clone(),
                // The engine's own report, passed through faithfully — including
                // an empty allowed set. Vigil does not know better than the
                // engine which labels it would have accepted, so it must not
                // invent NODE_SCOPE_LABEL as a stand-in for a fact the engine
                // did not report.
                allowed: allowed.join(", "),
            }
        }
        other => SettingsError::Store(other.to_string()),
    }
}

/// A store that cannot be opened is classified, not flattened: another runtime
/// holding this data directory is a different answer from a store that will
/// not open at all.
fn map_open_error(error: CgError) -> SettingsError {
    match error {
        CgError::StoreLocked { holder_pid, .. } => {
            SettingsError::LockedByAnotherRuntime { holder_pid }
        }
        other => SettingsError::Store(other.to_string()),
    }
}
