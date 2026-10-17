//! Statement splitting for multi-statement scripts.
//!
//! Pure text scanning. A semicolon separates statements only outside single
//! quotes, double quotes, line comments, and block comments. Every engine call
//! runs one statement, so a script is split here before it is sent.

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
            b'\'' => i = scan_quote(bytes, i, b'\''),
            b'"' => i = scan_quote(bytes, i, b'"'),
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len()
                    && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/'))
                {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
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
/// scan order. Placeholders inside string literals, quoted identifiers, line
/// comments, and block comments do not count. A `::` cast never counts, and
/// `$n` passes through untouched. Mixing `:name` and `?` in one run is a
/// [`DbError::Config`].
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
                b'\'' => i = scan_quote(bytes, i, b'\''),
                b'"' => i = scan_quote(bytes, i, b'"'),
                b'-' if bytes.get(i + 1) == Some(&b'-') => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i += 2;
                    while i < bytes.len()
                        && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/'))
                    {
                        i += 1;
                    }
                    i = (i + 2).min(bytes.len());
                }
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
/// renders as NULL. String literals, quoted identifiers, and comments copy
/// through untouched, as does a `:name` with no matching param and a `?`
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
            b'\'' | b'"' => {
                let end = scan_quote(bytes, i, bytes[i]);
                out.push_str(&statement[i..end]);
                i = end;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                let mut end = i + 2;
                while end < bytes.len() && bytes[end] != b'\n' {
                    end += 1;
                }
                out.push_str(&statement[i..end]);
                i = end;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut end = i + 2;
                while end < bytes.len()
                    && !(bytes[end] == b'*' && bytes.get(end + 1) == Some(&b'/'))
                {
                    end += 1;
                }
                end = (end + 2).min(bytes.len());
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
fn scan_quote(bytes: &[u8], start: usize, quote: u8) -> usize {
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

/// Statement heads that may add, drop, or rename schema objects, so a run
/// that begins with one is worth a navigator reload when it lands Ok.
const SCHEMA_KEYWORDS: [&str; 6] =
    ["CREATE", "ALTER", "DROP", "RENAME", "ATTACH", "DETACH"];

/// True when any statement in `sql` begins, after whitespace and comments,
/// with one of [`SCHEMA_KEYWORDS`]. Case-insensitive; the word only counts at
/// the statement head, so `SELECT` text mentioning a keyword never does.
pub fn sql_changes_schema(sql: &str) -> bool {
    split_statements(sql).iter().any(|statement| {
        first_word_after_comments(statement).is_some_and(|word| {
            SCHEMA_KEYWORDS.iter().any(|keyword| word.eq_ignore_ascii_case(keyword))
        })
    })
}

/// The leading ASCII word of `statement`, skipping whitespace, `--` line
/// comments, and `/* */` block comments. None when only comments remain or
/// the head is not a word.
fn first_word_after_comments(statement: &str) -> Option<&str> {
    let mut rest = statement;
    loop {
        rest = rest.trim_start();
        if let Some(tail) = rest.strip_prefix("--") {
            rest = tail.split_once('\n').map_or("", |(_, after)| after);
        } else if let Some(tail) = rest.strip_prefix("/*") {
            rest = tail.split_once("*/").map_or("", |(_, after)| after);
        } else {
            break;
        }
    }
    let end = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
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
