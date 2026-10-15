//! Pure SQL editor logic with no widget code.
//!
//! `query.rs` owns the `TextEdit` against a plain `String` buffer. This module
//! owns the text transforms that a later hook will call, so every function
//! here is pure and every span is a byte range into the caller's buffer.

use std::ops::Range;

/// Keywords uppercased by [`format_sql`]. Kept small on purpose.
const KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "GROUP", "BY", "ORDER", "HAVING", "LIMIT", "OFFSET", "DISTINCT",
    "ALL", "AS", "AND", "OR", "NOT", "IN", "IS", "NULL", "LIKE", "BETWEEN", "EXISTS", "JOIN",
    "INNER", "LEFT", "RIGHT", "FULL", "OUTER", "CROSS", "ON", "UNION", "EXCEPT", "INTERSECT",
    "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "WITH", "CASE", "WHEN", "THEN", "ELSE",
    "END", "ASC", "DESC", "CREATE", "TABLE", "DROP", "ALTER", "ADD", "PRIMARY", "KEY", "FOREIGN",
    "REFERENCES", "CONSTRAINT", "DEFAULT", "CHECK", "UNIQUE", "INDEX", "VIEW",
];

/// Uppercase known keywords and put each major clause on its own line.
///
/// Strings and comments pass through byte for byte, so keywords inside them
/// are never touched. Running the output through again changes nothing.
pub fn format_sql(sql: &str) -> String {
    let tokens = tokenize(sql);
    let mut out = String::with_capacity(sql.len());
    let mut gap = false;
    let mut i = 0;
    while i < tokens.len() {
        if let Some((text, taken)) = clause_at(&tokens, i) {
            if !out.is_empty() && !out.ends_with('(') {
                out.push('\n');
            }
            out.push_str(&text);
            gap = true;
            i += taken;
            continue;
        }
        match &tokens[i] {
            Tok::Word(w) => {
                if gap {
                    out.push(' ');
                }
                out.push_str(&upper_if_keyword(w));
                gap = true;
            }
            Tok::Num(n) => {
                if gap {
                    out.push(' ');
                }
                out.push_str(n);
                gap = true;
            }
            Tok::Literal(s) | Tok::Comment(s) => {
                if gap {
                    out.push(' ');
                }
                out.push_str(s);
                gap = true;
            }
            Tok::Op(op) => match op.as_str() {
                "," => {
                    out.push(',');
                    gap = true;
                }
                ";" => {
                    out.push(';');
                    if i + 1 < tokens.len() {
                        out.push('\n');
                    }
                    gap = false;
                }
                "(" => {
                    let prev = last_significant(&tokens, i);
                    let glued = matches!(prev, Some(Tok::Word(w)) if !is_keyword(w))
                        || matches!(prev, Some(Tok::Num(_)))
                        || matches!(prev, Some(Tok::Op(o)) if o == ")");
                    if gap && !glued {
                        out.push(' ');
                    }
                    out.push('(');
                    gap = false;
                }
                ")" => {
                    out.push(')');
                    gap = true;
                }
                "." => {
                    out.push('.');
                    gap = false;
                }
                _ => {
                    if gap {
                        out.push(' ');
                    }
                    out.push_str(op);
                    gap = true;
                }
            },
        }
        i += 1;
    }
    out
}

/// Search state for the editor find box. Holds the pending query text.
#[derive(Debug, Clone, Default)]
pub struct FindState {
    /// Current find query as typed by the user.
    pub query: String,
}

impl FindState {
    /// Wrap a query string.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
        }
    }

    /// Find every non-overlapping case-insensitive hit of `query` in `text`.
    ///
    /// Ranges are byte ranges into `text`. Empty queries match nothing and the
    /// result is capped at 500 hits so a one-letter query stays cheap.
    pub fn find_all(text: &str, query: &str) -> Vec<Range<usize>> {
        const CAP: usize = 500;
        let needle: Vec<char> = lowercase_chars(query);
        if needle.is_empty() {
            return Vec::new();
        }
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let lowered: Vec<Vec<char>> = chars
            .iter()
            .map(|(_, c)| c.to_lowercase().collect())
            .collect();
        let mut hits = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            if hits.len() >= CAP {
                break;
            }
            if match_at(&lowered, i, &needle) {
                let start = chars[i].0;
                let end = if i + needle.len() < chars.len() {
                    chars[i + needle.len()].0
                } else {
                    text.len()
                };
                hits.push(start..end);
                i += needle.len();
            } else {
                i += 1;
            }
        }
        hits
    }

    /// Find every hit of this state's query in `text`.
    pub fn find(&self, text: &str) -> Vec<Range<usize>> {
        Self::find_all(text, &self.query)
    }
}

/// Highlight kind for one tagged span. Gaps between spans are plain text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A SQL keyword such as `SELECT`.
    Keyword,
    /// A quoted `'...'`, `"..."`, or backtick literal.
    Str,
    /// A `--` or `/* ... */` comment.
    Comment,
    /// A numeric literal.
    Number,
}

/// One tagged byte range for later paint use. Sorted and non-overlapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Start byte in the line.
    pub start: usize,
    /// End byte in the line, exclusive.
    pub end: usize,
    /// What the range is.
    pub kind: Kind,
}

/// Tag keyword, string, comment, and number spans in one line.
///
/// Only tagged spans are returned. Everything else is plain text the painter
/// draws normally. Quotes win over keywords, and `--` wins over nothing once
/// a quote is open, because scanning respects literal boundaries.
pub fn highlight_kinds(line: &str) -> Vec<Span> {
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            spans.push(Span {
                start: i,
                end: bytes.len(),
                kind: Kind::Comment,
            });
            break;
        }
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            while i < bytes.len()
                && !(bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/')
            {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            spans.push(Span {
                start,
                end: i,
                kind: Kind::Comment,
            });
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            let start = i;
            i = scan_quoted(bytes, i);
            spans.push(Span {
                start,
                end: i,
                kind: Kind::Str,
            });
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'.' {
                let dot = i;
                i += 1;
                if i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                    while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                        i += 1;
                    }
                } else {
                    i = dot;
                }
            }
            spans.push(Span {
                start,
                end: i,
                kind: Kind::Number,
            });
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < bytes.len() && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_')
            {
                i += 1;
            }
            if is_keyword(&line[start..i]) {
                spans.push(Span {
                    start,
                    end: i,
                    kind: Kind::Keyword,
                });
            }
            continue;
        }
        i += 1;
    }
    spans
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(String),
    Num(String),
    Op(String),
    Literal(String),
    Comment(String),
}

fn tokenize(sql: &str) -> Vec<Tok> {
    let bytes = sql.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            toks.push(Tok::Comment(sql[start..i].to_owned()));
            continue;
        }
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            while i < bytes.len()
                && !(bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/')
            {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            toks.push(Tok::Comment(sql[start..i].to_owned()));
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            let start = i;
            i = scan_quoted(bytes, i);
            toks.push(Tok::Literal(sql[start..i].to_owned()));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < bytes.len()
                && ((bytes[i] as char).is_ascii_alphanumeric()
                    || bytes[i] == b'_'
                    || bytes[i] == b'$')
            {
                i += 1;
            }
            toks.push(Tok::Word(sql[start..i].to_owned()));
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                i += 1;
            }
            if i + 1 < bytes.len() && bytes[i] == b'.' && (bytes[i + 1] as char).is_ascii_digit() {
                i += 1;
                while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                    i += 1;
                }
            }
            toks.push(Tok::Num(sql[start..i].to_owned()));
            continue;
        }
        if i + 1 < bytes.len() {
            let two = &sql[i..i + 2];
            if matches!(two, "<=" | ">=" | "!=" | "<>" | "||" | "::") {
                toks.push(Tok::Op(two.to_owned()));
                i += 2;
                continue;
            }
        }
        toks.push(Tok::Op(c.to_string()));
        i += 1;
    }
    toks
}

fn scan_quoted(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            if quote == b'\'' && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

fn is_keyword(word: &str) -> bool {
    KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(word))
}

fn upper_if_keyword(word: &str) -> String {
    if is_keyword(word) {
        word.to_ascii_uppercase()
    } else {
        word.to_owned()
    }
}

fn word_at(tokens: &[Tok], i: usize) -> Option<&str> {
    match tokens.get(i) {
        Some(Tok::Word(w)) => Some(w),
        _ => None,
    }
}

fn word_is(tokens: &[Tok], i: usize, kw: &str) -> bool {
    word_at(tokens, i).is_some_and(|w| w.eq_ignore_ascii_case(kw))
}

fn last_significant(tokens: &[Tok], before: usize) -> Option<&Tok> {
    tokens[..before].iter().last()
}

/// If tokens[i..] starts a major clause, return its display text and length.
fn clause_at(tokens: &[Tok], i: usize) -> Option<(String, usize)> {
    let w = word_at(tokens, i)?;
    let up = w.to_ascii_uppercase();
    match up.as_str() {
        "SELECT" | "FROM" | "WHERE" | "HAVING" | "LIMIT" | "OFFSET" | "WITH" | "UPDATE"
        | "SET" | "VALUES" | "EXCEPT" | "INTERSECT" => Some((up, 1)),
        "GROUP" if word_is(tokens, i + 1, "BY") => Some(("GROUP BY".to_owned(), 2)),
        "ORDER" if word_is(tokens, i + 1, "BY") => Some(("ORDER BY".to_owned(), 2)),
        "UNION" => {
            if word_is(tokens, i + 1, "ALL") || word_is(tokens, i + 1, "DISTINCT") {
                Some((
                    format!("UNION {}", tokens[i + 1].clone().into_word().to_ascii_uppercase()),
                    2,
                ))
            } else {
                Some((up, 1))
            }
        }
        "INSERT" => {
            if word_is(tokens, i + 1, "INTO") {
                Some(("INSERT INTO".to_owned(), 2))
            } else {
                Some((up, 1))
            }
        }
        "DELETE" => {
            if word_is(tokens, i + 1, "FROM") {
                Some(("DELETE FROM".to_owned(), 2))
            } else {
                Some((up, 1))
            }
        }
        "INNER" | "LEFT" | "RIGHT" | "FULL" | "CROSS" => {
            let mut j = i + 1;
            if word_is(tokens, j, "OUTER") {
                j += 1;
            }
            if word_is(tokens, j, "JOIN") {
                let text: Vec<String> =
                    tokens[i..=j].iter().map(|t| t.clone().into_word().to_ascii_uppercase()).collect();
                Some((text.join(" "), j - i + 1))
            } else {
                None
            }
        }
        "JOIN" => Some((up, 1)),
        _ => None,
    }
}

fn lowercase_chars(s: &str) -> Vec<char> {
    s.chars().flat_map(|c| c.to_lowercase()).collect()
}

fn match_at(hay: &[Vec<char>], start: usize, needle: &[char]) -> bool {
    if start + needle.len() > hay.len() {
        return false;
    }
    hay[start..start + needle.len()]
        .iter()
        .zip(needle.iter())
        .all(|(h, n)| h.iter().collect::<String>() == n.to_string())
}

impl Tok {
    fn into_word(self) -> String {
        match self {
            Tok::Word(w) => w,
            other => panic!("expected word, found {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_is_idempotent_on_select_sample() {
        let once = format_sql("select id, name from users where age > 30 order by name limit 5");
        assert_eq!(
            once,
            "SELECT id, name\nFROM users\nWHERE age > 30\nORDER BY name\nLIMIT 5"
        );
        assert_eq!(once, format_sql(&once));
    }

    #[test]
    fn format_is_idempotent_on_join_sample() {
        let once = format_sql(
            "select d.name, count(*) as n from departments d left join employees e \
             on e.dept = d.id group by d.name having count(*) > 1",
        );
        assert_eq!(
            once,
            "SELECT d.name, count(*) AS n\nFROM departments d\n\
             LEFT JOIN employees e ON e.dept = d.id\n\
             GROUP BY d.name\nHAVING count(*) > 1"
        );
        assert_eq!(once, format_sql(&once));
    }

    #[test]
    fn find_ranges_are_exact_case_insensitive() {
        assert_eq!(
            FindState::find_all("Select sel SELECT", "sel"),
            vec![0..3, 7..10, 11..14]
        );
        assert!(FindState::find_all("select 1", "").is_empty());
    }

    #[test]
    fn find_caps_at_five_hundred() {
        let text = "a ".repeat(1000);
        assert_eq!(FindState::find_all(&text, "a").len(), 500);
    }

    #[test]
    fn highlight_tags_keyword_and_string() {
        let spans = highlight_kinds("SELECT 'hi' -- note");
        assert!(spans.contains(&Span {
            start: 0,
            end: 6,
            kind: Kind::Keyword,
        }));
        assert!(spans.contains(&Span {
            start: 7,
            end: 11,
            kind: Kind::Str,
        }));
        assert!(spans.contains(&Span {
            start: 12,
            end: 19,
            kind: Kind::Comment,
        }));
    }
}
