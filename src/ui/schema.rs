//! Schema tree inside a connection card. Reads state, returns actions.

use eframe::egui;

use crate::app::AppState;
use crate::db::{ConnectionId, TableInfo, TableKind};

pub enum SchemaAction {
    Refresh(ConnectionId),
    Preview(ConnectionId, String),
}

pub fn connection_schema(
    state: &AppState,
    ui: &mut egui::Ui,
    conn: ConnectionId,
) -> Vec<SchemaAction> {
    let mut actions = Vec::new();
    egui::CollapsingHeader::new(egui::RichText::new("Schema").small().strong())
        .default_open(true)
        .show(ui, |ui| {
            if state.schema_is_loading(conn) {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new("Loading schema…").small().weak());
                });
                return;
            }
            if let Some(err) = state.schema_error(conn) {
                ui.colored_label(ui.visuals().error_fg_color, egui::RichText::new(err).small());
                if ui.small_button("Retry").clicked() {
                    actions.push(SchemaAction::Refresh(conn));
                }
                return;
            }
            let Some(schema) = state.schema(conn) else {
                ui.label(egui::RichText::new("No schema loaded.").small().weak());
                if ui.small_button("Load").clicked() {
                    actions.push(SchemaAction::Refresh(conn));
                }
                return;
            };
            if schema.is_empty() {
                ui.label(egui::RichText::new("No tables or views found.").small().weak());
                return;
            }
            if schema.tables_truncated() || schema.columns_truncated() {
                ui.label(
                    egui::RichText::new("Truncated: showing first 500 tables, 200 columns each.")
                        .small()
                        .weak(),
                );
            }
            let tables: Vec<_> =
                schema.tables().iter().filter(|t| t.kind() == TableKind::Table).collect();
            let views: Vec<_> =
                schema.tables().iter().filter(|t| t.kind() == TableKind::View).collect();
            if !tables.is_empty() {
                ui.label(egui::RichText::new("TABLES").small().weak());
                ui.add_space(4.0);
                for (i, table) in tables.iter().enumerate() {
                    if i > 0 {
                        ui.separator();
                    }
                    show_table(ui, conn, table, &mut actions);
                }
                ui.add_space(4.0);
            }
            if !views.is_empty() {
                ui.label(egui::RichText::new("VIEWS").small().weak());
                ui.add_space(4.0);
                for (i, view) in views.iter().enumerate() {
                    if i > 0 {
                        ui.separator();
                    }
                    show_table(ui, conn, view, &mut actions);
                }
                ui.add_space(4.0);
            }
        });
    actions
}

fn show_table(
    ui: &mut egui::Ui,
    conn: ConnectionId,
    table: &TableInfo,
    actions: &mut Vec<SchemaAction>,
) {
    let name = table.name();
    let columns = table.columns();
    let header =
        egui::RichText::new(format!("{name} ({})", columns.len())).monospace().small();
    let resp = egui::CollapsingHeader::new(header).default_open(false).show(ui, |ui| {
        for col in columns {
            ui.horizontal(|ui| {
                if col.is_pk() {
                    ui.label(egui::RichText::new("🔑").small());
                }
                ui.label(egui::RichText::new(col.name()).monospace().small());
                if !col.type_name().is_empty() {
                    ui.label(egui::RichText::new(col.type_name()).small().weak());
                }
            });
        }
        if columns.is_empty() {
            ui.label(egui::RichText::new("No columns.").small().weak());
        }
        if !table.foreign_keys().is_empty() {
            ui.add_space(4.0);
            ui.label(egui::RichText::new("Relations").small().weak());
            for fk in table.foreign_keys() {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(fk.columns().join(", ")).monospace().small(),
                    );
                    ui.label(egui::RichText::new("→").small().weak());
                    if ui.link(egui::RichText::new(fk.ref_table()).monospace().small()).clicked()
                    {
                        actions.push(SchemaAction::Preview(
                            conn,
                            fk.ref_table().to_owned(),
                        ));
                    }
                    ui.label(
                        egui::RichText::new(format!("({})", fk.ref_columns().join(", ")))
                            .small()
                            .weak(),
                    );
                    if let Some(constraint) = fk.constraint_name() {
                        ui.label(
                            egui::RichText::new(constraint).small().weak(),
                        );
                    }
                });
            }
            if table.foreign_keys_truncated() {
                ui.label(egui::RichText::new("Showing first 50 relations.").small().weak());
            }
        }
        if !table.indexes().is_empty() {
            ui.add_space(4.0);
            ui.label(egui::RichText::new("Indexes").small().weak());
            for index in table.indexes() {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(index.name()).small().strong());
                    if index.is_unique() {
                        ui.label(egui::RichText::new("UNIQUE").small().strong());
                    }
                    if !index.columns().is_empty() {
                        ui.label(
                            egui::RichText::new(format!("({})", index.columns().join(", ")))
                                .small()
                                .weak(),
                        );
                    }
                });
            }
            if table.indexes_truncated() {
                ui.label(egui::RichText::new("Showing first 50 indexes.").small().weak());
            }
        }
    });
    if resp.header_response.clicked() {
        actions.push(SchemaAction::Preview(conn, name.to_owned()));
    }
}
