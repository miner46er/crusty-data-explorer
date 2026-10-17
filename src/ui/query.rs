//! Tab bar, SQL editor, toolbar, and the find bar.
//!
//! Clicks are collected while a tab borrow is live and applied after it
//! ends, so the editor borrow never overlaps a transition.

use std::collections::HashMap;

use eframe::egui;

use super::editor::{char_range, format_sql, FindState};
use crate::app::{AppState, RunOutcome, RunScope, TransactionAction};
use crate::db::{DbKind, TabId};

use super::Palette;

pub fn editor(state: &mut AppState, ui: &mut egui::Ui) {
    prune_find(state, ui.ctx());
    let modal_open = state.draft().is_some()
        || state.save_prompt().is_some()
        || state.password_prompt().is_some()
        || state.import_draft().is_some()
        || state.pending_write().is_some()
        || state.pending_params().is_some()
        || state.pending_edit_preview().is_some()
        || state.table_editor().is_some()
        || state.pending_table_ddl().is_some()
        || state.palette_open();
    let (new_tab_shortcut_pressed, close_tab_shortcut_pressed, find_shortcut_pressed) = ui
        .input_mut(|i| {
            (
                i.consume_shortcut(&new_tab_shortcut()),
                i.consume_shortcut(&close_tab_shortcut()),
                i.consume_shortcut(&find_shortcut()),
            )
        });
    if !modal_open {
        if new_tab_shortcut_pressed {
            if let Some(conn) = state.default_connection() {
                let id = state.new_tab(conn);
                state.activate(id);
            }
        }
        if close_tab_shortcut_pressed {
            if let Some(active) = state.active() {
                state.close_tab(active);
            }
        }
    }
    tab_bar(state, ui);
    ui.add_space(4.0);
    let mut find_opened = false;
    if !modal_open && find_shortcut_pressed {
        if let Some(id) = state.active() {
            find_opened = toggle_find(ui.ctx(), id);
        }
    }
    find_opened |= toolbar(state, ui);
    ui.add_space(4.0);
    save_window(state, ui);
    write_confirm_window(state, ui);
    edit_preview_window(state, ui);
    params_window(state, ui);
    if let Some(id) = state.active() {
        let hint = state
            .tab(id)
            .and_then(|tab| state.connections().iter().find(|c| Some(c.id) == tab.conn))
            .map(|c| format!("{} · {}", c.name, c.config.kind().label()))
            .unwrap_or_else(|| "no connection".to_owned());
        ui.label(egui::RichText::new(hint).small().weak());
        let schema = state
            .tab(id)
            .and_then(|tab| tab.conn)
            .and_then(|conn| state.schema(conn))
            .cloned();
        find_bar(state, ui, find_opened);
        if let Some(sql) = state.tab_sql_mut(id) {
            egui::Frame::canvas(ui.style())
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    super::complete::editor_with_completion(id, schema.as_ref(), sql, ui);
                });
        }
        // The completion popup gets first claim on Esc; close find only if free.
        if !modal_open
            && find_bar_of(ui.ctx(), id).open
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            close_find(ui.ctx(), id);
        }
    } else if state.connections().is_empty() {
        let palette = Palette::of(ui.ctx().theme());
        if ui
            .add(
                egui::Button::new(egui::RichText::new("+ New connection").strong())
                    .fill(palette.accent),
            )
            .clicked()
        {
            state.open_draft(None);
        }
    } else {
        ui.label("Open a connection and start a new query.");
    }
}

fn tab_bar(state: &mut AppState, ui: &mut egui::Ui) {
    let mut active = state.active();
    let mut activate: Option<TabId> = None;
    let mut close: Vec<TabId> = Vec::new();
    let mut new_tab = false;
    ui.horizontal(|ui| {
        let titles = numbered_titles(
            &state.tabs().iter().map(|tab| tab_title(tab.sql.clone())).collect::<Vec<_>>(),
        );
        for (tab, title) in state.tabs().iter().zip(titles) {
            let is_active = Some(tab.id) == active;
            let label = if is_active {
                egui::RichText::new(title).strong()
            } else {
                egui::RichText::new(title).weak()
            };
            if ui.selectable_value(&mut active, Some(tab.id), label).clicked() {
                activate = Some(tab.id);
            }
            let transaction_open = state
                .connections()
                .iter()
                .find(|c| Some(c.id) == tab.conn)
                .is_some_and(|c| c.transaction().is_open());
            if transaction_open {
                ui.label(egui::RichText::new("tx").small().strong())
                    .on_hover_text("Open transaction: commit or roll back to close it");
            }
            if ui.small_button("×").clicked() {
                close.push(tab.id);
            }
        }
        if ui
            .small_button("+")
            .on_hover_text("New tab (Ctrl+T)")
            .clicked()
        {
            new_tab = true;
        }
    });
    if active != state.active() {
        if let Some(id) = active {
            state.activate(id);
        }
    }
    if let Some(id) = activate {
        state.activate(id);
    }
    for id in close {
        state.close_tab(id);
    }
    if new_tab {
        if let Some(conn) = state.default_connection() {
            let id = state.new_tab(conn);
            state.activate(id);
        }
    }
}

fn toolbar(state: &mut AppState, ui: &mut egui::Ui) -> bool {
    let Some(tab_id) = state.active() else { return false };
    let find_open = find_bar_of(ui.ctx(), tab_id).open;
    let (mut run_clicked, mut explain_clicked, mut cancel_clicked, mut save_clicked) =
        (false, false, false, false);
    let (mut format_clicked, mut find_clicked) = (false, false);
    let mut transaction_clicked: Option<TransactionAction> = None;
    let mut autocommit_clicked: Option<bool> = None;
    let mut scope = state.tab(tab_id).map(|t| t.run_scope()).unwrap_or_default();
    if state.tab(tab_id).is_some() {
        let running = matches!(
            state.tab(tab_id).map(|t| t.outcome()),
            Some(RunOutcome::Running { .. })
        );
        let transaction_ready = state.transaction_ready(tab_id);
        let (autocommit, sqlite) = state
            .tab(tab_id)
            .and_then(|t| t.conn)
            .and_then(|id| state.connections().iter().find(|c| c.id == id))
            .map(|c| (c.autocommit(), c.config.kind() == DbKind::Sqlite))
            .unwrap_or((true, false));
        ui.horizontal(|ui| {
            if running {
                cancel_clicked = ui.button("Cancel").clicked();
            } else {
                egui::ComboBox::from_id_salt(("crusty_run_scope", tab_id))
                    .selected_text(scope.label())
                    .show_ui(ui, |ui| {
                        for option in RunScope::ALL {
                            ui.selectable_value(&mut scope, option, option.label());
                        }
                    });
                run_clicked = ui
                    .add(
                        egui::Button::new(egui::RichText::new("▶ Run").strong())
                            .fill(Palette::of(ui.ctx().theme()).success),
                    )
                    .clicked();
                explain_clicked = ui
                    .button("Explain")
                    .on_hover_text("Query execution plan")
                    .clicked();
                ui.label(
                    egui::RichText::new(format!("{} · Ctrl+Enter", scope.hint()))
                        .small()
                        .weak(),
                );
                save_clicked = ui.button("Save").clicked();
            }
            for action in TransactionAction::ALL {
                if ui
                    .add_enabled(transaction_ready, egui::Button::new(action.label()))
                    .on_hover_text(format!("Send {}", action.statement()))
                    .clicked()
                {
                    transaction_clicked = Some(action);
                }
            }
            ui.add_space(6.0);
            let mut shown = autocommit;
            let toggle = if transaction_ready && !sqlite {
                ui.add_enabled_ui(true, |ui| {
                    ui.toggle_value(&mut shown, "Autocommit")
                })
                .inner
                .on_hover_text(
                    "Off: every statement runs inside a transaction on one tagged \
                     connection until Commit or Rollback",
                )
            } else {
                autocommit_chip(
                    ui,
                    if sqlite {
                        "SQLite handles autocommit itself"
                    } else {
                        "Connect to change autocommit"
                    },
                )
            };
            if toggle.clicked() {
                autocommit_clicked = Some(!autocommit);
            }
            format_clicked = ui.button("Format").clicked();
            find_clicked = ui
                .selectable_label(find_open, "Find")
                .on_hover_text("Find (Ctrl+F)")
                .clicked();
            match state.tab(tab_id).map(|t| t.outcome()) {
                Some(RunOutcome::Running { started, .. }) => {
                    ui.spinner();
                    ui.label(format!("{:.1}s", started.elapsed().as_secs_f32()));
                }
                Some(RunOutcome::Err(msg)) => {
                    ui.colored_label(ui.visuals().error_fg_color, msg);
                }
                Some(RunOutcome::Ok { .. }) => {
                    let (rows, ms) = state
                        .tab(tab_id)
                        .and_then(|t| t.active_query_result())
                        .map(|r| (r.row_count(), r.took().as_secs_f64() * 1000.0))
                        .unwrap_or((0, 0.0));
                    ui.label(
                        egui::RichText::new(format!("{rows} rows · {ms:.0} ms")).small().weak(),
                    );
                }
                Some(RunOutcome::Never) | None => {}
            }
        });
    }
    if scope != state.tab(tab_id).map(|t| t.run_scope()).unwrap_or_default() {
        if let Some(tab) = state.tab_mut(tab_id) {
            tab.set_run_scope(scope);
        }
    }
    run_clicked |= ui.input_mut(|i| i.consume_shortcut(&run_shortcut()));
    if cancel_clicked {
        state.cancel_run(tab_id);
    }
    if let Some(action) = transaction_clicked {
        state.start_transaction(tab_id, action);
    }
    if let Some(on) = autocommit_clicked {
        state.set_autocommit(tab_id, on);
    }
    if save_clicked {
        state.open_save_prompt();
    }
    if format_clicked {
        if let Some(sql) = state.tab_sql_mut(tab_id) {
            *sql = format_sql(sql);
        }
    }
    let mut find_opened = false;
    if find_clicked {
        find_opened = toggle_find(ui.ctx(), tab_id);
    }
    if run_clicked {
        let sql = state.tab(tab_id).map(|t| t.sql.clone()).unwrap_or_default();
        let (cursor, selection) = super::complete::editor_offsets(ui.ctx(), tab_id, &sql);
        state.start_run(tab_id, &sql, cursor, selection);
    }
    if explain_clicked {
        let sql = state.tab(tab_id).map(|t| t.sql.clone()).unwrap_or_default();
        let (cursor, selection) = super::complete::editor_offsets(ui.ctx(), tab_id, &sql);
        state.start_explain(tab_id, &sql, cursor, selection);
    }
    find_opened
}

/// An Autocommit chip for the states where the toggle cannot act: filled and
/// colored as unavailable, sensed as hover only so it never lights up or
/// moves, with the reason on the tooltip.
fn autocommit_chip(ui: &mut egui::Ui, reason: &str) -> egui::Response {
    let palette = Palette::of(ui.ctx().theme());
    ui.add(
        egui::Button::new(egui::RichText::new("Autocommit").color(palette.disabled_text))
            .fill(palette.disabled_fill)
            .stroke(egui::Stroke::NONE)
            .sense(egui::Sense::hover()),
    )
    .on_hover_text(reason)
}

pub(crate) fn tab_title(sql: String) -> String {
    if let Some(table) = select_table(&sql) {
        return table;
    }
    let first = sql.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("SQL");
    let mut title: String = first.chars().take(24).collect();
    if first.chars().count() > 24 {
        title += "…";
    }
    title
}

/// Tab-strip labels: each title, with " 2", " 3", … appended to the second
/// and later tabs sharing a title, in tab order.
fn numbered_titles(titles: &[String]) -> Vec<String> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    titles
        .iter()
        .map(|title| {
            let count = seen.entry(title.as_str()).or_insert(0);
            *count += 1;
            if *count == 1 { title.clone() } else { format!("{title} {count}") }
        })
        .collect()
}

/// The table named by the SQL when its first statement is a SELECT: the
/// identifier after the FROM keyword. Anything else (no SELECT first, no
/// FROM, a subquery or a join without a first identifier) is None, so the
/// label falls back to the trimmed text.
fn select_table(sql: &str) -> Option<String> {
    let text = sql.trim_start();
    if !text.get(..6)?.eq_ignore_ascii_case("select") {
        return None;
    }
    if text[6..].chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let from = find_keyword(text, "from")?;
    let table = last_identifier(&text[from + 4..])?;
    if table.is_empty() { None } else { Some(table) }
}

/// Byte index of the first `keyword` token that is not inside quotes, a
/// line comment, or a block comment.
fn find_keyword(text: &str, keyword: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            quote @ (b'\'' | b'"' | b'`') => {
                index += 1;
                while index < bytes.len() && bytes[index] != quote {
                    index += 1;
                }
                index += 1;
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len()
                    && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index += 2;
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = index;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                if text[start..index].eq_ignore_ascii_case(keyword) {
                    return Some(start);
                }
            }
            _ => index += 1,
        }
    }
    None
}

/// The last segment of the possibly quoted, possibly dotted identifier at
/// the start of `text`: `users`, `"my table"`, and `` main.`orders` `` all
/// land as their final name.
fn last_identifier(text: &str) -> Option<String> {
    let mut rest = text.trim_start();
    loop {
        let (segment, tail) = read_identifier(rest)?;
        rest = tail.trim_start();
        match rest.strip_prefix('.') {
            Some(after) => rest = after.trim_start(),
            None => return Some(segment),
        }
    }
}

/// One identifier at the start of `text` and the text after it, with the
/// surrounding quotes or backticks removed.
fn read_identifier(text: &str) -> Option<(String, &str)> {
    let first = text.chars().next()?;
    match first {
        '"' | '\'' | '`' => {
            let start = first.len_utf8();
            let end = text[start..].find(first)? + start;
            let segment = text[start..end].to_owned();
            Some((segment, &text[end + first.len_utf8()..]))
        }
        _ if first.is_alphabetic() || first == '_' => {
            let mut end = first.len_utf8();
            for (index, character) in text.char_indices().skip(1) {
                if character.is_alphanumeric() || character == '_' {
                    end = index + character.len_utf8();
                } else {
                    break;
                }
            }
            Some((text[..end].to_owned(), &text[end..]))
        }
        _ => None,
    }
}

pub fn run_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::Enter)
}

pub fn new_tab_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::T)
}

pub fn close_tab_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::W)
}

pub fn find_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::F)
}

/// Per-tab find bar state, kept in the egui context data keyed by tab id.
#[derive(Clone, Default)]
struct FindBar {
    open: bool,
    selected: Option<usize>,
    state: FindState,
}

/// Find bar state id for one tab, the ctx-data pattern `complete.rs` uses.
fn find_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_find", tab))
}

fn find_bar_of(ctx: &egui::Context, tab: TabId) -> FindBar {
    ctx.data_mut(|d| d.get_temp(find_id(tab)).unwrap_or_default())
}

fn store_find_bar(ctx: &egui::Context, tab: TabId, bar: FindBar) {
    ctx.data_mut(|d| d.insert_temp(find_id(tab), bar));
}

/// Flip the find bar open and report the new state.
fn toggle_find(ctx: &egui::Context, tab: TabId) -> bool {
    let mut bar = find_bar_of(ctx, tab);
    bar.open = !bar.open;
    if bar.open {
        bar.selected = None;
    }
    let open = bar.open;
    store_find_bar(ctx, tab, bar);
    open
}

fn close_find(ctx: &egui::Context, tab: TabId) {
    let mut bar = find_bar_of(ctx, tab);
    bar.open = false;
    store_find_bar(ctx, tab, bar);
}

/// Drop the find bar and the editor selection for a tab that is gone.
fn forget_find(ctx: &egui::Context, tab: TabId) {
    ctx.data_mut(|d| d.remove::<FindBar>(find_id(tab)));
    super::complete::clear_selection(ctx, tab);
}

/// Find state is keyed by tab id, so a closed tab leaves it behind. Diff
/// against the live tab list instead of hooking every close path.
fn prune_find(state: &AppState, ctx: &egui::Context) {
    let id = egui::Id::new("crusty_find_tabs");
    let known: Vec<TabId> = ctx.data_mut(|d| d.get_temp(id).unwrap_or_default());
    let live: Vec<TabId> = state.tabs().iter().map(|t| t.id).collect();
    for tab in known.iter().filter(|t| !live.contains(t)) {
        forget_find(ctx, *tab);
    }
    if known != live {
        ctx.data_mut(|d| d.insert_temp(id, live));
    }
}

/// Find bar for the active tab. The count comes from `FindState::find_all`;
/// Next and Prev move the editor selection onto the match.
fn find_bar(state: &AppState, ui: &mut egui::Ui, just_opened: bool) {
    let Some(tab_id) = state.active() else { return };
    let mut bar = find_bar_of(ui.ctx(), tab_id);
    if !bar.open {
        return;
    }
    let sql = state.tab(tab_id).map(|t| t.sql.as_str()).unwrap_or("");
    let hits = bar.state.find(sql);
    if bar.selected.is_some_and(|i| i >= hits.len()) {
        bar.selected = None;
    }
    let mut pick: Option<usize> = None;
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label("Find");
        let field = ui.add(
            egui::TextEdit::singleline(&mut bar.state.query)
                .desired_width(200.0)
                .hint_text("text to find"),
        );
        if just_opened {
            field.request_focus();
        }
        if field.changed() {
            bar.selected = None;
        }
        ui.label(format!(
            "{}/{}",
            bar.selected.map_or(0, |i| i + 1),
            hits.len()
        ));
        if !hits.is_empty() {
            let count = hits.len();
            if ui.button("Prev").clicked() {
                pick = Some(bar.selected.map_or(count - 1, |i| (i + count - 1) % count));
            }
            if ui.button("Next").clicked() {
                pick = Some(bar.selected.map_or(0, |i| (i + 1) % count));
            }
        }
        if ui.small_button("×").clicked() {
            close = true;
        }
    });
    if let Some(i) = pick {
        bar.selected = Some(i);
        if let Some(bytes) = hits.get(i) {
            super::complete::set_selection(ui.ctx(), tab_id, char_range(sql, bytes));
            // egui paints a selection only while the TextEdit holds focus.
            ui.ctx()
                .memory_mut(|m| m.request_focus(super::complete::editor_id(tab_id)));
        }
    }
    if close {
        bar.open = false;
    }
    store_find_bar(ui.ctx(), tab_id, bar);
}

fn save_window(state: &mut AppState, ui: &mut egui::Ui) {
    if state.save_prompt().is_none() {
        return;
    }
    let mut confirm = false;
    let mut close = false;
    egui::Window::new("Save query").show(ui.ctx(), |ui| {
        let Some(prompt) = state.save_prompt_mut() else { return };
        ui.horizontal(|ui| {
            ui.label("Name:");
            ui.text_edit_singleline(&mut prompt.name);
        });
        ui.horizontal(|ui| {
            ui.label("Folder:");
            ui.add(
                egui::TextEdit::singleline(&mut prompt.folder)
                    .hint_text("no folder"),
            );
        });
        ui.checkbox(&mut prompt.pinned, "Pinned")
            .on_hover_text("List this entry first inside its folder group");
        if let Some(error) = prompt.error.clone() {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                confirm = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if confirm {
        state.confirm_save_prompt();
    } else if close {
        state.close_save_prompt();
    }
}

/// The write guard modal for an UPDATE or DELETE without WHERE. Names the
/// statement kind and the affected table; Confirm sends the held batch
/// exactly as prepared, Cancel drops it.
pub fn write_confirm_window(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(pending) = state.pending_write().cloned() else { return };
    let mut confirm = false;
    let mut cancel = false;
    egui::Window::new(format!("{} without WHERE?", pending.kind)).show(ui.ctx(), |ui| {
        ui.label(format!(
            "{} on {} has no WHERE clause and affects every row.",
            pending.kind, pending.table
        ));
        ui.horizontal(|ui| {
            if ui.button("Confirm").clicked() {
                confirm = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    });
    if confirm {
        state.confirm_write();
    } else if cancel {
        state.cancel_write();
    }
}

/// The staged-edit SQL preview. Lists the exact UPDATE, DELETE, and INSERT
/// statements the apply path would send, in send order; Confirm sends them
/// through the unchanged apply path, Cancel keeps the staged edits.
pub fn edit_preview_window(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(tab) = state.pending_edit_preview() else { return };
    let table = state.edit_target(tab).map(|target| target.table).unwrap_or_default();
    let statements = state.preview_edit_statements(tab).unwrap_or_default();
    if statements.is_empty() {
        state.cancel_pending_edits();
        return;
    }
    let mut confirm = false;
    let mut cancel = false;
    egui::Window::new("Apply staged edits?").show(ui.ctx(), |ui| {
        ui.label(format!(
            "{} statement{} on {table}. Confirm sends them, Cancel keeps the staged edits.",
            statements.len(),
            if statements.len() == 1 { "" } else { "s" },
        ));
        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
            for statement in &statements {
                ui.monospace(statement);
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Confirm").clicked() {
                confirm = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    });
    if confirm {
        state.confirm_pending_edits();
    } else if cancel {
        state.cancel_pending_edits();
    }
}

/// The value prompt for a run holding `:name` and `?` placeholders. Lists
/// each parameter with a text field; Run substitutes the values and continues
/// the prepared batch through the send path, Cancel sends nothing.
pub fn params_window(state: &mut AppState, ui: &mut egui::Ui) {
    if state.pending_params().is_none() {
        return;
    }
    let mut run = false;
    let mut cancel = false;
    egui::Window::new("Query parameters").show(ui.ctx(), |ui| {
        let Some(pending) = state.pending_params_mut() else { return };
        for param in &mut pending.params {
            ui.horizontal(|ui| {
                ui.label(param.display());
                ui.text_edit_singleline(&mut param.value);
            });
        }
        ui.horizontal(|ui| {
            if ui.button("Run").clicked() {
                run = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    });
    if run {
        state.confirm_params();
    } else if cancel {
        state.cancel_params();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_carry_expected_modifiers_and_keys() {
        let run = run_shortcut();
        assert_eq!(run.modifiers, egui::Modifiers::CTRL);
        assert_eq!(run.logical_key, egui::Key::Enter);
        let new_tab = new_tab_shortcut();
        assert_eq!(new_tab.modifiers, egui::Modifiers::CTRL);
        assert_eq!(new_tab.logical_key, egui::Key::T);
        let close_tab = close_tab_shortcut();
        assert_eq!(close_tab.modifiers, egui::Modifiers::CTRL);
        assert_eq!(close_tab.logical_key, egui::Key::W);
        let find = find_shortcut();
        assert_eq!(find.modifiers, egui::Modifiers::CTRL);
        assert_eq!(find.logical_key, egui::Key::F);
    }

    #[test]
    fn tab_title_names_the_select_table() {
        assert_eq!(
            tab_title("SELECT id, email FROM users WHERE active = 1 ORDER BY id".to_owned()),
            "users"
        );
        assert_eq!(tab_title("select * from Public.Users".to_owned()), "Users");
        assert_eq!(tab_title("SELECT * FROM \"my table\"".to_owned()), "my table");
        assert_eq!(tab_title("SELECT * FROM main.`orders`".to_owned()), "orders");
        assert_eq!(
            tab_title("SELECT * FROM (SELECT id FROM users) recent".to_owned()),
            "SELECT * FROM (SELECT id…"
        );
        assert_eq!(tab_title("SELECT /* from x */ * FROM users".to_owned()), "users");
        assert_eq!(tab_title("SELECT 1 -- from audit\nFROM users".to_owned()), "users");
    }

    #[test]
    fn tab_title_falls_back_to_the_trimmed_prefix() {
        assert_eq!(
            tab_title("WITH recent AS (SELECT * FROM users) SELECT * FROM recent".to_owned()),
            "WITH recent AS (SELECT *…"
        );
        assert_eq!(
            tab_title("INSERT INTO users VALUES (1)".to_owned()),
            "INSERT INTO users VALUES…"
        );
        assert_eq!(tab_title("SELECT 1".to_owned()), "SELECT 1");
        assert_eq!(tab_title("   ".to_owned()), "SQL");
    }

    #[test]
    fn numbered_titles_suffix_repeats_in_tab_order() {
        let titles: Vec<String> =
            ["users", "orders", "users", "users"].map(str::to_owned).to_vec();
        assert_eq!(
            numbered_titles(&titles),
            ["users", "orders", "users 2", "users 3"].map(str::to_owned).to_vec()
        );
    }
}
// Manual check: Ctrl+T creates a tab on default_connection and activates it
// (no-op with no connection), Ctrl+W closes the active tab (no-op with none),
// Ctrl+F toggles the find bar for the active tab, and all three stay off with
// a draft, save, or password modal open, using the same new_tab/activate/
// close_tab/default_connection transitions as the plus and cross buttons
// covered by AppState tests. Explain runs start_explain with the same offsets
// Run passes, and Format and the find bar's Next/Prev selection are manual
// too. The Autocommit toggle beside Begin/Commit/Rollback sends one
// `SetAutocommit` through `set_autocommit` (disabled for SQLite and while a
// run is in flight) and shows the landed state, covered by AppState tests.
