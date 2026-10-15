//! Schema tree inside a connection card. Reads state, returns actions.

use eframe::egui;

use crate::app::AppState;
use crate::db::{ConnectionId, TableInfo};

pub enum SchemaAction {
    Refresh(ConnectionId),
    Preview(ConnectionId, String, String),
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
            egui::CollapsingHeader::new(egui::RichText::new("Databases").small().strong())
                .default_open(true)
                .show(ui, |ui| {
                    for db in schema.databases() {
                        egui::CollapsingHeader::new(
                            egui::RichText::new(db.name()).monospace().small(),
                        )
                        .default_open(true)
                        .show(ui, |ui| {
                            egui::CollapsingHeader::new(
                                egui::RichText::new("Schemas").small().strong(),
                            )
                            .default_open(true)
                            .show(ui, |ui| {
                                for schema_node in db.schemas() {
                                    show_schema(ui, conn, schema_node, &mut actions);
                                }
                            });
                        });
                    }
                });
        });
    actions
}

fn show_schema(
    ui: &mut egui::Ui,
    conn: ConnectionId,
    schema_node: &crate::db::SchemaNode,
    actions: &mut Vec<SchemaAction>,
) {
    egui::CollapsingHeader::new(
        egui::RichText::new(schema_node.name()).monospace().small(),
    )
    .default_open(true)
    .show(ui, |ui| {
        if !schema_node.tables().is_empty() {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!("Tables ({})", schema_node.tables().len()))
                    .small()
                    .strong(),
            )
            .default_open(true)
            .show(ui, |ui| {
                for (i, table) in schema_node.tables().iter().enumerate() {
                    if i > 0 {
                        ui.separator();
                    }
                    show_table(ui, conn, schema_node.name(), table, actions);
                }
            });
            ui.add_space(4.0);
        }
        if !schema_node.views().is_empty() {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!("Views ({})", schema_node.views().len()))
                    .small()
                    .strong(),
            )
            .default_open(true)
            .show(ui, |ui| {
                for (i, view) in schema_node.views().iter().enumerate() {
                    if i > 0 {
                        ui.separator();
                    }
                    show_table(ui, conn, schema_node.name(), view, actions);
                }
            });
            ui.add_space(4.0);
        }
        if !schema_node.schema_indexes().is_empty() {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!(
                    "Indexes ({})",
                    schema_node.schema_indexes().len()
                ))
                .small()
                .strong(),
            )
            .default_open(false)
            .show(ui, |ui| {
                for index in schema_node.schema_indexes() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(index.index()).monospace().small());
                        ui.label(
                            egui::RichText::new(format!("({})", index.table()))
                                .small()
                                .weak(),
                        );
                    });
                }
            });
            ui.add_space(4.0);
        }
        if !schema_node.sequences().is_empty() {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!(
                    "Sequences ({})",
                    schema_node.sequences().len()
                ))
                .small()
                .strong(),
            )
            .default_open(false)
            .show(ui, |ui| {
                for seq in schema_node.sequences() {
                    ui.label(egui::RichText::new(seq.name()).monospace().small());
                }
            });
            ui.add_space(4.0);
        }
    });
}

fn show_table(
    ui: &mut egui::Ui,
    conn: ConnectionId,
    schema: &str,
    table: &TableInfo,
    actions: &mut Vec<SchemaAction>,
) {
    let name = table.name().to_owned();
    let schema_owned = schema.to_owned();
    let columns = table.columns();
    let header =
        egui::RichText::new(format!("{name} ({})", columns.len())).monospace().small();
    let resp = egui::CollapsingHeader::new(header).default_open(false).show(ui, |ui| {
        if !table.columns().is_empty() {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!("Columns ({})", table.columns().len()))
                    .small()
                    .strong(),
            )
            .default_open(true)
            .show(ui, |ui| {
                for col in table.columns() {
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
            });
        }
        if !table.constraints().is_empty() {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!("Constraints ({})", table.constraints().len()))
                    .small()
                    .strong(),
            )
            .default_open(true)
            .show(ui, |ui| {
                for constraint in table.constraints() {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(constraint.name()).monospace().small(),
                        );
                        ui.label(
                            egui::RichText::new(constraint.kind().label()).small().weak(),
                        );
                        if !constraint.columns().is_empty() {
                            ui.label(
                                egui::RichText::new(format!(
                                    "({})",
                                    constraint.columns().join(", ")
                                ))
                                .small()
                                .weak(),
                            );
                        }
                        if let Some(expr) = constraint.check_expr() {
                            ui.label(egui::RichText::new(expr).small().weak());
                        }
                    });
                }
            });
        }
        if !table.foreign_keys().is_empty() {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!(
                    "Foreign Keys ({})",
                    table.foreign_keys().len()
                ))
                .small()
                .strong(),
            )
            .default_open(true)
            .show(ui, |ui| {
                for fk in table.foreign_keys() {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(fk.columns().join(", ")).monospace().small(),
                        );
                        ui.label(egui::RichText::new("→").small().weak());
                        if ui
                            .link(egui::RichText::new(fk.ref_table()).monospace().small())
                            .clicked()
                        {
                            actions.push(SchemaAction::Preview(
                                conn,
                                schema_owned.clone(),
                                fk.ref_table().to_owned(),
                            ));
                        }
                        ui.label(
                            egui::RichText::new(format!("({})", fk.ref_columns().join(", ")))
                                .small()
                                .weak(),
                        );
                        if let Some(constraint) = fk.constraint_name() {
                            ui.label(egui::RichText::new(constraint).small().weak());
                        }
                    });
                }
                if table.foreign_keys_truncated() {
                    ui.label(egui::RichText::new("Showing first 50 relations.").small().weak());
                }
            });
        }
        if !table.referenced_by().is_empty() {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!(
                    "References ({})",
                    table.referenced_by().len()
                ))
                .small()
                .strong(),
            )
            .default_open(true)
            .show(ui, |ui| {
                for fk in table.referenced_by() {
                    ui.horizontal(|ui| {
                        if ui
                            .link(egui::RichText::new(fk.ref_table()).monospace().small())
                            .clicked()
                        {
                            actions.push(SchemaAction::Preview(
                                conn,
                                schema_owned.clone(),
                                fk.ref_table().to_owned(),
                            ));
                        }
                        ui.label(
                            egui::RichText::new(format!("({})", fk.ref_columns().join(", ")))
                                .small()
                                .weak(),
                        );
                        ui.label(egui::RichText::new("→").small().weak());
                        ui.label(
                            egui::RichText::new(fk.columns().join(", ")).monospace().small(),
                        );
                        if let Some(constraint) = fk.constraint_name() {
                            ui.label(egui::RichText::new(constraint).small().weak());
                        }
                    });
                }
            });
        }
        if !table.indexes().is_empty() {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!("Indexes ({})", table.indexes().len()))
                    .small()
                    .strong(),
            )
            .default_open(true)
            .show(ui, |ui| {
                for index in table.indexes() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(index.name()).small().strong());
                        if index.is_unique() {
                            ui.label(egui::RichText::new("UNIQUE").small().strong());
                        }
                        if !index.columns().is_empty() {
                            ui.label(
                                egui::RichText::new(format!(
                                    "({})",
                                    index.columns().join(", ")
                                ))
                                .small()
                                .weak(),
                            );
                        }
                    });
                }
                if table.indexes_truncated() {
                    ui.label(egui::RichText::new("Showing first 50 indexes.").small().weak());
                }
            });
        }
        if table.columns().is_empty()
            && table.constraints().is_empty()
            && table.foreign_keys().is_empty()
            && table.referenced_by().is_empty()
            && table.indexes().is_empty()
        {
            ui.label(egui::RichText::new("No columns.").small().weak());
        }
    });
    if resp.header_response.clicked() {
        actions.push(SchemaAction::Preview(conn, schema_owned, name));
    }
}
