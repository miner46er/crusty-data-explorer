use std::path::PathBuf;

use crusty_data_explorer::db::{ConnectionConfig, ROW_CAP, Session, Value};

#[test]
fn sqlite_in_memory_roundtrip() {
    let config = ConnectionConfig::Sqlite { path: PathBuf::from(":memory:") };
    let mut session = Session::open(&config, None, None).expect("open in-memory SQLite");

    let created = session
        .query("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        .expect("create table");
    assert_eq!(created.affected(), Some(0));

    let inserted = session
        .query("INSERT INTO users (name) VALUES ('Ada'), ('Grace')")
        .expect("insert rows");
    assert_eq!(inserted.affected(), Some(2));

    let result = session
        .query("SELECT id, name FROM users ORDER BY id")
        .expect("select rows");
    assert_eq!(result.row_count(), 2);
    assert_eq!(result.columns().len(), 2);
    assert_eq!(result.columns()[0].name(), "id");
    assert_eq!(result.columns()[1].name(), "name");
    assert_eq!(result.cell(0, 0), Some(&Value::Integer(1)));
    assert_eq!(result.cell(0, 1), Some(&Value::Text("Ada".to_owned())));
    assert_eq!(result.cell(1, 0), Some(&Value::Integer(2)));
    assert_eq!(result.cell(1, 1), Some(&Value::Text("Grace".to_owned())));
    assert_eq!(result.cell(2, 0), None);
    assert_eq!(result.affected(), None);
    assert!(!result.is_truncated());

    let nulls = session.query("SELECT 1, 'a', NULL, x'0102'").expect("mixed row");
    assert_eq!(nulls.cell(0, 0), Some(&Value::Integer(1)));
    assert_eq!(nulls.cell(0, 1), Some(&Value::Text("a".to_owned())));
    assert_eq!(nulls.cell(0, 2), Some(&Value::Null));
    assert_eq!(nulls.cell(0, 3), Some(&Value::Blob(vec![0x01, 0x02])));
    assert_eq!(
        nulls.cell(0, 3).unwrap().display(256).into_owned(),
        "[BLOB 2 B]"
    );
}

#[test]
fn row_cap_truncates() {
    let config = ConnectionConfig::Sqlite { path: PathBuf::from(":memory:") };
    let mut session = Session::open(&config, None, None).expect("open in-memory SQLite");

    let sql = format!(
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < {}) SELECT n FROM c",
        ROW_CAP + 5
    );
    let result = session.query(&sql).expect("over-cap select");
    assert_eq!(result.row_count(), ROW_CAP);
    assert!(result.is_truncated());
    assert_eq!(result.affected(), None);
}
