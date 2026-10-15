//! Inline editing: which SELECTs target one table, and how a staged change
//! renders as SQL.
//!
//! Pure text, no engine calls. `app.rs` decides when an edit may be staged
//! and sends the statements this module builds.

/// Some(table name) only for a single-table SELECT: a plain or double-quoted
/// identifier after FROM, with WHERE, GROUP, ORDER, HAVING, LIMIT, or OFFSET
/// allowed to follow it. Joins, unions, comma sources, parenthesized FROM
/// clauses, aliases, qualified names, and anything but a SELECT are not
/// editable.
pub fn editable_table(sql: &str) -> Option<String> {
    let tokens = lex(sql);
    if !matches!(tokens.first(), Some(Token::Word(w)) if w.eq_ignore_ascii_case("select")) {
        return None;
    }
    let mut index = 1;
    let (table, next) = loop {
        match tokens.get(index) {
            Some(Token::Word(word)) if word.eq_ignore_ascii_case("from") => {
                match tokens.get(index + 1) {
                    Some(Token::Word(name)) => break (name.clone(), index + 2),
                    Some(Token::Quoted(name)) => break (name.clone(), index + 2),
                    _ => return None,
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

/// The clause keywords a single-table SELECT may continue with.
fn is_trailing_clause(word: &str) -> bool {
    ["where", "group", "order", "having", "limit", "offset"]
        .iter()
        .any(|keyword| word.eq_ignore_ascii_case(keyword))
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    /// An unquoted word: identifier or keyword.
    Word(String),
    /// A double-quoted identifier, doubled quotes unescaped, so it never
    /// reads as a keyword.
    Quoted(String),
    /// A single-quoted string literal; its contents never matter here.
    Str,
    /// Any other single character: punctuation and operators.
    Punct(char),
}

fn lex(sql: &str) -> Vec<Token> {
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
        } else if c == '"' {
            let mut text = String::new();
            i += 1;
            while i < chars.len() {
                if chars[i] == '"' {
                    if chars.get(i + 1) == Some(&'"') {
                        text.push('"');
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
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() {
                if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    i += 2;
                    break;
                }
                i += 1;
            }
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

/// One cell text as a SQL literal. An empty text or a value the caller knows
/// to be NULL renders as `NULL`; a text that parses as a number stays bare;
/// everything else is single-quoted with quotes doubled.
pub fn to_literal(text: &str, was_null: bool) -> String {
    if text.is_empty() || was_null {
        return "NULL".to_owned();
    }
    if numeric_literal(text) {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', "''"))
}

/// True when `text` is a spelling of a number SQL accepts. The character
/// gate keeps the spellings `f64` parses but SQL does not, such as `inf`
/// and `NaN`, out.
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
    text.parse::<i64>().is_ok() || text.parse::<f64>().is_ok()
}

/// `UPDATE "table" SET "col" = <literal>, ... WHERE "key" = <literal> AND ...`
/// Set values are raw text and render through [`to_literal`], so an empty
/// value is NULL. Key values are literals already, rendered by the caller
/// with [`to_literal`], because only the caller can tell a NULL cell from a
/// text cell reading `NULL`.
pub fn update_statement(table: &str, pk: &[(String, String)], sets: &[(String, String)]) -> String {
    let sets = sets
        .iter()
        .map(|(column, text)| format!("{} = {}", ident(column), to_literal(text, false)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("UPDATE {} SET {} WHERE {}", ident(table), sets, predicate(pk))
}

/// `DELETE FROM "table" WHERE "key" = <literal> AND ...` with the key values
/// rendered by the caller through [`to_literal`].
pub fn delete_statement(table: &str, pk: &[(String, String)]) -> String {
    format!("DELETE FROM {} WHERE {}", ident(table), predicate(pk))
}

/// `INSERT INTO "table" ("col", ...) VALUES (<literal>, ...)`. Values are raw
/// text and render through [`to_literal`], so an empty value is NULL.
pub fn insert_statement(table: &str, columns: &[String], values: &[String]) -> String {
    let columns = columns.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", ");
    let values = values.iter().map(|v| to_literal(v, false)).collect::<Vec<_>>().join(", ");
    format!("INSERT INTO {} ({}) VALUES ({})", ident(table), columns, values)
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
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
        editable_table(sql)
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
            "SELECT * FROM main.users",
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
        assert_eq!(table_of("SELECT * FROM \"odd\"\"name\""), Some("odd\"name".to_owned()));
        // A quoted identifier is not the FROM keyword.
        assert_eq!(table_of("SELECT \"from\" FROM users").as_deref(), Some("users"));
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
    }
}
