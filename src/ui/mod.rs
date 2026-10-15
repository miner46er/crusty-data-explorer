//! Rendering only. Reads state through accessors, changes it through
//! transitions. Panel order is call order; `CentralPanel` is always last.

use eframe::egui;

use crate::app::{AppState, ConnectionState};

pub mod complete;
pub mod connections;
pub mod grid;
pub mod query;
pub mod schema;

pub fn root(state: &mut AppState, ui: &mut egui::Ui) {
    egui::Panel::left("connections")
        .resizable(true)
        .default_size(240.0)
        .show(ui, |ui| connections::sidebar(state, ui));
    egui::Panel::bottom("status").show(ui, |ui| status_bar(state, ui));
    egui::Panel::top("editor")
        .resizable(true)
        .show(ui, |ui| query::editor(state, ui));
    egui::CentralPanel::default().show(ui, |ui| grid::results(state, ui));
    connections::draft_window(state, ui);
    connections::password_window(state, ui);
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
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let backend = state
                .active()
                .and_then(|id| state.tab(id))
                .and_then(|tab| state.connections().iter().find(|c| c.id == tab.conn))
                .map(|c| c.config.kind().label())
                .unwrap_or("no backend");
            ui.label(egui::RichText::new(backend).small().weak());
        });
    });
}

/// Dark Beekeeper-ish visuals, installed once at startup.
pub fn install(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        let mut visuals = egui::Visuals::dark();
        visuals.dark_mode = true;
        visuals.window_fill = egui::Color32::from_rgb(24, 29, 37);
        visuals.panel_fill = egui::Color32::from_rgb(32, 39, 50);
        visuals.faint_bg_color = egui::Color32::from_rgb(42, 50, 63);
        visuals.extreme_bg_color = egui::Color32::from_rgb(18, 23, 30);
        visuals.code_bg_color = egui::Color32::from_rgb(40, 48, 60);
        visuals.selection.bg_fill = egui::Color32::from_rgb(58, 110, 165);
        visuals.selection.stroke =
            egui::Stroke::new(1.0, egui::Color32::from_rgb(192, 222, 255));
        visuals.hyperlink_color = egui::Color32::from_rgb(120, 170, 220);
        visuals.window_corner_radius = egui::CornerRadius::same(6);
        visuals.menu_corner_radius = egui::CornerRadius::same(6);
        for widget in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.open,
        ] {
            widget.corner_radius = egui::CornerRadius::same(4);
        }
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(58, 110, 165);
        visuals.widgets.active.weak_bg_fill = egui::Color32::from_rgb(58, 110, 165);
        style.visuals = visuals;

        style.spacing.item_spacing = egui::vec2(8.0, 4.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        style.spacing.indent = 16.0;

        use egui::{FontFamily, FontId, TextStyle};
        style.text_styles.insert(
            TextStyle::Heading,
            FontId::new(16.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Body,
            FontId::new(13.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(13.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Small,
            FontId::new(11.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Monospace,
            FontId::new(12.5, FontFamily::Monospace),
        );
    });
}
