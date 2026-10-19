//! Results grid over the active tab's [`RunOutcome`].
//!
//! Only visible rows render, via `TableBody::rows` virtualization.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use std::sync::{
    Arc, Mutex,
    atomic::AtomicBool,
};

use crate::app::{
    AppState, EditTarget, FilterClause, FilterOp, RunOutcome, SavedFilter, SortDir, Tab,
};
use crate::db::{
    CELL_CAP, CancelHandle, ConnectionConfig, DbCommand, DbHandle, QueryResult, RunId,
    DbKind, STREAM_PAGE_SIZE, TabId, Value, is_single_statement,
};
use crate::export::{
    StreamFormat, stream_session_to_file, to_csv, to_in_list, to_inserts, to_json, to_jsonl,
    to_markdown, to_tsv, to_xlsx,
};
use crate::edit::{STAGED_DEFAULT, editable_table};
use crate::import::{ParsedCsv, parse_csv, parse_jsonl, parse_xlsx};

use super::{ModalKey, Palette, autofocus, fit};

/// The row filter's desired width, and the narrowest it shrinks to before
/// the results toolbar wraps to a second row.
const FIELD_W: f32 = 180.0;
const FIELD_MIN: f32 = 64.0;

/// The filter's clear label. The results toolbar measures it before it draws
/// it, so both sides read this.
const CLEAR_LABEL: &str = "Clear";

/// Identity of the result a [`Selection`] was made on. A new run, a result
/// switch, or a shape change carries a new id and clears the selection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ResultId {
    active: usize,
    rows: usize,
    cols: usize,
    took: std::time::Duration,
}

/// Row selection for one result view: the selected result rows plus the
/// active cell cursor. Click selects one row, shift-click extends from the
/// anchor, ctrl-click toggles. Lives in egui temp data keyed by tab, so it
/// needs no `AppState` field, and clears when the result changes.
#[derive(Clone, Default, Debug)]
struct Selection {
    rows: BTreeSet<usize>,
    active: Option<(usize, usize)>,
    anchor: Option<usize>,
    result: Option<ResultId>,
}

impl Selection {
    /// Adopt a new result, dropping the rows when it changed.
    fn sync(&mut self, id: ResultId) {
        if self.result != Some(id) {
            *self = Selection { result: Some(id), ..Selection::default() };
        }
    }
    fn click(&mut self, row: usize, col: usize) {
        self.rows.clear();
        self.rows.insert(row);
        self.active = Some((row, col));
        self.anchor = Some(row);
    }
    fn toggle(&mut self, row: usize, col: usize) {
        if !self.rows.remove(&row) {
            self.rows.insert(row);
        }
        self.active = Some((row, col));
        self.anchor = Some(row);
    }
    /// Extend from the anchor to `row` over the view order, so a sorted view
    /// selects the rows the operator passed rather than a raw index range. An
    /// anchor that is not displayed selects only `row`.
    fn extend(&mut self, row: usize, col: usize, visible: &[usize]) {
        let origin =
            self.anchor.or_else(|| self.active.map(|(row, _)| row)).unwrap_or(row);
        self.rows.clear();
        let (Some(from), Some(to)) = (
            visible.iter().position(|visible_row| *visible_row == origin),
            visible.iter().position(|visible_row| *visible_row == row),
        ) else {
            self.rows.insert(row);
            self.active = Some((row, col));
            return;
        };
        self.rows.extend(visible[from.min(to)..=from.max(to)].iter().copied());
        self.active = Some((row, col));
    }
    /// One cell gesture: ctrl toggles, shift extends, else selects.
    fn gesture(
        &mut self,
        row: usize,
        col: usize,
        shift: bool,
        ctrl: bool,
        visible: &[usize],
    ) {
        if ctrl {
            self.toggle(row, col);
        } else if shift {
            self.extend(row, col, visible);
        } else {
            self.click(row, col);
        }
    }
    fn is_selected(&self, row: usize) -> bool {
        self.rows.contains(&row)
    }
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    fn active(&self) -> Option<(usize, usize)> {
        self.active
    }
    /// Selected rows in `visible` order; the whole view when empty, so every
    /// copy action covers the result set with nothing selected.
    fn resolve(&self, visible: &[usize]) -> Vec<usize> {
        if self.rows.is_empty() {
            return visible.to_vec();
        }
        visible.iter().copied().filter(|row| self.rows.contains(row)).collect()
    }

    /// Where the cursor sits in the view order, when it sits on a displayed
    /// row at all. A filter or sort can leave it off the view.
    fn position(&self, visible: &[usize]) -> Option<usize> {
        let (row, _) = self.active?;
        visible.iter().position(|visible_row| *visible_row == row)
    }

    /// One cursor move over `visible`, the view order: the active cell moves,
    /// edges clamp, and a plain step selects the row it lands on while a
    /// Shift step onto another row extends the row range from the anchor.
    /// `cols` counts the result columns the cursor may occupy, `page` the
    /// rows PageUp and PageDown travel. A step with no cursor on a displayed
    /// row lands on the first displayed row, column 0, and an empty view
    /// moves nothing.
    fn step(&mut self, step: Step, extend: bool, visible: &[usize], cols: usize, page: usize) {
        if visible.is_empty() || cols == 0 {
            return;
        }
        let Some(position) = self.position(visible) else {
            self.click(visible[0], 0);
            return;
        };
        let last = visible.len() - 1;
        let (active_row, active_col) = self.active.expect("a positioned cursor");
        let mut position = position;
        let mut col = active_col.min(cols - 1);
        match step {
            Step::Up => position = position.saturating_sub(1),
            Step::Down => position = (position + 1).min(last),
            Step::Left => col = col.saturating_sub(1),
            Step::Right => col = (col + 1).min(cols - 1),
            Step::PageUp => position = position.saturating_sub(page),
            Step::PageDown => position = (position + page).min(last),
            Step::RowStart => col = 0,
            Step::RowEnd => col = cols - 1,
            Step::First => position = 0,
            Step::Last => position = last,
        }
        let row = visible[position];
        if !extend {
            self.rows.clear();
            self.rows.insert(row);
            self.anchor = Some(row);
            self.active = Some((row, col));
        } else if row != active_row {
            self.extend(row, col, visible);
        } else {
            self.active = Some((row, col));
        }
    }

    /// Tab and Shift+Tab: a column step inside the row, wrapping onto the
    /// next or previous row at the row's ends. The wrap takes two [`Step`]s,
    /// since a single one can only move within a row.
    fn tab_step(&mut self, back: bool, visible: &[usize], cols: usize, page: usize) {
        if visible.is_empty() || cols == 0 {
            return;
        }
        let col = match self.position(visible) {
            Some(_) => self.active.expect("a positioned cursor").1.min(cols - 1),
            None => {
                self.step(Step::Right, false, visible, cols, page);
                return;
            }
        };
        if !back && col + 1 < cols {
            self.step(Step::Right, false, visible, cols, page);
        } else if !back {
            self.step(Step::RowStart, false, visible, cols, page);
            self.step(Step::Down, false, visible, cols, page);
        } else if col > 0 {
            self.step(Step::Left, false, visible, cols, page);
        } else {
            self.step(Step::RowEnd, false, visible, cols, page);
            self.step(Step::Up, false, visible, cols, page);
        }
    }
}

/// One keyboard cursor move over the grid's displayed rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    RowStart,
    RowEnd,
    First,
    Last,
}

fn selection_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_selection", tab))
}

fn selection_of(ctx: &egui::Context, tab: TabId) -> Selection {
    ctx.data(|data| data.get_temp(selection_id(tab))).unwrap_or_default()
}

fn store_selection(ctx: &egui::Context, tab: TabId, selection: &Selection) {
    ctx.data_mut(|data| data.insert_temp(selection_id(tab), selection.clone()));
}

fn clear_selection(ctx: &egui::Context, tab: TabId) {
    ctx.data_mut(|data| {
        data.remove::<Selection>(selection_id(tab));
        data.remove::<bool>(grid_armed_id(tab));
    });
}

/// Drop grid selection for closed tabs. Diffs against the live tab list, the
/// way the find bar forgets its state.
fn prune_selection(state: &AppState, ctx: &egui::Context) {
    let id = egui::Id::new("crusty_selection_tabs");
    let known: Vec<TabId> = ctx.data_mut(|data| data.get_temp(id).unwrap_or_default());
    let live: Vec<TabId> = state.tabs().iter().map(|tab| tab.id).collect();
    for tab in known.iter().filter(|tab| !live.contains(tab)) {
        clear_selection(ctx, *tab);
    }
    if known != live {
        ctx.data_mut(|data| data.insert_temp(id, live));
    }
}

/// Per-tab grid view state: hidden columns and explicit widths. Kept in egui
/// temp data keyed by tab beside the selection, so it lasts the session and
/// dies with the tab. Indices follow the result columns the way `sort_col`
/// does; out-of-range entries are ignored.
#[derive(Clone, Default, Debug, PartialEq)]
struct GridView {
    hidden: BTreeSet<usize>,
    widths: BTreeMap<usize, f32>,
}

/// One keyboard resize step, the clamp explicit widths stay inside, and the
/// fallback an auto-sized column widens from.
const GRID_WIDTH_STEP: f32 = 24.0;
const GRID_MIN_WIDTH: f32 = 48.0;
const GRID_MAX_WIDTH: f32 = 640.0;
const GRID_DEFAULT_WIDTH: f32 = 120.0;

impl GridView {
    fn hide(&mut self, col: usize) {
        self.hidden.insert(col);
    }
    fn show_all(&mut self) {
        self.hidden.clear();
    }
    fn is_hidden(&self, col: usize) -> bool {
        self.hidden.contains(&col)
    }
    /// Result columns on display, in order.
    fn visible(&self, ncols: usize) -> Vec<usize> {
        (0..ncols).filter(|col| !self.is_hidden(*col)).collect()
    }
    /// In-range hidden columns, so stale entries never mislabel the count.
    fn hidden_count(&self, ncols: usize) -> usize {
        self.hidden.iter().filter(|col| **col < ncols).count()
    }
    fn set_width(&mut self, col: usize, width: f32) {
        self.widths.insert(col, width.clamp(GRID_MIN_WIDTH, GRID_MAX_WIDTH));
    }
    fn reset_width(&mut self, col: usize) {
        self.widths.remove(&col);
    }
    fn width(&self, col: usize) -> Option<f32> {
        self.widths.get(&col).copied()
    }
    fn widen(&mut self, col: usize) {
        let width = self.width(col).unwrap_or(GRID_DEFAULT_WIDTH) + GRID_WIDTH_STEP;
        self.set_width(col, width);
    }
    fn narrow(&mut self, col: usize) {
        let width = self.width(col).unwrap_or(GRID_DEFAULT_WIDTH) - GRID_WIDTH_STEP;
        self.set_width(col, width);
    }
}

fn grid_view_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_grid_view", tab))
}

fn grid_view_of(ctx: &egui::Context, tab: TabId) -> GridView {
    ctx.data(|data| data.get_temp(grid_view_id(tab))).unwrap_or_default()
}

fn store_grid_view(ctx: &egui::Context, tab: TabId, view: &GridView) {
    ctx.data_mut(|data| data.insert_temp(grid_view_id(tab), view.clone()));
}

/// Drop grid views for closed tabs, the way [`prune_selection`] forgets its
/// selection state.
fn prune_grid_views(state: &AppState, ctx: &egui::Context) {
    let id = egui::Id::new("crusty_grid_view_tabs");
    let known: Vec<TabId> = ctx.data_mut(|data| data.get_temp(id).unwrap_or_default());
    let live: Vec<TabId> = state.tabs().iter().map(|tab| tab.id).collect();
    for tab in known.iter().filter(|tab| !live.contains(tab)) {
        ctx.data_mut(|data| data.remove::<GridView>(grid_view_id(*tab)));
    }
    if known != live {
        ctx.data_mut(|data| data.insert_temp(id, live));
    }
}

/// The table row index a keyboard step wants on screen. The key handler runs
/// after the table has drawn for the frame and outside its scroll area, so
/// the row is stored here and the next frame's scroll area consumes it.
fn grid_scroll_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_grid_scroll", tab))
}

fn request_grid_scroll(ctx: &egui::Context, tab: TabId, row: usize) {
    ctx.data_mut(|data| data.insert_temp(grid_scroll_id(tab), row));
}

fn take_grid_scroll(ctx: &egui::Context, tab: TabId) -> Option<usize> {
    ctx.data_mut(|data| data.remove_temp::<usize>(grid_scroll_id(tab)))
}

/// Whether the grid, rather than the chrome, owns the keyboard for one tab.
/// Armed by a cell click or a cursor key with nothing else focused; disarmed
/// when any widget takes focus, so Tab never runs the grid behind a field.
fn grid_armed_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_grid_armed", tab))
}

fn grid_armed(ctx: &egui::Context, tab: TabId) -> bool {
    ctx.data(|data| data.get_temp(grid_armed_id(tab))).unwrap_or(false)
}

fn set_grid_armed(ctx: &egui::Context, tab: TabId, armed: bool) {
    ctx.data_mut(|data| data.insert_temp(grid_armed_id(tab), armed));
}

const GRID_ROW_H: f32 = 20.0;

/// The page a PageUp or PageDown moves when the viewport cannot be measured.
const GRID_PAGE_FALLBACK: usize = 10;

fn page_rows(available_height: f32, row_spacing: f32) -> usize {
    if !available_height.is_finite() || available_height <= 0.0 {
        return GRID_PAGE_FALLBACK;
    }
    let row = GRID_ROW_H + row_spacing;
    let body = available_height - row;
    ((body / row).floor() as usize).max(1)
}

/// The worker and connection pieces the grid needs past the tab borrow: a
/// handle to ask for pages, a cancel for the in-flight page, and the config
/// plus secrets to open a background export session.
#[derive(Clone)]
struct GridConn {
    config: ConnectionConfig,
    password: String,
    ssh_password: String,
    handle: Option<DbHandle>,
}

/// One background streamed export. The file writes on its own thread through
/// paged [`stream_session_to_file`]; the grid polls `state` every frame for
/// progress and completion. `cancel` stops between pages.
#[derive(Clone)]
struct ExportJob {
    format: StreamFormat,
    path: std::path::PathBuf,
    state: std::sync::Arc<std::sync::Mutex<JobProgress>>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Clone)]
struct JobProgress {
    rows: u64,
    total: Option<u64>,
    done: bool,
    error: Option<String>,
    cancelled: bool,
    cancel_handle: Option<CancelHandle>,
}

fn export_job_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_export_job", tab))
}

fn export_job_of(ctx: &egui::Context, tab: TabId) -> Option<ExportJob> {
    ctx.data(|data| data.get_temp(export_job_id(tab)))
}

fn store_export_job(ctx: &egui::Context, tab: TabId, job: Option<ExportJob>) {
    ctx.data_mut(|data| match job {
        Some(job) => {
            data.insert_temp(export_job_id(tab), job);
        }
        None => {
            data.remove::<ExportJob>(export_job_id(tab));
        }
    });
}

/// The next paging run id for one tab, so page and count runs never share an
/// id with a run the app tracks.
fn paging_run_id(ctx: &egui::Context, tab: TabId) -> RunId {
    let id = egui::Id::new(("crusty_paging_run", tab));
    let next: u64 = ctx.data(|data| data.get_temp(id)).unwrap_or(1);
    ctx.data_mut(|data| data.insert_temp(id, next + 1));
    RunId::from_raw(next)
}

/// The truncation flag text. Names the on-demand total when one was counted.
fn truncate_label(total: Option<u64>) -> String {
    match total {
        Some(n) => format!("truncated at 10,000 · {n} total"),
        None => "truncated at 10,000".to_owned(),
    }
}

/// One of the grid footer's controls, in the order they read left to right.
/// The row packs them like the toolbars: what fits stays inline and the
/// contiguous tail moves behind the footer's one `…`, and both sides draw
/// through [`draw_footer_control`], so a control acts the same wherever the
/// row puts it.
#[derive(Clone, Copy)]
enum FooterControl {
    /// Count every row behind the truncation flag.
    CountTotal,
    /// Fetch the next page past the row cap.
    LoadMore,
    /// Bring back every hidden column.
    ShowColumns,
}

impl FooterControl {
    fn label(self, loaded: usize, hidden: usize) -> String {
        match self {
            Self::CountTotal => "Count total".to_owned(),
            Self::LoadMore => format!("Load more ({loaded} loaded)"),
            Self::ShowColumns => format!("Show columns ({hidden})"),
        }
    }
}

/// Draw one footer control and land its click. Pages append onto the loaded
/// rows through [`DbEvent::PageOk`]; a stale page whose offset misses the
/// loaded length is ignored, so a new run never gains old rows.
fn draw_footer_control(
    ui: &mut egui::Ui,
    control: FooterControl,
    label: &str,
    loaded: usize,
    tab: &mut Tab,
    conn: &Option<GridConn>,
    view: &mut GridView,
) {
    match control {
        FooterControl::CountTotal => {
            if ui
                .button(label)
                .on_hover_text("Count every row behind the truncation flag")
                .clicked()
            {
                match conn.as_ref().and_then(|c| c.handle.clone()) {
                    Some(handle) => {
                        if conn.as_ref().is_some_and(|c| is_single_statement(&tab.sql, c.config.kind())) {
                            let run = paging_run_id(ui.ctx(), tab.id);
                            let sql = tab.sql.clone();
                            if handle
                                .send(DbCommand::CountFor { run, tab: tab.id, sql })
                                .is_err()
                            {
                                tab.set_export_notice(
                                    "Count failed: connection is gone".to_owned(),
                                    true,
                                );
                            }
                        } else {
                            tab.set_export_notice(
                                "Counting needs a single SELECT".to_owned(),
                                true,
                            );
                        }
                    }
                    None => tab.set_export_notice(
                        "Counting needs a live connection".to_owned(),
                        true,
                    ),
                }
            }
        }
        FooterControl::LoadMore => {
            if ui
                .button(label)
                .on_hover_text("Fetch the next page past the row cap")
                .clicked()
            {
                match conn.as_ref().and_then(|c| c.handle.clone()) {
                    Some(handle) => {
                        if conn.as_ref().is_some_and(|c| is_single_statement(&tab.sql, c.config.kind())) {
                            let run = paging_run_id(ui.ctx(), tab.id);
                            let sql = tab.sql.clone();
                            if handle
                                .send(DbCommand::QueryPage {
                                    run,
                                    tab: tab.id,
                                    sql,
                                    offset: loaded as u64,
                                    limit: STREAM_PAGE_SIZE as u64,
                                })
                                .is_err()
                            {
                                tab.set_export_notice(
                                    "Page failed: connection is gone".to_owned(),
                                    true,
                                );
                            }
                        } else {
                            tab.set_export_notice(
                                "Paging needs a single SELECT".to_owned(),
                                true,
                            );
                        }
                    }
                    None => tab.set_export_notice(
                        "Paging needs a live connection".to_owned(),
                        true,
                    ),
                }
            }
        }
        FooterControl::ShowColumns => {
            if ui.button(label).clicked() {
                view.show_all();
            }
        }
    }
}

/// Poll one background streamed export: show its progress with a Cancel
/// button while it runs, and record its notice when it lands.
fn poll_export_job(tab: &mut Tab, ui: &mut egui::Ui) {
    let Some(job) = export_job_of(ui.ctx(), tab.id) else { return };
    let snapshot = job.state.lock().map(|s| s.clone()).unwrap_or(JobProgress {
        rows: 0,
        total: None,
        done: false,
        error: Some("export state is gone".to_owned()),
        cancelled: false,
        cancel_handle: None,
    });
    if snapshot.done {
        let name = job.path.display().to_string();
        tab.set_export_notice(format!("Exported {} rows to {name}", snapshot.rows), false);
        store_export_job(ui.ctx(), tab.id, None);
        return;
    }
    if let Some(error) = snapshot.error {
        tab.set_export_notice(format!("Export failed: {error}"), true);
        store_export_job(ui.ctx(), tab.id, None);
        return;
    }
    if snapshot.cancelled {
        tab.set_export_notice("Export cancelled".to_owned(), true);
        store_export_job(ui.ctx(), tab.id, None);
        return;
    }
    let progress = match snapshot.total {
        Some(total) => format!("Exporting {} ({} of {total} rows)…", job.format.label(), snapshot.rows),
        None => format!("Exporting {} ({} rows)…", job.format.label(), snapshot.rows),
    };
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(progress);
        if ui.button("Cancel export").clicked() {
            job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(cancel) = snapshot.cancel_handle {
                cancel.cancel();
            }
        }
    });
}

/// Start one background streamed export of the tab's whole query past the
/// cap. Falls back to the visible grid when the tab holds a batch, has no
/// live connection pieces, or already runs an export.
fn start_stream_export(
    tab: &mut Tab,
    conn: &Option<GridConn>,
    format: StreamFormat,
    visible: fn(&mut Tab),
    ui: &mut egui::Ui,
) {
    if export_job_of(ui.ctx(), tab.id).is_some() {
        tab.set_export_notice("An export is already running".to_owned(), true);
        return;
    }
    let Some(conn) = conn.clone() else {
        visible(tab);
        return;
    };
    if !is_single_statement(&tab.sql, conn.config.kind()) {
        visible(tab);
        return;
    }
    let truncated = tab.active_query_result().is_some_and(|r| r.is_truncated());
    if !truncated {
        visible(tab);
        return;
    }
    let (filter, extension) = match format {
        StreamFormat::Csv => ("CSV", "csv"),
        StreamFormat::Json => ("JSON", "json"),
        StreamFormat::Jsonl => ("JSONL", "jsonl"),
        StreamFormat::Xlsx => ("XLSX", "xlsx"),
        StreamFormat::Tsv => ("TSV", "tsv"),
        StreamFormat::Markdown => ("Markdown", "md"),
        StreamFormat::Inserts => ("SQL", "sql"),
    };
    let default_name = default_export_name(&tab.sql, extension);
    let Some(path) = rfd::FileDialog::new()
        .add_filter(filter, &[extension])
        .set_file_name(&default_name)
        .save_file()
    else {
        return;
    };
    let sql = tab.sql.clone();
    let state = Arc::new(Mutex::new(JobProgress {
        rows: 0,
        total: None,
        done: false,
        error: None,
        cancelled: false,
        cancel_handle: None,
    }));
    let cancel = Arc::new(AtomicBool::new(false));
    store_export_job(
        ui.ctx(),
        tab.id,
        Some(ExportJob { format, path: path.clone(), state: state.clone(), cancel: cancel.clone() }),
    );
    std::thread::spawn(move || {
        let outcome = run_stream_export(
            &conn.config,
            &conn.password,
            &conn.ssh_password,
            &sql,
            format,
            &path,
            &state,
            &cancel,
        );
        let mut guard = state.lock().expect("export state locks");
        match outcome {
            Ok(rows) => {
                guard.rows = rows;
                guard.done = true;
            }
            Err(error) if error == "cancelled" => {
                guard.cancelled = true;
            }
            Err(error) => {
                guard.error = Some(error);
            }
        }
    });
}

/// Open a background session and stream `sql` to `path` in pages. Records
/// the count for progress when one is available and the engine handle so
/// Cancel stops the in-flight page as well as the loop.
fn run_stream_export(
    config: &ConnectionConfig,
    password: &str,
    ssh_password: &str,
    sql: &str,
    format: StreamFormat,
    path: &std::path::Path,
    state: &Arc<Mutex<JobProgress>>,
    cancel: &Arc<AtomicBool>,
) -> Result<u64, String> {
    let password = (!password.is_empty()).then_some(password);
    let ssh_password = (!ssh_password.is_empty()).then_some(ssh_password);
    let mut session = crate::db::Session::open(config, password, ssh_password)
        .map_err(|e| e.to_string())?;
    if let Ok(handle) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        session.cancel_handle()
    })) {
        if let Ok(mut guard) = state.lock() {
            guard.cancel_handle = Some(handle);
        }
    }
    if let Ok(total) = session.count_for(sql) {
        if let Ok(mut guard) = state.lock() {
            guard.total = Some(total);
        }
    }
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("cancelled".to_owned());
    }
    let state_rows = state.clone();
    stream_session_to_file(
        &mut session,
        sql,
        format,
        "result",
        path,
        crate::db::STREAM_PAGE_SIZE,
        cancel,
        &mut |rows| {
            if let Ok(mut guard) = state_rows.lock() {
                guard.rows = rows;
            }
        },
    )
    .map_err(|e| match e {
        crate::export::StreamError::Db(crate::db::DbError::Query(message)) => message,
        other => other.to_string(),
    })
}

fn result_id_of(tab: &Tab) -> Option<ResultId> {
    let result = tab.active_query_result()?;
    if result.columns().is_empty() {
        return None;
    }
    Some(ResultId {
        active: tab.active_result(),
        rows: result.row_count(),
        cols: result.columns().len(),
        took: result.took(),
    })
}

/// The TSV headers toggle for one tab, kept beside the selection.
fn tsv_headers_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_tsv_headers", tab))
}

fn tsv_headers_of(ctx: &egui::Context, tab: TabId) -> bool {
    ctx.data(|data| data.get_temp(tsv_headers_id(tab))).unwrap_or(true)
}

/// The paste request in flight: set with the `RequestPaste` viewport command
/// and cleared when its `Paste` event is handled.
fn paste_armed_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_paste_armed", tab))
}

/// Row indices surviving `filter`, ordered by the sort column. The result
/// is never mutated. An out-of-range sort column keeps natural order.
pub fn visible_rows(
    result: &QueryResult,
    sort_col: Option<usize>,
    sort_dir: SortDir,
    filter: &str,
) -> Vec<usize> {
    let nrows = result.row_count();
    let ncols = result.columns().len();
    let needle = filter.to_lowercase();
    let mut rows: Vec<usize> = (0..nrows)
        .filter(|&r| {
            if needle.is_empty() {
                return true;
            }
            (0..ncols).any(|c| {
                result
                    .cell(r, c)
                    .is_some_and(|v| v.display(CELL_CAP).to_lowercase().contains(&needle))
            })
        })
        .collect();
    if let Some(col) = sort_col {
        if col < ncols {
            rows.sort_by(|&a, &b| {
                let va = result.cell(a, col);
                let vb = result.cell(b, col);
                let ord = cmp_values(va, vb);
                let ord = match sort_dir {
                    SortDir::Asc => ord,
                    SortDir::Desc => match (va, vb) {
                        (Some(Value::Null) | None, _) | (_, Some(Value::Null) | None) => ord,
                        _ => ord.reverse(),
                    },
                };
                ord.then_with(|| a.cmp(&b))
            });
        }
    }
    rows
}

/// Null ranks first, numerics compare numerically, text lexically, blobs
/// by length then bytes. Mixed integer and real share one numeric rank.
fn cmp_values(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    fn rank(v: Option<&Value>) -> u8 {
        match v {
            None | Some(Value::Null) => 0,
            Some(Value::Integer(_)) | Some(Value::Real(_)) => 1,
            Some(Value::Text(_)) => 2,
            Some(Value::Blob(_)) => 3,
        }
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Some(Value::Integer(x)), Some(Value::Integer(y))) => x.cmp(y),
        (Some(Value::Real(x)), Some(Value::Real(y))) => x.total_cmp(y),
        (Some(Value::Integer(x)), Some(Value::Real(y))) => (*x as f64).total_cmp(y),
        (Some(Value::Real(x)), Some(Value::Integer(y))) => x.total_cmp(&(*y as f64)),
        (Some(Value::Text(x)), Some(Value::Text(y))) => x.cmp(y),
        (Some(Value::Blob(x)), Some(Value::Blob(y))) => {
            x.len().cmp(&y.len()).then_with(|| x.cmp(y))
        }
        _ => Ordering::Equal,
    }
}

/// One CSV field: quotes guard commas, quotes, and line breaks.
fn csv_escape(raw: &str) -> String {
    if raw.contains(|c| matches!(c, ',' | '"' | '\n' | '\r')) {
        format!("\"{}\"", raw.replace('"', "\"\""))
    } else {
        raw.to_owned()
    }
}

/// Tab-delimited records re-emitted as comma-delimited text. Quotes guard
/// tabs and line breaks the way [`parse_csv`] guards commas, so one parser
/// reads both shapes.
fn tsv_to_csv(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut at_field_start = true;
    let mut pending = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if at_field_start => {
                in_quotes = true;
                at_field_start = false;
                pending = true;
            }
            '"' => {
                field.push('"');
                pending = true;
            }
            '\t' => {
                record.push(std::mem::take(&mut field));
                at_field_start = true;
                pending = true;
            }
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                if pending {
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                }
                at_field_start = true;
                pending = false;
            }
            _ => {
                field.push(c);
                at_field_start = false;
                pending = true;
            }
        }
    }
    if pending {
        record.push(field);
        records.push(record);
    }
    let mut out = String::new();
    for record in records {
        let line: Vec<String> = record.iter().map(|field| csv_escape(field)).collect();
        out.push_str(&line.join(","));
        out.push('\n');
    }
    out
}

/// Clipboard text into a parsed table: tab-separated text travels through
/// [`tsv_to_csv`] first, so quotes around tab and newline cells survive,
/// then everything parses through [`parse_csv`].
fn parse_paste(text: &str) -> Option<ParsedCsv> {
    if text.contains('\t') {
        if let Some(parsed) = parse_csv(&tsv_to_csv(text)) {
            return Some(parsed);
        }
    }
    parse_csv(text)
}

/// A parsed file into staged-row cells for `columns`: headers matched onto
/// the column names case-insensitively in column order, unmapped columns
/// left empty, extra headers ignored. None when no header matches. Rows cap
/// at the import [`crate::import::ROW_CAP`], so a paste and a JSONL import
/// truncate alike.
fn stage_parsed_rows(parsed: &ParsedCsv, columns: &[String]) -> Option<Vec<Vec<String>>> {
    let mut used = vec![false; parsed.headers.len()];
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for (col, name) in columns.iter().enumerate() {
        let found = parsed
            .headers
            .iter()
            .enumerate()
            .find(|(index, header)| !used[*index] && header.eq_ignore_ascii_case(name));
        if let Some((index, _)) = found {
            used[index] = true;
            pairs.push((index, col));
        }
    }
    if pairs.is_empty() {
        return None;
    }
    let capped = parsed.rows.len().min(crate::import::ROW_CAP);
    Some(
        parsed.rows[..capped]
            .iter()
            .map(|row| {
                let mut cells = vec![String::new(); columns.len()];
                for (header, col) in &pairs {
                    cells[*col] = row[*header].clone();
                }
                cells
            })
            .collect(),
    )
}

fn column_names(tab: &Tab) -> Vec<String> {
    tab.active_query_result()
        .map(|result| result.columns().iter().map(|c| c.name().to_owned()).collect())
        .unwrap_or_default()
}

pub fn results(
    state: &mut AppState,
    ui: &mut egui::Ui,
    cell_editor_key: Option<ModalKey>,
) {
    prune_selection(state, ui.ctx());
    prune_grid_views(state, ui.ctx());
    let modal_open = super::modal_open(state, ui.ctx());
    let Some(tab_id) = state.active() else {
        ui.label("No query tab.");
        return;
    };
    let Some(tab) = state.tab(tab_id) else {
        ui.label("No query tab.");
        return;
    };
    match tab.outcome() {
        RunOutcome::Ok { .. } => {}
        RunOutcome::Never => {
            ui.label(format!(
                "Type SQL above and press Run ({}).",
                ui.ctx().format_shortcut(&super::query::run_shortcut())
            ));
            return;
        }
        RunOutcome::Running { .. } => {
            // A new run replaces the rows under any open field.
            store_edit_field(ui.ctx(), tab_id, None);
            clear_selection(ui.ctx(), tab_id);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Running… Cancel in the toolbar to abandon.");
            });
            return;
        }
        RunOutcome::Err(message) => {
            ui.colored_label(ui.visuals().error_fg_color, message.clone());
            if tab.results().is_empty() {
                ui.label(
                    egui::RichText::new(
                        "Check SQL syntax and the connection, then press Run again.",
                    )
                    .small()
                    .weak(),
                );
                return;
            }
            ui.label(
                egui::RichText::new("Showing the last completed run.").small().weak(),
            );
        }
    }
    let target = state.edit_target(tab_id);
    let filter_history = state.filter_history().to_vec();
    let grid_conn = state
        .tab(tab_id)
        .and_then(|t| t.conn)
        .and_then(|cid| state.connections().iter().find(|c| c.id == cid))
        .map(|c| {
            GridConn {
                config: c.config.clone(),
                password: c.password.clone(),
                ssh_password: c.ssh_password.clone(),
                handle: c.handle(),
            }
        });
    let Some(tab) = state.tab_mut(tab_id) else { return };
    let actions = results_grid(
        tab,
        target.as_ref(),
        &filter_history,
        modal_open,
        grid_conn,
        cell_editor_key,
        ui,
    );
    apply_edit_actions(state, tab_id, actions);
}

/// One deferred grid edit transition, applied once the tab borrow ends.
enum EditAction {
    Stage { row: usize, col: usize, text: String },
    ToggleDelete(usize),
    AddRow,
    Apply,
    Discard,
    ImportCsv,
    ImportJsonl,
    ImportXlsx,
    PasteRows(Vec<Vec<String>>),
    ApplyTableFilter,
    ClearTableFilter,
    SaveTableFilter { name: String },
    RecallTableFilter { name: String },
    DeleteTableFilter { name: String },
    JumpFk { row: usize, col: usize },
}

fn apply_edit_actions(state: &mut AppState, tab: TabId, actions: Vec<EditAction>) {
    for action in actions {
        match action {
            EditAction::Stage { row, col, text } => state.stage_edit(tab, row, col, text),
            EditAction::ToggleDelete(row) => state.mark_row_deleted(tab, row),
            EditAction::AddRow => state.add_row(tab),
            EditAction::Apply => state.apply_edits(tab),
            EditAction::Discard => state.discard_edits(tab),
            EditAction::ImportCsv => pick_import_file(state, tab),
            EditAction::ImportJsonl => pick_import_jsonl_file(state, tab),
            EditAction::ImportXlsx => pick_import_xlsx_file(state, tab),
            EditAction::PasteRows(staged) => stage_rows_into(state, tab, staged, "Paste", "Pasted"),
            EditAction::ApplyTableFilter => state.apply_table_filter(tab),
            EditAction::ClearTableFilter => state.clear_table_filter(tab),
            EditAction::SaveTableFilter { name } => {
                state.save_filter(tab, &name);
            }
            EditAction::RecallTableFilter { name } => {
                state.recall_filter(tab, &name);
            }
            EditAction::DeleteTableFilter { name } => {
                state.delete_filter(&name);
            }
            EditAction::JumpFk { row, col } => {
                state.follow_foreign_key(tab, row, col);
            }
        }
    }
}

/// Record one import notice on a tab that still exists.
fn tab_notice(state: &mut AppState, tab: TabId, notice: String, failed: bool) {
    if let Some(current) = state.tab_mut(tab) {
        current.set_export_notice(notice, failed);
    }
}

/// Ask for a CSV or JSONL file and stage it on the tab through the import
/// modal. [`AppState::begin_import`] names the parser by extension. A
/// dismissed dialog changes nothing.
fn pick_import_file(state: &mut AppState, tab: TabId) {
    let Some(path) = rfd::FileDialog::new()
        .add_filter("CSV or JSONL", &["csv", "jsonl"])
        .pick_file()
    else {
        return;
    };
    state.begin_import(tab, &path);
}

/// Ask for a JSONL file and stage its objects as new rows for review: the
/// union of keys maps onto the result columns the way a paste's headers do,
/// and the same staged-edit path builds the INSERTs on apply.
fn pick_import_jsonl_file(state: &mut AppState, tab: TabId) {
    let Some(path) = rfd::FileDialog::new().add_filter("JSONL", &["jsonl"]).pick_file() else {
        return;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            tab_notice(state, tab, format!("Import failed: {}: {error}", path.display()), true);
            return;
        }
    };
    let Some(parsed) = parse_jsonl(&text) else {
        tab_notice(
            state,
            tab,
            "Import failed: the file needs one JSON object per line".to_owned(),
            true,
        );
        return;
    };
    let columns = state.tab(tab).map(column_names).unwrap_or_default();
    match stage_parsed_rows(&parsed, &columns) {
        Some(staged) => stage_rows_into(state, tab, staged, "Import", "Imported"),
        None => tab_notice(
            state,
            tab,
            "Import failed: no JSONL key matches the result columns".to_owned(),
            true,
        ),
    }
}

/// Ask for an XLSX file and stage its first worksheet's rows for review the
/// way [`pick_import_jsonl_file`] does; the notice names the sheet when the
/// workbook carries several.
fn pick_import_xlsx_file(state: &mut AppState, tab: TabId) {
    let Some(path) = rfd::FileDialog::new().add_filter("XLSX", &["xlsx"]).pick_file() else {
        return;
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            tab_notice(state, tab, format!("Import failed: {}: {error}", path.display()), true);
            return;
        }
    };
    let Some(workbook) = parse_xlsx(&bytes) else {
        tab_notice(
            state,
            tab,
            "Import failed: the first sheet needs a header row with rows under it".to_owned(),
            true,
        );
        return;
    };
    let columns = state.tab(tab).map(column_names).unwrap_or_default();
    match stage_parsed_rows(&workbook.parsed, &columns) {
        Some(staged) => {
            let done = if workbook.sheet_count > 1 {
                format!("Imported from sheet \"{}\":", workbook.sheet)
            } else {
                "Imported".to_owned()
            };
            stage_rows_into(state, tab, staged, "Import", &done);
        }
        None => tab_notice(
            state,
            tab,
            "Import failed: no XLSX header matches the result columns".to_owned(),
            true,
        ),
    }
}

/// Stage rows as new inserts: one added row per staged row, every cell
/// through the normal staged-edit path so the user reviews before apply.
/// Pre-existing added rows keep their indices. `action` and `done` name the
/// operation in the notices, as in `Paste failed` and `Pasted 3 rows`.
fn stage_rows_into(
    state: &mut AppState,
    tab: TabId,
    staged: Vec<Vec<String>>,
    action: &str,
    done: &str,
) {
    if state.edit_target(tab).is_none() {
        tab_notice(state, tab, format!("{action} failed: the table is not editable"), true);
        return;
    }
    let shape = state.tab(tab).and_then(|current| {
        current
            .active_query_result()
            .map(|result| (result.row_count(), current.added_rows().len()))
    });
    let Some((total, base)) = shape else { return };
    for (index, cells) in staged.iter().enumerate() {
        state.add_row(tab);
        for (col, text) in cells.iter().enumerate() {
            state.stage_edit(tab, total + base + index, col, text.clone());
        }
    }
    tab_notice(
        state,
        tab,
        format!("{done} {} rows for review — Apply to insert", staged.len()),
        false,
    );
}

/// The confirmation modal for a pending import: every mapped header beside
/// the table column it writes, the skipped headers, the truncation count,
/// and the Import and Cancel buttons.
pub fn import_window(
    state: &mut AppState,
    ui: &mut egui::Ui,
    key: Option<ModalKey>,
) -> Option<egui::LayerId> {
    let Some(draft) = state.import_draft() else { return None };
    let title = format!("Import into {}", draft.table);
    let mut confirm = false;
    let mut cancel = false;
    let window = egui::Window::new(title).show(ui.ctx(), |ui| {
        let Some(draft) = state.import_draft() else { return };
        ui.label(format!("{} rows", draft.rows.len()));
        egui::Grid::new("import_columns").striped(true).show(ui, |ui| {
            ui.label(egui::RichText::new(draft.column_title()).small().weak());
            ui.label(egui::RichText::new("Table column").small().weak());
            ui.end_row();
            for (header, column) in &draft.map.pairs {
                ui.monospace(draft.headers.get(*header).cloned().unwrap_or_default());
                ui.monospace(draft.map.columns.get(*column).cloned().unwrap_or_default());
                ui.end_row();
            }
        });
        if !draft.map.skipped.is_empty() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("Skipped headers: {}", draft.map.skipped.join(", ")),
            );
        }
        let dropped = crate::import::dropped_rows(draft.rows.len());
        if dropped > 0 {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!(
                    "{dropped} rows over the {} row cap will be dropped",
                    crate::import::ROW_CAP
                ),
            );
        }
        ui.horizontal(|ui| {
            if ui.button("Import").clicked() {
                confirm = true;
            }
            let cancel_button = ui.button("Cancel");
            autofocus(ui, "import-cancel", &cancel_button);
            if cancel_button.clicked() {
                cancel = true;
            }
        });
    });
    if confirm {
        state.commit_import();
    } else if cancel || key == Some(ModalKey::Cancel) {
        state.cancel_import();
    }
    window.map(|window| window.response.layer_id)
}

/// The in-place cell editor, kept in the egui context data keyed by tab, the
/// pattern the find bar uses.
#[derive(Clone)]
struct EditField {
    row: usize,
    col: usize,
    /// The active result the field opened on, so switching results closes it.
    result: usize,
    text: String,
    /// Focus is requested on the first frame the field draws.
    fresh: bool,
}

fn edit_field_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_edit_field", tab))
}

fn edit_field_of(ctx: &egui::Context, tab: TabId) -> Option<EditField> {
    ctx.data(|d| d.get_temp(edit_field_id(tab)))
}

fn store_edit_field(ctx: &egui::Context, tab: TabId, field: Option<EditField>) {
    ctx.data_mut(|d| match field {
        Some(field) => {
            d.insert_temp(edit_field_id(tab), field);
        }
        None => {
            d.remove::<EditField>(edit_field_id(tab));
        }
    });
}

/// Open an in-place editor seeded with the typed text, so typing replaces
/// the cell. Enter commits through [`EditAction::Stage`].
fn type_edit_field(row: usize, col: usize, result: usize, seed: String) -> EditField {
    EditField { row, col, result, text: seed, fresh: true }
}

/// Largest text the cell editor stages. A guard beside the grid's display
/// truncation, so a multi-megabyte value cannot flood the staged batch.
const CELL_EDITOR_LIMIT: usize = 1_000_000;

/// The dedicated cell editor modal, kept in the egui context data keyed by
/// tab, the pattern the find bar uses. Unlike the inline [`EditField`], it
/// shows the full value past [`CELL_CAP`] truncation, blobs as hex, with
/// explicit Set NULL and Set DEFAULT actions beside Save.
#[derive(Clone, Debug)]
struct CellEditor {
    row: usize,
    col: usize,
    /// The active result the editor opened on, so switching results closes it.
    result: usize,
    text: String,
    /// The seed shows a blob, so the modal warns the save writes text.
    is_blob: bool,
    /// Focus is requested on the first frame the modal draws.
    fresh: bool,
}

fn cell_editor_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_cell_editor", tab))
}

fn cell_editor_of(ctx: &egui::Context, tab: TabId) -> Option<CellEditor> {
    ctx.data(|d| d.get_temp(cell_editor_id(tab)))
}

/// Whether the active tab's cell editor owns the keyboard. The editor itself
/// stays in egui temp data because it carries a UI text buffer; this is the
/// piece the shared [`super::modal_open`] gate reads.
pub(crate) fn cell_editor_open(state: &AppState, ctx: &egui::Context) -> bool {
    state.active().is_some_and(|tab| cell_editor_of(ctx, tab).is_some())
}

fn store_cell_editor(ctx: &egui::Context, tab: TabId, editor: Option<CellEditor>) {
    ctx.data_mut(|d| match editor {
        Some(editor) => {
            d.insert_temp(cell_editor_id(tab), editor);
        }
        None => {
            d.remove::<CellEditor>(cell_editor_id(tab));
        }
    });
}

/// Show the modal and withdraw any inline field, so the two editors never
/// stage over each other.
fn show_cell_editor(ctx: &egui::Context, tab: TabId, editor: CellEditor) {
    store_edit_field(ctx, tab, None);
    store_cell_editor(ctx, tab, Some(editor));
}

/// Modal seed for one cell: the staged text when one exists (empty for a
/// staged DEFAULT, since empty stages NULL), else the full untruncated value
/// with blobs as hex. The flag says the seed shows a blob.
fn editor_seed(staged: Option<&str>, cell: Option<&Value>) -> (String, bool) {
    if let Some(staged) = staged {
        if staged == STAGED_DEFAULT {
            return (String::new(), false);
        }
        return (staged.to_owned(), false);
    }
    match cell {
        Some(Value::Text(text)) => (text.clone(), false),
        Some(Value::Blob(bytes)) => (blob_hex(bytes), true),
        Some(Value::Null) | None => (String::new(), false),
        Some(value) => (value.display(usize::MAX).into_owned(), false),
    }
}

/// Lowercase hex of raw bytes, so a blob is fully visible with no dependency.
fn blob_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// True when the modal may stage `text`: the byte cap beside the grid's
/// display truncation.
fn editor_size_ok(text: &str) -> bool {
    text.len() <= CELL_EDITOR_LIMIT
}

/// Open the modal over one result cell: bounds-checked, never on a deleted
/// row, seeded from the staged text or the full value.
fn open_cell_editor(tab: &Tab, row: usize, col: usize, result: usize) -> Option<CellEditor> {
    let (rows, cols) = tab
        .active_query_result()
        .map(|result| (result.row_count(), result.columns().len()))?;
    if row >= rows || col >= cols || tab.is_row_deleted(row) {
        return None;
    }
    let (text, is_blob) = editor_seed(
        tab.edit_text(row, col),
        tab.active_query_result().and_then(|result| result.cell(row, col)),
    );
    Some(CellEditor { row, col, result, text, is_blob, fresh: true })
}

/// Stage the SQL DEFAULT keyword for one cell through the normal staged-edit
/// path, via the [`STAGED_DEFAULT`] marker.
fn default_stage(row: usize, col: usize) -> EditAction {
    EditAction::Stage { row, col, text: STAGED_DEFAULT.to_owned() }
}

/// One-line state above the modal field: the staged edit when one exists,
/// else the original value with long text and blobs spelled out.
fn editor_status(tab: &Tab, editor: &CellEditor) -> String {
    match tab.edit_text(editor.row, editor.col) {
        Some(staged) if staged == STAGED_DEFAULT => "Staged: DEFAULT".to_owned(),
        Some("") => "Staged: NULL".to_owned(),
        Some(staged) => {
            let preview: String = staged.chars().take(120).collect();
            let more = if staged.chars().count() > 120 { "…" } else { "" };
            format!("Staged: {preview}{more}")
        }
        None => match tab
            .active_query_result()
            .and_then(|result| result.cell(editor.row, editor.col))
        {
            Some(Value::Null) | None => "Currently NULL".to_owned(),
            Some(Value::Text(text)) if text.chars().count() > CELL_CAP => {
                format!("{} chars; the grid shows the first {CELL_CAP}", text.chars().count())
            }
            Some(value) => format!("Currently: {}", value.display(usize::MAX)),
        },
    }
}

/// The dedicated cell editor modal: the full value past the grid's display
/// truncation with blobs as hex, and Save beside explicit Set NULL and Set
/// DEFAULT. Every write defers through [`EditAction::Stage`], so the normal
/// staged-edit path builds the UPDATE on apply. A stale result or a lost
/// edit target closes it, the way a run closes the inline field.
fn cell_editor_window(
    tab: &mut Tab,
    cols: &[(String, String)],
    editable: bool,
    active_result: usize,
    key: Option<ModalKey>,
    ui: &mut egui::Ui,
) -> Vec<EditAction> {
    let mut actions = Vec::new();
    let Some(mut editor) = cell_editor_of(ui.ctx(), tab.id) else { return actions };
    if !editable || editor.result != active_result || cols.get(editor.col).is_none() {
        store_cell_editor(ui.ctx(), tab.id, None);
        return actions;
    }
    let (name, type_name) = cols[editor.col].clone();
    let status = editor_status(tab, &editor);
    let title = format!("Cell editor — {name} (row {})", editor.row + 1);
    let mut save = false;
    let mut set_null = false;
    let mut set_default = false;
    let mut cancel = false;
    let window = egui::Window::new(title).collapsible(false).resizable(true).show(ui.ctx(), |ui| {
        ui.label(egui::RichText::new(format!("{name} · {type_name}")).small().weak());
        ui.label(egui::RichText::new(status).small().weak());
        if editor.is_blob {
            ui.label(
                egui::RichText::new("The blob shows as hex; saving writes the field as text.")
                    .small()
                    .weak(),
            );
        }
        let response = ui.add(
            egui::TextEdit::multiline(&mut editor.text)
                .desired_rows(12)
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Monospace),
        );
        if editor.fresh {
            response.request_focus();
            // The window's modal layer only applies next frame. Drop this
            // frame's Tab direction so the fresh field is not stepped over
            // before the layer takes hold.
            ui.ctx().memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
            editor.fresh = false;
        }
        if editor_size_ok(&editor.text) {
            ui.label(
                egui::RichText::new(format!(
                    "{} chars · {} bytes",
                    editor.text.chars().count(),
                    editor.text.len()
                ))
                .small()
                .weak(),
            );
        } else {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!(
                    "{} bytes pass the {CELL_EDITOR_LIMIT} byte cap; shorten the text or use Set NULL or Set DEFAULT",
                    editor.text.len()
                ),
            );
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(editor_size_ok(&editor.text), egui::Button::new("Save"))
                .clicked()
            {
                save = true;
            }
            if ui.button("Set NULL").clicked() {
                set_null = true;
            }
            if ui.button("Set DEFAULT").clicked() {
                set_default = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
        if key == Some(ModalKey::Cancel) {
            cancel = true;
        }
        store_cell_editor(ui.ctx(), tab.id, Some(editor.clone()));
    });
    if let Some(window) = &window {
        ui.ctx().memory_mut(|memory| memory.set_modal_layer(window.response.layer_id));
    }
    let (row, col) = (editor.row, editor.col);
    if save {
        actions.push(EditAction::Stage { row, col, text: editor.text.clone() });
        store_cell_editor(ui.ctx(), tab.id, None);
    } else if set_null {
        actions.push(null_stage(row, col));
        store_cell_editor(ui.ctx(), tab.id, None);
    } else if set_default {
        actions.push(default_stage(row, col));
        store_cell_editor(ui.ctx(), tab.id, None);
    } else if cancel {
        store_cell_editor(ui.ctx(), tab.id, None);
    }
    if cell_editor_of(ui.ctx(), tab.id).is_none() {
        set_grid_armed(ui.ctx(), tab.id, true);
    }
    actions
}

/// Stage empty text for one cell. Empty renders as NULL through
/// [`crate::edit::to_literal`], so Backspace sets NULL.
fn null_stage(row: usize, col: usize) -> EditAction {
    EditAction::Stage { row, col, text: String::new() }
}

/// One [`EditAction::Stage`] per target cell with the source text, skipping
/// the source row itself so a fill never stages a no-op UPDATE.
fn fill_stages(
    source_row: usize,
    col: usize,
    rows: &[usize],
    text: String,
) -> Vec<EditAction> {
    rows.iter()
        .filter(|row| **row != source_row)
        .map(|row| EditAction::Stage { row: *row, col, text: text.clone() })
        .collect()
}

/// The fill for one focused cell: its effective text (the staged edit, else
/// the full display) over `targets` in the same column. Empty for a deleted
/// source, a blob source, or no in-range target, so the staged path only ever
/// sees cells it can write.
fn fill_from(tab: &Tab, row: usize, col: usize, targets: &[usize]) -> Vec<EditAction> {
    let Some(result) = tab.active_query_result() else { return Vec::new() };
    if row >= result.row_count() || col >= result.columns().len() {
        return Vec::new();
    }
    if tab.is_row_deleted(row) {
        return Vec::new();
    }
    let source = result.cell(row, col);
    if matches!(source, Some(Value::Blob(_))) {
        return Vec::new();
    }
    let text = match tab.edit_text(row, col) {
        Some(staged) => staged.to_owned(),
        None => source.map(|v| v.display(usize::MAX).into_owned()).unwrap_or_default(),
    };
    let in_range: Vec<usize> = targets
        .iter()
        .copied()
        .filter(|target| *target < result.row_count() && !tab.is_row_deleted(*target))
        .collect();
    fill_stages(row, col, &in_range, text)
}

/// Result picker for a run that produced more than one result set.
fn result_tabs(tab: &mut Tab, ui: &mut egui::Ui) {
    let count = tab.results().len();
    if count <= 1 {
        return;
    }
    let mut pick: Option<usize> = None;
    let active = tab.active_result();
    ui.horizontal(|ui| {
        for index in 0..count {
            let label = format!("Result {}", index + 1);
            if ui.selectable_label(active == index, label).clicked() {
                pick = Some(index);
            }
        }
    });
    if let Some(index) = pick {
        tab.set_active_result(index);
    }
}

/// The one right-side pane a tab can show beside the grid: the row detail or
/// the chart. Kept beside the selection in egui temp data, so it lasts the
/// session and dies with the tab. Default is no pane; the two never share the
/// right side.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum SidePane {
    #[default]
    None,
    Detail,
    Chart,
}

/// The result actions, in the order they read left to right. The inline row
/// and the overflow menu both draw through the same drawer, so an action is
/// drawn once whatever the width.
#[derive(Clone, Copy)]
enum ActionItem {
    Copy,
    Export,
    Edit,
    Detail,
    Chart,
}

impl ActionItem {
    fn label(self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::Export => "Export",
            Self::Edit => "Edit",
            Self::Detail => "Detail",
            Self::Chart => "Chart",
        }
    }
}

fn side_pane_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_side_pane", tab))
}

pub(crate) fn side_pane_of(ctx: &egui::Context, tab: TabId) -> SidePane {
    ctx.data(|data| data.get_temp(side_pane_id(tab))).unwrap_or_default()
}

fn store_side_pane(ctx: &egui::Context, tab: TabId, pane: SidePane) {
    ctx.data_mut(|data| data.insert_temp(side_pane_id(tab), pane));
}

fn filter_focus_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_filter_focus", tab))
}

/// Ask the grid to focus `tab`'s row filter on its next draw.
pub(crate) fn focus_filter(ctx: &egui::Context, tab: TabId) {
    ctx.data_mut(|data| data.insert_temp(filter_focus_id(tab), true));
}

fn take_filter_focus(ctx: &egui::Context, tab: TabId) -> bool {
    ctx.data_mut(|data| data.remove_temp::<bool>(filter_focus_id(tab))).unwrap_or(false)
}

/// The structured filter body's open state for one tab: stored in temp data
/// and defaulting open when clauses exist, so a recalled filter is visible.
fn filters_open_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_filters_open", tab))
}

fn filters_open_of(ctx: &egui::Context, tab: TabId, clauses: usize) -> bool {
    ctx.data(|data| data.get_temp(filters_open_id(tab))).unwrap_or(clauses > 0)
}

fn store_filters_open(ctx: &egui::Context, tab: TabId, open: bool) {
    ctx.data_mut(|data| data.insert_temp(filters_open_id(tab), open));
}

/// Pretty JSON for one result row in the exported [`to_json`] shape, so the
/// pane and Copy JSON agree. None when the row left with the last run.
fn detail_json(result: &QueryResult, row: usize) -> Option<String> {
    if row >= result.row_count() {
        return None;
    }
    Some(to_json(result, &[row]))
}

/// Full read-only text for one cell: whole text past [`CELL_CAP`], blobs as
/// hex the way [`editor_seed`] seeds them, null spelled out.
fn detail_cell_text(cell: Option<&Value>) -> String {
    match cell {
        None | Some(Value::Null) => "NULL".to_owned(),
        Some(Value::Text(text)) => text.clone(),
        Some(Value::Blob(bytes)) => blob_hex(bytes),
        Some(value) => value.display(usize::MAX).into_owned(),
    }
}

/// One transposed record row per column: name, type, and the grid-style value.
/// Empty when the row is gone, so a stale focus renders the hint instead.
fn detail_record(result: &QueryResult, row: usize) -> Vec<(String, String, String)> {
    if row >= result.row_count() {
        return Vec::new();
    }
    result
        .columns()
        .iter()
        .enumerate()
        .map(|(col, column)| {
            let text = result
                .cell(row, col)
                .map(|value| value.display(CELL_CAP).into_owned())
                .unwrap_or_default();
            (column.name().to_owned(), column.type_name().to_owned(), text)
        })
        .collect()
}

/// Read-only detail for the focused row over the same active result the grid
/// reads: its JSON, its transposed record, and the focused cell's full text.
/// Labels and copies only, with no key handling, so no edit is ever staged
/// and the modal and focus gates hold.
fn row_detail(tab: &Tab, selection: &Selection, ui: &mut egui::Ui) {
    ui.heading("Row detail");
    let Some(result) = tab.active_query_result() else {
        ui.label("No result to inspect.");
        return;
    };
    let Some((row, col)) = selection.active() else {
        ui.label("Click a cell to inspect its row.");
        return;
    };
    if row >= result.row_count() {
        ui.label("The focused row left with the last run.");
        return;
    }
    ui.label(egui::RichText::new(format!("Row {} of {}", row + 1, result.row_count())).small().weak());
    let json = detail_json(result, row).unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("JSON").strong());
        if ui.button("Copy JSON").clicked() {
            ui.ctx().copy_text(json.clone());
        }
    });
    egui::ScrollArea::both().max_height(220.0).show(ui, |ui| {
        ui.monospace(&json);
    });
    ui.separator();
    ui.label(egui::RichText::new("Record").strong());
    egui::Grid::new("crusty_row_detail_record").striped(true).show(ui, |ui| {
        for (name, type_name, text) in detail_record(result, row) {
            ui.label(egui::RichText::new(name).strong());
            ui.label(egui::RichText::new(type_name).small().weak());
            ui.monospace(text);
            ui.end_row();
        }
    });
    ui.separator();
    if let Some(column) = result.columns().get(col) {
        let (name, type_name) = (column.name().to_owned(), column.type_name().to_owned());
        ui.label(egui::RichText::new("Value").strong());
        ui.label(egui::RichText::new(format!("{name} · {type_name}")).small().weak());
        if matches!(result.cell(row, col), Some(Value::Blob(_))) {
            ui.label(egui::RichText::new("The blob shows as hex.").small().weak());
        }
        egui::ScrollArea::both().max_height(220.0).show(ui, |ui| {
            ui.monospace(detail_cell_text(result.cell(row, col)));
        });
    }
}

/// The text color a data cell's label carries: a selected cell switches to
/// its theme's selection text so the value stays legible on either selection
/// fill. A deleted cell always carries the strikethrough, and the weak
/// treatment only when it is unselected, since the selection color wins.
fn cell_label_color(selected: bool, palette: &Palette) -> Option<egui::Color32> {
    selected.then_some(palette.selection_text)
}

fn results_grid(
    tab: &mut Tab,
    target: Option<&EditTarget>,
    filter_history: &[SavedFilter],
    modal_open: bool,
    grid_conn: Option<GridConn>,
    cell_editor_key: Option<ModalKey>,
    ui: &mut egui::Ui,
) -> Vec<EditAction> {
    let mut actions = Vec::new();
    result_tabs(tab, ui);
    let palette = Palette::of(ui.ctx().theme());
    super::plan::plan_section(tab, ui);
    poll_export_job(tab, ui);
    let (cols, total, ms, truncated, known_total) = {
        let Some(result) = tab.active_query_result() else { return actions };
        if result.columns().is_empty() {
            match result.affected() {
                Some(n) => {
                    ui.colored_label(
                        palette.success,
                        egui::RichText::new(format!("✔ {n} rows affected")).strong(),
                    );
                }
                None => {
                    ui.label("Done.");
                }
            }
            return actions;
        }
        let total = result.row_count();
        let cols: Vec<(String, String)> = result
            .columns()
            .iter()
            .map(|c| (c.name().to_owned(), c.type_name().to_owned()))
            .collect();
        let ms = result.took().as_secs_f64() * 1000.0;
        let truncated = result.is_truncated();
        let known_total = result.total();
        if total == 0 && tab.added_rows().is_empty() {
            ui.label(egui::RichText::new("No rows returned.").weak());
            return actions;
        }
        (cols, total, ms, truncated, known_total)
    };
    let editable = target.is_some();
    if !editable {
        // A run or a target change withdraws editing; drop any open field.
        store_edit_field(ui.ctx(), tab.id, None);
        store_cell_editor(ui.ctx(), tab.id, None);
    }
    let (sort_col, sort_dir, filter) = (tab.sort_col(), tab.sort_dir(), tab.filter().to_owned());
    let mut selection = match result_id_of(tab) {
        Some(id) => {
            let mut current = selection_of(ui.ctx(), tab.id);
            current.sync(id);
            current
        }
        None => {
            clear_selection(ui.ctx(), tab.id);
            Selection::default()
        }
    };
    let mut gestures: Vec<(usize, usize, bool, bool)> = Vec::new();
    let visible = match tab.active_query_result() {
        Some(result) => visible_rows(result, sort_col, sort_dir, &filter),
        None => return actions,
    };
    let mut view = grid_view_of(ui.ctx(), tab.id);
    let mut side_pane = side_pane_of(ui.ctx(), tab.id);
    let mut filters_open =
        filters_open_of(ui.ctx(), tab.id, tab.filter_clauses().len());
    let mut filters_toggled = false;
    let rows = selection.resolve(&visible);
    let nrows = rows.len();
    let edit_label = target.map(|target| target.table.clone());
    let can_export = tab
        .active_query_result()
        .is_some_and(|result| !result.columns().is_empty());
    let can_export_all = tab.results().iter().any(|result| !result.columns().is_empty());
    let action_items: Vec<ActionItem> = [
        ActionItem::Copy,
        ActionItem::Export,
        ActionItem::Edit,
        ActionItem::Detail,
        ActionItem::Chart,
    ]
    .into_iter()
    .filter(|item| match item {
        ActionItem::Copy => can_export,
        ActionItem::Edit => editable,
        ActionItem::Export | ActionItem::Detail | ActionItem::Chart => true,
    })
    .collect();
    let gap = ui.spacing().item_spacing.x;
    let clauses = tab.filter_clauses().len();
    let filters_label = if clauses > 0 {
        format!("Filters ({clauses})")
    } else {
        "Filters".to_owned()
    };
    let clear_shown = !tab.filter().is_empty();
    let clear_w = fit::button_w(ui, CLEAR_LABEL);
    let filters_w = fit::button_w(ui, &filters_label);
    let left_fixed = filters_w + if clear_shown { clear_w + gap } else { 0.0 };
    let tab_id = tab.id;
    let filter_before = tab.filter().to_owned();
    let mut filter_after: Option<String> = None;
    let action_w: Vec<f32> = action_items
        .iter()
        .map(|item| fit::button_w(ui, item.label()))
        .collect();
    let actions_w: f32 = action_w.iter().map(|w| w + gap).sum();
    let avail = ui.available_width();
    let one_row = avail >= FIELD_MIN + left_fixed + 2.0 * gap + actions_w;
    let mut draw_action = |ui: &mut egui::Ui, item: ActionItem| {
        match item {
            ActionItem::Copy => {
                let tab_id = tab.id;
                let mut headers = tsv_headers_of(ui.ctx(), tab_id);
                // A menu that closes on any click costs open, tick, reopen,
                // click to get TSV with headers, so the inline row keeps one
                // that ignores inside clicks and leaves the checkbox standing.
                // Inside the row's `…` menu every tail item is a plain menu
                // button, so the tail draws that instead of a nested popup.
                let mut copy_menu = |ui: &mut egui::Ui| {
                        if ui
                            .button("TSV")
                            .on_hover_text("Copy the selection as tab-separated values")
                            .clicked()
                        {
                            let text = tab
                                .active_query_result()
                                .map(|result| to_tsv(result, &rows, headers))
                                .unwrap_or_default();
                            ui.ctx().copy_text(text);
                            tab.set_export_notice(
                                format!("Copied {nrows} rows as TSV"),
                                false,
                            );
                            ui.close();
                        }
                        if ui
                            .button("CSV")
                            .on_hover_text("Copy the selection as CSV")
                            .clicked()
                        {
                            let text = tab
                                .active_query_result()
                                .map(|result| to_csv(result, &rows))
                                .unwrap_or_default();
                            ui.ctx().copy_text(text);
                            tab.set_export_notice(
                                format!("Copied {nrows} rows as CSV"),
                                false,
                            );
                            ui.close();
                        }
                        if ui
                            .button("JSON")
                            .on_hover_text("Copy the selection as JSON")
                            .clicked()
                        {
                            let text = tab
                                .active_query_result()
                                .map(|result| to_json(result, &rows))
                                .unwrap_or_default();
                            ui.ctx().copy_text(text);
                            tab.set_export_notice(
                                format!("Copied {nrows} rows as JSON"),
                                false,
                            );
                            ui.close();
                        }
                        if ui
                            .button("Markdown")
                            .on_hover_text("Copy the selection as a Markdown table")
                            .clicked()
                        {
                            let text = tab
                                .active_query_result()
                                .map(|result| to_markdown(result, &rows))
                                .unwrap_or_default();
                            ui.ctx().copy_text(text);
                            tab.set_export_notice(
                                format!("Copied {nrows} rows as Markdown"),
                                false,
                            );
                            ui.close();
                        }
                        if ui
                            .button("INSERTs")
                            .on_hover_text("Copy the selection as SQL INSERTs")
                            .clicked()
                        {
                            let text = tab
                                .active_query_result()
                                .map(|result| to_inserts("result", result, &rows))
                                .unwrap_or_default();
                            ui.ctx().copy_text(text);
                            tab.set_export_notice(
                                format!("Copied {nrows} rows as INSERTs"),
                                false,
                            );
                            ui.close();
                        }
                        if ui
                            .button("IN (...)")
                            .on_hover_text("Copy a typed IN list from the active column")
                            .clicked()
                        {
                            let pick = selection.active().and_then(|(_, col)| {
                                column_names(tab)
                                    .get(col)
                                    .cloned()
                                    .map(|name| (col, name))
                            });
                            match pick {
                                None => tab.set_export_notice(
                                    "Click a cell to choose the IN column".to_owned(),
                                    true,
                                ),
                                Some((col, name)) => {
                                    let clause = tab
                                        .active_query_result()
                                        .and_then(|result| to_in_list(result, &rows, col));
                                    match clause {
                                        Some(text) => {
                                            ui.ctx().copy_text(text);
                                            tab.set_export_notice(
                                                format!(
                                                    "Copied {name} IN list ({nrows} rows)"
                                                ),
                                                false,
                                            );
                                        }
                                        None => tab.set_export_notice(
                                            format!("No non-NULL values in {name}"),
                                            true,
                                        ),
                                    }
                                }
                            }
                            ui.close();
                        }
                        if ui
                            .checkbox(&mut headers, "Headers")
                            .on_hover_text("Include the header row in TSV copies")
                            .changed()
                        {
                            ui.ctx().data_mut(|data| {
                                data.insert_temp(tsv_headers_id(tab_id), headers)
                            });
                        }
                };
                if egui::menu::is_in_menu(ui) {
                    // A submenu in the tail keeps the inline menu's close
                    // behavior, so ticking Headers does not shut it.
                    egui::menu::SubMenuButton::new(item.label())
                        .config(egui::menu::MenuConfig::new().close_behavior(
                            egui::PopupCloseBehavior::CloseOnClickOutside,
                        ))
                        .ui(ui, &mut copy_menu);
                } else {
                    egui::menu::MenuButton::new(item.label())
                        .config(egui::menu::MenuConfig::new().close_behavior(
                            egui::PopupCloseBehavior::CloseOnClickOutside,
                        ))
                        .ui(ui, &mut copy_menu);
                }
            }
            ActionItem::Export => {
                ui.menu_button(item.label(), |ui| {
                    if ui.add_enabled(can_export, egui::Button::new("CSV")).clicked() {
                        export_csv_streaming(tab, &grid_conn, ui);
                    }
                    if ui.add_enabled(can_export, egui::Button::new("JSON")).clicked() {
                        export_json_streaming(tab, &grid_conn, ui);
                    }
                    if ui.add_enabled(can_export, egui::Button::new("JSONL")).clicked() {
                        export_jsonl_streaming(tab, &grid_conn, ui);
                    }
                    if ui
                        .add_enabled(can_export, egui::Button::new("XLSX"))
                        .on_hover_text("Export this view as a spreadsheet workbook")
                        .clicked()
                    {
                        export_xlsx_streaming(tab, &grid_conn, ui);
                    }
                    ui.separator();
                    // No scope state here, so it fires as an action rather than a check.
                    if ui
                        .add_enabled(can_export_all, egui::Button::new("All result sets"))
                        .on_hover_text("Export every result set into one folder, one CSV per set")
                        .clicked()
                    {
                        export_all_results(tab);
                    }
                });
            }
            ActionItem::Edit => {
                ui.menu_button(item.label(), |ui| {
                    if ui
                        .button("Add row")
                        .on_hover_text("Stage one empty row for INSERT")
                        .clicked()
                    {
                        actions.push(EditAction::AddRow);
                        ui.close();
                    }
                    if ui
                        .button("Paste rows")
                        .on_hover_text(
                            "Paste TSV or CSV from the clipboard as new rows for review",
                        )
                        .clicked()
                    {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                        ui.ctx()
                            .data_mut(|data| data.insert_temp(paste_armed_id(tab.id), true));
                        ui.close();
                    }
                    let can_fill = selection.active().is_some_and(|(row, col)| {
                        row < total
                            && tab
                                .active_query_result()
                                .is_some_and(|result| col < result.columns().len())
                            && !tab.is_row_deleted(row)
                    });
                    if ui
                        .add_enabled(can_fill, egui::Button::new("Fill down"))
                        .on_hover_text(format!(
                            "Fill the focused cell down the selection ({})",
                            ui.ctx().format_shortcut(&fill_shortcut())
                        ))
                        .clicked()
                    {
                        if let Some((row, col)) = selection.active() {
                            let targets: Vec<usize> = selection.resolve(&visible);
                            actions.extend(fill_from(tab, row, col, &targets));
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .button("Import file")
                        .on_hover_text("Load a CSV or JSONL file into this table")
                        .clicked()
                    {
                        actions.push(EditAction::ImportCsv);
                        ui.close();
                    }
                    if ui
                        .button("Import JSONL")
                        .on_hover_text("Load a JSONL file as new rows for review")
                        .clicked()
                    {
                        actions.push(EditAction::ImportJsonl);
                        ui.close();
                    }
                    if ui
                        .button("Import XLSX")
                        .on_hover_text(
                            "Load the first sheet of an XLSX file as new rows for review",
                        )
                        .clicked()
                    {
                        actions.push(EditAction::ImportXlsx);
                        ui.close();
                    }
                });
            }
            ActionItem::Detail => {
                if ui
                    .selectable_label(side_pane == SidePane::Detail, item.label())
                    .on_hover_text("Show the focused row as JSON and record beside the grid")
                    .clicked()
                {
                    side_pane = if side_pane == SidePane::Detail {
                        SidePane::None
                    } else {
                        SidePane::Detail
                    };
                    // A tail toggle ends like the tail's other items: the
                    // menu it was opened from closes.
                    if egui::menu::is_in_menu(ui) {
                        ui.close();
                    }
                }
            }
            ActionItem::Chart => {
                if ui
                    .selectable_label(side_pane == SidePane::Chart, item.label())
                    .on_hover_text("Plot the result beside the grid")
                    .clicked()
                {
                    side_pane = if side_pane == SidePane::Chart {
                        SidePane::None
                    } else {
                        SidePane::Chart
                    };
                    // A tail toggle ends like the tail's other items: the
                    // menu it was opened from closes.
                    if egui::menu::is_in_menu(ui) {
                        ui.close();
                    }
                }
            }
        }
    };
    ui.horizontal(|ui| {
        let mut text = filter_before;
        let field_w = if one_row {
            (avail - left_fixed - 2.0 * gap - actions_w).clamp(FIELD_MIN, FIELD_W)
        } else {
            (avail - left_fixed - gap).clamp(FIELD_MIN, FIELD_W)
        };
        let field = ui.add(
            egui::TextEdit::singleline(&mut text)
                .hint_text("Filter rows")
                .desired_width(field_w),
        );
        if take_filter_focus(ui.ctx(), tab_id) {
            field.request_focus();
        }
        if field.changed() {
            filter_after = Some(text);
        }
        if clear_shown && ui.button(CLEAR_LABEL).clicked() {
            filter_after = Some(String::new());
        }
        if ui
            .selectable_label(filters_open, filters_label)
            .on_hover_text("Show the structured filter builder")
            .clicked()
        {
            filters_open = !filters_open;
            filters_toggled = true;
        }
        if one_row {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for item in action_items.iter().rev() {
                    draw_action(ui, *item);
                }
            });
        }
    });
    if !one_row {
        ui.horizontal(|ui| {
            let labels: Vec<&str> =
                action_items.iter().map(|item| item.label()).collect();
            fit::packed_row(ui, &action_w, &labels, |ui, index| {
                draw_action(ui, action_items[index]);
            });
        });
    }
    if let Some(filter) = filter_after {
        tab.set_filter(filter);
    }

    store_side_pane(ui.ctx(), tab.id, side_pane);
    if filters_toggled {
        store_filters_open(ui.ctx(), tab.id, filters_open);
    }
    if editable && tab.staged_count() > 0 {
        let staged = tab.staged_count();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("{staged} staged")).small().strong());
            if ui
                .button(format!("Apply ({staged})"))
                .on_hover_text("Send the staged changes")
                .clicked()
            {
                actions.push(EditAction::Apply);
            }
            if ui
                .button("Discard")
                .on_hover_text("Drop every staged change")
                .clicked()
            {
                actions.push(EditAction::Discard);
            }
        });
    }
    if filters_open {
        let kind = grid_conn.as_ref().map(|c| c.config.kind());
        let mut filter_actions = structured_filter_body(tab, &cols, filter_history, kind, ui);
        actions.append(&mut filter_actions);
    }
    let hidden = view.hidden_count(cols.len());
    egui::Panel::bottom("crusty_grid_footer").show(ui, |ui| {
        ui.horizontal(|ui| {
            // The controls pack like the toolbars': measured before anything
            // draws, the inline prefix keeps its place and the contiguous
            // tail moves behind the row's one `…`, so a control the row
            // cannot hold stays reachable. The texts elide into what the
            // pack leaves. The texts reserve in the order of what the row
            // can least afford to lose: the truncation flag (the result is
            // incomplete), a failed-export notice, the counts, the tab's own
            // state, a success notice. Draw order is their own.
            let mut controls: Vec<FooterControl> = Vec::new();
            if truncated && known_total.is_none() {
                controls.push(FooterControl::CountTotal);
            }
            if truncated {
                controls.push(FooterControl::LoadMore);
            }
            if hidden > 0 {
                controls.push(FooterControl::ShowColumns);
            }
            let labels: Vec<String> =
                controls.iter().map(|control| control.label(total, hidden)).collect();
            let widths: Vec<f32> =
                labels.iter().map(|label| fit::button_w(ui, label)).collect();
            let inline = fit::spill(ui, &widths);
            let mut budget = fit::Budget::new(ui);
            for width in &widths[..inline] {
                budget.take(ui, *width);
            }
            let tailed = inline < controls.len();
            if tailed {
                budget.take(ui, fit::button_w(ui, fit::OVERFLOW_LABEL));
            }
            let notice = tab.export_notice().map(str::to_owned);
            let notice_failed = tab.export_failed();
            let truncate_text = if truncated {
                budget.elide(ui, &truncate_label(known_total), egui::TextStyle::Small)
            } else {
                String::new()
            };
            let failed_text = notice
                .as_deref()
                .filter(|_| notice_failed)
                .map(|notice| budget.elide(ui, notice, egui::TextStyle::Body))
                .unwrap_or_default();
            let stats_text = budget.elide(
                ui,
                &format!("{} of {} rows · {:.0} ms", visible.len(), total, ms),
                egui::TextStyle::Small,
            );
            let edit_text = match &edit_label {
                Some(table) => {
                    budget.elide(ui, &format!("editing {table}"), egui::TextStyle::Small)
                }
                None => budget.elide(ui, "read-only view", egui::TextStyle::Small),
            };
            let ok_text = notice
                .as_deref()
                .filter(|_| !notice_failed)
                .map(|notice| budget.elide(ui, notice, egui::TextStyle::Body))
                .unwrap_or_default();
            let notice_text = if notice_failed { failed_text } else { ok_text };

            if !stats_text.is_empty() {
                ui.label(egui::RichText::new(stats_text).small().weak());
            }
            if !truncate_text.is_empty() {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    egui::RichText::new(truncate_text).small().strong(),
                );
            }
            {
                let mut draw = |ui: &mut egui::Ui, index: usize| {
                    draw_footer_control(
                        ui,
                        controls[index],
                        &labels[index],
                        total,
                        tab,
                        &grid_conn,
                        &mut view,
                    );
                };
                for index in 0..inline {
                    draw(ui, index);
                }
                if tailed {
                    let hover = labels[inline..].join(", ");
                    ui.menu_button(fit::OVERFLOW_LABEL, |ui| {
                        for index in inline..controls.len() {
                            draw(ui, index);
                        }
                    })
                    .response
                    .on_hover_text(format!("More: {hover}"));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !edit_text.is_empty() {
                    let label = ui.label(egui::RichText::new(&edit_text).small().weak());
                    if edit_label.is_none() {
                        label.on_hover_text(
                            "Inline editing needs an idle connection and a single-table \
                             SELECT whose table has a primary key in the result.",
                        );
                    }
                }
                if !notice_text.is_empty() {
                    if notice_failed {
                        ui.colored_label(ui.visuals().error_fg_color, notice_text);
                    } else {
                        ui.colored_label(palette.success, notice_text);
                    }
                }
            });
        });
    });
    match side_pane {
        SidePane::Chart => super::charts::chart_side_panel(tab, ui),
        SidePane::Detail => {
            // A nested right panel reserves its side before the grid's scroll
            // area draws, so the grid keeps the leftover width. Read-only
            // content only.
            egui::Panel::right("crusty_row_detail").default_size(360.0).show(ui, |ui| {
                row_detail(tab, &selection, ui);
            });
        }
        SidePane::None => {}
    }
    let ncols = cols.len();
    let active_result = tab.active_result();
    let editor_open = cell_editor_of(ui.ctx(), tab.id).is_some();
    let added = tab.added_rows().len();
    let open_field = edit_field_of(ui.ctx(), tab.id);
    let field_cell = open_field
        .as_ref()
        .filter(|field| field.result == active_result)
        .map(|field| (field.row, field.col));
    let mut pending_sort: Option<usize> = None;
    let shown = view.visible(ncols);
    let widths: Vec<Option<f32>> = shown.iter().map(|col| view.width(*col)).collect();
    let page = page_rows(ui.available_height(), ui.spacing().item_spacing.y);
    let scroll_row = take_grid_scroll(ui.ctx(), tab.id);
    egui::ScrollArea::both().show(ui, |ui| {
        let mut table = TableBuilder::new(ui).striped(true);
        if let Some(row) = scroll_row {
            table = table.scroll_to_row(row, None);
        }
        for width in &widths {
            table = table.column(match width {
                Some(width) => Column::initial(*width).resizable(true),
                None => Column::auto().resizable(true),
            });
        }
        table
            .header(GRID_ROW_H, |mut header| {
                for (i, (name, type_name)) in cols.iter().enumerate() {
                    if view.is_hidden(i) {
                        continue;
                    }
                    let active = sort_col == Some(i);
                    let marker = if active {
                        match sort_dir {
                            SortDir::Asc => " ⏶",
                            SortDir::Desc => " ⏷",
                        }
                    } else {
                        ""
                    };
                    header.col(|ui| {
                        let head = ui.horizontal(|ui| {
                            let sort = ui
                                .selectable_label(
                                    active,
                                    egui::RichText::new(format!("{name}{marker}")).strong(),
                                )
                                .on_hover_text("Click to sort");
                            if sort.clicked() {
                                pending_sort = Some(i);
                            }
                            ui.label(egui::RichText::new(type_name).small().weak());
                            ui.label(
                                egui::RichText::new(format!("#{}", i + 1)).small().weak(),
                            );
                        });
                        head.response.context_menu(|ui| {
                            if ui.button(format!("Hide {name}")).clicked() {
                                view.hide(i);
                                ui.close();
                            }
                            if ui.button("Widen").clicked() {
                                view.widen(i);
                                ui.close();
                            }
                            if ui.button("Narrow").clicked() {
                                view.narrow(i);
                                ui.close();
                            }
                            if ui.button("Reset width").clicked() {
                                view.reset_width(i);
                                ui.close();
                            }
                        });
                    });
                }
            })
            .body(|body| {
                body.rows(GRID_ROW_H, visible.len() + added, |mut row| {
                    let index = row.index();
                    if let Some(added_index) = index.checked_sub(visible.len()) {
                        for c in 0..ncols {
                            if view.is_hidden(c) {
                                continue;
                            }
                            row.col(|ui| {
                                let mut text = tab
                                    .added_rows()
                                    .get(added_index)
                                    .and_then(|cells| cells.get(c))
                                    .cloned()
                                    .unwrap_or_default();
                                if ui
                                    .add(
                                        egui::TextEdit::singleline(&mut text)
                                            .desired_width(f32::INFINITY)
                                            .font(egui::TextStyle::Monospace),
                                    )
                                    .changed()
                                {
                                    actions.push(EditAction::Stage {
                                        row: total + added_index,
                                        col: c,
                                        text,
                                    });
                                }
                            });
                        }
                        return;
                    }
                    let r = visible[index];
                    for c in 0..ncols {
                        if view.is_hidden(c) {
                            continue;
                        }
                        row.col(|ui| {
                            if field_cell == Some((r, c)) {
                                let mut open = open_field.clone().expect("the open field");
                                let response = ui.add(
                                    egui::TextEdit::singleline(&mut open.text)
                                        .desired_width(f32::INFINITY)
                                        .font(egui::TextStyle::Monospace),
                                );
                                if open.fresh {
                                    response.request_focus();
                                    open.fresh = false;
                                }
                                let commit = response.lost_focus()
                                    && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                let cancel = ui.input_mut(|i| {
                                    i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
                                });
                                if commit {
                                    actions.push(EditAction::Stage {
                                        row: r,
                                        col: c,
                                        text: open.text,
                                    });
                                    store_edit_field(ui.ctx(), tab.id, None);
                                } else if cancel {
                                    store_edit_field(ui.ctx(), tab.id, None);
                                } else {
                                    store_edit_field(ui.ctx(), tab.id, Some(open));
                                }
                                return;
                            }
                            let deleted = tab.is_row_deleted(r);
                            let staged = tab.edit_text(r, c);
                            let text = match staged {
                                Some(text) if text == STAGED_DEFAULT => "DEFAULT".to_owned(),
                                Some(text) => text.to_owned(),
                                None => tab
                                    .active_query_result()
                                    .and_then(|result| result.cell(r, c))
                                    .map(|v| v.display(CELL_CAP).into_owned())
                                    .unwrap_or_default(),
                            };
                            let cell_rect = ui.max_rect();
                            let selected = selection.is_selected(r);
                            if selected {
                                ui.painter().rect_filled(
                                    cell_rect,
                                    egui::CornerRadius::ZERO,
                                    ui.visuals().selection.bg_fill,
                                );
                            }
                            if selection.active() == Some((r, c)) {
                                ui.painter().rect_stroke(
                                    cell_rect,
                                    egui::CornerRadius::ZERO,
                                    ui.visuals().selection.stroke,
                                    egui::StrokeKind::Inside,
                                );
                            }
                            if staged.is_some() && !deleted {
                                ui.painter().rect_filled(
                                    cell_rect,
                                    egui::CornerRadius::ZERO,
                                    palette.edit_tint,
                                );
                            }
                            let rich = if deleted {
                                egui::RichText::new(text).strikethrough()
                            } else {
                                egui::RichText::new(text)
                            };
                            let rich = if let Some(color) = cell_label_color(selected, &palette) {
                                rich.color(color)
                            } else if deleted {
                                rich.weak()
                            } else {
                                rich
                            };
                            let label = ui.monospace(rich);
                            let response = label.union(ui.interact(
                                cell_rect,
                                ui.id().with("edit_cell"),
                                egui::Sense::click(),
                            ));
                            if response.clicked() {
                                let (shift, ctrl) = ui.input(|i| {
                                    (i.modifiers.shift, i.modifiers.ctrl || i.modifiers.command)
                                });
                                gestures.push((r, c, shift, ctrl));
                            }
                            if !deleted && editable && !editor_open && response.double_clicked() {
                                let blob = tab
                                    .active_query_result()
                                    .and_then(|result| result.cell(r, c))
                                    .is_some_and(|cell| matches!(cell, Value::Blob(_)));
                                if blob {
                                    if let Some(editor) = open_cell_editor(tab, r, c, active_result)
                                    {
                                        show_cell_editor(ui.ctx(), tab.id, editor);
                                    }
                                } else {
                                    let preload = tab
                                        .active_query_result()
                                        .and_then(|result| result.cell(r, c))
                                        .map(|v| v.display(usize::MAX).into_owned())
                                        .unwrap_or_default();
                                    store_edit_field(
                                        ui.ctx(),
                                        tab.id,
                                        Some(EditField {
                                            row: r,
                                            col: c,
                                            result: active_result,
                                            text: preload,
                                            fresh: true,
                                        }),
                                    );
                                }
                            }
                            if editable {
                                let label = if deleted { "Restore row" } else { "Delete row" };
                                response.context_menu(|ui| {
                                    if !deleted && ui.button("Jump to referenced row").clicked()
                                    {
                                        actions.push(EditAction::JumpFk { row: r, col: c });
                                        ui.close();
                                    }
                                    if !deleted
                                        && !editor_open
                                        && ui.button("Edit cell…").clicked()
                                    {
                                        if let Some(editor) =
                                            open_cell_editor(tab, r, c, active_result)
                                        {
                                            show_cell_editor(ui.ctx(), tab.id, editor);
                                        }
                                        ui.close();
                                    }
                                    if ui.button(label).clicked() {
                                        actions.push(EditAction::ToggleDelete(r));
                                        ui.close();
                                    }
                                });
                            } else if !deleted {
                                response.context_menu(|ui| {
                                    if ui.button("Jump to referenced row").clicked() {
                                        actions.push(EditAction::JumpFk { row: r, col: c });
                                        ui.close();
                                    }
                                });
                            }
                        });
                    }
                });
            });
    });
    let clicked_cell = !gestures.is_empty();
    for (row, col, shift, ctrl) in gestures {
        selection.gesture(row, col, shift, ctrl, &visible);
    }
    if clicked_cell {
        if let Some(focused) = ui.ctx().memory(|memory| memory.focused()) {
            ui.ctx().memory_mut(|memory| memory.surrender_focus(focused));
        }
        set_grid_armed(ui.ctx(), tab.id, true);
    }
    let mut keyed = grid_edit_keys(tab, editable, &selection, &visible, &mut view, modal_open, ui);
    actions.append(&mut keyed);
    if let Some(row) =
        grid_cursor_keys(tab.id, &mut selection, &visible, ncols, page, modal_open, ui)
    {
        request_grid_scroll(ui.ctx(), tab.id, row);
    }
    let mut clipboard = grid_clipboard_keys(tab, editable, &selection, &visible, modal_open, ui);
    actions.append(&mut clipboard);
    let mut editor_modal =
        cell_editor_window(tab, &cols, editable, active_result, cell_editor_key, ui);
    actions.append(&mut editor_modal);
    store_selection(ui.ctx(), tab.id, &selection);
    store_grid_view(ui.ctx(), tab.id, &view);
    if let Some(col) = pending_sort {
        tab.set_sort(col);
    }
    actions
}

/// The save-name box for one tab's structured filter, kept in the egui
/// context data keyed by tab, the pattern the find bar uses.
fn filter_name_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_filter_name", tab))
}

/// The clause editor, raw WHERE box, and named history under the Filters
/// toggle. Clause and raw edits mutate the tab in place; apply, clear, save,
/// and recall defer through [`EditAction`] so the batch send runs after the
/// tab borrow ends. No shortcuts are added, so the existing modal and focus
/// gates hold: typing in a box keeps focus and the grid copy key stays dead.
fn structured_filter_body(
    tab: &mut Tab,
    columns: &[(String, String)],
    history: &[SavedFilter],
    kind: Option<DbKind>,
    ui: &mut egui::Ui,
) -> Vec<EditAction> {
    let mut actions = Vec::new();
    let tab_id = tab.id;
    {
        let mut remove: Option<usize> = None;
        for (index, clause) in tab.filter_clauses_mut().iter_mut().enumerate() {
            ui.horizontal(|ui| {
                if columns.is_empty() {
                    ui.text_edit_singleline(&mut clause.column);
                } else {
                    let current =
                        if clause.column.is_empty() { "<column>" } else { clause.column.as_str() };
                    egui::ComboBox::from_id_salt((tab_id, "column", index))
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            for (name, _) in columns {
                                ui.selectable_value(&mut clause.column, name.clone(), name);
                            }
                        });
                }
                egui::ComboBox::from_id_salt((tab_id, "op", index))
                    .selected_text(clause.op.label())
                    .show_ui(ui, |ui| {
                        for op in FilterOp::ALL {
                            ui.selectable_value(&mut clause.op, op, op.label());
                        }
                    });
                match clause.op {
                    FilterOp::IsNull | FilterOp::IsNotNull => {
                        ui.label(egui::RichText::new("no value").small().weak());
                    }
                    FilterOp::In => {
                        ui.text_edit_singleline(&mut clause.value)
                            .on_hover_text("Comma-separated values");
                    }
                    _ => {
                        ui.text_edit_singleline(&mut clause.value);
                    }
                }
                if ui.button("Remove").clicked() {
                    remove = Some(index);
                }
            });
        }
        if let Some(index) = remove {
            tab.remove_filter_clause(index);
        }
        ui.horizontal(|ui| {
            if ui.button("Add clause").clicked() {
                let column = columns
                    .first()
                    .map(|(name, _)| name.clone())
                    .unwrap_or_default();
                tab.add_filter_clause(FilterClause {
                    column,
                    op: FilterOp::Eq,
                    value: String::new(),
                });
            }
            let single = kind.is_some_and(|kind| editable_table(&tab.sql, kind).is_some());
            let can_apply = single && tab.filter_where().is_some();
            if ui
                .add_enabled(can_apply, egui::Button::new("Apply filter"))
                .on_hover_text("Re-run the table SELECT with the WHERE composed in")
                .clicked()
            {
                actions.push(EditAction::ApplyTableFilter);
            }
            if ui.button(CLEAR_LABEL).clicked() {
                actions.push(EditAction::ClearTableFilter);
            }
            if !single {
                ui.label(
                    egui::RichText::new("needs a single-table SELECT").small().weak(),
                );
            }
        });
        ui.horizontal(|ui| {
            ui.label("Raw WHERE");
            let mut raw = tab.filter_raw().to_owned();
            if ui.text_edit_singleline(&mut raw).changed() {
                tab.set_filter_raw(raw);
            }
        });
        ui.label(
            egui::RichText::new("Raw text is appended as written.").small().weak(),
        );
        ui.horizontal(|ui| {
            ui.label("Save as");
            let mut name: String =
                ui.ctx().data(|data| data.get_temp(filter_name_id(tab_id))).unwrap_or_default();
            if ui.text_edit_singleline(&mut name).changed() {
                ui.ctx()
                    .data_mut(|data| data.insert_temp(filter_name_id(tab_id), name.clone()));
            }
            if ui.button("Save").clicked() {
                actions.push(EditAction::SaveTableFilter { name });
            }
        });
        for entry in history {
            ui.horizontal(|ui| {
                ui.monospace(&entry.name);
                let mut shape = format!("{} clause", entry.clauses.len());
                if entry.clauses.len() != 1 {
                    shape.push('s');
                }
                if !entry.raw.trim().is_empty() {
                    shape.push_str(" + raw");
                }
                ui.label(egui::RichText::new(shape).small().weak());
                if ui.button("Recall").clicked() {
                    actions.push(EditAction::RecallTableFilter { name: entry.name.clone() });
                }
                if ui.button("Delete").clicked() {
                    actions.push(EditAction::DeleteTableFilter { name: entry.name.clone() });
                }
            });
        }
    }
    actions
}

fn export_csv_streaming(tab: &mut Tab, conn: &Option<GridConn>, ui: &mut egui::Ui) {
    start_stream_export(tab, conn, StreamFormat::Csv, export_current_view, ui);
}

fn export_json_streaming(tab: &mut Tab, conn: &Option<GridConn>, ui: &mut egui::Ui) {
    start_stream_export(tab, conn, StreamFormat::Json, export_json_view, ui);
}

fn export_jsonl_streaming(tab: &mut Tab, conn: &Option<GridConn>, ui: &mut egui::Ui) {
    start_stream_export(tab, conn, StreamFormat::Jsonl, export_jsonl_view, ui);
}

fn export_xlsx_streaming(tab: &mut Tab, conn: &Option<GridConn>, ui: &mut egui::Ui) {
    start_stream_export(tab, conn, StreamFormat::Xlsx, export_xlsx_view, ui);
}

fn export_current_view(tab: &mut Tab) {
    let (csv, default_name, nrows) = match tab.active_query_result() {
        Some(result) => {
            if result.columns().is_empty() {
                return;
            }
            let visible = visible_rows(result, tab.sort_col(), tab.sort_dir(), tab.filter());
            let csv = to_csv(result, &visible);
            let name = default_export_name(&tab.sql, "csv");
            (csv, name, visible.len())
        }
        None => return,
    };
    let Some(path) = rfd::FileDialog::new()
        .add_filter("CSV", &["csv"])
        .set_file_name(&default_name)
        .save_file()
    else {
        return;
    };
    match std::fs::write(&path, csv) {
        Ok(()) => tab.set_export_notice(
            format!("Exported {nrows} rows to {}", path.display()),
            false,
        ),
        Err(e) => tab.set_export_notice(format!("Export failed: {e}"), true),
    }
}

fn export_json_view(tab: &mut Tab) {
    let (json, default_name, nrows) = match tab.active_query_result() {
        Some(result) => {
            if result.columns().is_empty() {
                return;
            }
            let visible = visible_rows(result, tab.sort_col(), tab.sort_dir(), tab.filter());
            let json = to_json(result, &visible);
            let name = default_export_name(&tab.sql, "json");
            (json, name, visible.len())
        }
        None => return,
    };
    let Some(path) = rfd::FileDialog::new()
        .add_filter("JSON", &["json"])
        .set_file_name(&default_name)
        .save_file()
    else {
        return;
    };
    match std::fs::write(&path, json) {
        Ok(()) => tab.set_export_notice(
            format!("Exported {nrows} rows to {}", path.display()),
            false,
        ),
        Err(e) => tab.set_export_notice(format!("Export failed: {e}"), true),
    }
}

fn export_jsonl_view(tab: &mut Tab) {
    let (jsonl, default_name, nrows) = match tab.active_query_result() {
        Some(result) => {
            if result.columns().is_empty() {
                return;
            }
            let visible = visible_rows(result, tab.sort_col(), tab.sort_dir(), tab.filter());
            let jsonl = to_jsonl(result, &visible);
            let name = default_export_name(&tab.sql, "jsonl");
            (jsonl, name, visible.len())
        }
        None => return,
    };
    let Some(path) = rfd::FileDialog::new()
        .add_filter("JSONL", &["jsonl"])
        .set_file_name(&default_name)
        .save_file()
    else {
        return;
    };
    match std::fs::write(&path, jsonl) {
        Ok(()) => tab.set_export_notice(
            format!("Exported {nrows} rows to {}", path.display()),
            false,
        ),
        Err(e) => tab.set_export_notice(format!("Export failed: {e}"), true),
    }
}

fn export_xlsx_view(tab: &mut Tab) {
    let (bytes, default_name, nrows) = match tab.active_query_result() {
        Some(result) => {
            if result.columns().is_empty() {
                return;
            }
            let visible = visible_rows(result, tab.sort_col(), tab.sort_dir(), tab.filter());
            let bytes = match to_xlsx(result, &visible) {
                Ok(bytes) => bytes,
                Err(error) => {
                    tab.set_export_notice(format!("Export failed: {error}"), true);
                    return;
                }
            };
            let name = default_export_name(&tab.sql, "xlsx");
            (bytes, name, visible.len())
        }
        None => return,
    };
    let Some(path) = rfd::FileDialog::new()
        .add_filter("XLSX", &["xlsx"])
        .set_file_name(&default_name)
        .save_file()
    else {
        return;
    };
    match std::fs::write(&path, bytes) {
        Ok(()) => tab.set_export_notice(
            format!("Exported {nrows} rows to {}", path.display()),
            false,
        ),
        Err(e) => tab.set_export_notice(format!("Export failed: {e}"), true),
    }
}

/// A stable file-name slug: alphanumerics kept lowercase, every other run
/// collapsed to one dash, capped at 40 characters, and `query` when nothing
/// survives.
fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut separated = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if separated && !out.is_empty() {
                out.push('-');
            }
            separated = false;
            out.extend(c.to_lowercase());
        } else {
            separated = true;
        }
    }
    let capped: String = out.chars().take(40).collect();
    let capped = capped.trim_end_matches('-');
    if capped.is_empty() { "query".to_owned() } else { capped.to_owned() }
}

/// The default name for one export of `sql`: [`slug`] of its tab title plus
/// the format's extension, so a table name with spaces or slashes never
/// lands in a file dialog as a path.
fn default_export_name(sql: &str, extension: &str) -> String {
    format!("{}.{extension}", slug(&super::query::tab_title(sql.to_owned())))
}

/// One default file name per result set in run order: `<slug>-result-N.csv`,
/// N the 1-based statement index the set came from. A set without columns
/// keeps its number, so skipping it never renumbers a later set.
fn result_file_names(slug: &str, count: usize) -> Vec<String> {
    (1..=count).map(|index| format!("{slug}-result-{index}.csv")).collect()
}

/// The files a multi-result export writes: one default name and one CSV text
/// per set with columns, in run order. Sets without columns write nothing and
/// keep their number, and the filter and sort shape every set the way the
/// grid shows them.
fn result_files(
    results: &[QueryResult],
    slug: &str,
    sort_col: Option<usize>,
    sort_dir: SortDir,
    filter: &str,
) -> Vec<(String, String)> {
    let names = result_file_names(slug, results.len());
    results
        .iter()
        .enumerate()
        .filter(|(_, result)| !result.columns().is_empty())
        .map(|(index, result)| {
            let visible = visible_rows(result, sort_col, sort_dir, filter);
            (names[index].clone(), to_csv(result, &visible))
        })
        .collect()
}

/// Every result set the tab holds into one chosen folder, one file per set
/// named by [`result_file_names`].
fn export_all_results(tab: &mut Tab) {
    let files = result_files(
        tab.results(),
        &slug(&super::query::tab_title(tab.sql.clone())),
        tab.sort_col(),
        tab.sort_dir(),
        tab.filter(),
    );
    if files.is_empty() {
        return;
    }
    let Some(dir) = rfd::FileDialog::new().pick_folder() else { return };
    let mut written = 0;
    let mut failed: Option<String> = None;
    for (name, csv) in &files {
        match std::fs::write(dir.join(name), csv) {
            Ok(()) => written += 1,
            Err(error) => {
                failed = Some(error.to_string());
                break;
            }
        }
    }
    match failed {
        None => tab.set_export_notice(
            format!("Exported {written} result sets to {}", dir.display()),
            false,
        ),
        Some(error) if written == 0 => {
            tab.set_export_notice(format!("Export failed: {error}"), true);
        }
        Some(error) => {
            tab.set_export_notice(format!("Export failed after {written} files: {error}"), true);
        }
    }
}

pub fn copy_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::C)
}

/// Ctrl+Shift+C copies the active cell as plain text; Ctrl+C alone copies the
/// selected rows as TSV.
pub fn copy_cell_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::C)
}

/// One cell as plain text for the clipboard: the whole value, blobs as size
/// tags, and NULL empty, the way a TSV field renders one without the escapes
/// a single cell does not need.
fn cell_text(cell: Option<&Value>) -> String {
    match cell {
        None | Some(Value::Null) => String::new(),
        Some(value) => value.display(usize::MAX).into_owned(),
    }
}

/// Ctrl+V pastes through the same mapping as the Paste rows button.
pub fn paste_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::V)
}

/// Ctrl+D fills the focused cell down the selection.
pub fn fill_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::D)
}

/// Ctrl+H hides the focused column; the Columns button restores. Ctrl on
/// every platform, since Cmd+H hides the app on macOS.
pub fn hide_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::H)
}

/// Grid clipboard keys, gated like the query editor: dead while any modal
/// is open, while a cell editor is open, or while a text field holds focus,
/// so typing never copies the grid or stages rows. Paste arrives as an
/// `Event::Paste` the frame after the button requests it.
fn grid_clipboard_keys(
    tab: &mut Tab,
    editable: bool,
    selection: &Selection,
    visible: &[usize],
    modal_open: bool,
    ui: &mut egui::Ui,
) -> Vec<EditAction> {
    let mut actions = Vec::new();
    let pastes: Vec<String> = ui.input(|i| {
        i.events
            .iter()
            .filter_map(|event| match event {
                egui::Event::Paste(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    });
    if !pastes.is_empty() {
        let armed: bool =
            ui.ctx().data(|data| data.get_temp(paste_armed_id(tab.id))).unwrap_or(false);
        let typing = edit_field_of(ui.ctx(), tab.id).is_some()
            || ui.ctx().memory(|memory| memory.focused().is_some());
        if editable && (armed || (!modal_open && !typing)) {
            for text in &pastes {
                match parse_paste(text) {
                    None => tab.set_export_notice(
                        "Paste failed: the clipboard needs a header and rows of one width"
                            .to_owned(),
                        true,
                    ),
                    Some(parsed) => match stage_parsed_rows(&parsed, &column_names(tab)) {
                        Some(staged) => actions.push(EditAction::PasteRows(staged)),
                        None => tab.set_export_notice(
                            "Paste failed: no pasted header matches the result columns"
                                .to_owned(),
                            true,
                        ),
                    },
                }
            }
            ui.ctx().data_mut(|data| data.remove::<bool>(paste_armed_id(tab.id)));
        }
    }
    let typing = edit_field_of(ui.ctx(), tab.id).is_some()
        || ui.ctx().memory(|memory| memory.focused().is_some());
    if !modal_open && !typing {
        // The Shift chord goes first: `consume_shortcut` matches an event
        // with extra Shift, so the plain pattern would swallow it and copy
        // the selected rows instead of the one cell.
        if ui.input_mut(|i| i.consume_shortcut(&copy_cell_shortcut())) {
            match selection.active().and_then(|(row, col)| {
                tab.active_query_result().and_then(|result| result.cell(row, col))
            }) {
                Some(cell) => {
                    ui.ctx().copy_text(cell_text(Some(cell)));
                    tab.set_export_notice("Copied 1 cell".to_owned(), false);
                }
                None => tab.set_export_notice("Click a cell to copy it".to_owned(), true),
            }
        } else if ui.input_mut(|i| i.consume_shortcut(&copy_shortcut())) {
            let rows = selection.resolve(visible);
            let headers = tsv_headers_of(ui.ctx(), tab.id);
            let text = tab
                .active_query_result()
                .map(|result| to_tsv(result, &rows, headers))
                .unwrap_or_default();
            let nrows = rows.len();
            ui.ctx().copy_text(text);
            tab.set_export_notice(format!("Copied {nrows} rows as TSV"), false);
        }
    }
    actions
}

/// Spreadsheet keys over the focused cell, gated like
/// [`grid_clipboard_keys`]: dead while any modal is open, while a cell editor
/// is open, or while a text field holds focus, so typing never stages rows by
/// accident. Typing replaces the cell through the staged-edit path, Backspace
/// stages NULL, Enter opens the cell editor modal, Ctrl+D fills the selection,
/// and Ctrl+V arms a paste through
/// the existing mapping. Hiding and resizing are view state and stay
/// available read-only, the way sorting does.
fn grid_edit_keys(
    tab: &mut Tab,
    editable: bool,
    selection: &Selection,
    visible: &[usize],
    view: &mut GridView,
    modal_open: bool,
    ui: &mut egui::Ui,
) -> Vec<EditAction> {
    let mut actions = Vec::new();
    if modal_open {
        return actions;
    }
    let typing = edit_field_of(ui.ctx(), tab.id).is_some()
        || ui.ctx().memory(|memory| memory.focused().is_some());
    if typing {
        return actions;
    }
    let (total, ncols) = tab
        .active_query_result()
        .map(|result| (result.row_count(), result.columns().len()))
        .unwrap_or((0, 0));
    if ui.input_mut(|i| i.consume_shortcut(&hide_shortcut()))
        && let Some((_, col)) = selection.active()
        && col < ncols
    {
        view.hide(col);
    }
    if let Some((_, col)) = selection.active()
        && col < ncols
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowRight))
    {
        view.widen(col);
    } else if let Some((_, col)) = selection.active()
        && col < ncols
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowLeft))
    {
        view.narrow(col);
    }
    if !editable {
        return actions;
    }
    if ui.input_mut(|i| i.consume_shortcut(&paste_shortcut())) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
        ui.ctx().data_mut(|data| data.insert_temp(paste_armed_id(tab.id), true));
    }
    let Some((row, col)) = selection.active() else {
        return actions;
    };
    if row >= total || col >= ncols || tab.is_row_deleted(row) {
        return actions;
    }
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Backspace)) {
        actions.push(null_stage(row, col));
        return actions;
    }
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
        && let Some(editor) = open_cell_editor(tab, row, col, tab.active_result())
    {
        show_cell_editor(ui.ctx(), tab.id, editor);
        return actions;
    }
    if ui.input_mut(|i| i.consume_shortcut(&fill_shortcut())) {
        let targets: Vec<usize> = selection.resolve(visible);
        actions.extend(fill_from(tab, row, col, &targets));
        return actions;
    }
    if ui.input(|i| i.modifiers).is_none() {
        let seed = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Text(text) => Some(text.clone()),
                    _ => None,
                })
                .next()
        });
        if let Some(seed) = seed {
            let blob = tab
                .active_query_result()
                .and_then(|result| result.cell(row, col))
                .is_some_and(|cell| matches!(cell, Value::Blob(_)));
            if !blob {
                store_edit_field(
                    ui.ctx(),
                    tab.id,
                    Some(type_edit_field(row, col, tab.active_result(), seed)),
                );
            }
        }
    }
    actions
}

/// Arrow, Home/End, Page, and Tab movement of the grid cursor. The grid only
/// owns those keys while [`grid_armed`] says so, which a cell click or a
/// cursor key with nothing focused arms. Returns the table row the view
/// should scroll to.
fn grid_cursor_keys(
    tab: TabId,
    selection: &mut Selection,
    visible: &[usize],
    cols: usize,
    page: usize,
    modal_open: bool,
    ui: &egui::Ui,
) -> Option<usize> {
    if modal_open {
        set_grid_armed(ui.ctx(), tab, false);
        return None;
    }
    let armed = grid_armed(ui.ctx(), tab);
    let (shift, alt) = ui.input(|input| (input.modifiers.shift, input.modifiers.alt));
    let tab_pressed = ui.input(|input| input.key_pressed(egui::Key::Tab));
    let typing = edit_field_of(ui.ctx(), tab).is_some();
    let takes_tab = armed && !visible.is_empty() && cols > 0 && !typing;
    let focused = ui.ctx().memory(|memory| memory.focused());
    if let Some(focused) = focused {
        let newly_focused =
            !ui.ctx().memory(|memory| memory.had_focus_last_frame(focused));
        // Tab traversal runs before this handler. Only a Tab the grid can
        // step on drops the focus traversal gave it; every other focus means
        // the chrome owns the keyboard and the press stays with it.
        if !(tab_pressed && takes_tab && newly_focused) {
            set_grid_armed(ui.ctx(), tab, false);
            return None;
        }
        ui.ctx().memory_mut(|memory| memory.surrender_focus(focused));
    }
    if typing {
        return None;
    }
    if tab_pressed {
        if takes_tab
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Tab))
        {
            selection.tab_step(shift, visible, cols, page);
            return selection.position(visible);
        }
        return None;
    }
    let mut cursor: Option<Step> = None;
    let mut take = |modifiers: egui::Modifiers, key: egui::Key, step: Step| {
        if cursor.is_none() && ui.input_mut(|input| input.consume_key(modifiers, key)) {
            cursor = Some(step);
        }
    };
    // Alt stays with the resize keys `grid_edit_keys` takes first; a plain
    // pattern would match Alt+Arrow too once no column is active.
    if !alt {
        take(egui::Modifiers::COMMAND, egui::Key::Home, Step::First);
        take(egui::Modifiers::COMMAND, egui::Key::End, Step::Last);
        take(egui::Modifiers::NONE, egui::Key::Home, Step::RowStart);
        take(egui::Modifiers::NONE, egui::Key::End, Step::RowEnd);
        take(egui::Modifiers::NONE, egui::Key::PageUp, Step::PageUp);
        take(egui::Modifiers::NONE, egui::Key::PageDown, Step::PageDown);
        take(egui::Modifiers::NONE, egui::Key::ArrowUp, Step::Up);
        take(egui::Modifiers::NONE, egui::Key::ArrowDown, Step::Down);
        take(egui::Modifiers::NONE, egui::Key::ArrowLeft, Step::Left);
        take(egui::Modifiers::NONE, egui::Key::ArrowRight, Step::Right);
    }
    let step = cursor?;
    selection.step(step, shift, visible, cols, page);
    set_grid_armed(ui.ctx(), tab, true);
    selection.position(visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Column;
    use crate::edit::{insert_statement, to_literal};

    fn one_col(values: Vec<Value>) -> QueryResult {
        QueryResult::from_columns(
            vec![Column::new("v".to_owned(), "ANY".to_owned(), values)],
            false,
            None,
            std::time::Duration::from_millis(1),
        )
        .unwrap()
    }

    fn two_col(first: Vec<Value>, second: Vec<Value>) -> QueryResult {
        QueryResult::from_columns(
            vec![
                Column::new("a".to_owned(), "ANY".to_owned(), first),
                Column::new("b".to_owned(), "TEXT".to_owned(), second),
            ],
            false,
            None,
            std::time::Duration::from_millis(1),
        )
        .unwrap()
    }

    #[test]
    fn natural_order_without_sort_or_filter() {
        let result = one_col(vec![
            Value::Integer(3),
            Value::Integer(1),
            Value::Integer(2),
        ]);
        assert_eq!(visible_rows(&result, None, SortDir::Asc, ""), vec![0, 1, 2]);
    }

    #[test]
    fn integers_sort_asc_and_desc() {
        let result = one_col(vec![
            Value::Integer(3),
            Value::Integer(1),
            Value::Integer(2),
        ]);
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Asc, ""),
            vec![1, 2, 0]
        );
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Desc, ""),
            vec![0, 2, 1]
        );
    }

    #[test]
    fn reals_sort_numerically() {
        let result = one_col(vec![Value::Real(2.5), Value::Real(10.0), Value::Real(2.25)]);
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Asc, ""),
            vec![2, 0, 1]
        );
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Desc, ""),
            vec![1, 0, 2]
        );
    }

    #[test]
    fn mixed_integer_and_real_compare_numerically() {
        let result = one_col(vec![
            Value::Integer(10),
            Value::Real(2.5),
            Value::Integer(2),
        ]);
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Asc, ""),
            vec![2, 1, 0]
        );
    }

    #[test]
    fn text_sorts_lexically() {
        let result = one_col(vec![
            Value::Text("banana".to_owned()),
            Value::Text("Apple".to_owned()),
            Value::Text("cherry".to_owned()),
        ]);
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Asc, ""),
            vec![1, 0, 2]
        );
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Desc, ""),
            vec![2, 0, 1]
        );
    }

    #[test]
    fn null_sorts_first_in_both_directions() {
        let result = one_col(vec![
            Value::Integer(2),
            Value::Null,
            Value::Integer(1),
        ]);
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Asc, ""),
            vec![1, 2, 0]
        );
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Desc, ""),
            vec![1, 0, 2]
        );
    }

    #[test]
    fn blobs_sort_by_length_then_bytes() {
        let result = one_col(vec![
            Value::Blob(vec![0x02]),
            Value::Blob(vec![0x01, 0x02]),
            Value::Blob(vec![0x01]),
        ]);
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Asc, ""),
            vec![2, 0, 1]
        );
    }

    #[test]
    fn filter_matches_case_insensitive_substring() {
        let result = two_col(
            vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
            vec![
                Value::Text("Ada".to_owned()),
                Value::Text("Grace".to_owned()),
                Value::Text("ADAM".to_owned()),
            ],
        );
        assert_eq!(visible_rows(&result, None, SortDir::Asc, "ada"), vec![0, 2]);
        assert_eq!(visible_rows(&result, None, SortDir::Asc, "GRACE"), vec![1]);
        assert_eq!(
            visible_rows(&result, None, SortDir::Asc, ""),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn filter_scans_every_cell_in_row() {
        let result = two_col(
            vec![Value::Integer(42), Value::Integer(7)],
            vec![Value::Text("zzz".to_owned()), Value::Text("aaa".to_owned())],
        );
        assert_eq!(visible_rows(&result, None, SortDir::Asc, "42"), vec![0]);
        assert_eq!(visible_rows(&result, None, SortDir::Asc, "aaa"), vec![1]);
    }

    #[test]
    fn sort_and_filter_combine() {
        let result = two_col(
            vec![Value::Integer(3), Value::Integer(1), Value::Integer(2)],
            vec![
                Value::Text("ada".to_owned()),
                Value::Text("grace".to_owned()),
                Value::Text("adam".to_owned()),
            ],
        );
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Asc, "ada"),
            vec![2, 0]
        );
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Desc, "ada"),
            vec![0, 2]
        );
    }

    #[test]
    fn out_of_range_sort_col_keeps_filtered_order() {
        let result = one_col(vec![Value::Integer(3), Value::Integer(1)]);
        assert_eq!(
            visible_rows(&result, Some(9), SortDir::Asc, ""),
            vec![0, 1]
        );
        assert_eq!(
            visible_rows(&result, Some(9), SortDir::Desc, "1"),
            vec![1]
        );
    }

    #[test]
    fn ties_keep_original_index_order() {
        let result = two_col(
            vec![Value::Integer(1), Value::Integer(1), Value::Integer(1)],
            vec![
                Value::Text("c".to_owned()),
                Value::Text("a".to_owned()),
                Value::Text("b".to_owned()),
            ],
        );
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Asc, ""),
            vec![0, 1, 2]
        );
        assert_eq!(
            visible_rows(&result, Some(0), SortDir::Desc, ""),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn click_shift_and_ctrl_gestures_shape_the_selection() {
        let mut selection = Selection::default();
        selection.gesture(1, 0, false, false, &[0, 1, 2]);
        assert_eq!(selection.resolve(&[0, 1, 2]), vec![1]);
        assert_eq!(selection.active(), Some((1, 0)));
        selection.gesture(2, 1, true, false, &[0, 1, 2]);
        assert_eq!(selection.resolve(&[0, 1, 2]), vec![1, 2]);
        assert_eq!(selection.active(), Some((2, 1)));
        selection.gesture(0, 0, false, true, &[0, 1, 2]);
        assert_eq!(selection.resolve(&[2, 0, 1]), vec![2, 0, 1]);
        assert_eq!(selection.active(), Some((0, 0)));
        selection.gesture(1, 0, false, true, &[0, 1, 2]);
        assert_eq!(selection.resolve(&[0, 1, 2]), vec![0, 2]);
    }

    #[test]
    fn shift_without_an_anchor_selects_one_row() {
        let mut selection = Selection::default();
        selection.gesture(2, 1, true, false, &[0, 1, 2]);
        assert_eq!(selection.resolve(&[0, 1, 2]), vec![2]);
        assert_eq!(selection.active(), Some((2, 1)));
    }

    #[test]
    fn a_shift_step_extends_over_view_positions_not_raw_rows() {
        let visible = [5, 1, 9, 3];
        let mut selection = Selection::default();
        selection.click(5, 0);
        selection.step(Step::Down, true, &visible, 2, 1);
        assert_eq!(
            selection.resolve(&visible),
            vec![5, 1],
            "the step selects the two view positions it passed"
        );
    }

    #[test]
    fn a_shift_click_extends_over_view_positions_not_raw_rows() {
        let visible = [5, 1, 9, 3];
        let mut selection = Selection::default();
        selection.gesture(5, 0, false, false, &visible);
        selection.gesture(9, 1, true, false, &visible);
        assert_eq!(
            selection.resolve(&visible),
            vec![5, 1, 9],
            "the click selects the three displayed rows it passed"
        );
    }

    #[test]
    fn a_shift_click_with_an_off_view_anchor_selects_one_row() {
        let visible = [5, 1, 9, 3];
        let mut selection = Selection::default();
        selection.click(2, 0);
        selection.gesture(9, 1, true, false, &visible);
        assert_eq!(
            selection.resolve(&visible),
            vec![9],
            "an anchor the filter hid never selects rows between raw indices"
        );
    }

    #[test]
    fn an_empty_selection_resolves_to_the_whole_view() {
        let selection = Selection::default();
        assert!(selection.is_empty());
        assert_eq!(selection.resolve(&[2, 0]), vec![2, 0]);
    }

    #[test]
    fn sync_keeps_the_rows_until_the_result_changes() {
        use std::time::Duration;
        let first =
            ResultId { active: 0, rows: 2, cols: 2, took: Duration::from_millis(1) };
        let rerun =
            ResultId { active: 0, rows: 2, cols: 2, took: Duration::from_millis(2) };
        let mut selection = Selection::default();
        selection.sync(first);
        selection.gesture(0, 0, false, false, &[0, 1, 2]);
        selection.sync(first);
        assert_eq!(selection.resolve(&[0, 1]), vec![0]);
        selection.sync(rerun);
        assert!(selection.is_empty());
        assert_eq!(selection.active(), None);
        assert_eq!(selection.resolve(&[0, 1]), vec![0, 1]);
    }

    #[test]
    fn truncate_label_names_the_count_beside_the_flag() {
        assert_eq!(truncate_label(None), "truncated at 10,000");
        assert_eq!(truncate_label(Some(12_500)), "truncated at 10,000 · 12500 total");
    }

    fn paste_columns() -> Vec<String> {
        vec!["id".to_owned(), "name".to_owned()]
    }

    #[test]
    fn paste_csv_maps_headers_case_insensitively_in_column_order() {
        let parsed = parse_paste("NAME,extra,ID\nada,x,1\ngrace,y,2\n").expect("parses");
        assert_eq!(
            stage_parsed_rows(&parsed, &paste_columns()),
            Some(vec![
                vec!["1".to_owned(), "ada".to_owned()],
                vec!["2".to_owned(), "grace".to_owned()],
            ])
        );
    }

    #[test]
    fn paste_tsv_roundtrip_keeps_quoted_cells() {
        let parsed =
            parse_paste("id\tname\n1\t\"a\tb\"\n2\tplain\n").expect("parses");
        assert_eq!(
            stage_parsed_rows(&parsed, &paste_columns()),
            Some(vec![
                vec!["1".to_owned(), "a\tb".to_owned()],
                vec!["2".to_owned(), "plain".to_owned()],
            ])
        );
    }

    #[test]
    fn paste_rejects_garbage_and_unmatched_headers() {
        assert_eq!(parse_paste("just text"), None);
        assert_eq!(parse_paste("a,b\n1\n"), None);
        let parsed = parse_paste("zzz\n1\n").expect("parses");
        assert_eq!(stage_parsed_rows(&parsed, &paste_columns()), None);
    }

    #[test]
    fn paste_caps_rows_at_the_import_cap() {
        let mut text = String::from("id\n");
        for i in 0..1005 {
            text.push_str(&format!("{i}\n"));
        }
        let parsed = parse_paste(&text).expect("parses");
        let staged =
            stage_parsed_rows(&parsed, &["id".to_owned()]).expect("stages");
        assert_eq!(staged.len(), crate::import::ROW_CAP);
        assert_eq!(staged[0], vec!["0".to_owned()]);
        assert_eq!(staged[staged.len() - 1], vec!["999".to_owned()]);
    }

    #[test]
    fn jsonl_import_stages_a_ragged_union_onto_the_result_columns() {
        let text = "{\"id\": 1, \"name\": \"ada\"}\n{\"name\": \"grace\", \"extra\": \"x\"}\n";
        let parsed = parse_jsonl(text).expect("parses");
        assert_eq!(parsed.headers, vec!["id", "name", "extra"]);
        assert_eq!(
            stage_parsed_rows(&parsed, &["name".to_owned(), "id".to_owned()]),
            Some(vec![
                vec!["ada".to_owned(), "1".to_owned()],
                vec!["grace".to_owned(), String::new()],
            ])
        );
        assert_eq!(stage_parsed_rows(&parsed, &["zzz".to_owned()]), None);
    }

    #[test]
    fn jsonl_import_caps_rows_at_the_import_cap() {
        let mut text = String::new();
        for i in 0..1005 {
            text.push_str(&format!("{{\"id\": {i}}}\n"));
        }
        let parsed = parse_jsonl(&text).expect("parses");
        let staged = stage_parsed_rows(&parsed, &["id".to_owned()]).expect("stages");
        assert_eq!(staged.len(), crate::import::ROW_CAP);
        assert_eq!(staged[0], vec!["0".to_owned()]);
        assert_eq!(staged[staged.len() - 1], vec!["999".to_owned()]);
    }

    #[test]
    fn jsonl_export_roundtrips_through_parse_into_staged_rows() {
        let result = two_col(
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Text("ada".to_owned()), Value::Null],
        );
        let text = to_jsonl(&result, &[0, 1]);
        assert_eq!(text, "{\"a\": 1, \"b\": \"ada\"}\n{\"a\": 2, \"b\": null}\n");
        let parsed = parse_jsonl(&text).expect("parses");
        assert_eq!(parsed.headers, vec!["a", "b"]);
        assert_eq!(
            stage_parsed_rows(&parsed, &["b".to_owned(), "a".to_owned()]),
            Some(vec![
                vec!["ada".to_owned(), "1".to_owned()],
                vec![String::new(), "2".to_owned()],
            ])
        );
    }

    #[test]
    fn xlsx_export_roundtrips_through_parse_into_staged_rows() {
        let result = two_col(
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Text("ada".to_owned()), Value::Null],
        );
        let bytes = to_xlsx(&result, &[0, 1]).expect("writes");
        let workbook = parse_xlsx(&bytes).expect("parses");
        assert_eq!(workbook.sheet_count, 1);
        assert_eq!(workbook.parsed.headers, vec!["a", "b"]);
        assert_eq!(
            stage_parsed_rows(&workbook.parsed, &["b".to_owned(), "a".to_owned()]),
            Some(vec![
                vec!["ada".to_owned(), "1".to_owned()],
                vec![String::new(), "2".to_owned()],
            ])
        );
        assert_eq!(stage_parsed_rows(&workbook.parsed, &["zzz".to_owned()]), None);
    }

    #[test]
    fn xlsx_import_caps_rows_at_the_import_cap() {
        let numbers: Vec<Value> = (0..1005).map(|i| Value::Integer(i as i64)).collect();
        let result = two_col(numbers, vec![Value::Null; 1005]);
        let bytes = to_xlsx(&result, &(0..1005).collect::<Vec<_>>()).expect("writes");
        let workbook = parse_xlsx(&bytes).expect("parses");
        let staged = stage_parsed_rows(&workbook.parsed, &["a".to_owned()]).expect("stages");
        assert_eq!(staged.len(), crate::import::ROW_CAP);
        assert_eq!(staged[0], vec!["0".to_owned()]);
        assert_eq!(staged[staged.len() - 1], vec!["999".to_owned()]);
    }

    #[test]
    fn default_export_names_slug_the_table_title() {
        assert_eq!(
            default_export_name("SELECT * FROM users", "csv"),
            "users.csv".to_owned()
        );
        // A quoted name with a space and a numbered twin steer clear of the
        // dialog's path and of the tab numbering.
        assert_eq!(
            default_export_name("SELECT * FROM \"users 2\"", "json"),
            "users-2.json".to_owned()
        );
        assert_eq!(
            default_export_name("SELECT * FROM \"a/b\\\\c\"", "xlsx"),
            "a-b-c.xlsx".to_owned()
        );
    }

    #[test]
    fn multi_result_names_slug_the_title_and_statement_index() {
        assert_eq!(slug("SELECT * FROM users"), "select-from-users");
        assert_eq!(slug("SELECT 1; SELECT 2"), "select-1-select-2");
        assert_eq!(slug("* * *"), "query");
        assert_eq!(slug(&"x".repeat(50)), "x".repeat(40));
        assert_eq!(slug(&format!("{} y", "x".repeat(39))), "x".repeat(39));
        assert_eq!(
            result_file_names(&slug("SELECT * FROM users"), 3),
            vec![
                "select-from-users-result-1.csv".to_owned(),
                "select-from-users-result-2.csv".to_owned(),
                "select-from-users-result-3.csv".to_owned(),
            ]
        );
    }

    #[test]
    fn multi_export_files_skip_columnless_sets_without_renumbering() {
        let sets = vec![
            two_col(vec![Value::Integer(1)], vec![Value::Text("ada".to_owned())]),
            QueryResult::from_columns(
                Vec::new(),
                false,
                Some(2),
                std::time::Duration::from_millis(1),
            )
            .unwrap(),
            two_col(vec![Value::Integer(3)], vec![Value::Null]),
        ];
        assert_eq!(
            result_files(&sets, "select-a", None, SortDir::Asc, ""),
            vec![
                ("select-a-result-1.csv".to_owned(), "a,b\r\n1,ada\r\n".to_owned()),
                ("select-a-result-3.csv".to_owned(), "a,b\r\n3,\r\n".to_owned()),
            ]
        );
        let filtered = result_files(&sets, "select-a", None, SortDir::Asc, "ada");
        assert_eq!(filtered[1].1, "a,b\r\n");
    }

    #[test]
    fn copy_shortcut_is_command_c() {
        let shortcut = copy_shortcut();
        assert_eq!(shortcut.modifiers, egui::Modifiers::COMMAND);
        assert_eq!(shortcut.logical_key, egui::Key::C);
    }

    #[test]
    fn copy_cell_shortcut_is_command_shift_c() {
        let shortcut = copy_cell_shortcut();
        assert_eq!(shortcut.modifiers, egui::Modifiers::COMMAND | egui::Modifiers::SHIFT);
        assert_eq!(shortcut.logical_key, egui::Key::C);
    }

    #[test]
    fn a_copied_cell_is_plain_text_with_null_empty() {
        assert_eq!(cell_text(None), String::new());
        assert_eq!(cell_text(Some(&Value::Null)), String::new());
        assert_eq!(cell_text(Some(&Value::Integer(7))), "7");
        assert_eq!(cell_text(Some(&Value::Real(2.5))), "2.5");
        assert_eq!(
            cell_text(Some(&Value::Text("a\tb".to_owned()))),
            "a\tb",
            "a single cell keeps the tab a TSV field would flatten"
        );
        assert_eq!(cell_text(Some(&Value::Blob(vec![0x01, 0x02]))), "[BLOB 2 B]");
    }

    #[test]
    fn a_copied_cell_keeps_text_past_the_display_cap() {
        let long = "x".repeat(CELL_CAP + 50);
        assert_eq!(cell_text(Some(&Value::Text(long.clone()))), long);
    }

    #[test]
    fn type_to_edit_seeds_the_field_and_stages_its_literal() {
        let field = type_edit_field(2, 1, 0, "7".to_owned());
        assert_eq!((field.row, field.col), (2, 1));
        assert_eq!(field.text, "7");
        assert!(field.fresh);
        assert_eq!(to_literal(&field.text, false), "7");
        let quoted = type_edit_field(0, 0, 0, "O'Brien".to_owned());
        assert_eq!(to_literal(&quoted.text, false), "'O''Brien'");
    }

    #[test]
    fn backspace_stages_empty_text_which_renders_null() {
        let action = null_stage(1, 0);
        match action {
            EditAction::Stage { row, col, text } => {
                assert_eq!((row, col), (1, 0));
                assert_eq!(text, String::new());
                assert_eq!(to_literal(&text, false), "NULL");
            }
            _ => panic!("Backspace must stage through the edit path"),
        }
    }

    #[test]
    fn column_hide_and_resize_state_is_exact() {
        let mut view = GridView::default();
        assert_eq!(view.visible(4), vec![0, 1, 2, 3]);
        view.hide(1);
        view.hide(3);
        assert_eq!(view.visible(4), vec![0, 2]);
        assert_eq!(view.hidden_count(4), 2);
        view.hide(9);
        assert_eq!(view.hidden_count(4), 2);
        view.show_all();
        assert_eq!(view.visible(4), vec![0, 1, 2, 3]);
        assert_eq!(view.width(0), None);
        view.set_width(0, 120.0);
        assert_eq!(view.width(0), Some(120.0));
        view.set_width(0, 10.0);
        assert_eq!(view.width(0), Some(GRID_MIN_WIDTH));
        view.set_width(0, 9999.0);
        assert_eq!(view.width(0), Some(GRID_MAX_WIDTH));
        view.reset_width(0);
        assert_eq!(view.width(0), None);
        view.widen(2);
        assert_eq!(view.width(2), Some(GRID_DEFAULT_WIDTH + GRID_WIDTH_STEP));
        view.narrow(2);
        assert_eq!(view.width(2), Some(GRID_DEFAULT_WIDTH));
        view.narrow(2);
        assert_eq!(view.width(2), Some(GRID_DEFAULT_WIDTH - GRID_WIDTH_STEP));
    }

    #[test]
    fn range_fill_marks_every_target_through_stage() {
        let shortcut = fill_shortcut();
        assert_eq!(shortcut.modifiers, egui::Modifiers::COMMAND);
        assert_eq!(shortcut.logical_key, egui::Key::D);
        let actions = fill_stages(0, 1, &[0, 1, 2], "ada".to_owned());
        assert_eq!(actions.len(), 2);
        for action in &actions {
            match action {
                EditAction::Stage { row, col, text } => {
                    assert_ne!(*row, 0);
                    assert_eq!(*col, 1);
                    assert_eq!(text.as_str(), "ada");
                    assert_eq!(to_literal(text, false), "'ada'");
                }
                _ => panic!("fill must stage through the edit path"),
            }
        }
        let nulls = fill_stages(0, 0, &[0, 1], String::new());
        assert_eq!(nulls.len(), 1);
        match &nulls[0] {
            EditAction::Stage { row, text, .. } => {
                assert_eq!(*row, 1);
                assert_eq!(to_literal(text, false), "NULL");
            }
            _ => panic!("fill must stage through the edit path"),
        }
    }

    #[test]
    fn keyboard_paste_maps_through_the_staged_rows() {
        let shortcut = paste_shortcut();
        assert_eq!(shortcut.modifiers, egui::Modifiers::COMMAND);
        assert_eq!(shortcut.logical_key, egui::Key::V);
        let hide = hide_shortcut();
        assert_eq!(hide.modifiers, egui::Modifiers::CTRL);
        assert_eq!(hide.logical_key, egui::Key::H);
        let columns = vec!["id".to_owned(), "name".to_owned()];
        let parsed = parse_paste("id,name\n1,ada\n").expect("parses");
        let staged = stage_parsed_rows(&parsed, &columns).expect("stages");
        assert_eq!(staged, vec![vec!["1".to_owned(), "ada".to_owned()]]);
        assert_eq!(
            insert_statement("users", &columns, &staged[0]),
            "INSERT INTO \"users\" (\"id\", \"name\") VALUES (1, 'ada')"
        );
    }

    #[test]
    fn cell_editor_null_action_stages_empty_which_renders_null() {
        let action = null_stage(3, 2);
        match action {
            EditAction::Stage { row, col, text } => {
                assert_eq!((row, col), (3, 2));
                assert_eq!(text, String::new());
                assert_eq!(to_literal(&text, false), "NULL");
            }
            _ => panic!("Set NULL must stage through the edit path"),
        }
    }

    #[test]
    fn cell_editor_default_action_stages_the_default_keyword() {
        let action = default_stage(1, 2);
        match action {
            EditAction::Stage { row, col, text } => {
                assert_eq!((row, col), (1, 2));
                assert_eq!(text, STAGED_DEFAULT);
                assert_eq!(to_literal(&text, false), "DEFAULT");
            }
            _ => panic!("Set DEFAULT must stage through the edit path"),
        }
    }

    #[test]
    fn typed_default_stays_quoted_text() {
        assert_eq!(to_literal("DEFAULT", false), "'DEFAULT'");
    }

    #[test]
    fn cell_editor_seed_shows_full_text_past_the_display_cap() {
        let long = "x".repeat(CELL_CAP + 50);
        let value = Value::Text(long.clone());
        assert!(value.display(CELL_CAP).into_owned().len() < long.len());
        let (seed, is_blob) = editor_seed(None, Some(&value));
        assert_eq!(seed, long);
        assert!(!is_blob);
        assert_eq!(to_literal(&seed, false), format!("'{long}'"));
    }

    #[test]
    fn cell_editor_seed_renders_blobs_as_hex_and_null_as_empty() {
        let blob = Value::Blob(vec![0xDE, 0xAD, 0x00, 0xFF]);
        assert_eq!(editor_seed(None, Some(&blob)), ("dead00ff".to_owned(), true));
        assert_eq!(editor_seed(None, Some(&Value::Null)), (String::new(), false));
        assert_eq!(editor_seed(None, None), (String::new(), false));
        assert_eq!(
            editor_seed(None, Some(&Value::Integer(7))),
            ("7".to_owned(), false)
        );
    }

    #[test]
    fn cell_editor_seed_prefers_the_staged_text() {
        let value = Value::Text("orig".to_owned());
        assert_eq!(
            editor_seed(Some("edited"), Some(&value)),
            ("edited".to_owned(), false)
        );
        assert_eq!(editor_seed(Some(""), Some(&value)), (String::new(), false));
        assert_eq!(
            editor_seed(Some(STAGED_DEFAULT), Some(&value)),
            (String::new(), false)
        );
    }

    #[test]
    fn cell_editor_size_guard_caps_staging_at_one_million_bytes() {
        assert!(editor_size_ok(""));
        assert!(editor_size_ok(&"a".repeat(CELL_EDITOR_LIMIT)));
        assert!(!editor_size_ok(&"a".repeat(CELL_EDITOR_LIMIT + 1)));
    }

    #[test]
    fn row_detail_json_renders_the_focused_row_like_export() {
        let result = two_col(
            vec![Value::Integer(1), Value::Null],
            vec![Value::Text("a\"b".to_owned()), Value::Null],
        );
        assert_eq!(
            detail_json(&result, 0),
            Some("[\n  {\n    \"a\": 1,\n    \"b\": \"a\\\"b\"\n  }\n]".to_owned())
        );
        assert_eq!(
            detail_json(&result, 1),
            Some("[\n  {\n    \"a\": null,\n    \"b\": null\n  }\n]".to_owned())
        );
        assert_eq!(detail_json(&result, 5), None);
    }

    #[test]
    fn row_detail_json_renders_blobs_as_size_tags() {
        let result = one_col(vec![Value::Blob(vec![0x01, 0x02])]);
        assert_eq!(
            detail_json(&result, 0),
            Some("[\n  {\n    \"v\": \"[BLOB 2 B]\"\n  }\n]".to_owned())
        );
    }

    #[test]
    fn row_detail_record_transposes_names_types_and_values() {
        let result = two_col(vec![Value::Integer(7)], vec![Value::Null]);
        assert_eq!(
            detail_record(&result, 0),
            vec![
                ("a".to_owned(), "ANY".to_owned(), "7".to_owned()),
                ("b".to_owned(), "TEXT".to_owned(), "NULL".to_owned()),
            ]
        );
        assert!(detail_record(&result, 3).is_empty());
    }

    #[test]
    fn row_detail_record_renders_blobs_as_size_tags() {
        let result = one_col(vec![Value::Blob(vec![0xDE, 0xAD])]);
        assert_eq!(
            detail_record(&result, 0),
            vec![("v".to_owned(), "ANY".to_owned(), "[BLOB 2 B]".to_owned())]
        );
    }

    #[test]
    fn row_detail_shows_large_text_full_past_the_display_cap() {
        let long = "x".repeat(CELL_CAP + 50);
        let result = one_col(vec![Value::Text(long.clone())]);
        assert!(result.cell(0, 0).unwrap().display(CELL_CAP).into_owned().len() < long.len());
        assert_eq!(detail_cell_text(result.cell(0, 0)), long);
        assert_eq!(
            detail_record(&result, 0),
            vec![(
                "v".to_owned(),
                "ANY".to_owned(),
                format!("{}…", "x".repeat(CELL_CAP))
            )]
        );
        assert_eq!(
            detail_json(&result, 0),
            Some(format!("[\n  {{\n    \"v\": \"{long}\"\n  }}\n]"))
        );
    }

    #[test]
    fn row_detail_cell_text_spells_null_and_hex() {
        assert_eq!(detail_cell_text(None), "NULL");
        assert_eq!(detail_cell_text(Some(&Value::Null)), "NULL");
        assert_eq!(
            detail_cell_text(Some(&Value::Blob(vec![0xDE, 0xAD, 0x00, 0xFF]))),
            "dead00ff"
        );
        assert_eq!(detail_cell_text(Some(&Value::Integer(7))), "7");
    }

    #[test]
    fn selected_cell_text_switches_to_the_selection_color() {
        for palette in [Palette::dark(), Palette::light()] {
            assert_eq!(cell_label_color(true, &palette), Some(palette.selection_text));
            assert_eq!(cell_label_color(false, &palette), None);
        }
    }

    #[test]
    fn a_step_with_no_cursor_lands_on_the_first_displayed_row() {
        let visible = [3, 8];
        for step in [Step::Up, Step::Down, Step::PageDown, Step::Last, Step::Right] {
            let mut selection = Selection::default();
            selection.step(step, false, &visible, 2, 4);
            assert_eq!(selection.active(), Some((3, 0)), "{step:?} starts at the top left");
            assert_eq!(selection.resolve(&visible), vec![3]);
        }
        let mut selection = Selection::default();
        selection.click(7, 1);
        selection.step(Step::Right, false, &visible, 2, 4);
        assert_eq!(selection.active(), Some((3, 0)));
    }

    #[test]
    fn steps_move_through_the_view_order_not_the_row_indices() {
        let visible = [5, 1, 9];
        let mut selection = Selection::default();
        selection.step(Step::Down, false, &visible, 2, 1);
        assert_eq!(selection.active(), Some((5, 0)));
        selection.step(Step::Down, false, &visible, 2, 1);
        assert_eq!(selection.active(), Some((1, 0)));
        selection.step(Step::Up, false, &visible, 2, 1);
        assert_eq!(selection.active(), Some((5, 0)));
        assert_eq!(selection.resolve(&visible), vec![5]);
    }

    #[test]
    fn steps_clamp_at_the_view_edges() {
        let visible = [3, 8];
        let mut selection = Selection::default();
        selection.click(3, 0);
        selection.step(Step::Up, false, &visible, 2, 5);
        selection.step(Step::Left, false, &visible, 2, 5);
        assert_eq!(selection.active(), Some((3, 0)));
        selection.step(Step::Down, false, &visible, 2, 5);
        selection.step(Step::Down, false, &visible, 2, 5);
        selection.step(Step::Right, false, &visible, 2, 5);
        selection.step(Step::Right, false, &visible, 2, 5);
        assert_eq!(selection.active(), Some((8, 1)));
        selection.step(Step::RowStart, false, &visible, 2, 5);
        assert_eq!(selection.active(), Some((8, 0)));
        selection.step(Step::RowEnd, false, &visible, 2, 5);
        assert_eq!(selection.active(), Some((8, 1)));
        selection.step(Step::First, false, &visible, 2, 5);
        assert_eq!(selection.active(), Some((3, 1)));
        selection.step(Step::Last, false, &visible, 2, 5);
        assert_eq!(selection.active(), Some((8, 1)));
        selection.step(Step::Right, false, &visible, 2, 5);
        assert_eq!(selection.active(), Some((8, 1)), "the last column holds");
    }

    #[test]
    fn a_step_that_cannot_move_leaves_no_cursor() {
        let mut selection = Selection::default();
        selection.step(Step::Down, false, &[], 3, 2);
        assert_eq!(selection.active(), None);
        selection.step(Step::Right, false, &[0, 1], 0, 2);
        assert_eq!(selection.active(), None);
        selection.tab_step(false, &[], 3, 2);
        assert_eq!(selection.active(), None);
        selection.tab_step(false, &[0], 0, 2);
        assert_eq!(selection.active(), None);
    }

    #[test]
    fn a_cursor_column_past_the_end_is_pulled_back_in() {
        let visible = [0, 1];
        let mut selection = Selection::default();
        selection.click(0, 5);
        selection.step(Step::RowEnd, false, &visible, 2, 1);
        assert_eq!(selection.active(), Some((0, 1)));
        selection.click(0, 5);
        selection.step(Step::Left, false, &visible, 2, 1);
        assert_eq!(selection.active(), Some((0, 0)), "the stale column clamps, then steps");
    }

    #[test]
    fn a_shift_step_extends_the_row_range_from_the_anchor() {
        let visible: Vec<usize> = (0..6).collect();
        let mut selection = Selection::default();
        selection.step(Step::Down, false, &visible, 2, 2);
        selection.step(Step::Down, false, &visible, 2, 2);
        selection.step(Step::Down, false, &visible, 2, 2);
        assert_eq!(selection.active(), Some((2, 0)));
        selection.step(Step::Down, true, &visible, 2, 2);
        assert_eq!(selection.active(), Some((3, 0)));
        assert_eq!(selection.resolve(&visible), vec![2, 3]);
        selection.step(Step::Down, true, &visible, 2, 2);
        assert_eq!(selection.resolve(&visible), vec![2, 3, 4]);
        assert_eq!(selection.active(), Some((4, 0)));
        selection.step(Step::Up, true, &visible, 2, 2);
        assert_eq!(selection.resolve(&visible), vec![2, 3]);
        assert_eq!(selection.active(), Some((3, 0)));
        selection.step(Step::Right, true, &visible, 2, 2);
        assert_eq!(selection.active(), Some((3, 1)));
        assert_eq!(selection.resolve(&visible), vec![2, 3]);
        // A plain step collapses the range onto the row it lands on.
        selection.step(Step::Left, false, &visible, 2, 2);
        assert_eq!(selection.active(), Some((3, 0)));
        assert_eq!(selection.resolve(&visible), vec![3]);
    }

    #[test]
    fn page_steps_travel_the_viewport_worth_of_rows() {
        let visible: Vec<usize> = (0..10).collect();
        let mut selection = Selection::default();
        selection.step(Step::PageDown, false, &visible, 1, 3);
        assert_eq!(selection.active(), Some((0, 0)));
        selection.step(Step::PageDown, false, &visible, 1, 3);
        assert_eq!(selection.active(), Some((3, 0)));
        selection.step(Step::PageDown, false, &visible, 1, 3);
        assert_eq!(selection.active(), Some((6, 0)));
        selection.step(Step::PageDown, false, &visible, 1, 3);
        assert_eq!(selection.active(), Some((9, 0)));
        selection.step(Step::PageUp, false, &visible, 1, 3);
        assert_eq!(selection.active(), Some((6, 0)));
    }

    #[test]
    fn tab_wraps_onto_the_next_or_previous_row() {
        let visible: Vec<usize> = (0..3).collect();
        let mut selection = Selection::default();
        selection.tab_step(false, &visible, 3, 2);
        assert_eq!(selection.active(), Some((0, 0)));
        selection.tab_step(false, &visible, 3, 2);
        selection.tab_step(false, &visible, 3, 2);
        assert_eq!(selection.active(), Some((0, 2)));
        selection.tab_step(false, &visible, 3, 2);
        assert_eq!(selection.active(), Some((1, 0)));
        selection.tab_step(true, &visible, 3, 2);
        assert_eq!(selection.active(), Some((0, 2)));
        selection.click(2, 2);
        selection.tab_step(false, &visible, 3, 2);
        assert_eq!(selection.active(), Some((2, 0)));
        selection.click(0, 0);
        selection.tab_step(true, &visible, 3, 2);
        assert_eq!(selection.active(), Some((0, 2)));
    }

    #[test]
    fn tab_from_no_cursor_lands_like_any_step() {
        let mut selection = Selection::default();
        selection.tab_step(true, &[4, 9], 2, 2);
        assert_eq!(selection.active(), Some((4, 0)));
    }

    #[test]
    fn page_rows_measures_the_viewport_and_falls_back() {
        assert_eq!(page_rows(400.0, 0.0), 19, "the header takes one row");
        assert_eq!(page_rows(400.0, 4.0), 15, "row spacing and the header are excluded");
        assert_eq!(page_rows(19.0, 4.0), 1);
        assert_eq!(page_rows(0.0, 4.0), GRID_PAGE_FALLBACK);
        assert_eq!(page_rows(-20.0, 4.0), GRID_PAGE_FALLBACK);
        assert_eq!(page_rows(f32::INFINITY, 4.0), GRID_PAGE_FALLBACK);
        assert_eq!(page_rows(f32::NAN, 4.0), GRID_PAGE_FALLBACK);
    }
}
