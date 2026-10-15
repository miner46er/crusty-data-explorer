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
}
