//! SQL autocomplete for the query editor.
//!
//! Pure helpers ([`current_token`], [`suggest`], [`apply_completion`]) know
//! nothing about egui. [`editor_with_completion`] renders the editor plus its
//! popup. Popup state lives in the egui context temp data, so `AppState` is
//! untouched.

use std::collections::HashMap;

use eframe::egui;

use crate::db::SchemaInfo;

/// Cap on rows returned by [`suggest`].
pub const SUGGEST_CAP: usize = 50;

/// Exactly 40 SQL keywords offered as completions.
pub const KEYWORDS: [&str; 40] = [
    "SELECT", "FROM", "WHERE", "JOIN", "INNER", "LEFT", "RIGHT", "OUTER", "ON", "GROUP",
    "BY", "ORDER", "HAVING", "LIMIT", "OFFSET", "UNION", "ALL", "DISTINCT", "INSERT", "INTO",
    "VALUES", "UPDATE", "SET", "DELETE", "CREATE", "TABLE", "VIEW", "INDEX", "DROP", "ALTER",
    "ADD", "PRIMARY", "KEY", "FOREIGN", "REFERENCES", "NOT", "NULL", "AND", "OR", "AS",
];

/// Words that can never be a table alias, so `FROM users where` does not
/// invent an alias named `where`.
const CLAUSE_WORDS: [&str; 42] = [
    "where", "group", "order", "having", "limit", "offset", "union", "except", "intersect",
    "join", "inner", "left", "right", "full", "outer", "cross", "natural", "on", "using",
    "select", "from", "set", "values", "into", "update", "delete", "insert", "create",
    "table", "view", "index", "drop", "alter", "window", "fetch", "for", "lock",
    "returning", "when", "else", "end", "case",
];

/// What a completion inserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteKind {
    Table,
    Column,
    Keyword,
}

/// One popup row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub label: String,
    pub kind: CompleteKind,
    pub detail: String,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn clamp_cursor(sql: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(sql.len());
    while !sql.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

fn char_to_byte(sql: &str, char_idx: usize) -> usize {
    sql.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(sql.len())
}

/// The word under `cursor` plus the word before it. A cursor after
/// whitespace or punctuation yields an empty word with the last word as the
/// preceding token.
pub fn current_token(sql: &str, cursor: usize) -> (String, Option<String>) {
    let cursor = clamp_cursor(sql, cursor);
    let prefix = &sql[..cursor];
    let words: Vec<&str> =
        prefix.split(|c: char| !is_word_char(c)).filter(|w| !w.is_empty()).collect();
    let ends_in_word = prefix.chars().next_back().is_some_and(is_word_char);
    if ends_in_word {
        let word = words.last().copied().unwrap_or("").to_owned();
        let prev = words.iter().rev().nth(1).map(|s| s.to_string());
        (word, prev)
    } else {
        (String::new(), words.last().map(|s| s.to_string()))
    }
}

/// Trailing `qualifier.partial` at the end of `prefix`, if any.
fn dot_qualifier(prefix: &str) -> Option<(String, String)> {
    let mut partial_start = prefix.len();
    for (i, c) in prefix.char_indices().rev() {
        if is_word_char(c) {
            partial_start = i;
        } else {
            break;
        }
    }
    let partial = &prefix[partial_start..];
    let before_dot = &prefix[..partial_start];
    if !before_dot.ends_with('.') {
        return None;
    }
    let before = &before_dot[..before_dot.len() - 1];
    let mut qual_start = before.len();
    for (i, c) in before.char_indices().rev() {
        if is_word_char(c) {
            qual_start = i;
        } else {
            break;
        }
    }
    let qual = &before[qual_start..];
    if qual.is_empty() {
        return None;
    }
    Some((qual.to_owned(), partial.to_owned()))
}

fn all_tables(schema: &SchemaInfo) -> Vec<Completion> {
    let mut out = Vec::new();
    for db in schema.databases() {
        for node in db.schemas() {
            for table in node.tables() {
                out.push(Completion {
                    label: table.name().to_owned(),
                    kind: CompleteKind::Table,
                    detail: "table".to_owned(),
                });
            }
            for view in node.views() {
                out.push(Completion {
                    label: view.name().to_owned(),
                    kind: CompleteKind::Table,
                    detail: "view".to_owned(),
                });
            }
        }
    }
    out
}

fn find_table<'a>(
    schema: &'a SchemaInfo,
    name: &str,
) -> Option<&'a crate::db::TableInfo> {
    let lower = name.to_lowercase();
    schema
        .databases()
        .iter()
        .flat_map(|db| db.schemas())
        .flat_map(|node| node.tables().iter().chain(node.views()))
        .find(|t| t.name().to_lowercase() == lower)
}

/// Lowercased alias to lowercased table, from `FROM`/`JOIN` clauses.
fn alias_map(sql: &str) -> HashMap<String, String> {
    let words: Vec<&str> =
        sql.split(|c: char| !is_word_char(c)).filter(|w| !w.is_empty()).collect();
    let mut map = HashMap::new();
    let mut i = 0;
    while i < words.len() {
        if words[i].eq_ignore_ascii_case("from") || words[i].eq_ignore_ascii_case("join") {
            if let Some(table) = words.get(i + 1) {
                let table = table.to_lowercase();
                let mut alias = words.get(i + 2).copied();
                if alias.is_some_and(|a| a.eq_ignore_ascii_case("as")) {
                    alias = words.get(i + 3).copied();
                }
                if let Some(alias) = alias {
                    let low = alias.to_lowercase();
                    if !CLAUSE_WORDS.contains(&low.as_str()) {
                        map.insert(low, table);
                    }
                }
            }
        }
        i += 1;
    }
    map
}

fn starts_with_insensitive(label: &str, prefix: &str) -> bool {
    label.to_lowercase().starts_with(&prefix.to_lowercase())
}

/// Up to 50 completions: tables after `FROM`/`JOIN`, columns after a known
/// table or alias plus dot, else keywords and tables matching the prefix. An
/// empty prefix outside `FROM`/`JOIN` yields nothing.
pub fn suggest(schema: &SchemaInfo, sql: &str, cursor: usize) -> Vec<Completion> {
    let cursor = clamp_cursor(sql, cursor);
    let prefix = &sql[..cursor];
    if let Some((qual, partial)) = dot_qualifier(prefix) {
        let aliases = alias_map(sql);
        let key = qual.to_lowercase();
        let real = aliases.get(&key).cloned().unwrap_or(key);
        if let Some(table) = find_table(schema, &real) {
            let mut out: Vec<Completion> = table
                .columns()
                .iter()
                .filter(|c| starts_with_insensitive(c.name(), &partial))
                .map(|c| Completion {
                    label: c.name().to_owned(),
                    kind: CompleteKind::Column,
                    detail: if c.type_name().is_empty() {
                        "column".to_owned()
                    } else {
                        c.type_name().to_owned()
                    },
                })
                .collect();
            out.truncate(SUGGEST_CAP);
            return out;
        }
    }
    let (word, prev) = current_token(sql, cursor);
    let tables = all_tables(schema);
    if prev.as_deref().is_some_and(|p| {
        p.eq_ignore_ascii_case("from") || p.eq_ignore_ascii_case("join")
    }) {
        return tables
            .into_iter()
            .filter(|t| starts_with_insensitive(&t.label, &word))
            .take(SUGGEST_CAP)
            .collect();
    }
    if word.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Completion> = KEYWORDS
        .iter()
        .filter(|k| starts_with_insensitive(k, &word))
        .map(|k| Completion {
            label: k.to_string(),
            kind: CompleteKind::Keyword,
            detail: "keyword".to_owned(),
        })
        .collect();
    out.extend(tables.into_iter().filter(|t| starts_with_insensitive(&t.label, &word)));
    out.truncate(SUGGEST_CAP);
    out
}

/// Replace the word under `cursor` (or the partial after a dot) with the
/// completion label. Returns the new SQL and the new byte cursor.
pub fn apply_completion(
    sql: &str,
    cursor: usize,
    completion: &Completion,
) -> (String, usize) {
    let cursor = clamp_cursor(sql, cursor);
    let prefix = &sql[..cursor];
    let suffix = &sql[cursor..];
    let start = if let Some((_, partial)) = dot_qualifier(prefix) {
        cursor - partial.len()
    } else {
        let (word, _) = current_token(sql, cursor);
        cursor - word.len()
    };
    let mut next = String::with_capacity(sql.len() + completion.label.len());
    next.push_str(&sql[..start]);
    next.push_str(&completion.label);
    next.push_str(suffix);
    let next_cursor = start + completion.label.len();
    (next, next_cursor)
}

pub fn completion_shortcut() -> egui::KeyboardShortcut {
    egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::Space)
}

#[derive(Clone, Default)]
struct CompleteState {
    open: bool,
    selected: usize,
}

/// SQL editor with autocomplete. Ctrl+Space opens the popup, Enter applies
/// the selected row, Esc closes it.
pub fn editor_with_completion(
    schema: Option<&SchemaInfo>,
    sql: &mut String,
    ui: &mut egui::Ui,
) {
    let empty = SchemaInfo::new(Vec::new(), false, false);
    let schema = schema.unwrap_or(&empty);
    let id = egui::Id::new("crusty_complete");
    let mut state: CompleteState =
        ui.ctx().data_mut(|data| data.get_temp(id).unwrap_or_default());

    if ui.input_mut(|input| input.consume_shortcut(&completion_shortcut())) {
        state.open = true;
        state.selected = 0;
    }
    let mut apply_enter = false;
    if state.open {
        if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            state.open = false;
        } else {
            if ui.input_mut(|input| {
                input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)
            }) {
                state.selected = state.selected.saturating_add(1);
            }
            if ui.input_mut(|input| {
                input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)
            }) {
                state.selected = state.selected.saturating_sub(1);
            }
            if ui.input_mut(|input| {
                input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
            }) {
                apply_enter = true;
            }
        }
    }

    let out = egui::TextEdit::multiline(sql)
        .code_editor()
        .desired_rows(8)
        .desired_width(f32::INFINITY)
        .show(ui);
    let byte_cursor = out
        .cursor_range
        .map(|range| char_to_byte(sql, range.primary.index.0))
        .unwrap_or_else(|| sql.len());

    if apply_enter && state.open {
        let items = suggest(schema, sql, byte_cursor);
        if items.is_empty() {
            sql.insert(byte_cursor, '\n');
        } else if let Some(pick) = items.get(state.selected.min(items.len() - 1)) {
            let (next, _) = apply_completion(sql, byte_cursor, pick);
            *sql = next;
        }
        state.open = false;
    }

    if state.open {
        let items = suggest(schema, sql, byte_cursor);
        if items.is_empty() {
            state.open = false;
        } else {
            state.selected = state.selected.min(items.len() - 1);
            let mut clicked: Option<usize> = None;
            egui::Window::new("sql completion")
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                        for (i, item) in items.iter().enumerate() {
                            if ui
                                .selectable_label(
                                    state.selected == i,
                                    format!("{}  · {}", item.label, item.detail),
                                )
                                .clicked()
                            {
                                clicked = Some(i);
                            }
                        }
                    });
                });
            if let Some(i) = clicked {
                if let Some(pick) = items.get(i) {
                    let (next, _) = apply_completion(sql, byte_cursor, pick);
                    *sql = next;
                }
                state.open = false;
            }
        }
    }

    ui.ctx().data_mut(|data| data.insert_temp(id, state));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        ColumnInfo, DatabaseNode, SchemaNode, TableInfo, TableKind,
    };

    fn table(name: &str, cols: &[(&str, &str)]) -> TableInfo {
        TableInfo::new(
            name.to_owned(),
            TableKind::Table,
            cols.iter()
                .map(|(name, ty)| {
                    ColumnInfo::new((*name).to_owned(), (*ty).to_owned(), true, None)
                        .unwrap()
                })
                .collect(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            false,
        )
        .unwrap()
    }

    fn test_schema() -> SchemaInfo {
        let schema = SchemaNode::new(
            "main".to_owned(),
            vec![
                table("users", &[("id", "INTEGER"), ("name", "TEXT"), ("email", "TEXT")]),
                table("orders", &[("id", "INTEGER"), ("user_id", "INTEGER")]),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let db = DatabaseNode::new("db".to_owned(), vec![schema]).unwrap();
        SchemaInfo::new(vec![db], false, false)
    }

    fn many_schema(n: usize) -> SchemaInfo {
        let tables =
            (0..n).map(|i| table(&format!("t{i:02}"), &[("id", "INTEGER")])).collect();
        let schema =
            SchemaNode::new("main".to_owned(), tables, Vec::new(), Vec::new(), Vec::new())
                .unwrap();
        let db = DatabaseNode::new("db".to_owned(), vec![schema]).unwrap();
        SchemaInfo::new(vec![db], false, false)
    }

    #[test]
    fn tables_after_from() {
        let schema = test_schema();
        let sql = "SELECT * FROM us";
        let items = suggest(&schema, sql, sql.len());
        assert!(items.iter().any(|c| c.label == "users"
            && c.kind == CompleteKind::Table));
        assert!(!items.iter().any(|c| c.label == "orders"));
        let upper = "SELECT * FROM US";
        let items = suggest(&schema, upper, upper.len());
        assert!(items.iter().any(|c| c.label == "users"));
    }

    #[test]
    fn columns_after_alias_dot() {
        let schema = test_schema();
        let sql = "SELECT u. FROM users u";
        let cursor = "SELECT u.".len();
        let items = suggest(&schema, sql, cursor);
        let labels: Vec<&str> =
            items.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["id", "name", "email"]);
        assert!(items.iter().all(|c| c.kind == CompleteKind::Column));
        let direct = "SELECT users. FROM users";
        let cursor = "SELECT users.".len();
        let items = suggest(&schema, direct, cursor);
        assert!(items.iter().any(|c| c.label == "email"));
    }

    #[test]
    fn keyword_fallback() {
        let schema = test_schema();
        let sql = "SEL";
        let items = suggest(&schema, sql, sql.len());
        assert!(items.iter().any(|c| c.label == "SELECT"
            && c.kind == CompleteKind::Keyword));
    }

    #[test]
    fn empty_prefix_gives_nothing() {
        let schema = test_schema();
        assert!(suggest(&schema, "", 0).is_empty());
        let sql = "SELECT ";
        assert!(suggest(&schema, sql, sql.len()).is_empty());
    }

    #[test]
    fn cap_enforced() {
        let schema = many_schema(60);
        let sql = "SELECT * FROM ";
        let items = suggest(&schema, sql, sql.len());
        assert_eq!(items.len(), 50);
    }
}
