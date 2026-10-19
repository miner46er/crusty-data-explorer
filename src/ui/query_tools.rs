//! The editor toolbar and the tools it carries.
//!
//! The row measures every control before it draws: the tools fit on one row
//! with the run controls, or they take a second row and pack their tail
//! behind one `…` menu. Clicking any tool lands its action through the
//! tab's transitions in [`toolbar`].

use eframe::egui;

use super::editor::format_sql;
use super::query::{find_open, find_shortcut, run_shortcut, toggle_find};
use super::{Palette, fit};
use crate::app::{AppState, ConnectionState, RunOutcome, RunScope, TransactionAction};
use crate::db::{DbKind, TabId};

/// One editor-toolbar tool. The inline row and the overflow menu both draw
/// through [`ToolClicks::draw`], so a tool is drawn once whatever the width.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Save,
    Commit,
    Rollback,
    Transaction,
    Autocommit,
    Complete,
    Format,
    Find,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Self::Save => "Save",
            Self::Commit => "Commit",
            Self::Rollback => "Rollback",
            Self::Transaction => "Transaction",
            Self::Autocommit => "Autocommit",
            Self::Complete => super::complete::COMPLETE_MENU_LABEL,
            Self::Format => "Format",
            Self::Find => "Find",
        }
    }

    /// The width this tool allocates, measured before the row draws, so the
    /// row can reserve it.
    fn width(self, ui: &egui::Ui, state: &ToolState) -> f32 {
        match self {
            Self::Autocommit => match state.slot.chip(state.autocommit, state.sqlite) {
                Some((label, _)) => fit::button_w(ui, &label),
                None => fit::button_w(ui, self.label()),
            },
            Self::Save
            | Self::Commit
            | Self::Rollback
            | Self::Transaction
            | Self::Complete
            | Self::Format
            | Self::Find => fit::button_w(ui, self.label()),
        }
    }
}

/// What the tools read from the tab's connection and from the find bar.
struct ToolState<'a> {
    slot: &'a AutocommitSlot,
    autocommit: bool,
    sqlite: bool,
    transaction_open: bool,
    transaction_ready: bool,
    find_open: bool,
}

impl ToolState<'_> {
    /// The tools the row carries, in the order they read left to right. The
    /// transaction block is Commit and Rollback, or the autocommit control,
    /// or the Transaction menu while the mode is on and no transaction is
    /// open.
    fn tools(&self, running: bool) -> Vec<Tool> {
        let mut tools = Vec::new();
        if !running {
            tools.push(Tool::Save);
        }
        if self.transaction_open {
            tools.push(Tool::Commit);
            tools.push(Tool::Rollback);
        }
        if !self.autocommit {
            tools.push(Tool::Autocommit);
        } else if !self.transaction_open {
            tools.push(Tool::Transaction);
        }
        tools.push(Tool::Complete);
        tools.push(Tool::Format);
        tools.push(Tool::Find);
        tools
    }
}

/// What the tools on the row produced, so the inline row and the overflow
/// menu can share one drawer without an action landing twice.
#[derive(Default)]
struct ToolClicks {
    save: bool,
    transaction: Option<TransactionAction>,
    autocommit: Option<bool>,
    format: bool,
    find: bool,
}

/// A tail toggle ends like the tail's other items: the menu it was opened
/// from closes. Inline, the toggle has no menu to close.
fn close_in_menu(ui: &egui::Ui) {
    if egui::menu::is_in_menu(ui) {
        ui.close();
    }
}

impl ToolClicks {
    fn draw(&mut self, ui: &mut egui::Ui, tool: Tool, state: &ToolState) {        match tool {
            Tool::Save => self.save |= ui.button(tool.label()).clicked(),
            Tool::Commit | Tool::Rollback => {
                let action = if tool == Tool::Commit {
                    TransactionAction::Commit
                } else {
                    TransactionAction::Rollback
                };
                if ui
                    .add_enabled(state.transaction_ready, egui::Button::new(tool.label()))
                    .on_hover_text(format!("Send {}", action.statement()))
                    .clicked()
                {
                    self.transaction = Some(action);
                }
            }
            Tool::Transaction => {
                ui.menu_button(tool.label(), |ui| {
                    if ui
                        .add_enabled(state.transaction_ready, egui::Button::new("Begin"))
                        .on_hover_text("Send BEGIN")
                        .clicked()
                    {
                        self.transaction = Some(TransactionAction::Begin);
                        ui.close();
                    }
                    ui.separator();
                    let toggle =
                        autocommit_control(ui, state.slot, state.autocommit, state.sqlite);
                    if toggle.clicked() {
                        self.autocommit = Some(!state.autocommit);
                        close_in_menu(ui);
                    }
                });
            }
            Tool::Autocommit => {
                let toggle = autocommit_control(ui, state.slot, state.autocommit, state.sqlite);
                if toggle.clicked() {
                    self.autocommit = Some(!state.autocommit);
                    close_in_menu(ui);
                }
            }
            Tool::Complete => super::complete::complete_menu(ui),
            Tool::Format => self.format |= ui.button(tool.label()).clicked(),
            Tool::Find => {
                if ui
                    .selectable_label(state.find_open, tool.label())
                    .on_hover_text(format!(
                        "Find ({})",
                        ui.ctx().format_shortcut(&find_shortcut())
                    ))
                    .clicked()
                {
                    self.find = true;
                    close_in_menu(ui);
                }
            }
        }
    }
}

/// The run control's label. The row measures it in [`fits_one_row`] and
/// draws it in [`toolbar`], so both sides read this.
const RUN_LABEL: &str = "▶ Run";

/// The explain control's label, measured and drawn like [`RUN_LABEL`].
const EXPLAIN_LABEL: &str = "Explain";

/// The narrowest a hint may be elided to before it is dropped: the hint is
/// the softest text on the row, so it goes first.
const HINT_MIN: f32 = 24.0;

/// Whether the editor toolbar fits one row. Decided from the idle content
/// only, so a run starting or finishing never changes the row count. The
/// hint is not part of the decision: it is the softest text on the row, so it
/// elides into what the leading group and the tools leave, and the row wraps
/// only when those cannot share a line.
fn fits_one_row(
    ui: &egui::Ui,
    tool_state: &ToolState<'_>,
    scope_label: &str,
) -> bool {
    let gap = ui.spacing().item_spacing.x;
    let leading = fit::combo_w(ui, scope_label)
        + fit::button_w(ui, RUN_LABEL)
        + fit::button_w(ui, EXPLAIN_LABEL);
    // A run in flight widens the autocommit chip; measure the idle slot, or
    // starting a run would re-flow the row.
    let idle_slot = tool_state.slot.without_running(tool_state.sqlite);
    let idle = ToolState { slot: &idle_slot, ..*tool_state };
    let tools_w: f32 = idle
        .tools(false)
        .iter()
        .map(|tool| tool.width(ui, &idle) + gap)
        .sum();
    ui.available_width() >= leading + 3.0 * gap + tools_w
}

pub(crate) fn toolbar(state: &mut AppState, ui: &mut egui::Ui) -> bool {
    let Some(tab_id) = state.active() else { return false };
    let modal_open = super::modal_open(state, ui.ctx());
    let find_shown = find_open(ui.ctx(), tab_id);
    let (mut run_clicked, mut explain_clicked, mut cancel_clicked) = (false, false, false);
    let mut clicks = ToolClicks::default();
    let mut scope = state.tab(tab_id).map(|t| t.run_scope()).unwrap_or_default();
    if state.tab(tab_id).is_some() {
        let running = matches!(
            state.tab(tab_id).map(|t| t.outcome()),
            Some(RunOutcome::Running { .. })
        );
        let elapsed = match state.tab(tab_id).map(|t| t.outcome()) {
            Some(RunOutcome::Running { started, .. }) => {
                Some(format!("{:.1}s", started.elapsed().as_secs_f32()))
            }
            _ => None,
        };
        let transaction_ready = state.transaction_ready(tab_id);
        let (transaction_open, autocommit, sqlite) = state
            .tab(tab_id)
            .and_then(|t| t.conn)
            .and_then(|id| state.connections().iter().find(|c| c.id == id))
            .map(|c| {
                (
                    c.transaction().is_open(),
                    c.autocommit(),
                    c.config.kind() == DbKind::Sqlite,
                )
            })
            .unwrap_or((false, true, false));
        let tool_state = ToolState {
            slot: &AutocommitSlot::of(state, tab_id),
            autocommit,
            sqlite,
            transaction_open,
            transaction_ready,
            find_open: find_shown,
        };
        let hint = format!(
            "{} · {}",
            scope.hint(),
            ui.ctx().format_shortcut(&run_shortcut())
        );
        let tools = tool_state.tools(running);
        let tool_w: Vec<f32> = tools.iter().map(|tool| tool.width(ui, &tool_state)).collect();
        // The row count is fixed before anything draws, from the idle content.
        let one_row = fits_one_row(ui, &tool_state, scope.label());
        let mut two_rows = false;
        ui.horizontal(|ui| {
            let gap = ui.spacing().item_spacing.x;
            if running {
                cancel_clicked = ui.button("Cancel").clicked();
            } else {
                egui::ComboBox::from_id_salt(("crusty_run_scope", tab_id))
                    .selected_text(scope.label())
                    .show_ui(ui, |ui| {
                        for option in RunScope::ALL {
                            ui.selectable_value(&mut scope, option, option.label());
                        }
                    });
                run_clicked = ui
                    .add(
                        egui::Button::new(egui::RichText::new(RUN_LABEL).strong())
                            .fill(Palette::of(ui.ctx().theme()).success),
                    )
                    .clicked();
                explain_clicked = ui
                    .button(EXPLAIN_LABEL)
                    .on_hover_text("Query execution plan")
                    .clicked();
            }
            if !running {
                let tools_w: f32 = tool_w.iter().map(|w| w + gap).sum();
                let budget = if one_row {
                    ui.available_width() - gap - tools_w
                } else {
                    ui.available_width()
                };
                let text = fit::elide(ui, &hint, egui::TextStyle::Small, budget);
                if fit::text_w(ui, &text, egui::TextStyle::Small) >= HINT_MIN {
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).small().weak())
                            .wrap_mode(egui::TextWrapMode::Extend),
                    );
                }
            }
            if one_row {
                for tool in &tools {
                    clicks.draw(ui, *tool, &tool_state);
                }
            } else {
                two_rows = true;
            }
            if let Some(elapsed) = &elapsed {
                ui.spinner();
                ui.label(elapsed);
            }
        });
        if two_rows {
            ui.horizontal(|ui| {
                let labels: Vec<&str> = tools.iter().map(|tool| tool.label()).collect();
                fit::packed_row(ui, &tool_w, &labels, |ui, index| {
                    clicks.draw(ui, tools[index], &tool_state);
                });
            });
        }
    }
    if scope != state.tab(tab_id).map(|t| t.run_scope()).unwrap_or_default() {
        if let Some(tab) = state.tab_mut(tab_id) {
            tab.set_run_scope(scope);
        }
    }
    if !modal_open {
        run_clicked |= ui.input_mut(|i| i.consume_shortcut(&run_shortcut()));
    }
    if cancel_clicked {
        state.cancel_run(tab_id);
    }
    if let Some(action) = clicks.transaction {
        state.start_transaction(tab_id, action);
    }
    if let Some(on) = clicks.autocommit {
        state.set_autocommit(tab_id, on);
    }
    if clicks.save && !modal_open {
        state.open_save_prompt();
    }
    if clicks.format
        && !modal_open
        && let Some(sql) = state.tab_sql_mut(tab_id)
    {
        *sql = format_sql(sql);
    }
    let mut find_opened = false;
    if clicks.find && !modal_open {
        find_opened = toggle_find(ui.ctx(), tab_id);
    }
    if run_clicked && !modal_open {
        let sql = state.tab(tab_id).map(|t| t.sql.clone()).unwrap_or_default();
        let (cursor, selection) = super::complete::editor_offsets(ui.ctx(), tab_id, &sql);
        state.start_run(tab_id, &sql, cursor, selection);
    }
    if explain_clicked && !modal_open {
        let sql = state.tab(tab_id).map(|t| t.sql.clone()).unwrap_or_default();
        let (cursor, selection) = super::complete::editor_offsets(ui.ctx(), tab_id, &sql);
        state.start_explain(tab_id, &sql, cursor, selection);
    }
    find_opened
}

/// The Autocommit control's situation, read from the tab's connection at the
/// call site. The first three render as a chip; `Toggle` is the live toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutocommitSlot {
    /// No connection, still connecting, or failed: nothing to switch.
    Offline,
    /// Live on SQLite, which has no manual mode.
    Sqlite,
    /// Live on another engine with a run in flight: the mode has to wait.
    Running,
    /// Live on another engine and idle: the toggle can act.
    Toggle,
}

impl AutocommitSlot {
    /// The slot a row measures with: a run in flight does not change the
    /// width of the autocommit control, so a width decision must not see
    /// `Running`.
    pub(crate) fn without_running(self, sqlite: bool) -> Self {
        match self {
            Self::Running if sqlite => Self::Sqlite,
            Self::Running => Self::Toggle,
            other => other,
        }
    }

    /// Which slot `tab`'s connection is in.
    pub(crate) fn of(state: &AppState, tab: TabId) -> Self {
        let conn = state
            .tab(tab)
            .and_then(|t| t.conn)
            .and_then(|id| state.connections().iter().find(|c| c.id == id));
        let Some(conn) = conn else { return Self::Offline };
        if !matches!(
            conn.state(),
            ConnectionState::Connected { .. } | ConnectionState::QueryRunning { .. }
        ) {
            return Self::Offline;
        }
        if conn.config.kind() == DbKind::Sqlite {
            return Self::Sqlite;
        }
        if state.transaction_ready(tab) { Self::Toggle } else { Self::Running }
    }

    /// The chip's label and the reason on its tooltip, or None for the live
    /// toggle. `on` is the observed mode; `sqlite` keeps the label plain,
    /// since the mode there is constant whatever the state.
    pub(crate) fn chip(
        self,
        on: bool,
        sqlite: bool,
    ) -> Option<(String, &'static str)> {
        let label = if sqlite {
            "Autocommit".to_owned()
        } else if on {
            "Autocommit: on".to_owned()
        } else {
            "Autocommit: off".to_owned()
        };
        match self {
            Self::Toggle => None,
            Self::Offline => Some((label, "Connect to change autocommit")),
            Self::Running => Some((label, "Wait for the run to finish")),
            Self::Sqlite => Some((label, "SQLite handles autocommit itself")),
        }
    }
}

/// The Autocommit control for one slot: the chip for states that cannot act,
/// the live toggle otherwise. Shared by the inline control and the
/// Transaction menu so both read the same logic.
fn autocommit_control(
    ui: &mut egui::Ui,
    slot: &AutocommitSlot,
    autocommit: bool,
    sqlite: bool,
) -> egui::Response {
    match slot.chip(autocommit, sqlite) {
        Some((label, reason)) => autocommit_chip(ui, &label, reason),
        None => {
            let mut shown = autocommit;
            ui.toggle_value(&mut shown, "Autocommit").on_hover_text(
                "Off: every statement runs inside a transaction on one tagged \
                 connection until Commit or Rollback",
            )
        }
    }
}

/// An Autocommit chip for the states where the toggle cannot act: filled and
/// colored as unavailable, sensed as hover only so it never lights up or
/// moves, with the reason on the tooltip. The hairline edge is the neutral
/// outline color: the fill alone sits within a few percent of the panel, so
/// the edge is what makes the chip read as a control slot.
fn autocommit_chip(ui: &mut egui::Ui, label: &str, reason: &str) -> egui::Response {
    let palette = Palette::of(ui.ctx().theme());
    let edge = ui.visuals().widgets.noninteractive.bg_stroke.color;
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(palette.disabled_text))
            .fill(palette.disabled_fill)
            .stroke(egui::Stroke::new(1.0, edge))
            .sense(egui::Sense::hover()),
    )
    .on_hover_text(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_toolbar_wraps_narrow_and_fits_wide() {
        let fits = |width: f32| {
            let ctx = egui::Context::default();
            crate::ui::install(&ctx);
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 200.0),
                )),
                ..Default::default()
            };
            let mut answer = None;
            let mut output = ctx.run_ui(input, |ui| {
                let slot = AutocommitSlot::Toggle;
                let state = ToolState {
                    slot: &slot,
                    autocommit: true,
                    sqlite: false,
                    transaction_open: false,
                    transaction_ready: true,
                    find_open: false,
                };
                answer = Some(fits_one_row(ui, &state, RunScope::All.label()));
            });
            output.textures_delta.clear();
            answer.expect("the frame ran the check")
        };
        assert!(!fits(364.0), "the idle toolbar takes two rows at 364");
        assert!(fits(1200.0), "the idle toolbar fits one row at 1200");
    }

    /// A run in flight swaps the autocommit chip for a wider label. The row's
    /// decision measures the idle slot, so a run starting or finishing cannot
    /// move the tools to another row.
    #[test]
    fn a_run_in_flight_does_not_move_the_editor_row() {
        let same = |width: f32| {
            let ctx = egui::Context::default();
            crate::ui::install(&ctx);
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 200.0),
                )),
                ..Default::default()
            };
            let mut answer = None;
            let mut output = ctx.run_ui(input, |ui| {
                let idle_slot = AutocommitSlot::Toggle;
                let idle = ToolState {
                    slot: &idle_slot,
                    autocommit: false,
                    sqlite: false,
                    transaction_open: false,
                    transaction_ready: true,
                    find_open: false,
                };
                let running_slot = AutocommitSlot::Running;
                let running = ToolState { slot: &running_slot, ..idle };
                let idle_row = fits_one_row(ui, &idle, RunScope::All.label());
                let running_row = fits_one_row(ui, &running, RunScope::All.label());
                answer = Some((idle_row, running_row));
            });
            output.textures_delta.clear();
            answer.expect("the frame ran the check")
        };
        for width in [364.0, 520.0, 640.0, 1200.0] {
            let (idle, running) = same(width);
            assert_eq!(idle, running, "the row decision matches at {width}");
        }
    }

    #[test]
    fn autocommit_chip_carries_the_mode_and_the_reason_for_its_slot() {
        use AutocommitSlot::{Offline, Running, Sqlite, Toggle};
        assert_eq!(
            Offline.chip(true, false),
            Some(("Autocommit: on".to_owned(), "Connect to change autocommit"))
        );
        assert_eq!(
            Offline.chip(false, false),
            Some(("Autocommit: off".to_owned(), "Connect to change autocommit"))
        );
        // SQLite's mode is constant, so its label never carries one, even
        // while the connection is not live yet.
        assert_eq!(
            Offline.chip(true, true),
            Some(("Autocommit".to_owned(), "Connect to change autocommit"))
        );
        assert_eq!(
            Sqlite.chip(true, true),
            Some(("Autocommit".to_owned(), "SQLite handles autocommit itself"))
        );
        assert_eq!(
            Sqlite.chip(false, true),
            Some(("Autocommit".to_owned(), "SQLite handles autocommit itself"))
        );
        assert_eq!(
            Running.chip(false, false),
            Some(("Autocommit: off".to_owned(), "Wait for the run to finish"))
        );
        assert_eq!(Toggle.chip(true, false), None);
        assert_eq!(Toggle.chip(false, false), None);
    }
}

// Manual check: Explain runs start_explain with the same offsets Run passes,
// Format rewrites the editor's SQL, and the Autocommit toggle beside
// Begin/Commit/Rollback sends one `SetAutocommit` through `set_autocommit`
// (disabled for SQLite and while a run is in flight) and shows the landed
// state, covered by AppState tests.
