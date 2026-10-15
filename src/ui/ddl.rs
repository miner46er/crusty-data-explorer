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
        DbKind::Postgres => {
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
}
