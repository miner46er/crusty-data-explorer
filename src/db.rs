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
    pub fn open(config: &ConnectionConfig, _password: Option<&str>) -> Result<Session, DbError> {
        match config {
            ConnectionConfig::Sqlite { path } => rusqlite::Connection::open(path)
                .map(Session::Sqlite)
                .map_err(|e| DbError::Open(e.to_string())),
            ConnectionConfig::Postgres { .. } => Self::open_postgres(config),
        }
    }

    fn open_postgres(_config: &ConnectionConfig) -> Result<Session, DbError> {
        #[cfg(feature = "postgres")]
        {
            return Ok(Session::Postgres(PgSession::connect(_config)?));
        }
        #[cfg(not(feature = "postgres"))]
        {
            let _ = _config;
            return Err(DbError::NotSupported(
                "postgres needs the `postgres` feature".to_owned(),
            ));
        }
    }

    pub fn server_version(&self) -> String {
        match self {
            Session::Sqlite(_) => format!("SQLite {}", rusqlite::version()),
            #[cfg(feature = "postgres")]
            Session::Postgres(_) => "Postgres (stub)".to_owned(),
        }
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        match self {
            Session::Sqlite(conn) => {
                let handle = conn.get_interrupt_handle();
                CancelHandle::new(move || handle.interrupt())
            }
            #[cfg(feature = "postgres")]
            Session::Postgres(_) => CancelHandle::new(|| {}),
        }
    }

    /// One statement per call. Result columns become capped rows; no
    /// columns means an affected count via `changes()`.
    pub fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        match self {
            Session::Sqlite(conn) => execute_sqlite(conn, sql, ROW_CAP),
            #[cfg(feature = "postgres")]
            Session::Postgres(_) => Err(DbError::NotSupported(
                "postgres queries are not implemented yet".to_owned(),
            )),
        }
    }
}

/// Behind the `postgres` feature. Owns a current-thread tokio runtime so the
/// async sqlx model stays caged inside this driver file; the public trait
/// shape never changes. Query execution lands in the sqlx phase.
#[cfg(feature = "postgres")]
pub struct PgSession {
    #[allow(dead_code)]
    rt: tokio::runtime::Runtime,
}

#[cfg(feature = "postgres")]
impl PgSession {
    fn connect(_config: &ConnectionConfig) -> Result<Self, DbError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| DbError::Open(e.to_string()))?;
        Ok(Self { rt })
    }
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
}
