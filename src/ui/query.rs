//! Tab bar, SQL editor, and the Run/Cancel toolbar.
//!
//! Clicks are collected while a tab borrow is live and applied after it
//! ends, so the editor borrow never overlaps a transition.

use eframe::egui;

use crate::app::{AppState, RunOutcome};
use crate::db::TabId;

const RUN_GREEN: egui::Color32 = egui::Color32::from_rgb(115, 180, 115);

pub fn editor(state: &mut AppState, ui: &mut egui::Ui) {
    tab_bar(state, ui);
    ui.add_space(4.0);
    toolbar(state, ui);
    ui.add_space(4.0);
    save_window(state, ui);
    if let Some(id) = state.active() {
        let hint = state
            .tab(id)
            .and_then(|tab| state.connections().iter().find(|c| c.id == tab.conn))
            .map(|c| format!("{} · {}", c.name, c.config.kind().label()))
            .unwrap_or_else(|| "no connection".to_owned());
        ui.label(egui::RichText::new(hint).small().weak());
        if let Some(sql) = state.tab_sql_mut(id) {
            egui::Frame::canvas(ui.style())
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(sql)
                            .code_editor()
                            .desired_rows(8)
                            .desired_width(f32::INFINITY),
                    );
                });
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
        if ui.small_button("+").clicked() {
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
    let (mut run_clicked, mut cancel_clicked, mut save_clicked) = (false, false, false);
    if state.tab(tab_id).is_some() {
        let running = matches!(
            state.tab(tab_id).map(|t| t.outcome()),
            Some(RunOutcome::Running { .. })
        );
        ui.horizontal(|ui| {
            if running {
                cancel_clicked = ui.button("Cancel").clicked();
            } else {
                run_clicked = ui
                    .add(
                        egui::Button::new(egui::RichText::new("▶ Run").strong()).fill(RUN_GREEN),
                    )
                    .clicked();
                ui.label(egui::RichText::new("Ctrl+Enter").small().weak());
                save_clicked = ui.button("Save").clicked();
            }
            match state.tab(tab_id).map(|t| t.outcome()) {
                Some(RunOutcome::Running { started, .. }) => {
                    ui.spinner();
                    ui.label(format!("{:.1}s", started.elapsed().as_secs_f32()));
                }
                Some(RunOutcome::Err(msg)) => {
                    ui.colored_label(ui.visuals().error_fg_color, msg);
                }
                Some(RunOutcome::Ok(result)) => {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} rows · {:.0} ms",
                            result.row_count(),
                            result.took().as_secs_f64() * 1000.0
                        ))
                        .small()
                        .weak(),
                    );
                }
                Some(RunOutcome::Never) | None => {}
            }
        });
    }
    run_clicked |= ui.input_mut(|i| i.consume_shortcut(&run_shortcut()));
    if cancel_clicked {
        state.cancel_run(tab_id);
    }
    if save_clicked {
        state.open_save_prompt();
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
