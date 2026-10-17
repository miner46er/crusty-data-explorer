//! Rendered-frame captures for design review.
//!
//! These are not visual-regression tests. Each one renders a real frame
//! through the eframe path and writes a PNG under `target/frames/` for a
//! human or an agent to look at. The only assertion is that the frame
//! rendered at the requested size, so the captures stay portable across
//! platforms and font stacks. Committed goldens can grow from here once
//! one machine owns the baselines.
//!
//! Run with `cargo test --test frames -- --nocapture`; the PNG paths print
//! as the tests run.

use std::ffi::OsString;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use eframe::egui;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use crusty_data_explorer::app::{AppState, ConnectionState, Environment, RunOutcome};
use crusty_data_explorer::db::{ConnectionConfig, ConnectionId, TabId};
use crusty_data_explorer::ui;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Serialize the tests that mutate `XDG_CONFIG_HOME`, tolerating a mutex a
/// panicking test left poisoned: the scratch dir is restored on drop, so the
/// next test still starts from clean state.
fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The screen the captures render at. `CDE_FRAME_SIZE=WxH` overrides it so
/// width-dependent defects can be reproduced at the width where they bite.
/// Parsed once per process: one clear parse panic on the first deref, and
/// later tests fail on that memoized panic, not on a poisoned lock.
static FRAME_SIZE: LazyLock<[u32; 2]> = LazyLock::new(|| {
    let Some(value) = std::env::var_os("CDE_FRAME_SIZE") else {
        return [1440, 900];
    };
    let text = value.to_string_lossy();
    let parsed = text
        .split_once('x')
        .and_then(|(w, h)| Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?)))
        .filter(|(w, h)| *w > 0 && *h > 0);
    match parsed {
        Some((w, h)) => [w, h],
        None => panic!(
            "CDE_FRAME_SIZE must be WxH with nonzero dimensions, got {text:?}"
        ),
    }
});

/// Points `XDG_CONFIG_HOME` at a scratch dir for the guard's lifetime and
/// restores it on drop, so no real profile, tab, or settings file is read
/// or written. The caller serializes through [`env_lock`].
struct Scratch {
    dir: std::path::PathBuf,
    prev: Option<OsString>,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cde-frames-{tag}-{}", std::process::id()));
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
    let [w, h] = *FRAME_SIZE;
    Harness::builder()
        .with_size(egui::vec2(w as f32, h as f32))
        .build_eframe(|cc| {
            ui::install(&cc.egui_ctx);
            AppState::new(&cc.egui_ctx)
        })
}

/// Render the current frame to `target/frames/<name>.png`, asserting it
/// rendered at the requested size so a broken render fails the test.
fn capture(harness: &mut Harness<AppState>, name: &str) {
    let image = harness.render().expect("render the frame");
    let [w, h] = *FRAME_SIZE;
    assert_eq!(
        image.dimensions(),
        (w, h),
        "capture {name} rendered at the requested size"
    );
    let dir = std::path::Path::new("target/frames");
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{name}.png"));
    image.save(&path).expect("save the capture");
    println!("capture: {}", path.display());
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

/// Connect and wait for the session to land, so runs have somewhere to go.
fn connect_and_wait(harness: &mut Harness<AppState>, conn: ConnectionId) {
    harness.state_mut().connect(conn);
    step_until(harness, "the connection", |state| {
        matches!(
            state.connections().iter().find(|c| c.id == conn).map(|c| c.state()),
            Some(ConnectionState::Connected { .. }) | Some(ConnectionState::Error { .. })
        )
    });
}

/// Run one SQL text through the tab's default scope and wait for the
/// outcome, failing the test when the run itself fails.
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

/// The demo schema and rows every data capture shows.
fn demo_sql() -> &'static str {
    "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL, plan TEXT, active INTEGER, joined TEXT);\n\
     INSERT INTO users (email, plan, active, joined) VALUES\n\
     ('ada@example.com', 'pro', 1, '2026-01-04'),\n\
     ('grace@example.com', 'free', 0, '2026-02-11'),\n\
     ('alan@example.com', 'team', 1, '2026-03-02'),\n\
     ('edsger@example.com', 'pro', 1, '2026-03-19'),\n\
     ('barbara@example.com', 'free', 0, '2026-04-01');"
}

/// One harness on a fresh scratch profile with the in-memory demo schema
/// loaded and its tab selected: the preamble every data capture shares. The
/// scratch dir must outlive the harness, so it comes back beside it.
fn demo_setup(tag: &str) -> (Scratch, Harness<'static, AppState>, TabId) {
    let scratch = Scratch::new(tag);
    let mut harness = build();
    let conn = add_memory_database(harness.state_mut());
    connect_and_wait(&mut harness, conn);
    let tab = harness.state_mut().new_tab(conn);
    harness.state_mut().activate(tab);
    run_sql(&mut harness, tab, demo_sql());
    run_sql(
        &mut harness,
        tab,
        "SELECT id, email, plan, active, joined FROM users ORDER BY id;",
    );
    (scratch, harness, tab)
}

#[test]
fn empty_frame() {
    let _guard = env_lock();
    let _scratch = Scratch::new("empty");
    let mut harness = build();
    harness.run();
    capture(&mut harness, "empty-dark");
}

#[test]
fn grid_frames() {
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = demo_setup("grid");
    harness.run();
    capture(&mut harness, "grid-dark");

    harness.state_mut().toggle_theme();
    harness.run();
    capture(&mut harness, "grid-light");

    harness.state_mut().open_palette();
    harness.run();
    assert!(harness.state().palette_open(), "the palette stays open for the capture");
    capture(&mut harness, "palette-light");

    harness.state_mut().close_palette();
    harness.state_mut().toggle_theme();
    harness.state_mut().open_palette();
    harness.run();
    assert!(harness.state().palette_open(), "the palette stays open for the capture");
    capture(&mut harness, "palette-dark");
}

#[test]
fn line_chart_frame() {
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = demo_setup("line");
    harness.run();

    harness.get_by_label("Line").click();
    harness.run();
    capture(&mut harness, "chart-line-dark");
}

/// The results toolbar's two menus, one capture each: egui closes a menu on
/// any click, so opening the second would close the first.
#[test]
fn toolbar_menu_frames() {
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = demo_setup("menus");
    harness.run();

    harness.get_by_label("Export").click();
    harness.run_steps(6);
    capture(&mut harness, "export-menu-dark");
}

#[test]
fn copy_menu_frame() {
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = demo_setup("copy-menu");
    harness.run();

    harness.get_by_label("Copy").click();
    harness.run_steps(6);
    capture(&mut harness, "copy-menu-dark");
}

/// The chart hover tooltip. The pointer lands above the "N plotted" note,
/// which sits under the plot, and the steps clear egui's tooltip delay.
#[test]
fn chart_hover_frame() {
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = demo_setup("hover");
    harness.run();

    harness.get_by_label("Line").click();
    harness.run();
    let note = harness.get_by_label("5 plotted").rect();
    harness.hover_at(egui::pos2(note.center().x, note.top() - 110.0));
    harness.run_steps(4);
    capture(&mut harness, "chart-hover-dark");

    harness.get_by_label("Bar").click();
    harness.run();
    let note = harness.get_by_label("5 plotted").rect();
    harness.hover_at(egui::pos2(note.center().x, note.top() - 110.0));
    harness.run_steps(4);
    capture(&mut harness, "chart-hover-bar-dark");
}
