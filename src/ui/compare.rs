//! Table data compare for migration checks.
//!
//! The model is pure over two [`QueryResult`]s: [`CompareSpec`] names the
//! left and right table picks with their key columns, and [`diff_tables`]
//! classifies every key as added (right only), removed (left only), or
//! changed (both sides, at least one cell differs). Unchanged keys count but
//! keep no row, so a clean compare stays small. The view is a thin shell over
//! [`AppState`](crate::app::AppState) with plain egui widgets and no worker
//! change: [`AppState::start_compare`](crate::app::AppState::start_compare)
//! sends the two reads, the landings stash into compare state, and the cached
//! output re-diffs on every landing and key edit.

use eframe::egui;

use crate::app::AppState;
use crate::db::{CELL_CAP, DbKind, QueryResult, Value};

/// Rows shown past this cap stay out with a "+N more" note, so a 10k-row
/// diff never builds a 10k-widget frame.
pub const MAX_DIFF_ROWS: usize = 200;

/// Which side of the diff a row belongs to. Added is right only, removed is
/// left only, changed matched on both sides with at least one cell differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Added,
    Removed,
    Changed,
}

impl DiffKind {
    fn label(self) -> &'static str {
        match self {
            DiffKind::Added => "Added",
            DiffKind::Removed => "Removed",
            DiffKind::Changed => "Changed",
        }
    }
}

/// One diff row: its key label, its classification, the changed column names
/// (empty unless changed), and both sides' display cells over the union
/// columns. A missing side reads as empty strings, so added rows carry only
/// right cells and removed rows only left cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    pub key: String,
    pub kind: DiffKind,
    pub changed: Vec<String>,
    pub left: Vec<String>,
    pub right: Vec<String>,
}

/// The diff counts. `added` is right only, `removed` is left only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompareCounts {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub unchanged: usize,
}

impl CompareCounts {
    /// Every classified row: added plus removed plus changed.
    pub fn diffs(self) -> usize {
        self.added + self.removed + self.changed
    }
    /// Every matched key: diffs plus unchanged.
    pub fn total(self) -> usize {
        self.diffs() + self.unchanged
    }
}

/// One finished diff: the union column names (left order, then right-only
/// columns appended), the classified rows in encoded-key order, their
/// counts, and whether either input hit the row cap and read partial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareOutput {
    pub columns: Vec<String>,
    pub rows: Vec<DiffRow>,
    pub counts: CompareCounts,
    pub truncated: bool,
}

/// Why a diff refused to run. Every variant names the missing piece.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompareError {
    #[error("key columns are empty")]
    NoKeys,
    #[error("{side} result has no key column \"{column}\"")]
    MissingColumn { side: &'static str, column: String },
    #[error("duplicate key \"{key}\"")]
    DuplicateKey { key: String },
}

/// The full-table read one compare side sends. No LIMIT: the worker caps at
/// its row cap and flags truncation, which the output carries. Quoting is per
/// dialect, so the same pick runs on SQLite, Postgres, and MySQL.
pub fn compare_select(kind: DbKind, schema: &str, table: &str) -> String {
    match kind {
        DbKind::Mysql => format!("SELECT * FROM {}",
            mysql_qualified(schema, table)),
        DbKind::Sqlite | DbKind::Postgres => {
            format!("SELECT * FROM \"{}\".\"{}\"",
                schema.replace('"', "\"\""),
                table.replace('"', "\"\""))
        }
    }
}

/// One `db`.`table` reference with backticks doubled, the MySQL spelling of
/// the double-quoted form above.
fn mysql_qualified(schema: &str, table: &str) -> String {
    format!("`{}`.`{}`",
        schema.replace('`', "``"),
        table.replace('`', "``"))
}

/// Split raw key text on commas, trimming and dropping blanks, so
/// `" id, name ,, "` reads as two keys.
pub fn parse_key_columns(raw: &str) -> Vec<String> {
    raw.split(',').map(str::trim).filter(|part| !part.is_empty()).map(str::to_owned).collect()
}

/// Strict cell equality: values match by shape and content, so Integer 1 and
/// Real 1.0 differ the way the engines store them.
fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Integer(a), Value::Integer(b)) => a == b,
        (Value::Real(a), Value::Real(b)) => a == b,
        (Value::Text(a), Value::Text(b)) => a == b,
        (Value::Blob(a), Value::Blob(b)) => a == b,
        _ => false,
    }
}

/// One key part's map encoding. Every shape is self-delimiting (text carries
/// its length, reals their bits, blobs their hex), so distinct key tuples
/// never share an encoding.
fn key_part(value: &Value) -> String {
    match value {
        Value::Null => "N".to_owned(),
        Value::Integer(n) => format!("I{n}"),
        Value::Real(f) => format!("R{:016x}", f.to_bits()),
        Value::Text(s) => format!("T{}:{s}", s.len()),
        Value::Blob(bytes) => format!("B{}", blob_hex(bytes)),
    }
}

/// Lowercase hex of raw bytes for key encodings.
fn blob_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// The column index behind a name, case-insensitive, so `ID` matches `id`.
fn column_index(result: &QueryResult, name: &str) -> Option<usize> {
    result.columns().iter().position(|c| c.name().eq_ignore_ascii_case(name))
}

/// Diff two table reads over their key columns. Keys must be non-empty and
/// present on both sides, and unique per side. Non-key columns compare across
/// the union of both sides by name; a column missing on one side differs from
/// every value there, so a shape drift reads as changed rows rather than an
/// error. Empty inputs diff to empty counts, and a one-sided input reads as
/// all added or all removed.
pub fn diff_tables(
    left: &QueryResult,
    right: &QueryResult,
    keys: &[String],
) -> Result<CompareOutput, CompareError> {
    if keys.is_empty() {
        return Err(CompareError::NoKeys);
    }
    let mut left_keys = Vec::with_capacity(keys.len());
    let mut right_keys = Vec::with_capacity(keys.len());
    for key in keys {
        left_keys.push(column_index(left, key).ok_or_else(|| CompareError::MissingColumn {
            side: "left",
            column: key.clone(),
        })?);
        right_keys.push(column_index(right, key).ok_or_else(|| CompareError::MissingColumn {
            side: "right",
            column: key.clone(),
        })?);
    }
    let mut columns: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for column in left.columns().iter().chain(right.columns()) {
        let lowered = column.name().to_lowercase();
        if !seen.contains(&lowered) {
            seen.push(lowered);
            columns.push(column.name().to_owned());
        }
    }
    let at = |result: &QueryResult, col: usize, row: usize| -> (String, String) {
        match result.cell(row, col) {
            Some(value) => (key_part(value), value.display(CELL_CAP).into_owned()),
            None => (String::new(), String::new()),
        }
    };
    let mut left_rows = std::collections::BTreeMap::new();
    for row in 0..left.row_count() {
        let mut encoding = String::new();
        let mut label = String::new();
        for (position, col) in left_keys.iter().enumerate() {
            let (part, text) = at(left, *col, row);
            if position > 0 {
                encoding.push('\x1f');
                label.push_str(" | ");
            }
            encoding.push_str(&part);
            label.push_str(&text);
        }
        if left_rows.insert(encoding, row).is_some() {
            return Err(CompareError::DuplicateKey { key: label });
        }
    }
    let mut right_rows = std::collections::BTreeMap::new();
    for row in 0..right.row_count() {
        let mut encoding = String::new();
        let mut label = String::new();
        for (position, col) in right_keys.iter().enumerate() {
            let (part, text) = at(right, *col, row);
            if position > 0 {
                encoding.push('\x1f');
                label.push_str(" | ");
            }
            encoding.push_str(&part);
            label.push_str(&text);
        }
        if right_rows.insert(encoding, row).is_some() {
            return Err(CompareError::DuplicateKey { key: label });
        }
    }
    let mut key_order: Vec<&String> = left_rows.keys().chain(right_rows.keys()).collect();
    key_order.sort();
    key_order.dedup();
    let mut rows = Vec::new();
    let mut counts = CompareCounts::default();
    for encoding in key_order {
        let left_row = left_rows.get(encoding).copied();
        let right_row = right_rows.get(encoding).copied();
        let label = key_label(left, &left_keys, left_row, right, &right_keys, right_row);
        let mut left_cells = Vec::with_capacity(columns.len());
        let mut right_cells = Vec::with_capacity(columns.len());
        let mut changed = Vec::new();
        for name in &columns {
            let left_col = column_index(left, name);
            let right_col = column_index(right, name);
            let left_value = left_col.and_then(|col| left_row.and_then(|row| left.cell(row, col)));
            let right_value = right_col.and_then(|col| right_row.and_then(|row| right.cell(row, col)));
            left_cells.push(left_value.map(|v| v.display(CELL_CAP).into_owned()).unwrap_or_default());
            right_cells.push(right_value.map(|v| v.display(CELL_CAP).into_owned()).unwrap_or_default());
            let same = match (left_value, right_value) {
                (Some(a), Some(b)) => values_equal(a, b),
                (None, None) => true,
                _ => false,
            };
            if !same {
                changed.push(name.clone());
            }
        }
        match (left_row, right_row) {
            (Some(_), None) => {
                counts.removed += 1;
                rows.push(DiffRow { key: label, kind: DiffKind::Removed, changed, left: left_cells, right: right_cells });
            }
            (None, Some(_)) => {
                counts.added += 1;
                rows.push(DiffRow { key: label, kind: DiffKind::Added, changed, left: left_cells, right: right_cells });
            }
            (Some(_), Some(_)) if !changed.is_empty() => {
                counts.changed += 1;
                rows.push(DiffRow { key: label, kind: DiffKind::Changed, changed, left: left_cells, right: right_cells });
            }
            _ => {
                counts.unchanged += 1;
            }
        }
    }
    Ok(CompareOutput { columns, rows, counts, truncated: left.is_truncated() || right.is_truncated() })
}

/// The display label for one matched key: the left side's key cells when the
/// row exists there, else the right side's. One side always exists.
fn key_label(
    left: &QueryResult,
    left_keys: &[usize],
    left_row: Option<usize>,
    right: &QueryResult,
    right_keys: &[usize],
    right_row: Option<usize>,
) -> String {
    let (result, cols, row) = match (left_row, right_row) {
        (Some(row), _) => (left, left_keys, row),
        (None, Some(row)) => (right, right_keys, row),
        (None, None) => return String::new(),
    };
    cols.iter()
        .map(|col| {
            result.cell(row, *col).map(|v| v.display(CELL_CAP).into_owned()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The table compare window: left and right picks from the loaded schema, key
/// columns, the Compare button, and the cached diff with its counts. Closed by
/// default, so the grid keeps its full height until asked. Reads state and
/// sends through transitions only.
pub fn compare_window(state: &mut AppState, ui: &mut egui::Ui) {
    if !state.compare_window_open() {
        return;
    }
    let mut open = true;
    egui::Window::new("Table compare")
        .open(&mut open)
        .resizable(true)
        .default_width(520.0)
        .show(ui.ctx(), |ui| compare_body(state, ui));
    if !open {
        state.close_compare();
    }
}

fn compare_body(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(conn) = state.default_connection() else {
        ui.label(egui::RichText::new("Connect first, then pick two tables.").weak());
        return;
    };
    let conn_name =
        state.connections().iter().find(|c| c.id == conn).map(|c| c.name.clone()).unwrap_or_default();
    ui.label(egui::RichText::new(format!("On {conn_name}")).small().weak());
    let options = compare_options(state, conn);
    if options.is_empty() {
        ui.label(
            egui::RichText::new("Load the schema from the sidebar to pick tables.").small().weak(),
        );
    } else {
        if !state.compare().left().is_set() {
            state.set_compare_left(&options[0].0, &options[0].1);
        }
        if !state.compare().right().is_set() {
            state.set_compare_right(&options[0].0, &options[0].1);
        }
    }
    pick_row(state, ui, true, &options);
    pick_row(state, ui, false, &options);
    ui.horizontal(|ui| {
        ui.label("Keys");
        let mut keys = state.compare().keys().to_owned();
        if ui
            .add(
                egui::TextEdit::singleline(&mut keys)
                    .hint_text("key columns, e.g. id")
                    .desired_width(200.0),
            )
            .changed()
        {
            state.set_compare_keys(keys);
        }
        let running = state.compare().running();
        if ui.add_enabled(!running, egui::Button::new("Compare")).clicked() {
            state.start_compare();
        }
        if running {
            ui.spinner();
            ui.label("Comparing…");
        }
    });
    if let Some(error) = state.compare().error().map(str::to_owned) {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    let Some(output) = state.compare().output().cloned() else {
        if !state.compare().running() && state.compare().error().is_none() {
            ui.label(egui::RichText::new("Pick two tables and press Compare.").small().weak());
        }
        return;
    };
    let counts = output.counts;
    ui.label(
        egui::RichText::new(format!(
            "{} diffs · {} added · {} removed · {} changed · {} unchanged",
            counts.diffs(),
            counts.added,
            counts.removed,
            counts.changed,
            counts.unchanged
        ))
        .small()
        .strong(),
    );
    if output.truncated {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            egui::RichText::new("A read hit the row cap; the diff is partial.").small(),
        );
    }
    if output.rows.is_empty() {
        ui.label(egui::RichText::new("No differences.").weak());
        return;
    }
    for row in output.rows.iter().take(MAX_DIFF_ROWS) {
        diff_row(row, &output.columns, ui);
    }
    let rest = output.rows.len().saturating_sub(MAX_DIFF_ROWS);
    if rest > 0 {
        ui.label(egui::RichText::new(format!("+{rest} more")).small().weak());
    }
}

/// One table picker row: a schema-qualified dropdown for the left or right
/// pick. Empty options keep the current pick text, so a pick outlives a schema
/// reload that no longer lists it.
fn pick_row(state: &mut AppState, ui: &mut egui::Ui, is_left: bool, options: &[(String, String)]) {
    let title = if is_left { "Left" } else { "Right" };
    let current = if is_left { state.compare().left().clone() } else { state.compare().right().clone() };
    let selected = if current.is_set() {
        format!("{}.{}", current.schema, current.table)
    } else {
        "Pick a table".to_owned()
    };
    let mut picked: Option<(String, String)> = None;
    ui.horizontal(|ui| {
        ui.label(title);
        egui::ComboBox::from_id_salt(if is_left { "crusty_compare_left" } else { "crusty_compare_right" })
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for (schema, table) in options {
                    let label = format!("{schema}.{table}");
                    let marked = current.schema == *schema && current.table == *table;
                    if ui.selectable_label(marked, &label).clicked() {
                        picked = Some((schema.clone(), table.clone()));
                    }
                }
            });
    });
    if let Some((schema, table)) = picked {
        if is_left {
            state.set_compare_left(&schema, &table);
        } else {
            state.set_compare_right(&schema, &table);
        }
    }
}

/// One diff row: its kind badge, its key, and the changed columns with both
/// values for changed rows or the present side's cells otherwise.
fn diff_row(row: &DiffRow, columns: &[String], ui: &mut egui::Ui) {
    let color = match row.kind {
        DiffKind::Added => ui.visuals().weak_text_color(),
        DiffKind::Removed => ui.visuals().error_fg_color,
        DiffKind::Changed => ui.visuals().warn_fg_color,
    };
    ui.horizontal(|ui| {
        ui.colored_label(color, egui::RichText::new(row.kind.label()).small().strong());
        ui.monospace(&row.key);
    });
    ui.indent(egui::Id::new(("crusty_compare_row", row.key.clone())), |ui| {
        match row.kind {
            DiffKind::Changed => {
                for name in &row.changed {
                    let index = columns.iter().position(|c| c == name).unwrap_or(usize::MAX);
                    let before = row.left.get(index).cloned().unwrap_or_default();
                    let after = row.right.get(index).cloned().unwrap_or_default();
                    ui.monospace(format!("{name}: {before} → {after}"));
                }
            }
            DiffKind::Added => {
                ui.label(summary_cells(columns, &row.right).unwrap_or_else(|| "empty row".to_owned()));
            }
            DiffKind::Removed => {
                ui.label(summary_cells(columns, &row.left).unwrap_or_else(|| "empty row".to_owned()));
            }
        }
    });
}

/// `col=value` pairs for the non-empty cells, so an added or removed row
/// shows what arrived or left. None when every cell is empty.
fn summary_cells(columns: &[String], cells: &[String]) -> Option<String> {
    let pairs: Vec<String> = columns
        .iter()
        .zip(cells.iter())
        .filter(|(_, value)| !value.is_empty())
        .map(|(column, value)| format!("{column}={value}"))
        .collect();
    (!pairs.is_empty()).then(|| pairs.join(", "))
}

/// The loaded tables and views behind one connection, as schema and table
/// pairs in navigator order.
fn compare_options(state: &AppState, conn: crate::db::ConnectionId) -> Vec<(String, String)> {
    let mut options = Vec::new();
    if let Some(schema) = state.schema(conn) {
        for database in schema.databases() {
            for node in database.schemas() {
                for table in node.tables().iter().chain(node.views()) {
                    options.push((node.name().to_owned(), table.name().to_owned()));
                }
            }
        }
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Column;
    use std::time::Duration;

    fn result(names: &[(&str, &str)], values: Vec<Vec<Value>>) -> QueryResult {
        QueryResult::from_columns(
            names
                .iter()
                .zip(values)
                .map(|((name, type_name), column)| {
                    Column::new((*name).to_owned(), (*type_name).to_owned(), column)
                })
                .collect(),
            false,
            None,
            Duration::from_millis(1),
        )
        .unwrap()
    }

    fn people() -> (QueryResult, QueryResult) {
        let left = result(
            &[("id", "INTEGER"), ("name", "TEXT")],
            vec![
                vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
                vec![
                    Value::Text("ada".to_owned()),
                    Value::Text("grace".to_owned()),
                    Value::Text("lin".to_owned()),
                ],
            ],
        );
        let right = result(
            &[("id", "INTEGER"), ("name", "TEXT")],
            vec![
                vec![Value::Integer(1), Value::Integer(2), Value::Integer(4)],
                vec![
                    Value::Text("ada".to_owned()),
                    Value::Text("GRACE".to_owned()),
                    Value::Text("rae".to_owned()),
                ],
            ],
        );
        (left, right)
    }

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn diff_classifies_added_removed_changed_and_counts_the_rest() {
        let (left, right) = people();
        let output = diff_tables(&left, &right, &keys(&["id"])).expect("diff runs");
        assert_eq!(output.columns, vec!["id".to_owned(), "name".to_owned()]);
        assert_eq!(
            output.counts,
            CompareCounts { added: 1, removed: 1, changed: 1, unchanged: 1 }
        );
        assert_eq!(output.rows.len(), 3);
        let removed = output.rows.iter().find(|row| row.kind == DiffKind::Removed).expect("a removal");
        assert_eq!(removed.key, "3");
        assert_eq!(removed.left, vec!["3".to_owned(), "lin".to_owned()]);
        let added = output.rows.iter().find(|row| row.kind == DiffKind::Added).expect("an addition");
        assert_eq!(added.key, "4");
        assert_eq!(added.right, vec!["4".to_owned(), "rae".to_owned()]);
        let changed = output.rows.iter().find(|row| row.kind == DiffKind::Changed).expect("a change");
        assert_eq!(changed.key, "2");
        assert_eq!(changed.changed, vec!["name".to_owned()]);
        assert_eq!(changed.left, vec!["2".to_owned(), "grace".to_owned()]);
        assert_eq!(changed.right, vec!["2".to_owned(), "GRACE".to_owned()]);
    }

    #[test]
    fn diff_needs_key_columns() {
        let (left, right) = people();
        assert_eq!(
            diff_tables(&left, &right, &[]).unwrap_err(),
            CompareError::NoKeys
        );
        assert_eq!(CompareError::NoKeys.to_string(), "key columns are empty");
    }

    #[test]
    fn diff_reports_a_key_column_missing_on_either_side() {
        let (left, right) = people();
        assert_eq!(
            diff_tables(&left, &right, &keys(&["id", "missing"])).unwrap_err(),
            CompareError::MissingColumn { side: "left", column: "missing".to_owned() }
        );
        let narrow = result(
            &[("name", "TEXT")],
            vec![vec![Value::Text("ada".to_owned())]],
        );
        assert_eq!(
            diff_tables(&left, &narrow, &keys(&["id"])).unwrap_err(),
            CompareError::MissingColumn { side: "right", column: "id".to_owned() }
        );
    }

    #[test]
    fn diff_reads_empty_inputs_as_all_added_or_all_removed() {
        let (left, _) = people();
        let empty = result(
            &[("id", "INTEGER"), ("name", "TEXT")],
            vec![Vec::new(), Vec::new()],
        );
        let both = diff_tables(&empty, &empty, &keys(&["id"])).expect("empty diff runs");
        assert_eq!(both.counts, CompareCounts::default());
        assert!(both.rows.is_empty());
        let gone = diff_tables(&left, &empty, &keys(&["id"])).expect("removal diff runs");
        assert_eq!(gone.counts.removed, 3);
        assert_eq!(gone.counts.added, 0);
        let fresh = diff_tables(&empty, &left, &keys(&["id"])).expect("addition diff runs");
        assert_eq!(fresh.counts.added, 3);
        assert_eq!(fresh.counts.removed, 0);
    }

    #[test]
    fn diff_rejects_duplicate_keys() {
        let double = result(
            &[("id", "INTEGER")],
            vec![vec![Value::Integer(1), Value::Integer(1)]],
        );
        let single = result(&[("id", "INTEGER")], vec![vec![Value::Integer(1)]]);
        assert_eq!(
            diff_tables(&double, &single, &keys(&["id"])).unwrap_err(),
            CompareError::DuplicateKey { key: "1".to_owned() }
        );
        assert_eq!(
            diff_tables(&single, &double, &keys(&["id"])).unwrap_err(),
            CompareError::DuplicateKey { key: "1".to_owned() }
        );
    }

    #[test]
    fn diff_names_a_column_only_on_one_side_as_changed() {
        let left = result(
            &[("id", "INTEGER"), ("name", "TEXT")],
            vec![vec![Value::Integer(1)], vec![Value::Text("ada".to_owned())]],
        );
        let right = result(
            &[("id", "INTEGER"), ("name", "TEXT"), ("nick", "TEXT")],
            vec![
                vec![Value::Integer(1)],
                vec![Value::Text("ada".to_owned())],
                vec![Value::Text("a".to_owned())],
            ],
        );
        let output = diff_tables(&left, &right, &keys(&["id"])).expect("union diff runs");
        assert_eq!(output.columns, vec!["id".to_owned(), "name".to_owned(), "nick".to_owned()]);
        assert_eq!(output.counts.changed, 1);
        assert_eq!(output.rows[0].changed, vec!["nick".to_owned()]);
        assert_eq!(output.rows[0].left, vec!["1".to_owned(), "ada".to_owned(), String::new()]);
        assert_eq!(output.rows[0].right, vec!["1".to_owned(), "ada".to_owned(), "a".to_owned()]);
    }

    #[test]
    fn diff_matches_keys_case_insensitively_and_types_strictly() {
        let left = result(&[("ID", "INTEGER")], vec![vec![Value::Integer(1)]]);
        let right = result(&[("id", "REAL")], vec![vec![Value::Real(1.0)]]);
        let output = diff_tables(&left, &right, &keys(&["id"])).expect("typed diff runs");
        assert_eq!(output.counts.added, 1);
        assert_eq!(output.counts.removed, 1);
        assert_eq!(output.counts.changed, 0);
    }

    #[test]
    fn diff_flags_truncated_inputs() {
        let (mut left, right) = people();
        left = QueryResult::from_columns(
            left.columns().to_vec(),
            true,
            None,
            Duration::from_millis(1),
        )
        .unwrap();
        let output = diff_tables(&left, &right, &keys(&["id"])).expect("partial diff runs");
        assert!(output.truncated);
        let clean = diff_tables(&right, &right, &keys(&["id"])).expect("clean diff runs");
        assert!(!clean.truncated);
    }

    #[test]
    fn select_quotes_each_dialect() {
        assert_eq!(
            compare_select(DbKind::Postgres, "public", "users"),
            "SELECT * FROM \"public\".\"users\""
        );
        assert_eq!(
            compare_select(DbKind::Sqlite, "main", "we\"ird"),
            "SELECT * FROM \"main\".\"we\"\"ird\""
        );
        assert_eq!(
            compare_select(DbKind::Mysql, "cde_test", "users"),
            "SELECT * FROM `cde_test`.`users`"
        );
        assert_eq!(
            compare_select(DbKind::Mysql, "db", "we`ird"),
            "SELECT * FROM `db`.`we``ird`"
        );
    }

    #[test]
    fn keys_split_on_commas_and_drop_blanks() {
        assert_eq!(parse_key_columns(" id, name ,, "), vec!["id".to_owned(), "name".to_owned()]);
        assert!(parse_key_columns(" , ").is_empty());
    }

    #[test]
    fn counts_name_diffs_and_total() {
        let counts = CompareCounts { added: 1, removed: 2, changed: 3, unchanged: 4 };
        assert_eq!(counts.diffs(), 6);
        assert_eq!(counts.total(), 10);
    }
}
