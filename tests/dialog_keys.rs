//! Keyboard behavior of the modal dialogs over a rendered frame.
//!
//! Each test builds a real `AppState` on an in-memory SQLite database and
//! draws it through `ui::root`, so the assertions read what the operator
//! could see rather than a private flag.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use eframe::egui;
use egui_kittest::Harness;

use crusty_data_explorer::app::{AppState, ConnectionState, Environment, Modal, RunOutcome};
use crusty_data_explorer::db::{ConnectionConfig, ConnectionId, PostgresSslMode, TabId};
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
        let dir = std::env::temp_dir().join(format!("cde-dialog-keys-{tag}-{}", std::process::id()));
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

/// One saved Postgres connection, unconnected: the prompt and prod confirm
/// cases need its config and environment, not a server.
fn add_postgres(state: &mut AppState, env: Environment) -> ConnectionId {
    state.open_draft(None);
    {
        let draft = state.draft_mut().expect("the open draft");
        draft.name = "pg".to_owned();
        draft.env = env;
        draft.config = ConnectionConfig::Postgres {
            host: "localhost".to_owned(),
            port: 5432,
            database: "appdb".to_owned(),
            user: "owner".to_owned(),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
            read_only: false,
        };
    }
    state.save_draft();
    state.connections().last().expect("the saved connection").id
}

fn import_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("cde-dialogs-import-{}.csv", std::process::id()))
}

fn setup_draft(harness: &mut Harness<AppState>, _tab: TabId) {
    harness.state_mut().open_draft(None);
}
fn setup_import(harness: &mut Harness<AppState>, tab: TabId) {
    let conn = harness.state().tab(tab).and_then(|tab| tab.conn).expect("the tab connection");
    step_until(harness, "the schema", |state| state.schema(conn).is_some());
    let path = import_path();
    std::fs::write(&path, "id,email,plan\n9,nine@example.com,pro\n").unwrap();
    harness.state_mut().begin_import(tab, &path);
}
fn setup_write(harness: &mut Harness<AppState>, tab: TabId) {
    harness.state_mut().start_run(tab, "DELETE FROM users", 0, None);
}
fn setup_edit_preview(harness: &mut Harness<AppState>, tab: TabId) {
    let conn = harness.state().tab(tab).and_then(|tab| tab.conn).expect("the tab connection");
    step_until(harness, "the schema", |state| state.schema(conn).is_some());
    harness.state_mut().stage_edit(tab, 0, 1, "ada@example.com".to_owned());
    harness.state_mut().apply_edits(tab);
}
fn setup_prod_connect(harness: &mut Harness<AppState>, _tab: TabId) {
    let conn = add_postgres(harness.state_mut(), Environment::Prod);
    harness.state_mut().connect(conn);
}
fn setup_terminate(harness: &mut Harness<AppState>, _tab: TabId) {
    let conn = add_postgres(harness.state_mut(), Environment::Prod);
    harness.state_mut().request_terminate(conn, "4242");
}
fn setup_table_ddl(harness: &mut Harness<AppState>, _tab: TabId) {
    let conn = add_postgres(harness.state_mut(), Environment::Prod);
    let tab = harness.state_mut().new_tab(conn);
    harness.state_mut().open_table_create(conn);
    {
        let editor = harness.state_mut().table_editor_mut().expect("the table editor");
        editor.form.table = "audit".to_owned();
        editor.form.columns[0].name = "id".to_owned();
        editor.form.columns[0].data_type = "INTEGER".to_owned();
    }
    harness.state_mut().apply_table_ddl(tab);
}
fn setup_table_editor(harness: &mut Harness<AppState>, tab: TabId) {
    let conn = harness.state().tab(tab).and_then(|tab| tab.conn).expect("the tab connection");
    harness.state_mut().open_table_create(conn);
}
fn setup_compare(harness: &mut Harness<AppState>, _tab: TabId) {
    harness.state_mut().open_compare();
}
fn setup_palette(harness: &mut Harness<AppState>, _tab: TabId) {
    harness.state_mut().open_palette();
}

#[derive(Clone, Copy)]
struct DialogCase {
    name: &'static str,
    expected: Modal,
    setup: fn(&mut Harness<AppState>, TabId),
    open: fn(&AppState) -> bool,
    held: fn(&Harness<AppState>, TabId),
}

/// Approval is read from the fact it would change, never from `Running`:
/// a fast SQLite DELETE finishes before the test looks.
fn assert_sent_nothing(harness: &Harness<AppState>, tab: TabId, conns: usize) {
    let state = harness.state();
    assert_eq!(state.connections().len(), conns, "the dialog connected to nothing");
    let rows = state
        .tab(tab)
        .and_then(|t| t.active_query_result())
        .map(|result| result.row_count());
    assert_eq!(rows, Some(5), "the dialog ran nothing");
}

/// The approval's own fact, still unchanged: plain Enter never approves, so
/// the state approval would move stays put. Enter cancels the approve-type
/// dialogs through their focused Cancel, which clears the pending value the
/// same way approval does: the detector is the approval's effect, not the
/// pending flag. Connection and row counts alone cannot see a prod approval
/// go through.
fn prod_conn(harness: &Harness<AppState>) -> ConnectionId {
    harness
        .state()
        .connections()
        .iter()
        .find(|c| c.env == Environment::Prod)
        .expect("the prod connection")
        .id
}

fn held_prod_connect(harness: &Harness<AppState>, _tab: TabId) {
    let state = harness.state();
    let prod = prod_conn(harness);
    assert!(
        matches!(
            state.connections().iter().find(|c| c.id == prod).map(|c| c.state()),
            Some(ConnectionState::Disconnected)
        ),
        "prod-connect sent nothing on Enter"
    );
    assert!(
        state.password_prompt().is_none(),
        "prod-connect asked for no password on Enter"
    );
}

fn held_terminate(harness: &Harness<AppState>, _tab: TabId) {
    let state = harness.state();
    assert!(
        state.sessions_error(prod_conn(harness)).is_none(),
        "terminate sent nothing on Enter"
    );
}

fn held_draft(harness: &Harness<AppState>, _tab: TabId) {
    assert!(harness.state().draft().is_some(), "draft still open after Enter");
}

fn held_import(harness: &Harness<AppState>, tab: TabId) {
    assert!(
        !matches!(
            harness.state().tab(tab).map(|t| t.outcome()),
            Some(RunOutcome::Running { .. })
        ),
        "import sent nothing on Enter"
    );
}

fn held_write(harness: &Harness<AppState>, tab: TabId) {
    assert!(
        !matches!(
            harness.state().tab(tab).map(|t| t.outcome()),
            Some(RunOutcome::Running { .. })
        ),
        "write-confirm sent nothing on Enter"
    );
}

fn held_edit_preview(harness: &Harness<AppState>, tab: TabId) {
    let state = harness.state();
    assert_eq!(
        state.tab(tab).map(|t| t.staged_count()),
        Some(1),
        "cancel keeps the staged edit"
    );
    assert!(
        !matches!(
            state.tab(tab).map(|t| t.outcome()),
            Some(RunOutcome::Running { .. })
        ),
        "edit-preview sent nothing on Enter"
    );
}

fn held_table_ddl(harness: &Harness<AppState>, _tab: TabId) {
    let state = harness.state();
    let ddl = state
        .tabs()
        .iter()
        .find(|t| t.conn == Some(prod_conn(harness)))
        .expect("the ddl tab");
    assert!(
        matches!(ddl.outcome(), RunOutcome::Never),
        "table-ddl sent nothing on Enter"
    );
}

fn held_table_editor(harness: &Harness<AppState>, tab: TabId) {
    let state = harness.state();
    assert!(state.table_editor().is_some(), "table editor still open after Enter");
    assert!(
        matches!(
            state.tab(tab).map(|t| t.outcome()),
            Some(RunOutcome::Ok { .. })
        ),
        "the blank table editor applied nothing on Enter"
    );
}

fn held_compare(harness: &Harness<AppState>, _tab: TabId) {
    let state = harness.state();
    assert!(state.compare_window_open(), "compare still open after Enter");
    assert!(!state.compare().running(), "compare ran nothing on Enter");
    assert!(state.compare().left_result().is_none(), "compare ran nothing on Enter");
    assert!(state.compare().output().is_none(), "compare ran nothing on Enter");
}

fn held_palette(harness: &Harness<AppState>, _tab: TabId) {
    let state = harness.state();
    assert!(state.palette_open(), "palette still open after Enter");
    assert_eq!(state.tabs().len(), 1, "an empty palette opened no tab");
}

/// Every dialog opens through its real transition, plain Enter never approves
/// or sends from the approve-type and long-form windows, and Escape closes
/// the dialog it was opened with.
#[test]
fn every_dialog_answers_the_keyboard() {
    let _lock = env_lock();
    let cases = [
        DialogCase {
            name: "prod-connect",
            expected: Modal::ProdConnect,
            setup: setup_prod_connect,
            open: |state| state.pending_prod().is_some(),
            held: held_prod_connect,
        },
        DialogCase {
            name: "terminate",
            expected: Modal::Terminate,
            setup: setup_terminate,
            open: |state| state.pending_terminate().is_some(),
            held: held_terminate,
        },
        DialogCase {
            name: "draft",
            expected: Modal::Draft,
            setup: setup_draft,
            open: |state| state.draft().is_some(),
            held: held_draft,
        },
        DialogCase {
            name: "import",
            expected: Modal::Import,
            setup: setup_import,
            open: |state| state.import_draft().is_some(),
            held: held_import,
        },
        DialogCase {
            name: "write-confirm",
            expected: Modal::WriteConfirm,
            setup: setup_write,
            open: |state| state.pending_write().is_some(),
            held: held_write,
        },
        DialogCase {
            name: "edit-preview",
            expected: Modal::EditPreview,
            setup: setup_edit_preview,
            open: |state| state.pending_edit_preview().is_some(),
            held: held_edit_preview,
        },
        DialogCase {
            name: "table-ddl",
            expected: Modal::TableDdl,
            setup: setup_table_ddl,
            open: |state| state.pending_table_ddl().is_some(),
            held: held_table_ddl,
        },
        DialogCase {
            name: "table-editor",
            expected: Modal::TableEditor,
            setup: setup_table_editor,
            open: |state| state.table_editor().is_some(),
            held: held_table_editor,
        },
        DialogCase {
            name: "compare",
            expected: Modal::Compare,
            setup: setup_compare,
            open: |state| state.compare_window_open(),
            held: held_compare,
        },
        DialogCase {
            name: "palette",
            expected: Modal::Palette,
            setup: setup_palette,
            open: |state| state.palette_open(),
            held: held_palette,
        },
    ];
    for case in cases {
        let (_scratch, mut harness, tab) = grid_setup(case.name);
        (case.setup)(&mut harness, tab);
        harness.run();
        assert!((case.open)(harness.state()), "{} opens", case.name);
        assert_eq!(
            harness.state().modal(),
            Some(case.expected),
            "{} owns the keyboard",
            case.name
        );
        let conns = harness.state().connections().len();
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_sent_nothing(&harness, tab, conns);
        (case.held)(&harness, tab);
        harness.key_press(egui::Key::Escape);
        harness.run();
        assert!(!(case.open)(harness.state()), "{} closes on Escape", case.name);
        assert_sent_nothing(&harness, tab, conns);
    }
    let _ = std::fs::remove_file(import_path());

    // The three prompts: autofocus takes typing, Enter submits, Escape cancels.
    let (_scratch, mut harness, _tab) = grid_setup("dialog-save");
    harness.state_mut().open_save_prompt();
    harness.run();
    harness.event(egui::Event::Text("kept".to_owned()));
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(harness.state().save_prompt().is_none(), "SavePrompt Enter submits");
    assert_eq!(harness.state().saved_queries().len(), 1, "the query was saved");

    let (_scratch, mut harness, _tab) = grid_setup("dialog-save-escape");
    harness.state_mut().open_save_prompt();
    harness.run();
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(harness.state().save_prompt().is_none(), "SavePrompt Escape cancels");
    assert!(harness.state().saved_queries().is_empty(), "Escape saved nothing");

    let (_scratch, mut harness, _tab) = grid_setup("dialog-password");
    let conn = add_postgres(harness.state_mut(), Environment::Dev);
    harness.state_mut().open_password_prompt(conn);
    harness.run();
    harness.event(egui::Event::Text("pw".to_owned()));
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(
        harness.state().password_prompt().is_none(),
        "PasswordPrompt Enter submits"
    );

    let (_scratch, mut harness, _tab) = grid_setup("dialog-password-escape");
    let conn = add_postgres(harness.state_mut(), Environment::Dev);
    let conns = harness.state().connections().len();
    harness.state_mut().open_password_prompt(conn);
    harness.run();
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(
        harness.state().password_prompt().is_none(),
        "PasswordPrompt Escape cancels"
    );
    assert_eq!(
        harness.state().connections().len(),
        conns,
        "Escape connected to nothing"
    );

    let (_scratch, mut harness, tab) = grid_setup("dialog-params");
    harness.state_mut().start_run(tab, "SELECT :a AS v", 0, None);
    harness.run();
    harness.event(egui::Event::Text("7".to_owned()));
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(harness.state().pending_params().is_none(), "Params Enter submits");

    let (_scratch, mut harness, tab) = grid_setup("dialog-params-escape");
    harness.state_mut().start_run(tab, "SELECT :a AS v", 0, None);
    harness.run();
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(harness.state().pending_params().is_none(), "Params Escape cancels");
    assert_sent_nothing(&harness, tab, harness.state().connections().len());
}
