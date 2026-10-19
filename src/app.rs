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
    DriverRegistry, ForeignKeyInfo, Ids, QueryResult, RowCount, RunId, SchemaInfo, SessionInfo,
    TabId, TableInfo, TxIdle, Value, explain_sql, spawn_session,
    TX_IDLE_ROLLBACK_MINUTES,
};
use crate::edit::{
    delete_statement, editable_table, ident, insert_typed_statement, is_text_type, parse_join,
    qualified_parts, quote_qualified, to_literal, update_typed_statement, wildcard_select,
};
use crate::import::{
    ColumnMap, ROW_CAP, dropped_rows, insert_typed_batches, map_headers, parse_csv, parse_jsonl,
};
use crate::sql::{
    collect_params, current_statement, split_statements, statement_changes_schema,
    statement_ranges, substitute,
};
use crate::ui::compare::{
    CompareError, CompareOutput, compare_select, diff_tables, parse_key_columns,
};

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

/// The per-connection transaction flag, open or closed. A flag, not a
/// protocol mirror: it changes only when a transaction control batch lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transaction {
    #[default]
    Closed,
    Open,
}

impl Transaction {
    pub fn is_open(self) -> bool {
        matches!(self, Transaction::Open)
    }
}

/// One explicit transaction control the toolbar offers. Label, statement,
/// and the flag its Ok landing leaves are one table, so the button the user
/// presses and the observer in [`AppState::finish_run`] cannot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionAction {
    Begin,
    Commit,
    Rollback,
}

impl TransactionAction {
    pub const ALL: [TransactionAction; 3] =
        [TransactionAction::Begin, TransactionAction::Commit, TransactionAction::Rollback];

    /// The toolbar button text.
    pub fn label(self) -> &'static str {
        match self {
            TransactionAction::Begin => "Begin",
            TransactionAction::Commit => "Commit",
            TransactionAction::Rollback => "Rollback",
        }
    }

    /// The one statement this control sends.
    pub fn statement(self) -> &'static str {
        match self {
            TransactionAction::Begin => "BEGIN",
            TransactionAction::Commit => "COMMIT",
            TransactionAction::Rollback => "ROLLBACK",
        }
    }

    /// The flag this control's Ok landing leaves behind.
    pub fn on_ok(self) -> Transaction {
        match self {
            TransactionAction::Begin => Transaction::Open,
            TransactionAction::Commit | TransactionAction::Rollback => Transaction::Closed,
        }
    }
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
    /// SSH secret (a password or key passphrase), never persisted.
    pub ssh_password: String,
    /// Slash-separated folder path placing the card in a sidebar tree.
    /// None is the ungrouped root. Always normalized through
    /// [`normalize_folder_path`], so it never holds blanks or stray slashes.
    folder: Option<String>,
    state: ConnectionState,
    /// The observed transaction flag. A BEGIN batch landing Ok opens it; a
    /// COMMIT or ROLLBACK batch landing Ok closes it, and a fatal worker
    /// error or disconnect closes it too. Cancel leaves it as it was, since
    /// stopping a statement says nothing about the server's transaction.
    /// With autocommit off the worker's [`DbEvent::TxState`] reports win.
    transaction: Transaction,
    /// The observed autocommit mode, for the toolbar toggle. Defaults to on
    /// and moves only when [`DbEvent::AutocommitSet`] lands. SQLite keeps
    /// it on: the toggle covers Postgres and MySQL only.
    autocommit: bool,
}

impl Connection {
    pub fn state(&self) -> &ConnectionState {
        &self.state
    }
    /// The sidebar folder path, None at the ungrouped root.
    pub fn folder(&self) -> Option<&str> {
        self.folder.as_deref()
    }
    /// The observed transaction flag, for the tab and card indicators.
    pub fn transaction(&self) -> Transaction {
        self.transaction
    }
    /// The observed autocommit mode, for the toolbar toggle.
    pub fn autocommit(&self) -> bool {
        self.autocommit
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

/// One structured WHERE operator. Equality, the four comparisons, LIKE,
/// IN, and the two NULL tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum FilterOp {
    #[default]
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Like,
    In,
    IsNull,
    IsNotNull,
}

impl FilterOp {
    pub const ALL: [FilterOp; 10] = [
        FilterOp::Eq,
        FilterOp::NotEq,
        FilterOp::Lt,
        FilterOp::LtEq,
        FilterOp::Gt,
        FilterOp::GtEq,
        FilterOp::Like,
        FilterOp::In,
        FilterOp::IsNull,
        FilterOp::IsNotNull,
    ];
    pub fn label(self) -> &'static str {
        match self {
            FilterOp::Eq => "=",
            FilterOp::NotEq => "<>",
            FilterOp::Lt => "<",
            FilterOp::LtEq => "<=",
            FilterOp::Gt => ">",
            FilterOp::GtEq => ">=",
            FilterOp::Like => "LIKE",
            FilterOp::In => "IN",
            FilterOp::IsNull => "IS NULL",
            FilterOp::IsNotNull => "IS NOT NULL",
        }
    }
}

/// One structured WHERE clause: a column, an operator, and the raw typed
/// text rendered through [`to_literal`]. The NULL tests ignore the value;
/// IN splits it on commas into one literal per item. Every field defaults
/// so a filter file written before the field existed still loads.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FilterClause {
    #[serde(default)]
    pub column: String,
    #[serde(default)]
    pub op: FilterOp,
    #[serde(default)]
    pub value: String,
}

/// One clause as SQL: `"col" = <literal>`. None when the column is blank,
/// when a valued operator has no value text, or when IN has no items.
pub fn clause_sql(clause: &FilterClause) -> Option<String> {
    let column = clause.column.trim();
    if column.is_empty() {
        return None;
    }
    let lhs = ident(column);
    match clause.op {
        FilterOp::IsNull => Some(format!("{lhs} IS NULL")),
        FilterOp::IsNotNull => Some(format!("{lhs} IS NOT NULL")),
        FilterOp::In => {
            let items: Vec<String> = clause
                .value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| to_literal(item, false))
                .collect();
            if items.is_empty() {
                return None;
            }
            Some(format!("{lhs} IN ({})", items.join(", ")))
        }
        op => {
            let value = clause.value.trim();
            if value.is_empty() {
                return None;
            }
            let sql_op = match op {
                FilterOp::Eq => "=",
                FilterOp::NotEq => "<>",
                FilterOp::Lt => "<",
                FilterOp::LtEq => "<=",
                FilterOp::Gt => ">",
                FilterOp::GtEq => ">=",
                FilterOp::Like => "LIKE",
                FilterOp::In | FilterOp::IsNull | FilterOp::IsNotNull => return None,
            };
            Some(format!("{lhs} {sql_op} {}", to_literal(value, false)))
        }
    }
}

/// Clauses plus the raw box as one WHERE body: the clause predicates and the
/// raw text, parenthesized and appended exactly as written, joined by AND.
/// None when both are empty.
pub fn filter_where(clauses: &[FilterClause], raw: &str) -> Option<String> {
    let mut parts: Vec<String> = clauses.iter().filter_map(clause_sql).collect();
    let raw = raw.trim();
    if !raw.is_empty() {
        parts.push(format!("({raw})"));
    }
    if parts.is_empty() { None } else { Some(parts.join(" AND ")) }
}

/// A table SELECT with the filter's WHERE composed in, still capped at 100
/// rows like [`table_select`].
pub fn filtered_table_select(table: &str, clauses: &[FilterClause], raw: &str) -> String {
    match filter_where(clauses, raw) {
        Some(where_clause) => {
            format!("SELECT * FROM {} WHERE {where_clause} LIMIT 100", quote_qualified(table))
        }
        None => format!("SELECT * FROM {} LIMIT 100", quote_qualified(table)),
    }
}

/// Cap on retained named filters, newest first.
const FILTER_HISTORY_CAP: usize = 50;

/// One named structured filter: a tab's clauses plus its raw box. Clauses
/// and raw default so a filter file written before the field existed still
/// loads, with an empty filter in place of the missing shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SavedFilter {
    pub name: String,
    #[serde(default)]
    pub clauses: Vec<FilterClause>,
    #[serde(default)]
    pub raw: String,
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
/// text, so a text key can render as a quoted literal. A joined SELECT keeps
/// the single-table fields for its display and fills the join tables plus
/// one entry per result column: Some for an editable column, None for a
/// read-only one. A single-table SELECT leaves the join fields empty and
/// fills `columns` with each result column's source name; a None there is an
/// expression such as `upper(name)`, which stays read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditTarget {
    pub table: String,
    pub pk_cols: Vec<(String, usize)>,
    pub pk_types: Vec<bool>,
    pub join_tables: Vec<JoinTableTarget>,
    pub join_columns: Vec<Option<JoinColumnTarget>>,
    pub columns: Vec<Option<String>>,
}

/// One source table behind a joined edit: its name and every primary key
/// column with the result column index its value shows in and whether its
/// type is text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinTableTarget {
    pub table: String,
    pub pk_cols: Vec<(String, usize)>,
    pub pk_types: Vec<bool>,
}

/// One editable result column behind a joined edit: which join table it
/// writes and which source column it sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinColumnTarget {
    pub table_index: usize,
    pub column: String,
}

impl EditTarget {
    /// True for a joined SELECT; deletes and inserts stay single-table only.
    pub fn is_join(&self) -> bool {
        !self.join_tables.is_empty()
    }
    /// True when the result column edits through the staged path. Every
    /// column of an unannotated single-table target edits; a mapped
    /// single-table column edits only when its SELECT item names a real
    /// column, and a joined column only when its mapping resolved to a keyed
    /// table.
    pub fn is_column_editable(&self, col: usize) -> bool {
        if self.is_join() {
            self.join_columns.get(col).is_some_and(|slot| slot.is_some())
        } else if !self.columns.is_empty() {
            self.columns.get(col).is_some_and(|slot| slot.is_some())
        } else {
            true
        }
    }
}

/// What a tab is: a free-form query, or a plain table view opened from the
/// navigator. A table tab keeps its SQL so refresh, filters, and edit
/// transitions keep working, but shows the table header instead of the
/// editor until `edit_as_query` flips it.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum TabKind {
    #[default]
    Query,
    Table { schema: String, table: String },
}

#[derive(Debug)]
pub struct Tab {
    pub id: TabId,
    pub conn: Option<ConnectionId>,
    pub sql: String,
    pub kind: TabKind,
    /// The saved entry the tab was opened from or last saved as, so the save
    /// prompt reopens with its name, folder, and pin. Not persisted.
    saved: Option<u64>,
    outcome: RunOutcome,
    run_scope: RunScope,
    /// Display results kept when the outcome stops being `Ok`, so a failed
    /// run leaves the last finished grid on screen.
    results: Vec<QueryResult>,
    active_result: usize,
    sort_col: Option<usize>,
    sort_dir: SortDir,
    filter: String,
    filter_clauses: Vec<FilterClause>,
    filter_raw: String,
    export_notice: Option<String>,
    export_failed: bool,
    edits: Vec<RowEdit>,
    deleted_rows: Vec<usize>,
    added_rows: Vec<Vec<String>>,
    /// The apply batch in flight, so its landing can refresh the grid.
    pending_apply: Option<RunId>,
    /// The import batch in flight with the notice its Ok landing records.
    /// The notice is set after the post-import refresh is sent, so the
    /// refresh's own send cannot clear it.
    pending_import: Option<(RunId, String)>,
    /// The silent post-apply refresh in flight. It adds no history entry.
    silent_run: Option<RunId>,
    /// The table DDL batch in flight. Its Ok landing refreshes the schema
    /// so the new or altered table appears in the navigator.
    pending_table: Option<RunId>,
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
    pub fn filter_clauses(&self) -> &[FilterClause] {
        &self.filter_clauses
    }
    pub fn filter_clauses_mut(&mut self) -> &mut Vec<FilterClause> {
        &mut self.filter_clauses
    }
    /// Push one structured clause onto the tab.
    pub fn add_filter_clause(&mut self, clause: FilterClause) {
        self.filter_clauses.push(clause);
    }
    /// Drop the clause at `index`, if any.
    pub fn remove_filter_clause(&mut self, index: usize) {
        if index < self.filter_clauses.len() {
            self.filter_clauses.remove(index);
        }
    }
    pub fn filter_raw(&self) -> &str {
        &self.filter_raw
    }
    pub fn set_filter_raw(&mut self, raw: String) {
        self.filter_raw = raw;
    }
    /// The tab's clauses plus its raw box as one WHERE body.
    pub fn filter_where(&self) -> Option<String> {
        filter_where(&self.filter_clauses, &self.filter_raw)
    }
    /// Drop every structured clause and the raw box.
    pub fn clear_table_filter(&mut self) {
        self.filter_clauses.clear();
        self.filter_raw.clear();
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

/// One table's on-demand count as the navigator sees it, from the click
/// until the session ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowCountState {
    Pending,
    Ready(RowCount),
    Failed(String),
}

/// Why a batch was sent, so its landing knows what it ended.
#[derive(Debug)]
enum BatchPurpose {
    Run,
    /// The staged edits; a successful landing re-reads the tab.
    ApplyEdits,
    /// The CSV import, with the notice its successful landing records; it
    /// adds no history entry.
    Import(String),
    /// The silent re-read after an apply; it adds no history entry.
    Refresh,
    /// The table DDL batch; its Ok landing refreshes the schema instead
    /// of re-running the tab.
    TableDdl,
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

/// One run's elapsed time as the log shows it, whole milliseconds.
pub fn format_history_took(took: std::time::Duration) -> String {
    let ms = took.as_secs_f64() * 1000.0;
    format!("{ms:.0} ms")
}

/// One run's outcome, row count, and timing as the log shows it.
pub fn history_stats(entry: &HistoryEntry) -> String {
    let took = format_history_took(entry.took);
    match (entry.succeeded, entry.rows) {
        (true, Some(rows)) => format!("ok · {rows} rows · {took}"),
        (true, None) => format!("ok · {took}"),
        (false, _) => format!("failed · {took}"),
    }
}

/// One saved-query version: the SQL text as saved and the save time as
/// unix seconds. Newest first in [`SavedQuery::versions`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SavedQueryVersion {
    pub at: u64,
    pub sql: String,
}

/// One named query. Newest first in [`AppState::saved`]. Holds the SQL and
/// its connection only, never credentials. `folder` places it in a sidebar
/// tree as a slash-separated path (`Some("ops/db")` nests under `ops`);
/// `None` is the ungrouped root. `pinned` floats it to the top of its own
/// tree node. Both default so files written before folders still load.
/// `versions` holds per-save snapshots newest first, capped at
/// [`SAVED_VERSION_CAP`]. It defaults so files written before versions
/// still load with empty history.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SavedQuery {
    pub id: u64,
    pub name: String,
    pub sql: String,
    pub conn: ConnectionId,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub versions: Vec<SavedQueryVersion>,
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
        folder: Option<String>,
        pinned: bool,
    ) -> Result<Self, SavedQueryError> {
        if name.trim().is_empty() {
            return Err(SavedQueryError::EmptyName);
        }
        Ok(Self { id, name, sql, conn, folder, pinned, versions: Vec::new() })
    }

    /// Record one version snapshot newest first, capped.
    fn push_version(&mut self, sql: &str, at: u64) {
        self.versions.insert(0, SavedQueryVersion { at, sql: sql.to_owned() });
        self.versions.truncate(SAVED_VERSION_CAP);
    }
}

/// One `.sql` file batch import: how many files became saved queries and
/// which file names were skipped (unreadable or blank), in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlImportOutcome {
    pub imported: usize,
    pub skipped: Vec<String>,
}

/// The saved-query name for a `.sql` path: its file stem, else its file
/// name, else `query`. `reports/daily.sql` becomes `daily`.
pub fn sql_import_name_for_path(path: &std::path::Path) -> String {
    let stem =
        path.file_stem().and_then(|s| s.to_str()).map(str::trim).filter(|s| !s.is_empty());
    let name =
        path.file_name().and_then(|s| s.to_str()).map(str::trim).filter(|s| !s.is_empty());
    stem.or(name).unwrap_or("query").to_owned()
}

/// The file name shown for a skipped import: the file name, else the whole
/// path, so temp files still identify themselves.
fn sql_import_display_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

/// The `.sql` files directly inside `dir`, sorted by path. Missing or
/// unreadable directories read as empty, so a cancelled picker imports
/// nothing instead of erroring.
pub fn collect_sql_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.is_file()
                    && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("sql"))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort();
    files
}

/// Normalize a raw folder path into slash-separated segments: trims each
/// segment, drops blanks and stray slashes, so `" //ops// db/ "` becomes
/// `Some("ops/db")`. None or blank becomes None, the ungrouped root. Every
/// folder write funnels through here, so stored paths are always canonical.
pub fn normalize_folder_path(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let parts: Vec<&str> =
        raw.split('/').map(str::trim).filter(|part| !part.is_empty()).collect();
    if parts.is_empty() { None } else { Some(parts.join("/")) }
}

/// One sidebar tree node. `path` is the full slash-separated path (empty at
/// the root) and `name` its last segment. `entries` holds the node's direct
/// entries only; `children` nest alphabetically by name.
#[derive(Debug)]
pub struct FolderNode<T> {
    pub name: String,
    pub path: String,
    pub entries: Vec<T>,
    pub children: Vec<FolderNode<T>>,
}

impl<T> FolderNode<T> {
    /// Ensure the nested path exists, creating intermediate nodes. Used for
    /// freshly created empty folders, which have no entry carrying the path.
    /// Siblings stay alphabetical at every level.
    pub fn ensure_path(&mut self, path: &str) {
        let mut node = self;
        let mut prefix = String::new();
        for segment in path.split('/').map(str::trim).filter(|s| !s.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            if !node.children.iter().any(|child| child.name == segment) {
                node.children.push(FolderNode {
                    name: segment.to_owned(),
                    path: prefix.clone(),
                    entries: Vec::new(),
                    children: Vec::new(),
                });
            }
            node.children.sort_by(|a, b| a.name.cmp(&b.name));
            let index = node
                .children
                .iter()
                .position(|child| child.name == segment)
                .expect("the segment was just ensured");
            node = &mut node.children[index];
        }
    }
}

/// Push one entry onto the node its normalized path names, creating
/// intermediates. Paths are already canonical on write; the segment filter
/// keeps hand-built test shapes honest too.
fn insert_folder_entry<T>(root: &mut FolderNode<T>, path: Option<&str>, entry: T) {
    let mut node = root;
    let mut prefix = String::new();
    let segments: Vec<&str> =
        path.iter().flat_map(|p| p.split('/')).map(str::trim).filter(|s| !s.is_empty()).collect();
    for segment in segments {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(segment);
        let index = match node.children.iter().position(|child| child.name == segment) {
            Some(index) => index,
            None => {
                node.children.push(FolderNode {
                    name: segment.to_owned(),
                    path: prefix.clone(),
                    entries: Vec::new(),
                    children: Vec::new(),
                });
                node.children.len() - 1
            }
        };
        node = &mut node.children[index];
    }
    node.entries.push(entry);
}

/// The saved-query sidebar tree: entries grouped by nested folder path with
/// pinned entries first inside every node, each pin class in stored order.
pub fn saved_folder_tree(saved: &[SavedQuery]) -> FolderNode<&SavedQuery> {
    let mut root = FolderNode { name: String::new(), path: String::new(), entries: Vec::new(), children: Vec::new() };
    for query in saved {
        insert_folder_entry(&mut root, query.folder.as_deref(), query);
    }
    sort_saved_tree(&mut root);
    root
}

/// Sort one saved tree node: children alphabetically, entries pinned first
/// (stable, so each pin class keeps the stored newest-first order).
fn sort_saved_tree(node: &mut FolderNode<&SavedQuery>) {
    node.children.sort_by(|a, b| a.name.cmp(&b.name));
    node.entries.sort_by(|a, b| b.pinned.cmp(&a.pinned));
    for child in &mut node.children {
        sort_saved_tree(child);
    }
}

/// The connection sidebar tree: cards grouped by nested folder path in
/// stored order, folders alphabetically.
pub fn connection_folder_tree(connections: &[Connection]) -> FolderNode<&Connection> {
    let mut root = FolderNode { name: String::new(), path: String::new(), entries: Vec::new(), children: Vec::new() };
    for conn in connections {
        insert_folder_entry(&mut root, conn.folder.as_deref(), conn);
    }
    sort_connection_tree(&mut root);
    root
}

/// Sort one connection tree node: children alphabetically, entries keep the
/// stored sidebar order.
fn sort_connection_tree(node: &mut FolderNode<&Connection>) {
    node.children.sort_by(|a, b| a.name.cmp(&b.name));
    for child in &mut node.children {
        sort_connection_tree(child);
    }
}

/// One flat sidebar group. `folder` is `None` for the no-folder group, which
/// leads. Entries are pinned first, each pin class in stored order.
#[derive(Debug)]
pub struct SavedGroup<'a> {
    pub folder: Option<&'a str>,
    pub entries: Vec<&'a SavedQuery>,
}

/// Group saved queries for the sidebar: the no-folder group first, then
/// folders in alphabetical order, pinned entries before unpinned ones.
pub fn saved_groups(saved: &[SavedQuery]) -> Vec<SavedGroup<'_>> {
    let mut folders: Vec<&str> = saved.iter().filter_map(|q| q.folder.as_deref()).collect();
    folders.sort_unstable();
    folders.dedup();
    let mut groups: Vec<SavedGroup<'_>> =
        vec![SavedGroup { folder: None, entries: Vec::new() }];
    groups.extend(folders.into_iter().map(|folder| SavedGroup {
        folder: Some(folder),
        entries: Vec::new(),
    }));
    for query in saved {
        let group = groups
            .iter_mut()
            .find(|g| g.folder == query.folder.as_deref())
            .expect("every folder became a group");
        group.entries.push(query);
    }
    for group in &mut groups {
        // Stable, so each pin class keeps the stored newest-first order.
        group.entries.sort_by(|a, b| b.pinned.cmp(&a.pinned));
    }
    groups.retain(|group| !group.entries.is_empty());
    groups
}

/// One bookmarked table: the connection plus schema and table names the
/// sidebar section pins. Every field defaults so files written before a
/// field existed still load.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TableBookmark {
    pub conn: ConnectionId,
    #[serde(default)]
    pub schema: String,
    #[serde(default)]
    pub table: String,
}

/// Cap on retained bookmarks, newest first.
const BOOKMARKS_CAP: usize = 200;

/// Whether a table name survives the navigator filter. Empty filter matches
/// everything, else a case-insensitive substring on the table name.
pub fn table_matches_navigator_filter(table: &str, filter: &str) -> bool {
    let filter = filter.trim();
    if filter.is_empty() {
        return true;
    }
    table.to_lowercase().contains(&filter.to_lowercase())
}

/// One entity the Ctrl+K palette can open. The variant carries what opening
/// it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchResult {
    Connection(ConnectionId),
    Saved(u64),
    Table { conn: ConnectionId, schema: String, table: String },
    Column { conn: ConnectionId, schema: String, table: String, column: String },
}

impl SearchResult {
    /// The tie-break order across kinds: connections, saved queries, tables,
    /// columns.
    fn kind_rank(&self) -> u8 {
        match self {
            SearchResult::Connection(_) => 0,
            SearchResult::Saved(_) => 1,
            SearchResult::Table { .. } => 2,
            SearchResult::Column { .. } => 3,
        }
    }
}

/// The open palette: filter text, the selected row, and the ranked rows.
/// `None` in [`AppState`] is closed, so a closed palette holds no stale list.
#[derive(Debug)]
pub struct SearchPalette {
    pub query: String,
    pub selected: usize,
    pub results: Vec<SearchResult>,
}

/// One palette entry before filtering: what opening it does, its display
/// name, and the second text its kind matches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCandidate {
    pub result: SearchResult,
    pub name: String,
    /// A connection's kind or a saved query's folder; `None` for tables and
    /// columns, which match on their names only.
    pub extra: Option<String>,
}

/// A table entry the palette asked the navigator to open and scroll to,
/// consumed by the next schema render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigatorReveal {
    pub conn: ConnectionId,
    pub schema: String,
    pub table: String,
}

/// One side of a table data compare: the schema and table the read selects.
/// Both halves are plain text, so a pick outlives a schema reload that no
/// longer lists it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComparePick {
    pub schema: String,
    pub table: String,
}

impl ComparePick {
    /// False when either half is blank, so an unset pick never sends.
    pub fn is_set(&self) -> bool {
        !self.schema.trim().is_empty() && !self.table.trim().is_empty()
    }
}

/// Which read a compare run id belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompareSide {
    Left,
    Right,
}

/// The table data compare: two read-only table picks with key columns, their
/// loaded results, and the cached diff. Picks and keys move only through
/// methods that drop stale results, so the view never diffs old rows under
/// new picks. Runs land through [`AppState::apply`] with no tab outcome, so
/// a compare never flashes the grid.
#[derive(Debug, Default)]
pub struct CompareState {
    left: ComparePick,
    right: ComparePick,
    keys: String,
    left_result: Option<QueryResult>,
    right_result: Option<QueryResult>,
    left_run: Option<RunId>,
    right_run: Option<RunId>,
    error: Option<String>,
    output: Option<CompareOutput>,
}

impl CompareState {
    pub fn left(&self) -> &ComparePick {
        &self.left
    }
    pub fn right(&self) -> &ComparePick {
        &self.right
    }
    pub fn keys(&self) -> &str {
        &self.keys
    }
    pub fn left_result(&self) -> Option<&QueryResult> {
        self.left_result.as_ref()
    }
    pub fn right_result(&self) -> Option<&QueryResult> {
        self.right_result.as_ref()
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn output(&self) -> Option<&CompareOutput> {
        self.output.as_ref()
    }
    /// True while either read is in flight.
    pub fn running(&self) -> bool {
        self.left_run.is_some() || self.right_run.is_some()
    }
}

/// Cap on retained history entries, newest first.
const HISTORY_CAP: usize = 200;
/// Cap on history rows rendered in the sidebar.
pub const HISTORY_SHOWN: usize = 30;
/// Cap on retained saved queries, newest first.
const SAVED_CAP: usize = 200;
/// Cap on retained versions per saved query, newest first.
const SAVED_VERSION_CAP: usize = 20;
/// Cap on palette rows for one query, best-ranked first.
const SEARCH_CAP: usize = 50;

/// How one candidate matched the query, best first: the field starts with
/// it, a word inside it starts with it, or its characters appear in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MatchTier {
    Prefix,
    WordStart,
    Subsequence,
}

/// The best tier the lowercased `query` reaches inside `field`.
fn match_tier(query: &str, field: &str) -> Option<MatchTier> {
    let field = field.to_lowercase();
    if field.starts_with(query) {
        return Some(MatchTier::Prefix);
    }
    let mut after_word = false;
    for (index, ch) in field.char_indices() {
        if after_word && field[index..].starts_with(query) {
            return Some(MatchTier::WordStart);
        }
        after_word = !ch.is_alphanumeric();
    }
    is_subsequence(&field, query).then_some(MatchTier::Subsequence)
}

/// Whether every char of `query` appears in `field` in order.
fn is_subsequence(field: &str, query: &str) -> bool {
    let mut field_chars = field.chars();
    query.chars().all(|wanted| field_chars.any(|c| c == wanted))
}

/// Rank candidates against `query`: prefix matches above word starts above
/// plain subsequences, ties by kind then name, capped at [`SEARCH_CAP`]. The
/// empty query matches nothing, so a fresh palette opens empty.
fn rank_candidates(query: &str, candidates: &[SearchCandidate]) -> Vec<SearchResult> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut ranked: Vec<(MatchTier, u8, String, &SearchResult)> = candidates
        .iter()
        .filter_map(|candidate| {
            let tier = match_tier(&query, &candidate.name)
                .into_iter()
                .chain(candidate.extra.as_deref().and_then(|extra| match_tier(&query, extra)))
                .min()?;
            Some((
                tier,
                candidate.result.kind_rank(),
                candidate.name.to_lowercase(),
                &candidate.result,
            ))
        })
        .collect();
    ranked.sort_by(|a, b| {
        a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2))
    });
    ranked
        .into_iter()
        .take(SEARCH_CAP)
        .map(|(_, _, _, result)| result.clone())
        .collect()
}

/// Every entity the palette can open: all connections and saved queries,
/// plus the loaded tables and columns behind the default connection.
fn search_candidates(state: &AppState) -> Vec<SearchCandidate> {
    let mut candidates: Vec<SearchCandidate> = Vec::new();
    for conn in state.connections() {
        candidates.push(SearchCandidate {
            result: SearchResult::Connection(conn.id),
            name: conn.name.clone(),
            extra: Some(conn.config.kind().label().to_owned()),
        });
    }
    for query in state.saved_queries() {
        candidates.push(SearchCandidate {
            result: SearchResult::Saved(query.id),
            name: query.name.clone(),
            extra: query.folder.clone(),
        });
    }
    if let Some(conn) = state.default_connection() {
        if let Some(schema) = state.schema(conn) {
            for database in schema.databases() {
                for node in database.schemas() {
                    for table in node.tables().iter().chain(node.views()) {
                        candidates.push(SearchCandidate {
                            result: SearchResult::Table {
                                conn,
                                schema: node.name().to_owned(),
                                table: table.name().to_owned(),
                            },
                            name: table.name().to_owned(),
                            extra: None,
                        });
                        for column in table.columns() {
                            candidates.push(SearchCandidate {
                                result: SearchResult::Column {
                                    conn,
                                    schema: node.name().to_owned(),
                                    table: table.name().to_owned(),
                                    column: column.name().to_owned(),
                                },
                                name: column.name().to_owned(),
                                extra: None,
                            });
                        }
                    }
                }
            }
        }
    }
    candidates
}

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
    kind: DbKind,
    scope: RunScope,
    cursor: usize,
    selection: Option<Range<usize>>,
) -> Vec<String> {
    match scope {
        RunScope::All => split_statements(sql, kind),
        RunScope::Current => {
            let statements = split_statements(sql, kind);
            current_statement(sql, cursor, kind)
                .and_then(|index| statements.get(index).cloned())
                .into_iter()
                .collect()
        }
        RunScope::Selection => match selection {
            Some(selection) => statement_ranges(sql, kind)
                .into_iter()
                .filter(|range| range.start < selection.end && selection.start < range.end)
                .map(|range| sql[range].to_owned())
                .collect(),
            None => effective_statements(sql, kind, RunScope::Current, cursor, None),
        },
    }
}

/// The transaction control a landed batch carries: Some only when the batch
/// is exactly one statement spelling BEGIN, COMMIT, or ROLLBACK, so a script
/// that mixes statements, an explain form, and every other batch leave the
/// flag untouched. The spelling comes from [`TransactionAction`], so a typed
/// `begin` matches the button's `BEGIN`.
fn transaction_action(statements: &[String]) -> Option<TransactionAction> {
    let [statement] = statements else { return None };
    TransactionAction::ALL
        .into_iter()
        .find(|action| statement.eq_ignore_ascii_case(action.statement()))
}

/// The loaded table a possibly qualified name resolves to: under its schema
/// when qualified, else the first case-insensitive match across databases and
/// schemas. An unqualified name takes the first match; a deeper qualifier
/// resolves nothing.
fn find_table<'a>(schema: &'a SchemaInfo, table: &str) -> Option<&'a TableInfo> {
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
                if info.name().eq_ignore_ascii_case(wanted_table) {
                    return Some(info);
                }
            }
        }
    }
    None
}

/// The SELECT a table entry loads: every column, capped, with the schema
/// qualifier quoted.
fn table_select(schema: &str, table: &str) -> String {
    format!(
        "SELECT * FROM \"{}\".\"{}\" LIMIT 100",
        schema.replace('"', "\"\""),
        table.replace('"', "\"\"")
    )
}

/// One foreign key jump: the referenced schema and table plus the filtered
/// SELECT that lands on the referenced row. Pure, so tests assert literals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkJump {
    pub schema: String,
    pub table: String,
    pub sql: String,
}

/// The single-column foreign key on `child_column`, if exactly one exists.
/// None for no key, several keys, or a multi-column key, so ambiguous cells
/// stay put rather than guessing.
fn single_fk<'a>(info: &'a TableInfo, child_column: &str) -> Option<&'a ForeignKeyInfo> {
    let mut found = None;
    for fk in info.foreign_keys() {
        if fk.columns().len() != 1 || fk.ref_columns().len() != 1 {
            continue;
        }
        if !fk.columns()[0].eq_ignore_ascii_case(child_column) {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(fk);
    }
    found
}

/// The schema node and table behind a child name, tables only. Foreign keys
/// live on tables, so a view or a missing name resolves nothing.
fn fk_child<'a>(schema: &'a SchemaInfo, child_schema: &str, child_table: &str) -> Option<&'a TableInfo> {
    for database in schema.databases() {
        for node in database.schemas() {
            if !node.name().eq_ignore_ascii_case(child_schema) {
                continue;
            }
            for info in node.tables() {
                if info.name().eq_ignore_ascii_case(child_table) {
                    return Some(info);
                }
            }
        }
    }
    None
}

/// The referenced table behind `ref_table`: a qualified name resolves under
/// its own schema, a bare name under the child's schema only. No fallback
/// search, so an unresolvable name stays rather than guessing.
fn fk_parent<'a>(
    schema: &'a SchemaInfo,
    child_schema: &str,
    ref_table: &str,
) -> Option<(String, &'a TableInfo)> {
    let parts = qualified_parts(ref_table);
    let (wanted_schema, wanted_table) = match parts.as_slice() {
        [table] => (child_schema, table.as_str()),
        [first, second] => (first.as_str(), second.as_str()),
        _ => return None,
    };
    for database in schema.databases() {
        for node in database.schemas() {
            if !node.name().eq_ignore_ascii_case(wanted_schema) {
                continue;
            }
            for info in node.tables() {
                if info.name().eq_ignore_ascii_case(wanted_table) {
                    return Some((node.name().to_owned(), info));
                }
            }
        }
    }
    None
}

/// The filtered SELECT landing on one referenced row. None for a NULL value,
/// a NULL literal, a missing child or parent, a missing referenced column,
/// or anything but one single-column key, so those cells stay put.
pub fn fk_jump(
    schema: &SchemaInfo,
    child_schema: &str,
    child_table: &str,
    child_column: &str,
    value: &str,
    was_null: bool,
) -> Option<FkJump> {
    if was_null {
        return None;
    }
    let child = fk_child(schema, child_schema, child_table)?;
    let fk = single_fk(child, child_column)?;
    let ref_col = fk.ref_columns().first()?;
    let (parent_schema, parent) = fk_parent(schema, child_schema, fk.ref_table())?;
    if !parent.columns().iter().any(|c| c.name().eq_ignore_ascii_case(ref_col)) {
        return None;
    }
    let text_key = parent
        .columns()
        .iter()
        .find(|c| c.name().eq_ignore_ascii_case(ref_col))
        .map(|c| is_text_type(c.type_name()))
        .unwrap_or(false);
    let literal = key_literal(value, was_null, text_key);
    if literal == "NULL" {
        return None;
    }
    let sql = format!(
        "SELECT * FROM \"{}\".\"{}\" WHERE {} = {} LIMIT 100",
        parent_schema.replace('"', "\"\""),
        parent.name().replace('"', "\"\""),
        ident(ref_col),
        literal
    );
    Some(FkJump { schema: parent_schema, table: parent.name().to_owned(), sql })
}

/// The referenced table a schema link reveals: Some only for one
/// single-column key whose table and column resolve, so multi-column and
/// unresolvable links render as plain text and stay put.
pub fn fk_reveal_target(
    schema: &SchemaInfo,
    child_schema: &str,
    _child_table: &str,
    fk: &ForeignKeyInfo,
) -> Option<(String, String)> {
    if fk.columns().len() != 1 || fk.ref_columns().len() != 1 {
        return None;
    }
    let ref_col = fk.ref_columns().first()?;
    let (parent_schema, parent) = fk_parent(schema, child_schema, fk.ref_table())?;
    if !parent.columns().iter().any(|c| c.name().eq_ignore_ascii_case(ref_col)) {
        return None;
    }
    Some((parent_schema, parent.name().to_owned()))
}

/// The child table a References entry reveals: the entry carries the parent
/// columns with `ref_table` set to the child, so the same single-column and
/// resolvability gates hold with the roles swapped.
pub fn fk_incoming_target(
    schema: &SchemaInfo,
    parent_schema: &str,
    fk: &ForeignKeyInfo,
) -> Option<(String, String)> {
    if fk.columns().len() != 1 || fk.ref_columns().len() != 1 {
        return None;
    }
    let child_col = fk.ref_columns().first()?;
    let (child_schema, child) = fk_parent(schema, parent_schema, fk.ref_table())?;
    if !child.columns().iter().any(|c| c.name().eq_ignore_ascii_case(child_col)) {
        return None;
    }
    Some((child_schema, child.name().to_owned()))
}

/// Primary key columns in key order with their declared types, from the
/// loaded schema tree. A table without a key is not editable.
fn primary_key(schema: &SchemaInfo, table: &str) -> Option<Vec<(String, String)>> {
    let info = find_table(schema, table)?;
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
    (!key.is_empty()).then_some(key)
}

/// The one source a qualifier names: its alias first, else its table's last
/// part or full name. None when no source or several match, so ambiguous
/// qualifiers stay read-only rather than guessing.
fn resolve_qualifier(plan: &crate::edit::JoinPlan, qualifier: &str) -> Option<usize> {
    let mut found = None;
    for (index, source) in plan.sources.iter().enumerate() {
        let matches = source
            .alias
            .as_deref()
            .is_some_and(|alias| alias.eq_ignore_ascii_case(qualifier))
            || last_part(&source.table).eq_ignore_ascii_case(qualifier)
            || source.table.eq_ignore_ascii_case(qualifier);
        if matches {
            if found.is_some() {
                return None;
            }
            found = Some(index);
        }
    }
    found
}

/// The one source carrying a bare column name in the loaded schema. None
/// when no source or several carry it, so ambiguous columns stay read-only.
fn resolve_bare_column(
    schema: &SchemaInfo,
    plan: &crate::edit::JoinPlan,
    column: &str,
) -> Option<usize> {
    let mut found = None;
    for (index, source) in plan.sources.iter().enumerate() {
        let carries = find_table(schema, &source.table).is_some_and(|info| {
            info.columns().iter().any(|c| c.name().eq_ignore_ascii_case(column))
        });
        if carries {
            if found.is_some() {
                return None;
            }
            found = Some(index);
        }
    }
    found
}

/// The last dot part of a canonical table name, for qualifier matching.
fn last_part(table: &str) -> String {
    qualified_parts(table).last().cloned().unwrap_or_else(|| table.to_owned())
}

/// Column names of a loaded table in schema order, the way [`primary_key`]
/// resolves the table, so CSV headers map onto all of its columns.
fn table_columns(schema: &SchemaInfo, table: &str) -> Option<Vec<String>> {
    Some(find_table(schema, table)?.columns().iter().map(|c| c.name().to_owned()).collect())
}

/// True when the loaded schema calls `column` on `table` text, so a staged
/// or imported cell quotes as text instead of reading as a number.
fn table_column_is_text(schema: &SchemaInfo, table: &str, column: &str) -> bool {
    find_table(schema, table)
        .and_then(|info| {
            info.columns().iter().find(|c| c.name().eq_ignore_ascii_case(column))
        })
        .is_some_and(|c| is_text_type(c.type_name()))
}

/// Key pairs for one row: each key column with the literal of the original
/// cell display, so the WHERE clause names the row exactly as displayed. A
/// text key always renders quoted; the rest keep [`to_literal`]'s mapping.
/// None when a key cell is NULL: `= NULL` matches nothing, so an outer join's
/// unmatched side refuses rather than building a dead statement.
fn key_pairs(
    result: &QueryResult,
    target: &EditTarget,
    row: usize,
) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::with_capacity(target.pk_cols.len());
    for (index, (name, col)) in target.pk_cols.iter().enumerate() {
        let cell = result.cell(row, *col)?;
        if matches!(cell, Value::Null) {
            return None;
        }
        let text_key = target.pk_types.get(index).copied().unwrap_or(false);
        let literal = key_literal(&cell.display(usize::MAX), false, text_key);
        pairs.push((name.clone(), literal));
    }
    Some(pairs)
}

/// Key pairs for one joined table: the same literal mapping as
/// [`key_pairs`] over that table's result columns, and the same NULL refusal
/// so an unmatched outer-joined side never builds `WHERE "id" = NULL`.
fn join_key_pairs(
    result: &QueryResult,
    table: &JoinTableTarget,
    row: usize,
) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::with_capacity(table.pk_cols.len());
    for (index, (name, col)) in table.pk_cols.iter().enumerate() {
        let cell = result.cell(row, *col)?;
        if matches!(cell, Value::Null) {
            return None;
        }
        let text_key = table.pk_types.get(index).copied().unwrap_or(false);
        let literal = key_literal(&cell.display(usize::MAX), false, text_key);
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
    /// Badge tint per theme. Neutral for dev, amber for test, red for prod.
    pub fn badge_color(self, theme: crate::ui::Theme) -> eframe::egui::Color32 {
        let palette = crate::ui::Palette::of(theme.egui());
        match self {
            Environment::Dev => palette.badge_dev,
            Environment::Test => palette.badge_test,
            Environment::Prod => palette.badge_prod,
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
    /// SSH secret (a password or key passphrase), never persisted.
    pub ssh_password: String,
    /// Raw sidebar folder path; normalized to nested segments on save.
    pub folder: String,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct SavePrompt {
    pub name: String,
    pub folder: String,
    pub pinned: bool,
    pub error: Option<String>,
}

/// Which file shape staged the pending import: a CSV header row or one JSON
/// object per line. The file extension names it, so the modal and its errors
/// speak the file the user picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSource {
    Csv,
    Jsonl,
}

impl ImportSource {
    /// `.jsonl` in any case stages as JSONL; every other extension stages as
    /// CSV, so the existing `.csv` paths and extensionless temp files keep
    /// the old behavior.
    pub fn of_path(path: &std::path::Path) -> Self {
        if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl")) {
            ImportSource::Jsonl
        } else {
            ImportSource::Csv
        }
    }
    /// The modal's column header noun: `CSV` or `JSONL`.
    pub fn label(self) -> &'static str {
        match self {
            ImportSource::Csv => "CSV",
            ImportSource::Jsonl => "JSONL",
        }
    }
}

/// A parsed file awaiting its confirmation: the tab it imports into, the
/// file's headers and rows, the resolved header map, the target table, and
/// the shape it parsed as.
#[derive(Debug)]
pub struct ImportDraft {
    pub tab: TabId,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub map: ColumnMap,
    pub table: String,
    pub source: ImportSource,
}

impl ImportDraft {
    /// The modal's first column header: `CSV column` or `JSONL key`.
    pub fn column_title(&self) -> &'static str {
        match self.source {
            ImportSource::Csv => "CSV column",
            ImportSource::Jsonl => "JSONL key",
        }
    }
}

/// A run held for its write guard: the tab and connection it would run on,
/// the exact prepared batch Confirm would send, and the unqualified
/// statement the modal names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWriteConfirm {
    pub tab: TabId,
    pub conn: ConnectionId,
    pub statements: Vec<String>,
    pub kind: String,
    pub table: String,
}

/// A table DDL batch held for its prod confirmation: the connection and
/// tab it would run on plus the exact statements Confirm would send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTableDdl {
    pub conn: ConnectionId,
    pub tab: TabId,
    pub statements: Vec<String>,
}

/// Which sidebar section is on display. Not persisted; a relaunch starts at
/// the schema view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidebarView {
    #[default]
    Schema,
    History,
    Saved,
}

impl SidebarView {
    pub const ALL: [SidebarView; 3] = [Self::Schema, Self::History, Self::Saved];

    pub fn label(self) -> &'static str {
        match self {
            SidebarView::Schema => "Schema",
            SidebarView::History => "History",
            SidebarView::Saved => "Saved",
        }
    }
}

/// The open visual table editor: the connection it builds for, the loaded
/// table it edits (None for a create), and the live form the preview reads.
#[derive(Debug, Clone)]
pub struct TableEditorState {
    pub conn: ConnectionId,
    pub original: Option<TableInfo>,
    pub form: crate::ui::ddl::TableForm,
}

/// A session terminate held for its prod confirmation: the connection and
/// backend Confirm would kill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTerminate {
    pub conn: ConnectionId,
    pub backend: String,
}

/// The modal dialog that owns the screen, frontmost first. One variant per
/// dialog an `AppState` field can show, so every key gate and the root
/// dispatcher read the same one answer. Not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modal {
    /// The prod connect confirmation over `pending_prod`.
    ProdConnect,
    /// The prod session terminate confirmation over `pending_terminate`.
    Terminate,
    /// The new/edit connection form over `draft`.
    Draft,
    /// The save-query prompt over `save_prompt`.
    SavePrompt,
    /// The password prompt over `password_prompt`.
    PasswordPrompt,
    /// The import confirmation over `imports`.
    Import,
    /// The UPDATE/DELETE without WHERE confirmation over `pending_write`.
    WriteConfirm,
    /// The placeholder value prompt over `pending_params`.
    Params,
    /// The staged-edit SQL preview over `pending_edit_preview`.
    EditPreview,
    /// The prod table DDL confirmation over `pending_table_confirm`.
    TableDdl,
    /// The visual table creator over `table_editor`.
    TableEditor,
    /// The table compare window over `compare_open`.
    Compare,
    /// The Ctrl+K command palette over `search`.
    Palette,
}

impl Modal {
    /// Every dialog an `AppState` field can show, in the order ties fall back
    /// to when two were never marked.
    pub const ALL: [Modal; 13] = [
        Modal::ProdConnect,
        Modal::Terminate,
        Modal::Draft,
        Modal::SavePrompt,
        Modal::PasswordPrompt,
        Modal::Import,
        Modal::WriteConfirm,
        Modal::Params,
        Modal::EditPreview,
        Modal::TableDdl,
        Modal::TableEditor,
        Modal::Compare,
        Modal::Palette,
    ];

    /// This dialog's slot in [`Modal::ALL`]. Exhaustive, so adding a
    /// variant fails to compile until its slot is placed.
    fn index(self) -> usize {
        match self {
            Modal::ProdConnect => 0,
            Modal::Terminate => 1,
            Modal::Draft => 2,
            Modal::SavePrompt => 3,
            Modal::PasswordPrompt => 4,
            Modal::Import => 5,
            Modal::WriteConfirm => 6,
            Modal::Params => 7,
            Modal::EditPreview => 8,
            Modal::TableDdl => 9,
            Modal::TableEditor => 10,
            Modal::Compare => 11,
            Modal::Palette => 12,
        }
    }

    /// Whether plain Enter means submit. Only the prompts with a text field
    /// and the palette, whose Enter opens the selected row.
    pub fn submits_on_enter(self) -> bool {
        matches!(
            self,
            Modal::SavePrompt | Modal::PasswordPrompt | Modal::Params | Modal::Palette
        )
    }
}

/// A run held for its parameter values: the tab and connection it would run
/// on, whether it is an explain run, the raw scoped statements with their
/// placeholders, and the collected parameters with the typed values. Confirm
/// substitutes the values and continues through the prepared send path, so
/// history keeps this raw text, not the substituted batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingParams {
    pub tab: TabId,
    pub conn: ConnectionId,
    pub explain: bool,
    pub statements: Vec<String>,
    pub params: Vec<crate::sql::Param>,
}

pub struct AppState {
    connections: Vec<Connection>,
    tabs: Vec<Tab>,
    active: Option<TabId>,
    draft: Option<ConnectDraft>,
    password_prompt: Option<ConnectionId>,
    pending_prod: Option<ConnectionId>,
    pending_write: Option<PendingWriteConfirm>,
    pending_params: Option<PendingParams>,
    /// The tab whose staged edits await their SQL preview. Confirm sends
    /// through the apply path, Cancel keeps the staged edits.
    pending_edit_preview: Option<TabId>,
    prompt_password: String,
    ids: Ids,
    registry: DriverRegistry,
    events_tx: mpsc::Sender<DbEvent>,
    events_rx: mpsc::Receiver<DbEvent>,
    ui_ctx: eframe::egui::Context,
    schemas: HashMap<ConnectionId, SchemaInfo>,
    schema_loading: HashSet<ConnectionId>,
    schema_errors: HashMap<ConnectionId, String>,
    /// Navigator counts requested on demand, keyed by connection, schema,
    /// and table, kept until the session ends.
    row_counts: HashMap<(ConnectionId, String, String), RowCountState>,
    /// Session lists requested on demand per connection, kept until the
    /// session ends.
    sessions: HashMap<ConnectionId, Vec<SessionInfo>>,
    sessions_loading: HashSet<ConnectionId>,
    sessions_errors: HashMap<ConnectionId, String>,
    /// The terminate awaiting its prod confirmation, if any.
    pending_terminate: Option<PendingTerminate>,
    /// The open visual table editor, if any. Not persisted.
    table_editor: Option<TableEditorState>,
    /// The table compare window's open flag. Not persisted.
    compare_open: bool,
    /// The table DDL batch awaiting its prod confirmation, if any.
    pending_table_confirm: Option<PendingTableDdl>,
    history: Vec<HistoryEntry>,
    /// True narrows the execution log to the current connection. False shows
    /// every connection. Never persisted, so a relaunch starts at all.
    history_current_only: bool,
    saved: Vec<SavedQuery>,
    filter_history: Vec<SavedFilter>,
    bookmarks: Vec<TableBookmark>,
    navigator_filter: String,
    /// Which sidebar section shows. Not persisted.
    sidebar_view: SidebarView,
    search: Option<SearchPalette>,
    /// The table the palette asked the navigator to reveal; the schema tree
    /// consumes it on its next render.
    navigator_reveal: Option<NavigatorReveal>,
    save_prompt: Option<SavePrompt>,
    imports: Option<ImportDraft>,
    compare: CompareState,
    theme: crate::ui::Theme,
    /// The idle schedule for open manual transactions, threaded to each
    /// worker at spawn. Tests swap in a short schedule.
    tx_idle: TxIdle,
    /// The sequence number each dialog got when it last opened, indexed by
    /// [`Modal::index`]. The newest open dialog owns the keyboard.
    modal_marks: [u64; Modal::ALL.len()],
    modal_seq: u64,
}

impl AppState {
    pub fn new(egui_ctx: &eframe::egui::Context) -> Self {
        let profiles = load_connections();
        let mut ids = Ids::new();
        ids.resume_after(&profiles.iter().map(|c| c.id).collect::<Vec<_>>());
        let saved = load_saved_queries();
        ids.resume_after_saved(&saved.iter().map(|q| q.id).collect::<Vec<_>>());
        let settings = load_settings();
        crate::ui::apply_theme(egui_ctx, settings.theme);
        let (events_tx, events_rx) = mpsc::channel();
        let mut state = Self {
            connections: profiles,
            tabs: Vec::new(),
            active: None,
            draft: None,
            password_prompt: None,
            pending_prod: None,
            pending_write: None,
            pending_params: None,
            pending_edit_preview: None,
            prompt_password: String::new(),
            ids,
            registry: DriverRegistry::bundled(),
            events_tx,
            events_rx,
            ui_ctx: egui_ctx.clone(),
            schemas: HashMap::new(),
            schema_loading: HashSet::new(),
            schema_errors: HashMap::new(),
            row_counts: HashMap::new(),
            sessions: HashMap::new(),
            sessions_loading: HashSet::new(),
            sessions_errors: HashMap::new(),
            pending_terminate: None,
            table_editor: None,
            compare_open: false,
            pending_table_confirm: None,
            history: load_history(),
            history_current_only: false,
            saved,
            filter_history: load_saved_filters(),
            bookmarks: load_bookmarks(),
            navigator_filter: String::new(),
            sidebar_view: SidebarView::default(),
            search: None,
            navigator_reveal: None,
            save_prompt: None,
            imports: None,
            compare: CompareState::default(),
            theme: settings.theme,
            tx_idle: TxIdle::default(),
            modal_marks: [0; Modal::ALL.len()],
            modal_seq: 0,
        };
        state.restore_tabs();
        state
    }

    /// The modal that owns the screen right now. Every "is a modal open" gate
    /// and per-dialog key handler reads this one answer.
    ///
    /// The newest dialog wins: opening one over another stacks it on top,
    /// gives it the keyboard, and draws it last, so what the operator sees is
    /// what Escape and Enter reach. Ties, which only a transition that forgot
    /// to mark a dialog can create, fall back to [`Modal::ALL`] order.
    pub fn modal(&self) -> Option<Modal> {
        Modal::ALL
            .iter()
            .copied()
            .filter(|modal| self.modal_is_open(*modal))
            .max_by_key(|modal| {
                (self.modal_marks[modal.index()], std::cmp::Reverse(modal.index()))
            })
    }

    /// Every open dialog, back to front. `root` draws it in this order, so the
    /// last one is the topmost window and the one [`AppState::modal`] names.
    pub fn modals_back_to_front(&self) -> Vec<Modal> {
        let mut open: Vec<Modal> = Modal::ALL
            .iter()
            .copied()
            .filter(|modal| self.modal_is_open(*modal))
            .collect();
        open.sort_by_key(|modal| self.modal_marks[modal.index()]);
        open
    }

    /// Whether one dialog's backing state is set.
    fn modal_is_open(&self, modal: Modal) -> bool {
        match modal {
            Modal::ProdConnect => self.pending_prod.is_some(),
            Modal::Terminate => self.pending_terminate.is_some(),
            Modal::Draft => self.draft.is_some(),
            Modal::SavePrompt => self.save_prompt.is_some(),
            Modal::PasswordPrompt => self.password_prompt.is_some(),
            Modal::Import => self.imports.is_some(),
            Modal::WriteConfirm => self.pending_write.is_some(),
            Modal::Params => self.pending_params.is_some(),
            Modal::EditPreview => self.pending_edit_preview.is_some(),
            Modal::TableDdl => self.pending_table_confirm.is_some(),
            Modal::TableEditor => self.table_editor.is_some(),
            Modal::Compare => self.compare_open,
            Modal::Palette => self.search.is_some(),
        }
    }

    /// Record that a dialog just opened, so it draws above the dialogs already
    /// open and owns the keyboard.
    fn note_modal(&mut self, modal: Modal) {
        self.modal_seq = self.modal_seq.wrapping_add(1);
        self.modal_marks[modal.index()] = self.modal_seq;
    }

    /// The current app theme, flipped from the status bar.
    pub fn theme(&self) -> crate::ui::Theme {
        self.theme
    }

    /// Apply one theme now and persist the choice, so it survives relaunch.
    pub fn set_theme(&mut self, theme: crate::ui::Theme) {
        self.theme = theme;
        crate::ui::apply_theme(&self.ui_ctx, theme);
        self.persist_settings();
    }

    /// Flip dark to light and back, through [`AppState::set_theme`].
    pub fn toggle_theme(&mut self) {
        self.set_theme(self.theme.flipped());
    }

    /// Reload the saved tabs as fresh tabs in file order. A row bound to a
    /// connection that no longer exists is pruned; an unbound row is kept.
    /// Ids are minted, never resumed, so a saved row cannot collide.
    fn restore_tabs(&mut self) {
        let live: HashSet<ConnectionId> = self.connections.iter().map(|c| c.id).collect();
        for saved in load_tabs() {
            let conn = match saved.conn {
                Some(id) if live.contains(&id) => Some(id),
                Some(_) => continue,
                None => None,
            };
            self.push_tab(conn, saved.sql, saved.kind);
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

    /// The cached count state for one table, if its count was requested.
    pub fn row_count(
        &self,
        conn: ConnectionId,
        schema: &str,
        table: &str,
    ) -> Option<&RowCountState> {
        self.row_counts.get(&(conn, schema.to_owned(), table.to_owned()))
    }

    /// The cached session list for one connection, if it was requested.
    pub fn sessions(&self, conn: ConnectionId) -> Option<&[SessionInfo]> {
        self.sessions.get(&conn).map(Vec::as_slice)
    }
    pub fn sessions_is_loading(&self, conn: ConnectionId) -> bool {
        self.sessions_loading.contains(&conn)
    }
    pub fn sessions_error(&self, conn: ConnectionId) -> Option<&str> {
        self.sessions_errors.get(&conn).map(String::as_str)
    }
    /// The terminate awaiting its prod confirmation, if any.
    pub fn pending_terminate(&self) -> Option<&PendingTerminate> {
        self.pending_terminate.as_ref()
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
                return tab.conn;
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

    /// Whether the tab's connection is live and idle, the condition for the
    /// transaction controls. No connection, a connecting or failed
    /// connection, and a run in flight all read false.
    pub fn transaction_ready(&self, tab: TabId) -> bool {
        self.tab(tab)
            .and_then(|t| t.conn)
            .and_then(|id| self.connections.iter().find(|c| c.id == id))
            .is_some_and(|c| matches!(c.state, ConnectionState::Connected { .. }))
    }

    pub fn new_tab(&mut self, conn: ConnectionId) -> TabId {
        self.push_tab(Some(conn), String::new(), TabKind::Query)
    }

    /// The one tab constructor. Restore reuses it so a reloaded tab starts
    /// exactly like a fresh one: fresh id, `Never` outcome, empty display.
    fn push_tab(&mut self, conn: Option<ConnectionId>, sql: String, kind: TabKind) -> TabId {
        let id = self.ids.tab();
        self.tabs.push(Tab {
            id,
            conn,
            sql,
            kind,
            saved: None,
            outcome: RunOutcome::Never,
            run_scope: RunScope::All,
            results: Vec::new(),
            active_result: 0,
            sort_col: None,
            sort_dir: SortDir::Asc,
            filter: String::new(),
            filter_clauses: Vec::new(),
            filter_raw: String::new(),
            export_notice: None,
            export_failed: false,
            edits: Vec::new(),
            deleted_rows: Vec::new(),
            added_rows: Vec::new(),
            pending_apply: None,
            pending_import: None,
            silent_run: None,
            pending_table: None,
        });
        if self.active.is_none() {
            self.active = Some(id);
        }
        id
    }

    pub fn close_tab(&mut self, tab: TabId) {
        self.tabs.retain(|t| t.id != tab);
        self.clear_tab_confirms(tab);
        self.repair_active();
    }

    pub fn activate(&mut self, tab: TabId) {
        if self.tabs.iter().any(|t| t.id == tab) {
            self.active = Some(tab);
        }
    }

    /// Move the active tab `step` places forward (negative goes back),
    /// wrapping around the ends. No-op with no tabs.
    pub fn activate_relative(&mut self, step: isize) {
        let len = self.tabs.len();
        if len == 0 {
            return;
        }
        let Some(current) = self.active.and_then(|id| self.tabs.iter().position(|t| t.id == id))
        else {
            self.active = Some(self.tabs[0].id);
            return;
        };
        let next = (current as isize + step).rem_euclid(len as isize) as usize;
        self.active = Some(self.tabs[next].id);
    }

    /// Activate the `n`th tab, 1-based. Nine selects the last tab, the
    /// browser rule, whatever the count; other out-of-range values are no-ops.
    pub fn activate_nth(&mut self, n: usize) {
        let index = match n {
            0 => return,
            9 => self.tabs.len().checked_sub(1),
            n => n.checked_sub(1),
        };
        if let Some(index) = index.filter(|index| *index < self.tabs.len()) {
            self.active = Some(self.tabs[index].id);
        }
    }

    /// Resolve the tab's scope against the editor text and send the batch.
    /// An empty effective set is an error before anything reaches the worker.
    /// A batch with placeholders arms the value prompt instead of sending.
    pub fn start_run(
        &mut self,
        tab: TabId,
        sql: &str,
        cursor: usize,
        selection: Option<Range<usize>>,
    ) {
        self.start_batch(tab, sql, cursor, selection, false);
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
        self.start_batch(tab, sql, cursor, selection, true);
    }

    /// Drop this tab's armed confirms. A new batch replaces the statement a
    /// confirm was armed for, so approving afterwards could send text the
    /// operator never saw.
    fn clear_tab_confirms(&mut self, tab: TabId) {
        if self.pending_write.as_ref().is_some_and(|pending| pending.tab == tab) {
            self.pending_write = None;
        }
        if self.pending_params.as_ref().is_some_and(|pending| pending.tab == tab) {
            self.pending_params = None;
        }
        if self.pending_edit_preview == Some(tab) {
            self.pending_edit_preview = None;
        }
        if self.pending_table_confirm.as_ref().is_some_and(|pending| pending.tab == tab) {
            self.pending_table_confirm = None;
        }
    }

    /// The shared body of run and explain. Resolves the tab's connection, its
    /// handle, and the effective statements. A batch with `:name` or `?`
    /// placeholders arms the value prompt and holds instead of sending; a
    /// mixed-style run errors before anything reaches the worker. The rest
    /// continues through [`AppState::continue_batch`]: each statement in its
    /// prepared form, a read-only refusal when any prepared statement writes,
    /// and the write confirm for an UPDATE or DELETE without WHERE.
    /// Placeholder-free runs behave exactly as before.
    fn start_batch(
        &mut self,
        tab: TabId,
        sql: &str,
        cursor: usize,
        selection: Option<Range<usize>>,
        explain: bool,
    ) {
        self.clear_tab_confirms(tab);
        let (conn_id, kind, handle, scope, read_only) =
            match self.tabs.iter().find(|t| t.id == tab) {
                Some(t) => {
                    match t.conn.and_then(|id| self.connections.iter().find(|c| c.id == id)) {
                        Some(c) => {
                            (c.id, c.config.kind(), c.handle(), t.run_scope, c.config.is_read_only())
                        }
                        None => {
                            self.set_outcome(
                                tab,
                                RunOutcome::Err("connection not found".to_owned()),
                            );
                            return;
                        }
                    }
                }
                None => return,
            };
        let statements = effective_statements(sql, kind, scope, cursor, selection);
        if statements.is_empty() {
            self.set_outcome(tab, RunOutcome::Err("nothing to run".to_owned()));
            return;
        }
        match collect_params(&statements, kind) {
            Err(error) => {
                self.set_outcome(tab, RunOutcome::Err(error.to_string()));
                return;
            }
            Ok(params) if !params.is_empty() => {
                self.pending_params = Some(PendingParams {
                    tab,
                    conn: conn_id,
                    explain,
                    statements,
                    params,
                });
                self.note_modal(Modal::Params);
                return;
            }
            Ok(_) => {}
        }
        self.continue_batch(tab, conn_id, kind, handle, read_only, &statements, explain, None);
    }

    /// Send one transaction control as a plain single-statement batch through
    /// [`AppState::continue_batch`], so the read-only guard, the write
    /// confirm, and history behave exactly as a run's. The flag opens or
    /// closes only when the batch lands Ok.
    pub fn start_transaction(&mut self, tab: TabId, action: TransactionAction) {
        let (conn_id, kind, handle, read_only) =
            match self.tabs.iter().find(|t| t.id == tab) {
                Some(t) => {
                    match t.conn.and_then(|id| self.connections.iter().find(|c| c.id == id)) {
                        Some(c) => {
                            (c.id, c.config.kind(), c.handle(), c.config.is_read_only())
                        }
                        None => {
                            self.set_outcome(
                                tab,
                                RunOutcome::Err("connection not found".to_owned()),
                            );
                            return;
                        }
                    }
                }
                None => return,
            };
        self.continue_batch(
            tab,
            conn_id,
            kind,
            handle,
            read_only,
            &[action.statement().to_owned()],
            false,
            None,
        );
    }

    /// Ask the worker to switch autocommit for the tab's connection. The
    /// observed flag moves only when [`DbEvent::AutocommitSet`] lands, the
    /// way the transaction flag moves on a control batch landing. SQLite
    /// has no manual mode, so the toggle is a no-op there.
    pub fn set_autocommit(&mut self, tab: TabId, on: bool) {
        if !self.transaction_ready(tab) {
            return;
        }
        let Some(conn) = self
            .tabs
            .iter()
            .find(|t| t.id == tab)
            .and_then(|t| t.conn)
            .and_then(|id| self.connections.iter().find(|c| c.id == id))
        else {
            return;
        };
        if conn.config.kind() == DbKind::Sqlite {
            return;
        }
        if let Some(handle) = conn.handle() {
            let _ = handle.send(DbCommand::SetAutocommit { on });
        }
    }

    /// The prepared tail of [`AppState::start_batch`]: map each statement
    /// through the run or explain form, then the read-only guard, the write
    /// confirm, and the send. `raw_history` carries the raw placeholder text
    /// when the batch was substituted, so history keeps `:name` and `?`.
    fn continue_batch(
        &mut self,
        tab: TabId,
        conn: ConnectionId,
        kind: DbKind,
        handle: Option<DbHandle>,
        read_only: bool,
        statements: &[String],
        explain: bool,
        raw_history: Option<&[String]>,
    ) {
        self.clear_tab_confirms(tab);
        let prepare = |statement: &String| {
            if explain { explain_sql(kind, statement) } else { statement.clone() }
        };
        let prepared: Vec<String> = statements.iter().map(prepare).collect();
        let history = raw_history.map(|raw| {
            raw.iter()
                .map(|statement| {
                    if explain { explain_sql(kind, statement) } else { statement.clone() }
                })
                .collect()
        });
        if read_only {
            if let Some(kind) =
                prepared.iter().find_map(|statement| crate::db::write_statement_kind(statement))
            {
                self.set_outcome(
                    tab,
                    RunOutcome::Err(
                        crate::db::DbError::Config(format!(
                            "read-only connection refused {kind}"
                        ))
                        .to_string(),
                    ),
                );
                return;
            }
        }
        if let Some(found) =
            prepared.iter().find_map(|statement| crate::db::unqualified_write(statement))
        {
            self.pending_write = Some(PendingWriteConfirm {
                tab,
                conn,
                statements: prepared,
                kind: found.kind.to_owned(),
                table: found.table,
            });
            self.note_modal(Modal::WriteConfirm);
            return;
        }
        self.send_batch(tab, conn, handle, prepared, BatchPurpose::Run, history);
    }

    /// The run held for its parameter values, if any.
    pub fn pending_params(&self) -> Option<&PendingParams> {
        self.pending_params.as_ref()
    }

    /// The held parameters with their typed values, for the prompt to edit.
    pub fn pending_params_mut(&mut self) -> Option<&mut PendingParams> {
        self.pending_params.as_mut()
    }

    /// Substitute the typed values into the held batch and continue through
    /// the exact prepared path, including the write guard after substitution.
    /// History keeps the raw placeholder text.
    pub fn confirm_params(&mut self) {
        let Some(pending) = self.pending_params.take() else { return };
        let (kind, handle, read_only) =
            match self.connections.iter().find(|c| c.id == pending.conn) {
                Some(c) => (c.config.kind(), c.handle(), c.config.is_read_only()),
                None => {
                    self.set_outcome(pending.tab, RunOutcome::Err("connection not found".to_owned()));
                    return;
                }
            };
        let mut next_pos = 0;
        let substituted: Vec<String> = pending
            .statements
            .iter()
            .map(|statement| substitute(statement, &pending.params, &mut next_pos, kind))
            .collect();
        self.continue_batch(
            pending.tab,
            pending.conn,
            kind,
            handle,
            read_only,
            &substituted,
            pending.explain,
            Some(&pending.statements),
        );
    }

    /// Drop the held parameter batch without sending anything.
    pub fn cancel_params(&mut self) {
        self.pending_params = None;
    }

    /// The run held for its write guard, if any.
    pub fn pending_write(&self) -> Option<&PendingWriteConfirm> {
        self.pending_write.as_ref()
    }

    /// Send the held batch exactly as prepared. The connection may have been
    /// flipped read-only while the modal was open, so the guard re-runs here
    /// and refuses with the same error a run reports.
    pub fn confirm_write(&mut self) {
        let Some(pending) = self.pending_write.take() else { return };
        let (handle, read_only) = match self.connections.iter().find(|c| c.id == pending.conn) {
            Some(c) => (c.handle(), c.config.is_read_only()),
            None => (None, false),
        };
        if read_only {
            self.set_outcome(
                pending.tab,
                RunOutcome::Err(
                    crate::db::DbError::Config(format!(
                        "read-only connection refused {}",
                        pending.kind
                    ))
                    .to_string(),
                ),
            );
            return;
        }
        self.send_batch(pending.tab, pending.conn, handle, pending.statements, BatchPurpose::Run, None);
    }

    /// Drop the held batch without sending anything.
    pub fn cancel_write(&mut self) {
        self.pending_write = None;
    }

    /// One prepared batch: mint the run, send it, and fold the tab's outcome
    /// and the connection's running state. `purpose` marks the batch so the
    /// landing knows what it ended. `history` carries the raw placeholder
    /// text when the sent batch was substituted, so history keeps `:name`
    /// and `?`; None stores the sent batch as before.
    fn send_batch(
        &mut self,
        tab: TabId,
        conn_id: ConnectionId,
        handle: Option<DbHandle>,
        statements: Vec<String>,
        purpose: BatchPurpose,
        history: Option<Vec<String>>,
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
                                BatchPurpose::Import(notice) => {
                                    t.pending_import = Some((run, notice));
                                }
                                BatchPurpose::Refresh => t.silent_run = Some(run),
                                BatchPurpose::TableDdl => t.pending_table = Some(run),
                            }
                        }
                        self.set_outcome(
                            tab,
                            RunOutcome::Running {
                                run,
                                started: Instant::now(),
                                statements: history.unwrap_or(statements),
                            },
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
            let imported = t
                .pending_import
                .as_ref()
                .is_some_and(|(pending, _)| *pending == run);
            if imported {
                t.pending_import = None;
                t.clear_export_notice();
            }
            if t.silent_run == Some(run) {
                t.silent_run = None;
            }
            if t.pending_table == Some(run) {
                t.pending_table = None;
            }
        }
        if let Some(conn_id) = conn_id {
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
    }

    /// What the tab's grid can edit, if anything. Some only while the
    /// connection is idle, the tab's SQL is a SELECT the loaded schema keys,
    /// and every key column appears in the active result. A single-table
    /// SELECT edits every column its SELECT list names, while an expression
    /// column stays read-only; a joined SELECT edits each mapped column
    /// through its own source table while ambiguous or keyless columns stay
    /// read-only.
    pub fn edit_target(&self, tab: TabId) -> Option<EditTarget> {
        let tab = self.tabs.iter().find(|t| t.id == tab)?;
        let conn = tab.conn?;
        let idle = matches!(
            self.connections.iter().find(|c| c.id == conn)?.state(),
            ConnectionState::Connected { .. }
        );
        if !idle {
            return None;
        }
        if let Some(single) = self.single_edit_target(conn, &tab.sql, tab.active_query_result()?) {
            return Some(single);
        }
        self.join_edit_target(conn, &tab.sql, tab.active_query_result()?)
    }

    /// The single-table target behind `editable_table`, if its table carries
    /// a primary key with every key column in the result. Each result column
    /// maps to the source column its SELECT item names, so an alias still
    /// writes the source; a wildcard falls back to the result names.
    fn single_edit_target(
        &self,
        conn: ConnectionId,
        sql: &str,
        result: &QueryResult,
    ) -> Option<EditTarget> {
        let kind = self.connections.iter().find(|c| c.id == conn)?.config.kind();
        let table = editable_table(sql, kind)?;
        let schema = self.schemas.get(&conn)?;
        let pk = primary_key(schema, &table)?;
        let info = find_table(schema, &table);
        let names = result.columns();
        // A named SELECT list maps each item to its source column, so an
        // alias like `name AS label` still writes `name`. A wildcard has no
        // item names, and the result columns already carry the table's.
        let mut columns: Vec<Option<String>> = match parse_join(sql, kind) {
            Some(plan)
                if plan.sources.len() == 1
                    && plan.selects.len() == names.len()
                    && !wildcard_select(sql, kind) =>
            {
                plan.selects.iter().map(|select| select.column.clone()).collect()
            }
            _ => names.iter().map(|column| Some(column.name().to_owned())).collect(),
        };
        if let Some(info) = info {
            for slot in &mut columns {
                if let Some(name) = slot
                    && !info.columns().iter().any(|c| c.name().eq_ignore_ascii_case(name))
                {
                    *slot = None;
                }
            }
        }
        if columns.iter().all(|slot| slot.is_none()) {
            return None;
        }
        let mut pk_cols = Vec::with_capacity(pk.len());
        let mut pk_types = Vec::with_capacity(pk.len());
        for (column, type_name) in pk {
            let index = columns.iter().position(|slot| {
                slot.as_deref().is_some_and(|name| name.eq_ignore_ascii_case(&column))
            })?;
            pk_cols.push((column, index));
            pk_types.push(is_text_type(&type_name));
        }
        Some(EditTarget {
            table,
            pk_cols,
            pk_types,
            join_tables: Vec::new(),
            join_columns: Vec::new(),
            columns,
        })
    }

    /// The joined target behind `parse_join`: each result column maps
    /// positionally to its SELECT item, each qualifier resolves through its
    /// alias or table name, and each bare column resolves to the one source
    /// table carrying it. A column stays read-only when its item is an
    /// expression, its qualifier matches no table or several, its name is in
    /// no table or several, its source column is unknown, or its table's
    /// keys are missing from the result. None when no column resolves.
    fn join_edit_target(
        &self,
        conn: ConnectionId,
        sql: &str,
        result: &QueryResult,
    ) -> Option<EditTarget> {
        let kind = self.connections.iter().find(|c| c.id == conn)?.config.kind();
        let plan = parse_join(sql, kind)?;
        if plan.selects.len() != result.columns().len() {
            return None;
        }
        let schema = self.schemas.get(&conn)?;
        let mut resolved: Vec<Option<(usize, String)>> = Vec::with_capacity(plan.selects.len());
        for select in &plan.selects {
            let column = match &select.column {
                Some(column) => column,
                None => {
                    resolved.push(None);
                    continue;
                }
            };
            if let Some(qualifier) = &select.qualifier {
                match resolve_qualifier(&plan, qualifier) {
                    Some(index) => resolved.push(Some((index, column.clone()))),
                    None => resolved.push(None),
                }
            } else {
                match resolve_bare_column(schema, &plan, column) {
                    Some(index) => resolved.push(Some((index, column.clone()))),
                    None => resolved.push(None),
                }
            }
        }
        let mut tables: Vec<JoinTableTarget> = Vec::new();
        let mut table_index_of_source: Vec<Option<usize>> = vec![None; plan.sources.len()];
        let mut columns: Vec<Option<JoinColumnTarget>> = Vec::with_capacity(resolved.len());
        for slot in &resolved {
            let Some((source_index, column)) = slot else {
                columns.push(None);
                continue;
            };
            let source_table = &plan.sources[*source_index].table;
            let Some(info) = find_table(schema, source_table) else {
                columns.push(None);
                continue;
            };
            if !info.columns().iter().any(|c| c.name().eq_ignore_ascii_case(column)) {
                columns.push(None);
                continue;
            }
            let Some(pk) = primary_key(schema, source_table) else {
                columns.push(None);
                continue;
            };
            let mut key_cols = Vec::with_capacity(pk.len());
            let mut key_types = Vec::with_capacity(pk.len());
            let mut keyed = true;
            for (key_name, key_type) in &pk {
                let mut found = None;
                for (index, other) in resolved.iter().enumerate() {
                    if let Some((other_source, other_column)) = other
                        && *other_source == *source_index
                        && other_column.eq_ignore_ascii_case(key_name)
                    {
                        found = Some(index);
                        break;
                    }
                }
                match found {
                    Some(index) => {
                        key_cols.push((key_name.clone(), index));
                        key_types.push(is_text_type(key_type));
                    }
                    None => {
                        keyed = false;
                        break;
                    }
                }
            }
            if !keyed {
                columns.push(None);
                continue;
            }
            let table_index = match table_index_of_source[*source_index] {
                Some(index) => {
                    tables[index].pk_cols = key_cols.clone();
                    tables[index].pk_types = key_types.clone();
                    index
                }
                None => {
                    tables.push(JoinTableTarget {
                        table: source_table.clone(),
                        pk_cols: key_cols,
                        pk_types: key_types,
                    });
                    let index = tables.len() - 1;
                    table_index_of_source[*source_index] = Some(index);
                    index
                }
            };
            columns.push(Some(JoinColumnTarget { table_index, column: column.clone() }));
        }
        if columns.iter().all(|slot| slot.is_none()) {
            return None;
        }
        let display = tables.iter().map(|t| t.table.clone()).collect::<Vec<_>>().join(", ");
        Some(EditTarget {
            table: display,
            pk_cols: Vec::new(),
            pk_types: Vec::new(),
            join_tables: tables,
            join_columns: columns,
            columns: Vec::new(),
        })
    }

    /// Stage one cell edit. `row` counts result rows first, then staged added
    /// rows. No-op unless the target resolves and the cell is in the active
    /// result or a staged added row. A column whose SELECT item is an
    /// expression stages nothing; deletes and inserts stay single-table only.
    /// A later stage on the same cell replaces its text. No-op while the SQL
    /// preview is open, so the previewed statements cannot change underneath
    /// the modal.
    pub fn stage_edit(&mut self, tab: TabId, row: usize, col: usize, text: String) {
        if self.pending_edit_preview.is_some() {
            return;
        }
        let Some(target) = self.edit_target(tab) else { return };
        if !target.is_column_editable(col) {
            return;
        }
        let Some((rows, cols)) = self.result_shape(tab) else { return };
        if col >= cols {
            return;
        }
        if target.is_join() && row >= rows {
            return;
        }
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

    /// Drop every staged change on the tab. Clears its SQL preview as well,
    /// since there is nothing left to confirm.
    pub fn discard_edits(&mut self, tab: TabId) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
            t.edits.clear();
            t.deleted_rows.clear();
            t.added_rows.clear();
        }
        if self.pending_edit_preview == Some(tab) {
            self.pending_edit_preview = None;
        }
    }

    /// Stage a DELETE for one result row. Marking the row again restores it.
    /// Single-table only: a joined target never deletes, since the row spans
    /// tables. No-op unless the target resolves and the row is in the active
    /// result. No-op while the SQL preview is open, the way staging is.
    pub fn mark_row_deleted(&mut self, tab: TabId, row: usize) {
        if self.pending_edit_preview.is_some() {
            return;
        }
        if self.edit_target(tab).is_some_and(|target| target.is_join()) {
            return;
        }
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
    /// Single-table only: a joined result never inserts, since the row spans
    /// tables. No-op unless the target resolves. No-op while the SQL preview
    /// is open, the way staging is.
    pub fn add_row(&mut self, tab: TabId) {
        if self.pending_edit_preview.is_some() {
            return;
        }
        if self.edit_target(tab).is_some_and(|target| target.is_join()) {
            return;
        }
        if self.edit_target(tab).is_none() {
            return;
        }
        let Some((_, cols)) = self.result_shape(tab) else { return };
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) else { return };
        t.added_rows.push(vec![String::new(); cols]);
    }

    /// Preview the staged changes as the exact SQL the apply would send.
    /// Some only while the target resolves and the result reads.
    pub fn preview_edit_statements(&self, tab: TabId) -> Option<Vec<String>> {
        let target = self.edit_target(tab)?;
        self.edit_statements(tab, &target)
    }

    /// The tab whose staged edits await their SQL preview, if any.
    pub fn pending_edit_preview(&self) -> Option<TabId> {
        self.pending_edit_preview
    }

    /// Arm the SQL preview for the staged changes instead of sending. An
    /// empty stage or an unresolvable target arms nothing. A read-only
    /// connection refuses at once with the same error a run reports, so no
    /// preview opens it cannot send. The grid Apply button enters here, and
    /// the preview modal confirms from here.
    pub fn apply_edits(&mut self, tab: TabId) {
        if self.pending_edit_preview == Some(tab) {
            return;
        }
        let Some(target) = self.edit_target(tab) else { return };
        let Some(statements) = self.edit_statements(tab, &target) else { return };
        if statements.is_empty() {
            return;
        }
        let read_only = match self.tabs.iter().find(|t| t.id == tab) {
            Some(t) => {
                match t.conn.and_then(|id| self.connections.iter().find(|c| c.id == id)) {
                    Some(c) => c.config.is_read_only(),
                    None => return,
                }
            }
            None => return,
        };
        if read_only {
            let kind = statements
                .iter()
                .find_map(|statement| crate::db::write_statement_kind(statement))
                .unwrap_or("write");
            self.set_outcome(
                tab,
                RunOutcome::Err(
                    crate::db::DbError::Config(format!("read-only connection refused {kind}"))
                        .to_string(),
                ),
            );
            return;
        }
        self.pending_edit_preview = Some(tab);
        self.note_modal(Modal::EditPreview);
    }

    /// Send the previewed batch exactly as the apply path would: an UPDATE
    /// per dirty row, a DELETE per deleted row, then an INSERT per added
    /// row. The statements re-resolve through the same builder the preview
    /// reads, so the modal and the worker see the same text. Staging is
    /// locked while the preview is open, so the two cannot drift.
    pub fn confirm_pending_edits(&mut self) {
        let Some(tab) = self.pending_edit_preview.take() else { return };
        let Some(target) = self.edit_target(tab) else { return };
        let Some(statements) = self.edit_statements(tab, &target) else { return };
        if statements.is_empty() {
            return;
        }
        let (conn_id, handle, read_only) = match self.tabs.iter().find(|t| t.id == tab) {
            Some(t) => {
                match t.conn.and_then(|id| self.connections.iter().find(|c| c.id == id)) {
                    Some(c) => (c.id, c.handle(), c.config.is_read_only()),
                    None => return,
                }
            }
            None => return,
        };
        if read_only {
            let kind = statements
                .iter()
                .find_map(|statement| crate::db::write_statement_kind(statement))
                .unwrap_or("write");
            self.set_outcome(
                tab,
                RunOutcome::Err(
                    crate::db::DbError::Config(format!("read-only connection refused {kind}"))
                        .to_string(),
                ),
            );
            return;
        }
        self.send_batch(tab, conn_id, handle, statements, BatchPurpose::ApplyEdits, None);
    }

    /// Drop the SQL preview without sending anything. Staged edits stay.
    pub fn cancel_pending_edits(&mut self) {
        self.pending_edit_preview = None;
    }

    /// Open a blank create-table form for the connection. No-op for an
    /// unknown connection.
    pub fn open_table_create(&mut self, conn: ConnectionId) {
        if self.connections.iter().find(|c| c.id == conn).is_none() {
            return;
        }
        self.table_editor = Some(TableEditorState {
            conn,
            original: None,
            form: crate::ui::ddl::TableForm::blank(),
        });
        self.pending_table_confirm = None;
        self.note_modal(Modal::TableEditor);
    }

    /// Open the alter-table form prefilled from the loaded schema. No-op
    /// when the connection or the table is unknown.
    pub fn open_table_alter(&mut self, conn: ConnectionId, schema: &str, table: &str) {
        let info = self.schemas.get(&conn).and_then(|info| {
            info.databases().iter().flat_map(|db| db.schemas()).find_map(|node| {
                if !node.name().eq_ignore_ascii_case(schema) {
                    return None;
                }
                node.tables()
                    .iter()
                    .find(|t| t.name().eq_ignore_ascii_case(table))
                    .cloned()
            })
        });
        let Some(info) = info else { return };
        if self.connections.iter().find(|c| c.id == conn).is_none() {
            return;
        }
        let form = crate::ui::ddl::TableForm::from_table(schema, &info);
        self.table_editor = Some(TableEditorState { conn, original: Some(info), form });
        self.pending_table_confirm = None;
        self.note_modal(Modal::TableEditor);
    }

    /// Close the table editor without sending anything. A held prod
    /// confirmation goes with it.
    pub fn close_table_editor(&mut self) {
        self.table_editor = None;
        self.pending_table_confirm = None;
    }

    /// The open table editor, if any.
    pub fn table_editor(&self) -> Option<&TableEditorState> {
        self.table_editor.as_ref()
    }

    /// The open table editor for editing.
    pub fn table_editor_mut(&mut self) -> Option<&mut TableEditorState> {
        self.table_editor.as_mut()
    }

    /// The editor's SQL preview: the create statements for a new table,
    /// the alter diff for a loaded one. None without an editor or a
    /// connection whose dialect is known.
    pub fn table_ddl_statements(&self) -> Option<Vec<String>> {
        let editor = self.table_editor.as_ref()?;
        let kind = self.connections.iter().find(|c| c.id == editor.conn)?.config.kind();
        match &editor.original {
            Some(original) => Some(crate::ui::ddl::alter_table_statements(
                original, &editor.form, kind,
            )),
            None => Some(crate::ui::ddl::create_table_statements(&editor.form, kind)),
        }
    }

    /// The editor's validation errors. Empty means the preview can apply.
    pub fn table_ddl_errors(&self) -> Vec<String> {
        match self.table_editor.as_ref() {
            Some(editor) => crate::ui::ddl::validate_table_form(&editor.form),
            None => vec!["no table editor open".to_owned()],
        }
    }

    /// Send the editor's preview through the batch path. Validation failures
    /// and an empty diff land as the tab error; a read-only connection
    /// refuses the write kind like every other write path; a prod connection
    /// holds the batch for confirmation instead of sending. The tab must be
    /// bound to the editor's connection.
    pub fn apply_table_ddl(&mut self, tab: TabId) {
        let errors = self.table_ddl_errors();
        if !errors.is_empty() {
            self.set_outcome(tab, RunOutcome::Err(errors.join("; ")));
            return;
        }
        let (conn_id, statements) = match self.table_editor.as_ref() {
            Some(editor) => match self.table_ddl_statements() {
                Some(statements) if !statements.is_empty() => (editor.conn, statements),
                _ => {
                    self.set_outcome(
                        tab,
                        RunOutcome::Err(
                            "nothing to apply: the form matches the table".to_owned(),
                        ),
                    );
                    return;
                }
            },
            None => return,
        };
        let bound = self.tabs.iter().find(|t| t.id == tab).and_then(|t| t.conn);
        if bound != Some(conn_id) {
            self.set_outcome(tab, RunOutcome::Err("table editor connection does not match the tab".to_owned()));
            return;
        }
        let (handle, read_only, prod) =
            match self.connections.iter().find(|c| c.id == conn_id) {
                Some(c) => (c.handle(), c.config.is_read_only(), c.env == Environment::Prod),
                None => return,
            };
        if read_only {
            let kind = statements
                .iter()
                .find_map(|statement| crate::db::write_statement_kind(statement))
                .unwrap_or("write");
            self.set_outcome(
                tab,
                RunOutcome::Err(
                    crate::db::DbError::Config(format!("read-only connection refused {kind}"))
                        .to_string(),
                ),
            );
            return;
        }
        if prod {
            self.pending_table_confirm =
                Some(PendingTableDdl { conn: conn_id, tab, statements });
            self.note_modal(Modal::TableDdl);
            return;
        }
        self.send_batch(tab, conn_id, handle, statements, BatchPurpose::TableDdl, None);
    }

    /// Send the held prod table DDL exactly as previewed.
    pub fn confirm_table_ddl(&mut self) {
        let Some(pending) = self.pending_table_confirm.take() else { return };
        let handle = self
            .connections
            .iter()
            .find(|c| c.id == pending.conn)
            .and_then(|c| c.handle());
        self.send_batch(
            pending.tab,
            pending.conn,
            handle,
            pending.statements,
            BatchPurpose::TableDdl,
            None,
        );
    }

    /// Drop the held prod table DDL without sending anything.
    pub fn cancel_table_ddl(&mut self) {
        self.pending_table_confirm = None;
    }

    /// The table DDL batch held for its prod confirmation, if any.
    pub fn pending_table_ddl(&self) -> Option<&PendingTableDdl> {
        self.pending_table_confirm.as_ref()
    }

    /// Read and parse a CSV or JSONL file for one tab and stage it for
    /// confirmation. The extension names the parser: `.jsonl` reads through
    /// the JSONL parser, anything else through the CSV one. Failure lands in
    /// the tab's notice: an unreadable file, a file that is not one column
    /// count (CSV) or one JSON object per line (JSONL), or a tab the edit
    /// path cannot resolve. Joined results never import, since the row spans
    /// tables.
    pub fn begin_import(&mut self, tab: TabId, path: &std::path::Path) {
        let Some(target) = self.edit_target(tab) else {
            self.set_tab_notice(tab, "Import failed: the table is not editable".to_owned(), true);
            return;
        };
        if target.is_join() {
            self.set_tab_notice(tab, "Import failed: the table is not editable".to_owned(), true);
            return;
        }
        let conn = match self.tabs.iter().find(|t| t.id == tab) {
            Some(t) => match t.conn {
                Some(conn) => conn,
                None => return,
            },
            None => return,
        };
        let Some(columns) = self
            .schemas
            .get(&conn)
            .and_then(|schema| table_columns(schema, &target.table))
        else {
            self.set_tab_notice(
                tab,
                "Import failed: the table columns are not loaded".to_owned(),
                true,
            );
            return;
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                self.set_tab_notice(
                    tab,
                    format!("Import failed: {}: {error}", path.display()),
                    true,
                );
                return;
            }
        };
        let source = ImportSource::of_path(path);
        let parsed = match source {
            ImportSource::Csv => parse_csv(&text),
            ImportSource::Jsonl => parse_jsonl(&text),
        };
        let Some(parsed) = parsed else {
            let message = match source {
                ImportSource::Csv => {
                    "Import failed: the file needs a header and rows of one column count"
                }
                ImportSource::Jsonl => {
                    "Import failed: the file needs one JSON object per line"
                }
            };
            self.set_tab_notice(tab, message.to_owned(), true);
            return;
        };
        let map = map_headers(&parsed.headers, &columns);
        self.imports = Some(ImportDraft {
            tab,
            headers: parsed.headers,
            rows: parsed.rows,
            map,
            table: target.table,
            source,
        });
        self.note_modal(Modal::Import);
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
            t.clear_export_notice();
        }
    }

    /// Send the pending import as one batch through the edit write path: it
    /// adds no history entry and the tab re-reads itself when it lands.
    /// Nothing sends without a draft.
    pub fn commit_import(&mut self) {
        let Some(draft) = self.imports.take() else { return };
        if self.edit_target(draft.tab).is_none_or(|target| target.is_join()) {
            self.set_tab_notice(
                draft.tab,
                "Import failed: the table is not editable".to_owned(),
                true,
            );
            return;
        }
        let conn = self.tabs.iter().find(|t| t.id == draft.tab).and_then(|t| t.conn);
        let text_cols: Vec<bool> = conn
            .and_then(|conn| self.schemas.get(&conn))
            .map(|schema| {
                draft
                    .map
                    .columns
                    .iter()
                    .map(|column| table_column_is_text(schema, &draft.table, column))
                    .collect()
            })
            .unwrap_or_default();
        let statements = insert_typed_batches(&draft.table, &draft.map, &draft.rows, &text_cols);
        if statements.is_empty() {
            let noun = match draft.source {
                ImportSource::Csv => "CSV header",
                ImportSource::Jsonl => "JSONL key",
            };
            self.set_tab_notice(
                draft.tab,
                format!("Import failed: no {noun} matches {}", draft.table),
                true,
            );
            return;
        }
        let (conn_id, handle, read_only) = match self.tabs.iter().find(|t| t.id == draft.tab) {
            Some(t) => {
                match t.conn.and_then(|id| self.connections.iter().find(|c| c.id == id)) {
                    Some(c) => (c.id, c.handle(), c.config.is_read_only()),
                    None => return,
                }
            }
            None => return,
        };
        if read_only {
            let kind = statements
                .iter()
                .find_map(|statement| crate::db::write_statement_kind(statement))
                .unwrap_or("write");
            self.set_tab_notice(
                draft.tab,
                crate::db::DbError::Config(format!("read-only connection refused {kind}"))
                    .to_string(),
                true,
            );
            return;
        }
        let inserted = draft.rows.len().min(ROW_CAP);
        let skipped = draft.map.skipped.len();
        let dropped = dropped_rows(draft.rows.len());
        let notice = format!(
            "Imported {inserted} rows into {} ({skipped} skipped, {dropped} dropped)",
            draft.table
        );
        self.send_batch(
            draft.tab,
            conn_id,
            handle,
            statements,
            BatchPurpose::Import(notice),
            None,
        );
    }

    /// Drop the pending import without sending anything.
    pub fn cancel_import(&mut self) {
        self.imports = None;
    }

    /// The pending import awaiting its confirmation, if any.
    pub fn import_draft(&self) -> Option<&ImportDraft> {
        self.imports.as_ref()
    }

    fn set_tab_notice(&mut self, tab: TabId, notice: String, is_error: bool) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
            t.set_export_notice(notice, is_error);
        }
    }

    /// The same notice on every tab bound to the connection, for events
    /// about the connection rather than one tab.
    fn notice_connection(&mut self, conn: ConnectionId, notice: String, is_error: bool) {
        let tabs: Vec<TabId> = self
            .tabs
            .iter()
            .filter(|t| t.conn == Some(conn))
            .map(|t| t.id)
            .collect();
        for tab in tabs {
            self.set_tab_notice(tab, notice.clone(), is_error);
        }
    }

    /// Rows and columns of the tab's active result. None when there is none.
    fn result_shape(&self, tab: TabId) -> Option<(usize, usize)> {
        let result = self.tabs.iter().find(|t| t.id == tab)?.active_query_result()?;
        Some((result.row_count(), result.columns().len()))
    }

    /// The statements for the staged changes, in apply order. None when the
    /// displayed result cannot be read. A single-table target sends one
    /// UPDATE per dirty row plus deletes and inserts; a joined target sends
    /// one UPDATE per dirty row per touched source table and never deletes
    /// or inserts.
    fn edit_statements(&self, tab: TabId, target: &EditTarget) -> Option<Vec<String>> {
        if target.is_join() {
            return self.join_edit_statements(tab, target);
        }
        let t = self.tabs.iter().find(|t| t.id == tab)?;
        let result = t.active_query_result()?;
        let schema = t.conn.and_then(|conn| self.schemas.get(&conn));
        let columns: Vec<String> = (0..result.columns().len())
            .map(|col| {
                target
                    .columns
                    .get(col)
                    .and_then(|slot| slot.as_deref())
                    .unwrap_or_else(|| result.columns()[col].name())
                    .to_owned()
            })
            .collect();
        let text_cols: Vec<bool> = columns
            .iter()
            .map(|column| {
                schema.is_some_and(|schema| {
                    table_column_is_text(schema, &target.table, column)
                })
            })
            .collect();
        let mut statements = Vec::new();
        let mut dirty_rows: Vec<usize> = t.edits.iter().map(|e| e.row).collect();
        dirty_rows.sort_unstable();
        dirty_rows.dedup();
        for row in dirty_rows {
            if t.is_row_deleted(row) {
                continue;
            }
            let mut sets: Vec<(String, String, bool)> = Vec::new();
            for col in 0..columns.len() {
                if let Some(text) = t.edit_text(row, col) {
                    sets.push((columns[col].clone(), text.to_owned(), text_cols[col]));
                }
            }
            if sets.is_empty() {
                continue;
            }
            let pk = key_pairs(result, target, row)?;
            statements.push(update_typed_statement(&target.table, &pk, &sets));
        }
        for row in &t.deleted_rows {
            statements.push(delete_statement(&target.table, &key_pairs(result, target, *row)?));
        }
        for added in &t.added_rows {
            statements.push(insert_typed_statement(&target.table, &columns, added, &text_cols));
        }
        Some(statements)
    }

    /// One UPDATE per dirty row per touched source table. A staged edit on a
    /// read-only column never reaches here, since staging drops it; a row
    /// with only such edits sends nothing.
    fn join_edit_statements(&self, tab: TabId, target: &EditTarget) -> Option<Vec<String>> {
        let t = self.tabs.iter().find(|t| t.id == tab)?;
        let result = t.active_query_result()?;
        let schema = t.conn.and_then(|conn| self.schemas.get(&conn));
        let mut statements = Vec::new();
        let mut dirty_rows: Vec<usize> = t.edits.iter().map(|e| e.row).collect();
        dirty_rows.sort_unstable();
        dirty_rows.dedup();
        for row in dirty_rows {
            if row >= result.row_count() || t.is_row_deleted(row) {
                continue;
            }
            for (table_index, join_table) in target.join_tables.iter().enumerate() {
                let mut sets: Vec<(String, String, bool)> = Vec::new();
                for (col, slot) in target.join_columns.iter().enumerate() {
                    let Some(mapping) = slot else { continue };
                    if mapping.table_index != table_index {
                        continue;
                    }
                    if let Some(text) = t.edit_text(row, col) {
                        let is_text = schema.is_some_and(|schema| {
                            table_column_is_text(schema, &join_table.table, &mapping.column)
                        });
                        sets.push((mapping.column.clone(), text.to_owned(), is_text));
                    }
                }
                if sets.is_empty() {
                    continue;
                }
                let pk = join_key_pairs(result, join_table, row)?;
                statements.push(update_typed_statement(&join_table.table, &pk, &sets));
            }
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
        let (conn_id, kind, handle) = match self.tabs.iter().find(|t| t.id == tab) {
            Some(t) => {
                match t.conn.and_then(|id| self.connections.iter().find(|c| c.id == id)) {
                    Some(c) => (c.id, c.config.kind(), c.handle()),
                    None => return,
                }
            }
            None => return,
        };
        let statements = effective_statements(&sql, kind, RunScope::All, 0, None);
        if statements.is_empty() {
            return;
        }
        self.send_batch(tab, conn_id, handle, statements, BatchPurpose::Refresh, None);
    }

    /// Connect from a user gesture. A prod connection arms the confirmation
    /// instead of spawning; only the confirmed path reaches the spawn.
    pub fn connect(&mut self, id: ConnectionId) {
        if self.prod_needs_confirm(id) {
            self.pending_prod = Some(id);
            self.note_modal(Modal::ProdConnect);
            return;
        }
        self.connect_now(id);
    }

    /// The spawn path. Confirmation and password continuations enter here so
    /// a confirmed prod connection is never re-armed.
    fn connect_now(&mut self, id: ConnectionId) {
        let (config, password, ssh_password) =
            match self.connections.iter().find(|c| c.id == id) {
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
                    let ssh_password = if c.ssh_password.is_empty() {
                        None
                    } else {
                        Some(c.ssh_password.clone())
                    };
                    (c.config.clone(), password, ssh_password)
                }
                None => return,
            };
        if let Err(error) = config.validate() {
            if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
                c.state = ConnectionState::Error { message: error.to_string() };
            }
            return;
        }
        let handle = spawn_session(
            id,
            config,
            password,
            ssh_password,
            self.ui_ctx.clone(),
            self.events_tx.clone(),
            self.tx_idle,
        );
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
    /// including the password prompt when no password is stored.
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

    /// Memory-only SSH secret update (a password or key passphrase). Never
    /// persisted.
    pub fn set_ssh_password(&mut self, id: ConnectionId, password: String) {
        if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
            c.ssh_password = password;
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
            self.note_modal(Modal::ProdConnect);
            return;
        }
        self.prompt_or_connect(id);
    }

    /// Prompt when a password engine has no password stored, else spawn.
    fn prompt_or_connect(&mut self, id: ConnectionId) {
        let should_prompt = match self.connections.iter().find(|c| c.id == id) {
            Some(c) => c.config.prompts_for_password() && c.password.is_empty(),
            None => return,
        };
        if should_prompt {
            self.password_prompt = Some(id);
            self.note_modal(Modal::PasswordPrompt);
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
            c.transaction = Transaction::Closed;
        }
        self.schemas.remove(&id);
        self.schema_loading.remove(&id);
        self.schema_errors.remove(&id);
        self.row_counts.retain(|(conn, _, _), _| *conn != id);
        self.sessions.remove(&id);
        self.sessions_loading.remove(&id);
        self.sessions_errors.remove(&id);
        if self.pending_terminate.as_ref().is_some_and(|pending| pending.conn == id) {
            self.pending_terminate = None;
        }
    }

    pub fn delete_connection(&mut self, id: ConnectionId) {
        self.disconnect(id);
        self.tabs.retain(|t| t.conn != Some(id));
        self.connections.retain(|c| c.id != id);
        self.schemas.remove(&id);
        self.schema_loading.remove(&id);
        self.schema_errors.remove(&id);
        if self.bookmarks.iter().any(|b| b.conn == id) {
            self.bookmarks.retain(|b| b.conn != id);
            self.persist_bookmarks();
        }
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

    /// Ask the worker for one table's row count. A click while one is in
    /// flight is a no-op; the answer caches until the session ends.
    pub fn request_row_count(&mut self, conn: ConnectionId, schema: &str, table: &str) {
        let key = (conn, schema.to_owned(), table.to_owned());
        if matches!(self.row_counts.get(&key), Some(RowCountState::Pending)) {
            return;
        }
        let handle = match self.connections.iter().find(|c| c.id == conn) {
            Some(c) => match &c.state {
                ConnectionState::Connected { session }
                | ConnectionState::QueryRunning { session, .. } => session.handle.clone(),
                _ => return,
            },
            None => return,
        };
        self.row_counts.insert(key.clone(), RowCountState::Pending);
        let sent = handle.send(DbCommand::RowCount {
            schema: schema.to_owned(),
            table: table.to_owned(),
        });
        if sent.is_err() {
            self.row_counts.insert(key, RowCountState::Failed("connection is gone".to_owned()));
        }
    }

    /// Ask the worker for the live backends. A click while one is in flight
    /// is a no-op; the answer caches until the session ends.
    pub fn request_sessions(&mut self, conn: ConnectionId) {
        if self.sessions_loading.contains(&conn) {
            return;
        }
        let handle = match self.connections.iter().find(|c| c.id == conn) {
            Some(c) => match &c.state {
                ConnectionState::Connected { session }
                | ConnectionState::QueryRunning { session, .. } => session.handle.clone(),
                _ => return,
            },
            None => return,
        };
        self.sessions_loading.insert(conn);
        self.sessions_errors.remove(&conn);
        if handle.send(DbCommand::ListSessions).is_err() {
            self.sessions_loading.remove(&conn);
            self.sessions_errors.insert(conn, "connection is gone".to_owned());
        }
    }

    /// Terminate one backend by its session list id. Read-only connections
    /// never terminate: the request records an error and sends nothing. A
    /// prod connection arms the confirmation instead of sending; only the
    /// confirmed path reaches the worker.
    pub fn request_terminate(&mut self, conn: ConnectionId, backend: &str) {
        let (handle, read_only, prod) = match self.connections.iter().find(|c| c.id == conn) {
            Some(c) => (c.handle(), c.config.is_read_only(), c.env == Environment::Prod),
            None => return,
        };
        if read_only {
            self.sessions_errors
                .insert(conn, "read-only connection cannot terminate sessions".to_owned());
            return;
        }
        if prod {
            self.pending_terminate =
                Some(PendingTerminate { conn, backend: backend.to_owned() });
            self.note_modal(Modal::Terminate);
            return;
        }
        self.send_terminate(conn, backend.to_owned(), handle);
    }

    /// Send the held terminate exactly as requested.
    pub fn confirm_terminate(&mut self) {
        let Some(pending) = self.pending_terminate.take() else { return };
        let handle = self
            .connections
            .iter()
            .find(|c| c.id == pending.conn)
            .and_then(|c| c.handle());
        self.send_terminate(pending.conn, pending.backend, handle);
    }

    /// Drop the held terminate without sending anything.
    pub fn cancel_terminate(&mut self) {
        self.pending_terminate = None;
    }

    /// One terminate send: the worker answers with `TerminateOk`, which
    /// re-reads the list, or `TerminateErr`, which names the failure.
    fn send_terminate(
        &mut self,
        conn: ConnectionId,
        backend: String,
        handle: Option<DbHandle>,
    ) {
        match handle {
            Some(handle) => {
                if handle.send(DbCommand::TerminateSession { backend }).is_err() {
                    self.sessions_errors.insert(conn, "connection is gone".to_owned());
                }
            }
            None => {
                self.sessions_errors.insert(conn, "connection is gone".to_owned());
            }
        }
    }

    /// The tab's SQL rebuilt as a filtered table SELECT: the single table its
    /// SQL names with the tab's clauses and raw box composed into the WHERE.
    /// None when the SQL is not a single-table SELECT.
    pub fn table_filter_select(&self, tab: TabId) -> Option<String> {
        let current = self.tabs.iter().find(|t| t.id == tab)?;
        let kind = self.connections.iter().find(|c| Some(c.id) == current.conn)?.config.kind();
        let table = editable_table(&current.sql, kind)?;
        Some(filtered_table_select(
            &table,
            &current.filter_clauses,
            &current.filter_raw,
        ))
    }

    /// Re-run the tab through the normal batch path with its structured
    /// filter composed into the table SELECT. The tab's SQL becomes the
    /// filtered SELECT, so apply, refresh, and history behave like any run.
    /// No-op when the SQL names no single table.
    pub fn apply_table_filter(&mut self, tab: TabId) {
        let Some(sql) = self.table_filter_select(tab) else { return };
        if let Some(slot) = self.tab_sql_mut(tab) {
            *slot = sql.clone();
        }
        let len = sql.len();
        self.start_run(tab, &sql, len, None);
    }

    /// Drop the tab's structured clauses and raw box. The grid re-reads on
    /// the next apply.
    pub fn clear_table_filter(&mut self, tab: TabId) {
        if let Some(current) = self.tabs.iter_mut().find(|t| t.id == tab) {
            current.clear_table_filter();
        }
    }

    pub fn filter_history(&self) -> &[SavedFilter] {
        &self.filter_history
    }

    /// Save the tab's clauses and raw box under `name`. A duplicate name
    /// replaces the entry in place. False when the name is blank or the tab
    /// is gone.
    pub fn save_filter(&mut self, tab: TabId, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let Some(current) = self.tabs.iter().find(|t| t.id == tab) else { return false };
        let saved = SavedFilter {
            name: name.to_owned(),
            clauses: current.filter_clauses.clone(),
            raw: current.filter_raw.clone(),
        };
        match self.filter_history.iter_mut().find(|entry| entry.name == name) {
            Some(existing) => *existing = saved,
            None => {
                self.filter_history.insert(0, saved);
                self.filter_history.truncate(FILTER_HISTORY_CAP);
            }
        }
        self.persist_filters();
        true
    }

    /// Copy the named filter onto the tab. False for an unknown name or a
    /// missing tab.
    pub fn recall_filter(&mut self, tab: TabId, name: &str) -> bool {
        let Some(saved) = self.filter_history.iter().find(|entry| entry.name == name).cloned()
        else {
            return false;
        };
        let Some(current) = self.tabs.iter_mut().find(|t| t.id == tab) else { return false };
        current.filter_clauses = saved.clauses;
        current.filter_raw = saved.raw;
        true
    }

    /// Drop the named filter. False when no entry carries the name.
    pub fn delete_filter(&mut self, name: &str) -> bool {
        let before = self.filter_history.len();
        self.filter_history.retain(|entry| entry.name != name);
        let removed = self.filter_history.len() != before;
        if removed {
            self.persist_filters();
        }
        removed
    }

    /// Open a table view on its connection: an existing `Table` tab for the
    /// same table activates as-is; otherwise a fresh `Table` tab loads the
    /// capped SELECT and runs it at once, scope All, so the grid has data.
    /// None for an unknown connection.
    pub fn open_table_view(
        &mut self,
        conn: ConnectionId,
        schema: &str,
        table: &str,
    ) -> Option<TabId> {
        if self.connections.iter().find(|c| c.id == conn).is_none() {
            return None;
        }
        let kind = TabKind::Table { schema: schema.to_owned(), table: table.to_owned() };
        if let Some(existing) = self
            .tabs
            .iter()
            .find(|t| t.conn == Some(conn) && t.kind == kind)
            .map(|t| t.id)
        {
            self.activate(existing);
            return Some(existing);
        }
        let sql = table_select(schema, table);
        let tab = self.push_tab(Some(conn), sql.clone(), kind);
        self.activate(tab);
        let len = sql.len();
        self.start_run(tab, &sql, len, None);
        Some(tab)
    }

    /// Turn a table tab back into a query tab, keeping its SQL, so the editor
    /// takes over on the next frame.
    pub fn edit_as_query(&mut self, tab: TabId) {
        if let Some(current) = self.tabs.iter_mut().find(|t| t.id == tab) {
            current.kind = TabKind::Query;
        }
    }

    /// The table data compare state, for the compare view.
    pub fn compare(&self) -> &CompareState {
        &self.compare
    }

    /// True while the Table compare window is open.
    pub fn compare_window_open(&self) -> bool {
        self.compare_open
    }

    /// Open the Table compare window.
    pub fn open_compare(&mut self) {
        self.compare_open = true;
        self.note_modal(Modal::Compare);
    }

    /// Close the Table compare window without clearing its picks or diff.
    pub fn close_compare(&mut self) {
        self.compare_open = false;
    }

    /// Retarget the left pick. Drops loaded rows and runs, so stale results
    /// never diff under new picks.
    pub fn set_compare_left(&mut self, schema: &str, table: &str) {
        self.compare.left =
            ComparePick { schema: schema.to_owned(), table: table.to_owned() };
        self.clear_compare_results();
    }

    /// Retarget the right pick, dropping stale results like the left one.
    pub fn set_compare_right(&mut self, schema: &str, table: &str) {
        self.compare.right =
            ComparePick { schema: schema.to_owned(), table: table.to_owned() };
        self.clear_compare_results();
    }

    /// Replace the key columns and re-diff loaded rows at once, so a key
    /// edit surfaces its error or its new counts without another read.
    pub fn set_compare_keys(&mut self, keys: String) {
        self.compare.keys = keys;
        self.refresh_compare_output();
    }

    /// Drop loaded rows, in-flight runs, the cached diff, and the error. A
    /// landing for a dropped run matches nothing and is silently ignored.
    fn clear_compare_results(&mut self) {
        self.compare.left_result = None;
        self.compare.right_result = None;
        self.compare.left_run = None;
        self.compare.right_run = None;
        self.compare.output = None;
        self.compare.error = None;
    }

    /// Re-diff the loaded rows under the current keys. Missing rows clear the
    /// cached diff; a key error replaces it with the error text.
    fn refresh_compare_output(&mut self) {
        let (Some(left), Some(right)) =
            (self.compare.left_result.clone(), self.compare.right_result.clone())
        else {
            self.compare.output = None;
            return;
        };
        match diff_tables(&left, &right, &parse_key_columns(&self.compare.keys)) {
            Ok(output) => {
                self.compare.output = Some(output);
                self.compare.error = None;
            }
            Err(error) => {
                self.compare.output = None;
                self.compare.error = Some(error.to_string());
            }
        }
    }

    /// Read both picked tables through the worker batch path and diff the
    /// landings. Two single-statement SELECT batches, so the read-only guard
    /// below passes reads on a read-only connection and a SELECT never arms
    /// the write confirm. Landings stash with no tab outcome and no history
    /// entry, the silent-batch precedent, so the grid never flashes.
    pub fn start_compare(&mut self) {
        let conn = match self.default_connection() {
            Some(id) => id,
            None => {
                self.compare.error = Some("no connection for compare".to_owned());
                return;
            }
        };
        let (handle, read_only, live, kind) =
            match self.connections.iter().find(|c| c.id == conn) {
                Some(c) => (
                    c.handle(),
                    c.config.is_read_only(),
                    matches!(
                        c.state,
                        ConnectionState::Connected { .. } | ConnectionState::QueryRunning { .. }
                    ),
                    c.config.kind(),
                ),
                None => {
                    self.compare.error = Some("connection not found".to_owned());
                    return;
                }
            };
        if !live {
            self.compare.error = Some("not connected".to_owned());
            return;
        }
        if !self.compare.left.is_set() || !self.compare.right.is_set() {
            self.compare.error = Some("pick two tables to compare".to_owned());
            return;
        }
        if parse_key_columns(&self.compare.keys).is_empty() {
            self.compare.error = Some(CompareError::NoKeys.to_string());
            return;
        }
        let left_sql = compare_select(
            kind,
            &self.compare.left.schema.clone(),
            &self.compare.left.table.clone(),
        );
        let right_sql = compare_select(
            kind,
            &self.compare.right.schema.clone(),
            &self.compare.right.table.clone(),
        );
        if read_only {
            if let Some(kind) =
                [left_sql.clone(), right_sql.clone()]
                    .iter()
                    .find_map(|statement| crate::db::write_statement_kind(statement))
            {
                self.compare.error =
                    Some(format!("read-only connection refused {kind}"));
                return;
            }
        }
        self.clear_compare_results();
        let Some(handle) = handle else {
            self.compare.error = Some("not connected".to_owned());
            return;
        };
        let left_run = self.ids.run();
        match handle.send(DbCommand::QueryBatch {
            run: left_run,
            statements: vec![left_sql],
        }) {
            Ok(()) => self.compare.left_run = Some(left_run),
            Err(error) => {
                self.compare.error = Some(error.to_string());
                return;
            }
        }
        let right_run = self.ids.run();
        match handle.send(DbCommand::QueryBatch {
            run: right_run,
            statements: vec![right_sql],
        }) {
            Ok(()) => self.compare.right_run = Some(right_run),
            Err(error) => self.compare.error = Some(error.to_string()),
        }
    }

    /// Clear the pending run a landing belongs to, if any. Late landings for
    /// cleared picks match nothing, so a re-pick never inherits old rows.
    fn take_compare_run(&mut self, run: RunId) -> Option<CompareSide> {
        if self.compare.left_run == Some(run) {
            self.compare.left_run = None;
            Some(CompareSide::Left)
        } else if self.compare.right_run == Some(run) {
            self.compare.right_run = None;
            Some(CompareSide::Right)
        } else {
            None
        }
    }

    /// Stash one compare landing and re-diff when both sides hold rows. An
    /// empty batch errors instead of stalling the view on a missing side.
    fn land_compare_side(&mut self, side: CompareSide, results: Vec<QueryResult>) {
        let Some(result) = results.into_iter().next() else {
            self.compare.error = Some("empty compare batch".to_owned());
            return;
        };
        match side {
            CompareSide::Left => self.compare.left_result = Some(result),
            CompareSide::Right => self.compare.right_result = Some(result),
        }
        self.refresh_compare_output();
    }

    /// Follow the single-column foreign key behind one grid cell to its
    /// referenced row: the referenced table opens with a filtered SELECT and
    /// the navigator reveals its entry through [`AppState::reveal_table`].
    /// False when the cell is NULL, the tab names no single table, or the key
    /// is multi-column or unresolvable, which stays on the current view.
    pub fn follow_foreign_key(&mut self, tab: TabId, row: usize, col: usize) -> bool {
        let (conn, child_schema, child_table, column, text, was_null) = match self.tabs
            .iter()
            .find(|t| t.id == tab)
        {
            Some(current) => {
                let Some(conn) = current.conn else { return false };
                let Some(schema) = self.schemas.get(&conn) else { return false };
                let Some(result) = current.active_query_result() else { return false };
                if col >= result.columns().len() || row >= result.row_count() {
                    return false;
                }
                let Some(kind) =
                    self.connections.iter().find(|c| c.id == conn).map(|c| c.config.kind())
                else {
                    return false;
                };
                let table_name = match editable_table(&current.sql, kind) {
                    Some(name) => name,
                    None => return false,
                };
                let (resolved_schema, resolved_table) = match qualified_parts(&table_name)
                    .as_slice()
                {
                    [table] => {
                        let mut found = None;
                        for database in schema.databases() {
                            for node in database.schemas() {
                                for info in node.tables() {
                                    if info.name().eq_ignore_ascii_case(table) {
                                        found = Some((node.name().to_owned(), info.name().to_owned()));
                                        break;
                                    }
                                }
                                if found.is_some() {
                                    break;
                                }
                            }
                            if found.is_some() {
                                break;
                            }
                        }
                        match found {
                            Some(found) => found,
                            None => return false,
                        }
                    }
                    [first, second] => (first.clone(), second.clone()),
                    _ => return false,
                };
                let column = result.columns()[col].name().to_owned();
                let cell = result.cell(row, col);
                let was_null = matches!(cell, Some(Value::Null) | None);
                let text = cell
                    .map(|v| v.display(usize::MAX).into_owned())
                    .unwrap_or_default();
                (conn, resolved_schema, resolved_table, column, text, was_null)
            }
            None => return false,
        };
        let schema = match self.schemas.get(&conn) {
            Some(schema) => schema.clone(),
            None => return false,
        };
        let jump = match fk_jump(&schema, &child_schema, &child_table, &column, &text, was_null) {
            Some(jump) => jump,
            None => return false,
        };
        let sql = jump.sql.clone();
        let existing =
            self.tabs.iter().find(|t| t.conn == Some(conn) && t.sql == sql).map(|t| t.id);
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
        self.reveal_table(conn, &jump.schema, &jump.table);
        true
    }

    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    pub fn history_for(&self, conn: ConnectionId) -> Vec<&HistoryEntry> {
        self.history.iter().filter(|e| e.conn == conn).collect()
    }

    /// True narrows the execution log to the current connection.
    pub fn history_current_only(&self) -> bool {
        self.history_current_only
    }

    /// Pin the execution log to one scope. False shows every connection.
    pub fn set_history_current_only(&mut self, only: bool) {
        self.history_current_only = only;
    }

    /// Flip the execution log between the current connection and all.
    pub fn toggle_history_scope(&mut self) {
        self.history_current_only = !self.history_current_only;
    }

    /// The connection the log narrows to: the active tab's connection.
    pub fn history_scope_connection(&self) -> Option<ConnectionId> {
        self.active.and_then(|id| self.tab(id)).and_then(|tab| tab.conn)
    }

    /// True while the connection can run, so the log shows its rerun button.
    pub fn is_connection_live(&self, conn: ConnectionId) -> bool {
        self.connections.iter().find(|c| c.id == conn).is_some_and(|c| {
            matches!(
                c.state(),
                ConnectionState::Connected { .. } | ConnectionState::QueryRunning { .. }
            )
        })
    }

    /// True while the entry's connection can rerun it now.
    pub fn can_rerun(&self, entry: &HistoryEntry) -> bool {
        self.is_connection_live(entry.conn)
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        self.persist_history();
    }

    pub fn saved_queries(&self) -> &[SavedQuery] {
        &self.saved
    }

    /// Save the active tab's SQL under `name` in `folder`, a slash-separated
    /// path (`"ops/db"` nests under `ops`). A duplicate name replaces the
    /// entry in place when it sits in the same folder and keeps its id; the
    /// same name in another folder saves beside it. The active tab remembers
    /// the entry, so a later save prompt reopens with its folder and pin.
    pub fn save_active_tab(
        &mut self,
        name: &str,
        folder: Option<&str>,
        pinned: bool,
    ) -> Option<u64> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return None;
        }
        let folder = normalize_folder_path(folder);
        let (conn, sql) = match self.active.and_then(|id| self.tab(id)) {
            Some(tab) => match tab.conn {
                Some(conn) => (conn, tab.sql.clone()),
                None => return None,
            },
            None => return None,
        };
        if let Some(existing) = self
            .saved
            .iter_mut()
            .find(|q| q.name == trimmed && q.folder.as_deref() == folder.as_deref())
        {
            existing.sql = sql.clone();
            existing.conn = conn;
            existing.pinned = pinned;
            let at = now_unix_secs();
            existing.push_version(&sql, at);
            let id = existing.id;
            self.link_active_tab_to_saved(id);
            self.persist_saved();
            return Some(id);
        }
        let id = self.ids.saved();
        match SavedQuery::new(
            id,
            trimmed.to_owned(),
            sql.clone(),
            conn,
            folder,
            pinned,
        ) {
            Ok(mut query) => {
                query.push_version(&sql, now_unix_secs());
                self.saved.insert(0, query);
                self.saved.truncate(SAVED_CAP);
                self.link_active_tab_to_saved(id);
                self.persist_saved();
                Some(id)
            }
            Err(_) => None,
        }
    }

    /// Remember which saved entry the active tab came from or last saved as.
    fn link_active_tab_to_saved(&mut self, saved: u64) {
        let Some(active) = self.active else { return };
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == active) {
            tab.saved = Some(saved);
        }
    }

    /// Open a saved query in a fresh tab on its connection. Never auto-runs.
    /// The tab remembers the entry, so the save prompt reopens filled from it.
    pub fn open_saved(&mut self, id: u64) {
        let Some(query) = self.saved.iter().find(|q| q.id == id).cloned() else {
            return;
        };
        let tab = self.new_tab(query.conn);
        if let Some(slot) = self.tab_sql_mut(tab) {
            *slot = query.sql;
        }
        if let Some(slot) = self.tab_mut(tab) {
            slot.saved = Some(id);
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

    /// Retarget one saved query to a nested folder path, creating the folders
    /// by naming them. None (or blank) moves it back to the ungrouped root.
    /// Persists, so the move survives a restart. False for an unknown id.
    /// The sidebar drag and drop lands here.
    pub fn move_saved(&mut self, id: u64, folder: Option<&str>) -> bool {
        let Some(query) = self.saved.iter_mut().find(|q| q.id == id) else {
            return false;
        };
        query.folder = normalize_folder_path(folder);
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

    /// Create one saved query per `.sql` file in `folder`: the file stem
    /// names it, the file text is its SQL. A name already saved in the
    /// folder is replaced in place with a new version, the way
    /// [`AppState::save_active_tab`] replaces; anything else inserts
    /// newest-first in file order. Unreadable or blank files are skipped by
    /// file name. Persists once. None when no connection exists to attribute
    /// the queries to.
    pub fn import_sql_files(
        &mut self,
        paths: &[std::path::PathBuf],
        folder: Option<&str>,
    ) -> Option<SqlImportOutcome> {
        let conn = self
            .active
            .and_then(|id| self.tab(id))
            .and_then(|tab| tab.conn)
            .or_else(|| self.connections.first().map(|c| c.id))?;
        let folder = normalize_folder_path(folder);
        let mut imported = 0;
        let mut skipped = Vec::new();
        for path in paths.iter().rev() {
            let name = sql_import_name_for_path(path);
            let sql = match std::fs::read_to_string(path) {
                Ok(text) if !text.trim().is_empty() => text,
                _ => {
                    skipped.push(sql_import_display_name(path));
                    continue;
                }
            };
            if let Some(existing) = self
                .saved
                .iter_mut()
                .find(|q| q.name == name && q.folder.as_deref() == folder.as_deref())
            {
                existing.sql = sql.clone();
                existing.conn = conn;
                existing.push_version(&sql, now_unix_secs());
                imported += 1;
                continue;
            }
            match SavedQuery::new(
                self.ids.saved(),
                name,
                sql.clone(),
                conn,
                folder.clone(),
                false,
            ) {
                Ok(mut query) => {
                    query.push_version(&sql, now_unix_secs());
                    self.saved.insert(0, query);
                    imported += 1;
                }
                Err(_) => skipped.push(sql_import_display_name(path)),
            }
        }
        self.saved.truncate(SAVED_CAP);
        self.persist_saved();
        skipped.reverse();
        Some(SqlImportOutcome { imported, skipped })
    }

    /// Every retained version for one saved query, newest first. Empty for
    /// an unknown id. The sidebar version list renders this.
    pub fn saved_versions(&self, id: u64) -> Vec<SavedQueryVersion> {
        self.saved.iter().find(|q| q.id == id).map(|q| q.versions.clone()).unwrap_or_default()
    }

    /// Restore one version's text into its saved query. The restored text
    /// becomes the query SQL and is recorded as a new newest version, so
    /// the restore itself stays undoable. Persists. False for unknown id
    /// or index. Callers open the query through [`AppState::open_saved`]
    /// to show the restored text, reusing the existing open path.
    pub fn restore_saved_version(&mut self, id: u64, index: usize) -> bool {
        let Some(query) = self.saved.iter_mut().find(|q| q.id == id) else {
            return false;
        };
        let Some(version) = query.versions.get(index).cloned() else {
            return false;
        };
        query.sql = version.sql.clone();
        let at = now_unix_secs();
        query.push_version(&version.sql, at);
        self.persist_saved();
        true
    }

    /// Open one version's text in a fresh tab on the query's connection.
    /// Never auto-runs and never changes the saved entry. The tab links to
    /// the entry, so saving from it records a new version through the
    /// existing save path.
    pub fn open_saved_version(&mut self, id: u64, index: usize) {
        let Some(query) = self.saved.iter().find(|q| q.id == id).cloned() else {
            return;
        };
        let Some(version) = query.versions.get(index).cloned() else {
            return;
        };
        let tab = self.new_tab(query.conn);
        if let Some(slot) = self.tab_sql_mut(tab) {
            *slot = version.sql;
        }
        if let Some(slot) = self.tab_mut(tab) {
            slot.saved = Some(id);
        }
        self.activate(tab);
        self.set_outcome(tab, RunOutcome::Never);
    }

    /// Every bookmarked table, newest first.
    pub fn bookmarks(&self) -> &[TableBookmark] {
        &self.bookmarks
    }

    /// Whether the connection schema table is bookmarked. Exact match on
    /// all three fields, so a renamed table leaves its old entry behind.
    pub fn is_bookmarked(&self, conn: ConnectionId, schema: &str, table: &str) -> bool {
        self.bookmarks
            .iter()
            .any(|b| b.conn == conn && b.schema == schema && b.table == table)
    }

    /// Toggle one bookmark. True when the entry is bookmarked after the
    /// call. Blank schema or table names toggle nothing.
    pub fn toggle_bookmark(&mut self, conn: ConnectionId, schema: &str, table: &str) -> bool {
        if schema.trim().is_empty() || table.trim().is_empty() {
            return false;
        }
        match self
            .bookmarks
            .iter()
            .position(|b| b.conn == conn && b.schema == schema && b.table == table)
        {
            Some(index) => {
                self.bookmarks.remove(index);
                self.persist_bookmarks();
                false
            }
            None => {
                self.bookmarks.insert(
                    0,
                    TableBookmark {
                        conn,
                        schema: schema.to_owned(),
                        table: table.to_owned(),
                    },
                );
                self.bookmarks.truncate(BOOKMARKS_CAP);
                self.persist_bookmarks();
                true
            }
        }
    }

    /// The navigator tree filter text.
    pub fn navigator_filter(&self) -> &str {
        &self.navigator_filter
    }

    /// Replace the navigator tree filter text.
    pub fn set_navigator_filter(&mut self, filter: String) {
        self.navigator_filter = filter;
    }

    /// Which sidebar section is on display.
    pub fn sidebar_view(&self) -> SidebarView {
        self.sidebar_view
    }

    /// Switch the sidebar to one section.
    pub fn set_sidebar_view(&mut self, view: SidebarView) {
        self.sidebar_view = view;
    }

    /// Whether the palette is open.
    pub fn palette_open(&self) -> bool {
        self.search.is_some()
    }

    /// The open palette.
    pub fn palette(&self) -> Option<&SearchPalette> {
        self.search.as_ref()
    }

    /// The open palette's filter box and rows.
    pub fn palette_mut(&mut self) -> Option<&mut SearchPalette> {
        self.search.as_mut()
    }

    /// Open the palette fresh: empty query, first row selected, no rows
    /// until the query says something.
    pub fn open_palette(&mut self) {
        self.search = Some(SearchPalette {
            query: String::new(),
            selected: 0,
            results: Vec::new(),
        });
        self.note_modal(Modal::Palette);
    }

    pub fn close_palette(&mut self) {
        self.search = None;
    }

    pub fn toggle_palette(&mut self) {
        if self.palette_open() {
            self.close_palette();
        } else {
            self.open_palette();
        }
    }

    /// Replace the filter text and re-rank. The selection clamps to the
    /// smaller list.
    pub fn set_palette_query(&mut self, query: &str) {
        if let Some(palette) = self.search.as_mut() {
            palette.query = query.to_owned();
        }
        self.refilter_palette();
    }

    /// Re-run the matcher for the current query.
    fn refilter_palette(&mut self) {
        let Some(query) = self.search.as_ref().map(|palette| palette.query.clone()) else {
            return;
        };
        let results = rank_candidates(&query, &search_candidates(self));
        if let Some(palette) = self.search.as_mut() {
            palette.results = results;
            palette.selected = palette.selected.min(palette.results.len().saturating_sub(1));
        }
    }

    /// Move the selection by `delta` rows, wrapping around the ends. A no-op
    /// with no rows.
    pub fn move_palette_selection(&mut self, delta: isize) {
        let Some(palette) = self.search.as_mut() else { return };
        let len = palette.results.len();
        if len == 0 {
            return;
        }
        palette.selected =
            (palette.selected as isize + delta).rem_euclid(len as isize) as usize;
    }

    /// The row Enter would open.
    pub fn palette_selection(&self) -> Option<&SearchResult> {
        let palette = self.search.as_ref()?;
        palette.results.get(palette.selected)
    }

    /// Open the selected row and close the palette. False with no selection,
    /// which leaves the palette open.
    pub fn activate_palette_selection(&mut self) -> bool {
        let Some(result) = self.palette_selection().cloned() else { return false };
        self.open_search_result(&result);
        self.close_palette();
        true
    }

    /// Open one palette row: a connection activates (opening a tab when it
    /// has none), a saved query opens like the sidebar, a table opens its
    /// table view, and a column reveals its table in the navigator.
    pub fn open_search_result(&mut self, result: &SearchResult) {
        match result {
            SearchResult::Connection(conn) => self.select_connection(*conn),
            SearchResult::Saved(id) => self.open_saved(*id),
            SearchResult::Table { conn, schema, table } => {
                let _ = self.open_table_view(*conn, schema, table);
            }
            SearchResult::Column { conn, schema, table, .. } => {
                self.reveal_table(*conn, schema, table)
            }
        }
    }

    /// Make a connection the sidebar's selection. The sidebar highlights the
    /// active tab's connection, so this activates the connection's first tab
    /// or opens a fresh one when it has none.
    fn select_connection(&mut self, conn: ConnectionId) {
        if !self.connections.iter().any(|c| c.id == conn) {
            return;
        }
        match self.tabs.iter().find(|tab| tab.conn == Some(conn)) {
            Some(tab) => self.activate(tab.id),
            None => {
                let tab = self.new_tab(conn);
                self.activate(tab);
            }
        }
    }

    /// Ask the navigator to open and show one table's entry, by exact schema
    /// and table name.
    pub fn reveal_table(&mut self, conn: ConnectionId, schema: &str, table: &str) {
        self.navigator_reveal = Some(NavigatorReveal {
            conn,
            schema: schema.to_owned(),
            table: table.to_owned(),
        });
    }

    /// The table the navigator should reveal, while one is pending.
    pub fn navigator_reveal(&self) -> Option<&NavigatorReveal> {
        self.navigator_reveal.as_ref()
    }

    /// Clear the pending reveal once the schema tree showed it.
    pub fn clear_navigator_reveal(&mut self) {
        self.navigator_reveal = None;
    }

    pub fn save_prompt(&self) -> Option<&SavePrompt> {
        self.save_prompt.as_ref()
    }

    pub fn save_prompt_mut(&mut self) -> Option<&mut SavePrompt> {
        self.save_prompt.as_mut()
    }

    /// Open the save prompt, prefilled from the active tab's saved entry when
    /// it has one, so a re-save keeps the entry's folder and pin.
    pub fn open_save_prompt(&mut self) {
        let prefill = self
            .active
            .and_then(|id| self.tab(id))
            .and_then(|tab| tab.saved)
            .and_then(|saved| {
                self.saved
                    .iter()
                    .find(|q| q.id == saved)
                    .map(|q| (q.name.clone(), q.folder.clone().unwrap_or_default(), q.pinned))
            });
        self.save_prompt = Some(match prefill {
            Some((name, folder, pinned)) => SavePrompt { name, folder, pinned, error: None },
            None => SavePrompt {
                name: String::new(),
                folder: String::new(),
                pinned: false,
                error: None,
            },
        });
        self.note_modal(Modal::SavePrompt);
    }

    pub fn close_save_prompt(&mut self) {
        self.save_prompt = None;
    }

    pub fn confirm_save_prompt(&mut self) -> Option<u64> {
        let (name, folder, pinned) = self
            .save_prompt
            .as_ref()
            .map(|p| (p.name.clone(), p.folder.clone(), p.pinned))
            .unwrap_or_default();
        if name.trim().is_empty() {
            if let Some(prompt) = self.save_prompt.as_mut() {
                prompt.error = Some("name is empty".to_owned());
            }
            return None;
        }
        match self.save_active_tab(&name, Some(folder.as_str()), pinned) {
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
        let (name, env, config, password, folder) = match editing {
            Some(id) => match self.connections.iter().find(|c| c.id == id) {
                Some(c) => (
                    c.name.clone(),
                    c.env,
                    c.config.clone(),
                    c.password.clone(),
                    c.folder.clone().unwrap_or_default(),
                ),
                None => return,
            },
            None => (
                String::new(),
                Environment::default(),
                DbKind::Sqlite.default_config(),
                String::new(),
                String::new(),
            ),
        };
        self.draft = Some(ConnectDraft {
            editing,
            name,
            env,
            config,
            password,
            ssh_password: String::new(),
            folder,
            error: None,
        });
        self.note_modal(Modal::Draft);
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
                    c.ssh_password = draft.ssh_password;
                    c.folder = normalize_folder_path(Some(draft.folder.as_str()));
                }
            }
            None => {
                let id = self.ids.connection();
                let folder = normalize_folder_path(Some(draft.folder.as_str()));
                self.connections.push(Connection {
                    id,
                    name: draft.name,
                    env: draft.env,
                    config: draft.config,
                    password: draft.password,
                    ssh_password: draft.ssh_password,
                    folder,
                    state: ConnectionState::Disconnected,
                    transaction: Transaction::Closed,
                    autocommit: true,
                });
            }
        }
        self.persist();
    }

    /// Retarget one connection to a nested folder path, creating the folders
    /// by naming them. None (or blank) moves it back to the ungrouped root.
    /// Persists, so the move survives a restart. False for an unknown id.
    pub fn move_connection(&mut self, id: ConnectionId, folder: Option<&str>) -> bool {
        let Some(conn) = self.connections.iter_mut().find(|c| c.id == id) else {
            return false;
        };
        conn.folder = normalize_folder_path(folder);
        self.persist();
        true
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
                        // A fresh worker session starts with autocommit on, so
                        // a reconnect must not keep a stale observed mode.
                        c.autocommit = true;
                        c.transaction = Transaction::Closed;
                        Some(handle)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(handle) = handle {
                    // A fresh worker session invalidates the counts and the
                    // session list the previous session cached.
                    self.row_counts.retain(|(c, _, _), _| *c != conn);
                    self.sessions.remove(&conn);
                    self.sessions_loading.remove(&conn);
                    self.sessions_errors.remove(&conn);
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
                match self.take_compare_run(run) {
                    Some(side) => self.land_compare_side(side, vec![result]),
                    None => self.finish_run(run, RunOutcome::Ok { results: vec![result], active: 0 }),
                }
            }
            DbEvent::BatchOk { run, results } => {
                match self.take_compare_run(run) {
                    Some(side) => self.land_compare_side(side, results),
                    None => self.finish_run(run, RunOutcome::Ok { results, active: 0 }),
                }
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
            DbEvent::RowCountOk { conn, schema, table, count } => {
                self.row_counts.insert((conn, schema, table), RowCountState::Ready(count));
            }
            DbEvent::RowCountErr { conn, schema, table, error } => {
                self.row_counts
                    .insert((conn, schema, table), RowCountState::Failed(error.to_string()));
            }
            DbEvent::SessionsOk { conn, sessions } => {
                self.sessions_loading.remove(&conn);
                self.sessions_errors.remove(&conn);
                self.sessions.insert(conn, sessions);
            }
            DbEvent::SessionsErr { conn, error } => {
                self.sessions_loading.remove(&conn);
                self.sessions_errors.insert(conn, error.to_string());
            }
            DbEvent::TerminateOk { conn, backend } => {
                let _ = backend;
                self.sessions_errors.remove(&conn);
                // Re-read the list so the panel drops the killed backend.
                if let Some(handle) = self
                    .connections
                    .iter()
                    .find(|c| c.id == conn)
                    .and_then(|c| c.handle())
                {
                    self.sessions_loading.insert(conn);
                    if handle.send(DbCommand::ListSessions).is_err() {
                        self.sessions_loading.remove(&conn);
                        self.sessions_errors.insert(conn, "connection is gone".to_owned());
                    }
                }
            }
            DbEvent::TerminateErr { conn, backend, error } => {
                let _ = backend;
                self.sessions_errors.insert(conn, error.to_string());
            }
            DbEvent::QueryErr { run, error } => {
                if self.take_compare_run(run).is_some() {
                    self.compare.error = Some(error.to_string());
                    return;
                }
                let fatal = error.is_fatal();
                let message = error.to_string();
                let conn_id = self
                    .tabs
                    .iter()
                    .find(|t| matches!(&t.outcome, RunOutcome::Running { run: r, .. } if *r == run))
                    .and_then(|t| t.conn);
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
                                c.transaction = Transaction::Closed;
                            }
                        }
                    }
                }
            }
            DbEvent::AutocommitSet { conn, on, tx_open, error } => {
                if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn) {
                    // A landing that arrives after a disconnect must not
                    // move the observed state, same rule as the tx flag.
                    if matches!(
                        c.state,
                        ConnectionState::Connected { .. } | ConnectionState::QueryRunning { .. }
                    ) {
                        c.autocommit = on;
                        c.transaction =
                            if tx_open { Transaction::Open } else { Transaction::Closed };
                        if let Some(error) = error {
                            // The rollback on the way back to autocommit
                            // failed and the worker closed the connection.
                            let message = error.to_string();
                            c.state = ConnectionState::Error { message };
                            c.transaction = Transaction::Closed;
                        }
                    }
                }
            }
            DbEvent::TxState { conn, open } => {
                if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn) {
                    if matches!(
                        c.state,
                        ConnectionState::Connected { .. } | ConnectionState::QueryRunning { .. }
                    ) {
                        c.transaction = if open { Transaction::Open } else { Transaction::Closed };
                    }
                }
            }
            DbEvent::TxIdleWarning { conn } => {
                self.notice_connection(
                    conn,
                    format!(
                        "Open transaction idle for {} minutes; it rolls back at {}",
                        TX_IDLE_ROLLBACK_MINUTES - 1,
                        TX_IDLE_ROLLBACK_MINUTES
                    ),
                    false,
                );
            }
            DbEvent::TxIdleRollback { conn } => {
                if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn) {
                    if matches!(
                        c.state,
                        ConnectionState::Connected { .. } | ConnectionState::QueryRunning { .. }
                    ) {
                        c.transaction = Transaction::Closed;
                    }
                }
                self.notice_connection(
                    conn,
                    format!(
                        "Rolled back the idle transaction after {} minutes",
                        TX_IDLE_ROLLBACK_MINUTES
                    ),
                    false,
                );
            }
            DbEvent::PageOk { run: _, tab, offset, result } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    let active = t.active_result();
                    let target: Option<&mut QueryResult> = match &mut t.outcome {
                        RunOutcome::Ok { results, .. } => results.get_mut(active),
                        _ => t.results.get_mut(active),
                    };
                    if let Some(current) = target {
                        if current.row_count() as u64 == offset {
                            if let Err(error) = current.append_page(result) {
                                t.set_export_notice(
                                    format!("Page failed: {error}"),
                                    true,
                                );
                            }
                        }
                    }
                }
            }
            DbEvent::PageErr { run: _, tab, error } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.set_export_notice(format!("Page failed: {error}"), true);
                }
            }
            DbEvent::CountOk { run: _, tab, total } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    let active = t.active_result();
                    let target: Option<&mut QueryResult> = match &mut t.outcome {
                        RunOutcome::Ok { results, .. } => results.get_mut(active),
                        _ => t.results.get_mut(active),
                    };
                    if let Some(current) = target {
                        current.set_total(total);
                    }
                }
            }
            DbEvent::CountErr { run: _, tab, error } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.set_export_notice(format!("Count failed: {error}"), true);
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
                // A running tab always carries a connection; only a directly
                // minted test run can land here without one.
                let Some(conn) = t.conn else {
                    t.outcome = outcome;
                    return;
                };
                let (started, statements) = match &t.outcome {
                    RunOutcome::Running { started, statements, .. } => {
                        (*started, statements.clone())
                    }
                    _ => (Instant::now(), Vec::new()),
                };
                // The run's own statements decide whether a successful
                // landing should reload the navigator.
                let schema_change =
                    statements.iter().any(|statement| statement_changes_schema(statement));
                let ok = matches!(&outcome, RunOutcome::Ok { .. });
                // An import landing behaves like an apply that adds no
                // history entry and records its send-time notice.
                let pending_import = t.pending_import.take_if(|(pending, _)| *pending == run);
                let imported = pending_import.is_some();
                let import_notice =
                    pending_import.filter(|_| ok).map(|(_, notice)| notice);
                let applied = t.pending_apply == Some(run) || imported;
                let refresh = applied && ok;
                if t.pending_apply == Some(run) {
                    t.pending_apply = None;
                }
                let silent = t.silent_run == Some(run) || imported;
                if t.silent_run == Some(run) {
                    t.silent_run = None;
                }
                if t.pending_table == Some(run) {
                    t.pending_table = None;
                }
                // Only an Ok landing moves the transaction flag, so a failed
                // BEGIN, COMMIT, or ROLLBACK leaves it as it was.
                let transaction = if ok { transaction_action(&statements) } else { None };
                // A ROLLBACK can undo DDL the navigator has already listed,
                // so its landing reloads the schema like a schema change.
                let rollback = transaction == Some(TransactionAction::Rollback);
                let schema_refresh = ok && (schema_change || rollback);
                if ok {
                    t.edits.clear();
                    t.deleted_rows.clear();
                    t.added_rows.clear();
                    if self.pending_edit_preview == Some(t.id) {
                        self.pending_edit_preview = None;
                    }
                }
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
                // A silent run leaves the notice alone, so an import notice
                // set when its batch landed outlives the refresh; a failure
                // clears it.
                if !silent || !ok {
                    t.clear_export_notice();
                }
                Some((tab_id, conn, entries, refresh, import_notice, transaction, schema_refresh))
            }
            None => None,
        };
        let Some((tab_id, conn, entries, refresh, import_notice, transaction, schema_refresh)) =
            matched
        else {
            return;
        };
        if !entries.is_empty() {
            self.history.splice(0..0, entries);
            self.history.truncate(HISTORY_CAP);
            self.persist_history();
        }
        let still_running = self
            .tabs
            .iter()
            .any(|t| t.conn == Some(conn) && matches!(t.outcome, RunOutcome::Running { .. }));
        if !still_running {
            if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn) {
                if let ConnectionState::QueryRunning { session, .. } = &c.state {
                    let session = session.clone();
                    c.state = ConnectionState::Connected { session };
                }
            }
        }
        if let Some(action) = transaction {
            if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn) {
                // A landing that arrives after a disconnect must not reopen
                // the flag, so only a live connection moves it.
                if matches!(
                    c.state,
                    ConnectionState::Connected { .. } | ConnectionState::QueryRunning { .. }
                ) {
                    c.transaction = action.on_ok();
                }
            }
        }
        if refresh {
            self.start_refresh(tab_id);
        }
        if schema_refresh {
            self.refresh_schema(conn);
        }
        // After the refresh send: its set_outcome clears the notice, so the
        // import notice must land last to survive.
        if let Some(notice) = import_notice {
            self.set_tab_notice(tab_id, notice, false);
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

    fn persist_filters(&self) {
        if let Err(e) = save_saved_filters(&self.filter_history) {
            log::warn!("persist saved filters: {e}");
        }
    }

    fn persist_bookmarks(&self) {
        if let Err(e) = save_bookmarks(&self.bookmarks) {
            log::warn!("persist bookmarks: {e}");
        }
    }

    fn persist_tabs(&self) {
        if let Err(e) = save_tabs(&self.tabs) {
            log::warn!("persist tabs: {e}");
        }
    }

    fn persist_settings(&self) {
        if let Err(e) = save_settings(&Settings { theme: self.theme }) {
            log::warn!("persist settings: {e}");
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
    fn on_exit(&mut self) {
        self.persist_tabs();
        self.persist_settings();
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedProfile {
    id: u64,
    name: String,
    #[serde(default)]
    env: Environment,
    config: ConnectionConfig,
    /// Slash-separated sidebar folder path. Defaults so old files load at
    /// the ungrouped root.
    #[serde(default)]
    folder: Option<String>,
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
                ssh_password: String::new(),
                folder: normalize_folder_path(p.folder.as_deref()),
                state: ConnectionState::Disconnected,
                transaction: Transaction::Closed,
                autocommit: true,
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
            folder: c.folder.clone(),
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
            for query in &mut queries {
                query.folder = normalize_folder_path(query.folder.as_deref());
                query.versions.truncate(SAVED_VERSION_CAP);
            }
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

/// The named structured filters beside the saved queries, newest first.
/// One file per store, one shape per file, so a corrupt filters file cannot
/// take the saved queries down with it.
fn saved_filters_path() -> std::path::PathBuf {
    profiles_path().with_file_name("saved_filters.json")
}

/// Every retained filter in file order. A missing or corrupt file loads as
/// empty, and the 50 entry cap holds across restarts through the truncate.
fn load_saved_filters() -> Vec<SavedFilter> {
    let path = saved_filters_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_slice::<Vec<SavedFilter>>(&bytes) {
        Ok(mut filters) => {
            filters.retain(|f| !f.name.trim().is_empty());
            filters.truncate(FILTER_HISTORY_CAP);
            filters
        }
        Err(e) => {
            log::warn!("parse {}: {e}", path.display());
            Vec::new()
        }
    }
}

fn save_saved_filters(filters: &[SavedFilter]) -> std::io::Result<()> {
    let path = saved_filters_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(filters).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// The bookmarked tables beside the saved queries, newest first. One file
/// per store, so a corrupt bookmarks file cannot take the queries down.
fn bookmarks_path() -> std::path::PathBuf {
    profiles_path().with_file_name("bookmarks.json")
}

/// Every retained bookmark in file order. A missing or corrupt file loads
/// as empty, and rows with blank schema or table names are pruned.
fn load_bookmarks() -> Vec<TableBookmark> {
    let path = bookmarks_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_slice::<Vec<TableBookmark>>(&bytes) {
        Ok(mut bookmarks) => {
            bookmarks.retain(|b| !b.schema.trim().is_empty() && !b.table.trim().is_empty());
            bookmarks.truncate(BOOKMARKS_CAP);
            bookmarks
        }
        Err(e) => {
            log::warn!("parse {}: {e}", path.display());
            Vec::new()
        }
    }
}

fn save_bookmarks(bookmarks: &[TableBookmark]) -> std::io::Result<()> {
    let path = bookmarks_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(bookmarks).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// One persisted tab row: the connection, the SQL, and the kind. Every field
/// defaults so a partial row still loads instead of failing the file; a row
/// written before `kind` existed loads as a query tab.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedTab {
    #[serde(default)]
    conn: Option<ConnectionId>,
    #[serde(default)]
    sql: String,
    #[serde(default)]
    kind: TabKind,
}

fn tabs_path() -> std::path::PathBuf {
    profiles_path().with_file_name("tabs.json")
}

/// Every parseable row in file order. A corrupt file loads as empty; a row
/// that does not parse is skipped, so one bad row keeps the rest.
fn load_tabs() -> Vec<PersistedTab> {
    let path = tabs_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let rows = match serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) {
        Ok(rows) => rows,
        Err(e) => {
            log::warn!("parse {}: {e}", path.display());
            return Vec::new();
        }
    };
    rows.into_iter()
        .filter_map(|row| match serde_json::from_value::<PersistedTab>(row) {
            Ok(tab) => Some(tab),
            Err(e) => {
                log::warn!("skip tab row in {}: {e}", path.display());
                None
            }
        })
        .collect()
}

fn save_tabs(tabs: &[Tab]) -> std::io::Result<()> {
    let path = tabs_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let persisted: Vec<PersistedTab> = tabs
        .iter()
        .map(|t| PersistedTab { conn: t.conn, sql: t.sql.clone(), kind: t.kind.clone() })
        .collect();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&persisted).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// App settings beside the other stores. One field today, extendable later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
struct Settings {
    #[serde(default)]
    theme: crate::ui::Theme,
}

fn settings_path() -> std::path::PathBuf {
    profiles_path().with_file_name("settings.json")
}

/// The saved choice, or dark when no settings file exists yet.
fn load_settings() -> Settings {
    let path = settings_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Settings::default(),
    };
    match serde_json::from_slice::<Settings>(&bytes) {
        Ok(settings) => settings,
        Err(e) => {
            log::warn!("parse {}: {e}", path.display());
            Settings::default()
        }
    }
}

fn save_settings(settings: &Settings) -> std::io::Result<()> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(settings).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::PostgresSslMode;
    use crate::db::{ColumnInfo, DatabaseNode, MysqlSslMode, SchemaNode, TableInfo, TableKind};

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
            draft.config = ConnectionConfig::Sqlite { read_only: false, path: ":memory:".into() };
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
            draft.config = ConnectionConfig::Postgres { read_only: false,
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
                ssh: None,
            };
            draft.password = password.to_owned();
        }
        state.save_draft();
        state.connections().last().map(|c| c.id).expect("saved connection")
    }

    fn add_mysql_connection(state: &mut AppState, name: &str, password: &str) -> ConnectionId {
        state.open_draft(None);
        {
            let draft = state.draft_mut().expect("new draft");
            draft.name = name.to_owned();
            draft.env = Environment::Dev;
            draft.config = ConnectionConfig::Mysql { read_only: false,
                host: "localhost".to_owned(),
                port: 3307,
                database: "cde_test".to_owned(),
                user: "root".to_owned(),
                ssl_mode: MysqlSslMode::default(),
                ssh: None,
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

    /// The transaction flag for one connection.
    fn transaction_open(state: &AppState, conn: ConnectionId) -> bool {
        state
            .connections()
            .iter()
            .find(|c| c.id == conn)
            .expect("connection")
            .transaction()
            .is_open()
    }

    /// The observed autocommit for one connection.
    fn autocommit_on(state: &AppState, conn: ConnectionId) -> bool {
        state
            .connections()
            .iter()
            .find(|c| c.id == conn)
            .expect("connection")
            .autocommit()
    }

    /// Send one transaction control through the button path, asserting the
    /// exact statement reached the worker, and hand back its run id.
    fn send_transaction(
        state: &mut AppState,
        commands: &mpsc::Receiver<DbCommand>,
        tab: TabId,
        action: TransactionAction,
    ) -> RunId {
        state.start_transaction(tab, action);
        match commands.try_recv().expect("a command was sent") {
            DbCommand::QueryBatch { run, statements } => {
                assert_eq!(statements, vec![action.statement()]);
                run
            }
            other => panic!("expected QueryBatch, got {other:?}"),
        }
    }

    /// Send a Begin, land its Ok, and assert the flag opened.
    fn begin_and_land(
        state: &mut AppState,
        commands: &mpsc::Receiver<DbCommand>,
        tab: TabId,
        conn: ConnectionId,
    ) {
        let run = send_transaction(state, commands, tab, TransactionAction::Begin);
        state.apply(DbEvent::BatchOk { run, results: vec![affected_result(0)] });
        assert!(transaction_open(state, conn), "BEGIN Ok opens the flag");
    }

    /// Drain worker events into the fold until `done` holds or the deadline
    /// passes. Live tests only.
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    fn pump_until(state: &mut AppState, done: impl Fn(&AppState) -> bool) -> bool {
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if done(state) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            if let Ok(event) =
                state.events_rx.recv_timeout(std::time::Duration::from_millis(100))
            {
                state.apply(event);
            }
        }
    }

    /// Wait for the tab's run to land, then panic on a failed outcome.
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    fn settle(state: &mut AppState, tab: TabId, what: &str) {
        let landed =
            pump_until(state, |s| {
                !matches!(s.tab(tab).map(|t| t.outcome()), Some(RunOutcome::Running { .. }))
            });
        assert!(landed, "no landing for {what}");
        if let RunOutcome::Err(message) = state.tab(tab).expect("tab").outcome() {
            panic!("{what} failed: {message}");
        }
    }

    /// Run one statement through the app and wait for its landing.
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    fn live_run(state: &mut AppState, tab: TabId, sql: &str) {
        state.start_run(tab, sql, 0, None);
        settle(state, tab, sql);
    }

    /// Send one transaction control through the button path and wait for its
    /// landing.
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    fn live_transaction(state: &mut AppState, tab: TabId, action: TransactionAction) {
        state.start_transaction(tab, action);
        settle(state, tab, action.statement());
    }

    /// Toggle autocommit through the button path and wait for the landing.
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    fn live_autocommit(state: &mut AppState, tab: TabId, conn: ConnectionId, on: bool) {
        state.set_autocommit(tab, on);
        assert!(
            pump_until(state, |s| autocommit_on(s, conn) == on),
            "the autocommit toggle to {on} lands"
        );
    }

    /// The tab's first result cell as an integer.
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    fn integer_cell(state: &AppState, tab: TabId) -> i64 {
        match state.tab(tab).and_then(|t| t.active_query_result()).and_then(|r| r.cell(0, 0)) {
            Some(Value::Integer(n)) => *n,
            other => panic!("expected an integer cell, got {other:?}"),
        }
    }

    #[cfg(feature = "postgres")]
    fn add_postgres_probe_connection(state: &mut AppState) -> ConnectionId {
        state.open_draft(None);
        {
            let draft = state.draft_mut().expect("new draft");
            draft.name = "pg-probe".to_owned();
            draft.config = ConnectionConfig::Postgres { read_only: false,
                host: std::env::var("PGHOST").unwrap_or_else(|_| "127.0.0.1".to_owned()),
                port: std::env::var("PGPORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(5432),
                database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
                user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
                ssl_mode: PostgresSslMode::Disable,
                ssh: None,
            };
            draft.password = std::env::var("PGPASSWORD").unwrap_or_default();
        }
        state.save_draft();
        state.connections().last().map(|c| c.id).expect("saved connection")
    }

    #[cfg(feature = "mysql")]
    fn add_mysql_probe_connection(state: &mut AppState) -> ConnectionId {
        state.open_draft(None);
        {
            let draft = state.draft_mut().expect("new draft");
            draft.name = "mysql-probe".to_owned();
            draft.config = ConnectionConfig::Mysql { read_only: false,
                host: std::env::var("MYSQLHOST").unwrap_or_else(|_| "127.0.0.1".to_owned()),
                port: std::env::var("MYSQLPORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(3306),
                database: std::env::var("MYSQLDATABASE")
                    .unwrap_or_else(|_| "mysql".to_owned()),
                user: std::env::var("MYSQLUSER").unwrap_or_else(|_| "root".to_owned()),
                ssl_mode: MysqlSslMode::default(),
                ssh: None,
            };
            draft.password = std::env::var("MYSQLPASSWORD").unwrap_or_default();
        }
        state.save_draft();
        state.connections().last().map(|c| c.id).expect("saved connection")
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
        let node = SchemaNode::new(
            "main".to_owned(),
            vec![table],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            false,
        )
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

    /// Result shapes for the single-table mapping tests: the alias list
    /// `SELECT tenant, id, name AS label, upper(name) FROM items`, and the
    /// same list with both `id` and `name` aliased away.
    fn aliased_items_result() -> QueryResult {
        QueryResult::from_columns(
            vec![
                crate::db::Column::new(
                    "tenant".to_owned(),
                    "TEXT".to_owned(),
                    vec![crate::db::Value::Text("acme".to_owned())],
                ),
                crate::db::Column::new(
                    "id".to_owned(),
                    "INTEGER".to_owned(),
                    vec![crate::db::Value::Integer(1)],
                ),
                crate::db::Column::new(
                    "label".to_owned(),
                    "TEXT".to_owned(),
                    vec![crate::db::Value::Text("ada".to_owned())],
                ),
                crate::db::Column::new(
                    "upper(name)".to_owned(),
                    "TEXT".to_owned(),
                    vec![crate::db::Value::Text("ADA".to_owned())],
                ),
            ],
            false,
            None,
            std::time::Duration::from_millis(1),
        )
        .unwrap()
    }

    fn renamed_items_result() -> QueryResult {
        QueryResult::from_columns(
            vec![
                crate::db::Column::new(
                    "tenant".to_owned(),
                    "TEXT".to_owned(),
                    vec![crate::db::Value::Text("acme".to_owned())],
                ),
                crate::db::Column::new(
                    "item_id".to_owned(),
                    "INTEGER".to_owned(),
                    vec![crate::db::Value::Integer(1)],
                ),
                crate::db::Column::new(
                    "label".to_owned(),
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

    /// Replace the tab's displayed result with `result`, the way a finished
    /// run lands it.
    fn set_tab_result(state: &mut AppState, tab: TabId, result: QueryResult) {
        let run = state.ids.run();
        state.set_outcome(
            tab,
            RunOutcome::Running {
                run,
                started: Instant::now(),
                statements: Vec::new(),
            },
        );
        state.apply(DbEvent::BatchOk { run, results: vec![result] });
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

    /// Two keyed tables for join tests: users with one editable column and
    /// orders with a key, a link, and one editable column.
    fn join_schema() -> SchemaInfo {
        let users = TableInfo::new(
            "users".to_owned(),
            TableKind::Table,
            vec![
                ColumnInfo::new("id".to_owned(), "INTEGER".to_owned(), false, Some(1)).unwrap(),
                ColumnInfo::new("name".to_owned(), "TEXT".to_owned(), true, None).unwrap(),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            false,
        )
        .unwrap();
        let orders = TableInfo::new(
            "orders".to_owned(),
            TableKind::Table,
            vec![
                ColumnInfo::new("id".to_owned(), "INTEGER".to_owned(), false, Some(1)).unwrap(),
                ColumnInfo::new("user_id".to_owned(), "INTEGER".to_owned(), true, None).unwrap(),
                ColumnInfo::new("total".to_owned(), "INTEGER".to_owned(), true, None).unwrap(),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            false,
        )
        .unwrap();
        let node = SchemaNode::new(
            "main".to_owned(),
            vec![users, orders],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            false,
        )
        .unwrap();
        let database = DatabaseNode::new(":memory:".to_owned(), vec![node]).unwrap();
        SchemaInfo::new(vec![database], false, false)
    }

    fn join_result() -> QueryResult {
        QueryResult::from_columns(
            vec![
                crate::db::Column::new(
                    "id".to_owned(),
                    "INTEGER".to_owned(),
                    vec![crate::db::Value::Integer(1), crate::db::Value::Integer(2)],
                ),
                crate::db::Column::new(
                    "name".to_owned(),
                    "TEXT".to_owned(),
                    vec![
                        crate::db::Value::Text("ada".to_owned()),
                        crate::db::Value::Text("grace".to_owned()),
                    ],
                ),
                crate::db::Column::new(
                    "oid".to_owned(),
                    "INTEGER".to_owned(),
                    vec![crate::db::Value::Integer(10), crate::db::Value::Integer(20)],
                ),
                crate::db::Column::new(
                    "total".to_owned(),
                    "INTEGER".to_owned(),
                    vec![crate::db::Value::Integer(100), crate::db::Value::Integer(200)],
                ),
            ],
            false,
            None,
            std::time::Duration::from_millis(1),
        )
        .unwrap()
    }

    /// The left-joined shape where the second row has no orders side, so the
    /// `oid` and `total` cells are NULL.
    fn null_join_result() -> QueryResult {
        QueryResult::from_columns(
            vec![
                crate::db::Column::new(
                    "id".to_owned(),
                    "INTEGER".to_owned(),
                    vec![crate::db::Value::Integer(1), crate::db::Value::Integer(2)],
                ),
                crate::db::Column::new(
                    "name".to_owned(),
                    "TEXT".to_owned(),
                    vec![
                        crate::db::Value::Text("ada".to_owned()),
                        crate::db::Value::Text("grace".to_owned()),
                    ],
                ),
                crate::db::Column::new(
                    "oid".to_owned(),
                    "INTEGER".to_owned(),
                    vec![crate::db::Value::Integer(10), crate::db::Value::Null],
                ),
                crate::db::Column::new(
                    "total".to_owned(),
                    "INTEGER".to_owned(),
                    vec![crate::db::Value::Integer(100), crate::db::Value::Null],
                ),
            ],
            false,
            None,
            std::time::Duration::from_millis(1),
        )
        .unwrap()
    }

    /// An idle scripted connection with the join schema loaded and one tab
    /// showing the two-table join.
    fn join_tab(state: &mut AppState) -> (TabId, ConnectionId, mpsc::Receiver<DbCommand>) {
        let conn = add_sqlite_connection(state, "mem-join");
        let commands = attach_test_session(state, conn);
        state.apply(DbEvent::SchemaOk { conn, schema: join_schema() });
        let tab = state.new_tab(conn);
        let sql = "SELECT u.id, u.name, o.id AS oid, o.total \
                   FROM users u JOIN orders o ON o.user_id = u.id";
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
        state.apply(DbEvent::BatchOk { run, results: vec![join_result()] });
        (tab, conn, commands)
    }

    /// Point the tab's SQL at `sql` without touching its result, for
    /// resolver cases where the shape still lines up.
    fn set_tab_sql(state: &mut AppState, tab: TabId, sql: &str) {
        if let Some(slot) = state.tab_sql_mut(tab) {
            *slot = sql.to_owned();
        }
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
    fn activate_relative_wraps_and_activate_nth_follows_the_browser_rule() {
        with_isolated_state("tab-chords", |state| {
            state.activate_relative(1);
            state.activate_nth(1);
            assert_eq!(state.active(), None, "no tabs to switch to");

            let conn = add_sqlite_connection(state, "mem-chords");
            let first = state.new_tab(conn);
            let second = state.new_tab(conn);
            let third = state.new_tab(conn);
            assert_eq!(state.active(), Some(first), "the first tab starts active");

            state.activate_relative(1);
            assert_eq!(state.active(), Some(second));
            state.activate_relative(1);
            assert_eq!(state.active(), Some(third));
            state.activate_relative(1);
            assert_eq!(state.active(), Some(first), "forward wraps");
            state.activate_relative(-1);
            assert_eq!(state.active(), Some(third), "backward wraps");

            state.activate_nth(2);
            assert_eq!(state.active(), Some(second));
            state.activate_nth(1);
            assert_eq!(state.active(), Some(first));
            state.activate_nth(3);
            assert_eq!(state.active(), Some(third));
            state.activate_nth(9);
            assert_eq!(state.active(), Some(third), "9 selects the last tab");
            state.activate_nth(4);
            assert_eq!(state.active(), Some(third), "past the end is a no-op");
            state.activate_nth(0);
            assert_eq!(state.active(), Some(third), "0 is out of range");
        });
    }

    #[test]
    fn filter_clause_sql_covers_every_operator_with_literal_typing() {
        let clause = |column: &str, op: FilterOp, value: &str| FilterClause {
            column: column.to_owned(),
            op,
            value: value.to_owned(),
        };
        assert_eq!(
            clause_sql(&clause("age", FilterOp::Eq, "30")),
            Some("\"age\" = 30".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("name", FilterOp::Eq, "Ada")),
            Some("\"name\" = 'Ada'".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("name", FilterOp::Eq, "O'Brien")),
            Some("\"name\" = 'O''Brien'".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("score", FilterOp::Eq, "2.5")),
            Some("\"score\" = 2.5".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("label", FilterOp::Eq, "NaN")),
            Some("\"label\" = 'NaN'".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("age", FilterOp::NotEq, "30")),
            Some("\"age\" <> 30".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("age", FilterOp::Lt, "30")),
            Some("\"age\" < 30".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("age", FilterOp::LtEq, "30")),
            Some("\"age\" <= 30".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("age", FilterOp::Gt, "30")),
            Some("\"age\" > 30".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("age", FilterOp::GtEq, "30")),
            Some("\"age\" >= 30".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("name", FilterOp::Like, "Ada%")),
            Some("\"name\" LIKE 'Ada%'".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("id", FilterOp::In, "1, 2, ada")),
            Some("\"id\" IN (1, 2, 'ada')".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("name", FilterOp::IsNull, "ignored")),
            Some("\"name\" IS NULL".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("name", FilterOp::IsNotNull, "")),
            Some("\"name\" IS NOT NULL".to_owned())
        );
        assert_eq!(
            clause_sql(&clause("user name", FilterOp::Eq, "7")),
            Some("\"user name\" = 7".to_owned())
        );
        assert_eq!(clause_sql(&clause("", FilterOp::Eq, "30")), None);
        assert_eq!(clause_sql(&clause("  ", FilterOp::Eq, "30")), None);
        assert_eq!(clause_sql(&clause("age", FilterOp::Eq, "")), None);
        assert_eq!(clause_sql(&clause("age", FilterOp::Gt, "   ")), None);
        assert_eq!(clause_sql(&clause("id", FilterOp::In, "")), None);
        assert_eq!(clause_sql(&clause("id", FilterOp::In, " , ")), None);
    }

    #[test]
    fn filter_where_joins_clauses_and_appends_raw_verbatim() {
        let clause = FilterClause {
            column: "age".to_owned(),
            op: FilterOp::Gt,
            value: "30".to_owned(),
        };
        assert_eq!(
            filter_where(&[clause.clone()], "name <> 'x'"),
            Some("\"age\" > 30 AND (name <> 'x')".to_owned())
        );
        assert_eq!(
            filter_where(&[clause.clone()], ""),
            Some("\"age\" > 30".to_owned())
        );
        assert_eq!(
            filter_where(&[], "id > 1"),
            Some("(id > 1)".to_owned())
        );
        assert_eq!(filter_where(&[], "   "), None);
        assert_eq!(filter_where(&[], ""), None);
        let blank = FilterClause {
            column: String::new(),
            op: FilterOp::Eq,
            value: "1".to_owned(),
        };
        assert_eq!(
            filter_where(&[blank, clause.clone()], ""),
            Some("\"age\" > 30".to_owned())
        );
        assert_eq!(filter_where(&[clause], "  padded  "), Some("\"age\" > 30 AND (padded)".to_owned()));
    }

    #[test]
    fn filtered_table_select_quotes_tables_and_caps_rows() {
        let clause = FilterClause {
            column: "age".to_owned(),
            op: FilterOp::GtEq,
            value: "30".to_owned(),
        };
        assert_eq!(
            filtered_table_select("public.users", &[clause], ""),
            "SELECT * FROM \"public\".\"users\" WHERE \"age\" >= 30 LIMIT 100"
        );
        assert_eq!(
            filtered_table_select("users", &[], "id > 1"),
            "SELECT * FROM \"users\" WHERE (id > 1) LIMIT 100"
        );
        assert_eq!(
            filtered_table_select("users", &[], ""),
            "SELECT * FROM \"users\" LIMIT 100"
        );
    }

    #[test]
    fn filter_history_save_recall_and_delete_roundtrip() {
        with_isolated_state("filter-hist", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT * FROM users".to_owned();
            }
            state.tab_mut(tab).expect("tab").add_filter_clause(FilterClause {
                column: "age".to_owned(),
                op: FilterOp::Gt,
                value: "30".to_owned(),
            });
            state.tab_mut(tab).expect("tab").set_filter_raw("id > 1".to_owned());
            assert!(state.save_filter(tab, "  seniors  "));
            assert_eq!(state.filter_history().len(), 1);
            assert_eq!(state.filter_history()[0].name, "seniors");
            assert!(!state.save_filter(tab, "   "));
            assert!(!state.save_filter(tab, ""));

            state.tab_mut(tab).expect("tab").clear_table_filter();
            assert!(state.tab(tab).expect("tab").filter_clauses().is_empty());
            assert!(state.tab(tab).expect("tab").filter_where().is_none());
            assert!(state.recall_filter(tab, "seniors"));
            let view = state.tab(tab).expect("tab");
            assert_eq!(view.filter_clauses().len(), 1);
            assert_eq!(view.filter_clauses()[0].column, "age");
            assert_eq!(view.filter_clauses()[0].op, FilterOp::Gt);
            assert_eq!(view.filter_raw(), "id > 1");
            assert_eq!(
                view.filter_where(),
                Some("\"age\" > 30 AND (id > 1)".to_owned())
            );
            assert!(!state.recall_filter(tab, "missing"));

            state.tab_mut(tab).expect("tab").set_filter_raw("id > 2".to_owned());
            assert!(state.save_filter(tab, "seniors"));
            assert_eq!(state.filter_history().len(), 1, "a duplicate name replaces");
            assert_eq!(state.filter_history()[0].raw, "id > 2");

            assert!(state.delete_filter("seniors"));
            assert!(state.filter_history().is_empty());
            assert!(!state.delete_filter("seniors"));
            assert!(!state.recall_filter(tab, "seniors"));
        });
    }

    #[test]
    fn filter_history_save_and_reload_roundtrip() {
        with_isolated_state("filter-persist", |state| {
            assert!(state.filter_history().is_empty());
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.tab_mut(tab).expect("tab").add_filter_clause(FilterClause {
                column: "age".to_owned(),
                op: FilterOp::Gt,
                value: "30".to_owned(),
            });
            state.tab_mut(tab).expect("tab").set_filter_raw("id > 1".to_owned());
            assert!(state.save_filter(tab, "seniors"));
            assert_eq!(state.filter_history().len(), 1);
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.filter_history().len(), 1);
            assert_eq!(reloaded.filter_history()[0].name, "seniors");
            assert_eq!(
                reloaded.filter_history()[0].clauses,
                vec![FilterClause {
                    column: "age".to_owned(),
                    op: FilterOp::Gt,
                    value: "30".to_owned(),
                }]
            );
            assert_eq!(reloaded.filter_history()[0].raw, "id > 1");
            assert!(state.delete_filter("seniors"));
            let emptied = AppState::new(&eframe::egui::Context::default());
            assert!(emptied.filter_history().is_empty());
        });
    }

    #[test]
    fn old_filter_file_without_raw_loads_empty_filter() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("cde-filter-old-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crusty-data-explorer")).unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        std::fs::write(
            dir.join("crusty-data-explorer").join("saved_filters.json"),
            br#"[{"name":"legacy","clauses":[{"column":"age","op":"Gt","value":"30"}]},{"name":"bare"},{"name":"minimal","clauses":[{"column":"age"}]}]"#,
        )
        .unwrap();
        let state = AppState::new(&eframe::egui::Context::default());
        assert_eq!(state.filter_history().len(), 3);
        assert_eq!(state.filter_history()[0].name, "legacy");
        assert_eq!(
            state.filter_history()[0].clauses,
            vec![FilterClause {
                column: "age".to_owned(),
                op: FilterOp::Gt,
                value: "30".to_owned(),
            }]
        );
        assert_eq!(state.filter_history()[0].raw, "");
        assert_eq!(state.filter_history()[1].name, "bare");
        assert!(state.filter_history()[1].clauses.is_empty());
        assert_eq!(state.filter_history()[1].raw, "");
        assert_eq!(state.filter_history()[2].name, "minimal");
        assert_eq!(
            state.filter_history()[2].clauses,
            vec![FilterClause {
                column: "age".to_owned(),
                op: FilterOp::Eq,
                value: String::new(),
            }]
        );
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filter_history_cap_holds_across_reload() {
        with_isolated_state("filter-cap", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            for i in 0..60 {
                assert!(state.save_filter(tab, &format!("f{i}")));
            }
            assert_eq!(state.filter_history().len(), 50);
            assert_eq!(state.filter_history()[0].name, "f59");
            assert_eq!(state.filter_history()[49].name, "f10");
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.filter_history().len(), 50);
            assert_eq!(reloaded.filter_history()[0].name, "f59");
            assert_eq!(reloaded.filter_history()[49].name, "f10");
            let many: Vec<SavedFilter> = (0..60)
                .map(|i| SavedFilter {
                    name: format!("g{i}"),
                    clauses: Vec::new(),
                    raw: String::new(),
                })
                .collect();
            save_saved_filters(&many).expect("filters save");
            let loaded = load_saved_filters();
            assert_eq!(loaded.len(), 50);
            assert_eq!(loaded[0].name, "g0");
            assert_eq!(loaded[49].name, "g49");
        });
    }

    #[test]
    fn table_filter_select_needs_a_single_table_select() {
        with_isolated_state("filter-select", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT * FROM users JOIN orders ON orders.id = users.id".to_owned();
            }
            state.tab_mut(tab).expect("tab").add_filter_clause(FilterClause {
                column: "age".to_owned(),
                op: FilterOp::Gt,
                value: "30".to_owned(),
            });
            assert_eq!(state.table_filter_select(tab), None);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT * FROM users".to_owned();
            }
            assert_eq!(
                state.table_filter_select(tab),
                Some("SELECT * FROM \"users\" WHERE \"age\" > 30 LIMIT 100".to_owned())
            );
            state.clear_table_filter(tab);
            assert_eq!(
                state.table_filter_select(tab),
                Some("SELECT * FROM \"users\" LIMIT 100".to_owned())
            );
        });
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_structured_filter_narrows_a_scratch_table() {
        with_isolated_state("pg-filter-live", |state| {
            let conn = add_postgres_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live Postgres"
            );
            let tab = state.new_tab(conn);
            live_run(state, tab, "DROP TABLE IF EXISTS cde_filter_probe");
            live_run(state, tab, "CREATE TABLE cde_filter_probe (id INT PRIMARY KEY, name TEXT NOT NULL)");
            live_run(
                state,
                tab,
                "INSERT INTO cde_filter_probe VALUES (1, 'ada'), (2, 'grace'), (3, 'adam')",
            );
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT * FROM cde_filter_probe".to_owned();
            }
            state.tab_mut(tab).expect("tab").add_filter_clause(FilterClause {
                column: "name".to_owned(),
                op: FilterOp::Like,
                value: "ada%".to_owned(),
            });
            state.tab_mut(tab).expect("tab").set_filter_raw("id > 1".to_owned());
            let sql = state.table_filter_select(tab).expect("filtered SELECT");
            assert_eq!(
                sql,
                "SELECT * FROM \"cde_filter_probe\" WHERE \"name\" LIKE 'ada%' AND (id > 1) LIMIT 100"
            );
            state.apply_table_filter(tab);
            settle(state, tab, "the filtered SELECT");
            let result =
                state.tab(tab).expect("tab").active_query_result().expect("a result");
            assert_eq!(result.row_count(), 1, "only adam survives the clause plus the raw box");
            assert_eq!(
                result.cell(0, 1),
                Some(&Value::Text("adam".to_owned())),
                "the surviving row is adam"
            );
            live_run(state, tab, "DROP TABLE cde_filter_probe");
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
    fn structured_filter_header_renders_over_a_grid() {
        with_isolated_state("filter-render", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT * FROM users".to_owned();
            }
            let run = start_running(state, tab, &["SELECT * FROM users"]);
            state.apply(DbEvent::BatchOk { run, results: vec![rows_result(2)] });
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
                crate::ui::grid::results(state, ui, None);
            });
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "Filters"), "{text:?}");
            assert!(!text.iter().any(|t| t == "Add clause"), "{text:?}");

            // A clause opens the structured filter body by default.
            state.tab_mut(tab).expect("tab").add_filter_clause(FilterClause {
                column: "name".to_owned(),
                op: FilterOp::Eq,
                value: String::new(),
            });
            let mut output = ctx.run_ui(input, |ui| {
                crate::ui::grid::results(state, ui, None);
            });
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "Filters (1)"), "{text:?}");
            assert!(text.iter().any(|t| t == "Add clause"), "{text:?}");
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
                crate::ui::grid::results(state, ui, None);
            });
            grid.textures_delta.clear();
            let grid_text = rendered_text(&grid);
            assert!(grid_text.iter().any(|t| t == "Result 1"), "{grid_text:?}");
            assert!(grid_text.iter().any(|t| t == "Result 2"), "{grid_text:?}");
            assert!(
                grid_text.iter().any(|t| t == "2 of 2 rows · 2 ms"),
                "{grid_text:?}"
            );

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
                crate::ui::grid::results(state, ui, None);
            });
            switched.textures_delta.clear();
            let switched_text = rendered_text(&switched);
            assert!(
                switched_text.iter().any(|t| t == "3 of 3 rows · 2 ms"),
                "{switched_text:?}"
            );
            assert!(
                !switched_text.iter().any(|t| t == "2 of 2 rows · 2 ms"),
                "{switched_text:?}"
            );
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
    fn history_scope_defaults_to_all_and_toggles() {
        with_isolated_state("hist-scope", |state| {
            assert!(!state.history_current_only());
            state.set_history_current_only(true);
            assert!(state.history_current_only());
            state.toggle_history_scope();
            assert!(!state.history_current_only());
            state.toggle_history_scope();
            assert!(state.history_current_only());
            state.set_history_current_only(false);
            assert!(!state.history_current_only());
        });
    }

    #[test]
    fn history_scope_connection_follows_the_active_tab() {
        with_isolated_state("hist-scope-conn", |state| {
            assert_eq!(state.history_scope_connection(), None);
            let a = add_sqlite_connection(state, "a");
            let b = add_sqlite_connection(state, "b");
            finish_ok(state, a, "SELECT a");
            finish_ok(state, b, "SELECT b");
            state.set_history_current_only(true);
            assert!(state.history_current_only());
            let tab = state.new_tab(a);
            state.activate(tab);
            assert_eq!(state.history_scope_connection(), Some(a));
            let scoped = state.history_for(state.history_scope_connection().expect("conn"));
            assert_eq!(scoped.len(), 1);
            assert_eq!(scoped[0].sql, "SELECT a");
            let other = state.new_tab(b);
            state.activate(other);
            assert_eq!(state.history_scope_connection(), Some(b));
            let scoped = state.history_for(state.history_scope_connection().expect("conn"));
            assert_eq!(scoped.len(), 1);
            assert_eq!(scoped[0].sql, "SELECT b");
        });
    }

    #[test]
    fn history_stats_renders_outcome_rows_and_timing() {
        let ok = HistoryEntry {
            conn: ConnectionId::from_raw(1),
            sql: "SELECT 1".to_owned(),
            succeeded: true,
            rows: Some(3),
            took: std::time::Duration::from_millis(42),
            at: 1,
        };
        assert_eq!(history_stats(&ok), "ok · 3 rows · 42 ms");
        let affected = HistoryEntry {
            conn: ConnectionId::from_raw(1),
            sql: "DELETE FROM t".to_owned(),
            succeeded: true,
            rows: Some(1),
            took: std::time::Duration::from_millis(5),
            at: 2,
        };
        assert_eq!(history_stats(&affected), "ok · 1 rows · 5 ms");
        let failed = HistoryEntry {
            conn: ConnectionId::from_raw(1),
            sql: "SELECT bad".to_owned(),
            succeeded: false,
            rows: None,
            took: std::time::Duration::from_millis(7),
            at: 3,
        };
        assert_eq!(history_stats(&failed), "failed · 7 ms");
        assert_eq!(format_history_took(std::time::Duration::from_millis(2)), "2 ms");
    }

    #[test]
    fn history_rerun_is_live_only() {
        with_isolated_state("hist-rerun", |state| {
            let conn = add_sqlite_connection(state, "mem");
            finish_ok(state, conn, "SELECT 1");
            let entry = state.history()[0].clone();
            assert!(!state.can_rerun(&entry));
            assert!(!state.is_connection_live(conn));
            let _commands = attach_test_session(state, conn);
            assert!(state.is_connection_live(conn));
            assert!(state.can_rerun(&entry));
            state.disconnect(conn);
            assert!(!state.is_connection_live(conn));
            assert!(!state.can_rerun(&entry));
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
        assert!(
            SavedQuery::new(1, String::new(), "SELECT 1".to_owned(), conn, None, false)
                .is_err()
        );
        assert!(
            SavedQuery::new(
                1,
                "   ".to_owned(),
                "SELECT 1".to_owned(),
                conn,
                None,
                false
            )
            .is_err()
        );
        let query = SavedQuery::new(
            1,
            "daily".to_owned(),
            "SELECT 1".to_owned(),
            conn,
            Some("ops".to_owned()),
            true,
        )
        .unwrap();
        assert_eq!(query.id, 1);
        assert_eq!(query.name, "daily");
        assert_eq!(query.folder.as_deref(), Some("ops"));
        assert!(query.pinned);
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
            assert!(state.save_active_tab("   ", None, false).is_none());
            assert!(state.saved_queries().is_empty());
            let id = state.save_active_tab("first", None, false).expect("saved");
            assert_eq!(state.saved_queries().len(), 1);
            assert_eq!(state.saved_queries()[0].sql, "SELECT 1");
            assert_eq!(state.saved_queries()[0].conn, conn);
            assert_eq!(state.saved_queries()[0].folder, None);
            assert!(!state.saved_queries()[0].pinned);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 2".to_owned();
            }
            let again = state.save_active_tab("first", None, false).expect("replaced");
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
            assert_eq!(reloaded.saved_queries()[0].folder, None);
            assert!(!reloaded.saved_queries()[0].pinned);
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
                state.save_active_tab(&format!("q{i}"), None, false).expect("saved");
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
                folder: None,
                pinned: false,
                versions: Vec::new(),
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
    fn old_saved_file_without_folder_or_pinned_loads_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("cde-saved-old-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crusty-data-explorer")).unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        std::fs::write(
            dir.join("crusty-data-explorer").join("saved_queries.json"),
            br#"[{"id":7,"name":"daily","sql":"SELECT 1","conn":1}]"#,
        )
        .unwrap();
        let state = AppState::new(&eframe::egui::Context::default());
        assert_eq!(state.saved_queries().len(), 1);
        assert_eq!(state.saved_queries()[0].name, "daily");
        assert_eq!(state.saved_queries()[0].folder, None);
        assert!(!state.saved_queries()[0].pinned);
        assert!(state.saved_queries()[0].versions.is_empty());
        assert!(state.saved_versions(7).is_empty());
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_versions_append_on_save_newest_first() {
        with_isolated_state("saved-versions-append", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.activate(tab);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1".to_owned();
            }
            let id = state.save_active_tab("daily", None, false).expect("saved");
            assert_eq!(state.saved_versions(id).len(), 1);
            assert_eq!(state.saved_versions(id)[0].sql, "SELECT 1");
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 2".to_owned();
            }
            let again = state.save_active_tab("daily", None, false).expect("replaced");
            assert_eq!(id, again);
            let versions = state.saved_versions(id);
            assert_eq!(versions.len(), 2);
            assert_eq!(versions[0].sql, "SELECT 2");
            assert_eq!(versions[1].sql, "SELECT 1");
            assert_eq!(state.saved_queries()[0].sql, "SELECT 2");
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.saved_versions(id).len(), 2);
            assert_eq!(reloaded.saved_versions(id)[0].sql, "SELECT 2");
            assert_eq!(reloaded.saved_versions(id)[1].sql, "SELECT 1");
        });
    }

    #[test]
    fn saved_version_restore_roundtrip_brings_old_text_back() {
        with_isolated_state("saved-versions-restore", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.activate(tab);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1".to_owned();
            }
            let id = state.save_active_tab("daily", None, false).expect("saved");
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 2".to_owned();
            }
            state.save_active_tab("daily", None, false).expect("replaced");
            assert!(state.restore_saved_version(id, 1));
            assert_eq!(state.saved_queries()[0].sql, "SELECT 1");
            assert_eq!(state.saved_versions(id)[0].sql, "SELECT 1");
            state.open_saved(id);
            let opened = state.active().and_then(|t| state.tab(t)).expect("active tab");
            assert_eq!(opened.sql, "SELECT 1");
            assert!(!state.restore_saved_version(id, 99));
            assert!(!state.restore_saved_version(999_999, 0));
            assert!(state.saved_versions(999_999).is_empty());
        });
    }

    #[test]
    fn saved_version_open_shows_old_text_without_changing_saved() {
        with_isolated_state("saved-versions-open", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.activate(tab);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1".to_owned();
            }
            let id = state.save_active_tab("daily", None, false).expect("saved");
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 2".to_owned();
            }
            state.save_active_tab("daily", None, false).expect("replaced");
            state.open_saved_version(id, 1);
            let opened = state.active().and_then(|t| state.tab(t)).expect("active tab");
            assert_eq!(opened.sql, "SELECT 1");
            assert_eq!(state.saved_queries()[0].sql, "SELECT 2");
        });
    }

    #[test]
    fn saved_version_sidebar_restore_wiring_opens_restored_text() {
        with_isolated_state("saved-versions-sidebar-wiring", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.activate(tab);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1".to_owned();
            }
            let id = state.save_active_tab("daily", None, false).expect("saved");
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 2".to_owned();
            }
            state.save_active_tab("daily", None, false).expect("replaced");
            assert!(crate::ui::connections::restore_version_then_open(state, id, 1));
            let opened = state.active().and_then(|t| state.tab(t)).expect("active tab");
            assert_eq!(opened.sql, "SELECT 1");
            assert_eq!(state.saved_queries()[0].sql, "SELECT 1");
            assert!(!crate::ui::connections::restore_version_then_open(state, id, 99));
            assert!(!crate::ui::connections::restore_version_then_open(state, 999_999, 0));
        });
    }

    #[test]
    fn saved_versions_cap_at_20_newest_first() {
        with_isolated_state("saved-versions-cap", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.activate(tab);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 0".to_owned();
            }
            let id = state.save_active_tab("daily", None, false).expect("saved");
            for i in 1..30 {
                if let Some(slot) = state.tab_sql_mut(tab) {
                    *slot = format!("SELECT {i}");
                }
                state.save_active_tab("daily", None, false).expect("replaced");
            }
            let versions = state.saved_versions(id);
            assert_eq!(versions.len(), SAVED_VERSION_CAP);
            assert_eq!(versions[0].sql, "SELECT 29");
            assert_eq!(versions[SAVED_VERSION_CAP - 1].sql, "SELECT 10");
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.saved_versions(id).len(), SAVED_VERSION_CAP);
            assert_eq!(reloaded.saved_versions(id)[0].sql, "SELECT 29");
        });
    }

    #[test]
    fn saved_dedup_scopes_to_the_folder() {
        with_isolated_state("saved-folder-dedup", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.activate(tab);
            let ops = state.save_active_tab("daily", Some("ops"), false).expect("saved");
            let personal =
                state.save_active_tab("daily", Some("personal"), true).expect("saved");
            assert_ne!(ops, personal);
            assert_eq!(state.saved_queries().len(), 2);
            let again = state.save_active_tab("daily", Some("  ops  "), true).expect("replaced");
            assert_eq!(again, ops);
            assert_eq!(state.saved_queries().len(), 2);
            let entry = state.saved_queries().iter().find(|q| q.id == ops).unwrap();
            assert_eq!(entry.folder.as_deref(), Some("ops"));
            assert!(entry.pinned);
            let loose = state.save_active_tab("daily", None, false).expect("saved");
            assert_ne!(loose, ops);
            assert_eq!(state.saved_queries().len(), 3);
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.saved_queries().len(), 3);
            assert_eq!(
                reloaded.saved_queries().iter().filter(|q| q.name == "daily").count(),
                3
            );
        });
    }

    #[test]
    fn saved_cap_counts_across_folders() {
        with_isolated_state("saved-cap-folders", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.activate(tab);
            for i in 0..250 {
                let folder = if i % 2 == 0 { "evens" } else { "odds" };
                state
                    .save_active_tab(&format!("q{i}"), Some(folder), i % 3 == 0)
                    .expect("saved");
            }
            assert_eq!(state.saved_queries().len(), 200);
            assert_eq!(state.saved_queries()[0].name, "q249");
            assert_eq!(state.saved_queries()[199].name, "q50");
        });
    }

    #[test]
    fn saved_groups_order_folders_and_pins() {
        let conn = ConnectionId::from_raw(1);
        let query = |id: u64, name: &str, folder: Option<&str>, pinned: bool| {
            SavedQuery::new(
                id,
                name.to_owned(),
                "SELECT 1".to_owned(),
                conn,
                folder.map(str::to_owned),
                pinned,
            )
            .expect("named")
        };
        let saved = vec![
            query(1, "loose-old", None, false),
            query(2, "zeta-pinned", Some("zeta"), true),
            query(3, "alpha-plain", Some("alpha"), false),
            query(4, "loose-pinned", None, true),
            query(5, "zeta-plain", Some("zeta"), false),
            query(6, "alpha-pinned", Some("alpha"), true),
        ];
        let shape: Vec<(Option<&str>, Vec<&str>)> = saved_groups(&saved)
            .iter()
            .map(|g| (g.folder, g.entries.iter().map(|q| q.name.as_str()).collect()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (None, vec!["loose-pinned", "loose-old"]),
                (Some("alpha"), vec!["alpha-pinned", "alpha-plain"]),
                (Some("zeta"), vec!["zeta-pinned", "zeta-plain"]),
            ]
        );
    }

    #[test]
    fn ensured_empty_folders_sort_beside_entry_folders() {
        let conn = ConnectionId::from_raw(1);
        let saved = vec![
            SavedQuery::new(1, "q".to_owned(), "SELECT 1".to_owned(), conn, Some("ops".to_owned()), false)
                .expect("named"),
        ];
        let mut tree = saved_folder_tree(&saved);
        tree.ensure_path("alpha");
        tree.ensure_path("ops/db");
        assert_eq!(
            tree.children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "ops"]
        );
        let ops = &tree.children[1];
        assert_eq!(ops.entries.len(), 1);
        assert_eq!(ops.children.len(), 1);
        assert_eq!(ops.children[0].path, "ops/db");
        assert!(ops.children[0].entries.is_empty());
    }

    #[test]
    fn folder_paths_normalize_slashes_and_blanks() {        assert_eq!(normalize_folder_path(None), None);
        assert_eq!(normalize_folder_path(Some("")), None);
        assert_eq!(normalize_folder_path(Some("   ")), None);
        assert_eq!(normalize_folder_path(Some("///")), None);
        assert_eq!(normalize_folder_path(Some("ops")), Some("ops".to_owned()));
        assert_eq!(
            normalize_folder_path(Some(" //ops// db/ ")),
            Some("ops/db".to_owned())
        );
        assert_eq!(
            normalize_folder_path(Some("a/b/c")),
            Some("a/b/c".to_owned())
        );
        assert_eq!(normalize_folder_path(Some("a//b")), Some("a/b".to_owned()));
    }

    #[test]
    fn nested_folder_paths_group_into_a_tree_with_literal_shape() {
        let conn = ConnectionId::from_raw(1);
        let query = |id: u64, name: &str, folder: Option<&str>| {
            SavedQuery::new(
                id,
                name.to_owned(),
                "SELECT 1".to_owned(),
                conn,
                normalize_folder_path(folder),
                false,
            )
            .expect("named")
        };
        let saved = vec![
            query(1, "root-q", None),
            query(2, "db-q", Some("ops/db")),
            query(3, "api-q", Some("ops/api")),
            query(4, "ops-q", Some("ops")),
            query(5, "solo-q", Some("personal")),
        ];
        let tree = saved_folder_tree(&saved);
        assert_eq!(
            tree.entries.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(),
            vec!["root-q"]
        );
        assert_eq!(
            tree.children
                .iter()
                .map(|c| (c.name.as_str(), c.path.as_str()))
                .collect::<Vec<_>>(),
            vec![("ops", "ops"), ("personal", "personal")]
        );
        let ops = &tree.children[0];
        assert_eq!(
            ops.entries.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(),
            vec!["ops-q"]
        );
        assert_eq!(
            ops.children
                .iter()
                .map(|c| (c.name.as_str(), c.path.as_str()))
                .collect::<Vec<_>>(),
            vec![("api", "ops/api"), ("db", "ops/db")]
        );
        assert_eq!(ops.children[0].entries[0].name, "api-q");
        assert_eq!(ops.children[1].entries[0].name, "db-q");
        assert_eq!(tree.children[1].entries[0].name, "solo-q");
    }

    #[test]
    fn pin_precedence_holds_inside_nested_folders() {
        let conn = ConnectionId::from_raw(1);
        let query = |id: u64, name: &str, folder: Option<&str>, pinned: bool| {
            SavedQuery::new(
                id,
                name.to_owned(),
                "SELECT 1".to_owned(),
                conn,
                normalize_folder_path(folder),
                pinned,
            )
            .expect("named")
        };
        let saved = vec![
            query(1, "root-plain", None, false),
            query(2, "root-pinned", None, true),
            query(3, "nested-plain", Some("ops/db"), false),
            query(4, "nested-pinned", Some("ops/db"), true),
            query(5, "mid-plain", Some("ops"), false),
            query(6, "mid-pinned", Some("ops"), true),
        ];
        let tree = saved_folder_tree(&saved);
        assert_eq!(
            tree.entries.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(),
            vec!["root-pinned", "root-plain"]
        );
        let ops = tree.children.iter().find(|c| c.name == "ops").expect("ops node");
        assert_eq!(
            ops.entries.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(),
            vec!["mid-pinned", "mid-plain"]
        );
        let db = ops.children.iter().find(|c| c.name == "db").expect("db node");
        assert_eq!(db.path, "ops/db");
        assert_eq!(
            db.entries.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(),
            vec!["nested-pinned", "nested-plain"]
        );
    }

    #[test]
    fn move_saved_retargets_and_persists_across_restart() {
        with_isolated_state("saved-move", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1".to_owned();
            }
            state.activate(tab);
            let id = state.save_active_tab("daily", Some("ops"), false).expect("saved");
            assert!(state.move_saved(id, Some("ops/db")));
            assert_eq!(
                state.saved_queries().iter().find(|q| q.id == id).unwrap().folder.as_deref(),
                Some("ops/db")
            );
            assert!(state.move_saved(id, Some(" //ops// db/deep/ ")));
            assert_eq!(
                state.saved_queries().iter().find(|q| q.id == id).unwrap().folder.as_deref(),
                Some("ops/db/deep")
            );
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(
                reloaded.saved_queries().iter().find(|q| q.id == id).unwrap().folder.as_deref(),
                Some("ops/db/deep")
            );
            assert!(state.move_saved(id, None));
            assert_eq!(
                state.saved_queries().iter().find(|q| q.id == id).unwrap().folder,
                None
            );
            let back = AppState::new(&eframe::egui::Context::default());
            assert_eq!(
                back.saved_queries().iter().find(|q| q.id == id).unwrap().folder,
                None
            );
            assert!(!state.move_saved(999_999, Some("ops")));
        });
    }

    #[test]
    fn move_connection_retargets_and_persists_across_restart() {
        with_isolated_state("conn-move", |state| {
            let id = add_sqlite_connection(state, "mem");
            assert_eq!(state.connections().iter().find(|c| c.id == id).unwrap().folder(), None);
            assert!(state.move_connection(id, Some("team/a")));
            assert_eq!(
                state.connections().iter().find(|c| c.id == id).unwrap().folder(),
                Some("team/a")
            );
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(
                reloaded.connections().iter().find(|c| c.id == id).unwrap().folder(),
                Some("team/a")
            );
            assert!(state.move_connection(id, Some("  //team//b/ ")));
            assert_eq!(
                state.connections().iter().find(|c| c.id == id).unwrap().folder(),
                Some("team/b")
            );
            assert!(state.move_connection(id, None));
            assert_eq!(state.connections().iter().find(|c| c.id == id).unwrap().folder(), None);
            let back = AppState::new(&eframe::egui::Context::default());
            assert_eq!(back.connections().iter().find(|c| c.id == id).unwrap().folder(), None);
            assert!(!state.move_connection(ConnectionId::from_raw(999_999), Some("team")));
        });
    }

    #[test]
    fn connection_folder_tree_groups_nested_paths_with_literal_shape() {
        with_isolated_state("conn-tree", |state| {
            let first = add_sqlite_connection(state, "root-conn");
            let second = add_sqlite_connection(state, "db-conn");
            let third = add_sqlite_connection(state, "deep-conn");
            assert!(state.move_connection(second, Some("ops/db")));
            assert!(state.move_connection(third, Some("ops/db/deep")));
            let tree = connection_folder_tree(state.connections());
            assert_eq!(tree.entries.len(), 1);
            assert_eq!(tree.entries[0].id, first);
            assert_eq!(
                tree.children
                    .iter()
                    .map(|c| (c.name.as_str(), c.path.as_str()))
                    .collect::<Vec<_>>(),
                vec![("ops", "ops")]
            );
            let ops = &tree.children[0];
            assert!(ops.entries.is_empty());
            assert_eq!(ops.children.len(), 1);
            assert_eq!(ops.children[0].path, "ops/db");
            assert_eq!(ops.children[0].entries.len(), 1);
            assert_eq!(ops.children[0].entries[0].id, second);
            assert_eq!(ops.children[0].children.len(), 1);
            assert_eq!(ops.children[0].children[0].path, "ops/db/deep");
            assert_eq!(ops.children[0].children[0].entries[0].id, third);
        });
    }

    #[test]
    fn draft_folder_roundtrips_through_save_and_edit() {
        with_isolated_state("draft-folder", |state| {
            state.open_draft(None);
            {
                let draft = state.draft_mut().expect("new draft");
                draft.name = "nested".to_owned();
                draft.config = ConnectionConfig::Sqlite { read_only: false, path: ":memory:".into() };
                draft.folder = " //team// a/ ".to_owned();
            }
            state.save_draft();
            let id = state.connections().last().expect("saved").id;
            assert_eq!(
                state.connections().iter().find(|c| c.id == id).unwrap().folder(),
                Some("team/a")
            );
            state.open_draft(Some(id));
            assert_eq!(state.draft().expect("edit draft").folder, "team/a");
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(
                reloaded.connections().iter().find(|c| c.id == id).unwrap().folder(),
                Some("team/a")
            );
        });
    }

    #[test]
    fn old_files_without_folders_load_at_root() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("cde-folders-old-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crusty-data-explorer")).unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        std::fs::write(
            dir.join("crusty-data-explorer").join("saved_queries.json"),
            br#"[{"id":7,"name":"daily","sql":"SELECT 1","conn":1},{"id":8,"name":"flat","sql":"SELECT 2","conn":1,"folder":"ops"}]"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("crusty-data-explorer").join("connections.json"),
            br#"[{"id":1,"name":"pg","config":{"kind":"sqlite","read_only":false,"path":":memory:"}}]"#,
        )
        .unwrap();
        let state = AppState::new(&eframe::egui::Context::default());
        assert_eq!(state.saved_queries().len(), 2);
        assert_eq!(state.saved_queries()[0].folder, None);
        assert_eq!(state.saved_queries()[1].folder.as_deref(), Some("ops"));
        let tree = saved_folder_tree(state.saved_queries());
        assert_eq!(tree.entries.len(), 1);
        assert_eq!(tree.entries[0].name, "daily");
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].name, "ops");
        assert_eq!(tree.children[0].entries[0].name, "flat");
        assert_eq!(state.connections().len(), 1);
        assert_eq!(state.connections()[0].folder(), None);
        let conn_tree = connection_folder_tree(state.connections());
        assert_eq!(conn_tree.entries.len(), 1);
        assert!(conn_tree.children.is_empty());
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sidebar_renders_nested_folders_with_their_entries() {
        with_isolated_state("sidebar-tree", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let other = add_sqlite_connection(state, "other");
            assert!(state.move_connection(other, Some("ops/db")));
            let tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1".to_owned();
            }
            state.activate(tab);
            state.save_active_tab("root-q", None, false).expect("saved");
            state.save_active_tab("nested-q", Some("ops/db"), true).expect("saved");
            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input.clone(), |ui| {
                crate::ui::connections::sidebar(state, ui);
            });
            output.textures_delta.clear();
            let second = ctx.run_ui(input.clone(), |ui| {
                crate::ui::connections::sidebar(state, ui);
            });
            let text = rendered_text(&second);
            for wanted in ["CONNECTIONS", "mem", "other", "ops", "db"] {
                assert!(text.iter().any(|t| t.contains(wanted)), "{wanted} paints in {text:?}");
            }

            // The saved tree lives in the Saved view.
            state.set_sidebar_view(SidebarView::Saved);
            output.textures_delta.clear();
            let mut saved = ctx.run_ui(input, |ui| {
                crate::ui::connections::sidebar(state, ui);
            });
            saved.textures_delta.clear();
            let text = rendered_text(&saved);
            for wanted in ["SAVED", "root-q", "nested-q", "ops", "db"] {
                assert!(text.iter().any(|t| t.contains(wanted)), "{wanted} paints in {text:?}");
            }
        });
    }

    fn search_candidate(
        result: SearchResult,
        name: &str,
        extra: Option<&str>,
    ) -> SearchCandidate {
        SearchCandidate { result, name: name.to_owned(), extra: extra.map(str::to_owned) }
    }

    fn key_event(
        key: eframe::egui::Key,
        modifiers: eframe::egui::Modifiers,
    ) -> eframe::egui::Event {
        eframe::egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn palette_matcher_ranks_prefix_word_start_and_subsequence() {
        let conn = ConnectionId::from_raw(1);
        let table = |name: &str| SearchResult::Table {
            conn,
            schema: "main".to_owned(),
            table: name.to_owned(),
        };
        let column = |name: &str| SearchResult::Column {
            conn,
            schema: "main".to_owned(),
            table: "items".to_owned(),
            column: name.to_owned(),
        };
        let candidates = vec![
            search_candidate(
                SearchResult::Connection(conn),
                "big time machine",
                Some("SQLite"),
            ),
            search_candidate(
                SearchResult::Connection(ConnectionId::from_raw(2)),
                "prod items",
                Some("Postgres"),
            ),
            search_candidate(SearchResult::Saved(7), "Item Report", Some("reports")),
            search_candidate(table("items"), "items", None),
            search_candidate(column("item_id"), "item_id", None),
            search_candidate(table("metrics"), "metrics", None),
        ];
        assert_eq!(
            rank_candidates("item", &candidates),
            vec![
                SearchResult::Saved(7),
                table("items"),
                column("item_id"),
                SearchResult::Connection(ConnectionId::from_raw(2)),
                SearchResult::Connection(conn),
            ]
        );
    }

    #[test]
    fn palette_matcher_reads_saved_folders_and_connection_kinds() {
        let conn = ConnectionId::from_raw(1);
        let candidates = vec![
            search_candidate(SearchResult::Connection(conn), "analytics", Some("Postgres")),
            search_candidate(SearchResult::Saved(4), "daily rollup", Some("reports")),
        ];
        assert_eq!(
            rank_candidates("postgres", &candidates),
            vec![SearchResult::Connection(conn)]
        );
        assert_eq!(rank_candidates("reports", &candidates), vec![SearchResult::Saved(4)]);
    }

    #[test]
    fn palette_matcher_caps_rows_at_fifty() {
        let conn = ConnectionId::from_raw(1);
        let candidates: Vec<SearchCandidate> = (0..60)
            .map(|i| {
                let name = format!("item_{i:02}");
                search_candidate(
                    SearchResult::Table {
                        conn,
                        schema: "main".to_owned(),
                        table: name.clone(),
                    },
                    &name,
                    None,
                )
            })
            .collect();
        let ranked = rank_candidates("item", &candidates);
        assert_eq!(ranked.len(), SEARCH_CAP);
        assert_eq!(ranked.len(), 50);
        assert_eq!(
            ranked.first(),
            Some(&SearchResult::Table {
                conn,
                schema: "main".to_owned(),
                table: "item_00".to_owned(),
            })
        );
        assert_eq!(
            ranked.last(),
            Some(&SearchResult::Table {
                conn,
                schema: "main".to_owned(),
                table: "item_49".to_owned(),
            })
        );
    }

    #[test]
    fn palette_matcher_empty_query_matches_nothing() {
        let candidates = vec![search_candidate(
            SearchResult::Connection(ConnectionId::from_raw(1)),
            "prod",
            Some("Postgres"),
        )];
        assert!(rank_candidates("", &candidates).is_empty());
        assert!(rank_candidates("   ", &candidates).is_empty());
    }

    #[test]
    fn palette_opens_fresh_and_clears_on_reopen() {
        with_isolated_state("palette-toggle", |state| {
            add_sqlite_connection(state, "prod db");
            state.toggle_palette();
            assert!(state.palette_open());
            let palette = state.palette().expect("palette");
            assert_eq!(palette.query, "");
            assert_eq!(palette.selected, 0);
            assert!(palette.results.is_empty(), "a fresh palette lists nothing");
            state.set_palette_query("it");
            assert!(!state.palette().expect("palette").results.is_empty());
            state.toggle_palette();
            assert!(!state.palette_open());
            state.open_palette();
            let palette = state.palette().expect("palette");
            assert_eq!(palette.query, "");
            assert!(palette.results.is_empty(), "reopening clears query and rows");
        });
    }

    #[test]
    fn palette_query_filters_connections_saved_and_schema() {
        with_isolated_state("palette-filter", |state| {
            let conn = add_sqlite_connection(state, "prod db");
            let other = add_sqlite_connection(state, "warehouse");
            let _commands = attach_test_session(state, conn);
            state.apply(DbEvent::SchemaOk { conn, schema: items_schema() });
            let tab = state.new_tab(conn);
            state.activate(tab);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1".to_owned();
            }
            let saved = state.save_active_tab("item report", Some("reports"), false).expect("saved");
            state.open_palette();
            state.set_palette_query("item");
            assert_eq!(
                state.palette().expect("palette").results,
                vec![
                    SearchResult::Saved(saved),
                    SearchResult::Table {
                        conn,
                        schema: "main".to_owned(),
                        table: "items".to_owned(),
                    },
                ]
            );
            state.set_palette_query("reports");
            assert_eq!(
                state.palette().expect("palette").results,
                vec![SearchResult::Saved(saved)]
            );
            state.set_palette_query("warehouse");
            assert_eq!(
                state.palette().expect("palette").results,
                vec![SearchResult::Connection(other)]
            );
            state.set_palette_query("name");
            assert_eq!(
                state.palette().expect("palette").results,
                vec![SearchResult::Column {
                    conn,
                    schema: "main".to_owned(),
                    table: "items".to_owned(),
                    column: "name".to_owned(),
                }]
            );
        });
    }

    #[test]
    fn palette_selection_moves_and_wraps() {
        with_isolated_state("palette-select", |state| {
            let conn = add_sqlite_connection(state, "prod");
            let _commands = attach_test_session(state, conn);
            state.apply(DbEvent::SchemaOk { conn, schema: items_schema() });
            let tab = state.new_tab(conn);
            state.activate(tab);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1".to_owned();
            }
            state.save_active_tab("item report", None, false).expect("saved");
            state.open_palette();
            state.set_palette_query("item");
            assert_eq!(state.palette().expect("palette").results.len(), 2);
            assert_eq!(state.palette().expect("palette").selected, 0);
            state.move_palette_selection(1);
            assert_eq!(state.palette().expect("palette").selected, 1, "down moves");
            state.move_palette_selection(1);
            assert_eq!(state.palette().expect("palette").selected, 0, "down wraps");
            state.move_palette_selection(-1);
            assert_eq!(state.palette().expect("palette").selected, 1, "up wraps");
            state.set_palette_query("prod");
            assert_eq!(state.palette().expect("palette").results.len(), 1);
            assert_eq!(state.palette().expect("palette").selected, 0, "a shorter list clamps");
        });
    }

    #[test]
    fn palette_connection_row_opens_and_selects_its_tab() {
        with_isolated_state("palette-open-conn", |state| {
            let first = add_sqlite_connection(state, "first");
            let second = add_sqlite_connection(state, "second");
            let tab = state.new_tab(first);
            state.activate(tab);
            state.open_palette();
            state.set_palette_query("second");
            assert_eq!(
                state.palette().expect("palette").results,
                vec![SearchResult::Connection(second)]
            );
            assert!(state.activate_palette_selection(), "the row opens");
            assert!(!state.palette_open(), "opening closes the palette");
            let opened = state.active().expect("active tab");
            assert_ne!(opened, tab);
            assert_eq!(state.tab(opened).expect("tab").conn, Some(second));
            assert!(matches!(
                state.tab(opened).expect("tab").outcome(),
                RunOutcome::Never
            ));
            state.open_palette();
            state.set_palette_query("second");
            assert!(state.activate_palette_selection());
            assert_eq!(state.active(), Some(opened), "an existing tab activates");
            assert_eq!(state.tabs().len(), 2);
        });
    }

    #[test]
    fn palette_saved_row_opens_its_tab() {
        with_isolated_state("palette-open-saved", |state| {
            let conn = add_sqlite_connection(state, "prod");
            let tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 42".to_owned();
            }
            state.activate(tab);
            state
                .save_active_tab("daily rollup", Some("reports"), false)
                .expect("saved");
            state.open_palette();
            state.set_palette_query("rollup");
            assert!(state.activate_palette_selection(), "the row opens");
            let opened = state.active().and_then(|id| state.tab(id)).expect("active tab");
            assert_ne!(opened.id, tab);
            assert_eq!(opened.sql, "SELECT 42");
            assert_eq!(opened.conn, Some(conn));
            assert!(matches!(opened.outcome(), RunOutcome::Never));
        });
    }

    #[test]
    fn palette_table_row_opens_a_running_table_view() {
        with_isolated_state("palette-open-table", |state| {
            let conn = add_sqlite_connection(state, "prod");
            let commands = attach_test_session(state, conn);
            state.apply(DbEvent::SchemaOk { conn, schema: items_schema() });
            let tab = state.new_tab(conn);
            state.activate(tab);
            state.open_palette();
            state.set_palette_query("items");
            assert!(state.activate_palette_selection(), "the row opens");
            let opened = state.active().and_then(|id| state.tab(id)).expect("active tab");
            assert_ne!(opened.id, tab);
            assert_eq!(opened.sql, "SELECT * FROM \"main\".\"items\" LIMIT 100");
            assert_eq!(opened.conn, Some(conn));
            assert_eq!(
                opened.kind,
                TabKind::Table { schema: "main".to_owned(), table: "items".to_owned() }
            );
            assert!(matches!(opened.outcome(), RunOutcome::Running { .. }));
            match commands.try_recv().expect("the table run was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["SELECT * FROM \"main\".\"items\" LIMIT 100"]);
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn palette_column_row_reveals_its_table_in_the_navigator() {
        with_isolated_state("palette-open-column", |state| {
            let conn = add_sqlite_connection(state, "prod");
            let _commands = attach_test_session(state, conn);
            state.apply(DbEvent::SchemaOk { conn, schema: items_schema() });
            let tab = state.new_tab(conn);
            state.activate(tab);
            state.open_palette();
            state.set_palette_query("name");
            assert!(state.activate_palette_selection(), "the row opens");
            assert_eq!(
                state.navigator_reveal(),
                Some(&NavigatorReveal {
                    conn,
                    schema: "main".to_owned(),
                    table: "items".to_owned(),
                })
            );
            assert_eq!(state.active(), Some(tab), "a column opens no tab");
            assert_eq!(state.tabs().len(), 1);
            assert!(!state.palette_open());
            state.clear_navigator_reveal();
            assert!(state.navigator_reveal().is_none());
        });
    }

    #[test]
    fn palette_window_renders_over_the_root_with_rows() {
        with_isolated_state("palette-render", |state| {
            let conn = add_sqlite_connection(state, "prod");
            let _commands = attach_test_session(state, conn);
            state.apply(DbEvent::SchemaOk { conn, schema: items_schema() });
            let tab = state.new_tab(conn);
            state.activate(tab);
            state.open_palette();
            state.set_palette_query("item");
            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            // A fresh egui window paints from its second frame on.
            let mut first = ctx.run_ui(input.clone(), |ui| crate::ui::root(state, ui));
            first.textures_delta.clear();
            let mut output = ctx.run_ui(input, |ui| crate::ui::root(state, ui));
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "Search"), "{text:?}");
            assert!(text.iter().any(|t| t == "items  ·  main"), "{text:?}");
        });
    }

    #[test]
    fn compare_window_renders_over_the_root_when_opened() {
        with_isolated_state("compare-render", |state| {
            let conn = add_sqlite_connection(state, "prod");
            let _commands = attach_test_session(state, conn);
            state.apply(DbEvent::SchemaOk { conn, schema: items_schema() });
            let tab = state.new_tab(conn);
            state.activate(tab);
            assert!(!state.compare_window_open());
            state.open_compare();
            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            // A fresh egui window paints from its second frame on.
            let mut first = ctx.run_ui(input.clone(), |ui| crate::ui::root(state, ui));
            first.textures_delta.clear();
            let mut output = ctx.run_ui(input, |ui| crate::ui::root(state, ui));
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "Table compare"), "{text:?}");
            assert!(text.iter().any(|t| t == "Left"), "{text:?}");
            assert!(text.iter().any(|t| t == "Right"), "{text:?}");
            assert!(text.iter().any(|t| t == "Keys"), "{text:?}");
            state.close_compare();
            assert!(!state.compare_window_open());
        });
    }

    #[test]
    fn palette_keyboard_flow_filters_moves_and_opens() {
        with_isolated_state("palette-keys", |state| {
            let conn = add_sqlite_connection(state, "prod");
            let _commands = attach_test_session(state, conn);
            state.apply(DbEvent::SchemaOk { conn, schema: items_schema() });
            let tab = state.new_tab(conn);
            state.activate(tab);
            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let frame = |state: &mut AppState, events: Vec<eframe::egui::Event>| {
                let mut raw = input.clone();
                raw.events = events;
                let mut output = ctx.run_ui(raw, |ui| crate::ui::root(state, ui));
                output.textures_delta.clear();
                output
            };
            frame(
                state,
                vec![key_event(eframe::egui::Key::K, eframe::egui::Modifiers::COMMAND)],
            );
            assert!(state.palette_open(), "Ctrl+K opens");
            frame(state, vec![eframe::egui::Event::Text("item".to_owned())]);
            assert_eq!(state.palette().expect("palette").query, "item");
            assert_eq!(
                state.palette().expect("palette").results,
                vec![SearchResult::Table {
                    conn,
                    schema: "main".to_owned(),
                    table: "items".to_owned(),
                }]
            );
            frame(
                state,
                vec![key_event(eframe::egui::Key::Enter, eframe::egui::Modifiers::NONE)],
            );
            assert!(!state.palette_open(), "Enter closes the palette");
            let opened = state.active().and_then(|id| state.tab(id)).expect("active tab");
            assert_eq!(opened.sql, "SELECT * FROM \"main\".\"items\" LIMIT 100");
            assert_eq!(state.tabs().len(), 2);

            frame(
                state,
                vec![key_event(eframe::egui::Key::K, eframe::egui::Modifiers::COMMAND)],
            );
            frame(state, vec![eframe::egui::Event::Text("e".to_owned())]);
            assert_eq!(state.palette().expect("palette").results.len(), 4);
            frame(
                state,
                vec![key_event(eframe::egui::Key::ArrowDown, eframe::egui::Modifiers::NONE)],
            );
            assert_eq!(state.palette().expect("palette").selected, 1);
            frame(
                state,
                vec![key_event(eframe::egui::Key::Escape, eframe::egui::Modifiers::NONE)],
            );
            assert!(!state.palette_open(), "Escape closes");
            assert_eq!(state.tabs().len(), 2, "Escape opened nothing");

            frame(
                state,
                vec![key_event(eframe::egui::Key::K, eframe::egui::Modifiers::COMMAND)],
            );
            frame(state, vec![eframe::egui::Event::Text("e".to_owned())]);
            frame(
                state,
                vec![key_event(eframe::egui::Key::ArrowUp, eframe::egui::Modifiers::NONE)],
            );
            assert_eq!(state.palette().expect("palette").selected, 3, "up wraps");
            frame(
                state,
                vec![key_event(eframe::egui::Key::Enter, eframe::egui::Modifiers::NONE)],
            );
            assert!(!state.palette_open());
            // The sidebar consumes the reveal on the same frame it draws, so
            // a closed palette with no new tab is the trace the column row
            // leaves: Enter activated it instead of opening a tab.
            assert_eq!(state.tabs().len(), 2, "the column row opened no tab");
        });
    }

    /// One parent with an integer key and one child with a single-column key
    /// to it, for foreign key jump cases.
    fn fk_schema() -> SchemaInfo {
        let parent = TableInfo::new(
            "parent".to_owned(),
            TableKind::Table,
            vec![
                ColumnInfo::new("id".to_owned(), "INTEGER".to_owned(), false, Some(1)).unwrap(),
                ColumnInfo::new("name".to_owned(), "TEXT".to_owned(), true, None).unwrap(),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            false,
        )
        .unwrap();
        let child = TableInfo::new(
            "child".to_owned(),
            TableKind::Table,
            vec![
                ColumnInfo::new("id".to_owned(), "INTEGER".to_owned(), false, Some(1)).unwrap(),
                ColumnInfo::new("parent_id".to_owned(), "INTEGER".to_owned(), true, None)
                    .unwrap(),
            ],
            Vec::new(),
            vec![
                crate::db::ForeignKeyInfo::new(
                    vec!["parent_id".to_owned()],
                    "parent".to_owned(),
                    vec!["id".to_owned()],
                    None,
                )
                .unwrap(),
            ],
            Vec::new(),
            Vec::new(),
            false,
            false,
        )
        .unwrap();
        let node = SchemaNode::new(
            "main".to_owned(),
            vec![parent, child],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            false,
        )
        .unwrap();
        let database = DatabaseNode::new(":memory:".to_owned(), vec![node]).unwrap();
        SchemaInfo::new(vec![database], false, false)
    }

    /// The child rows behind [`fk_schema`]: two links into the parent.
    fn fk_child_result() -> QueryResult {
        QueryResult::from_columns(
            vec![
                crate::db::Column::new(
                    "id".to_owned(),
                    "INTEGER".to_owned(),
                    vec![crate::db::Value::Integer(10), crate::db::Value::Integer(20)],
                ),
                crate::db::Column::new(
                    "parent_id".to_owned(),
                    "INTEGER".to_owned(),
                    vec![crate::db::Value::Integer(1), crate::db::Value::Integer(2)],
                ),
            ],
            false,
            None,
            std::time::Duration::from_millis(1),
        )
        .unwrap()
    }

    #[test]
    fn fk_jump_builds_a_filtered_select_for_a_single_column_key() {
        let schema = fk_schema();
        assert_eq!(
            fk_jump(&schema, "main", "child", "parent_id", "1", false),
            Some(FkJump {
                schema: "main".to_owned(),
                table: "parent".to_owned(),
                sql: "SELECT * FROM \"main\".\"parent\" WHERE \"id\" = 1 LIMIT 100".to_owned(),
            })
        );
        assert_eq!(
            fk_reveal_target(
                &schema,
                "main",
                "child",
                &crate::db::ForeignKeyInfo::new(
                    vec!["parent_id".to_owned()],
                    "parent".to_owned(),
                    vec!["id".to_owned()],
                    None,
                )
                .unwrap(),
            ),
            Some(("main".to_owned(), "parent".to_owned()))
        );
    }

    #[test]
    fn fk_jump_quotes_a_text_key_that_prints_as_a_number() {
        let parent = TableInfo::new(
            "parent".to_owned(),
            TableKind::Table,
            vec![
                ColumnInfo::new("code".to_owned(), "TEXT".to_owned(), false, Some(1)).unwrap(),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            false,
        )
        .unwrap();
        let child = TableInfo::new(
            "child".to_owned(),
            TableKind::Table,
            vec![
                ColumnInfo::new("id".to_owned(), "INTEGER".to_owned(), false, Some(1)).unwrap(),
                ColumnInfo::new("code".to_owned(), "TEXT".to_owned(), true, None).unwrap(),
            ],
            Vec::new(),
            vec![
                crate::db::ForeignKeyInfo::new(
                    vec!["code".to_owned()],
                    "parent".to_owned(),
                    vec!["code".to_owned()],
                    None,
                )
                .unwrap(),
            ],
            Vec::new(),
            Vec::new(),
            false,
            false,
        )
        .unwrap();
        let node = SchemaNode::new(
            "main".to_owned(),
            vec![parent, child],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            false,
        )
        .unwrap();
        let schema =
            SchemaInfo::new(vec![DatabaseNode::new(":memory:".to_owned(), vec![node]).unwrap()], false, false);
        assert_eq!(
            fk_jump(&schema, "main", "child", "code", "010", false),
            Some(FkJump {
                schema: "main".to_owned(),
                table: "parent".to_owned(),
                sql: "SELECT * FROM \"main\".\"parent\" WHERE \"code\" = '010' LIMIT 100"
                    .to_owned(),
            })
        );
    }

    #[test]
    fn fk_jump_stays_for_null_multi_column_and_missing_targets() {
        let schema = fk_schema();
        assert_eq!(fk_jump(&schema, "main", "child", "parent_id", "1", true), None);
        assert_eq!(fk_jump(&schema, "main", "child", "id", "10", false), None);
        assert_eq!(fk_jump(&schema, "main", "child", "PARENT_ID", "2", false).map(|j| j.sql),
            Some("SELECT * FROM \"main\".\"parent\" WHERE \"id\" = 2 LIMIT 100".to_owned()));
        assert_eq!(fk_jump(&schema, "main", "child", "nope", "1", false), None);
        assert_eq!(fk_jump(&schema, "main", "gone", "parent_id", "1", false), None);
        assert_eq!(fk_jump(&schema, "main", "child", "parent_id", "", false), None);
        let multi = crate::db::ForeignKeyInfo::new(
            vec!["a".to_owned(), "b".to_owned()],
            "parent".to_owned(),
            vec!["a".to_owned(), "b".to_owned()],
            None,
        )
        .unwrap();
        assert_eq!(fk_reveal_target(&schema, "main", "child", &multi), None);
        let missing = crate::db::ForeignKeyInfo::new(
            vec!["parent_id".to_owned()],
            "gone".to_owned(),
            vec!["id".to_owned()],
            None,
        )
        .unwrap();
        assert_eq!(fk_reveal_target(&schema, "main", "child", &missing), None);
        let bad_col = crate::db::ForeignKeyInfo::new(
            vec!["parent_id".to_owned()],
            "parent".to_owned(),
            vec!["gone".to_owned()],
            None,
        )
        .unwrap();
        assert_eq!(fk_reveal_target(&schema, "main", "child", &bad_col), None);
    }

    #[test]
    fn follow_foreign_key_opens_the_filtered_parent_and_reveals_it() {
        with_isolated_state("fk-follow", |state| {
            let conn = add_sqlite_connection(state, "mem-fk");
            let commands = attach_test_session(state, conn);
            state.apply(DbEvent::SchemaOk { conn, schema: fk_schema() });
            let tab = state.new_tab(conn);
            let sql = "SELECT * FROM \"main\".\"child\"";
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
            state.apply(DbEvent::BatchOk { run, results: vec![fk_child_result()] });
            assert!(state.follow_foreign_key(tab, 0, 1), "the single key jumps");
            assert_eq!(
                state.navigator_reveal(),
                Some(&NavigatorReveal {
                    conn,
                    schema: "main".to_owned(),
                    table: "parent".to_owned(),
                })
            );
            let opened = state.active().and_then(|id| state.tab(id)).expect("jumped tab");
            assert_eq!(
                opened.sql,
                "SELECT * FROM \"main\".\"parent\" WHERE \"id\" = 1 LIMIT 100"
            );
            match commands.try_recv().expect("the filtered SELECT sends") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(
                        statements,
                        vec!["SELECT * FROM \"main\".\"parent\" WHERE \"id\" = 1 LIMIT 100"]
                    );
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
            let tabs = state.tabs().len();
            assert!(!state.follow_foreign_key(tab, 0, 0), "a plain column stays");
            assert!(!state.follow_foreign_key(tab, 9, 1), "a missing row stays");
            assert_eq!(state.tabs().len(), tabs, "a stay opens no tab");
            assert!(commands.try_recv().is_err(), "a stay sends nothing");
        });
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_fk_jump_from_a_scratch_child_row_to_its_parent() {
        with_isolated_state("pg-fk-jump-live", |state| {
            let conn = add_postgres_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live Postgres"
            );
            let tab = state.new_tab(conn);
            live_run(state, tab, "DROP TABLE IF EXISTS cde_fk_child");
            live_run(state, tab, "DROP TABLE IF EXISTS cde_fk_parent");
            live_run(
                state,
                tab,
                "CREATE TABLE cde_fk_parent (id INT PRIMARY KEY, name TEXT NOT NULL)",
            );
            live_run(
                state,
                tab,
                "CREATE TABLE cde_fk_child (id INT PRIMARY KEY, parent_id INT REFERENCES cde_fk_parent(id))",
            );
            live_run(
                state,
                tab,
                "INSERT INTO cde_fk_parent (id, name) VALUES (1, 'ada'), (2, 'grace')",
            );
            live_run(state, tab, "INSERT INTO cde_fk_child (id, parent_id) VALUES (10, 1)");
            state.refresh_schema(conn);
            assert!(
                pump_until(state, |s| {
                    s.schema(conn).is_some_and(|schema| {
                        schema.databases().iter().any(|db| {
                            db.schemas().iter().any(|node| {
                                node.tables().iter().any(|t| {
                                    t.name() == "cde_fk_child" && !t.foreign_keys().is_empty()
                                })
                            })
                        })
                    })
                }),
                "the schema loads the scratch foreign key"
            );
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT * FROM cde_fk_child".to_owned();
            }
            live_run(state, tab, "SELECT * FROM cde_fk_child");
            let result =
                state.tab(tab).expect("tab").active_query_result().expect("child rows");
            let col = result
                .columns()
                .iter()
                .position(|c| c.name().eq_ignore_ascii_case("parent_id"))
                .expect("the child names its link column");
            assert!(state.follow_foreign_key(tab, 0, col), "the child row jumps");
            let jumped = state.active().expect("the jump opens a tab");
            assert_ne!(jumped, tab, "the parent opens beside the child");
            assert!(
                pump_until(state, |s| {
                    !matches!(
                        s.tab(jumped).map(|t| t.outcome()),
                        Some(RunOutcome::Running { .. })
                    )
                }),
                "the filtered parent SELECT lands"
            );
            let result =
                state.tab(jumped).expect("tab").active_query_result().expect("parent rows");
            assert_eq!(result.row_count(), 1, "only the linked parent survives");
            let name_col = result
                .columns()
                .iter()
                .position(|c| c.name().eq_ignore_ascii_case("name"))
                .expect("the parent names its text column");
            assert_eq!(
                result.cell(0, name_col),
                Some(&Value::Text("ada".to_owned())),
                "the jump lands on ada"
            );
            live_run(state, tab, "DROP TABLE cde_fk_child");
            live_run(state, tab, "DROP TABLE cde_fk_parent");
        });
    }

    #[test]
    fn save_prompt_prefills_folder_and_pin_from_the_tabs_entry() {
        with_isolated_state("saved-prefill", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 1".to_owned();
            }
            state.activate(tab);
            let id = state.save_active_tab("daily", Some("ops"), true).expect("saved");
            state.open_save_prompt();
            {
                let prompt = state.save_prompt().expect("prompt");
                assert_eq!(prompt.name, "daily");
                assert_eq!(prompt.folder, "ops");
                assert!(prompt.pinned);
            }
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 2".to_owned();
            }
            state.confirm_save_prompt();
            assert_eq!(state.saved_queries().len(), 1);
            assert_eq!(state.saved_queries()[0].id, id);
            assert_eq!(state.saved_queries()[0].sql, "SELECT 2");
            assert_eq!(state.saved_queries()[0].folder.as_deref(), Some("ops"));
            assert!(state.saved_queries()[0].pinned);

            let fresh = state.new_tab(conn);
            state.activate(fresh);
            state.open_save_prompt();
            {
                let prompt = state.save_prompt().expect("prompt");
                assert_eq!(prompt.name, "");
                assert_eq!(prompt.folder, "");
                assert!(!prompt.pinned);
            }
            state.close_save_prompt();

            let reopened = state.new_tab(conn);
            state.activate(reopened);
            state.open_saved(id);
            state.open_save_prompt();
            let prompt = state.save_prompt().expect("prompt");
            assert_eq!(prompt.name, "daily");
            assert_eq!(prompt.folder, "ops");
            assert!(prompt.pinned);
        });
    }

    #[test]
    fn tabs_roundtrip_saves_conn_and_sql_in_order() {
        with_isolated_state("tabs-roundtrip", |state| {
            let a = add_sqlite_connection(state, "a");
            let b = add_sqlite_connection(state, "b");
            let first = state.new_tab(a);
            if let Some(slot) = state.tab_sql_mut(first) {
                *slot = "SELECT 1".to_owned();
            }
            let second = state.new_tab(b);
            if let Some(slot) = state.tab_sql_mut(second) {
                *slot = "SELECT 2".to_owned();
            }
            state.tab_mut(first).expect("tab").set_sort(0);
            state.tab_mut(first).expect("tab").set_filter("leak".to_owned());
            eframe::App::on_exit(state);
            let raw = std::fs::read_to_string(tabs_path()).expect("tabs file");
            let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(json[0]["sql"], serde_json::json!("SELECT 1"));
            assert_eq!(json[1]["sql"], serde_json::json!("SELECT 2"));
            for row in json.as_array().expect("rows") {
                let keys: Vec<&str> = row
                    .as_object()
                    .expect("row")
                    .keys()
                    .map(String::as_str)
                    .collect();
                assert_eq!(keys.len(), 3);
                assert!(keys.contains(&"conn"));
                assert!(keys.contains(&"sql"));
                assert!(keys.contains(&"kind"));
            }
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.tabs().len(), 2);
            assert_eq!(reloaded.tabs()[0].conn, Some(a));
            assert_eq!(reloaded.tabs()[0].sql, "SELECT 1");
            assert_eq!(reloaded.tabs()[1].conn, Some(b));
            assert_eq!(reloaded.tabs()[1].sql, "SELECT 2");
            assert_ne!(reloaded.tabs()[0].id, reloaded.tabs()[1].id);
            assert_eq!(reloaded.active(), Some(reloaded.tabs()[0].id));
            for tab in reloaded.tabs() {
                assert!(matches!(tab.outcome(), RunOutcome::Never));
                assert!(tab.results().is_empty());
                assert_eq!(tab.sort_col(), None);
                assert_eq!(tab.filter(), "");
            }
        });
    }

    #[test]
    fn tabs_prune_drops_missing_connections_and_keeps_unbound() {
        with_isolated_state("tabs-prune", |state| {
            let live = add_sqlite_connection(state, "live");
            let rows = vec![
                PersistedTab {
                    conn: Some(ConnectionId::from_raw(999_999)),
                    sql: "SELECT gone".to_owned(),
                    kind: TabKind::Query,
                },
                PersistedTab { conn: None, sql: "SELECT free".to_owned(), kind: TabKind::Query },
                PersistedTab {
                    conn: Some(live),
                    sql: "SELECT kept".to_owned(),
                    kind: TabKind::Query,
                },
            ];
            std::fs::write(tabs_path(), serde_json::to_vec(&rows).unwrap()).unwrap();
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.tabs().len(), 2);
            assert_eq!(reloaded.tabs()[0].conn, None);
            assert_eq!(reloaded.tabs()[0].sql, "SELECT free");
            assert_eq!(reloaded.tabs()[1].conn, Some(live));
            assert_eq!(reloaded.tabs()[1].sql, "SELECT kept");
            assert!(matches!(reloaded.tabs()[1].outcome(), RunOutcome::Never));
            assert_eq!(reloaded.active(), Some(reloaded.tabs()[0].id));
        });
    }

    #[test]
    fn open_table_view_creates_a_running_table_tab() {
        with_isolated_state("table-view-run", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let commands = attach_test_session(state, conn);
            let tab = state.open_table_view(conn, "main", "users").expect("table view");
            let view = state.tab(tab).expect("tab");
            assert_eq!(
                view.kind,
                TabKind::Table { schema: "main".to_owned(), table: "users".to_owned() }
            );
            assert_eq!(view.sql, "SELECT * FROM \"main\".\"users\" LIMIT 100");
            assert!(matches!(view.outcome(), RunOutcome::Running { .. }));
            assert_eq!(state.active(), Some(tab));
            match commands.try_recv().expect("the table run was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(
                        statements,
                        vec!["SELECT * FROM \"main\".\"users\" LIMIT 100"]
                    );
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn opening_the_same_table_view_twice_reuses_its_tab() {
        with_isolated_state("table-view-twice", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let commands = attach_test_session(state, conn);
            let first = state.open_table_view(conn, "main", "users");
            let _ = commands.try_recv().expect("the first run was sent");
            let second = state.open_table_view(conn, "main", "users");
            assert_eq!(first, second, "the existing tab is activated");
            assert_eq!(state.tabs().len(), 1);
            assert!(
                commands.try_recv().is_err(),
                "reactivating a table tab sends no second run"
            );
        });
    }

    #[test]
    fn edit_as_query_flips_the_kind_and_keeps_the_sql() {
        with_isolated_state("table-view-edit", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let _commands = attach_test_session(state, conn);
            let tab = state.open_table_view(conn, "main", "users").expect("table view");
            state.edit_as_query(tab);
            let view = state.tab(tab).expect("tab");
            assert_eq!(view.kind, TabKind::Query);
            assert_eq!(view.sql, "SELECT * FROM \"main\".\"users\" LIMIT 100");
        });
    }

    #[test]
    fn table_header_paints_the_name_and_reruns_on_ctrl_enter() {
        with_isolated_state("table-view-ctrl-enter", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let commands = attach_test_session(state, conn);
            let tab = state.open_table_view(conn, "main", "users").expect("table view");
            match commands.try_recv().expect("the first run") {
                DbCommand::QueryBatch { run, .. } => {
                    state.apply(DbEvent::BatchOk { run, results: vec![rows_result(1)] });
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(900.0, 200.0),
                )),
                events: vec![key_event(
                    eframe::egui::Key::Enter,
                    eframe::egui::Modifiers::COMMAND,
                )],
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                crate::ui::query::table_header(state, ui);
            });
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "main.users"), "{text:?}");
            assert!(text.iter().any(|t| t == "Refresh"), "{text:?}");
            assert!(text.iter().any(|t| t == "SQL"), "{text:?}");
            match commands.try_recv().expect("Ctrl+Enter re-runs") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["SELECT * FROM \"main\".\"users\" LIMIT 100"]);
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
    fn table_tab_kind_round_trips_through_save_and_load() {
        with_isolated_state("table-view-roundtrip", |state| {
            let conn = add_sqlite_connection(state, "live");
            let _commands = attach_test_session(state, conn);
            let _tab = state.open_table_view(conn, "main", "users");
            save_tabs(state.tabs()).unwrap();
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.tabs().len(), 1);
            assert_eq!(
                reloaded.tabs()[0].kind,
                TabKind::Table { schema: "main".to_owned(), table: "users".to_owned() }
            );
            assert_eq!(
                reloaded.tabs()[0].sql,
                "SELECT * FROM \"main\".\"users\" LIMIT 100"
            );
        });
    }

    #[test]
    fn a_json_tab_row_without_kind_loads_as_query() {
        with_isolated_state("table-view-old-row", |state| {
            let live = add_sqlite_connection(state, "live");
            let rows = serde_json::json!([{"conn": live.raw(), "sql": "SELECT 1"}]);
            std::fs::write(tabs_path(), serde_json::to_vec(&rows).unwrap()).unwrap();
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.tabs().len(), 1);
            assert_eq!(reloaded.tabs()[0].kind, TabKind::Query);
            assert_eq!(reloaded.tabs()[0].sql, "SELECT 1");
        });
    }

    #[test]
    fn tabs_empty_state_matches_fresh_start() {
        with_isolated_state("tabs-empty", |state| {
            assert!(!tabs_path().exists());
            assert!(state.tabs().is_empty());
            assert_eq!(state.active(), None);
            std::fs::write(tabs_path(), b"").unwrap();
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert!(reloaded.tabs().is_empty());
            assert_eq!(reloaded.active(), None);
        });
    }

    #[test]
    fn tabs_corrupt_file_loads_what_it_can() {
        with_isolated_state("tabs-corrupt", |state| {
            let live = add_sqlite_connection(state, "live");
            std::fs::write(tabs_path(), b"{oops").unwrap();
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert!(reloaded.tabs().is_empty());
            assert_eq!(reloaded.active(), None);
            let mixed = serde_json::json!([
                {"conn": live.raw(), "sql": "SELECT kept"},
                {"conn": "oops", "sql": "SELECT bad"},
                {"sql": "SELECT unbound"},
                {},
            ]);
            std::fs::write(tabs_path(), serde_json::to_vec(&mixed).unwrap()).unwrap();
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.tabs().len(), 3);
            assert_eq!(reloaded.tabs()[0].conn, Some(live));
            assert_eq!(reloaded.tabs()[0].sql, "SELECT kept");
            assert_eq!(reloaded.tabs()[1].conn, None);
            assert_eq!(reloaded.tabs()[1].sql, "SELECT unbound");
            assert_eq!(reloaded.tabs()[2].conn, None);
            assert_eq!(reloaded.tabs()[2].sql, "");
            assert_eq!(reloaded.active(), Some(reloaded.tabs()[0].id));
        });
    }

    #[test]
    fn persisted_profile_shape_has_no_password() {
        let profile = PersistedProfile {
            id: 1,
            name: "pg".to_owned(),
            env: Environment::Prod,
            folder: Some("ops/db".to_owned()),
            config: ConnectionConfig::Postgres { read_only: false,
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
                ssh: None,
            },
        };
        let json = serde_json::to_string(&profile).unwrap();
        assert!(!json.contains("password"));
        assert!(json.contains("\"kind\":\"postgres\""));
        assert!(json.contains("ops/db"));
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
            folder: Some("ops/db".to_owned()),
            config: ConnectionConfig::Postgres { read_only: false,
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
                ssh: None,
            },
            password: "transient-only".to_owned(),
            ssh_password: "ssh-transient-only".to_owned(),
            state: ConnectionState::Disconnected,
            transaction: Transaction::Closed,
            autocommit: true,
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
        assert!(!raw.contains("ssh-transient-only"));
        assert!(!raw.contains("ssh_password"));
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].password.is_empty());
        assert!(loaded[0].ssh_password.is_empty());
        assert_eq!(loaded[0].name, "pg");
        assert_eq!(loaded[0].env, Environment::Test);
    }

    #[test]
    fn ssh_password_rides_the_draft_without_persisting() {
        with_isolated_state("ssh-pw", |state| {
            state.open_draft(None);
            {
                let draft = state.draft_mut().expect("draft");
                draft.name = "pg".to_owned();
                draft.config = ConnectionConfig::Postgres { read_only: false,
                    host: "localhost".to_owned(),
                    port: 5432,
                    database: "appdb".to_owned(),
                    user: "owner".to_owned(),
                    ssl_mode: PostgresSslMode::Disable,
                    ssh: None,
                };
                draft.password = "db-secret".to_owned();
                draft.ssh_password = "ssh-secret".to_owned();
            }
            state.save_draft();
            let id = state.connections().last().expect("saved").id;
            assert_eq!(state.connections().last().expect("saved").ssh_password, "ssh-secret");
            let raw = std::fs::read_to_string(profiles_path()).expect("profiles file");
            assert!(!raw.contains("ssh-secret"));
            assert!(!raw.contains("db-secret"));

            // Both secrets are gone after a reload.
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert!(reloaded.connections()[0].ssh_password.is_empty());
            assert!(reloaded.connections()[0].password.is_empty());

            // set_ssh_password mirrors set_password; an edit draft starts
            // with the ssh secret empty while the db password is copied.
            state.set_ssh_password(id, "again".to_owned());
            assert_eq!(state.connections()[0].ssh_password, "again");
            state.open_draft(Some(id));
            let draft = state.draft().expect("edit draft");
            assert_eq!(draft.ssh_password, "");
            assert_eq!(draft.password, "db-secret");
        });
    }

    #[test]
    fn draft_window_offers_the_ssh_section_for_engines_only() {
        with_isolated_state("draft-ssh", |state| {
            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let frame = |state: &mut AppState| {
                let mut output = ctx.run_ui(input.clone(), |ui| {
                    crate::ui::connections::draft_window(state, ui, None);
                });
                output.textures_delta.clear();
                output
            };

            state.open_draft(None);
            frame(state);
            let sqlite_text = rendered_text(&frame(state));
            assert!(!sqlite_text.iter().any(|t| t == "Use SSH tunnel"), "{sqlite_text:?}");

            if let Some(draft) = state.draft_mut() {
                draft.config = crate::db::DbKind::Postgres.default_config();
            }
            frame(state);
            let postgres = frame(state);
            let text = rendered_text(&postgres);
            assert!(text.iter().any(|t| t == "Use SSH tunnel"), "{text:?}");
            assert!(!text.iter().any(|t| t == "SSH host:"), "{text:?}");

            let pos = text_center(&postgres, "Use SSH tunnel").expect("checkbox label paints");
            let click = |pos: eframe::egui::Pos2, pressed: bool| eframe::egui::Event::PointerButton {
                pos,
                button: eframe::egui::PointerButton::Primary,
                pressed,
                modifiers: eframe::egui::Modifiers::NONE,
            };
            let mut events = input.clone();
            events.events = vec![
                eframe::egui::Event::PointerMoved(pos),
                click(pos, true),
                click(pos, false),
            ];
            let mut clicked = ctx.run_ui(events, |ui| {
                crate::ui::connections::draft_window(state, ui, None);
            });
            clicked.textures_delta.clear();
            let after = frame(state);
            let text = rendered_text(&after);
            assert!(text.iter().any(|t| t == "SSH host:"), "{text:?}");
            assert!(text.iter().any(|t| t == "SSH password:"), "{text:?}");
            let draft = state.draft().expect("draft");
            assert!(matches!(
                &draft.config,
                ConnectionConfig::Postgres { ssh: Some(_), .. }
            ));

            // MySQL gets the same section; SQLite never does.
            if let Some(draft) = state.draft_mut() {
                draft.config = crate::db::DbKind::Mysql.default_config();
            }
            frame(state);
            let mysql_text = rendered_text(&frame(state));
            assert!(mysql_text.iter().any(|t| t == "Use SSH tunnel"), "{mysql_text:?}");
        });
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
            draft.config = ConnectionConfig::Postgres { read_only: false,
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
                ssh: None,
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
                ConnectionConfig::Postgres { read_only: false,
                    host: "localhost".to_owned(),
                    port: 5432,
                    database: "appdb".to_owned(),
                    user: "owner".to_owned(),
                    ssl_mode: PostgresSslMode::Disable,
                    ssh: None,
                }
            );
            assert_eq!(draft.password, "first");
        }
        {
            let draft = state.draft_mut().expect("edit draft mut");
            draft.name = "pg-renamed".to_owned();
            draft.config = ConnectionConfig::Postgres { read_only: false,
                host: "db.internal".to_owned(),
                port: 5433,
                database: "analytics".to_owned(),
                user: "reader".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
                ssh: None,
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
            ConnectionConfig::Postgres { read_only: false,
                host: "db.internal".to_owned(),
                port: 5433,
                database: "analytics".to_owned(),
                user: "reader".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
                ssh: None,
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
    fn prompt_opens_for_mysql_with_empty_password() {
        with_isolated_state("pw-mysql", |state| {
            let stored = add_mysql_connection(state, "my-stored", "cdepass");
            state.open_password_prompt(stored);
            assert_eq!(state.password_prompt(), None);
            assert!(state.prompt_password().is_empty());
            let conn =
                state.connections().iter().find(|c| c.id == stored).expect("mysql");
            assert!(matches!(conn.state(), ConnectionState::Connecting { .. }));

            let empty = add_mysql_connection(state, "my-empty", "");
            state.open_password_prompt(empty);
            assert_eq!(state.password_prompt(), Some(empty));
            assert_eq!(state.prompt_password(), "");
            let conn = state.connections().iter().find(|c| c.id == empty).expect("mysql");
            assert!(matches!(conn.state(), ConnectionState::Disconnected));
        });
    }

    #[test]
    fn mysql_profile_roundtrips_through_save_and_load() {
        with_isolated_state("mysql-persist", |state| {
            let id = add_mysql_connection(state, "mysql", "cdepass");
            let raw = std::fs::read_to_string(profiles_path()).expect("profiles file");
            let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(json[0]["config"]["kind"], serde_json::json!("mysql"));
            assert_eq!(json[0]["config"]["port"], serde_json::json!(3307));
            assert!(!raw.contains("cdepass"));
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.connections().len(), 1);
            let conn = &reloaded.connections()[0];
            assert_eq!(conn.id, id);
            assert_eq!(conn.name, "mysql");
            assert_eq!(
                conn.config,
                ConnectionConfig::Mysql { read_only: false,
                    host: "localhost".to_owned(),
                    port: 3307,
                    database: "cde_test".to_owned(),
                    user: "root".to_owned(),
                    ssl_mode: MysqlSslMode::default(),
                    ssh: None,
                }
            );
            assert!(conn.password.is_empty());
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
        assert_ne!(
            Environment::Dev.badge_color(crate::ui::Theme::Dark),
            Environment::Test.badge_color(crate::ui::Theme::Dark)
        );
        assert_ne!(
            Environment::Test.badge_color(crate::ui::Theme::Dark),
            Environment::Prod.badge_color(crate::ui::Theme::Dark)
        );
        assert_ne!(
            Environment::Dev.badge_color(crate::ui::Theme::Dark),
            Environment::Prod.badge_color(crate::ui::Theme::Dark)
        );
        assert_eq!(
            Environment::Dev.badge_color(crate::ui::Theme::Dark),
            eframe::egui::Color32::from_rgb(148, 155, 166)
        );
        assert_eq!(
            Environment::Dev.badge_color(crate::ui::Theme::Light),
            eframe::egui::Color32::from_rgb(95, 105, 115)
        );
    }

    #[test]
    fn theme_toggle_flips_and_reapplies() {
        with_isolated_state("theme-toggle", |state| {
            assert_eq!(state.theme(), crate::ui::Theme::Dark);
            state.toggle_theme();
            assert_eq!(state.theme(), crate::ui::Theme::Light);
            assert_eq!(state.ui_ctx.theme(), eframe::egui::Theme::Light);
            state.toggle_theme();
            assert_eq!(state.theme(), crate::ui::Theme::Dark);
            assert_eq!(state.ui_ctx.theme(), eframe::egui::Theme::Dark);
        });
    }

    #[test]
    fn theme_roundtrips_through_save_and_load() {
        with_isolated_state("theme-round", |state| {
            state.set_theme(crate::ui::Theme::Light);
            let raw = std::fs::read_to_string(settings_path()).expect("settings file");
            let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(json["theme"], serde_json::json!("light"));
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.theme(), crate::ui::Theme::Light);
        });
    }

    #[test]
    fn missing_settings_file_starts_dark() {
        with_isolated_state("theme-missing", |state| {
            let _ = std::fs::remove_file(settings_path());
            let fresh = AppState::new(&eframe::egui::Context::default());
            assert_eq!(fresh.theme(), crate::ui::Theme::Dark);
            assert_eq!(state.theme(), crate::ui::Theme::Dark);
        });
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
    fn join_target_maps_each_column_to_its_source_table() {
        with_isolated_state("edit-join-target", |state| {
            let (tab, _conn, _commands) = join_tab(state);
            let target = state.edit_target(tab).expect("join target");
            assert!(target.is_join());
            assert_eq!(target.table, "users, orders");
            assert_eq!(target.join_tables.len(), 2);
            assert_eq!(target.join_tables[0].table, "users");
            assert_eq!(target.join_tables[0].pk_cols, vec![("id".to_owned(), 0)]);
            assert_eq!(target.join_tables[1].table, "orders");
            assert_eq!(target.join_tables[1].pk_cols, vec![("id".to_owned(), 2)]);
            assert_eq!(
                target.join_columns,
                vec![
                    Some(JoinColumnTarget { table_index: 0, column: "id".to_owned() }),
                    Some(JoinColumnTarget { table_index: 0, column: "name".to_owned() }),
                    Some(JoinColumnTarget { table_index: 1, column: "id".to_owned() }),
                    Some(JoinColumnTarget { table_index: 1, column: "total".to_owned() }),
                ]
            );
            for col in 0..4 {
                assert!(target.is_column_editable(col), "col {col} edits");
            }

            set_tab_sql(
                state,
                tab,
                "SELECT users.id, users.name, orders.id AS oid, orders.total \
                 FROM users JOIN orders ON orders.user_id = users.id",
            );
            let named = state.edit_target(tab).expect("table names resolve");
            assert!(named.is_join());
            assert_eq!(named.join_columns.len(), 4);
            assert!(named.join_columns.iter().all(|slot| slot.is_some()));

            set_tab_sql(
                state,
                tab,
                "SELECT u.id, u.name, o.id AS oid, o.total FROM users AS u JOIN orders AS o \
                 ON o.user_id = u.id WHERE u.id > 0 ORDER BY u.id LIMIT 10",
            );
            assert!(state.edit_target(tab).expect("clauses resolve").is_join());
        });
    }

    #[test]
    fn join_staging_routes_each_cell_to_its_source_table() {
        with_isolated_state("edit-join-apply", |state| {
            let (tab, _conn, commands) = join_tab(state);
            state.stage_edit(tab, 0, 1, "Ada".to_owned());
            state.stage_edit(tab, 1, 3, "999".to_owned());
            assert_eq!(
                state.preview_edit_statements(tab),
                Some(vec![
                    "UPDATE \"users\" SET \"name\" = 'Ada' WHERE \"id\" = 1".to_owned(),
                    "UPDATE \"orders\" SET \"total\" = 999 WHERE \"id\" = 20".to_owned(),
                ])
            );
            state.apply_edits(tab);
            assert_eq!(state.pending_edit_preview(), Some(tab));
            state.confirm_pending_edits();
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => assert_eq!(
                    statements,
                    vec![
                        "UPDATE \"users\" SET \"name\" = 'Ada' WHERE \"id\" = 1",
                        "UPDATE \"orders\" SET \"total\" = 999 WHERE \"id\" = 20",
                    ]
                ),
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn join_unresolvable_ambiguous_and_keyless_columns_stay_read_only() {
        with_isolated_state("edit-join-readonly", |state| {
            let (tab, _conn, _commands) = join_tab(state);
            set_tab_sql(
                state,
                tab,
                "SELECT u.id, u.name, o.id AS oid, count(*) \
                 FROM users u JOIN orders o ON o.user_id = u.id",
            );
            let target = state.edit_target(tab).expect("partial target");
            assert!(target.is_join());
            assert_eq!(target.join_columns.len(), 4);
            assert!(target.join_columns[0].is_some());
            assert!(target.join_columns[1].is_some());
            assert!(target.join_columns[2].is_some());
            assert_eq!(target.join_columns[3], None);
            assert!(!target.is_column_editable(3));
            state.stage_edit(tab, 0, 3, "x".to_owned());
            assert!(state.tab(tab).expect("tab").edits().is_empty());
            state.stage_edit(tab, 0, 1, "Ada".to_owned());
            assert_eq!(state.tab(tab).expect("tab").edits().len(), 1);
            assert_eq!(
                state.preview_edit_statements(tab),
                Some(vec!["UPDATE \"users\" SET \"name\" = 'Ada' WHERE \"id\" = 1".to_owned()])
            );

            set_tab_sql(state, tab, "SELECT id, name, oid, total FROM users u JOIN orders o ON o.user_id = u.id");
            assert_eq!(state.edit_target(tab), None);

            set_tab_sql(state, tab, "SELECT a.id, a.name, a.oid, a.total FROM users a JOIN orders a ON a.user_id = a.id");
            assert_eq!(state.edit_target(tab), None);

            set_tab_sql(state, tab, "SELECT u.name, u.name, o.total, o.total FROM users u JOIN orders o ON o.user_id = u.id");
            assert_eq!(state.edit_target(tab), None);
        });
    }

    #[test]
    fn join_deletes_and_inserts_stay_disabled() {
        with_isolated_state("edit-join-no-delete", |state| {
            let (tab, _conn, _commands) = join_tab(state);
            state.mark_row_deleted(tab, 0);
            assert!(!state.tab(tab).expect("tab").is_row_deleted(0));
            state.add_row(tab);
            assert!(state.tab(tab).expect("tab").added_rows().is_empty());
            state.stage_edit(tab, 0, 1, "Ada".to_owned());
            state.apply_edits(tab);
            assert_eq!(state.pending_edit_preview(), Some(tab));
            assert_eq!(
                state.preview_edit_statements(tab),
                Some(vec!["UPDATE \"users\" SET \"name\" = 'Ada' WHERE \"id\" = 1".to_owned()])
            );
        });
    }

    #[test]
    fn single_table_alias_columns_edit_the_source_and_expressions_stay_read_only() {
        with_isolated_state("edit-alias", |state| {
            let (tab, _conn, commands) = editable_tab(state);
            set_tab_sql(state, tab, "SELECT tenant, id, name AS label, upper(name) FROM items");
            set_tab_result(state, tab, aliased_items_result());
            let target = state.edit_target(tab).expect("target");
            assert_eq!(target.table, "items");
            assert_eq!(
                target.columns,
                vec![
                    Some("tenant".to_owned()),
                    Some("id".to_owned()),
                    Some("name".to_owned()),
                    None,
                ]
            );
            assert_eq!(
                target.pk_cols,
                vec![("tenant".to_owned(), 0), ("id".to_owned(), 1)]
            );
            assert!(target.is_column_editable(2));
            assert!(!target.is_column_editable(3));
            state.stage_edit(tab, 0, 3, "ADA".to_owned());
            assert!(state.tab(tab).expect("tab").edits().is_empty());
            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            assert_eq!(
                state.preview_edit_statements(tab),
                Some(vec![
                    "UPDATE \"items\" SET \"name\" = 'Ada' \
                     WHERE \"tenant\" = 'acme' AND \"id\" = 1"
                        .to_owned()
                ])
            );
            assert!(commands.try_recv().is_err());

            // The primary key resolves through its alias too.
            set_tab_sql(state, tab, "SELECT tenant, id AS item_id, name AS label FROM items");
            set_tab_result(state, tab, renamed_items_result());
            let target = state.edit_target(tab).expect("aliased keys");
            assert_eq!(
                target.pk_cols,
                vec![("tenant".to_owned(), 0), ("id".to_owned(), 1)]
            );
            assert_eq!(
                target.columns,
                vec![
                    Some("tenant".to_owned()),
                    Some("id".to_owned()),
                    Some("name".to_owned()),
                ]
            );
        });
    }

    #[test]
    fn a_staged_text_column_quotes_numbers_as_text() {
        with_isolated_state("edit-typed", |state| {
            let (tab, _conn, _commands) = editable_tab(state);
            state.stage_edit(tab, 0, 1, "42".to_owned());
            state.stage_edit(tab, 0, 2, "42".to_owned());
            state.add_row(tab);
            state.stage_edit(tab, 2, 0, "42".to_owned());
            state.stage_edit(tab, 2, 1, "42".to_owned());
            state.stage_edit(tab, 2, 2, "42".to_owned());
            assert_eq!(
                state.preview_edit_statements(tab),
                Some(vec![
                    "UPDATE \"items\" SET \"id\" = 42, \"name\" = '42' \
                     WHERE \"tenant\" = 'acme' AND \"id\" = 1"
                        .to_owned(),
                    "INSERT INTO \"items\" (\"tenant\", \"id\", \"name\") \
                     VALUES ('42', 42, '42')"
                        .to_owned(),
                ])
            );
        });
    }

    #[test]
    fn a_null_key_cell_refuses_to_build_a_where_clause() {
        with_isolated_state("edit-null-key", |state| {
            let (tab, _conn, commands) = join_tab(state);
            set_tab_sql(
                state,
                tab,
                "SELECT u.id, u.name, o.id AS oid, o.total FROM users u \
                 LEFT JOIN orders o ON o.user_id = u.id",
            );
            set_tab_result(state, tab, null_join_result());
            let target = state.edit_target(tab).expect("join target");
            assert_eq!(target.join_tables[1].pk_cols, vec![("id".to_owned(), 2)]);
            state.stage_edit(tab, 1, 3, "5".to_owned());
            assert_eq!(state.tab(tab).expect("tab").edits().len(), 1);
            assert_eq!(state.preview_edit_statements(tab), None);
            state.apply_edits(tab);
            assert_eq!(state.pending_edit_preview(), None);
            assert!(commands.try_recv().is_err());
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
            let expected = vec![
                "UPDATE \"items\" SET \"name\" = 'ada lovelace' WHERE \"tenant\" = 'acme' AND \"id\" = 1".to_owned(),
                "DELETE FROM \"items\" WHERE \"tenant\" = 'globex' AND \"id\" = 2".to_owned(),
                "INSERT INTO \"items\" (\"tenant\", \"id\", \"name\") VALUES ('initech', 3, 'Lin')".to_owned(),
            ];
            state.apply_edits(tab);
            assert_eq!(state.pending_edit_preview(), Some(tab));
            assert_eq!(state.preview_edit_statements(tab), Some(expected.clone()));
            assert!(commands.try_recv().is_err(), "the preview holds the batch");
            state.confirm_pending_edits();
            assert_eq!(state.pending_edit_preview(), None);
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, expected);
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
    fn apply_cancel_keeps_the_staged_edits() {
        with_isolated_state("edit-preview-cancel", |state| {
            let (tab, _conn, commands) = editable_tab(state);
            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            state.mark_row_deleted(tab, 1);
            assert_eq!(state.tab(tab).expect("tab").staged_count(), 2);
            state.apply_edits(tab);
            assert_eq!(state.pending_edit_preview(), Some(tab));
            assert!(commands.try_recv().is_err(), "the preview holds the batch");
            state.cancel_pending_edits();
            assert_eq!(state.pending_edit_preview(), None);
            assert!(commands.try_recv().is_err(), "cancel sends nothing");
            let view = state.tab(tab).expect("tab");
            assert_eq!(view.staged_count(), 2);
            assert_eq!(view.edit_text(0, 2), Some("Ada"));
            assert!(view.is_row_deleted(1));
        });
    }

    #[test]
    fn table_create_sends_exact_statements_and_refreshes_schema_on_ok() {
        with_isolated_state("table-create-send", |state| {
            let conn = add_sqlite_connection(state, "mem-table");
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);
            state.open_table_create(conn);
            {
                let editor = state.table_editor_mut().expect("editor open");
                editor.form.table = "users".to_owned();
                editor.form.columns = vec![
                    crate::ui::ddl::TableColumnDraft {
                        name: "id".to_owned(),
                        data_type: "INTEGER".to_owned(),
                        nullable: false,
                        default: String::new(),
                        pk: true,
                    },
                    crate::ui::ddl::TableColumnDraft {
                        name: "name".to_owned(),
                        data_type: "TEXT".to_owned(),
                        nullable: true,
                        default: String::new(),
                        pk: false,
                    },
                ];
                editor.form.indexes = vec![crate::ui::ddl::TableIndexDraft {
                    name: "idx_users_name".to_owned(),
                    columns: "name".to_owned(),
                    unique: false,
                }];
            }
            assert!(state.table_ddl_errors().is_empty());
            assert_eq!(
                state.table_ddl_statements(),
                Some(vec![
                    "CREATE TABLE \"users\" (\n  \"id\" INTEGER NOT NULL,\n  \"name\" TEXT,\n  PRIMARY KEY (\"id\")\n);"
                        .to_owned(),
                    "CREATE INDEX \"idx_users_name\" ON \"users\" (\"name\");".to_owned(),
                ])
            );
            state.apply_table_ddl(tab);
            let run = match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { run, statements } => {
                    assert_eq!(
                        statements,
                        vec![
                            "CREATE TABLE \"users\" (\n  \"id\" INTEGER NOT NULL,\n  \"name\" TEXT,\n  PRIMARY KEY (\"id\")\n);",
                            "CREATE INDEX \"idx_users_name\" ON \"users\" (\"name\");",
                        ]
                    );
                    run
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            state.apply(DbEvent::BatchOk { run, results: vec![affected_result(0)] });
            match commands.try_recv().expect("the schema refresh was sent") {
                DbCommand::LoadSchema => {}
                other => panic!("expected LoadSchema, got {other:?}"),
            }
        });
    }

    #[test]
    fn schema_changing_run_reloads_the_navigator_on_ok() {
        with_isolated_state("schema-change-run", |state| {
            let conn = add_sqlite_connection(state, "mem-schema");
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);

            state.start_run(tab, "CREATE TABLE users (id INTEGER)", 0, None);
            let run = match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { run, statements } => {
                    assert_eq!(statements, vec!["CREATE TABLE users (id INTEGER)"]);
                    run
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            state.apply(DbEvent::BatchOk { run, results: vec![affected_result(0)] });
            match commands.try_recv().expect("the schema reload was sent") {
                DbCommand::LoadSchema => {}
                other => panic!("expected LoadSchema, got {other:?}"),
            }
        });
    }

    #[test]
    fn plain_run_sends_no_schema_reload() {
        with_isolated_state("select-no-reload", |state| {
            let conn = add_sqlite_connection(state, "mem-select");
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);

            state.start_run(tab, "SELECT 1", 0, None);
            let run = match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { run, .. } => run,
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            state.apply(DbEvent::BatchOk { run, results: vec![rows_result(1)] });
            assert!(commands.try_recv().is_err(), "a SELECT sends no schema reload");
        });
    }

    #[test]
    fn errored_schema_changing_run_sends_no_reload() {
        with_isolated_state("schema-change-err", |state| {
            let conn = add_sqlite_connection(state, "mem-schema-err");
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);

            state.start_run(tab, "DROP TABLE users", 0, None);
            let run = match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { run, .. } => run,
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            state.apply(DbEvent::QueryErr {
                run,
                error: crate::db::DbError::Query("no such table".to_owned()),
            });
            assert!(
                commands.try_recv().is_err(),
                "a failed schema-changing run sends no reload"
            );
        });
    }

    #[test]
    fn table_create_read_only_refuses_and_sends_nothing() {
        with_isolated_state("table-create-readonly", |state| {
            let conn = add_sqlite_connection(state, "mem-table-ro");
            let commands = attach_test_session(state, conn);
            state
                .connections
                .iter_mut()
                .find(|c| c.id == conn)
                .expect("connection")
                .config
                .set_read_only(true);
            let tab = state.new_tab(conn);
            state.open_table_create(conn);
            {
                let editor = state.table_editor_mut().expect("editor open");
                editor.form.table = "users".to_owned();
                editor.form.columns = vec![crate::ui::ddl::TableColumnDraft {
                    name: "id".to_owned(),
                    data_type: "INTEGER".to_owned(),
                    nullable: false,
                    default: String::new(),
                    pk: true,
                }];
            }
            state.apply_table_ddl(tab);
            assert!(commands.try_recv().is_err(), "read-only sends nothing");
            match state.tab(tab).expect("tab").outcome() {
                RunOutcome::Err(message) => assert!(message.contains("read-only"), "{message}"),
                other => panic!("expected a read-only error, got {other:?}"),
            }
        });
    }

    #[test]
    fn table_create_prod_holds_for_confirm_then_sends() {
        with_isolated_state("table-create-prod", |state| {
            let conn = add_sqlite_connection_env(state, "mem-table-prod", Environment::Prod);
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);
            state.open_table_create(conn);
            {
                let editor = state.table_editor_mut().expect("editor open");
                editor.form.table = "users".to_owned();
                editor.form.columns = vec![crate::ui::ddl::TableColumnDraft {
                    name: "id".to_owned(),
                    data_type: "INTEGER".to_owned(),
                    nullable: false,
                    default: String::new(),
                    pk: true,
                }];
            }
            state.apply_table_ddl(tab);
            assert!(commands.try_recv().is_err(), "prod holds the batch");
            assert!(state.pending_table_ddl().is_some(), "held for confirmation");
            state.cancel_table_ddl();
            assert!(state.pending_table_ddl().is_none());
            assert!(commands.try_recv().is_err(), "cancel sends nothing");
            state.apply_table_ddl(tab);
            assert!(state.pending_table_ddl().is_some());
            state.confirm_table_ddl();
            assert!(state.pending_table_ddl().is_none());
            match commands.try_recv().expect("confirm sends the batch") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert!(statements[0].contains("CREATE TABLE \"users\""), "{statements:?}");
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn modal_slots_match_all_order() {
        for (slot, modal) in Modal::ALL.iter().copied().enumerate() {
            assert_eq!(modal.index(), slot, "{modal:?} sits in its own slot");
        }
    }

    #[test]
    fn modal_ties_fall_back_to_all_order() {
        with_isolated_state("modal-tie", |state| {
            state.open_draft(None);
            state.open_save_prompt();
            state.modal_marks = [0; Modal::ALL.len()];
            assert_eq!(
                state.modal(),
                Some(Modal::Draft),
                "an unmarked tie names the earlier dialog"
            );
        });
    }

    #[test]
    fn modal_names_each_dialog_while_its_state_is_set() {
        with_isolated_state("modal-identity", |state| {
            assert_eq!(state.modal(), None, "a fresh state is free keyboard");

            state.open_draft(None);
            assert_eq!(state.modal(), Some(Modal::Draft));
            state.cancel_draft();

            state.open_save_prompt();
            assert_eq!(state.modal(), Some(Modal::SavePrompt));
            state.close_save_prompt();

            let (tab, conn, _commands) = editable_tab(state);
            state.activate(tab);

            let csv = std::env::temp_dir().join(format!(
                "cde-modal-import-{}.csv",
                std::process::id()
            ));
            std::fs::write(&csv, "tenant,id,name\nacme,1,Ada\n").unwrap();
            state.begin_import(tab, &csv);
            assert_eq!(state.modal(), Some(Modal::Import));
            state.cancel_import();
            let _ = std::fs::remove_file(&csv);

            state.start_run(tab, "DELETE FROM items", 0, None);
            assert_eq!(state.modal(), Some(Modal::WriteConfirm));
            state.cancel_write();

            state.start_run(tab, "SELECT item_name FROM items WHERE item_name = :name", 0, None);
            assert_eq!(state.modal(), Some(Modal::Params));
            state.cancel_params();

            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            state.apply_edits(tab);
            assert_eq!(state.modal(), Some(Modal::EditPreview));
            state.cancel_pending_edits();

            state.open_table_create(conn);
            assert_eq!(state.modal(), Some(Modal::TableEditor));
            state.close_table_editor();

            state.open_compare();
            assert_eq!(state.modal(), Some(Modal::Compare));
            state.close_compare();

            state.open_palette();
            assert_eq!(state.modal(), Some(Modal::Palette));
            state.close_palette();

            let prod = add_sqlite_connection_env(state, "prod-modal", Environment::Prod);
            state.connect(prod);
            assert_eq!(state.modal(), Some(Modal::ProdConnect));
            state.cancel_prod();

            state.request_terminate(prod, "4242");
            assert_eq!(state.modal(), Some(Modal::Terminate));
            state.cancel_terminate();

            let prod_tab = state.new_tab(prod);
            state.open_table_create(prod);
            {
                let editor = state.table_editor_mut().expect("editor open");
                editor.form.table = "audit".to_owned();
                editor.form.columns = vec![crate::ui::ddl::TableColumnDraft {
                    name: "id".to_owned(),
                    data_type: "INTEGER".to_owned(),
                    nullable: false,
                    default: String::new(),
                    pk: true,
                }];
            }
            state.apply_table_ddl(prod_tab);
            assert_eq!(state.modal(), Some(Modal::TableDdl), "the confirm outranks its editor");
            state.cancel_table_ddl();
            state.close_table_editor();

            let prompting = add_postgres_connection_env(state, "pg-modal", "", Environment::Dev);
            state.open_password_prompt(prompting);
            assert_eq!(state.modal(), Some(Modal::PasswordPrompt));
            assert_eq!(state.connections().len(), 3, "the prompt spawns nothing");
            state.cancel_password_prompt();
        });
    }

    #[test]
    fn table_create_validation_failure_lands_error_and_sends_nothing() {
        with_isolated_state("table-create-invalid", |state| {
            let conn = add_sqlite_connection(state, "mem-table-bad");
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);
            state.open_table_create(conn);
            assert!(!state.table_ddl_errors().is_empty(), "blank name fails");
            state.apply_table_ddl(tab);
            assert!(commands.try_recv().is_err(), "invalid sends nothing");
            match state.tab(tab).expect("tab").outcome() {
                RunOutcome::Err(message) => assert!(message.contains("table name"), "{message}"),
                other => panic!("expected a validation error, got {other:?}"),
            }
        });
    }

    #[test]
    fn table_alter_sends_add_column_and_index() {
        with_isolated_state("table-alter-send", |state| {
            let (tab, conn, commands) = editable_tab(state);
            state.open_table_alter(conn, "main", "items");
            {
                let editor = state.table_editor_mut().expect("editor open");
                editor.form.columns.push(crate::ui::ddl::TableColumnDraft {
                    name: "email".to_owned(),
                    data_type: "TEXT".to_owned(),
                    nullable: true,
                    default: String::new(),
                    pk: false,
                });
                editor.form.indexes.push(crate::ui::ddl::TableIndexDraft {
                    name: "idx_items_email".to_owned(),
                    columns: "email".to_owned(),
                    unique: false,
                });
            }
            assert!(state.table_ddl_errors().is_empty());
            state.apply_table_ddl(tab);
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => assert_eq!(
                    statements,
                    vec![
                        "ALTER TABLE \"main\".\"items\" ADD COLUMN \"email\" TEXT;",
                        "CREATE INDEX \"idx_items_email\" ON \"main\".\"items\" (\"email\");",
                    ]
                ),
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn edit_preview_modal_lists_the_exact_statements() {
        with_isolated_state("edit-preview-render", |state| {
            let (tab, _conn, _commands) = editable_tab(state);
            state.activate(tab);
            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            state.mark_row_deleted(tab, 1);
            state.apply_edits(tab);
            let expected = state.preview_edit_statements(tab).expect("preview");
            assert_eq!(expected.len(), 2);
            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input.clone(), |ui| {
                crate::ui::root(state, ui);
            });
            output.textures_delta.clear();
            let mut output = ctx.run_ui(input, |ui| {
                crate::ui::root(state, ui);
            });
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "Apply staged edits?"), "{text:?}");
            for statement in &expected {
                assert!(text.iter().any(|t| t == statement), "{text:?}");
            }
            assert!(text.iter().any(|t| t == "Confirm"), "{text:?}");
            assert!(text.iter().any(|t| t == "Cancel"), "{text:?}");
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
            assert_eq!(state.pending_edit_preview(), Some(tab));
            assert_eq!(
                state.preview_edit_statements(tab),
                Some(vec![
                    "UPDATE \"items\" SET \"name\" = 'Ada' WHERE \"tenant\" = '010' AND \"id\" = 1".to_owned()
                ])
            );
            assert!(commands.try_recv().is_err(), "the preview holds the batch");
            state.confirm_pending_edits();
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
            assert_eq!(state.pending_edit_preview(), Some(tab));
            assert!(commands.try_recv().is_err(), "the preview holds the batch");
            state.confirm_pending_edits();
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
            assert_eq!(state.pending_edit_preview(), Some(tab));
            state.confirm_pending_edits();
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
                crate::ui::grid::results(state, ui, None);
            });
            let pos = text_center(&first, "ada").expect("the name cell renders");
            first.textures_delta.clear();

            let mut double_click = input.clone();
            double_click.events = double_click_events(pos);
            let mut second = ctx.run_ui(double_click, |ui| {
                crate::ui::grid::results(state, ui, None);
            });
            second.textures_delta.clear();

            // The editor draws once before it holds focus, the way a user
            // types after the field appears.
            let mut opened = ctx.run_ui(input.clone(), |ui| {
                crate::ui::grid::results(state, ui, None);
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
                crate::ui::grid::results(state, ui, None);
            });
            third.textures_delta.clear();
            assert_eq!(state.tab(tab).expect("tab").edit_text(0, 2), Some("Grace"));

            // Esc closes the field without staging the typed text.
            let mut double_click = input.clone();
            double_click.events = double_click_events(pos);
            let mut fourth = ctx.run_ui(double_click, |ui| {
                crate::ui::grid::results(state, ui, None);
            });
            fourth.textures_delta.clear();
            let mut opened = ctx.run_ui(input.clone(), |ui| {
                crate::ui::grid::results(state, ui, None);
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
                crate::ui::grid::results(state, ui, None);
            });
            fifth.textures_delta.clear();
            assert_eq!(state.tab(tab).expect("tab").edit_text(0, 2), Some("Grace"));
            assert_eq!(state.tab(tab).expect("tab").staged_count(), 1);
        });
    }

    /// A CSV import end to end through the real SQLite worker: the batch
    /// inserts, the silent refresh re-reads the tab, and the notice lands.
    #[test]
    fn import_inserts_and_refreshes_through_the_sqlite_worker() {
        with_isolated_state("import-live", |state| {
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

            let path = write_csv("live", "tenant,id,name\ninitech,3,Lin\ncobalt,4,Ada\n");
            state.begin_import(tab, &path);
            assert!(state.import_draft().is_some(), "draft staged");
            state.commit_import();
            let notice = "Imported 2 rows into items (0 skipped, 0 dropped)";
            loop {
                state.drain_events();
                let view = state.tab(tab).expect("tab");
                let running = matches!(view.outcome(), RunOutcome::Running { .. });
                if !running && view.export_notice() == Some(notice) {
                    break;
                }
                assert!(Instant::now() < deadline, "import and refresh did not finish");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let view = state.tab(tab).expect("tab");
            let result = view.active_query_result().expect("refreshed result");
            assert_eq!(result.row_count(), 4);
            assert_eq!(result.cell(2, 0), Some(&Value::Text("initech".to_owned())));
            assert_eq!(result.cell(2, 1), Some(&Value::Integer(3)));
            assert_eq!(result.cell(2, 2), Some(&Value::Text("Lin".to_owned())));
            assert_eq!(result.cell(3, 0), Some(&Value::Text("cobalt".to_owned())));
            assert_eq!(result.cell(3, 1), Some(&Value::Integer(4)));
            assert_eq!(result.cell(3, 2), Some(&Value::Text("Ada".to_owned())));
            let _ = std::fs::remove_file(&path);
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
            assert_eq!(state.pending_edit_preview(), Some(tab));
            state.confirm_pending_edits();
            assert_eq!(state.pending_edit_preview(), None);
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
                    eframe::egui::vec2(1800.0, 800.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input.clone(), |ui| {
                crate::ui::grid::results(state, ui, None);
            });
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "Edit"), "{text:?}");
            assert!(text.iter().any(|t| t == "editing items"), "{text:?}");
            assert!(!text.iter().any(|t| t.starts_with("Apply (")), "{text:?}");
            assert!(!text.iter().any(|t| t == "Discard"), "{text:?}");
            assert!(!text.iter().any(|t| t.ends_with(" staged")), "{text:?}");

            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            state.add_row(tab);
            let mut output = ctx.run_ui(input.clone(), |ui| {
                crate::ui::grid::results(state, ui, None);
            });
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "2 staged"), "{text:?}");
            assert!(text.iter().any(|t| t == "Apply (2)"), "{text:?}");
            assert!(text.iter().any(|t| t == "Discard"), "{text:?}");

            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT i.id FROM items i JOIN items o ON o.id = i.id".to_owned();
            }
            let mut output = ctx.run_ui(input, |ui| {
                crate::ui::grid::results(state, ui, None);
            });
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "read-only view"), "{text:?}");
            assert!(!text.iter().any(|t| t == "Edit"), "{text:?}");
            assert!(!text.iter().any(|t| t.starts_with("Apply")), "{text:?}");
        });
    }

    #[test]
    fn sidebar_views_switch_between_schema_history_and_saved() {
        with_isolated_state("sidebar-availability", |state| {
            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let render = |state: &mut AppState| {
                let mut output = ctx.run_ui(input.clone(), |ui| {
                    crate::ui::connections::sidebar(state, ui);
                    crate::ui::query::editor(state, ui);
                });
                output.textures_delta.clear();
                rendered_text(&output)
            };

            // The default Schema view shows the segmented row and the empty
            // connection state, not the other sections.
            let text = render(state);
            assert!(text.iter().any(|t| t == "Schema"), "{text:?}");
            assert!(text.iter().any(|t| t == "History"), "{text:?}");
            assert!(text.iter().any(|t| t == "Saved"), "{text:?}");
            assert!(text.iter().any(|t| t.contains("No connections yet")), "{text:?}");
            assert!(!text.iter().any(|t| t == "HISTORY"), "{text:?}");
            assert!(!text.iter().any(|t| t == "SAVED"), "{text:?}");
            assert!(!text.iter().any(|t| t.contains("Import SQL")), "{text:?}");
            assert!(text.iter().any(|t| t == "+ New connection"), "{text:?}");

            // Each chosen view shows its section even when it is empty.
            state.set_sidebar_view(SidebarView::History);
            let text = render(state);
            assert!(text.iter().any(|t| t == "HISTORY"), "{text:?}");
            assert!(text.iter().any(|t| t == "No runs yet."), "{text:?}");
            assert!(!text.iter().any(|t| t == "SAVED"), "{text:?}");

            state.set_sidebar_view(SidebarView::Saved);
            let text = render(state);
            assert!(text.iter().any(|t| t == "SAVED"), "{text:?}");
            assert!(text.iter().any(|t| t == "No saved queries."), "{text:?}");
            assert!(text.iter().any(|t| t.contains("Import SQL")), "{text:?}");
            assert!(!text.iter().any(|t| t == "HISTORY"), "{text:?}");

            // Rows that outlive the last connection land in their views.
            state.saved.insert(
                0,
                SavedQuery::new(
                    1,
                    "keep".to_owned(),
                    "SELECT 1".to_owned(),
                    ConnectionId::from_raw(9),
                    None,
                    false,
                )
                .expect("named query"),
            );
            state.history.push(HistoryEntry {
                conn: ConnectionId::from_raw(9),
                sql: "SELECT 1".to_owned(),
                succeeded: true,
                rows: Some(1),
                took: std::time::Duration::from_millis(1),
                at: 0,
            });
            let text = render(state);
            assert!(text.iter().any(|t| t == "keep"), "{text:?}");

            state.set_sidebar_view(SidebarView::History);
            let text = render(state);
            assert!(text.iter().any(|t| t == "SELECT 1"), "{text:?}");

            state.set_sidebar_view(SidebarView::Schema);
            let text = render(state);
            assert!(!text.iter().any(|t| t == "SAVED"), "{text:?}");
            assert!(!text.iter().any(|t| t.contains("Import SQL")), "{text:?}");
        });
    }

    #[test]
    fn table_count_requests_once_caches_and_clears_with_the_session() {
        with_isolated_state("row-count-state", |state| {
            let conn = add_sqlite_connection(state, "mem-count");
            let commands = attach_test_session(state, conn);
            state.request_row_count(conn, "main", "items");
            match commands.try_recv().expect("a command was sent") {
                DbCommand::RowCount { schema, table } => {
                    assert_eq!(schema, "main");
                    assert_eq!(table, "items");
                }
                other => panic!("expected RowCount, got {other:?}"),
            }
            assert_eq!(state.row_count(conn, "main", "items"), Some(&RowCountState::Pending));
            // A second request while the first is in flight sends nothing.
            state.request_row_count(conn, "main", "items");
            assert!(commands.try_recv().is_err());
            state.apply(DbEvent::RowCountOk {
                conn,
                schema: "main".to_owned(),
                table: "items".to_owned(),
                count: RowCount::Exact(12),
            });
            assert_eq!(
                state.row_count(conn, "main", "items"),
                Some(&RowCountState::Ready(RowCount::Exact(12)))
            );
            state.disconnect(conn);
            assert_eq!(state.row_count(conn, "main", "items"), None);
        });
    }

    #[test]
    fn table_count_error_caches_the_message_and_a_retry_sends_again() {
        with_isolated_state("row-count-error", |state| {
            let conn = add_sqlite_connection(state, "mem-count-error");
            let commands = attach_test_session(state, conn);
            state.request_row_count(conn, "main", "gone");
            assert!(matches!(commands.try_recv(), Ok(DbCommand::RowCount { .. })));
            state.apply(DbEvent::RowCountErr {
                conn,
                schema: "main".to_owned(),
                table: "gone".to_owned(),
                error: crate::db::DbError::Query("no such table: main.gone".to_owned()),
            });
            assert_eq!(
                state.row_count(conn, "main", "gone"),
                Some(&RowCountState::Failed(
                    crate::db::DbError::Query("no such table: main.gone".to_owned())
                        .to_string()
                ))
            );
            state.request_row_count(conn, "main", "gone");
            assert!(matches!(commands.try_recv(), Ok(DbCommand::RowCount { .. })));

            // A fresh worker session drops the previous session's cache.
            let _reconnect = attach_test_handle(state, conn);
            state.apply(DbEvent::Connected {
                conn,
                cancel: CancelHandle::new(|| {}),
                server_version: "test".to_owned(),
            });
            assert_eq!(state.row_count(conn, "main", "gone"), None);
        });
    }

    #[test]
    fn schema_count_affordance_requests_and_renders_the_cached_count() {
        with_isolated_state("count-render", |state| {
            let conn = add_sqlite_connection(state, "mem-count-render");
            let commands = attach_test_session(state, conn);
            state.apply(DbEvent::SchemaOk { conn, schema: items_schema() });
            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            type Action = crate::ui::schema::SchemaAction;
            let render = |state: &mut AppState, events: Vec<eframe::egui::Event>| {
                let mut raw = input.clone();
                raw.events = events;
                let mut captured = Vec::new();
                let mut output = ctx.run_ui(raw, |ui| {
                    captured = crate::ui::schema::connection_schema(state, ui, conn);
                });
                output.textures_delta.clear();
                (output, captured)
            };
            let click = |pos, pressed| eframe::egui::Event::PointerButton {
                pos,
                button: eframe::egui::PointerButton::Primary,
                pressed,
                modifiers: eframe::egui::Modifiers::NONE,
            };
            let click_at = |pos| {
                vec![
                    eframe::egui::Event::PointerMoved(pos),
                    click(pos, true),
                    click(pos, false),
                ]
            };

            // Collapsed tables render no affordance.
            let (first, _) = render(state, Vec::new());
            let text = rendered_text(&first);
            assert!(text.iter().any(|t| t == "items (3)"), "{text:?}");
            assert!(!text.iter().any(|t| t == "Count rows"), "{text:?}");

            // Clicking the table header expands it, which reveals the count row.
            let header = text_center(&first, "items (3)").expect("table header");
            let (_, header_actions) = render(state, click_at(header));
            assert!(
                header_actions.iter().any(
                    |a| matches!(a, Action::Preview(_, s, t) if s == "main" && t == "items")
                ),
                "{header_actions:?}"
            );
            let (open, _) = render(state, Vec::new());
            let text = rendered_text(&open);
            assert!(text.iter().any(|t| t == "Count rows"), "{text:?}");

            // The count button returns a request action for this table.
            let count = text_center(&open, "Count rows").expect("count button");
            let (_, count_actions) = render(state, click_at(count));
            assert!(
                count_actions
                    .iter()
                    .any(|a| matches!(a, Action::Count(_, s, t) if s == "main" && t == "items")),
                "{count_actions:?}"
            );

            state.request_row_count(conn, "main", "items");
            assert!(matches!(commands.try_recv(), Ok(DbCommand::RowCount { .. })));
            let (pending, _) = render(state, Vec::new());
            let text = rendered_text(&pending);
            assert!(text.iter().any(|t| t == "Counting…"), "{text:?}");

            state.apply(DbEvent::RowCountOk {
                conn,
                schema: "main".to_owned(),
                table: "items".to_owned(),
                count: RowCount::Estimate(42),
            });
            let (estimated, _) = render(state, Vec::new());
            let text = rendered_text(&estimated);
            assert!(text.iter().any(|t| t == "~42 rows (estimate)"), "{text:?}");

            state.apply(DbEvent::RowCountOk {
                conn,
                schema: "main".to_owned(),
                table: "items".to_owned(),
                count: RowCount::Exact(7),
            });
            let (exact, _) = render(state, Vec::new());
            let text = rendered_text(&exact);
            assert!(text.iter().any(|t| t == "7 rows"), "{text:?}");
            assert!(!text.iter().any(|t| t.contains("estimate")), "{text:?}");

            // A failed count shows the engine error and a retry, and the
            // table's own sections still render.
            let message =
                crate::db::DbError::Query("no such table: main.items".to_owned()).to_string();
            state.apply(DbEvent::RowCountErr {
                conn,
                schema: "main".to_owned(),
                table: "items".to_owned(),
                error: crate::db::DbError::Query("no such table: main.items".to_owned()),
            });
            let (failed, _) = render(state, Vec::new());
            let text = rendered_text(&failed);
            assert!(text.iter().any(|t| t == &message), "{text:?}");
            assert!(text.iter().any(|t| t == "Retry"), "{text:?}");
            assert!(text.iter().any(|t| t == "Columns (3)"), "{text:?}");
        });
    }

    /// Write a CSV for a test and return its path. The caller removes it.
    fn write_csv(tag: &str, text: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("cde-csv-{tag}-{}.csv", std::process::id()));
        std::fs::write(&path, text).expect("write csv");
        path
    }

    #[test]
    fn begin_import_stages_the_file_and_commit_sends_the_exact_inserts() {
        with_isolated_state("import-commit", |state| {
            let (tab, _conn, commands) = editable_tab(state);
            let path = write_csv("commit", "TENANT,Id,note\nacme,1,first\nglobex,2,second\n");
            state.begin_import(tab, &path);
            let draft = state.import_draft().expect("draft");
            assert_eq!(draft.tab, tab);
            assert_eq!(draft.table, "items");
            assert_eq!(draft.headers, vec!["TENANT", "Id", "note"]);
            assert_eq!(draft.rows.len(), 2);
            assert_eq!(draft.map.pairs, vec![(0, 0), (1, 1)]);
            assert_eq!(draft.map.skipped, vec!["note"]);

            state.commit_import();
            assert!(state.import_draft().is_none());
            let run = match commands.try_recv().expect("insert batch") {
                DbCommand::QueryBatch { run, statements } => {
                    assert_eq!(
                        statements,
                        vec![
                            "INSERT INTO \"items\" (\"tenant\", \"id\") \
                             VALUES ('acme', 1), ('globex', 2)"
                        ]
                    );
                    run
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            // The notice lands with the batch, not when it is sent.
            assert_eq!(state.tab(tab).expect("tab").export_notice(), None);
            let notice = "Imported 2 rows into items (1 skipped, 0 dropped)";
            let before = state.history().len();
            state.apply(DbEvent::BatchOk { run, results: vec![affected_result(2)] });
            assert_eq!(state.history().len(), before);
            let view = state.tab(tab).expect("tab");
            assert_eq!(view.export_notice(), Some(notice));
            assert!(!view.export_failed());

            // The import batch adds no history entry and quietly re-reads
            // the tab; the notice outlives the refresh landing.
            let refresh_run = match commands.try_recv().expect("silent refresh") {
                DbCommand::QueryBatch { run, statements } => {
                    assert_eq!(statements, vec!["SELECT tenant, id, name FROM items"]);
                    run
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            state.apply(DbEvent::BatchOk { run: refresh_run, results: vec![items_result()] });
            assert_eq!(state.history().len(), before);
            let view = state.tab(tab).expect("tab");
            assert_eq!(view.export_notice(), Some(notice));
            assert_eq!(view.active_query_result().map(|r| r.row_count()), Some(2));
            assert!(commands.try_recv().is_err());
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn import_over_the_cap_sends_twenty_batches_and_reports_the_dropped_rows() {
        with_isolated_state("import-cap", |state| {
            let (tab, _conn, commands) = editable_tab(state);
            let mut text = String::from("tenant,id,name\n");
            for i in 0..1002 {
                text.push_str(&format!("t{i},{i},n{i}\n"));
            }
            let path = write_csv("cap", &text);
            state.begin_import(tab, &path);
            state.commit_import();
            let run = match commands.try_recv().expect("insert batches") {
                DbCommand::QueryBatch { run, statements } => {
                    assert_eq!(statements.len(), 20);
                    assert!(statements[0].contains("('t0', 0, 'n0')"), "{}", statements[0]);
                    assert!(statements[19].contains("('t999', 999, 'n999')"));
                    assert!(!statements[19].contains("t1000"));
                    run
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            state.apply(DbEvent::BatchOk { run, results: vec![affected_result(1000)] });
            assert_eq!(
                state.tab(tab).expect("tab").export_notice(),
                Some("Imported 1000 rows into items (0 skipped, 2 dropped)")
            );
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn cancel_import_clears_and_commit_without_a_draft_sends_nothing() {
        with_isolated_state("import-cancel", |state| {
            let (tab, _conn, commands) = editable_tab(state);
            state.commit_import();
            state.cancel_import();
            assert!(state.import_draft().is_none());
            assert!(commands.try_recv().is_err());

            let path = write_csv("cancel", "tenant,id,name\nacme,1,ada\n");
            state.begin_import(tab, &path);
            assert!(state.import_draft().is_some());
            state.cancel_import();
            assert!(state.import_draft().is_none());
            state.commit_import();
            assert!(commands.try_recv().is_err());
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn a_file_that_does_not_import_errors_into_the_tab_notice() {
        with_isolated_state("import-bad", |state| {
            let (tab, _conn, commands) = editable_tab(state);
            let ragged = write_csv("ragged", "tenant,id,name\nacme,1\n");
            state.begin_import(tab, &ragged);
            assert!(state.import_draft().is_none());
            let view = state.tab(tab).expect("tab");
            assert!(view.export_failed());
            assert_eq!(
                view.export_notice(),
                Some("Import failed: the file needs a header and rows of one column count")
            );
            state.begin_import(tab, std::path::Path::new("/nonexistent/cde-import.csv"));
            let notice = state.tab(tab).expect("tab").export_notice().unwrap_or_default();
            assert!(notice.starts_with("Import failed: "), "{notice}");
            assert!(notice.contains("cde-import.csv"), "{notice}");

            // Headers that match no column stage a draft that cannot send.
            let unmatched = write_csv("unmatched", "zzz\n1\n");
            state.begin_import(tab, &unmatched);
            assert_eq!(state.import_draft().expect("draft").map.skipped, vec!["zzz"]);
            state.commit_import();
            assert_eq!(
                state.tab(tab).expect("tab").export_notice(),
                Some("Import failed: no CSV header matches items")
            );
            assert!(state.tab(tab).expect("tab").export_failed());
            assert!(commands.try_recv().is_err());
            let _ = std::fs::remove_file(&ragged);
            let _ = std::fs::remove_file(&unmatched);
        });
    }

    /// Write a JSONL file for a test and return its path. The caller removes it.
    fn write_jsonl(tag: &str, text: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("cde-jsonl-{tag}-{}.jsonl", std::process::id()));
        std::fs::write(&path, text).expect("write jsonl");
        path
    }

    #[test]
    fn import_source_names_the_parser_by_extension() {
        assert_eq!(
            ImportSource::of_path(std::path::Path::new("rows.jsonl")),
            ImportSource::Jsonl
        );
        assert_eq!(
            ImportSource::of_path(std::path::Path::new("ROWS.JSONL")),
            ImportSource::Jsonl
        );
        assert_eq!(
            ImportSource::of_path(std::path::Path::new("rows.csv")),
            ImportSource::Csv
        );
        assert_eq!(
            ImportSource::of_path(std::path::Path::new("rows")),
            ImportSource::Csv
        );
        assert_eq!(ImportSource::Csv.label(), "CSV");
        assert_eq!(ImportSource::Jsonl.label(), "JSONL");
    }

    #[test]
    fn jsonl_by_extension_stages_through_the_modal_and_sends_the_exact_inserts() {
        with_isolated_state("import-jsonl-modal", |state| {
            let (tab, _conn, commands) = editable_tab(state);
            let path = write_jsonl(
                "modal",
                "{\"tenant\": \"acme\", \"id\": 1, \"note\": \"x\"}\n\
                 {\"tenant\": \"globex\", \"id\": 2}\n",
            );
            state.begin_import(tab, &path);
            let draft = state.import_draft().expect("draft");
            assert_eq!(draft.source, ImportSource::Jsonl);
            assert_eq!(draft.column_title(), "JSONL key");
            assert_eq!(draft.table, "items");
            // serde_json maps sort keys, so the union reads sorted per
            // object: the values still land on the right table columns.
            assert_eq!(draft.headers, vec!["id", "note", "tenant"]);
            assert_eq!(draft.map.pairs, vec![(2, 0), (0, 1)]);
            assert_eq!(draft.map.skipped, vec!["note"]);

            state.commit_import();
            assert!(state.import_draft().is_none());
            let run = match commands.try_recv().expect("insert batch") {
                DbCommand::QueryBatch { run, statements } => {
                    assert_eq!(
                        statements,
                        vec![
                            "INSERT INTO \"items\" (\"tenant\", \"id\") \
                             VALUES ('acme', 1), ('globex', 2)"
                        ]
                    );
                    run
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            let notice = "Imported 2 rows into items (1 skipped, 0 dropped)";
            let before = state.history().len();
            state.apply(DbEvent::BatchOk { run, results: vec![affected_result(2)] });
            assert_eq!(state.history().len(), before);
            let view = state.tab(tab).expect("tab");
            assert_eq!(view.export_notice(), Some(notice));
            assert!(!view.export_failed());
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn jsonl_modal_rejects_non_objects_and_unmatched_keys() {
        with_isolated_state("import-jsonl-bad", |state| {
            let (tab, _conn, commands) = editable_tab(state);
            let bad = write_jsonl("bad", "{\"tenant\": \"acme\"}\ntrailing\n");
            state.begin_import(tab, &bad);
            assert!(state.import_draft().is_none());
            let view = state.tab(tab).expect("tab");
            assert!(view.export_failed());
            assert_eq!(
                view.export_notice(),
                Some("Import failed: the file needs one JSON object per line")
            );

            let unmatched = write_jsonl("unmatched", "{\"zzz\": 1}\n");
            state.begin_import(tab, &unmatched);
            assert_eq!(
                state.import_draft().expect("draft").map.skipped,
                vec!["zzz"]
            );
            state.commit_import();
            assert_eq!(
                state.tab(tab).expect("tab").export_notice(),
                Some("Import failed: no JSONL key matches items")
            );
            assert!(state.tab(tab).expect("tab").export_failed());
            assert!(commands.try_recv().is_err());
            let _ = std::fs::remove_file(&bad);
            let _ = std::fs::remove_file(&unmatched);
        });
    }

    #[test]
    fn jsonl_modal_renders_the_key_mapping() {
        with_isolated_state("import-jsonl-render", |state| {
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
            let path = write_jsonl("render", "{\"tenant\": \"acme\", \"extra\": 1}\n");
            state.begin_import(tab, &path);
            assert!(state.import_draft().is_some(), "draft staged");
            let render = |state: &mut AppState| {
                let mut output = ctx.run_ui(input.clone(), |ui| {
                    crate::ui::root(state, ui);
                });
                output.textures_delta.clear();
                output
            };
            render(state);
            let output = render(state);
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "Import into items"), "{text:?}");
            assert!(text.iter().any(|t| t == "JSONL key"), "{text:?}");
            assert!(text.iter().any(|t| t == "Skipped headers: extra"), "{text:?}");
            assert!(text.iter().any(|t| t == "Import"), "{text:?}");
            assert!(text.iter().any(|t| t == "Cancel"), "{text:?}");
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn sql_import_names_come_from_file_stems() {
        assert_eq!(
            sql_import_name_for_path(std::path::Path::new("reports/daily.sql")),
            "daily"
        );
        assert_eq!(
            sql_import_name_for_path(std::path::Path::new("MONTHLY.SQL")),
            "MONTHLY"
        );
        assert_eq!(
            sql_import_name_for_path(std::path::Path::new("query")),
            "query"
        );
        assert_eq!(sql_import_name_for_path(std::path::Path::new(".sql")), ".sql");
    }

    /// Write one `.sql` file under `dir` and return its path.
    fn write_sql_file(dir: &std::path::Path, name: &str, sql: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, sql).expect("write sql");
        path
    }

    #[test]
    fn sql_import_directory_creates_one_query_per_file_in_folder() {
        with_isolated_state("sql-import-dir", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let dir = std::env::temp_dir()
                .join(format!("cde-sql-import-dir-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            write_sql_file(&dir, "b.sql", "SELECT 2");
            write_sql_file(&dir, "a.sql", "SELECT 1");
            write_sql_file(&dir, "notes.txt", "SELECT 3");

            let files = collect_sql_files(&dir);
            assert_eq!(
                files,
                vec![dir.join("a.sql"), dir.join("b.sql")],
                "only .sql files, sorted"
            );
            // A missing directory reads as empty instead of erroring.
            assert!(collect_sql_files(&dir.join("gone")).is_empty());

            let outcome =
                state.import_sql_files(&files, Some(" //ops// db/ ")).expect("imports");
            assert_eq!(outcome, SqlImportOutcome { imported: 2, skipped: vec![] });
            let saved = state.saved_queries();
            assert_eq!(saved.len(), 2);
            assert_eq!(saved[0].name, "a");
            assert_eq!(saved[0].sql, "SELECT 1");
            assert_eq!(saved[1].name, "b");
            assert_eq!(saved[1].sql, "SELECT 2");
            for query in saved {
                assert_eq!(query.folder.as_deref(), Some("ops/db"));
                assert_eq!(query.conn, conn);
                assert_eq!(query.versions.len(), 1);
                assert_eq!(query.versions[0].sql, query.sql);
            }

            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.saved_queries().len(), 2);
            assert_eq!(reloaded.saved_queries()[0].name, "a");
            assert_eq!(
                reloaded.saved_queries()[0].folder.as_deref(),
                Some("ops/db")
            );
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn sql_import_replaces_same_name_skips_blank_and_needs_a_connection() {
        with_isolated_state("sql-import-replace", |state| {
            // No connections yet: nothing to attribute the queries to.
            let dir = std::env::temp_dir()
                .join(format!("cde-sql-import-replace-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let fresh = write_sql_file(&dir, "daily.sql", "SELECT 1");
            assert!(state.import_sql_files(&[fresh.clone()], None).is_none());

            let conn = add_sqlite_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.activate(tab);
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT 0".to_owned();
            }
            let first = state.save_active_tab("daily", None, false).expect("saved");
            let blank = write_sql_file(&dir, "blank.sql", "  \n ");
            let outcome =
                state.import_sql_files(&[fresh, blank.clone()], None).expect("imports");
            assert_eq!(
                outcome,
                SqlImportOutcome { imported: 1, skipped: vec!["blank.sql".to_owned()] }
            );
            // The same name in the same folder replaces in place, keeping its
            // id and recording a version, instead of saving beside it.
            let saved = state.saved_queries();
            assert_eq!(saved.len(), 1);
            assert_eq!(saved[0].id, first);
            assert_eq!(saved[0].sql, "SELECT 1");
            assert_eq!(saved[0].conn, conn);
            assert_eq!(
                state.saved_versions(first).iter().map(|v| v.sql.clone()).collect::<Vec<_>>(),
                vec!["SELECT 1", "SELECT 0"]
            );
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn grid_import_button_and_modal_render_the_mapping_and_skipped_headers() {
        with_isolated_state("import-render", |state| {
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
                crate::ui::root(state, ui);
            });
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "Edit"), "{text:?}");

            let path = write_csv("render", "tenant,id,extra\nacme,1,x\n");
            state.begin_import(tab, &path);
            assert!(state.import_draft().is_some(), "draft staged");
            // A fresh egui window paints from its second frame on.
            let render = |state: &mut AppState| {
                let mut output = ctx.run_ui(input.clone(), |ui| {
                    crate::ui::root(state, ui);
                });
                output.textures_delta.clear();
                output
            };
            render(state);
            let output = render(state);
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "Import into items"), "{text:?}");
            assert!(text.iter().any(|t| t == "CSV column"), "{text:?}");
            assert!(text.iter().any(|t| t == "Skipped headers: extra"), "{text:?}");
            assert!(text.iter().any(|t| t == "Import"), "{text:?}");
            assert!(text.iter().any(|t| t == "Cancel"), "{text:?}");

            // A tab the edit path cannot resolve offers no import.
            state.cancel_import();
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = "SELECT i.id FROM items i JOIN items o ON o.id = i.id".to_owned();
            }
            let mut output = ctx.run_ui(input, |ui| {
                crate::ui::grid::results(state, ui, None);
            });
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(!text.iter().any(|t| t == "Edit"), "{text:?}");
            assert!(!text.iter().any(|t| t == "Import file"), "{text:?}");
            let _ = std::fs::remove_file(&path);
        });
    }

    fn add_read_only_sqlite_connection(state: &mut AppState, name: &str) -> ConnectionId {
        let id = add_sqlite_connection(state, name);
        state
            .connections
            .iter_mut()
            .find(|c| c.id == id)
            .expect("connection")
            .config
            .set_read_only(true);
        id
    }

    #[test]
    fn read_only_connection_refuses_each_write_kind() {
        with_isolated_state("read-only-gate", |state| {
            let conn = add_read_only_sqlite_connection(state, "mem-ro");
            let commands = attach_test_handle(state, conn);
            let tab = state.new_tab(conn);
            for (sql, kind) in [
                ("INSERT INTO t (a) VALUES (1)", "INSERT"),
                ("UPDATE t SET a = 1 WHERE id = 2", "UPDATE"),
                ("DELETE FROM t WHERE id = 1", "DELETE"),
                ("CREATE TABLE t (a INTEGER)", "CREATE"),
                ("ALTER TABLE t ADD COLUMN b TEXT", "ALTER"),
                ("DROP TABLE t", "DROP"),
                ("TRUNCATE TABLE t", "TRUNCATE"),
                ("REPLACE INTO t (a) VALUES (1)", "REPLACE"),
                ("MERGE INTO t USING s ON t.a = s.a WHEN MATCHED THEN UPDATE SET a = 1", "MERGE"),
                ("COPY t FROM 'f.csv'", "COPY"),
                ("VACUUM", "VACUUM"),
                ("GRANT SELECT ON t TO reader", "GRANT"),
                ("REVOKE SELECT ON t FROM reader", "REVOKE"),
                ("SELECT 1; DELETE FROM t", "DELETE"),
            ] {
                state.start_run(tab, sql, 0, None);
                let expected = format!("invalid settings: read-only connection refused {kind}");
                match state.tab(tab).expect("tab").outcome() {
                    RunOutcome::Err(message) => assert_eq!(message, &expected, "{sql}"),
                    other => panic!("expected Err for {sql}, got {other:?}"),
                }
                assert!(commands.try_recv().is_err(), "{sql} sent nothing");
                assert!(state.pending_write().is_none(), "{sql} arms no confirm");
            }
        });
    }

    #[test]
    fn read_only_connection_runs_reads_explain_and_pure_batches() {
        with_isolated_state("read-only-reads", |state| {
            let conn = add_read_only_sqlite_connection(state, "mem-ro");
            let commands = attach_test_handle(state, conn);
            let tab = state.new_tab(conn);
            state.start_run(tab, "SELECT * FROM t", 0, None);
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["SELECT * FROM t"]);
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
            let second = state.new_tab(conn);
            state.start_run(second, "SELECT 1; SELECT 2", 0, None);
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["SELECT 1", "SELECT 2"]);
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
            let explained = state.new_tab(conn);
            state.start_explain(explained, "DELETE FROM t", 0, None);
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["EXPLAIN QUERY PLAN DELETE FROM t"]);
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn read_only_connection_refuses_apply_and_import() {
        with_isolated_state("read-only-edits", |state| {
            let (tab, conn, commands) = editable_tab(state);
            state
                .connections
                .iter_mut()
                .find(|c| c.id == conn)
                .expect("connection")
                .config
                .set_read_only(true);
            state.stage_edit(tab, 0, 2, "Ada".to_owned());
            state.apply_edits(tab);
            assert_eq!(state.pending_edit_preview(), None, "read-only refuses without a preview");
            match state.tab(tab).expect("tab").outcome() {
                RunOutcome::Err(message) => assert_eq!(
                    message,
                    "invalid settings: read-only connection refused UPDATE"
                ),
                other => panic!("expected Err, got {other:?}"),
            }
            assert!(commands.try_recv().is_err());

            let path = write_csv("ro-import", "tenant,id,name\nacme,1,ada\n");
            state.begin_import(tab, &path);
            assert!(state.import_draft().is_some(), "draft staged");
            state.commit_import();
            assert!(state.import_draft().is_none());
            let view = state.tab(tab).expect("tab");
            assert_eq!(
                view.export_notice(),
                Some("invalid settings: read-only connection refused INSERT")
            );
            assert!(view.export_failed());
            assert!(commands.try_recv().is_err());
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn unqualified_delete_arms_confirm_that_sends_the_exact_batch() {
        with_isolated_state("write-confirm", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.start_run(tab, "SELECT 1; DELETE FROM users", 0, None);
            assert!(commands.try_recv().is_err());
            let pending = state.pending_write().expect("held batch").clone();
            assert_eq!(pending.tab, tab);
            assert_eq!(pending.conn, conn);
            assert_eq!(pending.statements, vec!["SELECT 1", "DELETE FROM users"]);
            assert_eq!(pending.kind, "DELETE");
            assert_eq!(pending.table, "users");
            state.confirm_write();
            assert!(state.pending_write().is_none());
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["SELECT 1", "DELETE FROM users"]);
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
    fn a_new_run_clears_the_tabs_armed_write_confirm() {
        with_isolated_state("write-confirm-superseded", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.start_run(tab, "DELETE FROM users", 0, None);
            assert!(state.pending_write().is_some(), "the guard holds the delete");
            state.start_run(tab, "SELECT 1", 0, None);
            assert!(
                state.pending_write().is_none(),
                "a later run drops the guard for the statement it replaced"
            );
            match commands.try_recv().expect("the new run was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["SELECT 1"]);
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn confirm_write_refuses_when_the_connection_turned_read_only() {
        with_isolated_state("write-confirm-ro", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.start_run(tab, "SELECT 1; DELETE FROM users", 0, None);
            assert!(state.pending_write().is_some());
            state
                .connections
                .iter_mut()
                .find(|c| c.id == conn)
                .expect("connection")
                .config
                .set_read_only(true);
            state.confirm_write();
            assert!(state.pending_write().is_none());
            match state.tab(tab).expect("tab").outcome() {
                RunOutcome::Err(message) => assert_eq!(
                    message,
                    "invalid settings: read-only connection refused DELETE"
                ),
                other => panic!("expected Err, got {other:?}"),
            }
            assert!(commands.try_recv().is_err(), "read-only sends nothing");
        });
    }

    #[test]
    fn unqualified_update_cancel_sends_nothing() {
        with_isolated_state("write-cancel", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.start_run(tab, "UPDATE users SET name = 'ada'", 0, None);
            assert!(commands.try_recv().is_err());
            let pending = state.pending_write().expect("held batch");
            assert_eq!(pending.kind, "UPDATE");
            assert_eq!(pending.table, "users");
            state.cancel_write();
            assert!(state.pending_write().is_none());
            assert!(commands.try_recv().is_err());
            assert!(matches!(
                state.tab(tab).expect("tab").outcome(),
                RunOutcome::Never
            ));

            let qualified = state.new_tab(conn);
            state.start_run(qualified, "DELETE FROM users WHERE id = 1", 0, None);
            assert!(state.pending_write().is_none());
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["DELETE FROM users WHERE id = 1"]);
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn named_param_prompts_then_sends_substituted_with_raw_history() {
        with_isolated_state("params-run", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            let sql = "SELECT * FROM users WHERE name = :name";
            state.start_run(tab, sql, 0, None);
            assert!(commands.try_recv().is_err(), "the prompt holds the batch");
            assert!(state.pending_write().is_none());
            assert!(state.pending_params().is_some());
            let pending = state.pending_params().expect("held params");
            assert_eq!(pending.tab, tab);
            assert_eq!(pending.conn, conn);
            assert!(!pending.explain);
            assert_eq!(pending.statements, vec![sql]);
            assert_eq!(pending.params.len(), 1);
            assert_eq!(pending.params[0].display(), ":name");

            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input.clone(), |ui| {
                crate::ui::root(state, ui);
            });
            output.textures_delta.clear();
            let mut output = ctx.run_ui(input, |ui| {
                crate::ui::root(state, ui);
            });
            output.textures_delta.clear();
            let text = rendered_text(&output);
            assert!(text.iter().any(|t| t == "Query parameters"), "{text:?}");
            assert!(text.iter().any(|t| t == ":name"), "{text:?}");
            assert!(text.iter().any(|t| t == "Run"), "{text:?}");
            assert!(text.iter().any(|t| t == "Cancel"), "{text:?}");

            state.pending_params_mut().expect("params").params[0].value =
                "ada".to_owned();
            state.confirm_params();
            assert!(state.pending_params().is_none());
            let run = match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { run, statements } => {
                    assert_eq!(statements, vec!["SELECT * FROM users WHERE name = 'ada'"]);
                    run
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            match state.tab(tab).expect("tab").outcome() {
                RunOutcome::Running { statements, .. } => {
                    assert_eq!(statements, &vec![sql.to_owned()]);
                }
                other => panic!("expected Running, got {other:?}"),
            }
            state.apply(DbEvent::BatchOk { run, results: vec![rows_result(2)] });
            assert_eq!(state.history().len(), 1);
            assert_eq!(state.history()[0].sql, sql);
            assert!(state.history()[0].succeeded);
            assert_eq!(state.history()[0].rows, Some(2));
        });
    }

    #[test]
    fn positional_params_number_across_statements() {
        with_isolated_state("params-pos", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.start_run(tab, "SELECT ?; SELECT ?, ?", 0, None);
            assert!(commands.try_recv().is_err());
            let names: Vec<String> = state
                .pending_params()
                .expect("held params")
                .params
                .iter()
                .map(|p| p.display())
                .collect();
            assert_eq!(names, vec!["?1", "?2", "?3"]);
            {
                let pending = state.pending_params_mut().expect("params");
                pending.params[0].value = "1".to_owned();
                pending.params[1].value = "ada".to_owned();
                pending.params[2].value = "7".to_owned();
            }
            state.confirm_params();
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["SELECT 1", "SELECT 'ada', 7"]);
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }
        });
    }

    #[test]
    fn params_cancel_sends_nothing() {
        with_isolated_state("params-cancel", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.start_run(tab, "SELECT * FROM t WHERE id = ?", 0, None);
            let pending = state.pending_params().expect("held params");
            assert_eq!(pending.params.len(), 1);
            assert_eq!(pending.params[0].display(), "?1");
            state.cancel_params();
            assert!(state.pending_params().is_none());
            assert!(commands.try_recv().is_err());
            assert!(matches!(
                state.tab(tab).expect("tab").outcome(),
                RunOutcome::Never
            ));
        });
    }

    #[test]
    fn mixed_placeholder_styles_error_without_sending() {
        with_isolated_state("params-mixed", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.start_run(tab, "SELECT :a, ?", 0, None);
            match state.tab(tab).expect("tab").outcome() {
                RunOutcome::Err(message) => assert!(message.contains("mix"), "{message}"),
                other => panic!("expected Err, got {other:?}"),
            }
            assert!(commands.try_recv().is_err());
            assert!(state.pending_params().is_none());
            assert!(state.pending_write().is_none());
        });
    }

    #[test]
    fn substituted_unqualified_update_arms_the_write_guard() {
        with_isolated_state("params-write", |state| {
            let (conn, commands) = scripted_connection(state, "mem");
            let tab = state.new_tab(conn);
            state.start_run(tab, "UPDATE users SET name = :name", 0, None);
            assert!(commands.try_recv().is_err());
            assert!(state.pending_write().is_none());
            state.pending_params_mut().expect("params").params[0].value =
                "ada".to_owned();
            state.confirm_params();
            assert!(state.pending_params().is_none());
            let pending = state.pending_write().expect("held write").clone();
            assert_eq!(pending.tab, tab);
            assert_eq!(pending.statements, vec!["UPDATE users SET name = 'ada'"]);
            assert_eq!(pending.kind, "UPDATE");
            assert_eq!(pending.table, "users");
            state.confirm_write();
            assert!(state.pending_write().is_none());
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { statements, .. } => {
                    assert_eq!(statements, vec!["UPDATE users SET name = 'ada'"]);
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
    fn transaction_controls_enable_only_for_an_idle_live_connection() {
        with_isolated_state("tx-ready", |state| {
            let conn = add_sqlite_connection(state, "mem-tx");
            let tab = state.new_tab(conn);
            let unbound = state.push_tab(None, String::new(), TabKind::Query);
            assert!(!state.transaction_ready(unbound), "no connection");
            assert!(!state.transaction_ready(tab), "disconnected");
            let _ = attach_test_handle(state, conn);
            assert!(!state.transaction_ready(tab), "connecting");

            let commands = attach_test_session(state, conn);
            assert!(state.transaction_ready(tab), "connected and idle");

            state.start_run(tab, "SELECT 1", 0, None);
            assert!(!state.transaction_ready(tab), "run in flight");
            let run = match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { run, .. } => run,
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            state.apply(DbEvent::BatchOk { run, results: vec![rows_result(1)] });
            assert!(state.transaction_ready(tab), "idle again");

            state.disconnect(conn);
            assert!(!state.transaction_ready(tab), "disconnected again");
        });
    }

    #[test]
    fn rollback_landing_reloads_the_schema_but_begin_and_commit_do_not() {
        with_isolated_state("tx-rollback-reload", |state| {
            let conn = add_sqlite_connection(state, "mem-rollback-reload");
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);

            let run = send_transaction(state, &commands, tab, TransactionAction::Begin);
            state.apply(DbEvent::BatchOk { run, results: vec![affected_result(0)] });
            assert!(transaction_open(state, conn), "BEGIN Ok opens the flag");
            assert!(commands.try_recv().is_err(), "a BEGIN landing sends nothing else");

            let run = send_transaction(state, &commands, tab, TransactionAction::Commit);
            state.apply(DbEvent::BatchOk { run, results: vec![affected_result(0)] });
            assert!(!transaction_open(state, conn), "COMMIT Ok closes the flag");
            assert!(commands.try_recv().is_err(), "a COMMIT landing sends nothing else");

            // The rollback may have undone DDL the navigator already listed,
            // so its landing reloads the schema.
            let run = send_transaction(state, &commands, tab, TransactionAction::Rollback);
            state.apply(DbEvent::BatchOk { run, results: vec![affected_result(0)] });
            assert!(!transaction_open(state, conn), "ROLLBACK Ok closes the flag");
            match commands.try_recv().expect("a ROLLBACK landing reloads") {
                DbCommand::LoadSchema => {}
                other => panic!("expected LoadSchema, got {other:?}"),
            }
        });
    }

    #[test]
    fn autocommit_chip_reason_follows_the_connection_state_and_engine() {
        use crate::ui::query_tools::AutocommitSlot;
        with_isolated_state("autocommit-chip", |state| {
            /// The chip for one real tab: the reason from its slot and the
            /// label from its engine.
            fn chip(state: &AppState, tab: TabId, on: bool) -> Option<(String, &'static str)> {
                let sqlite = state
                    .tab(tab)
                    .and_then(|t| t.conn)
                    .and_then(|id| state.connections().iter().find(|c| c.id == id))
                    .is_some_and(|c| c.config.kind() == DbKind::Sqlite);
                AutocommitSlot::of(state, tab).chip(on, sqlite)
            }

            // No connection at all.
            let unbound = state.push_tab(None, String::new(), TabKind::Query);
            assert_eq!(AutocommitSlot::of(state, unbound), AutocommitSlot::Offline);
            assert_eq!(
                chip(state, unbound, true),
                Some(("Autocommit: on".to_owned(), "Connect to change autocommit"))
            );

            let sqlite = add_sqlite_connection(state, "mem-chip");
            let tab = state.new_tab(sqlite);

            // Connecting, then failed: nothing to switch either way.
            attach_test_handle(state, sqlite);
            assert_eq!(
                chip(state, tab, true),
                Some(("Autocommit".to_owned(), "Connect to change autocommit"))
            );
            state
                .connections
                .iter_mut()
                .find(|c| c.id == sqlite)
                .expect("connection")
                .state = ConnectionState::Error { message: "boom".to_owned() };
            assert_eq!(
                chip(state, tab, false),
                Some(("Autocommit".to_owned(), "Connect to change autocommit"))
            );

            // Live on SQLite: the engine explains the chip, run or not.
            let commands = attach_test_session(state, sqlite);
            assert_eq!(AutocommitSlot::of(state, tab), AutocommitSlot::Sqlite);
            assert_eq!(
                chip(state, tab, true),
                Some(("Autocommit".to_owned(), "SQLite handles autocommit itself"))
            );
            state.start_run(tab, "SELECT 1", 0, None);
            assert_eq!(
                chip(state, tab, true).map(|(_, reason)| reason),
                Some("SQLite handles autocommit itself"),
                "a run in flight changes nothing on SQLite"
            );
            match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { run, .. } => {
                    state.apply(DbEvent::BatchOk { run, results: vec![rows_result(1)] });
                }
                other => panic!("expected QueryBatch, got {other:?}"),
            }

            // Live on Postgres: idle keeps the toggle, a run in flight waits
            // and the label carries the mode.
            let postgres = add_postgres_connection(state, "pg-chip", "");
            let pg_tab = state.new_tab(postgres);
            let _pg_commands = attach_test_session(state, postgres);
            assert_eq!(AutocommitSlot::of(state, pg_tab), AutocommitSlot::Toggle);
            assert_eq!(chip(state, pg_tab, false), None);
            state.start_run(pg_tab, "SELECT 1", 0, None);
            assert_eq!(AutocommitSlot::of(state, pg_tab), AutocommitSlot::Running);
            assert_eq!(
                chip(state, pg_tab, false),
                Some(("Autocommit: off".to_owned(), "Wait for the run to finish"))
            );
        });
    }

    #[test]
    fn begin_sends_exactly_begin_and_opens_the_flag_on_ok() {
        with_isolated_state("tx-begin", |state| {
            let conn = add_sqlite_connection(state, "mem-tx");
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);
            let run = send_transaction(state, &commands, tab, TransactionAction::Begin);
            assert!(!transaction_open(state, conn), "opens only on the Ok landing");

            state.apply(DbEvent::BatchOk { run, results: vec![affected_result(0)] });
            assert!(transaction_open(state, conn));
        });
    }

    #[test]
    fn commit_and_rollback_send_their_statement_and_close_the_flag_on_ok() {
        with_isolated_state("tx-controls", |state| {
            let conn = add_sqlite_connection(state, "mem-tx");
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);
            for action in [TransactionAction::Commit, TransactionAction::Rollback] {
                begin_and_land(state, &commands, tab, conn);
                let run = send_transaction(state, &commands, tab, action);
                state.apply(DbEvent::BatchOk { run, results: vec![affected_result(0)] });
                assert!(!transaction_open(state, conn), "{:?} Ok closes", action);
            }
        });
    }

    #[test]
    fn failed_begin_leaves_the_flag_closed() {
        with_isolated_state("tx-failed-begin", |state| {
            let conn = add_sqlite_connection(state, "mem-tx");
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);
            let run = send_transaction(state, &commands, tab, TransactionAction::Begin);
            state.apply(DbEvent::QueryErr {
                run,
                error: crate::db::DbError::Query("cannot begin".to_owned()),
            });
            assert!(!transaction_open(state, conn));
        });
    }

    #[test]
    fn cancel_leaves_the_flag_as_it_was() {
        with_isolated_state("tx-cancel", |state| {
            let conn = add_sqlite_connection(state, "mem-tx");
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);
            begin_and_land(state, &commands, tab, conn);

            state.start_run(tab, "SELECT 1", 0, None);
            let run = match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { run, .. } => run,
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            state.cancel_run(tab);
            assert!(transaction_open(state, conn), "cancel keeps the open transaction");
            state.apply(DbEvent::BatchOk { run, results: vec![rows_result(1)] });
            assert!(transaction_open(state, conn), "a cancelled run's landing is dropped");
        });
    }

    #[test]
    fn fatal_error_and_disconnect_close_an_open_transaction() {
        with_isolated_state("tx-closed", |state| {
            let conn = add_sqlite_connection(state, "mem-tx");
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);
            begin_and_land(state, &commands, tab, conn);

            state.start_run(tab, "SELECT 1", 0, None);
            let run = match commands.try_recv().expect("a command was sent") {
                DbCommand::QueryBatch { run, .. } => run,
                other => panic!("expected QueryBatch, got {other:?}"),
            };
            state.apply(DbEvent::QueryErr { run, error: crate::db::DbError::Gone });
            assert!(!transaction_open(state, conn), "fatal error closes");
            assert!(matches!(
                state.connections()[0].state(),
                ConnectionState::Error { .. }
            ));

            let commands = attach_test_session(state, conn);
            begin_and_land(state, &commands, tab, conn);
            state.disconnect(conn);
            assert!(!transaction_open(state, conn), "disconnect closes");

            // A BEGIN that lands after the disconnect must not reopen it.
            let commands = attach_test_session(state, conn);
            let run = send_transaction(state, &commands, tab, TransactionAction::Begin);
            state.disconnect(conn);
            state.apply(DbEvent::BatchOk { run, results: vec![affected_result(0)] });
            assert!(!transaction_open(state, conn), "late BEGIN Ok after disconnect");
        });
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_begin_commit_and_rollback_track_the_flag() {
        with_isolated_state("pg-tx-live", |state| {
            let conn = add_postgres_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live Postgres"
            );
            let tab = state.new_tab(conn);
            live_run(state, tab, "DROP TABLE IF EXISTS cde_tx_probe");
            live_run(state, tab, "CREATE TABLE cde_tx_probe (id INT PRIMARY KEY)");

            live_transaction(state, tab, TransactionAction::Begin);
            assert!(transaction_open(state, conn), "BEGIN Ok opens the flag");
            live_run(state, tab, "INSERT INTO cde_tx_probe VALUES (1)");
            live_transaction(state, tab, TransactionAction::Rollback);
            assert!(!transaction_open(state, conn), "ROLLBACK Ok closes the flag");
            live_run(state, tab, "SELECT COUNT(*) FROM cde_tx_probe");
            assert_eq!(integer_cell(state, tab), 0, "ROLLBACK dropped the insert");

            live_transaction(state, tab, TransactionAction::Begin);
            assert!(transaction_open(state, conn), "BEGIN Ok opens the flag");
            live_run(state, tab, "INSERT INTO cde_tx_probe VALUES (2)");
            live_transaction(state, tab, TransactionAction::Commit);
            assert!(!transaction_open(state, conn), "COMMIT Ok closes the flag");
            live_run(state, tab, "SELECT COUNT(*) FROM cde_tx_probe");
            assert_eq!(integer_cell(state, tab), 1, "COMMIT kept the insert");

            live_run(state, tab, "DROP TABLE cde_tx_probe");
        });
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_begin_commit_and_rollback_track_the_flag() {
        with_isolated_state("mysql-tx-live", |state| {
            let conn = add_mysql_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live MySQL"
            );
            let tab = state.new_tab(conn);
            live_run(state, tab, "DROP TABLE IF EXISTS cde_tx_probe");
            live_run(state, tab, "CREATE TABLE cde_tx_probe (id INT PRIMARY KEY)");

            live_transaction(state, tab, TransactionAction::Begin);
            assert!(transaction_open(state, conn), "BEGIN Ok opens the flag");
            live_run(state, tab, "INSERT INTO cde_tx_probe VALUES (1)");
            live_transaction(state, tab, TransactionAction::Rollback);
            assert!(!transaction_open(state, conn), "ROLLBACK Ok closes the flag");
            live_run(state, tab, "SELECT COUNT(*) FROM cde_tx_probe");
            assert_eq!(integer_cell(state, tab), 0, "ROLLBACK dropped the insert");

            live_transaction(state, tab, TransactionAction::Begin);
            assert!(transaction_open(state, conn), "BEGIN Ok opens the flag");
            live_run(state, tab, "INSERT INTO cde_tx_probe VALUES (2)");
            live_transaction(state, tab, TransactionAction::Commit);
            assert!(!transaction_open(state, conn), "COMMIT Ok closes the flag");
            live_run(state, tab, "SELECT COUNT(*) FROM cde_tx_probe");
            assert_eq!(integer_cell(state, tab), 1, "COMMIT kept the insert");

            live_run(state, tab, "DROP TABLE cde_tx_probe");
        });
    }

    #[test]
    fn autocommit_toggle_routes_one_command_and_moves_on_the_landing() {
        with_isolated_state("ac-toggle", |state| {
            let conn = add_postgres_connection(state, "pg-ac", "");
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);
            assert!(autocommit_on(state, conn), "defaults on");

            state.set_autocommit(tab, false);
            assert!(autocommit_on(state, conn), "the observed mode moves only on the landing");
            match commands.try_recv().expect("a command was sent") {
                DbCommand::SetAutocommit { on } => assert!(!on, "asks for manual mode"),
                other => panic!("expected SetAutocommit, got {other:?}"),
            }

            state.apply(DbEvent::AutocommitSet { conn, on: false, tx_open: false, error: None });
            assert!(!autocommit_on(state, conn), "the landing moves it");
        });
    }

    #[test]
    fn transaction_row_shows_the_controls_each_mode_needs() {
        with_isolated_state("tx-row", |state| {
            let conn = add_postgres_connection(state, "pg-row", "");
            let _commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);
            state.activate(tab);
            let ctx = eframe::egui::Context::default();
            let input = eframe::egui::RawInput {
                screen_rect: Some(eframe::egui::Rect::from_min_size(
                    eframe::egui::Pos2::ZERO,
                    eframe::egui::vec2(1600.0, 800.0),
                )),
                ..Default::default()
            };
            let row = |state: &mut AppState| {
                let mut output = ctx.run_ui(input.clone(), |ui| {
                    crate::ui::query::editor(state, ui);
                });
                output.textures_delta.clear();
                let text = rendered_text(&output);
                ["Transaction", "Commit", "Rollback", "Autocommit"]
                    .into_iter()
                    .filter(|label| text.iter().any(|t| t == label))
                    .collect::<Vec<_>>()
            };

            assert_eq!(row(state), vec!["Transaction"]);

            state.apply(DbEvent::AutocommitSet { conn, on: false, tx_open: false, error: None });
            assert_eq!(row(state), vec!["Autocommit"]);

            state.apply(DbEvent::TxState { conn, open: true });
            assert_eq!(row(state), vec!["Commit", "Rollback", "Autocommit"]);

            state.apply(DbEvent::AutocommitSet { conn, on: true, tx_open: false, error: None });
            state.apply(DbEvent::TxState { conn, open: true });
            assert_eq!(row(state), vec!["Commit", "Rollback"]);
        });
    }

    #[test]
    fn sqlite_has_no_autocommit_toggle() {
        with_isolated_state("ac-sqlite", |state| {
            let conn = add_sqlite_connection(state, "mem-ac");
            let commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);
            state.set_autocommit(tab, false);
            assert!(commands.try_recv().is_err(), "nothing reaches a SQLite worker");
            assert!(autocommit_on(state, conn), "SQLite stays on");
        });
    }

    #[test]
    fn tx_state_landings_keep_the_observed_flag_in_sync() {
        with_isolated_state("ac-txstate", |state| {
            let conn = add_postgres_connection(state, "pg-ac", "");
            let _commands = attach_test_session(state, conn);
            let _tab = state.new_tab(conn);
            assert!(!transaction_open(state, conn));

            state.apply(DbEvent::TxState { conn, open: true });
            assert!(transaction_open(state, conn), "the implicit BEGIN reports open");
            state.apply(DbEvent::TxState { conn, open: false });
            assert!(!transaction_open(state, conn), "the end of the transaction reports closed");
        });
    }

    #[test]
    fn idle_events_surface_the_notice_and_close_the_transaction() {
        with_isolated_state("ac-idle", |state| {
            let conn = add_postgres_connection(state, "pg-ac", "");
            let _commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);
            state.apply(DbEvent::TxState { conn, open: true });

            state.apply(DbEvent::TxIdleWarning { conn });
            assert_eq!(
                state.tab(tab).expect("tab").export_notice(),
                Some("Open transaction idle for 9 minutes; it rolls back at 10")
            );

            state.apply(DbEvent::TxIdleRollback { conn });
            assert!(!transaction_open(state, conn), "the idle rollback closes the transaction");
            assert_eq!(
                state.tab(tab).expect("tab").export_notice(),
                Some("Rolled back the idle transaction after 10 minutes")
            );
        });
    }

    #[test]
    fn failed_rollback_on_the_way_to_autocommit_errors_the_connection() {
        with_isolated_state("ac-error", |state| {
            let conn = add_postgres_connection(state, "pg-ac", "");
            let _commands = attach_test_session(state, conn);
            let _tab = state.new_tab(conn);
            state.apply(DbEvent::TxState { conn, open: true });

            state.apply(DbEvent::AutocommitSet {
                conn,
                on: true,
                tx_open: false,
                error: Some(crate::db::DbError::Query("rollback failed".to_owned())),
            });
            assert!(matches!(state.connections()[0].state(), ConnectionState::Error { .. }));
            assert!(!transaction_open(state, conn));
        });
    }

    #[test]
    fn a_toggle_landing_after_disconnect_does_not_move_the_observed_mode() {
        with_isolated_state("ac-late", |state| {
            let conn = add_postgres_connection(state, "pg-ac", "");
            let _commands = attach_test_session(state, conn);
            let tab = state.new_tab(conn);
            state.set_autocommit(tab, false);
            state.disconnect(conn);
            state.apply(DbEvent::AutocommitSet { conn, on: false, tx_open: true, error: None });
            assert!(autocommit_on(state, conn), "a dead connection keeps its observed mode");
            assert!(!transaction_open(state, conn));
        });
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_manual_mode_rollback_discards_and_commit_keeps() {
        with_isolated_state("pg-manual-live", |state| {
            let conn = add_postgres_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live Postgres"
            );
            let tab = state.new_tab(conn);
            live_run(state, tab, "DROP TABLE IF EXISTS cde_manual_probe");
            live_run(state, tab, "CREATE TABLE cde_manual_probe (id INT PRIMARY KEY)");

            live_autocommit(state, tab, conn, false);
            live_run(state, tab, "INSERT INTO cde_manual_probe VALUES (1)");
            live_run(state, tab, "INSERT INTO cde_manual_probe VALUES (2)");
            assert!(transaction_open(state, conn), "the first statement opened the transaction");
            live_transaction(state, tab, TransactionAction::Rollback);
            assert!(!transaction_open(state, conn), "ROLLBACK closed the transaction");
            live_run(state, tab, "SELECT COUNT(*) FROM cde_manual_probe");
            assert_eq!(integer_cell(state, tab), 0, "ROLLBACK discarded both inserts");

            live_run(state, tab, "INSERT INTO cde_manual_probe VALUES (3)");
            live_transaction(state, tab, TransactionAction::Commit);
            assert!(!transaction_open(state, conn), "COMMIT closed the transaction");
            live_run(state, tab, "SELECT COUNT(*) FROM cde_manual_probe");
            assert_eq!(integer_cell(state, tab), 1, "COMMIT kept the insert");

            live_autocommit(state, tab, conn, true);
            live_run(state, tab, "DROP TABLE cde_manual_probe");
        });
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_manual_mode_rollback_discards_and_commit_keeps() {
        with_isolated_state("mysql-manual-live", |state| {
            let conn = add_mysql_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live MySQL"
            );
            let tab = state.new_tab(conn);
            live_run(state, tab, "DROP TABLE IF EXISTS cde_manual_probe");
            live_run(state, tab, "CREATE TABLE cde_manual_probe (id INT PRIMARY KEY)");

            live_autocommit(state, tab, conn, false);
            live_run(state, tab, "INSERT INTO cde_manual_probe VALUES (1)");
            live_run(state, tab, "INSERT INTO cde_manual_probe VALUES (2)");
            assert!(transaction_open(state, conn), "the first statement opened the transaction");
            live_transaction(state, tab, TransactionAction::Rollback);
            assert!(!transaction_open(state, conn), "ROLLBACK closed the transaction");
            live_run(state, tab, "SELECT COUNT(*) FROM cde_manual_probe");
            assert_eq!(integer_cell(state, tab), 0, "ROLLBACK discarded both inserts");

            live_run(state, tab, "INSERT INTO cde_manual_probe VALUES (3)");
            live_transaction(state, tab, TransactionAction::Commit);
            assert!(!transaction_open(state, conn), "COMMIT closed the transaction");
            live_run(state, tab, "SELECT COUNT(*) FROM cde_manual_probe");
            assert_eq!(integer_cell(state, tab), 1, "COMMIT kept the insert");

            live_autocommit(state, tab, conn, true);
            live_run(state, tab, "DROP TABLE cde_manual_probe");
        });
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_cancel_keeps_the_manual_transaction_open() {
        with_isolated_state("pg-cancel-live", |state| {
            let conn = add_postgres_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live Postgres"
            );
            let tab = state.new_tab(conn);
            live_run(state, tab, "DROP TABLE IF EXISTS cde_cancel_probe");
            live_run(state, tab, "CREATE TABLE cde_cancel_probe (id INT PRIMARY KEY)");

            live_autocommit(state, tab, conn, false);
            live_run(state, tab, "INSERT INTO cde_cancel_probe VALUES (1)");
            assert!(transaction_open(state, conn), "the insert is inside the transaction");

            state.start_run(tab, "SELECT pg_sleep(30)", 0, None);
            std::thread::sleep(std::time::Duration::from_millis(300));
            state.cancel_run(tab);
            assert!(transaction_open(state, conn), "cancel keeps the transaction open");

            // Postgres aborts a transaction whose statement was cancelled, so
            // the follow-up error itself proves the session never left it.
            state.start_run(tab, "SELECT 1", 0, None);
            assert!(
                pump_until(state, |s| {
                    !matches!(s.tab(tab).map(|t| t.outcome()), Some(RunOutcome::Running { .. }))
                }),
                "the follow-up lands"
            );
            match state.tab(tab).expect("tab").outcome() {
                RunOutcome::Err(message) => {
                    assert!(
                        message.contains("aborted"),
                        "the follow-up ran inside the aborted transaction: {message}"
                    );
                }
                other => panic!("expected the aborted-transaction error, got {other:?}"),
            }
            assert!(transaction_open(state, conn), "the aborted transaction is still open");

            live_transaction(state, tab, TransactionAction::Rollback);
            live_run(state, tab, "SELECT COUNT(*) FROM cde_cancel_probe");
            assert_eq!(integer_cell(state, tab), 0, "ROLLBACK discarded the pre-cancel insert");

            live_autocommit(state, tab, conn, true);
            live_run(state, tab, "DROP TABLE cde_cancel_probe");
        });
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_cancel_keeps_the_manual_transaction_open() {
        with_isolated_state("mysql-cancel-live", |state| {
            let conn = add_mysql_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live MySQL"
            );
            let tab = state.new_tab(conn);
            live_run(state, tab, "DROP TABLE IF EXISTS cde_cancel_probe");
            live_run(state, tab, "CREATE TABLE cde_cancel_probe (id INT PRIMARY KEY)");

            live_autocommit(state, tab, conn, false);
            live_run(state, tab, "INSERT INTO cde_cancel_probe VALUES (1)");
            assert!(transaction_open(state, conn), "the insert is inside the transaction");

            state.start_run(tab, "SELECT SLEEP(30)", 0, None);
            std::thread::sleep(std::time::Duration::from_millis(300));
            state.cancel_run(tab);
            assert!(transaction_open(state, conn), "cancel keeps the transaction open");

            // MySQL keeps the transaction usable after KILL QUERY, so a
            // follow-up insert lands inside it and the rollback discards both.
            live_run(state, tab, "INSERT INTO cde_cancel_probe VALUES (2)");
            live_transaction(state, tab, TransactionAction::Rollback);
            live_run(state, tab, "SELECT COUNT(*) FROM cde_cancel_probe");
            assert_eq!(integer_cell(state, tab), 0, "ROLLBACK discarded the pre- and post-cancel inserts");

            live_autocommit(state, tab, conn, true);
            live_run(state, tab, "DROP TABLE cde_cancel_probe");
        });
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_idle_rollback_fires_and_surfaces_the_notice() {
        with_isolated_state("pg-idle-live", |state| {
            state.tx_idle = TxIdle {
                warn: std::time::Duration::from_millis(150),
                rollback: std::time::Duration::from_millis(450),
            };
            let conn = add_postgres_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live Postgres"
            );
            let tab = state.new_tab(conn);
            live_run(state, tab, "DROP TABLE IF EXISTS cde_idle_probe");
            live_run(state, tab, "CREATE TABLE cde_idle_probe (id INT PRIMARY KEY)");

            live_autocommit(state, tab, conn, false);
            live_run(state, tab, "INSERT INTO cde_idle_probe VALUES (1)");
            assert!(transaction_open(state, conn), "the insert is inside the transaction");

            let warned = pump_until(state, |s| {
                s.tab(tab)
                    .and_then(|t| t.export_notice())
                    .is_some_and(|notice| notice.contains("idle for 9 minutes"))
            });
            assert!(warned, "the warning notice lands");
            let rolled = pump_until(state, |s| {
                !transaction_open(s, conn)
                    && s.tab(tab)
                        .and_then(|t| t.export_notice())
                        .is_some_and(|notice| notice.contains("Rolled back the idle transaction"))
            });
            assert!(rolled, "the idle rollback lands with its notice");

            live_run(state, tab, "SELECT COUNT(*) FROM cde_idle_probe");
            assert_eq!(integer_cell(state, tab), 0, "the idle rollback discarded the insert");
            live_transaction(state, tab, TransactionAction::Rollback);
            live_autocommit(state, tab, conn, true);
            live_run(state, tab, "DROP TABLE cde_idle_probe");
        });
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_idle_rollback_fires_and_surfaces_the_notice() {
        with_isolated_state("mysql-idle-live", |state| {
            state.tx_idle = TxIdle {
                warn: std::time::Duration::from_millis(150),
                rollback: std::time::Duration::from_millis(450),
            };
            let conn = add_mysql_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live MySQL"
            );
            let tab = state.new_tab(conn);
            live_run(state, tab, "DROP TABLE IF EXISTS cde_idle_probe");
            live_run(state, tab, "CREATE TABLE cde_idle_probe (id INT PRIMARY KEY)");

            live_autocommit(state, tab, conn, false);
            live_run(state, tab, "INSERT INTO cde_idle_probe VALUES (1)");
            assert!(transaction_open(state, conn), "the insert is inside the transaction");

            let warned = pump_until(state, |s| {
                s.tab(tab)
                    .and_then(|t| t.export_notice())
                    .is_some_and(|notice| notice.contains("idle for 9 minutes"))
            });
            assert!(warned, "the warning notice lands");
            let rolled = pump_until(state, |s| {
                !transaction_open(s, conn)
                    && s.tab(tab)
                        .and_then(|t| t.export_notice())
                        .is_some_and(|notice| notice.contains("Rolled back the idle transaction"))
            });
            assert!(rolled, "the idle rollback lands with its notice");

            live_run(state, tab, "SELECT COUNT(*) FROM cde_idle_probe");
            assert_eq!(integer_cell(state, tab), 0, "the idle rollback discarded the insert");
            live_transaction(state, tab, TransactionAction::Rollback);
            live_autocommit(state, tab, conn, true);
            live_run(state, tab, "DROP TABLE cde_idle_probe");
        });
    }

    /// The first result cell as text.
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    fn text_cell(state: &AppState, tab: TabId) -> String {
        match state.tab(tab).and_then(|t| t.active_query_result()).and_then(|r| r.cell(0, 0)) {
            Some(Value::Text(text)) => text.clone(),
            other => panic!("expected a text cell, got {other:?}"),
        }
    }

    /// True once the loaded schema carries both join scratch tables.
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    fn has_join_tables(state: &AppState, conn: ConnectionId) -> bool {
        state.schema(conn).is_some_and(|schema| {
            let mut users = false;
            let mut orders = false;
            for database in schema.databases() {
                for node in database.schemas() {
                    for table in node.tables() {
                        if table.name().eq_ignore_ascii_case("cde_join_users") {
                            users = true;
                        }
                        if table.name().eq_ignore_ascii_case("cde_join_orders") {
                            orders = true;
                        }
                    }
                }
            }
            users && orders
        })
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_join_edit_writes_each_source_table() {
        with_isolated_state("pg-join-live", |state| {
            let conn = add_postgres_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live Postgres"
            );
            let tab = state.new_tab(conn);
            live_run(state, tab, "DROP TABLE IF EXISTS cde_join_orders");
            live_run(state, tab, "DROP TABLE IF EXISTS cde_join_users");
            live_run(state, tab, "CREATE TABLE cde_join_users (id INT PRIMARY KEY, name TEXT)");
            live_run(
                state,
                tab,
                "CREATE TABLE cde_join_orders (id INT PRIMARY KEY, user_id INT, total INT)",
            );
            live_run(
                state,
                tab,
                "INSERT INTO cde_join_users (id, name) VALUES (1, 'ada'), (2, 'grace')",
            );
            live_run(
                state,
                tab,
                "INSERT INTO cde_join_orders (id, user_id, total) VALUES (10, 1, 100), (20, 2, 200)",
            );
            state.refresh_schema(conn);
            assert!(
                pump_until(state, |s| has_join_tables(s, conn)),
                "the schema loads the join tables"
            );
            let join_sql = "SELECT u.id, u.name, o.id AS oid, o.total FROM cde_join_users u \
                            JOIN cde_join_orders o ON o.user_id = u.id ORDER BY u.id";
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = join_sql.to_owned();
            }
            live_run(state, tab, join_sql);
            let target = state.edit_target(tab).expect("the join resolves");
            assert!(target.is_join(), "aliased two-table SELECT edits");
            state.stage_edit(tab, 0, 1, "Ada".to_owned());
            state.stage_edit(tab, 1, 3, "999".to_owned());
            assert_eq!(
                state.preview_edit_statements(tab),
                Some(vec![
                    "UPDATE \"cde_join_users\" SET \"name\" = 'Ada' WHERE \"id\" = 1".to_owned(),
                    "UPDATE \"cde_join_orders\" SET \"total\" = 999 WHERE \"id\" = 20".to_owned(),
                ])
            );
            state.apply_edits(tab);
            state.confirm_pending_edits();
            assert!(
                pump_until(state, |s| {
                    !matches!(s.tab(tab).map(|t| t.outcome()), Some(RunOutcome::Running { .. }))
                }),
                "the join apply and refresh land"
            );
            assert_eq!(state.tab(tab).expect("tab").staged_count(), 0);
            live_run(state, tab, "SELECT name FROM cde_join_users WHERE id = 1");
            assert_eq!(text_cell(state, tab), "Ada");
            live_run(state, tab, "SELECT total FROM cde_join_orders WHERE id = 20");
            assert_eq!(integer_cell(state, tab), 999);
            live_run(state, tab, "DROP TABLE cde_join_orders");
            live_run(state, tab, "DROP TABLE cde_join_users");
        });
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_join_mapping_matches_postgres_without_applying() {
        with_isolated_state("mysql-join-live", |state| {
            let conn = add_mysql_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live MySQL"
            );
            let tab = state.new_tab(conn);
            live_run(state, tab, "DROP TABLE IF EXISTS cde_join_orders");
            live_run(state, tab, "DROP TABLE IF EXISTS cde_join_users");
            live_run(state, tab, "CREATE TABLE cde_join_users (id INT PRIMARY KEY, name TEXT)");
            live_run(
                state,
                tab,
                "CREATE TABLE cde_join_orders (id INT PRIMARY KEY, user_id INT, total INT)",
            );
            live_run(
                state,
                tab,
                "INSERT INTO cde_join_users (id, name) VALUES (1, 'ada'), (2, 'grace')",
            );
            live_run(
                state,
                tab,
                "INSERT INTO cde_join_orders (id, user_id, total) VALUES (10, 1, 100), (20, 2, 200)",
            );
            state.refresh_schema(conn);
            assert!(
                pump_until(state, |s| has_join_tables(s, conn)),
                "the schema loads the join tables"
            );
            let join_sql = "SELECT u.id, u.name, o.id AS oid, o.total FROM cde_join_users u \
                            JOIN cde_join_orders o ON o.user_id = u.id ORDER BY u.id";
            if let Some(slot) = state.tab_sql_mut(tab) {
                *slot = join_sql.to_owned();
            }
            live_run(state, tab, join_sql);
            let target = state.edit_target(tab).expect("the join resolves");
            assert!(target.is_join(), "aliased two-table SELECT edits");
            state.stage_edit(tab, 0, 1, "Ada".to_owned());
            state.stage_edit(tab, 1, 3, "999".to_owned());
            assert_eq!(
                state.preview_edit_statements(tab),
                Some(vec![
                    "UPDATE \"cde_join_users\" SET \"name\" = 'Ada' WHERE \"id\" = 1".to_owned(),
                    "UPDATE \"cde_join_orders\" SET \"total\" = 999 WHERE \"id\" = 20".to_owned(),
                ]),
                "MySQL maps the same UPDATEs as Postgres; the apply stays Postgres-only \
                 since MySQL rejects double-quoted identifiers"
            );
            live_run(state, tab, "DROP TABLE cde_join_orders");
            live_run(state, tab, "DROP TABLE cde_join_users");
        });
    }

    #[test]
    fn bookmark_toggle_adds_and_removes() {
        with_isolated_state("bookmark-toggle", |state| {
            let conn = add_sqlite_connection(state, "mem");
            assert!(state.bookmarks().is_empty());
            assert!(!state.is_bookmarked(conn, "main", "users"));
            assert!(state.toggle_bookmark(conn, "main", "users"));
            assert!(state.is_bookmarked(conn, "main", "users"));
            assert_eq!(state.bookmarks().len(), 1);
            assert_eq!(state.bookmarks()[0].schema, "main");
            assert_eq!(state.bookmarks()[0].table, "users");
            assert!(!state.toggle_bookmark(conn, "main", "users"));
            assert!(state.bookmarks().is_empty());
            assert!(!state.is_bookmarked(conn, "main", "users"));
            assert!(!state.toggle_bookmark(conn, "", "users"));
            assert!(!state.toggle_bookmark(conn, "main", "  "));
            assert!(state.bookmarks().is_empty());
        });
    }

    #[test]
    fn bookmark_save_and_reload_roundtrip() {
        with_isolated_state("bookmark-persist", |state| {
            assert!(state.bookmarks().is_empty());
            let conn = add_sqlite_connection(state, "mem");
            assert!(state.toggle_bookmark(conn, "main", "users"));
            assert!(state.toggle_bookmark(conn, "main", "orders"));
            assert_eq!(state.bookmarks().len(), 2);
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.bookmarks().len(), 2);
            assert_eq!(reloaded.bookmarks()[0].table, "orders");
            assert_eq!(reloaded.bookmarks()[1].table, "users");
            assert!(reloaded.is_bookmarked(conn, "main", "orders"));
        });
    }

    #[test]
    fn bookmark_delete_connection_prunes_entries() {
        with_isolated_state("bookmark-prune", |state| {
            let first = add_sqlite_connection(state, "one");
            let second = add_sqlite_connection(state, "two");
            assert!(state.toggle_bookmark(first, "main", "users"));
            assert!(state.toggle_bookmark(second, "main", "orders"));
            state.delete_connection(first);
            assert_eq!(state.bookmarks().len(), 1);
            assert_eq!(state.bookmarks()[0].conn, second);
            let reloaded = AppState::new(&eframe::egui::Context::default());
            assert_eq!(reloaded.bookmarks().len(), 1);
            assert_eq!(reloaded.bookmarks()[0].table, "orders");
        });
    }

    #[test]
    fn old_bookmark_file_without_fields_loads_empty() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("cde-bookmark-old-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crusty-data-explorer")).unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        std::fs::write(
            dir.join("crusty-data-explorer").join("bookmarks.json"),
            br#"[{"conn":1},{"conn":1,"schema":"main"},{"conn":2,"schema":"main","table":"users"}]"#,
        )
        .unwrap();
        let state = AppState::new(&eframe::egui::Context::default());
        assert_eq!(state.bookmarks().len(), 1);
        assert_eq!(state.bookmarks()[0].conn, ConnectionId::from_raw(2));
        assert_eq!(state.bookmarks()[0].schema, "main");
        assert_eq!(state.bookmarks()[0].table, "users");
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn navigator_filter_narrows_table_names() {
        assert!(table_matches_navigator_filter("users", ""));
        assert!(table_matches_navigator_filter("users", "   "));
        assert!(table_matches_navigator_filter("users", "user"));
        assert!(table_matches_navigator_filter("users", "USERS"));
        assert!(table_matches_navigator_filter("order_items", "Order"));
        assert!(table_matches_navigator_filter("users", "  use  "));
        assert!(!table_matches_navigator_filter("users", "order"));
        assert!(!table_matches_navigator_filter("users", "xyz"));
        with_isolated_state("nav-filter", |state| {
            assert_eq!(state.navigator_filter(), "");
            state.set_navigator_filter("use".to_owned());
            assert_eq!(state.navigator_filter(), "use");
            assert!(table_matches_navigator_filter("users", state.navigator_filter()));
            assert!(!table_matches_navigator_filter("orders", state.navigator_filter()));
        });
    }

    fn compare_result(rows: Vec<(i64, &str)>) -> QueryResult {
        QueryResult::from_columns(
            vec![
                crate::db::Column::new(
                    "id".to_owned(),
                    "INTEGER".to_owned(),
                    rows.iter().map(|(id, _)| Value::Integer(*id)).collect(),
                ),
                crate::db::Column::new(
                    "name".to_owned(),
                    "TEXT".to_owned(),
                    rows.iter().map(|(_, name)| Value::Text((*name).to_owned())).collect(),
                ),
            ],
            false,
            None,
            std::time::Duration::from_millis(1),
        )
        .unwrap()
    }

    #[test]
    fn compare_sends_two_read_only_batches() {
        with_isolated_state("compare-send", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let commands = attach_test_session(state, conn);
            let _tab = state.new_tab(conn);
            state.set_compare_left("main", "users");
            state.set_compare_right("main", "orders");
            state.set_compare_keys("id".to_owned());
            state.start_compare();
            assert!(state.compare().running(), "both reads are in flight");
            assert!(state.compare().error().is_none());
            let mut statements = Vec::new();
            for _ in 0..2 {
                match commands.try_recv().expect("a read was sent") {
                    DbCommand::QueryBatch { statements: sent, .. } => statements.push(sent),
                    other => panic!("expected QueryBatch, got {other:?}"),
                }
            }
            assert_eq!(
                statements,
                vec![
                    vec!["SELECT * FROM \"main\".\"users\"".to_owned()],
                    vec!["SELECT * FROM \"main\".\"orders\"".to_owned()],
                ]
            );
        });
    }

    #[test]
    fn compare_reads_pass_the_read_only_guard() {
        with_isolated_state("compare-ro", |state| {
            let conn = add_sqlite_connection(state, "mem-ro");
            state
                .connections
                .iter_mut()
                .find(|c| c.id == conn)
                .expect("connection")
                .config
                .set_read_only(true);
            let commands = attach_test_session(state, conn);
            let _tab = state.new_tab(conn);
            state.set_compare_left("main", "users");
            state.set_compare_right("main", "orders");
            state.set_compare_keys("id".to_owned());
            state.start_compare();
            assert!(state.compare().error().is_none(), "reads are allowed on read-only");
            assert!(commands.try_recv().is_ok(), "the left read was sent");
            assert!(commands.try_recv().is_ok(), "the right read was sent");
        });
    }

    #[test]
    fn compare_needs_picks_and_keys() {
        with_isolated_state("compare-blank", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let commands = attach_test_session(state, conn);
            let _tab = state.new_tab(conn);
            state.set_compare_keys("id".to_owned());
            state.start_compare();
            assert_eq!(state.compare().error(), Some("pick two tables to compare"));
            assert!(commands.try_recv().is_err(), "nothing was sent");
            state.set_compare_left("main", "users");
            state.set_compare_right("main", "orders");
            state.set_compare_keys(String::new());
            state.start_compare();
            assert_eq!(state.compare().error(), Some("key columns are empty"));
            assert!(commands.try_recv().is_err(), "nothing was sent");
        });
    }

    #[test]
    fn compare_landings_diff_into_counts() {
        use crate::ui::compare::DiffKind;

        with_isolated_state("compare-land", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let commands = attach_test_session(state, conn);
            let _tab = state.new_tab(conn);
            state.set_compare_left("main", "a");
            state.set_compare_right("main", "b");
            state.set_compare_keys("id".to_owned());
            state.start_compare();
            let left_run = state.compare.left_run.expect("left run");
            let right_run = state.compare.right_run.expect("right run");
            assert!(commands.try_recv().is_ok());
            assert!(commands.try_recv().is_ok());
            state.apply(DbEvent::BatchOk {
                run: left_run,
                results: vec![compare_result(vec![(1, "ada"), (2, "grace")])],
            });
            assert!(state.compare().output().is_none(), "one side alone diffs nothing");
            assert!(state.compare().running(), "the right read is still in flight");
            state.apply(DbEvent::BatchOk {
                run: right_run,
                results: vec![compare_result(vec![(1, "ada"), (2, "GRACE"), (3, "lin")])],
            });
            assert!(!state.compare().running());
            let output = state.compare().output().expect("both sides diff");
            assert_eq!(output.counts.added, 1);
            assert_eq!(output.counts.removed, 0);
            assert_eq!(output.counts.changed, 1);
            assert_eq!(output.counts.unchanged, 1);
            let changed =
                output.rows.iter().find(|row| row.kind == DiffKind::Changed).expect("a change");
            assert_eq!(changed.key, "2");
            assert_eq!(changed.changed, vec!["name".to_owned()]);
            assert_eq!(changed.left, vec!["2".to_owned(), "grace".to_owned()]);
            assert_eq!(changed.right, vec!["2".to_owned(), "GRACE".to_owned()]);
        });
    }

    #[test]
    fn compare_error_landing_surfaces_the_message() {
        with_isolated_state("compare-err", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let _commands = attach_test_session(state, conn);
            let _tab = state.new_tab(conn);
            state.set_compare_left("main", "a");
            state.set_compare_right("main", "b");
            state.set_compare_keys("id".to_owned());
            state.start_compare();
            let left_run = state.compare.left_run.expect("left run");
            state.apply(DbEvent::QueryErr {
                run: left_run,
                error: crate::db::DbError::Query("boom".to_owned()),
            });
            assert_eq!(state.compare().error(), Some("query failed: boom"));
            assert!(state.compare().output().is_none());
        });
    }

    #[test]
    fn compare_key_edits_recompute_and_picks_clear() {
        with_isolated_state("compare-rekey", |state| {
            let conn = add_sqlite_connection(state, "mem");
            let _commands = attach_test_session(state, conn);
            let _tab = state.new_tab(conn);
            state.set_compare_left("main", "a");
            state.set_compare_right("main", "b");
            state.set_compare_keys("id".to_owned());
            let left_run = state.ids.run();
            let right_run = state.ids.run();
            state.compare.left_run = Some(left_run);
            state.compare.right_run = Some(right_run);
            state.apply(DbEvent::BatchOk {
                run: left_run,
                results: vec![compare_result(vec![(1, "ada")])],
            });
            state.apply(DbEvent::BatchOk {
                run: right_run,
                results: vec![compare_result(vec![(1, "ada")])],
            });
            assert!(state.compare().output().is_some());
            state.set_compare_keys("nope".to_owned());
            assert_eq!(
                state.compare().error(),
                Some("left result has no key column \"nope\"")
            );
            assert!(state.compare().output().is_none());
            state.set_compare_keys("id".to_owned());
            assert!(state.compare().error().is_none());
            assert!(state.compare().output().is_some());
            state.set_compare_left("main", "c");
            assert!(state.compare().left_result().is_none(), "re-picks drop stale rows");
            assert!(state.compare().output().is_none());
            assert!(state.compare().error().is_none());
        });
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_compare_scratch_tables_surfaces_the_planted_difference() {
        use crate::ui::compare::DiffKind;

        with_isolated_state("pg-compare-live", |state| {
            let conn = add_postgres_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live Postgres"
            );
            let tab = state.new_tab(conn);
            for sql in [
                "DROP TABLE IF EXISTS cde_cmp_a",
                "DROP TABLE IF EXISTS cde_cmp_b",
                "CREATE TABLE cde_cmp_a (id INT PRIMARY KEY, name TEXT NOT NULL)",
                "CREATE TABLE cde_cmp_b (id INT PRIMARY KEY, name TEXT NOT NULL)",
                "INSERT INTO cde_cmp_a VALUES (1, 'ada'), (2, 'grace')",
                "INSERT INTO cde_cmp_b VALUES (1, 'ada'), (2, 'GRACE'), (3, 'lin')",
            ] {
                live_run(state, tab, sql);
            }
            state.set_compare_left("public", "cde_cmp_a");
            state.set_compare_right("public", "cde_cmp_b");
            state.set_compare_keys("id".to_owned());
            state.start_compare();
            assert!(
                pump_until(state, |s| {
                    s.compare().output().is_some() || s.compare().error().is_some()
                }),
                "the compare lands"
            );
            if let Some(error) = state.compare().error() {
                panic!("compare failed: {error}");
            }
            let output = state.compare().output().expect("a diff");
            assert_eq!(output.counts.added, 1, "row 3 is right only");
            assert_eq!(output.counts.removed, 0);
            assert_eq!(output.counts.changed, 1, "row 2 differs");
            assert_eq!(output.counts.unchanged, 1, "row 1 matches");
            let changed = output
                .rows
                .iter()
                .find(|row| row.kind == DiffKind::Changed)
                .expect("the planted change");
            assert_eq!(changed.key, "2");
            assert_eq!(changed.changed, vec!["name".to_owned()]);
            assert_eq!(changed.left, vec!["2".to_owned(), "grace".to_owned()]);
            assert_eq!(changed.right, vec!["2".to_owned(), "GRACE".to_owned()]);
            live_run(state, tab, "DROP TABLE cde_cmp_a");
            live_run(state, tab, "DROP TABLE cde_cmp_b");
        });
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_compare_scratch_tables_surface_the_planted_difference() {
        use crate::ui::compare::DiffKind;

        with_isolated_state("mysql-compare-live", |state| {
            let conn = add_mysql_probe_connection(state);
            state.connect(conn);
            assert!(
                pump_until(state, |s| {
                    s.connections().iter().any(|c| {
                        c.id == conn && matches!(c.state(), ConnectionState::Connected { .. })
                    })
                }),
                "connects to live MySQL"
            );
            let schema =
                std::env::var("MYSQLDATABASE").unwrap_or_else(|_| "mysql".to_owned());
            let tab = state.new_tab(conn);
            for sql in [
                "DROP TABLE IF EXISTS cde_cmp_a",
                "DROP TABLE IF EXISTS cde_cmp_b",
                "CREATE TABLE cde_cmp_a (id INT PRIMARY KEY, name TEXT NOT NULL)",
                "CREATE TABLE cde_cmp_b (id INT PRIMARY KEY, name TEXT NOT NULL)",
                "INSERT INTO cde_cmp_a VALUES (1, 'ada'), (2, 'grace')",
                "INSERT INTO cde_cmp_b VALUES (1, 'ada'), (2, 'GRACE'), (3, 'lin')",
            ] {
                live_run(state, tab, sql);
            }
            state.set_compare_left(&schema, "cde_cmp_a");
            state.set_compare_right(&schema, "cde_cmp_b");
            state.set_compare_keys("id".to_owned());
            state.start_compare();
            assert!(
                pump_until(state, |s| {
                    s.compare().output().is_some() || s.compare().error().is_some()
                }),
                "the compare lands"
            );
            if let Some(error) = state.compare().error() {
                panic!("compare failed: {error}");
            }
            let output = state.compare().output().expect("a diff");
            assert_eq!(output.counts.added, 1, "row 3 is right only");
            assert_eq!(output.counts.removed, 0);
            assert_eq!(output.counts.changed, 1, "row 2 differs");
            assert_eq!(output.counts.unchanged, 1, "row 1 matches");
            let changed = output
                .rows
                .iter()
                .find(|row| row.kind == DiffKind::Changed)
                .expect("the planted change");
            assert_eq!(changed.key, "2");
            assert_eq!(changed.changed, vec!["name".to_owned()]);
            assert_eq!(changed.left, vec!["2".to_owned(), "grace".to_owned()]);
            assert_eq!(changed.right, vec!["2".to_owned(), "GRACE".to_owned()]);
            live_run(state, tab, "DROP TABLE cde_cmp_a");
            live_run(state, tab, "DROP TABLE cde_cmp_b");
        });
    }
}
