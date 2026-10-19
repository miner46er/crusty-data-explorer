//! Tab bar, SQL editor, toolbar, and the find bar.
//!
//! Clicks are collected while a tab borrow is live and applied after it
//! ends, so the editor borrow never overlaps a transition.

use std::collections::{HashMap, HashSet};

use eframe::egui;

use super::editor::{char_range, FindState};
use crate::app::AppState;
use crate::db::TabId;

use super::{
    ModalKey, Palette, approve_button, autofocus, fit, note_dialog_field,
};

/// The tab strip, drawn by `root` for every tab whatever its kind. The new
/// and close shortcuts live here so they work on a table tab too.
pub fn tab_bar(state: &mut AppState, ui: &mut egui::Ui) {
    let modal_open = super::modal_open(state, ui.ctx());
    let (new_tab_shortcut_pressed, close_tab_shortcut_pressed) = ui.input_mut(|i| {
        (
            i.consume_shortcut(&new_tab_shortcut()),
            i.consume_shortcut(&close_tab_shortcut()),
        )
    });
    // Tab switching claims its chords before the TextEdit sees them, so
    // Ctrl+Tab never inserts a tab character and plain Tab stays focus
    // traversal. Previous-tab is checked first: `consume_shortcut` matches
    // logically, so CTRL would also eat Ctrl+Shift+Tab.
    let mut switch: Option<isize> = None;
    let mut switch_nth: Option<usize> = None;
    if !modal_open {
        ui.input_mut(|i| {
            if i.consume_shortcut(&previous_tab_shortcut())
                || i.consume_shortcut(&previous_page_shortcut())
            {
                switch = Some(-1);
            } else if i.consume_shortcut(&next_tab_shortcut())
                || i.consume_shortcut(&next_page_shortcut())
            {
                switch = Some(1);
            } else {
                switch_nth = (1..=9).find(|n| i.consume_shortcut(&tab_nth_shortcut(*n)));
            }
        });
        if let Some(step) = switch {
            state.activate_relative(step);
        }
        if let Some(n) = switch_nth {
            state.activate_nth(n);
        }
    }
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
    let mut active = state.active();
    let mut activate: Option<TabId> = None;
    let mut close: Vec<TabId> = Vec::new();
    let mut new_tab = false;
    // Wrapping, not scrolling: a narrow window puts the tail of the strip on
    // a second row so every tab and its close button stays one click away.
    ui.horizontal_wrapped(|ui| {
        let strip_w = ui.available_width();
        let titles = numbered_titles(
            &state
                .tabs()
                .iter()
                .map(|tab| match &tab.kind {
                    crate::app::TabKind::Table { table, .. } => table.clone(),
                    crate::app::TabKind::Query => tab_title(tab.sql.clone()),
                })
                .collect::<Vec<_>>(),
        );
        for (tab, title) in state.tabs().iter().zip(titles) {
            let transaction_open = state
                .connections()
                .iter()
                .find(|c| Some(c.id) == tab.conn)
                .is_some_and(|c| c.transaction().is_open());
            // One item per tab, measured before the strip lays it out: the
            // wrapped row can then put the whole group on the next line, so
            // the title never ends a row that its close button starts. A group
            // wider than the row tightens its own gap first; the buttons keep
            // their labels.
            let gap = ui.spacing().item_spacing.x;
            let title_w = fit::button_w(ui, &title);
            let close_w = fit::button_w(ui, "×");
            let tx_w = if transaction_open {
                fit::text_w(ui, "tx", egui::TextStyle::Small)
            } else {
                0.0
            };
            let gaps = if transaction_open { 2.0 } else { 1.0 };
            let content_w = title_w + close_w + tx_w;
            let inner_gap = if content_w + gaps * gap <= strip_w {
                gap
            } else {
                ((strip_w - 1.0 - content_w) / gaps).clamp(0.0, gap)
            };
            let group_w = content_w + gaps * inner_gap;
            let title_h =
                ui.text_style_height(&egui::TextStyle::Button) + 2.0 * ui.spacing().button_padding.y;
            ui.allocate_ui_with_layout(
                egui::vec2(group_w, title_h),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = inner_gap;
                    let is_active = Some(tab.id) == active;
                    let label = if is_active {
                        egui::RichText::new(title).strong()
                    } else {
                        egui::RichText::new(title).weak()
                    };
                    if ui.selectable_value(&mut active, Some(tab.id), label).clicked() {
                        activate = Some(tab.id);
                    }
                    if transaction_open {
                        ui.label(egui::RichText::new("tx").small().strong())
                            .on_hover_text("Open transaction: commit or roll back to close it");
                    }
                    if ui.small_button("×").clicked() {
                        close.push(tab.id);
                    }
                },
            );
        }
        if ui
            .small_button("+")
            .on_hover_text(format!("New tab ({})", ui.ctx().format_shortcut(&new_tab_shortcut())))
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

/// The SQL editor panel: toolbar, find bar, and the editor well. Drawn by
/// `root` for `Query` tabs only.
pub fn editor(state: &mut AppState, ui: &mut egui::Ui) {
    prune_find(state, ui.ctx());
    let modal_open = super::modal_open(state, ui.ctx());
    let find_shortcut_pressed = ui.input_mut(|i| i.consume_shortcut(&find_shortcut()));
    let mut find_opened = false;
    if !modal_open && find_shortcut_pressed {
        if let Some(id) = state.active() {
            find_opened = toggle_find(ui.ctx(), id);
        }
    }
    find_opened |= super::query_tools::toolbar(state, ui);
    ui.add_space(4.0);
    if let Some(id) = state.active() {
        // A dialog owns the keyboard: a stale editor caret would still take
        // its text, Enter, and Tab, and the completion popup its Enter and
        // Escape. Drop the caret and draw the well without an input.
        if modal_open
            && ui.ctx().memory(|memory| memory.focused())
                == Some(super::complete::editor_id(id))
        {
            ui.ctx()
                .memory_mut(|memory| memory.surrender_focus(super::complete::editor_id(id)));
        }
        let schema = state
            .tab(id)
            .and_then(|tab| tab.conn)
            .and_then(|conn| state.schema(conn))
            .cloned();
        ui.add_enabled_ui(!modal_open, |ui| find_bar(state, ui, find_opened));
        if let Some(sql) = state.tab_sql_mut(id) {
            egui::Frame::canvas(ui.style())
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    if modal_open {
                        super::complete::editor_paused(id, sql, ui);
                    } else {
                        super::complete::editor_with_completion(id, schema.as_ref(), sql, ui);
                    }
                });
        }
        // The completion popup gets first claim on Esc; close find only if
        // free, and hand the caret back to the editor either way.
        if !modal_open
            && find_bar_of(ui.ctx(), id).open
            && ui.input_mut(|i| super::consume_plain_key(i, egui::Key::Escape))
        {
            close_find(ui.ctx(), id);
            ui.ctx()
                .memory_mut(|m| m.request_focus(super::complete::editor_id(id)));
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

/// The table tab's header, in place of the editor: the qualified name, a
/// Refresh that re-runs the tab's current SQL, and SQL to take it over in the
/// editor. Ctrl+Enter reruns, the table tab's Run.
pub fn table_header(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(id) = state.active() else { return };
    let Some((schema, table, sql)) = state.tab(id).and_then(|tab| match &tab.kind {
        crate::app::TabKind::Table { schema, table } => {
            Some((schema.clone(), table.clone(), tab.sql.clone()))
        }
        crate::app::TabKind::Query => None,
    }) else {
        return;
    };
    let modal_open = super::modal_open(state, ui.ctx());
    let mut edit = false;
    let mut refresh = ui.input_mut(|i| i.consume_shortcut(&run_shortcut()));
    // A table tab has no editor to search, so find lands in the row filter.
    if ui.input_mut(|i| i.consume_shortcut(&find_shortcut())) && !modal_open {
        super::grid::focus_filter(ui.ctx(), id);
    }
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{schema}.{table}")).strong());
        refresh |= ui
            .button("Refresh")
            .on_hover_text("Re-run this table's SELECT")
            .clicked();
        edit = ui
            .button("SQL")
            .on_hover_text("Edit this table's SELECT as a query")
            .clicked();
    });
    if refresh && !modal_open {
        let len = sql.len();
        state.start_run(id, &sql, len, None);
    }
    if edit {
        state.edit_as_query(id);
    }
}

pub(crate) fn tab_title(sql: String) -> String {
    if let Some(table) = crate::sql::select_table_title(&sql) {
        return table;
    }
    let first = sql.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("SQL");
    crate::sql::capped_title(first)
}

/// Tab-strip labels: each title, with " 2", " 3", … appended to repeats in
/// tab order. A suffix a literal title already holds keeps bumping, so a
/// table named "users 2" never collides with the numbering.
fn numbered_titles(titles: &[String]) -> Vec<String> {
    let mut used: HashSet<String> = HashSet::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut labels = Vec::with_capacity(titles.len());
    for title in titles {
        let count = counts.entry(title.clone()).or_insert(0);
        *count += 1;
        let mut label = if *count == 1 { title.clone() } else { format!("{title} {count}") };
        while !used.insert(label.clone()) {
            *count += 1;
            label = format!("{title} {count}");
        }
        labels.push(label);
    }
    labels
}

pub fn run_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Enter)
}

pub fn new_tab_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::T)
}

pub fn close_tab_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::W)
}

pub fn find_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::F)
}

/// Ctrl+Tab on every platform: Cmd+Tab is the macOS app switcher.
pub fn next_tab_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::Tab)
}

/// Ctrl+Shift+Tab, the reverse of [`next_tab_shortcut`]. Checked first,
/// because `consume_shortcut` matches logically and Ctrl alone would eat it.
pub fn previous_tab_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::Tab)
}

pub fn next_page_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::PageDown)
}

pub fn previous_page_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::PageUp)
}

/// The Cmd/Ctrl+N tab chord, 1 through 9. Nine selects the last tab.
pub fn tab_nth_shortcut(n: usize) -> egui::KeyboardShortcut {
    const KEYS: [egui::Key; 9] = [
        egui::Key::Num1,
        egui::Key::Num2,
        egui::Key::Num3,
        egui::Key::Num4,
        egui::Key::Num5,
        egui::Key::Num6,
        egui::Key::Num7,
        egui::Key::Num8,
        egui::Key::Num9,
    ];
    let key = *KEYS.get(n - 1).expect("tab numbers are 1 through 9");
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, key)
}

/// Per-tab find bar state, kept in the egui context data keyed by tab id.
#[derive(Clone, Default)]
struct FindBar {
    open: bool,
    selected: Option<usize>,
    state: FindState,
    /// Whether the find field held focus at the end of last frame. egui
    /// clears focus on a plain Escape before the UI runs, so the bar cannot
    /// ask the live memory while deciding who owns Escape.
    field_focused: bool,
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

/// Whether the find bar is open for one tab, read by the editor toolbar.
pub(crate) fn find_open(ctx: &egui::Context, tab: TabId) -> bool {
    find_bar_of(ctx, tab).open
}

/// Flip the find bar open and report the new state.
pub(crate) fn toggle_find(ctx: &egui::Context, tab: TabId) -> bool {
    let mut bar = find_bar_of(ctx, tab);
    bar.open = !bar.open;
    if bar.open {
        bar.selected = None;
        bar.field_focused = false;
    }
    let open = bar.open;
    store_find_bar(ctx, tab, bar);
    open
}

fn close_find(ctx: &egui::Context, tab: TabId) {
    let mut bar = find_bar_of(ctx, tab);
    bar.open = false;
    bar.field_focused = false;
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
    // Claimed before the field draws: a singleline TextEdit answers Enter by
    // surrendering focus, which would swallow the step. Keyboard stepping
    // keeps the caret in the find field; the buttons below still hand focus
    // to the editor so the selection paints.
    let focused = bar.field_focused;
    let mut step: Option<isize> = None;
    if focused {
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::Enter)) {
            step = Some(-1);
        } else if ui.input_mut(|i| super::consume_plain_key(i, egui::Key::Enter)) {
            step = Some(1);
        }
    }
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
        bar.field_focused = field.has_focus();
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
    if let Some(direction) = step {
        let count = hits.len();
        if count > 0 {
            pick = Some(match direction {
                -1 => bar.selected.map_or(count - 1, |i| (i + count - 1) % count),
                _ => bar.selected.map_or(0, |i| (i + 1) % count),
            });
        }
    }
    if let Some(i) = pick {
        bar.selected = Some(i);
        if let Some(bytes) = hits.get(i) {
            super::complete::set_selection(ui.ctx(), tab_id, char_range(sql, bytes));
            if step.is_none() {
                // egui paints a selection only while the TextEdit holds focus.
                ui.ctx()
                    .memory_mut(|m| m.request_focus(super::complete::editor_id(tab_id)));
            }
        }
    }
    if close {
        bar.open = false;
        ui.ctx()
            .memory_mut(|m| m.request_focus(super::complete::editor_id(tab_id)));
    }
    store_find_bar(ui.ctx(), tab_id, bar);
}

pub fn save_window(
    state: &mut AppState,
    ui: &mut egui::Ui,
    key: Option<ModalKey>,
) -> Option<egui::LayerId> {
    if state.save_prompt().is_none() {
        return None;
    }
    let mut confirm = false;
    let mut close = false;
    let mut field_focused = false;
    let window = egui::Window::new("Save query").show(ui.ctx(), |ui| {
        let Some(prompt) = state.save_prompt_mut() else { return };
        ui.horizontal(|ui| {
            ui.label("Name:");
            let name = ui.text_edit_singleline(&mut prompt.name);
            field_focused |= name.has_focus();
            autofocus(ui, "save-name", &name);
            note_dialog_field(ui.ctx(), crate::app::Modal::SavePrompt, &name);
        });
        ui.horizontal(|ui| {
            ui.label("Folder:");
            let folder = ui.add(
                egui::TextEdit::singleline(&mut prompt.folder)
                    .hint_text("no folder"),
            );
            field_focused |= folder.has_focus();
            note_dialog_field(ui.ctx(), crate::app::Modal::SavePrompt, &folder);
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
    let free_focus = ui.ctx().memory(|memory| memory.focused().is_none());
    let submit = key == Some(ModalKey::Submit) && (field_focused || free_focus);
    if confirm || submit {
        state.confirm_save_prompt();
    } else if close || key == Some(ModalKey::Cancel) {
        state.close_save_prompt();
    }
    window.map(|window| window.response.layer_id)
}

/// The write guard modal for an UPDATE or DELETE without WHERE. Names the
/// statement kind and the affected table; Confirm sends the held batch
/// exactly as prepared, Cancel drops it.
pub fn write_confirm_window(
    state: &mut AppState,
    ui: &mut egui::Ui,
    key: Option<ModalKey>,
) -> Option<egui::LayerId> {
    let Some(pending) = state.pending_write().cloned() else { return None };
    let mut confirm = false;
    let mut cancel = false;
    let window = egui::Window::new(format!("{} without WHERE?", pending.kind)).show(ui.ctx(), |ui| {
        ui.label(format!(
            "{} on {} has no WHERE clause and affects every row.",
            pending.kind, pending.table
        ));
        ui.horizontal(|ui| {
            if approve_button(ui, "Confirm").clicked() {
                confirm = true;
            }
            let cancel_button = ui.button("Cancel");
            autofocus(ui, "write-cancel", &cancel_button);
            if cancel_button.clicked() {
                cancel = true;
            }
        });
    });
    if confirm {
        state.confirm_write();
    } else if cancel || key == Some(ModalKey::Cancel) {
        state.cancel_write();
    }
    window.map(|window| window.response.layer_id)
}

/// The staged-edit SQL preview. Lists the exact UPDATE, DELETE, and INSERT
/// statements the apply path would send, in send order; Confirm sends them
/// through the unchanged apply path, Cancel keeps the staged edits.
pub fn edit_preview_window(
    state: &mut AppState,
    ui: &mut egui::Ui,
    key: Option<ModalKey>,
) -> Option<egui::LayerId> {
    let Some(tab) = state.pending_edit_preview() else { return None };
    let table = state.edit_target(tab).map(|target| target.table).unwrap_or_default();
    let statements = state.preview_edit_statements(tab).unwrap_or_default();
    if statements.is_empty() {
        state.cancel_pending_edits();
        return None;
    }
    let mut confirm = false;
    let mut cancel = false;
    let window = egui::Window::new("Apply staged edits?").show(ui.ctx(), |ui| {
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
            if approve_button(ui, "Confirm").clicked() {
                confirm = true;
            }
            let cancel_button = ui.button("Cancel");
            autofocus(ui, "edit-preview-cancel", &cancel_button);
            if cancel_button.clicked() {
                cancel = true;
            }
        });
    });
    if confirm {
        state.confirm_pending_edits();
    } else if cancel || key == Some(ModalKey::Cancel) {
        state.cancel_pending_edits();
    }
    window.map(|window| window.response.layer_id)
}

/// The value prompt for a run holding `:name` and `?` placeholders. Lists
/// each parameter with a text field; Run substitutes the values and continues
/// the prepared batch through the send path, Cancel sends nothing.
pub fn params_window(
    state: &mut AppState,
    ui: &mut egui::Ui,
    key: Option<ModalKey>,
) -> Option<egui::LayerId> {
    if state.pending_params().is_none() {
        return None;
    }
    let mut run = false;
    let mut cancel = false;
    let mut field_focused = false;
    let window = egui::Window::new("Query parameters").show(ui.ctx(), |ui| {
        let Some(pending) = state.pending_params_mut() else { return };
        for (index, param) in pending.params.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(param.display());
                let field = ui.text_edit_singleline(&mut param.value);
                field_focused |= field.has_focus();
                note_dialog_field(ui.ctx(), crate::app::Modal::Params, &field);
                if index == 0 {
                    autofocus(ui, "params-first", &field);
                }
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
    let free_focus = ui.ctx().memory(|memory| memory.focused().is_none());
    if run || (key == Some(ModalKey::Submit) && (field_focused || free_focus)) {
        state.confirm_params();
    } else if cancel || key == Some(ModalKey::Cancel) {
        state.cancel_params();
    }
    window.map(|window| window.response.layer_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_carry_expected_modifiers_and_keys() {
        let run = run_shortcut();
        assert_eq!(run.modifiers, egui::Modifiers::COMMAND);
        assert_eq!(run.logical_key, egui::Key::Enter);
        let new_tab = new_tab_shortcut();
        assert_eq!(new_tab.modifiers, egui::Modifiers::COMMAND);
        assert_eq!(new_tab.logical_key, egui::Key::T);
        let close_tab = close_tab_shortcut();
        assert_eq!(close_tab.modifiers, egui::Modifiers::COMMAND);
        assert_eq!(close_tab.logical_key, egui::Key::W);
        let find = find_shortcut();
        assert_eq!(find.modifiers, egui::Modifiers::COMMAND);
        assert_eq!(find.logical_key, egui::Key::F);
        let next_tab = next_tab_shortcut();
        assert_eq!(next_tab.modifiers, egui::Modifiers::CTRL);
        assert_eq!(next_tab.logical_key, egui::Key::Tab);
        let previous_tab = previous_tab_shortcut();
        assert_eq!(previous_tab.modifiers, egui::Modifiers::CTRL | egui::Modifiers::SHIFT);
        assert_eq!(previous_tab.logical_key, egui::Key::Tab);
        let next_page = next_page_shortcut();
        assert_eq!(next_page.modifiers, egui::Modifiers::CTRL);
        assert_eq!(next_page.logical_key, egui::Key::PageDown);
        let previous_page = previous_page_shortcut();
        assert_eq!(previous_page.modifiers, egui::Modifiers::CTRL);
        assert_eq!(previous_page.logical_key, egui::Key::PageUp);
        for n in 1..=9 {
            let nth = tab_nth_shortcut(n);
            assert_eq!(nth.modifiers, egui::Modifiers::COMMAND, "tab {n}");
        }
        assert_eq!(tab_nth_shortcut(1).logical_key, egui::Key::Num1);
        assert_eq!(tab_nth_shortcut(9).logical_key, egui::Key::Num9);
    }

    #[test]
    fn run_hint_names_the_platform_modifier() {
        let hint = |os| {
            let ctx = egui::Context::default();
            ctx.set_os(os);
            let mut text = String::new();
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                text = ui.ctx().format_shortcut(&run_shortcut());
            });
            output.textures_delta.clear();
            text
        };
        assert_eq!(hint(egui::os::OperatingSystem::Nix), "Ctrl+Enter");
        assert_eq!(hint(egui::os::OperatingSystem::Windows), "Ctrl+Enter");
        assert_eq!(hint(egui::os::OperatingSystem::Mac), "Cmd+Enter");
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
    fn tab_title_ignores_a_from_inside_parentheses_or_a_later_statement() {
        assert_eq!(
            tab_title("SELECT EXTRACT(YEAR FROM created_at) FROM orders".to_owned()),
            "orders"
        );
        assert_eq!(
            tab_title("SELECT (SELECT count(*) FROM audit_log) AS m FROM users".to_owned()),
            "users"
        );
        assert_eq!(
            tab_title("SELECT 1; DELETE FROM users".to_owned()),
            "SELECT 1; DELETE FROM us…"
        );
        assert_eq!(tab_title("-- pick\nSELECT * FROM users".to_owned()), "users");
        assert_eq!(tab_title("SELECT * FROM \"my\"\"table\"".to_owned()), "my\"table");
        assert_eq!(tab_title("SELECT * FROM \"users 2\"".to_owned()), "users 2");
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

    #[test]
    fn numbered_titles_bump_past_a_suffix_a_literal_title_holds() {
        let titles: Vec<String> =
            ["users", "users 2", "users", "users"].map(str::to_owned).to_vec();
        assert_eq!(
            numbered_titles(&titles),
            ["users", "users 2", "users 3", "users 4"].map(str::to_owned).to_vec()
        );
        let titles: Vec<String> = ["a", "a", "a 2"].map(str::to_owned).to_vec();
        assert_eq!(
            numbered_titles(&titles),
            ["a", "a 2", "a 2 2"].map(str::to_owned).to_vec()
        );
    }
}

// Manual check: Ctrl+T creates a tab on default_connection and activates it
// (no-op with no connection), Ctrl+W closes the active tab (no-op with none),
// Ctrl+F toggles the find bar for the active tab, and all three stay off with
// a draft, save, or password modal open, using the same new_tab/activate/
// close_tab/default_connection transitions as the plus and cross buttons
// covered by AppState tests. The find bar's Next/Prev selection is manual too.
