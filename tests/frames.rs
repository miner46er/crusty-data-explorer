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
use egui_kittest::kittest::{By, NodeT, Queryable};

use crusty_data_explorer::app::{AppState, ConnectionState, Environment, Modal, RunOutcome};
use crusty_data_explorer::db::{ConnectionConfig, ConnectionId, PostgresSslMode, TabId};
use crusty_data_explorer::ui;
use crusty_data_explorer::ui::complete::editor_id;

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

/// A Postgres connection through the same draft transitions, left
/// unconnected: the config is all the password prompt needs.
fn add_postgres_draft(state: &mut AppState) -> ConnectionId {
    state.open_draft(None);
    {
        let draft = state.draft_mut().expect("the open draft");
        draft.name = "pg".to_owned();
        draft.env = Environment::Dev;
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

/// The default capture size: every row has room to keep today's shape.
const WIDE_WINDOW: [u32; 2] = [1440, 900];

/// The connection card's painted state dot, found by the drag source the dot
/// is: the card drags by its dot, so that response's rect is the dot's.
fn dot_rect(harness: &Harness<AppState>, conn: ConnectionId) -> egui::Rect {
    harness
        .ctx
        .read_response(egui::Id::new(("crusty_conn_drag", conn.raw())))
        .expect("the connection card's state dot")
        .rect
}

/// The pixel closest to `color` in a small window around the expected edge of
/// a ring, and its distance from the color. The ring's stroke is antialiased,
/// so the test asks for its best-covered pixel rather than one exact index.
fn best_pixel_near(
    image: &image::RgbaImage,
    center: egui::Pos2,
    from: f32,
    to: f32,
    color: egui::Color32,
) -> ([u8; 4], u8) {
    let color = color.to_array();
    let steps = ((to - from).abs().ceil() as i64).max(0) + 1;
    let mut best: Option<([u8; 4], u8)> = None;
    for step in 0..steps {
        for dy in -1..=1 {
            let x = (center.x + from + step as f32).round() as u32;
            let y = (center.y + dy as f32).round() as u32;
            let sample = pixel(image, x, y);
            let distance = channel_distance(sample, color);
            if best.is_none_or(|(_, previous)| distance < previous) {
                best = Some((sample, distance));
            }
        }
    }
    best.expect("a sampling window")
}

/// The state dot is painted, not typed: a glyph in the embedded fonts has no
/// disc, so the card and the history rows drew epaint's replacement box. This
/// renders a connected card, an offline one, and one whose connect failed,
/// then samples the pixels of all three dots: the disc's center is the live
/// color and its corner is not, and each ring's center is not the color while
/// its best-covered edge pixel is.
#[test]
fn state_dot_frames() {
    let _guard = env_lock();
    let _scratch = Scratch::new("dots");
    let mut harness = build();
    let live = add_memory_database(harness.state_mut());
    connect_and_wait(&mut harness, live);
    // A second connection that never connects: its card carries the gray ring.
    let offline = add_memory_database(harness.state_mut());
    // A third whose connect fails (no such directory): Guard Red ring.
    harness.state_mut().open_draft(None);
    {
        let draft = harness.state_mut().draft_mut().expect("the open draft");
        draft.name = "broken".to_owned();
        draft.config = ConnectionConfig::Sqlite {
            read_only: false,
            path: "/nonexistent-cde-dir/dot-test.db".into(),
        };
    }
    harness.state_mut().save_draft();
    let failed = harness.state_mut().connections().last().expect("the failed connection").id;
    connect_and_wait(&mut harness, failed);
    assert!(
        matches!(
            harness.state().connections().iter().find(|c| c.id == failed).map(|c| c.state()),
            Some(ConnectionState::Error { .. })
        ),
        "the bogus path must fail to connect, not connect"
    );
    harness.run();
    capture(&mut harness, "state-dots-dark");

    let image = harness.render().expect("render the frame");
    let palette = ui::Palette::dark();
    let live_color = palette.success;
    let offline_color = palette.badge_dev;
    let failed_color = palette.badge_prod;

    let disc = dot_rect(&harness, live);
    let center = pixel(
        &image,
        disc.center().x.round() as u32,
        disc.center().y.round() as u32,
    );
    let corner = pixel(
        &image,
        disc.left_top().x.round() as u32,
        disc.left_top().y.round() as u32,
    );
    println!(
        "disc {disc:?}: center {center:?} corner {corner:?} live {live_color:?}"
    );
    assert_eq!(center, live_color.to_array(), "the disc's center is the live color");
    assert_ne!(corner, live_color.to_array(), "the disc's corner is not the color");

    for (conn, color, what) in [
        (offline, offline_color, "offline"),
        (failed, failed_color, "failed"),
    ] {
        let ring = dot_rect(&harness, conn);
        let radius = ring.width() / 2.0;
        let hole = pixel(
            &image,
            ring.center().x.round() as u32,
            ring.center().y.round() as u32,
        );
        let (edge, distance) =
            best_pixel_near(&image, ring.center(), radius - 2.0, radius, color);
        println!(
            "{what} ring {ring:?}: center {hole:?} edge {edge:?} distance {distance} \
             color {color:?}"
        );
        assert_ne!(hole, color.to_array(), "the {what} ring's center stays hollow");
        // The best-covered edge pixel measured 7 of 255 from the color; the
        // threshold leaves room for another font scale's antialiasing.
        assert!(
            distance <= 12,
            "the {what} ring's edge carries its color: {edge:?} is {distance} from {color:?}"
        );
    }
}

/// The first node with `label` that rendered inside `band`. A label two
/// panels spell (the card's `Edit` beside the results toolbar's menu) answers
/// for the row the band names, so no query here rests on label uniqueness.
fn label_in_band(
    harness: &Harness<AppState>,
    label: &str,
    band: egui::Rect,
) -> Option<egui::Rect> {
    harness
        .query_all_by_label(label)
        .map(|node| node.rect())
        .find(|rect| band.intersects(*rect))
}

/// Every listed node that rendered inside `band`, as (label, rect) pairs. A
/// narrow layout may drop an informational node, so absence is not an error
/// here.
fn rects_of(
    harness: &Harness<AppState>,
    labels: &[&str],
    band: egui::Rect,
) -> Vec<(String, egui::Rect)> {
    labels
        .iter()
        .filter_map(|label| {
            label_in_band(harness, label, band).map(|rect| ((*label).to_owned(), rect))
        })
        .collect()
}

/// The window rect of a capture, in the coordinates node rects use.
fn window_rect([w, h]: [u32; 2]) -> egui::Rect {
    egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w as f32, h as f32))
}

/// No two of the listed nodes in `band` may paint over each other: chrome
/// covering chrome is the defect the screenshot shows. Only rendered pairs
/// are compared, with a half-pixel tolerance so edge-touching reads as clear.
fn assert_no_overlap(harness: &Harness<AppState>, labels: &[&str], band: egui::Rect) {
    let found = rects_of(harness, labels, band);
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

/// Every listed node that rendered inside `band` must sit inside the window.
/// A control painted past an edge is unreachable, the same defect from the
/// other side.
fn assert_inside_window(
    harness: &Harness<AppState>,
    labels: &[&str],
    size: [u32; 2],
    band: egui::Rect,
) {
    let window = window_rect(size);
    for (label, rect) in rects_of(harness, labels, band) {
        assert!(window.contains_rect(rect), "{label} {rect:?} paints outside {window:?}");
    }
}

/// Whether a node belongs to the row its band names: it lies inside the
/// band. A grid row the panel above clipped at the edge keeps its rect
/// running under the row, and a panel edge can leave a sliver of a row above
/// inside the band; neither is in the row the band names.
fn in_row(rect: egui::Rect, band: egui::Rect) -> bool {
    // The centre line decides: a control taller than the band's anchor (a
    // Small label) still belongs to that row, and containment would drop it.
    rect.center().y >= band.min.y && rect.center().y <= band.max.y
}

/// The text a node carries: a button's label or a plain label's value, so
/// the row's texts and its controls are named the same way.
fn node_text(node: &egui_kittest::Node<'_>) -> String {
    let access = node.accesskit_node();
    access.label().or_else(|| access.value()).unwrap_or_default()
}

/// Every node whose rect shares pixels with `band`, as (text, rect) pairs.
/// egui reports one widget as several nodes with the same rect, so those
/// collapse; a node with no text is a container or a clipped grid row, not
/// chrome the row drew.
fn band_nodes(harness: &Harness<AppState>, band: egui::Rect) -> Vec<(String, egui::Rect)> {
    let mut found: Vec<(String, egui::Rect)> = Vec::new();
    for node in harness.query_all(By::new()) {
        if node.accesskit_node().bounding_box().is_none() {
            continue;
        }
        let text = node_text(&node);
        if text.is_empty() {
            continue;
        }
        let rect = node.rect();
        if !in_row(rect, band) {
            continue;
        }
        if found
            .iter()
            .any(|(seen, at)| *seen == text && same_rect(*at, rect))
        {
            continue;
        }
        found.push((text, rect));
    }
    found
}


/// Whether two rects are one widget reported twice: egui's duplicate nodes
/// for one label differ by a fraction of a pixel.
fn same_rect(a: egui::Rect, b: egui::Rect) -> bool {
    let near = |x: f32, y: f32| (x - y).abs() <= 1.0;
    near(a.min.x, b.min.x)
        && near(a.min.y, b.min.y)
        && near(a.max.x, b.max.x)
        && near(a.max.y, b.max.y)
}

/// Every node the band touches stays inside the window, and no two of them
/// paint over each other. A label a query can name is checked beside this
/// (`assert_inside_window`); the walk is what sees an elided label no query
/// matches.
fn assert_band_clear(harness: &Harness<AppState>, band: egui::Rect, size: [u32; 2]) {
    let window = window_rect(size);
    let nodes = band_nodes(harness, band);
    assert!(!nodes.is_empty(), "the band {band:?} holds a node");
    for (label, rect) in &nodes {
        assert!(
            window.contains_rect(*rect),
            "{label:?} {rect:?} paints outside {window:?}"
        );
    }
    for (i, (a_label, a)) in nodes.iter().enumerate() {
        for (b_label, b) in &nodes[i + 1..] {
            let overlap = a.intersect(*b);
            assert!(
                overlap.width() <= 0.5 || overlap.height() <= 0.5,
                "{a_label:?} {a:?} paints over {b_label:?} {b:?} in {band:?}"
            );
        }
    }
}

/// Three query tabs on one memory database, each with a result in the grid:
/// the setup both narrow tab-strip tests share.
fn three_tabs_at(size: [u32; 2], tag: &str) -> (Scratch, Harness<'static, AppState>) {
    let scratch = Scratch::new(tag);
    let mut harness = build_at(size);
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
    (scratch, harness)
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

/// The tab strip's close buttons, in draw order: the `×` nodes render in the
/// same order as the titles, so one list pairs with the other by index.
fn tab_close_buttons(harness: &Harness<AppState>) -> Vec<egui::Rect> {
    harness.query_all_by_label("×").map(|node| node.rect()).collect()
}

/// Chrome at the width the operator reported, with three tabs, a folder and
/// a result in the grid: status bar, both toolbars, and the tab strip keep
/// their controls inside the window and never paint over each other.
#[test]
fn narrow_window_chrome_stays_clear() {
    let _guard = env_lock();
    let (_scratch, mut harness) = three_tabs_at(NARROW_WINDOW, "narrow");
    capture_at(&mut harness, "narrow-dark", NARROW_WINDOW);

    // One band per chrome row, so a label the sidebar spells too (`Edit`)
    // answers for the row under test.
    let status_band = toolbar_band(&harness, &["Compare", "☀ Light"], NARROW_WINDOW);
    let results_band = toolbar_band(&harness, &["Filters", "Copy"], NARROW_WINDOW);
    // Both editor rows: the run controls sit on the first, the tools on the
    // second when the row wraps.
    let editor_band = toolbar_band(&harness, &["▶ Run", "Save"], NARROW_WINDOW);

    assert_no_overlap(
        &harness,
        &["1 connection, 1 live", "local · SQLite", "Compare", "+ Table", "☀ Light"],
        status_band,
    );
    assert_inside_window(
        &harness,
        &["Compare", "+ Table", "☀ Light"],
        NARROW_WINDOW,
        status_band,
    );
    assert_no_overlap(
        &harness,
        &["Filters", "Copy", "Export", "Edit", "Detail", "Chart"],
        results_band,
    );
    assert_inside_window(
        &harness,
        &["Filters", "Copy", "Export", "Edit", "Detail", "Chart"],
        NARROW_WINDOW,
        results_band,
    );
    assert_no_overlap(
        &harness,
        &["▶ Run", "Explain", "Save", "Transaction", "Complete", "Format", "Find"],
        editor_band,
    );
    assert_inside_window(
        &harness,
        &["▶ Run", "Explain", "Save", "Transaction", "Complete", "Format", "Find"],
        NARROW_WINDOW,
        editor_band,
    );

    // The status bar's own band: its labels elide to text no query here
    // matches, so the band is walked whole.
    assert_band_clear(&harness, status_band, NARROW_WINDOW);

    // Every control is inline or behind the row's one `…`, and the row stays
    // in the window: a control that vanished would pass the checks above.
    assert_row_reachable(
        &mut harness,
        &["Compare"],
        &["Compare", "+ Table", "☀ Light"],
        NARROW_WINDOW,
    );
    assert_row_reachable(
        &mut harness,
        &["Copy"],
        &["Copy", "Export", "Edit", "Detail", "Chart"],
        NARROW_WINDOW,
    );
    assert_row_reachable(
        &mut harness,
        &["▶ Run", "Save"],
        &["▶ Run", "Explain", "Save", "Transaction", "Complete", "Format", "Find"],
        NARROW_WINDOW,
    );

    // The results row's tail: opening the `…` shows its items inside the
    // window, and a tail toggle both opens the pane and closes the menu.
    let overflow = harness
        .query_all_by_label("…")
        .find(|node| results_band.intersects(node.rect()))
        .expect("the results row's overflow button");
    overflow.click();
    harness.run_steps(6);
    let tail = harness
        .query_by_label("Chart")
        .expect("the tail holds Chart at this width")
        .rect();
    assert!(window_rect(NARROW_WINDOW).contains_rect(tail), "the tail's Chart {tail:?} leaves the window");
    harness.get_by_label("Chart").click();
    harness.run_steps(6);
    assert!(harness.query_by_label("Kind").is_some(), "the chart pane opened from the tail");
    // The popup lays out below the row, so the row's band cannot see whether
    // it closed. The item's own rect can: nothing may occupy it any more.
    assert!(
        !harness
            .query_all_by_label("Chart")
            .any(|node| node.rect().intersects(tail)),
        "a tail tick closes the menu it fired from"
    );

    // The editor row's tail closes its menu the same way. Format is not the
    // item clicked, so its popup node is the witness that the menu is gone.
    harness
        .query_all_by_label("…")
        .find(|node| editor_band.intersects(node.rect()))
        .expect("the editor row's overflow button")
        .click();
    harness.run_steps(6);
    let format = harness.get_by_label("Format").rect();
    harness.get_by_label("Find").click();
    harness.run_steps(6);
    assert!(
        !harness
            .query_all_by_label("Format")
            .any(|node| node.rect().intersects(format)),
        "an editor tail tick closes its menu too"
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

/// One row over `db::ROW_CAP`, so the result comes back truncated and the
/// footer grows its paging controls.
const TRUNCATED_SQL: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c \
                              WHERE x < 10001) SELECT x FROM c;";

/// Hide one column with the grid's own shortcut: click a cell of the column,
/// then press the hide chord, so the footer's `Show columns` control comes up.
/// The cell is the match right of the sidebar: a folder badge can carry the
/// same number.
fn hide_column(harness: &mut Harness<AppState>, cell: &str) {
    let node = harness
        .query_all_by_label(cell)
        .max_by(|a, b| a.rect().min.x.total_cmp(&b.rect().min.x))
        .unwrap_or_else(|| panic!("the grid cell {cell:?} renders"));
    node.click();
    harness.run_steps(4);
    harness.key_press_modifiers(egui::Modifiers::CTRL, egui::Key::H);
    harness.run_steps(4);
}

/// The truncated-result footer fixture of fix round 2 at one size: three tabs,
/// the active tab's result truncated at the row cap with one column hidden,
/// and a successful export notice set directly on the tab, so no export runs.
fn truncated_footer_at(
    size: [u32; 2],
    tag: &str,
) -> (Scratch, Harness<'static, AppState>, TabId) {
    let (scratch, mut harness) = three_tabs_at(size, tag);
    let tab = harness.state().active().expect("the active tab");
    run_sql(&mut harness, tab, TRUNCATED_SQL);
    assert!(
        harness
            .state()
            .tab(tab)
            .and_then(|t| t.active_query_result())
            .is_some_and(|result| result.is_truncated()),
        "the recursive query fills the row cap"
    );
    hide_column(&mut harness, "1");
    harness
        .state_mut()
        .tab_mut(tab)
        .expect("the active tab")
        .set_export_notice("Copied 10000 rows as TSV".to_owned(), false);
    harness.run();
    (scratch, harness, tab)
}

/// The grid footer at the reported width, with a truncated result, one hidden
/// column, and an export notice on it: the truncation flag keeps its place,
/// every control is inline or behind the row's one `…`, and nothing in the
/// row paints over a neighbor or past the window edge.
#[test]
fn truncated_footer_band_stays_clear() {
    let _guard = env_lock();
    let (_scratch, mut harness, tab) = truncated_footer_at(NARROW_WINDOW, "footer");
    capture_at(&mut harness, "footer-truncated-dark", NARROW_WINDOW);

    let band = band_of(&harness, "Count total", NARROW_WINDOW);
    assert_band_clear(&harness, band, NARROW_WINDOW);
    let mut drawn: Vec<String> =
        band_nodes(&harness, band).into_iter().map(|(label, _)| label).collect();
    drawn.sort();
    println!("footer band labels: {drawn:?}");
    assert!(
        drawn.iter().any(|label| label.starts_with("truncated at 10,000")),
        "the truncation flag keeps its place: {drawn:?}"
    );
    // Every expected control is inline or behind the row's one `…`.
    assert_row_reachable(
        &mut harness,
        &["Count total"],
        &["Count total", "Load more (10000 loaded)", "Show columns (1)"],
        NARROW_WINDOW,
    );
    // `Show columns` from the tail is the only gesture that brings the hidden
    // column back at this width.
    harness
        .query_all_by_label("…")
        .find(|node| band.intersects(node.rect()))
        .expect("the footer's overflow button")
        .click();
    harness.run_steps(6);
    harness.get_by_label("Show columns (1)").click();
    harness.run_steps(6);
    assert!(
        harness.query_by_label("x").is_some(),
        "the tail's Show columns restored the hidden column"
    );

    // Without the truncation the row has room for its texts: the counts and
    // the tab's own state keep their whole labels, and the success notice,
    // reserved last, is the one that gives up width.
    run_sql(&mut harness, tab, "SELECT personid FROM persons;");
    harness
        .state_mut()
        .tab_mut(tab)
        .expect("the active tab")
        .set_export_notice("Copied 10000 rows as TSV".to_owned(), false);
    harness.run();

    let band = band_of(&harness, "editing persons", NARROW_WINDOW);
    assert_band_clear(&harness, band, NARROW_WINDOW);
    let mut drawn: Vec<String> =
        band_nodes(&harness, band).into_iter().map(|(label, _)| label).collect();
    drawn.sort();
    println!("plain footer band labels: {drawn:?}");
    assert!(
        drawn.iter().any(|label| label == "1 of 1 rows · 0 ms"),
        "the counts keep their whole label: {drawn:?}"
    );
    assert!(
        drawn.iter().any(|label| label.starts_with("Copied") && label.ends_with('…')),
        "the success notice elides first: {drawn:?}"
    );

    // A failed export outranks the counts: its notice keeps its whole label
    // while the counts give up theirs.
    harness
        .state_mut()
        .tab_mut(tab)
        .expect("the active tab")
        .set_export_notice("Export failed: no space left on device".to_owned(), true);
    harness.run();
    let band = band_of(&harness, "Export failed: no space left on device", NARROW_WINDOW);
    assert_band_clear(&harness, band, NARROW_WINDOW);
    let mut drawn: Vec<String> =
        band_nodes(&harness, band).into_iter().map(|(label, _)| label).collect();
    drawn.sort();
    println!("failed footer band labels: {drawn:?}");
    assert!(
        drawn.iter().any(|label| label == "Export failed: no space left on device"),
        "the failed notice keeps its whole label: {drawn:?}"
    );
    assert!(
        drawn.iter().any(|label| label.starts_with('1') && label.ends_with('…')),
        "the counts elide behind the failed notice: {drawn:?}"
    );
}

/// The same fixture a little wider, inside the band the reviewer named
/// (660-710): the paging controls still fit, and the row's leftover is real.
/// Every text elides into that leftover; elision must give each text only
/// what it reserved, or the four of them paint over each other and past the
/// window edge.
#[test]
fn truncated_footer_mid_width_stays_clear() {
    const MID_WINDOW: [u32; 2] = [700, 640];
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = truncated_footer_at(MID_WINDOW, "footer-mid");
    capture_at(&mut harness, "footer-mid-width-dark", MID_WINDOW);

    let band = band_of(&harness, "Count total", MID_WINDOW);
    let mut drawn: Vec<String> =
        band_nodes(&harness, band).into_iter().map(|(label, _)| label).collect();
    drawn.sort();
    println!("mid footer band labels: {drawn:?}");
    assert_band_clear(&harness, band, MID_WINDOW);
}

/// The editor row at about 670px of row width and the results row at about
/// 490px: the band where the old rule wrapped before the hint or the filter
/// field had elided. With the hint out of the one-row decision the editor
/// tools stay inline and the hint elides; the results row sizes the field
/// from the leftover, clamped to its own range, so it keeps one row too.
#[test]
fn toolbars_elide_before_wrapping() {
    const EDITOR_ROW: [u32; 2] = [1024, 700];
    const RESULTS_ROW: [u32; 2] = [840, 700];
    let _guard = env_lock();

    // The editor tools share one line with the run controls, no tail, and
    // the hint is cut rather than pushed onto a second row.
    let (_scratch, mut harness) = three_tabs_at(EDITOR_ROW, "editor-elide");
    capture_at(&mut harness, "editor-row-elided-dark", EDITOR_ROW);
    let editor_band = toolbar_band(&harness, &["▶ Run"], EDITOR_ROW);
    assert_band_clear(&harness, editor_band, EDITOR_ROW);
    assert_same_row(
        &harness,
        &["▶ Run", "Explain", "Save", "Transaction", "Complete", "Format", "Find"],
        editor_band,
    );
    // The hint elides into the row: its label keeps the head of the scope
    // text and stays narrower than the whole "all statements · Ctrl+Enter".
    let hint = band_nodes(&harness, editor_band)
        .into_iter()
        .find(|(text, _)| text.starts_with("all"))
        .expect("the hint stays on the editor row, elided");
    println!("editor row: hint {hint:?} band {editor_band:?}");
    assert!(hint.1.width() < 120.0, "the hint is elided, not whole: {hint:?}");

    // The results row: one line, the actions inline, and the filter field
    // narrower than its full width.
    let (_scratch, mut harness) = three_tabs_at(RESULTS_ROW, "results-elide");
    capture_at(&mut harness, "results-row-elided-dark", RESULTS_ROW);
    let results_band = toolbar_band(&harness, &["Copy", "Filters"], RESULTS_ROW);
    assert_band_clear(&harness, results_band, RESULTS_ROW);
    assert_eq!(
        overflow_buttons_in(&harness, results_band),
        0,
        "the one-row results toolbar shows no `…`"
    );
    assert_same_row(
        &harness,
        &["Copy", "Export", "Edit", "Detail", "Chart"],
        results_band,
    );
    let field = filter_field(&harness);
    println!("results row: field {field:?}");
    assert!(
        field.width() < FIELD_W && field.width() >= 64.0,
        "the field is sized from the leftover, clamped to its own range: {field:?}"
    );
}

/// At the floor width the editor toolbar overflows its panel: the row's own
/// controls clip by design, but the tools row must keep its one `…` inside
/// the window. The budget must measure the panel, not the width egui grew for
/// the row above it, or the `…` is laid out past the edge and lost.
#[test]
fn the_editor_overflow_stays_reachable_at_the_floor_width() {
    const FLOOR: [u32; 2] = [460, 640];
    let _guard = env_lock();
    let (_scratch, mut harness) = three_tabs_at(FLOOR, "editor-floor");
    capture_at(&mut harness, "editor-floor-dark", FLOOR);

    let band = toolbar_band(&harness, &["▶ Run", "Save"], FLOOR);
    let overflow: Vec<egui::Rect> = harness
        .query_all_by_label("…")
        .map(|node| node.rect())
        .filter(|rect| band.intersects(*rect))
        .collect();
    assert_eq!(overflow.len(), 1, "the overflowing tools row keeps one overflow button");
    assert!(
        window_rect(FLOOR).contains_rect(overflow[0]),
        "the overflow button {} leaves the window",
        overflow[0]
    );
}

/// The tab strip at 460: each tab's close button shares its title's row and
/// stays inside the window. A close button that wraps away from its title
/// sits beside the next tab's title, and closing the wrong tab drops that
/// tab's SQL and params with no confirmation.
#[test]
fn tab_close_buttons_stay_with_their_titles() {
    const STRIP: [u32; 2] = [460, 640];
    let _guard = env_lock();
    let (_scratch, mut harness) = three_tabs_at(STRIP, "tab-close");
    capture_at(&mut harness, "tabs-narrow-dark", STRIP);

    let window = window_rect(STRIP);
    let closes = tab_close_buttons(&harness);
    let titles = ["persons", "persons 2", "persons 3"];
    assert_eq!(closes.len(), titles.len(), "one close button per tab: {closes:?}");
    for (title, close) in titles.iter().zip(&closes) {
        let title_rect = harness.get_by_label(title).rect();
        println!("{title}: title {title_rect:?} close {close:?}");
        assert!(
            title_rect.min.y <= close.max.y && close.min.y <= title_rect.max.y,
            "{title} {title_rect:?} and its close button {close:?} sit on different rows"
        );
        assert!(
            window.contains_rect(*close),
            "{title}'s close button {close:?} paints outside {window:?}"
        );
    }
}

/// Every listed node in `band` shares one y band: one row, no wrap.
fn assert_same_row(harness: &Harness<AppState>, labels: &[&str], band: egui::Rect) {
    let found = rects_of(harness, labels, band);
    assert_eq!(found.len(), labels.len(), "every row node renders: {found:?}");
    let (first_label, first) = &found[0];
    for (label, rect) in &found[1..] {
        assert!(
            (rect.center().y - first.center().y).abs() <= 1.0,
            "{label} {rect:?} left the row {first_label} {first:?} holds"
        );
    }
}

/// The full-width strip of window one toolbar occupies, taken from a node it
/// holds: the strip runs from that node's top edge to its bottom edge across
/// the whole window, so a node in any column of the same row lands in it.
fn band_of(harness: &Harness<AppState>, label: &str, [w, _]: [u32; 2]) -> egui::Rect {
    let node = harness.get_by_label(label).rect();
    egui::Rect::from_min_max(
        egui::pos2(0.0, node.min.y),
        egui::pos2(w as f32, node.max.y),
    )
}

/// How many overflow-menu buttons rendered inside `band`.
fn overflow_buttons_in(harness: &Harness<AppState>, band: egui::Rect) -> usize {
    harness
        .query_all_by_label("…")
        .filter(|node| band.intersects(node.rect()))
        .count()
}

/// The strip a toolbar occupies, across the whole window: from the top of its
/// first line's anchor to the bottom of its last. A one-line toolbar's two
/// anchors share a line, so the strip is that line.
fn toolbar_band(harness: &Harness<AppState>, anchors: &[&str], [w, _]: [u32; 2]) -> egui::Rect {
    let mut band: Option<egui::Rect> = None;
    for anchor in anchors {
        let node = harness
            .query_by_label(anchor)
            .unwrap_or_else(|| panic!("the toolbar anchor {anchor:?} renders"))
            .rect();
        let strip = egui::Rect::from_min_max(
            egui::pos2(0.0, node.min.y),
            egui::pos2(w as f32, node.max.y),
        );
        band = Some(match band {
            Some(band) => band.union(strip),
            None => strip,
        });
    }
    band.expect("at least one anchor")
}

/// Every expected control of one toolbar is inline in its band or inside the
/// toolbar's one `…` menu, which is opened and closed here; every node the
/// band touches stays inside the window. A label that rendered nowhere would
/// pass the frame's other assertions silently, so this one names it.
fn assert_row_reachable(
    harness: &mut Harness<'static, AppState>,
    anchors: &[&str],
    expected: &[&str],
    size: [u32; 2],
) {
    let window = window_rect(size);
    let band = toolbar_band(harness, anchors, size);
    for node in harness.query_all(By::new()) {
        if node.accesskit_node().bounding_box().is_none() {
            continue;
        }
        let rect = node.rect();
        if band.intersects(rect) {
            assert!(
                window.contains_rect(rect),
                "{:?} {rect:?} paints outside {window:?}",
                node.accesskit_node().label()
            );
        }
    }

    let missing: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|label| label_in_band(harness, label, band).is_none())
        .collect();
    let inline_overflow: Vec<egui::Rect> = harness
        .query_all_by_label("…")
        .map(|node| node.rect())
        .filter(|rect| band.intersects(*rect))
        .collect();
    if missing.is_empty() {
        assert!(inline_overflow.is_empty(), "a row with no tail shows no `…`");
        return;
    }
    // The menu draws a node for each tail label; a label the sidebar spells
    // too (`Edit`) is matched by the node that was not there before it opened.
    let before: Vec<(&str, Vec<egui::Rect>)> = missing
        .iter()
        .map(|label| {
            (*label, harness.query_all_by_label(label).map(|node| node.rect()).collect())
        })
        .collect();
    assert_eq!(inline_overflow.len(), 1, "the tail hides behind one `…`");
    harness
        .query_all_by_label("…")
        .find(|node| band.intersects(node.rect()))
        .expect("the row's overflow button")
        .click();
    harness.run_steps(6);
    for (label, before) in &before {
        let rect = harness
            .query_all_by_label(label)
            .map(|node| node.rect())
            .find(|rect| !before.contains(rect))
            .unwrap_or_else(|| {
                panic!("{label} is drawn nowhere: not inline, and not inside the row's menu")
            });
        assert!(
            window.contains_rect(rect),
            "{label} {rect:?} is reachable but leaves {window:?}"
        );
    }
    harness.key_press(egui::Key::Escape);
    harness.run_steps(4);
    // The menu's nodes sit below the band, so re-checking the band says
    // nothing. Diff against the snapshot instead: every node for the label
    // must be one the row already had, or the popup is still up.
    let (first_label, first_before) = before.first().expect("at least one tail label");
    assert!(
        harness
            .query_all_by_label(first_label)
            .all(|node| first_before.contains(&node.rect())),
        "Escape closes the menu holding {first_label}"
    );
}

/// The results toolbar's row filter field. A text edit carries no label, so
/// the field is the text input immediately left of the Filters toggle.
fn filter_field(harness: &Harness<AppState>) -> egui::Rect {
    let filters = harness.get_by_label("Filters").rect();
    harness
        .query_all_by_role(egui::accesskit::Role::TextInput)
        .map(|node| node.rect())
        .filter(|rect| {
            rect.max.x <= filters.min.x
                && rect.max.y >= filters.min.y
                && rect.min.y <= filters.max.y
        })
        .max_by(|a, b| a.min.x.total_cmp(&b.min.x))
        .expect("the row filter field")
}

/// The width the toolbar's text field keeps at a wide window. The node's
/// rect is the field's text area, which is the desired width exactly.
const FIELD_W: f32 = 180.0;

/// At a wide window every chrome row keeps today's single-row shape: nothing
/// folds behind an overflow menu, the results actions stay right-aligned, the
/// filter field keeps its full width, and the editor tools and the tabs each
/// share one row.
#[test]
fn wide_layout_unchanged() {
    let _guard = env_lock();
    let _scratch = Scratch::new("wide");
    let mut harness = build_at(WIDE_WINDOW);
    let conn = add_memory_database(harness.state_mut());
    connect_and_wait(&mut harness, conn);
    let first = harness.state_mut().new_tab(conn);
    harness.state_mut().activate(first);
    run_sql(&mut harness, first, demo_sql());
    let mut tabs = vec![first];
    for _ in 0..2 {
        let tab = harness.state_mut().new_tab(conn);
        harness.state_mut().activate(tab);
        tabs.push(tab);
    }
    for tab in &tabs {
        run_sql(
            &mut harness,
            *tab,
            "SELECT id, email, plan, active, joined FROM users ORDER BY id;",
        );
    }
    harness.state_mut().activate(first);
    harness.run();
    capture_at(&mut harness, "wide-chrome-dark", WIDE_WINDOW);

    // Both status-bar labels fit at this width.
    assert!(
        harness.query_by_label("1 connection, 1 live").is_some(),
        "the status bar keeps its stats label"
    );
    assert!(
        harness.query_by_label("local · SQLite").is_some(),
        "the status bar keeps its backend label"
    );

    // No row folded anything behind the overflow menu.
    for (name, anchor) in [
        ("status bar", "Compare"),
        ("results toolbar", "Chart"),
        ("editor toolbar", "▶ Run"),
    ] {
        let band = band_of(&harness, anchor, WIDE_WINDOW);
        assert!(band.height() > 0.0, "the {name} band is real");
        assert_eq!(
            overflow_buttons_in(&harness, band),
            0,
            "the {name} shows an overflow menu at 1440"
        );
    }

    // The results actions sit at the panel's right edge, reading left to
    // right as they always have, and the filter field keeps its width.
    let window = window_rect(WIDE_WINDOW);
    let chart = harness.get_by_label("Chart").rect();
    let gap = window.right() - chart.max.x;
    println!("wide: the action group ends {gap:.1}px from the window's right edge");
    assert!(gap <= 16.0, "the action group {chart:?} left the panel's right edge");
    let results_band = band_of(&harness, "Chart", WIDE_WINDOW);
    let mut previous = f32::NEG_INFINITY;
    for label in ["Copy", "Export", "Edit", "Detail", "Chart"] {
        let rect = label_in_band(&harness, label, results_band)
            .unwrap_or_else(|| panic!("{label} renders in the results row"));
        assert!(rect.min.x > previous, "{label} {rect:?} left the group's order");
        previous = rect.min.x;
    }
    let field = filter_field(&harness);
    println!("wide: filter field {field:?}");
    assert!(
        (field.width() - FIELD_W).abs() <= 1.0,
        "the filter field is {} wide, not {FIELD_W}",
        field.width()
    );

    // The editor tools and the tabs each hold one row.
    let editor_band = band_of(&harness, "▶ Run", WIDE_WINDOW);
    assert_same_row(
        &harness,
        &["▶ Run", "Explain", "Save", "Transaction", "Complete", "Format", "Find"],
        editor_band,
    );
    let strip_band = band_of(&harness, "users", WIDE_WINDOW);
    assert_same_row(&harness, &["users", "users 2", "users 3"], strip_band);
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

/// The params prompt: the first field takes focus, so typing lands there,
/// and Enter runs the substituted batch.
#[test]
fn params_prompt_focuses_the_first_field_and_enter_runs() {
    let _guard = env_lock();
    let (_scratch, mut harness, tab) = demo_setup("params-keys");
    harness.state_mut().start_run(tab, "SELECT :a AS v", 0, None);
    assert!(harness.state().pending_params().is_some(), "the prompt holds the run");
    harness.run();

    harness.event(egui::Event::Text("7".to_owned()));
    harness.run();
    assert_eq!(
        harness.state().pending_params().map(|pending| pending.params[0].value.as_str()),
        Some("7"),
        "typing lands in the first parameter field"
    );
    capture(&mut harness, "params-prompt-dark");

    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(harness.state().pending_params().is_none(), "Enter runs the batch");
    step_until(&mut harness, "the substituted run", |state| {
        !matches!(state.tab(tab).map(|t| t.outcome()), Some(RunOutcome::Running { .. }))
    });
    assert!(
        matches!(harness.state().tab(tab).map(|t| t.outcome()), Some(RunOutcome::Ok { .. })),
        "the substituted SELECT ran: {:?}",
        harness.state().tab(tab).map(|t| t.outcome())
    );
}

/// The write guard: Enter never approves; its Cancel focus cancels instead.
/// Escape cancels the re-armed guard and sends nothing.
#[test]
fn without_where_confirm_never_approves_on_enter_and_cancels_on_escape() {
    let _guard = env_lock();
    let (_scratch, mut harness, tab) = demo_setup("write-confirm-keys");
    harness.state_mut().start_run(tab, "DELETE FROM users", 0, None);
    assert!(harness.state().pending_write().is_some(), "the guard holds the delete");
    harness.run();
    capture(&mut harness, "write-confirm-dark");

    harness.key_press(egui::Key::Enter);
    harness.run();
    let approved = harness.state().pending_write().is_none()
        && matches!(
            harness.state().tab(tab).map(|tab| tab.outcome()),
            Some(RunOutcome::Running { .. })
        );
    assert!(!approved, "a stray Enter never approves a DELETE without WHERE");

    harness.state_mut().start_run(tab, "DELETE FROM users", 0, None);
    harness.run();
    assert!(harness.state().pending_write().is_some(), "the guard re-arms");
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(harness.state().pending_write().is_none(), "Escape cancels the guard");
    assert!(
        matches!(harness.state().tab(tab).map(|tab| tab.outcome()), Some(RunOutcome::Ok { .. })),
        "nothing was sent on cancel"
    );
}

/// The password prompt: its field takes focus on the first frame, so typing
/// lands in the hidden field, and Escape cancels without connecting.
#[test]
fn password_prompt_focuses_the_field_and_escape_cancels() {
    let _guard = env_lock();
    let _scratch = Scratch::new("password-keys");
    let mut harness = build();
    let conn = add_postgres_draft(harness.state_mut());
    harness.state_mut().open_password_prompt(conn);
    assert_eq!(harness.state().password_prompt(), Some(conn), "the prompt arms");
    harness.run();

    harness.event(egui::Event::Text("hunter2".to_owned()));
    harness.run();
    assert_eq!(
        harness.state().prompt_password(),
        "hunter2",
        "typing lands in the focused password field"
    );

    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(harness.state().password_prompt().is_none(), "Escape cancels the prompt");
}

/// The tab chords: Ctrl+Tab and Ctrl+PageDown step the strip, Ctrl+1..9
/// jump, and none of them lands as a tab character in the focused editor.
#[test]
fn tab_chords_switch_tabs_without_typing_into_the_editor() {
    let _guard = env_lock();
    let (_scratch, mut harness, first) = demo_setup("tab-chords");
    let conn = harness.state().tab(first).and_then(|tab| tab.conn).expect("a connection");
    let second = harness.state_mut().new_tab(conn);
    let third = harness.state_mut().new_tab(conn);
    harness.state_mut().activate(first);
    harness.ctx.memory_mut(|memory| memory.request_focus(editor_id(first)));
    harness.run();
    assert_eq!(
        harness.ctx.memory(|memory| memory.focused()),
        Some(editor_id(first)),
        "the SQL editor holds focus"
    );
    let first_sql = harness.state().tab(first).expect("first tab").sql.clone();

    harness.key_press_modifiers(egui::Modifiers::CTRL, egui::Key::Tab);
    harness.run();
    assert_eq!(harness.state().active(), Some(second), "Ctrl+Tab steps forward");
    assert_eq!(
        harness.state().tab(first).expect("first tab").sql,
        first_sql,
        "Ctrl+Tab inserted no tab character"
    );

    harness.ctx.memory_mut(|memory| memory.request_focus(editor_id(second)));
    harness.run();
    let second_sql = harness.state().tab(second).expect("second tab").sql.clone();
    harness.key_press_modifiers(
        egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
        egui::Key::Tab,
    );
    harness.run();
    assert_eq!(harness.state().active(), Some(first), "Ctrl+Shift+Tab steps back");
    assert_eq!(
        harness.state().tab(second).expect("second tab").sql,
        second_sql,
        "Ctrl+Shift+Tab inserted no tab character"
    );

    harness.key_press_modifiers(egui::Modifiers::CTRL, egui::Key::PageDown);
    harness.run();
    assert_eq!(harness.state().active(), Some(second), "Ctrl+PageDown steps forward");
    harness.key_press_modifiers(egui::Modifiers::CTRL, egui::Key::PageUp);
    harness.run();
    assert_eq!(harness.state().active(), Some(first), "Ctrl+PageUp steps back");

    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Num3);
    harness.run();
    assert_eq!(harness.state().active(), Some(third), "Cmd/Ctrl+3 jumps to the third tab");
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Num9);
    harness.run();
    assert_eq!(harness.state().active(), Some(third), "9 selects the last tab");
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Num1);
    harness.run();
    assert_eq!(harness.state().active(), Some(first), "1 selects the first tab");
}

/// The find bar's keys: Enter steps to the next match and Shift+Enter to the
/// previous, the caret stays in the find field, and Escape closes the bar and
/// hands the caret back to the editor.
#[test]
fn find_bar_steps_matches_with_enter_and_returns_focus_on_escape() {
    let _guard = env_lock();
    let (_scratch, mut harness, tab) = demo_setup("find-keys");
    let sql = "SELECT 1;\nSELECT 2;";
    *harness.state_mut().tab_sql_mut(tab).expect("the tab's sql") = sql.to_owned();
    harness.ctx.memory_mut(|memory| memory.request_focus(editor_id(tab)));
    harness.run();

    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::F);
    harness.run();
    harness.event(egui::Event::Text("SELECT".to_owned()));
    harness.run();
    assert!(harness.query_by_label("0/2").is_some(), "the query finds both statements");

    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(harness.query_by_label("1/2").is_some(), "Enter moves to the first match");
    assert_ne!(
        harness.ctx.memory(|memory| memory.focused()),
        Some(editor_id(tab)),
        "Enter keeps the caret in the find field"
    );

    harness.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::Enter);
    harness.run();
    assert!(
        harness.query_by_label("2/2").is_some(),
        "Shift+Enter wraps to the previous match"
    );

    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(harness.query_by_label("2/2").is_none(), "Escape closes the bar");
    assert_eq!(
        harness.ctx.memory(|memory| memory.focused()),
        Some(editor_id(tab)),
        "Escape returns the caret to the editor"
    );
    // egui collapses a selection set while the editor is unfocused to the
    // selection's end; stepping still parks the editor caret on the match.
    assert_eq!(
        crusty_data_explorer::ui::complete::editor_offsets(&harness.ctx, tab, sql).0,
        16,
        "the caret sits at the end of the stepped-to match"
    );
}

/// A connection saved as production, left unconnected: enough to arm every
/// prod confirmation without touching a server.
fn add_prod_sqlite(state: &mut AppState) -> ConnectionId {
    state.open_draft(None);
    {
        let draft = state.draft_mut().expect("the open draft");
        draft.name = "prod".to_owned();
        draft.env = Environment::Prod;
        draft.config = ConnectionConfig::Sqlite { read_only: false, path: ":memory:".into() };
    }
    state.save_draft();
    state.connections().last().expect("the saved connection").id
}

#[test]
fn every_destructive_confirm_starts_on_cancel_and_tab_enter_never_approves() {
    let _guard = env_lock();
    let (_scratch, mut harness, tab) = demo_setup("confirm-focus");
    let conn = harness.state().tab(tab).and_then(|tab| tab.conn).expect("the tab connection");
    step_until(&mut harness, "the schema", |state| state.schema(conn).is_some());

    harness.state_mut().start_run(tab, "DELETE FROM users", 0, None);
    harness.run();
    assert!(harness.state().pending_write().is_some(), "the write guard arms");
    assert!(
        harness.get_by_label("Cancel").is_focused(),
        "the write guard starts on Cancel"
    );
    harness.key_press(egui::Key::Tab);
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(
        harness.state().pending_write().is_some(),
        "Tab then Enter never approves the write guard"
    );
    harness.key_press(egui::Key::Escape);
    harness.run();

    harness.state_mut().stage_edit(tab, 0, 1, "ada@example.com".to_owned());
    harness.state_mut().apply_edits(tab);
    harness.run();
    assert!(harness.state().pending_edit_preview().is_some(), "the edit preview arms");
    assert!(
        harness.get_by_label("Cancel").is_focused(),
        "the edit preview starts on Cancel"
    );
    harness.key_press(egui::Key::Tab);
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(
        harness.state().pending_edit_preview().is_some(),
        "Tab then Enter never applies the staged edits"
    );
    harness.key_press(egui::Key::Escape);
    harness.run();

    let prod = add_prod_sqlite(harness.state_mut());
    harness.state_mut().connect(prod);
    harness.run();
    assert!(harness.state().pending_prod().is_some(), "the prod connect confirm arms");
    assert!(
        harness.get_by_label("Cancel").is_focused(),
        "the prod connect confirm starts on Cancel"
    );
    harness.key_press(egui::Key::Tab);
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(
        harness.state().pending_prod().is_some(),
        "Tab then Enter never connects to prod"
    );
    harness.key_press(egui::Key::Escape);
    harness.run();

    harness.state_mut().request_terminate(prod, "4242");
    harness.run();
    assert!(harness.state().pending_terminate().is_some(), "the terminate confirm arms");
    assert!(
        harness.get_by_label("Cancel").is_focused(),
        "the terminate confirm starts on Cancel"
    );
    harness.key_press(egui::Key::Tab);
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(
        harness.state().pending_terminate().is_some(),
        "Tab then Enter never terminates the session"
    );
    harness.key_press(egui::Key::Escape);
    harness.run();

    let prod_tab = harness.state_mut().new_tab(prod);
    harness.state_mut().open_table_create(prod);
    {
        let editor = harness.state_mut().table_editor_mut().expect("the table editor");
        editor.form.table = "audit".to_owned();
        editor.form.columns[0].name = "id".to_owned();
        editor.form.columns[0].data_type = "INTEGER".to_owned();
    }
    harness.state_mut().apply_table_ddl(prod_tab);
    harness.run();
    assert!(harness.state().pending_table_ddl().is_some(), "the table DDL confirm arms");
    assert!(
        harness.get_by_label("Cancel").is_focused(),
        "the table DDL confirm starts on Cancel"
    );
    harness.key_press(egui::Key::Tab);
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(
        harness.state().pending_table_ddl().is_some(),
        "Tab then Enter never applies prod DDL"
    );
}

#[test]
fn escape_in_the_find_bar_returns_focus_after_it_moved_away() {
    let _guard = env_lock();
    let (_scratch, mut harness, tab) = demo_setup("find-escape-focus");
    harness.ctx.memory_mut(|memory| memory.request_focus(editor_id(tab)));
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::F);
    harness.run();
    harness.event(egui::Event::Text("SELECT".to_owned()));
    harness.run();
    harness.key_press(egui::Key::Tab);
    harness.run();
    assert!(
        harness.ctx.memory(|memory| memory.focused()) != Some(editor_id(tab)),
        "Tab moved the keyboard off the editor"
    );
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(
        harness.query_by_label("0/2").is_none() && harness.query_by_label("1/2").is_none(),
        "Escape closes the find bar"
    );
    assert_eq!(
        harness.ctx.memory(|memory| memory.focused()),
        Some(editor_id(tab)),
        "closing the find bar hands the caret back to the editor"
    );
}

#[test]
fn a_newer_dialog_takes_escape_from_the_palette() {
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = demo_setup("dispatcher-palette");
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::K);
    harness.run();
    assert!(harness.state().palette_open(), "Ctrl+K opens the palette");
    harness.get_by_label("+ New connection").click();
    harness.run();
    assert!(harness.state().draft().is_some(), "the click opens the draft over the palette");
    assert_eq!(
        harness.state().modal(),
        Some(Modal::Draft),
        "the newest dialog owns the keyboard"
    );
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(
        harness.state().draft().is_none(),
        "Escape closes the frontmost dialog"
    );
    assert!(
        harness.state().palette_open(),
        "the palette underneath keeps its state"
    );
}

#[test]
fn a_params_prompt_armed_over_the_save_prompt_owns_escape() {
    let _guard = env_lock();
    let (_scratch, mut harness, tab) = demo_setup("dispatcher-params");
    harness.state_mut().open_save_prompt();
    harness.run();
    assert!(harness.state().save_prompt().is_some(), "the save prompt is open");
    harness.state_mut().start_run(tab, "SELECT :a AS v", 0, None);
    harness.run();
    assert!(harness.state().pending_params().is_some(), "the params prompt is armed");
    assert_eq!(
        harness.state().modal(),
        Some(Modal::Params),
        "the newer prompt is frontmost"
    );
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(
        harness.state().pending_params().is_none(),
        "Escape cancels the frontmost prompt"
    );
    assert!(
        harness.state().save_prompt().is_some(),
        "the save prompt underneath stays open"
    );
}

#[test]
fn enter_under_the_write_confirm_leaves_the_editor_text_alone() {
    let _guard = env_lock();
    let (_scratch, mut harness, tab) = demo_setup("dialog-editor");
    let sql = "DELETE FROM users";
    *harness.state_mut().tab_sql_mut(tab).expect("the tab sql") = sql.to_owned();
    harness.ctx.memory_mut(|memory| memory.request_focus(editor_id(tab)));
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Enter);
    harness.run();
    assert!(
        harness.state().pending_write().is_some(),
        "the write guard holds the delete"
    );
    let before = harness.state().tab(tab).expect("the tab").sql.clone();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert_eq!(
        harness.state().tab(tab).expect("the tab").sql,
        before,
        "Enter under the dialog never edits the hidden SQL"
    );
}

#[test]
fn tab_then_enter_never_approves_a_write_confirm() {
    let _guard = env_lock();
    let (_scratch, mut harness, tab) = demo_setup("confirm-tab");
    harness.state_mut().start_run(tab, "DELETE FROM users", 0, None);
    harness.run();
    assert!(harness.state().pending_write().is_some(), "the guard holds the delete");
    assert!(
        harness.get_by_label("Cancel").is_focused(),
        "the destructive confirm starts on Cancel"
    );
    harness.key_press(egui::Key::Tab);
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    let approved = harness.state().pending_write().is_none()
        && matches!(
            harness.state().tab(tab).map(|tab| tab.outcome()),
            Some(RunOutcome::Running { .. }) | Some(RunOutcome::Ok { .. })
        );
    assert!(!approved, "Tab then Enter never approves a destructive confirm");
}

#[test]
fn tab_in_the_find_field_moves_focus_without_moving_the_grid_cursor() {
    let _guard = env_lock();
    let (_scratch, mut harness, _tab) = demo_setup("find-tab");
    harness.get_by_label("Detail").click();
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::F);
    harness.run();
    harness.event(egui::Event::Text("SELECT".to_owned()));
    harness.run();
    let before_focus = harness.ctx.memory(|memory| memory.focused());
    assert!(before_focus.is_some(), "the find field holds the keyboard");
    harness.key_press(egui::Key::Tab);
    harness.run();
    assert!(
        harness.query_by_label("Click a cell to inspect its row.").is_some(),
        "Tab from the find field never moves the grid cursor"
    );
    let after_focus = harness.ctx.memory(|memory| memory.focused());
    assert_ne!(after_focus, before_focus, "Tab moves focus through the chrome");
    assert!(after_focus.is_some(), "focus lands somewhere in the chrome");
}

#[test]
fn escape_in_the_draft_closes_an_open_dropdown_before_the_form() {
    let _guard = env_lock();
    let _scratch = Scratch::new("draft-popup");
    let mut harness = build();
    let conn = add_postgres_draft(harness.state_mut());
    harness.state_mut().open_draft(Some(conn));
    harness.run();
    harness.get_by_value("Disable").click();
    harness.run();
    assert!(
        egui::Popup::is_any_open(&harness.ctx),
        "the SSL dropdown is open"
    );
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(
        harness.state().draft().is_some(),
        "Escape closes the dropdown, not the connection form"
    );
    assert!(
        !egui::Popup::is_any_open(&harness.ctx),
        "the dropdown closed"
    );
}

#[test]
fn enter_on_cancel_does_not_submit_the_password_prompt() {
    let _guard = env_lock();
    let _scratch = Scratch::new("password-cancel-enter");
    let mut harness = build();
    let conn = add_postgres_draft(harness.state_mut());
    harness.state_mut().open_password_prompt(conn);
    harness.run();
    harness.key_press(egui::Key::Tab);
    harness.run();
    harness.key_press(egui::Key::Tab);
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(
        harness.state().password_prompt().is_some(),
        "Enter on Cancel never submits the password"
    );
}

#[test]
fn enter_on_cancel_does_not_submit_the_params_prompt() {
    let _guard = env_lock();
    let (_scratch, mut harness, tab) = demo_setup("params-cancel-enter");
    harness.state_mut().start_run(tab, "SELECT :a AS v", 0, None);
    harness.run();
    assert!(harness.state().pending_params().is_some(), "the prompt is armed");
    harness.key_press(egui::Key::Tab);
    harness.run();
    harness.key_press(egui::Key::Tab);
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert!(
        harness.state().pending_params().is_some(),
        "Enter on Cancel never submits the parameters"
    );
}
