//! Rendering only. Reads state through accessors, changes it through
//! transitions. Panel order is call order; `CentralPanel` is always last.

use eframe::egui;

use crate::app::{AppState, ConnectionState};

pub mod charts;
pub mod compare;
pub mod complete;
pub mod connections;
pub mod ddl;
pub mod editor;
pub mod fit;
pub mod grid;
pub mod plan;
pub mod query;
pub mod schema;
pub mod search;
pub mod table_edit;

pub fn root(state: &mut AppState, ui: &mut egui::Ui) {
    // First, so the palette's keys win over the editor's completion popup.
    search::palette(state, ui);
    egui::Panel::left("connections")
        .resizable(true)
        .default_size(240.0)
        .show(ui, |ui| connections::sidebar(state, ui));
    egui::Panel::bottom("status").show(ui, |ui| status_bar(state, ui));
    egui::Panel::top("tabs").show(ui, |ui| query::tab_bar(state, ui));
    let table_tab = state
        .active()
        .and_then(|id| state.tab(id))
        .is_some_and(|tab| matches!(tab.kind, crate::app::TabKind::Table { .. }));
    if table_tab {
        egui::Panel::top("table").show(ui, |ui| query::table_header(state, ui));
    } else {
        egui::Panel::top("editor")
            .resizable(true)
            .default_size(210.0)
            .show(ui, |ui| query::editor(state, ui));
    }
    egui::CentralPanel::default().show(ui, |ui| grid::results(state, ui));
    compare::compare_window(state, ui);
    connections::draft_window(state, ui);
    connections::password_window(state, ui);
    table_edit::table_editor_window(state, ui);
    table_edit::table_prod_confirm_window(state, ui);
}

fn status_bar(state: &mut AppState, ui: &mut egui::Ui) {
    let live = state
        .connections()
        .iter()
        .filter(|c| {
            matches!(
                c.state(),
                ConnectionState::Connected { .. } | ConnectionState::QueryRunning { .. }
            )
        })
        .count();
    let stats = format!(
        "{} connection{}, {} live",
        state.connections().len(),
        if state.connections().len() == 1 { "" } else { "s" },
        live
    );
    let theme_label = match state.theme() {
        Theme::Dark => "☀ Light",
        Theme::Light => "🌙 Dark",
    };
    let backend = state
        .active()
        .and_then(|id| state.tab(id))
        .and_then(|tab| state.connections().iter().find(|c| Some(c.id) == tab.conn))
        .map(|c| format!("{} · {}", c.name, c.config.kind().label()))
        .unwrap_or_else(|| "no backend".to_owned());
    ui.horizontal(|ui| {
        // The controls group never moves: its three buttons are reserved
        // first. The left half of the bar reports next, so the counts keep
        // their room; the backend name takes what remains and drops when
        // nothing is left. Both labels go through `elide`, so neither can
        // paint over the other.
        let gap = ui.spacing().item_spacing.x;
        let mut budget = fit::Budget::new(ui);
        for control in [
            fit::button_w(ui, theme_label, false),
            fit::button_w(ui, table_edit::STATUS_LABEL, false),
            fit::button_w(ui, "Compare", false),
        ] {
            budget.take(ui, control);
        }
        let stats_text = fit::elide(ui, &stats, egui::TextStyle::Body, budget.leftover() - gap);
        if !stats_text.is_empty() {
            budget.take(ui, fit::text_w(ui, &stats_text, egui::TextStyle::Body));
        }
        let backend_text = fit::elide(
            ui,
            &backend,
            egui::TextStyle::Small,
            budget.leftover() - gap,
        );
        if !stats_text.is_empty() {
            ui.add(
                egui::Label::new(stats_text)
                    .wrap_mode(egui::TextWrapMode::Extend),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(theme_label).on_hover_text("Flip the app theme").clicked() {
                state.toggle_theme();
            }
            table_edit::status_entry(state, ui);
            if ui
                .add(egui::Button::new("Compare").small())
                .on_hover_text("Compare two tables row by row")
                .clicked()
            {
                state.open_compare();
            }
            if !backend_text.is_empty() {
                ui.add(
                    egui::Label::new(egui::RichText::new(backend_text).small().weak())
                        .wrap_mode(egui::TextWrapMode::Extend),
                );
            }
        });
    });
}

/// App theme. Dark is the default and the pre-theme behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    pub fn egui(self) -> egui::Theme {
        match self {
            Theme::Dark => egui::Theme::Dark,
            Theme::Light => egui::Theme::Light,
        }
    }
    /// The opposite theme, so the status-bar toggle is one call.
    pub fn flipped(self) -> Theme {
        match self {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Dark,
        }
    }
    pub fn from_egui(theme: egui::Theme) -> Theme {
        match theme {
            egui::Theme::Dark => Theme::Dark,
            egui::Theme::Light => Theme::Light,
        }
    }
}

/// Every app-specific color the modules use, resolved from the egui theme.
/// Module constants read through this instead of hardcoding dark values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub accent: egui::Color32,
    pub success: egui::Color32,
    /// Selected-cell text; each theme's selection stroke equals this color.
    pub selection_text: egui::Color32,
    /// The fill behind selected cells and selected widgets.
    pub selection_fill: egui::Color32,
    /// A selected connection card: the dark fill pulled down, the light
    /// fill as-is, which is lighter than the faint row but darker than the
    /// window.
    pub selected_card_fill: egui::Color32,
    /// The fill of an unavailable control: a whisper-quiet neutral that
    /// recedes into either panel and carries no trace of the selection blue.
    pub disabled_fill: egui::Color32,
    /// The label on `disabled_fill`, readable at 4.5 to 1 and still calmer
    /// than any enabled neighbor.
    pub disabled_text: egui::Color32,
    pub edit_tint: egui::Color32,
    pub keyword: egui::Color32,
    pub string: egui::Color32,
    pub comment: egui::Color32,
    pub number: egui::Color32,
    pub badge_dev: egui::Color32,
    pub badge_test: egui::Color32,
    pub badge_prod: egui::Color32,
}

impl Palette {
    pub fn dark() -> Self {
        let selection_fill = egui::Color32::from_rgb(58, 110, 165);
        Self {
            accent: egui::Color32::from_rgb(58, 110, 165),
            success: egui::Color32::from_rgb(115, 180, 115),
            selection_text: egui::Color32::from_rgb(255, 255, 255),
            selection_fill,
            selected_card_fill: selection_fill.gamma_multiply(0.35),
            disabled_fill: egui::Color32::from_rgb(35, 41, 51),
            disabled_text: egui::Color32::from_rgb(148, 155, 166),
            edit_tint: egui::Color32::from_rgba_unmultiplied(227, 179, 65, 64),
            keyword: egui::Color32::from_rgb(120, 170, 220),
            string: egui::Color32::from_rgb(140, 200, 150),
            comment: egui::Color32::from_rgb(130, 140, 155),
            number: egui::Color32::from_rgb(220, 170, 110),
            badge_dev: egui::Color32::from_rgb(148, 155, 166),
            badge_test: egui::Color32::from_rgb(227, 179, 65),
            badge_prod: egui::Color32::from_rgb(224, 91, 91),
        }
    }
    pub fn light() -> Self {
        let selection_fill = egui::Color32::from_rgb(207, 226, 247);
        Self {
            accent: egui::Color32::from_rgb(58, 110, 165),
            success: egui::Color32::from_rgb(56, 142, 60),
            selection_text: egui::Color32::from_rgb(30, 80, 140),
            selection_fill,
            selected_card_fill: selection_fill,
            disabled_fill: egui::Color32::from_rgb(234, 237, 241),
            disabled_text: egui::Color32::from_rgb(95, 105, 115),
            edit_tint: egui::Color32::from_rgba_unmultiplied(227, 179, 65, 64),
            keyword: egui::Color32::from_rgb(25, 118, 210),
            string: egui::Color32::from_rgb(46, 125, 50),
            comment: egui::Color32::from_rgb(110, 119, 129),
            number: egui::Color32::from_rgb(170, 100, 30),
            badge_dev: egui::Color32::from_rgb(95, 105, 115),
            badge_test: egui::Color32::from_rgb(150, 110, 15),
            badge_prod: egui::Color32::from_rgb(190, 60, 60),
        }
    }
    /// The palette for the current egui theme.
    pub fn of(theme: egui::Theme) -> Self {
        match theme {
            egui::Theme::Dark => Self::dark(),
            egui::Theme::Light => Self::light(),
        }
    }
}
/// Spacing and text setup shared by both themes.
fn apply_shared(style: &mut egui::Style) {
    style.spacing.item_spacing = egui::vec2(8.0, 4.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.indent = 16.0;

    use egui::{FontFamily, FontId, TextStyle};
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(16.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Body,
        FontId::new(13.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(13.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(11.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(12.5, FontFamily::Monospace),
    );
}

/// Dark Beekeeper-ish visuals, byte-identical to the pre-theme setup.
fn dark_visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    visuals.dark_mode = true;
    visuals.window_fill = egui::Color32::from_rgb(24, 29, 37);
    visuals.panel_fill = egui::Color32::from_rgb(32, 39, 50);
    visuals.faint_bg_color = egui::Color32::from_rgb(42, 50, 63);
    visuals.extreme_bg_color = egui::Color32::from_rgb(18, 23, 30);
    visuals.code_bg_color = egui::Color32::from_rgb(40, 48, 60);
    visuals.selection.bg_fill = Palette::dark().selection_fill;
    visuals.selection.stroke = egui::Stroke::new(1.0, Palette::dark().selection_text);
    visuals.hyperlink_color = egui::Color32::from_rgb(120, 170, 220);
    visuals.window_corner_radius = egui::CornerRadius::same(6);
    visuals.menu_corner_radius = egui::CornerRadius::same(6);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(4);
    }
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(58, 110, 165);
    visuals.widgets.active.weak_bg_fill = egui::Color32::from_rgb(58, 110, 165);
    visuals
}

fn light_visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::light();
    visuals.dark_mode = false;
    visuals.window_fill = egui::Color32::from_rgb(242, 244, 247);
    visuals.panel_fill = egui::Color32::from_rgb(236, 238, 242);
    visuals.faint_bg_color = egui::Color32::from_rgb(228, 231, 236);
    visuals.extreme_bg_color = egui::Color32::from_rgb(225, 228, 233);
    visuals.code_bg_color = egui::Color32::from_rgb(240, 242, 245);
    visuals.selection.bg_fill = Palette::light().selection_fill;
    visuals.selection.stroke = egui::Stroke::new(1.0, Palette::light().selection_text);
    visuals.hyperlink_color = egui::Color32::from_rgb(25, 118, 210);
    visuals.window_corner_radius = egui::CornerRadius::same(6);
    visuals.menu_corner_radius = egui::CornerRadius::same(6);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(4);
    }
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(58, 110, 165);
    visuals.widgets.active.weak_bg_fill = egui::Color32::from_rgb(58, 110, 165);
    visuals
}

fn dark_style() -> egui::Style {
    let mut style = egui::Style::default();
    style.visuals = dark_visuals();
    apply_shared(&mut style);
    style
}

fn light_style() -> egui::Style {
    let mut style = egui::Style::default();
    style.visuals = light_visuals();
    apply_shared(&mut style);
    style
}

/// Flip the live theme: set the preference and re-apply both palettes, so a
/// flip mid-session repaints the whole app on the next frame.
pub fn apply_theme(ctx: &egui::Context, theme: Theme) {
    ctx.set_theme(theme.egui());
    ctx.set_style_of(egui::Theme::Dark, dark_style());
    ctx.set_style_of(egui::Theme::Light, light_style());
}

/// Theme setup, installed once at startup. Defaults to dark, the pre-theme
/// behavior; `AppState::new` re-applies the persisted choice.
pub fn install(ctx: &egui::Context) {
    apply_theme(ctx, Theme::Dark);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG contrast for two sRGB colors, so the selection pair is measured.
    fn contrast_ratio(a: eframe::egui::Color32, b: eframe::egui::Color32) -> f32 {
        fn luminance(color: eframe::egui::Color32) -> f32 {
            let channel = |value: u8| {
                let value = value as f32 / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(color.r())
                + 0.7152 * channel(color.g())
                + 0.0722 * channel(color.b())
        }
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn palette_apply_both_ways_paints_each_bucket() {
        let ctx = egui::Context::default();
        apply_theme(&ctx, Theme::Dark);
        assert_eq!(
            ctx.style_of(egui::Theme::Dark).visuals.window_fill,
            egui::Color32::from_rgb(24, 29, 37)
        );
        assert_eq!(
            ctx.style_of(egui::Theme::Light).visuals.window_fill,
            egui::Color32::from_rgb(242, 244, 247)
        );
        apply_theme(&ctx, Theme::Light);
        assert_eq!(
            ctx.style_of(egui::Theme::Dark).visuals.window_fill,
            egui::Color32::from_rgb(24, 29, 37)
        );
        assert_eq!(
            ctx.style_of(egui::Theme::Light).visuals.window_fill,
            egui::Color32::from_rgb(242, 244, 247)
        );
    }

    #[test]
    fn selection_stroke_matches_palette_text_with_contrast() {
        let ctx = egui::Context::default();
        apply_theme(&ctx, Theme::Dark);
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let visuals = &ctx.style_of(theme).visuals;
            let palette = Palette::of(theme);
            let text = palette.selection_text;
            assert_eq!(visuals.selection.stroke.color, text);
            assert_eq!(visuals.selection.bg_fill, palette.selection_fill);
            assert!(
                contrast_ratio(text, visuals.selection.bg_fill) >= 4.5,
                "{theme:?} selection text holds 4.5 to 1 on its fill"
            );
            assert!(
                contrast_ratio(visuals.selection.stroke.color, visuals.text_edit_bg_color()) >= 3.0,
                "{theme:?} focused ring holds 3.0 to 1 on the field it outlines"
            );
        }
    }

    #[test]
    fn resolved_palette_carries_each_theme_literal() {
        let dark = Palette::of(egui::Theme::Dark);
        assert_eq!(dark.accent, egui::Color32::from_rgb(58, 110, 165));
        assert_eq!(dark.success, egui::Color32::from_rgb(115, 180, 115));
        assert_eq!(dark.keyword, egui::Color32::from_rgb(120, 170, 220));
        assert_eq!(dark.selection_text, egui::Color32::from_rgb(255, 255, 255));
        assert_eq!(dark.selection_fill, egui::Color32::from_rgb(58, 110, 165));
        assert_eq!(dark.selected_card_fill, dark.selection_fill.gamma_multiply(0.35));
        assert_eq!(dark.disabled_fill, egui::Color32::from_rgb(35, 41, 51));
        assert_eq!(dark.disabled_text, egui::Color32::from_rgb(148, 155, 166));
        let light = Palette::of(egui::Theme::Light);
        assert_eq!(light.accent, egui::Color32::from_rgb(58, 110, 165));
        assert_eq!(light.success, egui::Color32::from_rgb(56, 142, 60));
        assert_eq!(light.keyword, egui::Color32::from_rgb(25, 118, 210));
        assert_eq!(light.selection_text, egui::Color32::from_rgb(30, 80, 140));
        assert_eq!(light.selection_fill, egui::Color32::from_rgb(207, 226, 247));
        assert_eq!(light.selected_card_fill, egui::Color32::from_rgb(207, 226, 247));
        assert_eq!(light.disabled_fill, egui::Color32::from_rgb(234, 237, 241));
        assert_eq!(light.disabled_text, egui::Color32::from_rgb(95, 105, 115));
        assert_ne!(dark.success, light.success);
        assert_ne!(dark.keyword, light.keyword);
    }

    #[test]
    fn disabled_pair_holds_contrast_and_differs_from_selection() {
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let palette = Palette::of(theme);
            assert!(
                contrast_ratio(palette.disabled_text, palette.disabled_fill) >= 4.5,
                "{theme:?} disabled text holds 4.5 to 1 on its fill"
            );
            assert_ne!(
                palette.disabled_text, palette.selection_text,
                "{theme:?} disabled text is its own steel, not the selection text"
            );
            assert_ne!(
                palette.disabled_fill, palette.selection_fill,
                "{theme:?} disabled fill is its own quiet slate, not the selection fill"
            );
        }
    }
}
