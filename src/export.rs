use crate::db::{QueryResult, Value};

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

pub fn to_csv(result: &QueryResult, rows: &[usize]) -> String {
    let mut out = String::new();
    let header: Vec<String> = result
        .columns()
        .iter()
        .map(|c| escape_field(c.name()))
        .collect();
    out.push_str(&header.join(","));
    out.push_str("\r\n");
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

pub fn to_json(result: &QueryResult, rows: &[usize]) -> String {
    if rows.is_empty() {
        return "[]".to_owned();
    }
    let mut out = String::from("[\n");
    for (i, &r) in rows.iter().enumerate() {
        if result.columns().is_empty() {
            out.push_str("  {}");
        } else {
            out.push_str("  {\n");
            for (c, col) in result.columns().iter().enumerate() {
                let key =
                    serde_json::to_string(col.name()).unwrap_or_else(|_| "\"\"".to_owned());
                let val = value_to_json(result.cell(r, c));
                out.push_str(&format!("    {key}: {val}"));
                if c + 1 < result.columns().len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str("  }");
        }
        if i + 1 < rows.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push(']');
    out
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
    raw.replace("\r\n", "<br>").replace('\r', "<br>").replace('\n', "<br>").replace('|', "\\|")
}

pub fn to_markdown(result: &QueryResult, rows: &[usize]) -> String {
    let mut out = String::new();
    let header: Vec<String> =
        result.columns().iter().map(|c| escape_md_cell(c.name())).collect();
    out.push_str(&format!("| {} |\n", header.join(" | ")));
    let sep: Vec<String> = result.columns().iter().map(|_| "---".to_owned()).collect();
    out.push_str(&format!("| {} |\n", sep.join(" | ")));
    for &r in rows {
        let cells: Vec<String> =
            (0..result.columns().len()).map(|c| escape_md_cell(&md_raw(result.cell(r, c)))).collect();
        out.push_str(&format!("| {} |\n", cells.join(" | ")));
    }
    out
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
}
