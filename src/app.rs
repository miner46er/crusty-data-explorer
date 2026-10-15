//! App state: the one mutation point for connection and tab lifecycle.
//!
//! Every field carrying an invariant is private to this module. Rendering
//! reads through accessors and changes state only through transitions.
//! Worker events fold into the per-connection [`ConnectionState`] machine in
//! exactly one place: [`AppState::apply`], drained from [`AppState::logic`].

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::time::Instant;

use crate::db::{
    CancelHandle, ConnectionConfig, ConnectionId, DbCommand, DbEvent, DbHandle, DbKind,
    DriverRegistry, Ids, QueryResult, RunId, SchemaInfo, TabId, spawn_session,
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

#[derive(Debug)]
pub struct Tab {
    pub id: TabId,
    pub conn: ConnectionId,
    pub sql: String,
    outcome: RunOutcome,
}

impl Tab {
    pub fn outcome(&self) -> &RunOutcome {
        &self.outcome
    }
}

/// One state per tab. `Running` carries the run id events must match.
#[derive(Debug)]
pub enum RunOutcome {
    Never,
    Running { run: RunId, started: Instant },
    Ok(QueryResult),
    Err(String),
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

/// Cap on retained history entries, newest first.
const HISTORY_CAP: usize = 200;
/// Cap on history rows rendered in the sidebar.
pub const HISTORY_SHOWN: usize = 30;

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug)]
pub struct ConnectDraft {
    pub editing: Option<ConnectionId>,
    pub name: String,
    pub config: ConnectionConfig,
    pub password: String,
    pub error: Option<String>,
}

pub struct AppState {
    connections: Vec<Connection>,
    tabs: Vec<Tab>,
    active: Option<TabId>,
    draft: Option<ConnectDraft>,
    ids: Ids,
    registry: DriverRegistry,
    events_tx: mpsc::Sender<DbEvent>,
    events_rx: mpsc::Receiver<DbEvent>,
    ui_ctx: eframe::egui::Context,
    schemas: HashMap<ConnectionId, SchemaInfo>,
    schema_loading: HashSet<ConnectionId>,
    schema_errors: HashMap<ConnectionId, String>,
    history: Vec<HistoryEntry>,
}

impl AppState {
    pub fn new(egui_ctx: &eframe::egui::Context) -> Self {
        let profiles = load_connections();
        let mut ids = Ids::new();
        ids.resume_after(&profiles.iter().map(|c| c.id).collect::<Vec<_>>());
        let (events_tx, events_rx) = mpsc::channel();
        Self {
            connections: profiles,
            tabs: Vec::new(),
            active: None,
            draft: None,
            ids,
            registry: DriverRegistry::bundled(),
            events_tx,
            events_rx,
            ui_ctx: egui_ctx.clone(),
            schemas: HashMap::new(),
            schema_loading: HashSet::new(),
            schema_errors: HashMap::new(),
            history: load_history(),
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
        self.tabs.push(Tab { id, conn, sql: String::new(), outcome: RunOutcome::Never });
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

    pub fn start_run(&mut self, tab: TabId, sql: &str) {
        let run = self.ids.run();
        let (conn_id, handle) = match self.tabs.iter().find(|t| t.id == tab) {
            Some(t) => match self.connections.iter().find(|c| c.id == t.conn) {
                Some(c) => (t.conn, c.handle()),
                None => {
                    self.set_outcome(tab, RunOutcome::Err("connection not found".to_owned()));
                    return;
                }
            },
            None => return,
        };
        match handle {
            Some(handle) => match handle.send(DbCommand::Query { run, sql: sql.to_owned() }) {
                Ok(()) => {
                    self.set_outcome(tab, RunOutcome::Running { run, started: Instant::now() });
                    if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn_id) {
                        if let ConnectionState::Connected { session } = &c.state {
                            let session = session.clone();
                            c.state = ConnectionState::QueryRunning { session, run, tab };
                        }
                    }
                }
                Err(error) => self.set_outcome(tab, RunOutcome::Err(error.to_string())),
            },
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

    pub fn connect(&mut self, id: ConnectionId) {
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

    /// Memory-only password update for a later prompt. Never persisted.
    pub fn set_password(&mut self, id: ConnectionId, password: String) {
        if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
            c.password = password;
        }
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
                self.start_run(id, &sql.clone());
            }
            None => {
                let id = self.new_tab(conn);
                if let Some(slot) = self.tab_sql_mut(id) {
                    *slot = sql.clone();
                }
                self.activate(id);
                self.start_run(id, &sql);
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
            self.start_run(id, sql);
        } else {
            self.set_outcome(id, RunOutcome::Never);
        }
    }

    pub fn open_draft(&mut self, editing: Option<ConnectionId>) {
        let (name, config, password) = match editing {
            Some(id) => match self.connections.iter().find(|c| c.id == id) {
                Some(c) => (c.name.clone(), c.config.clone(), c.password.clone()),
                None => return,
            },
            None => (String::new(), DbKind::Sqlite.default_config(), String::new()),
        };
        self.draft = Some(ConnectDraft { editing, name, config, password, error: None });
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
                    c.config = draft.config;
                    c.password = draft.password;
                }
            }
            None => {
                let id = self.ids.connection();
                self.connections.push(Connection {
                    id,
                    name: draft.name,
                    config: draft.config,
                    password: draft.password,
                    state: ConnectionState::Disconnected,
                });
            }
        }
        self.persist();
    }

    fn set_outcome(&mut self, tab: TabId, outcome: RunOutcome) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
            t.outcome = outcome;
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
                self.finish_run(run, RunOutcome::Ok(result));
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
    /// land here and are silently dropped.
    fn finish_run(&mut self, run: RunId, outcome: RunOutcome) {
        let matched = match self
            .tabs
            .iter_mut()
            .find(|t| matches!(&t.outcome, RunOutcome::Running { run: r, .. } if *r == run))
        {
            Some(t) => {
                let started = match &t.outcome {
                    RunOutcome::Running { started, .. } => *started,
                    _ => Instant::now(),
                };
                let sql = t.sql.clone();
                let record = match &outcome {
                    RunOutcome::Ok(result) => Some((
                        true,
                        Some(result.affected().unwrap_or(result.row_count() as u64)),
                        result.took(),
                    )),
                    RunOutcome::Err(_) => Some((false, None, started.elapsed())),
                    _ => None,
                };
                let conn = t.conn;
                t.outcome = outcome;
                Some((conn, record.map(|(succeeded, rows, took)| (sql, succeeded, rows, took))))
            }
            None => None,
        };
        let Some((conn, record)) = matched else { return };
        if let Some((sql, succeeded, rows, took)) = record {
            self.history.insert(
                0,
                HistoryEntry { conn, sql, succeeded, rows, took, at: now_unix_secs() },
            );
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

#[cfg(test)]
mod tests {
    use super::*;

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
        state.open_draft(None);
        {
            let draft = state.draft_mut().expect("new draft");
            draft.name = name.to_owned();
            draft.config = ConnectionConfig::Sqlite { path: ":memory:".into() };
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
        state.set_outcome(tab, RunOutcome::Running { run, started: Instant::now() });
        let result = QueryResult::from_columns(
            Vec::new(),
            false,
            None,
            std::time::Duration::from_millis(1),
        )
        .unwrap();
        state.finish_run(run, RunOutcome::Ok(result));
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
            state.set_outcome(tab, RunOutcome::Running { run, started: Instant::now() });
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
            state.finish_run(run, RunOutcome::Ok(result));
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
                RunOutcome::Running { run: affected_run, started: Instant::now() },
            );
            let affected = QueryResult::from_columns(
                Vec::new(),
                false,
                Some(3),
                std::time::Duration::from_millis(5),
            )
            .unwrap();
            state.finish_run(affected_run, RunOutcome::Ok(affected));
            assert_eq!(state.history()[0].rows, Some(3));

            let err_tab = state.new_tab(conn);
            if let Some(slot) = state.tab_sql_mut(err_tab) {
                *slot = "SELECT bad".to_owned();
            }
            let err_run = state.ids.run();
            state.set_outcome(
                err_tab,
                RunOutcome::Running { run: err_run, started: Instant::now() },
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
    fn persisted_profile_shape_has_no_password() {
        let profile = PersistedProfile {
            id: 1,
            name: "pg".to_owned(),
            config: ConnectionConfig::Postgres {
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
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
            config: ConnectionConfig::Postgres {
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
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
            }
        );
        assert_eq!(conn.password, "second");
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
