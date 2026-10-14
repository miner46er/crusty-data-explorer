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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
}

#[derive(Debug)]
pub enum DbEvent {
    Connected { conn: ConnectionId, cancel: CancelHandle, server_version: String },
    ConnectFailed { conn: ConnectionId, error: DbError },
    QueryOk { run: RunId, result: QueryResult },
    QueryErr { run: RunId, error: DbError },
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
            // Abandon-only: the tab drops the late result by run-id match. The
            // upgrade path is a pg_cancel_backend() against the backend pid
            // captured at connect.
            Session::Postgres(_) => CancelHandle::new(|| {}),
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
}

/// Behind the `postgres` feature. Owns a current-thread tokio runtime so the
/// async sqlx model stays caged inside this driver file; the public trait
/// shape never changes. The worker thread calls `block_on`, so the UI thread
/// never blocks.
#[cfg(feature = "postgres")]
pub struct PgSession {
    rt: tokio::runtime::Runtime,
    pool: sqlx::PgPool,
    // Kept for diagnostics; the pool owns the live copy.
    #[allow(dead_code)]
    connection_string: String,
    server_version: String,
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
        let connection_string = pg_connection_string(host, *port, database, user, password);
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
        Ok(Self {
            rt,
            pool,
            connection_string,
            server_version,
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

#[cfg(feature = "postgres")]
fn pg_connection_string(
    host: &str,
    port: u16,
    database: &str,
    user: &str,
    password: Option<&str>,
) -> String {
    match password {
        Some(secret) if !secret.is_empty() => {
            format!("postgres://{user}:{secret}@{host}:{port}/{database}")
        }
        _ => format!("postgres://{user}@{host}:{port}/{database}"),
    }
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

    #[cfg(feature = "postgres")]
    #[test]
    fn pg_connection_string_keeps_parts_verbatim() {
        assert_eq!(
            pg_connection_string("db.internal", 5433, "analytics", "reader", Some("s3cret!")),
            "postgres://reader:s3cret!@db.internal:5433/analytics"
        );
        assert_eq!(
            pg_connection_string("localhost", 5432, "appdb", "owner", None),
            "postgres://owner@localhost:5432/appdb"
        );
        assert_eq!(
            pg_connection_string("localhost", 5432, "appdb", "owner", Some("")),
            "postgres://owner@localhost:5432/appdb"
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
}
