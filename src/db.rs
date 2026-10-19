//! Database engines, value types, and the worker-thread protocol.
//!
//! Everything that touches rusqlite (or sqlx, behind the `postgres` and
//! `mysql` features) lives in this module. Layers above only see [`Value`],
//! [`QueryResult`], [`DbError`], and the [`DbCommand`]/[`DbEvent`]
//! protocol. No client-library type crosses the boundary. A session that
//! needs an SSH tunnel owns it here for the session's whole life; the
//! tunnel itself lives in [`crate::ssh`].

use std::borrow::Cow;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use crate::sql::{
    blank_hash_comments, block_comment_end, hash_comments, line_comment_end, scan_dollar_quote,
    scan_quote,
};
use crate::ssh::SshConfig;

/// Cap on materialized rows per statement. The result reports truncation.
pub const ROW_CAP: usize = 10_000;
/// Rows per streaming page. Paging and streamed export fetch this many rows
/// per roundtrip, so a large result never materializes at once.
pub const STREAM_PAGE_SIZE: usize = 5_000;
/// Cap on displayed characters per text cell in the grid.
pub const CELL_CAP: usize = 256;

/// One whole token the scanner steps over: its end offset and whether the
/// text counts as code (`'...'`, `"..."`, `` `...` ``, `$$...$$`) or as a
/// comment that does not.
#[derive(Clone, Copy)]
enum Skipped {
    Code { end: usize },
    Comment { end: usize },
}

impl Skipped {
    /// One past the token.
    fn end(self) -> usize {
        match self {
            Skipped::Code { end } | Skipped::Comment { end } => end,
        }
    }
}

/// The string, quoted identifier, comment, or dollar-quoted literal starting
/// at `i`, with `#` opening a line comment only when `hash` is set. None when
/// `i` starts ordinary text, so callers decide what to do with the byte
/// themselves.
fn skipped_token(sql: &str, i: usize, hash: bool) -> Option<Skipped> {
    let bytes = sql.as_bytes();
    let token = match bytes.get(i) {
        Some(&b'\'') | Some(&b'"') | Some(&b'`') => {
            Skipped::Code { end: scan_quote(bytes, i, bytes[i]) }
        }
        Some(&b'$') => Skipped::Code { end: scan_dollar_quote(bytes, i)? },
        Some(&b'-') if bytes.get(i + 1) == Some(&b'-') => {
            Skipped::Comment { end: line_comment_end(bytes, i + 2) }
        }
        Some(&b'#') if hash => Skipped::Comment { end: line_comment_end(bytes, i + 1) },
        Some(&b'/') if bytes.get(i + 1) == Some(&b'*') => {
            Skipped::Comment { end: block_comment_end(bytes, i + 2) }
        }
        _ => return None,
    };
    Some(token)
}

/// One statement with its leading and trailing comments dropped and its
/// trailing terminators removed, so a page or count wrapper never nests a
/// second terminator or swallows its own closing paren in a trailing line
/// comment. Empty when no code token is left. A comment with code after it
/// stays, since its newline keeps the wrapper's paren out of it.
fn trim_statement(sql: &str, kind: DbKind) -> &str {
    let hash = hash_comments(kind);
    let bytes = sql.as_bytes();
    let mut first: Option<usize> = None;
    let mut last = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b';' {
            i += 1;
            continue;
        }
        if let Some(token) = skipped_token(sql, i, hash) {
            if let Skipped::Code { end } = token {
                if first.is_none() {
                    first = Some(i);
                }
                last = end;
            }
            i = token.end();
            continue;
        }
        let character = sql[i..].chars().next().expect("non-empty slice");
        if character.is_whitespace() {
            i += character.len_utf8();
            continue;
        }
        if first.is_none() {
            first = Some(i);
        }
        last = i + character.len_utf8();
        i = last;
    }
    first.map_or("", |start| &sql[start..last])
}

/// One page of `sql` as a subselect. `LIMIT` plus `OFFSET` reads on SQLite,
/// Postgres, and MySQL 8, so one wrapper covers every engine.
pub fn paged_sql(sql: &str, kind: DbKind, limit: u64, offset: u64) -> String {
    let inner = trim_statement(sql, kind);
    format!("SELECT * FROM ({inner}) AS __cde_page LIMIT {limit} OFFSET {offset}")
}

/// The total row count behind `sql` as one scalar row.
pub fn count_sql(sql: &str, kind: DbKind) -> String {
    let inner = trim_statement(sql, kind);
    format!("SELECT COUNT(*) FROM ({inner}) AS __cde_count")
}

/// True when `sql` holds one statement with no inner terminator. Paging and
/// streamed export wrap the whole text as a subselect, so a batch falls back
/// to the visible grid instead. A `;` inside a string, quoted identifier, or
/// comment never counts.
pub fn is_single_statement(sql: &str, kind: DbKind) -> bool {
    !has_terminator(trim_statement(sql, kind), kind)
}

/// True when `sql` holds a `;` outside strings, quoted identifiers, comments,
/// and dollar-quoted literals.
fn has_terminator(sql: &str, kind: DbKind) -> bool {
    let hash = hash_comments(kind);
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(token) = skipped_token(sql, i, hash) {
            i = token.end();
            continue;
        }
        if bytes[i] == b';' {
            return true;
        }
        i += 1;
    }
    false
}

/// Branded id so a tab id cannot stand in for a connection id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ConnectionId(u64);
/// Stable tab identity, never a vector index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(u64);
/// Caller-allocated request id, echoed back on completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(u64);

impl RunId {
    /// Mint one id from a caller counter, for grid paging runs the app
    /// never tracks as running.
    pub fn from_raw(id: u64) -> Self {
        Self(id)
    }
}

impl ConnectionId {
    pub fn raw(self) -> u64 {
        self.0
    }
    pub fn from_raw(id: u64) -> Self {
        Self(id)
    }
}

/// The single minter of every id in the app. One counter, three brands.
#[derive(Debug)]
pub struct Ids {
    next: u64,
}

impl Ids {
    pub fn new() -> Self {
        Self { next: 1 }
    }
    /// Called once at startup so loaded profile ids stay unique.
    pub fn resume_after(&mut self, existing: &[ConnectionId]) {
        if let Some(max) = existing.iter().map(|c| c.0).max() {
            self.next = self.next.max(max + 1);
        }
    }
    fn take(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }
    pub fn connection(&mut self) -> ConnectionId {
        ConnectionId(self.take())
    }
    pub fn tab(&mut self) -> TabId {
        TabId(self.take())
    }
    pub fn run(&mut self) -> RunId {
        RunId(self.take())
    }
    pub fn saved(&mut self) -> u64 {
        self.take()
    }
    /// Keep the single counter past loaded saved-query ids.
    pub fn resume_after_saved(&mut self, existing: &[u64]) {
        if let Some(max) = existing.iter().copied().max() {
            self.next = self.next.max(max + 1);
        }
    }
}

/// The kind the user picks in the New Connection form. A chooser, not
/// stored state: [`ConnectionConfig`] carries the kind, `kind()` derives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DbKind {
    Sqlite,
    Postgres,
    Mysql,
}

impl DbKind {
    pub const ALL: [DbKind; 3] = [DbKind::Sqlite, DbKind::Postgres, DbKind::Mysql];
    pub fn label(self) -> &'static str {
        match self {
            DbKind::Sqlite => "SQLite",
            DbKind::Postgres => "Postgres",
            DbKind::Mysql => "MySQL",
        }
    }
    pub fn default_port(self) -> Option<u16> {
        match self {
            DbKind::Sqlite => None,
            DbKind::Postgres => Some(5432),
            DbKind::Mysql => Some(3306),
        }
    }
    pub fn default_config(self) -> ConnectionConfig {
        match self {
            DbKind::Sqlite => ConnectionConfig::Sqlite {
                path: PathBuf::new(),
                read_only: false,
            },
            DbKind::Postgres => ConnectionConfig::Postgres {
                host: "localhost".to_owned(),
                port: 5432,
                database: String::new(),
                user: String::new(),
                ssl_mode: PostgresSslMode::Disable,
                ssh: None,
                read_only: false,
            },
            DbKind::Mysql => ConnectionConfig::Mysql {
                host: "localhost".to_owned(),
                port: 3306,
                database: String::new(),
                user: String::new(),
                ssl_mode: MysqlSslMode::default(),
                ssh: None,
                read_only: false,
            },
        }
    }
}

/// The statement form that returns a query plan for `kind`. Pure text: the
/// statement is trimmed and the engine's plan prefix is prepended.
pub fn explain_sql(kind: DbKind, statement: &str) -> String {
    match kind {
        DbKind::Sqlite => format!("EXPLAIN QUERY PLAN {}", statement.trim()),
        DbKind::Postgres | DbKind::Mysql => format!("EXPLAIN {}", statement.trim()),
    }
}

/// One write kind the read-only guard refuses, as uppercase text for errors.
/// The single classifier behind the guard; callers never match keywords. A
/// write hidden inside a `WITH` list or behind an `EXPLAIN ANALYZE` still
/// counts, since both run the statement they wrap. The text is read with `#`
/// as an operator and again with `#` as a MySQL comment, and a write under
/// either reading counts, so the guard needs no engine kind.
pub fn write_statement_kind(sql: &str) -> Option<&'static str> {
    write_kind(sql).or_else(|| write_kind(&blank_hash_comments(sql)))
}

/// [`write_statement_kind`] for one reading, with `#` as ordinary text.
fn write_kind(sql: &str) -> Option<&'static str> {
    let (keyword, end) = keyword_span(sql)?;
    match keyword.as_str() {
        "INSERT" => Some("INSERT"),
        "UPDATE" => Some("UPDATE"),
        "DELETE" => Some("DELETE"),
        "CREATE" => Some("CREATE"),
        "ALTER" => Some("ALTER"),
        "DROP" => Some("DROP"),
        "TRUNCATE" => Some("TRUNCATE"),
        "REPLACE" => Some("REPLACE"),
        "MERGE" => Some("MERGE"),
        "COPY" => Some("COPY"),
        "VACUUM" => Some("VACUUM"),
        "GRANT" => Some("GRANT"),
        "REVOKE" => Some("REVOKE"),
        "DO" => Some("DO"),
        "CALL" => Some("CALL"),
        "WITH" => {
            let (bodies, main) = with_clause(sql, end)?;
            bodies
                .iter()
                .find_map(|body| write_kind(&sql[body.clone()]))
                .or_else(|| write_kind(&sql[main..]))
        }
        "EXPLAIN" => explain_inner(sql, end).and_then(write_kind),
        _ => None,
    }
}

/// True when [`write_statement_kind`] names a write.
pub fn is_write_statement(sql: &str) -> bool {
    write_statement_kind(sql).is_some()
}

/// An UPDATE or DELETE with no outer WHERE: its kind and affected table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnqualifiedWrite {
    pub kind: &'static str,
    pub table: String,
}

/// Some only for an UPDATE or DELETE with no WHERE at paren depth zero.
/// Leading comments and wrapping parens are skipped; matching is
/// case-insensitive and strings, quoted identifiers, comments, and
/// dollar-quoted literals never count as WHERE. A `WITH` list is read CTE by
/// CTE and the statement after it on its own, and an `EXPLAIN ANALYZE` reads
/// as the statement it runs. Like [`write_statement_kind`], both readings of
/// `#` are tried and either one finding an unqualified write counts.
pub fn unqualified_write(sql: &str) -> Option<UnqualifiedWrite> {
    unqualified(sql).or_else(|| unqualified(&blank_hash_comments(sql)))
}

/// [`unqualified_write`] for one reading, with `#` as ordinary text.
fn unqualified(sql: &str) -> Option<UnqualifiedWrite> {
    let (keyword, end) = keyword_span(sql)?;
    match keyword.as_str() {
        "UPDATE" => {
            let (table, _) = identifier_at(sql, end)?;
            (!has_outer_where(sql)).then(|| UnqualifiedWrite { kind: "UPDATE", table })
        }
        "DELETE" => delete_write(sql, end),
        "WITH" => {
            let (bodies, main) = with_clause(sql, end)?;
            bodies
                .iter()
                .find_map(|body| unqualified(&sql[body.clone()]))
                .or_else(|| unqualified(&sql[main..]))
        }
        "EXPLAIN" => explain_inner(sql, end).and_then(unqualified),
        _ => None,
    }
}

/// One `DELETE` with no outer WHERE. The single-table form reads its target
/// after `FROM`; MySQL's multi-table form names its targets first and copies
/// only those tables, so it confirms too as long as a top-level `FROM`
/// follows the target list.
fn delete_write(sql: &str, after_delete: usize) -> Option<UnqualifiedWrite> {
    let mut pos = skip_ignorable(sql, after_delete);
    let (word, after_word) = loop {
        let (word, next) = word_at(sql, pos)?;
        if word.eq_ignore_ascii_case("from") || !is_delete_modifier(&word) {
            break (word, next);
        }
        pos = skip_ignorable(sql, next);
    };
    if word.eq_ignore_ascii_case("from") {
        let (table, _) = identifier_at(sql, after_word)?;
        return (!has_outer_where(sql))
            .then_some(UnqualifiedWrite { kind: "DELETE", table });
    }
    let (table, after_target) = identifier_at(sql, pos)?;
    if !targets_from(sql, after_target) || has_outer_where(sql) {
        return None;
    }
    Some(UnqualifiedWrite { kind: "DELETE", table })
}

/// MySQL's optional words between `DELETE` and its target list.
fn is_delete_modifier(word: &str) -> bool {
    ["LOW_PRIORITY", "QUICK", "IGNORE"]
        .iter()
        .any(|modifier| word.eq_ignore_ascii_case(modifier))
}

/// True when a top-level `FROM` comes before any top-level `WHERE` in
/// `sql[start..]`, the tail of a multi-table `DELETE`'s target list.
fn targets_from(sql: &str, start: usize) -> bool {
    first_top_level_word(sql, start, &["FROM", "WHERE"])
        .is_some_and(|word| word.eq_ignore_ascii_case("from"))
}

/// A readable `WITH` list: one byte range per CTE body and the offset where
/// the statement after the list begins. None when the text does not read as
/// `WITH [RECURSIVE] <name> [(<columns>)] AS [[NOT] MATERIALIZED] (...)`.
fn with_clause(sql: &str, after_with: usize) -> Option<(Vec<Range<usize>>, usize)> {
    let mut pos = skip_ignorable(sql, after_with);
    if let Some((word, next)) = word_at(sql, pos)
        && word.eq_ignore_ascii_case("recursive")
    {
        pos = skip_ignorable(sql, next);
    }
    let mut bodies = Vec::new();
    loop {
        let (_, next) = identifier_at(sql, pos)?;
        pos = skip_ignorable(sql, next);
        if sql.as_bytes().get(pos) == Some(&b'(') {
            pos = skip_ignorable(sql, matching_paren(sql, pos)? + 1);
        }
        let (word, next) = word_at(sql, pos)?;
        if !word.eq_ignore_ascii_case("as") {
            return None;
        }
        pos = skip_ignorable(sql, next);
        if let Some((word, next)) = word_at(sql, pos) {
            if word.eq_ignore_ascii_case("not") {
                pos = skip_ignorable(sql, next);
                let (word, next) = word_at(sql, pos)?;
                if !word.eq_ignore_ascii_case("materialized") {
                    return None;
                }
                pos = skip_ignorable(sql, next);
            } else if word.eq_ignore_ascii_case("materialized") {
                pos = skip_ignorable(sql, next);
            }
        }
        if sql.as_bytes().get(pos) != Some(&b'(') {
            return None;
        }
        let close = matching_paren(sql, pos)?;
        bodies.push(pos + 1..close);
        pos = skip_ignorable(sql, close + 1);
        if sql.as_bytes().get(pos) != Some(&b',') {
            return Some((bodies, pos));
        }
        pos = skip_ignorable(sql, pos + 1);
    }
}

/// The statement an `EXPLAIN` runs: Some only when its options turn `ANALYZE`
/// on, bare or inside the parenthesized option list. A plain plan never
/// executes the statement it describes.
fn explain_inner(sql: &str, after_explain: usize) -> Option<&str> {
    let pos = skip_ignorable(sql, after_explain);
    if sql.as_bytes().get(pos) == Some(&b'(') {
        let close = matching_paren(sql, pos)?;
        return analyze_enabled(&sql[pos + 1..close]).then(|| &sql[close + 1..]);
    }
    let (word, next) = word_at(sql, pos)?;
    if word.eq_ignore_ascii_case("verbose") {
        let (word, next) = word_at(sql, skip_ignorable(sql, next))?;
        return word.eq_ignore_ascii_case("analyze").then(|| &sql[next..]);
    }
    word.eq_ignore_ascii_case("analyze").then(|| &sql[next..])
}

/// True when a parenthesized `EXPLAIN` option list turns `ANALYZE` on.
fn analyze_enabled(options: &str) -> bool {
    options.split(',').any(analyze_on)
}

/// One option in an `EXPLAIN ( ... )` list, true when it is `ANALYZE` left on.
/// The option is off only when it is spelled false, off, or 0.
fn analyze_on(option: &str) -> bool {
    let Some((word, next)) = word_at(option, skip_ignorable(option, 0)) else {
        return false;
    };
    if !word.eq_ignore_ascii_case("analyze") {
        return false;
    }
    match word_at(option, skip_ignorable(option, next)) {
        Some((value, _)) => {
            !value.eq_ignore_ascii_case("false")
                && !value.eq_ignore_ascii_case("off")
                && value != "0"
        }
        None => true,
    }
}

/// Byte offset of the `)` matching the `(` at `open`, outside strings, quoted
/// identifiers, comments, and dollar-quoted literals. None when the parens
/// never close.
fn matching_paren(sql: &str, open: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let mut depth: i32 = 0;
    let mut i = open;
    while i < bytes.len() {
        if let Some(token) = skipped_token(sql, i, false) {
            i = token.end();
            continue;
        }
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The bare transaction controls. Only these spellings are special: the
/// toolbar sends them, and a typed `begin` matches the button's `BEGIN`.
#[cfg(any(feature = "postgres", feature = "mysql"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxControl {
    Begin,
    Commit,
    Rollback,
}

/// MySQL refuses BEGIN in the prepared statement protocol, so transaction
/// controls run on the text protocol through `raw_sql` there.
#[cfg(any(feature = "postgres", feature = "mysql"))]
fn tx_control(sql: &str) -> Option<TxControl> {
    let (keyword, _) = keyword_span(sql)?;
    match keyword.as_str() {
        "BEGIN" => Some(TxControl::Begin),
        "COMMIT" => Some(TxControl::Commit),
        "ROLLBACK" => Some(TxControl::Rollback),
        _ => None,
    }
}

/// What a manual-mode statement needs before it runs on the pinned
/// connection, given whether a transaction is already open there.
#[cfg(any(feature = "postgres", feature = "mysql"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualPlan {
    /// Run the statement alone.
    Only,
    /// Open the transaction implicitly, then run the statement inside it.
    BeginThenRun,
    /// BEGIN while a transaction is open: run nothing. MySQL's second BEGIN
    /// would implicitly commit the open transaction; Postgres only warns.
    Skip,
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
fn manual_plan(open: bool, control: Option<TxControl>) -> ManualPlan {
    match control {
        Some(TxControl::Begin) if open => ManualPlan::Skip,
        Some(_) => ManualPlan::Only,
        None if open => ManualPlan::Only,
        None => ManualPlan::BeginThenRun,
    }
}

/// Uppercase keyword at the head of `sql` and the byte offset after it,
/// skipping whitespace, `--` and `#` line comments, block comments, and the
/// wrapping parens and terminators around the head.
fn keyword_span(sql: &str) -> Option<(String, usize)> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if matches!(bytes.get(i), Some(&b'(') | Some(&b';')) {
            i += 1;
            continue;
        }
        match skipped_token(sql, i, false) {
            Some(Skipped::Comment { end }) => i = end,
            _ => break,
        }
    }
    let start = i;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
    {
        i += 1;
    }
    if start == i {
        return None;
    }
    Some((sql[start..i].to_ascii_uppercase(), i))
}

/// Byte offset past whitespace and SQL comments.
fn skip_ignorable(sql: &str, mut i: usize) -> usize {
    let bytes = sql.as_bytes();
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        match skipped_token(sql, i, false) {
            Some(Skipped::Comment { end }) => i = end,
            _ => return i,
        }
    }
}

/// One ASCII word at `i` with the offset after it.
fn word_at(sql: &str, i: usize) -> Option<(String, usize)> {
    let bytes = sql.as_bytes();
    let mut end = i;
    while end < bytes.len()
        && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'$')
    {
        end += 1;
    }
    if end == i {
        return None;
    }
    Some((sql[i..end].to_owned(), end))
}

/// One table name at `i`: a bare or quoted part with dots joining further
/// parts, and a trailing `.*` reading as the same table. Quoted parts
/// unescape; each part keeps its text.
fn identifier_at(sql: &str, i: usize) -> Option<(String, usize)> {
    let mut pos = skip_ignorable(sql, i);
    let mut parts = Vec::new();
    loop {
        let bytes = sql.as_bytes();
        match bytes.get(pos) {
            Some(&quote @ (b'"' | b'`')) => {
                let end = scan_quote(bytes, pos, quote);
                if bytes.get(end - 1) != Some(&quote) {
                    return None;
                }
                let text = unquote(&sql[pos..end], quote);
                if text.trim().is_empty() {
                    return None;
                }
                parts.push(text);
                pos = end;
            }
            _ => {
                let (word, next) = word_at(sql, pos)?;
                parts.push(word);
                pos = next;
            }
        }
        let after = skip_ignorable(sql, pos);
        if bytes.get(after) == Some(&b'.') {
            let next_part = skip_ignorable(sql, after + 1);
            if bytes.get(next_part) == Some(&b'*') {
                pos = skip_ignorable(sql, next_part + 1);
                break;
            }
            pos = next_part;
            continue;
        }
        pos = after;
        break;
    }
    (!parts.is_empty()).then(|| (parts.join("."), pos))
}

/// `text`, a quoted identifier including its quotes, with the quotes removed
/// and each doubled quote inside collapsed to one, the SQL standard escape.
fn unquote(text: &str, quote: u8) -> String {
    let inner = &text[1..text.len() - 1];
    let bytes = inner.as_bytes();
    let mut out = String::with_capacity(inner.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == quote && bytes.get(i + 1) == Some(&quote) {
            out.push(char::from(quote));
            i += 2;
            continue;
        }
        let character = inner[i..].chars().next().expect("non-empty slice");
        out.push(character);
        i += character.len_utf8();
    }
    out
}

/// True when a WHERE keyword sits at paren depth zero outside strings,
/// quoted identifiers, comments, and dollar-quoted literals.
fn has_outer_where(sql: &str) -> bool {
    first_top_level_word(sql, 0, &["WHERE"]).is_some()
}

/// The first word in `words`, matched case-insensitively, at paren depth zero
/// in `sql[start..]`. Strings, quoted identifiers, comments, and
/// dollar-quoted literals never count. None when none of `words` appears.
fn first_top_level_word<'a>(sql: &'a str, start: usize, words: &[&str]) -> Option<&'a str> {
    let bytes = sql.as_bytes();
    let mut depth: i32 = 0;
    let mut i = start;
    while i < bytes.len() {
        if let Some(token) = skipped_token(sql, i, false) {
            i = token.end();
            continue;
        }
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let word_start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric()
                        || bytes[i] == b'_'
                        || bytes[i] == b'$')
                {
                    i += 1;
                }
                if depth == 0
                    && words
                        .iter()
                        .any(|word| sql[word_start..i].eq_ignore_ascii_case(word))
                {
                    return Some(&sql[word_start..i]);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// The single list of bundled engines. The connection form iterates
/// `all()`, so a new backend appears in the UI with no UI change.
#[derive(Debug, Clone, Copy)]
pub struct DriverRegistry {
    kinds: &'static [DbKind],
}

impl DriverRegistry {
    pub fn bundled() -> Self {
        Self { kinds: &DbKind::ALL }
    }
    pub fn all(&self) -> impl Iterator<Item = DbKind> {
        self.kinds.iter().copied()
    }
}

/// Postgres TLS policy. Appended as `sslmode` on the connection string.
/// `Disable` is the default and leaves the URL untouched, so existing
/// profiles keep their old strings. Later additive options copy this shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PostgresSslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl Default for PostgresSslMode {
    fn default() -> Self {
        Self::Disable
    }
}

impl PostgresSslMode {
    pub const ALL: [PostgresSslMode; 5] = [
        PostgresSslMode::Disable,
        PostgresSslMode::Prefer,
        PostgresSslMode::Require,
        PostgresSslMode::VerifyCa,
        PostgresSslMode::VerifyFull,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            PostgresSslMode::Disable => "disable",
            PostgresSslMode::Prefer => "prefer",
            PostgresSslMode::Require => "require",
            PostgresSslMode::VerifyCa => "verify-ca",
            PostgresSslMode::VerifyFull => "verify-full",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            PostgresSslMode::Disable => "Disable",
            PostgresSslMode::Prefer => "Prefer",
            PostgresSslMode::Require => "Require",
            PostgresSslMode::VerifyCa => "Verify CA",
            PostgresSslMode::VerifyFull => "Verify Full",
        }
    }
}

/// MySQL TLS policy. `VerifyIdentity` is MySQL's counterpart of Postgres
/// `verify-full`; the rest mirror the Postgres set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MysqlSslMode {
    Disabled,
    Preferred,
    Required,
    VerifyCa,
    VerifyIdentity,
}

impl Default for MysqlSslMode {
    fn default() -> Self {
        Self::Preferred
    }
}

impl MysqlSslMode {
    pub const ALL: [MysqlSslMode; 5] = [
        MysqlSslMode::Disabled,
        MysqlSslMode::Preferred,
        MysqlSslMode::Required,
        MysqlSslMode::VerifyCa,
        MysqlSslMode::VerifyIdentity,
    ];
    /// The value sqlx's MySQL URL parser accepts, which is not the Postgres
    /// spelling. sqlx rejects `prefer`, `verify-ca`, and `verify-full`.
    pub fn as_str(self) -> &'static str {
        match self {
            MysqlSslMode::Disabled => "disabled",
            MysqlSslMode::Preferred => "preferred",
            MysqlSslMode::Required => "required",
            MysqlSslMode::VerifyCa => "verify_ca",
            MysqlSslMode::VerifyIdentity => "verify_identity",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            MysqlSslMode::Disabled => "Disabled",
            MysqlSslMode::Preferred => "Preferred",
            MysqlSslMode::Required => "Required",
            MysqlSslMode::VerifyCa => "Verify CA",
            MysqlSslMode::VerifyIdentity => "Verify Identity",
        }
    }
}

/// Persisted settings, one variant per engine. The variant IS the kind.
/// Serialized shape (the `kind` tag) is the on-disk contract.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConnectionConfig {
    Sqlite {
        path: PathBuf,
        #[serde(default)]
        read_only: bool,
    },
    Postgres {
        host: String,
        port: u16,
        database: String,
        user: String,
        #[serde(default)]
        ssl_mode: PostgresSslMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh: Option<SshConfig>,
        #[serde(default)]
        read_only: bool,
    },
    Mysql {
        host: String,
        port: u16,
        database: String,
        user: String,
        #[serde(default)]
        ssl_mode: MysqlSslMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh: Option<SshConfig>,
        #[serde(default)]
        read_only: bool,
    },
}

impl ConnectionConfig {
    pub fn kind(&self) -> DbKind {
        match self {
            ConnectionConfig::Sqlite { .. } => DbKind::Sqlite,
            ConnectionConfig::Postgres { .. } => DbKind::Postgres,
            ConnectionConfig::Mysql { .. } => DbKind::Mysql,
        }
    }
    /// The engines the UI prompts for a password when the profile stores none.
    pub fn prompts_for_password(&self) -> bool {
        matches!(self, ConnectionConfig::Postgres { .. } | ConnectionConfig::Mysql { .. })
    }
    pub fn is_read_only(&self) -> bool {
        match self {
            ConnectionConfig::Sqlite { read_only, .. }
            | ConnectionConfig::Postgres { read_only, .. }
            | ConnectionConfig::Mysql { read_only, .. } => *read_only,
        }
    }
    pub fn set_read_only(&mut self, read_only: bool) {
        match self {
            ConnectionConfig::Sqlite { read_only: flag, .. }
            | ConnectionConfig::Postgres { read_only: flag, .. }
            | ConnectionConfig::Mysql { read_only: flag, .. } => *flag = read_only,
        }
    }
    /// Boundary check before a session opens. Shared by the form and the worker.
    pub fn validate(&self) -> Result<(), DbError> {
        match self {
            ConnectionConfig::Sqlite { path, .. } => {
                if path.as_os_str().is_empty() {
                    return Err(DbError::Config("SQLite path is empty".to_owned()));
                }
                Ok(())
            }
            ConnectionConfig::Postgres { host, port, database, user, ssh, .. }
            | ConnectionConfig::Mysql { host, port, database, user, ssh, .. } => {
                if host.trim().is_empty() {
                    return Err(DbError::Config("host is empty".to_owned()));
                }
                if *port == 0 {
                    return Err(DbError::Config("port is not set".to_owned()));
                }
                if database.trim().is_empty() {
                    return Err(DbError::Config("database is empty".to_owned()));
                }
                if user.trim().is_empty() {
                    return Err(DbError::Config("user is empty".to_owned()));
                }
                if let Some(ssh) = ssh {
                    ssh.validate()?;
                }
                Ok(())
            }
        }
    }
}

/// One cell. Every backend maps into these five shapes at the boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    /// Single source of truth for how a DB value prints. `cap` bounds text
    /// length; blobs print as a size tag with no payload.
    pub fn display(&self, cap: usize) -> Cow<'_, str> {
        match self {
            Value::Null => Cow::Borrowed("NULL"),
            Value::Integer(n) => Cow::Owned(n.to_string()),
            Value::Real(f) => Cow::Owned(f.to_string()),
            Value::Text(s) => {
                if s.chars().count() > cap {
                    Cow::Owned(s.chars().take(cap).collect::<String>() + "…")
                } else {
                    Cow::Borrowed(s)
                }
            }
            Value::Blob(b) => Cow::Owned(format!("[BLOB {}]", format_bytes(b.len()))),
        }
    }
}

fn format_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else {
        format!("{:.1} KB", n as f64 / 1024.0)
    }
}

/// One result column. Fields are private; construction checks happen in
/// [`QueryResult::from_columns`], so ragged results cannot exist.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    name: String,
    type_name: String,
    values: Vec<Value>,
}

impl Column {
    pub fn new(name: String, type_name: String, values: Vec<Value>) -> Self {
        Self { name, type_name, values }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn type_name(&self) -> &str {
        &self.type_name
    }
    pub fn values(&self) -> &[Value] {
        &self.values
    }
    /// Split one page column into its parts for [`QueryResult::append_page`].
    fn into_parts(self) -> (String, String, Vec<Value>) {
        (self.name, self.type_name, self.values)
    }
    /// Append page values onto the loaded column.
    fn append_values(&mut self, values: &mut Vec<Value>) {
        self.values.append(values);
    }
}

/// A materialized result set in column-major order. `affected` is Some only
/// for statements without result columns. `truncated` says the row cap hit.
/// `total` carries an on-demand exact count beside the flag when one was
/// fetched, so the grid can name the full size without loading it.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    columns: Vec<Column>,
    row_count: usize,
    truncated: bool,
    affected: Option<u64>,
    took: Duration,
    total: Option<u64>,
}

impl QueryResult {
    /// The only checked constructor. Rejects ragged columns.
    pub fn from_columns(
        columns: Vec<Column>,
        truncated: bool,
        affected: Option<u64>,
        took: Duration,
    ) -> Result<Self, ShapeError> {
        let row_count = columns.first().map(|c| c.values.len()).unwrap_or(0);
        if columns.iter().any(|c| c.values.len() != row_count) {
            return Err(ShapeError::Ragged { expected: row_count });
        }
        Ok(Self { columns, row_count, truncated, affected, took, total: None })
    }
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }
    pub fn row_count(&self) -> usize {
        self.row_count
    }
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }
    pub fn affected(&self) -> Option<u64> {
        self.affected
    }
    pub fn took(&self) -> Duration {
        self.took
    }
    /// The on-demand total behind the truncation flag, if one was counted.
    pub fn total(&self) -> Option<u64> {
        self.total
    }
    /// Record the on-demand total beside the flag.
    pub fn set_total(&mut self, total: u64) {
        self.total = Some(total);
    }
    /// Append one page onto the loaded rows. Column counts and names must
    /// match positionally; the page's flag becomes the combined flag and a
    /// page total never carries over. Returns the page's flag, which says
    /// whether another page remains.
    pub fn append_page(&mut self, page: QueryResult) -> Result<bool, ShapeError> {
        if page.columns.len() != self.columns.len() {
            return Err(ShapeError::Ragged { expected: self.row_count });
        }
        for (own, next) in self.columns.iter().zip(page.columns.iter()) {
            if own.name() != next.name() {
                return Err(ShapeError::Ragged { expected: self.row_count });
            }
        }
        let has_more = page.truncated;
        for (own, next) in self.columns.iter_mut().zip(page.columns.into_iter()) {
            let (_, _, mut values) = next.into_parts();
            own.append_values(&mut values);
        }
        self.row_count = self.columns.first().map(|c| c.values.len()).unwrap_or(0);
        self.truncated = has_more;
        Ok(has_more)
    }
    /// Bounds-checked: the grid's visible range comes from the previous frame
    /// and the result can be replaced between frames.
    pub fn cell(&self, row: usize, col: usize) -> Option<&Value> {
        self.columns.get(col).and_then(|c| c.values.get(row))
    }
}

/// One table's row count. SQLite counts exactly; Postgres and MySQL read
/// their catalog's estimate, so the UI can label those honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowCount {
    Exact(u64),
    Estimate(u64),
}

impl RowCount {
    pub fn value(self) -> u64 {
        match self {
            RowCount::Exact(n) | RowCount::Estimate(n) => n,
        }
    }
    pub fn is_estimate(self) -> bool {
        matches!(self, RowCount::Estimate(_))
    }
    /// Display text: a bare number when exact, a tilde prefix on estimates.
    pub fn label(self) -> String {
        match self {
            RowCount::Exact(n) => n.to_string(),
            RowCount::Estimate(n) => format!("~{n}"),
        }
    }
}

/// Cap on sessions materialized per list. The panel shows the first page.
pub const SESSION_CAP: usize = 200;

/// One live backend behind a connection: who it is, what it runs, and what
/// lock it waits on. Built by read-only introspection queries; SQLite has
/// none. Fields are private; construction checks happen in
/// [`SessionInfo::new`], so an empty backend id cannot exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    backend_id: String,
    user: String,
    database: String,
    state: String,
    query: String,
    waiting: bool,
    locks: String,
    is_self: bool,
}

impl SessionInfo {
    /// Checked constructor. Rejects an empty backend id; every other field
    /// keeps its text because engines report blanks and NULLs for idle or
    /// restricted backends.
    pub fn new(
        backend_id: String,
        user: String,
        database: String,
        state: String,
        query: String,
        waiting: bool,
        locks: String,
        is_self: bool,
    ) -> Result<Self, SchemaError> {
        if backend_id.trim().is_empty() {
            return Err(SchemaError::EmptyName("session"));
        }
        Ok(Self { backend_id, user, database, state, query, waiting, locks, is_self })
    }
    pub fn backend_id(&self) -> &str {
        &self.backend_id
    }
    pub fn user(&self) -> &str {
        &self.user
    }
    pub fn database(&self) -> &str {
        &self.database
    }
    pub fn state(&self) -> &str {
        &self.state
    }
    pub fn query(&self) -> &str {
        &self.query
    }
    pub fn waiting(&self) -> bool {
        self.waiting
    }
    pub fn locks(&self) -> &str {
        &self.locks
    }
    /// True for the backend running this listing, which terminate refuses.
    pub fn is_self(&self) -> bool {
        self.is_self
    }
    /// The panel's lock note: empty when the backend waits on nothing.
    pub fn wait_note(&self) -> &str {
        if !self.waiting {
            return "";
        }
        if self.locks.is_empty() { "waiting on lock" } else { &self.locks }
    }
}

/// The Postgres session list: one row per `pg_stat_activity` backend with
/// its ungranted `pg_locks` aggregated. Column order is the decode contract
/// in [`pg_session_from_parts`]: pid, user, database, state, query, waiting,
/// locks.
#[cfg(any(test, feature = "postgres"))]
pub fn pg_sessions_sql() -> &'static str {
    "SELECT a.pid, a.usename, a.datname, a.state, a.query, (a.wait_event IS NOT NULL), \
     COALESCE((SELECT string_agg(l.locktype || ' ' || l.mode, ', ' \
     ORDER BY l.locktype, l.mode) FROM pg_locks l \
     WHERE l.pid = a.pid AND NOT l.granted), '') \
     FROM pg_stat_activity a ORDER BY a.pid"
}

/// The MySQL session list: one row per `information_schema.processlist`
/// entry. Column order is the decode contract in
/// [`mysql_session_from_parts`]: id, user, database, command, state, query.
#[cfg(any(test, feature = "mysql"))]
pub fn mysql_sessions_sql() -> &'static str {
    "SELECT ID, USER, DB, COMMAND, STATE, INFO \
     FROM information_schema.processlist ORDER BY ID"
}

/// The MySQL lock waits behind the list: each requesting processlist id with
/// its blocking processlist id, if that thread has one. Best effort: a
/// missing or disabled `performance_schema` reads as no waits.
#[cfg(any(test, feature = "mysql"))]
pub fn mysql_lock_waits_sql() -> &'static str {
    "SELECT rt.PROCESSLIST_ID, bt.PROCESSLIST_ID \
     FROM performance_schema.data_lock_waits w \
     JOIN performance_schema.threads rt ON rt.THREAD_ID = w.REQUESTING_THREAD_ID \
     LEFT JOIN performance_schema.threads bt ON bt.THREAD_ID = w.BLOCKING_THREAD_ID \
     WHERE rt.PROCESSLIST_ID IS NOT NULL"
}

/// Terminate one Postgres backend. `pid` is typed, so the id cannot inject.
#[cfg(any(test, feature = "postgres"))]
pub fn pg_terminate_sql(pid: i32) -> String {
    format!("SELECT pg_terminate_backend({pid})")
}

/// Kill one MySQL connection. `id` is typed, so the id cannot inject.
#[cfg(any(test, feature = "mysql"))]
pub fn mysql_terminate_sql(id: u64) -> String {
    format!("KILL {id}")
}

/// One Postgres introspection row as a [`SessionInfo`]. NULLs become empty
/// text because restricted backends hide their user, database, or query.
#[cfg(any(test, feature = "postgres"))]
pub(crate) fn pg_session_from_parts(
    pid: i32,
    user: Option<String>,
    database: Option<String>,
    state: Option<String>,
    query: Option<String>,
    waiting: bool,
    locks: Option<String>,
    is_self: bool,
) -> Result<SessionInfo, DbError> {
    SessionInfo::new(
        pid.to_string(),
        user.unwrap_or_default(),
        database.unwrap_or_default(),
        state.unwrap_or_default(),
        query.unwrap_or_default(),
        waiting,
        locks.unwrap_or_default(),
        is_self,
    )
    .map_err(|e| DbError::Query(e.to_string()))
}

/// One MySQL processlist row as a [`SessionInfo`]. NULLs become empty text.
#[cfg(any(test, feature = "mysql"))]
pub(crate) fn mysql_session_from_parts(
    id: u64,
    user: Option<String>,
    database: Option<String>,
    command: Option<String>,
    state: Option<String>,
    query: Option<String>,
    is_self: bool,
) -> Result<SessionInfo, DbError> {
    SessionInfo::new(
        id.to_string(),
        user.unwrap_or_default(),
        database.unwrap_or_default(),
        mysql_state_text(command, state),
        query.unwrap_or_default(),
        false,
        String::new(),
        is_self,
    )
    .map_err(|e| DbError::Query(e.to_string()))
}

/// The MySQL state text from its command plus detail words: each alone when
/// the other is blank, joined when both name something, empty when neither.
#[cfg(any(test, feature = "mysql"))]
pub(crate) fn mysql_state_text(command: Option<String>, state: Option<String>) -> String {
    let command = command.unwrap_or_default();
    let state = state.unwrap_or_default();
    match (command.is_empty(), state.is_empty()) {
        (true, true) => String::new(),
        (false, true) => command,
        (true, false) => state,
        (false, false) => format!("{command} · {state}"),
    }
}

/// Fold MySQL lock waits into a loaded list: each requesting backend waits,
/// named by its blocker when that thread has a processlist id.
#[cfg(any(test, feature = "mysql"))]
pub(crate) fn apply_mysql_lock_waits(
    sessions: &mut [SessionInfo],
    waits: &[(u64, Option<u64>)],
) {
    for (requesting, blocking) in waits {
        let wanted = requesting.to_string();
        if let Some(session) = sessions.iter_mut().find(|s| s.backend_id == wanted) {
            session.waiting = true;
            session.locks = match blocking {
                Some(blocker) => format!("blocked by {blocker}"),
                None => "waiting on lock".to_owned(),
            };
        }
    }
}

/// One Postgres backend id from panel text. Rejects blanks, non-numbers,
/// and non-positive pids, so interpolated SQL stays an integer literal.
#[cfg(any(test, feature = "postgres"))]
pub(crate) fn parse_pg_backend(backend: &str) -> Result<i32, DbError> {
    backend
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| DbError::Query(format!("unknown backend {backend}")))
}

/// One MySQL connection id from panel text. Rejects blanks, non-numbers,
/// and zero, so interpolated SQL stays an integer literal.
#[cfg(any(test, feature = "mysql"))]
pub(crate) fn parse_mysql_backend(backend: &str) -> Result<u64, DbError> {
    backend
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| DbError::Query(format!("unknown backend {backend}")))
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ShapeError {
    #[error("ragged result: expected {expected} values per column")]
    Ragged { expected: usize },
}

/// Cap on tables materialized per schema load. The schema reports truncation.
pub const SCHEMA_TABLE_CAP: usize = 500;
/// Cap on columns materialized per table. The schema reports truncation.
pub const SCHEMA_COLUMN_CAP: usize = 200;
/// Cap on foreign keys materialized per table. Each table reports truncation.
pub const SCHEMA_FK_CAP: usize = 50;
/// Cap on indexes materialized per table. Each table reports truncation.
pub const SCHEMA_INDEX_CAP: usize = 50;
/// Cap on schemas materialized per database.
pub const SCHEMA_SCHEMA_CAP: usize = 64;
/// Cap on sequences materialized per schema.
pub const SCHEMA_SEQUENCE_CAP: usize = 200;
/// Cap on aggregated schema indexes materialized per schema.
pub const SCHEMA_SCHEMA_INDEX_CAP: usize = 200;
/// Cap on triggers materialized per schema. Each schema reports truncation.
pub const SCHEMA_TRIGGER_CAP: usize = 200;
/// Cap on routines materialized per schema. Each schema reports truncation.
pub const SCHEMA_ROUTINE_CAP: usize = 200;

/// Table or view. The variant is the kind; no stringly matching above this file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Table,
    View,
}

/// One column in a schema tree. Fields are private; construction checks happen
/// in [`ColumnInfo::new`], so empty names cannot exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    name: String,
    type_name: String,
    nullable: bool,
    pk_position: Option<u32>,
}

impl ColumnInfo {
    /// Checked constructor. Rejects empty names.
    pub fn new(
        name: String,
        type_name: String,
        nullable: bool,
        pk_position: Option<u32>,
    ) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("column"));
        }
        Ok(Self { name, type_name, nullable, pk_position })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn type_name(&self) -> &str {
        &self.type_name
    }
    pub fn nullable(&self) -> bool {
        self.nullable
    }
    pub fn pk_position(&self) -> Option<u32> {
        self.pk_position
    }
    pub fn is_pk(&self) -> bool {
        self.pk_position.is_some()
    }
}

/// One foreign key from a table to a referenced table. Fields are private;
/// construction checks happen in [`ForeignKeyInfo::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyInfo {
    columns: Vec<String>,
    ref_table: String,
    ref_columns: Vec<String>,
    constraint_name: Option<String>,
}

impl ForeignKeyInfo {
    /// Checked constructor. Rejects blank table and column names, and a
    /// blank constraint name when one is present.
    pub fn new(
        columns: Vec<String>,
        ref_table: String,
        ref_columns: Vec<String>,
        constraint_name: Option<String>,
    ) -> Result<Self, SchemaError> {
        if ref_table.trim().is_empty() {
            return Err(SchemaError::EmptyName("referenced table"));
        }
        if columns.iter().any(|c| c.trim().is_empty()) {
            return Err(SchemaError::EmptyName("foreign key column"));
        }
        if ref_columns.iter().any(|c| c.trim().is_empty()) {
            return Err(SchemaError::EmptyName("referenced column"));
        }
        if constraint_name.as_ref().is_some_and(|n| n.trim().is_empty()) {
            return Err(SchemaError::EmptyName("foreign key constraint"));
        }
        Ok(Self { columns, ref_table, ref_columns, constraint_name })
    }
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn ref_table(&self) -> &str {
        &self.ref_table
    }
    pub fn ref_columns(&self) -> &[String] {
        &self.ref_columns
    }
    pub fn constraint_name(&self) -> Option<&str> {
        self.constraint_name.as_deref()
    }
}

/// One index on a table. Fields are private; construction checks happen in
/// [`IndexInfo::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    name: String,
    columns: Vec<String>,
    unique: bool,
}

impl IndexInfo {
    /// Checked constructor. Rejects blank index and column names.
    pub fn new(
        name: String,
        columns: Vec<String>,
        unique: bool,
    ) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("index"));
        }
        if columns.iter().any(|c| c.trim().is_empty()) {
            return Err(SchemaError::EmptyName("index column"));
        }
        Ok(Self { name, columns, unique })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn unique(&self) -> bool {
        self.unique
    }
    pub fn is_unique(&self) -> bool {
        self.unique
    }
}

/// One table constraint. Fields are private; construction checks happen in
/// [`ConstraintInfo::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    Pk,
    Unique,
    Check,
    Foreign,
}

impl ConstraintKind {
    pub fn label(self) -> &'static str {
        match self {
            ConstraintKind::Pk => "PRIMARY KEY",
            ConstraintKind::Unique => "UNIQUE",
            ConstraintKind::Check => "CHECK",
            ConstraintKind::Foreign => "FOREIGN KEY",
        }
    }
}

/// One named constraint on a table. `ref_table` is Some only for foreign
/// keys. `check_expr` is Some only for check constraints and holds the
/// `pg_get_constraintdef` output on Postgres.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintInfo {
    name: String,
    kind: ConstraintKind,
    columns: Vec<String>,
    ref_table: Option<String>,
    ref_columns: Vec<String>,
    check_expr: Option<String>,
}

impl ConstraintInfo {
    /// Checked constructor. Rejects blank names and blank nested names.
    pub fn new(
        name: String,
        kind: ConstraintKind,
        columns: Vec<String>,
        ref_table: Option<String>,
        ref_columns: Vec<String>,
        check_expr: Option<String>,
    ) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("constraint"));
        }
        if columns.iter().any(|c| c.trim().is_empty()) {
            return Err(SchemaError::EmptyName("constraint column"));
        }
        if ref_table.as_ref().is_some_and(|n| n.trim().is_empty()) {
            return Err(SchemaError::EmptyName("referenced table"));
        }
        if ref_columns.iter().any(|c| c.trim().is_empty()) {
            return Err(SchemaError::EmptyName("referenced column"));
        }
        if check_expr.as_ref().is_some_and(|e| e.trim().is_empty()) {
            return Err(SchemaError::EmptyName("check expression"));
        }
        Ok(Self { name, kind, columns, ref_table, ref_columns, check_expr })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn kind(&self) -> ConstraintKind {
        self.kind
    }
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn ref_table(&self) -> Option<&str> {
        self.ref_table.as_deref()
    }
    pub fn ref_columns(&self) -> &[String] {
        &self.ref_columns
    }
    pub fn check_expr(&self) -> Option<&str> {
        self.check_expr.as_deref()
    }
}

/// One sequence in a schema. Name only for phase one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceInfo {
    name: String,
}

impl SequenceInfo {
    /// Checked constructor. Rejects empty names.
    pub fn new(name: String) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("sequence"));
        }
        Ok(Self { name })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// One index in the per-schema aggregated folder. Points back to its table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRef {
    table: String,
    index: String,
}

impl IndexRef {
    /// Checked constructor. Rejects blank table and index names.
    pub fn new(table: String, index: String) -> Result<Self, SchemaError> {
        if table.trim().is_empty() {
            return Err(SchemaError::EmptyName("table"));
        }
        if index.trim().is_empty() {
            return Err(SchemaError::EmptyName("index"));
        }
        Ok(Self { table, index })
    }
    pub fn table(&self) -> &str {
        &self.table
    }
    pub fn index(&self) -> &str {
        &self.index
    }
}

/// One trigger in a schema. `table` is the bound table where the engine
/// reports one, `kind` the engine's own event words where it reports them
/// (for example `BEFORE INSERT`), and `definition` the statement text where
/// the engine exposes one. Fields are private; construction checks happen in
/// [`TriggerInfo::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerInfo {
    name: String,
    table: Option<String>,
    kind: Option<String>,
    definition: Option<String>,
}

impl TriggerInfo {
    /// Checked constructor. Rejects a blank name, and a blank table, kind, or
    /// definition when one is present.
    pub fn new(
        name: String,
        table: Option<String>,
        kind: Option<String>,
        definition: Option<String>,
    ) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("trigger"));
        }
        if table.as_ref().is_some_and(|t| t.trim().is_empty()) {
            return Err(SchemaError::EmptyName("trigger table"));
        }
        if kind.as_ref().is_some_and(|k| k.trim().is_empty()) {
            return Err(SchemaError::EmptyName("trigger kind"));
        }
        if definition.as_ref().is_some_and(|d| d.trim().is_empty()) {
            return Err(SchemaError::EmptyName("trigger definition"));
        }
        Ok(Self { name, table, kind, definition })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn table(&self) -> Option<&str> {
        self.table.as_deref()
    }
    pub fn kind(&self) -> Option<&str> {
        self.kind.as_deref()
    }
    pub fn definition(&self) -> Option<&str> {
        self.definition.as_deref()
    }
}

/// One stored routine in a schema. `kind` is the engine's own word (FUNCTION
/// or PROCEDURE), `language` the body language where the engine reports one,
/// and `definition` the body text where the engine exposes one. Fields are
/// private; construction checks happen in [`RoutineInfo::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineInfo {
    name: String,
    kind: Option<String>,
    language: Option<String>,
    definition: Option<String>,
}

impl RoutineInfo {
    /// Checked constructor. Rejects a blank name, and a blank kind, language,
    /// or definition when one is present.
    pub fn new(
        name: String,
        kind: Option<String>,
        language: Option<String>,
        definition: Option<String>,
    ) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("routine"));
        }
        if kind.as_ref().is_some_and(|k| k.trim().is_empty()) {
            return Err(SchemaError::EmptyName("routine kind"));
        }
        if language.as_ref().is_some_and(|l| l.trim().is_empty()) {
            return Err(SchemaError::EmptyName("routine language"));
        }
        if definition.as_ref().is_some_and(|d| d.trim().is_empty()) {
            return Err(SchemaError::EmptyName("routine definition"));
        }
        Ok(Self { name, kind, language, definition })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn kind(&self) -> Option<&str> {
        self.kind.as_deref()
    }
    pub fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }
    pub fn definition(&self) -> Option<&str> {
        self.definition.as_deref()
    }
}

/// One table or view in a schema tree. Fields are private; construction checks
/// happen in [`TableInfo::new`]. `referenced_by` holds incoming foreign keys:
/// each entry copies the child key with `columns` set to the parent columns
/// and `ref_table` set to the child table, so the UI renders
/// `child(child_cols) to parent(parent_cols)` with one clickable name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    name: String,
    kind: TableKind,
    columns: Vec<ColumnInfo>,
    constraints: Vec<ConstraintInfo>,
    foreign_keys: Vec<ForeignKeyInfo>,
    referenced_by: Vec<ForeignKeyInfo>,
    indexes: Vec<IndexInfo>,
    foreign_keys_truncated: bool,
    indexes_truncated: bool,
}

impl TableInfo {
    /// Checked constructor. Rejects empty names.
    pub fn new(
        name: String,
        kind: TableKind,
        columns: Vec<ColumnInfo>,
        constraints: Vec<ConstraintInfo>,
        foreign_keys: Vec<ForeignKeyInfo>,
        referenced_by: Vec<ForeignKeyInfo>,
        indexes: Vec<IndexInfo>,
        foreign_keys_truncated: bool,
        indexes_truncated: bool,
    ) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("table"));
        }
        Ok(Self {
            name,
            kind,
            columns,
            constraints,
            foreign_keys,
            referenced_by,
            indexes,
            foreign_keys_truncated,
            indexes_truncated,
        })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn kind(&self) -> TableKind {
        self.kind
    }
    pub fn columns(&self) -> &[ColumnInfo] {
        &self.columns
    }
    pub fn constraints(&self) -> &[ConstraintInfo] {
        &self.constraints
    }
    pub fn foreign_keys(&self) -> &[ForeignKeyInfo] {
        &self.foreign_keys
    }
    pub fn referenced_by(&self) -> &[ForeignKeyInfo] {
        &self.referenced_by
    }
    pub fn indexes(&self) -> &[IndexInfo] {
        &self.indexes
    }
    pub fn foreign_keys_truncated(&self) -> bool {
        self.foreign_keys_truncated
    }
    pub fn indexes_truncated(&self) -> bool {
        self.indexes_truncated
    }
}

/// One schema in a database: split tables and views plus sequences, the
/// aggregated per-schema index folder, triggers, and routines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaNode {
    name: String,
    tables: Vec<TableInfo>,
    views: Vec<TableInfo>,
    sequences: Vec<SequenceInfo>,
    schema_indexes: Vec<IndexRef>,
    triggers: Vec<TriggerInfo>,
    routines: Vec<RoutineInfo>,
    triggers_truncated: bool,
    routines_truncated: bool,
}

impl SchemaNode {
    pub fn new(
        name: String,
        tables: Vec<TableInfo>,
        views: Vec<TableInfo>,
        sequences: Vec<SequenceInfo>,
        schema_indexes: Vec<IndexRef>,
        triggers: Vec<TriggerInfo>,
        routines: Vec<RoutineInfo>,
        triggers_truncated: bool,
        routines_truncated: bool,
    ) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("schema"));
        }
        Ok(Self {
            name,
            tables,
            views,
            sequences,
            schema_indexes,
            triggers,
            routines,
            triggers_truncated,
            routines_truncated,
        })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn tables(&self) -> &[TableInfo] {
        &self.tables
    }
    pub fn views(&self) -> &[TableInfo] {
        &self.views
    }
    pub fn sequences(&self) -> &[SequenceInfo] {
        &self.sequences
    }
    pub fn schema_indexes(&self) -> &[IndexRef] {
        &self.schema_indexes
    }
    pub fn triggers(&self) -> &[TriggerInfo] {
        &self.triggers
    }
    pub fn routines(&self) -> &[RoutineInfo] {
        &self.routines
    }
    pub fn triggers_truncated(&self) -> bool {
        self.triggers_truncated
    }
    pub fn routines_truncated(&self) -> bool {
        self.routines_truncated
    }
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
            && self.views.is_empty()
            && self.sequences.is_empty()
            && self.schema_indexes.is_empty()
            && self.triggers.is_empty()
            && self.routines.is_empty()
    }
}

/// One database in the tree: a name plus its schemas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseNode {
    name: String,
    schemas: Vec<SchemaNode>,
}

impl DatabaseNode {
    pub fn new(name: String, schemas: Vec<SchemaNode>) -> Result<Self, SchemaError> {
        if name.trim().is_empty() {
            return Err(SchemaError::EmptyName("database"));
        }
        Ok(Self { name, schemas })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn schemas(&self) -> &[SchemaNode] {
        &self.schemas
    }
}

/// A browsable database tree: ordered databases with truncation flags.
/// Fields are private; drivers build it, the UI reads it. Memory only, so
/// reshaping needs no migration and the persist shape is untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaInfo {
    databases: Vec<DatabaseNode>,
    tables_truncated: bool,
    columns_truncated: bool,
}

impl SchemaInfo {
    pub fn new(
        databases: Vec<DatabaseNode>,
        tables_truncated: bool,
        columns_truncated: bool,
    ) -> Self {
        Self { databases, tables_truncated, columns_truncated }
    }
    pub fn databases(&self) -> &[DatabaseNode] {
        &self.databases
    }
    pub fn tables_truncated(&self) -> bool {
        self.tables_truncated
    }
    pub fn columns_truncated(&self) -> bool {
        self.columns_truncated
    }
    pub fn is_empty(&self) -> bool {
        self.databases.iter().all(|d| d.schemas().iter().all(|s| s.is_empty()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    #[error("{0} name is empty")]
    EmptyName(&'static str),
}

/// Row-wise constructor used by drivers. Buffers rows, transposes at finish.
pub struct ResultBuilder {
    columns: Vec<(String, String)>,
    rows: Vec<Vec<Value>>,
    truncated: bool,
    max_rows: usize,
}

impl ResultBuilder {
    pub fn new(columns: Vec<(String, String)>, max_rows: usize) -> Self {
        Self { columns, rows: Vec::new(), truncated: false, max_rows }
    }
    /// Append one row. Returns false when the row limit is reached; the
    /// driver then stops pulling rows and `finish` reports truncation.
    /// Panics on a width mismatch, which is a driver bug, not user input.
    pub fn push_row(&mut self, cells: Vec<Value>) -> bool {
        assert_eq!(cells.len(), self.columns.len(), "driver pushed a ragged row");
        if self.rows.len() >= self.max_rows {
            self.truncated = true;
            return false;
        }
        self.rows.push(cells);
        true
    }
    pub fn finish(self, took: Duration) -> QueryResult {
        let width = self.columns.len();
        let mut cols: Vec<Column> = self
            .columns
            .into_iter()
            .enumerate()
            .map(|(i, (name, type_name))| {
                let values = self.rows.iter().map(|r| r[i].clone()).collect();
                Column::new(name, type_name, values)
            })
            .collect();
        let _ = width;
        let row_count = self.rows.len();
        let truncated = self.truncated;
        debug_assert!(cols.iter().all(|c| c.values.len() == row_count));
        let _ = &mut cols;
        QueryResult { columns: cols, row_count, truncated, affected: None, took, total: None }
    }
}

/// Every failure the db layer reports, in domain terms. Client library
/// error types never escape their driver code.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DbError {
    #[error("could not open: {0}")]
    Open(String),
    #[error("query failed: {0}")]
    Query(String),
    #[error("connection is gone")]
    Gone,
    #[error("invalid settings: {0}")]
    Config(String),
    #[error("not supported: {0}")]
    NotSupported(String),
}

impl DbError {
    /// Single source for the fold's choice between "show this in the tab"
    /// and "move the connection to Error".
    pub fn is_fatal(&self) -> bool {
        !matches!(self, DbError::Query(_))
    }
}

/// Fires the engine's interrupt for the statement in flight. Safe from any
/// thread, safe while idle, safe to call twice. This is the cancel upgrade
/// path: the UI fires it, the worker never reads a cancel command.
#[derive(Clone)]
pub struct CancelHandle(Arc<dyn Fn() + Send + Sync + 'static>);

impl CancelHandle {
    pub fn new(fire: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(fire))
    }
    pub fn cancel(&self) {
        (self.0)();
    }
}

impl std::fmt::Debug for CancelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CancelHandle")
    }
}

/// The only way to talk to a worker. Holding one means the worker was
/// spawned; `send` fails only after the worker thread exited.
#[derive(Debug, Clone)]
pub struct DbHandle {
    cmds: mpsc::Sender<DbCommand>,
}

impl DbHandle {
    pub fn send(&self, cmd: DbCommand) -> Result<(), DbError> {
        self.cmds.send(cmd).map_err(|_| DbError::Gone)
    }
}

/// Test seam: a handle whose commands land in the returned receiver, so a
/// caller can assert what it sends without spawning a worker thread.
#[cfg(test)]
pub fn test_handle() -> (DbHandle, mpsc::Receiver<DbCommand>) {
    let (cmds_tx, cmds_rx) = mpsc::channel();
    (DbHandle { cmds: cmds_tx }, cmds_rx)
}

/// Idle minutes before the worker rolls back an open manual transaction.
/// The warning lands one minute earlier, derived from this one number.
pub const TX_IDLE_ROLLBACK_MINUTES: u64 = 10;

/// Idle timing for open manual transactions. The default follows
/// [`TX_IDLE_ROLLBACK_MINUTES`]; tests inject a short schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxIdle {
    /// Warn once when a transaction has been idle for this long.
    pub warn: Duration,
    /// Roll the transaction back when it has been idle this long.
    pub rollback: Duration,
}

impl Default for TxIdle {
    fn default() -> Self {
        let rollback = Duration::from_secs(TX_IDLE_ROLLBACK_MINUTES * 60);
        Self { warn: rollback - Duration::from_secs(60), rollback }
    }
}

/// One idle tick's outcome for the worker to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleTick {
    Warn,
    Rollback,
}

#[derive(Debug)]
pub enum DbCommand {
    QueryBatch { run: RunId, statements: Vec<String> },
    LoadSchema,
    /// One table's row count, on the navigator's explicit request.
    RowCount { schema: String, table: String },
    /// The live backends, on the session panel's explicit request.
    ListSessions,
    /// Terminate one backend by its [`SessionInfo::backend_id`].
    TerminateSession { backend: String },
    /// Flip the worker's autocommit for this session.
    SetAutocommit { on: bool },
    /// One page of a grid query past the row cap. `tab` routes the landing
    /// back to the grid without touching the tab's running run.
    QueryPage { run: RunId, tab: TabId, sql: String, offset: u64, limit: u64 },
    /// The exact total behind a grid query, on the count button's request.
    CountFor { run: RunId, tab: TabId, sql: String },
}

#[derive(Debug)]
pub enum DbEvent {
    Connected { conn: ConnectionId, cancel: CancelHandle, server_version: String },
    ConnectFailed { conn: ConnectionId, error: DbError },
    QueryOk { run: RunId, result: QueryResult },
    BatchOk { run: RunId, results: Vec<QueryResult> },
    QueryErr { run: RunId, error: DbError },
    SchemaOk { conn: ConnectionId, schema: SchemaInfo },
    SchemaErr { conn: ConnectionId, error: DbError },
    /// One table's row count landed for the navigator's affordance.
    RowCountOk { conn: ConnectionId, schema: String, table: String, count: RowCount },
    RowCountErr { conn: ConnectionId, schema: String, table: String, error: DbError },
    /// The live backends landed for the session panel.
    SessionsOk { conn: ConnectionId, sessions: Vec<SessionInfo> },
    SessionsErr { conn: ConnectionId, error: DbError },
    /// One backend terminated; the panel re-reads the list on this landing.
    TerminateOk { conn: ConnectionId, backend: String },
    TerminateErr { conn: ConnectionId, backend: String, error: DbError },
    /// The autocommit toggle landed. `on` and `tx_open` are the worker's
    /// state after applying it; an error means a rollback on the way back to
    /// autocommit failed and the connection was closed instead of returned.
    AutocommitSet { conn: ConnectionId, on: bool, tx_open: bool, error: Option<DbError> },
    /// The manual transaction opened or ended, so the observed flag can
    /// track the worker's real state between explicit controls.
    TxState { conn: ConnectionId, open: bool },
    /// An open manual transaction sat idle for the warn span.
    TxIdleWarning { conn: ConnectionId },
    /// The worker rolled an idle manual transaction back.
    TxIdleRollback { conn: ConnectionId },
    /// One grid page landed. `tab` is the grid that asked; `offset` is the
    /// page's first row in the full order. The tab appends it onto the
    /// loaded rows instead of replacing the run.
    PageOk { run: RunId, tab: TabId, offset: u64, result: QueryResult },
    /// One grid page failed. The loaded rows stay as they were.
    PageErr { run: RunId, tab: TabId, error: DbError },
    /// The exact total behind a grid query landed beside the flag.
    CountOk { run: RunId, tab: TabId, total: u64 },
    /// The count failed. The flag stays without a total.
    CountErr { run: RunId, tab: TabId, error: DbError },
}

/// Owned by one worker thread for its whole life. Not Sync, and that is the
/// point: no locks, ordered execution per connection.
pub enum Session {
    Sqlite(rusqlite::Connection),
    #[cfg(feature = "postgres")]
    Postgres(PgSession),
    #[cfg(feature = "mysql")]
    Mysql(MySession),
}

impl Session {
    /// The engine this session talks to.
    fn kind(&self) -> DbKind {
        match self {
            Session::Sqlite(_) => DbKind::Sqlite,
            #[cfg(feature = "postgres")]
            Session::Postgres(_) => DbKind::Postgres,
            #[cfg(feature = "mysql")]
            Session::Mysql(_) => DbKind::Mysql,
        }
    }

    /// Opens the engine. `password` is the database secret and
    /// `ssh_password` the SSH secret (an SSH password or key passphrase);
    /// both are memory-only.
    pub fn open(
        config: &ConnectionConfig,
        password: Option<&str>,
        ssh_password: Option<&str>,
    ) -> Result<Session, DbError> {
        match config {
            ConnectionConfig::Sqlite { path, read_only } => {
                let conn = rusqlite::Connection::open(path)
                    .map_err(|e| DbError::Open(e.to_string()))?;
                if *read_only {
                    conn.execute_batch("PRAGMA query_only = ON")
                        .map_err(|e| DbError::Open(e.to_string()))?;
                }
                Ok(Session::Sqlite(conn))
            }
            ConnectionConfig::Postgres { .. } => {
                Self::open_postgres(config, password, ssh_password)
            }
            ConnectionConfig::Mysql { .. } => Self::open_mysql(config, password, ssh_password),
        }
    }

    fn open_postgres(
        config: &ConnectionConfig,
        password: Option<&str>,
        ssh_password: Option<&str>,
    ) -> Result<Session, DbError> {
        #[cfg(feature = "postgres")]
        {
            return Ok(Session::Postgres(PgSession::connect(
                config,
                password,
                ssh_password,
            )?));
        }
        #[cfg(not(feature = "postgres"))]
        {
            let _ = (config, password, ssh_password);
            return Err(DbError::NotSupported(
                "postgres needs the `postgres` feature".to_owned(),
            ));
        }
    }

    fn open_mysql(
        config: &ConnectionConfig,
        password: Option<&str>,
        ssh_password: Option<&str>,
    ) -> Result<Session, DbError> {
        #[cfg(feature = "mysql")]
        {
            return Ok(Session::Mysql(MySession::connect(
                config,
                password,
                ssh_password,
            )?));
        }
        #[cfg(not(feature = "mysql"))]
        {
            let _ = (config, password, ssh_password);
            return Err(DbError::Config("mysql feature not enabled".to_owned()));
        }
    }

    pub fn server_version(&self) -> String {
        match self {
            Session::Sqlite(_) => format!("SQLite {}", rusqlite::version()),
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => pg.server_version.clone(),
            #[cfg(feature = "mysql")]
            Session::Mysql(my) => my.server_version.clone(),
        }
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        match self {
            Session::Sqlite(conn) => {
                let handle = conn.get_interrupt_handle();
                CancelHandle::new(move || handle.interrupt())
            }
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => {
                pg_cancel_handle(pg.connection_string.clone(), pg.backend_pid)
            }
            #[cfg(feature = "mysql")]
            Session::Mysql(my) => {
                mysql_cancel_handle(my.connection_string.clone(), my.connection_id)
            }
        }
    }

    /// One statement per call. Result columns become capped rows; no
    /// columns means an affected count via `changes()`.
    pub fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        match self {
            Session::Sqlite(conn) => execute_sqlite(conn, sql, ROW_CAP),
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => pg.query(sql),
            #[cfg(feature = "mysql")]
            Session::Mysql(my) => my.query(sql),
        }
    }

    /// One page of `sql` at `offset` with at most `limit` rows. The page
    /// carries the flag when another page may remain: the row count hitting
    /// the limit exactly. A total that divides the limit evenly costs one
    /// extra empty page, which the stream loop treats as the end.
    pub fn query_page(
        &mut self,
        sql: &str,
        offset: u64,
        limit: u64,
    ) -> Result<QueryResult, DbError> {
        if limit == 0 {
            return Err(DbError::Query("page size is zero".to_owned()));
        }
        let mut page = self.query(&paged_sql(sql, self.kind(), limit, offset))?;
        page.truncated = page.row_count() == limit as usize;
        Ok(page)
    }

    /// The exact total behind `sql` for the truncation flag. Runs the count
    /// wrapper through the same query path as any SELECT.
    pub fn count_for(&mut self, sql: &str) -> Result<u64, DbError> {
        let result = self.query(&count_sql(sql, self.kind()))?;
        match result.cell(0, 0) {
            Some(Value::Integer(n)) if *n >= 0 => Ok(*n as u64),
            Some(Value::Text(text)) => text
                .trim()
                .parse::<u64>()
                .map_err(|_| DbError::Query("count did not return a row".to_owned())),
            _ => Err(DbError::Query("count did not return a row".to_owned())),
        }
    }

    /// One engine query for one table's row count, scoped to the schema the
    /// navigator names. SQLite counts the table exactly; Postgres reads
    /// `pg_class.reltuples` and MySQL `information_schema.tables.table_rows`,
    /// so those come back as estimates. Identifiers are quoted inline because
    /// this goes through the same text-only query path as the introspection.
    pub fn row_count(&mut self, schema: &str, table: &str) -> Result<RowCount, DbError> {
        match self {
            Session::Sqlite(conn) => {
                let sql = format!(
                    "SELECT COUNT(*) FROM {}.{}",
                    quote_ident(schema),
                    quote_ident(table)
                );
                let result = execute_sqlite(conn, &sql, ROW_CAP)?;
                count_cell(&result, schema, table).map(RowCount::Exact)
            }
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => {
                let sql = format!(
                    "SELECT GREATEST(c.reltuples, 0)::bigint FROM pg_class c \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = {} AND c.relname = {}",
                    sql_text(schema),
                    sql_text(table)
                );
                let result = pg.query(&sql)?;
                count_cell(&result, schema, table).map(RowCount::Estimate)
            }
            #[cfg(feature = "mysql")]
            Session::Mysql(my) => {
                let sql = format!(
                    "SELECT CAST(COALESCE(table_rows, 0) AS SIGNED) \
                     FROM information_schema.tables \
                     WHERE table_schema = {} AND table_name = {}",
                    sql_text(schema),
                    sql_text(table)
                );
                let result = my.query(&sql)?;
                count_cell(&result, schema, table).map(RowCount::Estimate)
            }
        }
    }

    /// Ordered schema load, capped with truncation flags. No client type
    /// crosses the boundary.
    pub fn schema(&mut self) -> Result<SchemaInfo, DbError> {
        match self {
            Session::Sqlite(conn) => sqlite_schema(conn),
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => pg.schema(),
            #[cfg(feature = "mysql")]
            Session::Mysql(my) => my.schema(),
        }
    }

    /// Live backends through read-only introspection, capped at
    /// [`SESSION_CAP`]. The list includes this session, marked self.
    /// SQLite has no sessions.
    pub fn sessions(&mut self) -> Result<Vec<SessionInfo>, DbError> {
        match self {
            Session::Sqlite(_) => Err(DbError::NotSupported(
                "session list needs the postgres or mysql feature".to_owned(),
            )),
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => pg.sessions(),
            #[cfg(feature = "mysql")]
            Session::Mysql(my) => my.sessions(),
        }
    }

    /// Terminate one backend by its [`SessionInfo::backend_id`]. Refuses the
    /// session's own backend, so the lister cannot kill its own connection.
    /// SQLite has no sessions.
    pub fn terminate_session(&mut self, backend: &str) -> Result<(), DbError> {
        let _ = backend;
        match self {
            Session::Sqlite(_) => Err(DbError::NotSupported(
                "session list needs the postgres or mysql feature".to_owned(),
            )),
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => pg.terminate_session(backend),
            #[cfg(feature = "mysql")]
            Session::Mysql(my) => my.terminate_session(backend),
        }
    }

    /// Switch the worker's autocommit. SQLite keeps it on: it has no pooled
    /// connections to pin, so the T16b buttons stay its only transaction
    /// controls.
    pub fn set_autocommit(&mut self, _on: bool) -> Result<(), DbError> {
        match self {
            Session::Sqlite(_) => Ok(()),
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => pg.set_autocommit(_on),
            #[cfg(feature = "mysql")]
            Session::Mysql(my) => my.set_autocommit(_on),
        }
    }

    /// The worker's autocommit after any toggle landed.
    pub fn autocommit(&self) -> bool {
        match self {
            Session::Sqlite(_) => true,
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => pg.autocommit(),
            #[cfg(feature = "mysql")]
            Session::Mysql(my) => my.autocommit(),
        }
    }

    /// Whether a manual transaction is open on the pinned connection.
    pub fn manual_tx_open(&self) -> bool {
        match self {
            Session::Sqlite(_) => false,
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => pg.mode.tx_open(),
            #[cfg(feature = "mysql")]
            Session::Mysql(my) => my.mode.tx_open(),
        }
    }

    /// Time until the open manual transaction's next idle deadline.
    pub fn tx_idle_wait(&self, _idle: &TxIdle) -> Option<Duration> {
        match self {
            Session::Sqlite(_) => None,
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => pg.mode.idle_wait(_idle),
            #[cfg(feature = "mysql")]
            Session::Mysql(my) => my.mode.idle_wait(_idle),
        }
    }

    /// One idle tick of the manual transaction, if any is open.
    pub fn tick_tx_idle(&mut self, _idle: &TxIdle) -> Option<IdleTick> {
        match self {
            Session::Sqlite(_) => None,
            #[cfg(feature = "postgres")]
            Session::Postgres(pg) => pg.tick_tx_idle(_idle),
            #[cfg(feature = "mysql")]
            Session::Mysql(my) => my.tick_tx_idle(_idle),
        }
    }
}

/// One identifier part for inline SQL: double-quoted with embedded quotes
/// doubled, the same shape the introspection paths write.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// One SQL string literal for inline SQL: single quotes doubled.
#[cfg(any(feature = "postgres", feature = "mysql"))]
fn sql_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The single non-negative integer a count query returns. No row means the
/// table vanished between the schema load and the request.
fn count_cell(result: &QueryResult, schema: &str, table: &str) -> Result<u64, DbError> {
    match result.cell(0, 0) {
        Some(Value::Integer(n)) if *n >= 0 => Ok(*n as u64),
        _ => Err(DbError::Query(format!("no row count for \"{schema}\".\"{table}\""))),
    }
}

/// Page `sql` in chunks without materializing the full result. Each page
/// reaches `on_page` with its offset before the next fetch; the loop ends on
/// a short page. `cancel` is checked before every fetch and after every
/// page, so a cancelled stream stops with a `cancelled` query error and the
/// rows already fed stay written. Returns the total rows fed.
pub fn stream_pages(
    session: &mut Session,
    sql: &str,
    page_size: usize,
    cancel: &AtomicBool,
    mut on_page: impl FnMut(&QueryResult, u64),
) -> Result<u64, DbError> {
    if page_size == 0 {
        return Err(DbError::Query("page size is zero".to_owned()));
    }
    let mut offset: u64 = 0;
    let mut rows: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(DbError::Query("cancelled".to_owned()));
        }
        let page = session.query_page(sql, offset, page_size as u64)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(DbError::Query("cancelled".to_owned()));
        }
        let count = page.row_count() as u64;
        if count == 0 {
            break;
        }
        on_page(&page, offset);
        rows += count;
        offset += count;
        if count < page_size as u64 {
            break;
        }
    }
    Ok(rows)
}

/// Autocommit state for one engine session, a sum type so "autocommit with
/// a pin" and "manual without a pin" cannot be written down.
///
/// Contract, worker side (the only owner of the session): with autocommit
/// on, every statement acquires a connection for its call. With autocommit
/// off, statements pin one connection and run inside a transaction on it.
/// The first statement implicitly opens that transaction, `COMMIT` or
/// `ROLLBACK` ends it, and the next statement implicitly opens a fresh one.
/// Cancel stops the statement and leaves the transaction open; only
/// autocommit switching back on, the idle guard, a failed rollback, or the
/// session ending closes it otherwise.
#[cfg(any(feature = "postgres", feature = "mysql"))]
enum AutoMode<C> {
    On,
    /// Autocommit off with no pin yet: the first statement takes one.
    Manual,
    Pinned {
        conn: C,
        open: bool,
        idle_since: Instant,
        warned: bool,
    },
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
impl<C> AutoMode<C> {
    fn tx_open(&self) -> bool {
        matches!(self, AutoMode::Pinned { open: true, .. })
    }

    /// A statement ran on the transaction: restart its idle span.
    fn note_activity(&mut self) {
        if let AutoMode::Pinned { idle_since, warned, .. } = self {
            *idle_since = Instant::now();
            *warned = false;
        }
    }

    fn tx_close(&mut self) {
        if let AutoMode::Pinned { open, .. } = self {
            *open = false;
        }
    }

    /// Time until the next idle deadline, only while a transaction is open.
    fn idle_wait(&self, idle: &TxIdle) -> Option<Duration> {
        let AutoMode::Pinned { open: true, idle_since, warned, .. } = self else {
            return None;
        };
        let deadline = if *warned { idle.rollback } else { idle.warn };
        Some(deadline.saturating_sub(idle_since.elapsed()).max(Duration::from_millis(1)))
    }

    /// One idle tick. `Rollback` leaves the engine call to the caller, which
    /// closes the transaction through [`AutoMode::tx_close`] either way.
    fn idle_tick(&mut self, idle: &TxIdle) -> Option<IdleTick> {
        let AutoMode::Pinned { open: true, idle_since, warned, .. } = self else {
            return None;
        };
        let been_idle = idle_since.elapsed();
        if been_idle >= idle.rollback {
            return Some(IdleTick::Rollback);
        }
        if !*warned && been_idle >= idle.warn {
            *warned = true;
            return Some(IdleTick::Warn);
        }
        None
    }
}

/// A pool lease for one synchronous call. Returns its connection inside the
/// runtime on drop, because sqlx spawns the return-to-pool task on drop and
/// that needs a Tokio context; an early `?` error path must not unwind past
/// it on a bare thread.
#[cfg(any(feature = "postgres", feature = "mysql"))]
struct PooledLease<'a, C> {
    rt: &'a tokio::runtime::Runtime,
    conn: Option<C>,
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
impl<'a, C> PooledLease<'a, C> {
    fn new(rt: &'a tokio::runtime::Runtime, conn: C) -> Self {
        Self { rt, conn: Some(conn) }
    }
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
impl<C> Drop for PooledLease<'_, C> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.rt.block_on(async { drop(conn) });
        }
    }
}

/// The connection a schema load runs on: the manual pin when one is held,
/// else a pool lease for the load.
#[cfg(any(feature = "postgres", feature = "mysql"))]
enum SchemaConn<'a, C> {
    Pinned(&'a mut C),
    Leased(PooledLease<'a, C>),
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
impl<C> SchemaConn<'_, C> {
    fn conn(&mut self) -> &mut C {
        match self {
            SchemaConn::Pinned(conn) => conn,
            SchemaConn::Leased(lease) => lease.conn.as_mut().expect("leased"),
        }
    }
}

/// Behind the `postgres` feature. Owns a current-thread tokio runtime so the
/// async sqlx model stays caged inside this driver file; the public trait
/// shape never changes. The worker thread calls `block_on`, so the UI thread
/// never blocks. A configured SSH tunnel lives here for the whole session
/// and drops with it, so disconnect tears it down.
#[cfg(feature = "postgres")]
pub struct PgSession {
    rt: tokio::runtime::Runtime,
    pool: sqlx::PgPool,
    /// Autocommit state; see [`AutoMode`] for the manual-mode contract.
    mode: AutoMode<sqlx::pool::PoolConnection<sqlx::Postgres>>,
    connection_string: String,
    database: String,
    server_version: String,
    backend_pid: i32,
    /// Held for the session's whole life, so disconnect tears the tunnel
    /// down with the pool; declared last so it drops after the pool.
    _tunnel: Option<crate::ssh::Tunnel>,
}

#[cfg(feature = "postgres")]
impl PgSession {
    fn connect(
        config: &ConnectionConfig,
        password: Option<&str>,
        ssh_password: Option<&str>,
    ) -> Result<Self, DbError> {
        let ConnectionConfig::Postgres {
            host,
            port,
            database,
            user,
            ssl_mode,
            ssh,
            read_only,
        } = config
        else {
            return Err(DbError::Config("not a postgres connection".to_owned()));
        };
        let (tunnel, dial_host, dial_port) = crate::ssh::forward(
            ssh.as_ref(),
            ssh_password.unwrap_or_default(),
            host,
            *port,
        )?;
        let connection_string =
            postgres_url(&dial_host, dial_port, database, user, password, *ssl_mode);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| DbError::Open(e.to_string()))?;
        let read_only = *read_only;
        // `acquire_timeout` also bounds establishing the first connection;
        // sqlx 0.8 has no separate `connect_timeout` on the pool options.
        let pool = rt
            .block_on(
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(Duration::from_secs(10))
                    .after_connect(move |conn, _| {
                        Box::pin(async move {
                            if read_only {
                                sqlx::query("SET default_transaction_read_only = on").execute(conn).await?;
                            }
                            Ok(())
                        })
                    })
                    .connect(&connection_string),
            )
            .map_err(|e| DbError::Open(e.to_string()))?;
        let server_version = rt
            .block_on(async {
                let row = sqlx::query("SELECT version()")
                    .fetch_one(&pool)
                    .await
                    .map_err(|e| e.to_string())?;
                use sqlx::Row as _;
                row.try_get::<String, _>(0).map_err(|e| e.to_string())
            })
            .unwrap_or_else(|_| "Postgres".to_owned());
        let backend_pid = rt
            .block_on(async {
                let row = sqlx::query("SELECT pg_backend_pid()")
                    .fetch_one(&pool)
                    .await
                    .map_err(|e| e.to_string())?;
                use sqlx::Row as _;
                row.try_get::<i32, _>(0).map_err(|e| e.to_string())
            })
            .unwrap_or(-1);
        Ok(Self {
            rt,
            pool,
            mode: AutoMode::On,
            connection_string,
            database: database.clone(),
            server_version,
            backend_pid,
            _tunnel: tunnel,
        })
    }

    fn autocommit(&self) -> bool {
        matches!(self.mode, AutoMode::On)
    }

    /// One statement per call. Autocommit on acquires a connection for the
    /// call; autocommit off runs the statement on the pinned connection.
    fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        match self.mode {
            AutoMode::On => {
                let rt = &self.rt;
                let pool = &self.pool;
                rt.block_on(async {
                    let mut conn = pool
                        .acquire()
                        .await
                        .map_err(|e| DbError::Query(e.to_string()))?;
                    pg_statement(&mut conn, sql).await
                })
            }
            AutoMode::Manual | AutoMode::Pinned { .. } => self.query_manual(sql),
        }
    }

    fn query_manual(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let Self { rt, pool, mode, .. } = self;
        rt.block_on(async {
            if let AutoMode::Manual = mode {
                let conn = pool
                    .acquire()
                    .await
                    .map_err(|e| DbError::Query(e.to_string()))?;
                *mode = AutoMode::Pinned {
                    conn,
                    open: false,
                    idle_since: Instant::now(),
                    warned: false,
                };
            }
            let AutoMode::Pinned { conn, open, .. } = mode else {
                return Err(DbError::Query("autocommit is on".to_owned()));
            };
            let control = tx_control(sql);
            let plan = manual_plan(*open, control);
            let started = Instant::now();
            let outcome = match plan {
                ManualPlan::Skip => QueryResult::from_columns(
                    Vec::new(),
                    false,
                    Some(0),
                    started.elapsed(),
                )
                .map_err(|e| DbError::Query(e.to_string())),
                ManualPlan::Only if control == Some(TxControl::Begin) => {
                    match pg_begin(conn).await {
                        Ok(()) => {
                            *open = true;
                            QueryResult::from_columns(
                                Vec::new(),
                                false,
                                Some(0),
                                started.elapsed(),
                            )
                            .map_err(|e| DbError::Query(e.to_string()))
                        }
                        Err(error) => Err(error),
                    }
                }
                ManualPlan::Only => {
                    let result = pg_statement(conn, sql).await;
                    if result.is_ok()
                        && matches!(control, Some(TxControl::Commit | TxControl::Rollback))
                    {
                        *open = false;
                    }
                    result
                }
                ManualPlan::BeginThenRun => match pg_begin(conn).await {
                    Ok(()) => {
                        *open = true;
                        pg_statement(conn, sql).await
                    }
                    Err(error) => Err(error),
                },
            };
            if plan != ManualPlan::Skip {
                mode.note_activity();
            }
            outcome
        })
    }

    /// Switch autocommit. Leaving manual mode rolls an open transaction back
    /// and returns the pin to the pool; a failed rollback closes the
    /// connection instead, so the pool never hands out a dirty session.
    fn set_autocommit(&mut self, on: bool) -> Result<(), DbError> {
        if !on {
            if matches!(self.mode, AutoMode::On) {
                self.mode = AutoMode::Manual;
            }
            return Ok(());
        }
        match std::mem::replace(&mut self.mode, AutoMode::On) {
            AutoMode::On | AutoMode::Manual => Ok(()),
            AutoMode::Pinned { mut conn, open, .. } => {
                let rt = &self.rt;
                rt.block_on(async {
                    let ended = if open {
                        sqlx::raw_sql("ROLLBACK")
                            .execute(&mut *conn)
                            .await
                            .map_err(|e| DbError::Query(e.to_string()))
                            .map(|_| ())
                    } else {
                        Ok(())
                    };
                    match ended {
                        Ok(()) => drop(conn),
                        Err(error) => {
                            conn.close_on_drop();
                            drop(conn);
                            return Err(error);
                        }
                    }
                    Ok(())
                })
            }
        }
    }

    /// An idle tick: the rollback runs on the pin, which stays held; a
    /// failed rollback closes the connection and drops the pin.
    fn tick_tx_idle(&mut self, idle: &TxIdle) -> Option<IdleTick> {
        let tick = self.mode.idle_tick(idle)?;
        if tick == IdleTick::Rollback {
            let rt = &self.rt;
            let mode = &mut self.mode;
            let failed = match mode {
                AutoMode::Pinned { conn, open, .. } => {
                    *open && rt.block_on(sqlx::raw_sql("ROLLBACK").execute(&mut **conn)).is_err()
                }
                _ => false,
            };
            mode.tx_close();
            if failed {
                if let AutoMode::Pinned { mut conn, .. } =
                    std::mem::replace(mode, AutoMode::Manual)
                {
                    conn.close_on_drop();
                    let _ = rt.block_on(async { drop(conn) });
                }
            }
        }
        Some(tick)
    }

    fn schema(&mut self) -> Result<SchemaInfo, DbError> {
        use sqlx::Row as _;
        // Manual mode holds the pool's only connection, so the load runs on
        // the pin instead of timing out on an acquire.
        let mut held = match &mut self.mode {
            AutoMode::Pinned { conn, .. } => SchemaConn::Pinned(conn),
            _ => {
                let conn = self
                    .rt
                    .block_on(self.pool.acquire())
                    .map_err(|e| DbError::Query(e.to_string()))?;
                SchemaConn::Leased(PooledLease::new(&self.rt, conn))
            }
        };
        let conn: &mut sqlx::PgConnection = held.conn();
        let schema_names: Vec<String> = self
            .rt
            .block_on(
                sqlx::query(
                    "SELECT nspname FROM pg_namespace \
                     WHERE nspname NOT LIKE 'pg_%' \
                     AND nspname <> 'information_schema' \
                     ORDER BY nspname",
                )
                .fetch_all(&mut *conn),
            )
            .map_err(|e| DbError::Query(e.to_string()))?
            .iter()
            .map(|row| {
                row.try_get::<String, _>(0).map_err(|e| DbError::Query(e.to_string()))
            })
            .collect::<Result<Vec<_>, DbError>>()?;
        let schema_names: Vec<String> =
            schema_names.into_iter().take(SCHEMA_SCHEMA_CAP).collect();
        let mut tables_truncated = false;
        let mut columns_truncated = false;
        struct RawSchema {
            name: String,
            order: Vec<(String, TableKind)>,
            columns_by_table: std::collections::HashMap<String, Vec<(String, String, bool)>>,
            pk_by_table: std::collections::HashMap<String, Vec<String>>,
            constraints_by_table: std::collections::HashMap<String, Vec<ConstraintInfo>>,
            fk_by_table: std::collections::HashMap<String, Vec<ForeignKeyInfo>>,
            fk_truncated_by_table: std::collections::HashMap<String, bool>,
            idx_by_table: std::collections::HashMap<String, Vec<IndexInfo>>,
            idx_truncated_by_table: std::collections::HashMap<String, bool>,
            sequences: Vec<SequenceInfo>,
            schema_indexes: Vec<IndexRef>,
            triggers: Vec<TriggerInfo>,
            routines: Vec<RoutineInfo>,
            triggers_truncated: bool,
            routines_truncated: bool,
        }
        struct FkEdge {
            child_table: String,
            parent_schema: String,
            parent_table: String,
            child_cols: Vec<String>,
            parent_cols: Vec<String>,
            name: String,
        }
        let mut raw_schemas: Vec<RawSchema> = Vec::new();
        let mut all_edges: Vec<FkEdge> = Vec::new();
        for schema_name in &schema_names {
            let tables: Vec<(String, String)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT table_name, table_type FROM information_schema.tables \
                         WHERE table_schema = $1 \
                         AND table_type IN ('BASE TABLE', 'VIEW') \
                         ORDER BY table_name",
                    )
                    .bind(schema_name)
                    .fetch_all(&mut *conn),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    Ok((
                        row.try_get::<String, _>(0)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(1)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            if tables.len() > SCHEMA_TABLE_CAP {
                tables_truncated = true;
            }
            let order: Vec<(String, TableKind)> = tables
                .into_iter()
                .take(SCHEMA_TABLE_CAP)
                .map(|(name, kind)| {
                    let kind =
                        if kind == "VIEW" { TableKind::View } else { TableKind::Table };
                    (name, kind)
                })
                .collect();
            let columns: Vec<(String, String, String, String)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT table_name, column_name, data_type, is_nullable \
                         FROM information_schema.columns \
                         WHERE table_schema = $1 \
                         ORDER BY table_name, ordinal_position",
                    )
                    .bind(schema_name)
                    .fetch_all(&mut *conn),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    Ok((
                        row.try_get::<String, _>(0)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(1)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(2)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(3)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let mut columns_by_table: std::collections::HashMap<
                String,
                Vec<(String, String, bool)>,
            > = std::collections::HashMap::new();
            for (table, column, data_type, is_nullable) in columns {
                columns_by_table.entry(table).or_default().push((
                    column,
                    data_type,
                    is_nullable == "YES",
                ));
            }
            let con_rows: Vec<(
                String,
                String,
                String,
                Option<Vec<String>>,
                Option<String>,
                Option<String>,
                Option<Vec<String>>,
                String,
            )> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT c.conname, c.contype::text, t.relname, \
                         (SELECT array_agg(a.attname ORDER BY u.ord) \
                          FROM unnest(c.conkey) WITH ORDINALITY AS u(attnum, ord) \
                          JOIN pg_attribute a \
                          ON a.attrelid = c.conrelid AND a.attnum = u.attnum), \
                         ft.relname, fns.nspname, \
                         (SELECT array_agg(a.attname ORDER BY u.ord) \
                          FROM unnest(c.confkey) WITH ORDINALITY AS u(attnum, ord) \
                          JOIN pg_attribute a \
                          ON a.attrelid = c.confrelid AND a.attnum = u.attnum), \
                         pg_get_constraintdef(c.oid) \
                         FROM pg_constraint c \
                         JOIN pg_class t ON t.oid = c.conrelid \
                         JOIN pg_namespace n ON n.oid = t.relnamespace \
                         LEFT JOIN pg_class ft ON ft.oid = c.confrelid \
                         LEFT JOIN pg_namespace fns ON fns.oid = ft.relnamespace \
                         WHERE n.nspname = $1 AND c.contype IN ('p', 'u', 'c', 'f') \
                         ORDER BY t.relname, c.conname",
                    )
                    .bind(schema_name)
                    .fetch_all(&mut *conn),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    Ok((
                        row.try_get::<String, _>(0)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(1)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(2)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<Option<Vec<String>>, _>(3)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<Option<String>, _>(4)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<Option<String>, _>(5)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<Option<Vec<String>>, _>(6)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(7)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let mut pk_by_table: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            let mut constraints_by_table: std::collections::HashMap<
                String,
                Vec<ConstraintInfo>,
            > = std::collections::HashMap::new();
            let mut fk_grouped: std::collections::HashMap<
                String,
                Vec<(String, Vec<String>, String, Vec<String>)>,
            > = std::collections::HashMap::new();
            for (conname, contype, table, cols, ref_table, ref_schema, ref_cols, def) in
                &con_rows
            {
                let kind = match contype.as_str() {
                    "p" => ConstraintKind::Pk,
                    "u" => ConstraintKind::Unique,
                    "c" => ConstraintKind::Check,
                    "f" => ConstraintKind::Foreign,
                    _ => continue,
                };
                let cols = cols.clone().unwrap_or_default();
                if kind == ConstraintKind::Pk && !pk_by_table.contains_key(table) {
                    pk_by_table.insert(table.clone(), cols.clone());
                }
                let check_expr =
                    if kind == ConstraintKind::Check { Some(def.clone()) } else { None };
                let info = ConstraintInfo::new(
                    conname.clone(),
                    kind,
                    cols.clone(),
                    ref_table.clone(),
                    ref_cols.clone().unwrap_or_default(),
                    check_expr,
                )
                .map_err(|e| DbError::Query(e.to_string()))?;
                constraints_by_table.entry(table.clone()).or_default().push(info);
                if kind == ConstraintKind::Foreign {
                    if let (Some(parent), Some(parent_schema)) = (ref_table, ref_schema) {
                        let child_cols = cols.clone();
                        let parent_cols = ref_cols.clone().unwrap_or_default();
                        fk_grouped.entry(table.clone()).or_default().push((
                            conname.clone(),
                            child_cols.clone(),
                            parent.clone(),
                            parent_cols.clone(),
                        ));
                        all_edges.push(FkEdge {
                            child_table: table.clone(),
                            parent_schema: parent_schema.clone(),
                            parent_table: parent.clone(),
                            child_cols,
                            parent_cols,
                            name: conname.clone(),
                        });
                    }
                }
            }
            let mut fk_by_table: std::collections::HashMap<String, Vec<ForeignKeyInfo>> =
                std::collections::HashMap::new();
            let mut fk_truncated_by_table: std::collections::HashMap<String, bool> =
                std::collections::HashMap::new();
            for (table, groups) in fk_grouped {
                if groups.len() > SCHEMA_FK_CAP {
                    fk_truncated_by_table.insert(table.clone(), true);
                }
                let mut fks = Vec::new();
                for (conname, child_cols, parent, parent_cols) in
                    groups.into_iter().take(SCHEMA_FK_CAP)
                {
                    fks.push(
                        ForeignKeyInfo::new(
                            child_cols,
                            parent,
                            parent_cols,
                            Some(conname),
                        )
                        .map_err(|e| DbError::Query(e.to_string()))?,
                    );
                }
                fk_by_table.insert(table, fks);
            }
            let index_rows: Vec<(String, String, bool, Option<String>)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT t.relname, c.relname, ix.indisunique, a.attname \
                         FROM pg_class t \
                         JOIN pg_index ix ON t.oid = ix.indrelid \
                         JOIN pg_class c ON c.oid = ix.indexrelid \
                         LEFT JOIN pg_attribute a \
                         ON a.attrelid = t.oid AND a.attnum = ANY(ix.indkey) \
                         WHERE t.relkind IN ('r', 'm') \
                         AND t.relnamespace = $1::regnamespace \
                         AND ix.indisvalid \
                         ORDER BY t.relname, c.relname, \
                           array_position(ix.indkey, a.attnum)",
                    )
                    .bind(schema_name)
                    .fetch_all(&mut *conn),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    Ok((
                        row.try_get::<String, _>(0)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(1)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<bool, _>(2)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<Option<String>, _>(3)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let mut idx_grouped: std::collections::HashMap<
                String,
                Vec<(String, bool, Vec<String>)>,
            > = std::collections::HashMap::new();
            {
                let mut current: Option<(String, String, bool, Vec<String>)> = None;
                let flush = |current: &mut Option<(String, String, bool, Vec<String>)>,
                             map: &mut std::collections::HashMap<
                    String,
                    Vec<(String, bool, Vec<String>)>,
                >| {
                    if let Some((table, name, unique, cols)) = current.take() {
                        map.entry(table).or_default().push((name, unique, cols));
                    }
                };
                for (table, index, unique, column) in index_rows {
                    match current.as_mut() {
                        Some(cur) if cur.0 == table && cur.1 == index => {
                            if let Some(col) = column {
                                if !col.trim().is_empty() {
                                    cur.3.push(col);
                                }
                            }
                        }
                        _ => {
                            flush(&mut current, &mut idx_grouped);
                            let cols = match column {
                                Some(col) if !col.trim().is_empty() => vec![col],
                                _ => Vec::new(),
                            };
                            current = Some((table, index, unique, cols));
                        }
                    }
                }
                flush(&mut current, &mut idx_grouped);
            }
            let mut idx_by_table: std::collections::HashMap<String, Vec<IndexInfo>> =
                std::collections::HashMap::new();
            let mut idx_truncated_by_table: std::collections::HashMap<String, bool> =
                std::collections::HashMap::new();
            let mut schema_indexes: Vec<IndexRef> = Vec::new();
            for (table, groups) in idx_grouped {
                if groups.len() > SCHEMA_INDEX_CAP {
                    idx_truncated_by_table.insert(table.clone(), true);
                }
                let mut idxs = Vec::new();
                for (index_name, unique, idx_cols) in
                    groups.into_iter().take(SCHEMA_INDEX_CAP)
                {
                    schema_indexes.push(
                        IndexRef::new(table.clone(), index_name.clone())
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    );
                    idxs.push(
                        IndexInfo::new(index_name, idx_cols, unique)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    );
                }
                idx_by_table.insert(table, idxs);
            }
            schema_indexes.sort_by(|a, b| (a.table(), a.index()).cmp(&(b.table(), b.index())));
            schema_indexes.truncate(SCHEMA_SCHEMA_INDEX_CAP);
            let seq_names: Vec<String> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT sequencename FROM pg_sequences \
                         WHERE schemaname = $1 \
                         ORDER BY sequencename",
                    )
                    .bind(schema_name)
                    .fetch_all(&mut *conn),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    row.try_get::<String, _>(0).map_err(|e| DbError::Query(e.to_string()))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let mut sequences = Vec::new();
            for name in seq_names.into_iter().take(SCHEMA_SEQUENCE_CAP) {
                sequences.push(
                    SequenceInfo::new(name).map_err(|e| DbError::Query(e.to_string()))?,
                );
            }
            let trigger_rows: Vec<(String, String, i16, String)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT t.tgname, c.relname, t.tgtype, pg_get_triggerdef(t.oid) \
                         FROM pg_trigger t \
                         JOIN pg_class c ON c.oid = t.tgrelid \
                         JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE NOT t.tgisinternal AND n.nspname = $1 \
                         ORDER BY c.relname, t.tgname",
                    )
                    .bind(schema_name)
                    .fetch_all(&mut *conn),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    Ok((
                        row.try_get::<String, _>(0)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(1)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<i16, _>(2)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(3)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let triggers_truncated = trigger_rows.len() > SCHEMA_TRIGGER_CAP;
            let mut triggers = Vec::new();
            for (name, table, tgtype, definition) in
                trigger_rows.into_iter().take(SCHEMA_TRIGGER_CAP)
            {
                triggers.push(
                    TriggerInfo::new(
                        name,
                        Some(table),
                        Some(pg_trigger_kind(tgtype)),
                        Some(definition).filter(|d| !d.trim().is_empty()),
                    )
                    .map_err(|e| DbError::Query(e.to_string()))?,
                );
            }
            let routine_rows: Vec<(String, String, String, String)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT p.proname, p.prokind::text, l.lanname, p.prosrc \
                         FROM pg_proc p \
                         JOIN pg_namespace n ON n.oid = p.pronamespace \
                         JOIN pg_language l ON l.oid = p.prolang \
                         WHERE n.nspname = $1 AND p.prokind IN ('f', 'p') \
                         ORDER BY p.proname",
                    )
                    .bind(schema_name)
                    .fetch_all(&mut *conn),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    Ok((
                        row.try_get::<String, _>(0)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(1)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(2)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                        row.try_get::<String, _>(3)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let routines_truncated = routine_rows.len() > SCHEMA_ROUTINE_CAP;
            let mut routines = Vec::new();
            for (name, prokind, language, definition) in
                routine_rows.into_iter().take(SCHEMA_ROUTINE_CAP)
            {
                let kind = if prokind == "p" { "PROCEDURE" } else { "FUNCTION" };
                routines.push(
                    RoutineInfo::new(
                        name,
                        Some(kind.to_owned()),
                        Some(language),
                        Some(definition).filter(|d| !d.trim().is_empty()),
                    )
                    .map_err(|e| DbError::Query(e.to_string()))?,
                );
            }
            raw_schemas.push(RawSchema {
                name: schema_name.clone(),
                order,
                columns_by_table,
                pk_by_table,
                constraints_by_table,
                fk_by_table,
                fk_truncated_by_table,
                idx_by_table,
                idx_truncated_by_table,
                sequences,
                schema_indexes,
                triggers,
                routines,
                triggers_truncated,
                routines_truncated,
            });
        }
        let mut incoming: std::collections::HashMap<(String, String), Vec<ForeignKeyInfo>> =
            std::collections::HashMap::new();
        for edge in all_edges {
            let info = ForeignKeyInfo::new(
                edge.parent_cols,
                edge.child_table.clone(),
                edge.child_cols,
                Some(edge.name),
            )
            .map_err(|e| DbError::Query(e.to_string()))?;
            incoming
                .entry((edge.parent_schema, edge.parent_table))
                .or_default()
                .push(info);
        }
        let mut schema_nodes = Vec::new();
        for raw in &raw_schemas {
            let mut tables = Vec::new();
            let mut views = Vec::new();
            for (name, kind) in &raw.order {
                let col_rows = raw.columns_by_table.get(name).cloned().unwrap_or_default();
                if col_rows.len() > SCHEMA_COLUMN_CAP {
                    columns_truncated = true;
                }
                let pk_cols = raw.pk_by_table.get(name).cloned().unwrap_or_default();
                let mut pk_pos: std::collections::HashMap<String, u32> =
                    std::collections::HashMap::new();
                for (i, col) in pk_cols.iter().enumerate() {
                    pk_pos.insert(col.clone(), (i + 1) as u32);
                }
                let mut cols = Vec::new();
                for (col_name, type_name, nullable) in
                    col_rows.into_iter().take(SCHEMA_COLUMN_CAP)
                {
                    let pk_position = pk_pos.get(&col_name).copied();
                    let nullable = if pk_position.is_some() { false } else { nullable };
                    cols.push(
                        ColumnInfo::new(col_name, type_name, nullable, pk_position)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    );
                }
                let constraints =
                    raw.constraints_by_table.get(name).cloned().unwrap_or_default();
                let foreign_keys = raw.fk_by_table.get(name).cloned().unwrap_or_default();
                let referenced_by = incoming
                    .get(&(raw.name.clone(), name.clone()))
                    .cloned()
                    .unwrap_or_default();
                let indexes = raw.idx_by_table.get(name).cloned().unwrap_or_default();
                let fk_truncated =
                    raw.fk_truncated_by_table.get(name).copied().unwrap_or(false);
                let idx_truncated =
                    raw.idx_truncated_by_table.get(name).copied().unwrap_or(false);
                let table = TableInfo::new(
                    name.clone(),
                    *kind,
                    cols,
                    constraints,
                    foreign_keys,
                    referenced_by,
                    indexes,
                    fk_truncated,
                    idx_truncated,
                )
                .map_err(|e| DbError::Query(e.to_string()))?;
                match kind {
                    TableKind::Table => tables.push(table),
                    TableKind::View => views.push(table),
                }
            }
            schema_nodes.push(
                SchemaNode::new(
                    raw.name.clone(),
                    tables,
                    views,
                    raw.sequences.clone(),
                    raw.schema_indexes.clone(),
                    raw.triggers.clone(),
                    raw.routines.clone(),
                    raw.triggers_truncated,
                    raw.routines_truncated,
                )
                .map_err(|e| DbError::Query(e.to_string()))?,
            );
        }
        let db_node = DatabaseNode::new(self.database.clone(), schema_nodes)
            .map_err(|e| DbError::Query(e.to_string()))?;
        Ok(SchemaInfo::new(vec![db_node], tables_truncated, columns_truncated))
    }

    /// Live backends from `pg_stat_activity` plus ungranted `pg_locks`, a
    /// read-only SELECT pair. Runs on the manual pin when one is held, else
    /// on a pool lease. The row carrying this session's pid is marked self.
    fn sessions(&mut self) -> Result<Vec<SessionInfo>, DbError> {
        use sqlx::Row as _;
        // Manual mode holds the pool's only connection, so the list runs on
        // the pin instead of timing out on an acquire.
        let mut held = match &mut self.mode {
            AutoMode::Pinned { conn, .. } => SchemaConn::Pinned(conn),
            _ => {
                let conn = self
                    .rt
                    .block_on(self.pool.acquire())
                    .map_err(|e| DbError::Query(e.to_string()))?;
                SchemaConn::Leased(PooledLease::new(&self.rt, conn))
            }
        };
        let conn: &mut sqlx::PgConnection = held.conn();
        let rows = self
            .rt
            .block_on(sqlx::query(pg_sessions_sql()).fetch_all(&mut *conn))
            .map_err(|e| DbError::Query(e.to_string()))?;
        let own = self.backend_pid;
        let mut out = Vec::with_capacity(rows.len().min(SESSION_CAP));
        for row in rows.iter().take(SESSION_CAP) {
            let pid: i32 =
                row.try_get(0).map_err(|e| DbError::Query(e.to_string()))?;
            let user: Option<String> =
                row.try_get(1).map_err(|e| DbError::Query(e.to_string()))?;
            let database: Option<String> =
                row.try_get(2).map_err(|e| DbError::Query(e.to_string()))?;
            let state: Option<String> =
                row.try_get(3).map_err(|e| DbError::Query(e.to_string()))?;
            let query: Option<String> =
                row.try_get(4).map_err(|e| DbError::Query(e.to_string()))?;
            let waiting: bool =
                row.try_get(5).map_err(|e| DbError::Query(e.to_string()))?;
            let locks: Option<String> =
                row.try_get(6).map_err(|e| DbError::Query(e.to_string()))?;
            out.push(pg_session_from_parts(
                pid, user, database, state, query, waiting, locks, pid == own,
            )?);
        }
        Ok(out)
    }

    /// Terminate one backend by its panel id. Refuses this session's own pid,
    /// so the lister cannot kill its own connection. The id is parsed to an
    /// integer before it reaches SQL, so the statement cannot inject.
    fn terminate_session(&mut self, backend: &str) -> Result<(), DbError> {
        use sqlx::Row as _;
        let pid = parse_pg_backend(backend)?;
        if pid == self.backend_pid {
            return Err(DbError::Query("cannot terminate this session".to_owned()));
        }
        let mut held = match &mut self.mode {
            AutoMode::Pinned { conn, .. } => SchemaConn::Pinned(conn),
            _ => {
                let conn = self
                    .rt
                    .block_on(self.pool.acquire())
                    .map_err(|e| DbError::Query(e.to_string()))?;
                SchemaConn::Leased(PooledLease::new(&self.rt, conn))
            }
        };
        let conn: &mut sqlx::PgConnection = held.conn();
        let sql = pg_terminate_sql(pid);
        let row = self
            .rt
            .block_on(sqlx::query(&sql).fetch_one(&mut *conn))
            .map_err(|e| DbError::Query(e.to_string()))?;
        let ended: bool =
            row.try_get(0).map_err(|e| DbError::Query(e.to_string()))?;
        if ended {
            Ok(())
        } else {
            Err(DbError::Query(format!("backend {backend} is gone")))
        }
    }
}

#[cfg(feature = "postgres")]
async fn pg_begin(conn: &mut sqlx::PgConnection) -> Result<(), DbError> {
    sqlx::raw_sql("BEGIN")
        .execute(&mut *conn)
        .await
        .map(|_| ())
        .map_err(|e| DbError::Query(e.to_string()))
}

/// One statement on one connection. `describe` reports columns without
/// executing, so a data-changing statement runs exactly once on the
/// `execute` path; transaction controls run on the text protocol.
#[cfg(feature = "postgres")]
async fn pg_statement(conn: &mut sqlx::PgConnection, sql: &str) -> Result<QueryResult, DbError> {
    use sqlx::{Column as _, Executor as _, TypeInfo as _};
    let started = Instant::now();
    if tx_control(sql).is_some() {
        let done = sqlx::raw_sql(sql)
            .execute(&mut *conn)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        return QueryResult::from_columns(
            Vec::new(),
            false,
            Some(done.rows_affected()),
            started.elapsed(),
        )
        .map_err(|e| DbError::Query(e.to_string()));
    }
    let columns: Vec<(String, String)> = (&mut *conn)
        .describe(sql)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?
        .columns()
        .iter()
        .map(|c| (c.name().to_owned(), c.type_info().name().to_owned()))
        .collect();
    if columns.is_empty() {
        let done = sqlx::query(sql)
            .execute(&mut *conn)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        return QueryResult::from_columns(
            Vec::new(),
            false,
            Some(done.rows_affected()),
            started.elapsed(),
        )
        .map_err(|e| DbError::Query(e.to_string()));
    }
    let rows = sqlx::query(sql)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
    let mut builder = ResultBuilder::new(columns, ROW_CAP);
    for row in &rows {
        if !builder.push_row(map_pg_row(row)?) {
            break;
        }
    }
    Ok(builder.finish(started.elapsed()))
}

#[cfg(feature = "postgres")]
impl Drop for PgSession {
    /// The pin must go back inside the runtime: sqlx spawns the
    /// return-to-pool task on drop, and that needs a Tokio context.
    fn drop(&mut self) {
        if let AutoMode::Pinned { mut conn, open, .. } =
            std::mem::replace(&mut self.mode, AutoMode::On)
        {
            let rt = &self.rt;
            rt.block_on(async {
                if open {
                    let _ = sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await;
                }
                drop(conn);
            });
        }
    }
}

/// Renders a `pg_trigger.tgtype` bitmask as the timing plus event words the
/// definition prints, for example `BEFORE INSERT`. Bit assignments come from
/// PostgreSQL's `TRIGGER_TYPE_*` constants.
#[cfg(feature = "postgres")]
fn pg_trigger_kind(tgtype: i16) -> String {
    let mut parts = Vec::new();
    if tgtype & 2 != 0 {
        parts.push("BEFORE");
    } else if tgtype & 64 != 0 {
        parts.push("INSTEAD OF");
    } else {
        parts.push("AFTER");
    }
    if tgtype & 4 != 0 {
        parts.push("INSERT");
    }
    if tgtype & 16 != 0 {
        parts.push("UPDATE");
    }
    if tgtype & 8 != 0 {
        parts.push("DELETE");
    }
    if tgtype & 32 != 0 {
        parts.push("TRUNCATE");
    }
    parts.join(" ")
}

/// Behind the `mysql` feature. Owns a current-thread tokio runtime so the
/// async sqlx model stays caged inside this driver file. sqlx sends every
/// query through the prepared/binary protocol, even one with no binds, so
/// values arrive in their binary encodings; the cell decoder turns the
/// temporal ones back into the text the server itself would print.
#[cfg(feature = "mysql")]
pub struct MySession {
    rt: tokio::runtime::Runtime,
    pool: sqlx::MySqlPool,
    /// Autocommit state; see [`AutoMode`] for the manual-mode contract.
    mode: AutoMode<sqlx::pool::PoolConnection<sqlx::MySql>>,
    connection_string: String,
    database: String,
    server_version: String,
    connection_id: u64,
    /// Held for the session's whole life, so disconnect tears the tunnel
    /// down with the pool; declared last so it drops after the pool.
    _tunnel: Option<crate::ssh::Tunnel>,
}

#[cfg(feature = "mysql")]
impl MySession {
    fn connect(
        config: &ConnectionConfig,
        password: Option<&str>,
        ssh_password: Option<&str>,
    ) -> Result<Self, DbError> {
        let ConnectionConfig::Mysql { host, port, database, user, ssl_mode, ssh, read_only } =
            config
        else {
            return Err(DbError::Config("not a mysql connection".to_owned()));
        };
        let (tunnel, dial_host, dial_port) = crate::ssh::forward(
            ssh.as_ref(),
            ssh_password.unwrap_or_default(),
            host,
            *port,
        )?;
        let connection_string =
            mysql_url(&dial_host, dial_port, database, user, password, *ssl_mode);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| DbError::Open(e.to_string()))?;
        let read_only = *read_only;
        // `acquire_timeout` also bounds establishing the first connection.
        let pool = rt
            .block_on(
                sqlx::mysql::MySqlPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(Duration::from_secs(10))
                    .after_connect(move |conn, _| {
                        Box::pin(async move {
                            if read_only {
                                sqlx::query("SET SESSION TRANSACTION READ ONLY").execute(conn).await?;
                            }
                            Ok(())
                        })
                    })
                    .connect(&connection_string),
            )
            .map_err(|e| DbError::Open(e.to_string()))?;
        let server_version = rt
            .block_on(async {
                let row = sqlx::query("SELECT VERSION()")
                    .fetch_one(&pool)
                    .await
                    .map_err(|e| e.to_string())?;
                use sqlx::Row as _;
                row.try_get::<String, _>(0).map_err(|e| e.to_string())
            })
            .unwrap_or_else(|_| "MySQL".to_owned());
        let connection_id = rt
            .block_on(async {
                let row = sqlx::query("SELECT CONNECTION_ID()")
                    .fetch_one(&pool)
                    .await
                    .map_err(|e| e.to_string())?;
                use sqlx::Row as _;
                row.try_get::<u64, _>(0).map_err(|e| e.to_string())
            })
            .unwrap_or(0);
        Ok(Self {
            rt,
            pool,
            mode: AutoMode::On,
            connection_string,
            database: database.clone(),
            server_version,
            connection_id,
            _tunnel: tunnel,
        })
    }

    fn autocommit(&self) -> bool {
        matches!(self.mode, AutoMode::On)
    }

    /// One statement per call. Autocommit on acquires a connection for the
    /// call; autocommit off runs the statement on the pinned connection.
    fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        match self.mode {
            AutoMode::On => {
                let rt = &self.rt;
                let pool = &self.pool;
                rt.block_on(async {
                    let mut conn = pool
                        .acquire()
                        .await
                        .map_err(|e| DbError::Query(e.to_string()))?;
                    my_statement(&mut conn, sql).await
                })
            }
            AutoMode::Manual | AutoMode::Pinned { .. } => self.query_manual(sql),
        }
    }

    fn query_manual(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let Self { rt, pool, mode, .. } = self;
        rt.block_on(async {
            if let AutoMode::Manual = mode {
                let conn = pool
                    .acquire()
                    .await
                    .map_err(|e| DbError::Query(e.to_string()))?;
                *mode = AutoMode::Pinned {
                    conn,
                    open: false,
                    idle_since: Instant::now(),
                    warned: false,
                };
            }
            let AutoMode::Pinned { conn, open, .. } = mode else {
                return Err(DbError::Query("autocommit is on".to_owned()));
            };
            let control = tx_control(sql);
            let plan = manual_plan(*open, control);
            let started = Instant::now();
            let outcome = match plan {
                ManualPlan::Skip => QueryResult::from_columns(
                    Vec::new(),
                    false,
                    Some(0),
                    started.elapsed(),
                )
                .map_err(|e| DbError::Query(e.to_string())),
                ManualPlan::Only if control == Some(TxControl::Begin) => {
                    match my_begin(conn).await {
                        Ok(()) => {
                            *open = true;
                            QueryResult::from_columns(
                                Vec::new(),
                                false,
                                Some(0),
                                started.elapsed(),
                            )
                            .map_err(|e| DbError::Query(e.to_string()))
                        }
                        Err(error) => Err(error),
                    }
                }
                ManualPlan::Only => {
                    let result = my_statement(conn, sql).await;
                    if result.is_ok()
                        && matches!(control, Some(TxControl::Commit | TxControl::Rollback))
                    {
                        *open = false;
                    }
                    result
                }
                ManualPlan::BeginThenRun => match my_begin(conn).await {
                    Ok(()) => {
                        *open = true;
                        my_statement(conn, sql).await
                    }
                    Err(error) => Err(error),
                },
            };
            if plan != ManualPlan::Skip {
                mode.note_activity();
            }
            outcome
        })
    }

    /// Switch autocommit. Leaving manual mode rolls an open transaction back
    /// and returns the pin to the pool; a failed rollback closes the
    /// connection instead, so the pool never hands out a dirty session.
    fn set_autocommit(&mut self, on: bool) -> Result<(), DbError> {
        if !on {
            if matches!(self.mode, AutoMode::On) {
                self.mode = AutoMode::Manual;
            }
            return Ok(());
        }
        match std::mem::replace(&mut self.mode, AutoMode::On) {
            AutoMode::On | AutoMode::Manual => Ok(()),
            AutoMode::Pinned { mut conn, open, .. } => {
                let rt = &self.rt;
                rt.block_on(async {
                    let ended = if open {
                        sqlx::raw_sql("ROLLBACK")
                            .execute(&mut *conn)
                            .await
                            .map_err(|e| DbError::Query(e.to_string()))
                            .map(|_| ())
                    } else {
                        Ok(())
                    };
                    match ended {
                        Ok(()) => drop(conn),
                        Err(error) => {
                            conn.close_on_drop();
                            drop(conn);
                            return Err(error);
                        }
                    }
                    Ok(())
                })
            }
        }
    }

    /// An idle tick: the rollback runs on the pin, which stays held; a
    /// failed rollback closes the connection and drops the pin.
    fn tick_tx_idle(&mut self, idle: &TxIdle) -> Option<IdleTick> {
        let tick = self.mode.idle_tick(idle)?;
        if tick == IdleTick::Rollback {
            let rt = &self.rt;
            let mode = &mut self.mode;
            let failed = match mode {
                AutoMode::Pinned { conn, open, .. } => {
                    *open && rt.block_on(sqlx::raw_sql("ROLLBACK").execute(&mut **conn)).is_err()
                }
                _ => false,
            };
            mode.tx_close();
            if failed {
                if let AutoMode::Pinned { mut conn, .. } =
                    std::mem::replace(mode, AutoMode::Manual)
                {
                    conn.close_on_drop();
                    let _ = rt.block_on(async { drop(conn) });
                }
            }
        }
        Some(tick)
    }

    /// One `DatabaseNode` per visible database, each with a single schema
    /// node named after the database. A user without the privilege to list
    /// databases still sees the connected one.
    fn schema(&mut self) -> Result<SchemaInfo, DbError> {
        // Manual mode holds the pool's only connection, so the load runs on
        // the pin instead of timing out on an acquire.
        let mut held = match &mut self.mode {
            AutoMode::Pinned { conn, .. } => SchemaConn::Pinned(conn),
            _ => {
                let conn = self
                    .rt
                    .block_on(self.pool.acquire())
                    .map_err(|e| DbError::Query(e.to_string()))?;
                SchemaConn::Leased(PooledLease::new(&self.rt, conn))
            }
        };
        let conn: &mut sqlx::MySqlConnection = held.conn();
        let database_names: Vec<String> = match self.rt.block_on(
            sqlx::query(
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name NOT IN ('information_schema', 'performance_schema', 'mysql', 'sys') \
                 ORDER BY schema_name",
            )
            .fetch_all(&mut *conn),
        ) {
            Ok(rows) => rows
                .iter()
                .map(|row| my_string(row, 0))
                .collect::<Result<Vec<_>, DbError>>()?,
            Err(_) => vec![self.database.clone()],
        };
        let database_names: Vec<String> =
            database_names.into_iter().take(SCHEMA_SCHEMA_CAP).collect();
        let mut tables_truncated = false;
        let mut columns_truncated = false;
        let mut databases = Vec::new();
        for database in &database_names {
            let table_rows: Vec<(String, String)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT table_name, table_type FROM information_schema.tables \
                         WHERE table_schema = ? \
                         AND table_type IN ('BASE TABLE', 'VIEW') \
                         ORDER BY table_name",
                    )
                    .bind(database)
                    .fetch_all(&mut *conn),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| Ok((my_string(row, 0)?, my_string(row, 1)?)))
                .collect::<Result<Vec<_>, DbError>>()?;
            if table_rows.len() > SCHEMA_TABLE_CAP {
                tables_truncated = true;
            }
            let order: Vec<(String, TableKind)> = table_rows
                .into_iter()
                .take(SCHEMA_TABLE_CAP)
                .map(|(name, kind)| {
                    (name, if kind == "VIEW" { TableKind::View } else { TableKind::Table })
                })
                .collect();
            let column_rows: Vec<(String, String, String, String)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT table_name, column_name, column_type, is_nullable \
                         FROM information_schema.columns \
                         WHERE table_schema = ? \
                         ORDER BY table_name, ordinal_position",
                    )
                    .bind(database)
                    .fetch_all(&mut *conn),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    Ok((
                        my_string(row, 0)?,
                        my_string(row, 1)?,
                        my_string(row, 2)?,
                        my_string(row, 3)?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let mut columns_by_table: std::collections::HashMap<
                String,
                Vec<(String, String, bool)>,
            > = std::collections::HashMap::new();
            for (table, column, column_type, is_nullable) in column_rows {
                columns_by_table.entry(table).or_default().push((
                    column,
                    column_type,
                    is_nullable == "YES",
                ));
            }
            let pk_rows: Vec<(String, String)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT table_name, column_name \
                         FROM information_schema.key_column_usage \
                         WHERE table_schema = ? AND constraint_name = 'PRIMARY' \
                         ORDER BY table_name, ordinal_position",
                    )
                    .bind(database)
                    .fetch_all(&mut *conn),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| Ok((my_string(row, 0)?, my_string(row, 1)?)))
                .collect::<Result<Vec<_>, DbError>>()?;
            let mut pk_by_table: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for (table, column) in pk_rows {
                pk_by_table.entry(table).or_default().push(column);
            }
            let index_rows: Vec<(String, String, i64, Option<String>)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT table_name, index_name, non_unique, column_name \
                         FROM information_schema.statistics \
                         WHERE table_schema = ? \
                         ORDER BY table_name, index_name, seq_in_index",
                    )
                    .bind(database)
                    .fetch_all(&mut *conn),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    use sqlx::Row as _;
                    let non_unique = row
                        .try_get_unchecked::<i64, _>(2)
                        .map_err(|e| DbError::Query(e.to_string()))?;
                    // A functional index has no column name.
                    let column = row
                        .try_get_unchecked::<Option<String>, _>(3)
                        .map_err(|e| DbError::Query(e.to_string()))?;
                    Ok((my_string(row, 0)?, my_string(row, 1)?, non_unique, column))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let mut idx_grouped: std::collections::HashMap<
                String,
                Vec<(String, bool, Vec<String>)>,
            > = std::collections::HashMap::new();
            {
                let mut current: Option<(String, String, bool, Vec<String>)> = None;
                let flush = |current: &mut Option<(String, String, bool, Vec<String>)>,
                             map: &mut std::collections::HashMap<
                    String,
                    Vec<(String, bool, Vec<String>)>,
                >| {
                    if let Some((table, name, unique, cols)) = current.take() {
                        map.entry(table).or_default().push((name, unique, cols));
                    }
                };
                for (table, index, non_unique, column) in index_rows {
                    match current.as_mut() {
                        Some(cur) if cur.0 == table && cur.1 == index => {
                            if let Some(col) = column {
                                if !col.trim().is_empty() {
                                    cur.3.push(col);
                                }
                            }
                        }
                        _ => {
                            flush(&mut current, &mut idx_grouped);
                            let cols = match column {
                                Some(col) if !col.trim().is_empty() => vec![col],
                                _ => Vec::new(),
                            };
                            current = Some((table, index, non_unique == 0, cols));
                        }
                    }
                }
                flush(&mut current, &mut idx_grouped);
            }
            let mut idx_by_table: std::collections::HashMap<String, Vec<IndexInfo>> =
                std::collections::HashMap::new();
            let mut idx_truncated_by_table: std::collections::HashMap<String, bool> =
                std::collections::HashMap::new();
            let mut schema_indexes: Vec<IndexRef> = Vec::new();
            for (table, groups) in idx_grouped {
                if groups.len() > SCHEMA_INDEX_CAP {
                    idx_truncated_by_table.insert(table.clone(), true);
                }
                let mut idxs = Vec::new();
                for (index_name, unique, idx_cols) in
                    groups.into_iter().take(SCHEMA_INDEX_CAP)
                {
                    schema_indexes.push(
                        IndexRef::new(table.clone(), index_name.clone())
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    );
                    idxs.push(
                        IndexInfo::new(index_name, idx_cols, unique)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    );
                }
                idx_by_table.insert(table, idxs);
            }
            schema_indexes.sort_by(|a, b| (a.table(), a.index()).cmp(&(b.table(), b.index())));
            schema_indexes.truncate(SCHEMA_SCHEMA_INDEX_CAP);
            let trigger_rows: Vec<(String, String, String, String, String)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT trigger_name, event_object_table, action_timing, \
                         event_manipulation, action_statement \
                         FROM information_schema.triggers \
                         WHERE trigger_schema = ? \
                         ORDER BY event_object_table, trigger_name",
                    )
                    .bind(database)
                    .fetch_all(&mut *conn),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    Ok((
                        my_string(row, 0)?,
                        my_string(row, 1)?,
                        my_string(row, 2)?,
                        my_string(row, 3)?,
                        my_string(row, 4)?,
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let triggers_truncated = trigger_rows.len() > SCHEMA_TRIGGER_CAP;
            let mut triggers = Vec::new();
            for (name, table, timing, event, statement) in
                trigger_rows.into_iter().take(SCHEMA_TRIGGER_CAP)
            {
                triggers.push(
                    TriggerInfo::new(
                        name,
                        Some(table),
                        Some(format!("{timing} {event}")),
                        Some(statement).filter(|s| !s.trim().is_empty()),
                    )
                    .map_err(|e| DbError::Query(e.to_string()))?,
                );
            }
            let routine_rows: Vec<(String, String, String, Option<String>)> = self
                .rt
                .block_on(
                    sqlx::query(
                        "SELECT routine_name, routine_type, external_language, \
                         routine_definition \
                         FROM information_schema.routines \
                         WHERE routine_schema = ? \
                         ORDER BY routine_name",
                    )
                    .bind(database)
                    .fetch_all(&mut *conn),
                )
                .map_err(|e| DbError::Query(e.to_string()))?
                .iter()
                .map(|row| {
                    use sqlx::Row as _;
                    // NULL when the connected user cannot see the body.
                    let definition = row
                        .try_get_unchecked::<Option<String>, _>(3)
                        .map_err(|e| DbError::Query(e.to_string()))?;
                    Ok((
                        my_string(row, 0)?,
                        my_string(row, 1)?,
                        my_string(row, 2)?,
                        definition.map(|d| d.trim().to_owned()).filter(|d| !d.is_empty()),
                    ))
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            let routines_truncated = routine_rows.len() > SCHEMA_ROUTINE_CAP;
            let mut routines = Vec::new();
            for (name, kind, language, definition) in
                routine_rows.into_iter().take(SCHEMA_ROUTINE_CAP)
            {
                routines.push(
                    RoutineInfo::new(name, Some(kind), Some(language), definition)
                        .map_err(|e| DbError::Query(e.to_string()))?,
                );
            }
            let mut tables = Vec::new();
            let mut views = Vec::new();
            for (name, kind) in &order {
                let col_rows = columns_by_table.get(name).cloned().unwrap_or_default();
                if col_rows.len() > SCHEMA_COLUMN_CAP {
                    columns_truncated = true;
                }
                let pk_cols = pk_by_table.get(name).cloned().unwrap_or_default();
                let mut pk_pos: std::collections::HashMap<String, u32> =
                    std::collections::HashMap::new();
                for (i, col) in pk_cols.iter().enumerate() {
                    pk_pos.insert(col.clone(), (i + 1) as u32);
                }
                let mut cols = Vec::new();
                for (col_name, type_name, nullable) in
                    col_rows.into_iter().take(SCHEMA_COLUMN_CAP)
                {
                    let pk_position = pk_pos.get(&col_name).copied();
                    let nullable = if pk_position.is_some() { false } else { nullable };
                    cols.push(
                        ColumnInfo::new(col_name, type_name, nullable, pk_position)
                            .map_err(|e| DbError::Query(e.to_string()))?,
                    );
                }
                let indexes = idx_by_table.get(name).cloned().unwrap_or_default();
                let constraints = mysql_table_constraints(&pk_cols, &indexes)?;
                let idx_truncated =
                    idx_truncated_by_table.get(name).copied().unwrap_or(false);
                let table = TableInfo::new(
                    name.clone(),
                    *kind,
                    cols,
                    constraints,
                    Vec::new(),
                    Vec::new(),
                    indexes,
                    false,
                    idx_truncated,
                )
                .map_err(|e| DbError::Query(e.to_string()))?;
                match kind {
                    TableKind::Table => tables.push(table),
                    TableKind::View => views.push(table),
                }
            }
            let schema_node = SchemaNode::new(
                database.clone(),
                tables,
                views,
                Vec::new(),
                schema_indexes,
                triggers,
                routines,
                triggers_truncated,
                routines_truncated,
            )
            .map_err(|e| DbError::Query(e.to_string()))?;
            databases.push(
                DatabaseNode::new(database.clone(), vec![schema_node])
                    .map_err(|e| DbError::Query(e.to_string()))?,
            );
        }
        Ok(SchemaInfo::new(databases, tables_truncated, columns_truncated))
    }

    /// Live backends from `information_schema.processlist` with lock waits
    /// folded in from `performance_schema.data_lock_waits`, both read-only.
    /// The locks read is best effort: a missing or disabled
    /// `performance_schema` reads as no waits. Runs on the manual pin when
    /// one is held, else on a pool lease. The row carrying this session's
    /// connection id is marked self.
    fn sessions(&mut self) -> Result<Vec<SessionInfo>, DbError> {
        use sqlx::Row as _;
        // Manual mode holds the pool's only connection, so the list runs on
        // the pin instead of timing out on an acquire.
        let mut held = match &mut self.mode {
            AutoMode::Pinned { conn, .. } => SchemaConn::Pinned(conn),
            _ => {
                let conn = self
                    .rt
                    .block_on(self.pool.acquire())
                    .map_err(|e| DbError::Query(e.to_string()))?;
                SchemaConn::Leased(PooledLease::new(&self.rt, conn))
            }
        };
        let conn: &mut sqlx::MySqlConnection = held.conn();
        let rows = self
            .rt
            .block_on(sqlx::query(mysql_sessions_sql()).fetch_all(&mut *conn))
            .map_err(|e| DbError::Query(e.to_string()))?;
        let own = self.connection_id;
        let mut out = Vec::with_capacity(rows.len().min(SESSION_CAP));
        for row in rows.iter().take(SESSION_CAP) {
            let id: u64 = my_cell_u64(row, 0)?;
            let user: Option<String> = my_cell_string(row, 1)?;
            let database: Option<String> = my_cell_string(row, 2)?;
            let command: Option<String> = my_cell_string(row, 3)?;
            let state: Option<String> = my_cell_string(row, 4)?;
            let query: Option<String> = my_cell_string(row, 5)?;
            out.push(mysql_session_from_parts(
                id, user, database, command, state, query, id == own,
            )?);
        }
        let waits: Vec<(u64, Option<u64>)> = match self
            .rt
            .block_on(sqlx::query(mysql_lock_waits_sql()).fetch_all(&mut *conn))
        {
            Ok(lock_rows) => lock_rows
                .iter()
                .filter_map(|row| {
                    let requesting: Option<u64> =
                        row.try_get_unchecked(0).unwrap_or(None);
                    let blocking: Option<u64> =
                        row.try_get_unchecked(1).unwrap_or(None);
                    requesting.map(|id| (id, blocking))
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        apply_mysql_lock_waits(&mut out, &waits);
        Ok(out)
    }

    /// Kill one connection by its panel id. Refuses this session's own id,
    /// so the lister cannot kill its own connection. The id is parsed to an
    /// integer before it reaches SQL, so the statement cannot inject.
    fn terminate_session(&mut self, backend: &str) -> Result<(), DbError> {
        let id = parse_mysql_backend(backend)?;
        if id == self.connection_id {
            return Err(DbError::Query("cannot terminate this session".to_owned()));
        }
        let mut held = match &mut self.mode {
            AutoMode::Pinned { conn, .. } => SchemaConn::Pinned(conn),
            _ => {
                let conn = self
                    .rt
                    .block_on(self.pool.acquire())
                    .map_err(|e| DbError::Query(e.to_string()))?;
                SchemaConn::Leased(PooledLease::new(&self.rt, conn))
            }
        };
        let conn: &mut sqlx::MySqlConnection = held.conn();
        let sql = mysql_terminate_sql(id);
        self.rt
            .block_on(sqlx::raw_sql(&sql).execute(&mut *conn))
            .map(|_| ())
            .map_err(|e| DbError::Query(e.to_string()))
    }
}

#[cfg(feature = "mysql")]
async fn my_begin(conn: &mut sqlx::MySqlConnection) -> Result<(), DbError> {
    sqlx::raw_sql("BEGIN")
        .execute(&mut *conn)
        .await
        .map(|_| ())
        .map_err(|e| DbError::Query(e.to_string()))
}

/// One statement on one connection. MySQL refuses BEGIN in the prepared
/// statement protocol, and even an empty bind set prepares, so transaction
/// controls run through `raw_sql` on the text protocol; everything else
/// describes first so a data-changing statement runs exactly once.
#[cfg(feature = "mysql")]
async fn my_statement(conn: &mut sqlx::MySqlConnection, sql: &str) -> Result<QueryResult, DbError> {
    use sqlx::{Column as _, Executor as _, TypeInfo as _};
    let started = Instant::now();
    if tx_control(sql).is_some() {
        let done = sqlx::raw_sql(sql)
            .execute(&mut *conn)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        return QueryResult::from_columns(
            Vec::new(),
            false,
            Some(done.rows_affected()),
            started.elapsed(),
        )
        .map_err(|e| DbError::Query(e.to_string()));
    }
    let columns: Vec<(String, String)> = (&mut *conn)
        .describe(sql)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?
        .columns()
        .iter()
        .map(|c| (c.name().to_owned(), c.type_info().name().to_owned()))
        .collect();
    if columns.is_empty() {
        let done = sqlx::query(sql)
            .execute(&mut *conn)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        return QueryResult::from_columns(
            Vec::new(),
            false,
            Some(done.rows_affected()),
            started.elapsed(),
        )
        .map_err(|e| DbError::Query(e.to_string()));
    }
    let rows = sqlx::query(sql)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
    let mut builder = ResultBuilder::new(columns, ROW_CAP);
    for row in &rows {
        if !builder.push_row(map_my_row(row)?) {
            break;
        }
    }
    Ok(builder.finish(started.elapsed()))
}

#[cfg(feature = "mysql")]
impl Drop for MySession {
    /// The pin must go back inside the runtime: sqlx spawns the
    /// return-to-pool task on drop, and that needs a Tokio context.
    fn drop(&mut self) {
        if let AutoMode::Pinned { mut conn, open, .. } =
            std::mem::replace(&mut self.mode, AutoMode::On)
        {
            let rt = &self.rt;
            rt.block_on(async {
                if open {
                    let _ = sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await;
                }
                drop(conn);
            });
        }
    }
}

/// Mirrors `sqlite_table_constraints`: MySQL keeps the primary key in
/// `key_column_usage` and unique keys as unique indexes, so synthesize both.
/// The primary constraint is named `PRIMARY`, the server's own name, so the
/// DDL builder folds the primary index into it instead of emitting a
/// duplicate CREATE INDEX.
#[cfg(feature = "mysql")]
fn mysql_table_constraints(
    pk_cols: &[String],
    indexes: &[IndexInfo],
) -> Result<Vec<ConstraintInfo>, DbError> {
    let mut out = Vec::new();
    if !pk_cols.is_empty() {
        out.push(
            ConstraintInfo::new(
                "PRIMARY".to_owned(),
                ConstraintKind::Pk,
                pk_cols.to_vec(),
                None,
                Vec::new(),
                None,
            )
            .map_err(|e| DbError::Query(e.to_string()))?,
        );
    }
    for index in indexes.iter().filter(|i| i.is_unique() && i.name() != "PRIMARY") {
        out.push(
            ConstraintInfo::new(
                index.name().to_owned(),
                ConstraintKind::Unique,
                index.columns().to_vec(),
                None,
                Vec::new(),
                None,
            )
            .map_err(|e| DbError::Query(e.to_string()))?,
        );
    }
    Ok(out)
}

/// Builds the MySQL cancel closure. Owns the connection string plus the
/// connection id captured at connect. Unknown id no-ops. Else a fresh
/// current-thread runtime opens one connection and runs KILL QUERY. Every
/// error collapses into unit. Never panics, safe to fire twice.
#[cfg(feature = "mysql")]
fn mysql_cancel_handle(connection_string: String, connection_id: u64) -> CancelHandle {
    CancelHandle::new(move || {
        if connection_id == 0 {
            return;
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            rt.block_on(async {
                let pool = match sqlx::mysql::MySqlPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(Duration::from_secs(5))
                    .connect(&connection_string)
                    .await
                {
                    Ok(pool) => pool,
                    Err(_) => return,
                };
                let _ = sqlx::query(&format!("KILL QUERY {connection_id}"))
                    .execute(&pool)
                    .await;
                pool.close().await;
            });
        }));
    })
}

/// One MySQL column's decoding rule, keyed by `TypeInfo::name()`.
#[cfg(feature = "mysql")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MyCellKind {
    /// TINYINT through BIGINT, plus BOOLEAN (MySQL's TINYINT(1) alias).
    Signed,
    /// The `* UNSIGNED` names. Values above i64::MAX print as text.
    Unsigned,
    Float,
    Year,
    Date,
    Time,
    /// DATETIME and TIMESTAMP.
    DateTime,
    /// Binary payloads: BINARY, VARBINARY, the BLOB family, BIT, GEOMETRY.
    Blob,
    /// Text rendering: strings, decimals, JSON, enum, set, and anything
    /// unlisted.
    Text,
}

#[cfg(feature = "mysql")]
fn my_cell_kind(type_name: &str) -> MyCellKind {
    match type_name {
        "BOOLEAN" | "TINYINT" | "SMALLINT" | "INT" | "MEDIUMINT" | "BIGINT" => MyCellKind::Signed,
        "TINYINT UNSIGNED"
        | "SMALLINT UNSIGNED"
        | "INT UNSIGNED"
        | "MEDIUMINT UNSIGNED"
        | "BIGINT UNSIGNED" => MyCellKind::Unsigned,
        "FLOAT" | "DOUBLE" => MyCellKind::Float,
        "YEAR" => MyCellKind::Year,
        "DATE" => MyCellKind::Date,
        "TIME" => MyCellKind::Time,
        "DATETIME" | "TIMESTAMP" => MyCellKind::DateTime,
        "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BIT"
        | "GEOMETRY" => MyCellKind::Blob,
        _ => MyCellKind::Text,
    }
}

/// Renders a MySQL binary temporal payload the way the server's text
/// protocol would. Byte one is the payload length, then little-endian date
/// and time fields; a payload that fails the length check passes through as
/// bytes, which keeps a string-shaped value readable.
#[cfg(feature = "mysql")]
fn my_temporal_text(type_name: &str, bytes: &[u8]) -> String {
    let Some((&len, body)) = bytes.split_first() else {
        return String::new();
    };
    if usize::from(len) != body.len() {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let year = || u16::from_le_bytes([body[0], body[1]]);
    let u32_at =
        |at: usize| u32::from_le_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
    match type_name {
        "DATE" => match body.len() {
            0 => "0000-00-00".to_owned(),
            4 => format!("{:04}-{:02}-{:02}", year(), body[2], body[3]),
            _ => String::from_utf8_lossy(bytes).into_owned(),
        },
        "TIME" => match body.len() {
            0 => "00:00:00".to_owned(),
            8 | 12 => {
                let sign = if body[0] == 0 { "" } else { "-" };
                let hours = u32_at(1) * 24 + u32::from(body[5]);
                let mut out = format!("{sign}{hours:02}:{:02}:{:02}", body[6], body[7]);
                if body.len() == 12 {
                    out.push_str(&format!(".{:06}", u32_at(8)));
                }
                out
            }
            _ => String::from_utf8_lossy(bytes).into_owned(),
        },
        _ => match body.len() {
            0 => "0000-00-00 00:00:00".to_owned(),
            4 => format!("{:04}-{:02}-{:02} 00:00:00", year(), body[2], body[3]),
            7 | 11 => {
                let mut out = format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    year(),
                    body[2],
                    body[3],
                    body[4],
                    body[5],
                    body[6]
                );
                if body.len() == 11 {
                    out.push_str(&format!(".{:06}", u32_at(7)));
                }
                out
            }
            _ => String::from_utf8_lossy(bytes).into_owned(),
        },
    }
}

/// Pure URL builder for MySQL. User, password, and database are
/// percent-encoded; an empty or missing password omits the secret part.
/// `Preferred` leaves the URL untouched; any other mode appends `ssl-mode`.
pub fn mysql_url(
    host: &str,
    port: u16,
    database: &str,
    user: &str,
    password: Option<&str>,
    ssl_mode: MysqlSslMode,
) -> String {
    let user_enc = percent_encode(user);
    let db_enc = percent_encode(database);
    let base = match password {
        Some(s) if !s.is_empty() => {
            let enc: String = percent_encode(s);
            format!("mysql://{user_enc}:{enc}@{host}:{port}/{db_enc}")
        }
        _ => format!("mysql://{user_enc}@{host}:{port}/{db_enc}"),
    };
    if ssl_mode == MysqlSslMode::Preferred {
        base
    } else {
        format!("{base}?ssl-mode={}", ssl_mode.as_str())
    }
}

/// One `information_schema` string cell. Unchecked because MySQL 8 reports
/// some of those columns with the binary flag, which the checked path
/// rejects; the value is still decoded from UTF-8 and never coerced.
#[cfg(feature = "mysql")]
fn my_string(row: &sqlx::mysql::MySqlRow, index: usize) -> Result<String, DbError> {
    use sqlx::Row as _;
    row.try_get_unchecked::<String, _>(index)
        .map_err(|e| DbError::Query(e.to_string()))
}

/// One processlist integer cell. Unchecked for the same binary-flag reason
/// as [`my_string`].
#[cfg(feature = "mysql")]
fn my_cell_u64(row: &sqlx::mysql::MySqlRow, index: usize) -> Result<u64, DbError> {
    use sqlx::Row as _;
    row.try_get_unchecked::<u64, _>(index)
        .map_err(|e| DbError::Query(e.to_string()))
}

/// One nullable processlist text cell. Unchecked for the same binary-flag
/// reason as [`my_string`].
#[cfg(feature = "mysql")]
fn my_cell_string(
    row: &sqlx::mysql::MySqlRow,
    index: usize,
) -> Result<Option<String>, DbError> {
    use sqlx::Row as _;
    row.try_get_unchecked::<Option<String>, _>(index)
        .map_err(|e| DbError::Query(e.to_string()))
}

#[cfg(feature = "mysql")]
fn map_my_row(row: &sqlx::mysql::MySqlRow) -> Result<Vec<Value>, DbError> {
    use sqlx::{Column as _, Row as _, TypeInfo as _};
    let mut cells = Vec::with_capacity(row.len());
    for index in 0..row.len() {
        let type_name = row.column(index).type_info().name().to_owned();
        cells.push(decode_my_cell(row, index, &type_name)?);
    }
    Ok(cells)
}

#[cfg(feature = "mysql")]
fn decode_my_cell(
    row: &sqlx::mysql::MySqlRow,
    index: usize,
    type_name: &str,
) -> Result<Value, DbError> {
    match my_cell_kind(type_name) {
        MyCellKind::Signed => my_option_cell(row, index, Value::Integer),
        MyCellKind::Unsigned => {
            use sqlx::Row as _;
            row.try_get_unchecked::<Option<u64>, _>(index)
                .map(|cell| match cell {
                    Some(n) => match i64::try_from(n) {
                        Ok(n) => Value::Integer(n),
                        Err(_) => Value::Text(n.to_string()),
                    },
                    None => Value::Null,
                })
                .map_err(|e| DbError::Query(e.to_string()))
        }
        MyCellKind::Float => my_option_cell(row, index, Value::Real),
        MyCellKind::Year => my_option_cell(row, index, |n: u16| Value::Integer(i64::from(n))),
        MyCellKind::Date | MyCellKind::Time | MyCellKind::DateTime => {
            use sqlx::Row as _;
            row.try_get_unchecked::<Option<Vec<u8>>, _>(index)
                .map(|cell| match cell {
                    Some(bytes) => Value::Text(my_temporal_text(type_name, &bytes)),
                    None => Value::Null,
                })
                .map_err(|e| DbError::Query(e.to_string()))
        }
        MyCellKind::Blob => my_option_cell(row, index, Value::Blob),
        // The binary protocol already sends decimals, strings, and JSON as
        // their display bytes, so the text decode is the whole job.
        MyCellKind::Text => {
            use sqlx::Row as _;
            row.try_get_unchecked::<Option<String>, _>(index)
                .map(|cell| cell.map(Value::Text).unwrap_or(Value::Null))
                .map_err(|e| DbError::Query(e.to_string()))
        }
    }
}

/// Unchecked decode, because MySQL's leading `UNSIGNED` flag does not change
/// the type name for TINYINT(1) and would fail the checked compatibility gate.
#[cfg(feature = "mysql")]
fn my_option_cell<T>(
    row: &sqlx::mysql::MySqlRow,
    index: usize,
    into_value: impl FnOnce(T) -> Value,
) -> Result<Value, DbError>
where
    T: for<'r> sqlx::Decode<'r, sqlx::MySql> + sqlx::Type<sqlx::MySql>,
{
    use sqlx::Row as _;
    row.try_get_unchecked::<Option<T>, _>(index)
        .map(|cell| cell.map(into_value).unwrap_or(Value::Null))
        .map_err(|e| DbError::Query(e.to_string()))
}

/// Builds the Postgres cancel closure. Owns the connection string plus the
/// backend pid captured at connect. Unknown pid no-ops. Else a fresh
/// current-thread runtime opens one connection and runs pg_cancel_backend.
/// Every error collapses into unit. Never panics, safe to fire twice.
#[cfg(feature = "postgres")]
fn pg_cancel_handle(connection_string: String, backend_pid: i32) -> CancelHandle {
    CancelHandle::new(move || {
        if backend_pid < 0 {
            return;
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            rt.block_on(async {
                let pool = match sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(Duration::from_secs(5))
                    .connect(&connection_string)
                    .await
                {
                    Ok(pool) => pool,
                    Err(_) => return,
                };
                let _ = sqlx::query("SELECT pg_cancel_backend($1)")
                    .bind(backend_pid)
                    .execute(&pool)
                    .await;
                pool.close().await;
            });
        }));
    })
}

/// One Postgres column's decoding rule, keyed by `TypeInfo::name()`.
#[cfg(feature = "postgres")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PgCellKind {
    Bool,
    Int16,
    Int32,
    Int64,
    Oid,
    Float32,
    Float64,
    Blob,
    /// Text rendering: strings, numerics, UUIDs, JSON, dates, times, and
    /// anything unlisted, including arrays and custom types.
    Text,
}

#[cfg(feature = "postgres")]
fn pg_cell_kind(type_name: &str) -> PgCellKind {
    match type_name {
        "BOOL" => PgCellKind::Bool,
        "INT2" => PgCellKind::Int16,
        "INT4" => PgCellKind::Int32,
        "INT8" => PgCellKind::Int64,
        "OID" => PgCellKind::Oid,
        "FLOAT4" => PgCellKind::Float32,
        "FLOAT8" => PgCellKind::Float64,
        "BYTEA" => PgCellKind::Blob,
        _ => PgCellKind::Text,
    }
}

/// Pure URL builder for Postgres. User, password, and database are
/// percent-encoded; an empty or missing password omits the secret part.
/// `Disable` leaves the URL untouched; any other mode appends `sslmode`.
pub fn postgres_url(
    host: &str,
    port: u16,
    database: &str,
    user: &str,
    password: Option<&str>,
    ssl_mode: PostgresSslMode,
) -> String {
    let user_enc = percent_encode(user);
    let db_enc = percent_encode(database);
    let base = match password {
        Some(s) if !s.is_empty() => {
            let enc: String = percent_encode(s);
            format!("postgres://{user_enc}:{enc}@{host}:{port}/{db_enc}")
        }
        _ => format!("postgres://{user_enc}@{host}:{port}/{db_enc}"),
    };
    if ssl_mode == PostgresSslMode::Disable {
        base
    } else {
        format!("{base}?sslmode={}", ssl_mode.as_str())
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_' || *b == b'.' || *b == b'~' {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A pasted connection URL split into its persisted shape and its secret.
/// `config` never holds the password; `password` carries the URL's userinfo
/// secret for the draft's transient password row.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedConnectionUrl {
    pub kind: DbKind,
    pub config: ConnectionConfig,
    pub password: Option<String>,
}

/// Pure parser inverting [`postgres_url`] and [`mysql_url`]. `postgresql`
/// and `postgres` feed the Postgres variant, `mysql` and `mariadb` the Mysql
/// one, `sqlite` and a bare path the Sqlite one. User, password, and
/// database are percent-decoded; `sslmode` maps onto [`PostgresSslMode`]
/// while every other query parameter is ignored. Any malformed input reports
/// one [`DbError::Config`] message.
pub fn parse_connection_url(raw: &str) -> Result<ParsedConnectionUrl, DbError> {
    let input = raw.trim();
    if input.is_empty() {
        return Err(invalid_url());
    }
    if let Some(scheme_end) = input.find("://") {
        let scheme = input[..scheme_end].to_ascii_lowercase();
        let rest = &input[scheme_end + 3..];
        match scheme.as_str() {
            "postgres" | "postgresql" => parse_network_url(DbKind::Postgres, rest),
            "mysql" | "mariadb" => parse_network_url(DbKind::Mysql, rest),
            "sqlite" => parse_sqlite_rest(rest),
            _ => Err(invalid_url()),
        }
    } else if let Some(colon) = scheme_prefix_len(input) {
        if input[..colon].eq_ignore_ascii_case("sqlite") {
            parse_sqlite_path(&input[colon + 1..], true)
        } else {
            Err(invalid_url())
        }
    } else {
        parse_sqlite_path(input, false)
    }
}

/// Length of the `scheme` in `scheme:rest`, if the input opens with one.
fn scheme_prefix_len(input: &str) -> Option<usize> {
    let colon = input.find(':')?;
    let scheme = &input[..colon];
    let mut chars = scheme.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => (),
        _ => return None,
    }
    if chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        Some(colon)
    } else {
        None
    }
}

fn invalid_url() -> DbError {
    DbError::Config("invalid connection URL".to_owned())
}

/// One `scheme://` authority split: `[user[:password]@]host[:port][/database]`.
fn parse_network_url(kind: DbKind, rest: &str) -> Result<ParsedConnectionUrl, DbError> {
    let (authority_path, query) = match rest.find('?') {
        Some(question) => (&rest[..question], Some(&rest[question + 1..])),
        None => (rest, None),
    };
    let (authority, database_enc) = match authority_path.find('/') {
        Some(slash) => (&authority_path[..slash], &authority_path[slash + 1..]),
        None => (authority_path, ""),
    };
    if database_enc.contains('/') {
        return Err(invalid_url());
    }
    let database = percent_decode(database_enc)?;
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(at) => (&authority[..at], &authority[at + 1..]),
        None => ("", authority),
    };
    let (user_enc, password_enc) = match userinfo.find(':') {
        Some(colon) => (&userinfo[..colon], Some(&userinfo[colon + 1..])),
        None => (userinfo, None),
    };
    let user = percent_decode(user_enc)?;
    let password =
        password_enc.map(percent_decode).transpose()?.filter(|secret| !secret.is_empty());
    let (host, port) = split_host_port(kind, hostport)?;
    let mut ssl_mode = PostgresSslMode::Disable;
    if let Some(query) = query {
        for param in query.split('&') {
            if param.is_empty() {
                continue;
            }
            let (key, value) = match param.find('=') {
                Some(eq) => (&param[..eq], &param[eq + 1..]),
                None => (param, ""),
            };
            if kind == DbKind::Postgres && key.eq_ignore_ascii_case("sslmode") {
                ssl_mode = match_ssl_mode(value).ok_or_else(invalid_url)?;
            }
        }
    }
    let config = match kind {
        DbKind::Postgres => ConnectionConfig::Postgres {
            host,
            port,
            database,
            user,
            ssl_mode,
            ssh: None,
            read_only: false,
        },
        DbKind::Mysql => {
            ConnectionConfig::Mysql {
                host,
                port,
                database,
                user,
                ssl_mode: MysqlSslMode::default(),
                ssh: None,
                read_only: false,
            }
        }
        DbKind::Sqlite => return Err(invalid_url()),
    };
    Ok(ParsedConnectionUrl { kind, config, password })
}

fn match_ssl_mode(value: &str) -> Option<PostgresSslMode> {
    if value.eq_ignore_ascii_case("disable") {
        Some(PostgresSslMode::Disable)
    } else if value.eq_ignore_ascii_case("prefer") {
        Some(PostgresSslMode::Prefer)
    } else if value.eq_ignore_ascii_case("require") {
        Some(PostgresSslMode::Require)
    } else if value.eq_ignore_ascii_case("verify-ca") {
        Some(PostgresSslMode::VerifyCa)
    } else if value.eq_ignore_ascii_case("verify-full") {
        Some(PostgresSslMode::VerifyFull)
    } else {
        None
    }
}

/// Host and port with the kind's default when the port is missing. Bracketed
/// IPv6 keeps its colons; anything else with a second colon is malformed.
fn split_host_port(kind: DbKind, hostport: &str) -> Result<(String, u16), DbError> {
    let default_port = kind.default_port().ok_or_else(invalid_url)?;
    if let Some(bracketed) = hostport.strip_prefix('[') {
        let end = bracketed.find(']').ok_or_else(invalid_url)?;
        let host = &bracketed[..end];
        if host.is_empty() {
            return Err(invalid_url());
        }
        let port = match &bracketed[end + 1..] {
            "" => default_port,
            suffixed => parse_port(suffixed.strip_prefix(':').ok_or_else(invalid_url)?)?,
        };
        return Ok((host.to_owned(), port));
    }
    match hostport.rfind(':') {
        Some(colon) => {
            let host = &hostport[..colon];
            if host.is_empty() || host.contains(':') || host.bytes().any(|b| b.is_ascii_whitespace()) {
                return Err(invalid_url());
            }
            Ok((host.to_owned(), parse_port(&hostport[colon + 1..])?))
        }
        None => {
            if hostport.is_empty() || hostport.bytes().any(|b| b.is_ascii_whitespace()) {
                return Err(invalid_url());
            }
            Ok((hostport.to_owned(), default_port))
        }
    }
}

fn parse_port(raw: &str) -> Result<u16, DbError> {
    if raw.is_empty() {
        return Err(invalid_url());
    }
    raw.parse::<u16>().map_err(|_| invalid_url())
}

/// The path after `sqlite://`, with an empty authority stripped so
/// `sqlite:///tmp/app.db` keeps its absolute path.
fn parse_sqlite_rest(rest: &str) -> Result<ParsedConnectionUrl, DbError> {
    let path_enc = match rest.find('?') {
        Some(question) => &rest[..question],
        None => rest,
    };
    parse_sqlite_path(path_enc, true)
}

fn parse_sqlite_path(path: &str, encoded: bool) -> Result<ParsedConnectionUrl, DbError> {
    let decoded = if encoded { percent_decode(path)? } else { path.to_owned() };
    if decoded.is_empty() {
        return Err(invalid_url());
    }
    Ok(ParsedConnectionUrl {
        kind: DbKind::Sqlite,
        config: ConnectionConfig::Sqlite { path: PathBuf::from(decoded), read_only: false },
        password: None,
    })
}

/// Percent-decoding without `+`-as-space: a raw `+` in userinfo stays a
/// plus, since the builders encode a space as `%20` and a plus as `%2B`.
fn percent_decode(s: &str) -> Result<String, DbError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let pair = (bytes.get(i + 1).copied(), bytes.get(i + 2).copied());
            match pair {
                (Some(hi), Some(lo))
                    if hi.is_ascii_hexdigit() && lo.is_ascii_hexdigit() =>
                {
                    out.push(hex_val(hi) * 16 + hex_val(lo));
                    i += 3;
                }
                _ => return Err(invalid_url()),
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| invalid_url())
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

#[cfg(feature = "postgres")]
fn map_pg_row(row: &sqlx::postgres::PgRow) -> Result<Vec<Value>, DbError> {
    use sqlx::{Column as _, Row as _, TypeInfo as _};
    let mut cells = Vec::with_capacity(row.len());
    for index in 0..row.len() {
        let type_name = row.column(index).type_info().name().to_owned();
        cells.push(decode_pg_cell(row, index, &type_name)?);
    }
    Ok(cells)
}

#[cfg(feature = "postgres")]
fn decode_pg_cell(
    row: &sqlx::postgres::PgRow,
    index: usize,
    type_name: &str,
) -> Result<Value, DbError> {
    match pg_cell_kind(type_name) {
        PgCellKind::Bool => pg_option_cell(row, index, |b: bool| Value::Integer(i64::from(b))),
        PgCellKind::Int16 => pg_option_cell(row, index, |n: i16| Value::Integer(i64::from(n))),
        PgCellKind::Int32 => pg_option_cell(row, index, |n: i32| Value::Integer(i64::from(n))),
        PgCellKind::Int64 => pg_option_cell(row, index, Value::Integer),
        PgCellKind::Oid => pg_option_cell(row, index, |o: sqlx::postgres::types::Oid| {
            Value::Integer(i64::from(o.0))
        }),
        PgCellKind::Float32 => pg_option_cell(row, index, |f: f32| Value::Real(f64::from(f))),
        PgCellKind::Float64 => pg_option_cell(row, index, Value::Real),
        PgCellKind::Blob => pg_option_cell(row, index, Value::Blob),
        // A query without binds runs on the simple protocol, so every value
        // arrives as text and the text-type compatibility gate is skipped for
        // NUMERIC, UUID, date/time, and JSON names.
        PgCellKind::Text => {
            use sqlx::Row as _;
            row.try_get_unchecked::<Option<String>, _>(index)
                .map(|cell| cell.map(Value::Text).unwrap_or(Value::Null))
                .map_err(|e| DbError::Query(e.to_string()))
        }
    }
}

#[cfg(feature = "postgres")]
fn pg_option_cell<T>(
    row: &sqlx::postgres::PgRow,
    index: usize,
    into_value: impl FnOnce(T) -> Value,
) -> Result<Value, DbError>
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    use sqlx::Row as _;
    row.try_get::<Option<T>, _>(index)
        .map(|cell| cell.map(into_value).unwrap_or(Value::Null))
        .map_err(|e| DbError::Query(e.to_string()))
}

fn decode_value(v: rusqlite::types::Value) -> Value {
    match v {
        rusqlite::types::Value::Null => Value::Null,
        rusqlite::types::Value::Integer(n) => Value::Integer(n),
        rusqlite::types::Value::Real(f) => Value::Real(f),
        rusqlite::types::Value::Text(s) => Value::Text(s),
        rusqlite::types::Value::Blob(b) => Value::Blob(b),
    }
}

fn execute_sqlite(
    conn: &rusqlite::Connection,
    sql: &str,
    max_rows: usize,
) -> Result<QueryResult, DbError> {
    let started = Instant::now();
    let mut stmt = conn.prepare(sql).map_err(|e| DbError::Query(e.to_string()))?;
    let width = stmt.column_count();
    if width == 0 {
        stmt.execute([]).map_err(|e| DbError::Query(e.to_string()))?;
        let affected = conn.changes();
        return QueryResult::from_columns(Vec::new(), false, Some(affected), started.elapsed())
            .map_err(|e| DbError::Query(e.to_string()));
    }
    let names: Vec<(String, String)> = stmt
        .columns()
        .iter()
        .map(|c| (c.name().to_owned(), c.decl_type().unwrap_or("").to_owned()))
        .collect();
    let mut builder = ResultBuilder::new(names, max_rows);
    {
        let mut mapped = stmt
            .query_map([], |row| {
                let mut cells = Vec::with_capacity(width);
                for i in 0..width {
                    cells.push(decode_value(row.get::<_, rusqlite::types::Value>(i)?));
                }
                Ok(cells)
            })
            .map_err(|e| DbError::Query(e.to_string()))?;
        for cells in mapped.by_ref() {
            let cells = cells.map_err(|e| DbError::Query(e.to_string()))?;
            if !builder.push_row(cells) {
                break;
            }
        }
    }
    Ok(builder.finish(started.elapsed()))
}

fn sqlite_foreign_keys(
    conn: &rusqlite::Connection,
    table: &str,
) -> Result<(Vec<ForeignKeyInfo>, bool), DbError> {
    let pragma = format!("PRAGMA foreign_key_list(\"{}\")", table.replace('"', "\"\""));
    let mut stmt = conn.prepare(&pragma).map_err(|e| DbError::Query(e.to_string()))?;
    let rows: Vec<(i64, i64, String, String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| DbError::Query(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| DbError::Query(e.to_string()))?;
    let mut grouped: std::collections::BTreeMap<i64, Vec<(i64, String, String, String)>> =
        std::collections::BTreeMap::new();
    for (id, seq, ref_table, from, to) in rows {
        grouped.entry(id).or_default().push((seq, ref_table, from, to));
    }
    let total = grouped.len();
    let truncated = total > SCHEMA_FK_CAP;
    let mut out = Vec::new();
    for (_, mut group) in grouped.into_iter().take(SCHEMA_FK_CAP) {
        group.sort_by_key(|(seq, _, _, _)| *seq);
        let ref_table = group.first().map(|(_, t, _, _)| t.clone()).unwrap_or_default();
        let columns: Vec<String> =
            group.iter().map(|(_, _, from, _)| from.clone()).collect();
        let ref_columns: Vec<String> =
            group.iter().map(|(_, _, _, to)| to.clone()).collect();
        out.push(
            ForeignKeyInfo::new(columns, ref_table, ref_columns, None)
                .map_err(|e| DbError::Query(e.to_string()))?,
        );
    }
    Ok((out, truncated))
}

fn sqlite_indexes(
    conn: &rusqlite::Connection,
    table: &str,
) -> Result<(Vec<IndexInfo>, bool), DbError> {
    let pragma = format!("PRAGMA index_list(\"{}\")", table.replace('"', "\"\""));
    let mut stmt = conn.prepare(&pragma).map_err(|e| DbError::Query(e.to_string()))?;
    let rows: Vec<(String, bool)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)? != 0))
        })
        .map_err(|e| DbError::Query(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| DbError::Query(e.to_string()))?;
    let mut names: Vec<(String, bool)> = rows
        .into_iter()
        .filter(|(name, _)| !name.starts_with("sqlite_"))
        .collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    let truncated = names.len() > SCHEMA_INDEX_CAP;
    let mut out = Vec::new();
    for (index_name, unique) in names.into_iter().take(SCHEMA_INDEX_CAP) {
        let info_pragma =
            format!("PRAGMA index_info(\"{}\")", index_name.replace('"', "\"\""));
        let mut info_stmt =
            conn.prepare(&info_pragma).map_err(|e| DbError::Query(e.to_string()))?;
        let mut cols: Vec<(i64, String)> = info_stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(2)?)))
            .map_err(|e| DbError::Query(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| DbError::Query(e.to_string()))?;
        cols.sort_by_key(|(seqno, _)| *seqno);
        let columns: Vec<String> = cols
            .into_iter()
            .map(|(_, name)| name)
            .filter(|n| !n.trim().is_empty())
            .collect();
        out.push(
            IndexInfo::new(index_name, columns, unique)
                .map_err(|e| DbError::Query(e.to_string()))?,
        );
    }
    Ok((out, truncated))
}

fn sqlite_database_name(conn: &rusqlite::Connection) -> String {
    let file: String = conn
        .query_row(
            "SELECT file FROM pragma_database_list WHERE name = 'main'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();
    if file.trim().is_empty() {
        ":memory:".to_owned()
    } else {
        file
    }
}

fn sqlite_table_constraints(
    columns: &[ColumnInfo],
    indexes: &[IndexInfo],
) -> Result<Vec<ConstraintInfo>, DbError> {
    let mut out = Vec::new();
    let mut pk_cols: Vec<(u32, String)> = columns
        .iter()
        .filter_map(|c| c.pk_position().map(|pos| (pos, c.name().to_owned())))
        .collect();
    pk_cols.sort_by_key(|(pos, _)| *pos);
    if !pk_cols.is_empty() {
        let names: Vec<String> = pk_cols.into_iter().map(|(_, n)| n).collect();
        out.push(
            ConstraintInfo::new(
                "PRIMARY KEY".to_owned(),
                ConstraintKind::Pk,
                names,
                None,
                Vec::new(),
                None,
            )
            .map_err(|e| DbError::Query(e.to_string()))?,
        );
    }
    for index in indexes.iter().filter(|i| i.is_unique()) {
        out.push(
            ConstraintInfo::new(
                index.name().to_owned(),
                ConstraintKind::Unique,
                index.columns().to_vec(),
                None,
                Vec::new(),
                None,
            )
            .map_err(|e| DbError::Query(e.to_string()))?,
        );
    }
    // CHECK constraints skipped: PRAGMA table_info and index_list do not
    // expose CHECK definitions, and parsing sqlite_master SQL is out of
    // scope for phase one.
    Ok(out)
}

fn sqlite_schema(conn: &rusqlite::Connection) -> Result<SchemaInfo, DbError> {
    let mut stmt = conn
        .prepare(
            "SELECT name, type FROM sqlite_master \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .map_err(|e| DbError::Query(e.to_string()))?;
    let entries: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|e| DbError::Query(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| DbError::Query(e.to_string()))?;
    let tables_truncated = entries.len() > SCHEMA_TABLE_CAP;
    let mut built: Vec<(TableInfo, Vec<ForeignKeyInfo>, String)> = Vec::new();
    let mut columns_truncated = false;
    let mut schema_indexes: Vec<IndexRef> = Vec::new();
    for (name, kind) in entries.into_iter().take(SCHEMA_TABLE_CAP) {
        let kind = if kind == "view" { TableKind::View } else { TableKind::Table };
        let pragma = format!("PRAGMA table_info(\"{}\")", name.replace('"', "\"\""));
        let mut col_stmt =
            conn.prepare(&pragma).map_err(|e| DbError::Query(e.to_string()))?;
        let raw_cols: Vec<(String, String, bool, u32)> = col_stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    row.get::<_, i64>(3)? != 0,
                    row.get::<_, i64>(5).unwrap_or(0).max(0) as u32,
                ))
            })
            .map_err(|e| DbError::Query(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| DbError::Query(e.to_string()))?;
        if raw_cols.len() > SCHEMA_COLUMN_CAP {
            columns_truncated = true;
        }
        let mut columns = Vec::new();
        for (col_name, type_name, notnull, pk) in
            raw_cols.into_iter().take(SCHEMA_COLUMN_CAP)
        {
            let pk_position = if pk > 0 { Some(pk) } else { None };
            let nullable = !notnull && pk_position.is_none();
            let col = ColumnInfo::new(col_name, type_name, nullable, pk_position)
                .map_err(|e| DbError::Query(e.to_string()))?;
            columns.push(col);
        }
        let (foreign_keys, fk_truncated) = if kind == TableKind::Table {
            sqlite_foreign_keys(conn, &name)?
        } else {
            (Vec::new(), false)
        };
        let (indexes, idx_truncated) = if kind == TableKind::Table {
            sqlite_indexes(conn, &name)?
        } else {
            (Vec::new(), false)
        };
        let constraints = if kind == TableKind::Table {
            sqlite_table_constraints(&columns, &indexes)?
        } else {
            Vec::new()
        };
        for index in &indexes {
            schema_indexes.push(
                IndexRef::new(name.clone(), index.name().to_owned())
                    .map_err(|e| DbError::Query(e.to_string()))?,
            );
        }
        let outgoing = foreign_keys.clone();
        let table = TableInfo::new(
            name.clone(),
            kind,
            columns,
            constraints,
            foreign_keys,
            Vec::new(),
            indexes,
            fk_truncated,
            idx_truncated,
        )
        .map_err(|e| DbError::Query(e.to_string()))?;
        built.push((table, outgoing, name));
    }
    schema_indexes.sort_by(|a, b| (a.table(), a.index()).cmp(&(b.table(), b.index())));
    schema_indexes.truncate(SCHEMA_SCHEMA_INDEX_CAP);
    let mut incoming: std::collections::HashMap<String, Vec<ForeignKeyInfo>> =
        std::collections::HashMap::new();
    for (_, outgoing, child_name) in &built {
        for fk in outgoing {
            let info = ForeignKeyInfo::new(
                fk.ref_columns().to_vec(),
                child_name.clone(),
                fk.columns().to_vec(),
                fk.constraint_name().map(|s| s.to_owned()),
            )
            .map_err(|e| DbError::Query(e.to_string()))?;
            incoming.entry(fk.ref_table().to_owned()).or_default().push(info);
        }
    }
    let mut tables_out: Vec<TableInfo> = Vec::new();
    let mut views_out: Vec<TableInfo> = Vec::new();
    for (table, _, name) in built {
        let refs = incoming.remove(&name).unwrap_or_default();
        let rebuilt = TableInfo::new(
            table.name().to_owned(),
            table.kind(),
            table.columns().to_vec(),
            table.constraints().to_vec(),
            table.foreign_keys().to_vec(),
            refs,
            table.indexes().to_vec(),
            table.foreign_keys_truncated(),
            table.indexes_truncated(),
        )
        .map_err(|e| DbError::Query(e.to_string()))?;
        match rebuilt.kind() {
            TableKind::Table => tables_out.push(rebuilt),
            TableKind::View => views_out.push(rebuilt),
        }
    }
    let mut trigger_stmt = conn
        .prepare(
            "SELECT name, tbl_name, sql FROM sqlite_master \
             WHERE type = 'trigger' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .map_err(|e| DbError::Query(e.to_string()))?;
    let trigger_rows: Vec<(String, String, Option<String>)> = trigger_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|e| DbError::Query(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| DbError::Query(e.to_string()))?;
    let triggers_truncated = trigger_rows.len() > SCHEMA_TRIGGER_CAP;
    let mut triggers = Vec::new();
    for (name, table, definition) in trigger_rows.into_iter().take(SCHEMA_TRIGGER_CAP) {
        triggers.push(
            TriggerInfo::new(
                name,
                Some(table).filter(|t| !t.trim().is_empty()),
                None,
                definition.filter(|d| !d.trim().is_empty()),
            )
            .map_err(|e| DbError::Query(e.to_string()))?,
        );
    }
    let db_name = sqlite_database_name(conn);
    let schema_node = SchemaNode::new(
        "main".to_owned(),
        tables_out,
        views_out,
        Vec::new(),
        schema_indexes,
        triggers,
        Vec::new(),
        triggers_truncated,
        false,
    )
    .map_err(|e| DbError::Query(e.to_string()))?;
    let db_node =
        DatabaseNode::new(db_name, vec![schema_node]).map_err(|e| DbError::Query(e.to_string()))?;
    Ok(SchemaInfo::new(vec![db_node], tables_truncated, columns_truncated))
}

/// Spawns the connection's worker thread and returns its handle.
/// `password` and `ssh_password` are memory-only and never persisted;
/// SQLite ignores both. `tx_idle` is the idle schedule for open manual
/// transactions; tests pass short spans.
pub fn spawn_session(
    conn: ConnectionId,
    config: ConnectionConfig,
    password: Option<String>,
    ssh_password: Option<String>,
    ui: eframe::egui::Context,
    events: mpsc::Sender<DbEvent>,
    tx_idle: TxIdle,
) -> DbHandle {
    let (cmds_tx, cmds_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name(format!("cde-conn-{}", conn.raw()))
        .spawn(move || {
            worker_loop(conn, config, password, ssh_password, ui, events, cmds_rx, tx_idle)
        })
        .expect("spawn db worker thread");
    DbHandle { cmds: cmds_tx }
}

fn worker_loop(
    conn: ConnectionId,
    config: ConnectionConfig,
    password: Option<String>,
    ssh_password: Option<String>,
    ui: eframe::egui::Context,
    events: mpsc::Sender<DbEvent>,
    cmds: mpsc::Receiver<DbCommand>,
    tx_idle: TxIdle,
) {
    let mut session = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Session::open(&config, password.as_deref(), ssh_password.as_deref())
    })) {
        Ok(Ok(s)) => s,
        Ok(Err(error)) => {
            emit(&events, &ui, DbEvent::ConnectFailed { conn, error });
            return;
        }
        Err(_) => {
            emit(
                &events,
                &ui,
                DbEvent::ConnectFailed {
                    conn,
                    error: DbError::Open("driver panicked while opening".to_owned()),
                },
            );
            return;
        }
    };
    emit(
        &events,
        &ui,
        DbEvent::Connected {
            conn,
            cancel: session.cancel_handle(),
            server_version: session.server_version(),
        },
    );

    loop {
        // An open manual transaction needs the worker to wake at its idle
        // deadlines; with nothing open, block until the next command.
        let cmd = match session.tx_idle_wait(&tx_idle) {
            Some(wait) => match cmds.recv_timeout(wait) {
                Ok(cmd) => cmd,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    match session.tick_tx_idle(&tx_idle) {
                        Some(IdleTick::Warn) => {
                            emit(&events, &ui, DbEvent::TxIdleWarning { conn });
                        }
                        Some(IdleTick::Rollback) => {
                            emit(&events, &ui, DbEvent::TxIdleRollback { conn });
                        }
                        None => {}
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match cmds.recv() {
                Ok(cmd) => cmd,
                Err(_) => break,
            },
        };
        match cmd {
            DbCommand::QueryBatch { run, statements } => {
                let was_open = session.manual_tx_open();
                let event = match run_statements(&mut session, &statements) {
                    Ok(results) => DbEvent::BatchOk { run, results },
                    Err((index, error)) => DbEvent::QueryErr {
                        run,
                        error: statement_error(index, error),
                    },
                };
                if session.manual_tx_open() != was_open {
                    emit(
                        &events,
                        &ui,
                        DbEvent::TxState { conn, open: session.manual_tx_open() },
                    );
                }
                if events.send(event).is_err() {
                    break;
                }
                ui.request_repaint();
            }
            DbCommand::LoadSchema => {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    session.schema()
                }));
                let event = match outcome {
                    Ok(Ok(schema)) => DbEvent::SchemaOk { conn, schema },
                    Ok(Err(error)) => DbEvent::SchemaErr { conn, error },
                    Err(_) => DbEvent::SchemaErr {
                        conn,
                        error: DbError::Query("driver panicked while loading schema".to_owned()),
                    },
                };
                emit(&events, &ui, event);
            }
            DbCommand::RowCount { schema, table } => {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    session.row_count(&schema, &table)
                }));
                let event = match outcome {
                    Ok(Ok(count)) => DbEvent::RowCountOk { conn, schema, table, count },
                    Ok(Err(error)) => DbEvent::RowCountErr { conn, schema, table, error },
                    Err(_) => DbEvent::RowCountErr {
                        conn,
                        schema,
                        table,
                        error: DbError::Query("driver panicked while counting rows".to_owned()),
                    },
                };
                emit(&events, &ui, event);
            }
            DbCommand::ListSessions => {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    session.sessions()
                }));
                let event = match outcome {
                    Ok(Ok(sessions)) => DbEvent::SessionsOk { conn, sessions },
                    Ok(Err(error)) => DbEvent::SessionsErr { conn, error },
                    Err(_) => DbEvent::SessionsErr {
                        conn,
                        error: DbError::Query("driver panicked while listing sessions".to_owned()),
                    },
                };
                emit(&events, &ui, event);
            }
            DbCommand::TerminateSession { backend } => {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    session.terminate_session(&backend)
                }));
                let event = match outcome {
                    Ok(Ok(())) => DbEvent::TerminateOk { conn, backend },
                    Ok(Err(error)) => DbEvent::TerminateErr { conn, backend, error },
                    Err(_) => DbEvent::TerminateErr {
                        conn,
                        backend,
                        error: DbError::Query(
                            "driver panicked while terminating the session".to_owned(),
                        ),
                    },
                };
                emit(&events, &ui, event);
            }
            DbCommand::SetAutocommit { on } => {
                let was_open = session.manual_tx_open();
                let applied = session.set_autocommit(on);
                if session.manual_tx_open() != was_open {
                    emit(
                        &events,
                        &ui,
                        DbEvent::TxState { conn, open: session.manual_tx_open() },
                    );
                }
                emit(
                    &events,
                    &ui,
                    DbEvent::AutocommitSet {
                        conn,
                        on: session.autocommit(),
                        tx_open: session.manual_tx_open(),
                        error: applied.err(),
                    },
                );
            }
            DbCommand::QueryPage { run, tab, sql, offset, limit } => {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    session.query_page(&sql, offset, limit)
                }));
                let event = match outcome {
                    Ok(Ok(result)) => DbEvent::PageOk { run, tab, offset, result },
                    Ok(Err(error)) => DbEvent::PageErr { run, tab, error },
                    Err(_) => DbEvent::PageErr {
                        run,
                        tab,
                        error: DbError::Query("driver panicked while paging".to_owned()),
                    },
                };
                emit(&events, &ui, event);
            }
            DbCommand::CountFor { run, tab, sql } => {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    session.count_for(&sql)
                }));
                let event = match outcome {
                    Ok(Ok(total)) => DbEvent::CountOk { run, tab, total },
                    Ok(Err(error)) => DbEvent::CountErr { run, tab, error },
                    Err(_) => DbEvent::CountErr {
                        run,
                        tab,
                        error: DbError::Query("driver panicked while counting".to_owned()),
                    },
                };
                emit(&events, &ui, event);
            }
        }
    }
}

/// Runs statements in order, one engine call each, and stops at the first
/// failure. The error carries the zero-based index of the failing statement.
fn run_statements(
    session: &mut Session,
    statements: &[String],
) -> Result<Vec<QueryResult>, (usize, DbError)> {
    let mut results = Vec::with_capacity(statements.len());
    for (index, sql) in statements.iter().enumerate() {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.query(sql)));
        match outcome {
            Ok(Ok(result)) => results.push(result),
            Ok(Err(error)) => return Err((index, error)),
            Err(_) => {
                return Err((index, DbError::Query("driver panicked while running".to_owned())));
            }
        }
    }
    Ok(results)
}

/// Names the failing statement in a batch error. Only engine failures gain the
/// prefix; fatal errors stay as they are.
fn statement_error(index: usize, error: DbError) -> DbError {
    match error {
        DbError::Query(message) => DbError::Query(format!("statement {index}: {message}")),
        other => other,
    }
}

fn emit(events: &mpsc::Sender<DbEvent>, ui: &eframe::egui::Context, event: DbEvent) {
    if events.send(event).is_ok() {
        ui.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ragged_columns_rejected() {
        let cols = vec![
            Column::new("a".into(), "".into(), vec![Value::Integer(1)]),
            Column::new("b".into(), "".into(), vec![]),
        ];
        assert!(QueryResult::from_columns(cols, false, None, Duration::ZERO).is_err());
    }

    #[test]
    fn paged_sql_wraps_with_limit_and_offset() {
        assert_eq!(
            paged_sql("SELECT id FROM users ORDER BY id", DbKind::Postgres, 5000, 10000),
            "SELECT * FROM (SELECT id FROM users ORDER BY id) AS __cde_page LIMIT 5000 OFFSET 10000"
        );
    }

    #[test]
    fn paged_sql_strips_trailing_semicolons_and_whitespace() {
        assert_eq!(
            paged_sql("SELECT 1;  \n;", DbKind::Postgres, 10, 0),
            "SELECT * FROM (SELECT 1) AS __cde_page LIMIT 10 OFFSET 0"
        );
    }

    #[test]
    fn count_sql_wraps_the_statement() {
        assert_eq!(
            count_sql("SELECT id FROM users WHERE id > 3;", DbKind::Postgres),
            "SELECT COUNT(*) FROM (SELECT id FROM users WHERE id > 3) AS __cde_count"
        );
    }

    #[test]
    fn single_statement_rejects_batches() {
        assert!(is_single_statement("SELECT * FROM users", DbKind::Postgres));
        assert!(is_single_statement("SELECT 1;  ", DbKind::Postgres));
        assert!(!is_single_statement("SELECT 1; SELECT 2", DbKind::Postgres));
    }

    #[test]
    fn paged_sql_strips_trailing_comments_and_terminators() {
        assert_eq!(
            paged_sql("SELECT 1 -- trailing note", DbKind::Postgres, 10, 0),
            "SELECT * FROM (SELECT 1) AS __cde_page LIMIT 10 OFFSET 0"
        );
        assert_eq!(
            paged_sql("SELECT 1 # trailing note", DbKind::Mysql, 10, 0),
            "SELECT * FROM (SELECT 1) AS __cde_page LIMIT 10 OFFSET 0"
        );
        assert_eq!(
            paged_sql("SELECT 1 /* trailing */ ;", DbKind::Postgres, 10, 0),
            "SELECT * FROM (SELECT 1) AS __cde_page LIMIT 10 OFFSET 0"
        );
        assert_eq!(
            paged_sql("-- lead\nSELECT 1\n-- note", DbKind::Postgres, 10, 0),
            "SELECT * FROM (SELECT 1) AS __cde_page LIMIT 10 OFFSET 0"
        );
        assert_eq!(
            paged_sql("SELECT $$a;b$$", DbKind::Postgres, 10, 0),
            "SELECT * FROM (SELECT $$a;b$$) AS __cde_page LIMIT 10 OFFSET 0"
        );
        // A comment with code after it stays: its newline keeps the wrapper's
        // closing paren outside the comment.
        assert_eq!(
            paged_sql("SELECT 1 -- inline\n + 2", DbKind::Postgres, 10, 0),
            "SELECT * FROM (SELECT 1 -- inline\n + 2) AS __cde_page LIMIT 10 OFFSET 0"
        );
    }

    #[test]
    fn count_sql_strips_trailing_comments_and_terminators() {
        assert_eq!(
            count_sql("SELECT 1 -- trailing note", DbKind::Postgres),
            "SELECT COUNT(*) FROM (SELECT 1) AS __cde_count"
        );
        assert_eq!(
            count_sql("SELECT 1 # trailing note", DbKind::Mysql),
            "SELECT COUNT(*) FROM (SELECT 1) AS __cde_count"
        );
    }

    #[test]
    fn paged_and_count_sql_stay_valid_sqlite() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let page = paged_sql("SELECT 1 -- trailing note", DbKind::Sqlite, 10, 0);
        assert_eq!(conn.query_row(&page, [], |row| row.get::<_, i64>(0)).unwrap(), 1);
        let count = count_sql("SELECT 1 -- trailing note", DbKind::Sqlite);
        assert_eq!(conn.query_row(&count, [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    }

    #[test]
    fn hash_reads_as_an_operator_outside_mysql() {
        assert_eq!(
            paged_sql("SELECT data #>> '{a}' AS v FROM t", DbKind::Postgres, 10, 0),
            "SELECT * FROM (SELECT data #>> '{a}' AS v FROM t) AS __cde_page LIMIT 10 OFFSET 0"
        );
        assert_eq!(
            count_sql("SELECT 5 # 3", DbKind::Sqlite),
            "SELECT COUNT(*) FROM (SELECT 5 # 3) AS __cde_count"
        );
        assert!(!is_single_statement("SELECT 5 # 3; DELETE FROM t", DbKind::Postgres));
        assert!(is_single_statement("SELECT 5 # 3; DELETE FROM t", DbKind::Mysql));
    }

    #[test]
    fn single_statement_ignores_quoted_and_commented_terminators() {
        assert!(is_single_statement("SELECT ';'", DbKind::Postgres));
        assert!(is_single_statement("SELECT 1; -- note", DbKind::Postgres));
        assert!(is_single_statement("SELECT 1 # note", DbKind::Mysql));
        assert!(is_single_statement("SELECT $$a;b$$", DbKind::Postgres));
        assert!(is_single_statement("SELECT \"a;b\"", DbKind::Postgres));
        assert!(!is_single_statement("SELECT 1; SELECT 2", DbKind::Postgres));
        assert!(!is_single_statement("SELECT 1; -- note\nSELECT 2", DbKind::Postgres));
        assert!(!is_single_statement("SELECT 1 # note\n; SELECT 2", DbKind::Mysql));
    }

    fn two_rows(first: Vec<Value>, second: Vec<Value>) -> QueryResult {
        QueryResult::from_columns(
            vec![
                Column::new("a".to_owned(), "INT".to_owned(), first),
                Column::new("b".to_owned(), "TEXT".to_owned(), second),
            ],
            false,
            None,
            Duration::ZERO,
        )
        .unwrap()
    }

    #[test]
    fn append_page_extends_rows_and_takes_the_flag() {
        let mut loaded = two_rows(vec![Value::Integer(1)], vec![Value::Text("a".to_owned())]);
        let mut next = two_rows(vec![Value::Integer(2)], vec![Value::Text("b".to_owned())]);
        next.truncated = true;
        assert!(loaded.append_page(next).unwrap());
        assert_eq!(loaded.row_count(), 2);
        assert!(loaded.is_truncated());
        assert_eq!(loaded.cell(1, 0), Some(&Value::Integer(2)));
    }

    #[test]
    fn append_page_rejects_mismatched_columns() {
        let mut loaded = two_rows(vec![Value::Integer(1)], vec![Value::Text("a".to_owned())]);
        let other = QueryResult::from_columns(
            vec![Column::new("a".to_owned(), "INT".to_owned(), vec![Value::Integer(2)])],
            false,
            None,
            Duration::ZERO,
        )
        .unwrap();
        assert!(loaded.append_page(other).is_err());
    }

    #[test]
    fn total_defaults_to_none_and_sets() {
        let mut result =
            two_rows(vec![Value::Integer(1)], vec![Value::Text("a".to_owned())]);
        assert_eq!(result.total(), None);
        result.set_total(42);
        assert_eq!(result.total(), Some(42));
    }

    #[test]
    fn stream_pages_feeds_chunks_in_order() {
        let mut session = Session::Sqlite(rusqlite::Connection::open_in_memory().unwrap());
        session.query("CREATE TABLE cde_pages (id INTEGER)").unwrap();
        session.query("INSERT INTO cde_pages VALUES (1), (2), (3), (4), (5)").unwrap();
        let cancel = AtomicBool::new(false);
        let mut seen: Vec<(u64, Vec<i64>)> = Vec::new();
        let rows = stream_pages(
            &mut session,
            "SELECT id FROM cde_pages ORDER BY id",
            2,
            &cancel,
            |page, offset| {
                let ids: Vec<i64> = (0..page.row_count())
                    .map(|r| match page.cell(r, 0) {
                        Some(Value::Integer(n)) => *n,
                        other => panic!("expected integer, got {other:?}"),
                    })
                    .collect();
                seen.push((offset, ids));
            },
        )
        .unwrap();
        assert_eq!(rows, 5);
        assert_eq!(
            seen,
            vec![
                (0, vec![1, 2]),
                (2, vec![3, 4]),
                (4, vec![5]),
            ]
        );
    }

    #[test]
    fn stream_pages_counts_exactly_through_the_wrapper() {
        let mut session = Session::Sqlite(rusqlite::Connection::open_in_memory().unwrap());
        session.query("CREATE TABLE cde_count (id INTEGER)").unwrap();
        session.query("INSERT INTO cde_count VALUES (1), (2), (3)").unwrap();
        assert_eq!(session.count_for("SELECT id FROM cde_count WHERE id > 1").unwrap(), 2);
    }

    #[test]
    fn stream_pages_stops_when_cancelled() {
        let mut session = Session::Sqlite(rusqlite::Connection::open_in_memory().unwrap());
        session.query("CREATE TABLE cde_cancel (id INTEGER)").unwrap();
        session.query("INSERT INTO cde_cancel VALUES (1), (2), (3), (4)").unwrap();
        let cancel = AtomicBool::new(false);
        let mut pages = 0;
        let outcome = stream_pages(
            &mut session,
            "SELECT id FROM cde_cancel ORDER BY id",
            2,
            &cancel,
            |_, _| {
                pages += 1;
                cancel.store(true, Ordering::Relaxed);
            },
        );
        assert!(matches!(outcome, Err(DbError::Query(message)) if message == "cancelled"));
        assert_eq!(pages, 1);
    }

    #[test]
    fn query_page_marks_a_full_page_as_more() {
        let mut session = Session::Sqlite(rusqlite::Connection::open_in_memory().unwrap());
        session.query("CREATE TABLE cde_flag (id INTEGER)").unwrap();
        session.query("INSERT INTO cde_flag VALUES (1), (2), (3)").unwrap();
        let full = session.query_page("SELECT id FROM cde_flag ORDER BY id", 0, 2).unwrap();
        assert_eq!(full.row_count(), 2);
        assert!(full.is_truncated());
        let last = session.query_page("SELECT id FROM cde_flag ORDER BY id", 2, 2).unwrap();
        assert_eq!(last.row_count(), 1);
        assert!(!last.is_truncated());
    }

    #[test]
    fn query_errors_are_not_fatal() {
        assert!(!DbError::Query("x".into()).is_fatal());
        assert!(DbError::Open("x".into()).is_fatal());
        assert!(DbError::Gone.is_fatal());
    }

    #[test]
    fn ids_are_unique_and_resumable() {
        let mut ids = Ids::new();
        let a = ids.connection();
        let b = ids.connection();
        assert_ne!(a, b);
        let _ = (ids.tab(), ids.run());
        ids.resume_after(&[ConnectionId::from_raw(100)]);
        assert!(ids.connection().raw() > 100);
    }

    #[test]
    fn postgres_url_without_password_omits_secret() {
        assert_eq!(
            postgres_url("localhost", 5432, "appdb", "owner", None, PostgresSslMode::Disable),
            "postgres://owner@localhost:5432/appdb"
        );
    }

    #[test]
    fn postgres_url_empty_password_omits_secret() {
        assert_eq!(
            postgres_url(
                "localhost",
                5432,
                "appdb",
                "owner",
                Some(""),
                PostgresSslMode::Disable
            ),
            "postgres://owner@localhost:5432/appdb"
        );
    }

    #[test]
    fn postgres_url_with_password_includes_secret() {
        assert_eq!(
            postgres_url(
                "db.internal",
                5433,
                "analytics",
                "reader",
                Some("s3cret"),
                PostgresSslMode::Disable
            ),
            "postgres://reader:s3cret@db.internal:5433/analytics"
        );
    }

    #[test]
    fn postgres_url_encodes_special_chars_in_user_and_password() {
        assert_eq!(
            postgres_url(
                "localhost",
                5432,
                "appdb",
                "ad min",
                Some("p@ss:w/ord?"),
                PostgresSslMode::Disable
            ),
            "postgres://ad%20min:p%40ss%3Aw%2Ford%3F@localhost:5432/appdb"
        );
        assert_eq!(
            postgres_url("h", 1, "d", "u", Some("a#b%c!"), PostgresSslMode::Disable),
            "postgres://u:a%23b%25c%21@h:1/d"
        );
    }

    #[test]
    fn postgres_url_disable_keeps_old_string() {
        assert_eq!(
            postgres_url(
                "localhost",
                5432,
                "appdb",
                "owner",
                None,
                PostgresSslMode::Disable
            ),
            "postgres://owner@localhost:5432/appdb"
        );
        assert_eq!(
            postgres_url(
                "db.internal",
                5433,
                "analytics",
                "reader",
                Some("s3cret"),
                PostgresSslMode::Disable
            ),
            "postgres://reader:s3cret@db.internal:5433/analytics"
        );
    }

    #[test]
    fn postgres_url_appends_sslmode_per_variant() {
        for (mode, wire) in [
            (PostgresSslMode::Prefer, "prefer"),
            (PostgresSslMode::Require, "require"),
            (PostgresSslMode::VerifyCa, "verify-ca"),
            (PostgresSslMode::VerifyFull, "verify-full"),
        ] {
            assert_eq!(
                postgres_url("localhost", 5432, "appdb", "owner", None, mode),
                format!("postgres://owner@localhost:5432/appdb?sslmode={wire}"),
            );
            assert_eq!(
                postgres_url(
                    "db.internal",
                    5433,
                    "analytics",
                    "reader",
                    Some("s3cret"),
                    mode
                ),
                format!(
                    "postgres://reader:s3cret@db.internal:5433/analytics?sslmode={wire}"
                ),
            );
        }
    }

    #[test]
    fn postgres_ssl_mode_roundtrips_through_json() {
        for mode in PostgresSslMode::ALL {
            let config = ConnectionConfig::Postgres { read_only: false,
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: mode,
                ssh: None,
            };
            let json = serde_json::to_string(&config).unwrap();
            assert!(json.contains(mode.as_str()));
            let back: ConnectionConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(back, config);
        }
    }

    #[test]
    fn postgres_ssl_mode_missing_from_old_profiles_defaults_to_disable() {
        let old = serde_json::json!({
            "kind": "postgres",
            "host": "localhost",
            "port": 5432,
            "database": "appdb",
            "user": "owner",
        });
        let back: ConnectionConfig = serde_json::from_value(old).unwrap();
        assert_eq!(
            back,
            ConnectionConfig::Postgres { read_only: false,
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
                ssh: None,
            }
        );
        assert_eq!(PostgresSslMode::default(), PostgresSslMode::Disable);
    }

    #[test]
    fn mysql_ssl_mode_roundtrips_through_json() {
        for (mode, json_name) in [
            (MysqlSslMode::Disabled, "disabled"),
            (MysqlSslMode::Preferred, "preferred"),
            (MysqlSslMode::Required, "required"),
            (MysqlSslMode::VerifyCa, "verify_ca"),
            (MysqlSslMode::VerifyIdentity, "verify_identity"),
        ] {
            let config = ConnectionConfig::Mysql { read_only: false,
                host: "localhost".to_owned(),
                port: 3306,
                database: "cde_test".to_owned(),
                user: "root".to_owned(),
                ssl_mode: mode,
                ssh: None,
            };
            let json = serde_json::to_string(&config).unwrap();
            assert!(json.contains(&format!("\"ssl_mode\":\"{json_name}\"")), "{json}");
            let back: ConnectionConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(back, config);
        }
    }

    #[test]
    fn mysql_ssl_mode_missing_from_old_profiles_defaults_to_preferred() {
        let old = serde_json::json!({
            "kind": "mysql",
            "host": "localhost",
            "port": 3306,
            "database": "cde_test",
            "user": "root",
        });
        let back: ConnectionConfig = serde_json::from_value(old).unwrap();
        assert_eq!(
            back,
            ConnectionConfig::Mysql { read_only: false,
                host: "localhost".to_owned(),
                port: 3306,
                database: "cde_test".to_owned(),
                user: "root".to_owned(),
                ssl_mode: MysqlSslMode::Preferred,
                ssh: None,
            }
        );
        assert_eq!(MysqlSslMode::default(), MysqlSslMode::Preferred);
    }

    /// An ssh config shared by the tunnel tests.
    fn ssh_config() -> crate::ssh::SshConfig {
        crate::ssh::SshConfig {
            host: "jump.example".to_owned(),
            port: 2222,
            user: "deploy".to_owned(),
            auth: crate::ssh::SshAuth::Password,
        }
    }

    #[test]
    fn config_serde_roundtrips_with_and_without_ssh() {
        for ssh in [None, Some(ssh_config())] {
            let postgres = ConnectionConfig::Postgres { read_only: false,
                host: "db.internal".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
                ssh: ssh.clone(),
            };
            let json = serde_json::to_string(&postgres).unwrap();
            assert_eq!(serde_json::from_str::<ConnectionConfig>(&json).unwrap(), postgres);
            let mysql = ConnectionConfig::Mysql { read_only: false,
                host: "db.internal".to_owned(),
                port: 3307,
                database: "cde_test".to_owned(),
                user: "root".to_owned(),
                ssl_mode: MysqlSslMode::default(),
                ssh,
            };
            let json = serde_json::to_string(&mysql).unwrap();
            assert_eq!(serde_json::from_str::<ConnectionConfig>(&json).unwrap(), mysql);
        }
    }

    #[test]
    fn profiles_without_ssh_load_as_none_and_serialize_without_the_field() {
        let legacy = serde_json::json!({
            "kind": "postgres",
            "host": "localhost",
            "port": 5432,
            "database": "appdb",
            "user": "owner",
        });
        let back: ConnectionConfig = serde_json::from_value(legacy).unwrap();
        let ConnectionConfig::Postgres { ssh, .. } = &back else {
            panic!("expected a postgres profile");
        };
        assert!(ssh.is_none());
        let json = serde_json::to_string(&back).unwrap();
        assert!(!json.contains("ssh"), "{json}");
    }

    #[test]
    fn ssh_tunnel_pool_url_points_at_the_forwarded_port() {
        let (host, port) = crate::ssh::loopback_endpoint(41_234);
        assert_eq!((host.as_str(), port), ("127.0.0.1", 41_234));
        assert_eq!(
            postgres_url(&host, port, "appdb", "owner", Some("s3cret"), PostgresSslMode::Disable),
            "postgres://owner:s3cret@127.0.0.1:41234/appdb"
        );
        assert_eq!(
            mysql_url(&host, port, "cde_test", "root", None, MysqlSslMode::Preferred),
            "mysql://root@127.0.0.1:41234/cde_test"
        );
    }

    #[test]
    fn validate_checks_ssh_fields_when_present() {
        let config = |ssh: crate::ssh::SshConfig| ConnectionConfig::Postgres { read_only: false,
            host: "db.internal".to_owned(),
            port: 5432,
            database: "appdb".to_owned(),
            user: "owner".to_owned(),
            ssl_mode: PostgresSslMode::Disable,
            ssh: Some(ssh),
        };
        let mut ssh = ssh_config();
        assert!(config(ssh.clone()).validate().is_ok());
        ssh.host = "  ".to_owned();
        assert_eq!(
            config(ssh.clone()).validate(),
            Err(DbError::Config("ssh host is empty".to_owned()))
        );
        ssh.host = "jump.example".to_owned();
        ssh.port = 0;
        assert_eq!(
            config(ssh.clone()).validate(),
            Err(DbError::Config("ssh port is not set".to_owned()))
        );
        ssh.port = 22;
        ssh.user = String::new();
        assert_eq!(
            config(ssh.clone()).validate(),
            Err(DbError::Config("ssh user is empty".to_owned()))
        );
        ssh.user = "deploy".to_owned();
        ssh.auth = crate::ssh::SshAuth::KeyFile { path: PathBuf::new(), passphrase: None };
        assert_eq!(
            config(ssh).validate(),
            Err(DbError::Config("ssh key file is empty".to_owned()))
        );
    }

    /// The real path: a Postgres profile with ssh configured through
    /// `Session::open`, a SELECT over the forwarded port, then teardown.
    #[cfg(all(feature = "ssh", feature = "postgres"))]
    #[test]
    #[ignore = "needs cde-ssh and cde-postgres; set SSH_HOST/SSH_PORT/SSH_USER/SSH_PASSWORD and PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_ssh_session_roundtrip() {
        let ssh_password = std::env::var("SSH_PASSWORD").ok();
        let config = ConnectionConfig::Postgres { read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "127.0.0.1".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|port| port.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: Some(crate::ssh::SshConfig {
                host: std::env::var("SSH_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned()),
                port: std::env::var("SSH_PORT")
                    .ok()
                    .and_then(|port| port.parse().ok())
                    .unwrap_or(22),
                user: std::env::var("SSH_USER").unwrap_or_else(|_| "root".to_owned()),
                auth: crate::ssh::SshAuth::Password,
            }),
        };
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), ssh_password.as_deref())
            .expect("open through the tunnel");
        let result = session.query("SELECT 42 AS answer").expect("select through the tunnel");
        assert_eq!(result.cell(0, 0), Some(&Value::Integer(42)));
        // The session owns the tunnel; dropping it must not hang.
        let started = Instant::now();
        drop(session);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "teardown took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn mysql_kind_defaults_and_registry() {
        assert_eq!(DbKind::Mysql.label(), "MySQL");
        assert_eq!(DbKind::Mysql.default_port(), Some(3306));
        assert_eq!(
            DbKind::Mysql.default_config(),
            ConnectionConfig::Mysql { read_only: false,
                host: "localhost".to_owned(),
                port: 3306,
                database: String::new(),
                user: String::new(),
                ssl_mode: MysqlSslMode::default(),
                ssh: None,
            }
        );
        assert_eq!(DbKind::Mysql.default_config().kind(), DbKind::Mysql);
        let kinds: Vec<DbKind> = DriverRegistry::bundled().all().collect();
        assert_eq!(kinds, vec![DbKind::Sqlite, DbKind::Postgres, DbKind::Mysql]);
    }

    #[test]
    fn mysql_config_roundtrips_through_json() {
        let config = ConnectionConfig::Mysql { read_only: false,
            host: "db.internal".to_owned(),
            port: 3307,
            database: "cde_test".to_owned(),
            user: "root".to_owned(),
            ssl_mode: MysqlSslMode::Preferred,
            ssh: None,
        };
        assert_eq!(
            serde_json::to_string(&config).unwrap(),
            r#"{"kind":"mysql","host":"db.internal","port":3307,"database":"cde_test","user":"root","ssl_mode":"preferred","read_only":false}"#
        );
        let back: ConnectionConfig = serde_json::from_str(&serde_json::to_string(&config).unwrap())
            .unwrap();
        assert_eq!(back, config);
        assert!(back.prompts_for_password());
        assert!(!ConnectionConfig::Sqlite { read_only: false, path: ":memory:".into() }.prompts_for_password());
    }

    #[test]
    fn mysql_config_validate_matches_postgres_rules() {
        let bad = |host: &str, port: u16, database: &str, user: &str| ConnectionConfig::Mysql { read_only: false,
            host: host.to_owned(),
            port,
            database: database.to_owned(),
            user: user.to_owned(),
            ssl_mode: MysqlSslMode::default(),
            ssh: None,
        };
        assert!(
            bad("localhost", 3306, "cde_test", "root").validate().is_ok()
        );
        assert_eq!(
            bad(" ", 3306, "cde_test", "root").validate(),
            Err(DbError::Config("host is empty".to_owned()))
        );
        assert_eq!(
            bad("localhost", 0, "cde_test", "root").validate(),
            Err(DbError::Config("port is not set".to_owned()))
        );
        assert_eq!(
            bad("localhost", 3306, " ", "root").validate(),
            Err(DbError::Config("database is empty".to_owned()))
        );
        assert_eq!(
            bad("localhost", 3306, "cde_test", " ").validate(),
            Err(DbError::Config("user is empty".to_owned()))
        );
    }

    #[test]
    fn validate_accepts_every_mysql_ssl_mode() {
        for mode in MysqlSslMode::ALL {
            let config = ConnectionConfig::Mysql { read_only: false,
                host: "localhost".to_owned(),
                port: 3307,
                database: "cde_test".to_owned(),
                user: "root".to_owned(),
                ssl_mode: mode,
                ssh: None,
            };
            assert_eq!(config.validate(), Ok(()), "{mode:?}");
        }
    }

    #[test]
    fn mysql_url_without_password_omits_secret() {
        assert_eq!(
            mysql_url("localhost", 3306, "cde_test", "root", None, MysqlSslMode::Preferred),
            "mysql://root@localhost:3306/cde_test"
        );
        assert_eq!(
            mysql_url("localhost", 3306, "cde_test", "root", Some(""), MysqlSslMode::Preferred),
            "mysql://root@localhost:3306/cde_test"
        );
    }

    #[test]
    fn mysql_url_with_password_includes_secret() {
        assert_eq!(
            mysql_url("db.internal", 3307, "cde_test", "root", Some("cdepass"), MysqlSslMode::Preferred),
            "mysql://root:cdepass@db.internal:3307/cde_test"
        );
    }

    #[test]
    fn mysql_url_encodes_special_chars_in_user_and_password() {
        assert_eq!(
            mysql_url("localhost", 3306, "cde test", "ad min", Some("p@ss:w/ord?"), MysqlSslMode::Preferred),
            "mysql://ad%20min:p%40ss%3Aw%2Ford%3F@localhost:3306/cde%20test"
        );
    }

    #[test]
    fn mysql_url_preferred_default_keeps_old_string() {
        assert_eq!(
            mysql_url("localhost", 3306, "cde_test", "root", None, MysqlSslMode::Preferred),
            "mysql://root@localhost:3306/cde_test"
        );
        assert_eq!(
            mysql_url(
                "db.internal",
                3307,
                "cde_test",
                "root",
                Some("cdepass"),
                MysqlSslMode::Preferred,
            ),
            "mysql://root:cdepass@db.internal:3307/cde_test"
        );
    }

    #[test]
    fn mysql_url_appends_ssl_mode_per_variant() {
        for (mode, wire) in [
            (MysqlSslMode::Disabled, "disabled"),
            (MysqlSslMode::Required, "required"),
            (MysqlSslMode::VerifyCa, "verify_ca"),
            (MysqlSslMode::VerifyIdentity, "verify_identity"),
        ] {
            assert_eq!(
                mysql_url("localhost", 3306, "cde_test", "root", None, mode),
                format!("mysql://root@localhost:3306/cde_test?ssl-mode={wire}"),
            );
            assert_eq!(
                mysql_url("db.internal", 3307, "cde_test", "root", Some("cdepass"), mode),
                format!("mysql://root:cdepass@db.internal:3307/cde_test?ssl-mode={wire}"),
            );
        }
    }

    /// Pins every value to sqlx's own MySQL parser.
    #[cfg(feature = "mysql")]
    #[test]
    fn mysql_ssl_mode_wire_spellings_parse_as_sqlx_options() {
        for mode in MysqlSslMode::ALL {
            let url =
                format!("mysql://root@localhost:3306/cde_test?ssl-mode={}", mode.as_str());
            let options: sqlx::mysql::MySqlConnectOptions = url.parse().unwrap();
            assert_eq!(format!("{:?}", options.get_ssl_mode()), format!("{mode:?}"), "{url}");
        }
        for url in [
            "mysql://root@localhost:3306/cde_test?ssl-mode=prefer",
            "mysql://root@localhost:3306/cde_test?ssl-mode=verify-ca",
            "mysql://root@localhost:3306/cde_test?ssl-mode=verify-full",
        ] {
            assert!(url.parse::<sqlx::mysql::MySqlConnectOptions>().is_err(), "{url}");
        }
    }

    #[test]
    fn parse_postgres_url_covers_user_password_port_database_and_sslmode() {
        let parsed =
            parse_connection_url("postgres://owner:s3cret@db.internal:5433/analytics?sslmode=require")
                .unwrap();
        assert_eq!(parsed.kind, DbKind::Postgres);
        assert_eq!(
            parsed.config,
            ConnectionConfig::Postgres { read_only: false,
                host: "db.internal".to_owned(),
                port: 5433,
                database: "analytics".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Require,
                ssh: None,
            }
        );
        assert_eq!(parsed.password, Some("s3cret".to_owned()));
    }

    #[test]
    fn parse_postgresql_scheme_alias_parses_like_postgres() {
        let parsed = parse_connection_url("postgresql://owner@localhost:5432/appdb").unwrap();
        assert_eq!(parsed.kind, DbKind::Postgres);
        assert_eq!(
            parsed.config,
            ConnectionConfig::Postgres { read_only: false,
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
                ssh: None,
            }
        );
        assert_eq!(parsed.password, None);
    }

    #[test]
    fn parse_postgres_url_decodes_user_password_and_database() {
        let parsed = parse_connection_url(
            "postgres://ad%20min:p%40ss%3Aw%2Ford%3F@localhost:5432/cde%20test",
        )
        .unwrap();
        assert_eq!(parsed.kind, DbKind::Postgres);
        let ConnectionConfig::Postgres { host, port, database, user, ssl_mode, ssh, .. } =
            parsed.config
        else {
            panic!("expected a postgres config");
        };
        assert_eq!(user, "ad min");
        assert_eq!(database, "cde test");
        assert_eq!(
            (host.as_str(), port, ssl_mode, ssh),
            ("localhost", 5432, PostgresSslMode::Disable, None)
        );
        assert_eq!(parsed.password, Some("p@ss:w/ord?".to_owned()));
    }

    #[test]
    fn parse_postgres_url_defaults_a_missing_port() {
        let parsed = parse_connection_url("postgres://owner@localhost/appdb").unwrap();
        let ConnectionConfig::Postgres { port, .. } = parsed.config else {
            panic!("expected a postgres config");
        };
        assert_eq!(port, 5432);
    }

    #[test]
    fn parse_postgres_url_maps_every_sslmode() {
        for (wire, mode) in [
            ("disable", PostgresSslMode::Disable),
            ("prefer", PostgresSslMode::Prefer),
            ("require", PostgresSslMode::Require),
            ("verify-ca", PostgresSslMode::VerifyCa),
            ("verify-full", PostgresSslMode::VerifyFull),
        ] {
            let url = format!("postgres://owner@localhost:5432/appdb?sslmode={wire}");
            let parsed = parse_connection_url(&url).unwrap();
            let ConnectionConfig::Postgres { ssl_mode, .. } = parsed.config else {
                panic!("expected a postgres config");
            };
            assert_eq!(ssl_mode, mode, "{url}");
        }
    }

    #[test]
    fn parse_postgres_url_ignores_unknown_query_parameters() {
        let parsed = parse_connection_url(
            "postgres://owner@localhost:5432/appdb?connect_timeout=10&sslmode=prefer",
        )
        .unwrap();
        let ConnectionConfig::Postgres { ssl_mode, user, .. } = parsed.config else {
            panic!("expected a postgres config");
        };
        assert_eq!(ssl_mode, PostgresSslMode::Prefer);
        assert_eq!(user, "owner");
    }

    #[test]
    fn parse_mysql_url_covers_user_password_port_and_database() {
        let parsed =
            parse_connection_url("mysql://root:cdepass@db.internal:3307/cde_test").unwrap();
        assert_eq!(parsed.kind, DbKind::Mysql);
        assert_eq!(
            parsed.config,
            ConnectionConfig::Mysql { read_only: false,
                host: "db.internal".to_owned(),
                port: 3307,
                database: "cde_test".to_owned(),
                user: "root".to_owned(),
                ssl_mode: MysqlSslMode::default(),
                ssh: None,
            }
        );
        assert_eq!(parsed.password, Some("cdepass".to_owned()));
    }

    #[test]
    fn parse_mariadb_scheme_alias_parses_like_mysql() {
        let parsed = parse_connection_url("mariadb://root@localhost:3306/cde_test").unwrap();
        assert_eq!(parsed.kind, DbKind::Mysql);
        assert_eq!(
            parsed.config,
            ConnectionConfig::Mysql { read_only: false,
                host: "localhost".to_owned(),
                port: 3306,
                database: "cde_test".to_owned(),
                user: "root".to_owned(),
                ssl_mode: MysqlSslMode::default(),
                ssh: None,
            }
        );
        assert_eq!(parsed.password, None);
    }

    #[test]
    fn parse_mysql_url_decodes_parts_defaults_port_and_ignores_query() {
        let parsed = parse_connection_url(
            "mysql://ad%20min:p%40ss%3Aw%2Ford%3F@localhost/cde%20test?charset=utf8mb4",
        )
        .unwrap();
        assert_eq!(parsed.kind, DbKind::Mysql);
        assert_eq!(
            parsed.config,
            ConnectionConfig::Mysql { read_only: false,
                host: "localhost".to_owned(),
                port: 3306,
                database: "cde test".to_owned(),
                user: "ad min".to_owned(),
                ssl_mode: MysqlSslMode::default(),
                ssh: None,
            }
        );
        assert_eq!(parsed.password, Some("p@ss:w/ord?".to_owned()));
    }

    #[test]
    fn parse_sqlite_url_and_bare_paths_share_one_shape() {
        for raw in [
            "sqlite:///tmp/app.db",
            "sqlite:/tmp/app.db",
            "sqlite:tmp/app.db",
            "/tmp/app.db",
            "tmp/app.db",
            ":memory:",
        ] {
            let parsed = parse_connection_url(raw).unwrap();
            assert_eq!(parsed.kind, DbKind::Sqlite, "{raw}");
            assert_eq!(parsed.password, None, "{raw}");
        }
        assert_eq!(
            parse_connection_url("sqlite:///tmp/app.db").unwrap().config,
            ConnectionConfig::Sqlite { read_only: false, path: PathBuf::from("/tmp/app.db") }
        );
        assert_eq!(
            parse_connection_url("/tmp/app.db").unwrap().config,
            ConnectionConfig::Sqlite { read_only: false, path: PathBuf::from("/tmp/app.db") }
        );
        assert_eq!(
            parse_connection_url(":memory:").unwrap().config,
            ConnectionConfig::Sqlite { read_only: false, path: PathBuf::from(":memory:") }
        );
    }

    #[test]
    fn parse_rejects_a_bad_scheme() {
        assert_eq!(
            parse_connection_url("redis://localhost:6379/0"),
            Err(DbError::Config("invalid connection URL".to_owned()))
        );
    }

    #[test]
    fn parse_rejects_a_malformed_host() {
        assert_eq!(
            parse_connection_url("postgres://owner:secret@/appdb"),
            Err(DbError::Config("invalid connection URL".to_owned()))
        );
    }

    #[test]
    fn parse_rejects_a_percent_decoding_error() {
        assert_eq!(
            parse_connection_url("postgres://%ZZ@localhost:5432/appdb"),
            Err(DbError::Config("invalid connection URL".to_owned()))
        );
    }

    #[test]
    fn postgres_url_roundtrips_through_parse() {
        let config = ConnectionConfig::Postgres { read_only: false,
            host: "db.internal".to_owned(),
            port: 5433,
            database: "analytics".to_owned(),
            user: "reader".to_owned(),
            ssl_mode: PostgresSslMode::Require,
            ssh: None,
        };
        let url = postgres_url(
            "db.internal",
            5433,
            "analytics",
            "reader",
            Some("s3cret"),
            PostgresSslMode::Require,
        );
        let parsed = parse_connection_url(&url).unwrap();
        assert_eq!(parsed.kind, DbKind::Postgres);
        assert_eq!(parsed.config, config);
        assert_eq!(parsed.password, Some("s3cret".to_owned()));
        let encoded = postgres_url(
            "localhost",
            5432,
            "cde test",
            "ad min",
            Some("p@ss:w/ord?"),
            PostgresSslMode::VerifyFull,
        );
        let parsed = parse_connection_url(&encoded).unwrap();
        let ConnectionConfig::Postgres { host, port, database, user, ssl_mode, ssh, .. } =
            parsed.config
        else {
            panic!("expected a postgres config");
        };
        assert_eq!(host, "localhost");
        assert_eq!(port, 5432);
        assert_eq!(database, "cde test");
        assert_eq!(user, "ad min");
        assert_eq!(ssl_mode, PostgresSslMode::VerifyFull);
        assert_eq!(ssh, None);
        assert_eq!(parsed.password, Some("p@ss:w/ord?".to_owned()));
        let bare =
            postgres_url("localhost", 5432, "appdb", "owner", None, PostgresSslMode::Disable);
        let parsed = parse_connection_url(&bare).unwrap();
        assert_eq!(parsed.password, None);
        assert_eq!(
            parsed.config,
            ConnectionConfig::Postgres { read_only: false,
                host: "localhost".to_owned(),
                port: 5432,
                database: "appdb".to_owned(),
                user: "owner".to_owned(),
                ssl_mode: PostgresSslMode::Disable,
                ssh: None,
            }
        );
    }

    #[test]
    fn mysql_url_roundtrips_through_parse() {
        let config = ConnectionConfig::Mysql { read_only: false,
            host: "db.internal".to_owned(),
            port: 3307,
            database: "cde_test".to_owned(),
            user: "root".to_owned(),
            ssl_mode: MysqlSslMode::Preferred,
            ssh: None,
        };
        let url = mysql_url(
            "db.internal",
            3307,
            "cde_test",
            "root",
            Some("cdepass"),
            MysqlSslMode::Preferred,
        );
        let parsed = parse_connection_url(&url).unwrap();
        assert_eq!(parsed.kind, DbKind::Mysql);
        assert_eq!(parsed.config, config);
        assert_eq!(parsed.password, Some("cdepass".to_owned()));
        let encoded = mysql_url(
            "localhost",
            3306,
            "cde test",
            "ad min",
            Some("p@ss:w/ord?"),
            MysqlSslMode::Preferred,
        );
        let parsed = parse_connection_url(&encoded).unwrap();
        assert_eq!(
            parsed.config,
            ConnectionConfig::Mysql { read_only: false,
                host: "localhost".to_owned(),
                port: 3306,
                database: "cde test".to_owned(),
                user: "ad min".to_owned(),
                ssl_mode: MysqlSslMode::default(),
                ssh: None,
            }
        );
        assert_eq!(parsed.password, Some("p@ss:w/ord?".to_owned()));
    }

    #[cfg(not(feature = "mysql"))]
    #[test]
    fn mysql_feature_off_reports_config_error() {
        let config = ConnectionConfig::Mysql { read_only: false,
            host: "localhost".to_owned(),
            port: 3306,
            database: "cde_test".to_owned(),
            user: "root".to_owned(),
            ssl_mode: MysqlSslMode::default(),
            ssh: None,
        };
        match Session::open(&config, Some("cdepass"), None) {
            Err(error) => {
                assert_eq!(error, DbError::Config("mysql feature not enabled".to_owned()));
            }
            Ok(_) => panic!("expected the feature-off stub to refuse"),
        }
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn pg_cell_kind_covers_listed_type_names() {
        assert_eq!(pg_cell_kind("BOOL"), PgCellKind::Bool);
        assert_eq!(pg_cell_kind("INT2"), PgCellKind::Int16);
        assert_eq!(pg_cell_kind("INT4"), PgCellKind::Int32);
        assert_eq!(pg_cell_kind("INT8"), PgCellKind::Int64);
        assert_eq!(pg_cell_kind("OID"), PgCellKind::Oid);
        assert_eq!(pg_cell_kind("FLOAT4"), PgCellKind::Float32);
        assert_eq!(pg_cell_kind("FLOAT8"), PgCellKind::Float64);
        assert_eq!(pg_cell_kind("BYTEA"), PgCellKind::Blob);
        for name in [
            "NUMERIC",
            "TEXT",
            "VARCHAR",
            "CHAR",
            "\"CHAR\"",
            "NAME",
            "UNKNOWN",
            "UUID",
            "JSON",
            "JSONB",
            "DATE",
            "TIME",
            "TIMETZ",
            "TIMESTAMP",
            "TIMESTAMPTZ",
            "INTERVAL",
            "VOID",
            "INT4[]",
            "TEXT[]",
            "some_custom_type",
        ] {
            assert_eq!(pg_cell_kind(name), PgCellKind::Text, "{name}");
        }
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn pg_trigger_kind_covers_timing_and_events() {
        assert_eq!(pg_trigger_kind(7), "BEFORE INSERT");
        assert_eq!(pg_trigger_kind(17), "AFTER UPDATE");
        assert_eq!(pg_trigger_kind(22), "BEFORE INSERT UPDATE");
        assert_eq!(pg_trigger_kind(9), "AFTER DELETE");
        assert_eq!(pg_trigger_kind(80), "INSTEAD OF UPDATE");
        assert_eq!(pg_trigger_kind(32), "AFTER TRUNCATE");
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn pg_cancel_handle_double_fire_is_safe_without_server() {
        let unknown =
            pg_cancel_handle("postgres://owner@localhost:5432/appdb".to_owned(), -1);
        unknown.cancel();
        unknown.cancel();
        let bad_url = pg_cancel_handle("not-a-url".to_owned(), 424_242);
        bad_url.cancel();
        bad_url.cancel();
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_cancel_frees_sleeping_worker() {
        let config = ConnectionConfig::Postgres { read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        };
        let password = std::env::var("PGPASSWORD").ok();
        let session = Session::open(&config, password.as_deref(), None).expect("open");
        match &session {
            Session::Postgres(pg) => assert!(pg.backend_pid > 0),
            Session::Sqlite(_) => panic!("expected a Postgres session"),
            #[cfg(feature = "mysql")]
            Session::Mysql(_) => panic!("expected a Postgres session"),
        }
        let cancel = session.cancel_handle();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut session = session;
            let outcome = session.query("SELECT pg_sleep(30)");
            let _ = tx.send(outcome);
        });
        std::thread::sleep(Duration::from_millis(500));
        cancel.cancel();
        cancel.cancel();
        let outcome = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("worker returns within 10s after cancel");
        assert!(outcome.is_err());
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_roundtrip_covers_result_and_affected_paths() {
        let config = ConnectionConfig::Postgres { read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        };
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        assert!(!session.server_version().is_empty());
        session
            .query(
                "CREATE TEMPORARY TABLE cde_probe (id INT4, name TEXT, active BOOL, ratio FLOAT8, payload BYTEA, note TEXT)",
            )
            .expect("create");
        let inserted = session
            .query("INSERT INTO cde_probe VALUES (1, 'ada', TRUE, 1.5, '\\x0102', NULL)")
            .expect("insert");
        assert_eq!(inserted.affected(), Some(1));
        let result = session
            .query("SELECT id, name, active, ratio, payload, note FROM cde_probe ORDER BY id")
            .expect("select");
        assert_eq!(result.row_count(), 1);
        assert_eq!(result.affected(), None);
        assert_eq!(result.columns()[0].type_name(), "INT4");
        assert_eq!(result.columns()[2].type_name(), "BOOL");
        assert_eq!(result.cell(0, 0), Some(&Value::Integer(1)));
        assert_eq!(result.cell(0, 1), Some(&Value::Text("ada".to_owned())));
        assert_eq!(result.cell(0, 2), Some(&Value::Integer(1)));
        assert_eq!(result.cell(0, 3), Some(&Value::Real(1.5)));
        assert_eq!(result.cell(0, 4), Some(&Value::Blob(vec![0x01, 0x02])));
        assert_eq!(result.cell(0, 5), Some(&Value::Null));
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_batch_runs_in_order_and_names_the_failing_statement() {
        let config = ConnectionConfig::Postgres { read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        };
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        let statements: Vec<String> = vec![
            "CREATE TEMPORARY TABLE cde_batch (id INT4)".to_owned(),
            "INSERT INTO cde_batch VALUES (1)".to_owned(),
            "SELECT id FROM cde_batch".to_owned(),
        ];
        let results = run_statements(&mut session, &statements).expect("batch runs");
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].affected(), Some(0));
        assert_eq!(results[1].affected(), Some(1));
        assert_eq!(results[2].row_count(), 1);
        assert_eq!(results[2].cell(0, 0), Some(&Value::Integer(1)));

        let failing: Vec<String> = vec![
            "SELECT 1".to_owned(),
            "SELECT * FROM cde_missing_table".to_owned(),
            "SELECT 3".to_owned(),
        ];
        let (index, error) = run_statements(&mut session, &failing).expect_err("stops");
        assert_eq!(index, 1);
        let named = statement_error(index, error).to_string();
        assert!(named.contains("statement 1"), "{named}");
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_cancel_stops_a_running_batch() {
        let config = ConnectionConfig::Postgres { read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        };
        let password = std::env::var("PGPASSWORD").ok();
        let session = Session::open(&config, password.as_deref(), None).expect("open");
        let cancel = session.cancel_handle();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut session = session;
            let statements: Vec<String> = vec![
                "SELECT 1".to_owned(),
                "SELECT pg_sleep(30)".to_owned(),
                "SELECT 3".to_owned(),
            ];
            let _ = tx.send(run_statements(&mut session, &statements));
        });
        std::thread::sleep(Duration::from_millis(500));
        cancel.cancel();
        cancel.cancel();
        let outcome = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("batch returns within 10s after cancel");
        let (index, error) = outcome.expect_err("cancelled batch fails");
        assert_eq!(index, 1);
        assert!(matches!(error, DbError::Query(_)));
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_explain_returns_rows() {
        let config = ConnectionConfig::Postgres { read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        };
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        let plan = explain_sql(DbKind::Postgres, "SELECT 1");
        let result = session.query(&plan).expect("explain runs");
        assert!(result.row_count() > 0);
        let page = session
            .query_page(r#"SELECT '{"a":"x"}'::jsonb #>> '{a}' AS v"#, 0, 10)
            .expect("a page keeps the #>> operator");
        assert_eq!(page.cell(0, 0), Some(&Value::Text("x".to_owned())));
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_edit_statements_update_a_temp_table() {
        let config = ConnectionConfig::Postgres { read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        };
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        session
            .query("CREATE TEMPORARY TABLE cde_edit (id INT4 PRIMARY KEY, name TEXT)")
            .expect("create");
        session
            .query("INSERT INTO cde_edit (id, name) VALUES (1, 'ada'), (2, 'grace')")
            .expect("seed");
        let updated = session
            .query(&crate::edit::update_statement(
                "cde_edit",
                &[("id".to_owned(), "1".to_owned())],
                &[("name".to_owned(), "Ada".to_owned())],
            ))
            .expect("update");
        assert_eq!(updated.affected(), Some(1));
        let deleted = session
            .query(&crate::edit::delete_statement(
                "cde_edit",
                &[("id".to_owned(), "2".to_owned())],
            ))
            .expect("delete");
        assert_eq!(deleted.affected(), Some(1));
        let inserted = session
            .query(&crate::edit::insert_statement(
                "cde_edit",
                &["id".to_owned(), "name".to_owned()],
                &["3".to_owned(), "Lin".to_owned()],
            ))
            .expect("insert");
        assert_eq!(inserted.affected(), Some(1));
        let result = session
            .query("SELECT id, name FROM cde_edit ORDER BY id")
            .expect("reselect");
        assert_eq!(result.row_count(), 2);
        assert_eq!(result.cell(0, 0), Some(&Value::Integer(1)));
        assert_eq!(result.cell(0, 1), Some(&Value::Text("Ada".to_owned())));
        assert_eq!(result.cell(1, 0), Some(&Value::Integer(3)));
        assert_eq!(result.cell(1, 1), Some(&Value::Text("Lin".to_owned())));
    }

    #[cfg(feature = "postgres")]
    static LIVE_SCHEMA_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]

    fn pg_live_schema_lists_triggers_and_routines() {
        let _guard = LIVE_SCHEMA_LOCK.lock().expect("schema live lock");
        let config = ConnectionConfig::Postgres { read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        };
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        session
            .query("DROP TABLE IF EXISTS cde_schema_probe CASCADE")
            .expect("drop table");
        session
            .query("DROP FUNCTION IF EXISTS cde_schema_trg_fn()")
            .expect("drop trigger function");
        session
            .query("DROP FUNCTION IF EXISTS cde_schema_fn(INT)")
            .expect("drop function");
        session
            .query("CREATE TABLE cde_schema_probe (id INT PRIMARY KEY, note TEXT)")
            .expect("create table");
        session
            .query(
                "CREATE FUNCTION cde_schema_trg_fn() RETURNS trigger LANGUAGE plpgsql \
                 AS $$ BEGIN RETURN NEW; END $$",
            )
            .expect("create trigger function");
        session
            .query(
                "CREATE TRIGGER cde_schema_trg BEFORE INSERT ON cde_schema_probe \
                 FOR EACH ROW EXECUTE FUNCTION cde_schema_trg_fn()",
            )
            .expect("create trigger");
        session
            .query(
                "CREATE FUNCTION cde_schema_fn(a INT) RETURNS INT LANGUAGE sql \
                 AS $$ SELECT a + 1 $$",
            )
            .expect("create function");
        let schema = session.schema().expect("schema loads");
        session
            .query("DROP TABLE IF EXISTS cde_schema_probe CASCADE")
            .expect("cleanup table");
        session
            .query("DROP FUNCTION IF EXISTS cde_schema_trg_fn()")
            .expect("cleanup trigger function");
        session
            .query("DROP FUNCTION IF EXISTS cde_schema_fn(INT)")
            .expect("cleanup function");
        let node = schema
            .databases()
            .iter()
            .flat_map(|db| db.schemas())
            .find(|s| s.triggers().iter().any(|t| t.name() == "cde_schema_trg"))
            .expect("schema with the probe trigger");
        let trigger = node
            .triggers()
            .iter()
            .find(|t| t.name() == "cde_schema_trg")
            .expect("probe trigger");
        assert_eq!(trigger.table(), Some("cde_schema_probe"));
        assert_eq!(trigger.kind(), Some("BEFORE INSERT"));
        assert!(
            trigger.definition().is_some_and(|d| d.contains("cde_schema_trg_fn")),
            "{:?}",
            trigger.definition()
        );
        let routine = node
            .routines()
            .iter()
            .find(|r| r.name() == "cde_schema_fn")
            .expect("probe routine");
        assert_eq!(routine.kind(), Some("FUNCTION"));
        assert_eq!(routine.language(), Some("sql"));
        assert!(
            routine.definition().is_some_and(|d| d.contains("a + 1")),
            "{:?}",
            routine.definition()
        );
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_schema_caps_routines_and_flags_truncation() {
        let _guard = LIVE_SCHEMA_LOCK.lock().expect("schema live lock");
        let config = ConnectionConfig::Postgres { read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        };
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        session
            .query("DROP SCHEMA IF EXISTS cde_routine_cap CASCADE")
            .expect("drop schema");
        session.query("CREATE SCHEMA cde_routine_cap").expect("create schema");
        let over_cap = SCHEMA_ROUTINE_CAP + 1;
        let seed = format!(
            "DO $$ BEGIN \
             FOR i IN 1..{over_cap} LOOP \
             EXECUTE format('CREATE FUNCTION cde_routine_cap.f_%s() RETURNS int \
             LANGUAGE sql AS ''SELECT %s''', i, i); \
             END LOOP; \
             END $$"
        );
        session.query(&seed).expect("seed routines");
        let schema = session.schema().expect("schema loads");
        session
            .query("DROP SCHEMA IF EXISTS cde_routine_cap CASCADE")
            .expect("cleanup schema");
        let node = schema
            .databases()
            .iter()
            .flat_map(|db| db.schemas())
            .find(|s| s.name() == "cde_routine_cap")
            .expect("cap schema");
        assert_eq!(node.routines().len(), SCHEMA_ROUTINE_CAP);
        assert!(node.routines_truncated());
        assert!(!node.triggers_truncated());
    }

    /// The schema and table name the navigator would pass for one probe
    /// table, read back from a loaded tree.
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    fn probe_location(schema: &SchemaInfo, name: &str) -> (String, String) {
        schema
            .databases()
            .iter()
            .flat_map(|db| db.schemas())
            .find_map(|s| {
                s.tables()
                    .iter()
                    .find(|t| t.name() == name)
                    .map(|t| (s.name().to_owned(), t.name().to_owned()))
            })
            .expect("probe table in the schema")
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_row_count_estimates_an_analyzed_table() {
        let config = ConnectionConfig::Postgres { read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        };
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        session
            .query("DROP TABLE IF EXISTS cde_count_probe")
            .expect("drop probe");
        session
            .query("CREATE TABLE cde_count_probe (id INT)")
            .expect("create probe");
        session
            .query("INSERT INTO cde_count_probe (id) VALUES (1), (2), (3), (4), (5)")
            .expect("seed probe");
        session.query("ANALYZE cde_count_probe").expect("analyze probe");
        let schema = session.schema().expect("schema loads");
        let (schema_name, table) = probe_location(&schema, "cde_count_probe");
        let count = session.row_count(&schema_name, &table).expect("count");
        session
            .query("DROP TABLE IF EXISTS cde_count_probe")
            .expect("cleanup probe");
        assert_eq!(count, RowCount::Estimate(5));
        assert!(matches!(session.row_count(&schema_name, &table), Err(DbError::Query(_))));
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn my_cell_kind_covers_listed_type_names() {
        for name in ["BOOLEAN", "TINYINT", "SMALLINT", "INT", "MEDIUMINT", "BIGINT"] {
            assert_eq!(my_cell_kind(name), MyCellKind::Signed, "{name}");
        }
        for name in [
            "TINYINT UNSIGNED",
            "SMALLINT UNSIGNED",
            "INT UNSIGNED",
            "MEDIUMINT UNSIGNED",
            "BIGINT UNSIGNED",
        ] {
            assert_eq!(my_cell_kind(name), MyCellKind::Unsigned, "{name}");
        }
        for name in ["FLOAT", "DOUBLE"] {
            assert_eq!(my_cell_kind(name), MyCellKind::Float, "{name}");
        }
        assert_eq!(my_cell_kind("YEAR"), MyCellKind::Year);
        assert_eq!(my_cell_kind("DATE"), MyCellKind::Date);
        assert_eq!(my_cell_kind("TIME"), MyCellKind::Time);
        for name in ["DATETIME", "TIMESTAMP"] {
            assert_eq!(my_cell_kind(name), MyCellKind::DateTime, "{name}");
        }
        for name in [
            "BINARY",
            "VARBINARY",
            "BLOB",
            "TINYBLOB",
            "MEDIUMBLOB",
            "LONGBLOB",
            "BIT",
            "GEOMETRY",
        ] {
            assert_eq!(my_cell_kind(name), MyCellKind::Blob, "{name}");
        }
        for name in [
            "DECIMAL",
            "JSON",
            "VARCHAR",
            "CHAR",
            "TEXT",
            "ENUM",
            "SET",
            "NULL",
            "some_custom_type",
        ] {
            assert_eq!(my_cell_kind(name), MyCellKind::Text, "{name}");
        }
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn my_temporal_text_restores_server_text() {
        // Byte layouts captured from a live MySQL 8.4 server.
        assert_eq!(my_temporal_text("DATE", &[4, 234, 7, 10, 13]), "2026-10-13");
        assert_eq!(my_temporal_text("DATE", &[0]), "0000-00-00");
        assert_eq!(
            my_temporal_text("TIME", &[8, 0, 0, 0, 0, 0, 12, 34, 56]),
            "12:34:56"
        );
        assert_eq!(my_temporal_text("TIME", &[0]), "00:00:00");
        assert_eq!(
            my_temporal_text("TIME", &[12, 1, 0, 0, 0, 0, 12, 34, 56, 64, 226, 1, 0]),
            "-12:34:56.123456"
        );
        assert_eq!(
            my_temporal_text("TIME", &[8, 0, 34, 0, 0, 0, 22, 59, 59]),
            "838:59:59"
        );
        assert_eq!(
            my_temporal_text("TIME", &[8, 1, 34, 0, 0, 0, 22, 59, 59]),
            "-838:59:59"
        );
        assert_eq!(
            my_temporal_text("DATETIME", &[7, 234, 7, 10, 13, 12, 34, 56]),
            "2026-10-13 12:34:56"
        );
        assert_eq!(
            my_temporal_text(
                "DATETIME",
                &[11, 234, 7, 10, 13, 12, 34, 56, 64, 226, 1, 0]
            ),
            "2026-10-13 12:34:56.123456"
        );
        assert_eq!(
            my_temporal_text("TIMESTAMP", &[4, 234, 7, 10, 13]),
            "2026-10-13 00:00:00"
        );
        assert_eq!(my_temporal_text("DATETIME", &[0]), "0000-00-00 00:00:00");
        assert_eq!(my_temporal_text("DATE", b"2026-10-13"), "2026-10-13");
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn my_cancel_handle_double_fire_is_safe_without_server() {
        let unknown = mysql_cancel_handle("mysql://root@localhost:3307/cde_test".to_owned(), 0);
        unknown.cancel();
        unknown.cancel();
        let bad_url = mysql_cancel_handle("not-a-url".to_owned(), 424_242);
        bad_url.cancel();
        bad_url.cancel();
    }

    #[cfg(feature = "mysql")]
    fn mysql_live_config() -> ConnectionConfig {
        ConnectionConfig::Mysql { read_only: false,
            host: std::env::var("MYSQLHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("MYSQLPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(3306),
            database: std::env::var("MYSQLDATABASE").unwrap_or_else(|_| "cde_test".to_owned()),
            user: std::env::var("MYSQLUSER").unwrap_or_else(|_| "root".to_owned()),
            ssl_mode: MysqlSslMode::default(),
            ssh: None,
        }
    }

    #[cfg(feature = "mysql")]
    fn mysql_live_open() -> Session {
        let config = mysql_live_config();
        let password = std::env::var("MYSQLPASSWORD").ok();
        Session::open(&config, password.as_deref(), None).expect("open MySQL")
    }

    /// Runs DDL that the prepared-statement protocol refuses, such as
    /// CREATE TRIGGER and DROP FUNCTION, over the simple protocol.
    #[cfg(feature = "mysql")]
    fn mysql_live_raw(session: &Session, sql: &str) {
        let Session::Mysql(my) = session else {
            panic!("expected a mysql session");
        };
        my.rt
            .block_on(sqlx::raw_sql(sql).execute(&my.pool))
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_roundtrip_covers_result_and_affected_paths() {
        let mut session = mysql_live_open();
        assert!(!session.server_version().is_empty());
        session
            .query(
                "CREATE TEMPORARY TABLE cde_probe (\
                 id INT, name VARCHAR(64), active TINYINT(1), ratio DOUBLE, \
                 amount DECIMAL(10,2), born DATE, at DATETIME, clock TIME, \
                 delta TIME, payload BLOB, big BIGINT UNSIGNED, note VARCHAR(64))",
            )
            .expect("create");
        let inserted = session
            .query(
                "INSERT INTO cde_probe VALUES (1, 'ada', TRUE, 1.5, 1.5, \
                 '2026-10-13', '2026-10-13 12:34:56', '12:34:56', '-12:34:56', \
                 X'0102', 18446744073709551615, NULL)",
            )
            .expect("insert");
        assert_eq!(inserted.affected(), Some(1));
        let result = session
            .query(
                "SELECT id, name, active, ratio, amount, born, at, clock, delta, \
                 payload, big, note FROM cde_probe ORDER BY id",
            )
            .expect("select");
        assert_eq!(result.row_count(), 1);
        assert_eq!(result.affected(), None);
        assert_eq!(result.columns()[0].type_name(), "INT");
        assert_eq!(result.columns()[2].type_name(), "BOOLEAN");
        assert_eq!(result.columns()[5].type_name(), "DATE");
        assert_eq!(result.columns()[7].type_name(), "TIME");
        assert_eq!(result.columns()[10].type_name(), "BIGINT UNSIGNED");
        assert_eq!(result.cell(0, 0), Some(&Value::Integer(1)));
        assert_eq!(result.cell(0, 1), Some(&Value::Text("ada".to_owned())));
        assert_eq!(result.cell(0, 2), Some(&Value::Integer(1)));
        assert_eq!(result.cell(0, 3), Some(&Value::Real(1.5)));
        assert_eq!(result.cell(0, 4), Some(&Value::Text("1.50".to_owned())));
        assert_eq!(result.cell(0, 5), Some(&Value::Text("2026-10-13".to_owned())));
        assert_eq!(
            result.cell(0, 6),
            Some(&Value::Text("2026-10-13 12:34:56".to_owned()))
        );
        assert_eq!(result.cell(0, 7), Some(&Value::Text("12:34:56".to_owned())));
        assert_eq!(result.cell(0, 8), Some(&Value::Text("-12:34:56".to_owned())));
        assert_eq!(result.cell(0, 9), Some(&Value::Blob(vec![0x01, 0x02])));
        assert_eq!(
            result.cell(0, 10),
            Some(&Value::Text("18446744073709551615".to_owned()))
        );
        assert_eq!(result.cell(0, 11), Some(&Value::Null));
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_schema_introspection_returns_non_empty() {
        let config = mysql_live_config();
        let ConnectionConfig::Mysql { database, .. } = &config else {
            panic!("expected a mysql config");
        };
        let mut session = mysql_live_open();
        // MySQL rejects trigger and routine DDL on the prepared-statement
        // protocol the query path uses, so the scratch objects run on the
        // simple protocol. Dropping the table drops its trigger.
        mysql_live_raw(&session, "DROP FUNCTION IF EXISTS cde_schema_fn");
        session.query("DROP VIEW IF EXISTS cde_schema_view").expect("drop view");
        session.query("DROP TABLE IF EXISTS cde_schema_probe").expect("drop table");
        session
            .query(
                "CREATE TABLE cde_schema_probe (\
                 id INT NOT NULL, name VARCHAR(64), amount DECIMAL(10,2), \
                 PRIMARY KEY (id), UNIQUE KEY u_cde_name (name), \
                 KEY i_cde_amount (amount))",
            )
            .expect("create table");
        session
            .query("CREATE VIEW cde_schema_view AS SELECT id FROM cde_schema_probe")
            .expect("create view");
        mysql_live_raw(
            &session,
            "CREATE TRIGGER cde_schema_trg BEFORE INSERT ON cde_schema_probe \
             FOR EACH ROW SET NEW.amount = NEW.amount",
        );
        mysql_live_raw(
            &session,
            "CREATE FUNCTION cde_schema_fn(a INT) RETURNS INT DETERMINISTIC \
             RETURN a + 1",
        );
        let schema = session.schema();
        session.query("DROP VIEW IF EXISTS cde_schema_view").expect("cleanup view");
        session.query("DROP TABLE IF EXISTS cde_schema_probe").expect("cleanup table");
        mysql_live_raw(&session, "DROP FUNCTION IF EXISTS cde_schema_fn");
        let schema = schema.expect("schema loads");
        assert!(!schema.is_empty());
        let node = schema
            .databases()
            .iter()
            .find(|d| d.name() == database.as_str())
            .expect("connected database node");
        assert_eq!(node.schemas().len(), 1);
        let main = &node.schemas()[0];
        assert_eq!(main.name(), database.as_str());
        let table = main
            .tables()
            .iter()
            .find(|t| t.name() == "cde_schema_probe")
            .expect("probe table");
        let col_names: Vec<&str> = table.columns().iter().map(|c| c.name()).collect();
        assert_eq!(col_names, vec!["id", "name", "amount"]);
        let id = &table.columns()[0];
        assert_eq!(id.pk_position(), Some(1));
        assert!(!id.nullable());
        assert_eq!(id.type_name(), "int");
        let name_idx = table
            .indexes()
            .iter()
            .find(|i| i.name() == "u_cde_name")
            .expect("unique index");
        assert!(name_idx.unique());
        assert_eq!(name_idx.columns(), &["name".to_owned()]);
        let plain_idx = table
            .indexes()
            .iter()
            .find(|i| i.name() == "i_cde_amount")
            .expect("plain index");
        assert!(!plain_idx.unique());
        let pk: Vec<&ConstraintInfo> = table
            .constraints()
            .iter()
            .filter(|c| c.kind() == ConstraintKind::Pk)
            .collect();
        assert_eq!(pk.len(), 1);
        assert_eq!(pk[0].name(), "PRIMARY");
        assert_eq!(pk[0].columns(), &["id".to_owned()]);
        let view_names: Vec<&str> = main.views().iter().map(|t| t.name()).collect();
        assert!(view_names.contains(&"cde_schema_view"), "{view_names:?}");
        assert!(!main.triggers_truncated());
        let trigger = main
            .triggers()
            .iter()
            .find(|t| t.name() == "cde_schema_trg")
            .expect("probe trigger");
        assert_eq!(trigger.table(), Some("cde_schema_probe"));
        assert_eq!(trigger.kind(), Some("BEFORE INSERT"));
        assert!(
            trigger.definition().is_some_and(|d| d.contains("SET NEW.amount")),
            "{:?}",
            trigger.definition()
        );
        assert!(!main.routines_truncated());
        let routine = main
            .routines()
            .iter()
            .find(|r| r.name() == "cde_schema_fn")
            .expect("probe routine");
        assert_eq!(routine.kind(), Some("FUNCTION"));
        assert_eq!(routine.language(), Some("SQL"));
        assert!(
            routine.definition().is_some_and(|d| d.contains("a + 1")),
            "{:?}",
            routine.definition()
        );
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_row_count_estimates_an_analyzed_table() {
        let mut session = mysql_live_open();
        session.query("DROP TABLE IF EXISTS cde_count_probe").expect("drop probe");
        session
            .query("CREATE TABLE cde_count_probe (id INT)")
            .expect("create probe");
        session
            .query("INSERT INTO cde_count_probe (id) VALUES (1), (2), (3), (4), (5)")
            .expect("seed probe");
        session.query("ANALYZE TABLE cde_count_probe").expect("analyze probe");
        let schema = session.schema().expect("schema loads");
        let (schema_name, table) = probe_location(&schema, "cde_count_probe");
        let count = session.row_count(&schema_name, &table).expect("count");
        session.query("DROP TABLE IF EXISTS cde_count_probe").expect("cleanup probe");
        assert_eq!(count, RowCount::Estimate(5));
        assert!(matches!(session.row_count(&schema_name, &table), Err(DbError::Query(_))));
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_cancel_stops_a_running_batch() {
        let session = mysql_live_open();
        let cancel = session.cancel_handle();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut session = session;
            // SLEEP() returns 1 when killed instead of erroring, so the
            // interruptible statement is a cross join that never finishes.
            let statements: Vec<String> = vec![
                "SELECT 1".to_owned(),
                "SELECT COUNT(*) FROM information_schema.columns a \
                 CROSS JOIN information_schema.columns b \
                 CROSS JOIN information_schema.columns c"
                    .to_owned(),
                "SELECT 3".to_owned(),
            ];
            let _ = tx.send(run_statements(&mut session, &statements));
        });
        std::thread::sleep(Duration::from_millis(500));
        cancel.cancel();
        cancel.cancel();
        let outcome = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("batch returns within 10s after cancel");
        let (index, error) = outcome.expect_err("cancelled batch fails");
        assert_eq!(index, 1);
        assert!(matches!(error, DbError::Query(_)));
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_preferred_connects_and_required_reports_missing_tls() {
        let password = std::env::var("MYSQLPASSWORD").ok();
        let mut config = mysql_live_config();
        let session =
            Session::open(&config, password.as_deref(), None).expect("default mode opens");
        assert!(!session.server_version().is_empty());
        drop(session);
        if let ConnectionConfig::Mysql { ssl_mode, .. } = &mut config {
            *ssl_mode = MysqlSslMode::Disabled;
        }
        let session =
            Session::open(&config, password.as_deref(), None).expect("disabled mode opens");
        assert!(!session.server_version().is_empty());
        drop(session);
        if let ConnectionConfig::Mysql { ssl_mode, .. } = &mut config {
            *ssl_mode = MysqlSslMode::Required;
        }
        let error = match Session::open(&config, password.as_deref(), None) {
            Ok(_) => panic!("required cannot negotiate TLS without a TLS backend in this build"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("TLS"), "{error}");
    }

    #[test]
    fn schema_constructors_reject_empty_names() {
        assert!(ColumnInfo::new(String::new(), "TEXT".into(), true, None).is_err());
        assert!(ColumnInfo::new("  ".into(), "TEXT".into(), true, None).is_err());
        assert!(
            TableInfo::new(
                String::new(),
                TableKind::Table,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                false,
                false
            )
            .is_err()
        );
        assert!(
            TableInfo::new(
                " ".into(),
                TableKind::View,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                false,
                false
            )
            .is_err()
        );
        assert!(
            ForeignKeyInfo::new(
                vec!["child_id".into()],
                String::new(),
                vec!["id".into()],
                None
            )
            .is_err()
        );
        assert!(
            ForeignKeyInfo::new(vec![" ".into()], "parent".into(), vec!["id".into()], None)
                .is_err()
        );
        assert!(
            ForeignKeyInfo::new(
                vec!["child_id".into()],
                "parent".into(),
                vec!["id".into()],
                Some("  ".into())
            )
            .is_err()
        );
        assert!(IndexInfo::new(String::new(), vec!["id".into()], false).is_err());
        assert!(IndexInfo::new("  ".into(), vec!["id".into()], true).is_err());
        assert!(IndexInfo::new("idx_a".into(), vec![" ".into()], false).is_err());
        assert!(
            ConstraintInfo::new(
                String::new(),
                ConstraintKind::Pk,
                vec!["id".into()],
                None,
                Vec::new(),
                None
            )
            .is_err()
        );
        assert!(
            ConstraintInfo::new(
                " ".into(),
                ConstraintKind::Unique,
                vec!["id".into()],
                None,
                Vec::new(),
                None
            )
            .is_err()
        );
        assert!(SequenceInfo::new(String::new()).is_err());
        assert!(SequenceInfo::new("  ".into()).is_err());
        assert!(IndexRef::new(String::new(), "idx_a".into()).is_err());
        assert!(IndexRef::new("users".into(), "  ".into()).is_err());
        assert!(TriggerInfo::new(String::new(), None, None, None).is_err());
        assert!(TriggerInfo::new("  ".into(), None, None, None).is_err());
        assert!(TriggerInfo::new("trg_a".into(), Some(" ".into()), None, None).is_err());
        assert!(TriggerInfo::new("trg_a".into(), None, Some(" ".into()), None).is_err());
        assert!(TriggerInfo::new("trg_a".into(), None, None, Some(" ".into())).is_err());
        assert!(RoutineInfo::new(String::new(), None, None, None).is_err());
        assert!(RoutineInfo::new("  ".into(), None, None, None).is_err());
        assert!(RoutineInfo::new("fn_a".into(), Some(" ".into()), None, None).is_err());
        assert!(RoutineInfo::new("fn_a".into(), None, Some(" ".into()), None).is_err());
        assert!(RoutineInfo::new("fn_a".into(), None, None, Some(" ".into())).is_err());
        assert!(
            SchemaNode::new(
                String::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                false,
                false
            )
            .is_err()
        );
        assert!(DatabaseNode::new(String::new(), Vec::new()).is_err());
        assert!(ColumnInfo::new("id".into(), "INTEGER".into(), false, Some(1)).is_ok());
        assert!(
            ForeignKeyInfo::new(
                vec!["child_id".into()],
                "parent".into(),
                vec!["id".into()],
                None
            )
            .is_ok()
        );
        assert!(IndexInfo::new("idx_a".into(), vec!["id".into()], true).is_ok());
        assert!(
            ConstraintInfo::new(
                "PRIMARY KEY".into(),
                ConstraintKind::Pk,
                vec!["id".into()],
                None,
                Vec::new(),
                None
            )
            .is_ok()
        );
        assert!(SequenceInfo::new("seq_a".into()).is_ok());
        assert!(IndexRef::new("users".into(), "idx_a".into()).is_ok());
        assert!(
            TriggerInfo::new(
                "trg_a".into(),
                Some("users".into()),
                Some("BEFORE INSERT".into()),
                Some("CREATE TRIGGER trg_a BEFORE INSERT ON users".into())
            )
            .is_ok()
        );
        assert!(
            RoutineInfo::new(
                "fn_a".into(),
                Some("FUNCTION".into()),
                Some("plpgsql".into()),
                Some("BEGIN RETURN NEW; END".into())
            )
            .is_ok()
        );
        assert!(
            TableInfo::new(
                "users".into(),
                TableKind::Table,
                vec![
                    ColumnInfo::new("id".into(), "INTEGER".into(), false, Some(1)).unwrap()
                ],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                false,
                false,
            )
            .is_ok()
        );
    }

    #[test]
    fn sqlite_schema_lists_tables_view_and_pk() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, note TEXT)",
            [],
        )
        .unwrap();
        conn.execute("CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER)", [])
            .unwrap();
        conn.execute("CREATE VIEW user_names AS SELECT name FROM users", []).unwrap();
        conn.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT)", [])
            .unwrap();
        conn.execute(
            "CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL \
              REFERENCES parent(id))",
            [],
        )
        .unwrap();
        conn.execute("CREATE UNIQUE INDEX idx_child_parent_unique ON child(parent_id)", [])
            .unwrap();
        conn.execute("CREATE INDEX idx_child_id_plain ON child(id)", []).unwrap();
        conn.execute(
            "CREATE TRIGGER trg_users_note AFTER INSERT ON users BEGIN SELECT 1; END",
            [],
        )
        .unwrap();
        let mut session = Session::Sqlite(conn);
        let schema = session.schema().unwrap();
        assert!(!schema.tables_truncated());
        assert!(!schema.columns_truncated());
        assert_eq!(schema.databases().len(), 1);
        let db = &schema.databases()[0];
        assert_eq!(db.name(), ":memory:");
        assert_eq!(db.schemas().len(), 1);
        let main = &db.schemas()[0];
        assert_eq!(main.name(), "main");
        assert!(main.sequences().is_empty());
        assert!(!main.triggers_truncated());
        assert_eq!(main.triggers().len(), 1);
        let trigger = &main.triggers()[0];
        assert_eq!(trigger.name(), "trg_users_note");
        assert_eq!(trigger.table(), Some("users"));
        assert_eq!(trigger.kind(), None);
        assert!(
            trigger.definition().is_some_and(|d| d.contains("AFTER INSERT ON users")),
            "{:?}",
            trigger.definition()
        );
        assert!(main.routines().is_empty());
        assert!(!main.routines_truncated());
        let table_names: Vec<&str> =
            main.tables().iter().map(|t| t.name()).collect();
        assert_eq!(table_names, vec!["child", "parent", "posts", "users"]);
        let view_names: Vec<&str> = main.views().iter().map(|t| t.name()).collect();
        assert_eq!(view_names, vec!["user_names"]);
        let users = main.tables().iter().find(|t| t.name() == "users").unwrap();
        assert_eq!(users.kind(), TableKind::Table);
        assert_eq!(users.columns().len(), 3);
        let id = users.columns().iter().find(|c| c.name() == "id").unwrap();
        assert_eq!(id.pk_position(), Some(1));
        assert!(id.is_pk());
        assert!(!id.nullable());
        let name = users.columns().iter().find(|c| c.name() == "name").unwrap();
        assert_eq!(name.pk_position(), None);
        assert!(!name.nullable());
        assert_eq!(name.type_name(), "TEXT");
        let note = users.columns().iter().find(|c| c.name() == "note").unwrap();
        assert!(note.nullable());
        let pk: Vec<&ConstraintInfo> = users
            .constraints()
            .iter()
            .filter(|c| c.kind() == ConstraintKind::Pk)
            .collect();
        assert_eq!(pk.len(), 1);
        assert_eq!(pk[0].columns(), &["id".to_owned()]);
        assert!(users.referenced_by().is_empty());
        let view = main.views().iter().find(|t| t.name() == "user_names").unwrap();
        assert_eq!(view.kind(), TableKind::View);
        assert!(!view.columns().is_empty());
        assert!(view.constraints().is_empty());
        assert!(view.referenced_by().is_empty());
        let child = main.tables().iter().find(|t| t.name() == "child").unwrap();
        assert_eq!(child.foreign_keys().len(), 1);
        let fk = &child.foreign_keys()[0];
        assert_eq!(fk.columns(), &["parent_id".to_owned()]);
        assert_eq!(fk.ref_table(), "parent");
        assert_eq!(fk.ref_columns(), &["id".to_owned()]);
        assert!(child.referenced_by().is_empty());
        assert!(!child.foreign_keys_truncated());
        assert!(!child.indexes_truncated());
        let index_names: Vec<(&str, bool)> =
            child.indexes().iter().map(|i| (i.name(), i.is_unique())).collect();
        assert_eq!(
            index_names,
            vec![("idx_child_id_plain", false), ("idx_child_parent_unique", true)]
        );
        let unique_idx =
            child.indexes().iter().find(|i| i.name() == "idx_child_parent_unique").unwrap();
        assert!(unique_idx.unique());
        assert_eq!(unique_idx.columns(), &["parent_id".to_owned()]);
        let plain_idx =
            child.indexes().iter().find(|i| i.name() == "idx_child_id_plain").unwrap();
        assert!(!plain_idx.unique());
        assert_eq!(plain_idx.columns(), &["id".to_owned()]);
        let uniq: Vec<&ConstraintInfo> = child
            .constraints()
            .iter()
            .filter(|c| c.kind() == ConstraintKind::Unique)
            .collect();
        assert_eq!(uniq.len(), 1);
        assert_eq!(uniq[0].name(), "idx_child_parent_unique");
        assert_eq!(uniq[0].columns(), &["parent_id".to_owned()]);
        let parent = main.tables().iter().find(|t| t.name() == "parent").unwrap();
        assert!(parent.foreign_keys().is_empty());
        assert_eq!(parent.referenced_by().len(), 1);
        let incoming = &parent.referenced_by()[0];
        assert_eq!(incoming.ref_table(), "child");
        assert_eq!(incoming.ref_columns(), &["parent_id".to_owned()]);
        assert_eq!(incoming.columns(), &["id".to_owned()]);
        let parent_pk: Vec<&ConstraintInfo> = parent
            .constraints()
            .iter()
            .filter(|c| c.kind() == ConstraintKind::Pk)
            .collect();
        assert_eq!(parent_pk.len(), 1);
        let agg: Vec<(&str, &str)> = main
            .schema_indexes()
            .iter()
            .map(|r| (r.table(), r.index()))
            .collect();
        assert!(agg.contains(&("child", "idx_child_parent_unique")));
        assert!(agg.contains(&("child", "idx_child_id_plain")));
    }

    #[test]
    fn sqlite_schema_caps_triggers_and_flags_truncation() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY)", []).unwrap();
        for i in 0..SCHEMA_TRIGGER_CAP + 1 {
            conn.execute(
                &format!("CREATE TRIGGER trg_{i:03} AFTER INSERT ON users BEGIN SELECT 1; END"),
                [],
            )
            .unwrap();
        }
        let mut session = Session::Sqlite(conn);
        let schema = session.schema().unwrap();
        let main = &schema.databases()[0].schemas()[0];
        assert_eq!(main.triggers().len(), SCHEMA_TRIGGER_CAP);
        assert!(main.triggers_truncated());
        assert_eq!(main.triggers()[0].name(), "trg_000");
        assert!(main.routines().is_empty());
        assert!(!main.routines_truncated());
    }

    #[test]
    fn row_count_labels_exact_and_estimate() {
        assert_eq!(RowCount::Exact(3).label(), "3");
        assert_eq!(RowCount::Estimate(3).label(), "~3");
        assert!(RowCount::Estimate(3).is_estimate());
        assert!(!RowCount::Exact(3).is_estimate());
        assert_eq!(RowCount::Exact(3).value(), 3);
        assert_eq!(RowCount::Estimate(7).value(), 7);
    }

    #[test]
    fn sqlite_row_count_is_exact_and_a_dropped_table_errors() {
        let mut session = Session::Sqlite(rusqlite::Connection::open_in_memory().unwrap());
        session.query("CREATE TABLE users (id INTEGER PRIMARY KEY)").expect("create");
        session
            .query("INSERT INTO users (id) VALUES (1), (2), (3)")
            .expect("seed");
        assert_eq!(session.row_count("main", "users").expect("count"), RowCount::Exact(3));
        session.query("DROP TABLE users").expect("drop");
        assert!(matches!(session.row_count("main", "users"), Err(DbError::Query(_))));
    }

    #[test]
    fn sqlite_row_count_quotes_awkward_identifiers() {
        let mut session = Session::Sqlite(rusqlite::Connection::open_in_memory().unwrap());
        session
            .query("CREATE TABLE \"we\"\"ird table\" (id INTEGER)")
            .expect("create");
        session
            .query("INSERT INTO \"we\"\"ird table\" (id) VALUES (1)")
            .expect("seed");
        assert_eq!(
            session.row_count("main", "we\"ird table").expect("count"),
            RowCount::Exact(1)
        );
    }

    #[test]
    fn batch_runs_in_order_and_stops_at_the_failing_statement() {
        let mut session = Session::Sqlite(rusqlite::Connection::open_in_memory().unwrap());
        let statements: Vec<String> = vec![
            "CREATE TABLE t (id INTEGER)".to_owned(),
            "INSERT INTO t VALUES (1)".to_owned(),
            "SELECT id FROM t".to_owned(),
        ];
        let results = run_statements(&mut session, &statements).expect("batch runs");
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].affected(), Some(0));
        assert_eq!(results[1].affected(), Some(1));
        assert_eq!(results[2].row_count(), 1);
        assert_eq!(results[2].cell(0, 0), Some(&Value::Integer(1)));

        let failing: Vec<String> = vec![
            "SELECT 1".to_owned(),
            "SELECT nope FROM missing_table".to_owned(),
            "SELECT 3".to_owned(),
        ];
        let (index, error) = run_statements(&mut session, &failing).expect_err("stops");
        assert_eq!(index, 1);
        assert!(matches!(&error, DbError::Query(_)));
        let named = statement_error(index, error).to_string();
        assert!(named.contains("statement 1"), "{named}");
        assert!(named.contains("missing_table"), "{named}");
    }

    #[test]
    fn explain_sql_prefixes_each_engine() {
        assert_eq!(explain_sql(DbKind::Sqlite, "SELECT 1"), "EXPLAIN QUERY PLAN SELECT 1");
        assert_eq!(explain_sql(DbKind::Postgres, "SELECT 1"), "EXPLAIN SELECT 1");
        assert_eq!(
            explain_sql(DbKind::Sqlite, "  SELECT 1  "),
            "EXPLAIN QUERY PLAN SELECT 1"
        );
    }

    #[test]
    #[ignore = "live engine test; run with --ignored"]
    fn sqlite_live_explain_query_plan_returns_rows() {
        let mut session = Session::Sqlite(rusqlite::Connection::open_in_memory().unwrap());
        let plan = explain_sql(DbKind::Sqlite, "SELECT 1");
        let result = session.query(&plan).expect("explain runs");
        assert!(result.row_count() > 0);
    }

    #[test]
    #[ignore = "live engine test; run with --ignored"]
    fn sqlite_live_row_count_counts_exactly() {
        let mut session = Session::Sqlite(rusqlite::Connection::open_in_memory().unwrap());
        session
            .query("CREATE TABLE cde_count_probe (id INTEGER)")
            .expect("create probe");
        session
            .query("INSERT INTO cde_count_probe (id) VALUES (1), (2), (3), (4)")
            .expect("seed probe");
        let count = session.row_count("main", "cde_count_probe").expect("count");
        session.query("DROP TABLE cde_count_probe").expect("cleanup probe");
        assert_eq!(count, RowCount::Exact(4));
    }

    #[test]
    #[ignore = "live engine test; run with --ignored"]
    fn sqlite_live_edit_statements_write_and_reselect() {
        let mut session = Session::Sqlite(rusqlite::Connection::open_in_memory().unwrap());
        session
            .query("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
            .expect("create");
        session
            .query("INSERT INTO users (id, name) VALUES (1, 'ada'), (2, 'grace')")
            .expect("seed");
        let updated = session
            .query(&crate::edit::update_statement(
                "users",
                &[("id".to_owned(), "1".to_owned())],
                &[("name".to_owned(), "Ada".to_owned())],
            ))
            .expect("update");
        assert_eq!(updated.affected(), Some(1));
        let deleted = session
            .query(&crate::edit::delete_statement(
                "users",
                &[("id".to_owned(), "2".to_owned())],
            ))
            .expect("delete");
        assert_eq!(deleted.affected(), Some(1));
        let inserted = session
            .query(&crate::edit::insert_statement(
                "users",
                &["id".to_owned(), "name".to_owned()],
                &["3".to_owned(), "Lin".to_owned()],
            ))
            .expect("insert");
        assert_eq!(inserted.affected(), Some(1));
        let result = session
            .query("SELECT id, name FROM users ORDER BY id")
            .expect("reselect");
        assert_eq!(result.row_count(), 2);
        assert_eq!(result.cell(0, 0), Some(&Value::Integer(1)));
        assert_eq!(result.cell(0, 1), Some(&Value::Text("Ada".to_owned())));
        assert_eq!(result.cell(1, 0), Some(&Value::Integer(3)));
        assert_eq!(result.cell(1, 1), Some(&Value::Text("Lin".to_owned())));
    }

    #[test]
    fn worker_batch_protocol_roundtrip() {
        let (tx, rx) = mpsc::channel();
        let handle = spawn_session(
            ConnectionId::from_raw(9001),
            ConnectionConfig::Sqlite { read_only: false, path: ":memory:".into() },
            None,
            None,
            eframe::egui::Context::default(),
            tx,
            TxIdle::default(),
        );
        match rx.recv_timeout(Duration::from_secs(10)).expect("connects") {
            DbEvent::Connected { .. } => {}
            other => panic!("expected Connected, got {other:?}"),
        }
        let mut ids = Ids::new();
        let run = ids.run();
        handle
            .send(DbCommand::QueryBatch {
                run,
                statements: vec!["SELECT 1".to_owned(), "SELECT 2".to_owned()],
            })
            .expect("send");
        match rx.recv_timeout(Duration::from_secs(10)).expect("batch event") {
            DbEvent::BatchOk { run: got, results } => {
                assert_eq!(got, run);
                assert_eq!(results.len(), 2);
                assert_eq!(results[0].cell(0, 0), Some(&Value::Integer(1)));
                assert_eq!(results[1].cell(0, 0), Some(&Value::Integer(2)));
            }
            other => panic!("expected BatchOk, got {other:?}"),
        }
        let failed_run = ids.run();
        handle
            .send(DbCommand::QueryBatch {
                run: failed_run,
                statements: vec![
                    "SELECT 1".to_owned(),
                    "SELECT * FROM missing_table".to_owned(),
                    "SELECT 3".to_owned(),
                ],
            })
            .expect("send");
        match rx.recv_timeout(Duration::from_secs(10)).expect("failure event") {
            DbEvent::QueryErr { run: got, error } => {
                assert_eq!(got, failed_run);
                assert!(error.to_string().contains("statement 1"), "{error}");
            }
            other => panic!("expected QueryErr, got {other:?}"),
        }
    }

    #[test]
    fn worker_set_autocommit_on_sqlite_keeps_autocommit_on() {
        let (tx, rx) = mpsc::channel();
        let handle = spawn_session(
            ConnectionId::from_raw(9002),
            ConnectionConfig::Sqlite { read_only: false, path: ":memory:".into() },
            None,
            None,
            eframe::egui::Context::default(),
            tx,
            TxIdle::default(),
        );
        match rx.recv_timeout(Duration::from_secs(10)).expect("connects") {
            DbEvent::Connected { .. } => {}
            other => panic!("expected Connected, got {other:?}"),
        }
        handle.send(DbCommand::SetAutocommit { on: false }).expect("send");
        match rx.recv_timeout(Duration::from_secs(10)).expect("toggle event") {
            DbEvent::AutocommitSet { conn, on, tx_open, error } => {
                assert_eq!(conn.raw(), 9002);
                assert!(on, "SQLite has no manual mode");
                assert!(!tx_open);
                assert!(error.is_none());
            }
            other => panic!("expected AutocommitSet, got {other:?}"),
        }
    }

    #[cfg(any(feature = "postgres", feature = "mysql"))]
    #[test]
    fn manual_plan_opens_the_transaction_only_when_one_is_not_open() {
        use ManualPlan::{BeginThenRun, Only, Skip};
        let insert = tx_control("INSERT INTO t VALUES (1)");
        assert_eq!(manual_plan(false, insert), BeginThenRun);
        assert_eq!(manual_plan(true, insert), Only);
        assert_eq!(manual_plan(false, tx_control("begin")), Only);
        assert_eq!(manual_plan(true, tx_control("begin")), Skip);
        for control in [tx_control("COMMIT"), tx_control("rollback")] {
            assert_eq!(manual_plan(false, control), Only);
            assert_eq!(manual_plan(true, control), Only);
        }
        assert_eq!(tx_control("START TRANSACTION"), None);
        assert_eq!(tx_control("SELECT 1"), None);
    }

    #[cfg(any(feature = "postgres", feature = "mysql"))]
    #[test]
    fn the_idle_clock_warns_once_then_rolls_back_and_resets_on_activity() {
        let idle = TxIdle {
            warn: Duration::from_millis(20),
            rollback: Duration::from_millis(60),
        };
        let pinned = || AutoMode::<()>::Pinned {
            conn: (),
            open: true,
            idle_since: Instant::now(),
            warned: false,
        };
        let mut mode = pinned();
        assert!(mode.idle_wait(&idle).is_some_and(|wait| wait <= idle.warn));
        assert_eq!(mode.idle_tick(&idle), None);
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(mode.idle_tick(&idle), Some(IdleTick::Warn));
        assert_eq!(mode.idle_tick(&idle), None, "the warning fires once");
        assert!(mode.idle_wait(&idle).is_some_and(|wait| wait <= idle.rollback));
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(mode.idle_tick(&idle), Some(IdleTick::Rollback));
        mode.tx_close();
        assert_eq!(mode.idle_wait(&idle), None, "a closed transaction has no clock");

        let mut mode = pinned();
        std::thread::sleep(Duration::from_millis(25));
        mode.note_activity();
        assert_eq!(mode.idle_tick(&idle), None, "activity restarts the span");
    }

    #[test]
    fn write_classifier_names_each_write_kind() {
        for (sql, kind) in [
            ("INSERT INTO t (a) VALUES (1)", "INSERT"),
            ("UPDATE t SET a = 1", "UPDATE"),
            ("DELETE FROM t", "DELETE"),
            ("CREATE TABLE t (a INTEGER)", "CREATE"),
            ("ALTER TABLE t ADD COLUMN b TEXT", "ALTER"),
            ("DROP TABLE t", "DROP"),
            ("TRUNCATE TABLE t", "TRUNCATE"),
            ("REPLACE INTO t (a) VALUES (1)", "REPLACE"),
            ("MERGE INTO t USING s ON t.a = s.a WHEN MATCHED THEN UPDATE SET a = 1", "MERGE"),
            ("COPY t FROM 'f.csv'", "COPY"),
            ("VACUUM", "VACUUM"),
            ("GRANT SELECT ON t TO reader", "GRANT"),
            ("REVOKE SELECT ON t FROM reader", "REVOKE"),
            ("DO $$ BEGIN UPDATE t SET a = 1; END $$", "DO"),
            ("CALL refresh_stats()", "CALL"),
            ("  insert into t (a) values (1)", "INSERT"),
            ("-- wipe\nDELETE FROM t", "DELETE"),
            ("/* plan */ DROP TABLE t", "DROP"),
        ] {
            assert_eq!(write_statement_kind(sql), Some(kind), "{sql}");
            assert!(is_write_statement(sql), "{sql}");
        }
    }

    #[test]
    fn write_classifier_passes_reads_explain_and_parenthesized_selects() {
        for sql in [
            "SELECT * FROM t",
            "select * from t",
            "WITH ids AS (SELECT 1) SELECT * FROM ids",
            "EXPLAIN SELECT * FROM t",
            "EXPLAIN QUERY PLAN SELECT * FROM t",
            "EXPLAIN UPDATE t SET a = 1",
            "PRAGMA table_info(t)",
            "(SELECT * FROM t)",
            "((SELECT * FROM t))",
            "-- comment\nSELECT * FROM t",
            "/* comment */ SELECT * FROM t",
            "VALUES (1), (2)",
            "",
            "   ",
        ] {
            assert_eq!(write_statement_kind(sql), None, "{sql:?}");
            assert!(!is_write_statement(sql), "{sql:?}");
        }
    }

    #[test]
    fn guards_flag_a_write_under_either_reading_of_hash() {
        assert_eq!(
            write_statement_kind("WITH x AS (SELECT a # b FROM s) DELETE FROM t"),
            Some("DELETE")
        );
        assert_eq!(
            unqualified_write("WITH x AS (SELECT a # b FROM s) DELETE FROM t"),
            Some(UnqualifiedWrite { kind: "DELETE", table: "t".to_owned() })
        );
        // MySQL reads the WHERE as a comment, so the guard confirms on every
        // engine rather than trust the Postgres reading.
        assert_eq!(
            unqualified_write("UPDATE t SET f = f # 4 WHERE id = 1"),
            Some(UnqualifiedWrite { kind: "UPDATE", table: "t".to_owned() })
        );
        assert_eq!(unqualified_write("UPDATE t SET f = 1 WHERE f # 4 = 0"), None);
    }

    #[test]
    fn guards_read_hash_comments_and_backtick_identifiers() {
        assert_eq!(write_statement_kind("# wipe\nDELETE FROM t"), Some("DELETE"));
        assert_eq!(
            unqualified_write("DELETE FROM t # WHERE id = 1"),
            Some(UnqualifiedWrite { kind: "DELETE", table: "t".to_owned() })
        );
        assert_eq!(
            unqualified_write("UPDATE `my table` SET a = 1"),
            Some(UnqualifiedWrite { kind: "UPDATE", table: "my table".to_owned() })
        );
        assert_eq!(
            unqualified_write("DELETE FROM `a``b`"),
            Some(UnqualifiedWrite { kind: "DELETE", table: "a`b".to_owned() })
        );
        assert_eq!(unqualified_write("DELETE FROM `orders` WHERE id = 1"), None);
        assert_eq!(
            unqualified_write("UPDATE `t` SET note = 'x' -- where\n"),
            Some(UnqualifiedWrite { kind: "UPDATE", table: "t".to_owned() })
        );
    }

    #[test]
    fn dollar_quoted_text_never_reads_as_a_clause() {
        assert_eq!(
            unqualified_write("UPDATE t SET note = $$ WHERE $$"),
            Some(UnqualifiedWrite { kind: "UPDATE", table: "t".to_owned() })
        );
        assert_eq!(
            unqualified_write("UPDATE t SET note = $q$ WHERE $q$"),
            Some(UnqualifiedWrite { kind: "UPDATE", table: "t".to_owned() })
        );
        assert_eq!(
            unqualified_write("UPDATE t SET note = $$x$$ /* where */"),
            Some(UnqualifiedWrite { kind: "UPDATE", table: "t".to_owned() })
        );
        assert_eq!(unqualified_write("DELETE FROM t WHERE note = $$x$$"), None);
    }

    #[test]
    fn write_classifier_sees_cte_bodies_and_explain_analyze() {
        for (sql, kind) in [
            ("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d", "DELETE"),
            ("WITH ids AS (SELECT 1) DELETE FROM t", "DELETE"),
            ("WITH RECURSIVE t(n) AS (SELECT 1) UPDATE t SET n = 2", "UPDATE"),
            ("WITH x (a) AS (SELECT 1) UPDATE t SET a = 1", "UPDATE"),
            ("WITH x AS MATERIALIZED (SELECT 1) INSERT INTO t VALUES (1)", "INSERT"),
            (
                "WITH x AS (SELECT 1), y AS NOT MATERIALIZED (DELETE FROM t) SELECT 1",
                "DELETE",
            ),
            ("EXPLAIN ANALYZE UPDATE t SET a = 1", "UPDATE"),
            ("EXPLAIN VERBOSE ANALYZE DELETE FROM t", "DELETE"),
            ("EXPLAIN (ANALYZE, BUFFERS) DELETE FROM t", "DELETE"),
            ("EXPLAIN (VERBOSE, ANALYZE TRUE) INSERT INTO t VALUES (1)", "INSERT"),
            ("EXPLAIN (FORMAT JSON, ANALYZE) UPDATE t SET a = 1", "UPDATE"),
        ] {
            assert_eq!(write_statement_kind(sql), Some(kind), "{sql}");
            assert!(is_write_statement(sql), "{sql}");
        }
    }

    #[test]
    fn write_classifier_leaves_reads_and_plain_explain_alone() {
        for sql in [
            "WITH ids AS (SELECT 1) SELECT * FROM ids",
            "WITH x AS (SELECT 1), y AS (SELECT 2) SELECT * FROM y",
            "WITH x AS NOT MATERIALIZED (SELECT 1) SELECT 1",
            "EXPLAIN UPDATE t SET a = 1",
            "EXPLAIN QUERY PLAN DELETE FROM t",
            "EXPLAIN (VERBOSE) DELETE FROM t",
            "EXPLAIN (ANALYZE FALSE) DELETE FROM t",
            "EXPLAIN (ANALYZE OFF) DELETE FROM t",
            "EXPLAIN (ANALYZE 0) DELETE FROM t",
            "EXPLAIN ANALYZE SELECT 1",
        ] {
            assert_eq!(write_statement_kind(sql), None, "{sql:?}");
            assert!(!is_write_statement(sql), "{sql:?}");
        }
    }

    #[test]
    fn unqualified_write_sees_through_ctes_and_explain_analyze() {
        assert_eq!(
            unqualified_write("WITH ids AS (SELECT 1) DELETE FROM t"),
            Some(UnqualifiedWrite { kind: "DELETE", table: "t".to_owned() })
        );
        assert_eq!(
            unqualified_write("WITH d AS (UPDATE t SET a = 1 RETURNING *) SELECT * FROM d"),
            Some(UnqualifiedWrite { kind: "UPDATE", table: "t".to_owned() })
        );
        assert_eq!(
            unqualified_write("EXPLAIN ANALYZE DELETE FROM t"),
            Some(UnqualifiedWrite { kind: "DELETE", table: "t".to_owned() })
        );
        assert_eq!(
            unqualified_write("EXPLAIN (ANALYZE) UPDATE public.t SET a = 1"),
            Some(UnqualifiedWrite { kind: "UPDATE", table: "public.t".to_owned() })
        );
        assert_eq!(
            unqualified_write("WITH ids AS (SELECT 1) DELETE FROM t WHERE id = 1"),
            None
        );
        assert_eq!(unqualified_write("EXPLAIN ANALYZE DELETE FROM t WHERE id = 1"), None);
        assert_eq!(unqualified_write("EXPLAIN DELETE FROM t"), None);
        assert_eq!(unqualified_write("EXPLAIN (ANALYZE FALSE) DELETE FROM t"), None);
        assert_eq!(unqualified_write("WITH d AS (SELECT 1) SELECT * FROM d"), None);
    }

    #[test]
    fn unqualified_write_reads_mysql_multi_table_delete() {
        assert_eq!(
            unqualified_write("DELETE t1 FROM t1 JOIN t2 ON t1.id = t2.id"),
            Some(UnqualifiedWrite { kind: "DELETE", table: "t1".to_owned() })
        );
        assert_eq!(
            unqualified_write("DELETE LOW_PRIORITY QUICK IGNORE t1 FROM t1 JOIN t2 ON t1.id = t2.id"),
            Some(UnqualifiedWrite { kind: "DELETE", table: "t1".to_owned() })
        );
        assert_eq!(
            unqualified_write("DELETE t1.* FROM t1 JOIN t2 ON t1.id = t2.id"),
            Some(UnqualifiedWrite { kind: "DELETE", table: "t1".to_owned() })
        );
        assert_eq!(
            unqualified_write("DELETE t1, t2 FROM t1 JOIN t2 ON t1.id = t2.id"),
            Some(UnqualifiedWrite { kind: "DELETE", table: "t1".to_owned() })
        );
        assert_eq!(
            unqualified_write("DELETE t1 FROM t1 JOIN t2 ON t1.id = t2.id WHERE t2.a = 1"),
            None
        );
        assert_eq!(unqualified_write("DELETE t1 WHERE a = 1"), None);
        assert_eq!(unqualified_write("DELETE t1"), None);
    }

    #[test]
    fn read_only_flag_defaults_off_and_leaves_validate_green() {
        let old = serde_json::json!({
            "kind": "sqlite",
            "path": ":memory:",
        });
        let back: ConnectionConfig = serde_json::from_value(old).unwrap();
        assert_eq!(
            back,
            ConnectionConfig::Sqlite { path: ":memory:".into(), read_only: false }
        );
        assert!(!back.is_read_only());
        assert!(back.validate().is_ok());
        let mut flagged = back;
        flagged.set_read_only(true);
        assert!(flagged.is_read_only());
        assert!(flagged.validate().is_ok());
        let json = serde_json::to_string(&flagged).unwrap();
        assert!(json.contains("\"read_only\":true"), "{json}");
        let roundtrip: ConnectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, flagged);
    }

    #[test]
    fn sqlite_read_only_session_refuses_writes() {
        let read_only = ConnectionConfig::Sqlite {
            path: PathBuf::from(":memory:"),
            read_only: true,
        };
        let mut session = Session::open(&read_only, None, None).unwrap();
        assert_eq!(session.query("SELECT 1 AS n").unwrap().row_count(), 1);
        let refused = session.query("CREATE TABLE t (a INTEGER)").unwrap_err();
        assert!(refused.to_string().contains("readonly"), "{refused}");

        let writable = ConnectionConfig::Sqlite {
            path: PathBuf::from(":memory:"),
            read_only: false,
        };
        let mut session = Session::open(&writable, None, None).unwrap();
        session.query("CREATE TABLE t (a INTEGER)").expect("a writable session creates");
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_read_only_session_refuses_writes() {
        let config = ConnectionConfig::Postgres {
            read_only: true,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        };
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        assert_eq!(session.query("SELECT 1").expect("a read passes").row_count(), 1);
        let _ = session.query("SELECT pg_terminate_backend(pg_backend_pid())");
        assert!(
            (0..3).any(|_| session.query("SELECT 1").is_ok()),
            "the pool reconnects after its backend dies"
        );
        let refused = session
            .query("CREATE TABLE cde_read_only_probe (a integer)")
            .expect_err("a reconnected read-only session still refuses a write");
        assert!(refused.to_string().contains("read-only"), "{refused}");
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_read_only_session_refuses_writes() {
        let config = ConnectionConfig::Mysql {
            read_only: true,
            host: std::env::var("MYSQLHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("MYSQLPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(3306),
            database: std::env::var("MYSQLDATABASE").unwrap_or_else(|_| "cde_test".to_owned()),
            user: std::env::var("MYSQLUSER").unwrap_or_else(|_| "root".to_owned()),
            ssl_mode: MysqlSslMode::default(),
            ssh: None,
        };
        let password = std::env::var("MYSQLPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        assert_eq!(session.query("SELECT 1").expect("a read passes").row_count(), 1);
        let _ = session.query("KILL CONNECTION_ID()");
        assert!(
            (0..3).any(|_| session.query("SELECT 1").is_ok()),
            "the pool reconnects after its connection dies"
        );
        let refused = session
            .query("CREATE TABLE cde_read_only_probe (a integer)")
            .expect_err("a reconnected read-only session still refuses a write");
        let text = refused.to_string().to_ascii_lowercase();
        assert!(text.contains("read only") || text.contains("read-only"), "{refused}");
    }

    #[test]
    fn unqualified_write_names_kind_and_table() {
        assert_eq!(
            unqualified_write("UPDATE users SET name = 'ada'"),
            Some(UnqualifiedWrite { kind: "UPDATE", table: "users".to_owned() })
        );
        assert_eq!(
            unqualified_write("DELETE FROM users"),
            Some(UnqualifiedWrite { kind: "DELETE", table: "users".to_owned() })
        );
        assert_eq!(
            unqualified_write("update users set name = 'ada'"),
            Some(UnqualifiedWrite { kind: "UPDATE", table: "users".to_owned() })
        );
        assert_eq!(
            unqualified_write("-- wipe\nDELETE FROM users"),
            Some(UnqualifiedWrite { kind: "DELETE", table: "users".to_owned() })
        );
        assert_eq!(
            unqualified_write("UPDATE public.users SET name = 'ada'"),
            Some(UnqualifiedWrite { kind: "UPDATE", table: "public.users".to_owned() })
        );
        assert_eq!(unqualified_write("UPDATE users SET name = 'ada' WHERE id = 1"), None);
        assert_eq!(unqualified_write("DELETE FROM users WHERE id = 1"), None);
        assert_eq!(
            unqualified_write("delete from users where id = 1"),
            None
        );
        assert_eq!(unqualified_write("(SELECT * FROM t)"), None);
        assert_eq!(unqualified_write("-- only\n(SELECT * FROM t)"), None);
        assert_eq!(unqualified_write("SELECT * FROM t"), None);
        assert_eq!(unqualified_write("INSERT INTO t (a) VALUES (1)"), None);
        assert_eq!(unqualified_write("UPDATE t SET a = 'where'"), Some(UnqualifiedWrite {
            kind: "UPDATE",
            table: "t".to_owned()
        }));
        assert_eq!(
            unqualified_write("UPDATE t SET a = (SELECT max(a) FROM u WHERE u.id = t.id)"),
            Some(UnqualifiedWrite { kind: "UPDATE", table: "t".to_owned() })
        );
    }

    #[test]
    fn session_info_rejects_an_empty_backend_id() {
        assert!(SessionInfo::new(String::new(), "cde".into(), "cde_test".into(), "active".into(), "SELECT 1".into(), false, String::new(), false).is_err());
        assert!(SessionInfo::new("  ".into(), String::new(), String::new(), String::new(), String::new(), false, String::new(), false).is_err());
        let session = SessionInfo::new("12345".into(), "cde".into(), "cde_test".into(), "active".into(), "SELECT 1".into(), false, String::new(), true).unwrap();
        assert_eq!(session.backend_id(), "12345");
        assert_eq!(session.user(), "cde");
        assert_eq!(session.database(), "cde_test");
        assert_eq!(session.state(), "active");
        assert_eq!(session.query(), "SELECT 1");
        assert!(!session.waiting());
        assert_eq!(session.locks(), "");
        assert_eq!(session.wait_note(), "");
        assert!(session.is_self());
    }

    #[test]
    fn session_wait_note_names_the_lock_only_while_waiting() {
        let waiting = SessionInfo::new("11".into(), "cde".into(), "cde_test".into(), "active".into(), String::new(), true, "RowExclusiveLock relation".into(), false).unwrap();
        assert!(waiting.waiting());
        assert_eq!(waiting.wait_note(), "RowExclusiveLock relation");
        let unnamed = SessionInfo::new("12".into(), "cde".into(), "cde_test".into(), "active".into(), String::new(), true, String::new(), false).unwrap();
        assert_eq!(unnamed.wait_note(), "waiting on lock");
    }

    #[test]
    fn pg_sessions_sql_reads_activity_plus_locks() {
        let sql = pg_sessions_sql();
        assert!(sql.contains("pg_stat_activity"), "{sql}");
        assert!(sql.contains("pg_locks"), "{sql}");
    }

    #[test]
    fn mysql_sessions_sql_reads_processlist_and_locks_read_lock_waits() {
        assert!(mysql_sessions_sql().contains("processlist"), "{}", mysql_sessions_sql());
        let locks = mysql_lock_waits_sql();
        assert!(locks.contains("data_lock_waits"), "{locks}");
        assert!(locks.contains("threads"), "{locks}");
    }

    #[test]
    fn terminate_sql_names_the_backend_as_an_integer() {
        assert_eq!(pg_terminate_sql(12345), "SELECT pg_terminate_backend(12345)");
        assert_eq!(mysql_terminate_sql(7), "KILL 7");
    }

    #[test]
    fn pg_session_from_parts_maps_nulls_to_empty_text() {
        let session = pg_session_from_parts(
            12345,
            Some("cde".to_owned()),
            None,
            Some("active".to_owned()),
            None,
            true,
            Some("RowExclusiveLock relation".to_owned()),
            true,
        )
        .unwrap();
        assert_eq!(session.backend_id(), "12345");
        assert_eq!(session.user(), "cde");
        assert_eq!(session.database(), "");
        assert_eq!(session.state(), "active");
        assert_eq!(session.query(), "");
        assert!(session.waiting());
        assert_eq!(session.wait_note(), "RowExclusiveLock relation");
        assert!(session.is_self());
    }

    #[test]
    fn mysql_session_from_parts_joins_command_and_state() {
        let session = mysql_session_from_parts(
            7,
            Some("root".to_owned()),
            Some("cde_test".to_owned()),
            Some("Query".to_owned()),
            Some("executing".to_owned()),
            Some("SELECT 1".to_owned()),
            false,
        )
        .unwrap();
        assert_eq!(session.backend_id(), "7");
        assert_eq!(session.user(), "root");
        assert_eq!(session.database(), "cde_test");
        assert_eq!(session.state(), "Query · executing");
        assert_eq!(session.query(), "SELECT 1");
        assert!(!session.waiting());
        assert_eq!(session.wait_note(), "");
        assert!(!session.is_self());
    }

    #[test]
    fn mysql_state_text_keeps_whichever_half_names_something() {
        assert_eq!(mysql_state_text(Some("Sleep".to_owned()), None), "Sleep");
        assert_eq!(mysql_state_text(None, Some("User sleep".to_owned())), "User sleep");
        assert_eq!(mysql_state_text(None, None), "");
        assert_eq!(
            mysql_state_text(Some("Query".to_owned()), Some("executing".to_owned())),
            "Query · executing"
        );
    }

    #[test]
    fn mysql_lock_waits_mark_requesting_backends_with_their_blocker() {
        let mut sessions = vec![
            mysql_session_from_parts(7, None, None, None, None, None, false).unwrap(),
            mysql_session_from_parts(9, None, None, None, None, None, false).unwrap(),
        ];
        apply_mysql_lock_waits(&mut sessions, &[(7, Some(9)), (11, None)]);
        assert!(sessions[0].waiting());
        assert_eq!(sessions[0].wait_note(), "blocked by 9");
        assert!(!sessions[1].waiting());
        assert_eq!(sessions[1].wait_note(), "");
    }

    #[test]
    fn backend_parsers_accept_positive_ids_and_reject_the_rest() {
        assert_eq!(parse_pg_backend("12345").unwrap(), 12345);
        assert_eq!(parse_pg_backend("  42 ").unwrap(), 42);
        assert_eq!(parse_mysql_backend("7").unwrap(), 7);
        for raw in ["", "  ", "abc", "0", "-3", "12x", "1.5"] {
            assert!(parse_pg_backend(raw).is_err(), "{raw}");
            assert!(parse_mysql_backend(raw).is_err(), "{raw}");
        }
        assert_eq!(
            parse_pg_backend("abc"),
            Err(DbError::Query("unknown backend abc".to_owned()))
        );
    }

    /// Lists the live backends and finds its own among them.
    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn pg_live_session_lists_its_own_backend() {
        let config = ConnectionConfig::Postgres { read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        };
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        let sessions = session.sessions().expect("list sessions");
        assert!(!sessions.is_empty());
        let own = sessions.iter().find(|s| s.is_self()).expect("own backend listed");
        assert!(!own.backend_id().is_empty());
        assert!(!own.user().is_empty());
    }

    /// Lists the live connections and finds its own among them.
    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn mysql_live_session_lists_its_own_backend() {
        let mut session = mysql_live_open();
        let sessions = session.sessions().expect("list sessions");
        assert!(!sessions.is_empty());
        let own = sessions.iter().find(|s| s.is_self()).expect("own backend listed");
        assert!(!own.backend_id().is_empty());
        assert!(!own.user().is_empty());
    }
}
