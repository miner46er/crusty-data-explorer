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
    rect.min.y >= band.min.y - 1.0 && rect.max.y <= band.max.y + 1.0
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
    let editor_band = toolbar_band(&harness, &["▶ Run"], NARROW_WINDOW);

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
    assert!(
        !harness
            .query_all_by_label("Chart")
            .any(|node| results_band.intersects(node.rect())),
        "a tail tick closes the menu it fired from"
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

/// The grid footer at the reported width, with a truncated result and an
/// export notice on it: the paging controls keep their place, the counts and
/// the tab's own state elide into what is left, and nothing in the row paints
/// over a neighbor or past the window edge.
#[test]
fn truncated_footer_band_stays_clear() {
    let _guard = env_lock();
    let (_scratch, mut harness) = three_tabs_at(NARROW_WINDOW, "footer");
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
    // Set on the tab, so no export runs for the notice.
    harness
        .state_mut()
        .tab_mut(tab)
        .expect("the active tab")
        .set_export_notice("Copied 10000 rows as TSV".to_owned(), false);
    harness.run();
    capture_at(&mut harness, "footer-truncated-dark", NARROW_WINDOW);

    let band = band_of(&harness, "Count total", NARROW_WINDOW);
    assert_band_clear(&harness, band, NARROW_WINDOW);
    // The labels the row renders: both paging buttons, measured from the
    // tab's own state (the paging label carries the loaded count). The
    // counts, the truncation flag, and the notice have no room left and are
    // dropped rather than drawn over the buttons.
    let mut drawn: Vec<String> =
        band_nodes(&harness, band).into_iter().map(|(label, _)| label).collect();
    drawn.sort();
    println!("footer band labels: {drawn:?}");
    assert!(drawn.iter().any(|label| label == "Count total"), "the count button renders");
    assert!(
        drawn.iter().any(|label| label == "Load more (10000 loaded)"),
        "the paging button carries the loaded count: {drawn:?}"
    );

    // Without the truncation the row has room for its texts: the counts elide
    // into what the tab's own state leaves, and the notice keeps its place.
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
        drawn.iter().any(|label| label.starts_with("1 of") && label.ends_with('…')),
        "the counts elide into their share: {drawn:?}"
    );
    assert!(
        drawn.iter().any(|label| label == "Copied 10000 rows as TSV"),
        "the notice keeps its whole label at the row's right edge: {drawn:?}"
    );
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
    let closed = label_in_band(harness, missing[0], band).is_none();
    assert!(closed, "Escape closes the menu holding {}", missing[0]);
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
