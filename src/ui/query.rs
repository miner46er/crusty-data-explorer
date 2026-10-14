//! Tab bar, SQL editor, and the Run/Cancel toolbar.
//!
//! Clicks are collected while a tab borrow is live and applied after it
//! ends, so the editor borrow never overlaps a transition.

use eframe::egui;

use crate::app::{AppState, RunOutcome};
use crate::db::TabId;

pub fn editor(state: &mut AppState, ui: &mut egui::Ui) {
    tab_bar(state, ui);
    toolbar(state, ui);
    if let Some(id) = state.active() {
        if let Some(sql) = state.tab_sql_mut(id) {
            ui.add(
                egui::TextEdit::multiline(sql)
                    .code_editor()
                    .desired_rows(8)
                    .desired_width(f32::INFINITY),
            );
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
            let title = tab_title(tab.sql.clone());
            if ui.selectable_value(&mut active, Some(tab.id), title).clicked() {
                activate = Some(tab.id);
            }
            if ui.small_button("×").clicked() {
                close.push(tab.id);
            }
        }
        if ui.button("+").clicked() {
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

fn toolbar(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(tab_id) = state.active() else { return };
    let (mut run_clicked, mut cancel_clicked) = (false, false);
    if state.tab(tab_id).is_some() {
        ui.horizontal(|ui| {
            run_clicked = ui.button("Run").clicked();
            cancel_clicked = ui.button("Cancel").clicked();
            match state.tab(tab_id).map(|t| t.outcome()) {
                Some(RunOutcome::Running { started, .. }) => {
                    ui.spinner();
                    ui.label(format!("{:.1}s", started.elapsed().as_secs_f32()));
                }
                Some(RunOutcome::Err(msg)) => {
                    ui.colored_label(ui.visuals().error_fg_color, msg);
                }
                Some(RunOutcome::Ok(result)) => {
                    ui.label(format!(
                        "{:.0} ms",
                        result.took().as_secs_f64() * 1000.0
                    ));
                }
                Some(RunOutcome::Never) | None => {}
            }
        });
    }
    run_clicked |= ui.input_mut(|i| i.consume_shortcut(&run_shortcut()));
    if cancel_clicked {
        state.cancel_run(tab_id);
    }
    if run_clicked {
        let sql = state.tab(tab_id).map(|t| t.sql.clone()).unwrap_or_default();
        state.start_run(tab_id, &sql);
    }
}

fn tab_title(sql: String) -> String {
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
