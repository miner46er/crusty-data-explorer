//! The Ctrl+K command palette: a live-filtered list over connections, saved
//! queries, and the default connection's loaded tables and columns.

use eframe::egui;

use crate::app::{AppState, SearchResult};

pub fn search_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::K)
}

/// The name and detail one palette row shows.
fn row_text(state: &AppState, result: &SearchResult) -> (String, String) {
    match result {
        SearchResult::Connection(conn) => state
            .connections()
            .iter()
            .find(|c| c.id == *conn)
            .map(|c| {
                (c.name.clone(), format!("{} · {}", c.config.kind().label(), c.env.label()))
            })
            .unwrap_or_else(|| ("missing connection".to_owned(), String::new())),
        SearchResult::Saved(id) => state
            .saved_queries()
            .iter()
            .find(|q| q.id == *id)
            .map(|q| (q.name.clone(), q.folder.clone().unwrap_or_default()))
            .unwrap_or_else(|| ("missing query".to_owned(), String::new())),
        SearchResult::Table { schema, table, .. } => (table.clone(), schema.clone()),
        SearchResult::Column { schema, table, column, .. } => {
            (column.clone(), format!("{schema}.{table}"))
        }
    }
}

/// Pump the palette from `root`: the Ctrl+K toggle everywhere, then the
/// window, its keys, and its rows while it is open.
pub fn palette(state: &mut AppState, ui: &mut egui::Ui) {
    let other_modal_open = state.draft().is_some()
        || state.save_prompt().is_some()
        || state.password_prompt().is_some()
        || state.import_draft().is_some()
        || state.pending_write().is_some()
        || state.pending_params().is_some();
    let toggled = ui.input_mut(|i| i.consume_shortcut(&search_shortcut()));
    if toggled {
        if state.palette_open() {
            state.close_palette();
        } else if !other_modal_open {
            state.open_palette();
        }
    }
    if !state.palette_open() {
        return;
    }
    let just_opened = toggled;
    // The arrows, Enter, and Escape are claimed before the filter box sees
    // them, so they never move its caret.
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
        state.move_palette_selection(1);
    }
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
        state.move_palette_selection(-1);
    }
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
        state.activate_palette_selection();
    }
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        state.close_palette();
    }
    if !state.palette_open() {
        return;
    }

    let rows: Vec<(String, String)> = state
        .palette()
        .map(|palette| palette.results.iter().map(|result| row_text(state, result)).collect())
        .unwrap_or_default();
    let mut clicked: Option<usize> = None;
    let mut query_changed = false;
    egui::Window::new("Search")
        .collapsible(false)
        .resizable(false)
        .default_width(460.0)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 60.0))
        .show(ui.ctx(), |ui| {
            let Some(palette) = state.palette_mut() else { return };
            let focus = just_opened || ui.memory(|memory| memory.focused().is_none());
            let response = ui.add(
                egui::TextEdit::singleline(&mut palette.query)
                    .hint_text("Search connections, saved queries, tables, and columns")
                    .desired_width(f32::INFINITY),
            );
            query_changed = response.changed();
            if focus {
                response.request_focus();
            }
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for (index, (name, detail)) in rows.iter().enumerate() {
                    let mut text = name.clone();
                    if !detail.is_empty() {
                        text.push_str("  ·  ");
                        text.push_str(detail);
                    }
                    let row = ui.selectable_label(index == palette.selected, text);
                    if row.clicked() {
                        clicked = Some(index);
                    }
                    if index == palette.selected {
                        row.scroll_to_me(None);
                    }
                }
            });
        });
    if query_changed {
        let query = state.palette().map(|palette| palette.query.clone()).unwrap_or_default();
        state.set_palette_query(&query);
    }
    if let Some(index) = clicked {
        if let Some(palette) = state.palette_mut() {
            palette.selected = index;
        }
        state.activate_palette_selection();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_carries_ctrl_k() {
        let search = search_shortcut();
        assert_eq!(search.modifiers, egui::Modifiers::CTRL);
        assert_eq!(search.logical_key, egui::Key::K);
    }
}

// Manual check: Ctrl+K opens the palette over any tab; typing filters live by
// prefix, word start, or subsequence over connection names and kinds, saved
// names and folders, and the default connection's loaded tables and columns;
// Up/Down move the highlighted row with wrap-around; Enter opens the
// selection (a connection activates or opens its tab, a saved query opens
// its tab, a table fills a fresh tab with its SELECT unrun, a column expands
// and scrolls to its table's entry in the sidebar) and closes the palette;
// Escape closes it. A draft, save, password, write, params, or import window
// keeps Ctrl+K closed, and an open palette keeps the editor, tab, find, and
// grid clipboard shortcuts off the way any other modal does. The open
// actions run the same transitions AppState tests cover.
