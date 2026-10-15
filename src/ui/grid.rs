//! Results grid over the active tab's [`RunOutcome`].
//!
//! Only visible rows render, via `TableBody::rows` virtualization.

use std::cmp::Ordering;

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::app::{AppState, EditTarget, RunOutcome, SortDir, Tab};
use crate::db::{CELL_CAP, QueryResult, TabId, Value};
use crate::export::{to_csv, to_inserts, to_json, to_markdown};

const AFFECT_GREEN: egui::Color32 = egui::Color32::from_rgb(115, 180, 115);
/// Translucent amber over a cell with a staged edit.
const EDIT_TINT: egui::Color32 = egui::Color32::from_rgba_unmultiplied_const(227, 179, 65, 64);

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

pub fn results(state: &mut AppState, ui: &mut egui::Ui) {
    import_window(state, ui);
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
    let actions = results_grid(tab, target.as_ref(), ui);
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
        }
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
    ui: &mut egui::Ui,
) -> Vec<EditAction> {
    let mut actions = Vec::new();
    result_tabs(tab, ui);
    let (cols, total) = {
        let Some(result) = tab.active_query_result() else { return actions };
        if result.columns().is_empty() {
            match result.affected() {
                Some(n) => {
                    ui.colored_label(
                        AFFECT_GREEN,
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
            .add_enabled(can_export, egui::Button::new("Copy INSERTs"))
            .clicked()
        {
            copy_inserts_view(tab, ui.ctx());
        }
        if ui
            .add_enabled(can_export, egui::Button::new("Copy Markdown"))
            .clicked()
        {
            copy_markdown_view(tab, ui.ctx());
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
            ui.colored_label(AFFECT_GREEN, notice);
        }
    }
    let (sort_col, sort_dir, filter) = (tab.sort_col(), tab.sort_dir(), tab.filter().to_owned());
    let visible = match tab.active_query_result() {
        Some(result) => visible_rows(result, sort_col, sort_dir, &filter),
        None => return actions,
    };
    ui.label(
        egui::RichText::new(format!("{} of {} rows", visible.len(), total)).small().weak(),
    );
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
                            if staged.is_some() && !deleted {
                                ui.painter().rect_filled(
                                    cell_rect,
                                    egui::CornerRadius::ZERO,
                                    EDIT_TINT,
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

fn copy_inserts_view(tab: &mut Tab, ctx: &egui::Context) {
    let (text, nrows) = match tab.active_query_result() {
        Some(result) => {
            if result.columns().is_empty() {
                return;
            }
            let visible = visible_rows(result, tab.sort_col(), tab.sort_dir(), tab.filter());
            let text = to_inserts("result", result, &visible);
            (text, visible.len())
        }
        None => return,
    };
    ctx.copy_text(text);
    tab.set_export_notice(format!("Copied {nrows} rows as INSERTs"), false);
}

fn copy_markdown_view(tab: &mut Tab, ctx: &egui::Context) {
    let (text, nrows) = match tab.active_query_result() {
        Some(result) => {
            if result.columns().is_empty() {
                return;
            }
            let visible = visible_rows(result, tab.sort_col(), tab.sort_dir(), tab.filter());
            let text = to_markdown(result, &visible);
            (text, visible.len())
        }
        None => return,
    };
    ctx.copy_text(text);
    tab.set_export_notice(format!("Copied {nrows} rows as Markdown"), false);
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
}
