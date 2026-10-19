//! Graphical execution plan over EXPLAIN results.
//!
//! The model is pure over [`QueryResult`]: [`parse_plan`] reads the rows of
//! an EXPLAIN result into a [`PlanTree`] of costed nodes with the heavy path
//! marked, or returns None when the result is not a readable plan. The view
//! is a thin shell that renders the tree beside the grid rows with plain egui
//! shapes and no new dependencies. Unparseable results render nothing, so the
//! grid keeps its row rendering as the fallback. No worker change.

use eframe::egui;
use serde_json::Value as JsonValue;

use crate::app::Tab;
use crate::db::{QueryResult, Value};

use super::Palette;

/// Which engine shape the tree was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanEngine {
    PostgresText,
    PostgresJson,
    Mysql,
}

impl PlanEngine {
    fn label(self) -> &'static str {
        match self {
            PlanEngine::PostgresText => "Postgres plan",
            PlanEngine::PostgresJson => "Postgres JSON plan",
            PlanEngine::Mysql => "MySQL plan",
        }
    }
}

/// One plan step. `cost` is total cost on Postgres and estimated output rows
/// on MySQL, which has no cost column. `heavy` marks the costliest
/// root-to-leaf path, set by [`mark_heavy_path`].
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    pub operation: String,
    pub cost: Option<f64>,
    pub rows: Option<u64>,
    pub detail: String,
    pub children: Vec<PlanNode>,
    pub heavy: bool,
}

/// One parsed EXPLAIN result: the engine shape, its root, the root cost for
/// the bars, and the node count for the header.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanTree {
    pub engine: PlanEngine,
    pub root: PlanNode,
    pub total_cost: f64,
    pub node_count: usize,
}

/// Read an EXPLAIN result into a plan tree. Postgres text and JSON shapes and
/// MySQL traditional rows parse; anything else (including SQLite plans and
/// ordinary results) returns None so the caller keeps the row rendering.
pub fn parse_plan(result: &QueryResult) -> Option<PlanTree> {
    postgres_json(result)
        .or_else(|| postgres_text(result))
        .or_else(|| mysql_plan(result))
}

/// Full cell text. Text cells read whole; other types print through the
/// shared display with no cap.
fn cell_text(result: &QueryResult, row: usize, col: usize) -> String {
    match result.cell(row, col) {
        Some(Value::Text(text)) => text.clone(),
        Some(value) => value.display(usize::MAX).into_owned(),
        None => String::new(),
    }
}

/// Null, missing, and blank cells read as empty, so a MySQL NULL table does
/// not print as the word NULL inside an operation.
fn cell_blank(result: &QueryResult, row: usize, col: usize) -> String {
    match result.cell(row, col) {
        None | Some(Value::Null) => String::new(),
        Some(Value::Text(text)) => text.trim().to_owned(),
        Some(value) => value.display(usize::MAX).into_owned(),
    }
}

fn column_index(result: &QueryResult, name: &str) -> Option<usize> {
    result.columns().iter().position(|c| c.name().eq_ignore_ascii_case(name))
}

/// The comparison weight: cost first, rows as the fallback, zero when the
/// node carries no estimate.
fn weight(node: &PlanNode) -> f64 {
    node.cost.or(node.rows.map(|rows| rows as f64)).unwrap_or(0.0)
}

/// Mark the costliest root-to-leaf path. Ties keep the first child, so the
/// marking is deterministic on equal estimates.
fn mark_heavy_path(node: &mut PlanNode) {
    node.heavy = true;
    if node.children.is_empty() {
        return;
    }
    let mut best = 0;
    let mut best_weight = f64::NEG_INFINITY;
    for (index, child) in node.children.iter().enumerate() {
        if weight(child) > best_weight {
            best_weight = weight(child);
            best = index;
        }
    }
    for (index, child) in node.children.iter_mut().enumerate() {
        if index == best {
            mark_heavy_path(child);
        }
    }
}

fn count_nodes(node: &PlanNode) -> usize {
    1 + node.children.iter().map(count_nodes).sum::<usize>()
}

fn finish(engine: PlanEngine, mut root: PlanNode) -> PlanTree {
    let total_cost = root.cost.unwrap_or(0.0);
    mark_heavy_path(&mut root);
    let node_count = count_nodes(&root);
    PlanTree { engine, root, total_cost, node_count }
}

/// The EXPLAIN (FORMAT JSON) shape: one QUERY PLAN column holding a JSON
/// array with a Plan object.
fn postgres_json(result: &QueryResult) -> Option<PlanTree> {
    if result.columns().len() != 1 || result.row_count() == 0 {
        return None;
    }
    if !result.columns()[0].name().eq_ignore_ascii_case("QUERY PLAN") {
        return None;
    }
    let text: String =
        (0..result.row_count()).map(|row| cell_text(result, row, 0)).collect::<Vec<_>>().join("\n");
    let trimmed = text.trim();
    if !(trimmed.starts_with('[') || trimmed.starts_with('{')) {
        return None;
    }
    let value: JsonValue = serde_json::from_str(trimmed).ok()?;
    let root = json_node(json_root_plan(&value)?)?;
    Some(finish(PlanEngine::PostgresJson, root))
}

/// The Plan object inside a FORMAT JSON payload, whether wrapped in the
/// one-element array or handed over bare.
fn json_root_plan(value: &JsonValue) -> Option<&JsonValue> {
    match value {
        JsonValue::Array(items) => {
            items.first().and_then(|first| first.get("Plan")).or_else(|| items.first())
        }
        JsonValue::Object(_) => {
            if value.get("Node Type").is_some() { Some(value) } else { value.get("Plan") }
        }
        _ => None,
    }
}

/// One FORMAT JSON node. The operation names the node type and its relation;
/// the detail carries the join, index, and condition fields.
fn json_node(plan: &JsonValue) -> Option<PlanNode> {
    let node_type = plan.get("Node Type")?.as_str()?;
    let relation = plan.get("Relation Name").and_then(JsonValue::as_str).unwrap_or("");
    let operation = if relation.is_empty() {
        node_type.to_owned()
    } else {
        format!("{node_type} on {relation}")
    };
    let cost = plan.get("Total Cost").and_then(JsonValue::as_f64);
    let rows = plan
        .get("Plan Rows")
        .and_then(JsonValue::as_u64)
        .or_else(|| plan.get("Actual Rows").and_then(JsonValue::as_f64).map(|rows| rows as u64));
    let mut details = Vec::new();
    for key in ["Join Type", "Index Name", "Hash Cond", "Merge Cond", "Filter", "Sort Key"] {
        if let Some(text) = plan.get(key).and_then(JsonValue::as_str)
            && !text.is_empty()
        {
            details.push(format!("{key}: {text}"));
        }
    }
    let mut children = Vec::new();
    if let Some(plans) = plan.get("Plans").and_then(JsonValue::as_array) {
        for child in plans {
            if let Some(node) = json_node(child) {
                children.push(node);
            }
        }
    }
    Some(PlanNode {
        operation,
        cost,
        rows,
        detail: details.join(" · "),
        children,
        heavy: false,
    })
}

/// One indented text node plus the detail lines that follow it.
struct RawNode {
    indent: usize,
    operation: String,
    cost: Option<f64>,
    rows: Option<u64>,
    detail: String,
}

/// The Postgres text shape: one QUERY PLAN column of indented lines, where a
/// line with `cost=` opens a node and the other lines detail the nearest node
/// at or above their indent.
fn postgres_text(result: &QueryResult) -> Option<PlanTree> {
    let col = column_index(result, "QUERY PLAN")?;
    let mut lines = Vec::new();
    for row in 0..result.row_count() {
        for line in cell_text(result, row, col).split('\n') {
            lines.push(line.to_owned());
        }
    }
    if lines.iter().all(|line| line.trim().is_empty()) {
        return None;
    }
    let mut raw: Vec<RawNode> = Vec::new();
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for line in &lines {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let mut content = line.trim_start().to_owned();
        if let Some(rest) = content.strip_prefix("->") {
            content = rest.trim_start().to_owned();
        }
        if let Some((operation, cost, rows)) = split_cost_line(&content) {
            raw.push(RawNode { indent, operation, cost, rows, detail: String::new() });
            stack.push((indent, raw.len() - 1));
        } else if let Some((_, index)) =
            stack.iter().rev().find(|(depth, _)| *depth <= indent).copied()
        {
            let detail = &mut raw[index].detail;
            if !detail.is_empty() {
                detail.push_str(" · ");
            }
            detail.push_str(content.trim());
        }
    }
    if raw.is_empty() {
        return None;
    }
    let mut pos = 0;
    let mut tops = build_text_tree(&raw, &mut pos);
    if tops.len() == 1 {
        let root = tops.pop().expect("checked one top");
        Some(finish(PlanEngine::PostgresText, root))
    } else {
        let (cost_sum, rows_sum, rows_any) = tops.iter().fold(
            (0.0, 0u64, false),
            |(cost, rows, any), node| {
                (cost + node.cost.unwrap_or(0.0), rows + node.rows.unwrap_or(0), any || node.rows.is_some())
            },
        );
        Some(finish(
            PlanEngine::PostgresText,
            PlanNode {
                operation: "Plan".to_owned(),
                cost: Some(cost_sum),
                rows: rows_any.then_some(rows_sum),
                detail: String::new(),
                children: tops,
                heavy: false,
            },
        ))
    }
}

/// Split a `Seq Scan on users  (cost=0.00..35.50 rows=2550 width=68)` line
/// into its operation, total cost, and row estimate. None when the line
/// carries no cost tail.
fn split_cost_line(content: &str) -> Option<(String, Option<f64>, Option<u64>)> {
    let start = content.find("(cost=")?;
    let operation = content[..start].trim_end().to_owned();
    if operation.is_empty() {
        return None;
    }
    let tail = &content[start + 1..];
    let cost = tail.split_whitespace().find_map(|part| {
        let range = part.strip_prefix("cost=")?;
        let total = range.split_once("..").map(|(_, total)| total).unwrap_or(range);
        total.trim_end_matches(')').parse::<f64>().ok()
    });
    let rows = tail.split_whitespace().find_map(|part| {
        let rest = part.strip_prefix("rows=")?;
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() { None } else { digits.parse::<u64>().ok() }
    });
    Some((operation, cost, rows))
}

/// Nest the flat indented nodes: each node takes the following deeper nodes
/// as its children.
fn build_text_tree(items: &[RawNode], pos: &mut usize) -> Vec<PlanNode> {
    build_text_level(items, pos, -1)
}

fn build_text_level(items: &[RawNode], pos: &mut usize, parent_indent: isize) -> Vec<PlanNode> {
    let mut out = Vec::new();
    while *pos < items.len() {
        let indent = items[*pos].indent as isize;
        if indent <= parent_indent {
            break;
        }
        let index = *pos;
        *pos += 1;
        let children = build_text_level(items, pos, indent);
        out.push(PlanNode {
            operation: items[index].operation.clone(),
            cost: items[index].cost,
            rows: items[index].rows,
            detail: items[index].detail.clone(),
            children,
            heavy: false,
        });
    }
    out
}

/// The MySQL traditional shape: one flat row per step with select_type,
/// table, type, rows, and Extra columns. Steps become leaves under a
/// synthetic root; the rows estimate (times filtered) stands in for cost.
fn mysql_plan(result: &QueryResult) -> Option<PlanTree> {
    let select_col = column_index(result, "select_type")?;
    let table_col = column_index(result, "table")?;
    let access_col = column_index(result, "type")?;
    let rows_col = column_index(result, "rows");
    let filtered_col = column_index(result, "filtered");
    let extra_col = column_index(result, "extra");
    let key_col = column_index(result, "key");
    let ref_col = column_index(result, "ref");
    let possible_col = column_index(result, "possible_keys");
    let id_col = column_index(result, "id");
    if result.row_count() == 0 {
        return None;
    }
    let mut children = Vec::new();
    for row in 0..result.row_count() {
        let select = cell_blank(result, row, select_col);
        let table = cell_blank(result, row, table_col);
        let access = cell_blank(result, row, access_col);
        let mut operation = String::new();
        if !select.is_empty() {
            operation.push_str(&select);
        }
        if !table.is_empty() {
            if !operation.is_empty() {
                operation.push_str(" on ");
            }
            operation.push_str(&table);
        }
        if !access.is_empty() {
            if !operation.is_empty() {
                operation.push_str(" via ");
            }
            operation.push_str(&access);
        }
        if operation.is_empty() {
            continue;
        }
        if let Some(col) = id_col {
            let id = cell_blank(result, row, col);
            if !id.is_empty() {
                operation = format!("#{id} {operation}");
            }
        }
        let rows = rows_col.and_then(|col| parse_count(&cell_blank(result, row, col)));
        let filtered = filtered_col
            .and_then(|col| cell_blank(result, row, col).trim_end_matches('%').parse::<f64>().ok());
        let cost = rows.map(|count| count as f64 * filtered.unwrap_or(100.0) / 100.0);
        let mut details = Vec::new();
        if let Some(col) = extra_col {
            let extra = cell_blank(result, row, col);
            if !extra.is_empty() {
                details.push(extra);
            }
        }
        if let Some(col) = key_col {
            let key = cell_blank(result, row, col);
            if !key.is_empty() {
                details.push(format!("key: {key}"));
            }
        }
        if let Some(col) = ref_col {
            let reference = cell_blank(result, row, col);
            if !reference.is_empty() {
                details.push(format!("ref: {reference}"));
            }
        }
        if let Some(col) = possible_col {
            let possible = cell_blank(result, row, col);
            if !possible.is_empty() {
                details.push(format!("possible: {possible}"));
            }
        }
        if let Some(percent) = filtered {
            details.push(format!("filtered: {percent}%"));
        }
        children.push(PlanNode {
            operation,
            cost,
            rows,
            detail: details.join(" · "),
            children: Vec::new(),
            heavy: false,
        });
    }
    if children.is_empty() {
        return None;
    }
    let cost_sum: f64 = children.iter().map(|child| child.cost.unwrap_or(0.0)).sum();
    let rows_sum: u64 = children.iter().map(|child| child.rows.unwrap_or(0)).sum();
    let rows_any = children.iter().any(|child| child.rows.is_some());
    Some(finish(
        PlanEngine::Mysql,
        PlanNode {
            operation: "MySQL plan".to_owned(),
            cost: Some(cost_sum),
            rows: rows_any.then_some(rows_sum),
            detail: format!("{} steps", children.len()),
            children,
            heavy: false,
        },
    ))
}

/// Leading digits with commas stripped, so `1,024` and `10` read and `NULL`
/// or an empty cell does not.
fn parse_count(text: &str) -> Option<u64> {
    let plain: String = text.trim().chars().filter(|c| *c != ',').collect();
    let digits: String = plain.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() { None } else { digits.parse().ok() }
}

/// The plan above the grid rows. Renders nothing when the active result is
/// not a readable plan, which keeps the row rendering as the fallback.
pub fn plan_section(tab: &Tab, ui: &mut egui::Ui) {
    let Some(result) = tab.active_query_result() else { return };
    let Some(tree) = parse_plan(result) else { return };
    ui.push_id(tab.id, |ui| {
        let header = format!(
            "Plan · {} · {} nodes · total {:.2}",
            tree.engine.label(),
            tree.node_count,
            tree.total_cost
        );
        egui::CollapsingHeader::new(header).default_open(true).show(ui, |ui| {
            render_node(&tree.root, tree.total_cost, 0, 0, ui);
        });
    });
}

/// One node as a framed row with its cost share bar, then its children
/// indented beneath it. Heavy-path nodes carry the accent frame.
fn render_node(node: &PlanNode, total: f64, depth: usize, index: usize, ui: &mut egui::Ui) {
    let palette = Palette::of(ui.ctx().theme());
    let frame = if node.heavy {
        egui::Frame::group(ui.style()).stroke(egui::Stroke::new(2.0, palette.accent))
    } else {
        egui::Frame::group(ui.style())
    };
    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            if node.heavy {
                ui.label(egui::RichText::new(&node.operation).strong());
            } else {
                ui.label(&node.operation);
            }
            let mut stats = Vec::new();
            if let Some(cost) = node.cost {
                stats.push(format!("cost {cost:.2}"));
            }
            if let Some(rows) = node.rows {
                stats.push(format!("rows {rows}"));
            }
            if !stats.is_empty() {
                ui.label(egui::RichText::new(stats.join(" · ")).small().weak());
            }
            if node.heavy {
                ui.label(egui::RichText::new("heavy").small().strong());
            }
        });
        if !node.detail.is_empty() {
            ui.label(egui::RichText::new(&node.detail).small().weak());
        }
        cost_bar(weight(node), total, node.heavy, ui);
    });
    for (child_index, child) in node.children.iter().enumerate() {
        ui.indent(egui::Id::new(("crusty_plan", depth, index, child_index)), |ui| {
            render_node(child, total, depth + 1, child_index, ui);
        });
    }
}

/// The node's share of the plan total as a thin bar, accent on the heavy
/// path. Skipped when the total carries no estimate.
fn cost_bar(share: f64, total: f64, heavy: bool, ui: &mut egui::Ui) {
    if total <= 0.0 || share <= 0.0 {
        return;
    }
    let fraction = (share / total).clamp(0.0, 1.0) as f32;
    let width = ui.available_width().clamp(60.0, 280.0);
    let (response, painter) = ui.allocate_painter(egui::vec2(width, 6.0), egui::Sense::hover());
    painter.rect_filled(response.rect, 3.0, ui.visuals().faint_bg_color);
    let mut bar = response.rect;
    bar.max.x = bar.min.x + bar.width() * fraction;
    let color = if heavy { Palette::of(ui.ctx().theme()).accent } else { ui.visuals().weak_text_color() };
    painter.rect_filled(bar, 3.0, color);
    response.on_hover_text(format!("{share:.2} of {total:.2}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Column;
    use std::time::Duration;

    fn text_result(lines: &[&str]) -> QueryResult {
        QueryResult::from_columns(
            vec![Column::new(
                "QUERY PLAN".to_owned(),
                "TEXT".to_owned(),
                lines.iter().map(|line| Value::Text((*line).to_owned())).collect(),
            )],
            false,
            None,
            Duration::from_millis(1),
        )
        .unwrap()
    }

    fn result_with(columns: Vec<Column>) -> QueryResult {
        QueryResult::from_columns(columns, false, None, Duration::from_millis(1)).unwrap()
    }

    fn mysql_result(headers: &[&str], rows: Vec<Vec<Value>>) -> QueryResult {
        result_with(
            headers
                .iter()
                .enumerate()
                .map(|(index, name)| {
                    Column::new(
                        (*name).to_owned(),
                        "TEXT".to_owned(),
                        rows.iter().map(|row| row[index].clone()).collect(),
                    )
                })
                .collect(),
        )
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    #[test]
    fn postgres_text_parses_join_with_costs_and_rows() {
        let tree = parse_plan(&text_result(&[
            "Hash Join  (cost=37.50..70.00 rows=100 width=136)",
            "  ->  Seq Scan on orders  (cost=0.00..25.00 rows=1000 width=68)",
            "  Filter: (amount > 10)",
            "  ->  Hash  (cost=20.00..20.00 rows=1000 width=68)",
            "        ->  Seq Scan on customers  (cost=0.00..15.00 rows=500 width=68)",
            "  Hash Cond: (orders.customer_id = customers.id)",
        ]))
        .expect("join plan parses");
        assert_eq!(tree.engine, PlanEngine::PostgresText);
        assert_eq!(tree.root.operation, "Hash Join");
        assert_eq!(tree.root.cost, Some(70.0));
        assert_eq!(tree.root.rows, Some(100));
        assert_eq!(tree.node_count, 4);
        assert_eq!(tree.total_cost, 70.0);
        assert_eq!(tree.root.children.len(), 2);
        assert_eq!(tree.root.children[0].operation, "Seq Scan on orders");
        assert_eq!(tree.root.children[0].cost, Some(25.0));
        assert_eq!(tree.root.children[0].rows, Some(1000));
        assert!(tree.root.children[0].detail.contains("Filter: (amount > 10)"));
        assert_eq!(tree.root.children[1].operation, "Hash");
        assert!(tree.root.children[1].detail.contains("Hash Cond:"));
        assert_eq!(tree.root.children[1].children.len(), 1);
        assert_eq!(tree.root.children[1].children[0].operation, "Seq Scan on customers");
        assert_eq!(tree.root.children[1].children[0].cost, Some(15.0));
        assert_eq!(tree.root.children[1].children[0].rows, Some(500));
    }

    #[test]
    fn postgres_text_marks_the_costliest_branch_heavy() {
        let tree = parse_plan(&text_result(&[
            "Nested Loop  (cost=0.00..100.00 rows=10 width=64)",
            "  ->  Seq Scan on small  (cost=0.00..5.00 rows=10 width=32)",
            "  ->  Seq Scan on big  (cost=0.00..80.00 rows=10000 width=32)",
        ]))
        .expect("loop plan parses");
        assert!(tree.root.heavy);
        assert!(!tree.root.children[0].heavy);
        assert!(tree.root.children[1].heavy);
    }

    #[test]
    fn postgres_json_parses_nested_plans() {
        let payload = r#"[{"Plan": {"Node Type": "Hash Join", "Join Type": "Inner", "Total Cost": 70.0, "Plan Rows": 100, "Hash Cond": "(orders.customer_id = customers.id)", "Plans": [{"Node Type": "Seq Scan", "Relation Name": "orders", "Total Cost": 25.0, "Plan Rows": 1000}, {"Node Type": "Hash", "Total Cost": 20.0, "Plan Rows": 1000, "Plans": [{"Node Type": "Seq Scan", "Relation Name": "customers", "Total Cost": 15.0, "Plan Rows": 500}]}]}}]"#;
        let tree = parse_plan(&text_result(&[payload])).expect("json plan parses");
        assert_eq!(tree.engine, PlanEngine::PostgresJson);
        assert_eq!(tree.root.operation, "Hash Join");
        assert_eq!(tree.root.cost, Some(70.0));
        assert_eq!(tree.root.rows, Some(100));
        assert!(tree.root.detail.contains("Inner"));
        assert!(tree.root.detail.contains("Hash Cond"));
        assert_eq!(tree.node_count, 4);
        assert_eq!(tree.root.children.len(), 2);
        assert_eq!(tree.root.children[0].operation, "Seq Scan on orders");
        assert_eq!(tree.root.children[0].cost, Some(25.0));
        assert_eq!(tree.root.children[1].children[0].operation, "Seq Scan on customers");
        assert!(tree.root.heavy);
        assert!(tree.root.children[0].heavy);
        assert!(!tree.root.children[1].heavy);
    }

    #[test]
    fn mysql_traditional_builds_children_with_row_costs() {
        let tree = parse_plan(&mysql_result(
            &["id", "select_type", "table", "type", "key", "rows", "filtered", "Extra"],
            vec![
                vec![
                    Value::Integer(1),
                    text("SIMPLE"),
                    text("users"),
                    text("ALL"),
                    Value::Null,
                    Value::Integer(2550),
                    text("100.00"),
                    Value::Null,
                ],
                vec![
                    Value::Integer(1),
                    text("SIMPLE"),
                    text("orders"),
                    text("ref"),
                    text("idx_user"),
                    Value::Integer(10),
                    text("100.00"),
                    text("Using index"),
                ],
            ],
        ))
        .expect("mysql plan parses");
        assert_eq!(tree.engine, PlanEngine::Mysql);
        assert_eq!(tree.root.operation, "MySQL plan");
        assert_eq!(tree.root.children.len(), 2);
        assert_eq!(tree.root.children[0].operation, "#1 SIMPLE on users via ALL");
        assert_eq!(tree.root.children[0].rows, Some(2550));
        assert_eq!(tree.root.children[0].cost, Some(2550.0));
        assert_eq!(tree.root.children[1].operation, "#1 SIMPLE on orders via ref");
        assert_eq!(tree.root.children[1].rows, Some(10));
        assert!(tree.root.children[1].detail.contains("Using index"));
        assert!(tree.root.children[1].detail.contains("idx_user"));
        assert_eq!(tree.node_count, 3);
        assert!(tree.root.heavy);
        assert!(tree.root.children[0].heavy);
        assert!(!tree.root.children[1].heavy);
    }

    #[test]
    fn mysql_applies_filtered_to_the_cost_proxy() {
        let tree = parse_plan(&mysql_result(
            &["select_type", "table", "type", "rows", "filtered"],
            vec![vec![
                text("SIMPLE"),
                text("users"),
                text("ALL"),
                Value::Integer(1000),
                text("10.00"),
            ]],
        ))
        .expect("filtered plan parses");
        assert_eq!(tree.root.children[0].cost, Some(100.0));
        assert_eq!(tree.total_cost, 100.0);
    }

    #[test]
    fn garbage_falls_back_to_none() {
        let plain = result_with(vec![
            Column::new("name".to_owned(), "TEXT".to_owned(), vec![text("ada")]),
            Column::new("age".to_owned(), "INTEGER".to_owned(), vec![Value::Integer(36)]),
        ]);
        assert_eq!(parse_plan(&plain), None);
        let sqlite_shape = result_with(vec![
            Column::new("id".to_owned(), "INTEGER".to_owned(), vec![Value::Integer(2)]),
            Column::new("parent".to_owned(), "INTEGER".to_owned(), vec![Value::Integer(0)]),
            Column::new("notused".to_owned(), "INTEGER".to_owned(), vec![Value::Integer(0)]),
            Column::new("detail".to_owned(), "TEXT".to_owned(), vec![text("SCAN users")]),
        ]);
        assert_eq!(parse_plan(&sqlite_shape), None);
        let costless = text_result(&["Seq Scan on users", "  Filter: (id > 1)"]);
        assert_eq!(parse_plan(&costless), None);
        let empty = text_result(&[]);
        assert_eq!(parse_plan(&empty), None);
    }

    #[cfg(feature = "postgres")]
    #[test]
    #[ignore = "needs a live Postgres; set PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD"]
    fn plan_live_postgres_parses_real_explain() {
        use crate::db::{ConnectionConfig, PostgresSslMode, Session};

        let config = ConnectionConfig::Postgres {
            read_only: false,
            host: std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: std::env::var("PGPORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5432),
            database: std::env::var("PGDATABASE").unwrap_or_else(|_| "postgres".to_owned()),
            user: std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_owned()),
            ssl_mode: PostgresSslMode::Disable,
            ssh: None,
        };
        let password = std::env::var("PGPASSWORD").ok();
        let mut session = Session::open(&config, password.as_deref(), None).expect("open");
        session.query("CREATE TEMPORARY TABLE cde_plan_a (id INT)").expect("create probe a");
        session.query("CREATE TEMPORARY TABLE cde_plan_b (id INT)").expect("create probe b");
        let result = session
            .query("EXPLAIN SELECT * FROM cde_plan_a JOIN cde_plan_b ON cde_plan_a.id = cde_plan_b.id")
            .expect("explain runs");
        let tree = parse_plan(&result).expect("live plan parses");
        assert_eq!(tree.engine, PlanEngine::PostgresText);
        assert!(tree.node_count >= 3, "nodes: {}", tree.node_count);
        assert!(tree.total_cost > 0.0, "total: {}", tree.total_cost);
        assert!(tree.root.heavy);
        assert_eq!(tree.root.children.iter().filter(|child| child.heavy).count(), 1);
    }

    #[cfg(feature = "mysql")]
    #[test]
    #[ignore = "needs a live MySQL; set MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD"]
    fn plan_live_mysql_parses_real_explain() {
        // MySQL refuses EXPLAIN in the prepared statement protocol, so the
        // worker's describe-first path returns an affected count there. The
        // test fetches over the text protocol the way the worker runs
        // transaction controls, with no worker change.
        let host = std::env::var("MYSQLHOST").unwrap_or_else(|_| "localhost".to_owned());
        let port: u16 =
            std::env::var("MYSQLPORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3306);
        let database = std::env::var("MYSQLDATABASE").unwrap_or_else(|_| "cde_test".to_owned());
        let user = std::env::var("MYSQLUSER").unwrap_or_else(|_| "root".to_owned());
        let password = std::env::var("MYSQLPASSWORD").unwrap_or_default();
        let url = format!("mysql://{user}:{password}@{host}:{port}/{database}");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let pool = rt
            .block_on(
                sqlx::mysql::MySqlPoolOptions::new().max_connections(1).connect(&url),
            )
            .expect("connect");
        let flown = rt
            .block_on(
                sqlx::raw_sql(
                    "EXPLAIN SELECT * FROM (SELECT 1 AS a) t JOIN (SELECT 1 AS b) u ON true",
                )
                .fetch_all(&pool),
            )
            .expect("explain runs");
        assert!(!flown.is_empty());
        let result = live_mysql_result(&flown);
        let tree = parse_plan(&result).expect("live plan parses");
        assert_eq!(tree.engine, PlanEngine::Mysql);
        assert!(tree.node_count >= 2, "nodes: {}", tree.node_count);
        assert!(tree.root.heavy);
        assert_eq!(tree.root.children.iter().filter(|child| child.heavy).count(), 1);
    }

    /// One [`QueryResult`] from text-protocol rows, decoding through the same
    /// String, integer, real order the unit fixtures use.
    #[cfg(feature = "mysql")]
    fn live_mysql_result(rows: &[sqlx::mysql::MySqlRow]) -> QueryResult {
        use sqlx::{Column as _, Row as _, TypeInfo as _};

        let names: Vec<(String, String)> = rows[0]
            .columns()
            .iter()
            .map(|c| (c.name().to_owned(), c.type_info().name().to_owned()))
            .collect();
        result_with(
            names
                .iter()
                .enumerate()
                .map(|(index, (name, type_name))| {
                    Column::new(
                        name.clone(),
                        type_name.clone(),
                        rows.iter().map(|row| live_mysql_cell(row, index)).collect(),
                    )
                })
                .collect(),
        )
    }

    /// One text-protocol cell: text first, then integer, then real, else null.
    #[cfg(feature = "mysql")]
    fn live_mysql_cell(row: &sqlx::mysql::MySqlRow, index: usize) -> Value {
        use sqlx::Row as _;

        if let Ok(Some(text)) = row.try_get::<Option<String>, _>(index) {
            return Value::Text(text);
        }
        if let Ok(Some(number)) = row.try_get::<Option<i64>, _>(index) {
            return Value::Integer(number);
        }
        if let Ok(Some(number)) = row.try_get::<Option<f64>, _>(index) {
            return Value::Real(number);
        }
        Value::Null
    }
}
