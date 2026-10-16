//! Per-table DDL text and statement templates.
//!
//! The builders below are pure string functions over schema slices. The UI
//! renders one `show_table_ddl` section per table from `schema.rs`.

use eframe::egui;

use crate::db::{
    ColumnInfo, ConstraintInfo, ConstraintKind, DbKind, IndexInfo, TableInfo,
};

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn quoted_list(names: &[String]) -> String {
    names.iter().map(|n| quoted(n)).collect::<Vec<_>>().join(", ")
}

fn parenthesized(names: &[String]) -> String {
    if names.is_empty() {
        String::new()
    } else {
        format!(" ({})", quoted_list(names))
    }
}

fn column_def(col: &ColumnInfo) -> String {
    let mut def = quoted(col.name());
    if !col.type_name().is_empty() {
        def.push(' ');
        def.push_str(col.type_name());
    }
    if !col.nullable() {
        def.push_str(" NOT NULL");
    }
    def
}

fn foreign_line(constraint: &ConstraintInfo) -> String {
    let mut line = format!(
        "CONSTRAINT {} FOREIGN KEY ({})",
        quoted(constraint.name()),
        quoted_list(constraint.columns())
    );
    if let Some(ref_table) = constraint.ref_table() {
        line.push_str(&format!(" REFERENCES {}", quoted(ref_table)));
        if !constraint.ref_columns().is_empty() {
            line.push_str(&format!(" ({})", quoted_list(constraint.ref_columns())));
        }
    }
    line
}

fn build_create_table(
    name: &str,
    columns: &[ColumnInfo],
    constraints: &[ConstraintInfo],
    indexes: &[IndexInfo],
) -> String {
    let mut body: Vec<String> = columns.iter().map(column_def).collect();
    let mut pk_covered = false;
    for constraint in constraints.iter().filter(|c| c.kind() == ConstraintKind::Pk) {
        body.push(format!(
            "CONSTRAINT {} PRIMARY KEY{}",
            quoted(constraint.name()),
            parenthesized(constraint.columns())
        ));
        pk_covered = true;
    }
    if !pk_covered {
        let mut pk: Vec<(u32, &str)> = columns
            .iter()
            .filter_map(|c| c.pk_position().map(|pos| (pos, c.name())))
            .collect();
        pk.sort_by_key(|(pos, _)| *pos);
        if !pk.is_empty() {
            let names: Vec<String> = pk.iter().map(|(_, n)| quoted(n)).collect();
            body.push(format!("PRIMARY KEY ({})", names.join(", ")));
        }
    }
    for constraint in constraints.iter().filter(|c| c.kind() == ConstraintKind::Unique) {
        body.push(format!(
            "CONSTRAINT {} UNIQUE{}",
            quoted(constraint.name()),
            parenthesized(constraint.columns())
        ));
    }
    for constraint in constraints.iter().filter(|c| c.kind() == ConstraintKind::Check) {
        match constraint.check_expr() {
            Some(expr) => body.push(format!(
                "CONSTRAINT {} CHECK ({})",
                quoted(constraint.name()),
                expr
            )),
            None => body.push(format!("CONSTRAINT {} CHECK", quoted(constraint.name()))),
        }
    }
    for constraint in constraints.iter().filter(|c| c.kind() == ConstraintKind::Foreign) {
        body.push(foreign_line(constraint));
    }
    let mut out = if body.is_empty() {
        format!("CREATE TABLE {} ();", quoted(name))
    } else {
        format!("CREATE TABLE {} (\n  {}\n);", quoted(name), body.join(",\n  "))
    };
    for index in indexes {
        let backed = constraints.iter().any(|c| {
            (c.kind() == ConstraintKind::Pk || c.kind() == ConstraintKind::Unique)
                && c.name() == index.name()
                && c.columns() == index.columns()
        });
        if backed {
            continue;
        }
        let unique = if index.unique() { "UNIQUE " } else { "" };
        if index.columns().is_empty() {
            out.push_str(&format!(
                "\nCREATE {unique}INDEX {} ON {};",
                quoted(index.name()),
                quoted(name)
            ));
        } else {
            out.push_str(&format!(
                "\nCREATE {unique}INDEX {} ON {} ({});",
                quoted(index.name()),
                quoted(name),
                quoted_list(index.columns())
            ));
        }
    }
    out
}

pub fn table_ddl_sqlite(
    name: &str,
    columns: &[ColumnInfo],
    constraints: &[ConstraintInfo],
    indexes: &[IndexInfo],
) -> String {
    build_create_table(name, columns, constraints, indexes)
}

pub fn table_ddl_postgres(
    name: &str,
    columns: &[ColumnInfo],
    constraints: &[ConstraintInfo],
    indexes: &[IndexInfo],
) -> String {
    build_create_table(name, columns, constraints, indexes)
}

pub fn gen_select(table: &str, columns: &[String]) -> String {
    if columns.is_empty() {
        format!("SELECT * FROM {};", quoted(table))
    } else {
        format!("SELECT {} FROM {};", quoted_list(columns), quoted(table))
    }
}

pub fn gen_insert(table: &str, columns: &[String]) -> String {
    if columns.is_empty() {
        format!("INSERT INTO {} DEFAULT VALUES;", quoted(table))
    } else {
        let marks = vec!["?"; columns.len()].join(", ");
        format!(
            "INSERT INTO {} ({}) VALUES ({});",
            quoted(table),
            quoted_list(columns),
            marks
        )
    }
}

pub fn gen_update(table: &str, columns: &[String]) -> String {
    if columns.is_empty() {
        format!("UPDATE {};", quoted(table))
    } else {
        let sets: Vec<String> =
            columns.iter().map(|c| format!("{} = ?", quoted(c))).collect();
        format!("UPDATE {} SET {};", quoted(table), sets.join(", "))
    }
}

pub fn gen_delete(table: &str, _columns: &[String]) -> String {
    format!("DELETE FROM {};", quoted(table))
}

/// One column row in the visual table form. All text fields edit in place,
/// so blank means not yet typed rather than a typed empty value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableColumnDraft {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: String,
    pub pk: bool,
}

/// One index row in the visual table form. `columns` is the comma-separated
/// text the form edits; the builders split it into quoted names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableIndexDraft {
    pub name: String,
    pub columns: String,
    pub unique: bool,
}

/// The table form model: the table name plus its column and index rows.
/// `schema` is blank for an unqualified table, else the navigator schema
/// the table lives under. The pure builders below send this shape to SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableForm {
    pub schema: String,
    pub table: String,
    pub columns: Vec<TableColumnDraft>,
    pub indexes: Vec<TableIndexDraft>,
}

impl TableForm {
    pub fn blank() -> Self {
        Self {
            schema: String::new(),
            table: String::new(),
            columns: vec![TableColumnDraft {
                name: String::new(),
                data_type: "TEXT".to_owned(),
                nullable: true,
                default: String::new(),
                pk: false,
            }],
            indexes: Vec::new(),
        }
    }
    pub fn from_table(schema: &str, table: &TableInfo) -> Self {
        Self {
            schema: schema.to_owned(),
            table: table.name().to_owned(),
            columns: table
                .columns()
                .iter()
                .map(|c| TableColumnDraft {
                    name: c.name().to_owned(),
                    data_type: c.type_name().to_owned(),
                    nullable: c.nullable(),
                    default: String::new(),
                    pk: c.pk_position().is_some(),
                })
                .collect(),
            indexes: table
                .indexes()
                .iter()
                .map(|i| TableIndexDraft {
                    name: i.name().to_owned(),
                    columns: i.columns().join(", "),
                    unique: i.unique(),
                })
                .collect(),
        }
    }
}

/// The form errors as display lines. Empty means the form can build SQL.
pub fn validate_table_form(form: &TableForm) -> Vec<String> {
    let mut errors = Vec::new();
    if form.table.trim().is_empty() {
        errors.push("table name is empty".to_owned());
    }
    if form.columns.is_empty() {
        errors.push("add at least one column".to_owned());
    }
    let mut seen_cols: Vec<String> = Vec::new();
    for (index, col) in form.columns.iter().enumerate() {
        let name = col.name.trim();
        if name.is_empty() {
            errors.push(format!("column {} has no name", index + 1));
            continue;
        }
        let folded = name.to_lowercase();
        if seen_cols.contains(&folded) {
            errors.push(format!("duplicate column \"{}\"", name));
        } else {
            seen_cols.push(folded);
        }
        if col.data_type.trim().is_empty() {
            errors.push(format!("column \"{name}\" needs a type"));
        }
    }
    let mut seen_idx: Vec<String> = Vec::new();
    for (index, idx) in form.indexes.iter().enumerate() {
        let name = idx.name.trim();
        if name.is_empty() {
            errors.push(format!("index {} has no name", index + 1));
            continue;
        }
        let folded = name.to_lowercase();
        if seen_idx.contains(&folded) {
            errors.push(format!("duplicate index \"{name}\""));
        } else {
            seen_idx.push(folded);
        }
        let cols = split_index_columns(&idx.columns);
        if cols.is_empty() {
            errors.push(format!("index \"{name}\" needs at least one column"));
            continue;
        }
        for col in cols {
            if !form.columns.iter().any(|c| c.name.trim().eq_ignore_ascii_case(&col)) {
                errors.push(format!("index \"{name}\" names unknown column \"{col}\""));
            }
        }
    }
    errors
}

/// One identifier per dialect: double quotes everywhere but MySQL, which
/// needs backticks unless the server runs ANSI_QUOTES. Only the new table
/// builders use this; the shared DDL pane keeps its quoting.
fn qi(name: &str, dialect: DbKind) -> String {
    match dialect {
        DbKind::Mysql => format!("`{}`", name.replace('`', "``")),
        _ => quoted(name),
    }
}

fn qi_list(names: &[String], dialect: DbKind) -> String {
    names.iter().map(|n| qi(n, dialect)).collect::<Vec<_>>().join(", ")
}

fn qtable(schema: &str, table: &str, dialect: DbKind) -> String {
    if schema.trim().is_empty() {
        qi(table, dialect)
    } else {
        format!("{}.{}", qi(schema.trim(), dialect), qi(table, dialect))
    }
}

fn split_index_columns(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn column_definition(col: &TableColumnDraft, dialect: DbKind) -> String {
    let mut def = qi(col.name.trim(), dialect);
    if !col.data_type.trim().is_empty() {
        def.push(' ');
        def.push_str(col.data_type.trim());
    }
    if !col.nullable {
        def.push_str(" NOT NULL");
    }
    if !col.default.trim().is_empty() {
        def.push_str(" DEFAULT ");
        def.push_str(col.default.trim());
    }
    def
}

fn create_index_statement(
    schema: &str,
    table: &str,
    idx: &TableIndexDraft,
    dialect: DbKind,
) -> Option<String> {
    let cols = split_index_columns(&idx.columns);
    if idx.name.trim().is_empty() || cols.is_empty() {
        return None;
    }
    let unique = if idx.unique { "UNIQUE " } else { "" };
    Some(format!(
        "CREATE {unique}INDEX {} ON {} ({});",
        qi(idx.name.trim(), dialect),
        qtable(schema, table, dialect),
        qi_list(&cols, dialect)
    ))
}

/// The create flow as SQL: one CREATE TABLE plus one CREATE INDEX per index
/// row, in form order. Callers validate first; invalid rows render
/// best-effort and empty index rows are skipped.
pub fn create_table_statements(form: &TableForm, dialect: DbKind) -> Vec<String> {
    let table = form.table.trim();
    let schema = form.schema.trim();
    let mut body: Vec<String> =
        form.columns.iter().map(|c| column_definition(c, dialect)).collect();
    let pk: Vec<String> = form
        .columns
        .iter()
        .filter(|c| c.pk && !c.name.trim().is_empty())
        .map(|c| qi(c.name.trim(), dialect))
        .collect();
    if !pk.is_empty() {
        body.push(format!("PRIMARY KEY ({})", pk.join(", ")));
    }
    let mut out = Vec::new();
    if body.is_empty() {
        out.push(format!("CREATE TABLE {} ();", qtable(schema, table, dialect)));
    } else {
        out.push(format!(
            "CREATE TABLE {} (\n  {}\n);",
            qtable(schema, table, dialect),
            body.join(",\n  ")
        ));
    }
    for idx in &form.indexes {
        if let Some(statement) = create_index_statement(schema, table, idx, dialect) {
            out.push(statement);
        }
    }
    out
}

/// The alter flow as SQL from the loaded table to the edited form: a rename
/// first when the name moved, ADD COLUMN per added column, DROP COLUMN per
/// removed one, CREATE INDEX per added index, DROP INDEX per removed one.
/// Changed definitions on surviving columns stay hand-edited in the preview.
pub fn alter_table_statements(
    original: &TableInfo,
    form: &TableForm,
    dialect: DbKind,
) -> Vec<String> {
    let mut out = Vec::new();
    let new_table = form.table.trim();
    let schema = form.schema.trim();
    if !new_table.is_empty() && new_table != original.name() {
        out.push(format!(
            "ALTER TABLE {} RENAME TO {};",
            qtable(schema, original.name(), dialect),
            qi(new_table, dialect)
        ));
    }
    let target = if new_table.is_empty() { original.name() } else { new_table };
    for col in &form.columns {
        let name = col.name.trim();
        if name.is_empty() {
            continue;
        }
        let exists = original.columns().iter().any(|c| c.name().eq_ignore_ascii_case(name));
        if !exists {
            out.push(format!(
                "ALTER TABLE {} ADD COLUMN {};",
                qtable(schema, target, dialect),
                column_definition(col, dialect)
            ));
        }
    }
    for col in original.columns() {
        let gone = !form
            .columns
            .iter()
            .any(|c| c.name.trim().eq_ignore_ascii_case(col.name()));
        if gone {
            out.push(format!(
                "ALTER TABLE {} DROP COLUMN {};",
                qtable(schema, target, dialect),
                qi(col.name(), dialect)
            ));
        }
    }
    for idx in &form.indexes {
        let name = idx.name.trim();
        if name.is_empty() {
            continue;
        }
        let exists =
            original.indexes().iter().any(|i| i.name().eq_ignore_ascii_case(name));
        if !exists
            && let Some(statement) = create_index_statement(schema, target, idx, dialect)
        {
            out.push(statement);
        }
    }
    for idx in original.indexes() {
        let gone = !form
            .indexes
            .iter()
            .any(|i| i.name.trim().eq_ignore_ascii_case(idx.name()));
        if gone {
            match dialect {
                DbKind::Mysql => out.push(format!(
                    "DROP INDEX {} ON {};",
                    qi(idx.name(), dialect),
                    qtable(schema, target, dialect)
                )),
                DbKind::Postgres if !schema.is_empty() => out.push(format!(
                    "DROP INDEX {}.{};",
                    qi(schema, dialect),
                    qi(idx.name(), dialect)
                )),
                _ => out.push(format!("DROP INDEX {};", qi(idx.name(), dialect))),
            }
        }
    }
    out
}

/// SQLite reports foreign keys outside `constraints`, so fold them in as
/// named foreign constraints before building DDL text.
fn merged_constraints(table: &TableInfo) -> Vec<ConstraintInfo> {
    let mut out = table.constraints().to_vec();
    for (i, fk) in table.foreign_keys().iter().enumerate() {
        let already = out.iter().any(|c| {
            c.kind() == ConstraintKind::Foreign
                && c.columns() == fk.columns()
                && c.ref_table() == Some(fk.ref_table())
                && c.ref_columns() == fk.ref_columns()
        });
        if already {
            continue;
        }
        let name = fk.constraint_name().map(str::to_owned).unwrap_or_else(|| {
            if fk.columns().is_empty() {
                format!("fk_{i}")
            } else {
                format!("fk_{}_{}", table.name(), fk.columns().join("_"))
            }
        });
        if let Ok(info) = ConstraintInfo::new(
            name,
            ConstraintKind::Foreign,
            fk.columns().to_vec(),
            Some(fk.ref_table().to_owned()),
            fk.ref_columns().to_vec(),
            None,
        ) {
            out.push(info);
        }
    }
    out
}

pub fn show_table_ddl(ui: &mut egui::Ui, table: &TableInfo, dialect: DbKind) {
    let merged = merged_constraints(table);
    let ddl = match dialect {
        DbKind::Sqlite => {
            table_ddl_sqlite(table.name(), table.columns(), &merged, table.indexes())
        }
        // MySQL reuses the shared builder until its own quoting unit lands.
        DbKind::Postgres | DbKind::Mysql => {
            table_ddl_postgres(table.name(), table.columns(), &merged, table.indexes())
        }
    };
    let col_names: Vec<String> =
        table.columns().iter().map(|c| c.name().to_owned()).collect();
    let statements = [
        ("SELECT", gen_select(table.name(), &col_names)),
        ("INSERT", gen_insert(table.name(), &col_names)),
        ("UPDATE", gen_update(table.name(), &col_names)),
        ("DELETE", gen_delete(table.name(), &col_names)),
    ];
    egui::CollapsingHeader::new(egui::RichText::new("DDL").small().strong())
        .default_open(false)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("CREATE TABLE").small().strong());
                if ui.small_button("Copy DDL").clicked() {
                    ui.ctx().copy_text(ddl.clone());
                }
            });
            ui.code(ddl);
            for (label, sql) in statements {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(label).small().strong());
                    if ui.small_button(format!("Copy {label}")).clicked() {
                        ui.ctx().copy_text(sql.clone());
                    }
                });
                ui.code(sql);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (String, Vec<ColumnInfo>, Vec<ConstraintInfo>, Vec<IndexInfo>) {
        let columns = vec![
            ColumnInfo::new("id".to_owned(), "INTEGER".to_owned(), false, Some(1)).unwrap(),
            ColumnInfo::new("name".to_owned(), "TEXT".to_owned(), true, None).unwrap(),
            ColumnInfo::new("role_id".to_owned(), "INTEGER".to_owned(), true, None).unwrap(),
        ];
        let constraints = vec![
            ConstraintInfo::new(
                "pk_users".to_owned(),
                ConstraintKind::Pk,
                vec!["id".to_owned()],
                None,
                Vec::new(),
                None,
            )
            .unwrap(),
            ConstraintInfo::new(
                "uq_users_name".to_owned(),
                ConstraintKind::Unique,
                vec!["name".to_owned()],
                None,
                Vec::new(),
                None,
            )
            .unwrap(),
            ConstraintInfo::new(
                "fk_users_role".to_owned(),
                ConstraintKind::Foreign,
                vec!["role_id".to_owned()],
                Some("roles".to_owned()),
                vec!["id".to_owned()],
                None,
            )
            .unwrap(),
        ];
        let indexes = vec![
            IndexInfo::new("idx_users_name".to_owned(), vec!["name".to_owned()], false)
                .unwrap(),
        ];
        ("users".to_owned(), columns, constraints, indexes)
    }

    #[test]
    fn sqlite_ddl_has_pk_fk_and_unique_lines() {
        let (name, columns, constraints, indexes) = sample();
        let ddl = table_ddl_sqlite(&name, &columns, &constraints, &indexes);
        assert!(ddl.contains("PRIMARY KEY"), "missing PK line:\n{ddl}");
        assert!(ddl.contains("REFERENCES"), "missing FK REFERENCES:\n{ddl}");
        assert!(ddl.contains("UNIQUE"), "missing UNIQUE line:\n{ddl}");
    }

    #[test]
    fn postgres_ddl_has_pk_fk_unique_and_quoted_idents() {
        let (name, columns, constraints, indexes) = sample();
        let ddl = table_ddl_postgres(&name, &columns, &constraints, &indexes);
        assert!(ddl.contains("PRIMARY KEY"), "missing PK line:\n{ddl}");
        assert!(ddl.contains("REFERENCES"), "missing FK REFERENCES:\n{ddl}");
        assert!(ddl.contains("UNIQUE"), "missing UNIQUE line:\n{ddl}");
        assert!(ddl.contains("\"users\""), "missing quoted table:\n{ddl}");
        assert!(ddl.contains("\"role_id\""), "missing quoted column:\n{ddl}");
    }

    #[test]
    fn generated_statements_match_exactly() {
        let cols = vec!["id".to_owned(), "name".to_owned()];
        assert_eq!(
            gen_select("users", &cols),
            "SELECT \"id\", \"name\" FROM \"users\";"
        );
        assert_eq!(
            gen_insert("users", &cols),
            "INSERT INTO \"users\" (\"id\", \"name\") VALUES (?, ?);"
        );
        assert_eq!(
            gen_update("users", &cols),
            "UPDATE \"users\" SET \"id\" = ?, \"name\" = ?;"
        );
        assert_eq!(gen_delete("users", &cols), "DELETE FROM \"users\";");
    }

    fn form_sample() -> TableForm {
        TableForm {
            schema: String::new(),
            table: "users".to_owned(),
            columns: vec![
                TableColumnDraft {
                    name: "id".to_owned(),
                    data_type: "INTEGER".to_owned(),
                    nullable: false,
                    default: String::new(),
                    pk: true,
                },
                TableColumnDraft {
                    name: "name".to_owned(),
                    data_type: "TEXT".to_owned(),
                    nullable: true,
                    default: String::new(),
                    pk: false,
                },
            ],
            indexes: vec![TableIndexDraft {
                name: "idx_users_name".to_owned(),
                columns: "name".to_owned(),
                unique: false,
            }],
        }
    }

    #[test]
    fn create_form_builds_table_plus_index_literally() {
        let form = form_sample();
        assert!(validate_table_form(&form).is_empty());
        assert_eq!(
            create_table_statements(&form, DbKind::Sqlite),
            vec![
                "CREATE TABLE \"users\" (\n  \"id\" INTEGER NOT NULL,\n  \"name\" TEXT,\n  PRIMARY KEY (\"id\")\n);"
                    .to_owned(),
                "CREATE INDEX \"idx_users_name\" ON \"users\" (\"name\");".to_owned(),
            ]
        );
    }

    #[test]
    fn create_form_renders_defaults_and_unique_literally() {
        let form = TableForm {
            schema: String::new(),
            table: "accounts".to_owned(),
            columns: vec![TableColumnDraft {
                name: "active".to_owned(),
                data_type: "INTEGER".to_owned(),
                nullable: false,
                default: "1".to_owned(),
                pk: false,
            }],
            indexes: vec![TableIndexDraft {
                name: "uq_accounts_active".to_owned(),
                columns: "active".to_owned(),
                unique: true,
            }],
        };
        assert_eq!(
            create_table_statements(&form, DbKind::Postgres),
            vec![
                "CREATE TABLE \"accounts\" (\n  \"active\" INTEGER NOT NULL DEFAULT 1\n);"
                    .to_owned(),
                "CREATE UNIQUE INDEX \"uq_accounts_active\" ON \"accounts\" (\"active\");"
                    .to_owned(),
            ]
        );
    }

    #[test]
    fn create_form_qualifies_table_with_schema() {
        let mut form = form_sample();
        form.schema = "public".to_owned();
        let statements = create_table_statements(&form, DbKind::Postgres);
        assert_eq!(
            statements[0],
            "CREATE TABLE \"public\".\"users\" (\n  \"id\" INTEGER NOT NULL,\n  \"name\" TEXT,\n  PRIMARY KEY (\"id\")\n);"
        );
        assert_eq!(
            statements[1],
            "CREATE INDEX \"idx_users_name\" ON \"public\".\"users\" (\"name\");"
        );
    }

    #[test]
    fn alter_form_adds_a_column_and_an_index_literally() {
        let original = TableInfo::new(
            "users".to_owned(),
            crate::db::TableKind::Table,
            vec![
                ColumnInfo::new("id".to_owned(), "INTEGER".to_owned(), false, Some(1))
                    .unwrap(),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            false,
        )
        .unwrap();
        let mut form = TableForm {
            schema: String::new(),
            table: "users".to_owned(),
            columns: vec![
                TableColumnDraft {
                    name: "id".to_owned(),
                    data_type: "INTEGER".to_owned(),
                    nullable: false,
                    default: String::new(),
                    pk: true,
                },
                TableColumnDraft {
                    name: "email".to_owned(),
                    data_type: "TEXT".to_owned(),
                    nullable: true,
                    default: String::new(),
                    pk: false,
                },
            ],
            indexes: vec![TableIndexDraft {
                name: "idx_users_email".to_owned(),
                columns: "email".to_owned(),
                unique: false,
            }],
        };
        assert!(validate_table_form(&form).is_empty());
        assert_eq!(
            alter_table_statements(&original, &form, DbKind::Postgres),
            vec![
                "ALTER TABLE \"users\" ADD COLUMN \"email\" TEXT;".to_owned(),
                "CREATE INDEX \"idx_users_email\" ON \"users\" (\"email\");".to_owned(),
            ]
        );
        form.indexes.clear();
        form.columns.pop();
        assert!(alter_table_statements(&original, &form, DbKind::Postgres).is_empty());
    }

    #[test]
    fn alter_form_drops_a_column_and_an_index_per_dialect() {
        let original = TableInfo::new(
            "users".to_owned(),
            crate::db::TableKind::Table,
            vec![
                ColumnInfo::new("id".to_owned(), "INTEGER".to_owned(), false, Some(1))
                    .unwrap(),
                ColumnInfo::new("nick".to_owned(), "TEXT".to_owned(), true, None).unwrap(),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![
                IndexInfo::new("idx_users_nick".to_owned(), vec!["nick".to_owned()], false)
                    .unwrap(),
            ],
            false,
            false,
        )
        .unwrap();
        let form = TableForm {
            schema: String::new(),
            table: "users".to_owned(),
            columns: vec![TableColumnDraft {
                name: "id".to_owned(),
                data_type: "INTEGER".to_owned(),
                nullable: false,
                default: String::new(),
                pk: true,
            }],
            indexes: Vec::new(),
        };
        assert_eq!(
            alter_table_statements(&original, &form, DbKind::Sqlite),
            vec![
                "ALTER TABLE \"users\" DROP COLUMN \"nick\";".to_owned(),
                "DROP INDEX \"idx_users_nick\";".to_owned(),
            ]
        );
        assert_eq!(
            alter_table_statements(&original, &form, DbKind::Mysql),
            vec![
                "ALTER TABLE `users` DROP COLUMN `nick`;".to_owned(),
                "DROP INDEX `idx_users_nick` ON `users`;".to_owned(),
            ]
        );
    }

    #[test]
    fn create_form_quotes_mysql_idents_with_backticks() {
        let form = form_sample();
        assert_eq!(
            create_table_statements(&form, DbKind::Mysql),
            vec![
                "CREATE TABLE `users` (\n  `id` INTEGER NOT NULL,\n  `name` TEXT,\n  PRIMARY KEY (`id`)\n);"
                    .to_owned(),
                "CREATE INDEX `idx_users_name` ON `users` (`name`);".to_owned(),
            ]
        );
    }

    #[test]
    fn form_validation_names_each_problem() {
        let mut form = TableForm {
            schema: String::new(),
            table: "  ".to_owned(),
            columns: vec![
                TableColumnDraft {
                    name: "id".to_owned(),
                    data_type: "INTEGER".to_owned(),
                    nullable: false,
                    default: String::new(),
                    pk: true,
                },
                TableColumnDraft {
                    name: "ID".to_owned(),
                    data_type: String::new(),
                    nullable: true,
                    default: String::new(),
                    pk: false,
                },
            ],
            indexes: vec![TableIndexDraft {
                name: "idx_bad".to_owned(),
                columns: "missing".to_owned(),
                unique: false,
            }],
        };
        let errors = validate_table_form(&form);
        assert!(errors.iter().any(|e| e.contains("table name is empty")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("duplicate column")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("needs a type")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("unknown column")), "{errors:?}");
        form.table = "users".to_owned();
        form.columns.pop();
        form.indexes.clear();
        assert!(validate_table_form(&form).is_empty());
    }

    #[test]
    fn blank_form_starts_valid_shaped_with_one_text_column() {
        let form = TableForm::blank();
        assert_eq!(form.columns.len(), 1);
        assert_eq!(form.columns[0].data_type, "TEXT");
        assert!(form.indexes.is_empty());
    }

    #[test]
    fn from_table_prefills_columns_and_indexes() {
        let info = TableInfo::new(
            "users".to_owned(),
            crate::db::TableKind::Table,
            vec![
                ColumnInfo::new("id".to_owned(), "INTEGER".to_owned(), false, Some(1))
                    .unwrap(),
                ColumnInfo::new("name".to_owned(), "TEXT".to_owned(), true, None).unwrap(),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![
                IndexInfo::new("idx_users_name".to_owned(), vec!["name".to_owned()], false)
                    .unwrap(),
            ],
            false,
            false,
        )
        .unwrap();
        let form = TableForm::from_table("public", &info);
        assert_eq!(form.schema, "public");
        assert_eq!(form.table, "users");
        assert_eq!(form.columns.len(), 2);
        assert!(form.columns[0].pk);
        assert!(!form.columns[0].nullable);
        assert_eq!(form.indexes.len(), 1);
        assert_eq!(form.indexes[0].columns, "name");
        assert!(validate_table_form(&form).is_empty());
    }
}
