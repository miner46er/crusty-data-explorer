//! Database engines, value types, and the worker-thread protocol.
//!
//! Everything that touches rusqlite (or sqlx, behind the `postgres`
//! feature) lives in this module. Layers above only see [`Value`],
//! [`QueryResult`], [`DbError`], and the [`DbCommand`]/[`DbEvent`]
//! protocol. No client-library type crosses the boundary.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Cap on materialized rows per statement. The result reports truncation.
pub const ROW_CAP: usize = 10_000;
/// Cap on displayed characters per text cell in the grid.
pub const CELL_CAP: usize = 256;

/// Branded id so a tab id cannot stand in for a connection id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ConnectionId(u64);
/// Stable tab identity, never a vector index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(u64);
/// Caller-allocated request id, echoed back on completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(u64);

impl ConnectionId {
    pub fn raw(self) -> u64 {
        self.0
    }
    pub fn from_raw(id: u64) -> Self {
        Self(id)
    }
}

/// The single minter of every id in the app. One counter, three brands.
#[derive(Debug)]
pub struct Ids {
    next: u64,
}

impl Ids {
    pub fn new() -> Self {
        Self { next: 1 }
    }
    /// Called once at startup so loaded profile ids stay unique.
    pub fn resume_after(&mut self, existing: &[ConnectionId]) {
        if let Some(max) = existing.iter().map(|c| c.0).max() {
            self.next = self.next.max(max + 1);
        }
    }
    fn take(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }
    pub fn connection(&mut self) -> ConnectionId {
        ConnectionId(self.take())
    }
    pub fn tab(&mut self) -> TabId {
        TabId(self.take())
    }
    pub fn run(&mut self) -> RunId {
        RunId(self.take())
    }
    pub fn saved(&mut self) -> u64 {
        self.take()
    }
    /// Keep the single counter past loaded saved-query ids.
    pub fn resume_after_saved(&mut self, existing: &[u64]) {
        if let Some(max) = existing.iter().copied().max() {
            self.next = self.next.max(max + 1);
        }
    }
}

/// The kind the user picks in the New Connection form. A chooser, not
/// stored state: [`ConnectionConfig`] carries the kind, `kind()` derives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DbKind {
    Sqlite,
    Postgres,
}

impl DbKind {
    pub const ALL: [DbKind; 2] = [DbKind::Sqlite, DbKind::Postgres];
    pub fn label(self) -> &'static str {
        match self {
            DbKind::Sqlite => "SQLite",
            DbKind::Postgres => "Postgres",
        }
    }
    pub fn default_port(self) -> Option<u16> {
        match self {
            DbKind::Sqlite => None,
            DbKind::Postgres => Some(5432),
        }
    }
    pub fn default_config(self) -> ConnectionConfig {
        match self {
            DbKind::Sqlite => ConnectionConfig::Sqlite {
                path: PathBuf::new(),
            },
            DbKind::Postgres => ConnectionConfig::Postgres {
                host: "localhost".to_owned(),
                port: 5432,
                database: String::new(),
                user: String::new(),
            },
        }
    }
}

/// The single list of bundled engines. The connection form iterates
/// `all()`, so a new backend appears in the UI with no UI change.
#[derive(Debug, Clone, Copy)]
pub struct DriverRegistry {
    kinds: &'static [DbKind],
}

impl DriverRegistry {
    pub fn bundled() -> Self {
        Self { kinds: &DbKind::ALL }
    }
    pub fn all(&self) -> impl Iterator<Item = DbKind> {
        self.kinds.iter().copied()
    }
}

/// Persisted settings, one variant per engine. The variant IS the kind.
/// Serialized shape (the `kind` tag) is the on-disk contract.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConnectionConfig {
    Sqlite { path: PathBuf },
    Postgres { host: String, port: u16, database: String, user: String },
}

impl ConnectionConfig {
    pub fn kind(&self) -> DbKind {
        match self {
            ConnectionConfig::Sqlite { .. } => DbKind::Sqlite,
            ConnectionConfig::Postgres { .. } => DbKind::Postgres,
        }
    }
    /// Boundary check before a session opens. Shared by the form and the worker.
    pub fn validate(&self) -> Result<(), DbError> {
        match self {
            ConnectionConfig::Sqlite { path } => {
                if path.as_os_str().is_empty() {
                    return Err(DbError::Config("SQLite path is empty".to_owned()));
                }
                Ok(())
            }
            ConnectionConfig::Postgres { host, port, database, user } => {
                if host.trim().is_empty() {
                    return Err(DbError::Config("host is empty".to_owned()));
                }
                if *port == 0 {
                    return Err(DbError::Config("port is not set".to_owned()));
                }
                if database.trim().is_empty() {
                    return Err(DbError::Config("database is empty".to_owned()));
                }
                if user.trim().is_empty() {
                    return Err(DbError::Config("user is empty".to_owned()));
                }
                Ok(())
            }
        }
    }
}

/// One cell. Every backend maps into these five shapes at the boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    /// Single source of truth for how a DB value prints. `cap` bounds text
    /// length; blobs print as a size tag with no payload.
    pub fn display(&self, cap: usize) -> Cow<'_, str> {
        match self {
            Value::Null => Cow::Borrowed("NULL"),
            Value::Integer(n) => Cow::Owned(n.to_string()),
            Value::Real(f) => Cow::Owned(f.to_string()),
            Value::Text(s) => {
                if s.chars().count() > cap {
                    Cow::Owned(s.chars().take(cap).collect::<String>() + "…")
                } else {
                    Cow::Borrowed(s)
                }
            }
            Value::Blob(b) => Cow::Owned(format!("[BLOB {}]", format_bytes(b.len()))),
        }
    }
}

fn format_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else {
        format!("{:.1} KB", n as f64 / 1024.0)
    }
}

/// One result column. Fields are private; construction checks happen in
/// [`QueryResult::from_columns`], so ragged results cannot exist.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    name: String,
    type_name: String,
    values: Vec<Value>,
}

impl Column {
    pub fn new(name: String, type_name: String, values: Vec<Value>) -> Self {
        Self { name, type_name, values }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn type_name(&self) -> &str {
        &self.type_name
    }
    pub fn values(&self) -> &[Value] {
        &self.values
    }
}

/// A materialized result set in column-major order. `affected` is Some only
/// for statements without result columns. `truncated` says the row cap hit.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    columns: Vec<Column>,
    row_count: usize,
    truncated: bool,
    affected: Option<u64>,
    took: Duration,
}

impl QueryResult {
    /// The only checked constructor. Rejects ragged columns.
    pub fn from_columns(
        columns: Vec<Column>,
        truncated: bool,
        affected: Option<u64>,
        took: Duration,
    ) -> Result<Self, ShapeError> {
        let row_count = columns.first().map(|c| c.values.len()).unwrap_or(0);
        if columns.iter().any(|c| c.values.len() != row_count) {
            return Err(ShapeError::Ragged { expected: row_count });
        }
        Ok(Self { columns, row_count, truncated, affected, took })
    }
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }
    pub fn row_count(&self) -> usize {
        self.row_count
    }
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }
    pub fn affected(&self) -> Option<u64> {
        self.affected
    }
    pub fn took(&self) -> Duration {
        self.took
    }
    /// Bounds-checked: the grid's visible range comes from the previous frame
    /// and the result can be replaced between frames.
    pub fn cell(&self, row: usize, col: usize) -> Option<&Value> {
        self.columns.get(col).and_then(|c| c.values.get(row))
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ShapeError {
    #[error("ragged result: expected {expected} values per column")]
    Ragged { expected: usize },
}

/// Cap on tables materialized per schema load. The schema reports truncation.
pub const SCHEMA_TABLE_CAP: usize = 500;
/// Cap on columns materialized per table. The schema reports truncation.
pub const SCHEMA_COLUMN_CAP: usize = 200;
/// Cap on foreign keys materialized per table. Each table reports truncation.
pub const SCHEMA_FK_CAP: usize = 50;
/// Cap on indexes materialized per table. Each table reports truncation.
pub const SCHEMA_INDEX_CAP: usize = 50;
/// Cap on schemas materialized per database.
pub const SCHEMA_SCHEMA_CAP: usize = 64;
/// Cap on sequences materialized per schema.
pub const SCHEMA_SEQUENCE_CAP: usize = 200;
/// Cap on aggregated schema indexes materialized per schema.
pub const SCHEMA_SCHEMA_INDEX_CAP: usize = 200;

/// Table or view. The variant is the kind; no stringly matching above this file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Table,
    View,
}

/// One column in a schema tree. Fields are private; construction checks happen
/// in [`ColumnInfo::new`], so empty names cannot exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    name: String,
    type_name: String,
    nullable: bool,
    pk_position: Option<u32>,
}

impl ColumnInfo {
    /// Checked constructor. Rejects empty names.
    pub fn new(
        name: String,
        type_name: String,
        nullable: bool,
        pk_position: Option<u32>,
    ) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("column"));
        }
        Ok(Self { name, type_name, nullable, pk_position })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn type_name(&self) -> &str {
        &self.type_name
    }
    pub fn nullable(&self) -> bool {
        self.nullable
    }
    pub fn pk_position(&self) -> Option<u32> {
        self.pk_position
    }
    pub fn is_pk(&self) -> bool {
        self.pk_position.is_some()
    }
}

/// One foreign key from a table to a referenced table. Fields are private;
/// construction checks happen in [`ForeignKeyInfo::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyInfo {
    columns: Vec<String>,
    ref_table: String,
    ref_columns: Vec<String>,
    constraint_name: Option<String>,
}

impl ForeignKeyInfo {
    /// Checked constructor. Rejects blank table and column names, and a
    /// blank constraint name when one is present.
    pub fn new(
        columns: Vec<String>,
        ref_table: String,
        ref_columns: Vec<String>,
        constraint_name: Option<String>,
    ) -> Result<Self, SchemaError> {
        if ref_table.trim().is_empty() {
            return Err(SchemaError::EmptyName("referenced table"));
        }
        if columns.iter().any(|c| c.trim().is_empty()) {
            return Err(SchemaError::EmptyName("foreign key column"));
        }
        if ref_columns.iter().any(|c| c.trim().is_empty()) {
            return Err(SchemaError::EmptyName("referenced column"));
        }
        if constraint_name.as_ref().is_some_and(|n| n.trim().is_empty()) {
            return Err(SchemaError::EmptyName("foreign key constraint"));
        }
        Ok(Self { columns, ref_table, ref_columns, constraint_name })
    }
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn ref_table(&self) -> &str {
        &self.ref_table
    }
    pub fn ref_columns(&self) -> &[String] {
        &self.ref_columns
    }
    pub fn constraint_name(&self) -> Option<&str> {
        self.constraint_name.as_deref()
    }
}

/// One index on a table. Fields are private; construction checks happen in
/// [`IndexInfo::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    name: String,
    columns: Vec<String>,
    unique: bool,
}

impl IndexInfo {
    /// Checked constructor. Rejects blank index and column names.
    pub fn new(
        name: String,
        columns: Vec<String>,
        unique: bool,
    ) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("index"));
        }
        if columns.iter().any(|c| c.trim().is_empty()) {
            return Err(SchemaError::EmptyName("index column"));
        }
        Ok(Self { name, columns, unique })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn unique(&self) -> bool {
        self.unique
    }
    pub fn is_unique(&self) -> bool {
        self.unique
    }
}

/// One table constraint. Fields are private; construction checks happen in
/// [`ConstraintInfo::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    Pk,
    Unique,
    Check,
    Foreign,
}

impl ConstraintKind {
    pub fn label(self) -> &'static str {
        match self {
            ConstraintKind::Pk => "PRIMARY KEY",
            ConstraintKind::Unique => "UNIQUE",
            ConstraintKind::Check => "CHECK",
            ConstraintKind::Foreign => "FOREIGN KEY",
        }
    }
}

/// One named constraint on a table. `ref_table` is Some only for foreign
/// keys. `check_expr` is Some only for check constraints and holds the
/// `pg_get_constraintdef` output on Postgres.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintInfo {
    name: String,
    kind: ConstraintKind,
    columns: Vec<String>,
    ref_table: Option<String>,
    ref_columns: Vec<String>,
    check_expr: Option<String>,
}

impl ConstraintInfo {
    /// Checked constructor. Rejects blank names and blank nested names.
    pub fn new(
        name: String,
        kind: ConstraintKind,
        columns: Vec<String>,
        ref_table: Option<String>,
        ref_columns: Vec<String>,
        check_expr: Option<String>,
    ) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("constraint"));
        }
        if columns.iter().any(|c| c.trim().is_empty()) {
            return Err(SchemaError::EmptyName("constraint column"));
        }
        if ref_table.as_ref().is_some_and(|n| n.trim().is_empty()) {
            return Err(SchemaError::EmptyName("referenced table"));
        }
        if ref_columns.iter().any(|c| c.trim().is_empty()) {
            return Err(SchemaError::EmptyName("referenced column"));
        }
        if check_expr.as_ref().is_some_and(|e| e.trim().is_empty()) {
            return Err(SchemaError::EmptyName("check expression"));
        }
        Ok(Self { name, kind, columns, ref_table, ref_columns, check_expr })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn kind(&self) -> ConstraintKind {
        self.kind
    }
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn ref_table(&self) -> Option<&str> {
        self.ref_table.as_deref()
    }
    pub fn ref_columns(&self) -> &[String] {
        &self.ref_columns
    }
    pub fn check_expr(&self) -> Option<&str> {
        self.check_expr.as_deref()
    }
}

/// One sequence in a schema. Name only for phase one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceInfo {
    name: String,
}

impl SequenceInfo {
    /// Checked constructor. Rejects empty names.
    pub fn new(name: String) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("sequence"));
        }
        Ok(Self { name })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// One index in the per-schema aggregated folder. Points back to its table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRef {
    table: String,
    index: String,
}

impl IndexRef {
    /// Checked constructor. Rejects blank table and index names.
    pub fn new(table: String, index: String) -> Result<Self, SchemaError> {
        if table.trim().is_empty() {
            return Err(SchemaError::EmptyName("table"));
        }
        if index.trim().is_empty() {
            return Err(SchemaError::EmptyName("index"));
        }
        Ok(Self { table, index })
    }
    pub fn table(&self) -> &str {
        &self.table
    }
    pub fn index(&self) -> &str {
        &self.index
    }
}

/// One table or view in a schema tree. Fields are private; construction checks
/// happen in [`TableInfo::new`]. `referenced_by` holds incoming foreign keys:
/// each entry copies the child key with `columns` set to the parent columns
/// and `ref_table` set to the child table, so the UI renders
/// `child(child_cols) to parent(parent_cols)` with one clickable name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    name: String,
    kind: TableKind,
    columns: Vec<ColumnInfo>,
    constraints: Vec<ConstraintInfo>,
    foreign_keys: Vec<ForeignKeyInfo>,
    referenced_by: Vec<ForeignKeyInfo>,
    indexes: Vec<IndexInfo>,
    foreign_keys_truncated: bool,
    indexes_truncated: bool,
}

impl TableInfo {
    /// Checked constructor. Rejects empty names.
    pub fn new(
        name: String,
        kind: TableKind,
        columns: Vec<ColumnInfo>,
        constraints: Vec<ConstraintInfo>,
        foreign_keys: Vec<ForeignKeyInfo>,
        referenced_by: Vec<ForeignKeyInfo>,
        indexes: Vec<IndexInfo>,
        foreign_keys_truncated: bool,
        indexes_truncated: bool,
    ) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("table"));
        }
        Ok(Self {
            name,
            kind,
            columns,
            constraints,
            foreign_keys,
            referenced_by,
            indexes,
            foreign_keys_truncated,
            indexes_truncated,
        })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn kind(&self) -> TableKind {
        self.kind
    }
    pub fn columns(&self) -> &[ColumnInfo] {
        &self.columns
    }
    pub fn constraints(&self) -> &[ConstraintInfo] {
        &self.constraints
    }
    pub fn foreign_keys(&self) -> &[ForeignKeyInfo] {
        &self.foreign_keys
    }
    pub fn referenced_by(&self) -> &[ForeignKeyInfo] {
        &self.referenced_by
    }
    pub fn indexes(&self) -> &[IndexInfo] {
        &self.indexes
    }
    pub fn foreign_keys_truncated(&self) -> bool {
        self.foreign_keys_truncated
    }
    pub fn indexes_truncated(&self) -> bool {
        self.indexes_truncated
    }
}

/// One schema in a database: split tables and views plus sequences and the
/// aggregated per-schema index folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaNode {
    name: String,
    tables: Vec<TableInfo>,
    views: Vec<TableInfo>,
    sequences: Vec<SequenceInfo>,
    schema_indexes: Vec<IndexRef>,
}

impl SchemaNode {
    pub fn new(
        name: String,
        tables: Vec<TableInfo>,
        views: Vec<TableInfo>,
        sequences: Vec<SequenceInfo>,
        schema_indexes: Vec<IndexRef>,
    ) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("schema"));
        }
        Ok(Self { name, tables, views, sequences, schema_indexes })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn tables(&self) -> &[TableInfo] {
        &self.tables
    }
    pub fn views(&self) -> &[TableInfo] {
        &self.views
    }
    pub fn sequences(&self) -> &[SequenceInfo] {
        &self.sequences
    }
    pub fn schema_indexes(&self) -> &[IndexRef] {
        &self.schema_indexes
    }
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
            && self.views.is_empty()
            && self.sequences.is_empty()
            && self.schema_indexes.is_empty()
    }
}

/// One database in the tree: a name plus its schemas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseNode {
    name: String,
    schemas: Vec<SchemaNode>,
}

impl DatabaseNode {
    pub fn new(name: String, schemas: Vec<SchemaNode>) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("database"));
        }
        Ok(Self { name, schemas })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn schemas(&self) -> &[SchemaNode] {
        &self.schemas
    }
}

/// A browsable database tree: ordered databases with truncation flags.
/// Fields are private; drivers build it, the UI reads it. Memory only, so
/// reshaping needs no migration and the persist shape is untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaInfo {
    databases: Vec<DatabaseNode>,
    tables_truncated: bool,
    columns_truncated: bool,
}

impl SchemaInfo {
    pub fn new(
        databases: Vec<DatabaseNode>,
        tables_truncated: bool,
        columns_truncated: bool,
    ) -> Self {
        Self { databases, tables_truncated, columns_truncated }
    }
    pub fn databases(&self) -> &[DatabaseNode] {
        &self.databases
    }
    pub fn tables_truncated(&self) -> bool {
        self.tables_truncated
    }
    pub fn columns_truncated(&self) -> bool {
        self.columns_truncated
    }
    pub fn is_empty(&self) -> bool {
        self.databases.iter().all(|d| d.schemas().iter().all(|s| s.is_empty()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    #[error("{0} name is empty")]
    EmptyName(&'static str),
}

/// Row-wise constructor used by drivers. Buffers rows, transposes at finish.
pub struct ResultBuilder {
    columns: Vec<(String, String)>,
    rows: Vec<Vec<Value>>,
    truncated: bool,
    max_rows: usize,
}

impl ResultBuilder {
    pub fn new(columns: Vec<(String, String)>, max_rows: usize) -> Self {
        Self { columns, rows: Vec::new(), truncated: false, max_rows }
    }
    /// Append one row. Returns false when the row limit is reached; the
    /// driver then stops pulling rows and `finish` reports truncation.
    /// Panics on a width mismatch, which is a driver bug, not user input.
    pub fn push_row(&mut self, cells: Vec<Value>) -> bool {
        assert_eq!(cells.len(), self.columns.len(), "driver pushed a ragged row");
        if self.rows.len() >= self.max_rows {
            self.truncated = true;
            return false;
        }
        self.rows.push(cells);
        true
    }
    pub fn finish(self, took: Duration) -> QueryResult {
        let width = self.columns.len();
        let mut cols: Vec<Column> = self
            .columns
            .into_iter()
            .enumerate()
            .map(|(i, (name, type_name))| {
                let values = self.rows.iter().map(|r| r[i].clone()).collect();
                Column::new(name, type_name, values)
            })
            .collect();
        let _ = width;
        let row_count = self.rows.len();
        let truncated = self.truncated;
        debug_assert!(cols.iter().all(|c| c.values.len() == row_count));
        let _ = &mut cols;
        QueryResult { columns: cols, row_count, truncated, affected: None, took }
    }
}

/// Every failure the db layer reports, in domain terms. Client library
/// error types never escape their driver code.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DbError {
    #[error("could not open: {0}")]
    Open(String),
    #[error("query failed: {0}")]
    Query(String),
    #[error("connection is gone")]
    Gone,
    #[error("invalid settings: {0}")]
    Config(String),
    #[error("not supported: {0}")]
    NotSupported(String),
}

impl DbError {
    /// Single source for the fold's choice between "show this in the tab"
    /// and "move the connection to Error".
    pub fn is_fatal(&self) -> bool {
        !matches!(self, DbError::Query(_))
    }
}

/// Fires the engine's interrupt for the statement in flight. Safe from any
/// thread, safe while idle, safe to call twice. This is the cancel upgrade
/// path: the UI fires it, the worker never reads a cancel command.
#[derive(Clone)]
pub struct CancelHandle(Arc<dyn Fn() + Send + Sync + 'static>);

impl CancelHandle {
    pub fn new(fire: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(fire))
    }
    pub fn cancel(&self) {
        (self.0)();
    }
}

impl std::fmt::Debug for CancelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CancelHandle")
    }
}

/// The only way to talk to a worker. Holding one means the worker was
/// spawned; `send` fails only after the worker thread exited.
#[derive(Debug, Clone)]
pub struct DbHandle {
    cmds: mpsc::Sender<DbCommand>,
}

impl DbHandle {
    pub fn send(&self, cmd: DbCommand) -> Result<(), DbError> {
        self.cmds.send(cmd).map_err(|_| DbError::Gone)
    }
}

#[derive(Debug)]
pub enum DbCommand {
    Query { run: RunId, sql: String },
    LoadSchema,
}

#[derive(Debug)]
pub enum DbEvent {
    Connected { conn: ConnectionId, cancel: CancelHandle, server_version: String },
    ConnectFailed { conn: ConnectionId, error: DbError },
    QueryOk { run: RunId, result: QueryResult },
    QueryErr { run: RunId, error: DbError },
    SchemaOk { conn: ConnectionId, schema: SchemaInfo },
    SchemaErr { conn: ConnectionId, error: DbError },
}

/// Owned by one worker thread for its whole life. Not Sync, and that is the
/// point: no locks, ordered execution per connection.
pub enum Session {
    Sqlite(rusqlite::Connection),
    #[cfg(feature = "postgres")]
    Postgres(PgSession),
}

impl Session {
    pub fn open(config: &ConnectionConfig, password: Option<&str>) -> Result<Session, DbError> {
        match config {
            ConnectionConfig::Sqlite { path } => rusqlite::Connection::open(path)
                .map(Session::Sqlite)
                .map_err(|e| DbError::Open(e.to_string())),
            ConnectionConfig::Postgres { .. } => Self::open_postgres(config, password),
        }
    }

    fn open_postgres(
        config: &ConnectionConfig,
        password: Option<&str>,
    ) -> Result<Session, DbError> {
        #[cfg(feature = "postgres")]
        {
            return Ok(Session::Postgres(PgSession::connect(config, password)?));
        }
        #[cfg(not(feature = "postgres"))]
        {
            let _ = (config, password);
            return Err(DbError::NotSupported(
                "postgres needs the `postgres` feature".to_owned(),
            ));
        }
    }

    pub fn server_version(&self) -> String {
        match self {
            Session::Sqlite(_) => format!("SQLite {}", rusqlite::version()),
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => pg.server_version.clone(),
        }
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        match self {
            Session::Sqlite(conn) => {
                let handle = conn.get_interrupt_handle();
                CancelHandle::new(move || handle.interrupt())
            }
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => {
                pg_cancel_handle(pg.connection_string.clone(), pg.backend_pid)
            }
        }
    }

    /// One statement per call. Result columns become capped rows; no
    /// columns means an affected count via `changes()`.
    pub fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        match self {
            Session::Sqlite(conn) => execute_sqlite(conn, sql, ROW_CAP),
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => pg.query(sql),
        }
    }

    /// Ordered schema load, capped with truncation flags. No client type
    /// crosses the boundary.
    pub fn schema(&self) -> Result<SchemaInfo, DbError> {
        match self {
            Session::Sqlite(conn) => sqlite_schema(conn),
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => pg.schema(),
        }
    }
}

/// Behind the `postgres` feature. Owns a current-thread tokio runtime so the
/// async sqlx model stays caged inside this driver file; the public trait
/// shape never changes. The worker thread calls `block_on`, so the UI thread
/// never blocks.
#[cfg(feature = "postgres")]
pub struct PgSession {
    rt: tokio::runtime::Runtime,
    pool: sqlx::PgPool,
    connection_string: String,
    database: String,
    server_version: String,
    backend_pid: i32,
}

#[cfg(feature = "postgres")]
impl PgSession {
    fn connect(config: &ConnectionConfig, password: Option<&str>) -> Result<Self, DbError> {
        let ConnectionConfig::Postgres {
            host,
            port,
            database,
            user,
        } = config
        else {
            return Err(DbError::Config("not a postgres connection".to_owned()));
        };
        let connection_string = postgres_url(host, *port, database, user, password);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| DbError::Open(e.to_string()))?;
        // `acquire_timeout` also bounds establishing the first connection;
        // sqlx 0.8 has no separate `connect_timeout` on the pool options.
        let pool = rt
            .block_on(
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(Duration::from_secs(10))
                    .connect(&connection_string),
            )
            .map_err(|e| DbError::Open(e.to_string()))?;
        let server_version = rt
            .block_on(async {
                let row = sqlx::query("SELECT version()")
                    .fetch_one(&pool)
                    .await
                    .map_err(|e| e.to_string())?;
                use sqlx::Row as _;
                row.try_get::<String, _>(0).map_err(|e| e.to_string())
            })
            .unwrap_or_else(|_| "Postgres".to_owned());
        let backend_pid = rt
            .block_on(async {
                let row = sqlx::query("SELECT pg_backend_pid()")
                    .fetch_one(&pool)
                    .await
                    .map_err(|e| e.to_string())?;
                use sqlx::Row as _;
                row.try_get::<i32, _>(0).map_err(|e| e.to_string())
            })
            .unwrap_or(-1);
        Ok(Self {
            rt,
            pool,
            connection_string,
            database: database.clone(),
            server_version,
            backend_pid,
        })
    }

    /// One statement per call. `describe` reports columns without executing,
    /// so a data-changing statement runs exactly once on the `execute` path.
    fn query(&self, sql: &str) -> Result<QueryResult, DbError> {
        let started = Instant::now();
        let columns = self.describe_columns(sql)?;
        if columns.is_empty() {
            let done = self
                .rt
                .block_on(sqlx::query(sql).execute(&self.pool))
                .map_err(|e| DbError::Query(e.to_string()))?;
            return QueryResult::from_columns(
                Vec::new(),
                false,
                Some(done.rows_affected()),
                started.elapsed(),
            )
            .map_err(|e| DbError::Query(e.to_string()));
        }
        let rows = self
            .rt
            .block_on(sqlx::query(sql).fetch_all(&self.pool))
            .map_err(|e| DbError::Query(e.to_string()))?;
        let mut builder = ResultBuilder::new(columns, ROW_CAP);
        for row in &rows {
            if !builder.push_row(map_pg_row(row)?) {
                break;
            }
        }
        Ok(builder.finish(started.elapsed()))
    }

    fn describe_columns(&self, sql: &str) -> Result<Vec<(String, String)>, DbError> {
        use sqlx::{Column as _, Executor as _, TypeInfo as _};
        let described = self
            .rt
            .block_on(async { (&self.pool).describe(sql).await })
            .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(described
            .columns()
            .iter()
            .map(|c| (c.name().to_owned(), c.type_info().name().to_owned()))
            .collect())
    }

    fn schema(&self) -> Result<SchemaInfo, DbError> {
        use sqlx::Row as _;
        let schema_names: Vec<String> = self
            .rt
            .block_on(
                sqlx::query(
                    "SELECT nspname FROM pg_namespace \
                     WHERE nspname NOT LIKE 'pg_%' \
                     AND nspname <> 'information_schema' \
                     ORDER BY nspname",
                )
                .fetch_all(&self.pool),
            )
            .map_err(|e| DbError::Query(e.to_string()))?
            .iter()
            .map(|row| {
                row.try_get::<String, _>(0).map_err(|e| DbError::Query(e.to_string()))
            })
            .collect::<Result<Vec<_>, DbError>>()?;
        let schema_names: Vec<String> =
            schema_names.into_iter().take(SCHEMA_SCHEMA_CAP).collect();
        let mut tables_truncated = false;
        let mut columns_truncated = false;
        struct RawSchema {
            name: String,
            order: Vec<(String, TableKind)>,
            columns_by_table: std::collections::HashMap<String, Vec<(String, String, bool)>>,
            pk_by_table: std::collections::HashMap<String, Vec<String>>,
            constraints_by_table: std::collections::HashMap<String, Vec<ConstraintInfo>>,
            fk_by_table: std::collections::HashMap<String, Vec<ForeignKeyInfo>>,
            fk_truncated_by_table: std::collections::HashMap<String, bool>,
            idx_by_table: std::collections::HashMap<String, Vec<IndexInfo>>,
            idx_truncated_by_table: std::collections::HashMap<String, bool>,
            sequences: Vec<SequenceInfo>,
            schema_indexes: Vec<IndexRef>,
        }
        struct FkEdge {
            child_table: String,
            parent_schema: String,
            parent_table: String,
            child_cols: Vec<String>,
            parent_cols: Vec<String>,
            name: String,
        }
        let mut raw_schemas: Vec<RawSchema> = Vec::new();
        let mut all_edges: Vec<FkEdge> = Vec::new();
        for schema_name in &schema_names {
            let tables: Vec<(String, String)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT table_name, table_type FROM information_schema.tables \
                         WHERE table_schema = $1 \
                         AND table_type IN ('BASE TABLE', 'VIEW') \
                         ORDER BY table_name",
                    )
                    .bind(schema_name)
                    .fetch_all(&self.pool),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    Ok((
                        row.try_get::<String, _>(0)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(1)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            if tables.len() > SCHEMA_TABLE_CAP {
                tables_truncated = true;
            }
            let order: Vec<(String, TableKind)> = tables
                .into_iter()
                .take(SCHEMA_TABLE_CAP)
                .map(|(name, kind)| {
                    let kind =
                        if kind == "VIEW" { TableKind::View } else { TableKind::Table };
                    (name, kind)
                })
                .collect();
            let columns: Vec<(String, String, String, String)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT table_name, column_name, data_type, is_nullable \
                         FROM information_schema.columns \
                         WHERE table_schema = $1 \
                         ORDER BY table_name, ordinal_position",
                    )
                    .bind(schema_name)
                    .fetch_all(&self.pool),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    Ok((
                        row.try_get::<String, _>(0)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(1)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(2)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(3)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let mut columns_by_table: std::collections::HashMap<
                String,
                Vec<(String, String, bool)>,
            > = std::collections::HashMap::new();
            for (table, column, data_type, is_nullable) in columns {
                columns_by_table.entry(table).or_default().push((
                    column,
                    data_type,
                    is_nullable == "YES",
                ));
            }
            let con_rows: Vec<(
                String,
                String,
                String,
                Option<Vec<String>>,
                Option<String>,
                Option<String>,
                Option<Vec<String>>,
                String,
            )> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT c.conname, c.contype::text, t.relname, \
                         (SELECT array_agg(a.attname ORDER BY u.ord) \
                          FROM unnest(c.conkey) WITH ORDINALITY AS u(attnum, ord) \
                          JOIN pg_attribute a \
                          ON a.attrelid = c.conrelid AND a.attnum = u.attnum), \
                         ft.relname, fns.nspname, \
                         (SELECT array_agg(a.attname ORDER BY u.ord) \
                          FROM unnest(c.confkey) WITH ORDINALITY AS u(attnum, ord) \
                          JOIN pg_attribute a \
                          ON a.attrelid = c.confrelid AND a.attnum = u.attnum), \
                         pg_get_constraintdef(c.oid) \
                         FROM pg_constraint c \
                         JOIN pg_class t ON t.oid = c.conrelid \
                         JOIN pg_namespace n ON n.oid = t.relnamespace \
                         LEFT JOIN pg_class ft ON ft.oid = c.confrelid \
                         LEFT JOIN pg_namespace fns ON fns.oid = ft.relnamespace \
                         WHERE n.nspname = $1 AND c.contype IN ('p', 'u', 'c', 'f') \
                         ORDER BY t.relname, c.conname",
                    )
                    .bind(schema_name)
                    .fetch_all(&self.pool),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    Ok((
                        row.try_get::<String, _>(0)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(1)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(2)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<Option<Vec<String>>, _>(3)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<Option<String>, _>(4)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<Option<String>, _>(5)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<Option<Vec<String>>, _>(6)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(7)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let mut pk_by_table: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            let mut constraints_by_table: std::collections::HashMap<
                String,
                Vec<ConstraintInfo>,
            > = std::collections::HashMap::new();
            let mut fk_grouped: std::collections::HashMap<
                String,
                Vec<(String, Vec<String>, String, Vec<String>)>,
            > = std::collections::HashMap::new();
            for (conname, contype, table, cols, ref_table, ref_schema, ref_cols, def) in
                &con_rows
            {
                let kind = match contype.as_str() {
                    "p" => ConstraintKind::Pk,
                    "u" => ConstraintKind::Unique,
                    "c" => ConstraintKind::Check,
                    "f" => ConstraintKind::Foreign,
                    _ => continue,
                };
                let cols = cols.clone().unwrap_or_default();
                if kind == ConstraintKind::Pk && !pk_by_table.contains_key(table) {
                    pk_by_table.insert(table.clone(), cols.clone());
                }
                let check_expr =
                    if kind == ConstraintKind::Check { Some(def.clone()) } else { None };
                let info = ConstraintInfo::new(
                    conname.clone(),
                    kind,
                    cols.clone(),
                    ref_table.clone(),
                    ref_cols.clone().unwrap_or_default(),
                    check_expr,
                )
                .map_err(|e| DbError::Query(e.to_string()))?;
                constraints_by_table.entry(table.clone()).or_default().push(info);
                if kind == ConstraintKind::Foreign {
                    if let (Some(parent), Some(parent_schema)) = (ref_table, ref_schema) {
                        let child_cols = cols.clone();
                        let parent_cols = ref_cols.clone().unwrap_or_default();
                        fk_grouped.entry(table.clone()).or_default().push((
                            conname.clone(),
                            child_cols.clone(),
                            parent.clone(),
                            parent_cols.clone(),
                        ));
                        all_edges.push(FkEdge {
                            child_table: table.clone(),
                            parent_schema: parent_schema.clone(),
                            parent_table: parent.clone(),
                            child_cols,
                            parent_cols,
                            name: conname.clone(),
                        });
                    }
                }
            }
            let mut fk_by_table: std::collections::HashMap<String, Vec<ForeignKeyInfo>> =
                std::collections::HashMap::new();
            let mut fk_truncated_by_table: std::collections::HashMap<String, bool> =
                std::collections::HashMap::new();
            for (table, groups) in fk_grouped {
                if groups.len() > SCHEMA_FK_CAP {
                    fk_truncated_by_table.insert(table.clone(), true);
                }
                let mut fks = Vec::new();
                for (conname, child_cols, parent, parent_cols) in
                    groups.into_iter().take(SCHEMA_FK_CAP)
                {
                    fks.push(
                        ForeignKeyInfo::new(
                            child_cols,
                            parent,
                            parent_cols,
                            Some(conname),
                        )
                        .map_err(|e| DbError::Query(e.to_string()))?,
                    );
                }
                fk_by_table.insert(table, fks);
            }
            let index_rows: Vec<(String, String, bool, Option<String>)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT t.relname, c.relname, ix.indisunique, a.attname \
                         FROM pg_class t \
                         JOIN pg_index ix ON t.oid = ix.indrelid \
                         JOIN pg_class c ON c.oid = ix.indexrelid \
                         LEFT JOIN pg_attribute a \
                         ON a.attrelid = t.oid AND a.attnum = ANY(ix.indkey) \
                         WHERE t.relkind IN ('r', 'm') \
                         AND t.relnamespace = $1::regnamespace \
                         AND ix.indisvalid \
                         ORDER BY t.relname, c.relname, \
                           array_position(ix.indkey, a.attnum)",
                    )
                    .bind(schema_name)
                    .fetch_all(&self.pool),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    Ok((
                        row.try_get::<String, _>(0)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(1)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<bool, _>(2)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<Option<String>, _>(3)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let mut idx_grouped: std::collections::HashMap<
                String,
                Vec<(String, bool, Vec<String>)>,
            > = std::collections::HashMap::new();
            {
                let mut current: Option<(String, String, bool, Vec<String>)> = None;
                let flush = |current: &mut Option<(String, String, bool, Vec<String>)>,
                             map: &mut std::collections::HashMap<
                    String,
                    Vec<(String, bool, Vec<String>)>,
                >| {
                    if let Some((table, name, unique, cols)) = current.take() {
                        map.entry(table).or_default().push((name, unique, cols));
                    }
                };
                for (table, index, unique, column) in index_rows {
                    match current.as_mut() {
                        Some(cur) if cur.0 == table && cur.1 == index => {
                            if let Some(col) = column {
                                if !col.trim().is_empty() {
                                    cur.3.push(col);
                                }
                            }
                        }
                        _ => {
                            flush(&mut current, &mut idx_grouped);
                            let cols = match column {
                                Some(col) if !col.trim().is_empty() => vec![col],
                                _ => Vec::new(),
                            };
                            current = Some((table, index, unique, cols));
                        }
                    }
                }
                flush(&mut current, &mut idx_grouped);
            }
            let mut idx_by_table: std::collections::HashMap<String, Vec<IndexInfo>> =
                std::collections::HashMap::new();
            let mut idx_truncated_by_table: std::collections::HashMap<String, bool> =
                std::collections::HashMap::new();
            let mut schema_indexes: Vec<IndexRef> = Vec::new();
            for (table, groups) in idx_grouped {
                if groups.len() > SCHEMA_INDEX_CAP {
                    idx_truncated_by_table.insert(table.clone(), true);
                }
                let mut idxs = Vec::new();
                for (index_name, unique, idx_cols) in
                    groups.into_iter().take(SCHEMA_INDEX_CAP)
                {
                    schema_indexes.push(
                        IndexRef::new(table.clone(), index_name.clone())
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    );
                    idxs.push(
                        IndexInfo::new(index_name, idx_cols, unique)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    );
                }
                idx_by_table.insert(table, idxs);
            }
            schema_indexes.sort_by(|a, b| (a.table(), a.index()).cmp(&(b.table(), b.index())));
            schema_indexes.truncate(SCHEMA_SCHEMA_INDEX_CAP);
            let seq_names: Vec<String> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT sequencename FROM pg_sequences \
                         WHERE schemaname = $1 \
                         ORDER BY sequencename",
                    )
                    .bind(schema_name)
                    .fetch_all(&self.pool),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    row.try_get::<String, _>(0).map_err(|e| DbError::Query(e.to_string()))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let mut sequences = Vec::new();
            for name in seq_names.into_iter().take(SCHEMA_SEQUENCE_CAP) {
                sequences.push(
                    SequenceInfo::new(name).map_err(|e| DbError::Query(e.to_string()))?,
                );
            }
            raw_schemas.push(RawSchema {
                name: schema_name.clone(),
                order,
                columns_by_table,
                pk_by_table,
                constraints_by_table,
                fk_by_table,
                fk_truncated_by_table,
                idx_by_table,
                idx_truncated_by_table,
                sequences,
                schema_indexes,
            });
        }
        let mut incoming: std::collections::HashMap<(String, String), Vec<ForeignKeyInfo>> =
            std::collections::HashMap::new();
        for edge in all_edges {
            let info = ForeignKeyInfo::new(
                edge.parent_cols,
                edge.child_table.clone(),
                edge.child_cols,
                Some(edge.name),
            )
            .map_err(|e| DbError::Query(e.to_string()))?;
            incoming
                .entry((edge.parent_schema, edge.parent_table))
                .or_default()
                .push(info);
        }
        let mut schema_nodes = Vec::new();
        for raw in &raw_schemas {
            let mut tables = Vec::new();
            let mut views = Vec::new();
            for (name, kind) in &raw.order {
                let col_rows = raw.columns_by_table.get(name).cloned().unwrap_or_default();
                if col_rows.len() > SCHEMA_COLUMN_CAP {
                    columns_truncated = true;
                }
                let pk_cols = raw.pk_by_table.get(name).cloned().unwrap_or_default();
                let mut pk_pos: std::collections::HashMap<String, u32> =
                    std::collections::HashMap::new();
                for (i, col) in pk_cols.iter().enumerate() {
                    pk_pos.insert(col.clone(), (i + 1) as u32);
                }
                let mut cols = Vec::new();
                for (col_name, type_name, nullable) in
                    col_rows.into_iter().take(SCHEMA_COLUMN_CAP)
                {
                    let pk_position = pk_pos.get(&col_name).copied();
                    let nullable = if pk_position.is_some() { false } else { nullable };
                    cols.push(
                        ColumnInfo::new(col_name, type_name, nullable, pk_position)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    );
                }
                let constraints =
                    raw.constraints_by_table.get(name).cloned().unwrap_or_default();
                let foreign_keys = raw.fk_by_table.get(name).cloned().unwrap_or_default();
                let referenced_by = incoming
                    .get(&(raw.name.clone(), name.clone()))
                    .cloned()
                    .unwrap_or_default();
                let indexes = raw.idx_by_table.get(name).cloned().unwrap_or_default();
                let fk_truncated =
                    raw.fk_truncated_by_table.get(name).copied().unwrap_or(false);
                let idx_truncated =
                    raw.idx_truncated_by_table.get(name).copied().unwrap_or(false);
                let table = TableInfo::new(
                    name.clone(),
                    *kind,
                    cols,
                    constraints,
                    foreign_keys,
                    referenced_by,
                    indexes,
                    fk_truncated,
                    idx_truncated,
                )
                .map_err(|e| DbError::Query(e.to_string()))?;
                match kind {
                    TableKind::Table => tables.push(table),
                    TableKind::View => views.push(table),
                }
            }
            schema_nodes.push(
                SchemaNode::new(
                    raw.name.clone(),
                    tables,
                    views,
                    raw.sequences.clone(),
                    raw.schema_indexes.clone(),
                )
                .map_err(|e| DbError::Query(e.to_string()))?,
            );
        }
        let db_node = DatabaseNode::new(self.database.clone(), schema_nodes)
            .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(SchemaInfo::new(vec![db_node], tables_truncated, columns_truncated))
    }
}

/// Builds the Postgres cancel closure. Owns the connection string plus the
/// backend pid captured at connect. Unknown pid no-ops. Else a fresh
/// current-thread runtime opens one connection and runs pg_cancel_backend.
/// Every error collapses into unit. Never panics, safe to fire twice.
#[cfg(feature = "postgres")]
fn pg_cancel_handle(connection_string: String, backend_pid: i32) -> CancelHandle {
    CancelHandle::new(move || {
        if backend_pid < 0 {
            return;
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            rt.block_on(async {
                let pool = match sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(Duration::from_secs(5))
                    .connect(&connection_string)
                    .await
                {
                    Ok(pool) => pool,
                    Err(_) => return,
                };
                let _ = sqlx::query("SELECT pg_cancel_backend($1)")
                    .bind(backend_pid)
                    .execute(&pool)
                    .await;
                pool.close().await;
            });
        }));
    })
}

/// One Postgres column's decoding rule, keyed by `TypeInfo::name()`.
#[cfg(feature = "postgres")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PgCellKind {
    Bool,
    Int16,
    Int32,
    Int64,
    Oid,
    Float32,
    Float64,
    Blob,
    /// Text rendering: strings, numerics, UUIDs, JSON, dates, times, and
    /// anything unlisted, including arrays and custom types.
    Text,
}

#[cfg(feature = "postgres")]
fn pg_cell_kind(type_name: &str) -> PgCellKind {
    match type_name {
        "BOOL" => PgCellKind::Bool,
        "INT2" => PgCellKind::Int16,
        "INT4" => PgCellKind::Int32,
        "INT8" => PgCellKind::Int64,
        "OID" => PgCellKind::Oid,
        "FLOAT4" => PgCellKind::Float32,
        "FLOAT8" => PgCellKind::Float64,
        "BYTEA" => PgCellKind::Blob,
        _ => PgCellKind::Text,
    }
}

/// Pure URL builder for Postgres. User, password, and database are
/// percent-encoded; an empty or missing password omits the secret part.
pub fn postgres_url(
    host: &str,
    port: u16,
    database: &str,
    user: &str,
    password: Option<&str>,
) -> String {
    let user_enc = percent_encode(user);
    let db_enc = percent_encode(database);
    match password {
        Some(s) if !s.is_empty() => {
            let enc: String = percent_encode(s);
            format!("postgres://{user_enc}:{enc}@{host}:{port}/{db_enc}")
        }
        _ => format!("postgres://{user_enc}@{host}:{port}/{db_enc}"),
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_' || *b == b'.' || *b == b'~' {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(feature = "postgres")]
fn map_pg_row(row: &sqlx::postgres::PgRow) -> Result<Vec<Value>, DbError> {
    use sqlx::{Column as _, Row as _, TypeInfo as _};
    let mut cells = Vec::with_capacity(row.len());
    for index in 0..row.len() {
        let type_name = row.column(index).type_info().name().to_owned();
        cells.push(decode_pg_cell(row, index, &type_name)?);
    }
    Ok(cells)
}

#[cfg(feature = "postgres")]
fn decode_pg_cell(
    row: &sqlx::postgres::PgRow,
    index: usize,
    type_name: &str,
) -> Result<Value, DbError> {
    match pg_cell_kind(type_name) {
        PgCellKind::Bool => pg_option_cell(row, index, |b: bool| Value::Integer(i64::from(b))),
        PgCellKind::Int16 => pg_option_cell(row, index, |n: i16| Value::Integer(i64::from(n))),
        PgCellKind::Int32 => pg_option_cell(row, index, |n: i32| Value::Integer(i64::from(n))),
        PgCellKind::Int64 => pg_option_cell(row, index, Value::Integer),
        PgCellKind::Oid => pg_option_cell(row, index, |o: sqlx::postgres::types::Oid| {
            Value::Integer(i64::from(o.0))
        }),
        PgCellKind::Float32 => pg_option_cell(row, index, |f: f32| Value::Real(f64::from(f))),
        PgCellKind::Float64 => pg_option_cell(row, index, Value::Real),
        PgCellKind::Blob => pg_option_cell(row, index, Value::Blob),
        // A query without binds runs on the simple protocol, so every value
        // arrives as text and the text-type compatibility gate is skipped for
        // NUMERIC, UUID, date/time, and JSON names.
        PgCellKind::Text => {
            use sqlx::Row as _;
            row.try_get_unchecked::<Option<String>, _>(index)
                .map(|cell| cell.map(Value::Text).unwrap_or(Value::Null))
                .map_err(|e| DbError::Query(e.to_string()))
        }
    }
}

#[cfg(feature = "postgres")]
fn pg_option_cell<T>(
    row: &sqlx::postgres::PgRow,
    index: usize,
    into_value: impl FnOnce(T) -> Value,
) -> Result<Value, DbError>
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    use sqlx::Row as _;
    row.try_get::<Option<T>, _>(index)
        .map(|cell| cell.map(into_value).unwrap_or(Value::Null))
        .map_err(|e| DbError::Query(e.to_string()))
}

fn decode_value(v: rusqlite::types::Value) -> Value {
    match v {
        rusqlite::types::Value::Null => Value::Null,
        rusqlite::types::Value::Integer(n) => Value::Integer(n),
        rusqlite::types::Value::Real(f) => Value::Real(f),
        rusqlite::types::Value::Text(s) => Value::Text(s),
        rusqlite::types::Value::Blob(b) => Value::Blob(b),
    }
}

fn execute_sqlite(
    conn: &rusqlite::Connection,
    sql: &str,
    max_rows: usize,
) -> Result<QueryResult, DbError> {
    let started = Instant::now();
    let mut stmt = conn.prepare(sql).map_err(|e| DbError::Query(e.to_string()))?;
    let width = stmt.column_count();
    if width == 0 {
        stmt.execute([]).map_err(|e| DbError::Query(e.to_string()))?;
        let affected = conn.changes();
        return QueryResult::from_columns(Vec::new(), false, Some(affected), started.elapsed())
            .map_err(|e| DbError::Query(e.to_string()));
    }
    let names: Vec<(String, String)> = stmt
        .columns()
        .iter()
        .map(|c| (c.name().to_owned(), c.decl_type().unwrap_or("").to_owned()))
        .collect();
    let mut builder = ResultBuilder::new(names, max_rows);
    {
        let mut mapped = stmt
            .query_map([], |row| {
                let mut cells = Vec::with_capacity(width);
                for i in 0..width {
                    cells.push(decode_value(row.get::<_, rusqlite::types::Value>(i)?));
                }
                Ok(cells)
            })
            .map_err(|e| DbError::Query(e.to_string()))?;
        for cells in mapped.by_ref() {
            let cells = cells.map_err(|e| DbError::Query(e.to_string()))?;
            if !builder.push_row(cells) {
                break;
            }
        }
    }
    Ok(builder.finish(started.elapsed()))
}

fn sqlite_foreign_keys(
    conn: &rusqlite::Connection,
    table: &str,
) -> Result<(Vec<ForeignKeyInfo>, bool), DbError> {
    let pragma = format!("PRAGMA foreign_key_list(\"{}\")", table.replace('"', "\"\""));
    let mut stmt = conn.prepare(&pragma).map_err(|e| DbError::Query(e.to_string()))?;
    let rows: Vec<(i64, i64, String, String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| DbError::Query(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| DbError::Query(e.to_string()))?;
    let mut grouped: std::collections::BTreeMap<i64, Vec<(i64, String, String, String)>> =
        std::collections::BTreeMap::new();
    for (id, seq, ref_table, from, to) in rows {
        grouped.entry(id).or_default().push((seq, ref_table, from, to));
    }
    let total = grouped.len();
    let truncated = total > SCHEMA_FK_CAP;
    let mut out = Vec::new();
    for (_, mut group) in grouped.into_iter().take(SCHEMA_FK_CAP) {
        group.sort_by_key(|(seq, _, _, _)| *seq);
        let ref_table = group.first().map(|(_, t, _, _)| t.clone()).unwrap_or_default();
        let columns: Vec<String> =
            group.iter().map(|(_, _, from, _)| from.clone()).collect();
        let ref_columns: Vec<String> =
            group.iter().map(|(_, _, _, to)| to.clone()).collect();
        out.push(
            ForeignKeyInfo::new(columns, ref_table, ref_columns, None)
                .map_err(|e| DbError::Query(e.to_string()))?,
        );
    }
    Ok((out, truncated))
}

fn sqlite_indexes(
    conn: &rusqlite::Connection,
    table: &str,
) -> Result<(Vec<IndexInfo>, bool), DbError> {
    let pragma = format!("PRAGMA index_list(\"{}\")", table.replace('"', "\"\""));
    let mut stmt = conn.prepare(&pragma).map_err(|e| DbError::Query(e.to_string()))?;
    let rows: Vec<(String, bool)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)? != 0))
        })
        .map_err(|e| DbError::Query(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| DbError::Query(e.to_string()))?;
    let mut names: Vec<(String, bool)> = rows
        .into_iter()
        .filter(|(name, _)| !name.starts_with("sqlite_"))
        .collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    let truncated = names.len() > SCHEMA_INDEX_CAP;
    let mut out = Vec::new();
    for (index_name, unique) in names.into_iter().take(SCHEMA_INDEX_CAP) {
        let info_pragma =
            format!("PRAGMA index_info(\"{}\")", index_name.replace('"', "\"\""));
        let mut info_stmt =
            conn.prepare(&info_pragma).map_err(|e| DbError::Query(e.to_string()))?;
        let mut cols: Vec<(i64, String)> = info_stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(2)?)))
            .map_err(|e| DbError::Query(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| DbError::Query(e.to_string()))?;
        cols.sort_by_key(|(seqno, _)| *seqno);
        let columns: Vec<String> = cols
            .into_iter()
            .map(|(_, name)| name)
            .filter(|n| !n.trim().is_empty())
            .collect();
        out.push(
            IndexInfo::new(index_name, columns, unique)
                .map_err(|e| DbError::Query(e.to_string()))?,
        );
    }
    Ok((out, truncated))
}

fn sqlite_database_name(conn: &rusqlite::Connection) -> String {
    let file: String = conn
        .query_row(
            "SELECT file FROM pragma_database_list WHERE name = 'main'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();
    if file.trim().is_empty() {
        ":memory:".to_owned()
    } else {
        file
    }
}

fn sqlite_table_constraints(
    columns: &[ColumnInfo],
    indexes: &[IndexInfo],
) -> Result<Vec<ConstraintInfo>, DbError> {
    let mut out = Vec::new();
    let mut pk_cols: Vec<(u32, String)> = columns
        .iter()
        .filter_map(|c| c.pk_position().map(|pos| (pos, c.name().to_owned())))
        .collect();
    pk_cols.sort_by_key(|(pos, _)| *pos);
    if !pk_cols.is_empty() {
        let names: Vec<String> = pk_cols.into_iter().map(|(_, n)| n).collect();
        out.push(
            ConstraintInfo::new(
                "PRIMARY KEY".to_owned(),
                ConstraintKind::Pk,
                names,
                None,
                Vec::new(),
                None,
            )
            .map_err(|e| DbError::Query(e.to_string()))?,
        );
    }
    for index in indexes.iter().filter(|i| i.is_unique()) {
        out.push(
            ConstraintInfo::new(
                index.name().to_owned(),
                ConstraintKind::Unique,
                index.columns().to_vec(),
                None,
                Vec::new(),
                None,
            )
            .map_err(|e| DbError::Query(e.to_string()))?,
        );
    }
    // CHECK constraints skipped: PRAGMA table_info and index_list do not
    // expose CHECK definitions, and parsing sqlite_master SQL is out of
    // scope for phase one.
    Ok(out)
}

fn sqlite_schema(conn: &rusqlite::Connection) -> Result<SchemaInfo, DbError> {
    let mut stmt = conn
        .prepare(
            "SELECT name, type FROM sqlite_master \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .map_err(|e| DbError::Query(e.to_string()))?;
    let entries: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|e| DbError::Query(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| DbError::Query(e.to_string()))?;
    let tables_truncated = entries.len() > SCHEMA_TABLE_CAP;
    let mut built: Vec<(TableInfo, Vec<ForeignKeyInfo>, String)> = Vec::new();
    let mut columns_truncated = false;
    let mut schema_indexes: Vec<IndexRef> = Vec::new();
    for (name, kind) in entries.into_iter().take(SCHEMA_TABLE_CAP) {
        let kind = if kind == "view" { TableKind::View } else { TableKind::Table };
        let pragma = format!("PRAGMA table_info(\"{}\")", name.replace('"', "\"\""));
        let mut col_stmt =
            conn.prepare(&pragma).map_err(|e| DbError::Query(e.to_string()))?;
        let raw_cols: Vec<(String, String, bool, u32)> = col_stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    row.get::<_, i64>(3)? != 0,
                    row.get::<_, i64>(5).unwrap_or(0).max(0) as u32,
                ))
            })
            .map_err(|e| DbError::Query(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| DbError::Query(e.to_string()))?;
        if raw_cols.len() > SCHEMA_COLUMN_CAP {
            columns_truncated = true;
        }
        let mut columns = Vec::new();
        for (col_name, type_name, notnull, pk) in
            raw_cols.into_iter().take(SCHEMA_COLUMN_CAP)
        {
            let pk_position = if pk > 0 { Some(pk) } else { None };
            let nullable = !notnull && pk_position.is_none();
            let col = ColumnInfo::new(col_name, type_name, nullable, pk_position)
                .map_err(|e| DbError::Query(e.to_string()))?;
            columns.push(col);
        }
        let (foreign_keys, fk_truncated) = if kind == TableKind::Table {
            sqlite_foreign_keys(conn, &name)?
        } else {
            (Vec::new(), false)
        };
        let (indexes, idx_truncated) = if kind == TableKind::Table {
            sqlite_indexes(conn, &name)?
        } else {
            (Vec::new(), false)
        };
        let constraints = if kind == TableKind::Table {
            sqlite_table_constraints(&columns, &indexes)?
        } else {
            Vec::new()
        };
        for index in &indexes {
            schema_indexes.push(
                IndexRef::new(name.clone(), index.name().to_owned())
                    .map_err(|e| DbError::Query(e.to_string()))?,
            );
        }
        let outgoing = foreign_keys.clone();
        let table = TableInfo::new(
            name.clone(),
            kind,
            columns,
            constraints,
            foreign_keys,
            Vec::new(),
            indexes,
            fk_truncated,
            idx_truncated,
        )
        .map_err(|e| DbError::Query(e.to_string()))?;
        built.push((table, outgoing, name));
    }
    schema_indexes.sort_by(|a, b| (a.table(), a.index()).cmp(&(b.table(), b.index())));
    schema_indexes.truncate(SCHEMA_SCHEMA_INDEX_CAP);
    let mut incoming: std::collections::HashMap<String, Vec<ForeignKeyInfo>> =
        std::collections::HashMap::new();
    for (_, outgoing, child_name) in &built {
        for fk in outgoing {
            let info = ForeignKeyInfo::new(
                fk.ref_columns().to_vec(),
                child_name.clone(),
                fk.columns().to_vec(),
                fk.constraint_name().map(|s| s.to_owned()),
            )
            .map_err(|e| DbError::Query(e.to_string()))?;
            incoming.entry(fk.ref_table().to_owned()).or_default().push(info);
        }
    }
    let mut tables_out: Vec<TableInfo> = Vec::new();
    let mut views_out: Vec<TableInfo> = Vec::new();
    for (table, _, name) in built {
        let refs = incoming.remove(&name).unwrap_or_default();
        let rebuilt = TableInfo::new(
            table.name().to_owned(),
            table.kind(),
            table.columns().to_vec(),
            table.constraints().to_vec(),
            table.foreign_keys().to_vec(),
            refs,
            table.indexes().to_vec(),
            table.foreign_keys_truncated(),
            table.indexes_truncated(),
        )
        .map_err(|e| DbError::Query(e.to_string()))?;
        match rebuilt.kind() {
            TableKind::Table => tables_out.push(rebuilt),
            TableKind::View => views_out.push(rebuilt),
        }
    }
    let db_name = sqlite_database_name(conn);
    let schema_node =
        SchemaNode::new("main".to_owned(), tables_out, views_out, Vec::new(), schema_indexes)
            .map_err(|e| DbError::Query(e.to_string()))?;
    let db_node =
        DatabaseNode::new(db_name, vec![schema_node]).map_err(|e| DbError::Query(e.to_string()))?;
    Ok(SchemaInfo::new(vec![db_node], tables_truncated, columns_truncated))
}

/// Spawns the connection's worker thread and returns its handle.
/// `password` is memory-only and never persisted; SQLite ignores it.
pub fn spawn_session(
    conn: ConnectionId,
    config: ConnectionConfig,
    password: Option<String>,
    ui: eframe::egui::Context,
    events: mpsc::Sender<DbEvent>,
) -> DbHandle {
    let (cmds_tx, cmds_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name(format!("cde-conn-{}", conn.raw()))
        .spawn(move || worker_loop(conn, config, password, ui, events, cmds_rx))
        .expect("spawn db worker thread");
    DbHandle { cmds: cmds_tx }
}

fn worker_loop(
    conn: ConnectionId,
    config: ConnectionConfig,
    password: Option<String>,
    ui: eframe::egui::Context,
    events: mpsc::Sender<DbEvent>,
    cmds: mpsc::Receiver<DbCommand>,
) {
    let mut session = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Session::open(&config, password.as_deref())
    })) {
        Ok(Ok(s)) => s,
        Ok(Err(error)) => {
            emit(&events, &ui, DbEvent::ConnectFailed { conn, error });
            return;
        }
        Err(_) => {
            emit(
                &events,
                &ui,
                DbEvent::ConnectFailed {
                    conn,
                    error: DbError::Open("driver panicked while opening".to_owned()),
                },
            );
            return;
        }
    };
    emit(
        &events,
        &ui,
        DbEvent::Connected {
            conn,
            cancel: session.cancel_handle(),
            server_version: session.server_version(),
        },
    );

    while let Ok(cmd) = cmds.recv() {
        match cmd {
            DbCommand::Query { run, sql } => {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    session.query(&sql)
                }));
                let event = match outcome {
                    Ok(Ok(result)) => DbEvent::QueryOk { run, result },
                    Ok(Err(error)) => DbEvent::QueryErr { run, error },
                    Err(_) => DbEvent::QueryErr {
                        run,
                        error: DbError::Query("driver panicked while running".to_owned()),
                    },
                };
                if events.send(event).is_err() {
                    break;
                }
                ui.request_repaint();
            }
            DbCommand::LoadSchema => {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    session.schema()
                }));
                let event = match outcome {
                    Ok(Ok(schema)) => DbEvent::SchemaOk { conn, schema },
                    Ok(Err(error)) => DbEvent::SchemaErr { conn, error },
                    Err(_) => DbEvent::SchemaErr {
                        conn,
                        error: DbError::Query("driver panicked while loading schema".to_owned()),
                    },
                };
                emit(&events, &ui, event);
            }
        }
    }
}

fn emit(events: &mpsc::Sender<DbEvent>, ui: &eframe::egui::Context, event: DbEvent) {
    if events.send(event).is_ok() {
        ui.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ragged_columns_rejected() {
        let cols = vec![
            Column::new("a".into(), "".into(), vec![Value::Integer(1)]),
            Column::new("b".into(), "".into(), vec![]),
        ];
        assert!(QueryResult::from_columns(cols, false, None, Duration::ZERO).is_err());
    }

    #[test]
    fn query_errors_are_not_fatal() {
        assert!(!DbError::Query("x".into()).is_fatal());
        assert!(DbError::Open("x".into()).is_fatal());
        assert!(DbError::Gone.is_fatal());
    }

    #[test]
    fn ids_are_unique_and_resumable() {
        let mut ids = Ids::new();
        let a = ids.connection();
        let b = ids.connection();
        assert_ne!(a, b);
        let _ = (ids.tab(), ids.run());
        ids.resume_after(&[ConnectionId::from_raw(100)]);
        assert!(ids.connection().raw() > 100);
    }

    #[test]
    fn postgres_url_without_password_omits_secret() {
        assert_eq!(
            postgres_url("localhost", 5432, "appdb", "owner", None),
            "postgres://owner@localhost:5432/appdb"
        );
    }

    #[test]
    fn postgres_url_empty_password_omits_secret() {
        assert_eq!(
            postgres_url("localhost", 5432, "appdb", "owner", Some("")),
            "postgres://owner@localhost:5432/appdb"
        );
    }

    #[test]
    fn postgres_url_with_password_includes_secret() {
        assert_eq!(
            postgres_url("db.internal", 5433, "analytics", "reader", Some("s3cret")),
            "postgres://reader:s3cret@db.internal:5433/analytics"
        );
    }

    #[test]
    fn postgres_url_encodes_special_chars_in_user_and_password() {
        assert_eq!(
            postgres_url("localhost", 5432, "appdb", "ad min", Some("p@ss:w/ord?")),
            "postgres://ad%20min:p%40ss%3Aw%2Ford%3F@localhost:5432/appdb"
        );
        assert_eq!(
            postgres_url("h", 1, "d", "u", Some("a#b%c!")),
            "postgres://u:a%23b%25c%21@h:1/d"
        );
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn pg_cell_kind_covers_listed_type_names() {
        assert_eq!(pg_cell_kind("BOOL"), PgCellKind::Bool);
        assert_eq!(pg_cell_kind("INT2"), PgCellKind::Int16);
        assert_eq!(pg_cell_kind("INT4"), PgCellKind::Int32);
        assert_eq!(pg_cell_kind("INT8"), PgCellKind::Int64);
        assert_eq!(pg_cell_kind("OID"), PgCellKind::Oid);
        assert_eq!(pg_cell_kind("FLOAT4"), PgCellKind::Float32);
        assert_eq!(pg_cell_kind("FLOAT8"), PgCellKind::Float64);
        assert_eq!(pg_cell_kind("BYTEA"), PgCellKind::Blob);
        for name in [
            "NUMERIC",
            "TEXT",
            "VARCHAR",
            "CHAR",
            "\"CHAR\"",
            "NAME",
            "UNKNOWN",
            "UUID",
            "JSON",
            "JSONB",
            "DATE",
            "TIME",
            "TIMETZ",
            "TIMESTAMP",
            "TIMESTAMPTZ",
            "INTERVAL",
            "VOID",
            "INT4[]",
            "TEXT[]",
            "some_custom_type",
        ] {
            assert_eq!(pg_cell_kind(name), PgCellKind::Text, "{name}");
        }
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn pg_cancel_handle_double_fire_is_safe_without_server() {
        let unknown =
            pg_cancel_handle("postgres://owner@localhost:5432/appdb".to_owned(), -1);
        unknown.cancel();
        unknown.cancel();
        let bad_url = pg_cancel_handle("not-a-url".to_owned(), 424_242);
        bad_url.cancel();
        bad_url.cancel();
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_cancel_frees_sleeping_worker() {
        let config = ConnectionConfig::Postgres {
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
        };
        let password = std::env::var("PGPASSWORD").ok();
        let session = Session::open(&config, password.as_deref()).expect("open");
        match &session {
            Session::Postgres(pg) => assert!(pg.backend_pid > 0),
            Session::Sqlite(_) => panic!("expected a Postgres session"),
        }
        let cancel = session.cancel_handle();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut session = session;
            let outcome = session.query("SELECT pg_sleep(30)");
            let _ = tx.send(outcome);
        });
        std::thread::sleep(Duration::from_millis(500));
        cancel.cancel();
        cancel.cancel();
        let outcome = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("worker returns within 10s after cancel");
        assert!(outcome.is_err());
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_roundtrip_covers_result_and_affected_paths() {
        let config = ConnectionConfig::Postgres {
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
        };
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref()).expect("open");
        assert!(!session.server_version().is_empty());
        session
            .query(
                "CREATE TEMPORARY TABLE cde_probe (id INT4, name TEXT, active BOOL, ratio FLOAT8, payload BYTEA, note TEXT)",
            )
            .expect("create");
        let inserted = session
            .query("INSERT INTO cde_probe VALUES (1, 'ada', TRUE, 1.5, '\\x0102', NULL)")
            .expect("insert");
        assert_eq!(inserted.affected(), Some(1));
        let result = session
            .query("SELECT id, name, active, ratio, payload, note FROM cde_probe ORDER BY id")
            .expect("select");
        assert_eq!(result.row_count(), 1);
        assert_eq!(result.affected(), None);
        assert_eq!(result.columns()[0].type_name(), "INT4");
        assert_eq!(result.columns()[2].type_name(), "BOOL");
        assert_eq!(result.cell(0, 0), Some(&Value::Integer(1)));
        assert_eq!(result.cell(0, 1), Some(&Value::Text("ada".to_owned())));
        assert_eq!(result.cell(0, 2), Some(&Value::Integer(1)));
        assert_eq!(result.cell(0, 3), Some(&Value::Real(1.5)));
        assert_eq!(result.cell(0, 4), Some(&Value::Blob(vec![0x01, 0x02])));
        assert_eq!(result.cell(0, 5), Some(&Value::Null));
    }

    #[test]
    fn schema_constructors_reject_empty_names() {
        assert!(ColumnInfo::new(String::new(), "TEXT".into(), true, None).is_err());
        assert!(ColumnInfo::new("  ".into(), "TEXT".into(), true, None).is_err());
        assert!(
            TableInfo::new(
                String::new(),
                TableKind::Table,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                false,
                false
            )
            .is_err()
        );
        assert!(
            TableInfo::new(
                " ".into(),
                TableKind::View,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                false,
                false
            )
            .is_err()
        );
        assert!(
            ForeignKeyInfo::new(
                vec!["child_id".into()],
                String::new(),
                vec!["id".into()],
                None
            )
            .is_err()
        );
        assert!(
            ForeignKeyInfo::new(vec![" ".into()], "parent".into(), vec!["id".into()], None)
                .is_err()
        );
        assert!(
            ForeignKeyInfo::new(
                vec!["child_id".into()],
                "parent".into(),
                vec!["id".into()],
                Some("  ".into())
            )
            .is_err()
        );
        assert!(IndexInfo::new(String::new(), vec!["id".into()], false).is_err());
        assert!(IndexInfo::new("  ".into(), vec!["id".into()], true).is_err());
        assert!(IndexInfo::new("idx_a".into(), vec![" ".into()], false).is_err());
        assert!(
            ConstraintInfo::new(
                String::new(),
                ConstraintKind::Pk,
                vec!["id".into()],
                None,
                Vec::new(),
                None
            )
            .is_err()
        );
        assert!(
            ConstraintInfo::new(
                " ".into(),
                ConstraintKind::Unique,
                vec!["id".into()],
                None,
                Vec::new(),
                None
            )
            .is_err()
        );
        assert!(SequenceInfo::new(String::new()).is_err());
        assert!(SequenceInfo::new("  ".into()).is_err());
        assert!(IndexRef::new(String::new(), "idx_a".into()).is_err());
        assert!(IndexRef::new("users".into(), "  ".into()).is_err());
        assert!(SchemaNode::new(String::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new())
            .is_err());
        assert!(DatabaseNode::new(String::new(), Vec::new()).is_err());
        assert!(ColumnInfo::new("id".into(), "INTEGER".into(), false, Some(1)).is_ok());
        assert!(
            ForeignKeyInfo::new(
                vec!["child_id".into()],
                "parent".into(),
                vec!["id".into()],
                None
            )
            .is_ok()
        );
        assert!(IndexInfo::new("idx_a".into(), vec!["id".into()], true).is_ok());
        assert!(
            ConstraintInfo::new(
                "PRIMARY KEY".into(),
                ConstraintKind::Pk,
                vec!["id".into()],
                None,
                Vec::new(),
                None
            )
            .is_ok()
        );
        assert!(SequenceInfo::new("seq_a".into()).is_ok());
        assert!(IndexRef::new("users".into(), "idx_a".into()).is_ok());
        assert!(
            TableInfo::new(
                "users".into(),
                TableKind::Table,
                vec![
                    ColumnInfo::new("id".into(), "INTEGER".into(), false, Some(1)).unwrap()
                ],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                false,
                false,
            )
            .is_ok()
        );
    }

    #[test]
    fn sqlite_schema_lists_tables_view_and_pk() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, note TEXT)",
            [],
        )
        .unwrap();
        conn.execute("CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER)", [])
            .unwrap();
        conn.execute("CREATE VIEW user_names AS SELECT name FROM users", []).unwrap();
        conn.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT)", [])
            .unwrap();
        conn.execute(
            "CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL \
              REFERENCES parent(id))",
            [],
        )
        .unwrap();
        conn.execute("CREATE UNIQUE INDEX idx_child_parent_unique ON child(parent_id)", [])
            .unwrap();
        conn.execute("CREATE INDEX idx_child_id_plain ON child(id)", []).unwrap();
        let session = Session::Sqlite(conn);
        let schema = session.schema().unwrap();
        assert!(!schema.tables_truncated());
        assert!(!schema.columns_truncated());
        assert_eq!(schema.databases().len(), 1);
        let db = &schema.databases()[0];
        assert_eq!(db.name(), ":memory:");
        assert_eq!(db.schemas().len(), 1);
        let main = &db.schemas()[0];
        assert_eq!(main.name(), "main");
        assert!(main.sequences().is_empty());
        let table_names: Vec<&str> =
            main.tables().iter().map(|t| t.name()).collect();
        assert_eq!(table_names, vec!["child", "parent", "posts", "users"]);
        let view_names: Vec<&str> = main.views().iter().map(|t| t.name()).collect();
        assert_eq!(view_names, vec!["user_names"]);
        let users = main.tables().iter().find(|t| t.name() == "users").unwrap();
        assert_eq!(users.kind(), TableKind::Table);
        assert_eq!(users.columns().len(), 3);
        let id = users.columns().iter().find(|c| c.name() == "id").unwrap();
        assert_eq!(id.pk_position(), Some(1));
        assert!(id.is_pk());
        assert!(!id.nullable());
        let name = users.columns().iter().find(|c| c.name() == "name").unwrap();
        assert_eq!(name.pk_position(), None);
        assert!(!name.nullable());
        assert_eq!(name.type_name(), "TEXT");
        let note = users.columns().iter().find(|c| c.name() == "note").unwrap();
        assert!(note.nullable());
        let pk: Vec<&ConstraintInfo> = users
            .constraints()
            .iter()
            .filter(|c| c.kind() == ConstraintKind::Pk)
            .collect();
        assert_eq!(pk.len(), 1);
        assert_eq!(pk[0].columns(), &["id".to_owned()]);
        assert!(users.referenced_by().is_empty());
        let view = main.views().iter().find(|t| t.name() == "user_names").unwrap();
        assert_eq!(view.kind(), TableKind::View);
        assert!(!view.columns().is_empty());
        assert!(view.constraints().is_empty());
        assert!(view.referenced_by().is_empty());
        let child = main.tables().iter().find(|t| t.name() == "child").unwrap();
        assert_eq!(child.foreign_keys().len(), 1);
        let fk = &child.foreign_keys()[0];
        assert_eq!(fk.columns(), &["parent_id".to_owned()]);
        assert_eq!(fk.ref_table(), "parent");
        assert_eq!(fk.ref_columns(), &["id".to_owned()]);
        assert!(child.referenced_by().is_empty());
        assert!(!child.foreign_keys_truncated());
        assert!(!child.indexes_truncated());
        let index_names: Vec<(&str, bool)> =
            child.indexes().iter().map(|i| (i.name(), i.is_unique())).collect();
        assert_eq!(
            index_names,
            vec![("idx_child_id_plain", false), ("idx_child_parent_unique", true)]
        );
        let unique_idx =
            child.indexes().iter().find(|i| i.name() == "idx_child_parent_unique").unwrap();
        assert!(unique_idx.unique());
        assert_eq!(unique_idx.columns(), &["parent_id".to_owned()]);
        let plain_idx =
            child.indexes().iter().find(|i| i.name() == "idx_child_id_plain").unwrap();
        assert!(!plain_idx.unique());
        assert_eq!(plain_idx.columns(), &["id".to_owned()]);
        let uniq: Vec<&ConstraintInfo> = child
            .constraints()
            .iter()
            .filter(|c| c.kind() == ConstraintKind::Unique)
            .collect();
        assert_eq!(uniq.len(), 1);
        assert_eq!(uniq[0].name(), "idx_child_parent_unique");
        assert_eq!(uniq[0].columns(), &["parent_id".to_owned()]);
        let parent = main.tables().iter().find(|t| t.name() == "parent").unwrap();
        assert!(parent.foreign_keys().is_empty());
        assert_eq!(parent.referenced_by().len(), 1);
        let incoming = &parent.referenced_by()[0];
        assert_eq!(incoming.ref_table(), "child");
        assert_eq!(incoming.ref_columns(), &["parent_id".to_owned()]);
        assert_eq!(incoming.columns(), &["id".to_owned()]);
        let parent_pk: Vec<&ConstraintInfo> = parent
            .constraints()
            .iter()
            .filter(|c| c.kind() == ConstraintKind::Pk)
            .collect();
        assert_eq!(parent_pk.len(), 1);
        let agg: Vec<(&str, &str)> = main
            .schema_indexes()
            .iter()
            .map(|r| (r.table(), r.index()))
            .collect();
        assert!(agg.contains(&("child", "idx_child_parent_unique")));
        assert!(agg.contains(&("child", "idx_child_id_plain")));
    }
}
