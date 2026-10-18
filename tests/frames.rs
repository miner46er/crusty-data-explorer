//! Rendered-frame captures for design review.
//!
//! These are not visual-regression tests. Each one renders a real frame
//! through the eframe path and writes a PNG under `target/frames/` for a
//! human or an agent to look at. Every capture asserts that the frame
//! rendered at the requested size, and a test that covers one affordance
//! also asserts the node or the pixel that affordance produced, so no
//! capture is left assertion-free. The captures stay portable across
//! platforms and font stacks: the assertions name text and sampled pixels,
//! never a golden image. Committed goldens can grow from here once one
//! machine owns the baselines.
//!
//! Run with `cargo test --test frames -- --nocapture`; the PNG paths and
//! the sampled pixels print as the tests run.

use std::ffi::OsString;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use eframe::egui;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};

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
    build_at(*FRAME_SIZE)
}

/// A harness rendering at an explicit size, for defects that only bite at
/// one width.
fn build_at([w, h]: [u32; 2]) -> Harness<'static, AppState> {
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
    capture_at(harness, name, *FRAME_SIZE);
}

fn capture_at(harness: &mut Harness<AppState>, name: &str, [w, h]: [u32; 2]) {
    let image = harness.render().expect("render the frame");
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

/// One sampled row of the autocommit chip: the edge pixel, the fill behind
/// the label, and the panel beside the chip, each as the capture painted it.
#[derive(Debug)]
struct ChipPixels {
    edge: [u8; 4],
    fill: [u8; 4],
    panel: [u8; 4],
}

/// The greatest per-channel distance between two pixels.
fn channel_distance(a: [u8; 4], b: [u8; 4]) -> u8 {
    a.iter().zip(b).map(|(a, b)| a.abs_diff(b)).max().unwrap_or(0)
}

/// WCAG contrast for two sRGB pixels, so a sample answers a design review in
/// the terms it was raised in.
fn contrast_ratio(a: [u8; 4], b: [u8; 4]) -> f32 {
    fn luminance(pixel: [u8; 4]) -> f32 {
        let channel = |value: u8| {
            let value = value as f32 / 255.0;
            if value <= 0.04045 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(pixel[0]) + 0.7152 * channel(pixel[1]) + 0.0722 * channel(pixel[2])
    }
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// One pixel of a captured frame.
fn pixel(image: &image::RgbaImage, x: u32, y: u32) -> [u8; 4] {
    image.get_pixel(x.min(image.width() - 1), y.min(image.height() - 1)).0
}

/// Save the chip's capture and sample the pixels around its left edge: the
/// fill inside the chip, the panel to the right of the row, and the strongest
/// neighbour of the panel across the 1px stroke. All three print for the
/// design record.
fn sample_chip(harness: &mut Harness<AppState>, name: &str) -> ChipPixels {
    capture(harness, name);
    let chip = harness.get_by_label("Autocommit").rect();
    let image = harness.render().expect("render the frame");
    let y = chip.center().y.round() as u32;
    let fill = pixel(&image, (chip.left() + 5.0).round() as u32, y);
    let panel = pixel(&image, image.width() - 8, y);
    let left = (chip.left().round() as i64 - 2).max(0) as u32;
    let right = chip.left().round() as u32 + 2;
    let edge = (left..=right)
        .map(|x| pixel(&image, x, y))
        .max_by_key(|sample| channel_distance(*sample, panel))
        .expect("a sampling window");
    println!(
        "{name}: edge {edge:?} fill {fill:?} panel {panel:?} \
         edge/panel {:.2}:1 fill/panel {:.2}:1",
        contrast_ratio(edge, panel),
        contrast_ratio(fill, panel),
    );
    ChipPixels { edge, fill, panel }
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

/// The window size the operator's screenshot came from: the 240px sidebar
/// leaves the right-hand panels under 320px wide.
const NARROW_WINDOW: [u32; 2] = [640, 640];

/// Every listed node that rendered, as (label, rect) pairs. A narrow layout
/// may drop an informational node, so absence is not an error here.
fn rects_of(harness: &Harness<AppState>, labels: &[&str]) -> Vec<(String, egui::Rect)> {
    labels
        .iter()
        .filter_map(|label| {
            harness
                .query_by_label(label)
                .map(|node| ((*label).to_owned(), node.rect()))
        })
        .collect()
}

/// The window rect of a capture, in the coordinates node rects use.
fn window_rect([w, h]: [u32; 2]) -> egui::Rect {
    egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w as f32, h as f32))
}

/// No two of the listed nodes may paint over each other: chrome covering
/// chrome is the defect the screenshot shows. Only rendered pairs are
/// compared, with a half-pixel tolerance so edge-touching reads as clear.
fn assert_no_overlap(harness: &Harness<AppState>, labels: &[&str]) {
    let found = rects_of(harness, labels);
    for (i, (a_label, a)) in found.iter().enumerate() {
        for (b_label, b) in &found[i + 1..] {
            let overlap = a.intersect(*b);
            assert!(
                overlap.width() <= 0.5 || overlap.height() <= 0.5,
                "{a_label} {a:?} paints over {b_label} {b:?}"
            );
        }
    }
}

/// Every listed node that rendered must sit inside the window. A control
/// painted past an edge is unreachable, the same defect from the other side.
fn assert_inside_window(harness: &Harness<AppState>, labels: &[&str], size: [u32; 2]) {
    let window = window_rect(size);
    for (label, rect) in rects_of(harness, labels) {
        assert!(window.contains_rect(rect), "{label} {rect:?} paints outside {window:?}");
    }
}

/// The close button belonging to the tab row that starts at `label`: the
/// first one to its right on the same line.
fn close_button_after(harness: &Harness<AppState>, label: &str) -> Option<egui::Rect> {
    let row = harness.query_by_label(label)?.rect();
    harness
        .query_all_by_label("×")
        .map(|node| node.rect())
        .filter(|rect| {
            rect.min.x >= row.max.x && rect.min.y <= row.max.y && rect.max.y >= row.min.y
        })
        .min_by(|a, b| a.min.x.total_cmp(&b.min.x))
}

/// Chrome at the width the operator reported, with three tabs, a folder and
/// a result in the grid: status bar, both toolbars, and the tab strip keep
/// their controls inside the window and never paint over each other.
#[test]
fn narrow_window_chrome_stays_clear() {
    let _guard = env_lock();
    let _scratch = Scratch::new("narrow");
    let mut harness = build_at(NARROW_WINDOW);
    let conn = add_memory_database(harness.state_mut());
    harness.state_mut().move_connection(conn, Some("test folder"));
    connect_and_wait(&mut harness, conn);
    let setup = harness.state_mut().new_tab(conn);
    harness.state_mut().activate(setup);
    run_sql(
        &mut harness,
        setup,
        "CREATE TABLE persons (personid INTEGER PRIMARY KEY, lastname TEXT);\n\
         INSERT INTO persons (lastname) VALUES ('test');",
    );
    let mut tabs = vec![setup];
    for _ in 0..2 {
        let tab = harness.state_mut().new_tab(conn);
        harness.state_mut().activate(tab);
        tabs.push(tab);
    }
    for tab in &tabs {
        run_sql(&mut harness, *tab, "SELECT * FROM persons;");
    }
    let active = *tabs.last().expect("three tabs");
    harness.state_mut().activate(active);
    harness.run();
    capture_at(&mut harness, "narrow-dark", NARROW_WINDOW);

    assert_no_overlap(
        &harness,
        &["1 connection, 1 live", "local · SQLite", "Compare", "+ Table", "☀ Light"],
    );
    assert_inside_window(
        &harness,
        &[
            "Compare", "+ Table", "☀ Light", "Filters", "Copy", "Export", "Edit", "Detail",
            "Chart", "▶ Run", "Explain", "Save", "Transaction", "Complete", "Format", "Find",
        ],
        NARROW_WINDOW,
    );
    assert_no_overlap(
        &harness,
        &["Filters", "Copy", "Export", "Edit", "Detail", "Chart"],
    );
    assert_no_overlap(
        &harness,
        &["▶ Run", "Explain", "Save", "Transaction", "Complete", "Format", "Find"],
    );

    let window = window_rect(NARROW_WINDOW);
    let label = harness
        .query_by_label("persons 3")
        .map(|node| node.rect())
        .expect("the third tab renders");
    assert!(window.contains_rect(label), "the active tab {label:?} leaves the window");
    let close = close_button_after(&harness, "persons 3").expect("the tab's close button");
    assert!(
        window.contains_rect(close),
        "the third tab's close button {close:?} paints outside {window:?}"
    );
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
    assert!(
        harness.query_by_label("Table compare").is_none(),
        "table compare lives in a window, not in the results area"
    );
    assert!(
        harness.query_by_label("Chart").is_some(),
        "the Chart toggle is in the results toolbar"
    );
    assert!(
        harness.query_by_label("Kind").is_none(),
        "the chart body stays closed until its toggle is on"
    );
    assert!(
        harness.query_by_label("History").is_some(),
        "the sidebar view row offers History"
    );
    assert!(
        harness.query_by_label("Rerun").is_none(),
        "the default schema view hides the history rows"
    );
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
fn table_tab_frame() {
    let _guard = env_lock();
    let _scratch = Scratch::new("table-tab");
    let mut harness = build();
    let conn = add_memory_database(harness.state_mut());
    connect_and_wait(&mut harness, conn);
    let setup_tab = harness.state_mut().new_tab(conn);
    harness.state_mut().activate(setup_tab);
    run_sql(&mut harness, setup_tab, demo_sql());

    let tab = harness
        .state_mut()
        .open_table_view(conn, "main", "users")
        .expect("table view");
    step_until(&mut harness, "the table run", |state| {
        matches!(
            state.tab(tab).map(|t| t.outcome()),
            Some(RunOutcome::Ok { .. }) | Some(RunOutcome::Err(_))
        )
    });
    if let Some(tab) = harness.state().tab(tab)
        && let RunOutcome::Err(message) = tab.outcome()
    {
        panic!("the table run failed: {message}");
    }
    harness.run();
    assert!(
        harness.query_by_label("▶ Run").is_none(),
        "a table tab has no editor toolbar"
    );
    assert!(
        harness.query_by_label("email").is_some(),
        "the table tab's grid carries the email header"
    );
    capture(&mut harness, "table-tab-dark");

    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::F);
    harness.run();
    harness.event(egui::Event::Text("ada".to_owned()));
    harness.run();
    assert_eq!(
        harness.state().tab(tab).map(|t| t.filter().to_owned()).as_deref(),
        Some("ada"),
        "Ctrl+F on a table tab types into the row filter"
    );
}

#[test]
fn line_chart_frame() {
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = demo_setup("line");
    harness.run();

    harness.get_by_label("Chart").click();
    harness.run();
    harness.get_by_label("Line").click();
    harness.run();
    capture(&mut harness, "chart-line-dark");
}

/// The results toolbar's Export menu. The Copy menu is its own test
/// (`copy_menu_frame`): egui closes a menu on any click, so opening the
/// second would close the first.
#[test]
fn toolbar_menu_frames() {
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = demo_setup("menus");
    harness.run();

    harness.get_by_label("Export").click();
    harness.run_steps(6);
    assert!(
        harness.query_by_label("All result sets").is_some(),
        "the export menu is open"
    );
    capture(&mut harness, "export-menu-dark");
}

#[test]
fn copy_menu_frame() {
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = demo_setup("copy-menu");
    harness.run();

    harness.get_by_label("Copy").click();
    harness.run_steps(6);
    assert!(harness.query_by_label("Markdown").is_some(), "the copy menu is open");
    capture(&mut harness, "copy-menu-dark");
}

/// Ticking Headers inside the Copy menu leaves the menu standing: the menu
/// ignores inside clicks, so the capture after the tick still shows it.
#[test]
fn copy_menu_headers_frame() {
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = demo_setup("copy-headers");
    harness.run();

    harness.get_by_label("Copy").click();
    harness.run_steps(6);
    assert!(harness.query_by_label("Headers").is_some(), "the copy menu is open");
    let before = harness
        .get_by_label("Headers")
        .accesskit_node()
        .toggled()
        .expect("the Headers checkbox reports its state");
    harness.get_by_label("Headers").click();
    harness.run_steps(6);
    let after = harness
        .get_by_label("Headers")
        .accesskit_node()
        .toggled()
        .expect("the Headers checkbox reports its state");
    assert_ne!(before, after, "the tick landed");
    assert!(harness.query_by_label("Markdown").is_some(), "the menu is still open");
    capture(&mut harness, "copy-menu-headers-dark");
}

/// The chart hover tooltip. The pointer lands above the "N plotted" note,
/// which sits under the plot, and the steps clear egui's tooltip delay.
#[test]
fn chart_hover_frame() {
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = demo_setup("hover");
    harness.run();

    harness.get_by_label("Chart").click();
    harness.run();
    harness.get_by_label("Line").click();
    harness.run();
    let note = harness.get_by_label("5 plotted").rect();
    harness.hover_at(egui::pos2(note.center().x, note.top() - 110.0));
    harness.run_steps(4);
    // Only the tooltip puts a point's label and its value in one node.
    assert!(
        harness.query_all_by_label_contains("@example.com · ").next().is_some(),
        "the hover tooltip names the point"
    );
    capture(&mut harness, "chart-hover-dark");

    harness.get_by_label("Bar").click();
    harness.run();
    let note = harness.get_by_label("5 plotted").rect();
    harness.hover_at(egui::pos2(note.center().x, note.top() - 110.0));
    harness.run_steps(4);
    // Only the tooltip puts a point's label and its value in one node.
    assert!(
        harness.query_all_by_label_contains("@example.com · ").next().is_some(),
        "the hover tooltip names the point"
    );
    capture(&mut harness, "chart-hover-bar-dark");
}

/// The autocommit chip on a SQLite tab, where the mode is constant and the
/// label stays plain. Each capture prints the sampled edge, fill, and panel
/// pixels: the fill alone sits within a few percent of the panel, so the
/// edge is what makes the chip read as a control slot.
#[test]
fn autocommit_chip_frames() {
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = demo_setup("chip");
    harness.run();

    // On SQLite the autocommit control waits inside the Transaction menu.
    harness.get_by_label("Transaction").click();
    harness.run_steps(6);
    assert!(
        harness.query_by_label("Autocommit").is_some(),
        "the transaction menu is open"
    );
    let dark = sample_chip(&mut harness, "autocommit-chip-dark");
    assert!(
        channel_distance(dark.edge, dark.panel) >= 16,
        "the dark chip edge stands out from the panel: {dark:?}"
    );
    assert!(
        channel_distance(dark.fill, dark.panel) < 16,
        "the dark fill alone stays quiet against the panel: {dark:?}"
    );

    harness.state_mut().toggle_theme();
    harness.run();
    assert!(
        harness.query_by_label("Autocommit").is_some(),
        "the transaction menu survives the theme flip"
    );
    let light = sample_chip(&mut harness, "autocommit-chip-light");
    assert!(
        channel_distance(light.edge, light.panel) >= 16,
        "the light chip edge stands out from the panel: {light:?}"
    );
    assert!(
        channel_distance(light.fill, light.panel) < 16,
        "the light fill alone stays quiet against the panel: {light:?}"
    );
}
