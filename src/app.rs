//! App state: the one mutation point for connection and tab lifecycle.
//!
//! Every field carrying an invariant is private to this module. Rendering
//! reads through accessors and changes state only through transitions.
//! Worker events fold into the per-connection [`ConnectionState`] machine in
//! exactly one place: [`AppState::apply`], drained from [`AppState::logic`].

use std::sync::mpsc;
use std::time::Instant;

use crate::db::{
    CancelHandle, ConnectionConfig, ConnectionId, DbCommand, DbEvent, DbHandle, DbKind,
    DriverRegistry, Ids, QueryResult, RunId, TabId, spawn_session,
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

#[derive(Debug)]
pub struct ConnectDraft {
    pub editing: Option<ConnectionId>,
    pub name: String,
    pub config: ConnectionConfig,
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
        }
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
        let config = match self.connections.iter().find(|c| c.id == id) {
            Some(c) => {
                if matches!(
                    c.state,
                    ConnectionState::Connecting { .. }
                        | ConnectionState::Connected { .. }
                        | ConnectionState::QueryRunning { .. }
                ) {
                    return;
                }
                c.config.clone()
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
            spawn_session(id, config, None, self.ui_ctx.clone(), self.events_tx.clone());
        if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
            c.state = ConnectionState::Connecting { handle };
        }
    }

    pub fn disconnect(&mut self, id: ConnectionId) {
        if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
            c.state = ConnectionState::Disconnected;
        }
    }

    pub fn delete_connection(&mut self, id: ConnectionId) {
        self.disconnect(id);
        self.tabs.retain(|t| t.conn != id);
        self.connections.retain(|c| c.id != id);
        self.repair_active();
        self.persist();
    }

    pub fn open_draft(&mut self, editing: Option<ConnectionId>) {
        let (name, config) = match editing {
            Some(id) => match self.connections.iter().find(|c| c.id == id) {
                Some(c) => (c.name.clone(), c.config.clone()),
                None => return,
            },
            None => (String::new(), DbKind::Sqlite.default_config()),
        };
        self.draft = Some(ConnectDraft { editing, name, config, error: None });
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
                }
            }
            None => {
                let id = self.ids.connection();
                self.connections.push(Connection {
                    id,
                    name: draft.name,
                    config: draft.config,
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
                if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn) {
                    if let ConnectionState::Connecting { handle } = &c.state {
                        let session = LiveSession {
                            handle: handle.clone(),
                            cancel,
                            server_version,
                        };
                        c.state = ConnectionState::Connected { session };
                    }
                }
            }
            DbEvent::ConnectFailed { conn, error } => {
                if let Some(c) = self.connections.iter_mut().find(|c| c.id == conn) {
                    if matches!(c.state, ConnectionState::Connecting { .. }) {
                        c.state = ConnectionState::Error { message: error.to_string() };
                    }
                }
            }
            DbEvent::QueryOk { run, result } => {
                self.finish_run(run, RunOutcome::Ok(result));
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
        let conn_id = match self
            .tabs
            .iter_mut()
            .find(|t| matches!(&t.outcome, RunOutcome::Running { run: r, .. } if *r == run))
        {
            Some(t) => {
                t.outcome = outcome;
                Some(t.conn)
            }
            None => None,
        };
        if let Some(id) = conn_id {
            let still_running = self
                .tabs
                .iter()
                .any(|t| t.conn == id && matches!(t.outcome, RunOutcome::Running { .. }));
            if !still_running {
                if let Some(c) = self.connections.iter_mut().find(|c| c.id == id) {
                    if let ConnectionState::QueryRunning { session, .. } = &c.state {
                        let session = session.clone();
                        c.state = ConnectionState::Connected { session };
                    }
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
