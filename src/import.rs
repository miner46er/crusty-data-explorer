//! CSV, JSONL, and XLSX import: parse a file into headers and rows, map its
//! headers onto a table's columns, and render the INSERT batches.
//!
//! Pure text and bytes, no engine calls. `app.rs` holds the pending draft and
//! sends what this module builds through the same batch path an edit apply
//! uses, reusing [`crate::edit::quote_qualified`], [`crate::edit::ident`], and
//! [`crate::edit::to_literal`] so an import cannot drift from an edit.

use std::io::Cursor;

use calamine::{Data, ExcelDateTime, Reader, Xlsx, open_workbook_from_rs};

use crate::edit::{ident, quote_qualified, to_typed_literal};

/// Data rows beyond this count are dropped from one import.
pub const ROW_CAP: usize = 1000;
/// Value tuples per INSERT statement.
pub const BATCH_ROWS: usize = 50;

/// One parsed file: the header record (for JSONL, the union of its keys) and
/// the data records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCsv {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// One parsed workbook: the worksheet that was read, how many the workbook
/// carries, and the worksheet's records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedXlsx {
    pub sheet: String,
    pub sheet_count: usize,
    pub parsed: ParsedCsv,
}

/// The CSV headers matched onto a table's columns. `pairs` holds
/// `(CSV column index, table column index)` in table column order, and
/// `columns` is the table's column list the second index points into.
/// `skipped` names the headers no column claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMap {
    pub pairs: Vec<(usize, usize)>,
    pub columns: Vec<String>,
    pub skipped: Vec<String>,
}

/// Parse RFC 4180 text: quoted fields, doubled quotes inside them, commas and
/// line breaks inside them, CRLF or LF terminators, and an optional trailing
/// newline. None unless the text carries a header and at least one data
/// record, all of one width. Blank lines are skipped and a leading UTF-8 BOM
/// is dropped, the way exports of spreadsheets arrive.
pub fn parse_csv(text: &str) -> Option<ParsedCsv> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records = parse_records(text);
    if records.len() < 2 {
        return None;
    }
    let headers = records.remove(0);
    if records.iter().any(|record| record.len() != headers.len()) {
        return None;
    }
    Some(ParsedCsv { headers, rows: records })
}

/// The CSV records in order. A final terminator does not open an empty
/// record, a blank line between records is skipped, a quote only opens a
/// field at its start, and an unterminated quote runs to the end.
fn parse_records(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut at_field_start = true;
    let mut pending = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if at_field_start => {
                in_quotes = true;
                at_field_start = false;
                pending = true;
            }
            '"' => {
                field.push('"');
                pending = true;
            }
            ',' => {
                record.push(std::mem::take(&mut field));
                at_field_start = true;
                pending = true;
            }
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                if pending {
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                }
                at_field_start = true;
                pending = false;
            }
            _ => {
                field.push(c);
                at_field_start = false;
                pending = true;
            }
        }
    }
    if pending {
        record.push(field);
        records.push(record);
    }
    records
}

/// Parse JSON Lines: one JSON object per line, blank lines skipped, a leading
/// UTF-8 BOM dropped, CRLF or LF terminators, and an optional trailing
/// newline. The union of keys across objects is the header, in sorted order
/// because the JSON map does not preserve document order; each object
/// is one record. None when a line is not an object or no line carries one,
/// so a JSON array document or a non-JSON line is rejected whole, the way a
/// ragged CSV is. Values render as the text a cell holds: strings bare,
/// numbers and booleans as written, null and missing keys empty, and nested
/// arrays or objects as compact JSON.
pub fn parse_jsonl(text: &str) -> Option<ParsedCsv> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut objects = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let object: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(line).ok()?;
        objects.push(object);
    }
    if objects.is_empty() {
        return None;
    }
    let mut headers: Vec<String> = Vec::new();
    for object in &objects {
        for key in object.keys() {
            if !headers.iter().any(|header| header == key) {
                headers.push(key.clone());
            }
        }
    }
    let rows = objects
        .iter()
        .map(|object| {
            headers
                .iter()
                .map(|key| object.get(key).map(json_cell).unwrap_or_default())
                .collect()
        })
        .collect();
    Some(ParsedCsv { headers, rows })
}

/// One JSON value as the text [`crate::edit::to_literal`] reads: null empty so
/// it writes NULL, everything else the JSON spelling with strings bare.
fn json_cell(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Parse the first worksheet of an XLSX file. The first non-blank row is the
/// header, its trailing blank cells dropped; blank rows are skipped; a record
/// with blank cells at the end pads back to the header's width; a record with
/// a value past the header rejects the workbook whole, the way [`parse_csv`]
/// rejects a record of another width. Cells read as the text a spreadsheet
/// user sees: whole numbers without a decimal tail, booleans `true` or
/// `false`, dates ISO, formula errors in their Excel spelling, blanks empty.
/// None when the bytes are not a readable workbook, the workbook has no
/// worksheet, or the read worksheet has no header plus at least one record.
pub fn parse_xlsx(bytes: &[u8]) -> Option<ParsedXlsx> {
    let mut workbook: Xlsx<Cursor<&[u8]>> = open_workbook_from_rs(Cursor::new(bytes)).ok()?;
    let sheets = workbook.sheet_names();
    let sheet = sheets.first()?.clone();
    let range = workbook.worksheet_range(&sheet).ok()?;
    let mut records: Vec<Vec<String>> = range
        .rows()
        .map(|record| record.iter().map(xlsx_cell).collect())
        .filter(|record: &Vec<String>| record.iter().any(|cell| !cell.is_empty()))
        .collect();
    if records.len() < 2 {
        return None;
    }
    let mut headers = records.remove(0);
    while headers.last().is_some_and(String::is_empty) {
        headers.pop();
    }
    if headers.is_empty() {
        return None;
    }
    let mut rows = Vec::with_capacity(records.len());
    for mut record in records {
        while record.len() > headers.len() && record.last().is_some_and(String::is_empty) {
            record.pop();
        }
        if record.len() > headers.len() {
            return None;
        }
        record.resize(headers.len(), String::new());
        rows.push(record);
    }
    Some(ParsedXlsx { sheet, sheet_count: sheets.len(), parsed: ParsedCsv { headers, rows } })
}

/// One XLSX cell as the text [`crate::edit::to_literal`] reads.
fn xlsx_cell(data: &Data) -> String {
    match data {
        Data::Empty => String::new(),
        Data::String(text) => text.clone(),
        Data::Int(n) => n.to_string(),
        Data::Float(f) => f.to_string(),
        Data::Bool(true) => "true".to_owned(),
        Data::Bool(false) => "false".to_owned(),
        Data::DateTime(dt) => datetime_cell(dt),
        Data::DateTimeIso(text) | Data::DurationIso(text) => text.clone(),
        Data::Error(error) => error.to_string(),
    }
}

/// One Excel date, time, or duration cell as text: a duration as H:MM:SS
/// over its total hours, a value below one day as a time of day, anything
/// else as YYYY-MM-DD with the time of day appended when the value carries
/// one.
fn datetime_cell(dt: &ExcelDateTime) -> String {
    if dt.is_duration() {
        return duration_text(dt.as_f64());
    }
    let (year, month, day, hour, minute, second, milli) = dt.to_ymd_hms_milli();
    if dt.as_f64() < 1.0 {
        return clock_text(hour, minute, second, milli);
    }
    let mut text = format!("{year:04}-{month:02}-{day:02}");
    if (hour, minute, second, milli) != (0, 0, 0, 0) {
        text.push(' ');
        text.push_str(&clock_text(hour, minute, second, milli));
    }
    text
}

/// A duration-format cell as H:MM:SS over the total hours.
fn duration_text(value: f64) -> String {
    let total = (value * 86_400.0).round() as i64;
    let (sign, total) = if total < 0 { ("-", -total) } else { ("", total) };
    format!("{sign}{}:{:02}:{:02}", total / 3600, total % 3600 / 60, total % 60)
}

fn clock_text(hour: u8, minute: u8, second: u8, milli: u16) -> String {
    let base = format!("{hour:02}:{minute:02}:{second:02}");
    if milli == 0 {
        base
    } else {
        format!("{base}.{milli:03}")
    }
}

/// Match CSV headers onto table columns: for each table column in order, the
/// first header equal to it case-insensitively that no earlier column already
/// took. Headers left over are skipped, in CSV order.
pub fn map_headers(headers: &[String], table_columns: &[String]) -> ColumnMap {
    let mut used = vec![false; headers.len()];
    let mut pairs = Vec::new();
    for (column, name) in table_columns.iter().enumerate() {
        let found = headers
            .iter()
            .enumerate()
            .find(|(index, header)| !used[*index] && header.eq_ignore_ascii_case(name));
        if let Some((index, _)) = found {
            used[index] = true;
            pairs.push((index, column));
        }
    }
    let skipped = headers
        .iter()
        .enumerate()
        .filter(|(index, _)| !used[*index])
        .map(|(_, header)| header.clone())
        .collect();
    ColumnMap { pairs, columns: table_columns.to_vec(), skipped }
}

/// The INSERT statements one import sends: each row's mapped cells in table
/// column order, at most [`BATCH_ROWS`] value tuples per statement, and at
/// most [`ROW_CAP`] rows in all. Values render through
/// [`crate::edit::to_literal`] so an imported cell maps exactly like an
/// inserted edit row. No mapped column means no statements.
pub fn insert_batches(table: &str, map: &ColumnMap, rows: &[Vec<String>]) -> Vec<String> {
    insert_typed_batches(table, map, rows, &[])
}

/// [`insert_batches`] with one text flag per table column position: a cell
/// landing in a text column renders through [`crate::edit::to_typed_literal`],
/// so `"007"` keeps its text identity. A missing flag reads as false.
pub fn insert_typed_batches(
    table: &str,
    map: &ColumnMap,
    rows: &[Vec<String>],
    text_cols: &[bool],
) -> Vec<String> {
    if map.pairs.is_empty() {
        return Vec::new();
    }
    let columns = map
        .pairs
        .iter()
        .map(|(_, column)| ident(&map.columns[*column]))
        .collect::<Vec<_>>()
        .join(", ");
    let target = quote_qualified(table);
    let capped = rows.len().min(ROW_CAP);
    rows[..capped]
        .chunks(BATCH_ROWS)
        .map(|chunk| {
            let tuples = chunk
                .iter()
                .map(|row| {
                    let values = map
                        .pairs
                        .iter()
                        .map(|(header, column)| {
                            let is_text = text_cols.get(*column).copied().unwrap_or(false);
                            to_typed_literal(&row[*header], false, is_text)
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("({values})")
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("INSERT INTO {target} ({columns}) VALUES {tuples}")
        })
        .collect()
}

/// Rows one import drops from a file with `row_count` data rows.
pub fn dropped_rows(row_count: usize) -> usize {
    row_count.saturating_sub(ROW_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_xlsxwriter::{Format, Workbook};

    /// One single-sheet workbook, over a closure that writes its cells.
    fn sheet_bytes(build: impl FnOnce(&mut rust_xlsxwriter::Worksheet)) -> Vec<u8> {
        let mut workbook = Workbook::new();
        build(workbook.add_worksheet());
        workbook.save_to_buffer().expect("writes")
    }

    #[test]
    fn parse_handles_quotes_embedded_newlines_and_both_terminators() {
        let parsed = parse_csv("a,b\n\"x,y\",\"say \"\"hi\"\"\"\n").expect("parses");
        assert_eq!(parsed.headers, vec!["a", "b"]);
        assert_eq!(parsed.rows, vec![vec!["x,y".to_owned(), "say \"hi\"".to_owned()]]);

        // LF and CRLF terminators, with and without the trailing newline.
        let lf = parse_csv("a,b\n1,2").expect("lf");
        let crlf = parse_csv("a,b\r\n1,2\r\n").expect("crlf");
        assert_eq!(lf, crlf);
        assert_eq!(lf.rows, vec![vec!["1".to_owned(), "2".to_owned()]]);

        // A line break inside quotes stays in the field, CRLF included.
        let embedded = parse_csv("a,b\n\"line1\nline2\",2\r\n").expect("lf in quotes");
        assert_eq!(embedded.rows[0][0], "line1\nline2");
        let embedded_crlf = parse_csv("a,b\r\n\"line1\r\nline2\",2\r\n").expect("crlf in quotes");
        assert_eq!(embedded_crlf.rows[0][0], "line1\r\nline2");

        // A quoted empty field and quotes inside unquoted text.
        let odd = parse_csv("a,b\n\"\",he\"llo\n").expect("odd");
        assert_eq!(odd.rows, vec![vec![String::new(), "he\"llo".to_owned()]]);

        // Blank lines are skipped, and a leading BOM is dropped.
        let messy = parse_csv("\u{feff}a,b\n1,2\n\n").expect("messy");
        assert_eq!(messy.headers, vec!["a", "b"]);
        assert_eq!(messy.rows.len(), 1);

        // A second record may be wider or narrower than the header: rejected.
        assert_eq!(parse_csv("a,b\n1\n"), None);
        assert_eq!(parse_csv("a,b\n1,2,3\n"), None);
        // Fewer than a header plus one record is rejected.
        assert_eq!(parse_csv(""), None);
        assert_eq!(parse_csv("a,b"), None);
        assert_eq!(parse_csv("a,b\n"), None);
    }

    #[test]
    fn jsonl_lines_union_their_keys_in_first_seen_order() {
        let text = "{\"id\": 1, \"name\": \"ada\", \"note\": null}\n\
                    {\"id\": 2, \"extra\": {\"a\": 1}, \"list\": [1, 2], \"ok\": true}\n";
        let parsed = parse_jsonl(text).expect("parses");
        assert_eq!(parsed.headers, vec!["id", "name", "note", "extra", "list", "ok"]);
        assert_eq!(
            parsed.rows,
            vec![
                vec![
                    "1".to_owned(),
                    "ada".to_owned(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                ],
                vec![
                    "2".to_owned(),
                    String::new(),
                    String::new(),
                    "{\"a\":1}".to_owned(),
                    "[1,2]".to_owned(),
                    "true".to_owned(),
                ],
            ]
        );
    }

    #[test]
    fn jsonl_reads_one_object_with_bom_crlf_and_blank_lines() {
        let parsed = parse_jsonl("\u{feff}{\"a\": \"say \\\"hi\\\"\"}\r\n\r\n").expect("parses");
        assert_eq!(parsed.headers, vec!["a"]);
        assert_eq!(parsed.rows, vec![vec!["say \"hi\"".to_owned()]]);
    }

    #[test]
    fn jsonl_rejects_lines_that_are_not_objects() {
        assert_eq!(parse_jsonl(""), None);
        assert_eq!(parse_jsonl("  \n\n"), None);
        assert_eq!(parse_jsonl("not json"), None);
        assert_eq!(parse_jsonl("[1, 2]"), None);
        assert_eq!(parse_jsonl("{\"a\": 1}\ntrailing"), None);
        assert_eq!(parse_jsonl("\"text\"\n{\"a\": 1}"), None);
    }

    #[test]
    fn mapping_is_case_insensitive_in_table_order_with_skipped_unknowns() {
        let headers = vec![
            "NAME".to_owned(),
            "Id".to_owned(),
            "tenANT".to_owned(),
            "extra".to_owned(),
        ];
        let columns = vec!["tenant".to_owned(), "id".to_owned(), "name".to_owned()];
        let map = map_headers(&headers, &columns);
        assert_eq!(map.pairs, vec![(2, 0), (1, 1), (0, 2)]);
        assert_eq!(map.columns, columns);
        assert_eq!(map.skipped, vec!["extra"]);

        // A table column no header names is left out, not an error.
        let map = map_headers(&["id".to_owned()], &columns);
        assert_eq!(map.pairs, vec![(0, 1)]);
        assert_eq!(map.skipped, Vec::<String>::new());

        // A duplicate header does not claim a column twice.
        let map = map_headers(&["id".to_owned(), "ID".to_owned()], &columns);
        assert_eq!(map.pairs, vec![(0, 1)]);
        assert_eq!(map.skipped, vec!["ID"]);
    }

    #[test]
    fn insert_batches_chunks_120_rows_into_three_statements() {
        let map = ColumnMap {
            pairs: vec![(0, 0)],
            columns: vec!["n".to_owned()],
            skipped: Vec::new(),
        };
        let rows: Vec<Vec<String>> = (0..120).map(|i| vec![i.to_string()]).collect();
        let statements = insert_batches("counts", &map, &rows);
        assert_eq!(statements.len(), 3);
        assert_eq!(statements[0].matches("), (").count(), 49);
        assert_eq!(statements[1].matches("), (").count(), 49);
        assert_eq!(statements[2].matches("), (").count(), 19);
        assert!(statements[0].starts_with("INSERT INTO \"counts\" (\"n\") VALUES (0), (1), "));
        assert!(statements[2].ends_with("(119)"));

        // No mapped column means no statement, and no rows the same.
        let none = ColumnMap {
            pairs: Vec::new(),
            columns: vec!["n".to_owned()],
            skipped: Vec::new(),
        };
        assert!(insert_batches("counts", &none, &rows).is_empty());
        assert!(insert_batches("counts", &map, &[]).is_empty());
    }

    #[test]
    fn insert_batches_exact_text_follows_the_table_column_order() {
        let map = ColumnMap {
            pairs: vec![(1, 0), (2, 1), (0, 2)],
            columns: vec!["tenant".to_owned(), "id".to_owned(), "name".to_owned()],
            skipped: vec!["extra".to_owned()],
        };
        let rows = vec![
            vec!["O'Brien".to_owned(), "acme".to_owned(), "7".to_owned()],
            vec!["Lin".to_owned(), "globex".to_owned(), String::new()],
        ];
        assert_eq!(
            insert_batches("main.items", &map, &rows),
            vec![
                "INSERT INTO \"main\".\"items\" (\"tenant\", \"id\", \"name\") \
                 VALUES ('acme', 7, 'O''Brien'), ('globex', NULL, 'Lin')"
            ]
        );
    }

    #[test]
    fn insert_typed_batches_quote_text_columns_by_position() {
        let map = ColumnMap {
            pairs: vec![(0, 0), (1, 1)],
            columns: vec!["code".to_owned(), "n".to_owned()],
            skipped: Vec::new(),
        };
        let rows = vec![
            vec!["42".to_owned(), "42".to_owned()],
            vec!["007".to_owned(), String::new()],
        ];
        assert_eq!(
            insert_typed_batches("main.items", &map, &rows, &[true, false]),
            vec![
                "INSERT INTO \"main\".\"items\" (\"code\", \"n\") \
                 VALUES ('42', 42), ('007', NULL)"
            ]
        );
        assert_eq!(
            insert_typed_batches("main.items", &map, &rows, &[]),
            vec![
                "INSERT INTO \"main\".\"items\" (\"code\", \"n\") \
                 VALUES (42, 42), ('007', NULL)"
            ]
        );
        let none = ColumnMap {
            pairs: Vec::new(),
            columns: vec!["code".to_owned()],
            skipped: Vec::new(),
        };
        assert!(insert_typed_batches("main.items", &none, &rows, &[true]).is_empty());
    }

    #[test]
    fn rows_over_the_cap_are_truncated_and_counted() {
        let map = ColumnMap {
            pairs: vec![(0, 0)],
            columns: vec!["n".to_owned()],
            skipped: Vec::new(),
        };
        let rows: Vec<Vec<String>> = (0..1005).map(|i| vec![i.to_string()]).collect();
        let statements = insert_batches("counts", &map, &rows);
        assert_eq!(statements.len(), 20);
        assert!(statements[19].contains("(999)"));
        assert!(!statements[19].contains("(1000)"));
        assert_eq!(dropped_rows(rows.len()), 5);
        assert_eq!(dropped_rows(ROW_CAP), 0);
        assert_eq!(dropped_rows(3), 0);
    }

    #[test]
    fn xlsx_reads_the_first_sheet_and_carries_its_name_when_several() {
        let mut workbook = Workbook::new();
        {
            let sheet = workbook.add_worksheet();
            sheet.set_name("Alpha").unwrap();
            sheet.write_string(0, 0, "id").unwrap();
            sheet.write_string(0, 1, "name").unwrap();
            sheet.write_number(1, 0, 1.0).unwrap();
            sheet.write_string(1, 1, "ada").unwrap();
        }
        {
            let sheet = workbook.add_worksheet();
            sheet.set_name("Beta").unwrap();
            sheet.write_string(0, 0, "zzz").unwrap();
        }
        let parsed = parse_xlsx(&workbook.save_to_buffer().unwrap()).expect("parses");
        assert_eq!(parsed.sheet, "Alpha");
        assert_eq!(parsed.sheet_count, 2);
        assert_eq!(parsed.parsed.headers, vec!["id", "name"]);
        assert_eq!(parsed.parsed.rows, vec![vec!["1", "ada"]]);
    }

    #[test]
    fn xlsx_cells_read_like_a_spreadsheet() {
        let date = Format::new().set_num_format("yyyy-mm-dd");
        let bytes = sheet_bytes(|sheet| {
            sheet.write_string(0, 0, "id").unwrap();
            sheet.write_string(0, 1, "name").unwrap();
            sheet.write_string(0, 2, "ok").unwrap();
            sheet.write_string(0, 3, "when").unwrap();
            sheet.write_number(1, 0, 1.0).unwrap();
            sheet.write_string(1, 1, "say \"hi\"").unwrap();
            sheet.write_boolean(1, 2, true).unwrap();
            sheet.write_number_with_format(1, 3, 45293.0, &date).unwrap();
            sheet.write_number(2, 0, 2.5).unwrap();
        });
        let parsed = parse_xlsx(&bytes).expect("parses");
        assert_eq!(parsed.sheet_count, 1);
        assert_eq!(parsed.parsed.headers, vec!["id", "name", "ok", "when"]);
        assert_eq!(
            parsed.parsed.rows,
            vec![
                vec!["1", "say \"hi\"", "true", "2024-01-02"],
                vec!["2.5", "", "", ""],
            ]
        );
    }

    #[test]
    fn xlsx_ragged_records_pad_and_wide_records_reject() {
        let bytes = sheet_bytes(|sheet| {
            sheet.write_string(0, 0, "a").unwrap();
            sheet.write_string(0, 1, "b").unwrap();
            sheet.write_string(0, 2, "c").unwrap();
            sheet.write_number(1, 0, 1.0).unwrap();
            sheet.write_string(1, 1, "x").unwrap();
            sheet.write_number(2, 0, 2.0).unwrap();
        });
        let parsed = parse_xlsx(&bytes).expect("parses");
        assert_eq!(parsed.parsed.headers, vec!["a", "b", "c"]);
        assert_eq!(
            parsed.parsed.rows,
            vec![vec!["1", "x", ""], vec!["2", "", ""]]
        );

        // A value past the header has no column to land in: rejected whole.
        let bytes = sheet_bytes(|sheet| {
            sheet.write_string(0, 0, "a").unwrap();
            sheet.write_string(0, 1, "b").unwrap();
            sheet.write_number(1, 0, 1.0).unwrap();
            sheet.write_string(1, 2, "stray").unwrap();
        });
        assert_eq!(parse_xlsx(&bytes), None);
    }

    #[test]
    fn xlsx_rejects_files_without_a_header_and_a_data_row() {
        assert_eq!(parse_xlsx(b""), None);
        assert_eq!(parse_xlsx(b"not a zip"), None);
        let bytes = sheet_bytes(|sheet| {
            sheet.write_string(0, 0, "only").unwrap();
        });
        assert_eq!(parse_xlsx(&bytes), None);
    }

    #[test]
    fn xlsx_dates_and_durations_read_as_spreadsheet_text() {
        let date = Format::new().set_num_format("yyyy-mm-dd");
        let stamp = Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
        let span = Format::new().set_num_format("[h]:mm:ss");
        let bytes = sheet_bytes(|sheet| {
            sheet.write_string(0, 0, "when").unwrap();
            sheet.write_string(0, 1, "at").unwrap();
            sheet.write_string(0, 2, "over").unwrap();
            sheet.write_number_with_format(1, 0, 45293.0, &date).unwrap();
            sheet.write_number_with_format(1, 1, 45293.5, &stamp).unwrap();
            sheet.write_number_with_format(1, 2, 1.5, &span).unwrap();
        });
        let parsed = parse_xlsx(&bytes).expect("parses");
        assert_eq!(
            parsed.parsed.rows,
            vec![vec!["2024-01-02", "2024-01-02 12:00:00", "36:00:00"]]
        );
    }
}
