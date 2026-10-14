//! Rendering only. Reads state through accessors, changes it through
//! transitions. Panel order is call order; `CentralPanel` is always last.

use eframe::egui;

use crate::app::{AppState, ConnectionState};

pub mod connections;
pub mod grid;
pub mod query;

pub fn root(state: &mut AppState, ui: &mut egui::Ui) {
    egui::Panel::left("connections")
        .resizable(true)
        .show(ui, |ui| connections::sidebar(state, ui));
    egui::Panel::bottom("status").show(ui, |ui| status_bar(state, ui));
    egui::Panel::top("editor")
        .resizable(true)
        .show(ui, |ui| query::editor(state, ui));
    egui::CentralPanel::default().show(ui, |ui| grid::results(state, ui));
    connections::draft_window(state, ui);
}

fn status_bar(state: &AppState, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        let live = state
            .connections()
            .iter()
            .filter(|c| {
                matches!(
                    c.state(),
                    ConnectionState::Connected { .. } | ConnectionState::QueryRunning { .. }
                )
            })
            .count();
        ui.label(format!(
            "{} connection{}, {} live",
            state.connections().len(),
            if state.connections().len() == 1 { "" } else { "s" },
            live
        ));
        ui.separator();
        match state.active().and_then(|id| state.tab(id)) {
            Some(tab) => match tab.outcome() {
                crate::app::RunOutcome::Ok(result) => {
                    let mut text = format!("{} rows", result.row_count());
                    if result.is_truncated() {
                        text += " (truncated)";
                    }
                    if let Some(affected) = result.affected() {
                        text = format!("{affected} rows affected");
                    }
                    ui.label(text);
                }
                crate::app::RunOutcome::Err(_) => {
                    ui.colored_label(ui.visuals().error_fg_color, "query failed");
                }
                crate::app::RunOutcome::Running { .. } => {
                    ui.label("running…");
                }
                crate::app::RunOutcome::Never => {
                    ui.label("no query yet");
                }
            },
            None => {
                ui.label("no tab");
            }
        }
    });
}

/// Dark Beekeeper-ish visuals, installed once at startup.
pub fn install(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.selection.bg_fill = egui::Color32::from_rgb(58, 110, 165);
    ctx.set_visuals(visuals);
}
