//! Statement splitting for multi-statement scripts.
//!
//! Pure text scanning. A semicolon separates statements only outside single
//! quotes, double quotes, backtick-quoted identifiers, `--` and `#` line
//! comments, block comments, and Postgres dollar-quoted strings. Every engine
//! call runs one statement, so a script is split here before it is sent.

use std::ops::Range;

use crate::db::DbError;
use crate::edit::to_literal;

/// Byte ranges of the statements in `sql`, in order. Each range is trimmed of
/// surrounding whitespace and empty pieces are dropped.
pub fn statement_ranges(sql: &str) -> Vec<Range<usize>> {
    let bytes = sql.as_bytes();
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'\'' | b'"' | b'`') => i = scan_quote(bytes, i, quote),
            b'-' if bytes.get(i + 1) == Some(&b'-') => i = line_comment_end(bytes, i + 2),
            b'#' => i = line_comment_end(bytes, i + 1),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = block_comment_end(bytes, i + 2),
            b'$' => i = scan_dollar_quote(bytes, i).unwrap_or(i + 1),
            b';' => {
                if let Some(range) = trim_range(sql, start, i) {
                    ranges.push(range);
                }
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    if let Some(range) = trim_range(sql, start, bytes.len()) {
        ranges.push(range);
    }
    ranges
}

/// Which placeholder spelling a [`Param`] came from. One style per run: a
/// run mixing `:name` and `?` is refused before anything sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderStyle {
    Named,
    Positional,
}

/// One query parameter: a `:name` or `?` placeholder with the raw text the
/// user typed for it. An empty value substitutes as NULL through
/// [`to_literal`]. A `$n` spelling is not a placeholder here; it passes
/// through untouched for the engine's own native binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub style: PlaceholderStyle,
    pub value: String,
}

impl Param {
    /// The text the prompt lists: `:name` or `?1`.
    pub fn display(&self) -> String {
        match self.style {
            PlaceholderStyle::Named => format!(":{}", self.name),
            PlaceholderStyle::Positional => format!("?{}", self.name),
        }
    }
}

/// Parameters across `statements`, in first-appearance order. A repeated
/// `:name` collects once; each `?` collects separately, numbered from 1 in
/// scan order. Placeholders inside string literals, quoted identifiers, `--`
/// and `#` line comments, block comments, and dollar-quoted strings do not
/// count. A `::` cast never counts, and `$n` passes through untouched. Mixing
/// `:name` and `?` in one run is a [`DbError::Config`].
pub fn collect_params(statements: &[String]) -> Result<Vec<Param>, DbError> {
    let mut params = Vec::new();
    let mut positional = 0u32;
    let mut has_named = false;
    let mut has_positional = false;
    for statement in statements {
        let bytes = statement.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                quote @ (b'\'' | b'"' | b'`') => i = scan_quote(bytes, i, quote),
                b'-' if bytes.get(i + 1) == Some(&b'-') => i = line_comment_end(bytes, i + 2),
                b'#' => i = line_comment_end(bytes, i + 1),
                b'/' if bytes.get(i + 1) == Some(&b'*') => i = block_comment_end(bytes, i + 2),
                b'$' => i = scan_dollar_quote(bytes, i).unwrap_or(i + 1),
                b'?' => {
                    has_positional = true;
                    positional += 1;
                    params.push(Param {
                        name: positional.to_string(),
                        style: PlaceholderStyle::Positional,
                        value: String::new(),
                    });
                    i += 1;
                }
                b':' if bytes.get(i + 1).is_some_and(|b| is_name_start(*b))
                    && (i == 0 || bytes[i - 1] != b':') =>
                {
                    let mut end = i + 1;
                    while end < bytes.len() && is_name_char(bytes[end]) {
                        end += 1;
                    }
                    let name = statement[i + 1..end].to_owned();
                    has_named = true;
                    if !params
                        .iter()
                        .any(|p| p.style == PlaceholderStyle::Named && p.name == name)
                    {
                        params.push(Param {
                            name,
                            style: PlaceholderStyle::Named,
                            value: String::new(),
                        });
                    }
                    i = end;
                }
                _ => i += 1,
            }
        }
    }
    if has_named && has_positional {
        return Err(DbError::Config(
            "cannot mix :name and ? placeholders in one run".to_owned(),
        ));
    }
    Ok(params)
}

/// `statement` with placeholders replaced by `params` values rendered through
/// [`to_literal`], so text quotes and numbers stay bare, and an empty value
/// renders as NULL. String literals, quoted identifiers, `--` and `#` line
/// comments, block comments, and dollar-quoted strings copy through
/// untouched, as does a `:name` with no matching param and a `?`
/// past the end of the positional values. Each `?` consumes the next
/// positional param from `*next_pos`, which the caller threads across the
/// run's statements in order, so one counter numbers every `?` in the run.
pub fn substitute(statement: &str, params: &[Param], next_pos: &mut usize) -> String {
    let positional: Vec<&str> = params
        .iter()
        .filter(|p| p.style == PlaceholderStyle::Positional)
        .map(|p| p.value.as_str())
        .collect();
    let bytes = statement.as_bytes();
    let mut out = String::with_capacity(statement.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'\'' | b'"' | b'`') => {
                let end = scan_quote(bytes, i, quote);
                out.push_str(&statement[i..end]);
                i = end;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                let end = line_comment_end(bytes, i + 2);
                out.push_str(&statement[i..end]);
                i = end;
            }
            b'#' => {
                let end = line_comment_end(bytes, i + 1);
                out.push_str(&statement[i..end]);
                i = end;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let end = block_comment_end(bytes, i + 2);
                out.push_str(&statement[i..end]);
                i = end;
            }
            b'$' => {
                let end = scan_dollar_quote(bytes, i).unwrap_or(i + 1);
                out.push_str(&statement[i..end]);
                i = end;
            }
            b'?' => {
                match positional.get(*next_pos) {
                    Some(value) => out.push_str(&to_literal(value, false)),
                    None => out.push('?'),
                }
                *next_pos += 1;
                i += 1;
            }
            b':' if bytes.get(i + 1).is_some_and(|b| is_name_start(*b))
                && (i == 0 || bytes[i - 1] != b':') =>
            {
                let mut end = i + 1;
                while end < bytes.len() && is_name_char(bytes[end]) {
                    end += 1;
                }
                match params.iter().find(|p| {
                    p.style == PlaceholderStyle::Named && p.name.as_str() == &statement[i + 1..end]
                }) {
                    Some(param) => out.push_str(&to_literal(&param.value, false)),
                    None => out.push_str(&statement[i..end]),
                }
                i = end;
            }
            _ => {
                let c = statement[i..].chars().next().expect("non-empty slice");
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

/// First byte of a `:name`: a letter or underscore, never a digit, so `:1`
/// and `:=` are not placeholders.
fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

/// A `:name` continuation byte.
fn is_name_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Scan from an opening quote to just past its closing quote. A doubled quote
/// stays inside the literal, the SQL standard escape.
pub(crate) fn scan_quote(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// Index of the newline ending the line comment whose body starts at `from`,
/// or the end of `bytes` when none follows. `--` and MySQL's `#` share the
/// body scan; only the opening byte count differs.
pub(crate) fn line_comment_end(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

/// Just past the `*/` closing the block comment whose body starts at `from`,
/// or the end of `bytes` when it never closes.
pub(crate) fn block_comment_end(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
        i += 1;
    }
    (i + 2).min(bytes.len())
}

/// Just past the closing delimiter of the Postgres dollar-quoted string
/// opening at `start`, which is `$$` or `$tag$` with a tag that starts like an
/// identifier. None when no such delimiter opens there, so `$1` and a bare `$`
/// stay ordinary text and a `$n` placeholder passes through. Unclosed at the
/// end of the input reads as a string running to that end, the way an
/// unterminated quote does.
pub(crate) fn scan_dollar_quote(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'$') {
        return None;
    }
    let tag_end = match bytes.get(start + 1) {
        Some(&b'$') => start + 1,
        Some(&first) if is_name_start(first) => {
            let mut i = start + 2;
            while i < bytes.len() && is_name_char(bytes[i]) {
                i += 1;
            }
            if bytes.get(i) != Some(&b'$') {
                return None;
            }
            i
        }
        _ => return None,
    };
    let delimiter = &bytes[start..=tag_end];
    let mut i = tag_end + 1;
    while i < bytes.len() {
        if bytes[i..].starts_with(delimiter) {
            return Some(i + delimiter.len());
        }
        i += 1;
    }
    Some(bytes.len())
}

/// Trim whitespace off `start..end`, reporting None when nothing is left.
fn trim_range(sql: &str, start: usize, end: usize) -> Option<Range<usize>> {
    let mut start = start;
    let mut end = end;
    while start < end {
        let c = sql[start..end].chars().next().expect("non-empty slice");
        if !c.is_whitespace() {
            break;
        }
        start += c.len_utf8();
    }
    while end > start {
        let c = sql[start..end].chars().next_back().expect("non-empty slice");
        if !c.is_whitespace() {
            break;
        }
        end -= c.len_utf8();
    }
    (start < end).then_some(start..end)
}

/// Statements in `sql`, trimmed and with empties dropped.
pub fn split_statements(sql: &str) -> Vec<String> {
    statement_ranges(sql)
        .into_iter()
        .map(|range| sql[range].to_owned())
        .collect()
}

/// Statement heads that may add, drop, or rename schema objects.
const SCHEMA_KEYWORDS: [&str; 6] =
    ["CREATE", "ALTER", "DROP", "RENAME", "ATTACH", "DETACH"];

/// True when any statement in `sql` begins, after whitespace and comments,
/// with one of [`SCHEMA_KEYWORDS`].
pub fn sql_changes_schema(sql: &str) -> bool {
    split_statements(sql)
        .iter()
        .any(|statement| statement_changes_schema(statement))
}

/// True when `statement` begins, after whitespace and comments, with one of
/// [`SCHEMA_KEYWORDS`]. Case-insensitive; the word only counts at the
/// statement head, so `SELECT` text mentioning a keyword never does, and the
/// head ends at the first byte that is not ASCII alphanumeric or underscore,
/// so `CREATE_TABLE` is not `CREATE`.
pub fn statement_changes_schema(statement: &str) -> bool {
    let Some(range) = head_word(statement) else { return false };
    let word = &statement[range];
    SCHEMA_KEYWORDS.iter().any(|keyword| word.eq_ignore_ascii_case(keyword))
}

/// Byte range of `statement`'s leading word: the run of ASCII alphanumerics
/// and underscores after leading trivia. None when only comments remain or
/// the head is not a word.
fn head_word(statement: &str) -> Option<Range<usize>> {
    let rest = skip_leading_trivia(statement);
    let start = statement.len() - rest.len();
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then_some(start..start + end)
}

/// `statement` past its leading whitespace, `--` and `#` line comments, and
/// `/* */` block comments.
fn skip_leading_trivia(statement: &str) -> &str {
    let mut rest = statement;
    loop {
        rest = rest.trim_start();
        let bytes = rest.as_bytes();
        if bytes.starts_with(b"--") {
            rest = &rest[line_comment_end(bytes, 2)..];
        } else if bytes.starts_with(b"#") {
            rest = &rest[line_comment_end(bytes, 1)..];
        } else if bytes.starts_with(b"/*") {
            rest = &rest[block_comment_end(bytes, 2)..];
        } else {
            return rest;
        }
    }
}

/// Characters a table-derived title keeps before its ellipsis, the same cap
/// the editor's text fallback uses.
pub const TITLE_CAP: usize = 24;

/// `text` cut to [`TITLE_CAP`] characters, with an ellipsis when it ran over.
pub fn capped_title(text: &str) -> String {
    let mut title: String = text.chars().take(TITLE_CAP).collect();
    if text.chars().count() > TITLE_CAP {
        title.push('…');
    }
    title
}

/// The title for a script whose first statement is a SELECT: the identifier
/// after the statement's top-level FROM, capped like the text fallback. None
/// for anything else, so the caller falls back to the trimmed text: no
/// SELECT head, no FROM outside parentheses, or no identifier after it.
pub fn select_table_title(sql: &str) -> Option<String> {
    let first = split_statements(sql).into_iter().next()?;
    let head = head_word(&first)?;
    let after_head = head.end;
    if !first[head].eq_ignore_ascii_case("select") {
        return None;
    }
    let from = top_level_from(&first[after_head..])?;
    let table = last_identifier(&first[after_head + from + 4..])?;
    if table.is_empty() {
        return None;
    }
    Some(capped_title(&table))
}

/// Byte offset of the first `FROM` word in `text` outside parentheses,
/// skipping strings, quoted identifiers, `--` and `#` line comments, block
/// comments, and dollar-quoted strings. None when there is none.
fn top_level_from(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            quote @ (b'\'' | b'"' | b'`') => index = scan_quote(bytes, index, quote),
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = line_comment_end(bytes, index + 2);
            }
            b'#' => index = line_comment_end(bytes, index + 1),
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = block_comment_end(bytes, index + 2);
            }
            b'$' => index = scan_dollar_quote(bytes, index).unwrap_or(index + 1),
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            byte if depth == 0 && (byte.is_ascii_alphabetic() || byte == b'_') => {
                let start = index;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                if text[start..index].eq_ignore_ascii_case("from") {
                    return Some(start);
                }
            }
            _ => index += 1,
        }
    }
    None
}

/// The last segment of the possibly quoted, possibly dotted identifier at
/// the start of `text`: `users`, `"my table"`, and `` main.`orders` `` all
/// land as their final name.
fn last_identifier(text: &str) -> Option<String> {
    let mut rest = text.trim_start();
    loop {
        let (segment, tail) = read_identifier(rest)?;
        rest = tail.trim_start();
        match rest.strip_prefix('.') {
            Some(after) => rest = after.trim_start(),
            None => return Some(segment),
        }
    }
}

/// One identifier at the start of `text` and the text after it, with the
/// surrounding quotes or backticks removed and a doubled quote inside
/// collapsed to one, the SQL standard escape.
fn read_identifier(text: &str) -> Option<(String, &str)> {
    let bytes = text.as_bytes();
    let quote = match bytes.first().copied() {
        Some(quote @ (b'"' | b'\'' | b'`')) => quote,
        _ => {
            let mut chars = text.char_indices();
            let first = chars.next()?.1;
            if !(first.is_alphabetic() || first == '_') {
                return None;
            }
            let mut end = first.len_utf8();
            for (index, character) in chars {
                if !(character.is_alphanumeric() || character == '_') {
                    break;
                }
                end = index + character.len_utf8();
            }
            return Some((text[..end].to_owned(), &text[end..]));
        }
    };
    let end = scan_quote(bytes, 0, quote);
    let inner_end = if end > 1 && bytes.get(end - 1) == Some(&quote) { end - 1 } else { end };
    let inner = &text[1..inner_end];
    let mut segment = String::with_capacity(inner.len());
    let mut index = 0;
    while index < inner.len() {
        let character = inner[index..].chars().next().expect("non-empty slice");
        segment.push(character);
        index += character.len_utf8();
        if character == char::from(quote) && inner[index..].starts_with(character) {
            index += character.len_utf8();
        }
    }
    Some((segment, &text[end..]))
}

/// Index into [`split_statements`] of the statement under `cursor_byte`. A
/// cursor in the whitespace between statements, or after the last one, belongs
/// to the statement before it; leading whitespace belongs to the first.
pub fn current_statement(sql: &str, cursor_byte: usize) -> Option<usize> {
    let ranges = statement_ranges(sql);
    if ranges.is_empty() {
        return None;
    }
    let cursor = clamp_boundary(sql, cursor_byte);
    if let Some(index) = ranges.iter().position(|r| r.start <= cursor && cursor < r.end) {
        return Some(index);
    }
    if let Some(index) = ranges.iter().rposition(|r| r.end <= cursor) {
        return Some(index);
    }
    Some(0)
}

/// Clamp a byte offset into `sql` and back onto a character boundary.
fn clamp_boundary(sql: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(sql.len());
    while !sql.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semicolons_in_quotes_do_not_split() {
        assert_eq!(
            split_statements("SELECT ';'; SELECT 2"),
            vec!["SELECT ';'", "SELECT 2"]
        );
        assert_eq!(
            split_statements("SELECT \";\" FROM t; SELECT 2"),
            vec!["SELECT \";\" FROM t", "SELECT 2"]
        );
        assert_eq!(
            split_statements("SELECT 'it''s; ok'; SELECT 2"),
            vec!["SELECT 'it''s; ok'", "SELECT 2"]
        );
        assert_eq!(
            split_statements("SELECT \"a\"\"b;c\"; SELECT 2"),
            vec!["SELECT \"a\"\"b;c\"", "SELECT 2"]
        );
    }

    #[test]
    fn semicolons_in_comments_do_not_split() {
        assert_eq!(
            split_statements("SELECT 1 -- a; b\n; SELECT 2"),
            vec!["SELECT 1 -- a; b", "SELECT 2"]
        );
        assert_eq!(
            split_statements("SELECT /* a; b */ 1; SELECT 2"),
            vec!["SELECT /* a; b */ 1", "SELECT 2"]
        );
    }

    #[test]
    fn semicolons_in_backticks_and_hash_comments_do_not_split() {
        assert_eq!(
            split_statements("SELECT `a;b`; SELECT 2"),
            vec!["SELECT `a;b`", "SELECT 2"]
        );
        assert_eq!(
            split_statements("SELECT `a``b;c`; SELECT 2"),
            vec!["SELECT `a``b;c`", "SELECT 2"]
        );
        assert_eq!(
            split_statements("SELECT 1 # a; b\n; SELECT 2"),
            vec!["SELECT 1 # a; b", "SELECT 2"]
        );
    }

    #[test]
    fn semicolons_in_dollar_quotes_do_not_split() {
        assert_eq!(
            split_statements("SELECT $$a;b$$; SELECT 2"),
            vec!["SELECT $$a;b$$", "SELECT 2"]
        );
        assert_eq!(
            split_statements("SELECT $tag$a;b$tag$; SELECT 2"),
            vec!["SELECT $tag$a;b$tag$", "SELECT 2"]
        );
        // A positional parameter is not a dollar quote, so its semicolon splits.
        assert_eq!(
            split_statements("SELECT $1; SELECT 2"),
            vec!["SELECT $1", "SELECT 2"]
        );
        // Unclosed at the end of the script, the quote runs to that end.
        assert_eq!(split_statements("SELECT $$oops"), vec!["SELECT $$oops"]);
    }

    #[test]
    fn dollar_quote_delimiters_need_a_tag_or_a_second_dollar() {
        assert_eq!(scan_dollar_quote(b"$1", 0), None);
        assert_eq!(scan_dollar_quote(b"$", 0), None);
        assert_eq!(scan_dollar_quote(b"a", 0), None);
        assert_eq!(scan_dollar_quote(b"x$ta$", 0), None);
        assert_eq!(scan_dollar_quote(b"$$a$$; rest", 0), Some(5));
        assert_eq!(scan_dollar_quote(b"$q$a;b$q$; rest", 0), Some(9));
        assert_eq!(scan_dollar_quote(b"$q$unclosed", 0), Some(11));
    }

    #[test]
    fn trailing_text_without_a_semicolon_is_a_statement() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2"),
            vec!["SELECT 1", "SELECT 2"]
        );
        assert_eq!(split_statements("SELECT 1;"), vec!["SELECT 1"]);
        assert_eq!(split_statements("  SELECT 1  "), vec!["SELECT 1"]);
        assert_eq!(
            split_statements("SELECT 1;;SELECT 2"),
            vec!["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn empty_input_yields_no_statements() {
        assert!(split_statements("").is_empty());
        assert!(split_statements("   \n\t ").is_empty());
        assert!(split_statements(";").is_empty());
        assert!(split_statements(" ; ; ").is_empty());
        assert!(current_statement("", 0).is_none());
        assert!(current_statement("  ;  ", 2).is_none());
    }

    #[test]
    fn schema_change_classifier_reads_the_leading_keyword() {
        assert!(!sql_changes_schema(""));
        assert!(!sql_changes_schema("SELECT 1"));
        assert!(!sql_changes_schema("EXPLAIN SELECT * FROM t"));
        assert!(sql_changes_schema("create table t (id integer)"));
        assert!(sql_changes_schema("  DROP  TABLE t"));
        assert!(sql_changes_schema("Alter table t add column c text"));
        assert!(sql_changes_schema("RENAME TABLE a TO b"));
        assert!(sql_changes_schema("ATTACH DATABASE 'x.db' AS x"));
        assert!(sql_changes_schema("DETACH DATABASE x"));
    }

    #[test]
    fn schema_change_classifier_scans_every_statement() {
        assert!(sql_changes_schema(
            "SELECT 1;\nCREATE TABLE t (id integer);\nSELECT 2"
        ));
        assert!(!sql_changes_schema("SELECT 1; SELECT 2"));
    }

    #[test]
    fn schema_change_classifier_skips_strings_and_comments() {
        assert!(!sql_changes_schema("SELECT 'create table t'"));
        assert!(!sql_changes_schema("SELECT 1 -- create table t"));
        assert!(!sql_changes_schema("SELECT 1 /* drop table t */"));
        assert!(!sql_changes_schema("-- migrate\nSELECT 1"));
        assert!(sql_changes_schema("-- migrate\nCREATE TABLE t (id integer)"));
        assert!(sql_changes_schema("/* up */ ALTER TABLE t ADD COLUMN c TEXT"));
        assert!(sql_changes_schema("SELECT 1; -- make a table\nDROP TABLE t"));
    }

    #[test]
    fn schema_change_classifier_ends_the_head_word_at_a_non_word_byte() {
        assert!(statement_changes_schema("CREATE TABLE t (id integer)"));
        assert!(statement_changes_schema("\tCREATE"));
        assert!(!statement_changes_schema("CREATE_TABLE t (id integer)"));
        assert!(!statement_changes_schema("creat2 table t"));
        assert!(!statement_changes_schema("__create"));
        assert!(!statement_changes_schema("DROPX TABLE t"));
        assert!(!sql_changes_schema("SELECT 1; DROP_VIEW v"));
    }

    #[test]
    fn table_title_reads_the_first_selects_top_level_from() {
        assert_eq!(
            select_table_title("SELECT id FROM users WHERE id = 1"),
            Some("users".to_owned())
        );
        assert_eq!(select_table_title("select * from Public.Users"), Some("Users".to_owned()));
        assert_eq!(
            select_table_title("SELECT * FROM main.`orders`"),
            Some("orders".to_owned())
        );
        assert_eq!(
            select_table_title("SELECT /* from x */ * FROM users"),
            Some("users".to_owned())
        );
        assert_eq!(
            select_table_title("SELECT 1 -- from audit\nFROM users"),
            Some("users".to_owned())
        );
    }

    #[test]
    fn table_title_ignores_from_inside_parentheses_and_comments() {
        assert_eq!(
            select_table_title("SELECT EXTRACT(YEAR FROM created_at) FROM orders"),
            Some("orders".to_owned())
        );
        assert_eq!(
            select_table_title("SELECT (SELECT count(*) FROM audit_log) AS m FROM users"),
            Some("users".to_owned())
        );
        assert_eq!(select_table_title("SELECT 'from x' FROM users"), Some("users".to_owned()));
        assert_eq!(
            select_table_title("-- pick the table\nSELECT * FROM users"),
            Some("users".to_owned())
        );
    }

    #[test]
    fn table_title_takes_only_the_first_statement_and_unquotes_identifiers() {
        assert_eq!(select_table_title("SELECT 1; DELETE FROM users"), None);
        assert_eq!(
            select_table_title("SELECT * FROM \"my\"\"table\""),
            Some("my\"table".to_owned())
        );
        assert_eq!(select_table_title("SELECT * FROM \"users 2\""), Some("users 2".to_owned()));
        assert_eq!(select_table_title("SELECT * FROM \"\""), None);
    }

    #[test]
    fn table_title_caps_a_long_name_like_the_text_fallback() {
        assert_eq!(
            select_table_title("SELECT * FROM \"a very long table name here\""),
            Some("a very long table name h…".to_owned())
        );
        assert_eq!(capped_title("short"), "short".to_owned());
        assert_eq!(capped_title(&"x".repeat(TITLE_CAP)), "x".repeat(TITLE_CAP));
    }

    #[test]
    fn table_title_is_none_when_the_select_shape_is_missing() {
        assert_eq!(select_table_title(""), None);
        assert_eq!(select_table_title("SELECT 1"), None);
        assert_eq!(select_table_title("SELECTED * FROM users"), None);
        assert_eq!(select_table_title("WITH t AS (SELECT 1) SELECT * FROM t"), None);
        assert_eq!(select_table_title("INSERT INTO users VALUES (1)"), None);
        assert_eq!(select_table_title("SELECT * FROM (SELECT id FROM users) recent"), None);
        assert_eq!(select_table_title("SELECT * FROM t, u"), Some("t".to_owned()));
        assert_eq!(select_table_title("delete from users"), None);
    }

    #[test]
    fn cursor_in_first_middle_and_last_statement() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3";
        assert_eq!(current_statement(sql, 0), Some(0));
        assert_eq!(current_statement(sql, 3), Some(0));
        assert_eq!(current_statement(sql, 12), Some(1));
        assert_eq!(current_statement(sql, 24), Some(2));
        assert_eq!(current_statement(sql, sql.len()), Some(2));
    }

    #[test]
    fn cursor_in_whitespace_between_statements_takes_the_previous() {
        let sql = "SELECT 1;\n\nSELECT 2";
        assert_eq!(current_statement(sql, 9), Some(0));
        assert_eq!(current_statement(sql, 10), Some(0));
        // The semicolon itself belongs to the statement before it.
        assert_eq!(current_statement(sql, 8), Some(0));
        // Leading whitespace belongs to the first statement.
        assert_eq!(current_statement("   SELECT 1", 1), Some(0));
    }

    #[test]
    fn ranges_match_the_split_statements() {
        let sql = "SELECT 1; /* x;y */ SELECT ';' FROM t";
        let ranges = statement_ranges(sql);
        let statements = split_statements(sql);
        assert_eq!(ranges.len(), statements.len());
        for (range, statement) in ranges.iter().zip(statements.iter()) {
            assert_eq!(&sql[range.clone()], statement);
        }
    }

    #[test]
    fn named_repeats_collect_once_in_order() {
        let params =
            collect_params(&["SELECT :b, :a, :b".to_owned(), "SELECT :c".to_owned()])
                .unwrap();
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].name, "b");
        assert_eq!(params[1].name, "a");
        assert_eq!(params[2].name, "c");
        assert!(params.iter().all(|p| p.style == PlaceholderStyle::Named));
        assert!(params.iter().all(|p| p.value.is_empty()));
        assert_eq!(params[0].display(), ":b");
    }

    #[test]
    fn positional_placeholders_number_in_order() {
        let params =
            collect_params(&["SELECT ?, ?".to_owned(), "SELECT ?".to_owned()]).unwrap();
        assert_eq!(params.len(), 3);
        assert!(params.iter().all(|p| p.style == PlaceholderStyle::Positional));
        assert_eq!(params[0].name, "1");
        assert_eq!(params[1].name, "2");
        assert_eq!(params[2].name, "3");
        assert_eq!(params[2].display(), "?3");
    }

    #[test]
    fn placeholders_in_strings_identifiers_and_comments_do_not_count() {
        let params = collect_params(
            &["SELECT ':a', \"?\", x FROM t -- :b\n WHERE y = ? /* ? */".to_owned()],
        )
        .unwrap();
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].display(), "?1");
    }

    #[test]
    fn placeholders_in_backticks_hash_comments_and_dollar_quotes_do_not_count() {
        let params = collect_params(
            &["SELECT `:a`, `?` FROM t # :b ?\n WHERE x = $$?$$ AND y = ?".to_owned()],
        )
        .unwrap();
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].display(), "?1");
        assert!(collect_params(&["SELECT `:a`, $$:b ?$$".to_owned()]).unwrap().is_empty());
        assert!(
            collect_params(&["SELECT $q$:a ?$q$, $1, $2".to_owned()]).unwrap().is_empty()
        );
    }

    #[test]
    fn table_title_reads_past_hash_comments_and_dollar_quotes() {
        assert_eq!(
            select_table_title("SELECT 1 # from x\nFROM users"),
            Some("users".to_owned())
        );
        assert_eq!(
            select_table_title("SELECT $$FROM x$$ FROM users"),
            Some("users".to_owned())
        );
        assert_eq!(
            select_table_title("SELECT * FROM `my; table`"),
            Some("my; table".to_owned())
        );
    }

    #[test]
    fn mixing_named_and_positional_is_a_config_error() {
        let error =
            collect_params(&["SELECT :a".to_owned(), "SELECT ?".to_owned()]).unwrap_err();
        assert!(matches!(error, DbError::Config(_)));
        assert!(error.to_string().contains(":name"), "{error}");
        assert!(error.to_string().contains("?"), "{error}");
        assert!(collect_params(&["SELECT :a, ?".to_owned()]).is_err());
    }

    #[test]
    fn dollar_casts_and_bare_colons_pass_through() {
        assert!(collect_params(&["SELECT $1, $2".to_owned()]).unwrap().is_empty());
        assert!(collect_params(&["SELECT 1::text".to_owned()]).unwrap().is_empty());
        assert!(collect_params(&["SELECT :1, := 2".to_owned()]).unwrap().is_empty());
    }

    fn named_param(name: &str, value: &str) -> Param {
        Param {
            name: name.to_owned(),
            style: PlaceholderStyle::Named,
            value: value.to_owned(),
        }
    }

    #[test]
    fn substitute_renders_values_through_to_literal() {
        let mut next = 0;
        assert_eq!(
            substitute(
                "SELECT * FROM t WHERE name = :name",
                &[named_param("name", "ada")],
                &mut next
            ),
            "SELECT * FROM t WHERE name = 'ada'"
        );
        let mut next = 0;
        assert_eq!(
            substitute("SELECT * FROM t WHERE id = :id", &[named_param("id", "42")], &mut next),
            "SELECT * FROM t WHERE id = 42"
        );
        let mut next = 0;
        assert_eq!(
            substitute("SELECT :v", &[named_param("v", "O'Brien")], &mut next),
            "SELECT 'O''Brien'"
        );
        let mut next = 0;
        assert_eq!(
            substitute("SELECT :v", &[named_param("v", "")], &mut next),
            "SELECT NULL"
        );
    }

    #[test]
    fn substitute_leaves_strings_comments_and_unknowns_alone() {
        let mut next = 0;
        assert_eq!(
            substitute(
                "SELECT ':name', x FROM t -- :name\nWHERE y = :name AND z = :missing",
                &[named_param("name", "ada")],
                &mut next
            ),
            "SELECT ':name', x FROM t -- :name\nWHERE y = 'ada' AND z = :missing"
        );
    }

    #[test]
    fn substitute_leaves_backticks_hash_comments_and_dollar_quotes_alone() {
        let mut next = 0;
        assert_eq!(
            substitute(
                "SELECT `:a`, $$:a ?$$ # :a\n, :b",
                &[named_param("b", "ada")],
                &mut next
            ),
            "SELECT `:a`, $$:a ?$$ # :a\n, 'ada'"
        );
    }

    #[test]
    fn substitute_numbers_positional_in_order() {
        let params = vec![
            Param {
                name: "1".to_owned(),
                style: PlaceholderStyle::Positional,
                value: "ada".to_owned(),
            },
            Param {
                name: "2".to_owned(),
                style: PlaceholderStyle::Positional,
                value: "7".to_owned(),
            },
        ];
        let mut next = 0;
        assert_eq!(substitute("SELECT ?, ?", &params, &mut next), "SELECT 'ada', 7");
        assert_eq!(next, 2);
        let mut next = 0;
        assert_eq!(substitute("SELECT ?", &[], &mut next), "SELECT ?");
        assert_eq!(next, 1);
    }
}
