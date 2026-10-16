//! Results grid over the active tab's [`RunOutcome`].
//!
//! Only visible rows render, via `TableBody::rows` virtualization.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::app::{AppState, EditTarget, RunOutcome, SortDir, Tab};
use crate::db::{CELL_CAP, QueryResult, TabId, Value};
use crate::export::{to_csv, to_in_list, to_inserts, to_json, to_jsonl, to_markdown, to_tsv};
use crate::import::{ParsedCsv, parse_csv, parse_jsonl};

use super::Palette;

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
    fn extend(&mut self, row: usize, col: usize) {
        let origin =
            self.anchor.or_else(|| self.active.map(|(row, _)| row)).unwrap_or(row);
        self.rows.clear();
        self.rows.extend(origin.min(row)..=origin.max(row));
        self.active = Some((row, col));
    }
    /// One cell gesture: ctrl toggles, shift extends, else selects.
    fn gesture(&mut self, row: usize, col: usize, shift: bool, ctrl: bool) {
        if ctrl {
            self.toggle(row, col);
        } else if shift {
            self.extend(row, col);
        } else {
            self.click(row, col);
        }
    }
    fn is_selected(&self, row: usize) -> bool {
        self.rows.contains(&row)
    }
    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    fn len(&self) -> usize {
        self.rows.len()
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
    ctx.data_mut(|data| data.remove::<Selection>(selection_id(tab)));
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

pub fn results(state: &mut AppState, ui: &mut egui::Ui) {
    import_window(state, ui);
    prune_selection(state, ui.ctx());
    let modal_open = state.draft().is_some()
        || state.save_prompt().is_some()
        || state.password_prompt().is_some()
        || state.import_draft().is_some()
        || state.pending_write().is_some();
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
            ui.label("Type SQL above and press Run (Ctrl+Enter).");
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
    let Some(tab) = state.tab_mut(tab_id) else { return };
    let actions = results_grid(tab, target.as_ref(), modal_open, ui);
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
    PasteRows(Vec<Vec<String>>),
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
            EditAction::PasteRows(staged) => stage_rows_into(state, tab, staged, "Paste", "Pasted"),
        }
    }
}

/// Record one import notice on a tab that still exists.
fn tab_notice(state: &mut AppState, tab: TabId, notice: String, failed: bool) {
    if let Some(current) = state.tab_mut(tab) {
        current.set_export_notice(notice, failed);
    }
}

/// Ask for a CSV file and stage it on the tab. A dismissed dialog changes
/// nothing.
fn pick_import_file(state: &mut AppState, tab: TabId) {
    let Some(path) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).pick_file() else {
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
pub fn import_window(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(draft) = state.import_draft() else { return };
    let title = format!("Import into {}", draft.table);
    let mut confirm = false;
    let mut cancel = false;
    egui::Window::new(title).show(ui.ctx(), |ui| {
        let Some(draft) = state.import_draft() else { return };
        ui.label(format!("{} rows", draft.rows.len()));
        egui::Grid::new("import_columns").striped(true).show(ui, |ui| {
            ui.label(egui::RichText::new("CSV column").small().weak());
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
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    });
    if confirm {
        state.commit_import();
    } else if cancel {
        state.cancel_import();
    }
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

fn results_grid(
    tab: &mut Tab,
    target: Option<&EditTarget>,
    modal_open: bool,
    ui: &mut egui::Ui,
) -> Vec<EditAction> {
    let mut actions = Vec::new();
    result_tabs(tab, ui);
    let palette = Palette::of(ui.ctx().theme());
    let (cols, total) = {
        let Some(result) = tab.active_query_result() else { return actions };
        if result.columns().is_empty() {
            match result.affected() {
                Some(n) => {
                    ui.colored_label(
                        palette.success,
                        egui::RichText::new(format!("✓ {n} rows affected")).strong(),
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
        ui.horizontal(|ui| {
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::symmetric(8, 4))
                .corner_radius(egui::CornerRadius::same(4))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(format!("{} rows · {:.0} ms", total, ms))
                            .small()
                            .strong(),
                    );
                });
            if result.is_truncated() {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    egui::RichText::new("truncated at 10,000").small().strong(),
                );
            }
        });
        if total == 0 && tab.added_rows().is_empty() {
            ui.label(egui::RichText::new("No rows returned.").weak());
            return actions;
        }
        (cols, total)
    };
    let editable = target.is_some();
    if !editable {
        // A run or a target change withdraws editing; drop any open field.
        store_edit_field(ui.ctx(), tab.id, None);
    }
    ui.horizontal(|ui| {
        ui.label("Filter");
        let mut text = tab.filter().to_owned();
        if ui.text_edit_singleline(&mut text).changed() {
            tab.set_filter(text);
        }
        if ui.button("Clear").clicked() {
            tab.set_filter(String::new());
        }
        let can_export = tab
            .active_query_result()
            .is_some_and(|result| !result.columns().is_empty());
        let can_export_all =
            tab.results().iter().any(|result| !result.columns().is_empty());
        if ui
            .add_enabled(can_export, egui::Button::new("Export CSV"))
            .clicked()
        {
            export_current_view(tab);
        }
        if ui
            .add_enabled(can_export, egui::Button::new("Export JSON"))
            .clicked()
        {
            export_json_view(tab);
        }
        if ui
            .add_enabled(can_export, egui::Button::new("Export JSONL"))
            .clicked()
        {
            export_jsonl_view(tab);
        }
        if ui
            .add_enabled(can_export_all, egui::Button::new("Export all"))
            .on_hover_text("Export every result set into one folder, one CSV per set")
            .clicked()
        {
            export_all_results(tab);
        }
        ui.separator();
        match target {
            Some(target) => {
                let staged = tab.staged_count();
                if ui
                    .add_enabled(staged > 0, egui::Button::new(format!("Apply ({staged})")))
                    .on_hover_text("Send the staged changes")
                    .clicked()
                {
                    actions.push(EditAction::Apply);
                }
                if ui
                    .button("Import CSV")
                    .on_hover_text("Load a CSV file into this table")
                    .clicked()
                {
                    actions.push(EditAction::ImportCsv);
                }
                if ui
                    .button("Import JSONL")
                    .on_hover_text("Load a JSONL file as new rows for review")
                    .clicked()
                {
                    actions.push(EditAction::ImportJsonl);
                }
                if ui
                    .add_enabled(staged > 0, egui::Button::new("Discard"))
                    .on_hover_text("Drop every staged change")
                    .clicked()
                {
                    actions.push(EditAction::Discard);
                }
                if ui
                    .button("Add row")
                    .on_hover_text("Stage one empty row for INSERT")
                    .clicked()
                {
                    actions.push(EditAction::AddRow);
                }
                ui.label(
                    egui::RichText::new(format!("editing {}", target.table)).small().weak(),
                );
            }
            None => {
                ui.label(egui::RichText::new("read only").small().weak()).on_hover_text(
                    "Inline editing needs an idle connection and a single-table SELECT \
                     whose table has a primary key in the result.",
                );
            }
        }
    });
    if let Some(notice) = tab.export_notice().map(str::to_owned) {
        if tab.export_failed() {
            ui.colored_label(ui.visuals().error_fg_color, notice);
        } else {
            ui.colored_label(palette.success, notice);
        }
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
    ui.label(
        egui::RichText::new(format!("{} of {} rows", visible.len(), total)).small().weak(),
    );
    copy_bar(tab, editable, &selection, &visible, ui);
    let ncols = cols.len();
    let active_result = tab.active_result();
    let added = tab.added_rows().len();
    let open_field = edit_field_of(ui.ctx(), tab.id);
    let field_cell = open_field
        .as_ref()
        .filter(|field| field.result == active_result)
        .map(|field| (field.row, field.col));
    let mut pending_sort: Option<usize> = None;
    egui::ScrollArea::both().show(ui, |ui| {
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .columns(Column::auto(), ncols)
            .header(20.0, |mut header| {
                for (i, (name, type_name)) in cols.iter().enumerate() {
                    let active = sort_col == Some(i);
                    let marker = if active {
                        match sort_dir {
                            SortDir::Asc => " ▲",
                            SortDir::Desc => " ▼",
                        }
                    } else {
                        ""
                    };
                    header.col(|ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .selectable_label(
                                    active,
                                    egui::RichText::new(format!("{name}{marker}")).strong(),
                                )
                                .on_hover_text("Click to sort")
                                .clicked()
                            {
                                pending_sort = Some(i);
                            }
                            ui.label(egui::RichText::new(type_name).small().weak());
                            ui.label(
                                egui::RichText::new(format!("#{}", i + 1)).small().weak(),
                            );
                        });
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, visible.len() + added, |mut row| {
                    let index = row.index();
                    if let Some(added_index) = index.checked_sub(visible.len()) {
                        for c in 0..ncols {
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
                                Some(text) => text.to_owned(),
                                None => tab
                                    .active_query_result()
                                    .and_then(|result| result.cell(r, c))
                                    .map(|v| v.display(CELL_CAP).into_owned())
                                    .unwrap_or_default(),
                            };
                            let cell_rect = ui.max_rect();
                            if selection.is_selected(r) {
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
                            let label = if deleted {
                                ui.monospace(
                                    egui::RichText::new(text).strikethrough().weak(),
                                )
                            } else {
                                ui.monospace(text)
                            };
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
                            if !deleted && editable && response.double_clicked() {
                                let cell = tab
                                    .active_query_result()
                                    .and_then(|result| result.cell(r, c));
                                let preload = cell
                                    .map(|v| v.display(usize::MAX).into_owned())
                                    .unwrap_or_default();
                                if !matches!(cell, Some(Value::Blob(_))) {
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
                                    if ui.button(label).clicked() {
                                        actions.push(EditAction::ToggleDelete(r));
                                        ui.close();
                                    }
                                });
                            }
                        });
                    }
                });
            });
    });
    for (row, col, shift, ctrl) in gestures {
        selection.gesture(row, col, shift, ctrl);
    }
    let mut keyed = grid_clipboard_keys(tab, editable, &selection, &visible, modal_open, ui);
    actions.append(&mut keyed);
    store_selection(ui.ctx(), tab.id, &selection);
    if let Some(col) = pending_sort {
        tab.set_sort(col);
    }
    actions
}

fn export_current_view(tab: &mut Tab) {
    let (csv, default_name, nrows) = match tab.active_query_result() {
        Some(result) => {
            if result.columns().is_empty() {
                return;
            }
            let visible = visible_rows(result, tab.sort_col(), tab.sort_dir(), tab.filter());
            let csv = to_csv(result, &visible);
            let name = format!("{}.csv", super::query::tab_title(tab.sql.clone()));
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
            let name = format!("{}.json", super::query::tab_title(tab.sql.clone()));
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
            let name = format!("{}.jsonl", super::query::tab_title(tab.sql.clone()));
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

/// The selection copy row: TSV with a headers option, CSV, JSON, Markdown,
/// and SQL INSERTs from the selection (or the whole view when empty), the
/// IN-list builder from the active column, and paste as new rows.
fn copy_bar(
    tab: &mut Tab,
    editable: bool,
    selection: &Selection,
    visible: &[usize],
    ui: &mut egui::Ui,
) {
    ui.horizontal(|ui| {
        if tab.active_query_result().is_none_or(|result| result.columns().is_empty()) {
            return;
        }
        let tab_id = tab.id;
        let rows = selection.resolve(visible);
        let nrows = rows.len();
        let headers = tsv_headers_of(ui.ctx(), tab_id);
        if ui
            .button("Copy TSV")
            .on_hover_text("Copy the selection as tab-separated values")
            .clicked()
        {
            let text = tab
                .active_query_result()
                .map(|result| to_tsv(result, &rows, headers))
                .unwrap_or_default();
            ui.ctx().copy_text(text);
            tab.set_export_notice(format!("Copied {nrows} rows as TSV"), false);
        }
        let mut headers = headers;
        if ui
            .checkbox(&mut headers, "Headers")
            .on_hover_text("Include the header row in TSV copies")
            .changed()
        {
            ui.ctx().data_mut(|data| data.insert_temp(tsv_headers_id(tab_id), headers));
        }
        if ui.button("Copy CSV").on_hover_text("Copy the selection as CSV").clicked() {
            let text = tab
                .active_query_result()
                .map(|result| to_csv(result, &rows))
                .unwrap_or_default();
            ui.ctx().copy_text(text);
            tab.set_export_notice(format!("Copied {nrows} rows as CSV"), false);
        }
        if ui.button("Copy JSON").on_hover_text("Copy the selection as JSON").clicked() {
            let text = tab
                .active_query_result()
                .map(|result| to_json(result, &rows))
                .unwrap_or_default();
            ui.ctx().copy_text(text);
            tab.set_export_notice(format!("Copied {nrows} rows as JSON"), false);
        }
        if ui
            .button("Copy Markdown")
            .on_hover_text("Copy the selection as a Markdown table")
            .clicked()
        {
            let text = tab
                .active_query_result()
                .map(|result| to_markdown(result, &rows))
                .unwrap_or_default();
            ui.ctx().copy_text(text);
            tab.set_export_notice(format!("Copied {nrows} rows as Markdown"), false);
        }
        if ui
            .button("Copy INSERTs")
            .on_hover_text("Copy the selection as SQL INSERTs")
            .clicked()
        {
            let text = tab
                .active_query_result()
                .map(|result| to_inserts("result", result, &rows))
                .unwrap_or_default();
            ui.ctx().copy_text(text);
            tab.set_export_notice(format!("Copied {nrows} rows as INSERTs"), false);
        }
        if ui
            .button("IN (...)")
            .on_hover_text("Copy a typed IN list from the active column")
            .clicked()
        {
            let pick = selection
                .active()
                .and_then(|(_, col)| column_names(tab).get(col).cloned().map(|name| (col, name)));
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
                                format!("Copied {name} IN list ({nrows} rows)"),
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
        }
        if editable
            && ui
                .button("Paste rows")
                .on_hover_text("Paste TSV or CSV from the clipboard as new rows for review")
                .clicked()
        {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            ui.ctx().data_mut(|data| data.insert_temp(paste_armed_id(tab_id), true));
        }
        if selection.is_empty() {
            ui.label(egui::RichText::new("whole view").small().weak());
        } else if let Some((_, col)) = selection.active() {
            let name = column_names(tab).get(col).cloned().unwrap_or_default();
            ui.label(
                egui::RichText::new(format!("{} selected · {}", selection.len(), name))
                    .small()
                    .weak(),
            );
        } else {
            ui.label(
                egui::RichText::new(format!("{} selected", selection.len())).small().weak(),
            );
        }
    });
}

pub fn copy_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::C)
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
    if !modal_open
        && !typing
        && ui.input_mut(|i| i.consume_shortcut(&copy_shortcut()))
    {
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
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Column;

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
        selection.gesture(1, 0, false, false);
        assert_eq!(selection.resolve(&[0, 1, 2]), vec![1]);
        assert_eq!(selection.active(), Some((1, 0)));
        selection.gesture(2, 1, true, false);
        assert_eq!(selection.resolve(&[0, 1, 2]), vec![1, 2]);
        assert_eq!(selection.active(), Some((2, 1)));
        selection.gesture(0, 0, false, true);
        assert_eq!(selection.resolve(&[2, 0, 1]), vec![2, 0, 1]);
        assert_eq!(selection.active(), Some((0, 0)));
        selection.gesture(1, 0, false, true);
        assert_eq!(selection.resolve(&[0, 1, 2]), vec![0, 2]);
    }

    #[test]
    fn shift_without_an_anchor_selects_one_row() {
        let mut selection = Selection::default();
        selection.gesture(2, 1, true, false);
        assert_eq!(selection.resolve(&[0, 1, 2]), vec![2]);
        assert_eq!(selection.active(), Some((2, 1)));
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
        selection.gesture(0, 0, false, false);
        selection.sync(first);
        assert_eq!(selection.resolve(&[0, 1]), vec![0]);
        selection.sync(rerun);
        assert!(selection.is_empty());
        assert_eq!(selection.active(), None);
        assert_eq!(selection.resolve(&[0, 1]), vec![0, 1]);
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
    fn copy_shortcut_is_ctrl_c() {
        let shortcut = copy_shortcut();
        assert_eq!(shortcut.modifiers, egui::Modifiers::CTRL);
        assert_eq!(shortcut.logical_key, egui::Key::C);
    }
}
