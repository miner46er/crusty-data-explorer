use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use rust_xlsxwriter::{Workbook, XlsxError};

use crate::db::{DbError, QueryResult, Value};
use crate::edit::to_literal;

/// One streamed export shape. Every file export rides the same chunked
/// writers the grid copy path uses, so chunked and whole outputs agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    Csv,
    Json,
    Jsonl,
    Xlsx,
    Tsv,
    Markdown,
    Inserts,
}

impl StreamFormat {
    pub fn extension(self) -> &'static str {
        match self {
            StreamFormat::Csv => "csv",
            StreamFormat::Json => "json",
            StreamFormat::Jsonl => "jsonl",
            StreamFormat::Xlsx => "xlsx",
            StreamFormat::Tsv => "tsv",
            StreamFormat::Markdown => "md",
            StreamFormat::Inserts => "sql",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            StreamFormat::Csv => "CSV",
            StreamFormat::Json => "JSON",
            StreamFormat::Jsonl => "JSONL",
            StreamFormat::Xlsx => "XLSX",
            StreamFormat::Tsv => "TSV",
            StreamFormat::Markdown => "Markdown",
            StreamFormat::Inserts => "INSERTs",
        }
    }
}

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

/// The CSV header line with its CRLF, shared by the whole and chunked paths.
pub fn csv_header(result: &QueryResult) -> String {
    let header: Vec<String> =
        result.columns().iter().map(|c| escape_field(c.name())).collect();
    format!("{}\r\n", header.join(","))
}

/// CSV body lines for `rows` with no header, shared by both paths.
pub fn csv_rows(result: &QueryResult, rows: &[usize]) -> String {
    let mut out = String::new();
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

pub fn to_csv(result: &QueryResult, rows: &[usize]) -> String {
    let mut out = csv_header(result);
    out.push_str(&csv_rows(result, rows));
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

fn json_object(result: &QueryResult, r: usize) -> String {
    if result.columns().is_empty() {
        return "  {}".to_owned();
    }
    let mut out = String::from("  {\n");
    for (c, col) in result.columns().iter().enumerate() {
        let key = serde_json::to_string(col.name()).unwrap_or_else(|_| "\"\"".to_owned());
        let val = value_to_json(result.cell(r, c));
        out.push_str(&format!("    {key}: {val}"));
        if c + 1 < result.columns().len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("  }");
    out
}

/// Incremental JSON array writer. The first pushed row opens the array the
/// way [`to_json`] does; an empty stream finishes as `[]`.
#[derive(Debug, Default)]
pub struct JsonStream {
    first: bool,
    empty: bool,
}

impl JsonStream {
    pub fn new() -> Self {
        Self { first: true, empty: true }
    }
    pub fn header(&self) -> String {
        String::new()
    }
    /// Array rows for `rows`, with commas across chunks.
    pub fn push(&mut self, result: &QueryResult, rows: &[usize]) -> String {
        let mut out = String::new();
        for &r in rows {
            if self.first {
                out.push_str("[\n");
                self.first = false;
            } else {
                out.push_str(",\n");
            }
            out.push_str(&json_object(result, r));
            self.empty = false;
        }
        out
    }
    pub fn footer(self) -> String {
        if self.empty { "[]".to_owned() } else { "\n]".to_owned() }
    }
    pub fn is_empty(&self) -> bool {
        self.empty
    }
}

pub fn to_json(result: &QueryResult, rows: &[usize]) -> String {
    if rows.is_empty() {
        return "[]".to_owned();
    }
    let mut stream = JsonStream::new();
    let mut out = stream.push(result, rows);
    out.push_str(&stream.footer());
    out
}

/// One JSONL line per row with no header, shared by both paths.
pub fn jsonl_rows(result: &QueryResult, rows: &[usize]) -> String {
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

/// One JSON object per line over `rows`, values rendered exactly like
/// [`to_json`]: `{"id": 1, "name": "a\"b"}`. An empty row slice writes
/// nothing; a result with no columns writes `{}` per row, the way
/// [`to_json`] renders one.
pub fn to_jsonl(result: &QueryResult, rows: &[usize]) -> String {
    jsonl_rows(result, rows)
}

/// One worksheet, named by the writer's default, with a header row then
/// `rows`, as the bytes of a workbook a spreadsheet app opens. Integers and
/// reals write as numbers, so a spreadsheet sums and sorts them; an integer
/// past the ±2^53 that an f64 holds exactly writes as text to keep its
/// digits. Text and blob tags write as text, empty text and null leave the
/// cell blank. The value model has no date, so a date stored as text stays
/// text.
pub fn to_xlsx(result: &QueryResult, rows: &[usize]) -> Result<Vec<u8>, XlsxError> {
    let mut stream = XlsxStream::new(result)?;
    stream.push(result, rows)?;
    stream.finish()
}

/// Incremental XLSX writer. The header writes once at creation; each page
/// appends its rows in order, so the database pages while the workbook
/// holds the cells it must. Finishing saves the workbook bytes.
pub struct XlsxStream {
    workbook: Workbook,
    next_row: u32,
    ncols: usize,
}

impl XlsxStream {
    pub fn new(result: &QueryResult) -> Result<Self, XlsxError> {
        const EXACT_INT: i64 = 1 << 53;
        let _ = EXACT_INT;
        let mut workbook = Workbook::new();
        let ncols = result.columns().len();
        {
            let sheet = workbook.add_worksheet();
            for (c, column) in result.columns().iter().enumerate() {
                sheet.write_string(0, c as u16, column.name())?;
            }
        }
        Ok(Self { workbook, next_row: 1, ncols })
    }
    /// Append `rows` of `page` in order.
    pub fn push(&mut self, page: &QueryResult, rows: &[usize]) -> Result<(), XlsxError> {
        const EXACT_INT: i64 = 1 << 53;
        let sheet = self.workbook.worksheet_from_index(0)?;
        for &r in rows {
            let row = self.next_row;
            self.next_row += 1;
            for c in 0..self.ncols.min(page.columns().len()) {
                let Some(value) = page.cell(r, c) else { continue };
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
        Ok(())
    }
    pub fn finish(mut self) -> Result<Vec<u8>, XlsxError> {
        self.workbook.save_to_buffer()
    }
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
    inserts_rows(table, result, rows)
}

/// INSERT lines for `rows` with no header, shared by both paths.
pub fn inserts_rows(table: &str, result: &QueryResult, rows: &[usize]) -> String {
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
    raw.replace("\r\n", "<br>")
        .replace(['\r', '\n'], "<br>")
        .replace('|', "\\|")
}

pub fn to_markdown(result: &QueryResult, rows: &[usize]) -> String {
    let mut out = markdown_header(result);
    out.push_str(&markdown_rows(result, rows));
    out
}

/// The Markdown header plus separator, shared by both paths.
pub fn markdown_header(result: &QueryResult) -> String {
    let mut out = String::new();
    let header: Vec<String> =
        result.columns().iter().map(|c| escape_md_cell(c.name())).collect();
    out.push_str(&format!("| {} |\n", header.join(" | ")));
    let sep: Vec<String> = result.columns().iter().map(|_| "---".to_owned()).collect();
    out.push_str(&format!("| {} |\n", sep.join(" | ")));
    out
}

/// Markdown body rows with no header, shared by both paths.
pub fn markdown_rows(result: &QueryResult, rows: &[usize]) -> String {
    let mut out = String::new();
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
        out.push_str(&tsv_header(result));
    }
    out.push_str(&tsv_rows(result, rows));
    out
}

/// The TSV header line, shared by both paths.
pub fn tsv_header(result: &QueryResult) -> String {
    let header: Vec<String> = result.columns().iter().map(|c| tsv_field(c.name())).collect();
    format!("{}\n", header.join("\t"))
}

/// TSV body lines with no header, shared by both paths.
pub fn tsv_rows(result: &QueryResult, rows: &[usize]) -> String {
    let mut out = String::new();
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

/// Every failure a streamed export reports. Database errors pass through;
/// file and spreadsheet errors carry their message.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("export failed: {0}")]
    Io(String),
    #[error("spreadsheet failed: {0}")]
    Xlsx(String),
}

/// Page `fetch` in chunks and write `format` to `path` without holding the
/// full result. Text formats stream through a buffered writer page by page;
/// XLSX pages the database the same way while the workbook holds the cells
/// it must. `table` names the INSERT target and is ignored otherwise.
/// `cancel` stops between pages with a `cancelled` query error and removes
/// the partial file. `progress` sees the total rows written after each page.
pub fn stream_to_file(
    fetch: &mut dyn FnMut(u64, u64) -> Result<QueryResult, DbError>,
    format: StreamFormat,
    table: &str,
    path: &std::path::Path,
    page_size: usize,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> Result<u64, StreamError> {
    if page_size == 0 {
        return Err(StreamError::Db(DbError::Query("page size is zero".to_owned())));
    }
    if format == StreamFormat::Xlsx {
        return stream_xlsx_to_file(fetch, path, page_size, cancel, progress);
    }
    let mut offset: u64 = 0;
    let mut rows: u64 = 0;
    let mut writer: Option<BufWriter<std::fs::File>> = None;
    let mut json = JsonStream::new();
    let mut header_done = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(writer);
            let _ = std::fs::remove_file(path);
            return Err(StreamError::Db(DbError::Query("cancelled".to_owned())));
        }
        let page = fetch(offset, page_size as u64)?;
        if cancel.load(Ordering::Relaxed) {
            drop(writer);
            let _ = std::fs::remove_file(path);
            return Err(StreamError::Db(DbError::Query("cancelled".to_owned())));
        }
        let count = page.row_count() as u64;
        let file = match writer.as_mut() {
            Some(file) => file,
            None => {
                let file = std::fs::File::create(path)
                    .map_err(|e| StreamError::Io(e.to_string()))?;
                writer = Some(BufWriter::new(file));
                writer.as_mut().expect("stream file opened")
            }
        };
        if !header_done {
            let header = match format {
                StreamFormat::Csv => csv_header(&page),
                StreamFormat::Json => json.header(),
                StreamFormat::Jsonl => String::new(),
                StreamFormat::Tsv => tsv_header(&page),
                StreamFormat::Markdown => markdown_header(&page),
                StreamFormat::Inserts => String::new(),
                StreamFormat::Xlsx => unreachable!("xlsx takes its own path"),
            };
            file.write_all(header.as_bytes()).map_err(|e| StreamError::Io(e.to_string()))?;
            header_done = true;
        }
        if count == 0 {
            break;
        }
        let all: Vec<usize> = (0..page.row_count()).collect();
        let chunk = match format {
            StreamFormat::Csv => csv_rows(&page, &all),
            StreamFormat::Json => json.push(&page, &all),
            StreamFormat::Jsonl => jsonl_rows(&page, &all),
            StreamFormat::Tsv => tsv_rows(&page, &all),
            StreamFormat::Markdown => markdown_rows(&page, &all),
            StreamFormat::Inserts => inserts_rows(table, &page, &all),
            StreamFormat::Xlsx => unreachable!("xlsx takes its own path"),
        };
        file.write_all(chunk.as_bytes()).map_err(|e| StreamError::Io(e.to_string()))?;
        rows += count;
        offset += count;
        progress(rows);
        if count < page_size as u64 {
            break;
        }
    }
    if let Some(mut file) = writer {
        if format == StreamFormat::Json {
            file.write_all(json.footer().as_bytes())
                .map_err(|e| StreamError::Io(e.to_string()))?;
        }
        file.flush().map_err(|e| StreamError::Io(e.to_string()))?;
    } else {
        let mut file = BufWriter::new(
            std::fs::File::create(path).map_err(|e| StreamError::Io(e.to_string()))?,
        );
        match format {
            StreamFormat::Csv => {
                file.write_all(b"\r\n").map_err(|e| StreamError::Io(e.to_string()))?;
            }
            StreamFormat::Json => {
                file.write_all(b"[]").map_err(|e| StreamError::Io(e.to_string()))?;
            }
            StreamFormat::Tsv => {
                file.write_all(b"\n").map_err(|e| StreamError::Io(e.to_string()))?;
            }
            StreamFormat::Markdown => {
                file.write_all(b"|  |\n| --- |\n")
                    .map_err(|e| StreamError::Io(e.to_string()))?;
            }
            StreamFormat::Jsonl | StreamFormat::Inserts => {}
            StreamFormat::Xlsx => unreachable!("xlsx takes its own path"),
        }
        file.flush().map_err(|e| StreamError::Io(e.to_string()))?;
    }
    Ok(rows)
}

/// The XLSX leg of [`stream_to_file`]. Pages the database in chunks and
/// appends each page to one workbook, then saves once.
fn stream_xlsx_to_file(
    fetch: &mut dyn FnMut(u64, u64) -> Result<QueryResult, DbError>,
    path: &std::path::Path,
    page_size: usize,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> Result<u64, StreamError> {
    let mut offset: u64 = 0;
    let mut rows: u64 = 0;
    let mut stream: Option<XlsxStream> = None;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(path);
            return Err(StreamError::Db(DbError::Query("cancelled".to_owned())));
        }
        let page = fetch(offset, page_size as u64)?;
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(path);
            return Err(StreamError::Db(DbError::Query("cancelled".to_owned())));
        }
        let count = page.row_count() as u64;
        let all: Vec<usize> = (0..page.row_count()).collect();
        if stream.is_none() {
            stream = Some(
                XlsxStream::new(&page).map_err(|e| StreamError::Xlsx(e.to_string()))?,
            );
        }
        if count == 0 {
            break;
        }
        stream
            .as_mut()
            .expect("xlsx stream opened")
            .push(&page, &all)
            .map_err(|e| StreamError::Xlsx(e.to_string()))?;
        rows += count;
        offset += count;
        progress(rows);
        if count < page_size as u64 {
            break;
        }
    }
    let bytes = stream
        .expect("xlsx stream opened")
        .finish()
        .map_err(|e| StreamError::Xlsx(e.to_string()))?;
    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(path);
        return Err(StreamError::Db(DbError::Query("cancelled".to_owned())));
    }
    std::fs::write(path, bytes).map_err(|e| StreamError::Io(e.to_string()))?;
    Ok(rows)
}

/// Stream one [`crate::db::Session`] query to `path` in pages. `table`
/// names the INSERT target and is ignored otherwise.
#[allow(clippy::too_many_arguments)]
pub fn stream_session_to_file(
    session: &mut crate::db::Session,
    sql: &str,
    format: StreamFormat,
    table: &str,
    path: &std::path::Path,
    page_size: usize,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> Result<u64, StreamError> {
    let mut fetch = |offset: u64, limit: u64| session.query_page(sql, offset, limit);
    stream_to_file(&mut fetch, format, table, path, page_size, cancel, progress)
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

    fn stream_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "cde-stream-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    fn pages_of(result: &QueryResult, size: usize) -> Vec<QueryResult> {
        let mut pages = Vec::new();
        let mut start = 0;
        while start < result.row_count() {
            let end = (start + size).min(result.row_count());
            let cols = result
                .columns()
                .iter()
                .map(|c| {
                    Column::new(
                        c.name().to_owned(),
                        c.type_name().to_owned(),
                        c.values()[start..end].to_vec(),
                    )
                })
                .collect();
            pages.push(
                QueryResult::from_columns(cols, end < result.row_count(), None, Duration::ZERO)
                    .unwrap(),
            );
            start = end;
        }
        if pages.is_empty() {
            pages.push(
                QueryResult::from_columns(
                    result
                        .columns()
                        .iter()
                        .map(|c| {
                            Column::new(
                                c.name().to_owned(),
                                c.type_name().to_owned(),
                                Vec::new(),
                            )
                        })
                        .collect(),
                    false,
                    None,
                    Duration::ZERO,
                )
                .unwrap(),
            );
        }
        pages
    }

    #[test]
    fn csv_chunks_concatenate_to_the_whole() {
        let r = result(vec![
            col("a", vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)]),
            col(
                "b",
                vec![
                    Value::Text("x".to_owned()),
                    Value::Text("y,z".to_owned()),
                    Value::Null,
                ],
            ),
        ]);
        let pages = pages_of(&r, 2);
        let mut out = csv_header(&pages[0]);
        for page in &pages {
            let all: Vec<usize> = (0..page.row_count()).collect();
            out.push_str(&csv_rows(page, &all));
        }
        let all: Vec<usize> = (0..r.row_count()).collect();
        assert_eq!(out, to_csv(&r, &all));
        assert_eq!(out, "a,b\r\n1,x\r\n2,\"y,z\"\r\n3,\r\n");
    }

    #[test]
    fn json_chunks_concatenate_to_the_whole() {
        let r = result(vec![
            col("id", vec![Value::Integer(1), Value::Integer(2), Value::Null]),
            col(
                "name",
                vec![
                    Value::Text("a\"b".to_owned()),
                    Value::Text("c".to_owned()),
                    Value::Null,
                ],
            ),
        ]);
        let pages = pages_of(&r, 2);
        let mut stream = JsonStream::new();
        let mut out = String::new();
        for page in &pages {
            let all: Vec<usize> = (0..page.row_count()).collect();
            out.push_str(&stream.push(page, &all));
        }
        out.push_str(&stream.footer());
        let all: Vec<usize> = (0..r.row_count()).collect();
        assert_eq!(out, to_json(&r, &all));
    }

    #[test]
    fn jsonl_tsv_markdown_and_inserts_chunk_the_same() {
        let r = result(vec![
            col("id", vec![Value::Integer(1), Value::Integer(2)]),
            col(
                "name",
                vec![Value::Text("ada".to_owned()), Value::Text("gr,ace".to_owned())],
            ),
        ]);
        let pages = pages_of(&r, 1);
        let mut jsonl = String::new();
        let mut tsv = tsv_header(&pages[0]);
        let mut md = markdown_header(&pages[0]);
        let mut inserts = String::new();
        for page in &pages {
            let all: Vec<usize> = (0..page.row_count()).collect();
            jsonl.push_str(&jsonl_rows(page, &all));
            tsv.push_str(&tsv_rows(page, &all));
            md.push_str(&markdown_rows(page, &all));
            inserts.push_str(&inserts_rows("users", page, &all));
        }
        let all: Vec<usize> = (0..r.row_count()).collect();
        assert_eq!(jsonl, to_jsonl(&r, &all));
        assert_eq!(tsv, to_tsv(&r, &all, true));
        assert_eq!(md, to_markdown(&r, &all));
        assert_eq!(inserts, to_inserts("users", &r, &all));
        assert_eq!(jsonl, "{\"id\": 1, \"name\": \"ada\"}\n{\"id\": 2, \"name\": \"gr,ace\"}\n");
    }

    #[test]
    fn xlsx_chunks_concatenate_to_the_whole() {
        let r = result(vec![
            col("n", vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)]),
            col(
                "label",
                vec![
                    Value::Text("a".to_owned()),
                    Value::Text("b".to_owned()),
                    Value::Text("c".to_owned()),
                ],
            ),
        ]);
        let pages = pages_of(&r, 2);
        let mut stream = XlsxStream::new(&pages[0]).expect("stream opens");
        for page in &pages {
            let all: Vec<usize> = (0..page.row_count()).collect();
            stream.push(page, &all).expect("pushes");
        }
        let chunked = stream.finish().expect("finishes");
        let all: Vec<usize> = (0..r.row_count()).collect();
        let whole = to_xlsx(&r, &all).expect("writes");
        assert_eq!(xlsx_cells(&chunked), xlsx_cells(&whole));
    }

    #[test]
    fn stream_to_file_writes_csv_in_pages_with_progress() {
        let r = result(vec![
            col(
                "id",
                vec![
                    Value::Integer(1),
                    Value::Integer(2),
                    Value::Integer(3),
                    Value::Integer(4),
                    Value::Integer(5),
                ],
            ),
            col(
                "name",
                vec![
                    Value::Text("a".to_owned()),
                    Value::Text("b".to_owned()),
                    Value::Text("c".to_owned()),
                    Value::Text("d".to_owned()),
                    Value::Text("e".to_owned()),
                ],
            ),
        ]);
        let pages = pages_of(&r, 2);
        let path = stream_path("progress.csv");
        let cancel = AtomicBool::new(false);
        let mut seen = Vec::new();
        let mut fetch = {
            let mut at = 0;
            move |_: u64, _: u64| {
                let page = pages.get(at).cloned().unwrap_or_else(|| pages[0].clone());
                at += 1;
                Ok(page)
            }
        };
        let rows = stream_to_file(
            &mut fetch,
            StreamFormat::Csv,
            "t",
            &path,
            2,
            &cancel,
            &mut |rows| seen.push(rows),
        )
        .expect("streams");
        assert_eq!(rows, 5);
        assert_eq!(seen, vec![2, 4, 5]);
        let text = std::fs::read_to_string(&path).expect("reads");
        let all: Vec<usize> = (0..r.row_count()).collect();
        assert_eq!(text, to_csv(&r, &all));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stream_cancel_stops_and_removes_the_partial_file() {
        let r = result(vec![col(
            "id",
            vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
        )]);
        let pages = pages_of(&r, 1);
        let path = stream_path("cancel.csv");
        let cancel = AtomicBool::new(false);
        let mut fetch = {
            let mut at = 0;
            move |_: u64, _: u64| {
                let page = pages[at].clone();
                at += 1;
                Ok(page)
            }
        };
        let mut progress = |rows: u64| {
            if rows >= 1 {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        };
        let outcome =
            stream_to_file(&mut fetch, StreamFormat::Csv, "t", &path, 1, &cancel, &mut progress);
        assert!(matches!(outcome, Err(StreamError::Db(_))));
        assert!(!path.exists(), "cancel removes the partial file");
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_stream_export_past_cap_writes_every_row() {
        use crate::db::{ConnectionConfig, PostgresSslMode, Session};
        let config = ConnectionConfig::Postgres {
            read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        };
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        session.query("DROP TABLE IF EXISTS cde_stream_probe").expect("drop");
        session
            .query("CREATE TABLE cde_stream_probe (id INT PRIMARY KEY, name TEXT)")
            .expect("create");
        session
            .query(
                "INSERT INTO cde_stream_probe SELECT i, 'row-' || i FROM generate_series(1, 12500) i",
            )
            .expect("seed 12500 rows past the 10000 cap");
        let total = session.count_for("SELECT id FROM cde_stream_probe").expect("counts");
        assert_eq!(total, 12_500);
        let path = stream_path("pg-stream.csv");
        let cancel = AtomicBool::new(false);
        let rows = stream_session_to_file(
            &mut session,
            "SELECT id, name FROM cde_stream_probe ORDER BY id",
            StreamFormat::Csv,
            "cde_stream_probe",
            &path,
            5000,
            &cancel,
            &mut |_| {},
        )
        .expect("streams past the cap");
        assert_eq!(rows, 12_500);
        let text = std::fs::read_to_string(&path).expect("reads");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 12_501, "header plus every row");
        assert_eq!(lines[0], "id,name");
        assert_eq!(lines[1], "1,row-1");
        assert_eq!(lines[12_500], "12500,row-12500");
        session.query("DROP TABLE cde_stream_probe").expect("cleanup");
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_stream_export_past_cap_writes_every_row() {
        use crate::db::{ConnectionConfig, MysqlSslMode, Session};
        let config = ConnectionConfig::Mysql {
            read_only: false,
            host: std::env::var("MYSQLHOST").unwrap_or_else(|_| "127.0.0.1".to_owned()),
            port: std::env::var("MYSQLPORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3306),
            database: std::env::var("MYSQLDATABASE").unwrap_or_else(|_| "mysql".to_owned()),
            user: std::env::var("MYSQLUSER").unwrap_or_else(|_| "root".to_owned()),
            ssl_mode: MysqlSslMode::default(),
            ssh: None,
        };
        let password = std::env::var("MYSQLPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        session.query("DROP TABLE IF EXISTS cde_stream_probe").expect("drop");
        session
            .query("CREATE TABLE cde_stream_probe (id INT PRIMARY KEY, name TEXT)")
            .expect("create");
        for chunk in 0..25 {
            let base = chunk * 500;
            let values: Vec<String> = (1..=500)
                .map(|i| {
                    let id = base + i;
                    format!("({id}, 'row-{id}')")
                })
                .collect();
            session
                .query(&format!("INSERT INTO cde_stream_probe VALUES {}", values.join(",")))
                .expect("seed chunk");
        }
        let total = session.count_for("SELECT id FROM cde_stream_probe").expect("counts");
        assert_eq!(total, 12_500);
        let path = stream_path("mysql-stream.csv");
        let cancel = AtomicBool::new(false);
        let rows = stream_session_to_file(
            &mut session,
            "SELECT id, name FROM cde_stream_probe ORDER BY id",
            StreamFormat::Csv,
            "cde_stream_probe",
            &path,
            5000,
            &cancel,
            &mut |_| {},
        )
        .expect("streams past the cap");
        assert_eq!(rows, 12_500);
        let text = std::fs::read_to_string(&path).expect("reads");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 12_501, "header plus every row");
        assert_eq!(lines[0], "id,name");
        assert_eq!(lines[1], "1,row-1");
        assert_eq!(lines[12_500], "12500,row-12500");
        session.query("DROP TABLE cde_stream_probe").expect("cleanup");
        let _ = std::fs::remove_file(&path);
    }
}
