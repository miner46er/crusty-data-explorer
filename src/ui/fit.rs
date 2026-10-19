//! Width arithmetic for the chrome rows that have to fit a narrow window.
//!
//! Pure measurement and reservation: every number is measured against what
//! egui actually allocates, and a row draws only what its [`Budget`] accepted,
//! eliding the text into the room it reserved, so two controls never paint
//! over each other.

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
/// padding.
pub(crate) fn button_w(ui: &egui::Ui, text: &str) -> f32 {
    text_w(ui, text, egui::TextStyle::Button) + button_pad(ui)
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
pub(crate) const OVERFLOW_LABEL: &str = "…";

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
        // egui grows a ui's max_rect to fit a widget that overflows it, so a
        // row that ran past the panel would hand the next row a phantom
        // width. A row can only use what the panel shows.
        let visible = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        Self { available: visible.width().max(0.0), used: 0.0 }
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

    /// The widest form of `text` the row can still hold, reserved in the same
    /// step. Elision leaves room for the item spacing the reservation adds,
    /// and the cut is returned only when that reservation landed; otherwise
    /// the row draws nothing. The other texts on the row still see the whole
    /// leftover, so no two of them share it.
    pub(crate) fn elide(&mut self, ui: &egui::Ui, text: &str, style: egui::TextStyle) -> String {
        let room = self.leftover() - ui.spacing().item_spacing.x;
        let cut = elide(ui, text, style.clone(), room);
        if cut.is_empty() || !self.take(ui, text_w(ui, &cut, style)) {
            return String::new();
        }
        cut
    }
}

/// One packed row: items in order while they fit, the contiguous tail
/// behind one `…` menu whose hover text names what went in. `labels[i]`
/// names item `i`, and `draw` draws the item's control. Below the floor,
/// where even the `…` cannot fit, the row draws every item to clip at the
/// panel edge: there is no menu to attach a tail to, and a dropped control
/// is unreachable.
pub(crate) fn packed_row(
    ui: &mut egui::Ui,
    widths: &[f32],
    labels: &[&'static str],
    mut draw: impl FnMut(&mut egui::Ui, usize),
) {
    debug_assert_eq!(widths.len(), labels.len(), "one label per packed item");
    let spill = spill(ui, widths);
    if spill < widths.len() && overflow_fits(ui) {
        for index in 0..spill {
            draw(ui, index);
        }
        let tail = labels[spill..].join(", ");
        ui.menu_button(OVERFLOW_LABEL, |ui| {
            for index in spill..widths.len() {
                draw(ui, index);
            }
        })
        .response
        .on_hover_text(format!("More: {tail}"));
    } else {
        for index in 0..widths.len() {
            draw(ui, index);
        }
    }
}

/// Whether the row can hold the overflow button itself.
fn overflow_fits(ui: &egui::Ui) -> bool {
    let mut budget = Budget::new(ui);
    budget.take(ui, button_w(ui, OVERFLOW_LABEL))
}

/// The spill index for a row of `widths` packed left to right: the first item
/// that does not fit, leaving room for the overflow button when a tail
/// remains. `widths.len()` means everything fits inline.
pub(crate) fn spill(ui: &egui::Ui, widths: &[f32]) -> usize {
    let overflow_w = button_w(ui, OVERFLOW_LABEL);
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
    // Even the overflow button alone does not fit: the row is below its
    // floor, and it draws its items to clip at the panel edge instead.
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
            assert_eq!(elide(ui, "Save", egui::TextStyle::Button, budget), "Save");
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

    /// The honest elision rule: the text `Budget::elide` returns fits the
    /// room the budget had before the call, spacing included, so the row
    /// could reserve it. A cut that only fits the leftover is not returned:
    /// the next elide on the same budget still sees a real share.
    #[test]
    fn budget_elide_returns_only_text_it_reserved() {
        let ctx = context();
        frame(&ctx, [320.0, 200.0], |ui| {
            let gap = ui.spacing().item_spacing.x;
            for taken in [0.0, 100.0, 200.0, 250.0, 300.0] {
                let mut budget = Budget::new(ui);
                let _ = budget.take(ui, taken);
                let room = budget.leftover();
                let cut = budget.elide(ui, "a very long label indeed", egui::TextStyle::Body);
                let width = text_w(ui, &cut, egui::TextStyle::Body);
                assert!(
                    width + gap <= room + 0.001,
                    "a {taken}px reservation left {room}px but the cut is {width}px"
                );
            }
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
            assert!(budget.take(ui, available - gap));
            assert_eq!(budget.leftover(), 0.0);
            assert!(!budget.take(ui, 1.0));
            assert_eq!(budget.leftover(), 0.0);
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
            assert_eq!(spill(ui, &[100.0, 100.0, 100.0]), 2);
            assert_eq!(spill(ui, &[100.0, 100.0]), 2);
            // The overflow button fits; the item does not, so it goes behind
            // the button alone.
            assert_eq!(spill(ui, &[400.0]), 0);
        });
    }

    /// Below the floor, where even the `…` cannot fit, the row still draws
    /// its items: they clip at the panel edge, and a control that vanishes
    /// is unreachable.
    #[test]
    fn packed_row_draws_items_when_even_the_overflow_cannot_fit() {
        let ctx = context();
        frame(&ctx, [24.0, 200.0], |ui| {
            let mut drawn = 0;
            packed_row(ui, &[400.0], &["item"], |ui, _| {
                ui.button("item");
                drawn += 1;
            });
            assert_eq!(drawn, 1, "the item is drawn and clips, not dropped");
        });
    }

    /// The widths must equal what egui allocated to the real widgets, or the
    /// rows would reserve the wrong room.
    #[test]
    fn measurements_match_the_widgets_egui_allocated() {
        let ctx = context();
        let mut checked = 0;
        frame(&ctx, [720.0, 240.0], |ui| {
            for label in ["Save", "Run", "+ Table", "Compare", "Copy", "Chart"] {
                let measured = button_w(ui, label);
                let response = ui.add(egui::Button::new(label));
                assert!(
                    (response.rect.width() - measured).abs() <= 1.0,
                    "button {label:?}: egui allocated {} but button_w measured {measured}",
                    response.rect.width()
                );
                checked += 1;
            }
            // A menu button is a plain button in egui 0.36: the submenu arrow
            // belongs to `SubMenuButton`, which only appears inside a menu.
            for label in ["Export", "Transaction", "Complete"] {
                let measured = button_w(ui, label);
                let response = ui.menu_button(label, |_| {}).response;
                assert!(
                    (response.rect.width() - measured).abs() <= 1.0,
                    "menu {label:?}: egui allocated {} but button_w measured {measured}",
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
