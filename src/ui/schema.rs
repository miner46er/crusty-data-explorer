//! Schema tree inside a connection card. Reads state, returns actions.

use eframe::egui;

use crate::app::{
    AppState, NavigatorReveal, RowCountState, fk_incoming_target, fk_reveal_target,
    table_matches_navigator_filter,
};
use crate::db::{ConnectionId, DbKind, TableInfo, TableKind};

use super::ddl;

#[derive(Debug)]
pub enum SchemaAction {
    Refresh(ConnectionId),
    Preview(ConnectionId, String, String),
    /// Reveal one referenced table entry in the navigator without opening it.
    Reveal(ConnectionId, String, String),
    /// Ask the worker for one table's row count.
    Count(ConnectionId, String, String),
    /// Toggle one table bookmark.
    Bookmark(ConnectionId, String, String),
    /// The palette-requested table entry was opened and scrolled to.
    RevealHandled,
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
            let dialect = state
                .connections()
                .iter()
                .find(|c| c.id == conn)
                .map(|c| c.config.kind())
                .unwrap_or(DbKind::Sqlite);
            let reveal = state.navigator_reveal().filter(|reveal| reveal.conn == conn);
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
                                    show_schema(
                                        state,
                                        ui,
                                        conn,
                                        schema_node,
                                        dialect,
                                        &mut actions,
                                        reveal,
                                    );
                                }
                            });
                        });
                    }
                });
        });
    actions
}

fn show_schema(
    state: &AppState,
    ui: &mut egui::Ui,
    conn: ConnectionId,
    schema_node: &crate::db::SchemaNode,
    dialect: DbKind,
    actions: &mut Vec<SchemaAction>,
    reveal: Option<&NavigatorReveal>,
) {
    egui::CollapsingHeader::new(
        egui::RichText::new(schema_node.name()).monospace().small(),
    )
    .default_open(true)
    .show(ui, |ui| {
        let filter = state.navigator_filter().to_owned();
        if !schema_node.tables().is_empty() {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!("Tables ({})", schema_node.tables().len()))
                    .small()
                    .strong(),
            )
            .default_open(true)
            .show(ui, |ui| {
                let mut shown = 0;
                for table in schema_node.tables().iter().filter(|t| {
                    table_matches_navigator_filter(t.name(), filter.trim())
                }) {
                    if shown > 0 {
                        ui.separator();
                    }
                    shown += 1;
                    show_table(
                        state, ui, conn, schema_node.name(), table, dialect, actions, reveal,
                    );
                }
                if shown == 0 {
                    ui.label(egui::RichText::new("No tables match.").small().weak());
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
                let mut shown = 0;
                for view in schema_node.views().iter().filter(|v| {
                    table_matches_navigator_filter(v.name(), filter.trim())
                }) {
                    if shown > 0 {
                        ui.separator();
                    }
                    shown += 1;
                    show_table(
                        state, ui, conn, schema_node.name(), view, dialect, actions, reveal,
                    );
                }
                if shown == 0 {
                    ui.label(egui::RichText::new("No views match.").small().weak());
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
        if !schema_node.triggers().is_empty() {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!("Triggers ({})", schema_node.triggers().len()))
                    .small()
                    .strong(),
            )
            .default_open(false)
            .show(ui, |ui| {
                for trigger in schema_node.triggers() {
                    ui.horizontal(|ui| {
                        let name = ui.label(
                            egui::RichText::new(trigger.name()).monospace().small(),
                        );
                        if let Some(definition) = trigger.definition() {
                            name.on_hover_text(definition);
                        }
                        if let Some(table) = trigger.table() {
                            ui.label(
                                egui::RichText::new(format!("({table})")).small().weak(),
                            );
                        }
                        if let Some(kind) = trigger.kind() {
                            ui.label(egui::RichText::new(kind).small().weak());
                        }
                    });
                }
                if schema_node.triggers_truncated() {
                    ui.label(
                        egui::RichText::new("Showing first 200 triggers.").small().weak(),
                    );
                }
            });
            ui.add_space(4.0);
        }
        if !schema_node.routines().is_empty() {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!("Routines ({})", schema_node.routines().len()))
                    .small()
                    .strong(),
            )
            .default_open(false)
            .show(ui, |ui| {
                for routine in schema_node.routines() {
                    ui.horizontal(|ui| {
                        let name = ui.label(
                            egui::RichText::new(routine.name()).monospace().small(),
                        );
                        if let Some(definition) = routine.definition() {
                            name.on_hover_text(definition);
                        }
                        if let Some(kind) = routine.kind() {
                            ui.label(egui::RichText::new(kind).small().weak());
                        }
                        if let Some(language) = routine.language() {
                            ui.label(egui::RichText::new(language).small().weak());
                        }
                    });
                }
                if schema_node.routines_truncated() {
                    ui.label(
                        egui::RichText::new("Showing first 200 routines.").small().weak(),
                    );
                }
            });
            ui.add_space(4.0);
        }
    });
}

/// The on-demand count row for one table: a button that requests the count,
/// a spinner while it is in flight, the landed number, or the engine error
/// with a retry. Views get nothing because neither estimate engine reports a
/// meaningful one for them.
fn count_affordance(
    state: &AppState,
    ui: &mut egui::Ui,
    conn: ConnectionId,
    schema: &str,
    table: &TableInfo,
    actions: &mut Vec<SchemaAction>,
) {
    if table.kind() != TableKind::Table {
        return;
    }
    let name = table.name();
    ui.horizontal(|ui| match state.row_count(conn, schema, name) {
        None => {
            if ui.small_button("Count rows").clicked() {
                actions.push(SchemaAction::Count(conn, schema.to_owned(), name.to_owned()));
            }
        }
        Some(RowCountState::Pending) => {
            ui.spinner();
            ui.label(egui::RichText::new("Counting…").small().weak());
        }
        Some(RowCountState::Ready(count)) => {
            let text = if count.is_estimate() {
                format!("{} rows (estimate)", count.label())
            } else {
                format!("{} rows", count.label())
            };
            ui.label(egui::RichText::new(text).small());
        }
        Some(RowCountState::Failed(message)) => {
            ui.colored_label(
                ui.visuals().error_fg_color,
                egui::RichText::new(message.clone()).small(),
            );
            if ui.small_button("Retry").clicked() {
                actions.push(SchemaAction::Count(conn, schema.to_owned(), name.to_owned()));
            }
        }
    });
}

fn show_table(
    state: &AppState,
    ui: &mut egui::Ui,
    conn: ConnectionId,
    schema: &str,
    table: &TableInfo,
    dialect: DbKind,
    actions: &mut Vec<SchemaAction>,
    reveal: Option<&NavigatorReveal>,
) {
    let name = table.name().to_owned();
    let schema_owned = schema.to_owned();
    let columns = table.columns();
    let reveal_here = reveal
        .is_some_and(|reveal| reveal.schema == schema && reveal.table == name);
    let header =
        egui::RichText::new(format!("{name} ({})", columns.len())).monospace().small();
    let mut collapsing = egui::CollapsingHeader::new(header).default_open(false);
    if reveal_here {
        collapsing = collapsing.open(Some(true));
    }
    let resp = collapsing.show(ui, |ui| {
        let bookmarked = state.is_bookmarked(conn, schema, &name);
        ui.horizontal(|ui| {
            let label = if bookmarked { "★ Remove bookmark" } else { "☆ Bookmark" };
            if ui.small_button(label).clicked() {
                actions.push(SchemaAction::Bookmark(
                    conn,
                    schema_owned.clone(),
                    name.clone(),
                ));
            }
        });
        count_affordance(state, ui, conn, schema, table, actions);
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
                        let target = state
                            .schema(conn)
                            .and_then(|schema| fk_reveal_target(schema, schema_owned.as_str(), &name, fk));
                        match target {
                            Some((ref_schema, ref_table)) => {
                                if ui
                                    .link(egui::RichText::new(ref_table.clone()).monospace().small())
                                    .on_hover_text("Reveal the referenced table entry")
                                    .clicked()
                                {
                                    actions.push(SchemaAction::Reveal(conn, ref_schema, ref_table));
                                }
                            }
                            None => {
                                ui.label(
                                    egui::RichText::new(fk.ref_table()).monospace().small(),
                                );
                            }
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
                        let target = state
                            .schema(conn)
                            .and_then(|schema| fk_incoming_target(schema, schema_owned.as_str(), fk));
                        match target {
                            Some((child_schema, child_table)) => {
                                if ui
                                    .link(
                                        egui::RichText::new(child_table.clone()).monospace().small(),
                                    )
                                    .on_hover_text("Reveal the referencing table entry")
                                    .clicked()
                                {
                                    actions.push(SchemaAction::Reveal(
                                        conn,
                                        child_schema,
                                        child_table,
                                    ));
                                }
                            }
                            None => {
                                ui.label(
                                    egui::RichText::new(fk.ref_table()).monospace().small(),
                                );
                            }
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
        if table.kind() == TableKind::Table {
            ddl::show_table_ddl(ui, table, dialect);
        }
    });
    if reveal_here {
        resp.header_response.scroll_to_me(Some(egui::Align::Center));
        actions.push(SchemaAction::RevealHandled);
    }
    if resp.header_response.clicked() {
        actions.push(SchemaAction::Preview(conn, schema_owned, name));
    }
}
