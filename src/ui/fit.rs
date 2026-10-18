//! Width arithmetic for the chrome rows that have to fit a narrow window.
//!
//! Pure measurement and reservation: no state, no painting. A row that has
//! the width keeps today's layout; a row that does not elides its softest
//! text, wraps to a second line, or moves its tail behind one overflow menu.
//! Every number here is measured against what egui actually allocates, never
//! a hand-tuned constant: [`Budget`] refuses a widget the row cannot hold, so
//! two controls can never paint over each other.

use eframe::egui;

/// The galley width of `text` in `style`, unwrapped.
pub(crate) fn text_w(ui: &egui::Ui, text: &str, style: egui::TextStyle) -> f32 {
    let font = style.resolve(ui.style());
    let color = ui.visuals().text_color();
    ui.fonts_mut(|fonts| fonts.layout_no_wrap(text.to_owned(), font, color).size().x)
}

/// The horizontal padding a framed button adds around its text: the theme's
/// button padding on both sides.
fn button_pad(ui: &egui::Ui) -> f32 {
    let spacing = &ui.style().spacing;
    spacing.button_padding.x * 2.0
}

/// The width a button with `text` allocates: its galley plus the button
/// padding. `small` measures in the Small text style.
pub(crate) fn button_w(ui: &egui::Ui, text: &str, small: bool) -> f32 {
    let style = if small { egui::TextStyle::Small } else { egui::TextStyle::Button };
    text_w(ui, text, style) + button_pad(ui)
}

/// The width a menu button with `text` allocates. egui 0.36's `menu_button`
/// draws a plain button (the submenu arrow belongs to `SubMenuButton`, which
/// only appears inside a menu), so this is the button width. It keeps its own
/// name because the rows budget in "menus", and a future egui that adds the
/// arrow changes one function.
pub(crate) fn menu_w(ui: &egui::Ui, text: &str) -> f32 {
    button_w(ui, text, false)
}

/// The width a combo box allocates: egui 0.36 frames the selected text as
/// the wider of the galley plus the dropdown icon and its spacing, or the
/// style's `combo_width`, plus the frame's button padding on both sides.
pub(crate) fn combo_w(ui: &egui::Ui, selected_text: &str) -> f32 {
    let spacing = &ui.style().spacing;
    let galley = text_w(ui, selected_text, egui::TextStyle::Button);
    (galley + spacing.icon_spacing + spacing.icon_width)
        .max(spacing.combo_width - 2.0 * spacing.button_padding.x)
        + 2.0 * spacing.button_padding.x
}

/// The single ellipsis the overflow menu is labelled with.
pub(crate) const OVERFLOW_LABEL: &str = "\u{2026}";

/// `text` cut with an ellipsis so its measured width fits `budget`; empty
/// when even the ellipsis does not fit.
pub(crate) fn elide(ui: &egui::Ui, text: &str, style: egui::TextStyle, budget: f32) -> String {
    if text_w(ui, text, style.clone()) <= budget {
        return text.to_owned();
    }
    let ellipsis = OVERFLOW_LABEL;
    if text_w(ui, ellipsis, style.clone()) > budget {
        return String::new();
    }
    // The longest prefix whose galley plus the ellipsis still fits. Binary
    // search on char boundaries, so multi-byte text never splits.
    let chars: Vec<char> = text.chars().collect();
    let (mut low, mut high) = (0, chars.len());
    while low < high {
        let middle = (low + high).div_ceil(2);
        let candidate: String = chars[..middle].iter().collect::<String>() + ellipsis;
        if text_w(ui, &candidate, style.clone()) <= budget {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    chars[..low].iter().collect::<String>() + ellipsis
}

/// One row's measured budget: the width the row has, and the width reserved
/// so far. A reservation includes the row's item spacing, so the sum of what
/// [`Budget::take`] accepted always fits `available`.
pub(crate) struct Budget {
    available: f32,
    used: f32,
}

impl Budget {
    pub(crate) fn new(ui: &egui::Ui) -> Self {
        Self { available: ui.available_width(), used: 0.0 }
    }

    /// Reserve `w` plus the row's item spacing; false when it does not fit.
    pub(crate) fn take(&mut self, ui: &egui::Ui, w: f32) -> bool {
        let needed = w + ui.spacing().item_spacing.x;
        if self.used + needed <= self.available {
            self.used += needed;
            true
        } else {
            false
        }
    }

    pub(crate) fn leftover(&self) -> f32 {
        (self.available - self.used).max(0.0)
    }
}

/// The `...` menu that holds a row's tail when the row is too narrow. The
/// caller names the tail on the returned response's hover text, since only it
/// knows which controls went in.
pub(crate) fn overflow_menu<R>(
    ui: &mut egui::Ui,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::Response {
    ui.menu_button(OVERFLOW_LABEL, body).response
}

/// The spill index for a row of `widths` packed left to right: the first item
/// that does not fit, leaving room for the overflow button when a tail
/// remains. `widths.len()` means everything fits inline.
pub(crate) fn spill(ui: &egui::Ui, widths: &[f32]) -> usize {
    let overflow_w = button_w(ui, OVERFLOW_LABEL, false);
    for inline in (0..=widths.len()).rev() {
        let mut budget = Budget::new(ui);
        let mut fits = widths[..inline].iter().all(|w| budget.take(ui, *w));
        if inline < widths.len() {
            fits &= budget.take(ui, overflow_w);
        }
        if fits {
            return inline;
        }
    }
    // Even the overflow button alone does not fit: keep every item behind it.
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A context with the app's style installed: every measurement here is
    /// taken against the same theme the chrome renders with.
    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        crate::ui::install(&ctx);
        ctx
    }

    /// One frame at a fixed size. The texture deltas are dropped, since
    /// nothing uploads them.
    fn frame(ctx: &egui::Context, size: [f32; 2], run: impl FnMut(&mut egui::Ui)) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size[0], size[1]),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, run);
        output.textures_delta.clear();
    }

    #[test]
    fn elide_fits_the_budget_and_ends_with_the_ellipsis() {
        let ctx = context();
        frame(&ctx, [320.0, 200.0], |ui| {
            let text = "a very long label indeed";
            let budget = 60.0;
            let cut = elide(ui, text, egui::TextStyle::Body, budget);
            assert!(cut.ends_with(OVERFLOW_LABEL), "{cut:?}");
            assert!(cut.chars().count() < text.chars().count(), "{cut:?}");
            assert!(text_w(ui, &cut, egui::TextStyle::Body) <= budget, "{cut:?}");
            // A label that already fits comes back untouched.
            assert_eq!(elide(ui, "Save", egui::TextStyle::Button, budget), "Save");
            // The cut respects char boundaries: no half of a multi-byte char.
            let accented = elide(ui, "éééééééééééééééééééé", egui::TextStyle::Body, budget);
            assert!(accented.ends_with(OVERFLOW_LABEL), "{accented:?}");
            assert!(text_w(ui, &accented, egui::TextStyle::Body) <= budget);
        });
    }

    #[test]
    fn elide_is_empty_when_even_the_ellipsis_does_not_fit() {
        let ctx = context();
        frame(&ctx, [320.0, 200.0], |ui| {
            assert_eq!(elide(ui, "anything", egui::TextStyle::Body, 1.0), "");
            assert_eq!(elide(ui, "", egui::TextStyle::Body, 0.0), "");
        });
    }

    #[test]
    fn budget_refuses_past_the_available_width() {
        let ctx = context();
        frame(&ctx, [320.0, 200.0], |ui| {
            let mut budget = Budget::new(ui);
            let available = budget.available;
            let gap = ui.spacing().item_spacing.x;
            assert_eq!(available, 320.0);
            // A reservation is the width plus the row's item spacing.
            assert!(budget.take(ui, available - gap));
            assert_eq!(budget.leftover(), 0.0);
            // The next control, however small, no longer fits.
            assert!(!budget.take(ui, 1.0));
            assert_eq!(budget.leftover(), 0.0);
            // Two items on a roomy row leave their widths and both gaps.
            let mut roomy = Budget::new(ui);
            assert!(roomy.take(ui, 100.0));
            assert!(roomy.take(ui, 100.0));
            assert!((roomy.leftover() - (available - 200.0 - 2.0 * gap)).abs() < 0.001);
            assert!(!roomy.take(ui, available));
        });
    }

    #[test]
    fn spill_keeps_the_tail_behind_one_overflow_button() {
        let ctx = context();
        frame(&ctx, [320.0, 200.0], |ui| {
            // Two 100px controls plus the overflow button and three gaps fit
            // in 320; three of them do not.
            assert_eq!(spill(ui, &[100.0, 100.0, 100.0]), 2);
            // Everything fits: nothing spills.
            assert_eq!(spill(ui, &[100.0, 100.0]), 2);
            // A row that cannot hold even the overflow button spills all.
            assert_eq!(spill(ui, &[400.0]), 0);
        });
    }

    /// The widths must equal what egui allocated to the real widgets, or the
    /// rows would reserve the wrong room.
    #[test]
    fn measurements_match_the_widgets_egui_allocated() {
        let ctx = context();
        let mut checked = 0;
        frame(&ctx, [720.0, 240.0], |ui| {
            for (label, small) in [
                ("Save", false),
                ("Run", false),
                ("+ Table", false),
                ("Compare", false),
                ("Copy", false),
                ("Chart", false),
            ] {
                let measured = button_w(ui, label, small);
                let response = ui.add(egui::Button::new(label).small());
                assert!(
                    (response.rect.width() - measured).abs() <= 1.0,
                    "button {label:?}: egui allocated {} but button_w measured {measured}",
                    response.rect.width()
                );
                checked += 1;
            }
            // `Button::small()` only trims the vertical padding; a label that
            // really renders in the Small text style carries it on the text.
            for label in ["New query", "Delete"] {
                let measured = button_w(ui, label, true);
                let response =
                    ui.add(egui::Button::new(egui::RichText::new(label).small()).small());
                assert!(
                    (response.rect.width() - measured).abs() <= 1.0,
                    "small button {label:?}: egui allocated {} but button_w measured {measured}",
                    response.rect.width()
                );
                checked += 1;
            }
            for label in ["Export", "Transaction", "Complete"] {
                let measured = menu_w(ui, label);
                let response = ui.menu_button(label, |_| {}).response;
                assert!(
                    (response.rect.width() - measured).abs() <= 1.0,
                    "menu {label:?}: egui allocated {} but menu_w measured {measured}",
                    response.rect.width()
                );
                checked += 1;
            }
            for label in ["All", "Current statement"] {
                let measured = combo_w(ui, label);
                let response = egui::ComboBox::from_id_salt(("combo_probe", label))
                    .selected_text(label)
                    .show_ui(ui, |_| {})
                    .response;
                assert!(
                    (response.rect.width() - measured).abs() <= 1.0,
                    "combo {label:?}: egui allocated {} but combo_w measured {measured}",
                    response.rect.width()
                );
                checked += 1;
            }
        });
        assert!(checked >= 8, "the anchored check covered {checked} widgets");
    }
}
