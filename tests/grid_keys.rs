//! Keyboard behavior of the results grid over a rendered frame.
//!
//! Each test builds a real `AppState` on an in-memory SQLite database and
//! draws it through `ui::root`, so the assertions read what the operator
//! could see rather than a private flag.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use eframe::egui;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

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
         ('alan@example.com', NULL),\n\
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

/// One harness on a fresh scratch profile with a two-hundred-row table, so
/// most of the view sits below the viewport and virtualization hides it.
fn scrolling_setup(tag: &str) -> (Scratch, Harness<'static, AppState>, TabId) {
    let scratch = Scratch::new(tag);
    let mut harness = build();
    let conn = add_memory_database(harness.state_mut());
    connect_and_wait(&mut harness, conn);
    let tab = harness.state_mut().new_tab(conn);
    harness.state_mut().activate(tab);
    run_sql(
        &mut harness,
        tab,
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200)\n\
         SELECT i AS id, 'row-' || i AS note FROM n;",
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

/// Press `key` alone and let the frames settle.
fn press_key(harness: &mut Harness<AppState>, key: egui::Key) {
    harness.key_press(key);
    harness.run();
}

/// Open the row-detail pane, whose `Row N of M` text reports the cursor.
fn open_detail(harness: &mut Harness<AppState>) {
    harness.get_by_label("Detail").click();
    harness.run();
}

#[test]
fn arrow_keys_move_the_active_cell() {
    let _lock = env_lock();
    let (_scratch, mut harness, _tab) = grid_setup("arrows");
    open_detail(&mut harness);
    assert!(harness.query_by_label("Click a cell to inspect its row.").is_some());
    press_key(&mut harness, egui::Key::ArrowDown);
    assert!(
        harness.query_by_label("Row 1 of 5").is_some(),
        "the first displayed row takes the cursor"
    );
    press_key(&mut harness, egui::Key::ArrowDown);
    assert!(harness.query_by_label("Row 2 of 5").is_some());
    press_key(&mut harness, egui::Key::ArrowRight);
    assert!(
        harness.query_by_label("Row 2 of 5").is_some(),
        "a column step stays on the row"
    );
    press_key(&mut harness, egui::Key::End);
    press_key(&mut harness, egui::Key::Enter);
    assert!(
        harness.query_by_label("Currently: free").is_some(),
        "the editor opens on the last column of the moved-to row"
    );
    press_key(&mut harness, egui::Key::Escape);
    assert!(
        harness.query_by_label("Currently: free").is_none(),
        "Escape closes the cell editor"
    );
}

/// The Enter presses below prove Tab left no widget focused: they run behind
/// the same gate as Ctrl+C, so a Tab that moved egui's focus would open no
/// editor.
#[test]
fn tab_walks_the_view_and_keeps_the_grid_alive() {
    let _lock = env_lock();
    let (_scratch, mut harness, _tab) = grid_setup("tab");
    open_detail(&mut harness);
    press_key(&mut harness, egui::Key::Tab);
    assert!(
        harness.query_by_label("Row 1 of 5").is_some(),
        "Tab lands on the first displayed row"
    );
    press_key(&mut harness, egui::Key::Tab);
    press_key(&mut harness, egui::Key::Tab);
    press_key(&mut harness, egui::Key::Tab);
    assert!(
        harness.query_by_label("Row 2 of 5").is_some(),
        "Tab wraps off the last column onto the next row"
    );
    press_key(&mut harness, egui::Key::Enter);
    assert!(
        harness.query_by_label("Currently: 2").is_some(),
        "the cursor walked the columns before it wrapped to the row's first one"
    );
    press_key(&mut harness, egui::Key::Escape);
    harness.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::Tab);
    harness.run();
    press_key(&mut harness, egui::Key::Enter);
    assert!(
        harness.query_by_label("Currently: pro").is_some(),
        "Shift+Tab wraps back onto the previous row's last column"
    );
    press_key(&mut harness, egui::Key::Escape);
}

#[test]
fn escape_cancels_the_import_confirmation() {
    let _lock = env_lock();
    let (_scratch, mut harness, tab) = grid_setup("escape");
    let conn = harness.state().tab(tab).and_then(|tab| tab.conn).expect("the tab's connection");
    step_until(&mut harness, "the schema", |state| state.schema(conn).is_some());
    let path = std::env::temp_dir()
        .join(format!("cde-grid-keys-import-{}.csv", std::process::id()));
    std::fs::write(&path, "id,email,plan\n9,nine@example.com,pro\n").unwrap();
    harness.state_mut().begin_import(tab, &path);
    harness.run();
    assert!(
        harness.state().import_draft().is_some(),
        "the import file waits for confirmation"
    );
    press_key(&mut harness, egui::Key::Escape);
    assert!(
        harness.state().import_draft().is_none(),
        "Escape cancels the pending import"
    );
    assert_eq!(
        harness.state().tab(tab).map(|tab| tab.staged_count()),
        Some(0),
        "a cancelled import stages nothing"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn shift_command_c_copies_the_active_cell() {
    let _lock = env_lock();
    let (_scratch, mut harness, _tab) = grid_setup("copy-cell");
    press_key(&mut harness, egui::Key::ArrowDown);
    let copied = press_for_copy(
        &mut harness,
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        egui::Key::C,
    );
    assert_eq!(copied.as_deref(), Some("1"), "the cursor's cell copies as plain text");
    assert!(harness.query_by_label("Copied 1 cell").is_some());
    press_key(&mut harness, egui::Key::ArrowRight);
    press_key(&mut harness, egui::Key::ArrowRight);
    press_key(&mut harness, egui::Key::ArrowDown);
    press_key(&mut harness, egui::Key::ArrowDown);
    let copied = press_for_copy(
        &mut harness,
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        egui::Key::C,
    );
    assert_eq!(
        copied.as_deref(),
        Some(""),
        "a NULL cell copies as an empty string"
    );
    assert_eq!(
        press_for_copy(&mut harness, egui::Modifiers::COMMAND, egui::Key::C).as_deref(),
        Some("id\temail\tplan\n3\talan@example.com\t\n"),
        "plain Ctrl+C still copies the cursor's row as TSV"
    );
}

/// The cursor's row is scrolled into view: with 200 rows the last one is
/// virtualized away until Ctrl+End moves the cursor onto it.
#[test]
fn ctrl_end_scrolls_the_cursor_row_into_view() {
    let _lock = env_lock();
    let (_scratch, mut harness, _tab) = scrolling_setup("scroll");
    open_detail(&mut harness);
    assert!(
        harness.query_by_label("row-200").is_none(),
        "the last row starts below the viewport"
    );
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::End);
    harness.run();
    assert!(
        harness.query_by_label("Row 1 of 200").is_some(),
        "a step with no cursor lands on the first row"
    );
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::End);
    harness.run();
    assert!(
        harness.query_by_label("Row 200 of 200").is_some(),
        "Ctrl+End puts the cursor on the last row"
    );
    assert!(
        harness.query_by_label("row-200").is_some(),
        "the cursor's row scrolled into view"
    );
}
