//! Inline editing: which SELECTs target one table, and how a staged change
//! renders as SQL. The schema tree's preview form, a
//! `SELECT * FROM "schema"."table" LIMIT 100`, is editable too.
//!
//! Pure text, no engine calls. `app.rs` decides when an edit may be staged
//! and sends the statements this module builds.

use crate::db::DbKind;
use crate::sql::hash_comments;

/// Some(table name) only for a single-table SELECT: a plain, double-quoted,
/// or backtick-quoted identifier after FROM, optionally one more of either
/// kind joined by a single unquoted dot, with WHERE, GROUP, ORDER, HAVING,
/// LIMIT, or OFFSET allowed to follow it. Joins, unions, comma sources,
/// parenthesized FROM clauses, aliases, deeper qualifiers, and anything but
/// a SELECT are not editable. The name joins its parts with a dot; a part
/// containing a dot or quote keeps its quotes.
pub fn editable_table(sql: &str, kind: DbKind) -> Option<String> {
    let tokens = lex(sql, kind);
    if !matches!(tokens.first(), Some(Token::Word(w)) if w.eq_ignore_ascii_case("select")) {
        return None;
    }
    let mut index = 1;
    let (table, next) = loop {
        match tokens.get(index) {
            Some(Token::Word(word)) if word.eq_ignore_ascii_case("from") => {
                let (first, after_first) = identifier_at(&tokens, index + 1)?;
                match tokens.get(after_first) {
                    Some(Token::Punct('.')) => {
                        let (second, after_second) = identifier_at(&tokens, after_first + 1)?;
                        // One qualifier only: a further dot is a deeper path.
                        if matches!(tokens.get(after_second), Some(Token::Punct('.'))) {
                            return None;
                        }
                        break (canonical_name(&first, Some(&second)), after_second);
                    }
                    _ => break (canonical_name(&first, None), after_first),
                }
            }
            Some(_) => index += 1,
            None => return None,
        }
    };
    match tokens.get(next) {
        None => Some(table),
        Some(Token::Punct(';')) if next + 1 == tokens.len() => Some(table),
        Some(Token::Word(word)) if is_trailing_clause(word) => Some(table),
        Some(_) => None,
    }
}

/// One plain, double-quoted, or backtick-quoted identifier at `index`, with
/// the index after it.
fn identifier_at(tokens: &[Token], index: usize) -> Option<(String, usize)> {
    match tokens.get(index) {
        Some(Token::Word(name)) | Some(Token::Quoted(name)) => Some((name.clone(), index + 1)),
        _ => None,
    }
}

/// Join one or two name parts with a dot, quoting a part whose content
/// contains a dot or quote so the returned name re-splits on the same parts.
fn canonical_name(first: &str, second: Option<&str>) -> String {
    match second {
        Some(second) => format!("{}.{}", name_part(first), name_part(second)),
        None => name_part(first),
    }
}

fn name_part(name: &str) -> String {
    if name.contains('.') || name.contains('"') {
        ident(name)
    } else {
        name.to_owned()
    }
}

/// The clause keywords a single-table SELECT may continue with.
fn is_trailing_clause(word: &str) -> bool {
    ["where", "group", "order", "having", "limit", "offset"]
        .iter()
        .any(|keyword| word.eq_ignore_ascii_case(keyword))
}

/// One source table behind a SELECT: its canonical name and its optional
/// alias as written. The alias is the qualifier result columns use when the
/// query aliases its tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinSource {
    pub table: String,
    pub alias: Option<String>,
}

/// One SELECT list item as a column mapping: an optional qualifier and the
/// source column. None column is an unresolvable item: `*`, `table.*`, a
/// function, an expression, or anything but `[qualifier.]column` with an
/// optional `AS` alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinSelect {
    pub qualifier: Option<String>,
    pub column: Option<String>,
}

/// The parsed shape a joined SELECT edits: every source table and every
/// SELECT item in order, so a result column maps positionally to its item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinPlan {
    pub sources: Vec<JoinSource>,
    pub selects: Vec<JoinSelect>,
}

/// Parse a SELECT into its source tables and SELECT items. Some for a plain
/// SELECT with at least one source and one item; None for anything else:
/// unions, subquery sources, deeper qualifiers, or multiple statements. Each
/// item parses on its own, so one expression marks only its own column
/// unresolvable. Pure text, no engine calls.
pub fn parse_join(sql: &str, kind: DbKind) -> Option<JoinPlan> {
    let tokens = lex(sql, kind);
    if !matches!(tokens.first(), Some(Token::Word(w)) if w.eq_ignore_ascii_case("select")) {
        return None;
    }
    let from = top_level_from(&tokens)?;
    let selects = parse_select_list(&tokens[1..from])?;
    if selects.is_empty() {
        return None;
    }
    let sources = parse_sources(&tokens[from + 1..])?;
    if sources.is_empty() {
        return None;
    }
    Some(JoinPlan { sources, selects })
}

/// True when the SELECT list is exactly `*` or `table.*`, the forms whose
/// result columns read positionally from the one source table. A function or
/// arithmetic item is not a wildcard even though its parsed column is None.
pub fn wildcard_select(sql: &str, kind: DbKind) -> bool {
    let tokens = lex(sql, kind);
    let Some(from) = top_level_from(&tokens) else { return false };
    let mut list = &tokens[1..from];
    if matches!(
        list.first(),
        Some(Token::Word(w)) if w.eq_ignore_ascii_case("distinct") || w.eq_ignore_ascii_case("all")
    ) {
        list = &list[1..];
    }
    matches!(
        list,
        [Token::Punct('*')]
            | [Token::Word(_) | Token::Quoted(_), Token::Punct('.'), Token::Punct('*')]
    )
}

/// Index of the top-level FROM, so a subquery's FROM never reads as the
/// outer one.
fn top_level_from(tokens: &[Token]) -> Option<usize> {
    let mut depth: usize = 0;
    for (index, token) in tokens.iter().enumerate().skip(1) {
        match token {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => depth = depth.saturating_sub(1),
            Token::Word(w) if depth == 0 && w.eq_ignore_ascii_case("from") => return Some(index),
            _ => {}
        }
    }
    None
}

/// Every SELECT item in order. None when a comma split is empty.
fn parse_select_list(tokens: &[Token]) -> Option<Vec<JoinSelect>> {
    let mut rest = tokens;
    if matches!(rest.first(), Some(Token::Word(w)) if w.eq_ignore_ascii_case("distinct") || w.eq_ignore_ascii_case("all"))
    {
        rest = &rest[1..];
    }
    let mut items = Vec::new();
    let mut depth: usize = 0;
    let mut start = 0;
    let mut index = 0;
    while index <= rest.len() {
        let boundary = index == rest.len()
            || (depth == 0 && matches!(rest.get(index), Some(Token::Punct(','))));
        match rest.get(index) {
            Some(Token::Punct('(')) => depth += 1,
            Some(Token::Punct(')')) => depth = depth.saturating_sub(1),
            _ => {}
        }
        if boundary {
            if index > start {
                items.push(parse_select_item(&rest[start..index]));
            } else if index == rest.len() && items.is_empty() {
                return None;
            } else {
                return None;
            }
            start = index + 1;
        }
        index += 1;
    }
    Some(items)
}

/// One SELECT item as an optional qualifier plus an optional column. Only
/// `[qualifier.]column` with an optional `AS` alias resolves; everything
/// else keeps None so its result column stays read-only.
fn parse_select_item(chunk: &[Token]) -> JoinSelect {
    let unresolvable = JoinSelect { qualifier: None, column: None };
    if chunk.is_empty() {
        return unresolvable;
    }
    if chunk.len() == 1 && matches!(chunk[0], Token::Punct('*')) {
        return unresolvable;
    }
    let (first, after_first) = match identifier_at(chunk, 0) {
        Some(pair) => pair,
        None => return unresolvable,
    };
    let (qualifier, column, after_col) = match chunk.get(after_first) {
        Some(Token::Punct('.')) => match chunk.get(after_first + 1) {
            Some(Token::Punct('*')) => return JoinSelect {
                qualifier: Some(first),
                column: None,
            },
            Some(_) => match identifier_at(chunk, after_first + 1) {
                Some((second, after_second)) => {
                    if matches!(chunk.get(after_second), Some(Token::Punct('.'))) {
                        return unresolvable;
                    }
                    (Some(first), Some(second), after_second)
                }
                None => return unresolvable,
            },
            None => return unresolvable,
        },
        _ => (None, Some(first), after_first),
    };
    match chunk.get(after_col) {
        None => JoinSelect { qualifier, column },
        Some(Token::Word(w)) if after_col + 1 == chunk.len() && !is_join_reserved(w) => {
            JoinSelect { qualifier, column }
        }
        Some(Token::Quoted(_)) if after_col + 1 == chunk.len() => JoinSelect { qualifier, column },
        Some(Token::Word(w))
            if w.eq_ignore_ascii_case("as")
                && after_col + 2 == chunk.len()
                && matches!(chunk.get(after_col + 1), Some(Token::Word(_)) | Some(Token::Quoted(_))) =>
        {
            JoinSelect { qualifier, column }
        }
        _ => unresolvable,
    }
}

/// Whether a word can never be a bare table alias in a FROM clause.
fn is_join_reserved(word: &str) -> bool {
    [
        "where",
        "group",
        "order",
        "having",
        "limit",
        "offset",
        "join",
        "left",
        "right",
        "inner",
        "outer",
        "full",
        "cross",
        "natural",
        "straight_join",
        "on",
        "using",
        "union",
        "except",
        "intersect",
        "as",
        "select",
        "from",
    ]
    .iter()
    .any(|keyword| word.eq_ignore_ascii_case(keyword))
}

/// Every source table after FROM in order: comma sources and JOIN chains
/// with their ON/USING conditions skipped. None for subquery sources,
/// deeper qualifiers, unions, or trailing statements.
fn parse_sources(tokens: &[Token]) -> Option<Vec<JoinSource>> {
    let mut sources = Vec::new();
    let mut pos = 0;
    let (first, next) = parse_table_ref(tokens, pos)?;
    sources.push(first);
    pos = next;
    loop {
        match tokens.get(pos) {
            None => break,
            Some(Token::Punct(';')) => {
                if pos + 1 == tokens.len() {
                    break;
                }
                return None;
            }
            Some(Token::Punct(',')) => {
                let (source, next) = parse_table_ref(tokens, pos + 1)?;
                sources.push(source);
                pos = next;
            }
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("on") || w.eq_ignore_ascii_case("using") => {
                pos = skip_condition(tokens, pos + 1);
            }
            Some(Token::Word(_)) => {
                if is_trailing_clause_at(tokens, pos) {
                    break;
                }
                if is_union_at(tokens, pos) {
                    return None;
                }
                match consume_join(tokens, pos) {
                    Some(after_join) => {
                        let (source, next) = parse_table_ref(tokens, after_join)?;
                        sources.push(source);
                        pos = next;
                    }
                    None => return None,
                }
            }
            Some(_) => return None,
        }
    }
    Some(sources)
}

/// One `[schema.]table [[AS] alias]` at `pos` with the index after it. None
/// for subqueries, missing names, deeper qualifiers, or a bare AS.
fn parse_table_ref(tokens: &[Token], pos: usize) -> Option<(JoinSource, usize)> {
    let (first, after_first) = identifier_at(tokens, pos)?;
    let (table, after_table) = match tokens.get(after_first) {
        Some(Token::Punct('.')) => {
            let (second, after_second) = identifier_at(tokens, after_first + 1)?;
            if matches!(tokens.get(after_second), Some(Token::Punct('.'))) {
                return None;
            }
            (canonical_name(&first, Some(&second)), after_second)
        }
        _ => (canonical_name(&first, None), after_first),
    };
    match tokens.get(after_table) {
        Some(Token::Word(w)) if w.eq_ignore_ascii_case("as") => {
            let (alias, after_alias) = identifier_at(tokens, after_table + 1)?;
            Some((JoinSource { table, alias: Some(alias) }, after_alias))
        }
        Some(Token::Word(w)) if !is_join_reserved(w) => {
            let (alias, after_alias) = identifier_at(tokens, after_table)?;
            Some((JoinSource { table, alias: Some(alias) }, after_alias))
        }
        Some(Token::Quoted(_)) => {
            let (alias, after_alias) = identifier_at(tokens, after_table)?;
            Some((JoinSource { table, alias: Some(alias) }, after_alias))
        }
        _ => Some((JoinSource { table, alias: None }, after_table)),
    }
}

/// Index after a JOIN keyword chain (`JOIN`, `LEFT JOIN`, `LEFT OUTER
/// JOIN`, `STRAIGHT_JOIN`). None when no join starts at `pos`.
fn consume_join(tokens: &[Token], pos: usize) -> Option<usize> {
    let mut index = pos;
    loop {
        match tokens.get(index) {
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("straight_join") => return Some(index + 1),
            Some(Token::Word(w))
                if ["left", "right", "inner", "outer", "full", "cross", "natural"]
                    .iter()
                    .any(|m| w.eq_ignore_ascii_case(m)) =>
            {
                index += 1;
            }
            Some(Token::Word(w)) if w.eq_ignore_ascii_case("join") => return Some(index + 1),
            _ => return None,
        }
    }
}

/// Index where an ON/USING condition ends: the next top-level comma, join,
/// trailing clause, union, or end.
fn skip_condition(tokens: &[Token], mut pos: usize) -> usize {
    let mut depth: usize = 0;
    while let Some(token) = tokens.get(pos) {
        match token {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => depth = depth.saturating_sub(1),
            Token::Punct(',') if depth == 0 => break,
            Token::Word(w)
                if depth == 0
                    && (w.eq_ignore_ascii_case("join")
                        || w.eq_ignore_ascii_case("straight_join")
                        || ["left", "right", "inner", "outer", "full", "cross", "natural"]
                            .iter()
                            .any(|m| w.eq_ignore_ascii_case(m))
                        || is_trailing_clause(w)
                        || ["union", "except", "intersect"].iter().any(|k| w.eq_ignore_ascii_case(k))) =>
            {
                break;
            }
            Token::Punct(';') if depth == 0 => break,
            _ => {}
        }
        pos += 1;
    }
    pos
}

/// True at a trailing single-table clause or a statement end.
fn is_trailing_clause_at(tokens: &[Token], pos: usize) -> bool {
    match tokens.get(pos) {
        Some(Token::Word(w)) if is_trailing_clause(w) => true,
        Some(Token::Punct(';')) => true,
        None => true,
        _ => false,
    }
}

/// True at a set operator, which keeps the whole SELECT read-only.
fn is_union_at(tokens: &[Token], pos: usize) -> bool {
    matches!(tokens.get(pos), Some(Token::Word(w)) if ["union", "except", "intersect"].iter().any(|k| w.eq_ignore_ascii_case(k)))
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    /// An unquoted word: identifier or keyword.
    Word(String),
    /// A double- or backtick-quoted identifier, doubled quotes unescaped, so
    /// it never reads as a keyword.
    Quoted(String),
    /// A single-quoted string literal, a Postgres dollar-quoted string, or
    /// anything else whose contents never matter here.
    Str,
    /// Any other single character: punctuation and operators.
    Punct(char),
}

fn lex(sql: &str, kind: DbKind) -> Vec<Token> {
    let hash = hash_comments(kind);
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '\'' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            tokens.push(Token::Str);
        } else if c == '"' || c == '`' {
            let quote = c;
            let mut text = String::new();
            i += 1;
            while i < chars.len() {
                if chars[i] == quote {
                    if chars.get(i + 1) == Some(&quote) {
                        text.push(quote);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                text.push(chars[i]);
                i += 1;
            }
            tokens.push(Token::Quoted(text));
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '#' && hash {
            i += 1;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() {
                if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    i += 2;
                    break;
                }
                i += 1;
            }
        } else if let Some(end) = dollar_quote_end(&chars, i) {
            i = end;
            tokens.push(Token::Str);
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            tokens.push(Token::Word(chars[start..i].iter().collect()));
        } else {
            tokens.push(Token::Punct(c));
            i += 1;
        }
    }
    tokens
}

/// Index after the Postgres dollar-quoted string starting at `index`: `$$`
/// or `$tag$` where a nonempty tag starts with an ASCII letter or `_` and
/// continues with ASCII letters, digits, or `_`. None when no quote opens
/// there, so `$1` stays a word. An unterminated quote runs to the end.
fn dollar_quote_end(chars: &[char], index: usize) -> Option<usize> {
    if chars.get(index) != Some(&'$') {
        return None;
    }
    let mut tag_end = index + 1;
    while tag_end < chars.len()
        && (chars[tag_end].is_ascii_alphanumeric() || chars[tag_end] == '_')
    {
        tag_end += 1;
    }
    if chars.get(tag_end) != Some(&'$') {
        return None;
    }
    let tag = &chars[index + 1..tag_end];
    if let Some(first) = tag.first()
        && !(first.is_ascii_alphabetic() || *first == '_')
    {
        return None;
    }
    let mut close = Vec::with_capacity(tag.len() + 2);
    close.push('$');
    close.extend_from_slice(tag);
    close.push('$');
    let mut cursor = tag_end + 1;
    while cursor + close.len() <= chars.len() {
        if chars[cursor..cursor + close.len()] == close[..] {
            return Some(cursor + close.len());
        }
        cursor += 1;
    }
    Some(chars.len())
}

/// Staged text meaning the SQL `DEFAULT` keyword. A control-wrapped marker
/// rather than the bare word, so typing DEFAULT as plain text still quotes
/// as `'DEFAULT'` through [`to_literal`].
pub const STAGED_DEFAULT: &str = "\u{241e}DEFAULT\u{241e}";

/// One cell text as a SQL literal. An empty text or a value the caller knows
/// to be NULL renders as `NULL`; the [`STAGED_DEFAULT`] marker renders as
/// `DEFAULT`; a text that parses as a number stays bare; everything else is
/// single-quoted with quotes doubled.
pub fn to_literal(text: &str, was_null: bool) -> String {
    if text == STAGED_DEFAULT {
        return "DEFAULT".to_owned();
    }
    if text.is_empty() || was_null {
        return "NULL".to_owned();
    }
    if numeric_literal(text) {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', "''"))
}

/// One cell text as a literal for a column whose declared type is known. A
/// text column always takes the escaped quoted form, so `"007"` and `"1e3"`
/// keep their text identity; every other column keeps [`to_literal`]'s
/// mapping. The [`STAGED_DEFAULT`] marker and empty or NULL text win first,
/// since neither carries a column type.
pub fn to_typed_literal(text: &str, was_null: bool, is_text_column: bool) -> String {
    if text == STAGED_DEFAULT {
        return "DEFAULT".to_owned();
    }
    if text.is_empty() || was_null {
        return "NULL".to_owned();
    }
    if is_text_column {
        return format!("'{}'", text.replace('\'', "''"));
    }
    to_literal(text, was_null)
}

/// True when `text` is a spelling of a number SQL accepts. The character
/// gate keeps the spellings `f64` parses but SQL does not, such as `inf`
/// and `NaN`, out. A leading `+` and a leading-zero integer part are kept
/// out too, so `"007"` and `"010"` stay text: both engines read `010` as a
/// different value than the text it looks like.
fn numeric_literal(text: &str) -> bool {
    if !text.bytes().any(|b| b.is_ascii_digit()) {
        return false;
    }
    if !text
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'-' | b'e' | b'E'))
    {
        return false;
    }
    if text.starts_with('+') {
        return false;
    }
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    if unsigned.starts_with('0')
        && !matches!(unsigned.as_bytes().get(1).copied(), None | Some(b'.' | b'e' | b'E'))
    {
        return false;
    }
    text.parse::<i64>().is_ok() || text.parse::<f64>().is_ok()
}

/// True when a declared column type is text-ish, so a key of that type
/// renders as a quoted literal. Follows the affinity order the engines use:
/// INT wins first, then CHAR, CLOB, and TEXT; uuid and json spellings are
/// text as well. Integer, real, numeric, blob, and datetime families are not
/// text.
pub fn is_text_type(type_name: &str) -> bool {
    let lowered = type_name.trim().to_ascii_lowercase();
    if lowered.contains("int") {
        return false;
    }
    if ["char", "clob", "text"].iter().any(|marker| lowered.contains(marker)) {
        return true;
    }
    if lowered.contains("blob") {
        return false;
    }
    ["uuid", "json"].iter().any(|marker| lowered.contains(marker))
}

/// `UPDATE "table" SET "col" = <literal>, ... WHERE "key" = <literal> AND ...`
/// Set values are raw text and render through [`to_literal`], so an empty
/// value is NULL. Key values are literals already, rendered by the caller
/// with [`to_literal`], because only the caller can tell a NULL cell from a
/// text cell reading `NULL`.
pub fn update_statement(table: &str, pk: &[(String, String)], sets: &[(String, String)]) -> String {
    let typed = sets
        .iter()
        .map(|(column, text)| (column.clone(), text.clone(), false))
        .collect::<Vec<_>>();
    update_typed_statement(table, pk, &typed)
}

/// [`update_statement`] with each set column's text flag: a text column
/// renders through [`to_typed_literal`], so `"007"` keeps its text identity.
pub fn update_typed_statement(
    table: &str,
    pk: &[(String, String)],
    sets: &[(String, String, bool)],
) -> String {
    let sets = sets
        .iter()
        .map(|(column, text, is_text)| {
            format!("{} = {}", ident(column), to_typed_literal(text, false, *is_text))
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("UPDATE {} SET {} WHERE {}", quote_qualified(table), sets, predicate(pk))
}

/// `DELETE FROM "table" WHERE "key" = <literal> AND ...` with the key values
/// rendered by the caller through [`to_literal`].
pub fn delete_statement(table: &str, pk: &[(String, String)]) -> String {
    format!("DELETE FROM {} WHERE {}", quote_qualified(table), predicate(pk))
}

/// `INSERT INTO "table" ("col", ...) VALUES (<literal>, ...)`. Values are raw
/// text and render through [`to_literal`], so an empty value is NULL.
pub fn insert_statement(table: &str, columns: &[String], values: &[String]) -> String {
    insert_typed_statement(table, columns, values, &[])
}

/// [`insert_statement`] with one text flag per column position: a text
/// column renders through [`to_typed_literal`], so `"007"` keeps its text
/// identity. A missing flag reads as false.
pub fn insert_typed_statement(
    table: &str,
    columns: &[String],
    values: &[String],
    text_cols: &[bool],
) -> String {
    let columns = columns.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", ");
    let values = values
        .iter()
        .enumerate()
        .map(|(index, v)| {
            to_typed_literal(v, false, text_cols.get(index).copied().unwrap_or(false))
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO {} ({}) VALUES ({})", quote_qualified(table), columns, values)
}

pub fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A table name for SQL: split on unquoted dots only, so a literal table
/// named `a.b` stays one part, then double-quote each part with inner quotes
/// doubled.
pub fn quote_qualified(table: &str) -> String {
    qualified_parts(table).iter().map(|part| ident(part)).collect::<Vec<_>>().join(".")
}

/// Split a table name on its unquoted dots and unescape the parts: one part
/// for a bare or quoted name, two for `schema.table`. A dot inside a quoted
/// part stays in that part.
pub fn qualified_parts(table: &str) -> Vec<String> {
    split_unquoted(table).into_iter().map(|part| unquote(&part)).collect()
}

/// Split on dots outside double or backtick quotes, keeping each part's
/// quotes.
fn split_unquoted(table: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = table.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => {
                current.push(c);
                if chars.next_if_eq(&q).is_some() {
                    current.push(q);
                } else {
                    quote = None;
                }
            }
            Some(_) => current.push(c),
            None if c == '"' || c == '`' => {
                current.push(c);
                quote = Some(c);
            }
            None if c == '.' => parts.push(std::mem::take(&mut current)),
            None => current.push(c),
        }
    }
    parts.push(current);
    parts
}

/// One part's content: a fully quoted part loses its quotes and undoubles
/// its inner quotes; a bare part is kept as is.
fn unquote(part: &str) -> String {
    for quote in ['"', '`'] {
        if part.len() >= 2 && part.starts_with(quote) && part.ends_with(quote) {
            let doubled = format!("{quote}{quote}");
            return part[1..part.len() - 1].replace(&doubled, &quote.to_string());
        }
    }
    part.to_owned()
}

fn predicate(pk: &[(String, String)]) -> String {
    pk.iter()
        .map(|(column, literal)| format!("{} = {}", ident(column), literal))
        .collect::<Vec<_>>()
        .join(" AND ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_of(sql: &str) -> Option<String> {
        editable_table(sql, DbKind::Postgres)
    }

    #[test]
    fn simple_select_is_editable() {
        assert_eq!(table_of("SELECT * FROM users"), Some("users".to_owned()));
        assert_eq!(table_of("select id, name from users"), Some("users".to_owned()));
        assert_eq!(table_of("  SELECT id FROM orders ;  "), Some("orders".to_owned()));
        assert_eq!(table_of("SELECT count(*) FROM users"), Some("users".to_owned()));
    }

    #[test]
    fn where_group_order_having_limit_and_offset_are_editable() {
        for sql in [
            "SELECT * FROM users WHERE id > 3",
            "SELECT tenant FROM users GROUP BY tenant",
            "SELECT id FROM users ORDER BY name DESC",
            "SELECT tenant FROM users GROUP BY tenant HAVING count(*) > 1",
            "SELECT * FROM users LIMIT 10",
            "SELECT * FROM users LIMIT 10 OFFSET 20",
            "SELECT * FROM users WHERE name = 'from' ORDER BY id",
            "-- note\nSELECT * FROM users",
        ] {
            assert_eq!(table_of(sql).as_deref(), Some("users"), "{sql}");
        }
    }

    #[test]
    fn joins_unions_comma_sources_and_subqueries_are_not_editable() {
        for sql in [
            "SELECT * FROM users JOIN orders ON orders.user_id = users.id",
            "SELECT * FROM users LEFT JOIN orders ON orders.user_id = users.id",
            "SELECT id FROM users UNION SELECT id FROM orders",
            "SELECT id FROM users UNION ALL SELECT id FROM orders",
            "SELECT * FROM users, orders",
            "SELECT * FROM (SELECT 1 AS id) users",
            "SELECT (SELECT 1 FROM orders) FROM users",
            "SELECT * FROM users u",
            "SELECT * FROM 'users'",
            "SELECT * FROM users; SELECT * FROM orders",
        ] {
            assert_eq!(table_of(sql), None, "{sql}");
        }
    }

    #[test]
    fn statements_that_are_not_a_plain_select_are_not_editable() {
        for sql in [
            "",
            "   ",
            "UPDATE users SET name = 'ada' WHERE id = 1",
            "DELETE FROM users WHERE id = 1",
            "INSERT INTO users (id) VALUES (1)",
            "WITH ids AS (SELECT 1) SELECT * FROM ids",
            "EXPLAIN SELECT * FROM users",
            "SELECT 1",
            "SELECT * FROM",
        ] {
            assert_eq!(table_of(sql), None, "{sql:?}");
        }
    }

    #[test]
    fn a_quoted_identifier_is_accepted_and_unescaped() {
        assert_eq!(table_of("SELECT * FROM \"users\""), Some("users".to_owned()));
        assert_eq!(
            table_of("select \"id\" from \"Users\" where \"id\" = 1"),
            Some("Users".to_owned())
        );
        // A part containing a quote keeps its quoting.
        assert_eq!(
            table_of("SELECT * FROM \"odd\"\"name\""),
            Some("\"odd\"\"name\"".to_owned())
        );
        // A quoted identifier is not the FROM keyword.
        assert_eq!(table_of("SELECT \"from\" FROM users").as_deref(), Some("users"));
    }

    #[test]
    fn a_single_schema_qualifier_is_accepted() {
        assert_eq!(
            table_of("SELECT * FROM public.users"),
            Some("public.users".to_owned())
        );
        assert_eq!(
            table_of("SELECT * FROM \"main\".\"users\""),
            Some("main.users".to_owned())
        );
        assert_eq!(
            table_of("SELECT * FROM \"main\".users LIMIT 10"),
            Some("main.users".to_owned())
        );
        assert_eq!(
            table_of("select * from \"Main\".\"Users\" where id = 1"),
            Some("Main.Users".to_owned())
        );
        // A part containing a dot keeps its quoting, so it stays one part.
        assert_eq!(table_of("SELECT * FROM \"a.b\""), Some("\"a.b\"".to_owned()));
        assert_eq!(
            table_of("SELECT * FROM \"a.b\".c"),
            Some("\"a.b\".c".to_owned())
        );
        // One qualifier only, and both parts are required.
        assert_eq!(table_of("SELECT * FROM a.b.c"), None);
        assert_eq!(table_of("SELECT * FROM \"a\".\"b\".\"c\""), None);
        assert_eq!(table_of("SELECT * FROM main."), None);
        assert_eq!(table_of("SELECT * FROM main.users u"), None);
    }

    #[test]
    fn a_backtick_identifier_is_accepted_and_unescaped() {
        assert_eq!(table_of("SELECT * FROM `users`"), Some("users".to_owned()));
        assert_eq!(
            table_of("select `id` from `main`.`Users` where `id` = 1"),
            Some("main.Users".to_owned())
        );
        assert_eq!(table_of("SELECT * FROM `odd``name`"), Some("odd`name".to_owned()));
        // A backtick identifier is not the FROM keyword.
        assert_eq!(table_of("SELECT `from` FROM users").as_deref(), Some("users"));
    }

    #[test]
    fn hash_hides_nothing_outside_mysql() {
        let sql = "SELECT id, data #>> '{a}' AS label FROM t";
        let plan = parse_join(sql, DbKind::Postgres).expect("one source");
        let columns: Vec<_> = plan.selects.iter().map(|s| s.column.as_deref()).collect();
        assert_eq!(columns, vec![Some("id"), None]);
        assert_eq!(editable_table(sql, DbKind::Postgres), Some("t".to_owned()));
        assert_eq!(editable_table(sql, DbKind::Mysql), None);
    }

    #[test]
    fn hash_comments_and_dollar_quotes_lex_as_skipped_text() {
        assert_eq!(
            editable_table("SELECT * FROM users # note with from join\nWHERE id = 1", DbKind::Mysql),
            Some("users".to_owned())
        );
        assert_eq!(
            table_of("SELECT * FROM users WHERE note = $tag$from join 'where'$tag$"),
            Some("users".to_owned())
        );
        assert_eq!(
            table_of("SELECT * FROM users WHERE note = $$from join$$ LIMIT 5"),
            Some("users".to_owned())
        );
        assert_eq!(
            join_of("SELECT u.id FROM users u JOIN orders o ON o.note = $$join from$$"),
            Some(JoinPlan {
                sources: vec![
                    JoinSource { table: "users".to_owned(), alias: Some("u".to_owned()) },
                    JoinSource { table: "orders".to_owned(), alias: Some("o".to_owned()) },
                ],
                selects: vec![JoinSelect {
                    qualifier: Some("u".to_owned()),
                    column: Some("id".to_owned())
                }],
            })
        );
    }

    #[test]
    fn wildcard_selects_read_positionally() {
        assert!(wildcard_select("SELECT * FROM users", DbKind::Postgres));
        assert!(wildcard_select("SELECT DISTINCT t.* FROM users t", DbKind::Postgres));
        assert!(!wildcard_select("SELECT count(*) FROM users", DbKind::Postgres));
        assert!(!wildcard_select("SELECT u.id, u.* FROM users u", DbKind::Postgres));
    }

    #[test]
    fn quote_qualified_splits_only_on_unquoted_dots() {
        assert_eq!(quote_qualified("users"), "\"users\"");
        assert_eq!(quote_qualified("\"main\".\"users\""), "\"main\".\"users\"");
        assert_eq!(quote_qualified("\"a.b\""), "\"a.b\"");
        assert_eq!(quote_qualified("main.\"odd\"\"name\""), "\"main\".\"odd\"\"name\"");
    }

    #[test]
    fn text_types_are_recognized_across_families() {
        for type_name in [
            "text",
            "TEXT",
            "char",
            "CHAR(10)",
            "varchar",
            "VARCHAR(255)",
            "character varying",
            "nvarchar",
            "clob",
            "uuid",
            "UUID",
            "json",
            "jsonb",
            "citext",
            "enum('small','text-large')",
        ] {
            assert!(is_text_type(type_name), "{type_name}");
        }
        for type_name in [
            "integer",
            "int4",
            "bigint",
            "smallint",
            "real",
            "double precision",
            "float",
            "numeric",
            "numeric(10,2)",
            "decimal(8,2)",
            "blob",
            "bytea",
            "datetime",
            "date",
            "time",
            "timestamp without time zone",
            "boolean",
            "",
        ] {
            assert!(!is_text_type(type_name), "{type_name}");
        }
    }

    #[test]
    fn literal_mapping_covers_numbers_text_quotes_and_null() {
        assert_eq!(to_literal("42", false), "42");
        assert_eq!(to_literal("-7", false), "-7");
        assert_eq!(to_literal("2.5", false), "2.5");
        assert_eq!(to_literal("1e3", false), "1e3");
        assert_eq!(to_literal("ada", false), "'ada'");
        assert_eq!(to_literal("O'Brien", false), "'O''Brien'");
        assert_eq!(to_literal("", false), "NULL");
        assert_eq!(to_literal("ada", true), "NULL");
        assert_eq!(to_literal("", true), "NULL");
        // Spellings f64 parses but SQL does not accept stay text.
        assert_eq!(to_literal("NaN", false), "'NaN'");
        assert_eq!(to_literal("inf", false), "'inf'");
        assert_eq!(to_literal("1_000", false), "'1_000'");
        // A leading plus and a leading-zero integer part stay text, so the
        // literal reads back as the same characters.
        assert_eq!(to_literal("+7", false), "'+7'");
        assert_eq!(to_literal("007", false), "'007'");
        assert_eq!(to_literal("010", false), "'010'");
        assert_eq!(to_literal("-010", false), "'-010'");
        assert_eq!(to_literal("00.5", false), "'00.5'");
        assert_eq!(to_literal("0", false), "0");
        assert_eq!(to_literal("-0", false), "-0");
        assert_eq!(to_literal("-0.5", false), "-0.5");
        assert_eq!(to_literal("0e3", false), "0e3");
    }

    #[test]
    fn typed_literals_always_quote_a_text_column() {
        assert_eq!(to_typed_literal("007", false, true), "'007'");
        assert_eq!(to_typed_literal("007", false, false), "'007'");
        assert_eq!(to_typed_literal("42", false, true), "'42'");
        assert_eq!(to_typed_literal("42", false, false), "42");
        assert_eq!(to_typed_literal("O'Brien", false, true), "'O''Brien'");
        assert_eq!(to_typed_literal("", false, true), "NULL");
        assert_eq!(to_typed_literal("ada", true, true), "NULL");
        assert_eq!(to_typed_literal(STAGED_DEFAULT, false, true), "DEFAULT");
    }

    #[test]
    fn update_delete_and_insert_are_exact_on_a_composite_key() {
        let pk = [
            ("tenant".to_owned(), "'acme'".to_owned()),
            ("id".to_owned(), "42".to_owned()),
        ];
        let sets = [
            ("name".to_owned(), "Ada".to_owned()),
            ("note".to_owned(), String::new()),
        ];
        assert_eq!(
            update_statement("user notes", &pk, &sets),
            "UPDATE \"user notes\" SET \"name\" = 'Ada', \"note\" = NULL WHERE \"tenant\" = 'acme' AND \"id\" = 42"
        );
        assert_eq!(
            delete_statement("user notes", &pk),
            "DELETE FROM \"user notes\" WHERE \"tenant\" = 'acme' AND \"id\" = 42"
        );
        assert_eq!(
            insert_statement(
                "user notes",
                &["tenant".to_owned(), "id".to_owned(), "name".to_owned()],
                &["acme".to_owned(), "7".to_owned(), "O'Brien".to_owned()],
            ),
            "INSERT INTO \"user notes\" (\"tenant\", \"id\", \"name\") VALUES ('acme', 7, 'O''Brien')"
        );
        assert_eq!(
            delete_statement("a\"b", &[("id".to_owned(), "1".to_owned())]),
            "DELETE FROM \"a\"\"b\" WHERE \"id\" = 1"
        );
        assert_eq!(
            update_statement(
                "public.users",
                &[("id".to_owned(), "42".to_owned())],
                &[("name".to_owned(), "Ada".to_owned())],
            ),
            "UPDATE \"public\".\"users\" SET \"name\" = 'Ada' WHERE \"id\" = 42"
        );
        assert_eq!(
            delete_statement("\"a.b\"", &[("id".to_owned(), "1".to_owned())]),
            "DELETE FROM \"a.b\" WHERE \"id\" = 1"
        );
    }

    #[test]
    fn typed_update_and_insert_quote_text_columns() {
        let pk = [("id".to_owned(), "1".to_owned())];
        let sets = [
            ("code".to_owned(), "42".to_owned(), true),
            ("n".to_owned(), "42".to_owned(), false),
        ];
        assert_eq!(
            update_typed_statement("t", &pk, &sets),
            "UPDATE \"t\" SET \"code\" = '42', \"n\" = 42 WHERE \"id\" = 1"
        );
        assert_eq!(
            insert_typed_statement(
                "t",
                &["code".to_owned(), "n".to_owned()],
                &["007".to_owned(), "42".to_owned()],
                &[true, false],
            ),
            "INSERT INTO \"t\" (\"code\", \"n\") VALUES ('007', 42)"
        );
        // The untyped builders keep the old mapping.
        assert_eq!(
            update_statement("t", &pk, &[("n".to_owned(), "42".to_owned())]),
            "UPDATE \"t\" SET \"n\" = 42 WHERE \"id\" = 1"
        );
        assert_eq!(
            insert_statement("t", &["n".to_owned()], &["42".to_owned()]),
            "INSERT INTO \"t\" (\"n\") VALUES (42)"
        );
    }

    fn join_of(sql: &str) -> Option<JoinPlan> {
        parse_join(sql, DbKind::Postgres)
    }

    #[test]
    fn two_table_join_maps_each_column_to_its_source() {
        assert_eq!(
            join_of("SELECT u.id, u.name, o.total FROM users AS u JOIN orders AS o ON o.user_id = u.id"),
            Some(JoinPlan {
                sources: vec![
                    JoinSource { table: "users".to_owned(), alias: Some("u".to_owned()) },
                    JoinSource { table: "orders".to_owned(), alias: Some("o".to_owned()) },
                ],
                selects: vec![
                    JoinSelect { qualifier: Some("u".to_owned()), column: Some("id".to_owned()) },
                    JoinSelect { qualifier: Some("u".to_owned()), column: Some("name".to_owned()) },
                    JoinSelect { qualifier: Some("o".to_owned()), column: Some("total".to_owned()) },
                ],
            })
        );
        assert_eq!(
            join_of("SELECT users.id, orders.total FROM users JOIN orders ON orders.user_id = users.id WHERE users.id > 1 ORDER BY users.id LIMIT 10"),
            Some(JoinPlan {
                sources: vec![
                    JoinSource { table: "users".to_owned(), alias: None },
                    JoinSource { table: "orders".to_owned(), alias: None },
                ],
                selects: vec![
                    JoinSelect { qualifier: Some("users".to_owned()), column: Some("id".to_owned()) },
                    JoinSelect { qualifier: Some("orders".to_owned()), column: Some("total".to_owned()) },
                ],
            })
        );
        assert_eq!(
            join_of("SELECT u.id AS user_id, o.total AS order_total FROM users u LEFT OUTER JOIN orders o ON o.user_id = u.id"),
            Some(JoinPlan {
                sources: vec![
                    JoinSource { table: "users".to_owned(), alias: Some("u".to_owned()) },
                    JoinSource { table: "orders".to_owned(), alias: Some("o".to_owned()) },
                ],
                selects: vec![
                    JoinSelect { qualifier: Some("u".to_owned()), column: Some("id".to_owned()) },
                    JoinSelect { qualifier: Some("o".to_owned()), column: Some("total".to_owned()) },
                ],
            })
        );
    }

    #[test]
    fn aliased_tables_keep_their_canonical_names() {
        assert_eq!(
            join_of("SELECT u.id, u.name FROM users u"),
            Some(JoinPlan {
                sources: vec![JoinSource { table: "users".to_owned(), alias: Some("u".to_owned()) }],
                selects: vec![
                    JoinSelect { qualifier: Some("u".to_owned()), column: Some("id".to_owned()) },
                    JoinSelect { qualifier: Some("u".to_owned()), column: Some("name".to_owned()) },
                ],
            })
        );
        assert_eq!(
            join_of("SELECT \"u\".\"id\" FROM \"public\".\"users\" AS \"u\" JOIN \"orders\" \"o\" ON \"o\".\"user_id\" = \"u\".\"id\""),
            Some(JoinPlan {
                sources: vec![
                    JoinSource { table: "public.users".to_owned(), alias: Some("u".to_owned()) },
                    JoinSource { table: "orders".to_owned(), alias: Some("o".to_owned()) },
                ],
                selects: vec![JoinSelect {
                    qualifier: Some("u".to_owned()),
                    column: Some("id".to_owned())
                }],
            })
        );
        assert_eq!(
            join_of("SELECT a.id, b.total FROM users, orders a JOIN orders b ON b.user_id = a.id"),
            Some(JoinPlan {
                sources: vec![
                    JoinSource { table: "users".to_owned(), alias: None },
                    JoinSource { table: "orders".to_owned(), alias: Some("a".to_owned()) },
                    JoinSource { table: "orders".to_owned(), alias: Some("b".to_owned()) },
                ],
                selects: vec![
                    JoinSelect { qualifier: Some("a".to_owned()), column: Some("id".to_owned()) },
                    JoinSelect { qualifier: Some("b".to_owned()), column: Some("total".to_owned()) },
                ],
            })
        );
    }

    #[test]
    fn backtick_sources_and_columns_parse_as_join_items() {
        assert_eq!(
            join_of(
                "SELECT `u`.`id`, `o`.`total` FROM `users` `u` JOIN `orders` `o` \
                 ON `o`.`user_id` = `u`.`id`"
            ),
            Some(JoinPlan {
                sources: vec![
                    JoinSource { table: "users".to_owned(), alias: Some("u".to_owned()) },
                    JoinSource { table: "orders".to_owned(), alias: Some("o".to_owned()) },
                ],
                selects: vec![
                    JoinSelect { qualifier: Some("u".to_owned()), column: Some("id".to_owned()) },
                    JoinSelect { qualifier: Some("o".to_owned()), column: Some("total".to_owned()) },
                ],
            })
        );
    }

    #[test]
    fn expressions_stars_and_bad_shapes_leave_only_their_own_column_unresolvable() {
        assert_eq!(
            join_of("SELECT count(*), u.id + 1, *, o.* FROM users u JOIN orders o ON o.user_id = u.id"),
            Some(JoinPlan {
                sources: vec![
                    JoinSource { table: "users".to_owned(), alias: Some("u".to_owned()) },
                    JoinSource { table: "orders".to_owned(), alias: Some("o".to_owned()) },
                ],
                selects: vec![
                    JoinSelect { qualifier: None, column: None },
                    JoinSelect { qualifier: None, column: None },
                    JoinSelect { qualifier: None, column: None },
                    JoinSelect { qualifier: Some("o".to_owned()), column: None },
                ],
            })
        );
        assert_eq!(
            join_of("SELECT u.id, 'from', 42 FROM users u JOIN orders o ON o.user_id = u.id"),
            Some(JoinPlan {
                sources: vec![
                    JoinSource { table: "users".to_owned(), alias: Some("u".to_owned()) },
                    JoinSource { table: "orders".to_owned(), alias: Some("o".to_owned()) },
                ],
                selects: vec![
                    JoinSelect { qualifier: Some("u".to_owned()), column: Some("id".to_owned()) },
                    JoinSelect { qualifier: None, column: None },
                    JoinSelect { qualifier: None, column: Some("42".to_owned()) },
                ],
            })
        );
        assert_eq!(
            join_of("SELECT a.b.c, u.id FROM users u JOIN orders o ON o.user_id = u.id"),
            Some(JoinPlan {
                sources: vec![
                    JoinSource { table: "users".to_owned(), alias: Some("u".to_owned()) },
                    JoinSource { table: "orders".to_owned(), alias: Some("o".to_owned()) },
                ],
                selects: vec![
                    JoinSelect { qualifier: None, column: None },
                    JoinSelect { qualifier: Some("u".to_owned()), column: Some("id".to_owned()) },
                ],
            })
        );
        for sql in [
            "SELECT u.id FROM (SELECT 1 AS id) u JOIN orders o ON o.user_id = u.id",
            "SELECT u.id FROM users u JOIN orders o ON o.user_id = u.id UNION SELECT id FROM users",
            "SELECT u.id FROM a.b.c u JOIN orders o ON o.user_id = u.id",
            "SELECT * FROM users JOIN orders ON orders.user_id = users.id; SELECT 1",
            "UPDATE users SET name = 'ada'",
        ] {
            assert_eq!(join_of(sql), None, "{sql}");
        }
    }
}
