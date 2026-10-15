//! Tab bar, SQL editor, toolbar, and the find bar.
//!
//! Clicks are collected while a tab borrow is live and applied after it
//! ends, so the editor borrow never overlaps a transition.

use eframe::egui;

use super::editor::{char_range, format_sql, FindState};
use crate::app::{AppState, RunOutcome, RunScope};
use crate::db::TabId;

const RUN_GREEN: egui::Color32 = egui::Color32::from_rgb(115, 180, 115);

pub fn editor(state: &mut AppState, ui: &mut egui::Ui) {
    prune_find(state, ui.ctx());
    let modal_open = state.draft().is_some()
        || state.save_prompt().is_some()
        || state.password_prompt().is_some();
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
    if let Some(id) = state.active() {
        let hint = state
            .tab(id)
            .and_then(|tab| state.connections().iter().find(|c| c.id == tab.conn))
            .map(|c| format!("{} · {}", c.name, c.config.kind().label()))
            .unwrap_or_else(|| "no connection".to_owned());
        ui.label(egui::RichText::new(hint).small().weak());
        let schema = state
            .tab(id)
            .map(|tab| tab.conn)
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
        for tab in state.tabs() {
            let is_active = Some(tab.id) == active;
            let title = tab_title(tab.sql.clone());
            let label = if is_active {
                egui::RichText::new(title).strong()
            } else {
                egui::RichText::new(title).weak()
            };
            if ui.selectable_value(&mut active, Some(tab.id), label).clicked() {
                activate = Some(tab.id);
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
    let mut scope = state.tab(tab_id).map(|t| t.run_scope()).unwrap_or_default();
    if state.tab(tab_id).is_some() {
        let running = matches!(
            state.tab(tab_id).map(|t| t.outcome()),
            Some(RunOutcome::Running { .. })
        );
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
                        egui::Button::new(egui::RichText::new("▶ Run").strong()).fill(RUN_GREEN),
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

pub(crate) fn tab_title(sql: String) -> String {
    let first = sql.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("SQL");
    let mut title: String = first.chars().take(24).collect();
    if first.chars().count() > 24 {
        title += "…";
    }
    title
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
}
// Manual check: Ctrl+T creates a tab on default_connection and activates it
// (no-op with no connection), Ctrl+W closes the active tab (no-op with none),
// Ctrl+F toggles the find bar for the active tab, and all three stay off with
// a draft, save, or password modal open, using the same new_tab/activate/
// close_tab/default_connection transitions as the plus and cross buttons
// covered by AppState tests. Explain runs start_explain with the same offsets
// Run passes, and Format and the find bar's Next/Prev selection are manual
// too.
