//! Results grid over the active tab's [`RunOutcome`].
//!
//! Only visible rows render, via `TableBody::rows` virtualization.

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::app::{AppState, RunOutcome};
use crate::db::CELL_CAP;

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
        }
        RunOutcome::Ok(result) => {
            if result.columns().is_empty() {
                match result.affected() {
                    Some(n) => {
                        ui.label(format!("Done, {n} rows affected."));
                    }
                    None => {
                        ui.label("Done.");
                    }
                }
                return;
            }
            let mut summary = format!("{} rows", result.row_count());
            if result.is_truncated() {
                summary += " (truncated at 10,000)";
            }
            ui.label(summary);
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
                                ui.strong(col.name());
                                ui.small(col.type_name());
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(18.0, nrows, |mut row| {
                            let r = row.index();
                            for c in 0..ncols {
                                row.col(|ui| {
                                    let text = result
                                        .cell(r, c)
                                        .map(|v| v.display(CELL_CAP).into_owned())
                                        .unwrap_or_default();
                                    ui.label(text);
                                });
                            }
                        });
                    });
            });
        }
    }
}
