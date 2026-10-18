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
    /// Each value's own display string, the text the grid shows, so the
    /// tooltip reads an integer past 2^53 exactly instead of the rounded
    /// f64 conversion.
    pub value_labels: Vec<String>,
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
        value_labels: Vec::new(),
        dropped_nulls: 0,
        dropped_overflow: 0,
    };
    for row in 0..result.row_count() {
        let Some(value) = values.values().get(row) else {
            series.dropped_nulls += 1;
            continue;
        };
        let Some(number) = numeric_value(value) else {
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
        series.value_labels.push(value.display(CELL_CAP).into_owned());
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
/// and the empty state for results with nothing to plot. Renders only while
/// the tab's side pane is Chart, so the grid keeps the full width otherwise;
/// skips DDL-style results with no columns.
pub fn chart_side_panel(tab: &Tab, ui: &mut egui::Ui) {
    if super::grid::side_pane_of(ui.ctx(), tab.id) != super::grid::SidePane::Chart {
        return;
    }
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

/// Evenly spaced tick positions over `n` points, both ends included without
/// repeats. Empty for an empty series; `ticks` clamps to `n`, and the spread
/// rounds to the nearest point so a short span does not bunch ticks on the
/// left.
fn tick_indices(n: usize, ticks: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let ticks = ticks.min(n).max(1);
    let last = n.saturating_sub(1);
    let span = ticks.saturating_sub(1).max(1);
    (0..ticks).map(|i| (i * last + span / 2) / span).collect()
}

/// The x for every series point: bar centers inset by half a slot, line
/// points spanning `lo..hi`, a lone line point centered. Empty with no
/// points.
fn tick_xs(kind: ChartKind, n: usize, lo: f32, hi: f32) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }
    let span = hi - lo;
    match kind {
        ChartKind::Bar => {
            let slot = span / n as f32;
            (0..n).map(|i| lo + (i as f32 + 0.5) * slot).collect()
        }
        ChartKind::Line if n == 1 => vec![lo + span / 2.0],
        ChartKind::Line => {
            (0..n).map(|i| lo + i as f32 / (n - 1) as f32 * span).collect()
        }
    }
}

/// The x positions of the drawn ticks: the point mapping sampled at
/// [`tick_indices`], so the drawing path and its tests share one mapping.
fn drawn_tick_xs(kind: ChartKind, n: usize, lo: f32, hi: f32) -> Vec<f32> {
    let xs = tick_xs(kind, n, lo, hi);
    tick_indices(n, 6).iter().map(|index| xs[*index]).collect()
}

/// The series index nearest `pointer_x` on the x mapping both chart kinds
/// share. An exact tie (equal distance to two points) goes to the earlier
/// point, so a hover between a bar and its left neighbour reads the left one.
fn nearest_index(pointer_x: f32, xs: &[f32]) -> Option<usize> {
    let mut nearest = None;
    let mut best = f32::INFINITY;
    for (index, x) in xs.iter().enumerate() {
        let distance = (pointer_x - x).abs();
        if distance < best {
            best = distance;
            nearest = Some(index);
        }
    }
    nearest
}

/// One tick's x, the room its label may use on each side, and the total
/// width those rooms allow.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TickPlacement {
    x: f32,
    left: f32,
    right: f32,
    budget: f32,
}

/// One placement per tick x. Each adjacent pair splits the free distance
/// between them, half per side, so two labels can never claim the same room;
/// an end tick takes its full distance to the plot edge as outer room, which
/// is zero for a line endpoint.
fn tick_placements(xs: &[f32], lo: f32, hi: f32, gap: f32) -> Vec<TickPlacement> {
    let last = xs.len().saturating_sub(1);
    xs.iter()
        .enumerate()
        .map(|(i, &x)| {
            let left = if i == 0 { x - lo } else { (x - xs[i - 1] - gap) / 2.0 };
            let right = if i == last { hi - x } else { (xs[i + 1] - x - gap) / 2.0 };
            TickPlacement { x, left, right, budget: (left + right).max(0.0) }
        })
        .collect()
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
    let xs = tick_xs(kind, n, plot.left(), plot.right());
    match kind {
        ChartKind::Bar => {
            let bar = plot.width() / n as f32 * 0.7;
            for (index, value) in series.values.iter().enumerate() {
                let center = xs[index];
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
            let points: Vec<egui::Pos2> = xs
                .iter()
                .zip(&series.values)
                .map(|(x, value)| egui::pos2(*x, y_of(*value)))
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
    let ticks = tick_indices(n, 6);
    let placements = tick_placements(
        &drawn_tick_xs(kind, n, plot.left(), plot.right()),
        plot.left(),
        plot.right(),
        ui.spacing().item_spacing.x,
    );
    for (index, placement) in ticks.iter().zip(placements) {
        let mut job = egui::text::LayoutJob::default();
        job.append(
            &series.labels[*index],
            0.0,
            egui::TextFormat { font_id: font.clone(), color: weak, ..Default::default() },
        );
        job.wrap = egui::text::TextWrapping::truncate_at_width(placement.budget);
        let galley = painter.layout_job(job);
        let left_limit = placement.x - placement.left;
        let right_limit = (placement.x + placement.right - galley.size().x).max(left_limit);
        let pos = egui::pos2(
            (placement.x - galley.size().x / 2.0).clamp(left_limit, right_limit),
            response.rect.bottom() - 2.0 - galley.size().y,
        );
        painter.galley(pos, galley, weak);
    }
    if let Some(pointer) = response.hover_pos()
        && let Some(index) = nearest_index(pointer.x, &xs)
    {
        response.on_hover_ui_at_pointer(|ui| {
            ui.set_max_width(ui.spacing().tooltip_width);
            // The tick text elides to its room; the tooltip shows the
            // label at its 256-character cap and the value's own display
            // string, so a large integer reads as the grid shows it rather
            // than rounded through f64.
            ui.label(format!("{} · {}", series.labels[index], series.value_labels[index]));
        });
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
    fn series_keeps_each_values_own_display_text() {
        let big = 9_007_199_254_740_993_i64; // 2^53 + 1, not an f64
        let result = result(
            &[("label", "TEXT"), ("n", "INTEGER")],
            vec![vec![Value::Text("ada".to_owned())], vec![Value::Integer(big)]],
        );
        let series = series_points(&result, 0, 1).unwrap();
        assert_eq!(series.values, vec![big as f64], "the plot still reads f64");
        assert_eq!(series.value_labels, vec![big.to_string()], "the tooltip does not");
        assert_eq!(series.value_labels[0], "9007199254740993");
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

    #[test]
    fn tick_indices_label_a_single_point() {
        assert_eq!(tick_indices(1, 1), vec![0]);
    }

    #[test]
    fn tick_indices_label_both_ends_of_two_points() {
        assert_eq!(tick_indices(2, 2), vec![0, 1]);
    }

    #[test]
    fn tick_indices_label_every_point_when_ticks_match_points() {
        assert_eq!(tick_indices(5, 5), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn tick_indices_spread_across_seven_points() {
        assert_eq!(tick_indices(7, 6), vec![0, 1, 2, 4, 5, 6]);
    }

    #[test]
    fn tick_indices_reach_the_last_point_of_a_long_series() {
        assert_eq!(tick_indices(200, 6), vec![0, 40, 80, 119, 159, 199]);
    }

    #[test]
    fn tick_indices_never_repeat_when_ticks_exceed_points() {
        assert_eq!(tick_indices(3, 6), vec![0, 1, 2]);
    }

    #[test]
    fn tick_indices_are_empty_without_points() {
        assert_eq!(tick_indices(0, 6), Vec::<usize>::new());
    }

    #[test]
    fn tick_xs_inset_bar_centers() {
        assert_eq!(tick_xs(ChartKind::Bar, 4, 0.0, 100.0), vec![12.5, 37.5, 62.5, 87.5]);
    }

    #[test]
    fn tick_xs_span_line_endpoints_and_center_a_lone_point() {
        assert_eq!(tick_xs(ChartKind::Line, 3, 0.0, 100.0), vec![0.0, 50.0, 100.0]);
        assert_eq!(tick_xs(ChartKind::Line, 1, 0.0, 100.0), vec![50.0]);
    }

    #[test]
    fn tick_xs_are_empty_without_points() {
        assert_eq!(tick_xs(ChartKind::Bar, 0, 0.0, 100.0), Vec::<f32>::new());
        assert_eq!(tick_xs(ChartKind::Line, 0, 0.0, 100.0), Vec::<f32>::new());
    }

    #[test]
    fn nearest_index_is_none_without_points() {
        assert_eq!(nearest_index(12.0, &[]), None);
    }

    #[test]
    fn nearest_index_clamps_to_the_ends() {
        let xs = [0.0, 10.0, 20.0];
        assert_eq!(nearest_index(0.0, &xs), Some(0));
        assert_eq!(nearest_index(-5.0, &xs), Some(0));
        assert_eq!(nearest_index(20.0, &xs), Some(2));
        assert_eq!(nearest_index(40.0, &xs), Some(2));
    }

    #[test]
    fn nearest_index_picks_the_closer_point() {
        let xs = [0.0, 10.0, 20.0];
        assert_eq!(nearest_index(4.0, &xs), Some(0));
        assert_eq!(nearest_index(6.0, &xs), Some(1));
        assert_eq!(nearest_index(19.0, &xs), Some(2));
    }

    #[test]
    fn nearest_index_ties_go_to_the_earlier_point() {
        let xs = [0.0, 10.0, 20.0];
        assert_eq!(nearest_index(5.0, &xs), Some(0));
        assert_eq!(nearest_index(15.0, &xs), Some(1));
        let bars = tick_xs(ChartKind::Bar, 4, 0.0, 100.0);
        assert_eq!(nearest_index(25.0, &bars), Some(0));
    }

    #[test]
    fn nearest_index_reads_both_kind_mappings_alike() {
        let bars = tick_xs(ChartKind::Bar, 4, 0.0, 100.0);
        assert_eq!(nearest_index(30.0, &bars), Some(1));
        let line = tick_xs(ChartKind::Line, 3, 0.0, 100.0);
        assert_eq!(nearest_index(60.0, &line), Some(1));
    }

    /// Below this budget a tick label is a stub rather than a label; the
    /// sweep is the yardstick for keeping endpoints out of that class.
    const MIN_TICK_LABEL_WIDTH: f32 = 16.0;

    fn budgets(placements: &[TickPlacement]) -> Vec<f32> {
        placements.iter().map(|p| p.budget).collect()
    }

    #[test]
    fn tick_placements_give_the_edge_labels_room() {
        let placements = tick_placements(&[0.0, 50.0, 100.0], 0.0, 100.0, 8.0);
        assert_eq!(budgets(&placements), vec![21.0, 42.0, 21.0]);
    }

    #[test]
    fn tick_placements_keep_even_line_labels_apart() {
        let placements = tick_placements(&[0.0, 71.0, 142.0, 213.0, 284.0], 0.0, 284.0, 8.0);
        assert_eq!(budgets(&placements), vec![31.5, 63.0, 63.0, 63.0, 31.5]);
    }

    #[test]
    fn tick_placements_split_a_two_point_line_between_the_ends() {
        let placements = tick_placements(&[0.0, 284.0], 0.0, 284.0, 8.0);
        assert_eq!(budgets(&placements), vec![138.0, 138.0]);
    }

    #[test]
    fn tick_placements_center_inset_bar_ticks() {
        let placements = tick_placements(&[40.0, 120.0, 200.0, 280.0, 360.0], 0.0, 400.0, 8.0);
        assert_eq!(budgets(&placements), vec![76.0, 72.0, 72.0, 72.0, 76.0]);
    }

    #[test]
    fn tick_placements_keep_the_bar_budget_pin() {
        let xs = [28.75, 86.25, 143.75, 201.25, 258.75];
        let placements = tick_placements(&xs, 0.0, 287.5, 8.0);
        assert_eq!(budgets(&placements), vec![53.5, 49.5, 49.5, 49.5, 53.5]);
    }

    #[test]
    fn tick_placements_use_both_sides_for_a_single_tick() {
        let placements = tick_placements(&[50.0], 0.0, 100.0, 5.0);
        assert_eq!(placements[0].budget, 100.0);
    }

    #[test]
    fn tick_placements_collapse_a_pair_tighter_than_the_gap() {
        let placements = tick_placements(&[50.0, 52.0], 50.0, 52.0, 8.0);
        assert_eq!(budgets(&placements), vec![0.0, 0.0]);
    }

    #[test]
    fn dense_bar_endpoints_clear_the_label_floor() {
        // The A3-1 geometry: a 384px plot with 40 bars. The centred model
        // turned each endpoint budget into 9.6 and skipped both labels.
        let placements = tick_placements(
            &drawn_tick_xs(ChartKind::Bar, 40, 0.0, 384.0),
            0.0,
            384.0,
            8.0,
        );
        assert_eq!(placements.len(), 6);
        for endpoint in [placements.first().unwrap(), placements.last().unwrap()] {
            assert!(
                endpoint.budget >= MIN_TICK_LABEL_WIDTH,
                "a dense bar endpoint holds {:.1}px, under the floor",
                endpoint.budget
            );
        }
    }

    #[test]
    fn tick_placements_leave_the_gap_between_every_pair() {
        let mut geometries = 0usize;
        let mut pairs = 0usize;
        for kind in [ChartKind::Bar, ChartKind::Line] {
            for n in 1..=200usize {
                for width in [64.0f32, 150.0, 284.0, 400.0, 1400.0] {
                    geometries += 1;
                    let placements =
                        tick_placements(&drawn_tick_xs(kind, n, 0.0, width), 0.0, width, 8.0);
                    for placement in &placements {
                        assert!(
                            placement.x - placement.left >= -0.001
                                && placement.x + placement.right <= width + 0.001,
                            "{kind:?} n={n} width={width}: a label leaves the plot"
                        );
                    }
                    // Strict floor parity cannot hold: an endpoint's inner
                    // share is half its neighbour's pair share, so a 150px
                    // 8-point line has 6.7px endpoints beside 24.1px
                    // interiors. The form that does hold is that no endpoint
                    // is dropped to a zero budget while other labels draw.
                    if placements.iter().any(|p| p.budget >= MIN_TICK_LABEL_WIDTH) {
                        for endpoint in
                            [placements.first().unwrap(), placements.last().unwrap()]
                        {
                            assert!(
                                endpoint.budget > 0.0,
                                "{kind:?} n={n} width={width}: an endpoint label collapses"
                            );
                        }
                    }
                    for pair in placements.windows(2) {
                        if pair[0].budget <= 0.0 || pair[1].budget <= 0.0 {
                            continue;
                        }
                        pairs += 1;
                        let gap = (pair[1].x - pair[1].left) - (pair[0].x + pair[0].right);
                        assert!(
                            gap >= 8.0 - 0.001,
                            "{kind:?} n={n} width={width}: adjacent labels leave {gap:.3}px, under the 8px gap"
                        );
                    }
                }
            }
        }
        println!("swept {geometries} geometries, asserted {pairs} adjacent pairs");
    }
}
