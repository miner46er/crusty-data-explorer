//! App state: the one mutation point for connection and tab lifecycle.
//!
//! Every field carrying an invariant is private to this module. Rendering
//! reads through accessors and changes state only through transitions.
//! Worker events fold into the per-connection [`ConnectionState`] machine in
//! exactly one place: [`AppState::apply`], drained from [`AppState::logic`].

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::mpsc;
use std::time::Instant;

use crate::db::{
    CancelHandle, ConnectionConfig, ConnectionId, DbCommand, DbEvent, DbHandle, DbKind,
    DriverRegistry, Ids, QueryResult, RunId, SchemaInfo, TabId, Value, explain_sql, spawn_session,
};
use crate::edit::{
    delete_statement, editable_table, insert_statement, is_text_type, qualified_parts,
    to_literal, update_statement,
};
use crate::sql::{current_statement, split_statements, statement_ranges};

/// Everything that changes about one connection's lifecycle, in five states.
/// `Connected` and `QueryRunning` carry a [`LiveSession`], so SQL can only
/// be sent where a session exists.
#[derive(Debug)]
pub enum ConnectionState {
    Disconnected,
    Connecting { handle: DbHandle },
    Connected { session: LiveSession },
    QueryRunning { session: LiveSession, run: RunId, tab: TabId },
    Error { message: String },
}

/// Handles the UI may use from the egui thread while a worker runs.
#[derive(Debug, Clone)]
pub struct LiveSession {
    pub handle: DbHandle,
    pub cancel: CancelHandle,
    pub server_version: String,
}

#[derive(Debug)]
pub struct Connection {
    pub id: ConnectionId,
    pub name: String,
    pub env: Environment,
    pub config: ConnectionConfig,
    pub password: String,
    state: ConnectionState,
}

impl Connection {
    pub fn state(&self) -> &ConnectionState {
        &self.state
    }
    /// Some only while a worker exists to queue commands to.
    pub fn handle(&self) -> Option<DbHandle> {
        match &self.state {
            ConnectionState::Connecting { handle } => Some(handle.clone()),
            ConnectionState::Connected { session }
            | ConnectionState::QueryRunning { session, .. } => Some(session.handle.clone()),
            ConnectionState::Disconnected | ConnectionState::Error { .. } => None,
        }
    }
}

/// Sort direction for one grid column. `Asc` is the default a fresh tab
/// carries while `sort_col` is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDir {
    #[default]
    Asc,
    Desc,
}

/// Which statements a run sends. `All` is the whole editor buffer, `Current`
/// the statement under the cursor, `Selection` the statements overlapping the
/// editor selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunScope {
    #[default]
    All,
    Current,
    Selection,
}

impl RunScope {
    pub const ALL: [RunScope; 3] = [RunScope::All, RunScope::Current, RunScope::Selection];
    pub fn label(self) -> &'static str {
        match self {
            RunScope::All => "All",
            RunScope::Current => "Current",
            RunScope::Selection => "Selection",
        }
    }
    /// Muted toolbar hint naming what the scope runs.
    pub fn hint(self) -> &'static str {
        match self {
            RunScope::All => "all statements",
            RunScope::Current => "statement at cursor",
            RunScope::Selection => "selected statements",
        }
    }
}

/// One staged cell edit: a result row and column with the text to write on
/// the next apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowEdit {
    pub row: usize,
    pub col: usize,
    pub text: String,
}

/// The table one tab's grid edits: its name and every primary key column
/// with the result column index its value shows in and whether its type is
/// text, so a text key can render as a quoted literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditTarget {
    pub table: String,
    pub pk_cols: Vec<(String, usize)>,
    pub pk_types: Vec<bool>,
}

#[derive(Debug)]
pub struct Tab {
    pub id: TabId,
    pub conn: ConnectionId,
    pub sql: String,
    outcome: RunOutcome,
    run_scope: RunScope,
    /// Display results kept when the outcome stops being `Ok`, so a failed
    /// run leaves the last finished grid on screen.
    results: Vec<QueryResult>,
    active_result: usize,
    sort_col: Option<usize>,
    sort_dir: SortDir,
    filter: String,
    export_notice: Option<String>,
    export_failed: bool,
    edits: Vec<RowEdit>,
    deleted_rows: Vec<usize>,
    added_rows: Vec<Vec<String>>,
    /// The apply batch in flight, so its landing can refresh the grid.
    pending_apply: Option<RunId>,
    /// The silent post-apply refresh in flight. It adds no history entry.
    silent_run: Option<RunId>,
}

impl Tab {
    pub fn outcome(&self) -> &RunOutcome {
        &self.outcome
    }
    pub fn run_scope(&self) -> RunScope {
        self.run_scope
    }
    pub fn set_run_scope(&mut self, scope: RunScope) {
        self.run_scope = scope;
    }
    /// Results on display: the finished run's while the outcome is `Ok`, else
    /// the last finished run's.
    pub fn results(&self) -> &[QueryResult] {
        match &self.outcome {
            RunOutcome::Ok { results, .. } => results,
            _ => &self.results,
        }
    }
    pub fn active_result(&self) -> usize {
        match &self.outcome {
            RunOutcome::Ok { active, .. } => *active,
            _ => self.active_result,
        }
    }
    /// The result the grid, exports, and counts read. Bounds-checked.
    pub fn active_query_result(&self) -> Option<&QueryResult> {
        self.results().get(self.active_result())
    }
    pub fn set_active_result(&mut self, index: usize) {
        let count = self.results().len();
        if count == 0 {
            return;
        }
        let index = index.min(count - 1);
        self.active_result = index;
        if let RunOutcome::Ok { active, .. } = &mut self.outcome {
            *active = index;
        }
    }
    pub fn sort_col(&self) -> Option<usize> {
        self.sort_col
    }
    pub fn sort_dir(&self) -> SortDir {
        self.sort_dir
    }
    pub fn filter(&self) -> &str {
        &self.filter
    }
    /// Cycle one column through none, asc, desc. A different column
    /// starts at asc.
    pub fn set_sort(&mut self, col: usize) {
        match self.sort_col {
            Some(current) if current == col => match self.sort_dir {
                SortDir::Asc => self.sort_dir = SortDir::Desc,
                SortDir::Desc => {
                    self.sort_col = None;
                    self.sort_dir = SortDir::Asc;
                }
            },
            _ => {
                self.sort_col = Some(col);
                self.sort_dir = SortDir::Asc;
            }
        }
    }
    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
    }
    pub fn export_notice(&self) -> Option<&str> {
        self.export_notice.as_deref()
    }
    pub fn export_failed(&self) -> bool {
        self.export_failed
    }
    pub fn set_export_notice(&mut self, notice: String, is_error: bool) {
        self.export_notice = Some(notice);
        self.export_failed = is_error;
    }
    pub fn clear_export_notice(&mut self) {
        self.export_notice = None;
        self.export_failed = false;
    }
    pub fn edits(&self) -> &[RowEdit] {
        &self.edits
    }
    /// Staged text for one result cell, if any.
    pub fn edit_text(&self, row: usize, col: usize) -> Option<&str> {
        self.edits
            .iter()
            .find(|e| e.row == row && e.col == col)
            .map(|e| e.text.as_str())
    }
    pub fn is_row_deleted(&self, row: usize) -> bool {
        self.deleted_rows.contains(&row)
    }
    pub fn added_rows(&self) -> &[Vec<String>] {
        &self.added_rows
    }
    /// How many statements the next apply would send: one per dirty row,
    /// one per deleted row, and one per added row.
    pub fn staged_count(&self) -> usize {
        let mut dirty_rows: Vec<usize> = self
            .edits
            .iter()
            .map(|e| e.row)
            .filter(|row| !self.deleted_rows.contains(row))
            .collect();
        dirty_rows.sort_unstable();
        dirty_rows.dedup();
        dirty_rows.len() + self.deleted_rows.len() + self.added_rows.len()
    }
}

/// One state per tab. `Running` carries the run id events must match and the
/// statements that run, so history can name each one.
#[derive(Debug)]
pub enum RunOutcome {
    Never,
    Running { run: RunId, started: Instant, statements: Vec<String> },
    Ok { results: Vec<QueryResult>, active: usize },
    Err(String),
}

/// Why a batch was sent, so its landing knows what it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchPurpose {
    Run,
    /// The staged edits; a successful landing re-reads the tab.
    ApplyEdits,
    /// The silent re-read after an apply; it adds no history entry.
    Refresh,
}

/// One finished run. Newest first in [`AppState::history`]. Holds the SQL
/// only, never credentials.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    pub conn: ConnectionId,
    pub sql: String,
    pub succeeded: bool,
    pub rows: Option<u64>,
    pub took: std::time::Duration,
    pub at: u64,
}

/// One named query. Newest first in [`AppState::saved`]. Holds the SQL and
/// its connection only, never credentials.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SavedQuery {
    pub id: u64,
    pub name: String,
    pub sql: String,
    pub conn: ConnectionId,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SavedQueryError {
    #[error("name is empty")]
    EmptyName,
}

impl SavedQuery {
    /// Checked constructor. Rejects empty names.
    pub fn new(
        id: u64,
        name: String,
        sql: String,
        conn: ConnectionId,
    ) -> Result<Self, SavedQueryError> {
        if name.trim().is_empty() {
            return Err(SavedQueryError::EmptyName);
        }
        Ok(Self { id, name, sql, conn })
    }
}

/// Cap on retained history entries, newest first.
const HISTORY_CAP: usize = 200;
/// Cap on history rows rendered in the sidebar.
pub const HISTORY_SHOWN: usize = 30;
/// Cap on retained saved queries, newest first.
const SAVED_CAP: usize = 200;

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The statements a run sends for `scope`, in editor order. `Selection` falls
/// back to the statement under the cursor when nothing is selected, so a
/// programmatic run and an empty selection still run something.
fn effective_statements(
    sql: &str,
    scope: RunScope,
    cursor: usize,
    selection: Option<Range<usize>>,
) -> Vec<String> {
    match scope {
        RunScope::All => split_statements(sql),
        RunScope::Current => {
            let statements = split_statements(sql);
            current_statement(sql, cursor)
                .and_then(|index| statements.get(index).cloned())
                .into_iter()
                .collect()
        }
        RunScope::Selection => match selection {
            Some(selection) => statement_ranges(sql)
                .into_iter()
                .filter(|range| range.start < selection.end && selection.start < range.end)
                .map(|range| sql[range].to_owned())
                .collect(),
            None => effective_statements(sql, RunScope::Current, cursor, None),
        },
    }
}

/// Primary key columns in key order with their declared types, from the
/// loaded schema tree. A qualified name matches under its schema; an
/// unqualified one takes the first table matching case-insensitively across
/// databases and schemas. A table without a key is not editable.
fn primary_key(schema: &SchemaInfo, table: &str) -> Option<Vec<(String, String)>> {
    let parts = qualified_parts(table);
    let (wanted_schema, wanted_table) = match parts.as_slice() {
        [table] => (None, table.as_str()),
        [schema, table] => (Some(schema.as_str()), table.as_str()),
        _ => return None,
    };
    for database in schema.databases() {
        for node in database.schemas() {
            if wanted_schema.is_some_and(|wanted| !node.name().eq_ignore_ascii_case(wanted)) {
                continue;
            }
            for info in node.tables() {
                if !info.name().eq_ignore_ascii_case(wanted_table) {
                    continue;
                }
                let mut columns: Vec<(u32, String, String)> = info
                    .columns()
                    .iter()
                    .filter_map(|c| {
                        c.pk_position().map(|position| {
                            (position, c.name().to_owned(), c.type_name().to_owned())
                        })
                    })
                    .collect();
                columns.sort_by_key(|(position, _, _)| *position);
                let key: Vec<(String, String)> = columns
                    .into_iter()
                    .map(|(_, name, type_name)| (name, type_name))
                    .collect();
                return (!key.is_empty()).then_some(key);
            }
        }
    }
    None
}

/// Key pairs for one row: each key column with the literal of the original
/// cell display, so the WHERE clause names the row exactly as displayed. A
/// text key always renders quoted; the rest keep [`to_literal`]'s mapping.
fn key_pairs(
    result: &QueryResult,
    target: &EditTarget,
    row: usize,
) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::with_capacity(target.pk_cols.len());
    for (index, (name, col)) in target.pk_cols.iter().enumerate() {
        let cell = result.cell(row, *col)?;
        let was_null = matches!(cell, Value::Null);
        let text_key = target.pk_types.get(index).copied().unwrap_or(false);
        let literal = key_literal(&cell.display(usize::MAX), was_null, text_key);
        pairs.push((name.clone(), literal));
    }
    Some(pairs)
}

/// One key cell as a WHERE literal. A text key renders through the escaped
/// quoted form even when it parses as a number, so `010` keeps its text
/// identity; other keys keep [`to_literal`]'s mapping.
fn key_literal(text: &str, was_null: bool, text_key: bool) -> String {
    if text_key && !was_null {
        return format!("'{}'", text.replace('\'', "''"));
    }
    to_literal(text, was_null)
}

/// Target tier for a connection. Prod connects route through a confirmation
/// first; the badge tells the tiers apart at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    #[default]
    Dev,
    Test,
    Prod,
}

impl Environment {
    pub const ALL: [Environment; 3] = [Environment::Dev, Environment::Test, Environment::Prod];
    pub fn label(self) -> &'static str {
        match self {
            Environment::Dev => "Dev",
            Environment::Test => "Test",
            Environment::Prod => "Prod",
        }
    }
    /// Badge tint. Neutral for dev, amber for test, red for prod.
    pub fn badge_color(self) -> eframe::egui::Color32 {
        match self {
            Environment::Dev => eframe::egui::Color32::from_rgb(148, 155, 166),
            Environment::Test => eframe::egui::Color32::from_rgb(227, 179, 65),
            Environment::Prod => eframe::egui::Color32::from_rgb(224, 91, 91),
        }
    }
}

#[derive(Debug)]
pub struct ConnectDraft {
    pub editing: Option<ConnectionId>,
    pub name: String,
    pub env: Environment,
    pub config: ConnectionConfig,
    pub password: String,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct SavePrompt {
    pub name: String,
    pub error: Option<String>,
}

pub struct AppState {
    connections: Vec<Connection>,
    tabs: Vec<Tab>,
    active: Option<TabId>,
    draft: Option<ConnectDraft>,
    password_prompt: Option<ConnectionId>,
    pending_prod: Option<ConnectionId>,
    prompt_password: String,
    ids: Ids,
    registry: DriverRegistry,
    events_tx: mpsc::Sender<DbEvent>,
    events_rx: mpsc::Receiver<DbEvent>,
    ui_ctx: eframe::egui::Context,
    schemas: HashMap<ConnectionId, SchemaInfo>,
    schema_loading: HashSet<ConnectionId>,
    schema_errors: HashMap<ConnectionId, String>,
    history: Vec<HistoryEntry>,
    saved: Vec<SavedQuery>,
    save_prompt: Option<SavePrompt>,
}

impl AppState {
    pub fn new(egui_ctx: &eframe::egui::Context) -> Self {
        let profiles = load_connections();
        let mut ids = Ids::new();
        ids.resume_after(&profiles.iter().map(|c| c.id).collect::<Vec<_>>());
        let saved = load_saved_queries();
        ids.resume_after_saved(&saved.iter().map(|q| q.id).collect::<Vec<_>>());
        let (events_tx, events_rx) = mpsc::channel();
        Self {
            connections: profiles,
            tabs: Vec::new(),
            active: None,
            draft: None,
            password_prompt: None,
            pending_prod: None,
            prompt_password: String::new(),
            ids,
            registry: DriverRegistry::bundled(),
            events_tx,
            events_rx,
            ui_ctx: egui_ctx.clone(),
            schemas: HashMap::new(),
            schema_loading: HashSet::new(),
            schema_errors: HashMap::new(),
            history: load_history(),
            saved,
            save_prompt: None,
        }
    }

    pub fn schema(&self, conn: ConnectionId) -> Option<&SchemaInfo> {
        self.schemas.get(&conn)
    }
    pub fn schema_is_loading(&self, conn: ConnectionId) -> bool {
        self.schema_loading.contains(&conn)
    }
    pub fn schema_error(&self, conn: ConnectionId) -> Option<&str> {
        self.schema_errors.get(&conn).map(String::as_str)
    }

    pub fn connections(&self) -> &[Connection] {
        &self.connections
    }
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }
    pub fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }
    pub fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }
    pub fn tab_sql_mut(&mut self, id: TabId) -> Option<&mut String> {
        self.tabs.iter_mut().find(|t| t.id == id).map(|t| &mut t.sql)
    }
    pub fn active(&self) -> Option<TabId> {
        self.active
    }
    pub fn draft(&self) -> Option<&ConnectDraft> {
        self.draft.as_ref()
    }
    pub fn draft_mut(&mut self) -> Option<&mut ConnectDraft> {
        self.draft.as_mut()
    }
    pub fn registry(&self) -> DriverRegistry {
        self.registry
    }
    /// Connection of the active tab, else the first live one, else None.
    pub fn default_connection(&self) -> Option<ConnectionId> {
        if let Some(active) = self.active {
            if let Some(tab) = self.tab(active) {
                return Some(tab.conn);
            }
        }
        self.connections
            .iter()
            .find(|c| {
                matches!(
                    c.state,
                    ConnectionState::Connected { .. } | ConnectionState::QueryRunning { .. }
                )
            })
            .map(|c| c.id)
    }

    pub fn new_tab(&mut self, conn: ConnectionId) -> TabId {
        let id = self.ids.tab();
        self.tabs.push(Tab {
            id,
            conn,
            sql: String::new(),
            outcome: RunOutcome::Never,
            run_scope: RunScope::All,
            results: Vec::new(),
            active_result: 0,
            sort_col: None,
            sort_dir: SortDir::Asc,
            filter: String::new(),
            export_notice: None,
            export_failed: false,
            edits: Vec::new(),
            deleted_rows: Vec::new(),
            added_rows: Vec::new(),
            pending_apply: None,
            silent_run: None,
        });
        if self.active.is_none() {
            self.active = Some(id);
        }
        id
    }

    pub fn close_tab(&mut self, tab: TabId) {
        self.tabs.retain(|t| t.id != tab);
        self.repair_active();
    }

    pub fn activate(&mut self, tab: TabId) {
        if self.tabs.iter().any(|t| t.id == tab) {
            self.active = Some(tab);
        }
    }

    /// Resolve the tab's scope against the editor text and send the batch.
    /// An empty effective set is an error before anything reaches the worker.
    pub fn start_run(
        &mut self,
        tab: TabId,
        sql: &str,
        cursor: usize,
        selection: Option<Range<usize>>,
    ) {
        self.start_batch(tab, sql, cursor, selection, |_, statement| statement);
    }

    /// Send the same statements [`AppState::start_run`] would, each in its
    /// engine's explain form, so the plan renders through the grid like any
    /// other batch.
    pub fn start_explain(
        &mut self,
        tab: TabId,
        sql: &str,
        cursor: usize,
        selection: Option<Range<usize>>,
    ) {
        self.start_batch(tab, sql, cursor, selection, |kind, statement| {
            explain_sql(kind, &statement)
        });
    }

    /// The shared body of run and explain. Resolves the tab's connection, its
    /// handle, and the effective statements, maps each through `prepare`, and
    /// sends one batch. Both entry points share these error paths: a missing
    /// connection or an empty effective set never sends.
    fn start_batch(
        &mut self,
        tab: TabId,
        sql: &str,
        cursor: usize,
        selection: Option<Range<usize>>,
        prepare: impl Fn(DbKind, String) -> String,
    ) {
        let (conn_id, kind, handle, scope) = match self.tabs.iter().find(|t| t.id == tab) {
            Some(t) => match self.connections.iter().find(|c| c.id == t.conn) {
                Some(c) => (t.conn, c.config.kind(), c.handle(), t.run_scope),
                None => {
                    self.set_outcome(tab, RunOutcome::Err("connection not found".to_owned()));
                    return;
                }
            },
            None => return,
        };
        let statements = effective_statements(sql, scope, cursor, selection);
        if statements.is_empty() {
            self.set_outcome(tab, RunOutcome::Err("nothing to run".to_owned()));
            return;
        }
        let statements: Vec<String> =
            statements.into_iter().map(|statement| prepare(kind, statement)).collect();
        self.send_batch(tab, conn_id, handle, statements, BatchPurpose::Run);
    }

    /// One prepared batch: mint the run, send it, and fold the tab's outcome
    /// and the connection's running state. `purpose` marks the batch so the
    /// landing knows what it ended.
    fn send_batch(
        &mut self,
        tab: TabId,
        conn_id: ConnectionId,
        handle: Option<DbHandle>,
        statements: Vec<String>,
        purpose: BatchPurpose,
    ) {
        debug_assert!(!statements.is_empty());
        let run = self.ids.run();
        match handle {
            Some(handle) => {
                match handle.send(DbCommand::QueryBatch { run, statements: statements.clone() })
                {
                    Ok(()) => {
                        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                            match purpose {
                                BatchPurpose::Run => {}
                                BatchPurpose::ApplyEdits => t.pending_apply = Some(run),
                                BatchPurpose::Refresh => t.silent_run = Some(run),
                            }
                        }
                        self.set_outcome(
                            tab,
                            RunOutcome::Running { run, started: Instant::now(), statements },
                        );
                        if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn_id) {
                            if let ConnectionState::Connected { session } = &c.state {
                                let session = session.clone();
                                c.state = ConnectionState::QueryRunning { session, run, tab };
                            }
                        }
                    }
                    Err(error) => self.set_outcome(tab, RunOutcome::Err(error.to_string())),
                }
            }
            None => self.set_outcome(tab, RunOutcome::Err("not connected".to_owned())),
        }
    }

    /// Cancel as abandon: the tab stops waiting and the late result is
    /// dropped by id match. The engine interrupt fires first when the run
    /// is still the connection's current one.
    pub fn cancel_run(&mut self, tab: TabId) {
        let (run, conn_id) = match self.tabs.iter().find(|t| t.id == tab) {
            Some(t) => match &t.outcome {
                RunOutcome::Running { run, .. } => (*run, t.conn),
                _ => return,
            },
            None => return,
        };
        self.set_outcome(tab, RunOutcome::Err("cancelled".to_owned()));
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
            if t.pending_apply == Some(run) {
                t.pending_apply = None;
            }
            if t.silent_run == Some(run) {
                t.silent_run = None;
            }
        }
        if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn_id) {
            if let ConnectionState::QueryRunning { session, run: current, .. } = &c.state {
                if *current == run {
                    let session = session.clone();
                    session.cancel.cancel();
                    c.state = ConnectionState::Connected { session };
                }
            }
        }
    }

    /// What the tab's grid can edit, if anything. Some only while the
    /// connection is idle, the tab's SQL is a single-table SELECT, the loaded
    /// schema carries that table with a primary key, and every key column
    /// appears in the active result.
    pub fn edit_target(&self, tab: TabId) -> Option<EditTarget> {
        let tab = self.tabs.iter().find(|t| t.id == tab)?;
        let idle = matches!(
            self.connections.iter().find(|c| c.id == tab.conn)?.state(),
            ConnectionState::Connected { .. }
        );
        if !idle {
            return None;
        }
        let table = editable_table(&tab.sql)?;
        let pk = primary_key(self.schemas.get(&tab.conn)?, &table)?;
        let result = tab.active_query_result()?;
        let mut pk_cols = Vec::with_capacity(pk.len());
        let mut pk_types = Vec::with_capacity(pk.len());
        for (column, type_name) in pk {
            let index = result
                .columns()
                .iter()
                .position(|c| c.name().eq_ignore_ascii_case(&column))?;
            pk_cols.push((column, index));
            pk_types.push(is_text_type(&type_name));
        }
        Some(EditTarget { table, pk_cols, pk_types })
    }

    /// Stage one cell edit. `row` counts result rows first, then staged added
    /// rows. No-op unless the target resolves and the cell is in the active
    /// result or a staged added row. A later stage on the same cell replaces
    /// its text.
    pub fn stage_edit(&mut self, tab: TabId, row: usize, col: usize, text: String) {
        if self.edit_target(tab).is_none() {
            return;
        }
        let Some((rows, cols)) = self.result_shape(tab) else { return };
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) else { return };
        match row.checked_sub(rows).filter(|index| *index < t.added_rows.len()) {
            Some(index) => {
                if col < cols {
                    t.added_rows[index][col] = text;
                }
            }
            None => {
                if row < rows && col < cols {
                    match t.edits.iter_mut().find(|e| e.row == row && e.col == col) {
                        Some(edit) => edit.text = text,
                        None => t.edits.push(RowEdit { row, col, text }),
                    }
                }
            }
        }
    }

    /// Drop every staged change on the tab.
    pub fn discard_edits(&mut self, tab: TabId) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
            t.edits.clear();
            t.deleted_rows.clear();
            t.added_rows.clear();
        }
    }

    /// Stage a DELETE for one result row. Marking the row again restores it.
    /// No-op unless the target resolves and the row is in the active result.
    pub fn mark_row_deleted(&mut self, tab: TabId, row: usize) {
        if self.edit_target(tab).is_none() {
            return;
        }
        let Some((rows, _)) = self.result_shape(tab) else { return };
        if row >= rows {
            return;
        }
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) else { return };
        match t.deleted_rows.iter().position(|r| *r == row) {
            Some(index) => {
                t.deleted_rows.remove(index);
            }
            None => t.deleted_rows.push(row),
        }
    }

    /// Stage one empty row for INSERT, with one cell per result column.
    /// No-op unless the target resolves.
    pub fn add_row(&mut self, tab: TabId) {
        if self.edit_target(tab).is_none() {
            return;
        }
        let Some((_, cols)) = self.result_shape(tab) else { return };
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) else { return };
        t.added_rows.push(vec![String::new(); cols]);
    }

    /// Send every staged change as one batch: an UPDATE per dirty row, a
    /// DELETE per deleted row, then an INSERT per added row. Nothing sends
    /// when nothing is staged or the target does not resolve.
    pub fn apply_edits(&mut self, tab: TabId) {
        let Some(target) = self.edit_target(tab) else { return };
        let Some(statements) = self.edit_statements(tab, &target) else { return };
        if statements.is_empty() {
            return;
        }
        let (conn_id, handle) = match self.tabs.iter().find(|t| t.id == tab) {
            Some(t) => match self.connections.iter().find(|c| c.id == t.conn) {
                Some(c) => (t.conn, c.handle()),
                None => return,
            },
            None => return,
        };
        self.send_batch(tab, conn_id, handle, statements, BatchPurpose::ApplyEdits);
    }

    /// Rows and columns of the tab's active result. None when there is none.
    fn result_shape(&self, tab: TabId) -> Option<(usize, usize)> {
        let result = self.tabs.iter().find(|t| t.id == tab)?.active_query_result()?;
        Some((result.row_count(), result.columns().len()))
    }

    /// The statements for the staged changes, in apply order. None when the
    /// displayed result cannot be read.
    fn edit_statements(&self, tab: TabId, target: &EditTarget) -> Option<Vec<String>> {
        let t = self.tabs.iter().find(|t| t.id == tab)?;
        let result = t.active_query_result()?;
        let columns: Vec<String> =
            result.columns().iter().map(|c| c.name().to_owned()).collect();
        let mut statements = Vec::new();
        let mut dirty_rows: Vec<usize> = t.edits.iter().map(|e| e.row).collect();
        dirty_rows.sort_unstable();
        dirty_rows.dedup();
        for row in dirty_rows {
            if t.is_row_deleted(row) {
                continue;
            }
            let mut sets: Vec<(String, String)> = Vec::new();
            for col in 0..columns.len() {
                if let Some(text) = t.edit_text(row, col) {
                    sets.push((columns[col].clone(), text.to_owned()));
                }
            }
            if sets.is_empty() {
                continue;
            }
            let pk = key_pairs(result, target, row)?;
            statements.push(update_statement(&target.table, &pk, &sets));
        }
        for row in &t.deleted_rows {
            statements.push(delete_statement(&target.table, &key_pairs(result, target, *row)?));
        }
        for added in &t.added_rows {
            statements.push(insert_statement(&target.table, &columns, added));
        }
        Some(statements)
    }

    /// Re-run the tab's SQL once after an apply so the grid reads the rows
    /// back. The run is silent: it adds no history entry.
    fn start_refresh(&mut self, tab: TabId) {
        let sql = match self.tabs.iter().find(|t| t.id == tab) {
            Some(t) => t.sql.clone(),
            None => return,
        };
        let statements = effective_statements(&sql, RunScope::All, 0, None);
        if statements.is_empty() {
            return;
        }
        let (conn_id, handle) = match self.tabs.iter().find(|t| t.id == tab) {
            Some(t) => match self.connections.iter().find(|c| c.id == t.conn) {
                Some(c) => (t.conn, c.handle()),
                None => return,
            },
            None => return,
        };
        self.send_batch(tab, conn_id, handle, statements, BatchPurpose::Refresh);
    }

    /// Connect from a user gesture. A prod connection arms the confirmation
    /// instead of spawning; only the confirmed path reaches the spawn.
    pub fn connect(&mut self, id: ConnectionId) {
        if self.prod_needs_confirm(id) {
            self.pending_prod = Some(id);
            return;
        }
        self.connect_now(id);
    }

    /// The spawn path. Confirmation and password continuations enter here so
    /// a confirmed prod connection is never re-armed.
    fn connect_now(&mut self, id: ConnectionId) {
        let (config, password) = match self.connections.iter().find(|c| c.id == id) {
            Some(c) => {
                if matches!(
                    c.state,
                    ConnectionState::Connecting { .. }
                        | ConnectionState::Connected { .. }
                        | ConnectionState::QueryRunning { .. }
                ) {
                    return;
                }
                let password = if c.password.is_empty() {
                    None
                } else {
                    Some(c.password.clone())
                };
                (c.config.clone(), password)
            }
            None => return,
        };
        if let Err(error) = config.validate() {
            if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
                c.state = ConnectionState::Error { message: error.to_string() };
            }
            return;
        }
        let handle =
            spawn_session(id, config, password, self.ui_ctx.clone(), self.events_tx.clone());
        if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
            c.state = ConnectionState::Connecting { handle };
        }
    }

    fn prod_needs_confirm(&self, id: ConnectionId) -> bool {
        self.connections
            .iter()
            .find(|c| c.id == id)
            .is_some_and(|c| c.env == Environment::Prod)
    }

    /// The connection awaiting a prod confirmation, if any.
    pub fn pending_prod(&self) -> Option<ConnectionId> {
        self.pending_prod
    }

    /// Proceed with the connection whose prod confirmation is pending,
    /// including the Postgres password prompt when no password is stored.
    pub fn confirm_prod(&mut self) {
        let Some(id) = self.pending_prod.take() else { return };
        self.prompt_or_connect(id);
    }

    /// Drop the pending prod confirmation without connecting.
    pub fn cancel_prod(&mut self) {
        self.pending_prod = None;
    }

    /// Memory-only password update for a later prompt. Never persisted.
    pub fn set_password(&mut self, id: ConnectionId, password: String) {
        if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
            c.password = password;
        }
    }

    pub fn password_prompt(&self) -> Option<ConnectionId> {
        self.password_prompt
    }

    pub fn prompt_password(&self) -> &str {
        &self.prompt_password
    }

    pub fn prompt_password_mut(&mut self) -> &mut String {
        &mut self.prompt_password
    }

    pub fn open_password_prompt(&mut self, id: ConnectionId) {
        if self.prod_needs_confirm(id) {
            self.pending_prod = Some(id);
            return;
        }
        self.prompt_or_connect(id);
    }

    /// Prompt when a Postgres connection has no password, else spawn.
    fn prompt_or_connect(&mut self, id: ConnectionId) {
        let should_prompt = match self.connections.iter().find(|c| c.id == id) {
            Some(c) => {
                matches!(c.config, ConnectionConfig::Postgres { .. }) && c.password.is_empty()
            }
            None => return,
        };
        if should_prompt {
            self.password_prompt = Some(id);
            self.prompt_password.clear();
        } else {
            self.connect_now(id);
        }
    }

    pub fn submit_password(&mut self) {
        let Some(id) = self.password_prompt.take() else { return };
        let password = std::mem::take(&mut self.prompt_password);
        self.set_password(id, password);
        self.connect_now(id);
    }

    pub fn cancel_password_prompt(&mut self) {
        self.password_prompt = None;
        self.prompt_password.clear();
    }

    pub fn disconnect(&mut self, id: ConnectionId) {
        if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
            c.state = ConnectionState::Disconnected;
        }
        self.schemas.remove(&id);
        self.schema_loading.remove(&id);
        self.schema_errors.remove(&id);
    }

    pub fn delete_connection(&mut self, id: ConnectionId) {
        self.disconnect(id);
        self.tabs.retain(|t| t.conn != id);
        self.connections.retain(|c| c.id != id);
        self.schemas.remove(&id);
        self.schema_loading.remove(&id);
        self.schema_errors.remove(&id);
        self.repair_active();
        self.persist();
    }

    pub fn refresh_schema(&mut self, conn: ConnectionId) {
        let handle = match self.connections.iter().find(|c| c.id == conn) {
            Some(c) => match &c.state {
                ConnectionState::Connected { session }
                | ConnectionState::QueryRunning { session, .. } => session.handle.clone(),
                _ => return,
            },
            None => return,
        };
        self.schema_loading.insert(conn);
        self.schema_errors.remove(&conn);
        if handle.send(DbCommand::LoadSchema).is_err() {
            self.schema_loading.remove(&conn);
            self.schema_errors.insert(conn, "connection is gone".to_owned());
        }
    }

    pub fn preview_table(&mut self, conn: ConnectionId, schema: &str, table: &str) {
        if self.connections.iter().find(|c| c.id == conn).is_none() {
            return;
        }
        let sql = format!(
            "SELECT * FROM \"{}\".\"{}\" LIMIT 100",
            schema.replace('"', "\"\""),
            table.replace('"', "\"\"")
        );
        let existing = self.tabs.iter().find(|t| t.conn == conn && t.sql == sql).map(|t| t.id);
        match existing {
            Some(id) => {
                self.activate(id);
                self.start_run(id, &sql.clone(), sql.len(), None);
            }
            None => {
                let id = self.new_tab(conn);
                if let Some(slot) = self.tab_sql_mut(id) {
                    *slot = sql.clone();
                }
                self.activate(id);
                self.start_run(id, &sql, sql.len(), None);
            }
        }
    }

    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    pub fn history_for(&self, conn: ConnectionId) -> Vec<&HistoryEntry> {
        self.history.iter().filter(|e| e.conn == conn).collect()
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        self.persist_history();
    }

    pub fn saved_queries(&self) -> &[SavedQuery] {
        &self.saved
    }

    /// Save the active tab's SQL under `name` for its connection. A duplicate
    /// name replaces the existing entry in place and keeps its id.
    pub fn save_active_tab(&mut self, name: &str) -> Option<u64> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return None;
        }
        let (conn, sql) = match self.active.and_then(|id| self.tab(id)) {
            Some(tab) => (tab.conn, tab.sql.clone()),
            None => return None,
        };
        if let Some(existing) = self.saved.iter_mut().find(|q| q.name == trimmed) {
            existing.sql = sql;
            existing.conn = conn;
            let id = existing.id;
            self.persist_saved();
            return Some(id);
        }
        let id = self.ids.saved();
        match SavedQuery::new(id, trimmed.to_owned(), sql, conn) {
            Ok(query) => {
                self.saved.insert(0, query);
                self.saved.truncate(SAVED_CAP);
                self.persist_saved();
                Some(id)
            }
            Err(_) => None,
        }
    }

    /// Open a saved query in a fresh tab on its connection. Never auto-runs.
    pub fn open_saved(&mut self, id: u64) {
        let Some(query) = self.saved.iter().find(|q| q.id == id).cloned() else {
            return;
        };
        let tab = self.new_tab(query.conn);
        if let Some(slot) = self.tab_sql_mut(tab) {
            *slot = query.sql;
        }
        self.activate(tab);
        self.set_outcome(tab, RunOutcome::Never);
    }

    pub fn rename_saved(&mut self, id: u64, name: &str) -> bool {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return false;
        }
        let Some(query) = self.saved.iter_mut().find(|q| q.id == id) else {
            return false;
        };
        query.name = trimmed.to_owned();
        self.persist_saved();
        true
    }

    pub fn delete_saved(&mut self, id: u64) -> bool {
        let before = self.saved.len();
        self.saved.retain(|q| q.id != id);
        if self.saved.len() == before {
            return false;
        }
        self.persist_saved();
        true
    }

    pub fn save_prompt(&self) -> Option<&SavePrompt> {
        self.save_prompt.as_ref()
    }

    pub fn save_prompt_mut(&mut self) -> Option<&mut SavePrompt> {
        self.save_prompt.as_mut()
    }

    pub fn open_save_prompt(&mut self) {
        self.save_prompt =
            Some(SavePrompt { name: String::new(), error: None });
    }

    pub fn close_save_prompt(&mut self) {
        self.save_prompt = None;
    }

    pub fn confirm_save_prompt(&mut self) -> Option<u64> {
        let name = self
            .save_prompt
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_default();
        if name.trim().is_empty() {
            if let Some(prompt) = self.save_prompt.as_mut() {
                prompt.error = Some("name is empty".to_owned());
            }
            return None;
        }
        match self.save_active_tab(&name) {
            Some(id) => {
                self.save_prompt = None;
                Some(id)
            }
            None => {
                if let Some(prompt) = self.save_prompt.as_mut() {
                    prompt.error = Some("nothing to save".to_owned());
                }
                None
            }
        }
    }

    /// Reopen a past query in a fresh tab. Runs immediately when the
    /// connection is live, else loads the SQL with a `Never` outcome.
    pub fn open_history(&mut self, conn: ConnectionId, sql: &str) {
        let id = self.new_tab(conn);
        if let Some(slot) = self.tab_sql_mut(id) {
            *slot = sql.to_owned();
        }
        self.activate(id);
        let live = self.connections.iter().find(|c| c.id == conn).is_some_and(|c| {
            matches!(
                c.state(),
                ConnectionState::Connected { .. } | ConnectionState::QueryRunning { .. }
            )
        });
        if live {
            self.start_run(id, sql, sql.len(), None);
        } else {
            self.set_outcome(id, RunOutcome::Never);
        }
    }

    pub fn open_draft(&mut self, editing: Option<ConnectionId>) {
        let (name, env, config, password) = match editing {
            Some(id) => match self.connections.iter().find(|c| c.id == id) {
                Some(c) => (c.name.clone(), c.env, c.config.clone(), c.password.clone()),
                None => return,
            },
            None => (
                String::new(),
                Environment::default(),
                DbKind::Sqlite.default_config(),
                String::new(),
            ),
        };
        self.draft = Some(ConnectDraft { editing, name, env, config, password, error: None });
    }

    pub fn cancel_draft(&mut self) {
        self.draft = None;
    }

    pub fn save_draft(&mut self) {
        let Some(draft) = self.draft.take() else { return };
        if draft.name.trim().is_empty() {
            self.draft = Some(ConnectDraft {
                error: Some("name is empty".to_owned()),
                ..draft
            });
            return;
        }
        if let Err(error) = draft.config.validate() {
            self.draft = Some(ConnectDraft {
                error: Some(error.to_string()),
                ..draft
            });
            return;
        }
        match draft.editing {
            Some(id) => {
                if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
                    c.name = draft.name;
                    c.env = draft.env;
                    c.config = draft.config;
                    c.password = draft.password;
                }
            }
            None => {
                let id = self.ids.connection();
                self.connections.push(Connection {
                    id,
                    name: draft.name,
                    env: draft.env,
                    config: draft.config,
                    password: draft.password,
                    state: ConnectionState::Disconnected,
                });
            }
        }
        self.persist();
    }

    /// The one write of a tab's outcome. Leaving `Ok` moves its results into
    /// the tab's display state, so a later failed or refused run keeps the
    /// last finished grid on screen.
    fn set_outcome(&mut self, tab: TabId, outcome: RunOutcome) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
            if let RunOutcome::Ok { results, active } = &mut t.outcome {
                t.results = std::mem::take(results);
                t.active_result = *active;
            }
            t.outcome = outcome;
            t.clear_export_notice();
        }
    }

    fn repair_active(&mut self) {
        if self.active.is_some_and(|a| self.tabs.iter().any(|t| t.id == a)) {
            return;
        }
        self.active = self.tabs.last().map(|t| t.id);
    }

    /// The one fold of worker events into state. Every arm converges:
    /// replaying an event lands on the same state, so duplicates are no-ops.
    fn apply(&mut self, event: DbEvent) {
        match event {
            DbEvent::Connected { conn, cancel, server_version } => {
                let handle = if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn)
                {
                    if let ConnectionState::Connecting { handle } = &c.state {
                        let session = LiveSession {
                            handle: handle.clone(),
                            cancel,
                            server_version,
                        };
                        let handle = session.handle.clone();
                        c.state = ConnectionState::Connected { session };
                        Some(handle)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(handle) = handle {
                    self.schema_loading.insert(conn);
                    self.schema_errors.remove(&conn);
                    if handle.send(DbCommand::LoadSchema).is_err() {
                        self.schema_loading.remove(&conn);
                        self.schema_errors.insert(conn, "connection is gone".to_owned());
                    }
                }
            }
            DbEvent::ConnectFailed { conn, error } => {
                if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn) {
                    if matches!(c.state, ConnectionState::Connecting { .. }) {
                        c.state = ConnectionState::Error { message: error.to_string() };
                    }
                }
                self.schema_loading.remove(&conn);
            }
            DbEvent::QueryOk { run, result } => {
                self.finish_run(run, RunOutcome::Ok { results: vec![result], active: 0 });
            }
            DbEvent::BatchOk { run, results } => {
                self.finish_run(run, RunOutcome::Ok { results, active: 0 });
            }
            DbEvent::SchemaOk { conn, schema } => {
                self.schema_loading.remove(&conn);
                self.schema_errors.remove(&conn);
                self.schemas.insert(conn, schema);
            }
            DbEvent::SchemaErr { conn, error } => {
                self.schema_loading.remove(&conn);
                self.schema_errors.insert(conn, error.to_string());
            }
            DbEvent::QueryErr { run, error } => {
                let fatal = error.is_fatal();
                let message = error.to_string();
                let conn_id = self
                    .tabs
                    .iter()
                    .find(|t| matches!(&t.outcome, RunOutcome::Running { run: r, .. } if *r == run))
                    .map(|t| t.conn);
                self.finish_run(run, RunOutcome::Err(message.clone()));
                if fatal {
                    if let Some(id) = conn_id {
                        if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
                            if matches!(
                                c.state,
                                ConnectionState::Connected { .. }
                                    | ConnectionState::QueryRunning { .. }
                            ) {
                                c.state = ConnectionState::Error { message };
                            }
                        }
                    }
                }
            }
        }
    }

    /// The one staleness check. A run applies only if its tab is still
    /// Running that exact run id. Cancelled, replaced, and closed-tab runs
    /// land here and are silently dropped. A run that lands Ok replaces the
    /// displayed rows, so it clears the staged changes; a failure leaves them
    /// staged for a retry.
    fn finish_run(&mut self, run: RunId, outcome: RunOutcome) {
        let matched = match self
            .tabs
            .iter_mut()
            .find(|t| matches!(&t.outcome, RunOutcome::Running { run: r, .. } if *r == run))
        {
            Some(t) => {
                let ok = matches!(&outcome, RunOutcome::Ok { .. });
                let applied = t.pending_apply == Some(run);
                let refresh = applied && ok;
                if applied {
                    t.pending_apply = None;
                }
                let silent = t.silent_run == Some(run);
                if silent {
                    t.silent_run = None;
                }
                if ok {
                    t.edits.clear();
                    t.deleted_rows.clear();
                    t.added_rows.clear();
                }
                let (started, statements) = match &t.outcome {
                    RunOutcome::Running { started, statements, .. } => {
                        (*started, statements.clone())
                    }
                    _ => (Instant::now(), Vec::new()),
                };
                let conn = t.conn;
                let sql = t.sql.clone();
                let entries: Vec<HistoryEntry> = if silent {
                    Vec::new()
                } else {
                    match &outcome {
                        RunOutcome::Ok { results, .. } => results
                            .iter()
                            .enumerate()
                            .map(|(index, result)| HistoryEntry {
                                conn,
                                sql: statements
                                    .get(index)
                                    .cloned()
                                    .unwrap_or_else(|| sql.clone()),
                                succeeded: true,
                                rows: Some(
                                    result.affected().unwrap_or(result.row_count() as u64),
                                ),
                                took: result.took(),
                                at: now_unix_secs(),
                            })
                            .collect(),
                        RunOutcome::Err(_) => vec![HistoryEntry {
                            conn,
                            sql,
                            succeeded: false,
                            rows: None,
                            took: started.elapsed(),
                            at: now_unix_secs(),
                        }],
                        _ => Vec::new(),
                    }
                };
                let tab_id = t.id;
                t.outcome = outcome;
                t.clear_export_notice();
                Some((tab_id, conn, entries, refresh))
            }
            None => None,
        };
        let Some((tab_id, conn, entries, refresh)) = matched else { return };
        if !entries.is_empty() {
            self.history.splice(0..0, entries);
            self.history.truncate(HISTORY_CAP);
            self.persist_history();
        }
        let still_running = self
            .tabs
            .iter()
            .any(|t| t.conn == conn && matches!(t.outcome, RunOutcome::Running { .. }));
        if !still_running {
            if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn) {
                if let ConnectionState::QueryRunning { session, .. } = &c.state {
                    let session = session.clone();
                    c.state = ConnectionState::Connected { session };
                }
            }
        }
        if refresh {
            self.start_refresh(tab_id);
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            self.apply(event);
        }
    }

    fn persist(&self) {
        if let Err(e) = save_connections(&self.connections) {
            log::warn!("persist connections: {e}");
        }
    }

    fn persist_history(&self) {
        if let Err(e) = save_history(&self.history) {
            log::warn!("persist history: {e}");
        }
    }

    fn persist_saved(&self) {
        if let Err(e) = save_saved_queries(&self.saved) {
            log::warn!("persist saved queries: {e}");
        }
    }
}

impl eframe::App for AppState {
    fn logic(&mut self, _ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
    }
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        crate::ui::root(self, ui);
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedProfile {
    id: u64,
    name: String,
    #[serde(default)]
    env: Environment,
    config: ConnectionConfig,
}

fn profiles_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("crusty-data-explorer").join("connections.json")
}

fn load_connections() -> Vec<Connection> {
    let path = profiles_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_slice::<Vec<PersistedProfile>>(&bytes) {
        Ok(profiles) => profiles
            .into_iter()
            .map(|p| Connection {
                id: ConnectionId::from_raw(p.id),
                name: p.name,
                env: p.env,
                config: p.config,
                password: String::new(),
                state: ConnectionState::Disconnected,
            })
            .collect(),
        Err(e) => {
            log::warn!("parse {}: {e}", path.display());
            Vec::new()
        }
    }
}

fn save_connections(conns: &[Connection]) -> std::io::Result<()> {
    let path = profiles_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let profiles: Vec<PersistedProfile> = conns
        .iter()
        .map(|c| PersistedProfile {
            id: c.id.raw(),
            name: c.name.clone(),
            env: c.env,
            config: c.config.clone(),
        })
        .collect();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&profiles).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn history_path() -> std::path::PathBuf {
    profiles_path().with_file_name("history.json")
}

fn load_history() -> Vec<HistoryEntry> {
    let path = history_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_slice::<Vec<HistoryEntry>>(&bytes) {
        Ok(mut entries) => {
            entries.truncate(HISTORY_CAP);
            entries
        }
        Err(e) => {
            log::warn!("parse {}: {e}", path.display());
            Vec::new()
        }
    }
}

fn save_history(history: &[HistoryEntry]) -> std::io::Result<()> {
    let path = history_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(history).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn saved_queries_path() -> std::path::PathBuf {
    profiles_path().with_file_name("saved_queries.json")
}

fn load_saved_queries() -> Vec<SavedQuery> {
    let path = saved_queries_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_slice::<Vec<SavedQuery>>(&bytes) {
        Ok(mut queries) => {
            queries.retain(|q| !q.name.trim().is_empty());
            queries.truncate(SAVED_CAP);
            queries
        }
        Err(e) => {
            log::warn!("parse {}: {e}", path.display());
            Vec::new()
        }
    }
}

fn save_saved_queries(saved: &[SavedQuery]) -> std::io::Result<()> {
    let path = saved_queries_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(saved).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::PostgresSslMode;
    use crate::db::{ColumnInfo, DatabaseNode, SchemaNode, TableInfo, TableKind};

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_isolated_state(tag: &str, body: impl FnOnce(&mut AppState)) {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("cde-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crusty-data-explorer")).unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        {
            let ctx = eframe::egui::Context::default();
            let mut state = AppState::new(&ctx);
            for id in state.connections().iter().map(|c| c.id).collect::<Vec<_>>() {
                state.delete_connection(id);
            }
            body(&mut state);
        }
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn add_sqlite_connection(state: &mut AppState, name: &str) -> ConnectionId {
        add_sqlite_connection_env(state, name, Environment::Dev)
    }

    fn add_sqlite_connection_env(
        state: &mut AppState,
        name: &str,
        env: Environment,
    ) -> ConnectionId {
        state.open_draft(None);
        {
            let draft = state.draft_mut().expect("new draft");
            draft.name = name.to_owned();
            draft.env = env;
            draft.config = ConnectionConfig::Sqlite { path: ":memory:".into() };
        }
        state.save_draft();
        state.connections().last().map(|c| c.id).expect("saved connection")
    }

    fn add_postgres_connection(
        state: &mut AppState,
        name: &str,
        password: &str,
    ) -> ConnectionId {
        add_postgres_connection_env(state, name, password, Environment::Dev)
    }

    fn add_postgres_connection_env(
        state: &mut AppState,
        name: &str,
        password: &str,
        env: Environment,
    ) -> ConnectionId {
        state.open_draft(None);
        {
            let draft = state.draft_mut().expect("new draft");
            draft.name = name.to_owned();
            draft.env = env;
            draft.config = ConnectionConfig::Postgres {
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
            };
            draft.password = password.to_owned();
        }
        state.save_draft();
        state.connections().last().map(|c| c.id).expect("saved connection")
    }

    fn finish_ok(state: &mut AppState, conn: ConnectionId, sql: &str) {
        let tab = state.new_tab(conn);
        if let Some(slot) = state.tab_sql_mut(tab) {
            *slot = sql.to_owned();
        }
        let run = state.ids.run();
        state.set_outcome(
            tab,
            RunOutcome::Running {
                run,
                started: Instant::now(),
                statements: vec![sql.to_owned()],
            },
        );
        let result = QueryResult::from_columns(
            Vec::new(),
            false,
            None,
            std::time::Duration::from_millis(1),
        )
        .unwrap();
        state.finish_run(run, RunOutcome::Ok { results: vec![result], active: 0 });
    }

    fn rows_result(rows: i64) -> QueryResult {
        QueryResult::from_columns(
            vec![crate::db::Column::new(
                "n".to_owned(),
                "INTEGER".to_owned(),
                (0..rows).map(crate::db::Value::Integer).collect(),
            )],
            false,
            None,
            std::time::Duration::from_millis(2),
        )
        .unwrap()
    }

    fn affected_result(affected: u64) -> QueryResult {
        QueryResult::from_columns(
            Vec::new(),
            false,
            Some(affected),
            std::time::Duration::from_millis(1),
        )
        .unwrap()
    }

    fn start_running(state: &mut AppState, tab: TabId, statements: &[&str]) -> RunId {
        let run = state.ids.run();
        state.set_outcome(
            tab,
            RunOutcome::Running {
                run,
                started: Instant::now(),
                statements: statements.iter().map(|s| (*s).to_owned()).collect(),
            },
        );
        run
    }

    /// A connection whose worker channel is a scripted receiver, so a test can
    /// read the exact command `start_run` sends without a worker thread.
    fn scripted_connection(
        state: &mut AppState,
        name: &str,
    ) -> (ConnectionId, mpsc::Receiver<DbCommand>) {
        let id = add_sqlite_connection(state, name);
        let commands = attach_test_handle(state, id);
        (id, commands)
    }

    fn scripted_postgres_connection(
        state: &mut AppState,
        name: &str,
    ) -> (ConnectionId, mpsc::Receiver<DbCommand>) {
        let id = add_postgres_connection(state, name, "");
        let commands = attach_test_handle(state, id);
        (id, commands)
    }

    fn attach_test_handle(state: &mut AppState, id: ConnectionId) -> mpsc::Receiver<DbCommand> {
        let (handle, commands) = crate::db::test_handle();
        let conn = state.connections.iter_mut().find(|c| c.id == id).expect("connection");
        conn.state = ConnectionState::Connecting { handle };
        commands
    }

    /// A connected, idle connection whose worker channel is a scripted
    /// receiver.
    fn attach_test_session(state: &mut AppState, id: ConnectionId) -> mpsc::Receiver<DbCommand> {
        let (handle, commands) = crate::db::test_handle();
        let conn = state.connections.iter_mut().find(|c| c.id == id).expect("connection");
        conn.state = ConnectionState::Connected {
            session: LiveSession {
                handle,
                cancel: CancelHandle::new(|| {}),
                server_version: "test".to_owned(),
            },
        };
        commands
    }

    /// One table with a composite key and one editable column.
    fn items_schema() -> SchemaInfo {
        let columns = vec![
            ColumnInfo::new("tenant".to_owned(), "TEXT".to_owned(), false, Some(1)).unwrap(),
            ColumnInfo::new("id".to_owned(), "INTEGER".to_owned(), false, Some(2)).unwrap(),
            ColumnInfo::new("name".to_owned(), "TEXT".to_owned(), true, None).unwrap(),
        ];
        let table = TableInfo::new(
            "items".to_owned(),
            TableKind::Table,
            columns,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            false,
        )
        .unwrap();
        let node =
            SchemaNode::new("main".to_owned(), vec![table], Vec::new(), Vec::new(), Vec::new())
                .unwrap();
        let database = DatabaseNode::new(":memory:".to_owned(), vec![node]).unwrap();
        SchemaInfo::new(vec![database], false, false)
    }

    fn items_result() -> QueryResult {
        QueryResult::from_columns(
            vec![
                crate::db::Column::new(
                    "tenant".to_owned(),
                    "TEXT".to_owned(),
                    vec![
                        crate::db::Value::Text("acme".to_owned()),
                        crate::db::Value::Text("globex".to_owned()),
                    ],
                ),
                crate::db::Column::new(
                    "id".to_owned(),
                    "INTEGER".to_owned(),
                    vec![crate::db::Value::Integer(1), crate::db::Value::Integer(2)],
                ),
                crate::db::Column::new(
                    "name".to_owned(),
                    "TEXT".to_owned(),
                    vec![crate::db::Value::Text("ada".to_owned()), crate::db::Value::Null],
                ),
            ],
            false,
            None,
            std::time::Duration::from_millis(1),
        )
        .unwrap()
    }

    /// The items shape with a text key whose value prints as a number.
    fn numeric_text_key_result() -> QueryResult {
        QueryResult::from_columns(
            vec![
                crate::db::Column::new(
                    "tenant".to_owned(),
                    "TEXT".to_owned(),
                    vec![crate::db::Value::Text("010".to_owned())],
                ),
                crate::db::Column::new(
                    "id".to_owned(),
                    "INTEGER".to_owned(),
                    vec![crate::db::Value::Integer(1)],
                ),
                crate::db::Column::new(
                    "name".to_owned(),
                    "TEXT".to_owned(),
                    vec![crate::db::Value::Text("ada".to_owned())],
                ),
            ],
            false,
            None,
            std::time::Duration::from_millis(1),
        )
        .unwrap()
    }

    /// An idle scripted connection with the items schema loaded and one tab
    /// showing two items rows.
    fn editable_tab(state: &mut AppState) -> (TabId, ConnectionId, mpsc::Receiver<DbCommand>) {
        let conn = add_sqlite_connection(state, "mem-edit");
        let commands = attach_test_session(state, conn);
        state.apply(DbEvent::SchemaOk { conn, schema: items_schema() });
        let tab = state.new_tab(conn);
        let sql = "SELECT tenant, id, name FROM items";
        if let Some(slot) = state.tab_sql_mut(tab) {
            *slot = sql.to_owned();
        }
        let run = state.ids.run();
        state.set_outcome(
            tab,
            RunOutcome::Running {
                run,
                started: Instant::now(),
                statements: vec![sql.to_owned()],
            },
        );
        state.apply(DbEvent::BatchOk { run, results: vec![items_result()] });
        (tab, conn, commands)
    }

    /// Every text galley painted in one frame, for render assertions.
    fn rendered_text(output: &eframe::egui::FullOutput) -> Vec<String> {
        output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                eframe::egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn tab_sort_cycles_none_asc_desc() {
        with_isolated_state("tab-sort", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            let view = state.tab(tab).expect("tab");
            assert_eq!(view.sort_col(), None);
            assert_eq!(view.sort_dir(), SortDir::Asc);
            assert_eq!(view.filter(), "");
            state.tab_mut(tab).expect("tab").set_sort(0);
            assert_eq!(state.tab(tab).expect("tab").sort_col(), Some(0));
            assert_eq!(state.tab(tab).expect("tab").sort_dir(), SortDir::Asc);
            state.tab_mut(tab).expect("tab").set_sort(0);
            assert_eq!(state.tab(tab).expect("tab").sort_col(), Some(0));
            assert_eq!(state.tab(tab).expect("tab").sort_dir(), SortDir::Desc);
            state.tab_mut(tab).expect("tab").set_sort(0);
            assert_eq!(state.tab(tab).expect("tab").sort_col(), None);
            state.tab_mut(tab).expect("tab").set_sort(1);
            assert_eq!(state.tab(tab).expect("tab").sort_col(), Some(1));
            assert_eq!(state.tab(tab).expect("tab").sort_dir(), SortDir::Asc);
            state.tab_mut(tab).expect("tab").set_filter("ada".to_owned());
            assert_eq!(state.tab(tab).expect("tab").filter(), "ada");
        });
    }

    #[test]
    fn history_entry_shape_holds_sql_only() {
        let entry = HistoryEntry {
            conn: ConnectionId::from_raw(1),
            sql: "SELECT 1".to_owned(),
            succeeded: true,
            rows: Some(1),
            took: std::time::Duration::from_millis(7),
            at: 1_700_000_000,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("password"));
        assert!(json.contains("SELECT 1"));
    }

    #[test]
    fn finish_run_records_ok_and_err() {
        with_isolated_state("hist-rec", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1".to_owned();
            }
            let run = state.ids.run();
            state.set_outcome(
                tab,
                RunOutcome::Running {
                    run,
                    started: Instant::now(),
                    statements: vec!["SELECT 1".to_owned()],
                },
            );
            let result = QueryResult::from_columns(
                vec![crate::db::Column::new(
                    "n".to_owned(),
                    "INTEGER".to_owned(),
                    vec![
                        crate::db::Value::Integer(1),
                        crate::db::Value::Integer(2),
                    ],
                )],
                false,
                None,
                std::time::Duration::from_millis(42),
            )
            .unwrap();
            state.finish_run(run, RunOutcome::Ok { results: vec![result], active: 0 });
            assert_eq!(state.history().len(), 1);
            let entry = &state.history()[0];
            assert_eq!(entry.conn, conn);
            assert_eq!(entry.sql, "SELECT 1");
            assert!(entry.succeeded);
            assert_eq!(entry.rows, Some(2));
            assert_eq!(entry.took, std::time::Duration::from_millis(42));

            let affected_tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(affected_tab) {
                *slot = "DELETE FROM t".to_owned();
            }
            let affected_run = state.ids.run();
            state.set_outcome(
                affected_tab,
                RunOutcome::Running {
                    run: affected_run,
                    started: Instant::now(),
                    statements: vec!["DELETE FROM t".to_owned()],
                },
            );
            let affected = QueryResult::from_columns(
                Vec::new(),
                false,
                Some(3),
                std::time::Duration::from_millis(5),
            )
            .unwrap();
            state.finish_run(affected_run, RunOutcome::Ok { results: vec![affected], active: 0 });
            assert_eq!(state.history()[0].rows, Some(3));

            let err_tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(err_tab) {
                *slot = "SELECT bad".to_owned();
            }
            let err_run = state.ids.run();
            state.set_outcome(
                err_tab,
                RunOutcome::Running {
                    run: err_run,
                    started: Instant::now(),
                    statements: vec!["SELECT bad".to_owned()],
                },
            );
            state.finish_run(err_run, RunOutcome::Err("query failed".to_owned()));
            assert_eq!(state.history().len(), 3);
            let latest = &state.history()[0];
            assert_eq!(latest.sql, "SELECT bad");
            assert!(!latest.succeeded);
            assert_eq!(latest.rows, None);

            let before = state.history().len();
            state.finish_run(err_run, RunOutcome::Err("late duplicate".to_owned()));
            assert_eq!(state.history().len(), before);

            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.history().len(), state.history().len());
            assert_eq!(reloaded.history()[0].sql, "SELECT bad");
        });
    }

    #[test]
    fn batch_ok_lands_three_results_with_active_zero() {
        with_isolated_state("batch-ok", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1; SELECT 2; UPDATE t SET x = 1".to_owned();
            }
            let run =
                start_running(state, tab, &["SELECT 1", "SELECT 2", "UPDATE t SET x = 1"]);
            state.apply(DbEvent::BatchOk {
                run,
                results: vec![rows_result(1), rows_result(2), affected_result(1)],
            });
            let tab_view = state.tab(tab).expect("tab");
            match tab_view.outcome() {
                RunOutcome::Ok { results, active } => {
                    assert_eq!(results.len(), 3);
                    assert_eq!(*active, 0);
                }
                other => panic!("expected Ok, got {other:?}"),
            }
            assert_eq!(tab_view.results().len(), 3);
            assert_eq!(tab_view.active_result(), 0);
            assert_eq!(tab_view.active_query_result().map(|r| r.row_count()), Some(1));
            assert_eq!(state.history().len(), 3);
            assert_eq!(state.history()[0].sql, "SELECT 1");
            assert_eq!(state.history()[1].sql, "SELECT 2");
            assert_eq!(state.history()[2].sql, "UPDATE t SET x = 1");
            assert!(state.history().iter().all(|e| e.succeeded));
            assert_eq!(state.history()[2].rows, Some(1));

            // A late duplicate of the same run id stays dropped.
            state.apply(DbEvent::BatchOk { run, results: vec![rows_result(9)] });
            assert_eq!(state.history().len(), 3);
            assert_eq!(state.tab(tab).expect("tab").results().len(), 3);
        });
    }

    #[test]
    fn batch_ok_returns_connection_to_connected() {
        with_isolated_state("batch-conn", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            let run = start_running(state, tab, &["SELECT 1"]);
            let (handle, _commands) = crate::db::test_handle();
            let session = LiveSession {
                handle,
                cancel: CancelHandle::new(|| {}),
                server_version: "test".to_owned(),
            };
            if let Some(c) = state.connections.iter_mut().find(|c| c.id == conn) {
                c.state = ConnectionState::QueryRunning { session, run, tab };
            }
            state.apply(DbEvent::BatchOk { run, results: vec![rows_result(1)] });
            let conn = state.connections().iter().find(|c| c.id == conn).expect("conn");
            assert!(matches!(conn.state(), ConnectionState::Connected { .. }));
        });
    }

    #[test]
    fn batch_failure_keeps_the_first_result_and_names_the_index() {
        with_isolated_state("batch-fail", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            let run =
                start_running(state, tab, &["SELECT 1", "SELECT 2", "UPDATE t SET x = 1"]);
            state.apply(DbEvent::BatchOk {
                run,
                results: vec![rows_result(1), rows_result(2), affected_result(1)],
            });
            assert_eq!(state.tab(tab).expect("tab").results().len(), 3);

            let failed = start_running(state, tab, &["SELECT 1", "SELECT * FROM missing"]);
            state.apply(DbEvent::QueryErr {
                run: failed,
                error: crate::db::DbError::Query(
                    "statement 1: no such table: missing".to_owned(),
                ),
            });
            let tab = state.tab(tab).expect("tab");
            match tab.outcome() {
                RunOutcome::Err(message) => {
                    assert!(message.contains("statement 1"), "{message}");
                }
                other => panic!("expected Err, got {other:?}"),
            }
            // The first result of the last finished run stays on screen.
            assert_eq!(tab.results().len(), 3);
            assert_eq!(tab.active_result(), 0);
            assert!(tab.active_query_result().is_some());
            assert_eq!(state.history().len(), 4);
            assert!(!state.history()[0].succeeded);
        });
    }

    #[test]
    fn active_result_switches_and_clamps() {
        with_isolated_state("active-result", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            let run = start_running(state, tab, &["SELECT 1", "SELECT 2"]);
            state.apply(DbEvent::BatchOk { run, results: vec![rows_result(1), rows_result(2)] });
            state.tab_mut(tab).expect("tab").set_active_result(1);
            let tab_view = state.tab(tab).expect("tab");
            assert_eq!(tab_view.active_result(), 1);
            assert_eq!(tab_view.active_query_result().map(|r| r.row_count()), Some(2));
            match tab_view.outcome() {
                RunOutcome::Ok { active, .. } => assert_eq!(*active, 1),
                other => panic!("expected Ok, got {other:?}"),
            }
            state.tab_mut(tab).expect("tab").set_active_result(9);
            assert_eq!(state.tab(tab).expect("tab").active_result(), 1);
        });
    }

    #[test]
    fn scope_all_sends_every_statement() {
        with_isolated_state("scope-all", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            assert_eq!(state.tab(tab).expect("tab").run_scope(), RunScope::All);
            let sql = "SELECT 1; SELECT 2;";
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = sql.to_owned();
            }
            state.start_run(tab, sql, 0, None);
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["SELECT 1", "SELECT 2"]);
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
            assert!(matches!(
                state.tab(tab).expect("tab").outcome(),
                RunOutcome::Running { .. }
            ));
        });
    }

    #[test]
    fn scope_current_sends_only_the_statement_at_the_cursor() {
        with_isolated_state("scope-current", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.tab_mut(tab).expect("tab").set_run_scope(RunScope::Current);
            let sql = "SELECT 1;\nSELECT 2;\nSELECT 3";
            let cursor = sql.find("SELECT 2").expect("second statement") + 3;
            state.start_run(tab, sql, cursor, None);
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["SELECT 2"]);
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn scope_selection_sends_the_overlapping_statements() {
        with_isolated_state("scope-selection", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.tab_mut(tab).expect("tab").set_run_scope(RunScope::Selection);
            let sql = "SELECT 1;\nSELECT 2;\nSELECT 3";
            let start = sql.find("2;").expect("second statement");
            let end = sql.find("SELECT 3").expect("third statement") + 3;
            state.start_run(tab, sql, start, Some(start..end));
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["SELECT 2", "SELECT 3"]);
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }

            // No selection falls back to the statement at the cursor.
            let fallback = state.new_tab(conn);
            state.tab_mut(fallback).expect("tab").set_run_scope(RunScope::Selection);
            state.start_run(fallback, sql, 12, None);
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["SELECT 2"]);
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn empty_effective_set_errors_without_sending() {
        with_isolated_state("scope-empty", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.start_run(tab, "   ", 0, None);
            assert!(matches!(
                state.tab(tab).expect("tab").outcome(),
                RunOutcome::Err(message) if message.as_str() == "nothing to run"
            ));
            assert!(commands.try_recv().is_err());

            let current = state.new_tab(conn);
            state.tab_mut(current).expect("tab").set_run_scope(RunScope::Current);
            state.start_run(current, "", 0, None);
            assert!(matches!(
                state.tab(current).expect("tab").outcome(),
                RunOutcome::Err(message) if message.as_str() == "nothing to run"
            ));
            assert!(commands.try_recv().is_err());
        });
    }

    #[test]
    fn explain_sends_each_scope_prefixed_for_sqlite() {
        with_isolated_state("explain-scope", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            let sql = "SELECT 1;\nSELECT 2;\nSELECT 3";

            state.start_explain(tab, sql, 0, None);
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(
                        statements,
                        vec![
                            "EXPLAIN QUERY PLAN SELECT 1",
                            "EXPLAIN QUERY PLAN SELECT 2",
                            "EXPLAIN QUERY PLAN SELECT 3",
                        ]
                    );
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
            assert!(matches!(
                state.tab(tab).expect("tab").outcome(),
                RunOutcome::Running { .. }
            ));

            let current = state.new_tab(conn);
            state.tab_mut(current).expect("tab").set_run_scope(RunScope::Current);
            let cursor = sql.find("SELECT 2").expect("second statement") + 3;
            state.start_explain(current, sql, cursor, None);
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["EXPLAIN QUERY PLAN SELECT 2"]);
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }

            let selection = state.new_tab(conn);
            state.tab_mut(selection).expect("tab").set_run_scope(RunScope::Selection);
            let start = sql.find("2;").expect("second statement");
            let end = sql.find("SELECT 3").expect("third statement") + 3;
            state.start_explain(selection, sql, start, Some(start..end));
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(
                        statements,
                        vec!["EXPLAIN QUERY PLAN SELECT 2", "EXPLAIN QUERY PLAN SELECT 3"]
                    );
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn explain_uses_the_postgres_prefix_for_a_postgres_connection() {
        with_isolated_state("explain-pg", |state| {
            let (conn, commands) = scripted_postgres_connection(state, "pg");
            let tab = state.new_tab(conn);
            state.start_explain(tab, "SELECT 1", 0, None);
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["EXPLAIN SELECT 1"]);
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn explain_empty_effective_set_errors_without_sending() {
        with_isolated_state("explain-empty", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.start_explain(tab, "   ", 0, None);
            assert!(matches!(
                state.tab(tab).expect("tab").outcome(),
                RunOutcome::Err(message) if message.as_str() == "nothing to run"
            ));
            assert!(commands.try_recv().is_err());
        });
    }

    #[test]
    fn all_scope_runs_a_script_through_the_sqlite_worker() {
        with_isolated_state("batch-live", |state| {
            let conn = add_sqlite_connection(state, "mem");
            state.connect(conn);
            let tab = state.new_tab(conn);
            let sql = "CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1); SELECT id FROM t";
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = sql.to_owned();
            }
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            loop {
                state.drain_events();
                if matches!(state.connections()[0].state(), ConnectionState::Connected { .. }) {
                    break;
                }
                assert!(Instant::now() < deadline, "connection did not come up");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            state.start_run(tab, sql, 0, None);
            loop {
                state.drain_events();
                let running = matches!(
                    state.tab(tab).expect("tab").outcome(),
                    RunOutcome::Running { .. }
                );
                if !running {
                    break;
                }
                assert!(Instant::now() < deadline, "run did not finish");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let tab = state.tab(tab).expect("tab");
            match tab.outcome() {
                RunOutcome::Ok { results, active } => {
                    assert_eq!(results.len(), 3);
                    assert_eq!(*active, 0);
                    assert_eq!(results[0].affected(), Some(0));
                    assert_eq!(results[1].affected(), Some(1));
                    assert_eq!(results[2].row_count(), 1);
                    assert_eq!(results[2].cell(0, 0), Some(&crate::db::Value::Integer(1)));
                }
                other => panic!("expected Ok, got {other:?}"),
            }
            assert_eq!(state.history().len(), 3);
            assert!(matches!(
                state.connections()[0].state(),
                ConnectionState::Connected { .. }
            ));
        });
    }

    #[test]
    fn grid_and_toolbar_render_multi_result_state() {
        with_isolated_state("render", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1; SELECT 2".to_owned();
            }
            let run = start_running(state, tab, &["SELECT 1", "SELECT 2"]);
            state.apply(DbEvent::BatchOk { run, results: vec![rows_result(2), rows_result(3)] });
            state.activate(tab);
            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let mut grid = ctx.run_ui(input.clone(), |ui| {
                crate::ui::grid::results(state, ui);
            });
            grid.textures_delta.clear();
            let grid_text = rendered_text(&grid);
            assert!(grid_text.iter().any(|t| t == "Result 1"), "{grid_text:?}");
            assert!(grid_text.iter().any(|t| t == "Result 2"), "{grid_text:?}");
            assert!(grid_text.iter().any(|t| t == "2 rows · 2 ms"), "{grid_text:?}");

            let mut toolbar = ctx.run_ui(input.clone(), |ui| {
                crate::ui::query::editor(state, ui);
            });
            toolbar.textures_delta.clear();
            let toolbar_text = rendered_text(&toolbar);
            assert!(toolbar_text.iter().any(|t| t == "▶ Run"), "{toolbar_text:?}");
            assert!(toolbar_text.iter().any(|t| t == "Explain"), "{toolbar_text:?}");
            assert!(
                toolbar_text.iter().any(|t| t.contains("all statements")),
                "{toolbar_text:?}"
            );
            assert_eq!(state.tab(tab).expect("tab").active_result(), 0);

            // Switching the active result re-renders the grid for that result.
            state.tab_mut(tab).expect("tab").set_active_result(1);
            let mut switched = ctx.run_ui(input, |ui| {
                crate::ui::grid::results(state, ui);
            });
            switched.textures_delta.clear();
            let switched_text = rendered_text(&switched);
            assert!(switched_text.iter().any(|t| t == "3 rows · 2 ms"), "{switched_text:?}");
            assert!(switched_text.iter().any(|t| t == "3 of 3 rows"), "{switched_text:?}");
            assert!(!switched_text.iter().any(|t| t == "2 rows · 2 ms"), "{switched_text:?}");
        });
    }

    #[test]
    fn history_caps_at_200_newest_first() {
        with_isolated_state("hist-cap", |state| {
            let conn = add_sqlite_connection(state, "mem");
            for i in 0..250 {
                finish_ok(state, conn, &format!("SELECT {i}"));
            }
            assert_eq!(state.history().len(), 200);
            assert_eq!(state.history()[0].sql, "SELECT 249");
            assert_eq!(state.history()[199].sql, "SELECT 50");
        });
    }

    #[test]
    fn history_for_filters_by_connection() {
        with_isolated_state("hist-filter", |state| {
            let a = add_sqlite_connection(state, "a");
            let b = add_sqlite_connection(state, "b");
            finish_ok(state, a, "SELECT a");
            finish_ok(state, b, "SELECT b");
            let only_a = state.history_for(a);
            assert_eq!(only_a.len(), 1);
            assert_eq!(only_a[0].sql, "SELECT a");
            let only_b = state.history_for(b);
            assert_eq!(only_b.len(), 1);
            assert_eq!(only_b[0].sql, "SELECT b");
            state.open_history(a, "SELECT a");
            let tab = state.active().and_then(|id| state.tab(id)).expect("active tab");
            assert_eq!(tab.sql, "SELECT a");
            assert!(matches!(tab.outcome(), RunOutcome::Never));
            state.clear_history();
            assert!(state.history().is_empty());
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert!(reloaded.history().is_empty());
        });
    }

    #[test]
    fn corrupt_history_file_falls_back_to_empty() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir =
            std::env::temp_dir().join(format!("cde-hist-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crusty-data-explorer")).unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        std::fs::write(
            dir.join("crusty-data-explorer").join("history.json"),
            b"{oops",
        )
        .unwrap();
        let ctx = eframe::egui::Context::default();
        let state = AppState::new(&ctx);
        assert!(state.history().is_empty());
        let entries: Vec<HistoryEntry> = (0..250)
            .map(|i| HistoryEntry {
                conn: ConnectionId::from_raw(1),
                sql: format!("SELECT {i}"),
                succeeded: true,
                rows: Some(i),
                took: std::time::Duration::from_millis(1),
                at: i,
            })
            .collect();
        std::fs::write(
            dir.join("crusty-data-explorer").join("history.json"),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        let loaded = AppState::new(&ctx);
        assert_eq!(loaded.history().len(), 200);
        assert_eq!(loaded.history()[0].sql, "SELECT 0");
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_query_constructor_rejects_empty_names() {
        let conn = ConnectionId::from_raw(1);
        assert!(SavedQuery::new(1, String::new(), "SELECT 1".to_owned(), conn).is_err());
        assert!(SavedQuery::new(1, "   ".to_owned(), "SELECT 1".to_owned(), conn).is_err());
        let query =
            SavedQuery::new(1, "daily".to_owned(), "SELECT 1".to_owned(), conn).unwrap();
        assert_eq!(query.id, 1);
        assert_eq!(query.name, "daily");
        let json = serde_json::to_string(&query).unwrap();
        assert!(!json.contains("password"));
        assert!(json.contains("SELECT 1"));
    }

    #[test]
    fn save_duplicate_open_rename_delete_roundtrip() {
        with_isolated_state("saved-round", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1".to_owned();
            }
            state.activate(tab);
            assert!(state.save_active_tab("   ").is_none());
            assert!(state.saved_queries().is_empty());
            let id = state.save_active_tab("first").expect("saved");
            assert_eq!(state.saved_queries().len(), 1);
            assert_eq!(state.saved_queries()[0].sql, "SELECT 1");
            assert_eq!(state.saved_queries()[0].conn, conn);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 2".to_owned();
            }
            let again = state.save_active_tab("first").expect("replaced");
            assert_eq!(id, again);
            assert_eq!(state.saved_queries().len(), 1);
            assert_eq!(state.saved_queries()[0].sql, "SELECT 2");
            state.open_saved(id);
            let opened = state.active().and_then(|t| state.tab(t)).expect("active tab");
            assert_eq!(opened.sql, "SELECT 2");
            assert!(matches!(opened.outcome(), RunOutcome::Never));
            state.open_saved(999_999);
            let still = state.active().and_then(|t| state.tab(t)).expect("active tab");
            assert_eq!(still.sql, "SELECT 2");
            assert!(!state.rename_saved(id, "   "));
            assert_eq!(state.saved_queries()[0].name, "first");
            assert!(state.rename_saved(id, "renamed"));
            assert_eq!(state.saved_queries()[0].name, "renamed");
            assert!(!state.rename_saved(999_999, "nope"));
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.saved_queries().len(), 1);
            assert_eq!(reloaded.saved_queries()[0].name, "renamed");
            assert_eq!(reloaded.saved_queries()[0].sql, "SELECT 2");
            assert!(state.delete_saved(id));
            assert!(state.saved_queries().is_empty());
            assert!(!state.delete_saved(id));
            let emptied = AppState::new(&eframe::egui::Context::default());
            assert!(emptied.saved_queries().is_empty());
        });
    }

    #[test]
    fn saved_queries_cap_at_200_newest_first() {
        with_isolated_state("saved-cap", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.activate(tab);
            for i in 0..250 {
                if let Some(slot) = state.tab_sql_mut(tab) {
                    *slot = format!("SELECT {i}");
                }
                state.save_active_tab(&format!("q{i}")).expect("saved");
            }
            assert_eq!(state.saved_queries().len(), 200);
            assert_eq!(state.saved_queries()[0].name, "q249");
            assert_eq!(state.saved_queries()[199].name, "q50");
        });
    }

    #[test]
    fn corrupt_saved_file_falls_back_to_empty() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir =
            std::env::temp_dir().join(format!("cde-saved-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crusty-data-explorer")).unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        std::fs::write(
            dir.join("crusty-data-explorer").join("saved_queries.json"),
            b"{oops",
        )
        .unwrap();
        let ctx = eframe::egui::Context::default();
        let state = AppState::new(&ctx);
        assert!(state.saved_queries().is_empty());
        let entries: Vec<SavedQuery> = (0..250)
            .map(|i| SavedQuery {
                id: i,
                name: format!("q{i}"),
                sql: format!("SELECT {i}"),
                conn: ConnectionId::from_raw(1),
            })
            .collect();
        std::fs::write(
            dir.join("crusty-data-explorer").join("saved_queries.json"),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        let loaded = AppState::new(&ctx);
        assert_eq!(loaded.saved_queries().len(), 200);
        assert_eq!(loaded.saved_queries()[0].name, "q0");
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persisted_profile_shape_has_no_password() {
        let profile = PersistedProfile {
            id: 1,
            name: "pg".to_owned(),
            env: Environment::Prod,
            config: ConnectionConfig::Postgres {
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
            },
        };
        let json = serde_json::to_string(&profile).unwrap();
        assert!(!json.contains("password"));
        assert!(json.contains("\"kind\":\"postgres\""));
    }

    #[test]
    fn save_and_load_never_persist_password() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("cde-pw-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crusty-data-explorer")).unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        let conns = vec![Connection {
            id: ConnectionId::from_raw(7),
            name: "pg".to_owned(),
            env: Environment::Test,
            config: ConnectionConfig::Postgres {
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
            },
            password: "transient-only".to_owned(),
            state: ConnectionState::Disconnected,
        }];
        let result = save_connections(&conns);
        let raw = std::fs::read_to_string(dir.join("crusty-data-explorer").join("connections.json"))
            .unwrap_or_default();
        let loaded = load_connections();
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        result.unwrap();
        assert!(!raw.contains("transient-only"));
        assert!(!raw.contains("password"));
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].password.is_empty());
        assert_eq!(loaded[0].name, "pg");
        assert_eq!(loaded[0].env, Environment::Test);
    }

    #[test]
    fn edit_draft_restores_and_updates_connection() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("cde-edit-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crusty-data-explorer")).unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        let ctx = eframe::egui::Context::default();
        let mut state = AppState::new(&ctx);
        for id in state.connections().iter().map(|c| c.id).collect::<Vec<_>>() {
            state.delete_connection(id);
        }
        state.open_draft(None);
        {
            let draft = state.draft_mut().expect("new draft");
            draft.name = "pg".to_owned();
            draft.config = ConnectionConfig::Postgres {
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
            };
            draft.password = "first".to_owned();
        }
        state.save_draft();
        assert!(state.draft().is_none());
        assert_eq!(state.connections().len(), 1);
        let id = state.connections()[0].id;
        state.open_draft(Some(id));
        {
            let draft = state.draft().expect("edit draft");
            assert_eq!(draft.editing, Some(id));
            assert_eq!(draft.name, "pg");
            assert_eq!(
                draft.config,
                ConnectionConfig::Postgres {
                    host: "localhost".to_owned(),
                    port: 5432,
                    database: "appdb".to_owned(),
                    user: "owner".to_owned(),
                    ssl_mode: PostgresSslMode::Disable,
                }
            );
            assert_eq!(draft.password, "first");
        }
        {
            let draft = state.draft_mut().expect("edit draft mut");
            draft.name = "pg-renamed".to_owned();
            draft.config = ConnectionConfig::Postgres {
                host: "db.internal".to_owned(),
                port: 5433,
                database: "analytics".to_owned(),
                user: "reader".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
            };
            draft.password = "second".to_owned();
        }
        state.save_draft();
        assert!(state.draft().is_none());
        assert_eq!(state.connections().len(), 1);
        let conn = &state.connections()[0];
        assert_eq!(conn.id, id);
        assert_eq!(conn.name, "pg-renamed");
        assert_eq!(
            conn.config,
            ConnectionConfig::Postgres {
                host: "db.internal".to_owned(),
                port: 5433,
                database: "analytics".to_owned(),
                user: "reader".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
            }
        );
        assert_eq!(conn.password, "second");
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_opens_only_for_postgres_with_empty_password() {
        with_isolated_state("pw-open", |state| {
            let sqlite = add_sqlite_connection(state, "mem");
            state.open_password_prompt(sqlite);
            assert_eq!(state.password_prompt(), None);
            assert!(state.prompt_password().is_empty());

            let stored = add_postgres_connection(state, "pg-stored", "secret");
            state.open_password_prompt(stored);
            assert_eq!(state.password_prompt(), None);
            assert!(state.prompt_password().is_empty());

            let empty = add_postgres_connection(state, "pg-empty", "");
            state.open_password_prompt(empty);
            assert_eq!(state.password_prompt(), Some(empty));
            assert_eq!(state.prompt_password(), "");
            let conn = state.connections().iter().find(|c| c.id == empty).expect("pg");
            assert!(matches!(conn.state(), ConnectionState::Disconnected));
        });
    }

    #[test]
    fn submit_stores_and_clears() {
        with_isolated_state("pw-submit", |state| {
            let pg = add_postgres_connection(state, "pg", "");
            state.open_password_prompt(pg);
            assert_eq!(state.password_prompt(), Some(pg));
            state.prompt_password_mut().push_str("s3cret");
            state.submit_password();
            assert_eq!(state.password_prompt(), None);
            assert!(state.prompt_password().is_empty());
            let conn = state.connections().iter().find(|c| c.id == pg).expect("pg");
            assert_eq!(conn.password, "s3cret");
            assert!(matches!(conn.state(), ConnectionState::Connecting { .. }));
        });
    }

    #[test]
    fn cancel_clears_without_storing() {
        with_isolated_state("pw-cancel", |state| {
            let pg = add_postgres_connection(state, "pg", "");
            state.open_password_prompt(pg);
            state.prompt_password_mut().push_str("typed");
            state.cancel_password_prompt();
            assert_eq!(state.password_prompt(), None);
            assert!(state.prompt_password().is_empty());
            let conn = state.connections().iter().find(|c| c.id == pg).expect("pg");
            assert!(conn.password.is_empty());
            assert!(matches!(conn.state(), ConnectionState::Disconnected));
        });
    }

    #[test]
    fn sqlite_connects_directly_without_prompt() {
        with_isolated_state("pw-sqlite", |state| {
            let sqlite = add_sqlite_connection(state, "mem");
            state.open_password_prompt(sqlite);
            assert_eq!(state.password_prompt(), None);
            let conn =
                state.connections().iter().find(|c| c.id == sqlite).expect("sqlite");
            assert!(matches!(conn.state(), ConnectionState::Connecting { .. }));
        });
    }

    #[test]
    fn environment_serde_labels_and_defaults() {
        assert_eq!(Environment::default(), Environment::Dev);
        assert_eq!(Environment::Dev.label(), "Dev");
        assert_eq!(Environment::Test.label(), "Test");
        assert_eq!(Environment::Prod.label(), "Prod");
        assert_eq!(serde_json::to_string(&Environment::Dev).unwrap(), "\"dev\"");
        assert_eq!(serde_json::to_string(&Environment::Test).unwrap(), "\"test\"");
        assert_eq!(serde_json::to_string(&Environment::Prod).unwrap(), "\"prod\"");
        assert_ne!(Environment::Dev.badge_color(), Environment::Test.badge_color());
        assert_ne!(Environment::Test.badge_color(), Environment::Prod.badge_color());
        assert_ne!(Environment::Dev.badge_color(), Environment::Prod.badge_color());
    }

    #[test]
    fn old_json_without_env_loads_as_dev() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("cde-env-legacy-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crusty-data-explorer")).unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        std::fs::write(
            dir.join("crusty-data-explorer").join("connections.json"),
            br#"[{"id":3,"name":"legacy","config":{"kind":"sqlite","path":":memory:"}}]"#,
        )
        .unwrap();
        let state = AppState::new(&eframe::egui::Context::default());
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(state.connections().len(), 1);
        assert_eq!(state.connections()[0].name, "legacy");
        assert_eq!(state.connections()[0].env, Environment::Dev);
    }

    #[test]
    fn env_roundtrips_through_save_and_load() {
        with_isolated_state("env-round", |state| {
            let id = add_sqlite_connection(state, "db");
            state.open_draft(Some(id));
            {
                let draft = state.draft_mut().expect("edit draft");
                assert_eq!(draft.env, Environment::Dev);
                draft.env = Environment::Test;
            }
            state.save_draft();
            assert_eq!(state.connections()[0].env, Environment::Test);
            let raw = std::fs::read_to_string(profiles_path()).expect("profiles file");
            let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(json[0]["env"], serde_json::json!("test"));
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.connections().len(), 1);
            assert_eq!(reloaded.connections()[0].env, Environment::Test);
        });
    }

    #[test]
    fn prod_connect_arms_confirmation_without_spawning() {
        with_isolated_state("prod-arm", |state| {
            let prod = add_sqlite_connection_env(state, "prod-db", Environment::Prod);
            state.connect(prod);
            assert_eq!(state.pending_prod(), Some(prod));
            assert_eq!(state.password_prompt(), None);
            let conn = state.connections().iter().find(|c| c.id == prod).expect("conn");
            assert!(matches!(conn.state(), ConnectionState::Disconnected));
        });
    }

    #[test]
    fn prod_confirm_connects_and_clears() {
        with_isolated_state("prod-confirm", |state| {
            let prod = add_sqlite_connection_env(state, "prod-db", Environment::Prod);
            state.connect(prod);
            assert_eq!(state.pending_prod(), Some(prod));
            state.confirm_prod();
            assert_eq!(state.pending_prod(), None);
            let conn = state.connections().iter().find(|c| c.id == prod).expect("conn");
            assert!(matches!(conn.state(), ConnectionState::Connecting { .. }));
        });
    }

    #[test]
    fn prod_confirm_postgres_prompts_then_connects() {
        with_isolated_state("prod-pg", |state| {
            let prod = add_postgres_connection_env(state, "prod-pg", "", Environment::Prod);
            state.open_password_prompt(prod);
            assert_eq!(state.password_prompt(), None);
            assert_eq!(state.pending_prod(), Some(prod));
            {
                let conn =
                    state.connections().iter().find(|c| c.id == prod).expect("conn");
                assert!(matches!(conn.state(), ConnectionState::Disconnected));
            }
            state.confirm_prod();
            assert_eq!(state.pending_prod(), None);
            assert_eq!(state.password_prompt(), Some(prod));
            {
                let conn =
                    state.connections().iter().find(|c| c.id == prod).expect("conn");
                assert!(matches!(conn.state(), ConnectionState::Disconnected));
            }
            state.prompt_password_mut().push_str("s3cret");
            state.submit_password();
            assert_eq!(state.password_prompt(), None);
            let conn = state.connections().iter().find(|c| c.id == prod).expect("conn");
            assert_eq!(conn.password, "s3cret");
            assert!(matches!(conn.state(), ConnectionState::Connecting { .. }));
        });
    }

    #[test]
    fn prod_cancel_clears_without_connecting() {
        with_isolated_state("prod-cancel", |state| {
            let prod = add_sqlite_connection_env(state, "prod-db", Environment::Prod);
            state.connect(prod);
            state.cancel_prod();
            assert_eq!(state.pending_prod(), None);
            let conn = state.connections().iter().find(|c| c.id == prod).expect("conn");
            assert!(matches!(conn.state(), ConnectionState::Disconnected));
        });
    }

    #[test]
    fn dev_and_test_envs_skip_the_confirmation() {
        with_isolated_state("env-direct", |state| {
            for env in [Environment::Dev, Environment::Test] {
                let id = add_sqlite_connection_env(state, env.label(), env);
                state.open_password_prompt(id);
                assert_eq!(state.pending_prod(), None);
                let conn = state.connections().iter().find(|c| c.id == id).expect("conn");
                assert_eq!(conn.env, env);
                assert!(matches!(conn.state(), ConnectionState::Connecting { .. }));
            }
        });
    }

    #[test]
    fn edit_target_resolves_for_a_simple_select_and_not_for_a_join() {
        with_isolated_state("edit-target", |state| {
            let (tab, _conn, commands) = editable_tab(state);
            let target = state.edit_target(tab).expect("target");
            assert_eq!(target.table, "items");
            assert_eq!(
                target.pk_cols,
                vec![("tenant".to_owned(), 0), ("id".to_owned(), 1)]
            );
            assert_eq!(target.pk_types, vec![true, false]);

            // The tree's preview form resolves through the schema name.
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT * FROM \"main\".\"items\" LIMIT 100".to_owned();
            }
            let qualified = state.edit_target(tab).expect("qualified target");
            assert_eq!(qualified.table, "main.items");
            assert_eq!(
                qualified.pk_cols,
                vec![("tenant".to_owned(), 0), ("id".to_owned(), 1)]
            );
            assert_eq!(qualified.pk_types, vec![true, false]);
            // A qualifier naming no loaded schema does not resolve.
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT * FROM \"other\".\"items\"".to_owned();
            }
            assert_eq!(state.edit_target(tab), None);

            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT i.id FROM items i JOIN items o ON o.id = i.id".to_owned();
            }
            assert_eq!(state.edit_target(tab), None);
            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            assert!(state.tab(tab).expect("tab").edits().is_empty());
            state.apply_edits(tab);
            assert!(commands.try_recv().is_err());

            // A run in flight withdraws editing until it lands.
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT tenant, id, name FROM items".to_owned();
            }
            let sql = state.tab(tab).expect("tab").sql.clone();
            state.start_run(tab, &sql, 0, None);
            assert!(matches!(
                state.tab(tab).expect("tab").outcome(),
                RunOutcome::Running { .. }
            ));
            assert_eq!(state.edit_target(tab), None);
            let run = match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { run, .. } => run,
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            state.apply(DbEvent::BatchOk { run, results: vec![items_result()] });
            assert!(state.edit_target(tab).is_some());
        });
    }

    #[test]
    fn stage_and_discard_roundtrip() {
        with_isolated_state("edit-stage", |state| {
            let (tab, _conn, _commands) = editable_tab(state);
            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            state.stage_edit(tab, 1, 2, "grace".to_owned());
            state.stage_edit(tab, 1, 2, "Grace".to_owned());
            let view = state.tab(tab).expect("tab");
            assert_eq!(view.staged_count(), 2);
            assert_eq!(view.edit_text(0, 2), Some("Ada"));
            assert_eq!(view.edit_text(1, 2), Some("Grace"));
            assert_eq!(view.edits().len(), 2);
            assert_eq!(view.edits()[0], RowEdit { row: 0, col: 2, text: "Ada".to_owned() });

            state.mark_row_deleted(tab, 0);
            assert!(state.tab(tab).expect("tab").is_row_deleted(0));
            state.mark_row_deleted(tab, 0);
            assert!(!state.tab(tab).expect("tab").is_row_deleted(0));

            state.add_row(tab);
            let view = state.tab(tab).expect("tab");
            assert_eq!(
                view.added_rows(),
                &[vec![String::new(), String::new(), String::new()]]
            );
            assert_eq!(view.staged_count(), 3);
            state.stage_edit(tab, 2, 1, "7".to_owned());
            assert_eq!(
                state.tab(tab).expect("tab").added_rows(),
                &[vec![String::new(), "7".to_owned(), String::new()]]
            );
            // Out-of-range rows and columns stage nothing.
            state.stage_edit(tab, 9, 0, "x".to_owned());
            state.stage_edit(tab, 0, 9, "x".to_owned());
            assert_eq!(state.tab(tab).expect("tab").staged_count(), 3);

            state.discard_edits(tab);
            let view = state.tab(tab).expect("tab");
            assert_eq!(view.staged_count(), 0);
            assert!(view.edits().is_empty());
            assert!(!view.is_row_deleted(0));
            assert!(view.added_rows().is_empty());
        });
    }

    #[test]
    fn apply_sends_update_delete_and_insert_as_one_batch() {
        with_isolated_state("edit-apply", |state| {
            let (tab, _conn, commands) = editable_tab(state);
            state.stage_edit(tab, 0, 2, "ada lovelace".to_owned());
            state.mark_row_deleted(tab, 1);
            state.add_row(tab);
            state.stage_edit(tab, 2, 0, "initech".to_owned());
            state.stage_edit(tab, 2, 1, "3".to_owned());
            state.stage_edit(tab, 2, 2, "Lin".to_owned());
            state.apply_edits(tab);
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(
                        statements,
                        vec![
                            "UPDATE \"items\" SET \"name\" = 'ada lovelace' WHERE \"tenant\" = 'acme' AND \"id\" = 1",
                            "DELETE FROM \"items\" WHERE \"tenant\" = 'globex' AND \"id\" = 2",
                            "INSERT INTO \"items\" (\"tenant\", \"id\", \"name\") VALUES ('initech', 3, 'Lin')",
                        ]
                    );
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
            assert!(matches!(
                state.tab(tab).expect("tab").outcome(),
                RunOutcome::Running { .. }
            ));
            assert!(commands.try_recv().is_err());
        });
    }

    #[test]
    fn a_text_key_that_prints_as_a_number_stays_quoted_in_where() {
        with_isolated_state("edit-text-pk", |state| {
            let conn = add_sqlite_connection(state, "mem-text-pk");
            let commands = attach_test_session(state, conn);
            state.apply(DbEvent::SchemaOk { conn, schema: items_schema() });
            let tab = state.new_tab(conn);
            let sql = "SELECT tenant, id, name FROM items";
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = sql.to_owned();
            }
            let run = state.ids.run();
            state.set_outcome(
                tab,
                RunOutcome::Running {
                    run,
                    started: Instant::now(),
                    statements: vec![sql.to_owned()],
                },
            );
            state.apply(DbEvent::BatchOk {
                run,
                results: vec![numeric_text_key_result()],
            });
            let target = state.edit_target(tab).expect("target");
            assert_eq!(target.pk_types, vec![true, false]);
            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            state.apply_edits(tab);
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => assert_eq!(
                    statements,
                    vec![
                        "UPDATE \"items\" SET \"name\" = 'Ada' WHERE \"tenant\" = '010' AND \"id\" = 1"
                    ]
                ),
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn apply_with_no_staged_changes_sends_nothing() {
        with_isolated_state("edit-empty", |state| {
            let (tab, _conn, commands) = editable_tab(state);
            state.apply_edits(tab);
            assert!(commands.try_recv().is_err());
            assert!(matches!(state.tab(tab).expect("tab").outcome(), RunOutcome::Ok { .. }));

            // A target that does not resolve no-ops every transition.
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1".to_owned();
            }
            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            state.mark_row_deleted(tab, 0);
            state.add_row(tab);
            state.apply_edits(tab);
            assert!(commands.try_recv().is_err());
            let view = state.tab(tab).expect("tab");
            assert_eq!(view.staged_count(), 0);
            assert!(matches!(view.outcome(), RunOutcome::Ok { .. }));
        });
    }

    #[test]
    fn a_landed_apply_refreshes_the_tab_once_without_a_history_entry() {
        with_isolated_state("edit-refresh", |state| {
            let (tab, _conn, commands) = editable_tab(state);
            let sql = state.tab(tab).expect("tab").sql.clone();
            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            state.apply_edits(tab);
            let apply_run = match commands.try_recv().expect("apply batch") {
                DbCommand::QueryBatch { run, statements } => {
                    assert_eq!(statements.len(), 1);
                    run
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            let before = state.history().len();
            state.apply(DbEvent::BatchOk { run: apply_run, results: vec![affected_result(1)] });
            assert_eq!(state.history().len(), before + 1);
            assert_eq!(state.tab(tab).expect("tab").staged_count(), 0);

            let refresh_run = match commands.try_recv().expect("refresh batch") {
                DbCommand::QueryBatch { run, statements } => {
                    assert_eq!(statements, vec![sql]);
                    run
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            assert!(matches!(
                state.tab(tab).expect("tab").outcome(),
                RunOutcome::Running { .. }
            ));
            state.apply(DbEvent::BatchOk { run: refresh_run, results: vec![items_result()] });
            assert_eq!(state.history().len(), before + 1);
            let view = state.tab(tab).expect("tab");
            assert_eq!(view.active_query_result().map(|r| r.row_count()), Some(2));
            assert_eq!(view.staged_count(), 0);
            assert!(matches!(view.outcome(), RunOutcome::Ok { .. }));
            assert!(commands.try_recv().is_err());

            // A failed apply keeps the staged changes for a retry.
            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            state.apply_edits(tab);
            let failed_run = match commands.try_recv().expect("apply batch") {
                DbCommand::QueryBatch { run, .. } => run,
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            state.apply(DbEvent::QueryErr {
                run: failed_run,
                error: crate::db::DbError::Query("statement 0: nope".to_owned()),
            });
            assert_eq!(state.tab(tab).expect("tab").staged_count(), 1);
            assert!(commands.try_recv().is_err());
        });
    }

    #[test]
    fn a_landed_run_drops_stale_staging() {
        with_isolated_state("edit-stale", |state| {
            let (tab, _conn, commands) = editable_tab(state);
            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            state.mark_row_deleted(tab, 1);
            state.add_row(tab);
            assert_eq!(state.tab(tab).expect("tab").staged_count(), 3);
            let sql = state.tab(tab).expect("tab").sql.clone();
            state.start_run(tab, &sql, 0, None);
            let run = match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { run, .. } => run,
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            state.apply(DbEvent::BatchOk { run, results: vec![items_result()] });
            let view = state.tab(tab).expect("tab");
            assert_eq!(view.staged_count(), 0);
            assert!(view.edits().is_empty());
            assert!(view.added_rows().is_empty());
            assert!(!view.is_row_deleted(0));
            assert!(!view.is_row_deleted(1));
        });
    }

    /// Center of the first painted text whose galley reads `needle`.
    fn text_center(
        output: &eframe::egui::FullOutput,
        needle: &str,
    ) -> Option<eframe::egui::Pos2> {
        output.shapes.iter().find_map(|clipped| match &clipped.shape {
            eframe::egui::Shape::Text(text) if text.galley.text() == needle => Some(
                eframe::egui::Rect::from_min_size(text.pos, text.galley.size()).center(),
            ),
            _ => None,
        })
    }

    /// A double-click on the painted text `needle`: two press and release
    /// pairs at the same spot, so egui counts the second as a double click.
    fn double_click_events(
        pos: eframe::egui::Pos2,
    ) -> Vec<eframe::egui::Event> {
        use eframe::egui::{Event, Modifiers, PointerButton};
        let click = |pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        vec![
            Event::PointerMoved(pos),
            click(true),
            click(false),
            click(true),
            click(false),
        ]
    }

    #[test]
    fn double_click_opens_the_cell_editor_and_enter_stages() {
        with_isolated_state("edit-input", |state| {
            let (tab, _conn, _commands) = editable_tab(state);
            state.activate(tab);
            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let mut first = ctx.run_ui(input.clone(), |ui| {
                crate::ui::grid::results(state, ui);
            });
            let pos = text_center(&first, "ada").expect("the name cell renders");
            first.textures_delta.clear();

            let mut double_click = input.clone();
            double_click.events = double_click_events(pos);
            let mut second = ctx.run_ui(double_click, |ui| {
                crate::ui::grid::results(state, ui);
            });
            second.textures_delta.clear();

            // The editor draws once before it holds focus, the way a user
            // types after the field appears.
            let mut opened = ctx.run_ui(input.clone(), |ui| {
                crate::ui::grid::results(state, ui);
            });
            opened.textures_delta.clear();

            let mut typed = input.clone();
            typed.events = vec![
                eframe::egui::Event::Key {
                    key: eframe::egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: eframe::egui::Modifiers {
                        ctrl: true,
                        command: true,
                        ..Default::default()
                    },
                },
                eframe::egui::Event::Text("Grace".to_owned()),
                eframe::egui::Event::Key {
                    key: eframe::egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: eframe::egui::Modifiers::NONE,
                },
            ];
            let mut third = ctx.run_ui(typed, |ui| {
                crate::ui::grid::results(state, ui);
            });
            third.textures_delta.clear();
            assert_eq!(state.tab(tab).expect("tab").edit_text(0, 2), Some("Grace"));

            // Esc closes the field without staging the typed text.
            let mut double_click = input.clone();
            double_click.events = double_click_events(pos);
            let mut fourth = ctx.run_ui(double_click, |ui| {
                crate::ui::grid::results(state, ui);
            });
            fourth.textures_delta.clear();
            let mut opened = ctx.run_ui(input.clone(), |ui| {
                crate::ui::grid::results(state, ui);
            });
            opened.textures_delta.clear();
            let mut escaped = input;
            escaped.events = vec![
                eframe::egui::Event::Text("nope".to_owned()),
                eframe::egui::Event::Key {
                    key: eframe::egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: eframe::egui::Modifiers::NONE,
                },
            ];
            let mut fifth = ctx.run_ui(escaped, |ui| {
                crate::ui::grid::results(state, ui);
            });
            fifth.textures_delta.clear();
            assert_eq!(state.tab(tab).expect("tab").edit_text(0, 2), Some("Grace"));
            assert_eq!(state.tab(tab).expect("tab").staged_count(), 1);
        });
    }

    #[test]
    fn inline_edit_applies_and_refreshes_through_the_sqlite_worker() {
        with_isolated_state("edit-live", |state| {
            let conn = add_sqlite_connection(state, "mem");
            state.connect(conn);
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            loop {
                state.drain_events();
                if matches!(state.connections()[0].state(), ConnectionState::Connected { .. }) {
                    break;
                }
                assert!(Instant::now() < deadline, "connection did not come up");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            let tab = state.new_tab(conn);
            let sql = "SELECT tenant, id, name FROM items ORDER BY id";
            let setup = "CREATE TABLE items (tenant TEXT, id INTEGER, name TEXT, \
                         PRIMARY KEY (tenant, id)); \
                         INSERT INTO items (tenant, id, name) VALUES \
                         ('acme', 1, 'ada'), ('globex', 2, 'grace')";
            state.start_run(tab, setup, 0, None);
            state.start_run(tab, sql, 0, None);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = sql.to_owned();
            }
            // The schema loaded when the connection came up, before the table
            // existed, so the test refreshes it the way the tree's button does.
            state.refresh_schema(conn);
            let has_items = |state: &AppState| {
                state.schema(conn).is_some_and(|schema| {
                    schema.databases().iter().any(|database| {
                        database.schemas().iter().any(|node| {
                            node.tables().iter().any(|table| table.name() == "items")
                        })
                    })
                })
            };
            loop {
                state.drain_events();
                let settled = !matches!(
                    state.tab(tab).expect("tab").outcome(),
                    RunOutcome::Running { .. }
                ) && has_items(state);
                if settled {
                    break;
                }
                assert!(Instant::now() < deadline, "setup did not settle");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(state.edit_target(tab).is_some(), "editable after the setup batch");

            state.stage_edit(tab, 1, 2, "Grace".to_owned());
            state.add_row(tab);
            state.stage_edit(tab, 2, 0, "initech".to_owned());
            state.stage_edit(tab, 2, 1, "3".to_owned());
            state.stage_edit(tab, 2, 2, "Lin".to_owned());
            state.apply_edits(tab);
            loop {
                state.drain_events();
                let running = matches!(
                    state.tab(tab).expect("tab").outcome(),
                    RunOutcome::Running { .. }
                );
                if !running {
                    break;
                }
                assert!(Instant::now() < deadline, "apply and refresh did not finish");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let tab_view = state.tab(tab).expect("tab");
            assert_eq!(tab_view.staged_count(), 0);
            let result = tab_view.active_query_result().expect("refreshed result");
            assert_eq!(result.row_count(), 3);
            assert_eq!(result.cell(0, 2), Some(&Value::Text("ada".to_owned())));
            assert_eq!(result.cell(1, 2), Some(&Value::Text("Grace".to_owned())));
            assert_eq!(result.cell(2, 0), Some(&Value::Text("initech".to_owned())));
            assert_eq!(result.cell(2, 1), Some(&Value::Integer(3)));
            assert_eq!(result.cell(2, 2), Some(&Value::Text("Lin".to_owned())));
            assert!(matches!(tab_view.outcome(), RunOutcome::Ok { .. }));
        });
    }

    #[test]
    fn grid_edit_controls_render_the_staged_count_or_a_read_only_hint() {
        with_isolated_state("edit-render", |state| {
            let (tab, _conn, _commands) = editable_tab(state);
            state.activate(tab);
            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input.clone(), |ui| {
                crate::ui::grid::results(state, ui);
            });
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "Apply (0)"), "{text:?}");
            assert!(text.iter().any(|t| t == "Discard"), "{text:?}");
            assert!(text.iter().any(|t| t == "Add row"), "{text:?}");
            assert!(text.iter().any(|t| t == "editing items"), "{text:?}");

            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            state.add_row(tab);
            let mut output = ctx.run_ui(input.clone(), |ui| {
                crate::ui::grid::results(state, ui);
            });
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "Apply (2)"), "{text:?}");

            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT i.id FROM items i JOIN items o ON o.id = i.id".to_owned();
            }
            let mut output = ctx.run_ui(input, |ui| {
                crate::ui::grid::results(state, ui);
            });
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "read only"), "{text:?}");
            assert!(!text.iter().any(|t| t.starts_with("Apply")), "{text:?}");
        });
    }
}
