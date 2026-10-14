//! Results grid over the active tab's [`RunOutcome`].
//!
//! Only visible rows render, via `TableBody::rows` virtualization.

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::app::{AppState, RunOutcome};
use crate::db::CELL_CAP;

const AFFECT_GREEN: egui::Color32 = egui::Color32::from_rgb(115, 180, 115);

pub fn results(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(tab_id) = state.active() else {
        ui.label("No query tab.");
        return;
    };
    let Some(tab) = state.tab(tab_id) else {
        ui.label("No query tab.");
        return;
    };
    match tab.outcome() {
        RunOutcome::Never => {
            ui.label("Type SQL above and press Run (Ctrl+Enter).");
        }
        RunOutcome::Running { .. } => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Running… Cancel in the toolbar to abandon.");
            });
        }
        RunOutcome::Err(msg) => {
            ui.colored_label(ui.visuals().error_fg_color, msg);
            ui.label(
                egui::RichText::new("Check SQL syntax and the connection, then press Run again.")
                    .small()
                    .weak(),
            );
        }
        RunOutcome::Ok(result) => {
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
                return;
            }
            let ms = result.took().as_secs_f64() * 1000.0;
            ui.horizontal(|ui| {
                egui::Frame::group(ui.style())
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .corner_radius(egui::CornerRadius::same(4))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(format!("{} rows · {:.0} ms", result.row_count(), ms))
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
            if result.row_count() == 0 {
                ui.label(egui::RichText::new("No rows returned.").weak());
                return;
            }
            let ncols = result.columns().len();
            let nrows = result.row_count();
            egui::ScrollArea::both().show(ui, |ui| {
                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .columns(Column::auto(), ncols)
                    .header(20.0, |mut header| {
                        for col in result.columns() {
                            header.col(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(col.name()).strong());
                                    ui.label(
                                        egui::RichText::new(col.type_name()).small().weak(),
                                    );
                                });
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(20.0, nrows, |mut row| {
                            let r = row.index();
                            for c in 0..ncols {
                                row.col(|ui| {
                                    let text = result
                                        .cell(r, c)
                                        .map(|v| v.display(CELL_CAP).into_owned())
                                        .unwrap_or_default();
                                    ui.monospace(text);
                                });
                            }
                        });
                    });
            });
        }
    }
}
