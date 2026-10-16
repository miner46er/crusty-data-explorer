//! Read-only result chart beside the grid.
//!
//! The model is pure over [`QueryResult`]: [`plottable_columns`] names the
//! numeric columns, [`ChartSpec`] maps one label column and one numeric
//! column onto the axes, and [`series_points`] resolves the plottable rows.
//! View state lives in egui temp data keyed by tab beside the selection, so
//! no `AppState` field and no worker change. Rendering uses plain egui
//! shapes with no new dependencies.

use eframe::egui;

use crate::app::Tab;
use crate::db::{CELL_CAP, QueryResult, TabId, Value};

use super::Palette;

/// Bar or line rendering of one numeric series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChartKind {
    #[default]
    Bar,
    Line,
}

impl ChartKind {
    fn label(self) -> &'static str {
        match self {
            ChartKind::Bar => "Bar",
            ChartKind::Line => "Line",
        }
    }
}

/// Which columns feed the axes: any column labels, one numeric column values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChartSpec {
    pub kind: ChartKind,
    pub label_col: usize,
    pub value_col: usize,
}

/// One resolved series: row labels beside their numeric values. Null values
/// drop their rows, so a gap never plots as zero.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub labels: Vec<String>,
    pub values: Vec<f64>,
    pub dropped_nulls: usize,
    pub dropped_overflow: usize,
}

/// Rows drawn past this cap stay out with a "+N more" note, so a 10k-row
/// result never builds a 10k-shape frame.
pub const MAX_POINTS: usize = 200;

/// The f64 for a plottable cell. Null, text, and blobs are not numbers, even
/// when the text parses, so only typed numerics ever plot.
pub fn numeric_value(value: &Value) -> Option<f64> {
    match value {
        Value::Integer(n) => Some(*n as f64),
        Value::Real(f) => Some(*f),
        Value::Null | Value::Text(_) | Value::Blob(_) => None,
    }
}

/// True when the column holds at least one number and no text or blob.
/// All-null and empty columns are not plottable.
fn is_plottable(result: &QueryResult, col: usize) -> bool {
    let Some(column) = result.columns().get(col) else { return false };
    let mut seen = false;
    for value in column.values() {
        match value {
            Value::Integer(_) | Value::Real(_) => seen = true,
            Value::Null => {}
            Value::Text(_) | Value::Blob(_) => return false,
        }
    }
    seen
}

/// Indices of the numeric columns, in order.
pub fn plottable_columns(result: &QueryResult) -> Vec<usize> {
    (0..result.columns().len()).filter(|col| is_plottable(result, *col)).collect()
}

/// Indices of the listed-but-not-plottable columns, in order.
pub fn non_plottable_columns(result: &QueryResult) -> Vec<usize> {
    (0..result.columns().len()).filter(|col| !is_plottable(result, *col)).collect()
}

/// The empty state text, if the result cannot plot.
pub fn empty_text(result: &QueryResult) -> Option<&'static str> {
    if result.columns().is_empty() {
        return Some("No result to chart.");
    }
    if result.row_count() == 0 {
        return Some("No rows to chart.");
    }
    if plottable_columns(result).is_empty() {
        return Some("No numeric columns to chart.");
    }
    None
}

/// The first numeric column as the series and the first other column as the
/// labels. A lone numeric column labels itself. None when nothing plots.
pub fn default_spec(result: &QueryResult) -> Option<ChartSpec> {
    let plottable = plottable_columns(result);
    let value_col = *plottable.first()?;
    let label_col = (0..result.columns().len()).find(|col| *col != value_col).unwrap_or(value_col);
    Some(ChartSpec { kind: ChartKind::Bar, label_col, value_col })
}

/// Resolve one series. None when the value column is not plottable or either
/// index is out of range, so stale picker state never plots the wrong column.
pub fn series_points(
    result: &QueryResult,
    label_col: usize,
    value_col: usize,
) -> Option<Series> {
    if !plottable_columns(result).contains(&value_col) {
        return None;
    }
    let labels = result.columns().get(label_col)?;
    let values = result.columns().get(value_col)?;
    let mut series = Series {
        labels: Vec::new(),
        values: Vec::new(),
        dropped_nulls: 0,
        dropped_overflow: 0,
    };
    for row in 0..result.row_count() {
        let Some(number) = values.values().get(row).and_then(numeric_value) else {
            series.dropped_nulls += 1;
            continue;
        };
        if series.values.len() >= MAX_POINTS {
            series.dropped_overflow += 1;
            continue;
        }
        let label = labels
            .values()
            .get(row)
            .map(|value| value.display(CELL_CAP).into_owned())
            .unwrap_or_default();
        series.labels.push(label);
        series.values.push(number);
    }
    Some(series)
}

/// Per-tab picker state, kept in egui temp data beside the selection.
#[derive(Debug, Clone, Copy)]
struct ChartView {
    kind: ChartKind,
    label_col: usize,
    value_col: usize,
}

fn chart_view_id(tab: TabId) -> egui::Id {
    egui::Id::new(("crusty_chart_view", tab))
}

/// Stored pickers reconciled against the live result: stale indices fall back
/// to the default, so a new run never plots the wrong column.
fn chart_view_of(ctx: &egui::Context, tab: TabId, result: &QueryResult) -> Option<ChartView> {
    let fallback = default_spec(result)?;
    let stored: Option<ChartView> = ctx.data(|data| data.get_temp(chart_view_id(tab)));
    let valid = stored.is_some_and(|view| {
        view.label_col < result.columns().len()
            && plottable_columns(result).contains(&view.value_col)
    });
    Some(if valid {
        stored.expect("checked valid")
    } else {
        ChartView {
            kind: fallback.kind,
            label_col: fallback.label_col,
            value_col: fallback.value_col,
        }
    })
}

fn store_chart_view(ctx: &egui::Context, tab: TabId, view: ChartView) {
    ctx.data_mut(|data| data.insert_temp(chart_view_id(tab), view));
}

/// The read-only chart beside the grid: series pickers, a plain-shape canvas,
/// and the empty state for results with nothing to plot. Skips DDL-style
/// results with no columns entirely, so they keep the full width.
pub fn chart_side_panel(tab: &Tab, ui: &mut egui::Ui) {
    let Some(result) = tab.active_query_result() else { return };
    if result.columns().is_empty() {
        return;
    }
    egui::Panel::right("crusty_chart").default_size(320.0).show(ui, |ui| {
        chart_body(tab, result, ui);
    });
}

fn chart_body(tab: &Tab, result: &QueryResult, ui: &mut egui::Ui) {
    ui.heading("Chart");
    if let Some(text) = empty_text(result) {
        ui.label(egui::RichText::new(text).weak());
        parked_list(result, ui);
        return;
    }
    let Some(mut view) = chart_view_of(ui.ctx(), tab.id, result) else { return };
    ui.horizontal(|ui| {
        ui.label("Kind");
        for kind in [ChartKind::Bar, ChartKind::Line] {
            if ui.selectable_label(view.kind == kind, kind.label()).clicked() {
                view.kind = kind;
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label("Labels");
        egui::ComboBox::from_id_salt((tab.id, "chart_label"))
            .selected_text(result.columns()[view.label_col].name())
            .show_ui(ui, |ui| {
                for (index, column) in result.columns().iter().enumerate() {
                    ui.selectable_value(&mut view.label_col, index, column.name());
                }
            });
    });
    ui.horizontal(|ui| {
        ui.label("Series");
        egui::ComboBox::from_id_salt((tab.id, "chart_value"))
            .selected_text(result.columns()[view.value_col].name())
            .show_ui(ui, |ui| {
                for index in plottable_columns(result) {
                    ui.selectable_value(
                        &mut view.value_col,
                        index,
                        result.columns()[index].name(),
                    );
                }
            });
    });
    parked_list(result, ui);
    store_chart_view(ui.ctx(), tab.id, view);
    let spec = ChartSpec { kind: view.kind, label_col: view.label_col, value_col: view.value_col };
    match series_points(result, spec.label_col, spec.value_col) {
        None => {
            ui.label("The picked series left with the last run.");
        }
        Some(series) if series.values.is_empty() => {
            ui.label("The picked series has no numbers.");
        }
        Some(series) => {
            draw_series(spec.kind, &series, ui);
            let mut note = format!("{} plotted", series.values.len());
            if series.dropped_nulls > 0 {
                note += &format!(" · {} null skipped", series.dropped_nulls);
            }
            if series.dropped_overflow > 0 {
                note += &format!(" · +{} more", series.dropped_overflow);
            }
            ui.label(egui::RichText::new(note).small().weak());
        }
    }
}

/// The non-numeric columns, named so the picker exclusion is explicit.
fn parked_list(result: &QueryResult, ui: &mut egui::Ui) {
    let parked = non_plottable_columns(result);
    if parked.is_empty() {
        return;
    }
    let names: Vec<String> = parked
        .iter()
        .map(|col| {
            let column = &result.columns()[*col];
            format!("{} ({})", column.name(), column.type_name())
        })
        .collect();
    ui.label(egui::RichText::new(format!("Not plottable: {}", names.join(", "))).small().weak());
}

/// One bar or line series drawn with rects, segments, and points.
fn draw_series(kind: ChartKind, series: &Series, ui: &mut egui::Ui) {
    let width = ui.available_width().max(80.0);
    let height = 220.0;
    let (response, painter) = ui.allocate_painter(egui::vec2(width, height), egui::Sense::hover());
    let plot = response.rect.shrink2(egui::vec2(8.0, 20.0));
    let palette = Palette::of(ui.ctx().theme());
    let font = egui::TextStyle::Small.resolve(ui.style());
    let weak = ui.visuals().weak_text_color();
    let lo = series.values.iter().fold(0.0_f64, |a, b| a.min(*b));
    let mut hi = series.values.iter().fold(0.0_f64, |a, b| a.max(*b));
    if hi <= lo {
        hi = lo + 1.0;
    }
    let y_of = |value: f64| plot.bottom() - ((value - lo) / (hi - lo)) as f32 * plot.height();
    let n = series.values.len();
    let slot = plot.width() / n as f32;
    let x_of = |index: usize| match kind {
        ChartKind::Bar => plot.left() + (index as f32 + 0.5) * slot,
        ChartKind::Line if n == 1 => plot.center().x,
        ChartKind::Line => plot.left() + index as f32 / (n - 1) as f32 * plot.width(),
    };
    match kind {
        ChartKind::Bar => {
            let bar = slot * 0.7;
            for (index, value) in series.values.iter().enumerate() {
                let center = x_of(index);
                let top = y_of(value.max(0.0));
                let bottom = y_of(value.min(0.0));
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(center - bar / 2.0, top),
                        egui::pos2(center + bar / 2.0, bottom.max(top + 1.0)),
                    ),
                    egui::CornerRadius::ZERO,
                    palette.accent,
                );
            }
        }
        ChartKind::Line => {
            let points: Vec<egui::Pos2> = series
                .values
                .iter()
                .enumerate()
                .map(|(index, value)| egui::pos2(x_of(index), y_of(*value)))
                .collect();
            for window in points.windows(2) {
                painter.line_segment(
                    [window[0], window[1]],
                    egui::Stroke::new(2.0, palette.accent),
                );
            }
            for point in &points {
                painter.circle_filled(*point, 2.5, palette.accent);
            }
        }
    }
    painter.text(
        plot.left_top(),
        egui::Align2::LEFT_TOP,
        format!("{hi}"),
        font.clone(),
        weak,
    );
    painter.text(
        plot.left_bottom(),
        egui::Align2::LEFT_BOTTOM,
        format!("{lo}"),
        font.clone(),
        weak,
    );
    let ticks = 6usize.min(n);
    for tick in 0..ticks {
        let index = tick * (n - 1) / ticks.max(1);
        let x = x_of(index);
        let mut label: String = series.labels[index].chars().take(12).collect();
        if series.labels[index].chars().count() > 12 {
            label += "…";
        }
        painter.text(
            egui::pos2(x, response.rect.bottom() - 2.0),
            egui::Align2::CENTER_BOTTOM,
            label,
            font.clone(),
            weak,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Column;

    fn result(names: &[(&str, &str)], values: Vec<Vec<Value>>) -> QueryResult {
        QueryResult::from_columns(
            names
                .iter()
                .zip(values)
                .map(|((name, type_name), column)| {
                    Column::new((*name).to_owned(), (*type_name).to_owned(), column)
                })
                .collect(),
            false,
            None,
            std::time::Duration::from_millis(1),
        )
        .unwrap()
    }

    fn mixed_result() -> QueryResult {
        result(
            &[("name", "TEXT"), ("score", "INTEGER"), ("ratio", "REAL"), ("raw", "BLOB")],
            vec![
                vec![
                    Value::Text("ada".to_owned()),
                    Value::Text("grace".to_owned()),
                    Value::Text("lin".to_owned()),
                ],
                vec![Value::Integer(3), Value::Null, Value::Integer(7)],
                vec![Value::Real(0.5), Value::Real(2.5), Value::Real(1.5)],
                vec![Value::Blob(vec![1]), Value::Blob(vec![2]), Value::Blob(vec![3])],
            ],
        )
    }

    #[test]
    fn plottable_keeps_integer_and_real_only() {
        assert_eq!(plottable_columns(&mixed_result()), vec![1, 2]);
    }

    #[test]
    fn parked_lists_text_blob_and_skips_nothing_numeric() {
        assert_eq!(non_plottable_columns(&mixed_result()), vec![0, 3]);
    }

    #[test]
    fn all_null_column_is_parked() {
        let empty = result(
            &[("n", "INTEGER")],
            vec![vec![Value::Null, Value::Null]],
        );
        assert_eq!(plottable_columns(&empty), Vec::<usize>::new());
        assert_eq!(non_plottable_columns(&empty), vec![0]);
    }

    #[test]
    fn default_spec_pairs_first_other_column_with_first_numeric() {
        assert_eq!(
            default_spec(&mixed_result()),
            Some(ChartSpec { kind: ChartKind::Bar, label_col: 0, value_col: 1 })
        );
    }

    #[test]
    fn default_spec_labels_a_lone_numeric_with_itself() {
        let lone = result(&[("n", "INTEGER")], vec![vec![Value::Integer(1)]]);
        assert_eq!(
            default_spec(&lone),
            Some(ChartSpec { kind: ChartKind::Bar, label_col: 0, value_col: 0 })
        );
    }

    #[test]
    fn default_spec_is_none_without_numerics() {
        let text = result(
            &[("name", "TEXT")],
            vec![vec![Value::Text("ada".to_owned())]],
        );
        assert_eq!(default_spec(&text), None);
    }

    #[test]
    fn series_maps_labels_to_numbers_and_skips_nulls() {
        let series = series_points(&mixed_result(), 0, 1).unwrap();
        assert_eq!(series.labels, vec!["ada".to_owned(), "lin".to_owned()]);
        assert_eq!(series.values, vec![3.0, 7.0]);
        assert_eq!(series.dropped_nulls, 1);
        assert_eq!(series.dropped_overflow, 0);
    }

    #[test]
    fn series_reads_reals_and_integer_labels() {
        let series = series_points(&mixed_result(), 1, 2).unwrap();
        assert_eq!(series.labels, vec!["3".to_owned(), "NULL".to_owned(), "7".to_owned()]);
        assert_eq!(series.values, vec![0.5, 2.5, 1.5]);
    }

    #[test]
    fn series_rejects_text_values_and_bad_indices() {
        assert_eq!(series_points(&mixed_result(), 0, 0), None);
        assert_eq!(series_points(&mixed_result(), 0, 9), None);
        assert_eq!(series_points(&mixed_result(), 9, 1), None);
    }

    #[test]
    fn series_caps_long_results() {
        let many: Vec<Value> = (0..250).map(Value::Integer).collect();
        let labels: Vec<Value> =
            (0..250).map(|n| Value::Text(format!("r{n}"))).collect();
        let wide = result(&[("label", "TEXT"), ("n", "INTEGER")], vec![labels, many]);
        let series = series_points(&wide, 0, 1).unwrap();
        assert_eq!(series.values.len(), MAX_POINTS);
        assert_eq!(series.values[0], 0.0);
        assert_eq!(series.values[MAX_POINTS - 1], (MAX_POINTS - 1) as f64);
        assert_eq!(series.dropped_overflow, 50);
    }

    #[test]
    fn empty_states_name_the_missing_piece() {
        let no_columns = QueryResult::from_columns(
            Vec::new(),
            false,
            Some(3),
            std::time::Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(empty_text(&no_columns), Some("No result to chart."));
        let no_rows = result(
            &[("name", "TEXT"), ("n", "INTEGER")],
            vec![Vec::new(), Vec::new()],
        );
        assert_eq!(empty_text(&no_rows), Some("No rows to chart."));
        let no_numeric = result(
            &[("name", "TEXT")],
            vec![vec![Value::Text("ada".to_owned())]],
        );
        assert_eq!(empty_text(&no_numeric), Some("No numeric columns to chart."));
        assert_eq!(empty_text(&mixed_result()), None);
    }

    #[test]
    fn numbers_read_integers_reals_and_nothing_else() {
        assert_eq!(numeric_value(&Value::Integer(-4)), Some(-4.0));
        assert_eq!(numeric_value(&Value::Real(2.5)), Some(2.5));
        assert_eq!(numeric_value(&Value::Null), None);
        assert_eq!(numeric_value(&Value::Text("3".to_owned())), None);
        assert_eq!(numeric_value(&Value::Blob(vec![3])), None);
    }
}
