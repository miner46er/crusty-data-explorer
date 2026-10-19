//! Keyboard behavior of the results grid over a rendered frame.
//!
//! Each test builds a real `AppState` on an in-memory SQLite database, draws
//! it through `ui::root` with `egui_kittest`, and asserts what the operator
//! could see: the text nodes of the row-detail pane, the cell editor, or the
//! text a frame asked the clipboard to carry.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use eframe::egui;
use egui_kittest::Harness;

use crusty_data_explorer::app::{AppState, ConnectionState, Environment, RunOutcome};
use crusty_data_explorer::db::{ConnectionConfig, ConnectionId, TabId};
use crusty_data_explorer::ui;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Serialize the tests that mutate `XDG_CONFIG_HOME`, tolerating a mutex a
/// panicking test left poisoned: the scratch dir is restored on drop.
fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Points `XDG_CONFIG_HOME` at a scratch dir for the guard's lifetime.
struct Scratch {
    dir: std::path::PathBuf,
    prev: Option<OsString>,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cde-grid-keys-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("crusty-data-explorer")).unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        Self { dir, prev }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn build() -> Harness<'static, AppState> {
    Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .build_eframe(|cc| {
            ui::install(&cc.egui_ctx);
            AppState::new(&cc.egui_ctx)
        })
}

/// Step frames until `pred` holds, panicking after a bounded wait.
fn step_until(harness: &mut Harness<AppState>, what: &str, pred: impl Fn(&AppState) -> bool) {
    for _ in 0..600 {
        if pred(harness.state()) {
            return;
        }
        harness.step();
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("{what} never settled");
}

/// A fresh in-memory SQLite connection through the same draft transitions
/// the connection form uses.
fn add_memory_database(state: &mut AppState) -> ConnectionId {
    state.open_draft(None);
    {
        let draft = state.draft_mut().expect("the open draft");
        draft.name = "local".to_owned();
        draft.env = Environment::Dev;
        draft.config = ConnectionConfig::Sqlite { read_only: false, path: ":memory:".into() };
    }
    state.save_draft();
    state.connections().last().expect("the saved connection").id
}

fn connect_and_wait(harness: &mut Harness<AppState>, conn: ConnectionId) {
    harness.state_mut().connect(conn);
    step_until(harness, "the connection", |state| {
        matches!(
            state.connections().iter().find(|c| c.id == conn).map(|c| c.state()),
            Some(ConnectionState::Connected { .. }) | Some(ConnectionState::Error { .. })
        )
    });
}

fn run_sql(harness: &mut Harness<AppState>, tab: TabId, sql: &str) {
    if let Some(text) = harness.state_mut().tab_sql_mut(tab) {
        *text = sql.to_owned();
    }
    harness.state_mut().start_run(tab, sql, sql.len(), None);
    step_until(harness, "the run", |state| {
        matches!(
            state.tab(tab).map(|t| t.outcome()),
            Some(RunOutcome::Ok { .. }) | Some(RunOutcome::Err(_))
        )
    });
    if let Some(tab) = harness.state().tab(tab)
        && let RunOutcome::Err(message) = tab.outcome()
    {
        panic!("SQL failed: {message}");
    }
}

/// One harness on a fresh scratch profile with a five-row table in an
/// in-memory database and that table's SELECT as the active result.
fn grid_setup(tag: &str) -> (Scratch, Harness<'static, AppState>, TabId) {
    let scratch = Scratch::new(tag);
    let mut harness = build();
    let conn = add_memory_database(harness.state_mut());
    connect_and_wait(&mut harness, conn);
    let tab = harness.state_mut().new_tab(conn);
    harness.state_mut().activate(tab);
    run_sql(
        &mut harness,
        tab,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL, plan TEXT);\n\
         INSERT INTO users (email, plan) VALUES\n\
         ('ada@example.com', 'pro'),\n\
         ('grace@example.com', 'free'),\n\
         ('alan@example.com', 'team'),\n\
         ('edsger@example.com', 'pro'),\n\
         ('barbara@example.com', 'free');",
    );
    run_sql(
        &mut harness,
        tab,
        "SELECT id, email, plan FROM users ORDER BY id;",
    );
    (scratch, harness, tab)
}

/// The text a single frame asked the clipboard to carry, if any.
fn copied_text(harness: &Harness<AppState>) -> Option<String> {
    harness
        .output()
        .platform_output
        .commands
        .iter()
        .find_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        })
}

/// Press `key` under `modifiers` and read the text the frame that consumed
/// the press copied. The key-down event is queued alone so that frame's
/// output is the one read back.
fn press_for_copy(
    harness: &mut Harness<AppState>,
    modifiers: egui::Modifiers,
    key: egui::Key,
) -> Option<String> {
    let key_event = |pressed| egui::Event::Key {
        key,
        pressed,
        modifiers,
        repeat: false,
        physical_key: None,
    };
    harness.event(egui::Event::ModifiersChanged(modifiers));
    harness.event(key_event(true));
    harness.step();
    let copied = copied_text(harness);
    harness.event(key_event(false));
    harness.event(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
    harness.step();
    copied
}

/// The grid's Ctrl+C must stay dead while a modal owns the keyboard. The
/// params prompt is the modal the grid's own chain missed, so it is the one
/// that proves the grid reads the shared gate.
#[test]
fn ctrl_c_is_dead_while_the_params_prompt_is_open() {
    let _lock = env_lock();
    let (_scratch, mut harness, tab) = grid_setup("params-gate");
    assert!(
        press_for_copy(&mut harness, egui::Modifiers::COMMAND, egui::Key::C).is_some(),
        "Ctrl+C copies the selection with no modal open"
    );
    let params_sql = "SELECT id FROM users WHERE id = :id";
    harness.state_mut().start_run(tab, params_sql, params_sql.len(), None);
    harness.run();
    assert!(harness.state().pending_params().is_some(), "the prompt is armed");
    assert!(
        press_for_copy(&mut harness, egui::Modifiers::COMMAND, egui::Key::C).is_none(),
        "Ctrl+C stays dead under the params prompt"
    );
}
