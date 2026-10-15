//! Statement splitting for multi-statement scripts.
//!
//! Pure text scanning. A semicolon separates statements only outside single
//! quotes, double quotes, line comments, and block comments. Every engine call
//! runs one statement, so a script is split here before it is sent.

use std::ops::Range;

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
}
