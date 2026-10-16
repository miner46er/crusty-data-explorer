use rust_xlsxwriter::{Workbook, XlsxError};

use crate::db::{QueryResult, Value};
use crate::edit::to_literal;

fn escape_field(raw: &str) -> String {
    if raw.contains(',') || raw.contains('"') || raw.contains('\n') || raw.contains('\r') {
        format!("\"{}\"", raw.replace('"', "\"\""))
    } else {
        raw.to_owned()
    }
}

fn value_to_field(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::Integer(n)) => n.to_string(),
        Some(Value::Real(f)) => f.to_string(),
        Some(Value::Text(s)) => s.clone(),
        Some(Value::Blob(_)) => value.map(|v| v.display(0).into_owned()).unwrap_or_default(),
    }
}

pub fn to_csv(result: &QueryResult, rows: &[usize]) -> String {
    let mut out = String::new();
    let header: Vec<String> = result
        .columns()
        .iter()
        .map(|c| escape_field(c.name()))
        .collect();
    out.push_str(&header.join(","));
    out.push_str("\r\n");
    for &r in rows {
        let mut fields = Vec::with_capacity(result.columns().len());
        for c in 0..result.columns().len() {
            fields.push(escape_field(&value_to_field(result.cell(r, c))));
        }
        out.push_str(&fields.join(","));
        out.push_str("\r\n");
    }
    out
}

fn value_to_json(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "null".to_owned(),
        Some(Value::Integer(n)) => n.to_string(),
        Some(Value::Real(f)) => match serde_json::Number::from_f64(*f) {
            Some(n) => n.to_string(),
            None => "null".to_owned(),
        },
        Some(Value::Text(s)) => serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned()),
        Some(Value::Blob(_)) => {
            let tag = value.map(|v| v.display(0).into_owned()).unwrap_or_default();
            serde_json::to_string(&tag).unwrap_or_else(|_| "\"\"".to_owned())
        }
    }
}

pub fn to_json(result: &QueryResult, rows: &[usize]) -> String {
    if rows.is_empty() {
        return "[]".to_owned();
    }
    let mut out = String::from("[\n");
    for (i, &r) in rows.iter().enumerate() {
        if result.columns().is_empty() {
            out.push_str("  {}");
        } else {
            out.push_str("  {\n");
            for (c, col) in result.columns().iter().enumerate() {
                let key =
                    serde_json::to_string(col.name()).unwrap_or_else(|_| "\"\"".to_owned());
                let val = value_to_json(result.cell(r, c));
                out.push_str(&format!("    {key}: {val}"));
                if c + 1 < result.columns().len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str("  }");
        }
        if i + 1 < rows.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push(']');
    out
}

/// One JSON object per line over `rows`, values rendered exactly like
/// [`to_json`]: `{"id": 1, "name": "a\"b"}`. An empty row slice writes
/// nothing; a result with no columns writes `{}` per row, the way
/// [`to_json`] renders one.
pub fn to_jsonl(result: &QueryResult, rows: &[usize]) -> String {
    let mut out = String::new();
    for &r in rows {
        out.push('{');
        for (c, col) in result.columns().iter().enumerate() {
            if c > 0 {
                out.push_str(", ");
            }
            let key = serde_json::to_string(col.name()).unwrap_or_else(|_| "\"\"".to_owned());
            let val = value_to_json(result.cell(r, c));
            out.push_str(&format!("{key}: {val}"));
        }
        out.push_str("}\n");
    }
    out
}

/// One worksheet, named by the writer's default, with a header row then
/// `rows`, as the bytes of a workbook a spreadsheet app opens. Integers and
/// reals write as numbers, so a spreadsheet sums and sorts them; an integer
/// past the ±2^53 that an f64 holds exactly writes as text to keep its
/// digits. Text and blob tags write as text, empty text and null leave the
/// cell blank. The value model has no date, so a date stored as text stays
/// text.
pub fn to_xlsx(result: &QueryResult, rows: &[usize]) -> Result<Vec<u8>, XlsxError> {
    const EXACT_INT: i64 = 1 << 53;
    let mut workbook = Workbook::new();
    {
        let sheet = workbook.add_worksheet();
        for (c, column) in result.columns().iter().enumerate() {
            sheet.write_string(0, c as u16, column.name())?;
        }
        for (index, &r) in rows.iter().enumerate() {
            let row = index as u32 + 1;
            for c in 0..result.columns().len() {
                let Some(value) = result.cell(r, c) else { continue };
                match value {
                    Value::Null => {}
                    Value::Integer(n) if (-EXACT_INT..=EXACT_INT).contains(n) => {
                        sheet.write_number(row, c as u16, *n as f64)?;
                    }
                    Value::Integer(n) => {
                        sheet.write_string(row, c as u16, n.to_string())?;
                    }
                    Value::Real(f) => {
                        sheet.write_number(row, c as u16, *f)?;
                    }
                    Value::Text(text) => {
                        sheet.write_string(row, c as u16, text.as_str())?;
                    }
                    Value::Blob(_) => {
                        sheet.write_string(row, c as u16, value.display(0).into_owned())?;
                    }
                }
            }
        }
    }
    workbook.save_to_buffer()
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn sql_literal(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "NULL".to_owned(),
        Some(Value::Integer(n)) => n.to_string(),
        Some(Value::Real(f)) => match serde_json::Number::from_f64(*f) {
            Some(n) => n.to_string(),
            None => "NULL".to_owned(),
        },
        Some(Value::Text(s)) => format!("'{}'", s.replace('\'', "''")),
        Some(Value::Blob(_)) => {
            let tag = value.map(|v| v.display(0).into_owned()).unwrap_or_default();
            format!("'{}'", tag.replace('\'', "''"))
        }
    }
}

pub fn to_inserts(table: &str, result: &QueryResult, rows: &[usize]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols: Vec<String> = result.columns().iter().map(|c| quote_ident(c.name())).collect();
    let target = quote_ident(table);
    let mut out = String::new();
    for &r in rows {
        if cols.is_empty() {
            out.push_str(&format!("INSERT INTO {target} DEFAULT VALUES;\n"));
        } else {
            let vals: Vec<String> =
                (0..cols.len()).map(|c| sql_literal(result.cell(r, c))).collect();
            out.push_str(&format!(
                "INSERT INTO {target} ({}) VALUES ({});\n",
                cols.join(", "),
                vals.join(", ")
            ));
        }
    }
    out
}

fn md_raw(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "NULL".to_owned(),
        Some(Value::Integer(n)) => n.to_string(),
        Some(Value::Real(f)) => match serde_json::Number::from_f64(*f) {
            Some(n) => n.to_string(),
            None => "NULL".to_owned(),
        },
        Some(Value::Text(s)) => s.clone(),
        Some(Value::Blob(_)) => {
            value.map(|v| v.display(0).into_owned()).unwrap_or_default()
        }
    }
}

fn escape_md_cell(raw: &str) -> String {
    raw.replace("\r\n", "<br>").replace('\r', "<br>").replace('\n', "<br>").replace('|', "\\|")
}

pub fn to_markdown(result: &QueryResult, rows: &[usize]) -> String {
    let mut out = String::new();
    let header: Vec<String> =
        result.columns().iter().map(|c| escape_md_cell(c.name())).collect();
    out.push_str(&format!("| {} |\n", header.join(" | ")));
    let sep: Vec<String> = result.columns().iter().map(|_| "---".to_owned()).collect();
    out.push_str(&format!("| {} |\n", sep.join(" | ")));
    for &r in rows {
        let cells: Vec<String> =
            (0..result.columns().len()).map(|c| escape_md_cell(&md_raw(result.cell(r, c)))).collect();
        out.push_str(&format!("| {} |\n", cells.join(" | ")));
    }
    out
}

/// One TSV field: tabs and line breaks become spaces so a spreadsheet
/// paste stays rectangular.
fn tsv_field(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if matches!(c, '\t' | '\r' | '\n') { ' ' } else { c }
        })
        .collect()
}

/// Tab-separated values for spreadsheet paste: fields joined with tabs,
/// rows with newlines, and an optional header row. Values render like the
/// CSV export; an empty row list with headers still writes the header.
pub fn to_tsv(result: &QueryResult, rows: &[usize], headers: bool) -> String {
    let mut out = String::new();
    if headers {
        let header: Vec<String> =
            result.columns().iter().map(|c| tsv_field(c.name())).collect();
        out.push_str(&header.join("\t"));
        out.push('\n');
    }
    for &r in rows {
        let mut fields = Vec::with_capacity(result.columns().len());
        for c in 0..result.columns().len() {
            fields.push(tsv_field(&value_to_field(result.cell(r, c))));
        }
        out.push_str(&fields.join("\t"));
        out.push('\n');
    }
    out
}

/// `"col" IN (...)` over one column's values across `rows`. Every value
/// renders through [`to_literal`] so text quotes and numbers stay bare;
/// NULL and empty cells never enter the list. None when the column is out
/// of range, no rows are given, or no value survives.
pub fn to_in_list(result: &QueryResult, rows: &[usize], col: usize) -> Option<String> {
    if col >= result.columns().len() {
        return None;
    }
    let mut items = Vec::new();
    for &r in rows {
        let text = match result.cell(r, col) {
            None | Some(Value::Null) => continue,
            Some(value) => value.display(usize::MAX).into_owned(),
        };
        let literal = to_literal(&text, false);
        if literal == "NULL" {
            continue;
        }
        items.push(literal);
    }
    if items.is_empty() {
        return None;
    }
    Some(format!("{} IN ({})", quote_ident(result.columns()[col].name()), items.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Column;
    use std::time::Duration;

    fn result(columns: Vec<Column>) -> QueryResult {
        QueryResult::from_columns(columns, false, None, Duration::from_millis(1)).unwrap()
    }

    fn col(name: &str, values: Vec<Value>) -> Column {
        Column::new(name.to_owned(), "ANY".to_owned(), values)
    }

    #[test]
    fn header_and_plain_fields_use_crlf() {
        let r = result(vec![
            col("a", vec![Value::Integer(1), Value::Integer(2)]),
            col(
                "b",
                vec![Value::Text("x".to_owned()), Value::Text("y".to_owned())],
            ),
        ]);
        assert_eq!(to_csv(&r, &[0, 1]), "a,b\r\n1,x\r\n2,y\r\n");
    }

    #[test]
    fn commas_are_quoted() {
        let r = result(vec![col("name", vec![Value::Text("a,b".to_owned())])]);
        assert_eq!(to_csv(&r, &[0]), "name\r\n\"a,b\"\r\n");
    }

    #[test]
    fn quotes_are_doubled_and_wrapped() {
        let r = result(vec![col(
            "name",
            vec![Value::Text("say \"hi\"".to_owned())],
        )]);
        assert_eq!(to_csv(&r, &[0]), "name\r\n\"say \"\"hi\"\"\"\r\n");
    }

    #[test]
    fn newlines_are_quoted() {
        let r = result(vec![col(
            "name",
            vec![
                Value::Text("a\nb".to_owned()),
                Value::Text("c\rd\ne".to_owned()),
            ],
        )]);
        assert_eq!(to_csv(&r, &[0, 1]), "name\r\n\"a\nb\"\r\n\"c\rd\ne\"\r\n");
    }

    #[test]
    fn null_maps_to_empty_field() {
        let r = result(vec![
            col("a", vec![Value::Null, Value::Integer(1)]),
            col("b", vec![Value::Text("x".to_owned()), Value::Null]),
        ]);
        assert_eq!(to_csv(&r, &[0, 1]), "a,b\r\n,x\r\n1,\r\n");
    }

    #[test]
    fn blob_maps_to_display_tag() {
        let v = Value::Blob(vec![0x01, 0x02]);
        let tag = v.display(0).into_owned();
        let r = result(vec![col("data", vec![v])]);
        assert_eq!(to_csv(&r, &[0]), format!("data\r\n{tag}\r\n"));
    }

    #[test]
    fn header_names_are_quoted() {
        let r = result(vec![
            col("a,b", vec![Value::Integer(1)]),
            col("plain", vec![Value::Integer(2)]),
        ]);
        assert_eq!(to_csv(&r, &[0]), "\"a,b\",plain\r\n1,2\r\n");
    }

    #[test]
    fn integers_and_reals_map_to_plain_text() {
        let r = result(vec![col("n", vec![Value::Integer(-7), Value::Real(2.5)])]);
        assert_eq!(to_csv(&r, &[0, 1]), "n\r\n-7\r\n2.5\r\n");
    }

    #[test]
    fn text_is_not_truncated() {
        let long = "x".repeat(crate::db::CELL_CAP + 50);
        let r = result(vec![col("t", vec![Value::Text(long.clone())])]);
        assert_eq!(to_csv(&r, &[0]), format!("t\r\n{long}\r\n"));
    }

    #[test]
    fn visible_order_is_respected() {
        let r = result(vec![col(
            "v",
            vec![Value::Integer(3), Value::Integer(1), Value::Integer(2)],
        )]);
        assert_eq!(to_csv(&r, &[1, 2, 0]), "v\r\n1\r\n2\r\n3\r\n");
        assert_eq!(to_csv(&r, &[2, 0]), "v\r\n2\r\n3\r\n");
    }

    #[test]
    fn json_exact_on_typed_sample() {
        let blob = Value::Blob(vec![0x01, 0x02, 0x03]);
        let tag = blob.display(0).into_owned();
        let r = result(vec![
            col("id", vec![Value::Integer(1), Value::Null]),
            col("name", vec![Value::Text("a\"b".to_owned()), Value::Null]),
            col("score", vec![Value::Real(2.5), Value::Integer(3)]),
            col("data", vec![blob, Value::Null]),
        ]);
        let got = to_json(&r, &[0, 1]);
        let want = format!(
            "[\n  {{\n    \"id\": 1,\n    \"name\": \"a\\\"b\",\n    \"score\": 2.5,\n    \"data\": \"{tag}\"\n  }},\n  {{\n    \"id\": null,\n    \"name\": null,\n    \"score\": 3,\n    \"data\": null\n  }}\n]"
        );
        assert_eq!(got, want);
    }

    #[test]
    fn jsonl_exact_lines_on_mixed_values() {
        let blob = Value::Blob(vec![0x01, 0x02, 0x03]);
        let tag = blob.display(0).into_owned();
        let r = result(vec![
            col("id", vec![Value::Integer(1), Value::Null]),
            col("name", vec![Value::Text("a\"b".to_owned()), Value::Null]),
            col("score", vec![Value::Real(2.5), Value::Integer(3)]),
            col("data", vec![blob, Value::Null]),
        ]);
        let got = to_jsonl(&r, &[0, 1]);
        let want = format!(
            "{{\"id\": 1, \"name\": \"a\\\"b\", \"score\": 2.5, \"data\": \"{tag}\"}}\n{{\"id\": null, \"name\": null, \"score\": 3, \"data\": null}}\n"
        );
        assert_eq!(got, want);
        assert_eq!(to_jsonl(&r, &[]), "");
        assert_eq!(
            to_jsonl(&r, &[1]),
            "{\"id\": null, \"name\": null, \"score\": 3, \"data\": null}\n"
        );
    }

    #[test]
    fn jsonl_writes_one_braces_object_per_row_without_columns() {
        let r = result(vec![]);
        assert_eq!(to_jsonl(&r, &[0, 1]), "{}\n{}\n");
    }

    #[test]
    fn inserts_quoting_and_null_exact() {
        let r = result(vec![
            col("id", vec![Value::Integer(1), Value::Null]),
            col("name", vec![Value::Text("O'Brien".to_owned()), Value::Null]),
        ]);
        assert_eq!(
            to_inserts("users", &r, &[1, 0]),
            "INSERT INTO \"users\" (\"id\", \"name\") VALUES (NULL, NULL);\nINSERT INTO \"users\" (\"id\", \"name\") VALUES (1, 'O''Brien');\n"
        );
    }

    #[test]
    fn markdown_header_separator_exact() {
        let r = result(vec![
            col("id", vec![Value::Integer(1), Value::Integer(2)]),
            col(
                "name",
                vec![Value::Text("x".to_owned()), Value::Text("y|z".to_owned())],
            ),
        ]);
        assert_eq!(
            to_markdown(&r, &[0, 1]),
            "| id | name |\n| --- | --- |\n| 1 | x |\n| 2 | y\\|z |\n"
        );
    }

    #[test]
    fn tsv_with_headers_is_tabs_and_newlines() {
        let r = result(vec![
            col("id", vec![Value::Integer(1), Value::Integer(2)]),
            col(
                "name",
                vec![Value::Text("ada".to_owned()), Value::Text("grace".to_owned())],
            ),
        ]);
        assert_eq!(to_tsv(&r, &[0, 1], true), "id\tname\n1\tada\n2\tgrace\n");
    }

    #[test]
    fn tsv_without_headers_skips_the_first_line() {
        let r = result(vec![
            col("id", vec![Value::Integer(1), Value::Integer(2)]),
            col(
                "name",
                vec![Value::Text("ada".to_owned()), Value::Text("grace".to_owned())],
            ),
        ]);
        assert_eq!(to_tsv(&r, &[1, 0], false), "2\tgrace\n1\tada\n");
    }

    #[test]
    fn tsv_flattens_tabs_and_line_breaks_to_spaces() {
        let r = result(vec![col(
            "note",
            vec![Value::Text("a\tb\nc\rd\ne".to_owned()), Value::Null],
        )]);
        assert_eq!(to_tsv(&r, &[0, 1], true), "note\na b c d e\n\n");
    }

    #[test]
    fn in_list_quotes_text_and_leaves_numbers_bare() {
        let r = result(vec![
            col(
                "id",
                vec![Value::Integer(1), Value::Integer(2), Value::Null],
            ),
            col(
                "name",
                vec![
                    Value::Text("O'Brien".to_owned()),
                    Value::Text("42".to_owned()),
                    Value::Null,
                ],
            ),
        ]);
        assert_eq!(to_in_list(&r, &[0, 1, 2], 0), Some("\"id\" IN (1, 2)".to_owned()));
        assert_eq!(
            to_in_list(&r, &[0, 1, 2], 1),
            Some("\"name\" IN ('O''Brien', 42)".to_owned())
        );
    }

    #[test]
    fn in_list_skips_null_and_empty_cells() {
        let r = result(vec![col(
            "name",
            vec![Value::Null, Value::Text(String::new()), Value::Text("ada".to_owned())],
        )]);
        assert_eq!(to_in_list(&r, &[0, 1, 2], 0), Some("\"name\" IN ('ada')".to_owned()));
    }

    #[test]
    fn in_list_is_none_without_a_surviving_value() {
        let r = result(vec![col("name", vec![Value::Null, Value::Null])]);
        assert_eq!(to_in_list(&r, &[0, 1], 0), None);
        assert_eq!(to_in_list(&r, &[], 0), None);
        assert_eq!(to_in_list(&r, &[0], 9), None);
    }

    /// The sheet's cells as calamine reads them back.
    fn xlsx_cells(bytes: &[u8]) -> Vec<Vec<calamine::Data>> {
        use calamine::Reader;
        let mut workbook: calamine::Xlsx<std::io::Cursor<&[u8]>> =
            calamine::open_workbook_from_rs(std::io::Cursor::new(bytes)).expect("reads");
        let name = workbook.sheet_names().remove(0);
        workbook.worksheet_range(&name).expect("sheet").rows().map(<[_]>::to_vec).collect()
    }

    #[test]
    fn xlsx_roundtrips_exact_cells_through_calamine() {
        use calamine::Data;
        let blob = Value::Blob(vec![0x01, 0x02]);
        let tag = blob.display(0).into_owned();
        let r = result(vec![
            col("n", vec![Value::Integer(3), Value::Real(2.5), Value::Null]),
            col(
                "label",
                vec![
                    Value::Text("a,b \"quoted\"".to_owned()),
                    Value::Text("line\nbreak".to_owned()),
                    Value::Null,
                ],
            ),
            col(
                "note",
                vec![
                    Value::Text(String::new()),
                    Value::Null,
                    Value::Text("kept".to_owned()),
                ],
            ),
            col("data", vec![blob, Value::Null, Value::Null]),
        ]);
        let cells = xlsx_cells(&to_xlsx(&r, &[0, 1, 2]).expect("writes"));
        assert_eq!(
            cells[0],
            vec![
                Data::String("n".to_owned()),
                Data::String("label".to_owned()),
                Data::String("note".to_owned()),
                Data::String("data".to_owned()),
            ]
        );
        assert_eq!(
            cells[1],
            vec![
                Data::Float(3.0),
                Data::String("a,b \"quoted\"".to_owned()),
                Data::Empty,
                Data::String(tag),
            ]
        );
        assert_eq!(
            cells[2],
            vec![
                Data::Float(2.5),
                Data::String("line\nbreak".to_owned()),
                Data::Empty,
                Data::Empty,
            ]
        );
        assert_eq!(
            cells[3],
            vec![Data::Empty, Data::Empty, Data::String("kept".to_owned()), Data::Empty]
        );
        assert_eq!(cells.len(), 4);
    }

    #[test]
    fn xlsx_writes_integers_beyond_f64_exactness_as_text() {
        use calamine::Data;
        let r = result(vec![col(
            "n",
            vec![Value::Integer(1 << 53), Value::Integer(9_007_199_254_740_993)],
        )]);
        let cells = xlsx_cells(&to_xlsx(&r, &[0, 1]).expect("writes"));
        assert_eq!(cells[1], vec![Data::Float(9_007_199_254_740_992.0)]);
        assert_eq!(cells[2], vec![Data::String("9007199254740993".to_owned())]);
    }

    #[test]
    fn xlsx_rows_follow_the_given_order() {
        use calamine::Data;
        let r = result(vec![col(
            "v",
            vec![Value::Integer(3), Value::Integer(1), Value::Integer(2)],
        )]);
        let cells = xlsx_cells(&to_xlsx(&r, &[1, 2, 0]).expect("writes"));
        assert_eq!(cells[1], vec![Data::Float(1.0)]);
        assert_eq!(cells[2], vec![Data::Float(2.0)]);
        assert_eq!(cells[3], vec![Data::Float(3.0)]);
    }
}
