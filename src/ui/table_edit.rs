//! Visual table creator and structure editor.
//!
//! The form model lives in [`super::ddl`]: table name, column rows with name
//! plus type plus nullability plus default, and index rows. This module is
//! the egui shell around it: one window with Copy to SQL preview before
//! apply. Apply sends through the existing batch path with the read-only and
//! prod guards, and the Ok landing refreshes the schema so the table appears
//! in the navigator. Changed definitions on surviving columns stay
//! hand-edited in the preview; the alter diff covers renames, added and
//! dropped columns, and added and dropped indexes.

use eframe::egui;

use crate::app::AppState;
use crate::db::ConnectionId;

use super::{ModalKey, autofocus, park_focus};

/// The status-bar entry's label, measured by the status bar before it draws:
/// the entry and the measurement read one literal.
pub(crate) const STATUS_LABEL: &str = "+ Table";

/// The status-bar entry: opens a blank create form on the default
/// connection. Disabled without one.
pub fn status_entry(state: &mut AppState, ui: &mut egui::Ui) {
    let conn = state.default_connection();
    let clicked = ui
        .add_enabled(conn.is_some(), egui::Button::new(STATUS_LABEL).small())
        .on_hover_text("Create a table with the visual editor")
        .clicked();
    if clicked
        && let Some(conn) = conn
    {
        state.open_table_create(conn);
    }
}

/// The prod-gated table DDL confirmation: names the connection and lists the
/// exact held statements. Non-prod applies never arm this modal.
pub fn table_prod_confirm_window(
    state: &mut AppState,
    ui: &mut egui::Ui,
    key: Option<ModalKey>,
) {
    let Some(pending) = state.pending_table_ddl().cloned() else { return };
    let Some(name) =
        state.connections().iter().find(|c| c.id == pending.conn).map(|c| c.name.clone())
    else {
        state.cancel_table_ddl();
        return;
    };
    let mut confirm = false;
    let mut cancel = false;
    egui::Window::new("Apply table DDL on prod?").show(ui.ctx(), |ui| {
        ui.label(format!(
            "{} statement{} on {name}. Prod applies need one more click.",
            pending.statements.len(),
            if pending.statements.len() == 1 { "" } else { "s" },
        ));
        egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
            for statement in &pending.statements {
                ui.monospace(statement);
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() {
                confirm = true;
            }
            let cancel_button = ui.button("Cancel");
            autofocus(ui, "table-ddl-cancel", &cancel_button);
            if cancel_button.clicked() {
                cancel = true;
            }
            park_focus(ui);
        });
    });
    if confirm {
        state.confirm_table_ddl();
    } else if cancel || key == Some(ModalKey::Cancel) {
        state.cancel_table_ddl();
    }
}

fn alter_input_id(which: &str) -> egui::Id {
    egui::Id::new(format!("crusty_table_alter_input#{which}"))
}

fn alter_input(ctx: &egui::Context, which: &str) -> String {
    ctx.data_mut(|d| d.get_temp(alter_input_id(which))).unwrap_or_default()
}

fn set_alter_input(ctx: &egui::Context, which: &str, text: String) {
    ctx.data_mut(|d| d.insert_temp(alter_input_id(which), text));
}

/// Whether the loaded schema behind `conn` carries `table` under `schema`.
fn schema_has_table(state: &AppState, conn: ConnectionId, schema: &str, table: &str) -> bool {
    state
        .schema(conn)
        .map(|info| {
            info.databases().iter().flat_map(|db| db.schemas()).any(|node| {
                node.name().eq_ignore_ascii_case(schema)
                    && node.tables().iter().any(|t| t.name().eq_ignore_ascii_case(table))
            })
        })
        .unwrap_or(false)
}

/// The editor window: form rows, validation errors, SQL preview with Copy,
/// and Apply through the batch path. Nothing sends without Apply.
pub fn table_editor_window(
    state: &mut AppState,
    ui: &mut egui::Ui,
    key: Option<ModalKey>,
) {
    let (conn, conn_name, backend, read_only, prod, is_alter, editing_title) = match state
        .table_editor()
    {
        Some(editor) => {
            let (name, backend, read_only, prod) = state
                .connections()
                .iter()
                .find(|c| c.id == editor.conn)
                .map(|c| {
                    (
                        c.name.clone(),
                        c.config.kind().label().to_owned(),
                        c.config.is_read_only(),
                        c.env == crate::app::Environment::Prod,
                    )
                })
                .unwrap_or_else(|| ("deleted".to_owned(), String::new(), true, false));
            let is_alter = editor.original.is_some();
            let title = if is_alter {
                format!("Edit table {}.{}", editor.form.schema, editor.form.table)
            } else {
                "New table".to_owned()
            };
            (editor.conn, name, backend, read_only, prod, is_alter, title)
        }
        None => return,
    };
    let dismissed_popup =
        key == Some(ModalKey::Cancel) && egui::Popup::is_any_open(ui.ctx());
    if dismissed_popup {
        egui::Popup::close_all(ui.ctx());
    }
    let mut close = false;
    let mut apply = false;
    let mut load_alter: Option<(String, String)> = None;
    let mut back_to_new = false;
    egui::Window::new(editing_title).show(ui.ctx(), |ui| {
        ui.label(egui::RichText::new(format!("{conn_name} · {backend}")).small().weak());
        if is_alter {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Editing the loaded table.").small().weak());
                if ui.small_button("Back to new table").clicked() {
                    back_to_new = true;
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Edit an existing table:").small().weak());
                let mut schema = alter_input(ui.ctx(), "schema");
                let mut table = alter_input(ui.ctx(), "table");
                let schema_changed = ui
                    .add(
                        egui::TextEdit::singleline(&mut schema)
                            .hint_text("schema")
                            .desired_width(110.0),
                    )
                    .changed();
                let table_changed = ui
                    .add(
                        egui::TextEdit::singleline(&mut table)
                            .hint_text("table")
                            .desired_width(130.0),
                    )
                    .changed();
                if schema_changed {
                    set_alter_input(ui.ctx(), "schema", schema.clone());
                }
                if table_changed {
                    set_alter_input(ui.ctx(), "table", table.clone());
                }
                let found = !schema.trim().is_empty()
                    && !table.trim().is_empty()
                    && schema_has_table(state, conn, schema.trim(), table.trim());
                if ui
                    .add_enabled(found, egui::Button::new("Load").small())
                    .on_hover_text("Load the table into the form")
                    .clicked()
                {
                    load_alter = Some((schema.trim().to_owned(), table.trim().to_owned()));
                }
                if !found && (!schema.trim().is_empty() || !table.trim().is_empty()) {
                    ui.label(
                        egui::RichText::new("not in the loaded schema").small().weak(),
                    );
                }
            });
        }
        {
            let Some(editor) = state.table_editor_mut() else { return };
            ui.horizontal(|ui| {
                ui.label("Schema");
                ui.add(
                    egui::TextEdit::singleline(&mut editor.form.schema)
                        .hint_text("blank for default")
                        .desired_width(130.0),
                );
                ui.label("Table");
                ui.add(
                    egui::TextEdit::singleline(&mut editor.form.table)
                        .hint_text("table name")
                        .desired_width(170.0),
                );
            });
            ui.add_space(4.0);
            ui.label(egui::RichText::new("Columns").small().strong());
            let mut remove_col: Option<usize> = None;
            for (index, col) in editor.form.columns.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut col.name)
                            .hint_text("name")
                            .desired_width(110.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut col.data_type)
                            .hint_text("type")
                            .desired_width(90.0),
                    );
                    ui.checkbox(&mut col.nullable, "NULL");
                    ui.checkbox(&mut col.pk, "PK");
                    ui.add(
                        egui::TextEdit::singleline(&mut col.default)
                            .hint_text("default SQL")
                            .desired_width(90.0),
                    );
                    if ui.small_button("×").on_hover_text("Remove column").clicked() {
                        remove_col = Some(index);
                    }
                });
            }
            if let Some(index) = remove_col {
                editor.form.columns.remove(index);
            }
            if ui.small_button("+ Column").clicked() {
                editor.form.columns.push(crate::ui::ddl::TableColumnDraft {
                    name: String::new(),
                    data_type: "TEXT".to_owned(),
                    nullable: true,
                    default: String::new(),
                    pk: false,
                });
            }
            ui.add_space(4.0);
            ui.label(egui::RichText::new("Indexes").small().strong());
            let mut remove_idx: Option<usize> = None;
            for (index, idx) in editor.form.indexes.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut idx.name)
                            .hint_text("index name")
                            .desired_width(130.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut idx.columns)
                            .hint_text("columns, comma-separated")
                            .desired_width(170.0),
                    );
                    ui.checkbox(&mut idx.unique, "UNIQUE");
                    if ui.small_button("×").on_hover_text("Remove index").clicked() {
                        remove_idx = Some(index);
                    }
                });
            }
            if let Some(index) = remove_idx {
                editor.form.indexes.remove(index);
            }
            if ui.small_button("+ Index").clicked() {
                editor.form.indexes.push(crate::ui::ddl::TableIndexDraft {
                    name: String::new(),
                    columns: String::new(),
                    unique: false,
                });
            }
        }
        let errors = state.table_ddl_errors();
        if !errors.is_empty() {
            for error in &errors {
                ui.colored_label(ui.visuals().error_fg_color, egui::RichText::new(error).small());
            }
        }
        let statements = state.table_ddl_statements().unwrap_or_default();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("SQL preview").small().strong());
            if ui
                .small_button("Copy SQL")
                .on_hover_text("Copy the preview statements")
                .clicked()
            {
                ui.ctx().copy_text(statements.join("\n"));
            }
        });
        if statements.is_empty() {
            ui.label(
                egui::RichText::new("The form matches the table: nothing to apply.")
                    .small()
                    .weak(),
            );
        } else {
            egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                for statement in &statements {
                    ui.monospace(statement);
                }
            });
        }
        if read_only {
            ui.label(
                egui::RichText::new("Read-only connection will refuse this write.")
                    .small()
                    .weak(),
            );
        }
        if prod {
            ui.label(
                egui::RichText::new("Prod connection asks for confirmation before applying.")
                    .small()
                    .weak(),
            );
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    errors.is_empty() && !statements.is_empty(),
                    egui::Button::new("Apply"),
                )
                .on_hover_text("Send the preview through the batch path")
                .clicked()
            {
                apply = true;
            }
            if ui.button("Close").clicked() {
                close = true;
            }
        });
    });
    if let Some((schema, table)) = load_alter {
        state.open_table_alter(conn, &schema, &table);
        set_alter_input(ui.ctx(), "schema", String::new());
        set_alter_input(ui.ctx(), "table", String::new());
    } else if back_to_new {
        state.open_table_create(conn);
    } else if !dismissed_popup && (close || key == Some(ModalKey::Cancel)) {
        state.close_table_editor();
    } else if apply {
        let tab = state
            .active()
            .filter(|id| state.tab(*id).and_then(|t| t.conn) == Some(conn))
            .or_else(|| state.tabs().iter().find(|t| t.conn == Some(conn)).map(|t| t.id))
            .unwrap_or_else(|| state.new_tab(conn));
        state.activate(tab);
        state.apply_table_ddl(tab);
        let sent = matches!(state.tab(tab).map(|t| t.outcome()), Some(crate::app::RunOutcome::Running { .. }));
        if sent {
            state.close_table_editor();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ddl::{
        TableColumnDraft, TableForm, TableIndexDraft, alter_table_statements,
        create_table_statements, validate_table_form,
    };
    use crate::db::{ConnectionConfig, DbKind, PostgresSslMode, Session};
    #[cfg(feature = "mysql")]
    use crate::db::MysqlSslMode;

    fn probe_form(table: &str) -> TableForm {
        TableForm {
            schema: String::new(),
            table: table.to_owned(),
            columns: vec![
                TableColumnDraft {
                    name: "id".to_owned(),
                    data_type: "INTEGER".to_owned(),
                    nullable: false,
                    default: String::new(),
                    pk: true,
                },
                TableColumnDraft {
                    name: "note".to_owned(),
                    data_type: "TEXT".to_owned(),
                    nullable: true,
                    default: String::new(),
                    pk: false,
                },
            ],
            indexes: vec![TableIndexDraft {
                name: "idx_probe_note".to_owned(),
                columns: "note".to_owned(),
                unique: false,
            }],
        }
    }

    fn find_probe_table(schema: &crate::db::SchemaInfo, name: &str) -> crate::db::TableInfo {
        schema
            .databases()
            .iter()
            .flat_map(|db| db.schemas())
            .flat_map(|node| node.tables().iter())
            .find(|t| t.name() == name)
            .expect("probe table in the schema")
            .clone()
    }

    #[cfg(feature = "postgres")]
    fn pg_live_config() -> ConnectionConfig {
        ConnectionConfig::Postgres { read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        }
    }

    #[cfg(feature = "mysql")]
    fn mysql_live_config() -> ConnectionConfig {
        ConnectionConfig::Mysql { read_only: false,
            host: std::env::var("MYSQLHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("MYSQLPORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3306),
            database: std::env::var("MYSQLDATABASE").unwrap_or_else(|_| "cde_test".to_owned()),
            user: std::env::var("MYSQLUSER").unwrap_or_else(|_| "root".to_owned()),
            ssl_mode: MysqlSslMode::default(),
            ssh: None,
        }
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_table_edit_creates_and_alters_a_scratch_table() {
        let config = pg_live_config();
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        session.query("DROP TABLE IF EXISTS cde_table_edit_probe").expect("drop probe");
        let form = probe_form("cde_table_edit_probe");
        assert!(validate_table_form(&form).is_empty());
        let created = create_table_statements(&form, DbKind::Postgres);
        assert_eq!(created.len(), 2);
        for statement in &created {
            session.query(statement).unwrap_or_else(|e| panic!("{statement}: {e}"));
        }
        session
            .query("INSERT INTO \"cde_table_edit_probe\" (\"id\", \"note\") VALUES (1, 'ada')")
            .expect("seed probe");
        let info = find_probe_table(&session.schema().expect("schema loads"), "cde_table_edit_probe");
        assert!(info.columns().iter().any(|c| c.name() == "note"));
        let mut edited = TableForm::from_table("public", &info);
        edited.columns.push(TableColumnDraft {
            name: "email".to_owned(),
            data_type: "TEXT".to_owned(),
            nullable: true,
            default: String::new(),
            pk: false,
        });
        edited.indexes.push(TableIndexDraft {
            name: "idx_probe_email".to_owned(),
            columns: "email".to_owned(),
            unique: false,
        });
        assert!(validate_table_form(&edited).is_empty());
        let altered = alter_table_statements(&info, &edited, DbKind::Postgres);
        assert_eq!(
            altered,
            vec![
                "ALTER TABLE \"public\".\"cde_table_edit_probe\" ADD COLUMN \"email\" TEXT;"
                    .to_owned(),
                "CREATE INDEX \"idx_probe_email\" ON \"public\".\"cde_table_edit_probe\" (\"email\");"
                    .to_owned(),
            ]
        );
        for statement in &altered {
            session.query(statement).unwrap_or_else(|e| panic!("{statement}: {e}"));
        }
        let result = session
            .query("SELECT \"id\", \"note\", \"email\" FROM \"cde_table_edit_probe\" ORDER BY \"id\"")
            .expect("reselect probe");
        assert_eq!(result.row_count(), 1);
        session.query("DROP TABLE IF EXISTS cde_table_edit_probe").expect("cleanup probe");
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_table_edit_creates_and_alters_a_scratch_table() {
        let config = mysql_live_config();
        let password = std::env::var("MYSQLPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        session.query("DROP TABLE IF EXISTS cde_table_edit_probe").expect("drop probe");
        let form = TableForm {
            schema: String::new(),
            table: "cde_table_edit_probe".to_owned(),
            columns: vec![
                TableColumnDraft {
                    name: "id".to_owned(),
                    data_type: "INT".to_owned(),
                    nullable: false,
                    default: String::new(),
                    pk: true,
                },
                TableColumnDraft {
                    name: "note".to_owned(),
                    data_type: "VARCHAR(64)".to_owned(),
                    nullable: true,
                    default: String::new(),
                    pk: false,
                },
            ],
            indexes: vec![TableIndexDraft {
                name: "idx_probe_note".to_owned(),
                columns: "note".to_owned(),
                unique: false,
            }],
        };
        assert!(validate_table_form(&form).is_empty());
        let created = create_table_statements(&form, DbKind::Mysql);
        assert_eq!(
            created,
            vec![
                "CREATE TABLE `cde_table_edit_probe` (\n  `id` INT NOT NULL,\n  `note` VARCHAR(64),\n  PRIMARY KEY (`id`)\n);"
                    .to_owned(),
                "CREATE INDEX `idx_probe_note` ON `cde_table_edit_probe` (`note`);".to_owned(),
            ]
        );
        for statement in &created {
            session.query(statement).unwrap_or_else(|e| panic!("{statement}: {e}"));
        }
        session
            .query("INSERT INTO `cde_table_edit_probe` (`id`, `note`) VALUES (1, 'ada')")
            .expect("seed probe");
        let info = find_probe_table(&session.schema().expect("schema loads"), "cde_table_edit_probe");
        assert!(info.columns().iter().any(|c| c.name() == "note"));
        let mut edited = TableForm::from_table("", &info);
        edited.columns.push(TableColumnDraft {
            name: "email".to_owned(),
            data_type: "VARCHAR(64)".to_owned(),
            nullable: true,
            default: String::new(),
            pk: false,
        });
        assert!(validate_table_form(&edited).is_empty());
        let altered: Vec<String> = alter_table_statements(&info, &edited, DbKind::Mysql)
            .into_iter()
            .filter(|s| s.contains("ADD COLUMN"))
            .collect();
        assert_eq!(
            altered,
            vec!["ALTER TABLE `cde_table_edit_probe` ADD COLUMN `email` VARCHAR(64);".to_owned()]
        );
        for statement in &altered {
            session.query(statement).unwrap_or_else(|e| panic!("{statement}: {e}"));
        }
        let result = session
            .query("SELECT `id`, `note`, `email` FROM `cde_table_edit_probe` ORDER BY `id`")
            .expect("reselect probe");
        assert_eq!(result.row_count(), 1);
        session.query("DROP TABLE IF EXISTS cde_table_edit_probe").expect("cleanup probe");
    }
}
